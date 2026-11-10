//! Full-text search over the KV inverted index in `atomic_lib::search`.
//! The index updates on every commit. `--rebuild-indexes search` rebuilds it.

use crate::{appstate::AppState, context::RequestContext, errors::AtomicServerResult};
use actix_web::{web, HttpResponse};
use atomic_lib::{client::search::SearchOpts, urls, Resource, Storelike};
use serde::Deserialize;
use serde_with::{formats::CommaSeparator, StringWithSeparator};
use simple_server_timing_header::Timer;

// All this serde stuff is to allow comma separated lists in the query params.
#[serde_with::serde_as]
#[serde_with::skip_serializing_none]
#[derive(Deserialize, Debug)]
pub struct SearchQuery {
    /// The text search query entered by the user in the search box
    pub q: Option<String>,
    /// Maximum amount of results
    pub limit: Option<usize>,
    /// Only include resources that have one of these resources as its ancestor
    #[serde_as(as = "Option<StringWithSeparator::<CommaSeparator, String>>")]
    pub parents: Option<Vec<String>>,
    /// Filter on exact property-value pairs: `prop:"value" AND prop2:"value2"`.
    pub filters: Option<String>,
    pub include: Option<bool>,
    /// Also return resources in the drive's Trash folder (hidden by default).
    pub include_trashed: Option<bool>,
}

const DEFAULT_RETURN_LIMIT: usize = 30;
/// Upper bound for a client-supplied `limit`.
const MAX_RETURN_LIMIT: usize = 500;
// We fetch extra documents, as the user may not have the rights to the first ones!
// We filter these results later.
// https://github.com/atomicdata-dev/atomic-server/issues/279.
const UNAUTHORIZED_RESULTS_FACTOR: usize = 3;

/// Upper bound for an access token's `limit`. Each of up to three times this
/// many candidates is read-checked through an uncached walk for a token (the
/// agent's check and the scope walk), so a token's search costs at most a few
/// hundred walks. A client that wants more pages by `parents` instead.
pub const TOKEN_SEARCH_MAX_LIMIT: usize = 100;

/// The `limit` a search runs with: the requested one within bounds, lower
/// for an access token (`token`).
pub fn effective_limit(requested: Option<usize>, token: bool) -> usize {
    let max = if token {
        TOKEN_SEARCH_MAX_LIMIT
    } else {
        MAX_RETURN_LIMIT
    };
    match requested {
        // Bounded: the caller may be unauthenticated, and `limit` feeds
        // multiplications and a result loop below.
        Some(l) if l > 0 => l.min(max),
        _ => DEFAULT_RETURN_LIMIT.min(max),
    }
}

/// How many `parents` an access token's search may name. Each is fetched and
/// walked against the token's scopes before the index is asked.
pub const TOKEN_SEARCH_MAX_PARENTS: usize = 32;

/// The parents an access token's search runs under: its scope targets when
/// the request names none; otherwise each requested parent that lies inside a
/// scope, and each scope target that lies inside a requested parent. `None`
/// when nothing is left, which answers as a search without hits. Matches the
/// token may not read can then not fill the result window, and a requested
/// parent outside every scope learns nothing. 400 for more than
/// [`TOKEN_SEARCH_MAX_PARENTS`] requested parents.
async fn token_search_parents(
    store: &atomic_lib::Db,
    grant: &atomic_lib::agents::TokenGrant,
    requested: Option<&[String]>,
) -> AtomicServerResult<Option<Vec<String>>> {
    let targets: Vec<&str> = grant.scopes.iter().map(|s| s.target.as_str()).collect();
    let requested = match requested {
        Some(requested) if !requested.is_empty() => requested,
        _ => {
            return Ok(
                (!targets.is_empty()).then(|| targets.iter().map(|t| t.to_string()).collect())
            )
        }
    };
    if requested.len() > TOKEN_SEARCH_MAX_PARENTS {
        return Err(crate::errors::AtomicServerError::bad_request(format!(
            "An access token's search takes at most {TOKEN_SEARCH_MAX_PARENTS} parents"
        )));
    }
    // Only subjects this node holds: never fetch a requested URL elsewhere.
    let local = |raw: &str| {
        let subject = atomic_lib::Subject::from_raw(raw.trim(), store.get_base_domain().as_deref());
        subject.is_local().then_some(subject)
    };
    // The scope targets, fetched once for every requested parent.
    let mut target_resources = Vec::new();
    for target in &targets {
        if let Some(subject) = local(target) {
            if let Ok(resource) = store.get_resource(&subject).await {
                target_resources.push((*target, resource));
            }
        }
    }
    let mut parents: Vec<String> = Vec::new();
    for parent in requested {
        let Some(subject) = local(parent) else {
            continue;
        };
        if let Ok(resource) = store.get_resource(&subject).await {
            if atomic_lib::hierarchy::is_within_any(store, &resource, &targets).await {
                parents.push(parent.clone());
                continue;
            }
        }
        for (target, resource) in &target_resources {
            if !parents.iter().any(|p| p == target)
                && atomic_lib::hierarchy::is_within_any(store, resource, &[parent.as_str()]).await
            {
                parents.push(target.to_string());
            }
        }
    }
    Ok((!parents.is_empty()).then_some(parents))
}

