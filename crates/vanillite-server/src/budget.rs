//! The budget service: what the hardware allows, in one place (#141).
//!
//! It knows the CPU cores the server may use (cgroup quota and CPU affinity), the memory limit
//! (cgroup), and whether a tick is running. Producers of background work ask it before they
//! start a job or keep a cache:
//!
//! - The compute pool asks [`Budget::background_wait`] before every background job (chunk
//!   loading and generation, light, saving). With less than two cores ("scarce"), background
//!   jobs start only between ticks (or while the tick waits for one of them, see
//!   [`Budget::set_blocked`]), and only while the process stays below its CPU quota in the
//!   current 100 ms period, so the kernel never throttles the tick.
//! - The compute workers (one per core by default) run at a lower OS priority than the tick
//!   threads ([`Budget::worker_started`]): the tick wins a core whenever both want it, and
//!   generation still gets every core the tick leaves.
//! - Caches size themselves from [`Budget::cache_bytes`].
//! - [`Budget::release_memory`] hands freed memory back to the system (allocator specific,
//!   registered by the binary).
//!
//! Only *when* work runs changes, never its result: the `vanilla` profile stays exact.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// Below this many cores, background work runs only in the tick's spare time.
const SCARCE_CORES: f64 = 2.0;
/// The accounting period of the CPU quota (the kernel's default `cpu.max` period).
const PERIOD: Duration = Duration::from_millis(100);
/// Share of the quota background work may fill in a period before the tick's reserve.
const QUOTA_SHARE: f64 = 0.9;
/// Background work waits for a running tick at most this long.
const MAX_HOLD: Duration = Duration::from_millis(50);

const AVAILABLE_CORES: &str = "available cores";

/// The limits the server runs under.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    /// CPU cores the process may use (fractional with a quota).
    pub cpu: f64,
    /// Where `cpu` comes from.
    pub cpu_source: &'static str,
    /// Memory limit in bytes, if any.
    pub memory: Option<u64>,
    /// Where `memory` comes from.
    pub memory_source: &'static str,
}

impl Limits {
    /// The configured limits (`performance.budget`), detected where they are 0.
    pub fn detect(cpu: f64, memory_mib: u32) -> Self {
        let cores = std::thread::available_parallelism().map_or(1, usize::from) as f64;
        let (cpu, cpu_source) = if cpu > 0.0 {
            (cpu, "performance.budget.cpu")
        } else {
            match cgroup::cpu_quota() {
                // `available_parallelism` already rounds the quota up to whole cores.
                Some(quota) if quota <= cores => (quota, "cgroup CPU quota"),
                _ => (cores, AVAILABLE_CORES),
            }
        };
        let (memory, memory_source) = if memory_mib > 0 {
            (
                Some(u64::from(memory_mib) << 20),
                "performance.budget.memory",
            )
        } else {
            match cgroup::memory_limit() {
                Some(limit) => (Some(limit), "cgroup memory limit"),
                None => (None, "none"),
            }
        };
        Limits {
            cpu,
            cpu_source,
            memory,
            memory_source,
        }
    }

    /// Network I/O threads for `performance.threads.network = 0`: one per two cores, at
    /// least 2; 1 below two cores. Connections write independently, so a crowd's traffic
    /// spreads over the cores, and the other half stays with the tick and its phases.
    pub fn default_network_threads(&self) -> usize {
        if self.cpu < SCARCE_CORES {
            1
        } else {
            ((self.cpu / 2.0).floor() as usize).max(2)
        }
    }

    /// Compute workers for `performance.threads.compute = 0`: one per core, at least one. The
    /// tick threads keep their share through the workers' lower priority
    /// (`performance.threads.compute-nice`).
    pub fn default_workers(&self) -> usize {
        (self.cpu.ceil() as usize).max(1)
    }
}

