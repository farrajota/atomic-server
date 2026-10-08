//! This contains a minimal set of tests for the server.
//! Most of the more rigorous testing is done in the end-to-end tests:
//! https://github.com/atomicdata-dev/atomic-data-browser/tree/main/data-browser/tests

use crate::{appstate::AppState, config::Opts};

use super::*;
use actix_web::{
    body::MessageBody,
    dev::ServiceResponse,
    test::{self, TestRequest},
    web::Data,
    App,
};
use atomic_lib::{agents::ForAgent, urls, Storelike};
use base64::Engine;

/// Returns the request with signed headers. Also adds a json-ad accept header - overwrite this if you need something else.
fn build_request_authenticated(path: &str, appstate: &AppState) -> TestRequest {
    let origin = appstate.config.get_origin();
    let url = format!("{}{}", origin, path);
    let headers = atomic_lib::client::get_authentication_headers(
        &url,
        &appstate.store.get_default_agent().unwrap(),
    )
    .expect("could not get auth headers");

    let mut prereq = test::TestRequest::with_uri(path);
    for (k, v) in headers {
        prereq = prereq.insert_header((k, v));
    }

    // Ensure the Host header matches the origin used for signing
    if let Ok(u) = url::Url::parse(&origin) {
        if let Some(host) = u.host_str() {
            let authority = if let Some(port) = u.port() {
                format!("{}:{}", host, port)
            } else {
                host.to_string()
            };
            prereq = prereq.insert_header(("Host", authority));
        }
    }

    prereq.insert_header(("Accept", "application/ad+json"))
}

#[actix_rt::test]
async fn server_tests() {
    // Enable logging
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info,atomic_server=trace")
        .try_init();

    let unique_string = atomic_lib::utils::random_string(10);
    use clap::Parser;
    let opts = Opts::parse_from([
        "atomic-server",
        "--initialize",
        "--data-dir",
        &format!("./.temp/{}/db", unique_string),
        "--config-dir",
        &format!("./.temp/{}/config", unique_string),
    ]);

    let mut config = config::build_config(opts)
        .map_err(|e| format!("Initialization failed: {}", e))
        .expect("failed init config");
    // This prevents folder access issues when running concurrent tests
    config.search_index_path = format!("./.temp/{}/search_index", unique_string).into();
    config.vector_search_index_path =
        format!("./.temp/{}/vector_search_index", unique_string).into();

    let appstate = crate::appstate::AppState::init(config.clone())
        .await
        .expect("failed init appstate");

    // For tests, we manually populate a test drive and collections
    atomic_lib::test_utils::setup_test_env(&appstate.store)
        .await
        .unwrap();

    let data = Data::new(appstate.clone());
    let app = test::init_service(
        App::new()
            .app_data(data)
            .configure(crate::routes::config_routes),
    )
    .await;
    let store = &appstate.store;

    // Get HTML page
    let req =
        build_request_authenticated("/", &appstate).insert_header(("Accept", "application/html"));
    let resp = test::call_service(&app, req.to_request()).await;
    let is_success = resp.status().is_success();
    let body = get_body(resp);
    // println!("{:?}", body);
    assert!(is_success);
    assert!(body.as_str().contains("html"));

    // Should 404
    let req = test::TestRequest::with_uri("/doesnotexist")
        .append_header(("Accept", "application/ld+json"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_client_error());

    // Edit the main drive, make it hidden to the public agent
    let drive_did = store.get_drive_did("localhost").await.unwrap().unwrap();
    let mut drive = store.get_resource(&drive_did).await.unwrap();
    drive
        .set(
            urls::READ.into(),
            vec![appstate.store.get_default_agent().unwrap().subject].into(),
            &appstate.store,
        )
        .await
        .unwrap();
    drive.save(store).await.unwrap();

    // Should 401 (Unauthorized)
    let req = test::TestRequest::with_uri("/").insert_header(("Accept", "application/ad+json"));
    let resp = test::call_service(&app, req.to_request()).await;
    let status = resp.status().as_u16();
    let body = get_body(resp);
    if status != 401 {
        panic!(
            "Root resource should be 401 after editing rights. Status: {}, body: {:?}",
            status, body
        );
    }

    // Get JSON-AD
    let req = build_request_authenticated("/", &appstate);
    let resp = test::call_service(&app, req.to_request()).await;
    let status = resp.status().as_u16();
    let body = get_body(resp);
    if status >= 400 {
        panic!(
            "Auth request to /properties status: {}. Expected success. Body: {}",
            status, body
        );
    }
    if !body.contains("\"@id\"") {
        panic!("response should be json-ad. Body: {}", body);
    }

    // Resources with server-side Loro state should expose their snapshot in JSON-AD
    let mut loro_resource = atomic_lib::Resource::new("/loro-sync-test".into());
    loro_resource
        .set_unsafe(
            urls::READ.into(),
            vec![appstate.store.get_default_agent().unwrap().subject.clone()].into(),
        )
        .unwrap();
    loro_resource
        .set_unsafe(
            urls::WRITE.into(),
            vec![appstate.store.get_default_agent().unwrap().subject.clone()].into(),
        )
        .unwrap();
    loro_resource
        .set_unsafe(urls::NAME.into(), "Loro Sync Test".to_string().into())
        .unwrap();
    loro_resource
        .set_unsafe(
            urls::DESCRIPTION.into(),
            atomic_lib::Value::String("Synced through CRDT".into()),
        )
        .unwrap();
    loro_resource.ensure_materialized().unwrap();
    store
        .add_resource_opts(&loro_resource, false, true, true)
        .await
        .unwrap();

    let req = build_request_authenticated("/loro-sync-test", &appstate);
    let resp = test::call_service(&app, req.to_request()).await;
    assert!(
        resp.status().is_success(),
        "loro resource fetch should succeed"
    );
    let body = get_body(resp);
    assert!(
        body.as_str()
            .contains("\"https://atomicdata.dev/properties/loroUpdate\""),
        "resource fetch should include loroUpdate when server has a Loro snapshot: {}",
        body.as_str()
    );

    // Get JSON-LD
    let req = build_request_authenticated("/", &appstate)
        .insert_header(("Accept", "application/ld+json"));
    let resp = test::call_service(&app, req.to_request()).await;
    assert!(resp.status().is_success(), "setup not returning JSON-LD");
    let body = get_body(resp);
    assert!(
        body.as_str().contains("@context"),
        "response should be json-ld"
    );

    // Get turtle
    let req = build_request_authenticated("/", &appstate).insert_header(("Accept", "text/turtle"));
    let resp = test::call_service(&app, req.to_request()).await;
    assert!(resp.status().is_success());
    let body = get_body(resp);
    assert!(
        body.as_str().starts_with("<"),
        "response should be turtle, but was: {}",
        body.as_str()
    );

    // Get Search
    // Does not test the contents of the results - the index isn't built at this point
    let req = build_request_authenticated("/search?q=setup", &appstate);
    let resp = test::call_service(&app, req.to_request()).await;
    assert!(resp.status().is_success());
    let body = get_body(resp);
    println!("{}", body.as_str());
    assert!(
        body.as_str().contains("/results"),
        "response should be a search resource"
    );

    // Identifier resolution endpoints: /resource is canonical; /did and /atomic alias it.
    for path in ["/did", "/resource", "/atomic"] {
        let req = build_request_authenticated(path, &appstate);
        let resp = test::call_service(&app, req.to_request()).await;
        assert!(resp.status().is_success(), "{path}");
        let body = get_body(resp);
        assert!(
            body.as_str().contains("atomic:"),
            "response should describe identifier resolution, got: {}",
            body.as_str()
        );
    }

    // Path-form identifiers reach the store (404 if missing), not a 500/401 first.
    for path in ["/did:ad:test", "/atomic:test"] {
        let req = build_request_authenticated(path, &appstate);
        let resp = test::call_service(&app, req.to_request()).await;
        assert_eq!(
            resp.status(),
            404,
            "Should be a 404, because `{path}` does not exist"
        );
    }

    // Test Unauthenticated Invite with Public Key
    let issuer_agent = appstate.store.get_default_agent().unwrap();
    let target_resource_subject = "https://atomicdata.dev/test/resource";
    // We need to create the target resource to check write rights
    let mut target = atomic_lib::Resource::new(target_resource_subject.into());
    target
        .set(
            urls::READ.into(),
            vec![issuer_agent.subject.clone()].into(),
            &appstate.store,
        )
        .await
        .unwrap();
    target
        .set(
            urls::WRITE.into(),
            vec![issuer_agent.subject.clone()].into(),
            &appstate.store,
        )
        .await
        .unwrap();
    target.save_locally(&appstate.store).await.unwrap();

    let expiration = atomic_lib::utils::now() + 100000;

    // Construct the InviteToken manually as we don't have a helper in the lib for this yet
    // This replicates what the frontend does
    let mut signable_json = serde_json::Map::new();
    signable_json.insert(
        urls::TARGET.into(),
        serde_json::Value::String(target_resource_subject.into()),
    );
    signable_json.insert(urls::WRITE_BOOL.into(), serde_json::Value::Bool(true));
    signable_json.insert(
        urls::EXPIRES_AT.into(),
        serde_json::Value::Number(expiration.into()),
    );
    signable_json.insert(
        urls::SIGNER.into(),
        serde_json::Value::String(issuer_agent.subject.to_string()),
    );

    let serialized = serde_jcs::to_string(&signable_json).unwrap();
    let private_key = issuer_agent.private_key.clone().unwrap();
    let signature =
        atomic_lib::commit::sign_message(&serialized, &private_key, &issuer_agent.public_key)
            .unwrap();

    let mut map = signable_json;
    map.insert(urls::SIGNATURE.into(), serde_json::Value::String(signature));

    let bytes = serde_json::to_vec(&map).unwrap();
    let token_base64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    let token_encoded: String =
        url::form_urlencoded::byte_serialize(token_base64.as_bytes()).collect();

    // Generate a new public key for the visitor
    let visitor_agent = atomic_lib::agents::Agent::new(None).unwrap();
    let public_key = visitor_agent.public_key; // This gives the Base64 public key
    let public_key_encoded: String =
        url::form_urlencoded::byte_serialize(public_key.as_bytes()).collect();

    let path = format!(
        "/invites?token={}&public-key={}",
        token_encoded, public_key_encoded
    );

    // Use an unauthenticated request
    let req = test::TestRequest::with_uri(&path).insert_header(("Accept", "application/ad+json"));
    let resp = test::call_service(&app, req.to_request()).await;

    assert!(
        resp.status().is_success(),
        "Invite request failed: Status {}",
        resp.status()
    );

    let body = get_body(resp);
    assert!(
        body.contains(urls::DESTINATION) || body.contains(urls::INVITE),
        "Response should contain either destination (redirect) or invite metadata. Body: {}",
        body
    );
}

/// A brand-new store gets the core models without `--initialize`. Opening
/// the `Db` seeds them (`populate::bootstrap` in `Db::init_redb_file`);
/// `AppState::init` no longer has a bootstrap branch of its own, whose
/// "store did not exist yet" condition ran after the store directory had
/// already been created and so never fired.
#[actix_rt::test]
async fn fresh_store_gets_core_models_without_initialize() {
    use clap::Parser;
    let unique_string = atomic_lib::utils::random_string(10);
    let data_dir = format!("./.temp/{}/db", unique_string);
    assert!(!std::path::Path::new(&data_dir).exists());
    let opts = Opts::parse_from([
        "atomic-server",
        "--data-dir",
        &data_dir,
        "--config-dir",
        &format!("./.temp/{}/config", unique_string),
    ]);
    let mut config = config::build_config(opts).expect("failed init config");
    config.search_index_path = format!("./.temp/{}/search_index", unique_string).into();
    config.vector_search_index_path =
        format!("./.temp/{}/vector_search_index", unique_string).into();

    let appstate = crate::appstate::AppState::init(config)
        .await
        .expect("failed init appstate");
    let store = &appstate.store;

    for core in [
        urls::SHORTNAME,
        urls::DESCRIPTION,
        urls::CLASS,
        urls::PROPERTY,
    ] {
        assert!(
            store.has_stored_resource(&core.into()),
            "fresh store must have core model {core}"
        );
    }
    let class = store.get_resource(&urls::CLASS.into()).await.unwrap();
    assert_eq!(
        class.get(urls::SHORTNAME).unwrap().to_string(),
        "class",
        "core Class resource must be materialized"
    );
}

/// A plain GET to `/ws` (a crawler, a pasted URL) is not an upgrade. It must be
/// a 400, not the 500 that Sentry reports as an incident.
#[actix_rt::test]
async fn websocket_route_answers_400_to_a_request_that_is_not_an_upgrade() {
    use clap::Parser;
    let unique_string = atomic_lib::utils::random_string(10);
    let opts = Opts::parse_from([
        "atomic-server",
        "--initialize",
        "--data-dir",
        &format!("./.temp/{}/db", unique_string),
        "--config-dir",
        &format!("./.temp/{}/config", unique_string),
    ]);
    let mut config = config::build_config(opts)
        .map_err(|e| format!("Initialization failed: {}", e))
        .expect("failed init config");
    config.search_index_path = format!("./.temp/{}/search_index", unique_string).into();
    let appstate = crate::appstate::AppState::init(config.clone())
        .await
        .expect("failed init appstate");
    let app = test::init_service(
        App::new()
            .app_data(Data::new(appstate))
            .configure(crate::routes::config_routes),
    )
    .await;

    let req = test::TestRequest::get().uri("/ws").to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), actix_web::http::StatusCode::BAD_REQUEST);
}

