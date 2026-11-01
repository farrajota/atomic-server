use super::*;
use crate::{
    agents::{Agent, ForAgent},
    db::trees::Tree,
    Storelike,
};

struct Fixture {
    store: Db,
    owner: Agent,
    drive: String,
    subject: String,
    source_text: String,
}

async fn fixture(name: &str, retention: EnvelopeRetention) -> Fixture {
    let id = format!("native_recovery_{name}_{}", crate::utils::random_string(10));
    let store = Db::init_temp(&id).await.unwrap();
    store.set_envelope_retention(retention);
    let owner = Agent::new(Some("Recovery owner")).unwrap();
    store.set_default_agent(owner.clone());
    let drive = store.ensure_private_drive().await.unwrap();
    store.set_active_drive(&drive).unwrap();
    let source_text = "Full source text: the original bytes remain available offline.".to_string();
    let subject = store
        .create_resource(
            urls::CLASS,
            &drive,
            "Source",
            Some(vec![
                (urls::DESCRIPTION, Value::Markdown(source_text.clone())),
                (urls::SHORTNAME, Value::Slug("source".into())),
            ]),
        )
        .await
        .unwrap();
    for name in ["first edit", "second edit"] {
        let mut resource = store
            .get_resource(&Subject::from(subject.as_str()))
            .await
            .unwrap();
        resource
            .set(urls::NAME.into(), Value::String(name.into()), &store)
            .await
            .unwrap();
        resource.save_locally(&store).await.unwrap();
    }
    Fixture {
        store,
        owner,
        drive,
        subject,
        source_text,
    }
}

fn revision(store: &Db, subject: &str) -> String {
    let subject = Subject::from_raw(subject, store.get_base_domain().as_deref());
    store
        .get_resource_shallow(&subject)
        .unwrap()
        .get(urls::LAST_COMMIT)
        .unwrap()
        .to_string()
}

fn request_for(store: &Db, drive: &str, subject: &str) -> RecoveryRequest {
    RecoveryRequest {
        version: 1,
        resources: vec![
            RecoverySelection {
                subject: drive.to_string(),
                expected_last_commit: revision(store, drive),
            },
            RecoverySelection {
                subject: subject.to_string(),
                expected_last_commit: revision(store, subject),
            },
        ],
        allowed_drives: vec![drive.to_string()],
    }
}

fn decode_for_owner(bytes: &[u8], owner: &Agent) -> AtomicResult<RecoveryBundle> {
    decode_bundle(bytes, owner.subject.as_str())
}

fn reseal_as_source(bundle: &mut RecoveryBundle, source: &Agent) -> AtomicResult<Vec<u8>> {
    let payload = encode_native_payload(&bundle.resources)?;
    bundle.manifest.payload_bytes = payload.len();
    bundle.manifest.payload_sha256 = sha256(&payload);
    sign_source_manifest(&mut bundle.manifest, source)?;
    seal_bundle(&bundle.manifest, &payload)
}

fn reseal_transport_only(bundle: &mut RecoveryBundle) -> AtomicResult<Vec<u8>> {
    let payload = encode_native_payload(&bundle.resources)?;
    bundle.manifest.payload_bytes = payload.len();
    bundle.manifest.payload_sha256 = sha256(&payload);
    seal_bundle(&bundle.manifest, &payload)
}

fn reseal_modified_envelope(
    bundle: &mut RecoveryBundle,
    resource_index: usize,
    envelope_index: usize,
    source: &Agent,
) -> AtomicResult<Vec<u8>> {
    let envelope = &mut bundle.resources[resource_index].envelopes[envelope_index];
    let old_signature = envelope.signature.clone();
    let json: serde_json::Value = serde_json::from_str(&envelope.json)?;
    let signature = json
        .get(urls::SIGNATURE)
        .and_then(serde_json::Value::as_str)
        .ok_or("Mutated recovery envelope has no signature")?
        .to_string();
    let created_at = json
        .get(urls::CREATED_AT)
        .and_then(serde_json::Value::as_i64)
        .ok_or("Mutated recovery envelope has no timestamp")?;
    envelope.signature = signature.clone();
    envelope.created_at = created_at;

    let manifest = &mut bundle.manifest.resources[resource_index];
    let metadata = &mut manifest.envelopes[envelope_index];
    metadata.signature = signature.clone();
    metadata.created_at = created_at;
    metadata.length = envelope.json.len();
    metadata.sha256 = sha256(envelope.json.as_bytes());
    let attribution = manifest
        .history
        .attributions
        .iter_mut()
        .find(|attribution| attribution.signature == old_signature)
        .ok_or("Mutated recovery envelope has no attribution")?;
    attribution.signature = signature;
    reseal_as_source(bundle, source)
}

fn owner_actor(owner: &Agent) -> ForAgent {
    ForAgent::AgentSubject(owner.subject.clone())
}

fn assert_change_attribution_matches(
    source: &crate::envelopes::HistoryAttribution,
    restored: &crate::envelopes::HistoryAttribution,
) {
    assert_eq!(source.changes.len(), restored.changes.len());
    for (source, restored) in source.changes.iter().zip(&restored.changes) {
        assert_eq!(source.peer, restored.peer);
        assert_eq!(source.counter, restored.counter);
        assert_eq!(source.length, restored.length);
        assert_eq!(source.origin, restored.origin);
        assert_eq!(source.attribution, restored.attribution);
        assert_eq!(source.signer, restored.signer);
    }
}

fn rights_validating_opts() -> crate::commit::CommitOpts {
    crate::commit::CommitOpts {
        validate_schema: true,
        validate_constraints: true,
        validate_signature: true,
        validate_timestamp: false,
        validate_rights: true,
        validate_loro_causality: true,
        update_index: true,
        validate_for_agent: None,
        source_id: None,
    }
}

async fn signed_drive(store: &Db, owner: &Agent, name: &str) -> String {
    let mut builder = crate::commit::CommitBuilder::new("placeholder".into());
    builder.set(
        urls::IS_A.into(),
        Value::ResourceArray(vec![urls::DRIVE.into()]),
    );
    builder.set(urls::NAME.into(), Value::String(name.into()));
    for property in [urls::READ, urls::WRITE] {
        builder.set(
            property.into(),
            Value::ResourceArray(vec![owner.subject.as_str().into()]),
        );
    }
    let commit = crate::commit::Commit::create_did(builder, owner, store)
        .await
        .unwrap();
    let subject = commit.subject.to_string();
    store
        .apply_commit(commit, &rights_validating_opts())
        .await
        .unwrap();
    subject
}

