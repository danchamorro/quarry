//! The engine lives on one worker; rendering only reads a bounded viewport cache.
use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use gpui_kit::component::ActiveTheme;
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::{
    App, Context, IntoElement, ParentElement, Pixels, SharedString, Styled, Window, div, px, rgb,
};
use quarry_core::{FilterQuery, OpenOptions};

use crate::document::{Document, DocumentInfo, MAX_VIEWPORT_COLUMNS, MAX_VIEWPORT_ROWS, Viewport};

pub enum Command {
    /// The caller must confirm discarding unsaved edits before replacing a document.
    Open(PathBuf, OpenOptions),
    Cell(u64, usize),
    Edit(u64, usize, String),
    Undo,
    Redo,
    Filter(FilterQuery),
    ClearFilter,
    SaveCopy(PathBuf),
}

pub enum Response {
    Opened(DocumentInfo),
    Info(DocumentInfo),
    Viewport {
        generation: u64,
        sequence: u64,
        viewport: Viewport,
    },
    Cell {
        row: u64,
        column: usize,
        value: String,
    },
    Saved(PathBuf),
    Done,
    Error(String),
}

#[derive(Clone, PartialEq)]
struct ViewportKey {
    generation: u64,
    start: u64,
    count: usize,
    columns: Vec<usize>,
}

struct ViewportRequest {
    key: ViewportKey,
    sequence: u64,
}

#[derive(Default)]
struct ViewportSlot {
    pending: Option<ViewportRequest>,
    last: Option<ViewportKey>,
}

#[derive(Clone)]
pub struct ViewportRequester {
    slot: Arc<Mutex<ViewportSlot>>,
    generation: Arc<AtomicU64>,
    sequence: Arc<AtomicU64>,
}

impl ViewportRequester {
    fn request(&self, start: u64, count: usize, columns: Vec<usize>, force: bool) -> bool {
        let key = ViewportKey {
            generation: self.generation.load(Ordering::Acquire),
            start,
            count: count.min(MAX_VIEWPORT_ROWS),
            columns: columns.into_iter().take(MAX_VIEWPORT_COLUMNS).collect(),
        };
        // Render never waits for the worker, and newer requests replace older scroll positions.
        if let Ok(mut slot) = self.slot.try_lock() {
            if !force && slot.last.as_ref() == Some(&key) {
                return true;
            }
            let sequence = self.sequence.fetch_add(1, Ordering::AcqRel) + 1;
            slot.last = Some(key.clone());
            slot.pending = Some(ViewportRequest { key, sequence });
            return true;
        }
        false
    }
}

pub struct Bridge {
    commands: mpsc::SyncSender<(u64, Command)>,
    responses: mpsc::Receiver<(u64, Option<u64>, Response)>,
    requester: ViewportRequester,
}

impl Bridge {
    pub fn new() -> Self {
        let (commands, command_rx) = mpsc::sync_channel(8);
        let (response_tx, responses) = mpsc::sync_channel(16);
        let requester = ViewportRequester {
            slot: Arc::new(Mutex::new(ViewportSlot::default())),
            generation: Arc::new(AtomicU64::new(0)),
            sequence: Arc::new(AtomicU64::new(0)),
        };
        let worker_requester = requester.clone();
        std::thread::spawn(move || worker(command_rx, response_tx, worker_requester));
        Self {
            commands,
            responses,
            requester,
        }
    }

    pub fn requester(&self) -> ViewportRequester {
        self.requester.clone()
    }

