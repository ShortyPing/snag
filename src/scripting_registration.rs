use base64::Engine as Base64Engine;
use base64::prelude::BASE64_STANDARD;
use rhai::serde::{from_dynamic, to_dynamic};
use rhai::{Dynamic, Engine, EvalAltResult, FnPtr, Map};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// Per-test capture buffer, so parallel output doesn't interleave.
pub type OutputSink = Arc<Mutex<Vec<String>>>;

#[must_use] 
pub fn new_sink() -> OutputSink {
    Arc::new(Mutex::new(Vec::new()))
}

fn push(sink: &OutputSink, line: impl Into<String>) {
    if let Ok(mut guard) = sink.lock() {
        guard.push(line.into());
    }
}

// Cleanup registered from inside a script with on_teardown(...).
#[derive(Clone)]
pub struct TeardownCallback {
    pub func: FnPtr,
    // Run even when the test itself failed.
    pub always: bool,
}

// Callbacks land here in registration order; the runner unwinds them last-first.
// Rc, not Arc: a FnPtr belongs to the engine that made it, and an engine never
// leaves the worker thread that built it.
pub type TeardownQueue = Rc<RefCell<Vec<TeardownCallback>>>;

#[must_use] 
pub fn new_teardown_queue() -> TeardownQueue {
    Rc::new(RefCell::new(Vec::new()))
}

// on_teardown(|| ...) registers cleanup that runs after the test, pass or fail.
// on_teardown(|| ..., false) skips it when the test failed.
pub fn register_teardown(engine: &mut Engine, queue: TeardownQueue) {
    let q = queue.clone();
    engine.register_fn("on_teardown", move |func: FnPtr| {
        q.borrow_mut().push(TeardownCallback { func, always: true });
    });

    engine.register_fn("on_teardown", move |func: FnPtr, always: bool| {
        queue.borrow_mut().push(TeardownCallback { func, always });
    });
}

#[derive(Clone)]
struct ReqBuilder {
    client: reqwest::blocking::Client, // cheap to clone, Arc inside
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
}

#[derive(Clone, Debug)]
struct Response {
    status: u16,
    text: String,
    headers: Vec<(String, String)>,
    duration_ms: u64,
}

pub fn register_http(engine: &mut Engine, client: reqwest::blocking::Client) {
    // Nicer names than the reqwest types in error messages.
    engine.register_type_with_name::<ReqBuilder>("Request");
    engine.register_type_with_name::<Response>("Response");

    // get("..."), post("..."), etc. Each closure keeps its own client clone.
    for method in ["get", "post", "put", "patch", "delete", "head"] {
        let c = client.clone();
        let verb = method.to_uppercase();
        engine.register_fn(method, move |url: &str| ReqBuilder {
            client: c.clone(),
            method: verb.clone(),
            url: url.into(),
            headers: vec![],
            body: None,
        });
    }

    // Chainable builders: take by value, tweak, hand back.
    engine.register_fn("header", |mut b: ReqBuilder, k: &str, v: &str| {
        b.headers.push((k.into(), v.into()));
        b
    });
    engine.register_fn("bearer", |mut b: ReqBuilder, token: &str| {
        b.headers
            .push(("authorization".into(), format!("Bearer {token}")));
        b
    });
    engine.register_fn("body", |mut b: ReqBuilder, body: &str| {
        b.body = Some(body.into());
        b
    });
    engine.register_fn(
        "json",
        |mut b: ReqBuilder, body: Dynamic| -> Result<ReqBuilder, Box<EvalAltResult>> {
            b.headers
                .push(("content-type".into(), "application/json".into()));
            b.body = Some(dynamic_to_json_string(&body)?);
            Ok(b)
        },
    );

    // The only call that hits the network. Errors surface as Rhai throws.
    engine.register_fn(
        "send",
        |b: ReqBuilder| -> Result<Response, Box<EvalAltResult>> {
            let method = b
                .method
                .parse::<reqwest::Method>()
                .map_err(|_| format!("bad method: {}", b.method))?;
            let mut req = b.client.request(method, &b.url);
            for (k, v) in &b.headers {
                req = req.header(k, v);
            }
            if let Some(body) = b.body {
                req = req.body(body);
            }

            let start = std::time::Instant::now();
            let resp = req.send().map_err(|e| format!("request failed: {e}"))?;
            let status = resp.status().as_u16();
            let headers = resp
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();
            let text = resp.text().map_err(|e| format!("read body failed: {e}"))?;
            Ok(Response {
                status,
                text,
                headers,
                duration_ms: start.elapsed().as_millis() as u64,
            })
        },
    );

    engine.register_get("status", |r: &mut Response| i64::from(r.status));
    engine.register_get("ok", |r: &mut Response| (200..300).contains(&r.status));
    engine.register_get("text", |r: &mut Response| r.text.clone());
    engine.register_get("duration_ms", |r: &mut Response| r.duration_ms as i64);
    engine.register_fn("json", |r: &mut Response| json_to_dynamic(&r.text));
    engine.register_fn(
        "basic",
        |username: String, password: String| -> Result<String, Box<EvalAltResult>> {
            let str = format!("{username}:{password}");
            let bytes = str.as_bytes();
            let encoded = BASE64_STANDARD.encode(bytes);

            Ok(format!("Basic {encoded}"))
        },
    );
    engine.register_fn("header", |r: &mut Response, name: &str| {
        let wanted = name.to_ascii_lowercase();
        r.headers
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == wanted)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    });

    // field(res.json(), "a.b.0") walks a decoded body by dotted path.
    engine.register_fn("field", |value: Dynamic, path: &str| dig(value, path));
}

