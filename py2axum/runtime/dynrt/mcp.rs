//! The `mcp` SDK 2.2 server side, natively: `MCPServer(...)`, `@server.tool(...)` on `async def` tools
//! returning `dict[str, Any]`, `ToolError`, served by `server.session_manager.handle_request(scope,
//! receive, send)` over streamable HTTP **without state and with JSON responses**
//! (`streamable_http_app(stateless_http=True, json_response=True)`), as `mcp/server/streamable_http.py`
//! and `mcp/server/runner.py` answer it: transport checks in the SDK's order, the JSON-RPC envelope
//! validated like pydantic (same error text), `initialize`, `ping`, `tools/list`, `tools/call`, empty
//! resources/prompts lists. The tool's argument model, its JSON schema and the list of parameters
//! annotated `str` are computed by the transpiler (`ToolSpec`).
use std::sync::Arc;

use parking_lot::RwLock;

use super::pyd::{self, ErrDetail, SchemaDesc, TD};
use super::v::*;
use super::{ops, Cx};

/// What the transpiler knows of a `@server.tool(...)` function.
pub struct ToolSpec {
    pub name: &'static str,
    pub title: Option<&'static str>,
    pub description: &'static str,
    /// `inputSchema` (FastMCP's `<function>Arguments` model), JSON text in wire order
    pub input_schema: &'static str,
    /// `outputSchema`, JSON text
    pub output_schema: &'static str,
    /// the arguments model (`<function>Arguments`)
    pub args: &'static SchemaDesc,
    /// (parameter, annotated exactly `str`): the others get FastMCP's JSON pre-parsing of strings
    pub params: &'static [(&'static str, bool)],
}

pub struct Server {
    name: String,
    title: Option<String>,
    instructions: Option<String>,
    version: String,
    tools: RwLock<Vec<(&'static ToolSpec, V)>>,
}

const HANDSHAKE: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
const LATEST: &str = "2025-11-25";
const MAX_BODY: usize = 4 * 1024 * 1024;

fn kwarg<'a>(kwargs: &'a [(String, V)], k: &str) -> Option<&'a V> {
    kwargs.iter().find(|(n, _)| n == k).map(|(_, v)| v)
}

fn opt_str(v: Option<&V>) -> R<Option<String>> {
    match v {
        None | Some(V::None) => Ok(None),
        Some(V::Str(s)) => Ok(Some(s.to_string())),
        Some(o) => Err(Exc::type_error(format!("py2axum: MCPServer string option, got {}", o.type_name()))),
    }
}

/// `MCPServer(name=None, title=None, instructions=None, version=None)`
pub fn server(args: &[V], kwargs: &[(String, V)]) -> R {
    let name = opt_str(args.first().or_else(|| kwarg(kwargs, "name")))?.unwrap_or_else(|| "mcp-server".into());
    let title = opt_str(kwarg(kwargs, "title"))?;
    let instructions = opt_str(args.get(1).or_else(|| kwarg(kwargs, "instructions")))?;
    let version = opt_str(kwarg(kwargs, "version"))?.unwrap_or_default();
    Ok(V::native(Native::McpServer(Arc::new(Server { name, title, instructions, version, tools: RwLock::new(Vec::new()) }))))
}

/// `@server.tool(...)` (built by the transpiler): registers the function, returns it unchanged
pub fn tool_decorator(server: &V, spec: &'static ToolSpec) -> R {
    match server {
        V::Native(n) if matches!(&**n, Native::McpServer(_)) => {
            let Native::McpServer(s) = &**n else { unreachable!() };
            Ok(V::native(Native::McpToolDeco(s.clone(), spec)))
        }
        o => Err(Exc::type_error(format!("py2axum: @{}.tool() on a non-MCPServer value", o.type_name()))),
    }
}

pub fn decorate(s: &Arc<Server>, spec: &'static ToolSpec, args: &[V]) -> R {
    let f = args.first().cloned().ok_or_else(|| Exc::type_error("tool() decorator needs a function"))?;
    let mut tools = s.tools.write();
    // FastMCP's ToolManager: a second tool of the same name keeps the first (with a warning)
    if tools.iter().any(|(t, _)| t.name == spec.name) {
        eprintln!("WARNING:mcp.server.mcpserver.tools.tool_manager:Tool already exists: {}", spec.name);
    } else {
        tools.push((spec, f.clone()));
    }
    Ok(f)
}

