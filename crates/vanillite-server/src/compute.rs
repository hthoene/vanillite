//! The compute pool and the `scheduler` slot.
//!
//! There is exactly one pool. It has two queues: *urgent* jobs belong to a running tick phase
//! (fork-join, see [`Compute::scope`]) and always go first; *background* jobs (chunk generation,
//! lighting, saving, compression) run by priority, lowest number first. The built-in pool runs
//! them on separate threads: *tick helpers* at the tick threads' OS priority take only urgent
//! jobs, the *workers* (lower priority, see [`Budget::worker_started`]) take background jobs.
//! A tick phase thus never waits for a part that runs at the workers' priority. Nothing else in
//! the server creates threads for computation.

use std::any::Any;
use std::collections::{BinaryHeap, VecDeque};
use std::marker::PhantomData;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::{cmp, fmt, io};

use crate::budget::Budget;

/// A unit of work.
pub type Job = Box<dyn FnOnce() + Send + 'static>;

/// The `scheduler` slot: runs jobs and owns the threads of the server.
pub trait Scheduler: Send + Sync + 'static {
    /// Number of worker threads (0 when jobs run inline).
    fn workers(&self) -> usize;

    /// Number of threads that run urgent jobs besides the thread waiting for them; a parallel
    /// tick phase splits into one part more than this.
    fn phase_helpers(&self) -> usize {
        self.workers()
    }

    /// Queues a job of a running tick phase. Urgent jobs run before any background job.
    fn spawn_urgent(&self, job: Job);

    /// Queues a background job. Lower `priority` runs first; equal priorities run in
    /// submission order.
    fn spawn_background(&self, priority: u32, job: Job);

    /// Runs one queued urgent job on the calling thread. Returns `false` if there was none.
    /// Threads that wait for a fork-join scope call this instead of idling.
    fn help(&self) -> bool;

    /// Starts a long-running named thread (world tick threads, the watchdog).
    fn spawn_thread(&self, name: String, body: Job) -> io::Result<JoinHandle<()>>;

    /// Frees what the worker threads keep between jobs (thread-local scratch buffers and
    /// compression contexts, used stack pages). Queued and running jobs are not affected.
    fn release_memory(&self) {}

    /// Stops the workers after all queued jobs have run. Owners cancel jobs they no longer need
    /// before shutdown (the pool cannot know which ones matter).
    fn shutdown(&self);
}

/// A cheap, cloneable handle to the scheduler with the typed helpers.
#[derive(Clone)]
pub struct Compute {
    scheduler: Arc<dyn Scheduler>,
    /// Threads of a parallel tick phase, the calling thread included (at least 1).
    phase_threads: usize,
}

impl fmt::Debug for Compute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Compute")
            .field("workers", &self.scheduler.workers())
            .finish()
    }
}

impl Compute {
    /// Wraps a scheduler.
    pub fn new(scheduler: Arc<dyn Scheduler>) -> Self {
        let phase_threads = scheduler.phase_helpers() + 1;
        Compute {
            scheduler,
            phase_threads,
        }
    }

    /// Limits the threads of a parallel tick phase (`performance.threads.tick-phases`):
    /// 0 = the phase helpers plus the calling thread, 1 = the calling thread alone.
    #[must_use]
    pub fn with_phase_threads(mut self, threads: usize) -> Self {
        let all = self.scheduler.phase_helpers() + 1;
        self.phase_threads = if threads == 0 { all } else { threads.min(all) };
        self
    }

    /// Threads of a parallel tick phase, the calling thread included (at least 1).
    pub fn phase_threads(&self) -> usize {
        self.phase_threads
    }

    /// A parallel tick phase over `items`: splits them into at most
    /// [`Compute::phase_threads`] contiguous parts of at least `min_part` items and runs
    /// `job(offset, part)` for each, the first on the calling thread; returns when all are
    /// done. With one part (few items, one thread) it runs inline. Each part sees only its
    /// own items, so the result does not depend on how they were split.
    pub fn parts<T: Send>(
        &self,
        items: &mut [T],
        min_part: usize,
        job: impl Fn(usize, &mut [T]) + Sync,
    ) {
        let n = items.len();
        let parts = self.phase_threads.min(n.div_ceil(min_part.max(1))).max(1);
        if parts == 1 {
            job(0, items);
            return;
        }
        let per = n.div_ceil(parts);
        let job = &job;
        self.scope(|scope| {
            let mut chunks = items.chunks_mut(per).enumerate();
            let first = chunks.next();
            for (i, part) in chunks {
                scope.spawn(move || job(i * per, part));
            }
            if let Some((_, part)) = first {
                job(0, part);
            }
        });
    }

