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

/// Default tracked-payload single-run memory budget for out-of-core operations (300 MB decimal).
pub const DEFAULT_SINGLE_RUN_MEMORY_BUDGET_BYTES: usize = 300_000_000;

/// Minimum floor for the adaptive single-run memory budget (16 MiB).
const MIN_ADAPTIVE_SINGLE_RUN_MEMORY_BUDGET_BYTES: usize = 16 * 1024 * 1024;

/// Record size threshold above which workers must acquire the large-record admission gate
/// so at most one worker holds an oversized record at a time.
pub(crate) const LARGE_RECORD_GATE_BYTES: usize = 4 * 1024 * 1024;

/// Query total physical RAM on the host when supported by the OS.
pub(crate) fn physical_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let mut mib = [libc::CTL_HW, libc::HW_MEMSIZE];
        let mut mem: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        // SAFETY: `mib` has length 2, `mem` is a writable `u64`, and `len` is its exact byte size.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                (&mut mem as *mut u64).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 && len == std::mem::size_of::<u64>() && mem > 0 {
            Some(mem)
        } else {
            None
        }
    }

    #[cfg(target_os = "linux")]
    {
        let content = std::fs::read_to_string("/proc/meminfo").ok()?;
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                let kib: u64 = rest.split_whitespace().next()?.parse().ok()?;
                return kib.checked_mul(1024);
            }
        }
        None
    }

    #[cfg(windows)]
    {
        #[repr(C)]
        struct MemoryStatusEx {
            dw_length: u32,
            dw_memory_load: u32,
            ull_total_phys: u64,
            ull_avail_phys: u64,
            ull_total_page_file: u64,
            ull_avail_page_file: u64,
            ull_total_virtual: u64,
            ull_avail_virtual: u64,
            ull_avail_extended_virtual: u64,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GlobalMemoryStatusEx(lp_buffer: *mut MemoryStatusEx) -> i32;
        }
        let mut status = MemoryStatusEx {
            dw_length: std::mem::size_of::<MemoryStatusEx>() as u32,
            dw_memory_load: 0,
            ull_total_phys: 0,
            ull_avail_phys: 0,
            ull_total_page_file: 0,
            ull_avail_page_file: 0,
            ull_total_virtual: 0,
            ull_avail_virtual: 0,
            ull_avail_extended_virtual: 0,
        };
        // SAFETY: `status` has `dw_length` initialized to its exact struct size.
        if unsafe { GlobalMemoryStatusEx(&mut status) } != 0 && status.ull_total_phys > 0 {
            Some(status.ull_total_phys)
        } else {
            None
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        None
    }
}

/// Cap the default 300 MB single-run memory budget to a safe fraction (25%) of physical RAM.
pub fn default_single_run_memory_budget_bytes() -> usize {
    cap_single_run_budget_for_ram(
        DEFAULT_SINGLE_RUN_MEMORY_BUDGET_BYTES,
        physical_memory_bytes(),
    )
}

pub(crate) fn cap_single_run_budget_for_ram(requested: usize, physical_ram: Option<u64>) -> usize {
    match physical_ram {
        Some(ram) => {
            let quarter = (ram / 4).min(usize::MAX as u64) as usize;
            let cap = quarter.clamp(
                MIN_ADAPTIVE_SINGLE_RUN_MEMORY_BUDGET_BYTES,
                DEFAULT_SINGLE_RUN_MEMORY_BUDGET_BYTES,
            );
            requested.min(cap).max(1)
        }
        None => requested.max(1),
    }
}

