//! Non-blocking Live Resource HUD telemetry sampler and state-transition announcement tracker.
//!
//! Samples OS-reported process memory (`phys_footprint` on macOS, `VmRSS` on Linux) and
//! per-volume available disk space strictly off the UI thread at `<= 2 Hz`, and emits
//! screen-reader announcements only on real state transitions (never per polling sample).

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

/// Snapshot of the current Live Resource HUD metrics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceHudSample {
    pub memory: Option<ProcessMemorySample>,
    pub volumes: Vec<VolumeSpaceSample>,
    pub sampled_at: Instant,
}

impl Default for ResourceHudSample {
    fn default() -> Self {
        Self {
            memory: None,
            volumes: Vec::new(),
            sampled_at: Instant::now(),
        }
    }
}

/// Relevant directories whose volumes should be sampled by the background HUD thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceHudTargets {
    pub document_dir: Option<PathBuf>,
    pub scratch_dir: PathBuf,
}

impl Default for ResourceHudTargets {
    fn default() -> Self {
        Self {
            document_dir: None,
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
fn same_volume(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ma), Ok(mb)) => ma.dev() == mb.dev(),
        _ => a == b,
    }
}

#[cfg(not(unix))]
fn same_volume(a: &Path, b: &Path) -> bool {
    a == b
}

/// Sample OS process memory and relevant volume availability once.
/// Intended to run on a background thread so slow or network volumes never block UI rendering.
pub fn sample_resource_hud(targets: &ResourceHudTargets) -> ResourceHudSample {
    let memory = process_memory_usage();
    let mut volumes = Vec::new();
    let scratch = nearest_existing_dir(&targets.scratch_dir);
    let doc = targets
        .document_dir
        .as_deref()
        .and_then(nearest_existing_dir);

    match (doc, scratch) {
        (Some(doc_dir), Some(scratch_dir)) if same_volume(&doc_dir, &scratch_dir) => {
            if let Ok(available_bytes) = available_disk_space(&scratch_dir) {
                volumes.push(VolumeSpaceSample {
                    label: "Disk".into(),
                    directory: scratch_dir,
                    available_bytes,
                    low_disk: available_bytes < LOW_DISK_WARNING_BYTES,
                });
            }
        }
        (Some(doc_dir), Some(scratch_dir)) => {
            if let Ok(available_bytes) = available_disk_space(&doc_dir) {
                volumes.push(VolumeSpaceSample {
                    label: "File disk".into(),
                    directory: doc_dir,
                    available_bytes,
                    low_disk: available_bytes < LOW_DISK_WARNING_BYTES,
                });
            }
            if let Ok(available_bytes) = available_disk_space(&scratch_dir) {
                volumes.push(VolumeSpaceSample {
                    label: "Scratch".into(),
                    directory: scratch_dir,
                    available_bytes,
                    low_disk: available_bytes < LOW_DISK_WARNING_BYTES,
                });
            }
        }
        (Some(doc_dir), None) => {
            if let Ok(available_bytes) = available_disk_space(&doc_dir) {
                volumes.push(VolumeSpaceSample {
                    label: "Disk".into(),
                    directory: doc_dir,
                    available_bytes,
                    low_disk: available_bytes < LOW_DISK_WARNING_BYTES,
                });
            }
        }
        (None, Some(scratch_dir)) => {
            if let Ok(available_bytes) = available_disk_space(&scratch_dir) {
                volumes.push(VolumeSpaceSample {
                    label: "Disk".into(),
                    directory: scratch_dir,
                    available_bytes,
                    low_disk: available_bytes < LOW_DISK_WARNING_BYTES,
                });
            }
        }
        (None, None) => {}
    }

    ResourceHudSample {
        memory,
        volumes,
        sampled_at: Instant::now(),
    }
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
}

impl Drop for ResourceHudSampler {
    fn drop(&mut self) {
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
    pub fn update(
        &mut self,
        doc_instance: Option<u64>,
        phase: HudIndexPhase,
        committed_rows: u64,
        committed_bytes: u64,
        file_size: u64,
        volumes: &[VolumeSpaceSample],
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
        } else if !now_low && self.low_disk_active {
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
                &healthy
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
                &healthy
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
                &healthy
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
                &healthy
            ),
            None
        );

        // Low-disk rising edge emits one alert announcement; repeated low-disk polls stay silent.
        let low_msg = tracker
            .update(Some(1), HudIndexPhase::Paused, 900, 32_768, 100_000, &low)
            .expect("low disk transition announces");
        assert!(low_msg.is_alert);
        assert!(low_msg.text.contains("Low disk space on Scratch"));
        assert_eq!(
            tracker.update(Some(1), HudIndexPhase::Paused, 900, 32_768, 100_000, &low),
            None
        );

        // Recovery emits once.
        let recovered = tracker
            .update(
                Some(1),
                HudIndexPhase::Paused,
                900,
                32_768,
                100_000,
                &healthy,
            )
            .expect("disk recovery announces");
        assert!(!recovered.is_alert);
        assert!(recovered.text.contains("Disk space recovered"));

        // Resume -> Complete emit one announcement each.
        let resumed = tracker
            .update(
                Some(1),
                HudIndexPhase::Indexing,
                900,
                32_768,
                100_000,
                &healthy,
            )
            .expect("resume transition announces");
        assert!(resumed.text.contains("Indexing resumed"));

        let complete = tracker
            .update(
                Some(1),
                HudIndexPhase::Complete,
                2_000,
                100_000,
                100_000,
                &healthy,
            )
            .expect("complete transition announces");
        assert!(complete.text.contains("Indexing complete"));
        assert!(complete.text.contains("2,000"));

        // Repeated completed polls stay silent.
        assert_eq!(
            tracker.update(
                Some(1),
                HudIndexPhase::Complete,
                2_000,
                100_000,
                100_000,
                &healthy
            ),
            None
        );
    }

    #[test]
    fn sampler_runs_off_thread_deduplicates_same_volume_and_stops_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let sampler = ResourceHudSampler::start(ResourceHudTargets {
            document_dir: Some(dir.path().to_path_buf()),
            scratch_dir: dir.path().to_path_buf(),
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        let sample = loop {
            let s = sampler.snapshot();
            if !s.volumes.is_empty() {
                break s;
            }
            assert!(
                Instant::now() < deadline,
                "HUD sampler did not populate sample"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(sample.volumes.len(), 1);
        assert_eq!(sample.volumes[0].label, "Disk");
        assert!(sample.volumes[0].available_bytes > 0);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(sample.memory.is_some_and(|m| m.bytes > 0));
        drop(sampler);
    }
}