#[actix_rt::test]
async fn test_did_agent_edit() {
    use atomic_lib::{agents::Agent, commit::CommitBuilder, urls, Resource, Value};
    let unique_string = atomic_lib::utils::random_string(10);
    use clap::Parser;
    let opts = Opts::parse_from([
        "atomic-server",
        "--initialize",
        "--data-dir",
        &format!("./.temp/{}/db", unique_string),
        "--config-dir",
        &format!("./.temp/{}/config", unique_string),
    ]);

    let mut config = config::build_config(opts)
        .map_err(|e| format!("Initialization failed: {}", e))
        .expect("failed init config");
    config.search_index_path = format!("./.temp/{}/search_index", unique_string).into();

    let appstate = crate::appstate::AppState::init(config.clone())
        .await
        .expect("failed init appstate");

    let data = Data::new(appstate.clone());
    let app = test::init_service(
        App::new()
            .app_data(data)
            .configure(crate::routes::config_routes),
    )
    .await;

    // 1. Create a new agent locally
    let agent = Agent::new(Some("Test User")).unwrap();
    let agent_did = agent.subject.pure_id();

    // 2. Setup onboarding: create a drive and map it
    let drive_did = "did:ad:test-drive";
    let mut drive = Resource::new(drive_did.into());
    drive.set_class(urls::DRIVE).unwrap();
    drive
        .set(
            urls::READ.into(),
            vec![urls::PUBLIC_AGENT.to_string()].into(),
            &appstate.store,
        )
        .await
        .unwrap();
    drive
        .set(
            urls::WRITE.into(),
            vec![agent_did.clone()].into(),
            &appstate.store,
        )
        .await
        .unwrap();
    appstate.store.add_resource(&drive).await.unwrap();

    appstate
        .store
        .add_drive_mapping("localhost", &Value::AtomicUrl(drive_did.into()))
        .unwrap();

    // 3. Setup the agent resource manually in the store
    let mut agent_res = agent.to_resource().unwrap();
    agent_res.set_subject(agent_did.clone());
    agent_res
        .set_unsafe(urls::NAME.into(), Value::String("Initial Name".into()))
        .unwrap();
    // Dummy last commit to avoid genesis trigger
    agent_res
        .set_unsafe(
            urls::LAST_COMMIT.into(),
            Value::AtomicUrl("dummy-initial-commit".into()),
        )
        .unwrap();
    appstate
        .store
        .add_resource_opts(&agent_res, false, false, true)
        .await
        .unwrap();

    // 4. Create a commit to edit the agent's name
    let mut builder = CommitBuilder::new(agent_did.clone().into());
    builder.set(urls::NAME.into(), Value::String("Updated Name".into()));

    let commit = builder
        .sign(&agent, &appstate.store, &agent_res)
        .await
        .unwrap();
    let mut opts = atomic_lib::commit::CommitOpts::no_validations_no_index();
    opts.update_index = true;
    appstate
        .store
        .apply_commit(commit, &opts)
        .await
        .expect("Failed to apply commit directly");

    // 5. Fetch the agent resource via GET and verify the name change
    let req = test::TestRequest::get()
        .uri(&format!(
            "/resource?subject={}",
            urlencoding::encode(&agent_did)
        ))
        .insert_header(("Accept", "application/ad+json"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "Fetch failed with status: {:?}",
        resp.status()
    );

    let body = get_body(resp);
    assert!(
        body.contains("Updated Name"),
        "Body does not contain 'Updated Name'. Body: {}",
        body
    );
}

#[actix_rt::test]
async fn self_signed_agent_commit_keeps_name() {
    let unique_string = atomic_lib::utils::random_string(10);
    use clap::Parser;
    let opts = Opts::parse_from([
        "atomic-server",
        "--initialize",
        "--data-dir",
        &format!("./.temp/{}/db", unique_string),
        "--config-dir",
        &format!("./.temp/{}/config", unique_string),
    ]);

    let mut config = config::build_config(opts)
        .map_err(|e| format!("Initialization failed: {}", e))
        .expect("failed init config");
    config.search_index_path = format!("./.temp/{}/search_index", unique_string).into();

    let appstate = crate::appstate::AppState::init(config.clone())
        .await
        .expect("failed init appstate");

    let data = Data::new(appstate.clone());
    let app = test::init_service(
        App::new()
            .app_data(data)
            .configure(crate::routes::config_routes),
    )
    .await;

    let agent = atomic_lib::agents::Agent::new(None).unwrap();
    let agent_did = agent.subject.pure_id();
    let empty = atomic_lib::Resource::new(agent_did.clone());

    let mut builder = atomic_lib::commit::CommitBuilder::new(agent_did.clone().into());
    builder.is_genesis = true;
    builder.set(
        urls::IS_A.into(),
        atomic_lib::Value::ResourceArray(vec![urls::AGENT.to_string().into()]),
    );
    builder.set(
        urls::NAME.into(),
        atomic_lib::Value::String("Test User".into()),
    );

    let commit = builder.sign(&agent, &appstate.store, &empty).await.unwrap();
    let body = commit
        .into_resource(&appstate.store)
        .await
        .unwrap()
        .to_json_ad(Some(&appstate.config.get_origin()))
        .unwrap();

    let req = TestRequest::post()
        .uri("/commit")
        .insert_header(("Content-Type", "application/ad+json"))
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "commit post failed with status {:?}: {}",
        resp.status(),
        get_body(resp)
    );

    // Authenticate the GET — the resource lives behind the default rights
    // model, so unauthenticated reads return 401.
    let req = build_request_authenticated(
        &format!("/did?subject={}", urlencoding::encode(&agent_did)),
        &appstate,
    );
    let resp = test::call_service(&app, req.to_request()).await;
    assert!(
        resp.status().is_success(),
        "Fetch failed with status: {:?}",
        resp.status()
    );

    let body = get_body(resp);
    assert!(
        body.contains("Test User"),
        "Body does not contain persisted agent name. Body: {}",
        body
    );
}

/// A fresh, initialized `AppState` on its own temporary directories, with
/// `extra_args` appended to the command line (`["--domain", "x"]`).
pub(crate) async fn init_test_appstate(extra_args: &[&str]) -> AppState {
    let unique_string = atomic_lib::utils::random_string(10);
    use clap::Parser;
    let data_dir = format!("./.temp/{}/db", unique_string);
    let config_dir = format!("./.temp/{}/config", unique_string);
    let mut args = vec![
        "atomic-server",
        "--initialize",
        "--data-dir",
        &data_dir,
        "--config-dir",
        &config_dir,
    ];
    args.extend_from_slice(extra_args);
    let opts = Opts::parse_from(args);

    let mut config = config::build_config(opts).expect("failed init config");
    // Every test gets its own index directories: parallel runs sharing the
    // default ones trip Tantivy's `LockBusy`.
    config.search_index_path = format!("./.temp/{}/search_index", unique_string).into();
    config.vector_search_index_path =
        format!("./.temp/{}/vector_search_index", unique_string).into();

    crate::appstate::AppState::init(config)
        .await
        .expect("failed init appstate")
}

/// A client's mistake is answered as a client error, not as 500 (security
/// audit D: auth failures and malformed bodies used to report themselves as
/// crashes and fill Sentry). Real handlers, real bodies.
#[actix_rt::test]
async fn client_errors_are_not_server_errors() {
    let appstate = init_test_appstate(&[]).await;
    atomic_lib::test_utils::setup_test_env(&appstate.store)
        .await
        .unwrap();
    let app = test::init_service(
        App::new()
            .app_data(Data::new(appstate.clone()))
            .configure(crate::routes::config_routes),
    )
    .await;

    let post_commit = |body: String| {
        TestRequest::post()
            .uri("/commit")
            .insert_header(("Content-Type", "application/ad+json"))
            .set_payload(body)
            .to_request()
    };

    // A body that is not JSON.
    let resp = test::call_service(&app, post_commit("{\"not json".into())).await;
    assert_eq!(
        resp.status(),
        actix_web::http::StatusCode::BAD_REQUEST,
        "{}",
        get_body(resp)
    );

    // JSON, but not a commit: no signature.
    let signer = atomic_lib::agents::Agent::new(None).unwrap();
    let unsigned = serde_json::json!({
        urls::SUBJECT: "did:ad:something",
        urls::SIGNER: signer.subject.to_string(),
        urls::CREATED_AT: atomic_lib::utils::now(),
    })
    .to_string();
    let resp = test::call_service(&app, post_commit(unsigned)).await;
    assert_eq!(
        resp.status(),
        actix_web::http::StatusCode::BAD_REQUEST,
        "a commit without a signature is malformed"
    );
    let body = get_body(resp);
    assert!(body.contains("No signature field in Commit"), "{body}");

    // A real commit whose signature was tampered with.
    let agent = atomic_lib::agents::Agent::new(None).unwrap();
    let agent_did = agent.subject.pure_id();
    let empty = atomic_lib::Resource::new(agent_did.clone());
    let mut builder = atomic_lib::commit::CommitBuilder::new(agent_did.clone().into());
    builder.is_genesis = true;
    builder.set(
        urls::IS_A.into(),
        atomic_lib::Value::ResourceArray(vec![urls::AGENT.to_string().into()]),
    );
    let commit = builder.sign(&agent, &appstate.store, &empty).await.unwrap();
    let mut json: serde_json::Value = serde_json::from_str(
        &commit
            .into_resource(&appstate.store)
            .await
            .unwrap()
            .to_json_ad(Some(&appstate.config.get_origin()))
            .unwrap(),
    )
    .unwrap();
    json[urls::SIGNATURE] = serde_json::Value::String("AAAA".into());
    let resp = test::call_service(&app, post_commit(json.to_string())).await;
    assert_eq!(
        resp.status(),
        actix_web::http::StatusCode::UNAUTHORIZED,
        "a signature that does not verify is an authentication failure"
    );

    // A read with authentication headers that do not parse, and one whose
    // signature is garbage.
    let drive_did = appstate
        .store
        .get_drive_did("localhost")
        .await
        .unwrap()
        .unwrap();
    let path = format!("/did?subject={}", urlencoding::encode(drive_did.as_str()));
    let resp = test::call_service(
        &app,
        TestRequest::get()
            .uri(&path)
            .insert_header(("Accept", "application/ad+json"))
            .insert_header(("x-atomic-public-key", "AAAA"))
            .insert_header(("x-atomic-signature", "AAAA"))
            .insert_header(("x-atomic-agent", "did:ad:agent:AAAA"))
            .insert_header(("x-atomic-timestamp", "not-a-number"))
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        actix_web::http::StatusCode::UNAUTHORIZED,
        "malformed auth headers"
    );

    let req = build_request_authenticated(&path, &appstate)
        .insert_header(("x-atomic-signature", "AAAA"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        actix_web::http::StatusCode::UNAUTHORIZED,
        "an auth signature that does not verify"
    );
    let body = get_body(resp);
    assert!(body.contains("Authentication failed"), "{body}");
}

/// A multipart body that cannot be read is an error, not a shorter list of
/// files: `/upload` used to end its loop quietly on a multipart error and
/// answer 200 with whatever had been stored so far.
#[actix_rt::test]
async fn upload_reports_a_broken_multipart_body() {
    let appstate = init_test_appstate(&[]).await;
    let app = test::init_service(
        App::new()
            .app_data(Data::new(appstate.clone()))
            .configure(crate::routes::config_routes),
    )
    .await;
    let drive_did = atomic_lib::test_utils::create_test_drive(&appstate.store)
        .await
        .unwrap();
    let path = format!("/upload?parent={}", urlencoding::encode(drive_did.as_str()));

    // `multipart/form-data` without a boundary: nothing can be parsed.
    let req = build_request_authenticated(&path, &appstate)
        .method(actix_web::http::Method::POST)
        .insert_header(("Content-Type", "multipart/form-data"))
        .set_payload("--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nhello\r\n--boundary--\r\n")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        actix_web::http::StatusCode::BAD_REQUEST,
        "no boundary: {}",
        get_body(resp)
    );

    // A body that ends in the middle of a part.
    let req = build_request_authenticated(&path, &appstate)
        .method(actix_web::http::Method::POST)
        .insert_header(("Content-Type", "multipart/form-data; boundary=boundary"))
        .set_payload("--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nhel")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        actix_web::http::StatusCode::BAD_REQUEST,
        "truncated: {}",
        get_body(resp)
    );
    assert_eq!(
        appstate
            .store
            .kv
            .len(atomic_lib::db::trees::Tree::Blobs)
            .unwrap(),
        0,
        "nothing from a broken body is stored"
    );
}

/// Gets the body from the response as a String. Why doen't actix provide this?
/// Every visitor-facing form response must forbid HTTP caching — see
/// `handlers::form::NO_STORE`.
fn assert_cache_control_no_store(resp: &ServiceResponse, what: &str) {
    let cache_control = resp
        .headers()
        .get("Cache-Control")
        .unwrap_or_else(|| panic!("{what}: missing Cache-Control header"))
        .to_str()
        .unwrap();
    assert!(
        cache_control.contains("no-store"),
        "{what}: Cache-Control should contain no-store, got {cache_control}"
    );
}

/// Bytes served to a signed-in reader must stay out of shared caches, which
/// would hand them to the next visitor of the same URL.
fn assert_private_no_store(resp: &ServiceResponse, what: &str) {
    assert_eq!(
        resp.headers()
            .get(actix_web::http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("private, no-store"),
        "{what}: bytes served to a signed-in reader must not land in shared caches"
    );
}

fn get_body(resp: ServiceResponse) -> String {
    let boxbody = resp.into_body();
    let bytes = boxbody.try_into_bytes().unwrap();
    String::from_utf8(bytes.as_ref().into()).unwrap()
}

/// Content-addressed image URLs need no resource at `/files/<hash>`.
#[cfg(feature = "img")]
#[actix_rt::test]
async fn content_addressed_image_download() {
    content_addressed_image_with_storage(false).await;
}

#[cfg(feature = "img")]
#[actix_rt::test]
async fn remote_image_renditions_never_write_local_blobs() {
    content_addressed_image_with_storage(true).await;
}

#[cfg(feature = "img")]
async fn content_addressed_image_with_storage(remote: bool) {
    use clap::Parser;
    let dir = std::path::PathBuf::from(format!("./.temp/{}", atomic_lib::utils::random_string(10)));
    let opts = Opts::parse_from([
        "atomic-server",
        "--initialize",
        "--data-dir",
        dir.join("db").to_str().unwrap(),
        "--config-dir",
        dir.join("config").to_str().unwrap(),
    ]);
    let mut config = config::build_config(opts).unwrap();
    config.search_index_path = dir.join("search");
    config.vector_search_index_path = dir.join("vectors");
    let mut appstate = AppState::init(config).await.unwrap();
    if remote {
        appstate.store.blob_backend = Some(std::sync::Arc::new(
            crate::blob_storage::ObjectBlobBackend::new(
                std::sync::Arc::new(object_store::memory::InMemory::new()),
                "files",
            )
            .unwrap(),
        ));
    }
    let store = appstate.store.clone();
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        2,
        2,
        image::Rgba([40, 80, 120, 255]),
    ))
    .write_to(&mut png, image::ImageFormat::Png)
    .unwrap();
    let bytes = png.into_inner();
    let hash = blake3::hash(&bytes);
    appstate
        .store
        .put_blob(hash.as_bytes(), &bytes)
        .await
        .unwrap();
    let app = test::init_service(
        App::new()
            .app_data(Data::new(appstate))
            .configure(crate::routes::config_routes),
    )
    .await;
    let path = format!("/download/files/{}", hash.to_hex());
    let raw = test::call_service(&app, TestRequest::get().uri(&path).to_request()).await;
    assert_eq!(raw.status(), 200);
    assert_eq!(test::read_body(raw).await.as_ref(), bytes.as_slice());
    for (format, expected_format) in [
        ("webp", image::ImageFormat::WebP),
        ("avif", image::ImageFormat::Avif),
    ] {
        let resized = test::call_service(
            &app,
            TestRequest::get()
                .uri(&format!("{path}?f={format}&w=64&q=60"))
                .to_request(),
        )
        .await;
        assert_eq!(
            resized.status(),
            200,
            "resizing must use the existing blob, not a nonexistent File URL"
        );
        assert_eq!(
            resized.headers().get("content-type").unwrap(),
            format!("image/{format}").as_str()
        );
        assert_eq!(
            resized.headers().get("x-content-type-options").unwrap(),
            "nosniff"
        );
        assert!(resized
            .headers()
            .get("content-disposition")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("attachment"));
        let rendered = test::read_body(resized).await;
        assert_eq!(image::guess_format(&rendered).unwrap(), expected_format);
    }
    let missing = "0".repeat(64);
    for suffix in ["", "?f=webp&w=64&q=60"] {
        let response = test::call_service(
            &app,
            TestRequest::get()
                .uri(&format!("/download/files/{missing}{suffix}"))
                .to_request(),
        )
        .await;
        assert_eq!(
            response.status(),
            404,
            "missing blobs must not become HTTP 500"
        );
    }
    if remote {
        assert_eq!(store.kv.len(atomic_lib::db::trees::Tree::Blobs).unwrap(), 0);
    }
}