pub fn attr(s: &Arc<Server>, name: &str) -> R {
    match name {
        "session_manager" => Ok(V::native(Native::McpManager(s.clone()))),
        "name" => Ok(V::str(&s.name)),
        "title" => Ok(s.title.as_deref().map(V::str).unwrap_or(V::None)),
        "instructions" => Ok(s.instructions.as_deref().map(V::str).unwrap_or(V::None)),
        _ => Err(Exc::attr_error(format!("'MCPServer' object has no attribute '{name}'"))),
    }
}

/// `server.streamable_http_app(...)`: the options the transpiler checked; the app itself is not used
pub fn server_method(_s: &Arc<Server>, name: &str) -> R {
    match name {
        "streamable_http_app" => Ok(V::None),
        _ => Err(Exc::attr_error(format!("'MCPServer' object has no attribute '{name}'"))),
    }
}

// ---------------------------------------------------------------- ASGI side

fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
}

fn bytes(v: &V) -> Vec<u8> {
    match v {
        V::Bytes(b) => b.to_vec(),
        V::Str(s) => s.as_bytes().to_vec(),
        _ => Vec::new(),
    }
}

fn dict_get(d: &V, k: &str) -> Option<V> {
    match d {
        V::Dict(m) => m.lock().get(&Key::Str(Arc::from(k))).map(|(_, v)| v.clone()),
        _ => None,
    }
}

struct Out {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
    /// a never-ending SSE stream (GET)
    stream: bool,
}

fn json_out(status: u16, body: String) -> Out {
    let len = body.len().to_string();
    Out { status, headers: vec![("content-type", "application/json".into()), ("content-length", len)], body: body.into_bytes(), stream: false }
}

fn plain(status: u16, body: &str) -> Out {
    Out { status, headers: vec![("content-length", body.len().to_string())], body: body.as_bytes().to_vec(), stream: false }
}

/// `{"jsonrpc":"2.0","id":null,"error":{...}}` (the legacy transport's errors)
fn transport_error(status: u16, code: i64, msg: &str) -> Out {
    json_out(status, format!("{{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{{\"code\":{code},\"message\":{}}}}}", js(msg)))
}