#[tokio::test]
async fn restores_original_snapshot_history_and_full_source_for_fresh_reader() {
    let fixture = fixture("roundtrip", EnvelopeRetention::All).await;
    let request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    let original_snapshot = fixture
        .store
        .kv
        .get(
            Tree::LoroSnapshots,
            canonical_id(&fixture.store, &fixture.subject).as_bytes(),
        )
        .unwrap()
        .unwrap();
    let doc = strict_snapshot_doc(&original_snapshot).unwrap();
    assert!(!doc.get_all_datatypes().contains_key(urls::LAST_COMMIT));
    assert!(!doc.get_all_properties().contains_key(urls::LAST_COMMIT));
    let source_history = crate::envelopes::attribute_history(&fixture.store, &fixture.subject)
        .await
        .unwrap();
    let bytes = export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    let bundle = decode_for_owner(&bytes, &fixture.owner).unwrap();
    assert_eq!(bundle.manifest.version, 2);
    assert_eq!(
        bundle.manifest.source_agent,
        canonical_source_agent(&fixture.owner.subject.to_string()).unwrap()
    );
    let source_entry = bundle
        .manifest
        .resources
        .iter()
        .find(|entry| entry.subject == canonical_id(&fixture.store, &fixture.subject))
        .unwrap();
    assert_eq!(
        source_entry.last_commit,
        revision(&fixture.store, &fixture.subject)
    );
    assert_eq!(
        source_entry.history.attributions.len(),
        source_history.attributions.len()
    );
    for (wire, native) in source_entry
        .history
        .attributions
        .iter()
        .zip(&source_history.attributions)
    {
        assert_eq!(wire.spans, native.spans.clone().unwrap());
        assert_eq!(wire.server_spans, native.server_spans.clone().unwrap());
    }
    assert_eq!(bundle.manifest.resources.len(), 2);
    assert!(bundle
        .manifest
        .resources
        .iter()
        .any(|entry| entry.subject == canonical_id(&fixture.store, &fixture.subject)));

    let target = Db::init_redb(Some("https://localhost".into()))
        .await
        .unwrap();
    restore_bundle(&target, &bundle).await.unwrap();

    let source_subject = Subject::from_raw(&fixture.subject, target.get_base_domain().as_deref());
    let restored = target.get_resource(&source_subject).await.unwrap();
    let description = match restored.get(urls::DESCRIPTION).unwrap() {
        Value::Markdown(text) | Value::String(text) => text,
        value => panic!("unexpected restored Source description: {value:?}"),
    };
    assert_eq!(description.as_bytes(), fixture.source_text.as_bytes());
    assert_eq!(
        sha256(description.as_bytes()),
        sha256(fixture.source_text.as_bytes())
    );
    assert!(
        crate::hierarchy::check_read(&target, &restored, &owner_actor(&fixture.owner))
            .await
            .is_ok()
    );

    let canonical_subject = canonical_id(&target, &fixture.subject);
    assert_eq!(
        target
            .kv
            .get(Tree::LoroSnapshots, canonical_subject.as_bytes())
            .unwrap()
            .unwrap(),
        original_snapshot
    );
    assert_eq!(
        crate::envelopes::envelopes(&fixture.store, &fixture.subject),
        crate::envelopes::envelopes(&target, &fixture.subject)
    );
    let history = crate::envelopes::attribute_history(&target, &fixture.subject)
        .await
        .unwrap();
    assert!(history.complete);
    assert_eq!(
        revision(&target, &fixture.subject),
        source_entry.last_commit
    );
    assert_eq!(
        history.attributions.len(),
        source_entry.history.attributions.len()
    );
    for (wire, native) in source_entry
        .history
        .attributions
        .iter()
        .zip(&history.attributions)
    {
        assert_eq!(wire.spans, native.spans.clone().unwrap());
        assert_eq!(wire.server_spans, native.server_spans.clone().unwrap());
    }
    assert!(history
        .attributions
        .iter()
        .all(|attribution| attribution.verified));
    assert!(history
        .changes
        .iter()
        .any(|change| change.origin == crate::envelopes::ChangeOrigin::Signed));
    assert_change_attribution_matches(&source_history, &history);
    assert!(history
        .attributions
        .iter()
        .any(|attribution| attribution.signer == fixture.owner.subject));

    let outsider = Agent::new(Some("Recovery outsider")).unwrap();
    assert!(crate::hierarchy::check_read(
        &target,
        &restored,
        &ForAgent::AgentSubject(outsider.subject)
    )
    .await
    .is_err());
}

#[tokio::test]
async fn restores_receipt_free_cumulative_updates_with_optional_predecessors() {
    let fixture = fixture("signed_deltas", EnvelopeRetention::All).await;
    let mut original_signatures = Vec::new();
    for token in ["first-native-delta", "second-native-delta"] {
        let subject = Subject::from(fixture.subject.as_str());
        let resource = fixture.store.get_resource(&subject).await.unwrap();
        let snapshot = fixture
            .store
            .kv
            .get(
                Tree::LoroSnapshots,
                canonical_id(&fixture.store, &fixture.subject).as_bytes(),
            )
            .unwrap()
            .unwrap();
        let author = crate::loro::AtomicLoroDoc::from_snapshot(&snapshot).unwrap();
        let before = author.oplog_vv();
        author
            .set_property(urls::NAME, &Value::String(token.into()))
            .unwrap();
        author.commit_with_message(token);
        let mut builder = crate::commit::CommitBuilder::new(resource.get_subject().clone());
        builder.set_loro_update(author.export_updates_since(&before));
        let commit = builder
            .sign(&fixture.owner, &fixture.store, &resource)
            .await
            .unwrap();
        original_signatures.push((token.to_string(), commit.signature.clone().unwrap()));
        fixture
            .store
            .apply_commit(commit, &rights_validating_opts())
            .await
            .unwrap();
    }
    let resource = fixture
        .store
        .get_resource(&Subject::from(fixture.subject.as_str()))
        .await
        .unwrap();
    let mut builder = crate::commit::CommitBuilder::new(resource.get_subject().clone());
    builder.set(
        urls::NAME.into(),
        Value::String("later full snapshot".into()),
    );
    let mut later = builder
        .sign(&fixture.owner, &fixture.store, &resource)
        .await
        .unwrap();
    later.previous_commit = None;
    later.created_at = 1;
    let json = later
        .serialize_deterministically_json_ad(&fixture.store)
        .await
        .unwrap();
    later.signature = Some(
        crate::commit::sign_message(
            &json,
            fixture.owner.private_key.as_deref().unwrap(),
            &fixture.owner.public_key,
        )
        .unwrap(),
    );
    let latest_signature = later.signature.clone().unwrap();
    fixture
        .store
        .apply_commit(later, &rights_validating_opts())
        .await
        .unwrap();
    let source_history = crate::envelopes::attribute_history(&fixture.store, &fixture.subject)
        .await
        .unwrap();
    let request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    let bytes = export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    let bundle = decode_for_owner(&bytes, &fixture.owner).unwrap();
    let entry = bundle
        .manifest
        .resources
        .iter()
        .find(|entry| entry.subject == canonical_id(&fixture.store, &fixture.subject))
        .unwrap();
    assert_eq!(entry.envelopes[0].created_at, 1);
    assert_eq!(
        entry.last_commit,
        revision(&fixture.store, &fixture.subject)
    );
    assert_eq!(
        entry.last_commit,
        crate::identifiers::commit_subject(&latest_signature)
    );
    let resource = bundle
        .resources
        .iter()
        .find(|resource| resource.subject == entry.subject)
        .unwrap();
    let snapshot_doc = strict_snapshot_doc(&resource.snapshot).unwrap();
    assert!(!snapshot_doc
        .get_all_properties()
        .contains_key(urls::LAST_COMMIT));
    let latest_envelope = resource
        .envelopes
        .iter()
        .find(|envelope| envelope.signature == latest_signature)
        .unwrap();
    let latest_json: serde_json::Value = serde_json::from_str(&latest_envelope.json).unwrap();
    assert!(latest_json
        .get(urls::PREVIOUS_COMMIT)
        .is_none_or(serde_json::Value::is_null));
    for (token, signature) in &original_signatures {
        let attribution = entry
            .history
            .attributions
            .iter()
            .find(|attribution| attribution.tokens.contains(token))
            .unwrap();
        assert_eq!(&attribution.signature, signature);
        assert_eq!(attribution.signer, fixture.owner.subject.to_string());
    }
    let target = Db::init_redb(Some("https://localhost".into()))
        .await
        .unwrap();
    restore_bundle(&target, &bundle).await.unwrap();
    let history = crate::envelopes::attribute_history(&target, &fixture.subject)
        .await
        .unwrap();
    assert!(history.complete);
    assert_eq!(revision(&target, &fixture.subject), entry.last_commit);
    assert_eq!(history.attributions.len(), entry.history.attributions.len());
    for (wire, native) in entry.history.attributions.iter().zip(&history.attributions) {
        assert_eq!(wire.spans, native.spans.clone().unwrap());
        assert_eq!(wire.server_spans, native.server_spans.clone().unwrap());
    }
    for (token, signature) in &original_signatures {
        let change = history
            .changes
            .iter()
            .find(|change| change.message.as_deref() == Some(token.as_str()))
            .unwrap();
        assert_eq!(change.origin, crate::envelopes::ChangeOrigin::Signed);
        assert_eq!(
            change.signer.as_deref(),
            Some(fixture.owner.subject.as_str())
        );
        assert_eq!(
            &history.attributions[change.attribution.unwrap()].signature,
            signature
        );
    }
    assert_change_attribution_matches(&source_history, &history);
    let restored = target
        .get_resource(&Subject::from(fixture.subject.as_str()))
        .await
        .unwrap();
    assert_eq!(
        restored.get(urls::NAME).unwrap().to_string(),
        "later full snapshot"
    );
    assert_eq!(
        crate::envelopes::envelopes(&target, &fixture.subject),
        crate::envelopes::envelopes(&fixture.store, &fixture.subject)
    );
}

