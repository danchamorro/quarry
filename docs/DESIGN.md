# Quarry Design System

Quarry is a calm, high-density desktop tool for inspecting and editing massive delimited files on **macOS** and **Linux / Omarchy**. The data grid is the centerpiece; chrome stays quiet so numbers, text, and selection stand out immediately.

## Architecture and Source of Truth

| Layer | Location | Responsibility |
|---|---|---|
| **Design tokens & palettes** | [`crates/quarry-workbench/src/theme.rs`](../crates/quarry-workbench/src/theme.rs) | Frontend-neutral RGBA semantic palettes (**Light**, WCAG AA **Dark**, and derived **Omarchy** palettes), contrast guards, theme preference persistence, and numeric-alignment classification. |
| **GPUI theme bridge, window & dialogs** | [`apps/quarry-gpui/src/main.rs`](../apps/quarry-gpui/src/main.rs), `panels.rs`, `operations.rs`, `section_modal.rs`, `workflows_ui.rs`, `find_bar.rs` | Maps `quarry_workbench::theme::Palette` onto `gpui-kit` tokens and renders Quarry's window chrome, toolbar, popovers, modals, banners, and find/replace controls. |
| **Menus & shortcuts** | [`apps/quarry-gpui/src/menus.rs`](../apps/quarry-gpui/src/menus.rs) | Native macOS application menu bar, Linux in-window `AppMenuBar`, and platform keybindings (`Command` on macOS, `Ctrl` on Linux). |
| **Workbench shell & GPU grid** | [`apps/quarry-gpui/src/workbench.rs`](../apps/quarry-gpui/src/workbench.rs), [`apps/quarry-gpui/src/grid.rs`](../apps/quarry-gpui/src/grid.rs) | Multi-window GPUI tab/document orchestration, virtualized GPU grid, `#` row gutter (selection + bookmark dots), status bar, and Live Resource HUD. |
| **Frontend ADR** | [ADR 0013](adr/0013-adopt-gpui-desktop-frontend.md) | Adopts GPUI (`gpui-kit`) for macOS and Linux/Omarchy, superseding the historical egui bake-off ([ADR 0003](adr/0003-select-egui-ui.md)). |

Never hardcode ad-hoc RGB colors in views or dialogs. Always read semantic roles from `Palette` so every control renders legibly across **Light**, **Dark**, **System**, and **Omarchy** themes.

---

## Color System

Quarry ships four theme modes under **View → Theme**:

1. **System** — follows macOS or Linux dark/light appearance automatically.
2. **Light** — warm paper neutrals with brand amber (`#E9C46A`) reserved for selection, focus, bookmarks, and primary accents.
3. **Dark** — deep slate charcoal surfaces (`#131417` canvas, `#1A1B1E` chrome) engineered for WCAG AA contrast with brand amber accents.
4. **Omarchy** — hardened live local theme sync on Linux/Omarchy (reading `~/.config/omarchy`, `~/.local/share/omarchy`, `~/.local/state/omarchy` within a strict symlink allowlist and 64 KiB file cap; falls back to Dark if unavailable).

### Semantic Palette Roles (`quarry_workbench::theme::Palette`)

| Role | Light (`rgb`) | Dark (`rgb`) | Usage |
|---|---|---|---|
| `chrome` | `246, 245, 243` | `26, 27, 30` | Title bar, tab bar, toolbar, status bar |
| `separator` | `222, 220, 216` | `52, 54, 60` | Hairlines separating chrome bands and panels |
| `canvas` | `255, 255, 255` | `19, 20, 23` | Grid cells, modals, and popover backgrounds |
| `zebra` | `250, 249, 247` | `24, 25, 29` | Alternating grid rows |
| `grid_line` | `236, 234, 230` | `37, 39, 44` | Subtle cell gridlines |
| `header_fill` | `248, 247, 245` | `29, 30, 34` | Column header band and row-number gutter |
| `text` | `29, 29, 27` | `236, 235, 231` | Primary cell and UI copy |
| `text_secondary` | `104, 102, 97` | `168, 166, 160` | Column headers, status metadata, helper text |
| `text_tertiary` | `150, 147, 141` | `122, 120, 115` | Placeholders, disabled controls |
| `hover` / `pressed` | `232,230,226` / `220,218,213` | `39,41,46` / `49,51,58` | Borderless toolbar and menu interaction states |
| `control` / `control_border` | `255,255,255` / `211,209,204` | `31,33,38` / `66,69,77` | Inputs, dropdown triggers, secondary buttons |
| `selection` | `252, 238, 200` | `66, 54, 27` | Selected cells, rows, and columns |
| `selection_strong` | `233, 196, 106` (`ACCENT`) | `233, 196, 106` (`ACCENT`) | Primary action fills, focus rings, selected header accents |
| `on_accent` | `43, 33, 10` | `33, 25, 8` | High-contrast label text rendered over `selection_strong` |
| `accent_text` | `138, 96, 10` | `239, 206, 122` | Row numbers, bookmark markers, unsaved indicators, links |
| `error_text` / `error_fill` | `178,45,35` / `253,236,233` | `255,146,138` / `64,25,23` | Error banners, invalid inputs, destructive confirmation |
| `warning_text` / `warning_fill` | `128,84,0` / `255,245,219` | `245,201,102` / `59,44,16` | Speculative section trust badges, storage/conflict warnings |
| `success_text` | `29, 120, 74` | `110, 214, 152` | Verified section badges, completion confirmation |
| `mark` | `150, 147, 141` | `122, 120, 115` | Inline whitespace dots (`·`) and `[TAB]`/`[LF]` tokens |
| `indicator_fill` / `indicator_stroke` | `111,145,150` / `57,77,84` | `124,168,174` / `184,216,220` | Clipped-whitespace corner indicator |