/// The budget service; shared by the server, its worlds and the compute pool.
pub struct Budget {
    limits: Limits,
    scarce: bool,
    /// The CPU limit is a quota (not whole cores): background work is paced per period.
    paced: bool,
    /// Nice value of the compute workers.
    worker_nice: i32,
    /// A tick is running.
    in_tick: AtomicBool,
    /// When the running tick started, in nanoseconds since `epoch`.
    tick_start: AtomicU64,
    epoch: Instant,
    /// Ticks waiting for background jobs (synchronous chunk loads): those jobs must run.
    blocked: AtomicUsize,
    /// Average tick duration in nanoseconds (exponential, for the CPU reserve).
    tick_nanos: AtomicU64,
    /// Start of the current quota period and the process CPU time at its start.
    period: Mutex<(Instant, Duration)>,
    /// Wakes the compute workers when background work may run again.
    wake: OnceLock<Box<dyn Fn() + Send + Sync>>,
    /// Returns freed memory to the system (registered by the binary for its allocator).
    release: OnceLock<fn()>,
    /// Returns what the calling thread's allocator heap keeps (mimalloc: its free pages), for
    /// allocators that cannot do that for all threads at once.
    thread_release: OnceLock<fn()>,
    /// Bumped by [`Budget::release_memory`]: threads that saw an older round run
    /// `thread_release` at their next idle point ([`Budget::release_thread_memory`]).
    release_round: AtomicU64,
    /// Wake threads that sleep until work comes, so they reach their idle point.
    idle_wakers: Mutex<Vec<Box<dyn Fn() + Send + Sync>>>,
}

thread_local! {
    /// The last [`Budget::release_round`] this thread handed back.
    static RELEASED_ROUND: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl std::fmt::Debug for Budget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Budget")
            .field("limits", &self.limits)
            .field("scarce", &self.scarce)
            .finish_non_exhaustive()
    }
}

impl Budget {
    /// A budget for `limits` whose compute workers run at nice value `worker_nice`.
    pub fn new(limits: Limits, worker_nice: u8) -> Self {
        Budget {
            worker_nice: i32::from(worker_nice),
            scarce: limits.cpu < SCARCE_CORES,
            paced: limits.cpu_source != AVAILABLE_CORES,
            limits,
            in_tick: AtomicBool::new(false),
            tick_start: AtomicU64::new(0),
            epoch: Instant::now(),
            blocked: AtomicUsize::new(0),
            tick_nanos: AtomicU64::new(0),
            period: Mutex::new((Instant::now(), process_cpu())),
            wake: OnceLock::new(),
            release: OnceLock::new(),
            thread_release: OnceLock::new(),
            release_round: AtomicU64::new(0),
            idle_wakers: Mutex::new(Vec::new()),
        }
    }

    /// The limits.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Whether the hardware is scarce (less than two cores): background work then runs only
    /// in the tick's spare time.
    pub fn scarce(&self) -> bool {
        self.scarce
    }

    /// Bytes a cache of `share` (fraction of the memory limit) may keep, between `min` and
    /// `max`; `max` without a memory limit.
    pub fn cache_bytes(&self, share: f64, min: usize, max: usize) -> usize {
        match self.limits.memory {
            Some(limit) => ((limit as f64 * share) as usize).clamp(min, max),
            None => max,
        }
    }

    /// The compute pool's wake-up for [`Budget::leave_tick`] and [`Budget::set_blocked`].
    pub(crate) fn set_wake(&self, wake: Box<dyn Fn() + Send + Sync>) {
        let _ = self.wake.set(wake);
    }

    /// Registers the allocator's way to return freed memory to the system.
    pub fn set_memory_release(&self, release: fn()) {
        let _ = self.release.set(release);
    }

    /// Registers the allocator's way to return what the calling thread keeps: then every
    /// [`Budget::release_memory`] has each thread run it once at its next idle point.
    pub fn set_thread_memory_release(&self, release: fn()) {
        let _ = self.thread_release.set(release);
    }

    /// The per-thread release ([`Budget::set_thread_memory_release`]).
    pub fn thread_release(&self) -> Option<fn()> {
        self.thread_release.get().copied()
    }

    /// Registers a wake-up for threads that sleep until work comes (they then call
    /// [`Budget::release_thread_memory`] and sleep again).
    pub fn add_idle_waker(&self, wake: Box<dyn Fn() + Send + Sync>) {
        self.idle_wakers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(wake);
    }

    /// Returns freed memory to the system, if the allocator supports it. With a per-thread
    /// release ([`Budget::set_thread_memory_release`]) every thread hands back its part too.
    pub fn release_memory(&self) {
        if let Some(release) = self.release.get() {
            release();
        }
        if self.thread_release.get().is_some() {
            let round = self.release_round.fetch_add(1, Ordering::Relaxed) + 1;
            RELEASED_ROUND.set(round);
            for wake in self
                .idle_wakers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
            {
                wake();
            }
        }
    }

