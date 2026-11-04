//! The native macOS menu bar.
//!
//! A command with a keyboard shortcut becomes the same egui event its shortcut
//! produces, so focus rules (text-field undo, copy from a cell or a field) stay
//! where the shortcut is already handled. Other commands become an [`Action`]
//! that passes through the same gate as the toolbar.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]
use super::Action;
use eframe::egui;
use quarry_core::Encoding;

pub(super) const USER_GUIDE_URL: &str =
    "https://github.com/danchamorro/quarry/blob/main/docs/USER_GUIDE.md";
pub(super) const REPORT_ISSUE_URL: &str = "https://github.com/danchamorro/quarry/issues/new/choose";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MenuCommand {
    About,
    Open,
    OpenWithEncoding(Encoding),
    CombineFiles,
    JoinFiles,
    CloseTab,
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
    NextTab,
    PreviousTab,
    UserGuide,
    ReportIssue,
}

/// What a menu command does inside the egui frame.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Routed {
    Events(Vec<egui::Event>),
    Action(Action),
    Url(&'static str),
}

fn key(key: egui::Key, modifiers: egui::Modifiers) -> Routed {
    Routed::Events(vec![egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }])
}

/// Maps a command to its egui effect. `paste` reads the system clipboard only for Paste.
pub(super) fn route(
    command: MenuCommand,
    show_all_whitespace: bool,
    paste: impl FnOnce() -> Option<String>,
) -> Option<Routed> {
    use MenuCommand::*;
    use egui::{Key, Modifiers};
    Some(match command {
        Open => key(Key::O, Modifiers::COMMAND),
        CloseTab => key(Key::W, Modifiers::COMMAND),
        Save => key(Key::S, Modifiers::COMMAND),
        Undo => key(Key::Z, Modifiers::COMMAND),
        Redo => key(Key::Z, Modifiers::COMMAND | Modifiers::SHIFT),
        SelectAll => key(Key::A, Modifiers::COMMAND),
        Find => key(Key::F, Modifiers::COMMAND),
        NextTab => key(Key::Tab, Modifiers::CTRL),
        PreviousTab => key(Key::Tab, Modifiers::CTRL | Modifiers::SHIFT),
        Cut => Routed::Events(vec![egui::Event::Cut]),
        Copy => Routed::Events(vec![egui::Event::Copy]),
        Paste => Routed::Events(vec![egui::Event::Paste(paste()?)]),
        About => Routed::Action(Action::About),
        OpenWithEncoding(encoding) => Routed::Action(Action::ChooseWithEncoding(encoding)),
        CombineFiles => Routed::Action(Action::CombineFiles),
        JoinFiles => Routed::Action(Action::JoinFiles),
        ReloadFromDisk => Routed::Action(Action::ReloadFromDisk),
        SaveAs => Routed::Action(Action::ChooseSaveAs),
        SaveAsUtf8 => Routed::Action(Action::ChooseSaveAsUtf8),
        DiscardChanges => Routed::Action(Action::DiscardChanges),
        TemporaryStorage => Routed::Action(Action::TemporaryStorage),
        InspectFile => Routed::Action(Action::InspectFile),
        InsertRows => Routed::Action(Action::InsertRows),
        InsertColumns => Routed::Action(Action::InsertColumns),
        ShowAllWhitespace => Routed::Action(Action::ShowAllWhitespace(!show_all_whitespace)),
        Columns => Routed::Action(Action::OpenColumns),
        Filters => Routed::Action(Action::OpenFilters),
        UserGuide => Routed::Url(USER_GUIDE_URL),
        ReportIssue => Routed::Url(REPORT_ISSUE_URL),
    })
}

/// Which commands are available, mirrored into the menu bar each frame it changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct MenuState {
    pub disabled: Vec<MenuCommand>,
    pub show_all_whitespace: bool,
}

