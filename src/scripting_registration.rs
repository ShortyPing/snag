use base64::Engine as Base64Engine;
use base64::prelude::BASE64_STANDARD;
use cookie_store::RawCookie;
use reqwest::Url;
use reqwest::cookie::CookieStore;
use reqwest::header::HeaderValue;
use rhai::serde::{from_dynamic, to_dynamic};
use rhai::{Dynamic, Engine, EvalAltResult, FnPtr, Map};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex, RwLock};
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

// cookie_store directly rather than reqwest's Jar: Jar can only be asked what
// it would send to one URL, and print_cookies() needs the whole store.
#[derive(Default)]
pub struct CookieJar(RwLock<cookie_store::CookieStore>);

impl CookieJar {
    // Stores `cookie` as if `url` had sent it; one the store rejects is dropped.
    fn add(&self, cookie: &str, url: &Url) {
        if let (Ok(raw), Ok(mut store)) = (RawCookie::parse(cookie), self.0.write()) {
            store.store_response_cookies(std::iter::once(raw.into_owned()), url);
        }
    }

    fn clear(&self) {
        if let Ok(mut store) = self.0.write() {
            store.clear();
        }
    }

    // What the jar would send to `url`, as name/value pairs.
    fn pairs(&self, url: &Url) -> Vec<(String, String)> {
        let Ok(store) = self.0.read() else {
            return vec![];
        };
        store
            .get_request_values(url)
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // Every unexpired cookie as `name=value (domain d, path p)`, sorted so
    // output is stable across runs.
    fn describe(&self) -> Vec<String> {
        let Ok(store) = self.0.read() else {
            return vec![];
        };
        let mut all: Vec<(String, String, String, String)> = store
            .iter_unexpired()
            .map(|c| {
                let domain = c
                    .domain
                    .as_cow()
                    .map(|d| d.into_owned())
                    .unwrap_or_default();
                let path: &str = c.path.as_ref();
                (
                    domain,
                    path.to_string(),
                    c.name().to_string(),
                    c.value().to_string(),
                )
            })
            .collect();
        all.sort();
        all.into_iter()
            .map(|(domain, path, name, value)| {
                format!("{name}={value} (domain {domain}, path {path})")
            })
            .collect()
    }
}

impl CookieStore for CookieJar {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, url: &Url) {
        let cookies = cookie_headers.filter_map(|h| {
            let text = h.to_str().ok()?;
            RawCookie::parse(text).ok().map(RawCookie::into_owned)
        });
        if let Ok(mut store) = self.0.write() {
            store.store_response_cookies(cookies, url);
        }
    }

    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        let header = merge_cookie_header(self.pairs(url), &[]);
        if header.is_empty() {
            return None;
        }
        HeaderValue::from_str(&header).ok()
    }
}

#[derive(Clone)]
struct ReqBuilder {
    client: reqwest::blocking::Client, // cheap to clone, Arc inside
    jar: Option<Arc<CookieJar>>,
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    // Sent on this request only; never stored in the jar.
    cookies: Vec<(String, String)>,
    body: Option<String>,
}

#[derive(Clone, Debug)]
struct Response {
    status: u16,
    text: String,
    headers: Vec<(String, String)>,
    duration_ms: u64,
}

