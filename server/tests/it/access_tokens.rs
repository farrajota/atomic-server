//! Self-issued access tokens (`/tokens`) against a real server: minting with
//! the agent's own key, bearer reads narrowed to the token's scopes, listing,
//! revoking by key and by the token itself, expiry, read-only WebSocket
//! sessions that close on revoke and expiry, and tokens surviving a restart.
//!
//! Run with: cargo test -p atomic-server --test it access_tokens

use atomic_lib::{
    agents::Agent,
    client::{
        connected::Client,
        helpers::{get_authentication_headers, get_authentication_headers_v2},
        ws::{WsClient, WsMessage},
    },
    sync::protocol,
    urls, Value,
};
use reqwest::StatusCode;
use serde_json::{json, Value as Json};
use std::time::Duration;

use crate::common::{start_server, start_server_with_args, wait_for_server, ServerProcess};

/// A private drive of Alice's: folder A holding `doc_a`, and `doc_b` straight
/// under the drive.
struct Fixture {
    base: String,
    client: Client,
    alice: Agent,
    drive: String,
    folder_a: String,
    doc_a: String,
    doc_b: String,
}

async fn child(client: &Client, parent: &str, name: &str) -> String {
    let mut r = client.new_resource(parent).unwrap();
    r.set_name(name).unwrap();
    r.save_remote(client.store()).await.unwrap()
}

