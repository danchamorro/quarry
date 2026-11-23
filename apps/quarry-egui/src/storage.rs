use super::*;
use quarry_core::{
    StorageSpace, estimate_duplicate_target_temporary_bytes, inspect_storage,
    validate_scratch_directory,
};
use std::io::{Read, Write};
use std::thread::{self, JoinHandle};

const MAX_STORAGE_PREFERENCES_BYTES: u64 = 8 * 1024;

/// Ignore unusable environment roots instead of writing preferences beneath the working directory.
fn unix_storage_preferences_path(
    config_root: Option<PathBuf>,
    user_root: Option<PathBuf>,
) -> Option<PathBuf> {
    config_root
        .filter(|root| root.is_absolute())
        .or_else(|| {
            user_root
                .filter(|root| root.is_absolute())
                .map(|root| root.join(".config"))
        })
        .map(|root| root.join("quarry/storage-preferences"))
}

/// Persisted user-selected temporary scratch folder across restarts (ADR 0005 amendment).
/// Only stores an absolute custom folder path; `None` means the OS default temporary folder.
#[derive(Default)]
pub(super) struct StoragePreferences {
    pub scratch_directory: Option<PathBuf>,
    pub load_error: Option<String>,
    path: Option<PathBuf>,
    persist_enabled: bool,
}

impl StoragePreferences {
    #[cfg(test)]
    pub(super) fn load(path: Option<PathBuf>) -> Self {
        Self::load_with_persistence(path, false)
    }

    fn load_with_persistence(path: Option<PathBuf>, persist_enabled: bool) -> Self {
        let mut scratch_directory = None;
        let mut load_error = None;
        if let Some(pref_path) = path.as_ref() {
            match std::fs::File::open(pref_path) {
                Ok(file) => {
                    let mut raw = Vec::new();
                    match file
                        .take(MAX_STORAGE_PREFERENCES_BYTES + 1)
                        .read_to_end(&mut raw)
                    {
                        Ok(_) => {
                            if raw.len() as u64 > MAX_STORAGE_PREFERENCES_BYTES {
                                load_error = Some(format!(
                                    "Saved temporary folder preference ({}) exceeds the maximum supported size. Open File > Temporary storage… to choose your temporary folder again.",
                                    pref_path.display()
                                ));
                            } else {
                                match String::from_utf8(raw) {
                                    Ok(content) => {
                                        if !content.trim().is_empty() {
                                            match parse_scratch_directory_pref(&content) {
                                                Some(dir) => scratch_directory = Some(dir),
                                                None => {
                                                    load_error = Some(format!(
                                                        "Saved temporary folder preference ({}) could not be parsed. Open File > Temporary storage… to choose your temporary folder again.",
                                                        pref_path.display()
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                    Err(_) => {
                                        load_error = Some(format!(
                                            "Saved temporary folder preference ({}) could not be parsed. Open File > Temporary storage… to choose your temporary folder again.",
                                            pref_path.display()
                                        ));
                                    }
                                }
                            }
                        }
                        Err(error) => {
                            load_error = Some(format!(
                                "Could not read saved temporary folder preference ({}): {error}. Open File > Temporary storage… to choose your temporary folder again.",
                                pref_path.display()
                            ));
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    load_error = Some(format!(
                        "Could not read saved temporary folder preference ({}): {error}. Open File > Temporary storage… to choose your temporary folder again.",
                        pref_path.display()
                    ));
                }
            }
        }
        Self {
            scratch_directory,
            load_error,
            path,
            persist_enabled,
        }
    }

    pub(super) fn application() -> Self {
        let path = if cfg!(target_os = "macos") {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|root| root.is_absolute())
                .map(|root| root.join("Library/Application Support/Quarry/storage-preferences"))
        } else if cfg!(target_os = "windows") {
            std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .filter(|root| root.is_absolute())
                .map(|root| root.join("Quarry/storage-preferences"))
        } else {
            unix_storage_preferences_path(
                std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
                std::env::var_os("HOME").map(PathBuf::from),
            )
        };
        Self::load_with_persistence(path, true)
    }

    pub(super) fn set_scratch_directory(
        &mut self,
        directory: Option<&Path>,
    ) -> std::io::Result<()> {
        let encoded = match directory {
            Some(dir) => {
                if !dir.is_absolute() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "Temporary folder must be an absolute path.",
                    ));
                }
                let encoded = encode_scratch_directory_pref(dir)?;
                self.scratch_directory = Some(dir.to_path_buf());
                encoded
            }
            None => {
                self.scratch_directory = None;
                String::new()
            }
        };
        self.load_error = None;
        let Some(path) = &self.path else {
            if self.persist_enabled {
                return Err(std::io::Error::other(
                    "no writable configuration directory is available for saving preferences across restarts",
                ));
            }
            return Ok(());
        };
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("Invalid storage preference location"))?;
        std::fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        quarry_core::restrict_private_file(file.path(), file.as_file())?;
        file.write_all(encoded.as_bytes())?;
        file.flush()?;
        file.persist(path).map_err(|e| e.error)?;
        Ok(())
    }
}

fn encode_scratch_directory_pref(path: &Path) -> std::io::Result<String> {
    let encoded = if let Some(text) = path.to_str()
        && !text.contains('\n')
        && !text.contains('\r')
        && !text.contains('\0')
        && text.trim() == text
    {
        format!("scratch_directory={text}\n")
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let bytes = path.as_os_str().as_bytes();
            let mut hex =
                String::with_capacity("scratch_directory_hex=".len() + bytes.len() * 2 + 1);
            hex.push_str("scratch_directory_hex=");
            for byte in bytes {
                use std::fmt::Write as _;
                let _ = write!(hex, "{byte:02x}");
            }
            hex.push('\n');
            hex
        }
        #[cfg(not(unix))]
        {
            if let Some(text) = path.to_str()
                && !text.contains('\n')
                && !text.contains('\r')
                && !text.contains('\0')
            {
                format!("scratch_directory={text}\n")
            } else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Temporary folder path contains unsupported characters.",
                ));
            }
        }
    };
    if encoded.len() as u64 > MAX_STORAGE_PREFERENCES_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Temporary folder path is too long to save across restarts.",
        ));
    }
    Ok(encoded)
}

fn parse_scratch_directory_pref(content: &str) -> Option<PathBuf> {
    for line in content.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix("scratch_directory=")
            && !rest.is_empty()
            && !rest.contains('\0')
        {
            let candidate = PathBuf::from(rest);
            if candidate.is_absolute() {
                return Some(candidate);
            }
        }
        #[cfg(unix)]
        if let Some(hex) = line.strip_prefix("scratch_directory_hex=") {
            use std::os::unix::ffi::OsStringExt;
            let hex = hex.trim();
            if !hex.is_empty() && hex.len().is_multiple_of(2) {
                let mut bytes = Vec::with_capacity(hex.len() / 2);
                let mut valid = true;
                for pair in hex.as_bytes().as_chunks::<2>().0 {
                    let Ok(s) = std::str::from_utf8(pair) else {
                        valid = false;
                        break;
                    };
                    let Ok(byte) = u8::from_str_radix(s, 16) else {
                        valid = false;
                        break;
                    };
                    bytes.push(byte);
                }
                if valid && !bytes.contains(&0) {
                    let candidate = PathBuf::from(std::ffi::OsString::from_vec(bytes));
                    if candidate.is_absolute() {
                        return Some(candidate);
                    }
                }
            }
        }
    }
    None
}

pub(super) use quarry_workbench::{STORAGE_REVIEW_BYTES, format_unavailable_scratch_notice};

#[derive(Clone)]
pub(super) enum PendingStorageOperation {
    Structural(StructuralDialog),
    Materialize(ColumnTransformation, BTreeSet<usize>, u64),
    DeleteColumns(Vec<usize>),
    DeleteRows(Vec<RangeInclusive<u64>>),
    ReplaceAll {
        query: Vec<u8>,
        replacement: Vec<u8>,
        case_sensitivity: CaseSensitivity,
    },
    Save(Option<PathBuf>),
    SaveUtf8(PathBuf),
    Export(PathBuf),
}

