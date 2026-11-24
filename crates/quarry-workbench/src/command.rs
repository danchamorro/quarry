//! Frontend-neutral window/document identities, command enablement, and targeting.

use crate::{AppMessage, Document, JOIN_INPUT_RESERVED, WorkbenchTab};
use quarry_core::Encoding;
use std::fmt;

/// Stable identity for one top-level application window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(u64);

impl WindowId {
    /// Default primary window used by single-window frontends.
    pub const PRIMARY: Self = Self(1);

    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for WindowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "window:{}", self.0)
    }
}

/// Stable identity for one open document tab across switches, windows, and dormancy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocumentId(u64);

impl DocumentId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for DocumentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "document:{}", self.0)
    }
}

/// Frontend-neutral workbench command identifiers mapped from menus, toolbars, and dialogs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandId {
    About,
    NewWindow,
    Open,
    OpenWithEncoding(Encoding),
    CombineFiles,
    JoinFiles,
    CloseTab,
    CloseWindow,
    MoveTabToNewWindow,
    KeepEditing,
    DiscardAndClose,
    ReloadFromDisk,
    Save,
    SaveAs,
    SaveAsUtf8,
    DiscardChanges,
    TemporaryStorage,
    InspectFile,
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    Find,
    InsertRows,
    InsertColumns,
    ShowAllWhitespace,
    Columns,
    Filters,
    PauseResumeIndexing,
    NextTab,
    PreviousTab,
    Quit,
    UserGuide,
    ReportIssue,
}

/// Logical scope a command acts on when dispatched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CommandTarget {
    Application,
    Window(WindowId),
    Document {
        window: WindowId,
        document: DocumentId,
    },
}

/// Resolved availability and target for a command in a given workbench context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandResolution {
    pub enabled: bool,
    pub target: Option<CommandTarget>,
}

/// Which File commands are available; shared by the toolbar File menu and the menu bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileCommands {
    pub combine: bool,
    pub reload: bool,
    pub save: bool,
    pub save_as: bool,
    pub save_as_utf8: bool,
    pub discard: bool,
    pub temporary_storage: bool,
    pub inspect: bool,
}

impl FileCommands {
    /// `write_slot_available` is false when the app-wide limit on concurrent
    /// file writes is already reached.
    pub fn new(document: Option<&Document>, write_slot_available: bool) -> Self {
        let open = document.is_some();
        let dirty = document.is_some_and(Document::is_dirty);
        let file_operation_active = document.is_some_and(|document| {
            document.export_job.is_some()
                || document.save_job.is_some()
                || document.structural_job.is_some()
        });
        let join_reserved = document.is_some_and(Document::join_reserved);
        Self {
            // Combining writes a new file, so it needs only a free write slot.
            combine: write_slot_available,
            reload: open && !file_operation_active && !dirty && !join_reserved,
            save: document.is_some_and(Document::is_save_ready),
            save_as: document.is_some_and(Document::is_save_as_ready),
            save_as_utf8: document.is_some_and(|document| {
                document.session.encoding() != Encoding::Utf8 && document.save_operations_ready()
            }),
            discard: dirty && !file_operation_active && !join_reserved,
            temporary_storage: !file_operation_active,
            inspect: !file_operation_active,
        }
    }
}

/// Snapshot of window, active tab, and modal state used to evaluate command enablement.
pub struct CommandContext<'a> {
    pub window: WindowId,
    pub active_tab: &'a WorkbenchTab,
    pub open_document_count: usize,
    pub write_slot_available: bool,
    /// True when an app/frontend modal outside `WorkbenchTab` blocks document commands
    /// (quitting cleanup, inspection recovery, storage review, About, or Combine Files).
    pub external_modal_blocked: bool,
    /// True when a frontend text field currently owns keyboard focus (routes Undo/Redo locally).
    pub text_field_focused: bool,
    pub show_all_whitespace: bool,
}

impl CommandContext<'_> {
    /// True when any modal or pending close flow blocks ordinary document/menu commands.
    pub fn modal_blocked(&self) -> bool {
        self.external_modal_blocked || self.active_tab.blocks_commands()
    }
}