async fn fixture(port: u16) -> Fixture {
    wait_for_server(port).await;
    let base = format!("http://localhost:{port}");
    let client = Client::new(&base).await.unwrap();
    let alice = client.new_agent("Alice").await.unwrap();
    let drive = client.new_drive(&alice, "Private").await.unwrap();
    let folder_a = child(&client, &drive, "Folder A").await;
    let doc_a = child(&client, &folder_a, "Doc A").await;
    let doc_b = child(&client, &drive, "Doc B").await;
    Fixture {
        base,
        client,
        alice,
        drive,
        folder_a,
        doc_a,
        doc_b,
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// A request signed with version 2 (method, URL and body covered).
fn signed_v2(
    method: reqwest::Method,
    base: &str,
    path: &str,
    body: &[u8],
    agent: &Agent,
) -> reqwest::RequestBuilder {
    let url = format!("{base}{path}");
    let mut req = http().request(method.clone(), &url).body(body.to_vec());
    for (k, v) in get_authentication_headers_v2(method.as_str(), &url, body, agent).unwrap() {
        req = req.header(k, v);
    }
    req
}

/// A request signed with version 1 (URL and timestamp only).
fn signed_v1(
    method: reqwest::Method,
    base: &str,
    path: &str,
    agent: &Agent,
) -> reqwest::RequestBuilder {
    let url = format!("{base}{path}");
    let mut req = http().request(method, &url);
    for (k, v) in get_authentication_headers(&url, agent).unwrap() {
        req = req.header(k, v);
    }
    req
}

fn mint_body(scopes: &[(&str, &str)], ttl: i64, nonce: &str) -> Vec<u8> {
    let scopes: Vec<Json> = scopes
        .iter()
        .map(|(target, right)| json!({"target": target, "right": right}))
        .collect();
    serde_json::to_vec(&json!({
        "ttl_seconds": ttl,
        "scopes": scopes,
        "label": "openbao:atomic:test",
        "nonce": nonce,
    }))
    .unwrap()
}

/// Mint with Alice's key; returns the 201 body.
async fn mint(f: &Fixture, scopes: &[(&str, &str)], ttl: i64, nonce: &str) -> Json {
    let resp = signed_v2(
        reqwest::Method::POST,
        &f.base,
        "/tokens",
        &mint_body(scopes, ttl, nonce),
        &f.alice,
    )
    .send()
    .await
    .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, StatusCode::CREATED, "{body}");
    serde_json::from_str(&body).unwrap()
}

fn token_of(minted: &Json) -> String {
    minted["token"].as_str().unwrap().to_string()
}

fn id_of(minted: &Json) -> String {
    minted["token_id"].as_str().unwrap().to_string()
}

async fn read_with(base: &str, subject: &str, bearer: &str) -> (StatusCode, String) {
    let resp = http()
        .get(format!(
            "{base}/resource?subject={}",
            urlencoding::encode(subject)
        ))
        .header("Accept", "application/ad+json")
        .bearer_auth(bearer)
        .send()
        .await
        .unwrap();
    (resp.status(), resp.text().await.unwrap())
}

async fn introspect(base: &str, bearer: &str) -> (StatusCode, String) {
    let resp = http()
        .get(format!("{base}/tokens/self"))
        .bearer_auth(bearer)
        .send()
        .await
        .unwrap();
    (resp.status(), resp.text().await.unwrap())
}

#[tokio::test]
async fn a_token_reads_its_scope_and_nothing_else() {
    let f = fixture(start_server("tokens_scope")).await;
    let minted = mint(&f, &[(&f.folder_a, "read")], 600, "n1").await;
    let token = token_of(&minted);
    assert!(token.starts_with("atk_"), "{minted}");
    assert_eq!(
        minted["agent"],
        f.alice.subject.to_string().replace("did:ad:", "atomic:")
    );

    let (status, body) = read_with(&f.base, &f.doc_a, &token).await;
    assert_eq!(status, StatusCode::OK, "in scope: {body}");
    let (status, _) = read_with(&f.base, &f.doc_b, &token).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "Alice may read doc B, the token may not"
    );
    let (status, _) = read_with(&f.base, &f.drive, &token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the scope's ancestor");

    let (status, body) = introspect(&f.base, &token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let me: Json = serde_json::from_str(&body).unwrap();
    assert_eq!(me["token_id"], minted["token_id"]);
    assert_eq!(me["agent"], minted["agent"]);
    assert_eq!(me["expires_at"], minted["expires_at"]);
    assert!(
        me.get("token").is_none(),
        "introspection never returns the token: {me}"
    );
}

#[tokio::test]
async fn a_query_through_a_token_only_lists_what_the_scope_covers() {
    let f = fixture(start_server("tokens_query")).await;
    let token = token_of(&mint(&f, &[(&f.folder_a, "read")], 600, "n1").await);
    let path = format!(
        "/query?property={}&value={}",
        urlencoding::encode(urls::PARENT),
        urlencoding::encode(&f.drive)
    );

    let as_alice = signed_v1(reqwest::Method::GET, &f.base, &path, &f.alice)
        .header("Accept", "application/ad+json")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        as_alice.contains(&f.doc_b) && as_alice.contains(&f.folder_a),
        "{as_alice}"
    );

    let resp = http()
        .get(format!("{}{path}", f.base))
        .header("Accept", "application/ad+json")
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();
    assert!(body.contains(&f.folder_a), "{body}");
    assert!(!body.contains(&f.doc_b), "outside the scope: {body}");
}

#[tokio::test]
async fn minting_needs_a_version_2_signature_by_the_key_not_a_token() {
    let f = fixture(start_server("tokens_mint_auth")).await;
    let token = token_of(&mint(&f, &[(&f.drive, "read")], 600, "n1").await);
    let body = mint_body(&[(&f.drive, "read")], 600, "n2");

    let with_bearer = http()
        .post(format!("{}/tokens", f.base))
        .bearer_auth(&token)
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(
        with_bearer.status(),
        StatusCode::UNAUTHORIZED,
        "a token cannot mint"
    );

    let v1 = signed_v1(reqwest::Method::POST, &f.base, "/tokens", &f.alice)
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(
        v1.status(),
        StatusCode::UNAUTHORIZED,
        "v1 does not cover the body"
    );

    let anonymous = http()
        .post(format!("{}/tokens", f.base))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_tampered_body_does_not_mint() {
    let f = fixture(start_server("tokens_tamper")).await;
    let signed_for = mint_body(&[(&f.folder_a, "read")], 600, "n1");
    let sent = mint_body(&[(&f.drive, "write")], 600, "n1");
    let url = format!("{}/tokens", f.base);
    let mut req = http().post(&url).body(sent);
    for (k, v) in get_authentication_headers_v2("POST", &url, &signed_for, &f.alice).unwrap() {
        req = req.header(k, v);
    }
    assert_eq!(req.send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn mint_requests_are_validated() {
    let port = start_server_with_args("tokens_validate", &["--access-token-max-ttl", "120"]);
    let f = fixture(port).await;
    let post = |body: Vec<u8>| {
        signed_v2(reqwest::Method::POST, &f.base, "/tokens", &body, &f.alice).send()
    };

    let over_cap = post(mint_body(&[(&f.drive, "read")], 121, "n1"))
        .await
        .unwrap();
    assert_eq!(over_cap.status(), StatusCode::BAD_REQUEST);
    assert!(over_cap.text().await.unwrap().contains("120"));

    let no_scopes = post(mint_body(&[], 60, "n2")).await.unwrap();
    assert_eq!(no_scopes.status(), StatusCode::BAD_REQUEST);

    let bad_right = post(mint_body(&[(&f.drive, "admin")], 60, "n3"))
        .await
        .unwrap();
    assert_eq!(bad_right.status(), StatusCode::BAD_REQUEST);

    let at_cap = post(mint_body(&[(&f.drive, "read")], 120, "n4"))
        .await
        .unwrap();
    assert_eq!(at_cap.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn a_retried_mint_with_the_same_nonce_returns_the_existing_id_without_the_token() {
    let f = fixture(start_server("tokens_nonce")).await;
    let first = mint(&f, &[(&f.drive, "read")], 600, "same").await;
    let again = signed_v2(
        reqwest::Method::POST,
        &f.base,
        "/tokens",
        &mint_body(&[(&f.drive, "read")], 600, "same"),
        &f.alice,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(again.status(), StatusCode::CONFLICT);
    let body: Json = again.json().await.unwrap();
    assert_eq!(body["token_id"], first["token_id"]);
    assert!(body.get("token").is_none(), "{body}");
    assert!(!body.to_string().contains(&token_of(&first)));
}

#[tokio::test]
async fn listing_shows_label_and_nonce_and_never_a_token() {
    let f = fixture(start_server("tokens_list")).await;
    let minted = mint(&f, &[(&f.drive, "read")], 600, "list-nonce").await;
    let token = token_of(&minted);

    let resp = signed_v1(reqwest::Method::GET, &f.base, "/tokens", &f.alice)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = resp.text().await.unwrap();
    assert!(!text.contains(&token), "{text}");
    let listed: Vec<Json> = serde_json::from_str(&text).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["token_id"], minted["token_id"]);
    assert_eq!(listed[0]["label"], "openbao:atomic:test");
    assert_eq!(listed[0]["nonce"], "list-nonce");
    assert!(listed[0]["created_at"].is_i64());

    let as_token = http()
        .get(format!("{}/tokens", f.base))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        as_token.status(),
        StatusCode::UNAUTHORIZED,
        "listing needs the key"
    );

    // Another agent sees none of Alice's tokens.
    let bob = Client::new(&f.base)
        .await
        .unwrap()
        .new_agent("Bob")
        .await
        .unwrap();
    let as_bob: Vec<Json> = signed_v1(reqwest::Method::GET, &f.base, "/tokens", &bob)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(as_bob.is_empty());
}

#[tokio::test]
async fn revoking_with_the_key_stops_the_token() {
    let f = fixture(start_server("tokens_revoke_key")).await;
    let minted = mint(&f, &[(&f.drive, "read")], 600, "n1").await;
    let token = token_of(&minted);
    assert_eq!(read_with(&f.base, &f.doc_a, &token).await.0, StatusCode::OK);

    let path = format!("/tokens/{}", id_of(&minted));
    let bob = Client::new(&f.base)
        .await
        .unwrap()
        .new_agent("Bob")
        .await
        .unwrap();
    let by_bob = signed_v2(reqwest::Method::DELETE, &f.base, &path, b"", &bob)
        .send()
        .await
        .unwrap();
    assert_eq!(by_bob.status(), StatusCode::NOT_FOUND, "not Bob's token");
    assert_eq!(read_with(&f.base, &f.doc_a, &token).await.0, StatusCode::OK);

    let revoked = signed_v2(reqwest::Method::DELETE, &f.base, &path, b"", &f.alice)
        .send()
        .await
        .unwrap();
    assert_eq!(revoked.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        read_with(&f.base, &f.doc_a, &token).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        introspect(&f.base, &token).await.0,
        StatusCode::UNAUTHORIZED
    );

    let again = signed_v2(reqwest::Method::DELETE, &f.base, &path, b"", &f.alice)
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::NOT_FOUND, "already revoked");
    let unknown = signed_v2(
        reqwest::Method::DELETE,
        &f.base,
        "/tokens/nope",
        b"",
        &f.alice,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_token_can_revoke_itself_and_only_itself() {
    let f = fixture(start_server("tokens_revoke_self")).await;
    let one = mint(&f, &[(&f.drive, "read")], 600, "n1").await;
    let two = mint(&f, &[(&f.drive, "read")], 600, "n2").await;

    let other = http()
        .delete(format!("{}/tokens/{}", f.base, id_of(&two)))
        .bearer_auth(token_of(&one))
        .send()
        .await
        .unwrap();
    assert_eq!(
        other.status(),
        StatusCode::NOT_FOUND,
        "a token only revokes itself"
    );
    assert_eq!(introspect(&f.base, &token_of(&two)).await.0, StatusCode::OK);

    let own = http()
        .delete(format!("{}/tokens/{}", f.base, id_of(&one)))
        .bearer_auth(token_of(&one))
        .send()
        .await
        .unwrap();
    assert_eq!(own.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        introspect(&f.base, &token_of(&one)).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_token_stops_working_at_its_expiry() {
    let f = fixture(start_server("tokens_expiry")).await;
    let minted = mint(&f, &[(&f.drive, "read")], 2, "n1").await;
    let token = token_of(&minted);
    assert_eq!(read_with(&f.base, &f.doc_a, &token).await.0, StatusCode::OK);

    let expires_at = minted["expires_at"].as_i64().unwrap();
    let wait = expires_at - atomic_lib::utils::now() + 50;
    tokio::time::sleep(Duration::from_millis(wait.max(0) as u64)).await;
    assert_eq!(
        read_with(&f.base, &f.doc_a, &token).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, body) = introspect(&f.base, &token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("expired"), "{body}");
}

#[tokio::test]
async fn removing_the_agents_acl_right_applies_to_its_live_token() {
    let f = fixture(start_server("tokens_acl")).await;
    let bob_client = Client::new(&f.base).await.unwrap();
    let bob = bob_client.new_agent("Bob").await.unwrap();
    let set_drive_read = |readers: Vec<String>| {
        let f = &f;
        async move {
            let mut drive = f.client.get_resource(&f.drive).await.unwrap();
            drive
                .set_unsafe(
                    urls::READ.into(),
                    Value::ResourceArray(readers.into_iter().map(Into::into).collect()),
                )
                .unwrap();
            drive.save_remote(f.client.store()).await.unwrap();
        }
    };
    set_drive_read(vec![f.alice.subject.to_string(), bob.subject.to_string()]).await;

    let body = mint_body(&[(&f.drive, "read")], 600, "bob-1");
    let resp = signed_v2(reqwest::Method::POST, &f.base, "/tokens", &body, &bob)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let token = token_of(&resp.json().await.unwrap());
    assert_eq!(read_with(&f.base, &f.doc_a, &token).await.0, StatusCode::OK);

    set_drive_read(vec![f.alice.subject.to_string()]).await;
    assert_eq!(
        read_with(&f.base, &f.doc_a, &token).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_token_is_refused_by_endpoints_that_do_not_take_one() {
    let f = fixture(start_server("tokens_other_endpoints")).await;
    let token = token_of(&mint(&f, &[(&f.drive, "write")], 600, "n1").await);
    let doc = urlencoding::encode(&f.doc_a).into_owned();
    let drive = urlencoding::encode(&f.drive).into_owned();
    for path in [
        "/search?q=Doc".to_string(),
        format!("/export?subject={doc}&format=csv"),
        format!("/app-agent?drive={drive}&app={doc}"),
        format!("/drive-usage?subject={drive}"),
    ] {
        let resp = http()
            .get(format!("{}{path}", f.base))
            // Without it `/search` is answered by the HTML app shell.
            .header("Accept", "application/ad+json")
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
}

/// Commits are signed by the agent's key; a token carries no key. An
/// unsigned commit sent with a bearer token is refused and changes nothing.
#[tokio::test]
async fn a_token_cannot_commit() {
    let f = fixture(start_server("tokens_commit")).await;
    let token = token_of(&mint(&f, &[(&f.drive, "write")], 600, "n1").await);
    let commit = json!({
        "https://atomicdata.dev/properties/isA": ["https://atomicdata.dev/classes/Commit"],
        "https://atomicdata.dev/properties/subject": f.doc_a,
        "https://atomicdata.dev/properties/createdAt": atomic_lib::utils::now(),
        "https://atomicdata.dev/properties/signer": f.alice.subject.to_string(),
        "https://atomicdata.dev/properties/destroy": true,
    });
    let resp = http()
        .post(format!("{}/commit", f.base))
        .header("Content-Type", "application/ad+json")
        .bearer_auth(&token)
        .body(commit.to_string())
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_client_error(), "{}", resp.status());
    assert_eq!(
        read_with(&f.base, &f.doc_a, &token).await.0,
        StatusCode::OK,
        "still there"
    );
}

fn token_auth_frame(token: &str) -> Vec<u8> {
    let mut frame = vec![protocol::tag::AUTH];
    frame.extend(json!({ "accessToken": token }).to_string().into_bytes());
    frame
}

async fn token_socket(f: &Fixture, token: &str) -> WsClient {
    let ws_url = f.base.replace("http://", "ws://") + "/ws";
    let ws = WsClient::connect(&ws_url).await.unwrap();
    ws.authenticate_with_frame(token_auth_frame(token))
        .await
        .expect("token AUTH accepted");
    ws
}

async fn next_error(rx: &mut tokio::sync::broadcast::Receiver<WsMessage>) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(WsMessage::Error { message, .. }) => return message,
                Ok(_) => continue,
                Err(e) => panic!("closed before an ERROR: {e}"),
            }
        }
    })
    .await
    .expect("an ERROR within 5s")
}

#[tokio::test]
async fn a_token_session_receives_updates_in_scope_and_is_read_only() {
    let f = fixture(start_server("tokens_ws_read")).await;
    let token = token_of(&mint(&f, &[(&f.folder_a, "read")], 600, "n1").await);
    let ws = token_socket(&f, &token).await;
    let mut rx = ws.subscribe();

    ws.subscribe_resource(&f.doc_b).await.unwrap();
    let err = next_error(&mut rx).await;
    assert!(
        err.to_lowercase().contains("doc") || !err.is_empty(),
        "{err}"
    );

    ws.subscribe_resource(&f.doc_a).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut doc = f.client.get_resource(&f.doc_a).await.unwrap();
    doc.set_name("Doc A, renamed").unwrap();
    doc.save_remote(f.client.store()).await.unwrap();
    let updated = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(WsMessage::Update { subject, .. }) = rx.recv().await {
                if subject == f.doc_a
                    || subject.replace("did:ad:", "atomic:")
                        == f.doc_a.replace("did:ad:", "atomic:")
                {
                    return true;
                }
            }
        }
    })
    .await;
    assert!(
        updated.is_ok(),
        "the token session receives the update in its scope"
    );

    // Writes and identity-bearing frames are refused on a token session.
    ws.send_binary(protocol::encode_ephemeral(
        protocol::ephemeral_kind::PRESENCE,
        &f.drive,
        "",
        b"x",
    ))
    .await
    .unwrap();
    let err = next_error(&mut rx).await;
    assert!(err.contains("access token"), "{err}");
}

#[tokio::test]
async fn revoking_a_token_closes_its_websocket_sessions() {
    let f = fixture(start_server("tokens_ws_revoke")).await;
    let minted = mint(&f, &[(&f.drive, "read")], 600, "n1").await;
    let ws = token_socket(&f, &token_of(&minted)).await;
    ws.subscribe_resource(&f.drive).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!ws.is_closed());

    let revoked = signed_v2(
        reqwest::Method::DELETE,
        &f.base,
        &format!("/tokens/{}", id_of(&minted)),
        b"",
        &f.alice,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(revoked.status(), StatusCode::NO_CONTENT);
    tokio::time::timeout(Duration::from_secs(3), ws.wait_closed())
        .await
        .expect("the session closes when its token is revoked");
}

#[tokio::test]
async fn a_token_session_closes_when_the_token_expires() {
    let f = fixture(start_server("tokens_ws_expiry")).await;
    let minted = mint(&f, &[(&f.drive, "read")], 2, "n1").await;
    let ws = token_socket(&f, &token_of(&minted)).await;
    ws.subscribe_resource(&f.drive).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), ws.wait_closed())
        .await
        .expect("the session closes at the token's expiry");
}

