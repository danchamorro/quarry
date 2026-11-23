//! Frontend-neutral state machines for multi-file workflows (`CombineWorkflow`,
//! `JoinWorkflow`) and file inspection/recovery (`InspectWorkflow`).

use crate::compressed::{COMPRESSED_DESTINATION, DecompressionSlot, DecompressionSlots};
use crate::{CapturedDocument, DelimiterMode, Document, DocumentId, DocumentProbe, FailedOpen};
use quarry_core::{
    CombineJob, CombineOptions, CombinePlan, CombineProgress, CombineSummary, CompressedOpenConfig,
    CompressedRawDecompressJob, CompressedSourceGuard, DecompressedCopy, Diagnostic, Encoding,
    HeaderMode, InspectionEdits, InspectionJob, InspectionOptions, InspectionProgress,
    InspectionReport, InspectionStatus, JoinCase, JoinConfig, JoinFormat, JoinHeader, JoinInput,
    JoinJob, JoinMode, JoinOutputColumn, JoinOutputOptions, JoinPlan, JoinProgress, JoinSource,
    JoinSummary, QuarryError, RecoveryJob, RecoverySummary, Session, SourcePreview, StorageSpace,
    WorkingCopyCleanup, analyze_join_with_working, check_combine_files_with_encoding,
    check_storage, read_join_header_with_format, read_join_working_header,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

// =============================================================================
// Combine Files workflow
// =============================================================================

/// UI-neutral controller for strict exact-schema multi-file combination.
pub struct CombineWorkflow {
    pub paths: Vec<PathBuf>,
    pub has_header: bool,
    pub delimiter: DelimiterMode,
    pub encoding: Option<Encoding>,
    pub plan: Option<CombinePlan>,
    checking: Option<CombineJob<CombinePlan>>,
    writing: Option<CombineJob<CombineSummary>>,
    storage_check: Option<JoinHandle<Result<StorageSpace, String>>>,
    pub destination: Option<PathBuf>,
    pub space: Option<StorageSpace>,
    pub result: Option<CombineSummary>,
    pub message: Option<String>,
    pub problem_path: Option<PathBuf>,
    pub cancel_requested: bool,
}

impl Default for CombineWorkflow {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            has_header: true,
            delimiter: DelimiterMode::Auto,
            encoding: None,
            plan: None,
            checking: None,
            writing: None,
            storage_check: None,
            destination: None,
            space: None,
            result: None,
            message: None,
            problem_path: None,
            cancel_requested: false,
        }
    }
}

impl CombineWorkflow {
    pub fn busy(&self) -> bool {
        self.checking.is_some() || self.writing.is_some() || self.storage_check.is_some()
    }

    pub fn running_writer(&self) -> bool {
        self.checking.is_some() || self.writing.is_some()
    }

    pub fn reserved_output(&self) -> Option<&Path> {
        self.destination.as_deref()
    }

    pub fn check_progress(&self) -> Option<CombineProgress> {
        self.checking.as_ref().map(CombineJob::progress)
    }

    pub fn write_progress(&self) -> Option<CombineProgress> {
        self.writing.as_ref().map(CombineJob::progress)
    }

    pub fn invalidate(&mut self) {
        self.plan = None;
        self.destination = None;
        self.space = None;
        self.result = None;
        self.message = None;
        self.problem_path = None;
    }

    pub fn add_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        if self.busy() {
            return;
        }
        let mut changed = false;
        for path in paths {
            if !self.paths.contains(&path) {
                self.paths.push(path);
                changed = true;
            }
        }
        if changed {
            self.invalidate();
        }
    }

    pub fn remove_path(&mut self, index: usize) {
        if self.busy() || index >= self.paths.len() {
            return;
        }
        self.paths.remove(index);
        self.invalidate();
    }

    pub fn move_path(&mut self, from: usize, to: usize) {
        if self.busy() || from >= self.paths.len() || to >= self.paths.len() || from == to {
            return;
        }
        let path = self.paths.remove(from);
        self.paths.insert(to, path);
        self.invalidate();
    }

    pub fn set_options(
        &mut self,
        delimiter: DelimiterMode,
        has_header: bool,
        encoding: Option<Encoding>,
    ) {
        if self.busy() {
            return;
        }
        if self.delimiter != delimiter || self.has_header != has_header || self.encoding != encoding
        {
            self.delimiter = delimiter;
            self.has_header = has_header;
            self.encoding = encoding;
            self.invalidate();
        }
    }

    fn record_error(&mut self, error: QuarryError) {
        self.problem_path = match &error {
            QuarryError::CombineFile { path, .. } => Some(path.clone()),
            _ => None,
        };
        self.message = Some(Diagnostic::from_error(&error).details());
    }

    pub fn start_check(&mut self, write_slot_available: bool) {
        if self.busy() {
            return;
        }
        if !write_slot_available {
            self.message = Some("Two file operations are already running.".into());
            return;
        }
        self.invalidate();
        self.cancel_requested = false;
        match check_combine_files_with_encoding(
            self.paths.clone(),
            CombineOptions {
                delimiter: self.delimiter.delimiter(),
                has_header: self.has_header,
            },
            self.encoding,
        ) {
            Ok(job) => self.checking = Some(job),
            Err(error) => self.record_error(error),
        }
    }

    pub fn prepare_destination(
        &mut self,
        destination: PathBuf,
        is_protected: impl Fn(&Path) -> bool,
    ) {
        if self.busy() {
            return;
        }
        if quarry_core::is_compressed_destination(&destination) {
            self.space = None;
            self.destination = None;
            self.message = Some(COMPRESSED_DESTINATION.into());
            return;
        }
        if is_protected(&destination) {
            self.space = None;
            self.destination = None;
            self.message = Some(
                "This file is open or being written in another tab. Choose another output.".into(),
            );
            return;
        }
        let Some(plan) = self.plan.as_ref() else {
            return;
        };
        self.message = None;
        self.space = None;
        let required = plan.output_bytes;
        let path = destination.clone();
        self.storage_check = Some(thread::spawn(move || {
            if path.try_exists().map_err(|error| error.to_string())? {
                return Err(
                    "The output file already exists. Choose a new file name; existing files are never overwritten."
                        .into(),
                );
            }
            check_storage(
                path.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
                required,
            )
            .map_err(|error| error.to_string())
        }));
        self.destination = Some(destination);
    }

    pub fn start_write(
        &mut self,
        write_slot_available: bool,
        is_protected: impl Fn(&Path) -> bool,
    ) {
        if self.busy() {
            return;
        }
        if !write_slot_available {
            self.message = Some("Two file operations are already running.".into());
            return;
        }
        let Some(destination) = self.destination.clone() else {
            return;
        };
        if self.space.is_none() {
            return;
        }
        if is_protected(&destination) {
            self.message = Some(
                "The output is now reserved by another operation. Choose a different path.".into(),
            );
            return;
        }
        let Some(plan) = self.plan.take() else {
            return;
        };
        self.message = None;
        self.cancel_requested = false;
        match plan.start_write(destination) {
            Ok(job) => self.writing = Some(job),
            Err(error) => {
                self.record_error(error);
                self.space = None;
            }
        }
    }

    pub fn cancel(&mut self) {
        if let Some(job) = &self.checking {
            job.cancel();
        }
        if let Some(job) = &self.writing {
            job.cancel();
        }
        self.cancel_requested = true;
    }

    /// Returns `true` when any job transitioned or updated visible state.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if self
            .checking
            .as_ref()
            .is_some_and(|job| job.progress().done)
        {
            changed = true;
            match self.checking.take().unwrap().wait() {
                Ok(Some(plan)) => self.plan = Some(plan),
                Ok(None) => {
                    self.message = Some("File check cancelled. No output was written.".into());
                }
                Err(error) => self.record_error(error),
            }
            self.cancel_requested = false;
        }
        if self
            .storage_check
            .as_ref()
            .is_some_and(|job| job.is_finished())
        {
            changed = true;
            match self.storage_check.take().unwrap().join() {
                Ok(Ok(space)) => self.space = Some(space),
                Ok(Err(error)) => self.message = Some(error),
                Err(_) => {
                    self.message =
                        Some("The storage check failed. Choose the output again.".into());
                }
            }
        }
        if self.writing.as_ref().is_some_and(|job| job.progress().done) {
            changed = true;
            match self.writing.take().unwrap().wait() {
                Ok(Some(result)) => self.result = Some(result),
                Ok(None) => {
                    self.message =
                        Some("Combination cancelled. No combined file was published.".into());
                }
                Err(error) => {
                    self.record_error(error);
                    self.plan = None;
                    self.space = None;
                }
            }
            self.cancel_requested = false;
        }
        changed
    }
}