impl PendingStorageOperation {
    /// Describes the pending action in the temporary-storage review.
    fn action_name(&self) -> &'static str {
        match self {
            Self::Structural(dialog) => match dialog.request {
                StructuralRequest::CleanupTrim
                | StructuralRequest::CleanupCollapseSpaces
                | StructuralRequest::CleanupUppercase
                | StructuralRequest::CleanupLowercase
                | StructuralRequest::CleanupFill => "clean up columns",
                StructuralRequest::InsertRows => "insert rows",
                StructuralRequest::InsertColumns => "insert columns",
                StructuralRequest::Sort => "sort",
                StructuralRequest::Duplicates => "find duplicates",
                StructuralRequest::Split => "split columns",
                StructuralRequest::Combine => "combine columns",
                StructuralRequest::Move => "move columns",
            },
            Self::Materialize(..) => "split columns",
            Self::DeleteColumns(_) => "delete columns",
            Self::DeleteRows(_) => "delete rows",
            Self::ReplaceAll { .. } => "replace text",
            Self::Save(_) | Self::SaveUtf8(_) => "save",
            Self::Export(_) => "export",
        }
    }

    fn uses_working_directory(&self) -> bool {
        !matches!(self, Self::Save(_) | Self::SaveUtf8(_) | Self::Export(_))
    }

    /// Conservatively estimates additional disk space for the pending action.
    fn allowance(&self, document: &Document) -> u64 {
        let plain = || document.materialization_storage_estimate(None, None);
        match self {
            Self::Structural(dialog) => {
                match dialog.request {
                    StructuralRequest::CleanupTrim
                    | StructuralRequest::CleanupCollapseSpaces
                    | StructuralRequest::CleanupUppercase
                    | StructuralRequest::CleanupLowercase
                    | StructuralRequest::CleanupFill => document.materialization_storage_estimate(
                        Some(&ColumnTransformation::Cleanup(dialog.cleanup())),
                        None,
                    ),

                    StructuralRequest::InsertRows => document
                        .row_insertion_from_dialog(dialog)
                        .map_or(0, |insertion| {
                            plain().saturating_add(
                                insertion.additional_output_bytes().saturating_mul(
                                    if document.session.encoding() == Encoding::Utf8 {
                                        1
                                    } else {
                                        2
                                    },
                                ),
                            )
                        }),
                    StructuralRequest::InsertColumns => document
                        .column_insertion_from_dialog(dialog)
                        .map_or(0, |transformation| {
                            document.materialization_storage_estimate(Some(&transformation), None)
                        }),
                    // Read-only analysis determines the actual padded width before its review.
                    StructuralRequest::Split => 0,
                    StructuralRequest::Sort => document
                        .sort_plan_disk_estimate(&dialog.sort_plan(false))
                        .unwrap_or(u64::MAX),
                    StructuralRequest::Duplicates => estimate_duplicate_target_temporary_bytes(
                        document.reordered_storage_estimate(),
                        document
                            .storage_record_count()
                            .saturating_sub(document.data_start),
                        &dialog.duplicate_target(),
                    ),
                    StructuralRequest::Combine => document.materialization_storage_estimate(
                        Some(&ColumnTransformation::Join {
                            source_columns: dialog.columns.clone(),
                            separator: dialog.separator.as_bytes().to_vec(),
                            output_header: document.session.dialect.has_header.then(|| {
                                document
                                    .current_header_fields()
                                    .get(dialog.columns[0])
                                    .cloned()
                                    .unwrap_or_default()
                            }),
                        }),
                        None,
                    ),
                    StructuralRequest::Move => document.materialization_storage_estimate(
                        Some(&ColumnTransformation::Arrange {
                            source_width: document.total_columns,
                            output_columns: (0..document.total_columns).collect(),
                        }),
                        None,
                    ),
                }
            }
            Self::Materialize(transformation, _, records) => document
                .materialization_storage_estimate_for_records(Some(transformation), None, *records),
            Self::DeleteColumns(columns) => document.materialization_storage_estimate(
                Some(&ColumnTransformation::Arrange {
                    source_width: document.total_columns,
                    output_columns: (0..document.total_columns)
                        .filter(|column| !columns.contains(column))
                        .collect(),
                }),
                None,
            ),
            Self::ReplaceAll {
                query,
                replacement,
                case_sensitivity,
            } => document.materialization_storage_estimate(
                None,
                Some(&LiteralReplacement {
                    needle: query.clone(),
                    replacement: replacement.clone(),
                    case_sensitivity: *case_sensitivity,
                }),
            ),
            Self::Export(_) => document.session.file_size.saturating_mul(
                if document.session.encoding() == Encoding::Utf8 {
                    1
                } else {
                    3
                },
            ),
            Self::SaveUtf8(_) => document
                .materialization_storage_estimate(document.save_as_transformation().as_ref(), None),
            Self::Save(Some(_)) => document
                .materialization_storage_estimate(document.save_as_transformation().as_ref(), None),
            Self::DeleteRows(_) | Self::Save(None) => plain(),
        }
    }
}

pub(super) struct StorageReview {
    operation: Option<PendingStorageOperation>,
    path_input: String,
    required_bytes: u64,
    retained_paths: Vec<PathBuf>,
    checked_path: PathBuf,
    result: Option<Result<(StorageSpace, u64), String>>,
    worker: Option<JoinHandle<Result<(StorageSpace, u64), String>>>,
}

impl StorageReview {
    fn new(
        directory: PathBuf,
        required_bytes: u64,
        retained_paths: Vec<PathBuf>,
        operation: Option<PendingStorageOperation>,
    ) -> Self {
        let mut review = Self {
            operation,
            path_input: directory.display().to_string(),
            required_bytes,
            retained_paths,
            checked_path: PathBuf::new(),
            result: None,
            worker: None,
        };
        review.check();
        review
    }

    fn check(&mut self) {
        if self.worker.is_some() {
            return;
        }
        let directory = PathBuf::from(&self.path_input);
        self.checked_path = directory.clone();
        self.result = None;
        let retained_paths = self.retained_paths.clone();
        let validate_custom_scratch = self
            .operation
            .as_ref()
            .is_none_or(PendingStorageOperation::uses_working_directory)
            && directory != std::env::temp_dir();
        self.worker = Some(thread::spawn(move || {
            let space = if validate_custom_scratch {
                validate_scratch_directory(&directory)
            } else {
                inspect_storage(&directory)
            }
            .map_err(|error| error.to_string())?;
            let retained_bytes = retained_paths.iter().try_fold(0_u64, |bytes, path| {
                std::fs::metadata(path).map(|metadata| bytes.saturating_add(metadata.len()))
                    .map_err(|error| format!("Cannot inspect retained working file {}: {error}. Reconnect its drive before continuing.", path.display()))
            })?;
            Ok((space, retained_bytes))
        }));
    }

    fn poll(&mut self) {
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.is_finished())
        {
            self.result = Some(self.worker.take().unwrap().join().unwrap_or_else(|_| {
                Err("Storage check failed. Try checking the folder again.".into())
            }));
        }
    }

    fn ready(&self) -> bool {
        self.checked_path == Path::new(&self.path_input)
            && self.result.as_ref().is_some_and(|result| {
                result
                    .as_ref()
                    .is_ok_and(|(space, _)| space.available_bytes >= self.required_bytes)
            })
    }
}

impl QuarryApp {
    /// Applies persisted storage preferences and kicks off a non-blocking startup revalidation
    /// of any saved custom temporary folder without silently falling back to another volume.
    pub(super) fn apply_loaded_storage_preferences(&mut self) {
        if let Some(error) = self.storage_preferences.load_error.clone() {
            self.active.notice = Some(AppMessage::warning(error));
            return;
        }
        let Some(saved) = self.storage_preferences.scratch_directory.clone() else {
            return;
        };
        self.working_directory = saved.clone();
        self.custom_working_directory = true;
        for tab in std::iter::once(&mut self.active).chain(self.tabs.iter_mut()) {
            if let Some(document) = tab.document.as_mut() {
                document.working_directory = self.working_directory.clone();
                document.custom_working_directory = true;
            }
        }
        let checked = saved.clone();
        self.scratch_validation_worker = Some((
            checked,
            thread::spawn(move || {
                validate_scratch_directory(&saved)
                    .map(|_| ())
                    .map_err(|error| format_unavailable_scratch_notice(&saved, &error.to_string()))
            }),
        ));
    }

