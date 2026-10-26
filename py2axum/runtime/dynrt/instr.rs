//! prometheus_fastapi_instrumentator 7.1 / 8.x: `Instrumentator(...)`, `.instrument(app, ...)` (its middleware,
//! with `metrics.default()`: `http_requests_total`, `http_request_size_bytes`, `http_response_size_bytes`,
//! `http_request_duration_highr_seconds`, `http_request_duration_seconds`, and the optional in-progress gauge)
//! and `.expose(app, ...)` (the `/metrics` route: `generate_latest` of the registry). The metrics are
//! prometheus_client's (dynrt/prom.rs), created when the middleware stack is built, as Starlette instantiates
//! the middleware. The handler label is the template of the route that matches the request
//! (`routing.get_route_name`, over the binary's routes and those added at run time); "none" when nothing
//! matches (grouped). The values are recorded once the response's body is complete.
use std::sync::Arc;
use std::time::Instant;

use axum::response::Response;

use super::v::*;
use super::web::RouteDef;
use super::{ops, prom, Cx};

/// `PrometheusFastApiInstrumentator(...)`'s options
pub struct Instr {
    group_status: bool,
    ignore_untemplated: bool,
    group_untemplated: bool,
    round: Option<i32>,
    enabled: bool,
    inprogress: Option<(String, bool)>,
    exclude_streaming: bool,
    /// `re.compile(path)` of each excluded handler, searched in the handler
    excluded: Vec<V>,
    registry: Option<V>,
}

/// the middleware: the metrics it records
pub struct Layer {
    instr: Arc<Instr>,
    total: Arc<prom::Metric>,
    in_size: Arc<prom::Metric>,
    out_size: Arc<prom::Metric>,
    highr: Arc<prom::Metric>,
    lowr: Arc<prom::Metric>,
    highr_2xx_only: bool,
    inprogress: Option<(Arc<prom::Metric>, bool)>,
}

fn kw<'a>(kwargs: &'a [(String, V)], k: &str) -> Option<&'a V> {
    kwargs.iter().find(|(x, _)| x == k).map(|(_, v)| v)
}

fn flag(kwargs: &[(String, V)], k: &str, default: bool) -> R<bool> {
    kw(kwargs, k).map(ops::truthy).transpose().map(|b| b.unwrap_or(default))
}

fn text(kwargs: &[(String, V)], k: &str, default: &str) -> R<String> {
    match kw(kwargs, k) {
        None => Ok(default.to_string()),
        Some(V::Str(s)) => Ok(s.to_string()),
        Some(o) => Err(Exc::type_error(format!("py2axum: Instrumentator({k}=) must be a str, not {}", o.type_name()))),
    }
}

