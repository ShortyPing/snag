# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`snag` — a regression test runner for HTTP APIs. Suites are declared in TOML, test bodies are [Rhai](https://rhai.rs) scripts (`.snag` files), and results are emitted through pluggable reporters (human, json, jsonl, teamcity, junit). Single binary crate, Rust edition 2024.

## Commands

```bash
cargo build
cargo test                       # all unit tests; offline by design
cargo test runner::              # one module
cargo test fail_fast             # one test by name substring
cargo test -- --nocapture        # see println! output
cargo fmt                        # rustfmt defaults, never hand-format
cargo clippy --all-targets       # includes test code
```

All three of `cargo fmt`, `cargo clippy --all-targets`, `cargo test` are expected clean before pushing.

Smoke test after behaviour changes (`demo/` is the runnable example suite):

```bash
./target/debug/snag demo/suite.toml -t fast -v   # offline test only
./target/debug/snag demo/suite.toml              # includes network tests
./target/debug/snag list --long demo/suite.toml
./target/debug/snag check -v demo/suite.toml
./target/debug/snag demo/suite.toml -t fast -f json   # also junit, teamcity
./target/debug/snag revision --offline -o -           # release manifest, no network
./target/debug/snag update --check                    # network
```

Docs site (Docusaurus, Node 20+ / pnpm) lives in `documentation/`:

```bash
cd documentation && pnpm install && pnpm start
pnpm build        # also the link checker — onBrokenLinks: 'throw'
pnpm typecheck
```

## Architecture

Pipeline: **discovery → manifest → runner → report**, with `scripting_registration` supplying the Rhai script API.

- `main.rs` — clap CLI (`run`, `list`, `check`, `init`, `revision`, `update`, `completions`) plus `Ctx` (format/color/verbose/quiet) and `Exit` (0 green, 1 tests failed, 2 could not run). `implicit_run()` rewrites `snag ./suite.toml` into `snag run ./suite.toml` by hand, because clap will not take a variadic positional alongside subcommands — adding a flag that takes a value means adding it to that fn's `TAKES_VALUE` list too.
- `discovery.rs` — the single entry point `discover()` that `run`/`list`/`check` all share, so all three see the same set. Expands paths/globs/dir walks into suite files (`suite.toml`, `snag.toml`, `*.snag.toml`; a file named explicitly on the CLI bypasses the naming convention), then applies the `Filter` (`--filter`/`--exclude` match name *or* id; `--tag` is OR-ed; `--regex` switches matcher kind).
- `manifest.rs` — TOML deserialization (`Manifest`/`TestDefinition`, `deny_unknown_fields`) flattened into a resolved `Test`: script paths joined to the manifest dir and normalized, variables merged (suite → test), timeouts resolved (test → file, and `--timeout` wins later in the runner), setup/teardown `HookSpec` resolved into `Hook`s. Suite hooks wrap test hooks: setup is suite-first, teardown unwinds test-first then suite. `always` is teardown-only and rejected on setup.
- `runner.rs` — the whole execution model. `run()` partitions on `parallel_safe`: `false` tests run first in a serial batch, the rest share a worker pool. `execute_batch()` spawns scoped threads pulling from an `AtomicUsize` index and sends `Event`s back over an `mpsc` channel, so **the reporter only ever runs on the main thread** — no locking, and reporters need not be `Sync`. A shared `cancel` flag drains the remainder as `Skipped` (fail-fast, or a reporter error). Each test gets a fresh `Engine`, a fresh `reqwest::blocking::Client`, and a fresh output sink per attempt.
- `report.rs` — the `Reporter` trait (`run_started`/`test_started`/`test_finished`/`run_finished`, all defaulted). Streaming formats write per event; `Json`/`Junit` buffer and emit in `run_finished`. `Multi` broadcasts to several reporters, which is how `--report FILE` puts human output on the terminal and the machine report in the file (with `--format human`, the file gets JSON instead).
- `revision.rs` — `snag revision`, which writes the release manifest (`revision.json`): version, an incrementing build number, the commit baked in by `build.rs`, and a download URL per platform. `TARGETS` is the release matrix and must stay in step with the workflow build matrices.
- `update.rs` — `snag update` and the startup "an update is available" notice. `start_check()` never touches the network on the calling thread: it prints from a cache file and refreshes on a detached thread, and the whole thing is off unless stderr is a TTY.
- `scripting_registration.rs` — everything scripts can call, grouped into `register_http`, `register_debug`, `register_assertions`, `register_env`, `register_teardown`. Requests are a chainable `ReqBuilder`; `.send()` is the only call that touches the network. `field(value, "a.b.0")` walks decoded JSON via `dig()` and errors on a missing key rather than returning unit.

