//! Runtime support embedded in every generated project.
//! Mirrors the FastAPI/Starlette/Pydantic behaviour the generated handlers rely on:
//! error envelopes, Pydantic-style lax parsing and constraint messages, and a small
//! SQL builder for SQLAlchemy `select()` chains that are composed at runtime.
#![allow(dead_code)]

use axum::{
    body::Body,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{postgres::PgRow, FromRow, Postgres, QueryBuilder};

// ---------------------------------------------------------------- errors

#[derive(Debug, Serialize)]
pub struct ErrDetail {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub loc: Vec<Value>,
    pub msg: String,
    pub input: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ctx: Option<Value>,
}

#[derive(Debug)]
pub enum AppError {
    /// `raise HTTPException(status_code, detail)`
    Http(u16, String),
    /// Request validation failure (FastAPI answers 422).
    Validation(Vec<ErrDetail>),
    /// Unhandled exception (FastAPI answers a plain-text 500).
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::Http(code, detail) => (
                StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(json!({ "detail": detail })),
            )
                .into_response(),
            AppError::Validation(errs) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "detail": errs })),
            )
                .into_response(),
            AppError::Internal(msg) => {
                eprintln!("ERROR: {msg}");
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
                    .body(Body::from("Internal Server Error"))
                    .unwrap()
            }
        }
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        AppError::Internal(format!("database: {e}"))
    }
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        AppError::Internal(format!("http client: {e}"))
    }
}

fn unquote(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (
                (b[i + 1] as char).to_digit(16),
                (b[i + 2] as char).to_digit(16),
            ) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn quote_url(s: &str) -> String {
    let mut out = String::new();
    for &c in s.as_bytes() {
        if c.is_ascii_alphanumeric() || b"_.-~:/%#?=@[]!$&'()*+,;".contains(&c) {
            out.push(c as char);
        } else {
            out += &format!("%{c:02X}");
        }
    }
    out
}

/// Starlette's Router over (method, path regex, one-route axum router), in declaration order: first
/// full match, else 405 with the first path match's method, else 307 to the slash-toggled path, else 404.
pub async fn dispatch(
    table: std::sync::Arc<Vec<(&'static str, regex::Regex, axum::Router)>>,
    req: axum::extract::Request,
) -> Response {
    use tower::ServiceExt;
    let path = unquote(req.uri().path());
    let method = req.method().as_str().to_string();
    let mut partial: Option<&'static str> = None;
    for (m, re, router) in table.iter() {
        if re.is_match(&path) {
            if *m == method {
                return router
                    .clone()
                    .oneshot(req)
                    .await
                    .unwrap_or_else(|e| match e {});
            }
            partial.get_or_insert(m);
        }
    }
    if let Some(m) = partial {
        let mut resp = method_not_allowed().await;
        resp.headers_mut()
            .insert(axum::http::header::ALLOW, m.parse().unwrap());
        return resp;
    }
    if path != "/" {
        let alt = match path.strip_suffix('/') {
            Some(p) => p.to_string(),
            None => format!("{path}/"),
        };
        if table.iter().any(|(_, re, _)| re.is_match(&alt)) {
            let host = req
                .headers()
                .get(axum::http::header::HOST)
                .and_then(|h| h.to_str().ok())
                .unwrap_or("")
                .to_string();
            let mut url = format!("http://{host}{alt}");
            if let Some(q) = req.uri().query() {
                url += "?";
                url += q;
            }
            return Response::builder()
                .status(307)
                .header(axum::http::header::LOCATION, quote_url(&url))
                .header(axum::http::header::CONTENT_LENGTH, "0")
                .body(axum::body::Body::empty())
                .unwrap();
        }
    }
    not_found().await
}

pub async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "detail": "Not Found" })),
    )
        .into_response()
}

pub async fn method_not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(json!({ "detail": "Method Not Allowed" })),
    )
        .into_response()
}

pub fn json_response<T: Serialize>(status: u16, body: &T) -> Response {
    match serde_json::to_vec(body) {
        Ok(bytes) => Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(bytes))
            .unwrap(),
        Err(e) => AppError::Internal(format!("serialize: {e}")).into_response(),
    }
}

