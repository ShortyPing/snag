use std::fs;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use rhai::{Dynamic, Engine, EvalAltResult, Scope};

use crate::discovery::discover;
use crate::manifest::Test;
use crate::report::{Multi, Outcome, Reporter, Status, Summary, reporter_for};
use crate::scripting_registration::{
    new_sink, register_assertions, register_debug, register_env, register_http,
};
use crate::{Ctx, Exit, Format, RunArgs};

// Ops between deadline checks inside the interpreter.
const PROGRESS_INTERVAL: u64 = 2_000;

pub fn run(ctx: &Ctx, args: &RunArgs) -> anyhow::Result<Exit> {
    let mut tests = discover(&args.selection)?;

    if let Some(seed) = args.seed {
        shuffle(&mut tests, seed);
    }

    let mut reporter = build_reporter(ctx, args)?;
    reporter.run_started(&tests)?;

    let started = Instant::now();
    let mut summary = Summary::default();

    if args.dry_run {
        for test in &tests {
            let outcome = Outcome {
                test: test.clone(),
                status: Status::Skipped,
                message: Some("dry run".into()),
                duration: Duration::ZERO,
                attempts: 0,
                output: vec![],
            };
            summary.record(outcome.status);
            reporter.test_finished(&outcome)?;
        }
    } else {
        let (serial, parallel): (Vec<Test>, Vec<Test>) =
            tests.iter().cloned().partition(|t| !t.parallel_safe);

        let cancel = AtomicBool::new(false);
        execute_batch(&serial, args, 1, &cancel, &mut summary, reporter.as_mut())?;
        execute_batch(&parallel, args, jobs(args.jobs), &cancel, &mut summary, reporter.as_mut())?;
    }

    summary.duration = started.elapsed();
    reporter.run_finished(&summary)?;

    Ok(if summary.is_green() { Exit::Ok } else { Exit::TestsFailed })
}

pub fn check(ctx: &Ctx, tests: &[Test]) -> anyhow::Result<Exit> {
    let mut errors = 0;

    for test in tests {
        if let Err(e) = compile(test) {
            errors += 1;
            eprintln!("error: {}: {e}", test.qualified_id());
        } else if ctx.verbose > 0 {
            eprintln!("ok: {}", test.qualified_id());
        }
    }

    if errors > 0 {
        anyhow::bail!("{errors} of {} test(s) failed to compile", tests.len());
    }

    if !ctx.quiet {
        println!("checked {} test(s): no errors", tests.len());
    }
    Ok(Exit::Ok)
}

fn compile(test: &Test) -> anyhow::Result<()> {
    if !test.script.exists() {
        anyhow::bail!("script {} does not exist", test.script.display());
    }
    let source = fs::read_to_string(&test.script)?;

    let mut engine = Engine::new();
    register_http(&mut engine, Client::new());
    register_debug(&mut engine, new_sink());
    register_assertions(&mut engine);
    register_env(&mut engine);

    engine
        .compile(&source)
        .map_err(|e| anyhow::anyhow!("{} ({})", e, test.script.display()))?;
    Ok(())
}

fn build_reporter(ctx: &Ctx, args: &RunArgs) -> anyhow::Result<Box<dyn Reporter>> {
    let Some(path) = &args.report else {
        return Ok(reporter_for(ctx.format, ctx, Box::new(std::io::stdout())));
    };

    // --report splits the two streams: humans on the terminal, machine report
    // in the file. `--format human` there would be useless, so the file gets JSON.
    let file = std::fs::File::create(path)
        .map_err(|e| anyhow::anyhow!("cannot write report to {}: {e}", path.display()))?;
    let machine_format = if ctx.format == Format::Human { Format::Json } else { ctx.format };

    Ok(Box::new(Multi(vec![
        reporter_for(Format::Human, ctx, Box::new(std::io::stdout())),
        reporter_for(machine_format, ctx, Box::new(file)),
    ])))
}

fn jobs(requested: usize) -> usize {
    if requested > 0 {
        return requested;
    }
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

enum Event {
    Started(Test),
    Finished(Box<Outcome>),
}

// Runs tests across a thread pool; events come back over a channel so the
// reporter only ever runs on this thread, no locking.
fn execute_batch(
    tests: &[Test],
    args: &RunArgs,
    workers: usize,
    cancel: &AtomicBool,
    summary: &mut Summary,
    reporter: &mut dyn Reporter,
) -> anyhow::Result<()> {
    if tests.is_empty() {
        return Ok(());
    }

    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<Event>();
    let workers = workers.min(tests.len()).max(1);

    let mut pending: anyhow::Result<()> = Ok(());

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let next = &next;
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some(test) = tests.get(index) else { break };

                    // After a fail-fast trip, drain the rest as skipped rather
                    // than dropping them silently.
                    if cancel.load(Ordering::SeqCst) {
                        let _ = tx.send(Event::Finished(Box::new(Outcome {
                            test: test.clone(),
                            status: Status::Skipped,
                            message: Some("skipped after an earlier failure".into()),
                            duration: Duration::ZERO,
                            attempts: 0,
                            output: vec![],
                        })));
                        continue;
                    }

                    let _ = tx.send(Event::Started(test.clone()));
                    let outcome = execute(test, args);
                    if outcome.status.is_failure() && args.fail_fast {
                        cancel.store(true, Ordering::SeqCst);
                    }
                    let _ = tx.send(Event::Finished(Box::new(outcome)));
                }
            });
        }
        // Drop our sender so the loop below ends once the workers finish.
        drop(tx);

        for event in rx {
            let result = match event {
                Event::Started(test) => reporter.test_started(&test),
                Event::Finished(outcome) => {
                    summary.record(outcome.status);
                    reporter.test_finished(&outcome)
                }
            };
            if let Err(e) = result {
                pending = Err(e);
                cancel.store(true, Ordering::SeqCst);
            }
        }
    });

    pending
}

