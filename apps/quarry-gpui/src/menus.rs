//! Native macOS application menus, Linux in-window menu bar sync, and platform shortcuts.

use gpui_kit::{App, KeyBinding, Menu, MenuItem};
use quarry_core::Encoding;

gpui_kit::actions!(
    quarry_app,
    [
        NewWindow,
        OpenFile,
        OpenSection,
        OpenUtf8,
        OpenUtf16Le,
        OpenUtf16Be,
        OpenWindows1252,
        OpenIso88591,
        CloseTab,
        CloseWindow,
        MoveTabToNewWindow,
        NextTab,
        PreviousTab,
        ReloadFromDisk,
        SaveFile,
        SaveAsFile,
        SaveAsUtf8,
        ExportFiltered,
        DiscardChanges,
        UndoEdit,
        RedoEdit,
        CopySelection,
        EditCell,
        RenameHeader,
        OpenContextMenu,
        FindText,
        FindNext,
        FindPrevious,
        ReplaceText,
        ToggleBookmark,
        NextBookmark,
        PreviousBookmark,
        ClearBookmarks,
        SortRows,
        FindDuplicates,
        CleanupColumns,
        InsertRows,
        InsertColumns,
        DeleteSelectedRows,
        DeleteSelectedColumns,
        MoveColumns,
        SplitColumn,
        CombineColumns,
        PauseResumeIndexing,
        VerifySectionBoundary,
        ShowAllWhitespace,
        ToggleWordWrap,
        ThemeSystem,
        ThemeLight,
        ThemeDark,
        ThemeOmarchy,
        ToggleColumns,
        ToggleFilters,
        ToggleBookmarks,
        CombineFiles,
        JoinFiles,
        InspectFile,
        TemporaryStorage,
        AboutQuarry,
        QuitApp,
    ]
);

pub const QUARRY_KEY_CONTEXT: &str = "QuarryWindow";

pub fn encoding_from_label(index: usize) -> Option<Encoding> {
    match index {
        0 => None,
        1 => Some(Encoding::Utf8),
        2 => Some(Encoding::Utf16Le),
        3 => Some(Encoding::Utf16Be),
        4 => Some(Encoding::Windows1252),
        5 => Some(Encoding::Latin1),
        _ => None,
    }
}

pub fn encoding_index(encoding: Option<Encoding>) -> usize {
    match encoding {
        None => 0,
        Some(Encoding::Utf8) => 1,
        Some(Encoding::Utf16Le) => 2,
        Some(Encoding::Utf16Be) => 3,
        Some(Encoding::Windows1252) => 4,
        Some(Encoding::Latin1) => 5,
    }
}

pub fn encoding_label(encoding: Encoding) -> &'static str {
    encoding.label()
}

