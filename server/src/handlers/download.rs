use crate::{
    appstate::AppState, context::RequestContext, errors::AtomicServerResult,
    helpers::get_client_agent,
};
use actix_web::http::header::{ContentDisposition, DispositionType};
use actix_web::{web, HttpRequest, HttpResponse};
use atomic_lib::agents::ForAgent;
use atomic_lib::storelike::Query;
use atomic_lib::{urls, Resource, Storelike, Subject, Value};

use serde::Deserialize;
#[cfg(feature = "img")]
use std::collections::HashSet;

#[serde_with::serde_as]
#[serde_with::skip_serializing_none]
#[derive(Deserialize, Debug)]
pub struct DownloadParams {
    pub q: Option<f32>,
    pub w: Option<u32>,
    pub f: Option<String>,
}

const DEFAULT_MIMETYPE: &str = "application/octet-stream";

/// Downloads the File of the Resource that matches the same URL minus the `/download` path.
#[tracing::instrument(skip(appstate, req))]
pub async fn handle_download(
    path: Option<web::Path<String>>,
    appstate: web::Data<AppState>,
    params: web::Query<DownloadParams>,
    req: actix_web::HttpRequest,
) -> AtomicServerResult<HttpResponse> {
    let headers = req.headers();
    let origin = RequestContext::new(&req, &appstate).origin;
    let store = &appstate.store;

    let subject_path = if let Some(pth) = path {
        format!("/{}", pth)
    } else {
        // There is no end string, so It's the root of the URL, the base URL!
        return Err("Put `/download` in front of an File URL to download it.".into());
    };

    // Content-addressed URLs identify blob bytes, not a File resource at
    // `/files/<hash>`. This remains true when requesting an image rendition:
    // uploads have DID resources, and peers can hold the blob without metadata.
    // Whatever is served under the hash hashes to it, and the mimetype, which
    // the hash does not carry, comes from a File of those bytes the reader may
    // read — falling back to `application/octet-stream` only when there is
    // none (see `stored_blob_mimetype`; without the real mimetype, `nosniff`
    // makes browsers refuse to render an uploaded SVG inline).
    if let Some(hash_hex) = subject_path.strip_prefix("/files/") {
        if let Ok(hash) = blake3::Hash::from_hex(hash_hex) {
            let reader = authorize_blob_read(hash_hex, &req, &origin, &appstate).await?;
            let rendition = params.q.is_some() || params.w.is_some() || params.f.is_some();
            // A rendition carries its own type, so the stored blob's is only
            // looked up when the bytes are served as they are.
            let served = match blob_by_hash_hex(hash_hex, &appstate).await? {
                Some(bytes) if rendition => Some((bytes, None)),
                Some(bytes) => Some((
                    bytes,
                    Some(stored_blob_mimetype(hash_hex, reader.as_ref(), &appstate).await?),
                )),
                None => chunked_file_by_internal_id(hash_hex, &hash, reader.as_ref(), &appstate)
                    .await?
                    .map(|(bytes, mimetype)| (bytes, Some(mimetype))),
            };
            let (bytes, mimetype) = served.ok_or_else(|| {
                atomic_lib::errors::AtomicError::not_found(format!("Blob not found: {hash_hex}"))
            })?;
            let response = match mimetype {
                Some(mimetype) if !rendition => user_blob_response(mimetype, bytes),
                _ => serve_processed_image(&bytes, &hash, &params, &appstate).await?,
            };
            return Ok(private_unless_public(response, reader.as_ref()));
        }
    }

    let subject = atomic_lib::Subject::from_raw(&subject_path, None);

    // Support did:ad:blob: subjects directly in /download
    if subject.is_blob_did() {
        if let Some(hash_hex) = subject.blob_hash_hex() {
            let reader = authorize_blob_read(hash_hex, &req, &origin, &appstate).await?;
            let stored = match blake3::Hash::from_hex(hash_hex) {
                Ok(_) => blob_by_hash_hex(hash_hex, &appstate).await?,
                Err(_) => None,
            };
            if let Some(bytes) = stored {
                let mimetype = stored_blob_mimetype(hash_hex, reader.as_ref(), &appstate).await?;

                return Ok(private_unless_public(
                    user_blob_response(mimetype, bytes),
                    reader.as_ref(),
                ));
            }
        }
    }

    let resolved_subject = subject.resolve(&origin);

    let for_agent = get_client_agent(headers, &appstate, &resolved_subject).await?;
    tracing::info!("handle_download: {}", resolved_subject);

    let resource = store
        .get_resource_extended(&resolved_subject.into(), false, &for_agent)
        .await?
        .to_single();

    let response = download_file_handler_partial(&resource, &req, &params, &appstate).await?;
    Ok(private_unless_public(response, Some(&for_agent)))
}

