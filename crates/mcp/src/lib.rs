//! Model Context Protocol server: lets AI agents search and read code.
//!
//! `POST /.api/mcp` speaks the MCP Streamable HTTP transport, stateless and JSON-only: each
//! request carries one JSON-RPC message (or a batch), each response is `application/json`,
//! notifications get `202 Accepted`, and no `Mcp-Session-Id` is issued. `GET` and `DELETE`
//! answer 405, as the transport allows for a server without server-initiated messages.
//!
//! Callers authenticate as for the other APIs (`Authorization: token …`, or a session with
//! `X-Requested-With`); `Authorization: Bearer <access token>` is accepted here too, since
//! that is what MCP clients send, and so are the OAuth access tokens of
//! `sourcegull-oauth-server`, which work nowhere else. Without credentials the `401` carries
//! `WWW-Authenticate: Bearer resource_metadata="…"`, from which OAuth-capable clients discover
//! how to sign the user in. Every tool runs as the caller, so repository permissions apply to
//! every result; an unreadable repository looks like a missing one.
//!
//! The tools ([`tools`]) answer in compact text meant for a model's context: ripgrep-like
//! search results, numbered file lines, one line per symbol, usage or commit, with a hint
//! when output was cut.

mod feature;
mod format;
pub mod tools;

pub use feature::McpFeature;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};
use sourcegull_authn::{Actor, AuthError};
use sourcegull_http_extract::{MaybeActor, auth_error_response, error_response};
use sourcegull_services::AppState;

/// The endpoint path.
pub const PATH: &str = "/.api/mcp";

/// Protocol revisions this server speaks, newest first. A client asking for another one
/// gets the newest.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Server instructions sent with `initialize`: how to use the tools well.
const INSTRUCTIONS: &str = "\
Sourcegull searches and navigates the code of every repository this server mirrors, as the \
signed-in user may see it. Prefer it over cloning or reading many files.

Workflow: `search` (or `find_symbols` for definitions by name) to locate code, then \
`read_file` with a line range around the hits; `go_to_definition` / `find_references` on an \
identifier from a line you have read; `list_repos` / `list_files` to orient; \
`commit_log`, `get_commit`, `compare_revisions` and `blame` for history.

Repository names look like `gitlab.example.com/group/project`; any unique suffix such as \
`group/project` works. Lines are 1-based everywhere.

Query syntax (search tool): terms are ANDed per file, \"quoted phrase\" is exact, /regexp/ is a \
regular expression. Filters: repo:<regexp> (repo:^gitlab\\.com/acme/api$), file:<regexp> \
(file:\\.go$, -file:_test), lang:go, case:yes, type:symbol|file|path|commit|diff|repo, \
author:, before:/after: (commits), rev:<branch> (or repo:x@branch), select:repo|file, \
count:N, context:<name>. Combine with and/or/not and parentheses. Use count:all with care.";

/// The MCP routes, to merge into the public router.
pub fn routes() -> Router<AppState> {
    Router::new().route(
        PATH,
        post(handle)
            .get(method_not_allowed)
            .delete(method_not_allowed),
    )
}

async fn method_not_allowed() -> Response {
    let mut resp = StatusCode::METHOD_NOT_ALLOWED.into_response();
    resp.headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static("POST"));
    resp
}

/// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

async fn handle(
    State(state): State<AppState>,
    MaybeActor(actor): MaybeActor,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(resp) = check_origin(&state, &headers) {
        return resp;
    }
    let actor = match actor {
        Some(a) => Some(a),
        None => match bearer(&headers) {
            Some(token) => match authenticate_bearer(&state, &headers, &token).await {
                Ok(a) => a,
                Err(e) => {
                    let mut resp = auth_error_response(&e);
                    if resp.status() == StatusCode::UNAUTHORIZED {
                        set_challenge(&mut resp, &state, &headers, true);
                    }
                    return resp;
                }
            },
            None => None,
        },
    };
    if actor.is_none() && !state.config.load().value.auth_public {
        let mut resp = error_response(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "Sign in through OAuth, or send an access token: `Authorization: Bearer <token>` \
             or `Authorization: token <token>`.",
        );
        set_challenge(&mut resp, &state, &headers, false);
        return resp;
    }
    let message: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return json_response(rpc_error(Value::Null, PARSE_ERROR, &e.to_string())),
    };
    let ctx = tools::Ctx::new(&state, actor.as_ref());
    match message {
        Value::Array(batch) if !batch.is_empty() => {
            let mut out = Vec::new();
            for m in batch {
                if let Some(r) = dispatch(&ctx, m).await {
                    out.push(r);
                }
            }
            if out.is_empty() {
                StatusCode::ACCEPTED.into_response()
            } else {
                json_response(Value::Array(out))
            }
        }
        m => match dispatch(&ctx, m).await {
            Some(r) => json_response(r),
            None => StatusCode::ACCEPTED.into_response(),
        },
    }
}