pub fn empty_response(status: u16) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::empty())
        .unwrap()
}

// ---------------------------------------------------------------- locations

pub fn loc(prefix: &[&str], field: &str) -> Vec<Value> {
    let mut v: Vec<Value> = prefix.iter().map(|s| Value::from(*s)).collect();
    v.push(Value::from(field));
    v
}

fn err(
    errs: &mut Vec<ErrDetail>,
    kind: &'static str,
    loc: Vec<Value>,
    msg: impl Into<String>,
    input: &Value,
    ctx: Option<Value>,
) {
    errs.push(ErrDetail {
        kind,
        loc,
        msg: msg.into(),
        input: input.clone(),
        ctx,
    });
}

pub fn missing(errs: &mut Vec<ErrDetail>, loc: Vec<Value>, input: &Value) {
    err(errs, "missing", loc, "Field required", input, None);
}

// ---------------------------------------------------------------- lax parsing (Pydantic v2)

pub fn v_str(v: &Value, loc: Vec<Value>, errs: &mut Vec<ErrDetail>) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        _ => {
            err(
                errs,
                "string_type",
                loc,
                "Input should be a valid string",
                v,
                None,
            );
            None
        }
    }
}

pub fn v_int(v: &Value, loc: Vec<Value>, errs: &mut Vec<ErrDetail>) -> Option<i64> {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(i)
            } else if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 && f.abs() < 9.2e18 {
                    Some(f as i64)
                } else {
                    err(
                        errs,
                        "int_from_float",
                        loc,
                        "Input should be a valid integer, got a number with a fractional part",
                        v,
                        None,
                    );
                    None
                }
            } else {
                err(
                    errs,
                    "int_parsing_size",
                    loc,
                    "Unable to parse input string as an integer, exceeded maximum size",
                    v,
                    None,
                );
                None
            }
        }
        Value::String(s) => match s.trim().replace('_', "").parse::<i64>() {
            Ok(i) => Some(i),
            Err(_) => {
                err(
                    errs,
                    "int_parsing",
                    loc,
                    "Input should be a valid integer, unable to parse string as an integer",
                    v,
                    None,
                );
                None
            }
        },
        _ => {
            err(
                errs,
                "int_type",
                loc,
                "Input should be a valid integer",
                v,
                None,
            );
            None
        }
    }
}

pub fn v_float(v: &Value, loc: Vec<Value>, errs: &mut Vec<ErrDetail>) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => match s.trim().parse::<f64>() {
            Ok(f) => Some(f),
            Err(_) => {
                err(
                    errs,
                    "float_parsing",
                    loc,
                    "Input should be a valid number, unable to parse string as a number",
                    v,
                    None,
                );
                None
            }
        },
        _ => {
            err(
                errs,
                "float_type",
                loc,
                "Input should be a valid number",
                v,
                None,
            );
            None
        }
    }
}

pub fn v_bool(v: &Value, loc: Vec<Value>, errs: &mut Vec<ErrDetail>) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) if n.as_i64() == Some(0) => Some(false),
        Value::Number(n) if n.as_i64() == Some(1) => Some(true),
        Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "0" | "off" | "f" | "false" | "n" | "no" => Some(false),
            "1" | "on" | "t" | "true" | "y" | "yes" => Some(true),
            _ => {
                err(
                    errs,
                    "bool_parsing",
                    loc,
                    "Input should be a valid boolean, unable to interpret input",
                    v,
                    None,
                );
                None
            }
        },
        Value::Number(_) => {
            err(
                errs,
                "bool_parsing",
                loc,
                "Input should be a valid boolean, unable to interpret input",
                v,
                None,
            );
            None
        }
        _ => {
            err(
                errs,
                "bool_type",
                loc,
                "Input should be a valid boolean",
                v,
                None,
            );
            None
        }
    }
}

// ---------------------------------------------------------------- constraints

