# Quarry Design Guide

This guide covers how Quarry looks and behaves on screen. Follow it when you add
or change any UI in `apps/quarry-egui`. It explains where things go and which
shared pieces to use, so new features look like they belong.

The code is the source of truth for exact values:

| File | Owns |
|---|---|
| `src/theme.rs` | Palettes, fonts, widget styling, shared components |
| `src/icons.rs` | Toolbar icons, drawn with the painter |
| `src/menu.rs` | The native macOS menu bar |
| `src/main.rs` | Toolbar, grid, status bar, notices, dialogs |
| `src/tabs.rs` | The document tab strip |

## Principles

1. **The data is the interface.** The grid gets the most space and the most
   contrast. Chrome (toolbar, tabs, status bar) stays quiet and neutral.
2. **Native first.** Quarry should feel like a Mac app: system fonts, a toolbar
   in the title bar, a real menu bar, standard shortcuts, and light and dark
   appearances.
3. **Amber means "this one."** The brand amber marks selection, focus, the
   active filter, unsaved work, and the single primary action in a dialog.
   Don't use it for decoration.
4. **One way to do each thing.** Use the shared components below. Don't style a
   button, frame, or color inline.
5. **Calm under load.** Progress, cancel, and status messages live in the status
   bar. Only warnings and errors interrupt, as a banner.

## Color

Every color comes from `theme::Palette`. Get the palette for the current
appearance and use its semantic names:

```rust
let palette = theme::ui_palette(ui);      // inside a Ui
let palette = theme::ctx_palette(ctx);    // with only a Context
ui.colored_label(palette.text_secondary, "Detected: Comma");
```

**Never write `Color32::from_rgb(...)` outside `theme.rs`.** A hard-coded color
breaks dark mode. If no token fits, add one to `Palette`, with values in both
`LIGHT` and `DARK`.

| Token | Use for |
|---|---|
| `chrome` | Toolbar, tab strip, status bar, find bar |
| `separator` | Hairlines between chrome and content |
| `canvas` | Grid and dialog backgrounds |
| `zebra` | Alternate grid rows; soft inset panels inside dialogs |
| `grid_line` | Lines between cells |
| `header_fill` | Column header and row-number gutter; active tab |
| `text`, `text_secondary`, `text_tertiary` | Primary text, supporting text, row and column numbers and hints |
| `hover`, `pressed` | Fills for borderless controls |
| `control`, `control_border` | Framed buttons, fields, and combo boxes |
| `selection` | Selected cells, rows, columns, and segments (soft amber) |
| `selection_strong` | Focus ring, selected column number, active-filter outline (amber) |
| `on_accent` | Text on amber fills |
| `accent_text` | Unsaved marker, links, amber text on neutral backgrounds |
| `error_text` / `error_fill`, `warning_text` / `warning_fill` | Banners and inline validation |
| `success_text` | Completed states, such as "Index complete" |
| `mark` | Whitespace marks inside cells |

egui's `ui.visuals().error_fg_color` and `warn_fg_color` map to the same tokens,
so `ui.colored_label(ui.visuals().error_fg_color, message)` is fine for inline
errors.

## Typography

Quarry loads the macOS system faces at startup (`theme::install_fonts`): SF Pro
for text and SF Mono for monospace. On other platforms, or if the files are
missing, egui's bundled fonts are used.

| Style | Size | Use for |
|---|---|---|
| Heading | 17 | Dialog and popover titles |
| Body / Button | 13 | Everything else (the macOS default) |
| Small | 11–11.5 | Status bar, captions |
| Section label (`theme::section_label`) | 12, secondary | Labels above groups of fields |
| Grid text (`theme::grid_font`) | 13 proportional | Cell and header text |
| Grid numbers (`theme::grid_number_font`) | 12.5 mono | Numeric cells, so digits line up |

egui can't render font weights, so build hierarchy from size, color, and
spacing, not bold. Write labels in sentence case ("Sort rows", "Match case").
Menu items and buttons that open another step end with an ellipsis ("Save As…").

## Layout and sizes

| Element | Value |
|---|---|
| Toolbar (shares the title bar) | 38 px; leaves 72 px for the traffic lights |
| Tab strip | 34 px |
| Status bar | 26 px |
| Grid row | 22 px |
| Header | 18 px column-number row + 24 px name row |
| Cell padding | 8 px each side |
| Control height | 24 px in chrome, 28 px in dialogs |
| Corner radius | 6 controls, 10 popovers and windows, 12 modal dialogs |
| Spacing | 8 × 6 by default; 10 × 10 inside dialogs |

The layout must work from the minimum window size, 860 × 540. Below 1,000 px
wide, the toolbar switches to compact mode: icon-only buttons and a shorter
Format label. Test both sizes.

## Window structure

From top to bottom:

1. **Toolbar**, in the title bar. Left to right: File menu, Format, View | Undo,
   Redo | Insert, Columns, Filters, Find. Row jump and paging sit at the far
   right. Dragging empty toolbar space moves the window; double-clicking zooms
   it (`titlebar_drag_area`).
2. **Tab strip**, shown only when a document is open. The active tab uses
   `header_fill` so it joins the grid header below.
3. **Notice banner**, for warnings and errors only. One line: severity mark,
   message, then actions on the right (`notice_strip_with_details`).
4. **Find bar**, when open.
5. **Grid**.
6. **Status bar.** Left: unsaved marker, row range, column count, size,
   selection. Right: index status with a colored dot, or the active job's
   progress and Cancel button.