// Retries a failed test up to args.retries times, reporting the attempt count.
fn execute(test: &Test, args: &RunArgs) -> Outcome {
    let timeout = args.timeout.or(test.timeout);
    let started = Instant::now();
    let mut attempts = 0;

    loop {
        attempts += 1;
        let sink = new_sink();
        let result = execute_once(test, timeout, &sink);
        let output = sink.lock().map(|g| g.clone()).unwrap_or_default();

        let (status, message) = match result {
            Ok(()) => (Status::Passed, None),
            Err(Failure::TimedOut) => (
                Status::TimedOut,
                Some(format!(
                    "timed out after {}",
                    crate::report::fmt_duration(timeout.unwrap_or_default())
                )),
            ),
            Err(Failure::Error(msg)) => (Status::Failed, Some(msg)),
        };

        if status == Status::Passed || attempts > args.retries {
            return Outcome {
                test: test.clone(),
                status,
                message,
                duration: started.elapsed(),
                attempts,
                output,
            };
        }
    }
}

enum Failure {
    TimedOut,
    Error(String),
}

fn execute_once(test: &Test, timeout: Option<Duration>, sink: &crate::scripting_registration::OutputSink) -> Result<(), Failure> {
    if !test.script.exists() {
        return Err(Failure::Error(format!(
            "script {} does not exist",
            test.script.display()
        )));
    }

    let source = match fs::read_to_string(&test.script) {
        Ok(s) => s,
        Err(e) => return Err(Failure::Error(format!("reading {}: {e}", test.script.display()))),
    };

    // The client needs the deadline too: on_progress can't interrupt a socket
    // that's already blocked on a read.
    let mut builder = Client::builder();
    if let Some(t) = timeout {
        builder = builder.timeout(t);
    }
    let client = match builder.build() {
        Ok(c) => c,
        Err(e) => return Err(Failure::Error(format!("building HTTP client: {e}"))),
    };

    let mut engine = Engine::new();
    register_http(&mut engine, client);
    register_debug(&mut engine, sink.clone());
    register_assertions(&mut engine);
    register_env(&mut engine);

    if let Some(limit) = timeout {
        let deadline = Instant::now() + limit;
        engine.on_progress(move |ops| {
            if ops % PROGRESS_INTERVAL == 0 && Instant::now() >= deadline {
                // Returning any value aborts with ErrorTerminated.
                return Some(Dynamic::UNIT);
            }
            None
        });
    }

    // Push vars as constants so a script can read `base_url` but not reassign it.
    let mut scope = Scope::new();
    for (name, value) in &test.vars {
        scope.push_constant(name.as_str(), value.clone());
    }

    let started = Instant::now();
    match engine.run_with_scope(&mut scope, &source) {
        Ok(()) => Ok(()),
        Err(e) => Err(classify(*e, timeout, started)),
    }
}

// Tell a timeout apart from a real script failure, keeping the script's line/col.
fn classify(err: EvalAltResult, timeout: Option<Duration>, started: Instant) -> Failure {
    let timed_out = matches!(err, EvalAltResult::ErrorTerminated(..))
        || timeout.is_some_and(|t| started.elapsed() >= t);

    if timed_out {
        return Failure::TimedOut;
    }
    Failure::Error(err.to_string())
}

// Fisher-Yates over a seeded xorshift64 — good enough for ordering, no rng dep.
fn shuffle(tests: &mut [Test], seed: u64) {
    let mut state = seed | 1;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for i in (1..tests.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        tests.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn dummy(id: &str) -> Test {
        Test {
            id: id.into(),
            name: id.into(),
            tags: vec![],
            timeout: None,
            parallel_safe: true,
            script: PathBuf::from("t.snag"),
            vars: BTreeMap::new(),
            suite_title: "s".into(),
            suite_path: PathBuf::from("suite.toml"),
        }
    }

    fn ids(tests: &[Test]) -> Vec<String> {
        tests.iter().map(|t| t.id.clone()).collect()
    }

    #[test]
    fn same_seed_gives_same_order() {
        let mut a: Vec<Test> = (0..10).map(|i| dummy(&format!("t{i}"))).collect();
        let mut b = a.clone();
        shuffle(&mut a, 42);
        shuffle(&mut b, 42);
        assert_eq!(ids(&a), ids(&b));
    }

    #[test]
    fn different_seeds_give_different_orders() {
        let mut a: Vec<Test> = (0..10).map(|i| dummy(&format!("t{i}"))).collect();
        let mut b = a.clone();
        shuffle(&mut a, 1);
        shuffle(&mut b, 2);
        assert_ne!(ids(&a), ids(&b));
    }

    #[test]
    fn shuffle_keeps_every_test() {
        let mut a: Vec<Test> = (0..10).map(|i| dummy(&format!("t{i}"))).collect();
        shuffle(&mut a, 7);
        let mut got = ids(&a);
        got.sort();
        assert_eq!(got.len(), 10);
        got.dedup();
        assert_eq!(got.len(), 10);
    }

    #[test]
    fn jobs_zero_means_available_parallelism() {
        assert!(jobs(0) >= 1);
        assert_eq!(jobs(3), 3);
    }
}