#[tokio::test]
async fn ordinary_rights_validating_genesis_and_reparent_preserve_exact_bookkeeping() {
    let fixture = fixture("ordinary_rights", EnvelopeRetention::All).await;
    let opts = rights_validating_opts();
    let second_drive = signed_drive(&fixture.store, &fixture.owner, "Second signed root").await;

    for legacy in [false, true] {
        let mut builder = crate::commit::CommitBuilder::new("did:ad:placeholder".into());
        builder.set(
            urls::IS_A.into(),
            Value::ResourceArray(vec![urls::CLASS.into()]),
        );
        builder.set(
            urls::NAME.into(),
            Value::String("Ordinary rights-validating Source".into()),
        );
        builder.set(
            urls::DESCRIPTION.into(),
            Value::Markdown(fixture.source_text.clone()),
        );
        builder.set(
            urls::SHORTNAME.into(),
            Value::Slug("ordinary-source".into()),
        );
        builder.set(
            urls::PARENT.into(),
            Value::AtomicUrl(fixture.drive.as_str().into()),
        );
        let commit = if legacy {
            builder.is_genesis = true;
            let mut commit = builder
                .sign(
                    &fixture.owner,
                    &fixture.store,
                    &Resource::new("did:ad:placeholder".into()),
                )
                .await
                .unwrap();
            commit.subject =
                crate::identifiers::resource_subject(commit.signature.as_deref().unwrap()).into();
            commit
        } else {
            crate::commit::Commit::create_did(builder, &fixture.owner, &fixture.store)
                .await
                .unwrap()
        };
        let subject = commit.subject.to_string();
        let author_doc = strict_snapshot_doc(commit.loro_update.as_deref().unwrap()).unwrap();
        assert!(!author_doc.get_all_properties().contains_key(urls::WRITE));
        if legacy {
            assert!(!author_doc
                .get_all_properties()
                .contains_key(urls::DRIVE_PROP));
        }
        commit
            .validate_signature(&LocalRecoveryStore::new(&fixture.store))
            .await
            .unwrap();
        fixture.store.apply_commit(commit, &opts).await.unwrap();

        for moved in [false, true] {
            if moved {
                let resource = fixture
                    .store
                    .get_resource(&Subject::from(subject.as_str()))
                    .await
                    .unwrap();
                let mut update = crate::commit::CommitBuilder::new(resource.get_subject().clone());
                update.set(
                    urls::PARENT.into(),
                    Value::AtomicUrl(second_drive.as_str().into()),
                );
                let commit = update
                    .sign(&fixture.owner, &fixture.store, &resource)
                    .await
                    .unwrap();
                fixture.store.apply_commit(commit, &opts).await.unwrap();
            }
            let resource = fixture
                .store
                .get_resource(&Subject::from(subject.as_str()))
                .await
                .unwrap();
            let source_history = crate::envelopes::attribute_history(&fixture.store, &subject)
                .await
                .unwrap();
            assert!(source_history
                .changes
                .iter()
                .any(|change| change.origin == crate::envelopes::ChangeOrigin::Server));
            assert_eq!(
                resource
                    .get(urls::WRITE)
                    .unwrap()
                    .to_subjects(None)
                    .unwrap(),
                vec![fixture.owner.subject.to_string()]
            );
            let expected_drive = if moved { &second_drive } else { &fixture.drive };
            assert_eq!(
                resource.get(urls::DRIVE_PROP).unwrap().to_string(),
                expected_drive.as_str()
            );
            let mut request = request_for(&fixture.store, &fixture.drive, &subject);
            if moved {
                request.resources.push(RecoverySelection {
                    subject: second_drive.clone(),
                    expected_last_commit: revision(&fixture.store, &second_drive),
                });
                request.allowed_drives.push(second_drive.clone());
            }
            let bytes = export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
                .await
                .unwrap();
            let bundle = decode_for_owner(&bytes, &fixture.owner).unwrap();
            let target = Db::init_redb(Some("https://localhost".into()))
                .await
                .unwrap();
            restore_bundle(&target, &bundle).await.unwrap();
            let restored = target
                .get_resource(&Subject::from(subject.as_str()))
                .await
                .unwrap();
            let restored_history = crate::envelopes::attribute_history(&target, &subject)
                .await
                .unwrap();
            assert!(restored_history.complete);
            assert!(restored_history
                .changes
                .iter()
                .any(|change| change.origin == crate::envelopes::ChangeOrigin::Server));
            assert_change_attribution_matches(&source_history, &restored_history);
            assert_eq!(
                restored
                    .get(urls::DESCRIPTION)
                    .unwrap()
                    .to_string()
                    .as_bytes(),
                fixture.source_text.as_bytes()
            );
            assert_eq!(
                restored
                    .get(urls::WRITE)
                    .unwrap()
                    .to_subjects(None)
                    .unwrap(),
                vec![fixture.owner.subject.to_string()]
            );
            assert_eq!(
                restored.get(urls::DRIVE_PROP).unwrap().to_string(),
                resource.get(urls::DRIVE_PROP).unwrap().to_string()
            );
            assert_eq!(
                restored.get(urls::LAST_COMMIT).unwrap().to_string(),
                resource.get(urls::LAST_COMMIT).unwrap().to_string()
            );
            assert_eq!(
                crate::envelopes::envelopes(&target, &subject),
                crate::envelopes::envelopes(&fixture.store, &subject)
            );
            let key = canonical_id(&fixture.store, &subject);
            assert_eq!(
                target.kv.get(Tree::LoroSnapshots, key.as_bytes()).unwrap(),
                fixture
                    .store
                    .kv
                    .get(Tree::LoroSnapshots, key.as_bytes())
                    .unwrap()
            );
        }
    }
}