### Two invariants worth knowing before touching the runner

**Timeouts are enforced twice.** `engine.on_progress` checks a deadline every `PROGRESS_INTERVAL` ops (kills runaway loops), *and* the deadline is set on the HTTP client (kills hung sockets) — `on_progress` cannot interrupt a thread already blocked on a socket read. Don't "simplify" the duplication. The deadline lives behind an `Arc<Mutex<Option<Instant>>>` so teardown can be handed a *fresh* budget; otherwise cleanup after a timed-out test would abort on its first operation.

**Hook scripts, `fn setup()`/`fn teardown()` in the test script, and `on_teardown(||...)` callbacks all share one `Scope`.** Hook functions are called with `eval_ast(false)` (top-level statements don't re-run) and `rewind_scope(false)` (so `fn setup()` can leave variables for the test). `on_teardown` closures are `FnPtr`s owned by the engine that built them, so every script's functions are merged into one AST before they can be called back; the queue is `Rc`, not `Arc`, since an engine never leaves its worker thread. Everything is compiled up front so a broken hook fails before the first request goes out.

Suite/test variables are pushed into the scope as **constants**, so scripts read `base_url` as a plain identifier but cannot reassign it.

## Conventions

- **Comments explain why, not what.** The existing ones are load-bearing (see the timeout comment above); a comment restating the next line does not belong.
- **Errors carry context** — `anyhow::Context` on I/O, messages saying what was attempted (`reading suite {path}`).
- **Assertion and script errors state both sides**: `expected 200, got 404`, never "mismatch". This is the product.
- **No `println!` outside reporters and command handlers.** Script output goes to the per-test `OutputSink`.
- Commit messages: short, imperative, lowercase. One logical change per commit; formatting-only changes go in their own commit.

## Testing conventions

Tests live in `#[cfg(test)] mod tests` at the bottom of each module — there is no `tests/` directory, and **no test touches the network** (gate one behind `#[ignore]` if it genuinely must). Fixture files go in a process-scoped temp dir (`snag-manifest-{pid}`, `snag-runner-{pid}`) and file names must be unique per test, since `cargo test` runs them in one process in parallel. In-memory `Test` values come from the `dummy(id)` builder plus struct-update syntax.

For `execute_batch` tests: use `workers = 1` wherever ordering is asserted. Cancellation is set on the main thread while workers race ahead, so after a reporter error, *how many* tests were skipped is not deterministic — assert only the invariants (error returned, flag set, every test produced exactly one outcome). Assert on the message text, not just that an error occurred, and pass the actual value as the panic message so CI output is diagnosable.

## Documentation is part of the change

`documentation/docs/` is not a follow-up task. Per the contributing guide:

| Change | Also update |
| --- | --- |
| New script function | unit test for its failure message + `reference/script-api.mdx` |
| New CLI flag | `reference/cli.mdx` + the relevant guide |
| New manifest field | `reference/manifest.mdx` + a `manifest.rs` default test |
| New output format | `reference/report-formats.mdx`, `guides/reporters.mdx`, an escaping test |
| Runner behaviour | an `execute_batch` test + `guides/execution-model.mdx` |
| New release platform | `TARGETS` in `revision.rs` **and** both workflow build matrices |
| Update or notice behaviour | `guides/updating.mdx` + a test in `update.rs` |

New pages must be added to `documentation/sidebars.ts` or they won't appear. Cross-link with relative paths including the extension (`[CLI](../reference/cli.mdx)`) so `pnpm build` can verify them. Fence languages: `toml` manifests, `js` for `.snag` scripts, `console` for transcripts with a `$` prompt, `bash` for copyable commands. **Command output in the docs is real output** — re-run the binary and paste, never invent it.