/// Evaluates whether `command` is currently enabled and which scope it targets.
pub fn evaluate_command(command: CommandId, ctx: &CommandContext<'_>) -> CommandResolution {
    let document = ctx.active_tab.document.as_ref();
    let open = document.is_some();
    let doc_target = open.then_some(CommandTarget::Document {
        window: ctx.window,
        document: ctx.active_tab.document_id(),
    });
    let target = match command {
        CommandId::About
        | CommandId::NewWindow
        | CommandId::Open
        | CommandId::OpenWithEncoding(_)
        | CommandId::CombineFiles
        | CommandId::JoinFiles
        | CommandId::ShowAllWhitespace
        | CommandId::Quit
        | CommandId::UserGuide
        | CommandId::ReportIssue => Some(CommandTarget::Application),
        CommandId::CloseWindow | CommandId::NextTab | CommandId::PreviousTab => {
            Some(CommandTarget::Window(ctx.window))
        }
        CommandId::TemporaryStorage | CommandId::InspectFile | CommandId::KeepEditing => {
            doc_target.or(Some(CommandTarget::Window(ctx.window)))
        }
        CommandId::Undo | CommandId::Redo if ctx.text_field_focused => {
            Some(CommandTarget::Window(ctx.window))
        }
        CommandId::CloseTab
        | CommandId::MoveTabToNewWindow
        | CommandId::DiscardAndClose
        | CommandId::ReloadFromDisk
        | CommandId::Save
        | CommandId::SaveAs
        | CommandId::SaveAsUtf8
        | CommandId::DiscardChanges
        | CommandId::Undo
        | CommandId::Redo
        | CommandId::Cut
        | CommandId::Copy
        | CommandId::Paste
        | CommandId::SelectAll
        | CommandId::Find
        | CommandId::InsertRows
        | CommandId::InsertColumns
        | CommandId::Columns
        | CommandId::Filters
        | CommandId::PauseResumeIndexing => doc_target,
    };

    // Close-review actions are explicitly active during the close confirmation/wait modal.
    match command {
        CommandId::KeepEditing => {
            let pending = ctx.active_tab.close_confirmation_open
                || ctx.active_tab.close_after_save
                || ctx.active_tab.close_after_jobs;
            return CommandResolution {
                enabled: pending && !ctx.external_modal_blocked,
                target,
            };
        }
        CommandId::DiscardAndClose => {
            return CommandResolution {
                enabled: open
                    && ctx.active_tab.close_confirmation_open
                    && !ctx.external_modal_blocked,
                target,
            };
        }
        _ => {}
    }

    // Text-field history is safe to route even while a dialog blocks document commands.
    let text_history =
        ctx.text_field_focused && matches!(command, CommandId::Undo | CommandId::Redo);
    if ctx.modal_blocked() && !text_history {
        // Clipboard/View/Help commands that are not gated by the menu disabled list stay enabled
        // only when no document-modal rule applies; keep parity with desktop menu availability.
        return match command {
            CommandId::Cut
            | CommandId::Copy
            | CommandId::Paste
            | CommandId::SelectAll
            | CommandId::ShowAllWhitespace
            | CommandId::Quit
            | CommandId::UserGuide
            | CommandId::ReportIssue => CommandResolution {
                enabled: true,
                target,
            },
            _ => CommandResolution {
                enabled: false,
                target,
            },
        };
    }

    let files = FileCommands::new(document, ctx.write_slot_available);
    let filter_active = document.is_some_and(Document::filter_active);
    let insertion =
        |rows| document.is_some_and(|document| document.insertion_disabled_reason(rows).is_none());
    let available = match command {
        CommandId::About
        | CommandId::NewWindow
        | CommandId::Open
        | CommandId::OpenWithEncoding(_)
        | CommandId::JoinFiles
        | CommandId::CloseWindow
        | CommandId::Cut
        | CommandId::Copy
        | CommandId::Paste
        | CommandId::SelectAll
        | CommandId::ShowAllWhitespace
        | CommandId::Quit
        | CommandId::UserGuide
        | CommandId::ReportIssue => true,
        CommandId::CombineFiles => files.combine,
        CommandId::CloseTab
        | CommandId::MoveTabToNewWindow
        | CommandId::Columns
        | CommandId::Filters => open,
        CommandId::ReloadFromDisk => files.reload,
        CommandId::Save => files.save,
        CommandId::SaveAs => files.save_as,
        CommandId::SaveAsUtf8 => files.save_as_utf8,
        CommandId::DiscardChanges => files.discard,
        CommandId::InspectFile => files.inspect,
        CommandId::TemporaryStorage => files.temporary_storage,
        CommandId::Undo => ctx.text_field_focused || document.is_some_and(Document::can_undo),
        CommandId::Redo => ctx.text_field_focused || document.is_some_and(Document::can_redo),
        CommandId::Find => open && !filter_active,
        CommandId::InsertRows => insertion(true),
        CommandId::InsertColumns => insertion(false),
        CommandId::PauseResumeIndexing => {
            document.is_some_and(|doc| doc.can_pause_indexing() || doc.can_resume_indexing())
        }
        CommandId::NextTab | CommandId::PreviousTab => ctx.open_document_count > 1,
        CommandId::KeepEditing | CommandId::DiscardAndClose => unreachable!(),
    };

    CommandResolution {
        enabled: available,
        target,
    }
}

