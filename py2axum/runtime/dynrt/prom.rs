//! prometheus_client 0.26: `Counter`, `Gauge`, `Summary`, `Histogram`, `Info`, `Enum` (names built from
//! namespace/subsystem/unit, labels by position or keyword, children in creation order), the
//! `time()` / `count_exceptions()` / `track_inprogress()` context managers and decorators,
//! `CollectorRegistry` (duplicate detection, `target_info`), the default `REGISTRY` and
//! `generate_latest`, which writes the text format byte for byte (`floatToGoString`, label sorting
//! and escaping, `_created` series as trailing gauges).
//!
//! The default collectors of CPython (`GC_COLLECTOR`, `PLATFORM_COLLECTOR`, `PROCESS_COLLECTOR`) are
//! registered in `REGISTRY` and reserve their names, but produce no samples: the binary is not a
//! CPython process.
//!
//! Multiprocess mode (`PROMETHEUS_MULTIPROC_DIR` set at startup): every value is also written to the
//! per-process files of prometheus_client (`counter_<pid>.db`, `gauge_<mode>_<pid>.db`..., same layout
//! and keys), and `MultiProcessCollector` merges all the files of the directory like the library, so
//! the binary and Python workers can share it.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use indexmap::IndexMap;
use parking_lot::Mutex;

use super::methods::{b_float, call_value};
use super::v::*;
use super::{aio, ops, Cx, KwFn};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Counter,
    Gauge,
    Summary,
    Histogram,
    Info,
    Enum,
}

impl Kind {
    fn typ(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Summary => "summary",
            Kind::Histogram => "histogram",
            Kind::Info => "info",
            Kind::Enum => "stateset",
        }
    }
    fn class(self) -> &'static str {
        match self {
            Kind::Counter => "Counter",
            Kind::Gauge => "Gauge",
            Kind::Summary => "Summary",
            Kind::Histogram => "Histogram",
            Kind::Info => "Info",
            Kind::Enum => "Enum",
        }
    }
    fn reserved(self) -> &'static [&'static str] {
        match self {
            Kind::Summary => &["quantile"],
            Kind::Histogram => &["le"],
            _ => &[],
        }
    }
    /// the time series a metric of this type produces (`CollectorRegistry._get_names`)
    fn suffixes(typ: &str) -> &'static [&'static str] {
        match typ {
            "counter" => &["_total", "_created"],
            "summary" => &["_sum", "_count", "_created"],
            "histogram" => &["_bucket", "_sum", "_count", "_created"],
            "info" => &["_info"],
            _ => &[],
        }
    }
}

const MULTIPROC_MODES: [&str; 10] = ["all", "liveall", "min", "livemin", "max", "livemax", "sum", "livesum", "mostrecent", "livemostrecent"];

/// where a value lives in its multiprocess file: (file prefix, offset of its two doubles)
type Slot = (String, usize);

#[derive(Default)]
struct St {
    /// multiprocess mode: value (counter, gauge) / sum, count (summary) / sum, buckets (histogram)
    mp: Vec<Slot>,
    value: f64,
    created: f64,
    sum: f64,
    count: f64,
    buckets: Vec<f64>,
    info: Vec<(String, String)>,
    state: usize,
    func: Option<V>,
    /// the last exemplar of a counter / of each histogram bucket (not kept in multiprocess mode)
    exemplar: Option<Exemplar>,
    bucket_ex: Vec<Option<Exemplar>>,
}

/// A metric object: a labelled parent, a child (`labels(...)`) or a metric without labels.
pub struct Metric {
    kind: Kind,
    name: String,
    orig: String,
    namespace: String,
    subsystem: String,
    unit: String,
    doc: String,
    labelnames: Vec<String>,
    labelvalues: Vec<String>,
    bounds: Vec<f64>,
    states: Vec<String>,
    mode: String,
    children: Mutex<IndexMap<Vec<String>, Arc<Metric>>>,
    st: Mutex<St>,
}

impl Metric {
    fn is_parent(&self) -> bool {
        !self.labelnames.is_empty() && self.labelvalues.is_empty()
    }
    fn observable(&self) -> bool {
        !self.is_parent()
    }
    fn check_observable(&self) -> R<()> {
        if self.observable() {
            Ok(())
        } else {
            Err(Exc::value_error(format!("{} metric is missing label values", self.kind.typ())))
        }
    }
    fn str(&self) -> String {
        format!("{}:{}", self.kind.typ(), self.name)
    }
    fn repr(&self) -> String {
        format!("prometheus_client.metrics.{}({})", self.kind.class(), self.name)
    }
    fn most_recent(&self) -> bool {
        self.mode == "mostrecent" || self.mode == "livemostrecent"
    }
    fn names(&self) -> Vec<String> {
        let mut v = vec![self.name.clone()];
        v.extend(Kind::suffixes(self.kind.typ()).iter().map(|s| format!("{}{s}", self.name)));
        v
    }
    fn child(&self, values: Vec<String>) -> Arc<Metric> {
        let m = Metric {
            kind: self.kind,
            name: self.name.clone(),
            orig: self.orig.clone(),
            namespace: self.namespace.clone(),
            subsystem: self.subsystem.clone(),
            unit: self.unit.clone(),
            doc: self.doc.clone(),
            labelnames: self.labelnames.clone(),
            labelvalues: values,
            bounds: self.bounds.clone(),
            states: self.states.clone(),
            mode: self.mode.clone(),
            children: Mutex::new(IndexMap::new()),
            st: Mutex::new(St::default()),
        };
        m.init();
        Arc::new(m)
    }
    /// `_metric_init`
    fn init(&self) {
        let mut st = self.st.lock();
        st.created = now();
        if self.kind == Kind::Histogram {
            st.buckets = vec![0.0; self.bounds.len()];
            st.bucket_ex = vec![None; self.bounds.len()];
        }
        if let Some(mp) = multiproc() {
            // the values prometheus_client creates, in its order: (file type, sample name, extra label)
            let names: Vec<(String, Option<String>)> = match self.kind {
                Kind::Counter => vec![(format!("{}_total", self.name), None)],
                Kind::Gauge => vec![(self.name.clone(), None)],
                Kind::Summary => vec![(format!("{}_count", self.name), None), (format!("{}_sum", self.name), None)],
                Kind::Histogram => {
                    let mut v = vec![(format!("{}_sum", self.name), None)];
                    v.extend(self.bounds.iter().map(|b| (format!("{}_bucket", self.name), Some(go_float(*b)))));
                    v
                }
                Kind::Info | Kind::Enum => vec![],
            };
            let prefix = if self.kind == Kind::Gauge { format!("gauge_{}", self.mode) } else { self.kind.typ().to_string() };
            for (i, (sample, le)) in names.into_iter().enumerate() {
                let mut labels: Vec<(String, String)> = self.labelnames.iter().cloned().zip(self.labelvalues.iter().cloned()).collect();
                if let Some(le) = le {
                    labels.push(("le".into(), le));
                }
                let key = mmap_key(&self.name, &sample, &labels, &self.doc);
                let (pos, value) = match mp.slot(&prefix, &key) {
                    Ok(x) => x,
                    Err(e) => {
                        eprintln!("ERROR:py2axum:prometheus_client multiprocess file: {e}");
                        continue;
                    }
                };
                // a value the file already holds (a pid seen before), like MmapedValue
                match (self.kind, i) {
                    (Kind::Counter | Kind::Gauge, _) => st.value = value,
                    (Kind::Summary, 0) => st.count = value,
                    (Kind::Summary, _) | (Kind::Histogram, 0) => st.sum = value,
                    (Kind::Histogram, b) => st.buckets[b - 1] = value,
                    _ => {}
                }
                st.mp.push((prefix.clone(), pos));
            }
        }
    }
}

/// write a value to its multiprocess file (`MmapedValue.inc/set`)
fn persist(st: &St, i: usize, value: f64, ts: f64) {
    if let (Some(mp), Some((prefix, pos))) = (multiproc(), st.mp.get(i)) {
        mp.write(prefix, *pos, value, ts);
    }
}

/// The objects of this module held as Python values.
pub enum Prom {
    Metric(Arc<Metric>),
    Registry(Arc<Registry>),
    Timer(Timer),
    ExcCounter(Arc<Metric>, V),
    Inprogress(Arc<Metric>),
    /// `GC_COLLECTOR`, `PLATFORM_COLLECTOR`, `PROCESS_COLLECTOR`
    Builtin(&'static str),
    /// `multiprocess.MultiProcessCollector(registry, path)`
    MultiProc(Arc<String>),
    /// `registry.restricted_registry(names)`
    Restricted(Arc<Registry>, Arc<Vec<String>>),
}

pub struct Timer {
    metric: Mutex<Arc<Metric>>,
    start: Mutex<Option<Instant>>,
    duration: Mutex<Option<f64>>,
}

impl Timer {
    fn new(m: Arc<Metric>) -> Timer {
        Timer { metric: Mutex::new(m), start: Mutex::new(None), duration: Mutex::new(None) }
    }
}

fn prom(p: Prom) -> V {
    V::native(Native::Prom(Arc::new(p)))
}

fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn use_created() -> &'static AtomicBool {
    static U: OnceLock<AtomicBool> = OnceLock::new();
    U.get_or_init(|| {
        let v = std::env::var("PROMETHEUS_DISABLE_CREATED_SERIES").unwrap_or_else(|_| "False".into()).to_lowercase();
        AtomicBool::new(!matches!(v.as_str(), "true" | "1" | "t"))
    })
}

/// `disable_created_metrics()` / `enable_created_metrics()`
pub fn set_created(on: bool) -> R {
    use_created().store(on, Ordering::Relaxed);
    Ok(V::None)
}

// ---------------------------------------------------------------- registry

