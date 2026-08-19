use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use rhai::{AST, CallFnOptions, Dynamic, Engine, EvalAltResult, Scope};

use crate::discovery::discover;
use crate::manifest::{Hook, Test};
use crate::report::{Multi, Outcome, Reporter, Status, Summary, reporter_for};
use crate::scripting_registration::{
    OutputSink, TeardownQueue, new_sink, new_teardown_queue, register_assertions, register_debug,
    register_env, register_http, register_teardown,
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
    let mut engine = Engine::new();
    register_http(&mut engine, Client::new());
    register_debug(&mut engine, new_sink());
    register_assertions(&mut engine);
    register_env(&mut engine);
    register_teardown(&mut engine, new_teardown_queue());

    // Hooks are part of the test: a broken setup script is a broken test.
    let scripts = test
        .setup
        .iter()
        .chain(&test.teardown)
        .map(|h| h.script.as_path())
        .chain(std::iter::once(test.script.as_path()));

    for path in scripts {
        compile_file(&engine, path).map_err(|e| anyhow::anyhow!("{} ({})", e, path.display()))?;
    }
    Ok(())
}

fn compile_file(engine: &Engine, path: &Path) -> anyhow::Result<AST> {
    if !path.exists() {
        anyhow::bail!("script {} does not exist", path.display());
    }
    let source = fs::read_to_string(path)?;
    engine.compile(&source).map_err(|e| anyhow::anyhow!("{e}"))
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
        .map_or(1, std::num::NonZero::get)
}

enum Event {
    Started(Box<Test>),
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