#[actix_rt::test]
async fn upload_download_test() {
    upload_download_with_backend(None).await;
}

#[actix_rt::test]
async fn remote_upload_download_never_stores_bytes_locally() {
    let backend = crate::blob_storage::ObjectBlobBackend::new(
        std::sync::Arc::new(object_store::memory::InMemory::new()),
        "files",
    )
    .unwrap();
    upload_download_with_backend(Some(std::sync::Arc::new(backend))).await;
}

async fn upload_download_with_backend(
    backend: Option<std::sync::Arc<dyn atomic_lib::db::blob_backend::BlobBackend>>,
) {
    let unique_string = atomic_lib::utils::random_string(10);
    use clap::Parser;
    let opts = Opts::parse_from([
        "atomic-server",
        "--initialize",
        "--data-dir",
        &format!("./.temp/{}/db", unique_string),
        "--config-dir",
        &format!("./.temp/{}/config", unique_string),
    ]);

    let mut config = config::build_config(opts).expect("failed init config");
    // Prevent folder access issues when running concurrent tests — the other
    // server tests set this; without it, parallel runs share the default
    // search-index dir and trip Tantivy's `LockBusy` on the second test.
    config.search_index_path = format!("./.temp/{}/search_index", unique_string).into();
    let mut appstate = crate::appstate::AppState::init(config.clone())
        .await
        .expect("failed init appstate");

    if backend.is_some() {
        appstate.store.blob_backend = backend;
    }
    let remote = appstate.store.blob_backend.is_some();
    let data = Data::new(appstate.clone());
    let app = test::init_service(
        App::new()
            .app_data(data)
            .configure(crate::routes::config_routes),
    )
    .await;

    // Create a valid parent drive
    let drive_did = atomic_lib::test_utils::create_test_drive(&appstate.store)
        .await
        .unwrap();

    let test_content = b"hello blake3 world";
    let expected_hash = blake3::hash(test_content).to_hex().to_string();

    // 1. Upload
    let multipart_boundary = "boundary";
    let body = format!(
        "--{multipart_boundary}\r\n\
        Content-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\n\
        Content-Type: text/plain\r\n\r\n\
        {}\r\n\
        --{multipart_boundary}--\r\n",
        String::from_utf8_lossy(test_content)
    );

    let req = build_request_authenticated(
        &format!("/upload?parent={}", urlencoding::encode(drive_did.as_str())),
        &appstate,
    )
    .method(actix_web::http::Method::POST)
    .insert_header((
        "Content-Type",
        format!("multipart/form-data; boundary={multipart_boundary}"),
    ))
    .set_payload(body)
    .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "Upload failed: {:?}",
        resp.status()
    );

    let body_str = get_body(resp);
    assert!(body_str.contains(&expected_hash));

    // 2. Verify in DB
    let hash_bytes = blake3::hash(test_content);
    let blob = appstate
        .store
        .get_blob(hash_bytes.as_bytes())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(blob, test_content);
    if remote {
        assert_eq!(
            appstate
                .store
                .kv
                .len(atomic_lib::db::trees::Tree::Blobs)
                .unwrap(),
            0
        );
    }

    // 3. Download
    let req = build_request_authenticated(&format!("/download/files/{}", expected_hash), &appstate)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // The content-addressed route only gets a hash, but it must still answer
    // with the File's real mimetype: the response carries `nosniff`, so an
    // `application/octet-stream` answer makes the browser refuse to render the
    // bytes in an `<img>` — and `downloadURL` for every client-uploaded file
    // points here.
    assert_eq!(
        resp.headers()
            .get(actix_web::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/plain"),
        "content-addressed download must serve the uploaded mimetype"
    );

    let downloaded_bytes = test::read_body(resp).await;
    assert_eq!(downloaded_bytes, test_content.as_slice());
}

/// Server state in fresh temp dirs, started with `extra` CLI flags.
async fn fresh_appstate(extra: &[&str]) -> AppState {
    use clap::Parser;
    let unique = atomic_lib::utils::random_string(10);
    let data_dir = format!("./.temp/{unique}/db");
    let config_dir = format!("./.temp/{unique}/config");
    let mut args = vec![
        "atomic-server",
        "--initialize",
        "--data-dir",
        &data_dir,
        "--config-dir",
        &config_dir,
    ];
    args.extend_from_slice(extra);
    let mut config = config::build_config(Opts::parse_from(args)).expect("failed init config");
    config.search_index_path = format!("./.temp/{unique}/search_index").into();
    config.vector_search_index_path = format!("./.temp/{unique}/vector_search_index").into();
    AppState::init(config).await.expect("failed init appstate")
}

/// The `atomic_session` cookie the data browser installs for `agent`
/// (`setCookieAuthentication`): a proof signed for the server's origin. A
/// browser attaches it to every same-origin `<img src>` it loads.
fn session_cookie(agent: &atomic_lib::agents::Agent, origin: &str) -> String {
    let timestamp = atomic_lib::utils::now();
    let signature = atomic_lib::agents::sign_message(
        format!("{origin} {timestamp}").as_bytes(),
        agent.private_key.as_ref().unwrap(),
    )
    .unwrap();
    let proof = serde_json::json!({
        "https://atomicdata.dev/properties/auth/agent": agent.subject.to_string(),
        "https://atomicdata.dev/properties/auth/requestedSubject": origin,
        "https://atomicdata.dev/properties/auth/publicKey": agent.public_key,
        "https://atomicdata.dev/properties/auth/timestamp": timestamp,
        "https://atomicdata.dev/properties/auth/signature": signature,
    });
    format!(
        "atomic_session={}",
        base64::engine::general_purpose::STANDARD.encode(proof.to_string())
    )
}

/// With `--require-blob-auth`, content-addressed blob URLs
/// (`/download/files/<hash>`, `/download/<blob DID>`) stop being bearer
/// capabilities: the bytes belong to the resources that reference the hash,
/// and are served to an agent who may read one of those, however the hash
/// became known. The data browser keeps working: its `<img src>` requests
/// carry the same-origin session cookie.
#[actix_rt::test]
async fn content_addressed_download_requires_read_on_a_referencing_resource() {
    let appstate = fresh_appstate(&["--require-blob-auth"]).await;
    let store = appstate.store.clone();
    let owner = store.get_default_agent().unwrap();
    let origin = appstate.config.get_origin();

    let bytes = b"private bytes behind a guessable url";
    let hash = blake3::hash(bytes);
    let hash_hex = hash.to_hex().to_string();
    store.put_blob(hash.as_bytes(), bytes).await.unwrap();

    // A File only its owner may read.
    let mut file = atomic_lib::Resource::new(format!(
        "{origin}/files/private-{}",
        atomic_lib::utils::random_string(8)
    ));
    file.set_unsafe(urls::IS_A.into(), vec![urls::FILE.to_string()].into())
        .unwrap();
    file.set_unsafe(urls::INTERNAL_ID.into(), hash_hex.clone().into())
        .unwrap();
    file.set_unsafe(urls::MIMETYPE.into(), "text/plain".to_string().into())
        .unwrap();
    let download_url = format!("{origin}/download/files/{hash_hex}");
    file.set_unsafe(urls::DOWNLOAD_URL.into(), download_url.clone().into())
        .unwrap();
    file.set_unsafe(urls::READ.into(), vec![owner.subject.to_string()].into())
        .unwrap();
    store.add_resource(&file).await.unwrap();

    let app = test::init_service(
        App::new()
            .app_data(Data::new(appstate.clone()))
            .configure(crate::routes::config_routes),
    )
    .await;
    let host = url::Url::parse(&origin).unwrap();
    let host = match host.port() {
        Some(port) => format!("{}:{port}", host.host_str().unwrap()),
        None => host.host_str().unwrap().to_string(),
    };

    let paths = [
        format!("/download/files/{hash_hex}"),
        format!("/download/did:ad:blob:{hash_hex}"),
        format!("/download/atomic:blob:{hash_hex}"),
    ];
    for path in &paths {
        let anonymous = test::call_service(
            &app,
            TestRequest::get()
                .uri(path)
                .insert_header(("Host", host.as_str()))
                .to_request(),
        )
        .await;
        assert_eq!(
            anonymous.status(),
            401,
            "{path}: a caller who may read no File with this hash gets no bytes"
        );

        // Signed headers: how an agent client asks.
        let signed = test::call_service(
            &app,
            build_request_authenticated(path, &appstate).to_request(),
        )
        .await;
        assert_eq!(
            signed.status(),
            200,
            "{path}: the File's reader gets the bytes"
        );
        assert_private_no_store(&signed, path);
        assert_eq!(test::read_body(signed).await.as_ref(), bytes);

        // The session cookie: how the data browser's `<img src>` asks.
        let with_cookie = test::call_service(
            &app,
            TestRequest::get()
                .uri(path)
                .insert_header(("Host", host.as_str()))
                .insert_header(("Cookie", session_cookie(&owner, &origin)))
                .to_request(),
        )
        .await;
        assert_eq!(
            with_cookie.status(),
            200,
            "{path}: a browser carrying the reader's session cookie gets the bytes"
        );
        assert_private_no_store(&with_cookie, path);
    }

    // The File by its own URL: the same bytes, the same caching rule.
    let file_path = format!(
        "/download{}",
        file.get_subject()
            .to_string()
            .strip_prefix(&origin)
            .unwrap()
    );
    let by_subject = test::call_service(
        &app,
        request_signed_by(
            actix_web::http::Method::GET,
            &file_path,
            &file.get_subject().to_string(),
            &owner,
            &origin,
        ),
    )
    .await;
    assert_eq!(by_subject.status(), 200, "{file_path}: the owner reads it");
    assert_private_no_store(&by_subject, &file_path);
    assert_eq!(test::read_body(by_subject).await.as_ref(), bytes);

    // Once a File with these bytes is public, so are the bytes.
    let mut public_copy = atomic_lib::Resource::new(format!(
        "{origin}/files/public-{}",
        atomic_lib::utils::random_string(8)
    ));
    public_copy
        .set_unsafe(urls::IS_A.into(), vec![urls::FILE.to_string()].into())
        .unwrap();
    public_copy
        .set_unsafe(urls::INTERNAL_ID.into(), hash_hex.clone().into())
        .unwrap();
    public_copy
        .set_unsafe(urls::DOWNLOAD_URL.into(), download_url.into())
        .unwrap();
    public_copy
        .set_unsafe(
            urls::READ.into(),
            vec![urls::PUBLIC_AGENT.to_string()].into(),
        )
        .unwrap();
    store.add_resource(&public_copy).await.unwrap();
    let anonymous = test::call_service(
        &app,
        TestRequest::get()
            .uri(&paths[0])
            .insert_header(("Host", host.as_str()))
            .to_request(),
    )
    .await;
    assert_eq!(anonymous.status(), 200);
}

/// The `Host` header value (`host[:port]`) of the server's origin.
fn origin_authority(origin: &str) -> String {
    let url = url::Url::parse(origin).unwrap();
    match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap()),
        None => url.host_str().unwrap().to_string(),
    }
}

/// A request for `path` signed by `agent` over `signed_for` (the URL or
/// subject the handler checks the signature against).
fn request_signed_by(
    method: actix_web::http::Method,
    path: &str,
    signed_for: &str,
    agent: &atomic_lib::agents::Agent,
    origin: &str,
) -> actix_http::Request {
    let mut req = TestRequest::default()
        .method(method)
        .uri(path)
        .insert_header(("Host", origin_authority(origin)));
    for header in atomic_lib::client::get_authentication_headers(signed_for, agent).unwrap() {
        req = req.insert_header(header);
    }
    req.to_request()
}

/// Commit options for a commit signed by `agent` arriving from outside: the
/// signature and the signer's rights are checked.
fn signed_commit_opts(agent: &atomic_lib::agents::Agent) -> atomic_lib::commit::CommitOpts {
    atomic_lib::commit::CommitOpts {
        validate_signature: true,
        validate_timestamp: false,
        validate_rights: true,
        validate_for_agent: Some(agent.subject.to_string()),
        update_index: true,
        ..atomic_lib::commit::CommitOpts::no_validations_no_index()
    }
}

/// A genesis by `signer` of a child of `parent` (or a drive when `None`),
/// carrying `props`.
async fn signed_genesis(
    store: &atomic_lib::Db,
    signer: &atomic_lib::agents::Agent,
    parent: Option<&str>,
    props: Vec<(&str, atomic_lib::Value)>,
) -> atomic_lib::errors::AtomicResult<String> {
    let mut builder = atomic_lib::commit::CommitBuilder::new("placeholder".into());
    match parent {
        Some(parent) => builder.set(
            urls::PARENT.into(),
            atomic_lib::Value::AtomicUrl(parent.into()),
        ),
        None => builder.set(
            urls::IS_A.into(),
            atomic_lib::Value::ResourceArray(vec![urls::DRIVE.to_string().into()]),
        ),
    }
    for (property, value) in props {
        builder.set(property.into(), value);
    }
    let commit = atomic_lib::Commit::create_did(builder, signer, store).await?;
    let response = store
        .apply_commit(commit, &signed_commit_opts(signer))
        .await?;
    Ok(response.resource_new.unwrap().get_subject().to_string())
}

