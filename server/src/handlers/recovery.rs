//! Authenticated, read-only selected-resource recovery export.

use crate::{
    appstate::AppState, context::RequestContext, errors::AtomicServerResult,
    helpers::get_client_agent,
};
use actix_web::{web, HttpRequest, HttpResponse};

pub async fn handle_recovery_export(
    appstate: web::Data<AppState>,
    body: web::Bytes,
    request: HttpRequest,
) -> AtomicServerResult<HttpResponse> {
    let origin = RequestContext::new(&request, &appstate).origin;
    let full_url = format!("{}{}", origin, request.uri());
    let actor = get_client_agent(request.headers(), &appstate, &full_url).await?;
    let selection = atomic_lib::recovery::parse_request(&body)?;
    let bundle = atomic_lib::recovery::export_bundle(&appstate.store, &selection, &actor).await?;
    Ok(HttpResponse::Ok()
        .content_type("application/vnd.atomic-recovery")
        .body(bundle))
}