// =============================================================================
// 4-mode Join Files workflow (Inner, Left, Right, Full Outer) with DocumentLeases
// =============================================================================

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinOutputColumnDraft {
    pub source: JoinSource,
    pub column: usize,
    pub included: bool,
    pub name: String,
}

#[derive(Clone, Debug)]
pub enum JoinSideSource {
    File(PathBuf),
    OpenTab {
        doc_id: DocumentId,
        label: String,
        path: PathBuf,
    },
}

/// UI-neutral controller for 4-mode Join Files with composite keys, output column ordering,
/// Preview, safety confirmations, and `DocumentLease` (`JOIN_INPUT_RESERVED`) protection.
pub struct JoinWorkflow {
    pub sides: [Option<JoinSideSource>; 2],
    captured: [Option<CapturedDocument>; 2],
    probes: [Option<DocumentProbe>; 2],
    pub headers: [Option<JoinHeader>; 2],
    pub formats: [JoinFormat; 2],
    pub mode: JoinMode,
    pub keys: Vec<[usize; 2]>,
    pub ignore_ascii_case: bool,
    pub output_columns: Vec<JoinOutputColumnDraft>,
    pub output_has_header: bool,
    pub require_unique: [bool; 2],
    pub acknowledge_expansion: bool,
    pub acknowledge_layout: bool,
    pub temporary: PathBuf,
    header_jobs: [Option<JoinJob<JoinHeader>>; 2],
    checking: Option<JoinJob<JoinPlan>>,
    writing: Option<JoinJob<JoinSummary>>,
    cleanups: Vec<JoinHandle<Result<(), String>>>,
    pub plan: Option<JoinPlan>,
    pub result: Option<JoinSummary>,
    pub destination: Option<PathBuf>,
    pub output_delimiter: u8,
    pub message: Option<String>,
    pub cancel_requested: bool,
}

impl JoinWorkflow {
    pub fn new(temporary: PathBuf) -> Self {
        Self {
            sides: [None, None],
            captured: [None, None],
            probes: [None, None],
            headers: [None, None],
            formats: [JoinFormat::default(); 2],
            mode: JoinMode::Left,
            keys: vec![[0, 0]],
            ignore_ascii_case: false,
            output_columns: Vec::new(),
            output_has_header: true,
            require_unique: [false, false],
            acknowledge_expansion: false,
            acknowledge_layout: false,
            temporary,
            header_jobs: [None, None],
            checking: None,
            writing: None,
            cleanups: Vec::new(),
            plan: None,
            result: None,
            destination: None,
            output_delimiter: b',',
            message: None,
            cancel_requested: false,
        }
    }

    pub fn running_operation(&self) -> bool {
        self.header_jobs.iter().any(Option::is_some)
            || self.checking.is_some()
            || self.writing.is_some()
    }

    pub fn busy(&self) -> bool {
        self.running_operation()
    }

    pub fn reserved_output(&self) -> Option<&Path> {
        self.destination.as_deref()
    }

    pub fn analysis_progress(&self) -> Option<JoinProgress> {
        self.checking.as_ref().map(JoinJob::progress)
    }

    pub fn write_progress(&self) -> Option<JoinProgress> {
        self.writing.as_ref().map(JoinJob::progress)
    }

    pub fn needs_expansion_confirmation(&self) -> bool {
        self.plan
            .as_ref()
            .is_some_and(|plan| plan.analysis().many_to_many_groups > 0)
    }

    pub fn needs_layout_confirmation(&self) -> bool {
        self.plan
            .as_ref()
            .is_some_and(|plan| plan.analysis().new_columns != [0, 0])
    }

    pub fn confirmations_complete(&self) -> bool {
        (!self.needs_expansion_confirmation() || self.acknowledge_expansion)
            && (!self.needs_layout_confirmation() || self.acknowledge_layout)
    }

    pub fn invalidate_plan(&mut self) {
        if let Some(plan) = self.plan.take() {
            self.cleanups.push(thread::spawn(move || {
                plan.release().map_err(|error| error.to_string())
            }));
        }
        self.result = None;
        self.destination = None;
        self.acknowledge_expansion = false;
        self.acknowledge_layout = false;
        self.message = None;
    }

    fn side_busy(&self, side: usize) -> bool {
        self.header_jobs[side].is_some() || self.checking.is_some() || self.writing.is_some()
    }

