//! Debug-build checkpoints that hold a decompression worker, or a writer that
//! reads its private copy, at a known point, so tests can observe a running pass
//! without racing a fast machine. A held worker
//! continues when its pause is released or dropped, when its job is cancelled
//! (unless the pause also holds cancelled workers), or after a safety timeout.
//! Release builds leave this module out.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// A held worker continues on its own after this long, so a failed test cannot hang.
const SAFETY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long [`Pause::wait_for`] waits for workers to arrive.
const ARRIVAL_TIMEOUT: Duration = Duration::from_secs(20);

/// Where a worker stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checkpoint {
    /// After a chunk is written and its rows are published, once the private copy
    /// holds at least `min_bytes` decompressed bytes.
    Published { min_bytes: u64 },
    /// After the complete copy is flushed, before the source is checked again.
    BeforeSourceCheck,
    /// In a writer that reads a private copy, just before it checks the compressed
    /// source and publishes.
    BeforeGuardedPublication,
}

/// A point that a worker passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reached {
    Published,
    BeforeSourceCheck,
    BeforeGuardedPublication,
}

#[derive(Debug)]
struct Gate {
    source: PathBuf,
    checkpoint: Checkpoint,
    hold_cancelled: bool,
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct GateState {
    arrived: usize,
    released: bool,
}

impl Gate {
    fn state(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

static GATES: Mutex<Vec<Arc<Gate>>> = Mutex::new(Vec::new());

fn gates() -> MutexGuard<'static, Vec<Arc<Gate>>> {
    GATES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Holds every worker that decompresses `source` at a checkpoint until released.
#[derive(Debug)]
#[must_use = "dropping a pause releases it at once"]
pub struct Pause(Arc<Gate>);

impl Pause {
    /// A held worker also continues when its job is cancelled.
    pub fn new(source: impl Into<PathBuf>, checkpoint: Checkpoint) -> Self {
        Self::register(source.into(), checkpoint, false)
    }

    /// Holds a worker even after its job is cancelled, as while a closed tab's
    /// cleanup is still stopping it.
    pub fn holding_cancelled(source: impl Into<PathBuf>, checkpoint: Checkpoint) -> Self {
        Self::register(source.into(), checkpoint, true)
    }

    fn register(source: PathBuf, checkpoint: Checkpoint, hold_cancelled: bool) -> Self {
        let gate = Arc::new(Gate {
            source,
            checkpoint,
            hold_cancelled,
            state: Mutex::default(),
            changed: Condvar::new(),
        });
        gates().push(Arc::clone(&gate));
        Self(gate)
    }

    /// Waits until `workers` workers in total have stopped here; false on timeout.
    pub fn wait_for(&self, workers: usize) -> bool {
        let deadline = Instant::now() + ARRIVAL_TIMEOUT;
        let mut state = self.0.state();
        while state.arrived < workers {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self
                .0
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Lets every held worker continue; later workers pass without stopping.
    pub fn release(&self) {
        self.0.state().released = true;
        self.0.changed.notify_all();
    }
}

impl Drop for Pause {
    fn drop(&mut self) {
        self.release();
        gates().retain(|gate| !Arc::ptr_eq(gate, &self.0));
    }
}

/// The pause registered for a worker that starts on `source`, if any.
pub(crate) fn hold_for(source: &Path) -> Option<Hold> {
    let gates = gates();
    if gates.is_empty() {
        return None;
    }
    let canonical = source.canonicalize().ok();
    gates
        .iter()
        .find(|gate| {
            gate.source == source
                || canonical.is_some() && gate.source.canonicalize().ok() == canonical
        })
        .map(|gate| Hold {
            gate: Arc::clone(gate),
            stopped: AtomicBool::new(false),
        })
}

/// One worker's view of its pause; the worker stops there at most once.
pub(crate) struct Hold {
    gate: Arc<Gate>,
    stopped: AtomicBool,
}

impl Hold {
    pub(crate) fn reach(&self, point: Reached, written: u64, cancelled: &AtomicBool) {
        let here = match (self.gate.checkpoint, point) {
            (Checkpoint::Published { min_bytes }, Reached::Published) => written >= min_bytes,
            (Checkpoint::BeforeSourceCheck, Reached::BeforeSourceCheck)
            | (Checkpoint::BeforeGuardedPublication, Reached::BeforeGuardedPublication) => true,
            _ => false,
        };
        if !here || self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        let deadline = Instant::now() + SAFETY_TIMEOUT;
        let mut state = self.gate.state();
        state.arrived += 1;
        self.gate.changed.notify_all();
        while !state.released
            && (self.gate.hold_cancelled || !cancelled.load(Ordering::Acquire))
            && Instant::now() < deadline
        {
            state = self
                .gate
                .changed
                .wait_timeout(state, Duration::from_millis(5))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}