## Where a new feature goes

| The feature is… | Put it in… |
|---|---|
| Used on most files, most sessions | A toolbar button, plus a menu bar item |
| A file-level command | The **File** menu (toolbar and menu bar) |
| About the selected cell, row, or column | The grid's context menu |
| A setting with a few options | A popover from the toolbar, like Format or Filters |
| A multi-step or risky operation | A modal dialog |
| A long-running job | Start it from a dialog; show progress and Cancel in the status bar |
| A passing confirmation ("Saved") | The status bar's status text |
| A problem the user must notice | A notice banner |

Every command that has a menu-bar equivalent must appear in `menu.rs` (see
below). Don't add a second toolbar row.

## Components

Use these instead of building your own.

### Buttons

- **Primary:** `theme::primary_button("Sort")`. Amber fill, dark text, in both
  appearances. Use at most one per dialog, for the action that finishes it.
- **Secondary:** `ui.button("Cancel")`. Framed, neutral.
- **Destructive:** a framed button with `palette.error_text` text, placed at the
  far left of the footer ("Discard Changes and Close").
- **Toolbar:** `icons::button(ui, enabled, Icon::Filter, "Filters…", show_label)`.
  The label is always the accessible name, even when only the icon shows.
  Call `theme::chrome_controls(ui.style_mut())` in any chrome surface so buttons
  are borderless until hovered.

### Dialogs

All modals use the shared frame and spacing:

```rust
egui::Modal::new(id)
    .frame(tool_dialog_frame(ctx))
    .show(ctx, |ui| {
        tool_dialog_style(ui.style_mut());
        ui.set_width(480.0);
        ui.heading("Sort Rows");
        ui.weak("One sentence on what this does.");
        // fields…
        ui.separator();
        // footer: Cancel on the left, the primary action last, on the right
    });
```

- Movable windows (Join Files, Inspect File) use `egui::Window` with
  `.collapsible(false).frame(tool_dialog_frame(ctx))` and `tool_dialog_style`.
- Toolbar popovers (Filters, Columns, Format) use `compact_tool_frame` and
  `compact_tool_style`, with `compact_tool_placement` to hang under their
  button.
- Put longer explanations behind a collapsed **Details** header, not in the
  main text.
- Show why an action is disabled with `.on_disabled_hover_text(...)`.

### Choices

- Two or three exclusive options in a row: `theme::segmented(ui, &mut value, &options)`.
- Four or more, or long labels: `ui.radio_value` in a vertical list.
- Many options, or options that need little space: `egui::ComboBox`.
- On/off: `ui.checkbox`.

### Grid

- Text aligns left; values that `looks_numeric` accepts align right in the
  number font. Keep that check cheap, since it runs for every visible cell.
- Cells are square and borderless. Hover is a faint tint, selection is
  `selection`, and the focused cell gets a `selection_strong` ring.
- Keep 100-row viewport reads and `MAX_RENDERED_COLUMNS`. Visual changes must
  not add per-row work that grows with file size.

### Icons

Add an `Icon` variant in `icons.rs` and draw it with strokes on a 0–1 grid,
using the stroke width already computed there. Icons are 14 px, one color, and
follow the text color. Don't add image assets for UI icons.

## Native menu bar

`menu.rs` builds the macOS menu bar. Commands reach the app in two ways:

- A command with a standard shortcut (⌘S, ⌘Z, ⌘C, ⌘F…) becomes the same egui
  event that shortcut produces (`menu::route`). The existing keyboard handling
  then decides what happens, including text-field undo and copy.
- Any other command becomes an `Action` that passes through the same gate as a
  toolbar click.

To add a command: add a `MenuCommand` variant, route it in `route`, place it
in the right menu in `install`, and add its availability in
`QuarryApp::menu_state`. File command availability comes from `FileCommands`,
so the toolbar File menu and the menu bar always agree.

Call AppKit APIs that the enabled `objc2-app-kit` features don't cover through
`msg_send!`, with a `SAFETY` comment, as `menu.rs` does. Adding features changes
the workspace manifests, which makes the audited third-party notice inventory
stale (`./scripts/macos-app.sh self-test` fails). Regenerating and reviewing
that inventory needs the owner's approval.

## Accessibility

VoiceOver regressions are release blockers ([ADR 0003](adr/0003-select-egui-ui.md)).

- Every interactive control needs an accessible name. Icon-only controls get
  one through `icons::button`, or through `widget_info` when built by hand.
- Keep left-to-right creation order equal to visual order, so Tab focus moves
  predictably. Don't use `right_to_left` layouts for focusable controls;
  reserve the space and lay them out left to right (see the toolbar's row
  jump).
- Don't rely on color alone. The unsaved state is a dot and text; the active
  filter is an outline and a count.
- Text and background tokens are chosen to stay readable in both appearances.
  Keep that true when you add tokens.

## Checking your change

1. `cargo test -p quarry-egui`. Layout tests check single-row toolbars, bounds
   at 860 and 1,280 px, and accessible labels. If a test pins a value you
   meant to change, update the test and keep what it checks.
2. `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`.
3. Run the app and look at the change in both light and dark appearances, at
   the minimum window size and at full screen. Quarry follows the system
   appearance live.
4. Update `docs/USER_GUIDE.md` when a visible label, location, or shortcut
   changes.
