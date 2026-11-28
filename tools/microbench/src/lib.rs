//! Minimal harness for the micro benchmarks (`[[bench]]` targets with `harness = false`).
//!
//! `cargo bench` passes `--bench`: every benchmark warms up, calibrates its iterations per
//! sample and prints min, median and max time per iteration. Without `--bench` (`cargo test
//! --benches`) every routine runs once as a smoke test. A free argument filters by substring.

use std::time::{Duration, Instant};

pub use std::hint::black_box;

/// Time one sample should take after calibration.
const SAMPLE_TIME: Duration = Duration::from_millis(5);
/// Upper bound for the calibration (warm-up) of one benchmark.
const WARM_UP: Duration = Duration::from_millis(500);
/// Samples per benchmark, unless [`MEASURE_TIME`] runs out first.
const SAMPLES: usize = 100;
const MEASURE_TIME: Duration = Duration::from_secs(3);

/// Input size hint of [`Bencher::iter_batched_ref`]; inputs are always prepared per sample.
#[derive(Debug, Clone, Copy)]
pub enum BatchSize {
    SmallInput,
}

/// The benchmark runner: command line mode and filter.
#[derive(Debug)]
pub struct Bench {
    measure: bool,
    filter: Option<String>,
}

impl Default for Bench {
    fn default() -> Self {
        let mut bench = Self {
            measure: false,
            filter: None,
        };
        for arg in std::env::args().skip(1) {
            if arg == "--bench" {
                bench.measure = true;
            } else if !arg.starts_with('-') {
                bench.filter = Some(arg);
            }
        }
        bench
    }
}

impl Bench {
    /// Runs the benchmark `name` (skipped unless it matches the filter).
    pub fn bench_function(
        &mut self,
        name: impl AsRef<str>,
        f: impl FnOnce(&mut Bencher),
    ) -> &mut Self {
        let name = name.as_ref();
        if self
            .filter
            .as_ref()
            .is_some_and(|filter| !name.contains(filter.as_str()))
        {
            return self;
        }
        let mut bencher = Bencher {
            measure: self.measure,
            samples: Vec::new(),
        };
        f(&mut bencher);
        if self.measure {
            bencher.report(name);
        } else {
            println!("{name}: ok");
        }
        self
    }

    /// Benchmarks named `<name>/<id>`.
    pub fn benchmark_group(&mut self, name: impl Into<String>) -> BenchmarkGroup<'_> {
        BenchmarkGroup {
            bench: self,
            name: name.into(),
        }
    }
}

/// Benchmarks sharing a name prefix.
#[derive(Debug)]
pub struct BenchmarkGroup<'a> {
    bench: &'a mut Bench,
    name: String,
}

impl BenchmarkGroup<'_> {
    pub fn bench_function(
        &mut self,
        id: impl AsRef<str>,
        f: impl FnOnce(&mut Bencher),
    ) -> &mut Self {
        self.bench
            .bench_function(format!("{}/{}", self.name, id.as_ref()), f);
        self
    }

    pub fn finish(self) {}
}

/// Times one routine.
#[derive(Debug)]
pub struct Bencher {
    measure: bool,
    /// Time per iteration of every sample.
    samples: Vec<Duration>,
}

impl Bencher {
    /// Times `routine`.
    pub fn iter<O>(&mut self, mut routine: impl FnMut() -> O) {
        self.run(|n| {
            let start = Instant::now();
            for _ in 0..n {
                black_box(routine());
            }
            start.elapsed()
        });
    }

    /// Times `routine` on a fresh input from `setup` per iteration; `setup` and dropping the
    /// inputs are not timed.
    pub fn iter_batched_ref<I, O>(
        &mut self,
        mut setup: impl FnMut() -> I,
        mut routine: impl FnMut(&mut I) -> O,
        _size: BatchSize,
    ) {
        self.run(|n| {
            let mut inputs: Vec<I> = (0..n).map(|_| setup()).collect();
            let start = Instant::now();
            for input in &mut inputs {
                black_box(routine(input));
            }
            let elapsed = start.elapsed();
            drop(inputs);
            elapsed
        });
    }

    /// `sample(n)` runs `n` iterations and returns the time they took.
    fn run(&mut self, mut sample: impl FnMut(u32) -> Duration) {
        if !self.measure {
            sample(1);
            return;
        }
        let mut n = 1u32;
        let warm_up = Instant::now();
        loop {
            let elapsed = sample(n);
            if elapsed >= SAMPLE_TIME || warm_up.elapsed() >= WARM_UP || n == u32::MAX {
                break;
            }
            n = n.saturating_mul(2);
        }
        let start = Instant::now();
        while self.samples.len() < SAMPLES
            && (self.samples.is_empty() || start.elapsed() < MEASURE_TIME)
        {
            self.samples.push(sample(n) / n);
        }
    }

    fn report(&mut self, name: &str) {
        if self.samples.is_empty() {
            println!("{name}: no samples");
            return;
        }
        self.samples.sort_unstable();
        let median = self.samples[self.samples.len() / 2];
        println!(
            "{name:<48} time: [{:?} {:?} {:?}] ({} samples)",
            self.samples[0],
            median,
            self.samples[self.samples.len() - 1],
            self.samples.len()
        );
    }
}

/// Defines `fn $name(&mut Bench)` running the given benchmark functions.
#[macro_export]
macro_rules! bench_group {
    ($name:ident, $($function:path),+ $(,)?) => {
        fn $name(bench: &mut $crate::Bench) {
            $($function(bench);)+
        }
    };
}

/// Defines `main` running the given groups.
#[macro_export]
macro_rules! bench_main {
    ($($group:path),+ $(,)?) => {
        fn main() {
            let mut bench = $crate::Bench::default();
            $($group(&mut bench);)+
        }
    };
}