#[derive(Clone)]
enum Coll {
    Metric(Arc<Metric>),
    Builtin(&'static str),
    MultiProc(Arc<String>),
    /// the placeholder of `target_info` (`_EmptyCollector`)
    Empty,
}

impl Coll {
    fn same(&self, o: &Coll) -> bool {
        match (self, o) {
            (Coll::Metric(a), Coll::Metric(b)) => Arc::ptr_eq(a, b),
            (Coll::Builtin(a), Coll::Builtin(b)) => a == b,
            (Coll::MultiProc(a), Coll::MultiProc(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
    fn value(&self) -> V {
        match self {
            Coll::Metric(m) => prom(Prom::Metric(m.clone())),
            Coll::Builtin(n) => prom(Prom::Builtin(n)),
            Coll::MultiProc(p) => prom(Prom::MultiProc(p.clone())),
            Coll::Empty => V::None,
        }
    }
}

#[derive(Default)]
struct RegIn {
    colls: Vec<(Coll, Vec<String>)>,
    names: IndexMap<String, Coll>,
    target_info: Option<Vec<(String, String)>>,
}

pub struct Registry {
    inner: Mutex<RegIn>,
    /// names of collectors without `describe` taken from `collect` (the default REGISTRY)
    auto_describe: bool,
}

fn builtin_names(b: &str) -> Vec<String> {
    let counter = |n: &str| vec![n.to_string(), format!("{n}_total"), format!("{n}_created")];
    match b {
        "gc" => ["python_gc_objects_collected", "python_gc_objects_uncollectable", "python_gc_collections"].iter().flat_map(|n| counter(n)).collect(),
        "platform" => vec!["python_info".into()],
        // the process collector reads /proc: no series elsewhere
        "process" if cfg!(target_os = "linux") => {
            let mut v: Vec<String> = ["process_virtual_memory_bytes", "process_resident_memory_bytes", "process_start_time_seconds"].map(String::from).to_vec();
            v.extend(counter("process_cpu_seconds"));
            v.extend(["process_max_fds", "process_open_fds"].map(String::from));
            v
        }
        _ => vec![],
    }
}

fn default_registry() -> &'static Arc<Registry> {
    static R: OnceLock<Arc<Registry>> = OnceLock::new();
    R.get_or_init(|| {
        let r = Arc::new(Registry { inner: Mutex::new(RegIn::default()), auto_describe: true });
        for b in ["gc", "platform", "process"] {
            let _ = r.register(Coll::Builtin(b));
        }
        r
    })
}

fn set_repr(names: &[String]) -> String {
    format!("{{{}}}", names.iter().map(|n| ops::str_repr(n)).collect::<Vec<_>>().join(", "))
}

impl Registry {
    fn register(&self, c: Coll) -> R<()> {
        let names = match &c {
            Coll::Metric(m) => m.names(),
            Coll::Builtin(b) => builtin_names(b),
            Coll::MultiProc(path) if self.auto_describe => {
                let mut v = Vec::new();
                for f in mp_collect(path)? {
                    v.push(f.name.clone());
                    v.extend(Kind::suffixes(f.typ).iter().map(|s| format!("{}{s}", f.name)));
                }
                v
            }
            Coll::MultiProc(_) | Coll::Empty => vec![],
        };
        let mut g = self.inner.lock();
        // CPython prints a set (hash order); here the names in the collector's order
        let dups: Vec<String> = names.iter().filter(|n| g.names.contains_key(*n)).cloned().collect();
        if !dups.is_empty() {
            return Err(Exc::msg(&DUPLICATE_TIMESERIES, format!("Duplicated timeseries in CollectorRegistry: {}", set_repr(&dups))));
        }
        for n in &names {
            g.names.insert(n.clone(), c.clone());
        }
        match g.colls.iter_mut().find(|(x, _)| x.same(&c)) {
            Some(slot) => slot.1 = names,
            None => g.colls.push((c, names)),
        }
        Ok(())
    }
    fn unregister(&self, c: &Coll, v: &V) -> R<()> {
        let mut g = self.inner.lock();
        let Some(i) = g.colls.iter().position(|(x, _)| x.same(c)) else {
            return Err(Exc::new(&KEY_ERROR, vec![v.clone()]));
        };
        let (_, names) = g.colls.remove(i);
        for n in names {
            g.names.shift_remove(&n);
        }
        Ok(())
    }
    fn set_target_info(&self, labels: Option<Vec<(String, String)>>) -> R<()> {
        let mut g = self.inner.lock();
        match &labels {
            Some(l) if !l.is_empty() => {
                if g.target_info.as_ref().is_none_or(|t| t.is_empty()) && g.names.contains_key("target_info") {
                    return Err(Exc::value_error("CollectorRegistry already contains a target_info metric"));
                }
                g.names.insert("target_info".into(), Coll::Empty);
            }
            _ => {
                if g.target_info.as_ref().is_some_and(|t| !t.is_empty()) {
                    g.names.shift_remove("target_info");
                }
            }
        }
        g.target_info = labels;
        Ok(())
    }
}

// ---------------------------------------------------------------- arguments

fn bind(qual: &str, params: &[&str], required: usize, args: Vec<V>, kwargs: Vec<(String, V)>) -> R<Vec<Option<V>>> {
    if args.len() > params.len() {
        let range = if required == params.len() { format!("{}", required + 1) } else { format!("from {} to {}", required + 1, params.len() + 1) };
        return Err(Exc::type_error(format!("{qual}() takes {range} positional arguments but {} were given", args.len() + 1)));
    }
    let mut out: Vec<Option<V>> = vec![None; params.len()];
    for (i, a) in args.into_iter().enumerate() {
        out[i] = Some(a);
    }
    for (k, v) in kwargs {
        let Some(i) = params.iter().position(|p| *p == k) else {
            return Err(Exc::type_error(format!("{qual}() got an unexpected keyword argument '{k}'")));
        };
        if out[i].is_some() {
            return Err(Exc::type_error(format!("{qual}() got multiple values for argument '{k}'")));
        }
        out[i] = Some(v);
    }
    let missing: Vec<String> = params[..required].iter().zip(&out).filter(|(_, v)| v.is_none()).map(|(p, _)| format!("'{p}'")).collect();
    if !missing.is_empty() {
        let list = match missing.len() {
            1 => missing[0].clone(),
            n => format!("{} and {}", missing[..n - 1].join(", "), missing[n - 1]),
        };
        let s = if missing.len() == 1 { "" } else { "s" };
        return Err(Exc::type_error(format!("{qual}() missing {} required positional argument{s}: {list}", missing.len())));
    }
    Ok(out)
}

fn text(v: Option<&V>, what: &str) -> R<String> {
    match v {
        None | Some(V::None) => Ok(String::new()),
        Some(V::Str(s)) => Ok(s.to_string()),
        Some(o) if !ops::truthy(o)? => Ok(String::new()),
        Some(o) => Err(Exc::type_error(format!("can only concatenate str (not \"{}\") to str ({what})", o.type_name()))),
    }
}

/// `float(v)`
fn to_f(v: &V) -> R<f64> {
    match b_float(std::slice::from_ref(v))? {
        V::Float(f) => Ok(f),
        _ => Ok(f64::NAN),
    }
}

/// `amount` added to a float (`self._value += amount`)
fn amount(v: &V, op: &str) -> R<f64> {
    match v {
        V::Int(i) => Ok(*i as f64),
        V::Float(f) => Ok(*f),
        V::Bool(b) => Ok(*b as i64 as f64),
        o => Err(Exc::type_error(format!("unsupported operand type(s) for {op}: 'float' and '{}'", o.type_name()))),
    }
}

fn validate_labelname(l: &str) -> R<()> {
    // RESERVED_METRIC_LABEL_NAME_RE = ^__.*$ (`.` stops at a newline, `$` accepts a final one)
    if let Some(rest) = l.strip_prefix("__") {
        let rest = rest.strip_suffix('\n').unwrap_or(rest);
        if !rest.contains('\n') {
            return Err(Exc::value_error(format!("Reserved label metric name: {l}")));
        }
    }
    Ok(())
}

fn str_list(v: &V) -> R<Vec<String>> {
    ops::iter(v)?
        .iter()
        .map(|x| match x {
            V::Str(s) => Ok(s.to_string()),
            o => Err(Exc::attr_error(format!("'{}' object has no attribute 'encode'", o.type_name()))),
        })
        .collect()
}

// ---------------------------------------------------------------- constructors

/// `Counter(...)`, `Gauge(...)`, ...
pub fn new_metric(kind: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let kind = match kind {
        "Counter" => Kind::Counter,
        "Gauge" => Kind::Gauge,
        "Summary" => Kind::Summary,
        "Histogram" => Kind::Histogram,
        "Info" => Kind::Info,
        "Enum" => Kind::Enum,
        k => return Err(Exc::type_error(format!("py2axum: prometheus_client.{k} is not supported"))),
    };
    let mut params = vec!["name", "documentation", "labelnames", "namespace", "subsystem", "unit", "registry", "_labelvalues"];
    let qual = match kind {
        Kind::Gauge => {
            params.push("multiprocess_mode");
            "Gauge.__init__"
        }
        Kind::Histogram => {
            params.push("buckets");
            "Histogram.__init__"
        }
        Kind::Enum => {
            params.push("states");
            "Enum.__init__"
        }
        _ => "MetricWrapperBase.__init__",
    };
    let a = bind(qual, &params, 2, args, kwargs)?;
    if a[7].as_ref().is_some_and(|v| !v.is_none()) {
        return Err(Exc::type_error("py2axum: a positional _labelvalues is not supported"));
    }
    let labelnames = match &a[2] {
        Some(v) => str_list(v)?,
        None => vec![],
    };
    // the subclass checks that run before MetricWrapperBase.__init__
    let mut mode = String::from("all");
    let mut bounds = Vec::new();
    let mut states = Vec::new();
    match kind {
        Kind::Gauge => {
            if let Some(m) = &a[8] {
                mode = match m {
                    V::Str(s) => s.to_string(),
                    o => return Err(Exc::type_error(format!("can only concatenate str (not \"{}\") to str", o.type_name()))),
                };
            }
            if !MULTIPROC_MODES.contains(&mode.as_str()) {
                return Err(Exc::value_error(format!("Invalid multiprocess mode: {mode}")));
            }
        }
        Kind::Histogram => {
            bounds = match &a[8] {
                Some(b) => ops::iter(b)?.iter().map(|x| to_f(x)).collect::<R<Vec<f64>>>()?,
                None => vec![0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0, f64::INFINITY],
            };
            let mut sorted = bounds.clone();
            sorted.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
            if bounds != sorted {
                return Err(Exc::value_error("Buckets not in sorted order"));
            }
            if bounds.last().is_some_and(|l| *l != f64::INFINITY) {
                bounds.push(f64::INFINITY);
            }
            if bounds.len() < 2 {
                return Err(Exc::value_error("Must have at least two buckets"));
            }
        }
        Kind::Enum => {
            let name = match &a[0] {
                Some(V::Str(s)) => s.to_string(),
                _ => String::new(),
            };
            if labelnames.contains(&name) {
                return Err(Exc::value_error(format!("Overlapping labels for Enum metric: {name}")));
            }
            let given = match &a[8] {
                Some(v) if ops::truthy(v)? => v.clone(),
                _ => return Err(Exc::value_error(format!("No states provided for Enum metric: {name}"))),
            };
            for s in ops::iter(&given)? {
                match s {
                    V::Str(s) => states.push(s.to_string()),
                    o => return Err(Exc::type_error(format!("py2axum: Enum states must be str, not {}", o.type_name()))),
                }
            }
        }
        _ => {}
    }
    // _build_full_name
    let name = a[0].clone().unwrap_or(V::None);
    if !ops::truthy(&name)? {
        return Err(Exc::value_error("Metric name should not be empty"));
    }
    let (namespace, subsystem, unit) = (text(a[3].as_ref(), "namespace")?, text(a[4].as_ref(), "subsystem")?, text(a[5].as_ref(), "unit")?);
    let orig = match &name {
        V::Str(s) => s.to_string(),
        o => return Err(Exc::type_error(format!("can only concatenate str (not \"{}\") to str", o.type_name()))),
    };
    let mut full = String::new();
    if !namespace.is_empty() {
        full += &namespace;
        full.push('_');
    }
    if !subsystem.is_empty() {
        full += &subsystem;
        full.push('_');
    }
    full += &orig;
    if kind == Kind::Counter && full.ends_with("_total") {
        full.truncate(full.len() - 6);
    }
    if !unit.is_empty() && !full.ends_with(&format!("_{unit}")) {
        full = format!("{full}_{unit}");
    }
    if !unit.is_empty() && matches!(kind, Kind::Info | Kind::Enum) {
        return Err(Exc::value_error(format!("Metric name is of a type that cannot have a unit: {full}")));
    }
    for l in &labelnames {
        validate_labelname(l)?;
        if kind.reserved().contains(&l.as_str()) {
            // sic: prometheus_client's message
            return Err(Exc::value_error(format!("Reserved label methe fric name: {l}")));
        }
    }
    let doc = match &a[1] {
        Some(V::Str(s)) => s.to_string(),
        Some(o) => return Err(Exc::type_error(format!("py2axum: the documentation of a metric must be a str, not {}", o.type_name()))),
        None => unreachable!(),
    };
    let m = Arc::new(Metric {
        kind,
        name: full,
        orig,
        namespace,
        subsystem,
        unit,
        doc,
        labelnames,
        labelvalues: vec![],
        bounds,
        states,
        mode,
        children: Mutex::new(IndexMap::new()),
        st: Mutex::new(St::default()),
    });
    if m.observable() {
        m.init();
    }
    let registry = match &a[6] {
        None => Some(default_registry().clone()),
        Some(V::None) => None,
        Some(V::Native(n)) => match &**n {
            Native::Prom(p) => match &**p {
                Prom::Registry(r) => Some(r.clone()),
                _ => return Err(Exc::type_error("py2axum: registry= must be a CollectorRegistry")),
            },
            _ => return Err(Exc::type_error("py2axum: registry= must be a CollectorRegistry")),
        },
        Some(o) if !ops::truthy(o)? => None,
        Some(_) => return Err(Exc::type_error("py2axum: registry= must be a CollectorRegistry")),
    };
    if let Some(r) = registry {
        r.register(Coll::Metric(m.clone()))?;
    }
    Ok(prom(Prom::Metric(m)))
}

fn str_pairs(v: &V, what: &str) -> R<Vec<(String, String)>> {
    let V::Dict(d) = v else {
        return Err(Exc::type_error(format!("py2axum: {what} must be a dict")));
    };
    d.lock()
        .values()
        .map(|(k, x)| match (k, x) {
            (V::Str(k), V::Str(x)) => Ok((k.to_string(), x.to_string())),
            _ => Err(Exc::type_error(format!("py2axum: {what} must map str to str"))),
        })
        .collect()
}

fn pairs_dict(p: &[(String, String)]) -> R {
    V::dict_from(p.iter().map(|(k, v)| (V::str(k), V::str(v))).collect())
}

/// `CollectorRegistry(auto_describe=False, target_info=None, support_collectors_without_names=False)`
pub fn registry_new(args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let a = bind("CollectorRegistry.__init__", &["auto_describe", "target_info", "support_collectors_without_names"], 0, args, kwargs)?;
    let auto_describe = a[0].as_ref().map(ops::truthy).transpose()?.unwrap_or(false);
    let r = Arc::new(Registry { inner: Mutex::new(RegIn::default()), auto_describe });
    if let Some(t) = &a[1] {
        if ops::truthy(t)? {
            r.set_target_info(Some(str_pairs(t, "target_info")?))?;
        }
    }
    Ok(prom(Prom::Registry(r)))
}

/// the module's values: `REGISTRY`, the default collectors
pub fn value(name: &str) -> V {
    match name {
        "REGISTRY" => prom(Prom::Registry(default_registry().clone())),
        "GC_COLLECTOR" => prom(Prom::Builtin("gc")),
        "PLATFORM_COLLECTOR" => prom(Prom::Builtin("platform")),
        "PROCESS_COLLECTOR" => prom(Prom::Builtin("process")),
        _ => V::None,
    }
}

pub fn type_name(p: &Prom) -> &'static str {
    match p {
        Prom::Metric(m) => m.kind.class(),
        Prom::Registry(_) => "CollectorRegistry",
        Prom::Timer(_) => "Timer",
        Prom::ExcCounter(..) => "ExceptionCounter",
        Prom::Inprogress(_) => "InprogressTracker",
        Prom::Builtin("gc") => "GCCollector",
        Prom::Builtin("platform") => "PlatformCollector",
        Prom::Builtin(_) => "ProcessCollector",
        Prom::MultiProc(_) => "MultiProcessCollector",
        Prom::Restricted(..) => "RestrictedRegistry",
    }
}

fn builtin_module(b: &str) -> &'static str {
    match b {
        "gc" => "gc_collector",
        "platform" => "platform_collector",
        _ => "process_collector",
    }
}

/// `repr()`; objects without their own repr show `0x0` for CPython's address
pub fn repr(p: &Prom) -> String {
    match p {
        Prom::Metric(m) => m.repr(),
        Prom::Registry(_) => "<prometheus_client.registry.CollectorRegistry object at 0x0>".into(),
        Prom::Timer(_) => "<prometheus_client.context_managers.Timer object at 0x0>".into(),
        Prom::ExcCounter(..) => "<prometheus_client.context_managers.ExceptionCounter object at 0x0>".into(),
        Prom::Inprogress(_) => "<prometheus_client.context_managers.InprogressTracker object at 0x0>".into(),
        Prom::Builtin(b) => format!("<prometheus_client.{}.{} object at 0x0>", builtin_module(b), type_name(p)),
        Prom::MultiProc(_) => "<prometheus_client.multiprocess.MultiProcessCollector object at 0x0>".into(),
        Prom::Restricted(..) => "<prometheus_client.registry.RestrictedRegistry object at 0x0>".into(),
    }
}

/// `str()`
pub fn str(p: &Prom) -> String {
    match p {
        Prom::Metric(m) => m.str(),
        _ => repr(p),
    }
}

fn as_metric(v: &V) -> Option<Arc<Metric>> {
    match v {
        V::Native(n) => match &**n {
            Native::Prom(p) => match &**p {
                Prom::Metric(m) => Some(m.clone()),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

/// `isinstance(v, prometheus_client.X)`
pub fn isinstance(v: &V, class: &str) -> bool {
    let V::Native(n) = v else { return false };
    let Native::Prom(p) = &**n else { return false };
    match class {
        "CollectorRegistry" => matches!(&**p, Prom::Registry(_)),
        c => matches!(&**p, Prom::Metric(m) if m.kind.class() == c),
    }
}

// ---------------------------------------------------------------- operations

fn labels(m: &Arc<Metric>, args: Vec<V>, kwargs: Vec<(String, V)>) -> R<Arc<Metric>> {
    if m.labelnames.is_empty() {
        return Err(Exc::value_error(format!("No label names were set when constructing {}", m.str())));
    }
    if !m.labelvalues.is_empty() {
        let items: Vec<String> = m.labelnames.iter().zip(&m.labelvalues).map(|(k, v)| format!("{}: {}", ops::str_repr(k), ops::str_repr(v))).collect();
        return Err(Exc::value_error(format!("{} already has labels set ({{{}}}); can not chain calls to .labels()", m.str(), items.join(", "))));
    }
    if !args.is_empty() && !kwargs.is_empty() {
        return Err(Exc::value_error("Can't pass both *args and **kwargs"));
    }
    let values: Vec<String> = if !kwargs.is_empty() {
        let mut given: Vec<&str> = kwargs.iter().map(|(k, _)| k.as_str()).collect();
        let mut want: Vec<&str> = m.labelnames.iter().map(|s| s.as_str()).collect();
        given.sort();
        want.sort();
        if given != want {
            return Err(Exc::value_error("Incorrect label names"));
        }
        let mut out = Vec::new();
        for l in &m.labelnames {
            let v = &kwargs.iter().find(|(k, _)| k == l).unwrap().1;
            out.push(ops::str_(v)?);
        }
        out
    } else {
        if args.len() != m.labelnames.len() {
            return Err(Exc::value_error("Incorrect label count"));
        }
        args.iter().map(ops::str_).collect::<R<_>>()?
    };
    let mut ch = m.children.lock();
    if let Some(c) = ch.get(&values) {
        return Ok(c.clone());
    }
    let c = m.child(values.clone());
    ch.insert(values, c.clone());
    Ok(c)
}

fn no_attr(m: &Metric, name: &str) -> Exc {
    Exc::attr_error(format!("'{}' object has no attribute '{name}'", m.kind.class()))
}

/// `_validate_exemplar`, then the exemplar the value keeps (`set_exemplar`; a no-op in multiprocess mode)
fn validate_exemplar(e: &V, value: f64) -> R<Option<Exemplar>> {
    let mut runes = 0;
    let labels = str_pairs(e, "exemplar")?;
    for (k, v) in &labels {
        validate_labelname(k)?;
        runes += k.chars().count() + v.chars().count();
    }
    if runes > 128 {
        // sic: prometheus_client does not format this message
        return Err(Exc::value_error("Exemplar labels have %d UTF-8 characters, exceeding the limit of 128"));
    }
    Ok(multiproc().is_none().then(|| Exemplar { labels, value, ts: now() }))
}

fn observe(m: &Metric, x: &V, exemplar: Option<&V>) -> R<()> {
    m.check_observable()?;
    let v = amount(x, "+=")?;
    let mut st = m.st.lock();
    if m.kind == Kind::Summary {
        st.count += 1.0;
        st.sum += v;
        persist(&st, 0, st.count, 0.0);
        persist(&st, 1, st.sum, 0.0);
        return Ok(());
    }
    st.sum += v;
    persist(&st, 0, st.sum, 0.0);
    if let Some(i) = m.bounds.iter().position(|b| v <= *b) {
        st.buckets[i] += 1.0;
        persist(&st, i + 1, st.buckets[i], 0.0);
        drop(st);
        if let Some(e) = exemplar.filter(|e| !e.is_none()) {
            if ops::truthy(e)? {
                let ex = validate_exemplar(e, v)?;
                m.st.lock().bucket_ex[i] = ex;
            }
        }
    }
    Ok(())
}

fn gauge_set(m: &Metric, x: &V) -> R<()> {
    m.check_observable()?;
    let f = to_f(x)?;
    let mut st = m.st.lock();
    st.value = f;
    persist(&st, 0, f, if m.most_recent() { now() } else { 0.0 });
    Ok(())
}

/// the callback of a `Timer` on exit (`set` for a gauge, `observe` otherwise)
fn timer_done(m: &Metric, d: f64) -> R<()> {
    if m.kind == Kind::Gauge {
        gauge_set(m, &V::Float(d))
    } else {
        observe(m, &V::Float(d), None)
    }
}

fn counter_inc(m: &Metric, by: f64) -> R<()> {
    m.check_observable()?;
    add(m, by);
    Ok(())
}

/// `self._value.inc(amount)`
fn add(m: &Metric, by: f64) {
    let mut st = m.st.lock();
    st.value += by;
    persist(&st, 0, st.value, 0.0);
}

fn exc_matches(e: &Exc, t: &V) -> R<bool> {
    super::types::isinstance(&V::Exc(e.clone()), t)
}

pub async fn method(cx: &Cx, p: &Prom, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    match p {
        Prom::Metric(m) => metric_method(cx, m, name, args, kwargs).await,
        Prom::Registry(r) => registry_method(cx, r, name, args, kwargs).await,
        Prom::Timer(t) if name == "labels" => {
            let m = t.metric.lock().clone();
            let c = labels(&m, args, kwargs)?;
            *t.metric.lock() = c;
            Ok(V::None)
        }
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", type_name(p)))),
    }
}

async fn metric_method(cx: &Cx, m: &Arc<Metric>, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let class = m.kind.class();
    let q = |meth: &str| format!("{class}.{meth}");
    match (m.kind, name) {
        (_, "labels") => Ok(prom(Prom::Metric(labels(m, args, kwargs)?))),
        (_, "remove") => {
            if !kwargs.is_empty() {
                return Err(Exc::type_error(format!("{class}.remove() got an unexpected keyword argument '{}'", kwargs[0].0)));
            }
            if m.labelnames.is_empty() {
                return Err(Exc::value_error(format!("No label names were set when constructing {}", m.str())));
            }
            if args.len() != m.labelnames.len() {
                return Err(Exc::value_error(format!("Incorrect label count (expected {}, got {})", m.labelnames.len(), ops::repr(&V::tuple(args))?)));
            }
            let values: Vec<String> = args.iter().map(ops::str_).collect::<R<_>>()?;
            m.children.lock().shift_remove(&values);
            Ok(V::None)
        }
        (_, "remove_by_labels") => {
            let a = bind(&q("remove_by_labels"), &["labels"], 1, args, kwargs)?;
            let l = a[0].clone().unwrap();
            if m.labelnames.is_empty() {
                return Err(Exc::value_error(format!("No label names were set when constructing {}", m.str())));
            }
            let V::Dict(d) = &l else {
                return Err(Exc::type_error("labels must be a dict of {label_name: label_value}"));
            };
            let items: Vec<(V, V)> = d.lock().values().cloned().collect();
            if items.is_empty() {
                return Ok(V::None);
            }
            let invalid: Vec<V> = items.iter().filter(|(k, _)| !matches!(k, V::Str(s) if m.labelnames.iter().any(|n| n == &**s))).map(|(k, _)| k.clone()).collect();
            if !invalid.is_empty() {
                let names = V::tuple(m.labelnames.iter().map(V::str).collect());
                return Err(Exc::value_error(format!("Unknown label names: {}; expected {}", ops::repr(&V::list(invalid))?, ops::repr(&names)?)));
            }
            let mut filter = Vec::new();
            for (k, v) in &items {
                let pos = m.labelnames.iter().position(|n| Some(n.as_str()) == k.as_str()).unwrap();
                filter.push((pos, ops::str_(v)?));
            }
            m.children.lock().retain(|lv, _| !filter.iter().all(|(p, w)| &lv[*p] == w));
            Ok(V::None)
        }
        (_, "clear") => {
            bind(&q("clear"), &[], 0, args, kwargs)?;
            if !m.labelnames.is_empty() {
                m.children.lock().clear();
            }
            Ok(V::None)
        }
        (Kind::Counter, "inc") => {
            let a = bind(&q("inc"), &["amount", "exemplar"], 0, args, kwargs)?;
            m.check_observable()?;
            let by = a[0].clone().unwrap_or(V::Int(1));
            if ops::cmp(&by, &V::Int(0))? == std::cmp::Ordering::Less {
                return Err(Exc::value_error("Counters can only be incremented by non-negative amounts."));
            }
            let n = amount(&by, "+=")?;
            counter_inc(m, n)?;
            if let Some(e) = &a[1] {
                if ops::truthy(e)? {
                    let ex = validate_exemplar(e, n)?;
                    m.st.lock().exemplar = ex;
                }
            }
            Ok(V::None)
        }
        (Kind::Counter, "reset") => {
            bind(&q("reset"), &[], 0, args, kwargs)?;
            if !m.observable() {
                return Err(no_attr(m, "_value"));
            }
            let mut st = m.st.lock();
            st.value = 0.0;
            st.created = now();
            persist(&st, 0, 0.0, 0.0);
            Ok(V::None)
        }
        (Kind::Counter, "count_exceptions") => {
            let a = bind(&q("count_exceptions"), &["exception"], 0, args, kwargs)?;
            m.check_observable()?;
            Ok(prom(Prom::ExcCounter(m.clone(), a[0].clone().unwrap_or(V::Class(&EXCEPTION)))))
        }
        (Kind::Gauge, "inc" | "dec") => {
            let a = bind(&q(name), &["amount"], 0, args, kwargs)?;
            if m.most_recent() {
                return Err(Exc::runtime(format!("{name} must not be used with the mostrecent mode")));
            }
            m.check_observable()?;
            let by = a[0].clone().unwrap_or(V::Int(1));
            let by = if name == "dec" {
                match &by {
                    V::Int(_) | V::Float(_) | V::Bool(_) => -amount(&by, "+=")?,
                    o => return Err(Exc::type_error(format!("bad operand type for unary -: '{}'", o.type_name()))),
                }
            } else {
                amount(&by, "+=")?
            };
            add(m, by);
            Ok(V::None)
        }
        (Kind::Gauge, "set") => {
            let a = bind(&q("set"), &["value"], 1, args, kwargs)?;
            gauge_set(m, a[0].as_ref().unwrap())?;
            Ok(V::None)
        }
        (Kind::Gauge, "set_to_current_time") => {
            bind(&q("set_to_current_time"), &[], 0, args, kwargs)?;
            gauge_set(m, &V::Float(now()))?;
            Ok(V::None)
        }
        (Kind::Gauge, "track_inprogress") => {
            bind(&q("track_inprogress"), &[], 0, args, kwargs)?;
            m.check_observable()?;
            Ok(prom(Prom::Inprogress(m.clone())))
        }
        (Kind::Gauge, "set_function") => {
            let a = bind(&q("set_function"), &["f"], 1, args, kwargs)?;
            m.check_observable()?;
            m.st.lock().func = a[0].clone();
            Ok(V::None)
        }
        (Kind::Gauge | Kind::Summary | Kind::Histogram, "time") => {
            bind(&q("time"), &[], 0, args, kwargs)?;
            Ok(prom(Prom::Timer(Timer::new(m.clone()))))
        }
        (Kind::Summary, "observe") => {
            let a = bind(&q("observe"), &["amount"], 1, args, kwargs)?;
            observe(m, a[0].as_ref().unwrap(), None)?;
            Ok(V::None)
        }
        (Kind::Histogram, "observe") => {
            let a = bind(&q("observe"), &["amount", "exemplar"], 1, args, kwargs)?;
            observe(m, a[0].as_ref().unwrap(), a[1].as_ref())?;
            Ok(V::None)
        }
        (Kind::Info, "info") => {
            let a = bind(&q("info"), &["val"], 1, args, kwargs)?;
            if !m.observable() {
                return Err(no_attr(m, "_labelname_set"));
            }
            let val = a[0].clone().unwrap();
            let V::Dict(d) = &val else {
                return Err(Exc::attr_error(format!("'{}' object has no attribute 'keys'", val.type_name())));
            };
            let items: Vec<(V, V)> = d.lock().values().cloned().collect();
            if items.iter().any(|(k, _)| matches!(k, V::Str(s) if m.labelnames.iter().any(|n| n == &**s))) {
                let names = V::tuple(m.labelnames.iter().map(V::str).collect());
                return Err(Exc::value_error(format!("Overlapping labels for Info metric, metric: {} child: {}", ops::repr(&names)?, ops::repr(&val)?)));
            }
            if items.iter().any(|(_, v)| v.is_none()) {
                return Err(Exc::value_error("Label value cannot be None"));
            }
            m.st.lock().info = str_pairs(&val, "the value of an Info metric")?;
            Ok(V::None)
        }
        (Kind::Enum, "state") => {
            let a = bind(&q("state"), &["state"], 1, args, kwargs)?;
            m.check_observable()?;
            let s = a[0].clone().unwrap();
            let i = m.states.iter().position(|x| Some(x.as_str()) == s.as_str()).ok_or_else(|| {
                Exc::value_error(if super::python() >= (3, 14) { "list.index(x): x not in list".to_string() } else { format!("{} is not in list", ops::repr(&s).unwrap_or_default()) })
            })?;
            m.st.lock().state = i;
            Ok(V::None)
        }
        _ => {
            let _ = cx;
            Err(no_attr(m, name))
        }
    }
}

fn as_coll(v: &V) -> R<Coll> {
    if let V::Native(n) = v {
        if let Native::Prom(p) = &**n {
            match &**p {
                Prom::Metric(m) => return Ok(Coll::Metric(m.clone())),
                Prom::Builtin(b) => return Ok(Coll::Builtin(b)),
                Prom::MultiProc(p) => return Ok(Coll::MultiProc(p.clone())),
                _ => {}
            }
        }
    }
    Err(Exc::type_error(format!("py2axum: only prometheus_client metrics and default collectors can be registered, not {}", v.type_name())))
}

async fn registry_method(cx: &Cx, r: &Arc<Registry>, name: &str, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let q = format!("CollectorRegistry.{name}");
    match name {
        "register" => {
            let a = bind(&q, &["collector"], 1, args, kwargs)?;
            r.register(as_coll(a[0].as_ref().unwrap())?)?;
            Ok(V::None)
        }
        "unregister" => {
            let a = bind(&q, &["collector"], 1, args, kwargs)?;
            let v = a[0].clone().unwrap();
            r.unregister(&as_coll(&v)?, &v)?;
            Ok(V::None)
        }
        "get_sample_value" => {
            let a = bind(&q, &["name", "labels"], 1, args, kwargs)?;
            let want = a[0].clone().unwrap();
            let want_labels: Vec<(V, V)> = match &a[1] {
                None | Some(V::None) => vec![],
                Some(V::Dict(d)) => d.lock().values().cloned().collect(),
                Some(o) => return Err(Exc::type_error(format!("py2axum: get_sample_value(labels=) must be a dict, not {}", o.type_name()))),
            };
            for fam in collect(cx, &Source::Registry(r.clone())).await? {
                for s in fam.samples {
                    if Some(s.name.as_str()) != want.as_str() || s.labels.len() != want_labels.len() {
                        continue;
                    }
                    if want_labels.iter().all(|(k, v)| s.labels.iter().any(|(a, b)| Some(a.as_str()) == k.as_str() && Some(b.as_str()) == v.as_str())) {
                        return Ok(V::Float(s.value));
                    }
                }
            }
            Ok(V::None)
        }
        "restricted_registry" => {
            let a = bind(&q, &["names"], 1, args, kwargs)?;
            let mut names: Vec<String> = Vec::new();
            for n in ops::iter(a[0].as_ref().unwrap())? {
                let n = ops::str_(&n)?;
                if !names.contains(&n) {
                    names.push(n);
                }
            }
            Ok(prom(Prom::Restricted(r.clone(), Arc::new(names))))
        }
        "get_target_info" => {
            bind(&q, &[], 0, args, kwargs)?;
            match &r.inner.lock().target_info {
                Some(t) => pairs_dict(t),
                None => Ok(V::None),
            }
        }
        "set_target_info" => {
            let a = bind(&q, &["labels"], 1, args, kwargs)?;
            let l = a[0].clone().unwrap();
            r.set_target_info(if ops::truthy(&l)? { Some(str_pairs(&l, "target_info")?) } else if l.is_none() { None } else { Some(vec![]) })?;
            Ok(V::None)
        }
        _ => Err(Exc::attr_error(format!("'CollectorRegistry' object has no attribute '{name}'"))),
    }
}

/// attributes read as values
pub fn attr(p: &Arc<Prom>, name: &str) -> R {
    match (&**p, name) {
        (Prom::Timer(t), "duration") => Ok(t.duration.lock().map(V::Float).unwrap_or(V::None)),
        (Prom::Registry(r), "_names_to_collectors") => {
            let g = r.inner.lock();
            V::dict_from(g.names.iter().map(|(k, c)| (V::str(k), c.value())).collect())
        }
        (Prom::Metric(m), "_name") => Ok(V::str(&m.name)),
        (Prom::Metric(m), "_documentation") => Ok(V::str(&m.doc)),
        (Prom::Metric(m), "_type") => Ok(V::str(m.kind.typ())),
        (Prom::Metric(m), "_unit") => Ok(V::str(&m.unit)),
        (Prom::Metric(m), "_labelnames") => Ok(V::tuple(m.labelnames.iter().map(V::str).collect())),
        (Prom::Metric(m), "_labelvalues") => Ok(V::tuple(m.labelvalues.iter().map(V::str).collect())),
        (Prom::Metric(_) | Prom::Registry(_) | Prom::Timer(_), _) => {
            // a bound method held as a value
            let (p2, n) = (p.clone(), name.to_string());
            let call: KwFn = Arc::new(move |cx: &Cx, args: Vec<V>, kwargs: Vec<(String, V)>| {
                let (p, n) = (p2.clone(), n.clone());
                Box::pin(async move { method(cx, &p, &n, args, kwargs).await })
            });
            Ok(V::native(Native::PyFn(PyFn { call, is_async: false, attrs: Mutex::new(vec![("__name__".into(), V::str(name))]) })))
        }
        _ => Err(Exc::attr_error(format!("'{}' object has no attribute '{name}'", type_name(p)))),
    }
}

// ---------------------------------------------------------------- context managers and decorators

/// `with m.time()` / `with c.count_exceptions()` / `with g.track_inprogress()`
pub fn enter(v: &V, p: &Prom) -> R {
    match p {
        Prom::Timer(t) => {
            *t.start.lock() = Some(Instant::now());
            Ok(v.clone())
        }
        Prom::ExcCounter(..) => Ok(V::None),
        Prom::Inprogress(m) => {
            add(m, 1.0);
            Ok(V::None)
        }
        _ => Err(Exc::attr_error(format!("'{}' object does not support the context manager protocol", type_name(p)))),
    }
}

pub fn exit(p: &Prom, exc: Option<&Exc>) -> R {
    match p {
        Prom::Timer(t) => {
            let d = t.start.lock().map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0).max(0.0);
            *t.duration.lock() = Some(d);
            let m = t.metric.lock().clone();
            timer_done(&m, d)?;
        }
        Prom::ExcCounter(m, ty) => {
            if let Some(e) = exc {
                if exc_matches(e, ty)? {
                    counter_inc(m, 1.0)?;
                }
            }
        }
        Prom::Inprogress(m) => add(m, -1.0),
        _ => return Err(Exc::attr_error(format!("'{}' object does not support the context manager protocol", type_name(p)))),
    }
    Ok(V::Bool(false))
}

/// `@m.time()`, `@c.count_exceptions()`, `@g.track_inprogress()`: a plain (not async) function, like the
/// `decorator` package prometheus_client uses; on an `async def` it measures the creation of the
/// coroutine, as in CPython.
pub fn decorate(p: &Arc<Prom>, args: &[V]) -> R {
    let [f] = args else {
        return Err(Exc::type_error(format!("py2axum: '{}' object takes one function", type_name(p))));
    };
    if !matches!(&**p, Prom::Timer(_) | Prom::ExcCounter(..) | Prom::Inprogress(_)) {
        return Err(Exc::type_error(format!("'{}' object is not callable", type_name(p))));
    }
    let (p2, target) = (p.clone(), f.clone());
    let call: KwFn = Arc::new(move |cx: &Cx, args: Vec<V>, kwargs: Vec<(String, V)>| {
        let (p, f) = (p2.clone(), target.clone());
        Box::pin(async move {
            // a new timer per call (`Timer._new_timer`)
            let cm: Arc<Prom> = match &*p {
                Prom::Timer(t) => Arc::new(Prom::Timer(Timer::new(t.metric.lock().clone()))),
                _ => p.clone(),
            };
            let cmv = V::native(Native::Prom(cm.clone()));
            enter(&cmv, &cm)?;
            let r = aio::call_value_lazy(cx, &f, args, kwargs).await;
            exit(&cm, r.as_ref().err())?;
            r
        })
    });
    let mut attrs: Vec<(String, V)> = match f {
        V::Native(n) => match &**n {
            Native::PyFn(pf) => pf.attrs.lock().iter().filter(|(k, _)| k != "__wrapped__").cloned().collect(),
            _ => vec![],
        },
        _ => vec![],
    };
    attrs.push(("__wrapped__".into(), f.clone()));
    Ok(V::native(Native::PyFn(PyFn { call, is_async: false, attrs: Mutex::new(attrs) })))
}

// ---------------------------------------------------------------- collection and text formats

/// `Exemplar(labels, value, timestamp)`
#[derive(Clone)]
struct Exemplar {
    labels: Vec<(String, String)>,
    value: f64,
    ts: f64,
}

struct Sample {
    name: String,
    labels: Vec<(String, String)>,
    value: f64,
    exemplar: Option<Exemplar>,
}

struct Family {
    name: String,
    doc: String,
    typ: &'static str,
    unit: String,
    samples: Vec<Sample>,
}

enum Source {
    Registry(Arc<Registry>),
    Metric(Arc<Metric>),
    MultiProc(Arc<String>),
    /// `registry.restricted_registry(names)`
    Restricted(Arc<Registry>, Arc<Vec<String>>),
}

async fn child_samples(cx: &Cx, m: &Metric, prefix: &[(String, String)], out: &mut Vec<Sample>) -> R<()> {
    let created = use_created().load(Ordering::Relaxed);
    let lab = |extra: Vec<(String, String)>| {
        let mut l = prefix.to_vec();
        for (k, v) in extra {
            match l.iter_mut().find(|(a, _)| *a == k) {
                Some(slot) => slot.1 = v,
                None => l.push((k, v)),
            }
        }
        l
    };
    let mut push = |suffix: &str, labels: Vec<(String, String)>, value: f64, exemplar: Option<Exemplar>| {
        out.push(Sample { name: format!("{}{suffix}", m.name), labels: lab(labels), value, exemplar })
    };
    let func = m.st.lock().func.clone();
    if let Some(f) = func {
        let r = call_value(cx, &f, vec![], vec![]).await?;
        let x = to_f(&r)?;
        push("", vec![], x, None);
        return Ok(());
    }
    let st = m.st.lock();
    match m.kind {
        Kind::Counter => {
            push("_total", vec![], st.value, st.exemplar.clone());
            if created {
                push("_created", vec![], st.created, None);
            }
        }
        Kind::Gauge => push("", vec![], st.value, None),
        Kind::Summary => {
            push("_count", vec![], st.count, None);
            push("_sum", vec![], st.sum, None);
            if created {
                push("_created", vec![], st.created, None);
            }
        }
        Kind::Histogram => {
            let mut acc = 0.0;
            for (i, b) in m.bounds.iter().enumerate() {
                acc += st.buckets[i];
                push("_bucket", vec![("le".into(), go_float(*b))], acc, st.bucket_ex.get(i).cloned().flatten());
            }
            push("_count", vec![], acc, None);
            if m.bounds[0] >= 0.0 {
                push("_sum", vec![], st.sum, None);
            }
            if created {
                push("_created", vec![], st.created, None);
            }
        }
        Kind::Info => push("_info", st.info.clone(), 1.0, None),
        Kind::Enum => {
            for (i, s) in m.states.iter().enumerate() {
                push("", vec![(m.name.clone(), s.clone())], if i == st.state { 1.0 } else { 0.0 }, None);
            }
        }
    }
    Ok(())
}

async fn metric_family(cx: &Cx, m: &Metric) -> R<Family> {
    let mut samples = Vec::new();
    if m.is_parent() {
        let children: Vec<(Vec<String>, Arc<Metric>)> = m.children.lock().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (values, c) in children {
            let series: Vec<(String, String)> = m.labelnames.iter().cloned().zip(values).collect();
            Box::pin(child_samples(cx, &c, &series, &mut samples)).await?;
        }
    } else {
        Box::pin(child_samples(cx, m, &[], &mut samples)).await?;
    }
    Ok(Family { name: m.name.clone(), doc: m.doc.clone(), typ: m.kind.typ(), unit: m.unit.clone(), samples })
}

fn target_family(t: Vec<(String, String)>) -> Family {
    Family { name: "target".into(), doc: "Target metadata".into(), typ: "info", unit: String::new(), samples: vec![Sample { name: "target_info".into(), labels: t, value: 1.0, exemplar: None }] }
}

async fn coll_families(cx: &Cx, c: &Coll) -> R<Vec<Family>> {
    Ok(match c {
        Coll::Metric(m) => vec![metric_family(cx, m).await?],
        Coll::MultiProc(path) => mp_collect(path)?,
        _ => vec![],
    })
}

async fn collect(cx: &Cx, src: &Source) -> R<Vec<Family>> {
    let mut out = Vec::new();
    match src {
        Source::Metric(m) => out.push(metric_family(cx, m).await?),
        Source::MultiProc(path) => out.extend(mp_collect(path)?),
        Source::Registry(r) => {
            let (colls, ti): (Vec<Coll>, Option<Vec<(String, String)>>) = {
                let g = r.inner.lock();
                (g.colls.iter().map(|(c, _)| c.clone()).collect(), g.target_info.clone().filter(|t| !t.is_empty()))
            };
            if let Some(t) = ti {
                out.push(target_family(t));
            }
            for c in colls {
                out.extend(coll_families(cx, &c).await?);
            }
        }
        Source::Restricted(r, names) => {
            // CPython iterates a set of collectors (by object hash): here, in registration order
            let (colls, ti): (Vec<Coll>, Option<Vec<(String, String)>>) = {
                let g = r.inner.lock();
                let picked: Vec<Coll> = g
                    .colls
                    .iter()
                    .filter(|(c, _)| names.iter().any(|n| n != "target_info" && g.names.get(n).is_some_and(|x| x.same(c))))
                    .map(|(c, _)| c.clone())
                    .collect();
                (picked, g.target_info.clone().filter(|t| !t.is_empty() && names.iter().any(|n| n == "target_info")))
            };
            if let Some(t) = ti {
                out.push(target_family(t));
            }
            for c in colls {
                for mut f in coll_families(cx, &c).await? {
                    // `_restricted_metric`: only the samples named, families left empty dropped
                    f.samples.retain(|s| names.contains(&s.name));
                    if !f.samples.is_empty() {
                        f.unit = String::new();
                        out.push(f);
                    }
                }
            }
        }
    }
    Ok(out)
}

/// `utils.floatToGoString`
pub fn go_float(d: f64) -> String {
    if d == f64::INFINITY {
        return "+Inf".into();
    }
    if d == f64::NEG_INFINITY {
        return "-Inf".into();
    }
    if d.is_nan() {
        return "NaN".into();
    }
    let s = ops::float_repr(d);
    match s.find('.') {
        Some(dot) if d > 0.0 && dot > 6 => {
            let mantissa = format!("{}.{}{}", &s[..1], &s[1..dot], &s[dot + 1..]);
            let mantissa = mantissa.trim_end_matches(['0', '.']);
            format!("{mantissa}e+{:02}", dot - 1)
        }
        _ => s,
    }
}

/// openmetrics escaping schemes of metric and label names
#[derive(Clone, Copy, PartialEq)]
enum Esc {
    AllowUtf8,
    Underscores,
    Dots,
    Values,
}

fn esc_of(v: Option<&V>) -> R<Esc> {
    Ok(match v.map(|v| v.as_str()) {
        None | Some(Some("underscores")) => Esc::Underscores,
        Some(Some("allow-utf-8")) => Esc::AllowUtf8,
        Some(Some("dots")) => Esc::Dots,
        Some(Some("values")) => Esc::Values,
        // unknown schemes: no escaping branch matches (`_escape` returns the name unchanged)
        Some(Some(_)) => return Err(Exc::type_error("py2axum: unknown escaping scheme")),
        Some(None) => return Err(Exc::type_error("py2axum: escaping= must be a str")),
    })
}

fn legacy_rune(c: char, i: usize, metric: bool) -> bool {
    c.is_ascii_alphabetic() || c == '_' || (c.is_ascii_digit() && i > 0) || (metric && c == ':')
}

/// `_is_valid_legacy_metric_name` / `_is_valid_legacy_labelname` (`$` also matches before a final newline)
fn legacy_valid(s: &str, metric: bool) -> bool {
    let core = s.strip_suffix('\n').unwrap_or(s);
    !core.is_empty() && core.chars().enumerate().all(|(i, c)| legacy_rune(c, i, metric))
}

/// `_escape(s, escaping, rune)`
fn escape_raw(s: &str, esc: Esc, metric: bool) -> String {
    match esc {
        Esc::AllowUtf8 => s.replace('\\', r"\\").replace('\n', r"\n").replace('"', "\\\""),
        Esc::Underscores => s.chars().enumerate().map(|(i, c)| if legacy_rune(c, i, metric) { c } else { '_' }).collect(),
        Esc::Dots => {
            let mut out = String::new();
            for (i, c) in s.chars().enumerate() {
                match c {
                    '_' => out += "__",
                    '.' => out += "_dot_",
                    c if legacy_rune(c, i, metric) => out.push(c),
                    _ => out += "__",
                }
            }
            out
        }
        Esc::Values => {
            let mut out = String::from("U__");
            for (i, c) in s.chars().enumerate() {
                match c {
                    '_' => out += "__",
                    c if legacy_rune(c, i, metric) => out.push(c),
                    c => out += &format!("_{:x}_", c as u32),
                }
            }
            out
        }
    }
}

/// `escape_metric_name` / `escape_label_name`
fn escape_name(s: &str, esc: Esc, metric: bool) -> String {
    if s.is_empty() {
        return String::new();
    }
    let valid = legacy_valid(s, metric);
    match esc {
        Esc::AllowUtf8 if !valid => format!("\"{}\"", escape_raw(s, esc, metric)),
        Esc::AllowUtf8 | Esc::Dots => escape_raw(s, esc, metric),
        Esc::Underscores | Esc::Values if valid => s.to_string(),
        _ => escape_raw(s, esc, metric),
    }
}

fn escape_value(v: &str) -> String {
    escape_raw(v, Esc::AllowUtf8, false)
}

fn label_str(labels: &[(String, String)], esc: Esc) -> String {
    let mut labels = labels.to_vec();
    labels.sort_by(|a, b| a.0.cmp(&b.0));
    labels.iter().map(|(k, v)| format!("{}=\"{}\"", escape_name(k, esc, false), escape_value(v))).collect::<Vec<_>>().join(",")
}

/// the Prometheus text format (`exposition.generate_latest`)
fn text_format(fams: &[Family], esc: Esc) -> String {
    let sample_line = |s: &Sample| -> String {
        let lab = label_str(&s.labels, esc);
        if esc != Esc::AllowUtf8 || legacy_valid(&s.name, true) {
            let lab = if lab.is_empty() { lab } else { format!("{{{lab}}}") };
            format!("{}{lab} {}\n", escape_name(&s.name, esc, true), go_float(s.value))
        } else {
            let comma = if lab.is_empty() { "" } else { "," };
            format!("{{{}{comma}{lab}}} {}\n", escape_name(&s.name, esc, true), go_float(s.value))
        }
    };
    let mut out = String::new();
    for fam in fams {
        let (mut mname, mut mtype) = (fam.name.clone(), fam.typ);
        match fam.typ {
            "counter" => mname.push_str("_total"),
            "info" => {
                mname.push_str("_info");
                mtype = "gauge";
            }
            "stateset" => mtype = "gauge",
            _ => {}
        }
        let doc = fam.doc.replace('\\', r"\\").replace('\n', r"\n");
        let n = escape_name(&mname, esc, true);
        out += &format!("# HELP {n} {doc}\n# TYPE {n} {mtype}\n");
        let mut om: Vec<(&str, Vec<String>)> = Vec::new();
        for s in &fam.samples {
            match ["_created", "_gsum", "_gcount"].into_iter().find(|suf| s.name == format!("{}{suf}", fam.name)) {
                Some(suf) => match om.iter_mut().find(|(k, _)| *k == suf) {
                    Some((_, l)) => l.push(sample_line(s)),
                    None => om.push((suf, vec![sample_line(s)])),
                },
                None => out += &sample_line(s),
            }
        }
        om.sort_by(|a, b| a.0.cmp(b.0));
        for (suf, lines) in om {
            let n = escape_name(&format!("{}{suf}", fam.name), esc, true);
            out += &format!("# HELP {n} {doc}\n# TYPE {n} gauge\n");
            for l in lines {
                out += &l;
            }
        }
    }
    out
}

/// OpenMetrics 1.0 (`openmetrics.exposition.generate_latest`)
fn openmetrics_format(fams: &[Family], esc: Esc) -> R<String> {
    let mut out = String::new();
    for fam in fams {
        let n = escape_name(&fam.name, esc, true);
        out += &format!("# HELP {n} {}\n# TYPE {n} {}\n", escape_value(&fam.doc), fam.typ);
        if !fam.unit.is_empty() {
            out += &format!("# UNIT {n} {}\n", fam.unit);
        }
        for s in &fam.samples {
            let mut lab = if esc == Esc::AllowUtf8 && !legacy_valid(&s.name, true) {
                format!("{}{}", escape_name(&s.name, esc, true), if s.labels.is_empty() { "" } else { "," })
            } else {
                String::new()
            };
            lab += &label_str(&s.labels, esc);
            if !lab.is_empty() {
                lab = format!("{{{lab}}}");
            }
            let ex = match &s.exemplar {
                Some(e) => {
                    let valid = (fam.typ == "counter" && s.name.ends_with("_total")) || (matches!(fam.typ, "histogram" | "gaugehistogram") && s.name.ends_with("_bucket"));
                    if !valid {
                        return Err(Exc::value_error(format!("Metric {} has exemplars, but is not a histogram bucket or counter", fam.name)));
                    }
                    let mut l = e.labels.clone();
                    l.sort_by(|a, b| a.0.cmp(&b.0));
                    let ls = l.iter().map(|(k, v)| format!("{k}=\"{}\"", escape_value(v))).collect::<Vec<_>>().join(",");
                    format!(" # {{{ls}}} {} {}", go_float(e.value), ops::float_repr(e.ts))
                }
                None => String::new(),
            };
            if esc != Esc::AllowUtf8 || legacy_valid(&s.name, true) {
                out += &format!("{}{lab} {}{ex}\n", escape_raw(&s.name, esc, false), go_float(s.value));
            } else {
                out += &format!("{lab} {}{ex}\n", go_float(s.value));
            }
        }
    }
    out += "# EOF\n";
    Ok(out)
}

fn source_of(v: Option<&V>) -> R<Source> {
    Ok(match v {
        None => Source::Registry(default_registry().clone()),
        Some(v) => match v {
            V::Native(n) => match &**n {
                Native::Prom(p) => match &**p {
                    Prom::Registry(r) => Source::Registry(r.clone()),
                    Prom::Restricted(r, names) => Source::Restricted(r.clone(), names.clone()),
                    Prom::Metric(m) => Source::Metric(m.clone()),
                    Prom::MultiProc(path) => Source::MultiProc(path.clone()),
                    _ => return Err(Exc::type_error(format!("py2axum: collecting a {}", type_name(p)))),
                },
                _ => return Err(Exc::type_error(format!("py2axum: collecting a {}", v.type_name()))),
            },
            o => return Err(Exc::attr_error(format!("'{}' object has no attribute 'collect'", o.type_name()))),
        },
    })
}

/// `generate_latest(registry=REGISTRY, escaping='underscores')`
pub async fn generate_latest(cx: &Cx, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let a = bind("generate_latest", &["registry", "escaping"], 0, args, kwargs)?;
    let src = source_of(a[0].as_ref())?;
    let out = text_format(&collect(cx, &src).await?, esc_of(a[1].as_ref())?);
    Ok(V::Bytes(Arc::from(out.into_bytes())))
}

/// `openmetrics.exposition.generate_latest(registry, escaping='underscores', version='1.0.0')`
pub async fn generate_openmetrics(cx: &Cx, args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let a = bind("generate_latest", &["registry", "escaping", "version"], 1, args, kwargs)?;
    let src = source_of(a[0].as_ref())?;
    let out = openmetrics_format(&collect(cx, &src).await?, esc_of(a[1].as_ref())?)?;
    Ok(V::Bytes(Arc::from(out.into_bytes())))
}

// ---------------------------------------------------------------- start_http_server

/// `utils.parse_version` compared with (1, 0, 0)
fn version_at_least_1(v: &str) -> bool {
    let parts: Vec<Result<i64, &str>> = v.split('.').map(|p| p.parse::<i64>().map_err(|_| p)).collect();
    // a tuple mixing ints and strs: comparing a str with an int raises TypeError in CPython
    let want = [1i64, 0, 0];
    for (i, w) in want.iter().enumerate() {
        match parts.get(i) {
            None => return false,
            Some(Ok(x)) if x != w => return x > w,
            Some(Ok(_)) => continue,
            Some(Err(_)) => return false,
        }
    }
    true
}

/// `choose_encoder(accept_header)`: (openmetrics?, escaping, content type)
fn choose_encoder(accept: &str) -> (bool, Esc, String) {
    let tok = |toks: &[&str], key: &str| -> Option<String> {
        toks.iter().filter(|t| t.contains('=')).find_map(|t| {
            let (k, v) = t.trim().split_once('=').unwrap();
            (k == key).then(|| v.to_string())
        })
    };
    for accepted in accept.split(',') {
        let toks: Vec<&str> = accepted.split(';').collect();
        let mime = toks[0].trim();
        if mime != "application/openmetrics-text" && mime != "text/plain" {
            continue;
        }
        let version = tok(&toks, "version").unwrap_or_default();
        let esc_name = match tok(&toks, "escaping").as_deref() {
            Some(e @ ("allow-utf-8" | "underscores" | "dots" | "values")) => e.to_string(),
            _ => "underscores".to_string(),
        };
        let esc = esc_of(Some(&V::str(&esc_name))).unwrap_or(Esc::Underscores);
        if mime == "application/openmetrics-text" {
            if version.is_empty() {
                return (true, Esc::Underscores, "application/openmetrics-text; version=1.0.0; charset=utf-8".into());
            }
            if version_at_least_1(&version) {
                return (true, esc, format!("application/openmetrics-text; version={version}; charset=utf-8; escaping={esc_name}"));
            }
        } else if !version.is_empty() && version_at_least_1(&version) {
            return (false, esc, format!("text/plain; version=1.0.0; charset=utf-8; escaping={esc_name}"));
        }
    }
    (false, Esc::Underscores, "text/plain; version=0.0.4; charset=utf-8".into())
}

fn gzip_accepted(h: &str) -> bool {
    h.split(',').any(|a| a.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("gzip"))
}

/// one request to the exporter (`make_wsgi_app`)
async fn exporter(src: Arc<Source>, req: axum::extract::Request) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;
    let (parts, _) = req.into_parts();
    let method = parts.method.as_str().to_string();
    let get = |h: header::HeaderName| parts.headers.get(h).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let (accept, accept_enc) = (get(header::ACCEPT), get(header::ACCEPT_ENCODING));
    let (path, query) = (parts.uri.path().to_string(), parts.uri.query().unwrap_or("").to_string());
    drop(parts);
    if method == "OPTIONS" {
        return (StatusCode::OK, [(header::ALLOW, "OPTIONS,GET")]).into_response();
    }
    if method != "GET" {
        return (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "OPTIONS,GET")], format!("# HTTP 405 Method Not Allowed: {method}; use OPTIONS or GET\n")).into_response();
    }
    if path == "/favicon.ico" {
        return StatusCode::OK.into_response();
    }
    let names: Vec<String> = form_urlencoded::parse(query.as_bytes()).filter(|(k, v)| k == "name[]" && !v.is_empty()).map(|(_, v)| v.into_owned()).collect();
    let src = match (&*src, names.is_empty()) {
        (Source::Registry(r), false) => Arc::new(Source::Restricted(r.clone(), Arc::new(names))),
        _ => src,
    };
    let (om, esc, ctype) = choose_encoder(&accept);
    let cx = super::root_cx();
    let body = match collect(&cx, &src).await.and_then(|f| if om { openmetrics_format(&f, esc) } else { Ok(text_format(&f, esc)) }) {
        Ok(b) => b.into_bytes(),
        Err(e) => {
            eprintln!("ERROR:py2axum:prometheus_client exporter: {e:?}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if gzip_accepted(&accept_enc) {
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(9));
        let _ = gz.write_all(&body);
        let body = gz.finish().unwrap_or_default();
        return (StatusCode::OK, [(header::CONTENT_TYPE, ctype), (header::CONTENT_ENCODING, "gzip".to_string())], body).into_response();
    }
    (StatusCode::OK, [(header::CONTENT_TYPE, ctype)], body).into_response()
}

/// `start_http_server(port, addr='0.0.0.0', registry=REGISTRY)`: the exporter on its own port, for the
/// life of the process; returns (server, thread) like the library
pub fn start_http_server(args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let params = ["port", "addr", "registry", "certfile", "keyfile", "client_cafile", "client_capath", "protocol", "client_auth_required", "tls_min_version", "tls_max_version"];
    let a = bind("start_wsgi_server", &params, 1, args, kwargs)?;
    if a[3..].iter().any(|v| v.as_ref().is_some_and(|v| ops::truthy(v).unwrap_or(true))) {
        return Err(Exc::type_error("py2axum: start_http_server() with TLS options is not supported"));
    }
    let port = match a[0].as_ref().unwrap() {
        V::Int(p) if (0..=65535).contains(p) => *p as u16,
        V::Int(_) => return Err(Exc::msg(&OVERFLOW_ERROR, "bind(): port must be 0-65535.")),
        o => return Err(Exc::type_error(format!("'{}' object cannot be interpreted as an integer", o.type_name()))),
    };
    let addr = match &a[1] {
        Some(V::Str(s)) => s.to_string(),
        _ => "0.0.0.0".into(),
    };
    let src = Arc::new(source_of(a[2].as_ref())?);
    let listener = std::net::TcpListener::bind((addr.as_str(), port)).map_err(|e| Exc::msg(&OS_ERROR, format!("[Errno {}] {}", e.raw_os_error().unwrap_or(0), e)))?;
    listener.set_nonblocking(true).map_err(|e| Exc::msg(&OS_ERROR, e.to_string()))?;
    tokio::spawn(async move {
        let Ok(listener) = tokio::net::TcpListener::from_std(listener) else { return };
        let app = axum::Router::new().fallback(move |req: axum::extract::Request| exporter(src.clone(), req));
        let _ = axum::serve(listener, app).await;
    });
    Ok(V::tuple(vec![V::native(Native::Namespace("WSGIServer")), V::native(Native::Namespace("Thread"))]))
}

// ---------------------------------------------------------------- multiprocess mode

/// `mmap_key`: `json.dumps([metric_name, name, labels, help_text], sort_keys=True)`
fn mmap_key(metric: &str, name: &str, labels: &[(String, String)], doc: &str) -> String {
    let mut l: Vec<&(String, String)> = labels.iter().collect();
    l.sort_by(|a, b| a.0.cmp(&b.0));
    let obj = l.iter().map(|(k, v)| format!("{}: {}", json_str(k), json_str(v))).collect::<Vec<_>>().join(", ");
    format!("[{}, {}, {{{obj}}}, {}]", json_str(metric), json_str(name), json_str(doc))
}

/// a str as CPython's json module writes it (ensure_ascii)
fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out += "\\\"",
            '\\' => out += "\\\\",
            '\n' => out += "\\n",
            '\r' => out += "\\r",
            '\t' => out += "\\t",
            '\u{8}' => out += "\\b",
            '\u{c}' => out += "\\f",
            c if (c as u32) < 0x20 || (c as u32) > 0x7f => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out += &format!("\\u{:04x}", u);
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One `<prefix>_<pid>.db` file, mapped like `MmapedDict`.
struct MFile {
    file: std::fs::File,
    ptr: *mut u8,
    cap: usize,
    used: usize,
    positions: HashMap<String, usize>,
}

// the mapping is only touched under `Mp::files`' lock
unsafe impl Send for MFile {}

const INITIAL_MMAP_SIZE: usize = 1 << 16;

impl MFile {
    fn open(path: &str) -> std::io::Result<MFile> {
        use std::os::unix::io::AsRawFd;
        let file = std::fs::OpenOptions::new().read(true).append(true).create(true).open(path)?;
        let mut cap = file.metadata()?.len() as usize;
        if cap == 0 {
            file.set_len(INITIAL_MMAP_SIZE as u64)?;
            cap = INITIAL_MMAP_SIZE;
        }
        // the header and every offset come from a file other processes write: checked before any access
        // through the mapping (`unsafe` below relies on `8 <= used <= cap`)
        let corrupted = || std::io::Error::other("Read beyond file size detected, file is corrupted.");
        if cap < 8 {
            return Err(corrupted());
        }
        let ptr = map(file.as_raw_fd(), cap)?;
        let mut f = MFile { file, ptr, cap, used: 0, positions: HashMap::new() };
        let used = f.read_i32(0);
        if used < 0 || used as usize > cap || (used != 0 && used < 8) {
            unsafe { libc::munmap(f.ptr as *mut libc::c_void, f.cap) };
            return Err(corrupted());
        }
        f.used = used as usize;
        if f.used == 0 {
            f.used = 8;
            f.write_i32(0, 8);
        } else {
            let data = unsafe { std::slice::from_raw_parts(f.ptr, f.used) }.to_vec();
            for (key, _, _, pos) in read_values(&data, f.used).map_err(|e| std::io::Error::other(e.message()))? {
                f.positions.insert(key, pos);
            }
        }
        Ok(f)
    }
    fn read_i32(&self, pos: usize) -> i32 {
        let mut b = [0u8; 4];
        unsafe { std::ptr::copy_nonoverlapping(self.ptr.add(pos), b.as_mut_ptr(), 4) };
        i32::from_ne_bytes(b)
    }
    fn write_i32(&mut self, pos: usize, v: i32) {
        unsafe { std::ptr::copy_nonoverlapping(v.to_ne_bytes().as_ptr(), self.ptr.add(pos), 4) };
    }
    fn write_pair(&mut self, pos: usize, value: f64, ts: f64) {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&value.to_ne_bytes());
        b[8..].copy_from_slice(&ts.to_ne_bytes());
        unsafe { std::ptr::copy_nonoverlapping(b.as_ptr(), self.ptr.add(pos), 16) };
    }
    fn read_f64(&self, pos: usize) -> f64 {
        let mut b = [0u8; 8];
        unsafe { std::ptr::copy_nonoverlapping(self.ptr.add(pos), b.as_mut_ptr(), 8) };
        f64::from_ne_bytes(b)
    }
    /// `_init_value`: the key, padded to 8 bytes, then two doubles
    fn init_value(&mut self, key: &str) -> std::io::Result<usize> {
        use std::os::unix::io::AsRawFd;
        let enc = key.as_bytes();
        let pad = 8 - (enc.len() + 4) % 8;
        let mut entry = Vec::with_capacity(4 + enc.len() + pad + 16);
        entry.extend((enc.len() as i32).to_ne_bytes());
        entry.extend(enc);
        entry.extend(std::iter::repeat_n(b' ', pad));
        entry.extend([0u8; 16]);
        while self.used + entry.len() > self.cap {
            self.cap *= 2;
            self.file.set_len(self.cap as u64)?;
            unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.cap / 2) };
            self.ptr = map(self.file.as_raw_fd(), self.cap)?;
        }
        unsafe { std::ptr::copy_nonoverlapping(entry.as_ptr(), self.ptr.add(self.used), entry.len()) };
        self.used += entry.len();
        let used = self.used as i32;
        self.write_i32(0, used);
        let pos = self.used - 16;
        self.positions.insert(key.to_string(), pos);
        Ok(pos)
    }
}

