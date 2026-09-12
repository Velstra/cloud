//! The JSON gateway from `docs/rest-contract.md`.
//!
//! Every path in this file is an AIP resource name under `/api/v1/`, which is
//! why there is one route rather than one per collection: `projects/p1/
//! instances/i1` is an object because it is an even number of segments, and
//! `projects/p1/instances` is a collection because it is odd. A router with
//! ten hand-written paths would be ten chances for `nodes` to behave unlike
//! `instances`.
//!
//! This file decides nothing. It parses, calls [`crate::core::Api`], and
//! renders — including the two things the contract promises about renders:
//! `revision` is the ETag, and a list carries the revision to watch from.

use std::{collections::BTreeMap, convert::Infallible};

use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures::StreamExt;
use serde_json::{Value, json};
use velstra_cloud_model::meta::{ResourceName, Revision, Timestamp};

use crate::{
    auth::{Identity, identify},
    core::{Api, Filter, WatchEvent, created_body},
    error::{ApiError, ApiResult},
    json::{from_wire, to_wire},
    paging::{PageToken, Paging},
};

/// The header a client reads to learn where a list ended, so its watch starts
/// exactly there.
pub const REVISION_HEADER: &str = "x-velstra-revision";

pub fn router(api: Api) -> Router {
    Router::new()
        .route(
            "/api/v1/*name",
            get(read).post(create).patch(patch).delete(delete),
        )
        // Signing out and asking who you are need a token, so they belong behind
        // the layer with everything else. *Signing in* does not, and is added
        // after it below.
        .route("/api/v1/sessions/current", get(whoami).delete(sign_out))
        .route(
            "/api/v1/users/:id/password",
            axum::routing::put(set_password),
        )
        // Prometheus' text format, behind the same bearer token as everything
        // else: Prometheus scrapes with `authorization: credentials_file`
        // natively, and an unauthenticated metrics port would hand the cell's
        // machine names and capacity to whoever finds it.
        .route("/metrics", get(metrics))
        // The cell's names, for the resolver a node agent runs. Behind the
        // token like everything else here — it says who is asking, and only a
        // node's resolver and a cell operator may.
        //
        // Its own surface rather than a widening of `instances`: a node is
        // given the guests it holds and nothing else, deliberately, and
        // opening that up to serve DNS would hand every node every tenant's
        // machines with their specs. What a resolver needs is three fields —
        // a name, a subnet and an address — and every guest on a subnet can
        // already discover all three by scanning it.
        .route("/api/v1/directory", get(directory))
        .route(
            // A token for a service account. POST because it **makes** one, and
            // the answer carries it — once. Nothing can read it back.
            //
            // GET lists what an account holds, without the tokens: an operator
            // asked "how many tokens does this pipeline have, and which is the
            // one from the contractor" could not answer before, and the only
            // revocation was deleting the account.
            "/api/v1/users/:id/tokens",
            axum::routing::post(mint_service_token).get(list_service_tokens),
        )
        .route(
            // And take one out of use, by the id the list gives.
            "/api/v1/users/:id/tokens/:token",
            axum::routing::delete(revoke_service_token),
        )
        // The same two questions about a machine's own credentials, which could
        // not be asked at all: `:issueCredential` is additive by design, so an
        // operator who had issued three had three live ways into the cell and
        // no way to see or remove two of them.
        //
        // Spelled out per collection rather than with a `:kind` segment: the
        // generic object route is a catch-all, and a parameter in that
        // position collides with it.
        .route(
            "/api/v1/nodes/:id/credentials",
            axum::routing::get(list_node_credentials),
        )
        .route(
            "/api/v1/nodes/:id/credentials/:credential",
            axum::routing::delete(revoke_node_credential),
        )
        .route(
            "/api/v1/pools/:id/credentials",
            axum::routing::get(list_pool_credentials),
        )
        .route(
            "/api/v1/pools/:id/credentials/:credential",
            axum::routing::delete(revoke_pool_credential),
        )
        // The layer goes on before the console's routes, so only the API is
        // behind a token. The page itself is markup with no data in it — it
        // carries the sign-in form, and demanding a token to fetch the form
        // that asks for one is a locked door with the key inside.
        .layer(middleware::from_fn_with_state(api.clone(), authenticate))
        // Outside the token layer, so probes and sign-ins are counted too. A
        // request nobody can see is a request nobody can debug: before this
        // there was no latency, no status distribution, no request id to quote
        // back to a customer, and no way to answer "which tenant is hammering
        // us".
        .layer(middleware::from_fn_with_state(api.clone(), observe))
        // Outside the layer, and it has to be: this is the route that *issues*
        // the token every other route demands. Behind the layer it would be a
        // door whose key is on the other side of it.
        .route("/api/v1/sessions", post(sign_in))
        // Documentation, not data: the same schema the console page below
        // embeds, so it is served the way the page is — without a token.
        .route("/api/v1/openapi.json", get(openapi))
        // Probes, unauthenticated because the things that probe — a load
        // balancer, a container runtime, an orchestrator — hold no token and
        // never will. They are also the reason these routes exist at all
        // rather than the catch-all below answering 200 for every path: a
        // probe that cannot fail is a probe that never takes anything out of
        // rotation.
        //
        // `healthz` is liveness: this process is running and its executor is
        // not wedged. It touches nothing else, because a liveness probe that
        // depends on the store restarts every API in the cell when etcd
        // hiccups. `readyz` is readiness: this instance can reach the store,
        // so it can actually answer. Neither says whether the *cell* is well;
        // that is what the alerts are for.
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/", get(console))
        .route("/favicon.ico", get(favicon))
        // A deep link into the console is a path this API does not serve and
        // the page does: reloading `/instances/i1` has to return the console
        // rather than a 404, because a single-page console routes it itself.
        .route("/*path", get(console))
        .with_state(api)
}

/// The header a caller can quote back, and that every log line carries.
pub const REQUEST_ID_HEADER: &str = "x-velstra-request-id";

/// Count and time every request, and give each one an id.
///
/// The id is taken from the caller when they sent one, so a trace that starts
/// at their load balancer keeps one identity all the way through; otherwise
/// one is minted here. It goes back on the response *and* into the log line,
/// which is the whole point: a customer quoting an id gets the request.
///
/// The route is the matched path, not the URL — `/api/v1/*name` and not
/// `/api/v1/projects/acme/instances/db-1`. A per-object label would put one
/// time series per guest into the metrics and take the process down with it.
async fn observe(
    State(api): State<Api>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    let method = request.method().as_str().to_string();
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "other".into());
    let id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 64 && v.chars().all(|c| c.is_ascii_graphic()))
        .map(|v| v.to_string())
        .unwrap_or_else(new_request_id);
    let started = std::time::Instant::now();
    let mut answer = next.run(request).await;
    let micros = started.elapsed().as_micros() as u64;
    let status = answer.status().as_u16();
    api.requests().record(&method, &route, status, micros);
    if let Ok(value) = axum::http::HeaderValue::from_str(&id) {
        answer.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    // One line per request, at info. A refusal or a failure is worth a louder
    // line: those are the ones somebody goes looking for.
    let ms = micros as f64 / 1000.0;
    if status >= 500 {
        tracing::error!(request = %id, %method, %route, status, ms, "served");
    } else if status >= 400 {
        tracing::info!(request = %id, %method, %route, status, ms, "refused");
    } else {
        tracing::info!(request = %id, %method, %route, status, ms, "served");
    }
    answer
}