/// Safe concurrent file-descriptor ceiling for merge groups, staying well beneath macOS's 256 soft limit.
pub(crate) fn default_fd_budget_limit() -> usize {
    #[cfg(unix)]
    {
        let rlim = rustix::process::getrlimit(rustix::process::Resource::Nofile);
        if let Some(soft) = rlim.current {
            let safe = soft.saturating_sub(64).min(soft / 2);
            return safe.clamp(4, 192) as usize;
        }
        128
    }

    #[cfg(not(unix))]
    {
        192
    }
}

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
    out_of_core_budget: Arc<ByteBudget>,
    large_record_gate: Arc<LargeRecordGate>,
    fd_budget: Arc<FdBudget>,
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
            out_of_core_budget: Arc::new(ByteBudget::new(default_single_run_memory_budget_bytes())),
            large_record_gate: Arc::new(LargeRecordGate::new()),
            fd_budget: Arc::new(FdBudget::new(default_fd_budget_limit())),
        }
    }

    pub(crate) fn byte_budget(&self) -> &Arc<ByteBudget> {
        &self.byte_budget
    }

    pub(crate) fn out_of_core_budget(&self) -> &Arc<ByteBudget> {
        &self.out_of_core_budget
    }

    pub(crate) fn large_record_gate(&self) -> &Arc<LargeRecordGate> {
        &self.large_record_gate
    }

    pub(crate) fn fd_budget(&self) -> &Arc<FdBudget> {
        &self.fd_budget
    }

    /// Ensure at least one worker thread exists (targeting `target_workers`, capped at `MAX_POOL_WORKERS`).
    ///
    /// Returns the number of live worker threads available for this job (up to `target_workers`),
    /// or an I/O error if zero worker threads could be spawned.
    pub(crate) fn ensure_workers(
        &'static self,
        target_workers: usize,
    ) -> Result<usize, crate::QuarryError> {
        let desired = target_workers.clamp(1, MAX_POOL_WORKERS);
        let mut guard = self.queue.lock().unwrap();
        let mut last_error = None;
        while guard.spawned_workers < desired {
            let worker_id = guard.spawned_workers;
            guard.spawned_workers += 1;
            match thread::Builder::new()
                .name(format!("quarry-worker-{worker_id}"))
                .spawn(move || self.worker_loop())
            {
                Ok(_) => {}
                Err(error) => {
                    guard.spawned_workers -= 1;
                    last_error = Some(error);
                    break;
                }
            }
        }
        if guard.spawned_workers == 0 {
            return Err(crate::QuarryError::Io(last_error.unwrap_or_else(|| {
                std::io::Error::other("failed to start engine worker pool thread")
            })));
        }
        Ok(guard.spawned_workers.min(desired))
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
    #[cfg(test)]
    wait_signal: Option<Arc<AtomicBool>>,
}

pub(crate) struct BudgetGuard {
    budget: Arc<ByteBudget>,
    bytes: usize,
}

impl BudgetGuard {
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    /// Release unused reservation bytes immediately and wake waiting jobs.
    pub(crate) fn shrink_to(&mut self, new_bytes: usize) {
        if new_bytes >= self.bytes {
            return;
        }
        let release = self.bytes - new_bytes;
        let mut used = self.budget.used.lock().unwrap();
        *used = used.saturating_sub(release);
        self.bytes = new_bytes;
        self.budget.available.notify_all();
    }
}

impl Drop for BudgetGuard {
    fn drop(&mut self) {
        if self.bytes == 0 {
            return;
        }
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
            #[cfg(test)]
            wait_signal: None,
        }
    }

    pub(crate) fn limit(&self) -> usize {
        self.limit
    }

    #[cfg(test)]
    pub(crate) fn with_wait_signal(limit: usize, wait_signal: Option<Arc<AtomicBool>>) -> Self {
        Self {
            limit: limit.max(1),
            used: Mutex::new(0),
            available: Condvar::new(),
            wait_signal,
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
        self.acquire_or_bypass(bytes, cancelled, || false)
    }

    /// Reserve `bytes` from the shared budget, waiting until space frees up, `bypass()`
    /// becomes true (e.g., head-of-line chunk needed to unblock draining), or `cancelled` fires.
    pub(crate) fn acquire_or_bypass(
        self: &Arc<Self>,
        bytes: usize,
        cancelled: &AtomicBool,
        bypass: impl Fn() -> bool,
    ) -> Option<BudgetGuard> {
        let requested = bytes.max(1);
        let mut used = self.used.lock().unwrap();
        loop {
            if cancelled.load(Ordering::Acquire) {
                return None;
            }
            if *used == 0 || used.saturating_add(requested) <= self.limit || bypass() {
                *used = used.saturating_add(requested);
                return Some(BudgetGuard {
                    budget: Arc::clone(self),
                    bytes: requested,
                });
            }
            // Wait with a short timeout so an external cancel store wakes us promptly without
            // needing every caller to signal this condvar explicitly.
            #[cfg(test)]
            if let Some(signal) = &self.wait_signal {
                signal.store(true, Ordering::Release);
            }
            let (next_guard, _) = self
                .available
                .wait_timeout(used, std::time::Duration::from_millis(2))
                .unwrap();
            used = next_guard;
        }
    }

    /// Reserve between `min_bytes` and `preferred_bytes` from the shared budget, waiting until
    /// at least `min_bytes` fit (or the budget is idle for a single large request) or `cancelled` fires.
    pub(crate) fn acquire_up_to(
        self: &Arc<Self>,
        min_bytes: usize,
        preferred_bytes: usize,
        cancelled: &AtomicBool,
    ) -> Option<BudgetGuard> {
        let min_req = min_bytes.max(1);
        let pref_req = preferred_bytes.max(min_req);
        let mut used = self.used.lock().unwrap();
        loop {
            if cancelled.load(Ordering::Acquire) {
                return None;
            }
            if *used == 0 {
                *used = pref_req;
                return Some(BudgetGuard {
                    budget: Arc::clone(self),
                    bytes: pref_req,
                });
            }
            let free = self.limit.saturating_sub(*used);
            if free >= min_req {
                let granted = free.min(pref_req);
                *used = used.saturating_add(granted);
                return Some(BudgetGuard {
                    budget: Arc::clone(self),
                    bytes: granted,
                });
            }
            #[cfg(test)]
            if let Some(signal) = &self.wait_signal {
                signal.store(true, Ordering::Release);
            }
            let (next_guard, _) = self
                .available
                .wait_timeout(used, std::time::Duration::from_millis(2))
                .unwrap();
            used = next_guard;
        }
    }

    /// Wake any waiters whose bypass predicate may now hold (e.g., head-of-line cursor advanced).
    pub(crate) fn notify_all(&self) {
        let _guard = self.used.lock().unwrap();
        self.available.notify_all();
    }

    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        *self.used.lock().unwrap()
    }
}

