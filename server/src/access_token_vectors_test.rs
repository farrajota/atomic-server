//! The fixed request signatures in
//! `server/tests/fixtures/access_token_request_vectors.json` verify with this
//! server's own code. They were signed by an independent implementation
//! (Python `cryptography`), so a client that reproduces them byte for byte
//! (the OpenBao Go plugin) is proven against the Rust verifier, not only
//! against itself.

use atomic_lib::authentication::{
    check_auth_signature, request_signature_message_v2, sha256_hex, AuthValues, RequestBinding,
};

const VECTORS: &str = include_str!("../tests/fixtures/access_token_request_vectors.json");

#[derive(serde::Deserialize)]
struct Vectors {
    private_key: String,
    public_key: String,
    agent: String,
    vectors: Vec<Vector>,
}

#[derive(serde::Deserialize)]
struct Vector {
    name: String,
    version: u8,
    method: String,
    url: String,
    timestamp: i64,
    body: String,
    body_sha256_hex: String,
    message: String,
    signature: String,
}

fn auth_values(v: &Vectors, vector: &Vector, body: &[u8]) -> AuthValues {
    AuthValues {
        public_key: v.public_key.clone(),
        timestamp: vector.timestamp,
        signature: vector.signature.clone(),
        requested_subject: vector.url.clone(),
        agent_subject: v.agent.clone(),
        request: (vector.version == 2).then(|| RequestBinding::new(&vector.method, body)),
    }
}

#[test]
fn access_token_request_vectors_verify() {
    let v: Vectors = serde_json::from_str(VECTORS).unwrap();
    let pair = atomic_lib::agents::generate_public_key(&v.private_key);
    assert_eq!(
        pair.public, v.public_key,
        "public key derives from the seed"
    );
    assert_eq!(v.agent, format!("atomic:agent:{}", v.public_key));
    assert_eq!(v.vectors.len(), 3);

    for vector in &v.vectors {
        let name = &vector.name;
        assert_eq!(
            sha256_hex(vector.body.as_bytes()),
            vector.body_sha256_hex,
            "{name}"
        );
        let expected_message = match vector.version {
            2 => request_signature_message_v2(
                &vector.method,
                &vector.url,
                vector.timestamp,
                &vector.body_sha256_hex,
            ),
            _ => format!("{} {}", vector.url, vector.timestamp),
        };
        assert_eq!(
            expected_message, vector.message,
            "{name}: the signed message"
        );
        // Ed25519 is deterministic: the Rust signer makes the same bytes.
        assert_eq!(
            atomic_lib::agents::sign_message(vector.message.as_bytes(), &v.private_key).unwrap(),
            vector.signature,
            "{name}"
        );
        check_auth_signature(
            &vector.url,
            &auth_values(&v, vector, vector.body.as_bytes()),
        )
        .unwrap_or_else(|e| panic!("{name}: the server's verifier refused it: {e}"));

        if vector.version == 2 {
            let tampered = format!("{} ", vector.body);
            assert!(
                check_auth_signature(&vector.url, &auth_values(&v, vector, tampered.as_bytes()))
                    .is_err(),
                "{name}: a changed body must not verify"
            );
        }
    }
}

/// The URL the plugin signs is the public origin, while it may connect to the
/// in-cluster Service with `X-Forwarded-Host` / `X-Forwarded-Proto`. The
/// server must rebuild exactly the URL in the vectors from such a request.
#[actix_rt::test]
async fn the_server_rebuilds_the_signed_url_behind_forwarded_headers() {
    let appstate = crate::tests::init_test_appstate(&["--domain", "atomic.zay.home"]).await;
    let v: Vectors = serde_json::from_str(VECTORS).unwrap();
    for vector in &v.vectors {
        let path = vector.url.strip_prefix("https://atomic.zay.home").unwrap();
        let req = actix_web::test::TestRequest::default()
            .uri(path)
            .insert_header(("Host", "atomic-server.home-lab.svc.cluster.local:9883"))
            .insert_header(("X-Forwarded-Host", "atomic.zay.home"))
            .insert_header(("X-Forwarded-Proto", "https"))
            .to_http_request();
        assert_eq!(
            crate::handlers::access_tokens::signed_url(&req, &appstate).unwrap(),
            vector.url,
            "{}",
            vector.name
        );
    }
}

/// On `/tokens` the signed URL is bound to the host this node is configured
/// for. Loopback names are trusted elsewhere (`host_is_served_here`), so a
/// mint signed for `http://localhost:9883/tokens` on a laptop could otherwise
/// be replayed to a production node of the same agent.
#[actix_rt::test]
async fn a_tokens_request_signed_for_another_host_is_refused() {
    use actix_web::{test, web::Data, App};
    use atomic_lib::Storelike;

    let appstate = crate::tests::init_test_appstate(&["--domain", "atomic.zay.home"]).await;
    atomic_lib::test_utils::setup_test_env(&appstate.store)
        .await
        .unwrap();
    let signer = appstate.store.get_default_agent().unwrap();
    let drive = appstate
        .store
        .get_drive_did("localhost")
        .await
        .unwrap()
        .unwrap();
    let app = test::init_service(
        App::new()
            .app_data(Data::new(appstate.clone()))
            .configure(crate::routes::config_routes),
    )
    .await;
    let body = |nonce: &str| {
        serde_json::to_vec(&serde_json::json!({
            "ttl_seconds": 60,
            "scopes": [{"target": drive.to_string(), "right": "read"}],
            "nonce": nonce,
        }))
        .unwrap()
    };
    let request = |signed_url: &str, host: &str, forwarded: bool, body: Vec<u8>| {
        let mut req = test::TestRequest::post()
            .uri("/tokens")
            .insert_header(("Host", host))
            .set_payload(body.clone());
        if forwarded {
            req = req
                .insert_header(("X-Forwarded-Host", "atomic.zay.home"))
                .insert_header(("X-Forwarded-Proto", "https"));
        }
        for header in atomic_lib::client::helpers::get_authentication_headers_v2(
            "POST", signed_url, &body, &signer,
        )
        .unwrap()
        {
            req = req.insert_header(header);
        }
        req.to_request()
    };

    let replayed = test::call_service(
        &app,
        request(
            "http://localhost:9883/tokens",
            "localhost:9883",
            false,
            body("n1"),
        ),
    )
    .await;
    assert_eq!(replayed.status(), 401, "signed for another host");

    let configured = test::call_service(
        &app,
        request(
            "https://atomic.zay.home/tokens",
            "atomic-server.home-lab.svc.cluster.local:9883",
            true,
            body("n2"),
        ),
    )
    .await;
    assert_eq!(configured.status(), 201, "signed for this node's own host");
}