fn js(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// `session_manager.handle_request(scope, receive, send)`
pub async fn handle_request(cx: &Cx, s: &Arc<Server>, args: &[V]) -> R {
    let [scope, receive, send] = args else {
        return Err(Exc::type_error("handle_request() takes 3 positional arguments"));
    };
    let method = match dict_get(scope, "method") {
        Some(V::Str(m)) => m.to_string(),
        _ => return Err(Exc::type_error("py2axum: handle_request() needs an http scope")),
    };
    let mut headers = Vec::new();
    if let Some(h) = dict_get(scope, "headers") {
        for pair in ops::iter(&h)? {
            let kv = ops::iter(&pair)?;
            if let [k, v] = &kv[..] {
                headers.push((String::from_utf8_lossy(&bytes(k)).to_lowercase(), String::from_utf8_lossy(&bytes(v)).into_owned()));
            }
        }
    }
    // the body (the server delivers it in one message)
    let mut body = Vec::new();
    if method == "POST" {
        loop {
            let msg = super::aio::await_value(super::methods::call_value_boxed(cx, receive, vec![]).await?).await?;
            if let Some(b) = dict_get(&msg, "body") {
                body.extend(bytes(&b));
            }
            if !matches!(dict_get(&msg, "more_body"), Some(V::Bool(true))) {
                break;
            }
        }
    }
    let out = respond(cx, s, &method, &headers, &body).await?;
    let hs: Vec<V> = out.headers.iter().map(|(k, v)| V::tuple(vec![V::Bytes(Arc::from(k.as_bytes())), V::Bytes(Arc::from(v.as_bytes()))])).collect();
    let start = V::dict_from(vec![(V::str("type"), V::str("http.response.start")), (V::str("status"), V::Int(out.status as i64)), (V::str("headers"), V::list(hs))])?;
    super::aio::await_value(super::methods::call_value_boxed(cx, send, vec![start]).await?).await?;
    let msg = V::dict_from(vec![
        (V::str("type"), V::str("http.response.body")),
        (V::str("body"), V::Bytes(Arc::from(&out.body[..]))),
        (V::str("more_body"), V::Bool(out.stream)),
    ])?;
    super::aio::await_value(super::methods::call_value_boxed(cx, send, vec![msg]).await?).await?;
    if out.stream {
        // the standalone SSE stream of a stateless server: nothing is ever sent on it
        std::future::pending::<()>().await;
    }
    Ok(V::None)
}

fn accepts(headers: &[(String, String)]) -> (bool, bool) {
    let accept = header(headers, "accept").unwrap_or_default();
    let types: Vec<String> = accept.split(',').map(|t| t.split(';').next().unwrap_or("").trim().to_lowercase()).collect();
    let json = types.iter().any(|t| t == "*/*" || t == "application/json" || t == "application/*");
    let sse = types.iter().any(|t| t == "*/*" || t == "text/event-stream" || t == "text/*");
    (json, sse)
}

async fn respond(cx: &Cx, s: &Arc<Server>, method: &str, headers: &[(String, String)], body: &[u8]) -> R<Out> {
    // RequestBodyLimitMiddleware
    let declared = header(headers, "content-length").and_then(|l| l.trim().parse::<usize>().ok()).unwrap_or(0);
    if declared > MAX_BODY || body.len() > MAX_BODY {
        return Ok(plain(413, "Request body too large"));
    }
    if let Some(v) = header(headers, "mcp-protocol-version") {
        if !HANDSHAKE.contains(&v.as_str()) {
            return Ok(modern(method, headers, body));
        }
    }
    let ct = header(headers, "content-type");
    if method == "POST" && !ct.as_deref().is_some_and(|c| c.to_lowercase().starts_with("application/json")) {
        return Ok(plain(400, "Invalid Content-Type header"));
    }
    match method {
        "POST" => {}
        "GET" => {
            if !accepts(headers).1 {
                return Ok(transport_error(406, -32600, "Not Acceptable: Client must accept text/event-stream"));
            }
            return Ok(Out {
                status: 200,
                headers: vec![
                    ("cache-control", "no-cache, no-transform".into()),
                    ("connection", "keep-alive".into()),
                    ("content-type", "text/event-stream".into()),
                    ("x-accel-buffering", "no".into()),
                ],
                body: Vec::new(),
                stream: true,
            });
        }
        "DELETE" => return Ok(transport_error(405, -32600, "Method Not Allowed: Session termination not supported")),
        _ => {
            let mut o = transport_error(405, -32600, "Method Not Allowed");
            o.headers.insert(1, ("allow", "GET, POST, DELETE".into()));
            if method == "HEAD" {
                o.body.clear();
            }
            return Ok(o);
        }
    }
    if !accepts(headers).0 {
        return Ok(transport_error(406, -32600, "Not Acceptable: Client must accept application/json"));
    }
    let strict = ct.unwrap_or_default();
    if !strict.split(';').next().unwrap_or("").split(',').any(|t| t.trim() == "application/json") {
        return Ok(transport_error(415, -32600, "Unsupported Media Type: Content-Type must be application/json"));
    }
    let raw: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return Ok(transport_error(400, -32700, &format!("Parse error: {e}"))),
    };
    let msg = pyd::from_serde(&raw);
    let kind = match envelope(&msg) {
        Ok(k) => k,
        Err(errs) => {
            let text = pyd::error_str("union[JSONRPCRequest,JSONRPCNotification,JSONRPCResponse,JSONRPCError]", &errs);
            return Ok(transport_error(400, -32602, &format!("Validation error: {text}")));
        }
    };
    let Some(id) = kind else {
        // a notification, or a response/error posted by the client
        return Ok(Out { status: 202, headers: vec![("content-type", "application/json".into()), ("content-length", "0".into())], body: Vec::new(), stream: false });
    };
    let method_name = match dict_get(&msg, "method") {
        Some(V::Str(m)) => m.to_string(),
        _ => String::new(),
    };
    let params = dict_get(&msg, "params").filter(|p| !p.is_none());
    let reply = match dispatch(cx, s, &method_name, params.as_ref(), headers).await? {
        Ok(result) => format!("{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{}}}", id_json(&id), result),
        Err((code, message, data)) => format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{},\"error\":{{\"code\":{code},\"message\":{}{}}}}}",
            id_json(&id),
            js(&message),
            data.map(|d| format!(",\"data\":{d}")).unwrap_or_default()
        ),
    };
    Ok(json_out(200, reply))
}

