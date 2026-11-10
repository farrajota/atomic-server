//! Self-issued, TTL-bound access tokens.
//!
//! An agent signs `POST /tokens` with its own key and gets back a bearer token
//! (`atk_…`) that acts as that agent, narrowed to the scopes it asked for, until
//! it expires or is revoked. The server keeps only a SHA-256 of each token
//! ([`atomic_lib::db::access_tokens`]) and looks it up on every request; there
//! is no cache, so a revoke or an ACL change applies to the next request.
//!
//! What a token may do is decided by `atomic_lib::hierarchy::check_rights`
//! ([`ForAgent::Token`]): the intersection of the agent's current rights and the
//! token's scopes, plus whatever the public agent may do anyway. Where a token
//! is accepted at all is decided by the server: resource reads (which includes
//! `/query`), WebSocket `AUTH` for a read-only session, and `/tokens`. Every
//! other endpoint refuses it (`helpers::get_auth_headers_for_request`). Tokens
//! cannot sign commits; see `docs/src/access-tokens.md`.

use std::collections::HashMap;
use std::sync::Mutex;

use actix::Addr;
use actix_web::http::header::HeaderMap;
use atomic_lib::{
    agents::{ForAgent, TokenGrant, TokenScope},
    db::access_tokens::{AccessTokenRecord, StoredTokenScope},
    errors::AtomicError,
    hierarchy::Right,
    Db, Subject,
};
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::errors::{AtomicServerError, AtomicServerResult};
use crate::handlers::web_sockets::WebSocketConnection;

/// Every access token starts with this, so it can never be mistaken for the
/// base64 auth proof `Authorization: Bearer` has always carried.
pub const TOKEN_PREFIX: &str = "atk_";

/// Default for `--access-token-max-ttl`: 25 hours, one hour more than the
/// longest lease the OpenBao plugin hands out.
pub const DEFAULT_MAX_TTL_SECONDS: u64 = 25 * 60 * 60;

/// Bounds on what a mint request may carry.
const MAX_SCOPES: usize = 32;
const MAX_TARGET_LEN: usize = 2048;
const MAX_LABEL_LEN: usize = 256;
const MAX_NONCE_LEN: usize = 128;

/// Serializes mints and revokes, so two requests with the same nonce cannot
/// both pass the duplicate check. One process owns the store, so a process
/// lock is enough.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Lower-case hex SHA-256 of the whole token string. A token is 256 random
/// bits, so a plain hash is enough; there is nothing to brute-force.
pub fn hash_token(token: &str) -> String {
    atomic_lib::authentication::sha256_hex(token.as_bytes())
}

fn random_bytes<const N: usize>() -> [u8; N] {
    use rand::RngCore;
    let mut bytes = [0u8; N];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes
}

/// The access token in `Authorization: Bearer atk_…`, if the request carries
/// one. Any other `Authorization` value is not a token and is left alone.
pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|t| t.starts_with(TOKEN_PREFIX))
}

fn parse_right(right: &str) -> Option<Right> {
    match right {
        "read" => Some(Right::Read),
        "write" => Some(Right::Write),
        "append" => Some(Right::Append),
        _ => None,
    }
}

/// The canonical spelling of an agent subject: `atomic:agent:…`, also for a
/// legacy `did:ad:agent:` or `https://host/agents/…` subject.
fn canonical_agent(agent: &Subject) -> String {
    atomic_lib::identifiers::canonicalize_scheme(&atomic_lib::agents::migrate_legacy_agent_subject(
        agent.as_str(),
    ))
}

/// The identity a live record grants.
pub fn grant_of(record: &AccessTokenRecord) -> ForAgent {
    ForAgent::Token(TokenGrant {
        token_id: record.token_id.clone(),
        agent: Subject::from_raw(&record.agent, None),
        scopes: record
            .scopes
            .iter()
            .filter_map(|s| {
                Some(TokenScope {
                    target: s.target.clone(),
                    right: parse_right(&s.right)?,
                })
            })
            .collect(),
        expires_at: record.expires_at,
    })
}

