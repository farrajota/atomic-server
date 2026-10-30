//! Bounded native recovery bundles. Unsupported stores, incomplete history, missing support
//! resources, and malformed payloads fail the whole operation rather than dropping data.

use crate::{
    agents::ForAgent,
    db::{
        kv_store::{KvIter, KvReadView, KvStore},
        trees::Tree,
        Db,
    },
    envelopes::{EnvelopeRetention, RecoveryHistoryAttribution},
    errors::AtomicResult,
    resources::Resource,
    storelike::Storelike,
    subject::Subject,
    sync::protocol,
    urls, Value,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{self, Write},
    sync::Arc,
};

pub const MAX_RESOURCES: usize = 256;
pub const MAX_REQUEST_BYTES: usize = 128 * 1024;
pub const MAX_BUNDLE_BYTES: usize = 16 * 1024 * 1024;
const MAX_ENVELOPES: usize = u16::MAX as usize;
const BUNDLE_MAGIC: &[u8; 8] = b"ATRECOV1";
const BUNDLE_HEADER_BYTES: usize = 8 + 4 + 32;
const CODEC_DRIVE: &str = "atomic:recovery-payload:v1";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryRequest {
    pub version: u32,
    pub resources: Vec<RecoverySelection>,
    pub allowed_drives: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoverySelection {
    pub subject: String,
    pub expected_last_commit: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryManifest {
    pub version: u32,
    pub codec: String,
    pub codec_drive: String,
    pub retention: String,
    pub base_domain: Option<String>,
    pub allowed_drives: Vec<String>,
    pub resources: Vec<RecoveryManifestResource>,
    pub envelope_count: usize,
    pub payload_bytes: usize,
    pub payload_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryManifestResource {
    pub subject: String,
    pub last_commit: String,
    pub snapshot_length: usize,
    pub snapshot_sha256: String,
    pub version_vector: BTreeMap<String, i32>,
    pub envelopes: Vec<RecoveryManifestEnvelope>,
    pub history: RecoveryHistory,
    pub description_sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryManifestEnvelope {
    pub created_at: i64,
    pub signature: String,
    pub length: usize,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryHistory {
    pub retention: String,
    pub complete: bool,
    pub attributions: Vec<RecoveryAttribution>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryAttribution {
    pub signer: String,
    pub created_at: i64,
    pub signature: String,
    pub verified: bool,
    pub tokens: Vec<String>,
    pub destroy: bool,
    pub genesis: bool,
}

#[derive(Clone, Debug)]
pub struct RecoveryEnvelope {
    pub subject: String,
    pub created_at: i64,
    pub signature: String,
    pub json: String,
}

#[derive(Clone, Debug)]
pub struct RecoveryResource {
    pub subject: String,
    pub snapshot: Vec<u8>,
    pub envelopes: Vec<RecoveryEnvelope>,
}

/// A bundle is only constructible by validating its framing, checksums, membership, snapshots,
/// version vectors, retained envelope inventory, and original-history evidence.
#[derive(Clone, Debug)]
pub struct RecoveryBundle {
    manifest: RecoveryManifest,
    resources: Vec<RecoveryResource>,
}

impl RecoveryBundle {
    pub fn manifest(&self) -> &RecoveryManifest {
        &self.manifest
    }
}

struct ReadViewStore {
    view: Arc<dyn KvReadView>,
    max_bytes: usize,
}

impl KvStore for ReadViewStore {
    fn read_view(&self) -> AtomicResult<Arc<dyn KvReadView>> {
        Ok(self.view.clone())
    }

    fn get(&self, tree: Tree, key: &[u8]) -> AtomicResult<Option<Vec<u8>>> {
        self.view.get_bounded(tree, key, self.max_bytes)
    }

    fn insert(&self, _tree: Tree, _key: &[u8], _val: &[u8]) -> AtomicResult<()> {
        Err("Recovery authorization view is read-only".into())
    }

    fn write_recovery_snapshot(&self, _key: &[u8], _snapshot: &[u8]) -> AtomicResult<()> {
        Err("Recovery authorization view is read-only".into())
    }

    fn remove(&self, _tree: Tree, _key: &[u8]) -> AtomicResult<()> {
        Err("Recovery authorization view is read-only".into())
    }

    fn contains_key(&self, tree: Tree, key: &[u8]) -> AtomicResult<bool> {
        Ok(self.get(tree, key)?.is_some())
    }

    fn scan_prefix(&self, tree: Tree, prefix: &[u8]) -> KvIter {
        match self.view.scan_prefix_bounded(tree, prefix, MAX_ENVELOPES, self.max_bytes) {
            Ok(rows) => Box::new(rows.into_iter().map(Ok)),
            Err(error) => Box::new(std::iter::once(Err(error))),
        }
    }

    fn range(&self, _tree: Tree, _start: Vec<u8>, _end: Vec<u8>, _reverse: bool) -> KvIter {
        Box::new(std::iter::once(Err(
            "Ranges are unavailable in a recovery authorization view".into(),
        )))
    }

    fn iter_tree(&self, _tree: Tree) -> KvIter {
        Box::new(std::iter::once(Err(
            "Whole-tree iteration is unavailable in a recovery authorization view".into(),
        )))
    }

    fn clear_tree(&self, _tree: Tree) -> AtomicResult<()> {
        Err("Recovery authorization view is read-only".into())
    }

    fn apply_batch(&self, _operations: &[crate::db::trees::Operation]) -> AtomicResult<()> {
        Err("Recovery authorization view is read-only".into())
    }

    fn flush(&self) -> AtomicResult<()> {
        Ok(())
    }

    fn commit_batch(&self) -> AtomicResult<()> {
        Err("Recovery authorization view is read-only".into())
    }

    fn len(&self, _tree: Tree) -> AtomicResult<usize> {
        Err("Tree lengths are unavailable in a recovery authorization view".into())
    }
}

/// This resolver never fetches externally, including when ordinary shallow reads would fall back.
pub(crate) struct LocalRecoveryStore<'a> {
    db: &'a Db,
    reads: std::sync::atomic::AtomicUsize,
}

impl<'a> LocalRecoveryStore<'a> {
    pub(crate) fn new(db: &'a Db) -> Self {
        Self {
            db,
            reads: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl Storelike for LocalRecoveryStore<'_> {
    fn get_base_domain(&self) -> Option<String> {
        self.db.get_base_domain()
    }

    fn normalize_subject(&self, subject: &Subject) -> Subject {
        self.db.normalize_subject(subject)
    }

    fn get_default_agent(&self) -> AtomicResult<crate::agents::Agent> {
        self.db.get_default_agent()
    }

    async fn get_resource(&self, subject: &Subject) -> AtomicResult<Resource> {
        if self
            .reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            >= MAX_RESOURCES * 4
        {
            return Err("Recovery local resolution exceeds its 1024-read budget".into());
        }
        self.db
            .get_resource_shallow(subject)
            .map_err(|_| format!("Recovery resource is not available locally: {subject}").into())
    }

    async fn fetch_resource(
        &self,
        _subject: &str,
        _agent: Option<&crate::agents::Agent>,
    ) -> AtomicResult<Resource> {
        Err("Network resolution is unavailable during recovery".into())
    }

    async fn add_atoms(&self, _atoms: Vec<crate::Atom>) -> AtomicResult<()> {
        Err("Recovery resolver is read-only".into())
    }

    async fn add_resource_opts(
        &self,
        _resource: &Resource,
        _required: bool,
        _index: bool,
        _overwrite: bool,
    ) -> AtomicResult<()> {
        Err("Recovery resolver is read-only".into())
    }

    async fn remove_resource(&self, _subject: &Subject) -> AtomicResult<()> {
        Err("Recovery resolver is read-only".into())
    }

    fn add_drive_mapping(&self, _host: &str, _drive: &Value) -> AtomicResult<()> {
        Err("Recovery resolver is read-only".into())
    }

    fn remove_drive_mapping(&self, _host: &str) -> AtomicResult<()> {
        Err("Recovery resolver is read-only".into())
    }

    async fn query(
        &self,
        _query: &crate::storelike::Query,
    ) -> AtomicResult<crate::storelike::QueryResult> {
        Err("Queries are unavailable during recovery".into())
    }

    fn all_resources(&self, _external: bool) -> Box<dyn Iterator<Item = Resource> + Send> {
        panic!("Recovery must not enumerate the database")
    }

    fn set_default_agent(&self, _agent: crate::agents::Agent) {
        panic!("Recovery resolver must not change identities")
    }
}

#[derive(Clone)]
struct ScopeInfo {
    subject: String,
    parent: Option<String>,
    drive: Option<String>,
    is_drive: bool,
    dependencies: Vec<String>,
}

fn read_only_store(store: &Db, view: Arc<dyn KvReadView>) -> Db {
    let mut pinned = store.clone();
    pinned.kv = Arc::new(ReadViewStore {
        view,
        max_bytes: MAX_BUNDLE_BYTES,
    });
    pinned
}

pub fn parse_request(bytes: &[u8]) -> AtomicResult<RecoveryRequest> {
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(format!("Recovery request exceeds {MAX_REQUEST_BYTES} bytes").into());
    }
    let request: RecoveryRequest = serde_json::from_slice(bytes)
        .map_err(|_| crate::errors::AtomicError::parse_error("Invalid recovery request", None, None))?;
    if request.version != 1 {
        return Err("Unsupported recovery request version".into());
    }
    if request.resources.is_empty() || request.resources.len() > MAX_RESOURCES {
        return Err(format!("Recovery request must select 1 to {MAX_RESOURCES} resources").into());
    }
    if request.allowed_drives.is_empty() || request.allowed_drives.len() > MAX_RESOURCES {
        return Err("Recovery request must declare 1 to 256 allowed drives".into());
    }
    if request.resources.iter().any(|entry| {
        entry.subject.trim().is_empty() || entry.expected_last_commit.trim().is_empty()
    }) || request.allowed_drives.iter().any(|drive| drive.trim().is_empty())
    {
        return Err("Recovery request subjects, revisions, and drives must be explicit".into());
    }
    Ok(request)
}

pub async fn export_bundle(
    store: &Db,
    request: &RecoveryRequest,
    actor: &ForAgent,
) -> AtomicResult<Vec<u8>> {
    if !matches!(actor, ForAgent::AgentSubject(_)) {
        return Err(crate::errors::AtomicError::unauthorized(
            "Recovery export requires an authenticated agent".into(),
        ));
    }
    if request.version != 1
        || request.resources.is_empty()
        || request.resources.len() > MAX_RESOURCES
        || request.allowed_drives.is_empty()
        || request.allowed_drives.len() > MAX_RESOURCES
    {
        return Err("Invalid bounded recovery request".into());
    }
    let view = store.kv.read_view()?;
    export_bundle_from_view(store, view, request, actor).await
}

async fn export_bundle_from_view(
    source: &Db,
    view: Arc<dyn KvReadView>,
    request: &RecoveryRequest,
    actor: &ForAgent,
) -> AtomicResult<Vec<u8>> {
    let pinned = read_only_store(source, view.clone());
    let mut selected = HashSet::with_capacity(request.resources.len());
    for selection in &request.resources {
        let normalized = pinned.normalize_subject(&Subject::from_raw(
            &selection.subject,
            pinned.get_base_domain().as_deref(),
        ));
        if !selected.insert(canonical_id(&pinned, normalized.as_str())) {
            return Err("Recovery request contains duplicate canonical resources".into());
        }
    }
    let mut allowed_drives = Vec::with_capacity(request.allowed_drives.len());
    let mut allowed_set = HashSet::with_capacity(request.allowed_drives.len());
    for drive in &request.allowed_drives {
        let id = canonical_id(&pinned, drive);
        if !allowed_set.insert(id.clone()) {
            return Err("Recovery request contains duplicate canonical drives".into());
        }
        allowed_drives.push(id);
    }

    let mut resources = Vec::with_capacity(request.resources.len());
    let mut scope_infos = Vec::with_capacity(request.resources.len());
    let mut manifest_entries = Vec::with_capacity(request.resources.len());
    let mut payload_bytes = 0usize;
    let mut envelope_count = 0usize;

    for selection in &request.resources {
        let normalized = pinned.normalize_subject(&Subject::from_raw(
            &selection.subject,
            pinned.get_base_domain().as_deref(),
        ));
        let subject = canonical_id(&pinned, normalized.as_str());
        if is_pinned_builtin(&subject) {
            return Err("Pinned built-ins are local dependencies, not recovery payload rows".into());
        }

        let mut resource = pinned.get_resource_shallow(&normalized)?;
        let remaining = MAX_BUNDLE_BYTES
            .checked_sub(payload_bytes)
            .ok_or("Recovery payload exceeded its byte limit")?;
        let snapshot = view
            .get_bounded(Tree::LoroSnapshots, subject.as_bytes(), remaining)?
            .ok_or_else(|| format!("Recovery snapshot is missing for {subject}"))?;
        if snapshot.is_empty() {
            return Err(format!("Recovery snapshot is empty for {subject}").into());
        }
        let doc = strict_snapshot_doc(&snapshot)
            .map_err(|error| format!("Recovery snapshot is invalid for {subject}: {error}"))?;
        resource.apply_state_doc(doc)?;
        if canonical_id(&pinned, resource.get_subject().as_str()) != subject {
            return Err("Recovery resource projection changed its canonical subject".into());
        }
        atomic_read_check(&pinned, &resource, actor).await?;
        if resource.has_class(urls::COMMIT) || resource.get_subject().is_commit_did() {
            return Err("Commit rows are native history evidence, not selected recovery resources".into());
        }
        if has_unsupported_blob_payload(&resource, &subject) {
            return Err(format!("Recovery does not support selected file/blob payloads ({subject})").into());
        }

        let last_commit = resource
            .get(urls::LAST_COMMIT)
            .map_err(|_| format!("Recovery revision is missing for {subject}"))?
            .to_string();
        if canonical_id(&pinned, &last_commit)
            != canonical_id(&pinned, &selection.expected_last_commit)
        {
            return Err(format!("Recovery revision mismatch for {subject}").into());
        }

        let description_sha256 = description_hash(&resource);
        let version_vector = version_vector(&snapshot)?;
        let dependencies = required_dependencies(&pinned, &resource);
        let scope = scope_info(&pinned, &resource, subject.clone(), dependencies)?;
        validate_explicit_dependencies(&pinned, &scope.dependencies, &selected)?;

        let room = MAX_BUNDLE_BYTES
            .checked_sub(payload_bytes)
            .and_then(|remaining| remaining.checked_sub(snapshot.len()))
            .ok_or("Recovery payload exceeded its byte limit")?;
        payload_bytes = payload_bytes
            .checked_add(snapshot.len())
            .ok_or("Recovery payload exceeded its byte limit")?;
        let (history, stored_envelopes) = crate::envelopes::recovery_history_from_view(
            &pinned,
            view.as_ref(),
            &subject,
            &snapshot,
            MAX_ENVELOPES,
            room,
            &selected,
        )
        .await?;
        if history.retention != "all" || !history.complete {
            return Err(format!("Recovery history is incomplete for {subject}").into());
        }
        envelope_count = envelope_count
            .checked_add(stored_envelopes.len())
            .filter(|count| *count <= MAX_ENVELOPES)
            .ok_or("Recovery bundle exceeds the native envelope count limit")?;
        for envelope in &stored_envelopes {
            payload_bytes = payload_bytes
                .checked_add(envelope.json.len())
                .filter(|bytes| *bytes <= MAX_BUNDLE_BYTES)
                .ok_or("Recovery payload exceeded its byte limit")?;
        }

        let history_proof = history_proof(history);
        let mut envelope_metadata = Vec::with_capacity(stored_envelopes.len());
        for envelope in &stored_envelopes {
            envelope_metadata.push(RecoveryManifestEnvelope {
                created_at: envelope.created_at,
                signature: envelope.signature.clone(),
                length: envelope.json.len(),
                sha256: sha256(envelope.json.as_bytes()),
            });
        }
        let signer_dependencies = history_proof
            .attributions
            .iter()
            .map(|attribution| attribution.signer.clone())
            .collect::<Vec<_>>();
        validate_explicit_dependencies(&pinned, &signer_dependencies, &selected)?;

        manifest_entries.push(RecoveryManifestResource {
            subject: subject.clone(),
            last_commit,
            snapshot_length: snapshot.len(),
            snapshot_sha256: sha256(&snapshot),
            version_vector,
            envelopes: envelope_metadata,
            history: history_proof,
            description_sha256,
        });
        let envelopes = stored_envelopes
            .into_iter()
            .map(|envelope| RecoveryEnvelope {
                subject: subject.clone(),
                created_at: envelope.created_at,
                signature: envelope.signature,
                json: envelope.json,
            })
            .collect();
        resources.push(RecoveryResource {
            subject: subject.clone(),
            snapshot,
            envelopes,
        });
        scope_infos.push(scope);
    }

    validate_allowed_drive_scopes(&pinned, &scope_infos, &allowed_drives, &selected)?;
    revision_fence(source, &manifest_entries)?;
    let payload = encode_native_payload(&resources)?;
    let manifest = RecoveryManifest {
        version: 1,
        codec: "SYNC_PUSH".into(),
        codec_drive: CODEC_DRIVE.into(),
        retention: EnvelopeRetention::All.as_str().into(),
        base_domain: pinned.get_base_domain(),
        allowed_drives,
        resources: manifest_entries,
        envelope_count,
        payload_bytes: payload.len(),
        payload_sha256: sha256(&payload),
    };
    seal_bundle(&manifest, &payload)
}

fn encode_native_payload(resources: &[RecoveryResource]) -> AtomicResult<Vec<u8>> {
    let envelope_count: usize = resources.iter().map(|resource| resource.envelopes.len()).sum();
    if envelope_count > MAX_ENVELOPES {
        return Err("Recovery bundle exceeds the native envelope count limit".into());
    }
    let entries: Vec<(&str, &[u8])> = resources
        .iter()
        .map(|resource| (resource.subject.as_str(), resource.snapshot.as_slice()))
        .collect();
    let envelopes: Vec<(&str, &str)> = resources
        .iter()
        .flat_map(|resource| {
            resource
                .envelopes
                .iter()
                .map(move |envelope| (envelope.subject.as_str(), envelope.json.as_str()))
        })
        .collect();
    let estimated = native_payload_length(&entries, &envelopes)?;
    if estimated > MAX_BUNDLE_BYTES.saturating_sub(BUNDLE_HEADER_BYTES) {
        return Err("Recovery bundle exceeds the 16 MiB limit".into());
    }
    Ok(protocol::encode_sync_push_with_envelopes(
        CODEC_DRIVE,
        &entries,
        &envelopes,
        true,
    ))
}

fn native_payload_length(
    entries: &[(&str, &[u8])],
    envelopes: &[(&str, &str)],
) -> AtomicResult<usize> {
    let mut total = 1usize
        .checked_add(2)
        .and_then(|length| length.checked_add(CODEC_DRIVE.len()))
        .and_then(|length| length.checked_add(1 + 2))
        .ok_or("Recovery payload size overflow")?;
    for (subject, snapshot) in entries {
        if subject.len() > u16::MAX as usize || snapshot.len() > u32::MAX as usize {
            return Err("Recovery entry exceeds the native codec field limits".into());
        }
        total = total
            .checked_add(2 + subject.len() + 4)
            .and_then(|length| length.checked_add(snapshot.len()))
            .ok_or("Recovery payload size overflow")?;
    }
    if !envelopes.is_empty() {
        total = total.checked_add(2).ok_or("Recovery payload size overflow")?;
    }
    for (subject, json) in envelopes {
        if subject.len() > u16::MAX as usize || json.len() > u32::MAX as usize {
            return Err("Recovery envelope exceeds the native codec field limits".into());
        }
        total = total
            .checked_add(2 + subject.len() + 4)
            .and_then(|length| length.checked_add(json.len()))
            .ok_or("Recovery payload size overflow")?;
    }
    Ok(total)
}

fn seal_bundle(manifest: &RecoveryManifest, payload: &[u8]) -> AtomicResult<Vec<u8>> {
    let max_manifest = MAX_BUNDLE_BYTES
        .checked_sub(BUNDLE_HEADER_BYTES)
        .and_then(|limit| limit.checked_sub(payload.len()))
        .ok_or("Recovery bundle exceeds the 16 MiB limit")?;
    let mut writer = LimitedWriter::new(max_manifest);
    serde_json::to_writer(&mut writer, manifest)?;
    let manifest_bytes = writer.into_inner();
    let total = BUNDLE_HEADER_BYTES
        .checked_add(manifest_bytes.len())
        .and_then(|length| length.checked_add(payload.len()))
        .filter(|length| *length <= MAX_BUNDLE_BYTES)
        .ok_or("Recovery bundle exceeds the 16 MiB limit")?;
    let manifest_length = u32::try_from(manifest_bytes.len())
        .map_err(|_| "Recovery manifest exceeds its native length field")?;
    let digest = Sha256::digest(&manifest_bytes);
    let mut bundle = Vec::with_capacity(total);
    bundle.extend_from_slice(BUNDLE_MAGIC);
    bundle.extend_from_slice(&manifest_length.to_be_bytes());
    bundle.extend_from_slice(&digest);
    bundle.extend_from_slice(&manifest_bytes);
    bundle.extend_from_slice(payload);
    Ok(bundle)
}

struct LimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl LimitedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(64 * 1024)),
            limit,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self.bytes.len().checked_add(bytes.len()).ok_or_else(|| {
            io::Error::other("recovery manifest size overflow")
        })?;
        if next > self.limit {
            return Err(io::Error::other(
                "recovery manifest exceeds bundle limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn decode_bundle(bytes: &[u8]) -> AtomicResult<RecoveryBundle> {
    if bytes.len() > MAX_BUNDLE_BYTES {
        return Err("Recovery bundle exceeds the 16 MiB limit".into());
    }
    if bytes.len() < BUNDLE_HEADER_BYTES || bytes.get(..8) != Some(BUNDLE_MAGIC.as_slice()) {
        return Err("Invalid or truncated recovery bundle header".into());
    }
    let manifest_length = u32::from_be_bytes(
        bytes[8..12]
            .try_into()
            .map_err(|_| "Invalid recovery manifest length")?,
    ) as usize;
    let manifest_end = BUNDLE_HEADER_BYTES
        .checked_add(manifest_length)
        .filter(|end| *end <= bytes.len())
        .ok_or("Truncated recovery manifest")?;
    let manifest_bytes = &bytes[BUNDLE_HEADER_BYTES..manifest_end];
    if Sha256::digest(manifest_bytes).as_slice() != &bytes[12..BUNDLE_HEADER_BYTES] {
        return Err("Recovery manifest checksum mismatch".into());
    }
    let manifest: RecoveryManifest = serde_json::from_slice(manifest_bytes)
        .map_err(|_| crate::errors::AtomicError::parse_error("Invalid recovery manifest", None, None))?;
    let payload = &bytes[manifest_end..];
    validate_manifest(&manifest)?;
    if manifest.payload_bytes != payload.len() || sha256(payload) != manifest.payload_sha256 {
        return Err("Recovery native payload length or checksum mismatch".into());
    }
    let resources = decode_native_payload(&manifest, payload)?;
    Ok(RecoveryBundle { manifest, resources })
}

fn validate_manifest(manifest: &RecoveryManifest) -> AtomicResult<()> {
    if manifest.version != 1
        || manifest.codec != "SYNC_PUSH"
        || manifest.codec_drive != CODEC_DRIVE
        || manifest.retention != "all"
    {
        return Err("Unsupported recovery manifest version, codec, or retention".into());
    }
    if manifest.resources.is_empty() || manifest.resources.len() > MAX_RESOURCES {
        return Err("Recovery manifest resource count is invalid".into());
    }
    if manifest.allowed_drives.is_empty() || manifest.allowed_drives.len() > MAX_RESOURCES {
        return Err("Recovery manifest drive count is invalid".into());
    }
    if manifest.envelope_count > MAX_ENVELOPES {
        return Err("Recovery manifest envelope count exceeds the native codec limit".into());
    }
    let mut members = HashSet::with_capacity(manifest.resources.len());
    let mut drives = HashSet::with_capacity(manifest.allowed_drives.len());
    for drive in &manifest.allowed_drives {
        if !drives.insert(canonical_id_without_store(drive)) {
            return Err("Recovery manifest contains duplicate canonical drives".into());
        }
    }
    let mut envelopes = 0usize;
    for entry in &manifest.resources {
        let subject = canonical_id_without_store(&entry.subject);
        if subject != entry.subject || !members.insert(subject) {
            return Err("Recovery manifest membership is noncanonical or duplicated".into());
        }
        if entry.last_commit.trim().is_empty() || entry.snapshot_length == 0 {
            return Err("Recovery manifest has a missing revision or snapshot".into());
        }
        if entry.history.retention != "all"
            || !entry.history.complete
            || entry.history.attributions.is_empty()
            || entry.history.attributions.iter().any(|attribution| !attribution.verified)
            || entry.envelopes.len() != entry.history.attributions.len()
        {
            return Err("Recovery manifest history proof is incomplete".into());
        }
        for (envelope, attribution) in entry.envelopes.iter().zip(&entry.history.attributions) {
            if envelope.signature != attribution.signature
                || envelope.created_at != attribution.created_at
                || envelope.length == 0
            {
                return Err("Recovery manifest envelope and attribution inventories disagree".into());
            }
        }
        if let Some(hash) = &entry.description_sha256 {
            validate_sha256(hash)?;
        }
        validate_sha256(&entry.snapshot_sha256)?;
        for envelope in &entry.envelopes {
            validate_sha256(&envelope.sha256)?;
        }
        envelopes = envelopes
            .checked_add(entry.envelopes.len())
            .filter(|count| *count <= MAX_ENVELOPES)
            .ok_or("Recovery manifest envelope count exceeds the native codec limit")?;
    }
    if envelopes != manifest.envelope_count {
        return Err("Recovery manifest envelope count does not match its membership".into());
    }
    if manifest
        .allowed_drives
        .iter()
        .any(|drive| !members.contains(&canonical_id_without_store(drive)))
    {
        return Err("Every allowed recovery drive must be an explicit resource".into());
    }
    Ok(())
}

fn decode_native_payload(
    manifest: &RecoveryManifest,
    payload: &[u8],
) -> AtomicResult<Vec<RecoveryResource>> {
    if payload.first() != Some(&protocol::tag::SYNC_PUSH) {
        return Err("Recovery payload is not a native SYNC_PUSH frame".into());
    }
    let decoded = protocol::decode_sync_push(&payload[1..])
        .ok_or("Malformed or truncated native recovery payload")?;
    let entries: Vec<(&str, &[u8])> = decoded
        .entries
        .iter()
        .map(|entry| (entry.subject.as_str(), entry.loro_bytes.as_slice()))
        .collect();
    let envelopes: Vec<(&str, &str)> = decoded
        .envelopes
        .iter()
        .map(|envelope| (envelope.subject.as_str(), envelope.json.as_str()))
        .collect();
    if !decoded.last
        || decoded.drive != CODEC_DRIVE
        || protocol::encode_sync_push_with_envelopes(&decoded.drive, &entries, &envelopes, true)
            .as_slice()
            != payload
    {
        return Err("Native recovery payload framing is noncanonical or incomplete".into());
    }
    if decoded.entries.len() != manifest.resources.len()
        || decoded.envelopes.len() != manifest.envelope_count
    {
        return Err("Native recovery payload membership does not match the manifest".into());
    }

    let mut resources = Vec::with_capacity(manifest.resources.len());
    let mut envelope_index = 0usize;
    for (frame_entry, manifest_entry) in decoded.entries.iter().zip(&manifest.resources) {
        if frame_entry.subject != manifest_entry.subject
            || frame_entry.loro_bytes.len() != manifest_entry.snapshot_length
            || sha256(&frame_entry.loro_bytes) != manifest_entry.snapshot_sha256
        {
            return Err("Recovery snapshot membership, length, or hash mismatch".into());
        }
        if version_vector(&frame_entry.loro_bytes)? != manifest_entry.version_vector {
            return Err("Recovery snapshot version vector does not match its manifest".into());
        }
        let resource = resource_from_snapshot(&manifest_entry.subject, &frame_entry.loro_bytes)?;
        if canonical_id_without_store(
            &resource
                .get(urls::LAST_COMMIT)
                .map_err(|_| "Recovery snapshot has no lastCommit")?
                .to_string(),
        ) != canonical_id_without_store(&manifest_entry.last_commit)
        {
            return Err("Recovery snapshot lastCommit does not match its manifest".into());
        }
        if description_hash(&resource) != manifest_entry.description_sha256 {
            return Err("Recovery textual content hash does not match its manifest".into());
        }

        let mut resource_envelopes = Vec::with_capacity(manifest_entry.envelopes.len());
        for expected in &manifest_entry.envelopes {
            let envelope = decoded
                .envelopes
                .get(envelope_index)
                .ok_or("Recovery envelope list is truncated")?;
            envelope_index += 1;
            if envelope.subject != manifest_entry.subject
                || envelope.json.len() != expected.length
                || sha256(envelope.json.as_bytes()) != expected.sha256
            {
                return Err("Recovery envelope membership, length, or hash mismatch".into());
            }
            let json: serde_json::Value = serde_json::from_str(&envelope.json)
                .map_err(|_| crate::errors::AtomicError::parse_error("Invalid signed recovery envelope", None, None))?;
            if json.get(urls::SIGNATURE).and_then(serde_json::Value::as_str)
                != Some(expected.signature.as_str())
                || json
                    .get(urls::CREATED_AT)
                    .and_then(serde_json::Value::as_i64)
                    != Some(expected.created_at)
            {
                return Err("Recovery envelope metadata does not match its signed bytes".into());
            }
            resource_envelopes.push(RecoveryEnvelope {
                subject: envelope.subject.clone(),
                created_at: expected.created_at,
                signature: expected.signature.clone(),
                json: envelope.json.clone(),
            });
        }
        resources.push(RecoveryResource {
            subject: manifest_entry.subject.clone(),
            snapshot: frame_entry.loro_bytes.clone(),
            envelopes: resource_envelopes,
        });
    }
    if envelope_index != decoded.envelopes.len() {
        return Err("Native recovery payload contains unmanifested envelopes".into());
    }
    Ok(resources)
}

pub async fn restore_bundle(store: &Db, bundle: &RecoveryBundle) -> AtomicResult<()> {
    if store.get_base_domain() != bundle.manifest.base_domain {
        return Err("Recovery target base domain does not match the sealed source origin".into());
    }
    store.set_envelope_retention(EnvelopeRetention::All);
    let mut entries = Vec::with_capacity(bundle.resources.len());
    let mut scope_infos = Vec::with_capacity(bundle.resources.len());
    let mut members = HashSet::with_capacity(bundle.resources.len());
    let mut dependencies = Vec::new();

    for (payload, manifest) in bundle.resources.iter().zip(&bundle.manifest.resources) {
        if payload.subject != manifest.subject || !members.insert(payload.subject.clone()) {
            return Err("Recovery bundle has duplicate or mismatched resource membership".into());
        }
        let subject = Subject::from_raw(&payload.subject, store.get_base_domain().as_deref());
        if store.get_resource_shallow(&subject).is_ok() {
            return Err("Fresh recovery target already contains a selected subject".into());
        }
        let mut resource = resource_from_snapshot(&payload.subject, &payload.snapshot)?;
        if has_unsupported_blob_payload(&resource, &payload.subject) {
            return Err("Recovery does not support selected file/blob payloads".into());
        }
        let last_commit = resource
            .get(urls::LAST_COMMIT)
            .map_err(|_| "Recovery snapshot has no lastCommit")?
            .to_string();
        if canonical_id(store, &last_commit) != canonical_id(store, &manifest.last_commit) {
            return Err("Recovery snapshot lastCommit does not match its manifest".into());
        }
        let refs = required_dependencies(store, &resource);
        dependencies.extend(refs.iter().cloned());
        dependencies.extend(
            manifest
                .history
                .attributions
                .iter()
                .map(|attribution| attribution.signer.clone()),
        );
        let info = scope_info(store, &resource, payload.subject.clone(), refs)?;
        scope_infos.push(info);
        resource.set_subject(payload.subject.clone());
        entries.push((resource, Some(payload.snapshot.clone())));
    }

    let allowed_drives: Vec<String> = bundle
        .manifest
        .allowed_drives
        .iter()
        .map(|drive| canonical_id(store, drive))
        .collect();
    validate_allowed_drive_scopes(store, &scope_infos, &allowed_drives, &members)?;
    validate_explicit_dependencies(store, &dependencies, &members)?;

    store.persist_replicated_resources(entries).await?;
    for resource in &bundle.resources {
        store
            .kv
            .write_recovery_snapshot(resource.subject.as_bytes(), &resource.snapshot)?;
    }
    for resource in &bundle.resources {
        for envelope in &resource.envelopes {
            crate::envelopes::import_recovery_envelope(store, &resource.subject, &envelope.json)
                .await?;
        }
    }
    store.flush()?;

    let view = store.kv.read_view()?;
    let pinned = read_only_store(store, view.clone());
    let mut verified_scope = Vec::with_capacity(bundle.resources.len());
    let mut verified_spans = Vec::new();
    for (resource, manifest) in bundle.resources.iter().zip(&bundle.manifest.resources) {
        let snapshot = view
            .get_bounded(Tree::LoroSnapshots, resource.subject.as_bytes(), MAX_BUNDLE_BYTES)?
            .ok_or("Restored recovery snapshot is missing")?;
        if snapshot != resource.snapshot
            || snapshot.len() != manifest.snapshot_length
            || sha256(&snapshot) != manifest.snapshot_sha256
            || version_vector(&snapshot)? != manifest.version_vector
        {
            return Err("Restored recovery snapshot does not preserve original bytes and vectors".into());
        }
        let restored = resource_from_snapshot(&resource.subject, &snapshot)?;
        if canonical_id(&pinned, &restored.get(urls::LAST_COMMIT)?.to_string())
            != canonical_id(&pinned, &manifest.last_commit)
            || description_hash(&restored) != manifest.description_sha256
        {
            return Err("Restored recovery content or revision differs from its manifest".into());
        }
        let refs = required_dependencies(&pinned, &restored);
        verified_scope.push(scope_info(
            &pinned,
            &restored,
            resource.subject.clone(),
            refs,
        )?);
        let (history, stored_envelopes) = crate::envelopes::recovery_history_from_view(
            &pinned,
            view.as_ref(),
            &resource.subject,
            &snapshot,
            MAX_ENVELOPES,
            MAX_BUNDLE_BYTES,
            &members,
        )
        .await?;
        let span_ops = crate::envelopes::recovery_span_ops(&history, &stored_envelopes)?;
        if history_proof(history) != manifest.history
            || stored_envelopes.len() != resource.envelopes.len()
            || stored_envelopes
                .iter()
                .zip(&resource.envelopes)
                .any(|(stored, expected)| {
                    stored.subject != expected.subject
                        || stored.created_at != expected.created_at
                        || stored.signature != expected.signature
                        || stored.json.as_bytes() != expected.json.as_bytes()
                })
        {
            return Err("Restored signed history differs from the original recovery bundle".into());
        }
        verified_spans.extend(span_ops);
    }
    validate_allowed_drive_scopes(&pinned, &verified_scope, &allowed_drives, &members)?;
    validate_explicit_dependencies(&pinned, &dependencies, &members)?;
    drop(pinned);
    drop(view);
    store.kv.apply_batch(&verified_spans)?;
    store.flush()?;
    Ok(())
}

pub(crate) fn strict_snapshot_doc(snapshot: &[u8]) -> AtomicResult<crate::loro::AtomicLoroDoc> {
    let doc = crate::loro::AtomicLoroDoc::new();
    if doc.import_update_status(snapshot)? {
        return Err("Recovery snapshot contains pending causal operations".into());
    }
    let expected = crate::loro::AtomicLoroDoc::vv_map_from_snapshot(snapshot)?;
    let actual = doc.oplog_vv();
    if expected.len() != actual.iter().count()
        || expected.iter().any(|(peer, counter)| {
            peer.parse::<u64>()
                .ok()
                .and_then(|peer| actual.get(&peer).copied())
                != Some(*counter)
        })
    {
        return Err("Recovery snapshot does not materialize its complete version vector".into());
    }
    Ok(doc)
}

fn resource_from_snapshot(subject: &str, snapshot: &[u8]) -> AtomicResult<Resource> {
    let doc = strict_snapshot_doc(snapshot)
        .map_err(|error| format!("Invalid Loro snapshot for {subject}: {error}"))?;
    let mut resource = Resource::new(subject.to_string());
    resource.apply_state_doc(doc)?;
    Ok(resource)
}

fn version_vector(snapshot: &[u8]) -> AtomicResult<BTreeMap<String, i32>> {
    Ok(crate::loro::AtomicLoroDoc::vv_map_from_snapshot(snapshot)?
        .into_iter()
        .collect())
}

fn description_hash(resource: &Resource) -> Option<String> {
    match resource.get(urls::DESCRIPTION).ok()? {
        Value::String(text) | Value::Markdown(text) => Some(sha256(text.as_bytes())),
        _ => None,
    }
}

fn history_proof(history: RecoveryHistoryAttribution) -> RecoveryHistory {
    RecoveryHistory {
        retention: history.retention.into(),
        complete: history.complete,
        attributions: history
            .attributions
            .into_iter()
            .map(|attribution| RecoveryAttribution {
                signer: attribution.signer,
                created_at: attribution.created_at,
                signature: attribution.signature,
                verified: attribution.verified,
                tokens: attribution.tokens,
                destroy: attribution.destroy,
                genesis: attribution.genesis,
            })
            .collect(),
    }
}

fn scope_info(
    store: &Db,
    resource: &Resource,
    subject: String,
    dependencies: Vec<String>,
) -> AtomicResult<ScopeInfo> {
    let parent = resource
        .get(urls::PARENT)
        .ok()
        .and_then(value_subject)
        .map(|parent| canonical_id(store, &parent));
    let drive = resource
        .get(urls::DRIVE_PROP)
        .ok()
        .and_then(value_subject)
        .map(|drive| canonical_id(store, &drive));
    Ok(ScopeInfo {
        subject,
        parent,
        drive,
        is_drive: resource.has_class(urls::DRIVE),
        dependencies,
    })
}

fn value_subject(value: &Value) -> Option<String> {
    match value {
        Value::AtomicUrl(subject) => Some(subject.to_string()),
        Value::String(subject) if crate::utils::check_valid_url(subject).is_ok() => {
            Some(subject.clone())
        }
        _ => None,
    }
}

pub(crate) fn required_dependencies(store: &Db, resource: &Resource) -> Vec<String> {
    let mut dependencies = Vec::new();
    collect_property_dependencies(store, resource.get_propvals(), &mut dependencies);
    if let Some(parent) = resource.get(urls::PARENT).ok().and_then(value_subject) {
        dependencies.push(canonical_id(store, &parent));
    }
    if let Some(drive) = resource.get(urls::DRIVE_PROP).ok().and_then(value_subject) {
        dependencies.push(canonical_id(store, &drive));
    }
    dependencies.extend(
        resource
            .class_subjects()
            .iter()
            .map(|class| canonical_id(store, class)),
    );
    for property in [
        urls::SOURCE,
        urls::PACKAGE,
        urls::PLUGIN_FILE,
        urls::PREVIOUS_RELEASE,
        urls::RELEASE_PROP,
        urls::FORM_DATA_CLASS,
        urls::FORM_TARGET_TABLE,
        urls::FORM_OWNS_SCHEMA,
        urls::FORM_PAGES,
        urls::FORM_FIELDS,
        urls::FORM_MAPS_TO,
        urls::DEFAULT_ONTOLOGY,
        urls::COLLECTION_PROPERTY,
        urls::COLLECTION_VALUE,
        urls::COLLECTION_MEMBERS,
    ] {
        if let Ok(value) = resource.get(property) {
            collect_references(store, value, &mut dependencies);
        }
    }
    if resource.has_class(urls::CLASS)
        || resource.has_class(urls::PROPERTY)
        || resource.has_class(urls::TABLE)
    {
        for property in [
            urls::REQUIRES,
            urls::RECOMMENDS,
            urls::DATATYPE_PROP,
            urls::CLASSTYPE_PROP,
            urls::ALLOWS_ONLY,
            urls::PROPERTIES,
            urls::CLASSES,
            urls::SCHEMAS,
        ] {
            if let Ok(value) = resource.get(property) {
                collect_references(store, value, &mut dependencies);
            }
        }
    }
    dependencies.sort();
    dependencies.dedup();
    dependencies
}

fn collect_property_dependencies(
    store: &Db,
    propvals: &crate::resources::PropVals,
    out: &mut Vec<String>,
) {
    for (property, value) in propvals {
        out.push(canonical_id(store, property));
        if [
            urls::IS_A,
            urls::SUBJECT,
            urls::PARENT,
            urls::DRIVE_PROP,
            urls::REQUIRES,
            urls::RECOMMENDS,
            urls::DATATYPE_PROP,
            urls::CLASSTYPE_PROP,
            urls::ALLOWS_ONLY,
            urls::PROPERTIES,
            urls::CLASSES,
            urls::SCHEMAS,
        ]
        .contains(&property.as_str())
        {
            collect_references(store, value, out);
        }
        match value {
            Value::NestedResource(crate::values::SubResource::Nested(nested)) => {
                collect_property_dependencies(store, nested, out)
            }
            Value::ResourceArray(items) => {
                for item in items {
                    if let crate::values::SubResource::Nested(nested) = item {
                        collect_property_dependencies(store, nested, out);
                    }
                }
            }
            _ => {}
        }
    }
}

fn collect_references(store: &Db, value: &Value, out: &mut Vec<String>) {
    match value {
        Value::AtomicUrl(subject) => out.push(canonical_id(store, subject.as_str())),
        Value::ResourceArray(items) => {
            for item in items {
                collect_subresource_references(store, item, out);
            }
        }
        Value::NestedResource(item) => collect_subresource_references(store, item, out),
        Value::String(text) if crate::utils::check_valid_url(text).is_ok() => {
            out.push(canonical_id(store, text));
        }
        Value::Json(json) => collect_json_references(store, json, out),
        _ => {}
    }
}

fn collect_subresource_references(
    store: &Db,
    item: &crate::values::SubResource,
    out: &mut Vec<String>,
) {
    match item {
        crate::values::SubResource::Subject(subject) => {
            out.push(canonical_id(store, subject.as_str()));
        }
        crate::values::SubResource::Nested(propvals) => {
            for value in propvals.values() {
                collect_references(store, value, out);
            }
        }
    }
}

fn collect_json_references(store: &Db, value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) if crate::utils::check_valid_url(text).is_ok() => {
            out.push(canonical_id(store, text));
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_json_references(store, item, out);
            }
        }
        serde_json::Value::Object(items) => {
            for item in items.values() {
                collect_json_references(store, item, out);
            }
        }
        _ => {}
    }
}

pub(crate) fn validate_explicit_dependencies(
    store: &Db,
    dependencies: &[String],
    members: &HashSet<String>,
) -> AtomicResult<()> {
    for dependency in dependencies {
        let subject = canonical_id(store, dependency);
        if members.contains(&subject) {
            continue;
        }
        if Subject::from_raw(&subject, store.get_base_domain().as_deref()).is_agent_did() {
            continue;
        }
        if is_pinned_builtin(&subject)
            && store
                .get_resource_shallow(&Subject::from_raw(
                    &subject,
                    store.get_base_domain().as_deref(),
                ))
                .is_ok()
        {
            continue;
        }
        return Err(format!("Recovery dependency is not explicit: {subject}").into());
    }
    Ok(())
}

fn validate_allowed_drive_scopes(
    store: &Db,
    infos: &[ScopeInfo],
    allowed_drives: &[String],
    members: &HashSet<String>,
) -> AtomicResult<()> {
    let allowed: HashSet<String> = allowed_drives
        .iter()
        .map(|drive| canonical_id(store, drive))
        .collect();
    for drive in &allowed {
        let root = infos
            .iter()
            .find(|info| info.subject == *drive && info.is_drive)
            .ok_or("Every allowed drive must be an explicit Drive resource")?;
        if !members.contains(&root.subject) {
            return Err("Every allowed drive must be an explicit resource".into());
        }
    }
    let by_subject: HashMap<&str, &ScopeInfo> = infos
        .iter()
        .map(|info| (info.subject.as_str(), info))
        .collect();
    for info in infos {
        let drive = resolved_drive(info, &by_subject, &mut HashSet::new())?;
        if let Some(drive) = drive {
            if !allowed.contains(&drive) {
                return Err(format!(
                    "Recovery resource is outside the declared drives: {}",
                    info.subject
                )
                .into());
            }
        }
    }
    Ok(())
}

fn resolved_drive(
    info: &ScopeInfo,
    by_subject: &HashMap<&str, &ScopeInfo>,
    visiting: &mut HashSet<String>,
) -> AtomicResult<Option<String>> {
    if info.is_drive {
        return Ok(Some(info.subject.clone()));
    }
    if let Some(drive) = &info.drive {
        return Ok(Some(drive.clone()));
    }
    let Some(parent) = &info.parent else {
        return Ok(None);
    };
    if !visiting.insert(info.subject.clone()) {
        return Err("Recovery hierarchy contains a parent cycle".into());
    }
    let result = match by_subject.get(parent.as_str()) {
        Some(parent_info) => resolved_drive(parent_info, by_subject, visiting),
        None if is_pinned_builtin(parent) => Ok(None),
        None => Err(format!("Recovery parent is not an explicit resource: {parent}").into()),
    };
    visiting.remove(&info.subject);
    result
}

async fn atomic_read_check(store: &Db, resource: &Resource, actor: &ForAgent) -> AtomicResult<()> {
    crate::hierarchy::check_read(&LocalRecoveryStore::new(store), resource, actor)
        .await
        .map_err(|_| {
            crate::errors::AtomicError::unauthorized(
                "Recovery export is not authorized for every selected resource".into(),
            )
        })?;
    Ok(())
}

fn has_unsupported_blob_payload(resource: &Resource, subject: &str) -> bool {
    crate::identifiers::is_blob_id(subject)
        || resource.has_class(urls::FILE)
        || resource.get(urls::BLOB).is_ok()
        || resource.get(urls::CHUNKS).is_ok()
}

fn canonical_id(store: &impl Storelike, subject: &str) -> String {
    let parsed = Subject::from_raw(subject, store.get_base_domain().as_deref());
    let normalized = store.normalize_subject(&parsed);
    crate::identifiers::canonicalize_scheme(&normalized.pure_id())
}

fn canonical_id_without_store(subject: &str) -> String {
    let parsed = Subject::from_raw(subject, None);
    crate::identifiers::canonicalize_scheme(&parsed.pure_id())
}

fn is_pinned_builtin(subject: &str) -> bool {
    let bare = subject
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    [
        "atomicdata.dev/properties/",
        "atomicdata.dev/classes/",
        "atomicdata.dev/datatypes/",
        "atomicdata.dev/class/",
        "atomicdata.dev/property/",
        "atomicdata.dev/methods/",
        "atomicdata.dev/ontology/",
        "atomicdata.dev/01jtjxtsa9syxmfca2zx5gcnmj/class/",
    ]
    .iter()
    .any(|prefix| bare.starts_with(prefix))
        || bare.trim_end_matches('/') == "atomicdata.dev/agents/publicAgent"
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn validate_sha256(value: &str) -> AtomicResult<()> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("Recovery manifest contains an invalid SHA-256 value".into())
    }
}

fn revision_fence(source: &Db, resources: &[RecoveryManifestResource]) -> AtomicResult<()> {
    for expected in resources {
        let subject = Subject::from_raw(&expected.subject, source.get_base_domain().as_deref());
        let current = source
            .get_resource_shallow(&subject)
            .map_err(|_| "A selected recovery resource changed during export")?;
        let current_commit = current
            .get(urls::LAST_COMMIT)
            .map_err(|_| "A selected recovery resource lost its revision during export")?;
        if canonical_id(source, &current_commit.to_string())
            != canonical_id(source, &expected.last_commit)
        {
            return Err("A selected recovery resource changed during export".into());
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