pub fn c_str_len(
    s: String,
    min: Option<usize>,
    max: Option<usize>,
    loc: Vec<Value>,
    input: &Value,
    errs: &mut Vec<ErrDetail>,
) -> Option<String> {
    let n = s.chars().count();
    if let Some(m) = min {
        if n < m {
            let plural = if m == 1 { "" } else { "s" };
            err(
                errs,
                "string_too_short",
                loc,
                format!("String should have at least {m} character{plural}"),
                input,
                Some(json!({ "min_length": m })),
            );
            return None;
        }
    }
    if let Some(m) = max {
        if n > m {
            let plural = if m == 1 { "" } else { "s" };
            err(
                errs,
                "string_too_long",
                loc,
                format!("String should have at most {m} character{plural}"),
                input,
                Some(json!({ "max_length": m })),
            );
            return None;
        }
    }
    Some(s)
}

#[derive(Clone, Copy)]
pub enum NumCheck {
    Ge(f64),
    Gt(f64),
    Le(f64),
    Lt(f64),
}

fn fmt_num(x: f64) -> Value {
    if x.fract() == 0.0 {
        Value::from(x as i64)
    } else {
        Value::from(x)
    }
}

pub fn c_num<T: Copy + Into<f64> + Serialize>(
    v: T,
    checks: &[NumCheck],
    loc: Vec<Value>,
    input: &Value,
    errs: &mut Vec<ErrDetail>,
) -> Option<T> {
    let x: f64 = v.into();
    for c in checks {
        let (ok, kind, word, key, bound) = match *c {
            NumCheck::Ge(b) => (
                x >= b,
                "greater_than_equal",
                "greater than or equal to",
                "ge",
                b,
            ),
            NumCheck::Gt(b) => (x > b, "greater_than", "greater than", "gt", b),
            NumCheck::Le(b) => (x <= b, "less_than_equal", "less than or equal to", "le", b),
            NumCheck::Lt(b) => (x < b, "less_than", "less than", "lt", b),
        };
        if !ok {
            let b = fmt_num(bound);
            err(
                errs,
                kind,
                loc,
                format!("Input should be {word} {b}"),
                input,
                Some(json!({ key: b })),
            );
            return None;
        }
    }
    Some(v)
}

/// i64 does not implement Into<f64>; validate through a lossy view.
pub fn c_int(
    v: i64,
    checks: &[NumCheck],
    loc: Vec<Value>,
    input: &Value,
    errs: &mut Vec<ErrDetail>,
) -> Option<i64> {
    let x = v as f64;
    for c in checks {
        let (ok, kind, word, key, bound) = match *c {
            NumCheck::Ge(b) => (
                x >= b,
                "greater_than_equal",
                "greater than or equal to",
                "ge",
                b,
            ),
            NumCheck::Gt(b) => (x > b, "greater_than", "greater than", "gt", b),
            NumCheck::Le(b) => (x <= b, "less_than_equal", "less than or equal to", "le", b),
            NumCheck::Lt(b) => (x < b, "less_than", "less than", "lt", b),
        };
        if !ok {
            let b = fmt_num(bound);
            err(
                errs,
                kind,
                loc,
                format!("Input should be {word} {b}"),
                input,
                Some(json!({ key: b })),
            );
            return None;
        }
    }
    Some(v)
}

// ---------------------------------------------------------------- request inputs

/// Query string, last value wins like Starlette's QueryParams.
pub struct QueryMap(Vec<(String, String)>);

impl QueryMap {
    pub fn parse(raw: Option<&str>) -> Self {
        QueryMap(
            raw.map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect())
                .unwrap_or_default(),
        )
    }
    pub fn get(&self, key: &str) -> Option<Value> {
        self.0
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| Value::String(v.clone()))
    }
}

pub fn path_get(params: &axum::extract::RawPathParams, key: &str) -> Option<Value> {
    params
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| Value::String(v.to_string()))
}

