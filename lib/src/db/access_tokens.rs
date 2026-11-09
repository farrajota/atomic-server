//! Storage for self-issued access tokens (`POST /tokens` on the server).
//!
//! Only a SHA-256 of each token is kept, never the token itself. Two kinds of
//! row live in [`Tree::AccessTokens`]:
//!
//! - `id/<token_id>` holds the [`AccessTokenRecord`] as JSON;
//! - `hash/<sha256 hex of the token>` holds the token id, so a bearer token
//!   resolves with two point reads and no scan.
//!
//! Both are written and removed in one batch. Nothing here is cached: every
//! lookup reads the store, so a revoke takes effect on the next request.

use super::trees::{Method, Operation, Tree};
use crate::{errors::AtomicResult, Db};
use serde::{Deserialize, Serialize};

/// One scope of a token as stored: a target subject and `read`, `write` or
/// `append`.
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
}

impl AccessTokenRecord {
    /// Whether the token may still be used at `now` (Unix ms).
    pub fn is_live(&self, now: i64) -> bool {
        self.revoked_at.is_none() && now < self.expires_at
    }
}

fn id_key(token_id: &str) -> Vec<u8> {
    format!("id/{token_id}").into_bytes()
}

fn hash_key(token_hash: &str) -> Vec<u8> {
    format!("hash/{token_hash}").into_bytes()
}

fn decode(bytes: &[u8]) -> AtomicResult<AccessTokenRecord> {
    serde_json::from_slice(bytes).map_err(|e| format!("Corrupt access token record: {e}").into())
}

impl Db {
    /// Store a new or updated token record, and its hash index row.
    pub fn put_access_token(&self, record: &AccessTokenRecord) -> AtomicResult<()> {
        let json = serde_json::to_vec(record)
            .map_err(|e| format!("Could not serialize access token record: {e}"))?;
        self.kv.apply_batch(&[
            Operation {
                tree: Tree::AccessTokens,
                method: Method::Insert,
                key: id_key(&record.token_id),
                val: Some(json),
            },
            Operation {
                tree: Tree::AccessTokens,
                method: Method::Insert,
                key: hash_key(&record.token_hash),
                val: Some(record.token_id.clone().into_bytes()),
            },
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

    /// The record whose token hashes to `token_hash`, live or not.
    pub fn get_access_token_by_hash(
        &self,
        token_hash: &str,
    ) -> AtomicResult<Option<AccessTokenRecord>> {
        let Some(id) = self.kv.get(Tree::AccessTokens, &hash_key(token_hash))? else {
            return Ok(None);
        };
        let id = String::from_utf8(id).map_err(|_| "Corrupt access token hash index row")?;
        let record = self.get_access_token(&id)?;
        // A hash row whose record is gone or belongs to another token is
        // never an answer.
        Ok(record.filter(|r| r.token_hash == token_hash))
    }

    /// Every stored record, live or not.
    pub fn all_access_tokens(&self) -> AtomicResult<Vec<AccessTokenRecord>> {
        self.kv
            .scan_prefix(Tree::AccessTokens, b"id/")
            .map(|row| row.and_then(|(_, val)| decode(&val)))
            .collect()
    }

    /// Remove a record and its hash index row.
    pub fn remove_access_token(&self, record: &AccessTokenRecord) -> AtomicResult<()> {
        self.kv.apply_batch(&[
            Operation {
                tree: Tree::AccessTokens,
                method: Method::Delete,
                key: id_key(&record.token_id),
                val: None,
            },
            Operation {
                tree: Tree::AccessTokens,
                method: Method::Delete,
                key: hash_key(&record.token_hash),
                val: None,
            },
        ])?;
        self.kv.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, hash: &str) -> AccessTokenRecord {
        AccessTokenRecord {
            token_id: id.into(),
            token_hash: hash.into(),
            agent: "atomic:agent:abc".into(),
            scopes: vec![StoredTokenScope {
                target: "atomic:drive".into(),
                right: "read".into(),
            }],
            label: "label".into(),
            nonce: "nonce".into(),
            created_at: 1,
            expires_at: 100,
            revoked_at: None,
        }
    }

    #[tokio::test]
    async fn a_token_resolves_by_its_hash_and_by_its_id() {
        let db = Db::init_temp("access_tokens_lookup").await.unwrap();
        let stored = record("t1", "aa");
        db.put_access_token(&stored).unwrap();

        assert_eq!(
            db.get_access_token_by_hash("aa").unwrap(),
            Some(stored.clone())
        );
        assert_eq!(db.get_access_token("t1").unwrap(), Some(stored));
        assert_eq!(db.get_access_token_by_hash("bb").unwrap(), None);
    }

    #[tokio::test]
    async fn removing_a_token_removes_its_hash_row() {
        let db = Db::init_temp("access_tokens_remove").await.unwrap();
        let stored = record("t1", "aa");
        db.put_access_token(&stored).unwrap();
        db.remove_access_token(&stored).unwrap();

        assert_eq!(db.get_access_token_by_hash("aa").unwrap(), None);
        assert!(db.all_access_tokens().unwrap().is_empty());
    }

    #[test]
    fn a_revoked_or_expired_token_is_not_live() {
        let mut r = record("t1", "aa");
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
        let json = serde_json::to_value(record("t1", "aa")).unwrap();
        let keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        assert!(!keys.iter().any(|k| k == "token"), "{keys:?}");
    }
}