impl MenuState {
    pub(super) fn enabled(&self, command: MenuCommand) -> bool {
        let command = match command {
            MenuCommand::OpenWithEncoding(_) => MenuCommand::Open,
            command => command,
        };
        !self.disabled.contains(&command)
    }
}

/// One menu entry: title, command, shortcut key, and extra modifiers beyond Command.
#[cfg(target_os = "macos")]
enum Entry {
    Command(&'static str, MenuCommand, &'static str, Extra),
    Encodings,
    Native(&'static str, &'static str, &'static str, Extra),
    Separator,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
enum Extra {
    Plain,
    Shift,
    Option,
    Control,
    ControlShift,
    ControlCommand,
}

#[cfg(target_os = "macos")]
mod native {
    //! AppKit calls go through `msg_send!` on untyped objects. The typed bindings for
    //! NSMenu, NSMenuItem, and NSPasteboard would need extra `objc2-app-kit` features,
    //! which changes the audited dependency inventory.
    use super::*;
    use objc2::rc::{Allocated, Retained};
    use objc2::runtime::{AnyObject, NSObject, Sel};
    use objc2::{MainThreadMarker, MainThreadOnly, class, define_class, msg_send, sel};
    use objc2_app_kit::NSApplication;
    use objc2_foundation::NSString;
    use std::cell::RefCell;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Mutex, OnceLock};

    // NSEventModifierFlags values from AppKit's NSEvent.h.
    const SHIFT: usize = 1 << 17;
    const CONTROL: usize = 1 << 18;
    const OPTION: usize = 1 << 19;
    const COMMAND: usize = 1 << 20;

    /// Where menu actions go: the app's receiver and the context to wake.
    type Dispatch = Option<(Sender<MenuCommand>, egui::Context)>;
    static DISPATCH: OnceLock<Mutex<Dispatch>> = OnceLock::new();

    thread_local! {
        static ITEMS: RefCell<Vec<(MenuCommand, Retained<AnyObject>)>> = const { RefCell::new(Vec::new()) };
        static TARGET: RefCell<Option<Retained<MenuTarget>>> = const { RefCell::new(None) };
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements and MenuTarget has no Drop.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "QuarryMenuTarget"]
        struct MenuTarget;

        impl MenuTarget {
            #[unsafe(method(runCommand:))]
            fn run_command(&self, sender: &AnyObject) {
                // SAFETY: AppKit sends this action from an NSMenuItem, which has `tag`.
                let tag: isize = unsafe { msg_send![sender, tag] };
                let command = ITEMS.with(|items| {
                    items
                        .borrow()
                        .get(usize::try_from(tag).unwrap_or(usize::MAX))
                        .map(|(command, _)| *command)
                });
                if let (Some(command), Some(dispatch)) = (command, DISPATCH.get())
                    && let Ok(dispatch) = dispatch.lock()
                    && let Some((sender, ctx)) = dispatch.as_ref()
                    && sender.send(command).is_ok()
                {
                    ctx.request_repaint();
                }
            }
        }
    );

    fn modifiers(extra: Extra) -> usize {
        match extra {
            Extra::Plain => COMMAND,
            Extra::Shift => COMMAND | SHIFT,
            Extra::Option => COMMAND | OPTION,
            // Control-Tab tab switching has no Command modifier.
            Extra::Control => CONTROL,
            Extra::ControlShift => CONTROL | SHIFT,
            Extra::ControlCommand => COMMAND | CONTROL,
        }
    }

    fn new_menu(title: &str) -> Retained<AnyObject> {
        let title = NSString::from_str(title);
        // SAFETY: standard NSMenu allocation and designated initializer.
        unsafe {
            let menu: Allocated<AnyObject> = msg_send![class!(NSMenu), alloc];
            msg_send![menu, initWithTitle: &*title]
        }
    }