#[tokio::test]
async fn native_drive_bookkeeping_uses_original_parent_receipts_after_parent_moves() {
    let fixture = fixture("historical_parent", EnvelopeRetention::All).await;
    let second_drive = signed_drive(&fixture.store, &fixture.owner, "Later parent home").await;
    let mut builder = crate::commit::CommitBuilder::new("did:ad:placeholder".into());
    builder.is_genesis = true;
    builder.set(
        urls::IS_A.into(),
        Value::ResourceArray(vec![urls::CLASS.into()]),
    );
    builder.set(
        urls::NAME.into(),
        Value::String("Legacy child with derived drive".into()),
    );
    builder.set(
        urls::SHORTNAME.into(),
        Value::Slug("historical-child".into()),
    );
    builder.set(
        urls::DESCRIPTION.into(),
        Value::Markdown(fixture.source_text.clone()),
    );
    builder.set(
        urls::PARENT.into(),
        Value::AtomicUrl(fixture.subject.as_str().into()),
    );
    let mut commit = builder
        .sign(
            &fixture.owner,
            &fixture.store,
            &Resource::new("did:ad:placeholder".into()),
        )
        .await
        .unwrap();
    commit.subject =
        crate::identifiers::resource_subject(commit.signature.as_deref().unwrap()).into();
    let child = commit.subject.to_string();
    fixture
        .store
        .apply_commit(commit, &rights_validating_opts())
        .await
        .unwrap();

    let parent = fixture
        .store
        .get_resource(&Subject::from(fixture.subject.as_str()))
        .await
        .unwrap();
    let mut builder = crate::commit::CommitBuilder::new(parent.get_subject().clone());
    builder.set(
        urls::PARENT.into(),
        Value::AtomicUrl(second_drive.as_str().into()),
    );
    let commit = builder
        .sign(&fixture.owner, &fixture.store, &parent)
        .await
        .unwrap();
    fixture
        .store
        .apply_commit(commit, &rights_validating_opts())
        .await
        .unwrap();
    assert_eq!(
        fixture
            .store
            .get_resource(&Subject::from(fixture.subject.as_str()))
            .await
            .unwrap()
            .get(urls::DRIVE_PROP)
            .unwrap()
            .to_string(),
        second_drive
    );
    assert_eq!(
        fixture
            .store
            .get_resource(&Subject::from(child.as_str()))
            .await
            .unwrap()
            .get(urls::DRIVE_PROP)
            .unwrap()
            .to_string(),
        fixture.drive
    );

    let mut request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    for subject in [&child, &second_drive] {
        request.resources.push(RecoverySelection {
            subject: subject.clone(),
            expected_last_commit: revision(&fixture.store, subject),
        });
    }
    request.allowed_drives.push(second_drive);
    let bytes = export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    let target = Db::init_redb(Some("https://localhost".into()))
        .await
        .unwrap();
    restore_bundle(&target, &decode_for_owner(&bytes, &fixture.owner).unwrap())
        .await
        .unwrap();
    let restored = target
        .get_resource(&Subject::from(child.as_str()))
        .await
        .unwrap();
    assert_eq!(
        restored.get(urls::DRIVE_PROP).unwrap().to_string(),
        fixture.drive
    );
    assert_eq!(
        restored
            .get(urls::DESCRIPTION)
            .unwrap()
            .to_string()
            .as_bytes(),
        fixture.source_text.as_bytes()
    );
    assert_eq!(
        crate::envelopes::envelopes(&target, &child),
        crate::envelopes::envelopes(&fixture.store, &child)
    );
}

#[tokio::test]
async fn refuses_unauthorized_export_and_wrong_revision() {
    let fixture = fixture("auth_revision", EnvelopeRetention::All).await;
    let mut request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    let outsider = Agent::new(Some("Recovery outsider")).unwrap();
    let error = export_bundle(
        &fixture.store,
        &request,
        &ForAgent::AgentSubject(outsider.subject.clone()),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("not authorized for every selected resource"),
        "{error}"
    );
    for stale in [false, true] {
        let mut unreadable = request.clone();
        unreadable.resources.remove(0);
        if stale {
            unreadable.resources[0].expected_last_commit = "atomic:commit:unknown".into();
        }
        let error = export_bundle(
            &fixture.store,
            &unreadable,
            &ForAgent::AgentSubject(outsider.subject.clone()),
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("not authorized for every selected resource"),
            "{error}"
        );
        assert!(!error.to_string().contains(&fixture.drive), "{error}");
    }

    request.resources[1].expected_last_commit = "atomic:commit:not-the-current-revision".into();
    let error = export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Recovery revision mismatch"));
}

#[tokio::test]
async fn export_source_proof_uses_the_store_default_agent_and_fails_without_its_key() {
    let fixture = fixture("source_signer", EnvelopeRetention::All).await;
    let requester = owner_actor(&fixture.owner);
    let source_signer = Agent::new(Some("Recovery source signer")).unwrap();
    fixture.store.set_default_agent(source_signer.clone());
    let request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    let bytes = export_bundle(&fixture.store, &request, &requester)
        .await
        .unwrap();
    let bundle = decode_for_owner(&bytes, &source_signer).unwrap();
    assert_eq!(
        bundle.manifest.source_agent,
        canonical_source_agent(&source_signer.subject.to_string()).unwrap()
    );
    assert!(decode_for_owner(&bytes, &fixture.owner).is_err());

    let mut missing_key = source_signer;
    missing_key.private_key = None;
    fixture.store.set_default_agent(missing_key);
    let error = export_bundle(&fixture.store, &request, &requester)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("private signing key"), "{error}");
}

#[tokio::test]
async fn source_proof_requires_independent_pin_and_binds_manifest_inventory_spans_revision_and_payload(
) {
    let fixture = fixture("source_proof", EnvelopeRetention::All).await;
    let request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    let bytes = export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    let original = decode_for_owner(&bytes, &fixture.owner).unwrap();
    let attacker = Agent::new(Some("Recovery attacker")).unwrap();

    assert!(decode_bundle(&bytes, "").is_err());
    assert!(decode_bundle(&bytes, "not-a-native-agent").is_err());
    assert!(decode_bundle(&bytes, "atomic:agent:not-base64").is_err());
    assert!(decode_bundle(&bytes, attacker.subject.as_str()).is_err());
    let mut old_format = bytes.clone();
    old_format[..8].copy_from_slice(b"ATRECOV1");
    assert!(decode_bundle(&old_format, fixture.owner.subject.as_str()).is_err());

    for mutation in ["scope", "inventory", "spans", "revision"] {
        let mut altered = original.clone();
        match mutation {
            "scope" => altered.manifest.allowed_drives[0] = "atomic:drive:changed".into(),
            "inventory" => altered.manifest.resources[0].envelopes[0].sha256 = "f".repeat(64),
            "spans" => altered.manifest.resources[0].history.attributions[0]
                .spans
                .push(crate::loro::ChangeSpan {
                    peer: 0,
                    counter: 0,
                    length: 1,
                }),
            "revision" => {
                altered.manifest.resources[0].last_commit =
                    "atomic:commit:source-proof-substitution".into()
            }
            _ => unreachable!(),
        }
        let altered_bytes = reseal_transport_only(&mut altered).unwrap();
        assert!(
            decode_bundle(&altered_bytes, fixture.owner.subject.as_str()).is_err(),
            "source proof accepted a changed {mutation}"
        );
    }

    let mut changed_payload = original.clone();
    changed_payload.resources[0].snapshot[0] ^= 1;
    let changed_payload_bytes = reseal_transport_only(&mut changed_payload).unwrap();
    assert!(decode_bundle(&changed_payload_bytes, fixture.owner.subject.as_str()).is_err());

    let mut attacker_signed = original.clone();
    let proof = source_proof_message(&attacker_signed.manifest).unwrap();
    attacker_signed.manifest.source_signature =
        crate::agents::sign_message(&proof, attacker.private_key.as_deref().unwrap()).unwrap();
    let attacker_signed_bytes = seal_bundle(
        &attacker_signed.manifest,
        &encode_native_payload(&attacker_signed.resources).unwrap(),
    )
    .unwrap();
    assert!(decode_bundle(&attacker_signed_bytes, fixture.owner.subject.as_str()).is_err());

    let mut substituted_identity = original;
    sign_source_manifest(&mut substituted_identity.manifest, &attacker).unwrap();
    let substituted_bytes = seal_bundle(
        &substituted_identity.manifest,
        &encode_native_payload(&substituted_identity.resources).unwrap(),
    )
    .unwrap();
    assert!(decode_bundle(&substituted_bytes, fixture.owner.subject.as_str()).is_err());
}

