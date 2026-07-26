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
        execute_batch(
            &parallel,
            args,
            jobs(args.jobs),
            &cancel,
            &mut summary,
            reporter.as_mut(),
        )?;
    }

    summary.duration = started.elapsed();
    reporter.run_finished(&summary)?;

    Ok(if summary.is_green() {
        Exit::Ok
    } else {
        Exit::TestsFailed
    })
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
    let machine_format = if ctx.format == Format::Human {
        Format::Json
    } else {
        ctx.format
    };

    Ok(Box::new(Multi(vec![
        reporter_for(Format::Human, ctx, Box::new(std::io::stdout())),
        reporter_for(machine_format, ctx, Box::new(file)),
    ])))
}

fn jobs(requested: usize) -> usize {
    if requested > 0 {
        return requested;
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
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

fn execute_once(
    test: &Test,
    timeout: Option<Duration>,
    sink: &crate::scripting_registration::OutputSink,
) -> Result<(), Failure> {
    if !test.script.exists() {
        return Err(Failure::Error(format!(
            "script {} does not exist",
            test.script.display()
        )));
    }

    let source = match fs::read_to_string(&test.script) {
        Ok(s) => s,
        Err(e) => {
            return Err(Failure::Error(format!(
                "reading {}: {e}",
                test.script.display()
            )));
        }
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

    // Writes a script next to the other runner fixtures; names must be unique
    // because `cargo test` runs these in one process, in parallel.
    fn script(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("snag-runner-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn with_script(id: &str, path: PathBuf) -> Test {
        Test {
            script: path,
            ..dummy(id)
        }
    }

    fn args() -> RunArgs {
        RunArgs {
            selection: crate::SelectionArgs {
                paths: vec![],
                filter: vec![],
                tag: vec![],
                exclude: vec![],
                regex: false,
            },
            jobs: 1,
            fail_fast: false,
            timeout: None,
            retries: 0,
            seed: None,
            dry_run: false,
            report: None,
        }
    }

    #[derive(Default)]
    struct Recorder {
        started: Vec<String>,
        finished: Vec<Outcome>,
        // Makes test_finished fail once, for the reporter-error path.
        fail_on: Option<String>,
    }

    impl Recorder {
        fn finished_ids(&self) -> Vec<String> {
            self.finished.iter().map(|o| o.test.id.clone()).collect()
        }

        fn status_of(&self, id: &str) -> Status {
            self.finished
                .iter()
                .find(|o| o.test.id == id)
                .unwrap_or_else(|| panic!("{id} never finished"))
                .status
        }
    }

    impl Reporter for Recorder {
        fn test_started(&mut self, test: &Test) -> anyhow::Result<()> {
            self.started.push(test.id.clone());
            Ok(())
        }

        fn test_finished(&mut self, outcome: &Outcome) -> anyhow::Result<()> {
            self.finished.push(outcome.clone());
            if self.fail_on.as_deref() == Some(outcome.test.id.as_str()) {
                anyhow::bail!("reporter exploded on {}", outcome.test.id);
            }
            Ok(())
        }
    }

    fn batch(
        tests: &[Test],
        args: &RunArgs,
        workers: usize,
        cancel: &AtomicBool,
    ) -> (anyhow::Result<()>, Summary, Recorder) {
        let mut summary = Summary::default();
        let mut recorder = Recorder::default();
        let result = execute_batch(tests, args, workers, cancel, &mut summary, &mut recorder);
        (result, summary, recorder)
    }

    #[test]
    fn empty_batch_reports_nothing() {
        let (result, summary, recorder) = batch(&[], &args(), 4, &AtomicBool::new(false));
        assert!(result.is_ok());
        assert_eq!(summary.total(), 0);
        assert!(recorder.started.is_empty());
        assert!(recorder.finished.is_empty());
    }

    #[test]
    fn every_test_runs_exactly_once_across_workers() {
        let path = script("pass_once.snag", "let x = 1;");
        let tests: Vec<Test> = (0..8)
            .map(|i| with_script(&format!("t{i}"), path.clone()))
            .collect();

        let (result, summary, recorder) = batch(&tests, &args(), 4, &AtomicBool::new(false));

        assert!(result.is_ok());
        assert_eq!(summary.passed, 8);
        assert_eq!(summary.total(), 8);

        let mut got = recorder.finished_ids();
        got.sort();
        assert_eq!(got, ids(&tests));
        assert_eq!(recorder.started.len(), 8);
    }

    #[test]
    fn more_workers_than_tests_is_fine() {
        let path = script("pass_few.snag", "let x = 1;");
        let tests = vec![
            with_script("t0", path.clone()),
            with_script("t1", path.clone()),
        ];

        let (result, summary, recorder) = batch(&tests, &args(), 32, &AtomicBool::new(false));

        assert!(result.is_ok());
        assert_eq!(summary.passed, 2);
        assert_eq!(recorder.finished.len(), 2);
    }

    #[test]
    fn zero_workers_still_runs_the_batch() {
        let path = script("pass_zero_workers.snag", "let x = 1;");
        let tests = vec![with_script("t0", path)];

        let (result, summary, _) = batch(&tests, &args(), 0, &AtomicBool::new(false));

        assert!(result.is_ok());
        assert_eq!(summary.passed, 1);
    }

    #[test]
    fn a_missing_script_fails_the_test() {
        // dummy() points at a script that was never written.
        let tests = vec![dummy("t0")];

        let (result, summary, recorder) = batch(&tests, &args(), 1, &AtomicBool::new(false));

        assert!(result.is_ok());
        assert_eq!(summary.failed, 1);
        let outcome = &recorder.finished[0];
        assert_eq!(outcome.status, Status::Failed);
        assert!(
            outcome
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("does not exist"),
            "unexpected message: {:?}",
            outcome.message
        );
    }

    #[test]
    fn failures_do_not_stop_the_batch_without_fail_fast() {
        let ok = script("pass_no_ff.snag", "let x = 1;");
        let bad = script("throw_no_ff.snag", r#"throw "boom";"#);
        let tests = vec![
            with_script("t0", bad),
            with_script("t1", ok.clone()),
            with_script("t2", ok.clone()),
            with_script("t3", ok),
        ];

        let (result, summary, recorder) = batch(&tests, &args(), 1, &AtomicBool::new(false));

        assert!(result.is_ok());
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.passed, 3);
        assert_eq!(summary.skipped, 0);
        assert_eq!(recorder.started.len(), 4);
    }

    #[test]
    fn fail_fast_skips_the_remaining_tests() {
        let ok = script("pass_ff.snag", "let x = 1;");
        let bad = script("throw_ff.snag", r#"throw "boom";"#);
        let tests = vec![
            with_script("t0", bad),
            with_script("t1", ok.clone()),
            with_script("t2", ok.clone()),
            with_script("t3", ok),
        ];

        let mut args = args();
        args.fail_fast = true;
        let cancel = AtomicBool::new(false);

        // One worker keeps the order deterministic: t0 fails, t1..t3 are skipped.
        let (result, summary, recorder) = batch(&tests, &args, 1, &cancel);

        assert!(result.is_ok());
        assert!(cancel.load(Ordering::SeqCst));
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.skipped, 3);
        assert_eq!(summary.passed, 0);
        assert_eq!(recorder.started, vec!["t0"]);
        assert_eq!(recorder.status_of("t3"), Status::Skipped);
        assert_eq!(
            recorder.finished[3].message.as_deref(),
            Some("skipped after an earlier failure")
        );
    }

    #[test]
    fn an_already_cancelled_batch_skips_everything() {
        let path = script("pass_cancelled.snag", "let x = 1;");
        let tests: Vec<Test> = (0..3)
            .map(|i| with_script(&format!("t{i}"), path.clone()))
            .collect();

        let (result, summary, recorder) = batch(&tests, &args(), 2, &AtomicBool::new(true));

        assert!(result.is_ok());
        assert_eq!(summary.skipped, 3);
        assert_eq!(summary.passed, 0);
        assert!(recorder.started.is_empty());
    }

    #[test]
    fn a_reporter_error_is_returned_and_cancels_the_batch() {
        let path = script("pass_reporter_err.snag", "let x = 1;");
        let tests: Vec<Test> = (0..4)
            .map(|i| with_script(&format!("t{i}"), path.clone()))
            .collect();

        let mut summary = Summary::default();
        let mut recorder = Recorder {
            fail_on: Some("t0".into()),
            ..Recorder::default()
        };
        let cancel = AtomicBool::new(false);

        let result = execute_batch(&tests, &args(), 1, &cancel, &mut summary, &mut recorder);

        let err = result.unwrap_err();
        assert!(err.to_string().contains("reporter exploded on t0"));
        assert!(cancel.load(Ordering::SeqCst));
        // Every test still gets an outcome, whether it ran or was drained as
        // skipped — how many of each depends on how far the worker raced ahead
        // before the main thread saw the error.
        assert_eq!(summary.total(), 4);
        assert_eq!(recorder.finished.len(), 4);
        assert_eq!(recorder.started.first().map(String::as_str), Some("t0"));
    }

    #[test]
    fn a_failing_test_is_retried_before_it_is_reported() {
        let bad = script("throw_retry.snag", r#"throw "boom";"#);
        let tests = vec![with_script("t0", bad)];

        let mut args = args();
        args.retries = 2;

        let (result, summary, recorder) = batch(&tests, &args, 1, &AtomicBool::new(false));

        assert!(result.is_ok());
        assert_eq!(summary.failed, 1);
        assert_eq!(recorder.finished.len(), 1);
        assert_eq!(recorder.finished[0].attempts, 3);
    }

    #[test]
    fn a_slow_test_times_out() {
        let slow = script("slow_timeout.snag", "let i = 0; while true { i += 1; }");
        let tests = vec![with_script("t0", slow)];

        let mut args = args();
        args.timeout = Some(Duration::from_millis(50));

        let (result, summary, recorder) = batch(&tests, &args, 1, &AtomicBool::new(false));

        assert!(result.is_ok());
        assert_eq!(summary.timed_out, 1);
        assert_eq!(recorder.finished[0].status, Status::TimedOut);
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
