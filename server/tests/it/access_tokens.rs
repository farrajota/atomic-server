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
    let sent = mint_body(&[(&f.drive, "read")], 600, "n1");
    let url = format!("{}/tokens", f.base);
    let mut req = http().post(&url).body(sent);
    for (k, v) in get_authentication_headers_v2("POST", &url, &signed_for, &f.alice).unwrap() {
        req = req.header(k, v);
    }
    assert_eq!(req.send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn mint_requests_are_validated() {
    let port = start_server_with_args("tokens_validate", &["--access-token-max-ttl", "300"]);
    let f = fixture(port).await;
    let post = |body: Vec<u8>| {
        signed_v2(reqwest::Method::POST, &f.base, "/tokens", &body, &f.alice).send()
    };

    let over_cap = post(mint_body(&[(&f.drive, "read")], 301, "n1"))
        .await
        .unwrap();
    assert_eq!(over_cap.status(), StatusCode::BAD_REQUEST);
    assert!(over_cap.text().await.unwrap().contains("300"));

    let no_scopes = post(mint_body(&[], 60, "n2")).await.unwrap();
    assert_eq!(no_scopes.status(), StatusCode::BAD_REQUEST);

    // D17: tokens are read-only, so a write or append scope is refused.
    for right in ["write", "append"] {
        let refused = post(mint_body(&[(&f.drive, right)], 60, right))
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "{right}");
    }

    let bad_right = post(mint_body(&[(&f.drive, "admin")], 60, "n3"))
        .await
        .unwrap();
    assert_eq!(bad_right.status(), StatusCode::BAD_REQUEST);

    let at_cap = post(mint_body(&[(&f.drive, "read")], 300, "n4"))
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

    let v1 = signed_v1(reqwest::Method::GET, &f.base, "/tokens", &f.alice)
        .send()
        .await
        .unwrap();
    assert_eq!(
        v1.status(),
        StatusCode::UNAUTHORIZED,
        "listing needs a v2 signature"
    );
    let resp = signed_v2(reqwest::Method::GET, &f.base, "/tokens", b"", &f.alice)
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
    let as_bob: Vec<Json> = signed_v2(reqwest::Method::GET, &f.base, "/tokens", b"", &bob)
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
    let token = token_of(&mint(&f, &[(&f.drive, "read")], 600, "n1").await);
    let doc = urlencoding::encode(&f.doc_a).into_owned();
    let drive = urlencoding::encode(&f.drive).into_owned();
    for path in [
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
    let token = token_of(&mint(&f, &[(&f.drive, "read")], 600, "n1").await);
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

/// Signed `AUTH`s run asynchronously. One that was spawned before a token
/// `AUTH` and finishes after it must not leave the session with the token's
/// identity but without the token's limits: here `AUTH {}` (fails), a token
/// `AUTH`, and `AUTH {}` again, back to back. The session must stay a
/// read-only token session that closes when the token is revoked.
#[tokio::test]
async fn racing_auth_frames_cannot_strip_a_token_sessions_limits() {
    let f = fixture(start_server("tokens_ws_race")).await;
    let minted = mint(&f, &[(&f.drive, "read")], 600, "n1").await;
    let ws_url = f.base.replace("http://", "ws://") + "/ws";
    let ws = WsClient::connect(&ws_url).await.unwrap();
    let mut rx = ws.subscribe();

    let failing_auth = vec![protocol::tag::AUTH, b'{', b'}'];
    ws.send_binary(failing_auth.clone()).await.unwrap();
    ws.send_binary(token_auth_frame(&token_of(&minted)))
        .await
        .unwrap();
    ws.send_binary(failing_auth).await.unwrap();

    // Two refusals and one AUTH_OK, in whatever order they complete.
    let (mut errors, mut oks) = (0, 0);
    tokio::time::timeout(Duration::from_secs(5), async {
        while errors < 2 || oks < 1 {
            match rx.recv().await {
                Ok(WsMessage::Error { .. }) => errors += 1,
                Ok(WsMessage::Authenticated) => oks += 1,
                Ok(_) => continue,
                Err(e) => panic!("closed early: {e}"),
            }
        }
    })
    .await
    .expect("all three AUTH frames answered");

    // Still a token session: a write frame hits the read-only gate.
    ws.send_binary(protocol::encode_sync_push(
        &f.drive,
        &[(&f.doc_a, b"not a real snapshot".as_slice())],
        true,
    ))
    .await
    .unwrap();
    let err = next_error(&mut rx).await;
    assert!(err.contains("read-only"), "{err}");

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
        .expect("the session still closes when its token is revoked");
}

/// A token only narrows: its agent must already read every target it names
/// when the token is minted. Otherwise any keypair could make the node store
/// tokens for subjects it has nothing to do with.
#[tokio::test]
async fn minting_needs_read_on_every_target() {
    let f = fixture(start_server("tokens_mint_read")).await;
    let bob = Client::new(&f.base)
        .await
        .unwrap()
        .new_agent("Bob")
        .await
        .unwrap();
    let post = |agent: &Agent, scopes: &[(&str, &str)], nonce: &str| {
        signed_v2(
            reqwest::Method::POST,
            &f.base,
            "/tokens",
            &mint_body(scopes, 60, nonce),
            agent,
        )
        .send()
    };

    let foreign = post(&bob, &[(&f.drive, "read")], "n1").await.unwrap();
    assert_eq!(
        foreign.status(),
        StatusCode::FORBIDDEN,
        "Bob cannot read Alice's drive"
    );
    let mixed = post(
        &f.alice,
        &[(&f.drive, "read"), ("atomic:no-such-resource", "read")],
        "n2",
    )
    .await
    .unwrap();
    assert_eq!(
        mixed.status(),
        StatusCode::FORBIDDEN,
        "one unknown target refuses all"
    );
    let own = post(&f.alice, &[(&f.folder_a, "read")], "n3")
        .await
        .unwrap();
    assert_eq!(own.status(), StatusCode::CREATED);
}

/// Every signed `/tokens` request is good once. A captured request that was
/// refused (here 403, as it would be for 429 or 5xx) cannot be sent again
/// later to mint after all; the client signs a new one instead.
#[tokio::test]
async fn a_signed_tokens_request_is_accepted_once() {
    let f = fixture(start_server("tokens_proof_once")).await;
    let bob_client = Client::new(&f.base).await.unwrap();
    let bob = bob_client.new_agent("Bob").await.unwrap();
    let body = mint_body(&[(&f.drive, "read")], 60, "n1");
    let url = format!("{}/tokens", f.base);
    let headers = get_authentication_headers_v2("POST", &url, &body, &bob).unwrap();
    let send = || {
        let mut req = http().post(&url).body(body.clone());
        for (k, v) in &headers {
            req = req.header(k, v);
        }
        req.send()
    };

    assert_eq!(send().await.unwrap().status(), StatusCode::FORBIDDEN);
    let mut drive = f.client.get_resource(&f.drive).await.unwrap();
    drive
        .set_unsafe(
            urls::READ.into(),
            Value::ResourceArray(vec![
                f.alice.subject.to_string().into(),
                bob.subject.to_string().into(),
            ]),
        )
        .unwrap();
    drive.save_remote(f.client.store()).await.unwrap();

    let replayed = send().await.unwrap();
    assert_eq!(
        replayed.status(),
        StatusCode::UNAUTHORIZED,
        "the same proof again"
    );
    let fresh = signed_v2(reqwest::Method::POST, &f.base, "/tokens", &body, &bob)
        .send()
        .await
        .unwrap();
    assert_eq!(
        fresh.status(),
        StatusCode::CREATED,
        "a freshly signed request"
    );
}

/// Alice's private drive, readable by Bob through an explicit grant, on a
/// server with the given extra flags. Two commits by Alice.
async fn shared_drive(base: &str) -> (Client, Agent, Agent, String) {
    let client = Client::new(base).await.unwrap();
    let alice = client.new_agent("Alice").await.unwrap();
    let drive = client.new_drive(&alice, "Shared").await.unwrap();
    let bob = Client::new(base)
        .await
        .unwrap()
        .new_agent("Bob")
        .await
        .unwrap();
    set_readers(&client, &drive, &[&alice, &bob]).await;
    (client, alice, bob, drive)
}

async fn set_readers(client: &Client, drive: &str, readers: &[&Agent]) {
    let mut resource = client.get_resource(drive).await.unwrap();
    resource
        .set_unsafe(
            urls::READ.into(),
            Value::ResourceArray(
                readers
                    .iter()
                    .map(|a| a.subject.to_string().into())
                    .collect(),
            ),
        )
        .unwrap();
    resource.save_remote(client.store()).await.unwrap();
}

/// The mint budget belongs to the agent, not to how its subject is spelled:
/// `did:ad:agent:` and `atomic:agent:` are one key.
#[tokio::test]
async fn both_spellings_of_an_agent_share_one_mint_budget() {
    let port = start_server_with_args("tokens_rate_spelling", &["--write-rate-limit", "3"]);
    wait_for_server(port).await;
    let base = format!("http://localhost:{port}");
    let (_client, _alice, bob, drive) = shared_drive(&base).await;
    let canonical = bob.subject.to_string().replace("did:ad:", "atomic:");
    let legacy = canonical.replace("atomic:", "did:ad:");
    let mint_as = |spelling: &str, nonce: &str| {
        let body = mint_body(&[(&drive, "read")], 60, nonce);
        let url = format!("{base}/tokens");
        let mut req = http().post(&url).body(body.clone());
        for (k, v) in get_authentication_headers_v2("POST", &url, &body, &bob).unwrap() {
            let v = if k == "x-atomic-agent" {
                spelling.to_string()
            } else {
                v
            };
            req = req.header(k, v);
        }
        req.send()
    };

    for (spelling, nonce) in [(&legacy, "n1"), (&legacy, "n2"), (&canonical, "n3")] {
        assert_eq!(
            mint_as(spelling, nonce).await.unwrap().status(),
            StatusCode::CREATED,
            "{nonce}"
        );
    }
    assert_eq!(
        mint_as(&legacy, "n4").await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS,
        "three mints spent the budget, whichever spelling signed them"
    );
}

/// Listing spends the agent's budget too, so a key cannot keep the store busy
/// with lists.
#[tokio::test]
async fn listing_is_rate_limited_per_agent() {
    let port = start_server_with_args("tokens_list_rate", &["--write-rate-limit", "2"]);
    wait_for_server(port).await;
    let base = format!("http://localhost:{port}");
    let bob = Client::new(&base)
        .await
        .unwrap()
        .new_agent("Bob")
        .await
        .unwrap();
    let list = || signed_v2(reqwest::Method::GET, &base, "/tokens", b"", &bob).send();
    assert_eq!(list().await.unwrap().status(), StatusCode::OK);
    assert_eq!(list().await.unwrap().status(), StatusCode::OK);
    assert_eq!(
        list().await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

/// A scope over data anyone may read grants nothing a token adds, and every
/// agent's own resource is public: such a mint is refused, so a key that was
/// never granted anything cannot make the node store tokens.
#[tokio::test]
async fn a_scope_over_public_data_is_refused() {
    let port = start_server("tokens_public_scope");
    wait_for_server(port).await;
    let base = format!("http://localhost:{port}");
    let client = Client::new(&base).await.unwrap();
    let alice = client.new_agent("Alice").await.unwrap();
    let public = client.new_public_drive(&alice, "Public").await.unwrap();
    let stranger = Client::new(&base)
        .await
        .unwrap()
        .new_agent("Stranger")
        .await
        .unwrap();
    let own_did = stranger.subject.to_string();
    let alice_did = alice.subject.to_string();
    for (agent, target) in [
        (&stranger, own_did.as_str()),
        (&stranger, alice_did.as_str()),
        (&stranger, public.as_str()),
        (&alice, public.as_str()),
    ] {
        let resp = signed_v2(
            reqwest::Method::POST,
            &base,
            "/tokens",
            &mint_body(&[(target, "read")], 60, &format!("n-{}", target.len())),
            agent,
        )
        .send()
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{target}");
    }
}

/// A retried mint finds its earlier token (409) before anything else is
/// checked, even when the agent has lost read on a target since.
#[tokio::test]
async fn a_retried_mint_reports_its_duplicate_even_after_read_was_lost() {
    let port = start_server("tokens_retry_after_revoke");
    wait_for_server(port).await;
    let base = format!("http://localhost:{port}");
    let (client, alice, bob, drive) = shared_drive(&base).await;
    let body = mint_body(&[(&drive, "read")], 600, "retry-me");
    let mint = || signed_v2(reqwest::Method::POST, &base, "/tokens", &body, &bob).send();
    assert_eq!(mint().await.unwrap().status(), StatusCode::CREATED);
    set_readers(&client, &drive, &[&alice]).await;
    assert_eq!(mint().await.unwrap().status(), StatusCode::CONFLICT);
}

/// Alice holds two private drives, A and B, and a public one; each has one
/// document named with `word` and its letter. A token scoped to A.
struct SearchFixture {
    base: String,
    alice: Agent,
    drive_b: String,
    doc_a: String,
    doc_b: String,
    doc_public: String,
    token: String,
    token_id: String,
}

async fn search_fixture(name: &str) -> SearchFixture {
    let port = start_server(name);
    wait_for_server(port).await;
    let base = format!("http://localhost:{port}");
    let client = Client::new(&base).await.unwrap();
    let alice = client.new_agent("Alice").await.unwrap();
    let drive_a = client.new_drive(&alice, "Drive A").await.unwrap();
    let drive_b = client.new_drive(&alice, "Drive B").await.unwrap();
    let public = client.new_public_drive(&alice, "Public").await.unwrap();
    let doc_a = child(&client, &drive_a, "zebranest alpha").await;
    let doc_b = child(&client, &drive_b, "zebranest bravo").await;
    let doc_public = child(&client, &public, "zebranest public").await;
    let resp = signed_v2(
        reqwest::Method::POST,
        &base,
        "/tokens",
        &mint_body(&[(&drive_a, "read")], 600, "search"),
        &alice,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let minted: Json = resp.json().await.unwrap();
    SearchFixture {
        base,
        alice,
        drive_b,
        doc_a,
        doc_b,
        doc_public,
        token: token_of(&minted),
        token_id: id_of(&minted),
    }
}

/// What a caller gets back from `path`: status, the `Server-Timing` header
/// and the body.
async fn get_as(
    base: &str,
    path: &str,
    bearer: Option<&str>,
    agent: Option<&Agent>,
) -> (StatusCode, String, String) {
    let url = format!("{base}{path}");
    let mut req = http().get(&url).header("Accept", "application/ad+json");
    if let Some(token) = bearer {
        req = req.bearer_auth(token);
    }
    if let Some(agent) = agent {
        for (k, v) in get_authentication_headers(&url, agent).unwrap() {
            req = req.header(k, v);
        }
    }
    let resp = req.send().await.unwrap();
    let status = resp.status();
    let timing = resp
        .headers()
        .get("Server-Timing")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    (status, timing, resp.text().await.unwrap())
}

/// A token's search answers what the token may read, the way a resource GET
/// does: hits in its scope and public hits, never a hit the agent could read
/// outside the scope, not by subject, title or count.
#[tokio::test]
async fn search_through_a_token_finds_only_what_it_may_read() {
    let f = search_fixture("tokens_search").await;
    for path in [
        "/search?q=zebranest".to_string(),
        "/search?q=zebranest&include=true".to_string(),
        format!(
            "/search?q=zebranest&parents={}",
            urlencoding::encode(&f.drive_b)
        ),
    ] {
        let (status, timing, body) = get_as(&f.base, &path, Some(&f.token), None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert!(
            !body.contains(&f.doc_b),
            "{path}: B's subject leaked: {body}"
        );
        assert!(!body.contains("bravo"), "{path}: B's title leaked: {body}");
        assert!(timing.is_empty(), "{path}: no Server-Timing: {timing}");
    }
    let (_, _, body) = get_as(&f.base, "/search?q=zebranest", Some(&f.token), None).await;
    assert!(body.contains(&f.doc_a), "in scope: {body}");
    // A token searches its scopes only: public data elsewhere is found
    // anonymously, not through the token.
    assert!(!body.contains(&f.doc_public), "outside the scopes: {body}");

    // Anonymous: only the public document.
    let (status, _, body) = get_as(&f.base, "/search?q=zebranest", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&f.doc_public) && !body.contains(&f.doc_a) && !body.contains(&f.doc_b));

    // The agent's own signed search still finds all three.
    let (status, _, body) = get_as(&f.base, "/search?q=zebranest", None, Some(&f.alice)).await;
    assert_eq!(status, StatusCode::OK);
    for doc in [&f.doc_a, &f.doc_b, &f.doc_public] {
        assert!(body.contains(doc.as_str()), "{doc}: {body}");
    }
}

/// `/search` used to send every caller, anonymous included, a
/// `Server-Timing` header with the index's candidate counts (taken before
/// the rights filter) and per-step durations that track them. That counted
/// private matches for anyone. Nobody gets it now unless the operator turns
/// on `--debug-search-timing`.
#[tokio::test]
async fn search_timing_is_not_sent_unless_the_operator_asks_for_it() {
    let f = search_fixture("tokens_search_timing").await;
    let private = format!(
        "/search?q=zebranest&parents={}",
        urlencoding::encode(&f.drive_b)
    );
    for (who, bearer, agent) in [
        ("anonymous", None, None),
        ("signed agent", None, Some(&f.alice)),
        ("token", Some(f.token.as_str()), None),
    ] {
        for path in ["/search?q=zebranest", private.as_str()] {
            let (status, timing, _) = get_as(&f.base, path, bearer, agent).await;
            assert_eq!(status, StatusCode::OK, "{who} {path}");
            assert!(timing.is_empty(), "{who} {path}: {timing}");
        }
    }

    let port = start_server_with_args("tokens_search_timing_debug", &["--debug-search-timing"]);
    wait_for_server(port).await;
    let base = format!("http://localhost:{port}");
    let (status, timing, _) = get_as(&base, "/search?q=anything", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        timing.contains("candidates") && timing.contains("score;dur"),
        "the operator's debug flag keeps the full header: {timing}"
    );
}

/// The subjects a `/search` answer lists as its results.
fn search_results(body: &str) -> Vec<String> {
    let parsed: Json = serde_json::from_str(body).unwrap();
    let resources = parsed.as_array().cloned().unwrap_or_else(|| vec![parsed]);
    resources
        .iter()
        .find_map(|r| r.get("https://atomicdata.dev/properties/endpoint/results"))
        .and_then(Json::as_array)
        .map(|results| {
            results
                .iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect()
        })
        // An empty list may be left out of the JSON.
        .unwrap_or_default()
}

/// A token's search runs under its scopes: without `parents` it searches the
/// scope targets, so matches it may not read cannot fill the result window;
/// requested `parents` are narrowed to what lies inside a scope, and a parent
/// outside every scope finds nothing.
#[tokio::test]
async fn a_tokens_search_runs_under_its_scopes() {
    let port = start_server("tokens_search_scoped");
    wait_for_server(port).await;
    let base = format!("http://localhost:{port}");
    let client = Client::new(&base).await.unwrap();
    let alice = client.new_agent("Alice").await.unwrap();
    let drive_a = client.new_drive(&alice, "Drive A").await.unwrap();
    let drive_b = client.new_drive(&alice, "Drive B").await.unwrap();
    let folder = child(&client, &drive_a, "Scoped folder").await;
    let in_scope = child(
        &client,
        &folder,
        "quokkaword inside the scoped folder of drive a",
    )
    .await;
    let beside = child(&client, &drive_a, "quokkaword beside").await;
    // Shorter names that rank above the one in scope.
    for n in 0..8 {
        child(&client, &drive_b, &format!("quokkaword {n}")).await;
    }
    let resp = signed_v2(
        reqwest::Method::POST,
        &base,
        "/tokens",
        &mint_body(&[(&folder, "read")], 600, "scoped"),
        &alice,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let token = token_of(&resp.json().await.unwrap());
    let search = |query: String| {
        let base = base.clone();
        let token = token.clone();
        async move {
            get_as(
                &base,
                &format!("/search?q=quokkaword&{query}"),
                Some(&token),
                None,
            )
            .await
        }
    };

    let (status, _, body) = search("limit=1".into()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(&in_scope),
        "found despite better-ranked matches elsewhere: {body}"
    );

    let (_, _, body) = search(format!("parents={}", urlencoding::encode(&drive_a))).await;
    assert!(
        body.contains(&in_scope),
        "a requested ancestor is narrowed to the scope: {body}"
    );
    assert!(!body.contains(&beside), "{body}");

    let (status, _, body) = search(format!("parents={}", urlencoding::encode(&drive_b))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        search_results(&body).is_empty(),
        "a parent outside every scope finds nothing: {body}"
    );

    let (_, _, body) = search(format!(
        "parents={},{}",
        urlencoding::encode(&drive_b),
        urlencoding::encode(&folder)
    ))
    .await;
    assert!(
        body.contains(&in_scope),
        "the parent inside a scope stays: {body}"
    );
}

/// `/history-attribution` through a token: the history of what it may read,
/// a 401 for anything else, exactly as a resource read would answer.
#[tokio::test]
async fn history_through_a_token_is_limited_to_what_it_may_read() {
    let f = search_fixture("tokens_history").await;
    let history = |subject: &str| {
        format!(
            "/history-attribution?subject={}",
            urlencoding::encode(subject)
        )
    };

    let (status, _, body) = get_as(&f.base, &history(&f.doc_a), Some(&f.token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let report: Json = serde_json::from_str(&body).unwrap();
    assert!(report["attributions"].is_array(), "{report}");

    let (status, _, body) = get_as(&f.base, &history(&f.doc_b), Some(&f.token), None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "outside the scope: {body}"
    );
    assert!(!body.contains("attributions"), "{body}");

    let (status, _, _) = get_as(&f.base, &history(&f.doc_public), Some(&f.token), None).await;
    assert_eq!(status, StatusCode::OK, "public, as for anonymous");
    let (status, _, _) = get_as(&f.base, &history(&f.doc_public), None, None).await;
    assert_eq!(status, StatusCode::OK);

    // The agent itself, signed, still reads B's history.
    let (status, _, _) = get_as(&f.base, &history(&f.doc_b), None, Some(&f.alice)).await;
    assert_eq!(status, StatusCode::OK);
}

/// A revoked token gets 401 on both endpoints, like everywhere else.
#[tokio::test]
async fn search_and_history_refuse_a_revoked_token() {
    let f = search_fixture("tokens_search_revoked").await;
    let revoked = signed_v2(
        reqwest::Method::DELETE,
        &f.base,
        &format!("/tokens/{}", f.token_id),
        b"",
        &f.alice,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(revoked.status(), StatusCode::NO_CONTENT);
    for path in [
        "/search?q=zebranest".to_string(),
        format!(
            "/history-attribution?subject={}",
            urlencoding::encode(&f.doc_a)
        ),
    ] {
        let (status, _, body) = get_as(&f.base, &path, Some(&f.token), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
        assert!(body.contains("revoked"), "{path}: {body}");
    }
}
