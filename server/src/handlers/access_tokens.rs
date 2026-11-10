//! `/tokens`: mint, introspect, list and revoke self-issued access tokens.
//! See `crate::access_tokens` for the model and `docs/src/access-tokens.md`
//! for the wire contract.

use actix_web::{web, HttpRequest, HttpResponse};
use atomic_lib::{agents::ForAgent, errors::AtomicError, Db, Storelike, Subject};
use serde_json::json;

use crate::{
    access_tokens::{self, MintOutcome, MintRequest, TokenView},
    appstate::AppState,
    errors::{AppErrorType, AtomicServerError, AtomicServerResult},
};

fn unauthorized(message: &str) -> AtomicServerError {
    AtomicError::unauthorized(message.to_string()).into()
}

/// Run store work that blocks (it takes the access-token lock and flushes)
/// on actix's blocking pool, not on the worker that serves other requests.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> AtomicServerResult<T> + Send + 'static,
) -> AtomicServerResult<T> {
    web::block(work)
        .await
        .map_err(|e| format!("Access token task failed: {e}"))?
}

/// The URL a `/tokens` request must be signed for.
///
/// Bound to the host this node is configured for (`--domain`, or the host of
/// `--server-url`), with the port and scheme the request arrived with. Other
/// endpoints also trust loopback and served-suffix names
/// (`context::host_is_served_here`); here that would let a mint signed for
/// `http://localhost:9883/tokens` on a laptop be replayed to a production
/// node of the same agent. A request for any other host is checked against
/// the configured origin, which its signature then does not match.
pub(crate) fn signed_url(req: &HttpRequest, appstate: &AppState) -> AtomicServerResult<String> {
    let opts = &appstate.config.opts;
    let configured_host = opts
        .server_url
        .as_deref()
        .and_then(|u| url::Url::parse(u.trim()).ok())
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| opts.domain.trim().to_string());
    let headers = req.headers();
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))
        .and_then(|v| v.to_str().ok());
    let configured_origin = appstate.config.get_origin();
    let origin = match host {
        Some(host) if crate::context::strip_port(host).eq_ignore_ascii_case(&configured_host) => {
            let scheme = headers
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .unwrap_or(if configured_origin.starts_with("https://") {
                    "https"
                } else {
                    "http"
                });
            format!("{scheme}://{host}")
        }
        _ => configured_origin,
    };
    crate::helpers::signed_subject(req, &origin)
}

/// A verified signed request: who signed it, and the signature to use up.
struct Signed {
    /// The agent whose key signed, in its canonical spelling.
    agent: Subject,
    signature: String,
    timestamp: i64,
}

/// The agent whose own key signed `req`. Never an access token: tokens may
/// not mint, list, or revoke other tokens. The signature must be version 2,
/// so it covers the method and `body`. Only checks; [`spend`] then charges
/// the agent and uses the signature up.
async fn signing_agent(
    req: &HttpRequest,
    appstate: &AppState,
    body: &[u8],
) -> AtomicServerResult<Signed> {
    if access_tokens::bearer_token(req.headers()).is_some() {
        return Err(unauthorized(
            "An access token cannot do this; sign the request with the agent's own key",
        ));
    }
    let header = |name: &str| {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
    };
    if header(atomic_lib::authentication::SIGNATURE_VERSION_HEADER)
        != Some(atomic_lib::authentication::SIGNATURE_VERSION_2)
    {
        return Err(unauthorized(
            "This endpoint needs a version 2 request signature (x-atomic-signature-version: 2), which covers the method and body",
        ));
    }
    let url = signed_url(req, appstate)?;
    let agent = match crate::helpers::get_client_agent_for_request(req, body, appstate, &url)
        .await?
    {
        ForAgent::AgentSubject(agent) => agent,
        _ => {
            return Err(unauthorized(
                "Sign this request with the agent's key (x-atomic-* headers); a proof older than 5 minutes is not accepted",
            ))
        }
    };
    // A version 2 request that authenticated carried both headers.
    let signature = header("x-atomic-signature").unwrap_or_default().to_string();
    let timestamp = header("x-atomic-timestamp")
        .and_then(|t| t.parse::<i64>().ok())
        .ok_or_else(|| unauthorized("x-atomic-timestamp must be a number"))?;
    Ok(Signed {
        agent: Subject::from_raw(&access_tokens::canonical_agent(&agent), None),
        signature,
        timestamp,
    })
}

/// Charge a signed request to its agent's write budget, then use its
/// signature up, so the same signed request is refused when it comes again,
/// whatever happens to it next. The budget comes first: a key flooding
/// `/tokens` is refused at an in-memory check, before anything is recorded.
fn spend(req: &HttpRequest, appstate: &AppState, signed: &Signed) -> AtomicServerResult<()> {
    crate::helpers::enforce_write_rate_limit(
        appstate,
        req,
        &ForAgent::AgentSubject(signed.agent.clone()),
    )?;
    appstate.used_token_proofs.consume(
        &signed.signature,
        signed.timestamp,
        atomic_lib::utils::now(),
    )
}

/// 409: the agent already minted with this nonce. Never the token value,
/// which is not stored.
fn duplicate(
    record: &atomic_lib::db::access_tokens::AccessTokenRecord,
) -> AtomicServerResult<HttpResponse> {
    let mut body = serde_json::to_value(TokenView::from(record))
        .map_err(|e| format!("Could not serialize token: {e}"))?;
    body["error"] = json!(
        "A token with this nonce was already minted for this agent; its value is not stored and cannot be returned again"
    );
    Ok(HttpResponse::Conflict().json(body))
}

