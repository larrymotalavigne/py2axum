# MCP servers (`mcp` 2.2)

A FastMCP-style server served from a raw ASGI route: `MCPServer(name, title=,
instructions=, version=)`, `@server.tool(name=, title=, description=)` on `async def` tools returning
`dict[str, Any]`, `ToolError`, `server.streamable_http_app(stateless_http=True, json_response=True,
transport_security=TransportSecuritySettings(enable_dns_rebinding_protection=False))`, `async with
server.session_manager.run()`, `await server.session_manager.handle_request(scope, receive, send)`.

Raw ASGI routes are described in [Bigger applications](../tutorial/bigger-applications.md); the `mcp`
versions accepted in [Versions](../reference/versions.md).

## What is native

- Transport like `mcp/server/streamable_http.py` without sessions: 413 above 4 MiB, `Invalid Content-Type
  header`, 406 (Accept), 415 (strict content type), `Parse error` (-32700), the JSON-RPC envelope validated
  like pydantic's union of the four message models (same `Validation error: ...` text), 202 for
  notifications and posted responses, GET = an SSE stream that never sends anything, DELETE/HEAD 405.
- `initialize` (version negotiation, capabilities, `serverInfo`, `instructions`), `ping`, `tools/list`,
  `tools/call`, empty `resources/list`, `resources/templates/list`, `prompts/list`, `resources/read` and
  `prompts/get` errors, -32601 for the rest; -32602 `Invalid request parameters` for malformed params.
- Tools: the `<function>Arguments` model and its `inputSchema` are computed at compile time (pydantic 2.13's
  JSON schema, a closed subset of types: `str/int/float/bool/date/datetime/time/timedelta/UUID`, `Literal`,
  `Optional`/unions, `list`, `dict[str, T]`, `Any`, project `BaseModel`s; `Field(description=, title=,
  default=, examples=)` and constraints), FastMCP's JSON pre-parsing of string arguments, validation errors
  as `Error executing tool <name>: <pydantic message>`, `ToolError` text, other exceptions as
  `Error executing tool <name>`, structured output (`structuredContent` + indented JSON text).

## What stays in Python

- Refused at compile time: sync tools, other return annotations, other `tool()` options, parameters
  starting with `_` or shadowing a `BaseModel` attribute, resources, prompts, any other `MCPServer`
  attribute, sessions (`stateless_http=False`), SSE responses (`json_response=False`), Host/Origin checks.
- A FastMCP application mounted with `app.mount(...)` is not translated; it can be left to Python with
  `--python-side mount` ([Bigger applications](../tutorial/bigger-applications.md#mounted-applications)).

## Differences

The 2026-07-28 transport (a `mcp-protocol-version` outside the handshake list) answers its
envelope and header errors like the SDK, but a well-formed request of that protocol gets -32022
`Unsupported protocol version`; `capabilities` of `initialize` is only checked to be an object; JSON
parse-error messages come from serde_json (the same wording as pydantic-core's jiter for the usual
cases); floats in tool results use Python's repr rather than pydantic-core's formatting.