    /// Called by long-lived threads where they are idle (before they sleep, between ticks):
    /// hands back what this thread's allocator heap keeps, once per [`Budget::release_memory`].
    /// Otherwise one atomic load.
    #[inline]
    pub fn release_thread_memory(&self) {
        if self.thread_release_due()
            && let Some(release) = self.thread_release.get()
        {
            RELEASED_ROUND.set(self.release_round.load(Ordering::Relaxed));
            release();
        }
    }

    /// Whether [`Budget::release_thread_memory`] would hand back memory on this thread.
    #[inline]
    pub fn thread_release_due(&self) -> bool {
        let round = self.release_round.load(Ordering::Relaxed);
        round != 0 && RELEASED_ROUND.get() != round
    }

    /// [`Budget::release_memory`] for a server nobody plays on: the program's own code and
    /// constant tables leave memory as well. The system reads them from the program file
    /// again when they are used, so this is not for a busy server.
    pub fn release_idle_memory(&self) {
        self.release_memory();
        page_out_program();
    }

    /// A tick starts.
    pub fn enter_tick(&self) {
        self.tick_start
            .store(self.nanos_since_epoch(), Ordering::Relaxed);
        self.in_tick.store(true, Ordering::Release);
    }

    /// The tick ended after `nanos`.
    pub fn leave_tick(&self, nanos: u64) {
        let avg = self.tick_nanos.load(Ordering::Relaxed);
        self.tick_nanos
            .store(avg - avg / 8 + nanos / 8, Ordering::Relaxed);
        self.in_tick.store(false, Ordering::Release);
        if self.scarce {
            self.wake();
        }
    }

    /// The tick waits for background jobs (`true`) or stops waiting (`false`): while it waits,
    /// background work runs even during the tick.
    pub fn set_blocked(&self, blocked: bool) {
        if blocked {
            self.blocked.fetch_add(1, Ordering::AcqRel);
            if self.scarce {
                self.wake();
            }
        } else {
            self.blocked.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn nanos_since_epoch(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    fn wake(&self) {
        if let Some(wake) = self.wake.get() {
            wake();
        }
    }

    /// Whether a compute worker may start a background job now: `None` if it may, else how
    /// long to wait at most before asking again (a wake-up may come earlier).
    pub(crate) fn background_wait(&self) -> Option<Duration> {
        if !self.scarce || self.blocked.load(Ordering::Acquire) > 0 {
            return None;
        }
        if self.in_tick.load(Ordering::Acquire) {
            // The end of the tick wakes the workers. A tick that runs long may be waiting for
            // background work somewhere: it is held back at most `MAX_HOLD`.
            let running = self
                .nanos_since_epoch()
                .saturating_sub(self.tick_start.load(Ordering::Relaxed));
            let left = MAX_HOLD.saturating_sub(Duration::from_nanos(running));
            if !left.is_zero() {
                return Some(left);
            }
        }
        if !self.paced {
            return None;
        }
        // Keep the process below its quota in this period, with the tick's reserve.
        let now = Instant::now();
        let cpu = process_cpu();
        let mut period = self.period.lock().unwrap_or_else(PoisonError::into_inner);
        let elapsed = now.duration_since(period.0);
        if elapsed >= PERIOD {
            *period = (now, cpu);
            return None;
        }
        let used = cpu.saturating_sub(period.1);
        let reserve = Duration::from_nanos(self.tick_nanos.load(Ordering::Relaxed) * 2);
        let allowed = PERIOD
            .mul_f64(self.limits.cpu * QUOTA_SHARE)
            .saturating_sub(reserve);
        if used < allowed {
            None
        } else {
            Some(PERIOD - elapsed)
        }
    }

    /// Called by every compute worker when it starts: lower OS priority, so the tick threads
    /// win the core whenever both want it.
    pub(crate) fn worker_started(&self) {
        if self.worker_nice > 0 {
            lower_thread_priority(self.worker_nice);
        }
    }

    /// One line for the log.
    pub fn describe(&self) -> String {
        let memory = match self.limits.memory {
            Some(bytes) => format!("{} MiB ({})", bytes >> 20, self.limits.memory_source),
            None => "no memory limit".to_owned(),
        };
        format!(
            "{:.2} CPU cores ({}), {memory}{}",
            self.limits.cpu,
            self.limits.cpu_source,
            if self.scarce {
                "; background work runs between ticks"
            } else {
                ""
            }
        )
    }
}

/// Asks the kernel to drop the read-only file pages of the program and its libraries from
/// memory (`MADV_PAGEOUT`). Their contents do not change: they are read from the files again
/// on the next access. The kernel does it for files the process owns or may write; for others
/// nothing happens.
fn page_out_program() {
    #[cfg(target_os = "linux")]
    {
        let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
            return;
        };
        for line in maps.lines() {
            // `start-end perms offset dev inode path`
            let mut fields = line.split_ascii_whitespace();
            let (Some(range), Some(perms)) = (fields.next(), fields.next()) else {
                continue;
            };
            let inode = fields.nth(2);
            if !matches!(perms, "r-xp" | "r--p") || matches!(inode, None | Some("0")) {
                continue;
            }
            let Some((Ok(start), Ok(end))) = range
                .split_once('-')
                .map(|(s, e)| (usize::from_str_radix(s, 16), usize::from_str_radix(e, 16)))
            else {
                continue;
            };
            // SAFETY: the range is a mapping of this process; `MADV_PAGEOUT` only reclaims
            // its pages and keeps what they contain. A failure changes nothing.
            unsafe {
                libc::madvise(
                    start as *mut libc::c_void,
                    end.saturating_sub(start),
                    libc::MADV_PAGEOUT,
                );
            }
        }
    }
}

/// CPU time of the whole process.
fn process_cpu() -> Duration {
    #[cfg(unix)]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, writable timespec.
        if unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) } == 0 {
            return Duration::new(
                u64::try_from(ts.tv_sec).unwrap_or(0),
                u32::try_from(ts.tv_nsec).unwrap_or(0),
            );
        }
    }
    Duration::ZERO
}