/// A short, unique-enough id for one request.
///
/// Not a UUID: nothing joins on these, they are quoted by a person reading a
/// log or a customer reading an error. Sixteen hex characters of the clock and
/// a counter is enough to find one line among a day of them.
fn new_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0);
    format!("{:012x}{:04x}", now & 0xffff_ffff_ffff, n & 0xffff)
}

/// Every guest in the cell that has a name and an address, for the resolvers.
///
/// A node agent's, and only a node agent's: it is the one caller that answers
/// DNS, and the answer is scoped again on the way out — a guest is told about
/// its own subnet and no other.
async fn directory(
    State(api): State<Api>,
    Extension(who): Extension<Identity>,
) -> ApiResult<Json<serde_json::Value>> {
    if crate::sessions::agent_node(&who).is_none() && !api.is_operator(&who) {
        return Err(ApiError::forbidden(
            "the directory is what a node's resolver answers from; it is not a tenant's view",
        ));
    }
    Ok(Json(serde_json::json!({ "items": api.directory().await? })))
}

/// Liveness: the process answers.
async fn healthz() -> Response {
    (StatusCode::OK, "ok\n").into_response()
}

/// Readiness: the store answers, so this instance can serve.
async fn readyz(State(api): State<Api>) -> Response {
    if api.store_answers().await {
        (StatusCode::OK, "ready\n").into_response()
    } else {
        // 503 rather than 500: this is a "come back later", and every load
        // balancer and orchestrator reads it that way.
        (StatusCode::SERVICE_UNAVAILABLE, "store unreachable\n").into_response()
    }
}

// --- signing in -----------------------------------------------------------

/// What a sign-in request carries.
#[derive(serde::Deserialize)]
struct SignInBody {
    username: String,
    password: String,
}

/// Exchange a username and password for a bearer token.
///
/// Unauthenticated by construction — it is the route that issues what every
/// other route requires. The refusal is deliberately the same sentence for every
/// cause; see `crate::sessions`.
async fn sign_in(State(api): State<Api>, Json(body): Json<SignInBody>) -> ApiResult<Response> {
    let verdict = api.identity().sign_in(&body.username, &body.password).await;
    match verdict {
        Ok(signed_in) => {
            api.record_session(
                velstra_cloud_model::audit::AuditKind::SignedIn,
                &signed_in.subject,
                "",
            )
            .await;
            Ok((StatusCode::CREATED, Json(signed_in)).into_response())
        }
        Err(e) => {
            // The failures are the half an operator needs when they are asked
            // whether an account was under attack, and the model has carried
            // the kind for them since the beginning without anybody writing
            // one. The username is recorded as submitted — it may be nobody.
            api.record_session(
                velstra_cloud_model::audit::AuditKind::Refused,
                &body.username,
                &e.message,
            )
            .await;
            Err(e)
        }
    }
}