fn map(fd: i32, len: usize) -> std::io::Result<*mut u8> {
    let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
    if p == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error());
    }
    Ok(p as *mut u8)
}

/// `_read_all_values`: (key, value, timestamp, position)
fn read_values(data: &[u8], used: usize) -> R<Vec<(String, f64, f64, usize)>> {
    // bounds checked against both `used` (as prometheus_client) and the bytes really read: a corrupted
    // or concurrently written file is an error, never a panic
    let used = used.min(data.len());
    let corrupted = || Exc::runtime("Read beyond file size detected, file is corrupted.");
    let at = |p: usize, n: usize| -> R<&[u8]> { p.checked_add(n).filter(|e| *e <= used).map(|e| &data[p..e]).ok_or_else(corrupted) };
    let mut out = Vec::new();
    let mut pos = 8;
    while pos < used {
        let len = i32::from_ne_bytes(at(pos, 4)?.try_into().unwrap());
        let len = usize::try_from(len).map_err(|_| corrupted())?;
        if len.checked_add(pos).is_none_or(|e| e > used) {
            return Err(corrupted());
        }
        pos += 4;
        let key = String::from_utf8_lossy(at(pos, len)?).into_owned();
        pos += len + (8 - (len + 4) % 8);
        let value = f64::from_ne_bytes(at(pos, 8)?.try_into().unwrap());
        let ts = f64::from_ne_bytes(at(pos + 8, 8)?.try_into().unwrap());
        out.push((key, value, ts, pos));
        pos += 16;
    }
    Ok(out)
}

