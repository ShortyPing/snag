# snag

A regression test runner for HTTP APIs. Suites are declared in TOML, assertions
are written in [Rhai](https://rhai.rs) scripts, and results come out in whatever
format the consumer speaks — a terminal, a CI report, or an IDE test tree.

```
snag                     # run every suite found under the current directory
snag demo/suite.toml     # run one suite (the `run` verb is implicit)
snag list --long         # discover tests without executing them
snag check               # parse manifests and compile scripts only
snag init                # scaffold a suite.toml plus an example script
```

## Suite files

A suite is any file named `suite.toml`, `snag.toml`, or `*.snag.toml`. Naming a
file explicitly on the command line bypasses that convention.

```toml
title = "Snag Demo Regression Suite"
timeout = "10s"            # default for every test in the file

[variables]                # visible to every test in the file
base_url = "https://example.com"

[[test]]
id = "example-responds"    # unique within the file
name = "example.com answers 200"
tags = ["network", "smoke"]
parallel_safe = true       # false pins the test to a serial phase
timeout = "20s"            # overrides the file-level default
file = "./example_test.snag"

[test.variables]           # override suite variables for this test only
base_url = "https://httpbin.org"
```

Precedence, loosest to tightest: suite variable → test variable, and file
timeout → test timeout → `--timeout`.

## Scripts

Variables land in scope as constants, so `base_url` is just an identifier.

```rhai
let res = post(`${base_url}/post`)
    .header("accept", "application/json")
    .json(#{ project: "snag", ok: true })
    .send();

assert_ok(res);
assert_eq(field(res.json(), "json.project"), "snag");
```

**Requests** — `get`, `post`, `put`, `patch`, `delete`, `head` build a request;
`.header(k, v)`, `.bearer(token)`, `.body(text)` and `.json(value)` chain onto
it; `.send()` performs it.

**Responses** — `res.status`, `res.ok`, `res.text`, `res.duration_ms`,
`res.header("name")`, `res.json()`, and `field(value, "a.b.0")` to walk a decoded
body by dotted path (a missing key fails the test rather than returning `()`).

**Assertions** — `assert_status`, `assert_ok`, `assert_body_contains`,
`assert_faster_than`, `assert_eq`, `assert_contains`, `assert(cond, msg)`, `fail(msg)`.

**Auth** — `basic(user, password)` returns a ready `Basic <base64>` header value;
`.bearer(token)` covers the other common case.

**Misc** — `env("NAME")` (fails if unset), `env_or("NAME", "default")`,
`sleep_ms(n)`, `print(...)` and `print_response(res)`. Printed output is captured
per test, shown under failures, and under passes with `-v`.

## Setup and teardown

Shared steps live in their own scripts and are wired up per suite or per test.
Suite hooks wrap test hooks: setup runs outside-in, teardown unwinds inside-out.

```toml
setup = "./login.snag"                                # every test in the file
teardown = { file = "./reset.snag", always = true }   # `always = false` skips it after a failure

[[test]]
id = "cart-create"
file = "./cart_create.snag"
setup = ["./seed.snag"]                               # after the suite's setup
```

A script can also carry its own: `fn setup()` runs before the body and leaves its
variables in scope for it, `fn teardown()` runs after it whatever happened, and
`on_teardown(|| ...)` registers cleanup at the point the resource is created —
callbacks unwind last-registered-first. Hooks and the test share one scope, and
teardown gets a fresh timeout budget so cleanup survives a timed-out test.

## Selecting tests

`--filter/-k` and `--exclude/-e` match a test's name *or* id as substrings, or as
regular expressions with `--regex`. `--tag/-t` is OR-ed across tags. All three
work identically for `run`, `list`, and `check`.

## Running

`--jobs/-j N` sets the worker count (0 = logical CPUs). Tests marked
`parallel_safe = false` run first, one at a time; the rest share the pool.
`--fail-fast` reports the remainder as skipped instead of dropping them,
`--retries N` re-runs failures and reports the attempt count, `--seed N` shuffles
deterministically, and `--dry-run` lists what would run.

A timeout aborts the interpreter *and* caps the HTTP client, so both a runaway
loop and a hung socket end the test.

## Output

`--format` picks the reporter: `human`, `json`, `jsonl`, `teamcity` (IntelliJ
renders a native test tree), or `junit`. `--report FILE` keeps human output on
the terminal and writes the machine report to a file — with `--format human`
that file gets JSON, since a human report is nothing a tool can read.

Exit codes: `0` all green, `1` tests failed, `2` could not run.
