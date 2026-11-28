//! Soak parity runs (#166, [`vanillite_test::soak`]); started by `cargo xtask parity soak`,
//! which compiles the Java recorder and passes its class path.
//!
//! Runs the soak scenarios (`scenarios/soak/*.json`, or those of `--dir`; all or the named
//! ones) `--jobs` at a time and writes `<out>/<name>.md` per scenario and `<out>/summary.md`.
//! Exit code 1 if a scenario failed or diverged from Vanilla other than known. `cargo xtask
//! parity worlds` (#244) runs its generated world scenarios with `--dir` and `--title`.
//!
//! ```text
//! parity-soak --java <java> --agent <agent.jar> --classpath <cp> --work-dir <dir> --out <dir>
//!             [--dir <scenario dir>] [--title <summary title>] [--jobs N] [--minutes N]
//!             [<scenario>...]
//! ```

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use vanillite_test::scenario::Scenario;
use vanillite_test::soak::{self, Java, Report, Verdict};

struct Args {
    java: Java,
    out: PathBuf,
    jobs: usize,
    minutes: Option<u64>,
    only: Vec<String>,
    dir: PathBuf,
    title: String,
}

fn args() -> Result<Args, String> {
    let mut java = Java {
        java: PathBuf::from("java"),
        agent: PathBuf::new(),
        classpath: String::new(),
        work_dir: PathBuf::from("."),
    };
    let mut out = PathBuf::from("target/parity-soak");
    let mut jobs = 2;
    let mut minutes = None;
    let mut only = Vec::new();
    let mut dir = Scenario::soak_dir();
    let mut title = "Soak parity runs".to_owned();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--java" => java.java = value()?.into(),
            "--agent" => java.agent = value()?.into(),
            "--classpath" => java.classpath = value()?,
            "--work-dir" => java.work_dir = value()?.into(),
            "--out" => out = value()?.into(),
            "--dir" => dir = value()?.into(),
            "--title" => title = value()?,
            "--jobs" => jobs = value()?.parse().map_err(|e| format!("--jobs: {e}"))?,
            "--minutes" => {
                minutes = Some(value()?.parse().map_err(|e| format!("--minutes: {e}"))?);
            }
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            s => only.push(s.to_owned()),
        }
    }
    Ok(Args {
        java,
        out,
        jobs: jobs.max(1),
        minutes,
        only,
        dir,
        title,
    })
}

fn main() -> std::process::ExitCode {
    let args = match args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("parity-soak: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    let dir = args.dir.clone();
    let scenarios = match Scenario::all_in(&dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("parity-soak: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    let todo: Vec<Scenario> = scenarios
        .into_iter()
        .filter(|s| args.only.is_empty() || args.only.contains(&s.name))
        .collect();
    if todo.is_empty() {
        eprintln!("parity-soak: no scenario in {} matches", dir.display());
        return std::process::ExitCode::from(2);
    }
    let _ = std::fs::create_dir_all(&args.out);
    let started = Instant::now();
    let deadline = args.minutes.map(|m| started + Duration::from_secs(m * 60));
    let next = Mutex::new(0usize);
    let reports: Mutex<Vec<(usize, Report)>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..args.jobs.min(todo.len()) {
            scope.spawn(|| {
                loop {
                    let i = {
                        let mut n = next
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        *n += 1;
                        *n - 1
                    };
                    let Some(s) = todo.get(i) else { return };
                    let file = dir.join(format!("{}.json", s.name));
                    println!("{}: {} ticks", s.name, s.ticks);
                    let report = soak::run(s, &file, &args.java, &args.out, deadline);
                    println!("{}", report.row());
                    reports
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push((i, report));
                }
            });
        }
    });
    let mut reports = reports
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    reports.sort_by_key(|r| r.0);
    let mut summary = format!(
        "{} ({} scenarios, {:.1} min)\n\n| scenario | ticks | result | activity per 100 ticks (Vanillite) | \
         Vanilla recording | Vanillite |\n|---|---|---|---|---|---|\n",
        args.title,
        reports.len(),
        started.elapsed().as_secs_f64() / 60.0
    );
    for (_, r) in &reports {
        summary.push_str(&r.row());
        summary.push('\n');
    }
    let count = |f: &dyn Fn(&Report) -> bool| reports.iter().filter(|(_, r)| f(r)).count();
    let _ = write!(
        summary,
        "\n**{} same as Vanilla and active, {} inactive (same, but nothing changes in the \
         region), {} diverging, {} errors or stopped** of {}.\n",
        count(&|r| r.verdict == Verdict::Same && !r.activity.idle()),
        count(&|r| r.verdict == Verdict::Same && r.activity.idle()),
        count(&|r| matches!(r.verdict, Verdict::Diverges { .. })),
        count(&|r| matches!(r.verdict, Verdict::Error(_) | Verdict::Stopped(_))),
        reports.len()
    );
    summary.push_str("\nDetails per scenario: `<scenario>.md`.\n");
    let path = args.out.join("summary.md");
    if let Err(e) = std::fs::write(&path, &summary) {
        eprintln!("parity-soak: {}: {e}", path.display());
    }
    print!("{summary}");
    if reports.iter().any(|(_, r)| r.failed()) {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}