struct Mp {
    dir: String,
    pid: u32,
    files: Mutex<HashMap<String, MFile>>,
}

impl Mp {
    /// the slot of `key` in `<prefix>_<pid>.db`, created at 0.0 if absent, and its current value
    fn slot(&self, prefix: &str, key: &str) -> std::io::Result<(usize, f64)> {
        let mut files = self.files.lock();
        if !files.contains_key(prefix) {
            let f = MFile::open(&format!("{}/{prefix}_{}.db", self.dir, self.pid))?;
            files.insert(prefix.to_string(), f);
        }
        let f = files.get_mut(prefix).unwrap();
        let pos = match f.positions.get(key) {
            Some(p) => *p,
            None => f.init_value(key)?,
        };
        Ok((pos, f.read_f64(pos)))
    }
    fn write(&self, prefix: &str, pos: usize, value: f64, ts: f64) {
        if let Some(f) = self.files.lock().get_mut(prefix) {
            f.write_pair(pos, value, ts);
        }
    }
}

/// the multiprocess directory, read once (prometheus_client chooses its value class at import)
fn multiproc() -> Option<&'static Mp> {
    static MP: OnceLock<Option<Mp>> = OnceLock::new();
    MP.get_or_init(|| {
        let dir = std::env::var("PROMETHEUS_MULTIPROC_DIR").or_else(|_| std::env::var("prometheus_multiproc_dir")).ok()?;
        Some(Mp { dir, pid: std::process::id(), files: Mutex::new(HashMap::new()) })
    })
    .as_ref()
}

