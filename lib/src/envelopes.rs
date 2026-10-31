//! Signed commit envelopes kept per resource.
//!
//! Authorization is decided on state (`read` / `write` / `parent` in the
//! projection); the Loro oplog is the history of *what* changed and when. What
//! neither carries is *who signed the state you are looking at*: the oplog's
//! change messages are opaque drain tokens, and `lastCommit` is only an id.
//! This tree keeps the signed JSON-AD of the commits that produced a resource,
//! so any node holding it can re-verify the signature and attribute the state,
//! offline. See `planning/completed/commit-retention-floor-decision.md`
//! (F6 latest envelope, F7 every envelope).
//!
//! Layout ([`Tree::Envelopes`]): key
//! `pure_id || 0x00 || createdAt (u64 BE) || 0x00 || signature`, value the
//! commit JSON-AD exactly as `/commit` or the `COMMIT` frame accepted it. A
//! prefix scan on the pure id lists a resource's envelopes in time order. The
//! rows are not resources and not indexed: they never show up in queries,
//! `all_resources`, search or collections, so nothing has to filter
//! `did:ad:commit:` subjects by hand.
//!
//! How many rows survive is [`EnvelopeRetention`]: `Latest` keeps the one
//! that produced the current state (the floor), `All` keeps every envelope
//! and turns the oplog into a signed audit log ([`attribute_history`]).
//!
//! What is deliberately not here: envelopes inside the Loro doc (an envelope
//! would then sign a document containing itself), and a retention schedule
//! beyond the two settings. Replication of these rows is the sync layer's
//! job (`planning/auditability-loro-history.md`).

use crate::db::trees::{Method, Operation, Transaction, Tree};
use crate::errors::AtomicResult;
use crate::storelike::Storelike;
use crate::{commit::CommitResponse, Db};
use sha2::Digest;

/// Which envelopes a node keeps per resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EnvelopeRetention {
    /// One row per resource: the envelope that produced the current state.
    /// Attribution of the current state stays verifiable; older edits are
    /// visible in the Loro oplog but unattributed.
    #[default]
    Latest,
    /// Every envelope. Each Loro change maps back to the signed commit that
    /// introduced it, so History can show a verified signer per version.
    All,
}

impl EnvelopeRetention {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "latest" | "compact" => Some(Self::Latest),
            "all" | "full" | "audit" => Some(Self::All),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Latest => "latest",
            Self::All => "all",
        }
    }
}

/// One retained envelope, decoded from its key. `json` is the signed body.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StoredEnvelope {
    /// Pure id of the resource the commit is about.
    pub subject: String,
    /// Commit `createdAt`, Unix milliseconds.
    pub created_at: i64,
    pub signature: String,
    /// The commit's JSON-AD exactly as accepted.
    pub json: String,
}

impl StoredEnvelope {
    /// The commit id this envelope is stored under (`did:ad:commit:<sig>`),
    /// The same value surfaced as the resource's derived `lastCommit`.
    pub fn commit_id(&self) -> String {
        crate::identifiers::commit_subject(&self.signature)
    }

    /// Whether this envelope is a destroy.
    pub fn is_destroy(&self) -> bool {
        serde_json::from_str::<serde_json::Value>(&self.json)
            .ok()
            .and_then(|v| v.get(crate::urls::DESTROY).and_then(|d| d.as_bool()))
            .unwrap_or(false)
    }
}

fn prefix(subject: &str) -> Vec<u8> {
    let pure = crate::Subject::from_raw(subject, None).pure_id();
    let mut key = Vec::with_capacity(pure.len() + 1);
    key.extend_from_slice(pure.as_bytes());
    key.push(0);
    key
}

fn key(subject: &str, created_at: i64, signature: &str) -> Vec<u8> {
    let mut key = prefix(subject);
    key.extend_from_slice(&(created_at.max(0) as u64).to_be_bytes());
    key.push(0);
    key.extend_from_slice(signature.as_bytes());
    key
}

/// The resource, `createdAt` and signature a row key encodes.
fn parse_key(key: &[u8]) -> Option<(String, i64, String)> {
    let subject_end = key.iter().position(|b| *b == 0)?;
    let subject = std::str::from_utf8(&key[..subject_end]).ok()?.to_string();
    let rest = &key[subject_end + 1..];
    if rest.len() < 9 || rest[8] != 0 {
        return None;
    }
    let created_at = u64::from_be_bytes(rest[..8].try_into().ok()?) as i64;
    let signature = std::str::from_utf8(&rest[9..]).ok()?.to_string();
    Some((subject, created_at, signature))
}

/// What a stored row turned into.
enum Decoded {
    Row(StoredEnvelope),
    /// A row that is neither whole JSON nor a well-formed compact row.
    Unreadable,
}

fn decode(key: &[u8], value: Vec<u8>) -> Option<Decoded> {
    let (subject, created_at, signature) = parse_key(key)?;
    let json = match value.first() {
        Some(&COMPACT_V1) => match rebuild_json(&subject, created_at, &signature, &value) {
            Some(json) => json,
            None => return Some(Decoded::Unreadable),
        },
        _ => String::from_utf8(value).ok()?,
    };
    Some(Decoded::Row(StoredEnvelope {
        subject,
        created_at,
        signature,
        json,
    }))
}

// A compact row keeps what the key does not hold, and nothing is derived from
// the Loro doc: the signed `loroUpdate` bytes are stored as they were signed.
//
//   0x01 | has_previous | signer[32] | [previousCommit[64]] | loroUpdate bytes
//
// `signer` and `previousCommit` are raw bytes of the usual `atomic:agent:` /
// `atomic:commit:` ids; a commit with any other shape is stored as JSON. A
// legacy row is the commit JSON and starts with `{`, so the tag byte tells
// them apart.
const COMPACT_V1: u8 = 1;
const AGENT_PREFIX: &str = "atomic:agent:";

/// `value` as raw bytes when it is `prefix` plus the unpadded base64url form
/// of `len` bytes, and re-encodes to exactly that.
fn raw_id(value: &str, prefix: &str, len: usize) -> Option<Vec<u8>> {
    use base64::Engine;
    let body = value.strip_prefix(prefix)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body)
        .ok()?;
    (raw.len() == len && base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&raw) == body)
        .then_some(raw)
}

fn id_from_raw(prefix: &str, raw: &[u8]) -> String {
    use base64::Engine;
    format!(
        "{prefix}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
    )
}

/// The commit JSON-AD for a compact row, in the key order and spelling
/// `Resource::to_json_ad` writes. Needs nothing but the row and its key.
fn rebuild_json(subject: &str, created_at: i64, signature: &str, row: &[u8]) -> Option<String> {
    use base64::Engine;
    let (&[tag, has_previous], rest) = row.split_first_chunk::<2>()?;
    if tag != COMPACT_V1 || has_previous > 1 {
        return None;
    }
    let (signer, rest) = rest.split_at_checked(32)?;
    let (previous, update) = if has_previous == 1 {
        let (previous, update) = rest.split_at_checked(64)?;
        (
            Some(id_from_raw(
                crate::identifiers::ATOMIC_COMMIT_PREFIX,
                previous,
            )),
            update,
        )
    } else {
        (None, rest)
    };
    let mut map = serde_json::Map::new();
    map.insert(
        "@id".into(),
        crate::identifiers::commit_subject(signature).into(),
    );
    map.insert(crate::urls::CREATED_AT.into(), created_at.into());
    map.insert(
        crate::urls::IS_A.into(),
        serde_json::json!([crate::urls::COMMIT]),
    );
    map.insert(
        crate::urls::LORO_UPDATE.into(),
        base64::engine::general_purpose::STANDARD
            .encode(update)
            .into(),
    );
    if let Some(previous) = previous {
        map.insert(crate::urls::PREVIOUS_COMMIT.into(), previous.into());
    }
    map.insert(crate::urls::SIGNATURE.into(), signature.into());
    map.insert(
        crate::urls::SIGNER.into(),
        id_from_raw(AGENT_PREFIX, signer).into(),
    );
    map.insert(crate::urls::SUBJECT.into(), subject.into());
    serde_json::to_string(&map).ok()
}

/// The compact row for a commit, when the row alone rebuilds its JSON byte
/// for byte. Anything else (an unknown field, odd id shapes, another spelling)
/// is `None`, and the caller stores the full JSON.
fn compact_row(subject: &str, created_at: i64, signature: &str, json: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let map = value.as_object()?;
    let text = |key: &str| map.get(key).and_then(|v| v.as_str());
    let update = base64::engine::general_purpose::STANDARD
        .decode(text(crate::urls::LORO_UPDATE)?)
        .ok()?;
    let previous = match text(crate::urls::PREVIOUS_COMMIT) {
        Some(p) => Some(raw_id(p, crate::identifiers::ATOMIC_COMMIT_PREFIX, 64)?),
        None => None,
    };
    let mut row = vec![COMPACT_V1, u8::from(previous.is_some())];
    row.extend(raw_id(text(crate::urls::SIGNER)?, AGENT_PREFIX, 32)?);
    row.extend(previous.into_iter().flatten());
    row.extend(update);
    (rebuild_json(subject, created_at, signature, &row).as_deref() == Some(json)).then_some(row)
}

/// The value to store for a commit envelope: the compact row when it rebuilds
/// the JSON exactly, the commit JSON otherwise.
fn value_for(subject: &str, created_at: i64, signature: &str, json: String) -> Vec<u8> {
    let pure = crate::Subject::from_raw(subject, None).pure_id();
    compact_row(&pure, created_at, signature, &json).unwrap_or_else(|| json.into_bytes())
}

/// Most ancestors [`resolve_retention`] looks at. A parent chain is a few
/// levels deep; the bound only keeps a corrupt cycle from looping.
const MAX_PARENT_DEPTH: usize = 32;

/// What a resource's `retention` property, and that of its ancestors, add up to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    /// The resource or an ancestor says `audit`.
    Audit,
    /// The whole chain was read and none says `audit`: the node's setting applies.
    Compact,
    /// An ancestor could not be read (not held here yet), so an `audit` above
    /// it may exist. Callers that cannot undo a drop treat it as `Audit`.
    Unknown,
}

/// Reads the resource and its parent chain (propvals only, one shallow read
/// per level) and says whether any of them asks for `audit`. Audit only ever
/// wins: a `compact` on a child cannot lower an `audit` above it, so a setting
/// made on a folder holds for everything below it.
///
/// `resource` is the state a commit produced, or the last state for a destroy;
/// `None` is [`Resolved::Unknown`].
pub fn resolve_retention(store: &Db, resource: Option<&crate::Resource>) -> Resolved {
    let Some(start) = resource else {
        return Resolved::Unknown;
    };
    if is_audit(start) {
        return Resolved::Audit;
    }
    let mut parent = parent_of(start);
    for _ in 0..MAX_PARENT_DEPTH {
        let Some(subject) = parent else {
            return Resolved::Compact;
        };
        let Ok(ancestor) = store.get_resource_shallow(&subject) else {
            return Resolved::Unknown;
        };
        if is_audit(&ancestor) {
            return Resolved::Audit;
        }
        parent = parent_of(&ancestor);
    }
    Resolved::Compact
}

/// The retention that applies to a resource: the node's setting, raised to
/// `All` when the resource or an ancestor asks for `audit`. An unreadable
/// ancestor counts as the node's setting here; see [`Resolved::Unknown`].
pub fn effective_retention(store: &Db, resource: Option<&crate::Resource>) -> EnvelopeRetention {
    if store.envelope_retention() == EnvelopeRetention::All {
        return EnvelopeRetention::All;
    }
    match resolve_retention(store, resource) {
        Resolved::Audit => EnvelopeRetention::All,
        Resolved::Compact | Resolved::Unknown => EnvelopeRetention::Latest,
    }
}

/// True for the two values a `retention` property may hold.
pub fn is_valid_retention_value(value: &str) -> bool {
    matches!(value, "compact" | "audit")
}

/// Refuses a `retention` value that is neither `compact` nor `audit`, so a
/// typo cannot silently mean "no audit".
pub fn check_retention_value(resource: &crate::Resource) -> AtomicResult<()> {
    match resource.get(crate::urls::RETENTION) {
        Ok(value) if !is_valid_retention_value(&value.to_string()) => Err(format!(
            "`retention` on {} must be `compact` or `audit`, got `{}`",
            resource.get_subject(),
            value
        )
        .into()),
        _ => Ok(()),
    }
}

fn is_audit(resource: &crate::Resource) -> bool {
    resource
        .get(crate::urls::RETENTION)
        .ok()
        .is_some_and(|value| value.to_string() == "audit")
}

fn parent_of(resource: &crate::Resource) -> Option<crate::Subject> {
    let parent = resource.get(crate::urls::PARENT).ok()?.to_string();
    (!parent.is_empty()).then(|| crate::Subject::from(parent.as_str()))
}

/// Queue the writes that keep this commit's envelope, honouring the store's
/// retention. Appended to the apply transaction so the envelope lands with
/// the state it signs, or not at all. Unsigned commits (internal writes)
/// have nothing to keep.
pub fn record_ops(
    store: &Db,
    response: &CommitResponse,
    transaction: &mut Transaction,
) -> AtomicResult<()> {
    let Some(signature) = response.commit.signature.as_deref() else {
        return Ok(());
    };
    let subject = response.commit.subject.as_str();
    let json = response.commit_resource.to_json_ad(None)?;
    let new_key = key(subject, response.commit.created_at, signature);

    // Keep each envelope's operation spans alongside it. The genesis spans
    // stay even when its envelope is rebuilt from the commit row. Only the
    // first apply is recorded: a replay of the same envelope adds no ops,
    // and anyone who saw a signed commit can resend it, so it must not
    // overwrite what the first apply introduced.
    let already_recorded = store.kv.contains_key(Tree::EnvelopeSpans, &new_key)?;
    if let (Some(spans), false) = (&response.change_spans, already_recorded) {
        transaction.push(Operation {
            tree: Tree::EnvelopeSpans,
            method: Method::Insert,
            key: new_key.clone(),
            val: Some(serde_json::to_vec(spans)?),
        });
    }

    if genesis_is_kept_as_row(response) {
        return Ok(());
    }
    // A commit that takes audit away (clears `retention`, or moves the
    // resource out of an audited folder) is still written under audit: the
    // state before it asked for the full log, so its envelope and spans join
    // it and the history written so far is kept. Only commits after it prune:
    // each drops the envelope it supersedes and its spans, except genesis spans.
    // A state whose ancestor chain cannot be read might be audited, so it
    // counts as audited too: nothing is dropped on a guess.
    let audited = store.envelope_retention() == EnvelopeRetention::All
        || [
            response.resource_new.as_ref(),
            response.resource_old.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(|state| resolve_retention(store, Some(state)) != Resolved::Compact);
    // Later commits: under audit the genesis envelope is history that has to
    // stay, so write it out before something newer sits beside it.
    if audited {
        let nothing_stored = store
            .kv
            .scan_prefix(Tree::Envelopes, &prefix(subject))
            .next()
            .is_none();
        if nothing_stored {
            if let Some(genesis) = genesis_envelope_from_row(store, subject) {
                transaction.push(Operation {
                    tree: Tree::Envelopes,
                    method: Method::Insert,
                    key: key(subject, genesis.created_at, &genesis.signature),
                    val: Some(genesis.json.into_bytes()),
                });
            }
        }
    }

    if !audited {
        // The one envelope this one supersedes. Rows written under audit,
        // before a lowering, stay with their operation spans.
        let superseded = store
            .kv
            .scan_prefix(Tree::Envelopes, &prefix(subject))
            .last();
        if let Some(existing) = superseded {
            let (old_key, old_val) = existing?;
            // A newest row that cannot be read is not the one to supersede:
            // it stays until someone can say what it was.
            if old_key != new_key {
                if let Some(Decoded::Row(row)) = decode(&old_key, old_val) {
                    let is_genesis = Some(&row.signature) == genesis_signature(subject).as_ref();
                    transaction.push(Operation {
                        tree: Tree::Envelopes,
                        method: Method::Delete,
                        key: old_key.clone(),
                        val: None,
                    });
                    if !is_genesis {
                        transaction.push(Operation {
                            tree: Tree::EnvelopeSpans,
                            method: Method::Delete,
                            key: old_key,
                            val: None,
                        });
                    }
                }
            }
        }
    }

    let val = value_for(subject, response.commit.created_at, signature, json);
    transaction.push(Operation {
        tree: Tree::Envelopes,
        method: Method::Insert,
        key: new_key,
        val: Some(val),
    });
    Ok(())
}

/// The op spans recorded when this envelope was applied here. `None` for an
/// envelope this node did not apply itself (it arrived with a bulk push or a
/// vault pack), or one applied before spans were recorded.
pub fn recorded_spans(store: &Db, envelope: &StoredEnvelope) -> Option<crate::loro::EnvelopeSpans> {
    let row = store
        .kv
        .get(
            Tree::EnvelopeSpans,
            &key(&envelope.subject, envelope.created_at, &envelope.signature),
        )
        .ok()
        .flatten()?;
    serde_json::from_slice(&row).ok()
}

/// What a scan of a resource's envelope rows found.
pub struct Retained {
    /// Readable envelopes, oldest first.
    pub rows: Vec<StoredEnvelope>,
    /// Rows that exist but cannot be decoded. They are not in `rows`; a
    /// caller that ships or prunes must account for them.
    pub unreadable: usize,
    /// Whether the newest row (the one that produced the current state) is
    /// one of the unreadable ones.
    pub newest_unreadable: bool,
}

/// Every retained envelope of a resource, oldest first, and what could not be
/// read.
pub fn envelopes_checked(store: &Db, subject: &str) -> Retained {
    let mut out = Retained {
        rows: Vec::new(),
        unreadable: 0,
        newest_unreadable: false,
    };
    for (k, v) in store
        .kv
        .scan_prefix(Tree::Envelopes, &prefix(subject))
        .filter_map(|entry| entry.ok())
    {
        match decode(&k, v) {
            Some(Decoded::Row(row)) => {
                out.rows.push(row);
                out.newest_unreadable = false;
            }
            Some(Decoded::Unreadable) | None => {
                out.unreadable += 1;
                out.newest_unreadable = true;
            }
        }
    }
    if out.rows.is_empty() && out.unreadable == 0 {
        out.rows.extend(genesis_envelope_from_row(store, subject));
    }
    out
}

/// Readable envelopes only, oldest first. Unreadable rows are logged, not
/// returned; use [`envelopes_checked`] where their absence changes the answer.
pub fn envelopes(store: &Db, subject: &str) -> Vec<StoredEnvelope> {
    let found = envelopes_checked(store, subject);
    if found.unreadable > 0 {
        tracing::warn!(
            "{subject}: {} kept commit row(s) could not be read",
            found.unreadable
        );
    }
    found.rows
}

/// The signature that names `subject`, when it is a resource whose id was
/// derived from its genesis commit.
fn genesis_signature(subject: &str) -> Option<String> {
    let pure = crate::Subject::from_raw(subject, None).pure_id();
    let canonical = crate::identifiers::canonicalize_scheme(&pure);
    if !crate::identifiers::is_resource_id(&canonical) {
        return None;
    }
    crate::identifiers::identifier_body(&canonical).map(str::to_string)
}

/// A resource's genesis commit is stored as a row of its own (the durable
/// record, see `apply_commit`), so its envelope would hold the same signed
/// bytes a second time: about 2.4 KB for every chat message. It is not
/// written. While nothing newer has replaced it, this rebuilds the envelope
/// from that row: the row is the commit resource the envelope was written
/// from, so the JSON is the same.
fn genesis_envelope_from_row(store: &Db, subject: &str) -> Option<StoredEnvelope> {
    let signature = genesis_signature(subject)?;
    let commit_id = crate::identifiers::commit_subject(&signature);
    let propvals = store.get_propvals(&commit_id).ok()?;
    let row = crate::Resource::from_propvals(propvals, crate::Subject::from_raw(&commit_id, None));
    let created_at = row.get(crate::urls::CREATED_AT).ok()?.to_int().ok()?;
    let json = row.to_json_ad(None).ok()?;
    Some(StoredEnvelope {
        subject: subject.to_string(),
        created_at,
        signature,
        json,
    })
}

/// Whether `response` is the genesis commit of its resource, whose
/// `Tree::Resources` row already keeps it.
fn genesis_is_kept_as_row(response: &CommitResponse) -> bool {
    let Some(signature) = response.commit.signature.as_deref() else {
        return false;
    };
    response.auth_impact().genesis
        && response.resource_new.is_some()
        && genesis_signature(response.commit.subject.as_str()).as_deref() == Some(signature)
}

/// The envelope that produced the resource's current state, if kept.
/// `None` when that newest row cannot be read: an older row would be the
/// wrong answer.
pub fn latest_envelope(store: &Db, subject: &str) -> Option<StoredEnvelope> {
    let found = envelopes_checked(store, subject);
    if found.newest_unreadable {
        tracing::warn!("{subject}: the newest kept commit row could not be read");
        return None;
    }
    found.rows.into_iter().last()
}

/// The commit id of the newest retained envelope, read from the row key
/// alone (no row is decoded). The id `lastCommit` is served as when the
/// stored row does not carry it.
pub fn latest_commit_id(store: &Db, subject: &str) -> Option<String> {
    let (key, _) = store
        .kv
        .scan_prefix(Tree::Envelopes, &prefix(subject))
        .filter_map(|entry| entry.ok())
        .last()?;
    let (_, _, signature) = parse_key(&key)?;
    Some(crate::identifiers::commit_subject(&signature))
}

/// The id of the genesis commit of a resource whose id was derived from it
/// (its row is kept; see [`genesis_envelope_from_row`]).
pub fn genesis_commit_id(subject: &str) -> Option<String> {
    genesis_signature(subject).map(|signature| crate::identifiers::commit_subject(&signature))
}

/// The retained envelopes of each subject as commit JSON-AD, for a bulk
/// push or a vault pack. Subjects with none are absent. A subject with rows
/// that cannot be read ships the rest and logs a warning.
pub fn for_subjects<'a>(
    store: &Db,
    subjects: impl IntoIterator<Item = &'a str>,
) -> std::collections::HashMap<String, Vec<String>> {
    let mut out = std::collections::HashMap::new();
    for subject in subjects {
        let rows: Vec<String> = envelopes(store, subject)
            .into_iter()
            .map(|e| e.json)
            .collect();
        if !rows.is_empty() {
            out.insert(subject.to_string(), rows);
        }
    }
    out
}

