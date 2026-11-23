//! Frontend-neutral view-only whitespace inspection cache, analysis, and preferences.
//!
//! Raw cell bytes remain authoritative for all data operations.

use quarry_core::{FilterMatch, Row, Session};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, Read, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

const MAX_CACHE_CELLS: usize = 16_384;
const MAX_BATCH_CELLS: usize = 128;
const MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;
// Alternating marked and unmarked characters must not turn one bounded preview
// into thousands of shaping sections. The final slots cover the current run,
// an unstyled remainder, and a truncation marker.
pub const MAX_LAYOUT_SECTIONS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CellId {
    pub row: Option<u64>,
    pub column: usize,
}

#[derive(Clone)]
pub enum Input {
    Header(Arc<Session>, usize),
    Text(Arc<String>),
    Row(Arc<Row>, usize),
    Filtered(Arc<FilterMatch>, usize),
    Owned(Arc<Vec<u8>>),
}

impl Input {
    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::Header(session, column) => &session.first_rows[0].fields[*column],
            Self::Text(value) => value.as_bytes(),
            Self::Row(row, column) => &row.fields[*column],
            Self::Filtered(row, column) => &row.fields[*column],
            Self::Owned(value) => value,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub spaces: u64,
    pub tabs: u64,
    pub cr: u64,
    pub lf: u64,
    pub nbsp: u64,
    pub other: u64,
    pub leading: u64,
    pub trailing: u64,
    pub repeated_spaces: u64,
    pub leading_end: usize,
    pub trailing_start: usize,
}

impl Summary {
    pub fn any(self) -> bool {
        self.spaces + self.tabs + self.cr + self.lf + self.nbsp + self.other > 0
    }

    pub fn likely(self) -> bool {
        self.leading > 0
            || self.trailing > 0
            || self.repeated_spaces > 0
            || self.tabs + self.cr + self.lf + self.nbsp + self.other > 0
    }

    pub fn indicated(self, all: bool) -> bool {
        if all { self.any() } else { self.likely() }
    }

    pub fn description(self, all: bool) -> String {
        if !self.indicated(all) {
            return if self.any() {
                "Ordinary single spaces between words. Show All Whitespace to reveal them."
            } else {
                "No whitespace."
            }
            .into();
        }
        let mut parts = Vec::new();
        for (count, singular, plural) in [
            (
                self.leading,
                "leading whitespace character",
                "leading whitespace characters",
            ),
            (
                self.trailing,
                "trailing whitespace character",
                "trailing whitespace characters",
            ),
            (self.spaces, "space", "spaces"),
            (
                self.repeated_spaces,
                "space in repeated runs",
                "spaces in repeated runs",
            ),
            (self.tabs, "tab", "tabs"),
            (self.cr, "carriage return", "carriage returns"),
            (self.lf, "line feed", "line feeds"),
            (self.nbsp, "non-breaking space", "non-breaking spaces"),
            (
                self.other,
                "other Unicode whitespace character",
                "other Unicode whitespace characters",
            ),
        ] {
            if count > 0 {
                let label = if count == 1 { singular } else { plural };
                parts.push(format!("{count} {label}"));
            }
        }
        format!(
            "Whitespace: {}. Indicators describe the full value, including text outside the visible cell.",
            parts.join(", ")
        )
    }
}

/// Decode one scalar without treating invalid bytes as whitespace or losing byte positions.
pub fn scalar(bytes: &[u8], offset: usize) -> (char, usize) {
    let first = bytes[offset];
    if first < 0x80 {
        return (char::from(first), 1);
    }
    let width = match first {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return ('\u{fffd}', 1),
    };
    bytes
        .get(offset..offset + width)
        .and_then(|s| std::str::from_utf8(s).ok())
        .and_then(|s| s.chars().next())
        .map_or(('\u{fffd}', 1), |c| (c, width))
}