/// Parse a JSON request body the way FastAPI does for a single body parameter.
pub fn parse_body(bytes: &[u8], errs: &mut Vec<ErrDetail>) -> Option<Value> {
    if bytes.is_empty() {
        missing(errs, vec![Value::from("body")], &Value::Null);
        return None;
    }
    match serde_json::from_slice::<Value>(bytes) {
        Ok(v) => Some(v),
        Err(e) => {
            // FastAPI reports the byte offset of the error.
            let offset = byte_offset(bytes, e.line(), e.column());
            err(
                errs,
                "json_invalid",
                vec![Value::from("body"), Value::from(offset)],
                "JSON decode error",
                &json!({}),
                Some(json!({ "error": e.to_string() })),
            );
            None
        }
    }
}

fn byte_offset(bytes: &[u8], line: usize, column: usize) -> usize {
    let mut cur_line = 1;
    for (i, b) in bytes.iter().enumerate() {
        if cur_line == line {
            return (i + column.saturating_sub(1)).min(bytes.len());
        }
        if *b == b'\n' {
            cur_line += 1;
        }
    }
    bytes.len()
}

pub fn expect_object<'a>(
    v: &'a Value,
    loc: Vec<Value>,
    errs: &mut Vec<ErrDetail>,
) -> Option<&'a serde_json::Map<String, Value>> {
    match v.as_object() {
        Some(o) => Some(o),
        None => {
            err(
                errs,
                "model_attributes_type",
                loc,
                "Input should be a valid dictionary or object to extract fields from",
                v,
                None,
            );
            None
        }
    }
}

// ---------------------------------------------------------------- SQL

/// A bind value. Nulls are typed so Postgres can infer the parameter type.
#[derive(Clone, Debug)]
pub enum Val {
    Bool(bool),
    I32(i32),
    I64(i64),
    F64(f64),
    Str(String),
    NullBool,
    NullI64,
    NullF64,
    NullStr,
}

impl From<bool> for Val {
    fn from(v: bool) -> Self {
        Val::Bool(v)
    }
}
impl From<i32> for Val {
    fn from(v: i32) -> Self {
        Val::I32(v)
    }
}
impl From<i64> for Val {
    fn from(v: i64) -> Self {
        Val::I64(v)
    }
}
impl From<f64> for Val {
    fn from(v: f64) -> Self {
        Val::F64(v)
    }
}
impl From<String> for Val {
    fn from(v: String) -> Self {
        Val::Str(v)
    }
}
impl From<&str> for Val {
    fn from(v: &str) -> Self {
        Val::Str(v.to_string())
    }
}
impl From<Option<bool>> for Val {
    fn from(v: Option<bool>) -> Self {
        v.map(Val::Bool).unwrap_or(Val::NullBool)
    }
}
impl From<Option<i32>> for Val {
    fn from(v: Option<i32>) -> Self {
        v.map(Val::I32).unwrap_or(Val::NullI64)
    }
}
impl From<Option<i64>> for Val {
    fn from(v: Option<i64>) -> Self {
        v.map(Val::I64).unwrap_or(Val::NullI64)
    }
}
impl From<Option<f64>> for Val {
    fn from(v: Option<f64>) -> Self {
        v.map(Val::F64).unwrap_or(Val::NullF64)
    }
}
impl From<Option<String>> for Val {
    fn from(v: Option<String>) -> Self {
        v.map(Val::Str).unwrap_or(Val::NullStr)
    }
}

fn push_val(qb: &mut QueryBuilder<'static, Postgres>, v: Val) {
    match v {
        Val::Bool(x) => {
            qb.push_bind(x);
        }
        Val::I32(x) => {
            qb.push_bind(x);
        }
        Val::I64(x) => {
            qb.push_bind(x);
        }
        Val::F64(x) => {
            qb.push_bind(x);
        }
        Val::Str(x) => {
            qb.push_bind(x);
        }
        Val::NullBool => {
            qb.push_bind(None::<bool>);
        }
        Val::NullI64 => {
            qb.push_bind(None::<i64>);
        }
        Val::NullF64 => {
            qb.push_bind(None::<f64>);
        }
        Val::NullStr => {
            qb.push_bind(None::<String>);
        }
    }
}

