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
use velstra_cloud_model::meta::{ResourceName, Revision};

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
        // The layer goes on before the console's routes, so only the API is
        // behind a token. The page itself is markup with no data in it — it
        // carries the sign-in form, and demanding a token to fetch the form
        // that asks for one is a locked door with the key inside.
        .layer(middleware::from_fn_with_state(api.clone(), authenticate))
        // Outside the layer, and it has to be: this is the route that *issues*
        // the token every other route demands. Behind the layer it would be a
        // door whose key is on the other side of it.
        .route("/api/v1/sessions", post(sign_in))
        .route("/", get(console))
        // A deep link into the console is a path this API does not serve and
        // the page does: reloading `/instances/i1` has to return the console
        // rather than a 404, because a single-page console routes it itself.
        .route("/*path", get(console))
        .with_state(api)
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
    let signed_in = api
        .identity()
        .sign_in(&body.username, &body.password)
        .await?;
    Ok((StatusCode::CREATED, Json(signed_in)).into_response())
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
async fn set_password(
    State(api): State<Api>,
    Extension(who): Extension<Identity>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PasswordBody>,
) -> ApiResult<StatusCode> {
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
async fn console() -> Response {
    axum::response::Html(velstra_cloud_console::page_ref()).into_response()
}

/// Every request carries a bearer token, and this is the only place that knows
/// it is a bearer token. What kind of token it is belongs to the verifier.
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
    let identity = identify(api.verifier(), header.as_deref()).await?;
    request.extensions_mut().insert(identity);
    Ok(next.run(request).await)
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

/// Read `?pageSize=` and `?pageToken=` off a query string.
///
/// A page size that is not a number is refused rather than ignored: a client
/// sending `pageSize=twenty` and silently receiving the whole cell is the shape
/// where a load test passes and production does not.
fn paging_from(query: &BTreeMap<String, String>) -> ApiResult<Paging> {
    let size = match query.get("pageSize") {
        None => None,
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

// ---- handlers -------------------------------------------------------------

async fn read(
    State(api): State<Api>,
    Path(path): Path<String>,
    Query(query): Query<BTreeMap<String, String>>,
    Extension(who): Extension<Identity>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let _ = headers;
    match target(&path)? {
        Target::Object(name) => Ok(object(StatusCode::OK, api.get(&name, &who).await?)),
        Target::Verb { name, verb } if verb == "explainPlacement" => {
            Ok(Json(api.explain_placement(&name, &who).await?).into_response())
        }
        Target::Verb { name, verb } if verb == "explainMigration" => {
            Ok(Json(api.explain_migration(&name, &who).await?).into_response())
        }
        Target::Verb { name, verb } if verb == "explainReach" => {
            Ok(Json(api.explain_reach(&name, &who).await?).into_response())
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
            if query.get("watch").map(|w| w == "true").unwrap_or(false) {
                return watch(api, &parent, &kind, query.get("fromRevision"), filter, &who).await;
            }
            // `?pageSize=` / `?pageToken=`, spelled the way AIP-158 spells them.
            // A caller who asks for neither gets the whole collection, which is
            // what every existing client expects and what a controller wants;
            // a caller who asks for either gets a page and a token.
            let paging = paging_from(&query)?;
            let listing = api
                .list_page_for(&parent, &kind, &filter, &paging, &who)
                .await?;
            let mut body = json!({
                "items": listing.items.into_iter().map(to_wire).collect::<Vec<_>>(),
                "revision": listing.revision.to_string(),
            });
            // Present only when there is more. An always-present field that is
            // sometimes empty invites `if (body.nextPageToken !== undefined)`,
            // which loops forever.
            if let Some(token) = listing.next_page_token {
                body["nextPageToken"] = json!(token);
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
    let created = api
        .create(&parent, &kind, &document(&body)?, &identity)
        .await?;
    // 202, because the object exists but has not converged. The operation is
    // what a client waits on, and it is a resource it can come back to rather
    // than a connection it has to hold.
    Ok((StatusCode::ACCEPTED, Json(created_body(&created))).into_response())
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
    Ok(object(StatusCode::OK, updated))
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
    Ok(object(StatusCode::ACCEPTED, deleted.resource))
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
    let revision = document["meta"]["revision"]
        .as_u64()
        .unwrap_or_default()
        .to_string();
    (
        status,
        [(header::ETAG, format!("\"{revision}\""))],
        Json(to_wire(document)),
    )
        .into_response()
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
}