/// Resolves a bearer token: an OAuth access token bound to this endpoint, or an access token.
async fn authenticate_bearer(
    state: &AppState,
    headers: &HeaderMap,
    token: &str,
) -> Result<Option<Actor>, AuthError> {
    if token.starts_with(sourcegull_authn::oauth::ACCESS_TOKEN_PREFIX) {
        let resource =
            sourcegull_oauth_server::resource_url(state, headers).ok_or(AuthError::TokenInvalid)?;
        return state.auth.authenticate_oauth_token(token, &resource).await;
    }
    state
        .auth
        .authenticate_access_token(&format!("token {token}"))
        .await
}

/// Sets the `WWW-Authenticate` challenge of a 401: with OAuth on, `resource_metadata` points
/// MCP clients to the authorization server (and `invalid_token` tells them to refresh).
fn set_challenge(resp: &mut Response, state: &AppState, headers: &HeaderMap, invalid: bool) {
    let mut value = String::from("Bearer realm=\"sourcegull\"");
    if let Some(url) = sourcegull_oauth_server::resource_metadata_url(state, headers) {
        if invalid {
            value.push_str(", error=\"invalid_token\"");
        }
        value.push_str(&format!(
            ", resource_metadata=\"{url}\", scope=\"{}\"",
            sourcegull_authn::oauth::SCOPE_MCP
        ));
    }
    if let Ok(v) = HeaderValue::from_str(&value) {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
}

/// The token of `Authorization: Bearer <token>`.
fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    let (scheme, token) = value.split_once(char::is_whitespace)?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then(|| token.to_owned())
}

/// Rejects browser requests from other origins (DNS rebinding, as the transport asks).
/// Requests without `Origin` (agents, CLIs) pass.
fn check_origin(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    let origin = headers.get(header::ORIGIN)?.to_str().ok()?;
    let config = state.config.load();
    let allowed = config
        .value
        .external_url
        .as_deref()
        .is_some_and(|u| u.trim_end_matches('/').eq_ignore_ascii_case(origin));
    let same_host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|host| {
            origin
                .split_once("://")
                .is_some_and(|(_, rest)| rest.eq_ignore_ascii_case(host))
        });
    (!allowed && !same_host).then(|| {
        error_response(
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
            "Cross-origin MCP requests are not allowed.",
        )
    })
}

fn json_response(v: Value) -> Response {
    let mut resp = axum::Json(v).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Answers one JSON-RPC message; `None` for notifications and responses.
async fn dispatch(ctx: &tools::Ctx<'_>, msg: Value) -> Option<Value> {
    let Value::Object(mut msg) = msg else {
        return Some(rpc_error(
            Value::Null,
            INVALID_REQUEST,
            "expected an object",
        ));
    };
    let id = msg.remove("id");
    let Some(method) = msg.get("method").and_then(Value::as_str).map(str::to_owned) else {
        // A response to a server request (we send none) or garbage.
        return id.map(|id| rpc_error(id, INVALID_REQUEST, "missing method"));
    };
    let id = id?;
    let params = msg.remove("params").unwrap_or(Value::Null);
    Some(match method.as_str() {
        "initialize" => rpc_result(id, initialize(&params)),
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, json!({ "tools": tools::definitions() })),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(rpc_error(id, INVALID_PARAMS, "missing tool name"));
            };
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match ctx.call(name, &args).await {
                Some(out) => rpc_result(id, out.into_result()),
                None => rpc_error(id, INVALID_PARAMS, &format!("unknown tool: {name}")),
            }
        }
        "resources/list" => rpc_result(id, json!({ "resources": [] })),
        "resources/templates/list" => rpc_result(id, json!({ "resourceTemplates": [] })),
        "prompts/list" => rpc_result(id, json!({ "prompts": [] })),
        _ => rpc_error(id, METHOD_NOT_FOUND, &format!("method not found: {method}")),
    })
}

fn initialize(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = requested
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "sourcegull",
            "title": "Sourcegull code search",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

/// The caller, for tests and other crates that call tools directly.
pub fn ctx<'a>(state: &'a AppState, actor: Option<&'a Actor>) -> tools::Ctx<'a> {
    tools::Ctx::new(state, actor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiates_versions() {
        let v = initialize(&json!({ "protocolVersion": "2025-03-26" }));
        assert_eq!(v["protocolVersion"], "2025-03-26");
        let v = initialize(&json!({ "protocolVersion": "1999-01-01" }));
        assert_eq!(v["protocolVersion"], PROTOCOL_VERSIONS[0]);
        assert!(v["instructions"].as_str().unwrap().contains("search"));
    }

    #[test]
    fn path_is_the_oauth_resource() {
        assert_eq!(PATH, sourcegull_oauth_server::RESOURCE_PATH);
    }

    #[test]
    fn parses_bearer() {
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer skp_x"),
        );
        assert_eq!(bearer(&h).as_deref(), Some("skp_x"));
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("token skp_x"),
        );
        assert_eq!(bearer(&h), None);
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer "));
        assert_eq!(bearer(&h), None);
    }
}