#[tokio::test]
async fn refuses_duplicate_canonical_membership_and_missing_drive_dependency() {
    let fixture = fixture("membership", EnvelopeRetention::All).await;
    let mut duplicate = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    export_bundle(&fixture.store, &duplicate, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    let canonical = canonical_id(&fixture.store, &fixture.subject);
    duplicate.resources.push(RecoverySelection {
        subject: canonical,
        expected_last_commit: revision(&fixture.store, &fixture.subject),
    });
    let error = export_bundle(&fixture.store, &duplicate, &owner_actor(&fixture.owner))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("duplicate canonical resources"));

    let mut missing = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    missing.resources.remove(0);
    let error = export_bundle(&fixture.store, &missing, &owner_actor(&fixture.owner))
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("Recovery dependency is not explicit"));
}

#[tokio::test]
async fn refuses_export_outside_declared_drive() {
    let fixture = fixture("wrong_drive", EnvelopeRetention::All).await;
    let other_drive = signed_drive(&fixture.store, &fixture.owner, "Other drive").await;
    let mut request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    request.resources.push(RecoverySelection {
        subject: other_drive.clone(),
        expected_last_commit: revision(&fixture.store, &other_drive),
    });
    request.allowed_drives = vec![other_drive];
    let error = export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("outside the declared drives"));
}

#[tokio::test]
async fn refuses_selected_file_payloads_and_latest_only_history() {
    let file_fixture = fixture("file", EnvelopeRetention::All).await;
    let positive = request_for(
        &file_fixture.store,
        &file_fixture.drive,
        &file_fixture.subject,
    );
    export_bundle(
        &file_fixture.store,
        &positive,
        &owner_actor(&file_fixture.owner),
    )
    .await
    .unwrap();
    let file = file_fixture
        .store
        .create_resource(
            urls::FILE,
            &file_fixture.drive,
            "Attachment",
            Some(vec![(
                urls::DESCRIPTION,
                Value::Markdown("not a Source".into()),
            )]),
        )
        .await
        .unwrap();
    let request = request_for(&file_fixture.store, &file_fixture.drive, &file);
    let error = export_bundle(
        &file_fixture.store,
        &request,
        &owner_actor(&file_fixture.owner),
    )
    .await
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("does not support selected file/blob"));

    let latest = fixture("latest", EnvelopeRetention::Latest).await;
    let request = request_for(&latest.store, &latest.drive, &latest.subject);
    let error = export_bundle(&latest.store, &request, &owner_actor(&latest.owner))
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("requires `all` envelope retention"));
}

#[tokio::test]
async fn detects_a_revision_change_after_the_immutable_view_opens() {
    let fixture = fixture("race", EnvelopeRetention::All).await;
    let request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    let view = fixture.store.kv.read_view().unwrap();
    let mut resource = fixture
        .store
        .get_resource(&Subject::from(fixture.subject.as_str()))
        .await
        .unwrap();
    resource
        .set(
            urls::NAME.into(),
            Value::String("raced revision".into()),
            &fixture.store,
        )
        .await
        .unwrap();
    resource.save_locally(&fixture.store).await.unwrap();

    let error =
        export_bundle_from_view(&fixture.store, view, &request, &owner_actor(&fixture.owner))
            .await
            .unwrap_err();
    assert!(error.to_string().contains("changed during export"));
}

#[tokio::test]
async fn detects_ancestor_read_revocation_after_view_opens() {
    let fixture = fixture("ancestor_revocation", EnvelopeRetention::All).await;
    let reader = Agent::new(Some("Recovery reader without server root rights")).unwrap();
    let mut drive = fixture
        .store
        .get_resource(&Subject::from(fixture.drive.as_str()))
        .await
        .unwrap();
    drive
        .set(
            urls::READ.into(),
            Value::ResourceArray(vec![reader.subject.as_str().into()]),
            &fixture.store,
        )
        .await
        .unwrap();
    drive.save_locally(&fixture.store).await.unwrap();
    let request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    export_bundle(&fixture.store, &request, &owner_actor(&reader))
        .await
        .unwrap();
    let view = fixture.store.kv.read_view().unwrap();
    drive
        .set(
            urls::READ.into(),
            Value::ResourceArray(vec![]),
            &fixture.store,
        )
        .await
        .unwrap();
    drive.save_locally(&fixture.store).await.unwrap();
    let error = export_bundle_from_view(&fixture.store, view, &request, &owner_actor(&reader))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("changed during export"));
}

#[tokio::test]
async fn detects_reparent_after_view_opens() {
    let fixture = fixture("reparent_race", EnvelopeRetention::All).await;
    let other = signed_drive(&fixture.store, &fixture.owner, "Other parent").await;
    let request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    let view = fixture.store.kv.read_view().unwrap();
    let mut resource = fixture
        .store
        .get_resource(&Subject::from(fixture.subject.as_str()))
        .await
        .unwrap();
    resource
        .set(
            urls::PARENT.into(),
            Value::AtomicUrl(other.into()),
            &fixture.store,
        )
        .await
        .unwrap();
    resource.save_locally(&fixture.store).await.unwrap();
    let error =
        export_bundle_from_view(&fixture.store, view, &request, &owner_actor(&fixture.owner))
            .await
            .unwrap_err();
    assert!(error.to_string().contains("changed during export"));
}

#[tokio::test]
async fn rejects_tampered_and_truncated_native_payloads() {
    let fixture = fixture("tamper", EnvelopeRetention::All).await;
    let request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    let bytes = export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    assert!(decode_bundle(&bytes[..bytes.len() - 1], fixture.owner.subject.as_str()).is_err());
    let mut tampered_payload = bytes.clone();
    *tampered_payload.last_mut().unwrap() ^= 1;
    assert!(decode_bundle(&tampered_payload, fixture.owner.subject.as_str()).is_err());
    let mut tampered_manifest = bytes;
    tampered_manifest[BUNDLE_HEADER_BYTES - 1] ^= 1;
    assert!(decode_bundle(&tampered_manifest, fixture.owner.subject.as_str()).is_err());
}

#[tokio::test]
async fn rejects_resealed_snapshot_with_unmaterialized_causal_operations() {
    let fixture = fixture("pending_snapshot", EnvelopeRetention::All).await;
    let request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    let bytes = export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    let mut bundle = decode_for_owner(&bytes, &fixture.owner).unwrap();
    let author = crate::loro::AtomicLoroDoc::new();
    author
        .set_property(urls::NAME, &Value::String("missing predecessor".into()))
        .unwrap();
    author.commit_with_message("predecessor");
    let before = author.oplog_vv();
    author
        .set_property(urls::NAME, &Value::String("pending content".into()))
        .unwrap();
    author.commit_with_message("pending");
    let delta = author.export_updates_since(&before);
    let index = bundle
        .resources
        .iter()
        .position(|resource| resource.subject == canonical_id(&fixture.store, &fixture.subject))
        .unwrap();
    bundle.manifest.resources[index].snapshot_length = delta.len();
    bundle.manifest.resources[index].snapshot_sha256 = sha256(&delta);
    bundle.manifest.resources[index].version_vector = version_vector(&delta).unwrap();
    bundle.resources[index].snapshot = delta;
    let payload = encode_native_payload(&bundle.resources).unwrap();
    bundle.manifest.payload_bytes = payload.len();
    bundle.manifest.payload_sha256 = sha256(&payload);
    let malformed_bytes = reseal_as_source(&mut bundle, &fixture.owner).unwrap();
    let error = decode_for_owner(&malformed_bytes, &fixture.owner).unwrap_err();
    assert!(
        error.to_string().contains("pending causal operations"),
        "{error}"
    );
}

