//! Engine-owned bounded worker pool with foreground-first priority and shared byte budget.
//!
//! Built directly on `std::thread` plus bounded mutex/condvar queues so Quarry avoids
//! external thread-pool runtime dependencies while enforcing predictable memory ceilings
//! and sub-25 ms cancellation across concurrent jobs.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;

/// Maximum worker threads the engine pool will ever spawn.
pub(crate) const MAX_POOL_WORKERS: usize = 64;

/// Default shared byte budget across all concurrent engine worker allocations (48 MiB).
pub(crate) const DEFAULT_SHARED_BYTE_BUDGET: usize = 48 * 1024 * 1024;

/// Default engine worker count: `available_parallelism() - 1`, clamped to at least 1.
pub fn default_worker_threads() -> usize {
    thread::available_parallelism()
        .map(|count| count.get().saturating_sub(1).max(1))
        .unwrap_or(1)
}

/// Resolve an optional user thread count into a bounded effective worker count.
pub(crate) fn resolve_worker_count(requested: Option<usize>) -> Result<usize, crate::QuarryError> {
    match requested {
        Some(0) => Err(crate::QuarryError::InvalidOption(
            "thread count must be at least 1",
        )),
        Some(n) => Ok(n.min(MAX_POOL_WORKERS)),
        None => Ok(default_worker_threads().min(MAX_POOL_WORKERS)),
    }
}

/// Scheduling priority for engine worker tasks. Foreground work runs ahead of background jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobPriority {
    #[allow(dead_code)]
    Foreground,
    Background,
}

type Task = Box<dyn FnOnce() + Send + 'static>;

struct PoolQueue {
    foreground: VecDeque<Task>,
    background: VecDeque<Task>,
    spawned_workers: usize,
    shutdown: bool,
}

/// Shared engine worker pool.
pub(crate) struct EnginePool {
    queue: Mutex<PoolQueue>,
    ready: Condvar,
    byte_budget: Arc<ByteBudget>,
}

impl EnginePool {
    pub(crate) fn new(byte_budget_bytes: usize) -> Self {
        Self {
            queue: Mutex::new(PoolQueue {
                foreground: VecDeque::new(),
                background: VecDeque::new(),
                spawned_workers: 0,
                shutdown: false,
            }),
            ready: Condvar::new(),
            byte_budget: Arc::new(ByteBudget::new(byte_budget_bytes)),
        }
    }

    pub(crate) fn byte_budget(&self) -> &Arc<ByteBudget> {
        &self.byte_budget
    }

    /// Ensure at least `target_workers` threads exist in the shared pool (capped at `MAX_POOL_WORKERS`).
    pub(crate) fn ensure_workers(&'static self, target_workers: usize) {
        let desired = target_workers.clamp(1, MAX_POOL_WORKERS);
        let mut guard = self.queue.lock().unwrap();
        while guard.spawned_workers < desired {
            let worker_id = guard.spawned_workers;
            guard.spawned_workers += 1;
            let spawn_result = thread::Builder::new()
                .name(format!("quarry-worker-{worker_id}"))
                .spawn(move || self.worker_loop());
            if spawn_result.is_err() {
                guard.spawned_workers -= 1;
                break;
            }
        }
    }

    /// Submit one task with the given priority.
    pub(crate) fn submit(&'static self, priority: JobPriority, task: Task) {
        let mut guard = self.queue.lock().unwrap();
        match priority {
            JobPriority::Foreground => guard.foreground.push_back(task),
            JobPriority::Background => guard.background.push_back(task),
        }
        self.ready.notify_one();
    }

    fn worker_loop(&self) {
        set_worker_qos();
        loop {
            let task = {
                let mut guard = self.queue.lock().unwrap();
                loop {
                    if let Some(task) = guard.foreground.pop_front() {
                        break task;
                    }
                    if let Some(task) = guard.background.pop_front() {
                        break task;
                    }
                    if guard.shutdown {
                        return;
                    }
                    guard = self.ready.wait(guard).unwrap();
                }
            };
            // Isolate any unexpected panic inside a task so the shared worker thread survives.
            let _ = catch_unwind(AssertUnwindSafe(task));
        }
    }
}