/// Parses a search query and responds with a list of resources
#[tracing::instrument(skip(appstate, req))]
pub async fn search_query(
    appstate: web::Data<AppState>,
    params: web::Query<SearchQuery>,
    req: actix_web::HttpRequest,
) -> AtomicServerResult<HttpResponse> {
    let mut timer = Timer::new();
    let origin = RequestContext::new(&req, &appstate).origin;
    let store = &appstate.store;
    let path_and_query = req
        .uri()
        .path_and_query()
        .ok_or("Add a query param")?
        .to_string();
    // Who is asking, before any index work. An access token is accepted, as
    // for a resource read: every hit below is read-checked as this identity,
    // so a token sees the hits its narrowed rights allow and nothing else.
    let for_agent = crate::helpers::get_client_agent_or_access_token(
        req.headers(),
        &appstate,
        &format!("{}{}", origin, path_and_query),
    )
    .await?;
    let token = match &for_agent {
        atomic_lib::agents::ForAgent::Token(grant) => Some(grant),
        _ => None,
    };
    let limit = effective_limit(params.limit, token.is_some());

    let fetch_limit = (limit * UNAUTHORIZED_RESULTS_FACTOR) as u32;
    let mut opts = SearchOpts {
        limit: Some(fetch_limit),
        parents: params.parents.clone(),
        include_trashed: params.include_trashed.unwrap_or(false),
        ..Default::default()
    };
    if let Some(filter) = &params.filters {
        opts.filter_pairs = Some(atomic_lib::search::parse_search_filters(filter));
    }

    // A token searches under its scopes only (see `token_search_parents`).
    let mut nothing_to_search = false;
    if let Some(grant) = token {
        match token_search_parents(store, grant, params.parents.as_deref()).await? {
            Some(parents) => opts.parents = Some(parents),
            None => nothing_to_search = true,
        }
    }

    let q = params.q.as_deref().unwrap_or("");
    let (hits, timings) = if nothing_to_search {
        (Vec::new(), atomic_lib::search::QueryTimings::default())
    } else {
        atomic_lib::search::query_timed(store, q, &opts)?
    };
    timer.add("execute_query");
    crate::metrics::search_performed();

    let subjects: Vec<String> = hits.into_iter().map(|h| h.subject.to_string()).collect();
    tracing::debug!(
        "search_query: kv index returned {} subjects for params={:?}",
        subjects.len(),
        params
    );

    let subject =
        atomic_lib::Subject::from_raw(&path_and_query, store.get_base_domain().as_deref());

    let mut results_resource = crate::plugins::search::search_endpoint()
        .to_resource(store, &subject.to_string())
        .await?;
    results_resource.set_subject(subject.to_string());

    timer.add("build_endpoint");
    let resources = readable_resources(&appstate, &for_agent, subjects.clone(), limit).await;
    timer.add("get_resources");
    tracing::info!(
        "search_query: after auth filter -> {} resources (was {} subjects)",
        resources.len(),
        subjects.len()
    );

    let filtered_subjects: Vec<String> = resources
        .iter()
        .map(|r| r.get_subject().resolve(&origin))
        .collect();

    results_resource
        .set(
            urls::ENDPOINT_RESULTS.into(),
            filtered_subjects.into(),
            store,
        )
        .await?;

    let mut result_vec: Vec<Resource> = if params.include.unwrap_or(false) {
        resources
    } else {
        vec![]
    };

    result_vec.push(results_resource);

    let mut builder = HttpResponse::Ok();
    // Timing is for profiling, and only when the operator asks for it: the
    // candidate counts come from the index before the rights filter, and the
    // step durations grow with them, so either would tell any caller how many
    // resources match, private ones included.
    if appstate.config.opts.debug_search_timing {
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
        builder.append_header((
            "Server-Timing",
            format!(
                "{}, score;dur={:.1}, scope;dur={:.1}, rank;dur={:.1}, candidates;desc=\"{} in scope of {}\"",
                timer.header_value(),
                ms(timings.score),
                ms(timings.scope),
                ms(timings.rank),
                timings.in_scope,
                timings.candidates,
            ),
        ));
    }
    builder.content_type("application/ad+json");

    Ok(builder.body(Resource::vec_to_json_ad(&result_vec, Some(&origin))?))
}

/// [`readable_resources`] for a caller that is authenticated here and may not
/// use an access token (`/vector_search`).
#[cfg(feature = "vector-search")]
#[tracing::instrument(skip(appstate, req))]
pub async fn get_resources(
    req: actix_web::HttpRequest,
    appstate: &web::Data<AppState>,
    subject: &str,
    subjects: Vec<String>,
    limit: usize,
) -> AtomicServerResult<Vec<Resource>> {
    let for_agent = crate::helpers::get_client_agent(req.headers(), appstate, subject).await?;
    Ok(readable_resources(appstate, &for_agent, subjects, limit).await)
}

/// The first `limit` of `subjects` that `for_agent` may read, each fetched
/// through the same read check as a resource GET
/// (`get_resource_extended`, `hierarchy::check_read`). For an access token
/// that is its narrowed rights: (agent rights and scopes) or public. Every
/// check builds its own state; nothing is cached across results.
pub async fn readable_resources(
    appstate: &AppState,
    for_agent: &atomic_lib::agents::ForAgent,
    subjects: Vec<String>,
    limit: usize,
) -> Vec<Resource> {
    let mut resources: Vec<Resource> = Vec::new();
    for s in subjects {
        match appstate
            .store
            .get_resource_extended(&s.clone().into(), true, for_agent)
            .await
        {
            Ok(r) => {
                if resources.len() < limit {
                    resources.push(r.to_single());
                } else {
                    break;
                }
            }
            Err(_e) => {
                tracing::debug!("Skipping search result: {} : {}", s, _e);
                continue;
            }
        }
    }
    resources
}