#[tokio::test]
async fn refuses_resealed_unsigned_content_and_container_operations() {
    let fixture = fixture("unsigned_content", EnvelopeRetention::All).await;
    let request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    let bytes = export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    let original = decode_for_owner(&bytes, &fixture.owner).unwrap();
    for container_mutation in [false, true] {
        let mut bundle = original.clone();
        let index = bundle
            .resources
            .iter()
            .position(|resource| resource.subject == canonical_id(&fixture.store, &fixture.subject))
            .unwrap();
        let doc =
            crate::loro::AtomicLoroDoc::from_snapshot(&bundle.resources[index].snapshot).unwrap();
        if container_mutation {
            doc.doc()
                .get_map("unsigned-hidden-container")
                .insert("payload", "forged")
                .unwrap();
        } else {
            doc.set_property(
                urls::DESCRIPTION,
                &Value::Markdown("forged Source text".into()),
            )
            .unwrap();
        }
        let snapshot = doc.export_snapshot();
        let resource = resource_from_snapshot(&bundle.resources[index].subject, &snapshot).unwrap();
        let manifest = &mut bundle.manifest.resources[index];
        manifest.snapshot_length = snapshot.len();
        manifest.snapshot_sha256 = sha256(&snapshot);
        manifest.version_vector = version_vector(&snapshot).unwrap();
        manifest.description_sha256 = description_hash(&resource);
        bundle.resources[index].snapshot = snapshot;
        let malformed_bytes = reseal_as_source(&mut bundle, &fixture.owner).unwrap();
        let forged = decode_for_owner(&malformed_bytes, &fixture.owner).unwrap();
        let target = Db::init_redb(Some("https://localhost".into()))
            .await
            .unwrap();
        let error = restore_bundle(&target, &forged).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Recovery snapshot operation is not covered by original spans"),
            "{error}"
        );
    }
}

#[tokio::test]
async fn refuses_resealed_unsigned_acl_drive_and_late_signer_grants() {
    let fixture = fixture("unsigned_authority", EnvelopeRetention::All).await;
    let other_drive = signed_drive(&fixture.store, &fixture.owner, "Unrelated signed root").await;
    let mut request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    request.resources.push(RecoverySelection {
        subject: other_drive.clone(),
        expected_last_commit: revision(&fixture.store, &other_drive),
    });
    request.allowed_drives.push(other_drive.clone());
    let bytes = export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    let original = decode_for_owner(&bytes, &fixture.owner).unwrap();
    let outsider = Agent::new(Some("Unsigned grantee")).unwrap();
    for mutation in ["acl", "late-signer-grant", "drive"] {
        let mut bundle = original.clone();
        let index = bundle
            .resources
            .iter()
            .position(|resource| resource.subject == canonical_id(&fixture.store, &fixture.subject))
            .unwrap();
        let doc = strict_snapshot_doc(&bundle.resources[index].snapshot).unwrap();
        match mutation {
            "acl" => doc
                .set_property(
                    urls::WRITE,
                    &Value::ResourceArray(vec![
                        fixture.owner.subject.as_str().into(),
                        outsider.subject.as_str().into(),
                    ]),
                )
                .unwrap(),
            "late-signer-grant" => doc
                .set_property(
                    urls::WRITE,
                    &Value::ResourceArray(vec![fixture.owner.subject.as_str().into()]),
                )
                .unwrap(),
            "drive" => doc
                .set_property(
                    urls::DRIVE_PROP,
                    &Value::AtomicUrl(other_drive.as_str().into()),
                )
                .unwrap(),
            _ => unreachable!(),
        }
        let snapshot = doc.export_snapshot();
        let manifest = &mut bundle.manifest.resources[index];
        manifest.snapshot_length = snapshot.len();
        manifest.snapshot_sha256 = sha256(&snapshot);
        manifest.version_vector = version_vector(&snapshot).unwrap();
        bundle.resources[index].snapshot = snapshot;
        let malformed_bytes = reseal_as_source(&mut bundle, &fixture.owner).unwrap();
        let forged = decode_for_owner(&malformed_bytes, &fixture.owner).unwrap();
        let target = Db::init_redb(Some("https://localhost".into()))
            .await
            .unwrap();
        let error = restore_bundle(&target, &forged).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Recovery snapshot operation is not covered by original spans"),
            "{mutation}: {error}"
        );
    }
}

#[tokio::test]
async fn source_authenticated_invalid_native_signature_is_rejected() {
    let fixture = fixture("partial", EnvelopeRetention::All).await;
    let request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    let bytes = export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    let mut bundle = decode_for_owner(&bytes, &fixture.owner).unwrap();
    let envelope = &mut bundle.resources[0].envelopes[0].json;
    let mut json: serde_json::Value = serde_json::from_str(envelope).unwrap();
    json[urls::SIGNATURE] = serde_json::Value::String("AA==".into());
    *envelope = serde_json::to_string(&json).unwrap();
    let malformed_bytes = reseal_modified_envelope(&mut bundle, 0, 0, &fixture.owner).unwrap();
    let authenticated = decode_for_owner(&malformed_bytes, &fixture.owner).unwrap();

    let target = Db::init_redb(Some("https://localhost".into()))
        .await
        .unwrap();
    let error = restore_bundle(&target, &authenticated).await.unwrap_err();
    assert!(
        error.to_string().contains("signature must be 64 bytes"),
        "{error}"
    );
    assert!(target
        .get_resource(&Subject::from(fixture.drive.as_str()))
        .await
        .is_ok());
}

