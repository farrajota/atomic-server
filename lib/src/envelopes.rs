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
            "latest" => Some(Self::Latest),
            "all" | "full" => Some(Self::All),
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
    /// the same value `lastCommit` stamps on the resource.
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

fn decode(key: &[u8], value: Vec<u8>) -> Option<StoredEnvelope> {
    let subject_end = key.iter().position(|b| *b == 0)?;
    let subject = std::str::from_utf8(&key[..subject_end]).ok()?.to_string();
    let rest = &key[subject_end + 1..];
    if rest.len() < 9 || rest[8] != 0 {
        return None;
    }
    let created_at = u64::from_be_bytes(rest[..8].try_into().ok()?) as i64;
    let signature = std::str::from_utf8(&rest[9..]).ok()?.to_string();
    let json = String::from_utf8(value).ok()?;
    Some(StoredEnvelope {
        subject,
        created_at,
        signature,
        json,
    })
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

    // The ops this envelope introduced, recorded beside it whatever the
    // retention. The genesis envelope's spans are kept even when the
    // envelope itself is not (it is rebuilt from its commit row). Only the
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
    if store.envelope_retention() == EnvelopeRetention::Latest {
        prune_spans_except(store, subject, &new_key, transaction)?;
    }

    if genesis_is_kept_as_row(response) {
        return Ok(());
    }
    // Later commits: under `All` the genesis envelope is history that has to
    // stay, so write it out before something newer sits beside it.
    if store.envelope_retention() == EnvelopeRetention::All {
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

    if store.envelope_retention() == EnvelopeRetention::Latest {
        for existing in store.kv.scan_prefix(Tree::Envelopes, &prefix(subject)) {
            let (old_key, _) = existing?;
            if old_key != new_key {
                transaction.push(Operation {
                    tree: Tree::Envelopes,
                    method: Method::Delete,
                    key: old_key,
                    val: None,
                });
            }
        }
    }

    transaction.push(Operation {
        tree: Tree::Envelopes,
        method: Method::Insert,
        key: new_key,
        val: Some(json.into_bytes()),
    });
    Ok(())
}

/// Queue the removal of every recorded span row of `subject` except `keep`
/// and the genesis envelope's. Under `latest` retention only the newest
/// envelope (and the genesis, rebuilt from its commit row) is attributable.
fn prune_spans_except(
    store: &Db,
    subject: &str,
    keep: &[u8],
    transaction: &mut Transaction,
) -> AtomicResult<()> {
    let genesis = genesis_signature(subject);
    for existing in store.kv.scan_prefix(Tree::EnvelopeSpans, &prefix(subject)) {
        let (old_key, _) = existing?;
        let is_genesis = decode(&old_key, Vec::new())
            .is_some_and(|row| Some(&row.signature) == genesis.as_ref());
        if old_key != keep && !is_genesis {
            transaction.push(Operation {
                tree: Tree::EnvelopeSpans,
                method: Method::Delete,
                key: old_key,
                val: None,
            });
        }
    }
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

/// Every retained envelope of a resource, oldest first.
pub fn envelopes(store: &Db, subject: &str) -> Vec<StoredEnvelope> {
    let mut rows: Vec<StoredEnvelope> = store
        .kv
        .scan_prefix(Tree::Envelopes, &prefix(subject))
        .filter_map(|entry| entry.ok())
        .filter_map(|(k, v)| decode(&k, v))
        .collect();
    if rows.is_empty() {
        rows.extend(genesis_envelope_from_row(store, subject));
    }
    rows
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
pub fn latest_envelope(store: &Db, subject: &str) -> Option<StoredEnvelope> {
    envelopes(store, subject).into_iter().last()
}

/// The retained envelopes of each subject as commit JSON-AD, for a bulk
/// push or a vault pack. Subjects with none are absent.
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
    if store.envelope_retention() == EnvelopeRetention::Latest {
        let existing = envelopes(store, commit.subject.as_str());
        if existing.iter().any(|e| e.created_at > commit.created_at) {
            // A newer envelope is already the latest; the incoming one is
            // history this node chose not to keep.
            return Ok(());
        }
        let genesis = genesis_signature(commit.subject.as_str());
        for old in existing {
            let old_key = key(&old.subject, old.created_at, &old.signature);
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
        val: Some(json.as_bytes().to_vec()),
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
    /// `did:ad:commit:<signature>`, the value `lastCommit` stamps.
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
    /// Ops the server wrote while applying this envelope (`lastCommit`, the
    /// derived `drive`, the creator's `write`): server bookkeeping, not the
    /// signer's.
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
    /// Retention this node runs; tells a reader whether missing attributions
    /// are a gap or a policy.
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
    let retention = store.envelope_retention().as_str();
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
    let mut retained = envelopes(store, subject);
    if let Some(genesis) = genesis_envelope_from_row(store, subject) {
        if !retained.iter().any(|e| e.signature == genesis.signature) {
            retained.insert(0, genesis);
        }
    }

    let mut attributions: Vec<Attribution> = Vec::new();
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
    history: &RecoveryHistoryAttribution,
    envelopes: &[StoredEnvelope],
) -> AtomicResult<Vec<Operation>> {
    if !history.complete || history.attributions.len() != envelopes.len() {
        return Err("Recovery attribution inventory is incomplete".into());
    }
    history.attributions.iter().zip(envelopes).map(|(attribution, envelope)| {
        if !attribution.verified || attribution.signature != envelope.signature {
            return Err("Recovery attribution does not match its verified envelope".into());
        }
        let spans = crate::loro::EnvelopeSpans {
            client: attribution.spans.clone().ok_or("Recovery signed spans are missing")?,
            server: attribution.server_spans.clone().ok_or("Recovery server spans are missing")?,
        };
        Ok(Operation {
            tree: Tree::EnvelopeSpans,
            method: Method::Insert,
            key: key(&envelope.subject, envelope.created_at, &envelope.signature),
            val: Some(serde_json::to_vec(&spans)?),
        })
    }).collect()
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
                std::io::Error::other(
                    "Recovery operation proof exceeds its 1 GiB work limit",
                )
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

fn is_genesis_carrier(token: &str, attributions: &[Attribution]) -> bool {
    let token = crate::identifiers::canonicalize_scheme(token);
    attributions.iter().any(|entry| {
        entry.genesis && crate::identifiers::canonicalize_scheme(&entry.signer) == token
    })
}

struct RecoveryProofScope<'a> {
    store: &'a Db,
    members: &'a std::collections::HashSet<String>,
}

struct RecoveryOperationProof {
    changes: Vec<loro::JsonChange>,
    ranges: Vec<(loro::VersionVector, loro::VersionVector)>,
    stamps: Vec<(serde_json::Value, serde_json::Value, usize)>,
    prerequisites: Vec<loro::VersionVector>,
    bookkeeping_ranges: Vec<(u64, i32, i32)>,
    compared_bytes: usize,
    comparisons: usize,
    document_work: usize,
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
            stamps: Vec::new(),
            prerequisites: Vec::with_capacity(envelopes),
            bookkeeping_ranges: Vec::new(),
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
            return Err("Recovery snapshot operations differ from original signed native bytes".into());
        }
        self.ranges.push((start, end));
        let prerequisite = self.prerequisites.len();
        self.prerequisites.push(after);
        let mut signed_resource = crate::Resource::new(commit.subject.to_string());
        signed_resource.apply_state_doc(original)?;
        crate::recovery::validate_explicit_dependencies(
            store,
            &crate::recovery::required_dependencies(store, &signed_resource),
            members,
        )?;

        let template = crate::loro::AtomicLoroDoc::new();
        let signature = commit
            .signature
            .as_deref()
            .ok_or("Recovery envelope has no signature")?;
        template.set_property(
            crate::urls::LAST_COMMIT,
            &crate::Value::AtomicUrl(crate::identifiers::commit_subject(signature).into()),
        )?;
        if commit.is_genesis == Some(true) {
            template.set_property(
                crate::urls::CREATED_AT,
                &crate::Value::Timestamp(commit.created_at),
            )?;
        }
        template.commit();
        for change in template
            .doc()
            .export_json_updates_without_peer_compression(
                &loro::VersionVector::default(),
                &template.oplog_vv(),
            )
            .changes
        {
            for op in change.ops {
                self.stamps.push((
                    serde_json::to_value(&op.container)?,
                    serde_json::to_value(&op.content)?,
                    prerequisite,
                ));
            }
        }
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

    fn prove_bookkeeping(
        &mut self,
        scope: &RecoveryProofScope<'_>,
        stored: &crate::loro::AtomicLoroDoc,
        property_root: &serde_json::Value,
        receipts: &std::collections::HashMap<String, usize>,
        attributions: &[Attribution],
        subject: &str,
    ) -> AtomicResult<()> {
        let store = scope.store;
        let members = scope.members;
        for change_index in 0..self.changes.len() {
            let change = &self.changes[change_index];
            if change.msg.is_some() {
                continue;
            }
            let mut needs_native_proof = false;
            for op in &change.ops {
                Self::charge_work(&mut self.comparisons, self.ranges.len() + self.stamps.len())?;
                let container = serde_json::to_value(&op.container)?;
                let content = serde_json::to_value(&op.content)?;
                if !self
                    .stamps
                    .iter()
                    .any(|(root, stamp, _)| *root == container && *stamp == content)
                {
                    needs_native_proof = true;
                    break;
                }
            }
            if !needs_native_proof {
                continue;
            }
            let mut receipt = None;
            for op in &change.ops {
                if let loro::JsonOpContent::Map(loro::JsonMapOp::Insert {
                    key,
                    value: loro::LoroValue::String(value),
                }) = &op.content
                {
                    if key == crate::urls::LAST_COMMIT
                        && serde_json::to_value(&op.container)? == *property_root
                    {
                        if receipt.is_some() {
                            receipt = None;
                            break;
                        }
                        let index = *receipts
                            .get(&crate::identifiers::canonicalize_scheme(value.as_ref()))
                            .ok_or("Recovery retained history is missing a referenced commit envelope")?;
                        receipt = Some((index, value.to_string()));
                    }
                }
            }
            let Some((index, receipt_value)) = receipt else {
                continue;
            };
            if attributions[index].destroy {
                continue;
            }
            Self::charge_work(&mut self.comparisons, self.document_work)?;
            let causal = Self::causal_before(stored, change, change.id.counter)?;
            let before = causal.oplog_vv();
            if !Self::includes(&before, &self.prerequisites[index]) {
                continue;
            }

            let mut previous_receipts = loro::Frontiers::new();
            for previous_change in &self.changes {
                for op in &previous_change.ops {
                    if before
                        .get(&previous_change.id.peer)
                        .copied()
                        .unwrap_or(0)
                        <= op.counter
                    {
                        continue;
                    }
                    if let loro::JsonOpContent::Map(loro::JsonMapOp::Insert { key, .. }) = &op.content {
                        if key == crate::urls::LAST_COMMIT
                            && serde_json::to_value(&op.container)? == *property_root
                        {
                            previous_receipts.push(loro::ID::new(
                                previous_change.id.peer,
                                op.counter,
                            ));
                        }
                    }
                }
            }
            let is_new = previous_receipts.is_empty();
            let mut old = crate::Resource::new(subject.to_string());
            if !is_new {
                old.apply_state_doc(stored.fork_at(
                    &crate::loro::VersionID::from_frontiers(&previous_receipts),
                )?)?;
            }
            let mut resource = crate::Resource::new(subject.to_string());
            resource.apply_state_doc(causal)?;
            let parent = resource.get(crate::urls::PARENT).ok().map(|value| value.to_string());
            let parent_changed = old
                .get(crate::urls::PARENT)
                .ok()
                .map(|value| value.to_string())
                != parent;

            let mut writers = None;
            if is_new
                && attributions[index].genesis
                && resource.get_subject().is_did()
                && !resource.has_class(crate::urls::AGENT)
            {
                let mut grants = resource
                    .get(crate::urls::WRITE)
                    .ok()
                    .and_then(|value| value.to_subjects(None).ok())
                    .unwrap_or_default();
                if !grants.contains(&attributions[index].signer) {
                    grants.push(attributions[index].signer.clone());
                    writers = Some(crate::Value::from(grants));
                }
            }
            let drive = if is_new || parent_changed {
                match parent {
                    Some(parent) => {
                        let stamped = change.ops.iter().find_map(|op| match &op.content {
                            loro::JsonOpContent::Map(loro::JsonMapOp::Insert {
                                key,
                                value: loro::LoroValue::String(value),
                            }) if key == crate::urls::DRIVE_PROP
                                && serde_json::to_value(&op.container).ok().as_ref()
                                    == Some(property_root) => Some(value.to_string()),
                            _ => None,
                        }).or_else(|| resource.get(crate::urls::DRIVE_PROP).ok().map(|value| value.to_string()));
                        recovery_derived_drive(
                            store,
                            &resource,
                            &parent,
                            stamped.as_deref(),
                            property_root,
                            members,
                            &mut self.comparisons,
                        )?
                    }
                    None => None,
                }
            } else {
                None
            };

            for created_at in [false, true] {
                if created_at && (!is_new || !attributions[index].genesis) {
                    continue;
                }
                Self::charge_work(&mut self.comparisons, self.document_work)?;
                let template = resource.build_state_doc()?;
                template.set_peer_id(change.id.peer)?;
                if created_at {
                    template.set_property(
                        crate::urls::CREATED_AT,
                        &crate::Value::Timestamp(attributions[index].created_at),
                    )?;
                }
                if let Some(writers) = &writers {
                    template.set_property(crate::urls::WRITE, writers)?;
                }
                if let Some(drive) = &drive {
                    template.set_property(
                        crate::urls::DRIVE_PROP,
                        &crate::Value::AtomicUrl(drive.as_str().into()),
                    )?;
                }
                template.set_property(
                    crate::urls::LAST_COMMIT,
                    &crate::Value::AtomicUrl(receipt_value.as_str().into()),
                )?;
                template.commit();
                let after = template.oplog_vv();
                let generated = template
                    .doc()
                    .export_json_updates_without_peer_compression(&before, &after);
                let operations: Vec<_> = generated.changes.iter().flat_map(|c| &c.ops).collect();
                let remaining = (crate::recovery::MAX_BUNDLE_BYTES * 64)
                    .checked_sub(self.compared_bytes)
                    .ok_or("Recovery operation proof exceeds its 1 GiB work limit")?;
                let (expected_hash, expected_bytes) = operation_digest(&operations, remaining)?;
                let (actual_hash, actual_bytes) =
                    operation_digest(&change.ops, remaining - expected_bytes)?;
                self.compared_bytes += expected_bytes + actual_bytes;
                if expected_bytes == actual_bytes && expected_hash == actual_hash {
                    self.bookkeeping_ranges.push((
                        change.id.peer,
                        change.id.counter,
                        after.get(&change.id.peer).copied().unwrap_or(0),
                    ));
                    break;
                }
            }
        }
        Ok(())
    }

    fn finish(
        &mut self,
        store: &Db,
        members: &std::collections::HashSet<String>,
        subject: &str,
        stored: &crate::loro::AtomicLoroDoc,
        attributions: &mut [Attribution],
    ) -> AtomicResult<()> {
        let receipts: std::collections::HashMap<String, usize> = attributions
            .iter()
            .enumerate()
            .map(|(index, attribution)| {
                (
                    crate::identifiers::canonicalize_scheme(
                        &crate::identifiers::commit_subject(&attribution.signature),
                    ),
                    index,
                )
            })
            .collect();
        let mut receipt_order = vec![u64::MAX; attributions.len()];
        let root_template = crate::loro::AtomicLoroDoc::new();
        root_template.set_property(
            crate::urls::LAST_COMMIT,
            &crate::Value::AtomicUrl("atomic:commit:template".into()),
        )?;
        root_template.commit();
        let roots = root_template
            .doc()
            .export_json_updates_without_peer_compression(
                &loro::VersionVector::default(),
                &root_template.oplog_vv(),
            );
        let property_root = roots
            .changes
            .iter()
            .flat_map(|change| &change.ops)
            .find_map(|op| match &op.content {
                loro::JsonOpContent::Map(loro::JsonMapOp::Insert {
                    key,
                    value: loro::LoroValue::String(value),
                }) if key == crate::urls::LAST_COMMIT && value.as_ref() == "atomic:commit:template" => {
                    Some(serde_json::to_value(&op.container))
                }
                _ => None,
            })
            .ok_or("Recovery receipt template is missing its property map")??;
        for attribution in attributions.iter_mut() {
            attribution.tokens.clear();
        }
        for change in &self.changes {
            for op in &change.ops {
                if let loro::JsonOpContent::Map(loro::JsonMapOp::Insert {
                    key,
                    value: loro::LoroValue::String(value),
                }) = &op.content
                {
                    if key == crate::urls::LAST_COMMIT
                        && serde_json::to_value(&op.container)? == property_root
                    {
                        let index = receipts
                            .get(&crate::identifiers::canonicalize_scheme(value.as_ref()))
                            .ok_or("Recovery retained history is missing a referenced commit envelope")?;
                        Self::charge_work(&mut self.comparisons, self.document_work)?;
                        let causal = Self::causal_before(stored, change, op.counter)?.oplog_vv();
                        if !Self::includes(&causal, &self.prerequisites[*index]) {
                            return Err("Recovery apply receipt precedes its signed native update".into());
                        }
                        let order = (change.lamport as u64)
                            .checked_add((op.counter - change.id.counter) as u64)
                            .ok_or("Recovery receipt causal order overflow")?;
                        receipt_order[*index] = receipt_order[*index].min(order);
                    }
                }
            }
        }
        for (index, attribution) in attributions.iter().enumerate() {
            if !attribution.destroy && receipt_order[index] == u64::MAX {
                return Err("Recovery envelope has no original apply receipt in the snapshot".into());
            }
        }
        self.prove_bookkeeping(
            &RecoveryProofScope { store, members },
            stored,
            &property_root,
            &receipts,
            attributions,
            subject,
        )?;
        for change in &self.changes {
            if let Some(token) = &change.msg {
                Self::charge_work(&mut self.comparisons, self.ranges.len())?;
                let owner = self
                    .ranges
                    .iter()
                    .enumerate()
                    .filter(|(index, (start, end))| {
                        (!is_genesis_carrier(token, attributions) || attributions[*index].genesis)
                            && Self::carries(start, end, change.id.peer, change.id.counter)
                    })
                    .min_by_key(|(index, _)| receipt_order[*index])
                    .map(|(index, _)| index);
                if let Some(owner) = owner {
                    if !attributions[owner].tokens.contains(token) {
                        attributions[owner].tokens.push(token.clone());
                    }
                }
            }
        }
        for change in &self.changes {
            let mut causal_vv: Option<loro::VersionVector> = None;
            for op in &change.ops {
                let op_end = op
                    .counter
                    .checked_add(
                        i32::try_from(op.content.op_len())
                            .map_err(|_| "Recovery operation length overflow")?,
                    )
                    .ok_or("Recovery operation length overflow")?;
                let mut covered_until = op.counter;
                while covered_until < op_end {
                    Self::charge_work(&mut self.comparisons, self.ranges.len())?;
                    let next_signed = self
                        .ranges
                        .iter()
                        .filter(|(start, end)| {
                            Self::carries(start, end, change.id.peer, covered_until)
                        })
                        .map(|(_, end)| end.get(&change.id.peer).copied().unwrap_or(0))
                        .max()
                        .unwrap_or(covered_until);
                    let next_bookkeeping = self
                        .bookkeeping_ranges
                        .iter()
                        .filter(|(peer, start, end)| {
                            *peer == change.id.peer && covered_until >= *start && covered_until < *end
                        })
                        .map(|(_, _, end)| *end)
                        .max()
                        .unwrap_or(covered_until);
                    let next = next_signed.max(next_bookkeeping);
                    if next == covered_until {
                        break;
                    }
                    covered_until = next;
                }
                if covered_until < op_end || change.msg.is_none() {
                    if causal_vv.is_none() {
                        let mut causal = loro::Frontiers::new();
                        for dependency in &change.deps {
                            causal.push(*dependency);
                        }
                        if op.counter > change.id.counter {
                            causal.push(loro::ID::new(change.id.peer, op.counter - 1));
                        }
                        causal_vv = Some(if causal.is_empty() {
                            loro::VersionVector::default()
                        } else {
                            stored
                                .fork_at(&crate::loro::VersionID::from_frontiers(&causal))?
                                .oplog_vv()
                        });
                    }
                    let container = serde_json::to_value(&op.container)?;
                    let content = serde_json::to_value(&op.content)?;
                    Self::charge_work(&mut self.comparisons, self.stamps.len())?;
                    let allowed = change.msg.is_none()
                        && self.stamps.iter().any(|(root, stamp, prerequisite)| {
                            *root == container
                                && *stamp == content
                                && self.prerequisites[*prerequisite].iter().all(|(peer, end)| {
                                    let causal_end = causal_vv
                                        .as_ref()
                                        .unwrap()
                                        .get(peer)
                                        .copied()
                                        .unwrap_or(0);
                                    let causal_end = if *peer == change.id.peer {
                                        causal_end.max(op.counter)
                                    } else {
                                        causal_end
                                    };
                                    causal_end >= *end
                                })
                        });
                    if allowed {
                        self.bookkeeping_ranges.push((change.id.peer, op.counter, op_end));
                    } else if covered_until < op_end {
                        return Err(
                            "Unproved unsigned recovery operation in snapshot content or container"
                                .into(),
                        );
                    }
                }
            }
        }
        for attribution in attributions.iter_mut() {
            attribution.spans = Some(Vec::new());
            attribution.server_spans = Some(Vec::new());
        }
        for change in stored.changes() {
            let length = u32::try_from(change.len).map_err(|_| "Recovery change length overflow")?;
            let end = change.counter.checked_add(
                i32::try_from(length).map_err(|_| "Recovery change length overflow")?,
            ).ok_or("Recovery change length overflow")?;
            Self::charge_work(&mut self.comparisons, self.bookkeeping_ranges.len() + self.ranges.len())?;
            let mut cursor = change.counter;
            while cursor < end {
                Self::charge_work(&mut self.comparisons, self.bookkeeping_ranges.len())?;
                let next = self.bookkeeping_ranges.iter()
                    .filter(|(peer, start, stop)| *peer == change.peer && *start <= cursor && cursor < *stop)
                    .map(|(_, _, stop)| *stop)
                    .max().unwrap_or(cursor);
                if next == cursor { break; }
                cursor = next;
            }
            let span = crate::loro::ChangeSpan { peer: change.peer, counter: change.counter, length };
            if cursor >= end {
                attributions[0].server_spans.as_mut().unwrap().push(span);
                continue;
            }
            if self.bookkeeping_ranges.iter().any(|(peer, start, stop)| {
                *peer == change.peer && *start < end && *stop > change.counter
            }) {
                return Err("Recovery change mixes signed and server operations".into());
            }
            let owner = self.ranges.iter().enumerate()
                .filter(|(_, (start, stop))| {
                    Self::carries(start, stop, change.peer, change.counter)
                        && stop.get(&change.peer).copied().unwrap_or(0) >= end
                })
                .min_by_key(|(index, _)| receipt_order[*index])
                .map(|(index, _)| index)
                .ok_or("Recovery change has no single verified signed owner")?;
            attributions[owner].spans.as_mut().unwrap().push(span);
        }
        Ok(())
    }
}

fn recovery_derived_drive(
    store: &Db,
    resource: &crate::Resource,
    parent: &str,
    candidate: Option<&str>,
    property_root: &serde_json::Value,
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
                let subject = crate::Subject::from_raw(&current, store.get_base_domain().as_deref());
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
        for change in &schema.changes {
            for op in &change.ops {
                if let loro::JsonOpContent::Map(loro::JsonMapOp::Insert { key, .. }) = &op.content {
                    if key == crate::urls::LAST_COMMIT
                        && serde_json::to_value(&op.container)? == *property_root
                    {
                        versions.push(loro::ID::new(change.id.peer, op.counter));
                    }
                }
            }
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
                state.apply_state_doc(doc.fork_at(
                    &crate::loro::VersionID::from_frontiers(&loro::Frontiers::from_id(version)),
                )?)?;
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
                    && scheduled.get(&parent_id).is_none_or(|best| next_depth < *best)
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
                if candidate.is_none_or(|value| identity(value) == identity(&derived)) {
                    crate::recovery::validate_explicit_dependencies(
                        store,
                        std::slice::from_ref(&derived),
                        members,
                    )?;
                    return Ok(Some(candidate.unwrap_or(&derived).to_string()));
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
    let envelopes = recovery_envelopes_from_view(
        store,
        view,
        subject,
        envelope_limit,
        envelope_byte_limit,
    )?;
    if envelopes.is_empty() {
        return Err(format!("Recovery history is missing for {pure}").into());
    }

    let mut attributions = Vec::with_capacity(envelopes.len());
    let mut operation_proof = RecoveryOperationProof::new(&stored_doc, envelopes.len())?;
    for envelope in &envelopes {
        let local = crate::recovery::LocalRecoveryStore::new(store);
        if envelope.subject != pure {
            return Err("Recovery envelope subject does not match its key".into());
        }
        check_recovery_envelope_fields(store, &envelope.json)?;
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
        let is_genesis = commit.is_genesis == Some(true);
        operation_proof.verify_update(store, members, &stored_doc, &commit)?;
        attributions.push(Attribution {
            signer: commit.signer.to_string(),
            created_at: commit.created_at,
            signature: envelope.signature.clone(),
            commit_id: envelope.commit_id(),
            verified: true,
            tokens: Vec::new(),
            destroy: commit.destroy.unwrap_or(false),
            genesis: is_genesis,
            spans: None,
            server_spans: None,
        });
    }

    if genesis_signature(&pure).is_none() || !attributions.iter().any(|entry| entry.genesis) {
        return Err(format!("Recovery genesis history is incomplete for {pure}").into());
    }
    operation_proof.finish(store, members, subject, &stored_doc, &mut attributions)?;
    let stored_tokens: Vec<String> = stored_doc
        .get_history()
        .into_iter()
        .filter_map(|change| change.message)
        .collect();
    let complete = !attributions.is_empty()
        && stored_tokens
            .iter()
            .filter(|token| !is_genesis_carrier(token, &attributions))
            .all(|token| {
                attributions
                    .iter()
                    .any(|entry| entry.verified && entry.tokens.contains(token))
            });
    if !complete {
        return Err(format!("Recovery retained history is incomplete for {pure}").into());
    }
    Ok((
        RecoveryHistoryAttribution {
            retention: EnvelopeRetention::All.as_str(),
            attributions,
            complete,
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
        let envelope = decode(&key, value)
            .ok_or_else(|| format!("Malformed retained recovery envelope for {wanted}"))?;
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
}