/// Lowers the calling thread's scheduling priority (raising the nice value needs no rights).
fn lower_thread_priority(nice: i32) {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: plain syscalls on the calling thread.
        unsafe {
            let tid = libc::syscall(libc::SYS_gettid);
            if let Ok(tid) = libc::id_t::try_from(tid)
                && libc::setpriority(libc::PRIO_PROCESS, tid, nice) != 0
            {
                // Not allowed here (a sandbox or seccomp filter): the worker keeps the
                // default priority.
                log::debug!(
                    "Compute worker priority unchanged: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = nice;
}

/// Reading the limits of the process's cgroup (v2, and v1 as found in older containers).
mod cgroup {
    use std::path::{Path, PathBuf};

    const ROOT: &str = "/sys/fs/cgroup";

    /// The process's cgroup v2 directory and its ancestors up to the mount root.
    fn v2_dirs() -> Vec<PathBuf> {
        let Ok(text) = std::fs::read_to_string("/proc/self/cgroup") else {
            return Vec::new();
        };
        let Some(path) = text.lines().find_map(|l| l.strip_prefix("0::")) else {
            return Vec::new();
        };
        let mut dir = Path::new(ROOT).join(path.trim_start_matches('/'));
        let mut dirs = Vec::new();
        loop {
            dirs.push(dir.clone());
            if dir == Path::new(ROOT) || !dir.pop() || !dir.starts_with(ROOT) {
                break;
            }
        }
        dirs
    }

    fn read(path: &Path) -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_owned())
    }

    /// The smallest CPU quota in cores along the cgroup path.
    pub(super) fn cpu_quota() -> Option<f64> {
        let v2 = v2_dirs()
            .iter()
            .filter_map(|dir| {
                let text = read(&dir.join("cpu.max"))?;
                let mut parts = text.split_whitespace();
                let quota: f64 = parts.next()?.parse().ok()?;
                let period: f64 = parts.next()?.parse().ok()?;
                (period > 0.0).then(|| quota / period)
            })
            .reduce(f64::min);
        v2.or_else(|| {
            let dir = Path::new(ROOT).join("cpu");
            let quota: f64 = read(&dir.join("cpu.cfs_quota_us"))?.parse().ok()?;
            let period: f64 = read(&dir.join("cpu.cfs_period_us"))?.parse().ok()?;
            (quota > 0.0 && period > 0.0).then(|| quota / period)
        })
    }

    /// The smallest memory limit in bytes along the cgroup path (`memory.max`, or
    /// `memory.high` when lower).
    pub(super) fn memory_limit() -> Option<u64> {
        let v2 = v2_dirs()
            .iter()
            .flat_map(|dir| ["memory.max", "memory.high"].map(|f| dir.join(f)))
            .filter_map(|file| read(&file)?.parse::<u64>().ok())
            .min();
        v2.or_else(|| {
            let limit: u64 = read(Path::new("/sys/fs/cgroup/memory/memory.limit_in_bytes"))?
                .parse()
                .ok()?;
            // v1 reports "no limit" as a huge number.
            (limit < 1 << 60).then_some(limit)
        })
    }
}
