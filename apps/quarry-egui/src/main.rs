use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use std::fs::{File, OpenOptions as FsOpenOptions};
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "macos")]
use std::sync::{
    Mutex, OnceLock,
    mpsc::{self, Receiver, Sender},
};

use eframe::egui::{
    self, Align, AtomExt, Color32, FontFamily, FontId, Layout, RichText, TextStyle,
};
use egui_extras::{Column, TableBuilder};
#[cfg(target_os = "macos")]
use objc2::runtime::{AnyObject, Imp, Sel};
#[cfg(target_os = "macos")]
use objc2::{ffi, sel};
#[cfg(target_os = "macos")]
use objc2_app_kit::NSApplication;
#[cfg(target_os = "macos")]
use objc2_foundation::{MainThreadMarker, NSArray, NSURL};
#[cfg(test)]
use quarry_core::SearchProgress;
use quarry_core::{
    CaseSensitivity, ColumnTransformation, FilterExportJob, FilterExportOutcome,
    FilterExportProgress, FilterIndex, FilterJob, FilterMatch, FilterOperator, FilterPredicate,
    FilterProgress, FilterQuery, FilterReadJob, FilterReadOutcome, HeaderMode, IndexConfig,
    IndexJob, IndexProgress, LiteralReplacement, MAX_TRANSFORMATION_COLUMNS, OpenOptions,
    QuarryError, ReplaceAllJob, ReplaceAllOutcome, Row, SaveAsJob, SaveAsOutcome, SearchJob,
    SearchMatch, SearchOutcome, SearchPosition, Session, SortDirection, SortJob, SortOutcome,
    SortSpec, SplitAnalysisJob, SplitAnalysisOutcome, StructuralIndex,
    estimate_sort_temporary_bytes,
};
use tempfile::TempDir;

const BOOTSTRAP_ROWS: usize = 40;
const OVERSCAN_ROWS: usize = 16;
const ROW_HEIGHT: f32 = 17.0;
const COLUMN_RULER_HEIGHT: f32 = 22.0;
const HEADER_HEIGHT: f32 = COLUMN_RULER_HEIGHT + ROW_HEIGHT;
const ROW_NUMBER_WIDTH: f32 = 74.0;
const MAX_RENDERED_COLUMNS: usize = 64;
const SCROLLBAR_WIDTH: f32 = 18.0;
const MIN_THUMB_HEIGHT: f32 = 24.0;
const MAX_COPY_BYTES: usize = 64 * 1024 * 1024;
const APP_ID: &str = "io.github.danchamorro.quarry";
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const TOOLBAR_HEIGHT: f32 = 38.0;
const STATUS_BAR_HEIGHT: f32 = 32.0;
const STATUS_JOB_WIDTH: f32 = 360.0;
const STATUS_CANCEL_WIDTH: f32 = 112.0;
const DOCUMENT_MENU_WIDTH: f32 = 220.0;
const FORMAT_MENU_WIDTH: f32 = 190.0;
const COMPACT_DOCUMENT_MENU_WIDTH: f32 = 132.0;
const COMPACT_FORMAT_MENU_WIDTH: f32 = 90.0;
const TOOLBAR_JUMP_WIDTH: f32 = 52.0;
const TOOLBAR_FILTER_WIDTH: f32 = 78.0;
const JUMP_INPUT_ID: &str = "quarry-jump-input";
const FIND_INPUT_ID: &str = "quarry-find-input";
const REPLACE_INPUT_ID: &str = "quarry-replace-input";
const COLUMN_SEARCH_INPUT_ID: &str = "quarry-column-search-input";
const FILTER_COLUMN_INPUT_ID: &str = "quarry-filter-column-input";
const FILTER_VALUE_INPUT_ID: &str = "quarry-filter-value-input";
const STRUCTURAL_SEPARATOR_INPUT_ID: &str = "quarry-structural-separator-input";
const STRUCTURAL_POSITION_INPUT_ID: &str = "quarry-structural-position-input";
const SOURCE_CHANGED_NOTICE: &str =
    "The source file changed outside Quarry. Discard changes and reopen it.";
const QUARRY_YELLOW: Color32 = Color32::from_rgb(233, 196, 106);
const QUARRY_YELLOW_TEXT: Color32 = Color32::from_rgb(122, 88, 20);
const QUARRY_SELECTED_TEXT: Color32 = Color32::from_rgb(47, 38, 18);
const ERROR_FILL: Color32 = Color32::from_rgb(250, 232, 229);
const ERROR_TEXT: Color32 = Color32::from_rgb(171, 65, 53);
const WARNING_FILL: Color32 = Color32::from_rgb(250, 242, 214);

#[cfg(target_os = "macos")]
#[derive(Clone)]
struct OpenDocumentTarget {
    sender: Sender<PathBuf>,
    context: Option<egui::Context>,
}

#[cfg(target_os = "macos")]
static OPEN_DOCUMENT_TARGET: OnceLock<Mutex<Option<OpenDocumentTarget>>> = OnceLock::new();

#[cfg(target_os = "macos")]
fn file_url_path(url: &NSURL) -> Option<PathBuf> {
    if url.isFileURL() {
        url.to_file_path()
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C-unwind" fn application_open_urls(
    _delegate: &AnyObject,
    _selector: Sel,
    _application: &NSApplication,
    urls: &NSArray<NSURL>,
) {
    let target = OPEN_DOCUMENT_TARGET
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("open-document target lock should not be poisoned")
        .clone();
    let Some(target) = target else {
        return;
    };
    for index in 0..urls.count() {
        let url = urls.objectAtIndex(index);
        if let Some(path) = file_url_path(&url) {
            let _ = target.sender.send(path);
        }
    }
    if let Some(context) = target.context {
        context.request_repaint();
    }
}

#[cfg(target_os = "macos")]
fn install_open_document_handler() -> Receiver<PathBuf> {
    let (sender, receiver) = mpsc::channel();
    *OPEN_DOCUMENT_TARGET
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("open-document target lock should not be poisoned") = Some(OpenDocumentTarget {
        sender,
        context: None,
    });

    let mtm = MainThreadMarker::new().expect("macOS document handling runs on the main thread");
    let application = NSApplication::sharedApplication(mtm);
    let delegate = application
        .delegate()
        .expect("winit installed an app delegate");
    let delegate_object: &AnyObject = AsRef::<AnyObject>::as_ref(&*delegate);
    let class = delegate_object.class();
    let selector = sel!(application:openURLs:);
    if class.instance_method(selector).is_none() {
        let implementation: Imp = unsafe {
            std::mem::transmute(
                application_open_urls
                    as unsafe extern "C-unwind" fn(
                        &AnyObject,
                        Sel,
                        &NSApplication,
                        &NSArray<NSURL>,
                    ),
            )
        };
        let added = unsafe {
            ffi::class_addMethod(
                class as *const _ as *mut _,
                selector,
                implementation,
                c"v@:@@".as_ptr(),
            )
        };
        assert!(
            added.as_bool(),
            "failed to install macOS open-document handler"
        );
        application.setDelegate(None);
        application.setDelegate(Some(&delegate));
    }
    receiver
}

#[cfg(target_os = "macos")]
fn attach_open_document_context(context: &egui::Context) {
    let mut target = OPEN_DOCUMENT_TARGET
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("open-document target lock should not be poisoned");
    if let Some(target) = target.as_mut() {
        target.context = Some(context.clone());
    }
}

#[cfg(target_os = "macos")]
fn acquire_install_lock_at(path: &Path) -> std::io::Result<File> {
    let file = FsOpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    // SAFETY: flock only reads the valid descriptor owned by `file`.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
        Ok(file)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn acquire_install_lock() -> std::io::Result<File> {
    // SAFETY: geteuid has no preconditions and does not dereference memory.
    let user_id = unsafe { libc::geteuid() };
    acquire_install_lock_at(Path::new(&format!(
        "/private/tmp/{APP_ID}.{user_id}.install.lock"
    )))
}

fn main() -> eframe::Result<()> {
    #[cfg(target_os = "macos")]
    let _install_lock = match acquire_install_lock() {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("Quarry cannot start while an update is in progress: {error}");
            return Ok(());
        }
    };

    let started = Instant::now();
    let initial_path = std::env::args_os().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(APP_ID)
            .with_inner_size([1280.0, 780.0])
            .with_min_inner_size([860.0, 540.0]),
        centered: true,
        ..Default::default()
    };

    #[cfg(target_os = "macos")]
    {
        let event_loop =
            winit::event_loop::EventLoop::<eframe::UserEvent>::with_user_event().build()?;
        let open_document_receiver = install_open_document_handler();
        let mut app = eframe::create_native(
            "Quarry — Viewer Alpha",
            options,
            Box::new(move |creation| {
                configure_style(&creation.egui_ctx);
                attach_open_document_context(&creation.egui_ctx);
                let mut app = QuarryApp::new(initial_path, started);
                app.open_document_receiver = Some(open_document_receiver);
                Ok(Box::new(app))
            }),
            &event_loop,
        );
        event_loop.run_app(&mut app)?;
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    eframe::run_native(
        "Quarry — Viewer Alpha",
        options,
        Box::new(move |creation| {
            configure_style(&creation.egui_ctx);
            Ok(Box::new(QuarryApp::new(initial_path, started)))
        }),
    )
}

struct QuarryApp {
    jump_input: String,
    find_input: String,
    replace_input: String,
    find_match_case: bool,
    find_bar_open: bool,
    replace_expanded: bool,
    filter_match_case: bool,
    sort_match_case: bool,
    column_search_input: String,
    columns_open: bool,
    structural_dialog: Option<StructuralDialog>,
    filter_rules: Vec<FilterRuleDraft>,
    filters_open: bool,
    delimiter_mode: DelimiterMode,
    header_mode: HeaderMode,
    format_draft: Option<(DelimiterMode, HeaderMode)>,
    document: Option<Document>,
    notice: Option<AppMessage>,
    footer_status: Option<AppMessage>,
    close_confirmation_open: bool,
    close_after_save: bool,
    started: Instant,
    logged_first_update: bool,
    #[cfg(target_os = "macos")]
    open_document_receiver: Option<Receiver<PathBuf>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageSeverity {
    Error,
    Warning,
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppMessage {
    text: String,
    severity: MessageSeverity,
}

impl AppMessage {
    fn error(text: impl Into<String>) -> Self {
        Self::new(text, MessageSeverity::Error)
    }

    fn warning(text: impl Into<String>) -> Self {
        Self::new(text, MessageSeverity::Warning)
    }

    fn status(text: impl Into<String>) -> Self {
        Self::new(text, MessageSeverity::Status)
    }

    fn new(text: impl Into<String>, severity: MessageSeverity) -> Self {
        Self {
            text: text.into(),
            severity,
        }
    }
}

impl std::ops::Deref for AppMessage {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.text
    }
}

impl std::fmt::Display for AppMessage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.text)
    }
}

impl PartialEq<&str> for AppMessage {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FilterRuleDraft {
    column_input: String,
    operator: FilterOperator,
    value_input: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StructuralRequest {
    Split,
    Combine,
    Move,
    Sort,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GridColumnRequest {
    Dialog(StructuralDialog),
    Delete(Vec<usize>),
    DeleteRows(Vec<RangeInclusive<u64>>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StructuralDialog {
    request: StructuralRequest,
    columns: Vec<usize>,
    separator: String,
    position: String,
    sort_direction: SortDirection,
}

impl StructuralDialog {
    fn split(column: usize) -> Self {
        Self {
            request: StructuralRequest::Split,
            columns: vec![column],
            separator: String::new(),
            position: String::new(),
            sort_direction: SortDirection::Ascending,
        }
    }

    fn combine(columns: Vec<usize>) -> Self {
        Self {
            request: StructuralRequest::Combine,
            columns,
            separator: String::new(),
            position: String::new(),
            sort_direction: SortDirection::Ascending,
        }
    }

    fn move_columns(columns: Vec<usize>) -> Self {
        let position = columns
            .first()
            .map_or_else(String::new, |column| column.saturating_add(1).to_string());
        Self {
            request: StructuralRequest::Move,
            columns,
            separator: String::new(),
            position,
            sort_direction: SortDirection::Ascending,
        }
    }

    fn sort(column: usize) -> Self {
        Self {
            request: StructuralRequest::Sort,
            columns: vec![column],
            separator: String::new(),
            position: String::new(),
            sort_direction: SortDirection::Ascending,
        }
    }
}

impl Default for FilterRuleDraft {
    fn default() -> Self {
        Self {
            column_input: "1".into(),
            operator: FilterOperator::Contains,
            value_input: String::new(),
        }
    }
}

impl QuarryApp {
    fn new(initial_path: Option<PathBuf>, started: Instant) -> Self {
        let mut app = Self {
            jump_input: "1".into(),
            find_input: String::new(),
            replace_input: String::new(),
            find_match_case: false,
            find_bar_open: false,
            replace_expanded: false,
            filter_match_case: false,
            sort_match_case: false,
            column_search_input: String::new(),
            columns_open: false,
            structural_dialog: None,
            filter_rules: vec![FilterRuleDraft::default()],
            filters_open: false,
            delimiter_mode: DelimiterMode::Auto,
            header_mode: HeaderMode::Auto,
            format_draft: None,
            document: None,
            notice: None,
            footer_status: None,
            close_confirmation_open: false,
            close_after_save: false,
            started,
            logged_first_update: false,
            #[cfg(target_os = "macos")]
            open_document_receiver: None,
        };
        if let Some(path) = initial_path {
            app.open_path_and_report(path);
        }
        app
    }

    #[cfg(test)]
    fn open_options(&self) -> OpenOptions {
        OpenOptions {
            delimiter: self.delimiter_mode.delimiter(),
            header_mode: self.header_mode,
            ..OpenOptions::default()
        }
    }

    #[cfg(test)]
    fn open_path(&mut self, path: PathBuf) -> Result<(), String> {
        self.open_path_with_options(path, self.open_options())
            .map_err(|message| message.text)
    }

    fn open_new_path(&mut self, path: PathBuf) -> Result<(), AppMessage> {
        self.open_path_with_options(path, OpenOptions::default())?;
        self.delimiter_mode = DelimiterMode::Auto;
        self.header_mode = HeaderMode::Auto;
        Ok(())
    }

    fn open_path_with_options(
        &mut self,
        path: PathBuf,
        options: OpenOptions,
    ) -> Result<(), AppMessage> {
        if let Some(document) = self.document.as_mut() {
            if document.save_job.is_some() {
                return Err(AppMessage::warning(
                    "Cancel the active save before opening another file.",
                ));
            }
            if document.structural_job.is_some() {
                return Err(AppMessage::warning(
                    "Cancel the active change before opening another file.",
                ));
            }
            document.commit_edits();
            if document.is_dirty() {
                return Err(AppMessage::warning(
                    "Discard or save your changes before opening another file.",
                ));
            }
        }
        if self
            .document
            .as_ref()
            .is_some_and(|document| document.export_job.is_some())
        {
            return Err(AppMessage::warning(
                "Cancel the active export and wait for it to finish before opening another file.",
            ));
        }
        self.replace_document_with_options(path, options)
    }

    fn replace_document_with_options(
        &mut self,
        path: PathBuf,
        options: OpenOptions,
    ) -> Result<(), AppMessage> {
        let mut document = Document::prepare(&path, options).map_err(AppMessage::error)?;
        document.start_indexing().map_err(AppMessage::error)?;
        if let Some(current) = self.document.as_mut() {
            current.shutdown();
        }
        self.jump_input = "1".into();
        self.column_search_input.clear();
        self.columns_open = false;
        self.find_bar_open = false;
        self.replace_expanded = false;
        self.structural_dialog = None;
        self.filter_rules = vec![FilterRuleDraft::default()];
        self.filters_open = false;
        self.format_draft = None;
        self.document = Some(document);
        self.footer_status = None;
        Ok(())
    }

    fn open_path_and_report(&mut self, path: PathBuf) {
        let result = self.open_new_path(path);
        self.notice = result.err();
    }

    fn open_picker_result(&mut self, path: Option<PathBuf>) {
        if let Some(path) = path {
            self.open_path_and_report(path);
        }
    }

    fn choose_file(&mut self) {
        if let Some(document) = self.document.as_mut() {
            if document.save_job.is_some() {
                self.notice = Some(AppMessage::warning(
                    "Cancel the active save before opening another file.",
                ));
                return;
            }
            document.commit_edits();
            if document.is_dirty() {
                self.notice = Some(AppMessage::warning(
                    "Discard or save your changes before opening another file.",
                ));
                return;
            }
        }
        let path = rfd::FileDialog::new()
            .set_title("Open a delimited file")
            .pick_file();
        self.open_picker_result(path);
    }

    fn choose_filtered_export(&mut self) {
        if let Some(document) = self.document.as_mut() {
            if document.save_job.is_some() {
                self.notice = Some(AppMessage::warning(
                    "Cancel the active save before exporting filtered rows.",
                ));
                return;
            }
            document.commit_edits();
            if document.is_dirty() {
                self.notice = Some(AppMessage::warning(
                    "Save or discard your changes before exporting filtered rows.",
                ));
                return;
            }
        }
        let Some(source) = self
            .document
            .as_ref()
            .map(|document| document.logical_path.clone())
        else {
            self.notice = Some(AppMessage::warning(
                "Open and filter a file before exporting.",
            ));
            return;
        };
        let mut dialog = rfd::FileDialog::new()
            .set_title("Export filtered rows")
            .set_file_name(filtered_export_file_name(&source));
        if let Some(parent) = source.parent() {
            dialog = dialog.set_directory(parent);
        }
        self.export_picker_result(dialog.save_file());
    }

    fn choose_save_as(&mut self) -> bool {
        let Some(document) = self.document.as_mut() else {
            self.notice = Some(AppMessage::warning("Open a file before using Save As."));
            return false;
        };
        document.commit_edits();
        if !document.is_save_ready() {
            self.notice = Some(AppMessage::warning(if document.save_job.is_some() {
                "A save operation is already running."
            } else if document.export_job.is_some() {
                "Cancel the active export before using Save As."
            } else if document.source_changed {
                SOURCE_CHANGED_NOTICE
            } else {
                "Make a change before using Save As."
            }));
            return false;
        }
        let source = document.logical_path.clone();
        let mut dialog = rfd::FileDialog::new()
            .set_title("Save edited file as")
            .set_file_name(save_as_file_name(&source));
        if let Some(parent) = source.parent() {
            dialog = dialog.set_directory(parent);
        }
        self.save_as_picker_result(dialog.save_file())
    }

    fn save_current(&mut self) -> bool {
        let result = self
            .document
            .as_mut()
            .ok_or_else(|| AppMessage::warning("Open a file before saving."))
            .and_then(Document::start_save);
        self.notice = result.err();
        if self.notice.is_none() {
            self.footer_status = None;
        }
        self.notice.is_none()
    }

    fn save_as_picker_result(&mut self, destination: Option<PathBuf>) -> bool {
        let Some(destination) = destination else {
            return false;
        };
        let result = self
            .document
            .as_mut()
            .ok_or_else(|| AppMessage::warning("Open a file before using Save As."))
            .and_then(|document| document.start_save_as(destination));
        self.notice = result.err();
        if self.notice.is_none() {
            self.footer_status = None;
        }
        self.notice.is_none()
    }

    fn export_picker_result(&mut self, destination: Option<PathBuf>) {
        let Some(destination) = destination else {
            return;
        };
        let result = self
            .document
            .as_mut()
            .ok_or_else(|| AppMessage::warning("Open and filter a file before exporting."))
            .and_then(|document| document.start_filtered_export(destination));
        self.notice = result.err();
        if self.notice.is_none() {
            self.footer_status = None;
        }
    }

    fn reopen_document(&mut self, delimiter_mode: DelimiterMode, header_mode: HeaderMode) {
        self.format_draft = None;
        let Some(path) = self
            .document
            .as_ref()
            .map(|document| document.logical_path.clone())
        else {
            self.notice = Some(AppMessage::warning("Open a file first."));
            return;
        };
        let result = self.open_path_with_options(
            path,
            OpenOptions {
                delimiter: delimiter_mode.delimiter(),
                header_mode,
                ..OpenOptions::default()
            },
        );
        if result.is_ok() {
            self.delimiter_mode = delimiter_mode;
            self.header_mode = header_mode;
        }
        self.notice = result.err();
    }

    fn reload_document(&mut self) {
        let Some((path, options)) = self.document.as_ref().map(|document| {
            (
                document.logical_path.clone(),
                document.current_open_options(),
            )
        }) else {
            self.notice = Some(AppMessage::warning("Open a file first."));
            return;
        };
        self.notice = self.open_path_with_options(path, options).err();
    }

    fn handle_dropped_paths(&mut self, dropped: Vec<Option<PathBuf>>) {
        let count = dropped.len();
        let Some(path) = dropped.into_iter().flatten().next() else {
            self.notice = Some(AppMessage::warning(format!(
                "Ignored {count} dropped item(s); Quarry can only open a local file."
            )));
            return;
        };
        let ignored = count.saturating_sub(1);
        let result = self.open_new_path(path);
        self.notice = match (result, ignored) {
            (Ok(()), 0) => None,
            (Ok(()), ignored) => Some(AppMessage::warning(format!(
                "Opened one file and ignored {ignored} additional dropped item(s)."
            ))),
            (Err(error), 0) => Some(error),
            (Err(mut error), ignored) => {
                error.text = format!("{error} Ignored {ignored} additional dropped item(s).");
                Some(error)
            }
        };
    }

    #[cfg(target_os = "macos")]
    fn poll_open_documents(&mut self) {
        let paths = self
            .open_document_receiver
            .as_ref()
            .map(|receiver| receiver.try_iter().map(Some).collect::<Vec<_>>())
            .unwrap_or_default();
        if !paths.is_empty() {
            self.handle_dropped_paths(paths);
        }
    }

    fn apply(&mut self, ctx: &egui::Context, action: Action) {
        if let Some(document) = self.document.as_mut() {
            document.commit_edits();
        }
        match action {
            Action::Choose => return self.choose_file(),
            Action::ReopenWithFormat(delimiter, header) => {
                return self.reopen_document(delimiter, header);
            }
            Action::ReloadFromDisk => return self.reload_document(),
            Action::ChooseSaveAs => {
                self.choose_save_as();
                return;
            }
            Action::Save => {
                self.save_current();
                return;
            }
            Action::ChooseFilteredExport => return self.choose_filtered_export(),
            Action::DiscardChanges => {
                if self
                    .document
                    .as_ref()
                    .is_some_and(|document| document.structural_job.is_some())
                {
                    self.notice = Some(AppMessage::warning(
                        "Cancel the active change before discarding changes.",
                    ));
                    return;
                }
                let structural = self
                    .document
                    .as_ref()
                    .is_some_and(Document::has_structural_edits);
                if structural {
                    let document = self
                        .document
                        .as_ref()
                        .expect("structural changes require an open document");
                    let path = document.logical_path.clone();
                    let options = document.current_open_options();
                    self.notice = self.replace_document_with_options(path, options).err();
                    return;
                }
                if let Some(document) = self.document.as_mut() {
                    if document.save_job.is_some() {
                        self.notice = Some(AppMessage::warning(
                            "Wait for the save to finish before discarding changes.",
                        ));
                        return;
                    }
                    document.discard_edits();
                }
                self.notice = None;
                return;
            }
            Action::OpenColumns => {
                ctx.data_mut(|data| {
                    data.remove::<usize>(egui::Id::new("quarry-selected-managed-column"));
                });
                self.columns_open = true;
                return;
            }
            Action::UndoStructuralEdit => {
                self.notice = self.swap_structural_history(false).err();
                return;
            }
            Action::RedoStructuralEdit => {
                self.notice = self.swap_structural_history(true).err();
                return;
            }
            Action::OpenFilters => {
                self.filters_open = true;
                return;
            }
            _ => {}
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        if document.source_changed {
            self.notice = Some(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            return;
        }
        let result = match action {
            Action::Choose
            | Action::ReopenWithFormat(_, _)
            | Action::ReloadFromDisk
            | Action::Save
            | Action::ChooseSaveAs
            | Action::ChooseFilteredExport
            | Action::DiscardChanges
            | Action::OpenColumns
            | Action::UndoStructuralEdit
            | Action::RedoStructuralEdit
            | Action::OpenFilters => {
                unreachable!()
            }
            Action::PageUp => document.page(-1).map_err(AppMessage::error),
            Action::PageDown => document.page(1).map_err(AppMessage::error),
            Action::AutoFitColumns => {
                document.auto_fit_columns = true;
                Ok(())
            }
            Action::Jump => parse_data_row(&self.jump_input, document.data_start)
                .map_err(AppMessage::warning)
                .and_then(|start| document.navigate(start).map_err(AppMessage::error)),
            Action::FindPrevious => document
                .start_find_previous_with_case(
                    self.find_input.as_bytes(),
                    case_sensitivity(self.find_match_case),
                )
                .map_err(AppMessage::warning),
            Action::FindNext => document
                .start_find_next_with_case(
                    self.find_input.as_bytes(),
                    case_sensitivity(self.find_match_case),
                )
                .map_err(AppMessage::warning),
            Action::ReplaceCurrent => document
                .replace_current_match_with_case(
                    self.find_input.as_bytes(),
                    self.replace_input.as_bytes(),
                    case_sensitivity(self.find_match_case),
                )
                .map_err(AppMessage::warning),
            Action::ReplaceAll => document.start_replace_all_with_case(
                self.find_input.as_bytes(),
                self.replace_input.as_bytes(),
                case_sensitivity(self.find_match_case),
            ),
            Action::CancelSearch => {
                document.cancel_search();
                Ok(())
            }
            Action::ApplyFilter => {
                let predicates = self
                    .filter_rules
                    .iter()
                    .enumerate()
                    .map(|(index, rule)| {
                        parse_file_column(&rule.column_input, document.total_columns)
                            .map_err(|error| format!("Rule {}: {error}", index + 1))
                            .map(|column| FilterPredicate {
                                column,
                                operator: rule.operator,
                                value: rule.value_input.as_bytes().to_vec(),
                            })
                    })
                    .collect::<Result<Vec<_>, _>>();
                predicates
                    .map_err(AppMessage::warning)
                    .and_then(|predicates| {
                        document
                            .start_filter(FilterQuery {
                                predicates,
                                case_sensitivity: case_sensitivity(self.filter_match_case),
                            })
                            .map_err(AppMessage::warning)
                    })
            }
            Action::CancelFilter => {
                document.cancel_filter();
                Ok(())
            }
            Action::ClearFilter => document.clear_filter().map_err(AppMessage::warning),
            Action::CancelExport => {
                document.cancel_filtered_export();
                Ok(())
            }
            Action::CancelSave => {
                document.cancel_save();
                Ok(())
            }
            Action::CancelStructuralEdit => {
                document.cancel_structural_edit();
                Ok(())
            }
            Action::Cancel => {
                document.cancel();
                Ok(())
            }
        };
        if result.is_ok()
            && matches!(
                action,
                Action::FindPrevious
                    | Action::FindNext
                    | Action::ReplaceCurrent
                    | Action::ReplaceAll
                    | Action::ApplyFilter
                    | Action::ClearFilter
                    | Action::CancelSearch
                    | Action::CancelFilter
                    | Action::CancelExport
                    | Action::CancelSave
                    | Action::CancelStructuralEdit
                    | Action::Cancel
            )
        {
            self.footer_status = if matches!(action, Action::FindPrevious | Action::FindNext)
                && document.search_job.is_none()
            {
                document.search_status.clone().map(AppMessage::status)
            } else {
                None
            };
        }
        self.notice = result.err();
    }

    fn apply_column_command(&mut self, ctx: &egui::Context, command: ColumnCommand) {
        if command == ColumnCommand::AutoFit {
            self.apply(ctx, Action::AutoFitColumns);
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        let result = match command {
            ColumnCommand::SetShown { column, shown } => document.set_column_shown(column, shown),
            ColumnCommand::Move { column, position } => document.move_column(column, position),
            ColumnCommand::Reset => {
                document.reset_columns();
                Ok(())
            }
            ColumnCommand::AutoFit => unreachable!("auto-fit routes through the app action"),
        };
        self.notice = result.err().map(AppMessage::warning);
    }

    fn open_structural_dialog(&mut self, dialog: StructuralDialog) {
        let result = self
            .document
            .as_ref()
            .ok_or_else(|| "Open a file before editing columns.".to_owned())
            .and_then(|document| {
                document
                    .structural_edit_disabled_reason()
                    .map_or(Ok(()), |reason| Err(reason.to_owned()))
            });
        match result {
            Ok(()) => {
                self.structural_dialog = Some(dialog);
                self.notice = None;
            }
            Err(error) => self.notice = Some(AppMessage::warning(error)),
        }
    }

    fn apply_structural_dialog_action(&mut self, action: StructuralDialogAction) {
        if action == StructuralDialogAction::Cancel {
            self.structural_dialog = None;
            return;
        }
        let Some(dialog) = self.structural_dialog.clone() else {
            return;
        };
        let result =
            self.document
                .as_mut()
                .ok_or_else(|| AppMessage::warning("Open a file before editing columns."))
                .and_then(|document| match dialog.request {
                    StructuralRequest::Split => document
                        .start_split(dialog.columns[0], dialog.separator.as_bytes().to_vec()),
                    StructuralRequest::Combine => {
                        document.start_combine(dialog.columns, dialog.separator.as_bytes().to_vec())
                    }
                    StructuralRequest::Move => parse_move_position(
                        &dialog.position,
                        document.total_columns,
                        dialog.columns.len(),
                    )
                    .map_err(AppMessage::warning)
                    .and_then(|position| document.start_move_columns(dialog.columns, position)),
                    StructuralRequest::Sort => document.start_sort_rows_with_case(
                        dialog.columns[0],
                        dialog.sort_direction,
                        case_sensitivity(self.sort_match_case),
                    ),
                });
        match result {
            Ok(()) => {
                self.structural_dialog = None;
                self.notice = None;
                self.footer_status = self
                    .document
                    .as_ref()
                    .filter(|document| document.structural_job.is_none())
                    .and_then(|document| document.structural_status.clone())
                    .map(AppMessage::status);
            }
            Err(error) => self.notice = Some(error),
        }
    }

    fn apply_delete_columns(&mut self, columns: Vec<usize>) {
        let result = self
            .document
            .as_mut()
            .ok_or_else(|| AppMessage::warning("Open a file before editing columns."))
            .and_then(|document| document.start_delete_columns(columns));
        self.notice = result.err();
        if self.notice.is_none() {
            self.footer_status = self
                .document
                .as_ref()
                .filter(|document| document.structural_job.is_none())
                .and_then(|document| document.structural_status.clone())
                .map(AppMessage::status);
        }
    }

    fn apply_delete_rows(&mut self, rows: Vec<RangeInclusive<u64>>) {
        let result = self
            .document
            .as_mut()
            .ok_or_else(|| AppMessage::warning("Open a file before deleting rows."))
            .and_then(|document| document.start_delete_rows(rows));
        self.notice = result.err();
        if self.notice.is_none() {
            self.footer_status = self
                .document
                .as_ref()
                .filter(|document| document.structural_job.is_none())
                .and_then(|document| document.structural_status.clone())
                .map(AppMessage::status);
        }
    }

    fn install_materialized_working_copy(
        &mut self,
        ready: MaterializedWorkingCopy,
    ) -> Result<(), String> {
        let Some(current) = self.document.as_ref() else {
            return Err("The document closed before the change finished.".into());
        };
        let options = current.current_open_options();
        let logical_path = current.logical_path.clone();
        let mut replacement = Document::prepare(&ready.path, options)?;
        replacement.start_indexing()?;

        let current = self
            .document
            .as_mut()
            .expect("the materialized document is still open");
        replacement.logical_path = logical_path.clone();
        replacement.original_session = current.original_session.take();
        replacement.working_copy = current.working_copy.take();
        replacement.selected_columns = ready.selected_columns;
        let first_selected = replacement.selected_columns.iter().next().copied();
        replacement.column_selection_anchor = first_selected;
        replacement.column_focus_requested = first_selected;
        if let Some(first_selected) = first_selected
            && replacement.columns.view(first_selected)
        {
            replacement.refresh_column_headers();
        }
        current.shutdown();
        self.document = Some(replacement);
        self.format_draft = None;
        self.columns_open = false;
        self.structural_dialog = None;
        self.notice = None;
        self.footer_status = Some(AppMessage::status(ready.notice));
        Ok(())
    }

    fn swap_structural_history(&mut self, redo: bool) -> Result<(), AppMessage> {
        let Some(current) = self.document.as_mut() else {
            return Err(AppMessage::warning("Open a file before undoing a change."));
        };
        current.commit_edits();
        if !redo && (!current.header_renames.is_empty() || current.has_cell_edits()) {
            return Err(AppMessage::warning(
                "Save or discard later header and cell edits before undoing the change.",
            ));
        }
        if current.save_job.is_some()
            || current.export_job.is_some()
            || current.structural_job.is_some()
        {
            return Err(AppMessage::warning(
                "Wait for the active file operation to finish.",
            ));
        }
        let state = current
            .working_copy
            .as_ref()
            .ok_or_else(|| AppMessage::warning("There is no change to undo or redo."))?;
        let target = if redo {
            state.redo.clone()
        } else {
            state.undo.clone()
        }
        .ok_or_else(|| {
            AppMessage::warning(if redo {
                "There is no change to redo."
            } else {
                "There is no change to undo."
            })
        })?;
        if !redo && target.path == current.logical_path {
            let original = current
                .original_session
                .as_ref()
                .expect("a first structural undo retains the original source guard");
            match original.ensure_source_unchanged() {
                Ok(()) => {}
                Err(QuarryError::SourceChanged) => {
                    current.invalidate_changed_source();
                    return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
                }
                Err(error) => return Err(AppMessage::error(error.to_string())),
            }
        }

        let options = current.current_open_options();
        let logical_path = current.logical_path.clone();
        let current_snapshot = WorkingCopySnapshot {
            path: current.session.path().to_path_buf(),
            overlay: StructuralOverlay {
                header_renames: current.header_renames.clone(),
                cell_edits: current.cell_edits.clone(),
            },
        };
        let mut replacement =
            Document::prepare(&target.path, options).map_err(AppMessage::error)?;
        replacement.header_renames = target.overlay.header_renames.clone();
        replacement.cell_edits = target.overlay.cell_edits.clone();
        replacement.refresh_column_headers();
        replacement.start_indexing().map_err(AppMessage::error)?;

        let current = self
            .document
            .as_mut()
            .expect("the document remains open while history is swapped");
        let mut state = current
            .working_copy
            .take()
            .expect("the history target came from working-copy state");
        if redo {
            state.redo = None;
            state.undo = Some(current_snapshot);
        } else {
            state.undo = None;
            state.redo = Some(current_snapshot);
        }
        replacement.logical_path = logical_path.clone();
        replacement.original_session = current.original_session.take();
        replacement.working_copy = Some(state);
        current.shutdown();
        self.document = Some(replacement);
        self.structural_dialog = None;
        self.columns_open = false;
        self.notice = None;
        self.footer_status = Some(AppMessage::status(if redo {
            "Change restored."
        } else {
            "Change undone."
        }));
        Ok(())
    }

    fn copy_selection(&mut self, ctx: &egui::Context) {
        let result = self
            .document
            .as_ref()
            .ok_or_else(|| "Open a file before copying.".to_owned())
            .and_then(Document::copy_selection_text);
        match result {
            Ok(text) => {
                ctx.copy_text(text);
                self.notice = None;
            }
            Err(error) => self.notice = Some(AppMessage::warning(error)),
        }
    }

    fn intercept_dirty_close(&mut self, ctx: &egui::Context) {
        if !ctx.input(|input| input.viewport().close_requested()) {
            return;
        }
        if let Some(document) = self.document.as_mut() {
            if document.save_job.is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.close_confirmation_open = false;
                self.close_after_save = true;
                return;
            }
            document.commit_edits();
            if document.is_dirty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.close_confirmation_open = true;
            }
        }
    }

    fn keep_editing(&mut self) {
        self.close_confirmation_open = false;
        self.close_after_save = false;
    }

    fn discard_and_close(&mut self, ctx: &egui::Context) {
        if let Some(mut document) = self.document.take() {
            document.shutdown();
        }
        self.close_confirmation_open = false;
        self.close_after_save = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl eframe::App for QuarryApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.logged_first_update {
            eprintln!(
                "quarry-egui first update: {:.3} ms",
                self.started.elapsed().as_secs_f64() * 1000.0
            );
            self.logged_first_update = true;
        }

        self.intercept_dirty_close(ctx);

        #[cfg(target_os = "macos")]
        self.poll_open_documents();

        let local_file_hovered = self.document.is_none()
            && ctx.input(|input| {
                input
                    .raw
                    .hovered_files
                    .iter()
                    .any(|file| file.path.is_some())
            });
        let dropped_paths = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .map(|file| file.path.clone())
                .collect::<Vec<_>>()
        });
        if !dropped_paths.is_empty() {
            self.handle_dropped_paths(dropped_paths);
        }

        if let Some(document) = &mut self.document
            && let Err(error) = document.poll()
        {
            self.footer_status = None;
            self.notice = Some(AppMessage::error(error));
        }
        if let Some(document) = &mut self.document {
            let was_active = document.search_job.is_some();
            match document.poll_search() {
                Ok(()) if was_active && document.search_job.is_none() => {
                    self.notice = None;
                    self.footer_status = document.search_status.clone().map(AppMessage::status);
                }
                Ok(()) => {}
                Err(error) => {
                    self.footer_status = None;
                    self.notice = Some(AppMessage::error(error));
                }
            }
        }
        if let Some(document) = &mut self.document {
            let was_active = document.filter_job.is_some();
            match document.poll_filter() {
                Ok(()) if was_active && document.filter_job.is_none() => {
                    self.notice = None;
                    self.footer_status = document.filter_status.clone().map(AppMessage::status);
                }
                Ok(()) => {}
                Err(error) => {
                    self.footer_status = None;
                    self.notice = Some(AppMessage::error(error));
                }
            }
        }
        if let Some(document) = &mut self.document {
            let was_active = document.export_job.is_some();
            match document.poll_filtered_export() {
                Ok(()) if was_active && document.export_job.is_none() => {
                    let status = document.export_status.clone();
                    if status
                        .as_deref()
                        .is_some_and(|status| status.starts_with("Export failed"))
                    {
                        self.footer_status = None;
                        self.notice = status.map(AppMessage::error);
                    } else {
                        self.notice = None;
                        self.footer_status = status.map(AppMessage::status);
                    }
                }
                Ok(()) => {}
                Err(error) => {
                    self.footer_status = None;
                    self.notice = Some(AppMessage::error(error));
                }
            }
        }
        let structural_was_active = self
            .document
            .as_ref()
            .is_some_and(|document| document.structural_job.is_some());
        let structural_result = self
            .document
            .as_mut()
            .map_or(Ok(None), Document::poll_structural_edit);
        match structural_result {
            Ok(Some(ready)) => {
                if let Err(error) = self.install_materialized_working_copy(ready) {
                    self.footer_status = None;
                    self.notice = Some(AppMessage::error(error));
                }
            }
            Ok(None)
                if structural_was_active
                    && self
                        .document
                        .as_ref()
                        .is_some_and(|document| document.structural_job.is_none()) =>
            {
                self.notice = None;
                self.footer_status = self
                    .document
                    .as_ref()
                    .and_then(|document| document.structural_status.clone())
                    .map(AppMessage::status);
            }
            Ok(None) => {}
            Err(error) => {
                self.footer_status = None;
                self.notice = Some(error);
            }
        }
        let save_was_active = self
            .document
            .as_ref()
            .is_some_and(|document| document.save_job.is_some());
        let save_result = self.document.as_mut().map_or(Ok(None), Document::poll_save);
        match save_result {
            Ok(Some((destination, in_place))) => {
                let dialect = self
                    .document
                    .as_ref()
                    .expect("saved document is still open")
                    .session
                    .dialect;
                let options = OpenOptions {
                    delimiter: Some(dialect.delimiter),
                    header_mode: if dialect.has_header {
                        HeaderMode::FirstRow
                    } else {
                        HeaderMode::NoHeader
                    },
                    ..OpenOptions::default()
                };
                match self.replace_document_with_options(destination.clone(), options) {
                    Ok(()) => {
                        self.notice = None;
                        self.footer_status = Some(AppMessage::status(if in_place {
                            format!("Saved {}.", destination.display())
                        } else {
                            format!("Saved as {}.", destination.display())
                        }));
                        if self.close_after_save {
                            self.close_after_save = false;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    }
                    Err(error) if in_place => {
                        if let Some(mut document) = self.document.take() {
                            document.shutdown();
                        }
                        self.find_bar_open = false;
                        self.replace_expanded = false;
                        surrender_find_controls_focus(ctx);
                        self.footer_status = None;
                        self.notice = Some(AppMessage::error(format!(
                            "Saved {} but could not reload it: {error}. Reopen the file to continue.",
                            destination.display()
                        )));
                        if self.close_after_save {
                            self.close_after_save = false;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    }
                    Err(error) => {
                        if let Some(document) = self.document.as_mut() {
                            document.save_status = None;
                        }
                        self.footer_status = None;
                        self.notice = Some(AppMessage::error(format!(
                            "Saved {} but could not open it: {error}",
                            destination.display()
                        )));
                        if self.close_after_save {
                            self.close_after_save = false;
                            self.close_confirmation_open = true;
                        }
                    }
                }
            }
            Ok(None) => {
                if save_was_active
                    && self
                        .document
                        .as_ref()
                        .is_some_and(|document| document.save_job.is_none())
                {
                    self.notice = None;
                    self.footer_status = self
                        .document
                        .as_ref()
                        .and_then(|document| document.save_status.clone())
                        .map(AppMessage::status);
                }
                if self.close_after_save
                    && self
                        .document
                        .as_ref()
                        .is_some_and(|document| document.save_job.is_none())
                {
                    self.close_after_save = false;
                    self.close_confirmation_open = true;
                }
            }
            Err(error) => {
                self.footer_status = None;
                self.notice = Some(error);
                if self.close_after_save {
                    self.close_after_save = false;
                    self.close_confirmation_open = true;
                }
            }
        }

        let find_available = self
            .document
            .as_ref()
            .is_some_and(|document| !document.filter_active());
        let mut focus_find = false;
        if find_available
            && !self.close_confirmation_open
            && self.structural_dialog.is_none()
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::F))
        {
            self.find_bar_open = true;
            focus_find = true;
        }
        if !self.find_bar_open {
            surrender_find_controls_focus(ctx);
        }

        let mut action = None;
        egui::TopBottomPanel::top("quarry-toolbar")
            .exact_height(TOOLBAR_HEIGHT)
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(230, 235, 238))
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .stroke(egui::Stroke::new(
                        1.0_f32,
                        Color32::from_rgb(200, 209, 213),
                    )),
            )
            .show(ctx, |ui| {
                let document = self.document.as_ref();
                let document_open = document.is_some();
                let filter_active = document.is_some_and(Document::filter_active);
                let compact = ui.available_width() < 1_000.0;
                ui.spacing_mut().item_spacing.x = if compact { 4.0 } else { 6.0 };
                ui.spacing_mut().button_padding.x = if compact { 5.0 } else { 8.0 };
                ui.horizontal(|ui| {
                    if let Some(document_action) = document_menu(
                        ui,
                        document,
                        if compact {
                            COMPACT_DOCUMENT_MENU_WIDTH
                        } else {
                            DOCUMENT_MENU_WIDTH
                        },
                    ) {
                        action = Some(document_action);
                    }
                    if let Some(format_action) = format_menu(
                        ui,
                        document,
                        self.delimiter_mode,
                        self.header_mode,
                        &mut self.format_draft,
                        compact,
                        if compact {
                            COMPACT_FORMAT_MENU_WIDTH
                        } else {
                            FORMAT_MENU_WIDTH
                        },
                    ) {
                        action = Some(format_action);
                    }
                    let label = ui.label("Row");
                    let jump_enabled = document_open && !filter_active;
                    let jump = ui
                        .add_enabled(
                            jump_enabled,
                            egui::TextEdit::singleline(&mut self.jump_input)
                                .id(egui::Id::new(JUMP_INPUT_ID))
                                .horizontal_align(Align::RIGHT)
                                .desired_width(TOOLBAR_JUMP_WIDTH),
                        )
                        .labelled_by(label.id);
                    if ui
                        .add_enabled(jump_enabled, egui::Button::new("Go"))
                        .clicked()
                        || (jump_enabled
                            && jump.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                    {
                        action = Some(Action::Jump);
                    }
                    if let Some(page_action) = page_controls(ui, document_open) {
                        action = Some(page_action);
                    }

                    let can_undo = document.is_some_and(Document::can_undo_structural);
                    let undo = ui
                        .add_enabled(can_undo, egui::Button::new("Undo"))
                        .on_disabled_hover_text(
                            "Save or discard later cell and header edits before undoing the change.",
                        );
                    undo.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            can_undo,
                            "Undo Change",
                        )
                    });
                    if undo.clicked() {
                        action = Some(Action::UndoStructuralEdit);
                    }

                    let can_redo = document.is_some_and(Document::can_redo_structural);
                    let redo = ui.add_enabled(can_redo, egui::Button::new("Redo"));
                    redo.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            can_redo,
                            "Redo Change",
                        )
                    });
                    if redo.clicked() {
                        action = Some(Action::RedoStructuralEdit);
                    }

                    if ui
                        .add_enabled(document_open, egui::Button::new("Columns…"))
                        .clicked()
                    {
                        action = Some(Action::OpenColumns);
                    }
                    let filter_label = filter_button_label(
                        document.and_then(|document| document.filter_query.as_ref()),
                    );
                    let filters = ui
                        .add_enabled_ui(document_open, |ui| {
                            ui.add_sized(
                                [TOOLBAR_FILTER_WIDTH, 24.0],
                                egui::Button::new(filter_label).truncate(),
                            )
                        })
                        .inner;
                    if filters.clicked() {
                        action = Some(Action::OpenFilters);
                    }

                    let find_disabled_reason = if document_open {
                        "Clear the filter before using Find."
                    } else {
                        "Open a file first."
                    };
                    let find = ui
                        .add_enabled(jump_enabled, egui::Button::new("Find"))
                        .on_disabled_hover_text(find_disabled_reason);
                    if !jump_enabled {
                        let _ = ui.ctx().accesskit_node_builder(find.id, |node| {
                            node.set_description(find_disabled_reason);
                        });
                    }
                    if find.clicked() {
                        self.find_bar_open = true;
                        focus_find = true;
                    }
                });
            });

        if let Some(notice) = self.notice.as_ref() {
            let fill = match notice.severity {
                MessageSeverity::Error => ERROR_FILL,
                MessageSeverity::Warning => WARNING_FILL,
                MessageSeverity::Status => unreachable!("status messages belong in the footer"),
            };
            let mut dismiss = false;
            egui::TopBottomPanel::top("quarry-notice")
                .frame(panel_frame(fill).inner_margin(egui::Margin::symmetric(10, 5)))
                .show(ctx, |ui| {
                    dismiss = notice_strip(ui, notice);
                });
            if dismiss {
                self.notice = None;
            }
        }

        if self.find_bar_open {
            let mut close_find = false;
            let find_escape_allowed = !self.columns_open
                && !self.filters_open
                && !self.close_confirmation_open
                && self.structural_dialog.is_none()
                && self.document.as_ref().is_none_or(|document| {
                    document.cell_edit.is_none() && document.header_edit.is_none()
                });
            egui::TopBottomPanel::top("quarry-find")
                .frame(
                    egui::Frame::new()
                        .fill(Color32::from_rgb(238, 242, 244))
                        .inner_margin(egui::Margin::symmetric(10, 5))
                        .stroke(egui::Stroke::new(1.0_f32, Color32::from_rgb(200, 209, 213))),
                )
                .show(ctx, |ui| {
                    if focus_find {
                        ui.memory_mut(|memory| {
                            memory.request_focus(egui::Id::new(FIND_INPUT_ID));
                        });
                    }
                    if let Some(document) = self.document.as_ref() {
                        let can_find_previous = document.can_find_previous_with_case(
                            self.find_input.as_bytes(),
                            case_sensitivity(self.find_match_case),
                        );
                        let can_replace = document.can_replace_current_with_case(
                            self.find_input.as_bytes(),
                            case_sensitivity(self.find_match_case),
                        );
                        let (search_action, close_requested) = search_controls(
                            ui,
                            &mut self.find_input,
                            &mut self.replace_input,
                            &mut self.find_match_case,
                            &mut self.replace_expanded,
                            document.is_search_ready(),
                            can_find_previous,
                            can_replace,
                            document.search_job.is_some(),
                            document.filter_active(),
                        );
                        if let Some(search_action) = search_action {
                            action = Some(search_action);
                        }
                        let escape_pressed =
                            ctx.input(|input| input.key_pressed(egui::Key::Escape));
                        close_find = close_requested && (find_escape_allowed || !escape_pressed);
                        if close_find && escape_pressed {
                            ctx.input_mut(|input| {
                                let modifiers = input.modifiers;
                                input.consume_key(modifiers, egui::Key::Escape);
                            });
                        }
                    }
                });
            if close_find {
                self.find_bar_open = false;
                surrender_find_controls_focus(ctx);
            }
        }

        egui::TopBottomPanel::bottom("quarry-status")
            .exact_height(STATUS_BAR_HEIGHT)
            .frame(
                panel_frame(Color32::from_rgb(230, 235, 238))
                    .inner_margin(egui::Margin::symmetric(14, 4)),
            )
            .show(ctx, |ui| {
                if let Some(document) = &self.document {
                    if let Some(footer_action) =
                        status_bar(ui, document, self.footer_status.as_deref())
                    {
                        action = Some(footer_action);
                    }
                } else {
                    ui.label("No file open");
                }
            });

        if action.is_none()
            && !self.close_confirmation_open
            && self.document.as_ref().is_some_and(Document::is_save_ready)
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::S))
        {
            action = Some(Action::Save);
        }
        if action.is_none() && self.document.is_some() {
            action = ctx.input(|input| {
                if input.key_pressed(egui::Key::PageDown) {
                    Some(Action::PageDown)
                } else if input.key_pressed(egui::Key::PageUp) {
                    Some(Action::PageUp)
                } else {
                    None
                }
            });
        }
        if let Some(action) = action {
            self.apply(ctx, action);
        }

        let column_command = self.document.as_ref().and_then(|document| {
            show_column_manager(
                ctx,
                &mut self.columns_open,
                &mut self.column_search_input,
                document,
            )
        });
        if let Some(command) = column_command {
            self.apply_column_command(ctx, command);
        }

        let filter_action = self.document.as_ref().and_then(|document| {
            show_filter_manager(
                ctx,
                &mut self.filters_open,
                &mut self.filter_rules,
                &mut self.filter_match_case,
                document,
            )
        });
        if let Some(action) = filter_action {
            self.apply(ctx, action);
        }

        let mut grid_error = None;
        let mut requested_column_edit = None;
        let mut empty_state_action = None;
        egui::CentralPanel::default()
            .frame(panel_frame(Color32::from_rgb(244, 247, 248)))
            .show(ctx, |ui| {
                if let Some(document) = self.document.as_mut() {
                    match show_grid_with_filter_case(
                        ui,
                        document,
                        case_sensitivity(self.filter_match_case),
                    ) {
                        Ok(request) => requested_column_edit = request,
                        Err(error) => grid_error = Some(error),
                    }
                } else {
                    empty_state_action = show_empty_state(ui, local_file_hovered);
                }
            });
        if let Some(action) = empty_state_action {
            self.apply(ctx, action);
        }
        if let Some(request) = requested_column_edit {
            match request {
                GridColumnRequest::Dialog(dialog) => self.open_structural_dialog(dialog),
                GridColumnRequest::Delete(columns) => self.apply_delete_columns(columns),
                GridColumnRequest::DeleteRows(rows) => self.apply_delete_rows(rows),
            }
        }
        let structural_dialog_action = self
            .structural_dialog
            .as_mut()
            .zip(self.document.as_ref())
            .and_then(|(dialog, document)| {
                show_structural_dialog(ctx, dialog, &mut self.sort_match_case, document)
            });
        if let Some(action) = structural_dialog_action {
            self.apply_structural_dialog_action(action);
        }
        let copy_event_targets_selection = self.document.as_ref().is_some_and(|document| {
            document.selection.is_some()
                && selection_copy_requested(
                    ctx,
                    self.filter_rules.len(),
                    document.header_edit.as_ref().map(|edit| edit.column),
                    document
                        .cell_edit
                        .as_ref()
                        .map(|edit| (edit.row, edit.column)),
                )
        });
        if copy_event_targets_selection {
            self.copy_selection(ctx);
        }
        if grid_error.is_some() {
            self.notice = grid_error.map(AppMessage::error);
        }

        if self.close_confirmation_open {
            let mut discard_and_close = false;
            let mut keep_editing = false;
            let mut save_and_close = false;
            let mut save_as_and_close = false;
            let modal =
                egui::Modal::new(egui::Id::new("quarry-close-confirmation")).show(ctx, |ui| {
                    ui.heading("Unsaved changes");
                    ui.label("This file has unsaved changes.");
                    ui.label("Save them, save a new copy, keep editing, or discard them.");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Keep Editing").clicked() {
                            keep_editing = true;
                        }
                        if ui
                            .add_enabled(
                                self.document.as_ref().is_some_and(Document::is_save_ready),
                                egui::Button::new("Save and Close"),
                            )
                            .clicked()
                        {
                            save_and_close = true;
                        }
                        if ui
                            .add_enabled(
                                self.document.as_ref().is_some_and(Document::is_save_ready),
                                egui::Button::new("Save As and Close…"),
                            )
                            .clicked()
                        {
                            save_as_and_close = true;
                        }
                        if ui
                            .add_enabled(
                                self.document
                                    .as_ref()
                                    .is_none_or(|document| document.save_job.is_none()),
                                egui::Button::new("Discard Changes and Close"),
                            )
                            .clicked()
                        {
                            discard_and_close = true;
                        }
                    });
                });
            keep_editing |= modal.should_close();
            if keep_editing {
                self.keep_editing();
            } else if save_and_close {
                self.close_confirmation_open = false;
                self.close_after_save = true;
                if !self.save_current() {
                    self.close_after_save = false;
                    self.close_confirmation_open = true;
                }
            } else if save_as_and_close {
                self.close_confirmation_open = false;
                self.close_after_save = true;
                if !self.choose_save_as() {
                    self.close_after_save = false;
                    self.close_confirmation_open = true;
                }
            } else if discard_and_close {
                self.discard_and_close(ctx);
            }
        }
        if self.document.as_ref().is_some_and(|document| {
            document.job.is_some()
                || document.search_job.is_some()
                || document.filter_job.is_some()
                || document.filter_rows_loading()
                || document.export_job.is_some()
                || document.save_job.is_some()
                || document.structural_job.is_some()
        }) {
            ctx.request_repaint_after(POLL_INTERVAL);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DelimiterMode {
    Auto,
    Comma,
    Tab,
    Pipe,
    Semicolon,
}

impl DelimiterMode {
    const ALL: [Self; 5] = [
        Self::Auto,
        Self::Comma,
        Self::Tab,
        Self::Pipe,
        Self::Semicolon,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Comma => "Comma",
            Self::Tab => "Tab",
            Self::Pipe => "Pipe",
            Self::Semicolon => "Semicolon",
        }
    }

    fn delimiter(self) -> Option<u8> {
        match self {
            Self::Auto => None,
            Self::Comma => Some(b','),
            Self::Tab => Some(b'\t'),
            Self::Pipe => Some(b'|'),
            Self::Semicolon => Some(b';'),
        }
    }
}

fn header_mode_label(mode: HeaderMode) -> &'static str {
    match mode {
        HeaderMode::Auto => "Auto",
        HeaderMode::FirstRow => "First row is header",
        HeaderMode::NoHeader => "No header",
    }
}

fn compact_header_mode_label(mode: HeaderMode) -> &'static str {
    match mode {
        HeaderMode::Auto => "Auto",
        HeaderMode::FirstRow => "Header row",
        HeaderMode::NoHeader => "No header",
    }
}

fn detected_delimiter_label(delimiter: u8) -> &'static str {
    match delimiter {
        b',' => "Comma",
        b'\t' => "Tab",
        b'|' => "Pipe",
        b';' => "Semicolon",
        _ => "Custom",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Choose,
    ReopenWithFormat(DelimiterMode, HeaderMode),
    ReloadFromDisk,
    Save,
    PageUp,
    PageDown,
    AutoFitColumns,
    OpenColumns,
    UndoStructuralEdit,
    RedoStructuralEdit,
    OpenFilters,
    ChooseSaveAs,
    CancelSave,
    CancelStructuralEdit,
    ChooseFilteredExport,
    DiscardChanges,
    Jump,
    FindPrevious,
    FindNext,
    ReplaceCurrent,
    ReplaceAll,
    CancelSearch,
    ApplyFilter,
    CancelFilter,
    ClearFilter,
    CancelExport,
    Cancel,
}

#[derive(Clone, Debug, PartialEq)]
struct ActiveJobDisplay {
    label: String,
    fraction: f32,
    animate: bool,
    cancel_action: Action,
    cancel_label: &'static str,
    cancel_enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActiveJobKind {
    Structural,
    Save,
    Export,
    Filter,
    Search,
    Index,
}

impl ActiveJobKind {
    fn cancel_action(self) -> Action {
        match self {
            Self::Structural => Action::CancelStructuralEdit,
            Self::Save => Action::CancelSave,
            Self::Export => Action::CancelExport,
            Self::Filter => Action::CancelFilter,
            Self::Search => Action::CancelSearch,
            Self::Index => Action::Cancel,
        }
    }

    fn cancel_label(self, saving_in_place: bool) -> &'static str {
        match self {
            Self::Structural => "Cancel Change",
            Self::Save if saving_in_place => "Cancel Save",
            Self::Save => "Cancel Save As",
            Self::Export => "Cancel Export",
            Self::Filter => "Cancel filter",
            Self::Search => "Cancel Search",
            Self::Index => "Cancel",
        }
    }
}

fn first_active_job(active: [bool; 6]) -> Option<ActiveJobKind> {
    active
        .into_iter()
        .zip([
            ActiveJobKind::Structural,
            ActiveJobKind::Save,
            ActiveJobKind::Export,
            ActiveJobKind::Filter,
            ActiveJobKind::Search,
            ActiveJobKind::Index,
        ])
        .find_map(|(active, kind)| active.then_some(kind))
}

fn progress_fraction(bytes_scanned: u64, total_bytes: u64, done: bool) -> f32 {
    if total_bytes == 0 {
        if done { 1.0 } else { 0.0 }
    } else {
        (bytes_scanned as f32 / total_bytes as f32).clamp(0.0, 1.0)
    }
}

fn active_job_display(document: &Document) -> Option<ActiveJobDisplay> {
    let active = first_active_job([
        document.structural_job.is_some(),
        document.save_job.is_some(),
        document.export_job.is_some(),
        document.filter_job.is_some(),
        document.search_job.is_some(),
        document.job.is_some(),
    ])?;

    if active == ActiveJobKind::Structural {
        let progress = document
            .structural_progress()
            .expect("active structural job has progress");
        return Some(ActiveJobDisplay {
            label: progress.label,
            fraction: progress.fraction,
            animate: progress.animate,
            cancel_action: active.cancel_action(),
            cancel_label: active.cancel_label(document.saving_in_place),
            cancel_enabled: !document.structural_cancel_requested,
        });
    }

    if active == ActiveJobKind::Save {
        let job = document.save_job.as_ref().expect("active save job exists");
        let progress = job.progress();
        let fraction =
            progress_fraction(progress.bytes_scanned, progress.total_bytes, progress.done);
        let operation = if document.saving_in_place {
            "Save"
        } else {
            "Save As"
        };
        let label = if document.save_cancel_requested {
            format!("Cancelling {operation} · {:.1}%", fraction * 100.0)
        } else {
            format!("Saving edited file · {:.1}%", fraction * 100.0)
        };
        return Some(ActiveJobDisplay {
            label,
            fraction,
            animate: false,
            cancel_action: active.cancel_action(),
            cancel_label: active.cancel_label(document.saving_in_place),
            cancel_enabled: !document.save_cancel_requested && !progress.done,
        });
    }

    if active == ActiveJobKind::Export {
        let job = document
            .export_job
            .as_ref()
            .expect("active export job exists");
        let progress = job.progress();
        let fraction =
            progress_fraction(progress.bytes_scanned, progress.total_bytes, progress.done);
        return Some(ActiveJobDisplay {
            label: format!(
                "{} · {:.1}%",
                if document.export_cancel_requested {
                    "Cancelling export"
                } else {
                    "Exporting"
                },
                fraction * 100.0
            ),
            fraction,
            animate: false,
            cancel_action: active.cancel_action(),
            cancel_label: active.cancel_label(document.saving_in_place),
            cancel_enabled: !document.export_cancel_requested && !progress.done,
        });
    }

    if active == ActiveJobKind::Filter {
        let job = document
            .filter_job
            .as_ref()
            .expect("active filter job exists");
        let progress = job.progress();
        let fraction = progress_fraction(progress.bytes_scanned, progress.file_size, progress.done);
        return Some(ActiveJobDisplay {
            label: format!(
                "{} · {:.1}%",
                if progress.cancelled {
                    "Cancelling filter"
                } else {
                    "Filtering"
                },
                fraction * 100.0
            ),
            fraction,
            animate: false,
            cancel_action: active.cancel_action(),
            cancel_label: active.cancel_label(document.saving_in_place),
            cancel_enabled: !progress.cancelled && !progress.done,
        });
    }

    if active == ActiveJobKind::Search {
        let job = document
            .search_job
            .as_ref()
            .expect("active search job exists");
        let progress = job.progress();
        let fraction =
            progress_fraction(progress.bytes_scanned, progress.total_bytes, progress.done);
        return Some(ActiveJobDisplay {
            label: format!(
                "{} · {:.1}%",
                if progress.cancelled {
                    "Cancelling search"
                } else {
                    "Searching"
                },
                fraction * 100.0
            ),
            fraction,
            animate: false,
            cancel_action: active.cancel_action(),
            cancel_label: active.cancel_label(document.saving_in_place),
            cancel_enabled: !progress.cancelled && !progress.done,
        });
    }

    document.job.as_ref().map(|job| {
        let progress = job.progress();
        let fraction = progress_fraction(progress.bytes_scanned, progress.file_size, progress.done);
        ActiveJobDisplay {
            label: format!(
                "{} · {:.1}%",
                if progress.cancelled {
                    "Cancelling index"
                } else {
                    "Indexing"
                },
                fraction * 100.0
            ),
            fraction,
            animate: false,
            cancel_action: active.cancel_action(),
            cancel_label: active.cancel_label(document.saving_in_place),
            cancel_enabled: !progress.cancelled && !progress.done,
        }
    })
}

fn footer_range_text(document: &Document) -> String {
    if document.filter_active() {
        let total = document.available_filter_rows();
        let visible = document.visible_row_count() as u64;
        if document.filter_job.is_some() && total == 0 {
            return "Finding matching rows…".into();
        }
        if document.filter_rows_loading() && visible == 0 {
            return "Loading matching rows…".into();
        }
        if total == 0 {
            return "No matching rows".into();
        }
        if visible == 0 {
            return format!("{total} filter matches");
        }
        let start = document.filter_viewport_start.saturating_add(1);
        let end = document
            .filter_viewport_start
            .saturating_add(visible)
            .min(total);
        format!("matches {start}–{end} of {total}")
    } else {
        format!(
            "rows {}–{} of {}",
            document.display_start(),
            document.display_end(),
            document.available_data_rows()
        )
    }
}

fn active_job_controls(ui: &mut egui::Ui, display: &ActiveJobDisplay) -> Option<Action> {
    let clicked = ui
        .add_enabled(
            display.cancel_enabled,
            egui::Button::new(display.cancel_label).min_size(egui::vec2(STATUS_CANCEL_WIDTH, 0.0)),
        )
        .clicked();
    ui.add(
        egui::ProgressBar::new(display.fraction)
            .animate(display.animate)
            .desired_width(ui.available_width())
            .text(&display.label),
    );
    clicked.then_some(display.cancel_action)
}

fn notice_strip(ui: &mut egui::Ui, notice: &AppMessage) -> bool {
    let (color, live) = match notice.severity {
        MessageSeverity::Error => (ERROR_TEXT, egui::accesskit::Live::Assertive),
        MessageSeverity::Warning => (QUARRY_YELLOW_TEXT, egui::accesskit::Live::Polite),
        MessageSeverity::Status => unreachable!("status messages belong in the footer"),
    };
    let mut dismiss = false;
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 90.0).max(0.0);
        let response = ui.add_sized(
            egui::vec2(width, 0.0),
            egui::Label::new(RichText::new(&notice.text).color(color)).wrap(),
        );
        let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
            node.set_live(live);
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            dismiss = ui.button("Dismiss").clicked();
        });
    });
    dismiss
}

fn status_bar(ui: &mut egui::Ui, document: &Document, app_status: Option<&str>) -> Option<Action> {
    let active_job = active_job_display(document);
    let available_width = ui.available_width();
    let gap = ui.spacing().item_spacing.x;
    let job_width = STATUS_JOB_WIDTH.min((available_width - 180.0).max(0.0));
    let metadata_width = (available_width - job_width - gap).max(0.0);
    let height = ui.available_height();
    let mut action = None;

    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(metadata_width, height),
            Layout::left_to_right(Align::Center),
            |ui| {
                if document.is_dirty() {
                    ui.colored_label(Color32::from_rgb(171, 65, 53), "Modified (not saved)");
                    ui.separator();
                }
                ui.label(footer_range_text(document));

                if metadata_width >= 540.0 {
                    ui.separator();
                    ui.label(format_bytes(document.session.file_size));
                }
                if metadata_width >= 650.0 {
                    ui.separator();
                    ui.label(format!("{} columns", document.total_columns));
                }
                let shown = document.columns.shown_count();
                if metadata_width >= 760.0 && shown != document.total_columns {
                    ui.separator();
                    ui.label(format!("{shown} shown"));
                }
                let selected_rows = document.selected_rows.count();
                let selected_columns = document.selected_columns.len();
                if metadata_width >= 840.0 && (selected_rows > 0 || selected_columns > 0) {
                    ui.separator();
                    if selected_rows > 0 {
                        ui.label(format!(
                            "{selected_rows} row{} selected",
                            if selected_rows == 1 { "" } else { "s" }
                        ));
                    } else {
                        ui.label(format!(
                            "{selected_columns} column{} selected",
                            if selected_columns == 1 { "" } else { "s" }
                        ));
                    }
                }
            },
        );

        ui.allocate_ui_with_layout(
            egui::vec2(job_width, height),
            Layout::right_to_left(Align::Center),
            |ui| {
                if let Some(display) = active_job.as_ref() {
                    action = active_job_controls(ui, display);
                    return;
                }

                let status = app_status.or_else(|| {
                    let status = document.index_status();
                    (!matches!(status, "Index failed" | "Source changed")).then_some(status)
                });
                if let Some(status) = status {
                    let response = ui
                        .add(egui::Label::new(status).truncate())
                        .on_hover_text(status);
                    let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
                        node.set_live(egui::accesskit::Live::Polite);
                    });
                } else if let Some(read) = document.last_viewport_read {
                    ui.weak(format!("viewport {:.3} ms", read.as_secs_f64() * 1000.0));
                }
            },
        );
    });

    action
}

fn menu_button_with_arrow(
    ui: &mut egui::Ui,
    arrow_salt: &'static str,
    button: egui::Button<'_>,
    width: f32,
) -> egui::Response {
    let arrow_id = ui.id().with(arrow_salt);
    let button = button
        .right_text(egui::Atom::custom(arrow_id, egui::vec2(10.0, 10.0)))
        .min_size(egui::vec2(width, 24.0))
        .truncate();
    let direction = ui.layout().main_dir();
    let rendered = ui
        .allocate_ui_with_layout(
            egui::vec2(width, 24.0),
            Layout::centered_and_justified(direction),
            |ui| button.atom_ui(ui),
        )
        .inner;
    if let Some(rect) = rendered.rect(arrow_id) {
        let mut arrow = rendered.response.clone();
        arrow.rect = rect;
        egui::collapsing_header::paint_default_icon(ui, 1.0, &arrow);
    }
    rendered.response
}

fn format_menu(
    ui: &mut egui::Ui,
    document: Option<&Document>,
    applied_delimiter: DelimiterMode,
    applied_header: HeaderMode,
    draft: &mut Option<(DelimiterMode, HeaderMode)>,
    compact: bool,
    width: f32,
) -> Option<Action> {
    let document_open = document.is_some();
    let accessibility_label = if document_open {
        format!(
            "Format: {} · {}",
            applied_delimiter.label(),
            compact_header_mode_label(applied_header)
        )
    } else {
        "Format".to_owned()
    };
    let label = if compact && document_open {
        applied_delimiter.label().to_owned()
    } else {
        accessibility_label.clone()
    };
    let response = ui
        .add_enabled_ui(document_open, |ui| {
            menu_button_with_arrow(
                ui,
                "format-menu-arrow",
                egui::Button::new(RichText::new(label.clone()).atom_shrink(true)),
                width,
            )
        })
        .inner
        .on_disabled_hover_text("Open a file first.");
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            document_open,
            accessibility_label.replace(" · ", ", "),
        )
    });
    if let Some(document) = document {
        let dialect = document.session.dialect;
        let description = format!(
            "Applied {}, {}. Detected {}, {}.",
            applied_delimiter.label(),
            compact_header_mode_label(applied_header),
            detected_delimiter_label(dialect.delimiter),
            if dialect.has_header {
                "Header row"
            } else {
                "No header"
            }
        );
        let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
            node.set_description(description);
        });
    }
    let mut popup_open = draft.is_some();
    if response.clicked() {
        popup_open = !popup_open;
        *draft = popup_open.then_some((applied_delimiter, applied_header));
    }

    let mut discard_draft = false;
    let menu = egui::Popup::menu(&response)
        .open_bool(&mut popup_open)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_min_width(340.0);
            let dialect = document
                .expect("disabled Format control cannot open")
                .session
                .dialect;
            ui.label(format!(
                "Applied: {} · {}",
                applied_delimiter.label(),
                compact_header_mode_label(applied_header)
            ));
            ui.label(format!(
                "Detected: {} · {}",
                detected_delimiter_label(dialect.delimiter),
                if dialect.has_header {
                    "Header row"
                } else {
                    "No header"
                }
            ));
            ui.separator();

            let (draft_delimiter, draft_header) =
                draft.get_or_insert((applied_delimiter, applied_header));
            ui.label("Delimiter");
            ui.horizontal_wrapped(|ui| {
                for mode in DelimiterMode::ALL {
                    ui.radio_value(draft_delimiter, mode, mode.label());
                }
            });
            ui.add_space(4.0);
            ui.label("Header");
            ui.horizontal_wrapped(|ui| {
                for mode in [HeaderMode::Auto, HeaderMode::FirstRow, HeaderMode::NoHeader] {
                    ui.radio_value(draft_header, mode, header_mode_label(mode));
                }
            });
            ui.separator();

            let selected = (*draft_delimiter, *draft_header);
            let changed = selected != (applied_delimiter, applied_header);
            let dirty = document.is_some_and(Document::is_dirty);
            let operation_active = document.is_some_and(|document| {
                document.save_job.is_some()
                    || document.export_job.is_some()
                    || document.structural_job.is_some()
            });
            let mut action = None;
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    discard_draft = true;
                    ui.close();
                }
                let reopen = ui
                    .add_enabled(
                        changed && !dirty && !operation_active,
                        egui::Button::new("Reopen with Changes"),
                    )
                    .on_disabled_hover_text(if !changed {
                        "Choose a different delimiter or header mode first."
                    } else if dirty {
                        "Discard or save your changes before reopening the file."
                    } else {
                        "Cancel the active file operation and wait for it to finish first."
                    });
                if reopen.clicked() {
                    action = Some(Action::ReopenWithFormat(selected.0, selected.1));
                    discard_draft = true;
                    ui.close();
                }
            });
            action
        });
    if !popup_open || menu.is_none() || discard_draft {
        *draft = None;
    }
    menu.and_then(|inner| inner.inner)
}

fn document_menu(ui: &mut egui::Ui, document: Option<&Document>, width: f32) -> Option<Action> {
    let document_open = document.is_some();
    let dirty = document.is_some_and(Document::is_dirty);
    let file_operation_active = document.is_some_and(|document| {
        document.export_job.is_some()
            || document.save_job.is_some()
            || document.structural_job.is_some()
    });
    let save_ready = document.is_some_and(Document::is_save_ready);
    let discard_ready = document.is_some_and(|document| {
        document.is_dirty() && document.save_job.is_none() && document.structural_job.is_none()
    });
    let (filename, full_path) = document.map_or_else(
        || ("File".to_owned(), None),
        |document| {
            (
                document.logical_path.file_name().map_or_else(
                    || document.logical_path.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                ),
                Some(document.logical_path.display().to_string()),
            )
        },
    );
    let marker = RichText::new("●").color(if dirty {
        QUARRY_YELLOW_TEXT
    } else {
        Color32::TRANSPARENT
    });
    let response = menu_button_with_arrow(
        ui,
        "document-menu-arrow",
        egui::Button::new((marker, RichText::new(filename.clone()).atom_shrink(true))),
        width,
    );
    let menu = egui::Popup::menu(&response).show(|ui| {
        ui.set_min_width(190.0);
        let mut action = None;
        let open = ui
            .add_enabled(!file_operation_active && !dirty, egui::Button::new("Open…"))
            .on_disabled_hover_text(if dirty {
                "Discard or save your changes before opening another file."
            } else {
                "Cancel the active export and wait for it to finish first."
            });
        if open.clicked() {
            action = Some(Action::Choose);
        }
        let reload = ui
            .add_enabled(
                document_open && !file_operation_active && !dirty,
                egui::Button::new("Reload from Disk"),
            )
            .on_disabled_hover_text(if !document_open {
                "Open a file first."
            } else if dirty {
                "Discard or save your changes before reloading the file."
            } else {
                "Cancel the active file operation and wait for it to finish first."
            });
        if reload.clicked() {
            action = Some(Action::ReloadFromDisk);
        }
        ui.separator();
        let save = ui
            .add_enabled(save_ready, egui::Button::new("Save"))
            .on_hover_text("Save changes to this file (⌘S)")
            .on_disabled_hover_text(
                "Make a change, or wait for the active file operation to finish.",
            );
        if save.clicked() {
            action = Some(Action::Save);
        }
        let save_as = ui
            .add_enabled(save_ready, egui::Button::new("Save As…"))
            .on_disabled_hover_text(if !document_open {
                "Open a file before using Save As."
            } else if !dirty {
                "Make a change before using Save As."
            } else {
                "Wait for the active file operation to finish."
            });
        if save_as.clicked() {
            action = Some(Action::ChooseSaveAs);
        }
        let discard = ui
            .add_enabled(discard_ready, egui::Button::new("Discard Changes"))
            .on_disabled_hover_text(if !document_open {
                "Open a file first."
            } else if !dirty {
                "There are no changes to discard."
            } else {
                "Wait for the active file operation to finish."
            });
        if discard.clicked() {
            action = Some(Action::DiscardChanges);
        }
        action
    });
    let accessibility_label = if document_open {
        format!("File menu: {filename}")
    } else {
        "File menu".to_owned()
    };
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, accessibility_label.clone())
    });
    if let Some(full_path) = full_path {
        let description = if dirty {
            format!("Modified file at {full_path}")
        } else {
            full_path.clone()
        };
        let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
            node.set_description(description);
        });
        let _ = response.on_hover_text(full_path);
    }
    menu.and_then(|inner| inner.inner)
}

fn page_controls(ui: &mut egui::Ui, enabled: bool) -> Option<Action> {
    let page_up = ui
        .add_enabled(enabled, egui::Button::new("Page Up"))
        .clicked();
    let page_down = ui
        .add_enabled(enabled, egui::Button::new("Page Down"))
        .clicked();
    if page_up {
        Some(Action::PageUp)
    } else if page_down {
        Some(Action::PageDown)
    } else {
        None
    }
}

fn show_empty_state(ui: &mut egui::Ui, local_file_hovered: bool) -> Option<Action> {
    let mut action = None;
    let fill = if local_file_hovered {
        WARNING_FILL
    } else {
        Color32::from_rgb(250, 251, 251)
    };
    let stroke = if local_file_hovered {
        egui::Stroke::new(2.0, QUARRY_YELLOW)
    } else {
        egui::Stroke::new(1.0, Color32::from_rgb(200, 209, 213))
    };

    let available = ui.available_rect_before_wrap();
    let drop_rect = egui::Rect::from_center_size(
        available.center(),
        egui::vec2(available.width().min(520.0), available.height().min(112.0)),
    );
    ui.expand_to_include_rect(drop_rect);
    ui.painter()
        .rect(drop_rect, 8.0, fill, stroke, egui::StrokeKind::Inside);
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(drop_rect.shrink(24.0))
            .layout(egui::Layout::left_to_right(Align::Center).with_main_align(Align::Center)),
        |ui| {
            ui.label(RichText::new("Drop a delimited file here, or").size(16.0));
            if ui.button("Open…").clicked() {
                action = Some(Action::Choose);
            }
        },
    );
    action
}

fn filter_column_input_id(rule_index: usize) -> egui::Id {
    egui::Id::new((FILTER_COLUMN_INPUT_ID, rule_index))
}

fn filter_value_input_id(rule_index: usize) -> egui::Id {
    egui::Id::new((FILTER_VALUE_INPUT_ID, rule_index))
}

fn is_filter_text_input(focused: egui::Id, filter_rule_count: usize) -> bool {
    (0..filter_rule_count).any(|index| {
        focused == filter_column_input_id(index) || focused == filter_value_input_id(index)
    })
}

fn surrender_filter_text_focus(ctx: &egui::Context, filter_rule_count: usize) {
    let focused = ctx.memory(|memory| memory.focused());
    if let Some(focused) =
        focused.filter(|focused| is_filter_text_input(*focused, filter_rule_count))
    {
        ctx.memory_mut(|memory| memory.surrender_focus(focused));
    }
}

fn find_controls_have_focus(ctx: &egui::Context) -> bool {
    ctx.memory(|memory| {
        memory.focused().is_some_and(|focused| {
            focused == egui::Id::new(FIND_INPUT_ID) || focused == egui::Id::new(REPLACE_INPUT_ID)
        })
    })
}

fn surrender_find_controls_focus(ctx: &egui::Context) {
    let focused = ctx.memory(|memory| memory.focused());
    if let Some(focused) = focused.filter(|_| find_controls_have_focus(ctx)) {
        ctx.memory_mut(|memory| memory.surrender_focus(focused));
    }
}

fn selection_copy_requested(
    ctx: &egui::Context,
    filter_rule_count: usize,
    edited_header: Option<usize>,
    edited_cell: Option<(u64, usize)>,
) -> bool {
    let copy_event = ctx.input(|input| {
        input
            .events
            .iter()
            .any(|event| matches!(event, egui::Event::Copy))
    });
    let text_input_focused = ctx.memory(|memory| {
        memory.focused().is_some_and(|focused| {
            [
                JUMP_INPUT_ID,
                FIND_INPUT_ID,
                REPLACE_INPUT_ID,
                COLUMN_SEARCH_INPUT_ID,
            ]
            .into_iter()
            .any(|id| focused == egui::Id::new(id))
                || is_filter_text_input(focused, filter_rule_count)
                || focused == egui::Id::new(STRUCTURAL_SEPARATOR_INPUT_ID)
                || edited_header.is_some_and(|column| focused == header_edit_id(column))
                || edited_cell.is_some_and(|(row, column)| focused == cell_edit_id(row, column))
        })
    });
    copy_event && !text_input_focused
}

fn header_edit_id(column: usize) -> egui::Id {
    egui::Id::new(("quarry-header-edit", column))
}

fn cell_edit_id(row: u64, column: usize) -> egui::Id {
    egui::Id::new(("quarry-cell-edit", row, column))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColumnCommand {
    SetShown { column: usize, shown: bool },
    Move { column: usize, position: usize },
    Reset,
    AutoFit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ColumnDrag {
    column: usize,
}

fn show_column_manager(
    ctx: &egui::Context,
    open: &mut bool,
    search: &mut String,
    document: &Document,
) -> Option<ColumnCommand> {
    let mut command = None;
    let mut close_requested = false;
    let query = search.trim().to_lowercase();
    let filtered_positions = document
        .columns
        .order
        .iter()
        .enumerate()
        .filter(|(_, column)| {
            query.is_empty()
                || document
                    .column_name(**column)
                    .to_lowercase()
                    .contains(&query)
                || column.saturating_add(1).to_string().contains(&query)
        })
        .map(|(position, _)| position)
        .collect::<Vec<_>>();
    egui::Window::new("Columns")
        .id(egui::Id::new("quarry-column-manager"))
        .open(open)
        .fixed_size(egui::vec2(520.0, 520.0))
        .resizable(false)
        .show(ctx, |ui| {
            ui.label("Choose which file columns appear and their left-to-right order.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let search_response = ui.add_sized(
                    [220.0, 30.0],
                    egui::TextEdit::singleline(search)
                        .id(egui::Id::new(COLUMN_SEARCH_INPUT_ID))
                        .hint_text("Search columns"),
                );
                let _ = ui.ctx().accesskit_node_builder(search_response.id, |node| {
                    node.set_label("Search columns");
                });
                ui.label(format!(
                    "{} shown of {}",
                    document.columns.shown_count(),
                    document.total_columns
                ));
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if column_action_button(ui, "Reset", true, "Reset columns".into()) {
                        command = Some(ColumnCommand::Reset);
                    }
                });
            });
            ui.add_space(8.0);
            ui.separator();
            if document.total_columns == 0 {
                ui.label("No columns");
                return;
            }
            let row_height = 36.0;
            egui::ScrollArea::vertical()
                .id_salt("quarry-column-manager-list")
                .auto_shrink([false, false])
                .max_height(370.0)
                .show_rows(ui, row_height, filtered_positions.len(), |ui, rows| {
                    for filtered_position in rows {
                        let position = filtered_positions[filtered_position];
                        let column = document.columns.order[position];
                        ui.push_id(("managed-column", column), |ui| {
                            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                            let selected = ui.ctx().data_mut(|data| {
                                data.get_persisted::<usize>(egui::Id::new(
                                    "quarry-selected-managed-column",
                                )) == Some(column)
                            });
                            let mut checkbox_right = 0.0;
                            let row = egui::Frame::new()
                                .fill(if selected {
                                    ui.visuals().selection.bg_fill
                                } else {
                                    ui.visuals().faint_bg_color
                                })
                                .inner_margin(egui::Margin::symmetric(8, 4))
                                .show(ui, |ui| {
                                    ui.set_min_height(row_height - 8.0);
                                    ui.horizontal(|ui| {
                                        let name = document.column_name(column);
                                        let mut shown = !document.columns.hidden[column];
                                        let checkbox = ui
                                            .add(egui::Checkbox::without_text(&mut shown))
                                            .on_hover_text(if shown {
                                                "Hide column"
                                            } else {
                                                "Show column"
                                            });
                                        let _ =
                                            ui.ctx().accesskit_node_builder(checkbox.id, |node| {
                                                node.set_label(format!(
                                                    "{}  {name}",
                                                    column.saturating_add(1)
                                                ));
                                            });
                                        checkbox_right = checkbox.rect.right();
                                        if checkbox.changed() {
                                            command =
                                                Some(ColumnCommand::SetShown { column, shown });
                                        }
                                        egui::Frame::new()
                                            .fill(ui.visuals().extreme_bg_color)
                                            .corner_radius(4.0)
                                            .inner_margin(egui::Margin::symmetric(8, 2))
                                            .show(ui, |ui| {
                                                ui.label(
                                                    egui::RichText::new(
                                                        column.saturating_add(1).to_string(),
                                                    )
                                                    .monospace(),
                                                );
                                            });
                                        ui.add(egui::Label::new(name.clone()).truncate());
                                    })
                                })
                                .response;
                            let drag_target = ui.interact(
                                egui::Rect::from_min_max(
                                    egui::pos2(checkbox_right, row.rect.top()),
                                    row.rect.max,
                                ),
                                ui.id().with("column-drag-target"),
                                egui::Sense::click_and_drag(),
                            );
                            if drag_target.clicked() || drag_target.drag_started() {
                                ui.ctx().data_mut(|data| {
                                    data.insert_persisted(
                                        egui::Id::new("quarry-selected-managed-column"),
                                        column,
                                    );
                                });
                            }
                            drag_target.dnd_set_drag_payload(ColumnDrag { column });
                            drag_target.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    true,
                                    format!("Select and drag column {} to reorder", column + 1),
                                )
                            });
                            if ui.rect_contains_pointer(row.rect)
                                && let (Some(pointer), Some(dragged)) = (
                                    ui.input(|input| input.pointer.interact_pos()),
                                    egui::DragAndDrop::payload::<ColumnDrag>(ui.ctx()),
                                )
                            {
                                let (line_y, insertion) = if dragged.column == column {
                                    (row.rect.center().y, position)
                                } else if pointer.y < row.rect.center().y {
                                    (row.rect.top(), position)
                                } else {
                                    (row.rect.bottom(), position.saturating_add(1))
                                };
                                ui.painter().hline(
                                    row.rect.x_range(),
                                    line_y,
                                    egui::Stroke::new(2.0_f32, ui.visuals().selection.stroke.color),
                                );
                                if ui.input(|input| input.pointer.any_released())
                                    && let Some(dropped) =
                                        egui::DragAndDrop::take_payload::<ColumnDrag>(ui.ctx())
                                    && let Some(source_position) = document
                                        .columns
                                        .order
                                        .iter()
                                        .position(|source| *source == dropped.column)
                                {
                                    command = Some(ColumnCommand::Move {
                                        column: dropped.column,
                                        position: column_drop_position(
                                            source_position,
                                            insertion,
                                            document.columns.order.len(),
                                        ),
                                    });
                                }
                            }
                        });
                    }
                });
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Drag to reorder · Uncheck to hide");
                if ui
                    .button("Auto-fit columns")
                    .on_hover_text("Fit every shown column to its header and loaded cell values")
                    .clicked()
                {
                    command = Some(ColumnCommand::AutoFit);
                }
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Done").clicked() {
                        close_requested = true;
                    }
                });
            });
        });
    if close_requested {
        *open = false;
    }
    command
}

fn filter_button_label(filter_query: Option<&FilterQuery>) -> String {
    filter_query.map_or_else(
        || "Filters…".to_owned(),
        |query| format!("Filters ({})…", query.predicates.len()),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StructuralDialogAction {
    Apply,
    Cancel,
}

fn show_structural_dialog(
    ctx: &egui::Context,
    dialog: &mut StructuralDialog,
    sort_match_case: &mut bool,
    document: &Document,
) -> Option<StructuralDialogAction> {
    let mut action = None;
    let sort_disk = (dialog.request == StructuralRequest::Sort)
        .then(|| document.sort_temporary_disk_estimate())
        .flatten();
    let sort_description = (dialog.request == StructuralRequest::Sort).then(|| {
        let disk = sort_disk.map_or_else(
            || "Temporary disk allowance is available after indexing finishes.".to_owned(),
            |bytes| {
                format!(
                    "Conservative temporary disk allowance: {}.",
                    format_bytes(bytes)
                )
            },
        );
        let case = if *sort_match_case {
            "Letter case must match."
        } else {
            "Letter case is ignored."
        };
        format!(
            "{case} Equal values keep their original order (stable sort). The header stays fixed. Missing values sort as empty cells. {disk}"
        )
    });
    let title = match dialog.request {
        StructuralRequest::Split => "Split Columns",
        StructuralRequest::Combine => "Combine Columns",
        StructuralRequest::Move => "Move Columns",
        StructuralRequest::Sort => "Sort Rows",
    };
    let modal = egui::Modal::new(egui::Id::new("quarry-structural-dialog")).show(ctx, |ui| {
        ui.set_min_width(360.0);
        ui.heading(title);
        let columns = dialog
            .columns
            .iter()
            .map(|column| {
                let name = document
                    .current_header_fields()
                    .get(*column)
                    .map(|name| field_text(name))
                    .unwrap_or_default();
                if name.is_empty() {
                    (column.saturating_add(1)).to_string()
                } else {
                    format!("{}: {name}", column.saturating_add(1))
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        ui.label(format!(
            "Selected column{}: {columns}",
            if dialog.columns.len() == 1 { "" } else { "s" }
        ));
        ui.add_space(6.0);
        let (valid, field_has_focus, disabled_reason) = match dialog.request {
            StructuralRequest::Split | StructuralRequest::Combine => {
                let label = ui.label("Separator");
                let separator = ui
                    .add_sized(
                        [ui.available_width(), 26.0],
                        egui::TextEdit::singleline(&mut dialog.separator)
                            .id(egui::Id::new(STRUCTURAL_SEPARATOR_INPUT_ID))
                            .hint_text("Enter a literal separator"),
                    )
                    .labelled_by(label.id);
                let valid = !dialog.separator.is_empty()
                    || matches!(dialog.request, StructuralRequest::Combine);
                let reason =
                    (!valid).then(|| "Enter the text that separates each value.".to_owned());
                if let Some(reason) = &reason {
                    ui.small(reason);
                }
                (valid, separator.has_focus(), reason)
            }
            StructuralRequest::Move => {
                let label = ui.label("Destination position");
                let position = ui
                    .add_sized(
                        [ui.available_width(), 26.0],
                        egui::TextEdit::singleline(&mut dialog.position)
                            .id(egui::Id::new(STRUCTURAL_POSITION_INPUT_ID))
                            .hint_text("Enter a 1-based output position"),
                    )
                    .labelled_by(label.id);
                let _ = ui.ctx().accesskit_node_builder(position.id, |node| {
                    node.set_label("Destination position");
                });
                let reason = parse_move_position(
                    &dialog.position,
                    document.total_columns,
                    dialog.columns.len(),
                )
                .err();
                if let Some(reason) = &reason {
                    ui.small(reason);
                } else {
                    ui.small("The selected columns will start at this output position.");
                }
                (reason.is_none(), position.has_focus(), reason)
            }
            StructuralRequest::Sort => {
                ui.label("Direction");
                ui.horizontal(|ui| {
                    ui.radio_value(
                        &mut dialog.sort_direction,
                        SortDirection::Ascending,
                        "Ascending",
                    );
                    ui.radio_value(
                        &mut dialog.sort_direction,
                        SortDirection::Descending,
                        "Descending",
                    );
                });
                ui.checkbox(sort_match_case, "Match case");
                ui.small(if *sort_match_case {
                    "Uppercase and lowercase letters sort separately."
                } else {
                    "Uppercase and lowercase letters sort together."
                });
                ui.small("Equal values keep their original order (stable sort).");
                ui.small("The header stays fixed. Missing values sort as empty cells.");
                let reason = sort_disk.is_none().then(|| {
                    "Wait for indexing to finish so Quarry can calculate temporary disk space."
                        .to_owned()
                });
                if let Some(bytes) = sort_disk {
                    ui.small(format!(
                        "Conservative temporary disk allowance: {}.",
                        format_bytes(bytes)
                    ));
                } else if let Some(reason) = &reason {
                    ui.small(reason);
                }
                (reason.is_none(), false, reason)
            }
        };
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                action = Some(StructuralDialogAction::Cancel);
            }
            let submit_label = match dialog.request {
                StructuralRequest::Move => "Move",
                StructuralRequest::Sort => "Sort",
                StructuralRequest::Split | StructuralRequest::Combine => "OK",
            };
            let mut apply = ui.add_enabled(valid, egui::Button::new(submit_label));
            if let Some(reason) = disabled_reason {
                apply = apply.on_disabled_hover_text(reason);
            }
            if let Some(description) = sort_description.as_deref() {
                let _ = ui.ctx().accesskit_node_builder(apply.id, |node| {
                    node.set_description(description);
                });
            }
            if apply.clicked()
                || (valid
                    && field_has_focus
                    && ui.input(|input| input.key_pressed(egui::Key::Enter)))
            {
                action = Some(StructuralDialogAction::Apply);
            }
        });
    });
    if modal.should_close() {
        action = Some(StructuralDialogAction::Cancel);
    }
    action
}

fn show_filter_manager(
    ctx: &egui::Context,
    open: &mut bool,
    rules: &mut Vec<FilterRuleDraft>,
    match_case: &mut bool,
    document: &Document,
) -> Option<Action> {
    let mut action = None;
    egui::Window::new("Filters")
        .id(egui::Id::new("quarry-filter-manager"))
        .open(open)
        .default_width(520.0)
        .min_width(420.0)
        .vscroll(true)
        .resizable(false)
        .show(ctx, |ui| {
            ui.label(
                "Rows must match every filtered column. Equals and Contains values in the same column are alternatives.",
            );
            ui.checkbox(match_case, "Match case");
            ui.add_space(6.0);
            let rule_count = rules.len();
            let sole_rule = rule_count == 1;
            let mut remove_index = None;
            for (index, rule) in rules.iter_mut().enumerate() {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.strong(format!("Rule {}", index + 1));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let response =
                                ui.add_enabled(!sole_rule, egui::Button::new("Remove"));
                            let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
                                node.set_label(format!("Remove rule {}", index + 1));
                            });
                            if response.clicked() {
                                surrender_filter_text_focus(ui.ctx(), rule_count);
                                remove_index = Some(index);
                            }
                        });
                    });
                    ui.horizontal(|ui| {
                        let label = ui.label(format!("Rule {} file column (1-based)", index + 1));
                        let _ = ui
                            .add_sized(
                                [96.0, 26.0],
                                egui::TextEdit::singleline(&mut rule.column_input)
                                    .id(filter_column_input_id(index))
                                    .horizontal_align(Align::RIGHT),
                            )
                            .labelled_by(label.id);
                        if let Ok(column) =
                            parse_file_column(&rule.column_input, document.total_columns)
                        {
                            ui.label(document.column_name(column));
                        }
                    });
                    ui.horizontal(|ui| {
                        let label = ui.label(format!("Rule {} match", index + 1));
                        let _ = egui::ComboBox::from_id_salt((
                            "quarry-filter-operator",
                            index,
                        ))
                        .selected_text(filter_operator_label(rule.operator))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut rule.operator,
                                FilterOperator::Contains,
                                "Contains",
                            );
                            ui.selectable_value(
                                &mut rule.operator,
                                FilterOperator::Equals,
                                "Equals",
                            );
                            ui.selectable_value(
                                &mut rule.operator,
                                FilterOperator::NotEquals,
                                "Does not equal",
                            );
                        })
                        .response
                        .labelled_by(label.id);
                    });
                    let label = ui.label(format!("Rule {} value", index + 1));
                    let _ = ui
                        .add_sized(
                            [ui.available_width(), 48.0],
                            egui::TextEdit::multiline(&mut rule.value_input)
                                .desired_rows(2)
                                .id(filter_value_input_id(index))
                                .hint_text("Literal text"),
                        )
                        .labelled_by(label.id);
                });
                ui.add_space(4.0);
            }
            if let Some(index) = remove_index {
                rules.remove(index);
            }
            if ui.button("Add rule").clicked() {
                surrender_filter_text_focus(ui.ctx(), rules.len());
                rules.push(FilterRuleDraft::default());
            }

            let can_apply = document.is_filter_ready()
                && !document.has_cell_edits()
                && document.search_job.is_none()
                && document.filter_job.is_none()
                && document.export_job.is_none()
                && !rules.is_empty()
                && rules.iter().all(|rule| {
                    parse_file_column(&rule.column_input, document.total_columns).is_ok()
                        && (rule.operator != FilterOperator::Contains
                            || !rule.value_input.is_empty())
                });
            if ui
                .add_enabled(can_apply, egui::Button::new("Apply filters"))
                .clicked()
            {
                action = Some(Action::ApplyFilter);
            }
            ui.small("Contains requires a value. Equals and Does not equal can compare with an empty cell. Values are literal.");
            if document.has_cell_edits() {
                ui.small("Save or discard cell edits before filtering the source file.");
            }

            if let Some(query) = document.filter_query.as_ref() {
                ui.add_space(6.0);
                ui.label(format!(
                    "Active: {} rule{} ({})",
                    query.predicates.len(),
                    if query.predicates.len() == 1 { "" } else { "s" },
                    case_sensitivity_label(query.case_sensitivity),
                ));
                for (index, predicate) in query.predicates.iter().enumerate() {
                    let value = field_text(&predicate.value);
                    ui.label(format!(
                        "{}. file column {} ({}) {} {:?}",
                        index + 1,
                        predicate.column.saturating_add(1),
                        document.column_name(predicate.column),
                        filter_operator_label(predicate.operator).to_lowercase(),
                        value
                    ));
                }
            }
            if document.search_job.is_some() {
                ui.label("Cancel the active search before filtering.");
            } else if document.total_columns == 0 {
                ui.label("Open a file with at least one column to filter rows.");
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        document.filter_active() && document.export_job.is_none(),
                        egui::Button::new("Clear filter"),
                    )
                    .clicked()
                {
                    action = Some(Action::ClearFilter);
                }
                if document.filter_active()
                    && ui
                        .add_enabled(
                            document.is_filtered_export_ready() && !document.is_dirty(),
                            egui::Button::new("Export Filtered Rows…"),
                        )
                        .on_hover_text(if document.is_dirty() {
                            "Save or discard your changes before exporting filtered rows."
                        } else {
                            "Export all matching rows to a new file"
                        })
                        .clicked()
                {
                    action = Some(Action::ChooseFilteredExport);
                }
            });
        });
    action
}

fn filter_operator_label(operator: FilterOperator) -> &'static str {
    match operator {
        FilterOperator::Contains => "Contains",
        FilterOperator::Equals => "Equals",
        FilterOperator::NotEquals => "Does not equal",
    }
}

fn case_sensitivity(match_case: bool) -> CaseSensitivity {
    if match_case {
        CaseSensitivity::Sensitive
    } else {
        CaseSensitivity::Insensitive
    }
}

fn case_sensitivity_label(case_sensitivity: CaseSensitivity) -> &'static str {
    match case_sensitivity {
        CaseSensitivity::Insensitive => "case-insensitive",
        CaseSensitivity::Sensitive => "case-sensitive",
    }
}

fn column_drop_position(source_position: usize, insertion: usize, total_columns: usize) -> usize {
    insertion
        .saturating_sub(usize::from(source_position < insertion))
        .min(total_columns.saturating_sub(1))
}

fn column_action_button(
    ui: &mut egui::Ui,
    text: &str,
    enabled: bool,
    accessible_label: String,
) -> bool {
    let response = ui
        .add_enabled(enabled, egui::Button::new(text))
        .on_hover_text(&accessible_label);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            enabled && ui.is_enabled(),
            accessible_label.clone(),
        )
    });
    response.clicked()
}

#[allow(clippy::too_many_arguments)]
fn search_controls(
    ui: &mut egui::Ui,
    query: &mut String,
    replacement: &mut String,
    match_case: &mut bool,
    replace_expanded: &mut bool,
    index_ready: bool,
    can_find_previous: bool,
    can_replace: bool,
    searching: bool,
    filter_active: bool,
) -> (Option<Action>, bool) {
    let mut action = None;
    let mut close_requested = false;
    ui.horizontal(|ui| {
        let label = ui.label("Find (literal)");
        let input_id = egui::Id::new(FIND_INPUT_ID);
        let input = ui
            .add_enabled(
                !searching && !filter_active,
                egui::TextEdit::singleline(query)
                    .id(input_id)
                    .hint_text("Text to find")
                    .desired_width(180.0),
            )
            .labelled_by(label.id);
        if (input.has_focus() || input.lost_focus() && !input.clicked_elsewhere())
            && ui.input(|input| input.key_pressed(egui::Key::Escape))
        {
            close_requested = true;
        }
        let can_find = index_ready && !searching && !filter_active && !query.is_empty();
        let enter_pressed =
            input.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
        let previous_shortcut = enter_pressed && ui.input(|input| input.modifiers.shift);
        let next_shortcut = enter_pressed && !ui.input(|input| input.modifiers.shift);
        if ui
            .add_enabled(
                can_find && can_find_previous,
                egui::Button::new("Find Previous"),
            )
            .on_disabled_hover_text("Find another match before moving back.")
            .clicked()
            || (can_find && can_find_previous && previous_shortcut)
        {
            action = Some(Action::FindPrevious);
        }
        if ui
            .add_enabled(can_find, egui::Button::new("Find Next"))
            .clicked()
            || (can_find && next_shortcut)
        {
            action = Some(Action::FindNext);
        }

        ui.add_enabled(
            !searching && !filter_active,
            egui::Checkbox::new(match_case, "Match case"),
        );
        let was_expanded = *replace_expanded;
        let replace = ui.add_enabled(
            !searching && !filter_active,
            egui::Button::new("Replace").selected(*replace_expanded),
        );
        if replace.clicked() {
            *replace_expanded = !*replace_expanded;
        }
        let _ = ui.ctx().accesskit_node_builder(replace.id, |node| {
            node.set_toggled(if *replace_expanded {
                egui::accesskit::Toggled::True
            } else {
                egui::accesskit::Toggled::False
            });
        });
        if was_expanded && !*replace_expanded {
            ui.memory_mut(|memory| {
                memory.surrender_focus(egui::Id::new(REPLACE_INPUT_ID));
            });
        }
        if ui.button("Close find").clicked() {
            close_requested = true;
        }
    });

    if *replace_expanded {
        ui.horizontal(|ui| {
            let label = ui.label("Replace with (literal)");
            let input_id = egui::Id::new(REPLACE_INPUT_ID);
            let input = ui
                .add_enabled(
                    !searching && !filter_active,
                    egui::TextEdit::singleline(replacement)
                        .id(input_id)
                        .hint_text("Replacement text")
                        .desired_width(180.0),
                )
                .labelled_by(label.id);
            if (input.has_focus() || input.lost_focus() && !input.clicked_elsewhere())
                && ui.input(|input| input.key_pressed(egui::Key::Escape))
            {
                close_requested = true;
            }
            let can_replace = can_replace && !searching && !filter_active;
            if ui
                .add_enabled(can_replace, egui::Button::new("Replace in Cell"))
                .on_disabled_hover_text("Use Find Next or Find Previous to select a match first.")
                .clicked()
                || (can_replace
                    && input.lost_focus()
                    && ui.input(|input| input.key_pressed(egui::Key::Enter)))
            {
                action = Some(Action::ReplaceCurrent);
            }
            if ui
                .add_enabled(
                    index_ready
                        && !searching
                        && !filter_active
                        && !query.is_empty()
                        && query != replacement,
                    egui::Button::new("Replace All"),
                )
                .clicked()
            {
                action = Some(Action::ReplaceAll);
            }
        });
    }
    if filter_active {
        ui.small("Clear the filter before using Find.");
    }
    (action, close_requested)
}

fn filtered_export_file_name(source: &Path) -> String {
    let stem = source
        .file_stem()
        .or_else(|| source.file_name())
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "export".into());
    match source.extension() {
        Some(extension) => format!("{stem}-filtered.{}", extension.to_string_lossy()),
        None => format!("{stem}-filtered"),
    }
}

fn save_as_file_name(source: &Path) -> String {
    let stem = source
        .file_stem()
        .or_else(|| source.file_name())
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "edited".into());
    match source.extension() {
        Some(extension) => format!("{stem}-edited.{}", extension.to_string_lossy()),
        None => format!("{stem}-edited"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GridSelection {
    Cell { row: u64, column: usize },
    Row { row: u64 },
}

#[derive(Default)]
struct GridInteraction {
    selection: Option<GridSelection>,
    row_selection: Option<(RowSelection, Option<u64>)>,
    column_request: Option<GridColumnRequest>,
    filter_query: Option<FilterQuery>,
    copy_selection: bool,
}

impl GridSelection {
    fn row(self) -> u64 {
        match self {
            Self::Cell { row, .. } | Self::Row { row, .. } => row,
        }
    }

    fn selects_row(self, row: u64) -> bool {
        matches!(self, Self::Row { row: selected, .. } if selected == row)
    }

    fn selects_cell(self, row: u64, column: usize) -> bool {
        self.selects_row(row)
            || matches!(
                self,
                Self::Cell {
                    row: selected_row,
                    column: selected_column,
                    ..
                } if selected_row == row && selected_column == column
            )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RowSelection {
    ranges: Vec<RangeInclusive<u64>>,
    anchor: Option<u64>,
}

impl RowSelection {
    fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    fn contains(&self, row: u64) -> bool {
        let index = self.ranges.partition_point(|range| *range.end() < row);
        self.ranges
            .get(index)
            .is_some_and(|range| range.contains(&row))
    }

    fn count(&self) -> u64 {
        self.ranges.iter().fold(0_u64, |count, range| {
            count.saturating_add(range.end().saturating_sub(*range.start()).saturating_add(1))
        })
    }

    fn first(&self) -> Option<u64> {
        self.ranges.first().map(|range| *range.start())
    }

    fn clear(&mut self) {
        self.ranges.clear();
        self.anchor = None;
    }

    fn select(&mut self, row: u64, modifiers: egui::Modifiers) {
        if modifiers.shift {
            let anchor = self
                .anchor
                .or_else(|| self.ranges.first().map(|range| *range.start()))
                .unwrap_or(row);
            self.ranges = vec![anchor.min(row)..=anchor.max(row)];
            self.anchor = Some(anchor);
        } else if modifiers.command {
            self.toggle(row);
        } else {
            self.ranges = vec![row..=row];
            self.anchor = Some(row);
        }
    }

    fn toggle(&mut self, row: u64) {
        let index = self.ranges.partition_point(|range| *range.end() < row);
        if self
            .ranges
            .get(index)
            .is_some_and(|range| range.contains(&row))
        {
            let start = *self.ranges[index].start();
            let end = *self.ranges[index].end();
            match (row == start, row == end) {
                (true, true) => {
                    self.ranges.remove(index);
                }
                (true, false) => self.ranges[index] = row.saturating_add(1)..=end,
                (false, true) => self.ranges[index] = start..=row.saturating_sub(1),
                (false, false) => {
                    self.ranges[index] = start..=row.saturating_sub(1);
                    self.ranges.insert(index + 1, row.saturating_add(1)..=end);
                }
            }
            if self.anchor == Some(row) {
                self.anchor = self.ranges.first().map(|range| *range.start());
            }
            return;
        }

        self.ranges.insert(index, row..=row);
        self.merge_adjacent();
        self.anchor = Some(row);
    }

    fn merge_adjacent(&mut self) {
        let mut merged: Vec<RangeInclusive<u64>> = Vec::with_capacity(self.ranges.len());
        for range in self.ranges.drain(..) {
            if let Some(previous) = merged.last_mut()
                && *range.start() <= previous.end().saturating_add(1)
            {
                let start = *previous.start();
                let end = (*previous.end()).max(*range.end());
                *previous = start..=end;
            } else {
                merged.push(range);
            }
        }
        self.ranges = merged;
    }
}

struct WorkingCopyState {
    directory: TempDir,
    next_generation: u64,
    undo: Option<WorkingCopySnapshot>,
    redo: Option<WorkingCopySnapshot>,
}

impl WorkingCopyState {
    fn new() -> Result<Self, String> {
        Ok(Self {
            directory: tempfile::Builder::new()
                .prefix("quarry-working-")
                .tempdir()
                .map_err(|error| error.to_string())?,
            next_generation: 1,
            undo: None,
            redo: None,
        })
    }

    fn next_path(&mut self) -> PathBuf {
        let path = self
            .directory
            .path()
            .join(format!("generation-{}.csv", self.next_generation));
        self.next_generation = self.next_generation.saturating_add(1);
        path
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct StructuralOverlay {
    header_renames: BTreeMap<usize, String>,
    cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkingCopySnapshot {
    path: PathBuf,
    overlay: StructuralOverlay,
}

struct MaterializationPreparation {
    undo: WorkingCopySnapshot,
    destination: PathBuf,
    renames: BTreeMap<usize, Vec<u8>>,
}

enum StructuralJob {
    AnalyzingSplit {
        job: SplitAnalysisJob,
        source_column: usize,
        separator: Vec<u8>,
    },
    Materializing {
        job: SaveAsJob,
        destination: PathBuf,
        selected_columns: BTreeSet<usize>,
        undo: WorkingCopySnapshot,
    },
    Replacing {
        job: ReplaceAllJob,
        destination: PathBuf,
        selected_columns: BTreeSet<usize>,
        undo: WorkingCopySnapshot,
    },
    Sorting {
        job: SortJob,
        destination: PathBuf,
        selected_columns: BTreeSet<usize>,
        undo: WorkingCopySnapshot,
        column: usize,
        direction: SortDirection,
    },
    DeletingRows {
        job: SaveAsJob,
        destination: PathBuf,
        undo: WorkingCopySnapshot,
        count: u64,
    },
}

struct MaterializedWorkingCopy {
    path: PathBuf,
    selected_columns: BTreeSet<usize>,
    notice: String,
}

struct StructuralProgressDisplay {
    fraction: f32,
    label: String,
    animate: bool,
}

fn sort_merge_progress(
    bytes_scanned: u64,
    total_bytes: u64,
    done: bool,
) -> Option<StructuralProgressDisplay> {
    (!done && (total_bytes == 0 || bytes_scanned >= total_bytes)).then(|| {
        StructuralProgressDisplay {
            fraction: 0.9,
            label: "Merging sorted rows…".into(),
            animate: true,
        }
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ColumnView {
    order: Vec<usize>,
    hidden: Vec<bool>,
    shown: usize,
    start: usize,
    visible: Vec<usize>,
}

impl ColumnView {
    fn new(total_columns: usize) -> Self {
        let mut view = Self {
            order: (0..total_columns).collect(),
            hidden: vec![false; total_columns],
            shown: total_columns,
            start: 0,
            visible: Vec::new(),
        };
        view.refresh();
        view
    }

    fn shown_count(&self) -> usize {
        self.shown
    }

    fn extend_to(&mut self, total_columns: usize) {
        let current = self.hidden.len();
        if total_columns <= current {
            return;
        }
        self.order.extend(current..total_columns);
        self.hidden.resize(total_columns, false);
        self.shown = self.shown.saturating_add(total_columns - current);
        self.refresh();
    }

    fn view(&mut self, column: usize) -> bool {
        if column >= self.hidden.len() {
            return false;
        }
        if self.hidden[column] {
            self.hidden[column] = false;
            self.shown = self.shown.saturating_add(1);
        }
        self.refresh();
        true
    }

    fn set_shown(&mut self, column: usize, shown: bool) -> bool {
        let Some(hidden) = self.hidden.get_mut(column) else {
            return false;
        };
        let was_shown = !*hidden;
        if was_shown != shown {
            self.shown = if shown {
                self.shown.saturating_add(1)
            } else {
                self.shown.saturating_sub(1)
            };
        }
        *hidden = !shown;
        self.refresh();
        true
    }

    fn move_column(&mut self, column: usize, target: usize) -> bool {
        let Some(position) = self.order.iter().position(|source| *source == column) else {
            return false;
        };
        if target >= self.order.len() {
            return false;
        }
        if position != target {
            let column = self.order.remove(position);
            self.order.insert(target, column);
        }
        self.refresh();
        true
    }

    fn reset(&mut self) {
        for (column, source) in self.order.iter_mut().enumerate() {
            *source = column;
        }
        self.hidden.fill(false);
        self.shown = self.hidden.len();
        self.refresh();
    }

    fn refresh(&mut self) {
        self.start = 0;
        // ponytail: render every shown column; add horizontal virtualization if very-wide files become measurably slow.
        self.visible = self
            .order
            .iter()
            .copied()
            .filter(|source| !self.hidden[*source])
            .collect();
    }
}

struct Document {
    session: Session,
    logical_path: PathBuf,
    original_session: Option<Session>,
    working_copy: Option<WorkingCopyState>,
    job: Option<IndexJob>,
    index: Option<StructuralIndex>,
    progress: IndexProgress,
    search_job: Option<SearchJob>,
    search_query: Vec<u8>,
    search_case_sensitivity: CaseSensitivity,
    last_match: Option<SearchMatch>,
    search_history: Vec<SearchMatch>,
    search_history_index: Option<usize>,
    search_status: Option<String>,
    filter_job: Option<FilterJob>,
    filter_index: Option<FilterIndex>,
    filter_query: Option<FilterQuery>,
    filter_progress: Option<FilterProgress>,
    filter_status: Option<String>,
    export_job: Option<FilterExportJob>,
    export_progress: Option<FilterExportProgress>,
    export_status: Option<String>,
    export_cancel_requested: bool,
    save_job: Option<SaveAsJob>,
    save_status: Option<String>,
    save_cancel_requested: bool,
    saving_in_place: bool,
    structural_job: Option<StructuralJob>,
    structural_status: Option<String>,
    structural_cancel_requested: bool,
    source_changed: bool,
    filter_viewport_start: u64,
    filter_buffer_start: u64,
    filtered_rows: Vec<FilterMatch>,
    filter_read: Option<ActiveFilterRead>,
    pending_filter_read: Option<FilterReadWindow>,
    reveal_cell: Option<(u64, usize)>,
    selection: Option<GridSelection>,
    selected_rows: RowSelection,
    selected_columns: BTreeSet<usize>,
    column_selection_anchor: Option<usize>,
    headers: Vec<String>,
    header_renames: BTreeMap<usize, String>,
    header_edit: Option<HeaderEdit>,
    cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
    cell_edit: Option<CellEdit>,
    cell_focus_requested: Option<(u64, usize)>,
    column_focus_requested: Option<usize>,
    total_columns: usize,
    columns: ColumnView,
    data_start: u64,
    viewport_start: u64,
    buffer_start: u64,
    buffered_rows: Vec<Row>,
    auto_fit_columns: bool,
    fitted_column_widths: BTreeMap<usize, f32>,
    columns_to_fit: VecDeque<usize>,
    reset_table_widths: bool,
    visible_rows: usize,
    scroll_points: f32,
    last_viewport_read: Option<Duration>,
    last_poll: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HeaderEdit {
    column: usize,
    draft: String,
    focus_requested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CellEdit {
    row: u64,
    column: usize,
    source: Vec<u8>,
    draft: String,
    focus_requested: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FilterReadWindow {
    start_match: u64,
    count: usize,
}

struct ActiveFilterRead {
    window: FilterReadWindow,
    job: FilterReadJob,
    started: Instant,
    cancel_requested: bool,
}

impl Document {
    #[cfg(test)]
    fn open(path: &Path, options: OpenOptions) -> Result<Self, String> {
        let mut document = Self::prepare(path, options)?;
        document.start_indexing()?;
        Ok(document)
    }

    fn prepare(path: &Path, options: OpenOptions) -> Result<Self, String> {
        let buffer_rows = BOOTSTRAP_ROWS + 2 * OVERSCAN_ROWS;
        let session = Session::open(
            path,
            OpenOptions {
                rows: buffer_rows + 1,
                ..options
            },
        )
        .map_err(|error| error.to_string())?;
        let data_start = u64::from(session.dialect.has_header);
        let total_columns = session
            .first_rows
            .iter()
            .map(|row| row.fields.len())
            .max()
            .unwrap_or(0);
        let columns = ColumnView::new(total_columns);
        let headers = headers_for(&session, &columns.visible);
        let buffered_rows = session
            .first_rows
            .iter()
            .skip(data_start as usize)
            .take(buffer_rows)
            .cloned()
            .collect();
        let progress = IndexProgress {
            bytes_scanned: 0,
            rows_scanned: 0,
            file_size: session.file_size,
            elapsed: Duration::ZERO,
            done: false,
            cancelled: false,
        };

        Ok(Self {
            session,
            logical_path: path.to_path_buf(),
            original_session: None,
            working_copy: None,
            job: None,
            index: None,
            progress,
            search_job: None,
            search_query: Vec::new(),
            search_case_sensitivity: CaseSensitivity::Insensitive,
            last_match: None,
            search_history: Vec::new(),
            search_history_index: None,
            search_status: None,
            filter_job: None,
            filter_index: None,
            filter_query: None,
            filter_progress: None,
            filter_status: None,
            export_job: None,
            export_progress: None,
            export_status: None,
            export_cancel_requested: false,
            save_job: None,
            save_status: None,
            save_cancel_requested: false,
            saving_in_place: false,
            structural_job: None,
            structural_status: None,
            structural_cancel_requested: false,
            source_changed: false,
            filter_viewport_start: 0,
            filter_buffer_start: 0,
            filtered_rows: Vec::new(),
            filter_read: None,
            pending_filter_read: None,
            reveal_cell: None,
            selection: None,
            selected_rows: RowSelection::default(),
            selected_columns: BTreeSet::new(),
            column_selection_anchor: None,
            headers,
            header_renames: BTreeMap::new(),
            header_edit: None,
            cell_edits: BTreeMap::new(),
            cell_edit: None,
            cell_focus_requested: None,
            column_focus_requested: None,
            total_columns,
            columns,
            data_start,
            viewport_start: data_start,
            buffer_start: data_start,
            buffered_rows,
            auto_fit_columns: false,
            fitted_column_widths: BTreeMap::new(),
            columns_to_fit: VecDeque::new(),
            reset_table_widths: false,
            visible_rows: BOOTSTRAP_ROWS,
            scroll_points: 0.0,
            last_viewport_read: None,
            last_poll: Instant::now() - POLL_INTERVAL,
        })
    }

    fn current_open_options(&self) -> OpenOptions {
        OpenOptions {
            delimiter: Some(self.session.dialect.delimiter),
            header_mode: if self.session.dialect.has_header {
                HeaderMode::FirstRow
            } else {
                HeaderMode::NoHeader
            },
            ..OpenOptions::default()
        }
    }

    fn structural_edit_disabled_reason(&self) -> Option<&'static str> {
        if self.source_changed {
            Some("Reopen the changed source before editing the document.")
        } else if self.filter_active() {
            Some("Clear the filter before editing the document.")
        } else if self.save_job.is_some()
            || self.export_job.is_some()
            || self.structural_job.is_some()
            || self.search_job.is_some()
            || self.filter_job.is_some()
        {
            Some("Wait for the active file operation to finish.")
        } else {
            None
        }
    }

    fn start_split(&mut self, source_column: usize, separator: Vec<u8>) -> Result<(), AppMessage> {
        if let Some(reason) = self.structural_edit_disabled_reason() {
            return Err(AppMessage::warning(reason));
        }
        if source_column >= self.total_columns {
            return Err(AppMessage::warning(format!(
                "Column {} is outside this file.",
                source_column.saturating_add(1)
            )));
        }
        if separator.is_empty() {
            return Err(AppMessage::warning("Enter a non-empty separator."));
        }
        self.commit_edits();
        self.cancel_search();
        let max_pieces = MAX_TRANSFORMATION_COLUMNS
            .saturating_sub(self.total_columns)
            .saturating_add(1)
            .min(MAX_TRANSFORMATION_COLUMNS);
        if max_pieces < 2 {
            return Err(AppMessage::warning(format!(
                "Splitting would exceed the {MAX_TRANSFORMATION_COLUMNS}-column file limit."
            )));
        }
        let job = match self.session.start_analyze_split(
            self.cell_edits.clone(),
            source_column,
            separator.clone(),
            max_pieces,
        ) {
            Ok(job) => job,
            Err(QuarryError::SourceChanged) => {
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(error) => return Err(AppMessage::error(error.to_string())),
        };
        self.structural_job = Some(StructuralJob::AnalyzingSplit {
            job,
            source_column,
            separator,
        });
        self.structural_status = Some("Checking split width…".into());
        self.structural_cancel_requested = false;
        Ok(())
    }

    fn start_combine(
        &mut self,
        source_columns: Vec<usize>,
        separator: Vec<u8>,
    ) -> Result<(), AppMessage> {
        if let Some(reason) = self.structural_edit_disabled_reason() {
            return Err(AppMessage::warning(reason));
        }
        if source_columns.len() < 2 {
            return Err(AppMessage::warning(
                "Select at least two columns to combine.",
            ));
        }
        if source_columns
            .iter()
            .any(|column| *column >= self.total_columns)
        {
            return Err(AppMessage::warning(
                "A selected column is outside this file.",
            ));
        }
        let mut unique = BTreeSet::new();
        if source_columns.iter().any(|column| !unique.insert(*column)) {
            return Err(AppMessage::warning("Select each column only once."));
        }
        self.commit_edits();
        self.cancel_search();
        let output_header = self.session.dialect.has_header.then(|| {
            self.current_header_fields()
                .get(source_columns[0])
                .cloned()
                .unwrap_or_default()
        });
        let insertion = *source_columns
            .iter()
            .min()
            .expect("at least two combine columns were validated");
        self.begin_materialization(
            ColumnTransformation::Join {
                source_columns,
                separator,
                output_header,
            },
            BTreeSet::from([insertion]),
        )
    }

    fn start_move_columns(
        &mut self,
        columns: Vec<usize>,
        position: usize,
    ) -> Result<(), AppMessage> {
        let selected =
            validated_column_selection(columns, self.total_columns).map_err(AppMessage::warning)?;
        let remaining = (0..self.total_columns)
            .filter(|column| !selected.contains(column))
            .collect::<Vec<_>>();
        if position > remaining.len() {
            return Err(AppMessage::warning(format!(
                "Destination position must be between 1 and {}.",
                remaining.len().saturating_add(1)
            )));
        }
        let mut output_columns = Vec::with_capacity(self.total_columns);
        output_columns.extend_from_slice(&remaining[..position]);
        output_columns.extend(selected.iter().copied());
        output_columns.extend_from_slice(&remaining[position..]);
        let selected_columns =
            (position..position.saturating_add(selected.len())).collect::<BTreeSet<_>>();
        self.start_arrangement(output_columns, selected_columns)
    }

    fn start_delete_columns(&mut self, columns: Vec<usize>) -> Result<(), AppMessage> {
        let selected =
            validated_column_selection(columns, self.total_columns).map_err(AppMessage::warning)?;
        if selected.len() == self.total_columns {
            return Err(AppMessage::warning("At least one column must remain."));
        }
        let output_columns = (0..self.total_columns)
            .filter(|column| !selected.contains(column))
            .collect::<Vec<_>>();
        let first_deleted = *selected
            .iter()
            .next()
            .expect("validated selection is not empty");
        let nearest = first_deleted.min(output_columns.len() - 1);
        self.start_arrangement(output_columns, BTreeSet::from([nearest]))
    }

    fn start_delete_rows(&mut self, rows: Vec<RangeInclusive<u64>>) -> Result<(), AppMessage> {
        if let Some(reason) = self.structural_edit_disabled_reason() {
            return Err(AppMessage::warning(reason));
        }
        if rows.is_empty() {
            return Err(AppMessage::warning(
                "Select at least one numbered row first.",
            ));
        }
        let count = rows.iter().fold(0_u64, |count, range| {
            count.saturating_add(range.end().saturating_sub(*range.start()).saturating_add(1))
        });
        self.commit_edits();
        self.cancel_search();
        let MaterializationPreparation {
            undo,
            destination,
            renames,
        } = self.prepare_materialization()?;
        let job = match self.session.start_create_working_copy_deleting_rows(
            renames,
            self.cell_edits.clone(),
            rows,
            &destination,
        ) {
            Ok(job) => job,
            Err(QuarryError::SourceChanged) => {
                self.cleanup_empty_working_copy();
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(QuarryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.cleanup_empty_working_copy();
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(error) => {
                self.cleanup_empty_working_copy();
                return Err(AppMessage::error(error.to_string()));
            }
        };
        self.structural_job = Some(StructuralJob::DeletingRows {
            job,
            destination,
            undo,
            count,
        });
        self.structural_status = Some(format!(
            "Deleting {count} selected row{}…",
            if count == 1 { "" } else { "s" }
        ));
        self.structural_cancel_requested = false;
        Ok(())
    }

    fn start_arrangement(
        &mut self,
        output_columns: Vec<usize>,
        selected_columns: BTreeSet<usize>,
    ) -> Result<(), AppMessage> {
        if let Some(reason) = self.structural_edit_disabled_reason() {
            return Err(AppMessage::warning(reason));
        }
        if output_columns.iter().copied().eq(0..self.total_columns) {
            self.structural_status = Some("Columns are already in that position.".into());
            return Ok(());
        }
        self.commit_edits();
        self.cancel_search();
        self.begin_materialization(
            ColumnTransformation::Arrange {
                source_width: self.total_columns,
                output_columns,
            },
            selected_columns,
        )
    }

    fn start_sort_rows_with_case(
        &mut self,
        column: usize,
        direction: SortDirection,
        case_sensitivity: CaseSensitivity,
    ) -> Result<(), AppMessage> {
        if let Some(reason) = self.structural_edit_disabled_reason() {
            return Err(AppMessage::warning(reason));
        }
        if self.sort_temporary_disk_estimate().is_none() {
            return Err(AppMessage::warning(
                "Wait for indexing to finish so Quarry can calculate temporary disk space.",
            ));
        }
        self.validate_column(column).map_err(AppMessage::warning)?;
        self.commit_edits();
        self.cancel_search();
        let MaterializationPreparation {
            undo,
            destination,
            renames,
        } = self.prepare_materialization()?;
        let job = match self.session.start_create_sorted_working_copy(
            renames,
            self.cell_edits.clone(),
            SortSpec {
                column,
                direction,
                case_sensitivity,
            },
            &destination,
        ) {
            Ok(job) => job,
            Err(QuarryError::SourceChanged) => {
                self.cleanup_empty_working_copy();
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(QuarryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.cleanup_empty_working_copy();
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(error) => {
                self.cleanup_empty_working_copy();
                return Err(AppMessage::error(error.to_string()));
            }
        };
        self.structural_job = Some(StructuralJob::Sorting {
            job,
            destination,
            selected_columns: BTreeSet::from([column]),
            undo,
            column,
            direction,
        });
        self.structural_status = Some("Sorting rows…".into());
        self.structural_cancel_requested = false;
        Ok(())
    }

    fn sort_temporary_disk_estimate(&self) -> Option<u64> {
        let data_rows = self
            .index
            .as_ref()?
            .indexed_rows()
            .saturating_sub(self.data_start);
        let effective_bytes_upper_bound = self.header_renames.values().fold(
            self.session
                .file_size
                .saturating_add(data_rows.saturating_mul(2)),
            |bytes, value| bytes.saturating_add(serialized_field_upper_bound(value.as_bytes())),
        );
        let effective_bytes_upper_bound = self
            .cell_edits
            .values()
            .fold(effective_bytes_upper_bound, |bytes, value| {
                bytes.saturating_add(serialized_field_upper_bound(value))
            });
        let effective_bytes_upper_bound =
            self.header_edit
                .as_ref()
                .map_or(effective_bytes_upper_bound, |edit| {
                    effective_bytes_upper_bound
                        .saturating_add(serialized_field_upper_bound(edit.draft.as_bytes()))
                });
        let effective_bytes_upper_bound =
            self.cell_edit
                .as_ref()
                .map_or(effective_bytes_upper_bound, |edit| {
                    effective_bytes_upper_bound
                        .saturating_add(serialized_field_upper_bound(edit.draft.as_bytes()))
                });
        Some(estimate_sort_temporary_bytes(
            effective_bytes_upper_bound,
            data_rows,
        ))
    }

    fn prepare_materialization(&mut self) -> Result<MaterializationPreparation, AppMessage> {
        if self.original_session.is_none() {
            match self.session.ensure_source_unchanged() {
                Ok(()) => {}
                Err(QuarryError::SourceChanged) => {
                    self.invalidate_changed_source();
                    return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
                }
                Err(error) => return Err(AppMessage::error(error.to_string())),
            }
            let original = match Session::open(&self.logical_path, self.current_open_options()) {
                Ok(original) => original,
                Err(QuarryError::SourceChanged) => {
                    self.invalidate_changed_source();
                    return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
                }
                Err(QuarryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.invalidate_changed_source();
                    return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
                }
                Err(error) => return Err(AppMessage::error(error.to_string())),
            };
            self.original_session = Some(original);
        }
        if self.working_copy.is_none() {
            self.working_copy = Some(WorkingCopyState::new().map_err(AppMessage::error)?);
        }
        let undo = WorkingCopySnapshot {
            path: self.session.path().to_path_buf(),
            overlay: StructuralOverlay {
                header_renames: self.header_renames.clone(),
                cell_edits: self.cell_edits.clone(),
            },
        };
        let state = self
            .working_copy
            .as_mut()
            .expect("the working-copy state was created");
        let destination = state.next_path();
        let renames = self
            .header_renames
            .iter()
            .map(|(column, name)| (*column, name.as_bytes().to_vec()))
            .collect();
        Ok(MaterializationPreparation {
            undo,
            destination,
            renames,
        })
    }

    fn begin_materialization(
        &mut self,
        transformation: ColumnTransformation,
        selected_columns: BTreeSet<usize>,
    ) -> Result<(), AppMessage> {
        let MaterializationPreparation {
            undo,
            destination,
            renames,
        } = self.prepare_materialization()?;
        let job = match self.session.start_create_working_copy(
            renames,
            self.cell_edits.clone(),
            transformation,
            destination.clone(),
        ) {
            Ok(job) => job,
            Err(QuarryError::SourceChanged) => {
                self.cleanup_empty_working_copy();
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(QuarryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.cleanup_empty_working_copy();
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(error) => {
                self.cleanup_empty_working_copy();
                return Err(AppMessage::error(error.to_string()));
            }
        };
        self.structural_job = Some(StructuralJob::Materializing {
            job,
            destination,
            selected_columns,
            undo,
        });
        self.structural_status = Some("Applying column edit…".into());
        self.structural_cancel_requested = false;
        Ok(())
    }

    #[cfg(test)]
    fn start_replace_all(&mut self, query: &[u8], replacement: &[u8]) -> Result<(), String> {
        self.start_replace_all_with_case(query, replacement, CaseSensitivity::Insensitive)
            .map_err(|message| message.text)
    }

    fn start_replace_all_with_case(
        &mut self,
        query: &[u8],
        replacement: &[u8],
        case_sensitivity: CaseSensitivity,
    ) -> Result<(), AppMessage> {
        if let Some(reason) = self.structural_edit_disabled_reason() {
            return Err(AppMessage::warning(reason));
        }
        if query.is_empty() {
            return Err(AppMessage::warning("Enter text to find."));
        }
        if query == replacement {
            self.structural_status = Some(
                "Search and replacement text are identical. The document was not changed.".into(),
            );
            return Ok(());
        }
        self.commit_edits();
        self.cancel_search();
        let selected_columns = self.selected_columns.clone();
        let MaterializationPreparation {
            undo,
            destination,
            renames,
        } = self.prepare_materialization()?;
        let job = match self.session.start_create_replaced_working_copy(
            renames,
            self.cell_edits.clone(),
            LiteralReplacement {
                needle: query.to_vec(),
                replacement: replacement.to_vec(),
                case_sensitivity,
            },
            destination.clone(),
        ) {
            Ok(job) => job,
            Err(QuarryError::SourceChanged) => {
                self.cleanup_empty_working_copy();
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(QuarryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.cleanup_empty_working_copy();
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(error) => {
                self.cleanup_empty_working_copy();
                return Err(AppMessage::error(error.to_string()));
            }
        };
        self.structural_job = Some(StructuralJob::Replacing {
            job,
            destination,
            selected_columns,
            undo,
        });
        self.structural_status = Some("Replacing matches…".into());
        self.structural_cancel_requested = false;
        Ok(())
    }

    fn accept_materialized_change(&mut self, undo: WorkingCopySnapshot) {
        let state = self
            .working_copy
            .as_mut()
            .expect("materialization owns a working-copy directory");
        let current_path = self.session.path().to_path_buf();
        for obsolete in [state.undo.take(), state.redo.take()].into_iter().flatten() {
            if obsolete.path != current_path && obsolete.path.starts_with(state.directory.path()) {
                let _ = std::fs::remove_file(obsolete.path);
            }
        }
        debug_assert_eq!(undo.path, current_path);
        state.undo = Some(undo);
        state.redo = None;
        self.structural_status = None;
    }

    fn poll_structural_edit(&mut self) -> Result<Option<MaterializedWorkingCopy>, AppMessage> {
        let Some(job) = self.structural_job.as_ref() else {
            return Ok(None);
        };
        let done = match job {
            StructuralJob::AnalyzingSplit { job, .. } => job.progress().done,
            StructuralJob::Materializing { job, .. } => job.progress().done,
            StructuralJob::Replacing { job, .. } => job.progress().done,
            StructuralJob::Sorting { job, .. } => job.progress().done,
            StructuralJob::DeletingRows { job, .. } => job.progress().done,
        };
        if !done {
            return Ok(None);
        }

        let job = self
            .structural_job
            .take()
            .expect("the completed structural job is present");
        self.structural_cancel_requested = false;
        match job {
            StructuralJob::AnalyzingSplit {
                job,
                source_column,
                separator,
            } => match job.wait() {
                Ok(SplitAnalysisOutcome::Complete(summary)) if summary.max_pieces >= 2 => {
                    let source_header = self.session.dialect.has_header.then(|| {
                        self.current_header_fields()
                            .get(source_column)
                            .cloned()
                            .unwrap_or_default()
                    });
                    let transformation = ColumnTransformation::split_with_blank_headers(
                        source_column,
                        separator,
                        summary.max_pieces,
                        source_header,
                    )
                    .map_err(|error| AppMessage::error(error.to_string()))?;
                    let selected_columns =
                        (source_column..source_column.saturating_add(summary.max_pieces)).collect();
                    self.begin_materialization(transformation, selected_columns)?;
                    Ok(None)
                }
                Ok(SplitAnalysisOutcome::Complete(_)) => {
                    self.structural_status = None;
                    Err(AppMessage::warning(
                        "The separator was not found in the selected column.",
                    ))
                }
                Ok(SplitAnalysisOutcome::Cancelled) => {
                    self.structural_status =
                        Some("Split cancelled. The document was not changed.".into());
                    Ok(None)
                }
                Err(QuarryError::SourceChanged) => {
                    self.invalidate_changed_source();
                    Err(AppMessage::warning(SOURCE_CHANGED_NOTICE))
                }
                Err(error) => {
                    self.structural_status =
                        Some("Split failed. The document was not changed.".into());
                    Err(AppMessage::error(error.to_string()))
                }
            },
            StructuralJob::Materializing {
                job,
                destination,
                selected_columns,
                undo,
            } => match job.wait() {
                Ok(SaveAsOutcome::Complete(summary)) => {
                    debug_assert_eq!(summary.destination, destination);
                    self.accept_materialized_change(undo);
                    Ok(Some(MaterializedWorkingCopy {
                        path: summary.destination,
                        selected_columns,
                        notice: "Column edit applied. Save to keep it, or discard changes.".into(),
                    }))
                }
                Ok(SaveAsOutcome::Cancelled) => {
                    let _ = std::fs::remove_file(destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status =
                        Some("Column edit cancelled. The document was not changed.".into());
                    Ok(None)
                }
                Err(QuarryError::SourceChanged) => {
                    let _ = std::fs::remove_file(destination);
                    self.invalidate_changed_source();
                    Err(AppMessage::warning(SOURCE_CHANGED_NOTICE))
                }
                Err(error) => {
                    let _ = std::fs::remove_file(destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status =
                        Some("Column edit failed. The document was not changed.".into());
                    Err(AppMessage::error(error.to_string()))
                }
            },
            StructuralJob::DeletingRows {
                job,
                destination,
                undo,
                count,
            } => match job.wait() {
                Ok(SaveAsOutcome::Complete(summary)) => {
                    debug_assert_eq!(summary.destination, destination);
                    self.accept_materialized_change(undo);
                    Ok(Some(MaterializedWorkingCopy {
                        path: summary.destination,
                        selected_columns: BTreeSet::new(),
                        notice: format!(
                            "Deleted {count} row{}. Save to keep it, or discard changes.",
                            if count == 1 { "" } else { "s" }
                        ),
                    }))
                }
                Ok(SaveAsOutcome::Cancelled) => {
                    let _ = std::fs::remove_file(destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status =
                        Some("Row deletion cancelled. The document was not changed.".into());
                    Ok(None)
                }
                Err(QuarryError::SourceChanged) => {
                    let _ = std::fs::remove_file(destination);
                    self.invalidate_changed_source();
                    Err(AppMessage::warning(SOURCE_CHANGED_NOTICE))
                }
                Err(error) => {
                    let _ = std::fs::remove_file(destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status =
                        Some("Row deletion failed. The document was not changed.".into());
                    Err(AppMessage::error(error.to_string()))
                }
            },
            StructuralJob::Replacing {
                job,
                destination,
                selected_columns,
                undo,
            } => match job.wait() {
                Ok(ReplaceAllOutcome::Complete(summary)) => {
                    debug_assert_eq!(summary.destination, destination);
                    self.accept_materialized_change(undo);
                    let count = summary.replacements;
                    Ok(Some(MaterializedWorkingCopy {
                        path: summary.destination,
                        selected_columns,
                        notice: format!(
                            "Replaced {count} occurrence{}. Save to keep it, or discard changes.",
                            if count == 1 { "" } else { "s" }
                        ),
                    }))
                }
                Ok(ReplaceAllOutcome::NoMatch) => {
                    let _ = std::fs::remove_file(destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status =
                        Some("No matches found. The document was not changed.".into());
                    Ok(None)
                }
                Ok(ReplaceAllOutcome::Cancelled) => {
                    let _ = std::fs::remove_file(destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status =
                        Some("Replace All cancelled. The document was not changed.".into());
                    Ok(None)
                }
                Err(QuarryError::SourceChanged) => {
                    let _ = std::fs::remove_file(destination);
                    self.invalidate_changed_source();
                    Err(AppMessage::warning(SOURCE_CHANGED_NOTICE))
                }
                Err(error) => {
                    let _ = std::fs::remove_file(destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status =
                        Some("Replace All failed. The document was not changed.".into());
                    Err(AppMessage::error(error.to_string()))
                }
            },
            StructuralJob::Sorting {
                job,
                destination,
                selected_columns,
                undo,
                column,
                direction,
            } => match job.wait() {
                Ok(SortOutcome::Complete(summary)) => {
                    debug_assert_eq!(summary.destination, destination);
                    self.accept_materialized_change(undo);
                    Ok(Some(MaterializedWorkingCopy {
                        path: summary.destination,
                        selected_columns,
                        notice: format!(
                            "Sorted rows by column {} {}. Save to keep it, or discard changes.",
                            column.saturating_add(1),
                            sort_direction_label(direction).to_lowercase()
                        ),
                    }))
                }
                Ok(SortOutcome::Cancelled) => {
                    let _ = std::fs::remove_file(destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status =
                        Some("Sort cancelled. The document was not changed.".into());
                    Ok(None)
                }
                Err(QuarryError::SourceChanged) => {
                    let _ = std::fs::remove_file(destination);
                    self.invalidate_changed_source();
                    Err(AppMessage::warning(SOURCE_CHANGED_NOTICE))
                }
                Err(error) => {
                    let _ = std::fs::remove_file(destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status =
                        Some("Sort failed. The document was not changed.".into());
                    Err(AppMessage::error(error.to_string()))
                }
            },
        }
    }

    fn cancel_structural_edit(&mut self) {
        let Some(job) = self.structural_job.as_ref() else {
            return;
        };
        match job {
            StructuralJob::AnalyzingSplit { job, .. } => job.cancel(),
            StructuralJob::Materializing { job, .. } => job.cancel(),
            StructuralJob::Replacing { job, .. } => job.cancel(),
            StructuralJob::Sorting { job, .. } => job.cancel(),
            StructuralJob::DeletingRows { job, .. } => job.cancel(),
        }
        self.structural_cancel_requested = true;
        self.structural_status = Some("Cancelling change…".into());
    }

    fn cleanup_empty_working_copy(&mut self) {
        let empty = self.working_copy.as_ref().is_some_and(|state| {
            state.undo.is_none() && state.redo.is_none() && self.session.path() == self.logical_path
        });
        if empty {
            self.working_copy = None;
            self.original_session = None;
        }
    }

    fn invalidate_structural_redo(&mut self) {
        let Some(state) = self.working_copy.as_mut() else {
            return;
        };
        let Some(redo) = state.redo.take() else {
            return;
        };
        if redo.path != self.session.path() && redo.path.starts_with(state.directory.path()) {
            let _ = std::fs::remove_file(redo.path);
        }
    }

    fn structural_progress(&self) -> Option<StructuralProgressDisplay> {
        let job = self.structural_job.as_ref()?;
        let (bytes_scanned, total_bytes, done, operation, sorting) = match job {
            StructuralJob::AnalyzingSplit { job, .. } => {
                let progress = job.progress();
                (
                    progress.bytes_scanned,
                    progress.total_bytes,
                    progress.done,
                    "Checking split width",
                    false,
                )
            }
            StructuralJob::Materializing { job, .. } => {
                let progress = job.progress();
                (
                    progress.bytes_scanned,
                    progress.total_bytes,
                    progress.done,
                    "Applying column edit",
                    false,
                )
            }
            StructuralJob::Replacing { job, .. } => {
                let progress = job.progress();
                (
                    progress.bytes_scanned,
                    progress.total_bytes,
                    progress.done,
                    "Replacing matches",
                    false,
                )
            }
            StructuralJob::Sorting { job, .. } => {
                let progress = job.progress();
                (
                    progress.bytes_scanned,
                    progress.total_bytes,
                    progress.done,
                    "Sorting rows",
                    true,
                )
            }
            StructuralJob::DeletingRows { job, .. } => {
                let progress = job.progress();
                (
                    progress.bytes_scanned,
                    progress.total_bytes,
                    progress.done,
                    "Deleting selected rows",
                    false,
                )
            }
        };
        if sorting && let Some(progress) = sort_merge_progress(bytes_scanned, total_bytes, done) {
            return Some(progress);
        }
        let fraction = if total_bytes == 0 {
            if done { 1.0 } else { 0.0 }
        } else {
            (bytes_scanned as f32 / total_bytes as f32).clamp(0.0, 1.0)
        };
        Some(StructuralProgressDisplay {
            fraction,
            label: format!("{operation} · {:.1}%", fraction * 100.0),
            animate: false,
        })
    }

    fn start_indexing(&mut self) -> Result<(), String> {
        let job = self
            .session
            .start_indexing(IndexConfig::default())
            .map_err(|error| error.to_string())?;
        self.progress = job.progress();
        self.job = Some(job);
        self.last_poll = Instant::now() - POLL_INTERVAL;
        Ok(())
    }

    fn poll(&mut self) -> Result<(), String> {
        let Some(job) = &self.job else {
            return Ok(());
        };
        let progress = job.progress();
        if self.last_poll.elapsed() < POLL_INTERVAL && !progress.done {
            return Ok(());
        }
        self.last_poll = Instant::now();
        self.progress = progress;
        if self.progress.done {
            let job = self.job.take().expect("index job is present");
            self.index = Some(job.wait().map_err(|error| error.to_string())?);
            self.load_buffer(self.viewport_start)?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn start_find_next(&mut self, query: &[u8]) -> Result<(), String> {
        self.start_find_next_with_case(query, CaseSensitivity::Insensitive)
    }

    fn start_find_next_with_case(
        &mut self,
        query: &[u8],
        case_sensitivity: CaseSensitivity,
    ) -> Result<(), String> {
        if query.is_empty() {
            return Err("Enter text to find.".into());
        }
        if self.filter_active() {
            return Err("Clear the filter before using Find.".into());
        }
        if self.search_job.is_some() {
            return Err("A search is already running.".into());
        }
        if !self.is_search_ready() {
            return Err("Search is available after indexing completes.".into());
        }
        self.commit_edits();
        if self.search_query != query || self.search_case_sensitivity != case_sensitivity {
            self.search_query.clear();
            self.search_query.extend_from_slice(query);
            self.search_case_sensitivity = case_sensitivity;
            self.last_match = None;
            self.search_history.clear();
            self.search_history_index = None;
        }
        if let Some(index) = self.search_history_index
            && let Some(found) = self.search_history.get(index.saturating_add(1)).copied()
        {
            self.search_history_index = Some(index + 1);
            return self.show_search_match(found);
        }
        let position = self.last_match.as_ref().map_or(
            SearchPosition {
                row: self.viewport_start,
                column: 0,
            },
            |found| SearchPosition {
                row: found.row,
                column: found.column.saturating_add(1),
            },
        );
        let index = self.index.as_ref().expect("index was checked above");
        self.search_job = Some(
            self.session
                .start_search_with_cell_edits_and_case(
                    index,
                    query.to_vec(),
                    position,
                    self.cell_edits.clone(),
                    case_sensitivity,
                )
                .map_err(|error| error.to_string())?,
        );
        self.search_status = None;
        self.reveal_cell = None;
        Ok(())
    }

    fn can_find_previous_with_case(&self, query: &[u8], case_sensitivity: CaseSensitivity) -> bool {
        !query.is_empty()
            && self.search_job.is_none()
            && !self.filter_active()
            && self.is_search_ready()
            && self.search_query == query
            && self.search_case_sensitivity == case_sensitivity
            && match self.search_history_index {
                Some(index) => index > 0,
                None => !self.search_history.is_empty(),
            }
    }

    fn start_find_previous_with_case(
        &mut self,
        query: &[u8],
        case_sensitivity: CaseSensitivity,
    ) -> Result<(), String> {
        self.commit_edits();
        if !self.can_find_previous_with_case(query, case_sensitivity) {
            return Err("Find at least two matches before moving back.".into());
        }
        let index = self.search_history_index.map_or_else(
            || self.search_history.len().checked_sub(1),
            |index| index.checked_sub(1),
        );
        let index = index.expect("a previous match was checked above");
        let found = self.search_history[index];
        self.search_history_index = Some(index);
        self.show_search_match(found)
    }

    #[cfg(test)]
    fn can_replace_current(&self, query: &[u8]) -> bool {
        self.can_replace_current_with_case(query, CaseSensitivity::Insensitive)
    }

    fn can_replace_current_with_case(
        &self,
        query: &[u8],
        case_sensitivity: CaseSensitivity,
    ) -> bool {
        if query.is_empty()
            || self.search_job.is_some()
            || self.save_job.is_some()
            || self.structural_job.is_some()
            || self.cell_edit.is_some()
            || self.header_edit.is_some()
            || self.search_query != query
            || self.search_case_sensitivity != case_sensitivity
        {
            return false;
        }
        let Some(found) = self.last_match.as_ref() else {
            return false;
        };
        matches!(
            self.selection,
            Some(GridSelection::Cell { row, column })
                if row == found.row && column == found.column
        ) && self
            .effective_cell(found.row, found.column)
            .is_some_and(|value| literal_contains(value, query, case_sensitivity))
    }

    #[cfg(test)]
    fn replace_current_match(&mut self, query: &[u8], replacement: &[u8]) -> Result<(), String> {
        self.replace_current_match_with_case(query, replacement, CaseSensitivity::Insensitive)
    }

    fn replace_current_match_with_case(
        &mut self,
        query: &[u8],
        replacement: &[u8],
        case_sensitivity: CaseSensitivity,
    ) -> Result<(), String> {
        if !self.can_replace_current_with_case(query, case_sensitivity) {
            return Err("Find a current match before replacing it.".into());
        }
        let found = self
            .last_match
            .expect("replaceable match was checked above");
        let key = (found.row, found.column);
        let source = self
            .source_cell(found.row, found.column)
            .ok_or_else(|| {
                "The current matched cell is no longer available for replacement.".to_owned()
            })?
            .to_vec();
        let effective = self
            .cell_edits
            .get(&key)
            .map_or(source.as_slice(), Vec::as_slice);
        let next = replace_literal_all_with_case(effective, query, replacement, case_sensitivity)
            .expect("replaceable match contains the query");
        if next.as_slice() != effective {
            self.invalidate_structural_redo();
            if next == source {
                self.cell_edits.remove(&key);
            } else {
                self.cell_edits.insert(key, next);
            }
        }
        self.selection = Some(GridSelection::Cell {
            row: found.row,
            column: found.column,
        });
        self.selected_rows.clear();
        if let Some(index) = self.search_history_index.take() {
            self.search_history.truncate(index);
        }
        self.start_find_next_with_case(query, case_sensitivity)
    }

    fn show_search_match(&mut self, found: SearchMatch) -> Result<(), String> {
        let row = found.row;
        let column = found.column;
        self.navigate(row)?;
        self.center_column(column);
        self.reveal_cell = Some((row, column));
        self.selection = Some(GridSelection::Cell { row, column });
        self.selected_rows.clear();
        self.selected_columns.clear();
        self.column_selection_anchor = None;
        self.search_status = Some(format!(
            "Found row {}, column {}.",
            row.saturating_sub(self.data_start).saturating_add(1),
            column.saturating_add(1)
        ));
        self.last_match = Some(found);
        Ok(())
    }

    fn poll_search(&mut self) -> Result<(), String> {
        let Some(job) = &self.search_job else {
            return Ok(());
        };
        if !job.progress().done {
            return Ok(());
        }
        let job = self.search_job.take().expect("search job is present");
        match job.wait().map_err(|error| error.to_string())? {
            SearchOutcome::Match(found) => {
                if let Some(index) = self.search_history_index {
                    self.search_history.truncate(index + 1);
                }
                self.search_history.push(found);
                self.search_history_index = self.search_history.len().checked_sub(1);
                self.show_search_match(found)?;
            }
            SearchOutcome::NotFound => {
                self.search_status = Some("No further matches.".into());
            }
            SearchOutcome::Cancelled => {
                self.search_status = Some("Search cancelled.".into());
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn search_progress(&self) -> Option<SearchProgress> {
        self.search_job.as_ref().map(SearchJob::progress)
    }

    fn start_filter(&mut self, query: FilterQuery) -> Result<(), String> {
        if self.export_job.is_some() {
            return Err("Cancel the active export before changing filters.".into());
        }
        if self.filter_job.is_some() {
            return Err("A filter is already running.".into());
        }
        if self.search_job.is_some() {
            return Err("Cancel the active search before filtering.".into());
        }
        if self.has_cell_edits() {
            return Err("Save or discard cell edits before filtering the source file.".into());
        }
        if !self.is_filter_ready() {
            return Err("Open a file with at least one column before filtering.".into());
        }
        if query.predicates.is_empty() {
            return Err("Add at least one filter rule.".into());
        }
        for (index, predicate) in query.predicates.iter().enumerate() {
            self.validate_column(predicate.column)
                .map_err(|error| format!("Rule {}: {error}", index + 1))?;
            if predicate.operator == FilterOperator::Contains && predicate.value.is_empty() {
                return Err(format!(
                    "Rule {}: enter text for a Contains filter.",
                    index + 1
                ));
            }
        }

        self.commit_edits();
        let job = self
            .session
            .start_filter(query.clone())
            .map_err(|error| error.to_string())?;
        self.stop_filter_read();
        self.filter_progress = Some(job.progress());
        self.filter_index = Some(job.snapshot());
        self.filter_job = Some(job);
        self.filter_query = Some(query);
        self.filter_status = Some("Filtering rows…".into());
        self.export_progress = None;
        self.export_status = None;
        self.export_cancel_requested = false;
        self.filter_viewport_start = 0;
        self.filter_buffer_start = 0;
        self.filtered_rows.clear();
        self.selection = None;
        self.selected_rows.clear();
        self.reveal_cell = None;
        self.scroll_points = 0.0;
        Ok(())
    }

    fn poll_filter(&mut self) -> Result<(), String> {
        self.poll_filter_scan()?;
        self.poll_filter_read()
    }

    fn poll_filter_scan(&mut self) -> Result<(), String> {
        let Some(job) = self.filter_job.as_ref() else {
            return Ok(());
        };
        let progress = job.progress();
        self.filter_progress = Some(progress);

        if progress.matches_found > 0 {
            let required = self
                .filter_viewport_start
                .saturating_add(self.visible_rows as u64)
                .min(progress.matches_found);
            self.refresh_filter_snapshot_for(required);
            self.navigate_filter(self.filter_viewport_start)?;
        } else {
            self.filtered_rows.clear();
            self.filter_buffer_start = 0;
            self.filter_viewport_start = 0;
        }
        if !progress.done {
            return Ok(());
        }

        let job = self.filter_job.take().expect("filter job is present");
        match job.wait() {
            Ok(index) => {
                let matches = index.matches_found();
                self.filter_index = Some(index);
                self.filter_status = Some(if progress.cancelled {
                    format!("Filter cancelled after {matches} matches.")
                } else if matches == 0 {
                    "Filter complete. No matching rows.".into()
                } else {
                    format!("Filter complete. {matches} matches.")
                });
                if matches > 0 {
                    self.navigate_filter(self.filter_viewport_start)?;
                }
                Ok(())
            }
            Err(error) => {
                self.stop_filter_read();
                self.filter_index = None;
                self.filter_query = None;
                self.filter_progress = None;
                self.filter_status = None;
                self.filtered_rows.clear();
                self.filter_viewport_start = 0;
                self.filter_buffer_start = 0;
                Err(error.to_string())
            }
        }
    }

    fn poll_filter_read(&mut self) -> Result<(), String> {
        let done = self
            .filter_read
            .as_ref()
            .is_some_and(|active| active.job.progress().done);
        if !done {
            return Ok(());
        }

        let active = self
            .filter_read
            .take()
            .expect("completed filter read should be active");
        let desired = self.filter_read_window(self.filter_viewport_start);
        match active.job.wait() {
            Ok(FilterReadOutcome::Complete(rows)) if desired == Some(active.window) => {
                let loaded_columns = rows.iter().map(|row| row.fields.len()).max().unwrap_or(0);
                if loaded_columns > self.total_columns {
                    self.ensure_column_count(loaded_columns);
                    self.refresh_column_headers();
                }
                self.last_viewport_read = Some(active.started.elapsed());
                self.filter_buffer_start = active.window.start_match;
                self.filtered_rows = rows;
            }
            Ok(FilterReadOutcome::Complete(_) | FilterReadOutcome::Cancelled) => {}
            Err(error) => {
                self.pending_filter_read = None;
                return Err(error.to_string());
            }
        }

        self.schedule_filter_buffer(self.filter_viewport_start)
    }

    fn start_filtered_export(&mut self, destination: PathBuf) -> Result<(), AppMessage> {
        if self.save_job.is_some() {
            return Err(AppMessage::warning(
                "Cancel the active save before exporting filtered rows.",
            ));
        }
        self.commit_edits();
        if self.is_dirty() {
            return Err(AppMessage::warning(
                "Save or discard your changes before exporting filtered rows.",
            ));
        }
        if self.export_job.is_some() {
            return Err(AppMessage::warning("A filtered export is already running."));
        }
        let query = self
            .filter_query
            .clone()
            .ok_or_else(|| AppMessage::warning("Apply a filter before exporting rows."))?;
        let progress = self.filter_progress().ok_or_else(|| {
            AppMessage::warning("Wait for filtering to complete before exporting.")
        })?;
        if self.filter_job.is_some() || !progress.done {
            return Err(AppMessage::warning(
                "Wait for filtering to complete before exporting.",
            ));
        }
        if progress.cancelled {
            return Err(AppMessage::warning(
                "Run the filter to completion before exporting.",
            ));
        }

        let job = self
            .session
            .start_filtered_export(query, destination)
            .map_err(|error| AppMessage::error(error.to_string()))?;
        self.export_progress = Some(job.progress());
        self.export_job = Some(job);
        self.export_status = Some(
            "Export in progress. To open or reapply a file, cancel this export and wait for it to finish."
                .into(),
        );
        self.export_cancel_requested = false;
        Ok(())
    }

    fn poll_filtered_export(&mut self) -> Result<(), String> {
        let Some(job) = self.export_job.as_ref() else {
            return Ok(());
        };
        let progress = job.progress();
        self.export_progress = Some(progress);
        if !progress.done {
            return Ok(());
        }

        let job = self
            .export_job
            .take()
            .expect("filter export job is present");
        self.export_cancel_requested = false;
        match job.wait() {
            Ok(FilterExportOutcome::Complete(summary)) => {
                self.export_status = Some(format!(
                    "Export complete: {} rows ({}) saved to {}.",
                    summary.rows_written,
                    format_bytes(summary.bytes_written),
                    summary.destination.display()
                ));
                Ok(())
            }
            Ok(FilterExportOutcome::Cancelled) => {
                self.export_status = Some("Export cancelled. No output file was created.".into());
                Ok(())
            }
            Err(error) => {
                self.export_status = Some("Export failed. No output file was created.".into());
                Err(error.to_string())
            }
        }
    }

    fn cancel_filtered_export(&mut self) {
        if let Some(job) = &self.export_job {
            job.cancel();
            self.export_cancel_requested = true;
            self.export_status = Some(
                "Cancelling export… Wait for it to finish before opening or reapplying the file."
                    .into(),
            );
        }
    }

    fn is_filtered_export_ready(&self) -> bool {
        self.filter_query.is_some()
            && !self.has_cell_edits()
            && self.filter_job.is_none()
            && self.export_job.is_none()
            && self.save_job.is_none()
            && self
                .filter_progress
                .is_some_and(|progress| progress.done && !progress.cancelled)
    }

    fn is_save_ready(&self) -> bool {
        !self.source_changed
            && self.is_dirty()
            && self.save_job.is_none()
            && self.export_job.is_none()
            && self.structural_job.is_none()
    }

    fn start_save(&mut self) -> Result<(), AppMessage> {
        self.start_save_operation(None)
    }

    fn start_save_as(&mut self, destination: PathBuf) -> Result<(), AppMessage> {
        self.start_save_operation(Some(destination))
    }

    fn start_save_operation(&mut self, destination: Option<PathBuf>) -> Result<(), AppMessage> {
        if self.source_changed {
            return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
        }
        if self.save_job.is_some() {
            return Err(AppMessage::warning("A save operation is already running."));
        }
        if self.export_job.is_some() {
            return Err(AppMessage::warning(
                "Cancel the active export before saving.",
            ));
        }
        self.commit_edits();
        if !self.is_dirty() {
            return Err(AppMessage::warning("Make a change before saving."));
        }
        let renames = self
            .header_renames
            .iter()
            .map(|(column, name)| (*column, name.as_bytes().to_vec()))
            .collect();
        let saving_in_place = destination.is_none();
        let result = match destination {
            Some(destination) => {
                self.session
                    .start_save_as_with_edits(renames, self.cell_edits.clone(), destination)
            }
            None if self.has_structural_edits() => self.session.start_save_to_original(
                renames,
                self.cell_edits.clone(),
                self.original_session
                    .as_ref()
                    .expect("a structural working copy retains the original source guard"),
            ),
            None => self
                .session
                .start_save_with_edits(renames, self.cell_edits.clone()),
        };
        let job = match result {
            Ok(job) => job,
            Err(QuarryError::SourceChanged) => {
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(error) => return Err(AppMessage::error(error.to_string())),
        };
        self.save_job = Some(job);
        self.save_status = Some("Saving edited file…".into());
        self.save_cancel_requested = false;
        self.saving_in_place = saving_in_place;
        Ok(())
    }

    fn poll_save(&mut self) -> Result<Option<(PathBuf, bool)>, AppMessage> {
        let Some(job) = self.save_job.as_ref() else {
            return Ok(None);
        };
        let progress = job.progress();
        if !progress.done {
            return Ok(None);
        }

        let job = self.save_job.take().expect("save job is present");
        let in_place = self.saving_in_place;
        self.save_cancel_requested = false;
        self.saving_in_place = false;
        match job.wait() {
            Ok(SaveAsOutcome::Complete(summary)) => Ok(Some((summary.destination, in_place))),
            Ok(SaveAsOutcome::Cancelled) => {
                self.save_status = Some(if in_place {
                    "Save cancelled. Quarry did not replace the current file.".into()
                } else {
                    "Save As cancelled. No output file was created.".into()
                });
                Ok(None)
            }
            Err(QuarryError::SourceChanged) => {
                self.invalidate_changed_source();
                Err(AppMessage::warning(SOURCE_CHANGED_NOTICE))
            }
            Err(error) => {
                self.save_status = Some(if in_place {
                    "Save failed. Quarry did not replace the current file.".into()
                } else {
                    "Save As failed. No output file was created.".into()
                });
                Err(AppMessage::error(error.to_string()))
            }
        }
    }

    fn invalidate_changed_source(&mut self) {
        self.shutdown();
        self.source_changed = true;
        self.index = None;
        self.filter_index = None;
        self.filter_query = None;
        self.filter_progress = None;
        self.buffered_rows.clear();
        self.filtered_rows.clear();
        self.selection = None;
        self.selected_rows.clear();
        self.reveal_cell = None;
        self.save_status = Some(SOURCE_CHANGED_NOTICE.into());
    }

    fn cancel_save(&mut self) {
        if let Some(job) = &self.save_job {
            job.cancel();
            self.save_cancel_requested = true;
            self.save_status = Some(if self.saving_in_place {
                "Cancelling Save…".into()
            } else {
                "Cancelling Save As…".into()
            });
        }
    }

    fn cancel_filter(&self) {
        if let Some(job) = &self.filter_job {
            job.cancel();
        }
    }

    fn clear_filter(&mut self) -> Result<(), String> {
        if self.export_job.is_some() {
            return Err("Cancel the active export before clearing filters.".into());
        }
        self.stop_filter_read();
        if let Some(job) = self.filter_job.take() {
            job.cancel();
            drop(job);
        }
        self.filter_index = None;
        self.filter_query = None;
        self.filter_progress = None;
        self.filter_status = None;
        self.export_progress = None;
        self.export_status = None;
        self.export_cancel_requested = false;
        self.filter_viewport_start = 0;
        self.filter_buffer_start = 0;
        self.filtered_rows.clear();
        self.selection = None;
        self.selected_rows.clear();
        self.reveal_cell = None;
        self.scroll_points = 0.0;
        if self.available_data_rows() > 0 {
            self.navigate(self.viewport_start)?;
        }
        Ok(())
    }

    fn filter_active(&self) -> bool {
        self.filter_query.is_some()
    }

    fn is_filter_ready(&self) -> bool {
        self.total_columns > 0 && self.structural_job.is_none()
    }

    fn filter_progress(&self) -> Option<FilterProgress> {
        self.filter_job
            .as_ref()
            .map(FilterJob::progress)
            .or(self.filter_progress)
    }

    fn filter_rows_loading(&self) -> bool {
        self.filter_read.is_some() || self.pending_filter_read.is_some()
    }

    fn available_filter_rows(&self) -> u64 {
        self.filter_progress
            .map(|progress| progress.matches_found)
            .unwrap_or(0)
            .max(
                self.filter_index
                    .as_ref()
                    .map(FilterIndex::matches_found)
                    .unwrap_or(0),
            )
    }

    fn refresh_filter_snapshot_for(&mut self, required_matches: u64) {
        let indexed = self
            .filter_index
            .as_ref()
            .map(FilterIndex::matches_found)
            .unwrap_or(0);
        if indexed >= required_matches {
            return;
        }
        if let Some(job) = &self.filter_job {
            self.filter_index = Some(job.snapshot());
        }
    }

    fn is_search_ready(&self) -> bool {
        self.index.is_some()
            && self.progress.done
            && !self.progress.cancelled
            && self.structural_job.is_none()
    }

    fn cancel_search(&self) {
        if let Some(job) = &self.search_job {
            job.cancel();
        }
    }

    fn center_column(&mut self, column: usize) {
        self.commit_edits();
        self.ensure_column_count(column.saturating_add(1));
        self.columns.view(column);
        self.refresh_column_headers();
        self.clear_hidden_selection();
    }

    #[cfg(test)]
    fn view_column(&mut self, column: usize) -> Result<(), String> {
        self.commit_edits();
        self.validate_column(column)?;
        self.columns.view(column);
        self.refresh_column_headers();
        self.reveal_cell = Some((self.current_source_row(), column));
        self.clear_hidden_selection();
        Ok(())
    }

    fn set_column_shown(&mut self, column: usize, shown: bool) -> Result<(), String> {
        self.commit_edits();
        self.validate_column(column)?;
        self.columns.set_shown(column, shown);
        self.refresh_column_headers();
        if !shown
            && self
                .reveal_cell
                .is_some_and(|(_, revealed)| revealed == column)
        {
            self.reveal_cell = None;
        }
        self.clear_hidden_selection();
        Ok(())
    }

    fn move_column(&mut self, column: usize, position: usize) -> Result<(), String> {
        self.commit_edits();
        self.validate_column(column)?;
        if position >= self.total_columns {
            return Err(format!(
                "Display position must be between 1 and {}.",
                self.total_columns
            ));
        }
        self.columns.move_column(column, position);
        self.refresh_column_headers();
        self.clear_hidden_selection();
        Ok(())
    }

    fn reset_columns(&mut self) {
        self.commit_edits();
        self.columns.reset();
        self.refresh_column_headers();
        self.reveal_cell = self
            .columns
            .visible
            .first()
            .copied()
            .map(|column| (self.current_source_row(), column));
        self.clear_hidden_selection();
    }

    fn ensure_column_count(&mut self, total_columns: usize) {
        if total_columns > self.total_columns {
            self.total_columns = total_columns;
            self.columns.extend_to(total_columns);
        }
    }

    fn refresh_column_headers(&mut self) {
        self.headers = self
            .columns
            .visible
            .iter()
            .map(|column| self.column_name(*column))
            .collect();
    }

    fn column_name(&self, column: usize) -> String {
        if let Some(edit) = self
            .header_edit
            .as_ref()
            .filter(|edit| edit.column == column)
        {
            return field_text(edit.draft.as_bytes());
        }
        self.header_renames
            .get(&column)
            .map(|name| field_text(name.as_bytes()))
            .unwrap_or_else(|| column_name(&self.session, column))
    }

    fn current_header_fields(&self) -> Vec<Vec<u8>> {
        let mut fields = self
            .session
            .first_rows
            .first()
            .map(|row| row.fields.clone())
            .unwrap_or_default();
        for (column, name) in &self.header_renames {
            if let Some(field) = fields.get_mut(*column) {
                *field = name.as_bytes().to_vec();
            }
        }
        if let Some(edit) = &self.header_edit
            && let Some(field) = fields.get_mut(edit.column)
        {
            *field = edit.draft.as_bytes().to_vec();
        }
        fields
    }

    fn header_is_editable(&self, column: usize) -> bool {
        self.save_job.is_none()
            && self.export_job.is_none()
            && self.search_job.is_none()
            && self.structural_job.is_none()
            && self.source_header_name(column).is_some()
    }

    fn source_header_name(&self, column: usize) -> Option<&str> {
        if !self.session.dialect.has_header {
            return None;
        }
        let field = self.session.first_rows.first()?.fields.get(column)?;
        std::str::from_utf8(field).ok()
    }

    fn begin_header_edit(&mut self, column: usize) {
        if !self.header_is_editable(column) {
            return;
        }
        if let Some(source_name) = self.source_header_name(column) {
            let draft = self
                .header_renames
                .get(&column)
                .cloned()
                .unwrap_or_else(|| source_name.to_owned());
            self.commit_cell_edit();
            self.commit_header_edit();
            self.header_edit = Some(HeaderEdit {
                column,
                draft,
                focus_requested: true,
            });
        }
    }

    fn rename_header(&mut self, column: usize, name: String) -> Result<(), String> {
        if self.save_job.is_some() {
            return Err("Wait for the save to finish before editing headers.".into());
        }
        if self.export_job.is_some() {
            return Err("Wait for the filtered export to finish before editing headers.".into());
        }
        if self.search_job.is_some() {
            return Err("Wait for the search to finish before editing headers.".into());
        }
        if !self.header_is_editable(column) {
            return Err("Only columns in the source header row can be renamed.".into());
        }
        let source_name = self
            .source_header_name(column)
            .expect("editable header has source text");
        let next = (name.as_bytes() != source_name.as_bytes()).then_some(name);
        if self.header_renames.get(&column) != next.as_ref() {
            self.invalidate_structural_redo();
            match next {
                Some(name) => {
                    self.header_renames.insert(column, name);
                }
                None => {
                    self.header_renames.remove(&column);
                }
            }
            self.refresh_column_headers();
        }
        Ok(())
    }

    fn commit_header_edit(&mut self) {
        if self.save_job.is_some() {
            return;
        }
        if let Some(edit) = self.header_edit.take() {
            let _ = self.rename_header(edit.column, edit.draft);
        }
    }

    fn cell_is_editable(&self, source: Option<&[u8]>) -> bool {
        self.cell_edit_disabled_reason(source).is_none()
    }

    fn cell_edit_disabled_reason(&self, source: Option<&[u8]>) -> Option<&'static str> {
        if self.source_changed {
            Some("Reopen the changed source before editing data cells.")
        } else if self.filter_active() {
            Some("Clear the filter before editing data cells.")
        } else if self.save_job.is_some()
            || self.export_job.is_some()
            || self.search_job.is_some()
            || self.filter_job.is_some()
            || self.structural_job.is_some()
        {
            Some("Wait for the active file operation before editing data cells.")
        } else if source.is_none() {
            Some("This row has no cell in this file column.")
        } else if source.is_some_and(|source| std::str::from_utf8(source).is_err()) {
            Some("This cell is not valid UTF-8 and cannot be edited.")
        } else {
            None
        }
    }

    fn begin_cell_edit(&mut self, row: u64, column: usize, source: Vec<u8>) -> Result<(), String> {
        if self.filter_active() {
            return Err("Clear the filter before editing data cells.".into());
        }
        if let Some(reason) = self.cell_edit_disabled_reason(Some(&source)) {
            return Err(reason.into());
        }
        self.commit_header_edit();
        self.commit_cell_edit();
        let draft = self
            .cell_edits
            .get(&(row, column))
            .map_or(source.as_slice(), Vec::as_slice);
        let draft = std::str::from_utf8(draft)
            .expect("editable cell text was checked as UTF-8")
            .to_owned();
        self.selection = Some(GridSelection::Cell { row, column });
        self.selected_rows.clear();
        self.cell_edit = Some(CellEdit {
            row,
            column,
            source,
            draft,
            focus_requested: true,
        });
        Ok(())
    }

    fn commit_cell_edit(&mut self) {
        if self.save_job.is_some() {
            return;
        }
        let Some(edit) = self.cell_edit.take() else {
            return;
        };
        let key = (edit.row, edit.column);
        let next =
            (edit.draft.as_bytes() != edit.source.as_slice()).then(|| edit.draft.into_bytes());
        if self.cell_edits.get(&key) == next.as_ref() {
            return;
        }
        self.invalidate_structural_redo();
        match next {
            Some(value) => {
                self.cell_edits.insert(key, value);
            }
            None => {
                self.cell_edits.remove(&key);
            }
        }
        self.search_query.clear();
        self.last_match = None;
        self.search_history.clear();
        self.search_history_index = None;
        self.search_status = None;
        self.reveal_cell = None;
    }

    fn commit_edits(&mut self) {
        self.commit_header_edit();
        self.commit_cell_edit();
    }

    fn discard_header_edits(&mut self) {
        if self.save_job.is_some() {
            return;
        }
        self.header_edit = None;
        self.header_renames.clear();
        self.refresh_column_headers();
    }

    fn discard_edits(&mut self) {
        if self.save_job.is_some() {
            return;
        }
        self.discard_header_edits();
        self.cell_edit = None;
        self.cell_edits.clear();
        self.selection = None;
        self.selected_rows.clear();
        self.reveal_cell = None;
        self.search_query.clear();
        self.last_match = None;
        self.search_history.clear();
        self.search_history_index = None;
        self.search_status = None;
        self.cell_focus_requested = None;
        self.column_focus_requested = None;
        if self.session.path() == self.logical_path {
            self.working_copy = None;
            self.original_session = None;
        }
    }

    fn has_cell_edits(&self) -> bool {
        !self.cell_edits.is_empty()
            || self
                .cell_edit
                .as_ref()
                .is_some_and(|edit| edit.draft.as_bytes() != edit.source.as_slice())
    }

    fn is_dirty(&self) -> bool {
        let header_dirty = self.header_edit.as_ref().map_or_else(
            || !self.header_renames.is_empty(),
            |edit| {
                self.header_renames
                    .keys()
                    .any(|column| *column != edit.column)
                    || edit.draft.as_bytes()
                        != self
                            .source_header_name(edit.column)
                            .expect("active header edit has source text")
                            .as_bytes()
            },
        );
        header_dirty || self.has_cell_edits() || self.has_structural_edits()
    }

    fn has_structural_edits(&self) -> bool {
        self.session.path() != self.logical_path
    }

    fn can_undo_structural(&self) -> bool {
        self.working_copy
            .as_ref()
            .is_some_and(|state| state.undo.is_some())
            && self.header_renames.is_empty()
            && !self.has_cell_edits()
            && self.header_edit.is_none()
            && self.cell_edit.is_none()
            && self.save_job.is_none()
            && self.export_job.is_none()
            && self.structural_job.is_none()
    }

    fn can_redo_structural(&self) -> bool {
        self.working_copy
            .as_ref()
            .is_some_and(|state| state.redo.is_some())
            && self.header_edit.is_none()
            && self.cell_edit.is_none()
            && self.save_job.is_none()
            && self.export_job.is_none()
            && self.structural_job.is_none()
    }

    fn validate_column(&self, column: usize) -> Result<(), String> {
        if column < self.total_columns {
            Ok(())
        } else if self.total_columns == 0 {
            Err("This file has no columns.".into())
        } else {
            Err(format!(
                "File column must be between 1 and {}.",
                self.total_columns
            ))
        }
    }

    fn index_status(&self) -> &'static str {
        if self.source_changed {
            "Source changed"
        } else if self.job.is_none() && self.index.is_none() {
            "Index failed"
        } else if self.progress.cancelled {
            "Index cancelled"
        } else if self.job.is_some() {
            "Indexing"
        } else {
            "Index complete"
        }
    }

    fn shutdown(&mut self) {
        self.stop_filter_read();
        if let Some(job) = self.structural_job.take() {
            match job {
                StructuralJob::AnalyzingSplit { job, .. } => job.cancel(),
                StructuralJob::Materializing { job, .. } => job.cancel(),
                StructuralJob::Replacing { job, .. } => job.cancel(),
                StructuralJob::Sorting { job, .. } => job.cancel(),
                StructuralJob::DeletingRows { job, .. } => job.cancel(),
            }
        }
        self.structural_cancel_requested = false;
        if let Some(job) = self.save_job.take() {
            job.cancel();
            drop(job);
        }
        self.save_cancel_requested = false;
        self.saving_in_place = false;
        if let Some(job) = self.export_job.take() {
            job.cancel();
            drop(job);
        }
        self.export_cancel_requested = false;
        if let Some(job) = self.filter_job.take() {
            job.cancel();
            drop(job);
        }
        if let Some(job) = self.search_job.take() {
            job.cancel();
            drop(job);
        }
        if let Some(job) = self.job.take() {
            drop(job);
        }
    }

    fn stop_filter_read(&mut self) {
        self.pending_filter_read = None;
        if let Some(active) = self.filter_read.take() {
            active.job.cancel_without_waiting();
        }
    }

    fn cancel(&self) {
        if let Some(job) = &self.job {
            job.cancel();
        }
    }

    fn available_rows(&self) -> u64 {
        self.index
            .as_ref()
            .map(StructuralIndex::indexed_rows)
            .unwrap_or(self.progress.rows_scanned)
            .max(self.session.first_rows.len() as u64)
    }

    fn available_data_rows(&self) -> u64 {
        self.available_rows().saturating_sub(self.data_start)
    }

    #[cfg(test)]
    fn indexed_fraction(&self) -> f32 {
        if self.session.file_size == 0 && (self.progress.done || self.index.is_some()) {
            1.0
        } else {
            self.progress.bytes_scanned as f32 / self.session.file_size.max(1) as f32
        }
    }

    fn set_visible_rows(&mut self, visible_rows: usize) -> Result<(), String> {
        let visible_rows = visible_rows.max(1);
        if visible_rows == self.visible_rows {
            return Ok(());
        }
        self.commit_edits();
        self.visible_rows = visible_rows;
        if self.filter_active() {
            return self.navigate_filter(self.filter_viewport_start);
        }
        if self.available_data_rows() == 0 {
            return Ok(());
        }
        let target = self.reveal_cell.map_or(self.viewport_start, |(row, _)| row);
        self.navigate(target)
    }

    fn page(&mut self, direction: i64) -> Result<(), String> {
        let page = i64::try_from(self.visible_rows).unwrap_or(i64::MAX);
        self.scroll_rows(direction.saturating_mul(page))
    }

    fn scroll_by_points(&mut self, delta_y: f32, row_stride: f32) -> Result<(), String> {
        let row_stride = row_stride.max(f32::EPSILON);
        self.scroll_points -= delta_y;
        let rows = (self.scroll_points / row_stride).trunc() as i64;
        if rows == 0 {
            return Ok(());
        }
        self.scroll_points -= rows as f32 * row_stride;
        self.scroll_rows(rows)
    }

    fn scroll_rows(&mut self, rows: i64) -> Result<(), String> {
        if self.filter_active() {
            let available = self.available_filter_rows();
            let maximum = max_viewport_start(available, self.visible_rows);
            let current = self.filter_viewport_start;
            let target = if rows.is_negative() {
                current.saturating_sub(rows.unsigned_abs())
            } else {
                current.saturating_add(rows as u64).min(maximum)
            };
            if target == current {
                self.scroll_points = 0.0;
                return Ok(());
            }
            return self.navigate_filter(target);
        }
        let available = self.available_data_rows();
        let maximum = max_viewport_start(available, self.visible_rows);
        let current = self.viewport_start.saturating_sub(self.data_start);
        let target = if rows.is_negative() {
            current.saturating_sub(rows.unsigned_abs())
        } else {
            current.saturating_add(rows as u64).min(maximum)
        };
        if target == current {
            self.scroll_points = 0.0;
            return Ok(());
        }
        self.navigate(self.data_start.saturating_add(target))
    }

    fn navigate_filter(&mut self, requested: u64) -> Result<(), String> {
        let available = self.available_filter_rows();
        if available == 0 {
            self.cancel_filter_read_for_navigation();
            self.filter_viewport_start = 0;
            self.filter_buffer_start = 0;
            self.filtered_rows.clear();
            self.clear_hidden_selection();
            return Ok(());
        }
        let start = requested.min(max_viewport_start(available, self.visible_rows));
        self.schedule_filter_buffer(start)?;
        self.filter_viewport_start = start;
        self.clear_hidden_selection();
        Ok(())
    }

    fn navigate(&mut self, requested: u64) -> Result<(), String> {
        self.commit_edits();
        let available = self.available_rows();
        if requested.max(self.data_start) >= available {
            return Err(format!(
                "Data row {} is not indexed yet ({} available).",
                requested.saturating_sub(self.data_start).saturating_add(1),
                self.available_data_rows()
            ));
        }
        let start = logical_viewport_start(
            requested,
            self.data_start,
            self.available_data_rows(),
            self.visible_rows,
        );
        self.load_buffer(start)?;
        self.viewport_start = start;
        self.clear_hidden_selection();
        Ok(())
    }

    fn load_buffer(&mut self, viewport_start: u64) -> Result<(), String> {
        let available = self.available_rows();
        let visible_end = viewport_start
            .saturating_add((available - viewport_start).min(self.visible_rows as u64));
        let loaded_end = self
            .buffer_start
            .saturating_add(self.buffered_rows.len() as u64);
        let capacity = self.visible_rows.saturating_add(2 * OVERSCAN_ROWS) as u64;
        if self.buffer_start <= viewport_start
            && loaded_end >= visible_end
            && self.buffered_rows.len() as u64 <= capacity
        {
            return Ok(());
        }

        let last_start = available.saturating_sub(capacity).max(self.data_start);
        let start = viewport_start
            .saturating_sub(OVERSCAN_ROWS as u64)
            .max(self.data_start)
            .min(last_start);
        let count = (available - start).min(capacity) as usize;
        let end = start.saturating_add(count as u64);
        let began = Instant::now();
        let rows = if end <= self.session.first_rows.len() as u64 {
            self.session.first_rows[start as usize..end as usize].to_vec()
        } else if let Some(index) = &self.index {
            self.session
                .read_rows(index, start, count)
                .map_err(|error| error.to_string())?
        } else if let Some(job) = &self.job {
            self.session
                .read_rows(&job.snapshot(), start, count)
                .map_err(|error| error.to_string())?
        } else {
            return Err("No structural index is available.".into());
        };
        let loaded_columns = rows.iter().map(|row| row.fields.len()).max().unwrap_or(0);
        if loaded_columns > self.total_columns {
            self.ensure_column_count(loaded_columns);
            self.refresh_column_headers();
        }
        self.last_viewport_read = Some(began.elapsed());
        self.buffer_start = start;
        self.buffered_rows = rows;
        Ok(())
    }

    fn filter_read_window(&self, viewport_start: u64) -> Option<FilterReadWindow> {
        let available = self.available_filter_rows();
        if available == 0 {
            return None;
        }
        let capacity = self.visible_rows.saturating_add(2 * OVERSCAN_ROWS) as u64;
        let last_start = available.saturating_sub(capacity);
        let start_match = viewport_start
            .saturating_sub(OVERSCAN_ROWS as u64)
            .min(last_start);
        let count = (available - start_match).min(capacity) as usize;
        Some(FilterReadWindow { start_match, count })
    }

    fn filter_buffer_covers(&self, viewport_start: u64) -> bool {
        let available = self.available_filter_rows();
        if available == 0 {
            return false;
        }
        let visible_end = viewport_start
            .saturating_add((available - viewport_start).min(self.visible_rows as u64));
        let loaded_end = self
            .filter_buffer_start
            .saturating_add(self.filtered_rows.len() as u64);
        let capacity = self.visible_rows.saturating_add(2 * OVERSCAN_ROWS) as u64;
        self.filter_buffer_start <= viewport_start
            && loaded_end >= visible_end
            && self.filtered_rows.len() as u64 <= capacity
    }

    fn schedule_filter_buffer(&mut self, viewport_start: u64) -> Result<(), String> {
        let Some(window) = self.filter_read_window(viewport_start) else {
            self.cancel_filter_read_for_navigation();
            self.filter_buffer_start = 0;
            self.filtered_rows.clear();
            return Ok(());
        };

        if self.filtered_rows.len() > window.count && window.start_match >= self.filter_buffer_start
        {
            let offset =
                usize::try_from(window.start_match.saturating_sub(self.filter_buffer_start))
                    .unwrap_or(self.filtered_rows.len());
            if offset.saturating_add(window.count) <= self.filtered_rows.len() {
                self.filtered_rows.drain(..offset);
                self.filtered_rows.truncate(window.count);
                self.filter_buffer_start = window.start_match;
            }
        }

        if self.filter_buffer_covers(viewport_start) {
            self.cancel_filter_read_for_navigation();
            return Ok(());
        }

        self.refresh_filter_snapshot_for(window.start_match.saturating_add(window.count as u64));
        if let Some(active) = self.filter_read.as_mut() {
            if active.window == window && !active.cancel_requested {
                self.pending_filter_read = None;
            } else {
                if !active.cancel_requested {
                    active.job.cancel();
                    active.cancel_requested = true;
                }
                self.pending_filter_read = Some(window);
            }
            return Ok(());
        }

        self.start_filter_read(window)
    }

    fn start_filter_read(&mut self, window: FilterReadWindow) -> Result<(), String> {
        self.pending_filter_read = None;
        let index = self
            .filter_index
            .as_ref()
            .ok_or_else(|| "No filter index is available.".to_owned())?;
        let job = self
            .session
            .start_filtered_read(index, window.start_match, window.count)
            .map_err(|error| error.to_string())?;
        self.filter_read = Some(ActiveFilterRead {
            window,
            job,
            started: Instant::now(),
            cancel_requested: false,
        });
        Ok(())
    }

    fn cancel_filter_read_for_navigation(&mut self) {
        self.pending_filter_read = None;
        if let Some(active) = self.filter_read.as_mut()
            && !active.cancel_requested
        {
            active.job.cancel();
            active.cancel_requested = true;
        }
    }

    fn visible_rows(&self) -> &[Row] {
        let offset = usize::try_from(self.viewport_start.saturating_sub(self.buffer_start))
            .unwrap_or(self.buffered_rows.len())
            .min(self.buffered_rows.len());
        let end = offset
            .saturating_add(self.visible_rows)
            .min(self.buffered_rows.len());
        &self.buffered_rows[offset..end]
    }

    fn visible_filter_rows(&self) -> &[FilterMatch] {
        let offset = usize::try_from(
            self.filter_viewport_start
                .saturating_sub(self.filter_buffer_start),
        )
        .unwrap_or(self.filtered_rows.len())
        .min(self.filtered_rows.len());
        let end = offset
            .saturating_add(self.visible_rows)
            .min(self.filtered_rows.len());
        &self.filtered_rows[offset..end]
    }

    fn visible_row_count(&self) -> usize {
        if self.filter_active() {
            self.visible_filter_rows().len()
        } else {
            self.visible_rows().len()
        }
    }

    fn visible_row(&self, index: usize) -> Option<(u64, &[Vec<u8>])> {
        if self.filter_active() {
            self.visible_filter_rows()
                .get(index)
                .map(|row| (row.row, row.fields.as_slice()))
        } else {
            self.visible_rows().get(index).map(|row| {
                (
                    self.viewport_start.saturating_add(index as u64),
                    row.fields.as_slice(),
                )
            })
        }
    }

    fn source_cell(&self, row: u64, column: usize) -> Option<&[u8]> {
        let index = usize::try_from(row.checked_sub(self.buffer_start)?).ok()?;
        self.buffered_rows
            .get(index)?
            .fields
            .get(column)
            .map(Vec::as_slice)
    }

    fn effective_cell(&self, row: u64, column: usize) -> Option<&[u8]> {
        self.cell_edits
            .get(&(row, column))
            .map(Vec::as_slice)
            .or_else(|| self.source_cell(row, column))
    }

    fn cell_value<'a>(&'a self, row: u64, column: usize, source: &'a [u8]) -> &'a [u8] {
        if let Some(edit) = self
            .cell_edit
            .as_ref()
            .filter(|edit| edit.row == row && edit.column == column)
        {
            return edit.draft.as_bytes();
        }
        self.cell_edits
            .get(&(row, column))
            .map_or(source, Vec::as_slice)
    }

    fn row_with_edits(&self, row: u64, fields: &[Vec<u8>]) -> Vec<Vec<u8>> {
        fields
            .iter()
            .enumerate()
            .map(|(column, source)| self.cell_value(row, column, source).to_vec())
            .collect()
    }

    fn current_source_row(&self) -> u64 {
        self.visible_row(0)
            .map(|(row, _)| row)
            .unwrap_or(self.viewport_start)
    }

    fn clear_hidden_selection(&mut self) {
        self.selected_columns.retain(|column| {
            self.columns
                .hidden
                .get(*column)
                .is_some_and(|hidden| !hidden)
        });
        if !self
            .column_selection_anchor
            .is_some_and(|anchor| self.selected_columns.contains(&anchor))
        {
            self.column_selection_anchor = self
                .columns
                .order
                .iter()
                .copied()
                .find(|column| self.selected_columns.contains(column));
        }
        let Some(selection) = self.selection else {
            return;
        };
        let row_visible = if self.filter_active() {
            self.visible_filter_rows()
                .iter()
                .any(|row| row.row == selection.row())
        } else {
            let row_end = self
                .viewport_start
                .saturating_add(self.visible_rows().len() as u64);
            (self.viewport_start..row_end).contains(&selection.row())
        };
        let column_visible = match selection {
            GridSelection::Row { .. } => true,
            GridSelection::Cell { column, .. } => self.columns.visible.contains(&column),
        };
        if !row_visible || !column_visible {
            self.selection = None;
        }
    }

    fn copy_selection_text(&self) -> Result<String, String> {
        let selection = self
            .selection
            .ok_or_else(|| "Select a cell or row before copying.".to_owned())?;
        let fields = if self.filter_active() {
            let row = self
                .visible_filter_rows()
                .iter()
                .find(|row| row.row == selection.row())
                .ok_or_else(|| {
                    "The selected row is no longer visible. Select it again.".to_owned()
                })?;
            self.row_with_edits(row.row, &row.fields)
        } else {
            let relative = selection
                .row()
                .checked_sub(self.buffer_start)
                .ok_or_else(|| {
                    "The selected row is no longer visible. Select it again.".to_owned()
                })?;
            let offset = usize::try_from(relative).map_err(|_| {
                "The selected row is no longer visible. Select it again.".to_owned()
            })?;
            let row = self.buffered_rows.get(offset).ok_or_else(|| {
                "The selected row is no longer visible. Select it again.".to_owned()
            })?;
            self.row_with_edits(selection.row(), &row.fields)
        };
        selection_fields_text(&fields, selection, MAX_COPY_BYTES)
    }

    fn grid_total_rows(&self) -> u64 {
        if self.filter_active() {
            self.available_filter_rows()
        } else {
            self.available_data_rows()
        }
    }

    fn grid_position(&self) -> u64 {
        if self.filter_active() {
            self.filter_viewport_start
        } else {
            self.viewport_start.saturating_sub(self.data_start)
        }
    }

    fn navigate_grid_position(&mut self, position: u64) -> Result<(), String> {
        self.commit_edits();
        if self.filter_active() {
            self.navigate_filter(position)
        } else {
            self.navigate(self.data_start.saturating_add(position))
        }
    }

    fn display_start(&self) -> u64 {
        if self.available_data_rows() == 0 {
            return 0;
        }
        self.viewport_start
            .saturating_sub(self.data_start)
            .saturating_add(1)
    }

    fn display_end(&self) -> u64 {
        if self.visible_rows().is_empty() {
            return 0;
        }
        self.display_start()
            .saturating_add(self.visible_rows().len().saturating_sub(1) as u64)
    }
}

fn max_viewport_start(total_rows: u64, visible_rows: usize) -> u64 {
    total_rows.saturating_sub(visible_rows as u64)
}

fn logical_viewport_start(
    requested: u64,
    data_start: u64,
    total_rows: u64,
    visible_rows: usize,
) -> u64 {
    data_start.saturating_add(
        requested
            .saturating_sub(data_start)
            .min(max_viewport_start(total_rows, visible_rows)),
    )
}

fn scroll_fraction_for_row(row: u64, data_start: u64, total_rows: u64, visible_rows: usize) -> f64 {
    let maximum = max_viewport_start(total_rows, visible_rows);
    if maximum == 0 {
        0.0
    } else {
        row.saturating_sub(data_start).min(maximum) as f64 / maximum as f64
    }
}

fn row_for_scroll_fraction(
    fraction: f64,
    data_start: u64,
    total_rows: u64,
    visible_rows: usize,
) -> u64 {
    let maximum = max_viewport_start(total_rows, visible_rows);
    let relative = (fraction.clamp(0.0, 1.0) * maximum as f64).round() as u64;
    data_start.saturating_add(relative.min(maximum))
}

fn parse_data_row(value: &str, data_start: u64) -> Result<u64, String> {
    let row: u64 = value
        .trim()
        .parse()
        .map_err(|_| "Data row must be a positive whole number.".to_owned())?;
    if row == 0 {
        return Err("Data rows start at 1.".into());
    }
    Ok(data_start.saturating_add(row - 1))
}

fn parse_file_column(value: &str, total_columns: usize) -> Result<usize, String> {
    let column: usize = value
        .trim()
        .parse()
        .map_err(|_| "File column must be a positive whole number.".to_owned())?;
    if column == 0 {
        return Err("File columns start at 1.".into());
    }
    if column > total_columns {
        return Err(if total_columns == 0 {
            "This file has no columns.".into()
        } else {
            format!("File column must be between 1 and {total_columns}.")
        });
    }
    Ok(column - 1)
}

fn parse_move_position(
    value: &str,
    total_columns: usize,
    selected_columns: usize,
) -> Result<usize, String> {
    if selected_columns == 0 {
        return Err("Select at least one numbered column first.".into());
    }
    if selected_columns > total_columns {
        return Err("A selected column is outside this file.".into());
    }
    let position: usize = value
        .trim()
        .parse()
        .map_err(|_| "Destination position must be a positive whole number.".to_owned())?;
    if position == 0 {
        return Err("Destination positions start at 1.".into());
    }
    let maximum = total_columns - selected_columns + 1;
    if position > maximum {
        return Err(format!(
            "Destination position must be between 1 and {maximum}."
        ));
    }
    Ok(position - 1)
}

fn literal_contains(value: &[u8], query: &[u8], case_sensitivity: CaseSensitivity) -> bool {
    !query.is_empty()
        && value
            .windows(query.len())
            .any(|part| literal_equals(part, query, case_sensitivity))
}

fn literal_equals(left: &[u8], right: &[u8], case_sensitivity: CaseSensitivity) -> bool {
    match case_sensitivity {
        CaseSensitivity::Insensitive => left.eq_ignore_ascii_case(right),
        CaseSensitivity::Sensitive => left == right,
    }
}

fn replace_literal_all_with_case(
    value: &[u8],
    query: &[u8],
    replacement: &[u8],
    case_sensitivity: CaseSensitivity,
) -> Option<Vec<u8>> {
    if query.is_empty() {
        return None;
    }
    let mut start = 0;
    let mut output = Vec::with_capacity(value.len());
    let mut replaced = false;
    while let Some(relative) = value[start..]
        .windows(query.len())
        .position(|part| literal_equals(part, query, case_sensitivity))
    {
        let found = start + relative;
        output.extend_from_slice(&value[start..found]);
        output.extend_from_slice(replacement);
        start = found + query.len();
        replaced = true;
    }
    if !replaced {
        return None;
    }
    output.extend_from_slice(&value[start..]);
    Some(output)
}

impl Drop for Document {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn headers_for(session: &Session, columns: &[usize]) -> Vec<String> {
    columns
        .iter()
        .map(|column| column_name(session, *column))
        .collect()
}

fn column_name(session: &Session, column: usize) -> String {
    if session.dialect.has_header
        && let Some(field) = session
            .first_rows
            .first()
            .and_then(|row| row.fields.get(column))
    {
        return field_text(field);
    }
    format!("Column {}", column + 1)
}

fn selected_split_column(selected_columns: &BTreeSet<usize>) -> Option<usize> {
    if selected_columns.len() == 1 {
        selected_columns.iter().next().copied()
    } else {
        None
    }
}

fn validated_column_selection(
    columns: Vec<usize>,
    total_columns: usize,
) -> Result<BTreeSet<usize>, String> {
    if columns.is_empty() {
        return Err("Select at least one numbered column first.".into());
    }
    if columns.iter().any(|column| *column >= total_columns) {
        return Err("A selected column is outside this file.".into());
    }
    let selected = columns.iter().copied().collect::<BTreeSet<_>>();
    if selected.len() != columns.len() {
        return Err("Select each column only once.".into());
    }
    Ok(selected)
}

fn select_column(
    columns: &ColumnView,
    selected_columns: &mut BTreeSet<usize>,
    anchor: &mut Option<usize>,
    column: usize,
    modifiers: egui::Modifiers,
) {
    if modifiers.shift {
        let shown_columns = columns
            .order
            .iter()
            .copied()
            .filter(|shown| !columns.hidden[*shown])
            .collect::<Vec<_>>();
        let anchor_column = (*anchor)
            .filter(|anchor| shown_columns.contains(anchor))
            .or_else(|| {
                shown_columns
                    .iter()
                    .copied()
                    .find(|shown| selected_columns.contains(shown))
            })
            .unwrap_or(column);
        selected_columns.clear();
        let Some(anchor_position) = shown_columns
            .iter()
            .position(|shown| *shown == anchor_column)
        else {
            selected_columns.insert(column);
            *anchor = Some(column);
            return;
        };
        let Some(column_position) = shown_columns.iter().position(|shown| *shown == column) else {
            selected_columns.insert(column);
            *anchor = Some(column);
            return;
        };
        let range = anchor_position.min(column_position)..=anchor_position.max(column_position);
        selected_columns.extend(shown_columns[range].iter().copied());
        *anchor = Some(anchor_column);
    } else if modifiers.command {
        if selected_columns.remove(&column) {
            if *anchor == Some(column) {
                *anchor = selected_columns.iter().next().copied();
            }
        } else {
            selected_columns.insert(column);
            *anchor = Some(column);
        }
    } else {
        selected_columns.clear();
        selected_columns.insert(column);
        *anchor = Some(column);
    }
}

fn column_selection_fill(visuals: &egui::Visuals) -> Color32 {
    visuals.selection.bg_fill.gamma_multiply(0.35)
}

fn column_ruler_divider_stroke(visuals: &egui::Visuals) -> egui::Stroke {
    visuals.widgets.noninteractive.bg_stroke
}

fn paint_column_selection(ui: &egui::Ui, selected: bool) {
    if selected {
        ui.painter()
            .rect_filled(ui.max_rect(), 0.0, column_selection_fill(ui.visuals()));
    }
}

fn rendered_column_range(
    viewport: egui::Rect,
    column_offsets: &[f32],
    column_spacing: f32,
    focused_column: Option<usize>,
) -> std::ops::Range<usize> {
    let total_columns = column_offsets.len().saturating_sub(1);
    if total_columns <= MAX_RENDERED_COLUMNS {
        return 0..total_columns;
    }
    let maximum_start = total_columns - MAX_RENDERED_COLUMNS;
    let start = focused_column.map_or_else(
        || {
            let x = (viewport.min.x - ROW_NUMBER_WIDTH - column_spacing).max(0.0);
            let first_visible = column_offsets
                .partition_point(|offset| *offset <= x)
                .saturating_sub(1);
            first_visible.saturating_sub(1).min(maximum_start)
        },
        |column| {
            column
                .saturating_sub(MAX_RENDERED_COLUMNS / 2)
                .min(maximum_start)
        },
    );
    start..start + MAX_RENDERED_COLUMNS
}

fn show_grid_with_filter_case(
    ui: &mut egui::Ui,
    document: &mut Document,
    filter_case_sensitivity: CaseSensitivity,
) -> Result<Option<GridColumnRequest>, String> {
    if document.source_changed {
        ui.centered_and_justified(|ui| {
            ui.label(SOURCE_CHANGED_NOTICE);
        });
        return Ok(None);
    }
    let grid_height = ui.available_height();
    let horizontal_scrollbar = ui.spacing().scroll.allocated_width();
    let body_height = (grid_height - HEADER_HEIGHT - horizontal_scrollbar).max(ROW_HEIGHT);
    let row_stride = ROW_HEIGHT;
    let visible_rows = (body_height / row_stride).floor().max(1.0) as usize;
    document.set_visible_rows(visible_rows)?;
    let total_rows = document.grid_total_rows();

    let grid_rect = ui.available_rect_before_wrap();
    if total_rows > 0 && ui.rect_contains_pointer(grid_rect) {
        let delta_y = ui.input_mut(|input| {
            let delta_y = input.smooth_scroll_delta.y;
            input.smooth_scroll_delta.y = 0.0;
            delta_y
        });
        if delta_y != 0.0 {
            document.scroll_by_points(delta_y, row_stride)?;
        }
    }

    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
        let position = document.grid_position();
        let fraction = scroll_fraction_for_row(position, 0, total_rows, document.visible_rows);
        let mut slider_position = 1.0 - fraction;
        let thumb_height = scrollbar_thumb_height(grid_height, total_rows, document.visible_rows);
        let handle_radius = SCROLLBAR_WIDTH / 2.5;
        let scroll_enabled = total_rows > 0;
        let label = if scroll_enabled && document.filter_active() {
            format!(
                "Vertical scroll, filter match {} of {total_rows}",
                position.saturating_add(1)
            )
        } else if scroll_enabled {
            format!(
                "Vertical scroll, row {} of {total_rows}",
                document.display_start()
            )
        } else {
            "Vertical scroll, no data rows".into()
        };
        let hover_text = if scroll_enabled && document.filter_active() {
            format!(
                "Filter match {} of {total_rows}",
                position.saturating_add(1)
            )
        } else if scroll_enabled {
            format!("Row {} of {total_rows}", document.display_start())
        } else {
            "No data rows".into()
        };
        let response = ui
            .allocate_ui_with_layout(
                egui::vec2(SCROLLBAR_WIDTH, grid_height),
                Layout::top_down(Align::Center),
                |ui| {
                    ui.spacing_mut().slider_width = grid_height;
                    ui.add_enabled(
                        scroll_enabled,
                        egui::Slider::new(&mut slider_position, 0.0..=1.0)
                            .vertical()
                            .show_value(false)
                            .smart_aim(false)
                            .handle_shape(egui::style::HandleShape::Rect {
                                aspect_ratio: thumb_height / (2.0 * handle_radius),
                            }),
                    )
                },
            )
            .inner
            .on_hover_text(hover_text);
        response.widget_info(|| {
            egui::WidgetInfo::slider(scroll_enabled && ui.is_enabled(), slider_position, &label)
        });
        if scroll_enabled && response.changed() {
            let target = row_for_scroll_fraction(
                1.0 - slider_position,
                0,
                total_rows,
                document.visible_rows,
            );
            if target != position {
                document.navigate_grid_position(target)?;
            }
        }

        let reveal_cell = document.reveal_cell.take();
        ui.separator();
        let interaction = ui
            .allocate_ui_with_layout(ui.available_size(), Layout::top_down(Align::Min), |ui| {
                show_table(ui, document, reveal_cell, filter_case_sensitivity)
            })
            .inner;
        if let Some((row_selection, active_row)) = interaction.row_selection.clone() {
            document.selection = active_row.map(|row| GridSelection::Row { row });
            document.selected_rows = row_selection;
            document.selected_columns.clear();
            document.column_selection_anchor = None;
        }
        if let Some(selection) = interaction.selection {
            if matches!(selection, GridSelection::Cell { .. }) {
                document.selected_rows.clear();
            }
            document.selection = Some(selection);
            document.selected_columns.clear();
            document.column_selection_anchor = None;
        }
        if interaction.copy_selection {
            ui.ctx().copy_text(document.copy_selection_text()?);
        }
        if let Some(query) = interaction.filter_query {
            document.start_filter(query)?;
        }
        Ok(interaction.column_request)
    })
    .inner
}

#[cfg(test)]
fn show_grid(
    ui: &mut egui::Ui,
    document: &mut Document,
) -> Result<Option<GridColumnRequest>, String> {
    show_grid_with_filter_case(ui, document, CaseSensitivity::Insensitive)
}

fn show_table(
    ui: &mut egui::Ui,
    document: &mut Document,
    reveal_cell: Option<(u64, usize)>,
    filter_case_sensitivity: CaseSensitivity,
) -> GridInteraction {
    let row_count = document.visible_row_count();
    let auto_fit_columns = std::mem::take(&mut document.auto_fit_columns);
    let virtualized = document.headers.len() > MAX_RENDERED_COLUMNS;
    if auto_fit_columns {
        document.columns_to_fit.clear();
        if virtualized || !document.fitted_column_widths.is_empty() {
            document.columns_to_fit.extend(&document.columns.visible);
        }
    }
    // Fit in bounded batches so very wide files do not block the UI.
    for _ in 0..MAX_RENDERED_COLUMNS {
        let Some(column) = document.columns_to_fit.pop_front() else {
            break;
        };
        if document.columns.hidden[column] {
            continue;
        }
        let text_width = |text| {
            ui.painter()
                .layout_no_wrap(
                    text,
                    FontId::new(13.0, FontFamily::Monospace),
                    ui.visuals().text_color(),
                )
                .size()
                .x
        };
        let mut width = (text_width(document.column_name(column)) + 12.0).max(80.0);
        for index in 0..row_count {
            if let Some((row, fields)) = document.visible_row(index)
                && let Some(source) = fields.get(column)
            {
                width = width.max(
                    text_width(field_text(document.cell_value(row, column, source)))
                        + 2.0 * ui.spacing().button_padding.x,
                );
            }
        }
        document.fitted_column_widths.insert(column, width.ceil());
        document.reset_table_widths = true;
    }
    if !document.columns_to_fit.is_empty() {
        ui.ctx().request_repaint();
    }
    let mut interaction = GridInteraction::default();
    let mut active_header_edit = document.header_edit.take();
    let mut begin_header_edit = None;
    let mut commit_header_edit = false;
    let mut cancel_header_edit = false;
    let mut active_cell_edit = document.cell_edit.take();
    let cell_focus_requested = document.cell_focus_requested.take();
    let column_focus_requested = document.column_focus_requested.take();
    let mut begin_cell_edit = None;
    let mut commit_cell_edit = false;
    let mut cancel_cell_edit = false;
    let mut restore_cell_focus = None;
    let grid_height = ui.available_height();
    let viewport_width = ui.available_width();
    let column_spacing = ui.spacing().item_spacing.x;
    let column_width = if virtualized {
        ((viewport_width - ROW_NUMBER_WIDTH) / MAX_RENDERED_COLUMNS.saturating_sub(2).max(1) as f32
            - column_spacing)
            .max(80.0)
    } else {
        ((viewport_width - 82.0) / document.headers.len().max(1) as f32).clamp(80.0, 160.0)
    };
    let column_widths = document
        .columns
        .visible
        .iter()
        .map(|column| {
            document
                .fitted_column_widths
                .get(column)
                .copied()
                .unwrap_or(column_width)
                .max(if virtualized { column_width } else { 80.0 })
        })
        .collect::<Vec<_>>();
    let mut column_offsets = Vec::with_capacity(column_widths.len() + 1);
    column_offsets.push(0.0);
    for width in &column_widths {
        column_offsets.push(column_offsets.last().unwrap() + width + column_spacing);
    }
    let content_width = ROW_NUMBER_WIDTH + column_offsets.last().unwrap();
    let body_height =
        (grid_height - HEADER_HEIGHT - ui.spacing().scroll.allocated_width()).max(ROW_HEIGHT);
    let focused_source_column = reveal_cell
        .map(|(_, column)| column)
        .or(column_focus_requested)
        .or(cell_focus_requested.map(|(_, column)| column))
        .or(active_header_edit.as_ref().map(|edit| edit.column))
        .or(active_cell_edit.as_ref().map(|edit| edit.column));
    let focused_column = focused_source_column.and_then(|column| {
        document
            .columns
            .visible
            .iter()
            .position(|visible| *visible == column)
    });

    let mut horizontal_scroll = egui::ScrollArea::horizontal()
        .id_salt("quarry-grid-horizontal")
        .auto_shrink([false, false])
        .max_height(grid_height);
    if virtualized && let Some(column) = focused_column {
        let target_x = ROW_NUMBER_WIDTH + column_spacing + column_offsets[column];
        horizontal_scroll = horizontal_scroll.horizontal_scroll_offset(
            (target_x - (viewport_width - column_widths[column]).max(0.0) / 2.0).max(0.0),
        );
    }

    horizontal_scroll.show_viewport(ui, |ui, viewport| {
        let rendered_range = rendered_column_range(
            viewport,
            &column_offsets,
            column_spacing,
            virtualized.then_some(focused_column).flatten(),
        );
        let spacer_width = (rendered_range.start > 0)
            .then_some(column_offsets[rendered_range.start] - column_spacing);
        let visible_headers = rendered_range
            .clone()
            .map(|visible_column| {
                (
                    visible_column,
                    document.columns.visible[visible_column],
                    document.headers[visible_column].clone(),
                )
            })
            .collect::<Vec<_>>();
        let header_min_widths = visible_headers
            .iter()
            .map(|(_, _, name)| {
                if auto_fit_columns && !virtualized {
                    ui.painter()
                        .layout_no_wrap(
                            name.clone(),
                            FontId::new(13.0, FontFamily::Monospace),
                            ui.visuals().text_color(),
                        )
                        .size()
                        .x
                        + 12.0
                } else {
                    80.0
                }
            })
            .collect::<Vec<_>>();

        ui.set_min_width(content_width.max(viewport_width));
        ui.spacing_mut().item_spacing.y = 0.0;
        let divider_left = ui.cursor().left();
        let divider_y = ui.cursor().top() + COLUMN_RULER_HEIGHT;
        let mut table = TableBuilder::new(ui)
            .id_salt(if virtualized {
                "quarry-grid-virtual"
            } else {
                "quarry-grid"
            })
            .striped(true)
            .resizable(!virtualized)
            .vscroll(false)
            .auto_shrink([false, false])
            .min_scrolled_height(body_height)
            .max_scroll_height(body_height)
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(
                Column::exact(ROW_NUMBER_WIDTH)
                    .clip(true)
                    .resizable(false),
            );
        if let Some(spacer_width) = spacer_width {
            table = table.column(
                Column::exact(spacer_width)
                    .clip(true)
                    .resizable(false),
            );
        }
        for (visible_column, header_min_width) in rendered_range.clone().zip(&header_min_widths) {
            table = if virtualized {
                table.column(
                    Column::exact(column_widths[visible_column])
                        .clip(true)
                        .resizable(false),
                )
            } else {
                table.column(
                    Column::initial(column_widths[visible_column])
                        .at_least(header_min_width.max(80.0))
                        .clip(true)
                        .resizable(true)
                        .auto_size_this_frame(auto_fit_columns),
                )
            };
        }
        if !virtualized && (std::mem::take(&mut document.reset_table_widths) || auto_fit_columns) {
            table.reset();
        }
        table
            .header(HEADER_HEIGHT, |mut header| {
                header.col(|ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.add_space(COLUMN_RULER_HEIGHT);
                    });
                });
                if spacer_width.is_some() {
                    header.col(|_| {});
                }
                for (_, column, name) in &visible_headers {
                    let column = *column;
                    header.col(|ui| {
                        ui.push_id(("column-header", column), |ui| {
                            let selected = document.selected_columns.contains(&column);
                            paint_column_selection(ui, selected);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                let width = ui.available_width();
                                let mut button = egui::Button::selectable(
                                        selected,
                                        RichText::new(column.saturating_add(1).to_string())
                                            .monospace()
                                            .size(11.0),
                                    )
                                    .small();
                                if selected {
                                    button = button
                                        .fill(ui.visuals().selection.bg_fill)
                                        .stroke(ui.visuals().selection.stroke);
                                }
                                let response = ui
                                    .add_sized([width, COLUMN_RULER_HEIGHT], button)
                                    .on_hover_text(
                                        "Click to select. Shift-click a range. Command/Ctrl-click to add or remove. Right-click a selected number for column tools and row sorting.",
                                    );
                                if column_focus_requested == Some(column) {
                                    response.request_focus();
                                }
                                response.widget_info(|| {
                                    egui::WidgetInfo::selected(
                                        egui::WidgetType::SelectableLabel,
                                        ui.is_enabled(),
                                        selected,
                                        format!(
                                            "Select file column {} ({})",
                                            column.saturating_add(1),
                                            accessible_header_name(name)
                                        ),
                                    )
                                });
                                let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
                                    node.set_selected(selected);
                                    node.set_description(
                                        "Activate to add or remove this column from the selection.",
                                    );
                                    node.add_action(egui::accesskit::Action::ShowContextMenu);
                                });
                                let accesskit_context_menu = ui.input_mut(|input| {
                                    let mut requested = false;
                                    input.consume_accesskit_action_requests(
                                        response.id,
                                        |request| {
                                            let matches = request.action
                                                == egui::accesskit::Action::ShowContextMenu;
                                            requested |= matches;
                                            matches
                                        },
                                    );
                                    requested
                                });
                                let accesskit_click = ui.input(|input| {
                                    input.has_accesskit_action_request(
                                        response.id,
                                        egui::accesskit::Action::Click,
                                    )
                                });
                                let keyboard_context_menu = response.has_focus()
                                    && ui.input(|input| {
                                        input.modifiers.shift
                                            && input.key_pressed(egui::Key::F10)
                                    });
                                let open_context_menu = response.secondary_clicked()
                                    || keyboard_context_menu
                                    || accesskit_context_menu;
                                if open_context_menu && !selected {
                                    document.selected_columns.clear();
                                    document.selected_columns.insert(column);
                                    document.column_selection_anchor = Some(column);
                                    document.selection = None;
                                    document.selected_rows.clear();
                                }
                                if response.clicked() {
                                    let mut modifiers = ui.input(|input| input.modifiers);
                                    if accesskit_click {
                                        modifiers.shift = false;
                                        modifiers.command = true;
                                    }
                                    select_column(
                                        &document.columns,
                                        &mut document.selected_columns,
                                        &mut document.column_selection_anchor,
                                        column,
                                        modifiers,
                                    );
                                    document.selection = None;
                                    document.selected_rows.clear();
                                }
                                let popup_command = if open_context_menu {
                                    Some(egui::SetOpenCommand::Bool(true))
                                } else if response.clicked() {
                                    Some(egui::SetOpenCommand::Bool(false))
                                } else {
                                    None
                                };
                                egui::Popup::menu(&response)
                                    .open_memory(popup_command)
                                    .show(|ui| {
                                    let split_column = selected_split_column(
                                        &document.selected_columns,
                                    );
                                    if ui
                                        .add_enabled(
                                            split_column.is_some(),
                                            egui::Button::new("Split Columns…"),
                                        )
                                        .on_disabled_hover_text(
                                            "Select exactly one numbered column first.",
                                        )
                                        .clicked()
                                    {
                                        interaction.column_request = Some(
                                            GridColumnRequest::Dialog(StructuralDialog::split(
                                                split_column.expect(
                                                    "enabled split has one selected column",
                                                ),
                                            )),
                                        );
                                        ui.close();
                                    }
                                    let combine_columns = document
                                        .selected_columns
                                        .iter()
                                        .copied()
                                        .collect::<Vec<_>>();
                                    if ui
                                        .add_enabled(
                                            combine_columns.len() >= 2,
                                            egui::Button::new("Combine Columns…"),
                                        )
                                        .on_disabled_hover_text(
                                            "Select at least two numbered columns first. Shift-click a range, or Command/Ctrl-click separate columns.",
                                        )
                                        .clicked()
                                    {
                                        interaction.column_request = Some(
                                            GridColumnRequest::Dialog(StructuralDialog::combine(
                                                combine_columns.clone(),
                                            )),
                                        );
                                        ui.close();
                                    }
                                    ui.separator();
                                    if ui
                                        .add_enabled(
                                            !combine_columns.is_empty(),
                                            egui::Button::new("Move Selected Columns…"),
                                        )
                                        .on_disabled_hover_text(
                                            "Select at least one numbered column first.",
                                        )
                                        .clicked()
                                    {
                                        interaction.column_request = Some(
                                            GridColumnRequest::Dialog(
                                                StructuralDialog::move_columns(
                                                    combine_columns.clone(),
                                                ),
                                            ),
                                        );
                                        ui.close();
                                    }
                                    if ui
                                        .add_enabled(
                                            !combine_columns.is_empty()
                                                && combine_columns.len()
                                                    < document.total_columns,
                                            egui::Button::new("Delete Selected Columns"),
                                        )
                                        .on_disabled_hover_text(
                                            "At least one column must remain.",
                                        )
                                        .clicked()
                                    {
                                        interaction.column_request = Some(
                                            GridColumnRequest::Delete(combine_columns),
                                        );
                                        ui.close();
                                    }
                                    ui.separator();
                                    if ui
                                        .add_enabled(
                                            split_column.is_some(),
                                            egui::Button::new("Sort Rows…"),
                                        )
                                        .on_disabled_hover_text(
                                            "Select exactly one numbered column first.",
                                        )
                                        .clicked()
                                    {
                                        interaction.column_request = Some(
                                            GridColumnRequest::Dialog(StructuralDialog::sort(
                                                split_column.expect(
                                                    "enabled sort has one selected column",
                                                ),
                                            )),
                                        );
                                        ui.close();
                                    }
                                    });
                                if let Some(edit) = active_header_edit
                                    .as_mut()
                                    .filter(|edit| edit.column == column)
                                {
                                    let response = ui.add_sized(
                                        [width, 17.0],
                                        egui::TextEdit::singleline(&mut edit.draft)
                                            .id(header_edit_id(column))
                                            .margin(egui::Margin::ZERO),
                                    );
                                    let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
                                        node.set_label(format!(
                                            "New name for file column {}",
                                            column.saturating_add(1)
                                        ));
                                    });
                                    if edit.focus_requested {
                                        response.request_focus();
                                        edit.focus_requested = false;
                                    }
                                    if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                                        cancel_header_edit = true;
                                    } else if (response.has_focus()
                                        && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                                        || response.lost_focus()
                                    {
                                        commit_header_edit = true;
                                    }
                                } else {
                                    let editable = document.header_is_editable(column);
                                    let response = ui.add_sized(
                                        [width, 17.0],
                                        egui::Label::new(RichText::new(name).monospace())
                                            .truncate()
                                            .sense(if editable {
                                                egui::Sense::click()
                                            } else {
                                                egui::Sense::hover()
                                            }),
                                    );
                                    if editable {
                                        response.widget_info(|| {
                                            egui::WidgetInfo::labeled(
                                                egui::WidgetType::Button,
                                                ui.is_enabled(),
                                                format!(
                                                    "Rename file column {} ({})",
                                                    column.saturating_add(1),
                                                    accessible_header_name(name)
                                                ),
                                            )
                                        });
                                        let activate = response.clicked()
                                            || (response.has_focus()
                                                && ui.input(|input| {
                                                    input.key_pressed(egui::Key::Enter)
                                                }));
                                        response.on_hover_text("Click to rename");
                                        if activate {
                                            begin_header_edit = Some(column);
                                        }
                                    }
                                }
                            });
                        });
                    });
                }
            })
            .body(|body| {
                body.rows(ROW_HEIGHT, row_count, |mut table_row| {
                        let row_index = table_row.index();
                        let (record_row, fields) = document
                            .visible_row(row_index)
                            .expect("the table row is visible");
                        let display_row = record_row
                            .saturating_sub(document.data_start)
                            .saturating_add(1);
                        table_row.col(|ui| {
                            ui.scope_builder(
                                egui::UiBuilder::new().id(("row-selection", record_row)),
                                |ui| {
                                let selected = document.selected_rows.contains(record_row)
                                    || document
                                        .selection
                                        .is_some_and(|selection| selection.selects_row(record_row));
                                let color = if selected {
                                    ui.visuals().selection.stroke.color
                                } else {
                                    QUARRY_YELLOW_TEXT
                                };
                                let response = ui.add_sized(
                                    [ui.available_width(), ROW_HEIGHT],
                                    egui::Button::selectable(
                                        selected,
                                        RichText::new(display_row.to_string())
                                            .monospace()
                                            .color(color),
                                    )
                                    .small(),
                                );
                                let enabled = ui.is_enabled();
                                response.widget_info(|| {
                                    egui::WidgetInfo::selected(
                                        egui::WidgetType::SelectableLabel,
                                        enabled,
                                        selected,
                                        format!("Select row {display_row}"),
                                    )
                                });
                                let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
                                    node.set_selected(selected);
                                    node.set_description(
                                        "Activate to select this row. Shift-click a range. Command/Ctrl-click to add or remove. Open the context menu for row actions.",
                                    );
                                    node.add_action(egui::accesskit::Action::ShowContextMenu);
                                });
                                let accesskit_context_menu = ui.input_mut(|input| {
                                    let mut requested = false;
                                    input.consume_accesskit_action_requests(
                                        response.id,
                                        |request| {
                                            let matches = request.action
                                                == egui::accesskit::Action::ShowContextMenu;
                                            requested |= matches;
                                            matches
                                        },
                                    );
                                    requested
                                });
                                let accesskit_click = ui.input(|input| {
                                    input.has_accesskit_action_request(
                                        response.id,
                                        egui::accesskit::Action::Click,
                                    )
                                });
                                let keyboard_context_menu = response.has_focus()
                                    && ui.input(|input| {
                                        input.modifiers.shift
                                            && input.key_pressed(egui::Key::F10)
                                    });
                                let open_context_menu = response.secondary_clicked()
                                    || keyboard_context_menu
                                    || accesskit_context_menu;
                                let mut effective_row_selection = None;
                                if open_context_menu
                                    && !document.selected_rows.contains(record_row)
                                {
                                    let mut selection = document.selected_rows.clone();
                                    selection.select(record_row, egui::Modifiers::NONE);
                                    interaction.row_selection = Some((
                                        selection.clone(),
                                        Some(record_row),
                                    ));
                                    effective_row_selection = Some(selection);
                                }
                                if response.clicked() {
                                    response.request_focus();
                                    let mut modifiers = ui.input(|input| input.modifiers);
                                    if accesskit_click {
                                        modifiers.shift = false;
                                        modifiers.command = true;
                                    }
                                    let mut selection = document.selected_rows.clone();
                                    selection.select(record_row, modifiers);
                                    let active_row = selection
                                        .contains(record_row)
                                        .then_some(record_row)
                                        .or_else(|| selection.first());
                                    interaction.row_selection =
                                        Some((selection.clone(), active_row));
                                    effective_row_selection = Some(selection);
                                }
                                let popup_command = if open_context_menu {
                                    Some(egui::SetOpenCommand::Bool(true))
                                } else if response.clicked() {
                                    Some(egui::SetOpenCommand::Bool(false))
                                } else {
                                    None
                                };
                                let row_selection = effective_row_selection
                                    .as_ref()
                                    .unwrap_or(&document.selected_rows);
                                egui::Popup::menu(&response)
                                    .open_memory(popup_command)
                                    .show(|ui| {
                                        let disabled_reason = document
                                            .structural_edit_disabled_reason()
                                            .or_else(|| {
                                                row_selection.is_empty().then_some(
                                                    "Select at least one numbered row first.",
                                                )
                                            });
                                        if ui
                                            .add_enabled(
                                                disabled_reason.is_none(),
                                                egui::Button::new("Delete Selected Rows"),
                                            )
                                            .on_disabled_hover_text(
                                                disabled_reason.unwrap_or_default(),
                                            )
                                            .clicked()
                                        {
                                            interaction.column_request = Some(
                                                GridColumnRequest::DeleteRows(
                                                    row_selection.ranges.clone(),
                                                ),
                                            );
                                            ui.close();
                                        }
                                    });
                                },
                            );
                        });
                        if spacer_width.is_some() {
                            table_row.col(|_| {});
                        }
                        for (visible_column, column, _) in &visible_headers {
                            let visible_column = *visible_column;
                            let column = *column;
                            table_row.col(|ui| {
                                ui.scope_builder(
                                    egui::UiBuilder::new()
                                        .id(("cell-selection", record_row, column)),
                                    |ui| {
                                    paint_column_selection(
                                        ui,
                                        document.selected_columns.contains(&column),
                                    );
                                    let source = fields.get(column).map(Vec::as_slice);
                                    let header = &document.headers[visible_column];
                                    let accessible_header = accessible_header_name(header);
                                    if let Some(edit) = active_cell_edit.as_mut().filter(|edit| {
                                        edit.row == record_row && edit.column == column
                                    }) {
                                        let response = ui.add_sized(
                                            [ui.available_width(), ROW_HEIGHT],
                                            egui::TextEdit::multiline(&mut edit.draft)
                                                .id(cell_edit_id(record_row, column))
                                                .font(TextStyle::Monospace)
                                                .desired_rows(1)
                                                .return_key(egui::KeyboardShortcut::new(
                                                    egui::Modifiers::SHIFT,
                                                    egui::Key::Enter,
                                                ))
                                                .margin(egui::Margin::ZERO),
                                        );
                                        let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
                                            node.set_label(format!(
                                                "Edit data row {display_row}, file column {} ({accessible_header})",
                                                column.saturating_add(1),
                                            ));
                                        });
                                        if edit.focus_requested {
                                            response.request_focus();
                                            edit.focus_requested = false;
                                        }
                                        let escape = ui.input(|input| {
                                            input.key_pressed(egui::Key::Escape)
                                        });
                                        let enter = response.has_focus()
                                            && ui.input(|input| {
                                                input.key_pressed(egui::Key::Enter)
                                                    && !input.modifiers.shift
                                            });
                                        if escape {
                                            cancel_cell_edit = true;
                                            restore_cell_focus = Some((record_row, column));
                                        } else if enter {
                                            commit_cell_edit = true;
                                            restore_cell_focus = Some((record_row, column));
                                        } else if response.lost_focus() {
                                            commit_cell_edit = true;
                                        }
                                        if reveal_cell == Some((record_row, column)) {
                                            response.scroll_to_me(Some(Align::Center));
                                        }
                                    } else {
                                        let value = source.map(|source| {
                                            document.cell_value(record_row, column, source)
                                        });
                                        let text = value.map_or_else(String::new, field_text);
                                        let active_selection_highlight = document
                                            .selection
                                            .is_some_and(|selection| {
                                                selection.selects_cell(record_row, column)
                                            });
                                        let active_cell_selected = matches!(
                                            document.selection,
                                            Some(GridSelection::Cell {
                                                row: selected_row,
                                                column: selected_column,
                                            }) if selected_row == record_row && selected_column == column
                                        );
                                        let selected = document.selected_rows.contains(record_row)
                                            || active_selection_highlight;
                                        let response = ui.add_sized(
                                            [ui.available_width(), ROW_HEIGHT],
                                            egui::Button::selectable(
                                                selected,
                                                RichText::new(&text).monospace(),
                                            )
                                            .small(),
                                        );
                                        let enabled = ui.is_enabled();
                                        response.widget_info(|| {
                                            egui::WidgetInfo::selected(
                                                egui::WidgetType::SelectableLabel,
                                                enabled,
                                                selected,
                                                format!(
                                                    "Select row {display_row}, column {} ({accessible_header}): {text}",
                                                    column.saturating_add(1),
                                                ),
                                            )
                                        });
                                        let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
                                            node.set_selected(selected);
                                            node.set_description(
                                                "Activate to select. Open the context menu to copy or filter by this value.",
                                            );
                                            node.add_action(
                                                egui::accesskit::Action::ShowContextMenu,
                                            );
                                        });
                                        let accesskit_context_menu = ui.input_mut(|input| {
                                            let mut requested = false;
                                            input.consume_accesskit_action_requests(
                                                response.id,
                                                |request| {
                                                    let matches = request.action
                                                        == egui::accesskit::Action::ShowContextMenu;
                                                    requested |= matches;
                                                    matches
                                                },
                                            );
                                            requested
                                        });
                                        let keyboard_context_menu = response.has_focus()
                                            && ui.input(|input| {
                                                input.modifiers.shift
                                                    && input.key_pressed(egui::Key::F10)
                                            });
                                        let open_context_menu = response.secondary_clicked()
                                            || keyboard_context_menu
                                            || accesskit_context_menu;
                                        if open_context_menu {
                                            interaction.selection = Some(GridSelection::Cell {
                                                row: record_row,
                                                column,
                                            });
                                        }
                                        if response.clicked() {
                                            response.request_focus();
                                            interaction.selection = Some(GridSelection::Cell {
                                                row: record_row,
                                                column,
                                            });
                                        }
                                        let popup_command = if open_context_menu {
                                            Some(egui::SetOpenCommand::Bool(true))
                                        } else if response.clicked() {
                                            Some(egui::SetOpenCommand::Bool(false))
                                        } else {
                                            None
                                        };
                                        let can_filter = source.is_some()
                                            && document.is_filter_ready()
                                            && !document.has_cell_edits()
                                            && document.search_job.is_none()
                                            && document.filter_job.is_none()
                                            && document.export_job.is_none();
                                        egui::Popup::menu(&response)
                                            .open_memory(popup_command)
                                            .show(|ui| {
                                                if ui
                                                    .add_enabled(
                                                        can_filter,
                                                        egui::Button::new(
                                                            "Filter to This Value",
                                                        ),
                                                    )
                                                    .clicked()
                                                {
                                                    interaction.filter_query = Some(FilterQuery {
                                                        predicates: vec![FilterPredicate {
                                                            column,
                                                            operator: FilterOperator::Equals,
                                                            value: value
                                                                .expect(
                                                                    "enabled filtering has a cell value",
                                                                )
                                                                .to_vec(),
                                                        }],
                                                        case_sensitivity:
                                                            filter_case_sensitivity,
                                                    });
                                                    ui.close();
                                                }
                                                if ui
                                                    .add_enabled(
                                                        can_filter,
                                                        egui::Button::new(
                                                            "Filter Out This Value",
                                                        ),
                                                    )
                                                    .clicked()
                                                {
                                                    interaction.filter_query = Some(FilterQuery {
                                                        predicates: vec![FilterPredicate {
                                                            column,
                                                            operator: FilterOperator::NotEquals,
                                                            value: value
                                                                .expect(
                                                                    "enabled filtering has a cell value",
                                                                )
                                                                .to_vec(),
                                                        }],
                                                        case_sensitivity:
                                                            filter_case_sensitivity,
                                                    });
                                                    ui.close();
                                                }
                                                ui.separator();
                                                if ui.button("Copy").clicked() {
                                                    interaction.copy_selection = true;
                                                    ui.close();
                                                }
                                            });
                                        if cell_focus_requested == Some((record_row, column)) {
                                            response.request_focus();
                                        }
                                        let keyboard_activate = active_cell_selected
                                            && response.has_focus()
                                            && ui.input(|input| {
                                                input.key_pressed(egui::Key::Enter)
                                                    || input.key_pressed(egui::Key::F2)
                                            });
                                        if document.cell_is_editable(source)
                                            && (response.double_clicked() || keyboard_activate)
                                        {
                                            begin_cell_edit = source.map(|source| {
                                                (record_row, column, source.to_vec())
                                            });
                                        }
                                        if reveal_cell == Some((record_row, column)) {
                                            response.scroll_to_me(Some(Align::Center));
                                        }
                                        if let Some(reason) =
                                            document.cell_edit_disabled_reason(source)
                                        {
                                            response.on_hover_text(reason);
                                        } else {
                                            response.on_hover_text(
                                                "Double-click or press Enter/F2 to edit",
                                            );
                                        }
                                    }
                                    },
                                );
                            });
                        }
                });
            });
            ui.painter().line_segment(
                [
                    egui::pos2(divider_left, divider_y),
                    egui::pos2(ui.min_rect().right(), divider_y),
                ],
                column_ruler_divider_stroke(ui.visuals()),
            );
        });

    if cancel_header_edit {
        active_header_edit = None;
    } else if commit_header_edit && let Some(edit) = active_header_edit.take() {
        let _ = document.rename_header(edit.column, edit.draft);
    }
    if cancel_cell_edit {
        active_cell_edit = None;
    } else if commit_cell_edit && let Some(edit) = active_cell_edit.take() {
        document.cell_edit = Some(edit);
        document.commit_cell_edit();
    }
    if let Some(column) = begin_header_edit {
        if let Some(edit) = active_cell_edit.take() {
            document.cell_edit = Some(edit);
            document.commit_cell_edit();
        }
        if let Some(edit) = active_header_edit.take() {
            let _ = document.rename_header(edit.column, edit.draft);
        }
        document.begin_header_edit(column);
    } else if let Some((row, column, source)) = begin_cell_edit {
        if let Some(edit) = active_header_edit.take() {
            let _ = document.rename_header(edit.column, edit.draft);
        }
        if let Some(edit) = active_cell_edit.take() {
            document.cell_edit = Some(edit);
            document.commit_cell_edit();
        }
        let _ = document.begin_cell_edit(row, column, source);
    } else {
        document.header_edit = active_header_edit;
        document.cell_edit = active_cell_edit;
    }
    document.cell_focus_requested = restore_cell_focus;
    interaction
}

fn scrollbar_thumb_height(track_height: f32, total_rows: u64, visible_rows: usize) -> f32 {
    if total_rows == 0 {
        return track_height;
    }
    let ratio = (visible_rows as f64 / total_rows as f64).min(1.0) as f32;
    (track_height * ratio).clamp(MIN_THUMB_HEIGHT.min(track_height), track_height)
}

fn configure_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new(20.0, FontFamily::Proportional),
    );
    style
        .text_styles
        .insert(TextStyle::Body, FontId::new(14.0, FontFamily::Proportional));
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(13.0, FontFamily::Monospace),
    );
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);

    let mut visuals = egui::Visuals::light();
    visuals.override_text_color = Some(Color32::from_rgb(24, 35, 42));
    visuals.hyperlink_color = QUARRY_YELLOW_TEXT;
    visuals.selection.bg_fill = QUARRY_YELLOW;
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, QUARRY_SELECTED_TEXT);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(238, 242, 244);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(220, 228, 233);
    visuals.widgets.active.bg_fill = Color32::from_rgb(204, 218, 227);
    visuals.faint_bg_color = Color32::from_rgb(235, 240, 242);
    visuals.extreme_bg_color = Color32::from_rgb(250, 251, 251);
    style.visuals = visuals;
    ctx.set_style(style);
}

fn panel_frame(fill: Color32) -> egui::Frame {
    egui::Frame::new()
        .fill(fill)
        .inner_margin(egui::Margin::symmetric(14, 10))
        .stroke(egui::Stroke::new(1.0_f32, Color32::from_rgb(200, 209, 213)))
}

fn field_text(field: &[u8]) -> String {
    let rendered = String::from_utf8_lossy(field)
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    if rendered.chars().count() <= 120 {
        rendered
    } else {
        rendered.chars().take(117).collect::<String>() + "..."
    }
}

fn accessible_header_name(name: &str) -> &str {
    if name.is_empty() {
        "unnamed header"
    } else {
        name
    }
}

#[cfg(test)]
fn selection_text(row: &Row, selection: GridSelection, max_bytes: usize) -> Result<String, String> {
    selection_fields_text(&row.fields, selection, max_bytes)
}

fn selection_fields_text(
    fields: &[Vec<u8>],
    selection: GridSelection,
    max_bytes: usize,
) -> Result<String, String> {
    match selection {
        GridSelection::Cell { column, .. } => {
            let mut output = String::new();
            append_clipboard_field(
                &mut output,
                fields.get(column).map_or(&[], Vec::as_slice),
                false,
                max_bytes,
            )?;
            Ok(output)
        }
        GridSelection::Row { .. } => {
            let mut output = String::new();
            for (index, field) in fields.iter().enumerate() {
                if index > 0 {
                    ensure_copy_capacity(&output, 1, max_bytes)?;
                    output.push('\t');
                }
                append_clipboard_field(&mut output, field, true, max_bytes)?;
            }
            Ok(output)
        }
    }
}

fn append_clipboard_field(
    output: &mut String,
    field: &[u8],
    quote_for_tsv: bool,
    max_bytes: usize,
) -> Result<(), String> {
    let text = std::str::from_utf8(field)
        .map_err(|_| "The selected data is not valid UTF-8 and cannot be copied yet.".to_owned())?;
    let needs_quotes = quote_for_tsv
        && text
            .bytes()
            .any(|byte| matches!(byte, b'\t' | b'\r' | b'\n' | b'"'));
    let added = text.len().saturating_add(if needs_quotes {
        2 + text.bytes().filter(|byte| *byte == b'"').count()
    } else {
        0
    });
    ensure_copy_capacity(output, added, max_bytes)?;

    if needs_quotes {
        output.push('"');
        for character in text.chars() {
            if character == '"' {
                output.push('"');
            }
            output.push(character);
        }
        output.push('"');
    } else {
        output.push_str(text);
    }
    Ok(())
}

fn ensure_copy_capacity(output: &str, added: usize, max_bytes: usize) -> Result<(), String> {
    if output.len().saturating_add(added) > max_bytes {
        Err(format!(
            "Copy exceeds the {} clipboard limit.",
            format_bytes(max_bytes as u64)
        ))
    } else {
        Ok(())
    }
}

fn sort_direction_label(direction: SortDirection) -> &'static str {
    match direction {
        SortDirection::Ascending => "Ascending",
        SortDirection::Descending => "Descending",
    }
}

fn serialized_field_upper_bound(value: &[u8]) -> u64 {
    u64::try_from(value.len())
        .unwrap_or(u64::MAX)
        .saturating_mul(2)
        .saturating_add(2)
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::Path;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use eframe::egui;

    use super::{
        Action, ActiveJobDisplay, ActiveJobKind, AppMessage, CaseSensitivity, ColumnCommand,
        ColumnView, DelimiterMode, Document, FIND_INPUT_ID, FilterOperator, FilterPredicate,
        FilterProgress, FilterQuery, GridColumnRequest, GridSelection, HeaderMode, IndexConfig,
        MAX_RENDERED_COLUMNS, MessageSeverity, OpenOptions, QuarryApp, REPLACE_INPUT_ID,
        ROW_NUMBER_WIDTH, Row, RowSelection, SOURCE_CHANGED_NOTICE, SearchMatch, Session,
        StructuralDialog, StructuralDialogAction, WorkingCopyState, active_job_controls,
        column_drop_position, column_ruler_divider_stroke, column_selection_fill, configure_style,
        estimate_sort_temporary_bytes, filter_button_label, filtered_export_file_name,
        first_active_job, footer_range_text, logical_viewport_start, max_viewport_start,
        notice_strip, page_controls, parse_data_row, parse_file_column, parse_move_position,
        rendered_column_range, row_for_scroll_fraction, save_as_file_name, scroll_fraction_for_row,
        search_controls, select_column, selected_split_column, selection_text, show_column_manager,
        show_empty_state, show_filter_manager, show_grid, show_grid_with_filter_case,
        show_structural_dialog, sort_merge_progress,
    };

    #[test]
    fn theme_uses_readable_soft_yellow_accents() {
        let ctx = egui::Context::default();
        configure_style(&ctx);
        let style = ctx.style();

        assert_eq!(style.visuals.selection.bg_fill, super::QUARRY_YELLOW);
        assert_eq!(
            style.visuals.selection.stroke.color,
            super::QUARRY_SELECTED_TEXT
        );
        assert_eq!(style.visuals.hyperlink_color, super::QUARRY_YELLOW_TEXT);
    }

    #[test]
    fn document_menu_is_bounded_accessible_and_reflects_dirty_state() {
        let mut app = QuarryApp::new(None, Instant::now());
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut frame = eframe::Frame::_new_kittest();
        let output = ctx.run(grid_input_with_width(860.0), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        let (menu_id, menu_node) = accessible_button(&output, "File menu");
        assert!(menu_node.supports_action(egui::accesskit::Action::Click));
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: menu_id,
                        data: None,
                    },
                )],
                ..grid_input_with_width(860.0)
            },
            |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            },
        );
        let output = ctx.run(grid_input_with_width(860.0), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        for label in [
            "Open…",
            "Reload from Disk",
            "Save",
            "Save As…",
            "Discard Changes",
        ] {
            let (_, node) = accessible_button(&output, label);
            assert_eq!(!node.is_disabled(), label == "Open…");
        }

        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let filename = format!(
            "quarry-document-menu-{}-{name}.csv",
            "very-long-filename-".repeat(7)
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(&filename);
        fs::write(&path, b"name,value\nfirst,1\n").unwrap();
        app.header_mode = HeaderMode::FirstRow;
        app.open_path(path.clone()).unwrap();

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let output = ctx.run(grid_input_with_width(860.0), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        let document_label = format!("File menu: {filename}");
        let (menu_id, menu_node) = accessible_button(&output, &document_label);
        let clean_bounds = menu_node
            .bounds()
            .expect("document menu should have bounds");
        let menu_width = clean_bounds.x1 - clean_bounds.x0;
        assert!(
            menu_width <= super::DOCUMENT_MENU_WIDTH as f64 + 1.0,
            "document menu width was {menu_width}"
        );
        assert_eq!(
            menu_node.description(),
            Some(path.to_string_lossy().as_ref())
        );
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: menu_id,
                        data: None,
                    },
                )],
                ..grid_input_with_width(860.0)
            },
            |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            },
        );
        let output = ctx.run(grid_input_with_width(860.0), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        for label in ["Open…", "Reload from Disk"] {
            assert!(!accessible_button(&output, label).1.is_disabled());
        }
        for label in ["Save", "Save As…", "Discard Changes"] {
            assert!(accessible_button(&output, label).1.is_disabled());
        }

        app.document
            .as_mut()
            .unwrap()
            .rename_header(0, "changed".into())
            .unwrap();
        let dirty_ctx = egui::Context::default();
        dirty_ctx.enable_accesskit();
        let output = dirty_ctx.run(grid_input_with_width(860.0), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        let (menu_id, menu_node) = accessible_button(&output, &document_label);
        assert_eq!(menu_node.bounds(), Some(clean_bounds));
        let modified_description = format!("Modified file at {}", path.display());
        assert_eq!(menu_node.description(), Some(modified_description.as_str()));
        let _ = dirty_ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: menu_id,
                        data: None,
                    },
                )],
                ..grid_input_with_width(860.0)
            },
            |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            },
        );
        let output = dirty_ctx.run(grid_input_with_width(860.0), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        for label in ["Open…", "Reload from Disk"] {
            assert!(accessible_button(&output, label).1.is_disabled());
        }
        for label in ["Save", "Save As…", "Discard Changes"] {
            assert!(!accessible_button(&output, label).1.is_disabled());
        }

        app.document.as_mut().unwrap().shutdown();
    }

    #[test]
    fn format_menu_is_accessible_and_cancel_discards_its_draft() {
        let mut app = QuarryApp::new(None, Instant::now());
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut frame = eframe::Frame::_new_kittest();
        let output = ctx.run(grid_input_with_width(860.0), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        let (_, format) = accessible_button(&output, "Format");
        assert!(format.is_disabled());

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("format-menu.csv");
        fs::write(&path, b"name,value\nfirst,1\n").unwrap();
        app.open_new_path(path).unwrap();

        let output = ctx.run(grid_input_with_width(860.0), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        let (format_id, format) = accessible_button(&output, "Format: Auto, Auto");
        assert!(!format.is_disabled());
        let bounds = format.bounds().expect("Format menu should have bounds");
        assert!(bounds.x1 - bounds.x0 <= super::FORMAT_MENU_WIDTH as f64 + 1.0);
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: format_id,
                        data: None,
                    },
                )],
                ..grid_input_with_width(860.0)
            },
            |ctx| eframe::App::update(&mut app, ctx, &mut frame),
        );
        let output = ctx.run(grid_input_with_width(860.0), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        let tree = output
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("open Format menu should be accessible");
        assert_eq!(
            accessible_button(&output, "Format: Auto, Auto")
                .1
                .description(),
            Some("Applied Auto, Auto. Detected Comma, Header row.")
        );
        assert!(
            accessible_button(&output, "Reopen with Changes")
                .1
                .is_disabled()
        );

        let tab_position = tree
            .nodes
            .iter()
            .find(|(_, node)| {
                node.label() == Some("Tab") && node.supports_action(egui::accesskit::Action::Click)
            })
            .and_then(|(_, node)| node.bounds())
            .map(|bounds| {
                egui::pos2(
                    ((bounds.x0 + bounds.x1) / 2.0) as f32,
                    ((bounds.y0 + bounds.y1) / 2.0) as f32,
                )
            })
            .expect("Tab should be an accessible format choice");
        let _ = ctx.run(
            egui::RawInput {
                events: vec![
                    egui::Event::PointerMoved(tab_position),
                    egui::Event::PointerButton {
                        pos: tab_position,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: tab_position,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..grid_input_with_width(860.0)
            },
            |ctx| eframe::App::update(&mut app, ctx, &mut frame),
        );
        let output = ctx.run(grid_input_with_width(860.0), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        assert_eq!(
            app.format_draft,
            Some((DelimiterMode::Tab, HeaderMode::Auto))
        );
        assert!(
            !accessible_button(&output, "Reopen with Changes")
                .1
                .is_disabled()
        );

        let reopen = accessible_button(&output, "Reopen with Changes").0;
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: reopen,
                        data: None,
                    },
                )],
                ..grid_input_with_width(860.0)
            },
            |ctx| eframe::App::update(&mut app, ctx, &mut frame),
        );
        assert_eq!(app.format_draft, None);
        assert_eq!(app.delimiter_mode, DelimiterMode::Tab);
        assert_eq!(app.header_mode, HeaderMode::Auto);

        let output = ctx.run(grid_input_with_width(860.0), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        let (format_id, _) = accessible_button(&output, "Format: Tab, Auto");
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: format_id,
                        data: None,
                    },
                )],
                ..grid_input_with_width(860.0)
            },
            |ctx| eframe::App::update(&mut app, ctx, &mut frame),
        );
        let output = ctx.run(grid_input_with_width(860.0), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        let cancel = accessible_button(&output, "Cancel").0;
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: cancel,
                        data: None,
                    },
                )],
                ..grid_input_with_width(860.0)
            },
            |ctx| eframe::App::update(&mut app, ctx, &mut frame),
        );
        assert_eq!(app.format_draft, None);
        assert_eq!(app.delimiter_mode, DelimiterMode::Tab);

        app.delimiter_mode = DelimiterMode::Semicolon;
        app.header_mode = HeaderMode::FirstRow;
        for width in [860.0, 1280.0] {
            let output = ctx.run(grid_input_with_width(width), |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            });
            let (_, format) = accessible_button(&output, "Format: Semicolon, Header row");
            let bounds = format.bounds().expect("Format menu should have bounds");
            assert!(bounds.x1 - bounds.x0 <= super::FORMAT_MENU_WIDTH as f64 + 1.0);
        }

        app.document.as_mut().unwrap().shutdown();
    }

    #[test]
    fn toolbar_stays_single_row_and_in_bounds_at_supported_widths() {
        let central_panel_top = |output: &egui::FullOutput, width: f32| {
            output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::epaint::Shape::Rect(rect)
                        if rect.fill == egui::Color32::from_rgb(244, 247, 248)
                            && rect.rect.width() >= width - 1.0
                            && rect.rect.height() > 100.0 =>
                    {
                        Some(rect.rect.top())
                    }
                    _ => None,
                })
                .next()
                .expect("central panel should be painted")
        };
        let assert_toolbar =
            |ctx: &egui::Context, output: &egui::FullOutput, labels: &[&str], width: f32| {
                let bounds = labels
                    .iter()
                    .map(|label| {
                        accessible_button(output, label)
                            .1
                            .bounds()
                            .unwrap_or_else(|| panic!("{label} should have bounds"))
                    })
                    .collect::<Vec<_>>();
                for pair in bounds.windows(2) {
                    assert!(
                        pair[0].x1 <= pair[1].x0,
                        "toolbar controls should remain left-to-right"
                    );
                }
                let first = bounds[0];
                let first_center = (first.y0 + first.y1) / 2.0;
                assert!(
                    bounds
                        .iter()
                        .all(|bounds| ((bounds.y0 + bounds.y1) / 2.0 - first_center).abs() < 1.0)
                );
                assert!(bounds.last().unwrap().x1 <= f64::from(width - 10.0));

                let tree = output
                    .platform_output
                    .accesskit_update
                    .as_ref()
                    .expect("toolbar should be accessible");
                let row_input = tree
                    .nodes
                    .iter()
                    .find_map(|(_, node)| {
                        (node.role() == egui::accesskit::Role::TextInput
                            && !node.labelled_by().is_empty())
                        .then_some(node)
                    })
                    .expect("Row input should be accessible");
                assert!(
                    row_input.labelled_by().iter().any(|label_id| {
                        tree.nodes
                            .iter()
                            .any(|(id, node)| id == label_id && node.value() == Some("Row"))
                    }),
                    "Row input labels: {:?}",
                    row_input
                        .labelled_by()
                        .iter()
                        .filter_map(|label_id| tree.nodes.iter().find(|(id, _)| id == label_id))
                        .collect::<Vec<_>>()
                );
                let row_input = row_input.bounds().expect("Row input should have bounds");
                assert!(
                    bounds[1].x1 <= row_input.x0,
                    "Format ended at {}, Row input started at {}",
                    bounds[1].x1,
                    row_input.x0
                );
                assert!(
                    row_input.x1 <= bounds[2].x0,
                    "Row input ended at {}, Go started at {}",
                    row_input.x1,
                    bounds[2].x0
                );
                assert!(((row_input.y0 + row_input.y1) / 2.0 - first_center).abs() < 1.0);

                let height =
                    egui::containers::panel::PanelState::load(ctx, egui::Id::new("quarry-toolbar"))
                        .expect("toolbar panel state should be stored")
                        .size()
                        .y;
                assert!(
                    (height - (super::TOOLBAR_HEIGHT + 2.0)).abs() < 0.1,
                    "toolbar frame height was {height}"
                );
                central_panel_top(output, width)
            };

        let mut app = QuarryApp::new(None, Instant::now());
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut frame = eframe::Frame::_new_kittest();
        let empty_labels = [
            "File menu",
            "Format",
            "Go",
            "Page Up",
            "Page Down",
            "Undo Change",
            "Redo Change",
            "Columns…",
            "Filters…",
            "Find",
        ];
        let widths = [860.0, 1280.0];
        let mut empty_tops = [0.0; 2];
        for (index, width) in widths.into_iter().enumerate() {
            let output = ctx.run(grid_input_with_width(width), |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            });
            empty_tops[index] = assert_toolbar(&ctx, &output, &empty_labels, width);
            for label in &empty_labels[1..] {
                assert!(accessible_button(&output, label).1.is_disabled());
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let filename = format!("{}toolbar.csv", "long-file-name-".repeat(12));
        let path = directory.path().join(&filename);
        let mut contents = b"name,value\n".to_vec();
        contents.extend_from_slice(&b"first,1\n".repeat(131_072));
        fs::write(&path, contents).unwrap();
        app.open_new_path(path).unwrap();
        let document = app.document.as_mut().unwrap();
        let initial_job = document.job.take().expect("index job should be active");
        initial_job.cancel();
        initial_job.wait().unwrap();
        let slow_job = document
            .session
            .start_indexing(IndexConfig {
                chunk_bytes: 1,
                ..IndexConfig::default()
            })
            .unwrap();
        document.progress = slow_job.progress();
        document.job = Some(slow_job);
        document.last_poll = Instant::now();
        let document_label = format!("File menu: {filename}");
        let document_labels = [
            document_label.as_str(),
            "Format: Auto, Auto",
            "Go",
            "Page Up",
            "Page Down",
            "Undo Change",
            "Redo Change",
            "Columns…",
            "Filters…",
            "Find",
        ];
        for (index, width) in widths.into_iter().enumerate() {
            let output = ctx.run(grid_input_with_width(width), |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            });
            assert_eq!(
                assert_toolbar(&ctx, &output, &document_labels, width),
                empty_tops[index]
            );
            assert!(
                app.document
                    .as_ref()
                    .unwrap()
                    .job
                    .as_ref()
                    .is_some_and(|job| !job.progress().done),
                "indexing should remain active during layout assertions"
            );
        }
        finish_index(app.document.as_mut().unwrap());

        for (index, width) in widths.into_iter().enumerate() {
            let output = ctx.run(grid_input_with_width(width), |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            });
            assert_eq!(
                assert_toolbar(&ctx, &output, &document_labels, width),
                empty_tops[index]
            );
        }

        app.document
            .as_mut()
            .unwrap()
            .rename_header(0, "changed".into())
            .unwrap();
        for (index, width) in widths.into_iter().enumerate() {
            let output = ctx.run(grid_input_with_width(width), |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            });
            assert_eq!(
                assert_toolbar(&ctx, &output, &document_labels, width),
                empty_tops[index]
            );
        }

        app.document.as_mut().unwrap().filter_query = Some(FilterQuery {
            predicates: (0..12)
                .map(|_| FilterPredicate {
                    column: 0,
                    operator: FilterOperator::Equals,
                    value: b"first".to_vec(),
                })
                .collect(),
            case_sensitivity: CaseSensitivity::Insensitive,
        });
        let filtered_labels = [
            document_label.as_str(),
            "Format: Auto, Auto",
            "Go",
            "Page Up",
            "Page Down",
            "Undo Change",
            "Redo Change",
            "Columns…",
            "Filters (12)…",
            "Find",
        ];
        for (index, width) in widths.into_iter().enumerate() {
            let output = ctx.run(grid_input_with_width(width), |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            });
            assert_eq!(
                assert_toolbar(&ctx, &output, &filtered_labels, width),
                empty_tops[index]
            );
            assert!(accessible_button(&output, "Find").1.is_disabled());
        }

        app.document.as_mut().unwrap().shutdown();
    }

    #[test]
    fn empty_state_routes_open_and_dropped_files_through_existing_actions() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut action = None;
        let output = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                action = show_empty_state(ui, false);
            });
        });
        let tree = output
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("empty state should be accessible");
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.value() == Some("Drop a delimited file here, or")
                || node.label() == Some("Drop a delimited file here, or")
        }));
        assert_eq!(
            click_accessible_button("Open…", |ui| show_empty_state(ui, false)),
            Some(Action::Choose)
        );

        for width in [860.0, 1967.0] {
            let ctx = egui::Context::default();
            let mut available = None;
            let output = ctx.run(grid_input_with_width(width), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    available = Some(ui.available_rect_before_wrap());
                    show_empty_state(ui, false);
                });
            });
            let available = available.unwrap();
            let drop_rect = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::epaint::Shape::Rect(rect)
                        if rect.fill == egui::Color32::from_rgb(250, 251, 251)
                            && rect.stroke.color == egui::Color32::from_rgb(200, 209, 213) =>
                    {
                        Some(rect.rect)
                    }
                    _ => None,
                })
                .expect("empty-state drop zone should be painted");
            assert!((drop_rect.center().x - available.center().x).abs() < 1.0);
            assert!((drop_rect.center().y - available.center().y).abs() < 1.0);
            assert!((drop_rect.width() - available.width().min(520.0)).abs() < 1.0);
            assert!((drop_rect.height() - available.height().min(112.0)).abs() < 1.0);
            assert!(available.contains_rect(drop_rect));
        }

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("dropped.csv");
        fs::write(&path, b"name,value\nfirst,1\n").unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        let hover_ctx = egui::Context::default();
        let mut frame = eframe::Frame::_new_kittest();
        let mut hover_input = grid_input();
        hover_input.hovered_files = vec![egui::HoveredFile {
            path: Some(path.clone()),
            ..Default::default()
        }];
        let hovered = hover_ctx.run(hover_input, |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        assert!(app.document.is_none(), "hovering must not open a file");
        assert!(hovered.shapes.iter().any(|shape| {
            matches!(
                &shape.shape,
                egui::epaint::Shape::Rect(rect)
                    if rect.fill == super::WARNING_FILL
                        && rect.stroke.color == super::QUARRY_YELLOW
                        && rect.stroke.width == 2.0
            )
        }));

        let mut drop_input = grid_input();
        drop_input.dropped_files = vec![egui::DroppedFile {
            path: Some(path.clone()),
            ..Default::default()
        }];
        let _ = hover_ctx.run(drop_input, |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        assert_eq!(app.document.as_ref().unwrap().session.path(), path);
        app.document.as_mut().unwrap().shutdown();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn app_install_lock_excludes_bundle_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("install.lock");
        let app_lock = super::acquire_install_lock_at(&path).unwrap();
        let blocked = std::process::Command::new("/usr/bin/lockf")
            .args(["-k", "-s", "-t", "0"])
            .arg(&path)
            .arg("/usr/bin/true")
            .status()
            .unwrap();
        assert_eq!(blocked.code(), Some(75));

        drop(app_lock);
        let acquired = std::process::Command::new("/usr/bin/lockf")
            .args(["-k", "-s", "-t", "0"])
            .arg(&path)
            .arg("/usr/bin/true")
            .status()
            .unwrap();
        assert!(acquired.success());
    }

    fn accessible_button<'a>(
        output: &'a egui::FullOutput,
        label: &str,
    ) -> (egui::accesskit::NodeId, &'a egui::accesskit::Node) {
        output
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("accessibility tree should be present")
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::Button && node.label() == Some(label)
            })
            .map(|(id, node)| (*id, node))
            .unwrap_or_else(|| panic!("{label} is not an accessible button"))
    }

    fn click_accessible_button(
        label: &str,
        mut render: impl FnMut(&mut egui::Ui) -> Option<Action>,
    ) -> Option<Action> {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut action = None;
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                action = render(ui);
            });
        });
        let (target, node) = accessible_button(&output, label);
        assert!(node.supports_action(egui::accesskit::Action::Click));

        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target,
                        data: None,
                    },
                )],
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    action = render(ui);
                });
            },
        );
        action
    }

    #[test]
    fn notice_sources_use_explicit_severity() {
        let missing = std::env::temp_dir().join("quarry-missing-notice-source.csv");
        let mut app = QuarryApp::new(None, Instant::now());

        app.open_path_and_report(missing);
        let failed_open = app.notice.as_ref().unwrap().severity;

        app.save_current();
        let blocked_save = app.notice.as_ref().unwrap().severity;

        app.copy_selection(&egui::Context::default());
        let blocked_copy = app.notice.as_ref().unwrap().severity;

        app.handle_dropped_paths(vec![None]);
        let ignored_drop = app.notice.as_ref().unwrap().severity;

        for (source, actual, expected) in [
            ("failed open", failed_open, MessageSeverity::Error),
            ("blocked save", blocked_save, MessageSeverity::Warning),
            ("blocked copy", blocked_copy, MessageSeverity::Warning),
            ("ignored drop", ignored_drop, MessageSeverity::Warning),
        ] {
            assert_eq!(actual, expected, "wrong severity for {source}");
        }
    }

    #[test]
    fn notice_strip_is_dismissible_and_announces_its_severity() {
        for (notice, expected_live) in [
            (
                AppMessage::error("Could not open the file."),
                egui::accesskit::Live::Assertive,
            ),
            (
                AppMessage::warning("Choose a column first."),
                egui::accesskit::Live::Polite,
            ),
        ] {
            let ctx = egui::Context::default();
            ctx.enable_accesskit();
            let mut dismissed = false;
            let output = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    dismissed = notice_strip(ui, &notice);
                });
            });
            assert!(!dismissed);
            assert!(
                output
                    .platform_output
                    .accesskit_update
                    .as_ref()
                    .unwrap()
                    .nodes
                    .iter()
                    .any(|(_, node)| node.live() == Some(expected_live)),
                "notice text should use the expected live-region priority"
            );
            let (target, node) = accessible_button(&output, "Dismiss");
            assert!(node.supports_action(egui::accesskit::Action::Click));

            let _ = ctx.run(
                egui::RawInput {
                    events: vec![egui::Event::AccessKitActionRequest(
                        egui::accesskit::ActionRequest {
                            action: egui::accesskit::Action::Click,
                            target,
                            data: None,
                        },
                    )],
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        dismissed = notice_strip(ui, &notice);
                    });
                },
            );
            assert!(dismissed);
        }
    }

    fn grid_input_with_width(width: f32) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, 780.0),
            )),
            ..Default::default()
        }
    }

    fn grid_input() -> egui::RawInput {
        grid_input_with_width(1280.0)
    }

    fn grid_control_id(
        ctx: &egui::Context,
        label: &str,
        document: &mut Document,
    ) -> egui::accesskit::NodeId {
        let output = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show_grid(ui, document).unwrap();
            });
        });
        output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present")
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::Button
                    && node.label() == Some(label)
                    && node.supports_action(egui::accesskit::Action::Click)
            })
            .map(|(id, _)| *id)
            .unwrap_or_else(|| panic!("{label} is not an accessible button"))
    }

    fn click_grid_control(
        label: &str,
        document: &mut Document,
    ) -> (egui::Context, egui::accesskit::NodeId) {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let target = grid_control_id(&ctx, label, document);

        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid(ui, document).unwrap();
                });
            },
        );
        (ctx, target)
    }

    fn render_grid(ctx: &egui::Context, document: &mut Document) -> egui::FullOutput {
        ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show_grid(ui, document).unwrap();
            });
        })
    }

    fn assert_column_ruler_divider(
        output: &egui::FullOutput,
        visuals: &egui::Visuals,
        header_count: usize,
    ) -> f32 {
        let divider_stroke = column_ruler_divider_stroke(visuals);
        let minimum_divider_span = 74.0 + header_count as f32 * 80.0;
        let divider_shapes = output
            .shapes
            .iter()
            .enumerate()
            .filter_map(|(index, shape)| match &shape.shape {
                egui::Shape::LineSegment { points, stroke }
                    if *stroke == divider_stroke
                        && points[0].y == points[1].y
                        && points[1].x - points[0].x >= minimum_divider_span =>
                {
                    Some((index, *points))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            divider_shapes.len(),
            1,
            "the ruler and header names should have one uninterrupted divider"
        );
        let (divider_index, points) = divider_shapes[0];
        let tint = column_selection_fill(visuals);
        let last_selection_fill = output
            .shapes
            .iter()
            .enumerate()
            .filter_map(|(index, shape)| {
                matches!(&shape.shape, egui::Shape::Rect(rect) if rect.fill == tint)
                    .then_some(index)
            })
            .max()
            .expect("selected columns should paint a persistent fill");
        assert!(
            divider_index > last_selection_fill,
            "the divider should be painted above selected column fills"
        );
        points[1].x - points[0].x
    }

    fn press_grid_key(
        ctx: &egui::Context,
        document: &mut Document,
        key: egui::Key,
        modifiers: egui::Modifiers,
    ) {
        let _ = ctx.run(
            egui::RawInput {
                modifiers,
                events: vec![egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                }],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid(ui, document).unwrap();
                });
            },
        );
    }

    fn click_column_manager_control(
        role: egui::accesskit::Role,
        label: &str,
        document: &Document,
    ) -> ColumnCommand {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut open = true;
        let mut search = String::new();
        let mut command = None;
        let output = ctx.run(grid_input(), |ctx| {
            command = show_column_manager(ctx, &mut open, &mut search, document);
        });
        let target = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present")
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == role
                    && node.label() == Some(label)
                    && node.supports_action(egui::accesskit::Action::Click)
            })
            .map(|(id, _)| *id)
            .unwrap_or_else(|| panic!("{label} is not an accessible control"));

        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                command = show_column_manager(ctx, &mut open, &mut search, document);
            },
        );
        command.unwrap_or_else(|| panic!("{label} did not produce a column command"))
    }

    fn finish_index(document: &mut Document) {
        let job = document.job.take().expect("index job should be active");
        document.index = Some(job.wait().unwrap());
        document.progress.done = true;
        document.progress.bytes_scanned = document.session.file_size;
    }

    fn finish_structural_edit(app: &mut QuarryApp) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let ready = app
                .document
                .as_mut()
                .expect("the document remains open")
                .poll_structural_edit()
                .unwrap();
            if let Some(ready) = ready {
                app.install_materialized_working_copy(ready).unwrap();
            }
            if app
                .document
                .as_ref()
                .is_some_and(|document| document.structural_job.is_none())
            {
                break;
            }
            assert!(Instant::now() < deadline, "column edit timed out");
            std::thread::yield_now();
        }
        finish_index(app.document.as_mut().unwrap());
    }

    fn finish_search(document: &mut Document) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !document
            .search_job
            .as_ref()
            .expect("search job should be active")
            .progress()
            .done
        {
            assert!(Instant::now() < deadline, "search timed out");
            std::thread::yield_now();
        }
        document.poll_search().unwrap();
    }

    fn finish_filter(document: &mut Document) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            document.poll_filter().unwrap();
            if document.filter_job.is_none() && !document.filter_rows_loading() {
                break;
            }
            assert!(Instant::now() < deadline, "filter timed out");
            std::thread::yield_now();
        }
    }

    fn finish_filter_read(document: &mut Document) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while document.filter_rows_loading() {
            document.poll_filter().unwrap();
            assert!(Instant::now() < deadline, "filter read timed out");
            std::thread::yield_now();
        }
    }

    fn finish_filtered_export(document: &mut Document) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while document.export_job.is_some() {
            document.poll_filtered_export().unwrap();
            assert!(Instant::now() < deadline, "filtered export timed out");
            std::thread::yield_now();
        }
    }

    fn finish_app_save(
        app: &mut QuarryApp,
        ctx: &egui::Context,
        frame: &mut eframe::Frame,
    ) -> egui::FullOutput {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !app
            .document
            .as_ref()
            .and_then(|document| document.save_job.as_ref())
            .expect("save job should be active")
            .progress()
            .done
        {
            assert!(Instant::now() < deadline, "save timed out");
            std::thread::yield_now();
        }
        ctx.run(grid_input(), |ctx| {
            eframe::App::update(app, ctx, frame);
        })
    }

    #[test]
    fn page_navigation_controls_are_clickable() {
        assert!(matches!(
            click_accessible_button("Page Up", |ui| page_controls(ui, true)),
            Some(Action::PageUp)
        ));
        assert!(matches!(
            click_accessible_button("Page Down", |ui| page_controls(ui, true)),
            Some(Action::PageDown)
        ));
    }

    #[test]
    fn page_keys_move_one_viewport_page() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("page-keys.csv");
        let mut file = File::create(&source).unwrap();
        writeln!(file, "name").unwrap();
        for row in 1..=250 {
            writeln!(file, "row{row}").unwrap();
        }
        drop(file);

        let mut document = Document::open(&source, OpenOptions::default()).unwrap();
        finish_index(&mut document);
        let mut app = QuarryApp::new(None, Instant::now());
        app.document = Some(document);
        let ctx = egui::Context::default();
        let mut frame = eframe::Frame::_new_kittest();
        let _ = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        let document = app.document.as_ref().unwrap();
        let first = document.viewport_start;
        let page = document.visible_rows as u64;

        for (key, expected) in [
            (egui::Key::PageDown, first + page),
            (egui::Key::PageUp, first),
        ] {
            let _ = ctx.run(
                egui::RawInput {
                    events: vec![egui::Event::Key {
                        key,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    }],
                    ..grid_input()
                },
                |ctx| {
                    eframe::App::update(&mut app, ctx, &mut frame);
                },
            );
            assert_eq!(app.document.as_ref().unwrap().viewport_start, expected);
        }
    }

    #[test]
    fn filter_manager_applies_a_labelled_multiline_equals_value_by_button() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-filter-a11y-{name}.csv"));
        fs::write(
            &path,
            b"name,note\nfirst,\"line one\nline two\"\nsecond,single line\n",
        )
        .unwrap();

        let mut app = QuarryApp::new(None, Instant::now());
        app.header_mode = HeaderMode::FirstRow;
        app.open_path(path.clone()).unwrap();
        app.filters_open = true;
        app.filter_rules[0].column_input = "2".to_owned();
        app.filter_rules[0].operator = FilterOperator::Equals;

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut action = None;
        let output = ctx.run(grid_input(), |ctx| {
            action = show_filter_manager(
                ctx,
                &mut app.filters_open,
                &mut app.filter_rules,
                &mut app.filter_match_case,
                app.document.as_ref().unwrap(),
            );
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present");
        assert_eq!(
            tree.nodes
                .iter()
                .filter(|(_, node)| {
                    matches!(
                        node.role(),
                        egui::accesskit::Role::TextInput
                            | egui::accesskit::Role::MultilineTextInput
                    ) && !node.labelled_by().is_empty()
                })
                .count(),
            2
        );
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::MultilineTextInput
                && !node.labelled_by().is_empty()
        }));
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::ComboBox && !node.labelled_by().is_empty()
        }));
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::CheckBox && node.label() == Some("Match case")
        }));
        let target = tree
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::Button
                    && node.label() == Some("Apply filters")
                    && node.supports_action(egui::accesskit::Action::Click)
            })
            .map(|(id, _)| *id)
            .expect("Apply filters should be accessible");

        ctx.memory_mut(|memory| {
            memory.request_focus(super::filter_value_input_id(0));
        });
        let _ = ctx.run(
            egui::RawInput {
                events: vec![
                    egui::Event::Text("line one".to_owned()),
                    egui::Event::Key {
                        key: egui::Key::Enter,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::Text("line two".to_owned()),
                ],
                ..grid_input()
            },
            |ctx| {
                action = show_filter_manager(
                    ctx,
                    &mut app.filters_open,
                    &mut app.filter_rules,
                    &mut app.filter_match_case,
                    app.document.as_ref().unwrap(),
                );
            },
        );
        assert!(action.is_none(), "Enter should insert data, not apply");
        assert_eq!(app.filter_rules[0].value_input, "line one\nline two");

        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                action = show_filter_manager(
                    ctx,
                    &mut app.filters_open,
                    &mut app.filter_rules,
                    &mut app.filter_match_case,
                    app.document.as_ref().unwrap(),
                );
            },
        );
        assert!(matches!(action, Some(Action::ApplyFilter)));

        app.apply(&ctx, action.unwrap());
        let document = app.document.as_mut().unwrap();
        finish_filter(document);
        assert_eq!(document.available_filter_rows(), 1);
        assert_eq!(document.visible_filter_rows()[0].fields[0], b"first");
        assert_eq!(
            document.visible_filter_rows()[0].fields[1],
            b"line one\nline two"
        );
        document.shutdown();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn zero_match_filter_footer_omits_an_invalid_range() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("empty-filter.csv");
        fs::write(&path, b"name,status\nfirst,keep\n").unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"missing".to_vec(),
            ))
            .unwrap();
        finish_filter(&mut document);

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        configure_style(&ctx);
        let mut app = QuarryApp::new(None, Instant::now());
        app.document = Some(document);
        let mut frame = eframe::Frame::_new_kittest();
        let output = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        let painted_text = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(painted_text.contains(&"No matching rows"));
        assert!(
            painted_text
                .iter()
                .all(|text| !text.starts_with("matches "))
        );

        app.document.as_mut().unwrap().shutdown();
    }

    #[test]
    fn filter_manager_adds_and_removes_accessible_rules_and_suppresses_copy() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-filter-rules-a11y-{name}.csv"));
        fs::write(&path, b"name,status\nfirst,keep\n").unwrap();
        let mut document = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut open = true;
        let mut rules = vec![super::FilterRuleDraft::default()];
        let mut match_case = false;

        let output = ctx.run(grid_input(), |ctx| {
            let _ = show_filter_manager(ctx, &mut open, &mut rules, &mut match_case, &document);
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present");
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::Button
                && node.label() == Some("Remove rule 1")
                && node.is_disabled()
        }));
        let add_target = tree
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::Button
                    && node.label() == Some("Add rule")
                    && node.supports_action(egui::accesskit::Action::Click)
            })
            .map(|(id, _)| *id)
            .expect("Add rule should be accessible");

        ctx.memory_mut(|memory| memory.request_focus(super::filter_value_input_id(0)));
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: add_target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                let _ = show_filter_manager(ctx, &mut open, &mut rules, &mut match_case, &document);
            },
        );
        assert_eq!(rules.len(), 2);
        assert!(!ctx.memory(|memory| {
            memory
                .focused()
                .is_some_and(|focused| super::is_filter_text_input(focused, rules.len()))
        }));
        rules[0].value_input = "first".into();
        rules[1].value_input = "second".into();

        let output = ctx.run(grid_input(), |ctx| {
            let _ = show_filter_manager(ctx, &mut open, &mut rules, &mut match_case, &document);
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present");
        assert_eq!(
            tree.nodes
                .iter()
                .filter(|(_, node)| {
                    matches!(
                        node.role(),
                        egui::accesskit::Role::TextInput
                            | egui::accesskit::Role::MultilineTextInput
                    ) && !node.labelled_by().is_empty()
                })
                .count(),
            4
        );
        assert_eq!(
            tree.nodes
                .iter()
                .filter(|(_, node)| {
                    node.role() == egui::accesskit::Role::ComboBox && !node.labelled_by().is_empty()
                })
                .count(),
            2
        );
        let remove_target = tree
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::Button
                    && node.label() == Some("Remove rule 2")
                    && node.supports_action(egui::accesskit::Action::Click)
            })
            .map(|(id, _)| *id)
            .expect("Remove rule 2 should be enabled with two rules");

        for focused in [
            super::filter_column_input_id(1),
            super::filter_value_input_id(1),
        ] {
            ctx.memory_mut(|memory| memory.request_focus(focused));
            let mut selection_copy = true;
            let _ = ctx.run(
                egui::RawInput {
                    events: vec![egui::Event::Copy],
                    ..grid_input()
                },
                |ctx| {
                    selection_copy = super::selection_copy_requested(ctx, rules.len(), None, None);
                },
            );
            assert!(!selection_copy);
        }

        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: remove_target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                let _ = show_filter_manager(ctx, &mut open, &mut rules, &mut match_case, &document);
            },
        );
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].value_input, "first");
        assert!(!ctx.memory(|memory| {
            memory
                .focused()
                .is_some_and(|focused| super::is_filter_text_input(focused, 2))
        }));

        document.shutdown();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn applying_same_column_values_keeps_alternatives_and_narrows_other_columns() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-filter-groups-{name}.csv"));
        fs::write(
            &path,
            b"id,state,status\n1,TX,active\n2,FL,active\n3,CA,active\n4,TX,inactive\n",
        )
        .unwrap();

        let mut app = QuarryApp::new(None, Instant::now());
        app.header_mode = HeaderMode::FirstRow;
        app.open_path(path.clone()).unwrap();
        app.filter_rules = vec![
            super::FilterRuleDraft {
                column_input: "2".into(),
                operator: FilterOperator::Equals,
                value_input: "tx".into(),
            },
            super::FilterRuleDraft {
                column_input: "2".into(),
                operator: FilterOperator::Equals,
                value_input: "fL".into(),
            },
            super::FilterRuleDraft {
                column_input: "3".into(),
                operator: FilterOperator::NotEquals,
                value_input: "INACTIVE".into(),
            },
        ];
        assert_eq!(
            filter_button_label(app.document.as_ref().unwrap().filter_query.as_ref()),
            "Filters…"
        );
        let ctx = egui::Context::default();
        app.apply(&ctx, Action::ApplyFilter);
        assert!(app.notice.is_none());

        let document = app.document.as_mut().unwrap();
        finish_filter(document);
        assert_eq!(document.filter_query.as_ref().unwrap().predicates.len(), 3);
        assert_eq!(
            filter_button_label(document.filter_query.as_ref()),
            "Filters (3)…"
        );
        assert_eq!(
            document.filter_query.as_ref().unwrap().case_sensitivity,
            CaseSensitivity::Insensitive
        );
        assert_eq!(document.available_filter_rows(), 2);
        assert_eq!(
            document
                .visible_filter_rows()
                .iter()
                .map(|row| row.fields[0].as_slice())
                .collect::<Vec<_>>(),
            vec![b"1".as_slice(), b"2".as_slice()]
        );
        assert!(document.visible_filter_rows().iter().all(|row| {
            matches!(row.fields[1].as_slice(), b"TX" | b"FL")
                && row.fields[2].as_slice() == b"active"
        }));

        document.shutdown();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn filtered_export_control_is_accessible_only_for_a_completed_active_filter() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-export-control-{name}.csv"));
        fs::write(&path, b"name,status\nfirst,keep\nsecond,skip\n").unwrap();
        let mut document = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut open = true;
        let mut rules = vec![super::FilterRuleDraft::default()];
        let mut match_case = false;
        let output = ctx.run(grid_input(), |ctx| {
            assert!(
                show_filter_manager(ctx, &mut open, &mut rules, &mut match_case, &document,)
                    .is_none()
            );
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present");
        assert!(!tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::Button
                && node.label() == Some("Export Filtered Rows…")
        }));

        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"keep".to_vec(),
            ))
            .unwrap();
        assert_eq!(footer_range_text(&document), "Finding matching rows…");
        finish_filter(&mut document);
        assert!(document.is_filtered_export_ready());
        let output = ctx.run(grid_input(), |ctx| {
            let _ = show_filter_manager(ctx, &mut open, &mut rules, &mut match_case, &document);
        });
        let target = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present")
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::Button
                    && node.label() == Some("Export Filtered Rows…")
                    && node.supports_action(egui::accesskit::Action::Click)
            })
            .map(|(id, _)| *id)
            .expect("completed filter should enable filtered export");
        let mut action = None;
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                action =
                    show_filter_manager(ctx, &mut open, &mut rules, &mut match_case, &document);
            },
        );
        assert_eq!(action, Some(Action::ChooseFilteredExport));

        document.shutdown();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn header_renames_keep_lossless_source_identity_and_clear_when_restored() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-header-rename-{name}.csv"));
        let long_header = "x".repeat(140);
        let original_header = "\u{feff}line\nname";
        let source = format!("\"{original_header}\",,{long_header}\nfirst,second,third\n");
        fs::write(&path, source.as_bytes()).unwrap();
        let mut document = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();

        assert_eq!(document.source_header_name(0), Some(original_header));
        assert_eq!(document.source_header_name(1), Some(""));
        assert_eq!(document.source_header_name(2), Some(long_header.as_str()));
        assert!(document.column_name(2).ends_with("..."));

        document.begin_header_edit(0);
        assert_eq!(
            document
                .header_edit
                .as_ref()
                .map(|edit| edit.draft.as_str()),
            Some(original_header)
        );
        document.header_edit.as_mut().unwrap().draft = "renamed".into();
        document.move_column(0, 2).unwrap();
        assert!(document.is_dirty());
        assert_eq!(document.column_name(0), "renamed");

        document.set_column_shown(0, false).unwrap();
        document.set_column_shown(0, true).unwrap();
        assert_eq!(document.column_name(0), "renamed");
        assert_eq!(
            document.header_renames.get(&0).map(String::as_str),
            Some("renamed")
        );

        document.rename_header(0, original_header.into()).unwrap();
        assert!(!document.is_dirty());
        document.rename_header(1, "Column 2".into()).unwrap();
        assert!(document.is_dirty());
        document.rename_header(1, String::new()).unwrap();
        assert!(!document.is_dirty());
        assert_eq!(fs::read(&path).unwrap(), source.as_bytes());

        document.shutdown();
        let mut no_header = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::NoHeader,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        assert!(!no_header.header_is_editable(0));
        no_header.begin_header_edit(0);
        assert!(no_header.header_edit.is_none());
        assert_eq!(
            no_header.rename_header(0, "renamed".into()).unwrap_err(),
            "Only columns in the source header row can be renamed."
        );
        no_header.shutdown();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn accessible_header_editor_commits_cancels_and_keeps_copy_in_the_editor() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-header-editor-{name}.csv"));
        fs::write(&path, b"name,value\nfirst,1\n").unwrap();
        let mut document = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        document.selection = Some(GridSelection::Cell { row: 1, column: 0 });

        let (ctx, _) = click_grid_control("Rename file column 1 (name)", &mut document);
        assert_eq!(document.header_edit.as_ref().unwrap().column, 0);
        document.header_edit.as_mut().unwrap().draft = "renamed".into();
        let output = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show_grid(ui, &mut document).unwrap();
            });
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present");
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::TextInput
                && node.label() == Some("New name for file column 1")
        }));
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid(ui, &mut document).unwrap();
                });
            },
        );
        assert!(document.header_edit.is_none());
        assert_eq!(document.column_name(0), "renamed");

        let _ = click_grid_control("Rename file column 1 (renamed)", &mut document);
        document.header_edit.as_mut().unwrap().draft = "cancelled".into();
        let _ = render_grid(&ctx, &mut document);
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid(ui, &mut document).unwrap();
                });
            },
        );
        assert!(document.header_edit.is_none());
        assert_eq!(document.column_name(0), "renamed");

        document.begin_header_edit(0);
        let mut selection_copy = true;
        let _ = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show_grid(ui, &mut document).unwrap();
            });
        });
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Copy],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid(ui, &mut document).unwrap();
                });
                selection_copy = super::selection_copy_requested(ctx, 0, Some(0), None);
            },
        );
        assert!(!selection_copy);

        document.shutdown();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn inline_cell_editor_is_accessible_sparse_and_lossless() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-cell-editor-{name}.csv"));
        fs::write(&path, b"name,value\nfirst,1\nsecond,2\n").unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);

        let (ctx, cell_control) =
            click_grid_control("Select row 1, column 2 (value): 1", &mut document);
        press_grid_key(&ctx, &mut document, egui::Key::Enter, egui::Modifiers::NONE);
        assert_eq!(
            document
                .cell_edit
                .as_ref()
                .map(|edit| (edit.row, edit.column, edit.draft.as_str())),
            Some((1, 1, "1"))
        );

        let output = render_grid(&ctx, &mut document);
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present");
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::MultilineTextInput
                && node.label() == Some("Edit data row 1, file column 2 (value)")
        }));

        press_grid_key(
            &ctx,
            &mut document,
            egui::Key::Enter,
            egui::Modifiers::SHIFT,
        );
        assert_eq!(document.cell_edit.as_ref().unwrap().draft, "1\n");
        document.cell_edit.as_mut().unwrap().draft = "changed\r\nvalue".into();
        document.search_query = b"stale".to_vec();
        document.last_match = Some(SearchMatch {
            row: 1,
            column: 1,
            record_offset: 0,
        });
        document.search_status = Some("Stale search result".into());
        document.reveal_cell = Some((1, 1));

        let mut selection_copy = true;
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Copy],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid(ui, &mut document).unwrap();
                });
                selection_copy = super::selection_copy_requested(ctx, 0, None, Some((1, 1)));
            },
        );
        assert!(!selection_copy);

        press_grid_key(&ctx, &mut document, egui::Key::Enter, egui::Modifiers::NONE);
        assert!(document.cell_edit.is_none());
        let output = render_grid(&ctx, &mut document);
        assert_eq!(
            output
                .platform_output
                .accesskit_update
                .expect("accessibility tree should be present")
                .focus,
            cell_control
        );
        assert_eq!(
            document.cell_edits.get(&(1, 1)).map(Vec::as_slice),
            Some(b"changed\r\nvalue".as_slice())
        );
        assert!(document.search_query.is_empty());
        assert!(document.last_match.is_none());
        assert!(document.search_status.is_none());
        assert!(document.reveal_cell.is_none());
        assert!(document.is_dirty());
        assert_eq!(document.copy_selection_text().unwrap(), "changed\r\nvalue");
        document.start_find_next(b"first").unwrap();
        finish_search(&mut document);
        let found = document.last_match.as_ref().unwrap();
        assert_eq!((found.row, found.column), (1, 0));
        assert_eq!(
            document
                .start_filter(FilterQuery::single(
                    0,
                    FilterOperator::Contains,
                    b"first".to_vec(),
                ))
                .unwrap_err(),
            "Save or discard cell edits before filtering the source file."
        );

        document.set_column_shown(1, false).unwrap();
        document.set_column_shown(1, true).unwrap();
        document.move_column(1, 0).unwrap();
        document.set_visible_rows(1).unwrap();
        document.navigate(2).unwrap();
        document.navigate(1).unwrap();
        assert_eq!(
            document.cell_edits.get(&(1, 1)).map(Vec::as_slice),
            Some(b"changed\r\nvalue".as_slice())
        );
        let _ = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show_grid(ui, &mut document).unwrap();
            });
        });

        let cell_control = grid_control_id(
            &ctx,
            "Select row 1, column 2 (value): changed\\r\\nvalue",
            &mut document,
        );
        document.begin_cell_edit(1, 1, b"1".to_vec()).unwrap();
        assert_eq!(
            document.cell_edit.as_ref().unwrap().draft,
            "changed\r\nvalue"
        );
        document.cell_edit.as_mut().unwrap().draft = "cancelled".into();
        let _ = render_grid(&ctx, &mut document);
        press_grid_key(
            &ctx,
            &mut document,
            egui::Key::Escape,
            egui::Modifiers::NONE,
        );
        assert!(document.cell_edit.is_none());
        let output = render_grid(&ctx, &mut document);
        assert_eq!(
            output
                .platform_output
                .accesskit_update
                .expect("accessibility tree should be present")
                .focus,
            cell_control
        );
        assert_eq!(
            document.cell_edits.get(&(1, 1)).map(Vec::as_slice),
            Some(b"changed\r\nvalue".as_slice())
        );

        document.begin_cell_edit(1, 1, b"1".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "focus loss".into();
        let _ = render_grid(&ctx, &mut document);
        let mut other_text = String::new();
        let _ = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut other_text)
                        .id(egui::Id::new("cell-focus-loss-target")),
                )
                .request_focus();
                show_grid(ui, &mut document).unwrap();
            });
        });
        let _ = render_grid(&ctx, &mut document);
        assert!(document.cell_edit.is_none());
        assert_eq!(
            document.cell_edits.get(&(1, 1)).map(Vec::as_slice),
            Some(b"focus loss".as_slice())
        );

        document.set_visible_rows(1).unwrap();
        document.begin_cell_edit(1, 1, b"1".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "wheel navigation".into();
        document.scroll_rows(1).unwrap();
        assert!(document.cell_edit.is_none());
        assert_eq!(
            document.cell_edits.get(&(1, 1)).map(Vec::as_slice),
            Some(b"wheel navigation".as_slice())
        );
        document.navigate(1).unwrap();

        document.begin_cell_edit(1, 1, b"1".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "1".into();
        document.commit_cell_edit();
        assert!(!document.is_dirty());
        assert!(document.cell_edits.is_empty());
        assert_eq!(
            document.cell_edit_disabled_reason(None),
            Some("This row has no cell in this file column.")
        );
        assert_eq!(
            document.cell_edit_disabled_reason(Some(&[0xff])),
            Some("This cell is not valid UTF-8 and cannot be edited.")
        );
        document
            .start_filter(FilterQuery::single(
                0,
                FilterOperator::Contains,
                b"first".to_vec(),
            ))
            .unwrap();
        assert_eq!(
            document.begin_cell_edit(1, 1, b"1".to_vec()).unwrap_err(),
            "Clear the filter before editing data cells."
        );

        document.shutdown();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn save_button_and_command_s_replace_the_current_file_and_reopen_it_clean() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-save-ui-{name}.csv"));
        fs::write(&path, b"name,value\nfirst,1\n").unwrap();

        let mut app = QuarryApp::new(None, Instant::now());
        app.header_mode = HeaderMode::FirstRow;
        app.open_path(path.clone()).unwrap();
        let document = app.document.as_mut().unwrap();
        document.rename_header(0, "button_name".into()).unwrap();
        document.begin_cell_edit(1, 1, b"1".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "line one\nline two".into();

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut frame = eframe::Frame::_new_kittest();
        app.close_confirmation_open = true;
        let _ = ctx.run(
            egui::RawInput {
                modifiers: egui::Modifiers::COMMAND,
                events: vec![egui::Event::Key {
                    key: egui::Key::S,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::COMMAND,
                }],
                ..grid_input()
            },
            |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            },
        );
        assert!(app.close_confirmation_open);
        assert!(app.document.as_ref().unwrap().save_job.is_none());
        assert!(app.document.as_ref().unwrap().is_dirty());
        app.keep_editing();

        let output = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        let filename = path.file_name().unwrap().to_string_lossy();
        let (document_menu, _) = accessible_button(&output, &format!("File menu: {filename}"));
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: document_menu,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            },
        );
        let output = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        let (save_button, save_node) = accessible_button(&output, "Save");
        assert!(!save_node.is_disabled());
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: save_button,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            },
        );
        assert!(app.document.as_ref().unwrap().save_job.is_some());
        let _ = finish_app_save(&mut app, &ctx, &mut frame);
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.session.path(), path);
        assert_eq!(document.column_name(0), "button_name");
        assert!(!document.is_dirty());
        assert_eq!(app.delimiter_mode, DelimiterMode::Auto);
        assert_eq!(app.header_mode, HeaderMode::FirstRow);
        assert_eq!(
            fs::read(&path).unwrap(),
            b"button_name,value\nfirst,\"line one\nline two\"\n"
        );

        let document = app.document.as_mut().unwrap();
        document.begin_header_edit(0);
        document.header_edit.as_mut().unwrap().draft = "shortcut_name".into();
        let _ = ctx.run(
            egui::RawInput {
                modifiers: egui::Modifiers::COMMAND,
                events: vec![egui::Event::Key {
                    key: egui::Key::S,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::COMMAND,
                }],
                ..grid_input()
            },
            |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            },
        );
        assert!(app.document.as_ref().unwrap().save_job.is_some());
        let _ = finish_app_save(&mut app, &ctx, &mut frame);
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.session.path(), path);
        assert_eq!(document.column_name(0), "shortcut_name");
        assert!(!document.is_dirty());
        assert_eq!(
            fs::read(&path).unwrap(),
            b"shortcut_name,value\nfirst,\"line one\nline two\"\n"
        );

        app.document
            .as_mut()
            .unwrap()
            .rename_header(0, "close_name".into())
            .unwrap();
        app.close_confirmation_open = true;
        let output = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        let save_and_close = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present")
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::Button
                    && node.label() == Some("Save and Close")
                    && node.supports_action(egui::accesskit::Action::Click)
            })
            .map(|(id, _)| *id)
            .expect("Save and Close should be an accessible button");
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: save_and_close,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            },
        );
        assert!(app.close_after_save);
        assert!(app.document.as_ref().unwrap().save_job.is_some());
        let close_output = finish_app_save(&mut app, &ctx, &mut frame);
        assert!(
            close_output
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .unwrap()
                .commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::Close))
        );
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.column_name(0), "close_name");
        assert!(!document.is_dirty());
        assert_eq!(
            fs::read(&path).unwrap(),
            b"close_name,value\nfirst,\"line one\nline two\"\n"
        );

        app.document.as_mut().unwrap().shutdown();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn headerless_cell_edits_stay_headerless_after_save_and_save_as() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("quarry-headerless-save-{name}.csv"));
        let destination =
            std::env::temp_dir().join(format!("quarry-headerless-save-{name}-copy.csv"));
        fs::write(&source, b"\xEF\xBB\xBFfirst,1\nsecond,2\n").unwrap();

        let mut app = QuarryApp::new(None, Instant::now());
        app.header_mode = HeaderMode::NoHeader;
        app.open_path(source.clone()).unwrap();
        finish_index(app.document.as_mut().unwrap());
        let document = app.document.as_mut().unwrap();
        let source_cell = document.visible_row(0).unwrap().1[0].clone();
        assert_eq!(source_cell, b"first");
        document.begin_cell_edit(0, 0, source_cell).unwrap();
        assert_eq!(document.cell_edit.as_ref().unwrap().draft, "first");
        document.cell_edit.as_mut().unwrap().draft = "saved".into();

        let ctx = egui::Context::default();
        let mut frame = eframe::Frame::_new_kittest();
        assert!(app.save_current());
        let _ = finish_app_save(&mut app, &ctx, &mut frame);
        assert!(app.notice.is_none());
        assert_eq!(
            app.footer_status.as_ref().map(|message| message.severity),
            Some(MessageSeverity::Status)
        );
        let document = app.document.as_ref().unwrap();
        assert!(!document.session.dialect.has_header);
        assert_eq!(document.data_start, 0);
        assert!(!document.is_dirty());
        assert_eq!(app.delimiter_mode, DelimiterMode::Auto);
        assert_eq!(app.header_mode, HeaderMode::NoHeader);
        assert_eq!(
            fs::read(&source).unwrap(),
            b"\xEF\xBB\xBFsaved,1\nsecond,2\n"
        );

        finish_index(app.document.as_mut().unwrap());
        let document = app.document.as_mut().unwrap();
        document.begin_cell_edit(1, 1, b"2".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "saved as".into();
        assert!(app.save_as_picker_result(Some(destination.clone())));
        let _ = finish_app_save(&mut app, &ctx, &mut frame);
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.session.path(), destination);
        assert!(!document.session.dialect.has_header);
        assert_eq!(document.data_start, 0);
        assert!(!document.is_dirty());
        assert_eq!(app.delimiter_mode, DelimiterMode::Auto);
        assert_eq!(app.header_mode, HeaderMode::NoHeader);
        assert_eq!(
            fs::read(&source).unwrap(),
            b"\xEF\xBB\xBFsaved,1\nsecond,2\n"
        );
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"\xEF\xBB\xBFsaved,1\nsecond,saved as\n"
        );

        app.document.as_mut().unwrap().shutdown();
        for path in [source, destination] {
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn successful_save_with_failed_reload_drops_the_stale_document() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-save-reload-{name}.csv"));
        let moved = std::env::temp_dir().join(format!("quarry-save-reload-{name}-moved.csv"));
        fs::write(&path, b"name,value\nfirst,1\n").unwrap();

        let mut app = QuarryApp::new(None, Instant::now());
        app.header_mode = HeaderMode::FirstRow;
        app.open_path(path.clone()).unwrap();
        app.find_bar_open = true;
        app.replace_expanded = true;
        {
            let document = app.document.as_mut().unwrap();
            document.rename_header(0, "renamed".into()).unwrap();
            document.start_save().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !document.save_job.as_ref().unwrap().progress().done {
                assert!(Instant::now() < deadline, "save timed out");
                std::thread::yield_now();
            }
        }
        fs::rename(&path, &moved).unwrap();

        let ctx = egui::Context::default();
        let mut frame = eframe::Frame::_new_kittest();
        let _ = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        assert!(app.document.is_none());
        assert!(!app.find_bar_open);
        assert!(!app.replace_expanded);
        assert!(
            app.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("could not reload it"))
        );
        assert_eq!(fs::read(&moved).unwrap(), b"renamed,value\nfirst,1\n");

        fs::remove_file(moved).unwrap();
    }

    #[test]
    fn source_change_conflict_freezes_stale_navigation_until_reopen() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-save-conflict-{name}.csv"));
        let moved = path.with_extension("moved.csv");
        let destination = path.with_extension("saved-as.csv");
        fs::write(&path, b"name,value\nfirst,1\n").unwrap();

        let mut app = QuarryApp::new(None, Instant::now());
        app.header_mode = HeaderMode::FirstRow;
        app.open_path(path.clone()).unwrap();
        app.document
            .as_mut()
            .unwrap()
            .rename_header(0, "renamed".into())
            .unwrap();
        fs::rename(&path, &moved).unwrap();

        assert!(!app.save_current());
        let document = app.document.as_ref().unwrap();
        assert!(document.source_changed);
        assert!(document.is_dirty());
        assert_eq!(document.index_status(), "Source changed");
        assert!(document.index.is_none());
        assert!(document.buffered_rows.is_empty());
        let viewport_start = document.viewport_start;

        let ctx = egui::Context::default();
        app.apply(&ctx, Action::PageDown);
        assert_eq!(app.notice.as_deref(), Some(SOURCE_CHANGED_NOTICE));
        assert_eq!(
            app.document.as_ref().unwrap().viewport_start,
            viewport_start
        );

        app.apply(&ctx, Action::DiscardChanges);
        fs::rename(&moved, &path).unwrap();
        app.reload_document();
        let document = app.document.as_ref().unwrap();
        assert!(!document.source_changed);
        assert!(!document.is_dirty());
        assert_eq!(document.session.first_rows[1].fields[0], b"first");

        app.document
            .as_mut()
            .unwrap()
            .rename_header(0, "save_as_name".into())
            .unwrap();
        fs::rename(&path, &moved).unwrap();
        assert!(!app.save_as_picker_result(Some(destination.clone())));
        let document = app.document.as_ref().unwrap();
        assert!(document.source_changed);
        assert!(document.is_dirty());
        assert!(document.index.is_none());
        assert!(document.buffered_rows.is_empty());
        let viewport_start = document.viewport_start;
        app.apply(&ctx, Action::PageDown);
        assert_eq!(app.notice.as_deref(), Some(SOURCE_CHANGED_NOTICE));
        assert_eq!(
            app.document.as_ref().unwrap().viewport_start,
            viewport_start
        );
        assert!(!destination.exists());

        app.apply(&ctx, Action::DiscardChanges);
        fs::rename(&moved, &path).unwrap();
        app.document.as_mut().unwrap().shutdown();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn dirty_lifecycle_blocks_loss_and_save_as_reopens_the_edited_copy() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("quarry-save-as-ui-{name}.csv"));
        let other = std::env::temp_dir().join(format!("quarry-save-as-ui-{name}-other.csv"));
        let unopenable = std::env::temp_dir().join(format!("quarry-save-as-ui-{name}-removed.csv"));
        let destination = std::env::temp_dir().join(format!("quarry-save-as-ui-{name}-edited.csv"));
        let source_bytes = b"name,value\nfirst,1\n";
        fs::write(&source, source_bytes).unwrap();
        fs::write(&other, b"other,value\nsecond,2\n").unwrap();

        let mut app = QuarryApp::new(None, Instant::now());
        app.header_mode = HeaderMode::FirstRow;
        app.open_path(source.clone()).unwrap();
        let document = app.document.as_mut().unwrap();
        document.rename_header(0, "renamed".into()).unwrap();
        document.begin_cell_edit(1, 1, b"1".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "edited".into();
        let ctx = egui::Context::default();
        let mut close_input = grid_input();
        close_input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let close_output = ctx.run(close_input, |ctx| app.intercept_dirty_close(ctx));
        assert!(app.close_confirmation_open);
        assert!(
            close_output
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .unwrap()
                .commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::CancelClose))
        );
        app.close_after_save = true;
        app.keep_editing();
        assert!(!app.close_confirmation_open);
        assert!(!app.close_after_save);
        assert_eq!(
            app.open_path(other.clone()).unwrap_err(),
            "Discard or save your changes before opening another file."
        );
        app.handle_dropped_paths(vec![Some(other.clone())]);
        assert_eq!(app.document.as_ref().unwrap().session.path(), source);
        assert_eq!(
            app.document
                .as_mut()
                .unwrap()
                .start_filtered_export(destination.clone())
                .unwrap_err(),
            "Save or discard your changes before exporting filtered rows."
        );

        app.document
            .as_mut()
            .unwrap()
            .start_save_as(unopenable.clone())
            .unwrap();
        let mut saving_close_input = grid_input();
        saving_close_input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let saving_close_output = ctx.run(saving_close_input, |ctx| app.intercept_dirty_close(ctx));
        assert!(app.close_after_save);
        assert!(!app.close_confirmation_open);
        assert!(
            saving_close_output
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .unwrap()
                .commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::CancelClose))
        );
        let document = app.document.as_mut().unwrap();
        assert_eq!(
            document.rename_header(1, "amount".into()).unwrap_err(),
            "Wait for the save to finish before editing headers."
        );
        assert_eq!(
            document.begin_cell_edit(1, 1, b"1".to_vec()).unwrap_err(),
            "Wait for the active file operation before editing data cells."
        );
        document.discard_header_edits();
        assert!(document.is_dirty());
        assert_eq!(
            app.open_path(other.clone()).unwrap_err(),
            "Cancel the active save before opening another file."
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        while !app
            .document
            .as_ref()
            .unwrap()
            .save_job
            .as_ref()
            .unwrap()
            .progress()
            .done
        {
            assert!(Instant::now() < deadline, "Save As timed out");
            std::thread::yield_now();
        }
        fs::remove_file(&unopenable).unwrap();
        let mut frame = eframe::Frame::_new_kittest();
        let _ = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        assert!(app.close_confirmation_open);
        assert!(!app.close_after_save);
        let document = app.document.as_mut().unwrap();
        assert_eq!(document.session.path(), source);
        assert!(document.is_dirty());
        assert_eq!(
            document.cell_edits.get(&(1, 1)).map(Vec::as_slice),
            Some(b"edited".as_slice())
        );
        assert!(document.save_status.is_none());

        app.close_confirmation_open = false;
        app.document
            .as_mut()
            .unwrap()
            .start_save_as(destination.clone())
            .unwrap();
        app.close_after_save = true;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !app
            .document
            .as_ref()
            .unwrap()
            .save_job
            .as_ref()
            .unwrap()
            .progress()
            .done
        {
            assert!(Instant::now() < deadline, "Save As timed out");
            std::thread::yield_now();
        }
        let saved_output = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        assert!(
            saved_output
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .unwrap()
                .commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::Close))
        );
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.session.path(), destination);
        assert_eq!(document.column_name(0), "renamed");
        assert!(!document.is_dirty());
        assert_eq!(app.delimiter_mode, DelimiterMode::Auto);
        assert_eq!(app.header_mode, HeaderMode::FirstRow);
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"renamed,value\nfirst,edited\n"
        );
        assert_eq!(
            save_as_file_name(Path::new("report.csv")),
            "report-edited.csv"
        );

        app.document.as_mut().unwrap().shutdown();
        for path in [source, other, destination] {
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn discard_and_close_drops_a_structural_working_copy_before_closing() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.csv");
        let working = directory.path().join("working.csv");
        fs::write(&source, b"name\noriginal\n").unwrap();
        fs::write(&working, b"name\nchanged\n").unwrap();

        let mut app = QuarryApp::new(Some(working), Instant::now());
        let working_copy = WorkingCopyState::new().unwrap();
        let working_directory = working_copy.directory.path().to_path_buf();
        let document = app.document.as_mut().unwrap();
        document.logical_path = source.clone();
        document.working_copy = Some(working_copy);
        app.close_confirmation_open = true;

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut frame = eframe::Frame::_new_kittest();
        let output = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        let discard = output
            .platform_output
            .accesskit_update
            .unwrap()
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::Button
                    && node.label() == Some("Discard Changes and Close")
            })
            .map(|(id, _)| *id)
            .unwrap();
        let output = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: discard,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| eframe::App::update(&mut app, ctx, &mut frame),
        );

        assert!(app.document.is_none());
        assert!(!working_directory.exists());
        assert_eq!(fs::read(&source).unwrap(), b"name\noriginal\n");
        assert!(
            output
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .unwrap()
                .commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::Close))
        );
    }

    #[test]
    fn filtered_export_reports_success_and_never_overwrites() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-export-success-{name}.csv"));
        let destination =
            std::env::temp_dir().join(format!("quarry-export-success-{name}-filtered.csv"));
        let source = b"id,note,status\n1,\"line one\nline two\",keep\n2,skip,skip\n3,\"quote \"\"yes\"\"\",keep\n";
        let expected =
            b"id,note,status\n1,\"line one\nline two\",keep\n3,\"quote \"\"yes\"\"\",keep\n";
        fs::write(&path, source).unwrap();
        let mut document = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let query = FilterQuery::single(2, FilterOperator::Equals, b"keep".to_vec());
        document.start_filter(query.clone()).unwrap();
        finish_filter(&mut document);

        document.start_filtered_export(destination.clone()).unwrap();
        assert_eq!(
            document.rename_header(0, "renamed".into()).unwrap_err(),
            "Wait for the filtered export to finish before editing headers."
        );
        assert_eq!(
            document.clear_filter().unwrap_err(),
            "Cancel the active export before clearing filters."
        );
        assert_eq!(
            document.start_filter(query.clone()).unwrap_err(),
            "Cancel the active export before changing filters."
        );
        finish_filtered_export(&mut document);

        assert_eq!(fs::read(&destination).unwrap(), expected);
        let status = document.export_status.as_deref().unwrap();
        assert!(status.contains("2 rows"));
        assert!(status.contains(&destination.display().to_string()));
        assert_eq!(
            document
                .start_filtered_export(destination.clone())
                .unwrap_err(),
            "export destination already exists"
        );
        assert_eq!(fs::read(&destination).unwrap(), expected);
        assert_eq!(
            document.start_filtered_export(path.clone()).unwrap_err(),
            "export destination must differ from the source file"
        );
        assert_eq!(fs::read(&path).unwrap(), source);

        document.start_filter(query).unwrap();
        assert!(document.export_progress.is_none());
        assert!(document.export_status.is_none());
        finish_filter(&mut document);
        document.shutdown();
        for path in [path, destination] {
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn filtered_export_cancel_reopen_and_picker_cancel_are_safe() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let first = std::env::temp_dir().join(format!("quarry-export-life-{name}-first.csv"));
        let second = std::env::temp_dir().join(format!("quarry-export-life-{name}-second.csv"));
        let cancelled =
            std::env::temp_dir().join(format!("quarry-export-life-{name}-cancelled.csv"));
        let reopened = std::env::temp_dir().join(format!("quarry-export-life-{name}-reopened.csv"));
        let mut file = File::create(&first).unwrap();
        file.write_all(b"name,status\n").unwrap();
        file.write_all(&b"row,keep\n".repeat(1_000_000)).unwrap();
        drop(file);
        fs::write(&second, b"name,status\nsecond,keep\n").unwrap();

        let mut app = QuarryApp::new(None, Instant::now());
        app.header_mode = HeaderMode::FirstRow;
        app.open_path(first.clone()).unwrap();
        let document = app.document.as_mut().unwrap();
        let file_size = document.session.file_size;
        document.filter_query = Some(FilterQuery::single(
            1,
            FilterOperator::Equals,
            b"keep".to_vec(),
        ));
        document.filter_progress = Some(FilterProgress {
            bytes_scanned: file_size,
            rows_scanned: 1_000_001,
            matches_found: 1_000_000,
            file_size,
            elapsed: Duration::from_millis(1),
            done: true,
            cancelled: false,
        });

        app.notice = Some(AppMessage::warning("unchanged"));
        app.export_picker_result(None);
        assert_eq!(app.notice.as_deref(), Some("unchanged"));
        app.export_picker_result(Some(first.clone()));
        assert_eq!(
            app.notice.as_deref(),
            Some("export destination must differ from the source file")
        );
        app.export_picker_result(Some(cancelled.clone()));
        let document = app.document.as_mut().unwrap();
        document.cancel_filtered_export();
        assert!(document.export_cancel_requested);
        assert_eq!(
            document.export_status.as_deref(),
            Some("Cancelling export… Wait for it to finish before opening or reapplying the file.")
        );
        finish_filtered_export(document);
        assert_eq!(
            document.export_status.as_deref(),
            Some("Export cancelled. No output file was created.")
        );
        assert!(!cancelled.exists());

        document.start_filtered_export(reopened.clone()).unwrap();
        let error = app.open_path(second.clone()).unwrap_err();
        assert_eq!(
            error,
            "Cancel the active export and wait for it to finish before opening another file."
        );
        let document = app.document.as_mut().unwrap();
        assert_eq!(document.session.path(), first);
        assert!(document.export_job.is_some());

        document.cancel_filtered_export();
        finish_filtered_export(document);
        app.open_path(second.clone()).unwrap();
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.session.path(), second);
        assert!(document.export_job.is_none());
        assert!(document.export_status.is_none());
        assert_eq!(
            filtered_export_file_name(Path::new("report.csv")),
            "report-filtered.csv"
        );
        assert_eq!(
            filtered_export_file_name(Path::new("report")),
            "report-filtered"
        );

        app.document.as_mut().unwrap().shutdown();
        for path in [first, second, reopened] {
            if path.exists() {
                fs::remove_file(path).unwrap();
            }
        }
    }

    #[test]
    fn filtered_grid_is_match_only_bounded_and_keeps_source_column_identity() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-filter-grid-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        writeln!(file, "id,status,note").unwrap();
        for row in 1..=200 {
            let status = if row % 3 == 0 { "keep" } else { "skip" };
            writeln!(file, "{row},{status},note{row}").unwrap();
        }
        drop(file);

        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document.move_column(1, 0).unwrap();
        document.set_column_shown(1, false).unwrap();
        let order = document.columns.order.clone();
        let hidden = document.columns.hidden.clone();

        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"keep".to_vec(),
            ))
            .unwrap();
        finish_filter(&mut document);
        document.set_visible_rows(7).unwrap();

        assert_eq!(document.available_filter_rows(), 66);
        assert_eq!(document.filter_viewport_start, 0);
        assert_eq!(document.visible_row_count(), 7);
        assert!(
            document
                .visible_filter_rows()
                .iter()
                .all(|row| { row.fields.get(1).map(Vec::as_slice) == Some(b"keep".as_slice()) })
        );
        assert_eq!(document.visible_filter_rows()[0].match_ordinal, 0);
        assert_eq!(document.visible_filter_rows()[0].row, 3);
        assert_eq!(document.columns.order, order);
        assert_eq!(document.columns.hidden, hidden);

        let first_row = document.visible_filter_rows()[0].row;
        document.selection = Some(GridSelection::Cell {
            row: first_row,
            column: 2,
        });
        assert_eq!(document.copy_selection_text().unwrap(), "note3");
        document.selection = Some(GridSelection::Row { row: first_row });
        assert_eq!(document.copy_selection_text().unwrap(), "3\tkeep\tnote3");

        document.page(1).unwrap();
        assert_eq!(document.filter_viewport_start, 7);
        assert_eq!(document.visible_filter_rows()[0].match_ordinal, 7);
        assert_eq!(document.visible_filter_rows()[0].row, 24);
        assert!(document.filtered_rows.len() <= document.visible_rows + 2 * super::OVERSCAN_ROWS);

        let final_start =
            max_viewport_start(document.available_filter_rows(), document.visible_rows);
        document.navigate_filter(final_start).unwrap();
        assert!(document.filter_rows_loading());
        finish_filter_read(&mut document);
        assert_eq!(document.visible_filter_rows().last().unwrap().row, 198);
        assert!(document.filtered_rows.len() <= document.visible_rows + 2 * super::OVERSCAN_ROWS);
        document.page(-1).unwrap();
        assert_eq!(
            document.filter_viewport_start,
            final_start.saturating_sub(document.visible_rows as u64)
        );

        document.clear_filter().unwrap();
        assert!(!document.filter_active());
        assert!(document.filtered_rows.is_empty());
        assert_eq!(document.columns.order, order);
        assert_eq!(document.columns.hidden, hidden);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn active_filter_navigation_refreshes_a_stale_snapshot_before_reading() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-filter-stale-{name}.csv"));
        let prefix_path =
            std::env::temp_dir().join(format!("quarry-filter-stale-prefix-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        file.write_all(b"name,status\n").unwrap();
        file.write_all(&b"row,keep\n".repeat(2_000)).unwrap();
        drop(file);
        let mut prefix = File::create(&prefix_path).unwrap();
        prefix.write_all(b"name,status\n").unwrap();
        prefix.write_all(&b"row,keep\n".repeat(1_000)).unwrap();
        drop(prefix);

        let mut document = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        document.visible_rows = 5;
        let query = FilterQuery::single(1, FilterOperator::Equals, b"keep".to_vec());
        let stale_session = Session::open(
            &prefix_path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let stale_job = stale_session.start_filter(query.clone()).unwrap();
        let stale_index = stale_job.wait().unwrap();
        let stale_matches = stale_index.matches_found();
        assert_eq!(stale_matches, 1_000);

        document.start_filter(query).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let progress = loop {
            let progress = document.filter_job.as_ref().unwrap().progress();
            if progress.done {
                break progress;
            }
            assert!(Instant::now() < deadline, "filter did not complete");
            std::thread::yield_now();
        };
        assert_eq!(progress.matches_found, 2_000);
        document.filter_progress = Some(progress);
        document.filter_index = Some(stale_index);
        let target = max_viewport_start(progress.matches_found, document.visible_rows);
        let required = target
            .saturating_add(document.visible_rows as u64)
            .min(progress.matches_found);
        assert!(stale_matches < required);

        document.navigate_filter(target).unwrap();
        assert!(document.filter_rows_loading());
        finish_filter_read(&mut document);

        assert!(document.filter_index.as_ref().unwrap().matches_found() >= required);
        assert_eq!(document.visible_filter_rows()[0].match_ordinal, target);
        assert!(document.filtered_rows.len() <= document.visible_rows + 2 * super::OVERSCAN_ROWS);
        document.shutdown();
        for path in [path, prefix_path] {
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn filter_navigation_is_async_and_latest_request_wins() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-filter-latest-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        writeln!(file, "id,status").unwrap();
        for row in 0..25_000 {
            let status = if row % 20 == 0 { "keep" } else { "skip" };
            writeln!(file, "{row},{status}").unwrap();
        }
        drop(file);

        let mut document = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        document.visible_rows = 5;
        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"keep".to_vec(),
            ))
            .unwrap();
        finish_filter(&mut document);

        document.navigate_filter(100).unwrap();
        assert!(document.filter_read.is_some());
        assert!(document.visible_filter_rows().is_empty());
        assert!(document.filter_rows_loading());
        assert_eq!(footer_range_text(&document), "Loading matching rows…");

        document.navigate_filter(500).unwrap();
        assert!(document.filter_read.as_ref().unwrap().cancel_requested);
        document.navigate_filter(900).unwrap();
        assert_eq!(
            document.pending_filter_read,
            document.filter_read_window(900)
        );
        assert_eq!(document.filter_viewport_start, 900);

        finish_filter_read(&mut document);
        assert_eq!(document.visible_filter_rows()[0].match_ordinal, 900);
        assert_eq!(document.visible_filter_rows()[0].fields[0], b"18000");
        assert!(
            document
                .visible_filter_rows()
                .iter()
                .all(|row| row.fields[1] == b"keep")
        );
        assert!(document.filtered_rows.len() <= document.visible_rows + 2 * super::OVERSCAN_ROWS);
        assert!(document.filter_read.is_none());
        assert!(document.pending_filter_read.is_none());

        document.shutdown();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn filter_cancel_clear_and_reopen_reset_the_filter_lifecycle() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let first = std::env::temp_dir().join(format!("quarry-filter-life-{name}-first.csv"));
        let second = std::env::temp_dir().join(format!("quarry-filter-life-{name}-second.csv"));
        let mut file = File::create(&first).unwrap();
        writeln!(file, "name,status").unwrap();
        for row in 0..50_000 {
            writeln!(file, "row{row},keep").unwrap();
        }
        drop(file);
        fs::write(&second, b"name,status\nsecond,keep\n").unwrap();

        let mut app = QuarryApp::new(None, Instant::now());
        app.header_mode = HeaderMode::FirstRow;
        app.open_path(first.clone()).unwrap();
        let document = app.document.as_mut().unwrap();
        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Contains,
                b"keep".to_vec(),
            ))
            .unwrap();
        document.cancel_filter();
        finish_filter(document);
        assert!(document.filter_active());
        let status = document.filter_status.as_deref().unwrap();
        assert!(
            status.starts_with("Filter cancelled after") || status.starts_with("Filter complete.")
        );

        document.clear_filter().unwrap();
        assert!(!document.filter_active());
        assert!(document.filter_job.is_none());
        assert!(document.filter_index.is_none());

        let query = FilterQuery::single(1, FilterOperator::Equals, b"keep".to_vec());
        document.start_filter(query.clone()).unwrap();
        finish_filter(document);
        document.navigate_filter(100).unwrap();
        document.navigate_filter(200).unwrap();
        assert!(document.filter_read.is_some());
        assert!(document.pending_filter_read.is_some());

        document.start_filter(query.clone()).unwrap();
        assert!(document.filter_read.is_none());
        assert!(document.pending_filter_read.is_none());
        finish_filter(document);
        document.navigate_filter(100).unwrap();
        document.navigate_filter(200).unwrap();
        assert!(document.filter_read.is_some());
        assert!(document.pending_filter_read.is_some());
        document.clear_filter().unwrap();
        assert!(document.filter_read.is_none());
        assert!(document.pending_filter_read.is_none());

        document.start_filter(query).unwrap();
        finish_filter(document);
        document.navigate_filter(100).unwrap();
        document.navigate_filter(200).unwrap();
        assert!(document.filter_read.is_some());
        assert!(document.pending_filter_read.is_some());
        app.filters_open = true;
        app.filter_rules[0].column_input = "2".into();
        app.filter_rules[0].value_input = "keep".into();
        app.filter_rules.push(super::FilterRuleDraft::default());
        app.open_path(second.clone()).unwrap();
        assert!(!app.filters_open);
        assert_eq!(app.filter_rules, vec![super::FilterRuleDraft::default()]);
        assert!(!app.document.as_ref().unwrap().filter_active());
        assert!(app.document.as_ref().unwrap().filter_read.is_none());
        assert!(app.document.as_ref().unwrap().pending_filter_read.is_none());

        app.document.as_mut().unwrap().shutdown();
        for path in [first, second] {
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn very_wide_column_ranges_stay_bounded_and_reach_the_last_column() {
        let column_width = 80.0;
        let column_spacing = 4.0;
        let total_columns = 65_536;
        let mut column_offsets = (0..=total_columns)
            .map(|column| column as f32 * (column_width + column_spacing))
            .collect::<Vec<_>>();
        let viewport = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1_200.0, 800.0));
        assert_eq!(
            rendered_column_range(viewport, &column_offsets, column_spacing, None),
            0..MAX_RENDERED_COLUMNS
        );

        let content_width =
            ROW_NUMBER_WIDTH + total_columns as f32 * (column_width + column_spacing);
        let last_viewport = viewport.translate(egui::vec2(content_width - viewport.width(), 0.0));
        let last = rendered_column_range(last_viewport, &column_offsets, column_spacing, None);
        assert_eq!(last.len(), MAX_RENDERED_COLUMNS);
        assert_eq!(last.end, total_columns);

        let focused =
            rendered_column_range(viewport, &column_offsets, column_spacing, Some(40_000));
        assert!(focused.contains(&40_000));
        assert_eq!(
            rendered_column_range(viewport, &column_offsets[..=42], column_spacing, None),
            0..42
        );

        // A wide first column must not make scrolling skip narrow columns after it.
        for offset in &mut column_offsets[1..] {
            *offset += 4_000.0;
        }
        let shifted = viewport.translate(egui::vec2(4_500.0, 0.0));
        assert_eq!(
            rendered_column_range(shifted, &column_offsets, column_spacing, None).start,
            4
        );
    }

    #[test]
    fn auto_fit_expands_wide_files_and_keeps_widths_with_source_columns() {
        let directory = tempfile::tempdir().unwrap();
        for total_columns in [64, 65, 100, 664] {
            let path = directory.path().join(format!("wide-{total_columns}.csv"));
            let mut headers = vec!["short"; total_columns];
            headers[total_columns - 1] = "A long header in the last file column";
            let mut values = vec!["x"; total_columns];
            values[0] = "A long cell value that needs more than eighty pixels";
            fs::write(
                &path,
                format!("{}\n{}\n", headers.join(","), values.join(",")),
            )
            .unwrap();
            let mut app = QuarryApp::new(None, Instant::now());
            app.document = Some(
                Document::prepare(
                    &path,
                    OpenOptions {
                        header_mode: HeaderMode::FirstRow,
                        ..OpenOptions::default()
                    },
                )
                .unwrap(),
            );
            let ctx = egui::Context::default();
            configure_style(&ctx);
            ctx.enable_accesskit();
            let header_width = |output: &egui::FullOutput, column: usize| {
                let prefix = format!("Select file column {} (", column + 1);
                output
                    .platform_output
                    .accesskit_update
                    .as_ref()
                    .unwrap()
                    .nodes
                    .iter()
                    .find(|(_, node)| node.label().is_some_and(|label| label.starts_with(&prefix)))
                    .unwrap()
                    .1
                    .bounds()
                    .unwrap()
                    .width()
            };
            if total_columns == 65 {
                let document = app.document.as_mut().unwrap();
                document.set_column_shown(64, false).unwrap();
                let _ = render_grid(&ctx, document);
                document.set_column_shown(64, true).unwrap();
            }
            let before = render_grid(&ctx, app.document.as_mut().unwrap());
            let initial_width = header_width(&before, 0);
            let command = click_column_manager_control(
                egui::accesskit::Role::Button,
                "Auto-fit columns",
                app.document.as_ref().unwrap(),
            );
            app.apply_column_command(&ctx, command);
            let document = app.document.as_mut().unwrap();
            for _ in 0..total_columns.div_ceil(MAX_RENDERED_COLUMNS) + 2 {
                let _ = render_grid(&ctx, document);
            }
            assert!(document.columns_to_fit.is_empty());
            let after = render_grid(&ctx, document);
            assert!(header_width(&after, 0) > initial_width + 100.0);
            if total_columns <= MAX_RENDERED_COLUMNS {
                continue;
            }
            assert_eq!(document.fitted_column_widths.len(), total_columns);
            let last_width = document.fitted_column_widths[&(total_columns - 1)];
            assert!(last_width > 200.0);
            document.reveal_cell = Some((1, total_columns - 1));
            let _ = render_grid(&ctx, document);
            let last = render_grid(&ctx, document);
            assert!(header_width(&last, total_columns - 1) >= f64::from(last_width) - 1.0);
            let rendered_headers = last
                .platform_output
                .accesskit_update
                .unwrap()
                .nodes
                .iter()
                .filter(|(_, node)| {
                    node.label()
                        .is_some_and(|label| label.starts_with("Select file column "))
                })
                .count();
            assert!(rendered_headers <= MAX_RENDERED_COLUMNS);

            document.move_column(total_columns - 1, 0).unwrap();
            document.set_column_shown(0, false).unwrap();
            document.reveal_cell = Some((1, total_columns - 1));
            let _ = render_grid(&ctx, document);
            let reordered = render_grid(&ctx, document);
            assert!(header_width(&reordered, total_columns - 1) >= f64::from(last_width) - 1.0);
            assert!(document.cell_edits.is_empty());
            assert!(document.header_renames.is_empty());
            if total_columns == 65 {
                document
                    .rename_header(total_columns - 1, "short".into())
                    .unwrap();
                document.auto_fit_columns = true;
                let _ = render_grid(&ctx, document);
                let refitted = render_grid(&ctx, document);
                assert!(header_width(&refitted, total_columns - 1) < f64::from(last_width) - 100.0);
                assert_eq!(document.fitted_column_widths[&(total_columns - 1)], 80.0);
                document.set_column_shown(0, true).unwrap();
                document.reveal_cell = Some((1, total_columns - 1));
                let _ = render_grid(&ctx, document);
                let restored = render_grid(&ctx, document);
                assert!(header_width(&restored, total_columns - 1) < f64::from(last_width) - 100.0);
            }
        }
    }

    #[test]
    fn revealing_an_oversized_fitted_column_keeps_its_start_visible() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oversized-column.csv");
        let headers = vec!["column"; 65];
        let mut values = vec!["x".to_owned(); 65];
        values[64] = "wide value ".repeat(10);
        fs::write(
            &path,
            format!("{}\n{}\n", headers.join(","), values.join(",")),
        )
        .unwrap();
        let mut document = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let ctx = egui::Context::default();
        configure_style(&ctx);
        ctx.enable_accesskit();
        let render = |document: &mut Document| {
            ctx.run(grid_input_with_width(600.0), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid(ui, document).unwrap();
                });
            })
        };
        document.auto_fit_columns = true;
        for _ in 0..3 {
            let _ = render(&mut document);
        }
        document.view_column(64).unwrap();
        for _ in 0..3 {
            let output = render(&mut document);
            let tree = output.platform_output.accesskit_update.unwrap();
            let bounds = tree
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some("Select file column 65 (column)"))
                .unwrap()
                .1
                .bounds()
                .unwrap();
            assert!(bounds.width() > 600.0);
            assert!(
                bounds.x0 >= 0.0 && bounds.x0 < 100.0,
                "column start is off-screen: {bounds:?}"
            );
        }
    }

    #[test]
    fn very_wide_grid_renders_a_bounded_last_column_window() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-very-wide-{name}.csv"));
        let total_columns = 65_536;
        let headers = (1..=total_columns)
            .map(|column| format!("c{column}"))
            .collect::<Vec<_>>()
            .join(",");
        let values = (1..=total_columns)
            .map(|column| format!("v{column}"))
            .collect::<Vec<_>>()
            .join(",");
        fs::write(&path, format!("{headers}\n{values}\n")).unwrap();

        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document.reveal_cell = Some((1, total_columns - 1));

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let output = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show_grid(ui, &mut document).unwrap();
            });
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present");
        let numbered_headers = tree
            .nodes
            .iter()
            .filter(|(_, node)| {
                node.label()
                    .is_some_and(|label| label.starts_with("Select file column "))
            })
            .count();
        assert!(numbered_headers <= MAX_RENDERED_COLUMNS);
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.label()
                .is_some_and(|label| label.starts_with("Select file column 65536 (c65536)"))
        }));
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.label().is_some_and(|label| {
                label.starts_with("Select row 1, column 65536 (c65536): v65536")
            })
        }));
        assert_eq!(document.columns.visible.len(), total_columns);
        assert!(document.reveal_cell.is_none());

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn column_view_hides_reorders_resets_and_keeps_every_shown_column() {
        let mut view = ColumnView::new(40);
        assert_eq!(view.visible, (0..40).collect::<Vec<_>>());

        assert!(view.set_shown(5, false));
        assert!(!view.visible.contains(&5));
        assert_eq!(view.shown_count(), 39);
        assert!(view.move_column(39, 0));
        assert_eq!(&view.order[..3], &[39, 0, 1]);
        assert!(view.move_column(39, 39));
        assert_eq!(view.order, (0..40).collect::<Vec<_>>());
        assert!(view.move_column(0, 39));
        assert_eq!(&view.order[37..], &[38, 39, 0]);
        assert!(view.move_column(0, 0));
        assert_eq!(view.order, (0..40).collect::<Vec<_>>());
        assert!(view.move_column(39, 39));
        assert!(!view.move_column(40, 0));
        assert!(!view.move_column(39, 40));
        assert!(view.move_column(39, 0));

        assert_eq!(column_drop_position(0, 3, 4), 2);
        assert_eq!(column_drop_position(3, 1, 4), 1);
        assert_eq!(column_drop_position(2, 2, 4), 2);
        assert_eq!(column_drop_position(0, 4, 4), 3);

        assert!(view.view(39));
        assert!(view.visible.contains(&39));
        assert_eq!(view.visible.len(), 39);

        for column in 0..40 {
            assert!(view.set_shown(column, false));
        }
        assert!(view.visible.is_empty());
        assert_eq!(view.start, 0);

        view.extend_to(42);
        assert_eq!(&view.order[..3], &[39, 0, 1]);
        assert_eq!(&view.order[39..], &[38, 40, 41]);
        assert_eq!(view.visible, [40, 41]);

        view.reset();
        assert_eq!(view.order, (0..42).collect::<Vec<_>>());
        assert!(view.hidden.iter().all(|hidden| !hidden));
        assert_eq!(view.visible, (0..42).collect::<Vec<_>>());
    }

    #[test]
    fn column_manager_exposes_clear_list_controls() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-column-a11y-{name}.csv"));
        let headers = (1..=40)
            .map(|column| format!("c{column}"))
            .collect::<Vec<_>>()
            .join(",");
        fs::write(&path, format!("{headers}\n{}\n", vec!["x"; 40].join(","))).unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);

        let mut app = QuarryApp::new(None, Instant::now());
        app.document = Some(document);
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        configure_style(&ctx);
        let mut open = true;
        let mut search = String::new();
        let mut command = None;
        let output = ctx.run(grid_input(), |ctx| {
            command =
                show_column_manager(ctx, &mut open, &mut search, app.document.as_ref().unwrap());
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present");
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::TextInput
                && node.label() == Some("Search columns")
        }));
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::CheckBox
                && node.label().is_some_and(|label| label.contains("c1"))
        }));
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.label() == Some("Select and drag column 1 to reorder") && !node.is_hidden()
        }));
        let drag_bounds = [1, 2].map(|column| {
            let label = format!("Select and drag column {column} to reorder");
            tree.nodes
                .iter()
                .find(|(_, node)| node.label() == Some(label.as_str()))
                .and_then(|(_, node)| node.bounds())
                .unwrap_or_else(|| panic!("{label} should have bounds"))
        });
        let row_height = drag_bounds[0].y1 - drag_bounds[0].y0;
        let row_stride = drag_bounds[1].y0 - drag_bounds[0].y0;
        assert!((row_height - 36.0).abs() < f64::EPSILON);
        assert!(
            (row_stride - (36.0 + f64::from(ctx.style().spacing.item_spacing.y))).abs()
                < f64::EPSILON
        );

        ctx.data_mut(|data| {
            data.insert_persisted(egui::Id::new("quarry-selected-managed-column"), 1_usize);
        });
        app.apply(&ctx, Action::OpenColumns);
        assert!(ctx.data_mut(|data| {
            data.get_persisted::<usize>(egui::Id::new("quarry-selected-managed-column"))
                .is_none()
        }));

        search = "c40".into();
        let filtered = ctx.run(grid_input(), |ctx| {
            show_column_manager(ctx, &mut open, &mut search, app.document.as_ref().unwrap());
        });
        let filtered_tree = filtered.platform_output.accesskit_update.unwrap();
        assert!(filtered_tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::CheckBox
                && node.label().is_some_and(|label| label.contains("c40"))
        }));
        assert!(!filtered_tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::CheckBox
                && node.label().is_some_and(|label| label.contains("c1"))
        }));

        let command = click_column_manager_control(
            egui::accesskit::Role::CheckBox,
            "1  c1",
            app.document.as_ref().unwrap(),
        );
        app.apply_column_command(&ctx, command);
        assert!(app.document.as_ref().unwrap().columns.hidden[0]);

        let command = click_column_manager_control(
            egui::accesskit::Role::Button,
            "Reset columns",
            app.document.as_ref().unwrap(),
        );
        app.apply_column_command(&ctx, command);
        assert_eq!(
            app.document.as_ref().unwrap().columns.order,
            (0..40).collect::<Vec<_>>()
        );
        assert!(
            app.document
                .as_ref()
                .unwrap()
                .columns
                .hidden
                .iter()
                .all(|hidden| !hidden)
        );
        let command = click_column_manager_control(
            egui::accesskit::Role::Button,
            "Auto-fit columns",
            app.document.as_ref().unwrap(),
        );
        app.apply_column_command(&ctx, command);
        assert!(app.document.as_ref().unwrap().auto_fit_columns);

        app.apply_column_command(
            &ctx,
            ColumnCommand::Move {
                column: 39,
                position: 0,
            },
        );
        assert_eq!(
            &app.document.as_ref().unwrap().columns.order[..3],
            &[39, 0, 1]
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn column_selection_modifiers_match_editor_conventions() {
        let columns = ColumnView::new(6);
        let mut selected = BTreeSet::new();
        let mut anchor = None;

        select_column(
            &columns,
            &mut selected,
            &mut anchor,
            2,
            egui::Modifiers::NONE,
        );
        assert_eq!(selected, BTreeSet::from([2]));
        assert_eq!(anchor, Some(2));

        select_column(
            &columns,
            &mut selected,
            &mut anchor,
            5,
            egui::Modifiers::SHIFT,
        );
        assert_eq!(selected, BTreeSet::from([2, 3, 4, 5]));
        assert_eq!(anchor, Some(2));

        select_column(
            &columns,
            &mut selected,
            &mut anchor,
            1,
            egui::Modifiers::NONE,
        );
        select_column(
            &columns,
            &mut selected,
            &mut anchor,
            4,
            egui::Modifiers::COMMAND,
        );
        assert_eq!(selected, BTreeSet::from([1, 4]));
        assert_eq!(anchor, Some(4));

        select_column(
            &columns,
            &mut selected,
            &mut anchor,
            1,
            egui::Modifiers::COMMAND,
        );
        assert_eq!(selected, BTreeSet::from([4]));
        select_column(
            &columns,
            &mut selected,
            &mut anchor,
            4,
            egui::Modifiers::COMMAND,
        );
        assert!(selected.is_empty());
        assert_eq!(anchor, None);
    }

    #[test]
    fn row_selection_modifiers_are_compact_and_match_editor_conventions() {
        let mut selected = RowSelection::default();

        selected.select(2, egui::Modifiers::NONE);
        assert_eq!(selected.ranges, vec![2..=2]);
        assert_eq!(selected.anchor, Some(2));

        selected.select(100_000_000, egui::Modifiers::SHIFT);
        assert_eq!(selected.ranges, vec![2..=100_000_000]);
        assert_eq!(selected.count(), 99_999_999);
        assert_eq!(selected.ranges.len(), 1);
        assert_eq!(selected.anchor, Some(2));

        selected.select(10, egui::Modifiers::NONE);
        selected.select(20, egui::Modifiers::COMMAND);
        assert_eq!(selected.ranges, vec![10..=10, 20..=20]);
        assert_eq!(selected.anchor, Some(20));

        selected.select(10, egui::Modifiers::COMMAND);
        assert_eq!(selected.ranges, vec![20..=20]);
        selected.select(20, egui::Modifiers::COMMAND);
        assert!(selected.is_empty());
        assert_eq!(selected.anchor, None);
    }

    #[test]
    fn filtering_clears_row_selection_and_blocks_row_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("filtered-row-deletion.csv");
        fs::write(&source, b"name,city\nAda,London\nGrace,Arlington\n").unwrap();
        let mut document = Document::open(
            &source,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document.selected_rows.select(1, egui::Modifiers::NONE);
        document.selection = Some(GridSelection::Row { row: 1 });

        document
            .start_filter(FilterQuery::single(
                0,
                FilterOperator::Equals,
                b"Ada".to_vec(),
            ))
            .unwrap();
        assert!(document.selected_rows.is_empty());
        assert!(document.selection.is_none());
        assert_eq!(
            document.start_delete_rows(vec![1..=1]).unwrap_err(),
            "Clear the filter before editing the document."
        );
        assert!(document.structural_job.is_none());
        assert!(document.working_copy.is_none());
    }

    #[test]
    fn shift_selection_uses_shown_display_order_and_hiding_prunes_it() {
        let mut columns = ColumnView::new(6);
        assert!(columns.move_column(5, 1));
        assert!(columns.set_shown(1, false));
        let mut selected = BTreeSet::new();
        let mut anchor = None;

        select_column(
            &columns,
            &mut selected,
            &mut anchor,
            5,
            egui::Modifiers::NONE,
        );
        select_column(
            &columns,
            &mut selected,
            &mut anchor,
            3,
            egui::Modifiers::SHIFT,
        );
        assert_eq!(selected.iter().copied().collect::<Vec<_>>(), vec![2, 3, 5]);
        assert!(!selected.contains(&1));

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("hidden-column-selection.csv");
        fs::write(&source, b"a,b,c,d,e,f\n1,2,3,4,5,6\n").unwrap();
        let mut document = Document::prepare(
            &source,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        document.move_column(5, 1).unwrap();
        document.set_column_shown(1, false).unwrap();
        document.selected_columns = selected;
        document.column_selection_anchor = anchor;

        document.set_column_shown(5, false).unwrap();
        assert_eq!(document.selected_columns, BTreeSet::from([2, 3]));
        assert_eq!(document.column_selection_anchor, Some(2));
        document.set_column_shown(2, false).unwrap();
        assert_eq!(document.selected_columns, BTreeSet::from([3]));
        assert_eq!(document.column_selection_anchor, Some(3));
        document.set_column_shown(3, false).unwrap();
        assert!(document.selected_columns.is_empty());
        assert_eq!(document.column_selection_anchor, None);
    }

    #[test]
    fn numbered_columns_are_visibly_and_accessibly_multi_selectable() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("column-selection.csv");
        fs::write(
            &source,
            b"first,middle,last\nAda,King,Lovelace\nGrace,Brewster,Hopper\n",
        )
        .unwrap();
        let mut document = Document::prepare(
            &source,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();

        click_grid_control("Select file column 1 (first)", &mut document);
        click_grid_control("Select file column 3 (last)", &mut document);
        assert_eq!(document.selected_columns, BTreeSet::from([0, 2]));

        let ctx = egui::Context::default();
        configure_style(&ctx);
        ctx.enable_accesskit();
        let output = render_grid(&ctx, &mut document);
        let tree = output
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("the selected columns should be accessible");
        for label in [
            "Select file column 1 (first)",
            "Select file column 3 (last)",
        ] {
            let node = tree
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some(label))
                .map(|(_, node)| node)
                .unwrap_or_else(|| panic!("missing accessible numbered header {label}"));
            assert_eq!(node.is_selected(), Some(true));
        }
        let middle = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Select file column 2 (middle)"))
            .map(|(_, node)| node)
            .expect("the unselected numbered header should be accessible");
        assert_eq!(middle.is_selected(), Some(false));

        let tint = column_selection_fill(&ctx.style().visuals);
        let painted_cells = output
            .shapes
            .iter()
            .filter(|shape| matches!(&shape.shape, egui::Shape::Rect(rect) if rect.fill == tint))
            .count();
        let expected_cells = (document.visible_row_count() + 1) * document.selected_columns.len();
        assert!(
            painted_cells >= expected_cells,
            "each selected header and visible column cell should be tinted"
        );

        assert_column_ruler_divider(&output, &ctx.style().visuals, document.headers.len());

        let sizing_ctx = egui::Context::default();
        configure_style(&sizing_ctx);
        let render_at_width = |width, document: &mut Document| {
            sizing_ctx.run(grid_input_with_width(width), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid(ui, document).unwrap();
                });
            })
        };
        let narrow = render_at_width(420.0, &mut document);
        let narrow_span = assert_column_ruler_divider(
            &narrow,
            &sizing_ctx.style().visuals,
            document.headers.len(),
        );
        let wide = render_at_width(1680.0, &mut document);
        let wide_span =
            assert_column_ruler_divider(&wide, &sizing_ctx.style().visuals, document.headers.len());
        assert!(
            wide_span > narrow_span,
            "the continuous divider should follow the resized table width"
        );

        let selected_target = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Select file column 3 (last)"))
            .map(|(id, _)| *id)
            .unwrap();
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::ShowContextMenu,
                        target: selected_target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid(ui, &mut document).unwrap();
                });
            },
        );
        assert_eq!(document.selected_columns, BTreeSet::from([0, 2]));
        let menu_output = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show_grid(ui, &mut document).unwrap();
            });
        });
        let menu_tree = menu_output
            .platform_output
            .accesskit_update
            .expect("the multi-column menu should be accessible");
        let combine = menu_tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Combine Columns…"))
            .map(|(_, node)| node)
            .expect("Combine Columns should be in the selected-column menu");
        assert!(!combine.is_disabled());

        let context = egui::Context::default();
        context.enable_accesskit();
        let unselected_target =
            grid_control_id(&context, "Select file column 2 (middle)", &mut document);
        let _ = context.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::ShowContextMenu,
                        target: unselected_target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid(ui, &mut document).unwrap();
                });
            },
        );
        assert_eq!(document.selected_columns, BTreeSet::from([1]));
    }

    #[test]
    fn combine_from_numbered_headers_becomes_the_normal_editable_grid() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("combine-from-grid.csv");
        fs::write(
            &source,
            b"first,last,age\nAda,Lovelace,36\nGrace,Hopper,85\n",
        )
        .unwrap();
        let original = fs::read(&source).unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.document.as_mut().unwrap());

        click_grid_control(
            "Select file column 1 (first)",
            app.document.as_mut().unwrap(),
        );
        click_grid_control(
            "Select file column 2 (last)",
            app.document.as_mut().unwrap(),
        );
        assert_eq!(
            app.document.as_ref().unwrap().selected_columns,
            BTreeSet::from([0, 1])
        );

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let header_target = grid_control_id(
            &ctx,
            "Select file column 2 (last)",
            app.document.as_mut().unwrap(),
        );
        let mut opened_dialog = None;
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::ShowContextMenu,
                        target: header_target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    opened_dialog = show_grid(ui, app.document.as_mut().unwrap()).unwrap();
                });
            },
        );
        let menu_output = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                opened_dialog = show_grid(ui, app.document.as_mut().unwrap()).unwrap();
            });
        });
        let menu_tree = menu_output
            .platform_output
            .accesskit_update
            .expect("the selected-column menu should be accessible");
        let combine_target = menu_tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Combine Columns…") && !node.is_disabled())
            .map(|(id, _)| *id)
            .expect("Combine Columns should be enabled for two selected headers");
        let sort_node = menu_tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Sort Rows…"))
            .map(|(_, node)| node)
            .expect("Sort Rows should be present");
        assert!(
            sort_node.is_disabled(),
            "Sort Rows should require exactly one selected column"
        );
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: combine_target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    opened_dialog = show_grid(ui, app.document.as_mut().unwrap()).unwrap();
                });
            },
        );
        let GridColumnRequest::Dialog(mut dialog) =
            opened_dialog.expect("Combine Columns should open its dialog")
        else {
            panic!("Combine Columns should request a dialog");
        };
        assert_eq!(dialog.columns, vec![0, 1]);
        dialog.separator = " ".into();
        app.open_structural_dialog(dialog);
        app.apply_structural_dialog_action(StructuralDialogAction::Apply);
        finish_structural_edit(&mut app);

        let document = app.document.as_ref().unwrap();
        assert_eq!(document.total_columns, 2);
        assert_eq!(
            document.session.first_rows[0].fields,
            vec![b"first".to_vec(), b"age".to_vec()]
        );
        assert_eq!(
            document.session.first_rows[1].fields,
            vec![b"Ada Lovelace".to_vec(), b"36".to_vec()]
        );
        assert_eq!(document.selected_columns, BTreeSet::from([0]));
        assert_eq!(document.column_selection_anchor, Some(0));
        assert!(document.cell_is_editable(Some(b"Ada Lovelace")));
        assert!(document.is_dirty());
        assert_eq!(fs::read(&source).unwrap(), original);

        let focus_ctx = egui::Context::default();
        focus_ctx.enable_accesskit();
        let output = render_grid(&focus_ctx, app.document.as_mut().unwrap());
        let tree = output
            .platform_output
            .accesskit_update
            .expect("the combined grid should be accessible");
        let focused = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Select file column 1 (first)"))
            .map(|(id, node)| (*id, node))
            .expect("the combined result header should be accessible");
        assert_eq!(focused.1.is_selected(), Some(true));
        assert_eq!(tree.focus, focused.0);
        assert!(
            tree.nodes
                .iter()
                .any(|(_, node)| node.label() == Some("Select file column 2 (age)"))
        );
    }

    #[test]
    fn move_and_delete_columns_use_the_editable_grid_and_structural_history() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("move-delete-columns.csv");
        fs::write(&source, b"a,b,c,d\nA1,B1,C1,D1\nA2,B2,C2,D2\n").unwrap();
        let original = fs::read(&source).unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.document.as_mut().unwrap());
        assert!(parse_move_position("0", 4, 2).is_err());
        assert!(parse_move_position("4", 4, 2).is_err());
        assert_eq!(
            app.document
                .as_mut()
                .unwrap()
                .start_delete_columns(vec![0, 1, 2, 3])
                .unwrap_err(),
            "At least one column must remain."
        );
        assert!(app.document.as_ref().unwrap().working_copy.is_none());

        let mut dialog = StructuralDialog::move_columns(vec![1, 3]);
        assert_eq!(dialog.position, "2");
        dialog.position = "1".into();
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let output = ctx.run(grid_input(), |ctx| {
            assert_eq!(
                show_structural_dialog(
                    ctx,
                    &mut dialog,
                    &mut app.sort_match_case,
                    app.document.as_ref().unwrap(),
                ),
                None
            );
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("the move dialog should be accessible");
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::TextInput
                && node.label() == Some("Destination position")
                && node.value() == Some("1")
        }));
        for label in ["Move", "Cancel"] {
            assert!(
                tree.nodes
                    .iter()
                    .any(|(_, node)| node.label() == Some(label)),
                "missing accessible move control {label}"
            );
        }

        app.open_structural_dialog(dialog);
        app.apply_structural_dialog_action(StructuralDialogAction::Apply);
        app.format_draft = Some((DelimiterMode::Tab, HeaderMode::NoHeader));
        finish_structural_edit(&mut app);
        assert_eq!(app.format_draft, None);
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.total_columns, 4);
        assert_eq!(
            document.session.first_rows[0].fields,
            ["b", "d", "a", "c"].map(|field| field.as_bytes().to_vec())
        );
        assert_eq!(
            document.session.first_rows[1].fields,
            ["B1", "D1", "A1", "C1"].map(|field| field.as_bytes().to_vec())
        );
        assert_eq!(document.selected_columns, BTreeSet::from([0, 1]));
        assert_eq!(document.column_selection_anchor, Some(0));
        assert!(document.is_dirty());
        assert_eq!(fs::read(&source).unwrap(), original);

        app.swap_structural_history(false).unwrap();
        finish_index(app.document.as_mut().unwrap());
        let redo_path = app
            .document
            .as_ref()
            .unwrap()
            .working_copy
            .as_ref()
            .unwrap()
            .redo
            .as_ref()
            .unwrap()
            .path
            .clone();
        {
            let document = app.document.as_mut().unwrap();
            assert_eq!(
                document.session.first_rows[0].fields,
                ["a", "b", "c", "d"].map(|field| field.as_bytes().to_vec())
            );
            document.start_move_columns(vec![0], 0).unwrap();
            assert!(document.structural_job.is_none());
            assert_eq!(
                document
                    .working_copy
                    .as_ref()
                    .unwrap()
                    .redo
                    .as_ref()
                    .unwrap()
                    .path,
                redo_path
            );
        }

        app.swap_structural_history(true).unwrap();
        finish_index(app.document.as_mut().unwrap());
        app.apply_delete_columns(vec![0, 2]);
        assert!(app.notice.is_none());
        finish_structural_edit(&mut app);
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.total_columns, 2);
        assert_eq!(
            document.session.first_rows[0].fields,
            ["d", "c"].map(|field| field.as_bytes().to_vec())
        );
        assert_eq!(
            document.session.first_rows[1].fields,
            ["D1", "C1"].map(|field| field.as_bytes().to_vec())
        );
        assert_eq!(document.selected_columns, BTreeSet::from([0]));
        assert_eq!(document.column_selection_anchor, Some(0));
        assert_eq!(fs::read(&source).unwrap(), original);

        app.swap_structural_history(false).unwrap();
        finish_index(app.document.as_mut().unwrap());
        assert_eq!(
            app.document.as_ref().unwrap().session.first_rows[0].fields,
            ["b", "d", "a", "c"].map(|field| field.as_bytes().to_vec())
        );
        app.swap_structural_history(true).unwrap();
        finish_index(app.document.as_mut().unwrap());
        assert_eq!(
            app.document.as_ref().unwrap().session.first_rows[0].fields,
            ["d", "c"].map(|field| field.as_bytes().to_vec())
        );
    }

    #[test]
    fn delete_selected_rows_uses_working_copy_and_structural_history() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("delete-rows.csv");
        let original = b"id,name\n1,Ada\n2,Grace\n3,Linus\n4,Margaret\n";
        fs::write(&source, original).unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.document.as_mut().unwrap());
        {
            let document = app.document.as_mut().unwrap();
            document.cell_edits.insert((3, 1), b"Edited".to_vec());
            document.selected_rows.ranges = vec![2..=2, 4..=4];
            document.selected_rows.anchor = Some(2);
            document.selection = Some(GridSelection::Row { row: 2 });
        }

        app.apply_delete_rows(vec![2..=2, 4..=4]);
        assert!(app.notice.is_none());
        finish_structural_edit(&mut app);
        let document = app.document.as_ref().unwrap();
        assert_eq!(
            document.session.first_rows[0].fields,
            ["id", "name"].map(|field| field.as_bytes().to_vec())
        );
        assert_eq!(
            document.session.first_rows[1].fields,
            ["1", "Ada"].map(|field| field.as_bytes().to_vec())
        );
        assert_eq!(
            document.session.first_rows[2].fields,
            ["3", "Edited"].map(|field| field.as_bytes().to_vec())
        );
        assert!(document.selected_rows.is_empty());
        assert!(document.is_dirty());
        assert_eq!(fs::read(&source).unwrap(), original);

        app.swap_structural_history(false).unwrap();
        finish_index(app.document.as_mut().unwrap());
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.session.first_rows.len(), 5);
        assert_eq!(document.cell_edits.get(&(3, 1)), Some(&b"Edited".to_vec()));
        assert_eq!(fs::read(&source).unwrap(), original);

        app.swap_structural_history(true).unwrap();
        finish_index(app.document.as_mut().unwrap());
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.session.first_rows.len(), 3);
        assert_eq!(document.session.first_rows[2].fields[1], b"Edited");
        assert_eq!(fs::read(&source).unwrap(), original);

        app.apply(&egui::Context::default(), Action::DiscardChanges);
        finish_index(app.document.as_mut().unwrap());
        assert_eq!(app.document.as_ref().unwrap().session.first_rows.len(), 5);
        assert!(!app.document.as_ref().unwrap().is_dirty());
        assert_eq!(fs::read(&source).unwrap(), original);
    }

    #[test]
    fn sort_merge_progress_stays_active_until_the_worker_finishes() {
        let progress = sort_merge_progress(100, 100, false).unwrap();
        assert_eq!(progress.fraction, 0.9);
        assert_eq!(progress.label, "Merging sorted rows…");
        assert!(progress.animate);
        assert!(sort_merge_progress(100, 100, true).is_none());
    }

    #[test]
    fn sort_dialog_is_accessible_and_sort_uses_structural_history() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("sort-from-grid.csv");
        fs::write(
            &source,
            b"key,name\nb,first-b\na,lower\n,missing\nA,upper\nb,second-b\n",
        )
        .unwrap();
        let original = fs::read(&source).unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.document.as_mut().unwrap());
        let document = app.document.as_ref().unwrap();
        assert_eq!(
            document.sort_temporary_disk_estimate(),
            Some(estimate_sort_temporary_bytes(
                document.session.file_size.saturating_add(10),
                5,
            ))
        );
        app.document.as_mut().unwrap().selected_columns = BTreeSet::from([0]);

        let mut dialog = StructuralDialog::sort(0);
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let output = ctx.run(grid_input(), |ctx| {
            assert_eq!(
                show_structural_dialog(
                    ctx,
                    &mut dialog,
                    &mut app.sort_match_case,
                    app.document.as_ref().unwrap(),
                ),
                None
            );
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("the sort dialog should be accessible");
        let accessible_labels = tree
            .nodes
            .iter()
            .filter_map(|(_, node)| node.label().map(str::to_owned))
            .collect::<Vec<_>>();
        for label in ["Ascending", "Descending", "Match case", "Sort", "Cancel"] {
            assert!(
                accessible_labels.iter().any(|candidate| candidate == label),
                "missing accessible sort control or explanation {label}: {accessible_labels:?}"
            );
        }
        let sort_description = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Sort"))
            .and_then(|(_, node)| node.description())
            .expect("the Sort button should describe sort semantics");
        for detail in [
            "Letter case is ignored.",
            "stable sort",
            "header stays fixed",
            "Missing values sort as empty cells",
            "Conservative temporary disk allowance:",
        ] {
            assert!(
                sort_description.contains(detail),
                "missing accessible sort detail {detail}: {sort_description}"
            );
        }

        app.open_structural_dialog(dialog);
        app.apply_structural_dialog_action(StructuralDialogAction::Apply);
        finish_structural_edit(&mut app);
        let document = app.document.as_ref().unwrap();
        assert_eq!(
            document.session.first_rows[0].fields,
            ["key", "name"].map(|value| value.as_bytes().to_vec())
        );
        let sorted_names = document
            .session
            .first_rows
            .iter()
            .skip(1)
            .map(|row| row.fields[1].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            sorted_names,
            ["missing", "lower", "upper", "first-b", "second-b"]
                .map(|value| value.as_bytes().to_vec())
        );
        assert_eq!(document.selected_columns, BTreeSet::from([0]));
        assert!(document.is_dirty());
        assert_eq!(fs::read(&source).unwrap(), original);

        app.swap_structural_history(false).unwrap();
        finish_index(app.document.as_mut().unwrap());
        let original_names = app
            .document
            .as_ref()
            .unwrap()
            .session
            .first_rows
            .iter()
            .skip(1)
            .map(|row| row.fields[1].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            original_names,
            ["first-b", "lower", "missing", "upper", "second-b"]
                .map(|value| value.as_bytes().to_vec())
        );

        app.swap_structural_history(true).unwrap();
        finish_index(app.document.as_mut().unwrap());
        let redone_names = app
            .document
            .as_ref()
            .unwrap()
            .session
            .first_rows
            .iter()
            .skip(1)
            .map(|row| row.fields[1].clone())
            .collect::<Vec<_>>();
        assert_eq!(redone_names, sorted_names);
    }

    #[test]
    fn split_from_numbered_ruler_becomes_the_normal_editable_grid_and_discards_cleanly() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("split-source.csv");
        fs::write(
            &source,
            b"email,city\nalice@example.com,New York\nbob@test.org,Boston\n",
        )
        .unwrap();
        let original = fs::read(&source).unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.document.as_mut().unwrap());

        let document = app.document.as_mut().unwrap();
        let (_ctx, _target) = click_grid_control("Select file column 1 (email)", document);
        assert_eq!(
            document
                .selected_columns
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![0]
        );

        app.open_structural_dialog(StructuralDialog::split(0));
        let dialog = app.structural_dialog.as_mut().unwrap();
        assert_eq!(dialog.columns, vec![0]);
        dialog.separator = "@".into();
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let output = ctx.run(grid_input(), |ctx| {
            assert_eq!(
                show_structural_dialog(
                    ctx,
                    dialog,
                    &mut app.sort_match_case,
                    app.document.as_ref().unwrap(),
                ),
                None
            );
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("the split dialog should be accessible");
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::TextInput
                && node.value() == Some(dialog.separator.as_str())
        }));
        for label in ["OK", "Cancel"] {
            assert!(
                tree.nodes
                    .iter()
                    .any(|(_, node)| node.label() == Some(label)),
                "missing accessible split control {label}"
            );
        }

        app.apply_structural_dialog_action(StructuralDialogAction::Apply);
        finish_structural_edit(&mut app);
        let working_directory = app
            .document
            .as_ref()
            .unwrap()
            .working_copy
            .as_ref()
            .unwrap()
            .directory
            .path()
            .to_path_buf();
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.logical_path, source);
        assert_ne!(document.session.path(), source);
        assert_eq!(document.total_columns, 3);
        assert_eq!(
            document.session.first_rows[0].fields,
            vec![b"email".to_vec(), Vec::new(), b"city".to_vec()]
        );
        assert_eq!(
            document.session.first_rows[1].fields,
            vec![
                b"alice".to_vec(),
                b"example.com".to_vec(),
                b"New York".to_vec()
            ]
        );
        assert_eq!(document.column_name(1), "");
        assert!(document.cell_is_editable(Some(b"example.com")));
        assert!(document.is_dirty());
        assert_eq!(fs::read(&source).unwrap(), original);

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let output = render_grid(&ctx, app.document.as_mut().unwrap());
        let tree = output
            .platform_output
            .accesskit_update
            .expect("the transformed grid should be accessible");
        let focused_header = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Select file column 1 (email)"))
            .map(|(id, _)| *id)
            .expect("the affected numbered header should be accessible");
        assert_eq!(tree.focus, focused_header);
        for label in [
            "Select file column 2 (unnamed header)",
            "Rename file column 2 (unnamed header)",
            "Select row 1, column 2 (unnamed header): example.com",
        ] {
            assert!(
                tree.nodes
                    .iter()
                    .any(|(_, node)| node.label() == Some(label)),
                "missing accessible blank-header label {label}"
            );
        }

        app.apply(&egui::Context::default(), Action::DiscardChanges);
        let document = app.document.as_mut().unwrap();
        finish_index(document);
        assert_eq!(document.session.path(), source);
        assert_eq!(document.total_columns, 2);
        assert!(!document.is_dirty());
        assert!(!working_directory.exists());
        assert_eq!(fs::read(&source).unwrap(), original);
    }

    #[test]
    fn numbered_column_menu_opens_from_the_accessibility_context_action() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("accessible-column-menu.csv");
        fs::write(&source, b"email,city\na@b,x\n").unwrap();
        let mut document = Document::prepare(&source, OpenOptions::default()).unwrap();
        let (ctx, target) = click_grid_control("Select file column 1 (email)", &mut document);
        let initial = render_grid(&ctx, &mut document);
        let initial_tree = initial
            .platform_output
            .accesskit_update
            .expect("the numbered grid should be accessible");
        assert!(initial_tree.nodes.iter().any(|(id, node)| {
            *id == target && node.supports_action(egui::accesskit::Action::ShowContextMenu)
        }));

        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::ShowContextMenu,
                        target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid(ui, &mut document).unwrap();
                });
            },
        );
        let output = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show_grid(ui, &mut document).unwrap();
            });
        });
        let menu_tree = output
            .platform_output
            .accesskit_update
            .expect("the column menu should be accessible");
        for label in [
            "Split Columns…",
            "Combine Columns…",
            "Move Selected Columns…",
            "Delete Selected Columns",
            "Sort Rows…",
        ] {
            assert!(
                menu_tree
                    .nodes
                    .iter()
                    .any(|(_, node)| node.label() == Some(label)),
                "missing accessible column menu item {label}"
            );
        }
        let (_delete_target, delete_node) = menu_tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Delete Selected Columns"))
            .expect("Delete Selected Columns should be present");
        assert!(
            !delete_node.is_disabled(),
            "Delete Selected Columns should be enabled for {:?} of {} columns",
            document.selected_columns,
            document.total_columns
        );
        let (sort_target, sort_node) = menu_tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Sort Rows…"))
            .expect("Sort Rows should be present");
        assert!(
            !sort_node.is_disabled(),
            "Sort Rows should be enabled for exactly one selected column"
        );
        let sort_target = *sort_target;
        let mut request = None;
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: sort_target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    request = show_grid(ui, &mut document).unwrap();
                });
            },
        );
        assert_eq!(
            request,
            Some(GridColumnRequest::Dialog(StructuralDialog::sort(0)))
        );
    }

    #[test]
    fn numbered_rows_are_accessibly_selectable_and_expose_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("accessible-row-menu.csv");
        fs::write(&source, b"name,city\nAda,London\nGrace,Arlington\n").unwrap();
        let mut document = Document::open(
            &source,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let target = grid_control_id(&ctx, "Select row 1", &mut document);
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid(ui, &mut document).unwrap();
                });
            },
        );
        assert_eq!(document.selected_rows.ranges, vec![1..=1]);

        let second_target = grid_control_id(&ctx, "Select row 2", &mut document);
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: second_target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid(ui, &mut document).unwrap();
                });
            },
        );
        assert_eq!(document.selected_rows.ranges, vec![1..=2]);

        let selected = render_grid(&ctx, &mut document);
        let selected_tree = selected
            .platform_output
            .accesskit_update
            .expect("the selected row should be accessible");
        for (row, name) in [(1, "Ada"), (2, "Grace")] {
            let label = format!("Select row {row}");
            let (_, row_node) = selected_tree
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some(label.as_str()))
                .expect("each selected row gutter should be present");
            assert_eq!(row_node.is_selected(), Some(true));
            assert!(row_node.supports_action(egui::accesskit::Action::ShowContextMenu));
            assert!(selected_tree.nodes.iter().any(|(_, node)| {
                node.label() == Some(format!("Select row {row}, column 1 (name): {name}").as_str())
                    && node.is_selected() == Some(true)
            }));
        }

        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::ShowContextMenu,
                        target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid(ui, &mut document).unwrap();
                });
            },
        );
        let menu = render_grid(&ctx, &mut document);
        let (delete_target, delete_node) = menu
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("the row menu should be accessible")
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Delete Selected Rows"))
            .expect("Delete Selected Rows should be present");
        assert!(!delete_node.is_disabled());
        let delete_target = *delete_target;
        let mut request = None;
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: delete_target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    request = show_grid(ui, &mut document).unwrap();
                });
            },
        );
        assert_eq!(request, Some(GridColumnRequest::DeleteRows(vec![1..=2])));
    }

    #[test]
    fn data_cell_context_menu_copies_and_filters_the_full_clicked_value() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("cell-context-menu.csv");
        fs::write(
            &source,
            b"name,note\nalpha,\"line one\nline two\"\nbeta,other\n",
        )
        .unwrap();
        let mut document = Document::open(
            &source,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);

        let _ = click_grid_control("Select row 2, column 1 (name): beta", &mut document);
        assert!(matches!(
            document.selection,
            Some(GridSelection::Cell { row: 2, column: 0 })
        ));

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let cell_label = "Select row 1, column 2 (note): line one\\nline two";
        let open_menu = |document: &mut Document| {
            let target = grid_control_id(&ctx, cell_label, document);
            let _ = ctx.run(
                egui::RawInput {
                    events: vec![egui::Event::AccessKitActionRequest(
                        egui::accesskit::ActionRequest {
                            action: egui::accesskit::Action::ShowContextMenu,
                            target,
                            data: None,
                        },
                    )],
                    ..grid_input()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        show_grid(ui, document).unwrap();
                    });
                },
            );
            assert!(matches!(
                document.selection,
                Some(GridSelection::Cell { row: 1, column: 1 })
            ));
            render_grid(&ctx, document)
        };
        let menu_item = |output: &egui::FullOutput, label: &str| {
            output
                .platform_output
                .accesskit_update
                .as_ref()
                .expect("the cell menu should be accessible")
                .nodes
                .iter()
                .find(|(_, node)| {
                    node.label() == Some(label)
                        && node.supports_action(egui::accesskit::Action::Click)
                        && !node.is_disabled()
                })
                .map(|(id, _)| *id)
                .unwrap_or_else(|| panic!("missing enabled cell menu item {label}"))
        };
        let click_item = |target,
                          document: &mut Document,
                          filter_case_sensitivity: CaseSensitivity| {
            ctx.run(
                egui::RawInput {
                    events: vec![egui::Event::AccessKitActionRequest(
                        egui::accesskit::ActionRequest {
                            action: egui::accesskit::Action::Click,
                            target,
                            data: None,
                        },
                    )],
                    ..grid_input()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        show_grid_with_filter_case(ui, document, filter_case_sensitivity).unwrap();
                    });
                },
            )
        };

        let menu = open_menu(&mut document);
        for label in ["Filter to This Value", "Filter Out This Value", "Copy"] {
            let _ = menu_item(&menu, label);
        }
        let copy_target = menu_item(&menu, "Copy");
        let output = click_item(copy_target, &mut document, CaseSensitivity::Insensitive);
        assert!(output.platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::CopyText(text) if text == "line one\nline two")
        }));

        let menu = open_menu(&mut document);
        let filter_target = menu_item(&menu, "Filter to This Value");
        let _ = click_item(filter_target, &mut document, CaseSensitivity::Insensitive);
        assert_eq!(
            document.filter_query,
            Some(FilterQuery {
                predicates: vec![FilterPredicate {
                    column: 1,
                    operator: FilterOperator::Equals,
                    value: b"line one\nline two".to_vec(),
                }],
                case_sensitivity: CaseSensitivity::Insensitive,
            })
        );
        finish_filter(&mut document);
        document.clear_filter().unwrap();

        let menu = open_menu(&mut document);
        let exclude_target = menu_item(&menu, "Filter Out This Value");
        let _ = click_item(exclude_target, &mut document, CaseSensitivity::Sensitive);
        assert_eq!(
            document.filter_query,
            Some(FilterQuery {
                predicates: vec![FilterPredicate {
                    column: 1,
                    operator: FilterOperator::NotEquals,
                    value: b"line one\nline two".to_vec(),
                }],
                case_sensitivity: CaseSensitivity::Sensitive,
            })
        );
        finish_filter(&mut document);
    }

    #[test]
    fn split_targets_exactly_one_selection_and_keeps_a_wide_result_in_view() {
        let mut selected = BTreeSet::new();
        assert_eq!(selected_split_column(&selected), None);
        selected.insert(39);
        assert_eq!(selected_split_column(&selected), Some(39));
        selected.insert(2);
        assert_eq!(selected_split_column(&selected), None);

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("wide-split.csv");
        let headers = (1..=40)
            .map(|column| format!("column-{column}"))
            .collect::<Vec<_>>()
            .join(",");
        let values = (1..=40)
            .map(|column| {
                if column == 40 {
                    "left:right".to_owned()
                } else {
                    column.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        fs::write(&source, format!("{headers}\n{values}\n")).unwrap();
        let mut app = QuarryApp::new(Some(source), Instant::now());
        finish_index(app.document.as_mut().unwrap());
        app.document
            .as_mut()
            .unwrap()
            .start_split(39, b":".to_vec())
            .unwrap();
        finish_structural_edit(&mut app);

        let document = app.document.as_ref().unwrap();
        assert_eq!(document.total_columns, 41);
        assert_eq!(
            document
                .selected_columns
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![39, 40]
        );
        assert!(document.columns.visible.contains(&39));
        assert!(document.columns.visible.contains(&40));
        assert_eq!(
            document.session.first_rows[1].fields[39..=40],
            [b"left".to_vec(), b"right".to_vec()]
        );
    }

    #[test]
    fn split_then_editing_a_derived_cell_saves_as_without_touching_the_original() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("split-edit-source.csv");
        let destination = directory.path().join("split-edit-output.csv");
        fs::write(&source, b"email,city\nalice@example.com,New York\n").unwrap();
        let original = fs::read(&source).unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.document.as_mut().unwrap());
        app.document
            .as_mut()
            .unwrap()
            .start_split(0, b"@".to_vec())
            .unwrap();
        finish_structural_edit(&mut app);

        let document = app.document.as_mut().unwrap();
        let row = document.data_start;
        document
            .begin_cell_edit(row, 1, b"example.com".to_vec())
            .unwrap();
        document.cell_edit.as_mut().unwrap().draft = "domain.test".into();
        document.commit_cell_edit();
        document.start_save_as(destination.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let saved = loop {
            if let Some(saved) = document.poll_save().unwrap() {
                break saved;
            }
            assert!(Instant::now() < deadline, "Save As timed out");
            std::thread::yield_now();
        };
        assert_eq!(saved, (destination.clone(), false));
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"email,,city\nalice,domain.test,New York\n"
        );
        assert_eq!(fs::read(&source).unwrap(), original);
    }

    #[test]
    fn split_then_save_replaces_the_guarded_original() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("split-save-source.csv");
        fs::write(&source, b"email,city\nalice@example.com,New York\n").unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.document.as_mut().unwrap());
        app.document
            .as_mut()
            .unwrap()
            .start_split(0, b"@".to_vec())
            .unwrap();
        finish_structural_edit(&mut app);

        let document = app.document.as_mut().unwrap();
        document.start_save().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let saved = loop {
            if let Some(saved) = document.poll_save().unwrap() {
                break saved;
            }
            assert!(Instant::now() < deadline, "Save timed out");
            std::thread::yield_now();
        };
        assert_eq!(saved, (source.clone(), true));
        assert_eq!(
            fs::read(&source).unwrap(),
            b"email,,city\nalice,example.com,New York\n"
        );
    }

    #[test]
    fn combine_is_atomic_and_one_step_undo_redo_restores_each_table() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("combine-source.csv");
        fs::write(&source, b"first,last,age\nAda,Lovelace,36\n").unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.document.as_mut().unwrap());
        app.document
            .as_mut()
            .unwrap()
            .start_combine(vec![0, 1], b" ".to_vec())
            .unwrap();
        finish_structural_edit(&mut app);

        let document = app.document.as_ref().unwrap();
        assert_eq!(document.total_columns, 2);
        assert_eq!(
            document.session.first_rows[0].fields,
            vec![b"first".to_vec(), b"age".to_vec()]
        );
        assert_eq!(
            document.session.first_rows[1].fields,
            vec![b"Ada Lovelace".to_vec(), b"36".to_vec()]
        );
        assert!(document.can_undo_structural());
        assert!(document.is_dirty());

        app.swap_structural_history(false).unwrap();
        finish_index(app.document.as_mut().unwrap());
        assert_eq!(app.footer_status.as_deref(), Some("Change undone."));
        assert_eq!(
            app.footer_status.as_ref().map(|message| message.severity),
            Some(MessageSeverity::Status)
        );
        let document = app.document.as_mut().unwrap();
        assert_eq!(document.session.path(), source);
        assert_eq!(document.total_columns, 3);
        assert_eq!(
            document.session.first_rows[1].fields,
            vec![b"Ada".to_vec(), b"Lovelace".to_vec(), b"36".to_vec()]
        );
        assert!(document.can_redo_structural());
        assert!(!document.is_dirty());

        app.swap_structural_history(true).unwrap();
        finish_index(app.document.as_mut().unwrap());
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.total_columns, 2);
        assert_eq!(
            document.session.first_rows[1].fields,
            vec![b"Ada Lovelace".to_vec(), b"36".to_vec()]
        );
        assert!(document.can_undo_structural());
        assert!(document.is_dirty());
    }

    #[test]
    fn split_accepts_a_column_discovered_beyond_a_short_header() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("ragged-header.csv");
        fs::write(&source, b"only\nleft,right:tail\n").unwrap();
        let mut app = QuarryApp::new(Some(source), Instant::now());
        finish_index(app.document.as_mut().unwrap());
        assert_eq!(app.document.as_ref().unwrap().total_columns, 2);
        app.document
            .as_mut()
            .unwrap()
            .start_split(1, b":".to_vec())
            .unwrap();
        finish_structural_edit(&mut app);

        let document = app.document.as_ref().unwrap();
        assert_eq!(document.total_columns, 3);
        assert_eq!(
            document.session.first_rows[0].fields,
            vec![b"only".to_vec(), Vec::new(), Vec::new()]
        );
        assert_eq!(
            document.session.first_rows[1].fields,
            vec![b"left".to_vec(), b"right".to_vec(), b"tail".to_vec()]
        );
    }

    #[test]
    fn structural_undo_restores_prior_sparse_edits_and_later_edits_invalidate_redo() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("undo-overlay.csv");
        fs::write(&source, b"email,city\na@b,x\n").unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.document.as_mut().unwrap());
        let document = app.document.as_mut().unwrap();
        document.rename_header(1, "town".into()).unwrap();
        document.begin_cell_edit(1, 0, b"a@b".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "alpha@beta".into();
        document.commit_cell_edit();
        document.start_split(0, b"@".to_vec()).unwrap();
        finish_structural_edit(&mut app);
        let document = app.document.as_ref().unwrap();
        assert_eq!(
            document.session.first_rows[0].fields,
            vec![b"email".to_vec(), Vec::new(), b"town".to_vec()]
        );
        assert_eq!(
            document.session.first_rows[1].fields,
            vec![b"alpha".to_vec(), b"beta".to_vec(), b"x".to_vec()]
        );

        app.swap_structural_history(false).unwrap();
        finish_index(app.document.as_mut().unwrap());
        let document = app.document.as_mut().unwrap();
        assert_eq!(document.session.path(), source);
        assert_eq!(document.column_name(1), "town");
        assert_eq!(document.cell_edits.get(&(1, 0)).unwrap(), b"alpha@beta");
        assert!(document.is_dirty());
        assert!(document.can_redo_structural());

        document.rename_header(1, "town".into()).unwrap();
        assert!(document.can_redo_structural());
        document.begin_cell_edit(1, 0, b"a@b".to_vec()).unwrap();
        document.commit_cell_edit();
        assert!(document.can_redo_structural());
        assert_eq!(
            document.start_split(0, Vec::new()).unwrap_err(),
            "Enter a non-empty separator."
        );
        assert!(document.can_redo_structural());
        document.start_split(0, b"|".to_vec()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let error = loop {
            match document.poll_structural_edit() {
                Ok(None) => {}
                Ok(Some(_)) => panic!("missing-separator split must not materialize"),
                Err(error) => break error,
            }
            assert!(Instant::now() < deadline, "failed split analysis timed out");
            std::thread::yield_now();
        };
        assert_eq!(error, "The separator was not found in the selected column.");
        assert!(document.can_redo_structural());

        app.swap_structural_history(true).unwrap();
        finish_index(app.document.as_mut().unwrap());
        app.swap_structural_history(false).unwrap();
        finish_index(app.document.as_mut().unwrap());
        let document = app.document.as_mut().unwrap();
        assert!(document.can_redo_structural());
        document.begin_cell_edit(1, 1, b"x".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "y".into();
        document.commit_cell_edit();
        assert!(!document.can_redo_structural());
    }

    #[test]
    fn first_structural_undo_freezes_if_the_original_changed_externally() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("undo-source-guard.csv");
        fs::write(&source, b"email,city\na@b,x\n").unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.document.as_mut().unwrap());
        app.document
            .as_mut()
            .unwrap()
            .start_split(0, b"@".to_vec())
            .unwrap();
        finish_structural_edit(&mut app);

        fs::write(&source, b"email,city\nexternal,change\n").unwrap();
        assert_eq!(
            app.swap_structural_history(false).unwrap_err(),
            SOURCE_CHANGED_NOTICE
        );
        assert!(app.document.as_ref().unwrap().source_changed);
    }

    #[test]
    fn first_combine_freezes_a_missing_source_without_allocating_working_state() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("combine-source-guard.csv");
        fs::write(&source, b"first,last\nAda,Lovelace\n").unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.document.as_mut().unwrap());
        fs::remove_file(source).unwrap();

        let document = app.document.as_mut().unwrap();
        assert_eq!(
            document
                .start_combine(vec![0, 1], b" ".to_vec())
                .unwrap_err(),
            SOURCE_CHANGED_NOTICE
        );
        assert!(document.source_changed);
        assert!(document.original_session.is_none());
        assert!(document.working_copy.is_none());
    }

    #[test]
    fn first_split_freezes_a_missing_source_without_allocating_working_state() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("split-source-guard.csv");
        fs::write(&source, b"email,city\na@b,x\n").unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.document.as_mut().unwrap());
        fs::remove_file(source).unwrap();

        let document = app.document.as_mut().unwrap();
        assert_eq!(
            document.start_split(0, b"@".to_vec()).unwrap_err(),
            SOURCE_CHANGED_NOTICE
        );
        assert!(document.source_changed);
        assert!(document.original_session.is_none());
        assert!(document.working_copy.is_none());
        assert!(document.structural_job.is_none());
    }

    #[test]
    fn selected_cells_and_rows_copy_full_bounded_text() {
        let row = Row {
            offset: 0,
            fields: vec![
                vec![b'x'; 200],
                b"has\ttab".to_vec(),
                b"say \"hi\"".to_vec(),
                Vec::new(),
            ],
        };
        let cell = GridSelection::Cell { row: 0, column: 0 };
        assert_eq!(selection_text(&row, cell, 200).unwrap(), "x".repeat(200));
        assert!(selection_text(&row, cell, 199).is_err());

        let selected_row = GridSelection::Row { row: 0 };
        assert_eq!(
            selection_text(&row, selected_row, 1024).unwrap(),
            format!("{}\t\"has\ttab\"\t\"say \"\"hi\"\"\"\t", "x".repeat(200))
        );

        let invalid = Row {
            offset: 0,
            fields: vec![vec![0xff]],
        };
        assert!(selection_text(&invalid, cell, 1024).is_err());
    }

    #[test]
    fn grid_selection_is_clickable_and_command_c_copies_decoded_data() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-copy-{name}.csv"));
        fs::write(
            &path,
            b"name,notes\nalpha,\"line one\nline two\"\nbeta,other\ngamma,third\ndelta,fourth\n",
        )
        .unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut app = QuarryApp::new(None, Instant::now());
        app.document = Some(document);
        let mut frame = eframe::Frame::_new_kittest();
        let output = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        let target = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present")
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::Button
                    && node.label() == Some("Select row 1, column 2 (notes): line one\\nline two")
            })
            .map(|(id, _)| *id)
            .expect("cell selection should be accessible");
        let output = ctx.run(
            egui::RawInput {
                events: vec![
                    egui::Event::AccessKitActionRequest(egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target,
                        data: None,
                    }),
                    egui::Event::Copy,
                ],
                ..grid_input()
            },
            |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            },
        );
        let selection = app.document.as_ref().unwrap().selection.unwrap();
        assert!(matches!(
            selection,
            GridSelection::Cell {
                row: 1,
                column: 1,
                ..
            }
        ));
        assert_eq!(
            app.document
                .as_ref()
                .unwrap()
                .copy_selection_text()
                .unwrap(),
            "line one\nline two"
        );
        assert!(output.platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::CopyText(text) if text == "line one\nline two")
        }));

        app.find_bar_open = true;
        ctx.memory_mut(|memory| {
            memory.request_focus(egui::Id::new(FIND_INPUT_ID));
        });
        let output = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Copy],
                ..grid_input()
            },
            |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            },
        );
        assert!(!output.platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::CopyText(text) if text == "line one\nline two")
        }));

        let mut document = app.document.take().unwrap();

        let _ = click_grid_control("Select row 1", &mut document);
        assert!(matches!(
            document.selection,
            Some(GridSelection::Row { row: 1, .. })
        ));
        assert_eq!(
            document.copy_selection_text().unwrap(),
            "alpha\t\"line one\nline two\""
        );

        document.set_visible_rows(3).unwrap();
        let (ctx, row_id) = click_grid_control("Select row 3", &mut document);
        document.navigate(2).unwrap();
        assert_eq!(grid_control_id(&ctx, "Select row 3", &mut document), row_id);
        assert!(matches!(
            document.selection,
            Some(GridSelection::Row { row: 3 })
        ));
        document.set_visible_rows(1).unwrap();
        assert!(document.selection.is_none());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn replacement_input_copy_does_not_overwrite_text_with_the_grid_selection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("replacement-copy.csv");
        fs::write(&path, b"name\ngrid value\n").unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document.selection = Some(GridSelection::Cell { row: 1, column: 0 });

        let ctx = egui::Context::default();
        let mut app = QuarryApp::new(None, Instant::now());
        app.find_bar_open = true;
        app.replace_expanded = true;
        app.replace_input = "copy this".into();
        app.document = Some(document);
        let mut frame = eframe::Frame::_new_kittest();
        let _ = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });

        let replace_id = egui::Id::new(REPLACE_INPUT_ID);
        ctx.memory_mut(|memory| memory.request_focus(replace_id));
        let mut state = egui::TextEdit::load_state(&ctx, replace_id)
            .expect("replacement input should retain text-edit state");
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(0),
                egui::text::CCursor::new(4),
            )));
        state.store(&ctx, replace_id);

        let output = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Copy],
                ..grid_input()
            },
            |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            },
        );
        let copied = output
            .platform_output
            .commands
            .iter()
            .filter_map(|command| match command {
                egui::OutputCommand::CopyText(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(copied, ["copy"]);

        app.document.as_mut().unwrap().shutdown();
    }

    #[test]
    fn reference_window_is_dense_and_visible_rows_adapt_to_height() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-density-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        writeln!(file, "name,value").unwrap();
        for row in 1..=120 {
            writeln!(file, "row{row},{row}").unwrap();
        }
        drop(file);

        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        super::configure_style(&ctx);
        let mut app = QuarryApp::new(None, Instant::now());
        app.document = Some(document);
        let mut frame = eframe::Frame::_new_kittest();
        let render = |height: f32, app: &mut QuarryApp, frame: &mut eframe::Frame| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1728.0, height),
                    )),
                    ..Default::default()
                },
                |ctx| eframe::App::update(app, ctx, frame),
            )
        };
        let output = render(1052.0, &mut app, &mut frame);

        let document = app.document.as_ref().unwrap();
        let reference_rows = document.visible_rows;
        assert!(reference_rows > 0);
        assert_eq!(document.display_end(), reference_rows as u64);
        assert!(
            document.buffered_rows.len()
                <= document
                    .visible_rows
                    .saturating_add(2 * super::OVERSCAN_ROWS)
        );
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present");
        let last_row_label = format!("Select row {reference_rows}");
        let last_cell_label = format!("Select row {reference_rows}, column 1");
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::Button
                && node.label() == Some(last_row_label.as_str())
        }));
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::Button
                && node
                    .label()
                    .is_some_and(|label| label.starts_with(&last_cell_label))
        }));
        assert!(
            tree.nodes
                .iter()
                .all(|(_, node)| node.label() != Some("ROW")),
            "the row-number gutter should not have a redundant visible label"
        );

        let _ = render(852.0, &mut app, &mut frame);
        let smaller_rows = app.document.as_ref().unwrap().visible_rows;
        let _ = render(1252.0, &mut app, &mut frame);
        let larger_rows = app.document.as_ref().unwrap().visible_rows;
        assert!(
            smaller_rows < reference_rows,
            "smaller window kept {smaller_rows} rows from the {reference_rows}-row reference"
        );
        assert!(
            larger_rows > reference_rows,
            "larger window kept {larger_rows} rows from the {reference_rows}-row reference"
        );

        app.document.as_mut().unwrap().shutdown();
        drop(app);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn search_controls_are_accessible_and_clickable() {
        let mut query = "needle".to_owned();
        let mut replacement = "replacement".to_owned();
        let mut match_case = false;
        let mut replace_expanded = false;

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let output = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                search_controls(
                    ui,
                    &mut query,
                    &mut replacement,
                    &mut match_case,
                    &mut replace_expanded,
                    true,
                    true,
                    true,
                    false,
                    false,
                );
            });
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("collapsed search controls should be accessible");
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::Button && node.label() == Some("Close find")
        }));
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.label() == Some("Replace")
                && node.toggled() == Some(egui::accesskit::Toggled::False)
        }));
        assert!(!tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::TextInput && node.value() == Some("replacement")
        }));
        let replace_target = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Replace"))
            .map(|(id, _)| *id)
            .expect("Replace should be accessible");
        let output = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: replace_target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    search_controls(
                        ui,
                        &mut query,
                        &mut replacement,
                        &mut match_case,
                        &mut replace_expanded,
                        true,
                        true,
                        true,
                        false,
                        false,
                    );
                });
            },
        );
        assert!(replace_expanded);
        let tree = output
            .platform_output
            .accesskit_update
            .expect("expanding Replace should publish its updated accessibility state");
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.label() == Some("Replace")
                && node.toggled() == Some(egui::accesskit::Toggled::True)
        }));

        let find = click_accessible_button("Find Next", |ui| {
            search_controls(
                ui,
                &mut query,
                &mut replacement,
                &mut match_case,
                &mut replace_expanded,
                true,
                false,
                false,
                false,
                false,
            )
            .0
        });
        assert!(matches!(find, Some(Action::FindNext)));
        let previous = click_accessible_button("Find Previous", |ui| {
            search_controls(
                ui,
                &mut query,
                &mut replacement,
                &mut match_case,
                &mut replace_expanded,
                true,
                true,
                false,
                false,
                false,
            )
            .0
        });
        assert!(matches!(previous, Some(Action::FindPrevious)));
        let replace = click_accessible_button("Replace in Cell", |ui| {
            search_controls(
                ui,
                &mut query,
                &mut replacement,
                &mut match_case,
                &mut replace_expanded,
                true,
                false,
                true,
                false,
                false,
            )
            .0
        });
        assert!(matches!(replace, Some(Action::ReplaceCurrent)));
        let replace_all = click_accessible_button("Replace All", |ui| {
            search_controls(
                ui,
                &mut query,
                &mut replacement,
                &mut match_case,
                &mut replace_expanded,
                true,
                false,
                false,
                false,
                false,
            )
            .0
        });
        assert!(matches!(replace_all, Some(Action::ReplaceAll)));

        let output = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                search_controls(
                    ui,
                    &mut query,
                    &mut replacement,
                    &mut match_case,
                    &mut replace_expanded,
                    true,
                    true,
                    true,
                    false,
                    false,
                );
            });
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("search controls should be accessible");
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::TextInput
                && !node.labelled_by().is_empty()
                && node.value() == Some("replacement")
        }));
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::CheckBox && node.label() == Some("Match case")
        }));
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.label() == Some("Replace")
                && node.toggled() == Some(egui::accesskit::Toggled::True)
        }));
    }

    #[test]
    fn find_bar_shortcuts_are_contextual_and_focus_scoped() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("find-shortcuts.csv");
        fs::write(&path, b"name\nfirst\n").unwrap();

        let mut app = QuarryApp::new(None, Instant::now());
        app.header_mode = HeaderMode::FirstRow;
        app.open_path(path.clone()).unwrap();
        let ctx = egui::Context::default();
        let mut frame = eframe::Frame::_new_kittest();

        let _ = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        assert!(!app.find_bar_open);

        let command = egui::Modifiers::COMMAND;
        let _ = ctx.run(
            egui::RawInput {
                modifiers: command,
                events: vec![egui::Event::Key {
                    key: egui::Key::F,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: command,
                }],
                ..grid_input()
            },
            |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            },
        );
        assert!(app.find_bar_open);
        assert!(ctx.memory(|memory| memory.has_focus(egui::Id::new(FIND_INPUT_ID))));

        let path_position = ctx
            .read_response(egui::Id::new(super::JUMP_INPUT_ID))
            .expect("data row input should be rendered")
            .rect
            .center();
        let _ = ctx.run(
            egui::RawInput {
                events: vec![
                    egui::Event::PointerMoved(path_position),
                    egui::Event::PointerButton {
                        pos: path_position,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: path_position,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::Key {
                        key: egui::Key::Escape,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..grid_input()
            },
            |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            },
        );
        assert!(app.find_bar_open);

        app.document
            .as_mut()
            .unwrap()
            .begin_cell_edit(1, 0, b"first".to_vec())
            .unwrap();
        ctx.memory_mut(|memory| memory.request_focus(egui::Id::new(FIND_INPUT_ID)));
        let _ = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..grid_input()
            },
            |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            },
        );
        assert!(app.find_bar_open);
        assert!(app.document.as_ref().unwrap().cell_edit.is_none());

        let _ = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        ctx.memory_mut(|memory| memory.request_focus(egui::Id::new(FIND_INPUT_ID)));
        let _ = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        assert!(ctx.memory(|memory| memory.has_focus(egui::Id::new(FIND_INPUT_ID))));
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..grid_input()
            },
            |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            },
        );
        assert!(!app.find_bar_open);
        assert!(!ctx.memory(|memory| memory.has_focus(egui::Id::new(FIND_INPUT_ID))));

        app.find_bar_open = true;
        app.replace_expanded = true;
        app.open_path(path).unwrap();
        assert!(!app.find_bar_open);
        assert!(!app.replace_expanded);
        app.document.as_mut().unwrap().shutdown();
    }

    #[test]
    fn filtered_find_button_explains_why_it_is_disabled() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("filtered-find.csv");
        fs::write(&path, b"name\nfirst\nsecond\n").unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document
            .start_filter(FilterQuery::single(
                0,
                FilterOperator::Equals,
                b"first".to_vec(),
            ))
            .unwrap();
        finish_filter(&mut document);

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut app = QuarryApp::new(None, Instant::now());
        app.document = Some(document);
        let mut frame = eframe::Frame::_new_kittest();
        let output = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("filtered toolbar should be accessible");
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.label() == Some("Find")
                && node.is_disabled()
                && node.description() == Some("Clear the filter before using Find.")
        }));

        let command = egui::Modifiers::COMMAND;
        let _ = ctx.run(
            egui::RawInput {
                modifiers: command,
                events: vec![egui::Event::Key {
                    key: egui::Key::F,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: command,
                }],
                ..grid_input()
            },
            |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            },
        );
        assert!(!app.find_bar_open);

        app.document.as_mut().unwrap().shutdown();
    }

    #[test]
    fn shared_job_controls_route_every_cancel_action() {
        for (kind, saving_in_place, label, expected) in [
            (
                ActiveJobKind::Structural,
                false,
                "Cancel Change",
                Action::CancelStructuralEdit,
            ),
            (ActiveJobKind::Save, true, "Cancel Save", Action::CancelSave),
            (
                ActiveJobKind::Save,
                false,
                "Cancel Save As",
                Action::CancelSave,
            ),
            (
                ActiveJobKind::Export,
                false,
                "Cancel Export",
                Action::CancelExport,
            ),
            (
                ActiveJobKind::Filter,
                false,
                "Cancel filter",
                Action::CancelFilter,
            ),
            (
                ActiveJobKind::Search,
                false,
                "Cancel Search",
                Action::CancelSearch,
            ),
            (ActiveJobKind::Index, false, "Cancel", Action::Cancel),
        ] {
            assert_eq!(kind.cancel_action(), expected);
            assert_eq!(kind.cancel_label(saving_in_place), label);
            let display = ActiveJobDisplay {
                label: "Working · 50.0%".into(),
                fraction: 0.5,
                animate: false,
                cancel_action: kind.cancel_action(),
                cancel_label: kind.cancel_label(saving_in_place),
                cancel_enabled: true,
            };
            let action = click_accessible_button(label, |ui| active_job_controls(ui, &display));
            assert_eq!(action, Some(expected));
        }
    }

    #[test]
    fn active_job_priority_is_deterministic() {
        for (active, expected) in [
            ([false; 6], None),
            (
                [true, true, true, true, true, true],
                Some(ActiveJobKind::Structural),
            ),
            (
                [false, true, true, true, true, true],
                Some(ActiveJobKind::Save),
            ),
            (
                [false, false, true, true, true, true],
                Some(ActiveJobKind::Export),
            ),
            (
                [false, false, false, true, true, true],
                Some(ActiveJobKind::Filter),
            ),
            (
                [false, false, false, false, true, true],
                Some(ActiveJobKind::Search),
            ),
            (
                [false, false, false, false, false, true],
                Some(ActiveJobKind::Index),
            ),
        ] {
            assert_eq!(first_active_job(active), expected);
        }
    }

    #[test]
    fn find_next_navigates_resumes_resets_and_reports_no_match() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-find-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        writeln!(file, "one,two,three,four").unwrap();
        for row in 1..=50 {
            match row {
                25 => writeln!(file, "row25,x,needle,x").unwrap(),
                30 => writeln!(file, "fresh,needle,x,x").unwrap(),
                _ => writeln!(file, "row{row},x,x,x").unwrap(),
            }
        }
        drop(file);

        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document.set_visible_rows(5).unwrap();

        document.start_find_next(b"needle").unwrap();
        assert!(!document.header_is_editable(0));
        assert_eq!(
            document.rename_header(0, "renamed".into()).unwrap_err(),
            "Wait for the search to finish before editing headers."
        );
        finish_search(&mut document);
        let first = document.last_match.as_ref().unwrap();
        assert_eq!((first.row, first.column), (25, 2));
        assert_eq!(document.viewport_start, 25);
        assert_eq!(document.reveal_cell, Some((25, 2)));
        assert_eq!(
            document.selection,
            Some(GridSelection::Cell { row: 25, column: 2 })
        );
        assert_eq!(
            document.search_status.as_deref(),
            Some("Found row 25, column 3.")
        );

        document.start_find_next(b"needle").unwrap();
        finish_search(&mut document);
        let second = document.last_match.as_ref().unwrap();
        assert_eq!((second.row, second.column), (30, 1));
        assert_eq!(document.viewport_start, 30);

        document
            .start_find_previous_with_case(b"needle", CaseSensitivity::Insensitive)
            .unwrap();
        let previous = document.last_match.as_ref().unwrap();
        assert_eq!((previous.row, previous.column), (25, 2));
        assert_eq!(
            document.selection,
            Some(GridSelection::Cell { row: 25, column: 2 })
        );
        document.start_find_next(b"needle").unwrap();
        assert!(document.search_job.is_none());
        let forward = document.last_match.as_ref().unwrap();
        assert_eq!((forward.row, forward.column), (30, 1));

        document.start_find_next(b"fresh").unwrap();
        finish_search(&mut document);
        let reset = document.last_match.as_ref().unwrap();
        assert_eq!((reset.row, reset.column), (30, 0));

        document.set_visible_rows(25).unwrap();
        document.start_find_next(b"row50").unwrap();
        finish_search(&mut document);
        assert_eq!(document.viewport_start, 26);
        document.set_visible_rows(10).unwrap();
        assert_eq!(document.viewport_start, 41);
        assert_eq!(document.reveal_cell, Some((50, 0)));

        let before = document.viewport_start;
        document.start_find_next(b"missing").unwrap();
        finish_search(&mut document);
        assert_eq!(document.viewport_start, before);
        assert!(document.last_match.is_none());
        assert_eq!(
            document.search_status.as_deref(),
            Some("No further matches.")
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn find_mode_is_part_of_the_cursor_and_replace_in_cell_uses_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("find-case.csv");
        fs::write(&path, b"value\nNeedle needle\n").unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);

        document
            .start_find_next_with_case(b"needle", CaseSensitivity::Insensitive)
            .unwrap();
        finish_search(&mut document);
        assert_eq!(
            document.selection,
            Some(GridSelection::Cell { row: 1, column: 0 })
        );
        document.reveal_cell.take();
        assert!(document.can_replace_current_with_case(b"needle", CaseSensitivity::Insensitive));
        assert!(!document.can_replace_current_with_case(b"needle", CaseSensitivity::Sensitive));

        document
            .start_find_next_with_case(b"needle", CaseSensitivity::Sensitive)
            .unwrap();
        finish_search(&mut document);
        document
            .replace_current_match_with_case(b"needle", b"X", CaseSensitivity::Sensitive)
            .unwrap();
        assert_eq!(
            document.cell_edits.get(&(1, 0)).map(Vec::as_slice),
            Some(b"Needle X".as_slice())
        );

        document.cancel_search();
        document.shutdown();
    }

    #[test]
    fn find_next_uses_unsaved_cell_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("find-overlay.csv");
        fs::write(&path, b"first,second\nneedle,one\nother,two\n").unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);

        document.begin_cell_edit(1, 0, b"needle".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "hidden".into();
        document.commit_cell_edit();
        document.begin_cell_edit(2, 1, b"two".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "overlay needle".into();
        document.commit_cell_edit();

        document.start_find_next(b"needle").unwrap();
        finish_search(&mut document);
        let found = document.last_match.as_ref().unwrap();
        assert_eq!((found.row, found.column), (2, 1));
        assert_eq!(document.reveal_cell, Some((2, 1)));
        assert!(document.is_dirty());
    }

    #[test]
    fn replace_current_replaces_all_non_overlapping_matches_and_advances() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("replace-current.csv");
        fs::write(&path, b"first,second\naaaa,x\nx,aa\n").unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);

        document.start_find_next(b"aa").unwrap();
        finish_search(&mut document);
        assert!(document.can_replace_current(b"aa"));
        document.replace_current_match(b"aa", b"x").unwrap();
        assert_eq!(document.cell_edits.get(&(1, 0)).unwrap(), b"xx");
        finish_search(&mut document);
        let next = document.last_match.as_ref().unwrap();
        assert_eq!((next.row, next.column), (2, 1));

        document.replace_current_match(b"aa", b"").unwrap();
        assert_eq!(document.cell_edits.get(&(2, 1)).unwrap(), b"");
        finish_search(&mut document);
        assert_eq!(
            document.search_status.as_deref(),
            Some("No further matches.")
        );
        assert!(!document.can_replace_current(b"aa"));
        assert!(document.is_dirty());
    }

    #[test]
    fn replace_current_reports_an_overlay_match_without_a_source_cell() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("overlay-only-replace.csv");
        fs::write(&path, b"first,second\nvalue\n").unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document.search_query = b"needle".to_vec();
        document.last_match = Some(SearchMatch {
            row: 1,
            column: 1,
            record_offset: 0,
        });
        document.selection = Some(GridSelection::Cell { row: 1, column: 1 });
        document.cell_edits.insert((1, 1), b"needle".to_vec());

        assert!(document.can_replace_current(b"needle"));
        assert_eq!(
            document
                .replace_current_match(b"needle", b"replacement")
                .unwrap_err(),
            "The current matched cell is no longer available for replacement."
        );
        assert_eq!(
            document.cell_edits.get(&(1, 1)).map(Vec::as_slice),
            Some(b"needle".as_slice())
        );
        document.shutdown();
    }

    #[test]
    fn replace_current_is_blocked_during_background_rewrites() {
        let directory = tempfile::tempdir().unwrap();
        let open_replaceable = |name: &str| {
            let path = directory.path().join(name);
            fs::write(&path, b"name,other\nneedle,x\n").unwrap();
            let mut document = Document::open(
                &path,
                OpenOptions {
                    header_mode: HeaderMode::FirstRow,
                    ..OpenOptions::default()
                },
            )
            .unwrap();
            finish_index(&mut document);
            document.start_find_next(b"needle").unwrap();
            finish_search(&mut document);
            assert!(document.can_replace_current(b"needle"));
            document
        };

        let mut saving = open_replaceable("replace-during-save.csv");
        saving.rename_header(0, "renamed".into()).unwrap();
        saving
            .start_save_as(directory.path().join("saved.csv"))
            .unwrap();
        assert!(saving.save_job.is_some());
        assert!(!saving.can_replace_current(b"needle"));
        assert!(saving.replace_current_match(b"needle", b"changed").is_err());
        assert!(saving.cell_edits.is_empty());
        saving.shutdown();

        let mut materializing = open_replaceable("replace-during-structural.csv");
        materializing.start_replace_all(b"x", b"y").unwrap();
        assert!(materializing.structural_job.is_some());
        assert!(!materializing.can_replace_current(b"needle"));
        assert!(
            materializing
                .replace_current_match(b"needle", b"changed")
                .is_err()
        );
        assert!(materializing.cell_edits.is_empty());
        materializing.shutdown();
    }

    #[test]
    fn replace_all_materializes_effective_cells_and_uses_change_history() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("replace-all.csv");
        let source = b"name\nplain\nsource needle\n";
        fs::write(&path, source).unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document.begin_cell_edit(1, 0, b"plain".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "overlay needle needle".into();
        document.commit_cell_edit();

        let mut app = QuarryApp::new(None, Instant::now());
        app.find_input = "needle".into();
        app.replace_input = "x".into();
        app.document = Some(document);
        app.apply(&egui::Context::default(), Action::ReplaceAll);
        finish_structural_edit(&mut app);

        let document = app.document.as_ref().unwrap();
        assert_eq!(fs::read(&path).unwrap(), source);
        assert_ne!(document.session.path(), path);
        assert_eq!(
            fs::read(document.session.path()).unwrap(),
            b"name\noverlay x x\nsource x\n"
        );
        assert!(document.is_dirty());
        assert!(app.notice.is_none());
        assert_eq!(
            app.footer_status.as_deref(),
            Some("Replaced 3 occurrences. Save to keep it, or discard changes.")
        );

        app.swap_structural_history(false).unwrap();
        finish_index(app.document.as_mut().unwrap());
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.session.path(), path);
        assert_eq!(
            document.cell_edits.get(&(1, 0)).map(Vec::as_slice),
            Some(b"overlay needle needle".as_slice())
        );
    }

    #[test]
    fn cancel_search_action_sets_state_and_shutdown_clears_the_job() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "quarry-search-cancel-{}-{name}.csv",
            std::process::id()
        ));
        fs::write(&path, b"name\nvalue\n").unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);

        let mut app = QuarryApp::new(None, Instant::now());
        app.find_input = "missing".into();
        app.document = Some(document);
        let ctx = egui::Context::default();
        app.apply(&ctx, Action::FindNext);
        app.apply(&ctx, Action::CancelSearch);
        assert!(
            app.document
                .as_ref()
                .unwrap()
                .search_progress()
                .unwrap()
                .cancelled
        );

        app.document.as_mut().unwrap().shutdown();
        assert!(app.document.as_ref().unwrap().search_job.is_none());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn search_reveals_a_match_beyond_the_first_column_window() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-find-wide-{name}.csv"));
        let headers = (1..=40)
            .map(|column| format!("c{column}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut values = vec!["x"; 40];
        values[39] = "needle";
        fs::write(&path, format!("{headers}\n{}\n", values.join(","))).unwrap();

        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document.set_column_shown(39, false).unwrap();
        assert!(document.columns.hidden[39]);
        document.start_find_next(b"needle").unwrap();
        finish_search(&mut document);

        let found = document.last_match.as_ref().unwrap();
        assert_eq!((found.row, found.column), (1, 39));
        assert!(!document.columns.hidden[39]);
        assert!(document.columns.visible.contains(&39));
        assert_eq!(document.columns.start, 0);
        assert_eq!(document.headers.first().map(String::as_str), Some("c1"));
        assert_eq!(document.headers.last().map(String::as_str), Some("c40"));
        assert_eq!(document.reveal_cell, Some((1, 39)));

        let ctx = egui::Context::default();
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 780.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid(ui, &mut document).unwrap();
                });
            },
        );
        assert!(document.reveal_cell.is_none());

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn column_controls_preserve_source_identity_and_row_copy_order() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-columns-{name}.csv"));
        let headers = (1..=40)
            .map(|column| format!("c{column}"))
            .collect::<Vec<_>>();
        let values = (1..=40)
            .map(|column| format!("v{column}"))
            .collect::<Vec<_>>();
        fs::write(
            &path,
            format!("{}\n{}\n", headers.join(","), values.join(",")),
        )
        .unwrap();

        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);

        document.view_column(39).unwrap();
        assert_eq!(document.columns.start, 0);
        assert_eq!(document.columns.visible.len(), 40);
        assert_eq!(document.headers.first().map(String::as_str), Some("c1"));
        assert_eq!(document.headers.last().map(String::as_str), Some("c40"));

        document.selection = Some(GridSelection::Cell { row: 1, column: 39 });
        assert_eq!(document.copy_selection_text().unwrap(), "v40");
        document.move_column(39, 30).unwrap();
        assert_eq!(&document.columns.order[29..33], &[29, 39, 30, 31]);
        assert_eq!(document.headers[30], "c40");
        assert_eq!(document.headers[31], "c31");
        assert_eq!(document.copy_selection_text().unwrap(), "v40");

        document.move_column(39, 8).unwrap();
        let header_ctx = egui::Context::default();
        let output = header_ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show_grid(ui, &mut document).unwrap();
            });
        });
        let rendered_text = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(rendered_text.windows(2).any(|texts| texts == ["40", "c40"]));
        assert!(!rendered_text.windows(2).any(|texts| texts == ["9", "c40"]));

        document.selection = Some(GridSelection::Row { row: 1 });
        assert_eq!(document.copy_selection_text().unwrap(), values.join("\t"));

        document.selection = Some(GridSelection::Cell { row: 1, column: 39 });
        document.set_column_shown(39, false).unwrap();
        assert!(document.selection.is_none());
        assert!(!document.columns.visible.contains(&39));
        document.set_column_shown(39, true).unwrap();
        document.view_column(39).unwrap();
        assert!(document.columns.visible.contains(&39));

        for column in 0..40 {
            document.set_column_shown(column, false).unwrap();
        }
        assert!(document.headers.is_empty());
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let output = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show_grid(ui, &mut document).unwrap();
            });
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present");
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::Button && node.label() == Some("Select row 1")
        }));
        assert!(!tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::Button
                && node
                    .label()
                    .is_some_and(|label| label.starts_with("Select row 1, column"))
        }));

        document.reset_columns();
        assert_eq!(document.columns.start, 0);
        assert_eq!(document.columns.order, (0..40).collect::<Vec<_>>());
        assert!(document.columns.hidden.iter().all(|hidden| !hidden));
        assert_eq!(document.headers.first().map(String::as_str), Some("c1"));
        assert_eq!(document.headers.last().map(String::as_str), Some("c40"));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn logical_scrollbar_maps_the_full_row_range() {
        let data_start = 1;
        let total_rows = 1_000;
        let visible_rows = 100;

        assert_eq!(
            row_for_scroll_fraction(0.0, data_start, total_rows, visible_rows),
            1
        );
        assert_eq!(
            row_for_scroll_fraction(0.5, data_start, total_rows, visible_rows),
            451
        );
        assert_eq!(
            row_for_scroll_fraction(1.0, data_start, total_rows, visible_rows),
            901
        );
        assert_eq!(
            scroll_fraction_for_row(1, data_start, total_rows, visible_rows),
            0.0
        );
        assert_eq!(
            scroll_fraction_for_row(901, data_start, total_rows, visible_rows),
            1.0
        );

        assert_eq!(max_viewport_start(10, 100), 0);
        assert_eq!(
            row_for_scroll_fraction(1.0, data_start, 10, 100),
            data_start
        );
        assert_eq!(
            logical_viewport_start(u64::MAX, data_start, 10, 100),
            data_start
        );
    }

    #[test]
    fn logical_scrollbar_is_monotonic_at_117_million_rows() {
        let data_start = 1;
        let total_rows = 117_168_829;
        let visible_rows = 24;
        let final_row = data_start + total_rows - visible_rows as u64;
        assert_eq!(
            row_for_scroll_fraction(1.0, data_start, total_rows, visible_rows),
            final_row
        );

        let mut previous = data_start;
        for step in 0..=10_000 {
            let row = row_for_scroll_fraction(
                step as f64 / 10_000.0,
                data_start,
                total_rows,
                visible_rows,
            );
            assert!(row >= previous);
            assert!(row <= final_row);
            previous = row;
        }
        assert_eq!(previous, final_row);
        assert_eq!(row_for_scroll_fraction(1.0, 1, u64::MAX, 1), u64::MAX);
    }

    #[test]
    fn continuous_scrolling_refills_stays_bounded_and_reaches_the_last_record() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-scroll-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        writeln!(file, "name,value").unwrap();
        for row in 1..=250 {
            writeln!(file, "row{row},{row}").unwrap();
        }
        drop(file);

        let mut document = Document::open(&path, OpenOptions::default()).unwrap();
        let index = document.job.take().unwrap().wait().unwrap();

        document.progress.rows_scanned = document.session.first_rows.len() as u64;
        let bootstrap_data_rows = document
            .session
            .first_rows
            .len()
            .saturating_sub(document.data_start as usize);
        document.set_visible_rows(bootstrap_data_rows + 1).unwrap();
        assert_eq!(document.visible_rows().len(), bootstrap_data_rows);

        document.index = Some(index);
        document.set_visible_rows(25).unwrap();
        assert_eq!(document.visible_rows().len(), 25);

        let capacity = document.visible_rows + 2 * super::OVERSCAN_ROWS;
        assert!(document.buffered_rows.len() <= capacity);

        let first = document.data_start;
        document.navigate(first + 100).unwrap();
        document.last_viewport_read = None;
        let mut refills = 0;
        for _ in 0..48 {
            document
                .scroll_by_points(-super::ROW_HEIGHT / 4.0, super::ROW_HEIGHT)
                .unwrap();
            refills += usize::from(document.last_viewport_read.take().is_some());
        }
        assert_eq!(document.viewport_start, first + 112);
        assert!(
            refills <= 1,
            "trackpad-sized scrolling refilled {refills} times"
        );

        document.navigate(first).unwrap();

        let row_stride = super::ROW_HEIGHT;
        document
            .scroll_by_points(-(row_stride - 1.0), row_stride)
            .unwrap();
        assert_eq!(document.viewport_start, first);
        document.scroll_by_points(-1.0, row_stride).unwrap();
        assert_eq!(document.viewport_start, first + 1);
        document.scroll_by_points(row_stride, row_stride).unwrap();
        assert_eq!(document.viewport_start, first);

        document.page(1).unwrap();
        assert_eq!(document.viewport_start, first + 25);
        assert!(document.buffered_rows.len() <= capacity);
        document.page(-1).unwrap();
        assert_eq!(document.viewport_start, first);

        let target = row_for_scroll_fraction(
            1.0,
            document.data_start,
            document.available_data_rows(),
            document.visible_rows,
        );
        document.navigate(target).unwrap();

        assert_eq!(document.display_end(), 250);
        assert_eq!(document.visible_rows().last().unwrap().fields[0], b"row250");
        assert!(document.buffered_rows.len() <= capacity);
        document
            .scroll_by_points(-1_000.0 * row_stride, row_stride)
            .unwrap();
        assert_eq!(document.viewport_start, target);
        assert!(document.buffered_rows.len() <= capacity);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn completed_index_refills_a_short_visible_buffer_without_scrolling() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-index-refill-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        writeln!(file, "name,value").unwrap();
        for row in 1..=250 {
            writeln!(file, "row{row},{row}").unwrap();
        }
        drop(file);

        let mut document = Document::open(&path, OpenOptions::default()).unwrap();
        document.visible_rows = document.buffered_rows.len() + 3;
        let deadline = Instant::now() + Duration::from_secs(2);
        while !document.job.as_ref().unwrap().progress().done {
            assert!(Instant::now() < deadline, "index refill timed out");
            std::thread::yield_now();
        }

        document.poll().unwrap();

        assert_eq!(document.visible_rows().len(), document.visible_rows);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn later_wider_rows_expand_visible_columns() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-ragged-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        let narrow_rows = super::BOOTSTRAP_ROWS + 2 * super::OVERSCAN_ROWS + 1;
        for row in 1..=narrow_rows {
            writeln!(file, "{row},{}", row + 100).unwrap();
        }
        writeln!(file, "{},121,visible", narrow_rows + 1).unwrap();
        drop(file);

        let mut document = Document::open(&path, OpenOptions::default()).unwrap();
        assert!(!document.session.dialect.has_header);
        assert_eq!(document.total_columns, 2);
        document.move_column(1, 0).unwrap();
        document.set_column_shown(0, false).unwrap();

        let index = document.job.take().unwrap().wait().unwrap();
        document.index = Some(index);
        document.navigate(narrow_rows as u64).unwrap();

        assert_eq!(document.total_columns, 3);
        assert_eq!(document.columns.order, [1, 0, 2]);
        assert!(document.columns.hidden[0]);
        assert_eq!(document.columns.visible, [1, 2]);
        assert_eq!(document.headers, ["Column 2", "Column 3"]);
        assert_eq!(
            document.visible_rows().last().unwrap().fields[2],
            b"visible"
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn late_index_error_is_reported_as_failed() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-malformed-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        writeln!(file, "name,value").unwrap();
        for _ in 0..300_000 {
            file.write_all(b"a,1\n").unwrap();
        }
        file.write_all(b"\"unterminated").unwrap();
        drop(file);

        let mut document = Document::open(&path, OpenOptions::default()).unwrap();
        document.last_poll = Instant::now();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !document.job.as_ref().unwrap().progress().done {
            assert!(Instant::now() < deadline, "malformed indexing timed out");
            std::thread::yield_now();
        }
        document.cancel();
        let error = document.poll().unwrap_err();

        assert!(error.contains("unterminated quoted field"));
        assert!(document.job.is_none());
        assert!(document.index.is_none());
        assert_eq!(document.index_status(), "Index failed");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn empty_file_has_no_active_row() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-empty-{name}.csv"));
        File::create(&path).unwrap();

        let mut document = Document::prepare(&path, OpenOptions::default()).unwrap();
        assert!(document.job.is_none());
        document.start_indexing().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !document.job.as_ref().unwrap().progress().done {
            assert!(Instant::now() < deadline, "empty file indexing timed out");
            std::thread::yield_now();
        }
        document.poll().unwrap();
        document.set_visible_rows(25).unwrap();

        assert!(document.job.is_none());
        assert!(document.index.is_some());
        assert_eq!(document.index_status(), "Index complete");
        assert_eq!(document.indexed_fraction(), 1.0);
        assert_eq!(document.available_data_rows(), 0);
        assert!(document.visible_rows().is_empty());
        assert_eq!(document.display_start(), 0);
        assert_eq!(document.display_end(), 0);
        document.index = None;
        assert_eq!(document.index_status(), "Index failed");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn replaces_a_document_while_its_indexer_is_active() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let first = std::env::temp_dir().join(format!("quarry-active-{name}-first.csv"));
        let second = std::env::temp_dir().join(format!("quarry-active-{name}-second.csv"));
        fs::write(&first, b"a,b\n".repeat(250_000)).unwrap();
        fs::write(&second, b"name,value\nsecond,2\n").unwrap();

        let mut current = Document::prepare(&first, OpenOptions::default()).unwrap();
        let job = current
            .session
            .start_indexing(IndexConfig {
                chunk_bytes: 1,
                checkpoint_every: 8,
                memory_budget_bytes: 64 * 1024,
            })
            .unwrap();
        current.progress = job.progress();
        current.job = Some(job);

        let deadline = Instant::now() + Duration::from_secs(2);
        let progress = loop {
            let progress = current.job.as_ref().unwrap().progress();
            if progress.rows_scanned >= 100 || progress.done {
                break progress;
            }
            assert!(Instant::now() < deadline, "index did not make progress");
            std::thread::yield_now();
        };
        assert!(!progress.done, "test file indexed before replacement");

        let mut app = QuarryApp::new(None, Instant::now());
        app.document = Some(current);
        app.open_path(second.clone()).unwrap();

        let document = app.document.as_ref().unwrap();
        assert_eq!(document.session.path(), second);
        assert_eq!(document.buffered_rows[0].fields[0], b"second");

        app.document.as_mut().unwrap().shutdown();
        drop(app);
        for path in [first, second] {
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn failed_and_cancelled_opens_preserve_the_document_and_drops_open_one_file() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let first = std::env::temp_dir().join(format!("quarry-open-{name}-first.csv"));
        let second = std::env::temp_dir().join(format!("quarry-open-{name}-second.csv"));
        let malformed = std::env::temp_dir().join(format!("quarry-open-{name}-bad.csv"));
        fs::write(&first, b"name,value\nfirst,1\n").unwrap();
        fs::write(&second, b"name,value\nsecond,2\n").unwrap();
        fs::write(&malformed, b"\"unterminated").unwrap();

        let mut app = QuarryApp::new(None, Instant::now());
        app.delimiter_mode = DelimiterMode::Comma;
        app.header_mode = HeaderMode::FirstRow;
        app.open_path(first.clone()).unwrap();
        assert_eq!(app.delimiter_mode, DelimiterMode::Comma);
        assert_eq!(app.header_mode, HeaderMode::FirstRow);
        app.document.as_mut().unwrap().move_column(1, 0).unwrap();
        app.document
            .as_mut()
            .unwrap()
            .set_column_shown(0, false)
            .unwrap();
        app.open_picker_result(None);
        assert_eq!(app.document.as_ref().unwrap().session.path(), first);
        assert_eq!(app.delimiter_mode, DelimiterMode::Comma);
        assert_eq!(app.header_mode, HeaderMode::FirstRow);

        app.open_path_and_report(malformed.clone());
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.session.path(), first);
        assert_eq!(document.columns.order, [1, 0]);
        assert!(document.columns.hidden[0]);
        assert!(app.notice.as_deref().unwrap().contains("unterminated"));
        assert_eq!(app.delimiter_mode, DelimiterMode::Comma);
        assert_eq!(app.header_mode, HeaderMode::FirstRow);

        app.open_picker_result(Some(second.clone()));
        assert_eq!(app.document.as_ref().unwrap().session.path(), second);
        assert_eq!(app.delimiter_mode, DelimiterMode::Auto);
        assert_eq!(app.header_mode, HeaderMode::Auto);
        app.reopen_document(DelimiterMode::Comma, HeaderMode::FirstRow);

        app.handle_dropped_paths(vec![Some(first.clone()), Some(second.clone())]);
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.session.path(), first);
        assert_eq!(document.columns.order, [0, 1]);
        assert!(document.columns.hidden.iter().all(|hidden| !hidden));
        assert!(app.notice.as_deref().unwrap().contains("ignored 1"));
        assert_eq!(app.delimiter_mode, DelimiterMode::Auto);
        assert_eq!(app.header_mode, HeaderMode::Auto);

        app.handle_dropped_paths(vec![None]);
        assert_eq!(app.document.as_ref().unwrap().session.path(), first);
        assert!(app.notice.as_deref().unwrap().contains("local file"));

        app.document.as_mut().unwrap().shutdown();
        drop(app);
        for path in [first, second, malformed] {
            fs::remove_file(path).unwrap();
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_open_document_urls_preserve_file_paths() {
        let path = std::env::temp_dir().join("quarry open document.csv");
        let url = objc2_foundation::NSURL::from_file_path(&path).unwrap();
        assert_eq!(super::file_url_path(&url), Some(path));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn queued_macos_open_document_uses_the_source_file() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("quarry-native-open-{name}.csv"));
        fs::write(&source, b"name,value\nfirst,1\n").unwrap();

        let (sender, receiver) = std::sync::mpsc::channel();
        let mut app = QuarryApp::new(None, Instant::now());
        app.delimiter_mode = DelimiterMode::Tab;
        app.header_mode = HeaderMode::NoHeader;
        app.open_document_receiver = Some(receiver);
        sender.send(source.clone()).unwrap();
        app.poll_open_documents();

        assert_eq!(app.document.as_ref().unwrap().session.path(), source);
        assert_eq!(app.delimiter_mode, DelimiterMode::Auto);
        assert_eq!(app.header_mode, HeaderMode::Auto);
        app.document.as_mut().unwrap().shutdown();
        fs::remove_file(source).unwrap();
    }

    #[test]
    fn reload_uses_applied_format_and_reopen_uses_draft() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-format-{name}.csv"));
        fs::write(&path, b"name,value\nfirst,1\n").unwrap();

        let mut app = QuarryApp::new(None, Instant::now());
        app.open_new_path(path.clone()).unwrap();
        assert_eq!(app.delimiter_mode, DelimiterMode::Auto);
        assert_eq!(app.header_mode, HeaderMode::Auto);
        assert_eq!(
            app.document.as_ref().unwrap().session.dialect.delimiter,
            b','
        );
        assert!(app.document.as_ref().unwrap().session.dialect.has_header);

        app.format_draft = Some((DelimiterMode::Tab, HeaderMode::NoHeader));
        fs::write(&path, b"name\tvalue\nsecond\t2\n").unwrap();
        app.reload_document();
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.session.path(), path);
        assert_eq!(document.session.dialect.delimiter, b',');
        assert!(document.session.dialect.has_header);
        assert_eq!(document.headers, ["name\tvalue"]);
        assert_eq!(document.buffered_rows[0].fields[0], b"second\t2");
        assert_eq!(app.delimiter_mode, DelimiterMode::Auto);
        assert_eq!(app.header_mode, HeaderMode::Auto);
        assert_eq!(app.format_draft, None);

        app.reopen_document(DelimiterMode::Tab, HeaderMode::NoHeader);
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.session.path(), path);
        assert_eq!(document.session.dialect.delimiter, b'\t');
        assert!(!document.session.dialect.has_header);
        assert_eq!(document.headers, ["Column 1", "Column 2"]);
        assert_eq!(document.buffered_rows[0].fields[0], b"name");
        assert_eq!(document.buffered_rows[0].fields[1], b"value");
        assert_eq!(app.delimiter_mode, DelimiterMode::Tab);
        assert_eq!(app.header_mode, HeaderMode::NoHeader);
        assert_eq!(DelimiterMode::Pipe.delimiter(), Some(b'|'));
        assert_eq!(DelimiterMode::Semicolon.delimiter(), Some(b';'));

        fs::write(&path, b"\"unterminated").unwrap();
        app.reopen_document(DelimiterMode::Comma, HeaderMode::FirstRow);
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.headers, ["Column 1", "Column 2"]);
        assert_eq!(document.buffered_rows[0].fields[0], b"name");
        assert_eq!(app.delimiter_mode, DelimiterMode::Tab);
        assert_eq!(app.header_mode, HeaderMode::NoHeader);
        assert!(app.notice.as_deref().unwrap().contains("unterminated"));

        app.document.as_mut().unwrap().shutdown();
        drop(app);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn row_and_file_column_inputs_are_one_based() {
        assert_eq!(parse_data_row("1", 1).unwrap(), 1);
        assert_eq!(parse_data_row("100000000", 1).unwrap(), 100_000_000);
        assert_eq!(parse_data_row("0", 1).unwrap_err(), "Data rows start at 1.");
        assert_eq!(parse_file_column("40", 40).unwrap(), 39);
        assert_eq!(
            parse_file_column("0", 40).unwrap_err(),
            "File columns start at 1."
        );
        assert_eq!(
            parse_file_column("41", 40).unwrap_err(),
            "File column must be between 1 and 40."
        );
    }
}