pub fn analyze(bytes: &[u8], cancel: &AtomicBool) -> Option<Summary> {
    let mut result = Summary::default();
    let mut offset = 0;
    let mut checked = 0;
    let mut nonwhite = false;
    let mut space_run = 0;
    while offset < bytes.len() {
        if offset >= checked {
            if cancel.load(Ordering::Acquire) {
                return None;
            }
            checked = offset.saturating_add(16 * 1024);
        }
        let (c, width) = scalar(bytes, offset);
        if c.is_whitespace() {
            if !nonwhite {
                result.leading += 1;
                result.leading_end = offset + width;
            }
            result.trailing += 1;
            match c {
                ' ' => result.spaces += 1,
                '\t' => result.tabs += 1,
                '\r' => result.cr += 1,
                '\n' => result.lf += 1,
                '\u{a0}' => result.nbsp += 1,
                _ => result.other += 1,
            }
        } else {
            nonwhite = true;
            result.trailing = 0;
            result.trailing_start = offset + width;
        }
        if c == ' ' {
            space_run += 1;
            result.repeated_spaces += match space_run {
                2 => 2,
                3.. => 1,
                _ => 0,
            };
        } else {
            space_run = 0;
        }
        offset += width;
    }
    (!cancel.load(Ordering::Acquire)).then_some(result)
}

struct Job {
    cancel: Arc<AtomicBool>,
    handle: JoinHandle<Vec<(CellId, Summary)>>,
}

impl Job {
    fn stop(self) {
        self.cancel.store(true, Ordering::Release);
        let _ = self.handle.join();
    }
}

#[derive(Default)]
pub struct Cache {
    entries: BTreeMap<CellId, (u64, Summary)>,
    clock: u64,
    usage: BTreeSet<(u64, CellId)>,
    requests: Vec<(CellId, Input)>,
    requested: BTreeSet<CellId>,
    queued_bytes: usize,
    job: Option<Job>,
    failed: bool,
}

impl Cache {
    pub fn begin_frame(&mut self) {
        if self.job.as_ref().is_some_and(|j| j.handle.is_finished()) {
            let job = self.job.take().unwrap();
            match job.handle.join() {
                Ok(entries) => {
                    for (id, summary) in entries {
                        if self.entries.len() >= MAX_CACHE_CELLS
                            && !self.entries.contains_key(&id)
                            && let Some((_, oldest)) = self.usage.pop_first()
                        {
                            self.entries.remove(&oldest);
                        }
                        if let Some((used, _)) = self.entries.insert(id, (self.clock, summary)) {
                            self.usage.remove(&(used, id));
                        }
                        self.usage.insert((self.clock, id));
                    }
                }
                Err(_) => self.failed = true,
            }
        }
        self.requests.clear();
        self.requested.clear();
        self.queued_bytes = 0;
    }

    pub fn get(
        &mut self,
        id: CellId,
        bytes: usize,
        input: impl FnOnce() -> Input,
    ) -> Option<Summary> {
        if bytes == 0 {
            return Some(Summary::default());
        }
        self.clock = self.clock.saturating_add(1);
        if let Some((used, summary)) = self.entries.get_mut(&id) {
            self.usage.remove(&(*used, id));
            *used = self.clock;
            self.usage.insert((self.clock, id));
            return Some(*summary);
        }
        if self.job.is_none()
            && !self.failed
            && self.requests.len() < MAX_BATCH_CELLS
            && (self.requests.is_empty()
                || self.queued_bytes.saturating_add(bytes) <= MAX_BATCH_BYTES)
            && self.requested.insert(id)
        {
            self.queued_bytes = self.queued_bytes.saturating_add(bytes);
            self.requests.push((id, input()));
        }
        None
    }

    /// Spawns any queued inspection batch and calls `notify` when the worker completes.
    /// Returns `true` when inspection work is still pending and the frontend should schedule
    /// a follow-up poll.
    pub fn finish_frame_with_notify(&mut self, notify: impl FnOnce() + Send + 'static) -> bool {
        if self.job.is_none() && !self.requests.is_empty() {
            let inputs = std::mem::take(&mut self.requests);
            let cancel = Arc::new(AtomicBool::new(false));
            let stopped = cancel.clone();
            let handle = thread::spawn(move || {
                let mut results = Vec::with_capacity(inputs.len());
                for (id, input) in inputs {
                    let Some(summary) = analyze(input.bytes(), &stopped) else {
                        break;
                    };
                    results.push((id, summary));
                }
                notify();
                results
            });
            self.job = Some(Job { cancel, handle });
        }
        self.job.is_some() || !self.requested.is_empty()
    }

