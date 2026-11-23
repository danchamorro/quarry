mod inspection_ui;
#[cfg(test)]
use quarry_core::estimate_sort_temporary_bytes;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

#[cfg(target_os = "macos")]
use std::fs::{File, OpenOptions as FsOpenOptions};
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "macos")]
use std::sync::{
    Mutex, OnceLock,
    mpsc::{self, Receiver, Sender},
};

use eframe::egui::{self, Align, AtomExt, Color32, FontId, Layout, RichText, TextStyle};
use egui_extras::{Column, TableBuilder};
#[cfg(target_os = "macos")]
use objc2::runtime::{AnyObject, Imp, Sel};
#[cfg(target_os = "macos")]
use objc2::{ffi, sel};
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSApplication, NSApplicationTerminateReply};
#[cfg(target_os = "macos")]
use objc2_foundation::{MainThreadMarker, NSArray, NSURL};
use quarry_core::{
    CaseSensitivity, CleanupOperation, ColumnCleanup, ColumnTransformation, CompletedIndex,
    DateFormatSuggestion, DateTimeFormat, DuplicateSummary, Encoding, FilterOperator,
    FilterPredicate, FilterQuery, HeaderMode, LiteralReplacement, MAX_SORT_KEYS, OpenOptions,
    QuarryError, Session, SortDirection, SortMode, SortSpec,
};
#[cfg(test)]
use quarry_core::{CleanupOutcome, FilterProgress, IndexConfig, SearchMatch};
use quarry_workbench::{
    AppMessage, CloseCommandOutcome, ColumnView, CommandContext, CommandId, DelimiterMode,
    Document, FileCommands, FilterRuleDraft, GridSelection, MAX_GRID_TEXT_CHARS,
    MaterializedWorkingCopy, MessageSeverity, POLL_INTERVAL, RowSelection, SOURCE_CHANGED_NOTICE,
    SelectionModifiers, StructuralDialog, StructuralJob, StructuralOverlay, StructuralRequest,
    WindowId, WorkbenchTab, WorkingCopySnapshot, case_sensitivity, evaluate_command, field_preview,
    field_text, filter_predicate_from_rule, filter_query_from_rules, footer_range_text,
    format_bytes, grid_field_text, group_digits, open_failure_line, parse_data_row,
    parse_file_column, parse_move_position, progress_fraction, row_for_scroll_fraction,
    scroll_fraction_for_row, selected_split_column, sort_direction_label,
};
#[cfg(test)]
use quarry_workbench::{
    CellEdit, FilterReadWindow, MAX_EDIT_HISTORY_BYTES, MAX_EDIT_HISTORY_ENTRIES, OVERSCAN_ROWS,
    WorkingCopyState, parse_insertion_count, parse_insertion_position,
};

mod about;
mod combine_files;
mod compressed;
mod icons;
mod join_files;
mod menu;
mod storage;
mod tabs;
mod theme;
mod whitespace;
use storage::{PendingStorageOperation, StorageReview};
use tabs::{DocumentTab, EguiGridState};

const ROW_HEIGHT: f32 = 22.0;
const COLUMN_RULER_HEIGHT: f32 = 18.0;
const HEADER_NAME_HEIGHT: f32 = 24.0;
const HEADER_HEIGHT: f32 = COLUMN_RULER_HEIGHT + HEADER_NAME_HEIGHT;
const ROW_NUMBER_WIDTH: f32 = 64.0;
/// Horizontal text inset inside grid cells.
const CELL_PADDING: f32 = 8.0;
const MAX_RENDERED_COLUMNS: usize = 64;
const SCROLLBAR_WIDTH: f32 = 12.0;
const MIN_THUMB_HEIGHT: f32 = 24.0;
const APP_ID: &str = "io.github.danchamorro.quarry";
const TOOLBAR_HEIGHT: f32 = 38.0;
const TRAFFIC_LIGHT_INSET: f32 = 72.0;
const STATUS_BAR_HEIGHT: f32 = 26.0;
const STATUS_JOB_WIDTH: f32 = 360.0;
const STATUS_CANCEL_WIDTH: f32 = 112.0;
const DOCUMENT_MENU_WIDTH: f32 = 58.0;
const FORMAT_MENU_WIDTH: f32 = 176.0;
const COMPACT_DOCUMENT_MENU_WIDTH: f32 = 54.0;
const COMPACT_FORMAT_MENU_WIDTH: f32 = 92.0;
const TOOLBAR_JUMP_WIDTH: f32 = 64.0;
const JUMP_INPUT_ID: &str = "quarry-jump-input";
const FIND_INPUT_ID: &str = "quarry-find-input";
const REPLACE_INPUT_ID: &str = "quarry-replace-input";
const COLUMN_SEARCH_INPUT_ID: &str = "quarry-column-search-input";
const FILTER_COLUMN_INPUT_ID: &str = "quarry-filter-column-input";
const FILTER_VALUE_INPUT_ID: &str = "quarry-filter-value-input";
const STRUCTURAL_SEPARATOR_INPUT_ID: &str = "quarry-structural-separator-input";
const STRUCTURAL_POSITION_INPUT_ID: &str = "quarry-structural-position-input";

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
fn native_quit_reply(context: Option<&egui::Context>) -> NSApplicationTerminateReply {
    let Some(context) = context else {
        return NSApplicationTerminateReply::TerminateNow;
    };
    // Native Quit otherwise skips the viewport's unsaved-change and active-save guards.
    context.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Close);
    NSApplicationTerminateReply::TerminateCancel
}

#[cfg(target_os = "macos")]
unsafe extern "C-unwind" fn application_should_terminate(
    _delegate: &AnyObject,
    _selector: Sel,
    _application: &NSApplication,
) -> NSApplicationTerminateReply {
    let context = OPEN_DOCUMENT_TARGET
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("open-document target lock should not be poisoned")
        .as_ref()
        .and_then(|target| target.context.clone());
    native_quit_reply(context.as_ref())
}

#[cfg(target_os = "macos")]
fn install_application_handlers() -> Receiver<PathBuf> {
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
    }
    let selector = sel!(applicationShouldTerminate:);
    if class.instance_method(selector).is_none() {
        let implementation: Imp = unsafe {
            std::mem::transmute(
                application_should_terminate
                    as unsafe extern "C-unwind" fn(
                        &AnyObject,
                        Sel,
                        &NSApplication,
                    )
                        -> NSApplicationTerminateReply,
            )
        };
        let added = unsafe {
            ffi::class_addMethod(
                class as *const _ as *mut _,
                selector,
                implementation,
                c"Q@:@".as_ptr(),
            )
        };
        assert!(added.as_bool(), "failed to install macOS quit handler");
    }
    // AppKit caches which optional delegate methods are implemented.
    application.setDelegate(None);
    application.setDelegate(Some(&delegate));
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
    let _nofile_status = quarry_core::raise_nofile_limit(quarry_workbench::DEFAULT_NOFILE_TARGET);
    let initial_paths = std::env::args_os()
        .skip(1)
        .filter(|argument| !argument.to_string_lossy().starts_with("-psn_"))
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(APP_ID)
            .with_inner_size([1280.0, 780.0])
            .with_min_inner_size([860.0, 540.0])
            .with_fullsize_content_view(true)
            .with_titlebar_shown(false)
            .with_title_shown(false),
        centered: true,
        ..Default::default()
    };

    #[cfg(target_os = "macos")]
    {
        let event_loop =
            winit::event_loop::EventLoop::<eframe::UserEvent>::with_user_event().build()?;
        let open_document_receiver = install_application_handlers();
        let mut app = eframe::create_native(
            "Quarry",
            options,
            Box::new(move |creation| {
                theme::install_fonts(&creation.egui_ctx);
                configure_style(&creation.egui_ctx);
                install_light_appearance();
                install_unified_titlebar();
                attach_open_document_context(&creation.egui_ctx);
                let mut app = QuarryApp::with_paths(initial_paths, started);
                app.open_document_receiver = Some(open_document_receiver);
                app.menu_receiver = menu::install(&creation.egui_ctx);
                Ok(Box::new(app))
            }),
            &event_loop,
        );
        event_loop.run_app(&mut app)?;
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    eframe::run_native(
        "Quarry",
        options,
        Box::new(move |creation| {
            theme::install_fonts(&creation.egui_ctx);
            configure_style(&creation.egui_ctx);
            Ok(Box::new(QuarryApp::with_paths(initial_paths, started)))
        }),
    )
}

struct QuarryApp {
    working_cleanup: quarry_core::WorkingCopyCleanup,
    /// Every running decompression worker of this app, including retiring ones.
    decompression_slots: compressed::DecompressionSlots,
    document_cleanup: Option<tabs::DocumentCleanup>,
    whitespace_preferences: whitespace::Preferences,
    storage_preferences: storage::StoragePreferences,
    about: about::About,
    combine_files: Option<combine_files::CombineDialog>,
    join_files: Option<join_files::JoinDialog>,
    working_directory: PathBuf,
    custom_working_directory: bool,
    scratch_validation_worker: Option<(PathBuf, std::thread::JoinHandle<Result<(), String>>)>,
    index_cache_enabled: bool,
    cache_clear_status: Option<Result<String, String>>,
    cache_clear_worker: Option<std::thread::JoinHandle<Result<String, String>>>,
    active: DocumentTab,
    tabs: Vec<DocumentTab>,
    next_tab_id: u64,
    quit_pending: bool,
    quit_authorized: bool,
    quit_approved: BTreeSet<u64>,
    pending_tab_focus: bool,
    reveal_tab: bool,
    started: Instant,
    logged_first_update: bool,
    #[cfg(target_os = "macos")]
    open_document_receiver: Option<Receiver<PathBuf>>,
    /// A native menu command waiting for the toolbar's action gate this frame.
    pending_menu_action: Option<Action>,
    #[cfg(target_os = "macos")]
    menu_receiver: Option<Receiver<menu::MenuCommand>>,
    #[cfg(target_os = "macos")]
    menu_state: Option<menu::MenuState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GridColumnRequest {
    Dialog(StructuralDialog),
    Delete(Vec<usize>),
    DeleteRows(Vec<RangeInclusive<u64>>),
}

impl QuarryApp {
    /// Turns native menu commands into events or actions, then mirrors availability back.
    #[cfg(target_os = "macos")]
    fn handle_menu_commands(&mut self, ctx: &egui::Context) {
        let commands = self
            .menu_receiver
            .as_ref()
            .map(|receiver| receiver.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        for command in commands {
            match menu::route(
                command,
                self.whitespace_preferences.show_all,
                menu::pasteboard_text,
            ) {
                Some(menu::Routed::Events(events)) => {
                    ctx.input_mut(|input| input.events.extend(events));
                }
                Some(menu::Routed::Action(action)) => self.pending_menu_action = Some(action),
                Some(menu::Routed::Url(url)) => ctx.open_url(egui::OpenUrl::new_tab(url)),
                None => {}
            }
        }
        if self.menu_receiver.is_some() {
            // A focused text field handles its own Undo/Redo through the routed key.
            let text_field_focused = ctx
                .memory(|memory| memory.focused())
                .is_some_and(|id| egui::TextEdit::load_state(ctx, id).is_some());
            let state = self.menu_state(text_field_focused);
            if self.menu_state.as_ref() != Some(&state) {
                menu::sync(&state);
                self.menu_state = Some(state);
            }
        }
    }

    /// Menu availability follows the same blocking rules as the toolbar.
    fn menu_state(&self, text_field_focused: bool) -> menu::MenuState {
        let external_modal_blocked = self.quitting_cleanup()
            || self.active.inspection.recovering()
            || self.active.storage_review.is_some()
            || self.about.open
            || self.combine_files.is_some();
        let open_document_count = self
            .document_tabs()
            .filter(|tab| tab.document.is_some())
            .count();
        let ctx = CommandContext {
            window: WindowId::PRIMARY,
            active_tab: &self.active.state,
            open_document_count,
            write_slot_available: self.write_slot_available(),
            external_modal_blocked,
            text_field_focused,
            show_all_whitespace: self.whitespace_preferences.show_all,
        };
        menu::MenuState {
            disabled: menu::MenuCommand::EVALUATED
                .into_iter()
                .filter(|command| !evaluate_command(command.command_id(), &ctx).enabled)
                .collect(),
            show_all_whitespace: self.whitespace_preferences.show_all,
        }
    }
}

impl QuarryApp {
    /// Opens each command-line path using the same tab and error policy as file drops.
    fn with_paths(paths: Vec<PathBuf>, started: Instant) -> Self {
        let mut app = Self::new(None, started);
        app.whitespace_preferences = whitespace::Preferences::application();
        app.storage_preferences = storage::StoragePreferences::application();
        app.apply_loaded_storage_preferences();
        app.handle_dropped_paths(paths.into_iter().map(Some).collect());
        app
    }

    /// Creates shared application state and an empty tab, optionally opening a source.
    fn new(initial_path: Option<PathBuf>, started: Instant) -> Self {
        let mut app = Self {
            whitespace_preferences: whitespace::Preferences::default(),
            storage_preferences: storage::StoragePreferences::default(),
            about: about::About::new(),
            combine_files: None,
            join_files: None,
            working_cleanup: quarry_core::WorkingCopyCleanup::default(),
            decompression_slots: compressed::DecompressionSlots::default(),
            document_cleanup: None,
            working_directory: std::env::temp_dir(),
            custom_working_directory: false,
            scratch_validation_worker: None,
            index_cache_enabled: true,
            cache_clear_status: None,
            cache_clear_worker: None,
            active: DocumentTab::new(1),
            tabs: Vec::new(),
            next_tab_id: 2,
            quit_pending: false,
            quit_authorized: false,
            quit_approved: BTreeSet::new(),
            pending_tab_focus: false,
            reveal_tab: false,
            started,
            logged_first_update: false,
            #[cfg(target_os = "macos")]
            open_document_receiver: None,
            pending_menu_action: None,
            #[cfg(target_os = "macos")]
            menu_receiver: None,
            #[cfg(target_os = "macos")]
            menu_state: None,
        };
        if let Some(path) = initial_path {
            app.open_path_and_report(path);
        }
        app
    }

    #[cfg(test)]
    fn open_options(&self) -> OpenOptions {
        OpenOptions {
            delimiter: self.active.delimiter_mode.delimiter(),
            header_mode: self.active.header_mode,
            ..OpenOptions::default()
        }
    }

    #[cfg(test)]
    fn open_path(&mut self, path: PathBuf) -> Result<(), String> {
        self.open_path_with_options(path, self.open_options())
            .map_err(|message| message.text)
    }

    /// Opens a source with fresh format detection, preserving every existing tab.
    fn open_new_path(&mut self, path: PathBuf) -> Result<(), AppMessage> {
        self.open_path_with_options(path, OpenOptions::default())?;
        Ok(())
    }

    /// Routes an explicit format to tab opening after checking the modal Combine dialog.
    fn open_path_with_options(
        &mut self,
        path: PathBuf,
        options: OpenOptions,
    ) -> Result<(), AppMessage> {
        if self.combine_files.is_some() {
            return Err(AppMessage::warning(
                "Close Combine Files before opening another document.",
            ));
        }
        self.open_document_tab(path, options)
    }

    /// Prepares a replacement before releasing the active tab's old document.
    /// Callers must first guard against unsaved changes and active file operations.
    fn replace_document_with_options(
        &mut self,
        path: PathBuf,
        options: OpenOptions,
    ) -> Result<(), AppMessage> {
        self.replace_document_with_encoding(path, options, self.active.encoding_mode)
    }

    fn replace_document_with_encoding(
        &mut self,
        path: PathBuf,
        options: OpenOptions,
        encoding: Option<Encoding>,
    ) -> Result<(), AppMessage> {
        if self
            .active
            .document
            .as_ref()
            .is_some_and(Document::join_reserved)
        {
            return Err(AppMessage::warning(join_files::INPUT_RESERVED));
        }
        // Only the bootstrap runs here; a failure leaves the current document intact.
        let document =
            Document::prepare_with_encoding(&path, options, encoding).map_err(AppMessage::error)?;
        self.install_replacement_document(document, encoding)
    }

    /// Starts the prepared replacement's worker, then releases the active tab's old document.
    /// A replacement that must decompress is admitted from the prepared document itself.
    /// The old document's own pass is stopped first, so the shared file-operation limit
    /// is never exceeded, even briefly; if the new pass then fails to start, the old
    /// document stays open as a usable preview.
    fn install_replacement_document(
        &mut self,
        mut document: Document,
        encoding: Option<Encoding>,
    ) -> Result<(), AppMessage> {
        document.working_directory = self.working_directory.clone();
        document.custom_working_directory = self.custom_working_directory;
        document.index_cache_enabled = self.index_cache_enabled;
        document.working_cleanup = self.working_cleanup.clone();
        document.decompression_slots = self.decompression_slots.clone();
        if document.awaiting_decompression() {
            let outgoing = self
                .active
                .document
                .as_ref()
                .is_some_and(Document::decompression_running);
            if !self.write_slot_available_releasing(usize::from(outgoing)) {
                return Err(AppMessage::warning(compressed::WRITE_SLOTS_FULL));
            }
            if let Err(message) = self.ensure_working_directory_available() {
                return Err(AppMessage::warning(message));
            }
            if let Some(current) = self.active.document.as_mut() {
                current.release_decompression();
            }
        }
        document.start_indexing().map_err(AppMessage::error)?;
        if let Some(current) = self.active.document.as_mut() {
            current.shutdown();
        }
        self.active.jump_input = "1".into();
        self.active.column_search_input.clear();
        self.active.columns_open = false;
        self.active.find_bar_open = false;
        self.active.replace_expanded = false;
        self.active.structural_dialog = None;
        self.active.filter_rules = vec![FilterRuleDraft::default()];
        self.active.filters_open = false;
        self.active.format_draft = None;
        self.active.encoding_mode = encoding;
        self.active.document = Some(document);
        self.active.view.grid.reset();
        self.active.footer_status = None;
        Ok(())
    }

    /// Opens a new source and displays any failure on the tab that remains visible.
    fn open_path_and_report(&mut self, path: PathBuf) {
        if let Err(error) = self.open_new_path(path) {
            self.active.notice = Some(error);
        }
    }

    #[cfg(test)]
    fn open_picker_result(&mut self, path: Option<PathBuf>) {
        if let Some(path) = path {
            self.open_path_and_report(path);
        }
    }

    /// Sends all selected native file-picker paths through the multi-file open policy.
    fn choose_file_with_encoding(&mut self, encoding: Encoding) {
        if let Some(paths) = rfd::FileDialog::new()
            .set_title(format!("Open as {}", encoding.label()))
            .pick_files()
        {
            for path in paths {
                if let Err(error) = self.open_document_tab_with_encoding(
                    path,
                    OpenOptions::default(),
                    Some(encoding),
                ) {
                    self.active.notice = Some(error);
                }
            }
        }
    }

    fn choose_file(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .set_title("Open delimited files")
            .pick_files()
        {
            self.handle_dropped_paths(paths.into_iter().map(Some).collect());
        }
    }

    /// Commits this tab's drafts before the native export picker takes over polling.
    fn prepare_filtered_export(&mut self) -> Result<PathBuf, AppMessage> {
        let document = self
            .active
            .document
            .as_mut()
            .ok_or_else(|| AppMessage::warning("Open and filter a file before exporting."))?;
        if document.save_job.is_some() {
            return Err(AppMessage::warning(
                "Cancel the active save before exporting filtered rows.",
            ));
        }
        document.commit_edits();
        if document.filter_refresh_pending {
            return Err(AppMessage::warning(
                "Wait for matches to refresh, then choose Export Filtered Rows again.",
            ));
        }
        Ok(document.logical_path.clone())
    }

    /// Chooses a destination only after the working filter is ready to export.
    fn choose_filtered_export(&mut self) {
        let source = match self.prepare_filtered_export() {
            Ok(source) => source,
            Err(error) => {
                self.active.notice = Some(error);
                return;
            }
        };
        let mut dialog = rfd::FileDialog::new()
            .set_title("Export filtered rows")
            .set_file_name(filtered_export_file_name(&source));
        if let Some(parent) = source.parent() {
            dialog = dialog.set_directory(parent);
        }
        self.export_picker_result(dialog.save_file());
    }

    /// Chooses a destination for this tab; false means no save or review was started.
    fn choose_save_as(&mut self) -> bool {
        self.choose_save_as_format(false)
    }

    fn choose_save_as_utf8(&mut self) -> bool {
        self.choose_save_as_format(true)
    }

    fn choose_save_as_format(&mut self, utf8: bool) -> bool {
        let Some(document) = self.active.document.as_mut() else {
            self.active.notice = Some(AppMessage::warning("Open a file before using Save As."));
            return false;
        };
        document.commit_edits();
        if !(document.is_save_as_ready() || utf8 && document.save_operations_ready()) {
            self.active.notice = Some(AppMessage::warning(if document.save_job.is_some() {
                "A save operation is already running."
            } else if document.export_job.is_some() {
                "Cancel the active export before using Save As."
            } else if document.source_changed {
                SOURCE_CHANGED_NOTICE
            } else {
                document
                    .compressed_unavailable_reason()
                    .unwrap_or("Make a change or reorder columns before using Save As.")
            }));
            return false;
        }
        let source = document.logical_path.clone();
        let mut dialog = rfd::FileDialog::new()
            .set_title(match (utf8, document.is_compressed()) {
                (true, true) => "Save uncompressed UTF-8 copy as",
                (true, false) => "Save a UTF-8 copy",
                (false, true) => "Save uncompressed copy as",
                (false, false) => "Save edited file as",
            })
            .set_file_name(save_as_file_name(&source));
        if let Some(parent) = source.parent() {
            dialog = dialog.set_directory(parent);
        }
        self.save_as_picker_result_format(dialog.save_file(), utf8)
    }

    /// Starts an in-place save or storage review on the active tab, subject to job limits.
    fn save_current(&mut self) -> bool {
        if self.defer_storage(PendingStorageOperation::Save(None)) {
            return self.active.storage_review.is_some();
        }
        let result = self
            .active
            .document
            .as_mut()
            .ok_or_else(|| AppMessage::warning("Open a file before saving."))
            .and_then(Document::start_save);
        self.active.notice = result.err();
        if self.active.notice.is_none() {
            self.active.footer_status = None;
        }
        self.active.notice.is_none()
    }

    /// Rejects cross-tab destinations, then starts or reviews this tab's Save As.
    fn save_as_picker_result(&mut self, destination: Option<PathBuf>) -> bool {
        self.save_as_picker_result_format(destination, false)
    }

    fn save_as_picker_result_format(&mut self, destination: Option<PathBuf>, utf8: bool) -> bool {
        let Some(destination) = destination else {
            return false;
        };
        if let Some(warning) = compressed::gzip_destination_warning(&destination) {
            self.active.notice = Some(warning);
            return false;
        }
        if !self.destination_available(&destination) {
            self.active.notice = Some(AppMessage::warning(
                "This file is open or being written in another tab. Choose another destination or close that tab first.",
            ));
            return false;
        }
        let operation = if utf8 {
            PendingStorageOperation::SaveUtf8(destination.clone())
        } else {
            PendingStorageOperation::Save(Some(destination.clone()))
        };
        if self.defer_storage(operation) {
            return self.active.storage_review.is_some();
        }
        let result = self
            .active
            .document
            .as_mut()
            .ok_or_else(|| AppMessage::warning("Open a file before using Save As."))
            .and_then(|document| {
                if utf8 {
                    document.start_save_operation_format(Some(destination.clone()), true)
                } else {
                    document.start_save_as(destination.clone())
                }
            });
        self.active.notice = result.err();
        if self.active.notice.is_none() {
            self.active.write_destination = Some(destination);
            self.active.footer_status = None;
        }
        self.active.notice.is_none()
    }

    /// Reserves a safe destination for this tab's filtered export after storage approval.
    fn export_picker_result(&mut self, destination: Option<PathBuf>) {
        let Some(destination) = destination else {
            return;
        };
        if let Some(warning) = compressed::gzip_destination_warning(&destination) {
            self.active.notice = Some(warning);
            return;
        }
        if !self.destination_available(&destination) {
            self.active.notice = Some(AppMessage::warning(
                "This file is open or being written in another tab. Choose another destination or close that tab first.",
            ));
            return;
        }
        if self.defer_storage(PendingStorageOperation::Export(destination.clone())) {
            return;
        }
        let result = self
            .active
            .document
            .as_mut()
            .ok_or_else(|| AppMessage::warning("Open and filter a file before exporting."))
            .and_then(|document| document.start_filtered_export(destination.clone()));
        self.active.notice = result.err();
        if self.active.notice.is_none() {
            self.active.write_destination = Some(destination);
            self.active.footer_status = None;
        }
    }

    /// Applies a format draft by guarded replacement of only the active document.
    #[cfg(test)]
    fn reopen_document(&mut self, delimiter_mode: DelimiterMode, header_mode: HeaderMode) {
        self.reopen_document_with_encoding(delimiter_mode, header_mode, self.active.encoding_mode);
    }

    fn reopen_document_with_encoding(
        &mut self,
        delimiter_mode: DelimiterMode,
        header_mode: HeaderMode,
        encoding: Option<Encoding>,
    ) {
        if self
            .active
            .document
            .as_ref()
            .is_some_and(Document::join_reserved)
        {
            self.active.notice = Some(AppMessage::warning(join_files::INPUT_RESERVED));
            return;
        }
        self.active.format_draft = None;
        let Some(path) = self
            .active
            .document
            .as_ref()
            .map(|document| document.logical_path.clone())
        else {
            self.active.notice = Some(AppMessage::warning("Open a file first."));
            return;
        };
        let options = OpenOptions {
            delimiter: delimiter_mode.delimiter(),
            header_mode,
            ..OpenOptions::default()
        };
        // A decompressed document re-reads its verified private copy, not the source.
        let reused = self
            .active
            .document
            .as_ref()
            .filter(|document| document.save_job.is_none() && document.export_job.is_none())
            .and_then(|document| document.reopen_decompressed_copy(options, encoding));
        let result = match reused {
            Some(document) => self.install_replacement_document(document, encoding),
            None => self.reopen_current_path_with_encoding(path, options, encoding),
        };
        if result.is_ok() {
            self.active.delimiter_mode = delimiter_mode;
            self.active.header_mode = header_mode;
        }
        self.active.notice = result.err();
    }

    /// Reloads only the active document using its currently applied format.
    fn reload_document(&mut self) {
        let Some((path, options)) = self.active.document.as_ref().map(|document| {
            (
                document.logical_path.clone(),
                document.current_open_options(),
            )
        }) else {
            self.active.notice = Some(AppMessage::warning("Open a file first."));
            return;
        };
        self.active.notice = self.reopen_current_path(path, options).err();
    }

    /// Refuses replacement while this tab has unsaved edits or a file operation.
    fn reopen_current_path(
        &mut self,
        path: PathBuf,
        options: OpenOptions,
    ) -> Result<(), AppMessage> {
        self.reopen_current_path_with_encoding(path, options, self.active.encoding_mode)
    }

    fn reopen_current_path_with_encoding(
        &mut self,
        path: PathBuf,
        options: OpenOptions,
        encoding: Option<Encoding>,
    ) -> Result<(), AppMessage> {
        if self
            .active
            .document
            .as_ref()
            .is_some_and(Document::join_reserved)
        {
            return Err(AppMessage::warning(join_files::INPUT_RESERVED));
        }
        if let Some(document) = self.active.document.as_mut() {
            document.commit_edits();
            if document.save_job.is_some()
                || document.export_job.is_some()
                || document.structural_job.is_some()
            {
                return Err(AppMessage::warning(
                    "Wait for the active file operation before reopening this document.",
                ));
            }
            if document.is_dirty() {
                return Err(AppMessage::warning(
                    "Discard or save your changes before reopening this document.",
                ));
            }
        }
        self.replace_document_with_encoding(path, options, encoding)
    }

    /// Attempts every local path and collects failures without evicting open documents.
    fn handle_dropped_paths(&mut self, dropped: Vec<Option<PathBuf>>) {
        let mut failures = Vec::new();
        let mut inspection_target = None;
        for path in dropped {
            match path {
                Some(path) => {
                    if let Err(error) = self.open_new_path(path.clone()) {
                        if inspection_target.is_none() {
                            inspection_target = error.inspection_target;
                        }
                        failures.push(open_failure_line(&path, &error.text));
                    }
                }
                None => failures
                    .push("Ignored a dropped item; Quarry can only open a local file.".into()),
            }
        }
        if !failures.is_empty() {
            let mut notice = AppMessage::warning(failures.join("\n"));
            notice.inspection_target = inspection_target;
            self.active.notice = Some(notice);
        }
    }

    #[cfg(target_os = "macos")]
    /// Drains Finder open requests into independent tabs when modal UI permits polling.
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

    /// Dispatches document actions to the active tab and application actions to shared state.
    fn apply(&mut self, ctx: &egui::Context, action: Action) {
        if self.active.inspection.recovering() {
            return;
        }
        if action == Action::CloseTab {
            self.request_tab_close(ctx);
            return;
        }
        if action == Action::JoinFiles {
            self.open_join_files();
            return;
        }
        if action == Action::CombineFiles {
            self.open_combine_files();
            return;
        }
        if matches!(action, Action::InsertRows | Action::InsertColumns) {
            let dialog = self.active.document.as_ref().map(|document| {
                if action == Action::InsertRows {
                    StructuralDialog::insert_rows(
                        document
                            .storage_record_count()
                            .saturating_sub(document.data_start),
                    )
                } else {
                    StructuralDialog::insert_columns(document.total_columns)
                }
            });
            if let Some(dialog) = dialog {
                self.open_structural_dialog(dialog);
            }
            return;
        }
        if let Action::ShowAllWhitespace(all) = action {
            if let Err(error) = self.whitespace_preferences.set(all) {
                self.active.notice = Some(AppMessage::warning(format!(
                    "Whitespace view changed, but its preference could not be saved: {error}"
                )));
            }
            ctx.request_repaint();
            return;
        }
        if action == Action::About {
            self.about.open();
            return;
        }
        if let Some(document) = self.active.document.as_mut() {
            document.commit_edits();
        }
        match action {
            Action::TemporaryStorage => {
                self.open_storage_settings();
                return;
            }
            Action::ReplaceAll if !self.active.storage_approved => {
                if self.defer_storage(PendingStorageOperation::ReplaceAll {
                    query: self.active.find_input.as_bytes().to_vec(),
                    replacement: self.active.replace_input.as_bytes().to_vec(),
                    case_sensitivity: case_sensitivity(self.active.find_match_case),
                }) {
                    return;
                }
            }
            Action::InspectFile => {
                self.start_current_inspection();
                return;
            }
            Action::Choose => return self.choose_file(),
            Action::ChooseWithEncoding(encoding) => {
                return self.choose_file_with_encoding(encoding);
            }
            Action::ReopenWithFormat(delimiter, header, encoding) => {
                return self.reopen_document_with_encoding(delimiter, header, encoding);
            }
            Action::ReloadFromDisk => return self.reload_document(),
            Action::ChooseSaveAs => {
                self.choose_save_as();
                return;
            }
            Action::ChooseSaveAsUtf8 => {
                self.choose_save_as_utf8();
                return;
            }
            Action::Save => {
                self.save_current();
                return;
            }
            Action::ChooseFilteredExport => return self.choose_filtered_export(),
            Action::DiscardChanges => {
                if self
                    .active
                    .document
                    .as_ref()
                    .is_some_and(Document::join_reserved)
                {
                    self.active.notice = Some(AppMessage::warning(join_files::INPUT_RESERVED));
                    return;
                }
                if self.active.document.as_ref().is_some_and(|document| {
                    document.structural_job.is_some()
                        || document.save_job.is_some()
                        || document.export_job.is_some()
                }) {
                    self.active.notice = Some(AppMessage::warning(
                        "Wait for the active file operation before discarding changes.",
                    ));
                    return;
                }
                let structural = self
                    .active
                    .document
                    .as_ref()
                    .is_some_and(Document::has_structural_edits);
                if structural {
                    let document = self
                        .active
                        .document
                        .as_ref()
                        .expect("structural changes require an open document");
                    let path = document.logical_path.clone();
                    let options = document.current_open_options();
                    self.active.notice = self.replace_document_with_options(path, options).err();
                    return;
                }
                if let Some(document) = self.active.document.as_mut() {
                    if document.save_job.is_some() {
                        self.active.notice = Some(AppMessage::warning(
                            "Wait for the save to finish before discarding changes.",
                        ));
                        return;
                    }
                    document.discard_edits();
                    self.active.view.grid.clear_focus();
                }
                self.active.notice = None;
                return;
            }
            Action::OpenColumns => {
                surrender_filter_text_focus(ctx, self.active.filter_rules.len());
                self.active.filters_open = false;
                self.active.format_draft = None;
                ctx.data_mut(|data| {
                    data.remove::<usize>(egui::Id::new("quarry-selected-managed-column"));
                });
                self.active.columns_open = true;
                return;
            }
            Action::Undo => {
                self.active.notice = self.swap_edit_history(false).err();
                return;
            }
            Action::Redo => {
                self.active.notice = self.swap_edit_history(true).err();
                return;
            }
            Action::OpenFilters => {
                ctx.memory_mut(|memory| {
                    memory.surrender_focus(egui::Id::new(COLUMN_SEARCH_INPUT_ID))
                });
                self.active.columns_open = false;
                self.active.format_draft = None;
                self.active.filters_open = true;
                return;
            }
            _ => {}
        }
        let active = &mut self.active.state;
        let grid_view = &mut self.active.view.grid;
        let Some(document) = active.document.as_mut() else {
            return;
        };
        if document.source_changed {
            active.notice = Some(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            return;
        }
        let result = match action {
            Action::InspectFile => unreachable!("inspection handled before document dispatch"),
            Action::Choose
            | Action::ChooseWithEncoding(_)
            | Action::CloseTab
            | Action::TemporaryStorage
            | Action::About
            | Action::ShowAllWhitespace(_)
            | Action::CombineFiles
            | Action::JoinFiles
            | Action::ReopenWithFormat(_, _, _)
            | Action::ReloadFromDisk
            | Action::Save
            | Action::ChooseSaveAs
            | Action::ChooseSaveAsUtf8
            | Action::ChooseFilteredExport
            | Action::DiscardChanges
            | Action::OpenColumns
            | Action::InsertRows
            | Action::InsertColumns
            | Action::Undo
            | Action::Redo
            | Action::OpenFilters => {
                unreachable!()
            }
            Action::PageUp => document.page(-1).map_err(AppMessage::error),
            Action::PageDown => document.page(1).map_err(AppMessage::error),
            Action::AutoFitColumns => {
                grid_view.auto_fit_columns = true;
                Ok(())
            }
            Action::Jump => parse_data_row(&active.jump_input, document.data_start)
                .map_err(AppMessage::warning)
                .and_then(|start| document.navigate(start).map_err(AppMessage::error)),
            Action::FindPrevious => document
                .start_find_previous_with_case(
                    active.find_input.as_bytes(),
                    case_sensitivity(active.find_match_case),
                )
                .map_err(AppMessage::warning),
            Action::FindNext => document
                .start_find_next_with_case(
                    active.find_input.as_bytes(),
                    case_sensitivity(active.find_match_case),
                )
                .map_err(AppMessage::warning),
            Action::ReplaceCurrent => document
                .replace_current_match_with_case(
                    active.find_input.as_bytes(),
                    active.replace_input.as_bytes(),
                    case_sensitivity(active.find_match_case),
                )
                .map_err(AppMessage::warning),
            Action::ReplaceAll => document.start_replace_all_with_case(
                active.find_input.as_bytes(),
                active.replace_input.as_bytes(),
                case_sensitivity(active.find_match_case),
            ),
            Action::CancelSearch => {
                document.cancel_search();
                Ok(())
            }
            Action::ApplyFilter => filter_query_from_rules(
                &active.filter_rules,
                active.filter_match_case,
                document.total_columns,
            )
            .map_err(AppMessage::warning)
            .and_then(|query| document.start_filter(query).map_err(AppMessage::warning)),
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
            Action::Cancel => match document.cancel_decompression() {
                Some(Ok(notice)) => Err(AppMessage::warning(notice)),
                Some(Err(error)) => Err(AppMessage::error(error)),
                None => {
                    document.cancel();
                    Ok(())
                }
            },
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
            self.active.footer_status = if matches!(action, Action::FindPrevious | Action::FindNext)
                && document.search_job.is_none()
            {
                document.search_status.clone().map(AppMessage::status)
            } else {
                None
            };
        }
        self.active.notice = result.err();
    }

    fn apply_column_command(&mut self, ctx: &egui::Context, command: ColumnCommand) {
        if command == ColumnCommand::AutoFit {
            self.apply(ctx, Action::AutoFitColumns);
            return;
        }
        let Some(document) = self.active.document.as_mut() else {
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
        self.active.notice = result.err().map(AppMessage::warning);
    }

    fn open_structural_dialog(&mut self, dialog: StructuralDialog) {
        let result = self
            .active
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
                if dialog.request.is_cleanup()
                    && let Some(document) = self.active.document.as_mut()
                {
                    document.commit_edits();
                }
                self.active.structural_dialog = Some(dialog);
                self.active.notice = None;
            }
            Err(error) => self.active.notice = Some(AppMessage::warning(error)),
        }
    }

    /// Validate and dispatch the selected operation through the shared storage and history flow.
    fn apply_structural_dialog_action(&mut self, action: StructuralDialogAction) {
        if action == StructuralDialogAction::Cancel {
            self.active.structural_dialog = None;
            return;
        }
        let Some(dialog) = self.active.structural_dialog.clone() else {
            return;
        };
        if self.defer_storage(PendingStorageOperation::Structural(dialog.clone())) {
            self.active.structural_dialog = None;
            return;
        }
        let sort_match_case = self.active.state.sort_match_case;
        let result = self
            .active
            .state
            .document
            .as_mut()
            .ok_or_else(|| AppMessage::warning("Open a file before editing columns."))
            .and_then(|document| match dialog.request {
                StructuralRequest::CleanupTrim
                | StructuralRequest::CleanupCollapseSpaces
                | StructuralRequest::CleanupUppercase
                | StructuralRequest::CleanupLowercase
                | StructuralRequest::CleanupFill => document.start_cleanup(dialog.cleanup()),
                StructuralRequest::InsertRows => document.insert_rows_from_dialog(&dialog),
                StructuralRequest::InsertColumns => document.insert_columns_from_dialog(&dialog),
                StructuralRequest::Split => {
                    document.start_split(dialog.columns[0], dialog.separator.as_bytes().to_vec())
                }
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
                StructuralRequest::Sort => {
                    document.start_sort_plan(dialog.sort_plan(sort_match_case))
                }
                StructuralRequest::Duplicates => document.start_find_duplicates_target(
                    dialog.duplicate_target(),
                    case_sensitivity(dialog.match_case),
                ),
            });
        match result {
            Ok(()) => {
                self.active.structural_dialog = None;
                self.active.notice = None;
                self.active.footer_status = self
                    .active
                    .document
                    .as_ref()
                    .filter(|document| document.structural_job.is_none())
                    .and_then(|document| document.structural_status.clone())
                    .map(AppMessage::status);
            }
            Err(error) => self.active.notice = Some(error),
        }
    }

    fn apply_delete_columns(&mut self, columns: Vec<usize>) {
        if self.defer_storage(PendingStorageOperation::DeleteColumns(columns.clone())) {
            return;
        }
        let result = self
            .active
            .document
            .as_mut()
            .ok_or_else(|| AppMessage::warning("Open a file before editing columns."))
            .and_then(|document| document.start_delete_columns(columns));
        self.active.notice = result.err();
        if self.active.notice.is_none() {
            self.active.footer_status = self
                .active
                .document
                .as_ref()
                .filter(|document| document.structural_job.is_none())
                .and_then(|document| document.structural_status.clone())
                .map(AppMessage::status);
        }
    }

    fn apply_delete_rows(&mut self, rows: Vec<RangeInclusive<u64>>) {
        if self.defer_storage(PendingStorageOperation::DeleteRows(rows.clone())) {
            return;
        }
        let result = self
            .active
            .document
            .as_mut()
            .ok_or_else(|| AppMessage::warning("Open a file before deleting rows."))
            .and_then(|document| document.start_delete_rows(rows));
        self.active.notice = result.err();
        if self.active.notice.is_none() {
            self.active.footer_status = self
                .active
                .document
                .as_ref()
                .filter(|document| document.structural_job.is_none())
                .and_then(|document| document.structural_status.clone())
                .map(AppMessage::status);
        }
    }

    fn prepare_materialized_document(
        &self,
        path: &Path,
        options: OpenOptions,
        encoding: Encoding,
        output_index: Option<CompletedIndex>,
    ) -> Result<Document, String> {
        let mut replacement = Document::prepare_with_encoding(path, options, Some(encoding))?;
        if let Some(index) = output_index {
            let index = replacement
                .session
                .adopt_completed_index(index)
                .map_err(|error| error.to_string())?;
            replacement.progress.bytes_scanned = index.indexed_bytes();
            replacement.progress.rows_scanned = index.indexed_rows();
            replacement.progress.done = true;
            replacement.progress.columns_found = index.maximum_width();
            replacement.apply_indexed_width(index.maximum_width());
            replacement.index = Some(index);
            replacement.load_buffer(replacement.viewport_start)?;
        } else {
            replacement.index_cache_enabled = self.index_cache_enabled;
            replacement.start_indexing()?;
        }
        Ok(replacement)
    }

    fn install_materialized_working_copy(
        &mut self,
        mut ready: MaterializedWorkingCopy,
    ) -> Result<(), String> {
        let Some(current) = self.active.document.as_ref() else {
            return Err("The document closed before the change finished.".into());
        };
        let replacement = self.prepare_materialized_document(
            &ready.path,
            current.current_open_options(),
            current.session.encoding(),
            ready.output_index.take(),
        )?;
        self.install_prepared_working_copy(replacement, ready);
        Ok(())
    }

    fn install_prepared_working_copy(
        &mut self,
        mut replacement: Document,
        ready: MaterializedWorkingCopy,
    ) {
        let current = self
            .active
            .document
            .as_mut()
            .expect("the materialized document is still open");
        current.accept_materialized_change(ready.undo);
        replacement.logical_path = current.logical_path.clone();
        replacement.original_session = current.original_session.take();
        replacement.working_copy = current.working_copy.take();
        replacement.working_directory = self.working_directory.clone();
        replacement.custom_working_directory = self.custom_working_directory;
        replacement.working_cleanup = self.working_cleanup.clone();
        replacement.selected_columns = ready.selected_columns;
        let first_selected = replacement.selected_columns.iter().next().copied();
        replacement.column_selection_anchor = first_selected;
        if let Some(first_selected) = first_selected
            && replacement.columns.view(first_selected)
        {
            replacement.refresh_column_headers();
        }
        current.shutdown();
        self.active.document = Some(replacement);
        self.active.view.grid.reset();
        self.active.view.grid.column_focus_requested = first_selected;
        self.active.format_draft = None;
        self.active.columns_open = false;
        self.active.structural_dialog = None;
        self.active.notice = None;
        self.active.footer_status = Some(AppMessage::status(ready.notice));
    }

    fn confirm_duplicate_removal(&mut self) -> Result<(), AppMessage> {
        let (destination, options, encoding, output_index) = {
            let Some(document) = self.active.document.as_ref() else {
                return Ok(());
            };
            let Some(StructuralJob::DuplicatePreview { summary, .. }) = &document.structural_job
            else {
                return Ok(());
            };
            (
                summary.destination.clone(),
                document.current_open_options(),
                document.session.encoding(),
                summary.output_index.clone(),
            )
        };
        {
            let document = self
                .active
                .document
                .as_mut()
                .expect("the preview document is open");
            if document.session.ensure_source_unchanged().is_err()
                || document
                    .original_session
                    .as_ref()
                    .is_some_and(|source| source.ensure_source_unchanged().is_err())
            {
                document.cancel_structural_edit();
                document.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
        }
        // Prepare the candidate (adopting its completed index when present) before consuming
        // the preview or history so a reopen/adoption failure leaves the open document intact.
        let replacement = self
            .prepare_materialized_document(&destination, options, encoding, output_index)
            .map_err(AppMessage::error)?;
        let document = self
            .active
            .document
            .as_mut()
            .expect("the preview document is open");
        let Some(StructuralJob::DuplicatePreview {
            summary,
            selected_columns,
            undo,
        }) = document.structural_job.take()
        else {
            unreachable!("the duplicate preview remains present");
        };
        self.install_prepared_working_copy(
            replacement,
            MaterializedWorkingCopy {
                path: summary.destination,
                output_index: None,
                undo,
                selected_columns,
                notice: format!(
                    "Removed {} extra duplicate rows. Save to keep the change, or discard changes.",
                    summary.duplicate_rows
                ),
            },
        );
        Ok(())
    }

    fn swap_edit_history(&mut self, redo: bool) -> Result<(), AppMessage> {
        let current = self
            .active
            .document
            .as_mut()
            .ok_or_else(|| AppMessage::warning("Open a file before undoing a change."))?;
        if !current.history_ready() {
            return Err(AppMessage::warning(if current.source_changed {
                SOURCE_CHANGED_NOTICE
            } else {
                "Finish editing and wait for active operations before Undo or Redo."
            }));
        }
        match current.session.ensure_source_unchanged() {
            Ok(()) => {}
            Err(QuarryError::SourceChanged) => {
                current.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(error) => return Err(AppMessage::core_error(error)),
        }
        let edit = if redo {
            current.edit_history.redo.pop()
        } else {
            current.edit_history.undo.pop_back()
        };
        let Some(edit) = edit else {
            if current.filter_active() {
                return Err(AppMessage::warning(
                    "Clear the filter before undoing structural changes.",
                ));
            }
            return self.swap_structural_history(redo);
        };
        current.set_edit_value(
            edit.target,
            if redo {
                edit.after.clone()
            } else {
                edit.before.clone()
            },
        );
        current.clear_edit_search();
        if redo {
            current.edit_history.undo.push_back(edit);
        } else {
            current.edit_history.redo.push(edit);
        }
        self.active.footer_status = Some(AppMessage::status(if redo {
            "Edit restored."
        } else {
            "Edit undone."
        }));
        Ok(())
    }

    fn swap_structural_history(&mut self, redo: bool) -> Result<(), AppMessage> {
        let Some(current) = self.active.document.as_mut() else {
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
                Err(error) => return Err(AppMessage::core_error(error)),
            }
        }

        let options = current.current_open_options();
        let logical_path = current.logical_path.clone();
        let current_snapshot = WorkingCopySnapshot {
            path: current.session.path().to_path_buf(),
            overlay: StructuralOverlay {
                header_renames: current.header_renames.as_ref().clone(),
                cell_edits: current.cell_edits.as_ref().clone(),
            },
            edit_history: current.edit_history.clone(),
        };
        let mut replacement = Document::prepare_with_encoding(
            &target.path,
            options,
            Some(current.session.encoding()),
        )
        .map_err(AppMessage::error)?;
        replacement.header_renames = Arc::new(target.overlay.header_renames.clone());
        replacement.cell_edits = Arc::new(target.overlay.cell_edits.clone());
        replacement.edit_history = target.edit_history;
        replacement.refresh_column_headers();
        replacement.index_cache_enabled = self.index_cache_enabled;
        replacement.start_indexing().map_err(AppMessage::error)?;

        let current = self
            .active
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
        replacement.working_directory = self.working_directory.clone();
        replacement.custom_working_directory = self.custom_working_directory;
        replacement.working_cleanup = self.working_cleanup.clone();
        current.shutdown();
        self.active.document = Some(replacement);
        self.active.view.grid.reset();
        self.active.structural_dialog = None;
        self.active.columns_open = false;
        self.active.notice = None;
        self.active.footer_status = Some(AppMessage::status(if redo {
            "Change restored."
        } else {
            "Change undone."
        }));
        Ok(())
    }

    fn copy_selection(&mut self, ctx: &egui::Context) {
        let result = self
            .active
            .document
            .as_ref()
            .ok_or_else(|| "Open a file before copying.".to_owned())
            .and_then(Document::copy_selection_text);
        match result {
            Ok(text) => {
                ctx.copy_text(text);
                self.active.notice = None;
            }
            Err(error) => self.active.notice = Some(AppMessage::warning(error)),
        }
    }

    /// Applies completions to the tab temporarily held in the active slot.
    /// Background polling suppresses changes to the visible tab's editor focus.
    fn poll_active_document(&mut self, ctx: &egui::Context, visible: bool) {
        if let Some(document) = &mut self.active.document
            && let Err(error) = document.poll()
        {
            self.active.footer_status = None;
            self.active.notice = Some(AppMessage::error(error));
        }
        let active = &mut self.active.state;
        if let Some(document) = &mut active.document {
            let was_active = document.search_job.is_some();
            match document.poll_search() {
                Ok(()) if was_active && document.search_job.is_none() => {
                    active.notice = None;
                    active.footer_status = document.search_status.clone().map(AppMessage::status);
                }
                Ok(()) => {}
                Err(error) => {
                    active.footer_status = None;
                    active.notice = Some(AppMessage::error(error));
                }
            }
        }
        if let Some(document) = &mut active.document {
            let was_active = document.filter_job.is_some();
            match document.poll_filter() {
                Ok(()) if was_active && document.filter_job.is_none() => {
                    active.notice = None;
                    active.footer_status = document.filter_status.clone().map(AppMessage::status);
                }
                Ok(()) => {}
                Err(error) => {
                    active.footer_status = None;
                    active.notice = Some(AppMessage::error(error));
                }
            }
        }
        if let Some(document) = &mut self.active.document {
            let was_active = document.export_job.is_some();
            match document.poll_filtered_export() {
                Ok(()) if was_active && document.export_job.is_none() => {
                    let status = document.export_status.clone();
                    if status
                        .as_deref()
                        .is_some_and(|status| status.starts_with("Export failed"))
                    {
                        self.active.footer_status = None;
                        self.active.notice = status.map(AppMessage::error);
                    } else {
                        self.active.notice = None;
                        self.active.footer_status = status.map(AppMessage::status);
                    }
                }
                Ok(()) => {}
                Err(error) => {
                    self.active.footer_status = None;
                    self.active.notice = Some(AppMessage::error(error));
                }
            }
        }
        let structural_was_active = self
            .active
            .document
            .as_ref()
            .is_some_and(|document| document.structural_job.is_some());
        let structural_result = self
            .active
            .document
            .as_mut()
            .map_or(Ok(None), Document::poll_structural_edit);
        match structural_result {
            Ok(Some(ready)) => {
                if let Err(error) = self.install_materialized_working_copy(ready) {
                    self.active.footer_status = None;
                    self.active.notice = Some(AppMessage::error(error));
                }
            }
            Ok(None)
                if structural_was_active
                    && self
                        .active
                        .document
                        .as_ref()
                        .is_some_and(|document| document.structural_job.is_none()) =>
            {
                self.active.notice = None;
                self.active.footer_status = self
                    .active
                    .document
                    .as_ref()
                    .and_then(|document| document.structural_status.clone())
                    .map(AppMessage::status);
            }
            Ok(None) => {}
            Err(error) => {
                self.active.footer_status = None;
                self.active.notice = Some(error);
            }
        }
        if self.active.storage_review.is_none()
            && let Some((transformation, selected_columns, records)) = self
                .active
                .document
                .as_mut()
                .and_then(|document| document.pending_materialization.take())
        {
            self.begin_storage_review(PendingStorageOperation::Materialize(
                transformation,
                selected_columns,
                records,
            ));
        }
        let save_was_active = self
            .active
            .document
            .as_ref()
            .is_some_and(|document| document.save_job.is_some());
        let save_result = self
            .active
            .document
            .as_mut()
            .map_or(Ok(None), Document::poll_save);
        match save_result {
            Ok(Some((destination, in_place))) => {
                let dialect = self
                    .active
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
                let encoding = self
                    .active
                    .document
                    .as_ref()
                    .and_then(|doc| doc.save_output_encoding)
                    .expect("completed save retains its output encoding");
                let prior_choice = self.active.encoding_mode;
                let source_encoding = self.active.document.as_ref().unwrap().session.encoding();
                match self.replace_document_with_encoding(
                    destination.clone(),
                    options,
                    Some(encoding),
                ) {
                    Ok(()) => {
                        if encoding == source_encoding {
                            self.active.encoding_mode = prior_choice;
                        }
                        self.active.notice = None;
                        self.active.footer_status = Some(AppMessage::status(if in_place {
                            format!("Saved {}.", destination.display())
                        } else {
                            format!("Saved as {}.", destination.display())
                        }));
                        if self.active.close_after_save {
                            self.active.close_after_save = false;
                            self.active.close_ready = true;
                        }
                    }
                    Err(error) if in_place => {
                        if let Some(mut document) = self.active.document.take() {
                            document.shutdown();
                        }
                        self.active.find_bar_open = false;
                        self.active.replace_expanded = false;
                        if visible {
                            surrender_find_controls_focus(ctx);
                        }
                        self.active.footer_status = None;
                        self.active.notice = Some(AppMessage::error(format!(
                            "Saved {} but could not reload it: {error}. Reopen the file to continue.",
                            destination.display()
                        )));
                        if self.active.close_after_save {
                            self.active.close_after_save = false;
                            self.active.close_ready = true;
                        }
                    }
                    Err(error) => {
                        if let Some(document) = self.active.document.as_mut() {
                            document.save_status = None;
                        }
                        self.active.footer_status = None;
                        self.active.notice = Some(AppMessage::error(format!(
                            "Saved {} but could not open it: {error}",
                            destination.display()
                        )));
                        if self.active.close_after_save {
                            self.active.close_after_save = false;
                            self.active.close_confirmation_open = true;
                        }
                    }
                }
            }
            Ok(None) => {
                if save_was_active
                    && self
                        .active
                        .document
                        .as_ref()
                        .is_some_and(|document| document.save_job.is_none())
                {
                    self.active.notice = None;
                    self.active.footer_status = self
                        .active
                        .document
                        .as_ref()
                        .and_then(|document| document.save_status.clone())
                        .map(AppMessage::status);
                }
                if self.active.close_after_save
                    && self.active.storage_review.is_none()
                    && self
                        .active
                        .document
                        .as_ref()
                        .is_some_and(|document| document.save_job.is_none())
                {
                    self.active.close_after_save = false;
                    self.active.close_confirmation_open = true;
                }
            }
            Err(error) => {
                self.active.footer_status = None;
                self.active.notice = Some(error);
                if self.active.close_after_save {
                    self.active.close_after_save = false;
                    self.active.close_confirmation_open = true;
                }
            }
        }
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

        self.remember_tab_focus(ctx);
        #[cfg(target_os = "macos")]
        self.handle_menu_commands(ctx);

        if self.active.storage_review.is_none() {
            self.intercept_dirty_close(ctx);

            #[cfg(target_os = "macos")]
            if !self.about.open && self.combine_files.is_none() {
                self.poll_open_documents();
            }
        }

        let local_file_hovered = self.active.document.is_none()
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
        if self.active.storage_review.is_none()
            && !self.about.open
            && self.combine_files.is_none()
            && !dropped_paths.is_empty()
        {
            self.handle_dropped_paths(dropped_paths);
        }

        self.reset_tab_focus(ctx);
        self.poll_document_tabs(ctx);
        self.poll_inspections(ctx);
        self.poll_cache_clear_worker();
        self.poll_scratch_validation_worker();

        if self.active.close_confirmation_open {
            self.about.open = false;
        }
        let tabs_blocked = self.quitting_cleanup()
            || self.active.inspection.recovering()
            || self.active.storage_review.is_some()
            || self.about.open
            || self.combine_files.is_some()
            || self.active.close_confirmation_open
            || self.active.close_after_save
            || self.active.close_after_jobs
            || self.active.structural_dialog.is_some();
        let storage_review_open = self.active.storage_review.is_some();
        let interaction_blocked = self.quitting_cleanup()
            || self.active.inspection.recovering()
            || storage_review_open
            || self.about.open
            || self.combine_files.is_some()
            || self.active.close_after_save
            || self.active.close_after_jobs;
        if storage_review_open && ctx.input(|input| input.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }

        let find_available = self
            .active
            .document
            .as_ref()
            .is_some_and(|document| !document.filter_active());
        let mut focus_find = false;
        if !interaction_blocked
            && find_available
            && !self.active.close_confirmation_open
            && self.active.structural_dialog.is_none()
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::F))
        {
            self.active.find_bar_open = true;
            focus_find = true;
        }
        if !self.active.find_bar_open {
            surrender_find_controls_focus(ctx);
        }

        let mut action = self.pending_menu_action.take();
        let palette = theme::ctx_palette(ctx);
        let titlebar_inset = titlebar_inset(ctx);
        let write_slot_available = self.write_slot_available();
        egui::TopBottomPanel::top("quarry-toolbar")
            .exact_height(TOOLBAR_HEIGHT)
            .show_separator_line(false)
            .frame(theme::chrome_frame(palette).inner_margin(egui::Margin {
                left: (12.0 + titlebar_inset) as i8,
                right: 12,
                top: 0,
                bottom: 0,
            }))
            .show(ctx, |ui| {
                theme::hairline(ui.painter(), ui.max_rect().expand2(egui::vec2(200.0, 0.0)), true, palette.separator);
                titlebar_drag_area(ui);
                if interaction_blocked {
                    ui.disable();
                    ui.set_opacity(1.0);
                }
                theme::chrome_controls(ui.style_mut());
                let document = self.active.state.document.as_ref();
                let document_open = document.is_some();
                let filter_active = document.is_some_and(Document::filter_active);
                let compact = ui.available_width() < 1_000.0;
                ui.spacing_mut().item_spacing.x = if compact { 2.0 } else { 4.0 };
                ui.horizontal_centered(|ui| {
                    if let Some(document_action) = document_menu(
                        ui,
                        document,
                        write_slot_available,
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
                        (
                            self.active.state.delimiter_mode,
                            self.active.state.header_mode,
                            self.active.state.encoding_mode,
                        ),
                        &mut self.active.state.format_draft,
                        compact,
                        if compact {
                            COMPACT_FORMAT_MENU_WIDTH
                        } else {
                            FORMAT_MENU_WIDTH
                        },
                    ) {
                        action = Some(format_action);
                    }
                    if let Some(view_action) = whitespace_view_menu(ui, self.whitespace_preferences.show_all) {
                        action = Some(view_action);
                    }
                    toolbar_separator(ui);

                    let can_undo = document.is_some_and(Document::can_undo);
                    let undo = icons::button(ui, can_undo, icons::Icon::Undo, "Undo", false)
                        .on_disabled_hover_text(
                            "No retained change to undo. Finish editing before using history, and clear filters before whole-file Undo. Edits beyond the history limit can block whole-file Undo. Save to keep changes or discard to restore the source; both reset history.",
                        )
                        .on_hover_text("Undo the last edit or whole-file change (⌘Z).");
                    undo.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            can_undo,
                            "Undo Change",
                        )
                    });
                    if undo.clicked() {
                        action = Some(Action::Undo);
                    }

                    let can_redo = document.is_some_and(Document::can_redo);
                    let redo = icons::button(ui, can_redo, icons::Icon::Redo, "Redo", false)
                        .on_hover_text("Redo the last undone change (⇧⌘Z).");
                    redo.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            can_redo,
                            "Redo Change",
                        )
                    });
                    if redo.clicked() {
                        action = Some(Action::Redo);
                    }
                    toolbar_separator(ui);

                    ui.add_enabled_ui(document_open, |ui| {
                        ui.menu_button("Insert", |ui| {
                            for (label, insertion_action, rows) in [
                                ("Rows…", Action::InsertRows, true),
                                ("Columns…", Action::InsertColumns, false),
                            ] {
                                let reason = document.and_then(|document| document.insertion_disabled_reason(rows));
                                if ui.add_enabled(reason.is_none(), egui::Button::new(label))
                                    .on_disabled_hover_text(reason.unwrap_or_default()).clicked() {
                                    action = Some(insertion_action);
                                    ui.close();
                                }
                            }
                        });
                    });
                    let columns = icons::button(ui, document_open, icons::Icon::Columns, "Columns…", !compact)
                        .on_hover_text("Show, hide, reorder, and fit columns.");
                    ctx.data_mut(|data| data.insert_temp(egui::Id::new(("quarry-column-manager", "anchor")), columns.rect));
                    if columns.clicked() {
                        action = Some(Action::OpenColumns);
                    }
                    let filter_label = filter_button_label(
                        document.and_then(|document| document.filter_query.as_ref()),
                    );
                    let filters = icons::button(ui, document_open, icons::Icon::Filter, &filter_label, !compact)
                        .on_hover_text("Keep rows that match conditions.");
                    if filter_active {
                        ui.painter().rect_stroke(
                            filters.rect,
                            theme::CONTROL_RADIUS,
                            egui::Stroke::new(1.0_f32, palette.selection_strong),
                            egui::StrokeKind::Inside,
                        );
                    }
                    ctx.data_mut(|data| data.insert_temp(egui::Id::new(("quarry-filter-manager", "anchor")), filters.rect));
                    if filters.clicked() {
                        action = Some(Action::OpenFilters);
                    }

                    let jump_enabled = ui.is_enabled() && document_open && !filter_active;
                    let find_disabled_reason = if document_open {
                        "Clear the filter before using Find."
                    } else {
                        "Open a file first."
                    };
                    let find = icons::button(ui, jump_enabled, icons::Icon::Search, "Find", !compact)
                        .on_hover_text("Find and replace (⌘F).")
                        .on_disabled_hover_text(find_disabled_reason);
                    if !jump_enabled {
                        let _ = ui.ctx().accesskit_node_builder(find.id, |node| {
                            node.set_description(find_disabled_reason);
                        });
                    }
                    if find.clicked() {
                        self.active.state.find_bar_open = true;
                        focus_find = true;
                    }

                    // Navigation sits flush right while keeping left-to-right focus order.
                    let text_width = |text: &str| {
                        ui.fonts_mut(|fonts| {
                            fonts
                                .layout_no_wrap(text.to_owned(), FontId::proportional(13.0), Color32::WHITE)
                                .size()
                                .x
                        })
                    };
                    let text_edit_margin = ui.spacing().button_padding.x.min(4.0) * 2.0;
                    let nav_width = text_width("Row")
                        + TOOLBAR_JUMP_WIDTH
                        + text_edit_margin
                        + text_width("Go")
                        + 2.0 * ui.spacing().button_padding.x
                        + 2.0 * (icons::ICON_SIZE + 2.0 * ui.spacing().button_padding.x).max(26.0)
                        + 4.0 * ui.spacing().item_spacing.x;
                    ui.add_space((ui.available_width() - nav_width).max(0.0));
                    let label = ui.label(RichText::new("Row").color(palette.text_secondary));
                    let jump = ui
                        .add_enabled(
                            jump_enabled,
                            egui::TextEdit::singleline(&mut self.active.state.jump_input)
                                .id(egui::Id::new(JUMP_INPUT_ID))
                                .horizontal_align(Align::RIGHT)
                                .vertical_align(Align::Center)
                                .min_size(egui::vec2(0.0, 24.0))
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
                });
            });

        // Tabs sit below the title-bar toolbar, as in Safari and Finder.
        self.show_document_tabs(ctx, tabs_blocked);

        let notice_has_details = self.notice_has_inspection_details();
        let mut show_error_details = false;
        if let Some(notice) = self.active.notice.as_ref() {
            let fill = match notice.severity {
                MessageSeverity::Error => palette.error_fill,
                MessageSeverity::Warning => palette.warning_fill,
                MessageSeverity::Status => unreachable!("status messages belong in the footer"),
            };
            let mut dismiss = false;
            egui::TopBottomPanel::top("quarry-notice")
                .show_separator_line(false)
                .frame(
                    egui::Frame::new()
                        .fill(fill)
                        .inner_margin(egui::Margin::symmetric(14, 6)),
                )
                .show(ctx, |ui| {
                    if interaction_blocked {
                        ui.disable();
                        ui.set_opacity(1.0);
                    }
                    (dismiss, show_error_details) =
                        notice_strip_with_details(ui, notice, notice_has_details);
                });
            if dismiss {
                self.active.notice = None;
            }
        }

        if show_error_details {
            self.show_operation_diagnostic();
        }

        if self.active.find_bar_open {
            let mut close_find = false;
            let find_escape_allowed = !self.about.open
                && !self.active.columns_open
                && !self.active.filters_open
                && !self.active.close_confirmation_open
                && self.active.structural_dialog.is_none()
                && self.active.document.as_ref().is_none_or(|document| {
                    document.cell_edit.is_none() && document.header_edit.is_none()
                });
            egui::TopBottomPanel::top("quarry-find")
                .show_separator_line(false)
                .frame(theme::chrome_frame(palette).inner_margin(egui::Margin::symmetric(14, 6)))
                .show(ctx, |ui| {
                    theme::hairline(
                        ui.painter(),
                        ui.max_rect().expand2(egui::vec2(20.0, 6.0)),
                        true,
                        palette.separator,
                    );
                    if interaction_blocked {
                        ui.disable();
                        ui.set_opacity(1.0);
                    }
                    if focus_find {
                        ui.memory_mut(|memory| {
                            memory.request_focus(egui::Id::new(FIND_INPUT_ID));
                        });
                    }
                    if let Some(document) = self.active.state.document.as_ref() {
                        let can_find_previous = document.can_find_previous_with_case(
                            self.active.state.find_input.as_bytes(),
                            case_sensitivity(self.active.state.find_match_case),
                        );
                        let can_replace = document.can_replace_current_with_case(
                            self.active.state.find_input.as_bytes(),
                            case_sensitivity(self.active.state.find_match_case),
                        );
                        let (search_action, close_requested) = search_controls(
                            ui,
                            &mut self.active.state.find_input,
                            &mut self.active.state.replace_input,
                            &mut self.active.state.find_match_case,
                            &mut self.active.state.replace_expanded,
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
                self.active.find_bar_open = false;
                surrender_find_controls_focus(ctx);
            }
        }

        egui::TopBottomPanel::bottom("quarry-status")
            .exact_height(STATUS_BAR_HEIGHT)
            .show_separator_line(false)
            .frame(theme::chrome_frame(palette).inner_margin(egui::Margin::symmetric(14, 0)))
            .show(ctx, |ui| {
                theme::hairline(
                    ui.painter(),
                    ui.max_rect().expand2(egui::vec2(20.0, 0.0)),
                    false,
                    palette.separator,
                );
                theme::chrome_controls(ui.style_mut());
                ui.style_mut().override_text_style = Some(TextStyle::Small);
                ui.style_mut()
                    .text_styles
                    .insert(TextStyle::Small, FontId::proportional(11.5));
                ui.visuals_mut().override_text_color = Some(palette.text_secondary);
                if interaction_blocked {
                    ui.disable();
                    ui.set_opacity(1.0);
                }
                if let Some(document) = &self.active.document {
                    if let Some(footer_action) =
                        status_bar(ui, document, self.active.footer_status.as_deref())
                    {
                        action = Some(footer_action);
                    }
                } else {
                    ui.label("No file open");
                }
            });

        if !interaction_blocked
            && action.is_none()
            && !self.active.close_confirmation_open
            && self
                .active
                .document
                .as_ref()
                .is_some_and(Document::is_save_ready)
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::S))
        {
            action = Some(Action::Save);
        }
        if !interaction_blocked
            && action.is_none()
            && !self.active.close_confirmation_open
            && self.active.structural_dialog.is_none()
            && self.active.format_draft.is_none()
            && let Some(document) = self.active.document.as_ref()
        {
            action = edit_history_shortcut(ctx, document);
        }
        let filter_owns_keys = self.active.filters_open
            && (egui::Popup::is_any_open(ctx)
                || ctx.memory(|memory| {
                    memory
                        .focused()
                        .is_some_and(|id| is_filter_text_input(id, self.active.filter_rules.len()))
                }));
        if !interaction_blocked
            && action.is_none()
            && self.active.document.is_some()
            && !filter_owns_keys
        {
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
        if !interaction_blocked && let Some(action) = action {
            self.apply(ctx, action);
        }

        if !interaction_blocked && self.active.format_draft.is_some() {
            self.active.columns_open = false;
            self.active.filters_open = false;
            surrender_filter_text_focus(ctx, self.active.filter_rules.len());
            ctx.memory_mut(|memory| memory.surrender_focus(egui::Id::new(COLUMN_SEARCH_INPUT_ID)));
        }
        let column_command = self
            .active
            .state
            .document
            .as_ref()
            .filter(|_| !interaction_blocked)
            .and_then(|document| {
                show_column_manager(
                    ctx,
                    &mut self.active.state.columns_open,
                    &mut self.active.state.column_search_input,
                    document,
                )
            });
        if let Some(command) = column_command {
            self.apply_column_command(ctx, command);
        }

        let filter_action = self
            .active
            .state
            .document
            .as_ref()
            .filter(|_| !interaction_blocked)
            .and_then(|document| {
                show_filter_manager(
                    ctx,
                    &mut self.active.state.filters_open,
                    &mut self.active.state.filter_rules,
                    &mut self.active.state.filter_match_case,
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
            .frame(egui::Frame::new().fill(palette.canvas))
            .show(ctx, |ui| {
                if interaction_blocked {
                    ui.disable();
                    ui.set_opacity(1.0);
                }
                if let Some(document) = self.active.state.document.as_mut() {
                    document.whitespace_all = self.whitespace_preferences.show_all;
                    let grid_result = ui
                        .push_id(self.active.state.id, |ui| {
                            show_grid_with_filter_case(
                                ui,
                                document,
                                &mut self.active.view.grid,
                                case_sensitivity(self.active.state.filter_match_case),
                            )
                        })
                        .inner;
                    match grid_result {
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
            .active
            .state
            .structural_dialog
            .as_mut()
            .filter(|_| !interaction_blocked)
            .zip(self.active.state.document.as_ref())
            .and_then(|(dialog, document)| {
                show_structural_dialog(
                    ctx,
                    dialog,
                    &mut self.active.state.sort_match_case,
                    document,
                )
            });
        if let Some(action) = structural_dialog_action {
            self.apply_structural_dialog_action(action);
        }
        let duplicate_action = self
            .active
            .document
            .as_ref()
            .filter(|_| !interaction_blocked)
            .and_then(|document| {
                let StructuralJob::DuplicatePreview { summary, .. } =
                    document.structural_job.as_ref()?
                else {
                    return None;
                };
                show_duplicate_preview(ctx, summary)
            });
        match duplicate_action {
            Some(StructuralDialogAction::Apply) => {
                self.active.notice = self.confirm_duplicate_removal().err();
            }
            Some(StructuralDialogAction::Cancel) => self.apply(ctx, Action::CancelStructuralEdit),
            None => {}
        }
        let copy_event_targets_selection = self.active.document.as_ref().is_some_and(|document| {
            document.selection.is_some()
                && selection_copy_requested(
                    ctx,
                    self.active.filter_rules.len(),
                    document.header_edit.as_ref().map(|edit| edit.column),
                    document
                        .cell_edit
                        .as_ref()
                        .map(|edit| (edit.row, edit.column)),
                )
        });
        if !interaction_blocked && copy_event_targets_selection {
            self.copy_selection(ctx);
        }
        if grid_error.is_some() {
            self.active.notice = grid_error.map(AppMessage::error);
        }

        if !interaction_blocked && self.active.close_confirmation_open {
            let mut discard_and_close = false;
            let mut keep_editing = false;
            let mut save_and_close = false;
            let mut save_as_and_close = false;
            let save_ready = self
                .active
                .document
                .as_ref()
                .is_some_and(Document::is_save_ready);
            let discard_ready = self
                .active
                .document
                .as_ref()
                .is_none_or(|document| document.save_job.is_none());
            let message = self
                .active
                .document
                .as_ref()
                .map(|document| format!("{} has unsaved changes.", document.logical_path.display()))
                .unwrap_or_default();
            let modal = egui::Modal::new(egui::Id::new("quarry-close-confirmation"))
                .frame(tool_dialog_frame(ctx))
                .show(ctx, |ui| {
                    tool_dialog_style(ui.style_mut());
                    // Wide enough for all four actions on one row without overlap.
                    let actions = [
                        "Discard Changes and Close",
                        "Keep Editing",
                        "Save As and Close…",
                        "Save and Close",
                    ];
                    ui.set_width(
                        theme::buttons_width(ui, &actions)
                            + 3.0 * ui.spacing().item_spacing.x
                            + 24.0,
                    );
                    ui.heading("Unsaved changes");
                    ui.label(message);
                    ui.weak("Save them, save a new copy, keep editing, or discard them.");
                    ui.add_space(6.0);
                    // macOS order: destructive on the left, the default action on the far right.
                    ui.horizontal(|ui| {
                        let palette = theme::ui_palette(ui);
                        if ui
                            .add_enabled(
                                discard_ready,
                                egui::Button::new(
                                    RichText::new("Discard Changes and Close")
                                        .color(palette.error_text),
                                ),
                            )
                            .clicked()
                        {
                            discard_and_close = true;
                        }
                        // Reserve the space so Tab order follows the visible left-to-right order.
                        theme::push_right(
                            ui,
                            theme::buttons_width(
                                ui,
                                &["Keep Editing", "Save As and Close…", "Save and Close"],
                            ),
                        );
                        if ui.button("Keep Editing").clicked() {
                            keep_editing = true;
                        }
                        if ui
                            .add_enabled(save_ready, egui::Button::new("Save As and Close…"))
                            .clicked()
                        {
                            save_as_and_close = true;
                        }
                        if ui
                            .add_enabled(save_ready, theme::primary_button("Save and Close"))
                            .clicked()
                        {
                            save_and_close = true;
                        }
                    });
                });
            keep_editing |= modal.should_close();
            if keep_editing {
                self.keep_editing();
            } else if save_and_close {
                self.active.close_confirmation_open = false;
                self.active.close_after_save = true;
                if !self.save_current() {
                    self.active.close_after_save = false;
                    self.active.close_confirmation_open = true;
                }
            } else if save_as_and_close {
                self.active.close_confirmation_open = false;
                self.active.close_after_save = true;
                if !self.choose_save_as() {
                    self.active.close_after_save = false;
                    self.active.close_confirmation_open = true;
                }
            } else if discard_and_close {
                self.discard_and_close(ctx);
            }
        }
        self.show_closing_operation(ctx);
        if self.active.storage_review.is_some() {
            // Redraw a newly opened review without its previous dialog or an active grid.
            if !storage_review_open {
                ctx.request_discard("storage review opened");
            }
            self.show_storage_review(ctx);
        }
        if self.active.storage_review.is_none()
            && !self.active.close_confirmation_open
            && self.combine_files.is_none()
        {
            self.about.show(ctx);
        }
        if self.document_tabs().any(DocumentTab::busy)
            || self.cache_clear_worker.is_some()
            || self.scratch_validation_worker.is_some()
        {
            ctx.request_repaint_after(POLL_INTERVAL);
        }
        if self.combine_files.is_some() {
            self.show_combine_files(ctx);
        }
        self.show_join_files(ctx);
        self.show_inspection(ctx);
        self.show_document_cleanup(ctx);
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
    InspectFile,
    ShowAllWhitespace(bool),
    CloseTab,
    About,
    CombineFiles,
    JoinFiles,
    Choose,
    ChooseWithEncoding(Encoding),
    TemporaryStorage,
    ReopenWithFormat(DelimiterMode, HeaderMode, Option<Encoding>),
    ReloadFromDisk,
    Save,
    PageUp,
    PageDown,
    AutoFitColumns,
    OpenColumns,
    InsertRows,
    InsertColumns,
    Undo,
    Redo,
    OpenFilters,
    ChooseSaveAs,
    ChooseSaveAsUtf8,
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

fn active_job_display(document: &Document) -> Option<ActiveJobDisplay> {
    if let Some(display) = compressed::decompression_display(document) {
        return Some(display);
    }
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

#[cfg(test)]
fn notice_strip(ui: &mut egui::Ui, notice: &AppMessage) -> bool {
    notice_strip_with_details(ui, notice, false).0
}

/// A one-line banner: severity mark and message on the left, actions on the right.
/// Returns (dismiss, show details).
fn notice_strip_with_details(
    ui: &mut egui::Ui,
    notice: &AppMessage,
    details: bool,
) -> (bool, bool) {
    let palette = theme::ui_palette(ui);
    let (color, live) = match notice.severity {
        MessageSeverity::Error => (palette.error_text, egui::accesskit::Live::Assertive),
        MessageSeverity::Warning => (palette.warning_text, egui::accesskit::Live::Polite),
        MessageSeverity::Status => unreachable!("status messages belong in the footer"),
    };
    let mut dismiss = false;
    let mut show_details = false;
    let actions_width = if details { 210.0 } else { 90.0 };
    ui.horizontal(|ui| {
        let (mark, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
        ui.painter().circle_filled(mark.center(), 7.0, color);
        ui.painter().text(
            mark.center(),
            egui::Align2::CENTER_CENTER,
            "!",
            FontId::proportional(11.0),
            palette.canvas,
        );
        let width = (ui.available_width() - actions_width).max(0.0);
        let response = add_aligned(
            ui,
            egui::vec2(width, 0.0),
            Align::Min,
            egui::Label::new(RichText::new(&notice.text).color(color)).wrap(),
        );
        let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
            node.set_live(live);
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            dismiss = ui.button("Dismiss").clicked();
            if details {
                show_details = ui.button("Details / Inspect…").clicked();
            }
        });
    });
    (dismiss, show_details)
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
                let palette = theme::ui_palette(ui);
                ui.spacing_mut().item_spacing.x = 14.0;
                if document.is_dirty() {
                    ui.colored_label(palette.accent_text, "● Unsaved changes");
                }
                ui.label(RichText::new(footer_range_text(document)).color(palette.text));
                if let Some(label) = document.compressed_label() {
                    ui.label(label);
                }

                if metadata_width >= 540.0 {
                    ui.label(format!(
                        "{} column{}{}",
                        group_digits(document.known_width() as u64),
                        if document.known_width() == 1 { "" } else { "s" },
                        if document.width_confirmed() {
                            ""
                        } else {
                            " found so far"
                        }
                    ));
                }
                let shown = document.columns.shown_count();
                if metadata_width >= 650.0 && shown != document.known_width() {
                    ui.label(format!("{} shown", group_digits(shown as u64)));
                }
                if metadata_width >= 760.0 && !document.is_compressed() {
                    ui.label(format_bytes(document.session.file_size));
                }
                let selected_rows = document.selected_rows.count();
                let selected_columns = document.selected_columns.len();
                if metadata_width >= 840.0 && (selected_rows > 0 || selected_columns > 0) {
                    let text = if selected_rows > 0 {
                        format!(
                            "{} row{} selected",
                            group_digits(selected_rows),
                            if selected_rows == 1 { "" } else { "s" }
                        )
                    } else {
                        format!(
                            "{selected_columns} column{} selected",
                            if selected_columns == 1 { "" } else { "s" }
                        )
                    };
                    ui.colored_label(palette.accent_text, text);
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
                    let palette = theme::ui_palette(ui);
                    let (dot, _) =
                        ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                    let color = match status {
                        "Index complete" => palette.success_text,
                        "Indexing" => palette.selection_strong,
                        _ => palette.text_tertiary,
                    };
                    ui.painter().circle_filled(dot.center(), 3.0, color);
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
        .right_text(egui::Atom::custom(arrow_id, egui::vec2(9.0, 9.0)))
        .min_size(egui::vec2(width, 26.0))
        .truncate();
    let direction = ui.layout().main_dir();
    let rendered = ui
        .allocate_ui_with_layout(
            egui::vec2(width, 26.0),
            Layout::centered_and_justified(direction),
            |ui| button.atom_ui(ui),
        )
        .inner;
    if let Some(rect) = rendered.rect(arrow_id) {
        let color = if ui.is_enabled() {
            theme::ui_palette(ui).text_secondary
        } else {
            theme::ui_palette(ui).text_tertiary
        };
        icons::paint(ui.painter(), rect, icons::Icon::ChevronDown, color);
    }
    rendered.response
}

fn encoding_choice_label(encoding: Option<Encoding>) -> &'static str {
    encoding.map_or("Auto (BOM or UTF-8)", Encoding::label)
}

fn format_menu(
    ui: &mut egui::Ui,
    document: Option<&Document>,
    applied: (DelimiterMode, HeaderMode, Option<Encoding>),
    draft: &mut Option<(DelimiterMode, HeaderMode, Option<Encoding>)>,
    compact: bool,
    width: f32,
) -> Option<Action> {
    let (applied_delimiter, applied_header, applied_encoding) = applied;
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
    // Show what Quarry is actually using, not the word "Auto".
    let label = document.map_or_else(
        || "Format".to_owned(),
        |document| {
            let dialect = document.session.dialect;
            let delimiter = if applied_delimiter == DelimiterMode::Auto {
                detected_delimiter_label(dialect.delimiter)
            } else {
                applied_delimiter.label()
            };
            let header = match applied_header {
                HeaderMode::Auto if dialect.has_header => "Header row",
                HeaderMode::Auto => "No header",
                mode => compact_header_mode_label(mode),
            };
            if compact {
                delimiter.to_owned()
            } else {
                format!("{delimiter} · {header}")
            }
        },
    );
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
    if !ui.is_enabled() {
        return None;
    }
    let mut popup_open = draft.is_some();
    if response.clicked() {
        popup_open = !popup_open;
        *draft = popup_open.then_some((applied_delimiter, applied_header, applied_encoding));
    }

    // This panel owns its open state; egui memory tracks its nested choosers.
    let chooser_was_open = popup_open && egui::Popup::is_any_open(ui.ctx());
    let mut discard_draft = false;
    let menu = egui::Popup::menu(&response)
        .open_bool(&mut popup_open)
        .close_behavior(if chooser_was_open {
            egui::PopupCloseBehavior::IgnoreClicks
        } else {
            egui::PopupCloseBehavior::CloseOnClickOutside
        })
        .style(egui::style::StyleModifier::new(compact_tool_style))
        .frame(compact_tool_frame(ui.ctx()))
        .show(|ui| {
            ui.set_width(316.0);
            ui.heading("Format");
            let dialect = document
                .expect("disabled Format control cannot open")
                .session
                .dialect;
            ui.weak(format!(
                "Applied: {} · {}",
                applied_delimiter.label(),
                compact_header_mode_label(applied_header)
            ));
            ui.weak(format!(
                "Detected: {} · {}",
                detected_delimiter_label(dialect.delimiter),
                if dialect.has_header {
                    "Header row"
                } else {
                    "No header"
                }
            ));
            ui.weak(format!(
                "Encoding: {}",
                document.unwrap().session.encoding().label()
            ));
            ui.separator();

            let (draft_delimiter, draft_header, draft_encoding) =
                draft.get_or_insert((applied_delimiter, applied_header, applied_encoding));
            egui::Grid::new("quarry-format-fields")
                .spacing(egui::vec2(14.0, 8.0))
                .show(ui, |ui| {
                    let label = ui.label("Delimiter");
                    egui::ComboBox::from_id_salt("quarry-format-delimiter")
                        .selected_text(draft_delimiter.label())
                        .width(238.0)
                        .popup_style(egui::style::StyleModifier::new(compact_tool_style))
                        .show_ui(ui, |ui| {
                            for mode in DelimiterMode::ALL {
                                if ui
                                    .selectable_value(draft_delimiter, mode, mode.label())
                                    .clicked()
                                {
                                    ui.close();
                                }
                            }
                        })
                        .response
                        .labelled_by(label.id);
                    ui.end_row();
                    let label = ui.label("Header");
                    egui::ComboBox::from_id_salt("quarry-format-header")
                        .selected_text(header_mode_label(*draft_header))
                        .width(238.0)
                        .popup_style(egui::style::StyleModifier::new(compact_tool_style))
                        .show_ui(ui, |ui| {
                            for mode in
                                [HeaderMode::Auto, HeaderMode::FirstRow, HeaderMode::NoHeader]
                            {
                                if ui
                                    .selectable_value(draft_header, mode, header_mode_label(mode))
                                    .clicked()
                                {
                                    ui.close();
                                }
                            }
                        })
                        .response
                        .labelled_by(label.id);
                    ui.end_row();
                    let label = ui.label("Encoding");
                    egui::ComboBox::from_id_salt("quarry-format-encoding")
                        .selected_text(encoding_choice_label(*draft_encoding))
                        .width(238.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(draft_encoding, None, "Auto (BOM or UTF-8)");
                            for encoding in Encoding::ALL {
                                ui.selectable_value(
                                    draft_encoding,
                                    Some(encoding),
                                    encoding.label(),
                                );
                            }
                        })
                        .response
                        .labelled_by(label.id);
                    ui.end_row();
                });
            if chooser_was_open {
                ui.input_mut(|input| {
                    input.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
                });
            }
            ui.separator();

            let selected = (*draft_delimiter, *draft_header, *draft_encoding);
            let changed = selected != (applied_delimiter, applied_header, applied_encoding);
            let dirty = document.is_some_and(Document::is_dirty);
            let operation_active = document.is_some_and(|document| {
                document.save_job.is_some()
                    || document.export_job.is_some()
                    || document.structural_job.is_some()
            });
            let mut action = None;
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let reopen = ui
                    .add_enabled(
                        changed && !dirty && !operation_active,
                        theme::primary_button("Reopen with Changes"),
                    )
                    .on_disabled_hover_text(if !changed {
                        "Choose a different delimiter, header mode, or encoding first."
                    } else if dirty {
                        "Discard or save your changes before reopening the file."
                    } else {
                        "Cancel the active file operation and wait for it to finish first."
                    });
                if reopen.clicked() {
                    action = Some(Action::ReopenWithFormat(selected.0, selected.1, selected.2));
                    discard_draft = true;
                    ui.close();
                }
                if ui.button("Cancel").clicked() {
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

fn document_menu(
    ui: &mut egui::Ui,
    document: Option<&Document>,
    write_slot_available: bool,
    width: f32,
) -> Option<Action> {
    let commands = FileCommands::new(document, write_slot_available);
    let document_open = document.is_some();
    let dirty = document.is_some_and(Document::is_dirty);
    let save_ready = commands.save;
    let save_as_ready = commands.save_as;
    let join_reserved = document.is_some_and(Document::join_reserved);
    let discard_ready = commands.discard;
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
    // The tab names the file; this menu holds file commands.
    let response = menu_button_with_arrow(
        ui,
        "document-menu-arrow",
        egui::Button::new(RichText::new("File").atom_shrink(true)),
        width,
    );
    let mut popup = egui::Popup::menu(&response);
    if !ui.is_enabled() {
        popup = popup.open(false);
    }
    let menu = popup.show(|ui| {
        ui.set_min_width(190.0);
        let mut action = None;
        let open = ui
            .button("Open…")
            .on_hover_text("Open files in new tabs (⌘O)");
        if open.clicked() {
            action = Some(Action::Choose);
        }
        ui.menu_button("Open with Encoding…", |ui| {
            for encoding in Encoding::ALL {
                if ui.button(encoding.label()).clicked() {
                    action = Some(Action::ChooseWithEncoding(encoding));
                    ui.close();
                }
            }
        });
        if ui
            .add_enabled(commands.combine, egui::Button::new("Combine Files…"))
            .on_disabled_hover_text(
                "Two file operations are already running. Wait for one to finish or cancel it.",
            )
            .clicked()
        {
            action = Some(Action::CombineFiles);
        }
        if ui.button("Join Files…").clicked() {
            action = Some(Action::JoinFiles);
        }
        if ui
            .add_enabled(document_open, egui::Button::new("Close Tab"))
            .on_hover_text("Close this document (⌘W)")
            .clicked()
        {
            action = Some(Action::CloseTab);
        }
        let reload = ui
            .add_enabled(commands.reload, egui::Button::new("Reload from Disk"))
            .on_disabled_hover_text(if !document_open {
                "Open a file first."
            } else if join_reserved {
                join_files::INPUT_RESERVED
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
            .add_enabled(save_as_ready, egui::Button::new("Save As…"))
            .on_hover_text("Save a new copy in the current column order, including hidden columns")
            .on_disabled_hover_text(if !document_open {
                "Open a file before using Save As."
            } else if !dirty && !document.is_some_and(Document::has_reordered_columns) {
                "Make a change or reorder columns before using Save As."
            } else {
                "Wait for the active file operation to finish."
            });
        if save_as.clicked() {
            action = Some(Action::ChooseSaveAs);
        }
        if ui.add_enabled(
            commands.save_as_utf8,
            egui::Button::new("Save As UTF-8…"),
        ).on_hover_text("Create a UTF-8 copy with the current edits and column order. The source stays unchanged.")
            .clicked()
        {
            action = Some(Action::ChooseSaveAsUtf8);
        }
        let discard = ui
            .add_enabled(discard_ready, egui::Button::new("Discard Changes"))
            .on_disabled_hover_text(if !document_open {
                "Open a file first."
            } else if !dirty {
                "There are no changes to discard."
            } else if join_reserved {
                join_files::INPUT_RESERVED
            } else {
                "Wait for the active file operation to finish."
            });
        if discard.clicked() {
            action = Some(Action::DiscardChanges);
        }
        ui.separator();
        if ui
            .add_enabled(
                commands.temporary_storage,
                egui::Button::new("Temporary storage…"),
            )
            .clicked()
        {
            action = Some(Action::TemporaryStorage);
        }
        ui.separator();
        if ui.add_enabled(commands.inspect, egui::Button::new("Inspect File…")).clicked() {
            action = Some(Action::InspectFile);
        }
        if ui.button("About & Feedback…").clicked() {
            action = Some(Action::About);
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
    let page_up = icons::button(ui, enabled, icons::Icon::ChevronUp, "Page Up", false)
        .on_hover_text("Previous page (Page Up)")
        .clicked();
    let page_down = icons::button(ui, enabled, icons::Icon::ChevronDown, "Page Down", false)
        .on_hover_text("Next page (Page Down)")
        .clicked();
    if page_up {
        Some(Action::PageUp)
    } else if page_down {
        Some(Action::PageDown)
    } else {
        None
    }
}

const EMPTY_STATE_SIZE: egui::Vec2 = egui::vec2(520.0, 212.0);

/// Drop-zone fill and stroke, highlighted while a file hovers over the window.
fn empty_state_colors(palette: &theme::Palette, hovered: bool) -> (Color32, egui::Stroke) {
    if hovered {
        (
            palette.selection,
            egui::Stroke::new(2.0_f32, palette.selection_strong),
        )
    } else {
        (
            palette.zebra,
            egui::Stroke::new(1.0_f32, palette.control_border),
        )
    }
}

fn show_empty_state(ui: &mut egui::Ui, local_file_hovered: bool) -> Option<Action> {
    let mut action = None;
    let palette = theme::ui_palette(ui);
    let (fill, stroke) = empty_state_colors(palette, local_file_hovered);

    let available = ui.available_rect_before_wrap();
    let drop_rect =
        egui::Rect::from_center_size(available.center(), EMPTY_STATE_SIZE.min(available.size()));
    ui.expand_to_include_rect(drop_rect);
    ui.painter()
        .rect(drop_rect, 14.0, fill, stroke, egui::StrokeKind::Inside);
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(drop_rect.shrink(28.0))
            .layout(egui::Layout::top_down(Align::Center)),
        |ui| {
            let (icon_rect, _) =
                ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::hover());
            icons::paint(
                ui.painter(),
                icon_rect,
                icons::Icon::Document,
                palette.text_tertiary,
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new("Drop a delimited file here, or")
                    .size(17.0)
                    .color(palette.text),
            );
            ui.label(
                RichText::new(
                    "CSV, TSV, pipe, or semicolon. First rows appear while the rest indexes.",
                )
                .color(palette.text_secondary),
            );
            ui.add_space(10.0);
            let buttons = egui::vec2(332.0, 28.0);
            ui.allocate_ui_with_layout(buttons, Layout::left_to_right(Align::Center), |ui| {
                ui.spacing_mut().interact_size.y = 28.0;
                if ui.add(theme::primary_button("Open…")).clicked() {
                    action = Some(Action::Choose);
                }
                if ui.button("Combine Files…").clicked() {
                    action = Some(Action::CombineFiles);
                }
                if ui.button("Join Files…").clicked() {
                    action = Some(Action::JoinFiles);
                }
            });
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

fn filter_upper_bound_input_id(rule_index: usize) -> egui::Id {
    egui::Id::new(("quarry-filter-upper-bound", rule_index))
}

fn is_filter_text_input(focused: egui::Id, filter_rule_count: usize) -> bool {
    (0..filter_rule_count).any(|index| {
        focused == filter_column_input_id(index)
            || focused == filter_value_input_id(index)
            || focused == filter_upper_bound_input_id(index)
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

fn edit_history_shortcut(ctx: &egui::Context, document: &Document) -> Option<Action> {
    // Text inputs own their typing history; a focused grid button still allows document Undo.
    if ctx
        .memory(|memory| memory.focused())
        .is_some_and(|id| egui::TextEdit::load_state(ctx, id).is_some())
    {
        return None;
    }
    ctx.input_mut(|input| {
        if document.can_redo()
            && (input.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::Z,
            ) || (!cfg!(target_os = "macos")
                && input.consume_key(egui::Modifiers::CTRL, egui::Key::Y)))
        {
            Some(Action::Redo)
        } else if document.can_undo()
            && !input.modifiers.shift
            && input.consume_key(egui::Modifiers::COMMAND, egui::Key::Z)
        {
            Some(Action::Undo)
        } else {
            None
        }
    })
}

fn reset_editor_undo(ctx: &egui::Context, id: egui::Id) {
    if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
        state.clear_undoer();
        state.store(ctx, id);
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

fn tool_dialog_escape_requested(ctx: &egui::Context, window_id: &str) -> bool {
    let layer = egui::LayerId::new(egui::Order::Middle, egui::Id::new(window_id));
    !egui::Popup::is_any_open(ctx)
        && ctx.memory(|memory| {
            memory.allows_interaction(layer)
                && memory
                    .areas()
                    .top_layer_id(egui::Order::Middle)
                    .filter(|top| memory.areas().is_visible(top))
                    .is_none_or(|top| top == layer)
        })
        && ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
}

fn show_column_manager(
    ctx: &egui::Context,
    open: &mut bool,
    search: &mut String,
    document: &Document,
) -> Option<ColumnCommand> {
    if !*open {
        return None;
    }
    if tool_dialog_escape_requested(ctx, "quarry-column-manager") {
        ctx.memory_mut(|memory| memory.surrender_focus(egui::Id::new(COLUMN_SEARCH_INPUT_ID)));
        *open = false;
        return None;
    }
    let mut command = None;
    let mut close_requested = false;
    egui::Window::new("Columns")
        .id(egui::Id::new("quarry-column-manager"))
        .title_bar(false)
        .pivot(compact_tool_placement(ctx, "quarry-column-manager", 390.0).0)
        .fixed_pos(compact_tool_placement(ctx, "quarry-column-manager", 390.0).1)
        .min_width(360.0)
        .max_width(360.0)
        .resizable(false)
        .frame(compact_tool_frame(ctx))
        .show(ctx, |ui| {
            compact_tool_style(ui.style_mut());
            ui.set_width(360.0);
            ui.horizontal(|ui| {
                ui.heading("Columns");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    close_requested = compact_tool_close(ui);
                });
            });
            let search_response = ui.add_sized(
                [ui.available_width(), 28.0],
                egui::TextEdit::singleline(search)
                    .id(egui::Id::new(COLUMN_SEARCH_INPUT_ID))
                    .hint_text("Search columns"),
            );
            let _ = ui.ctx().accesskit_node_builder(search_response.id, |node| {
                node.set_label("Search columns");
            });
            ui.horizontal(|ui| {
                ui.weak(format!(
                    "{} shown of {} {}",
                    document.columns.shown_count(),
                    document.known_width(),
                    if document.width_confirmed() {
                        "confirmed"
                    } else {
                        "known"
                    }
                ));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if column_action_button(ui, "Reset", true, "Reset columns".into()) {
                        command = Some(ColumnCommand::Reset);
                    }
                });
            });
            ui.separator();
            if document.known_width() > document.total_columns {
                ui.weak(format!(
                    "First {} columns available in this view",
                    document.total_columns
                ));
            }
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
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let row_height = 28.0;
                egui::ScrollArea::vertical()
                    .id_salt(("quarry-column-manager-list", &document.logical_path))
                    .auto_shrink([false, false])
                    .max_height(
                        (ctx.content_rect().bottom()
                            - compact_tool_anchor(ctx, "quarry-column-manager").y
                            - 222.0)
                            .clamp(96.0, 336.0),
                    )
                    .show_rows(ui, row_height, filtered_positions.len(), |ui, rows| {
                        if filtered_positions.is_empty() {
                            ui.weak(if document.total_columns == 0 {
                                "No columns"
                            } else {
                                "No matching columns"
                            });
                        }
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
                                let name = document.column_name(column);
                                let mut checkbox_right = 0.0;
                                let row = egui::Frame::new()
                                    .fill(if selected {
                                        ui.visuals().selection.bg_fill
                                    } else if filtered_position % 2 == 0 {
                                        theme::ui_palette(ui).canvas
                                    } else {
                                        theme::ui_palette(ui).zebra
                                    })
                                    .inner_margin(egui::Margin::symmetric(4, 0))
                                    .show(ui, |ui| {
                                        ui.set_min_width(ui.available_width());
                                        ui.horizontal(|ui| {
                                            let mut shown = !document.columns.hidden[column];
                                            let checkbox = ui
                                                .add(egui::Checkbox::without_text(&mut shown))
                                                .on_hover_text(if shown {
                                                    "Hide column"
                                                } else {
                                                    "Show column"
                                                });
                                            let _ = ui.ctx().accesskit_node_builder(
                                                checkbox.id,
                                                |node| {
                                                    node.set_label(format!(
                                                        "{}  {name}",
                                                        column.saturating_add(1)
                                                    ));
                                                },
                                            );
                                            checkbox_right = checkbox.rect.right();
                                            if checkbox.changed() {
                                                command =
                                                    Some(ColumnCommand::SetShown { column, shown });
                                            }
                                            let (handle, _) = ui.allocate_exact_size(
                                                egui::vec2(12.0, 28.0),
                                                egui::Sense::hover(),
                                            );
                                            for x in [-2.5, 2.5] {
                                                for y in [-5.0, 0.0, 5.0] {
                                                    ui.painter().circle_filled(
                                                        handle.center() + egui::vec2(x, y),
                                                        1.0,
                                                        ui.visuals().weak_text_color(),
                                                    );
                                                }
                                            }
                                            ui.add_sized(
                                                [28.0, 28.0],
                                                egui::Label::new(
                                                    RichText::new(
                                                        column.saturating_add(1).to_string(),
                                                    )
                                                    .monospace()
                                                    .weak(),
                                                ),
                                            );
                                            ui.allocate_ui_with_layout(
                                                egui::vec2(ui.available_width(), 28.0),
                                                Layout::left_to_right(Align::Center),
                                                |ui| {
                                                    ui.add(egui::Label::new(&name).truncate())
                                                        .on_hover_text(&name);
                                                },
                                            );
                                        });
                                    })
                                    .response;
                                let drag_target = ui
                                    .interact(
                                        egui::Rect::from_min_max(
                                            egui::pos2(checkbox_right, row.rect.top()),
                                            row.rect.max,
                                        ),
                                        ui.id().with("column-drag-target"),
                                        egui::Sense::click_and_drag(),
                                    )
                                    .on_hover_cursor(egui::CursorIcon::Grab)
                                    .on_hover_text(&name);
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
                                        egui::Stroke::new(
                                            2.0_f32,
                                            ui.visuals().selection.stroke.color,
                                        ),
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
            });
            ui.weak("Drag to reorder · Uncheck to hide");
            ui.separator();
            ui.horizontal(|ui| {
                if ui
                    .button("Auto-fit columns")
                    .on_hover_text("Fit every shown column to its header and loaded cell values")
                    .clicked()
                {
                    command = Some(ColumnCommand::AutoFit);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Done").clicked() {
                        close_requested = true;
                    }
                });
            });
        });
    if close_requested {
        ctx.memory_mut(|memory| memory.surrender_focus(egui::Id::new(COLUMN_SEARCH_INPUT_ID)));
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

/// Name the comparison mode independently of its selected format or shuffle seed.
fn sort_mode_label(mode: SortMode) -> &'static str {
    match mode {
        SortMode::Text => "Text",
        SortMode::Natural => "Natural",
        SortMode::Number => "Number",
        SortMode::DateTime { .. } => "Date / time",
        SortMode::CharacterCount => "Character count",
        SortMode::WordCount => "Word count",
        SortMode::Shuffle { .. } => "Shuffle",
        SortMode::Reverse => "Reverse",
    }
}

/// Format a bounded date/time sample hint without ever auto-selecting a format.
fn format_date_suggestion_hint(suggestion: &DateFormatSuggestion) -> String {
    let row_noun = if suggestion.sampled_rows == 1 {
        "row"
    } else {
        "rows"
    };
    let value_noun = if suggestion.sampled_nonblank_values == 1 {
        "value"
    } else {
        "values"
    };
    let scope = if suggestion.truncated {
        format!("first {} sampled {row_noun}", suggestion.sampled_rows)
    } else {
        format!("{} sampled {row_noun}", suggestion.sampled_rows)
    };
    if suggestion.sampled_nonblank_values == 0 {
        return format!(
            "Sample hint ({scope}): no nonblank values found. Choose a format explicitly."
        );
    }
    if let Some(format) = suggestion.unambiguous_format() {
        return format!(
            "Sample hint ({} {value_noun} in {scope}): matches {}. Choose explicitly; full file is checked when sorting.",
            suggestion.sampled_nonblank_values,
            format.label()
        );
    }
    if suggestion.is_ambiguous() {
        let labels = suggestion
            .matching_formats
            .iter()
            .map(|format| format.label())
            .collect::<Vec<_>>()
            .join(", ");
        return format!(
            "Sample hint ({} {value_noun} in {scope}): ambiguous — matches {labels}. Choose explicitly; full file is checked when sorting.",
            suggestion.sampled_nonblank_values
        );
    }
    format!(
        "Sample hint ({} {value_noun} in {scope}): no single date/time format matched all sampled values.",
        suggestion.sampled_nonblank_values
    )
}

/// Shows cleanup controls and a bounded sample without starting the write job.
fn show_cleanup_dialog(
    ctx: &egui::Context,
    dialog: &mut StructuralDialog,
    document: &Document,
) -> Option<StructuralDialogAction> {
    let mut action = None;
    let modal = egui::Modal::new(egui::Id::new("quarry-cleanup-dialog"))
        .frame(tool_dialog_frame(ctx))
        .show(ctx, |ui| {
            tool_dialog_style(ui.style_mut());
            ui.set_width(480.0);
            ui.heading("Clean Up Columns");
            ui.label("Applies to all data rows in the selected file columns.");
            let mut targets = dialog.columns.iter().take(8)
                .map(|c| format!("{}: {}", c + 1, field_preview(document.column_name(*c).as_bytes(), 80)))
                .collect::<Vec<_>>().join(", ");
            if dialog.columns.len() > 8 {
                targets.push_str(&format!(" … and {} more", dialog.columns.len() - 8));
            }
            ui.add(egui::Label::new(format!("Selected columns: {targets}")).truncate()).on_hover_text(&targets);
            ui.separator();
            theme::section_label(ui, "Operation");
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                for (request, name) in [
                    (StructuralRequest::CleanupTrim, "Trim leading/trailing whitespace"),
                    (StructuralRequest::CleanupCollapseSpaces, "Collapse repeated spaces"),
                    (StructuralRequest::CleanupUppercase, "Uppercase"),
                    (StructuralRequest::CleanupLowercase, "Lowercase"),
                    (StructuralRequest::CleanupFill, "Fill Empty Cells"),
                ] {
                    ui.radio_value(&mut dialog.request, request, name);
                }
            });
            if dialog.request == StructuralRequest::CleanupFill {
                let label = ui.label("Fill value");
                let input = ui.add(egui::TextEdit::multiline(&mut dialog.separator)
                    .id_salt("cleanup-fill-value").desired_rows(2).desired_width(f32::INFINITY))
                    .labelled_by(label.id);
                let _ = ctx.accesskit_node_builder(input.id, |node| node.set_label("Fill value"));
                ui.weak("Only existing empty fields are filled. Spaces and missing fields stay unchanged.");
            } else if dialog.request == StructuralRequest::CleanupTrim {
                ui.weak("Remove whitespace at the beginning and end of each cell. Keep spaces, tabs and line breaks inside.");
            } else if dialog.request == StructuralRequest::CleanupCollapseSpaces {
                ui.weak("Replace two or more ordinary spaces with one space. Keep tabs, line breaks and special spaces.");
            } else {
                ui.weak("Unicode case conversion. No language-specific rules or transliteration.");
            }
            ui.separator();
            ui.strong("Sample preview");
            ui.weak("A small sample of loaded rows, not a full-file scan. Other rows may also change.");
            let cleanup = dialog.cleanup();
            let preview = cleanup_preview(document, &cleanup);
            egui::ScrollArea::vertical().id_salt("cleanup-preview").max_height(180.0).show(ui, |ui| {
                for line in &preview.lines { ui.label(line); }
                if preview.lines.is_empty() { ui.label("No loaded fields to preview."); }
                if preview.skipped { ui.weak("Some large values are not previewed; Apply still checks the full values."); }
            });
            let reason = document.structural_edit_disabled_reason().map(str::to_owned)
                .or_else(|| cleanup.validate().err().map(|e| e.to_string()))
                .or(preview.error);
            if let Some(error) = &reason { ui.colored_label(ui.visuals().error_fg_color, error); }
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() { action = Some(StructuralDialogAction::Cancel); }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add_enabled(reason.is_none(), theme::primary_button("Apply")).clicked() {
                        action = Some(StructuralDialogAction::Apply);
                    }
                });
            });
        });
    if modal.should_close() {
        action = Some(StructuralDialogAction::Cancel);
    }
    action
}

struct CleanupPreview {
    lines: Vec<String>,
    skipped: bool,
    error: Option<String>,
}

fn cleanup_preview(document: &Document, cleanup: &ColumnCleanup) -> CleanupPreview {
    let mut result = CleanupPreview {
        lines: Vec::new(),
        skipped: false,
        error: None,
    };
    let mut budget = 32 * 1024usize;
    for (offset, row) in document.buffered_rows.iter().enumerate().take(20) {
        let physical = document.buffer_start + offset as u64;
        if physical < document.data_start {
            continue;
        }
        let data_row = physical - document.data_start + 1;
        for &column in &cleanup.columns {
            if result.lines.len() >= 8 {
                return result;
            }
            let Some(source) = row.fields.get(column) else {
                continue;
            };
            let value = document
                .cell_edits
                .get(&(physical, column))
                .map_or(source.as_slice(), |value| value.as_slice());
            let fill_len = if let CleanupOperation::FillEmpty(v) = &cleanup.operation {
                v.len()
            } else {
                0
            };
            if value.len() > 4096
                || fill_len > 4096
                || value.len().saturating_add(fill_len) > budget
            {
                result.skipped = true;
                continue;
            }
            budget -= value.len().saturating_add(fill_len);
            match cleanup
                .operation
                .clean_value(value, data_row, column + 1, 64 * 1024 * 1024)
            {
                Ok(next) => {
                    let after = next.as_deref().unwrap_or(value);
                    result.lines.push(format!(
                        "Row {data_row}, column {}: before \"{}\"; after \"{}\"",
                        column + 1,
                        field_preview(value, 100),
                        field_preview(after, 100)
                    ));
                }
                Err(error) => {
                    result.error = Some(error.to_string());
                    return result;
                }
            }
        }
    }
    result
}

/// Pick a physical column with virtualized rows so wide files do not build a huge popup.
fn show_sort_column(ui: &mut egui::Ui, document: &Document, column: &mut usize, index: usize) {
    let label = if index == 0 {
        "Sort by".to_owned()
    } else {
        format!("Then by (level {})", index + 1)
    };
    ui.weak(&label);
    let selected = format!(
        "{}: {}",
        column.saturating_add(1),
        document.column_name(*column)
    );
    let response = egui::ComboBox::from_id_salt(("sort-column", index))
        .width(ui.available_width())
        .selected_text(selected)
        .popup_style(egui::style::StyleModifier::new(tool_dialog_style))
        .show_ui(ui, |ui| {
            egui::ScrollArea::vertical().max_height(240.0).show_rows(
                ui,
                ui.text_style_height(&egui::TextStyle::Body),
                document.total_columns,
                |ui, rows| {
                    for candidate in rows {
                        let name =
                            format!("{}: {}", candidate + 1, document.column_name(candidate));
                        if ui.selectable_value(column, candidate, name).clicked() {
                            ui.close();
                        }
                    }
                },
            );
        })
        .response;
    let _ = ui
        .ctx()
        .accesskit_node_builder(response.id, |node| node.set_label(label));
}

/// Give repeated level actions distinct accessible names while keeping the visual labels short.
fn sort_level_action(ui: &mut egui::Ui, label: &str, accessible: String, enabled: bool) -> bool {
    let response = ui
        .add_enabled(enabled, egui::Button::new(label))
        .on_hover_text(&accessible);
    let _ = ui
        .ctx()
        .accesskit_node_builder(response.id, |node| node.set_label(accessible));
    response.clicked()
}

/// Draw independent type, direction, case, and date-format options for a tie-breaker.
fn show_additional_sort_level(
    ui: &mut egui::Ui,
    document: &Document,
    key: &mut SortSpec,
    index: usize,
    date_suggestions: &mut BTreeMap<usize, Option<DateFormatSuggestion>>,
) {
    ui.push_id(("sort-level", index), |ui| {
        show_sort_column(ui, document, &mut key.column, index);
        let mode_label = ui.weak("Sort as");
        let mode_response = egui::ComboBox::from_id_salt("mode")
            .width(ui.available_width())
            .selected_text(sort_mode_label(key.mode))
            .popup_style(egui::style::StyleModifier::new(tool_dialog_style))
            .show_ui(ui, |ui| {
                for mode in [
                    SortMode::Text,
                    SortMode::Natural,
                    SortMode::Number,
                    SortMode::DateTime {
                        format: DateTimeFormat::YearMonthDay,
                    },
                    SortMode::CharacterCount,
                    SortMode::WordCount,
                ] {
                    let selected = key.mode == mode
                        || matches!(
                            (key.mode, mode),
                            (SortMode::DateTime { .. }, SortMode::DateTime { .. })
                        );
                    if ui
                        .selectable_label(selected, sort_mode_label(mode))
                        .clicked()
                    {
                        if !selected {
                            key.mode = mode;
                        }
                        ui.close();
                    }
                }
            })
            .response
            .labelled_by(mode_label.id);
        let _ = ui.ctx().accesskit_node_builder(mode_response.id, |node| {
            node.set_label(format!("Sort as (level {})", index + 1))
        });
        if let SortMode::DateTime { format } = &mut key.mode {
            let format_label = ui.weak("Date / time format");
            let format_response = egui::ComboBox::from_id_salt("date-format")
                .width(ui.available_width())
                .selected_text(format.label())
                .popup_style(egui::style::StyleModifier::new(tool_dialog_style))
                .show_ui(ui, |ui| {
                    for choice in DateTimeFormat::ALL {
                        if ui
                            .selectable_value(format, choice, choice.label())
                            .clicked()
                        {
                            ui.close();
                        }
                    }
                })
                .response
                .labelled_by(format_label.id);
            let _ = ui.ctx().accesskit_node_builder(format_response.id, |node| {
                node.set_label(format!("Date / time format (level {})", index + 1))
            });
            ui.small(format.description());
            let suggestion = date_suggestions
                .entry(key.column)
                .or_insert_with(|| document.suggest_date_time_formats(key.column));
            if let Some(suggestion) = suggestion.as_ref() {
                ui.small(format_date_suggestion_hint(suggestion));
            }
        }
        let directions = [SortDirection::Ascending, SortDirection::Descending]
            .map(|direction| (direction, sort_direction_label(key.mode, direction)));
        for ((_, label), response) in
            directions
                .iter()
                .zip(theme::segmented(ui, &mut key.direction, &directions))
        {
            let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
                node.set_label(format!("{label} (level {})", index + 1));
            });
        }
        if matches!(key.mode, SortMode::Text | SortMode::Natural) {
            let mut sensitive = key.case_sensitivity == CaseSensitivity::Sensitive;
            let response = ui.checkbox(&mut sensitive, "Match case");
            let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
                node.set_label(format!("Match case (level {})", index + 1));
            });
            key.case_sensitivity = case_sensitivity(sensitive);
        }
    });
}

/// Describe ties for column sorts and whole-row operations separately.
fn sort_ordering_description(mode: SortMode) -> &'static str {
    if mode.uses_column() {
        "Rows equal at every level keep their original order (stable sort). Missing values sort as empty cells."
    } else {
        "All cells stay with their row."
    }
}

/// Describe comparison semantics without allocating or inspecting document values.
fn sort_value_description(mode: SortMode, match_case: bool) -> &'static str {
    match mode {
        SortMode::Text => {
            if match_case {
                "Uppercase and lowercase letters sort separately."
            } else {
                "Uppercase and lowercase letters sort together."
            }
        }
        SortMode::Natural => {
            if match_case {
                "ASCII digit runs compare numerically (for example v1.9 before v1.10). Uppercase and lowercase letters sort separately. Leading zeros tie-break in original row order."
            } else {
                "ASCII digit runs compare numerically (for example v1.9 before v1.10). Uppercase and lowercase letters sort together. Leading zeros tie-break in original row order."
            }
        }
        SortMode::Number => {
            "Signed numbers, decimals and scientific notation. Blank cells sort first ascending, last descending. Other values stop the sort."
        }
        SortMode::DateTime { format } => format.description(),
        SortMode::CharacterCount => {
            "Count Unicode characters, including spaces. Combining marks count separately. Invalid UTF-8 stops the sort."
        }
        SortMode::WordCount => {
            "Count words separated by Unicode whitespace. Invalid UTF-8 stops the sort."
        }
        SortMode::Shuffle { .. } => {
            "Randomize the entire data row order. The selected column is ignored."
        }
        SortMode::Reverse => {
            "Reverse the entire current data row order. The selected column is ignored."
        }
    }
}

/// Keep actions and disk allowance outside the scrollable operation settings.
fn show_structural_dialog(
    ctx: &egui::Context,
    dialog: &mut StructuralDialog,
    sort_match_case: &mut bool,
    document: &Document,
) -> Option<StructuralDialogAction> {
    if dialog.request.is_cleanup() {
        return show_cleanup_dialog(ctx, dialog, document);
    }
    let mut action = None;
    let mut sort_description = None;
    let mut disk_description = None;
    let title = match dialog.request {
        StructuralRequest::InsertRows => "Insert Rows",
        StructuralRequest::InsertColumns => "Insert Columns",
        StructuralRequest::Split => "Split Columns",
        StructuralRequest::Combine => "Combine Columns",
        StructuralRequest::Move => "Move Columns",
        StructuralRequest::Sort => "Sort Rows",
        StructuralRequest::Duplicates => "Find Duplicates",
        _ => unreachable!("cleanup rendered separately"),
    };
    let modal = egui::Modal::new(egui::Id::new("quarry-structural-dialog"))
        .frame(tool_dialog_frame(ctx))
        .show(ctx, |ui| {
        tool_dialog_style(ui.style_mut());
        ui.set_width(480.0);
        ui.heading(title);
        ui.separator();
        let (valid, field_has_focus, disabled_reason) = egui::ScrollArea::vertical()
            .id_salt("quarry-structural-dialog-body")
            .max_height((ctx.content_rect().height() - 220.0).clamp(96.0, 360.0))
            .show(ui, |ui| {
        let columns = dialog
            .columns
            .iter()
            .map(|column| {
                let name = document.column_name(*column);
                if name.is_empty() {
                    (column.saturating_add(1)).to_string()
                } else {
                    format!("{}: {name}", column.saturating_add(1))
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let selected = format!(
            "Selected column{}: {columns}",
            if dialog.columns.len() == 1 { "" } else { "s" }
        );
        if !matches!(
            dialog.request,
            StructuralRequest::InsertRows
                | StructuralRequest::InsertColumns
                | StructuralRequest::Sort
                | StructuralRequest::Duplicates
        ) {
            ui.add(egui::Label::new(&selected).truncate()).on_hover_text(&selected);
        }
        ui.add_space(6.0);
        match dialog.request {
            StructuralRequest::CleanupTrim | StructuralRequest::CleanupCollapseSpaces | StructuralRequest::CleanupUppercase | StructuralRequest::CleanupLowercase | StructuralRequest::CleanupFill => unreachable!("cleanup rendered separately"),
            StructuralRequest::InsertRows | StructuralRequest::InsertColumns => {
                let rows = dialog.request == StructuralRequest::InsertRows;
                let position_label = if rows { "Insert before data row" } else { "Insert before file column" };
                let label = ui.label(position_label);
                let position = ui.add_sized([ui.available_width(), 26.0],
                    egui::TextEdit::singleline(&mut dialog.position)
                        .id(egui::Id::new(STRUCTURAL_POSITION_INPUT_ID))).labelled_by(label.id);
                let _ = ui.ctx().accesskit_node_builder(position.id, |node| node.set_label(position_label));
                let count_label = if rows { "Number of rows" } else { "Number of columns" };
                let label = ui.label(count_label);
                let count = ui.add_sized([ui.available_width(), 26.0],
                    egui::TextEdit::singleline(&mut dialog.count)
                        .id(egui::Id::new("quarry-insertion-count"))).labelled_by(label.id);
                let _ = ui.ctx().accesskit_node_builder(count.id, |node| node.set_label(count_label));
                let mut focused = position.has_focus() || count.has_focus();
                if !rows && document.session.dialect.has_header {
                    let label = ui.label("First new column name (optional)");
                    let name = ui.add_sized([ui.available_width(), 26.0],
                        egui::TextEdit::singleline(&mut dialog.separator)
                            .id(egui::Id::new("quarry-insertion-header"))).labelled_by(label.id);
                    let _ = ui.ctx().accesskit_node_builder(name.id, |node| node.set_label("First new column name (optional)"));
                    focused |= name.has_focus();
                    ui.small("Additional new column names start blank and can be renamed in the grid.");
                }
                let size = if rows { document.storage_record_count().saturating_sub(document.data_start) } else { document.total_columns as u64 };
                ui.small(format!("Positions are 1–{}. Use {} to add at the end.", size.saturating_add(1), size.saturating_add(1)));
                ui.small(if rows {
                    "Blank rows are added in the current data-row order. The header stays fixed."
                } else {
                    "Positions follow numbered file columns, including hidden columns. The full file order is shown after insertion."
                });
                ui.small("New cells are editable. Undo reverses the insertion; Save or Save As keeps it.");
                let reason = document.insertion_disabled_reason(rows).map(str::to_owned).or_else(|| {
                    if rows { document.row_insertion_from_dialog(dialog).map(|_| ()).err() }
                    else { document.column_insertion_from_dialog(dialog).map(|_| ()).err() }
                });
                if let Some(reason) = &reason { ui.colored_label(ui.visuals().error_fg_color, reason); }
                (reason.is_none(), focused, reason)
            }
            StructuralRequest::Duplicates => {
                theme::section_label(ui, "Match");
                ui.horizontal(|ui| {
                    ui.radio_value(&mut dialog.duplicate_all_fields, false, "Selected columns");
                    ui.radio_value(
                        &mut dialog.duplicate_all_fields,
                        true,
                        "All fields (whole row)",
                    );
                });
                if !dialog.duplicate_all_fields && !dialog.columns.is_empty() {
                    ui.add(egui::Label::new(&selected).truncate())
                        .on_hover_text(&selected);
                }
                if dialog.duplicate_all_fields {
                    ui.label("Find rows with identical decoded values across every field.");
                } else {
                    ui.label("Find rows with matching values in the selected columns.");
                }
                ui.checkbox(&mut dialog.match_case, "Match case");
                ui.label("Review the count first. Removal keeps the first occurrence in the current row order.");
                egui::CollapsingHeader::new("Details").id_salt("duplicate-details").show(ui, |ui| {
                    if dialog.duplicate_all_fields {
                        ui.label("Compare every decoded field and exact field count (for example a,b differs from a,b,; quoted and unquoted forms match). Spaces are significant.");
                    } else {
                        ui.label("Compare the values in every selected column. Blank and missing fields match; spaces are significant.");
                    }
                    ui.label(if dialog.match_case {
                        "Uppercase and lowercase letters match separately."
                    } else {
                        "ASCII uppercase and lowercase letters match together. Other bytes match exactly."
                    });
                    ui.label("The header stays fixed. Unsaved cell values are included in the comparison.");
                });
                let reason = (!dialog.duplicate_all_fields && dialog.columns.is_empty())
                    .then(|| "Select at least one numbered column first.".to_owned());
                if let Some(reason) = &reason { ui.colored_label(ui.visuals().error_fg_color, reason); }
                (reason.is_none(), false, reason)
            }
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
                ui.small("Sort by the first level, then use each following level to break ties.");
                let ordering = sort_ordering_description(dialog.sort_mode);
                let description = sort_value_description(dialog.sort_mode, *sort_match_case);
                egui::CollapsingHeader::new("Details").id_salt("sort-details").show(ui, |ui| {
                    ui.label(description);
                    ui.label(ordering);
                    ui.label("The header stays fixed. Unsaved cell values are included.");
                });
                if dialog.sort_mode.uses_column() {
                    show_sort_column(ui, document, &mut dialog.columns[0], 0);
                }
                let label = ui.weak("Sort as");
                let response = egui::ComboBox::from_id_salt("quarry-sort-mode")
                    .width(ui.available_width())
                    .selected_text(sort_mode_label(dialog.sort_mode))
                    .popup_style(egui::style::StyleModifier::new(tool_dialog_style))
                    .show_ui(ui, |ui| {
                        for mode in [SortMode::Text, SortMode::Natural, SortMode::Number,
                            SortMode::DateTime { format: DateTimeFormat::YearMonthDay }, SortMode::CharacterCount,
                            SortMode::WordCount, SortMode::Shuffle { seed: 0 }, SortMode::Reverse] {
                            let selected = dialog.sort_mode == mode || matches!((dialog.sort_mode, mode),
                                (SortMode::DateTime { .. }, SortMode::DateTime { .. }));
                            if ui.selectable_label(selected, sort_mode_label(mode)).clicked() {
                                if !selected { dialog.sort_mode = mode; }
                                ui.close();
                            }
                        }
                    }).response.labelled_by(label.id);
                let _ = ui.ctx().accesskit_node_builder(response.id, |node| node.set_label("Sort as"));
                if let SortMode::DateTime { format } = &mut dialog.sort_mode {
                    let label = ui.weak("Date / time format");
                    let response = egui::ComboBox::from_id_salt("quarry-sort-date-time-format")
                        .width(ui.available_width())
                        .selected_text(format.label())
                        .popup_style(egui::style::StyleModifier::new(tool_dialog_style))
                        .show_ui(ui, |ui| {
                            for choice in DateTimeFormat::ALL {
                                if ui.selectable_value(format, choice, choice.label()).clicked() { ui.close(); }
                            }
                        }).response.labelled_by(label.id);
                    let _ = ui.ctx().accesskit_node_builder(response.id, |node| node.set_label("Date / time format"));
                    ui.small(format.description());
                    let primary_column = dialog.columns[0];
                    let suggestion = dialog
                        .date_format_suggestions
                        .entry(primary_column)
                        .or_insert_with(|| document.suggest_date_time_formats(primary_column));
                    if let Some(suggestion) = suggestion.as_ref() {
                        ui.small(format_date_suggestion_hint(suggestion));
                    }
                    ui.small("Blank cells sort first oldest, last newest. Other invalid values stop the sort. Surrounding ASCII whitespace is ignored.");
                }
                if dialog.sort_mode.uses_column() {
                    theme::section_label(ui, "Direction");
                    let directions = [SortDirection::Ascending, SortDirection::Descending]
                        .map(|direction| (direction, sort_direction_label(dialog.sort_mode, direction)));
                    theme::segmented(ui, &mut dialog.sort_direction, &directions);
                }
                if matches!(dialog.sort_mode, SortMode::Text | SortMode::Natural) {
                    ui.checkbox(sort_match_case, "Match case");
                }
                let values = sort_value_description(dialog.sort_mode, *sort_match_case);
                if dialog.sort_mode.uses_column() {
                    let mut keys = dialog.sort_plan(*sort_match_case).keys;
                    let mut move_level = None;
                    let mut remove_level = None;
                    for index in 0..keys.len() {
                        if index > 0 {
                            ui.separator();
                            show_additional_sort_level(
                                ui,
                                document,
                                &mut keys[index],
                                index,
                                &mut dialog.date_format_suggestions,
                            );
                        }
                        ui.horizontal(|ui| {
                            ui.weak(format!("Level {}", index + 1));
                            if sort_level_action(ui, "Move up", format!("Move level {} up", index + 1), index > 0) {
                                move_level = Some((index, index - 1));
                            }
                            if sort_level_action(ui, "Move down", format!("Move level {} down", index + 1), index + 1 < keys.len()) {
                                move_level = Some((index, index + 1));
                            }
                            if sort_level_action(ui, "Remove", format!("Remove level {}", index + 1), keys.len() > 1) {
                                remove_level = Some(index);
                            }
                        });
                    }
                    if let Some((from, to)) = move_level { keys.swap(from, to); }
                    if let Some(index) = remove_level { keys.remove(index); }
                    let next_column = (0..document.total_columns)
                        .find(|column| !keys.iter().any(|key| key.column == *column));
                    if ui.add_enabled(keys.len() < MAX_SORT_KEYS && next_column.is_some(),
                        egui::Button::new("Add sort level")).clicked() {
                        keys.push(SortSpec {
                            column: next_column.unwrap(), mode: SortMode::Text,
                            direction: SortDirection::Ascending, case_sensitivity: CaseSensitivity::Insensitive,
                        });
                    }
                    dialog.set_sort_keys(keys, sort_match_case);
                }
                let ordering = sort_ordering_description(dialog.sort_mode);
                let sort_plan = dialog.sort_plan(*sort_match_case);
                let sort_disk = document.sort_plan_disk_estimate(&sort_plan);
                let reason = sort_plan.validate().err()
                    .map(|error| error.to_string()).or_else(|| sort_disk.is_none().then(|| {
                    "Wait for indexing to finish so Quarry can calculate temporary disk space."
                        .to_owned()
                }));
                if let Some(reason) = &reason { ui.colored_label(ui.visuals().error_fg_color, reason); }
                let disk = sort_disk.map_or_else(
                    || reason.clone().unwrap_or_default(),
                    |bytes| format!(
                        "Conservative temporary disk allowance: {}.",
                        format_bytes(bytes)
                    ),
                );
                disk_description = Some(disk.clone());
                sort_description = Some(format!(
                    "{values} {ordering} The header stays fixed. {disk}"
                ));
                (reason.is_none(), false, reason)
            }
        }
        }).inner;
        if let Some(disk) = disk_description { ui.label(disk); }
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                action = Some(StructuralDialogAction::Cancel);
            }
            let submit_label = match dialog.request {
                StructuralRequest::InsertRows | StructuralRequest::InsertColumns => "Insert",
                StructuralRequest::Move => "Move",
                StructuralRequest::Sort => "Sort",
                StructuralRequest::Duplicates => "Find duplicates",
                StructuralRequest::Split | StructuralRequest::Combine => "OK",
                _ => unreachable!("cleanup rendered separately"),
            };
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let mut apply = ui.add_enabled(valid, theme::primary_button(submit_label));
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
    });
    if modal.should_close() {
        action = Some(StructuralDialogAction::Cancel);
    }
    action
}

fn show_duplicate_preview(
    ctx: &egui::Context,
    summary: &DuplicateSummary,
) -> Option<StructuralDialogAction> {
    let mut action = None;
    let modal = egui::Modal::new(egui::Id::new("quarry-duplicate-preview"))
        .frame(tool_dialog_frame(ctx))
        .show(ctx, |ui| {
        tool_dialog_style(ui.style_mut());
        ui.set_width(480.0);
        ui.heading("Review Duplicates");
        ui.weak("Review the result before changing your document.");
        ui.separator();
        egui::Frame::new().fill(theme::ui_palette(ui).zebra).corner_radius(8).inner_margin(12).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.strong(format!("Extra duplicate rows: {}", summary.duplicate_rows));
            ui.label(format!("Rows to keep: {}", summary.retained_rows));
        });
        ui.label("Keep the first occurrence in the current row order. The header stays fixed.");
        ui.weak("Removal creates an unsaved working version and can be undone.");
        egui::CollapsingHeader::new("Details").id_salt("duplicate-review-details").show(ui, |ui| {
            ui.label("The document has not changed. Retained rows stay intact and in the same relative order.");
        });
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                action = Some(StructuralDialogAction::Cancel);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(theme::primary_button("Remove extra rows")).clicked() {
                    action = Some(StructuralDialogAction::Apply);
                }
            });
        });
    });
    if modal.should_close() {
        action = Some(StructuralDialogAction::Cancel);
    }
    action
}

fn tool_dialog_frame(ctx: &egui::Context) -> egui::Frame {
    let palette = theme::ctx_palette(ctx);
    egui::Frame::window(&ctx.style())
        .fill(palette.canvas)
        .inner_margin(20)
        .corner_radius(theme::WINDOW_RADIUS + 2)
        .stroke(egui::Stroke::new(1.0_f32, palette.control_border))
}

fn tool_dialog_style(style: &mut egui::Style) {
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.interact_size.y = 28.0;
    let palette = &theme::LIGHT;
    let visuals = &mut style.visuals;
    visuals.override_text_color = Some(palette.text);
    visuals.weak_text_color = Some(palette.text_secondary);
    visuals.text_edit_bg_color = Some(palette.control);
    visuals.widgets.inactive.bg_fill = palette.control;
    visuals.widgets.inactive.weak_bg_fill = palette.control;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, palette.control_border);
    visuals.widgets.hovered.bg_fill = palette.hover;
    visuals.widgets.hovered.weak_bg_fill = palette.hover;
}

fn compact_tool_frame(ctx: &egui::Context) -> egui::Frame {
    tool_dialog_frame(ctx)
        .inner_margin(14)
        .corner_radius(theme::WINDOW_RADIUS)
        .shadow(ctx.style().visuals.popup_shadow)
}

fn compact_tool_style(style: &mut egui::Style) {
    tool_dialog_style(style);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.interact_size.y = 28.0;
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    for text_style in [TextStyle::Body, TextStyle::Button] {
        style
            .text_styles
            .insert(text_style, FontId::proportional(13.0));
    }
    style
        .text_styles
        .insert(TextStyle::Heading, FontId::proportional(14.0));
}

fn compact_tool_anchor(ctx: &egui::Context, window_id: &str) -> egui::Pos2 {
    compact_tool_placement(ctx, window_id, 0.0).1
}

/// Popovers hang below their toolbar button, right-aligned unless that would leave the window.
fn compact_tool_placement(
    ctx: &egui::Context,
    window_id: &str,
    width: f32,
) -> (egui::Align2, egui::Pos2) {
    let gap = egui::vec2(0.0, 6.0);
    let Some(button) =
        ctx.data(|data| data.get_temp::<egui::Rect>(egui::Id::new((window_id, "anchor"))))
    else {
        return (
            egui::Align2::RIGHT_TOP,
            ctx.content_rect().right_top() + egui::vec2(-16.0, TOOLBAR_HEIGHT) + gap,
        );
    };
    if button.right() - width >= ctx.content_rect().left() + 8.0 {
        (egui::Align2::RIGHT_TOP, button.right_bottom() + gap)
    } else {
        (egui::Align2::LEFT_TOP, button.left_bottom() + gap)
    }
}

fn compact_tool_close(ui: &mut egui::Ui) -> bool {
    let response = ui
        .add(egui::Button::new("×").frame(false))
        .on_hover_text("Close");
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Close"));
    response.clicked()
}

fn filter_column_picker(
    ui: &mut egui::Ui,
    index: usize,
    rule: &mut FilterRuleDraft,
    document: &Document,
    label: egui::Id,
) {
    let selected = parse_file_column(&rule.column_input, document.total_columns)
        .map(|column| format!("{}: {}", column + 1, document.column_name(column)))
        .unwrap_or_else(|_| "Choose a column".into());
    let picker = egui::ComboBox::from_id_salt(("quarry-filter-column", index))
        .selected_text(selected.clone())
        .width(ui.available_width())
        .wrap_mode(egui::TextWrapMode::Truncate)
        .height(280.0)
        .popup_style(egui::style::StyleModifier::new(compact_tool_style))
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show_ui(ui, |ui| {
            ui.set_width(300.0);
            let search = ui.add(
                egui::TextEdit::singleline(&mut rule.column_search)
                    .id(filter_column_input_id(index))
                    .desired_width(f32::INFINITY)
                    .hint_text("Search name or column number"),
            );
            let _ = ui.ctx().accesskit_node_builder(search.id, |node| {
                node.set_label(format!("Search columns for rule {}", index + 1));
            });
            ui.separator();
            let query = rule.column_search.trim().to_lowercase();
            let source_number = query.parse::<usize>().ok();
            let columns: Vec<_> = (0..document.total_columns)
                .filter(|column| {
                    query.is_empty()
                        || source_number == Some(column + 1)
                        || document
                            .column_name(*column)
                            .to_lowercase()
                            .contains(&query)
                })
                .collect();
            if columns.is_empty() {
                ui.weak("No matching columns");
            }
            // Keep searches from shrinking the height remembered by the ComboBox popup.
            egui::ScrollArea::vertical()
                .max_height(216.0)
                .auto_shrink([true, false])
                .show_rows(ui, 30.0, columns.len(), |ui, rows| {
                    for row in rows {
                        let column = columns[row];
                        let label = format!("{}: {}", column + 1, document.column_name(column));
                        let selected = rule.column_input == (column + 1).to_string();
                        if ui
                            .add_sized(
                                [ui.available_width(), 30.0],
                                egui::Button::selectable(selected, label).truncate(),
                            )
                            .clicked()
                        {
                            rule.column_input = (column + 1).to_string();
                            ui.close();
                        }
                    }
                });
        });
    let response = picker.response.labelled_by(label).on_hover_text(selected);
    let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
        node.set_label(format!("Rule {} column", index + 1));
    });
    if response.clicked() {
        rule.column_search.clear();
        ui.memory_mut(|memory| memory.request_focus(filter_column_input_id(index)));
    }
}

fn show_filter_manager(
    ctx: &egui::Context,
    open: &mut bool,
    rules: &mut Vec<FilterRuleDraft>,
    match_case: &mut bool,
    document: &Document,
) -> Option<Action> {
    if !*open {
        return None;
    }
    if tool_dialog_escape_requested(ctx, "quarry-filter-manager") {
        surrender_filter_text_focus(ctx, rules.len());
        *open = false;
        return None;
    }
    let mut action = None;
    let mut close = false;
    egui::Window::new("Filters")
        .id(egui::Id::new("quarry-filter-manager"))
        .title_bar(false)
        .pivot(compact_tool_placement(ctx, "quarry-filter-manager", 630.0).0)
        .fixed_pos(compact_tool_placement(ctx, "quarry-filter-manager", 630.0).1)
        .min_width(600.0)
        .max_width(600.0)
        .resizable(false)
        .frame(compact_tool_frame(ctx))
        .show(ctx, |ui| {
            compact_tool_style(ui.style_mut());
            ui.set_width(600.0);
            ui.horizontal(|ui| {
                ui.heading("Filters");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    close = compact_tool_close(ui);
                });
            });
            ui.separator();
            let rule_count = rules.len();
            let mut remove_index = None;
            let column_width = 188.0;
            let condition_width = 150.0;
            let number_width = 20.0;
            let remove_width = 24.0;
            let labels = ui.horizontal(|ui| {
                ui.add_space(number_width + ui.spacing().item_spacing.x);
                let column = ui.allocate_ui_with_layout(egui::vec2(column_width, 16.0), Layout::top_down(Align::Min), |ui| { ui.set_min_width(column_width); ui.weak("Column") }).inner.id;
                let condition = ui.allocate_ui_with_layout(egui::vec2(condition_width, 16.0), Layout::top_down(Align::Min), |ui| { ui.set_min_width(condition_width); ui.weak("Condition") }).inner.id;
                let value = ui.weak("Value").id;
                (column, condition, value)
            }).inner;
            egui::ScrollArea::vertical()
                .id_salt(("quarry-filter-rules", &document.logical_path))
                .max_height((ctx.content_rect().bottom() - compact_tool_anchor(ctx, "quarry-filter-manager").y - 242.0).clamp(96.0, 360.0))
                .show(ui, |ui| {
                    let value_width = ui.available_width() - column_width - condition_width
                        - number_width - remove_width - 4.0 * ui.spacing().item_spacing.x;
                    for (index, rule) in rules.iter_mut().enumerate() {
                        ui.horizontal_top(|ui| {
                            ui.add_sized([number_width, 28.0], egui::Label::new(RichText::new((index + 1).to_string()).weak()));
                            ui.allocate_ui_with_layout(egui::vec2(column_width, 28.0), Layout::top_down(Align::Min), |ui| {
                                filter_column_picker(ui, index, rule, document, labels.0);
                            });
                            ui.allocate_ui_with_layout(egui::vec2(condition_width, 28.0), Layout::top_down(Align::Min), |ui| {
                                let response = egui::ComboBox::from_id_salt(("quarry-filter-operator", index))
                                    .width(condition_width)
                                    .wrap_mode(egui::TextWrapMode::Truncate)
                                    .popup_style(egui::style::StyleModifier::new(compact_tool_style))
                                    .selected_text(filter_operator_label(rule.operator))
                                    .show_ui(ui, |ui| {
                                        for operator in [FilterOperator::Contains, FilterOperator::Equals, FilterOperator::NotEquals] {
                                            if ui.selectable_value(&mut rule.operator, operator, filter_operator_label(operator)).clicked() {
                                                ui.close();
                                            }
                                        }
                                        ui.separator();
                                        for operator in [FilterOperator::GreaterThan, FilterOperator::GreaterThanOrEqual, FilterOperator::LessThan, FilterOperator::LessThanOrEqual, FilterOperator::Between] {
                                            if ui.selectable_value(&mut rule.operator, operator, filter_operator_label(operator)).clicked() {
                                                ui.close();
                                            }
                                        }
                                    }).response.labelled_by(labels.1)
                                    .on_hover_text(filter_operator_label(rule.operator));
                                let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
                                    node.set_label(format!("Rule {} match", index + 1));
                                });
                            });
                            ui.allocate_ui_with_layout(egui::vec2(value_width, 28.0), Layout::top_down(Align::Min), |ui| {
                                if rule.operator.is_numeric() {
                                    let between = rule.operator == FilterOperator::Between;
                                    let response = ui.add_sized([value_width, 28.0], egui::TextEdit::singleline(&mut rule.value_input)
                                        .id(filter_value_input_id(index))
                                        .hint_text(if between { "Lower bound" } else { "Number" }))
                                        .labelled_by(labels.2);
                                    let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
                                        node.set_label(format!("Rule {} {}", index + 1, if between { "lower bound" } else { "number" }));
                                    });
                                    if between {
                                        let response = ui.add_sized([value_width, 28.0], egui::TextEdit::singleline(&mut rule.upper_bound_input)
                                            .id(filter_upper_bound_input_id(index)).hint_text("Upper bound"))
                                            .labelled_by(labels.2);
                                        let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
                                            node.set_label(format!("Rule {} upper bound", index + 1));
                                        });
                                    }
                                } else {
                                    let response = egui::ScrollArea::vertical()
                                        .id_salt(("quarry-filter-value-scroll", index))
                                        .max_height(64.0)
                                        .show(ui, |ui| {
                                            ui.add_sized([ui.available_width(), 28.0],
                                                egui::TextEdit::multiline(&mut rule.value_input).desired_rows(1)
                                                    .id(filter_value_input_id(index)).hint_text("Value"))
                                                .labelled_by(labels.2)
                                                .on_hover_text("Literal text, including spaces and line breaks.")
                                        }).inner;
                                    let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
                                        node.set_label(format!("Rule {} value", index + 1));
                                    });
                                }
                            });
                            let response = ui.add_enabled(rule_count > 1, egui::Button::new("×").frame(false).min_size(egui::vec2(remove_width, 28.0)))
                                .on_hover_text(format!("Remove rule {}", index + 1));
                            let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
                                node.set_label(format!("Remove rule {}", index + 1));
                            });
                            if response.clicked() {
                                surrender_filter_text_focus(ui.ctx(), rule_count);
                                remove_index = Some(index);
                            }
                        });
                        let error = filter_predicate_from_rule(rule, document.total_columns).and_then(|predicate| {
                            FilterQuery { predicates: vec![predicate], case_sensitivity: case_sensitivity(*match_case) }
                                .validate().map_err(|error| match error {
                                    QuarryError::InvalidFilter { reason, .. } => reason.to_owned(),
                                    error => error.to_string(),
                                })
                        }).err();
                        if rule.operator == FilterOperator::Contains && rule.value_input.is_empty() {
                            ui.weak("Enter text to match.");
                        } else if let Some(error) = error {
                            ui.colored_label(ui.visuals().error_fg_color, error);
                        } else if rule.operator == FilterOperator::Between {
                            ui.weak("Includes both endpoints.");
                        }
                        if index + 1 < rule_count {
                            ui.separator();
                        }
                    }
                    egui::CollapsingHeader::new("Details").show(ui, |ui| {
                        ui.label("Filters include unsaved edits. Committing a cell edit refreshes matching rows.");
                        ui.label("Rows must match every filtered column. Inclusion rules in the same column are alternatives. Does not equal rules exclude matches.");
                        ui.label("Use Between for a numeric range. Blank, missing and invalid numbers do not match numeric rules.");
                        ui.label("Text values are literal, including spaces and line breaks. Equals and Does not equal can compare empty cells. Match case applies to text only.");
                        if let Some(query) = &document.filter_query {
                            ui.separator();
                            ui.strong("Applied rules");
                            for predicate in &query.predicates {
                                let mut value = format!("{:?}", field_text(&predicate.value));
                                if let Some(upper) = &predicate.upper_bound {
                                    value.push_str(&format!(" and {:?}", field_text(upper)));
                                }
                                ui.label(format!("{}: {} · {} {}", predicate.column + 1, document.column_name(predicate.column), filter_operator_label(predicate.operator), value));
                            }
                        }
                    });
                });
            if let Some(index) = remove_index {
                rules.remove(index);
            }
            ui.horizontal(|ui| {
                if ui.button("Add rule").clicked() {
                    surrender_filter_text_focus(ctx, rules.len());
                    rules.push(FilterRuleDraft::default());
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.checkbox(match_case, "Match case").on_hover_text("Applies to text rules only.");
                });
            });
            ui.separator();
            if document.search_job.is_some() {
                ui.label("Cancel the active search before filtering.");
            } else if document.total_columns == 0 {
                ui.label("Open a file with at least one column to filter rows.");
            }
            if let Some(query) = &document.filter_query {
                ui.weak(format!("{} matching rows · {} active rule{} · {}{}",
                    document.available_filter_rows(), query.predicates.len(),
                    if query.predicates.len() == 1 { "" } else { "s" },
                    case_sensitivity_label(query.case_sensitivity),
                    if document.filter_job.is_some() { " · Filtering…" } else { "" }));
            }
            let can_apply = document.is_filter_ready()
                && document.search_job.is_none()
                && document.filter_job.is_none()
                && document.export_job.is_none()
                && filter_query_from_rules(rules, *match_case, document.total_columns).is_ok();
            ui.horizontal(|ui| {
                if ui.add_enabled(document.filter_active() && document.export_job.is_none(), egui::Button::new("Clear filter")).clicked() {
                    action = Some(Action::ClearFilter);
                }
                if document.filter_active()
                    && ui.add_enabled(document.is_filtered_export_ready(), egui::Button::new("Export Filtered Rows…"))
                        .on_hover_text("Export all matching rows with your current edits to a new file").clicked()
                {
                    action = Some(Action::ChooseFilteredExport);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add_enabled(can_apply, theme::primary_button("Apply filters")).clicked() {
                        action = Some(Action::ApplyFilter);
                    }
                });
            });
        });
    if close {
        surrender_filter_text_focus(ctx, rules.len());
        *open = false;
    }
    action
}

fn filter_operator_label(operator: FilterOperator) -> &'static str {
    match operator {
        FilterOperator::Contains => "Contains",
        FilterOperator::Equals => "Equals",
        FilterOperator::NotEquals => "Does not equal",
        FilterOperator::GreaterThan => "Greater than (>)",
        FilterOperator::GreaterThanOrEqual => "Greater than or equal (>=)",
        FilterOperator::LessThan => "Less than (<)",
        FilterOperator::LessThanOrEqual => "Less than or equal (<=)",
        FilterOperator::Between => "Between (inclusive)",
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
        if ui.is_enabled()
            && (input.has_focus() || input.lost_focus() && !input.clicked_elsewhere())
            && ui.input(|input| input.key_pressed(egui::Key::Escape))
        {
            close_requested = true;
        }
        let can_find =
            ui.is_enabled() && index_ready && !searching && !filter_active && !query.is_empty();
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
            if ui.is_enabled()
                && (input.has_focus() || input.lost_focus() && !input.clicked_elsewhere())
                && ui.input(|input| input.key_pressed(egui::Key::Escape))
            {
                close_requested = true;
            }
            let can_replace = ui.is_enabled() && can_replace && !searching && !filter_active;
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
    let source = compressed::uncompressed_name(source);
    let source = source.as_path();
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
    // A compressed source suggests its uncompressed name: sales.csv.gz -> sales.csv.
    let uncompressed = compressed::uncompressed_name(source);
    if uncompressed != source
        && let Some(name) = uncompressed.file_name()
    {
        return name.to_string_lossy().into_owned();
    }
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

#[derive(Default)]
struct GridInteraction {
    selection: Option<GridSelection>,
    row_selection: Option<(RowSelection, Option<u64>)>,
    column_request: Option<GridColumnRequest>,
    filter_query: Option<FilterQuery>,
    copy_selection: bool,
}

fn selection_modifiers(modifiers: egui::Modifiers) -> SelectionModifiers {
    SelectionModifiers {
        shift: modifiers.shift,
        command: modifiers.command,
    }
}

fn select_column(
    columns: &ColumnView,
    selected_columns: &mut BTreeSet<usize>,
    anchor: &mut Option<usize>,
    column: usize,
    modifiers: egui::Modifiers,
) {
    quarry_workbench::select_column(
        columns,
        selected_columns,
        anchor,
        column,
        selection_modifiers(modifiers),
    );
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

/// Grid cells are square, borderless, and show a hover tint; the focused cell gets an amber ring.
fn grid_style(style: &mut egui::Style) {
    let palette = &theme::LIGHT;
    style.spacing.button_padding.x = CELL_PADDING;
    let widgets = &mut style.visuals.widgets;
    for visuals in [
        &mut widgets.noninteractive,
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
        &mut widgets.open,
    ] {
        visuals.corner_radius = egui::CornerRadius::ZERO;
        visuals.expansion = 0.0;
    }
    widgets.inactive.bg_stroke = egui::Stroke::NONE;
    widgets.hovered.bg_stroke = egui::Stroke::NONE;
    widgets.hovered.weak_bg_fill = palette.hover.gamma_multiply(0.55);
    widgets.hovered.bg_fill = widgets.hovered.weak_bg_fill;
    widgets.active.weak_bg_fill = palette.selection;
    widgets.active.bg_fill = palette.selection;
    widgets.active.bg_stroke = egui::Stroke::new(1.5_f32, palette.selection_strong);
}

fn paint_header_fill(ui: &egui::Ui, palette: &theme::Palette) {
    ui.painter()
        .rect_filled(ui.max_rect(), 0.0, palette.header_fill);
}

/// Adds `widget` filling `size`, with its content aligned left or right.
fn add_aligned(
    ui: &mut egui::Ui,
    size: egui::Vec2,
    align: Align,
    widget: impl egui::Widget,
) -> egui::Response {
    ui.allocate_ui_with_layout(
        size,
        Layout::left_to_right(Align::Center)
            .with_main_align(align)
            .with_main_justify(true),
        |ui| ui.add(widget),
    )
    .inner
}

/// A header label inset like the cells below it.
fn add_padded_label(ui: &mut egui::Ui, width: f32, label: egui::Label) -> egui::Response {
    ui.allocate_ui_with_layout(
        egui::vec2(width, HEADER_NAME_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.add_space(CELL_PADDING);
            add_aligned(
                ui,
                egui::vec2((width - 2.0 * CELL_PADDING).max(0.0), HEADER_NAME_HEIGHT),
                Align::Min,
                label,
            )
        },
    )
    .inner
}

/// Numbers, amounts, and percentages align right so digits line up.
///
/// Accepts an optional exponent (`1e3`, `-2.5E-4`) with up to seven digits, matching
/// the exponent range the Number sort supports.
fn looks_numeric(text: &str) -> bool {
    let text = text.trim();
    let body = text
        .trim_start_matches(['-', '+', '$', '€', '£'])
        .trim_end_matches('%');
    if body.is_empty() || body.len() > 32 {
        return false;
    }
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(index) => (&body[..index], Some(&body[index + 1..])),
        None => (body, None),
    };
    let mantissa_ok = mantissa.bytes().any(|byte| byte.is_ascii_digit())
        && mantissa
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b','))
        && mantissa.bytes().filter(|byte| *byte == b'.').count() <= 1;
    let exponent_ok = exponent.is_none_or(|exponent| {
        let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        !digits.is_empty() && digits.len() <= 7 && digits.bytes().all(|byte| byte.is_ascii_digit())
    });
    mantissa_ok && exponent_ok
}

/// Classifies a cell's source value. Whitespace rendering turns edge spaces into
/// marks, so alignment must be decided before it. Long values are never numeric.
fn source_looks_numeric(value: &[u8]) -> bool {
    value.len() <= 64 && std::str::from_utf8(value).is_ok_and(looks_numeric)
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
    grid_view: &mut EguiGridState,
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
    if ui.is_enabled() && total_rows > 0 && ui.rect_contains_pointer(grid_rect) {
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
                    scrollbar_slider(ui, &mut slider_position, thumb_height, scroll_enabled)
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
                show_table(
                    ui,
                    document,
                    grid_view,
                    reveal_cell,
                    filter_case_sensitivity,
                )
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
    let mut grid_view = EguiGridState::default();
    show_grid_with_filter_case(ui, document, &mut grid_view, CaseSensitivity::Insensitive)
}

#[cfg(test)]
fn show_grid_with_view(
    ui: &mut egui::Ui,
    document: &mut Document,
    grid_view: &mut EguiGridState,
) -> Result<Option<GridColumnRequest>, String> {
    show_grid_with_filter_case(ui, document, grid_view, CaseSensitivity::Insensitive)
}

fn whitespace_view_menu(ui: &mut egui::Ui, show_all: bool) -> Option<Action> {
    let mut action = None;
    ui.menu_button("View", |ui| {
        let mut all = show_all;
        if ui.checkbox(&mut all, "Show All Whitespace").changed() {
            action = Some(Action::ShowAllWhitespace(all));
            ui.close();
        }
        ui.label("Normal view marks edge and repeated spaces,\ntabs, line breaks, and special whitespace.");
    });
    action
}

fn show_table(
    ui: &mut egui::Ui,
    document: &mut Document,
    grid_view: &mut EguiGridState,
    reveal_cell: Option<(u64, usize)>,
    filter_case_sensitivity: CaseSensitivity,
) -> GridInteraction {
    grid_style(ui.style_mut());
    let digit_padding = theme::digit_padding(ui);
    let palette = theme::ui_palette(ui);
    let mut whitespace = std::mem::take(&mut document.whitespace);
    whitespace.begin_frame();
    let show_all = document.whitespace_all;
    let row_count = document.visible_row_count();
    let auto_fit_columns = std::mem::take(&mut grid_view.auto_fit_columns);
    let virtualized = document.headers.len() > MAX_RENDERED_COLUMNS;
    if auto_fit_columns {
        grid_view.columns_to_fit.clear();
        if virtualized || !grid_view.fitted_column_widths.is_empty() {
            grid_view.columns_to_fit.extend(&document.columns.visible);
        }
    }
    // Fit in bounded batches so very wide files do not block the UI.
    for _ in 0..MAX_RENDERED_COLUMNS {
        let Some(column) = grid_view.columns_to_fit.pop_front() else {
            break;
        };
        if document.columns.hidden[column] {
            continue;
        }
        let text_width = |text: String, numeric: bool| {
            let extra = if numeric {
                theme::tabular_extra_width(&text, &digit_padding)
            } else {
                0.0
            };
            ui.painter()
                .layout_no_wrap(text, theme::grid_font(), ui.visuals().text_color())
                .size()
                .x
                + extra
        };
        let header_text = document.header_value(column).map_or_else(
            || document.column_name(column),
            |value| {
                whitespace::render(
                    value,
                    None,
                    show_all,
                    120,
                    ui.visuals().text_color(),
                    palette.mark,
                )
                .job
                .text
            },
        );
        let mut width = (text_width(header_text, false) + 16.0).max(80.0);
        for index in 0..row_count {
            if let Some((row, fields)) = document.visible_row(index)
                && let Some(source) = fields.get(column)
            {
                let value = document.cell_value(row, column, source);
                width = width.max(
                    text_width(
                        whitespace::render(
                            value,
                            None,
                            show_all,
                            MAX_GRID_TEXT_CHARS,
                            ui.visuals().text_color(),
                            palette.mark,
                        )
                        .job
                        .text,
                        source_looks_numeric(value),
                    ) + 2.0 * ui.spacing().button_padding.x,
                );
            }
        }
        grid_view.fitted_column_widths.insert(column, width.ceil());
        grid_view.reset_table_widths = true;
    }
    if !grid_view.columns_to_fit.is_empty() {
        ui.ctx().request_repaint();
    }
    let mut interaction = GridInteraction::default();
    let mut active_header_edit = document.header_edit.take();
    let mut begin_header_edit = None;
    let mut commit_header_edit = false;
    let mut cancel_header_edit = false;
    let mut active_cell_edit = document.cell_edit.take();
    let cell_focus_requested = grid_view.cell_focus_requested.take();
    let column_focus_requested = grid_view.column_focus_requested.take();
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
            grid_view
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
                            theme::grid_font(),
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
        let mut header_rect = egui::Rect::NOTHING;
        let mut row_bottoms = Vec::with_capacity(row_count);
        let mut fixed_dividers = Vec::new();
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
        if !virtualized && (std::mem::take(&mut grid_view.reset_table_widths) || auto_fit_columns) {
            table.reset();
        }
        table
            .header(HEADER_HEIGHT, |mut header| {
                header.col(|ui| {
                    header_rect = ui.max_rect();
                    paint_header_fill(ui, palette);
                    fixed_dividers.push(ui.max_rect().right() + column_spacing * 0.5);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.add_space(COLUMN_RULER_HEIGHT);
                    });
                });
                if spacer_width.is_some() {
                    header.col(|ui| {
                        header_rect = header_rect.union(ui.max_rect());
                        fixed_dividers.push(ui.max_rect().right() + column_spacing * 0.5);
                    });
                }
                for (_, column, name) in &visible_headers {
                    let column = *column;
                    header.col(|ui| {
                        header_rect = header_rect.union(ui.max_rect());
                        if virtualized {
                            fixed_dividers.push(ui.max_rect().right() + column_spacing * 0.5);
                        }
                        ui.push_id(("column-header", column), |ui| {
                            let selected = document.selected_columns.contains(&column);
                            paint_header_fill(ui, palette);
                            paint_column_selection(ui, selected);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                let width = ui.available_width();
                                let mut button = egui::Button::selectable(
                                        selected,
                                        RichText::new(column.saturating_add(1).to_string())
                                            .size(10.5)
                                            .color(if selected { palette.on_accent } else { palette.text_secondary }),
                                    )
                                    .small();
                                if selected {
                                    button = button
                                        .fill(palette.selection_strong)
                                        .stroke(egui::Stroke::NONE);
                                }
                                let response = ui
                                    .add_sized([width, COLUMN_RULER_HEIGHT], button)
                                    .on_hover_text(
                                        "Click to select. Shift-click a range. Command/Ctrl-click to add or remove. Right-click a selected number for column tools and row sorting.",
                                    );
                                if ui.is_enabled() && column_focus_requested == Some(column) {
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
                                let accesskit_context_menu = ui.is_enabled() && ui.input_mut(|input| {
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
                                let keyboard_context_menu = ui.is_enabled() && response.has_focus()
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
                                let popup_command = if !ui.is_enabled() {
                                    Some(egui::SetOpenCommand::Bool(false))
                                } else if open_context_menu {
                                    Some(egui::SetOpenCommand::Bool(true))
                                } else if response.clicked() {
                                    Some(egui::SetOpenCommand::Bool(false))
                                } else {
                                    None
                                };
                                egui::Popup::menu(&response)
                                    .open_memory(popup_command)
                                    .show(|ui| {
                                    for (label, position) in [("Insert Columns Before…", column), ("Insert Columns After…", column.saturating_add(1))] {
                                        let reason = document.insertion_disabled_reason(false);
                                        if ui.add_enabled(reason.is_none(), egui::Button::new(label))
                                            .on_disabled_hover_text(reason.unwrap_or_default()).clicked() {
                                            interaction.column_request = Some(GridColumnRequest::Dialog(StructuralDialog::insert_columns(position)));
                                            ui.close();
                                        }
                                    }
                                    ui.separator();
                                    let cleanup_columns = document.selected_columns.iter().copied().filter(|c| *c < document.total_columns).collect::<Vec<_>>();
                                    let cleanup_reason = document.structural_edit_disabled_reason();
                                    if ui.add_enabled(cleanup_reason.is_none() && !cleanup_columns.is_empty(), egui::Button::new("Clean Up Columns…"))
                                        .on_disabled_hover_text(cleanup_reason.unwrap_or("Select a file column first.")).clicked() {
                                        interaction.column_request = Some(GridColumnRequest::Dialog(StructuralDialog::cleanup_columns(cleanup_columns)));
                                        ui.close();
                                    }
                                    ui.separator();
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
                                            GridColumnRequest::Delete(combine_columns.clone()),
                                        );
                                        ui.close();
                                    }
                                    ui.separator();
                                    if ui.add_enabled(
                                        !combine_columns.is_empty(),
                                        egui::Button::new("Find Duplicates…"),
                                    ).clicked() {
                                        interaction.column_request = Some(GridColumnRequest::Dialog(
                                            StructuralDialog::duplicates(combine_columns.clone()),
                                        ));
                                        ui.close();
                                    }
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
                                    if ui.is_enabled() && edit.focus_requested {
                                        reset_editor_undo(ui.ctx(), header_edit_id(column));
                                    }
                                    let response = ui.add_sized(
                                        [width, HEADER_NAME_HEIGHT],
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
                                    if ui.is_enabled() && edit.focus_requested {
                                        response.request_focus();
                                        edit.focus_requested = false;
                                    }
                                    if ui.is_enabled() && ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                                        cancel_header_edit = true;
                                    } else if ui.is_enabled() && ((response.has_focus()
                                        && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                                        || response.lost_focus())
                                    {
                                        commit_header_edit = true;
                                    }
                                } else {
                                    let editable = document.header_is_editable(column);
                                    let header_value = document.header_value(column);
                                    let summary = header_value.and_then(|value| whitespace.get(
                                        whitespace::CellId { row: None, column }, value.len(),
                                        || document.whitespace_header_input(column),
                                    ));
                                    let rendered = whitespace::render(
                                        header_value.unwrap_or(name.as_bytes()), summary, show_all && header_value.is_some(), 120,
                                        ui.visuals().text_color(), palette.mark,
                                    );
                                    let indicated = summary.is_some_and(|s| s.indicated(show_all)) || rendered.visible_marks;
                                    let pending = header_value.is_some() && summary.is_none();
                                    let explanation = header_value.map(|_| whitespace.explanation(summary, show_all));
                                    let response = add_padded_label(
                                        ui,
                                        width,
                                        egui::Label::new(rendered.job)
                                            .truncate()
                                            .sense(if editable {
                                                egui::Sense::click()
                                            } else {
                                                egui::Sense::hover()
                                            }),
                                    );
                                    whitespace::paint_indicator(ui, &response, indicated, pending);
                                    if let Some(explanation) = &explanation {
                                        let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
                                            node.set_description(explanation.clone());
                                        });
                                    }
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
                                        let activate = ui.is_enabled() && (response.clicked()
                                            || (response.has_focus()
                                                && ui.input(|input| {
                                                    input.key_pressed(egui::Key::Enter)
                                                })));
                                        response.on_hover_text(format!("Click to rename\n{}", explanation.unwrap_or_default()));
                                        if activate {
                                            begin_header_edit = Some(column);
                                        }
                                    } else if let Some(explanation) = explanation {
                                        response.on_hover_text(explanation);
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
                            row_bottoms.push(ui.max_rect().bottom());
                            ui.scope_builder(
                                egui::UiBuilder::new().id(("row-selection", record_row)),
                                |ui| {
                                let selected = document.selected_rows.contains(record_row)
                                    || document
                                        .selection
                                        .is_some_and(|selection| selection.selects_row(record_row));
                                let color = if selected {
                                    palette.text
                                } else {
                                    palette.text_secondary
                                };
                                if !selected {
                                    paint_header_fill(ui, palette);
                                }
                                let response = add_aligned(
                                    ui,
                                    egui::vec2(ui.available_width(), ROW_HEIGHT),
                                    Align::Max,
                                    egui::Button::selectable(
                                        selected,
                                        RichText::new(display_row.to_string())
                                            .size(11.5)
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
                                let accesskit_context_menu = ui.is_enabled() && ui.input_mut(|input| {
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
                                let keyboard_context_menu = ui.is_enabled() && response.has_focus()
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
                                    selection.select(record_row, SelectionModifiers::NONE);
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
                                    selection.select(record_row, selection_modifiers(modifiers));
                                    let active_row = selection
                                        .contains(record_row)
                                        .then_some(record_row)
                                        .or_else(|| selection.first());
                                    interaction.row_selection =
                                        Some((selection.clone(), active_row));
                                    effective_row_selection = Some(selection);
                                }
                                let popup_command = if !ui.is_enabled() {
                                    Some(egui::SetOpenCommand::Bool(false))
                                } else if open_context_menu {
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
                                        for (label, position) in [("Insert Rows Above…", display_row - 1), ("Insert Rows Below…", display_row)] {
                                            let reason = document.insertion_disabled_reason(true);
                                            if ui.add_enabled(reason.is_none(), egui::Button::new(label))
                                                .on_disabled_hover_text(reason.unwrap_or_default()).clicked() {
                                                interaction.column_request = Some(GridColumnRequest::Dialog(StructuralDialog::insert_rows(position)));
                                                ui.close();
                                            }
                                        }
                                        ui.separator();
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
                                    let source = fields.get(column).map(|value| value.as_slice());
                                    let header = &document.headers[visible_column];
                                    let accessible_header = accessible_header_name(header);
                                    if let Some(edit) = active_cell_edit.as_mut().filter(|edit| {
                                        edit.row == record_row && edit.column == column
                                    }) {
                                        if ui.is_enabled() && edit.focus_requested {
                                            reset_editor_undo(ui.ctx(), cell_edit_id(record_row, column));
                                        }
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
                                        if ui.is_enabled() && edit.focus_requested {
                                            response.request_focus();
                                            edit.focus_requested = false;
                                        }
                                        let escape = ui.is_enabled() && ui.input(|input| {
                                            input.key_pressed(egui::Key::Escape)
                                        });
                                        let enter = ui.is_enabled() && response.has_focus()
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
                                        } else if ui.is_enabled() && response.lost_focus() {
                                            commit_cell_edit = true;
                                        }
                                        if reveal_cell == Some((record_row, column)) {
                                            response.scroll_to_me(Some(Align::Center));
                                        }
                                    } else {
                                        let value = source.map(|source| {
                                            document.cell_value(record_row, column, source)
                                        });
                                        let text = value.map_or_else(String::new, grid_field_text);
                                        let summary = value.and_then(|value| whitespace.get(
                                            whitespace::CellId { row: Some(record_row), column },
                                            value.len(), || document.whitespace_input(record_row, column),
                                        ));
                                        let rendered = whitespace::render(value.unwrap_or_default(), summary,
                                            show_all, MAX_GRID_TEXT_CHARS, ui.visuals().text_color(), palette.mark);
                                        let indicated = summary.is_some_and(|s| s.indicated(show_all)) || rendered.visible_marks;
                                        let pending = value.is_some() && summary.is_none();
                                        let explanation = if value.is_some() {
                                            whitespace.explanation(summary, show_all)
                                        } else { "Field is absent from this row.".into() };
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
                                        let mut rendered = rendered;
                                        let align = if value.is_some_and(source_looks_numeric) {
                                            // Equal-width digits keep right-aligned columns of numbers straight.
                                            rendered.job = theme::tabular_digits(&rendered.job, &digit_padding);
                                            Align::Max
                                        } else {
                                            Align::Min
                                        };
                                        let response = add_aligned(
                                            ui,
                                            egui::vec2(ui.available_width(), ROW_HEIGHT),
                                            align,
                                            egui::Button::selectable(
                                                selected,
                                                rendered.job,
                                            )
                                            .wrap_mode(egui::TextWrapMode::Extend)
                                            .small(),
                                        );
                                        whitespace::paint_indicator(ui, &response, indicated, pending);
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
                                                format!("Activate to select. Open the context menu to copy or filter by this value. {explanation}"),
                                            );
                                            node.add_action(
                                                egui::accesskit::Action::ShowContextMenu,
                                            );
                                        });
                                        let accesskit_context_menu = ui.is_enabled() && ui.input_mut(|input| {
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
                                        let keyboard_context_menu = ui.is_enabled() && response.has_focus()
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
                                        let popup_command = if !ui.is_enabled() {
                                            Some(egui::SetOpenCommand::Bool(false))
                                        } else if open_context_menu {
                                            Some(egui::SetOpenCommand::Bool(true))
                                        } else if response.clicked() {
                                            Some(egui::SetOpenCommand::Bool(false))
                                        } else {
                                            None
                                        };
                                        let can_filter = source.is_some()
                                            && document.is_filter_ready()
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
                                                            upper_bound: None,
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
                                                            upper_bound: None,
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
                                        if ui.is_enabled() && cell_focus_requested == Some((record_row, column)) {
                                            response.request_focus();
                                        }
                                        let keyboard_activate = ui.is_enabled() && active_cell_selected
                                            && response.has_focus()
                                            && ui.input(|input| {
                                                input.key_pressed(egui::Key::Enter)
                                                    || input.key_pressed(egui::Key::F2)
                                            });
                                        // Keep a click to the next cell from being swallowed by
                                        // the refresh triggered when the current editor commits.
                                        let continue_filtered_edit = document.filter_active()
                                            && active_cell_edit.is_some()
                                            && response.clicked();
                                        if document.cell_is_editable(source)
                                            && (response.double_clicked() || keyboard_activate
                                                || continue_filtered_edit)
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
                                            response.on_hover_text(format!("{reason}\n{explanation}"));
                                        } else {
                                            response.on_hover_text(format!(
                                                "Double-click or press Enter/F2 to edit\n{explanation}",
                                            ));
                                        }
                                    }
                                    },
                                );
                            });
                        }
                });
            });
            // Paint after all cell backgrounds so stripes and selection cannot erase borders.
            // Geometry comes only from the rendered rows/columns, including resized columns.
            let stroke = column_ruler_divider_stroke(ui.visuals());
            let left = header_rect.left();
            let right = ui.min_rect().right().max(header_rect.right() + column_spacing * 0.5);
            let bottom = row_bottoms.last().copied().unwrap_or(header_rect.bottom());
            for y in [header_rect.top(), header_rect.top() + COLUMN_RULER_HEIGHT, header_rect.bottom()]
                .into_iter().chain(row_bottoms)
            {
                ui.painter().hline(left..=right, y, stroke);
            }
            for x in std::iter::once(left).chain(fixed_dividers) {
                ui.painter().vline(x, header_rect.top()..=bottom, stroke);
            }
        });

    whitespace::finish_frame(&mut whitespace, ui.ctx());
    document.whitespace = whitespace;
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
    grid_view.cell_focus_requested = restore_cell_focus;
    interaction
}

/// A thin overlay-style thumb with no track, like macOS scroll bars.
fn scrollbar_style(ui: &mut egui::Ui) {
    let palette = theme::ui_palette(ui);
    ui.spacing_mut().slider_rail_height = 0.0;
    let widgets = &mut ui.visuals_mut().widgets;
    for (visuals, alpha) in [
        (&mut widgets.inactive, 0.45),
        (&mut widgets.hovered, 0.7),
        (&mut widgets.active, 0.9),
    ] {
        visuals.bg_fill = palette.text_tertiary.gamma_multiply(alpha);
        visuals.weak_bg_fill = Color32::TRANSPARENT;
        visuals.bg_stroke = egui::Stroke::NONE;
        visuals.fg_stroke = egui::Stroke::NONE;
        visuals.expansion = 0.0;
    }
}

/// The grid's vertical scrollbar: an egui slider drawn as a thin thumb.
fn scrollbar_slider(
    ui: &mut egui::Ui,
    slider_position: &mut f64,
    thumb_height: f32,
    enabled: bool,
) -> egui::Response {
    scrollbar_style(ui);
    // egui makes a slider as thick as the larger of the body text height and
    // `interact_size.y`, and sizes the thumb from that thickness. Pin both to
    // the scrollbar width (the slider shows no text) so the thumb is exactly
    // `thumb_height` tall. A taller thumb than intended can outgrow its track,
    // which makes dragging run backwards.
    ui.spacing_mut().interact_size.y = SCROLLBAR_WIDTH;
    ui.style_mut()
        .text_styles
        .insert(TextStyle::Body, FontId::proportional(SCROLLBAR_WIDTH / 2.0));
    let thickness = ui
        .text_style_height(&TextStyle::Body)
        .max(ui.spacing().interact_size.y);
    let handle_radius = thickness / 2.5;
    ui.add_enabled(
        enabled,
        egui::Slider::new(slider_position, 0.0..=1.0)
            .vertical()
            .show_value(false)
            .smart_aim(false)
            .handle_shape(egui::style::HandleShape::Rect {
                aspect_ratio: thumb_height / (2.0 * handle_radius),
            }),
    )
}

fn scrollbar_thumb_height(track_height: f32, total_rows: u64, visible_rows: usize) -> f32 {
    if total_rows == 0 {
        return track_height;
    }
    let ratio = (visible_rows as f64 / total_rows as f64).min(1.0) as f32;
    (track_height * ratio).clamp(MIN_THUMB_HEIGHT.min(track_height), track_height)
}

fn configure_style(ctx: &egui::Context) {
    theme::configure(ctx);
}

/// Width reserved for the macOS traffic lights while the toolbar shares the title bar.
fn titlebar_inset(ctx: &egui::Context) -> f32 {
    if cfg!(target_os = "macos") && !ctx.input(|input| input.viewport().fullscreen.unwrap_or(false))
    {
        TRAFFIC_LIGHT_INSET
    } else {
        0.0
    }
}

/// Empty title-bar space moves the window and double-click zooms it, as in native apps.
fn titlebar_drag_area(ui: &mut egui::Ui) {
    let response = ui.interact(
        ui.max_rect(),
        ui.id().with("titlebar-drag"),
        egui::Sense::click_and_drag(),
    );
    if response.drag_started_by(egui::PointerButton::Primary) {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    if response.double_clicked() {
        let maximized = ui.input(|input| input.viewport().maximized.unwrap_or(false));
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
    }
}

fn toolbar_separator(ui: &mut egui::Ui) {
    let palette = theme::ui_palette(ui);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 18.0), egui::Sense::hover());
    ui.painter().vline(
        rect.center().x,
        rect.y_range(),
        egui::Stroke::new(1.0_f32, palette.separator),
    );
}

/// Keeps AppKit windows that Quarry opens, such as the file picker and alerts,
/// light to match Quarry's light-only interface when the system uses Dark mode.
/// Menu-bar menus still follow the system, because macOS draws them.
#[cfg(target_os = "macos")]
fn install_light_appearance() {
    use objc2_app_kit::{NSAppearance, NSAppearanceNameAqua};
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // SAFETY: `NSAppearanceNameAqua` is an immutable AppKit string constant.
    let name = unsafe { NSAppearanceNameAqua };
    if let Some(appearance) = NSAppearance::appearanceNamed(name) {
        NSApplication::sharedApplication(mtm).setAppearance(Some(&appearance));
    }
}

/// Gives the window a unified compact toolbar so the traffic lights center in Quarry's toolbar.
#[cfg(target_os = "macos")]
fn install_unified_titlebar() {
    use objc2_app_kit::NSWindowToolbarStyle;
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    for window in NSApplication::sharedApplication(mtm).windows().iter() {
        // SAFETY: NSToolbar's `new` returns an empty toolbar, and NSWindow's
        // `setToolbar:` takes one. Untyped calls avoid an extra objc2-app-kit feature.
        unsafe {
            let toolbar: objc2::rc::Retained<AnyObject> =
                objc2::msg_send![objc2::class!(NSToolbar), new];
            let _: () = objc2::msg_send![&*window, setToolbar: &*toolbar];
        }
        window.setToolbarStyle(NSWindowToolbarStyle::UnifiedCompact);
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
mod tests {
    use quarry_core::Encoding;
    use std::sync::Arc;
    fn individual_edit_app(path: &Path) -> QuarryApp {
        let mut app = QuarryApp::new(None, Instant::now());
        app.active.header_mode = HeaderMode::FirstRow;
        app.open_path(path.to_path_buf()).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        app
    }

    fn commit_test_cell(document: &mut Document, row: u64, column: usize, value: &str) {
        let source = document.source_cell(row, column).unwrap().to_vec();
        document.begin_cell_edit(row, column, source).unwrap();
        document.cell_edit.as_mut().unwrap().draft = value.into();
        document.commit_cell_edit();
    }

    #[test]
    fn individual_edits_undo_repeated_cells_headers_and_multiline_values_in_order() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("individual.csv");
        let original = b"\xef\xbb\xbfname,value\r\nfirst,1\r\n";
        fs::write(&source, original).unwrap();
        let mut app = individual_edit_app(&source);
        let document = app.active.document.as_mut().unwrap();
        commit_test_cell(document, 1, 1, "line one\r\nline two,\"quoted\"");
        document.rename_header(1, "amount".into()).unwrap();
        commit_test_cell(document, 1, 1, "last");
        commit_test_cell(document, 1, 1, "1");
        assert!(document.cell_edits.is_empty());

        for expected in [Some("last"), Some("line one\r\nline two,\"quoted\"")] {
            app.swap_edit_history(false).unwrap();
            assert_eq!(
                app.active
                    .document
                    .as_ref()
                    .unwrap()
                    .cell_edits
                    .get(&(1, 1))
                    .map(|value| value.as_slice()),
                expected.map(str::as_bytes),
            );
        }
        app.swap_edit_history(false).unwrap();
        assert_eq!(
            app.active.document.as_ref().unwrap().column_name(1),
            "value"
        );
        app.swap_edit_history(false).unwrap();
        assert!(!app.active.document.as_ref().unwrap().is_dirty());
        assert!(!app.active.document.as_ref().unwrap().can_undo());

        for _ in 0..4 {
            app.swap_edit_history(true).unwrap();
        }
        let document = app.active.document.as_mut().unwrap();
        assert_eq!(document.column_name(1), "amount");
        assert!(document.cell_edits.is_empty());
        assert!(!document.can_redo());
        document.rename_header(1, "value".into()).unwrap();
        assert!(!document.is_dirty());
        app.swap_edit_history(false).unwrap();
        assert_eq!(
            app.active.document.as_ref().unwrap().column_name(1),
            "amount"
        );
        assert_eq!(fs::read(&source).unwrap(), original);
    }

    #[test]
    fn individual_edit_noops_and_cancel_preserve_redo_but_new_edits_branch() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("branch.csv");
        fs::write(&source, b"name,value\nfirst,1\n").unwrap();
        let mut app = individual_edit_app(&source);
        commit_test_cell(app.active.document.as_mut().unwrap(), 1, 1, "2");
        app.active
            .document
            .as_mut()
            .unwrap()
            .rename_header(0, "renamed".into())
            .unwrap();
        app.swap_edit_history(false).unwrap();
        let document = app.active.document.as_mut().unwrap();
        let retained_bytes = document.edit_history.bytes;
        document.rename_header(0, "name".into()).unwrap();
        commit_test_cell(document, 1, 1, "2");
        let ctx = egui::Context::default();
        let _ = render_grid(&ctx, document);
        document.begin_cell_edit(1, 1, b"1".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "cancelled".into();
        let _ = render_grid(&ctx, document);
        press_grid_key(&ctx, document, egui::Key::Escape, egui::Modifiers::NONE);
        assert!(document.cell_edit.is_none());
        assert!(document.can_redo());
        assert_eq!(document.edit_history.bytes, retained_bytes);

        commit_test_cell(document, 1, 1, "branch");
        assert!(!document.can_redo());
        app.swap_edit_history(false).unwrap();
        assert_eq!(
            app.active.document.as_ref().unwrap().cell_edits[&(1, 1)].as_slice(),
            b"2"
        );
        app.swap_edit_history(false).unwrap();
        assert!(!app.active.document.as_ref().unwrap().is_dirty());

        let document = app.active.document.as_mut().unwrap();
        document.start_find_next(b"first").unwrap();
        finish_search(document);
        document
            .replace_current_match(b"first", b"replacement")
            .unwrap();
        finish_search(document);
        assert!(!document.can_redo());
        assert_eq!(document.cell_edits[&(1, 0)].as_slice(), b"replacement");
        app.swap_edit_history(false).unwrap();
        assert!(!app.active.document.as_ref().unwrap().is_dirty());
    }

    #[test]
    fn individual_edit_history_crosses_the_adjacent_structural_boundary_in_both_directions() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("structural.csv");
        let original = b"email,city\na@b,x\n";
        fs::write(&source, original).unwrap();
        let mut app = individual_edit_app(&source);
        let document = app.active.document.as_mut().unwrap();
        document.rename_header(1, "town".into()).unwrap();
        commit_test_cell(document, 1, 0, "alpha@beta");
        document.start_split(0, b"@".to_vec()).unwrap();
        finish_structural_edit(&mut app);
        commit_test_cell(app.active.document.as_mut().unwrap(), 1, 1, "changed\npart");

        app.swap_edit_history(false).unwrap();
        assert!(app.active.document.as_ref().unwrap().cell_edits.is_empty());
        app.swap_edit_history(false).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(document.total_columns, 2);
        assert_eq!(document.column_name(1), "town");
        assert_eq!(document.cell_edits[&(1, 0)].as_slice(), b"alpha@beta");
        app.swap_edit_history(false).unwrap();
        app.swap_edit_history(false).unwrap();
        assert!(!app.active.document.as_ref().unwrap().is_dirty());

        app.swap_edit_history(true).unwrap();
        app.swap_edit_history(true).unwrap();
        app.swap_edit_history(true).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        assert_eq!(app.active.document.as_ref().unwrap().total_columns, 3);
        app.swap_edit_history(true).unwrap();
        assert_eq!(
            app.active.document.as_ref().unwrap().cell_edits[&(1, 1)].as_slice(),
            b"changed\npart"
        );
        assert!(!app.active.document.as_ref().unwrap().can_redo());
        assert_eq!(fs::read(&source).unwrap(), original);

        app.swap_edit_history(false).unwrap();
        app.swap_edit_history(false).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        app.active
            .document
            .as_mut()
            .unwrap()
            .rename_header(1, "new branch".into())
            .unwrap();
        assert!(!app.active.document.as_ref().unwrap().can_redo());
        assert!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .working_copy
                .as_ref()
                .unwrap()
                .redo
                .is_none()
        );
    }

    #[test]
    fn individual_edit_redo_branch_stays_obsolete_after_a_new_structural_change() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("structural-branch.csv");
        fs::write(&source, b"email,city\na@b,x\n").unwrap();
        let mut app = individual_edit_app(&source);
        commit_test_cell(app.active.document.as_mut().unwrap(), 1, 0, "alpha@beta");
        app.active
            .document
            .as_mut()
            .unwrap()
            .rename_header(1, "obsolete".into())
            .unwrap();
        app.swap_edit_history(false).unwrap();
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_split(0, b"@".to_vec())
            .unwrap();
        finish_structural_edit(&mut app);
        app.swap_edit_history(false).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        assert!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .edit_history
                .redo
                .is_empty()
        );
        assert_eq!(app.active.document.as_ref().unwrap().column_name(1), "city");
        app.swap_edit_history(false).unwrap();
        assert!(!app.active.document.as_ref().unwrap().is_dirty());
        app.swap_edit_history(true).unwrap();
        assert_eq!(
            app.active.document.as_ref().unwrap().cell_edits[&(1, 0)].as_slice(),
            b"alpha@beta"
        );
        app.swap_edit_history(true).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        assert_eq!(app.active.document.as_ref().unwrap().column_name(2), "city");
        assert!(!app.active.document.as_ref().unwrap().can_redo());
    }

    #[test]
    fn individual_edit_history_limits_entries_and_bytes_without_skipping_untracked_edits() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("limits.csv");
        fs::write(&source, b"name\nvalue\n").unwrap();
        let mut app = individual_edit_app(&source);
        for edit in 0..=super::MAX_EDIT_HISTORY_ENTRIES {
            app.active
                .document
                .as_mut()
                .unwrap()
                .rename_header(0, format!("name {edit}"))
                .unwrap();
        }
        assert_eq!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .edit_history
                .undo
                .len(),
            super::MAX_EDIT_HISTORY_ENTRIES
        );
        for _ in 0..super::MAX_EDIT_HISTORY_ENTRIES {
            app.swap_edit_history(false).unwrap();
        }
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(document.column_name(0), "name 0");
        assert!(!document.can_undo());
        assert_eq!(
            document.edit_history.redo.len(),
            super::MAX_EDIT_HISTORY_ENTRIES
        );

        app.apply(&egui::Context::default(), Action::DiscardChanges);
        let value_bytes = 6 * 1024 * 1024;
        let document = app.active.document.as_mut().unwrap();
        document.rename_header(0, "a".repeat(value_bytes)).unwrap();
        document.rename_header(0, "b".repeat(value_bytes)).unwrap();
        assert_eq!(document.edit_history.undo.len(), 1);
        assert_eq!(document.edit_history.bytes, 2 * value_bytes);
        assert!(document.edit_history.bytes <= super::MAX_EDIT_HISTORY_BYTES);
        app.swap_edit_history(false).unwrap();
        assert!(!app.active.document.as_ref().unwrap().can_undo());
        assert_eq!(
            app.active.document.as_ref().unwrap().edit_history.bytes,
            2 * value_bytes
        );
        app.swap_edit_history(true).unwrap();
        let document = app.active.document.as_mut().unwrap();
        document
            .rename_header(0, "x".repeat(super::MAX_EDIT_HISTORY_BYTES + 1))
            .unwrap();
        assert!(!document.can_undo());
        assert!(!document.can_redo());
        assert_eq!(document.edit_history.bytes, 0);
        assert!(document.is_dirty());
        assert_eq!(fs::read(&source).unwrap(), b"name\nvalue\n");

        app.apply(&egui::Context::default(), Action::DiscardChanges);
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_split(0, b"a".to_vec())
            .unwrap();
        finish_structural_edit(&mut app);
        app.active
            .document
            .as_mut()
            .unwrap()
            .rename_header(0, "x".repeat(super::MAX_EDIT_HISTORY_BYTES + 1))
            .unwrap();
        assert!(
            !app.active.document.as_ref().unwrap().can_undo(),
            "structural Undo must not jump over an untracked edit"
        );
        assert!(app.swap_edit_history(false).is_err());
    }

    #[test]
    fn individual_edit_history_resets_only_after_successful_document_lifecycle_changes() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("lifecycle.csv");
        let saved_copy = directory.path().join("saved.csv");
        let other = directory.path().join("other.csv");
        fs::write(&source, b"name,value\nfirst,1\n").unwrap();
        fs::write(&other, b"other,value\nsecond,2\n").unwrap();
        let mut app = individual_edit_app(&source);
        commit_test_cell(app.active.document.as_mut().unwrap(), 1, 1, "2");
        app.open_path(other.clone()).unwrap();
        app.open_path(source.clone()).unwrap();
        assert!(app.active.document.as_ref().unwrap().can_undo());
        app.save_as_picker_result(None);
        assert!(app.active.document.as_ref().unwrap().can_undo());

        let ctx = egui::Context::default();
        let mut frame = eframe::Frame::_new_kittest();
        if app
            .active
            .document
            .as_mut()
            .unwrap()
            .start_save_as(directory.path().join("missing/output.csv"))
            .is_ok()
        {
            let _ = finish_app_save(&mut app, &ctx, &mut frame);
        }
        assert_eq!(app.active.document.as_ref().unwrap().logical_path, source);
        assert!(app.active.document.as_ref().unwrap().can_undo());
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_save_as(saved_copy.clone())
            .unwrap();
        assert!(!app.active.document.as_ref().unwrap().can_undo());
        let _ = finish_app_save(&mut app, &ctx, &mut frame);
        let document = app.active.document.as_mut().unwrap();
        assert_eq!(document.logical_path, saved_copy);
        assert!(!document.can_undo());
        assert!(!document.can_redo());
        assert_eq!(fs::read(&source).unwrap(), b"name,value\nfirst,1\n");
        assert_eq!(fs::read(&saved_copy).unwrap(), b"name,value\nfirst,2\n");

        commit_test_cell(document, 1, 1, "3");
        document.start_save().unwrap();
        let _ = finish_app_save(&mut app, &ctx, &mut frame);
        assert!(!app.active.document.as_ref().unwrap().can_undo());
        assert_eq!(fs::read(&saved_copy).unwrap(), b"name,value\nfirst,3\n");
        commit_test_cell(app.active.document.as_mut().unwrap(), 1, 1, "4");
        app.apply(&ctx, Action::DiscardChanges);
        assert!(!app.active.document.as_ref().unwrap().can_undo());
        assert!(!app.active.document.as_ref().unwrap().can_redo());

        commit_test_cell(app.active.document.as_mut().unwrap(), 1, 1, "5");
        app.swap_edit_history(false).unwrap();
        assert!(!app.active.document.as_ref().unwrap().is_dirty());
        assert!(app.open_path(directory.path().join("missing.csv")).is_err());
        assert!(app.active.document.as_ref().unwrap().can_redo());
        app.open_path(other.clone()).unwrap();
        assert_eq!(app.active.document.as_ref().unwrap().logical_path, other);
        assert!(!app.active.document.as_ref().unwrap().can_undo());
        assert!(!app.active.document.as_ref().unwrap().can_redo());
    }

    #[test]
    fn individual_edit_history_freezes_after_a_source_conflict() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("conflict.csv");
        for action in ["undo", "redo", "save"] {
            fs::write(&source, b"name,value\nfirst,1\n").unwrap();
            let mut app = individual_edit_app(&source);
            commit_test_cell(app.active.document.as_mut().unwrap(), 1, 1, "2");
            if action == "redo" {
                app.swap_edit_history(false).unwrap();
            }
            let before = app.active.document.as_ref().unwrap().cell_edits.clone();
            fs::write(&source, b"name,value\nexternal,changed\n").unwrap();
            if action == "save" {
                assert!(app.active.document.as_mut().unwrap().start_save().is_err());
            } else {
                assert!(app.swap_edit_history(action == "redo").is_err());
            }
            assert!(app.active.document.as_ref().unwrap().source_changed);
            assert!(!app.active.document.as_ref().unwrap().can_undo());
            assert!(!app.active.document.as_ref().unwrap().can_redo());
            assert_eq!(app.active.document.as_ref().unwrap().cell_edits, before);
            assert_eq!(
                fs::read(&source).unwrap(),
                b"name,value\nexternal,changed\n"
            );
        }
    }

    fn individual_edit_key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    fn individual_edit_frame(
        app: &mut QuarryApp,
        ctx: &egui::Context,
        time: &mut f64,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        *time += 2.0;
        let modifiers = events
            .iter()
            .find_map(|event| match event {
                egui::Event::Key { modifiers, .. } => Some(*modifiers),
                _ => None,
            })
            .unwrap_or_default();
        ctx.run(
            egui::RawInput {
                time: Some(*time),
                events,
                modifiers,
                ..grid_input()
            },
            |ctx| {
                eframe::App::update(app, ctx, &mut eframe::Frame::_new_kittest());
            },
        )
    }

    #[test]
    fn storage_review_keeps_grid_visible_without_accepting_background_input() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("review.csv");
        let original = format!(
            "name,value\n{}",
            (0..100)
                .map(|row| format!("row_{row},{row}\n"))
                .collect::<String>()
        );
        fs::write(&source, &original).unwrap();
        let mut app = individual_edit_app(&source);
        commit_test_cell(app.active.document.as_mut().unwrap(), 1, 1, "edited");
        app.active.document.as_mut().unwrap().selection =
            Some(GridSelection::Cell { row: 1, column: 0 });
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut time = 0.0;
        let output = individual_edit_frame(&mut app, &ctx, &mut time, vec![]);
        let row_target = accessible_button(&output, "Select row 2").0;
        let undo_target = accessible_button(&output, "Undo Change").0;
        let position = app.active.document.as_ref().unwrap().grid_position();
        app.begin_storage_review(super::PendingStorageOperation::Structural(
            super::StructuralDialog::sort(0),
        ));
        let output = individual_edit_frame(
            &mut app,
            &ctx,
            &mut time,
            vec![
                individual_edit_key(egui::Key::F, egui::Modifiers::COMMAND),
                individual_edit_key(egui::Key::S, egui::Modifiers::COMMAND),
                individual_edit_key(egui::Key::Z, egui::Modifiers::COMMAND),
                individual_edit_key(egui::Key::PageDown, egui::Modifiers::NONE),
                egui::Event::Copy,
                egui::Event::AccessKitActionRequest(egui::accesskit::ActionRequest {
                    action: egui::accesskit::Action::ShowContextMenu,
                    target: row_target,
                    data: None,
                }),
                egui::Event::AccessKitActionRequest(egui::accesskit::ActionRequest {
                    action: egui::accesskit::Action::Click,
                    target: undo_target,
                    data: None,
                }),
                egui::Event::PointerMoved(egui::pos2(150.0, 300.0)),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -100.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert!(app.active.storage_review.is_some());
        assert!(!app.active.find_bar_open);
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(document.grid_position(), position);
        assert_eq!(document.cell_edits[&(1, 1)].as_slice(), b"edited");
        assert_eq!(
            document.selection,
            Some(GridSelection::Cell { row: 1, column: 0 })
        );
        assert!(document.selected_rows.is_empty());
        assert!(document.save_job.is_none());
        assert!(document.structural_job.is_none());
        assert!(accessible_button(&output, "Select row 2").1.is_disabled());
        assert!(
            output.shapes.iter().any(|shape| {
                matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text() == "row_0")
            }),
            "the grid must remain painted behind the storage review"
        );
        assert!(
            !output
                .platform_output
                .commands
                .iter()
                .any(|command| { matches!(command, egui::OutputCommand::CopyText(_)) })
        );
        assert_eq!(fs::read_to_string(&source).unwrap(), original);
        app.active.document.as_mut().unwrap().shutdown();
    }

    #[test]
    fn individual_edit_buttons_and_shortcuts_leave_typing_undo_inside_each_editor_session() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("keyboard.csv");
        fs::write(&source, b"name,value\nfirst,1\n").unwrap();
        let mut app = individual_edit_app(&source);
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut time = 0.0;
        let command = egui::Modifiers::COMMAND;
        let redo = egui::Modifiers {
            shift: true,
            ..command
        };
        commit_test_cell(app.active.document.as_mut().unwrap(), 1, 1, "2");
        let output = individual_edit_frame(&mut app, &ctx, &mut time, vec![]);
        let (undo_id, undo_node) = accessible_button(&output, "Undo Change");
        assert!(!undo_node.is_disabled());
        let _ = individual_edit_frame(
            &mut app,
            &ctx,
            &mut time,
            vec![egui::Event::AccessKitActionRequest(
                egui::accesskit::ActionRequest {
                    action: egui::accesskit::Action::Click,
                    target: undo_id,
                    data: None,
                },
            )],
        );
        assert!(!app.active.document.as_ref().unwrap().is_dirty());
        let output = individual_edit_frame(&mut app, &ctx, &mut time, vec![]);
        let (redo_id, redo_node) = accessible_button(&output, "Redo Change");
        assert!(!redo_node.is_disabled());
        let _ = individual_edit_frame(
            &mut app,
            &ctx,
            &mut time,
            vec![egui::Event::AccessKitActionRequest(
                egui::accesskit::ActionRequest {
                    action: egui::accesskit::Action::Click,
                    target: redo_id,
                    data: None,
                },
            )],
        );
        assert_eq!(
            app.active.document.as_ref().unwrap().cell_edits[&(1, 1)].as_slice(),
            b"2"
        );
        let _ = individual_edit_frame(
            &mut app,
            &ctx,
            &mut time,
            vec![individual_edit_key(egui::Key::Z, command)],
        );
        assert!(!app.active.document.as_ref().unwrap().is_dirty());
        let _ = individual_edit_frame(
            &mut app,
            &ctx,
            &mut time,
            vec![individual_edit_key(egui::Key::Z, redo)],
        );
        assert_eq!(
            app.active.document.as_ref().unwrap().cell_edits[&(1, 1)].as_slice(),
            b"2"
        );

        for header in [false, true] {
            let before = if header { "name" } else { "2" };
            let document = app.active.document.as_mut().unwrap();
            let history_len = document.edit_history.undo.len();
            if header {
                document.begin_header_edit(0);
            } else {
                document.begin_cell_edit(1, 1, b"1".to_vec()).unwrap();
            }
            let _ = individual_edit_frame(&mut app, &ctx, &mut time, vec![]);
            let _ = individual_edit_frame(
                &mut app,
                &ctx,
                &mut time,
                vec![
                    individual_edit_key(egui::Key::A, command),
                    egui::Event::Text("typed value".into()),
                ],
            );
            let _ = individual_edit_frame(
                &mut app,
                &ctx,
                &mut time,
                vec![individual_edit_key(egui::Key::Z, command)],
            );
            let document = app.active.document.as_ref().unwrap();
            let draft = if header {
                &document.header_edit.as_ref().unwrap().draft
            } else {
                &document.cell_edit.as_ref().unwrap().draft
            };
            assert_eq!(draft, before);
            assert_eq!(document.edit_history.undo.len(), history_len);
            let _ = individual_edit_frame(
                &mut app,
                &ctx,
                &mut time,
                vec![individual_edit_key(egui::Key::Z, redo)],
            );
            let _ = individual_edit_frame(
                &mut app,
                &ctx,
                &mut time,
                vec![individual_edit_key(egui::Key::Enter, egui::Modifiers::NONE)],
            );
            let document = app.active.document.as_mut().unwrap();
            assert_eq!(document.edit_history.undo.len(), history_len + 1);
            if header {
                document.begin_header_edit(0);
            } else {
                document.begin_cell_edit(1, 1, b"1".to_vec()).unwrap();
            }
            let _ = individual_edit_frame(&mut app, &ctx, &mut time, vec![]);
            let _ = individual_edit_frame(
                &mut app,
                &ctx,
                &mut time,
                vec![individual_edit_key(egui::Key::Z, command)],
            );
            let document = app.active.document.as_ref().unwrap();
            let draft = if header {
                &document.header_edit.as_ref().unwrap().draft
            } else {
                &document.cell_edit.as_ref().unwrap().draft
            };
            assert_eq!(
                draft, "typed value",
                "typing history must reset when an editor reopens"
            );
            assert_eq!(document.edit_history.undo.len(), history_len + 1);
            let _ = individual_edit_frame(
                &mut app,
                &ctx,
                &mut time,
                vec![individual_edit_key(
                    egui::Key::Escape,
                    egui::Modifiers::NONE,
                )],
            );
            let _ = individual_edit_frame(&mut app, &ctx, &mut time, vec![]);
            let _ = individual_edit_frame(
                &mut app,
                &ctx,
                &mut time,
                vec![individual_edit_key(egui::Key::Z, command)],
            );
            let document = app.active.document.as_ref().unwrap();
            assert_eq!(document.edit_history.undo.len(), history_len);
            if header {
                assert_eq!(document.column_name(0), before);
            } else {
                assert_eq!(document.cell_edits[&(1, 1)].as_slice(), before.as_bytes());
            }
        }
        assert_eq!(fs::read(&source).unwrap(), b"name,value\nfirst,1\n");
    }
    use std::collections::BTreeSet;
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::Path;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use eframe::egui;

    use super::{
        Action, ActiveJobDisplay, ActiveJobKind, AppMessage, CaseSensitivity, ColumnCommand,
        ColumnView, DelimiterMode, Document, EguiGridState, FIND_INPUT_ID, FilterOperator,
        FilterPredicate, FilterProgress, FilterQuery, GridColumnRequest, GridSelection, HeaderMode,
        IndexConfig, MAX_RENDERED_COLUMNS, MessageSeverity, OpenOptions, QuarryApp,
        REPLACE_INPUT_ID, ROW_NUMBER_WIDTH, SOURCE_CHANGED_NOTICE, SearchMatch, Session, SortMode,
        StructuralDialog, StructuralDialogAction, WorkingCopyState, active_job_controls,
        column_ruler_divider_stroke, column_selection_fill, configure_style,
        estimate_sort_temporary_bytes, filter_button_label, filtered_export_file_name,
        first_active_job, notice_strip, page_controls, parse_move_position, rendered_column_range,
        save_as_file_name, search_controls, select_column, selected_split_column,
        show_column_manager, show_empty_state, show_filter_manager, show_grid,
        show_grid_with_filter_case, show_grid_with_view, show_structural_dialog,
    };

    #[test]
    fn numeric_detection_accepts_exponents_and_rejects_malformed_values() {
        for value in ["1e3", "-2.5E-4", "6.02e+23", "$1,204.50", "42%", " 7 "] {
            assert!(
                super::looks_numeric(value),
                "{value} should align as a number"
            );
        }
        for value in [
            "1e",
            "e3",
            "1e3.5",
            "1ee3",
            "1e12345678",
            "2025-12-13",
            "ABC",
            "",
        ] {
            assert!(!super::looks_numeric(value), "{value} should align as text");
        }
        // Classification uses the source value, so edge spaces don't hide a number.
        assert!(super::source_looks_numeric(b" 1e3 "));
        assert!(super::source_looks_numeric(b"\t42\t"));
        assert!(!super::source_looks_numeric(&[0xff, b'1']));
        assert!(!super::source_looks_numeric("1".repeat(65).as_bytes()));
    }

    #[test]
    fn menu_undo_follows_a_focused_text_field() {
        let app = QuarryApp::new(None, Instant::now());
        let undo = super::menu::MenuCommand::Undo;
        let redo = super::menu::MenuCommand::Redo;
        // No document history: Undo and Redo are unavailable...
        let state = app.menu_state(false);
        assert!(!state.enabled(undo) && !state.enabled(redo));
        // ...until a text field owns the typing history.
        let state = app.menu_state(true);
        assert!(state.enabled(undo) && state.enabled(redo));
        assert!(!state.enabled(super::menu::MenuCommand::Save));
    }

    #[test]
    fn file_commands_respect_join_reservations_and_write_slots() {
        use super::menu::MenuCommand as M;
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("file-commands.csv");
        std::fs::write(&source, "id,name\n1,a\n").unwrap();
        let mut document = Document::open(&source, OpenOptions::default()).unwrap();
        finish_index(&mut document);
        let mut app = QuarryApp::new(None, Instant::now());
        app.active.document = Some(document);
        assert!(app.menu_state(false).enabled(M::ReloadFromDisk));
        assert!(app.menu_state(false).enabled(M::CombineFiles));

        // Join Files holds the document: Reload would only show a warning.
        let reservations =
            std::sync::Arc::clone(&app.active.document.as_ref().unwrap().join_reservations);
        reservations.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        assert!(!app.menu_state(false).enabled(M::ReloadFromDisk));
        reservations.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        assert!(app.menu_state(false).enabled(M::ReloadFromDisk));

        // Combine needs only a free write slot.
        let document = app.active.document.as_ref();
        assert!(super::FileCommands::new(document, true).combine);
        assert!(!super::FileCommands::new(document, false).combine);
        assert!(!super::FileCommands::new(None, false).combine);
        assert!(super::FileCommands::new(None, true).combine);
        app.active.document.as_mut().unwrap().shutdown();
    }

    #[test]
    fn theme_stays_light_with_soft_yellow_accents_when_the_system_is_dark() {
        let ctx = egui::Context::default();
        configure_style(&ctx);
        let input = egui::RawInput {
            system_theme: Some(egui::Theme::Dark),
            ..Default::default()
        };
        let _ = ctx.run(input, |_| {});

        let palette = &super::theme::LIGHT;
        let style = ctx.style();
        assert_eq!(ctx.theme(), egui::Theme::Light);
        assert!(!style.visuals.dark_mode);
        assert_eq!(style.visuals.selection.bg_fill, palette.selection);
        assert_eq!(style.visuals.selection.stroke.color, palette.text);
        assert_eq!(style.visuals.hyperlink_color, palette.accent_text);
        assert_eq!(style.visuals.panel_fill, palette.chrome);
        assert_eq!(super::theme::ctx_palette(&ctx), palette);
    }

    #[test]
    fn about_blocks_dropped_files_and_defers_native_open_requests() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original.csv");
        let replacement = directory.path().join("replacement.csv");
        fs::write(&original, b"name,value\nfirst,1\n").unwrap();
        fs::write(&replacement, b"name,value\nsecond,2\n").unwrap();
        for editing in [false, true] {
            let ctx = egui::Context::default();
            let mut app = QuarryApp::new(Some(original.clone()), Instant::now());
            finish_index(app.active.document.as_mut().unwrap());
            let _ = ctx.run(grid_input(), |ctx| {
                eframe::App::update(&mut app, ctx, &mut eframe::Frame::_new_kittest());
            });
            if editing {
                let document = app.active.document.as_mut().unwrap();
                document.begin_cell_edit(1, 1, b"1".to_vec()).unwrap();
                document.cell_edit.as_mut().unwrap().draft = "unsaved".into();
            }
            app.apply(&ctx, Action::About);
            #[cfg(target_os = "macos")]
            {
                let (sender, receiver) = std::sync::mpsc::channel();
                app.open_document_receiver = Some(receiver);
                sender.send(replacement.clone()).unwrap();
            }
            let _ = ctx.run(
                egui::RawInput {
                    dropped_files: vec![egui::DroppedFile {
                        path: Some(replacement.clone()),
                        ..Default::default()
                    }],
                    ..grid_input()
                },
                |ctx| eframe::App::update(&mut app, ctx, &mut eframe::Frame::_new_kittest()),
            );
            let document = app.active.document.as_ref().unwrap();
            assert_eq!(document.logical_path, original);
            assert!(app.active.notice.is_none());
            if editing {
                assert_eq!(document.cell_edit.as_ref().unwrap().draft, "unsaved");
                assert!(document.cell_edits.is_empty());
            }

            #[cfg(target_os = "macos")]
            if !editing {
                app.about.open = false;
                let _ = ctx.run(grid_input(), |ctx| {
                    eframe::App::update(&mut app, ctx, &mut eframe::Frame::_new_kittest());
                });
                assert_eq!(
                    app.active.document.as_ref().unwrap().logical_path,
                    replacement
                );
            }
        }
    }

    #[test]
    fn about_feedback_is_accessible_and_preserves_document_edits() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("private-example.csv");
        fs::write(&source, b"name,value\nprivate-value,1\n").unwrap();
        for with_document in [false, true] {
            let mut app = QuarryApp::new(None, Instant::now());
            if with_document {
                app.open_path(source.clone()).unwrap();
                finish_index(app.active.document.as_mut().unwrap());
                commit_test_cell(app.active.document.as_mut().unwrap(), 1, 1, "2");
            }
            let ctx = egui::Context::default();
            ctx.enable_accesskit();
            configure_style(&ctx);
            let mut time = 0.0;
            let mut render = |app: &mut QuarryApp, events| {
                time += 2.0;
                ctx.run(
                    egui::RawInput {
                        time: Some(time),
                        events,
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(860.0, 540.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| eframe::App::update(app, ctx, &mut eframe::Frame::_new_kittest()),
                )
            };
            let click = |target| {
                egui::Event::AccessKitActionRequest(egui::accesskit::ActionRequest {
                    action: egui::accesskit::Action::Click,
                    target,
                    data: None,
                })
            };
            let output = render(&mut app, vec![]);
            let menu_label = if with_document {
                "File menu: private-example.csv"
            } else {
                "File menu"
            };
            let (menu, _) = accessible_button(&output, menu_label);
            render(&mut app, vec![click(menu)]);
            let output = render(&mut app, vec![]);
            let (about, _) = accessible_button(&output, "About & Feedback…");
            render(&mut app, vec![click(about)]);
            assert!(app.about.open);
            let output = render(&mut app, vec![]);
            let (copy, _) = accessible_button(&output, "Copy build details");
            let output = render(&mut app, vec![click(copy)]);
            let copied = output
                .platform_output
                .commands
                .iter()
                .find_map(|command| {
                    if let egui::OutputCommand::CopyText(text) = command {
                        Some(text)
                    } else {
                        None
                    }
                })
                .expect("copying build details should populate the clipboard");
            assert!(copied.contains(env!("CARGO_PKG_VERSION")));
            assert!(copied.contains("Build: source build"));
            assert!(!copied.contains("private"));

            for (label, url) in [
                (
                    "Report a bug or share feedback",
                    "https://github.com/danchamorro/quarry/issues/new?template=bug_report.yml",
                ),
                (
                    "User guide",
                    "https://github.com/danchamorro/quarry/blob/main/docs/USER_GUIDE.md",
                ),
            ] {
                let output = render(&mut app, vec![]);
                let (button, node) = accessible_button(&output, label);
                let bounds = node.bounds().expect("action has screen bounds");
                assert!(bounds.x0 >= 0.0 && bounds.x1 <= 860.0 && bounds.y1 <= 540.0);
                let output = render(&mut app, vec![click(button)]);
                assert!(output.platform_output.commands.iter().any(|command| {
                    matches!(command, egui::OutputCommand::OpenUrl(open) if open.url == url)
                }));
            }

            // About owns keyboard input; inspecting build details must not save or undo data.
            for key in [egui::Key::S, egui::Key::Z, egui::Key::F] {
                render(
                    &mut app,
                    vec![individual_edit_key(key, egui::Modifiers::COMMAND)],
                );
            }
            assert!(!app.active.find_bar_open);
            if with_document {
                assert_eq!(
                    app.active.document.as_ref().unwrap().cell_edits[&(1, 1)].as_slice(),
                    b"2"
                );
            }
            assert_eq!(fs::read(&source).unwrap(), b"name,value\nprivate-value,1\n");
            render(
                &mut app,
                vec![individual_edit_key(
                    egui::Key::Escape,
                    egui::Modifiers::NONE,
                )],
            );
            assert!(!app.about.open);
        }
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
        app.active.header_mode = HeaderMode::FirstRow;
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

        app.active
            .document
            .as_mut()
            .unwrap()
            .move_column(1, 0)
            .unwrap();
        let output = ctx.run(grid_input_with_width(860.0), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        assert!(!accessible_button(&output, "Save As…").1.is_disabled());
        for label in ["Save", "Discard Changes"] {
            assert!(accessible_button(&output, label).1.is_disabled());
        }
        app.active.document.as_mut().unwrap().columns.reset();
        let output = ctx.run(grid_input_with_width(860.0), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        assert!(accessible_button(&output, "Save As…").1.is_disabled());

        app.active
            .document
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
        assert!(!accessible_button(&output, "Open…").1.is_disabled());
        assert!(
            accessible_button(&output, "Reload from Disk")
                .1
                .is_disabled()
        );
        for label in ["Save", "Save As…", "Discard Changes"] {
            assert!(!accessible_button(&output, label).1.is_disabled());
        }

        app.active.document.as_mut().unwrap().shutdown();
    }

    #[test]
    fn format_menu_is_accessible_and_cancel_discards_its_draft() {
        let mut app = QuarryApp::new(None, Instant::now());
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut frame = eframe::Frame::_new_kittest();
        let mut render = |app: &mut QuarryApp, events| {
            ctx.run(
                egui::RawInput {
                    events,
                    ..grid_input_with_width(860.0)
                },
                |ctx| eframe::App::update(app, ctx, &mut frame),
            )
        };
        let click = |target| {
            vec![egui::Event::AccessKitActionRequest(
                egui::accesskit::ActionRequest {
                    action: egui::accesskit::Action::Click,
                    target,
                    data: None,
                },
            )]
        };
        let chooser = |output: &egui::FullOutput, label: &str| {
            let tree = output.platform_output.accesskit_update.as_ref().unwrap();
            tree.nodes
                .iter()
                .find(|(_, node)| {
                    node.role() == egui::accesskit::Role::ComboBox
                        && node.labelled_by().iter().any(|label_id| {
                            tree.nodes
                                .iter()
                                .any(|(id, node)| id == label_id && node.value() == Some(label))
                        })
                })
                .map(|(id, _)| *id)
                .unwrap_or_else(|| panic!("missing accessible {label} chooser"))
        };
        let output = render(&mut app, vec![]);
        assert!(accessible_button(&output, "Format").1.is_disabled());

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("format-menu.csv");
        fs::write(&path, b"name,value\nfirst,1\n").unwrap();
        app.open_new_path(path).unwrap();

        let output = render(&mut app, vec![]);
        let (format_id, format) = accessible_button(&output, "Format: Auto, Auto");
        assert!(!format.is_disabled());
        let bounds = format.bounds().expect("Format menu should have bounds");
        assert!(bounds.x1 - bounds.x0 <= super::FORMAT_MENU_WIDTH as f64 + 1.0);
        let _ = render(&mut app, click(format_id));
        let output = render(&mut app, vec![]);
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
        let delimiter = chooser(&output, "Delimiter");
        let _ = chooser(&output, "Header");
        let _ = render(&mut app, click(delimiter));
        let _ = render(&mut app, vec![]);
        let _ = render(
            &mut app,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert_eq!(
            app.active.format_draft,
            Some((DelimiterMode::Auto, HeaderMode::Auto, None))
        );
        assert!(
            !egui::Popup::is_any_open(&ctx),
            "Escape closes only the chooser"
        );

        let output = render(&mut app, vec![]);
        let _ = render(&mut app, click(chooser(&output, "Delimiter")));
        let output = render(&mut app, vec![]);
        let tab_position = output
            .platform_output
            .accesskit_update
            .as_ref()
            .unwrap()
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
        let _ = render(
            &mut app,
            vec![
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
        );
        let output = render(&mut app, vec![]);
        assert_eq!(
            app.active.format_draft,
            Some((DelimiterMode::Tab, HeaderMode::Auto, None))
        );
        assert!(
            !egui::Popup::is_any_open(&ctx),
            "selection closes the chooser"
        );
        let (reopen, node) = accessible_button(&output, "Reopen with Changes");
        assert!(!node.is_disabled());
        let _ = render(&mut app, click(reopen));
        assert_eq!(app.active.format_draft, None);
        assert_eq!(app.active.delimiter_mode, DelimiterMode::Tab);
        assert_eq!(app.active.header_mode, HeaderMode::Auto);

        let output = render(&mut app, vec![]);
        let (format_id, _) = accessible_button(&output, "Format: Tab, Auto");
        let _ = render(&mut app, click(format_id));
        let output = render(&mut app, vec![]);
        let _ = render(&mut app, click(chooser(&output, "Header")));
        let output = render(&mut app, vec![]);
        let no_header = output
            .platform_output
            .accesskit_update
            .as_ref()
            .unwrap()
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("No header"))
            .map(|(id, _)| *id)
            .unwrap();
        let _ = render(&mut app, click(no_header));
        let output = render(&mut app, vec![]);
        assert_eq!(
            app.active.format_draft,
            Some((DelimiterMode::Tab, HeaderMode::NoHeader, None))
        );
        assert!(
            !egui::Popup::is_any_open(&ctx),
            "accessible selection closes the chooser"
        );
        let cancel = accessible_button(&output, "Cancel").0;
        let _ = render(&mut app, click(cancel));
        assert_eq!(app.active.format_draft, None);
        assert_eq!(app.active.delimiter_mode, DelimiterMode::Tab);
        assert_eq!(app.active.header_mode, HeaderMode::Auto);

        app.active.delimiter_mode = DelimiterMode::Semicolon;
        app.active.header_mode = HeaderMode::FirstRow;
        for width in [860.0, 1280.0] {
            let output = ctx.run(grid_input_with_width(width), |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            });
            let (_, format) = accessible_button(&output, "Format: Semicolon, Header row");
            let bounds = format.bounds().expect("Format menu should have bounds");
            assert!(bounds.x1 - bounds.x0 <= super::FORMAT_MENU_WIDTH as f64 + 1.0);
        }
        app.active.document.as_mut().unwrap().shutdown();
    }

    #[test]
    fn toolbar_stays_single_row_and_in_bounds_at_supported_widths() {
        let central_panel_top = |output: &egui::FullOutput, width: f32| {
            output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::epaint::Shape::Rect(rect)
                        if rect.fill == super::theme::LIGHT.canvas
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
                    bounds[6].x1 <= row_input.x0,
                    "Find ended at {}, Row input started at {}",
                    bounds[6].x1,
                    row_input.x0
                );
                assert!(
                    row_input.x1 <= bounds[7].x0,
                    "Row input ended at {}, Go started at {}",
                    row_input.x1,
                    bounds[7].x0
                );
                assert!(((row_input.y0 + row_input.y1) / 2.0 - first_center).abs() < 1.0);

                let height =
                    egui::containers::panel::PanelState::load(ctx, egui::Id::new("quarry-toolbar"))
                        .expect("toolbar panel state should be stored")
                        .size()
                        .y;
                assert!(
                    (height - super::TOOLBAR_HEIGHT).abs() < 0.1,
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
            "Undo Change",
            "Redo Change",
            "Columns…",
            "Filters…",
            "Find",
            "Go",
            "Page Up",
            "Page Down",
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
        let document = app.active.document.as_mut().unwrap();
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
            "Undo Change",
            "Redo Change",
            "Columns…",
            "Filters…",
            "Find",
            "Go",
            "Page Up",
            "Page Down",
        ];
        for (index, width) in widths.into_iter().enumerate() {
            let output = ctx.run(grid_input_with_width(width), |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            });
            assert_eq!(
                assert_toolbar(&ctx, &output, &document_labels, width),
                empty_tops[index] + super::tabs::TAB_STRIP_HEIGHT
            );
            assert!(
                app.active
                    .document
                    .as_ref()
                    .unwrap()
                    .job
                    .as_ref()
                    .is_some_and(|job| !job.progress().done),
                "indexing should remain active during layout assertions"
            );
        }
        finish_index(app.active.document.as_mut().unwrap());

        for (index, width) in widths.into_iter().enumerate() {
            let output = ctx.run(grid_input_with_width(width), |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            });
            assert_eq!(
                assert_toolbar(&ctx, &output, &document_labels, width),
                empty_tops[index] + super::tabs::TAB_STRIP_HEIGHT
            );
        }

        app.active
            .document
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
                empty_tops[index] + super::tabs::TAB_STRIP_HEIGHT
            );
        }

        app.active.document.as_mut().unwrap().filter_query = Some(FilterQuery {
            predicates: (0..12)
                .map(|_| FilterPredicate {
                    column: 0,
                    operator: FilterOperator::Equals,
                    value: b"first".to_vec(),
                    upper_bound: None,
                })
                .collect(),
            case_sensitivity: CaseSensitivity::Insensitive,
        });
        let filtered_labels = [
            document_label.as_str(),
            "Format: Auto, Auto",
            "Undo Change",
            "Redo Change",
            "Columns…",
            "Filters (12)…",
            "Find",
            "Go",
            "Page Up",
            "Page Down",
        ];
        for (index, width) in widths.into_iter().enumerate() {
            let output = ctx.run(grid_input_with_width(width), |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            });
            assert_eq!(
                assert_toolbar(&ctx, &output, &filtered_labels, width),
                empty_tops[index] + super::tabs::TAB_STRIP_HEIGHT
            );
            assert!(accessible_button(&output, "Find").1.is_disabled());
        }

        app.active.document.as_mut().unwrap().shutdown();
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
                        if (rect.fill, rect.stroke)
                            == super::empty_state_colors(
                                super::theme::ctx_palette(&ctx),
                                false,
                            ) =>
                    {
                        Some(rect.rect)
                    }
                    _ => None,
                })
                .expect("empty-state drop zone should be painted");
            assert!((drop_rect.center().x - available.center().x).abs() < 1.0);
            assert!((drop_rect.center().y - available.center().y).abs() < 1.0);
            assert!(
                (drop_rect.width() - available.width().min(super::EMPTY_STATE_SIZE.x)).abs() < 1.0
            );
            assert!(
                (drop_rect.height() - available.height().min(super::EMPTY_STATE_SIZE.y)).abs()
                    < 1.0
            );
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
        assert!(
            app.active.document.is_none(),
            "hovering must not open a file"
        );
        assert!(hovered.shapes.iter().any(|shape| {
            matches!(
                &shape.shape,
                egui::epaint::Shape::Rect(rect)
                    if (rect.fill, rect.stroke)
                        == super::empty_state_colors(super::theme::ctx_palette(&hover_ctx), true)
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
        assert_eq!(app.active.document.as_ref().unwrap().session.path(), path);
        app.active.document.as_mut().unwrap().shutdown();
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
        let failed_open = app.active.notice.as_ref().unwrap().severity;

        app.save_current();
        let blocked_save = app.active.notice.as_ref().unwrap().severity;

        app.copy_selection(&egui::Context::default());
        let blocked_copy = app.active.notice.as_ref().unwrap().severity;

        app.handle_dropped_paths(vec![None]);
        let ignored_drop = app.active.notice.as_ref().unwrap().severity;

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
        let mut grid_view = EguiGridState::default();
        render_grid_with_view(ctx, document, &mut grid_view)
    }

    fn render_grid_with_view(
        ctx: &egui::Context,
        document: &mut Document,
        grid_view: &mut EguiGridState,
    ) -> egui::FullOutput {
        ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show_grid_with_view(ui, document, grid_view).unwrap();
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
        let horizontal_lines = output
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
        let top = horizontal_lines
            .iter()
            .map(|(_, points)| points[0].y)
            .fold(f32::INFINITY, f32::min);
        let divider_shapes = horizontal_lines
            .into_iter()
            .filter(|(_, points)| points[0].y == top + super::COLUMN_RULER_HEIGHT)
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
        let mut grid_view = EguiGridState::default();
        press_grid_key_with_view(ctx, document, &mut grid_view, key, modifiers);
    }

    fn press_grid_key_with_view(
        ctx: &egui::Context,
        document: &mut Document,
        grid_view: &mut EguiGridState,
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
                    show_grid_with_view(ui, document, grid_view).unwrap();
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
        if document.job.is_some() {
            await_index_worker(document);
            document.poll().unwrap();
        } else {
            assert!(
                document.index.is_some() && document.progress.done,
                "document must either have an active index job or an adopted completed index"
            );
        }
    }

    fn finish_structural_edit(app: &mut QuarryApp) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let ready = app
                .active
                .document
                .as_mut()
                .expect("the document remains open")
                .poll_structural_edit()
                .unwrap();
            if let Some(ready) = ready {
                app.install_materialized_working_copy(ready).unwrap();
            }
            if app
                .active
                .document
                .as_ref()
                .is_some_and(|document| document.structural_job.is_none())
            {
                break;
            }
            assert!(Instant::now() < deadline, "column edit timed out");
            std::thread::yield_now();
        }
        let document = app.active.document.as_mut().unwrap();
        if document.job.is_some() {
            finish_index(document);
        } else {
            assert!(document.index.is_some() && document.progress.done);
        }
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

    /// Wait for the real document retirement worker and return its final frame.
    pub(super) fn finish_document_cleanup(
        app: &mut QuarryApp,
        ctx: &egui::Context,
    ) -> egui::FullOutput {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "document cleanup timed out");
            let output = ctx.run(grid_input(), |ctx| {
                app.poll_document_tabs(ctx);
            });
            if app.document_cleanup.is_none() {
                return output;
            }
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
            .active
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
        let output = ctx.run(grid_input(), |ctx| {
            eframe::App::update(app, ctx, frame);
        });
        if app.document_cleanup.is_some() {
            finish_document_cleanup(app, ctx)
        } else {
            output
        }
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
        app.active.document = Some(document);
        let ctx = egui::Context::default();
        let mut frame = eframe::Frame::_new_kittest();
        let _ = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        let document = app.active.document.as_ref().unwrap();
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
            assert_eq!(
                app.active.document.as_ref().unwrap().viewport_start,
                expected
            );
        }
    }

    fn filter_manager_frame(
        app: &mut QuarryApp,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        size: egui::Vec2,
    ) -> (Option<Action>, egui::FullOutput) {
        let mut action = None;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events,
                ..Default::default()
            },
            |ctx| {
                action = show_filter_manager(
                    ctx,
                    &mut app.active.state.filters_open,
                    &mut app.active.state.filter_rules,
                    &mut app.active.state.filter_match_case,
                    app.active.state.document.as_ref().unwrap(),
                );
            },
        );
        (action, output)
    }

    fn filter_control_click(output: &egui::FullOutput, label: &str) -> Vec<egui::Event> {
        let target = output
            .platform_output
            .accesskit_update
            .as_ref()
            .unwrap()
            .nodes
            .iter()
            .find(|(_, node)| {
                node.label() == Some(label) && node.supports_action(egui::accesskit::Action::Click)
            })
            .map(|(id, _)| *id)
            .unwrap_or_else(|| panic!("missing clickable filter control {label}"));
        vec![egui::Event::AccessKitActionRequest(
            egui::accesskit::ActionRequest {
                action: egui::accesskit::Action::Click,
                target,
                data: None,
            },
        )]
    }

    #[test]
    fn filter_column_picker_restores_visible_choices_after_search() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("picker.csv");
        fs::write(
            &source,
            b"First name,Last name,Amount,Status,Note\nAvery,Chen,100,Active,sample\n",
        )
        .unwrap();
        let mut app = individual_edit_app(&source);
        app.active.filters_open = true;
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        super::configure_style(&ctx);
        let size = egui::vec2(1280.0, 780.0);
        let (_, output) = filter_manager_frame(&mut app, &ctx, vec![], size);
        let _ = filter_manager_frame(
            &mut app,
            &ctx,
            filter_control_click(&output, "Rule 1 column"),
            size,
        );
        let (_, output) =
            filter_manager_frame(&mut app, &ctx, vec![egui::Event::Text("Note".into())], size);
        assert_eq!(app.active.filter_rules[0].column_search, "Note");
        let _ = filter_manager_frame(
            &mut app,
            &ctx,
            filter_control_click(&output, "5: Note"),
            size,
        );
        let (_, output) = filter_manager_frame(&mut app, &ctx, vec![], size);
        let _ = filter_manager_frame(
            &mut app,
            &ctx,
            filter_control_click(&output, "Rule 1 column"),
            size,
        );
        for _ in 0..3 {
            let _ = filter_manager_frame(&mut app, &ctx, vec![], size);
        }
        let (_, output) = filter_manager_frame(&mut app, &ctx, vec![], size);
        assert!(app.active.filter_rules[0].column_search.is_empty());
        let check_visible = |output: &egui::FullOutput| {
            for label in [
                "1: First name",
                "2: Last name",
                "3: Amount",
                "4: Status",
                "5: Note",
            ] {
                assert!(
                    output.shapes.iter().any(|shape| match &shape.shape {
                        egui::Shape::Text(text) =>
                            text.galley.text() == label
                                && shape.clip_rect.contains_rect(text.visual_bounding_rect()),
                        _ => false,
                    }),
                    "{label} must be fully visible after reopening the picker"
                );
            }
        };
        check_visible(&output);
        let _ = filter_manager_frame(
            &mut app,
            &ctx,
            filter_control_click(&output, "1: First name"),
            size,
        );
        let (_, output) = filter_manager_frame(&mut app, &ctx, vec![], size);
        let _ = filter_manager_frame(
            &mut app,
            &ctx,
            filter_control_click(&output, "Add rule"),
            size,
        );
        let (_, output) = filter_manager_frame(&mut app, &ctx, vec![], size);
        let _ = filter_manager_frame(
            &mut app,
            &ctx,
            filter_control_click(&output, "Rule 2 column"),
            size,
        );
        let (_, output) = filter_manager_frame(&mut app, &ctx, vec![], size);
        check_visible(&output);
        app.active.document.as_mut().unwrap().shutdown();
    }

    #[test]
    fn filter_column_picker_preserves_source_identity_search_copy_and_escape() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("wide-filter.csv");
        let mut headers = (1..=100)
            .map(|column| format!("c{column}"))
            .collect::<Vec<_>>();
        headers[2] = "Repeated".into();
        headers[96] = "Repeated".into();
        let mut first = vec!["skip"; 100];
        first[96] = "keep";
        let mut second = vec!["skip"; 100];
        second[2] = "keep";
        let original = format!(
            "{}\n{}\n{}\n",
            headers.join(","),
            first.join(","),
            second.join(",")
        );
        fs::write(&source, &original).unwrap();
        let mut app = individual_edit_app(&source);
        let document = app.active.document.as_mut().unwrap();
        document.move_column(96, 0).unwrap();
        document.set_column_shown(96, false).unwrap();
        app.active.filters_open = true;
        app.active.filter_rules[0].operator = FilterOperator::Equals;
        app.active.filter_rules[0].value_input = "keep".into();
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let size = egui::vec2(1280.0, 780.0);
        let (_, output) = filter_manager_frame(&mut app, &ctx, vec![], size);
        let _ = filter_manager_frame(
            &mut app,
            &ctx,
            filter_control_click(&output, "Rule 1 column"),
            size,
        );
        ctx.memory_mut(|memory| memory.request_focus(super::filter_column_input_id(0)));
        let (_, output) = filter_manager_frame(
            &mut app,
            &ctx,
            vec![egui::Event::Text("Repeated".into())],
            size,
        );
        assert_eq!(app.active.filter_rules[0].column_search, "Repeated");
        let tree = output.platform_output.accesskit_update.as_ref().unwrap();
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::TextInput
                && node.label() == Some("Search columns for rule 1")
        }));
        for label in ["3: Repeated", "97: Repeated"] {
            assert!(
                tree.nodes
                    .iter()
                    .any(|(_, node)| node.label() == Some(label))
            );
        }
        let (_, output) = filter_manager_frame(&mut app, &ctx, vec![egui::Event::Copy], size);
        assert!(!super::selection_copy_requested(&ctx, 1, None, None));
        let _ = filter_manager_frame(
            &mut app,
            &ctx,
            filter_control_click(&output, "97: Repeated"),
            size,
        );
        assert_eq!(app.active.filter_rules[0].column_input, "97");
        let query = super::filter_query_from_rules(&app.active.filter_rules, false, 100).unwrap();
        assert_eq!(query.predicates[0].column, 96);
        app.apply(&ctx, Action::ApplyFilter);
        let document = app.active.document.as_mut().unwrap();
        finish_filter(document);
        assert_eq!(document.available_filter_rows(), 1);
        assert_eq!(document.visible_filter_rows()[0].row, 1);

        let (_, output) = filter_manager_frame(&mut app, &ctx, vec![], size);
        assert!(
            !output
                .platform_output
                .accesskit_update
                .as_ref()
                .unwrap()
                .nodes
                .iter()
                .any(|(_, node)| { node.label() == Some("Search columns for rule 1") }),
            "selecting a column must close its picker"
        );
        let _ = filter_manager_frame(
            &mut app,
            &ctx,
            filter_control_click(&output, "Rule 1 column"),
            size,
        );
        let draft = super::filter_query_from_rules(&app.active.filter_rules, false, 100).unwrap();
        let escape = || {
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }]
        };
        let (action, _) = filter_manager_frame(&mut app, &ctx, escape(), size);
        assert!(action.is_none());
        assert!(
            app.active.filters_open,
            "Escape must close the picker before Filters"
        );
        let (_, output) = filter_manager_frame(&mut app, &ctx, vec![], size);
        assert!(
            !output
                .platform_output
                .accesskit_update
                .unwrap()
                .nodes
                .iter()
                .any(|(_, node)| { node.label() == Some("Search columns for rule 1") })
        );
        let (action, _) = filter_manager_frame(&mut app, &ctx, escape(), size);
        assert!(action.is_none());
        assert!(!app.active.filters_open);
        assert_eq!(
            super::filter_query_from_rules(&app.active.filter_rules, false, 100).unwrap(),
            draft
        );
        assert_eq!(
            app.active.document.as_ref().unwrap().filter_query.as_ref(),
            Some(&query)
        );
        app.active.document.as_mut().unwrap().shutdown();
        assert_eq!(fs::read_to_string(source).unwrap(), original);
    }

    #[test]
    fn filter_footer_stays_visible_with_many_rules_in_a_small_window() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("filter-layout.csv");
        fs::write(&source, b"name\nfirst\n").unwrap();
        let mut app = individual_edit_app(&source);
        app.active.filters_open = true;
        app.active.filter_rules = vec![super::FilterRuleDraft::default(); 12];
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        super::configure_style(&ctx);
        let size = egui::vec2(860.0, 540.0);
        let check_footer = |output: &egui::FullOutput, controls: &[(&str, bool)]| {
            for (label, disabled) in controls {
                let (_, button) = accessible_button(output, label);
                let bounds = button.bounds().unwrap();
                assert!(bounds.x0 >= 0.0 && bounds.y0 >= 0.0);
                assert!(
                    bounds.x1 <= f64::from(size.x) && bounds.y1 <= f64::from(size.y),
                    "{label} must remain visible in the footer: {bounds:?}"
                );
                assert_eq!(button.is_disabled(), *disabled, "{label}");
            }
        };
        let _ = filter_manager_frame(&mut app, &ctx, vec![], size);
        let (_, output) = filter_manager_frame(&mut app, &ctx, vec![], size);
        check_footer(&output, &[("Apply filters", true), ("Clear filter", true)]);
        let tree = output.platform_output.accesskit_update.unwrap();
        assert!(
            tree.nodes
                .iter()
                .any(|(_, node)| node.value() == Some("Enter text to match."))
        );
        assert!(!tree.nodes.iter().any(|(_, node)| {
            node.value()
                .is_some_and(|label| label.contains("invalid filter rule"))
        }));
        let document = app.active.document.as_mut().unwrap();
        document
            .start_filter(FilterQuery::single(
                0,
                FilterOperator::Equals,
                b"first".to_vec(),
            ))
            .unwrap();
        finish_filter(document);
        let _ = filter_manager_frame(&mut app, &ctx, vec![], size);
        let (_, output) = filter_manager_frame(&mut app, &ctx, vec![], size);
        check_footer(
            &output,
            &[
                ("Apply filters", true),
                ("Clear filter", false),
                ("Export Filtered Rows…", false),
            ],
        );
        app.active
            .document
            .as_mut()
            .unwrap()
            .rename_header(0, "customer".into())
            .unwrap();
        let (_, output) = filter_manager_frame(&mut app, &ctx, vec![], size);
        check_footer(
            &output,
            &[
                ("Apply filters", true),
                ("Clear filter", false),
                ("Export Filtered Rows…", false),
            ],
        );

        let document = app.active.document.as_mut().unwrap();
        document.clear_filter().unwrap();
        commit_test_cell(document, 1, 0, "edited");
        let _ = filter_manager_frame(&mut app, &ctx, vec![], size);
        let (_, output) = filter_manager_frame(&mut app, &ctx, vec![], size);
        check_footer(&output, &[("Apply filters", true), ("Clear filter", true)]);
        app.active.document.as_mut().unwrap().shutdown();
    }

    #[test]
    fn compact_toolbar_panels_stay_anchored_and_preserve_filter_drafts() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("compact-panels.csv");
        fs::write(
            &source,
            format!("name,{}\nfirst,second\n", "long_column_name_".repeat(24)),
        )
        .unwrap();
        let mut app = individual_edit_app(&source);
        app.active.filter_rules = vec![super::FilterRuleDraft::default(); 2];
        app.active.filter_rules[0].value_input = "first".into();
        app.active.filter_rules[1].value_input = "second".into();
        app.active.filter_match_case = true;
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        super::configure_style(&ctx);
        let mut frame = eframe::Frame::_new_kittest();
        let mut render = |app: &mut QuarryApp, events: Vec<egui::Event>, size| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events,
                ..grid_input()
            };
            let mut output = ctx.run(input, |ctx| eframe::App::update(app, ctx, &mut frame));
            for _ in 0..2 {
                output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                        ..grid_input()
                    },
                    |ctx| eframe::App::update(app, ctx, &mut frame),
                );
            }
            output
        };
        for size in [egui::vec2(860.0, 540.0), egui::vec2(1280.0, 780.0)] {
            app.active.columns_open = false;
            app.active.filters_open = false;
            app.active.filter_rules[0].column_input = "1".into();
            let mut output = render(&mut app, vec![], size);
            for (label, id) in [
                ("Filters…", "quarry-filter-manager"),
                ("Columns…", "quarry-column-manager"),
                ("Filters…", "quarry-filter-manager"),
            ] {
                let draft = app.active.filter_rules.clone();
                let events = filter_control_click(&output, label);
                output = render(&mut app, events, size);
                assert_eq!(app.active.filters_open, label == "Filters…");
                assert_eq!(app.active.columns_open, label == "Columns…");
                assert_eq!(app.active.filter_rules, draft);
                assert!(app.active.filter_match_case);
                let panel = ctx
                    .memory(|memory| memory.area_rect(egui::Id::new(id)))
                    .unwrap();
                assert!(
                    egui::Rect::from_min_size(egui::Pos2::ZERO, size).contains_rect(panel),
                    "{id}: {panel:?}"
                );
                let button = accessible_button(&output, label).1.bounds().unwrap();
                // The popover hangs under its button: edge-aligned when it fits, else kept on screen.
                assert!(
                    f64::from(panel.left()) <= button.x0 + 1.0
                        && f64::from(panel.right()) >= button.x1 - 1.0,
                    "{id} must follow its toolbar button: panel={panel:?}, button={button:?}"
                );
                assert!(
                    (f64::from(panel.top()) - button.y1 - 6.0).abs() <= 1.0,
                    "{id} must sit below its toolbar button: panel={panel:?}, button={button:?}, viewport={size:?}"
                );
                if !app.active.filters_open {
                    continue;
                }
                app.active.filter_rules[0].column_input = "2".into();
                output = render(&mut app, vec![], size);
                let long_panel = ctx
                    .memory(|memory| memory.area_rect(egui::Id::new(id)))
                    .unwrap();
                assert!(
                    (panel.width() - long_panel.width()).abs() < 1.0,
                    "Long column names must truncate instead of widening Filters"
                );
                let nodes = &output
                    .platform_output
                    .accesskit_update
                    .as_ref()
                    .unwrap()
                    .nodes;
                for rule in 1..=2 {
                    let bounds = ["column", "match", "value"].map(|field| {
                        let label = format!("Rule {rule} {field}");
                        nodes
                            .iter()
                            .find(|(_, node)| node.label() == Some(label.as_str()))
                            .and_then(|(_, node)| node.bounds())
                            .unwrap()
                    });
                    for pair in bounds.windows(2) {
                        assert!(
                            pair[0].x1 <= pair[1].x0,
                            "Rule {rule} controls must read across"
                        );
                        assert!(
                            (pair[0].y0 - pair[1].y0).abs() <= 1.0,
                            "Rule {rule} controls must share a row"
                        );
                    }
                    for (label, control) in ["Column", "Condition", "Value"].into_iter().zip(bounds)
                    {
                        let left = output
                            .shapes
                            .iter()
                            .find_map(|shape| match &shape.shape {
                                egui::Shape::Text(text) if text.galley.text() == label => {
                                    Some(text.pos.x)
                                }
                                _ => None,
                            })
                            .unwrap();
                        assert!(
                            (f64::from(left) - control.x0).abs() <= 1.0,
                            "{label} must align with its control"
                        );
                    }
                    assert!(bounds[2].x1 <= f64::from(long_panel.right()));
                }
            }
            let long_value = "literal text ".repeat(60);
            for value in [
                long_value.clone(),
                format!(
                    "{long_value}\nline two\nline three\nline four\nline five\nline six\nline seven"
                ),
            ] {
                app.active.filter_rules[0].value_input = value.clone();
                output = render(&mut app, vec![], size);
                assert_eq!(app.active.filter_rules[0].value_input, value);
                let panel = ctx
                    .memory(|memory| memory.area_rect(egui::Id::new("quarry-filter-manager")))
                    .unwrap();
                assert!(
                    panel.height() <= 350.0,
                    "Long literals must scroll inside their value field: {panel:?}"
                );
                assert!(egui::Rect::from_min_size(egui::Pos2::ZERO, size).contains_rect(panel));
                let nodes = &output
                    .platform_output
                    .accesskit_update
                    .as_ref()
                    .unwrap()
                    .nodes;
                for label in [
                    "Rule 2 column",
                    "Rule 2 match",
                    "Rule 2 value",
                    "Add rule",
                    "Match case",
                    "Clear filter",
                    "Apply filters",
                ] {
                    let node = &nodes
                        .iter()
                        .find(|(_, node)| node.label() == Some(label))
                        .unwrap()
                        .1;
                    let bounds = node.bounds().unwrap();
                    assert!(
                        !node.is_hidden(),
                        "{label} must remain visible with a long literal"
                    );
                    assert!(
                        bounds.x0 >= 0.0
                            && bounds.y0 >= 0.0
                            && bounds.x1 <= f64::from(size.x)
                            && bounds.y1 <= f64::from(size.y),
                        "{label} must remain inside the viewport: {bounds:?}"
                    );
                }
            }
            app.active.filter_rules[0].value_input = "first".into();
        }
        app.active.document.as_mut().unwrap().shutdown();
    }

    #[test]
    fn numeric_filter_controls_validate_bounds_and_export_exact_rows() {
        fn render(
            app: &mut QuarryApp,
            ctx: &egui::Context,
            events: Vec<egui::Event>,
        ) -> (Option<Action>, egui::FullOutput) {
            let mut action = None;
            let output = ctx.run(
                egui::RawInput {
                    events,
                    ..grid_input()
                },
                |ctx| {
                    action = show_filter_manager(
                        ctx,
                        &mut app.active.state.filters_open,
                        &mut app.active.state.filter_rules,
                        &mut app.active.state.filter_match_case,
                        app.active.state.document.as_ref().unwrap(),
                    );
                },
            );
            (action, output)
        }
        fn click(target: egui::accesskit::NodeId) -> Vec<egui::Event> {
            vec![egui::Event::AccessKitActionRequest(
                egui::accesskit::ActionRequest {
                    action: egui::accesskit::Action::Click,
                    target,
                    data: None,
                },
            )]
        }

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("numeric-filter.csv");
        let destination = directory.path().join("numeric-filter-export.csv");
        let original = b"amount,note\r\n99.999,below\r\n100,lower\r\n1e3,upper\r\n1000.0001,above\r\n,blank\r\nbad,invalid\r\n 1.00e2 ,\"line one\nline two\"\r\n";
        fs::write(&source, original).unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        app.active.header_mode = HeaderMode::FirstRow;
        app.open_path(source.clone()).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        app.active.filters_open = true;
        app.active.filter_match_case = true;
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let (_, output) = render(&mut app, &ctx, vec![]);
        let combo = output
            .platform_output
            .accesskit_update
            .unwrap()
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::ComboBox
                    && node.label() == Some("Rule 1 match")
            })
            .map(|(id, _)| *id)
            .unwrap();
        let (_, output) = render(&mut app, &ctx, click(combo));
        let tree = output.platform_output.accesskit_update.unwrap();
        for operator in [
            FilterOperator::GreaterThan,
            FilterOperator::GreaterThanOrEqual,
            FilterOperator::LessThan,
            FilterOperator::LessThanOrEqual,
            FilterOperator::Between,
        ] {
            assert!(tree.nodes.iter().any(|(_, node)| {
                node.label() == Some(super::filter_operator_label(operator))
                    && node.supports_action(egui::accesskit::Action::Click)
            }));
        }
        let between = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Between (inclusive)"))
            .map(|(id, _)| *id)
            .unwrap();
        let _ = render(&mut app, &ctx, click(between));
        assert_eq!(app.active.filter_rules[0].operator, FilterOperator::Between);
        let (_, output) = render(&mut app, &ctx, vec![]);
        assert!(accessible_button(&output, "Apply filters").1.is_disabled());
        for operator in [
            FilterOperator::Contains,
            FilterOperator::Equals,
            FilterOperator::NotEquals,
            FilterOperator::GreaterThan,
            FilterOperator::GreaterThanOrEqual,
            FilterOperator::LessThan,
            FilterOperator::LessThanOrEqual,
            FilterOperator::Between,
        ] {
            assert!(
                !output
                    .platform_output
                    .accesskit_update
                    .as_ref()
                    .unwrap()
                    .nodes
                    .iter()
                    .any(|(_, node)| {
                        node.label() == Some(super::filter_operator_label(operator))
                            && node.supports_action(egui::accesskit::Action::Click)
                    }),
                "operator choices must close after selecting Between"
            );
        }
        assert_eq!(
            output
                .platform_output
                .accesskit_update
                .unwrap()
                .nodes
                .iter()
                .filter(|(_, node)| node.role() == egui::accesskit::Role::TextInput
                    && !node.labelled_by().is_empty())
                .count(),
            2
        );

        ctx.memory_mut(|memory| memory.request_focus(super::filter_value_input_id(0)));
        let _ = render(&mut app, &ctx, vec![egui::Event::Text("100".into())]);
        ctx.memory_mut(|memory| memory.request_focus(super::filter_upper_bound_input_id(0)));
        let (_, output) = render(&mut app, &ctx, vec![egui::Event::Text("1e3".into())]);
        assert!(super::is_filter_text_input(
            super::filter_upper_bound_input_id(0),
            1
        ));
        let (apply, node) = accessible_button(&output, "Apply filters");
        assert!(!node.is_disabled());
        let (action, _) = render(&mut app, &ctx, click(apply));
        assert_eq!(action, Some(Action::ApplyFilter));
        app.apply(&ctx, action.unwrap());
        assert!(app.active.notice.is_none());
        let document = app.active.document.as_mut().unwrap();
        finish_filter(document);
        assert_eq!(document.available_filter_rows(), 3);
        let active_query = document.filter_query.clone();
        let active_rows = document
            .visible_filter_rows()
            .iter()
            .map(|row| row.row)
            .collect::<Vec<_>>();
        assert_eq!(active_rows, vec![2, 3, 7]);

        for invalid in ["", "99", "NaN", "1,000"] {
            app.active.filter_rules[0].upper_bound_input = invalid.into();
            let (_, output) = render(&mut app, &ctx, vec![]);
            assert!(accessible_button(&output, "Apply filters").1.is_disabled());
            app.apply(&ctx, Action::ApplyFilter);
            assert!(app.active.notice.is_some());
            let document = app.active.document.as_ref().unwrap();
            assert_eq!(document.filter_query, active_query);
            assert_eq!(
                document
                    .visible_filter_rows()
                    .iter()
                    .map(|row| row.row)
                    .collect::<Vec<_>>(),
                active_rows
            );
        }
        let document = app.active.document.as_mut().unwrap();
        document.start_filtered_export(destination.clone()).unwrap();
        finish_filtered_export(document);
        assert_eq!(
            fs::read(destination).unwrap(),
            b"amount,note\r\n100,lower\r\n1e3,upper\r\n 1.00e2 ,\"line one\nline two\"\r\n"
        );
        assert_eq!(fs::read(source).unwrap(), original);
        document.shutdown();
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
        app.active.header_mode = HeaderMode::FirstRow;
        app.open_path(path.clone()).unwrap();
        app.active.filters_open = true;
        app.active.filter_rules[0].column_input = "2".to_owned();
        app.active.filter_rules[0].operator = FilterOperator::Equals;

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut action = None;
        let output = ctx.run(grid_input(), |ctx| {
            action = show_filter_manager(
                ctx,
                &mut app.active.state.filters_open,
                &mut app.active.state.filter_rules,
                &mut app.active.state.filter_match_case,
                app.active.state.document.as_ref().unwrap(),
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
            1
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
                    &mut app.active.state.filters_open,
                    &mut app.active.state.filter_rules,
                    &mut app.active.state.filter_match_case,
                    app.active.state.document.as_ref().unwrap(),
                );
            },
        );
        assert!(action.is_none(), "Enter should insert data, not apply");
        assert_eq!(app.active.filter_rules[0].value_input, "line one\nline two");

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
                    &mut app.active.state.filters_open,
                    &mut app.active.state.filter_rules,
                    &mut app.active.state.filter_match_case,
                    app.active.state.document.as_ref().unwrap(),
                );
            },
        );
        assert!(matches!(action, Some(Action::ApplyFilter)));

        app.apply(&ctx, action.unwrap());
        let document = app.active.document.as_mut().unwrap();
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
        app.active.document = Some(document);
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
                .all(|text| !text.starts_with("Matches "))
        );

        app.active.document.as_mut().unwrap().shutdown();
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
            2
        );
        assert_eq!(
            tree.nodes
                .iter()
                .filter(|(_, node)| {
                    node.role() == egui::accesskit::Role::ComboBox && !node.labelled_by().is_empty()
                })
                .count(),
            4
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
        app.active.header_mode = HeaderMode::FirstRow;
        app.open_path(path.clone()).unwrap();
        app.active.filter_rules = vec![
            super::FilterRuleDraft {
                column_input: "2".into(),
                column_search: String::new(),
                operator: FilterOperator::Equals,
                value_input: "tx".into(),
                upper_bound_input: String::new(),
            },
            super::FilterRuleDraft {
                column_input: "2".into(),
                column_search: String::new(),
                operator: FilterOperator::Equals,
                value_input: "fL".into(),
                upper_bound_input: String::new(),
            },
            super::FilterRuleDraft {
                column_input: "3".into(),
                column_search: String::new(),
                operator: FilterOperator::NotEquals,
                value_input: "INACTIVE".into(),
                upper_bound_input: String::new(),
            },
        ];
        assert_eq!(
            filter_button_label(app.active.document.as_ref().unwrap().filter_query.as_ref()),
            "Filters…"
        );
        let ctx = egui::Context::default();
        app.apply(&ctx, Action::ApplyFilter);
        assert!(app.active.notice.is_none());

        let document = app.active.document.as_mut().unwrap();
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
    fn failed_save_start_preserves_completed_filter_and_position() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("save-filter.csv");
        let destination = directory.path().join("exists.csv");
        fs::write(
            &source,
            format!(
                "name,status\n{}",
                (0..100).map(|i| format!("{i},keep\n")).collect::<String>()
            ),
        )
        .unwrap();
        fs::write(&destination, b"existing").unwrap();
        let mut app = individual_edit_app(&source);
        let document = app.active.document.as_mut().unwrap();
        commit_test_cell(document, 1, 0, "changed");
        document.set_visible_rows(5).unwrap();
        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"keep".to_vec(),
            ))
            .unwrap();
        finish_filter(document);
        document.navigate_filter(50).unwrap();
        finish_filter_read(document);
        let edits = document.cell_edits.clone();
        let rows = document
            .visible_filter_rows()
            .iter()
            .map(|r| r.row)
            .collect::<Vec<_>>();
        assert!(document.start_save_as(destination.clone()).is_err());
        assert!(document.filter_active());
        assert_eq!(document.filter_viewport_start, 50);
        assert_eq!(
            document
                .visible_filter_rows()
                .iter()
                .map(|r| r.row)
                .collect::<Vec<_>>(),
            rows
        );
        assert_eq!(document.cell_edits, edits);
        assert_eq!(fs::read(destination).unwrap(), b"existing");
        document.shutdown();
    }

    #[test]
    fn active_filtered_export_blocks_discard_without_losing_overlays() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("discard-filter.csv");
        let destination = directory.path().join("export.csv");
        fs::write(&source, b"name,status\nAda,keep\nBob,drop\n").unwrap();
        let mut app = individual_edit_app(&source);
        let document = app.active.document.as_mut().unwrap();
        commit_test_cell(document, 2, 1, "keep");
        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"keep".to_vec(),
            ))
            .unwrap();
        finish_filter(document);
        document.start_filtered_export(destination).unwrap();
        let edits = document.cell_edits.clone();
        app.apply(&egui::Context::default(), Action::DiscardChanges);
        let document = app.active.document.as_mut().unwrap();
        assert_eq!(
            document.cell_edits, edits,
            "discard action must wait for export"
        );
        document.discard_edits();
        assert_eq!(
            document.cell_edits, edits,
            "direct discard must wait for export"
        );
        assert!(document.filter_active());
        let deadline = Instant::now() + Duration::from_secs(5);
        while document.export_job.is_some() {
            document.poll_filtered_export().unwrap();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        document.discard_edits();
        assert!(document.cell_edits.is_empty());
        assert!(!document.filter_active());
        assert_eq!(
            fs::read(source).unwrap(),
            b"name,status\nAda,keep\nBob,drop\n"
        );
        document.shutdown();
    }

    #[test]
    fn clicking_another_filtered_cell_continues_editing_after_committing_the_old_cell() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("filter-click-switch.csv");
        fs::write(&source, b"name,status\nAda,keep\nBob,keep\n").unwrap();
        let mut app = individual_edit_app(&source);
        let document = app.active.document.as_mut().unwrap();
        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"keep".to_vec(),
            ))
            .unwrap();
        finish_filter(document);
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        for _ in 0..3 {
            let _ = render_grid(&ctx, document);
            finish_filter_read(document);
        }
        document.begin_cell_edit(1, 0, b"Ada".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "Anna".into();
        let output = render_grid(&ctx, document);
        let nodes = output.platform_output.accesskit_update.unwrap().nodes;
        let target = nodes
            .iter()
            .find(|(_, node)| {
                node.label()
                    .is_some_and(|label| label.contains("column 1") && label.ends_with(": Bob"))
            })
            .unwrap_or_else(|| {
                panic!(
                    "missing second cell: {:?}",
                    nodes
                        .iter()
                        .filter_map(|(_, node)| node.label())
                        .collect::<Vec<_>>()
                )
            })
            .0;
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
        let edit = document
            .cell_edit
            .as_ref()
            .expect("clicked cell retains its editor");
        assert_eq!((edit.row, edit.column, edit.draft.as_str()), (2, 0, "Bob"));
        assert_eq!(document.cell_edits[&(1, 0)].as_slice(), b"Anna");
        finish_filter(document);
        assert_eq!(document.cell_edit.as_ref().unwrap().row, 2);
        document.shutdown();
    }

    #[test]
    fn switching_filtered_editors_keeps_the_next_activation_during_refresh() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("filter-editor-switch.csv");
        fs::write(&source, b"name,status\nAda,keep\nBob,keep\n").unwrap();
        let mut app = individual_edit_app(&source);
        let document = app.active.document.as_mut().unwrap();
        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"keep".to_vec(),
            ))
            .unwrap();
        finish_filter(document);
        document.begin_cell_edit(1, 0, b"Ada".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "Anna".into();
        document.begin_cell_edit(2, 0, b"Bob".to_vec()).unwrap();
        assert!(document.filter_refresh_pending);
        let edit = document
            .cell_edit
            .as_ref()
            .expect("second activation retained");
        assert_eq!((edit.row, edit.column, edit.draft.as_str()), (2, 0, "Bob"));
        document.cell_edit.as_mut().unwrap().draft = "Bobby".into();
        document.poll_filter().unwrap();
        assert_eq!(document.cell_edit.as_ref().unwrap().draft, "Bobby");
        assert!(!document.cell_edits.contains_key(&(2, 0)));
        finish_filter(document);
        assert_eq!(document.cell_edit.as_ref().unwrap().draft, "Bobby");
        document.commit_cell_edit();
        finish_filter(document);
        assert_eq!(document.effective_cell(1, 0), Some(b"Anna".as_slice()));
        assert_eq!(document.effective_cell(2, 0), Some(b"Bobby".as_slice()));
        assert_eq!(
            fs::read(&source).unwrap(),
            b"name,status\nAda,keep\nBob,keep\n"
        );
        document.shutdown();
    }

    #[test]
    fn filtered_export_waits_for_draft_refresh_before_opening_the_picker() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("filter-export-draft.csv");
        fs::write(&source, b"name,status\nAda,keep\nBob,keep\n").unwrap();
        let mut app = individual_edit_app(&source);
        let document = app.active.document.as_mut().unwrap();
        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"keep".to_vec(),
            ))
            .unwrap();
        finish_filter(document);
        document.begin_cell_edit(1, 1, b"keep".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "drop".into();
        assert!(app.prepare_filtered_export().is_err());
        let document = app.active.document.as_mut().unwrap();
        assert!(document.filter_refresh_pending);
        finish_filter(document);
        assert_eq!(document.available_filter_rows(), 1);
        assert_eq!(app.prepare_filtered_export().unwrap(), source);
        app.active.document.as_mut().unwrap().shutdown();
    }

    #[test]
    fn incomplete_filter_can_display_a_partial_first_page() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("filter-partial-first-page.csv");
        fs::write(
            &source,
            format!(
                "id,status\n{}",
                (0..100)
                    .map(|id| format!("{id},keep\n"))
                    .collect::<String>()
            ),
        )
        .unwrap();
        let mut app = individual_edit_app(&source);
        let document = app.active.document.as_mut().unwrap();
        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"keep".to_vec(),
            ))
            .unwrap();
        finish_filter(document);
        let complete = document.filter_progress.unwrap();
        document.stop_filter_read();
        document.filter_index = Some(
            document
                .session
                .start_filter(FilterQuery::single(
                    0,
                    FilterOperator::LessThan,
                    b"10".to_vec(),
                ))
                .unwrap()
                .wait()
                .unwrap(),
        );
        document.filter_progress = Some(quarry_core::FilterProgress {
            done: false,
            matches_found: 10,
            ..complete
        });
        document.filtered_rows.clear();
        document.set_visible_rows(64).unwrap();
        finish_filter_read(document);
        assert_eq!(document.visible_filter_rows().len(), 10);
        assert_eq!(document.filter_viewport_start, 0);
        assert!(!document.filter_progress.unwrap().done);
        document.shutdown();
    }

    #[test]
    fn resizing_during_a_partial_filter_refresh_keeps_the_requested_page() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("filter-resize.csv");
        fs::write(
            &source,
            format!(
                "id,status\n{}",
                (0..1000)
                    .map(|id| format!("{id},keep\n"))
                    .collect::<String>()
            ),
        )
        .unwrap();
        let mut app = individual_edit_app(&source);
        let document = app.active.document.as_mut().unwrap();
        document.set_visible_rows(5).unwrap();
        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"keep".to_vec(),
            ))
            .unwrap();
        finish_filter(document);
        document.navigate_filter(500).unwrap();
        finish_filter_read(document);
        let index = document.filter_index.take().unwrap();
        let complete = document.filter_progress.unwrap();
        document.filter_index = Some(
            document
                .session
                .start_filter(FilterQuery::single(
                    0,
                    FilterOperator::LessThan,
                    b"10".to_vec(),
                ))
                .unwrap()
                .wait()
                .unwrap(),
        );
        document.stop_filter_read();
        document.filtered_rows.clear();
        document.filter_progress = Some(quarry_core::FilterProgress {
            done: false,
            matches_found: 10,
            ..complete
        });
        document.scroll_rows(1).unwrap();
        assert_eq!(document.filter_viewport_start, 500);
        document.navigate_grid_position(1).unwrap();
        assert_eq!(document.filter_viewport_start, 500);
        document.set_visible_rows(6).unwrap();
        assert_eq!(document.filter_viewport_start, 500);
        document.filter_index = Some(index);
        document.filter_progress = Some(complete);
        document
            .navigate_filter(document.filter_viewport_start)
            .unwrap();
        finish_filter_read(document);
        assert_eq!(document.filter_viewport_start, 500);
        assert_eq!(document.visible_filter_rows()[0].row, 501);
        document.shutdown();
    }

    #[test]
    fn editing_a_later_filtered_page_preserves_its_position() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("filter-position.csv");
        fs::write(
            &source,
            format!(
                "id,status\n{}",
                (0..1000)
                    .map(|id| format!("{id},keep\n"))
                    .collect::<String>()
            ),
        )
        .unwrap();
        let mut app = individual_edit_app(&source);
        let document = app.active.document.as_mut().unwrap();
        document.set_visible_rows(5).unwrap();
        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"keep".to_vec(),
            ))
            .unwrap();
        finish_filter(document);
        document.navigate_filter(500).unwrap();
        finish_filter_read(document);
        commit_test_cell(document, 501, 0, "edited");
        finish_filter(document);
        assert_eq!(document.filter_viewport_start, 500);
        assert_eq!(document.visible_filter_rows()[0].row, 501);
        commit_test_cell(document, 501, 1, "drop");
        finish_filter(document);
        assert_eq!(document.filter_viewport_start, 500);
        assert_eq!(document.visible_filter_rows()[0].row, 502);
        document.navigate_filter(999).unwrap();
        finish_filter_read(document);
        let row = document.visible_filter_rows()[0].row;
        commit_test_cell(document, row, 1, "drop");
        finish_filter(document);
        assert_eq!(
            document.filter_viewport_start,
            document.available_filter_rows() - 5
        );
        document.shutdown();
    }

    #[test]
    fn edited_filtered_rows_refresh_and_history_keeps_source_row_identity() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("working-filter.csv");
        let original = b"id,state\n1,keep\n2,drop\n3,keep\n";
        fs::write(&source, original).unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        app.active.header_mode = HeaderMode::FirstRow;
        app.open_path(source.clone()).unwrap();
        let document = app.active.document.as_mut().unwrap();
        commit_test_cell(document, 2, 1, "keep");
        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"keep".to_vec(),
            ))
            .unwrap();
        finish_filter(document);
        assert_eq!(
            document
                .visible_filter_rows()
                .iter()
                .map(|row| row.row)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(document.source_cell(2, 1), Some(b"drop".as_slice()));
        assert_eq!(document.effective_cell(2, 1), Some(b"keep".as_slice()));
        commit_test_cell(document, 2, 1, "drop");
        assert!(document.filtered_rows.is_empty()); // old rows cannot be adopted during refresh
        assert!(!document.is_filtered_export_ready());
        finish_filter(document);
        assert_eq!(
            document
                .visible_filter_rows()
                .iter()
                .map(|row| row.row)
                .collect::<Vec<_>>(),
            [1, 3]
        );
        assert!(document.start_find_next(b"keep").is_err());
        app.swap_edit_history(false).unwrap();
        let document = app.active.document.as_mut().unwrap();
        finish_filter(document);
        assert_eq!(
            document
                .visible_filter_rows()
                .iter()
                .map(|row| row.row)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        app.swap_edit_history(true).unwrap();
        let document = app.active.document.as_mut().unwrap();
        finish_filter(document);
        assert_eq!(
            document
                .visible_filter_rows()
                .iter()
                .map(|row| row.row)
                .collect::<Vec<_>>(),
            [1, 3]
        );
        commit_test_cell(document, 3, 0, "changed-id");
        finish_filter(document);
        document.rename_header(0, "identifier".into()).unwrap();
        let export = directory.path().join("filtered.csv");
        document.start_filtered_export(export.clone()).unwrap();
        finish_filtered_export(document);
        assert_eq!(
            fs::read(export).unwrap(),
            b"identifier,state\n1,keep\nchanged-id,keep\n"
        );
        assert_eq!(fs::read(&source).unwrap(), original);
        assert!(document.is_dirty());
        let saved = directory.path().join("saved.csv");
        document.start_save_as(saved.clone()).unwrap();
        assert!(!document.filter_active());
        while document.poll_save().unwrap().is_none() {
            std::thread::yield_now();
        }
        assert_eq!(
            fs::read(saved).unwrap(),
            b"identifier,state\n1,keep\n2,drop\nchanged-id,keep\n"
        );
        assert_eq!(fs::read(source).unwrap(), original);
        document.shutdown();
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
        // The worker can publish matches before the first frame, but export must
        // stay disabled until the document has consumed its completion.
        assert!(document.filter_job.is_some());
        assert!(!document.is_filtered_export_ready());
        let output = ctx.run(grid_input(), |ctx| {
            let _ = show_filter_manager(ctx, &mut open, &mut rules, &mut match_case, &document);
        });
        assert!(
            accessible_button(&output, "Export Filtered Rows…")
                .1
                .is_disabled()
        );
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

        let mut grid_view = EguiGridState::default();
        press_grid_key_with_view(
            &ctx,
            &mut document,
            &mut grid_view,
            egui::Key::Enter,
            egui::Modifiers::NONE,
        );
        assert!(document.cell_edit.is_none());
        let output = render_grid_with_view(&ctx, &mut document, &mut grid_view);
        assert_eq!(
            output
                .platform_output
                .accesskit_update
                .expect("accessibility tree should be present")
                .focus,
            cell_control
        );
        assert_eq!(
            document
                .cell_edits
                .get(&(1, 1))
                .map(|value| value.as_slice()),
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
        document
            .start_filter(FilterQuery::single(
                0,
                FilterOperator::Contains,
                b"first".to_vec(),
            ))
            .unwrap();
        finish_filter(&mut document);
        assert_eq!(document.available_filter_rows(), 1);
        document.clear_filter().unwrap();

        document.set_column_shown(1, false).unwrap();
        document.set_column_shown(1, true).unwrap();
        document.move_column(1, 0).unwrap();
        document.set_visible_rows(1).unwrap();
        document.navigate(2).unwrap();
        document.navigate(1).unwrap();
        assert_eq!(
            document
                .cell_edits
                .get(&(1, 1))
                .map(|value| value.as_slice()),
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
        let _ = render_grid_with_view(&ctx, &mut document, &mut grid_view);
        press_grid_key_with_view(
            &ctx,
            &mut document,
            &mut grid_view,
            egui::Key::Escape,
            egui::Modifiers::NONE,
        );
        assert!(document.cell_edit.is_none());
        let output = render_grid_with_view(&ctx, &mut document, &mut grid_view);
        assert_eq!(
            output
                .platform_output
                .accesskit_update
                .expect("accessibility tree should be present")
                .focus,
            cell_control
        );
        assert_eq!(
            document
                .cell_edits
                .get(&(1, 1))
                .map(|value| value.as_slice()),
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
            document
                .cell_edits
                .get(&(1, 1))
                .map(|value| value.as_slice()),
            Some(b"focus loss".as_slice())
        );

        document.set_visible_rows(1).unwrap();
        document.begin_cell_edit(1, 1, b"1".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "wheel navigation".into();
        document.scroll_rows(1).unwrap();
        assert!(document.cell_edit.is_none());
        assert_eq!(
            document
                .cell_edits
                .get(&(1, 1))
                .map(|value| value.as_slice()),
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
            "Wait for the active file operation before editing data cells."
        );
        finish_filter(&mut document);
        document.begin_cell_edit(1, 1, b"1".to_vec()).unwrap();

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
        app.active.header_mode = HeaderMode::FirstRow;
        app.open_path(path.clone()).unwrap();
        let document = app.active.document.as_mut().unwrap();
        document.rename_header(0, "button_name".into()).unwrap();
        document.begin_cell_edit(1, 1, b"1".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "line one\nline two".into();

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut frame = eframe::Frame::_new_kittest();
        app.quit_pending = true;
        app.active.close_confirmation_open = true;
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
        assert!(app.active.close_confirmation_open);
        assert!(app.active.document.as_ref().unwrap().save_job.is_none());
        assert!(app.active.document.as_ref().unwrap().is_dirty());
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
        assert!(app.active.document.as_ref().unwrap().save_job.is_some());
        let _ = finish_app_save(&mut app, &ctx, &mut frame);
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(document.session.path(), path);
        assert_eq!(document.column_name(0), "button_name");
        assert!(!document.is_dirty());
        assert_eq!(app.active.delimiter_mode, DelimiterMode::Auto);
        assert_eq!(app.active.header_mode, HeaderMode::FirstRow);
        assert_eq!(
            fs::read(&path).unwrap(),
            b"button_name,value\nfirst,\"line one\nline two\"\n"
        );

        let document = app.active.document.as_mut().unwrap();
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
        assert!(app.active.document.as_ref().unwrap().save_job.is_some());
        let _ = finish_app_save(&mut app, &ctx, &mut frame);
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(document.session.path(), path);
        assert_eq!(document.column_name(0), "shortcut_name");
        assert!(!document.is_dirty());
        assert_eq!(
            fs::read(&path).unwrap(),
            b"shortcut_name,value\nfirst,\"line one\nline two\"\n"
        );

        app.active
            .document
            .as_mut()
            .unwrap()
            .rename_header(0, "close_name".into())
            .unwrap();
        app.quit_pending = true;
        app.active.close_confirmation_open = true;
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
        assert!(app.active.close_after_save);
        assert!(app.active.document.as_ref().unwrap().save_job.is_some());
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
        assert!(app.quit_authorized);
        assert!(app.document_tabs().all(|tab| tab.document.is_none()));
        let reopened = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(reopened.column_name(0), "close_name");
        assert!(!reopened.is_dirty());
        drop(reopened);
        assert_eq!(
            fs::read(&path).unwrap(),
            b"close_name,value\nfirst,\"line one\nline two\"\n"
        );

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
        app.active.header_mode = HeaderMode::NoHeader;
        app.open_path(source.clone()).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        let document = app.active.document.as_mut().unwrap();
        let source_cell = document.visible_row(0).unwrap().1[0].clone();
        assert_eq!(source_cell, b"first");
        document.begin_cell_edit(0, 0, source_cell).unwrap();
        assert_eq!(document.cell_edit.as_ref().unwrap().draft, "first");
        document.cell_edit.as_mut().unwrap().draft = "saved".into();

        let ctx = egui::Context::default();
        let mut frame = eframe::Frame::_new_kittest();
        assert!(app.save_current());
        let _ = finish_app_save(&mut app, &ctx, &mut frame);
        assert!(app.active.notice.is_none());
        assert_eq!(
            app.active
                .footer_status
                .as_ref()
                .map(|message| message.severity),
            Some(MessageSeverity::Status)
        );
        let document = app.active.document.as_ref().unwrap();
        assert!(!document.session.dialect.has_header);
        assert_eq!(document.data_start, 0);
        assert!(!document.is_dirty());
        assert_eq!(app.active.delimiter_mode, DelimiterMode::Auto);
        assert_eq!(app.active.header_mode, HeaderMode::NoHeader);
        assert_eq!(
            fs::read(&source).unwrap(),
            b"\xEF\xBB\xBFsaved,1\nsecond,2\n"
        );

        finish_index(app.active.document.as_mut().unwrap());
        let document = app.active.document.as_mut().unwrap();
        document.begin_cell_edit(1, 1, b"2".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "saved as".into();
        assert!(app.save_as_picker_result(Some(destination.clone())));
        let _ = finish_app_save(&mut app, &ctx, &mut frame);
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(document.session.path(), destination);
        assert!(!document.session.dialect.has_header);
        assert_eq!(document.data_start, 0);
        assert!(!document.is_dirty());
        assert_eq!(app.active.delimiter_mode, DelimiterMode::Auto);
        assert_eq!(app.active.header_mode, HeaderMode::NoHeader);
        assert_eq!(
            fs::read(&source).unwrap(),
            b"\xEF\xBB\xBFsaved,1\nsecond,2\n"
        );
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"\xEF\xBB\xBFsaved,1\nsecond,saved as\n"
        );

        app.active.document.as_mut().unwrap().shutdown();
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
        app.active.header_mode = HeaderMode::FirstRow;
        app.open_path(path.clone()).unwrap();
        app.active.find_bar_open = true;
        app.active.replace_expanded = true;
        {
            let document = app.active.document.as_mut().unwrap();
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
        assert!(app.active.document.is_none());
        assert!(!app.active.find_bar_open);
        assert!(!app.active.replace_expanded);
        assert!(
            app.active
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("could not reload it"))
        );
        assert_eq!(fs::read(&moved).unwrap(), b"renamed,value\nfirst,1\n");

        fs::remove_file(moved).unwrap();
    }

    #[test]
    fn filtered_page_start_freezes_a_changed_source() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("filter-source-change.csv");
        fs::write(&source, b"name,status\nAda,keep\nBob,keep\n").unwrap();
        let mut app = individual_edit_app(&source);
        let document = app.active.document.as_mut().unwrap();
        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"keep".to_vec(),
            ))
            .unwrap();
        finish_filter(document);
        document.stop_filter_read();
        fs::write(&source, b"name,status\nChanged,keep\n").unwrap();
        assert!(
            document
                .start_filter_read(super::FilterReadWindow {
                    start_match: 0,
                    count: 1
                })
                .is_err()
        );
        assert!(document.source_changed);
        assert!(document.filter_index.is_none());
        assert!(document.filtered_rows.is_empty());
        assert!(!document.filter_active());
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
        app.active.header_mode = HeaderMode::FirstRow;
        app.open_path(path.clone()).unwrap();
        app.active
            .document
            .as_mut()
            .unwrap()
            .rename_header(0, "renamed".into())
            .unwrap();
        fs::rename(&path, &moved).unwrap();

        assert!(!app.save_current());
        let document = app.active.document.as_ref().unwrap();
        assert!(document.source_changed);
        assert!(document.is_dirty());
        assert_eq!(document.index_status(), "Source changed");
        assert!(document.index.is_none());
        assert!(document.buffered_rows.is_empty());
        let viewport_start = document.viewport_start;

        let ctx = egui::Context::default();
        app.apply(&ctx, Action::PageDown);
        assert_eq!(app.active.notice.as_deref(), Some(SOURCE_CHANGED_NOTICE));
        assert_eq!(
            app.active.document.as_ref().unwrap().viewport_start,
            viewport_start
        );

        app.apply(&ctx, Action::DiscardChanges);
        fs::rename(&moved, &path).unwrap();
        app.reload_document();
        let document = app.active.document.as_ref().unwrap();
        assert!(!document.source_changed);
        assert!(!document.is_dirty());
        assert_eq!(document.session.first_rows[1].fields[0], b"first");

        app.active
            .document
            .as_mut()
            .unwrap()
            .rename_header(0, "save_as_name".into())
            .unwrap();
        fs::rename(&path, &moved).unwrap();
        assert!(!app.save_as_picker_result(Some(destination.clone())));
        let document = app.active.document.as_ref().unwrap();
        assert!(document.source_changed);
        assert!(document.is_dirty());
        assert!(document.index.is_none());
        assert!(document.buffered_rows.is_empty());
        let viewport_start = document.viewport_start;
        app.apply(&ctx, Action::PageDown);
        assert_eq!(app.active.notice.as_deref(), Some(SOURCE_CHANGED_NOTICE));
        assert_eq!(
            app.active.document.as_ref().unwrap().viewport_start,
            viewport_start
        );
        assert!(!destination.exists());

        app.apply(&ctx, Action::DiscardChanges);
        fs::rename(&moved, &path).unwrap();
        app.active.document.as_mut().unwrap().shutdown();
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
        app.active.header_mode = HeaderMode::FirstRow;
        app.open_path(source.clone()).unwrap();
        let document = app.active.document.as_mut().unwrap();
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
        assert!(app.active.close_confirmation_open);
        assert!(
            close_output
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .unwrap()
                .commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::CancelClose))
        );
        app.active.close_after_save = true;
        app.keep_editing();
        assert!(!app.active.close_confirmation_open);
        assert!(!app.active.close_after_save);
        app.open_path(other.clone()).unwrap();
        app.handle_dropped_paths(vec![Some(source.clone())]);
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active.document.as_ref().unwrap().session.path(), source);
        assert_eq!(
            app.active
                .document
                .as_mut()
                .unwrap()
                .start_filtered_export(destination.clone())
                .unwrap_err(),
            "Apply a filter before exporting rows."
        );

        app.active
            .document
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
        assert!(app.active.close_after_save);
        assert!(!app.active.close_confirmation_open);
        assert!(
            saving_close_output
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .unwrap()
                .commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::CancelClose))
        );
        let document = app.active.document.as_mut().unwrap();
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
            "Finish closing the document, or keep editing, before opening another file."
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        while !app
            .active
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
        assert!(app.active.close_confirmation_open);
        assert!(!app.active.close_after_save);
        let document = app.active.document.as_mut().unwrap();
        assert_eq!(document.session.path(), source);
        assert!(document.is_dirty());
        assert_eq!(
            document
                .cell_edits
                .get(&(1, 1))
                .map(|value| value.as_slice()),
            Some(b"edited".as_slice())
        );
        assert!(document.save_status.is_none());

        app.active.close_confirmation_open = false;
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_save_as(destination.clone())
            .unwrap();
        app.active.close_after_save = true;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !app
            .active
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
        // Inspect the saved/reopened tab before the approved quit retires it.
        app.poll_active_document(&ctx, true);
        let saved_tab = app
            .document_tabs()
            .find(|tab| {
                tab.document
                    .as_ref()
                    .is_some_and(|doc| doc.logical_path == destination)
            })
            .unwrap();
        let saved_document = saved_tab.document.as_ref().unwrap();
        assert_eq!(saved_document.session.path(), destination);
        assert_eq!(saved_document.column_name(0), "renamed");
        assert!(!saved_document.is_dirty());
        assert_eq!(saved_tab.delimiter_mode, DelimiterMode::Auto);
        assert_eq!(saved_tab.header_mode, HeaderMode::FirstRow);
        let _ = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        let saved_output = finish_document_cleanup(&mut app, &ctx);
        assert!(
            saved_output
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .unwrap()
                .commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::Close))
        );
        assert!(app.document_tabs().all(|tab| tab.document.is_none()));
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"renamed,value\nfirst,edited\n"
        );
        assert_eq!(
            save_as_file_name(Path::new("report.csv")),
            "report-edited.csv"
        );

        assert!(app.document_tabs().all(|tab| tab.document.is_none()));
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
        let document = app.active.document.as_mut().unwrap();
        document.logical_path = source.clone();
        document.working_copy = Some(working_copy);
        app.active.close_confirmation_open = true;

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

        assert!(app.active.document.is_none());
        finish_document_cleanup(&mut app, &ctx);
        assert!(!working_directory.exists());
        assert_eq!(fs::read(&source).unwrap(), b"name\noriginal\n");
        assert!(
            output
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .unwrap()
                .commands
                .iter()
                .all(|command| !matches!(command, egui::ViewportCommand::Close))
        );
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
        app.active.header_mode = HeaderMode::FirstRow;
        app.open_path(first.clone()).unwrap();
        let document = app.active.document.as_mut().unwrap();
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

        app.active.notice = Some(AppMessage::warning("unchanged"));
        app.export_picker_result(None);
        assert_eq!(app.active.notice.as_deref(), Some("unchanged"));
        app.export_picker_result(Some(first.clone()));
        assert_eq!(
            app.active.notice.as_deref(),
            Some("export destination must differ from the source file")
        );
        app.export_picker_result(Some(cancelled.clone()));
        let document = app.active.document.as_mut().unwrap();
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
        app.open_path(second.clone()).unwrap();
        assert_eq!(app.active.document.as_ref().unwrap().session.path(), second);
        app.open_path(first.clone()).unwrap();
        let document = app.active.document.as_mut().unwrap();
        assert_eq!(document.session.path(), first);
        assert!(document.export_job.is_some());

        document.cancel_filtered_export();
        finish_filtered_export(document);
        app.open_path(second.clone()).unwrap();
        let document = app.active.document.as_ref().unwrap();
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

        app.active.document.as_mut().unwrap().shutdown();
        for path in [first, second, reopened] {
            if path.exists() {
                fs::remove_file(path).unwrap();
            }
        }
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
        app.active.header_mode = HeaderMode::FirstRow;
        app.open_path(first.clone()).unwrap();
        let document = app.active.document.as_mut().unwrap();
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
        app.active.filters_open = true;
        app.active.filter_rules[0].column_input = "2".into();
        app.active.filter_rules[0].value_input = "keep".into();
        app.active
            .filter_rules
            .push(super::FilterRuleDraft::default());
        app.open_path(second.clone()).unwrap();
        assert!(!app.active.filters_open);
        assert_eq!(
            app.active.filter_rules,
            vec![super::FilterRuleDraft::default()]
        );
        assert!(!app.active.document.as_ref().unwrap().filter_active());
        assert!(app.active.document.as_ref().unwrap().filter_read.is_none());
        assert!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .pending_filter_read
                .is_none()
        );

        app.active.document.as_mut().unwrap().shutdown();
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
    fn long_cells_auto_fit_without_truncation_in_regular_and_wide_tables() {
        let directory = tempfile::tempdir().unwrap();
        let value = "Long description ".repeat(10) + "VISIBLE END";
        for total_columns in [34, 65] {
            let path = directory.path().join(format!("long-{total_columns}.csv"));
            let mut values = vec!["short"; total_columns];
            values[0] = &value;
            let original = format!(
                "{}\n{}\n",
                vec!["description"; total_columns].join(","),
                values.join(",")
            );
            fs::write(&path, &original).unwrap();
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
            let mut grid_view = EguiGridState::default();
            let render = |document: &mut Document, grid_view: &mut EguiGridState| {
                ctx.run(grid_input_with_width(2400.0), |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        show_grid_with_view(ui, document, grid_view).unwrap();
                    });
                })
            };
            for _ in 0..2 {
                let _ = render(&mut document, &mut grid_view);
            }
            // Exercise the source value and a longer unsaved replacement.
            for expected in [&value, &(value.clone() + " with an unsaved ending")] {
                if expected != &value {
                    commit_test_cell(&mut document, 1, 0, expected);
                }
                grid_view.auto_fit_columns = true;
                for _ in 0..4 {
                    let _ = render(&mut document, &mut grid_view);
                }
                let output = render(&mut document, &mut grid_view);
                let text = output
                    .shapes
                    .iter()
                    .find_map(|shape| match &shape.shape {
                        egui::Shape::Text(text) if text.galley.text() == expected => {
                            Some((shape.clip_rect, text))
                        }
                        _ => None,
                    })
                    .expect("the full long cell must be painted");
                assert_eq!(text.1.galley.rows.len(), 1, "cells must stay on one row");
                let bounds = text.1.galley.rect.translate(text.1.pos.to_vec2());
                assert!(
                    text.0.expand(1.0).contains_rect(bounds),
                    "auto-fit clips the cell ending: {bounds:?} in {:?}",
                    text.0
                );
                let label = format!("Select row 1, column 1 (description): {expected}");
                assert!(
                    output
                        .platform_output
                        .accesskit_update
                        .as_ref()
                        .unwrap()
                        .nodes
                        .iter()
                        .any(|(_, node)| node.label() == Some(label.as_str()))
                );
                document.selection = Some(GridSelection::Cell { row: 1, column: 0 });
                assert_eq!(document.copy_selection_text().unwrap(), *expected);
            }
            assert_eq!(fs::read_to_string(path).unwrap(), original);
        }
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
            app.active.document = Some(
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
                let document = app.active.state.document.as_mut().unwrap();
                document.set_column_shown(64, false).unwrap();
                let _ = render_grid_with_view(&ctx, document, &mut app.active.view.grid);
                document.set_column_shown(64, true).unwrap();
            }
            let before = render_grid_with_view(
                &ctx,
                app.active.state.document.as_mut().unwrap(),
                &mut app.active.view.grid,
            );
            let initial_width = header_width(&before, 0);
            let command = click_column_manager_control(
                egui::accesskit::Role::Button,
                "Auto-fit columns",
                app.active.state.document.as_ref().unwrap(),
            );
            app.apply_column_command(&ctx, command);
            let document = app.active.state.document.as_mut().unwrap();
            let grid_view = &mut app.active.view.grid;
            for _ in 0..total_columns.div_ceil(MAX_RENDERED_COLUMNS) + 2 {
                let _ = render_grid_with_view(&ctx, document, grid_view);
            }
            assert!(grid_view.columns_to_fit.is_empty());
            let after = render_grid_with_view(&ctx, document, grid_view);
            assert!(header_width(&after, 0) > initial_width + 100.0);
            if total_columns <= MAX_RENDERED_COLUMNS {
                continue;
            }
            assert_eq!(grid_view.fitted_column_widths.len(), total_columns);
            let last_width = grid_view.fitted_column_widths[&(total_columns - 1)];
            assert!(last_width > 200.0);
            document.reveal_cell = Some((1, total_columns - 1));
            let _ = render_grid_with_view(&ctx, document, grid_view);
            let last = render_grid_with_view(&ctx, document, grid_view);
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
            let _ = render_grid_with_view(&ctx, document, grid_view);
            let reordered = render_grid_with_view(&ctx, document, grid_view);
            assert!(header_width(&reordered, total_columns - 1) >= f64::from(last_width) - 1.0);
            assert!(document.cell_edits.is_empty());
            assert!(document.header_renames.is_empty());
            if total_columns == 65 {
                document
                    .rename_header(total_columns - 1, "short".into())
                    .unwrap();
                grid_view.auto_fit_columns = true;
                let _ = render_grid_with_view(&ctx, document, grid_view);
                let refitted = render_grid_with_view(&ctx, document, grid_view);
                assert!(header_width(&refitted, total_columns - 1) < f64::from(last_width) - 100.0);
                assert_eq!(grid_view.fitted_column_widths[&(total_columns - 1)], 80.0);
                document.set_column_shown(0, true).unwrap();
                document.reveal_cell = Some((1, total_columns - 1));
                let _ = render_grid_with_view(&ctx, document, grid_view);
                let restored = render_grid_with_view(&ctx, document, grid_view);
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
        let mut grid_view = EguiGridState::default();
        let render = |document: &mut Document, grid_view: &mut EguiGridState| {
            ctx.run(grid_input_with_width(600.0), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid_with_view(ui, document, grid_view).unwrap();
                });
            })
        };
        grid_view.auto_fit_columns = true;
        for _ in 0..3 {
            let _ = render(&mut document, &mut grid_view);
        }
        document.view_column(64).unwrap();
        for _ in 0..3 {
            let output = render(&mut document, &mut grid_view);
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

    fn await_index_worker(doc: &Document) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !doc.job.as_ref().unwrap().progress().done {
            assert!(Instant::now() < deadline, "index timed out");
            std::thread::yield_now();
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
    fn tool_dialog_escape_only_closes_the_front_window() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), "name,amount\nAvery,20\n").unwrap();
        let mut document = Document::open(file.path(), OpenOptions::default()).unwrap();
        finish_index(&mut document);
        let ctx = egui::Context::default();
        let mut columns_open = true;
        let mut filters_open = true;
        let mut search = String::new();
        let mut rules = vec![super::FilterRuleDraft::default()];
        let mut match_case = false;
        let escape_input = || egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..grid_input()
        };
        for _ in 0..2 {
            let _ = ctx.run(grid_input(), |ctx| {
                show_column_manager(ctx, &mut columns_open, &mut search, &document);
                show_filter_manager(
                    ctx,
                    &mut filters_open,
                    &mut rules,
                    &mut match_case,
                    &document,
                );
                ctx.move_to_top(egui::LayerId::new(
                    egui::Order::Middle,
                    egui::Id::new("quarry-filter-manager"),
                ));
            });
        }
        let _ = ctx.run(escape_input(), |ctx| {
            show_column_manager(ctx, &mut columns_open, &mut search, &document);
            show_filter_manager(
                ctx,
                &mut filters_open,
                &mut rules,
                &mut match_case,
                &document,
            );
        });
        assert!(columns_open);
        assert!(!filters_open);

        // A closed window's retained layer must not block the remaining tool window.
        let _ = ctx.run(grid_input(), |ctx| {
            show_column_manager(ctx, &mut columns_open, &mut search, &document);
        });
        let _ = ctx.run(escape_input(), |ctx| {
            show_column_manager(ctx, &mut columns_open, &mut search, &document);
        });
        assert!(!columns_open);

        columns_open = true;
        filters_open = true;
        let modal_id = egui::Id::new("column-tool-test-modal");
        let _ = ctx.run(grid_input(), |ctx| {
            show_column_manager(ctx, &mut columns_open, &mut search, &document);
            show_filter_manager(
                ctx,
                &mut filters_open,
                &mut rules,
                &mut match_case,
                &document,
            );
            egui::Modal::new(modal_id).show(ctx, |ui| {
                ui.label("Confirmation");
            });
        });
        let mut close_modal = false;
        let _ = ctx.run(escape_input(), |ctx| {
            show_column_manager(ctx, &mut columns_open, &mut search, &document);
            show_filter_manager(
                ctx,
                &mut filters_open,
                &mut rules,
                &mut match_case,
                &document,
            );
            close_modal = egui::Modal::new(modal_id)
                .show(ctx, |ui| {
                    ui.label("Confirmation");
                })
                .should_close();
        });
        assert!(columns_open && filters_open);
        assert!(close_modal);
    }

    #[test]
    fn column_manager_exposes_clear_list_controls() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-column-a11y-{name}.csv"));
        let headers = (1..=40)
            .map(|column| {
                if column == 2 {
                    format!("c2_{}", "long_column_name_".repeat(12))
                } else {
                    format!("c{column}")
                }
            })
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
        app.active.document = Some(document);
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        configure_style(&ctx);
        let mut open = true;
        let mut search = String::new();
        let mut command = None;
        for _ in 0..2 {
            let _ = ctx.run(grid_input(), |ctx| {
                show_column_manager(
                    ctx,
                    &mut open,
                    &mut search,
                    app.active.document.as_ref().unwrap(),
                );
            });
        }
        let output = ctx.run(grid_input(), |ctx| {
            command = show_column_manager(
                ctx,
                &mut open,
                &mut search,
                app.active.document.as_ref().unwrap(),
            );
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
        assert!((row_height - 28.0).abs() < f64::EPSILON);
        assert!((row_stride - 28.0).abs() < f64::EPSILON);
        assert_eq!(drag_bounds[0].x0, drag_bounds[1].x0);
        assert_eq!(drag_bounds[0].x1, drag_bounds[1].x1);
        assert!(drag_bounds[0].x1 - drag_bounds[0].x0 > 300.0);
        let name_left = [0, 1].map(|column| {
            let name = app.active.document.as_ref().unwrap().column_name(column);
            output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == name => {
                        Some(text.pos.x + text.galley.rect.left())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{name} should be rendered"))
        });
        assert_eq!(
            name_left[0], name_left[1],
            "Short and truncated names should align left"
        );

        for _ in 0..2 {
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(860.0, 540.0),
                    )),
                    ..grid_input()
                },
                |ctx| {
                    show_column_manager(
                        ctx,
                        &mut open,
                        &mut search,
                        app.active.document.as_ref().unwrap(),
                    );
                },
            );
            let tree = output.platform_output.accesskit_update.unwrap();
            for label in ["Search columns", "Auto-fit columns", "Done", "Close"] {
                let node = tree
                    .nodes
                    .iter()
                    .find(|(_, node)| node.label() == Some(label))
                    .unwrap_or_else(|| panic!("{label} should remain present"));
                let bounds = node.1.bounds().unwrap();
                assert!(!node.1.is_hidden());
                assert!(
                    bounds.y0 >= 0.0 && bounds.y1 <= 540.0,
                    "{label}: {bounds:?}"
                );
            }
        }

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
            show_column_manager(
                ctx,
                &mut open,
                &mut search,
                app.active.document.as_ref().unwrap(),
            );
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
            app.active.document.as_ref().unwrap(),
        );
        app.apply_column_command(&ctx, command);
        assert!(app.active.document.as_ref().unwrap().columns.hidden[0]);

        let command = click_column_manager_control(
            egui::accesskit::Role::Button,
            "Reset columns",
            app.active.document.as_ref().unwrap(),
        );
        app.apply_column_command(&ctx, command);
        assert_eq!(
            app.active.document.as_ref().unwrap().columns.order,
            (0..40).collect::<Vec<_>>()
        );
        assert!(
            app.active
                .document
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
            app.active.document.as_ref().unwrap(),
        );
        app.apply_column_command(&ctx, command);
        assert!(app.active.view.grid.auto_fit_columns);

        app.apply_column_command(
            &ctx,
            ColumnCommand::Move {
                column: 39,
                position: 0,
            },
        );
        assert_eq!(
            &app.active.document.as_ref().unwrap().columns.order[..3],
            &[39, 0, 1]
        );

        ctx.memory_mut(|memory| memory.request_focus(egui::Id::new(super::COLUMN_SEARCH_INPUT_ID)));
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
                show_column_manager(
                    ctx,
                    &mut open,
                    &mut search,
                    app.active.document.as_ref().unwrap(),
                );
            },
        );
        assert!(!open);
        assert_eq!(search, "c40");
        assert!(
            !ctx.memory(|memory| memory.has_focus(egui::Id::new(super::COLUMN_SEARCH_INPUT_ID)))
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
        finish_index(app.active.document.as_mut().unwrap());

        click_grid_control(
            "Select file column 1 (first)",
            app.active.document.as_mut().unwrap(),
        );
        click_grid_control(
            "Select file column 2 (last)",
            app.active.document.as_mut().unwrap(),
        );
        assert_eq!(
            app.active.document.as_ref().unwrap().selected_columns,
            BTreeSet::from([0, 1])
        );

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let header_target = grid_control_id(
            &ctx,
            "Select file column 2 (last)",
            app.active.document.as_mut().unwrap(),
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
                    opened_dialog = show_grid(ui, app.active.document.as_mut().unwrap()).unwrap();
                });
            },
        );
        let menu_output = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                opened_dialog = show_grid(ui, app.active.document.as_mut().unwrap()).unwrap();
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
                    opened_dialog = show_grid(ui, app.active.document.as_mut().unwrap()).unwrap();
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

        let document = app.active.document.as_ref().unwrap();
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
        let output = render_grid_with_view(
            &focus_ctx,
            app.active.state.document.as_mut().unwrap(),
            &mut app.active.view.grid,
        );
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
        finish_index(app.active.document.as_mut().unwrap());
        assert!(parse_move_position("0", 4, 2).is_err());
        assert!(parse_move_position("4", 4, 2).is_err());
        assert_eq!(
            app.active
                .document
                .as_mut()
                .unwrap()
                .start_delete_columns(vec![0, 1, 2, 3])
                .unwrap_err(),
            "At least one column must remain."
        );
        assert!(app.active.document.as_ref().unwrap().working_copy.is_none());

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
                    &mut app.active.state.sort_match_case,
                    app.active.state.document.as_ref().unwrap(),
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
        app.active.format_draft = Some((DelimiterMode::Tab, HeaderMode::NoHeader, None));
        finish_structural_edit(&mut app);
        assert_eq!(app.active.format_draft, None);
        let document = app.active.document.as_ref().unwrap();
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
        finish_index(app.active.document.as_mut().unwrap());
        let redo_path = app
            .active
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
            let document = app.active.document.as_mut().unwrap();
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
        finish_index(app.active.document.as_mut().unwrap());
        app.apply_delete_columns(vec![0, 2]);
        assert!(app.active.notice.is_none());
        finish_structural_edit(&mut app);
        let document = app.active.document.as_ref().unwrap();
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
        finish_index(app.active.document.as_mut().unwrap());
        assert_eq!(
            app.active.document.as_ref().unwrap().session.first_rows[0].fields,
            ["b", "d", "a", "c"].map(|field| field.as_bytes().to_vec())
        );
        app.swap_structural_history(true).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        assert_eq!(
            app.active.document.as_ref().unwrap().session.first_rows[0].fields,
            ["d", "c"].map(|field| field.as_bytes().to_vec())
        );
    }

    fn finish_duplicate_search(app: &mut QuarryApp) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let document = app.active.document.as_mut().unwrap();
            assert!(document.poll_structural_edit().unwrap().is_none());
            if !matches!(
                document.structural_job,
                Some(super::StructuralJob::FindingDuplicates { .. })
            ) {
                break;
            }
            assert!(Instant::now() < deadline, "duplicate search timed out");
            std::thread::yield_now();
        }
    }

    #[test]
    fn duplicates_preview_effective_values_and_remove_with_history_and_save_as() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("duplicates.csv");
        let original = b"\xEF\xBB\xBFkey,status,note\r\na,keep,\"first\nline\"\r\nA,keep,second\r\nb,keep,last\r\na,skip,other\r\n";
        fs::write(&source, original).unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        app.open_path_with_options(
            source.clone(),
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        let document = app.active.document.as_mut().unwrap();
        document.rename_header(2, "memo".into()).unwrap();
        document.begin_cell_edit(3, 0, b"b".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "a".into();

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut dialog = StructuralDialog::duplicates(vec![0, 1]);
        let (action, _) = click_sort_dialog_control(&ctx, &mut dialog, &mut app, "Find duplicates");
        assert_eq!(action, Some(StructuralDialogAction::Apply));
        app.open_structural_dialog(dialog);
        app.apply_structural_dialog_action(action.unwrap());
        assert!(app.active.notice.is_none());
        finish_duplicate_search(&mut app);
        let document = app.active.document.as_mut().unwrap();
        let Some(super::StructuralJob::DuplicatePreview { summary, .. }) = &document.structural_job
        else {
            panic!("expected duplicate preview");
        };
        assert_eq!((summary.duplicate_rows, summary.retained_rows), (2, 2));
        let output = ctx.run(grid_input(), |ctx| {
            super::show_duplicate_preview(ctx, summary);
        });
        for label in [
            "Extra duplicate rows: 2",
            "Rows to keep: 2",
            "Remove extra rows",
            "Cancel",
        ] {
            assert!(
                output
                    .platform_output
                    .accesskit_update
                    .as_ref()
                    .unwrap()
                    .nodes
                    .iter()
                    .any(|(_, node)| node.label() == Some(label) || node.value() == Some(label)),
                "missing accessible preview label {label}"
            );
        }
        let (target, _) = accessible_button(&output, "Remove extra rows");
        let mut preview_action = None;
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
            |ctx| preview_action = super::show_duplicate_preview(ctx, summary),
        );
        assert_eq!(preview_action, Some(StructuralDialogAction::Apply));
        assert_eq!(document.session.path(), source);
        assert_eq!(document.cell_edits[&(3, 0)].as_slice(), b"a");
        assert!(!document.can_undo());
        assert!(document.begin_cell_edit(1, 0, b"a".to_vec()).is_err());
        assert!(document.start_save().is_err());
        let history_len = document.edit_history.undo.len();
        assert_eq!(fs::read(&source).unwrap(), original);

        app.confirm_duplicate_removal().unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        let document = app.active.document.as_ref().unwrap();
        let expected = b"\xEF\xBB\xBFkey,status,memo\r\na,keep,\"first\nline\"\r\na,skip,other\r\n";
        assert_eq!(fs::read(document.session.path()).unwrap(), expected);
        assert!(document.is_dirty());
        assert_eq!(document.selected_columns, BTreeSet::from([0, 1]));
        app.swap_structural_history(false).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(document.session.path(), source);
        assert_eq!(document.cell_edits[&(3, 0)].as_slice(), b"a");
        assert_eq!(document.column_name(2), "memo");
        assert_eq!(document.edit_history.undo.len(), history_len);
        app.swap_structural_history(true).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        let saved = directory.path().join("saved.csv");
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_save_as(saved.clone())
            .unwrap();
        finish_app_save(&mut app, &ctx, &mut eframe::Frame::_new_kittest());
        assert_eq!(fs::read(saved).unwrap(), expected);
        assert_eq!(fs::read(source).unwrap(), original);
        assert!(!app.active.document.as_ref().unwrap().is_dirty());
    }

    #[test]
    fn duplicates_cancel_no_match_and_discard_preserve_document_and_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("duplicates.csv");
        let original = b"key,value\na,first\nA,second\n";
        fs::write(&source, original).unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        app.open_path_with_options(
            source.clone(),
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_find_duplicates(vec![0], CaseSensitivity::Sensitive)
            .unwrap();
        finish_duplicate_search(&mut app);
        let document = app.active.document.as_ref().unwrap();
        assert!(document.structural_job.is_none());
        assert!(document.working_copy.is_none());
        assert!(!document.is_dirty());
        assert!(
            document
                .structural_status
                .as_ref()
                .unwrap()
                .starts_with("No duplicates")
        );

        app.active
            .document
            .as_mut()
            .unwrap()
            .start_find_duplicates(vec![0], CaseSensitivity::Insensitive)
            .unwrap();
        finish_duplicate_search(&mut app);
        let document = app.active.document.as_mut().unwrap();
        let temp = document
            .working_copy
            .as_ref()
            .unwrap()
            .directory
            .path()
            .to_path_buf();
        document.cancel_structural_edit();
        assert!(!temp.exists());
        assert!(document.structural_job.is_none());
        assert!(!document.is_dirty());

        document
            .start_find_duplicates(vec![0], CaseSensitivity::Insensitive)
            .unwrap();
        document.cancel_structural_edit();
        finish_duplicate_search(&mut app);
        assert!(app.active.document.as_ref().unwrap().working_copy.is_none());
        assert!(!app.active.document.as_ref().unwrap().is_dirty());

        app.active
            .document
            .as_mut()
            .unwrap()
            .start_find_duplicates(vec![0], CaseSensitivity::Insensitive)
            .unwrap();
        finish_duplicate_search(&mut app);
        app.confirm_duplicate_removal().unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        let working = app
            .active
            .document
            .as_ref()
            .unwrap()
            .session
            .path()
            .to_path_buf();
        app.apply(&egui::Context::default(), Action::DiscardChanges);
        finish_index(app.active.document.as_mut().unwrap());
        assert!(!working.exists());
        assert_eq!(app.active.document.as_ref().unwrap().session.path(), source);
        assert!(!app.active.document.as_ref().unwrap().is_dirty());
        assert_eq!(fs::read(source).unwrap(), original);
    }

    #[test]
    fn duplicate_search_without_matches_releases_its_owned_candidate_with_existing_history() {
        use quarry_core::{CleanupOperation, ColumnCleanup};
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("unique.csv");
        let original = b"id,value\na, first \nb, second \n";
        fs::write(&source, original).unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.active.document.as_mut().unwrap());
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_cleanup(ColumnCleanup {
                columns: vec![1],
                operation: CleanupOperation::Trim,
            })
            .unwrap();
        finish_structural_edit(&mut app);
        let document = app.active.document.as_ref().unwrap();
        let current = document.session.path().to_owned();
        let root = document
            .working_copy
            .as_ref()
            .unwrap()
            .directory
            .path()
            .to_owned();
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_find_duplicates(vec![0], CaseSensitivity::Sensitive)
            .unwrap();
        finish_duplicate_search(&mut app);
        let document = app.active.document.as_ref().unwrap();
        assert!(document.structural_job.is_none());
        assert_eq!(document.session.path(), current);
        assert_eq!(
            fs::read_dir(&root).unwrap().count(),
            1,
            "a zero-duplicate candidate must not accumulate beside current/history files"
        );
        assert_eq!(fs::read(source).unwrap(), original);
    }

    #[test]
    fn duplicates_preview_source_conflict_and_reopen_failure_do_not_consume_history() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("duplicates.csv");
        fs::write(&source, b"key,value\na,first\na,second\n").unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        app.open_path_with_options(
            source.clone(),
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        app.active
            .document
            .as_mut()
            .unwrap()
            .rename_header(1, "memo".into())
            .unwrap();
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_find_duplicates(vec![0], CaseSensitivity::Insensitive)
            .unwrap();
        finish_duplicate_search(&mut app);
        let document = app.active.document.as_ref().unwrap();
        let Some(super::StructuralJob::DuplicatePreview { summary, .. }) = &document.structural_job
        else {
            panic!("expected preview");
        };
        let candidate = summary.destination.clone();
        let candidate_bytes = fs::read(&candidate).unwrap();
        fs::remove_file(&candidate).unwrap();
        let history_len = document.edit_history.undo.len();
        assert!(app.confirm_duplicate_removal().is_err());
        assert_eq!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .edit_history
                .undo
                .len(),
            history_len
        );
        assert_eq!(app.active.document.as_ref().unwrap().session.path(), source);
        fs::write(&candidate, &candidate_bytes).unwrap();
        fs::write(&source, b"key,value\nexternal,changed\n").unwrap();
        assert_eq!(
            app.confirm_duplicate_removal().unwrap_err(),
            SOURCE_CHANGED_NOTICE
        );
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(document.edit_history.undo.len(), history_len);
        assert_eq!(document.column_name(1), "memo");
        // Recreating the same bytes produces a different file identity.
        // Cleanup has no authority to delete that replacement.
        assert_eq!(fs::read(&candidate).unwrap(), candidate_bytes);
        assert!(!document.working_cleanup.take_errors().is_empty());
        assert!(document.source_changed);
        assert_eq!(fs::read(source).unwrap(), b"key,value\nexternal,changed\n");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn duplicate_confirmation_adopts_completed_index_without_rescanning() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("dedup-adopt.csv");
        let original = b"key,value\na,first\nb,keep\na,dup\nc,last\n";
        fs::write(&source, original).unwrap();
        let mut app = individual_edit_app(&source);
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_find_duplicates(vec![0], CaseSensitivity::Insensitive)
            .unwrap();
        finish_duplicate_search(&mut app);
        let candidate = {
            let document = app.active.document.as_ref().unwrap();
            let Some(super::StructuralJob::DuplicatePreview { summary, .. }) =
                &document.structural_job
            else {
                panic!("expected duplicate preview");
            };
            assert!(
                summary.output_index.is_some(),
                "macOS duplicate output should emit a completed index"
            );
            summary.destination.clone()
        };

        // Tampering with the published candidate before confirming must fail adoption without
        // consuming the preview or mutating undo history.
        let original_candidate = fs::read(&candidate).unwrap();
        let backup = directory.path().join("candidate-backup.csv");
        fs::rename(&candidate, &backup).unwrap();
        fs::write(&candidate, b"key,value\ntampered,1\n").unwrap();
        assert!(app.confirm_duplicate_removal().is_err());
        assert!(matches!(
            app.active.document.as_ref().unwrap().structural_job,
            Some(super::StructuralJob::DuplicatePreview { .. })
        ));
        assert_eq!(app.active.document.as_ref().unwrap().session.path(), source);

        // Restoring the exact published file identity lets confirmation adopt the index
        // immediately with zero background re-indexing job.
        fs::remove_file(&candidate).unwrap();
        fs::rename(&backup, &candidate).unwrap();
        app.confirm_duplicate_removal().unwrap();
        let document = app.active.document.as_ref().unwrap();
        assert!(
            document.job.is_none(),
            "confirming duplicate removal must adopt output_index without starting another index scan"
        );
        assert!(document.progress.done);
        assert_eq!(document.progress.bytes_scanned, document.session.file_size);
        assert_eq!(document.progress.rows_scanned, 4);
        assert_eq!(
            fs::read(document.session.path()).unwrap(),
            original_candidate
        );
    }

    #[test]
    fn delete_selected_rows_uses_working_copy_and_structural_history() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("delete-rows.csv");
        let original = b"id,name\n1,Ada\n2,Grace\n3,Linus\n4,Margaret\n";
        fs::write(&source, original).unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.active.document.as_mut().unwrap());
        {
            let document = app.active.document.as_mut().unwrap();
            Arc::make_mut(&mut document.cell_edits).insert((3, 1), b"Edited".to_vec().into());
            document.selected_rows.ranges = vec![2..=2, 4..=4];
            document.selected_rows.anchor = Some(2);
            document.selection = Some(GridSelection::Row { row: 2 });
        }

        app.apply_delete_rows(vec![2..=2, 4..=4]);
        assert!(app.active.notice.is_none());
        finish_structural_edit(&mut app);
        let document = app.active.document.as_ref().unwrap();
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
        finish_index(app.active.document.as_mut().unwrap());
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(document.session.first_rows.len(), 5);
        assert_eq!(
            document
                .cell_edits
                .get(&(3, 1))
                .map(|value| value.as_slice()),
            Some(b"Edited".as_slice())
        );
        assert_eq!(fs::read(&source).unwrap(), original);

        app.swap_structural_history(true).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(document.session.first_rows.len(), 3);
        assert_eq!(document.session.first_rows[2].fields[1], b"Edited");
        assert_eq!(fs::read(&source).unwrap(), original);

        app.apply(&egui::Context::default(), Action::DiscardChanges);
        finish_index(app.active.document.as_mut().unwrap());
        assert_eq!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .session
                .first_rows
                .len(),
            5
        );
        assert!(!app.active.document.as_ref().unwrap().is_dirty());
        assert_eq!(fs::read(&source).unwrap(), original);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rejected_sort_handoff_preserves_history_and_external_destination() {
        for retain_redo in [false, true] {
            for replace_published in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let source = directory.path().join("source.csv");
                let original = b"key,name\nb,second\na,first\n";
                let external = b"external,value\nkeep,7\n";
                fs::write(&source, original).unwrap();
                let mut app = individual_edit_app(&source);
                app.active
                    .document
                    .as_mut()
                    .unwrap()
                    .start_sort_rows(
                        0,
                        super::SortDirection::Ascending,
                        SortMode::Text,
                        CaseSensitivity::Insensitive,
                    )
                    .unwrap();
                finish_structural_edit(&mut app);
                if retain_redo {
                    app.swap_structural_history(false).unwrap();
                    finish_index(app.active.document.as_mut().unwrap());
                }
                let document = app.active.document.as_ref().unwrap();
                let current_path = document.session.path().to_path_buf();
                let current_bytes = fs::read(&current_path).unwrap();
                let history = document.working_copy.as_ref().unwrap();
                let expected_undo = history.undo.clone();
                let expected_redo = history.redo.clone();
                let expected_history_buttons = (document.can_undo(), document.can_redo());
                app.active
                    .document
                    .as_mut()
                    .unwrap()
                    .start_sort_rows(
                        0,
                        super::SortDirection::Descending,
                        SortMode::Text,
                        CaseSensitivity::Insensitive,
                    )
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                let destination = loop {
                    let Some(super::StructuralJob::Sorting {
                        job, destination, ..
                    }) = app
                        .active
                        .document
                        .as_ref()
                        .unwrap()
                        .structural_job
                        .as_ref()
                    else {
                        panic!("expected pending sort");
                    };
                    if job.progress().done {
                        break destination.clone();
                    }
                    assert!(Instant::now() < deadline, "sort timed out");
                    std::thread::yield_now();
                };
                if replace_published {
                    let ready = app
                        .active
                        .document
                        .as_mut()
                        .unwrap()
                        .poll_structural_edit()
                        .unwrap()
                        .unwrap();
                    assert!(ready.output_index.is_some());
                    fs::rename(&destination, directory.path().join("completed.csv")).unwrap();
                    fs::write(&destination, external).unwrap();
                    assert!(app.install_materialized_working_copy(ready).is_err());
                } else {
                    fs::write(&destination, external).unwrap();
                    assert!(
                        app.active
                            .document
                            .as_mut()
                            .unwrap()
                            .poll_structural_edit()
                            .is_err()
                    );
                }
                let document = app.active.document.as_ref().unwrap();
                assert_eq!(document.session.path(), current_path);
                assert_eq!(fs::read(&current_path).unwrap(), current_bytes);
                assert_eq!(fs::read(&source).unwrap(), original);
                assert_eq!(fs::read(&destination).unwrap(), external);
                let history = document.working_copy.as_ref().unwrap();
                assert_eq!(history.undo, expected_undo);
                assert_eq!(history.redo, expected_redo);
                assert_eq!(
                    (document.can_undo(), document.can_redo()),
                    expected_history_buttons
                );
                for retained in [expected_undo, expected_redo].into_iter().flatten() {
                    assert!(retained.path.exists());
                }
                app.active.document.as_mut().unwrap().shutdown();
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sorted_output_is_immediately_indexed_and_reuses_its_count_on_the_next_sort() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("sort-index.csv");
        let original = format!(
            "key,name\n{}",
            (0..9_000)
                .rev()
                .map(|row| format!("{row},row-{row}\n"))
                .collect::<String>()
        );
        fs::write(&source, &original).unwrap();
        let mut app = individual_edit_app(&source);
        for (direction, last_value) in [
            (super::SortDirection::Ascending, "8999"),
            (super::SortDirection::Descending, "0"),
        ] {
            app.active
                .document
                .as_mut()
                .unwrap()
                .start_sort_rows(0, direction, SortMode::Number, CaseSensitivity::Insensitive)
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let Some(super::StructuralJob::Sorting { job, .. }) = app
                    .active
                    .document
                    .as_ref()
                    .unwrap()
                    .structural_job
                    .as_ref()
                else {
                    panic!("sort job should remain active until its handoff");
                };
                if job.progress().done {
                    assert_eq!(job.progress().preparation_bytes_scanned, 0);
                    break;
                }
                assert!(Instant::now() < deadline, "sort timed out");
                std::thread::yield_now();
            }
            let ready = app
                .active
                .document
                .as_mut()
                .unwrap()
                .poll_structural_edit()
                .unwrap()
                .unwrap();
            assert!(ready.output_index.is_some());
            app.install_materialized_working_copy(ready).unwrap();
            let document = app.active.document.as_mut().unwrap();
            assert!(
                document.job.is_none(),
                "sort must not launch another index scan"
            );
            assert!(document.progress.done);
            assert_eq!(document.progress.bytes_scanned, document.session.file_size);
            assert_eq!(document.progress.rows_scanned, 9_001);
            let index = document.index.as_ref().unwrap();
            assert_eq!(index.indexed_rows(), 9_001);
            assert_eq!(index.indexed_bytes(), document.session.file_size);
            assert!(index.checkpoints().len() >= 3);
            document.navigate(9_000).unwrap();
            let (row, fields) = document
                .visible_row(document.visible_row_count() - 1)
                .unwrap();
            assert_eq!(row, 9_000);
            assert_eq!(fields[0], last_value.as_bytes());
            assert!(document.can_undo());
            assert_eq!(fs::read_to_string(&source).unwrap(), original);
        }
        app.swap_structural_history(false).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        assert_eq!(
            app.active.document.as_ref().unwrap().session.first_rows[1].fields[0],
            b"0"
        );
        assert_eq!(fs::read_to_string(&source).unwrap(), original);
        app.active.document.as_mut().unwrap().shutdown();
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
        finish_index(app.active.document.as_mut().unwrap());
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(
            document.sort_temporary_disk_estimate(),
            Some(estimate_sort_temporary_bytes(
                document
                    .session
                    .file_size
                    .saturating_add(12)
                    .saturating_mul(2)
                    .saturating_add(3),
                5,
            ))
        );
        app.active.document.as_mut().unwrap().selected_columns = BTreeSet::from([0]);

        let mut dialog = StructuralDialog::sort(0);
        assert_eq!(dialog.sort_mode, SortMode::Text);
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let output = ctx.run(grid_input(), |ctx| {
            assert_eq!(
                show_structural_dialog(
                    ctx,
                    &mut dialog,
                    &mut app.active.state.sort_match_case,
                    app.active.state.document.as_ref().unwrap(),
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
        for label in [
            "Sort as",
            "Ascending",
            "Descending",
            "Match case",
            "Details",
            "Sort",
            "Cancel",
        ] {
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
            "Uppercase and lowercase letters sort together.",
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

        let (action, output) = click_sort_dialog_control(&ctx, &mut dialog, &mut app, "Sort as");
        assert_eq!(action, None);
        let tree = output.platform_output.accesskit_update.unwrap();
        for label in [
            "Text",
            "Number",
            "Character count",
            "Word count",
            "Shuffle",
            "Reverse",
        ] {
            assert!(
                tree.nodes.iter().any(|(_, node)| {
                    node.label() == Some(label)
                        && node.supports_action(egui::accesskit::Action::Click)
                }),
                "missing accessible sort choice {label}"
            );
        }
        assert_eq!(
            click_sort_dialog_control(&ctx, &mut dialog, &mut app, "Text").0,
            None
        );
        assert!(!egui::Popup::is_any_open(&ctx));

        app.open_structural_dialog(dialog);
        app.apply_structural_dialog_action(StructuralDialogAction::Apply);
        finish_structural_edit(&mut app);
        let document = app.active.document.as_ref().unwrap();
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
        finish_index(app.active.document.as_mut().unwrap());
        let original_names = app
            .active
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
        finish_index(app.active.document.as_mut().unwrap());
        let redone_names = app
            .active
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
    fn structural_dialog_details_keep_actions_visible_and_escape_closes_popup_first() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("modal-layout.csv");
        fs::write(&source, b"name,value\nAvery,1\nAvery,2\n").unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.active.document.as_mut().unwrap());
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        super::configure_style(&ctx);
        ctx.style_mut(|style| style.animation_time = 0.0);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(860.0, 540.0));
        let mut time = 0.0;
        let mut render = |dialog: &mut StructuralDialog, events| {
            time += 0.1;
            let mut action = None;
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    time: Some(time),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    action = show_structural_dialog(
                        ctx,
                        dialog,
                        &mut app.active.state.sort_match_case,
                        app.active.state.document.as_ref().unwrap(),
                    );
                },
            );
            (action, output)
        };
        let visible = |output: &egui::FullOutput, label: &str| {
            output.shapes.iter().any(|shape| match &shape.shape {
                egui::Shape::Text(text) => {
                    text.galley.text() == label
                        && shape.clip_rect.contains_rect(text.visual_bounding_rect())
                        && screen.contains_rect(text.visual_bounding_rect())
                }
                _ => false,
            })
        };
        let mut dialog = StructuralDialog::sort(0);
        let _ = render(&mut dialog, vec![]);
        let (_, output) = render(&mut dialog, vec![]);
        let detail = "Rows equal at every level keep their original order (stable sort). Missing values sort as empty cells.";
        assert!(
            !visible(&output, detail),
            "advanced details start collapsed"
        );
        assert!(
            output.shapes.iter().any(|shape| match &shape.shape {
                egui::Shape::Text(text) =>
                    text.galley
                        .text()
                        .contains("Conservative temporary disk allowance:")
                        && shape.clip_rect.contains_rect(text.visual_bounding_rect())
                        && screen.contains_rect(text.visual_bounding_rect()),
                _ => false,
            }),
            "disk allowance stays visible without opening Details"
        );
        let _ = render(&mut dialog, filter_control_click(&output, "Details"));
        let (_, output) = render(&mut dialog, vec![]);
        assert!(
            output.shapes.iter().any(|shape| match &shape.shape {
                egui::Shape::Text(text) => text.galley.text() == detail,
                _ => false,
            }),
            "opening Details renders the advanced explanation"
        );
        for label in ["Sort", "Cancel"] {
            assert!(
                visible(&output, label),
                "{label} remains visible in a small window"
            );
        }
        let _ = render(&mut dialog, filter_control_click(&output, "Sort as"));
        assert!(egui::Popup::is_any_open(&ctx));
        assert_eq!(
            render(
                &mut dialog,
                vec![individual_edit_key(
                    egui::Key::Escape,
                    egui::Modifiers::NONE
                )]
            )
            .0,
            None
        );
        assert!(!egui::Popup::is_any_open(&ctx));
        let _ = render(&mut dialog, vec![]);
        assert_eq!(
            render(
                &mut dialog,
                vec![individual_edit_key(
                    egui::Key::Escape,
                    egui::Modifiers::NONE
                )]
            )
            .0,
            Some(StructuralDialogAction::Cancel)
        );
        assert_eq!(dialog.sort_mode, SortMode::Text);

        let mut dialog = StructuralDialog::duplicates(vec![0, 1]);
        let _ = render(&mut dialog, vec![]);
        let (_, output) = render(&mut dialog, vec![]);
        let _ = render(&mut dialog, filter_control_click(&output, "Details"));
        let (_, output) = render(&mut dialog, vec![]);
        for label in ["Find duplicates", "Cancel"] {
            assert!(
                visible(&output, label),
                "{label} remains visible in a small window"
            );
        }
        assert_eq!(
            render(
                &mut dialog,
                vec![individual_edit_key(
                    egui::Key::Escape,
                    egui::Modifiers::NONE
                )]
            )
            .0,
            Some(StructuralDialogAction::Cancel)
        );
        assert!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .structural_job
                .is_none()
        );
        assert!(!app.active.document.as_ref().unwrap().is_dirty());
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_find_duplicates(vec![0], CaseSensitivity::Insensitive)
            .unwrap();
        finish_duplicate_search(&mut app);
        let Some(super::StructuralJob::DuplicatePreview { summary, .. }) =
            &app.active.document.as_ref().unwrap().structural_job
        else {
            panic!("expected duplicate preview");
        };
        let mut render_preview = |events| {
            time += 0.1;
            let mut action = None;
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    time: Some(time),
                    events,
                    ..Default::default()
                },
                |ctx| action = super::show_duplicate_preview(ctx, summary),
            );
            (action, output)
        };
        let _ = render_preview(vec![]);
        let (_, output) = render_preview(vec![]);
        let _ = render_preview(filter_control_click(&output, "Details"));
        let (_, output) = render_preview(vec![]);
        for label in [
            "Extra duplicate rows: 1",
            "Rows to keep: 1",
            "The document has not changed. Retained rows stay intact and in the same relative order.",
            "Remove extra rows",
            "Cancel",
        ] {
            assert!(
                visible(&output, label),
                "{label} remains visible in the expanded preview at minimum window size"
            );
        }
        assert_eq!(
            render_preview(vec![individual_edit_key(
                egui::Key::Escape,
                egui::Modifiers::NONE
            )])
            .0,
            Some(StructuralDialogAction::Cancel)
        );
        app.active
            .document
            .as_mut()
            .unwrap()
            .cancel_structural_edit();
        assert!(!app.active.document.as_ref().unwrap().is_dirty());
        assert_eq!(fs::read(source).unwrap(), b"name,value\nAvery,1\nAvery,2\n");
    }

    fn click_sort_dialog_control(
        ctx: &egui::Context,
        dialog: &mut StructuralDialog,
        app: &mut QuarryApp,
        label: &str,
    ) -> (Option<StructuralDialogAction>, egui::FullOutput) {
        if matches!(
            label,
            "Text"
                | "Number"
                | "Date / time"
                | "Character count"
                | "Word count"
                | "Shuffle"
                | "Reverse"
        ) && !egui::Popup::is_any_open(ctx)
        {
            assert_eq!(
                click_sort_dialog_control(ctx, dialog, app, "Sort as").0,
                None
            );
        }
        let output = ctx.run(grid_input(), |ctx| {
            show_structural_dialog(
                ctx,
                dialog,
                &mut app.active.state.sort_match_case,
                app.active.state.document.as_ref().unwrap(),
            );
        });
        let tree = output.platform_output.accesskit_update.unwrap();
        let target = tree
            .nodes
            .iter()
            .find(|(_, node)| {
                node.label() == Some(label) && node.supports_action(egui::accesskit::Action::Click)
            })
            .map(|(id, _)| *id)
            .unwrap_or_else(|| panic!("missing accessible control {label}"));
        let mut action = None;
        let output = ctx.run(
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
                action = show_structural_dialog(
                    ctx,
                    dialog,
                    &mut app.active.state.sort_match_case,
                    app.active.state.document.as_ref().unwrap(),
                );
            },
        );
        (action, output)
    }

    #[test]
    fn number_sort_choice_uses_numeric_order_and_preserves_history_and_save_as() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("number-sort.csv");
        let destination = directory.path().join("number-sort-output.csv");
        let original = b"amount,name\n10,ten\n2,two\n,blank\n+2.0,two-again\n1e2,hundred\n9007199254740993,large-2\n9007199254740992,large-1\n";
        let sorted = b"amount,name\n,blank\n2,two\n+2.0,two-again\n10,ten\n1e2,hundred\n9007199254740992,large-1\n9007199254740993,large-2\n";
        fs::write(&source, original).unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.active.document.as_mut().unwrap());
        app.active.sort_match_case = true;
        let mut dialog = StructuralDialog::sort(0);
        assert_eq!(dialog.sort_mode, SortMode::Text);
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut action = None;
        for label in ["Number", "Text", "Number", "Sort"] {
            let (next_action, output) =
                click_sort_dialog_control(&ctx, &mut dialog, &mut app, label);
            action = next_action;
            let tree = output.platform_output.accesskit_update.unwrap();
            assert_eq!(
                tree.nodes
                    .iter()
                    .any(|(_, node)| node.label() == Some("Match case")),
                dialog.sort_mode == SortMode::Text,
            );
            if dialog.sort_mode == SortMode::Number {
                let description = tree
                    .nodes
                    .iter()
                    .find(|(_, node)| node.label() == Some("Sort"))
                    .and_then(|(_, node)| node.description())
                    .unwrap();
                assert!(description.contains("Other values stop the sort."));
            }
            assert!(
                app.active.sort_match_case,
                "switching mode should retain the case preference"
            );
        }
        assert_eq!(dialog.sort_mode, SortMode::Number);
        assert_eq!(action, Some(StructuralDialogAction::Apply));
        app.open_structural_dialog(dialog);
        app.apply_structural_dialog_action(action.unwrap());
        finish_structural_edit(&mut app);
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(fs::read(document.session.path()).unwrap(), sorted);
        assert_eq!(document.selected_columns, BTreeSet::from([0]));
        assert!(document.is_dirty());
        assert_eq!(fs::read(&source).unwrap(), original);

        app.swap_structural_history(false).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        assert_eq!(
            fs::read(app.active.document.as_ref().unwrap().session.path()).unwrap(),
            original
        );
        app.swap_structural_history(true).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        assert_eq!(
            fs::read(app.active.document.as_ref().unwrap().session.path()).unwrap(),
            sorted
        );

        assert!(app.save_as_picker_result(Some(destination.clone())));
        let mut frame = eframe::Frame::_new_kittest();
        finish_app_save(&mut app, &ctx, &mut frame);
        assert_eq!(fs::read(destination).unwrap(), sorted);
        assert_eq!(fs::read(source).unwrap(), original);
        assert!(!app.active.document.as_ref().unwrap().is_dirty());
    }

    #[test]
    fn date_sort_format_is_accessible_and_preserves_history_and_save_as() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("date-sort.csv");
        let destination = directory.path().join("date-sort-output.csv");
        let original = b"date,name\n03/04/2026,march\n04/03/2026,april\n,blank\n03/04/2026,march-again\n02/29/2024,leap\n";
        let sorted = b"date,name\n04/03/2026,april\n03/04/2026,march\n03/04/2026,march-again\n02/29/2024,leap\n,blank\n";
        fs::write(&source, original).unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.active.document.as_mut().unwrap());
        app.active.sort_match_case = true;
        let mut dialog = StructuralDialog::sort(0);
        assert_eq!(dialog.sort_mode, SortMode::Text);
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let (_, output) = click_sort_dialog_control(&ctx, &mut dialog, &mut app, "Date / time");
        let tree = output.platform_output.accesskit_update.unwrap();
        for label in ["Date / time format", "Oldest first", "Newest first"] {
            assert!(
                tree.nodes
                    .iter()
                    .any(|(_, node)| node.label() == Some(label)),
                "{label}"
            );
        }
        assert!(
            !tree
                .nodes
                .iter()
                .any(|(_, node)| node.label() == Some("Match case"))
        );
        click_sort_dialog_control(&ctx, &mut dialog, &mut app, "Date / time format");
        click_sort_dialog_control(&ctx, &mut dialog, &mut app, "MM/DD/YYYY");
        assert_eq!(
            dialog.sort_mode,
            SortMode::DateTime {
                format: super::DateTimeFormat::MonthDayYear
            }
        );
        // Re-selecting the same mode must retain the chosen date format.
        click_sort_dialog_control(&ctx, &mut dialog, &mut app, "Date / time");
        assert_eq!(
            dialog.sort_mode,
            SortMode::DateTime {
                format: super::DateTimeFormat::MonthDayYear
            }
        );
        click_sort_dialog_control(&ctx, &mut dialog, &mut app, "Newest first");
        let (action, _) = click_sort_dialog_control(&ctx, &mut dialog, &mut app, "Sort");
        assert_eq!(action, Some(StructuralDialogAction::Apply));
        assert!(app.active.sort_match_case);
        app.open_structural_dialog(dialog);
        app.apply_structural_dialog_action(action.unwrap());
        finish_structural_edit(&mut app);
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(fs::read(document.session.path()).unwrap(), sorted);
        assert_eq!(document.selected_columns, BTreeSet::from([0]));
        assert!(document.is_dirty());
        assert_eq!(fs::read(&source).unwrap(), original);

        app.swap_structural_history(false).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        assert_eq!(
            fs::read(app.active.document.as_ref().unwrap().session.path()).unwrap(),
            original
        );
        app.swap_structural_history(true).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        assert_eq!(
            fs::read(app.active.document.as_ref().unwrap().session.path()).unwrap(),
            sorted
        );

        assert!(app.save_as_picker_result(Some(destination.clone())));
        let mut frame = eframe::Frame::_new_kittest();
        finish_app_save(&mut app, &ctx, &mut frame);
        assert_eq!(fs::read(destination).unwrap(), sorted);
        assert_eq!(fs::read(source).unwrap(), original);
        assert!(!app.active.document.as_ref().unwrap().is_dirty());
    }

    #[test]
    fn count_and_row_order_sort_choices_preserve_rows_and_history() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("row-order.csv");
        let original = "value,name\né,accent\ne\u{301},combining\na b,words\n,blank\nx\u{2003}y,unicode-space\nlong,long\n";
        fs::write(&source, original).unwrap();
        for (choice, direction, expected) in [
            (
                "Character count",
                Some("Shortest first"),
                Some([
                    "blank",
                    "accent",
                    "combining",
                    "words",
                    "unicode-space",
                    "long",
                ]),
            ),
            (
                "Character count",
                Some("Longest first"),
                Some([
                    "long",
                    "words",
                    "unicode-space",
                    "combining",
                    "accent",
                    "blank",
                ]),
            ),
            (
                "Word count",
                Some("Fewest first"),
                Some([
                    "blank",
                    "accent",
                    "combining",
                    "long",
                    "words",
                    "unicode-space",
                ]),
            ),
            (
                "Word count",
                Some("Most first"),
                Some([
                    "words",
                    "unicode-space",
                    "accent",
                    "combining",
                    "long",
                    "blank",
                ]),
            ),
            (
                "Reverse",
                None,
                Some([
                    "long",
                    "unicode-space",
                    "blank",
                    "words",
                    "combining",
                    "accent",
                ]),
            ),
            ("Shuffle", None, None),
        ] {
            let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
            finish_index(app.active.document.as_mut().unwrap());
            app.active.sort_match_case = true;
            let mut dialog = StructuralDialog::sort(0);
            dialog.sort_direction = super::SortDirection::Descending;
            let ctx = egui::Context::default();
            ctx.enable_accesskit();
            let (_, output) = click_sort_dialog_control(&ctx, &mut dialog, &mut app, choice);
            let tree = output.platform_output.accesskit_update.unwrap();
            assert!(
                !tree
                    .nodes
                    .iter()
                    .any(|(_, node)| node.label() == Some("Match case"))
            );
            assert_eq!(
                tree.nodes.iter().any(|(_, node)| matches!(
                    node.label(),
                    Some("Shortest first" | "Longest first" | "Fewest first" | "Most first")
                )),
                direction.is_some()
            );
            assert!(
                !tree
                    .nodes
                    .iter()
                    .any(|(_, node)| matches!(node.label(), Some("Ascending" | "Descending")))
            );
            if let Some(direction) = direction {
                click_sort_dialog_control(&ctx, &mut dialog, &mut app, direction);
            } else {
                let description = tree
                    .nodes
                    .iter()
                    .find(|(_, node)| node.label() == Some("Sort"))
                    .and_then(|(_, node)| node.description())
                    .unwrap();
                assert!(description.contains("selected column is ignored"));
                assert!(!description.contains("stable sort"));
            }
            let (action, _) = click_sort_dialog_control(&ctx, &mut dialog, &mut app, "Sort");
            assert_eq!(action, Some(StructuralDialogAction::Apply));
            assert!(app.active.sort_match_case);
            app.open_structural_dialog(dialog);
            app.apply_structural_dialog_action(action.unwrap());
            if direction.is_none() {
                assert!(matches!(
                    app.active
                        .document
                        .as_ref()
                        .unwrap()
                        .structural_job
                        .as_ref(),
                    Some(super::StructuralJob::Sorting { plan, .. })
                        if plan.keys[0].direction == super::SortDirection::Ascending
                ));
            }
            finish_structural_edit(&mut app);
            let document = app.active.document.as_ref().unwrap();
            let sorted = fs::read(document.session.path()).unwrap();
            assert_eq!(
                document.session.first_rows[0].fields,
                [b"value".to_vec(), b"name".to_vec()]
            );
            let names = document
                .session
                .first_rows
                .iter()
                .skip(1)
                .map(|row| std::str::from_utf8(&row.fields[1]).unwrap())
                .collect::<Vec<_>>();
            if let Some(expected) = expected {
                assert_eq!(names, expected, "{choice} {direction:?}");
            } else {
                let mut actual_rows = document
                    .session
                    .first_rows
                    .iter()
                    .skip(1)
                    .map(|row| row.fields.clone())
                    .collect::<Vec<_>>();
                let mut original_rows = Session::open(&source, OpenOptions::default())
                    .unwrap()
                    .first_rows
                    .into_iter()
                    .skip(1)
                    .map(|row| row.fields)
                    .collect::<Vec<_>>();
                actual_rows.sort();
                original_rows.sort();
                assert_eq!(actual_rows, original_rows);
            }
            if direction.is_none() {
                let status = app.active.footer_status.as_ref().unwrap().to_string();
                assert!(status.starts_with(if choice == "Reverse" {
                    "Reversed row order."
                } else {
                    "Shuffled rows."
                }));
                assert!(!status.contains("column"));
            }
            assert!(document.is_dirty());
            assert_eq!(fs::read(&source).unwrap(), original.as_bytes());
            app.swap_structural_history(false).unwrap();
            finish_index(app.active.document.as_mut().unwrap());
            assert_eq!(
                fs::read(app.active.document.as_ref().unwrap().session.path()).unwrap(),
                original.as_bytes()
            );
            app.swap_structural_history(true).unwrap();
            finish_index(app.active.document.as_mut().unwrap());
            assert_eq!(
                fs::read(app.active.document.as_ref().unwrap().session.path()).unwrap(),
                sorted
            );
        }
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
        finish_index(app.active.document.as_mut().unwrap());

        let document = app.active.document.as_mut().unwrap();
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
        let dialog = app.active.state.structural_dialog.as_mut().unwrap();
        assert_eq!(dialog.columns, vec![0]);
        dialog.separator = "@".into();
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let output = ctx.run(grid_input(), |ctx| {
            assert_eq!(
                show_structural_dialog(
                    ctx,
                    dialog,
                    &mut app.active.state.sort_match_case,
                    app.active.state.document.as_ref().unwrap(),
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
            .active
            .document
            .as_ref()
            .unwrap()
            .working_copy
            .as_ref()
            .unwrap()
            .directory
            .path()
            .to_path_buf();
        let document = app.active.document.as_ref().unwrap();
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
        let output = render_grid_with_view(
            &ctx,
            app.active.state.document.as_mut().unwrap(),
            &mut app.active.view.grid,
        );
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
        let document = app.active.document.as_mut().unwrap();
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
        assert_eq!(
            request,
            Some(GridColumnRequest::DeleteRows(
                std::iter::once(1..=2).collect()
            ))
        );
    }

    #[test]
    fn data_cell_context_menu_copies_and_filters_the_full_clicked_unsaved_value() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("cell-context-menu.csv");
        fs::write(&source, b"name,note\nalpha,source\nbeta,other\n").unwrap();
        let mut document = Document::open(
            &source,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        commit_test_cell(&mut document, 1, 1, "line one\nline two");

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
        let click_item =
            |target, document: &mut Document, filter_case_sensitivity: CaseSensitivity| {
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
                            let mut grid_view = EguiGridState::default();
                            show_grid_with_filter_case(
                                ui,
                                document,
                                &mut grid_view,
                                filter_case_sensitivity,
                            )
                            .unwrap();
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
                    upper_bound: None,
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
                    upper_bound: None,
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
        finish_index(app.active.document.as_mut().unwrap());
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_split(39, b":".to_vec())
            .unwrap();
        finish_structural_edit(&mut app);

        let document = app.active.document.as_ref().unwrap();
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
    fn save_as_applies_view_order_and_edits_preserving_hidden_columns_and_source() {
        for header_mode in [HeaderMode::FirstRow, HeaderMode::NoHeader] {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("source.csv");
            let destination = directory.path().join("reordered.csv");
            let data = "1,\"Alpha, One\",10\r\n2,\"line one\nline two\",20\r\n3,,30\r\n";
            let original = if header_mode == HeaderMode::FirstRow {
                format!("ID,Name,Amount\r\n{data}")
            } else {
                data.to_owned()
            };
            fs::write(&source, &original).unwrap();
            let mut app = QuarryApp::new(None, Instant::now());
            app.active.header_mode = header_mode;
            app.open_path(source.clone()).unwrap();
            finish_index(app.active.document.as_mut().unwrap());
            let document = app.active.document.as_mut().unwrap();
            document.move_column(2, 1).unwrap();
            document.set_column_shown(1, false).unwrap();
            assert!(!document.is_dirty());
            assert!(!document.is_save_ready());
            assert!(document.is_save_as_ready());
            if header_mode == HeaderMode::FirstRow {
                document.rename_header(2, "Total".into()).unwrap();
            }
            document
                .begin_cell_edit(document.data_start, 1, b"Alpha, One".to_vec())
                .unwrap();
            document.cell_edit.as_mut().unwrap().draft = "Edited, \"name\"".into();
            assert!(app.save_as_picker_result(Some(destination.clone())));
            assert!(!app.active.document.as_ref().unwrap().is_save_as_ready());

            let ctx = egui::Context::default();
            let deadline = Instant::now() + Duration::from_secs(5);
            while app.active.document.as_ref().unwrap().logical_path != destination {
                let _ = ctx.run(grid_input(), |ctx| {
                    eframe::App::update(&mut app, ctx, &mut eframe::Frame::_new_kittest());
                });
                assert!(app.active.notice.is_none(), "{:?}", app.active.notice);
                assert!(
                    Instant::now() < deadline,
                    "Save As did not reopen its output"
                );
                std::thread::yield_now();
            }
            let document = app.active.document.as_ref().unwrap();
            assert!(!document.is_dirty());
            assert!(!document.has_reordered_columns());
            assert_eq!(document.total_columns, 3);
            assert_eq!(document.columns.visible, [0, 1, 2]);
            assert_eq!(fs::read_to_string(&source).unwrap(), original);
            let expected =
                "1,10,\"Edited, \"\"name\"\"\"\r\n2,20,\"line one\nline two\"\r\n3,30,\r\n";
            let expected = if header_mode == HeaderMode::FirstRow {
                format!("ID,Total,Name\r\n{expected}")
            } else {
                expected.to_owned()
            };
            assert_eq!(fs::read_to_string(&destination).unwrap(), expected);
        }
    }

    #[test]
    fn reordered_save_as_preserves_late_ragged_fields_and_failed_save_state() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.csv");
        let destination = directory.path().join("reordered.csv");
        let mut original = "ID,Name,Amount\n".to_owned();
        original.push_str(&"1,Alpha,10\n".repeat(100));
        original.push_str("2,Beta,20,undiscovered,tail\n3\n");
        fs::write(&source, &original).unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        // Reorder before the UI has received the width of the last rows.
        let document = app.active.document.as_mut().unwrap();
        assert_eq!(document.total_columns, 3);
        document.move_column(2, 1).unwrap();
        let order = document.columns.order.clone();
        assert!(document.start_save().is_err());
        assert!(!app.save_as_picker_result(None));
        fs::write(&destination, b"keep this file").unwrap();
        assert!(!app.save_as_picker_result(Some(destination.clone())));
        assert_eq!(fs::read(&destination).unwrap(), b"keep this file");
        let document = app.active.document.as_mut().unwrap();
        assert_eq!(document.columns.order, order);
        assert!(document.is_save_as_ready());
        document.source_changed = true;
        assert!(!document.is_save_as_ready());
        assert!(document.start_save_as(destination.clone()).is_err());
        document.source_changed = false;
        fs::remove_file(&destination).unwrap();
        document.start_save_as(destination.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while document.poll_save().unwrap().is_none() {
            assert!(Instant::now() < deadline, "Save As timed out");
            std::thread::yield_now();
        }
        let mut expected = "ID,Amount,Name\n".to_owned();
        expected.push_str(&"1,10,Alpha\n".repeat(100));
        expected.push_str("2,20,Beta,undiscovered,tail\n3,,\n");
        assert_eq!(fs::read_to_string(&destination).unwrap(), expected);
        assert_eq!(fs::read_to_string(&source).unwrap(), original);
    }

    #[test]
    fn structural_move_and_view_order_can_be_saved_without_cell_edits() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.csv");
        let destination = directory.path().join("reordered.csv");
        let original = b"ID,Name,Amount\n1,Alpha,10\n";
        fs::write(&source, original).unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.active.document.as_mut().unwrap());
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_move_columns(vec![2], 1)
            .unwrap();
        finish_structural_edit(&mut app);
        let document = app.active.document.as_mut().unwrap();
        assert!(document.is_dirty());
        assert!(document.is_save_as_ready());
        document.move_column(0, 2).unwrap();
        document.start_save_as(destination.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while document.poll_save().unwrap().is_none() {
            assert!(Instant::now() < deadline, "Save As timed out");
            std::thread::yield_now();
        }
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"Amount,Name,ID\n10,Alpha,1\n"
        );
        assert_eq!(fs::read(&source).unwrap(), original);
    }

    #[test]
    fn split_then_editing_a_derived_cell_saves_as_without_touching_the_original() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("split-edit-source.csv");
        let destination = directory.path().join("split-edit-output.csv");
        fs::write(&source, b"email,city\nalice@example.com,New York\n").unwrap();
        let original = fs::read(&source).unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.active.document.as_mut().unwrap());
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_split(0, b"@".to_vec())
            .unwrap();
        finish_structural_edit(&mut app);

        let document = app.active.document.as_mut().unwrap();
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
        finish_index(app.active.document.as_mut().unwrap());
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_split(0, b"@".to_vec())
            .unwrap();
        finish_structural_edit(&mut app);

        let document = app.active.document.as_mut().unwrap();
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
        finish_index(app.active.document.as_mut().unwrap());
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_combine(vec![0, 1], b" ".to_vec())
            .unwrap();
        finish_structural_edit(&mut app);

        let document = app.active.document.as_ref().unwrap();
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
        finish_index(app.active.document.as_mut().unwrap());
        assert_eq!(app.active.footer_status.as_deref(), Some("Change undone."));
        assert_eq!(
            app.active
                .footer_status
                .as_ref()
                .map(|message| message.severity),
            Some(MessageSeverity::Status)
        );
        let document = app.active.document.as_mut().unwrap();
        assert_eq!(document.session.path(), source);
        assert_eq!(document.total_columns, 3);
        assert_eq!(
            document.session.first_rows[1].fields,
            vec![b"Ada".to_vec(), b"Lovelace".to_vec(), b"36".to_vec()]
        );
        assert!(document.can_redo_structural());
        assert!(!document.is_dirty());

        app.swap_structural_history(true).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        let document = app.active.document.as_ref().unwrap();
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
        finish_index(app.active.document.as_mut().unwrap());
        assert_eq!(app.active.document.as_ref().unwrap().total_columns, 2);
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_split(1, b":".to_vec())
            .unwrap();
        finish_structural_edit(&mut app);

        let document = app.active.document.as_ref().unwrap();
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
        finish_index(app.active.document.as_mut().unwrap());
        let document = app.active.document.as_mut().unwrap();
        document.rename_header(1, "town".into()).unwrap();
        document.begin_cell_edit(1, 0, b"a@b".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "alpha@beta".into();
        document.commit_cell_edit();
        document.start_split(0, b"@".to_vec()).unwrap();
        finish_structural_edit(&mut app);
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(
            document.session.first_rows[0].fields,
            vec![b"email".to_vec(), Vec::new(), b"town".to_vec()]
        );
        assert_eq!(
            document.session.first_rows[1].fields,
            vec![b"alpha".to_vec(), b"beta".to_vec(), b"x".to_vec()]
        );

        app.swap_structural_history(false).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        let document = app.active.document.as_mut().unwrap();
        assert_eq!(document.session.path(), source);
        assert_eq!(document.column_name(1), "town");
        assert_eq!(
            document.cell_edits.get(&(1, 0)).unwrap().as_slice(),
            b"alpha@beta"
        );
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
        finish_index(app.active.document.as_mut().unwrap());
        app.swap_structural_history(false).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        let document = app.active.document.as_mut().unwrap();
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
        finish_index(app.active.document.as_mut().unwrap());
        app.active
            .document
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
        assert!(app.active.document.as_ref().unwrap().source_changed);
    }

    #[test]
    fn first_combine_freezes_a_missing_source_without_allocating_working_state() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("combine-source-guard.csv");
        fs::write(&source, b"first,last\nAda,Lovelace\n").unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.active.document.as_mut().unwrap());
        fs::remove_file(source).unwrap();

        let document = app.active.document.as_mut().unwrap();
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
        finish_index(app.active.document.as_mut().unwrap());
        fs::remove_file(source).unwrap();

        let document = app.active.document.as_mut().unwrap();
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
        app.active.document = Some(document);
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
        let selection = app.active.document.as_ref().unwrap().selection.unwrap();
        assert!(matches!(
            selection,
            GridSelection::Cell {
                row: 1,
                column: 1,
                ..
            }
        ));
        assert_eq!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .copy_selection_text()
                .unwrap(),
            "line one\nline two"
        );
        assert!(output.platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::CopyText(text) if text == "line one\nline two")
        }));

        app.active.find_bar_open = true;
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

        let mut document = app.active.document.take().unwrap();

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
        app.active.find_bar_open = true;
        app.active.replace_expanded = true;
        app.active.replace_input = "copy this".into();
        app.active.document = Some(document);
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

        app.active.document.as_mut().unwrap().shutdown();
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
        app.active.document = Some(document);
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

        let document = app.active.document.as_ref().unwrap();
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
        let smaller_rows = app.active.document.as_ref().unwrap().visible_rows;
        let _ = render(1252.0, &mut app, &mut frame);
        let larger_rows = app.active.document.as_ref().unwrap().visible_rows;
        assert!(
            smaller_rows < reference_rows,
            "smaller window kept {smaller_rows} rows from the {reference_rows}-row reference"
        );
        assert!(
            larger_rows > reference_rows,
            "larger window kept {larger_rows} rows from the {reference_rows}-row reference"
        );

        app.active.document.as_mut().unwrap().shutdown();
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
        app.active.header_mode = HeaderMode::FirstRow;
        app.open_path(path.clone()).unwrap();
        let ctx = egui::Context::default();
        let mut frame = eframe::Frame::_new_kittest();

        let _ = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        assert!(!app.active.find_bar_open);

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
        assert!(app.active.find_bar_open);
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
        assert!(app.active.find_bar_open);

        app.active
            .document
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
        assert!(app.active.find_bar_open);
        assert!(app.active.document.as_ref().unwrap().cell_edit.is_none());

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
        assert!(!app.active.find_bar_open);
        assert!(!ctx.memory(|memory| memory.has_focus(egui::Id::new(FIND_INPUT_ID))));

        app.active.find_bar_open = true;
        app.active.replace_expanded = true;
        app.open_path(path).unwrap();
        assert!(app.active.find_bar_open);
        assert!(app.active.replace_expanded);
        app.active.document.as_mut().unwrap().shutdown();
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
        app.active.document = Some(document);
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
        assert!(!app.active.find_bar_open);

        app.active.document.as_mut().unwrap().shutdown();
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
        app.active.find_input = "needle".into();
        app.active.replace_input = "x".into();
        app.active.document = Some(document);
        app.apply(&egui::Context::default(), Action::ReplaceAll);
        finish_structural_edit(&mut app);

        let document = app.active.document.as_ref().unwrap();
        assert_eq!(fs::read(&path).unwrap(), source);
        assert_ne!(document.session.path(), path);
        assert_eq!(
            fs::read(document.session.path()).unwrap(),
            b"name\noverlay x x\nsource x\n"
        );
        assert!(document.is_dirty());
        assert!(app.active.notice.is_none());
        assert_eq!(
            app.active.footer_status.as_deref(),
            Some("Replaced 3 occurrences. Save to keep it, or discard changes.")
        );

        app.swap_structural_history(false).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(document.session.path(), path);
        assert_eq!(
            document
                .cell_edits
                .get(&(1, 0))
                .map(|value| value.as_slice()),
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
        app.active.find_input = "missing".into();
        app.active.document = Some(document);
        let ctx = egui::Context::default();
        app.apply(&ctx, Action::FindNext);
        app.apply(&ctx, Action::CancelSearch);
        assert!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .search_progress()
                .unwrap()
                .cancelled
        );

        app.active.document.as_mut().unwrap().shutdown();
        assert!(app.active.document.as_ref().unwrap().search_job.is_none());
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
        assert_eq!(
            document.headers.first().map(|value| value.as_str()),
            Some("c1")
        );
        assert_eq!(
            document.headers.last().map(|value| value.as_str()),
            Some("c40")
        );
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
        assert_eq!(
            document.headers.first().map(|value| value.as_str()),
            Some("c1")
        );
        assert_eq!(
            document.headers.last().map(|value| value.as_str()),
            Some("c40")
        );

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
        assert_eq!(
            document.headers.first().map(|value| value.as_str()),
            Some("c1")
        );
        assert_eq!(
            document.headers.last().map(|value| value.as_str()),
            Some("c40")
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn scrollbar_thumb_is_drawn_at_its_computed_size() {
        // A thumb drawn taller than its track makes dragging run backwards.
        let ctx = egui::Context::default();
        super::configure_style(&ctx);
        let track = 300.0;
        let thumb_height = super::scrollbar_thumb_height(track, 150, 100);
        assert_eq!(thumb_height, 200.0);
        let mut slider = None;
        let output = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(super::SCROLLBAR_WIDTH, track),
                    egui::Layout::top_down(egui::Align::Center),
                    |ui| {
                        ui.spacing_mut().slider_width = track;
                        let mut position = 1.0;
                        slider = Some(
                            super::scrollbar_slider(ui, &mut position, thumb_height, true).rect,
                        );
                    },
                );
            });
        });
        let slider = slider.unwrap();
        assert_eq!(slider.width(), super::SCROLLBAR_WIDTH);
        assert_eq!(slider.height(), track);

        let thumb_fill = super::theme::ctx_palette(&ctx)
            .text_tertiary
            .gamma_multiply(0.45);
        // The slider also paints a zero-width rail in the same color; skip it.
        let thumb = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::epaint::Shape::Rect(rect)
                    if rect.fill == thumb_fill && rect.rect.width() > 1.0 =>
                {
                    Some(rect.rect)
                }
                _ => None,
            })
            .expect("scrollbar thumb should be painted");
        assert!(
            (thumb.height() - thumb_height).abs() < 0.5,
            "thumb is {} tall, expected {thumb_height}",
            thumb.height()
        );
        assert!(
            slider.expand(0.5).contains_rect(thumb),
            "{thumb:?} outside {slider:?}"
        );
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
        app.active.document = Some(current);
        app.open_path(second.clone()).unwrap();

        let document = app.active.document.as_ref().unwrap();
        assert_eq!(document.session.path(), second);
        assert_eq!(document.buffered_rows[0].fields[0], b"second");

        app.active.document.as_mut().unwrap().shutdown();
        drop(app);
        for path in [first, second] {
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn failed_and_cancelled_opens_preserve_the_document_and_drops_open_each_file() {
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
        app.active.delimiter_mode = DelimiterMode::Comma;
        app.active.header_mode = HeaderMode::FirstRow;
        app.open_path(first.clone()).unwrap();
        assert_eq!(app.active.delimiter_mode, DelimiterMode::Comma);
        assert_eq!(app.active.header_mode, HeaderMode::FirstRow);
        app.active
            .document
            .as_mut()
            .unwrap()
            .move_column(1, 0)
            .unwrap();
        app.active
            .document
            .as_mut()
            .unwrap()
            .set_column_shown(0, false)
            .unwrap();
        app.open_picker_result(None);
        assert_eq!(app.active.document.as_ref().unwrap().session.path(), first);
        assert_eq!(app.active.delimiter_mode, DelimiterMode::Comma);
        assert_eq!(app.active.header_mode, HeaderMode::FirstRow);

        app.open_path_and_report(malformed.clone());
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(document.session.path(), first);
        assert_eq!(document.columns.order, [1, 0]);
        assert!(document.columns.hidden[0]);
        assert!(
            app.active
                .notice
                .as_deref()
                .unwrap()
                .contains("unterminated")
        );
        assert_eq!(app.active.delimiter_mode, DelimiterMode::Comma);
        assert_eq!(app.active.header_mode, HeaderMode::FirstRow);

        app.open_picker_result(Some(second.clone()));
        assert_eq!(app.active.document.as_ref().unwrap().session.path(), second);
        assert_eq!(app.active.delimiter_mode, DelimiterMode::Auto);
        assert_eq!(app.active.header_mode, HeaderMode::Auto);
        app.reopen_document(DelimiterMode::Comma, HeaderMode::FirstRow);

        app.handle_dropped_paths(vec![Some(first.clone()), Some(second.clone())]);
        assert_eq!(app.active.document.as_ref().unwrap().session.path(), second);
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.tabs[0].document.as_ref().unwrap().columns.order, [1, 0]);
        assert!(app.tabs[0].document.as_ref().unwrap().columns.hidden[0]);
        app.handle_dropped_paths(vec![None]);
        assert_eq!(app.active.document.as_ref().unwrap().session.path(), second);
        assert!(app.active.notice.as_deref().unwrap().contains("local file"));

        app.active.document.as_mut().unwrap().shutdown();
        drop(app);
        for path in [first, second, malformed] {
            fs::remove_file(path).unwrap();
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_quit_preserves_unsaved_sort_and_waits_for_save() {
        use objc2_app_kit::NSApplicationTerminateReply;
        use quarry_core::SortDirection;

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.csv");
        let destination = directory.path().join("saved.csv");
        fs::write(&source, b"value\n2\n1\n").unwrap();
        let mut app = individual_edit_app(&source);
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_sort_rows(
                0,
                SortDirection::Ascending,
                SortMode::Number,
                CaseSensitivity::Insensitive,
            )
            .unwrap();
        finish_structural_edit(&mut app);
        let document = app.active.document.as_mut().unwrap();
        let working_path = document.session.path().to_path_buf();
        document.begin_cell_edit(1, 0, b"1".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "3".into();

        let ctx = egui::Context::default();
        let request_native_quit = |app: &mut QuarryApp| {
            assert_eq!(
                super::native_quit_reply(Some(&ctx)),
                NSApplicationTerminateReply::TerminateCancel
            );
            let output = ctx.run(grid_input(), |_| {});
            let commands = &output.viewport_output[&egui::ViewportId::ROOT].commands;
            assert!(
                commands
                    .iter()
                    .any(|command| matches!(command, egui::ViewportCommand::Close))
            );

            // egui-winit translates the queued command into this viewport event.
            let mut input = grid_input();
            input
                .viewports
                .get_mut(&egui::ViewportId::ROOT)
                .unwrap()
                .events
                .push(egui::ViewportEvent::Close);
            let output = ctx.run(input, |ctx| app.intercept_dirty_close(ctx));
            assert!(
                output.viewport_output[&egui::ViewportId::ROOT]
                    .commands
                    .iter()
                    .any(|command| matches!(command, egui::ViewportCommand::CancelClose))
            );
        };
        request_native_quit(&mut app);
        assert!(app.active.close_confirmation_open);
        assert!(working_path.exists());
        assert!(app.active.document.as_ref().unwrap().cell_edit.is_none());
        assert_eq!(
            app.active.document.as_ref().unwrap().cell_edits[&(1, 0)].as_slice(),
            b"3"
        );
        app.keep_editing();
        assert!(!app.active.close_confirmation_open);
        assert!(working_path.exists());

        app.active
            .document
            .as_mut()
            .unwrap()
            .start_save_as(destination.clone())
            .unwrap();
        request_native_quit(&mut app);
        assert!(app.active.close_after_save);
        assert!(!app.active.close_confirmation_open);
        let output = finish_app_save(&mut app, &ctx, &mut eframe::Frame::_new_kittest());
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::Close))
        );
        assert!(app.quit_authorized);
        assert!(app.document_tabs().all(|tab| tab.document.is_none()));
        assert!(!working_path.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"value\n3\n2\n");
        assert_eq!(fs::read(&source).unwrap(), b"value\n2\n1\n");
        assert_eq!(
            super::native_quit_reply(None),
            NSApplicationTerminateReply::TerminateNow
        );
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
        app.active.delimiter_mode = DelimiterMode::Tab;
        app.active.header_mode = HeaderMode::NoHeader;
        app.open_document_receiver = Some(receiver);
        sender.send(source.clone()).unwrap();
        app.poll_open_documents();

        assert_eq!(app.active.document.as_ref().unwrap().session.path(), source);
        assert_eq!(app.active.delimiter_mode, DelimiterMode::Auto);
        assert_eq!(app.active.header_mode, HeaderMode::Auto);
        app.active.document.as_mut().unwrap().shutdown();
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
        assert_eq!(app.active.delimiter_mode, DelimiterMode::Auto);
        assert_eq!(app.active.header_mode, HeaderMode::Auto);
        assert_eq!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .session
                .dialect
                .delimiter,
            b','
        );
        assert!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .session
                .dialect
                .has_header
        );

        app.active.format_draft = Some((DelimiterMode::Tab, HeaderMode::NoHeader, None));
        fs::write(&path, b"name\tvalue\nsecond\t2\n").unwrap();
        app.reload_document();
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(document.session.path(), path);
        assert_eq!(document.session.dialect.delimiter, b',');
        assert!(document.session.dialect.has_header);
        assert_eq!(document.headers, ["name\tvalue"]);
        assert_eq!(document.buffered_rows[0].fields[0], b"second\t2");
        assert_eq!(app.active.delimiter_mode, DelimiterMode::Auto);
        assert_eq!(app.active.header_mode, HeaderMode::Auto);
        assert_eq!(app.active.format_draft, None);

        app.reopen_document(DelimiterMode::Tab, HeaderMode::NoHeader);
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(document.session.path(), path);
        assert_eq!(document.session.dialect.delimiter, b'\t');
        assert!(!document.session.dialect.has_header);
        assert_eq!(document.headers, ["Column 1", "Column 2"]);
        assert_eq!(document.buffered_rows[0].fields[0], b"name");
        assert_eq!(document.buffered_rows[0].fields[1], b"value");
        assert_eq!(app.active.delimiter_mode, DelimiterMode::Tab);
        assert_eq!(app.active.header_mode, HeaderMode::NoHeader);
        assert_eq!(DelimiterMode::Pipe.delimiter(), Some(b'|'));
        assert_eq!(DelimiterMode::Semicolon.delimiter(), Some(b';'));

        fs::write(&path, b"\"unterminated").unwrap();
        app.reopen_document(DelimiterMode::Comma, HeaderMode::FirstRow);
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(document.headers, ["Column 1", "Column 2"]);
        assert_eq!(document.buffered_rows[0].fields[0], b"name");
        assert_eq!(app.active.delimiter_mode, DelimiterMode::Tab);
        assert_eq!(app.active.header_mode, HeaderMode::NoHeader);
        assert!(
            app.active
                .notice
                .as_deref()
                .unwrap()
                .contains("unterminated")
        );

        app.active.document.as_mut().unwrap().shutdown();
        drop(app);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn insertion_rows_and_columns_keep_pending_edits_history_and_exact_save_as() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.csv");
        let destination = directory.path().join("inserted.csv");
        let original = b"ID,Note\n1,one\n2,two\n";
        fs::write(&source, original).unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.active.document.as_mut().unwrap());
        let document = app.active.document.as_mut().unwrap();
        document.rename_header(0, "Number".into()).unwrap();
        document.begin_cell_edit(1, 1, b"one".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "edited, 猫\nline".into();
        let mut rows = StructuralDialog::insert_rows(1);
        rows.count = "2".into();
        app.active.structural_dialog = Some(rows);
        app.apply_structural_dialog_action(StructuralDialogAction::Apply);
        assert!(app.active.notice.is_none(), "{:?}", app.active.notice);
        finish_structural_edit(&mut app);
        let expected = "Number,Note\n1,\"edited, 猫\nline\"\n,\n,\n2,two\n";
        assert_eq!(
            fs::read_to_string(app.active.document.as_ref().unwrap().session.path()).unwrap(),
            expected
        );
        assert_eq!(fs::read(&source).unwrap(), original);
        app.swap_structural_history(false).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(
            document.header_renames.get(&0).map(|value| value.as_str()),
            Some("Number")
        );
        assert_eq!(
            document.cell_edits.get(&(1, 1)).unwrap().as_slice(),
            "edited, 猫\nline".as_bytes()
        );
        app.swap_structural_history(true).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        assert_eq!(
            fs::read_to_string(app.active.document.as_ref().unwrap().session.path()).unwrap(),
            expected
        );
        let mut columns = StructuralDialog::insert_columns(1);
        columns.count = "2".into();
        columns.separator = "New".into();
        app.active.structural_dialog = Some(columns);
        app.apply_structural_dialog_action(StructuralDialogAction::Apply);
        finish_structural_edit(&mut app);
        let document = app.active.document.as_mut().unwrap();
        assert_eq!(document.total_columns, 4);
        assert_eq!(document.selected_columns, BTreeSet::from([1, 2]));
        document.begin_cell_edit(2, 1, Vec::new()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "added".into();
        assert!(app.save_as_picker_result(Some(destination.clone())));
        let ctx = egui::Context::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.active.document.as_ref().unwrap().logical_path != destination {
            let _ = ctx.run(grid_input(), |ctx| {
                eframe::App::update(&mut app, ctx, &mut eframe::Frame::_new_kittest())
            });
            assert!(app.active.notice.is_none(), "{:?}", app.active.notice);
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(
            fs::read_to_string(&destination).unwrap(),
            "Number,New,,Note\n1,,,\"edited, 猫\nline\"\n,added,,\n,,,\n2,,,two\n"
        );
        assert_eq!(fs::read(&source).unwrap(), original);
        assert!(!app.active.document.as_ref().unwrap().is_dirty());
    }

    #[test]
    fn insertion_supports_empty_headerless_and_header_only_documents() {
        for (bytes, header_mode) in [
            (b"".as_slice(), HeaderMode::NoHeader),
            (b"A,B".as_slice(), HeaderMode::FirstRow),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("source.csv");
            fs::write(&source, bytes).unwrap();
            let mut app = QuarryApp::new(None, Instant::now());
            app.active.header_mode = header_mode;
            app.open_path(source.clone()).unwrap();
            finish_index(app.active.document.as_mut().unwrap());
            let document = app.active.document.as_mut().unwrap();
            if bytes.is_empty() {
                assert!(document.insertion_disabled_reason(false).is_some());
            }
            let mut rows = StructuralDialog::insert_rows(0);
            rows.count = "2".into();
            document.insert_rows_from_dialog(&rows).unwrap();
            finish_structural_edit(&mut app);
            let document = app.active.document.as_mut().unwrap();
            assert_eq!(
                document.index.as_ref().unwrap().indexed_rows(),
                document.data_start + 2
            );
            let mut columns = StructuralDialog::insert_columns(document.total_columns);
            columns.count = "2".into();
            document.insert_columns_from_dialog(&columns).unwrap();
            finish_structural_edit(&mut app);
            let document = app.active.document.as_mut().unwrap();
            let row = document.data_start;
            let column = document.total_columns - 1;
            document.begin_cell_edit(row, column, Vec::new()).unwrap();
            document.cell_edit.as_mut().unwrap().draft = "new value".into();
            document.commit_edits();
            let destination = directory.path().join("saved.csv");
            document.start_save_as(destination.clone()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while document.poll_save().unwrap().is_none() {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            assert_eq!(
                fs::read_to_string(destination).unwrap(),
                if bytes.is_empty() {
                    ",,new value\n,,\n"
                } else {
                    "A,B,,\n,,,new value\n,,,\n"
                }
            );
            assert_eq!(fs::read(&source).unwrap(), bytes);
        }
    }

    #[test]
    fn insertion_positions_counts_and_hidden_reordered_columns_are_explicit() {
        for invalid in ["", "0", "-1", "1.5", "18446744073709551616"] {
            assert!(super::parse_insertion_count(invalid).is_err());
            assert!(super::parse_insertion_position(invalid, 3).is_err());
        }
        assert_eq!(super::parse_insertion_position("4", 3).unwrap(), 3);
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.csv");
        let original = b"A,B,C\none,two,three\n";
        fs::write(&source, original).unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        app.active.header_mode = super::HeaderMode::FirstRow;
        app.open_path(source.clone()).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        let document = app.active.document.as_mut().unwrap();
        document.move_column(2, 0).unwrap();
        document.set_column_shown(1, false).unwrap();
        let mut columns = StructuralDialog::insert_columns(1);
        columns.separator = "New".into();
        document.insert_columns_from_dialog(&columns).unwrap();
        finish_structural_edit(&mut app);
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(document.columns.order, [0, 1, 2, 3]);
        assert_eq!(document.columns.visible, [0, 1, 2, 3]);
        assert_eq!(
            fs::read_to_string(document.session.path()).unwrap(),
            "A,New,B,C\none,,two,three\n"
        );
        assert_eq!(fs::read(source).unwrap(), original);
    }

    #[test]
    fn insertion_requires_clear_filter_and_completed_index_without_losing_work() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.csv");
        fs::write(&source, b"A,B\none,two\n").unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        let document = app.active.document.as_mut().unwrap();
        assert!(
            document
                .insert_rows_from_dialog(&StructuralDialog::insert_rows(0))
                .is_err()
        );
        finish_index(document);
        document
            .start_filter(FilterQuery::single(
                0,
                FilterOperator::Equals,
                b"one".to_vec(),
            ))
            .unwrap();
        finish_filter(document);
        assert!(
            document
                .insert_rows_from_dialog(&StructuralDialog::insert_rows(0))
                .is_err()
        );
        assert!(
            document
                .insert_columns_from_dialog(&StructuralDialog::insert_columns(0))
                .is_err()
        );
        assert!(!document.is_dirty());
        assert_eq!(fs::read(source).unwrap(), b"A,B\none,two\n");
    }

    #[test]
    fn insertion_dialog_controls_are_accessible_and_invalid_submission_is_disabled() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.csv");
        fs::write(&source, b"A,B\n1,x\n").unwrap();
        let mut app = QuarryApp::new(Some(source), Instant::now());
        finish_index(app.active.document.as_mut().unwrap());
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        for rows in [true, false] {
            let mut dialog = if rows {
                StructuralDialog::insert_rows(0)
            } else {
                StructuralDialog::insert_columns(0)
            };
            dialog.count = "0".into();
            let out = ctx.run(grid_input(), |ctx| {
                assert_eq!(
                    show_structural_dialog(
                        ctx,
                        &mut dialog,
                        &mut app.active.state.sort_match_case,
                        app.active.state.document.as_ref().unwrap()
                    ),
                    None
                );
            });
            let nodes = out.platform_output.accesskit_update.unwrap().nodes;
            let label = if rows {
                "Number of rows"
            } else {
                "Number of columns"
            };
            assert!(nodes.iter().any(|(_, node)| node.label() == Some(label)));
            let (_, apply) = nodes
                .iter()
                .find(|(_, node)| node.label() == Some("Insert"))
                .unwrap();
            assert!(apply.is_disabled());
            dialog.count = "1".into();
            let (action, _) = click_sort_dialog_control(&ctx, &mut dialog, &mut app, "Insert");
            assert_eq!(action, Some(StructuralDialogAction::Apply));
        }
    }
    #[test]
    fn insertion_context_menus_use_clicked_file_boundaries() {
        for (target_label, command, expected) in [
            (
                "Select file column 2 (city)",
                "Insert Columns Before…",
                StructuralDialog::insert_columns(1),
            ),
            (
                "Select file column 2 (city)",
                "Insert Columns After…",
                StructuralDialog::insert_columns(2),
            ),
            (
                "Select row 2",
                "Insert Rows Above…",
                StructuralDialog::insert_rows(1),
            ),
            (
                "Select row 2",
                "Insert Rows Below…",
                StructuralDialog::insert_rows(2),
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("menu.csv");
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
            let target = grid_control_id(&ctx, target_label, &mut document);
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
            let tree = menu.platform_output.accesskit_update.unwrap();
            let (target, node) = tree
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some(command))
                .unwrap();
            assert!(!node.is_disabled());
            let target = *target;
            let mut request = None;
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
                        request = show_grid(ui, &mut document).unwrap();
                    });
                },
            );
            assert_eq!(request, Some(GridColumnRequest::Dialog(expected)));
        }
    }
    #[test]
    fn cleanup_preview_and_apply_use_pending_values_and_keep_history() {
        use super::{StructuralDialog, StructuralDialogAction, cleanup_preview};
        use quarry_core::{CleanupOperation, ColumnCleanup};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cleanup.csv");
        let original = b"name,other\n  Alice  ,keep\n  Bob  ,untouched\n";
        fs::write(&path, original).unwrap();
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
            .begin_cell_edit(1, 0, b"  Alice  ".to_vec())
            .unwrap();
        document.cell_edit.as_mut().unwrap().draft = "  Carol  ".into();
        let mut app = QuarryApp::new(None, Instant::now());
        app.active.document = Some(document);
        app.open_structural_dialog(StructuralDialog::cleanup_columns(vec![0]));
        let doc = app.active.document.as_ref().unwrap();
        let preview = cleanup_preview(
            doc,
            &ColumnCleanup {
                columns: vec![0],
                operation: CleanupOperation::Trim,
            },
        );
        assert!(preview.error.is_none());
        assert!(
            preview
                .lines
                .iter()
                .any(|l| l.contains("Carol") && !l.contains("Alice"))
        );
        app.apply_structural_dialog_action(StructuralDialogAction::Apply);
        finish_structural_edit(&mut app);
        let doc = app.active.document.as_ref().unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(
            fs::read(doc.session.path()).unwrap(),
            b"name,other\nCarol,keep\nBob,untouched\n"
        );
        assert!(
            app.active
                .footer_status
                .as_ref()
                .unwrap()
                .contains("Cleaned 2 cells")
        );
        app.swap_structural_history(false).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        let doc = app.active.document.as_ref().unwrap();
        assert_eq!(
            doc.cell_edits.get(&(1, 0)).map(|value| value.as_slice()),
            Some(b"  Carol  ".as_slice())
        );
        app.swap_structural_history(true).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        assert_eq!(
            fs::read(app.active.document.as_ref().unwrap().session.path()).unwrap(),
            b"name,other\nCarol,keep\nBob,untouched\n"
        );
    }

    #[test]
    fn cleanup_modes_noop_and_preview_bounds_are_observable() {
        use super::{StructuralDialog, StructuralDialogAction, StructuralRequest, cleanup_preview};
        use quarry_core::{CleanupOperation, ColumnCleanup};
        for (request, fill, expected, count) in [
            (
                StructuralRequest::CleanupUppercase,
                "",
                "name,value\nSTRASSE,\nΟΣ,  \n",
                1,
            ),
            (
                StructuralRequest::CleanupLowercase,
                "",
                "name,value\nstraße,\nος,  \n",
                2,
            ),
            (
                StructuralRequest::CleanupFill,
                "filled",
                "name,value\nStraße,filled\nΟΣ,  \n",
                1,
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("cleanup-modes.csv");
            fs::write(&path, "name,value\nStraße,\nΟΣ,  \n").unwrap();
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
            app.active.document = Some(document);
            let mut dialog = StructuralDialog::cleanup_columns(vec![0, 1]);
            dialog.request = request;
            dialog.separator = fill.into();
            app.open_structural_dialog(dialog);
            app.apply_structural_dialog_action(StructuralDialogAction::Apply);
            finish_structural_edit(&mut app);
            assert_eq!(
                fs::read(app.active.document.as_ref().unwrap().session.path()).unwrap(),
                expected.as_bytes()
            );
            assert!(
                app.active
                    .footer_status
                    .as_ref()
                    .unwrap()
                    .contains(&format!("Cleaned {count} cell"))
            );
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("preview-large.csv");
        fs::write(&path, format!("value\n{}\n", "a".repeat(8192))).unwrap();
        let mut doc = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut doc);
        let preview = cleanup_preview(
            &doc,
            &ColumnCleanup {
                columns: vec![0],
                operation: CleanupOperation::Uppercase,
            },
        );
        assert!(preview.skipped);
        assert!(preview.lines.is_empty());
        assert!(preview.error.is_none());
        let mut app = QuarryApp::new(None, Instant::now());
        app.active.document = Some(doc);
        app.open_structural_dialog(StructuralDialog::cleanup_columns(vec![0]));
        app.apply_structural_dialog_action(StructuralDialogAction::Apply);
        finish_structural_edit(&mut app);
        let doc = app.active.document.as_ref().unwrap();
        assert!(!doc.is_dirty());
        assert_eq!(doc.session.path(), path);
    }

    #[test]
    fn cleanup_dialog_exposes_operations_fill_label_and_filter_guard() {
        use super::{StructuralRequest, show_cleanup_dialog};
        use quarry_core::{CleanupOperation, ColumnCleanup};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cleanup-controls.csv");
        fs::write(&path, b"name,value\nAlice,\n").unwrap();
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
        let mut dialog = StructuralDialog::cleanup_columns(vec![1]);
        dialog.request = StructuralRequest::CleanupFill;
        let output = ctx.run(grid_input(), |ctx| {
            assert_eq!(show_cleanup_dialog(ctx, &mut dialog, &document), None);
        });
        let nodes = output.platform_output.accesskit_update.unwrap().nodes;
        for label in [
            "Trim leading/trailing whitespace",
            "Collapse repeated spaces",
            "Uppercase",
            "Lowercase",
            "Fill Empty Cells",
            "Fill value",
            "Apply",
            "Cancel",
        ] {
            assert!(
                nodes.iter().any(|(_, node)| node.label() == Some(label)),
                "missing {label}"
            );
        }
        document
            .start_filter(FilterQuery::single(
                0,
                FilterOperator::Equals,
                b"Alice".to_vec(),
            ))
            .unwrap();
        finish_filter(&mut document);
        let output = ctx.run(grid_input(), |ctx| {
            assert_eq!(show_cleanup_dialog(ctx, &mut dialog, &document), None);
        });
        let nodes = output.platform_output.accesskit_update.unwrap().nodes;
        assert!(
            nodes
                .iter()
                .find(|(_, node)| node.label() == Some("Apply"))
                .unwrap()
                .1
                .is_disabled()
        );
        assert!(
            document
                .start_cleanup(ColumnCleanup {
                    columns: vec![1],
                    operation: CleanupOperation::FillEmpty(b"filled".to_vec())
                })
                .is_err()
        );
        assert!(document.structural_job.is_none());
        assert!(!document.is_dirty());
    }

    #[test]
    fn cleanup_dialog_collapse_spaces_updates_values_history_and_indicators() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("collapse.csv");
        let original = b"city,other\nLos  Angeles,keep  this\nNew York,\n";
        fs::write(&path, original).unwrap();
        let mut app = individual_edit_app(&path);
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        commit_test_cell(app.active.document.as_mut().unwrap(), 1, 0, "Los   Angeles");
        settle_whitespace(&ctx, app.active.document.as_mut().unwrap());
        app.open_structural_dialog(StructuralDialog::cleanup_columns(vec![0]));
        let render = |app: &mut QuarryApp, events| {
            ctx.run(
                egui::RawInput {
                    events,
                    ..grid_input()
                },
                |ctx| {
                    let _ = super::show_cleanup_dialog(
                        ctx,
                        app.active.state.structural_dialog.as_mut().unwrap(),
                        app.active.state.document.as_ref().unwrap(),
                    );
                },
            )
        };
        let output = render(&mut app, vec![]);
        let target = output
            .platform_output
            .accesskit_update
            .unwrap()
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Collapse repeated spaces"))
            .expect("cleanup must expose Collapse repeated spaces")
            .0;
        let _ = render(
            &mut app,
            vec![egui::Event::AccessKitActionRequest(
                egui::accesskit::ActionRequest {
                    action: egui::accesskit::Action::Click,
                    target,
                    data: None,
                },
            )],
        );
        let dialog = app.active.structural_dialog.as_ref().unwrap();
        assert_eq!(
            dialog.cleanup().operation.label(),
            "Collapse repeated spaces"
        );
        let preview =
            super::cleanup_preview(app.active.document.as_ref().unwrap(), &dialog.cleanup());
        assert!(
            preview
                .lines
                .iter()
                .any(|line| line.contains("before \"Los   Angeles\"; after \"Los Angeles\""))
        );
        app.apply_structural_dialog_action(StructuralDialogAction::Apply);
        finish_structural_edit(&mut app);
        let expected = b"city,other\nLos Angeles,keep  this\nNew York,\n";
        let document = app.active.state.document.as_mut().unwrap();
        assert_eq!(fs::read(document.session.path()).unwrap(), expected);
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(
            app.active
                .state
                .footer_status
                .as_ref()
                .unwrap()
                .contains("Cleaned 1 cell")
        );
        let output = settle_whitespace(&ctx, document);
        let description = output
            .platform_output
            .accesskit_update
            .unwrap()
            .nodes
            .into_iter()
            .find(|(_, node)| {
                node.label()
                    .is_some_and(|label| label.starts_with("Select row 1, column 1"))
            })
            .unwrap()
            .1
            .description()
            .unwrap()
            .to_owned();
        assert!(
            description.contains("Ordinary single spaces"),
            "{description}"
        );
        let summary = document
            .whitespace
            .get(
                super::whitespace::CellId {
                    row: Some(1),
                    column: 0,
                },
                11,
                || panic!("expected completed inspection"),
            )
            .unwrap();
        assert!(
            !summary.indicated(false),
            "the subtle whitespace triangle must clear"
        );
        assert!(
            summary.indicated(true),
            "Show All still marks the remaining single space"
        );
        app.swap_structural_history(false).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        assert_eq!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .cell_edits
                .get(&(1, 0))
                .unwrap()
                .as_slice(),
            b"Los   Angeles",
        );
        app.swap_structural_history(true).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        let saved = directory.path().join("saved.csv");
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_save_as(saved.clone())
            .unwrap();
        let mut frame = eframe::Frame::_new_kittest();
        finish_app_save(&mut app, &ctx, &mut frame);
        assert_eq!(fs::read(saved).unwrap(), expected);
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn cleanup_scans_past_preview_and_preserves_work_after_failure_or_noop() {
        use quarry_core::{CleanupOperation, ColumnCleanup};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("late-cleanup.csv");
        let mut original = b"name,raw\n".to_vec();
        for _ in 0..30 {
            original.extend_from_slice(b"clean,ok\n");
        }
        original.extend_from_slice(b"  late  ,\xff\n");
        fs::write(&path, &original).unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        let trim = ColumnCleanup {
            columns: vec![0],
            operation: CleanupOperation::Trim,
        };
        let sample = super::cleanup_preview(&document, &trim);
        assert!(sample.error.is_none());
        assert!(sample.lines.iter().all(|line| !line.contains("late")));
        let mut app = QuarryApp::new(None, Instant::now());
        app.active.document = Some(document);
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_cleanup(trim.clone())
            .unwrap();
        finish_structural_edit(&mut app);
        assert!(
            app.active
                .footer_status
                .as_deref()
                .unwrap()
                .contains("Cleaned 1 cell")
        );
        let doc = app.active.document.as_mut().unwrap();
        assert!(
            fs::read(doc.session.path())
                .unwrap()
                .ends_with(b"late,\xff\n")
        );
        doc.begin_cell_edit(1, 0, b"clean".to_vec()).unwrap();
        doc.cell_edit.as_mut().unwrap().draft = "Carol".into();
        doc.commit_edits();
        let previous_path = doc.session.path().to_path_buf();
        let edits = doc.cell_edits.clone();
        let history = doc.edit_history.clone();
        let undo = doc.working_copy.as_ref().unwrap().undo.clone();
        doc.start_cleanup(ColumnCleanup {
            columns: vec![1],
            operation: CleanupOperation::Lowercase,
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let error = loop {
            match doc.poll_structural_edit() {
                Err(error) => break error,
                Ok(None) => {}
                Ok(Some(_)) => panic!("invalid text must not produce a working version"),
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        };
        assert!(
            error.text.contains("31") && error.text.contains("2"),
            "{}",
            error.text
        );
        assert_eq!(doc.session.path(), previous_path);
        assert_eq!(doc.cell_edits, edits);
        assert_eq!(doc.edit_history, history);
        assert_eq!(doc.working_copy.as_ref().unwrap().undo, undo);
        doc.start_cleanup(trim).unwrap();
        finish_structural_edit(&mut app);
        let doc = app.active.document.as_ref().unwrap();
        assert_eq!(doc.session.path(), previous_path);
        assert_eq!(doc.cell_edits, edits);
        assert_eq!(doc.edit_history, history);
        assert_eq!(doc.working_copy.as_ref().unwrap().undo, undo);
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(
            fs::read_dir(doc.working_copy.as_ref().unwrap().directory.path())
                .unwrap()
                .count(),
            1
        );
    }

    fn cleanup_source_change_warning(at_handoff: bool) {
        use quarry_core::{CleanupOperation, ColumnCleanup};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cleanup-source-change.csv");
        let original = b"name\n  Alice  \n";
        fs::write(&path, original).unwrap();
        let mut app = individual_edit_app(&path);
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_cleanup(ColumnCleanup {
                columns: vec![0],
                operation: CleanupOperation::Trim,
            })
            .unwrap();
        finish_structural_edit(&mut app);
        let doc = app.active.document.as_mut().unwrap();
        let working_path = doc.session.path().to_path_buf();
        let cleanup = ColumnCleanup {
            columns: vec![0],
            operation: CleanupOperation::Uppercase,
        };
        let message = if at_handoff {
            doc.start_cleanup(cleanup).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let super::StructuralJob::Cleaning { job, .. } =
                    doc.structural_job.as_ref().unwrap()
                else {
                    panic!("expected cleanup");
                };
                if job.progress().done {
                    break;
                }
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            fs::write(&working_path, b"name\nchanged outside\n").unwrap();
            loop {
                match doc.poll_structural_edit() {
                    Err(error) => break error,
                    Ok(None) => {}
                    Ok(Some(_)) => panic!("changed source must not be accepted"),
                }
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
        } else {
            fs::write(&working_path, b"name\nchanged outside\n").unwrap();
            doc.start_cleanup(cleanup).unwrap_err()
        };
        assert_eq!(
            message,
            super::AppMessage::warning(super::SOURCE_CHANGED_NOTICE)
        );
        assert!(doc.source_changed);
        assert!(doc.structural_job.is_none());
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn cleanup_start_source_change_uses_shared_recovery_warning() {
        cleanup_source_change_warning(false);
    }

    #[test]
    fn cleanup_handoff_source_change_uses_shared_recovery_warning() {
        cleanup_source_change_warning(true);
    }

    fn settle_whitespace(ctx: &egui::Context, document: &mut Document) -> egui::FullOutput {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let output = render_grid(ctx, document);
            if !document.whitespace.pending() {
                return output;
            }
            assert!(Instant::now() < deadline, "whitespace inspection timed out");
            std::thread::yield_now();
        }
    }

    #[test]
    fn whitespace_toggle_is_accessible_keyboard_operable_and_shared_without_editing() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("spaces.csv");
        fs::write(&source, b"name\nNew York\n").unwrap();
        let mut app = individual_edit_app(&source);
        let ctx = egui::Context::default();
        configure_style(&ctx);
        ctx.enable_accesskit();
        let mut frame = eframe::Frame::_new_kittest();
        let mut render = |app: &mut QuarryApp, events: Vec<egui::Event>| {
            ctx.run(
                egui::RawInput {
                    events,
                    ..grid_input()
                },
                |ctx| {
                    eframe::App::update(app, ctx, &mut frame);
                },
            )
        };
        let click = |target| {
            egui::Event::AccessKitActionRequest(egui::accesskit::ActionRequest {
                action: egui::accesskit::Action::Click,
                target,
                data: None,
            })
        };
        let output = render(&mut app, vec![]);
        let menu = accessible_button(&output, "View").0;
        let _ = render(&mut app, vec![click(menu)]);
        let output = render(&mut app, vec![]);
        let (target, node) = output
            .platform_output
            .accesskit_update
            .as_ref()
            .unwrap()
            .nodes
            .iter()
            .find(|(_, n)| n.label() == Some("Show All Whitespace"))
            .unwrap();
        assert_eq!(node.role(), egui::accesskit::Role::CheckBox);
        assert_eq!(node.toggled(), Some(egui::accesskit::Toggled::False));
        let _ = render(&mut app, vec![click(*target)]);
        assert!(app.whitespace_preferences.show_all);
        assert!(!app.active.document.as_ref().unwrap().is_dirty());
        assert!(!app.active.document.as_ref().unwrap().can_undo());
        let output = render(&mut app, vec![]);
        assert!(
            output
                .shapes
                .iter()
                .any(|s| matches!(&s.shape,egui::Shape::Text(t) if t.galley.text()=="New·York"))
        );
        let second = directory.path().join("second.csv");
        fs::write(&second, b"name\nSan Diego\n").unwrap();
        app.open_new_path(second).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        let output = render(&mut app, vec![]);
        assert!(
            output
                .shapes
                .iter()
                .any(|s| matches!(&s.shape,egui::Shape::Text(t) if t.galley.text()=="San·Diego"))
        );
        // Open by accessibility, then operate the focused checkbox with Space.
        let menu = accessible_button(&output, "View").0;
        let _ = render(&mut app, vec![click(menu)]);
        let output = render(&mut app, vec![]);
        let (target, _) = output
            .platform_output
            .accesskit_update
            .as_ref()
            .unwrap()
            .nodes
            .iter()
            .find(|(_, n)| n.label() == Some("Show All Whitespace"))
            .unwrap();
        let _ = render(
            &mut app,
            vec![egui::Event::AccessKitActionRequest(
                egui::accesskit::ActionRequest {
                    action: egui::accesskit::Action::Focus,
                    target: *target,
                    data: None,
                },
            )],
        );
        let _ = render(
            &mut app,
            vec![egui::Event::Key {
                key: egui::Key::Space,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(!app.whitespace_preferences.show_all);
        // A view action must not commit an existing editor draft.
        let document = app.active.document.as_mut().unwrap();
        document
            .begin_cell_edit(1, 0, b"San Diego".to_vec())
            .unwrap();
        document.cell_edit.as_mut().unwrap().draft = " draft ".into();
        app.apply(&ctx, Action::ShowAllWhitespace(true));
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(document.cell_edit.as_ref().unwrap().draft, " draft ");
        assert!(document.cell_edits.is_empty());
        assert!(!document.can_undo());
    }

    #[test]
    fn whitespace_indicator_reports_hidden_tail_and_raw_accessible_value() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("hidden.csv");
        let value = "x".repeat(super::MAX_GRID_TEXT_CHARS + 100) + " \t";
        fs::write(&source, format!("name\n{value}\n")).unwrap();
        let mut document = Document::open(
            &source,
            OpenOptions {
                delimiter: Some(b','),
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let output = settle_whitespace(&ctx, &mut document);
        let description = output
            .platform_output
            .accesskit_update
            .unwrap()
            .nodes
            .into_iter()
            .find(|(_, n)| {
                n.label()
                    .is_some_and(|label| label.starts_with("Select row 1, column 1"))
            })
            .unwrap()
            .1
            .description()
            .unwrap()
            .to_owned();
        assert!(
            description.contains("2 trailing whitespace characters"),
            "{description}"
        );
        assert!(description.contains("1 tab"));
        let indicators: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::Shape::Path(path) if path.fill == super::theme::LIGHT.indicator_fill => {
                    Some((s.clip_rect, path))
                }
                _ => None,
            })
            .collect();
        assert!(!indicators.is_empty());
        for (clip, path) in indicators {
            assert!(
                path.points.iter().all(|point| clip.contains(*point)),
                "clipped text must not push the indicator outside its cell"
            );
        }
        document.selection = Some(GridSelection::Cell { row: 1, column: 0 });
        assert_eq!(document.copy_selection_text().unwrap(), value);
    }

    #[test]
    fn whitespace_metadata_refreshes_after_cell_header_history_and_discard() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.csv");
        fs::write(&path, b"name\nword\n").unwrap();
        let mut app = individual_edit_app(&path);
        let ctx = egui::Context::default();
        let key = super::whitespace::CellId {
            row: Some(1),
            column: 0,
        };
        let cached = |d: &mut Document| {
            d.whitespace
                .get(key, 4, || panic!("expected cached summary"))
                .unwrap()
        };
        settle_whitespace(&ctx, app.active.document.as_mut().unwrap());
        assert!(!cached(app.active.document.as_mut().unwrap()).any());
        commit_test_cell(app.active.document.as_mut().unwrap(), 1, 0, " word ");
        settle_whitespace(&ctx, app.active.document.as_mut().unwrap());
        assert_eq!(cached(app.active.document.as_mut().unwrap()).trailing, 1);
        app.swap_edit_history(false).unwrap();
        settle_whitespace(&ctx, app.active.document.as_mut().unwrap());
        assert!(!cached(app.active.document.as_mut().unwrap()).any());
        app.swap_edit_history(true).unwrap();
        settle_whitespace(&ctx, app.active.document.as_mut().unwrap());
        assert!(cached(app.active.document.as_mut().unwrap()).likely());
        let d = app.active.document.as_mut().unwrap();
        d.rename_header(0, " name ".into()).unwrap();
        settle_whitespace(&ctx, d);
        assert_eq!(
            d.whitespace
                .get(
                    super::whitespace::CellId {
                        row: None,
                        column: 0
                    },
                    6,
                    || panic!("cached header")
                )
                .unwrap()
                .leading,
            1
        );
        d.discard_edits();
        settle_whitespace(&ctx, d);
        assert!(!cached(d).any());
    }

    #[test]
    fn whitespace_wide_filtered_rows_use_physical_cells_and_isolated_documents() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wide.csv");
        let mut values = vec!["normal"; 65];
        values[64] = " end ";
        fs::write(
            &path,
            format!("{}\n{}\n", vec!["column"; 65].join(","), values.join(",")),
        )
        .unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document
            .start_filter(FilterQuery::single(
                64,
                FilterOperator::Equals,
                b" end ".to_vec(),
            ))
            .unwrap();
        finish_filter(&mut document);
        document.reveal_cell = Some((1, 64));
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let output = settle_whitespace(&ctx, &mut document);
        let node = output
            .platform_output
            .accesskit_update
            .unwrap()
            .nodes
            .into_iter()
            .find(|(_, n)| n.label() == Some("Select row 1, column 65 (column):  end "))
            .unwrap()
            .1;
        assert!(node.description().unwrap().contains("1 leading whitespace"));
        assert!(
            node.description()
                .unwrap()
                .contains("1 trailing whitespace")
        );
        let other = directory.path().join("other.csv");
        fs::write(&other, b"name\nend\n").unwrap();
        let mut other = Document::open(
            &other,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        finish_index(&mut other);
        settle_whitespace(&ctx, &mut other);
        assert!(
            !other
                .whitespace
                .get(
                    super::whitespace::CellId {
                        row: Some(1),
                        column: 0
                    },
                    3,
                    || panic!("cached")
                )
                .unwrap()
                .any()
        );
    }

    #[test]
    fn whitespace_cleanup_reclassifies_replacement_and_save_preserves_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("raw.csv");
        let original = b" name ,notes,empty\n New York ,\"a\tb\r\nc\",\n";
        fs::write(&path, original).unwrap();
        let mut app = individual_edit_app(&path);
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut frame = eframe::Frame::_new_kittest();
        app.apply(&ctx, Action::ShowAllWhitespace(true));
        let d = app.active.document.as_mut().unwrap();
        d.whitespace_all = true;
        settle_whitespace(&ctx, d);
        d.selection = Some(GridSelection::Cell { row: 1, column: 1 });
        assert_eq!(d.copy_selection_text().unwrap(), "a\tb\r\nc");
        let saved = directory.path().join("saved.csv");
        assert_eq!(
            d.start_save_as(saved.clone()).unwrap_err(),
            AppMessage::warning("Make a change before saving.")
        );
        commit_test_cell(d, 1, 2, "saved");
        d.start_save_as(saved.clone()).unwrap();
        finish_app_save(&mut app, &ctx, &mut frame);
        assert_eq!(
            fs::read(&saved).unwrap(),
            b" name ,notes,empty\n New York ,\"a\tb\r\nc\",saved\n"
        );
        let d = app.active.document.as_mut().unwrap();
        d.start_cleanup(quarry_core::ColumnCleanup {
            columns: vec![0],
            operation: quarry_core::CleanupOperation::Trim,
        })
        .unwrap();
        finish_structural_edit(&mut app);
        let d = app.active.document.as_mut().unwrap();
        settle_whitespace(&ctx, d);
        let s = d
            .whitespace
            .get(
                super::whitespace::CellId {
                    row: Some(1),
                    column: 0,
                },
                8,
                || panic!("cached"),
            )
            .unwrap();
        assert!(!s.likely());
        assert!(s.any());
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn whitespace_view_does_not_mark_generated_headers_or_empty_missing_fields() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("no-header.csv");
        fs::write(&path, b"New York,,\none\n").unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                delimiter: Some(b','),
                header_mode: HeaderMode::NoHeader,
                ..Default::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document.whitespace_all = true;
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let output = settle_whitespace(&ctx, &mut document);
        assert!(!output.shapes.iter().any(
            |s| matches!(&s.shape,egui::Shape::Text(t) if t.galley.text().contains("Column·"))
        ));
        let nodes = output.platform_output.accesskit_update.unwrap().nodes;
        assert!(nodes.iter().any(|(_, n)| n.label()
            == Some("Select row 1, column 2 (Column 2): ")
            && n.description().unwrap().contains("No whitespace")));
        assert!(nodes.iter().any(|(_, n)| n.label()
            == Some("Select row 2, column 2 (Column 2): ")
            && n.description().unwrap().contains("absent")));
    }

    #[test]
    fn whitespace_large_header_is_not_reinspected_when_row_buffers_change() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large-header.csv");
        fs::write(
            &path,
            format!(
                "{},value\n{}",
                "H".repeat(1024 * 1024) + " ",
                "row,x\n".repeat(400)
            ),
        )
        .unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                delimiter: Some(b','),
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        let ctx = egui::Context::default();
        settle_whitespace(&ctx, &mut document);
        let id = super::whitespace::CellId {
            row: None,
            column: 0,
        };
        assert_eq!(
            document
                .whitespace
                .get(id, 1024 * 1024 + 1, || panic!(
                    "header should already be classified"
                ))
                .unwrap()
                .trailing,
            1
        );
        document.load_buffer(250).unwrap();
        assert_eq!(
            document
                .whitespace
                .get(id, 1024 * 1024 + 1, || panic!(
                    "scrolling must retain completed header inspection"
                ))
                .unwrap()
                .trailing,
            1
        );
    }

    #[test]
    fn whitespace_zero_match_filter_poll_keeps_completed_header_inspection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zero-match-header.csv");
        fs::write(&path, b" name ,value\nrow,x\n").unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        let ctx = egui::Context::default();
        settle_whitespace(&ctx, &mut document);
        document
            .start_filter(FilterQuery::single(
                0,
                FilterOperator::Equals,
                b"absent".to_vec(),
            ))
            .unwrap();
        document.poll_filter_scan().unwrap();
        let id = super::whitespace::CellId {
            row: None,
            column: 0,
        };
        assert_eq!(
            document
                .whitespace
                .get(id, 6, || panic!(
                    "zero-match polls must retain header inspection"
                ))
                .unwrap()
                .leading,
            1
        );
    }

    #[test]
    fn sort_estimate_and_dialog_frames_do_not_copy_owned_edit_snapshots() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sort-snapshots.csv");
        fs::write(&path, b"name,value\nrow,x\n").unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        let pending = "p".repeat(1024 * 1024);
        commit_test_cell(&mut document, 1, 1, &pending);
        document.rename_header(0, "header".repeat(1024)).unwrap();
        document.begin_cell_edit(1, 1, b"x".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "effective draft".into();
        let headers = std::collections::BTreeMap::from([(
            0,
            document.header_renames[&0].as_bytes().to_vec(),
        )]);
        let cells = std::collections::BTreeMap::from([((1, 1), b"effective draft".to_vec())]);
        let records = document.index.as_ref().unwrap().indexed_rows();
        let expected = estimate_sort_temporary_bytes(
            quarry_core::estimate_edited_output_bytes(
                document.session.file_size,
                records,
                &headers,
                &cells,
                None,
                None,
            ),
            records - document.data_start,
        );
        quarry_workbench::reset_owned_edit_snapshot_bytes();
        assert_eq!(document.sort_temporary_disk_estimate(), Some(expected));
        let ctx = egui::Context::default();
        let mut dialog = StructuralDialog::sort(0);
        let mut match_case = false;
        for _ in 0..3 {
            let _ = ctx.run(grid_input(), |ctx| {
                show_structural_dialog(ctx, &mut dialog, &mut match_case, &document);
            });
        }
        assert_eq!(
            quarry_workbench::owned_edit_snapshot_bytes(),
            0,
            "read-only estimates must not copy pending edit payloads"
        );
        assert_eq!(document.cell_edits[&(1, 1)].as_slice(), pending.as_bytes());
        assert_eq!(
            document.cell_edit.as_ref().unwrap().draft,
            "effective draft"
        );
        assert_eq!(fs::read(path).unwrap(), b"name,value\nrow,x\n");
    }

    #[test]
    fn multi_column_sort_controls_reorder_levels_and_keep_independent_settings() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("levels.csv");
        fs::write(&source, b"account,amount,date\nb,3,2026-10-18\n").unwrap();
        let mut app = QuarryApp::new(Some(source), Instant::now());
        finish_index(app.active.document.as_mut().unwrap());
        let mut dialog = StructuralDialog::sort(0);
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        assert_eq!(
            click_sort_dialog_control(&ctx, &mut dialog, &mut app, "Add sort level").0,
            None
        );
        assert_eq!(dialog.sort_levels.len(), 1);
        assert_eq!(dialog.sort_levels[0].column, 1);
        dialog.sort_levels[0].mode = SortMode::Number;
        dialog.sort_levels[0].direction = super::SortDirection::Descending;
        assert_eq!(
            click_sort_dialog_control(&ctx, &mut dialog, &mut app, "Move level 1 down").0,
            None
        );
        assert_eq!(dialog.columns, vec![1]);
        assert_eq!(dialog.sort_mode, SortMode::Number);
        assert_eq!(dialog.sort_direction, super::SortDirection::Descending);
        assert_eq!(dialog.sort_levels[0].column, 0);
        assert_eq!(dialog.sort_levels[0].mode, SortMode::Text);
        assert_eq!(
            click_sort_dialog_control(&ctx, &mut dialog, &mut app, "Remove level 1").0,
            None
        );
        assert!(dialog.sort_levels.is_empty());
        assert_eq!(dialog.columns, vec![0]);
    }

    #[test]
    fn multi_column_sort_includes_pending_edits_and_has_one_history_step() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("multi.csv");
        let original = b"id,account,amount\n1,b,1\n2,a,2\n3,a,10\n4,a,2\n";
        fs::write(&source, original).unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        let document = app.active.document.as_mut().unwrap();
        finish_index(document);
        commit_test_cell(document, 1, 1, "a");
        let mut dialog = StructuralDialog::sort(1);
        dialog.sort_levels.push(super::SortSpec {
            column: 2,
            mode: SortMode::Number,
            direction: super::SortDirection::Descending,
            case_sensitivity: CaseSensitivity::Sensitive,
        });
        app.active.structural_dialog = Some(dialog);
        app.apply_structural_dialog_action(StructuralDialogAction::Apply);
        finish_structural_edit(&mut app);
        let document = app.active.document.as_ref().unwrap();
        let sorted = fs::read(document.session.path()).unwrap();
        assert_eq!(sorted, b"id,account,amount\n3,a,10\n2,a,2\n4,a,2\n1,a,1\n");
        assert!(document.is_dirty());
        app.swap_structural_history(false).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        assert_eq!(
            app.active.document.as_ref().unwrap().cell_edits[&(1, 1)].as_slice(),
            b"a"
        );
        app.swap_structural_history(true).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        assert_eq!(
            fs::read(app.active.document.as_ref().unwrap().session.path()).unwrap(),
            sorted
        );
        let saved = directory.path().join("saved.csv");
        assert!(app.save_as_picker_result(Some(saved.clone())));
        let ctx = egui::Context::default();
        let mut frame = eframe::Frame::_new_kittest();
        finish_app_save(&mut app, &ctx, &mut frame);
        assert_eq!(fs::read(saved).unwrap(), sorted);
        assert_eq!(fs::read(source).unwrap(), original);
    }

    #[test]
    fn secondary_sort_levels_have_distinct_accessible_direction_and_case_names() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("accessible-levels.csv");
        fs::write(&source, b"a,b,c\nx,y,z\n").unwrap();
        let mut app = QuarryApp::new(Some(source), Instant::now());
        finish_index(app.active.document.as_mut().unwrap());
        let document = app.active.document.as_ref().unwrap();
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let output = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let mut date_suggestions = std::collections::BTreeMap::new();
                for index in 1..=2 {
                    let mut key = super::SortSpec {
                        column: index,
                        mode: SortMode::Text,
                        direction: super::SortDirection::Ascending,
                        case_sensitivity: CaseSensitivity::Insensitive,
                    };
                    super::show_additional_sort_level(
                        ui,
                        document,
                        &mut key,
                        index,
                        &mut date_suggestions,
                    );
                }
            });
        });
        let tree = output.platform_output.accesskit_update.unwrap();
        let labels: Vec<_> = tree
            .nodes
            .iter()
            .filter_map(|(_, node)| node.label())
            .collect();
        for level in 2..=3 {
            for control in ["Ascending", "Descending", "Match case"] {
                let expected = format!("{control} (level {level})");
                assert!(
                    labels.contains(&expected.as_str()),
                    "missing {expected}: {labels:?}"
                );
            }
        }
        for control in ["Ascending", "Descending", "Match case"] {
            assert!(
                !labels.contains(&control),
                "secondary controls must identify their level"
            );
        }
    }
    #[test]
    fn encoding_choice_survives_structural_history_and_native_save() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("latin.csv");
        let copy = directory.path().join("copy.csv");
        let input = b"first,last,age\r\nAndr\xe9,M\xfcller,36\r\n";
        fs::write(&source, input).unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        app.open_document_tab_with_encoding(
            source.clone(),
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
            Some(Encoding::Latin1),
        )
        .unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_combine(vec![0, 1], b" ".to_vec())
            .unwrap();
        finish_structural_edit(&mut app);
        assert_eq!(
            app.active.document.as_ref().unwrap().session.encoding(),
            Encoding::Latin1
        );
        app.swap_structural_history(false).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        assert_eq!(
            app.active.document.as_ref().unwrap().session.first_rows[1].fields[0],
            "André".as_bytes()
        );
        app.swap_structural_history(true).unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_save_as(copy.clone())
            .unwrap();
        let ctx = egui::Context::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.active.document.as_ref().unwrap().save_job.is_some() {
            assert!(Instant::now() < deadline);
            app.poll_active_document(&ctx, true);
            std::thread::yield_now();
        }
        assert_eq!(
            fs::read(&copy).unwrap(),
            b"first,age\r\nAndr\xe9 M\xfcller,36\r\n"
        );
        assert_eq!(fs::read(&source).unwrap(), input);
        assert_eq!(
            app.active.document.as_ref().unwrap().session.encoding(),
            Encoding::Latin1
        );
        assert_eq!(app.active.encoding_mode, Some(Encoding::Latin1));
        app.reload_document();
        assert!(app.active.notice.is_none());
        assert_eq!(
            app.active.document.as_ref().unwrap().session.first_rows[1].fields[0],
            "André Müller".as_bytes()
        );
    }

    #[test]
    fn utf8_copy_retains_edits_after_native_encoding_rejects_them() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("windows.csv");
        let copy = directory.path().join("unicode.csv");
        let input = b"name\r\ncaf\xe9\r\n";
        fs::write(&source, input).unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        app.open_document_tab_with_encoding(
            source.clone(),
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
            Some(Encoding::Windows1252),
        )
        .unwrap();
        finish_index(app.active.document.as_mut().unwrap());
        let doc = app.active.document.as_mut().unwrap();
        doc.begin_cell_edit(1, 0, "café".as_bytes().to_vec())
            .unwrap();
        doc.cell_edit.as_mut().unwrap().draft = "café 😀".into();
        doc.start_save().unwrap();
        let ctx = egui::Context::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.active.document.as_ref().unwrap().save_job.is_some() {
            assert!(Instant::now() < deadline);
            app.poll_active_document(&ctx, true);
            std::thread::yield_now();
        }
        assert_eq!(fs::read(&source).unwrap(), input);
        assert!(app.active.document.as_ref().unwrap().is_dirty());
        assert!(
            app.active
                .notice
                .as_ref()
                .unwrap()
                .text
                .contains("Save As UTF-8")
        );
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_save_operation_format(Some(copy.clone()), true)
            .unwrap();
        while app.active.document.as_ref().unwrap().save_job.is_some() {
            assert!(Instant::now() < deadline);
            app.poll_active_document(&ctx, true);
            std::thread::yield_now();
        }
        assert_eq!(fs::read(&copy).unwrap(), "name\r\ncafé 😀\r\n".as_bytes());
        assert_eq!(fs::read(&source).unwrap(), input);
        assert_eq!(
            app.active.document.as_ref().unwrap().session.encoding(),
            Encoding::Utf8
        );
        assert!(!app.active.document.as_ref().unwrap().is_dirty());
    }

    #[test]
    fn changing_encoding_reopens_values_without_changing_source() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("legacy.csv");
        fs::write(&source, b"name\ncaf\xe9\n").unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        app.reopen_document_with_encoding(
            DelimiterMode::Comma,
            HeaderMode::FirstRow,
            Some(Encoding::Latin1),
        );
        assert!(app.active.notice.is_none());
        assert_eq!(
            app.active.document.as_ref().unwrap().session.first_rows[1].fields[0],
            "café".as_bytes()
        );
        assert_eq!(app.active.encoding_mode, Some(Encoding::Latin1));
        assert_eq!(fs::read(&source).unwrap(), b"name\ncaf\xe9\n");
    }
    #[test]
    fn native_save_keeps_the_requested_auto_encoding_choice() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("unicode.csv");
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend("name\ncafé\n".encode_utf16().flat_map(u16::to_le_bytes));
        fs::write(&source, bytes).unwrap();
        let mut app = QuarryApp::new(Some(source), Instant::now());
        let doc = app.active.document.as_mut().unwrap();
        finish_index(doc);
        doc.begin_cell_edit(1, 0, "café".as_bytes().to_vec())
            .unwrap();
        doc.cell_edit.as_mut().unwrap().draft = "new".into();
        doc.start_save().unwrap();
        let ctx = egui::Context::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.active.document.as_ref().unwrap().save_job.is_some() {
            assert!(Instant::now() < deadline);
            app.poll_active_document(&ctx, true);
            std::thread::yield_now();
        }
        assert_eq!(app.active.encoding_mode, None);
        assert_eq!(
            app.active.document.as_ref().unwrap().session.encoding(),
            Encoding::Utf16Le
        );
    }
    #[test]
    fn opening_an_existing_tab_with_another_encoding_explains_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.csv");
        fs::write(&source, b"name\ncaf\xe9\n").unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        let error = app
            .open_document_tab_with_encoding(source, OpenOptions::default(), Some(Encoding::Latin1))
            .unwrap_err();
        assert!(error.text.contains("Format"));
        assert_eq!(
            app.active.document.as_ref().unwrap().session.encoding(),
            Encoding::Utf8
        );
    }
    #[test]
    fn discarding_structural_changes_keeps_auto_encoding_for_later_reload() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.csv");
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend(
            "first,last\nAda,Lovelace\n"
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        );
        fs::write(&source, bytes).unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish_index(app.active.document.as_mut().unwrap());
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_combine(vec![0, 1], b" ".to_vec())
            .unwrap();
        finish_structural_edit(&mut app);
        app.apply(&egui::Context::default(), Action::DiscardChanges);
        assert_eq!(app.active.encoding_mode, None);
        fs::write(&source, "first,last\nGrace,Hopper\n").unwrap();
        app.reload_document();
        assert!(app.active.notice.is_none());
        assert_eq!(
            app.active.document.as_ref().unwrap().session.encoding(),
            Encoding::Utf8
        );
    }

    #[test]
    fn sort_dialog_natural_mode_and_bounded_date_suggestion_hint_never_auto_select() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phase3b-sort.csv");
        fs::write(
            &path,
            b"ver,when\nv1.10,01/02/2020\nv1.2,03/04/2026\nV1.9,01/02/2020\n",
        )
        .unwrap();
        let mut app = individual_edit_app(&path);
        let document = app.active.document.as_ref().unwrap();

        // Natural sort mode plan & disk estimate.
        let mut natural_dialog = StructuralDialog::sort(0);
        natural_dialog.sort_mode = SortMode::Natural;
        let natural_plan = natural_dialog.sort_plan(false);
        assert_eq!(natural_plan.keys[0].mode, SortMode::Natural);
        assert_eq!(
            natural_plan.keys[0].case_sensitivity,
            CaseSensitivity::Insensitive
        );
        let text_plan = StructuralDialog::sort(0).sort_plan(false);
        assert!(
            document.sort_plan_disk_estimate(&natural_plan).unwrap()
                > document.sort_plan_disk_estimate(&text_plan).unwrap()
        );

        // Date / time mode caches bounded suggestion without copying edits or auto-selecting.
        let mut date_dialog = StructuralDialog::sort(1);
        date_dialog.sort_mode = SortMode::DateTime {
            format: quarry_core::DateTimeFormat::YearMonthDay,
        };
        quarry_workbench::reset_owned_edit_snapshot_bytes();
        let ctx = egui::Context::default();
        let mut sort_match_case = false;
        let _ = ctx.run(grid_input(), |ctx| {
            let _ = super::show_structural_dialog(
                ctx,
                &mut date_dialog,
                &mut sort_match_case,
                document,
            );
        });
        assert_eq!(quarry_workbench::owned_edit_snapshot_bytes(), 0);
        // Must NEVER auto-change the user's chosen format.
        assert_eq!(
            date_dialog.sort_mode,
            SortMode::DateTime {
                format: quarry_core::DateTimeFormat::YearMonthDay
            }
        );
        let cached = date_dialog
            .date_format_suggestions
            .get(&1)
            .and_then(|entry| entry.as_ref())
            .expect("date suggestion should be cached for column 1");
        assert!(cached.is_ambiguous());
        let hint = super::format_date_suggestion_hint(cached);
        assert!(hint.contains("ambiguous"));
        assert!(hint.contains("MM/DD/YYYY"));
        assert!(hint.contains("DD/MM/YYYY"));

        // Run Natural sort end-to-end through the dialog.
        app.open_structural_dialog(natural_dialog);
        app.apply_structural_dialog_action(StructuralDialogAction::Apply);
        finish_structural_edit(&mut app);
        let document = app.active.document.as_ref().unwrap();
        let rows = document
            .session
            .read_rows(document.index.as_ref().unwrap(), 1, 3)
            .unwrap();
        let first_col: Vec<String> = rows
            .iter()
            .map(|r| String::from_utf8_lossy(&r.fields[0]).into_owned())
            .collect();
        assert_eq!(first_col, vec!["v1.2", "V1.9", "v1.10"]);
    }

    #[test]
    fn duplicates_dialog_all_fields_runs_without_selected_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phase3b-dedup.csv");
        fs::write(&path, b"a,b\nx,1\n\"x\",1\nx,1,\ny,2\n").unwrap();
        let mut app = individual_edit_app(&path);

        let dialog = StructuralDialog::duplicates(Vec::new());
        assert!(dialog.duplicate_all_fields);
        assert_eq!(
            dialog.duplicate_target(),
            quarry_core::DuplicateTarget::AllFields
        );
        app.open_structural_dialog(dialog);
        app.apply_structural_dialog_action(StructuralDialogAction::Apply);
        finish_duplicate_search(&mut app);
        let Some(super::StructuralJob::DuplicatePreview { summary, .. }) =
            &app.active.document.as_ref().unwrap().structural_job
        else {
            panic!("expected duplicate preview");
        };
        assert_eq!(summary.duplicate_rows, 1);
        assert_eq!(summary.retained_rows, 3);
        app.confirm_duplicate_removal().unwrap();
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(
            fs::read_to_string(document.session.path()).unwrap(),
            "a,b\nx,1\nx,1,\ny,2\n"
        );
    }
}