fn mp_dir(path: Option<&V>) -> Option<String> {
    match path {
        Some(V::Str(s)) => Some(s.to_string()),
        _ => std::env::var("PROMETHEUS_MULTIPROC_DIR").or_else(|_| std::env::var("prometheus_multiproc_dir")).ok(),
    }
}

/// `multiprocess.MultiProcessCollector(registry, path=None)`
pub fn multiproc_new(args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let a = bind("MultiProcessCollector.__init__", &["registry", "path"], 1, args, kwargs)?;
    let path = mp_dir(a[1].as_ref().filter(|p| !p.is_none()));
    let Some(path) = path.filter(|p| !p.is_empty() && std::path::Path::new(p).is_dir()) else {
        return Err(Exc::value_error("env PROMETHEUS_MULTIPROC_DIR is not set or not a directory"));
    };
    let c = Arc::new(path);
    match &a[0] {
        Some(V::Native(n)) => match &**n {
            Native::Prom(p) => match &**p {
                Prom::Registry(r) => r.register(Coll::MultiProc(c.clone()))?,
                _ => return Err(Exc::type_error("py2axum: MultiProcessCollector(registry) expects a CollectorRegistry")),
            },
            _ => return Err(Exc::type_error("py2axum: MultiProcessCollector(registry) expects a CollectorRegistry")),
        },
        Some(v) if !ops::truthy(v)? => {}
        _ => return Err(Exc::type_error("py2axum: MultiProcessCollector(registry) expects a CollectorRegistry")),
    }
    Ok(prom(Prom::MultiProc(c)))
}