    /// Navigation changes loaded rows, but not the current document's header values.
    pub fn invalidate_rows(&mut self) {
        if let Some(job) = self.job.take() {
            job.stop();
        }
        self.entries.retain(|id, _| id.row.is_none());
        self.usage.retain(|(_, id)| id.row.is_none());
        self.requests.clear();
        self.requested.clear();
        self.queued_bytes = 0;
        self.failed = false;
    }

    pub fn invalidate(&mut self) {
        if let Some(job) = self.job.take() {
            job.stop();
        }
        self.entries.clear();
        self.usage.clear();
        self.requests.clear();
        self.requested.clear();
        self.queued_bytes = 0;
        self.failed = false;
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn pending(&self) -> bool {
        self.job.is_some() || !self.requests.is_empty()
    }

    pub fn explanation(&self, summary: Option<Summary>, all: bool) -> String {
        summary.map_or_else(
            || {
                if self.failed {
                    "Whitespace inspection could not complete. Reopen the file to retry.".into()
                } else {
                    "Checking the full value for whitespace...".into()
                }
            },
            |s| s.description(all),
        )
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            job.stop();
        }
    }
}

/// Ignore unusable environment roots instead of writing preferences beneath the working directory.
pub fn unix_preferences_path(
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
        .map(|root| root.join("quarry/view-preferences"))
}

#[derive(Default)]
pub struct Preferences {
    pub show_all: bool,
    path: Option<PathBuf>,
}

impl Preferences {
    pub fn load(path: Option<PathBuf>) -> Self {
        let show_all = path
            .as_ref()
            .and_then(|p| File::open(p).ok())
            .and_then(|file| {
                let mut value = String::new();
                file.take(64).read_to_string(&mut value).ok()?;
                Some(value.trim() == "show_all_whitespace=1")
            })
            .unwrap_or(false);
        Self { show_all, path }
    }