    /// A scheduler that runs every job inline at submission, for deterministic tests.
    pub fn inline() -> Self {
        Compute::new(Arc::new(InlineScheduler))
    }

    /// The scheduler behind this handle.
    pub fn scheduler(&self) -> &dyn Scheduler {
        &*self.scheduler
    }

    /// See [`Scheduler::release_memory`].
    pub fn release_memory(&self) {
        self.scheduler.release_memory();
    }

    /// Queues a background job; see [`Scheduler::spawn_background`].
    pub fn spawn_background(&self, priority: u32, job: impl FnOnce() + Send + 'static) {
        self.scheduler.spawn_background(priority, Box::new(job));
    }

    /// Runs `job(0)`, …, `job(n - 1)` on [`Compute::phase_threads`] threads (the calling
    /// thread included) and returns when all are done. The indices are handed out one by one,
    /// so uneven jobs balance out.
    pub fn for_each(&self, n: usize, job: &(dyn Fn(usize) + Sync)) {
        let workers = self.phase_threads - 1;
        if n <= 1 || workers == 0 {
            for i in 0..n {
                job(i);
            }
            return;
        }
        let next = AtomicUsize::new(0);
        let run = || {
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                job(i);
            }
        };
        self.scope(|scope| {
            for _ in 0..workers.min(n - 1) {
                scope.spawn(run);
            }
            run();
        });
    }

    /// Fork-join: jobs spawned on the scope may borrow from the caller and all of them have
    /// finished when `scope` returns. The calling thread helps with urgent jobs while it waits,
    /// so a scope never deadlocks, even with one worker. A panic in a job is re-raised here.
    pub fn scope<'env, F, T>(&self, f: F) -> T
    where
        F: for<'scope> FnOnce(&'scope Scope<'scope, 'env>) -> T,
    {
        let scope = Scope {
            compute: self,
            state: Arc::new(ScopeState::default()),
            _scope: PhantomData,
            _env: PhantomData,
        };
        let result = panic::catch_unwind(AssertUnwindSafe(|| f(&scope)));
        scope.wait();
        let job_panic = scope
            .state
            .panic
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        match (result, job_panic) {
            (Err(payload), _) | (Ok(_), Some(payload)) => panic::resume_unwind(payload),
            (Ok(value), None) => value,
        }
    }
}

/// A fork-join scope; see [`Compute::scope`].
pub struct Scope<'scope, 'env: 'scope> {
    compute: &'scope Compute,
    state: Arc<ScopeState>,
    _scope: PhantomData<&'scope mut &'scope ()>,
    _env: PhantomData<&'env mut &'env ()>,
}

impl fmt::Debug for Scope<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scope")
            .field("pending", &self.state.pending.load(Ordering::Relaxed))
            .finish()
    }
}

#[derive(Default)]
struct ScopeState {
    pending: AtomicUsize,
    panic: Mutex<Option<Box<dyn Any + Send>>>,
    lock: Mutex<()>,
    done: Condvar,
}

impl<'scope> Scope<'scope, '_> {
    /// Runs `job` as an urgent job of this scope.
    pub fn spawn(&self, job: impl FnOnce() + Send + 'scope) {
        self.state.pending.fetch_add(1, Ordering::AcqRel);
        let state = Arc::clone(&self.state);
        let job: Box<dyn FnOnce() + Send + 'scope> = Box::new(move || {
            if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(job)) {
                let mut slot = state.panic.lock().unwrap_or_else(PoisonError::into_inner);
                slot.get_or_insert(payload);
            }
            if state.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
                let _guard = state.lock.lock().unwrap_or_else(PoisonError::into_inner);
                state.done.notify_all();
            }
        });
        // SAFETY: `Compute::scope` does not return (not even by unwinding) before `pending` is
        // back to zero, i.e. before this job has run to completion. Everything the job borrows
        // for `'scope` therefore outlives it, so erasing the lifetime is sound.
        let job: Job =
            unsafe { std::mem::transmute::<Box<dyn FnOnce() + Send + 'scope>, Job>(job) };
        self.compute.scheduler.spawn_urgent(job);
    }

    fn wait(&self) {
        while self.state.pending.load(Ordering::Acquire) != 0 {
            if self.compute.scheduler.help() {
                continue;
            }
            let guard = self
                .state
                .lock
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if self.state.pending.load(Ordering::Acquire) == 0 {
                break;
            }
            // Short timeout: new urgent jobs we could help with do not notify this condvar.
            let _ = self
                .state
                .done
                .wait_timeout(guard, Duration::from_micros(200));
        }
    }
}

/// Runs every job inline in submission order: deterministic, no threads (test mode).
#[derive(Debug)]
pub struct InlineScheduler;