fn id_json(id: &V) -> String {
    match id {
        V::Int(i) => i.to_string(),
        V::Str(s) => js(s),
        _ => "null".into(),
    }
}

/// a protocol version the SDK routes to its 2026 transport: what it answers to a request without the
/// envelope it requires (requests carrying it are not supported by the binary)
fn modern(method: &str, headers: &[(String, String)], body: &[u8]) -> Out {
    if method != "POST" {
        return Out { status: 405, headers: vec![("allow", "POST".into()), ("content-length", "0".into())], body: Vec::new(), stream: false };
    }
    let ct = header(headers, "content-type").unwrap_or_default();
    if !ct.to_lowercase().starts_with("application/json") {
        return plain(400, "Invalid Content-Type header");
    }
    if !accepts(headers).0 {
        return Out { status: 406, headers: vec![("content-length", "0".into())], body: Vec::new(), stream: false };
    }
    let err = |id: &str, code: i64, msg: &str, first: bool| {
        let body = if first {
            format!("{{\"jsonrpc\":\"2.0\",\"error\":{{\"code\":{code},\"message\":{}}},\"id\":{id}}}", ascii(msg))
        } else {
            format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":{code},\"message\":{}}}}}", ascii(msg))
        };
        Out { status: 400, headers: vec![("content-length", body.len().to_string()), ("content-type", "application/json".into())], body: body.into_bytes(), stream: false }
    };
    let raw: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return err("null", -32700, "Parse error", true),
    };
    let shape = "Body must be a single JSON-RPC request or notification object";
    let Some(obj) = raw.as_object() else {
        return err("null", -32600, shape, true);
    };
    if !obj.get("method").is_some_and(|m| m.is_string()) {
        return err("null", -32600, shape, true);
    }
    let version = header(headers, "mcp-protocol-version").unwrap_or_default();
    let Some(id) = obj.get("id") else {
        if version != MODERN {
            let body = format!(
                "{{\"jsonrpc\":\"2.0\",\"error\":{{\"code\":-32022,\"message\":\"Unsupported protocol version\",\"data\":{{\"supported\":[\"{MODERN}\"],\"requested\":{}}}}},\"id\":null}}",
                ascii(&version)
            );
            return Out { status: 400, headers: vec![("content-length", body.len().to_string()), ("content-type", "application/json".into())], body: body.into_bytes(), stream: false };
        }
        return Out { status: 202, headers: vec![("content-length", "0".into())], body: Vec::new(), stream: false };
    };
    let id = serde_json::to_string(id).unwrap_or_else(|_| "null".into());
    let meta = obj.get("params").and_then(|p| p.get("_meta")).and_then(|m| m.as_object());
    let envelope = meta.and_then(|m| Some((m.get("io.modelcontextprotocol/protocolVersion")?, m.get("io.modelcontextprotocol/clientCapabilities")?)));
    let Some((asked, _)) = envelope else {
        return err(
            &id,
            -32602,
            "params._meta must be an object carrying the required 'io.modelcontextprotocol/protocolVersion' and 'io.modelcontextprotocol/clientCapabilities' envelope keys",
            false,
        );
    };
    if asked.as_str() != Some(version.as_str()) {
        return err(&id, -32020, "mcp-protocol-version header does not match the request envelope's protocol version", false);
    }
    if header(headers, "mcp-method").as_deref() != obj.get("method").and_then(|m| m.as_str()) {
        return err(&id, -32020, "mcp-method header does not match the request body's method", false);
    }
    // a well-formed 2026-07-28 request: that protocol is not implemented by the binary
    err(&id, -32022, "Unsupported protocol version", false)
}