/// Bytes served because a signed-in agent may read them must not be kept by
/// shared caches, which would hand them to the next visitor of the URL.
/// `None` is a content-addressed read without `--require-blob-auth`, where
/// the hash itself is the capability and no reader was established.
fn private_unless_public(mut response: HttpResponse, reader: Option<&ForAgent>) -> HttpResponse {
    if reader.is_some_and(|agent| *agent != ForAgent::Public) {
        response.headers_mut().insert(
            actix_web::http::header::CACHE_CONTROL,
            actix_web::http::header::HeaderValue::from_static("private, no-store"),
        );
    }
    response
}

/// With `--require-blob-auth`, a content-addressed request is answered only
/// for an agent who may read a resource referencing the blob. The agent comes
/// from signed headers (over the URL as requested) or, for the `<img>` and
/// `<video>` tags the data browser points at these URLs, the same-origin
/// session cookie. Without the flag the hash is the capability, as documented
/// in `docs/src/files.md`. Returns the authorized agent when the flag is on,
/// so later lookups by hash can be limited to what that agent may read.
async fn authorize_blob_read(
    hash_hex: &str,
    req: &HttpRequest,
    origin: &str,
    appstate: &AppState,
) -> AtomicServerResult<Option<ForAgent>> {
    if !appstate.store.requires_blob_read_auth() {
        return Ok(None);
    }
    let requested = format!(
        "{origin}{}",
        req.uri()
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or_else(|| req.path())
    );
    let for_agent = get_client_agent(req.headers(), appstate, &requested).await?;
    atomic_lib::hierarchy::check_blob_read(&appstate.store, hash_hex, &for_agent).await?;
    Ok(Some(for_agent))
}

/// Serves user-uploaded blob bytes as a forced download rather than rendering
/// inline. The stored `mimetype` is attacker-controlled at upload time (e.g.
/// `text/html`, `image/svg+xml`); with no Content-Disposition/nosniff, a
/// browser opening the download link would render an uploaded script
/// same-origin, reading the session cookie and the Agent's private key out of
/// IndexedDB. Forcing `attachment` + `nosniff` closes that off; it doesn't
/// affect legitimate inline use (e.g. `<img>` embedding), since
/// Content-Disposition only governs top-level navigation, not embedded
/// resource fetches.
fn user_blob_response(content_type: impl AsRef<str>, bytes: Vec<u8>) -> HttpResponse {
    HttpResponse::Ok()
        .content_type(content_type.as_ref())
        .insert_header((
            actix_web::http::header::CONTENT_DISPOSITION,
            ContentDisposition {
                disposition: DispositionType::Attachment,
                parameters: vec![],
            },
        ))
        .insert_header((
            actix_web::http::header::HeaderName::from_static("x-content-type-options"),
            actix_web::http::header::HeaderValue::from_static("nosniff"),
        ))
        .body(bytes)
}

/// Look up a blob by its hex-encoded BLAKE3 hash. Returns `None` if the input
/// is not a 64-char hex string or no blob is stored under that hash.
async fn blob_by_hash_hex(
    hash_hex: &str,
    appstate: &AppState,
) -> AtomicServerResult<Option<Vec<u8>>> {
    if hash_hex.len() != 64 {
        return Ok(None);
    }
    let Ok(hash_bytes) = hex::decode(hash_hex) else {
        return Ok(None);
    };
    Ok(appstate.store.get_blob(&hash_bytes).await?)
}

/// The most bytes a chunked File is rebuilt to: its declared `filesize`, and
/// never more than an upload may carry. Anyone may list the same huge chunk
/// any number of times, and the File's subject and every hash it claims are
/// served by rebuilding it.
fn rebuild_limit(resource: &Resource) -> usize {
    let declared = match resource.get(urls::FILESIZE) {
        Ok(Value::Integer(size)) => usize::try_from(*size).ok(),
        _ => None,
    };
    declared.map_or(crate::serve::PAYLOAD_MAX, |size| {
        size.min(crate::serve::PAYLOAD_MAX)
    })
}

