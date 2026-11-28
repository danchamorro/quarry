//! Frontend-neutral display-space grid cursor navigation, opt-in Vim motions,
//! pending-key (`gg`) timeout state machine, VoiceOver Quick Nav non-conflict checks,
//! and atomic Vim preference persistence.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Timeout after pressing `g` before the pending `gg` sequence expires.
pub const VIM_GG_TIMEOUT: Duration = Duration::from_millis(800);

/// Maximum size accepted for the persisted Vim motions preference file (128 bytes).
const MAX_VIM_PREFS_BYTES: u64 = 128;

/// Display-space cursor navigation motion across visible (possibly filtered) rows and shown columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorMotion {
    Up,
    Down,
    Left,
    Right,
    /// Jump to the first shown column in the current row (`Home`, `Cmd/Ctrl+Left`, Vim `0`).
    RowStart,
    /// Jump to the last shown column in the current row (`End`, `Cmd/Ctrl+Right`, Vim `$`).
    RowEnd,
    /// Jump to the first visible row, keeping the current shown column (`Cmd/Ctrl+Up`, `Cmd/Ctrl+Home`, Vim `gg`).
    DocStart,
    /// Jump to the last visible row, keeping the current shown column (`Cmd/Ctrl+Down`, `Cmd/Ctrl+End`, Vim `G`).
    DocEnd,
    /// Move up by `rows` visible rows (`PageUp`).
    PageUp(usize),
    /// Move down by `rows` visible rows (`PageDown`).
    PageDown(usize),
}

/// Steps a 0-based `(display_row, display_col_idx)` cursor within `0..total_rows` and
/// `0..shown_columns` without wrapping/looping at boundaries.
///
/// Returns `None` when the grid has no visible rows or no shown columns.
pub fn step_display_cursor(
    current: Option<(usize, usize)>,
    motion: CursorMotion,
    total_rows: usize,
    shown_columns: usize,
) -> Option<(usize, usize)> {
    if total_rows == 0 || shown_columns == 0 {
        return None;
    }
    let max_row = total_rows - 1;
    let max_col = shown_columns - 1;

    let Some((row, col)) = current.map(|(r, c)| (r.min(max_row), c.min(max_col))) else {
        return Some(match motion {
            CursorMotion::DocEnd => (max_row, 0),
            CursorMotion::RowEnd => (0, max_col),
            CursorMotion::Up
            | CursorMotion::Down
            | CursorMotion::Left
            | CursorMotion::Right
            | CursorMotion::RowStart
            | CursorMotion::DocStart
            | CursorMotion::PageUp(_)
            | CursorMotion::PageDown(_) => (0, 0),
        });
    };

    Some(match motion {
        CursorMotion::Up => (row.saturating_sub(1), col),
        CursorMotion::Down => (row.saturating_add(1).min(max_row), col),
        CursorMotion::Left => (row, col.saturating_sub(1)),
        CursorMotion::Right => (row, col.saturating_add(1).min(max_col)),
        CursorMotion::RowStart => (row, 0),
        CursorMotion::RowEnd => (row, max_col),
        CursorMotion::DocStart => (0, col),
        CursorMotion::DocEnd => (max_row, col),
        CursorMotion::PageUp(step) => (row.saturating_sub(step.max(1)), col),
        CursorMotion::PageDown(step) => (row.saturating_add(step.max(1)).min(max_row), col),
    })
}

/// Moves one cell right (`forward = true`, e.g. `Tab` inside edit mode) or one cell left
/// (`forward = false`, e.g. `Shift+Tab` inside edit mode) in display order.
///
/// Wraps from the last shown column to the first shown column of the next visible row (and
/// vice versa for `Shift+Tab`), clamping cleanly at the very first or last cell of the grid.
pub fn step_adjacent_cell(
    current_row: usize,
    current_col_idx: usize,
    forward: bool,
    total_rows: usize,
    shown_columns: usize,
) -> Option<(usize, usize)> {
    if total_rows == 0 || shown_columns == 0 {
        return None;
    }
    let max_row = total_rows - 1;
    let max_col = shown_columns - 1;
    let row = current_row.min(max_row);
    let col = current_col_idx.min(max_col);

    if forward {
        if col < max_col {
            Some((row, col + 1))
        } else if row < max_row {
            Some((row + 1, 0))
        } else {
            Some((max_row, max_col))
        }
    } else if col > 0 {
        Some((row, col - 1))
    } else if row > 0 {
        Some((row - 1, max_col))
    } else {
        Some((0, 0))
    }
}