    pub fn clear_side(&mut self, side: usize) {
        if side >= 2 || self.side_busy(side) {
            return;
        }
        self.invalidate_plan();
        self.sides[side] = None;
        self.captured[side] = None;
        self.probes[side] = None;
        self.headers[side] = None;
        self.rebuild_output_columns();
    }

    pub fn set_file_side(&mut self, side: usize, path: PathBuf) {
        if side >= 2 || self.side_busy(side) {
            return;
        }
        self.invalidate_plan();
        self.captured[side] = None;
        self.probes[side] = None;
        self.headers[side] = None;
        self.sides[side] = Some(JoinSideSource::File(path.clone()));
        match read_join_header_with_format(path, self.formats[side]) {
            Ok(job) => self.header_jobs[side] = Some(job),
            Err(error) => self.message = Some(Diagnostic::from_error(&error).details()),
        }
    }

    /// Captures an open hydrated document for `side` (0 = Main, 1 = Additional), acquiring a
    /// [`crate::DocumentLease`] (`JOIN_INPUT_RESERVED`) that protects the tab until cleared/closed.
    pub fn set_open_tab_side(
        &mut self,
        side: usize,
        doc_id: DocumentId,
        label: String,
        doc: &mut Document,
    ) -> Result<(), String> {
        if side >= 2 {
            return Err("Invalid join side.".into());
        }
        if self.side_busy(side) {
            return Err("Wait for the active join step to finish.".into());
        }
        // Release an existing reservation on the same side before capturing a replacement so
        // selecting the same tab again or switching tabs never deadlocks on itself.
        self.invalidate_plan();
        self.captured[side] = None;
        self.probes[side] = None;
        self.headers[side] = None;
        let captured = doc.capture_join_document(doc_id.raw())?;
        let path = captured.path.clone();
        let probe = DocumentProbe::capture(doc_id.raw(), doc);
        let working = captured.source.clone();
        let known_cols = captured.columns;
        self.sides[side] = Some(JoinSideSource::OpenTab {
            doc_id,
            label,
            path,
        });
        self.captured[side] = Some(captured);
        self.probes[side] = Some(probe);
        match read_join_working_header(working, known_cols) {
            Ok(job) => {
                self.header_jobs[side] = Some(job);
                Ok(())
            }
            Err(error) => {
                self.captured[side] = None;
                self.probes[side] = None;
                self.sides[side] = None;
                let msg = Diagnostic::from_error(&error).details();
                self.message = Some(msg.clone());
                Err(msg)
            }
        }
    }

    pub fn set_mode(&mut self, mode: JoinMode) {
        if self.busy() || self.mode == mode {
            return;
        }
        self.mode = mode;
        self.invalidate_plan();
    }

    pub fn set_ignore_ascii_case(&mut self, ignore: bool) {
        if self.busy() || self.ignore_ascii_case == ignore {
            return;
        }
        self.ignore_ascii_case = ignore;
        self.invalidate_plan();
    }

    pub fn set_require_unique(&mut self, side: usize, required: bool) {
        if side >= 2 || self.busy() || self.require_unique[side] == required {
            return;
        }
        self.require_unique[side] = required;
        self.invalidate_plan();
    }

    pub fn add_key_pair(&mut self) {
        if self.busy() || self.keys.len() >= 16 {
            return;
        }
        let max_main = self.headers[0]
            .as_ref()
            .map_or(1, |h| h.columns.len().max(1));
        let max_add = self.headers[1]
            .as_ref()
            .map_or(1, |h| h.columns.len().max(1));
        let used_main: BTreeSet<usize> = self.keys.iter().map(|k| k[0]).collect();
        let used_add: BTreeSet<usize> = self.keys.iter().map(|k| k[1]).collect();
        let next_main = (0..max_main).find(|c| !used_main.contains(c)).unwrap_or(0);
        let next_add = (0..max_add).find(|c| !used_add.contains(c)).unwrap_or(0);
        self.keys.push([next_main, next_add]);
        self.invalidate_plan();
    }

    pub fn remove_key_pair(&mut self, index: usize) {
        if self.busy() || self.keys.len() <= 1 || index >= self.keys.len() {
            return;
        }
        self.keys.remove(index);
        self.invalidate_plan();
    }

    pub fn set_key_column(&mut self, pair_index: usize, side: usize, column: usize) {
        if self.busy() || pair_index >= self.keys.len() || side >= 2 {
            return;
        }
        if self.keys[pair_index][side] != column {
            self.keys[pair_index][side] = column;
            self.invalidate_plan();
        }
    }

    pub fn toggle_output_column(&mut self, index: usize, included: bool) {
        if self.busy() || index >= self.output_columns.len() {
            return;
        }
        if self.output_columns[index].included != included {
            self.output_columns[index].included = included;
            self.invalidate_plan();
        }
    }

    pub fn move_output_column(&mut self, from: usize, to: usize) {
        if self.busy()
            || from >= self.output_columns.len()
            || to >= self.output_columns.len()
            || from == to
        {
            return;
        }
        let col = self.output_columns.remove(from);
        self.output_columns.insert(to, col);
        self.invalidate_plan();
    }

    fn rebuild_output_columns(&mut self) {
        self.output_columns.clear();
        let mut seen_names: BTreeMap<String, usize> = BTreeMap::new();
        for (side_ix, source) in [JoinSource::Main, JoinSource::Additional]
            .into_iter()
            .enumerate()
        {
            let Some(header) = self.headers[side_ix].as_ref() else {
                continue;
            };
            for (col_ix, raw_name) in header.columns.iter().enumerate() {
                let base = if raw_name.is_empty() {
                    format!("Column {}", col_ix + 1)
                } else {
                    raw_name.clone()
                };
                let count = seen_names.entry(base.clone()).or_insert(0);
                *count += 1;
                let name = if *count > 1 && source == JoinSource::Additional {
                    format!("{base} (Additional)")
                } else {
                    base
                };
                self.output_columns.push(JoinOutputColumnDraft {
                    source,
                    column: col_ix,
                    included: true,
                    name,
                });
            }
        }
        self.output_has_header = self.headers.iter().flatten().any(|h| h.has_header);
        if let Some(main_header) = self.headers[0].as_ref() {
            self.output_delimiter = main_header.delimiter;
        }
        // Clamp key indices into valid column ranges.
        let main_len = self.headers[0]
            .as_ref()
            .map_or(1, |h| h.columns.len().max(1));
        let add_len = self.headers[1]
            .as_ref()
            .map_or(1, |h| h.columns.len().max(1));
        for pair in &mut self.keys {
            pair[0] = pair[0].min(main_len - 1);
            pair[1] = pair[1].min(add_len - 1);
        }
    }