/// With `--require-blob-auth`, the bytes behind a hash go to readers of a
/// resource that references it. A registered agent who learned the hash of a
/// private file must not get its bytes by minting a reference of their own:
/// not through `/download/files/<hash>` (GET or HEAD), not through the
/// chunked-file fallback for a whole-file hash, and not through
/// `/download/<their resource>`. Readers keep access through `internalId`,
/// `blob` and `chunks` references alike.
#[actix_rt::test]
async fn a_forged_hash_reference_does_not_unlock_blob_bytes() {
    let appstate = fresh_appstate(&["--require-blob-auth"]).await;
    let store = appstate.store.clone();
    let owner = store.get_default_agent().unwrap();
    let origin = appstate.config.get_origin();
    let stranger = store.create_agent(Some("Stranger")).await.unwrap();

    // The owner's private chunked file: two stored chunks; the whole-file
    // hash is never stored as a blob of its own.
    let (chunk_a, chunk_b) = (b"first private chunk ".as_slice(), b"second".as_slice());
    let mut whole = chunk_a.to_vec();
    whole.extend_from_slice(chunk_b);
    let whole_hash = blake3::hash(&whole).to_hex().to_string();
    let mut chunk_refs = Vec::new();
    for chunk in [chunk_a, chunk_b] {
        let hash = blake3::hash(chunk);
        store.put_blob(hash.as_bytes(), chunk).await.unwrap();
        chunk_refs.push(atomic_lib::identifiers::blob_subject(&hash.to_hex()));
    }
    let chunk_a_hash = blake3::hash(chunk_a).to_hex().to_string();
    let private_read = vec![owner.subject.to_string()];
    let mut chunked = atomic_lib::Resource::new(format!(
        "{origin}/files/chunked-{}",
        atomic_lib::utils::random_string(8)
    ));
    chunked
        .set_unsafe(urls::IS_A.into(), vec![urls::FILE.to_string()].into())
        .unwrap();
    chunked
        .set_unsafe(urls::INTERNAL_ID.into(), whole_hash.clone().into())
        .unwrap();
    chunked
        .set_unsafe(
            urls::DOWNLOAD_URL.into(),
            format!("{origin}/download/files/{whole_hash}").into(),
        )
        .unwrap();
    chunked
        .set_unsafe(
            urls::CHUNKS.into(),
            atomic_lib::Value::ResourceArray(
                chunk_refs.iter().map(|c| c.as_str().into()).collect(),
            ),
        )
        .unwrap();
    chunked
        .set_unsafe(urls::READ.into(), private_read.clone().into())
        .unwrap();
    store.add_resource(&chunked).await.unwrap();

    // A private resource naming its bytes through `blob`.
    let blob_bytes = b"bytes named through the blob property";
    let blob_hash = blake3::hash(blob_bytes);
    store
        .put_blob(blob_hash.as_bytes(), blob_bytes)
        .await
        .unwrap();
    let blob_hash = blob_hash.to_hex().to_string();
    let mut by_blob = atomic_lib::Resource::new(format!(
        "{origin}/files/by-blob-{}",
        atomic_lib::utils::random_string(8)
    ));
    by_blob
        .set_unsafe(
            urls::BLOB.into(),
            atomic_lib::Value::AtomicUrl(
                atomic_lib::identifiers::blob_subject(&blob_hash)
                    .as_str()
                    .into(),
            ),
        )
        .unwrap();
    by_blob
        .set_unsafe(urls::READ.into(), private_read.into())
        .unwrap();
    store.add_resource(&by_blob).await.unwrap();

    // The stranger's own drive, where they may write anything.
    let stranger_drive = signed_genesis(&store, &stranger, None, vec![])
        .await
        .expect("anyone may mint a drive of their own");

    let app = test::init_service(
        App::new()
            .app_data(Data::new(appstate.clone()))
            .configure(crate::routes::config_routes),
    )
    .await;
    let get = actix_web::http::Method::GET;
    let head = actix_web::http::Method::HEAD;
    let as_agent =
        |method: &actix_web::http::Method, path: &str, agent: &atomic_lib::agents::Agent| {
            request_signed_by(
                method.clone(),
                path,
                &format!("{origin}{path}"),
                agent,
                &origin,
            )
        };

    // Readers get the bytes through each kind of reference.
    for (path, expected) in [
        (format!("/download/files/{whole_hash}"), whole.as_slice()),
        (format!("/download/files/{chunk_a_hash}"), chunk_a),
        (
            format!("/download/files/{blob_hash}"),
            blob_bytes.as_slice(),
        ),
    ] {
        let resp = test::call_service(&app, as_agent(&get, &path, &owner)).await;
        assert_eq!(resp.status(), 200, "{path}: the owner reads their bytes");
        assert_eq!(test::read_body(resp).await.as_ref(), expected, "{path}");
        let resp = test::call_service(&app, as_agent(&get, &path, &stranger)).await;
        assert_eq!(
            resp.status(),
            401,
            "{path}: a stranger who may read no reference gets nothing"
        );
    }

    // References to bytes held here, by `chunks` or `blob`, are refused.
    for (property, value) in [
        (
            urls::CHUNKS,
            atomic_lib::Value::ResourceArray(vec![chunk_refs[0].as_str().into()]),
        ),
        (
            urls::BLOB,
            atomic_lib::Value::AtomicUrl(
                atomic_lib::identifiers::blob_subject(&blob_hash)
                    .as_str()
                    .into(),
            ),
        ),
    ] {
        let refused = signed_genesis(
            &store,
            &stranger,
            Some(&stranger_drive),
            vec![(property, value)],
        )
        .await;
        assert!(
            refused.is_err(),
            "{property}: referencing held bytes the stranger may not read is refused"
        );
    }

    // `/download/<resource>` serves what the stranger's own resource names:
    // a blob the stranger referenced before uploading it works...
    let own_bytes = b"the stranger's own upload";
    let own_hash = blake3::hash(own_bytes);
    let own = signed_genesis(
        &store,
        &stranger,
        Some(&stranger_drive),
        vec![(
            urls::INTERNAL_ID,
            atomic_lib::Value::String(own_hash.to_hex().to_string()),
        )],
    )
    .await
    .expect("referencing bytes this node does not hold yet is the upload order");
    store
        .put_blob(own_hash.as_bytes(), own_bytes)
        .await
        .unwrap();
    let own_path = format!("/download/{own}");
    let resp = test::call_service(
        &app,
        request_signed_by(get.clone(), &own_path, &own, &stranger, &origin),
    )
    .await;
    assert_eq!(resp.status(), 200, "the stranger downloads their own file");
    assert_eq!(test::read_body(resp).await.as_ref(), own_bytes);

    // ...but a resource naming only the private file's whole-file hash, which
    // is never stored as a blob, yields nothing: not by its subject, and not
    // through the chunked-file fallback of `/download/files/<hash>`.
    let forged = signed_genesis(
        &store,
        &stranger,
        Some(&stranger_drive),
        vec![(
            urls::INTERNAL_ID,
            atomic_lib::Value::String(whole_hash.clone()),
        )],
    )
    .await
    .expect("no bytes are held under the whole-file hash, so the reference is allowed");
    let forged_path = format!("/download/{forged}");
    let resp = test::call_service(
        &app,
        request_signed_by(get.clone(), &forged_path, &forged, &stranger, &origin),
    )
    .await;
    assert_ne!(resp.status(), 200, "{forged_path} must not serve the file");
    let whole_path = format!("/download/files/{whole_hash}");
    for method in [&get, &head] {
        let resp = test::call_service(&app, as_agent(method, &whole_path, &stranger)).await;
        assert_ne!(
            resp.status(),
            200,
            "{method} {whole_path}: the chunked fallback may only use a File the caller can read"
        );
        assert_ne!(
            test::read_body(resp).await.as_ref(),
            whole.as_slice(),
            "{method} {whole_path}"
        );
    }
    let resp = test::call_service(&app, as_agent(&get, &whole_path, &owner)).await;
    assert_eq!(resp.status(), 200, "the owner still gets the chunked file");
}

/// `POST /upload?parent=<parent>` of one file, signed by `agent`.
fn upload_request(
    agent: &atomic_lib::agents::Agent,
    parent: &str,
    filename: &str,
    bytes: &[u8],
    origin: &str,
) -> actix_http::Request {
    let path = format!("/upload?parent={}", urlencoding::encode(parent));
    let body = [
        format!(
            "--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
        bytes,
        b"\r\n--boundary--\r\n",
    ]
    .concat();
    let mut req = TestRequest::post()
        .uri(&path)
        .insert_header(("Host", origin_authority(origin)))
        .insert_header(("Content-Type", "multipart/form-data; boundary=boundary"))
        .set_payload(body);
    for header in
        atomic_lib::client::get_authentication_headers(&format!("{origin}{path}"), agent).unwrap()
    {
        req = req.insert_header(header);
    }
    req.to_request()
}

/// Every resource whose `internalId` is `hash_hex`.
async fn files_with_internal_id(
    store: &atomic_lib::Db,
    hash_hex: &str,
) -> Vec<atomic_lib::Resource> {
    store
        .query(&atomic_lib::storelike::Query::new_prop_val(
            urls::INTERNAL_ID,
            hash_hex,
        ))
        .await
        .unwrap()
        .resources
}

/// Two users who upload identical bytes each get a File of their own. The
/// bytes are stored once, but the second upload neither replaces the first
/// File nor moves it into the second uploader's drive; both keep the same
/// content-addressed download URL.
#[actix_rt::test]
async fn identical_uploads_by_different_users_keep_separate_files() {
    let appstate = fresh_appstate(&["--require-blob-auth"]).await;
    let store = appstate.store.clone();
    let origin = appstate.config.get_origin();
    let alice = store.create_agent(Some("Alice")).await.unwrap();
    let bob = store.create_agent(Some("Bob")).await.unwrap();
    let outsider = store.create_agent(Some("Outsider")).await.unwrap();
    let alice_drive = signed_genesis(&store, &alice, None, vec![]).await.unwrap();
    let bob_drive = signed_genesis(&store, &bob, None, vec![]).await.unwrap();
    let app = test::init_service(
        App::new()
            .app_data(Data::new(appstate.clone()))
            .configure(crate::routes::config_routes),
    )
    .await;

    let bytes = b"identical bytes uploaded by two people";
    let hash_hex = blake3::hash(bytes).to_hex().to_string();

    let resp = test::call_service(
        &app,
        upload_request(&alice, &alice_drive, "same.txt", bytes, &origin),
    )
    .await;
    assert_eq!(resp.status(), 200, "alice's upload: {}", get_body(resp));
    let alice_files = files_with_internal_id(&store, &hash_hex).await;
    assert_eq!(alice_files.len(), 1, "alice's upload made one File");
    let alice_file = alice_files[0].get_subject().clone();

    let resp = test::call_service(
        &app,
        upload_request(&bob, &bob_drive, "same.txt", bytes, &origin),
    )
    .await;
    assert_eq!(resp.status(), 200, "bob's upload: {}", get_body(resp));

    let files = files_with_internal_id(&store, &hash_hex).await;
    assert_eq!(
        files.len(),
        2,
        "each upload is a File of its own: {:?}",
        files
            .iter()
            .map(|f| f.get_subject().to_string())
            .collect::<Vec<_>>()
    );
    let first = files
        .iter()
        .find(|f| f.get_subject() == &alice_file)
        .expect("alice's File still exists under its own subject");
    assert_eq!(
        first.get(urls::PARENT).unwrap().to_string(),
        alice_drive,
        "bob's upload must not move alice's File into bob's drive"
    );
    let second = files
        .iter()
        .find(|f| f.get_subject() != &alice_file)
        .unwrap();
    assert_eq!(second.get(urls::PARENT).unwrap().to_string(), bob_drive);
    for file in &files {
        assert_eq!(
            file.get(urls::DOWNLOAD_URL).unwrap().to_string(),
            format!("{origin}/download/files/{hash_hex}"),
            "the download URL stays content-addressed"
        );
    }
    assert_eq!(
        store.kv.len(atomic_lib::db::trees::Tree::Blobs).unwrap(),
        1,
        "the bytes are stored once"
    );

    let path = format!("/download/files/{hash_hex}");
    let download = |agent: &atomic_lib::agents::Agent| {
        request_signed_by(
            actix_web::http::Method::GET,
            &path,
            &format!("{origin}{path}"),
            agent,
            &origin,
        )
    };
    for uploader in [&alice, &bob] {
        let resp = test::call_service(&app, download(uploader)).await;
        assert_eq!(resp.status(), 200, "each uploader reads their bytes");
        assert_eq!(test::read_body(resp).await.as_ref(), bytes);
    }
    let resp = test::call_service(&app, download(&outsider)).await;
    assert_eq!(resp.status(), 401, "a reader of neither drive gets nothing");
}

/// Two agents with drives of their own and image Files that share a claimed
/// hash: alice's private picture is chunked (so its whole-file hash is never
/// stored as a blob and anyone may reference it), and mallory's File claims
/// that hash next to a chunk of her own, created through a signed commit like
/// any client's.
#[cfg(feature = "img")]
struct RenditionFixture {
    appstate: AppState,
    origin: String,
    alice: atomic_lib::agents::Agent,
    mallory: atomic_lib::agents::Agent,
    mallory_drive: String,
    mallory_file: String,
    red: Vec<u8>,
    green: Vec<u8>,
    hash_hex: String,
}

#[cfg(feature = "img")]
impl RenditionFixture {
    async fn new() -> Self {
        let appstate = fresh_appstate(&["--require-blob-auth"]).await;
        let store = appstate.store.clone();
        let origin = appstate.config.get_origin();
        let alice = store.create_agent(Some("Alice")).await.unwrap();
        let mallory = store.create_agent(Some("Mallory")).await.unwrap();
        let alice_drive = signed_genesis(&store, &alice, None, vec![]).await.unwrap();
        let mallory_drive = signed_genesis(&store, &mallory, None, vec![])
            .await
            .unwrap();
        let blob_did = |bytes: &[u8]| atomic_lib::identifiers::blob_subject(&blob_hex(bytes));

        let red = solid_png([200, 10, 10, 255]);
        let hash_hex = blob_hex(&red);
        let (red_a, red_b) = red.split_at(red.len() / 2);
        signed_genesis(
            &store,
            &alice,
            Some(&alice_drive),
            vec![
                (
                    urls::INTERNAL_ID,
                    atomic_lib::Value::String(hash_hex.clone()),
                ),
                (
                    urls::MIMETYPE,
                    atomic_lib::Value::String("image/png".into()),
                ),
                (
                    urls::CHUNKS,
                    atomic_lib::Value::ResourceArray(vec![
                        blob_did(red_a).as_str().into(),
                        blob_did(red_b).as_str().into(),
                    ]),
                ),
            ],
        )
        .await
        .unwrap();
        for chunk in [red_a, red_b] {
            store
                .put_blob(blake3::hash(chunk).as_bytes(), chunk)
                .await
                .unwrap();
        }

        let green = solid_png([10, 200, 10, 255]);
        let mallory_file = signed_genesis(
            &store,
            &mallory,
            Some(&mallory_drive),
            vec![
                (
                    urls::INTERNAL_ID,
                    atomic_lib::Value::String(hash_hex.clone()),
                ),
                (
                    urls::CHUNKS,
                    atomic_lib::Value::ResourceArray(vec![blob_did(&green).as_str().into()]),
                ),
            ],
        )
        .await
        .expect("referencing a hash whose bytes are not held is allowed");
        store
            .put_blob(blake3::hash(&green).as_bytes(), &green)
            .await
            .unwrap();

        Self {
            appstate,
            origin,
            alice,
            mallory,
            mallory_drive,
            mallory_file,
            red,
            green,
            hash_hex,
        }
    }

    /// Alice asks for a webp rendition through `/download/files/<hash>`.
    fn alice_rendition(&self, w: u32, q: u32) -> actix_http::Request {
        let path = format!("/download/files/{}?f=webp&w={w}&q={q}", self.hash_hex);
        request_signed_by(
            actix_web::http::Method::GET,
            &path,
            &format!("{}{path}", self.origin),
            &self.alice,
            &self.origin,
        )
    }

    /// Mallory asks for a webp rendition of her File by its subject.
    fn mallory_rendition(&self, w: u32, q: u32) -> actix_http::Request {
        request_signed_by(
            actix_web::http::Method::GET,
            &format!("/download/{}?f=webp&w={w}&q={q}", self.mallory_file),
            &self.mallory_file,
            &self.mallory,
            &self.origin,
        )
    }
}

/// An 8x8 PNG of one colour.
#[cfg(feature = "img")]
fn solid_png(rgba: [u8; 4]) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(8, 8, image::Rgba(rgba)))
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