// Routes print/debug and print_response into the sink instead of stdout.
pub fn register_debug(engine: &mut Engine, sink: OutputSink) {
    let s = sink.clone();
    engine.on_print(move |text| push(&s, text));

    let s = sink.clone();
    engine.on_debug(move |text, source, pos| {
        let src = source.map(|s| format!(" {s}")).unwrap_or_default();
        push(&s, format!("[debug{src} {pos}] {text}"));
    });

    let s = sink.clone();
    engine.register_fn("print_response", move |r: &mut Response| {
        push(&s, format!("{} ({}ms)", r.status, r.duration_ms));
        for (k, v) in &r.headers {
            push(&s, format!("  {k}: {v}"));
        }
        for line in r.text.lines().take(40) {
            push(&s, format!("  {line}"));
        }
    });
}

pub fn register_assertions(engine: &mut Engine) {
    engine.register_fn(
        "assert_status",
        |r: &mut Response, expected: i64| -> Result<(), Box<EvalAltResult>> {
            let status = i64::from(r.status);
            if status == expected {
                return Ok(());
            }
            Err(format!(
                "assertion failed: expected status {expected}, got {status}\nbody: {}",
                truncate(&r.text, 500)
            )
            .into())
        },
    );

    engine.register_fn(
        "assert_ok",
        |r: &mut Response| -> Result<(), Box<EvalAltResult>> {
            if (200..300).contains(&r.status) {
                return Ok(());
            }
            Err(format!(
                "assertion failed: expected a 2xx status, got {}\nbody: {}",
                r.status,
                truncate(&r.text, 500)
            )
            .into())
        },
    );

    engine.register_fn(
        "assert_body_contains",
        |r: &mut Response, needle: &str| -> Result<(), Box<EvalAltResult>> {
            if r.text.contains(needle) {
                return Ok(());
            }
            Err(format!(
                "assertion failed: body does not contain {needle:?}\nbody: {}",
                truncate(&r.text, 500)
            )
            .into())
        },
    );

    engine.register_fn(
        "assert_faster_than",
        |r: &mut Response, max_ms: i64| -> Result<(), Box<EvalAltResult>> {
            if (r.duration_ms as i64) <= max_ms {
                return Ok(());
            }
            Err(format!(
                "assertion failed: request took {}ms, budget was {max_ms}ms",
                r.duration_ms
            )
            .into())
        },
    );

    engine.register_fn(
        "assert",
        |cond: bool, msg: &str| -> Result<(), Box<EvalAltResult>> {
            if cond {
                return Ok(());
            }
            Err(format!("assertion failed: {msg}").into())
        },
    );

    // No generics in Rhai, so assert_eq is one overload per scalar type.
    engine.register_fn("assert_eq", |a: i64, b: i64| eq_result(a, b));
    engine.register_fn("assert_eq", |a: bool, b: bool| eq_result(a, b));
    engine.register_fn("assert_eq", |a: f64, b: f64| eq_result(a, b));
    engine.register_fn("assert_eq", |a: &str, b: &str| eq_result(a, b));

    engine.register_fn(
        "assert_contains",
        |haystack: &str, needle: &str| -> Result<(), Box<EvalAltResult>> {
            if haystack.contains(needle) {
                return Ok(());
            }
            Err(format!("assertion failed: {haystack:?} does not contain {needle:?}").into())
        },
    );
}