/// The live record for `token`, or a 401 saying why there is none.
pub fn resolve(store: &Db, token: &str) -> AtomicServerResult<AccessTokenRecord> {
    let refused = |why: &str| -> AtomicServerError {
        AtomicError::unauthorized(format!("Access token refused: {why}")).into()
    };
    let record = store
        .get_access_token_by_hash(&hash_token(token))?
        .ok_or_else(|| refused("unknown token"))?;
    if record.revoked_at.is_some() {
        return Err(refused("revoked"));
    }
    if !record.is_live(atomic_lib::utils::now()) {
        return Err(refused("expired"));
    }
    Ok(record)
}

/// Whether the token with this id is still live. Used by WebSocket sessions to
/// re-check the token they were opened with.
pub fn is_live(store: &Db, token_id: &str) -> bool {
    matches!(
        store.get_access_token(token_id),
        Ok(Some(record)) if record.is_live(atomic_lib::utils::now())
    )
}

/// One scope as it travels over HTTP.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeJson {
    pub target: String,
    pub right: String,
}

/// `POST /tokens` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MintRequest {
    pub ttl_seconds: i64,
    pub scopes: Vec<ScopeJson>,
    #[serde(default)]
    pub label: String,
    pub nonce: String,
}

/// A token as callers may see it: everything but the token itself.
#[derive(Debug, Serialize)]
pub struct TokenView {
    pub token_id: String,
    pub agent: String,
    pub scopes: Vec<ScopeJson>,
    pub label: String,
    pub nonce: String,
    pub created_at: i64,
    pub expires_at: i64,
}

impl From<&AccessTokenRecord> for TokenView {
    fn from(r: &AccessTokenRecord) -> Self {
        TokenView {
            token_id: r.token_id.clone(),
            agent: r.agent.clone(),
            scopes: r
                .scopes
                .iter()
                .map(|s| ScopeJson {
                    target: s.target.clone(),
                    right: s.right.clone(),
                })
                .collect(),
            label: r.label.clone(),
            nonce: r.nonce.clone(),
            created_at: r.created_at,
            expires_at: r.expires_at,
        }
    }
}

pub enum MintOutcome {
    /// A new token. The only time its value exists outside the caller.
    Created {
        record: AccessTokenRecord,
        token: String,
    },
    /// A token with this agent and nonce was already minted within the TTL
    /// cap. Its value is not stored, so it cannot be returned.
    Duplicate(AccessTokenRecord),
}

fn validate(request: &MintRequest, max_ttl_seconds: u64) -> AtomicServerResult<()> {
    let bad = |m: String| Err(AtomicServerError::bad_request(m));
    if request.ttl_seconds < 1 {
        return bad("ttl_seconds must be at least 1".into());
    }
    if request.ttl_seconds as u64 > max_ttl_seconds {
        return bad(format!(
            "ttl_seconds {} exceeds this server's maximum of {max_ttl_seconds} (--access-token-max-ttl)",
            request.ttl_seconds
        ));
    }
    if request.scopes.is_empty() {
        return bad("scopes must name at least one target; an empty list grants nothing".into());
    }
    if request.scopes.len() > MAX_SCOPES {
        return bad(format!("at most {MAX_SCOPES} scopes"));
    }
    for scope in &request.scopes {
        if scope.target.trim().is_empty() || scope.target.len() > MAX_TARGET_LEN {
            return bad(format!(
                "a scope target must be a subject of 1 to {MAX_TARGET_LEN} characters"
            ));
        }
        // Tokens are read-only (D17): a write or append scope would promise
        // something the rights check never grants.
        if scope.right != "read" {
            return bad(format!(
                "scope right {:?} is not allowed: access tokens are read-only, so the only right is \"read\"",
                scope.right
            ));
        }
    }
    if request.label.len() > MAX_LABEL_LEN {
        return bad(format!("label is longer than {MAX_LABEL_LEN} bytes"));
    }
    if request.nonce.is_empty() || request.nonce.len() > MAX_NONCE_LEN {
        return bad(format!("nonce must be 1 to {MAX_NONCE_LEN} bytes"));
    }
    Ok(())
}