/// Keep an envelope that arrived with a bulk push or a vault pack.
///
/// It is verified the way apply verifies a commit (the signature, by the
/// signer the commit names) and must be about `expected_subject`, the entry
/// it travelled with; otherwise nothing is written. Retention applies as for
/// a local commit: on `latest` only the newest envelope by `createdAt`
/// survives, which may be one already here. Idempotent for a row already
/// stored.
pub async fn import_envelope(store: &Db, expected_subject: &str, json: &str) -> AtomicResult<()> {
    let resource = crate::parse::parse_json_ad_commit_resource(json, store).await?;
    let commit = crate::commit::Commit::from_resource(resource)?;
    let expected = crate::Subject::from_raw(expected_subject, None).pure_id();
    let actual = commit.subject.pure_id();
    if actual != expected {
        return Err(format!("envelope is about {actual}, not {expected}").into());
    }
    commit.validate_signature(store).await?;
    let signature = commit
        .signature
        .as_deref()
        .ok_or("envelope has no signature")?;
    let new_key = key(commit.subject.as_str(), commit.created_at, signature);
    if store.kv.contains_key(Tree::Envelopes, &new_key)? {
        return Ok(());
    }

    let mut ops = Vec::new();
    let local = store.get_resource_shallow(&commit.subject).ok();
    // An ancestor that is not held yet may be the one asking for audit, and a
    // dropped envelope cannot be fetched again, so only a fully read chain
    // allows pruning.
    let may_prune = store.envelope_retention() == EnvelopeRetention::Latest
        && resolve_retention(store, local.as_ref()) == Resolved::Compact;
    if may_prune {
        let found = envelopes_checked(store, commit.subject.as_str());
        let existing = found.rows;
        if found.newest_unreadable {
            // The newest row cannot be compared or safely superseded: keep
            // it and add the incoming one beside it.
            tracing::warn!(
                "{}: newest kept commit row unreadable, importing without pruning",
                commit.subject.as_str()
            );
        } else if existing.iter().any(|e| e.created_at > commit.created_at) {
            // A newer envelope is already the latest; the incoming one is
            // history this node chose not to keep.
            return Ok(());
        }
        if let Some(old) = existing.last().filter(|_| !found.newest_unreadable) {
            let old_key = key(&old.subject, old.created_at, &old.signature);
            let genesis = genesis_signature(commit.subject.as_str());
            if Some(&old.signature) != genesis.as_ref() {
                ops.push(Operation {
                    tree: Tree::EnvelopeSpans,
                    method: Method::Delete,
                    key: old_key.clone(),
                    val: None,
                });
            }
            ops.push(Operation {
                tree: Tree::Envelopes,
                method: Method::Delete,
                key: old_key,
                val: None,
            });
        }
    }
    ops.push(Operation {
        tree: Tree::Envelopes,
        method: Method::Insert,
        key: new_key,
        val: Some(value_for(
            commit.subject.as_str(),
            commit.created_at,
            signature,
            json.to_string(),
        )),
    });
    store.kv.apply_batch(&ops)
}

/// Drop every retained envelope of a resource. Not called on destroy: the
/// destroy envelope is the proof a peer needs (`SYNC_DIFF.removeCommits`).
pub fn clear_envelopes(store: &Db, subject: &str) {
    for tree in [Tree::Envelopes, Tree::EnvelopeSpans] {
        for (k, _) in store.kv.scan_prefix(tree, &prefix(subject)).flatten() {
            let _ = store.kv.remove(tree, &k);
        }
    }
}

/// One signed change, as History shows it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Attribution {
    pub signer: String,
    /// Commit `createdAt`, Unix milliseconds.
    pub created_at: i64,
    pub signature: String,
    /// `did:ad:commit:<signature>`, surfaced as the derived `lastCommit`.
    pub commit_id: String,
    /// The signature checks out against the signer's key on this node.
    pub verified: bool,
    /// Messages of the Loro changes this envelope introduced. A display hint
    /// kept for older readers: messages are chosen by the client, so two
    /// envelopes can list the same one. Attribution is by [`Self::spans`]
    /// and [`HistoryAttribution::changes`], never by message.
    pub tokens: Vec<String>,
    pub destroy: bool,
    pub genesis: bool,
    /// The Loro op IDs this envelope's own update added to the stored
    /// document when this node applied it. `None` when this node did not
    /// record them (the envelope came with a bulk push or vault pack): its
    /// ops are then unattributed here.
    pub spans: Option<Vec<crate::loro::ChangeSpan>>,
    /// Ops the server wrote while applying this envelope, such as a derived
    /// `drive` value or creator's `write` grant; these are not signer-authored.
    pub server_spans: Option<Vec<crate::loro::ChangeSpan>>,
}

/// Where a Loro change in the stored document came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeOrigin {
    /// Every op was introduced by one verified envelope: its signer made it.
    Signed,
    /// Every op was introduced by one envelope whose signature does not
    /// verify on this node.
    Unverified,
    /// Every op was written by this server while applying envelopes.
    Server,
    /// Some op is covered by no recorded envelope (an edit that did not come
    /// through a signed commit here, history from before spans were
    /// recorded, or an envelope this node did not apply itself).
    Unattributed,
    /// The ops are covered, but by more than one source.
    Ambiguous,
}

/// One Loro change of the stored document and who it is attributed to.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChangeAttribution {
    #[serde(with = "crate::loro::peer_id_string")]
    pub peer: u64,
    pub counter: i32,
    pub length: u32,
    pub lamport: u32,
    /// Change timestamp, Unix milliseconds (0 if unrecorded).
    pub timestamp: i64,
    /// The client-chosen change message. Not evidence of anything.
    pub message: Option<String>,
    pub origin: ChangeOrigin,
    /// Index into [`HistoryAttribution::attributions`] of the envelope that
    /// introduced the change, for `signed` and `unverified`.
    pub attribution: Option<usize>,
    /// The signer of that envelope, for `signed` and `unverified`.
    pub signer: Option<String>,
}

/// What this node can say about who signed a resource's history.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HistoryAttribution {
    pub subject: String,
    /// Retention that applies to this resource (the node's setting, raised
    /// by a `retention: audit` on it or an ancestor); tells a reader whether
    /// missing attributions are a gap or a policy.
    pub retention: &'static str,
    /// How changes are mapped to signers: `"change-ids"`, the Loro op IDs
    /// each envelope introduced when it was applied here.
    pub attribution_source: &'static str,
    /// Oldest first.
    pub attributions: Vec<Attribution>,
    /// Every change of the stored document, oldest (lowest Lamport) first.
    pub changes: Vec<ChangeAttribution>,
    /// Every change in the stored document is `signed` (by a verified
    /// envelope) or `server` bookkeeping. `false` as soon as one is not, and
    /// while the subject is destroyed or nothing is retained.
    pub complete: bool,
    /// Retained envelopes that could not be rebuilt from their compact row
    /// (the stored Loro doc is gone or no longer reproduces the signed
    /// update). They are not in `attributions`, and `complete` is `false`
    /// while any exist.
    pub unreadable: usize,
}

/// Verify the retained envelopes of a resource and map them onto its Loro
/// history by op ID.
///
/// When this node applies a signed commit it records which op IDs the
/// commit's update added to the stored document and which ops the server
/// wrote on top ([`crate::loro::EnvelopeSpans`]). Each change of the stored
/// document is attributed to the one envelope whose recorded spans cover all
/// of its ops; nothing a client puts in a change (its message, its peer ID)
/// decides who made it. Each envelope's signature is checked with the same
/// code apply uses. Anything not covered is unattributed, never a guessed
/// signer.
pub async fn attribute_history(store: &Db, subject: &str) -> AtomicResult<HistoryAttribution> {
    let retention = effective_retention(
        store,
        store
            .get_resource_shallow(&crate::Subject::from(subject))
            .ok()
            .as_ref(),
    )
    .as_str();
    let mut attributions: Vec<Attribution> = Vec::new();
    let pure = crate::Subject::from_raw(subject, None).pure_id();
    let stored_doc = store
        .kv
        .get(Tree::LoroSnapshots, pure.as_bytes())
        .ok()
        .flatten()
        .and_then(|bytes| crate::loro::AtomicLoroDoc::from_snapshot(&bytes).ok());
    let stored_changes = stored_doc
        .as_ref()
        .map(|doc| doc.changes())
        .unwrap_or_default();

    // Under `latest` retention the genesis envelope is no longer a row once
    // an edit replaced it, but its commit row and its spans are kept: the
    // genesis is still attributable.
    let mut found = envelopes_checked(store, subject);
    if let Some(genesis) = genesis_envelope_from_row(store, subject) {
        if !found.rows.iter().any(|e| e.signature == genesis.signature) {
            found.rows.insert(0, genesis);
        }
    }
    let Retained {
        rows: retained,
        unreadable,
        ..
    } = found;
    for envelope in retained {
        let resource = crate::parse::parse_json_ad_commit_resource(&envelope.json, store).await?;
        let commit = crate::commit::Commit::from_resource(resource)?;
        let verified = commit.validate_signature(store).await.is_ok();
        let spans = recorded_spans(store, &envelope);
        let tokens = match &spans {
            Some(spans) => messages_within(&stored_changes, &spans.client),
            None => Vec::new(),
        };
        attributions.push(Attribution {
            signer: commit.signer.to_string(),
            created_at: commit.created_at,
            commit_id: envelope.commit_id(),
            signature: envelope.signature.clone(),
            verified,
            tokens,
            destroy: commit.destroy.unwrap_or(false),
            genesis: commit.is_genesis == Some(true),
            server_spans: spans.as_ref().map(|s| s.server.clone()),
            spans: spans.map(|s| s.client),
        });
    }

    let changes = classify_changes(&stored_changes, &attributions);
    let complete = stored_doc.is_some()
        && !attributions.is_empty()
        && unreadable == 0
        && changes
            .iter()
            .all(|c| matches!(c.origin, ChangeOrigin::Signed | ChangeOrigin::Server));

    Ok(HistoryAttribution {
        subject: pure,
        retention,
        attribution_source: "change-ids",
        attributions,
        changes,
        complete,
        unreadable,
    })
}

/// Who wrote a run of ops: an envelope's signer (by index) or the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Envelope(usize),
    Server,
}

/// Attribute each change to the source whose recorded spans cover all of its
/// ops. A change covered only in part, or by more than one source, is not
/// attributed to anyone.
fn classify_changes(
    changes: &[crate::loro::ChangeInfo],
    attributions: &[Attribution],
) -> Vec<ChangeAttribution> {
    let mut covering: Vec<(crate::loro::ChangeSpan, Source)> = Vec::new();
    for (index, attribution) in attributions.iter().enumerate() {
        for span in attribution.spans.iter().flatten() {
            covering.push((*span, Source::Envelope(index)));
        }
        for span in attribution.server_spans.iter().flatten() {
            covering.push((*span, Source::Server));
        }
    }

    changes
        .iter()
        .map(|change| {
            let start = change.counter;
            let end = change.counter + change.len as i32;
            let mut pieces: Vec<(i32, i32, Source)> = covering
                .iter()
                .filter(|(span, _)| span.peer == change.peer)
                .filter_map(|(span, source)| {
                    let from = span.counter.max(start);
                    let to = span.end().min(end);
                    (to > from).then_some((from, to, *source))
                })
                .collect();
            pieces.sort_by_key(|(from, _, _)| *from);

            let mut cursor = start;
            let mut overlaps = false;
            for (from, to, _) in &pieces {
                if *from > cursor {
                    break;
                }
                if *from < cursor {
                    overlaps = true;
                }
                cursor = cursor.max(*to);
            }
            let covered = cursor >= end;
            let mut sources: Vec<Source> = pieces.iter().map(|(_, _, s)| *s).collect();
            sources.dedup();
            let single = sources.windows(2).all(|w| w[0] == w[1]);

            let (origin, attribution) = if !covered {
                (ChangeOrigin::Unattributed, None)
            } else if overlaps || !single {
                (ChangeOrigin::Ambiguous, None)
            } else {
                match sources.first() {
                    Some(Source::Server) => (ChangeOrigin::Server, None),
                    Some(Source::Envelope(index)) if attributions[*index].verified => {
                        (ChangeOrigin::Signed, Some(*index))
                    }
                    Some(Source::Envelope(index)) => (ChangeOrigin::Unverified, Some(*index)),
                    None => (ChangeOrigin::Unattributed, None),
                }
            };
            ChangeAttribution {
                peer: change.peer,
                counter: change.counter,
                length: change.len as u32,
                lamport: change.lamport,
                timestamp: crate::loro::normalize_change_timestamp_ms(change.timestamp),
                message: change.message.clone(),
                origin,
                signer: attribution.map(|index| attributions[index].signer.clone()),
                attribution,
            }
        })
        .collect()
}

/// Messages of the changes whose ops all lie inside `spans`.
fn messages_within(
    changes: &[crate::loro::ChangeInfo],
    spans: &[crate::loro::ChangeSpan],
) -> Vec<String> {
    let mut messages = Vec::new();
    for change in changes {
        let start = change.counter;
        let end = change.counter + change.len as i32;
        let inside = spans
            .iter()
            .any(|s| s.peer == change.peer && s.counter <= start && s.end() >= end);
        if let (true, Some(message)) = (inside, change.message.as_ref()) {
            if !messages.contains(message) {
                messages.push(message.clone());
            }
        }
    }
    messages
}

#[derive(Debug)]
pub(crate) struct RecoveryHistoryAttribution {
    pub retention: &'static str,
    pub attributions: Vec<Attribution>,
    pub complete: bool,
}