                    let _ = tx.send(Event::Started(Box::new(test.clone())));
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

fn execute_once(test: &Test, timeout: Option<Duration>, sink: &OutputSink) -> Result<(), Failure> {
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

    let teardowns = new_teardown_queue();

    let mut engine = Engine::new();
    register_http(&mut engine, client);
    register_debug(&mut engine, sink.clone());
    register_assertions(&mut engine);
    register_env(&mut engine);
    register_teardown(&mut engine, teardowns.clone());

    // Shared so the teardown phase can be handed a fresh budget: cleanup after
    // a timed-out test would otherwise abort on the very first operation.
    let deadline: Arc<Mutex<Option<Instant>>> =
        Arc::new(Mutex::new(timeout.map(|limit| Instant::now() + limit)));
    if timeout.is_some() {
        let deadline = deadline.clone();
        engine.on_progress(move |ops| {
            if ops % PROGRESS_INTERVAL == 0 {
                let expired = deadline
                    .lock()
                    .is_ok_and(|deadline| deadline.is_some_and(|at| Instant::now() >= at));
                if expired {
                    // Returning any value aborts with ErrorTerminated.
                    return Some(Dynamic::UNIT);
                }
            }
            None
        });
    }

    // Compile everything up front so a broken hook fails before the first
    // request goes out.
    let setup = match compile_hooks(&engine, &test.setup) {
        Ok(asts) => asts,
        Err(msg) => return Err(Failure::Error(format!("setup: {msg}"))),
    };
    let teardown = match compile_hooks(&engine, &test.teardown) {
        Ok(asts) => asts,
        Err(msg) => return Err(Failure::Error(format!("teardown: {msg}"))),
    };
    let body = match compile_file(&engine, &test.script) {
        Ok(ast) => ast,
        Err(e) => return Err(Failure::Error(e.to_string())),
    };

    // Closures registered with on_teardown live in the AST that defined them,
    // so calling them later needs every script's functions in one AST.
    let mut functions = body.clone_functions_only();
    for (_, ast) in setup.iter().chain(&teardown) {
        functions = functions.merge(ast);
    }

    // Push vars as constants so a script can read `base_url` but not reassign it.
    let mut scope = Scope::new();
    for (name, value) in &test.vars {
        scope.push_constant(name.as_str(), value.clone());
    }

    let started = Instant::now();
    let result = run_test_body(&engine, &mut scope, &setup, &body, timeout, started);

    // Cleanup gets its own budget, measured from here.
    let teardown_started = Instant::now();
    if let Some(limit) = timeout
        && let Ok(mut deadline) = deadline.lock()
    {
        *deadline = Some(teardown_started + limit);
    }

    let cleanup = run_teardown(
        &engine,
        &mut scope,
        &teardowns,
        &functions,
        &body,
        &teardown,
        result.is_err(),
        timeout,
        teardown_started,
        sink,
    );

    // A failing test keeps its own message; teardown noise goes to the output.
    result.and(cleanup)
}

fn compile_hooks(engine: &Engine, hooks: &[Hook]) -> Result<Vec<(Hook, AST)>, String> {
    hooks
        .iter()
        .map(|hook| {
            compile_file(engine, &hook.script)
                .map(|ast| (hook.clone(), ast))
                .map_err(|e| format!("{}: {e}", hook.script.display()))
        })
        .collect()
}

// Setup scripts, then a `fn setup()` in the test script, then the test itself.
// All of it shares one scope, so a setup script can hand the test a variable.
fn run_test_body(
    engine: &Engine,
    scope: &mut Scope,
    setup: &[(Hook, AST)],
    body: &AST,
    timeout: Option<Duration>,
    started: Instant,
) -> Result<(), Failure> {
    for (hook, ast) in setup {
        engine
            .run_ast_with_scope(scope, ast)
            .map_err(|e| phase_failure("setup", Some(&hook.script), *e, timeout, started))?;
    }

    if defines(body, "setup") {
        call_hook_fn(engine, scope, body, "setup")
            .map_err(|e| phase_failure("setup", None, *e, timeout, started))?;
    }

    engine
        .run_ast_with_scope(scope, body)
        .map_err(|e| classify(*e, timeout, started))
}

// Unwinds in reverse: on_teardown callbacks last-registered first, then a
// `fn teardown()`, then the teardown scripts (test's own before the suite's).
#[allow(clippy::too_many_arguments)]
fn run_teardown(
    engine: &Engine,
    scope: &mut Scope,
    queue: &TeardownQueue,
    functions: &AST,
    body: &AST,
    hooks: &[(Hook, AST)],
    failed: bool,
    timeout: Option<Duration>,
    started: Instant,
    sink: &OutputSink,
) -> Result<(), Failure> {
    let callbacks = queue.borrow().clone();

    // The first error is what a passing test fails with; the rest would be lost,
    // so they go to the captured output instead.
    let mut first: Option<Failure> = None;
    let mut record = |failure: Failure, sink: &OutputSink| {
        if !failed && first.is_none() {
            first = Some(failure);
            return;
        }
        if let Ok(mut guard) = sink.lock() {
            guard.push(match failure {
                Failure::TimedOut => "teardown timed out".to_string(),
                Failure::Error(msg) => msg,
            });
        }
    };

    for callback in callbacks.iter().rev() {
        if failed && !callback.always {
            continue;
        }
        if let Err(e) = callback.func.call::<Dynamic>(engine, functions, ()) {
            record(phase_failure("teardown", None, *e, timeout, started), sink);
        }
    }

    if defines(body, "teardown")
        && let Err(e) = call_hook_fn(engine, scope, body, "teardown")
    {
        record(phase_failure("teardown", None, *e, timeout, started), sink);
    }

    for (hook, ast) in hooks {
        if failed && !hook.always {
            continue;
        }
        if let Err(e) = engine.run_ast_with_scope(scope, ast) {
            record(
                phase_failure("teardown", Some(&hook.script), *e, timeout, started),
                sink,
            );
        }
    }

    match first {
        Some(failure) => Err(failure),
        None => Ok(()),
    }
}

// eval_ast(false) keeps the top-level statements from running again;
// rewind_scope(false) lets `fn setup()` leave variables behind for the test.
fn call_hook_fn(
    engine: &Engine,
    scope: &mut Scope,
    ast: &AST,
    name: &str,
) -> Result<(), Box<EvalAltResult>> {
    let options = CallFnOptions::new().eval_ast(false).rewind_scope(false);
    engine
        .call_fn_with_options::<Dynamic>(options, scope, ast, name, ())
        .map(|_| ())
}

fn defines(ast: &AST, name: &str) -> bool {
    ast.iter_functions()
        .any(|f| f.name == name && f.params.is_empty())
}

// Same classification as the test body, but the message says which phase and
// which script blew up.
fn phase_failure(
    phase: &str,
    script: Option<&Path>,
    err: EvalAltResult,
    timeout: Option<Duration>,
    started: Instant,
) -> Failure {
    match classify(err, timeout, started) {
        Failure::TimedOut => Failure::TimedOut,
        Failure::Error(msg) => Failure::Error(match script {
            Some(path) => format!("{phase} {}: {msg}", path.display()),
            None => format!("{phase}: {msg}"),
        }),
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
            setup: vec![],
            teardown: vec![],
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

    fn hook(path: PathBuf, always: bool) -> Hook {
        Hook {
            script: path,
            always,
        }
    }

    // Runs one test on its own and hands back the outcome.
    fn run_one(test: Test) -> Outcome {
        let (result, _, recorder) = batch(&[test], &args(), 1, &AtomicBool::new(false));
        result.unwrap();
        recorder.finished.into_iter().next().unwrap()
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
    fn a_setup_script_shares_its_scope_with_the_test() {
        let setup = script("setup_scope.snag", r#"let token = "s3cret";"#);
        let body = script("setup_scope_body.snag", r#"print("token=" + token);"#);

        let test = Test {
            setup: vec![hook(setup, true)],
            ..with_script("t0", body)
        };
        let outcome = run_one(test);

        assert_eq!(outcome.status, Status::Passed, "{:?}", outcome.message);
        assert_eq!(outcome.output, ["token=s3cret".to_string()]);
    }

    #[test]
    fn a_failing_setup_fails_the_test_without_running_it() {
        let setup = script("setup_boom.snag", r#"throw "no database";"#);
        let body = script("setup_boom_body.snag", r#"print("ran");"#);

        let test = Test {
            setup: vec![hook(setup, true)],
            ..with_script("t0", body)
        };
        let outcome = run_one(test);

        assert_eq!(outcome.status, Status::Failed);
        let message = outcome.message.unwrap_or_default();
        assert!(message.starts_with("setup "), "{message}");
        assert!(message.contains("setup_boom.snag"), "{message}");
        assert!(message.contains("no database"), "{message}");
        assert!(outcome.output.is_empty(), "{:?}", outcome.output);
    }

    #[test]
    fn a_missing_setup_script_fails_the_test() {
        let body = script("missing_setup_body.snag", "let x = 1;");

        let test = Test {
            setup: vec![hook(PathBuf::from("nowhere.snag"), true)],
            ..with_script("t0", body)
        };
        let outcome = run_one(test);

        assert_eq!(outcome.status, Status::Failed);
        assert!(
            outcome
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("does not exist"),
            "{:?}",
            outcome.message
        );
    }

    #[test]
    fn teardown_scripts_run_in_order_after_the_test() {
        let first = script("teardown_first.snag", r#"print("first");"#);
        let second = script("teardown_second.snag", r#"print("second");"#);
        let body = script("teardown_order_body.snag", r#"print("body");"#);

        let test = Test {
            teardown: vec![hook(first, true), hook(second, true)],
            ..with_script("t0", body)
        };
        let outcome = run_one(test);

        assert_eq!(outcome.status, Status::Passed);
        assert_eq!(outcome.output, ["body", "first", "second"]);
    }

    #[test]
    fn a_failed_test_skips_teardown_that_is_not_always() {
        let always = script("teardown_always.snag", r#"print("always");"#);
        let sometimes = script("teardown_sometimes.snag", r#"print("sometimes");"#);
        let body = script("teardown_failed_body.snag", r#"throw "boom";"#);

        let test = Test {
            teardown: vec![hook(sometimes, false), hook(always, true)],
            ..with_script("t0", body)
        };
        let outcome = run_one(test);

        assert_eq!(outcome.status, Status::Failed);
        assert_eq!(outcome.output, ["always"]);
    }

    #[test]
    fn a_failing_teardown_fails_an_otherwise_passing_test() {
        let bad = script("teardown_boom.snag", r#"throw "cleanup failed";"#);
        let body = script("teardown_boom_body.snag", "let x = 1;");

        let test = Test {
            teardown: vec![hook(bad, true)],
            ..with_script("t0", body)
        };
        let outcome = run_one(test);

        assert_eq!(outcome.status, Status::Failed);
        let message = outcome.message.unwrap_or_default();
        assert!(message.starts_with("teardown "), "{message}");
        assert!(message.contains("teardown_boom.snag"), "{message}");
    }

    #[test]
    fn a_failing_teardown_keeps_the_tests_own_failure() {
        let bad = script("teardown_after_fail.snag", r#"throw "cleanup failed";"#);
        let body = script("teardown_after_fail_body.snag", r#"throw "the real bug";"#);

        let test = Test {
            teardown: vec![hook(bad, true)],
            ..with_script("t0", body)
        };
        let outcome = run_one(test);

        assert_eq!(outcome.status, Status::Failed);
        assert!(
            outcome
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("the real bug"),
            "{:?}",
            outcome.message
        );
        // The cleanup error is still visible, just not as the headline.
        assert!(
            outcome.output.iter().any(|l| l.contains("cleanup failed")),
            "{:?}",
            outcome.output
        );
    }

    #[test]
    fn script_setup_and_teardown_functions_run_around_the_body() {
        let body = script(
            "fn_hooks.snag",
            r#"
fn setup() { let user = "kim"; print("setup"); }
fn teardown() { print("teardown"); }
print("body " + user);
"#,
        );
        let outcome = run_one(with_script("t0", body));

        assert_eq!(outcome.status, Status::Passed, "{:?}", outcome.message);
        assert_eq!(outcome.output, ["setup", "body kim", "teardown"]);
    }

    #[test]
    fn on_teardown_callbacks_run_last_registered_first() {
        let body = script(
            "on_teardown_order.snag",
            r#"
on_teardown(|| print("one"));
on_teardown(|| print("two"));
print("body");
"#,
        );
        let outcome = run_one(with_script("t0", body));

        assert_eq!(outcome.status, Status::Passed, "{:?}", outcome.message);
        assert_eq!(outcome.output, ["body", "two", "one"]);
    }

    #[test]
    fn a_setup_script_can_register_its_own_cleanup() {
        let setup = script(
            "setup_registers.snag",
            r#"let resource = "res-42"; on_teardown(|| print("deleting " + resource));"#,
        );
        let body = script("setup_registers_body.snag", r#"print("body");"#);

        let test = Test {
            setup: vec![hook(setup, true)],
            ..with_script("t0", body)
        };
        let outcome = run_one(test);

        assert_eq!(outcome.status, Status::Passed, "{:?}", outcome.message);
        assert_eq!(outcome.output, ["body", "deleting res-42"]);
    }

    #[test]
    fn on_teardown_can_opt_out_of_running_after_a_failure() {
        let body = script(
            "on_teardown_opt_out.snag",
            r#"
on_teardown(|| print("always"));
on_teardown(|| print("only on success"), false);
throw "boom";
"#,
        );
        let outcome = run_one(with_script("t0", body));

        assert_eq!(outcome.status, Status::Failed);
        assert_eq!(outcome.output, ["always"]);
    }

    #[test]
    fn teardown_still_runs_after_the_test_times_out() {
        let cleanup = script("teardown_after_timeout.snag", r#"print("cleaned up");"#);
        let body = script(
            "timeout_with_teardown.snag",
            "let i = 0; while true { i += 1; }",
        );

        let test = Test {
            teardown: vec![hook(cleanup, true)],
            ..with_script("t0", body)
        };

        let mut args = args();
        args.timeout = Some(Duration::from_millis(50));
        let (result, _, recorder) = batch(&[test], &args, 1, &AtomicBool::new(false));
        result.unwrap();
        let outcome = &recorder.finished[0];

        assert_eq!(outcome.status, Status::TimedOut);
        assert_eq!(outcome.output, ["cleaned up"]);
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
