//! Non-blocking Live Resource HUD telemetry sampler and state-transition announcement tracker.
//!
//! Samples OS-reported process memory (`phys_footprint` on macOS, `VmRSS` on Linux) and
//! per-volume available disk space strictly off the UI thread at `<= 2 Hz`, and emits
//! screen-reader announcements only on real state transitions (never per polling sample).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use quarry_core::{ProcessMemorySample, available_disk_space, process_memory_usage};

use crate::{format_bytes, group_digits};

/// Advisory free-space threshold below which a volume triggers a low-disk transition (256 MiB).
pub const LOW_DISK_WARNING_BYTES: u64 = 256 * 1024 * 1024;

/// Default minimum sampling interval enforcing the `<= 2 Hz` background polling cap.
pub const DEFAULT_HUD_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

/// Advisory free space measured on one relevant volume (active file or temporary scratch folder).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeSpaceSample {
    pub label: String,
    pub directory: PathBuf,
    pub available_bytes: u64,
    pub low_disk: bool,
}

/// Snapshot of the current Live Resource HUD metrics across all tracked window document volumes
/// and the shared temporary scratch volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceHudSample {
    pub memory: Option<ProcessMemorySample>,
    pub scratch_volume: Option<VolumeSpaceSample>,
    pub scratch_failed: bool,
    /// Per-document-directory volume reading (`None` when sampling failed / unknown).
    pub document_volumes: BTreeMap<PathBuf, Option<VolumeSpaceSample>>,
    /// Deduplicated summary across the scratch volume and first distinct document volume.
    pub volumes: Vec<VolumeSpaceSample>,
    pub sampled_at: Instant,
}

impl Default for ResourceHudSample {
    fn default() -> Self {
        Self {
            memory: None,
            scratch_volume: None,
            scratch_failed: false,
            document_volumes: BTreeMap::new(),
            volumes: Vec::new(),
            sampled_at: Instant::now(),
        }
    }
}

impl ResourceHudSample {
    /// Resolve the deduplicated volume list for a specific window's active document directory
    /// together with a flag indicating whether any required volume reading is missing/failed.
    pub fn volumes_for(&self, document_dir: Option<&Path>) -> (Vec<VolumeSpaceSample>, bool) {
        let mut out = Vec::new();
        let mut incomplete = self.scratch_failed;

        match document_dir {
            Some(dir) => match self.document_volumes.get(dir) {
                Some(Some(doc_vol)) => {
                    if let Some(scratch) = &self.scratch_volume {
                        if same_volume(&doc_vol.directory, &scratch.directory) {
                            let mut merged = scratch.clone();
                            merged.label = "Disk".into();
                            out.push(merged);
                        } else {
                            out.push(doc_vol.clone());
                            out.push(scratch.clone());
                        }
                    } else {
                        let mut single = doc_vol.clone();
                        single.label = "Disk".into();
                        out.push(single);
                    }
                }
                Some(None) => {
                    incomplete = true;
                    if let Some(scratch) = &self.scratch_volume {
                        out.push(scratch.clone());
                    }
                }
                None => {
                    // Window's document directory has not been sampled yet; do not treat missing
                    // data as recovery while still showing scratch space if available.
                    incomplete = true;
                    if let Some(scratch) = &self.scratch_volume {
                        let mut merged = scratch.clone();
                        merged.label = "Disk".into();
                        out.push(merged);
                    }
                }
            },
            None => {
                if let Some(scratch) = &self.scratch_volume {
                    let mut merged = scratch.clone();
                    merged.label = "Disk".into();
                    out.push(merged);
                }
            }
        }

        (out, incomplete)
    }
}

/// Relevant directories whose volumes should be sampled by the background HUD thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceHudTargets {
    pub document_dirs: BTreeSet<PathBuf>,
    pub scratch_dir: PathBuf,
}

impl Default for ResourceHudTargets {
    fn default() -> Self {
        Self {
            document_dirs: BTreeSet::new(),
            scratch_dir: std::env::temp_dir(),
        }
    }
}