/// `Instrumentator(...)`: keyword options only (as the documentation writes them)
pub fn new(args: &[V], kwargs: &[(String, V)]) -> R {
    const KNOWN: [&str; 15] = ["should_group_status_codes", "should_ignore_untemplated", "should_group_untemplated", "should_include_root_path",
        "should_round_latency_decimals", "should_respect_env_var", "should_instrument_requests_inprogress", "should_exclude_streaming_duration",
        "excluded_handlers", "body_handlers", "round_latency_decimals", "env_var_name", "inprogress_name", "inprogress_labels", "registry"];
    if !args.is_empty() {
        return Err(Exc::type_error("py2axum: Instrumentator() takes keyword options only"));
    }
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !KNOWN.contains(&k.as_str())) {
        return Err(Exc::type_error(format!("PrometheusFastApiInstrumentator.__init__() got an unexpected keyword argument '{k}'")));
    }
    if flag(kwargs, "should_include_root_path", false)? {
        return Err(Exc::type_error("py2axum: Instrumentator(should_include_root_path=True) is not supported"));
    }
    let excluded = match kw(kwargs, "excluded_handlers") {
        None => vec![],
        Some(v) => ops::iter(v)?
            .iter()
            .map(|p| {
                let p = ops::str_(p)?;
                super::stdlib::compile(&V::str(&p), None)
            })
            .collect::<R<_>>()?,
    };
    let decimals = match kw(kwargs, "round_latency_decimals") {
        None => 4,
        Some(V::Int(n)) => *n as i32,
        Some(o) => return Err(Exc::type_error(format!("py2axum: round_latency_decimals= must be an int, not {}", o.type_name()))),
    };
    let env = text(kwargs, "env_var_name", "ENABLE_METRICS")?;
    // `_should_instrumentate`: os.getenv(name, "False").lower() in ["true", "1"]
    let enabled = !flag(kwargs, "should_respect_env_var", false)?
        || matches!(std::env::var(&env).unwrap_or_else(|_| "False".into()).to_lowercase().as_str(), "true" | "1");
    // PROMETHEUS_MULTIPROC_DIR must name a directory (the constructor's check)
    if let Ok(d) = std::env::var("PROMETHEUS_MULTIPROC_DIR") {
        if !std::path::Path::new(&d).is_dir() {
            return Err(Exc::value_error(format!("Env var PROMETHEUS_MULTIPROC_DIR='{d}' not a directory.")));
        }
    }
    let instr = Instr {
        group_status: flag(kwargs, "should_group_status_codes", true)?,
        ignore_untemplated: flag(kwargs, "should_ignore_untemplated", false)?,
        group_untemplated: flag(kwargs, "should_group_untemplated", true)?,
        round: flag(kwargs, "should_round_latency_decimals", false)?.then_some(decimals),
        enabled,
        inprogress: if flag(kwargs, "should_instrument_requests_inprogress", false)? {
            Some((text(kwargs, "inprogress_name", "http_requests_inprogress")?, flag(kwargs, "inprogress_labels", false)?))
        } else {
            None
        },
        exclude_streaming: flag(kwargs, "should_exclude_streaming_duration", false)?,
        excluded,
        registry: kw(kwargs, "registry").filter(|v| !v.is_none()).cloned(),
    };
    Ok(V::native(Native::Instr(Arc::new(instr))))
}

fn metric(kind: &str, name: &str, doc: &str, labels: &[&str], ns: &V, sub: &V, buckets: Option<V>, registry: &Option<V>, extra: Vec<(String, V)>) -> R<Arc<prom::Metric>> {
    let mut kwargs = vec![
        ("name".to_string(), V::str(name)),
        ("documentation".to_string(), V::str(doc)),
        ("labelnames".to_string(), V::tuple(labels.iter().map(V::str).collect())),
        ("namespace".to_string(), ns.clone()),
        ("subsystem".to_string(), sub.clone()),
    ];
    if let Some(b) = buckets {
        kwargs.push(("buckets".into(), b));
    }
    if let Some(r) = registry {
        kwargs.push(("registry".into(), r.clone()));
    }
    kwargs.extend(extra);
    let v = prom::new_metric(kind, vec![], kwargs)?;
    prom::metric_of(&v).ok_or_else(|| Exc::type_error("py2axum: not a metric"))
}

/// the buckets with `inf` appended when the last is not (`metrics.default`)
fn buckets(v: Option<&V>, default: &[f64]) -> R<V> {
    let mut items: Vec<V> = match v {
        None => default.iter().map(|f| V::Float(*f)).collect(),
        Some(v) => ops::iter(v)?,
    };
    let last_inf = match items.last() {
        Some(V::Float(f)) => f.is_infinite() && *f > 0.0,
        Some(V::Str(s)) => matches!(s.to_lowercase().as_str(), "inf" | "+inf" | "infinity"),
        _ => false,
    };
    if !last_inf {
        items.push(V::Float(f64::INFINITY));
    }
    Ok(V::list(items))
}

const HIGHR: [f64; 21] = [0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 1.5, 2.0, 2.5, 3.0, 3.5, 4.0, 4.5, 5.0, 7.5, 10.0, 30.0, 60.0];