/// Frontend-neutral modifier flags for grid keyboard events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GridKeyModifiers {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub platform: bool,
    pub function: bool,
}

/// Returns `true` for the macOS VoiceOver modifier chord (`Ctrl+Option`), which must never be
/// intercepted by single-key grid motions or Quick Filter.
pub const fn is_voiceover_quick_nav_chord(mods: GridKeyModifiers) -> bool {
    mods.control && mods.alt
}

/// Returns `true` when any command, control, alt/option, or function modifier is active, ensuring
/// single-key Vim motions and `/` never conflict with system shortcuts or VoiceOver Quick Nav.
pub const fn conflicts_with_voiceover_or_shortcut(mods: GridKeyModifiers) -> bool {
    mods.control || mods.alt || mods.platform || mods.function
}

/// Outcome of feeding a grid-focused keystroke into [`VimKeyState`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VimAction {
    /// Execute a cursor motion on the focused grid.
    Move(CursorMotion),
    /// Jump to the next Find match (`n`).
    FindNext,
    /// Jump to the previous Find match (`N`).
    FindPrevious,
    /// Entered a pending multi-key sequence (e.g. `"g…"` awaiting a second `g`).
    PendingKey(&'static str),
    /// A pending key sequence was cancelled by Escape or an unrelated key.
    ClearedPending,
    /// Key was not handled as a Vim motion.
    Ignored,
}

/// Pure, frontend-neutral state machine for opt-in grid Vim motions (`hjkl`, `gg`, `G`, `0`, `$`, `n`/`N`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VimKeyState {
    pending_g: bool,
    pending_started_at: Option<Instant>,
    pending_generation: u64,
}

impl VimKeyState {
    /// Returns the visible pending-key indicator (e.g. `"g…"`) while awaiting the second key of `gg`.
    pub const fn pending_indicator(&self) -> Option<&'static str> {
        if self.pending_g { Some("g…") } else { None }
    }

    /// Monotonic generation bumped every time a pending `g` begins, for safe async timeout expiry.
    pub const fn pending_generation(&self) -> u64 {
        self.pending_generation
    }

    /// Clears any pending multi-key sequence. Returns `true` if one was active.
    pub fn cancel(&mut self) -> bool {
        let was_pending = self.pending_g;
        self.pending_g = false;
        self.pending_started_at = None;
        was_pending
    }

    /// Expires a pending `g` sequence if `generation` still matches the active pending key.
    pub fn expire_generation(&mut self, generation: u64) -> bool {
        if self.pending_g && self.pending_generation == generation {
            self.pending_g = false;
            self.pending_started_at = None;
            true
        } else {
            false
        }
    }

    /// Expires a pending `g` sequence if at least [`VIM_GG_TIMEOUT`] has elapsed since `now`.
    pub fn expire_if_elapsed(&mut self, now: Instant) -> bool {
        if self.pending_g
            && self
                .pending_started_at
                .is_some_and(|started| now.saturating_duration_since(started) >= VIM_GG_TIMEOUT)
        {
            self.pending_g = false;
            self.pending_started_at = None;
            true
        } else {
            false
        }
    }

    /// Handles a key press on the focused grid when Vim motions are enabled.
    ///
    /// Never fires when `enabled` is `false`, during IME composition (`is_composing`), or when
    /// `Ctrl`/`Alt`/`Cmd`/`Fn` (including VoiceOver `Ctrl+Option`) modifiers are held.
    pub fn handle_key_at(
        &mut self,
        enabled: bool,
        key: &str,
        key_char: Option<&str>,
        mods: GridKeyModifiers,
        is_composing: bool,
        now: Instant,
    ) -> VimAction {
        if !enabled {
            self.cancel();
            return VimAction::Ignored;
        }
        if is_composing || conflicts_with_voiceover_or_shortcut(mods) {
            return VimAction::Ignored;
        }

        let expired = self.expire_if_elapsed(now);

        let is_lower_g = !mods.shift && key == "g" && key_char.is_none_or(|c| c == "g");
        let is_upper_g =
            (mods.shift && key.eq_ignore_ascii_case("g")) || key == "G" || key_char == Some("G");
        let is_dollar = key == "$" || key_char == Some("$") || (mods.shift && key == "4");
        let is_zero = !mods.shift && (key == "0" || key_char == Some("0"));
        let is_lower_n = !mods.shift && key == "n" && key_char.is_none_or(|c| c == "n");
        let is_upper_n =
            (mods.shift && key.eq_ignore_ascii_case("n")) || key == "N" || key_char == Some("N");

        if is_lower_g {
            if self.pending_g {
                self.pending_g = false;
                self.pending_started_at = None;
                return VimAction::Move(CursorMotion::DocStart);
            }
            self.pending_g = true;
            self.pending_started_at = Some(now);
            self.pending_generation = self.pending_generation.wrapping_add(1);
            return VimAction::PendingKey("g…");
        }

        let had_pending = self.cancel() || expired;

        if !mods.shift {
            match key {
                "h" if key_char.is_none_or(|c| c == "h") => {
                    return VimAction::Move(CursorMotion::Left);
                }
                "j" if key_char.is_none_or(|c| c == "j") => {
                    return VimAction::Move(CursorMotion::Down);
                }
                "k" if key_char.is_none_or(|c| c == "k") => {
                    return VimAction::Move(CursorMotion::Up);
                }
                "l" if key_char.is_none_or(|c| c == "l") => {
                    return VimAction::Move(CursorMotion::Right);
                }
                _ => {}
            }
        }

        if is_zero {
            return VimAction::Move(CursorMotion::RowStart);
        }
        if is_dollar {
            return VimAction::Move(CursorMotion::RowEnd);
        }
        if is_upper_g {
            return VimAction::Move(CursorMotion::DocEnd);
        }
        if is_lower_n {
            return VimAction::FindNext;
        }
        if is_upper_n {
            return VimAction::FindPrevious;
        }

        if had_pending {
            VimAction::ClearedPending
        } else {
            VimAction::Ignored
        }
    }
}

