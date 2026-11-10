//! Storage for self-issued access tokens (`POST /tokens` on the server).
//!
//! Only a SHA-256 of each token is kept, never the token itself. Every row
//! lives in [`Tree::AccessTokens`], under a key that says what it is:
//!
//! - `id\0<token_id>`: the [`AccessTokenRecord`] as JSON;
//! - `hash\0<sha256 hex of the token>`: the token id, so a bearer token
//!   resolves with two point reads;
//! - `nonce\0<agent>\0<nonce>`: the token id, so a repeated mint is found
//!   without a scan;
//! - `agent\0<agent>\0<token_id>`: empty, so one agent's tokens are a prefix;
//! - `proof\0<hash of a request signature>`: a signed request already used;
//! - `exp\0<retain_until, 16 hex digits>\0<t|p>\0<id or proof hash>`: empty,
//!   ordered by when the row may go, so pruning reads only what has expired.
//!
//! A record's rows are written and removed in one batch. Nothing here is
//! cached: every lookup reads the store, so a revoke applies to the next
//! request. Agents, nonces and ids never contain `\0` (the server refuses
//! control characters in a nonce, and the others are generated or canonical).

use super::trees::{Method, Operation, Tree};
use crate::{errors::AtomicResult, Db};
use serde::{Deserialize, Serialize};

/// One scope of a token as stored: a target subject and its right.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredTokenScope {
    pub target: String,
    pub right: String,
}

/// A token as the server keeps it. Holds the hash of the token, not the token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessTokenRecord {
    pub token_id: String,
    /// Lower-case hex SHA-256 of the full token string.
    pub token_hash: String,
    /// The agent the token acts as.
    pub agent: String,
    pub scopes: Vec<StoredTokenScope>,
    pub label: String,
    pub nonce: String,
    /// Unix ms.
    pub created_at: i64,
    /// Unix ms. The token is refused from this instant on.
    pub expires_at: i64,
    /// Unix ms, once revoked.
    pub revoked_at: Option<i64>,
    /// Unix ms until which the record, and with it its nonce, is kept after
    /// it stopped being usable. At least `expires_at`.
    pub retain_until: i64,
}

impl AccessTokenRecord {
    /// Whether the token may still be used at `now` (Unix ms).
    pub fn is_live(&self, now: i64) -> bool {
        self.revoked_at.is_none() && now < self.expires_at
    }
}

const RECORD: u8 = b't';
const PROOF: u8 = b'p';

fn key(parts: &[&[u8]]) -> Vec<u8> {
    parts.join(&b"\0"[..])
}

fn id_key(token_id: &str) -> Vec<u8> {
    key(&[b"id", token_id.as_bytes()])
}

fn hash_key(token_hash: &str) -> Vec<u8> {
    key(&[b"hash", token_hash.as_bytes()])
}

fn nonce_key(agent: &str, nonce: &str) -> Vec<u8> {
    key(&[b"nonce", agent.as_bytes(), nonce.as_bytes()])
}

fn agent_prefix(agent: &str) -> Vec<u8> {
    let mut prefix = key(&[b"agent", agent.as_bytes()]);
    prefix.push(0);
    prefix
}

fn agent_key(agent: &str, token_id: &str) -> Vec<u8> {
    let mut k = agent_prefix(agent);
    k.extend_from_slice(token_id.as_bytes());
    k
}

fn proof_key(proof_hash: &str) -> Vec<u8> {
    key(&[b"proof", proof_hash.as_bytes()])
}

/// Hex of a Unix ms time, fixed width, so keys sort by time. Times before
/// 1970 sort first.
fn time_hex(ms: i64) -> String {
    format!("{:016x}", ms.max(0) as u64)
}

fn expiry_key(retain_until: i64, kind: u8, id: &str) -> Vec<u8> {
    key(&[
        b"exp",
        time_hex(retain_until).as_bytes(),
        &[kind],
        id.as_bytes(),
    ])
}

fn decode(bytes: &[u8]) -> AtomicResult<AccessTokenRecord> {
    serde_json::from_slice(bytes).map_err(|e| format!("Corrupt access token record: {e}").into())
}