/// `.instrument(app)` calls made before the middleware stack is built (at import): Starlette instantiates the
/// middleware, so creates its metrics, when it builds the stack
static PENDING: parking_lot::Mutex<Vec<(V, Vec<(String, V)>)>> = parking_lot::Mutex::new(Vec::new());

const INSTRUMENT_KW: [&str; 5] = ["metric_namespace", "metric_subsystem", "should_only_respect_2xx_for_highr", "latency_highr_buckets", "latency_lowr_buckets"];

/// `.instrument(app, metric_namespace=, metric_subsystem=, should_only_respect_2xx_for_highr=, latency_highr_buckets=,
/// latency_lowr_buckets=)`: `app.add_middleware(PrometheusInstrumentatorMiddleware, ...)`; returns the instrumentator
pub fn instrument(recv: &V, kwargs: Vec<(String, V)>) -> R {
    let Some(instr) = instr_of(recv) else { return Err(Exc::type_error("py2axum: instrument() of a non-Instrumentator")) };
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| !INSTRUMENT_KW.contains(&k.as_str())) {
        return Err(Exc::type_error(format!("PrometheusFastApiInstrumentator.instrument() got an unexpected keyword argument '{k}'")));
    }
    if !instr.enabled {
        return Ok(recv.clone());
    }
    if super::routing::is_building() {
        if let Some(l) = layer(recv, &kwargs)? {
            super::routing::push_built(super::asgi::Mw::Instrument(Arc::new(l)))?;
        }
    } else {
        PENDING.lock().push((recv.clone(), kwargs));
    }
    Ok(recv.clone())
}

/// the middlewares of the `.instrument(app)` calls made so far, built now (the stack is being built)
pub fn take_pending() -> R<Vec<super::asgi::Mw>> {
    let pending = std::mem::take(&mut *PENDING.lock());
    let mut out = Vec::new();
    for (recv, kwargs) in pending {
        if let Some(l) = layer(&recv, &kwargs)? {
            out.push(super::asgi::Mw::Instrument(Arc::new(l)));
        }
    }
    Ok(out)
}

/// the middleware and its metrics (None when disabled by the variable)
fn layer(recv: &V, kwargs: &[(String, V)]) -> R<Option<Layer>> {
    let Some(instr) = instr_of(recv) else { return Err(Exc::type_error("py2axum: instrument() of a non-Instrumentator")) };
    if !instr.enabled {
        return Ok(None);
    }
    let ns = V::str(text(kwargs, "metric_namespace", "")?);
    let sub = V::str(text(kwargs, "metric_subsystem", "")?);
    let reg = &instr.registry;
    let highr = buckets(kw(kwargs, "latency_highr_buckets"), &HIGHR)?;
    let lowr = buckets(kw(kwargs, "latency_lowr_buckets"), &[0.1, 0.5, 1.0])?;
    let total = metric("Counter", "http_requests_total", "Total number of requests by method, status and handler.",
                       &["method", "status", "handler"], &ns, &sub, None, reg, vec![])?;
    let in_size = metric("Summary", "http_request_size_bytes",
                         "Content length of incoming requests by handler. Only value of header is respected. Otherwise ignored. No percentile calculated. ",
                         &["handler"], &ns, &sub, None, reg, vec![])?;
    let out_size = metric("Summary", "http_response_size_bytes",
                          "Content length of outgoing responses by handler. Only value of header is respected. Otherwise ignored. No percentile calculated. ",
                          &["handler"], &ns, &sub, None, reg, vec![])?;
    let highr_m = metric("Histogram", "http_request_duration_highr_seconds",
                         "Latency with many buckets but no API specific labels. Made for more accurate percentile calculations. ",
                         &[], &ns, &sub, Some(highr), reg, vec![])?;
    let lowr_m = metric("Histogram", "http_request_duration_seconds",
                        "Latency with only few buckets by handler. Made to be only used if aggregation by handler is important. ",
                        &["method", "handler"], &ns, &sub, Some(lowr), reg, vec![])?;
    // the in-progress gauge: the middleware's own (default registry, no namespace)
    let inprogress = match &instr.inprogress {
        Some((name, labelled)) => {
            let labels: &[&str] = if *labelled { &["method", "handler"] } else { &[] };
            let g = metric("Gauge", name, "Number of HTTP requests in progress.", labels, &V::str(""), &V::str(""), None, &None,
                           vec![("multiprocess_mode".into(), V::str("livesum"))])?;
            Some((g, *labelled))
        }
        None => None,
    };
    Ok(Some(Layer {
        instr,
        total,
        in_size,
        out_size,
        highr: highr_m,
        lowr: lowr_m,
        highr_2xx_only: flag(kwargs, "should_only_respect_2xx_for_highr", false)?,
        inprogress,
    }))
}