impl Scheduler for InlineScheduler {
    fn workers(&self) -> usize {
        0
    }
    fn spawn_urgent(&self, job: Job) {
        job();
    }
    fn spawn_background(&self, _priority: u32, job: Job) {
        job();
    }
    fn help(&self) -> bool {
        false
    }
    fn spawn_thread(&self, name: String, body: Job) -> io::Result<JoinHandle<()>> {
        thread::Builder::new().name(name).spawn(body)
    }
    fn shutdown(&self) {}
}

/// The built-in compute pool: a fixed set of workers and tick helpers over one urgent FIFO and
/// one background priority heap.
pub struct ComputePool {
    shared: Arc<Shared>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    count: usize,
}

/// What a pool thread runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Background jobs, at the workers' priority; urgent jobs too in a pool without helpers.
    Worker,
    /// Urgent jobs only, at the priority the pool was started with.
    Helper,
}

impl fmt::Debug for ComputePool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ComputePool")
            .field("workers", &self.count)
            .field("helpers", &self.shared.helpers)
            .finish()
    }
}

struct Shared {
    queues: Mutex<Queues>,
    /// Wakes workers (background jobs, and urgent ones without helpers).
    available: Condvar,
    /// Wakes tick helpers (urgent jobs).
    urgent_ready: Condvar,
    /// Tick helpers; 0 = the workers take the urgent jobs.
    helpers: usize,
    /// Decides when background jobs may start ([`Budget::background_wait`]).
    budget: Option<Arc<Budget>>,
}

#[derive(Default)]
struct Queues {
    urgent: VecDeque<Job>,
    background: BinaryHeap<BackgroundJob>,
    next_seq: u64,
    stopping: bool,
    /// Workers started before this generation end after their job
    /// ([`Scheduler::release_memory`]).
    generation: u64,
    /// Jobs taken by workers of the current generation.
    jobs_taken: u64,
}

struct BackgroundJob {
    priority: u32,
    seq: u64,
    job: Job,
}