/// Global shared engine pool used across indexing, search, filter, and export jobs.
pub(crate) fn engine_pool() -> &'static EnginePool {
    static POOL: OnceLock<EnginePool> = OnceLock::new();
    POOL.get_or_init(|| EnginePool::new(DEFAULT_SHARED_BYTE_BUDGET))
}

/// Shared byte budget capping in-flight chunk buffers across concurrent jobs.
pub(crate) struct ByteBudget {
    limit: usize,
    used: Mutex<usize>,
    available: Condvar,
}

pub(crate) struct BudgetGuard {
    budget: Arc<ByteBudget>,
    bytes: usize,
}

impl Drop for BudgetGuard {
    fn drop(&mut self) {
        let mut used = self.budget.used.lock().unwrap();
        *used = used.saturating_sub(self.bytes);
        self.budget.available.notify_all();
    }
}

impl ByteBudget {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            used: Mutex::new(0),
            available: Condvar::new(),
        }
    }

    /// Reserve `bytes` from the shared budget, waiting until space frees up or `cancelled` fires.
    ///
    /// A single request larger than `self.limit` is admitted once `used == 0` so oversized
    /// records can still make forward progress without deadlocking.
    pub(crate) fn acquire(
        self: &Arc<Self>,
        bytes: usize,
        cancelled: &AtomicBool,
    ) -> Option<BudgetGuard> {
        let requested = bytes.max(1);
        let mut used = self.used.lock().unwrap();
        loop {
            if cancelled.load(Ordering::Acquire) {
                return None;
            }
            if *used == 0 || used.saturating_add(requested) <= self.limit {
                *used = used.saturating_add(requested);
                return Some(BudgetGuard {
                    budget: Arc::clone(self),
                    bytes: requested,
                });
            }
            // Wait with a short timeout so an external cancel store wakes us promptly without
            // needing every caller to signal this condvar explicitly.
            let (next_guard, _) = self
                .available
                .wait_timeout(used, std::time::Duration::from_millis(2))
                .unwrap();
            used = next_guard;
        }
    }

    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        *self.used.lock().unwrap()
    }
}

/// Per-job concurrency limiter layered over the shared `EnginePool`.
pub(crate) struct JobExecutor {
    pool: &'static EnginePool,
    priority: JobPriority,
    max_workers: usize,
    in_flight: Arc<(Mutex<usize>, Condvar)>,
    panicked: Arc<AtomicBool>,
    active_count: Arc<AtomicUsize>,
}