fn insert(key: Vec<u8>, val: Vec<u8>) -> Operation {
    Operation {
        tree: Tree::AccessTokens,
        method: Method::Insert,
        key,
        val: Some(val),
    }
}

fn delete(key: Vec<u8>) -> Operation {
    Operation {
        tree: Tree::AccessTokens,
        method: Method::Delete,
        key,
        val: None,
    }
}

fn record_keys(record: &AccessTokenRecord) -> [Vec<u8>; 5] {
    [
        id_key(&record.token_id),
        hash_key(&record.token_hash),
        nonce_key(&record.agent, &record.nonce),
        agent_key(&record.agent, &record.token_id),
        expiry_key(record.retain_until, RECORD, &record.token_id),
    ]
}

impl Db {
    /// Store a new or updated token record and all its index rows, in one
    /// batch. An update must not change `retain_until`, `agent`, `nonce` or
    /// `token_hash` (only `revoked_at` changes after a mint).
    pub fn put_access_token(&self, record: &AccessTokenRecord) -> AtomicResult<()> {
        let json = serde_json::to_vec(record)
            .map_err(|e| format!("Could not serialize access token record: {e}"))?;
        let [id, hash, nonce, agent, expiry] = record_keys(record);
        let token_id = record.token_id.clone().into_bytes();
        self.kv.apply_batch(&[
            insert(id, json),
            insert(hash, token_id.clone()),
            insert(nonce, token_id),
            insert(agent, Vec::new()),
            insert(expiry, Vec::new()),
        ])?;
        self.kv.flush()?;
        Ok(())
    }

    pub fn get_access_token(&self, token_id: &str) -> AtomicResult<Option<AccessTokenRecord>> {
        match self.kv.get(Tree::AccessTokens, &id_key(token_id))? {
            Some(bytes) => Ok(Some(decode(&bytes)?)),
            None => Ok(None),
        }
    }

    /// The record an index row points at, if it still exists and agrees.
    fn access_token_via(
        &self,
        index_key: &[u8],
        agrees: impl Fn(&AccessTokenRecord) -> bool,
    ) -> AtomicResult<Option<AccessTokenRecord>> {
        let Some(id) = self.kv.get(Tree::AccessTokens, index_key)? else {
            return Ok(None);
        };
        let id = String::from_utf8(id).map_err(|_| "Corrupt access token index row")?;
        Ok(self.get_access_token(&id)?.filter(agrees))
    }

    /// The record whose token hashes to `token_hash`, live or not.
    pub fn get_access_token_by_hash(
        &self,
        token_hash: &str,
    ) -> AtomicResult<Option<AccessTokenRecord>> {
        self.access_token_via(&hash_key(token_hash), |r| r.token_hash == token_hash)
    }

    /// The record `agent` minted with `nonce`, while it is retained.
    pub fn get_access_token_by_nonce(
        &self,
        agent: &str,
        nonce: &str,
    ) -> AtomicResult<Option<AccessTokenRecord>> {
        self.access_token_via(&nonce_key(agent, nonce), |r| {
            r.agent == agent && r.nonce == nonce
        })
    }