    pub fn ready_to_analyze(&self) -> bool {
        let open_tabs_captured = (0..2).all(|side| {
            !matches!(self.sides[side], Some(JoinSideSource::OpenTab { .. }))
                || self.captured[side].is_some()
        });
        !self.busy()
            && open_tabs_captured
            && self.sides[0].is_some()
            && self.sides[1].is_some()
            && self.headers[0].is_some()
            && self.headers[1].is_some()
            && !self.keys.is_empty()
            && self.output_columns.iter().any(|c| c.included)
    }

    pub fn start_analysis(&mut self, write_slot_available: bool) {
        if self.busy() {
            return;
        }
        if !write_slot_available {
            self.message = Some("Two file operations are already running.".into());
            return;
        }
        if !self.ready_to_analyze() {
            self.message = Some(
                "Select both Main and Additional inputs, at least one key pair, and at least one output column."
                    .into(),
            );
            return;
        }
        self.invalidate_plan();
        self.cancel_requested = false;

        let path_for = |side: usize| -> PathBuf {
            match self.sides[side].as_ref().unwrap() {
                JoinSideSource::File(p) => p.clone(),
                JoinSideSource::OpenTab { path, .. } => path.clone(),
            }
        };
        let main_keys: Vec<usize> = self.keys.iter().map(|k| k[0]).collect();
        let add_keys: Vec<usize> = self.keys.iter().map(|k| k[1]).collect();
        let inputs = [
            JoinInput {
                path: path_for(0),
                key_columns: main_keys,
                delimiter: self.headers[0].as_ref().map(|h| h.delimiter),
                has_header: self.headers[0].as_ref().is_some_and(|h| h.has_header),
                encoding: self.headers[0].as_ref().map(|h| h.encoding),
                require_unique: self.require_unique[0],
                expected_version: self.headers[0].as_ref().map(|h| h.source_version.clone()),
            },
            JoinInput {
                path: path_for(1),
                key_columns: add_keys,
                delimiter: self.headers[1].as_ref().map(|h| h.delimiter),
                has_header: self.headers[1].as_ref().is_some_and(|h| h.has_header),
                encoding: self.headers[1].as_ref().map(|h| h.encoding),
                require_unique: self.require_unique[1],
                expected_version: self.headers[1].as_ref().map(|h| h.source_version.clone()),
            },
        ];
        let working = [
            self.captured[0].as_ref().map(|c| c.source.clone()),
            self.captured[1].as_ref().map(|c| c.source.clone()),
        ];
        let config = JoinConfig {
            mode: self.mode,
            case: if self.ignore_ascii_case {
                JoinCase::AsciiInsensitive
            } else {
                JoinCase::Sensitive
            },
            ..JoinConfig::default()
        };
        let selected_cols: Vec<JoinOutputColumn> = self
            .output_columns
            .iter()
            .filter(|c| c.included)
            .map(|c| JoinOutputColumn {
                source: c.source,
                column: c.column,
                name: (!c.name.trim().is_empty()).then(|| c.name.clone()),
            })
            .collect();
        let known_widths = [
            self.headers[0].as_ref().map_or(0, |h| h.columns.len()),
            self.headers[1].as_ref().map_or(0, |h| h.columns.len()),
        ];
        let output_options = JoinOutputOptions {
            known_widths: Some(known_widths),
            columns: Some(selected_cols),
            include_header: Some(self.output_has_header),
        };
        match analyze_join_with_working(
            inputs,
            working,
            self.temporary.clone(),
            config,
            output_options,
        ) {
            Ok(job) => self.checking = Some(job),
            Err(error) => self.message = Some(Diagnostic::from_error(&error).details()),
        }
    }

    pub fn prepare_destination(
        &mut self,
        destination: PathBuf,
        is_protected: impl Fn(&Path) -> bool,
    ) -> Result<(), String> {
        if quarry_core::is_compressed_destination(&destination) {
            self.destination = None;
            let msg = COMPRESSED_DESTINATION.to_owned();
            self.message = Some(msg.clone());
            return Err(msg);
        }
        if is_protected(&destination) {
            self.destination = None;
            let msg = "This file is open or being written in another tab. Choose another output."
                .to_owned();
            self.message = Some(msg.clone());
            return Err(msg);
        }
        if destination.try_exists().unwrap_or(false) {
            self.destination = None;
            let msg = "The output file already exists. Choose a new file name; existing files are never overwritten.".to_owned();
            self.message = Some(msg.clone());
            return Err(msg);
        }
        self.message = None;
        self.destination = Some(destination);
        Ok(())
    }

    pub fn start_write(
        &mut self,
        write_slot_available: bool,
        is_protected: impl Fn(&Path) -> bool,
    ) -> Result<(), String> {
        if self.busy() {
            return Err("Wait for the active join step to finish.".into());
        }
        if !write_slot_available {
            let msg = "Two file operations are already running.".to_owned();
            self.message = Some(msg.clone());
            return Err(msg);
        }
        if !self.confirmations_complete() {
            let msg =
                "Review and confirm the join safety checks before creating the output.".to_owned();
            self.message = Some(msg.clone());
            return Err(msg);
        }
        let Some(destination) = self.destination.clone() else {
            return Err("Choose an output destination first.".into());
        };
        if is_protected(&destination) {
            let msg = "The output is now reserved by another operation. Choose a different path."
                .to_owned();
            self.message = Some(msg.clone());
            return Err(msg);
        }
        let Some(plan) = self.plan.take() else {
            return Err("Run Preview before creating the joined file.".into());
        };
        self.message = None;
        self.cancel_requested = false;
        match plan.start_write_reviewed(
            destination,
            self.acknowledge_expansion,
            self.acknowledge_layout,
        ) {
            Ok(job) => {
                self.writing = Some(job);
                Ok(())
            }
            Err(error) => {
                let msg = Diagnostic::from_error(&error).details();
                self.message = Some(msg.clone());
                Err(msg)
            }
        }
    }

    pub fn cancel(&mut self) {
        for job in self.header_jobs.iter().flatten() {
            job.cancel();
        }
        if let Some(job) = &self.checking {
            job.cancel();
        }
        if let Some(job) = &self.writing {
            job.cancel();
        }
        self.cancel_requested = true;
    }

    /// Clears any open-tab side selections and headers whose captures are being released so
    /// subsequent analyses cannot fall back to unedited disk files.
    fn clear_open_tab_sides(&mut self) {
        let mut cleared_any = false;
        for side in 0..2 {
            if matches!(self.sides[side], Some(JoinSideSource::OpenTab { .. })) {
                self.sides[side] = None;
                self.headers[side] = None;
                cleared_any = true;
            }
            self.captured[side] = None;
            self.probes[side] = None;
        }
        if cleared_any {
            self.rebuild_output_columns();
        }
    }