/// The webp rendition the server makes of `bytes` (parameters already on
/// its grid).
#[cfg(feature = "img")]
fn webp_rendition_of(bytes: &[u8], w: u32, q: u32) -> Vec<u8> {
    crate::handlers::image::process_image_bytes(
        bytes,
        &crate::handlers::download::DownloadParams {
            q: Some(q as f32),
            w: Some(w),
            f: Some("webp".into()),
        },
        "webp",
    )
    .unwrap()
}

/// C2: an image rendition is cached under the bytes it was made from. A File
/// that claims another user's whole-file hash next to chunks of its own
/// neither poisons nor reads that user's cached renditions.
#[cfg(feature = "img")]
#[actix_rt::test]
async fn renditions_follow_the_bytes_actually_served() {
    let fx = RenditionFixture::new().await;
    let app = test::init_service(
        App::new()
            .app_data(Data::new(fx.appstate.clone()))
            .configure(crate::routes::config_routes),
    )
    .await;
    assert_ne!(
        webp_rendition_of(&fx.red, 64, 60),
        webp_rendition_of(&fx.green, 64, 60)
    );

    // Poisoning: mallory renders first, alice must still get her own picture.
    let resp = test::call_service(&app, fx.mallory_rendition(64, 60)).await;
    assert_eq!(resp.status(), 200, "mallory renders her own File");
    assert_eq!(
        test::read_body(resp).await,
        webp_rendition_of(&fx.green, 64, 60)
    );
    let resp = test::call_service(&app, fx.alice_rendition(64, 60)).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        test::read_body(resp).await,
        webp_rendition_of(&fx.red, 64, 60),
        "alice's thumbnail was poisoned by a File claiming her hash"
    );

    // Reading: alice renders first, mallory must not get alice's picture.
    let resp = test::call_service(&app, fx.alice_rendition(128, 70)).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        test::read_body(resp).await,
        webp_rendition_of(&fx.red, 128, 70)
    );
    let resp = test::call_service(&app, fx.mallory_rendition(128, 70)).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        test::read_body(resp).await,
        webp_rendition_of(&fx.green, 128, 70),
        "mallory read alice's cached thumbnail through a File claiming her hash"
    );
}

/// C2: a rendition cache key computable from a public hash, referenced before
/// the rendition exists, must not hand out the rendition once it is made:
/// neither through `/download/files/<key>` nor through the referencing File.
#[cfg(feature = "img")]
#[actix_rt::test]
async fn a_rendition_cache_key_cannot_be_referenced_before_it_is_rendered() {
    let fx = RenditionFixture::new().await;
    let store = fx.appstate.store.clone();
    let app = test::init_service(
        App::new()
            .app_data(Data::new(fx.appstate.clone()))
            .configure(crate::routes::config_routes),
    )
    .await;

    // The 32-byte key renditions used to be cached under, from the hash
    // alone.
    let old_key =
        blake3::hash(format!("processed|hash={}|f=webp|q=50|w=256", fx.hash_hex).as_bytes())
            .to_hex()
            .to_string();
    let key_file = signed_genesis(
        &store,
        &fx.mallory,
        Some(&fx.mallory_drive),
        vec![(
            urls::INTERNAL_ID,
            atomic_lib::Value::String(old_key.clone()),
        )],
    )
    .await
    .expect("nothing is held under the key yet");

    let resp = test::call_service(&app, fx.alice_rendition(256, 50)).await;
    assert_eq!(resp.status(), 200);
    let alice_thumbnail = test::read_body(resp).await;
    assert_eq!(alice_thumbnail, webp_rendition_of(&fx.red, 256, 50));

    let key_path = format!("/download/files/{old_key}");
    let resp = test::call_service(
        &app,
        request_signed_by(
            actix_web::http::Method::GET,
            &key_path,
            &format!("{}{key_path}", fx.origin),
            &fx.mallory,
            &fx.origin,
        ),
    )
    .await;
    assert_eq!(resp.status(), 404, "{key_path} reached a cached rendition");
    let key_file_path = format!("/download/{key_file}");
    let resp = test::call_service(
        &app,
        request_signed_by(
            actix_web::http::Method::GET,
            &key_file_path,
            &key_file,
            &fx.mallory,
            &fx.origin,
        ),
    )
    .await;
    assert_ne!(
        resp.status(),
        200,
        "{key_file_path} reached a cached rendition"
    );
    assert_ne!(test::read_body(resp).await, alice_thumbnail);
}

/// M1: `/download/files/<hash>` answers only with bytes that hash to it. A
/// public chunked File claiming alice's whole-file hash next to chunks of its
/// own may be readable by everyone, but neither its bytes nor its mimetype
/// are served under her content address.
#[cfg(feature = "img")]
#[actix_rt::test]
async fn a_spoofed_chunked_file_cannot_serve_other_bytes_under_a_hash() {
    let fx = RenditionFixture::new().await;
    let store = fx.appstate.store.clone();
    let green_did = atomic_lib::identifiers::blob_subject(&blob_hex(&fx.green));
    // Several, so that some sort before alice's File whatever the query order.
    for i in 0..4 {
        signed_genesis(
            &store,
            &fx.mallory,
            Some(&fx.mallory_drive),
            vec![
                (urls::NAME, atomic_lib::Value::String(format!("spoof {i}"))),
                (
                    urls::INTERNAL_ID,
                    atomic_lib::Value::String(fx.hash_hex.clone()),
                ),
                (
                    urls::MIMETYPE,
                    atomic_lib::Value::String("application/octet-stream".into()),
                ),
                (
                    urls::CHUNKS,
                    atomic_lib::Value::ResourceArray(vec![green_did.as_str().into()]),
                ),
                (
                    urls::READ,
                    atomic_lib::Value::ResourceArray(vec![urls::PUBLIC_AGENT.into()]),
                ),
            ],
        )
        .await
        .expect("mallory may publish a File of bytes she can read");
    }
    let app = test::init_service(
        App::new()
            .app_data(Data::new(fx.appstate.clone()))
            .configure(crate::routes::config_routes),
    )
    .await;

    // Everyone may read the spoofs; only alice may read her File.
    let path = format!("/download/files/{}", fx.hash_hex);
    for path in [path.clone(), format!("{path}?f=webp&w=64&q=60")] {
        let resp = test::call_service(
            &app,
            TestRequest::get()
                .uri(&path)
                .insert_header(("Host", origin_authority(&fx.origin)))
                .to_request(),
        )
        .await;
        assert_eq!(
            resp.status(),
            404,
            "{path}: a File whose bytes do not hash to the address must not answer for it"
        );
        let body = test::read_body(resp).await;
        assert_ne!(body, fx.green, "{path}");
        assert_ne!(body, webp_rendition_of(&fx.green, 64, 60), "{path}");
    }

    let resp = test::call_service(
        &app,
        request_signed_by(
            actix_web::http::Method::GET,
            &path,
            &format!("{}{path}", fx.origin),
            &fx.alice,
            &fx.origin,
        ),
    )
    .await;
    assert_eq!(resp.status(), 200, "alice reads her picture");
    assert_eq!(
        resp.headers()
            .get(actix_web::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("image/png"),
        "the mimetype comes from the File whose bytes are served"
    );
    assert_eq!(test::read_body(resp).await, fx.red);
    let resp = test::call_service(&app, fx.alice_rendition(64, 60)).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        test::read_body(resp).await,
        webp_rendition_of(&fx.red, 64, 60)
    );
}