/// End the session the caller presented.
///
/// Takes no body and names no session: a caller may only end the one they are
/// holding, because a route that ended a session by *name* would be a way to
/// sign somebody else out.
async fn sign_out(
    State(api): State<Api>,
    headers: HeaderMap,
    Extension(_who): Extension<Identity>,
) -> ApiResult<StatusCode> {
    let token = bearer(&headers).unwrap_or_default();
    api.identity().sign_out(&token).await?;
    api.record_session(
        velstra_cloud_model::audit::AuditKind::SignedOut,
        &_who.subject,
        "",
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Who the caller is, and what they may do at cell scope.
///
/// The console needs both to decide what to draw, and a console that decided
/// from its own copy of the rules would show buttons the API then refuses. The
/// answer comes from the same identity every other route is authorised against.
async fn whoami(
    State(api): State<Api>,
    headers: HeaderMap,
    Extension(who): Extension<Identity>,
) -> ApiResult<Json<serde_json::Value>> {
    let record = api.identity().user(&who.subject).await?;
    // Whether *this token* is a session, not whether a user record exists: a
    // static token or a service account can share a subject with a stored user
    // and still have no session behind it, and a sign-out for one of those ends
    // nothing. The token itself is the only honest thing to ask.
    let session = match bearer(&headers) {
        Some(token) => api.identity().session_present(&token).await,
        None => false,
    };
    Ok(Json(serde_json::json!({
        "subject": who.subject,
        "displayName": record
            .as_ref()
            .map(|u| u.spec.display_name.clone())
            .unwrap_or_default(),
        "cellAdmin": api.is_operator(&who),
        // False for a service account or a static token: there is no session
        // record behind those, so there is nothing for a sign-out to end and the
        // console should not offer one.
        "session": session,
        // The strongest rung held in each project, by id — so a console draws
        // the buttons an account can use and not every button plus a refusal.
        "projects": api.project_roles(&who).await,
        // Which cell answered. A node being joined holds an address and a
        // registration token and has read nothing else, so the region and the
        // cell are two answers it would otherwise have to be told twice — once
        // by whoever set it up and once, identically, by whoever typed them
        // into its seed. See `Api::placement`.
        "region": api.placement().region,
        "cell": api.placement().cell,
    })))
}

/// camelCase on the wire, like every other body this API serves: the field is
/// `currentPassword`, and without the rename it would silently arrive as `None`
/// — turning a self-service change that *did* prove the current password into a
/// 403, or worse, letting one through that did not.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PasswordBody {
    password: String,
    /// The caller's current password, required only for a self-service change.
    /// Absent — or wrong — is refused for that path; an operator resetting
    /// someone else's account never sends it and is never asked.
    #[serde(default)]
    current_password: Option<String>,
}

/// Set a user's password.
///
/// A cell operator may set anyone's; anybody may set their own. Nobody else may
/// set anyone's — a project administrator administers a *project*, and letting
/// them take over an account would make project membership a route to the cell.
///
/// A self-service change must prove the *current* password. Without that, a
/// stolen session is a permanent account takeover: the thief sets a new password
/// (which revokes every other session, the owner's included) and the owner
/// cannot take it back. Proving the old password is what separates the owner
/// from whoever picked up their token. An operator resetting another account is
/// the deliberate exception — the whole point of the reset is that nobody has
/// the old password.
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct TokenBody {
    /// What this token is for, in the words of whoever minted it. Not
    /// decoration: an operator looking at four tokens for one account has to
    /// know which is which before revoking one.
    #[serde(default)]
    purpose: String,
}

/// Mint a token for a service account.
///
/// A cell operator's, like creating the account: a token is authority, and an
/// account able to mint its own would be an account that cannot be contained by
/// taking one away.
/// Every token an account holds, without the tokens themselves.
async fn list_service_tokens(
    State(api): State<Api>,
    Extension(who): Extension<Identity>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    // The same rung as minting. Knowing how many keys exist for an account is
    // knowing about the way into the cell.
    if !api.is_operator(&who) {
        return Err(ApiError::forbidden(
            "an account's tokens are a way into this cell; only a cell operator may list them",
        ));
    }
    let held = api.identity().service_credentials_for(&id).await?;
    Ok(Json(serde_json::json!({
        "items": held
            .iter()
            .map(|c| serde_json::json!({
                "id": c.meta.name.id(),
                "purpose": c.spec.purpose,
                "issuedAt": c.spec.issued_at.0,
            }))
            .collect::<Vec<_>>(),
    })))
}

async fn list_node_credentials(
    api: State<Api>,
    who: Extension<Identity>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    list_agent_credentials(api, who, "nodes", id).await
}

async fn list_pool_credentials(
    api: State<Api>,
    who: Extension<Identity>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    list_agent_credentials(api, who, "pools", id).await
}

async fn revoke_node_credential(
    api: State<Api>,
    who: Extension<Identity>,
    Path((id, credential)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    revoke_agent_credential(api, who, "nodes", id, credential).await
}

async fn revoke_pool_credential(
    api: State<Api>,
    who: Extension<Identity>,
    Path((id, credential)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    revoke_agent_credential(api, who, "pools", id, credential).await
}

/// Every credential a machine holds, without the tokens.
async fn list_agent_credentials(
    State(api): State<Api>,
    Extension(who): Extension<Identity>,
    _kind: &'static str,
    id: String,
) -> ApiResult<Json<serde_json::Value>> {
    // The same rung as issuing one. Knowing how many keys exist for a machine
    // is knowing about the ways into the cell.
    if !api.is_operator(&who) {
        return Err(ApiError::forbidden(
            "a machine's credentials are a way into this cell; only a cell operator may list them",
        ));
    }
    let held = api.identity().agent_credentials_for(&id).await?;
    Ok(Json(serde_json::json!({
        "items": held
            .iter()
            .map(|c| serde_json::json!({
                "id": c.meta.name.id(),
                "purpose": c.spec.purpose,
                "issuedAt": c.spec.issued_at.0,
                "expiresAt": c.spec.expires_at.map(|t| t.0),
            }))
            .collect::<Vec<_>>(),
    })))
}

/// Take one machine credential out of use.
async fn revoke_agent_credential(
    State(api): State<Api>,
    Extension(who): Extension<Identity>,
    kind: &'static str,
    id: String,
    credential: String,
) -> ApiResult<StatusCode> {
    if !api.is_operator(&who) {
        return Err(ApiError::forbidden(
            "revoking a credential is a change to who can reach this cell; only a cell operator              may",
        ));
    }
    api.identity()
        .revoke_agent_credential(&id, &credential)
        .await?;
    api.record_change(
        &who,
        "delete",
        &velstra_cloud_model::meta::ResourceName::parse(&format!("{kind}/{id}"))
            .map_err(|e| ApiError::invalid(e.to_string()))?,
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Take one token out of use.
async fn revoke_service_token(
    State(api): State<Api>,
    Extension(who): Extension<Identity>,
    Path((id, token)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    if !api.is_operator(&who) {
        return Err(ApiError::forbidden(
            "revoking a token is a change to who can reach this cell; only a cell operator may",
        ));
    }
    api.identity()
        .revoke_service_credential(&id, &token)
        .await?;
    api.record_change(
        &who,
        "delete",
        &velstra_cloud_model::meta::ResourceName::parse(&format!("users/{id}"))
            .map_err(|e| ApiError::invalid(e.to_string()))?,
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn mint_service_token(
    State(api): State<Api>,
    Extension(who): Extension<Identity>,
    Path(id): Path<String>,
    body: Option<Json<TokenBody>>,
) -> ApiResult<Json<serde_json::Value>> {
    if !api.is_operator(&who) {
        return Err(ApiError::forbidden(
            "minting a token is a change to who can reach this cell; only a cell operator may",
        ));
    }
    let Some(account) = api.identity().user(&id).await? else {
        return Err(ApiError::new(
            crate::error::Code::NotFound,
            format!("there is no user called `{id}`"),
        ));
    };
    if !account.spec.service {
        return Err(ApiError::invalid(format!(
            "`{id}` is a person, and a person signs in with a password rather than \
             carrying a token. Set `spec.service` on an account that is a program."
        ))
        .at("id"));
    }
    let purpose = body.map(|Json(b)| b.purpose).unwrap_or_default();
    let token = api
        .identity()
        .mint_service_credential(&id, &purpose)
        .await?;
    // Once. The platform keeps a digest and cannot show it again — which is
    // said here rather than left to be discovered.
    Ok(Json(serde_json::json!({
        "token": token,
        "user": id,
        "purpose": purpose,
        "shownOnce": true,
    })))
}

async fn metrics(
    State(api): State<Api>,
    Extension(who): Extension<Identity>,
) -> ApiResult<Response> {
    let body = api.metrics(&who).await?;
    Ok((
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
        .into_response())
}

async fn set_password(
    State(api): State<Api>,
    Extension(who): Extension<Identity>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PasswordBody>,
) -> ApiResult<StatusCode> {
    // A program has no password to set. Refused rather than stored, because a
    // service account with a usable password is an account that can be signed
    // into by whoever guesses it — and nobody would be looking for that.
    if api
        .identity()
        .user(&id)
        .await?
        .is_some_and(|u| u.spec.service)
    {
        return Err(ApiError::invalid(format!(
            "`{id}` is a service account: it authenticates with a token, not a password. \
             POST /api/v1/users/{id}/tokens to mint one."
        ))
        .at("password"));
    }
    let own = who.subject == id;
    if !api.is_operator(&who) && !own {
        return Err(ApiError::forbidden(
            "only a cell operator may set another user's password",
        ));
    }
    if own {
        let current = body.current_password.as_deref().unwrap_or_default();
        if !api.identity().verify_current(&id, current).await? {
            return Err(ApiError::forbidden("the current password was not correct"));
        }
    }
    // Changing your own password ends your other sessions and not the one you
    // are sitting in. Somebody else changing it ends all of them, including any
    // the account's owner is holding — which is the point when an operator is
    // shutting a door.
    let keep = own.then(|| bearer(&headers)).flatten();
    api.identity()
        .set_password_keeping(&id, &body.password, keep.as_deref())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The bearer token on a request, if it carries one.
fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .map(|t| t.trim().to_string())
}

/// The operator's console, held for the lifetime of the process rather than
/// rebuilt per request — it is one document and it never changes.
/// `GET /api/v1/openapi.json` — the surface as OpenAPI 3.1, derived from the
/// router and the console's schema. See [`crate::openapi`].
async fn openapi() -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        crate::openapi::pretty(),
    )
        .into_response()
}

async fn console() -> Response {
    axum::response::Html(velstra_cloud_console::page_ref()).into_response()
}

/// `GET /favicon.ico` — the tab icon (see `velstra_cloud_console::FAVICON_PNG`).
async fn favicon() -> axum::response::Response {
    (
        [
            (axum::http::header::CONTENT_TYPE, "image/png"),
            (axum::http::header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        velstra_cloud_console::FAVICON_PNG,
    )
        .into_response()
}

/// Every request carries a bearer token, and this is the only place that knows
/// it is a bearer token. What kind of token it is belongs to the verifier.
///
/// With one exception, and it is not a loophole but the reason the ticket
/// exists: a **console stream** presents its ticket instead. `new WebSocket(url)`
/// takes a URL and nothing else — a browser has no way to set a header on the
/// upgrade — so a console stream that demanded one was a console no browser
/// could open. That is exactly what shipped, and it passed every test, because a
/// test client sends the header a browser cannot.
async fn authenticate(
    State(api): State<Api>,
    mut request: axum::extract::Request,
    next: Next,
) -> Result<Response, ApiError> {
    let header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let identity = match (header.as_deref(), console_ticket(&request)) {
        // The header wins where there is one, so nothing about an ordinary
        // request changes and a ticket cannot be used to *widen* a session.
        (Some(_), _) => identify(api.verifier(), header.as_deref()).await?,
        (None, Some((session, ticket))) => {
            let session = ResourceName::parse(&session)?;
            api.identity_from_console_ticket(&session, &ticket).await?
        }
        (None, None) => identify(api.verifier(), None).await?,
    };
    request.extensions_mut().insert(identity);
    Ok(next.run(request).await)
}

/// The session and ticket a console stream carries, and only a console stream.
///
/// Narrow on purpose: a GET, the `:consoleStream` verb, both values present.
/// Anything else takes the ordinary door.
fn console_ticket(request: &axum::extract::Request) -> Option<(String, String)> {
    if request.method() != axum::http::Method::GET {
        return None;
    }
    let uri = request.uri();
    if !uri.path().ends_with(":consoleStream") {
        return None;
    }
    let query: BTreeMap<String, String> =
        serde_urlencoded::from_str(uri.query().unwrap_or_default()).ok()?;
    Some((query.get("session")?.clone(), query.get("ticket")?.clone()))
}

/// What a path addresses. The distinction is the segment count, per AIP: an
/// even number of `collection/id` pairs is one object, an odd number ends in a
/// collection.
enum Target {
    Collection {
        parent: String,
        kind: String,
    },
    Object(ResourceName),
    Verb {
        name: ResourceName,
        verb: String,
    },
    /// A custom method on a whole collection: `nodes:explainCpu`.
    ///
    /// Some answers are about a set rather than a member. Which nodes can
    /// exchange guests is a property of the fleet, and hanging it off an
    /// arbitrary node would make the answer look like that node's while being
    /// the same for every one of them.
    CollectionVerb {
        parent: String,
        kind: String,
        verb: String,
    },
}

fn target(path: &str) -> ApiResult<Target> {
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return Err(ApiError::invalid(
            "a path addresses a collection or an object",
        ));
    }
    // `…/instances/i1:explainPlacement` — the custom-method form AIP-136 uses,
    // and the only thing in this API that is not a plain name.
    if let Some((name, verb)) = path.rsplit_once(':') {
        // An object name if it parses as one, a collection otherwise. Tried in
        // that order because an object name is the common case and the more
        // specific shape; a collection path has an odd number of segments and
        // would never parse as a name.
        if let Ok(name) = ResourceName::parse(name) {
            return Ok(Target::Verb {
                name,
                verb: verb.to_string(),
            });
        }
        let segments: Vec<&str> = name.split('/').collect();
        let (kind, parent) = segments.split_last().expect("split never yields nothing");
        return Ok(Target::CollectionVerb {
            parent: parent.join("/"),
            kind: kind.to_string(),
            verb: verb.to_string(),
        });
    }
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() % 2 == 0 {
        return Ok(Target::Object(ResourceName::parse(path)?));
    }
    let (kind, parent) = segments.split_last().expect("checked non-empty");
    Ok(Target::Collection {
        parent: parent.join("/"),
        kind: kind.to_string(),
    })
}

/// Every query parameter this surface reads.
///
/// **A name nobody has is refused, exactly as a field nobody has is.** The
/// contract argues the case for values — "a `pageSize` of `twenty` silently
/// answering with the whole cell is the shape where a load test passes and
/// production does not" — and the same failure is one keystroke away through
/// the *name*: `?label=env=prod` returned the collection unfiltered,
/// `?pagesize=20` returned the default page, `?Since=1h` returned everything,
/// and `?watch=1` returned a list where a stream was asked for. Each of those
/// is a client that looks like it is working.
///
/// Not every parameter is legal on every route; this is the union, and the
/// route-specific readers still decide what they do with one. That is the
/// cheap half of the check and it catches every case above.
const KNOWN_QUERY: &[&str] = &[
    "fields",
    "fromRevision",
    "labels",
    "mode",
    "month",
    "node",
    "orderBy",
    "pageSize",
    "pageToken",
    "pool",
    "session",
    "since",
    "target",
    "ticket",
    "until",
    "watch",
];

/// Whether this read is a subscription.
///
/// Spelled out rather than `== "true"` for the reason `paging_from` gives
/// about a page size: a value this surface cannot read is refused, never
/// quietly taken for the default.
fn watching(query: &BTreeMap<String, String>) -> ApiResult<bool> {
    match query.get("watch").map(String::as_str) {
        None | Some("") => Ok(false),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(other) => Err(ApiError::invalid(format!(
            "`watch` is `true` or `false`, and `{other}` is neither. Asked as anything else \
             this answered with a page, which is a plausible answer to a question nobody asked."
        ))
        .at("watch")),
    }
}

/// Refuse a query parameter nobody has, by name.
fn check_query(query: &BTreeMap<String, String>) -> ApiResult<()> {
    for key in query.keys() {
        if KNOWN_QUERY.contains(&key.as_str()) {
            continue;
        }
        // An empty value is somebody echoing a URL back, or a client that
        // builds a query string from a struct with a blank field. Nothing was
        // meant by it, so nothing is lost by ignoring it — the same rule the
        // unknown-*field* guard follows for the same reason.
        if query.get(key).is_some_and(|v| v.is_empty()) {
            continue;
        }
        let near = KNOWN_QUERY
            .iter()
            .find(|k| k.eq_ignore_ascii_case(key))
            .map(|k| format!(" Did you mean `{k}`?"))
            .unwrap_or_default();
        return Err(ApiError::invalid(format!(
            "there is no query parameter called `{key}`; it would have been ignored, and the \
             answer would have looked like one to the question you asked.{near}"
        ))
        .at(key.clone()));
    }
    Ok(())
}

/// Read `?pageSize=` and `?pageToken=` off a query string.
///
/// A page size that is not a number is refused rather than ignored: a client
/// sending `pageSize=twenty` and silently receiving the whole cell is the shape
/// where a load test passes and production does not.
fn paging_from(query: &BTreeMap<String, String>) -> ApiResult<Paging> {
    let size = match query.get("pageSize") {
        // A caller who says nothing gets a page, not the collection. The
        // unbounded default was the one shape in which a single `curl` — or a
        // generated client, or a Terraform provider — pulled a cell's whole
        // audit log into one response and one allocation. Internal callers
        // that genuinely need everything say so with `Paging::unpaged()`;
        // over HTTP there is a `nextPageToken` and AIP-158 says to follow it.
        None => Some(crate::paging::DEFAULT_PAGE_SIZE),
        Some(raw) => Some(raw.parse::<usize>().map_err(|_| {
            ApiError::invalid(format!("pageSize must be a whole number, and was {raw:?}"))
                .at("pageSize")
        })?),
    };
    let token = match query.get("pageToken") {
        None => None,
        Some(raw) => Some(PageToken::decode(raw)?),
    };
    Ok(Paging { size, token })
}

/// Read `?since=` / `?until=` off a query string.
///
/// Two spellings, because the two questions people actually ask have different
/// shapes: an absolute moment (`since=1788960000000`, milliseconds since the
/// epoch, the same number every timestamp in this API is) and a span back from
/// now (`since=1h`, `since=30m`, `since=7d`). "The refusals of the last hour"
/// is the second one, and making somebody compute an epoch to ask it is how a
/// filter ends up unused.
///
/// A value that parses as neither is refused rather than ignored: a client
/// sending `since=yesterday` and silently receiving the whole audit is the
/// shape where a page loads in development and times out in production.
fn moment_from(
    query: &BTreeMap<String, String>,
    key: &str,
    now: Timestamp,
) -> ApiResult<Option<Timestamp>> {
    let Some(raw) = query.get(key) else {
        return Ok(None);
    };
    if let Ok(ms) = raw.parse::<u64>() {
        return Ok(Some(Timestamp(ms)));
    }
    let (count, unit) = raw.split_at(raw.len().saturating_sub(1));
    let seconds: u64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => {
            return Err(ApiError::invalid(format!(
                "{key} is milliseconds since the epoch, or a span back from now like 30m, 6h or \
                 7d, and was {raw:?}"
            ))
            .at(key));
        }
    };
    let count: u64 = count.parse().map_err(|_| {
        ApiError::invalid(format!(
            "{key} is milliseconds since the epoch, or a span back from now like 30m, 6h or 7d, \
             and was {raw:?}"
        ))
        .at(key)
    })?;
    Ok(Some(Timestamp(now.0.saturating_sub(
        count.saturating_mul(seconds).saturating_mul(1_000),
    ))))
}

/// How a listing is to be ordered, read off `?orderBy=`.
///
/// AIP-132's spelling: a field name, optionally followed by ` desc`. Two
/// fields and no more — `name` and `createdAt` — because those are the two
/// orders a collection has that mean the same thing in every collection.
/// Sorting on a status field would order half a list by a value the other half
/// does not carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Order {
    by: OrderKey,
    descending: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OrderKey {
    Name,
    CreatedAt,
}

/// Read `?orderBy=` off a query string.
///
/// An ordered listing is a **top-N, not a pageable ordered stream**, and that
/// is the whole design. A page is a slice of the store's own key order and the
/// token is a key, so sorting the hundred rows of one page by creation time
/// would give a list that is ordered inside each page and unordered across
/// them — which looks sorted and is not, and is the kind of wrong that is
/// found months later.
///
/// So an ordered request reads everything the filter admits, sorts it, and
/// returns the first `pageSize` of the sorted order with **no** page token.
/// "The fifty newest refusals" is the question people have, and it now has an
/// answer that is actually the fifty newest. Continuing past that is refused
/// rather than answered wrongly: `pageToken` and `orderBy` together are an
/// error.
fn order_from(query: &BTreeMap<String, String>) -> ApiResult<Option<Order>> {
    let Some(raw) = query.get("orderBy") else {
        return Ok(None);
    };
    let raw = raw.trim();
    let (field, descending) = match raw.rsplit_once(char::is_whitespace) {
        Some((field, "desc")) => (field.trim(), true),
        Some((field, "asc")) => (field.trim(), false),
        Some(_) => (raw, false),
        None => (raw, false),
    };
    let by = match field {
        "name" | "meta.name" => OrderKey::Name,
        "createdAt" | "meta.createdAt" => OrderKey::CreatedAt,
        other => {
            return Err(ApiError::invalid(format!(
                "orderBy is name or createdAt, either alone or followed by desc, and was \
                 {other:?}. Those are the two orders every collection has; a status field is \
                 not one every object carries."
            ))
            .at("orderBy"));
        }
    };
    Ok(Some(Order { by, descending }))
}

/// Order a page of wire documents in place.
fn sorted(items: &mut [Value], order: Order) {
    items.sort_by(|a, b| {
        let ord = match order.by {
            OrderKey::Name => {
                let name = |v: &Value| {
                    v.get("meta")
                        .and_then(|m| m.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                };
                name(a).cmp(&name(b))
            }
            OrderKey::CreatedAt => {
                let at = |v: &Value| {
                    v.get("meta")
                        .and_then(|m| m.get("createdAt"))
                        .and_then(Value::as_u64)
                        .unwrap_or_default()
                };
                at(a).cmp(&at(b))
            }
        };
        if order.descending { ord.reverse() } else { ord }
    });
}

/// Keep only the paths named in `?fields=`, plus `meta.name`.
///
/// AIP-157's read mask, spelled as a comma-separated list of dotted paths:
/// `?fields=meta.name,status.conditions`. A board that shows four columns of a
/// guest was pulling the whole document — cloud-init, every condition, every
/// address — for every row.
///
/// `meta.name` is always kept, whether it was asked for or not: a document
/// without its name is a row nothing can be done with, and a caller who
/// forgets it gets a list they cannot use rather than an error they can read.
fn only_fields(document: &Value, fields: &[Vec<String>]) -> Value {
    let mut out = Value::Object(serde_json::Map::new());
    for path in fields {
        if let Some(found) = pick(document, path) {
            graft(&mut out, path, found);
        }
    }
    if let Some(name) = document.get("meta").and_then(|m| m.get("name")) {
        graft(&mut out, &["meta".into(), "name".into()], name.clone());
    }
    out
}

fn pick(document: &Value, path: &[String]) -> Option<Value> {
    let mut at = document;
    for step in path {
        at = at.get(step)?;
    }
    Some(at.clone())
}

fn graft(into: &mut Value, path: &[String], leaf: Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut at = into;
    for step in parents {
        if !at.get(step).map(Value::is_object).unwrap_or(false) {
            at[step] = Value::Object(serde_json::Map::new());
        }
        at = &mut at[step];
    }
    at[last] = leaf;
}

/// Read `?fields=` off a query string, as dotted paths.
fn fields_from(query: &BTreeMap<String, String>) -> Option<Vec<Vec<String>>> {
    let raw = query.get("fields")?;
    let paths: Vec<Vec<String>> = raw
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| p.split('.').map(str::to_string).collect())
        .collect();
    (!paths.is_empty()).then_some(paths)
}

// ---- handlers -------------------------------------------------------------

async fn read(
    State(api): State<Api>,
    Path(path): Path<String>,
    Query(query): Query<BTreeMap<String, String>>,
    Extension(who): Extension<Identity>,
    upgrade: Option<axum::extract::ws::WebSocketUpgrade>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let _ = headers;
    check_query(&query)?;
    match target(&path)? {
        Target::Object(name) => {
            let mut document = api.get(&name, &who).await?;
            // `?fields=` narrows one object as it narrows a list. The parameter
            // was read in the collection branch and nowhere else, so a masked
            // single read came back whole, with no error — a client asking for
            // two fields of a guest was handed the guest, which is the shape
            // where a mobile client's bill is a surprise.
            if let Some(fields) = fields_from(&query) {
                document = only_fields(&document, &fields);
            }
            Ok(object(StatusCode::OK, document))
        }
        // The stream itself. A GET because it is an upgrade, and it carries a
        // ticket rather than resting on the session cookie: the ticket is what
        // the node checks, and a stream that opened on a cookie alone would be
        // one another page could open in the background.
        Target::Verb { name, verb } if verb == "consoleStream" => {
            let (Some(session), Some(ticket)) =
                (query.get("session").cloned(), query.get("ticket").cloned())
            else {
                return Err(ApiError::invalid(
                    "a console stream carries the session and the ticket it was given",
                ));
            };
            // Two credentials, two questions.
            //
            // A ticket is minted against one guest, so the question is whether
            // it is *this* guest — exact, and stronger than any role check: a
            // ticket that leaked out of a URL opens the machine it was made for
            // and nothing else.
            //
            // A bearer token is the person, so the question is the ordinary one
            // every reader answers. `get` is that read, and its answer is thrown
            // away.
            match crate::sessions::console_instance(&who) {
                Some(instance) if instance == name.to_string() => {}
                Some(_) => {
                    return Err(ApiError::new(
                        crate::error::Code::PermissionDenied,
                        "that ticket was minted for another guest",
                    ));
                }
                None => {
                    api.get(&name, &who).await?;
                }
            }
            // Only once the caller is allowed does the shape of the request
            // matter. A refusal that leads with "you did not ask to upgrade"
            // tells somebody presenting a ticket for another machine that their
            // request was nearly right.
            let Some(upgrade) = upgrade else {
                return Err(ApiError::invalid(
                    "a console stream is a websocket; this request did not ask to upgrade",
                ));
            };
            let endpoint = api
                .console_endpoint_for(&ResourceName::parse(&session)?)
                .await?;
            // `wss://` when the node says it serves TLS on this port, and
            // `ws://` when it says it does not. Asked of the node rather than
            // configured here: only the machine knows what it actually bound,
            // and an API that assumed TLS would fail every attach on a node
            // that has no certificate — while one that assumed plaintext would
            // throw away the protection on a node that does.
            let private = api
                .console_is_private(&ResourceName::parse(&session)?)
                .await;
            let scheme = if private { "wss" } else { "ws" };
            let url = format!("{scheme}://{endpoint}/console?session={session}&ticket={ticket}");
            if !private {
                // Once per attach, and deliberately at `warn`: what crosses
                // this connection is a serial line, and an operator typing a
                // root password into it is the ordinary use.
                tracing::warn!(
                    %endpoint,
                    "this console stream is not encrypted — give the node \
                     --console-tls-cert and --console-tls-key to make it private"
                );
            }
            let connector = crate::console_proxy::trust(api.console_ca());
            Ok(upgrade.on_upgrade(move |socket| async move {
                if let Err(e) = crate::console_proxy::relay(socket, url, connector).await {
                    tracing::warn!(error = %e, "a console stream could not be relayed");
                }
            }))
        }
        Target::Verb { name, verb } if verb == "explainPlacement" => {
            Ok(Json(api.explain_placement(&name, &who).await?).into_response())
        }
        // `?mode=Reboot` — the answer is only true of one mode, and the two
        // refuse different things: a cold move crosses processors a live one
        // cannot. A spelling that is not a mode is refused rather than quietly
        // read as `Live`, which would answer a question nobody asked.
        Target::Verb { name, verb } if verb == "explainMigration" => {
            let mode = match query.get("mode") {
                None => velstra_cloud_model::migration::MigrationMode::default(),
                Some(raw) => serde_json::from_value(serde_json::Value::String(raw.clone()))
                    .map_err(|_| {
                        ApiError::invalid(format!(
                            "mode is Live, PostCopy or Reboot, and was {raw:?}"
                        ))
                        .at("mode")
                    })?,
            };
            Ok(Json(api.explain_migration(&name, mode, &who).await?).into_response())
        }
        Target::Verb { name, verb } if verb == "explainReach" => {
            Ok(Json(api.explain_reach(&name, &who).await?).into_response())
        }
        // `?month=2026-08`; nothing means the current one.
        Target::Verb { name, verb } if verb == "explainUsage" => {
            let month = query.get("month").map(String::as_str);
            return Ok(Json(api.explain_usage(&name, month, &who).await?).into_response());
        }
        Target::Verb { name, verb } if verb == "explainQuota" => {
            Ok(Json(api.explain_quota(&name, &who).await?).into_response())
        }
        Target::Verb { name, verb } if verb == "explainMaintenance" => {
            Ok(Json(api.explain_maintenance(&name, &who).await?).into_response())
        }
        Target::Verb { name, verb } if verb == "explainRecovery" => {
            Ok(Json(api.explain_recovery(&name, &who).await?).into_response())
        }
        // `parent` must be empty: nodes are a cell's, not a project's, and
        // `projects/p1/nodes:explainCpu` reading as the whole cell's report
        // would be an answer about somebody else's machines.
        Target::CollectionVerb { parent, kind, verb }
            if parent.is_empty() && kind == "nodes" && verb == "explainCpu" =>
        {
            Ok(Json(api.explain_cpu(&who).await?).into_response())
        }
        Target::CollectionVerb { parent, kind, verb }
            if parent.is_empty() && kind == "nodes" && verb == "explainCapacity" =>
        {
            Ok(Json(api.explain_capacity(&who).await?).into_response())
        }
        Target::CollectionVerb { parent, kind, verb } => {
            Err(ApiError::invalid(if parent.is_empty() {
                format!("{kind} has no method {verb:?}")
            } else {
                format!("{parent}/{kind} has no method {verb:?}")
            }))
        }
        Target::Verb { verb, .. } => Err(ApiError::invalid(format!(
            "there is no method called {verb}"
        ))),
        Target::Collection { parent, kind } => {
            // `?node=` and `?pool=` are what keep a cell's size off an agent's
            // wire. Anything else asking for a collection is asking about the
            // cell on purpose. Both at once is refused rather than intersected:
            // an agent is one or the other, and a caller that sent both has not
            // decided what it wants.
            let filter = match (query.get("node"), query.get("pool")) {
                (Some(_), Some(_)) => {
                    return Err(ApiError::invalid(
                        "node and pool name two different kinds of agent; ask as one of them",
                    ));
                }
                (Some(node), None) => Filter::for_node(node),
                (None, Some(pool)) => Filter::for_pool(pool),
                (None, None) => Filter::none(),
            };
            // `?labels=env=prod,tier=web`. Every term must match; an "or"
            // would need precedence rules, and a filter whose meaning depends
            // on precedence is one people get wrong silently.
            //
            // A selector that matches nothing is an empty list, not an error:
            // "no guests are tagged that" is an answer, and refusing it would
            // make a typo look like a broken endpoint.
            // `?target=projects/p1/instances/i1` — the records *about* one
            // object. Only `operations` and `audit` carry a target, and asking
            // any other collection for one is a caller who has misunderstood
            // rather than one who should get the whole cell back.
            let filter = match query.get("target") {
                Some(_) if kind != "operations" && kind != "audit" => {
                    return Err(ApiError::invalid(format!(
                        "{kind} are not records about another object; only operations and audit                          carry a target"
                    )));
                }
                Some(target) => Filter {
                    target: Some(target.clone()),
                    ..filter
                },
                None => filter,
            };
            let filter = match query.get("labels") {
                Some(text) => Filter {
                    labels: velstra_cloud_model::meta::parse_selector(text),
                    ..filter
                },
                None => filter,
            };
            // `?since=1h&until=10m` — the slice of a collection somebody
            // actually wants. On `meta.createdAt`, so it means the same thing
            // in every collection.
            let now = Timestamp::now();
            let filter = Filter {
                since: moment_from(&query, "since", now)?,
                until: moment_from(&query, "until", now)?,
                ..filter
            };
            // `true` or `false`, and nothing else. `?watch=1` used to return a
            // *list* — a plausible answer to a question nobody asked, and the
            // one shape where a client is written against a stream, tested
            // against a page, and works until the day the collection is big.
            if watching(&query)? {
                return watch(api, &parent, &kind, query.get("fromRevision"), filter, &who).await;
            }
            // `?pageSize=` / `?pageToken=`, spelled the way AIP-158 spells them.
            // A caller who asks for neither gets the whole collection, which is
            // what every existing client expects and what a controller wants;
            // a caller who asks for either gets a page and a token.
            let paging = paging_from(&query)?;
            // `?orderBy=` and paging are refused together rather than
            // combined: see `order_from`. Checked before the read, so a caller
            // who asked for something impossible does not pay for a listing
            // first.
            let order = order_from(&query)?;
            if order.is_some() && paging.token.is_some() {
                return Err(ApiError::invalid(
                    "orderBy and pageToken cannot both be asked for. An ordered listing is the \
                     first page of the sorted order and nothing after it: a page token is a key \
                     in the store's own order, and following one would give a list that is \
                     ordered inside each page and unordered across them. Ask for a larger \
                     pageSize, or page unordered and sort at the client.",
                )
                .at("orderBy"));
            }
            // Ordered: read everything the filter admits, sort it, and hand
            // back the first page of the sorted order. The read is bounded by
            // the filter and not by the page, which is what makes "the fifty
            // newest" actually the fifty newest.
            let read = if order.is_some() {
                crate::paging::Paging::unpaged()
            } else {
                paging.clone()
            };
            let listing = api
                .list_page_for(&parent, &kind, &filter, &read, &who)
                .await?;
            let truncated_by_order =
                order.is_some() && listing.items.len() > paging.resolved_size();
            let mut items: Vec<Value> = listing.items.into_iter().map(to_wire).collect();
            if let Some(order) = order {
                sorted(&mut items, order);
                items.truncate(paging.resolved_size());
            }
            // Last, on the wire document, so a caller can name a computed
            // field — `status.addresses` is not in the store and is exactly
            // the kind of thing a board asks for.
            if let Some(fields) = fields_from(&query) {
                items = items.iter().map(|d| only_fields(d, &fields)).collect();
            }
            let mut body = json!({
                "items": items,
                "revision": listing.revision.to_string(),
            });
            // Present only when there is more. An always-present field that is
            // sometimes empty invites `if (body.nextPageToken !== undefined)`,
            // which loops forever.
            //
            // Never for an ordered listing: the token is a key in the store's
            // own order, and handing one back would invite exactly the walk
            // that produces a list ordered inside each page and unordered
            // across them.
            if order.is_none()
                && let Some(token) = listing.next_page_token
            {
                body["nextPageToken"] = json!(token);
            }
            // …but say that the answer was cut, so a caller asking for the
            // fifty newest of two hundred is not left thinking there were
            // fifty.
            if truncated_by_order {
                body["truncated"] = json!(true);
            }
            Ok((
                StatusCode::OK,
                [(REVISION_HEADER, listing.revision.to_string())],
                Json(body),
            )
                .into_response())
        }
    }
}

async fn create(
    State(api): State<Api>,
    Extension(identity): Extension<Identity>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let (parent, kind) = match target(&path)? {
        Target::Collection { parent, kind } => (parent, kind),
        // A node agent's status write, AIP-136's custom-method shape: a POST to
        // `…/instances/i1:reportStatus`. It is here rather than on PATCH because
        // PATCH is the client's spec-only surface by contract, and a status write
        // is a different caller (a node) doing a different thing.
        // A console is asked for with POST because it *makes* something — a
        // session object, spent once. A GET that minted a credential would be
        // one a browser could be made to issue from another page.
        // `{"kind": "Vnc"}` in the body asks for the display instead of the
        // serial line; nothing, `{}` or `{"kind": "Serial"}` is the serial
        // console it always was. A kind that is neither is refused rather than
        // read as the default — a ticket for a line nobody asked about.
        Target::Verb { name, verb } if verb == "console" => {
            let kind = match document(&body)?.get("kind") {
                None | Some(serde_json::Value::Null) => {
                    velstra_cloud_model::console::ConsoleKind::Serial
                }
                Some(raw) => serde_json::from_value(raw.clone()).map_err(|_| {
                    ApiError::invalid(format!("kind is Serial or Vnc, and was {raw}")).at("kind")
                })?,
            };
            let opened = api.open_console(&name, kind, &identity).await?;
            return Ok(object(StatusCode::OK, opened));
        }
        // POST, and never GET: it mints a credential, and a GET that did that
        // is one a browser can be made to issue from somebody else's page.
        Target::Verb { name, verb } if verb == "issueCredential" => {
            // The body is optional: `{}` is the ordinary case, and a caller
            // may say `purpose` and `expiresAt`.
            let ask = document(&body).unwrap_or_else(|_| serde_json::json!({}));
            let issued = api.issue_credential(&name, &ask, &identity).await?;
            return Ok((StatusCode::OK, Json(issued)).into_response());
        }
        Target::Verb { name, verb } if verb == "reportStatus" => {
            let reported = api
                .report_status(&name, &document(&body)?, if_match(&headers)?, &identity)
                .await?;
            return Ok(object(StatusCode::OK, reported));
        }
        Target::CollectionVerb { verb, .. } => {
            return Err(ApiError::invalid(format!(
                "{verb:?} is not something that can be posted to a collection"
            )));
        }
        Target::Verb { verb, .. } => {
            return Err(ApiError::invalid(format!(
                "there is no method called {verb} on a create"
            )));
        }
        Target::Object(_) => {
            return Err(ApiError::invalid(
                "a create posts to a collection; the id goes in the body, not in the path",
            ));
        }
    };
    // A key makes the retry safe; without one this is the create it always was.
    // Read from the header rather than the body on purpose: the body is
    // fingerprinted, and a field that changed between two attempts of the same
    // create would make every retry look like a different request.
    let key = headers
        .get(crate::core::IDEMPOTENCY_HEADER)
        .and_then(|v| v.to_str().ok());
    let (created, replayed) = api
        .create_with_key(&parent, &kind, &document(&body)?, &identity, key)
        .await?;
    // 202, because the object exists but has not converged. The operation is
    // what a client waits on, and it is a resource it can come back to rather
    // than a connection it has to hold. A replay answers the same way, with a
    // header saying so — a client that ignores it is still correct.
    let mut answer = (StatusCode::ACCEPTED, Json(created_body(&created))).into_response();
    if replayed {
        answer.headers_mut().insert(
            crate::core::REPLAYED_HEADER,
            axum::http::HeaderValue::from_static("true"),
        );
    }
    Ok(answer)
}

async fn patch(
    State(api): State<Api>,
    Path(path): Path<String>,
    Extension(who): Extension<Identity>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let Target::Object(name) = target(&path)? else {
        return Err(ApiError::invalid(
            "a change addresses one object, not a collection",
        ));
    };
    let updated = api
        .patch(&name, &document(&body)?, if_match(&headers)?, &who)
        .await?;
    Ok(with_operation(
        StatusCode::OK,
        updated.resource,
        updated.operation,
    ))
}

async fn delete(
    State(api): State<Api>,
    Path(path): Path<String>,
    Extension(who): Extension<Identity>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let Target::Object(name) = target(&path)? else {
        return Err(ApiError::invalid(
            "a delete addresses one object, not a collection",
        ));
    };
    let deleted = api.delete(&name, if_match(&headers)?, &who).await?;
    // 202 whether or not anything still holds the object: the client asked, the
    // deletion is recorded, and "gone" is something it learns by getting a 404
    // rather than by reading a different success code.
    Ok(with_operation(
        StatusCode::ACCEPTED,
        deleted.resource,
        deleted.operation,
    ))
}

/// A watch is a read, and a read is authorised. It used not to be: this took no
/// identity at all, so `?watch=true` under another tenant's project streamed
/// their objects to anybody holding an accepted token. See [`Api::watch_for`].
async fn watch(
    api: Api,
    parent: &str,
    kind: &str,
    from: Option<&String>,
    filter: Filter,
    who: &Identity,
) -> ApiResult<Response> {
    let from = from
        .map(|r| {
            r.parse::<u64>().map(Revision).map_err(|_| {
                ApiError::invalid("fromRevision is the revision a list reported").at("fromRevision")
            })
        })
        .transpose()?;
    let stream = api
        .watch_for(parent, kind, from, filter, who)
        .await?
        .map(|event| {
            let data = match event {
                WatchEvent::Put(resource) => {
                    json!({ "type": "PUT", "resource": to_wire(resource) })
                }
                WatchEvent::Delete { name, revision } => {
                    json!({ "type": "DELETE", "name": name, "revision": revision.to_string() })
                }
            };
            Ok::<Event, Infallible>(Event::default().data(data.to_string()))
        });
    // The keep-alive is what stops an idle watch from being reaped by whatever
    // proxy sits in front of this, and it costs one colon every fifteen seconds.
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}

// ---- rendering ------------------------------------------------------------

/// One object, with its revision as the ETag — which is the whole of the
/// contract's "send it back as `If-Match`".
fn object(status: StatusCode, document: Value) -> Response {
    with_operation(status, document, None)
}

/// The name of the header that carries the operation minted for a change.
///
/// A create answers `202` with the operation in the body, because there is no
/// object yet to be the body. A change and a delete already have a body — the
/// object — so the handle goes in a header rather than reshaping a response
/// every client already reads. It is additive: a client that does not look
/// sees exactly what it saw before.
pub const OPERATION_HEADER: &str = "velstra-operation";

fn with_operation(status: StatusCode, document: Value, operation: Option<String>) -> Response {
    let revision = document["meta"]["revision"]
        .as_u64()
        .unwrap_or_default()
        .to_string();
    let mut response = (
        status,
        [(header::ETAG, format!("\"{revision}\""))],
        Json(to_wire(document)),
    )
        .into_response();
    if let Some(operation) = operation
        && let Ok(value) = operation.parse()
    {
        response.headers_mut().insert(OPERATION_HEADER, value);
    }
    response
}

fn document(body: &Bytes) -> ApiResult<Value> {
    if body.is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| ApiError::invalid(format!("the body is not JSON: {e}")))?;
    if !value.is_object() {
        return Err(ApiError::invalid(
            "the body is an object with spec, and optionally meta.labels and id",
        ));
    }
    Ok(from_wire(value))
}

/// `If-Match: "412"`, `If-Match: 412` and `If-Match: W/"412"` all mean the same
/// thing. Absent means last-writer-wins, and the client said so by leaving it
/// out.
fn if_match(headers: &HeaderMap) -> ApiResult<Option<Revision>> {
    let Some(value) = headers.get(header::IF_MATCH) else {
        return Ok(None);
    };
    let raw = value
        .to_str()
        .map_err(|_| ApiError::invalid("If-Match is not readable text").at("If-Match"))?;
    let trimmed = raw.trim().trim_start_matches("W/").trim_matches('"');
    trimmed
        .parse::<u64>()
        .map(|r| Some(Revision(r)))
        .map_err(|_| {
            ApiError::invalid(format!(
                "If-Match is a revision this API handed out, not {raw:?}"
            ))
            .at("If-Match")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_an_object_or_a_collection_by_its_shape() {
        assert!(matches!(
            target("projects/p1/instances/i1").unwrap(),
            Target::Object(_)
        ));
        assert!(matches!(
            target("projects/p1/instances").unwrap(),
            Target::Collection { .. }
        ));
        // A root collection has no parent, and must not be mistaken for an
        // object called `nodes`.
        match target("nodes").unwrap() {
            Target::Collection { parent, kind } => {
                assert_eq!(parent, "");
                assert_eq!(kind, "nodes");
            }
            _ => panic!("a root collection was read as an object"),
        }
    }

    #[test]
    fn a_custom_method_is_split_off_the_name() {
        match target("projects/p1/instances/i1:explainPlacement").unwrap() {
            Target::Verb { name, verb } => {
                assert_eq!(name.to_string(), "projects/p1/instances/i1");
                assert_eq!(verb, "explainPlacement");
            }
            _ => panic!("the verb was read as part of the id"),
        }
    }

    #[test]
    fn an_etag_is_accepted_in_every_shape_a_client_might_send_it() {
        let etag = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::IF_MATCH, v.parse().unwrap());
            if_match(&h).unwrap()
        };
        assert_eq!(etag("412"), Some(Revision(412)));
        assert_eq!(etag("\"412\""), Some(Revision(412)));
        assert_eq!(etag("W/\"412\""), Some(Revision(412)));
        assert_eq!(if_match(&HeaderMap::new()).unwrap(), None);
    }
    fn q(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_span_back_from_now_is_a_moment() {
        let now = Timestamp(1_000_000_000);
        assert_eq!(
            moment_from(&q(&[("since", "1h")]), "since", now).unwrap(),
            Some(Timestamp(1_000_000_000 - 3_600_000))
        );
        assert_eq!(
            moment_from(&q(&[("since", "30m")]), "since", now).unwrap(),
            Some(Timestamp(1_000_000_000 - 1_800_000))
        );
        assert_eq!(
            moment_from(&q(&[("since", "7d")]), "since", now).unwrap(),
            Some(Timestamp(1_000_000_000 - 604_800_000))
        );
    }

    #[test]
    fn an_absolute_moment_is_taken_as_it_is() {
        assert_eq!(
            moment_from(&q(&[("until", "412")]), "until", Timestamp(9)).unwrap(),
            Some(Timestamp(412))
        );
        assert_eq!(moment_from(&q(&[]), "since", Timestamp(9)).unwrap(), None);
    }

    /// A span nobody can parse is refused, not ignored: silently returning the
    /// whole audit is the shape where a page loads in development and times
    /// out in production.
    #[test]
    fn a_span_in_words_is_refused_with_the_spellings_that_work() {
        let e = moment_from(&q(&[("since", "yesterday")]), "since", Timestamp(9))
            .expect_err("this is not a span");
        let said = format!("{e:?}");
        assert!(said.contains("30m"), "{said}");
    }

    #[test]
    fn an_order_is_a_field_and_a_direction() {
        assert_eq!(
            order_from(&q(&[("orderBy", "createdAt desc")])).unwrap(),
            Some(Order {
                by: OrderKey::CreatedAt,
                descending: true
            })
        );
        assert_eq!(
            order_from(&q(&[("orderBy", "name")])).unwrap(),
            Some(Order {
                by: OrderKey::Name,
                descending: false
            })
        );
        assert_eq!(order_from(&q(&[])).unwrap(), None);
    }

    /// Two fields and no more, and the refusal says which — a status field is
    /// not one every object in a collection carries.
    #[test]
    fn an_order_on_something_else_is_refused_by_name() {
        let e = order_from(&q(&[("orderBy", "status.phase")])).expect_err("not an order");
        let said = format!("{e:?}");
        assert!(said.contains("createdAt"), "{said}");
    }

    #[test]
    fn ordering_puts_the_newest_first_when_asked_to() {
        let mut items = vec![
            json!({"meta": {"name": "b", "createdAt": 200}}),
            json!({"meta": {"name": "a", "createdAt": 300}}),
            json!({"meta": {"name": "c", "createdAt": 100}}),
        ];
        sorted(
            &mut items,
            Order {
                by: OrderKey::CreatedAt,
                descending: true,
            },
        );
        let names: Vec<&str> = items
            .iter()
            .map(|i| i["meta"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["a", "b", "c"]);
        sorted(
            &mut items,
            Order {
                by: OrderKey::Name,
                descending: false,
            },
        );
        let names: Vec<&str> = items
            .iter()
            .map(|i| i["meta"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn a_read_mask_keeps_what_was_asked_for_and_the_name() {
        let document = json!({
            "meta": {"name": "projects/p1/instances/i1", "labels": {"env": "prod"}},
            "spec": {"cloudInit": "#cloud-config\nlots and lots"},
            "status": {"phase": "Running", "addresses": ["10.0.0.2"]},
        });
        let fields = fields_from(&q(&[("fields", "status.phase, meta.labels")])).unwrap();
        let trimmed = only_fields(&document, &fields);
        assert_eq!(trimmed["status"]["phase"], "Running");
        assert_eq!(trimmed["meta"]["labels"]["env"], "prod");
        // The name comes back whether it was named or not: a row without it is
        // one nothing can be done with.
        assert_eq!(trimmed["meta"]["name"], "projects/p1/instances/i1");
        assert!(trimmed["spec"].is_null(), "{trimmed}");
        assert!(trimmed["status"]["addresses"].is_null(), "{trimmed}");
    }

    #[test]
    fn a_read_mask_naming_nothing_that_exists_still_gives_back_the_name() {
        let document = json!({"meta": {"name": "nodes/hv-1"}});
        let fields = fields_from(&q(&[("fields", "status.nothing.here")])).unwrap();
        let trimmed = only_fields(&document, &fields);
        assert_eq!(trimmed["meta"]["name"], "nodes/hv-1");
    }
}