#[derive(Clone, Debug)]
pub enum Cond {
    Cmp(&'static str, &'static str, Val),
    IsNull(&'static str),
    NotNull(&'static str),
    And(Vec<Cond>),
    Or(Vec<Cond>),
}

fn push_cond(qb: &mut QueryBuilder<'static, Postgres>, c: Cond) {
    match c {
        Cond::Cmp(col, op, v) => {
            qb.push(col).push(" ").push(op).push(" ");
            push_val(qb, v);
        }
        Cond::IsNull(col) => {
            qb.push(col).push(" IS NULL");
        }
        Cond::NotNull(col) => {
            qb.push(col).push(" IS NOT NULL");
        }
        Cond::And(cs) => push_group(qb, cs, " AND "),
        Cond::Or(cs) => push_group(qb, cs, " OR "),
    }
}

fn push_group(qb: &mut QueryBuilder<'static, Postgres>, cs: Vec<Cond>, sep: &str) {
    qb.push("(");
    for (i, c) in cs.into_iter().enumerate() {
        if i > 0 {
            qb.push(sep);
        }
        push_cond(qb, c);
    }
    qb.push(")");
}

/// Runtime form of `select(Model).where(...).order_by(...).offset(...).limit(...)`.
#[derive(Clone, Debug)]
pub struct Select {
    table: &'static str,
    cols: &'static str,
    conds: Vec<Cond>,
    order: Vec<&'static str>,
    offset: Option<Val>,
    limit: Option<Val>,
}

impl Select {
    pub fn new(table: &'static str, cols: &'static str) -> Self {
        Select {
            table,
            cols,
            conds: Vec::new(),
            order: Vec::new(),
            offset: None,
            limit: None,
        }
    }
    pub fn and_where(&mut self, c: Cond) {
        self.conds.push(c);
    }
    pub fn order_by(&mut self, o: &'static str) {
        self.order.push(o);
    }
    pub fn offset(&mut self, v: impl Into<Val>) {
        self.offset = Some(v.into());
    }
    pub fn limit(&mut self, v: impl Into<Val>) {
        self.limit = Some(v.into());
    }

    /// True when the query has a LIMIT known to be at most `n` rows.
    pub fn limit_at_most(&self, n: i64) -> bool {
        match self.limit {
            Some(Val::I64(l)) => l <= n,
            Some(Val::I32(l)) => i64::from(l) <= n,
            _ => false,
        }
    }

    fn build(self) -> QueryBuilder<'static, Postgres> {
        let mut qb = QueryBuilder::new("SELECT ");
        qb.push(self.cols).push(" FROM ").push(self.table);
        for (i, c) in self.conds.into_iter().enumerate() {
            qb.push(if i == 0 { " WHERE " } else { " AND " });
            push_cond(&mut qb, c);
        }
        if !self.order.is_empty() {
            qb.push(" ORDER BY ").push(self.order.join(", "));
        }
        if let Some(v) = self.limit {
            qb.push(" LIMIT ");
            push_val(&mut qb, v);
        }
        if let Some(v) = self.offset {
            qb.push(" OFFSET ");
            push_val(&mut qb, v);
        }
        qb
    }

    pub async fn fetch_all<'e, T, E>(self, ex: E) -> Result<Vec<T>, sqlx::Error>
    where
        T: for<'r> FromRow<'r, PgRow> + Send + Unpin,
        E: sqlx::Executor<'e, Database = Postgres>,
    {
        let mut qb = self.build();
        qb.build_query_as::<T>().fetch_all(ex).await
    }

    pub async fn fetch_optional<'e, T, E>(self, ex: E) -> Result<Option<T>, sqlx::Error>
    where
        T: for<'r> FromRow<'r, PgRow> + Send + Unpin,
        E: sqlx::Executor<'e, Database = Postgres>,
    {
        let mut qb = self.build();
        qb.build_query_as::<T>().fetch_optional(ex).await
    }
}

/// Flush of the attributes changed on a loaded ORM object (SQLAlchemy UPDATE).
pub async fn update_returning<'e, T, E>(
    table: &'static str,
    pk_col: &'static str,
    pk: Val,
    sets: Vec<(&'static str, Val)>,
    cols: &'static str,
    ex: E,
) -> Result<T, sqlx::Error>
where
    T: for<'r> FromRow<'r, PgRow> + Send + Unpin,
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let mut qb: QueryBuilder<'static, Postgres> = QueryBuilder::new("UPDATE ");
    qb.push(table).push(" SET ");
    for (i, (col, v)) in sets.into_iter().enumerate() {
        if i > 0 {
            qb.push(", ");
        }
        qb.push(col).push(" = ");
        push_val(&mut qb, v);
    }
    qb.push(" WHERE ").push(pk_col).push(" = ");
    push_val(&mut qb, pk);
    qb.push(" RETURNING ").push(cols);
    qb.build_query_as::<T>().fetch_one(ex).await
}