    fn add_item(menu: &AnyObject, item: &AnyObject) {
        // SAFETY: `menu` is an NSMenu and `item` an NSMenuItem.
        unsafe {
            let _: () = msg_send![menu, addItem: item];
        }
    }

    fn separator() -> Retained<AnyObject> {
        // SAFETY: class method returning an autoreleased NSMenuItem.
        unsafe { msg_send![class!(NSMenuItem), separatorItem] }
    }

    fn item(title: &str, action: Option<Sel>, key: &str, extra: Extra) -> Retained<AnyObject> {
        let title = NSString::from_str(title);
        let key_equivalent = NSString::from_str(key);
        // SAFETY: standard NSMenuItem allocation and designated initializer; a null
        // action is allowed, and the modifier mask is an NSUInteger.
        unsafe {
            let item: Allocated<AnyObject> = msg_send![class!(NSMenuItem), alloc];
            let item: Retained<AnyObject> = msg_send![
                item,
                initWithTitle: &*title,
                action: action,
                keyEquivalent: &*key_equivalent
            ];
            if !key.is_empty() {
                let _: () = msg_send![&*item, setKeyEquivalentModifierMask: modifiers(extra)];
            }
            item
        }
    }

    fn command_item(
        target: &MenuTarget,
        title: &str,
        command: MenuCommand,
        key: &str,
        extra: Extra,
    ) -> Retained<AnyObject> {
        let item = item(title, Some(sel!(runCommand:)), key, extra);
        ITEMS.with(|items| {
            let mut items = items.borrow_mut();
            // SAFETY: NSMenuItem tag and target setters; TARGET keeps the target alive
            // for the app's lifetime because menu items hold their target weakly.
            unsafe {
                let _: () = msg_send![&*item, setTag: items.len() as isize];
                let _: () = msg_send![&*item, setTarget: target];
            }
            items.push((command, item.clone()));
        });
        item
    }

    fn native_item(title: &str, selector: &str, key: &str, extra: Extra) -> Retained<AnyObject> {
        let selector = Sel::register(&std::ffi::CString::new(selector).unwrap());
        item(title, Some(selector), key, extra)
    }

    fn submenu(
        target: &MenuTarget,
        title: &str,
        entries: &[Entry],
        autoenables: bool,
    ) -> (Retained<AnyObject>, Retained<AnyObject>) {
        let menu = new_menu(title);
        // SAFETY: NSMenu setter taking a BOOL.
        unsafe {
            let _: () = msg_send![&*menu, setAutoenablesItems: autoenables];
        }
        for entry in entries {
            match entry {
                Entry::Command(title, command, key, extra) => {
                    add_item(&menu, &command_item(target, title, *command, key, *extra));
                }
                Entry::Native(title, selector, key, extra) => {
                    add_item(&menu, &native_item(title, selector, key, *extra));
                }
                Entry::Separator => add_item(&menu, &separator()),
                Entry::Encodings => {
                    let (parent, encodings) = submenu(target, "Open with Encoding", &[], false);
                    for encoding in Encoding::ALL {
                        add_item(
                            &encodings,
                            &command_item(
                                target,
                                encoding.label(),
                                MenuCommand::OpenWithEncoding(encoding),
                                "",
                                Extra::Plain,
                            ),
                        );
                    }
                    add_item(&menu, &parent);
                }
            }
        }
        let parent = item(title, None, "", Extra::Plain);
        // SAFETY: NSMenuItem submenu setter.
        unsafe {
            let _: () = msg_send![&*parent, setSubmenu: &*menu];
        }
        (parent, menu)
    }