    pub fn send(&self, command: Command) -> Result<(), String> {
        let changes_view = !matches!(command, Command::Cell(..) | Command::SaveCopy(..));
        let previous = self.requester.generation.load(Ordering::Acquire);
        let generation = previous + u64::from(changes_view);
        self.commands
            .try_send((generation, command))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => {
                    "The engine is busy. Try again when the current operation finishes.".to_string()
                }
                mpsc::TrySendError::Disconnected(_) => {
                    "The comparison engine stopped unexpectedly.".to_string()
                }
            })?;
        self.requester
            .generation
            .store(generation, Ordering::Release);
        Ok(())
    }

    pub fn drain(&self) -> Vec<Response> {
        let generation = self.requester.generation.load(Ordering::Acquire);
        let sequence = self.requester.sequence.load(Ordering::Acquire);
        self.responses
            .try_iter()
            .filter_map(|(version, viewport_sequence, response)| {
                (version == generation && viewport_sequence.is_none_or(|value| value == sequence))
                    .then_some(response)
            })
            .collect()
    }
}

#[derive(PartialEq, Eq)]
struct ProgressKey {
    rows: u64,
    columns: usize,
    indexed_bytes: u64,
    indexing_done: bool,
    filter: Option<(u64, u64, bool)>,
    dirty: bool,
    can_undo: bool,
    can_redo: bool,
}

impl From<&DocumentInfo> for ProgressKey {
    fn from(info: &DocumentInfo) -> Self {
        Self {
            rows: info.total_rows,
            columns: info.headers.len(),
            indexed_bytes: info.index_progress.bytes_scanned,
            indexing_done: info.index_progress.done,
            filter: info
                .filter_progress
                .map(|p| (p.bytes_scanned, p.matches_found, p.done)),
            dirty: info.dirty,
            can_undo: info.can_undo,
            can_redo: info.can_redo,
        }
    }
}

type ResponseSender = mpsc::SyncSender<(u64, Option<u64>, Response)>;