// ---------------------------------------------------------------- streamed list responses

/// Blocks queued between the query task and the socket (back-pressure bound).
const STREAM_QUEUE: usize = 4;

/// (block size in bytes, LIMIT below which a query skips streaming).
/// Defaults 64 KiB / 1000 rows; PY2AXUM_STREAM_CHUNK and PY2AXUM_STREAM_MIN_ROWS override them
/// (the conformance suite sets them very low to force multi-block streaming on small lists).
fn stream_cfg() -> (usize, i64) {
    static CFG: std::sync::LazyLock<(usize, i64)> = std::sync::LazyLock::new(|| {
        let chunk = std::env::var("PY2AXUM_STREAM_CHUNK")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(64 * 1024);
        let min_rows = std::env::var("PY2AXUM_STREAM_MIN_ROWS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        (chunk, min_rows)
    });
    *CFG
}

enum StreamMsg {
    /// The whole body fitted in one block: answer normally, with Content-Length.
    Complete(axum::body::Bytes),
    /// First block of a body that will be streamed.
    Start(axum::body::Bytes),
    Chunk(axum::body::Bytes),
}

/// `return (await session.execute(q)).scalars().all()` with `response_model=list[S]`.
///
/// Rows are read from Postgres as a stream and serialised to JSON block by block, so memory
/// stays bounded (~STREAM_QUEUE x block size per request) whatever the result size.
/// The bytes produced are identical to the buffered `serde_json::to_vec(&Vec<S>)`.
///
/// Error semantics: an error before the first block gives a 500 like FastAPI; an error after
/// streaming started can only abort the connection (headers are already sent).
pub async fn stream_json_list<M, S>(
    pool: sqlx::PgPool,
    select: Select,
    status: u16,
) -> Result<Response, AppError>
where
    M: for<'r> FromRow<'r, PgRow> + Send + Unpin + 'static,
    S: From<M> + Serialize + Send + 'static,
{
    use axum::body::Bytes;
    use futures_util::{stream, StreamExt, TryStreamExt};

    let (chunk_size, min_rows) = stream_cfg();
    if select.limit_at_most(min_rows) {
        let rows: Vec<M> = select.fetch_all(&pool).await?;
        let out: Vec<S> = rows.into_iter().map(S::from).collect();
        return Ok(json_response(status, &out));
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<StreamMsg, String>>(STREAM_QUEUE);
    tokio::spawn(async move {
        let mut qb = select.build();
        let mut rows = qb.build_query_as::<M>().fetch(&pool);
        let mut buf: Vec<u8> = Vec::with_capacity(chunk_size + 4096);
        buf.push(b'[');
        let mut first = true;
        let mut started = false;
        loop {
            match rows.try_next().await {
                Ok(Some(row)) => {
                    if !first {
                        buf.push(b',');
                    }
                    first = false;
                    if let Err(e) = serde_json::to_writer(&mut buf, &S::from(row)) {
                        let _ = tx.send(Err(format!("serialize: {e}"))).await;
                        return;
                    }
                    if buf.len() >= chunk_size {
                        let block = Bytes::from(std::mem::replace(
                            &mut buf,
                            Vec::with_capacity(chunk_size + 4096),
                        ));
                        let msg = if started {
                            StreamMsg::Chunk(block)
                        } else {
                            started = true;
                            StreamMsg::Start(block)
                        };
                        if tx.send(Ok(msg)).await.is_err() {
                            return; // client went away: dropping `rows` cancels the query
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(Err(format!("database: {e}"))).await;
                    return;
                }
            }
        }
        buf.push(b']');
        let block = Bytes::from(buf);
        let msg = if started {
            StreamMsg::Chunk(block)
        } else {
            StreamMsg::Complete(block)
        };
        let _ = tx.send(Ok(msg)).await;
    });

    match rx.recv().await {
        Some(Ok(StreamMsg::Complete(body))) => Ok(Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap()),
        Some(Ok(StreamMsg::Start(block))) | Some(Ok(StreamMsg::Chunk(block))) => {
            let rest = stream::unfold(rx, |mut rx| async move {
                match rx.recv().await {
                    Some(Ok(
                        StreamMsg::Chunk(b) | StreamMsg::Start(b) | StreamMsg::Complete(b),
                    )) => Some((Ok::<Bytes, std::io::Error>(b), rx)),
                    Some(Err(e)) => {
                        eprintln!("ERROR: stream aborted: {e}");
                        Some((Err(std::io::Error::other(e)), rx))
                    }
                    None => None,
                }
            });
            // fuse(): body wrappers such as the gzip layer may poll again after the end.
            let body = stream::once(async move { Ok::<Bytes, std::io::Error>(block) })
                .chain(rest)
                .fuse();
            Ok(Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from_stream(body))
                .unwrap())
        }
        Some(Err(e)) => Err(AppError::Internal(e)),
        None => Err(AppError::Internal(
            "stream task ended without a result".into(),
        )),
    }
}

/// Starlette's GZipMiddleware writes `Vary: Accept-Encoding`; tower-http's compression `accept-encoding`
pub async fn starlette_vary(mut r: axum::response::Response) -> axum::response::Response {
    let vals: Vec<String> = r
        .headers()
        .get_all("vary")
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .collect();
    if vals.iter().any(|v| v.contains("accept-encoding")) {
        r.headers_mut().remove("vary");
        for v in vals {
            if let Ok(h) =
                axum::http::HeaderValue::from_str(&v.replace("accept-encoding", "Accept-Encoding"))
            {
                r.headers_mut().append("vary", h);
            }
        }
    }
    r
}

// ---------------------------------------------------------------- graceful shutdown (as uvicorn)

/// the signal that started the shutdown, raised again at the end (uvicorn's `capture_signals`)
static SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static STOPPING: tokio::sync::Notify = tokio::sync::Notify::const_new();

async fn next_signal() -> i32 {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => 15,
        _ = int.recv() => 2,
    }
}

/// SIGTERM or SIGINT: the server stops accepting, closes idle connections and finishes in-flight requests
pub async fn shutdown_signal() {
    SIGNAL.store(next_signal().await, std::sync::atomic::Ordering::SeqCst);
    eprintln!("INFO:     Shutting down");
    STOPPING.notify_one();
}

/// PY2AXUM_SHUTDOWN_TIMEOUT seconds (default 25) after the signal, or a second signal (uvicorn's force exit)
pub async fn shutdown_deadline() {
    STOPPING.notified().await;
    let secs: f64 = std::env::var("PY2AXUM_SHUTDOWN_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(25.0);
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_secs_f64(secs.max(0.0))) => {
            eprintln!("WARNING:  py2axum: shutdown timeout, open connections dropped");
        }
        _ = next_signal() => {}
    }
}

/// the pool closed (Terminate sent to PostgreSQL), then the signal raised again with its default action
pub async fn exit_after_shutdown(pool: &sqlx::PgPool) -> ! {
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), pool.close()).await;
    let sig = SIGNAL.load(std::sync::atomic::Ordering::SeqCst);
    if sig != 0 {
        unsafe extern "C" {
            fn signal(signum: i32, handler: usize) -> usize;
            fn raise(sig: i32) -> i32;
        }
        // SAFETY: restores SIG_DFL (0 on Linux and macOS) for the signal, then delivers it to this process
        unsafe {
            signal(sig, 0);
            raise(sig);
        }
    }
    std::process::exit(if sig == 0 { 0 } else { 128 + sig })
}