    /// Replaces winit's default menu bar. Call once on the main thread after the app exists.
    pub(in super::super) fn install(ctx: &egui::Context) -> Option<Receiver<MenuCommand>> {
        use Entry::{Command as C, Native as N, Separator as S};
        use Extra::*;
        use MenuCommand as M;
        let mtm = MainThreadMarker::new()?;
        let (sender, receiver) = mpsc::channel();
        *DISPATCH.get_or_init(|| Mutex::new(None)).lock().ok()? = Some((sender, ctx.clone()));
        // SAFETY: MenuTarget subclasses NSObject, whose `init` is its designated initializer.
        let target: Retained<MenuTarget> = unsafe { msg_send![mtm.alloc::<MenuTarget>(), init] };
        let app = NSApplication::sharedApplication(mtm);
        let bar = new_menu("");

        let (app_item, app_menu) = submenu(
            &target,
            "Quarry",
            &[C("About Quarry", M::About, "", Plain), S],
            true,
        );
        let (services_item, services) = submenu(&target, "Services", &[], true);
        add_item(&app_menu, &services_item);
        // SAFETY: NSApplication menu setters each take an NSMenu.
        unsafe {
            let _: () = msg_send![&*app, setServicesMenu: &*services];
        }
        for entry in [
            S,
            N("Hide Quarry", "hide:", "h", Plain),
            N("Hide Others", "hideOtherApplications:", "h", Option),
            N("Show All", "unhideAllApplications:", "", Plain),
            S,
            N("Quit Quarry", "terminate:", "q", Plain),
        ] {
            match entry {
                N(title, selector, key, extra) => {
                    add_item(&app_menu, &native_item(title, selector, key, extra));
                }
                _ => add_item(&app_menu, &separator()),
            }
        }
        add_item(&bar, &app_item);

        let file = [
            C("Open…", M::Open, "o", Plain),
            Entry::Encodings,
            C("Combine Files…", M::CombineFiles, "", Plain),
            C("Join Files…", M::JoinFiles, "", Plain),
            S,
            C("Close Tab", M::CloseTab, "w", Plain),
            S,
            C("Save", M::Save, "s", Plain),
            C("Save As…", M::SaveAs, "s", Shift),
            C("Save As UTF-8…", M::SaveAsUtf8, "", Plain),
            C("Reload from Disk", M::ReloadFromDisk, "", Plain),
            C("Discard Changes", M::DiscardChanges, "", Plain),
            S,
            C("Inspect File…", M::InspectFile, "", Plain),
            C("Temporary Storage…", M::TemporaryStorage, "", Plain),
        ];
        let edit = [
            C("Undo", M::Undo, "z", Plain),
            C("Redo", M::Redo, "z", Shift),
            S,
            C("Cut", M::Cut, "x", Plain),
            C("Copy", M::Copy, "c", Plain),
            C("Paste", M::Paste, "v", Plain),
            C("Select All", M::SelectAll, "a", Plain),
            S,
            C("Find…", M::Find, "f", Plain),
            S,
            C("Insert Rows…", M::InsertRows, "", Plain),
            C("Insert Columns…", M::InsertColumns, "", Plain),
        ];
        let view = [
            C("Show All Whitespace", M::ShowAllWhitespace, "", Plain),
            S,
            C("Columns…", M::Columns, "", Plain),
            C("Filters…", M::Filters, "", Plain),
            S,
            C("Show Next Tab", M::NextTab, "\t", Control),
            C("Show Previous Tab", M::PreviousTab, "\t", ControlShift),
            S,
            N(
                "Enter Full Screen",
                "toggleFullScreen:",
                "f",
                ControlCommand,
            ),
        ];
        let window = [
            N("Minimize", "performMiniaturize:", "m", Plain),
            N("Zoom", "performZoom:", "", Plain),
            S,
            N("Bring All to Front", "arrangeInFront:", "", Plain),
        ];
        let help = [
            C("Quarry User Guide", M::UserGuide, "", Plain),
            C("Report an Issue…", M::ReportIssue, "", Plain),
        ];
        for (title, entries, autoenables) in [
            ("File", &file[..], false),
            ("Edit", &edit[..], false),
            ("View", &view[..], false),
            ("Window", &window[..], true),
            ("Help", &help[..], false),
        ] {
            let (item, menu) = submenu(&target, title, entries, autoenables);
            add_item(&bar, &item);
            // SAFETY: NSApplication menu setters each take an NSMenu.
            unsafe {
                match title {
                    "Window" => {
                        let _: () = msg_send![&*app, setWindowsMenu: &*menu];
                    }
                    "Help" => {
                        let _: () = msg_send![&*app, setHelpMenu: &*menu];
                    }
                    _ => {}
                }
            }
        }
        // SAFETY: NSApplication's main-menu setter takes an NSMenu.
        unsafe {
            let _: () = msg_send![&*app, setMainMenu: &*bar];
        }
        TARGET.with(|slot| *slot.borrow_mut() = Some(target));
        Some(receiver)
    }