impl JobExecutor {
    pub(crate) fn new(
        pool: &'static EnginePool,
        priority: JobPriority,
        max_workers: usize,
    ) -> Self {
        let max_workers = max_workers.clamp(1, MAX_POOL_WORKERS);
        pool.ensure_workers(max_workers);
        Self {
            pool,
            priority,
            max_workers,
            in_flight: Arc::new((Mutex::new(0), Condvar::new())),
            panicked: Arc::new(AtomicBool::new(false)),
            active_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Wait for a per-job worker slot (bounded by `max_workers`), then submit `work` to the pool.
    ///
    /// Returns `false` without scheduling if `cancelled` becomes true while waiting.
    pub(crate) fn spawn<F>(&self, cancelled: &AtomicBool, work: F) -> bool
    where
        F: FnOnce() + Send + 'static,
    {
        let (lock, cvar) = &*self.in_flight;
        let mut in_flight = lock.lock().unwrap();
        while *in_flight >= self.max_workers {
            if cancelled.load(Ordering::Acquire) {
                return false;
            }
            let (next, _) = cvar
                .wait_timeout(in_flight, std::time::Duration::from_millis(2))
                .unwrap();
            in_flight = next;
        }
        if cancelled.load(Ordering::Acquire) {
            return false;
        }
        *in_flight += 1;
        drop(in_flight);

        let tracker = Arc::clone(&self.in_flight);
        let panicked = Arc::clone(&self.panicked);
        let active_count = Arc::clone(&self.active_count);
        self.pool.submit(
            self.priority,
            Box::new(move || {
                active_count.fetch_add(1, Ordering::AcqRel);
                struct InFlightGuard {
                    tracker: Arc<(Mutex<usize>, Condvar)>,
                    active_count: Arc<AtomicUsize>,
                }
                impl Drop for InFlightGuard {
                    fn drop(&mut self) {
                        self.active_count.fetch_sub(1, Ordering::AcqRel);
                        let (lock, cvar) = &*self.tracker;
                        let mut count = lock.lock().unwrap();
                        *count = count.saturating_sub(1);
                        cvar.notify_all();
                    }
                }
                let _guard = InFlightGuard {
                    tracker,
                    active_count,
                };
                if catch_unwind(AssertUnwindSafe(work)).is_err() {
                    panicked.store(true, Ordering::Release);
                }
            }),
        );
        true
    }

    /// Wait until every submitted task for this job has finished executing.
    pub(crate) fn drain(&self) -> Result<(), crate::QuarryError> {
        let (lock, cvar) = &*self.in_flight;
        let mut in_flight = lock.lock().unwrap();
        while *in_flight > 0 {
            in_flight = cvar.wait(in_flight).unwrap();
        }
        if self.panicked.load(Ordering::Acquire) {
            Err(crate::QuarryError::WorkerPanicked)
        } else {
            Ok(())
        }
    }
}

#[cfg(target_os = "macos")]
fn set_worker_qos() {
    // SAFETY: `pthread_set_qos_class_self_np` is a thread-local QoS hint on macOS.
    unsafe {
        let _ = libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INITIATED, 0);
    }
}

#[cfg(not(target_os = "macos"))]
fn set_worker_qos() {}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    use super::{ByteBudget, JobExecutor, JobPriority, default_worker_threads, engine_pool};

    #[test]
    fn default_workers_is_at_least_one() {
        assert!(default_worker_threads() >= 1);
    }

    #[test]
    fn byte_budget_enforces_limit_and_wakes_on_cancel() {
        let budget = Arc::new(ByteBudget::new(1024));
        let cancelled = Arc::new(AtomicBool::new(false));
        let g1 = budget.acquire(800, &cancelled).expect("fits under limit");
        assert_eq!(budget.used(), 800);

        let budget_clone = Arc::clone(&budget);
        let cancel_clone = Arc::clone(&cancelled);
        let waiter = thread::spawn(move || budget_clone.acquire(500, &cancel_clone).is_some());

        thread::sleep(Duration::from_millis(10));
        cancelled.store(true, Ordering::Release);
        assert!(!waiter.join().unwrap(), "cancelled waiter must return None");
        drop(g1);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn job_executor_bounds_concurrency_and_drains_cleanly() {
        let executor = JobExecutor::new(engine_pool(), JobPriority::Foreground, 3);
        let cancelled = AtomicBool::new(false);
        let current = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicUsize::new(0));

        for _ in 0..12 {
            let current = Arc::clone(&current);
            let peak = Arc::clone(&peak);
            let done = Arc::clone(&done);
            assert!(executor.spawn(&cancelled, move || {
                let now = current.fetch_add(1, Ordering::AcqRel) + 1;
                peak.fetch_max(now, Ordering::AcqRel);
                thread::sleep(Duration::from_millis(5));
                current.fetch_sub(1, Ordering::AcqRel);
                done.fetch_add(1, Ordering::AcqRel);
            }));
        }

        executor.drain().unwrap();
        assert_eq!(done.load(Ordering::Acquire), 12);
        assert!(peak.load(Ordering::Acquire) <= 3);
    }
}