/// Mint a token for `agent`, or find the one this agent already minted with
/// the same nonce within the TTL cap. Also drops records nobody can use or
/// match a nonce against any more.
pub fn mint(
    store: &Db,
    agent: &Subject,
    request: &MintRequest,
    max_ttl_seconds: u64,
) -> AtomicServerResult<MintOutcome> {
    validate(request, max_ttl_seconds)?;
    let agent = canonical_agent(agent);
    let now = atomic_lib::utils::now();
    let cap_ms = (max_ttl_seconds as i64).saturating_mul(1000);

    let _guard = WRITE_LOCK
        .lock()
        .map_err(|_| "access token lock poisoned")?;
    let mut duplicate = None;
    for record in store.all_access_tokens()? {
        // Kept until the cap has passed since it was minted, so a retried
        // mint with the same nonce is recognised for as long as the token it
        // made could have lived.
        if now >= record.expires_at && now >= record.created_at.saturating_add(cap_ms) {
            store.remove_access_token(&record)?;
            continue;
        }
        if record.agent == agent && record.nonce == request.nonce {
            duplicate = Some(record);
        }
    }
    if let Some(record) = duplicate {
        return Ok(MintOutcome::Duplicate(record));
    }

    let token = format!(
        "{TOKEN_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes::<32>())
    );
    let record = AccessTokenRecord {
        token_id: hex::encode(random_bytes::<16>()),
        token_hash: hash_token(&token),
        agent,
        scopes: request
            .scopes
            .iter()
            .map(|s| StoredTokenScope {
                target: atomic_lib::identifiers::canonicalize_scheme(s.target.trim()),
                right: s.right.clone(),
            })
            .collect(),
        label: request.label.clone(),
        nonce: request.nonce.clone(),
        created_at: now,
        expires_at: now.saturating_add(request.ttl_seconds.saturating_mul(1000)),
        revoked_at: None,
    };
    store.put_access_token(&record)?;
    Ok(MintOutcome::Created { record, token })
}

/// The live tokens of `agent`, oldest first.
pub fn list(store: &Db, agent: &Subject) -> AtomicServerResult<Vec<AccessTokenRecord>> {
    let agent = canonical_agent(agent);
    let now = atomic_lib::utils::now();
    let mut records: Vec<_> = store
        .all_access_tokens()?
        .into_iter()
        .filter(|r| r.agent == agent && r.is_live(now))
        .collect();
    records.sort_by(|a, b| (a.created_at, &a.token_id).cmp(&(b.created_at, &b.token_id)));
    Ok(records)
}