    /// Mirrors availability and the whitespace checkmark into the menu bar.
    pub(in super::super) fn sync(state: &MenuState) {
        ITEMS.with(|items| {
            for (command, item) in items.borrow().iter() {
                // SAFETY: NSMenuItem setters taking a BOOL and an NSControlStateValue.
                unsafe {
                    let _: () = msg_send![&**item, setEnabled: state.enabled(*command)];
                    if *command == MenuCommand::ShowAllWhitespace {
                        let _: () =
                            msg_send![&**item, setState: isize::from(state.show_all_whitespace)];
                    }
                }
            }
        });
    }

    /// Plain text from the general pasteboard, for Paste.
    pub(in super::super) fn pasteboard_text() -> Option<String> {
        // "public.utf8-plain-text" is the value of NSPasteboardTypeString.
        let kind = NSString::from_str("public.utf8-plain-text");
        // SAFETY: NSPasteboard class method and `stringForType:`, which returns
        // an NSString or nil.
        let text: Option<Retained<NSString>> = unsafe {
            let pasteboard: Option<Retained<AnyObject>> =
                msg_send![class!(NSPasteboard), generalPasteboard];
            msg_send![&*pasteboard?, stringForType: &*kind]
        };
        text.map(|text| text.to_string())
    }
}

#[cfg(target_os = "macos")]
pub(super) use native::{install, pasteboard_text, sync};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_commands_become_the_shortcut_events() {
        let pressed = |key, modifiers| {
            Some(Routed::Events(vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            }]))
        };
        let none = || None;
        assert_eq!(
            route(MenuCommand::Save, false, none),
            pressed(egui::Key::S, egui::Modifiers::COMMAND)
        );
        assert_eq!(
            route(MenuCommand::Redo, false, none),
            pressed(
                egui::Key::Z,
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT
            )
        );
        assert_eq!(
            route(MenuCommand::Copy, false, none),
            Some(Routed::Events(vec![egui::Event::Copy]))
        );
        assert_eq!(
            route(MenuCommand::Paste, false, || Some("a,b".into())),
            Some(Routed::Events(vec![egui::Event::Paste("a,b".into())]))
        );
        assert_eq!(route(MenuCommand::Paste, false, none), None);
    }

    #[test]
    fn other_commands_become_toolbar_actions() {
        assert_eq!(
            route(MenuCommand::ShowAllWhitespace, true, || None),
            Some(Routed::Action(Action::ShowAllWhitespace(false)))
        );
        assert_eq!(
            route(MenuCommand::OpenWithEncoding(Encoding::Utf8), false, || {
                None
            }),
            Some(Routed::Action(Action::ChooseWithEncoding(Encoding::Utf8)))
        );
        assert_eq!(
            route(MenuCommand::UserGuide, false, || None),
            Some(Routed::Url(USER_GUIDE_URL))
        );
        let state = MenuState {
            disabled: vec![MenuCommand::Open],
            show_all_whitespace: false,
        };
        assert!(!state.enabled(MenuCommand::OpenWithEncoding(Encoding::Utf8)));
        assert!(state.enabled(MenuCommand::Save));
    }
}