fn nearest_existing_dir(path: &Path) -> Option<PathBuf> {
    let mut current = Some(path);
    while let Some(dir) = current {
        if dir.is_dir() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

#[cfg(unix)]
fn volume_device(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|meta| meta.dev())
}

#[cfg(unix)]
fn same_volume(a: &Path, b: &Path) -> bool {
    match (volume_device(a), volume_device(b)) {
        (Some(da), Some(db)) => da == db,
        _ => a == b,
    }
}

#[cfg(not(unix))]
fn same_volume(a: &Path, b: &Path) -> bool {
    a == b
}

/// Sample OS process memory and all relevant volume availabilities once.
/// Intended to run on a background thread so slow or network volumes never block UI rendering.
pub fn sample_resource_hud(targets: &ResourceHudTargets) -> ResourceHudSample {
    let memory = process_memory_usage();
    let scratch_existing = nearest_existing_dir(&targets.scratch_dir);

    #[cfg(unix)]
    let mut dev_cache: BTreeMap<u64, Option<u64>> = BTreeMap::new();
    let mut query_space = |dir: &Path| -> Option<u64> {
        #[cfg(unix)]
        if let Some(dev) = volume_device(dir) {
            if let Some(&cached) = dev_cache.get(&dev) {
                return cached;
            }
            let reading = available_disk_space(dir).ok();
            dev_cache.insert(dev, reading);
            return reading;
        }
        available_disk_space(dir).ok()
    };

    let (scratch_volume, scratch_failed) = match scratch_existing {
        Some(dir) => match query_space(&dir) {
            Some(available_bytes) => (
                Some(VolumeSpaceSample {
                    label: "Scratch".into(),
                    directory: dir,
                    available_bytes,
                    low_disk: available_bytes < LOW_DISK_WARNING_BYTES,
                }),
                false,
            ),
            None => (None, true),
        },
        None => (None, true),
    };

    let mut document_volumes = BTreeMap::new();
    for raw_dir in &targets.document_dirs {
        let entry = match nearest_existing_dir(raw_dir) {
            Some(dir) => query_space(&dir).map(|available_bytes| VolumeSpaceSample {
                label: "File disk".into(),
                directory: dir,
                available_bytes,
                low_disk: available_bytes < LOW_DISK_WARNING_BYTES,
            }),
            None => None,
        };
        document_volumes.insert(raw_dir.clone(), entry);
    }

    let first_doc = targets.document_dirs.iter().next().map(PathBuf::as_path);
    let mut sample = ResourceHudSample {
        memory,
        scratch_volume,
        scratch_failed,
        document_volumes,
        volumes: Vec::new(),
        sampled_at: Instant::now(),
    };
    let (summary_volumes, _) = sample.volumes_for(first_doc);
    sample.volumes = summary_volumes;
    sample
}

struct SamplerState {
    targets: ResourceHudTargets,
    targets_changed: bool,
    shutdown: bool,
}

/// Off-thread sampler collecting process memory and per-volume disk availability at `<= 2 Hz`.
pub struct ResourceHudSampler {
    latest: Arc<Mutex<ResourceHudSample>>,
    state: Arc<(Mutex<SamplerState>, Condvar)>,
    handle: Option<JoinHandle<()>>,
}

impl ResourceHudSampler {
    /// Start a background HUD sampler enforcing at least `interval` (clamped to `>= 500 ms`
    /// in production so sampling stays `<= 2 Hz`).
    pub fn start(initial_targets: ResourceHudTargets) -> Self {
        Self::start_with_interval(initial_targets, DEFAULT_HUD_SAMPLE_INTERVAL)
    }

    pub fn start_with_interval(initial_targets: ResourceHudTargets, interval: Duration) -> Self {
        let interval = interval.max(DEFAULT_HUD_SAMPLE_INTERVAL);
        let latest = Arc::new(Mutex::new(ResourceHudSample::default()));
        let state = Arc::new((
            Mutex::new(SamplerState {
                targets: initial_targets,
                targets_changed: true,
                shutdown: false,
            }),
            Condvar::new(),
        ));

        let worker_latest = Arc::clone(&latest);
        let worker_state = Arc::clone(&state);
        let handle = thread::Builder::new()
            .name("quarry-hud-sampler".into())
            .spawn(move || {
                let mut last_sample: Option<Instant> = None;
                loop {
                    let targets = {
                        let (lock, cvar) = &*worker_state;
                        let mut guard = lock.lock().unwrap();
                        loop {
                            if guard.shutdown {
                                return;
                            }
                            let due = match last_sample {
                                None => true,
                                Some(prev) => prev.elapsed() >= interval,
                            };
                            if due {
                                guard.targets_changed = false;
                                break guard.targets.clone();
                            }
                            let remaining = interval
                                .saturating_sub(last_sample.unwrap().elapsed())
                                .max(Duration::from_millis(10));
                            let (next, _) = cvar.wait_timeout(guard, remaining).unwrap();
                            guard = next;
                        }
                    };
                    let sample = sample_resource_hud(&targets);
                    last_sample = Some(sample.sampled_at);
                    *worker_latest.lock().unwrap() = sample;
                }
            })
            .ok();

        Self {
            latest,
            state,
            handle,
        }
    }

    /// Update the active document and scratch directories without blocking the caller.
    pub fn set_targets(&self, targets: ResourceHudTargets) {
        let (lock, cvar) = &*self.state;
        let mut guard = lock.lock().unwrap();
        if guard.targets != targets {
            guard.targets = targets;
            guard.targets_changed = true;
            cvar.notify_one();
        }
    }

    /// Return the latest background-collected resource sample without doing any I/O.
    pub fn snapshot(&self) -> ResourceHudSample {
        self.latest.lock().unwrap().clone()
    }

    #[cfg(test)]
    pub fn stop_and_join(&mut self) {
        {
            let (lock, cvar) = &*self.state;
            let mut guard = lock.lock().unwrap();
            guard.shutdown = true;
            cvar.notify_all();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ResourceHudSampler {
    fn drop(&mut self) {
        // Signal shutdown and detach without blocking the caller/UI thread if a slow volume
        // statvfs call is currently in progress; the worker owns its Arc state and exits cleanly
        // as soon as the in-flight sample returns.
        {
            let (lock, cvar) = &*self.state;
            let mut guard = lock.lock().unwrap();
            guard.shutdown = true;
            cvar.notify_all();
        }
        let _ = self.handle.take();
    }
}

/// Indexing lifecycle phase tracked for quiet state-transition announcements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HudIndexPhase {
    Idle,
    Indexing,
    Paused,
    Complete,
}

/// Screen-reader announcement emitted strictly on state transitions (never on polling ticks).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HudAnnouncement {
    pub text: String,
    pub is_alert: bool,
}

/// Pure state-transition detector for Live Resource HUD accessibility announcements.
#[derive(Clone, Debug, Default)]
pub struct HudAnnouncementTracker {
    active_doc_instance: Option<u64>,
    last_index_phase: Option<HudIndexPhase>,
    low_disk_active: bool,
    last_announcement: Option<HudAnnouncement>,
}

impl HudAnnouncementTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Latest state-transition announcement, if any.
    pub fn current(&self) -> Option<&HudAnnouncement> {
        self.last_announcement.as_ref()
    }

    /// Observe the current indexing phase and disk state, returning a new announcement **only**
    /// when a meaningful state transition occurred (paused, resumed, completed, or low-disk edge).
    /// When `sample_incomplete` is true (or `volumes` is empty), a failed/unknown disk reading
    /// never clears an active low-disk state or announces recovery.
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        doc_instance: Option<u64>,
        phase: HudIndexPhase,
        committed_rows: u64,
        committed_bytes: u64,
        file_size: u64,
        volumes: &[VolumeSpaceSample],
        sample_incomplete: bool,
    ) -> Option<HudAnnouncement> {
        let mut next: Option<HudAnnouncement> = None;

        // Switching to a different document resets phase history without spuriously announcing
        // an already-complete document.
        if self.active_doc_instance != doc_instance {
            self.active_doc_instance = doc_instance;
            self.last_index_phase = Some(phase);
        } else if let Some(prev) = self.last_index_phase
            && prev != phase
        {
            self.last_index_phase = Some(phase);
            next = match (prev, phase) {
                (_, HudIndexPhase::Paused) => Some(HudAnnouncement {
                    text: format!(
                        "Indexing paused at row {} ({} of {} committed).",
                        group_digits(committed_rows),
                        format_bytes(committed_bytes),
                        format_bytes(file_size),
                    ),
                    is_alert: false,
                }),
                (HudIndexPhase::Paused, HudIndexPhase::Indexing) => Some(HudAnnouncement {
                    text: "Indexing resumed.".into(),
                    is_alert: false,
                }),
                (HudIndexPhase::Indexing | HudIndexPhase::Paused, HudIndexPhase::Complete) => {
                    Some(HudAnnouncement {
                        text: format!("Indexing complete ({} rows).", group_digits(committed_rows),),
                        is_alert: false,
                    })
                }
                _ => None,
            };
        }

        let low_vol = volumes.iter().find(|v| v.low_disk);
        let now_low = low_vol.is_some();
        if now_low && !self.low_disk_active {
            self.low_disk_active = true;
            if let Some(vol) = low_vol {
                next = Some(HudAnnouncement {
                    text: format!(
                        "Low disk space on {}: {} free.",
                        vol.label,
                        format_bytes(vol.available_bytes),
                    ),
                    is_alert: true,
                });
            }
        } else if !now_low && !sample_incomplete && !volumes.is_empty() && self.low_disk_active {
            self.low_disk_active = false;
            if next.is_none() {
                next = Some(HudAnnouncement {
                    text: "Disk space recovered.".into(),
                    is_alert: false,
                });
            }
        }

        if let Some(ref announcement) = next {
            self.last_announcement = Some(announcement.clone());
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announcements_fire_only_on_state_transitions_never_on_polling_ticks() {
        let mut tracker = HudAnnouncementTracker::new();
        let healthy = vec![VolumeSpaceSample {
            label: "Disk".into(),
            directory: PathBuf::from("/tmp"),
            available_bytes: 10 * 1024 * 1024 * 1024,
            low_disk: false,
        }];
        let low = vec![VolumeSpaceSample {
            label: "Scratch".into(),
            directory: PathBuf::from("/tmp"),
            available_bytes: 64 * 1024 * 1024,
            low_disk: true,
        }];

        // Initial observation of indexing document does not spam.
        assert_eq!(
            tracker.update(
                Some(1),
                HudIndexPhase::Indexing,
                100,
                4096,
                100_000,
                &healthy,
                false,
            ),
            None
        );
        // Repeated polling ticks with growing bytes/rows emit nothing.
        assert_eq!(
            tracker.update(
                Some(1),
                HudIndexPhase::Indexing,
                500,
                16_384,
                100_000,
                &healthy,
                false,
            ),
            None
        );
        assert_eq!(
            tracker.update(
                Some(1),
                HudIndexPhase::Indexing,
                900,
                32_768,
                100_000,
                &healthy,
                false,
            ),
            None
        );

        // Transition Indexing -> Paused emits exactly one status announcement.
        let paused = tracker
            .update(
                Some(1),
                HudIndexPhase::Paused,
                900,
                32_768,
                100_000,
                &healthy,
                false,
            )
            .expect("pause transition announces");
        assert!(!paused.is_alert);
        assert!(paused.text.contains("Indexing paused"));
        assert!(paused.text.contains("900"));

        // Steady-state paused ticks emit nothing.
        assert_eq!(
            tracker.update(
                Some(1),
                HudIndexPhase::Paused,
                900,
                32_768,
                100_000,
                &healthy,
                false,
            ),
            None
        );

        // Low-disk edge fires an Alert once.
        let alert = tracker
            .update(
                Some(1),
                HudIndexPhase::Paused,
                900,
                32_768,
                100_000,
                &low,
                false,
            )
            .expect("low disk transition announces alert");
        assert!(alert.is_alert);
        assert!(alert.text.contains("Low disk space"));

        // A failed/incomplete disk reading must NOT clear low_disk_active or announce recovery.
        assert_eq!(
            tracker.update(
                Some(1),
                HudIndexPhase::Paused,
                900,
                32_768,
                100_000,
                &[],
                true,
            ),
            None
        );
        assert_eq!(
            tracker.update(
                Some(1),
                HudIndexPhase::Paused,
                900,
                32_768,
                100_000,
                &healthy,
                true,
            ),
            None
        );

        // Only a complete healthy reading announces recovery.
        let recovered = tracker
            .update(
                Some(1),
                HudIndexPhase::Paused,
                900,
                32_768,
                100_000,
                &healthy,
                false,
            )
            .expect("complete healthy sample announces recovery");
        assert!(!recovered.is_alert);
        assert!(recovered.text.contains("recovered"));

        // Transition Paused -> Indexing -> Complete.
        let resumed = tracker
            .update(
                Some(1),
                HudIndexPhase::Indexing,
                900,
                32_768,
                100_000,
                &healthy,
                false,
            )
            .expect("resume transition announces");
        assert!(resumed.text.contains("resumed"));
        let done = tracker
            .update(
                Some(1),
                HudIndexPhase::Complete,
                2500,
                100_000,
                100_000,
                &healthy,
                false,
            )
            .expect("completion transition announces");
        assert!(done.text.contains("Indexing complete"));
        assert!(done.text.contains("2,500"));
    }

    #[test]
    fn sampler_runs_off_thread_deduplicates_same_volume_and_stops_cleanly() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let mut sampler = ResourceHudSampler::start(ResourceHudTargets {
            document_dirs: BTreeSet::from([dir_a.path().to_path_buf(), dir_b.path().to_path_buf()]),
            scratch_dir: dir_a.path().to_path_buf(),
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let sample = loop {
            let s = sampler.snapshot();
            if !s.volumes.is_empty() {
                break s;
            }
            assert!(Instant::now() < deadline, "HUD sampler timed out");
            thread::sleep(Duration::from_millis(10));
        };
        assert!(sample.volumes.iter().all(|v| v.available_bytes > 0));
        let (vols_a, incomplete_a) = sample.volumes_for(Some(dir_a.path()));
        assert!(!incomplete_a);
        assert_eq!(vols_a.len(), 1);
        assert_eq!(vols_a[0].label, "Disk");

        // Per-window selection handles distinct document volumes independently.
        let synthetic = ResourceHudSample {
            memory: sample.memory,
            scratch_volume: Some(VolumeSpaceSample {
                label: "Scratch".into(),
                directory: PathBuf::from("/vol-a"),
                available_bytes: 50 * 1024 * 1024 * 1024,
                low_disk: false,
            }),
            scratch_failed: false,
            document_volumes: BTreeMap::from([
                (
                    PathBuf::from("/vol-a/doc1"),
                    Some(VolumeSpaceSample {
                        label: "File disk".into(),
                        directory: PathBuf::from("/vol-a"),
                        available_bytes: 50 * 1024 * 1024 * 1024,
                        low_disk: false,
                    }),
                ),
                (
                    PathBuf::from("/vol-b/doc2"),
                    Some(VolumeSpaceSample {
                        label: "File disk".into(),
                        directory: PathBuf::from("/vol-b"),
                        available_bytes: 64 * 1024 * 1024,
                        low_disk: true,
                    }),
                ),
            ]),
            volumes: Vec::new(),
            sampled_at: Instant::now(),
        };
        let (win1_vols, win1_inc) = synthetic.volumes_for(Some(Path::new("/vol-a/doc1")));
        assert!(!win1_inc);
        assert_eq!(win1_vols.len(), 1);
        assert!(!win1_vols[0].low_disk);

        let (win2_vols, win2_inc) = synthetic.volumes_for(Some(Path::new("/vol-b/doc2")));
        assert!(!win2_inc);
        assert_eq!(win2_vols.len(), 2);
        assert_eq!(win2_vols[0].label, "File disk");
        assert!(win2_vols[0].low_disk);

        sampler.stop_and_join();
    }
}