    /// Every retained record of `agent`, live or not.
    pub fn access_tokens_of(&self, agent: &str) -> AtomicResult<Vec<AccessTokenRecord>> {
        let prefix = agent_prefix(agent);
        let mut records = Vec::new();
        for row in self.kv.scan_prefix(Tree::AccessTokens, &prefix) {
            let (key, _) = row?;
            let id = std::str::from_utf8(&key[prefix.len()..])
                .map_err(|_| "Corrupt access token agent row")?;
            if let Some(record) = self.get_access_token(id)? {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Every stored record, live or not.
    pub fn all_access_tokens(&self) -> AtomicResult<Vec<AccessTokenRecord>> {
        self.kv
            .scan_prefix(Tree::AccessTokens, b"id\0")
            .map(|row| row.and_then(|(_, val)| decode(&val)))
            .collect()
    }

    /// Remove a record and all its index rows.
    pub fn remove_access_token(&self, record: &AccessTokenRecord) -> AtomicResult<()> {
        let ops: Vec<Operation> = record_keys(record).into_iter().map(delete).collect();
        self.kv.apply_batch(&ops)?;
        self.kv.flush()?;
        Ok(())
    }

    /// Record that the signed request `proof_hash` was used, until
    /// `retain_until`. `false` when it was already recorded: a replay.
    pub fn consume_access_token_proof(
        &self,
        proof_hash: &str,
        retain_until: i64,
    ) -> AtomicResult<bool> {
        if self
            .kv
            .contains_key(Tree::AccessTokens, &proof_key(proof_hash))?
        {
            return Ok(false);
        }
        self.kv.apply_batch(&[
            insert(proof_key(proof_hash), time_hex(retain_until).into_bytes()),
            insert(expiry_key(retain_until, PROOF, proof_hash), Vec::new()),
        ])?;
        self.kv.flush()?;
        Ok(true)
    }

    /// Drop every record and used proof whose retention ended at or before
    /// `now`. Reads only the expired part of the expiry index. Returns how
    /// many rows of either kind went.
    pub fn prune_access_tokens(&self, now: i64) -> AtomicResult<usize> {
        let start = b"exp\0".to_vec();
        let end = key(&[b"exp", time_hex(now.saturating_add(1)).as_bytes()]);
        let mut ops = Vec::new();
        let mut pruned = 0;
        for row in self.kv.range(Tree::AccessTokens, start, end, false) {
            let (expiry, _) = row?;
            // exp \0 <16 hex> \0 <kind> \0 <id>
            let rest = &expiry[4 + 16 + 1..];
            let (kind, id) = (rest[0], &rest[2..]);
            let id = std::str::from_utf8(id).map_err(|_| "Corrupt access token expiry row")?;
            match kind {
                RECORD => {
                    if let Some(record) = self.get_access_token(id)? {
                        ops.extend(record_keys(&record).into_iter().map(delete));
                    }
                }
                PROOF => ops.push(delete(proof_key(id))),
                _ => return Err("Corrupt access token expiry row".into()),
            }
            ops.push(delete(expiry));
            pruned += 1;
        }
        if !ops.is_empty() {
            self.kv.apply_batch(&ops)?;
            self.kv.flush()?;
        }
        Ok(pruned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, hash: &str, agent: &str, nonce: &str) -> AccessTokenRecord {
        AccessTokenRecord {
            token_id: id.into(),
            token_hash: hash.into(),
            agent: agent.into(),
            scopes: vec![StoredTokenScope {
                target: "atomic:drive".into(),
                right: "read".into(),
            }],
            label: "label".into(),
            nonce: nonce.into(),
            created_at: 1,
            expires_at: 100,
            revoked_at: None,
            retain_until: 1_000,
        }
    }

    fn rows(db: &Db) -> usize {
        db.kv.iter_tree(Tree::AccessTokens).count()
    }

    #[tokio::test]
    async fn a_token_resolves_by_its_hash_its_id_and_its_nonce() {
        let db = Db::init_temp("access_tokens_lookup").await.unwrap();
        let stored = record("t1", "aa", "atomic:agent:a", "n1");
        db.put_access_token(&stored).unwrap();

        assert_eq!(
            db.get_access_token_by_hash("aa").unwrap(),
            Some(stored.clone())
        );
        assert_eq!(db.get_access_token("t1").unwrap(), Some(stored.clone()));
        assert_eq!(
            db.get_access_token_by_nonce("atomic:agent:a", "n1")
                .unwrap(),
            Some(stored)
        );
        assert_eq!(db.get_access_token_by_hash("bb").unwrap(), None);
        assert_eq!(
            db.get_access_token_by_nonce("atomic:agent:b", "n1")
                .unwrap(),
            None,
            "a nonce is per agent"
        );
    }

    #[tokio::test]
    async fn an_agents_tokens_are_found_by_prefix_only() {
        let db = Db::init_temp("access_tokens_agent").await.unwrap();
        db.put_access_token(&record("t1", "aa", "atomic:agent:a", "n1"))
            .unwrap();
        db.put_access_token(&record("t2", "bb", "atomic:agent:a", "n2"))
            .unwrap();
        // A longer agent sharing the prefix must not be counted as `a`.
        db.put_access_token(&record("t3", "cc", "atomic:agent:ab", "n1"))
            .unwrap();

        let ids: Vec<_> = db
            .access_tokens_of("atomic:agent:a")
            .unwrap()
            .into_iter()
            .map(|r| r.token_id)
            .collect();
        assert_eq!(ids, ["t1", "t2"]);
    }

    #[tokio::test]
    async fn removing_a_token_removes_every_row() {
        let db = Db::init_temp("access_tokens_remove").await.unwrap();
        let stored = record("t1", "aa", "atomic:agent:a", "n1");
        db.put_access_token(&stored).unwrap();
        db.remove_access_token(&stored).unwrap();
        assert_eq!(rows(&db), 0);
    }

    #[tokio::test]
    async fn pruning_drops_only_what_is_past_its_retention() {
        let db = Db::init_temp("access_tokens_prune").await.unwrap();
        let mut old = record("old", "aa", "atomic:agent:a", "n1");
        old.retain_until = 500;
        let kept = record("kept", "bb", "atomic:agent:a", "n2");
        db.put_access_token(&old).unwrap();
        db.put_access_token(&kept).unwrap();
        assert!(db.consume_access_token_proof("p-old", 500).unwrap());
        assert!(db.consume_access_token_proof("p-kept", 1_000).unwrap());

        assert_eq!(db.prune_access_tokens(499).unwrap(), 0);
        assert_eq!(db.prune_access_tokens(500).unwrap(), 2);

        assert_eq!(db.get_access_token("old").unwrap(), None);
        assert_eq!(
            db.get_access_token_by_nonce("atomic:agent:a", "n1")
                .unwrap(),
            None
        );
        assert_eq!(db.get_access_token_by_hash("aa").unwrap(), None);
        assert_eq!(db.get_access_token("kept").unwrap(), Some(kept.clone()));
        assert!(
            db.consume_access_token_proof("p-old", 2_000).unwrap(),
            "pruned"
        );
        assert!(
            !db.consume_access_token_proof("p-kept", 2_000).unwrap(),
            "kept"
        );
        // kept: 5 rows; p-kept: 2 rows; p-old again: 2 rows.
        assert_eq!(rows(&db), 9);
    }

    #[tokio::test]
    async fn a_proof_is_consumed_once() {
        let db = Db::init_temp("access_tokens_proof").await.unwrap();
        assert!(db.consume_access_token_proof("p", 1_000).unwrap());
        assert!(!db.consume_access_token_proof("p", 1_000).unwrap());
    }

    #[tokio::test]
    async fn revoking_rewrites_the_record_under_the_same_keys() {
        let db = Db::init_temp("access_tokens_revoke").await.unwrap();
        let mut stored = record("t1", "aa", "atomic:agent:a", "n1");
        db.put_access_token(&stored).unwrap();
        stored.revoked_at = Some(50);
        db.put_access_token(&stored).unwrap();
        assert_eq!(rows(&db), 5);
        assert_eq!(db.get_access_token("t1").unwrap(), Some(stored));
    }

    #[test]
    fn a_revoked_or_expired_token_is_not_live() {
        let mut r = record("t1", "aa", "atomic:agent:a", "n1");
        assert!(r.is_live(99));
        assert!(
            !r.is_live(100),
            "expiry is exact: the expiry instant itself is refused"
        );
        r.revoked_at = Some(50);
        assert!(!r.is_live(60));
    }

    #[test]
    fn the_stored_record_never_holds_the_token() {
        // The field set is the contract: a token value has nowhere to go.
        let json = serde_json::to_value(record("t1", "aa", "atomic:agent:a", "n1")).unwrap();
        let keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        assert!(!keys.iter().any(|k| k == "token"), "{keys:?}");
    }
}