/// Mutual-exclusion admission gate ensuring at most one worker holds an oversized record at a time.
pub(crate) struct LargeRecordGate {
    held: Mutex<bool>,
    available: Condvar,
    #[cfg(test)]
    acquisitions: AtomicUsize,
}

pub(crate) struct LargeRecordGuard {
    gate: Arc<LargeRecordGate>,
}

impl Drop for LargeRecordGuard {
    fn drop(&mut self) {
        let mut held = self.gate.held.lock().unwrap();
        *held = false;
        self.gate.available.notify_all();
    }
}

impl LargeRecordGate {
    pub(crate) fn new() -> Self {
        Self {
            held: Mutex::new(false),
            available: Condvar::new(),
            #[cfg(test)]
            acquisitions: AtomicUsize::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn acquisitions(&self) -> usize {
        self.acquisitions.load(Ordering::Acquire)
    }

    pub(crate) fn try_acquire(self: &Arc<Self>) -> Option<LargeRecordGuard> {
        let mut held = self.held.lock().unwrap();
        if !*held {
            *held = true;
            #[cfg(test)]
            self.acquisitions.fetch_add(1, Ordering::AcqRel);
            Some(LargeRecordGuard {
                gate: Arc::clone(self),
            })
        } else {
            None
        }
    }

    pub(crate) fn acquire(self: &Arc<Self>, cancelled: &AtomicBool) -> Option<LargeRecordGuard> {
        let mut held = self.held.lock().unwrap();
        loop {
            if cancelled.load(Ordering::Acquire) {
                return None;
            }
            if !*held {
                *held = true;
                #[cfg(test)]
                self.acquisitions.fetch_add(1, Ordering::AcqRel);
                return Some(LargeRecordGuard {
                    gate: Arc::clone(self),
                });
            }
            let (next, _) = self
                .available
                .wait_timeout(held, std::time::Duration::from_millis(2))
                .unwrap();
            held = next;
        }
    }
}

/// Shared file-descriptor reservation limiter keeping concurrent merges beneath OS FD soft limits.
pub(crate) struct FdBudget {
    limit: usize,
    used: Mutex<usize>,
    available: Condvar,
}

pub(crate) struct FdGuard {
    budget: Arc<FdBudget>,
    count: usize,
}

impl Drop for FdGuard {
    fn drop(&mut self) {
        let mut used = self.budget.used.lock().unwrap();
        *used = used.saturating_sub(self.count);
        self.budget.available.notify_all();
    }
}

impl FdBudget {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            used: Mutex::new(0),
            available: Condvar::new(),
        }
    }

    pub(crate) fn limit(&self) -> usize {
        self.limit
    }