#[tokio::test]
async fn rejects_unknown_envelope_properties_without_contacting_loopback() {
    let fixture = fixture("unknown_envelope_property", EnvelopeRetention::All).await;
    let request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    let bytes = export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    let mut bundle = decode_for_owner(&bytes, &fixture.owner).unwrap();
    let trap = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    trap.set_nonblocking(true).unwrap();
    let property = format!("http://{}/unknown-property", trap.local_addr().unwrap());
    let envelope = &mut bundle.resources[0].envelopes[0].json;
    let mut json: serde_json::Value = serde_json::from_str(envelope).unwrap();
    json[property] = serde_json::Value::String("untrusted".into());
    json[urls::SIGNATURE] = serde_json::Value::String("invalid".into());
    *envelope = serde_json::to_string(&json).unwrap();
    let malformed_bytes = reseal_modified_envelope(&mut bundle, 0, 0, &fixture.owner).unwrap();
    let authenticated = decode_for_owner(&malformed_bytes, &fixture.owner).unwrap();
    let target = Db::init_redb(Some("https://localhost".into()))
        .await
        .unwrap();
    let error = restore_bundle(&target, &authenticated).await.unwrap_err();
    assert!(error.to_string().contains("unsupported property"));
    assert_eq!(
        trap.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn signed_optional_property_and_nested_class_require_explicit_support() {
    let fixture = fixture("custom_schema", EnvelopeRetention::All).await;
    let nested_class = fixture
        .store
        .create_resource(
            urls::CLASS,
            &fixture.drive,
            "Nested class",
            Some(vec![
                (urls::SHORTNAME, Value::Slug("nested-class".into())),
                (
                    urls::DESCRIPTION,
                    Value::Markdown("Nested custom class".into()),
                ),
            ]),
        )
        .await
        .unwrap();
    let mut properties = Vec::new();
    for (shortname, datatype) in [
        ("optional-text", urls::STRING),
        ("optional-nested", urls::ATOMIC_URL),
    ] {
        properties.push(
            fixture
                .store
                .create_resource(
                    urls::PROPERTY,
                    &fixture.drive,
                    shortname,
                    Some(vec![
                        (urls::SHORTNAME, Value::Slug(shortname.into())),
                        (
                            urls::DESCRIPTION,
                            Value::Markdown("Optional, absent from requires/recommends".into()),
                        ),
                        (urls::DATATYPE_PROP, Value::AtomicUrl(datatype.into())),
                    ]),
                )
                .await
                .unwrap(),
        );
    }
    let mut source = fixture
        .store
        .get_resource(&Subject::from(fixture.subject.as_str()))
        .await
        .unwrap();
    source
        .set(
            properties[0].clone(),
            Value::String("signed optional value".into()),
            &fixture.store,
        )
        .await
        .unwrap();
    let nested: HashMap<String, Value> = HashMap::from([
        (
            urls::IS_A.into(),
            Value::ResourceArray(vec![nested_class.as_str().into()]),
        ),
        (
            urls::NAME.into(),
            Value::String("signed nested value".into()),
        ),
    ]);
    let encoded_nested = serde_json::to_string(&nested).unwrap();
    let nested_doc = source.build_state_doc().unwrap();
    nested_doc
        .doc()
        .get_map("properties")
        .insert(properties[1].as_str(), encoded_nested.as_str())
        .unwrap();
    nested_doc
        .doc()
        .get_map("datatypes")
        .insert(properties[1].as_str(), "resource")
        .unwrap();
    source.apply_state_doc(nested_doc).unwrap();
    source.save_locally(&fixture.store).await.unwrap();

    fn assert_persisted_nested_value(resource: &Resource, property: &str, nested_class: &str) {
        let value = resource.get(property).unwrap();
        let Value::NestedResource(crate::values::SubResource::Nested(properties)) = value else {
            panic!("persisted nested property was not materialized as a typed nested resource: {value:?}");
        };
        let Value::ResourceArray(classes) = properties.get(urls::IS_A).unwrap() else {
            panic!("nested isA was not materialized as a resource array");
        };
        assert_eq!(
            classes.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec![nested_class.to_string()]
        );
        assert!(matches!(
            properties.get(urls::NAME),
            Some(Value::String(name)) if name == "signed nested value"
        ));
    }

    let persisted_source = fixture
        .store
        .get_resource(&Subject::from(fixture.subject.as_str()))
        .await
        .unwrap();
    assert_persisted_nested_value(&persisted_source, &properties[1], nested_class.as_str());
    let mut complete = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    for subject in properties.iter().chain(std::iter::once(&nested_class)) {
        complete.resources.push(RecoverySelection {
            subject: subject.clone(),
            expected_last_commit: revision(&fixture.store, subject),
        });
    }
    let bytes = export_bundle(&fixture.store, &complete, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    let target = Db::init_redb(Some("https://localhost".into()))
        .await
        .unwrap();
    restore_bundle(&target, &decode_for_owner(&bytes, &fixture.owner).unwrap())
        .await
        .unwrap();
    let restored = target
        .get_resource(&Subject::from(fixture.subject.as_str()))
        .await
        .unwrap();
    assert_eq!(
        restored.get(&properties[0]).unwrap().to_string(),
        "signed optional value"
    );
    assert_persisted_nested_value(&restored, &properties[1], nested_class.as_str());

    for (missing_kind, missing) in [
        ("optional property", &properties[0]),
        ("nested class", &nested_class),
    ] {
        let mut incomplete = complete.clone();
        incomplete
            .resources
            .retain(|entry| &entry.subject != missing);
        let error = export_bundle(&fixture.store, &incomplete, &owner_actor(&fixture.owner))
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Recovery dependency is not explicit"),
            "missing {missing_kind}: {error}"
        );
        assert!(
            error
                .to_string()
                .contains(&canonical_id(&fixture.store, missing)),
            "missing {missing_kind}: {error}"
        );

        let mut incomplete_bundle = decode_for_owner(&bytes, &fixture.owner).unwrap();
        let omitted = canonical_id(&fixture.store, missing);
        let removed_envelopes = incomplete_bundle
            .resources
            .iter()
            .find(|resource| resource.subject == omitted)
            .unwrap()
            .envelopes
            .len();
        incomplete_bundle
            .resources
            .retain(|resource| resource.subject != omitted);
        incomplete_bundle
            .manifest
            .resources
            .retain(|resource| resource.subject != omitted);
        incomplete_bundle.manifest.envelope_count -= removed_envelopes;
        let malformed_bytes = reseal_as_source(&mut incomplete_bundle, &fixture.owner).unwrap();
        let resealed = decode_for_owner(&malformed_bytes, &fixture.owner).unwrap();
        let target = Db::init_redb(Some("https://localhost".into()))
            .await
            .unwrap();
        let error = restore_bundle(&target, &resealed).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Recovery dependency is not explicit"),
            "missing {missing_kind}: {error}"
        );
        assert!(
            error.to_string().contains(&omitted),
            "missing {missing_kind}: {error}"
        );
    }
}

#[tokio::test]
async fn declared_missing_external_ancestor_never_contacts_loopback() {
    let fixture = fixture("missing_ancestor", EnvelopeRetention::All).await;
    let positive = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    export_bundle(&fixture.store, &positive, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    let trap = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    trap.set_nonblocking(true).unwrap();
    let missing = format!(
        "http://{}/unmaterialized-ancestor",
        trap.local_addr().unwrap()
    );
    let mut child = fixture
        .store
        .get_resource(&Subject::from(fixture.subject.as_str()))
        .await
        .unwrap();
    child
        .set(
            urls::PARENT.into(),
            Value::AtomicUrl(missing.clone().into()),
            &fixture.store,
        )
        .await
        .unwrap();
    child.save_locally(&fixture.store).await.unwrap();
    let mut request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    request.resources.swap(0, 1);
    request.resources.push(RecoverySelection {
        subject: missing,
        expected_last_commit: "atomic:commit:unavailable".into(),
    });
    let outsider = Agent::new(Some("No ancestor grant")).unwrap();
    let error = export_bundle(&fixture.store, &request, &owner_actor(&outsider))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("not authorized for every selected resource"),
        "{error}"
    );
    assert_eq!(
        trap.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn later_signed_snapshot_cannot_replace_missing_original_envelope() {
    let fixture = fixture("missing_middle_history", EnvelopeRetention::All).await;
    let request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    let bytes = export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    let mut bundle = decode_for_owner(&bytes, &fixture.owner).unwrap();
    let index = bundle
        .resources
        .iter()
        .position(|resource| resource.subject == canonical_id(&fixture.store, &fixture.subject))
        .unwrap();
    assert!(bundle.resources[index].envelopes.len() >= 3);
    let missing = bundle.resources[index].envelopes.remove(1);
    bundle.manifest.resources[index]
        .envelopes
        .retain(|envelope| envelope.signature != missing.signature);
    bundle.manifest.resources[index]
        .history
        .attributions
        .retain(|attribution| attribution.signature != missing.signature);
    bundle.manifest.envelope_count -= 1;
    let malformed_bytes = reseal_as_source(&mut bundle, &fixture.owner).unwrap();
    let resealed = decode_for_owner(&malformed_bytes, &fixture.owner).unwrap();
    let target = Db::init_redb(Some("https://localhost".into()))
        .await
        .unwrap();
    let error = restore_bundle(&target, &resealed).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Recovery snapshot operation is not covered by original spans"),
        "{error}"
    );
    let prefix = format!("{}\0", canonical_id(&fixture.store, &fixture.subject));
    let key = fixture
        .store
        .kv
        .scan_prefix(Tree::Envelopes, prefix.as_bytes())
        .map(Result::unwrap)
        .find(|(key, _)| key.ends_with(missing.signature.as_bytes()))
        .unwrap()
        .0;
    fixture.store.kv.remove(Tree::Envelopes, &key).unwrap();
    let error = export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Recovery snapshot operation is not covered by original spans"),
        "{error}"
    );
}

#[tokio::test]
async fn refuses_export_with_unreadable_compact_history() {
    let fixture = fixture("unreadable_compact_history", EnvelopeRetention::All).await;
    let request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    let prefix = format!("{}\0", canonical_id(&fixture.store, &fixture.subject));
    let key = fixture
        .store
        .kv
        .scan_prefix(Tree::Envelopes, prefix.as_bytes())
        .next()
        .unwrap()
        .unwrap()
        .0;
    fixture
        .store
        .kv
        .insert(Tree::Envelopes, &key, &[1, 0])
        .unwrap();
    let error = export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Malformed retained recovery envelope"),
        "{error}"
    );
}