/// The bytes of a File: concatenated chunk blobs when it is chunked (its `chunks`
/// property is a non-empty ordered list of `did:ad:blob:` refs), otherwise the
/// single blob referenced by `internalId`. Rebuilding stops with an error as
/// soon as the chunks exceed [rebuild_limit].
async fn reconstruct_file_bytes(
    resource: &Resource,
    appstate: &AppState,
) -> AtomicServerResult<Vec<u8>> {
    if let Ok(Value::ResourceArray(chunks)) = resource.get(urls::CHUNKS) {
        if !chunks.is_empty() {
            let limit = rebuild_limit(resource);
            let mut out = Vec::new();

            for chunk in chunks {
                let did = chunk.to_string();
                let subject = Subject::from(did.as_str());
                let hash_hex = subject
                    .blob_hash_hex()
                    .ok_or_else(|| format!("Invalid chunk reference: {did}"))?;
                let bytes = blob_by_hash_hex(hash_hex, appstate)
                    .await?
                    .ok_or_else(|| format!("Chunk blob not found: {hash_hex}"))?;
                if bytes.len() > limit - out.len() {
                    return Err(format!(
                        "The chunks of {} exceed its size of at most {limit} bytes",
                        resource.get_subject()
                    )
                    .into());
                }
                out.extend_from_slice(&bytes);
            }

            return Ok(out);
        }
    }

    let internal_id = resource
        .get(urls::INTERNAL_ID)
        .map_err(|e| format!("Internal ID of file could not be resolved. {}", e))?
        .to_string();
    let hash_bytes = hex::decode(&internal_id)
        .map_err(|_| format!("File internalId is not hex: {}", internal_id))?;
    if hash_bytes.len() != 32 {
        return Err(format!(
            "File internalId is not a 32-byte BLAKE3 hash: {}",
            internal_id
        )
        .into());
    }

    appstate
        .store
        .get_blob(&hash_bytes)
        .await?
        .ok_or_else(|| format!("Blob not found: {}", internal_id).into())
}

/// Resources whose whole-file `internalId` is this hash. Lets the
/// content-addressed `/download/files/{hash}` route recover the metadata
/// (mimetype, chunk list) that the hash alone does not carry. More than one
/// File can share a hash — the same bytes uploaded twice are stored once.
/// Like the read check on the blob, only the first
/// [MAX_BLOB_REFERENCES_CHECKED](atomic_lib::hierarchy::MAX_BLOB_REFERENCES_CHECKED)
/// are looked at: anyone may name a hash from as many resources as they like.
async fn files_by_internal_id(
    hash_hex: &str,
    appstate: &AppState,
) -> AtomicServerResult<Vec<Resource>> {
    let mut query = Query::new_prop_val(urls::INTERNAL_ID, hash_hex);
    query.limit = Some(atomic_lib::hierarchy::MAX_BLOB_REFERENCES_CHECKED);
    let result = appstate.store.query(&query).await?;

    Ok(result.resources)
}

/// How many chunked Files claiming a whole-file hash
/// [chunked_file_by_internal_id] rebuilds and hashes, at most, per request.
/// The whole-file hash is never stored, so anyone may claim it from Files of
/// their own; each costs a rebuild. A real file is normally claimed by one or
/// a few Files. Past the bound the address answers `404` while each File's
/// own `/download/{subject}` keeps working.
const MAX_CHUNKED_CLAIMANTS_REBUILT: usize = 8;

fn has_chunks(resource: &Resource) -> bool {
    matches!(resource.get(urls::CHUNKS), Ok(Value::ResourceArray(c)) if !c.is_empty())
}

/// With `reader` (blob read auth on), whether that agent may read `file`.
/// Without it the hash is the capability and every File counts.
async fn may_read(file: &Resource, reader: Option<&ForAgent>, appstate: &AppState) -> bool {
    match reader {
        Some(reader) => atomic_lib::hierarchy::check_read(&appstate.store, file, reader)
            .await
            .is_ok(),
        None => true,
    }
}

/// The bytes of a chunked File when they hash to `hash`. Its `internalId` is
/// only a claim: anyone may name a whole-file hash that has no stored blob,
/// next to chunks of their own. A File whose chunks are not all held here
/// cannot produce the bytes and does not match either.
async fn chunked_bytes_matching(
    file: &Resource,
    hash: &blake3::Hash,
    appstate: &AppState,
) -> Option<Vec<u8>> {
    match reconstruct_file_bytes(file, appstate).await {
        Ok(bytes) if blake3::hash(&bytes) == *hash => Some(bytes),
        Ok(_) => {
            tracing::debug!(
                "{} claims blob {hash} but its chunks hash otherwise",
                file.get_subject()
            );
            None
        }
        Err(e) => {
            tracing::debug!(
                "{} claims blob {hash} but its bytes cannot be rebuilt: {e}",
                file.get_subject()
            );
            None
        }
    }
}