/// Recovery accepts only the native commit envelope schema before the general parser runs.
pub(crate) fn check_recovery_envelope_fields(store: &Db, json: &str) -> AtomicResult<()> {
    let object: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(json).map_err(|_| "Invalid recovery envelope JSON")?;
    for (key, value) in &object {
        if ![
            "@id",
            crate::urls::LOCAL_ID,
            crate::urls::IS_A,
            crate::urls::SUBJECT,
            crate::urls::CREATED_AT,
            crate::urls::SIGNER,
            crate::urls::LORO_UPDATE,
            crate::urls::DESTROY,
            crate::urls::SIGNATURE,
            crate::urls::PREVIOUS_COMMIT,
            crate::urls::IS_GENESIS,
        ]
        .contains(&key.as_str())
        {
            return Err("Recovery envelope contains an unsupported property".into());
        }
        if key != "@id" && key != crate::urls::LOCAL_ID {
            let subject = crate::Subject::from_raw(key, store.get_base_domain().as_deref());
            store
                .get_resource_shallow(&subject)
                .map_err(|_| "Recovery envelope property definition is not available locally")?;
        }
        if value.is_object()
            || value
                .as_array()
                .is_some_and(|items| items.iter().any(|item| !item.is_string()))
        {
            return Err("Recovery envelope contains a nested or invalid property value".into());
        }
    }
    Ok(())
}

pub(crate) fn recovery_span_ops(
    subject: &str,
    history: &RecoveryHistoryAttribution,
) -> AtomicResult<Vec<Operation>> {
    if subject.trim().is_empty()
        || !history.complete
        || history.retention != EnvelopeRetention::All.as_str()
        || history.attributions.is_empty()
    {
        return Err("Recovery attribution inventory is incomplete".into());
    }
    recovery_owned_spans(&history.attributions)?;
    #[derive(serde::Serialize)]
    struct SpanRecord<'a> {
        client: &'a [crate::loro::ChangeSpan],
        server: &'a [crate::loro::ChangeSpan],
    }

    let mut identities = std::collections::HashSet::with_capacity(history.attributions.len());
    history
        .attributions
        .iter()
        .map(|attribution| {
            if !attribution.verified
                || attribution.signer.is_empty()
                || attribution.signature.is_empty()
                || attribution.created_at < 0
                || attribution.commit_id
                    != crate::identifiers::commit_subject(&attribution.signature)
                || !identities.insert(attribution.signature.as_str())
            {
                return Err("Recovery attribution does not match its verified envelope".into());
            }
            let spans = SpanRecord {
                client: attribution
                    .spans
                    .as_deref()
                    .ok_or("Recovery signed spans are missing")?,
                server: attribution
                    .server_spans
                    .as_deref()
                    .ok_or("Recovery server spans are missing")?,
            };
            Ok(Operation {
                tree: Tree::EnvelopeSpans,
                method: Method::Insert,
                key: key(subject, attribution.created_at, &attribution.signature),
                val: Some(serde_json::to_vec(&spans)?),
            })
        })
        .collect()
}

/// Import recovery evidence without the ordinary sync path's retention or omission semantics.
pub(crate) async fn import_recovery_envelope(
    store: &Db,
    expected_subject: &str,
    json: &str,
) -> AtomicResult<()> {
    check_recovery_envelope_fields(store, json)?;
    let local = crate::recovery::LocalRecoveryStore::new(store);
    let resource = crate::parse::parse_json_ad_commit_resource(json, &local).await?;
    let commit = crate::commit::Commit::from_resource(resource)?;
    if recovery_envelope_id(store, commit.subject.as_str())
        != recovery_envelope_id(store, expected_subject)
    {
        return Err("Recovery envelope is about a different subject".into());
    }
    commit.validate_signature(&local).await?;
    let signature = commit
        .signature
        .as_deref()
        .ok_or("Recovery envelope has no signature")?;
    store.kv.insert(
        Tree::Envelopes,
        &key(
            &recovery_envelope_id(store, expected_subject),
            commit.created_at,
            signature,
        ),
        json.as_bytes(),
    )
}

struct OperationDigest {
    hash: sha2::Sha256,
    bytes: usize,
    limit: usize,
}