/// Revoke the live token `token_id` of `agent`. `false` when there is none:
/// unknown, someone else's, already revoked or expired.
pub fn revoke(store: &Db, agent: &Subject, token_id: &str) -> AtomicServerResult<bool> {
    let agent = canonical_agent(agent);
    let _guard = WRITE_LOCK
        .lock()
        .map_err(|_| "access token lock poisoned")?;
    let now = atomic_lib::utils::now();
    match store.get_access_token(token_id)? {
        Some(mut record) if record.agent == agent && record.is_live(now) => {
            record.revoked_at = Some(now);
            store.put_access_token(&record)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Closes the WebSocket sessions that authenticated with a token.
#[derive(actix::Message)]
#[rtype(result = "()")]
pub struct CloseTokenSession {
    pub token_id: String,
    pub reason: String,
}

/// The open WebSocket sessions of each token, so a revoke can close them at
/// once instead of waiting for their next liveness check.
#[derive(Default)]
pub struct TokenSessions {
    sessions: Mutex<HashMap<String, Vec<Addr<WebSocketConnection>>>>,
}

impl TokenSessions {
    pub fn register(&self, token_id: &str, addr: Addr<WebSocketConnection>) {
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.entry(token_id.to_string()).or_default().push(addr);
        }
    }

    pub fn deregister(&self, token_id: &str, addr: &Addr<WebSocketConnection>) {
        if let Ok(mut sessions) = self.sessions.lock() {
            if let Some(addrs) = sessions.get_mut(token_id) {
                addrs.retain(|a| a != addr);
                if addrs.is_empty() {
                    sessions.remove(token_id);
                }
            }
        }
    }

    /// Ask every session of `token_id` to close.
    pub fn close(&self, token_id: &str, reason: &str) {
        let addrs = match self.sessions.lock() {
            Ok(mut sessions) => sessions.remove(token_id).unwrap_or_default(),
            Err(_) => return,
        };
        for addr in addrs {
            addr.do_send(CloseTokenSession {
                token_id: token_id.to_string(),
                reason: reason.to_string(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(nonce: &str, ttl: i64) -> MintRequest {
        MintRequest {
            ttl_seconds: ttl,
            scopes: vec![ScopeJson {
                target: "atomic:somedrive".into(),
                right: "read".into(),
            }],
            label: "openbao:atomic:role".into(),
            nonce: nonce.into(),
        }
    }

    async fn store(name: &str) -> Db {
        Db::init_temp(&format!("access_tokens_{name}"))
            .await
            .unwrap()
    }

    fn agent() -> Subject {
        "atomic:agent:O2onvM62pC1io6jQKm8Nc2UyFXcd4kOmOsBIoYtZ2ik".into()
    }

    fn created(outcome: MintOutcome) -> (AccessTokenRecord, String) {
        match outcome {
            MintOutcome::Created { record, token } => (record, token),
            MintOutcome::Duplicate(_) => panic!("expected a new token"),
        }
    }

    #[tokio::test]
    async fn a_minted_token_is_stored_only_as_a_hash() {
        let store = store("hash_only").await;
        let (record, token) = created(mint(&store, &agent(), &request("n1", 60), 3600).unwrap());

        assert!(token.starts_with(TOKEN_PREFIX));
        assert_eq!(record.token_hash, hash_token(&token));
        let stored = serde_json::to_string(&store.all_access_tokens().unwrap()).unwrap();
        assert!(
            !stored.contains(&token),
            "the token value must not be stored"
        );
        assert_eq!(resolve(&store, &token).unwrap().token_id, record.token_id);
    }

    #[tokio::test]
    async fn expiry_is_the_requested_ttl() {
        let store = store("ttl").await;
        let before = atomic_lib::utils::now();
        let (record, _) = created(mint(&store, &agent(), &request("n1", 90), 3600).unwrap());
        let after = atomic_lib::utils::now();
        assert!(record.expires_at >= before + 90_000 && record.expires_at <= after + 90_000);
    }

    #[tokio::test]
    async fn a_ttl_above_the_cap_is_refused_not_clamped() {
        let store = store("cap").await;
        let err = mint(&store, &agent(), &request("n1", 3601), 3600)
            .err()
            .expect("over the cap");
        assert!(err.message.contains("3600"), "{}", err.message);
        assert!(store.all_access_tokens().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_empty_scope_list_is_refused() {
        let store = store("empty_scopes").await;
        let mut r = request("n1", 60);
        r.scopes.clear();
        assert!(mint(&store, &agent(), &r, 3600).is_err());
    }

    #[tokio::test]
    async fn write_and_append_scopes_are_refused() {
        let store = store("write_scope").await;
        for right in ["write", "append"] {
            let mut r = request(right, 60);
            r.scopes[0].right = right.into();
            let err = mint(&store, &agent(), &r, 3600)
                .err()
                .expect("tokens are read-only");
            assert!(err.message.contains("read"), "{}", err.message);
        }
        assert!(store.all_access_tokens().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unknown_right_is_refused() {
        let store = store("bad_right").await;
        let mut r = request("n1", 60);
        r.scopes[0].right = "admin".into();
        assert!(mint(&store, &agent(), &r, 3600).is_err());
    }

    #[tokio::test]
    async fn the_same_nonce_returns_the_existing_token_without_its_value() {
        let store = store("dup").await;
        let (first, _) = created(mint(&store, &agent(), &request("n1", 60), 3600).unwrap());
        match mint(&store, &agent(), &request("n1", 60), 3600).unwrap() {
            MintOutcome::Duplicate(record) => assert_eq!(record.token_id, first.token_id),
            MintOutcome::Created { .. } => panic!("a retried mint must not make a second token"),
        }
        // Still recognised after the first token was revoked.
        assert!(revoke(&store, &agent(), &first.token_id).unwrap());
        assert!(matches!(
            mint(&store, &agent(), &request("n1", 60), 3600).unwrap(),
            MintOutcome::Duplicate(_)
        ));
        // Another agent's nonce is its own.
        let other: Subject = "atomic:agent:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into();
        assert!(matches!(
            mint(&store, &other, &request("n1", 60), 3600).unwrap(),
            MintOutcome::Created { .. }
        ));
    }

    #[tokio::test]
    async fn a_revoked_token_is_refused_and_not_listed() {
        let store = store("revoke").await;
        let (record, token) = created(mint(&store, &agent(), &request("n1", 60), 3600).unwrap());
        assert_eq!(list(&store, &agent()).unwrap().len(), 1);

        assert!(revoke(&store, &agent(), &record.token_id).unwrap());
        assert!(resolve(&store, &token).is_err());
        assert!(list(&store, &agent()).unwrap().is_empty());
        assert!(
            !revoke(&store, &agent(), &record.token_id).unwrap(),
            "already revoked"
        );
    }

    #[tokio::test]
    async fn an_agent_cannot_revoke_or_list_another_agents_token() {
        let store = store("foreign").await;
        let (record, token) = created(mint(&store, &agent(), &request("n1", 60), 3600).unwrap());
        let other: Subject = "atomic:agent:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into();
        assert!(!revoke(&store, &other, &record.token_id).unwrap());
        assert!(list(&store, &other).unwrap().is_empty());
        assert!(resolve(&store, &token).is_ok());
    }

    #[tokio::test]
    async fn an_expired_token_is_refused() {
        let store = store("expired").await;
        let (mut record, token) =
            created(mint(&store, &agent(), &request("n1", 60), 3600).unwrap());
        record.expires_at = atomic_lib::utils::now();
        store.put_access_token(&record).unwrap();
        let err = resolve(&store, &token).expect_err("expired");
        assert!(err.message.contains("expired"), "{}", err.message);
        assert!(list(&store, &agent()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn records_past_the_cap_are_dropped_on_the_next_mint() {
        let store = store("prune").await;
        let (mut old, _) = created(mint(&store, &agent(), &request("old", 60), 3600).unwrap());
        old.created_at -= 3_600_001;
        old.expires_at = old.created_at + 60_000;
        store.put_access_token(&old).unwrap();

        created(mint(&store, &agent(), &request("new", 60), 3600).unwrap());
        assert!(store.get_access_token(&old.token_id).unwrap().is_none());
        // And its nonce may be used again.
        assert!(matches!(
            mint(&store, &agent(), &request("old", 60), 3600).unwrap(),
            MintOutcome::Created { .. }
        ));
    }

    #[test]
    fn only_atk_bearer_values_are_tokens() {
        let mut headers = HeaderMap::new();
        headers.insert(
            actix_web::http::header::AUTHORIZATION,
            "Bearer atk_abc".parse().unwrap(),
        );
        assert_eq!(bearer_token(&headers), Some("atk_abc"));
        headers.insert(
            actix_web::http::header::AUTHORIZATION,
            "Bearer eyJodHRwczovL2F0b21pY2RhdGEuZGV2In0="
                .parse()
                .unwrap(),
        );
        assert_eq!(bearer_token(&headers), None);
    }
}