/// Bytes and mimetype for the content-addressed URL of a chunked file, whose
/// whole-file blob is never stored: from the first chunked File with this
/// `internalId` that the reader may read and whose chunks hash to it. `None`
/// if there is no such File among the first [MAX_CHUNKED_CLAIMANTS_REBUILT]
/// readable ones.
///
/// The whole-file hash has no bytes of its own, so anyone may reference it
/// from a resource they write; that reference passes `check_blob_read`, and
/// must neither unlock somebody else's chunks nor serve its own chunks under
/// somebody else's hash.
async fn chunked_file_by_internal_id(
    hash_hex: &str,
    hash: &blake3::Hash,
    reader: Option<&ForAgent>,
    appstate: &AppState,
) -> AtomicServerResult<Option<(Vec<u8>, String)>> {
    let mut rebuilt = 0;
    for file in files_by_internal_id(hash_hex, appstate).await? {
        if rebuilt >= MAX_CHUNKED_CLAIMANTS_REBUILT {
            tracing::debug!(
                "blob {hash}: more than {MAX_CHUNKED_CLAIMANTS_REBUILT} chunked Files claim it"
            );
            break;
        }
        if !has_chunks(&file) || !may_read(&file, reader, appstate).await {
            continue;
        }
        rebuilt += 1;
        if let Some(bytes) = chunked_bytes_matching(&file, hash, appstate).await {
            return Ok(Some((bytes, mimetype_of(&file))));
        }
    }
    Ok(None)
}

/// The File's stored `mimetype`, or `application/octet-stream` when it has none.
fn mimetype_of(resource: &Resource) -> String {
    resource
        .get(urls::MIMETYPE)
        .map(|v| v.to_string())
        .unwrap_or_else(|_| DEFAULT_MIMETYPE.to_string())
}

/// The mimetype to serve the stored blob `hash` with.
///
/// The content-addressed routes are handed nothing but a hash, but they must
/// still answer with the real mimetype: `user_blob_response` sets `nosniff`, so
/// an `application/octet-stream` answer makes the browser refuse to render the
/// bytes in an `<img>` — which is exactly how every client-uploaded file is
/// referenced, since `downloadURL` points at `/download/files/{hash}`.
///
/// Anyone may name a hash before its bytes are uploaded, so the type comes
/// only from a File the reader may read whose own bytes are this blob: one
/// naming it by `internalId` or `blob`, without chunks. A chunked File's bytes
/// are its chunks; telling whether they hash to this blob would take
/// rebuilding them, for every such File, on every request.
async fn stored_blob_mimetype(
    hash_hex: &str,
    reader: Option<&ForAgent>,
    appstate: &AppState,
) -> AtomicServerResult<String> {
    let mut by_blob = Query::new();
    by_blob.property = Some(urls::BLOB.to_string());
    by_blob.value = Some(Value::AtomicUrl(
        atomic_lib::identifiers::blob_subject(&hash_hex.to_ascii_lowercase()).into(),
    ));
    by_blob.limit = Some(atomic_lib::hierarchy::MAX_BLOB_REFERENCES_CHECKED);
    let named_by_blob = appstate.store.query(&by_blob).await?.resources;
    let claimants = files_by_internal_id(hash_hex, appstate)
        .await?
        .into_iter()
        .chain(named_by_blob);
    for file in claimants {
        let Ok(mimetype) = file.get(urls::MIMETYPE) else {
            continue;
        };
        let own_bytes = match file.get(urls::INTERNAL_ID) {
            Ok(id) => id.to_string().eq_ignore_ascii_case(hash_hex),
            Err(_) => true,
        };
        if !own_bytes || has_chunks(&file) || !may_read(&file, reader, appstate).await {
            continue;
        }
        return Ok(mimetype.to_string());
    }
    Ok(DEFAULT_MIMETYPE.to_string())
}

pub async fn download_file_handler_partial(
    resource: &Resource,
    _req: &HttpRequest,
    params: &web::Query<DownloadParams>,
    appstate: &AppState,
) -> AtomicServerResult<HttpResponse> {
    let bytes = reconstruct_file_bytes(resource, appstate).await?;

    let mimetype = mimetype_of(resource);

    // No params: serve the original bytes verbatim.
    if params.q.is_none() && params.w.is_none() && params.f.is_none() {
        return Ok(user_blob_response(mimetype, bytes));
    }

    // With image params: serve a processed rendition, cached in the blob
    // backend under a key derived from these bytes (see `processed_cache_key`).
    serve_processed_image(&bytes, &blake3::hash(&bytes), params, appstate).await
}