/// Outcome of executing a close-lifecycle command (`CloseTab`, `KeepEditing`, `DiscardAndClose`)
/// on a [`WorkbenchTab`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseCommandOutcome {
    /// Tab has unsaved changes; frontend should present Save / Discard / Keep Editing.
    ConfirmUnsaved,
    /// Tab has an active save job and will close once saving finishes.
    WaitForSave,
    /// Tab has active file jobs or cleanup in flight and will re-review once they finish.
    WaitForJobs,
    /// Pending close was cancelled (Keep Editing); document and drafts are preserved.
    KeptEditing,
    /// Tab is clear to retire (or record a quit approval).
    ReadyToClose,
    /// Command was a no-op in the current state (e.g. Keep Editing during quitting cleanup).
    Ignored,
}

impl WorkbenchTab {
    /// Returns this tab's typed [`DocumentId`].
    pub const fn document_id(&self) -> DocumentId {
        DocumentId::new(self.id)
    }

    /// True when this tab owns an active modal dialog or close flow that blocks ordinary commands.
    pub fn blocks_commands(&self) -> bool {
        self.close_after_save
            || self.close_after_jobs
            || self.close_confirmation_open
            || self.structural_dialog.is_some()
    }

    /// Executes the UI-neutral close vertical slice (`CloseTab`, `KeepEditing`, `DiscardAndClose`).
    pub fn dispatch_close_command(
        &mut self,
        command: CommandId,
        extra_writing: bool,
        cleanup_busy: bool,
        quitting_cleanup: bool,
    ) -> Result<CloseCommandOutcome, AppMessage> {
        match command {
            CommandId::CloseTab => {
                if cleanup_busy {
                    if !quitting_cleanup {
                        self.close_after_jobs = true;
                    }
                    return Ok(CloseCommandOutcome::WaitForJobs);
                }
                if self.document.as_ref().is_some_and(Document::join_reserved) {
                    return Err(AppMessage::warning(JOIN_INPUT_RESERVED));
                }
                if let Some(document) = self.document.as_mut() {
                    document.commit_edits();
                    if document.save_job.is_some() {
                        self.close_after_save = true;
                        return Ok(CloseCommandOutcome::WaitForSave);
                    }
                }
                if extra_writing || self.document_writing() {
                    self.close_after_jobs = true;
                    return Ok(CloseCommandOutcome::WaitForJobs);
                }
                if self.is_dirty() {
                    self.close_confirmation_open = true;
                    return Ok(CloseCommandOutcome::ConfirmUnsaved);
                }
                self.close_confirmation_open = false;
                self.close_after_save = false;
                self.close_after_jobs = false;
                Ok(CloseCommandOutcome::ReadyToClose)
            }
            CommandId::KeepEditing => {
                if quitting_cleanup {
                    return Ok(CloseCommandOutcome::Ignored);
                }
                self.cancel_close();
                Ok(CloseCommandOutcome::KeptEditing)
            }
            CommandId::DiscardAndClose => {
                if !self.close_confirmation_open {
                    return Ok(CloseCommandOutcome::Ignored);
                }
                if cleanup_busy {
                    if !quitting_cleanup {
                        self.close_after_jobs = true;
                    }
                    return Ok(CloseCommandOutcome::WaitForJobs);
                }
                if self.document.as_ref().is_some_and(Document::join_reserved) {
                    return Err(AppMessage::warning(JOIN_INPUT_RESERVED));
                }
                if self
                    .document
                    .as_ref()
                    .is_some_and(|document| document.save_job.is_some())
                {
                    self.close_confirmation_open = false;
                    self.close_after_save = true;
                    return Ok(CloseCommandOutcome::WaitForSave);
                }
                if extra_writing || self.document_writing() {
                    self.close_confirmation_open = false;
                    self.close_after_jobs = true;
                    return Ok(CloseCommandOutcome::WaitForJobs);
                }
                self.approve_discard_and_close();
                Ok(CloseCommandOutcome::ReadyToClose)
            }
            _ => Ok(CloseCommandOutcome::Ignored),
        }
    }
}