fn worker(
    commands: mpsc::Receiver<(u64, Command)>,
    responses: ResponseSender,
    requester: ViewportRequester,
) {
    let mut document: Option<Document> = None;
    let mut generation = 0;
    let mut progress_key = None;
    let mut progress_running = false;
    let mut last_progress = Instant::now();
    loop {
        match commands.recv_timeout(Duration::from_millis(12)) {
            Ok((version, command)) => {
                generation = version;
                let result = match command {
                    Command::Open(path, options) => {
                        Document::open(&path, options).and_then(|next| {
                            let info = next.info()?;
                            document = Some(next);
                            progress_running = jobs_running(&info);
                            progress_key = Some(ProgressKey::from(&info));
                            Ok(Response::Opened(info))
                        })
                    }
                    command => document
                        .as_mut()
                        .ok_or_else(|| "Open a file first.".to_string())
                        .and_then(|document| execute(document, command)),
                };
                let response = result.unwrap_or_else(Response::Error);
                if !matches!(response, Response::Done)
                    && responses.send((generation, None, response)).is_err()
                {
                    break;
                }
                // A failed command may retain the previous document. Report that live state too.
                // Even a second edit while already dirty must refresh the same viewport.
                progress_key = None;
                match publish_info(document.as_ref(), generation, &responses, &mut progress_key) {
                    Ok(running) => progress_running = running,
                    Err(()) => break,
                }
                if responses.send((generation, None, Response::Done)).is_err() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        let request = requester
            .slot
            .lock()
            .ok()
            .and_then(|mut slot| slot.pending.take());
        if let Some(request) = request.filter(|request| {
            request.key.generation == generation
                && generation == requester.generation.load(Ordering::Acquire)
        }) && let Some(document) = document.as_mut()
        {
            let result =
                document.viewport(request.key.start, request.key.count, &request.key.columns);
            if responses
                .send((
                    generation,
                    Some(request.sequence),
                    result
                        .map(|viewport| Response::Viewport {
                            generation,
                            sequence: request.sequence,
                            viewport,
                        })
                        .unwrap_or_else(Response::Error),
                ))
                .is_err()
            {
                break;
            }
            // Reading a ragged row can reveal columns outside the opening sample.
            match publish_info(Some(document), generation, &responses, &mut progress_key) {
                Ok(running) => progress_running = running,
                Err(()) => break,
            }
        }
        if progress_running && last_progress.elapsed() >= Duration::from_millis(100) {
            last_progress = Instant::now();
            match publish_info(document.as_ref(), generation, &responses, &mut progress_key) {
                Ok(running) => progress_running = running,
                Err(()) => break,
            }
        }
    }
}

fn publish_info(
    document: Option<&Document>,
    generation: u64,
    responses: &ResponseSender,
    progress_key: &mut Option<ProgressKey>,
) -> Result<bool, ()> {
    let Some(document) = document else {
        return Ok(false);
    };
    match document.info() {
        Ok(info) => {
            let running = jobs_running(&info);
            let key = ProgressKey::from(&info);
            if progress_key.as_ref() != Some(&key) {
                *progress_key = Some(key);
                responses
                    .send((generation, None, Response::Info(info)))
                    .map_err(|_| ())?;
            }
            Ok(running)
        }
        Err(error) => {
            responses
                .send((generation, None, Response::Error(error)))
                .map_err(|_| ())?;
            Ok(false)
        }
    }
}

fn jobs_running(info: &DocumentInfo) -> bool {
    !info.index_progress.done || info.filter_progress.is_some_and(|progress| !progress.done)
}

fn execute(document: &mut Document, command: Command) -> Result<Response, String> {
    match command {
        Command::Cell(row, column) => Ok(Response::Cell {
            row,
            column,
            value: document.cell_value(row, column)?,
        }),
        Command::Edit(row, column, value) => {
            document.edit(row, column, value).map(|()| Response::Done)
        }
        Command::Undo => document.undo().map(|()| Response::Done),
        Command::Redo => document.redo().map(|()| Response::Done),
        Command::Filter(query) => document.start_filter(query).map(|()| Response::Done),
        Command::ClearFilter => {
            document.clear_filter();
            Ok(Response::Done)
        }
        Command::SaveCopy(path) => {
            document.save_as(&path)?;
            Ok(Response::Saved(path))
        }
        Command::Open(..) => unreachable!("Open is handled before accessing a document"),
    }
}

pub struct Grid {
    requester: ViewportRequester,
    generation: u64,
    info: Option<DocumentInfo>,
    columns: Vec<usize>,
    widths: HashMap<usize, Pixels>,
    viewport: Option<Viewport>,
    visible_rows: Range<usize>,
    visible_columns: Range<usize>,
    retry_request: bool,
    retry_force: bool,
}

impl Grid {
    pub fn new(requester: ViewportRequester) -> Self {
        Self {
            requester,
            generation: 0,
            info: None,
            columns: Vec::new(),
            widths: HashMap::new(),
            viewport: None,
            visible_rows: 0..64,
            visible_columns: 0..16,
            retry_request: false,
            retry_force: false,
        }
    }

    pub fn columns(&self) -> &[usize] {
        &self.columns
    }

    pub fn set_info(&mut self, info: DocumentInfo) {
        let generation = self.requester.generation.load(Ordering::Acquire);
        let changed_view = generation != self.generation;
        self.generation = generation;
        let previous_columns = self.info.as_ref().map_or(0, |info| info.headers.len());
        let changed_file = self.info.as_ref().is_none_or(|old| old.path != info.path);
        let rows_grew = self
            .info
            .as_ref()
            .is_none_or(|old| old.available_rows < info.available_rows);
        if changed_file {
            self.columns = (0..info.headers.len()).collect();
            self.widths.clear();
            self.viewport = None;
            self.visible_rows = 0..64;
            self.visible_columns = 0..16;
        } else {
            self.columns.retain(|&column| column < info.headers.len());
            self.columns.extend(previous_columns..info.headers.len());
        }
        if changed_view {
            self.viewport = None;
            if self.visible_rows.start as u64 >= info.available_rows {
                self.visible_rows = 0..64;
            }
        }
        self.info = Some(info);
        self.request(rows_grew || changed_view);
    }

    pub fn reset_columns(&mut self) {
        self.columns = (0..self.info.as_ref().map_or(0, |info| info.headers.len())).collect();
        self.widths.clear();
        self.visible_columns = 0..16;
        self.invalidate();
    }

    pub fn set_columns(&mut self, columns: Vec<usize>) {
        let count = self.info.as_ref().map_or(0, |info| info.headers.len());
        let mut seen = std::collections::HashSet::new();
        self.columns = columns
            .into_iter()
            .filter(|&column| column < count && seen.insert(column))
            .collect();
        self.visible_columns = 0..16;
        self.invalidate();
    }

    pub fn set_widths(&mut self, widths: &[Pixels]) {
        for (&source, &width) in self.columns.iter().zip(widths.iter().skip(1)) {
            self.widths.insert(source, width);
        }
    }

    pub fn auto_fit(&mut self) {
        if let Some(info) = &self.info {
            for &source in &self.columns {
                let mut length = info
                    .headers
                    .get(source)
                    .map_or(0, |name| name.chars().count());
                if let Some(viewport) = &self.viewport
                    && let Some(column) =
                        viewport.columns.iter().position(|&column| column == source)
                {
                    for row in &viewport.rows {
                        length = length.max(row.cells.get(column).map_or(0, |value| {
                            value
                                .lines()
                                .map(|line| line.chars().count())
                                .max()
                                .unwrap_or(0)
                        }));
                    }
                }
                // ponytail: sampled monospace estimate; measure glyphs if exact fitting is needed.
                self.widths
                    .insert(source, px((length as f32 * 7.5 + 24.).clamp(72., 360.)));
            }
        }
    }

    pub fn set_viewport(&mut self, generation: u64, sequence: u64, viewport: Viewport) {
        if generation == self.requester.generation.load(Ordering::Acquire)
            && sequence == self.requester.sequence.load(Ordering::Acquire)
        {
            self.viewport = Some(viewport);
        }
    }

    pub fn invalidate(&mut self) {
        self.viewport = None;
        self.request(true);
    }

    pub fn cell_at(&self, row: usize, column: usize) -> Option<(u64, usize)> {
        let source_column = *self.columns.get(column.checked_sub(1)?)?;
        let viewport = self.viewport.as_ref()?;
        let index = u64::try_from(row).ok()?.checked_sub(viewport.start)?;
        let source_row = viewport.rows.get(usize::try_from(index).ok()?)?.source_row;
        Some((source_row, source_column))
    }

    fn request(&mut self, force: bool) {
        let Some(info) = &self.info else {
            return;
        };
        let start = self.visible_rows.start.saturating_sub(12) as u64;
        let count = self
            .visible_rows
            .len()
            .saturating_add(24)
            .min(MAX_VIEWPORT_ROWS)
            .min(usize::try_from(info.available_rows.saturating_sub(start)).unwrap_or(usize::MAX));
        let col_start = self
            .visible_columns
            .start
            .saturating_sub(2)
            .min(self.columns.len());
        let col_end = self
            .visible_columns
            .end
            .saturating_add(2)
            .min(self.columns.len());
        self.retry_force |= force;
        self.retry_request = !self.requester.request(
            start,
            count,
            self.columns[col_start..col_end].to_vec(),
            self.retry_force,
        );
        if !self.retry_request {
            self.retry_force = false;
        }
    }

    fn display(&self, row: usize, column: usize) -> String {
        let Some(viewport) = &self.viewport else {
            return String::new();
        };
        let Some(row) = (row as u64)
            .checked_sub(viewport.start)
            .and_then(|index| viewport.rows.get(index as usize))
        else {
            return String::new();
        };
        if column == 0 {
            let header = u64::from(self.info.as_ref().is_some_and(|info| info.has_header));
            return (row.source_row + 1 - header).to_string();
        }
        self.columns
            .get(column - 1)
            .and_then(|source| viewport.columns.iter().position(|column| column == source))
            .and_then(|column| row.cells.get(column))
            .cloned()
            .unwrap_or_default()
    }
}

impl TableDelegate for Grid {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len() + 1
    }
    fn rows_count(&self, _: &App) -> usize {
        self.info.as_ref().map_or(0, |info| {
            usize::try_from(info.available_rows).unwrap_or(usize::MAX)
        })
    }
    fn column(&self, column: usize, _: &App) -> Column {
        if column == 0 {
            return Column::new("source-row", "#")
                .width(px(58.))
                .fixed_left()
                .resizable(false)
                .movable(false)
                .selectable(false)
                .text_right();
        }
        let source = self.columns[column - 1];
        let title = self
            .info
            .as_ref()
            .and_then(|info| info.headers.get(source))
            .cloned()
            .unwrap_or_default();
        Column::new(format!("source-{source}"), title)
            .width(self.widths.get(&source).copied().unwrap_or(px(124.)))
            .min_width(px(60.))
            .max_width(px(900.))
    }
    fn cell_label(&self, row: usize, column: usize, _: &App) -> Option<SharedString> {
        let value = self.display(row, column);
        if column == 0 {
            return (!value.is_empty()).then(|| format!("Row {value}").into());
        }
        let source = *self.columns.get(column - 1)?;
        let header = self
            .info
            .as_ref()
            .and_then(|info| info.headers.get(source))
            .map(String::as_str)
            .unwrap_or("Column");
        if value.is_empty() {
            Some(format!("{header}: empty").into())
        } else {
            Some(format!("{header}: {value}").into())
        }
    }
    fn render_td(
        &mut self,
        row: usize,
        column: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        if self.retry_request {
            self.request(false);
        }
        div()
            .size_full()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_size(px(12.))
            .font_family("Menlo")
            .text_color(if column == 0 {
                rgb(0x9b7624).into()
            } else {
                cx.theme().foreground
            })
            .child(self.display(row, column))
    }
    fn visible_rows_changed(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) {
        self.visible_rows = range;
        self.request(false);
    }
    fn visible_columns_changed(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) {
        // GPUI's horizontal virtual list excludes fixed columns, including our source-row gutter.
        self.visible_columns = range;
        self.request(false);
    }
    fn move_column(
        &mut self,
        column: usize,
        to: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        if column > 0 && to > 0 && column <= self.columns.len() && to <= self.columns.len() {
            let source = self.columns.remove(column - 1);
            self.columns.insert(to - 1, source);
            self.invalidate();
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::ViewportRow;

    fn until(
        bridge: &Bridge,
        mut grid: Option<&mut Grid>,
        matches: impl Fn(&Response) -> bool,
    ) -> Vec<Response> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut responses = Vec::new();
        loop {
            if let Some(grid) = grid.as_deref_mut()
                && grid.retry_request
            {
                grid.request(false);
            }
            let batch = bridge.drain();
            for response in &batch {
                if let Response::Error(error) = response {
                    panic!("engine error: {error}");
                }
            }
            let done = batch.iter().any(&matches);
            responses.extend(batch);
            if done {
                return responses;
            }
            assert!(Instant::now() < deadline, "engine reply timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn repeated_edits_refresh_the_same_viewport_and_each_command_completes() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("sample.csv");
        std::fs::write(&source, "name,value\nfirst,1\nsecond,2\n").unwrap();
        let bridge = Bridge::new();
        let mut grid = Grid::new(bridge.requester());
        bridge
            .send(Command::Open(
                source,
                OpenOptions {
                    header_mode: quarry_core::HeaderMode::FirstRow,
                    ..OpenOptions::default()
                },
            ))
            .unwrap();
        let opened = until(&bridge, None, |response| matches!(response, Response::Done));
        assert!(
            opened
                .iter()
                .any(|response| matches!(response, Response::Opened(_)))
        );
        let info = opened
            .into_iter()
            .find_map(|response| match response {
                Response::Info(info) => Some(info),
                _ => None,
            })
            .unwrap();
        grid.set_info(info);
        for value in ["edited once", "edited twice"] {
            let (generation, sequence, viewport) = until(&bridge, Some(&mut grid), |response| {
                matches!(response, Response::Viewport { .. })
            })
            .into_iter()
            .find_map(|response| match response {
                Response::Viewport {
                    generation,
                    sequence,
                    viewport,
                } => Some((generation, sequence, viewport)),
                _ => None,
            })
            .unwrap();
            grid.set_viewport(generation, sequence, viewport);
            assert_eq!(grid.cell_at(0, 1), Some((1, 0)));
            assert_eq!(grid.display(0, 0), "1");
            bridge.send(Command::Edit(1, 0, value.into())).unwrap();
            let replies = until(&bridge, None, |response| matches!(response, Response::Done));
            let info = replies
                .into_iter()
                .find_map(|response| match response {
                    Response::Info(info) => Some(info),
                    _ => None,
                })
                .expect("every edit sends updated info, even when already dirty");
            assert!(info.dirty);
            grid.set_info(info);
            assert!(grid.viewport.is_none(), "an edit invalidates cached cells");
        }
        bridge.send(Command::Cell(1, 0)).unwrap();
        let replies = until(&bridge, None, |response| matches!(response, Response::Done));
        assert!(replies.iter().any(|response| matches!(response,
            Response::Cell { value, .. } if value == "edited twice")));
        bridge
            .send(Command::SaveCopy(directory.path().join("saved.csv")))
            .unwrap();
        let replies = until(&bridge, None, |response| matches!(response, Response::Done));
        assert!(
            replies
                .iter()
                .any(|response| matches!(response, Response::Saved(_)))
        );
    }

    #[test]
    fn viewport_requests_coalesce_and_stale_results_do_not_reach_the_grid() {
        let requester = ViewportRequester {
            slot: Arc::new(Mutex::new(ViewportSlot::default())),
            generation: Arc::new(AtomicU64::new(0)),
            sequence: Arc::new(AtomicU64::new(0)),
        };
        requester.request(0, 2000, (0..100).collect(), false);
        requester.request(900, 32, vec![299, 0, 150], false);
        let slot = requester.slot.lock().unwrap();
        let request = slot.pending.as_ref().unwrap();
        assert_eq!(request.key.start, 900);
        assert_eq!(request.key.columns, [299, 0, 150]);
        assert_eq!(request.key.count, 32);
        drop(slot);
        let (commands, _command_rx) = mpsc::sync_channel(8);
        let (response_tx, responses) = mpsc::sync_channel(16);
        let bridge = Bridge {
            commands,
            responses,
            requester: requester.clone(),
        };
        response_tx.send((0, Some(1), Response::Done)).unwrap();
        response_tx.send((0, Some(2), Response::Done)).unwrap();
        response_tx.send((99, None, Response::Done)).unwrap();
        assert_eq!(bridge.drain().len(), 1);
        let mut grid = Grid::new(requester);
        grid.columns = vec![299, 0, 150];
        let sample = Viewport {
            start: 900,
            columns: vec![299, 0, 150],
            rows: vec![ViewportRow {
                source_row: 1201,
                cells: vec!["last".into(), "first".into(), "middle".into()],
            }],
        };
        // Stale sequence must be rejected at installation time even if drained in the same batch.
        grid.set_viewport(0, 1, sample.clone());
        assert_eq!(grid.cell_at(900, 1), None);
        grid.set_viewport(0, 2, sample);
        assert_eq!(grid.cell_at(900, 1), Some((1201, 299)));
        assert_eq!(grid.cell_at(900, 3), Some((1201, 150)));
        assert_eq!(grid.cell_at(900, 0), None);
        assert_eq!(grid.display(900, 0), "1202");
        assert_eq!(grid.display(900, 2), "first");
        assert_eq!(grid.cell_at(899, 1), None);
    }
}