/// `source_hash` is the BLAKE3 hash of `source_bytes`.
#[cfg(feature = "img")]
async fn serve_processed_image(
    source_bytes: &[u8],
    source_hash: &blake3::Hash,
    params: &web::Query<DownloadParams>,
    appstate: &AppState,
) -> AtomicServerResult<HttpResponse> {
    use crate::handlers::image::{is_image_bytes, process_image_bytes};

    let quantized = quantize_params(params);
    let params = &quantized;
    let format = get_format(params)?;
    let cache_key = processed_cache_key(source_hash, &format, params);

    if let Some(cached) = appstate.store.get_blob(&cache_key).await? {
        return Ok(user_blob_response(mimetype_for(&format), cached));
    }

    if !is_image_bytes(source_bytes) {
        return Err("Quality or width parameters are only supported for image files".into());
    }

    let encoded = process_image_bytes(source_bytes, params, &format)?;
    appstate.store.put_blob(&cache_key, &encoded).await?;

    Ok(user_blob_response(mimetype_for(&format), encoded))
}

#[cfg(not(feature = "img"))]
async fn serve_processed_image(
    _source_bytes: &[u8],
    _source_hash: &blake3::Hash,
    _params: &web::Query<DownloadParams>,
    _appstate: &AppState,
) -> AtomicServerResult<HttpResponse> {
    Err("Image processing is not enabled in this build (compile with the `img` feature)".into())
}

/// Prefix of every rendition cache key. It makes the key longer than the 32
/// bytes of a BLAKE3 hash, the only length `/download/files/<hash>`, a blob
/// identifier or a sync blob request can name.
#[cfg(feature = "img")]
const RENDITION_KEY_PREFIX: &[u8] = b"rendition:";

/// Cache key for a processed rendition: derived from the hash of the bytes it
/// is made from (computed from them, or the content address they were
/// verified against) and the parameters, never from the hash a File merely
/// claims as its `internalId`. A File claiming someone else's hash next to
/// chunks of its own would otherwise read or overwrite their cached
/// renditions, and a 32-byte key computable from a public hash could be
/// referenced before it is rendered and then fetched as a blob.
#[cfg(feature = "img")]
fn processed_cache_key(
    source_hash: &blake3::Hash,
    format: &str,
    params: &DownloadParams,
) -> Vec<u8> {
    let canonical = format!(
        "rendition|source={}|f={}|q={}|w={}",
        source_hash.to_hex(),
        format,
        params.q.map(|q| q.to_string()).unwrap_or_default(),
        params.w.map(|w| w.to_string()).unwrap_or_default(),
    );
    [
        RENDITION_KEY_PREFIX,
        blake3::hash(canonical.as_bytes()).as_bytes(),
    ]
    .concat()
}

/// Largest width a rendition is produced at; wider requests are clamped.
#[cfg(feature = "img")]
const MAX_RENDITION_WIDTH: u32 = 4096;
/// Widths are rounded up to a multiple of this, qualities to whole numbers.
#[cfg(feature = "img")]
const RENDITION_WIDTH_STEP: u32 = 64;

/// Snap the rendition parameters onto a small grid before they reach either
/// the encoder or the cache key. Renditions are persisted per distinct
/// parameter set and nothing evicts them, so with `q` an `f32` and `w` any
/// number, a reader of one public image could grow the store without bound by
/// varying them. On the grid there are at most 100 x 64 renditions per format.
#[cfg(feature = "img")]
fn quantize_params(params: &DownloadParams) -> DownloadParams {
    let q = params.q.map(|q| {
        let q = if q.is_finite() { q } else { 80.0 };
        q.round().clamp(1.0, 100.0)
    });
    let w = params.w.map(|w| {
        let w = w.clamp(1, MAX_RENDITION_WIDTH);
        w.div_ceil(RENDITION_WIDTH_STEP) * RENDITION_WIDTH_STEP
    });
    DownloadParams {
        q,
        w,
        f: params.f.clone(),
    }
}

#[cfg(feature = "img")]
fn mimetype_for(format: &str) -> &'static str {
    match format {
        "webp" => "image/webp",
        "avif" => "image/avif",
        _ => "application/octet-stream",
    }
}

#[cfg(feature = "img")]
fn get_format(params: &DownloadParams) -> AtomicServerResult<String> {
    let supported_compression_formats: HashSet<String> =
        HashSet::from_iter(vec!["webp".to_string(), "avif".to_string()]);

    let format = params.f.clone().unwrap_or("webp".to_string());
    if !supported_compression_formats.contains(&format) {
        return Err("Unsupported format".into());
    }

    Ok(format)
}