    /// Releases all open-tab `DocumentLease`s and any analyzed join workspace.
    pub fn release_all(&mut self) {
        self.cancel();
        for slot in &mut self.header_jobs {
            if let Some(job) = slot.take() {
                let _ = job.wait();
            }
        }
        if let Some(job) = self.checking.take()
            && let Ok(Some(plan)) = job.wait()
        {
            let _ = plan.release();
        }
        if let Some(job) = self.writing.take() {
            let _ = job.wait();
        }
        if let Some(plan) = self.plan.take() {
            let _ = plan.release();
        }
        for cleanup in self.cleanups.drain(..) {
            let _ = cleanup.join();
        }
        self.clear_open_tab_sides();
        self.destination = None;
    }

    /// Returns `true` when any worker transitioned state.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        for side in 0..2 {
            if self.header_jobs[side]
                .as_ref()
                .is_some_and(|job| job.progress().done)
            {
                changed = true;
                match self.header_jobs[side].take().unwrap().wait() {
                    Ok(Some(header)) => {
                        self.headers[side] = Some(header);
                        self.rebuild_output_columns();
                    }
                    Ok(None) => {
                        self.message = Some("Column check cancelled.".into());
                    }
                    Err(error) => {
                        self.captured[side] = None;
                        self.probes[side] = None;
                        self.sides[side] = None;
                        self.message = Some(Diagnostic::from_error(&error).details());
                    }
                }
            }
        }
        if self
            .checking
            .as_ref()
            .is_some_and(|job| job.progress().done)
        {
            changed = true;
            match self.checking.take().unwrap().wait() {
                Ok(Some(plan)) => self.plan = Some(plan),
                Ok(None) => {
                    self.message = Some("Join preview cancelled. No output was written.".into());
                }
                Err(error) => {
                    self.message = Some(Diagnostic::from_error(&error).details());
                }
            }
            self.cancel_requested = false;
        }
        if self.writing.as_ref().is_some_and(|job| job.progress().done) {
            changed = true;
            match self.writing.take().unwrap().wait() {
                Ok(Some(summary)) => {
                    self.result = Some(summary);
                    // Release open-tab leases and clear open-tab selections so a later Preview
                    // cannot silently read unedited disk bytes without re-capturing.
                    self.clear_open_tab_sides();
                }
                Ok(None) => {
                    self.message = Some("Join cancelled. No joined file was published.".into());
                }
                Err(error) => {
                    self.message = Some(Diagnostic::from_error(&error).details());
                }
            }
            self.cancel_requested = false;
        }
        let mut remaining = Vec::with_capacity(self.cleanups.len());
        for handle in self.cleanups.drain(..) {
            if handle.is_finished() {
                changed = true;
                if let Ok(Err(err)) = handle.join() {
                    self.message = Some(err);
                }
            } else {
                remaining.push(handle);
            }
        }
        self.cleanups = remaining;
        changed
    }
}

impl Drop for JoinWorkflow {
    fn drop(&mut self) {
        self.release_all();
    }
}

// =============================================================================
// Inspect File & Recovery workflow
// =============================================================================

struct RawGzipDecompression {
    job: CompressedRawDecompressJob,
    cancelled: bool,
    _slot: DecompressionSlot,
}

/// UI-neutral controller for file inspection, source previews, location jumps, and recovery copies.
pub struct InspectWorkflow {
    pub path: Option<PathBuf>,
    pub display_path: Option<PathBuf>,
    pub target_doc_id: Option<DocumentId>,
    pub delimiter: DelimiterMode,
    pub header_mode: HeaderMode,
    pub encoding: Option<Encoding>,
    pub options: InspectionOptions,
    edits: InspectionEdits,
    session_for_compressed_guard: Option<Arc<Session>>,
    job: Option<InspectionJob>,
    recovery: Option<RecoveryJob>,
    raw_decompression: Option<RawGzipDecompression>,
    retired_jobs: Vec<JoinHandle<()>>,
    pub report: Option<InspectionReport>,
    pub selected: BTreeSet<usize>,
    pub focused: usize,
    pub preview: Option<SourcePreview>,
    pub recovery_result: Option<RecoverySummary>,
    pub recovery_destination: Option<PathBuf>,
    pub message: Option<String>,
    pub operation_diagnostic: Option<Diagnostic>,
    working_directory: PathBuf,
    custom_working_directory: bool,
    working_cleanup: WorkingCopyCleanup,
    decompression_slots: DecompressionSlots,
    // Declared last so private copy outlives any worker reading it.
    private_copy: Option<DecompressedCopy>,
}

impl InspectWorkflow {
    pub fn new(
        working_directory: PathBuf,
        custom_working_directory: bool,
        working_cleanup: WorkingCopyCleanup,
        decompression_slots: DecompressionSlots,
    ) -> Self {
        Self {
            path: None,
            display_path: None,
            target_doc_id: None,
            delimiter: DelimiterMode::Auto,
            header_mode: HeaderMode::Auto,
            encoding: None,
            options: InspectionOptions::default(),
            edits: InspectionEdits::default(),
            session_for_compressed_guard: None,
            job: None,
            recovery: None,
            raw_decompression: None,
            retired_jobs: Vec::new(),
            report: None,
            selected: BTreeSet::new(),
            focused: 0,
            preview: None,
            recovery_result: None,
            recovery_destination: None,
            message: None,
            operation_diagnostic: None,
            working_directory,
            custom_working_directory,
            working_cleanup,
            decompression_slots,
            private_copy: None,
        }
    }

    pub fn set_working_directory(&mut self, directory: PathBuf, custom: bool) {
        self.working_directory = directory;
        self.custom_working_directory = custom;
    }

    pub fn busy(&self) -> bool {
        self.job.is_some() || self.recovery.is_some() || self.raw_decompression.is_some()
    }

    pub fn recovering(&self) -> bool {
        self.recovery.is_some()
    }

    pub fn reserved_output(&self) -> Option<&Path> {
        self.recovery_destination.as_deref()
    }

    pub fn progress(&self) -> Option<InspectionProgress> {
        self.job
            .as_ref()
            .map(InspectionJob::progress)
            .or_else(|| self.recovery.as_ref().map(RecoveryJob::progress))
    }

    pub fn clear_report(&mut self) {
        self.report = None;
        self.selected.clear();
        self.focused = 0;
        self.preview = None;
        self.recovery_result = None;
        self.recovery_destination = None;
        self.message = None;
    }