#[tokio::test]
async fn native_export_ceiling_counts_manifest_after_a_valid_signed_payload() {
    let fixture = fixture("native_bundle_ceiling", EnvelopeRetention::All).await;
    let request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    let bytes = export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    let bundle = decode_for_owner(&bytes, &fixture.owner).unwrap();
    let payload = encode_native_payload(&bundle.resources).unwrap();
    let manifest_size = serde_json::to_vec(bundle.manifest()).unwrap().len();
    let padding = MAX_BUNDLE_BYTES - BUNDLE_HEADER_BYTES - manifest_size + 1 - payload.len();
    let prefix = format!("{}\0", canonical_id(&fixture.store, &fixture.subject));
    let (key, _) = fixture
        .store
        .kv
        .scan_prefix(Tree::Envelopes, prefix.as_bytes())
        .next()
        .unwrap()
        .unwrap();
    let mut json = crate::envelopes::envelopes(&fixture.store, fixture.subject.as_str())
        .into_iter()
        .find(|envelope| key.ends_with(envelope.signature.as_bytes()))
        .unwrap()
        .json
        .into_bytes();
    json.resize(json.len() + padding, b' ');
    fixture
        .store
        .kv
        .insert(Tree::Envelopes, &key, &json)
        .unwrap();
    let error = export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("recovery manifest exceeds bundle limit"));
}

struct PrimitiveBackend(Arc<dyn KvStore>);

impl KvStore for PrimitiveBackend {
    fn get(&self, tree: Tree, key: &[u8]) -> AtomicResult<Option<Vec<u8>>> {
        self.0.get(tree, key)
    }

    fn insert(&self, tree: Tree, key: &[u8], value: &[u8]) -> AtomicResult<()> {
        self.0.insert(tree, key, value)
    }

    fn remove(&self, tree: Tree, key: &[u8]) -> AtomicResult<()> {
        self.0.remove(tree, key)
    }

    fn contains_key(&self, tree: Tree, key: &[u8]) -> AtomicResult<bool> {
        self.0.contains_key(tree, key)
    }

    fn scan_prefix(&self, tree: Tree, prefix: &[u8]) -> KvIter {
        self.0.scan_prefix(tree, prefix)
    }

    fn range(&self, tree: Tree, start: Vec<u8>, end: Vec<u8>, reverse: bool) -> KvIter {
        self.0.range(tree, start, end, reverse)
    }

    fn iter_tree(&self, tree: Tree) -> KvIter {
        self.0.iter_tree(tree)
    }

    fn clear_tree(&self, tree: Tree) -> AtomicResult<()> {
        self.0.clear_tree(tree)
    }

    fn apply_batch(&self, operations: &[crate::db::trees::Operation]) -> AtomicResult<()> {
        self.0.apply_batch(operations)
    }

    fn flush(&self) -> AtomicResult<()> {
        self.0.flush()
    }

    fn len(&self, tree: Tree) -> AtomicResult<usize> {
        self.0.len(tree)
    }
}

#[tokio::test]
async fn recovery_refuses_backend_without_immutable_view_before_reading_payload() {
    let mut fixture = fixture("unsupported_backend", EnvelopeRetention::All).await;
    let request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    fixture.store.kv = Arc::new(PrimitiveBackend(fixture.store.kv.clone()));
    let error = export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Storage backend does not support consistent read views"
    );
    assert!(fixture
        .store
        .get_resource(&Subject::from(fixture.subject.as_str()))
        .await
        .is_ok());
}

#[cfg(feature = "db-sled")]
#[tokio::test]
async fn populated_sled_backend_refuses_recovery_but_preserves_ordinary_reads() {
    let path = std::path::PathBuf::from(format!(
        ".temp/db/native_recovery_sled_{}",
        crate::utils::random_string(10)
    ));
    let store = Db::init(&path, Some("https://localhost".into()))
        .await
        .unwrap();
    store.set_envelope_retention(EnvelopeRetention::All);
    let (owner, drive) = store.setup("Disposable Sled owner").await.unwrap();
    let subject = store
        .create_resource(
            urls::CLASS,
            &drive,
            "Sled Source",
            Some(vec![
                (urls::SHORTNAME, Value::Slug("sled-source".into())),
                (
                    urls::DESCRIPTION,
                    Value::Markdown("Full textual Source on the unsupported backend".into()),
                ),
            ]),
        )
        .await
        .unwrap();
    let request = request_for(&store, &drive, &subject);
    let before = store
        .get_resource(&Subject::from(subject.as_str()))
        .await
        .unwrap();
    assert_eq!(
        before.get(urls::DESCRIPTION).unwrap().to_string(),
        "Full textual Source on the unsupported backend"
    );
    let error = export_bundle(&store, &request, &owner_actor(&owner))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not support consistent read views"),
        "{error}"
    );
    let after = store
        .get_resource(&Subject::from(subject.as_str()))
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&before.to_json_ad(None).unwrap()).unwrap(),
        serde_json::from_str::<serde_json::Value>(&after.to_json_ad(None).unwrap()).unwrap(),
    );
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn complete_bundle_ceiling_includes_header_and_manifest() {
    let fixture = fixture("bundle_ceiling", EnvelopeRetention::All).await;
    let request = request_for(&fixture.store, &fixture.drive, &fixture.subject);
    let bytes = export_bundle(&fixture.store, &request, &owner_actor(&fixture.owner))
        .await
        .unwrap();
    let bundle = decode_for_owner(&bytes, &fixture.owner).unwrap();
    let manifest_size = serde_json::to_vec(bundle.manifest()).unwrap().len();
    let payload_size = MAX_BUNDLE_BYTES - BUNDLE_HEADER_BYTES - manifest_size;
    let payload = vec![0; payload_size + 1];
    assert_eq!(
        seal_bundle(bundle.manifest(), &payload[..payload_size])
            .unwrap()
            .len(),
        MAX_BUNDLE_BYTES
    );
    assert!(seal_bundle(bundle.manifest(), &payload).is_err());
}

#[test]
fn enforces_request_size_and_explicit_resource_count() {
    let too_many = RecoveryRequest {
        version: 1,
        resources: (0..=MAX_RESOURCES)
            .map(|index| RecoverySelection {
                subject: format!("atomic:resource:{index}"),
                expected_last_commit: format!("atomic:commit:{index}"),
            })
            .collect(),
        allowed_drives: vec!["atomic:resource:drive".into()],
    };
    assert!(parse_request(&serde_json::to_vec(&too_many).unwrap()).is_err());

    let oversized = format!(
        "{{\"version\":1,\"resources\":[],\"allowed_drives\":[],\"padding\":\"{}\"}}",
        "x".repeat(MAX_REQUEST_BYTES)
    );
    assert!(parse_request(oversized.as_bytes()).is_err());
}