pub fn register_http(
    engine: &mut Engine,
    client: reqwest::blocking::Client,
    jar: Option<Arc<CookieJar>>,
) {
    // Nicer names than the reqwest types in error messages.
    engine.register_type_with_name::<ReqBuilder>("Request");
    engine.register_type_with_name::<Response>("Response");

    // get("..."), post("..."), etc. Each closure keeps its own client clone.
    for method in ["get", "post", "put", "patch", "delete", "head"] {
        let c = client.clone();
        let j = jar.clone();
        let verb = method.to_uppercase();
        engine.register_fn(method, move |url: &str| ReqBuilder {
            client: c.clone(),
            jar: j.clone(),
            method: verb.clone(),
            url: url.into(),
            headers: vec![],
            cookies: vec![],
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
    engine.register_fn("cookie", |mut b: ReqBuilder, name: &str, value: &str| {
        b.cookies.push((name.into(), value.into()));
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
            // reqwest leaves the jar out entirely once a request carries its own
            // Cookie header, so per-request cookies have to be merged in here.
            if !b.cookies.is_empty() {
                let stored = match (&b.jar, Url::parse(&b.url)) {
                    (Some(jar), Ok(url)) => jar.pairs(&url),
                    _ => vec![],
                };
                req = req.header("cookie", merge_cookie_header(stored, &b.cookies));
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

    engine.register_fn(
        "cookie",
        |r: &mut Response, name: &str| -> Result<String, Box<EvalAltResult>> {
            set_cookie_value(&r.headers, name).map_err(Into::into)
        },
    );

    // field(res.json(), "a.b.0") walks a decoded body by dotted path.
    engine.register_fn("field", |value: Dynamic, path: &str| dig(value, path));
}

// Reads and writes the per-test jar. `None` means the manifest set
// `cookies = false`; every call then says so instead of quietly doing nothing.
pub fn register_cookies(engine: &mut Engine, jar: Option<Arc<CookieJar>>) {
    let j = jar.clone();
    engine.register_fn(
        "cookie",
        move |url: &str, name: &str| -> Result<String, Box<EvalAltResult>> {
            let pairs = enabled(j.as_deref())?.pairs(&parse_url(url)?);
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .ok_or_else(|| {
                    format!("no cookie `{name}` for {url}, jar has: {}", names(&pairs)).into()
                })
        },
    );

    let j = jar.clone();
    engine.register_fn(
        "cookies",
        move |url: &str| -> Result<Map, Box<EvalAltResult>> {
            let pairs = enabled(j.as_deref())?.pairs(&parse_url(url)?);
            Ok(pairs
                .into_iter()
                .map(|(k, v)| (k.into(), Dynamic::from(v)))
                .collect())
        },
    );

    let j = jar.clone();
    engine.register_fn(
        "set_cookie",
        move |url: &str, cookie: &str| -> Result<(), Box<EvalAltResult>> {
            let jar = enabled(j.as_deref())?;
            let parsed = parse_url(url)?;
            let name = cookie
                .split(';')
                .next()
                .and_then(|pair| pair.split_once('='))
                .map(|(k, _)| k.trim())
                .filter(|k| !k.is_empty())
                .ok_or_else(|| format!("bad cookie {cookie:?}: expected `name=value`"))?;
            jar.add(cookie, &parsed);
            // The store drops a cookie that doesn't fit the URL without a word.
            if jar.pairs(&parsed).iter().any(|(k, _)| k == name) {
                return Ok(());
            }
            Err(format!(
                "cookie `{name}` was not stored for {url}: domain, path or Secure does not match"
            )
            .into())
        },
    );

    engine.register_fn(
        "clear_cookies",
        move || -> Result<(), Box<EvalAltResult>> {
            enabled(jar.as_deref())?.clear();
            Ok(())
        },
    );
}

fn enabled(jar: Option<&CookieJar>) -> Result<&CookieJar, Box<EvalAltResult>> {
    jar.ok_or_else(|| {
        "cookie jar is disabled for this test (cookies = false in the manifest)".into()
    })
}

fn parse_url(url: &str) -> Result<Url, Box<EvalAltResult>> {
    Url::parse(url).map_err(|e| format!("bad URL {url:?}: {e}").into())
}

// Routes print/debug and print_response into the sink instead of stdout.
pub fn register_debug(engine: &mut Engine, sink: OutputSink, jar: Option<Arc<CookieJar>>) {
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

    let s = sink.clone();
    engine.register_fn(
        "print_cookies",
        move || -> Result<(), Box<EvalAltResult>> {
            let lines = enabled(jar.as_deref())?.describe();
            if lines.is_empty() {
                push(&s, "(no cookies)");
            }
            for line in lines {
                push(&s, line);
            }
            Ok(())
        },
    );
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

// The value a response set for `name`, from its Set-Cookie headers.
fn set_cookie_value(headers: &[(String, String)], name: &str) -> Result<String, String> {
    let set: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
        .filter_map(|(_, v)| {
            let (k, v) = v.split(';').next()?.split_once('=')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    set.iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| format!("no cookie `{name}` in Set-Cookie, got: {}", names(&set)))
}

// Jar cookies first, minus any the request overrides by name, then the request's own.
fn merge_cookie_header(stored: Vec<(String, String)>, extra: &[(String, String)]) -> String {
    stored
        .into_iter()
        .filter(|(k, _)| !extra.iter().any(|(e, _)| e == k))
        .chain(extra.iter().cloned())
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

fn names(pairs: &[(String, String)]) -> String {
    if pairs.is_empty() {
        return "none".into();
    }
    pairs
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(", ")
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
        register_http(&mut e, reqwest::blocking::Client::new(), None);
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
        register_debug(&mut e, sink.clone(), None);
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

    fn cookie_engine(jar: Option<Arc<CookieJar>>) -> Engine {
        let mut e = engine();
        register_cookies(&mut e, jar);
        e
    }

    fn jar() -> Option<Arc<CookieJar>> {
        Some(Arc::new(CookieJar::default()))
    }

    #[test]
    fn set_cookie_then_read_it_back() {
        let e = cookie_engine(jar());
        let v: String = e
            .eval(
                r#"set_cookie("https://api.test/", "sid=abc; Path=/");
                   cookie("https://api.test/users", "sid")"#,
            )
            .unwrap();
        assert_eq!(v, "abc");
    }

    #[test]
    fn cookies_returns_every_cookie_for_the_url() {
        let e = cookie_engine(jar());
        let out: Map = e
            .eval(
                r#"set_cookie("https://api.test/", "a=1");
                   set_cookie("https://api.test/", "b=2");
                   cookies("https://api.test/")"#,
            )
            .unwrap();
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out["a"].clone().into_string().unwrap(), "1");
    }

    #[test]
    fn clear_cookies_empties_the_jar() {
        let e = cookie_engine(jar());
        let n: i64 = e
            .eval(
                r#"set_cookie("https://api.test/", "a=1");
                   clear_cookies();
                   cookies("https://api.test/").len()"#,
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn a_missing_cookie_names_what_the_jar_has() {
        let e = cookie_engine(jar());
        let err = e
            .eval::<String>(
                r#"set_cookie("https://api.test/", "a=1");
                   cookie("https://api.test/", "sid")"#,
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no cookie `sid` for https://api.test/, jar has: a"),
            "{err}"
        );
    }

    #[test]
    fn set_cookie_reports_a_cookie_the_jar_refused() {
        let e = cookie_engine(jar());
        let err = e
            .run(r#"set_cookie("https://api.test/", "sid=abc; Domain=other.test");"#)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("cookie `sid` was not stored for https://api.test/"),
            "{err}"
        );
    }

    #[test]
    fn set_cookie_rejects_a_value_without_a_name() {
        let e = cookie_engine(jar());
        let err = e
            .run(r#"set_cookie("https://api.test/", "nonsense");"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected `name=value`"), "{err}");
    }

    #[test]
    fn set_cookie_reports_a_bad_url() {
        let e = cookie_engine(jar());
        let err = e
            .run(r#"set_cookie("not a url", "a=1");"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains(r#"bad URL "not a url""#), "{err}");
    }

    #[test]
    fn a_disabled_jar_says_so() {
        let e = cookie_engine(None);
        for script in [
            r#"cookie("https://api.test/", "a")"#,
            r#"cookies("https://api.test/")"#,
            r#"set_cookie("https://api.test/", "a=1")"#,
            "clear_cookies()",
        ] {
            let err = e.eval::<Dynamic>(script).unwrap_err().to_string();
            assert!(
                err.contains("cookie jar is disabled for this test"),
                "{script}: {err}"
            );
        }
    }

    fn print_cookies_output(script: &str, jar: Option<Arc<CookieJar>>) -> Vec<String> {
        let mut e = cookie_engine(jar.clone());
        let sink = new_sink();
        register_debug(&mut e, sink.clone(), jar);
        e.run(script).unwrap();
        sink.lock().unwrap().clone()
    }

    #[test]
    fn print_cookies_lists_every_domain_sorted() {
        let out = print_cookies_output(
            r#"set_cookie("https://b.test/", "z=1");
               set_cookie("https://a.test/admin", "sid=abc; Path=/admin");
               set_cookie("https://a.test/", "lang=en");
               print_cookies();"#,
            jar(),
        );
        assert_eq!(
            out,
            [
                "lang=en (domain a.test, path /)",
                "sid=abc (domain a.test, path /admin)",
                "z=1 (domain b.test, path /)",
            ]
        );
    }

    #[test]
    fn print_cookies_on_an_empty_jar_says_so() {
        let out = print_cookies_output("print_cookies();", jar());
        assert_eq!(out, ["(no cookies)"]);
    }

    #[test]
    fn print_cookies_without_a_jar_says_it_is_disabled() {
        let mut e = cookie_engine(None);
        register_debug(&mut e, new_sink(), None);
        let err = e.run("print_cookies();").unwrap_err().to_string();
        assert!(
            err.contains("cookie jar is disabled for this test"),
            "{err}"
        );
    }

    fn pair(k: &str, v: &str) -> (String, String) {
        (k.into(), v.into())
    }

    #[test]
    fn set_cookie_value_reads_the_named_cookie() {
        let headers = vec![
            pair("Set-Cookie", "a=1; Path=/"),
            pair("content-type", "text/plain"),
            pair("set-cookie", "sid=abc; HttpOnly"),
        ];
        assert_eq!(set_cookie_value(&headers, "sid").unwrap(), "abc");
        let err = set_cookie_value(&headers, "nope").unwrap_err();
        assert_eq!(err, "no cookie `nope` in Set-Cookie, got: a, sid");
    }

    #[test]
    fn set_cookie_value_with_no_cookies_says_none() {
        let err = set_cookie_value(&[], "sid").unwrap_err();
        assert_eq!(err, "no cookie `sid` in Set-Cookie, got: none");
    }

    #[test]
    fn request_cookies_override_the_jar_by_name() {
        let merged = merge_cookie_header(
            vec![pair("sid", "old"), pair("lang", "en")],
            &[pair("sid", "new"), pair("x", "1")],
        );
        assert_eq!(merged, "lang=en; sid=new; x=1");
    }

    #[test]
    fn truncate_keeps_a_prefix() {
        assert!(truncate(&"x".repeat(600), 500).starts_with("xxxxx"));
        assert_eq!(truncate("short", 500), "short");
    }
}