/// M1: the mimetype served for a stored blob comes only from a File the
/// reader may read whose bytes are that blob. Under `nosniff` a wrong type
/// breaks the picture for its readers, so neither a File the reader may not
/// read nor one naming the hash next to chunks of other bytes chooses it.
#[actix_rt::test]
async fn a_blobs_mimetype_comes_from_a_readable_file_of_its_bytes() {
    let appstate = fresh_appstate(&["--require-blob-auth"]).await;
    let store = appstate.store.clone();
    let origin = appstate.config.get_origin();
    let alice = store.create_agent(Some("Alice")).await.unwrap();
    let mallory = store.create_agent(Some("Mallory")).await.unwrap();
    let bob = store.create_agent(Some("Bob")).await.unwrap();
    let alice_drive = signed_genesis(&store, &alice, None, vec![]).await.unwrap();
    let mallory_drive = signed_genesis(&store, &mallory, None, vec![])
        .await
        .unwrap();
    let public_read = || atomic_lib::Value::ResourceArray(vec![urls::PUBLIC_AGENT.into()]);
    let internal_id = |hash: &str| (urls::INTERNAL_ID, atomic_lib::Value::String(hash.into()));
    let mimetype = |m: &str| (urls::MIMETYPE, atomic_lib::Value::String(m.into()));

    let bytes = b"alice's picture, stored whole".as_slice();
    let hash_hex = blake3::hash(bytes).to_hex().to_string();
    let other = b"mallory's own bytes".as_slice();
    let other_hash = blake3::hash(other);

    // Mallory names the hash before it is uploaded, as the upload order
    // allows: privately, and publicly next to chunks of other bytes.
    signed_genesis(
        &store,
        &mallory,
        Some(&mallory_drive),
        vec![internal_id(&hash_hex), mimetype("text/html")],
    )
    .await
    .unwrap();
    signed_genesis(
        &store,
        &mallory,
        Some(&mallory_drive),
        vec![
            internal_id(&hash_hex),
            mimetype("application/x-spoof"),
            (
                urls::CHUNKS,
                atomic_lib::Value::ResourceArray(vec![atomic_lib::identifiers::blob_subject(
                    &other_hash.to_hex(),
                )
                .as_str()
                .into()]),
            ),
            (urls::READ, public_read()),
        ],
    )
    .await
    .unwrap();
    // Alice's File with the real type, readable by her, and a public File of
    // the same bytes with none.
    signed_genesis(
        &store,
        &alice,
        Some(&alice_drive),
        vec![internal_id(&hash_hex), mimetype("image/png")],
    )
    .await
    .unwrap();
    signed_genesis(
        &store,
        &alice,
        Some(&alice_drive),
        vec![internal_id(&hash_hex), (urls::READ, public_read())],
    )
    .await
    .unwrap();
    store.put_blob(other_hash.as_bytes(), other).await.unwrap();
    store
        .put_blob(blake3::hash(bytes).as_bytes(), bytes)
        .await
        .unwrap();

    let app = test::init_service(
        App::new()
            .app_data(Data::new(appstate.clone()))
            .configure(crate::routes::config_routes),
    )
    .await;
    for path in [
        format!("/download/files/{hash_hex}"),
        format!("/download/did:ad:blob:{hash_hex}"),
    ] {
        for (agent, expected) in [
            (&alice, "image/png"),
            // Bob reads the public copy, which has no type, and the spoof.
            (&bob, "application/octet-stream"),
        ] {
            let resp = test::call_service(
                &app,
                request_signed_by(
                    actix_web::http::Method::GET,
                    &path,
                    &format!("{origin}{path}"),
                    agent,
                    &origin,
                ),
            )
            .await;
            assert_eq!(resp.status(), 200, "{path} as {}", agent.subject);
            assert_eq!(
                resp.headers()
                    .get(actix_web::http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
                Some(expected),
                "{path} as {}: the mimetype of a readable File of these bytes",
                agent.subject
            );
            assert_eq!(test::read_body(resp).await.as_ref(), bytes);
        }
    }
}

/// N1: a chunked File is rebuilt only up to the size it declares. Anyone may
/// claim a whole-file hash next to chunks of their own, so without a bound a
/// File listing one huge chunk many times makes every request for it, or for
/// the hash it claims, assemble that many bytes. A File within its declared
/// size is still served by its subject and its content address.
#[actix_rt::test]
async fn a_chunked_file_is_not_rebuilt_past_its_declared_size() {
    let appstate = fresh_appstate(&[]).await;
    let store = appstate.store.clone();
    let origin = appstate.config.get_origin();
    let alice = store.create_agent(Some("Alice")).await.unwrap();
    let drive = signed_genesis(&store, &alice, None, vec![]).await.unwrap();
    let app = test::init_service(
        App::new()
            .app_data(Data::new(appstate.clone()))
            .configure(crate::routes::config_routes),
    )
    .await;

    let mut failures = Vec::new();
    for (case, filler, declared_extra, served) in
        [("oversized", 1u8, -1i64, false), ("within", 2u8, 0, true)]
    {
        let chunk_a = vec![filler; 64];
        let chunk_b = vec![filler.wrapping_add(10); 64];
        let whole = [chunk_a.as_slice(), chunk_b.as_slice()].concat();
        let whole_hash = blake3::hash(&whole).to_hex().to_string();
        let chunk_ref =
            |bytes: &[u8]| atomic_lib::identifiers::blob_subject(&blake3::hash(bytes).to_hex());
        let file = signed_genesis(
            &store,
            &alice,
            Some(&drive),
            vec![
                (
                    urls::INTERNAL_ID,
                    atomic_lib::Value::String(whole_hash.clone()),
                ),
                (
                    urls::CHUNKS,
                    atomic_lib::Value::ResourceArray(vec![
                        chunk_ref(&chunk_a).as_str().into(),
                        chunk_ref(&chunk_b).as_str().into(),
                    ]),
                ),
                (
                    urls::FILESIZE,
                    atomic_lib::Value::Integer(whole.len() as i64 + declared_extra),
                ),
                (
                    urls::READ,
                    atomic_lib::Value::ResourceArray(vec![urls::PUBLIC_AGENT.into()]),
                ),
            ],
        )
        .await
        .unwrap();
        for chunk in [&chunk_a, &chunk_b] {
            store
                .put_blob(blake3::hash(chunk).as_bytes(), chunk)
                .await
                .unwrap();
        }

        for path in [
            format!("/download/{file}"),
            format!("/download/files/{whole_hash}"),
        ] {
            let resp = test::call_service(
                &app,
                TestRequest::get()
                    .uri(&path)
                    .insert_header(("Host", origin_authority(&origin)))
                    .to_request(),
            )
            .await;
            let status = resp.status();
            let got_whole = test::read_body(resp).await.as_ref() == whole.as_slice();
            if served != (status == 200 && got_whole) {
                failures.push(format!(
                    "{case} {path}: status {status}, served the rebuilt bytes: {got_whole}"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// N1: the mimetype of a stored blob comes from a File whose own bytes are
/// that blob. A chunked File's bytes are its chunks, so naming the blob's hash
/// as its `internalId` does not count, and finding out whether its chunks
/// happen to hash to it would mean rebuilding them on every request.
#[actix_rt::test]
async fn a_stored_blobs_mimetype_is_not_taken_from_a_chunked_file() {
    let appstate = fresh_appstate(&[]).await;
    let store = appstate.store.clone();
    let origin = appstate.config.get_origin();
    let alice = store.create_agent(Some("Alice")).await.unwrap();
    let drive = signed_genesis(&store, &alice, None, vec![]).await.unwrap();

    let whole = b"bytes stored whole and as chunks".as_slice();
    let (chunk_a, chunk_b) = whole.split_at(whole.len() / 2);
    let whole_hash = blake3::hash(whole).to_hex().to_string();
    let chunk_ref =
        |bytes: &[u8]| atomic_lib::identifiers::blob_subject(&blake3::hash(bytes).to_hex());
    signed_genesis(
        &store,
        &alice,
        Some(&drive),
        vec![
            (
                urls::INTERNAL_ID,
                atomic_lib::Value::String(whole_hash.clone()),
            ),
            (
                urls::MIMETYPE,
                atomic_lib::Value::String("image/png".into()),
            ),
            (
                urls::CHUNKS,
                atomic_lib::Value::ResourceArray(vec![
                    chunk_ref(chunk_a).as_str().into(),
                    chunk_ref(chunk_b).as_str().into(),
                ]),
            ),
        ],
    )
    .await
    .unwrap();
    for bytes in [whole, chunk_a, chunk_b] {
        store
            .put_blob(blake3::hash(bytes).as_bytes(), bytes)
            .await
            .unwrap();
    }

    let app = test::init_service(
        App::new()
            .app_data(Data::new(appstate.clone()))
            .configure(crate::routes::config_routes),
    )
    .await;
    let path = format!("/download/files/{whole_hash}");
    let resp = test::call_service(
        &app,
        request_signed_by(
            actix_web::http::Method::GET,
            &path,
            &format!("{origin}{path}"),
            &alice,
            &origin,
        ),
    )
    .await;
    assert_eq!(resp.status(), 200, "the stored blob is served");
    assert_eq!(
        resp.headers()
            .get(actix_web::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/octet-stream"),
        "no File whose own bytes are the blob names a type"
    );
    assert_eq!(test::read_body(resp).await.as_ref(), whole);
}

/// N2: a published form serves its images to anonymous visitors through the
/// server's own agent, so the form's drive is not enough of a boundary: an
/// agent who may only append to a drive could publish a form there whose
/// cover is a File of that drive they cannot read themselves. The form's
/// verified creator must be able to read what it serves.
#[actix_rt::test]
async fn a_form_does_not_serve_a_file_its_creator_cannot_read() {
    let appstate = fresh_appstate(&[]).await;
    let store = appstate.store.clone();
    let alice = store.create_agent(Some("Alice")).await.unwrap();
    let mallory = store.create_agent(Some("Mallory")).await.unwrap();
    let drive = signed_genesis(
        &store,
        &alice,
        None,
        vec![(
            urls::APPEND,
            atomic_lib::Value::ResourceArray(vec![mallory.subject.to_string().into()]),
        )],
    )
    .await
    .unwrap();

    let bytes = b"alice's private picture".as_slice();
    let hash_hex = blake3::hash(bytes).to_hex().to_string();
    let file = signed_genesis(
        &store,
        &alice,
        Some(&drive),
        vec![
            (
                urls::IS_A,
                atomic_lib::Value::ResourceArray(vec![urls::FILE.into()]),
            ),
            (urls::INTERNAL_ID, atomic_lib::Value::String(hash_hex)),
            (
                urls::MIMETYPE,
                atomic_lib::Value::String("image/png".into()),
            ),
        ],
    )
    .await
    .unwrap();
    store
        .put_blob(blake3::hash(bytes).as_bytes(), bytes)
        .await
        .unwrap();
    let file_resource = store.get_resource(&file.as_str().into()).await.unwrap();
    assert!(
        atomic_lib::hierarchy::check_read(&store, &file_resource, &ForAgent::from(&mallory))
            .await
            .is_err(),
        "fixture: Mallory may append to the drive but not read the File"
    );

    let form_props = || {
        vec![
            (
                urls::IS_A,
                atomic_lib::Value::ResourceArray(vec![urls::FORM.into()]),
            ),
            (urls::NAME, atomic_lib::Value::String("Survey".into())),
            (
                urls::COVER_IMAGE,
                atomic_lib::Value::AtomicUrl(file.as_str().into()),
            ),
            (
                urls::FORM_PUBLISHED_AT,
                atomic_lib::Value::Timestamp(atomic_lib::utils::now()),
            ),
        ]
    };
    let mallorys_form = signed_genesis(&store, &mallory, Some(&drive), form_props())
        .await
        .expect("append on the drive lets Mallory create a form in it");
    let alices_form = signed_genesis(&store, &alice, Some(&drive), form_props())
        .await
        .unwrap();

    let app = test::init_service(
        App::new()
            .app_data(Data::new(appstate.clone()))
            .configure(crate::routes::config_routes),
    )
    .await;
    let mut failures = Vec::new();
    for (form, expected) in [(&mallorys_form, 404), (&alices_form, 200)] {
        let resp = test::call_service(
            &app,
            TestRequest::get()
                .uri(&format!("/form/{form}/image"))
                .to_request(),
        )
        .await;
        let status = resp.status().as_u16();
        let body = test::read_body(resp).await;
        if status != expected {
            failures.push(format!("{form}: status {status}, expected {expected}"));
        }
        if expected == 404 && body.as_ref() == bytes {
            failures.push(format!("{form}: served the private File"));
        }
        if expected == 200 && body.as_ref() != bytes {
            failures.push(format!("{form}: the owner's form lost its cover image"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Lowercase hex BLAKE3 hash of `bytes`.
#[cfg(feature = "img")]
fn blob_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// `--served-domain-suffix` (`ATOMIC_SERVED_DOMAIN_SUFFIX`) lets a node answer
/// under a second hostname. A GET signed for a URL on that hostname must
/// authenticate there: the request origin follows the `Host` the client used,
/// so the signature's subject matches. A `Host` outside every configured name
/// is not trusted, so the very same signature is refused.
#[actix_rt::test]
async fn signed_get_on_a_served_domain_suffix_host_is_accepted() {
    let appstate = fresh_appstate(&[
        "--domain",
        "atomic.example",
        "--served-domain-suffix",
        "second.example",
    ])
    .await;
    let agent = appstate.store.get_default_agent().unwrap();
    let drive = appstate.store.ensure_private_drive().await.unwrap();
    let app = test::init_service(
        App::new()
            .app_data(Data::new(appstate.clone()))
            .configure(crate::routes::config_routes),
    )
    .await;

    let path = format!("/{drive}");
    let signed_for = format!("http://kb.second.example{path}");
    let get = |host: &'static str, signed: bool| {
        let mut req = TestRequest::get()
            .uri(&path)
            .insert_header(("Host", host))
            .insert_header(("Accept", "application/ad+json"));
        if signed {
            for header in
                atomic_lib::client::get_authentication_headers(&signed_for, &agent).unwrap()
            {
                req = req.insert_header(header);
            }
        }
        req.to_request()
    };

    let anonymous = test::call_service(&app, get("kb.second.example", false)).await;
    assert_eq!(anonymous.status(), 401, "the private drive is not public");

    let on_suffix_host = test::call_service(&app, get("kb.second.example", true)).await;
    assert_eq!(
        on_suffix_host.status(),
        200,
        "a GET signed for the served-suffix host authenticates there: {}",
        get_body(on_suffix_host)
    );

    let on_unserved_host = test::call_service(&app, get("kb.unserved.example", true)).await;
    assert_eq!(
        on_unserved_host.status(),
        401,
        "an unserved Host must not decide which origin a signature is checked against"
    );
}

/// `GET /drive-usage` reports a drive's resource count + blob/Loro bytes for the
/// sync page. The frontend has shipped this UI for a while, but the endpoint was
/// never implemented server-side (it 404'd), so the usage bar silently never
/// appeared — this test guards against that regressing again.
#[actix_rt::test]
async fn drive_usage_endpoint() {
    let unique_string = atomic_lib::utils::random_string(10);
    use clap::Parser;
    let opts = Opts::parse_from([
        "atomic-server",
        "--initialize",
        "--data-dir",
        &format!("./.temp/{}/db", unique_string),
        "--config-dir",
        &format!("./.temp/{}/config", unique_string),
    ]);

    let mut config = config::build_config(opts).expect("failed init config");
    config.search_index_path = format!("./.temp/{}/search_index", unique_string).into();
    let appstate = crate::appstate::AppState::init(config.clone())
        .await
        .expect("failed init appstate");

    let data = Data::new(appstate.clone());
    let app = test::init_service(
        App::new()
            .app_data(data)
            .configure(crate::routes::config_routes),
    )
    .await;

    let drive_did = atomic_lib::test_utils::create_test_drive(&appstate.store)
        .await
        .unwrap();

    // Upload a file so the drive has a resource with a blob to account for.
    let test_content = b"hello blake3 world";
    let multipart_boundary = "boundary";
    let body = format!(
        "--{multipart_boundary}\r\n\
        Content-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\n\
        Content-Type: text/plain\r\n\r\n\
        {}\r\n\
        --{multipart_boundary}--\r\n",
        String::from_utf8_lossy(test_content)
    );
    let req = build_request_authenticated(
        &format!("/upload?parent={}", urlencoding::encode(drive_did.as_str())),
        &appstate,
    )
    .method(actix_web::http::Method::POST)
    .insert_header((
        "Content-Type",
        format!("multipart/form-data; boundary={multipart_boundary}"),
    ))
    .set_payload(body)
    .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "upload failed: {:?}",
        resp.status()
    );

    // Now the endpoint the sync page calls should report real numbers.
    let req = build_request_authenticated(
        &format!(
            "/drive-usage?subject={}",
            urlencoding::encode(drive_did.as_str())
        ),
        &appstate,
    )
    .to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body = get_body(resp);
    assert!(status.is_success(), "drive-usage status {status}: {body}");

    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    // camelCase field names — the shape `fetchNodeDriveUsage` reads.
    assert!(
        json["resourceCount"].as_u64().unwrap() >= 1,
        "expected at least the uploaded file counted: {body}"
    );
    assert_eq!(
        json["blobBytes"].as_u64().unwrap(),
        test_content.len() as u64,
        "blobBytes should equal the uploaded content length: {body}"
    );
    assert!(json.get("loroBytes").is_some(), "loroBytes missing: {body}");

    // The per-resource breakdown must add up to the drive totals, with the
    // uploaded file's bytes attributed to exactly one resource.
    let req = build_request_authenticated(
        &format!(
            "/drive-usage/breakdown?subject={}",
            urlencoding::encode(drive_did.as_str())
        ),
        &appstate,
    )
    .to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body = get_body(resp);
    assert!(status.is_success(), "breakdown status {status}: {body}");
    let breakdown: serde_json::Value = serde_json::from_str(&body).unwrap();
    let rows = breakdown["resources"].as_array().unwrap();
    let blob_total: u64 = rows.iter().map(|r| r["blobBytes"].as_u64().unwrap()).sum();
    let loro_total: u64 = rows.iter().map(|r| r["loroBytes"].as_u64().unwrap()).sum();
    assert_eq!(blob_total, test_content.len() as u64, "{body}");
    assert_eq!(rows.len() as u64, json["resourceCount"].as_u64().unwrap());
    assert_eq!(loro_total, json["loroBytes"].as_u64().unwrap());
}

/// `GET /server` describes the node itself as a `Server` resource, replacing the
/// bespoke `/node-info` and `/iroh-node-id` JSON shapes. It must be real JSON-AD
/// with an `isA` of Server, so any Atomic client can read it, not just our own
/// data-browser.
#[actix_rt::test]
async fn server_info_endpoint() {
    let unique_string = atomic_lib::utils::random_string(10);
    use clap::Parser;
    let opts = Opts::parse_from([
        "atomic-server",
        "--initialize",
        "--data-dir",
        &format!("./.temp/{}/db", unique_string),
        "--config-dir",
        &format!("./.temp/{}/config", unique_string),
    ]);

    let mut config = config::build_config(opts).expect("failed init config");
    config.search_index_path = format!("./.temp/{}/search_index", unique_string).into();
    let appstate = crate::appstate::AppState::init(config.clone())
        .await
        .expect("failed init appstate");

    let data = Data::new(appstate.clone());
    let app = test::init_service(
        App::new()
            .app_data(data)
            .configure(crate::routes::config_routes),
    )
    .await;

    // A node seeded before the `Server` properties existed still has to be able
    // to say what it is. Dropping one of the Property resources stands in for
    // such a store: rendering must not depend on the ontology being present,
    // or every existing deployment answers 500 until an operator repopulates.
    appstate
        .store
        .remove_resource(&urls::SERVER_VERSION.into())
        .await
        .expect("could not remove property");

    let req = build_request_authenticated("/server", &appstate).to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body = get_body(resp);
    assert!(status.is_success(), "/server status {status}: {body}");

    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        json[urls::IS_A][0].as_str(),
        Some(urls::SERVER),
        "/server should be typed as a Server: {body}"
    );
    assert_eq!(
        json[urls::SERVER_VERSION].as_str(),
        Some(env!("CARGO_PKG_VERSION")),
        "version should be this build's version: {body}"
    );
    // An unmanaged (self-hosted) node reports managed:false and omits the portal.
    assert_eq!(json[urls::SERVER_MANAGED].as_bool(), Some(false), "{body}");
    assert!(
        json.get(urls::SERVER_PORTAL_URL).is_none(),
        "portalUrl should be absent on a self-hosted node: {body}"
    );
}

/// With `ATOMIC_HOME_DRIVE` set, `/server` carries the drive as `homeDrive`.
/// The property was used by the endpoint before it existed in the default
/// store, so every node with a home drive answered 500 on `/server` — and the
/// data-browser then took its own origin for "not a node": no known server,
/// no place to register a private drive on sign-in.
#[actix_rt::test]
async fn server_info_endpoint_with_home_drive() {
    let unique_string = atomic_lib::utils::random_string(10);
    use clap::Parser;
    let opts = Opts::parse_from([
        "atomic-server",
        "--initialize",
        "--data-dir",
        &format!("./.temp/{}/db", unique_string),
        "--config-dir",
        &format!("./.temp/{}/config", unique_string),
        "--home-drive",
        "http://localhost/",
    ]);

    let mut config = config::build_config(opts).expect("failed init config");
    config.search_index_path = format!("./.temp/{}/search_index", unique_string).into();
    let appstate = crate::appstate::AppState::init(config.clone())
        .await
        .expect("failed init appstate");

    let data = Data::new(appstate.clone());
    let app = test::init_service(
        App::new()
            .app_data(data)
            .configure(crate::routes::config_routes),
    )
    .await;

    // Plain JSON (not JSON-AD) needs every property's datatype to render, so
    // this is the representation that failed: the JSON-AD one got away with
    // the property missing from the store.
    let req = build_request_authenticated("/server", &appstate)
        .insert_header(("Accept", "application/json"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body = get_body(resp);
    assert!(status.is_success(), "/server status {status}: {body}");

    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    // Plain JSON keys by shortname.
    assert_eq!(
        json["home-drive"].as_str(),
        Some("http://localhost/"),
        "homeDrive should be the configured drive: {body}"
    );
}

/// Phase 3 of `planning/atomic-forms.md`: builds a Form + FormPage + FormField
/// graph pointing at a Table/Class pair (mirroring what the Phase 2
/// data-browser builder produces), then drives the two new HTTP endpoints
/// end to end: publish gating, slug minting + resolution, a valid
/// submission landing as a table row, and the required-field / honeypot /
/// unpublished rejection paths.
#[actix_rt::test]
async fn form_submission_flow() {
    use atomic_lib::{Resource, Value};

    let unique_string = atomic_lib::utils::random_string(10);
    use clap::Parser;
    let opts = Opts::parse_from([
        "atomic-server",
        "--initialize",
        "--data-dir",
        &format!("./.temp/{}/db", unique_string),
        "--config-dir",
        &format!("./.temp/{}/config", unique_string),
    ]);

    let mut config = config::build_config(opts).expect("failed init config");
    config.search_index_path = format!("./.temp/{}/search_index", unique_string).into();
    let appstate = crate::appstate::AppState::init(config.clone())
        .await
        .expect("failed init appstate");

    let data = Data::new(appstate.clone());
    let app = test::init_service(
        App::new()
            .app_data(data)
            .configure(crate::routes::config_routes),
    )
    .await;
    let store = &appstate.store;

    // Class + Property + Table (mirrors what NewFormDialog/useFormFieldPropertySync build client-side)
    let mut class = Resource::new_instance(urls::CLASS, store).await.unwrap();
    class
        .set(
            urls::SHORTNAME.into(),
            Value::Slug("submission".into()),
            store,
        )
        .await
        .unwrap();
    class
        .set(
            urls::DESCRIPTION.into(),
            Value::Markdown("A form submission row".into()),
            store,
        )
        .await
        .unwrap();
    class.save_locally(store).await.unwrap();

    let mut email_prop = Resource::new_instance(urls::PROPERTY, store).await.unwrap();
    email_prop
        .set(urls::SHORTNAME.into(), Value::Slug("email".into()), store)
        .await
        .unwrap();
    email_prop
        .set(
            urls::DESCRIPTION.into(),
            Value::Markdown("Respondent email".into()),
            store,
        )
        .await
        .unwrap();
    email_prop
        .set(
            urls::DATATYPE_PROP.into(),
            Value::AtomicUrl(urls::STRING.into()),
            store,
        )
        .await
        .unwrap();
    email_prop.save_locally(store).await.unwrap();

    let mut table = Resource::new_instance(urls::TABLE, store).await.unwrap();
    table
        .set(
            urls::NAME.into(),
            Value::String("Submissions".into()),
            store,
        )
        .await
        .unwrap();
    table
        .set(
            urls::CLASSTYPE_PROP.into(),
            Value::AtomicUrl(class.get_subject().to_string().into()),
            store,
        )
        .await
        .unwrap();
    table.save_locally(store).await.unwrap();

    // FormField -> FormPage -> Form
    let mut field = Resource::new_instance(urls::FORM_FIELD, store)
        .await
        .unwrap();
    field
        .set(urls::NAME.into(), Value::String("Email".into()), store)
        .await
        .unwrap();
    field
        .set(
            urls::FORM_MAPS_TO.into(),
            Value::AtomicUrl(email_prop.get_subject().to_string().into()),
            store,
        )
        .await
        .unwrap();
    field
        .set(
            urls::FORM_FIELD_TYPE.into(),
            Value::String("email".into()),
            store,
        )
        .await
        .unwrap();
    field
        .set(urls::REQUIRED.into(), Value::Boolean(true), store)
        .await
        .unwrap();
    field.save_locally(store).await.unwrap();

    let mut page = Resource::new_instance(urls::FORM_PAGE, store)
        .await
        .unwrap();
    page.set(
        urls::FORM_FIELDS.into(),
        Value::ResourceArray(vec![field.get_subject().to_string().into()]),
        store,
    )
    .await
    .unwrap();
    page.save_locally(store).await.unwrap();

    let mut form = Resource::new_instance(urls::FORM, store).await.unwrap();
    form.set(urls::NAME.into(), Value::String("Feedback".into()), store)
        .await
        .unwrap();
    form.set(
        urls::FORM_DATA_CLASS.into(),
        Value::AtomicUrl(class.get_subject().to_string().into()),
        store,
    )
    .await
    .unwrap();
    form.set(
        urls::FORM_TARGET_TABLE.into(),
        Value::AtomicUrl(table.get_subject().to_string().into()),
        store,
    )
    .await
    .unwrap();
    form.set(
        urls::FORM_PAGES.into(),
        Value::ResourceArray(vec![page.get_subject().to_string().into()]),
        store,
    )
    .await
    .unwrap();
    // Filed under its results table, like "Create form from this table" does,
    // so the form and the table share a drive (see `forms::FormScope`).
    form.set(
        urls::PARENT.into(),
        Value::AtomicUrl(table.get_subject().to_string().into()),
        store,
    )
    .await
    .unwrap();
    // DID (genesis) subject — matches how forms are actually created by the
    // data-browser client, and exercises the slug bootstrap fallback below.
    form.save_as_genesis(store).await.unwrap();
    let form_did_id = form.get_subject().pure_id();

    // 1. Unpublished -> 410
    let req = test::TestRequest::get()
        .uri(&format!("/form/{}/definition", form_did_id))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 410, "unpublished form should 410");
    // A 410 is cacheable by default. Chromium replayed a cached "not
    // accepting responses" for the definition of a form that had since been
    // published, so every visitor-facing answer must forbid caching.
    assert_cache_control_no_store(&resp, "410 definition");

    // 1b. The unpublished HTML page (`not_available_page`) still allows
    // embedding — Phase 6 "Embedding": a stale snippet should show the
    // friendly closed-form card inside the iframe, not a browser-blocked
    // blank frame.
    let req = test::TestRequest::get()
        .uri(&format!("/form/{}", form_did_id))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.headers().get("Content-Security-Policy").unwrap(),
        "frame-ancestors *",
        "unpublished form page should allow embedding"
    );

    // 2. Publish, GET by DID -> 200, slug gets minted
    form.set(
        urls::FORM_PUBLISHED_AT.into(),
        Value::Timestamp(atomic_lib::utils::now()),
        store,
    )
    .await
    .unwrap();
    form.save_locally(store).await.unwrap();

    let req = test::TestRequest::get()
        .uri(&format!("/form/{}/definition", form_did_id))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "definition fetch after publish failed: {:?}",
        resp.status()
    );
    assert_cache_control_no_store(&resp, "200 definition");
    let body: serde_json::Value = serde_json::from_str(&get_body(resp)).unwrap();
    let slug = body["id"]
        .as_str()
        .expect("slug should be minted")
        .to_string();
    assert!(!slug.is_empty());

    // 2b. A GET must serve the *persisted* Loro state. The Form class
    // extender adds `form-submission-summary` to every fetched Form; it used
    // to do so through `set`, which also recorded a Loro op on the doc the
    // response re-exported as `loroUpdate`. A client that seeded its doc from
    // that response built every later delta on an op this store never
    // persisted, and `apply_commit` parked them ("Commit's Loro update
    // depends on ops the server does not have") — in the builder, Publish
    // after a reload, and Unpublish → Publish, stopped reaching visitors.
    let served = store
        .get_resource_extended(form.get_subject(), false, &ForAgent::Sudo)
        .await
        .unwrap()
        .to_single();
    assert!(
        served.get(urls::FORM_SUBMISSION_SUMMARY).is_ok(),
        "the extender should still shape the response"
    );
    let served_json: serde_json::Value =
        serde_json::from_str(&served.to_json_ad(None).unwrap()).unwrap();
    let served_snapshot = base64::engine::general_purpose::STANDARD
        .decode(served_json[urls::LORO_UPDATE].as_str().unwrap())
        .unwrap();
    let persisted_doc = store
        .get_resource(form.get_subject())
        .await
        .unwrap()
        .build_state_doc()
        .unwrap();
    let persisted_vv = persisted_doc.oplog_vv_map();
    persisted_doc.import_update(&served_snapshot).unwrap();
    assert_eq!(
        persisted_vv,
        persisted_doc.oplog_vv_map(),
        "the served loroUpdate carried Loro ops the store has not persisted"
    );
    assert_eq!(
        body["pages"][0]["blocks"][0]["mapsTo"],
        email_prop.get_subject().to_string()
    );
    assert_eq!(
        body["captcha"]["challengeUrl"],
        format!("/form/{slug}/challenge"),
        "definition should carry the captcha client config"
    );

    // 3. GET by the minted slug -> same definition
    let req = test::TestRequest::get()
        .uri(&format!("/form/{}/definition", slug))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "definition fetch by slug failed"
    );

    // 3b. Phase 6 "Embedding": the published HTML page allows framing from
    // any origin (forms have no auth boundary once published — same trust
    // level as the direct share link).
    let req = test::TestRequest::get()
        .uri(&format!("/form/{}", slug))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let csp = resp
        .headers()
        .get("Content-Security-Policy")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        csp.contains("frame-ancestors *"),
        "published form page should allow embedding: {csp}"
    );

    // 3c. Captcha: fetch a challenge and solve it natively (difficulty is
    // lowered under cfg(test) — see `crate::captcha`), mirroring what the
    // ALTCHA widget does in the visitor's browser.
    macro_rules! solve_captcha {
        () => {{
            let req = test::TestRequest::get()
                .uri(&format!("/form/{}/challenge", slug))
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert!(resp.status().is_success(), "challenge fetch failed");
            let challenge: altcha::Challenge =
                serde_json::from_str(&get_body(resp)).expect("challenge should parse");
            let solution =
                altcha::solve_challenge(altcha::SolveChallengeOptions::new(&challenge))
                    .unwrap()
                    .expect("challenge should be solvable");
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(
                serde_json::to_vec(&serde_json::json!({
                    "challenge": challenge,
                    "solution": solution,
                }))
                .unwrap(),
            )
        }};
    }

    // 3d. Picture-choice option images: the definition rewrites File subjects
    // into publish-gated `/form/{id}/image?file=` URLs (the visitor has no
    // agent, so `/download` is unreachable), and that route only serves images
    // this form actually references — otherwise it would be an open proxy for
    // anything the server agent can read.
    let mut picture_prop = Resource::new_instance(urls::PROPERTY, store).await.unwrap();
    picture_prop
        .set(urls::SHORTNAME.into(), Value::Slug("pick".into()), store)
        .await
        .unwrap();
    picture_prop
        .set(
            urls::DESCRIPTION.into(),
            Value::Markdown("Picture choice".into()),
            store,
        )
        .await
        .unwrap();
    picture_prop
        .set(
            urls::DATATYPE_PROP.into(),
            Value::AtomicUrl(urls::RESOURCE_ARRAY.into()),
            store,
        )
        .await
        .unwrap();
    picture_prop
        .set(
            urls::CLASSTYPE_PROP.into(),
            Value::AtomicUrl(urls::TAG.into()),
            store,
        )
        .await
        .unwrap();
    picture_prop.save_locally(store).await.unwrap();

    // Options are Tags on the property's `allowsOnly`; a picture-choice
    // option's image is the Tag's `cover-image`.
    // A real File in the form's drive: the definition only keeps images it
    // can see the form may show (`forms::FormScope`).
    let mut cat_file = Resource::new_instance(urls::FILE, store).await.unwrap();
    cat_file
        .set(
            urls::DOWNLOAD_URL.into(),
            Value::String("https://example.com/files/cat".into()),
            store,
        )
        .await
        .unwrap();
    cat_file
        .set(
            urls::PARENT.into(),
            Value::AtomicUrl(table.get_subject().to_string().into()),
            store,
        )
        .await
        .unwrap();
    cat_file.save_locally(store).await.unwrap();
    let cat_file_subject = cat_file.get_subject().to_string();
    let referenced_image = cat_file_subject.as_str();
    let mut tag_subjects = Vec::new();
    for (name, image) in [("Cat", Some(referenced_image)), ("Dog", None)] {
        let mut tag = Resource::new_instance(urls::TAG, store).await.unwrap();
        tag.set(urls::NAME.into(), Value::String(name.into()), store)
            .await
            .unwrap();
        tag.set(
            urls::SHORTNAME.into(),
            Value::Slug(name.to_lowercase()),
            store,
        )
        .await
        .unwrap();
        if let Some(image) = image {
            tag.set(
                urls::COVER_IMAGE.into(),
                Value::AtomicUrl(image.into()),
                store,
            )
            .await
            .unwrap();
        }
        tag.set(
            urls::PARENT.into(),
            Value::AtomicUrl(picture_prop.get_subject().to_string().into()),
            store,
        )
        .await
        .unwrap();
        tag.save_locally(store).await.unwrap();
        tag_subjects.push(tag.get_subject().to_string());
    }
    picture_prop
        .set(
            urls::ALLOWS_ONLY.into(),
            Value::ResourceArray(tag_subjects.iter().cloned().map(Into::into).collect()),
            store,
        )
        .await
        .unwrap();
    picture_prop.save_locally(store).await.unwrap();
    let mut picture_field = Resource::new_instance(urls::FORM_FIELD, store)
        .await
        .unwrap();
    picture_field
        .set(urls::NAME.into(), Value::String("Pick one".into()), store)
        .await
        .unwrap();
    picture_field
        .set(
            urls::FORM_MAPS_TO.into(),
            Value::AtomicUrl(picture_prop.get_subject().to_string().into()),
            store,
        )
        .await
        .unwrap();
    picture_field
        .set(
            urls::FORM_FIELD_TYPE.into(),
            Value::String("picture-choice".into()),
            store,
        )
        .await
        .unwrap();
    picture_field.save_locally(store).await.unwrap();

    page.set(
        urls::FORM_FIELDS.into(),
        Value::ResourceArray(vec![
            field.get_subject().to_string().into(),
            picture_field.get_subject().to_string().into(),
        ]),
        store,
    )
    .await
    .unwrap();
    page.save_locally(store).await.unwrap();

    let req = test::TestRequest::get()
        .uri(&format!("/form/{}/definition", slug))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let body: serde_json::Value = serde_json::from_str(&get_body(resp)).unwrap();
    assert_eq!(
        body["pages"][0]["blocks"][1]["options"]["options"],
        serde_json::json!([
            {
                "value": tag_subjects[0],
                "label": "Cat",
                "image": format!(
                    "/form/{}/image?file={}",
                    slug,
                    urlencoding::encode(referenced_image)
                ),
            },
            { "value": tag_subjects[1], "label": "Dog" },
        ]),
        "tags resolve into inline options, with image subjects rewritten into gated URLs"
    );

    let req = test::TestRequest::get()
        .uri(&format!(
            "/form/{}/image?file={}",
            slug,
            urlencoding::encode("https://example.com/files/not-referenced")
        ))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        404,
        "the image route must not serve files the form doesn't reference"
    );

    // 4. Valid submission (with solved captcha) -> 201, row lands under the table
    let captcha_payload = solve_captcha!();
    let submit_body = serde_json::json!({
        "values": { email_prop.get_subject().to_string(): "visitor@example.com" },
        "altcha": captcha_payload,
    });
    let req = test::TestRequest::post()
        .uri(&format!("/form/{}/submit", slug))
        .set_json(&submit_body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        201,
        "valid submission should succeed: {}",
        get_body(resp)
    );

    let query =
        atomic_lib::storelike::Query::new_prop_val(urls::PARENT, table.get_subject().as_str());
    // Rows only: the form and its image File are filed under the table too.
    let not_rows = [form.get_subject().to_string(), cat_file_subject.clone()];
    let count_rows = |result: &atomic_lib::storelike::QueryResult| {
        result
            .subjects
            .iter()
            .filter(|s| !not_rows.contains(&s.to_string()))
            .count()
    };
    let result = store.query(&query).await.unwrap();
    assert_eq!(
        count_rows(&result),
        1,
        "submission row should exist under the table"
    );

    // 4b. Missing captcha payload -> 400
    let req = test::TestRequest::post()
        .uri(&format!("/form/{}/submit", slug))
        .set_json(serde_json::json!({
            "values": { email_prop.get_subject().to_string(): "visitor2@example.com" }
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        400,
        "captcha-less submission should be rejected"
    );

    // 4c. Replayed captcha payload (already consumed by step 4) -> 400
    let req = test::TestRequest::post()
        .uri(&format!("/form/{}/submit", slug))
        .set_json(serde_json::json!({
            "values": { email_prop.get_subject().to_string(): "visitor2@example.com" },
            "altcha": captcha_payload,
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400, "replayed captcha should be rejected");

    // 5. Missing required field -> 400 with a field error (fresh captcha —
    // field validation runs after captcha verification)
    let req = test::TestRequest::post()
        .uri(&format!("/form/{}/submit", slug))
        .set_json(serde_json::json!({ "values": {}, "altcha": solve_captcha!() }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = serde_json::from_str(&get_body(resp)).unwrap();
    assert!(body["errors"][0]["message"].as_str().is_some());

    // 6. Honeypot filled -> 400 (checked before the captcha, so no payload needed)
    let req = test::TestRequest::post()
        .uri(&format!("/form/{}/submit", slug))
        .set_json(serde_json::json!({
            "values": { email_prop.get_subject().to_string(): "bot@example.com" },
            "hp": "i-am-a-bot",
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        400,
        "honeypot-filled submission should be rejected"
    );

    // Only the one valid submission from step 4 should have landed.
    let result = store.query(&query).await.unwrap();
    assert_eq!(count_rows(&result), 1);

    // 7. Unpublish -> submit now 410
    form.remove_propval(urls::FORM_PUBLISHED_AT).unwrap();
    form.save_locally(store).await.unwrap();

    let req = test::TestRequest::post()
        .uri(&format!("/form/{}/submit", slug))
        .set_json(&submit_body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 410, "submit to unpublished form should 410");

    // 7b. Scheduling (Phase 7): a published form still obeys its
    // `form-open-at` / `form-close-at` window, on every visitor-facing
    // route. Republish first — step 7 left it unpublished.
    form.set(
        urls::FORM_PUBLISHED_AT.into(),
        Value::Timestamp(atomic_lib::utils::now()),
        store,
    )
    .await
    .unwrap();

    let hour = 3_600_000;
    let opens_at = atomic_lib::utils::now() + hour;
    form.set(urls::FORM_OPEN_AT.into(), Value::Timestamp(opens_at), store)
        .await
        .unwrap();
    form.save_locally(store).await.unwrap();

    let req = test::TestRequest::get()
        .uri(&format!("/form/{}/definition", slug))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        410,
        "form scheduled to open later should 410"
    );
    let body = get_body(resp);
    assert!(
        body.contains("isn't open yet"),
        "a not-yet-open form needs its own wording, got: {body}"
    );
    // The message spells the moment out in UTC (a request carries no
    // timezone), and rides the raw moment along so the visitor's browser can
    // restate it locally — `form-app`'s `localizeMoment`.
    assert!(
        body.contains("momentMs") && body.contains("momentUtc"),
        "a scheduled 410 must carry the moment for client-side localization, got: {body}"
    );

    // The HTML page localizes it itself: the UTC text sits in a `<time>` the
    // inline script rewrites in the visitor's timezone.
    let req = test::TestRequest::get()
        .uri(&format!("/form/{}", slug))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 410, "not-yet-open form page should 410");
    let page = get_body(resp);
    assert!(
        page.contains("<time datetime=") && page.contains("data-ms="),
        "the not-available page must mark the moment up for localization, got: {page}"
    );
    assert!(
        page.contains("Intl.DateTimeFormat"),
        "the not-available page must ship the localization script, got: {page}"
    );

    let req = test::TestRequest::post()
        .uri(&format!("/form/{}/submit", slug))
        .set_json(&submit_body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 410, "submit before open-at should 410");

    // Open-at in the past -> open again.
    form.set(
        urls::FORM_OPEN_AT.into(),
        Value::Timestamp(atomic_lib::utils::now() - hour),
        store,
    )
    .await
    .unwrap();
    form.save_locally(store).await.unwrap();

    let req = test::TestRequest::get()
        .uri(&format!("/form/{}/definition", slug))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "form past its open-at should be reachable: {:?}",
        resp.status()
    );

    // A close-at in the past shuts it again, with closed-specific wording.
    let closed_at = atomic_lib::utils::now() - 1;
    form.set(
        urls::FORM_CLOSE_AT.into(),
        Value::Timestamp(closed_at),
        store,
    )
    .await
    .unwrap();
    form.save_locally(store).await.unwrap();

    let req = test::TestRequest::get()
        .uri(&format!("/form/{}/definition", slug))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 410, "form past its close-at should 410");
    let body = get_body(resp);
    assert!(
        body.contains("closed"),
        "a closed form needs its own wording, got: {body}"
    );

    // The HTML page renders the friendly card (not a blank frame) and stays
    // embeddable, same as the unpublished case in step 1b.
    let req = test::TestRequest::get()
        .uri(&format!("/form/{}", slug))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 410, "closed form page should 410");
    assert_eq!(
        resp.headers().get("Content-Security-Policy").unwrap(),
        "frame-ancestors *",
        "closed form page should allow embedding"
    );

    let req = test::TestRequest::post()
        .uri(&format!("/form/{}/submit", slug))
        .set_json(&submit_body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 410, "submit after close-at should 410");

    // Clear the schedule so the invite-code steps below run against a
    // plainly-open form.
    form.remove_propval(urls::FORM_OPEN_AT).unwrap();
    form.remove_propval(urls::FORM_CLOSE_AT).unwrap();
    form.save_locally(store).await.unwrap();

    // 8. Private links (Phase 6): republish and switch to invite-only.
    form.set(
        urls::FORM_PUBLISHED_AT.into(),
        Value::Timestamp(atomic_lib::utils::now()),
        store,
    )
    .await
    .unwrap();
    form.set(
        urls::FORM_ACCESS.into(),
        Value::String("invite-only".into()),
        store,
    )
    .await
    .unwrap();
    form.save_locally(store).await.unwrap();

    // Definition without / with an unknown code -> 403 (the questions must
    // not leak to someone holding only the share URL).
    let req = test::TestRequest::get()
        .uri(&format!("/form/{}/definition", slug))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        403,
        "invite-only definition without code should 403"
    );

    let req = test::TestRequest::get()
        .uri(&format!("/form/{}/definition?code=wrong", slug))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        403,
        "invite-only definition with unknown code should 403"
    );

    // The HTML page is gated the same way (the definition is injected inline).
    let req = test::TestRequest::get()
        .uri(&format!("/form/{}", slug))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        403,
        "invite-only form page without code should 403"
    );

    // Mint an invite code, child of the form (as the builder UI does).
    let mut invite = Resource::new_instance(urls::FORM_INVITE_CODE, store)
        .await
        .unwrap();
    invite
        .set(
            urls::PARENT.into(),
            Value::AtomicUrl(form.get_subject().to_string().into()),
            store,
        )
        .await
        .unwrap();
    invite
        .set(
            urls::FORM_CODE.into(),
            Value::String("secret-code".into()),
            store,
        )
        .await
        .unwrap();
    invite.save_locally(store).await.unwrap();

    // Definition with the code -> 200, and fetching does NOT consume it.
    for _ in 0..2 {
        let req = test::TestRequest::get()
            .uri(&format!("/form/{}/definition?code=secret-code", slug))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(
            resp.status().is_success(),
            "invite-only definition with valid code should succeed: {:?}",
            resp.status()
        );
    }

    // Submit without a code -> 403 (pre-check runs before captcha
    // verification, so no solved payload is needed).
    let req = test::TestRequest::post()
        .uri(&format!("/form/{}/submit", slug))
        .set_json(serde_json::json!({
            "values": { email_prop.get_subject().to_string(): "visitor3@example.com" }
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        403,
        "invite-only submit without code should 403"
    );

    // Submit with the code -> 201, and the code is now consumed.
    let req = test::TestRequest::post()
        .uri(&format!("/form/{}/submit", slug))
        .set_json(serde_json::json!({
            "values": { email_prop.get_subject().to_string(): "invited@example.com" },
            "altcha": solve_captcha!(),
            "code": "secret-code",
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        201,
        "invite-only submit with valid code should succeed: {}",
        get_body(resp)
    );
    let result = store.query(&query).await.unwrap();
    assert_eq!(
        count_rows(&result),
        2,
        "invited submission should land in the table"
    );
    let invite = store
        .get_resource(&invite.get_subject().clone())
        .await
        .unwrap();
    assert!(
        invite.get(urls::USED_AT).is_ok(),
        "the invite code should be marked used after the submission"
    );

    // Replaying the consumed code -> 403 on both submit and definition.
    let req = test::TestRequest::post()
        .uri(&format!("/form/{}/submit", slug))
        .set_json(serde_json::json!({
            "values": { email_prop.get_subject().to_string(): "sneaky@example.com" },
            "code": "secret-code",
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 403, "used code should be rejected at submit");

    let req = test::TestRequest::get()
        .uri(&format!("/form/{}/definition?code=secret-code", slug))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        403,
        "used code should be rejected at definition"
    );

    // No row landed for the rejected replay.
    let result = store.query(&query).await.unwrap();
    assert_eq!(count_rows(&result), 2);

    // Switching back to public opens the plain link again.
    form.set(
        urls::FORM_ACCESS.into(),
        Value::String("public".into()),
        store,
    )
    .await
    .unwrap();
    form.save_locally(store).await.unwrap();

    let req = test::TestRequest::get()
        .uri(&format!("/form/{}/definition", slug))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "public definition should work again after switching back"
    );

    // Pointing the form at a table in another drive must not let visitors
    // write there: the server, not the form's editor, would sign the row.
    let mut foreign_table = Resource::new_instance(urls::TABLE, store).await.unwrap();
    foreign_table
        .set(urls::NAME.into(), Value::String("Not yours".into()), store)
        .await
        .unwrap();
    foreign_table
        .set(
            urls::CLASSTYPE_PROP.into(),
            Value::AtomicUrl(class.get_subject().to_string().into()),
            store,
        )
        .await
        .unwrap();
    foreign_table
        .set_unsafe(
            urls::DRIVE_PROP.into(),
            Value::AtomicUrl("did:ad:someone-elses-drive".into()),
        )
        .unwrap();
    foreign_table.save_locally(store).await.unwrap();
    form.set(
        urls::FORM_TARGET_TABLE.into(),
        Value::AtomicUrl(foreign_table.get_subject().to_string().into()),
        store,
    )
    .await
    .unwrap();
    form.save_locally(store).await.unwrap();

    let req = test::TestRequest::post()
        .uri(&format!("/form/{}/submit", slug))
        .set_json(serde_json::json!({
            "values": { email_prop.get_subject().to_string(): "visitor3@example.com" },
            "altcha": solve_captcha!(),
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        403,
        "a target table outside the form's drive should be refused"
    );
    let foreign_rows = store
        .query(&atomic_lib::storelike::Query::new_prop_val(
            urls::PARENT,
            foreign_table.get_subject().as_str(),
        ))
        .await
        .unwrap();
    assert!(
        foreign_rows.subjects.is_empty(),
        "no row lands in the other drive"
    );
}