    /// Cancels and detaches any in-flight inspection, recovery, or raw decompression job so
    /// stale completions can never populate a newly configured target while keeping private
    /// copies and decompression slots alive until the cancelled worker exits.
    pub fn discard_in_flight(&mut self) {
        self.cancel();
        let job = self.job.take();
        let recovery = self.recovery.take();
        let raw = self.raw_decompression.take();
        let private_copy = self.private_copy.take();
        self.recovery_destination = None;
        if job.is_some() || recovery.is_some() || raw.is_some() {
            self.retired_jobs.push(thread::spawn(move || {
                if let Some(j) = job {
                    let _ = j.wait();
                }
                if let Some(r) = recovery {
                    let _ = r.wait();
                }
                if let Some(raw_decomp) = raw {
                    let _ = raw_decomp.job.wait();
                    drop(raw_decomp._slot);
                }
                drop(private_copy);
            }));
        }
    }

    pub fn configure_for_document(&mut self, doc_id: DocumentId, doc: &Document) {
        self.discard_in_flight();
        self.clear_report();
        self.target_doc_id = Some(doc_id);
        self.operation_diagnostic = doc.pending_diagnostic.as_deref().cloned();
        self.delimiter = match doc.session.dialect.delimiter {
            b',' => DelimiterMode::Comma,
            b'\t' => DelimiterMode::Tab,
            b'|' => DelimiterMode::Pipe,
            b';' => DelimiterMode::Semicolon,
            _ => DelimiterMode::Auto,
        };
        self.header_mode = if doc.session.dialect.has_header {
            HeaderMode::FirstRow
        } else {
            HeaderMode::NoHeader
        };
        self.encoding = Some(doc.session.encoding());
        self.edits = InspectionEdits {
            headers: doc
                .header_renames
                .iter()
                .map(|(k, v)| (*k, v.as_bytes().to_vec()))
                .collect(),
            cells: doc
                .cell_edits
                .iter()
                .map(|(k, v)| (*k, v.as_ref().clone()))
                .collect(),
        };
        if let Some(raw_gz) = doc.raw_inspection_target() {
            self.path = Some(raw_gz.to_path_buf());
            self.display_path = Some(raw_gz.to_path_buf());
            self.session_for_compressed_guard = None;
        } else {
            self.path = Some(doc.session.path().to_path_buf());
            self.display_path = Some(doc.logical_path.clone());
            self.session_for_compressed_guard =
                doc.is_compressed().then(|| Arc::clone(&doc.session));
        }
    }

    pub fn configure_for_failed_open(&mut self, failed: &FailedOpen) {
        self.discard_in_flight();
        self.clear_report();
        self.target_doc_id = None;
        self.session_for_compressed_guard = None;
        self.edits = InspectionEdits::default();
        self.path = Some(failed.path.clone());
        self.display_path = Some(failed.path.clone());
        self.delimiter = match failed.options.delimiter {
            Some(b',') => DelimiterMode::Comma,
            Some(b'\t') => DelimiterMode::Tab,
            Some(b'|') => DelimiterMode::Pipe,
            Some(b';') => DelimiterMode::Semicolon,
            _ => DelimiterMode::Auto,
        };
        self.header_mode = failed.options.header_mode;
        self.encoding = failed.encoding;
        self.message = Some(failed.message.clone());
        if !crate::compressed::compressed_source(&failed.path) {
            self.preview = SourcePreview::prefix(&failed.path, failed.encoding).ok();
        }
    }

    pub fn configure_for_path(&mut self, path: PathBuf) {
        self.discard_in_flight();
        self.clear_report();
        self.target_doc_id = None;
        self.session_for_compressed_guard = None;
        self.edits = InspectionEdits::default();
        self.delimiter = DelimiterMode::Auto;
        self.header_mode = HeaderMode::Auto;
        self.encoding = None;
        self.path = Some(path.clone());
        self.display_path = Some(path);
    }

    pub fn start_inspect(&mut self, write_slot_available: bool) {
        if self.busy() {
            return;
        }
        let Some(path) = self.path.clone() else {
            self.message = Some("Choose a file to inspect first.".into());
            return;
        };
        self.clear_report();
        self.options = InspectionOptions {
            delimiter: self.delimiter.delimiter(),
            header_mode: self.header_mode,
            encoding: self.encoding,
            ..InspectionOptions::default()
        };
        if crate::compressed::is_gzip(&path) && self.session_for_compressed_guard.is_none() {
            if !write_slot_available {
                self.message = Some("Wait for an active file operation to finish.".into());
                return;
            }
            if self.custom_working_directory
                && let Err(error) = quarry_core::validate_scratch_directory(&self.working_directory)
            {
                self.message = Some(crate::format_unavailable_scratch_notice(
                    &self.working_directory,
                    &error.to_string(),
                ));
                return;
            }
            let config = CompressedOpenConfig::new(self.working_directory.clone());
            let slot = self.decompression_slots.claim();
            match CompressedRawDecompressJob::start(&path, config, self.working_cleanup.clone()) {
                Ok(job) => {
                    self.raw_decompression = Some(RawGzipDecompression {
                        job,
                        cancelled: false,
                        _slot: slot,
                    });
                }
                Err(error) => self.message = Some(Diagnostic::from_error(&error).details()),
            }
            return;
        }
        let inspect_path = self
            .private_copy
            .as_ref()
            .map(|copy| copy.path().to_path_buf())
            .unwrap_or(path);
        match InspectionJob::start(&inspect_path, self.options, self.edits.clone()) {
            Ok(job) => self.job = Some(job),
            Err(error) => self.message = Some(Diagnostic::from_error(&error).details()),
        }
    }

    pub fn focus_finding(&mut self, index: usize) {
        let Some(report) = self.report.as_ref() else {
            return;
        };
        if index >= report.findings.len() {
            return;
        }
        self.focused = index;
        match report.preview(index) {
            Ok(preview) => self.preview = Some(preview),
            Err(error) => self.message = Some(error.to_string()),
        }
    }

    pub fn toggle_finding(&mut self, index: usize) {
        let Some(report) = self.report.as_ref() else {
            return;
        };
        if !report.can_omit(index) {
            return;
        }
        if !self.selected.remove(&index) {
            self.selected.insert(index);
        }
    }

    pub fn select_all_eligible(&mut self) {
        let Some(report) = self.report.as_ref() else {
            return;
        };
        self.selected = (0..report.findings.len())
            .filter(|&i| report.can_omit(i))
            .collect();
    }