const MODERN: &str = "2026-07-28";

/// `json.dumps(s)` (ensure_ascii)
fn ascii(s: &str) -> String {
    pyd::to_json(&V::str(s), &pyd::DUMPS, false).unwrap_or_default()
}

// ---------------------------------------------------------------- the JSON-RPC envelope

fn err(kind: &'static str, loc: Vec<V>, msg: &str, input: &V) -> ErrDetail {
    ErrDetail { kind, loc, msg: msg.to_string(), input: input.clone(), ctx: None }
}

fn loc(model: &str, rest: &[&str]) -> Vec<V> {
    std::iter::once(V::str(model)).chain(rest.iter().map(|s| V::str(*s))).collect()
}

fn check_jsonrpc(m: &str, d: &V, errs: &mut Vec<ErrDetail>) {
    match dict_get(d, "jsonrpc") {
        None => errs.push(err("missing", loc(m, &["jsonrpc"]), "Field required", d)),
        Some(V::Str(s)) if &*s == "2.0" => {}
        Some(v) => errs.push(ErrDetail { kind: "literal_error", loc: loc(m, &["jsonrpc"]), msg: "Input should be '2.0'".into(), input: v, ctx: None }),
    }
}

/// `RequestId = Annotated[int, Field(strict=True)] | str`
fn check_id(m: &str, d: &V, nullable: bool, errs: &mut Vec<ErrDetail>) {
    match dict_get(d, "id") {
        None => errs.push(err("missing", loc(m, &["id"]), "Field required", d)),
        Some(V::Int(_)) | Some(V::Str(_)) => {}
        Some(V::None) if nullable => {}
        Some(v) => {
            errs.push(err("int_type", loc(m, &["id", "int"]), "Input should be a valid integer", &v));
            errs.push(err("string_type", loc(m, &["id", "str"]), "Input should be a valid string", &v));
        }
    }
}

fn check_str(m: &str, d: &V, field: &str, errs: &mut Vec<ErrDetail>) {
    match dict_get(d, field) {
        None => errs.push(err("missing", vec![V::str(m), V::str(field)], "Field required", d)),
        Some(V::Str(_)) => {}
        Some(v) => errs.push(err("string_type", vec![V::str(m), V::str(field)], "Input should be a valid string", &v)),
    }
}

fn check_dict(m: &str, d: &V, field: &str, optional: bool, errs: &mut Vec<ErrDetail>) {
    match dict_get(d, field) {
        None if optional => {}
        None => errs.push(err("missing", vec![V::str(m), V::str(field)], "Field required", d)),
        Some(V::Dict(_)) => {}
        Some(V::None) if optional => {}
        Some(v) => errs.push(err("dict_type", vec![V::str(m), V::str(field)], "Input should be a valid dictionary", &v)),
    }
}

/// `ErrorData`: code (lax int), message (str), data (Any)
fn check_error_data(d: &V, errs: &mut Vec<ErrDetail>) {
    let m = "JSONRPCError";
    let Some(e) = dict_get(d, "error") else {
        errs.push(err("missing", loc(m, &["error"]), "Field required", d));
        return;
    };
    if !matches!(e, V::Dict(_)) {
        errs.push(err("model_type", loc(m, &["error"]), "Input should be a valid dictionary or instance of ErrorData", &e));
        return;
    }
    let mut sub = Vec::new();
    match dict_get(&e, "code") {
        None => sub.push(err("missing", loc(m, &["error", "code"]), "Field required", &e)),
        Some(c) => {
            let mut es = Vec::new();
            let _ = pyd::validate_sync(&c, &TD::Int(pyd::NO_NUM), &[V::str(m), V::str("error"), V::str("code")], &mut es);
            sub.extend(es);
        }
    }
    match dict_get(&e, "message") {
        None => sub.push(err("missing", loc(m, &["error", "message"]), "Field required", &e)),
        Some(V::Str(_)) => {}
        Some(v) => sub.push(err("string_type", loc(m, &["error", "message"]), "Input should be a valid string", &v)),
    }
    errs.extend(sub);
}