    /// Collects the non-blocking startup scratch validation result and surfaces a notice
    /// only if the checked folder is still the active custom working folder.
    pub(super) fn poll_scratch_validation_worker(&mut self) {
        if self
            .scratch_validation_worker
            .as_ref()
            .is_some_and(|(_, worker)| worker.is_finished())
        {
            let (checked, worker) = self.scratch_validation_worker.take().unwrap();
            let outcome = worker.join().unwrap_or_else(|_| {
                Err(format_unavailable_scratch_notice(
                    &checked,
                    "validation worker failed",
                ))
            });
            if let Err(message) = outcome
                && self.custom_working_directory
                && self.working_directory == checked
            {
                self.active.notice = Some(AppMessage::warning(message));
            }
        }
    }

    /// Strictly revalidates a persisted/custom scratch folder before any operation uses it.
    /// Never silently falls back to another volume.
    pub(super) fn ensure_working_directory_available(&mut self) -> Result<(), String> {
        if !self.custom_working_directory {
            return Ok(());
        }
        validate_scratch_directory(&self.working_directory)
            .map(|_| ())
            .map_err(|error| {
                format_unavailable_scratch_notice(&self.working_directory, &error.to_string())
            })
    }

    /// Enforces the shared worker limit and defers large writes for this tab's review.
    /// Returns true when the caller must not start the operation immediately.
    pub(super) fn defer_storage(&mut self, operation: PendingStorageOperation) -> bool {
        if self
            .active
            .document
            .as_ref()
            .is_some_and(Document::join_reserved)
        {
            self.active.notice = Some(AppMessage::warning(join_files::INPUT_RESERVED));
            return true;
        }
        if !self.write_slot_available() {
            self.active.notice = Some(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish or cancel it before starting another.",
            ));
            return true;
        }
        if self.active.storage_approved {
            if operation.uses_working_directory()
                && let Err(message) = self.ensure_working_directory_available()
            {
                self.begin_storage_review(operation);
                self.active.notice = Some(AppMessage::warning(message));
                return true;
            }
            return false;
        }
        if operation.uses_working_directory()
            && let Err(message) = self.ensure_working_directory_available()
        {
            self.begin_storage_review(operation);
            self.active.notice = Some(AppMessage::warning(message));
            return true;
        }
        let Some(document) = self.active.document.as_mut() else {
            return false;
        };
        document.commit_edits();
        if operation.allowance(document) < STORAGE_REVIEW_BYTES {
            return false;
        }
        // A completed index prevents a byte-count fallback from greatly overstating padding/spill needs.
        if document.index.is_none() {
            self.active.notice = Some(AppMessage::warning(
                "Wait for indexing to finish so Quarry can calculate storage requirements.",
            ));
            return true;
        }
        self.begin_storage_review(operation);
        true
    }

    /// Stores the pending operation and disk estimate on the active document tab.
    pub(super) fn begin_storage_review(&mut self, operation: PendingStorageOperation) {
        let Some(document) = self.active.document.as_ref() else {
            return;
        };
        let directory = match &operation {
            PendingStorageOperation::Save(destination) => destination
                .as_ref()
                .unwrap_or(&document.logical_path)
                .parent(),
            PendingStorageOperation::SaveUtf8(destination)
            | PendingStorageOperation::Export(destination) => destination.parent(),
            _ => Some(self.working_directory.as_path()),
        }
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
        self.active.storage_review = Some(StorageReview::new(
            directory,
            operation.allowance(document),
            document.retained_storage_paths(),
            Some(operation),
        ));
        self.active.notice = None;
    }

    /// Applies the persistent index cache setting immediately across the app and open tabs.
    pub(super) fn set_index_cache_enabled(&mut self, enabled: bool) {
        self.index_cache_enabled = enabled;
        for tab in std::iter::once(&mut self.active).chain(self.tabs.iter_mut()) {
            if let Some(document) = tab.document.as_mut() {
                document.index_cache_enabled = enabled;
            }
        }
    }

    /// Starts clearing the persistent index cache on a background worker owned by `QuarryApp`.
    pub(super) fn start_clear_index_cache(&mut self) {
        if self.cache_clear_worker.is_some() {
            return;
        }
        self.cache_clear_status = None;
        self.cache_clear_worker = Some(thread::spawn(|| {
            quarry_core::clear_index_cache(None)
                .map(|summary| {
                    if summary.files_removed == 0 {
                        "Index cache is already empty.".into()
                    } else {
                        format!(
                            "Cleared {} cached index files ({}).",
                            summary.files_removed,
                            format_bytes(summary.bytes_reclaimed)
                        )
                    }
                })
                .map_err(|error| error.to_string())
        }));
    }

    /// Collects a finished cache-clear worker even after the settings dialog has closed.
    pub(super) fn poll_cache_clear_worker(&mut self) {
        if self
            .cache_clear_worker
            .as_ref()
            .is_some_and(|worker| worker.is_finished())
        {
            let outcome = self
                .cache_clear_worker
                .take()
                .unwrap()
                .join()
                .unwrap_or_else(|_| Err("Failed to clear index cache.".into()));
            if self.active.storage_review.is_none() {
                self.active.notice = Some(match &outcome {
                    Ok(message) => AppMessage::status(message.clone()),
                    Err(error) => AppMessage::error(error.clone()),
                });
            }
            self.cache_clear_status = Some(outcome);
        }
    }

    /// Opens working-folder settings without reserving a document operation.
    pub(super) fn open_storage_settings(&mut self) {
        self.poll_cache_clear_worker();
        if self.cache_clear_worker.is_none() {
            self.cache_clear_status = None;
        }
        self.active.storage_review = Some(StorageReview::new(
            self.working_directory.clone(),
            0,
            self.active
                .document
                .as_ref()
                .map(Document::retained_storage_paths)
                .unwrap_or_default(),
            None,
        ));
    }

    /// Reviews this tab's storage and propagates an accepted working folder to all tabs.
    pub(super) fn show_storage_review(&mut self, ctx: &egui::Context) {
        self.poll_cache_clear_worker();
        let write_slot_available = self.write_slot_available();
        let mut cache_enabled = self.index_cache_enabled;
        let clearing_cache = self.cache_clear_worker.is_some();
        let cache_clear_status = self.cache_clear_status.clone();
        let Some(review) = self.active.storage_review.as_mut() else {
            return;
        };
        review.poll();
        let working = review
            .operation
            .as_ref()
            .is_none_or(PendingStorageOperation::uses_working_directory);
        let ready = review.ready();
        let current = review.checked_path == Path::new(&review.path_input);
        let problem = current && review.result.is_some() && !ready;
        let mut accept = false;
        let mut cancel = false;
        let mut check = false;
        let mut clear_cache = false;
        let modal = egui::Modal::new(egui::Id::new("quarry-storage-review"))
            .frame(compact_tool_frame(ctx))
            .backdrop_color(Color32::from_black_alpha(40))
            .show(ctx, |ui| {
                compact_tool_style(ui.style_mut());
                ui.set_width(440.0);
                if let Some(operation) = &review.operation {
                    ui.heading(if problem {
                        if review.result.as_ref().is_some_and(Result::is_ok) {
                            "More disk space needed".to_owned()
                        } else {
                            "Storage needs attention".to_owned()
                        }
                    } else if ready {
                        format!("Ready to {}", operation.action_name())
                    } else {
                        format!("Preparing to {}…", operation.action_name())
                    });
                    ui.add_space(4.0);
                    ui.label(match operation {
                        PendingStorageOperation::Structural(dialog)
                            if dialog.request == StructuralRequest::Sort =>
                        {
                            "Quarry uses temporary files to sort large files without loading everything into memory. Your original file stays unchanged until you save."
                        }
                        PendingStorageOperation::Save(_) | PendingStorageOperation::SaveUtf8(_) =>
                            "Quarry uses temporary space to save your changes safely. Continue will save your changes.",
                        PendingStorageOperation::Export(_) =>
                            "Quarry uses temporary space to prepare your export. Your open file will stay unchanged.",
                        _ => "Quarry uses temporary files to prepare this operation safely. Your original file stays unchanged until you save.",
                    });
                } else {
                    ui.heading("Temporary storage");
                }
                if review.operation.is_some() && !write_slot_available {
                    ui.colored_label(ui.visuals().error_fg_color, "Two file operations are already running. Wait for one to finish or cancel it before continuing.");
                }
                ui.add_space(6.0);
                if !current {
                    ui.label("Folder changed. Check again before continuing.");
                } else {
                    match &review.result {
                        Some(Ok(_)) if ready => {
                            ui.colored_label(theme::ui_palette(ui).success_text, "Enough disk space is available.");
                        }
                        Some(Ok(_)) => {
                            ui.label(if working {
                                "Free up space or choose a working folder on another drive."
                            } else {
                                "Free up space, or cancel and choose another destination."
                            });
                        }
                        Some(Err(_)) => {
                            ui.label("Check the storage details below to continue.");
                        }
                        None => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Checking disk space…");
                            });
                        }
                    }
                }
                egui::ScrollArea::vertical()
                    .id_salt("quarry-storage-details-scroll")
                    .max_height((ctx.content_rect().height() - 220.0).clamp(80.0, 440.0))
                    .show(ui, |ui| {
                        let mut details = |ui: &mut egui::Ui| {
                            if review.operation.is_some() {
                                ui.label(format!("Estimated temporary space (conservative): {}", format_bytes(review.required_bytes)));
                                ui.small("This is a cautious allowance. Actual usage may be lower.");
                            }
                            if current {
                                match &review.result {
                                    Some(Ok((space, retained))) => {
                                        ui.label(format!("Available space: {}", format_bytes(space.available_bytes)));
                                        ui.label(format!("Retained working and Undo/Redo files: {}", format_bytes(*retained)));
                                        ui.small("Retained files already reduce the free space and are not counted twice.");
                                    }
                                    Some(Err(error)) => { ui.colored_label(ui.visuals().error_fg_color, error); }
                                    None => {}
                                }
                            }
                            ui.separator();
                            let label = ui.label(if working { "Working folder" } else { "Destination volume" });
                            ui.add_enabled(working && review.worker.is_none(), egui::TextEdit::singleline(&mut review.path_input).desired_width(f32::INFINITY)).labelled_by(label.id);
                            if working {
                                ui.add_enabled_ui(review.worker.is_none(), |ui| {
                                    ui.horizontal_wrapped(|ui| {
                                        if ui.button("Choose another folder…").clicked()
                                            && let Some(path) = rfd::FileDialog::new()
                                                .set_title("Choose temporary working folder")
                                                .set_directory(&review.path_input)
                                                .pick_folder()
                                        {
                                            review.path_input = path.display().to_string();
                                            check = true;
                                        }
                                        if ui.button("Use system temporary folder").clicked() {
                                            review.path_input = std::env::temp_dir().display().to_string(); check = true;
                                        }
                                    });
                                });
                                ui.small("Quarry remembers this folder across restarts and checks it before each use. Existing working and Undo files stay on their current drive until no longer needed. Keep that drive connected.");
                            } else {
                                ui.small("Save and export use temporary space beside the destination. To use another drive, cancel and choose Save As or another export destination.");
                            }
                            if ready && ui.button("Check again").clicked() { check = true; }
                            ui.small("Quarry checks again before writing. If a write fails, it removes unfinished output and preserves your current document and required Undo files.");
                            if review.operation.is_none() {
                                ui.separator();
                                ui.label("Persistent index cache");
                                ui.checkbox(
                                    &mut cache_enabled,
                                    "Reuse cached index for large local files",
                                );
                                ui.horizontal_wrapped(|ui| {
                                    if ui
                                        .add_enabled(!clearing_cache, egui::Button::new("Clear index cache"))
                                        .clicked()
                                    {
                                        clear_cache = true;
                                    }
                                    if clearing_cache {
                                        ui.spinner();
                                        ui.small("Clearing index cache…");
                                    }
                                });
                                if let Some(status) = &cache_clear_status {
                                    match status {
                                        Ok(message) => {
                                            ui.small(message);
                                        }
                                        Err(error) => {
                                            ui.colored_label(ui.visuals().error_fg_color, error);
                                        }
                                    }
                                }
                                ui.small("Stores structural offsets only in a private folder (up to 256 MB); never file names or cell data.");
                            }
                        };
                        if review.operation.is_none() {
                            details(ui);
                        } else {
                            egui::CollapsingHeader::new("Storage details")
                                .id_salt("quarry-storage-details")
                                .open(problem.then_some(true))
                                .show(ui, details);
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() { cancel = true; }
                    if !review.ready() && ui.add_enabled(review.worker.is_none(), egui::Button::new("Check again")).clicked() { check = true; }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add_enabled(review.ready() && !check && (review.operation.is_none() || write_slot_available), theme::primary_button(if review.operation.is_some() { "Continue" } else { "Use folder" })).clicked() { accept = true; }
                    });
                });
            });
        cancel |= modal.should_close();
        if check {
            review.check();
        }
        if cache_enabled != self.index_cache_enabled {
            self.set_index_cache_enabled(cache_enabled);
        }
        if clear_cache {
            self.start_clear_index_cache();
        }
        if cancel {
            self.active.storage_review = None;
            if self.active.close_after_save {
                self.active.close_after_save = false;
                self.active.close_confirmation_open = true;
            }
        } else if accept {
            self.accept_storage_review(ctx, working);
        }
        ctx.request_repaint_after(POLL_INTERVAL);
    }

    /// Rechecks capacity before consuming a review, including deferred split materialization.
    /// A full worker budget retains the operation and its completed storage check for retry.
    fn accept_storage_review(&mut self, ctx: &egui::Context, working: bool) {
        if self
            .active
            .storage_review
            .as_ref()
            .is_some_and(|review| review.operation.is_some())
            && !self.write_slot_available()
        {
            return;
        }
        let mut review = self.active.storage_review.take().unwrap();
        let mut persistence_warning = None;
        if working {
            let chosen = PathBuf::from(&review.path_input);
            let is_custom = chosen != std::env::temp_dir();
            if is_custom && let Err(error) = validate_scratch_directory(&chosen) {
                review.checked_path = chosen;
                review.result = Some(Err(error.to_string()));
                self.active.storage_review = Some(review);
                return;
            }
            self.working_directory = chosen.clone();
            self.custom_working_directory = is_custom;
            if let Err(error) = self
                .storage_preferences
                .set_scratch_directory(is_custom.then_some(chosen.as_path()))
            {
                let warning = AppMessage::warning(format!(
                    "Using {} for this session, but Quarry could not save this preference: {error}",
                    chosen.display()
                ));
                self.active.notice = Some(warning.clone());
                persistence_warning = Some(warning);
            }
            for tab in std::iter::once(&mut self.active).chain(self.tabs.iter_mut()) {
                if let Some(document) = tab.document.as_mut() {
                    document.working_directory = self.working_directory.clone();
                    document.custom_working_directory = self.custom_working_directory;
                }
            }
            if let Some(dialog) = self.join_files.as_mut() {
                dialog.temporary = self.working_directory.clone();
            }
        }
        if let Some(operation) = review.operation {
            // Only the operation resumed by this dialog bypasses the presentation gate.
            // Engine checks still run immediately before writes.
            self.active.storage_approved = true;
            self.resume_storage_operation(operation, ctx);
            self.active.storage_approved = false;
            if self.active.notice.is_none() {
                self.active.notice = persistence_warning;
            }
        }
    }

    /// Resumes the owning tab's approved operation through its normal safety guards.
    fn resume_storage_operation(
        &mut self,
        operation: PendingStorageOperation,
        ctx: &egui::Context,
    ) {
        match operation {
            PendingStorageOperation::Structural(dialog) => {
                self.active.structural_dialog = Some(dialog);
                self.apply_structural_dialog_action(StructuralDialogAction::Apply);
            }
            PendingStorageOperation::Materialize(transformation, columns, _) => {
                self.active.notice = self
                    .active
                    .document
                    .as_mut()
                    .unwrap()
                    .begin_materialization(transformation, columns)
                    .err();
            }
            PendingStorageOperation::DeleteColumns(columns) => self.apply_delete_columns(columns),
            PendingStorageOperation::DeleteRows(rows) => self.apply_delete_rows(rows),
            PendingStorageOperation::ReplaceAll {
                query,
                replacement,
                case_sensitivity,
            } => {
                self.active.notice = self
                    .active
                    .document
                    .as_mut()
                    .unwrap()
                    .start_replace_all_with_case(&query, &replacement, case_sensitivity)
                    .err();
            }
            PendingStorageOperation::Save(destination) => {
                let started = if let Some(destination) = destination {
                    self.save_as_picker_result(Some(destination))
                } else {
                    self.save_current()
                };
                if !started && self.active.close_after_save {
                    self.active.close_after_save = false;
                    self.active.close_confirmation_open = true;
                }
            }
            PendingStorageOperation::SaveUtf8(destination) => {
                self.save_as_picker_result_format(Some(destination), true);
            }
            PendingStorageOperation::Export(destination) => {
                self.export_picker_result(Some(destination))
            }
        }
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update_review_until(
        app: &mut QuarryApp,
        ctx: &egui::Context,
        ready: impl Fn(&QuarryApp) -> bool,
    ) -> egui::FullOutput {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(app.active.storage_review.is_some());
            let output = ctx.run(egui::RawInput::default(), |ctx| {
                eframe::App::update(app, ctx, &mut eframe::Frame::_new_kittest());
            });
            if ready(app) {
                return output;
            }
            assert!(
                Instant::now() < deadline,
                "background job stalled behind storage review"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn deferred_materialization_retains_review_until_a_global_write_slot_is_free() {
        let directory = tempfile::tempdir().unwrap();
        let a = directory.path().join("a.csv");
        let b = directory.path().join("b.csv");
        let c = directory.path().join("c.csv");
        let original = b"name,value\nAlpha|Beta,1\n";
        for path in [&a, &b, &c] {
            std::fs::write(path, original).unwrap();
        }
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut app = QuarryApp::new(Some(a.clone()), Instant::now());
        finish(&mut app);
        let transformation = ColumnTransformation::split_with_blank_headers(
            0,
            b"|".to_vec(),
            2,
            Some(b"name".to_vec()),
        )
        .unwrap();
        app.begin_storage_review(PendingStorageOperation::Materialize(
            transformation,
            [0, 1].into_iter().collect(),
            2,
        ));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !app.active.storage_review.as_ref().unwrap().ready() {
            app.active.storage_review.as_mut().unwrap().poll();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        for (path, header) in [(&b, "saved-b"), (&c, "saved-c")] {
            app.open_new_path(path.clone()).unwrap();
            app.active
                .document
                .as_mut()
                .unwrap()
                .rename_header(0, header.into())
                .unwrap();
            assert!(app.save_as_picker_result(Some(path.with_extension("saved.csv"))));
        }
        app.open_new_path(a.clone()).unwrap();
        assert!(!app.write_slot_available());
        app.accept_storage_review(&ctx, true);
        assert!(app.active.storage_review.as_ref().unwrap().ready());
        assert!(!app.active.storage_approved);
        assert!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .structural_job
                .is_none()
        );
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            app.show_storage_review(ctx)
        });
        let nodes = output.platform_output.accesskit_update.unwrap().nodes;
        assert!(nodes.iter().any(|(_, node)| {
            node.label()
                .or(node.value())
                .is_some_and(|text| text.contains("Two file operations"))
        }));
        let continue_button = nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Continue"))
            .unwrap()
            .1
            .clone();
        assert!(continue_button.is_disabled());

        // Complete one owning tab's save without polling the other reserved writer.
        app.open_new_path(b.clone()).unwrap();
        while app.active.document.as_ref().unwrap().save_job.is_some() {
            app.poll_active_document(&ctx, true);
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        app.open_new_path(a.clone()).unwrap();
        assert!(app.write_slot_available());
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            app.show_storage_review(ctx)
        });
        let nodes = output.platform_output.accesskit_update.unwrap().nodes;
        let (target, button) = nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Continue"))
            .unwrap();
        assert!(!button.is_disabled());
        let input = egui::RawInput {
            events: vec![egui::Event::AccessKitActionRequest(
                egui::accesskit::ActionRequest {
                    action: egui::accesskit::Action::Click,
                    target: *target,
                    data: None,
                },
            )],
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| app.show_storage_review(ctx));
        assert!(app.active.storage_review.is_none());
        assert!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .structural_job
                .is_some()
        );
        assert!(!app.write_slot_available());
        while app.document_tabs().any(DocumentTab::busy) {
            app.poll_document_tabs(&ctx);
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(
            std::fs::read(app.active.document.as_ref().unwrap().session.path()).unwrap(),
            b"name,,value\nAlpha,Beta,1\n"
        );
        for path in [&a, &b, &c] {
            assert_eq!(std::fs::read(path).unwrap(), original);
        }
    }

    #[test]
    fn reordered_save_as_reserves_space_for_padding_every_ragged_row() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("ragged.csv");
        let header = vec!["x"; 128].join(",");
        let original = format!("{header}\n{}", "1\n".repeat(1000));
        std::fs::write(&source, &original).unwrap();
        let mut document = Document::open(
            &source,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        document.index = Some(document.job.take().unwrap().wait().unwrap());
        let ordinary_save_bytes = PendingStorageOperation::Save(None).allowance(&document);
        document.move_column(127, 0).unwrap();
        let copy = PendingStorageOperation::Save(Some(directory.path().join("copy.csv")));
        // Each short row must gain 127 separators when projected to the known width.
        let padded_bytes = original.len() as u64 + 127 * 1000;
        assert!(copy.allowance(&document) >= padded_bytes);
        assert_eq!(
            PendingStorageOperation::Save(None).allowance(&document),
            ordinary_save_bytes
        );
    }

    #[test]
    fn storage_review_keeps_background_jobs_advancing_and_blocks_document_input() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.csv");
        let other = directory.path().join("other.csv");
        let output = directory.path().join("filtered.csv");
        let original = b"name,value\nAda Lovelace,1\nGrace Hopper,2\n";
        std::fs::write(&source, original).unwrap();
        std::fs::write(&other, b"other\nfile\n").unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        assert!(app.active.document.as_ref().unwrap().index.is_none());
        app.open_storage_settings();
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        input.dropped_files.push(egui::DroppedFile {
            path: Some(other),
            ..Default::default()
        });
        let close_output = ctx.run(input, |ctx| {
            eframe::App::update(&mut app, ctx, &mut eframe::Frame::_new_kittest());
        });
        assert!(
            close_output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::CancelClose))
        );
        assert!(!app.active.close_confirmation_open);
        assert_eq!(app.active.document.as_ref().unwrap().session.path(), source);
        let frame = update_review_until(&mut app, &ctx, |app| {
            app.active.document.as_ref().unwrap().index.is_some()
                && app.active.storage_review.as_ref().unwrap().worker.is_none()
        });
        assert!(
            frame
                .platform_output
                .accesskit_update
                .unwrap()
                .nodes
                .iter()
                .any(|(_, node)| node.label() == Some("Use folder"))
        );

        app.active.storage_review = None;
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_find_next(b"Grace")
            .unwrap();
        app.open_storage_settings();
        update_review_until(&mut app, &ctx, |app| {
            app.active.document.as_ref().unwrap().search_job.is_none()
        });
        assert_eq!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .last_match
                .unwrap()
                .row,
            2
        );

        app.active.storage_review = None;
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"1".to_vec(),
            ))
            .unwrap();
        app.open_storage_settings();
        update_review_until(&mut app, &ctx, |app| {
            let document = app.active.document.as_ref().unwrap();
            document.filter_job.is_none() && document.visible_filter_rows().len() == 1
        });
        assert!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .filter_progress()
                .unwrap()
                .done
        );

        app.active.storage_review = None;
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_filtered_export(output.clone())
            .unwrap();
        app.open_storage_settings();
        update_review_until(&mut app, &ctx, |app| {
            app.active.document.as_ref().unwrap().export_job.is_none()
        });
        assert_eq!(
            std::fs::read(output).unwrap(),
            b"name,value\nAda Lovelace,1\n"
        );

        app.active.storage_review = None;
        let document = app.active.document.as_mut().unwrap();
        document.clear_filter().unwrap();
        document.start_split(0, b" ".to_vec()).unwrap();
        // Exercise the large-operation gate without allocating a large fixture.
        document.whitespace.invalidate();
        std::sync::Arc::get_mut(&mut document.session)
            .unwrap()
            .file_size = STORAGE_REVIEW_BYTES;
        app.open_storage_settings();
        update_review_until(&mut app, &ctx, |app| {
            app.active
                .document
                .as_ref()
                .unwrap()
                .pending_materialization
                .is_some()
        });
        assert!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .structural_job
                .is_none()
        );
        assert!(
            app.active
                .storage_review
                .as_ref()
                .unwrap()
                .operation
                .is_none()
        );
        assert_eq!(std::fs::read(&source).unwrap(), original);
        app.active.storage_review = None;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut eframe::Frame::_new_kittest());
        });
        assert!(matches!(
            app.active.storage_review.as_ref().unwrap().operation,
            Some(PendingStorageOperation::Materialize(..))
        ));
        assert!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .pending_materialization
                .is_none()
        );
    }

    fn finish(app: &mut QuarryApp) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let document = app.active.document.as_mut().unwrap();
            document.poll().unwrap();
            if let Some(ready) = document.poll_structural_edit().unwrap() {
                app.install_materialized_working_copy(ready).unwrap();
            }
            let document = app.active.document.as_ref().unwrap();
            if document.structural_job.is_none() && document.index.is_some() {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
    }

    #[test]
    fn changing_working_folder_preserves_history_failures_and_atomic_save() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let source = directory.path().join("source.csv");
        let original = b"key,value,extra\na,1,x\nb,2,y\nc,3,z\n";
        std::fs::write(&source, original).unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish(&mut app);
        app.working_directory = first.clone();
        app.active.document.as_mut().unwrap().working_directory = first.clone();
        app.apply_delete_columns(vec![2]);
        finish(&mut app);
        let first_copy = app
            .active
            .document
            .as_ref()
            .unwrap()
            .session
            .path()
            .to_path_buf();
        assert!(first_copy.starts_with(&first));
        assert_eq!(
            std::fs::read(&first_copy).unwrap(),
            b"key,value\na,1\nb,2\nc,3\n"
        );
        app.working_directory = second.clone();
        app.active.document.as_mut().unwrap().working_directory = second.clone();
        app.apply_delete_rows(Vec::from([2..=2]));
        finish(&mut app);
        let second_copy = app
            .active
            .document
            .as_ref()
            .unwrap()
            .session
            .path()
            .to_path_buf();
        assert!(second_copy.starts_with(&second));
        assert!(first_copy.exists());
        app.swap_edit_history(false).unwrap();
        finish(&mut app);
        assert_eq!(
            app.active.document.as_ref().unwrap().session.path(),
            first_copy
        );
        app.swap_edit_history(true).unwrap();
        finish(&mut app);
        assert_eq!(
            app.active.document.as_ref().unwrap().session.path(),
            second_copy
        );
        app.active.document.as_mut().unwrap().working_directory =
            directory.path().join("disconnected");
        app.apply_delete_rows(Vec::from([1..=1]));
        assert!(
            app.active
                .notice
                .as_ref()
                .unwrap()
                .contains("Choose another folder")
        );
        assert!(!app.active.document.as_ref().unwrap().source_changed);
        assert!(first_copy.exists() && second_copy.exists());
        assert_eq!(std::fs::read(&source).unwrap(), original);
        // Saving uses the source volume even though the selected working location vanished.
        assert!(app.save_current());
        let deadline = Instant::now() + Duration::from_secs(10);
        let saved = loop {
            if let Some(saved) = app.active.document.as_mut().unwrap().poll_save().unwrap() {
                break saved;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        };
        assert_eq!(saved, (source.clone(), true));
        assert_eq!(std::fs::read(&source).unwrap(), b"key,value\na,1\nc,3\n");
        app.replace_document_with_options(source, OpenOptions::default())
            .unwrap();
        assert!(!first_copy.exists() && !second_copy.exists());
        assert_eq!(std::fs::read_dir(first).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(second).unwrap().count(), 0);
    }

    #[test]
    fn storage_review_exposes_capacity_and_requires_a_current_successful_check() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_path_buf();
        let mut app = QuarryApp::new(None, Instant::now());
        app.active.storage_review = Some(StorageReview {
            operation: Some(PendingStorageOperation::DeleteRows(Vec::from([1..=1]))),
            path_input: path.display().to_string(),
            checked_path: path.clone(),
            required_bytes: 200,
            retained_paths: Vec::new(),
            worker: None,
            result: Some(Ok((
                StorageSpace {
                    directory: path,
                    required_bytes: 0,
                    available_bytes: 100,
                },
                50,
            ))),
        });
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 750.0),
                )),
                ..Default::default()
            },
            |ctx| app.show_storage_review(ctx),
        );
        let nodes = output.platform_output.accesskit_update.unwrap().nodes;
        assert!(
            nodes
                .iter()
                .any(|(_, node)| node.label() == Some("Continue") && node.is_disabled())
        );
        for expected in [
            "More disk space needed",
            "Estimated temporary space",
            "Available space",
            "Retained working and Undo/Redo files",
            "Choose another folder",
        ] {
            assert!(
                nodes.iter().any(|(_, node)| node
                    .value()
                    .is_some_and(|value| value.contains(expected))
                    || node.label().is_some_and(|value| value.contains(expected))),
                "missing {expected}"
            );
        }
        let review = app.active.storage_review.as_mut().unwrap();
        review
            .result
            .as_mut()
            .unwrap()
            .as_mut()
            .unwrap()
            .0
            .available_bytes = 200;
        assert!(review.ready());
        review.path_input.push_str("/different");
        assert!(!review.ready());
        review.check();
        let deadline = Instant::now() + Duration::from_secs(5);
        while review.worker.is_some() {
            review.poll();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(review.result.as_ref().unwrap().is_err());
        assert!(!review.ready());
    }

    #[test]
    fn ready_sort_keeps_details_optional_and_continues_into_a_working_copy() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.csv");
        let original = b"key,value\nb,2\na,1\n";
        std::fs::write(&source, original).unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish(&mut app);
        app.begin_storage_review(PendingStorageOperation::Structural(StructuralDialog::sort(
            0,
        )));
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        ctx.style_mut(|style| style.animation_time = 0.0);
        let output = update_review_until(&mut app, &ctx, |app| {
            app.active.storage_review.as_ref().unwrap().ready()
        });
        let nodes = output.platform_output.accesskit_update.unwrap().nodes;
        for expected in [
            "Ready to sort",
            "Enough disk space is available",
            "Storage details",
        ] {
            assert!(
                nodes.iter().any(|(_, node)| node
                    .label()
                    .is_some_and(|text| text.contains(expected))
                    || node.value().is_some_and(|text| text.contains(expected))),
                "missing {expected}: {:?}",
                nodes
                    .iter()
                    .map(|(_, node)| (node.label(), node.value()))
                    .collect::<Vec<_>>()
            );
        }
        assert!(!nodes.iter().any(
            |(_, node)| node.label().or(node.value()).is_some_and(|text| {
                text.contains("Estimated temporary space") || text == "Working folder"
            })
        ));
        assert!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .structural_job
                .is_none()
        );

        let target = nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Storage details"))
            .unwrap()
            .0;
        let click = |target| egui::RawInput {
            events: vec![egui::Event::AccessKitActionRequest(
                egui::accesskit::ActionRequest {
                    action: egui::accesskit::Action::Click,
                    target,
                    data: None,
                },
            )],
            ..Default::default()
        };
        let _ = ctx.run(click(target), |ctx| {
            eframe::App::update(&mut app, ctx, &mut eframe::Frame::_new_kittest());
        });
        let output = update_review_until(&mut app, &ctx, |_| true);
        let nodes = output.platform_output.accesskit_update.unwrap().nodes;
        assert!(nodes.iter().any(|(_, node)| {
            node.label()
                .or(node.value())
                .is_some_and(|text| text.contains("Estimated temporary space"))
        }));
        let (target, button) = nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Continue"))
            .unwrap();
        assert!(!button.is_disabled());
        let _ = ctx.run(click(*target), |ctx| {
            eframe::App::update(&mut app, ctx, &mut eframe::Frame::_new_kittest());
        });
        assert!(app.active.storage_review.is_none());
        finish(&mut app);
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(
            std::fs::read(document.session.path()).unwrap(),
            b"key,value\na,1\nb,2\n"
        );
        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert!(document.can_undo());
    }

    #[test]
    fn large_save_waits_for_review_without_allocating_or_losing_edits() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.csv");
        std::fs::write(&source, b"key\na\n").unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish(&mut app);
        let document = app.active.document.as_mut().unwrap();
        Arc::make_mut(&mut document.cell_edits).insert((1, 0), b"edited".to_vec().into());
        // Simulate large metadata without writing a large fixture. No worker starts here.
        document.whitespace.invalidate();
        std::sync::Arc::get_mut(&mut document.session)
            .unwrap()
            .file_size = STORAGE_REVIEW_BYTES;
        app.active.close_after_save = true;
        assert!(app.save_current());
        assert!(app.active.storage_review.is_some());
        assert!(app.active.document.as_ref().unwrap().save_job.is_none());
        assert_eq!(std::fs::read(&source).unwrap(), b"key\na\n");
        update_review_until(&mut app, &egui::Context::default(), |app| {
            app.active.storage_review.as_ref().unwrap().worker.is_none()
        });
        assert!(app.active.close_after_save);
        assert!(!app.active.close_confirmation_open);
        app.active.storage_review = None;
        assert_eq!(
            app.active.document.as_ref().unwrap().cell_edits[&(1, 0)].as_slice(),
            b"edited"
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
    #[test]
    fn encoding_storage_allowances_apply_expansion_only_once() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.csv");
        std::fs::write(&source, b"name\ncaf\xe9\n").unwrap();
        let document = Document::prepare_with_encoding(
            &source,
            OpenOptions::default(),
            Some(Encoding::Latin1),
        )
        .unwrap();
        assert_eq!(
            PendingStorageOperation::SaveUtf8(directory.path().join("copy.csv"))
                .allowance(&document),
            document.materialization_storage_estimate(None, None)
        );
        let utf8_source = directory.path().join("utf8.csv");
        std::fs::write(&utf8_source, b"name\nvalue\n").unwrap();
        let utf8 = Document::prepare(&utf8_source, OpenOptions::default()).unwrap();
        assert_eq!(
            PendingStorageOperation::Export(directory.path().join("filtered.csv")).allowance(&utf8),
            utf8.session.file_size
        );
    }
    fn publish_test_generation(
        state: &mut WorkingCopyState,
        parent: &Path,
        value: &str,
    ) -> PathBuf {
        let source = parent.join("generation-fixture.csv");
        std::fs::write(&source, format!("id,value\nx, {value} \n")).unwrap();
        let session = Session::open(
            &source,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        let path = state.next_path().unwrap();
        let job = session
            .start_create_cleaned_working_copy(
                BTreeMap::new(),
                BTreeMap::new(),
                ColumnCleanup {
                    columns: vec![1],
                    operation: CleanupOperation::Trim,
                },
                path.clone(),
            )
            .unwrap();
        job.retain_working_storage(Arc::clone(&state.directory))
            .unwrap();
        assert!(matches!(job.wait().unwrap(), CleanupOutcome::Complete(_)));
        path
    }

    #[test]
    fn join_generation_lease_keeps_pruned_files_until_worker_finishes() {
        let parent = tempfile::tempdir().unwrap();
        let next_parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("original.csv");
        std::fs::write(&source, b"id,value\nx,saved\n").unwrap();
        let mut state = WorkingCopyState::new_in(parent.path()).unwrap();
        let working = publish_test_generation(&mut state, parent.path(), "working");
        let lease = Arc::clone(&state.directory);
        let private_root = lease.path().to_path_buf();
        let start = Arc::new(std::sync::Barrier::new(2));
        let worker_start = Arc::clone(&start);
        let worker = std::thread::spawn(move || {
            worker_start.wait();
            // The captured owner keeps the generation available to a real
            // background reader even after its original tab state is gone.
            let session = Session::open(
                lease.path().join("generation-1.csv"),
                OpenOptions {
                    header_mode: HeaderMode::FirstRow,
                    rows: 2,
                    ..Default::default()
                },
            )
            .unwrap();
            session.first_rows[1].fields[1].clone()
        });
        state.use_directory(next_parent.path()).unwrap();
        let current = state.next_path().unwrap();
        state.prune_directories(&current);
        assert!(state.previous_directories.is_empty());
        drop(state);
        let retained = working.exists();
        start.wait();
        let value = worker.join().unwrap();
        assert!(
            retained,
            "pruning and tab disposal must not invalidate captured input"
        );
        assert_eq!(value, b"working");
        assert!(
            !private_root.exists(),
            "last capture release removes owned private storage"
        );
        assert_eq!(std::fs::read(source).unwrap(), b"id,value\nx,saved\n");
    }
    #[cfg(unix)]
    #[test]
    fn working_generation_cleanup_never_follows_a_replaced_root() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::tempdir().unwrap();
        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        let mut state = WorkingCopyState::new_in(parent.path()).unwrap();
        let root = state.directory.path().to_owned();
        let capture = Arc::clone(&state.directory);
        let _working = publish_test_generation(&mut state, parent.path(), "current");
        let moved = parent.path().join("moved-working");
        std::fs::rename(&root, &moved).unwrap();
        std::fs::create_dir_all(root.join("unrelated")).unwrap();
        std::fs::write(
            root.join("unrelated/keep.csv"),
            b"unrelated,data\nkeep,me\n",
        )
        .unwrap();
        drop(state);
        assert!(moved.join("generation-1.csv").exists());
        drop(capture);
        assert_eq!(
            std::fs::read(root.join("unrelated/keep.csv")).unwrap(),
            b"unrelated,data\nkeep,me\n",
            "last lease cleanup must preserve the replacement tree"
        );
        assert!(
            !moved.exists(),
            "the actual owned generation root must be cleaned"
        );
    }
    #[test]
    fn cancelling_a_private_writer_preserves_a_foreign_destination_and_reports_cleanup() {
        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("source.csv");
        let original = b"id,value\nx, needs trim \n";
        std::fs::write(&source, original).unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish(&mut app);
        let document = app.active.document.as_mut().unwrap();
        document
            .start_cleanup(ColumnCleanup {
                columns: vec![1],
                operation: CleanupOperation::Trim,
            })
            .unwrap();
        let destination = match document.structural_job.as_ref().unwrap() {
            StructuralJob::Cleaning { destination, .. } => destination.clone(),
            _ => panic!("cleanup job expected"),
        };
        let foreign = b"foreign,data\npreserve,me\n";
        std::fs::write(&destination, foreign).unwrap();
        document.cancel_structural_edit();
        let deadline = Instant::now() + Duration::from_secs(5);
        while document.structural_job.is_some() {
            assert!(document.poll_structural_edit().unwrap().is_none());
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(std::fs::read(&destination).unwrap(), foreign);
        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert!(!document.working_cleanup.take_errors().is_empty());
    }

    #[test]
    fn persistent_index_cache_toggle_and_clear_worker_survive_unwritable_folder_and_dialog_close() {
        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("source.csv");
        std::fs::write(&source, b"a,b\n1,2\n").unwrap();
        let mut app = QuarryApp::new(Some(source), Instant::now());
        finish(&mut app);

        let missing_dir = parent.path().join("disconnected-drive");
        app.active.storage_review = Some(StorageReview {
            operation: None,
            path_input: missing_dir.display().to_string(),
            checked_path: missing_dir,
            required_bytes: 0,
            retained_paths: Vec::new(),
            worker: None,
            result: Some(Err("Volume disconnected".into())),
        });
        assert!(!app.active.storage_review.as_ref().unwrap().ready());

        // Toggling index cache applies immediately even though the working folder check failed.
        app.set_index_cache_enabled(false);
        assert!(!app.index_cache_enabled);
        assert!(!app.active.document.as_ref().unwrap().index_cache_enabled);

        // An in-flight cache clear owned by QuarryApp survives closing and reopening settings,
        // prevents duplicate starts, and reports completion in `active.notice` when closed.
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        app.cache_clear_worker = Some(std::thread::spawn(move || {
            let _ = rx.recv();
            Ok("Cleared 1 cached index files (4.0 KiB).".into())
        }));
        app.active.storage_review = None;
        app.open_storage_settings();
        assert!(app.cache_clear_worker.is_some());
        // Attempting to start a second clear while one is active must be a no-op.
        app.start_clear_index_cache();
        assert!(app.cache_clear_worker.is_some());

        // Closing settings before completion must still retain the worker and report its result.
        app.active.storage_review = None;
        tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.cache_clear_worker.is_some() {
            app.poll_cache_clear_worker();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(
            app.active
                .notice
                .as_ref()
                .is_some_and(|msg| msg.text.contains("Cleared 1 cached index files")),
            "expected completion notice after settings closed, got: {:?}",
            app.active.notice.as_ref().map(|m| &m.text)
        );
    }

    #[test]
    fn persisted_scratch_folder_reloads_and_refuses_silent_fallback_when_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let pref_path = dir.path().join("settings/storage-preferences");
        let custom_dir = dir.path().join("custom-scratch");
        std::fs::create_dir(&custom_dir).unwrap();

        let mut prefs = StoragePreferences::load(Some(pref_path.clone()));
        assert_eq!(prefs.scratch_directory, None);
        prefs.set_scratch_directory(Some(&custom_dir)).unwrap();
        assert_eq!(
            StoragePreferences::load(Some(pref_path.clone())).scratch_directory,
            Some(custom_dir.clone())
        );

        // Reloading into a fresh QuarryApp restores the persisted custom folder.
        let source = dir.path().join("source.csv");
        std::fs::write(&source, b"k,v\nb,2\na,1\n").unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish(&mut app);
        app.storage_preferences = StoragePreferences::load(Some(pref_path.clone()));
        app.apply_loaded_storage_preferences();
        assert_eq!(app.working_directory, custom_dir);
        assert!(app.custom_working_directory);
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.scratch_validation_worker.is_some() {
            app.poll_scratch_validation_worker();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(app.active.notice.is_none());

        // Removing the saved scratch folder (e.g. unmounted drive) must NEVER silently fall back
        // to the system temporary folder; it must surface a notice and require reselection.
        std::fs::remove_dir(&custom_dir).unwrap();
        assert!(app.ensure_working_directory_available().is_err());
        assert_eq!(
            app.working_directory, custom_dir,
            "must not silently fall back to another volume"
        );
        let deferred = app.defer_storage(PendingStorageOperation::DeleteRows(Vec::from([1..=1])));
        assert!(deferred);
        assert!(app.active.storage_review.is_some());
        assert!(
            app.active
                .notice
                .as_ref()
                .is_some_and(|m| m.text.contains("Saved temporary folder")),
            "expected unavailable scratch notice, got {:?}",
            app.active.notice.as_ref().map(|m| &m.text)
        );
        assert_eq!(
            app.working_directory, custom_dir,
            "failed revalidation must keep the path until the user explicitly chooses another"
        );

        // Clearing back to system temporary folder clears the persisted setting.
        prefs.set_scratch_directory(None).unwrap();
        assert_eq!(
            StoragePreferences::load(Some(pref_path.clone())).scratch_directory,
            None
        );

        // Paths with trailing spaces must round-trip losslessly without trimming (both hex-encoded
        // and legacy plain `scratch_directory=` entries when trimmed and untrimmed siblings coexist).
        #[cfg(unix)]
        {
            let trimmed_dir = dir.path().join("scratch with trailing space");
            let spaced_dir = dir.path().join("scratch with trailing space ");
            std::fs::create_dir(&trimmed_dir).unwrap();
            std::fs::create_dir(&spaced_dir).unwrap();
            prefs.set_scratch_directory(Some(&spaced_dir)).unwrap();
            assert_eq!(
                StoragePreferences::load(Some(pref_path.clone())).scratch_directory,
                Some(spaced_dir.clone())
            );

            // Legacy plain entry with trailing space must select the exact trailing-space folder on startup.
            std::fs::write(
                &pref_path,
                format!("scratch_directory={}\n", spaced_dir.display()),
            )
            .unwrap();
            let mut plain_app = QuarryApp::new(Some(source.clone()), Instant::now());
            finish(&mut plain_app);
            plain_app.storage_preferences = StoragePreferences::load(Some(pref_path.clone()));
            plain_app.apply_loaded_storage_preferences();
            assert_eq!(plain_app.working_directory, spaced_dir);
            let deadline = Instant::now() + Duration::from_secs(5);
            while plain_app.scratch_validation_worker.is_some() {
                plain_app.poll_scratch_validation_worker();
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            assert!(plain_app.active.notice.is_none());
            assert_eq!(plain_app.working_directory, spaced_dir);

            // An oversized encoded path (> MAX_STORAGE_PREFERENCES_BYTES) must be rejected before
            // saving rather than truncated on reload into a shorter valid parent path.
            let huge_name = format!("{} ", "a".repeat(4_090));
            let huge_path = PathBuf::from("/").join(huge_name);
            assert!(prefs.set_scratch_directory(Some(&huge_path)).is_err());
            assert_eq!(prefs.scratch_directory, Some(spaced_dir));

            // An oversized preference file on disk must fail to load with an explicit error
            // rather than parsing a truncated prefix.
            let oversized_content = format!(
                "scratch_directory_hex={}\n",
                "2f".repeat((MAX_STORAGE_PREFERENCES_BYTES as usize) / 2 + 32)
            );
            assert!(oversized_content.len() as u64 > MAX_STORAGE_PREFERENCES_BYTES);
            std::fs::write(&pref_path, oversized_content).unwrap();
            let loaded = StoragePreferences::load(Some(pref_path));
            assert_eq!(loaded.scratch_directory, None);
            assert!(
                loaded
                    .load_error
                    .as_deref()
                    .is_some_and(|msg| msg.contains("maximum supported size")),
                "expected oversized preference error, got {:?}",
                loaded.load_error
            );
        }

        // Resuming a successful storage-reviewed operation after a preference-save failure must
        // preserve the persistence warning instead of clearing `active.notice`.
        let unwritable_pref = dir.path().join("pref-parent-is-file");
        std::fs::write(&unwritable_pref, b"block").unwrap();
        let mut app = QuarryApp::new(Some(source), Instant::now());
        finish(&mut app);
        app.storage_preferences = StoragePreferences::load(Some(unwritable_pref.join("prefs")));
        let valid_dir = dir.path().join("valid-session-scratch");
        std::fs::create_dir(&valid_dir).unwrap();
        let ctx = egui::Context::default();
        app.begin_storage_review(PendingStorageOperation::DeleteRows(Vec::from([1..=1])));
        let review = app.active.storage_review.as_mut().unwrap();
        review.path_input = valid_dir.display().to_string();
        review.checked_path = valid_dir.clone();
        review.result = Some(Ok((
            StorageSpace {
                directory: valid_dir.clone(),
                required_bytes: 1,
                available_bytes: 1024 * 1024,
            },
            1,
        )));
        app.accept_storage_review(&ctx, true);
        assert!(
            app.active
                .notice
                .as_ref()
                .is_some_and(|m| m.text.contains("could not save this preference")),
            "expected persistence warning to survive successful resumed operation, got {:?}",
            app.active.notice.as_ref().map(|m| &m.text)
        );
    }
}