    pub fn application() -> Self {
        let path = if cfg!(target_os = "macos") {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|root| root.is_absolute())
                .map(|root| root.join("Library/Application Support/Quarry/view-preferences"))
        } else if cfg!(target_os = "windows") {
            std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .filter(|root| root.is_absolute())
                .map(|root| root.join("Quarry/view-preferences"))
        } else {
            unix_preferences_path(
                std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
                std::env::var_os("HOME").map(PathBuf::from),
            )
        };
        Self::load(path)
    }

    pub fn set(&mut self, show_all: bool) -> io::Result<()> {
        self.show_all = show_all;
        let Some(path) = &self.path else {
            return Ok(());
        };
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("Invalid view preference location"))?;
        std::fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(if show_all {
            b"show_all_whitespace=1\n"
        } else {
            b"show_all_whitespace=0\n"
        })?;
        file.flush()?;
        file.persist(path).map_err(|e| e.error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(row: u64) -> CellId {
        CellId {
            row: Some(row),
            column: 0,
        }
    }

    fn input(value: &[u8]) -> Input {
        Input::Owned(Arc::new(value.to_vec()))
    }

    fn finish(cache: &mut Cache) {
        let _ = cache.finish_frame_with_notify(|| {});
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while cache.job.as_ref().is_some_and(|j| !j.handle.is_finished()) {
            assert!(std::time::Instant::now() < deadline);
            thread::yield_now();
        }
        cache.begin_frame();
    }

    #[test]
    fn cancellation_stops_analysis_and_empty_cache_requests_need_no_worker() {
        assert_eq!(
            analyze(&vec![b' '; 1024 * 1024], &AtomicBool::new(true)),
            None
        );
        let mut cache = Cache::default();
        assert_eq!(
            cache.get(id(0), 0, || panic!("empty values need no snapshot")),
            Some(Summary::default())
        );
        assert!(!cache.pending());
    }

    #[test]
    fn cache_inspects_hidden_tails_and_invalidation_discards_old_results() {
        let mut cache = Cache::default();
        let mut value = vec![b'x'; 1024 * 1024];
        value.push(b' ');
        cache.begin_frame();
        assert!(cache.get(id(1), value.len(), || input(&value)).is_none());
        finish(&mut cache);
        assert_eq!(
            cache
                .get(id(1), value.len(), || panic!("cached"))
                .unwrap()
                .trailing,
            1
        );
        cache.invalidate();
        assert!(cache.get(id(1), 1, || input(b"x")).is_none());
        finish(&mut cache);
        assert!(!cache.get(id(1), 1, || panic!("cached")).unwrap().any());
        cache.get(id(2), 64 * 1024 * 1024, || {
            Input::Owned(Arc::new(vec![b' '; 64 * 1024 * 1024]))
        });
        let _ = cache.finish_frame_with_notify(|| {});
        cache.invalidate();
        assert!(!cache.pending());
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn queue_and_cache_are_bounded_and_snapshots_are_lazy() {
        let mut cache = Cache::default();
        cache.begin_frame();
        for row in 0..200 {
            cache.get(id(row), 1, || {
                assert!(row < MAX_BATCH_CELLS as u64);
                input(b" ")
            });
        }
        assert_eq!(cache.requests.len(), MAX_BATCH_CELLS);
        finish(&mut cache);
        cache.get(id(0), 1, || panic!("cached"));
        cache.get(id(500), MAX_BATCH_BYTES, || input(b" "));
        cache.get(id(501), 1, || panic!("byte budget exceeded"));
        assert_eq!(cache.requests.len(), 1);
        cache.invalidate();
        // Exercise LRU at capacity without scanning or retaining source bytes.
        for row in 0..MAX_CACHE_CELLS as u64 {
            cache.entries.insert(id(row), (row, Summary::default()));
            cache.usage.insert((row, id(row)));
        }
        cache.clock = MAX_CACHE_CELLS as u64;
        cache.get(id(0), 1, || panic!("cached"));
        cache.get(id(99_999), 1, || input(b" "));
        finish(&mut cache);
        assert_eq!(cache.entries.len(), MAX_CACHE_CELLS);
        assert_eq!(cache.usage.len(), MAX_CACHE_CELLS);
        assert!(cache.entries.contains_key(&id(0)));
        assert!(!cache.entries.contains_key(&id(1)));
    }

    #[test]
    fn row_and_filtered_snapshots_share_raw_values() {
        let row = Arc::new(Row {
            offset: 0,
            fields: vec![b" x ".to_vec()],
        });
        let source = Input::Row(row.clone(), 0);
        assert_eq!(Arc::strong_count(&row), 2);
        assert_eq!(source.bytes(), b" x ");
        let filtered = Arc::new(FilterMatch {
            match_ordinal: 0,
            row: 2,
            record_offset: 3,
            fields: vec![b"\xff ".to_vec()],
        });
        assert_eq!(Input::Filtered(filtered, 0).bytes(), b"\xff ");
    }

    #[test]
    fn preferences_start_off_and_persist_only_the_boolean() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings/view");
        let mut prefs = Preferences::load(Some(path.clone()));
        assert!(!prefs.show_all);
        prefs.set(true).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "show_all_whitespace=1\n"
        );
        assert!(Preferences::load(Some(path.clone())).show_all);
        prefs.set(false).unwrap();
        assert!(!Preferences::load(Some(path.clone())).show_all);
        for corrupt in ["invalid", "show_all_whitespace=2", &"x".repeat(1000)] {
            std::fs::write(&path, corrupt).unwrap();
            assert!(!Preferences::load(Some(path.clone())).show_all);
        }
        let mut blocked = Preferences::load(Some(dir.path().to_path_buf()));
        assert!(blocked.set(true).is_err());
        assert!(blocked.show_all, "view still changes if persistence fails");
    }

    #[test]
    #[cfg(unix)]
    fn preference_paths_reject_empty_and_relative_config_roots() {
        let user_root = PathBuf::from("/users/quarry-test");
        let fallback = user_root.join(".config/quarry/view-preferences");
        assert_eq!(unix_preferences_path(None, Some(PathBuf::from(""))), None);
        assert_eq!(
            unix_preferences_path(None, Some(PathBuf::from("relative"))),
            None
        );
        for value in ["", "config", "./config", "../config"] {
            assert_eq!(
                unix_preferences_path(Some(PathBuf::from(value)), Some(user_root.clone())),
                Some(fallback.clone())
            );
            assert_eq!(
                unix_preferences_path(Some(PathBuf::from(value)), None),
                None
            );
        }
        assert_eq!(unix_preferences_path(None, Some(user_root)), Some(fallback));
        assert_eq!(
            unix_preferences_path(Some(PathBuf::from("/config-root")), None),
            Some(PathBuf::from("/config-root/quarry/view-preferences"))
        );
    }
}