pub fn bind_shortcuts(cx: &mut App) {
    #[cfg(target_os = "macos")]
    let bindings = [
        KeyBinding::new("cmd-n", NewWindow, None),
        KeyBinding::new("cmd-o", OpenFile, None),
        KeyBinding::new("cmd-shift-o", OpenSection, None),
        KeyBinding::new("cmd-w", CloseTab, None),
        KeyBinding::new("cmd-shift-w", CloseWindow, None),
        KeyBinding::new("cmd-s", SaveFile, None),
        KeyBinding::new("cmd-shift-s", SaveAsFile, None),
        KeyBinding::new("cmd-r", ReloadFromDisk, Some(QUARRY_KEY_CONTEXT)),
        KeyBinding::new("cmd-q", QuitApp, None),
        KeyBinding::new("cmd-f", FindText, Some(QUARRY_KEY_CONTEXT)),
        KeyBinding::new("cmd-g", FindNext, Some(QUARRY_KEY_CONTEXT)),
        KeyBinding::new("cmd-shift-g", FindPrevious, Some(QUARRY_KEY_CONTEXT)),
        KeyBinding::new("cmd-alt-f", ReplaceText, Some(QUARRY_KEY_CONTEXT)),
        KeyBinding::new("f2", EditCell, Some(QUARRY_KEY_CONTEXT)),
        KeyBinding::new("shift-f10", OpenContextMenu, Some(QUARRY_KEY_CONTEXT)),
        KeyBinding::new("cmd-z", UndoEdit, Some("QuarryWindow && !Input")),
        KeyBinding::new("cmd-shift-z", RedoEdit, Some("QuarryWindow && !Input")),
        KeyBinding::new("cmd-c", CopySelection, Some("QuarryWindow && !Input")),
        KeyBinding::new("cmd-b", ToggleBookmark, Some("QuarryWindow && !Input")),
        KeyBinding::new("cmd-]", NextBookmark, Some("QuarryWindow && !Input")),
        KeyBinding::new("cmd-[", PreviousBookmark, Some("QuarryWindow && !Input")),
        KeyBinding::new("alt-z", ToggleWordWrap, Some("QuarryWindow && !Input")),
        KeyBinding::new("cmd-shift-]", NextTab, Some(QUARRY_KEY_CONTEXT)),
        KeyBinding::new("cmd-shift-[", PreviousTab, Some(QUARRY_KEY_CONTEXT)),
    ];
    #[cfg(not(target_os = "macos"))]
    let bindings = [
        KeyBinding::new("ctrl-n", NewWindow, None),
        KeyBinding::new("ctrl-o", OpenFile, None),
        KeyBinding::new("ctrl-shift-o", OpenSection, None),
        KeyBinding::new("ctrl-w", CloseTab, None),
        KeyBinding::new("ctrl-shift-w", CloseWindow, None),
        KeyBinding::new("ctrl-s", SaveFile, None),
        KeyBinding::new("ctrl-shift-s", SaveAsFile, None),
        KeyBinding::new("ctrl-r", ReloadFromDisk, Some(QUARRY_KEY_CONTEXT)),
        KeyBinding::new("ctrl-q", QuitApp, None),
        KeyBinding::new("ctrl-f", FindText, Some(QUARRY_KEY_CONTEXT)),
        KeyBinding::new("ctrl-g", FindNext, Some(QUARRY_KEY_CONTEXT)),
        KeyBinding::new("ctrl-shift-g", FindPrevious, Some(QUARRY_KEY_CONTEXT)),
        KeyBinding::new("ctrl-h", ReplaceText, Some(QUARRY_KEY_CONTEXT)),
        KeyBinding::new("f2", EditCell, Some(QUARRY_KEY_CONTEXT)),
        KeyBinding::new("shift-f10", OpenContextMenu, Some(QUARRY_KEY_CONTEXT)),
        KeyBinding::new("ctrl-z", UndoEdit, Some("QuarryWindow && !Input")),
        KeyBinding::new("ctrl-shift-z", RedoEdit, Some("QuarryWindow && !Input")),
        KeyBinding::new("ctrl-c", CopySelection, Some("QuarryWindow && !Input")),
        KeyBinding::new("ctrl-b", ToggleBookmark, Some("QuarryWindow && !Input")),
        KeyBinding::new("ctrl-]", NextBookmark, Some("QuarryWindow && !Input")),
        KeyBinding::new("ctrl-[", PreviousBookmark, Some("QuarryWindow && !Input")),
        KeyBinding::new("alt-z", ToggleWordWrap, Some("QuarryWindow && !Input")),
        KeyBinding::new("ctrl-tab", NextTab, Some(QUARRY_KEY_CONTEXT)),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, Some(QUARRY_KEY_CONTEXT)),
    ];
    cx.bind_keys(bindings);
}