    /// Reserve `count` file descriptors, admitting a single request > limit once `used == 0`.
    pub(crate) fn acquire(
        self: &Arc<Self>,
        count: usize,
        cancelled: &AtomicBool,
    ) -> Option<FdGuard> {
        let requested = count.max(1);
        let mut used = self.used.lock().unwrap();
        loop {
            if cancelled.load(Ordering::Acquire) {
                return None;
            }
            if *used == 0 || used.saturating_add(requested) <= self.limit {
                *used = used.saturating_add(requested);
                return Some(FdGuard {
                    budget: Arc::clone(self),
                    count: requested,
                });
            }
            let (next, _) = self
                .available
                .wait_timeout(used, std::time::Duration::from_millis(2))
                .unwrap();
            used = next;
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
    ) -> Result<Self, crate::QuarryError> {
        let available = pool.ensure_workers(max_workers)?;
        let max_workers = max_workers.clamp(1, available);
        Ok(Self {
            pool,
            priority,
            max_workers,
            in_flight: Arc::new((Mutex::new(0), Condvar::new())),
            panicked: Arc::new(AtomicBool::new(false)),
            active_count: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub(crate) fn effective_workers(&self) -> usize {
        self.max_workers
    }

    pub(crate) fn has_panicked(&self) -> bool {
        self.panicked.load(Ordering::Acquire)
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
        let executor = JobExecutor::new(engine_pool(), JobPriority::Foreground, 3).unwrap();
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

    #[test]
    fn adaptive_single_run_budget_caps_by_physical_ram() {
        assert_eq!(
            super::cap_single_run_budget_for_ram(300_000_000, Some(16 * 1024 * 1024 * 1024)),
            300_000_000
        );
        assert_eq!(
            super::cap_single_run_budget_for_ram(300_000_000, Some(512 * 1024 * 1024)),
            128 * 1024 * 1024
        );
        assert_eq!(
            super::cap_single_run_budget_for_ram(300_000_000, Some(32 * 1024 * 1024)),
            16 * 1024 * 1024
        );
        assert_eq!(
            super::cap_single_run_budget_for_ram(300_000_000, None),
            300_000_000
        );
        assert!(super::default_single_run_memory_budget_bytes() <= 300_000_000);
        assert!(super::default_fd_budget_limit() >= 4);
    }

    #[test]
    fn large_record_gate_allows_one_holder_and_wakes_on_cancel() {
        let gate = Arc::new(super::LargeRecordGate::new());
        let cancelled = Arc::new(AtomicBool::new(false));
        let first = gate.acquire(&cancelled).expect("first holder admitted");
        let gate_clone = Arc::clone(&gate);
        let cancel_clone = Arc::clone(&cancelled);
        let waiter = thread::spawn(move || gate_clone.acquire(&cancel_clone).is_some());
        thread::sleep(Duration::from_millis(10));
        cancelled.store(true, Ordering::Release);
        assert!(!waiter.join().unwrap());
        drop(first);
        let fresh = AtomicBool::new(false);
        assert!(gate.acquire(&fresh).is_some());
    }

    #[test]
    fn fd_budget_bounds_concurrent_reservations_and_admits_single_large_request() {
        let budget = Arc::new(super::FdBudget::new(10));
        let cancelled = Arc::new(AtomicBool::new(false));
        let g1 = budget.acquire(6, &cancelled).expect("fits");
        assert_eq!(budget.used(), 6);
        let b2 = Arc::clone(&budget);
        let c2 = Arc::clone(&cancelled);
        let waiter = thread::spawn(move || b2.acquire(6, &c2).is_some());
        thread::sleep(Duration::from_millis(10));
        drop(g1);
        assert!(waiter.join().unwrap());
        assert_eq!(budget.used(), 0);
        // A single request larger than limit is still admitted when idle.
        let large = budget
            .acquire(33, &cancelled)
            .expect("single merge admitted");
        assert_eq!(budget.used(), 33);
        drop(large);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn acquire_up_to_and_shrink_to_share_and_release_budget_cleanly() {
        let budget = Arc::new(ByteBudget::new(1000));
        let cancelled = AtomicBool::new(false);
        let mut g1 = budget.acquire_up_to(200, 1000, &cancelled).expect("idle");
        assert_eq!(g1.bytes(), 1000);
        assert_eq!(budget.used(), 1000);

        let b2 = Arc::clone(&budget);
        let c2 = Arc::new(AtomicBool::new(false));
        let c2_wait = Arc::clone(&c2);
        let waiter = thread::spawn(move || {
            b2.acquire_up_to(300, 800, &c2_wait)
                .map(|guard| guard.bytes())
        });
        thread::sleep(Duration::from_millis(10));
        // Shrinking g1 from 1000 -> 400 frees 600 bytes (>= min 300), waking waiter for 600.
        g1.shrink_to(400);
        assert_eq!(g1.bytes(), 400);
        assert_eq!(waiter.join().unwrap(), Some(600));
        drop(g1);
        assert_eq!(budget.used(), 0);
    }
}
