//! Worker-owned adapter for the comparison UI. Reads and job destruction can block.
//! The engine parses complete requested records before projection, as in egui.
//! Its 64 MiB record limit and our 256-row limit permit 16 GiB of field payload
//! in a pathological request, plus allocation overhead. Display and history caps
//! are separate limits, not a total-memory guarantee or a bound on sparse edits.
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

use quarry_core::{
    FilterJob, FilterProgress, FilterQuery, IndexConfig, IndexJob, IndexProgress, OpenOptions, Row,
    SaveAsOutcome, Session,
};

pub const MAX_VIEWPORT_ROWS: usize = 256;
pub const MAX_VIEWPORT_COLUMNS: usize = 64;
pub const MAX_EDIT_BYTES: usize = 1024 * 1024;
const DISPLAY_BYTES: usize = 512;
const MAX_HISTORY_ENTRIES: usize = 1_000;
const MAX_HISTORY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct DocumentInfo {
    pub path: PathBuf,
    pub file_size: u64,
    pub headers: Vec<String>,
    pub delimiter: u8,
    pub has_header: bool,
    /// Available unfiltered data rows, growing until indexing finishes.
    pub total_rows: u64,
    /// Available data rows or filter matches in the current view.
    pub available_rows: u64,
    pub index_progress: IndexProgress,
    pub filter_progress: Option<FilterProgress>,
    pub dirty: bool,
    pub can_undo: bool,
    pub can_redo: bool,
}

#[derive(Clone, Debug)]
pub struct Viewport {
    pub start: u64,
    /// Original source-column identities, in the requested display order.
    pub columns: Vec<usize>,
    pub rows: Vec<ViewportRow>,
}

#[derive(Clone, Debug)]
pub struct ViewportRow {
    /// Zero-based physical source record, including any header record.
    pub source_row: u64,
    /// Bounded display text. Use cell_value for the complete editable value.
    pub cells: Vec<String>,
}

struct Edit {
    key: (u64, usize),
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
}

impl Edit {
    fn bytes(&self) -> usize {
        self.before.as_ref().map_or(0, Vec::len) + self.after.as_ref().map_or(0, Vec::len)
    }
}

pub struct Document {
    session: Session,
    index: IndexJob,
    filter: Option<FilterJob>,
    headers: Vec<String>,
    edits: BTreeMap<(u64, usize), Vec<u8>>,
    undo: VecDeque<Edit>,
    redo: Vec<Edit>,
    history_bytes: usize,
}

impl Document {
    pub fn open(path: &Path, options: OpenOptions) -> Result<Self, String> {
        let session = Session::open(
            path,
            OpenOptions {
                rows: options.rows.clamp(1, MAX_VIEWPORT_ROWS + 1),
                ..options
            },
        )
        .map_err(|error| error.to_string())?;
        let index = session
            .start_indexing(IndexConfig::default())
            .map_err(|error| error.to_string())?;
        let column_count = session
            .first_rows
            .iter()
            .map(|row| row.fields.len())
            .max()
            .unwrap_or(0);
        let mut document = Self {
            session,
            index,
            filter: None,
            headers: Vec::new(),
            edits: BTreeMap::new(),
            undo: VecDeque::new(),
            redo: Vec::new(),
            history_bytes: 0,
        };
        document.extend_headers(column_count);
        Ok(document)
    }

    pub fn info(&self) -> Result<DocumentInfo, String> {
        self.check_errors()?;
        let index_progress = self.index.progress();
        let total_rows = index_progress
            .rows_scanned
            .max(self.session.first_rows.len() as u64)
            .saturating_sub(self.data_start());
        let filter_progress = self.filter.as_ref().map(FilterJob::progress);
        let mut headers = self.headers.clone();
        let discovered_columns = index_progress.columns_found.min(65_536);
        for column in headers.len()..discovered_columns {
            headers.push(self.header_name(column));
        }
        Ok(DocumentInfo {
            path: self.session.path().to_path_buf(),
            file_size: self.session.file_size,
            headers,
            delimiter: self.session.dialect.delimiter,
            has_header: self.session.dialect.has_header,
            total_rows,
            available_rows: filter_progress.map_or(total_rows, |progress| progress.matches_found),
            index_progress,
            filter_progress,
            dirty: !self.edits.is_empty(),
            can_undo: self.filter.is_none() && !self.undo.is_empty(),
            can_redo: self.filter.is_none() && !self.redo.is_empty(),
        })
    }