/// The message as pydantic's smart union of the four models sees it: Ok(Some(id)) for a request,
/// Ok(None) for anything else that validates, Err(errors of every member) otherwise.
fn envelope(d: &V) -> Result<Option<V>, Vec<ErrDetail>> {
    let models = ["JSONRPCRequest", "JSONRPCNotification", "JSONRPCResponse", "JSONRPCError"];
    if !matches!(d, V::Dict(_)) {
        return Err(models
            .iter()
            .map(|m| err("model_type", vec![V::str(*m)], &format!("Input should be a valid dictionary or instance of {m}"), d))
            .collect());
    }
    let mut all = Vec::new();
    // JSONRPCRequest
    let mut e = Vec::new();
    check_jsonrpc(models[0], d, &mut e);
    check_id(models[0], d, false, &mut e);
    check_str(models[0], d, "method", &mut e);
    check_dict(models[0], d, "params", true, &mut e);
    if e.is_empty() {
        return Ok(dict_get(d, "id"));
    }
    all.extend(e);
    // JSONRPCNotification
    let mut e = Vec::new();
    check_jsonrpc(models[1], d, &mut e);
    check_str(models[1], d, "method", &mut e);
    check_dict(models[1], d, "params", true, &mut e);
    if e.is_empty() {
        return Ok(None);
    }
    all.extend(e);
    // JSONRPCResponse
    let mut e = Vec::new();
    check_jsonrpc(models[2], d, &mut e);
    check_id(models[2], d, false, &mut e);
    check_dict(models[2], d, "result", false, &mut e);
    if e.is_empty() {
        return Ok(None);
    }
    all.extend(e);
    // JSONRPCError
    let mut e = Vec::new();
    check_jsonrpc(models[3], d, &mut e);
    check_id(models[3], d, true, &mut e);
    check_error_data(d, &mut e);
    if e.is_empty() {
        return Ok(None);
    }
    all.extend(e);
    Err(all)
}

// ---------------------------------------------------------------- dispatch

type Reply = Result<String, (i64, String, Option<String>)>;

fn invalid_params() -> Reply {
    Err((-32602, "Invalid request parameters".into(), Some("\"\"".into())))
}

fn is_str(p: Option<&V>, k: &str) -> bool {
    p.is_some_and(|p| matches!(dict_get(p, k), Some(V::Str(_))))
}

fn opt_of(p: Option<&V>, k: &str, ok: fn(&V) -> bool) -> bool {
    match p.and_then(|p| dict_get(p, k)) {
        None | Some(V::None) => true,
        Some(v) => ok(&v),
    }
}