// Non-HTTP helpers: env vars, sleep, fail.
pub fn register_env(engine: &mut Engine) {
    engine.register_fn("env", |name: &str| -> Result<String, Box<EvalAltResult>> {
        std::env::var(name).map_err(|_| format!("environment variable `{name}` is not set").into())
    });
    engine.register_fn("env_or", |name: &str, fallback: &str| {
        std::env::var(name).unwrap_or_else(|_| fallback.to_string())
    });
    engine.register_fn("sleep_ms", |ms: i64| {
        std::thread::sleep(Duration::from_millis(ms.max(0) as u64));
    });
    engine.register_fn("fail", |msg: &str| -> Result<(), Box<EvalAltResult>> {
        Err(msg.to_string().into())
    });
}

fn eq_result<T: PartialEq + std::fmt::Debug>(a: T, b: T) -> Result<(), Box<EvalAltResult>> {
    if a == b {
        return Ok(());
    }
    Err(format!("assertion failed: expected {b:?}, got {a:?}").into())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}… ({} bytes total)", s.len())
}

// Walk `a.b.0` into a decoded value. A missing key errors instead of returning
// unit, so a mistyped path fails loudly.
fn dig(value: Dynamic, path: &str) -> Result<Dynamic, Box<EvalAltResult>> {
    let mut current = value;
    for segment in path.split('.').filter(|s| !s.is_empty()) {
        if let Some(map) = current.clone().try_cast::<Map>() {
            current = map
                .get(segment)
                .cloned()
                .ok_or_else(|| format!("no key `{segment}` in path `{path}`"))?;
        } else if let Some(arr) = current.clone().try_cast::<rhai::Array>() {
            let index: usize = segment
                .parse()
                .map_err(|_| format!("`{segment}` in path `{path}` is not an array index"))?;
            current = arr
                .get(index)
                .cloned()
                .ok_or_else(|| format!("index {index} out of range in path `{path}`"))?;
        } else {
            return Err(format!(
                "cannot descend into `{segment}`: value is not an object or array"
            )
            .into());
        }
    }
    Ok(current)
}

fn json_to_dynamic(text: &str) -> Result<Dynamic, Box<EvalAltResult>> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("response body is not valid JSON: {e}"))?;
    to_dynamic(value)
}

fn dynamic_to_json_string(d: &Dynamic) -> Result<String, Box<EvalAltResult>> {
    let value: serde_json::Value = from_dynamic(d)?;
    serde_json::to_string(&value).map_err(|e| e.to_string().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        let mut e = Engine::new();
        register_assertions(&mut e);
        register_env(&mut e);
        register_http(&mut e, reqwest::blocking::Client::new());
        e
    }

    #[test]
    fn assert_eq_reports_both_sides() {
        let err = engine().run("assert_eq(1, 2);").unwrap_err().to_string();
        assert!(err.contains("expected 2, got 1"), "{err}");
    }

    #[test]
    fn assert_passes_silently() {
        engine()
            .run(r#"assert_eq("a", "a"); assert(true, "x");"#)
            .unwrap();
    }

    #[test]
    fn field_walks_maps_and_arrays() {
        let e = engine();
        let out: i64 = e
            .eval(r#"let v = parse_json(`{"a":{"b":[10,20]}}`); field(v, "a.b.1")"#)
            .unwrap();
        assert_eq!(out, 20);
    }

    #[test]
    fn field_errors_on_missing_key() {
        let e = engine();
        let err = e
            .eval::<Dynamic>(r#"field(parse_json("{}"), "nope")"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no key `nope`"), "{err}");
    }

    #[test]
    fn print_is_captured_not_printed() {
        let mut e = engine();
        let sink = new_sink();
        register_debug(&mut e, sink.clone());
        e.run(r#"print("hello");"#).unwrap();
        assert_eq!(sink.lock().unwrap().as_slice(), ["hello".to_string()]);
    }

    #[test]
    fn env_or_falls_back() {
        let e = engine();
        let v: String = e
            .eval(r#"env_or("SNAG_DEFINITELY_UNSET_VAR", "fallback")"#)
            .unwrap();
        assert_eq!(v, "fallback");
    }

    #[test]
    fn truncate_keeps_a_prefix() {
        assert!(truncate(&"x".repeat(600), 500).starts_with("xxxxx"));
        assert_eq!(truncate("short", 500), "short");
    }
}