    pub fn viewport(
        &mut self,
        start: u64,
        count: usize,
        columns: &[usize],
    ) -> Result<Viewport, String> {
        self.check_source()?;
        let columns: Vec<usize> = columns.iter().copied().take(MAX_VIEWPORT_COLUMNS).collect();
        let mut viewport = Viewport {
            start,
            columns,
            rows: Vec::new(),
        };
        let count = count.min(MAX_VIEWPORT_ROWS);
        if count == 0 {
            return Ok(viewport);
        }
        let rows: Vec<(u64, Row)> = if let Some(filter) = &self.filter {
            let snapshot = filter.snapshot();
            if start >= snapshot.matches_found() {
                return Ok(viewport);
            }
            self.session
                .read_filtered_rows(&snapshot, start, count)
                .map_err(|error| error.to_string())?
                .into_iter()
                .map(|found| {
                    (
                        found.row,
                        Row {
                            offset: found.record_offset,
                            fields: found.fields,
                        },
                    )
                })
                .collect()
        } else {
            let start = start.saturating_add(self.data_start());
            self.read_source_rows(start, count)?
                .into_iter()
                .enumerate()
                .map(|(offset, row)| (start + offset as u64, row))
                .collect()
        };
        for (source_row, row) in rows {
            self.extend_headers(row.fields.len());
            viewport.rows.push(ViewportRow {
                source_row,
                cells: viewport
                    .columns
                    .iter()
                    .map(|&column| {
                        let bytes = self
                            .edits
                            .get(&(source_row, column))
                            .or_else(|| row.fields.get(column))
                            .map_or(&[][..], Vec::as_slice);
                        display_text(bytes)
                    })
                    .collect(),
            });
        }
        Ok(viewport)
    }

    pub fn cell_value(&self, source_row: u64, column: usize) -> Result<String, String> {
        self.check_editable()?;
        let source = self.source_cell(source_row, column)?;
        let bytes = self.edits.get(&(source_row, column)).unwrap_or(&source);
        if bytes.len() > MAX_EDIT_BYTES {
            return Err("The comparison editor supports cells up to 1 MiB.".into());
        }
        String::from_utf8(bytes.clone())
            .map_err(|_| "This cell is not valid UTF-8 and cannot be edited.".into())
    }

    pub fn edit(&mut self, source_row: u64, column: usize, value: String) -> Result<(), String> {
        self.check_editable()?;
        if value.len() > MAX_EDIT_BYTES {
            return Err("The comparison editor supports cells up to 1 MiB.".into());
        }
        let source = self.source_cell(source_row, column)?;
        std::str::from_utf8(&source)
            .map_err(|_| "This cell is not valid UTF-8 and cannot be edited.".to_string())?;
        if source.len() > MAX_EDIT_BYTES {
            return Err("The comparison editor supports cells up to 1 MiB.".into());
        }
        let key = (source_row, column);
        let before = self.edits.get(&key).cloned();
        let after = (value.as_bytes() != source).then(|| value.into_bytes());
        if before != after {
            self.set_edit(key, after.clone());
            for obsolete in self.redo.drain(..) {
                self.history_bytes -= obsolete.bytes();
            }
            let edit = Edit { key, before, after };
            let bytes = edit.bytes();
            while self.undo.len() >= MAX_HISTORY_ENTRIES
                || self.history_bytes.saturating_add(bytes) > MAX_HISTORY_BYTES
            {
                let Some(obsolete) = self.undo.pop_front() else {
                    // An untracked edit is a history boundary, never jump over it.
                    return Ok(());
                };
                self.history_bytes -= obsolete.bytes();
            }
            self.history_bytes += bytes;
            self.undo.push_back(edit);
        }
        Ok(())
    }

    pub fn undo(&mut self) -> Result<(), String> {
        self.check_editable()?;
        if let Some(edit) = self.undo.pop_back() {
            self.set_edit(edit.key, edit.before.clone());
            self.redo.push(edit);
        }
        Ok(())
    }

    pub fn redo(&mut self) -> Result<(), String> {
        self.check_editable()?;
        if let Some(edit) = self.redo.pop() {
            self.set_edit(edit.key, edit.after.clone());
            self.undo.push_back(edit);
        }
        Ok(())
    }