/// `multiprocess.mark_process_dead(pid, path=None)`: the `live*` gauges of a dead process are dropped
pub fn mark_process_dead(args: Vec<V>, kwargs: Vec<(String, V)>) -> R {
    let a = bind("mark_process_dead", &["pid", "path"], 1, args, kwargs)?;
    let pid = ops::str_(a[0].as_ref().unwrap())?;
    let Some(dir) = mp_dir(a[1].as_ref().filter(|p| !p.is_none())) else {
        return Err(Exc::type_error("expected str, bytes or os.PathLike object, not NoneType"));
    };
    for mode in ["liveall", "livemin", "livemax", "livesum", "livemostrecent"] {
        let _ = std::fs::remove_file(format!("{dir}/gauge_{mode}_{pid}.db"));
    }
    Ok(V::None)
}

/// `MultiProcessCollector.collect()`: every `*.db` file of the directory, in listing order (as `glob`)
fn mp_collect(path: &str) -> R<Vec<Family>> {
    let io = |e: std::io::Error| Exc::msg(&OS_ERROR, e.to_string());
    let mut files = Vec::new();
    for e in std::fs::read_dir(path).map_err(io)? {
        let e = e.map_err(io)?;
        let name = e.file_name().to_string_lossy().into_owned();
        if name.ends_with(".db") && !name.starts_with('.') {
            files.push(name);
        }
    }
    // metric name -> (help, type, mode, samples)
    struct Raw {
        doc: String,
        typ: &'static str,
        mode: String,
        samples: Vec<(String, Vec<(String, String)>, f64, f64)>,
    }
    let mut metrics: IndexMap<String, Raw> = IndexMap::new();
    for fname in files {
        let parts: Vec<&str> = fname.split('_').collect();
        let typ: &'static str = match parts[0] {
            "counter" => "counter",
            "gauge" => "gauge",
            "summary" => "summary",
            "histogram" => "histogram",
            other => return Err(Exc::value_error(format!("Invalid metric type: {other}"))),
        };
        let data = match std::fs::read(format!("{path}/{fname}")) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && typ == "gauge" && parts.get(1).is_some_and(|p| p.starts_with("live")) => continue,
            Err(e) => return Err(io(e)),
        };
        if data.len() < 8 {
            continue;
        }
        let used = i32::from_ne_bytes(data[..4].try_into().unwrap()) as usize;
        for (key, value, ts, _) in read_values(&data[..used.min(data.len())], used)? {
            let parsed: serde_json::Value = serde_json::from_str(&key).map_err(|e| Exc::value_error(e.to_string()))?;
            let (metric, name, labels, doc) = match &parsed {
                serde_json::Value::Array(v) if v.len() == 4 => (
                    v[0].as_str().unwrap_or_default().to_string(),
                    v[1].as_str().unwrap_or_default().to_string(),
                    v[2].as_object().map(|o| o.iter().map(|(k, x)| (k.clone(), x.as_str().unwrap_or_default().to_string())).collect::<Vec<_>>()).unwrap_or_default(),
                    v[3].as_str().unwrap_or_default().to_string(),
                ),
                _ => return Err(Exc::value_error("py2axum: unexpected key in a multiprocess file")),
            };
            let mut labels = labels;
            labels.sort();
            let m = metrics.entry(metric).or_insert_with(|| Raw { doc, typ, mode: String::new(), samples: vec![] });
            if typ == "gauge" {
                let pid = parts.get(2).map(|p| p.trim_end_matches(".db").to_string()).unwrap_or_default();
                m.mode = parts.get(1).unwrap_or(&"").to_string();
                labels.push(("pid".into(), pid));
            }
            m.samples.push((name, labels, value, ts));
        }
    }
    let mut out = Vec::new();
    for (mname, m) in metrics {
        type Labels = Vec<(String, String)>;
        // labels -> (name, labels) -> value, in first-seen order
        let mut samples: IndexMap<Labels, IndexMap<(String, Labels), f64>> = IndexMap::new();
        let mut stamps: HashMap<(Labels, String), f64> = HashMap::new();
        let mut buckets: IndexMap<Labels, Vec<(f64, f64)>> = IndexMap::new();
        for (name, labels, value, ts) in m.samples {
            let mut labels = labels;
            let agg = matches!(m.mode.as_str(), "min" | "livemin" | "max" | "livemax" | "sum" | "livesum" | "mostrecent" | "livemostrecent");
            if m.typ == "gauge" && agg {
                labels.retain(|(k, _)| k != "pid");
            }
            if m.typ == "gauge" {
                let group = samples.entry(labels.clone()).or_default();
                let k = (name.clone(), labels.clone());
                match m.mode.as_str() {
                    "min" | "livemin" => {
                        let cur = *group.entry(k.clone()).or_insert(value);
                        if value < cur {
                            group.insert(k, value);
                        }
                    }
                    "max" | "livemax" => {
                        let cur = *group.entry(k.clone()).or_insert(value);
                        if value > cur {
                            group.insert(k, value);
                        }
                    }
                    "sum" | "livesum" => *group.entry(k).or_insert(0.0) += value,
                    "mostrecent" | "livemostrecent" => {
                        let cur = stamps.get(&(labels.clone(), name.clone())).copied().unwrap_or(0.0);
                        if cur < ts {
                            group.insert(k, value);
                            stamps.insert((labels.clone(), name.clone()), ts);
                        }
                    }
                    _ => {
                        group.insert(k, value);
                    }
                }
            } else if m.typ == "histogram" {
                match labels.iter().position(|(k, _)| k == "le") {
                    Some(i) => {
                        let le: f64 = match labels[i].1.as_str() {
                            "+Inf" => f64::INFINITY,
                            "-Inf" => f64::NEG_INFINITY,
                            s => s.parse().unwrap_or(f64::NAN),
                        };
                        let mut without = labels.clone();
                        without.remove(i);
                        let b = buckets.entry(without).or_default();
                        match b.iter_mut().find(|(x, _)| *x == le) {
                            Some(slot) => slot.1 += value,
                            None => b.push((le, value)),
                        }
                    }
                    None => *samples.entry(labels.clone()).or_default().entry((name, labels)).or_insert(0.0) += value,
                }
            } else {
                *samples.entry(labels.clone()).or_default().entry((name, labels)).or_insert(0.0) += value;
            }
        }
        if m.typ == "histogram" {
            for (labels, mut values) in buckets {
                values.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
                let mut acc = 0.0;
                let group = samples.entry(labels.clone()).or_default();
                for (le, v) in values {
                    acc += v;
                    let mut l = labels.clone();
                    l.push(("le".into(), go_float(le)));
                    group.insert((format!("{mname}_bucket"), l), acc);
                }
                group.insert((format!("{mname}_count"), labels), acc);
            }
        }
        let samples = samples.into_values().flat_map(|g| g.into_iter().map(|((name, labels), value)| Sample { name, labels, value, exemplar: None })).collect();
        out.push(Family { name: mname, doc: m.doc, typ: m.typ, unit: String::new(), samples });
    }
    Ok(out)
}