async fn dispatch(cx: &Cx, s: &Arc<Server>, method: &str, params: Option<&V>, headers: &[(String, String)]) -> R<Reply> {
    let _ = headers;
    Ok(match method {
        "initialize" => {
            let ok = params.is_some()
                && is_str(params, "protocolVersion")
                && matches!(params.and_then(|p| dict_get(p, "capabilities")), Some(V::Dict(_)))
                && match params.and_then(|p| dict_get(p, "clientInfo")) {
                    Some(ci @ V::Dict(_)) => is_str(Some(&ci), "name") && is_str(Some(&ci), "version"),
                    _ => false,
                };
            if !ok {
                return Ok(invalid_params());
            }
            let asked = match params.and_then(|p| dict_get(p, "protocolVersion")) {
                Some(V::Str(v)) => v.to_string(),
                _ => String::new(),
            };
            let version = if HANDSHAKE.contains(&asked.as_str()) { asked } else { LATEST.to_string() };
            let mut out = String::from("{\"capabilities\":{\"experimental\":{},\"prompts\":{\"listChanged\":false},\"resources\":{\"listChanged\":false,\"subscribe\":false},\"tools\":{\"listChanged\":false}}");
            if let Some(i) = &s.instructions {
                out += &format!(",\"instructions\":{}", js(i));
            }
            out += &format!(",\"protocolVersion\":{},\"serverInfo\":{{\"name\":{}", js(&version), js(&s.name));
            if let Some(t) = &s.title {
                out += &format!(",\"title\":{}", js(t));
            }
            out += &format!(",\"version\":{}}}}}", js(&s.version));
            Ok(out)
        }
        "ping" => Ok("{}".into()),
        "tools/list" | "resources/list" | "resources/templates/list" | "prompts/list" => {
            if !opt_of(params, "cursor", |v| matches!(v, V::Str(_))) {
                return Ok(invalid_params());
            }
            match method {
                "tools/list" => Ok(tools_list(s)),
                "resources/list" => Ok("{\"resources\":[]}".into()),
                "resources/templates/list" => Ok("{\"resourceTemplates\":[]}".into()),
                _ => Ok("{\"prompts\":[]}".into()),
            }
        }
        "resources/read" => {
            let Some(V::Str(uri)) = params.and_then(|p| dict_get(p, "uri")) else {
                return Ok(invalid_params());
            };
            Err((-32602, format!("Unknown resource: {uri}"), Some(format!("{{\"uri\":{}}}", js(&uri)))))
        }
        "prompts/get" => {
            if !is_str(params, "name") || !opt_of(params, "arguments", |v| matches!(v, V::Dict(_))) {
                return Ok(invalid_params());
            }
            let Some(V::Str(name)) = params.and_then(|p| dict_get(p, "name")) else { unreachable!() };
            Err((0, format!("Unknown prompt: {name}"), None))
        }
        "tools/call" => {
            if !is_str(params, "name") || !opt_of(params, "arguments", |v| matches!(v, V::Dict(_))) {
                return Ok(invalid_params());
            }
            let Some(V::Str(name)) = params.and_then(|p| dict_get(p, "name")) else { unreachable!() };
            let arguments = params.and_then(|p| dict_get(p, "arguments")).filter(|a| !a.is_none());
            call_tool(cx, s, &name, arguments).await?
        }
        other => Err((-32601, "Method not found".into(), Some(js(other)))),
    })
}

fn tools_list(s: &Arc<Server>) -> String {
    let tools = s.tools.read();
    let items: Vec<String> = tools
        .iter()
        .map(|(t, _)| {
            let mut o = format!("{{\"description\":{},\"inputSchema\":{},\"name\":{},\"outputSchema\":{}", js(t.description), t.input_schema, js(t.name), t.output_schema);
            if let Some(title) = t.title {
                o += &format!(",\"title\":{}", js(title));
            }
            o + "}"
        })
        .collect();
    format!("{{\"tools\":[{}]}}", items.join(","))
}

fn text_result(text: &str, is_error: bool) -> String {
    format!("{{\"content\":[{{\"text\":{},\"type\":\"text\"}}],\"isError\":{is_error}}}", js(text))
}

/// FastMCP's `pre_parse_json`: a string given for a parameter not annotated `str` is replaced by its
/// JSON value when that value is a list, a dict, a bool or null
fn pre_parse(spec: &ToolSpec, args: &V) -> R {
    let V::Dict(m) = args else { return Ok(args.clone()) };
    let items: Vec<(V, V)> = m.lock().values().cloned().collect();
    let mut out = Vec::with_capacity(items.len());
    for (k, v) in items {
        let parse = match (&k, &v) {
            (V::Str(name), V::Str(_)) => spec.params.iter().any(|(p, is_str)| p == &&**name && !is_str),
            _ => false,
        };
        if parse {
            if let V::Str(text) = &v {
                if let Ok(j) = serde_json::from_str::<serde_json::Value>(text) {
                    if j.is_array() || j.is_object() || j.is_boolean() || j.is_null() {
                        out.push((k, pyd::from_serde(&j)));
                        continue;
                    }
                }
            }
        }
        out.push((k, v));
    }
    V::dict_from(out)
}

