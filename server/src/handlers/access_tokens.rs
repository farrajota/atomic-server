//! `/tokens`: mint, introspect, list and revoke self-issued access tokens.
//! See `crate::access_tokens` for the model and `docs/src/access-tokens.md`
//! for the wire contract.

use actix_web::{web, HttpRequest, HttpResponse};
use atomic_lib::{agents::ForAgent, errors::AtomicError, Subject};
use serde_json::json;

use crate::{
    access_tokens::{self, MintOutcome, MintRequest, TokenView},
    appstate::AppState,
    context::RequestContext,
    errors::AtomicServerResult,
};

fn unauthorized(message: &str) -> crate::errors::AtomicServerError {
    AtomicError::unauthorized(message.to_string()).into()
}

/// The agent whose own key signed `req`. Never an access token: tokens may
/// not mint, list, or revoke other tokens. With `require_v2`, the signature
/// must be version 2, so it covers the method and `body` and a captured proof
/// cannot be replayed with another body.
async fn signing_agent(
    req: &HttpRequest,
    appstate: &AppState,
    context: &RequestContext,
    body: &[u8],
    require_v2: bool,
) -> AtomicServerResult<Subject> {
    if access_tokens::bearer_token(req.headers()).is_some() {
        return Err(unauthorized(
            "An access token cannot do this; sign the request with the agent's own key",
        ));
    }
    let version = req
        .headers()
        .get(atomic_lib::authentication::SIGNATURE_VERSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim);
    if require_v2 && version != Some(atomic_lib::authentication::SIGNATURE_VERSION_2) {
        return Err(unauthorized(
            "This endpoint needs a version 2 request signature (x-atomic-signature-version: 2), which covers the method and body",
        ));
    }
    match crate::helpers::authenticate(req, appstate, context, body).await? {
        ForAgent::AgentSubject(agent) => Ok(agent),
        _ => Err(unauthorized(
            "Sign this request with the agent's key (x-atomic-* headers)",
        )),
    }
}

/// `POST /tokens`: mint a token for the agent that signed the request.
pub async fn mint(
    req: HttpRequest,
    body: web::Bytes,
    appstate: web::Data<AppState>,
    context: RequestContext,
) -> AtomicServerResult<HttpResponse> {
    let agent = signing_agent(&req, &appstate, &context, &body, true).await?;
    crate::helpers::enforce_write_rate_limit(
        &appstate,
        &req,
        &ForAgent::AgentSubject(agent.clone()),
    )?;
    let request: MintRequest = crate::helpers::parse_json(&body)?;
    match access_tokens::mint(
        &appstate.store,
        &agent,
        &request,
        appstate.config.opts.access_token_max_ttl,
    )? {
        MintOutcome::Created { record, token } => {
            let mut body = serde_json::to_value(TokenView::from(&record))
                .map_err(|e| format!("Could not serialize token: {e}"))?;
            body["token"] = json!(token);
            Ok(HttpResponse::Created()
                .insert_header(("Cache-Control", "no-store"))
                .json(body))
        }
        MintOutcome::Duplicate(record) => {
            let mut body = serde_json::to_value(TokenView::from(&record))
                .map_err(|e| format!("Could not serialize token: {e}"))?;
            body["error"] = json!(
                "A token with this nonce was already minted for this agent; its value is not stored and cannot be returned again"
            );
            Ok(HttpResponse::Conflict().json(body))
        }
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
    context: RequestContext,
) -> AtomicServerResult<HttpResponse> {
    let agent = signing_agent(&req, &appstate, &context, &[], false).await?;
    let views: Vec<TokenView> = access_tokens::list(&appstate.store, &agent)?
        .iter()
        .map(TokenView::from)
        .collect();
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
    context: RequestContext,
) -> AtomicServerResult<HttpResponse> {
    let token_id = path.into_inner();
    let revoked = match access_tokens::bearer_token(req.headers()) {
        Some(token) => {
            let record = access_tokens::resolve(&appstate.store, token)?;
            crate::helpers::enforce_write_rate_limit(
                &appstate,
                &req,
                &access_tokens::grant_of(&record),
            )?;
            // A token may revoke itself and nothing else.
            record.token_id == token_id
                && access_tokens::revoke(
                    &appstate.store,
                    &Subject::from_raw(&record.agent, None),
                    &token_id,
                )?
        }
        None => {
            let agent = signing_agent(&req, &appstate, &context, &[], true).await?;
            crate::helpers::enforce_write_rate_limit(
                &appstate,
                &req,
                &ForAgent::AgentSubject(agent.clone()),
            )?;
            access_tokens::revoke(&appstate.store, &agent, &token_id)?
        }
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