#[tokio::test]
async fn an_unknown_token_does_not_open_a_session() {
    let f = fixture(start_server("tokens_ws_unknown")).await;
    let ws_url = f.base.replace("http://", "ws://") + "/ws";
    let ws = WsClient::connect(&ws_url).await.unwrap();
    assert!(ws
        .authenticate_with_frame(token_auth_frame("atk_not-a-token"))
        .await
        .is_err());
}

#[tokio::test]
async fn tokens_survive_a_restart() {
    let server = ServerProcess::start("tokens_restart", &[]).await;
    let f = fixture(server.port).await;
    let minted = mint(&f, &[(&f.drive, "read")], 600, "n1").await;
    let revoked = mint(&f, &[(&f.drive, "read")], 600, "n2").await;
    let del = signed_v2(
        reqwest::Method::DELETE,
        &f.base,
        &format!("/tokens/{}", id_of(&revoked)),
        b"",
        &f.alice,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(del.status(), StatusCode::NO_CONTENT);

    let server = server.restart().await;
    let base = format!("http://localhost:{}", server.port);
    let (status, body) = read_with(&base, &f.doc_a, &token_of(&minted)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        introspect(&base, &token_of(&minted)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        introspect(&base, &token_of(&revoked)).await.0,
        StatusCode::UNAUTHORIZED,
        "a revoke survives the restart too"
    );
    // The nonce is still remembered.
    let again = signed_v2(
        reqwest::Method::POST,
        &base,
        "/tokens",
        &mint_body(&[(&f.drive, "read")], 600, "n1"),
        &f.alice,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(again.status(), StatusCode::CONFLICT);
}