All semantic text/fill pairs are validated against WCAG 2.1 AA contrast ratios in unit tests inside `crates/quarry-workbench/src/theme.rs`.

---

## Typography

- **UI and grid text:** Native platform text shaping via GPUI (`CoreText` on macOS, `cosmic-text` / FreeType on Linux). Default grid cell font size is **`13.0 pt`** (`GRID_FONT_SIZE`).
- **Tabular numbers (`tnum`):** Numeric values and row numbers render with OpenType `tnum` tabular figures so digits align vertically down every column. Columns classified as numeric right-align automatically for fast visual scanning.
- **Hierarchy:** Dialog titles, active tab labels, and primary actions use clear weight and contrast separation; secondary metadata (row counts, file sizes, encoding, Live Resource HUD) uses `text_secondary` at compact caption sizes so it never competes with cell content.

---

## Layout and Chrome Structure

A Quarry window is organized into five clean horizontal bands separated by 1 px `separator` hairlines:

1. **Menu bar:** Native macOS system menu bar on macOS; compact in-window `AppMenuBar` (`Quarry`, `File`, `Edit`, `Data`, `View`, `Window`) at the top of each window on Linux.
2. **Tab bar:** Compact document tabs with unsaved markers, close buttons, `Move to Window`, and `+ Window` actions. Supports up to 200 open tabs across multiple windows with automatic background dormancy.
3. **Toolbar:** Single-row, low-noise action bar containing the active file/section badge (`Verified` / `Speculative` for bounded sections), `Format`, `Row` jump input + `Go`, `Undo`/`Redo`, `Columns`, `Filters`, `Bookmarks`, `Find`, `Sort`, `Duplicates`, `Clean`, `Whitespace`, and right-aligned `Save` / `Save As…`. Borderless controls sit flat on `chrome` and reveal `hover`/`pressed` fills on interaction.
4. **Main grid canvas:** Sticky numbered column ruler + editable header row, `#` row-number gutter (with amber/accent bookmark dots and range selection), and a GPU-virtualized viewport painting only visible rows and columns. Inline `Find` / `Replace` strips and dismissible status notices dock cleanly above or below the grid without shifting column layout unexpectedly.
5. **Status bar & Live Resource HUD:** Left side shows row count (or filtered match count), file or section size, visible/total columns, and encoding. Right side shows indexing state (with Pause/Resume or Cancel controls while active), live memory (`phys_footprint` on macOS, `VmRSS` on Linux), free disk space on the active volume, and open/active tab counts.

### Geometry Constants

- `CONTROL_RADIUS = 4` — subtle rounding on inputs, buttons, and checkboxes (keeps checkboxes unmistakably square).
- `WINDOW_RADIUS = 10` — outer corner radius for floating popovers and modal dialogs.

---

## Shared Components & Dialog Patterns

- **Floating popovers (`Columns`, `Filters`, `Bookmarks`):** Anchored cleanly below their toolbar button (`DialogAnchor`), keyboard-dismissible (`Esc`), and non-blocking where appropriate so the grid stays visible behind them.
- **Modal dialogs (`Open Section…`, `Sort Rows…`, `Find & Remove Duplicates…`, `Clean Selected Columns…`, `Combine Files…`, `Join Files…`, `Inspect File & Recovery…`, `Temporary Storage…`, `About Quarry`):** Clear title, concise helper copy, inline validation before execution, explicit storage-allowance preflight for large rewrites (`>= 256 MiB`), and right-aligned action buttons (`Cancel` + primary action).
- **Direct manipulation & menus:** All file, edit, data, view, and window actions are accessible through the top menu bar (`Quarry`, `File`, `Edit`, `Data`, `View`, `Window`), toolbar buttons, and keyboard shortcuts. Right-clicking a row number in the `#` gutter directly toggles a session row bookmark on that row.

---

## Accessibility

- **macOS (`accesskit_macos`):** Every window maintains its own AccessKit tree with labeled controls, grid cells, dialogs, and state-transition announcements (`Role::Status` / `Role::Alert`).
- **Linux (`accesskit_unix` / AT-SPI):** Exposes the same semantic tree over the session AT-SPI bus on Wayland/Hyprland and standard Linux desktops.
- **Quiet telemetry:** The Live Resource HUD updates off-thread (`<= 2 Hz`) inside a non-live `Role::Group` so screen readers are never flooded by periodic memory/disk ticks; only real state transitions (indexing complete, errors, bookmark updates, operation completion) emit live announcements.