fn instr_of(v: &V) -> Option<Arc<Instr>> {
    match v {
        V::Native(n) => match &**n {
            Native::Instr(i) => Some(i.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// CPython's `int(text)` of a header value (surrounding whitespace, a sign, `_` between digits)
fn py_int(s: &str) -> Option<i64> {
    let t = s.trim();
    let (neg, d) = match t.as_bytes().first() {
        Some(b'+') => (false, &t[1..]),
        Some(b'-') => (true, &t[1..]),
        _ => (false, t),
    };
    if d.is_empty() || d.starts_with('_') || d.ends_with('_') || d.contains("__") || !d.chars().all(|c| c.is_ascii_digit() || c == '_') {
        return None;
    }
    let n: i64 = d.replace('_', "").parse().ok()?;
    Some(if neg { -n } else { n })
}

/// Python's `round(x, n)` of a duration (half to even on the decimal value)
fn round_to(x: f64, n: i32) -> f64 {
    let f = 10f64.powi(n);
    let y = x * f;
    let r = y.round();
    let r = if (y - y.trunc()).abs() == 0.5 { 2.0 * (y / 2.0).round() } else { r };
    r / f
}

impl Layer {
    pub async fn call<F>(self: &Arc<Self>, cx: &Cx, routes: &'static [RouteDef], next: F) -> R<Response>
    where
        F: std::future::Future<Output = R<Response>>,
    {
        let start = Instant::now();
        let (handler, templated) = match super::web::route_template(cx, routes) {
            Some(t) => (t, true),
            None => (super::web::unquote(&cx.req.path), false),
        };
        let excluded = (!templated && self.instr.ignore_untemplated)
            || self.instr.excluded.iter().any(|p| super::stdlib::pattern_search(p, &handler));
        let handler = if !templated && self.instr.group_untemplated { "none".to_string() } else { handler };
        let method = cx.req.method.clone();
        let gauge = match (&self.inprogress, excluded) {
            (Some((g, true)), false) => Some(prom::child(g, &[&method, &handler])?),
            (Some((g, false)), false) => Some(g.clone()),
            _ => None,
        };
        if let Some(g) = &gauge {
            prom::gauge_add(g, 1.0)?;
        }
        let req_len = cx.req.header("content-length");
        let res = next.await;
        let started = start.elapsed().as_secs_f64();
        let (status, out_len) = match &res {
            Ok(r) => (r.status().as_u16(), match r.headers().get("content-length").and_then(|v| v.to_str().ok()) {
                Some(v) => py_int(v).unwrap_or(0),
                // Response(content=b"", headers=...) populates content-length 0 (none for 1xx, 204, 304)
                None => 0,
            }),
            Err(_) => (500, 0),
        };
        let this = self.clone();
        let record = move || {
            if excluded {
                return;
            }
            let duration = start.elapsed().as_secs_f64();
            let (mut d, mut ds) = (duration, started);
            if let Some(g) = &gauge {
                let _ = prom::gauge_add(g, -1.0);
            }
            if let Some(n) = this.instr.round {
                d = round_to(d, n);
                ds = round_to(ds, n);
            }
            let status = status.to_string();
            let status = if this.instr.group_status { format!("{}xx", &status[..1]) } else { status };
            let d = if this.instr.exclude_streaming { ds } else { d };
            let _ = (|| -> R<()> {
                let c = prom::child(&this.total, &[&method, &status, &handler])?;
                prom::inc_one(&c)?;
                // int(request.headers.get("Content-Length", 0)): an unparsable value raises there, nothing else recorded
                let Some(n) = req_len.as_deref().map(py_int).unwrap_or(Some(0)) else { return Ok(()) };
                let c = prom::child(&this.in_size, &[&handler])?;
                prom::observe_f(&c, n as f64)?;
                let c = prom::child(&this.out_size, &[&handler])?;
                prom::observe_f(&c, out_len as f64)?;
                if !this.highr_2xx_only || status.starts_with('2') {
                    prom::observe_f(&this.highr, d)?;
                }
                let c = prom::child(&this.lowr, &[&method, &handler])?;
                prom::observe_f(&c, d)?;
                Ok(())
            })();
        };
        match res {
            Ok(r) => Ok(super::rawasgi::on_end(r, record)),
            Err(e) => {
                record();
                Err(e)
            }
        }
    }
}

/// `.expose(app, should_gzip=False, endpoint="/metrics", include_in_schema=True, tags=None, **kwargs)`: a GET route
/// answering `generate_latest(registry)` (gzip-compressed when asked and accepted)
pub fn expose(recv: &V, kwargs: Vec<(String, V)>) -> R {
    let kwargs = &kwargs[..];
    let Some(instr) = instr_of(recv) else { return Err(Exc::type_error("py2axum: expose() of a non-Instrumentator")) };
    for (k, _) in kwargs {
        if !matches!(k.as_str(), "should_gzip" | "endpoint" | "include_in_schema" | "tags" | "summary" | "description" | "name") {
            return Err(Exc::type_error(format!("py2axum: Instrumentator.expose({k}=) is not supported")));
        }
    }
    if !instr.enabled {
        return Ok(recv.clone());
    }
    let gzip = flag(kwargs, "should_gzip", false)?;
    let endpoint = text(kwargs, "endpoint", "/metrics")?;
    let reg = instr.registry.clone();
    let f: super::FnVal = Arc::new(move |cx: &Cx, _args: Vec<V>| {
        let reg = reg.clone();
        Box::pin(async move { metrics_response(cx, reg, gzip).await })
    });
    // `app.get(endpoint)(metrics)`: an APIRoute (GET only), after the declared routes
    super::routing::add_native_route(&endpoint, &["GET"], "metrics", V::native(Native::Builtin("metrics", f)))?;
    Ok(recv.clone())
}

async fn metrics_response(cx: &Cx, registry: Option<V>, gzip: bool) -> R {
    // PROMETHEUS_MULTIPROC_DIR: a CollectorRegistry of its own over the MultiProcessCollector
    let reg = if std::env::var_os("PROMETHEUS_MULTIPROC_DIR").is_some() {
        let r = prom::registry_new(vec![], vec![])?;
        prom::multiproc_new(vec![r.clone()], vec![])?;
        r
    } else {
        registry.unwrap_or_else(|| prom::value("REGISTRY"))
    };
    let body = prom::generate_latest(cx, vec![reg], vec![]).await?;
    let accepts = cx.req.header("accept-encoding").is_some_and(|a| a.contains("gzip"));
    // Response(content=...), then resp.headers["Content-Type"] = CONTENT_TYPE_LATEST (and Content-Encoding)
    let body = if gzip && accepts { super::stdlib::gzip_compress(&[body], &[])? } else { body };
    let r = super::resp::new("Response", &[body], &[])?;
    let h = super::methods::getattr(cx, &r, "headers").await?;
    ops::setitem(&h, &V::str("Content-Type"), V::str("text/plain; version=1.0.0; charset=utf-8"))?;
    if gzip && accepts {
        ops::setitem(&h, &V::str("Content-Encoding"), V::str("gzip"))?;
    }
    Ok(r)
}