    /// Returns a 0-based `(source_row, optional_column)` target for navigating an open tab
    /// to the focused or specified finding.
    pub fn finding_jump_target(&self, index: usize) -> Option<(u64, Option<usize>)> {
        let report = self.report.as_ref()?;
        let finding = report.findings.get(index)?;
        let source_row = if let Some(record) = finding.record {
            record.saturating_sub(1)
        } else {
            let data_row = finding.data_row?;
            data_row
                .saturating_sub(1)
                .saturating_add(u64::from(report.has_header))
        };
        let col = finding.column.map(|c| c.saturating_sub(1));
        Some((source_row, col))
    }

    fn compressed_source_guard(&self) -> Option<CompressedSourceGuard> {
        self.private_copy
            .as_ref()
            .map(DecompressedCopy::source_guard)
            .or_else(|| {
                self.session_for_compressed_guard
                    .as_ref()
                    .and_then(|s| s.compressed_source_guard())
            })
    }

    pub fn start_recovery(
        &mut self,
        destination: PathBuf,
        write_slot_available: bool,
        is_protected: impl Fn(&Path) -> bool,
    ) -> Result<(), String> {
        if self.busy() {
            return Err("Wait for the active inspection or recovery job to finish.".into());
        }
        if !write_slot_available {
            let msg = "Two file operations are already running.".to_owned();
            self.message = Some(msg.clone());
            return Err(msg);
        }
        if quarry_core::is_compressed_destination(&destination) {
            let msg = COMPRESSED_DESTINATION.to_owned();
            self.message = Some(msg.clone());
            return Err(msg);
        }
        if is_protected(&destination) {
            let msg = "This file is open or being written in another tab. Choose another output."
                .to_owned();
            self.message = Some(msg.clone());
            return Err(msg);
        }
        if destination.try_exists().unwrap_or(false) {
            let msg = "The output file already exists. Choose a new file name; existing files are never overwritten.".to_owned();
            self.message = Some(msg.clone());
            return Err(msg);
        }
        let Some(report) = self.report.as_ref() else {
            return Err("Run Inspect before creating a recovery copy.".into());
        };
        let started = if let Some(guard) = self.compressed_source_guard() {
            RecoveryJob::start_with_source_guard(report, &self.selected, &destination, guard)
        } else {
            RecoveryJob::start(report, &self.selected, &destination)
        };
        match started {
            Ok(job) => {
                self.recovery = Some(job);
                self.recovery_destination = Some(destination);
                self.message = None;
                Ok(())
            }
            Err(error) => {
                let msg = Diagnostic::from_error(&error).details();
                self.message = Some(msg.clone());
                Err(msg)
            }
        }
    }

    pub fn cancel(&mut self) {
        if let Some(job) = &self.job {
            job.cancel();
        }
        if let Some(job) = &self.recovery {
            job.cancel();
        }
        if let Some(raw) = &mut self.raw_decompression {
            raw.cancelled = true;
            raw.job.cancel();
        }
    }

    /// Returns `true` when any background job finished or changed state.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if self
            .raw_decompression
            .as_ref()
            .is_some_and(|raw| raw.job.progress().done)
        {
            changed = true;
            let raw = self.raw_decompression.take().unwrap();
            match raw.job.wait() {
                Ok(Some(copy)) if !raw.cancelled => {
                    let copy_path = copy.path().to_path_buf();
                    self.private_copy = Some(copy);
                    match InspectionJob::start(&copy_path, self.options, self.edits.clone()) {
                        Ok(job) => self.job = Some(job),
                        Err(error) => self.message = Some(Diagnostic::from_error(&error).details()),
                    }
                }
                Ok(_) => {
                    self.message = Some(
                        "Decompression cancelled; temporary files were removed. Choose Inspect again to retry."
                            .into(),
                    );
                }
                Err(error) => self.message = Some(Diagnostic::from_error(&error).details()),
            }
        }
        if self.job.as_ref().is_some_and(|job| job.progress().done) {
            changed = true;
            match self.job.take().unwrap().wait() {
                Ok(report) => {
                    let preview = if !report.findings.is_empty() {
                        report.preview(0).ok()
                    } else {
                        None
                    };
                    self.preview = preview;
                    self.focused = 0;
                    self.selected.clear();
                    if report.status == InspectionStatus::Cancelled {
                        self.message = Some("Inspection cancelled.".into());
                    } else if report.findings.is_empty() {
                        self.message =
                            Some("No structural or encoding problems were found.".into());
                    }
                    self.report = Some(report);
                }
                Err(error) => self.message = Some(Diagnostic::from_error(&error).details()),
            }
        }
        if self
            .recovery
            .as_ref()
            .is_some_and(|job| job.progress().done)
        {
            changed = true;
            self.recovery_destination = None;
            match self.recovery.take().unwrap().wait() {
                Ok(Some(summary)) => {
                    self.recovery_result = Some(summary);
                }
                Ok(None) => {
                    self.message =
                        Some("Recovery cancelled. No recovery copy was published.".into());
                }
                Err(error) => self.message = Some(Diagnostic::from_error(&error).details()),
            }
        }
        let mut remaining = Vec::with_capacity(self.retired_jobs.len());
        for handle in self.retired_jobs.drain(..) {
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                remaining.push(handle);
            }
        }
        self.retired_jobs = remaining;
        changed
    }
}