async fn call_tool(cx: &Cx, s: &Arc<Server>, name: &str, arguments: Option<V>) -> R<Reply> {
    let found = s.tools.read().iter().find(|(t, _)| t.name == name).map(|(t, f)| (*t, f.clone()));
    let Some((spec, f)) = found else {
        return Ok(Ok(text_result(&format!("Unknown tool: {name}"), true)));
    };
    let args = match arguments {
        Some(a) => a,
        None => V::dict_from(vec![])?,
    };
    let args = pre_parse(spec, &args)?;
    let mut errs = Vec::new();
    let td: &'static TD = Box::leak(Box::new(TD::Schema(spec.args)));
    let inst = pyd::validate(cx, &args, td, &[], &mut errs).await?;
    let Some(inst) = inst.filter(|_| errs.is_empty()) else {
        let text = pyd::error_str(spec.args.name, &errs);
        return Ok(Ok(text_result(&format!("Error executing tool {name}: {text}"), true)));
    };
    // model_dump_one_level: the validated values, by parameter name
    let kwargs: Vec<(String, V)> = match &inst {
        V::Inst(i) => {
            let vals = i.vals.lock().clone();
            spec.args.fields.iter().zip(vals).map(|(fd, v)| (fd.name.to_string(), v)).collect()
        }
        _ => return Err(Exc::runtime("py2axum: tool arguments did not validate to a model")),
    };
    let result = super::methods::call_value_kw_boxed(cx, &f, kwargs).await;
    let result = match result {
        Ok(v) => super::aio::await_value(v).await,
        Err(e) => Err(e),
    };
    match result {
        Ok(v) => {
            let jv = pyd::dump(&v, pyd::DumpOpts { json: true, ..Default::default() })?;
            if !matches!(jv, V::Dict(_)) {
                return Ok(Ok(text_result(&format!("Error executing tool {name}"), true)));
            }
            let text = indent_json(&jv)?;
            let structured = pyd::to_json(&jv, &pyd::RESPONSE, false)?;
            Ok(Ok(format!("{{\"content\":[{{\"text\":{},\"type\":\"text\"}}],\"isError\":false,\"structuredContent\":{structured}}}", js(&text))))
        }
        Err(e) if e.isinstance(&MCP_TOOL_ERROR) => Ok(Ok(text_result(&format!("Error executing tool {name}: {}", e.message()), true))),
        Err(e) if e.isinstance(&EXCEPTION) => {
            eprintln!("ERROR:mcp.server.mcpserver.tools.base:Error executing tool {name}: {:?}", e);
            Ok(Ok(text_result(&format!("Error executing tool {name}"), true)))
        }
        Err(e) => Err(e),
    }
}

/// `pydantic_core.to_json(value, indent=2)`: 2-space indent, `": "`, raw UTF-8
fn indent_json(v: &V) -> R<String> {
    let mut out = String::new();
    write_indented(&mut out, v, 0)?;
    Ok(out)
}

fn write_indented(out: &mut String, v: &V, level: usize) -> R<()> {
    let pad = |n: usize| "  ".repeat(n);
    match v {
        V::Dict(d) => {
            let items = d.lock().values().cloned().collect::<Vec<_>>();
            if items.is_empty() {
                out.push_str("{}");
                return Ok(());
            }
            out.push_str("{\n");
            for (i, (k, x)) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                out.push_str(&pad(level + 1));
                out.push_str(&pyd::to_json(&V::str(ops::str_(k)?), &pyd::RESPONSE, false)?);
                out.push_str(": ");
                write_indented(out, x, level + 1)?;
            }
            out.push('\n');
            out.push_str(&pad(level));
            out.push('}');
        }
        V::List(_) | V::Tuple(_) => {
            let items = ops::iter(v)?;
            if items.is_empty() {
                out.push_str("[]");
                return Ok(());
            }
            out.push_str("[\n");
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                out.push_str(&pad(level + 1));
                write_indented(out, x, level + 1)?;
            }
            out.push('\n');
            out.push_str(&pad(level));
            out.push(']');
        }
        other => out.push_str(&pyd::to_json(other, &pyd::RESPONSE, false)?),
    }
    Ok(())
}

pub async fn manager_method(cx: &Cx, s: &Arc<Server>, name: &str, args: &[V]) -> R {
    match name {
        // `async with session_manager.run():` (the task group of a stateless manager holds nothing)
        "run" => Ok(V::native(Native::McpRun)),
        "handle_request" => {
            let (cx2, s2, args2) = (cx.clone(), s.clone(), args.to_vec());
            Ok(super::aio::coro(Box::pin(async move { handle_request(&cx2, &s2, &args2).await })))
        }
        _ => Err(Exc::attr_error(format!("'StreamableHTTPSessionManager' object has no attribute '{name}'"))),
    }
}