pub fn build_app_menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "Quarry".into(),
            disabled: false,
            items: vec![
                MenuItem::action("About Quarry", AboutQuarry),
                MenuItem::separator(),
                MenuItem::action("New Window", NewWindow),
                MenuItem::separator(),
                MenuItem::action("Quit Quarry", QuitApp),
            ],
        },
        Menu {
            name: "File".into(),
            disabled: false,
            items: vec![
                MenuItem::action("Open…", OpenFile),
                MenuItem::action("Open Section…", OpenSection),
                MenuItem::submenu(Menu {
                    name: "Open with Encoding".into(),
                    disabled: false,
                    items: vec![
                        MenuItem::action("UTF-8…", OpenUtf8),
                        MenuItem::action("UTF-16 LE…", OpenUtf16Le),
                        MenuItem::action("UTF-16 BE…", OpenUtf16Be),
                        MenuItem::action("Windows-1252…", OpenWindows1252),
                        MenuItem::action("ISO-8859-1…", OpenIso88591),
                    ],
                }),
                MenuItem::separator(),
                MenuItem::action("Combine Files…", CombineFiles),
                MenuItem::action("Join Files…", JoinFiles),
                MenuItem::action("Inspect File & Recovery…", InspectFile),
                MenuItem::separator(),
                MenuItem::action("Save", SaveFile),
                MenuItem::action("Save As…", SaveAsFile),
                MenuItem::action("Save As UTF-8…", SaveAsUtf8),
                MenuItem::action("Export Filtered…", ExportFiltered),
                MenuItem::separator(),
                MenuItem::action("Reload from Disk", ReloadFromDisk),
                MenuItem::action("Discard Changes", DiscardChanges),
                MenuItem::separator(),
                MenuItem::action("Temporary Storage…", TemporaryStorage),
                MenuItem::separator(),
                MenuItem::action("Close Tab", CloseTab),
                MenuItem::action("Close Window", CloseWindow),
            ],
        },
        Menu {
            name: "Edit".into(),
            disabled: false,
            items: vec![
                MenuItem::action("Undo", UndoEdit),
                MenuItem::action("Redo", RedoEdit),
                MenuItem::separator(),
                MenuItem::action("Copy Selection", CopySelection),
                MenuItem::action("Edit Selected Cell…", EditCell),
                MenuItem::action("Rename Selected Column Header…", RenameHeader),
                MenuItem::separator(),
                MenuItem::action("Find…", FindText),
                MenuItem::action("Find Next", FindNext),
                MenuItem::action("Find Previous", FindPrevious),
                MenuItem::action("Replace…", ReplaceText),
                MenuItem::separator(),
                MenuItem::action("Toggle Bookmark", ToggleBookmark),
                MenuItem::action("Next Bookmark", NextBookmark),
                MenuItem::action("Previous Bookmark", PreviousBookmark),
                MenuItem::action("Clear Bookmarks", ClearBookmarks),
            ],
        },
        Menu {
            name: "Data".into(),
            disabled: false,
            items: vec![
                MenuItem::action("Sort Rows…", SortRows),
                MenuItem::action("Find & Remove Duplicates…", FindDuplicates),
                MenuItem::action("Clean Selected Columns…", CleanupColumns),
                MenuItem::separator(),
                MenuItem::action("Insert Rows…", InsertRows),
                MenuItem::action("Insert Columns…", InsertColumns),
                MenuItem::action("Delete Selected Rows", DeleteSelectedRows),
                MenuItem::action("Delete Selected Columns", DeleteSelectedColumns),
                MenuItem::separator(),
                MenuItem::action("Move Selected Columns…", MoveColumns),
                MenuItem::action("Split Selected Column…", SplitColumn),
                MenuItem::action("Combine Selected Columns…", CombineColumns),
                MenuItem::separator(),
                MenuItem::action("Pause / Resume Indexing", PauseResumeIndexing),
                MenuItem::action("Verify Section Boundary", VerifySectionBoundary),
            ],
        },
        Menu {
            name: "View".into(),
            disabled: false,
            items: vec![
                MenuItem::action("Show All Whitespace", ShowAllWhitespace),
                MenuItem::action("Word Wrap", ToggleWordWrap),
                MenuItem::separator(),
                MenuItem::submenu(Menu {
                    name: "Theme".into(),
                    disabled: false,
                    items: vec![
                        MenuItem::action("System", ThemeSystem),
                        MenuItem::action("Light", ThemeLight),
                        MenuItem::action("Dark", ThemeDark),
                        MenuItem::action("Omarchy", ThemeOmarchy),
                    ],
                }),
                MenuItem::separator(),
                MenuItem::action("Columns…", ToggleColumns),
                MenuItem::action("Filters…", ToggleFilters),
                MenuItem::action("Bookmarks…", ToggleBookmarks),
            ],
        },
        Menu {
            name: "Window".into(),
            disabled: false,
            items: vec![
                MenuItem::action("New Window", NewWindow),
                MenuItem::action("Move Tab to New Window", MoveTabToNewWindow),
                MenuItem::separator(),
                MenuItem::action("Next Tab", NextTab),
                MenuItem::action("Previous Tab", PreviousTab),
            ],
        },
    ]
}

pub fn install_menus(cx: &mut App) {
    cx.set_menus(build_app_menus());
    let owned = build_app_menus().into_iter().map(Menu::owned).collect();
    gpui_kit::base::GlobalState::global_mut(cx).set_app_menus(owned);
}