impl Drop for InspectWorkflow {
    fn drop(&mut self) {
        self.discard_in_flight();
        for handle in self.retired_jobs.drain(..) {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OpenOptions, WindowId, WorkbenchScheduler, WorkbenchSchedulerConfig};
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    use std::time::Duration;

    fn wait_combine(wf: &mut CombineWorkflow) {
        for _ in 0..400 {
            wf.poll();
            if !wf.busy() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("CombineWorkflow timed out");
    }

    fn wait_join(wf: &mut JoinWorkflow) {
        for _ in 0..400 {
            wf.poll();
            if !wf.busy() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("JoinWorkflow timed out");
    }

    fn wait_inspect(wf: &mut InspectWorkflow) {
        for _ in 0..400 {
            wf.poll();
            if !wf.busy() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("InspectWorkflow timed out");
    }

    #[test]
    fn combine_workflow_validates_schema_rejects_gzip_and_writes_output() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.csv");
        let b = dir.path().join("b.csv");
        std::fs::write(&a, "id,name\n1,Alice\n").unwrap();
        std::fs::write(&b, "id,name\n2,Bob\n").unwrap();

        let mut wf = CombineWorkflow::default();
        wf.add_paths([a, b]);
        wf.start_check(true);
        wait_combine(&mut wf);
        assert!(wf.plan.is_some());

        // Reject compressed destination.
        wf.prepare_destination(dir.path().join("out.csv.gz"), |_| false);
        assert!(wf.destination.is_none());
        assert!(wf.message.as_deref().unwrap_or("").contains(".gz"));

        let out = dir.path().join("combined.csv");
        wf.prepare_destination(out.clone(), |_| false);
        wait_combine(&mut wf);
        assert!(wf.space.is_some());

        wf.start_write(true, |_| false);
        wait_combine(&mut wf);
        let summary = wf.result.as_ref().expect("combine summary");
        assert_eq!(summary.destination, out);
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "id,name\n1,Alice\n2,Bob\n"
        );
    }

    #[test]
    fn join_workflow_supports_all_four_modes_composite_keys_and_document_leases() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let main_path = dir.path().join("main.csv");
        let add_path = dir.path().join("add.csv");
        std::fs::write(&main_path, "k1,k2,val\nA,1,m1\nB,2,m2\n").unwrap();
        std::fs::write(&add_path, "k1,k2,extra\nA,1,e1\nC,3,e3\n").unwrap();

        let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
            working_directory: scratch.path().to_path_buf(),
            index_cache_enabled: false,
            ..Default::default()
        });
        let ids = scheduler
            .open_many_lazy(
                WindowId::PRIMARY,
                [main_path.clone()],
                OpenOptions {
                    header_mode: HeaderMode::FirstRow,
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        for _ in 0..400 {
            let _ = scheduler.poll_all();
            if scheduler
                .tab(ids[0])
                .unwrap()
                .document
                .as_ref()
                .unwrap()
                .progress
                .done
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        let mut wf = JoinWorkflow::new(scratch.path().to_path_buf());
        {
            let doc = scheduler
                .tab_mut(ids[0])
                .unwrap()
                .document
                .as_mut()
                .unwrap();
            wf.set_open_tab_side(0, ids[0], "main.csv".into(), doc)
                .unwrap();
            assert!(
                doc.join_reserved(),
                "open tab must hold DocumentLease while in join"
            );
        }
        wf.set_file_side(1, add_path);
        wait_join(&mut wf);
        assert!(wf.headers[0].is_some() && wf.headers[1].is_some());

        // Composite key on (k1, k2).
        wf.add_key_pair();
        wf.set_key_column(0, 0, 0);
        wf.set_key_column(0, 1, 0);
        wf.set_key_column(1, 0, 1);
        wf.set_key_column(1, 1, 1);

        for (mode, expected_rows) in [
            (JoinMode::Inner, 1u64),
            (JoinMode::Left, 2),
            (JoinMode::Right, 2),
            (JoinMode::Full, 3),
        ] {
            wf.set_mode(mode);
            wf.start_analysis(true);
            wait_join(&mut wf);
            let analysis = wf.plan.as_ref().unwrap().analysis();
            assert_eq!(analysis.output_rows, expected_rows, "mode {mode:?}");
        }

        let out = dir.path().join("joined-full.csv");
        wf.prepare_destination(out.clone(), |_| false).unwrap();
        wf.start_write(true, |_| false).unwrap();
        wait_join(&mut wf);
        assert!(wf.result.is_some());
        assert!(out.exists());

        // Lease AND open-tab selection must be cleared automatically after write completion so
        // reopening Join cannot silently run against unedited disk bytes.
        let doc = scheduler.tab(ids[0]).unwrap().document.as_ref().unwrap();
        assert!(!doc.join_reserved());
        assert!(wf.sides[0].is_none());
        assert!(wf.headers[0].is_none());
        assert!(!wf.ready_to_analyze());

        // Also verify release_all() clears OpenTab side selections & headers.
        {
            let doc = scheduler
                .tab_mut(ids[0])
                .unwrap()
                .document
                .as_mut()
                .unwrap();
            wf.set_open_tab_side(0, ids[0], "main.csv".into(), doc)
                .unwrap();
        }
        wait_join(&mut wf);
        assert!(wf.ready_to_analyze());
        wf.release_all();
        assert!(wf.sides[0].is_none());
        assert!(wf.headers[0].is_none());
        assert!(!wf.ready_to_analyze());
    }

    #[test]
    fn inspect_workflow_finds_bad_rows_recovers_copy_and_inspects_raw_gzip() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let bad = dir.path().join("uneven.csv");
        std::fs::write(&bad, "a,b\n1,2\n3,4,5\n6,7\n").unwrap();

        let mut wf = InspectWorkflow::new(
            scratch.path().to_path_buf(),
            true,
            WorkingCopyCleanup::default(),
            DecompressionSlots::default(),
        );
        wf.configure_for_path(bad);
        wf.start_inspect(true);
        wait_inspect(&mut wf);
        let report = wf.report.as_ref().expect("inspection report");
        assert!(!report.findings.is_empty());
        wf.select_all_eligible();
        assert!(!wf.selected.is_empty());
        assert!(wf.finding_jump_target(0).is_some());

        let recovered = dir.path().join("recovered.csv");
        wf.start_recovery(recovered.clone(), true, |_| false)
            .unwrap();
        wait_inspect(&mut wf);
        let summary = wf.recovery_result.as_ref().expect("recovery summary");
        assert_eq!(summary.removed_records, 1);
        assert_eq!(
            std::fs::read_to_string(&recovered).unwrap(),
            "a,b\n1,2\n6,7\n"
        );

        // Also test raw gzip inspection end to end.
        let gz_path = dir.path().join("uneven.csv.gz");
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(b"a,b\n1,2\n3,4,5\n").unwrap();
        std::fs::write(&gz_path, enc.finish().unwrap()).unwrap();
        wf.configure_for_path(gz_path);
        wf.start_inspect(true);
        wait_inspect(&mut wf);
        assert!(!wf.report.as_ref().unwrap().findings.is_empty());

        // Reconfiguring target while an inspection job is in flight must discard the old worker
        // so poll() never installs the old file's report under the new target.
        let clean = dir.path().join("clean.csv");
        std::fs::write(&clean, "x,y\n10,20\n").unwrap();
        let big_bad = dir.path().join("big_bad.csv");
        let mut buf = String::from("a,b\n");
        for _ in 0..10_000 {
            buf.push_str("1,2,3\n");
        }
        std::fs::write(&big_bad, buf).unwrap();
        wf.configure_for_path(big_bad);
        wf.start_inspect(true);
        wf.configure_for_path(clean.clone());
        assert!(!wf.busy());
        for _ in 0..40 {
            wf.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            wf.report.is_none(),
            "stale inspection from prior target must never populate the newly configured target"
        );
        assert_eq!(wf.display_path.as_deref(), Some(clean.as_path()));
    }
}