    pub fn start_filter(&mut self, query: FilterQuery) -> Result<(), String> {
        self.check_source()?;
        if !self.edits.is_empty() {
            return Err("Save a copy and open it, or undo edits, before filtering.".into());
        }
        query.validate().map_err(|error| error.to_string())?;
        self.extend_headers(self.index.progress().columns_found);
        if query
            .predicates
            .iter()
            .any(|rule| rule.column >= self.headers.len())
        {
            return Err("Choose a column from this file.".into());
        }
        let next = self
            .session
            .start_filter(query)
            .map_err(|error| error.to_string())?;
        self.filter = Some(next);
        Ok(())
    }

    pub fn clear_filter(&mut self) {
        self.filter = None;
    }

    /// Save a complete copy with edits. Never replace the source or another file.
    /// The source session and its dirty state are retained after saving a copy.
    pub fn save_as(&self, destination: &Path) -> Result<(), String> {
        self.check_source()?;
        if self.filter.is_some() {
            return Err("Clear the filter before saving a complete copy.".into());
        }
        match self
            .session
            .start_save_as_with_edits(BTreeMap::new(), self.edits.clone(), destination)
            .map_err(|error| error.to_string())?
            .wait()
            .map_err(|error| error.to_string())?
        {
            SaveAsOutcome::Complete(_) => Ok(()),
            SaveAsOutcome::Cancelled => Err("Saving the copy was cancelled.".into()),
        }
    }

    fn data_start(&self) -> u64 {
        u64::from(self.session.dialect.has_header)
    }

    fn check_errors(&self) -> Result<(), String> {
        if let Some(error) = self
            .index
            .error()
            .or_else(|| self.filter.as_ref().and_then(FilterJob::error))
        {
            return Err(error);
        }
        Ok(())
    }

    fn check_source(&self) -> Result<(), String> {
        self.check_errors()?;
        self.session
            .ensure_source_unchanged()
            .map_err(|error| error.to_string())
    }

    fn check_editable(&self) -> Result<(), String> {
        self.check_source()?;
        if self.filter.is_some() {
            return Err("Clear the filter before editing cells.".into());
        }
        Ok(())
    }

    fn read_source_rows(&self, start: u64, count: usize) -> Result<Vec<Row>, String> {
        let snapshot = self.index.snapshot();
        let bootstrap_len = self.session.first_rows.len() as u64;
        if start < snapshot.indexed_rows()
            && (snapshot.indexed_rows() >= bootstrap_len
                || start.saturating_add(count as u64) <= snapshot.indexed_rows())
        {
            let rows = self
                .session
                .read_rows(&snapshot, start, count)
                .map_err(|error| error.to_string())?;
            self.check_source()?;
            return Ok(rows);
        }
        // Bootstrap rows remain usable while the progressive index catches up.
        Ok(self
            .session
            .first_rows
            .iter()
            .skip(usize::try_from(start).unwrap_or(usize::MAX))
            .take(count)
            .cloned()
            .collect())
    }

    fn source_cell(&self, row: u64, column: usize) -> Result<Vec<u8>, String> {
        if row < self.data_start() {
            return Err("Choose a data cell to edit.".into());
        }
        self.read_source_rows(row, 1)?
            .first()
            .and_then(|row| row.fields.get(column))
            .cloned()
            .ok_or_else(|| {
                "This row has no cell in this source column, or is not indexed yet.".into()
            })
    }

    fn set_edit(&mut self, key: (u64, usize), value: Option<Vec<u8>>) {
        if let Some(value) = value {
            self.edits.insert(key, value);
        } else {
            self.edits.remove(&key);
        }
    }

    fn header_name(&self, column: usize) -> String {
        self.session
            .first_rows
            .first()
            .filter(|_| self.session.dialect.has_header)
            .and_then(|row| row.fields.get(column))
            .map_or_else(
                || format!("Column {}", column + 1),
                |bytes| display_text(bytes),
            )
    }

    fn extend_headers(&mut self, count: usize) {
        let limit = count.min(65_536);
        for column in self.headers.len()..limit {
            self.headers.push(self.header_name(column));
        }
    }
}