impl PartialEq for BackgroundJob {
    fn eq(&self, other: &Self) -> bool {
        (self.priority, self.seq) == (other.priority, other.seq)
    }
}
impl Eq for BackgroundJob {}
impl PartialOrd for BackgroundJob {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for BackgroundJob {
    /// `BinaryHeap` is a max-heap: the lowest priority number and then the oldest job is
    /// the greatest.
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        (other.priority, other.seq).cmp(&(self.priority, self.seq))
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queues> {
        self.queues.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ComputePool {
    /// Starts a pool with `workers` threads (at least one) that also run the urgent jobs.
    pub fn new(workers: usize) -> io::Result<ComputePool> {
        Self::start(workers, 0, None)
    }

    /// Starts a pool whose background jobs wait for `budget` ([`Budget::background_wait`]) and
    /// run at its worker priority, with `helpers` tick helpers for the urgent jobs (0 = the
    /// workers run them).
    pub fn with_budget(
        workers: usize,
        helpers: usize,
        budget: Arc<Budget>,
    ) -> io::Result<ComputePool> {
        Self::start(workers, helpers, Some(budget))
    }

    fn start(
        workers: usize,
        helpers: usize,
        budget: Option<Arc<Budget>>,
    ) -> io::Result<ComputePool> {
        let count = workers.max(1);
        let shared = Arc::new(Shared {
            queues: Mutex::new(Queues::default()),
            available: Condvar::new(),
            urgent_ready: Condvar::new(),
            helpers,
            budget,
        });
        if let Some(budget) = &shared.budget {
            let weak = Arc::downgrade(&shared);
            budget.set_wake(Box::new(move || {
                if let Some(shared) = weak.upgrade() {
                    // Under the lock: a worker that just found the budget closed is waiting
                    // before this notification can happen.
                    let _queues = shared.lock();
                    shared.available.notify_all();
                }
            }));
        }
        let handles = spawn_threads(&shared, count, 0)?;
        Ok(ComputePool {
            shared,
            workers: Mutex::new(handles),
            count,
        })
    }
}

/// Starts the `workers` workers and the tick helpers of `generation`.
fn spawn_threads(
    shared: &Arc<Shared>,
    workers: usize,
    generation: u64,
) -> io::Result<Vec<JoinHandle<()>>> {
    let roles = (0..workers)
        .map(|index| (Role::Worker, index))
        .chain((0..shared.helpers).map(|index| (Role::Helper, index)));
    roles
        .map(|(role, index)| spawn_thread(shared, role, index, generation))
        .collect()
}

/// Starts thread `index` of `role` and `generation`.
fn spawn_thread(
    shared: &Arc<Shared>,
    role: Role,
    index: usize,
    generation: u64,
) -> io::Result<JoinHandle<()>> {
    let shared = Arc::clone(shared);
    let name = match role {
        Role::Worker => format!("Compute #{index}"),
        Role::Helper => format!("Tick helper #{index}"),
    };
    thread::Builder::new().name(name).spawn(move || match role {
        Role::Worker => {
            if let Some(budget) = &shared.budget {
                budget.worker_started();
            }
            worker(&shared, generation);
        }
        Role::Helper => helper(&shared, generation),
    })
}

/// A tick helper: runs urgent jobs as they come.
fn helper(shared: &Shared, generation: u64) {
    let mut queues = shared.lock();
    loop {
        if queues.generation != generation {
            return;
        }
        match queues.urgent.pop_front() {
            Some(job) => {
                queues.jobs_taken += 1;
                drop(queues);
                run_job(job);
                queues = shared.lock();
            }
            None if queues.stopping => return,
            None => {
                queues = shared
                    .urgent_ready
                    .wait(queues)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
    }
}

fn worker(shared: &Shared, generation: u64) {
    let mut queues = shared.lock();
    loop {
        if queues.generation != generation {
            // Replaced by a fresh thread: ending this one frees what it kept.
            return;
        }
        let urgent = if shared.helpers == 0 {
            queues.urgent.pop_front()
        } else {
            None
        };
        let job = match urgent {
            Some(job) => job,
            None if !queues.background.is_empty()
                && let Some(wait) = shared.budget.as_ref().and_then(|b| b.background_wait()) =>
            {
                queues = shared
                    .available
                    .wait_timeout(queues, wait)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
                continue;
            }
            None => match queues.background.pop() {
                Some(background) => background.job,
                None if queues.stopping => return,
                None => {
                    queues = shared
                        .available
                        .wait(queues)
                        .unwrap_or_else(PoisonError::into_inner);
                    continue;
                }
            },
        };
        queues.jobs_taken += 1;
        drop(queues);
        run_job(job);
        queues = shared.lock();
    }
}

fn run_job(job: Job) {
    if panic::catch_unwind(AssertUnwindSafe(job)).is_err() {
        log::error!("A compute job panicked; the job was dropped");
    }
}

impl Scheduler for ComputePool {
    fn workers(&self) -> usize {
        self.count
    }

    fn phase_helpers(&self) -> usize {
        match self.shared.helpers {
            0 => self.count,
            helpers => helpers,
        }
    }

    fn spawn_urgent(&self, job: Job) {
        self.shared.lock().urgent.push_back(job);
        if self.shared.helpers == 0 {
            self.shared.available.notify_one();
        } else {
            self.shared.urgent_ready.notify_one();
        }
    }

    fn spawn_background(&self, priority: u32, job: Job) {
        let mut queues = self.shared.lock();
        let seq = queues.next_seq;
        queues.next_seq += 1;
        queues.background.push(BackgroundJob { priority, seq, job });
        drop(queues);
        self.shared.available.notify_one();
    }

    fn help(&self) -> bool {
        let job = self.shared.lock().urgent.pop_front();
        match job {
            Some(job) => {
                // Scope jobs catch their own panics; this only guards foreign urgent jobs.
                run_job(job);
                true
            }
            None => false,
        }
    }

    fn spawn_thread(&self, name: String, body: Job) -> io::Result<JoinHandle<()>> {
        thread::Builder::new().name(name).spawn(body)
    }

    fn release_memory(&self) {
        let mut workers = self.workers.lock().unwrap_or_else(PoisonError::into_inner);
        let mut queues = self.shared.lock();
        if queues.jobs_taken == 0 || queues.stopping {
            // Fresh threads keep nothing.
            return;
        }
        // Fresh workers first (they wait for the queue lock held here); the old ones are
        // only retired once all of them exist, and end after the job they run.
        let generation = queues.generation + 1;
        match spawn_threads(&self.shared, self.count, generation) {
            Ok(fresh) => {
                queues.generation = generation;
                queues.jobs_taken = 0;
                workers.retain(|worker| !worker.is_finished());
                workers.extend(fresh);
            }
            // Those that did start end at once: their generation never begins.
            Err(e) => log::warn!("Cannot replace the compute workers: {e}"),
        }
        drop(queues);
        self.shared.available.notify_all();
        self.shared.urgent_ready.notify_all();
    }

    fn shutdown(&self) {
        self.shared.lock().stopping = true;
        self.shared.available.notify_all();
        self.shared.urgent_ready.notify_all();
        let handles =
            std::mem::take(&mut *self.workers.lock().unwrap_or_else(PoisonError::into_inner));
        for handle in handles {
            let _ = handle.join();
        }
    }
}