/// Ignore unusable environment roots instead of writing preferences beneath the working directory.
pub fn unix_vim_preferences_path(
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
        .map(|root| root.join("quarry/vim-motions-preference"))
}

/// Persisted opt-in Vim motions preference (strictly off by default).
#[derive(Debug, Clone, Default)]
pub struct VimMotionPreferences {
    pub enabled: bool,
    path: Option<PathBuf>,
}

impl VimMotionPreferences {
    /// Loads the preference from an explicit path (defaults to `enabled: false`).
    pub fn load(path: Option<PathBuf>) -> Self {
        let mut prefs = Self {
            enabled: false,
            path,
        };
        if let Some(pref_path) = prefs.path.as_deref()
            && let Ok(file) = quarry_core::open_regular_file_nofollow(pref_path)
        {
            let mut raw = String::new();
            if file
                .take(MAX_VIM_PREFS_BYTES + 1)
                .read_to_string(&mut raw)
                .is_ok()
                && raw.len() as u64 <= MAX_VIM_PREFS_BYTES
                && let Some(enabled) = parse_vim_prefs(&raw)
            {
                prefs.enabled = enabled;
            }
        }
        prefs
    }

    /// Loads application-wide Vim motion preference from the standard per-user config directory.
    pub fn application() -> Self {
        let path = if cfg!(target_os = "macos") {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|root| root.is_absolute())
                .map(|root| root.join("Library/Application Support/Quarry/vim-motions-preference"))
        } else if cfg!(target_os = "windows") {
            std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .filter(|root| root.is_absolute())
                .map(|root| root.join("Quarry/vim-motions-preference"))
        } else {
            unix_vim_preferences_path(
                std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
                std::env::var_os("HOME").map(PathBuf::from),
            )
        };
        Self::load(path)
    }

    /// Returns the configured preference file path when persistence is enabled.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Updates the in-memory toggle and persists it atomically with private (`0600`) permissions.
    pub fn set_enabled(&mut self, enabled: bool) -> io::Result<()> {
        self.enabled = enabled;
        self.persist_current()
    }

    fn persist_current(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("Invalid Vim motions preference location"))?;
        std::fs::create_dir_all(parent)?;
        let encoded = format!("vim_motions={}\n", if self.enabled { 1 } else { 0 });
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        quarry_core::restrict_private_file(file.path(), file.as_file())?;
        file.write_all(encoded.as_bytes())?;
        file.flush()?;
        file.persist(path).map_err(|e| e.error)?;
        Ok(())
    }
}