impl std::io::Write for OperationDigest {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let next = self
            .bytes
            .checked_add(bytes.len())
            .filter(|next| *next <= self.limit)
            .ok_or_else(|| {
                std::io::Error::other("Recovery operation proof exceeds its 1 GiB work limit")
            })?;
        self.hash.update(bytes);
        self.bytes = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn operation_digest<T: serde::Serialize + ?Sized>(
    changes: &T,
    limit: usize,
) -> AtomicResult<([u8; 32], usize)> {
    let mut writer = OperationDigest {
        hash: sha2::Sha256::new(),
        bytes: 0,
        limit,
    };
    serde_json::to_writer(&mut writer, changes)?;
    Ok((writer.hash.finalize().into(), writer.bytes))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoverySpanOwner {
    Client(usize),
    Server(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RecoveryOwnedSpan {
    peer: u64,
    start: i32,
    end: i32,
    owner: RecoverySpanOwner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RecoveryRange {
    peer: u64,
    start: i32,
    end: i32,
}

fn recovery_span_end(span: &crate::loro::ChangeSpan) -> AtomicResult<i32> {
    if span.counter < 0 || span.length == 0 {
        return Err("Recovery contains a malformed original span".into());
    }
    span.counter
        .checked_add(i32::try_from(span.length).map_err(|_| "Recovery span length overflow")?)
        .ok_or_else(|| "Recovery span counter overflow".into())
}

fn validate_original_span_vector(spans: &[crate::loro::ChangeSpan]) -> AtomicResult<()> {
    let mut previous_peer = None;
    for span in spans {
        recovery_span_end(span)?;
        if previous_peer.is_some_and(|peer| peer >= span.peer) {
            return Err("Recovery original spans are not in native peer order".into());
        }
        previous_peer = Some(span.peer);
    }
    Ok(())
}

fn recovery_owned_spans(attributions: &[Attribution]) -> AtomicResult<Vec<RecoveryOwnedSpan>> {
    let mut spans = Vec::new();
    for (index, attribution) in attributions.iter().enumerate() {
        if !attribution.verified {
            return Err("Recovery source spans belong to an unverified envelope".into());
        }
        let client = attribution
            .spans
            .as_deref()
            .ok_or("Recovery signed spans are missing")?;
        let server = attribution
            .server_spans
            .as_deref()
            .ok_or("Recovery server spans are missing")?;
        validate_original_span_vector(client)?;
        validate_original_span_vector(server)?;
        spans.reserve(client.len() + server.len());
        for (source, entries) in [
            (RecoverySpanOwner::Client(index), client),
            (RecoverySpanOwner::Server(index), server),
        ] {
            for span in entries {
                spans.push(RecoveryOwnedSpan {
                    peer: span.peer,
                    start: span.counter,
                    end: recovery_span_end(span)?,
                    owner: source,
                });
            }
        }
    }
    spans.sort_unstable_by_key(|span| (span.peer, span.start, span.end));
    for pair in spans.windows(2) {
        if pair[0].peer == pair[1].peer && pair[0].end > pair[1].start {
            return Err("Recovery original spans overlap".into());
        }
    }
    Ok(spans)
}

fn normalized_recovery_ranges(mut ranges: Vec<RecoveryRange>) -> AtomicResult<Vec<RecoveryRange>> {
    ranges.sort_unstable_by_key(|range| (range.peer, range.start, range.end));
    let mut normalized: Vec<RecoveryRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if range.start < 0 || range.end <= range.start {
            return Err("Recovery snapshot contains an invalid operation range".into());
        }
        if let Some(previous) = normalized.last_mut() {
            if previous.peer == range.peer {
                if previous.end > range.start {
                    return Err("Recovery snapshot operation counters overlap".into());
                }
                if previous.end == range.start {
                    previous.end = range.end;
                    continue;
                }
            }
        }
        normalized.push(range);
    }
    Ok(normalized)
}

fn recovery_owner_for_range(
    spans: &[RecoveryOwnedSpan],
    peer: u64,
    start: i32,
    end: i32,
    work: &mut usize,
) -> AtomicResult<RecoverySpanOwner> {
    let mut index =
        spans.partition_point(|span| span.peer < peer || (span.peer == peer && span.end <= start));
    let mut cursor = start;
    let mut owner = None;
    while let Some(span) = spans
        .get(index)
        .filter(|span| span.peer == peer && span.start < end)
    {
        RecoveryOperationProof::charge_work(work, 1)?;
        let covered_from = span.start.max(start);
        if covered_from > cursor {
            return Err("Recovery snapshot operation is not covered by original spans".into());
        }
        if let Some(previous) = owner {
            if previous != span.owner {
                return Err(
                    "Recovery snapshot change mixes signed owners or server bookkeeping".into(),
                );
            }
        } else {
            owner = Some(span.owner);
        }
        cursor = span.end.min(end);
        index += 1;
        if cursor == end {
            break;
        }
    }
    if cursor != end {
        return Err("Recovery snapshot operation is not covered by original spans".into());
    }
    owner.ok_or_else(|| "Recovery snapshot operation has no original source".into())
}

struct RecoveryOperationProof {
    changes: Vec<loro::JsonChange>,
    ranges: Vec<(loro::VersionVector, loro::VersionVector)>,
    prerequisites: Vec<loro::VersionVector>,
    compared_bytes: usize,
    comparisons: usize,
    document_work: usize,
}
#[derive(Clone, Copy)]
struct RecoveryServerSnapshot<'a> {
    store: &'a Db,
    members: &'a std::collections::HashSet<String>,
    subject: &'a str,
    stored: &'a crate::loro::AtomicLoroDoc,
    spans: &'a [RecoveryOwnedSpan],
    property_root: &'a serde_json::Value,
}

#[derive(Clone, Copy)]
struct RecoveryServerTransition<'a> {
    resource: &'a crate::Resource,
    old: &'a crate::Resource,
    signer: &'a str,
    is_new: bool,
    parent_changed: bool,
    drive_changed: bool,
}

impl RecoveryOperationProof {
    fn new(doc: &crate::loro::AtomicLoroDoc, envelopes: usize) -> AtomicResult<Self> {
        let schema = doc.doc().export_json_updates_without_peer_compression(
            &loro::VersionVector::default(),
            &doc.oplog_vv(),
        );
        if !schema.start_version.is_empty() {
            return Err("Recovery snapshot has truncated operation history".into());
        }
        let changes = schema.changes;
        let operations = changes.iter().map(|change| change.ops.len()).sum::<usize>();
        if operations
            .checked_mul(envelopes)
            .is_none_or(|work| work > 16 * 1024 * 1024)
        {
            return Err("Recovery operation proof exceeds its 16 million comparison limit".into());
        }
        let document_work = changes
            .iter()
            .map(|change| change.ops.len() + change.deps.len())
            .sum();
        Ok(Self {
            changes,
            ranges: Vec::with_capacity(envelopes),
            prerequisites: Vec::with_capacity(envelopes),
            compared_bytes: 0,
            comparisons: 0,
            document_work,
        })
    }

    fn charge_work(counter: &mut usize, amount: usize) -> AtomicResult<()> {
        *counter = counter
            .checked_add(amount)
            .filter(|work| *work <= 16 * 1024 * 1024)
            .ok_or("Recovery operation proof exceeds its 16 million comparison limit")?;
        Ok(())
    }

    fn carries(
        start: &loro::VersionVector,
        end: &loro::VersionVector,
        peer: u64,
        counter: i32,
    ) -> bool {
        counter >= start.get(&peer).copied().unwrap_or(0)
            && counter < end.get(&peer).copied().unwrap_or(0)
    }

    fn verify_update(
        &mut self,
        store: &Db,
        members: &std::collections::HashSet<String>,
        stored: &crate::loro::AtomicLoroDoc,
        commit: &crate::commit::Commit,
    ) -> AtomicResult<()> {
        let Some(update) = commit.loro_update.as_deref() else {
            if !commit.destroy.unwrap_or(false) {
                return Err("Recovery content envelope has no native update".into());
            }
            self.ranges.push((
                loro::VersionVector::default(),
                loro::VersionVector::default(),
            ));
            self.prerequisites.push(loro::VersionVector::default());
            return Ok(());
        };
        let (start, end) = crate::loro::AtomicLoroDoc::update_range(update)?;
        let stored_vv = stored.oplog_vv();
        for (peer, counter) in end.iter() {
            if *counter > stored_vv.get(peer).copied().unwrap_or(0) {
                return Err("Recovery signed update is not contained in its snapshot".into());
            }
        }
        let mut predecessors = loro::Frontiers::new();
        for (peer, counter) in start.iter() {
            if *counter > 0 {
                predecessors.push(loro::ID::new(*peer, *counter - 1));
            }
        }
        Self::charge_work(&mut self.comparisons, self.document_work)?;
        for change in &self.changes {
            if change
                .ops
                .iter()
                .any(|op| Self::carries(&start, &end, change.id.peer, op.counter))
            {
                for dependency in &change.deps {
                    if !Self::carries(&start, &end, dependency.peer, dependency.counter) {
                        predecessors.push(*dependency);
                    }
                }
            }
        }
        let original = if predecessors.is_empty() {
            crate::loro::AtomicLoroDoc::new()
        } else {
            stored.fork_at(&crate::loro::VersionID::from_frontiers(&predecessors))?
        };
        let before = original.oplog_vv();
        for (peer, counter) in before.iter() {
            if *counter > start.get(peer).copied().unwrap_or(0)
                && end.get(peer).copied().unwrap_or(0) > start.get(peer).copied().unwrap_or(0)
            {
                return Err("Recovery signed update context contains its own operations".into());
            }
        }
        if original.import_update_status(update)? {
            return Err("Recovery signed update has missing causal operations".into());
        }
        let after = original.oplog_vv();
        for (peer, counter) in end.iter() {
            if after.get(peer).copied().unwrap_or(0) < *counter {
                return Err("Recovery signed update did not materialize its complete range".into());
            }
        }
        let mut signed = original
            .doc()
            .export_json_updates_without_peer_compression(&before, &after);
        let mut supplied = stored
            .doc()
            .export_json_updates_without_peer_compression(&before, &after);
        for changes in [&mut signed.changes, &mut supplied.changes] {
            changes.sort_by_key(|change| (change.id.peer, change.id.counter));
            for change in changes.iter_mut() {
                change.deps.sort_by_key(|id| (id.peer, id.counter));
            }
        }
        let remaining = (crate::recovery::MAX_BUNDLE_BYTES * 64)
            .checked_sub(self.compared_bytes)
            .ok_or("Recovery operation proof exceeds its 1 GiB work limit")?;
        let (signed_hash, signed_bytes) = operation_digest(&signed.changes, remaining)?;
        let (supplied_hash, supplied_bytes) =
            operation_digest(&supplied.changes, remaining - signed_bytes)?;
        self.compared_bytes += signed_bytes + supplied_bytes;
        if signed_bytes != supplied_bytes || signed_hash != supplied_hash {
            return Err(
                "Recovery snapshot operations differ from original signed native bytes".into(),
            );
        }
        self.ranges.push((start, end));
        self.prerequisites.push(after);
        let mut signed_resource = crate::Resource::new(commit.subject.to_string());
        signed_resource.apply_state_doc(original)?;
        crate::recovery::validate_explicit_dependencies(
            store,
            &crate::recovery::required_dependencies(store, &signed_resource),
            members,
        )?;
        Ok(())
    }

    fn causal_before(
        stored: &crate::loro::AtomicLoroDoc,
        change: &loro::JsonChange,
        counter: i32,
    ) -> AtomicResult<crate::loro::AtomicLoroDoc> {
        let mut causal = loro::Frontiers::new();
        for dependency in &change.deps {
            causal.push(*dependency);
        }
        if counter > 0 {
            causal.push(loro::ID::new(change.id.peer, counter - 1));
        }
        if causal.is_empty() {
            Ok(crate::loro::AtomicLoroDoc::new())
        } else {
            stored.fork_at(&crate::loro::VersionID::from_frontiers(&causal))
        }
    }

    fn includes(have: &loro::VersionVector, required: &loro::VersionVector) -> bool {
        required
            .iter()
            .all(|(peer, end)| have.get(peer).copied().unwrap_or(0) >= *end)
    }

    fn validate_snapshot_ownership(
        &mut self,
        stored: &crate::loro::AtomicLoroDoc,
        attributions: &[Attribution],
        spans: &[RecoveryOwnedSpan],
    ) -> AtomicResult<Vec<RecoverySpanOwner>> {
        if self.ranges.len() != attributions.len() || self.prerequisites.len() != attributions.len()
        {
            return Err("Recovery signed envelope inventory is inconsistent".into());
        }
        let stored_vv = stored.oplog_vv();
        let mut span_ranges = Vec::with_capacity(spans.len());
        for span in spans {
            if span.end > stored_vv.get(&span.peer).copied().unwrap_or(0) {
                return Err("Recovery original span is outside its snapshot".into());
            }
            span_ranges.push(RecoveryRange {
                peer: span.peer,
                start: span.start,
                end: span.end,
            });
        }
        for (index, attribution) in attributions.iter().enumerate() {
            let (start, end) = &self.ranges[index];
            for span in attribution
                .spans
                .as_deref()
                .ok_or("Recovery signed spans are missing")?
            {
                let span_end = recovery_span_end(span)?;
                if span.counter < start.get(&span.peer).copied().unwrap_or(0)
                    || span_end > end.get(&span.peer).copied().unwrap_or(0)
                {
                    return Err("Recovery signed spans exceed the original signed update".into());
                }
            }
        }

        let mut snapshot_ranges = Vec::new();
        let mut change_owners = Vec::with_capacity(self.changes.len());
        for change in &self.changes {
            if change.ops.is_empty() {
                return Err("Recovery snapshot contains an operation-free change".into());
            }
            let mut change_owner = None;
            for op in &change.ops {
                Self::charge_work(&mut self.comparisons, 1)?;
                let length = i32::try_from(op.content.op_len())
                    .map_err(|_| "Recovery operation length overflow")?;
                let end = op
                    .counter
                    .checked_add(length)
                    .ok_or("Recovery operation counter overflow")?;
                if op.counter < 0
                    || length <= 0
                    || end > stored_vv.get(&change.id.peer).copied().unwrap_or(0)
                {
                    return Err("Recovery snapshot contains an inconsistent peer counter".into());
                }
                let owner = recovery_owner_for_range(
                    spans,
                    change.id.peer,
                    op.counter,
                    end,
                    &mut self.comparisons,
                )?;
                if change_owner.is_some_and(|previous| previous != owner) {
                    return Err("Recovery snapshot change has multiple original owners".into());
                }
                change_owner = Some(owner);
                snapshot_ranges.push(RecoveryRange {
                    peer: change.id.peer,
                    start: op.counter,
                    end,
                });
            }
            change_owners.push(change_owner.ok_or("Recovery snapshot change has no operations")?);
        }
        if normalized_recovery_ranges(snapshot_ranges)? != normalized_recovery_ranges(span_ranges)?
        {
            return Err("Recovery original spans do not exactly cover snapshot operations".into());
        }
        Ok(change_owners)
    }

    async fn verify_server_change(
        &mut self,
        snapshot: &RecoveryServerSnapshot<'_>,
        attribution: &Attribution,
        owner_index: usize,
        change: &loro::JsonChange,
        seen: &mut RecoveryServerActions,
    ) -> AtomicResult<()> {
        let RecoveryServerSnapshot {
            subject,
            stored,
            spans,
            property_root,
            ..
        } = *snapshot;
        Self::charge_work(&mut self.comparisons, self.document_work)?;
        let causal = Self::causal_before(stored, change, change.id.counter)?;
        let before = causal.oplog_vv();
        if !Self::includes(&before, &self.prerequisites[owner_index]) {
            return Err("Recovery server bookkeeping precedes its signed update".into());
        }
        let mut prior = before.clone();
        for span in spans.iter().filter(|span| {
            span.owner == RecoverySpanOwner::Client(owner_index)
                || span.owner == RecoverySpanOwner::Server(owner_index)
        }) {
            let present = prior.get(&span.peer).copied().unwrap_or(0);
            if present > span.start {
                if span.start == 0 {
                    prior.remove(&span.peer);
                } else {
                    prior.insert(span.peer, span.start);
                }
            }
        }
        let prior_doc = recovery_doc_at_vector(stored, &prior)?;
        let mut old = crate::Resource::new(subject.to_string());
        old.apply_state_doc(prior_doc)?;
        let mut resource = crate::Resource::new(subject.to_string());
        resource.apply_state_doc(causal)?;
        let is_new = prior.values().all(|counter| *counter == 0);
        if is_new && !attribution.genesis {
            return Err("Recovery server bookkeeping has no signed genesis envelope".into());
        }
        let property_changed = |property: &str| {
            old.get(property).ok().map(|value| value.to_string())
                != resource.get(property).ok().map(|value| value.to_string())
        };
        let parent_changed = property_changed(crate::urls::PARENT);
        let drive_changed = property_changed(crate::urls::DRIVE_PROP);
        let transition = RecoveryServerTransition {
            resource: &resource,
            old: &old,
            signer: &attribution.signer,
            is_new,
            parent_changed,
            drive_changed,
        };

        let mut writer_action = None;
        if is_new
            && resource.get_subject().is_did()
            && !crate::hierarchy::is_chat_log(&resource)
            && !resource
                .get(crate::urls::IS_A)
                .ok()
                .and_then(|value| value.to_subjects(None).ok())
                .is_some_and(|classes| classes.iter().any(|class| class == crate::urls::AGENT))
        {
            let mut writers = resource
                .get(crate::urls::WRITE)
                .ok()
                .and_then(|value| value.to_subjects(None).ok())
                .unwrap_or_default();
            if !writers.contains(&attribution.signer) {
                writers.push(attribution.signer.clone());
                writer_action = Some(crate::Value::from(writers));
            }
        }

        let drive_triggered = is_new || parent_changed || drive_changed;
        let mut drive_actions = vec![None];
        if drive_triggered {
            for candidate in recovery_drive_insert_values(change, property_root)? {
                if self
                    .server_drive_candidate_allowed(snapshot, &transition, &candidate)
                    .await?
                {
                    let action = Some(RecoveryDriveAction::Set(candidate));
                    if !drive_actions.contains(&action) {
                        drive_actions.push(action);
                    }
                }
            }
            if self
                .server_drive_removal_allowed(snapshot, &transition)
                .await?
            {
                drive_actions.push(Some(RecoveryDriveAction::Remove));
            }
        }

        let mut verified = None;
        for add_writer in [false, true] {
            if add_writer && writer_action.is_none() {
                continue;
            }
            for drive_action in &drive_actions {
                if !add_writer && drive_action.is_none() {
                    continue;
                }
                Self::charge_work(&mut self.comparisons, 1)?;
                let expected = recovery_server_operations(
                    &resource,
                    change.id.peer,
                    add_writer.then(|| writer_action.as_ref().unwrap()),
                    drive_action.as_ref(),
                )?;
                let remaining = (crate::recovery::MAX_BUNDLE_BYTES * 64)
                    .checked_sub(self.compared_bytes)
                    .ok_or("Recovery operation proof exceeds its 1 GiB work limit")?;
                let (expected_hash, expected_bytes) = operation_digest(&expected, remaining)?;
                let (actual_hash, actual_bytes) =
                    operation_digest(&change.ops, remaining - expected_bytes)?;
                self.compared_bytes += expected_bytes + actual_bytes;
                if expected_bytes == actual_bytes && expected_hash == actual_hash {
                    let actions = RecoveryServerActions {
                        writer: add_writer,
                        drive: drive_action.is_some(),
                    };
                    if verified.replace(actions).is_some() {
                        return Err("Recovery server bookkeeping has ambiguous source proof".into());
                    }
                }
            }
        }
        let actions = verified.ok_or("Unproved unsigned recovery server bookkeeping")?;
        if actions.writer && std::mem::replace(&mut seen.writer, true) {
            return Err("Recovery envelope repeats server-generated write bookkeeping".into());
        }
        if actions.drive && std::mem::replace(&mut seen.drive, true) {
            return Err("Recovery envelope repeats server-generated drive bookkeeping".into());
        }
        Ok(())
    }

    async fn server_drive_candidate_allowed(
        &mut self,
        snapshot: &RecoveryServerSnapshot<'_>,
        transition: &RecoveryServerTransition<'_>,
        candidate: &str,
    ) -> AtomicResult<bool> {
        let RecoveryServerSnapshot {
            store,
            members,
            subject,
            ..
        } = *snapshot;
        let RecoveryServerTransition {
            resource,
            old,
            signer,
            is_new,
            parent_changed,
            drive_changed,
        } = *transition;
        let current_drive = resource
            .get(crate::urls::DRIVE_PROP)
            .ok()
            .map(|value| value.to_string());
        let parent = resource
            .get(crate::urls::PARENT)
            .ok()
            .map(|value| value.to_string());
        if let Some(parent) = parent {
            let derived = recovery_derived_drive(
                store,
                resource,
                &parent,
                candidate,
                members,
                &mut self.comparisons,
            )?;
            if derived.as_deref() == Some(candidate) {
                return Ok(true);
            }
            let normalized_candidate = store
                .normalize_subject(&crate::Subject::from_raw(
                    candidate,
                    store.get_base_domain().as_deref(),
                ))
                .pure_id();
            let normalized_parent = store
                .normalize_subject(&crate::Subject::from_raw(
                    &parent,
                    store.get_base_domain().as_deref(),
                ))
                .pure_id();
            if current_drive.is_some()
                && (is_new || parent_changed)
                && normalized_candidate == normalized_parent
            {
                crate::recovery::validate_explicit_dependencies(
                    store,
                    std::slice::from_ref(&parent),
                    members,
                )?;
                return Ok(true);
            }
            if current_drive.is_some()
                && !is_new
                && !parent_changed
                && drive_changed
                && old
                    .get(crate::urls::DRIVE_PROP)
                    .ok()
                    .is_some_and(|value| value.to_string() == candidate)
            {
                return Ok(true);
            }
            return Ok(false);
        }

        let own = store
            .normalize_subject(&crate::Subject::from_raw(
                subject,
                store.get_base_domain().as_deref(),
            ))
            .pure_id();
        let normalized_candidate = store
            .normalize_subject(&crate::Subject::from_raw(
                candidate,
                store.get_base_domain().as_deref(),
            ))
            .pure_id();
        let Some(claimed) = current_drive else {
            return Ok(false);
        };
        let normalized_claim = store
            .normalize_subject(&crate::Subject::from_raw(
                &claimed,
                store.get_base_domain().as_deref(),
            ))
            .pure_id();
        if normalized_candidate != own || normalized_claim == own {
            return Ok(false);
        }
        Ok(!recovery_claimed_drive_is_appendable(store, resource, signer).await)
    }

    async fn server_drive_removal_allowed(
        &mut self,
        snapshot: &RecoveryServerSnapshot<'_>,
        transition: &RecoveryServerTransition<'_>,
    ) -> AtomicResult<bool> {
        let RecoveryServerSnapshot { store, members, .. } = *snapshot;
        let RecoveryServerTransition {
            resource,
            old,
            signer,
            is_new,
            parent_changed,
            drive_changed,
        } = *transition;
        if is_new
            || parent_changed
            || !drive_changed
            || old.get(crate::urls::DRIVE_PROP).is_ok()
            || resource.get(crate::urls::DRIVE_PROP).is_err()
        {
            return Ok(false);
        }
        let Some(parent) = resource
            .get(crate::urls::PARENT)
            .ok()
            .map(|value| value.to_string())
        else {
            return Ok(false);
        };
        let candidate = resource.get(crate::urls::DRIVE_PROP)?.to_string();
        if recovery_derived_drive(
            store,
            resource,
            &parent,
            &candidate,
            members,
            &mut self.comparisons,
        )?
        .is_some()
        {
            return Ok(false);
        }
        Ok(!recovery_claimed_drive_is_appendable(store, resource, signer).await)
    }

    async fn finish(
        &mut self,
        store: &Db,
        members: &std::collections::HashSet<String>,
        subject: &str,
        stored: &crate::loro::AtomicLoroDoc,
        attributions: &[Attribution],
    ) -> AtomicResult<()> {
        let spans = recovery_owned_spans(attributions)?;
        let owners = self.validate_snapshot_ownership(stored, attributions, &spans)?;
        let property_root = recovery_property_root()?;
        let snapshot = RecoveryServerSnapshot {
            store,
            members,
            subject,
            stored,
            spans: &spans,
            property_root: &property_root,
        };
        let mut seen = vec![RecoveryServerActions::default(); attributions.len()];
        let changes = std::mem::take(&mut self.changes);
        for (change, owner) in changes.iter().zip(owners) {
            if let RecoverySpanOwner::Server(index) = owner {
                self.verify_server_change(
                    &snapshot,
                    &attributions[index],
                    index,
                    change,
                    &mut seen[index],
                )
                .await?;
            }
        }
        self.changes = changes;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RecoveryDriveAction {
    Set(String),
    Remove,
}

#[derive(Clone, Copy, Default)]
struct RecoveryServerActions {
    writer: bool,
    drive: bool,
}

fn recovery_doc_at_vector(
    stored: &crate::loro::AtomicLoroDoc,
    vector: &loro::VersionVector,
) -> AtomicResult<crate::loro::AtomicLoroDoc> {
    let mut frontiers = loro::Frontiers::new();
    for (peer, counter) in vector.iter() {
        if *counter > 0 {
            frontiers.push(loro::ID::new(*peer, *counter - 1));
        }
    }
    let doc = if frontiers.is_empty() {
        crate::loro::AtomicLoroDoc::new()
    } else {
        stored.fork_at(&crate::loro::VersionID::from_frontiers(&frontiers))?
    };
    let actual = doc.oplog_vv();
    if !RecoveryOperationProof::includes(&actual, vector)
        || !RecoveryOperationProof::includes(vector, &actual)
    {
        return Err("Recovery source spans do not define a causal pre-apply state".into());
    }
    Ok(doc)
}

fn recovery_property_root() -> AtomicResult<serde_json::Value> {
    let marker = "atomic:drive:recovery-root";
    let template = crate::loro::AtomicLoroDoc::new();
    template.set_property(
        crate::urls::DRIVE_PROP,
        &crate::Value::AtomicUrl(marker.into()),
    )?;
    template.commit();
    let root = template
        .doc()
        .export_json_updates_without_peer_compression(
            &loro::VersionVector::default(),
            &template.oplog_vv(),
        )
        .changes
        .iter()
        .flat_map(|change| &change.ops)
        .find_map(|op| match &op.content {
            loro::JsonOpContent::Map(loro::JsonMapOp::Insert {
                key,
                value: loro::LoroValue::String(value),
            }) if key == crate::urls::DRIVE_PROP && value.as_ref() == marker => {
                Some(serde_json::to_value(&op.container))
            }
            _ => None,
        })
        .ok_or("Recovery server-operation template has no property map")??;
    Ok(root)
}
fn recovery_drive_insert_values(
    change: &loro::JsonChange,
    property_root: &serde_json::Value,
) -> AtomicResult<Vec<String>> {
    let mut values: Vec<String> = Vec::new();
    for op in &change.ops {
        if serde_json::to_value(&op.container)? != *property_root {
            continue;
        }
        if let loro::JsonOpContent::Map(loro::JsonMapOp::Insert {
            key,
            value: loro::LoroValue::String(value),
        }) = &op.content
        {
            if crate::identifiers::canonicalize_scheme(key)
                == crate::identifiers::canonicalize_scheme(crate::urls::DRIVE_PROP)
                && !values
                    .iter()
                    .any(|existing| existing.as_str() == value.as_ref())
            {
                values.push(value.to_string());
            }
        }
    }
    Ok(values)
}

fn recovery_server_operations(
    resource: &crate::Resource,
    peer: u64,
    writer: Option<&crate::Value>,
    drive: Option<&RecoveryDriveAction>,
) -> AtomicResult<Vec<loro::JsonOp>> {
    let template_doc = resource.build_state_doc()?;
    let before = template_doc.oplog_vv();
    template_doc.set_peer_id(peer)?;
    let mut template = crate::Resource::new(resource.get_subject().to_string());
    template.apply_state_doc(template_doc)?;
    if let Some(writer) = writer {
        template.set_unsafe(crate::urls::WRITE.into(), (*writer).clone())?;
    }
    match drive {
        Some(RecoveryDriveAction::Set(value)) => {
            template.set_unsafe(
                crate::urls::DRIVE_PROP.into(),
                crate::Value::AtomicUrl(value.as_str().into()),
            )?;
        }
        Some(RecoveryDriveAction::Remove) => template.remove_propval(crate::urls::DRIVE_PROP)?,
        None => {}
    }
    let output = template.build_state_doc()?;
    output.commit();
    let after = output.oplog_vv();
    Ok(output
        .doc()
        .export_json_updates_without_peer_compression(&before, &after)
        .changes
        .into_iter()
        .flat_map(|change| change.ops)
        .collect())
}

async fn recovery_claimed_drive_is_appendable(
    store: &Db,
    resource: &crate::Resource,
    signer: &str,
) -> bool {
    let Ok(claimed) = resource.get(crate::urls::DRIVE_PROP) else {
        return false;
    };
    let drive_subject = crate::Subject::from(claimed.to_string());
    if &drive_subject == resource.get_subject()
        || matches!(
            store.normalize_subject(&drive_subject),
            crate::Subject::External(_)
        )
    {
        return false;
    }
    let local = crate::recovery::LocalRecoveryStore::new(store);
    let Ok(drive) = local.get_resource(&drive_subject).await else {
        return false;
    };
    let agent: crate::agents::ForAgent = signer.into();
    crate::hierarchy::check_rights(&local, &drive, &agent, crate::hierarchy::Right::Append)
        .await
        .is_ok()
}

fn recovery_derived_drive(
    store: &Db,
    resource: &crate::Resource,
    parent: &str,
    candidate: &str,
    members: &std::collections::HashSet<String>,
    work: &mut usize,
) -> AtomicResult<Option<String>> {
    let identity = |value: &str| {
        store
            .normalize_subject(&crate::Subject::from_raw(
                value,
                store.get_base_domain().as_deref(),
            ))
            .pure_id()
    };
    let mut pending = vec![(parent.to_string(), 0usize)];
    let mut scheduled = std::collections::HashMap::from([(identity(parent), 0usize)]);
    while let Some((current, depth)) = pending.pop() {
        let id = identity(&current);
        crate::recovery::validate_explicit_dependencies(
            store,
            std::slice::from_ref(&current),
            members,
        )?;
        if depth >= 32 || scheduled.get(&id).is_some_and(|best| *best < depth) {
            continue;
        }
        RecoveryOperationProof::charge_work(work, 1)?;
        let doc = match store.kv.get(Tree::LoroSnapshots, id.as_bytes())? {
            Some(snapshot) => crate::recovery::strict_snapshot_doc(&snapshot)?,
            None if !members.contains(&id) => {
                let subject =
                    crate::Subject::from_raw(&current, store.get_base_domain().as_deref());
                store
                    .get_resource_shallow(&store.normalize_subject(&subject))?
                    .build_state_doc()?
            }
            None => return Err("Recovery native bookkeeping parent snapshot is missing".into()),
        };
        let mut ancestor = crate::Resource::new(current.clone());
        ancestor.apply_state_doc(doc.fork_at(&doc.current_version())?)?;
        let schema = doc.doc().export_json_updates_without_peer_compression(
            &loro::VersionVector::default(),
            &doc.oplog_vv(),
        );
        RecoveryOperationProof::charge_work(
            work,
            schema
                .changes
                .iter()
                .map(|change| change.ops.len() + change.deps.len())
                .sum(),
        )?;
        let mut versions = Vec::new();
        for change in doc.changes() {
            let length =
                i32::try_from(change.len).map_err(|_| "Recovery drive history length overflow")?;
            if length == 0 {
                continue;
            }
            let last = change
                .counter
                .checked_add(length - 1)
                .ok_or("Recovery drive history counter overflow")?;
            versions.push(loro::ID::new(change.peer, last));
        }
        let version_work = schema
            .changes
            .iter()
            .map(|change| change.ops.len() + change.deps.len())
            .sum();
        for version in std::iter::once(None).chain(versions.into_iter().map(Some)) {
            RecoveryOperationProof::charge_work(work, version_work)?;
            let historical;
            let state = if let Some(version) = version {
                let mut state = crate::Resource::new(current.clone());
                state.apply_state_doc(doc.fork_at(&crate::loro::VersionID::from_frontiers(
                    &loro::Frontiers::from_id(version),
                ))?)?;
                historical = state;
                &historical
            } else {
                &ancestor
            };
            let derived = if let Ok(drive) = state.get(crate::urls::DRIVE_PROP) {
                Some(drive.to_string())
            } else if state.has_class(crate::urls::DRIVE) {
                Some(current.clone())
            } else if let Ok(parent) = state.get(crate::urls::PARENT) {
                let parent = parent.to_string();
                let parent_id = identity(&parent);
                let next_depth = depth + 1;
                if next_depth < 32
                    && scheduled
                        .get(&parent_id)
                        .is_none_or(|best| next_depth < *best)
                {
                    scheduled.insert(parent_id, next_depth);
                    pending.push((parent, next_depth));
                }
                None
            } else {
                resource
                    .get(crate::urls::DRIVE_PROP)
                    .is_ok()
                    .then(|| current.clone())
            };
            if let Some(derived) = derived {
                if identity(candidate) == identity(&derived) {
                    crate::recovery::validate_explicit_dependencies(
                        store,
                        std::slice::from_ref(&derived),
                        members,
                    )?;
                    return Ok(Some(candidate.to_string()));
                }
            }
        }
    }
    Ok(None)
}

fn recovery_envelope_id(store: &Db, subject: &str) -> String {
    store
        .normalize_subject(&crate::Subject::from_raw(
            subject,
            store.get_base_domain().as_deref(),
        ))
        .pure_id()
}

fn recovery_spans_from_view(
    view: &dyn crate::db::kv_store::KvReadView,
    envelope: &StoredEnvelope,
    max_bytes: usize,
) -> AtomicResult<(crate::loro::EnvelopeSpans, usize)> {
    let key = key(&envelope.subject, envelope.created_at, &envelope.signature);
    let bytes = view
        .get_bounded(Tree::EnvelopeSpans, &key, max_bytes)?
        .ok_or("Recovery original envelope spans are missing")?;
    if bytes.len() > max_bytes {
        return Err("Recovery original envelope spans exceed their byte limit".into());
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SpanEntry {
        #[serde(with = "crate::loro::peer_id_string")]
        peer: u64,
        counter: i32,
        length: u32,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SpanRecord {
        client: Vec<SpanEntry>,
        server: Vec<SpanEntry>,
    }
    let record: SpanRecord =
        serde_json::from_slice(&bytes).map_err(|_| "Malformed original recovery spans")?;
    let spans = crate::loro::EnvelopeSpans {
        client: record
            .client
            .into_iter()
            .map(|span| crate::loro::ChangeSpan {
                peer: span.peer,
                counter: span.counter,
                length: span.length,
            })
            .collect(),
        server: record
            .server
            .into_iter()
            .map(|span| crate::loro::ChangeSpan {
                peer: span.peer,
                counter: span.counter,
                length: span.length,
            })
            .collect(),
    };
    validate_original_span_vector(&spans.client)?;
    validate_original_span_vector(&spans.server)?;
    Ok((spans, bytes.len()))
}

pub(crate) async fn recovery_history_from_view(
    store: &Db,
    view: &dyn crate::db::kv_store::KvReadView,
    subject: &str,
    snapshot: &[u8],
    envelope_limit: usize,
    envelope_byte_limit: usize,
    members: &std::collections::HashSet<String>,
) -> AtomicResult<(RecoveryHistoryAttribution, Vec<StoredEnvelope>)> {
    if store.envelope_retention() != EnvelopeRetention::All {
        return Err("Recovery requires `all` envelope retention".into());
    }

    let pure = recovery_envelope_id(store, subject);
    let stored_doc = crate::recovery::strict_snapshot_doc(snapshot)
        .map_err(|error| format!("Recovery snapshot is invalid for {pure}: {error}"))?;
    let envelopes =
        recovery_envelopes_from_view(store, view, subject, envelope_limit, envelope_byte_limit)?;
    if envelopes.is_empty() {
        return Err(format!("Recovery history is missing for {pure}").into());
    }

    genesis_signature(&pure)
        .ok_or_else(|| format!("Recovery genesis history is incomplete for {pure}"))?;
    let mut inventory = std::collections::HashSet::with_capacity(envelopes.len());
    for envelope in &envelopes {
        if envelope.subject != pure {
            return Err("Recovery envelope subject does not match its key".into());
        }
        let commit_id = crate::identifiers::canonicalize_scheme(&envelope.commit_id());
        if !inventory.insert(commit_id) {
            return Err("Recovery envelope inventory contains a duplicate signed commit".into());
        }
    }

    let mut attributions = Vec::with_capacity(envelopes.len());
    let mut operation_proof = RecoveryOperationProof::new(&stored_doc, envelopes.len())?;
    let stored_changes = stored_doc.changes();
    let mut span_bytes = 0usize;
    let mut found_genesis = false;
    for envelope in &envelopes {
        check_recovery_envelope_fields(store, &envelope.json)?;
        let local = crate::recovery::LocalRecoveryStore::new(store);
        let resource = crate::parse::parse_json_ad_commit_resource(&envelope.json, &local).await?;
        let commit = crate::commit::Commit::from_resource(resource)?;
        if recovery_envelope_id(store, commit.subject.as_str()) != pure
            || commit.signature.as_deref() != Some(envelope.signature.as_str())
            || commit.created_at != envelope.created_at
        {
            return Err("Recovery envelope metadata does not match its signed commit".into());
        }
        let signer = store.normalize_subject(&commit.signer);
        if !commit.signer.is_agent_did()
            && crate::agents::legacy_agent_pubkey(commit.signer.as_str()).is_none()
            && signer.is_local()
        {
            store
                .get_resource_shallow(&signer)
                .map_err(|_| "Recovery signer profile is missing from the immutable view")?;
        }
        commit.validate_signature(&local).await?;

        let current_commit_id = crate::identifiers::canonicalize_scheme(&envelope.commit_id());
        if let Some(previous) = &commit.previous_commit {
            let previous = crate::identifiers::canonicalize_scheme(previous);
            if previous == current_commit_id || !inventory.contains(&previous) {
                return Err(
                    "Recovery signed predecessor is missing from the source inventory".into(),
                );
            }
        }
        // Native signature validation above also checks the genesis identity:
        // certificate-created DIDs name the certificate signature, not this
        // envelope's content signature; legacy DIDs name the latter.
        let is_genesis = commit.is_genesis == Some(true);
        if is_genesis && std::mem::replace(&mut found_genesis, true) {
            return Err("Recovery source inventory has inconsistent genesis metadata".into());
        }

        let remaining = envelope_byte_limit
            .checked_sub(span_bytes)
            .ok_or("Recovery original envelope spans exceed their byte limit")?;
        let (source_spans, used_bytes) = recovery_spans_from_view(view, envelope, remaining)?;
        span_bytes = span_bytes
            .checked_add(used_bytes)
            .filter(|bytes| *bytes <= envelope_byte_limit)
            .ok_or("Recovery original envelope spans exceed their byte limit")?;

        operation_proof.verify_update(store, members, &stored_doc, &commit)?;
        let tokens = messages_within(&stored_changes, &source_spans.client);
        attributions.push(Attribution {
            signer: commit.signer.to_string(),
            created_at: commit.created_at,
            signature: envelope.signature.clone(),
            commit_id: envelope.commit_id(),
            verified: true,
            tokens,
            destroy: commit.destroy.unwrap_or(false),
            genesis: is_genesis,
            spans: Some(source_spans.client),
            server_spans: Some(source_spans.server),
        });
    }
    if !found_genesis {
        return Err(format!("Recovery source inventory is missing genesis for {pure}").into());
    }

    operation_proof
        .finish(store, members, subject, &stored_doc, &attributions)
        .await?;
    Ok((
        RecoveryHistoryAttribution {
            retention: EnvelopeRetention::All.as_str(),
            attributions,
            complete: true,
        },
        envelopes,
    ))
}

fn recovery_envelopes_from_view(
    store: &Db,
    view: &dyn crate::db::kv_store::KvReadView,
    subject: &str,
    limit: usize,
    max_bytes: usize,
) -> AtomicResult<Vec<StoredEnvelope>> {
    let wanted = recovery_envelope_id(store, subject);
    let rows = view.scan_prefix_bounded(Tree::Envelopes, &prefix(&wanted), limit, max_bytes)?;
    let mut envelopes = Vec::with_capacity(rows.len().max(1));
    for (key, value) in rows {
        let envelope = match decode(&key, value) {
            Some(Decoded::Row(envelope)) => envelope,
            Some(Decoded::Unreadable) | None => {
                return Err(format!("Malformed retained recovery envelope for {wanted}").into());
            }
        };
        if envelope.subject != wanted {
            return Err("Recovery envelope prefix and key subject disagree".into());
        }
        envelopes.push(envelope);
    }
    if envelopes.is_empty() {
        if let Some(genesis) = genesis_envelope_from_row(store, subject) {
            envelopes.push(genesis);
        }
    }
    Ok(envelopes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{Agent, ForAgent};
    use crate::sync::engine::{ingest_commit_json, CommitIngestOpts};
    use crate::{urls, Storelike, Value};

    /// A signed content edit by the store's default agent, applied through
    /// `Db::apply_commit` like every other write.
    async fn signed_edit(db: &Db, subject: &crate::Subject, name: &str) {
        let mut resource = db.get_resource(subject).await.unwrap();
        resource
            .set(urls::NAME.into(), Value::String(name.into()), db)
            .await
            .unwrap();
        let response = resource.save_locally(db).await.unwrap();
        assert!(response.commit.signature.is_some(), "save_locally signs");
    }

    async fn child(db: &Db, drive: &str) -> crate::Subject {
        let subject = db
            .create_resource(
                urls::CLASS,
                drive,
                "Doc",
                Some(vec![
                    (urls::DESCRIPTION, Value::String("d".into())),
                    (urls::SHORTNAME, Value::Slug("doc".into())),
                ]),
            )
            .await
            .unwrap();
        crate::Subject::from_raw(&subject, None)
    }

    /// A resource Alice creates in her drive with `write` for both Alice and
    /// Bob, through a genesis commit (kept as a commit row), so its history
    /// starts with one signed genesis and nothing else.
    async fn shared_doc(db: &Db, drive: &str, alice: &Agent, bob: &Agent) -> crate::Subject {
        let mut resource = crate::Resource::new("did:ad:placeholder".into());
        resource
            .set(urls::PARENT.into(), Value::AtomicUrl(drive.into()), db)
            .await
            .unwrap();
        resource
            .set(urls::NAME.into(), Value::String("shared".into()), db)
            .await
            .unwrap();
        resource
            .set(
                urls::WRITE.into(),
                Value::ResourceArray(vec![
                    alice.subject.to_string().into(),
                    bob.subject.to_string().into(),
                ]),
                db,
            )
            .await
            .unwrap();
        resource
            .save_as_genesis(db)
            .await
            .unwrap()
            .resource_new
            .unwrap()
            .get_subject()
            .clone()
    }

    fn signed_opts(signer: &Agent) -> crate::commit::CommitOpts {
        crate::commit::CommitOpts {
            validate_signature: true,
            validate_timestamp: false,
            validate_rights: true,
            validate_for_agent: Some(signer.subject.to_string()),
            update_index: true,
            ..crate::commit::CommitOpts::no_validations_no_index()
        }
    }

    /// The stored Loro snapshot of `subject`: what a client holds after
    /// fetching it.
    async fn snapshot(db: &Db, subject: &crate::Subject) -> Vec<u8> {
        db.get_resource(subject)
            .await
            .unwrap()
            .materialized_state()
            .expect("stored state")
    }

    /// A raw client commit by `signer`: on top of `base`, as Loro peer
    /// `peer`, `description` is set and committed with `message` (`None`: a
    /// change with no message at all). Only the new change is sent, and it is
    /// applied with the signature and rights checks `/commit` uses.
    async fn client_commit(
        db: &Db,
        subject: &crate::Subject,
        signer: &Agent,
        base: &[u8],
        peer: u64,
        description: &str,
        message: Option<&str>,
    ) -> crate::commit::Commit {
        let doc = crate::loro::AtomicLoroDoc::from_snapshot(base).unwrap();
        doc.set_peer_id(peer).unwrap();
        let base_vv = doc.oplog_vv();
        doc.set_property(urls::DESCRIPTION, &Value::Markdown(description.into()))
            .unwrap();
        match message {
            Some(message) => doc.commit_with_message(message),
            None => doc.commit(),
        }
        let delta = doc.export_updates_since(&base_vv);
        let resource = db.get_resource(subject).await.unwrap();
        let mut builder = crate::commit::CommitBuilder::new(subject.clone());
        builder.set_loro_update(delta);
        let commit = builder.sign(signer, db, &resource).await.unwrap();
        db.apply_commit(commit.clone(), &signed_opts(signer))
            .await
            .expect("a writer's commit applies");
        commit
    }

    /// The changes Loro peer `peer` made, as the report attributes them.
    fn changes_of_peer(report: &HistoryAttribution, peer: u64) -> Vec<&ChangeAttribution> {
        report.changes.iter().filter(|c| c.peer == peer).collect()
    }

    /// Every change is the verified signer's or server bookkeeping.
    fn assert_fully_attributed(report: &HistoryAttribution) {
        assert!(report.complete, "{report:#?}");
        assert!(
            report
                .changes
                .iter()
                .all(|c| matches!(c.origin, ChangeOrigin::Signed | ChangeOrigin::Server)),
            "{report:#?}"
        );
    }

    /// Sets the resource's `retention` through a signed commit.
    async fn set_retention(db: &Db, subject: &crate::Subject, value: &str) {
        let mut resource = db.get_resource(subject).await.unwrap();
        resource
            .set(urls::RETENTION.into(), Value::String(value.into()), db)
            .await
            .unwrap();
        resource.save_locally(db).await.unwrap();
    }

    #[tokio::test]
    async fn an_audited_resource_keeps_every_envelope_beside_a_compact_sibling() {
        let db = Db::init_temp("envelopes_per_resource").await.unwrap();
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let audited = child(&db, &drive).await;
        let plain = child(&db, &drive).await;
        set_retention(&db, &audited, "audit").await;
        for name in ["one", "two", "three"] {
            signed_edit(&db, &audited, name).await;
            signed_edit(&db, &plain, name).await;
        }
        assert_eq!(
            envelopes(&db, audited.as_str()).len(),
            5,
            "genesis, the retention commit and three edits"
        );
        assert_eq!(
            envelopes(&db, plain.as_str()).len(),
            1,
            "the node default still keeps one"
        );
        let report = attribute_history(&db, audited.as_str()).await.unwrap();
        assert_eq!(report.retention, "all");
        let report = attribute_history(&db, plain.as_str()).await.unwrap();
        assert_eq!(report.retention, "latest");
    }

    #[tokio::test]
    async fn audit_on_a_folder_holds_below_it_and_a_child_cannot_lower_it() {
        let db = Db::init_temp("envelopes_inherited").await.unwrap();
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let folder = child(&db, &drive).await;
        set_retention(&db, &folder, "audit").await;
        let kid = child(&db, folder.as_str()).await;
        set_retention(&db, &kid, "compact").await;
        for name in ["one", "two"] {
            signed_edit(&db, &kid, name).await;
        }
        assert_eq!(
            envelopes(&db, kid.as_str()).len(),
            4,
            "genesis, the compact setting and two edits all stay"
        );
    }

    #[tokio::test]
    async fn a_node_running_all_is_not_lowered_by_a_compact_resource() {
        let db = Db::init_temp("envelopes_node_all").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        set_retention(&db, &subject, "compact").await;
        signed_edit(&db, &subject, "one").await;
        assert_eq!(envelopes(&db, subject.as_str()).len(), 3);
    }

    /// Taking `audit` off never deletes the audit era: the lowering commit
    /// is kept beside the earlier rows, and only later commits prune the
    /// newest superseded envelope (which, after one edit, is the lowering
    /// commit itself).
    #[tokio::test]
    async fn lowering_audit_keeps_the_audit_era() {
        let db = Db::init_temp("envelopes_lowered").await.unwrap();
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        set_retention(&db, &subject, "audit").await;
        signed_edit(&db, &subject, "one").await;
        assert_eq!(envelopes(&db, subject.as_str()).len(), 3);
        set_retention(&db, &subject, "compact").await;
        assert_eq!(
            envelopes(&db, subject.as_str()).len(),
            4,
            "the lowering commit is kept beside the audit era"
        );
        let audit_era: Vec<String> = envelopes(&db, subject.as_str())
            .iter()
            .take(3)
            .map(|e| e.signature.clone())
            .collect();
        for name in ["two", "three", "four"] {
            signed_edit(&db, &subject, name).await;
        }
        let kept = envelopes(&db, subject.as_str());
        assert_eq!(kept.len(), 4, "later commits replace only the newest row");
        let kept_sigs: Vec<String> = kept.iter().take(3).map(|e| e.signature.clone()).collect();
        assert_eq!(kept_sigs, audit_era, "the audit era stays untouched");
    }

    #[tokio::test]
    async fn resolve_retention_reads_compact_and_audit() {
        let db = Db::init_temp("envelopes_resolve").await.unwrap();
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let plain = child(&db, &drive).await;
        let audited = child(&db, &drive).await;
        set_retention(&db, &audited, "audit").await;
        let kid = child(&db, audited.as_str()).await;
        let explicit = child(&db, &drive).await;
        set_retention(&db, &explicit, "compact").await;
        for (subject, want) in [
            (&plain, Resolved::Compact),
            (&audited, Resolved::Audit),
            (&kid, Resolved::Audit),
            (&explicit, Resolved::Compact),
        ] {
            let resource = db.get_resource(subject).await.unwrap();
            assert_eq!(resolve_retention(&db, Some(&resource)), want);
        }
    }

    #[tokio::test]
    async fn a_local_write_under_an_unreadable_ancestor_prunes_nothing() {
        let db = Db::init_temp("envelopes_unknown_local").await.unwrap();
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        let mut resource = db.get_resource(&subject).await.unwrap();
        resource
            .set(
                urls::PARENT.into(),
                Value::AtomicUrl(crate::Subject::from_raw(
                    "https://example.com/not-held",
                    None,
                )),
                &db,
            )
            .await
            .unwrap();
        resource.save_locally(&db).await.unwrap();
        signed_edit(&db, &subject, "one").await;
        assert!(envelopes(&db, subject.as_str()).len() >= 2);
    }

    /// The commit that takes audit off is an authorization-relevant commit:
    /// it is kept as a commit resource, so who lowered audit stays provable
    /// after later edits replace its envelope row.
    #[tokio::test]
    async fn the_commit_that_lowers_audit_stays_provable() {
        let db = Db::init_temp("envelopes_lowering_kept").await.unwrap();
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        set_retention(&db, &subject, "audit").await;
        let mut resource = db.get_resource(&subject).await.unwrap();
        resource
            .set(urls::RETENTION.into(), Value::String("compact".into()), &db)
            .await
            .unwrap();
        let response = resource.save_locally(&db).await.unwrap();
        let lowering = response.commit_resource.get_subject().to_string();
        for name in ["one", "two"] {
            signed_edit(&db, &subject, name).await;
        }
        assert!(db.has_resource_locally(&lowering));
    }

    #[tokio::test]
    async fn moving_out_of_an_audited_folder_keeps_the_audit_era() {
        let db = Db::init_temp("envelopes_moved_out").await.unwrap();
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let folder = child(&db, &drive).await;
        set_retention(&db, &folder, "audit").await;
        let kid = child(&db, folder.as_str()).await;
        signed_edit(&db, &kid, "one").await;
        assert_eq!(envelopes(&db, kid.as_str()).len(), 2);
        let mut resource = db.get_resource(&kid).await.unwrap();
        resource
            .set(
                urls::PARENT.into(),
                Value::AtomicUrl(crate::Subject::from_raw(&drive, None)),
                &db,
            )
            .await
            .unwrap();
        resource.save_locally(&db).await.unwrap();
        assert_eq!(
            envelopes(&db, kid.as_str()).len(),
            3,
            "the move out is kept beside the audit era"
        );
    }

    #[tokio::test]
    async fn an_invalid_retention_value_is_refused() {
        let db = Db::init_temp("envelopes_invalid_value").await.unwrap();
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        let mut resource = db.get_resource(&subject).await.unwrap();
        resource
            .set(urls::RETENTION.into(), Value::String("audits".into()), &db)
            .await
            .unwrap();
        assert!(resource.save_locally(&db).await.is_err());
    }

    #[tokio::test]
    async fn import_envelope_keeps_every_envelope_of_a_resource_marked_audit() {
        let source = Db::init_temp("envelopes_import_audit_source")
            .await
            .unwrap();
        let (_alice, drive) = source.setup("Alice").await.unwrap();
        let subject = child(&source, &drive).await;
        set_retention(&source, &subject, "audit").await;
        signed_edit(&source, &subject, "one").await;
        signed_edit(&source, &subject, "two").await;
        let all = envelopes(&source, subject.as_str());
        assert_eq!(all.len(), 4);

        // The receiver holds the resource, with its setting, before the
        // envelopes arrive, as in a bulk push.
        let sink = Db::init_temp("envelopes_import_audit_sink").await.unwrap();
        let resource = source.get_resource(&subject).await.unwrap();
        sink.add_resource(&resource).await.unwrap();
        for e in &all {
            import_envelope(&sink, subject.as_str(), &e.json)
                .await
                .unwrap();
        }
        assert_eq!(envelopes(&sink, subject.as_str()).len(), 4);
    }

    /// An ancestor the receiver does not hold yet may be the one asking for
    /// audit, so nothing is pruned until the whole chain can be read.
    #[tokio::test]
    async fn import_before_the_parent_is_held_keeps_every_envelope() {
        let source = Db::init_temp("envelopes_unknown_source").await.unwrap();
        source.set_envelope_retention(EnvelopeRetention::All);
        let (_alice, drive) = source.setup("Alice").await.unwrap();
        let subject = child(&source, &drive).await;
        signed_edit(&source, &subject, "one").await;
        signed_edit(&source, &subject, "two").await;
        let all = envelopes(&source, subject.as_str());
        assert_eq!(all.len(), 3);

        let sink = Db::init_temp("envelopes_unknown_sink").await.unwrap();
        let resource = source.get_resource(&subject).await.unwrap();
        sink.add_resource(&resource).await.unwrap();
        for e in &all {
            import_envelope(&sink, subject.as_str(), &e.json)
                .await
                .unwrap();
        }
        assert_eq!(envelopes(&sink, subject.as_str()).len(), 3);
    }
    #[tokio::test]
    async fn latest_retention_keeps_one_envelope_per_resource() {
        let db = Db::init_temp("envelopes_latest").await.unwrap();
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        assert_eq!(
            envelopes(&db, subject.as_str()).len(),
            1,
            "create_resource signs a genesis"
        );

        for name in ["one", "two"] {
            signed_edit(&db, &subject, name).await;
        }
        let kept = envelopes(&db, subject.as_str());
        assert_eq!(kept.len(), 1, "Latest keeps only the newest envelope");
        let latest = latest_envelope(&db, subject.as_str()).unwrap();
        assert_eq!(kept[0], latest);
        let stamp = db
            .get_resource(&subject)
            .await
            .unwrap()
            .get(urls::LAST_COMMIT)
            .unwrap()
            .to_string();
        assert_eq!(latest.commit_id(), stamp, "the kept envelope is lastCommit");
    }

    #[tokio::test]
    async fn all_retention_keeps_every_envelope_in_time_order() {
        let db = Db::init_temp("envelopes_all").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;

        for name in ["one", "two", "three"] {
            signed_edit(&db, &subject, name).await;
        }
        let kept = envelopes(&db, subject.as_str());
        assert_eq!(kept.len(), 4, "genesis plus three edits");
        assert!(kept.windows(2).all(|w| w[0].created_at <= w[1].created_at));
        let stamp = db
            .get_resource(&subject)
            .await
            .unwrap()
            .get(urls::LAST_COMMIT)
            .unwrap()
            .to_string();
        assert_eq!(kept.last().unwrap().commit_id(), stamp);
    }

    /// A receiver keeps a pushed envelope only after verifying it: a
    /// tampered signature or a mismatched subject writes nothing.
    #[tokio::test]
    async fn import_envelope_verifies_before_storing() {
        let source = Db::init_temp("envelopes_import_source").await.unwrap();
        let (_alice, drive) = source.setup("Alice").await.unwrap();
        let subject = child(&source, &drive).await;
        signed_edit(&source, &subject, "edited").await;
        let envelope = latest_envelope(&source, subject.as_str()).unwrap();

        let sink = Db::init_temp("envelopes_import_sink").await.unwrap();
        assert!(envelopes(&sink, subject.as_str()).is_empty());

        assert!(
            import_envelope(&sink, "did:ad:someone-else", &envelope.json)
                .await
                .is_err(),
            "an envelope about another subject is refused"
        );
        let tampered = envelope.json.replace(&envelope.signature[..8], "AAAAAAAA");
        assert!(
            import_envelope(&sink, subject.as_str(), &tampered)
                .await
                .is_err(),
            "a bad signature is refused"
        );
        assert!(envelopes(&sink, subject.as_str()).is_empty());

        import_envelope(&sink, subject.as_str(), &envelope.json)
            .await
            .unwrap();
        // Idempotent.
        import_envelope(&sink, subject.as_str(), &envelope.json)
            .await
            .unwrap();
        let kept = envelopes(&sink, subject.as_str());
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].signature, envelope.signature);
        assert_eq!(kept[0].json, envelope.json);
    }

    /// `latest` retention on the receiver keeps the newest envelope whatever
    /// order they arrive in; `all` keeps every one.
    #[tokio::test]
    async fn import_envelope_honours_the_receivers_retention() {
        let source = Db::init_temp("envelopes_import_ret_source").await.unwrap();
        source.set_envelope_retention(EnvelopeRetention::All);
        let (_alice, drive) = source.setup("Alice").await.unwrap();
        let subject = child(&source, &drive).await;
        signed_edit(&source, &subject, "one").await;
        signed_edit(&source, &subject, "two").await;
        let all = envelopes(&source, subject.as_str());
        assert_eq!(all.len(), 3);
        let newest = all.last().unwrap().clone();

        let latest_sink = Db::init_temp("envelopes_import_ret_latest").await.unwrap();
        // The resource arrives before its envelopes, as in a bulk push.
        for held in [&subject, &crate::Subject::from_raw(&drive, None)] {
            let resource = source.get_resource(held).await.unwrap();
            latest_sink.add_resource(&resource).await.unwrap();
        }
        // Newest first, then older ones: the older must not displace it.
        for e in all.iter().rev() {
            import_envelope(&latest_sink, subject.as_str(), &e.json)
                .await
                .unwrap();
        }
        let kept = envelopes(&latest_sink, subject.as_str());
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].signature, newest.signature);

        let all_sink = Db::init_temp("envelopes_import_ret_all").await.unwrap();
        all_sink.set_envelope_retention(EnvelopeRetention::All);
        for e in &all {
            import_envelope(&all_sink, subject.as_str(), &e.json)
                .await
                .unwrap();
        }
        assert_eq!(envelopes(&all_sink, subject.as_str()).len(), 3);
    }

    #[tokio::test]
    async fn envelopes_are_not_resources_or_query_hits() {
        let db = Db::init_temp("envelopes_not_indexed").await.unwrap();
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        let latest = latest_envelope(&db, subject.as_str()).unwrap();
        // The genesis commit row is retained as a resource (critical), but the
        // envelope tree itself is invisible to the resource model.
        assert!(!db.has_resource_locally(&format!("envelope:{}", latest.signature)));
        let mut query = crate::storelike::Query::new_prop_val(urls::SIGNER, "did:ad:agent:nobody");
        query.limit = Some(10);
        assert_eq!(db.query(&query).await.unwrap().count, 0);
    }

    #[tokio::test]
    async fn history_attribution_maps_verified_signers_onto_loro_tokens() {
        let db = Db::init_temp("envelopes_attribution").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        signed_edit(&db, &subject, "edited").await;

        let report = attribute_history(&db, subject.as_str()).await.unwrap();
        assert_eq!(report.retention, "all");
        assert_eq!(report.attributions.len(), 2);
        assert!(report.attributions.iter().all(|a| a.verified));
        assert!(report.attributions[0].genesis);
        assert!(!report.attributions[1].genesis);
        assert!(report
            .attributions
            .iter()
            .all(|a| a.signer == alice.subject));
        assert!(
            report.complete,
            "replaying the retained envelopes must reproduce the stored oplog"
        );

        // Every Loro change of the stored doc is claimed by exactly one envelope.
        let resource = db.get_resource(&subject).await.unwrap();
        let versions = crate::history::versions(&resource).unwrap();
        for version in versions.iter().filter_map(|v| v.message.clone()) {
            let owners = report
                .attributions
                .iter()
                .filter(|a| a.tokens.contains(&version))
                .count();
            assert_eq!(owners, 1, "token {version} must map to one signer");
        }
    }

    #[tokio::test]
    async fn latest_node_preserves_signed_attribution_for_audited_edits() {
        let db = Db::init_temp("envelopes_latest_audit_attribution")
            .await
            .unwrap();
        db.set_envelope_retention(EnvelopeRetention::Latest);
        let (bob, _) = db.setup("Bob").await.unwrap();
        let (alice, drive) = db.setup("Alice").await.unwrap();
        let subject = shared_doc(&db, &drive, &alice, &bob).await;
        set_retention(&db, &subject, "audit").await;

        let alice_base = snapshot(&db, &subject).await;
        let alice_edit = client_commit(
            &db,
            &subject,
            &alice,
            &alice_base,
            101,
            "first audited edit",
            Some("alice-audit-edit"),
        )
        .await;
        let bob_base = snapshot(&db, &subject).await;
        let bob_edit = client_commit(
            &db,
            &subject,
            &bob,
            &bob_base,
            202,
            "second audited edit",
            Some("bob-audit-edit"),
        )
        .await;

        let report = attribute_history(&db, subject.as_str()).await.unwrap();
        assert_eq!(report.retention, "all");
        assert_fully_attributed(&report);
        for (peer, commit, signer) in [
            (101, &alice_edit, &alice.subject),
            (202, &bob_edit, &bob.subject),
        ] {
            let changes = changes_of_peer(&report, peer);
            assert_eq!(changes.len(), 1, "peer {peer} edit should be retained");
            let change = changes[0];
            assert!(matches!(change.origin, ChangeOrigin::Signed));
            assert_eq!(change.signer.as_deref(), Some(signer.as_str()));

            let attribution = change.attribution.expect("signed edit has an envelope");
            let envelope = &report.attributions[attribution];
            assert!(envelope.verified);
            assert_eq!(envelope.signer.as_str(), signer.as_str());
            assert_eq!(
                envelope.signature.as_str(),
                commit.signature.as_deref().unwrap()
            );
        }
    }

    #[tokio::test]
    async fn tampered_envelope_is_unverified_and_history_incomplete() {
        let db = Db::init_temp("envelopes_tampered").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        signed_edit(&db, &subject, "edited").await;

        // Corrupt the newest stored row in place.
        let rows = envelopes(&db, subject.as_str());
        let last = rows.last().unwrap();
        let mut broken: serde_json::Value = serde_json::from_str(&last.json).unwrap();
        broken[urls::SIGNATURE] = serde_json::Value::String("AAAA".into());
        db.kv
            .insert(
                Tree::Envelopes,
                &key(subject.as_str(), last.created_at, &last.signature),
                broken.to_string().as_bytes(),
            )
            .unwrap();

        let report = attribute_history(&db, subject.as_str()).await.unwrap();
        assert!(report.attributions[0].verified);
        assert!(!report.attributions[1].verified);
        assert!(!report.complete);
    }

    #[tokio::test]
    async fn latest_retention_under_a_second_writer_keeps_the_newest_signer() {
        let db = Db::init_temp("envelopes_two_writers").await.unwrap();
        let (alice, drive) = db.setup("Alice").await.unwrap();
        let bob = db.create_agent(Some("Bob")).await.unwrap();
        let subject = shared_doc(&db, &drive, &alice, &bob).await;

        db.set_default_agent(bob.clone());
        signed_edit(&db, &subject, "by bob").await;
        db.set_default_agent(alice.clone());
        let report = attribute_history(&db, subject.as_str()).await.unwrap();
        assert_eq!(
            report.attributions.len(),
            2,
            "the newest envelope, plus the genesis rebuilt from its commit row"
        );
        assert!(report.attributions[0].genesis);
        assert_eq!(report.attributions[0].signer, alice.subject.to_string());
        let newest = report.attributions.last().unwrap();
        assert_eq!(newest.signer, bob.subject.to_string());
        assert!(newest.verified);
        assert!(
            !newest
                .tokens
                .iter()
                .any(|t| crate::identifiers::is_agent_id(t)),
            "a snapshot-carrying edit must not be credited with the genesis change"
        );
        assert_fully_attributed(&report);
    }

    #[tokio::test]
    async fn all_retention_credits_each_writer_with_their_own_change() {
        let db = Db::init_temp("envelopes_two_writers_all").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (alice, drive) = db.setup("Alice").await.unwrap();
        let bob = db.create_agent(Some("Bob")).await.unwrap();
        let subject = shared_doc(&db, &drive, &alice, &bob).await;

        signed_edit(&db, &subject, "by alice").await;
        db.set_default_agent(bob.clone());
        signed_edit(&db, &subject, "by bob").await;
        db.set_default_agent(alice.clone());

        let report = attribute_history(&db, subject.as_str()).await.unwrap();
        assert_fully_attributed(&report);
        let signers: Vec<&str> = report
            .attributions
            .iter()
            .map(|a| a.signer.as_str())
            .collect();
        assert_eq!(
            signers,
            vec![
                alice.subject.as_str(),
                alice.subject.as_str(),
                bob.subject.as_str()
            ],
            "genesis, Alice's edit, Bob's edit"
        );
        assert!(report.attributions[0].genesis);
        assert_eq!(report.attributions[1].tokens.len(), 1);
        assert_eq!(report.attributions[2].tokens.len(), 1);
        assert_ne!(report.attributions[1].tokens, report.attributions[2].tokens);
        let stored = db.get_resource(&subject).await.unwrap();
        let versions = crate::history::versions(&stored).unwrap();
        for token in versions.iter().filter_map(|v| v.message.clone()) {
            let owners = report
                .attributions
                .iter()
                .filter(|a| a.tokens.contains(&token))
                .count();
            assert_eq!(owners, 1, "token {token} must map to exactly one signer");
        }
    }

    /// A change's message is whatever the client wrote, including nothing.
    /// A change without one is still the signer's: it is attributed by the
    /// op IDs the signed envelope brought in, and it counts toward
    /// `complete` like any other.
    #[tokio::test]
    async fn a_change_without_a_message_is_attributed_to_its_signer() {
        let db = Db::init_temp("envelopes_no_message").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (alice, drive) = db.setup("Alice").await.unwrap();
        let bob = db.create_agent(Some("Bob")).await.unwrap();
        let subject = shared_doc(&db, &drive, &alice, &bob).await;

        let base = snapshot(&db, &subject).await;
        client_commit(&db, &subject, &bob, &base, 2002, "accepted", None).await;

        let report = attribute_history(&db, subject.as_str()).await.unwrap();
        let bobs = changes_of_peer(&report, 2002);
        assert_eq!(bobs.len(), 1, "{report:#?}");
        assert_eq!(bobs[0].message, None);
        assert_eq!(bobs[0].origin, ChangeOrigin::Signed, "{report:#?}");
        assert_eq!(bobs[0].signer.as_deref(), Some(bob.subject.as_str()));
        assert_fully_attributed(&report);
    }

    /// Reusing another agent's change message (its "token") must not move
    /// the change to that agent: the message is not what attributes it.
    #[tokio::test]
    async fn a_change_reusing_another_agents_message_stays_with_its_signer() {
        let db = Db::init_temp("envelopes_reused_message").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (alice, drive) = db.setup("Alice").await.unwrap();
        let bob = db.create_agent(Some("Bob")).await.unwrap();
        let subject = shared_doc(&db, &drive, &alice, &bob).await;

        let base = snapshot(&db, &subject).await;
        client_commit(
            &db,
            &subject,
            &alice,
            &base,
            1001,
            "draft",
            Some("c-alice-1"),
        )
        .await;
        let base = snapshot(&db, &subject).await;
        client_commit(
            &db,
            &subject,
            &bob,
            &base,
            2002,
            "accepted",
            Some("c-alice-1"),
        )
        .await;

        let report = attribute_history(&db, subject.as_str()).await.unwrap();
        let alices = changes_of_peer(&report, 1001);
        let bobs = changes_of_peer(&report, 2002);
        assert_eq!((alices.len(), bobs.len()), (1, 1), "{report:#?}");
        assert_eq!(alices[0].signer.as_deref(), Some(alice.subject.as_str()));
        assert_eq!(
            bobs[0].signer.as_deref(),
            Some(bob.subject.as_str()),
            "Bob's change carries Alice's message but was signed by Bob"
        );
        assert_eq!(bobs[0].origin, ChangeOrigin::Signed);
        assert_fully_attributed(&report);
    }

    /// Two agents editing the same property concurrently, from the same base:
    /// both changes stay in the document (one wins the value), and each is
    /// attributed to the agent who signed it, whichever lands second.
    #[tokio::test]
    async fn concurrent_edits_are_each_attributed_to_their_signer() {
        let db = Db::init_temp("envelopes_concurrent").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (alice, drive) = db.setup("Alice").await.unwrap();
        let bob = db.create_agent(Some("Bob")).await.unwrap();
        let subject = shared_doc(&db, &drive, &alice, &bob).await;

        let base = snapshot(&db, &subject).await;
        client_commit(&db, &subject, &alice, &base, 1001, "accepted", Some("c-a")).await;
        client_commit(&db, &subject, &bob, &base, 2002, "rejected", Some("c-b")).await;

        let report = attribute_history(&db, subject.as_str()).await.unwrap();
        let alices = changes_of_peer(&report, 1001);
        let bobs = changes_of_peer(&report, 2002);
        assert_eq!((alices.len(), bobs.len()), (1, 1), "{report:#?}");
        assert_eq!(
            alices[0].lamport, bobs[0].lamport,
            "the edits are concurrent"
        );
        assert_eq!(alices[0].signer.as_deref(), Some(alice.subject.as_str()));
        assert_eq!(bobs[0].signer.as_deref(), Some(bob.subject.as_str()));
        assert_fully_attributed(&report);
    }

    /// Re-applying an envelope this node already applied adds no ops, and
    /// must not erase what was recorded the first time: anyone can resend a
    /// signed commit they saw, and that must not unattribute its changes.
    #[tokio::test]
    async fn replaying_an_envelope_keeps_its_attribution() {
        let db = Db::init_temp("envelopes_replay").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (alice, drive) = db.setup("Alice").await.unwrap();
        let bob = db.create_agent(Some("Bob")).await.unwrap();
        let subject = shared_doc(&db, &drive, &alice, &bob).await;

        let base = snapshot(&db, &subject).await;
        let commit = client_commit(&db, &subject, &bob, &base, 2002, "accepted", None).await;
        db.apply_commit(commit, &signed_opts(&bob))
            .await
            .expect("an idempotent replay is accepted");

        let report = attribute_history(&db, subject.as_str()).await.unwrap();
        let bobs = changes_of_peer(&report, 2002);
        assert_eq!(bobs.len(), 1, "{report:#?}");
        assert_eq!(bobs[0].signer.as_deref(), Some(bob.subject.as_str()));
        assert_fully_attributed(&report);
    }

    /// A change that reached the document without a signed commit (here a
    /// direct store write) belongs to nobody: it is reported as unattributed
    /// and the history is not complete.
    #[tokio::test]
    async fn a_change_without_an_envelope_makes_history_incomplete() {
        let db = Db::init_temp("envelopes_unsigned_write").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (alice, drive) = db.setup("Alice").await.unwrap();
        let bob = db.create_agent(Some("Bob")).await.unwrap();
        let subject = shared_doc(&db, &drive, &alice, &bob).await;
        assert_fully_attributed(&attribute_history(&db, subject.as_str()).await.unwrap());

        let mut resource = db.get_resource(&subject).await.unwrap();
        resource
            .set_unsafe(urls::DESCRIPTION.into(), Value::String("unsigned".into()))
            .unwrap();
        db.add_resource_opts(&resource, false, true, true)
            .await
            .unwrap();

        let report = attribute_history(&db, subject.as_str()).await.unwrap();
        assert!(!report.complete);
        assert!(report
            .changes
            .iter()
            .any(|c| c.origin == ChangeOrigin::Unattributed && c.signer.is_none()));
    }

    /// The browser signs a *delta* (ops since its last save) tagged with a
    /// drain token, not a snapshot. Its tokens must still be read: a delta
    /// imported into an empty doc is pending (missing deps) and lists no
    /// changes, which is how attribution first shipped with empty tokens.
    #[tokio::test]
    async fn browser_style_delta_envelope_is_attributed_by_its_token() {
        let db = Db::init_temp("envelopes_delta").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;

        // A client that holds the current state edits it and exports only
        // the new change, tagged the way the drain tags it.
        let resource = db.get_resource(&subject).await.unwrap();
        let snapshot = resource.materialized_state().expect("stored state");
        let base_vv = crate::loro::AtomicLoroDoc::from_snapshot(&snapshot)
            .unwrap()
            .oplog_vv();
        let client_doc = crate::loro::AtomicLoroDoc::from_snapshot(&snapshot).unwrap();
        client_doc
            .set_property(urls::NAME, &Value::String("delta edit".into()))
            .unwrap();
        client_doc.commit_with_message("c-01test-delta-token");
        let delta = client_doc.export_updates_since(&base_vv);
        assert!(!delta.is_empty());

        let mut builder = crate::commit::CommitBuilder::new(subject.clone());
        builder.set_loro_update(delta);
        let commit = builder.sign(&alice, &db, &resource).await.unwrap();
        let json = commit
            .into_resource(&db)
            .await
            .unwrap()
            .to_json_ad(None)
            .unwrap();
        ingest_commit_json(&db, &json, &CommitIngestOpts::peer())
            .await
            .expect("the delta applies");

        let report = attribute_history(&db, subject.as_str()).await.unwrap();
        let last = report.attributions.last().unwrap();
        assert!(last.verified);
        assert_eq!(
            last.tokens,
            vec!["c-01test-delta-token".to_string()],
            "the delta's own change token is credited to its envelope"
        );
        assert!(report.complete, "{report:?}");
        let stored = db.get_resource(&subject).await.unwrap();
        assert!(crate::history::versions(&stored)
            .unwrap()
            .iter()
            .any(|v| v.message.as_deref() == Some("c-01test-delta-token")));
    }

    #[tokio::test]
    async fn destroy_envelope_is_the_latest_row_of_a_destroyed_subject() {
        let db = Db::init_temp("envelopes_destroy").await.unwrap();
        let (alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        let resource = db.get_resource(&subject).await.unwrap();
        let mut builder = crate::commit::CommitBuilder::new(subject.clone());
        builder.destroy(true);
        let commit = builder.sign(&alice, &db, &resource).await.unwrap();
        let json = commit
            .into_resource(&db)
            .await
            .unwrap()
            .to_json_ad(None)
            .unwrap();
        ingest_commit_json(&db, &json, &CommitIngestOpts::peer())
            .await
            .unwrap();

        let latest = latest_envelope(&db, subject.as_str()).unwrap();
        assert!(latest.is_destroy());
        assert_eq!(
            crate::sync::tombstones::destroy_envelope(&db, subject.as_str()).as_deref(),
            Some(latest.json.as_str()),
            "the tombstone's envelope is the envelope tree's latest row"
        );
        assert!(crate::sync::tombstones::is_tombstoned(
            &db,
            subject.as_str()
        ));
        let _ = ForAgent::Public;
    }

    /// A critical commit's `Tree::Resources` row is the durable record, and
    /// has to verify on its own. Under `latest` retention the first ordinary
    /// edit deletes the genesis envelope, so if the row leaned on the
    /// envelope for its `loroUpdate` there would be nothing left for the
    /// signature to cover.
    #[tokio::test]
    async fn stored_genesis_commit_keeps_its_signed_payload_after_a_later_edit() {
        let db = Db::init_temp("envelopes_genesis_row_payload")
            .await
            .unwrap();
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        let genesis_id = latest_envelope(&db, subject.as_str()).unwrap().commit_id();

        signed_edit(&db, &subject, "edited").await;
        assert!(
            envelopes(&db, subject.as_str())
                .iter()
                .all(|e| e.commit_id() != genesis_id),
            "latest retention has dropped the genesis envelope"
        );

        let row = db.get_resource(&genesis_id.as_str().into()).await.unwrap();
        assert!(
            matches!(row.get(urls::LORO_UPDATE), Ok(Value::LoroDoc(bytes)) if !bytes.is_empty()),
            "the stored genesis commit keeps its signed loroUpdate"
        );
        let commit = crate::commit::Commit::from_resource(row.clone()).unwrap();
        commit
            .validate_signature(&db)
            .await
            .expect("the stored genesis commit verifies without its envelope");

        // Without the payload the same row no longer verifies, so the check
        // above is really about the payload being there.
        let mut stripped = row;
        stripped.remove_propval(urls::LORO_UPDATE).unwrap();
        let stripped = crate::commit::Commit::from_resource(stripped).unwrap();
        assert!(stripped.validate_signature(&db).await.is_err());
    }

    /// After a destroy and a re-create of the same subject, `latest` has
    /// replaced the destroy envelope with the new genesis. The destroy's
    /// commit row is then the only local evidence that this destroy was
    /// already applied, and `Db::apply_commit` must refuse to replay it.
    #[tokio::test]
    async fn destroy_replay_after_recreate_is_refused_by_the_commit_row() {
        let db = Db::init_temp("envelopes_destroy_replay").await.unwrap();
        let (alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        let genesis_json = latest_envelope(&db, subject.as_str()).unwrap().json;

        let resource = db.get_resource(&subject).await.unwrap();
        let mut builder = crate::commit::CommitBuilder::new(subject.clone());
        builder.destroy(true);
        let destroy = builder.sign(&alice, &db, &resource).await.unwrap();
        let destroy_id = crate::identifiers::commit_subject(destroy.signature.as_deref().unwrap());
        let destroy_json = destroy
            .into_resource(&db)
            .await
            .unwrap()
            .to_json_ad(None)
            .unwrap();
        ingest_commit_json(&db, &destroy_json, &CommitIngestOpts::peer())
            .await
            .unwrap();
        assert!(!db.has_resource_locally(&subject.pure_id()));

        // A peer re-sends the genesis: the subject exists again.
        ingest_commit_json(&db, &genesis_json, &CommitIngestOpts::peer())
            .await
            .unwrap();
        assert!(db.has_resource_locally(&subject.pure_id()));
        assert!(
            envelopes(&db, subject.as_str())
                .iter()
                .all(|e| !e.is_destroy()),
            "latest retention has dropped the destroy envelope"
        );
        assert!(
            db.has_resource_locally(&destroy_id),
            "the destroy commit row survives as the durable record"
        );

        let err = ingest_commit_json(&db, &destroy_json, &CommitIngestOpts::peer())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("already applied here"),
            "expected the replay guard, got: {err}"
        );
        assert!(db.has_resource_locally(&subject.pure_id()));
    }

    /// The genesis commit is kept as a `Tree::Resources` row, so its envelope
    /// is not written a second time; it is rebuilt from the row, and is the
    /// same JSON the envelope would have held.
    #[tokio::test]
    async fn genesis_envelope_is_rebuilt_from_the_commit_row_not_stored() {
        let db = Db::init_temp("envelopes_genesis_not_stored").await.unwrap();
        let mut resource = crate::Resource::new("did:ad:placeholder".into());
        resource
            .set(urls::NAME.into(), Value::String("hallo".into()), &db)
            .await
            .unwrap();
        let response = resource.save_as_genesis(&db).await.unwrap();
        let subject = response
            .resource_new
            .as_ref()
            .unwrap()
            .get_subject()
            .clone();
        let written = response.commit_resource.to_json_ad(None).unwrap();

        assert!(
            db.kv
                .scan_prefix(Tree::Envelopes, &prefix(subject.as_str()))
                .next()
                .is_none(),
            "nothing is stored in the envelope tree for a fresh resource"
        );
        let kept = envelopes(&db, subject.as_str());
        assert_eq!(kept.len(), 1);
        assert_eq!(
            kept[0].json, written,
            "the rebuilt envelope is the commit as signed"
        );
        assert_eq!(
            kept[0].commit_id(),
            response.commit_resource.get_subject().as_str().to_string()
        );

        // Once an edit replaces it, `latest` keeps only the edit.
        signed_edit(&db, &subject, "edited").await;
        let kept = envelopes(&db, subject.as_str());
        assert_eq!(kept.len(), 1);
        assert_ne!(kept[0].json, written);
    }

    /// Under `all` the genesis envelope is history: the edit that follows
    /// writes it out so it stays beside the new one.
    #[tokio::test]
    async fn all_retention_keeps_the_genesis_envelope_through_an_edit() {
        let db = Db::init_temp("envelopes_genesis_all").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let mut resource = crate::Resource::new("did:ad:placeholder".into());
        resource
            .set(urls::NAME.into(), Value::String("hallo".into()), &db)
            .await
            .unwrap();
        let response = resource.save_as_genesis(&db).await.unwrap();
        let subject = response
            .resource_new
            .as_ref()
            .unwrap()
            .get_subject()
            .clone();
        let written = response.commit_resource.to_json_ad(None).unwrap();

        signed_edit(&db, &subject, "edited").await;
        let kept = envelopes(&db, subject.as_str());
        assert_eq!(kept.len(), 2, "genesis and the edit");
        assert_eq!(kept[0].json, written);
    }

    // ---- compact rows ----

    /// The raw stored values of a resource's envelope rows, key order.
    fn raw_rows(db: &Db, subject: &crate::Subject) -> Vec<(Vec<u8>, Vec<u8>)> {
        db.kv
            .scan_prefix(Tree::Envelopes, &prefix(subject.as_str()))
            .map(|row| row.unwrap())
            .collect()
    }

    fn is_compact(value: &[u8]) -> bool {
        value.first() == Some(&COMPACT_V1)
    }

    /// A signed edit through `save_locally`; returns the commit JSON-AD as
    /// signed.
    async fn signed_edit_json(db: &Db, subject: &crate::Subject, name: &str) -> String {
        let mut resource = db.get_resource(subject).await.unwrap();
        resource
            .set(urls::NAME.into(), Value::String(name.into()), db)
            .await
            .unwrap();
        let response = resource.save_locally(db).await.unwrap();
        response.commit_resource.to_json_ad(None).unwrap()
    }

    #[tokio::test]
    async fn rust_path_edits_are_stored_compact_and_read_back_byte_identical() {
        let db = Db::init_temp("envelopes_compact_rust").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        let mut written = Vec::new();
        for name in ["one", "two", "three", "four"] {
            written.push(signed_edit_json(&db, &subject, name).await);
        }

        let rows = raw_rows(&db, &subject);
        let compact = rows.iter().filter(|(_, v)| is_compact(v)).count();
        assert_eq!(compact, written.len(), "every edit is compact: {rows:?}");
        let read = envelopes(&db, subject.as_str());
        let read_edits: Vec<&str> = read
            .iter()
            .rev()
            .take(written.len())
            .rev()
            .map(|e| e.json.as_str())
            .collect();
        assert_eq!(read_edits, written, "rebuilt JSON is what was signed");

        let report = attribute_history(&db, subject.as_str()).await.unwrap();
        assert!(report.attributions.iter().all(|a| a.verified), "{report:?}");
        assert!(report.complete, "{report:?}");
        assert_eq!(report.unreadable, 0);
    }

    #[tokio::test]
    async fn browser_style_delta_is_stored_compact_and_verifies_after_rebuild() {
        let db = Db::init_temp("envelopes_compact_delta").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        // Something earlier than the delta so the commit carries a
        // `previousCommit`-free, single-peer range after the genesis.
        signed_edit(&db, &subject, "before").await;

        let resource = db.get_resource(&subject).await.unwrap();
        let snapshot = resource.materialized_state().expect("stored state");
        let base_vv = crate::loro::AtomicLoroDoc::from_snapshot(&snapshot)
            .unwrap()
            .oplog_vv();
        let client_doc = crate::loro::AtomicLoroDoc::from_snapshot(&snapshot).unwrap();
        client_doc
            .set_property(urls::NAME, &Value::String("delta edit".into()))
            .unwrap();
        client_doc.commit_with_message("c-01test-compact-token");
        let delta = client_doc.export_updates_since(&base_vv);
        let mut builder = crate::commit::CommitBuilder::new(subject.clone());
        builder.set_loro_update(delta);
        let commit = builder.sign(&alice, &db, &resource).await.unwrap();
        let json = commit
            .into_resource(&db)
            .await
            .unwrap()
            .to_json_ad(None)
            .unwrap();
        ingest_commit_json(&db, &json, &CommitIngestOpts::peer())
            .await
            .expect("the delta applies");

        let rows = raw_rows(&db, &subject);
        assert!(is_compact(&rows.last().unwrap().1), "the delta is compact");
        let last = latest_envelope(&db, subject.as_str()).unwrap();
        assert_eq!(last.json, json, "byte-identical to the signed commit");

        // The rebuilt JSON verifies on its own, through the same path a
        // receiving peer uses.
        let resource = crate::parse::parse_json_ad_commit_resource(&last.json, &db)
            .await
            .unwrap();
        let commit = crate::commit::Commit::from_resource(resource).unwrap();
        commit
            .validate_signature(&db)
            .await
            .expect("signature holds");

        let report = attribute_history(&db, subject.as_str()).await.unwrap();
        assert_eq!(
            report.attributions.last().unwrap().tokens,
            vec!["c-01test-compact-token".to_string()]
        );
        assert!(report.complete, "{report:?}");
    }

    #[tokio::test]
    async fn a_legacy_json_row_still_reads() {
        let db = Db::init_temp("envelopes_compact_legacy").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        let json = signed_edit_json(&db, &subject, "old era").await;
        let (key, value) = raw_rows(&db, &subject).pop().unwrap();
        assert!(is_compact(&value));

        db.kv
            .insert(Tree::Envelopes, &key, json.as_bytes())
            .unwrap();
        assert_eq!(latest_envelope(&db, subject.as_str()).unwrap().json, json);
        let report = attribute_history(&db, subject.as_str()).await.unwrap();
        assert!(report.complete, "{report:?}");
        assert!(report.attributions.iter().all(|a| a.verified));
    }

    #[tokio::test]
    async fn import_stores_compact_and_falls_back_to_json_when_the_rebuild_differs() {
        let db = Db::init_temp("envelopes_compact_import").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        let json = signed_edit_json(&db, &subject, "imported").await;
        let (key, _) = raw_rows(&db, &subject).pop().unwrap();

        // The canonical bytes come back compact.
        db.kv.remove(Tree::Envelopes, &key).unwrap();
        import_envelope(&db, subject.as_str(), &json).await.unwrap();
        let (new_key, value) = raw_rows(&db, &subject).pop().unwrap();
        assert_eq!(new_key, key);
        assert!(is_compact(&value), "import stores compact too");
        assert_eq!(latest_envelope(&db, subject.as_str()).unwrap().json, json);

        // The same commit pretty-printed is a valid envelope (the signature
        // covers the canonical form) but differs byte for byte: kept whole.
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let respelled = serde_json::to_string_pretty(&value).unwrap();
        assert_ne!(respelled, json);
        db.kv.remove(Tree::Envelopes, &key).unwrap();
        import_envelope(&db, subject.as_str(), &respelled)
            .await
            .unwrap();
        let (_, value) = raw_rows(&db, &subject).pop().unwrap();
        assert!(!is_compact(&value), "a different spelling is kept whole");
        assert_eq!(value, respelled.as_bytes());
    }

    #[tokio::test]
    async fn a_genesis_snapshot_is_never_compact() {
        let db = Db::init_temp("envelopes_compact_other").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        signed_edit(&db, &subject, "x").await;
        let genesis = envelopes(&db, subject.as_str()).remove(0);
        assert!(
            compact_row(
                &subject.pure_id(),
                genesis.created_at,
                &genesis.signature,
                &genesis.json,
            )
            .is_none(),
            "a genesis carries fields the compact row does not rebuild"
        );
    }

    #[tokio::test]
    async fn compact_rows_survive_the_loro_doc_being_rewritten_or_removed() {
        let db = Db::init_temp("envelopes_compact_no_doc").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        let mut written = Vec::new();
        // Same-peer commits merge into one Loro change in the oplog.
        for name in ["a", "b", "c", "d"] {
            written.push(signed_edit_json(&db, &subject, name).await);
        }
        db.kv
            .remove(Tree::LoroSnapshots, subject.pure_id().as_bytes())
            .unwrap();
        let read: Vec<String> = envelopes(&db, subject.as_str())
            .into_iter()
            .rev()
            .take(written.len())
            .rev()
            .map(|e| e.json)
            .collect();
        assert_eq!(read, written, "the proof does not depend on the doc");
        assert_eq!(envelopes_checked(&db, subject.as_str()).unreadable, 0);
    }

    #[tokio::test]
    async fn a_mixed_legacy_and_compact_tree_reads_in_order() {
        let db = Db::init_temp("envelopes_compact_mixed").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        let mut written = Vec::new();
        for name in ["one", "two", "three"] {
            written.push(signed_edit_json(&db, &subject, name).await);
        }
        let rows = raw_rows(&db, &subject);
        let middle = &rows[rows.len() - 2];
        db.kv
            .insert(Tree::Envelopes, &middle.0, written[1].as_bytes())
            .unwrap();
        let after = raw_rows(&db, &subject);
        assert!(!is_compact(&after[after.len() - 2].1));
        assert!(is_compact(&after[after.len() - 1].1));
        let read: Vec<String> = envelopes(&db, subject.as_str())
            .into_iter()
            .rev()
            .take(3)
            .rev()
            .map(|e| e.json)
            .collect();
        assert_eq!(read, written);
        let report = attribute_history(&db, subject.as_str()).await.unwrap();
        assert!(report.complete && report.unreadable == 0, "{report:?}");
    }

    #[tokio::test]
    async fn history_of_a_destroyed_subject_still_reads() {
        let db = Db::init_temp("envelopes_compact_destroy").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        signed_edit(&db, &subject, "before").await;
        let resource = db.get_resource(&subject).await.unwrap();
        let mut builder = crate::commit::CommitBuilder::new(subject.clone());
        builder.destroy(true);
        let commit = builder.sign(&alice, &db, &resource).await.unwrap();
        let json = commit
            .into_resource(&db)
            .await
            .unwrap()
            .to_json_ad(None)
            .unwrap();
        ingest_commit_json(&db, &json, &CommitIngestOpts::peer())
            .await
            .unwrap();

        let found = envelopes_checked(&db, subject.as_str());
        assert_eq!(found.unreadable, 0);
        assert!(found.rows.len() >= 2);
        assert!(latest_envelope(&db, subject.as_str()).unwrap().is_destroy());
        let report = attribute_history(&db, subject.as_str()).await.unwrap();
        assert_eq!(report.unreadable, 0, "{report:?}");
    }

    #[tokio::test]
    async fn a_subject_with_a_drive_hint_stays_readable_in_either_form() {
        let db = Db::init_temp("envelopes_compact_hint").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        let json = signed_edit_json(&db, &subject, "hinted").await;
        let (row_key, _) = raw_rows(&db, &subject).pop().unwrap();
        let (_, created_at, signature) = parse_key(&row_key).unwrap();

        let hinted = format!("{}?drive={drive}", subject.pure_id());
        let mut map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&json).unwrap();
        map.insert(urls::SUBJECT.into(), hinted.clone().into());
        let hinted_json = serde_json::to_string(&map).unwrap();
        // The row's subject is the pure id, so the hinted spelling is not
        // rebuilt by the compact form and is stored whole.
        let value = value_for(&hinted, created_at, &signature, hinted_json.clone());
        assert_eq!(value, hinted_json.as_bytes());
        db.kv.insert(Tree::Envelopes, &row_key, &value).unwrap();
        assert_eq!(
            latest_envelope(&db, subject.as_str()).unwrap().json,
            hinted_json
        );
        // The plain spelling of the same commit compacts and reads back.
        let value = value_for(subject.as_str(), created_at, &signature, json.clone());
        assert!(is_compact(&value));
        db.kv.insert(Tree::Envelopes, &row_key, &value).unwrap();
        assert_eq!(latest_envelope(&db, subject.as_str()).unwrap().json, json);
    }

    #[tokio::test]
    async fn an_unreadable_row_is_reported_and_never_replaces_the_newest() {
        let db = Db::init_temp("envelopes_compact_unreadable").await.unwrap();
        db.set_envelope_retention(EnvelopeRetention::All);
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        signed_edit(&db, &subject, "x").await;
        signed_edit(&db, &subject, "y").await;
        let (newest_key, value) = raw_rows(&db, &subject).pop().unwrap();
        assert!(is_compact(&value));
        let before = attribute_history(&db, subject.as_str()).await.unwrap();
        assert!(before.complete && before.unreadable == 0, "{before:?}");
        let readable_before = envelopes_checked(&db, subject.as_str()).rows.len();

        // A truncated compact row.
        db.kv
            .insert(Tree::Envelopes, &newest_key, &value[..10])
            .unwrap();
        let found = envelopes_checked(&db, subject.as_str());
        assert_eq!(found.unreadable, 1);
        assert!(found.newest_unreadable);
        assert_eq!(found.rows.len(), readable_before - 1);
        assert!(
            latest_envelope(&db, subject.as_str()).is_none(),
            "an older row is not the latest"
        );
        let after = attribute_history(&db, subject.as_str()).await.unwrap();
        assert_eq!(after.unreadable, 1);
        assert!(!after.complete);

        // Latest retention: a local write and an import both leave the
        // unreadable newest row in place.
        db.set_envelope_retention(EnvelopeRetention::Latest);
        let json = signed_edit_json(&db, &subject, "z").await;
        assert!(
            raw_rows(&db, &subject)
                .iter()
                .any(|(k, _)| *k == newest_key),
            "the unreadable row was not pruned"
        );
        let (new_key, _) = raw_rows(&db, &subject).pop().unwrap();
        db.kv.remove(Tree::Envelopes, &new_key).unwrap();
        import_envelope(&db, subject.as_str(), &json).await.unwrap();
        assert!(raw_rows(&db, &subject)
            .iter()
            .any(|(k, _)| *k == newest_key));
    }

    /// Changes in a resource's stored Loro doc with no commit token: what a
    /// node writes on its own on top of the signed commits.
    fn untokened_changes(db: &Db, subject: &crate::Subject) -> usize {
        let doc =
            crate::loro::AtomicLoroDoc::from_snapshot(&db.get_loro_snapshot(subject).unwrap())
                .unwrap();
        let mut untokened = 0;
        doc.doc()
            .travel_change_ancestors(
                &doc.doc().oplog_frontiers().iter().collect::<Vec<_>>(),
                &mut |change| {
                    if change.message.as_deref().is_none_or(str::is_empty) {
                        untokened += 1;
                    }
                    std::ops::ControlFlow::Continue(())
                },
            )
            .unwrap();
        untokened
    }

    fn doc_stamp(db: &Db, subject: &crate::Subject) -> Option<loro::LoroValue> {
        crate::loro::AtomicLoroDoc::from_snapshot(&db.get_loro_snapshot(subject).unwrap())
            .unwrap()
            .get_all_properties()
            .get(urls::LAST_COMMIT)
            .cloned()
    }

    /// `lastCommit` is served from the newest envelope and is not a change in
    /// the document: a commit adds exactly the author's change to the oplog.
    #[tokio::test]
    async fn last_commit_is_derived_and_adds_nothing_to_the_oplog() {
        let db = Db::init_temp("envelopes_derived_last_commit")
            .await
            .unwrap();
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;

        let size = |db: &Db| db.get_loro_snapshot(&subject).unwrap().len();
        let (changes_before, bytes_before) = (
            crate::loro::AtomicLoroDoc::from_snapshot(&db.get_loro_snapshot(&subject).unwrap())
                .unwrap()
                .doc()
                .len_changes(),
            size(&db),
        );
        signed_edit(&db, &subject, "one").await;
        let doc =
            crate::loro::AtomicLoroDoc::from_snapshot(&db.get_loro_snapshot(&subject).unwrap())
                .unwrap();
        println!(
            "OPLOG one small edit: snapshot {bytes_before} B -> {} B, changes {changes_before} -> {}",
            size(&db),
            doc.doc().len_changes()
        );
        assert_eq!(
            doc.doc().len_changes(),
            changes_before + 1,
            "one commit, one change: no server-peer bookkeeping"
        );
        assert!(
            doc_stamp(&db, &subject).is_none(),
            "nothing is stamped in the doc"
        );

        let latest = latest_envelope(&db, subject.as_str()).unwrap();
        let served = db.get_resource(&subject).await.unwrap();
        assert_eq!(
            served.get(urls::LAST_COMMIT).unwrap().to_string(),
            latest.commit_id()
        );
        assert_eq!(
            latest_commit_id(&db, subject.as_str()).as_deref(),
            Some(latest.commit_id().as_str())
        );
        // The shallow read (the stored row) serves it too.
        assert_eq!(
            db.get_resource_shallow(&subject)
                .unwrap()
                .get(urls::LAST_COMMIT)
                .unwrap()
                .to_string(),
            latest.commit_id()
        );
        assert_eq!(untokened_changes(&db, &subject), 0);
    }

    /// A row written from a synced doc carries no `lastCommit`: the newest
    /// envelope is served, then the genesis commit.
    #[tokio::test]
    async fn last_commit_falls_back_to_the_envelope_when_the_row_has_none() {
        let db = Db::init_temp("envelopes_last_commit_fallback")
            .await
            .unwrap();
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        signed_edit(&db, &subject, "one").await;
        let latest = latest_envelope(&db, subject.as_str()).unwrap();

        let mut stripped = db.get_resource(&subject).await.unwrap();
        stripped.remove_propval(urls::LAST_COMMIT).unwrap();
        db.add_resource_opts(&stripped, false, true, true)
            .await
            .unwrap();
        assert!(db
            .get_resource_shallow(&subject)
            .unwrap()
            .get(urls::LAST_COMMIT)
            .is_err());
        assert_eq!(
            db.get_resource(&subject)
                .await
                .unwrap()
                .get(urls::LAST_COMMIT)
                .unwrap()
                .to_string(),
            latest.commit_id()
        );
    }

    /// A doc that still holds the old stamp: the derived value wins, and the
    /// stamp stands when nothing derived exists.
    #[tokio::test]
    async fn a_stamp_in_an_old_doc_loses_to_the_derived_value() {
        let db = Db::init_temp("envelopes_old_stamp").await.unwrap();
        let (_alice, drive) = db.setup("Alice").await.unwrap();
        let subject = child(&db, &drive).await;
        signed_edit(&db, &subject, "one").await;
        let latest = latest_envelope(&db, subject.as_str()).unwrap();

        let old = "atomic:commit:OLDSTAMP";
        let doc =
            crate::loro::AtomicLoroDoc::from_snapshot(&db.get_loro_snapshot(&subject).unwrap())
                .unwrap();
        doc.set_property(urls::LAST_COMMIT, &Value::AtomicUrl(old.into()))
            .unwrap();
        let mut resource = db.get_resource(&subject).await.unwrap();
        resource.apply_state_doc(doc).unwrap();
        assert_eq!(resource.get(urls::LAST_COMMIT).unwrap().to_string(), old);
        db.add_resource_opts(&resource, false, true, true)
            .await
            .unwrap();
        assert!(
            doc_stamp(&db, &subject).is_some(),
            "premise: the stored doc holds the stamp"
        );

        // The row now says `old` too (it was written from the stamped doc);
        // the next signed commit derives a fresh value over it.
        signed_edit(&db, &subject, "two").await;
        let newest = latest_envelope(&db, subject.as_str()).unwrap();
        assert_ne!(newest.commit_id(), latest.commit_id());
        assert_eq!(
            db.get_resource(&subject)
                .await
                .unwrap()
                .get(urls::LAST_COMMIT)
                .unwrap()
                .to_string(),
            newest.commit_id(),
            "the derived value wins over the doc's stamp"
        );

        // Without a row value, the stamp the doc holds stands over a
        // missing envelope.
        let mut stamped = resource;
        stamped.set_derived_last_commit(Value::AtomicUrl(old.into()));
        db.serve_derived_last_commit(&mut stamped, "internal:/no-envelopes", None);
        assert_eq!(stamped.get(urls::LAST_COMMIT).unwrap().to_string(), old);
    }
    #[test]
    fn recovery_span_ops_preserves_present_empty_and_nonempty_original_vectors() {
        let client = crate::loro::ChangeSpan {
            peer: 9_007_199_254_740_997,
            counter: 4,
            length: 3,
        };
        let server = crate::loro::ChangeSpan {
            peer: 7,
            counter: 12,
            length: 1,
        };
        let signature = "native-signature";
        let history = RecoveryHistoryAttribution {
            retention: "all",
            complete: true,
            attributions: vec![Attribution {
                signer: "atomic:agent:signer".into(),
                created_at: 42,
                signature: signature.into(),
                commit_id: crate::identifiers::commit_subject(signature),
                verified: true,
                tokens: vec!["display-only".into()],
                destroy: false,
                genesis: false,
                spans: Some(vec![client]),
                server_spans: Some(vec![server]),
            }],
        };

        let ops = recovery_span_ops("did:ad:subject", &history).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].tree, Tree::EnvelopeSpans);
        assert!(matches!(ops[0].method, Method::Insert));
        assert_eq!(
            ops[0].key,
            key("did:ad:subject", 42, signature),
            "source span metadata is keyed by the signed envelope identity"
        );
        let stored: crate::loro::EnvelopeSpans =
            serde_json::from_slice(ops[0].val.as_deref().unwrap()).unwrap();
        assert_eq!(stored.client, vec![client]);
        assert_eq!(stored.server, vec![server]);

        let mut empty = history;
        empty.attributions[0].spans = Some(Vec::new());
        empty.attributions[0].server_spans = Some(Vec::new());
        let ops = recovery_span_ops("did:ad:subject", &empty).unwrap();
        let stored: crate::loro::EnvelopeSpans =
            serde_json::from_slice(ops[0].val.as_deref().unwrap()).unwrap();
        assert!(stored.client.is_empty());
        assert!(stored.server.is_empty());
    }

    #[test]
    fn recovery_span_ops_rejects_missing_or_unverified_source_metadata() {
        let signature = "native-signature";
        let attribution = Attribution {
            signer: "atomic:agent:signer".into(),
            created_at: 42,
            signature: signature.into(),
            commit_id: crate::identifiers::commit_subject(signature),
            verified: true,
            tokens: Vec::new(),
            destroy: false,
            genesis: false,
            spans: Some(Vec::new()),
            server_spans: Some(Vec::new()),
        };
        let mut history = RecoveryHistoryAttribution {
            retention: "all",
            attributions: vec![attribution],
            complete: true,
        };
        history.attributions[0].spans = None;
        assert!(recovery_span_ops("did:ad:subject", &history).is_err());

        history.attributions[0].spans = Some(Vec::new());
        history.attributions[0].verified = false;
        assert!(recovery_span_ops("did:ad:subject", &history).is_err());

        history.attributions[0].verified = true;
        history.complete = false;
        assert!(recovery_span_ops("did:ad:subject", &history).is_err());
    }

    #[test]
    fn recovery_span_ops_rejects_overlapping_source_ranges() {
        let signature = "native-signature";
        let history = RecoveryHistoryAttribution {
            retention: "all",
            complete: true,
            attributions: vec![Attribution {
                signer: "atomic:agent:signer".into(),
                created_at: 42,
                signature: signature.into(),
                commit_id: crate::identifiers::commit_subject(signature),
                verified: true,
                tokens: Vec::new(),
                destroy: false,
                genesis: false,
                spans: Some(vec![crate::loro::ChangeSpan {
                    peer: 7,
                    counter: 2,
                    length: 4,
                }]),
                server_spans: Some(vec![crate::loro::ChangeSpan {
                    peer: 7,
                    counter: 5,
                    length: 2,
                }]),
            }],
        };

        assert!(recovery_span_ops("did:ad:subject", &history).is_err());
    }

    #[test]
    fn recovery_span_ops_rejects_duplicate_signed_envelopes() {
        let signature = "native-signature";
        let attribution = Attribution {
            signer: "atomic:agent:signer".into(),
            created_at: 42,
            signature: signature.into(),
            commit_id: crate::identifiers::commit_subject(signature),
            verified: true,
            tokens: Vec::new(),
            destroy: false,
            genesis: false,
            spans: Some(Vec::new()),
            server_spans: Some(Vec::new()),
        };
        let history = RecoveryHistoryAttribution {
            retention: "all",
            attributions: vec![attribution.clone(), attribution],
            complete: true,
        };

        assert!(recovery_span_ops("did:ad:subject", &history).is_err());
    }
}