/// A token only narrows rights its agent holds, so the agent must read every
/// target now, and not only through public rights: a scope over data anyone
/// may read (every agent's own resource included) grants nothing a token
/// adds, so a key that was never granted anything cannot mint. A target that
/// does not exist here, or lives elsewhere, is refused the same way.
async fn require_readable(store: &Db, agent: &Subject, target: &str) -> AtomicServerResult<()> {
    let refused = || {
        AtomicServerError {
        message: format!(
            "{agent} cannot read the scope target {target} (or it is not on this node); a token can only narrow rights its agent holds"
        ),
        error_type: AppErrorType::Forbidden,
        error_resource: None,
    }
    };
    let subject = Subject::from_raw(target.trim(), store.get_base_domain().as_deref());
    if !subject.is_local() {
        return Err(refused());
    }
    let resource = store.get_resource(&subject).await.map_err(|_| refused())?;
    if atomic_lib::hierarchy::check_read(store, &resource, &ForAgent::Public)
        .await
        .is_ok()
    {
        return Err(AtomicServerError {
            message: format!(
                "Anyone may read the scope target {target}; a token over public data adds nothing, so none is issued"
            ),
            error_type: AppErrorType::Forbidden,
            error_resource: None,
        });
    }
    atomic_lib::hierarchy::check_read(store, &resource, &ForAgent::AgentSubject(agent.clone()))
        .await
        .map_err(|_| refused())?;
    Ok(())
}

/// `POST /tokens`: mint a token for the agent that signed the request.
pub async fn mint(
    req: HttpRequest,
    body: web::Bytes,
    appstate: web::Data<AppState>,
) -> AtomicServerResult<HttpResponse> {
    let signed = signing_agent(&req, &appstate, &body).await?;
    spend(&req, &appstate, &signed)?;
    let agent = signed.agent;
    let request: MintRequest = crate::helpers::parse_json(&body)?;
    let max_ttl = appstate.config.opts.access_token_max_ttl;
    access_tokens::validate(&request, max_ttl)?;
    // A retried mint learns about its earlier token before anything else is
    // checked: its agent may have lost read on a target meanwhile.
    let (store, a, nonce) = (appstate.store.clone(), agent.clone(), request.nonce.clone());
    if let Some(record) =
        blocking(move || access_tokens::minted_with_nonce(&store, &a, &nonce)).await?
    {
        return duplicate(&record);
    }
    for scope in &request.scopes {
        require_readable(&appstate.store, &agent, &scope.target).await?;
    }
    let store = appstate.store.clone();
    let outcome = blocking(move || access_tokens::mint(&store, &agent, &request, max_ttl)).await?;
    match outcome {
        MintOutcome::Created { record, token } => {
            let mut body = serde_json::to_value(TokenView::from(&record))
                .map_err(|e| format!("Could not serialize token: {e}"))?;
            body["token"] = json!(token);
            Ok(HttpResponse::Created()
                .insert_header(("Cache-Control", "no-store"))
                .json(body))
        }
        MintOutcome::Duplicate(record) => duplicate(&record),
    }
}

/// `GET /tokens/self`: what the bearer token is.
pub async fn introspect(
    req: HttpRequest,
    appstate: web::Data<AppState>,
) -> AtomicServerResult<HttpResponse> {
    let token = access_tokens::bearer_token(req.headers())
        .ok_or_else(|| unauthorized("Send the access token as Authorization: Bearer atk_..."))?;
    let record = access_tokens::resolve(&appstate.store, token)?;
    Ok(HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(TokenView::from(&record)))
}

/// `GET /tokens`: the signing agent's live tokens, without their values.
pub async fn list(
    req: HttpRequest,
    appstate: web::Data<AppState>,
) -> AtomicServerResult<HttpResponse> {
    let signed = signing_agent(&req, &appstate, &[]).await?;
    spend(&req, &appstate, &signed)?;
    let (store, agent) = (appstate.store.clone(), signed.agent);
    let records = blocking(move || access_tokens::list(&store, &agent)).await?;
    let views: Vec<TokenView> = records.iter().map(TokenView::from).collect();
    Ok(HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(views))
}

/// `DELETE /tokens/{id}`: revoke, signed (v2) by the token's agent, or by the
/// token itself. 404 for a token that is unknown, not the caller's, already
/// revoked or expired.
pub async fn revoke(
    req: HttpRequest,
    path: web::Path<String>,
    appstate: web::Data<AppState>,
) -> AtomicServerResult<HttpResponse> {
    let token_id = path.into_inner();
    let agent = match access_tokens::bearer_token(req.headers()) {
        Some(token) => {
            let record = access_tokens::resolve(&appstate.store, token)?;
            // A token may revoke itself and nothing else.
            let agent =
                (record.token_id == token_id).then(|| Subject::from_raw(&record.agent, None));
            if let Some(agent) = &agent {
                crate::helpers::enforce_write_rate_limit(
                    &appstate,
                    &req,
                    &ForAgent::AgentSubject(agent.clone()),
                )?;
            }
            agent
        }
        None => {
            let signed = signing_agent(&req, &appstate, &[]).await?;
            spend(&req, &appstate, &signed)?;
            Some(signed.agent)
        }
    };
    let revoked = match agent {
        Some(agent) => {
            let store = appstate.store.clone();
            let id = token_id.clone();
            blocking(move || access_tokens::revoke(&store, &agent, &id)).await?
        }
        None => false,
    };
    if !revoked {
        return Ok(HttpResponse::NotFound().json(json!({
            "error": "No live access token with this id belongs to the caller"
        })));
    }
    appstate
        .token_sessions
        .close(&token_id, "access token revoked");
    Ok(HttpResponse::NoContent().finish())
}