fn parse_vim_prefs(content: &str) -> Option<bool> {
    let mut enabled = None;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once('=')?;
        if key == "vim_motions" {
            match value {
                "0" => enabled = Some(false),
                "1" => enabled = Some(true),
                _ => return None,
            }
        }
    }
    enabled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_display_cursor_clamps_without_wrapping() {
        assert_eq!(step_display_cursor(None, CursorMotion::Down, 0, 3), None);
        assert_eq!(step_display_cursor(None, CursorMotion::Down, 5, 0), None);
        assert_eq!(
            step_display_cursor(None, CursorMotion::Down, 5, 3),
            Some((0, 0))
        );
        assert_eq!(
            step_display_cursor(Some((0, 0)), CursorMotion::Up, 5, 3),
            Some((0, 0))
        );
        assert_eq!(
            step_display_cursor(Some((0, 0)), CursorMotion::Left, 5, 3),
            Some((0, 0))
        );
        assert_eq!(
            step_display_cursor(Some((4, 2)), CursorMotion::Down, 5, 3),
            Some((4, 2))
        );
        assert_eq!(
            step_display_cursor(Some((4, 2)), CursorMotion::Right, 5, 3),
            Some((4, 2))
        );
        assert_eq!(
            step_display_cursor(Some((2, 1)), CursorMotion::RowStart, 10, 4),
            Some((2, 0))
        );
        assert_eq!(
            step_display_cursor(Some((2, 1)), CursorMotion::RowEnd, 10, 4),
            Some((2, 3))
        );
        assert_eq!(
            step_display_cursor(Some((2, 1)), CursorMotion::DocStart, 10, 4),
            Some((0, 1))
        );
        assert_eq!(
            step_display_cursor(Some((2, 1)), CursorMotion::DocEnd, 10, 4),
            Some((9, 1))
        );
        assert_eq!(
            step_display_cursor(Some((1, 2)), CursorMotion::PageDown(4), 10, 4),
            Some((5, 2))
        );
        assert_eq!(
            step_display_cursor(Some((5, 2)), CursorMotion::PageUp(10), 10, 4),
            Some((0, 2))
        );
    }

    #[test]
    fn step_adjacent_cell_moves_right_and_wraps_rows() {
        assert_eq!(step_adjacent_cell(0, 0, true, 3, 3), Some((0, 1)));
        assert_eq!(step_adjacent_cell(0, 2, true, 3, 3), Some((1, 0)));
        assert_eq!(step_adjacent_cell(2, 2, true, 3, 3), Some((2, 2)));
        assert_eq!(step_adjacent_cell(1, 0, false, 3, 3), Some((0, 2)));
        assert_eq!(step_adjacent_cell(0, 0, false, 3, 3), Some((0, 0)));
    }

    #[test]
    fn vim_state_is_off_by_default_and_handles_motions_and_gg_timeout() {
        let mut state = VimKeyState::default();
        let t0 = Instant::now();
        let none = GridKeyModifiers::default();

        // Off by default: ignores everything.
        assert_eq!(
            state.handle_key_at(false, "j", Some("j"), none, false, t0),
            VimAction::Ignored
        );
        assert_eq!(state.pending_indicator(), None);

        // Basic hjkl, 0, $, G, n, N.
        assert_eq!(
            state.handle_key_at(true, "h", Some("h"), none, false, t0),
            VimAction::Move(CursorMotion::Left)
        );
        assert_eq!(
            state.handle_key_at(true, "j", Some("j"), none, false, t0),
            VimAction::Move(CursorMotion::Down)
        );
        assert_eq!(
            state.handle_key_at(true, "k", Some("k"), none, false, t0),
            VimAction::Move(CursorMotion::Up)
        );
        assert_eq!(
            state.handle_key_at(true, "l", Some("l"), none, false, t0),
            VimAction::Move(CursorMotion::Right)
        );
        assert_eq!(
            state.handle_key_at(true, "0", Some("0"), none, false, t0),
            VimAction::Move(CursorMotion::RowStart)
        );
        let shift = GridKeyModifiers {
            shift: true,
            ..none
        };
        assert_eq!(
            state.handle_key_at(true, "4", Some("$"), shift, false, t0),
            VimAction::Move(CursorMotion::RowEnd)
        );
        assert_eq!(
            state.handle_key_at(true, "g", Some("G"), shift, false, t0),
            VimAction::Move(CursorMotion::DocEnd)
        );
        assert_eq!(
            state.handle_key_at(true, "n", Some("n"), none, false, t0),
            VimAction::FindNext
        );
        assert_eq!(
            state.handle_key_at(true, "n", Some("N"), shift, false, t0),
            VimAction::FindPrevious
        );

        // gg within timeout moves to DocStart.
        assert_eq!(
            state.handle_key_at(true, "g", Some("g"), none, false, t0),
            VimAction::PendingKey("g…")
        );
        assert_eq!(state.pending_indicator(), Some("g…"));
        assert_eq!(
            state.handle_key_at(
                true,
                "g",
                Some("g"),
                none,
                false,
                t0 + Duration::from_millis(200)
            ),
            VimAction::Move(CursorMotion::DocStart)
        );
        assert_eq!(state.pending_indicator(), None);

        // g followed by timeout expires and next g starts a fresh pending g.
        assert_eq!(
            state.handle_key_at(true, "g", Some("g"), none, false, t0),
            VimAction::PendingKey("g…")
        );
        assert!(state.expire_if_elapsed(t0 + VIM_GG_TIMEOUT));
        assert_eq!(state.pending_indicator(), None);
        assert_eq!(
            state.handle_key_at(
                true,
                "g",
                Some("g"),
                none,
                false,
                t0 + VIM_GG_TIMEOUT + Duration::from_millis(10)
            ),
            VimAction::PendingKey("g…")
        );
        // Unrelated key clears pending g.
        assert_eq!(
            state.handle_key_at(true, "escape", None, none, false, t0 + VIM_GG_TIMEOUT),
            VimAction::ClearedPending
        );
        assert_eq!(state.pending_indicator(), None);
    }

    #[test]
    fn vim_state_never_fires_under_ime_or_voiceover_quick_nav_modifiers() {
        let mut state = VimKeyState::default();
        let t0 = Instant::now();
        let none = GridKeyModifiers::default();

        // IME composition active.
        assert_eq!(
            state.handle_key_at(true, "j", Some("j"), none, true, t0),
            VimAction::Ignored
        );

        // VoiceOver Ctrl+Option chord.
        let vo = GridKeyModifiers {
            control: true,
            alt: true,
            ..none
        };
        assert!(is_voiceover_quick_nav_chord(vo));
        assert!(conflicts_with_voiceover_or_shortcut(vo));
        assert_eq!(
            state.handle_key_at(true, "h", Some("h"), vo, false, t0),
            VimAction::Ignored
        );
        assert_eq!(
            state.handle_key_at(true, "l", Some("l"), vo, false, t0),
            VimAction::Ignored
        );

        // Platform/Cmd shortcut chord.
        let cmd = GridKeyModifiers {
            platform: true,
            ..none
        };
        assert_eq!(
            state.handle_key_at(true, "k", None, cmd, false, t0),
            VimAction::Ignored
        );
    }

    #[test]
    fn vim_preferences_default_off_and_persist_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vim-motions-preference");
        let mut prefs = VimMotionPreferences::load(Some(path.clone()));
        assert!(!prefs.enabled);

        prefs.set_enabled(true).unwrap();
        let reloaded = VimMotionPreferences::load(Some(path.clone()));
        assert!(reloaded.enabled);

        prefs.set_enabled(false).unwrap();
        let reloaded_off = VimMotionPreferences::load(Some(path));
        assert!(!reloaded_off.enabled);
    }

    #[test]
    fn vim_gg_expiration_reports_cleared_pending_on_late_unrecognized_key() {
        let mut state = VimKeyState::default();
        let none = GridKeyModifiers::default();
        let t0 = Instant::now();
        let after = t0 + VIM_GG_TIMEOUT + Duration::from_millis(5);

        assert_eq!(
            state.handle_key_at(true, "g", Some("g"), none, false, t0),
            VimAction::PendingKey("g…")
        );
        // Late unrecognized key after deadline must report ClearedPending so the UI repaints.
        assert_eq!(
            state.handle_key_at(true, "x", Some("x"), none, false, after),
            VimAction::ClearedPending
        );
        assert_eq!(state.pending_indicator(), None);

        // Late `g` after deadline starts a new pending sequence instead of DocStart.
        assert_eq!(
            state.handle_key_at(true, "g", Some("g"), none, false, t0),
            VimAction::PendingKey("g…")
        );
        assert_eq!(
            state.handle_key_at(true, "g", Some("g"), none, false, after),
            VimAction::PendingKey("g…")
        );

        // Late recognized motion after deadline still executes the motion.
        assert_eq!(
            state.handle_key_at(true, "j", Some("j"), none, false, after + VIM_GG_TIMEOUT),
            VimAction::Move(CursorMotion::Down)
        );
    }
}