fn display_text(bytes: &[u8]) -> String {
    let mut end = bytes.len().min(DISPLAY_BYTES);
    // Avoid cutting a valid UTF-8 scalar while bounding decoding work for huge cells.
    if end < bytes.len() {
        while end > 0 && (bytes[end] & 0b1100_0000) == 0b1000_0000 {
            end -= 1;
        }
    }
    let mut text = String::from_utf8_lossy(&bytes[..end]).into_owned();
    if end < bytes.len() {
        text.push('…');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use quarry_core::{CaseSensitivity, FilterOperator, FilterPredicate, HeaderMode};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    static NEXT_CASE: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "quarry-gpui-{}-{}",
                std::process::id(),
                NEXT_CASE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::write(path.join("source.csv"), bytes).unwrap();
            Self(path)
        }

        fn open(&self, header_mode: HeaderMode) -> Document {
            Document::open(
                &self.0.join("source.csv"),
                OpenOptions {
                    rows: 2,
                    header_mode,
                    ..OpenOptions::default()
                },
            )
            .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn wait_ready(document: &Document) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let info = document.info().unwrap();
            if info.index_progress.done && info.filter_progress.is_none_or(|progress| progress.done)
            {
                break;
            }
            assert!(Instant::now() < deadline, "engine jobs did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn wide_viewport_preserves_source_identity_edits_and_save_copy() {
        let mut csv = (0..300)
            .map(|column| format!("column{column}"))
            .collect::<Vec<_>>()
            .join(",")
            + "\n";
        for row in 0..400 {
            csv.push_str(
                &(0..300)
                    .map(|column| format!("r{row}c{column}"))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            csv.push('\n');
        }
        let fixture = Fixture::new(csv.as_bytes());
        let mut document = fixture.open(HeaderMode::FirstRow);
        assert!(document.info().unwrap().available_rows >= 1);
        wait_ready(&document);
        let viewport = document.viewport(300, 2, &[299, 0, 150]).unwrap();
        assert_eq!(viewport.columns, [299, 0, 150]);
        assert_eq!(viewport.rows[0].source_row, 301);
        assert_eq!(viewport.rows[0].cells, ["r300c299", "r300c0", "r300c150"]);
        document.edit(301, 299, "a,quoted\nvalue".into()).unwrap();
        assert_eq!(
            document.viewport(300, 1, &[299]).unwrap().rows[0].cells,
            ["a,quoted\nvalue"]
        );
        document.undo().unwrap();
        assert!(!document.info().unwrap().dirty);
        document.redo().unwrap();
        let destination = fixture.0.join("copy.csv");
        document.save_as(&destination).unwrap();
        assert_eq!(
            fs::read(fixture.0.join("source.csv")).unwrap(),
            csv.as_bytes()
        );
        let mut saved = Document::open(
            &destination,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        wait_ready(&saved);
        assert_eq!(
            saved.viewport(300, 1, &[299]).unwrap().rows[0].cells,
            ["a,quoted\nvalue"]
        );
        assert!(document.save_as(&destination).is_err());
        assert!(document.save_as(&fixture.0.join("source.csv")).is_err());
        assert!(
            document
                .save_as(&fixture.0.join("missing/copy.csv"))
                .is_err()
        );
        assert!(document.info().unwrap().dirty);
        let bounded = document
            .viewport(0, usize::MAX, &(0..300).collect::<Vec<_>>())
            .unwrap();
        assert_eq!(bounded.rows.len(), MAX_VIEWPORT_ROWS);
        assert_eq!(bounded.columns.len(), MAX_VIEWPORT_COLUMNS);
        assert!(
            document
                .viewport(u64::MAX, 1, &[0])
                .unwrap()
                .rows
                .is_empty()
        );
    }

    #[test]
    fn filters_use_engine_rules_and_physical_rows_with_edit_guards() {
        let fixture = Fixture::new(
            b"name,state,amount\n\"one\ncontinued\",NV,10\ntwo,TX,20\nthree,nv,30\nfour,NV,40\n",
        );
        let mut document = fixture.open(HeaderMode::FirstRow);
        wait_ready(&document);
        let query = FilterQuery {
            predicates: vec![
                FilterPredicate {
                    column: 1,
                    operator: FilterOperator::Equals,
                    value: b"NV".to_vec(),
                    upper_bound: None,
                },
                FilterPredicate {
                    column: 2,
                    operator: FilterOperator::Between,
                    value: b"10".to_vec(),
                    upper_bound: Some(b"30".to_vec()),
                },
            ],
            case_sensitivity: CaseSensitivity::Insensitive,
        };
        document.edit(1, 1, "TX".into()).unwrap();
        assert!(document.start_filter(query.clone()).is_err());
        document.undo().unwrap();
        document.start_filter(query.clone()).unwrap();
        wait_ready(&document);
        assert_eq!(document.info().unwrap().available_rows, 2);
        let viewport = document.viewport(0, 10, &[2, 0]).unwrap();
        assert_eq!(
            viewport
                .rows
                .iter()
                .map(|row| row.source_row)
                .collect::<Vec<_>>(),
            [1, 3]
        );
        assert_eq!(viewport.rows[0].cells, ["10", "one\ncontinued"]);
        assert!(document.edit(1, 1, "TX".into()).is_err());
        let mut invalid = query;
        invalid.predicates[1].upper_bound = Some(b"not a number".to_vec());
        assert!(document.start_filter(invalid).is_err());
        assert_eq!(document.info().unwrap().available_rows, 2);
        document.clear_filter();
        document.edit(1, 1, "TX".into()).unwrap();
    }

    #[test]
    fn editor_rejects_missing_invalid_utf8_and_changed_source() {
        let fixture = Fixture::new(b"name,value\nvalid,\xff\nshort\n");
        let mut document = fixture.open(HeaderMode::FirstRow);
        wait_ready(&document);
        assert!(document.cell_value(1, 1).unwrap_err().contains("UTF-8"));
        assert!(document.edit(1, 1, "replacement".into()).is_err());
        assert!(document.edit(2, 1, "missing".into()).is_err());
        assert!(document.edit(0, 0, "header".into()).is_err());
        assert!(document.edit(1, 0, "x".repeat(MAX_EDIT_BYTES + 1)).is_err());
        fs::write(fixture.0.join("source.csv"), b"replacement\n").unwrap();
        assert!(document.viewport(0, 1, &[0]).is_err());
        assert!(document.edit(1, 0, "changed".into()).is_err());
        assert!(document.save_as(&fixture.0.join("copy.csv")).is_err());
    }

    #[test]
    fn format_and_display_limits_keep_full_cell_for_editing() {
        let long = "é".repeat(400);
        let fixture = Fixture::new(format!("alpha;{long}\nbeta;value\n").as_bytes());
        let mut document = Document::open(
            &fixture.0.join("source.csv"),
            OpenOptions {
                delimiter: Some(b';'),
                header_mode: HeaderMode::NoHeader,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        wait_ready(&document);
        assert_eq!(document.info().unwrap().headers, ["Column 1", "Column 2"]);
        assert_eq!(document.info().unwrap().total_rows, 2);
        let viewport = document.viewport(0, 1, &[1]).unwrap();
        assert_eq!(viewport.rows[0].source_row, 0);
        assert_eq!(viewport.rows[0].cells[0], "é".repeat(256) + "…");
        assert_eq!(document.cell_value(0, 1).unwrap(), long);
    }

    #[test]
    fn history_limits_entries_and_bytes_without_skipping_edits() {
        let fixture = Fixture::new(b"name\noriginal\n");
        let mut document = fixture.open(HeaderMode::FirstRow);
        wait_ready(&document);
        for value in 0..MAX_HISTORY_ENTRIES + 2 {
            document.edit(1, 0, value.to_string()).unwrap();
        }
        assert_eq!(document.undo.len(), MAX_HISTORY_ENTRIES);
        while document.info().unwrap().can_undo {
            document.undo().unwrap();
        }
        assert_eq!(document.cell_value(1, 0).unwrap(), "1");
        assert!(document.info().unwrap().dirty);
        for value in 0..12 {
            document
                .edit(
                    1,
                    0,
                    char::from(b'a' + value).to_string().repeat(MAX_EDIT_BYTES),
                )
                .unwrap();
            assert!(document.history_bytes <= MAX_HISTORY_BYTES);
        }
        assert!(document.redo.is_empty());
        let bytes = document.history_bytes;
        let entries = document.undo.len();
        assert!(entries < 12);
        for _ in 0..entries {
            document.undo().unwrap();
            assert_eq!(document.history_bytes, bytes);
        }
        assert_ne!(document.cell_value(1, 0).unwrap(), "original");
        for _ in 0..entries {
            document.redo().unwrap();
            assert_eq!(document.history_bytes, bytes);
        }
        assert_eq!(
            document.cell_value(1, 0).unwrap(),
            "l".repeat(MAX_EDIT_BYTES)
        );
        document.undo().unwrap();
        document.edit(1, 0, "replacement".into()).unwrap();
        assert!(document.redo.is_empty());
        assert!(document.history_bytes <= MAX_HISTORY_BYTES);
        assert_eq!(
            document.history_bytes,
            document.undo.iter().map(Edit::bytes).sum::<usize>()
        );
    }
}
