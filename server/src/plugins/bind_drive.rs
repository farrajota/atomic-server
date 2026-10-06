use atomic_lib::{
    agents::ForAgent,
    endpoints::{BoxFuture, Endpoint, HandlePostContext},
    errors::{AtomicError, AtomicResult},
    hierarchy::check_write,
    storelike::ResourceResponse,
    urls, Resource, Storelike, Value,
};
use serde::Deserialize;

#[derive(Deserialize)]
struct BindDriveRequest {
    #[serde(rename = "https://atomicdata.dev/properties/initialDrive")]
    drive: String,
}

/// `server_owner` is the node's `ATOMIC_OWNER_AGENT`, if any.
pub fn bind_drive_endpoint(server_owner: Option<String>) -> Endpoint {
    Endpoint::builder("/bind-drive")
        .params([urls::SETUP_RESET])
        .description("Binds the current host to a Drive DID, routing all requests on this domain to that drive.")
        .handle_post(move |context| handle_bind_drive_request(context, server_owner.clone()))
        .build()
}

fn handle_bind_drive_request<'a>(
    context: HandlePostContext<'a>,
    server_owner: Option<String>,
) -> BoxFuture<'a, AtomicResult<ResourceResponse>> {
    Box::pin(async move {
        let HandlePostContext {
            store,
            body,
            subject,
            for_agent,
        } = context;

        let host = subject.host_str().unwrap_or("localhost");

        // Binding is first come, first served: any agent can mint a drive
        // and bind an unbound host to it, after which only that drive's
        // writers could change it. The server owner can always take a host
        // back, also when the bound drive resource is gone and its writers
        // can no longer be checked.
        let is_server_owner = is_server_owner(server_owner.as_deref(), for_agent);

        // ?reset clears the drive mapping for this host. Unbinding takes the
        // drive off this host just as surely as rebinding does, so it needs
        // the same right: write on the drive currently bound. With nothing
        // bound there is nothing to protect, and reset is a no-op.
        let is_reset = subject.query_pairs().any(|(k, _)| k == "reset");

        if is_reset {
            if let Some(current_drive) = store.get_drive_did(host).await? {
                if !is_server_owner {
                    let unauthorized = || {
                        AtomicError::unauthorized(
                            "Only the server owner or agents with write access to the drive bound to this host can unbind it."
                                .into(),
                        )
                    };
                    let drive_resource = store
                        .get_resource(&current_drive)
                        .await
                        .map_err(|_| unauthorized())?;
                    check_write(store, &drive_resource, for_agent)
                        .await
                        .map_err(|_| unauthorized())?;
                }
                store.remove_drive_mapping(host)?;
            }
            let root = store
                .get_resource(&"internal:/".into())
                .await
                .unwrap_or_else(|_| Resource::new("internal:/".into()));
            return Ok(root.into());
        }

        // If the host is already bound, only allow rebinding if the caller has
        // write rights on the current drive.
        if let Some(current_drive) = store.get_drive_did(host).await? {
            if !is_server_owner {
                let unauthorized = || {
                    AtomicError::unauthorized(
                        "This host is already bound to a drive. Only the server owner or agents with write access to the current drive can rebind it."
                            .into(),
                    )
                };
                let drive_resource = store
                    .get_resource(&current_drive)
                    .await
                    .map_err(|_| unauthorized())?;
                check_write(store, &drive_resource, for_agent)
                    .await
                    .map_err(|_| unauthorized())?;
            }
        }

        let request: BindDriveRequest =
            serde_json::from_slice(&body).map_err(|e| format!("Failed to parse request: {}", e))?;

        // Binding a host to a drive must never be possible for a caller with no
        // relationship to that drive. An unbound host previously had NO check at
        // all here, so an unauthenticated caller could claim a fresh host and
        // point it at any drive DID — including one the legitimate operator can
        // never write to, which would permanently lock them out of re-binding it
        // via this endpoint (rebinding above requires write on the CURRENT drive).
        let target_drive = store
            .get_resource(&request.drive.clone().into())
            .await
            .map_err(|_| format!("Drive not found: {}", request.drive))?;
        check_write(store, &target_drive, for_agent)
            .await
            .map_err(|_| "You need write access to the drive you're binding this host to.")?;

        // The mapping is the routing source of truth: requests on this host now
        // resolve `/` (and drive-relative paths) to the bound drive.
        store.add_drive_mapping(host, &Value::AtomicUrl(request.drive.into()))?;

        Ok(target_drive.into())
    })
}

/// Whether `for_agent` is the node's configured owner (`ATOMIC_OWNER_AGENT`).
fn is_server_owner(server_owner: Option<&str>, for_agent: &ForAgent) -> bool {
    let (Some(owner), ForAgent::AgentSubject(agent)) = (server_owner, for_agent) else {
        return false;
    };
    atomic_lib::identifiers::canonicalize_scheme(owner)
        == atomic_lib::identifiers::canonicalize_scheme(agent.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_lib::{Db, Subject};

    fn bind_body(drive: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "https://atomicdata.dev/properties/initialDrive": drive
        }))
        .unwrap()
    }

    async fn call(
        store: &Db,
        url: &str,
        body: Vec<u8>,
        for_agent: &ForAgent,
    ) -> AtomicResult<ResourceResponse> {
        call_on_node_owned_by(store, None, url, body, for_agent).await
    }

    /// `call` on a node whose `ATOMIC_OWNER_AGENT` is `server_owner`.
    async fn call_on_node_owned_by(
        store: &Db,
        server_owner: Option<&str>,
        url: &str,
        body: Vec<u8>,
        for_agent: &ForAgent,
    ) -> AtomicResult<ResourceResponse> {
        handle_bind_drive_request(
            HandlePostContext {
                subject: url::Url::parse(url).unwrap(),
                store,
                for_agent,
                body,
            },
            server_owner.map(str::to_string),
        )
        .await
    }

    /// A drive minted by `agent` through a signed, rights-checked commit.
    async fn drive_of(store: &Db, agent: &atomic_lib::agents::Agent) -> String {
        let mut builder = atomic_lib::commit::CommitBuilder::new("placeholder".into());
        builder.set(
            urls::IS_A.into(),
            Value::ResourceArray(vec![urls::DRIVE.to_string().into()]),
        );
        let commit = atomic_lib::Commit::create_did(builder, agent, store)
            .await
            .unwrap();
        let opts = atomic_lib::commit::CommitOpts {
            validate_signature: true,
            validate_rights: true,
            validate_for_agent: Some(agent.subject.to_string()),
            update_index: true,
            ..atomic_lib::commit::CommitOpts::no_validations_no_index()
        };
        store
            .apply_commit(commit, &opts)
            .await
            .unwrap()
            .resource_new
            .unwrap()
            .get_subject()
            .to_string()
    }

    async fn bound(store: &Db, host: &str) -> Option<String> {
        store
            .get_drive_did(host)
            .await
            .unwrap()
            .map(|d| d.to_string())
    }

    #[tokio::test]
    async fn binds_host_and_routes_root_to_drive() {
        let store = Db::init_temp("setup_endpoint_bind").await.unwrap();
        let (agent, drive) = store.setup("Setup Tester").await.unwrap();
        let for_agent = ForAgent::AgentSubject(agent.subject.clone());

        // An anonymous caller must not be able to claim an unbound host.
        let denied = call(
            &store,
            "http://example.com/bind-drive",
            bind_body(&drive),
            &ForAgent::Public,
        )
        .await;
        assert!(denied.is_err());
        assert!(store.get_drive_did("example.com").await.unwrap().is_none());

        // An agent with write access on the drive binds the host.
        call(
            &store,
            "http://example.com/bind-drive",
            bind_body(&drive),
            &for_agent,
        )
        .await
        .unwrap();
        let bound = store.get_drive_did("example.com").await.unwrap().unwrap();
        assert_eq!(bound.as_str(), drive);

        // Root on the bound host now resolves to the drive.
        let resolved = store
            .resolve_request_target(
                &Subject::from_raw("/", None),
                "example.com",
                "/",
                "http://example.com",
            )
            .await
            .unwrap();
        assert_eq!(resolved.subject.as_str(), drive);

        // ?reset unbinds the host again.
        call(
            &store,
            "http://example.com/bind-drive?reset=true",
            Vec::new(),
            &for_agent,
        )
        .await
        .unwrap();
        assert!(store.get_drive_did("example.com").await.unwrap().is_none());
    }

    /// `?reset` unbinds a host, which is as consequential as rebinding it:
    /// the host stops serving the drive. It needs the same right, write on
    /// the drive currently bound, and a refusal is an authorization error.
    #[tokio::test]
    async fn reset_requires_write_on_the_bound_drive() {
        let store = Db::init_temp("bind_drive_reset_rights").await.unwrap();
        let (owner, drive) = store.setup("Owner").await.unwrap();
        let owner = ForAgent::AgentSubject(owner.subject.clone());
        call(
            &store,
            "http://example.com/bind-drive",
            bind_body(&drive),
            &owner,
        )
        .await
        .unwrap();

        let stranger = store.create_agent(Some("Stranger")).await.unwrap();
        for caller in [ForAgent::Public, ForAgent::AgentSubject(stranger.subject)] {
            let err = call(
                &store,
                "http://example.com/bind-drive?reset=true",
                Vec::new(),
                &caller,
            )
            .await
            .err()
            .unwrap_or_else(|| panic!("{caller} must not unbind the host"));
            assert!(
                matches!(
                    err.error_type,
                    atomic_lib::AtomicErrorType::UnauthorizedError
                ),
                "a refused reset is an authorization error, got: {err}"
            );
            assert_eq!(
                store
                    .get_drive_did("example.com")
                    .await
                    .unwrap()
                    .map(|d| d.to_string()),
                Some(drive.clone()),
                "a refused reset must leave the binding in place"
            );
        }

        call(
            &store,
            "http://example.com/bind-drive?reset=true",
            Vec::new(),
            &owner,
        )
        .await
        .expect("a writer of the bound drive may unbind the host");
        assert!(store.get_drive_did("example.com").await.unwrap().is_none());
    }

    /// With nothing bound there is nothing to protect: reset is a no-op that
    /// succeeds for anyone and binds nothing.
    #[tokio::test]
    async fn reset_on_an_unbound_host_is_a_no_op() {
        let store = Db::init_temp("bind_drive_reset_unbound").await.unwrap();
        store.setup("Owner").await.unwrap();

        call(
            &store,
            "http://unbound.example/bind-drive?reset=true",
            Vec::new(),
            &ForAgent::Public,
        )
        .await
        .expect("resetting an unbound host is a no-op");
        assert!(store
            .get_drive_did("unbound.example")
            .await
            .unwrap()
            .is_none());
    }

    /// First come, first bound: any registered agent can mint a drive and
    /// bind an unbound host to it, after which only that drive's writers
    /// could unbind or rebind it. The server owner must be able to take the
    /// host back, by resetting or by rebinding it to one of their drives.
    #[tokio::test]
    async fn the_server_owner_may_reset_or_rebind_any_host() {
        let store = Db::init_temp("bind_drive_owner_override").await.unwrap();
        // The node's own agent has rights of its own; the owner is a person.
        store.setup("Node").await.unwrap();
        let operator = store.create_agent(Some("Operator")).await.unwrap();
        let operator_drive = drive_of(&store, &operator).await;
        let owner = operator.subject.to_string();
        let operator = ForAgent::AgentSubject(operator.subject.clone());
        let squatter = store.create_agent(Some("Squatter")).await.unwrap();
        let squatter_drive = drive_of(&store, &squatter).await;
        let squatter = ForAgent::AgentSubject(squatter.subject.clone());
        let host = "http://kb.tailnet.example/bind-drive";

        let squat = || async {
            call_on_node_owned_by(
                &store,
                Some(&owner),
                host,
                bind_body(&squatter_drive),
                &squatter,
            )
            .await
            .expect("an unbound host goes to whoever binds it first");
        };

        squat().await;
        call_on_node_owned_by(
            &store,
            None,
            &format!("{host}?reset=true"),
            Vec::new(),
            &operator,
        )
        .await
        .err()
        .expect("without an owner configured, only the bound drive's writers may reset");
        call_on_node_owned_by(
            &store,
            Some(&owner),
            &format!("{host}?reset=true"),
            Vec::new(),
            &operator,
        )
        .await
        .expect("the server owner may reset any binding");
        assert_eq!(bound(&store, "kb.tailnet.example").await, None);

        squat().await;
        call_on_node_owned_by(
            &store,
            Some(&owner),
            host,
            bind_body(&operator_drive),
            &operator,
        )
        .await
        .expect("the server owner may rebind any binding to a drive they write");
        assert_eq!(
            bound(&store, "kb.tailnet.example").await,
            Some(operator_drive)
        );
    }

    /// A binding whose drive resource is gone cannot be checked against that
    /// drive's writers. Only the server owner may clear it; everyone else
    /// gets an authorization error rather than an internal one.
    #[tokio::test]
    async fn only_the_server_owner_may_reset_a_binding_to_a_missing_drive() {
        let store = Db::init_temp("bind_drive_missing_drive").await.unwrap();
        store.setup("Node").await.unwrap();
        let operator = store.create_agent(Some("Operator")).await.unwrap();
        let owner = operator.subject.to_string();
        let operator = ForAgent::AgentSubject(operator.subject.clone());
        let stranger = store.create_agent(Some("Stranger")).await.unwrap();
        // A drive id that names no resource here: the commit is never applied.
        let mut never_applied = atomic_lib::commit::CommitBuilder::new("placeholder".into());
        never_applied.set(urls::NAME.into(), Value::String("gone".into()));
        let gone = atomic_lib::Commit::create_did(never_applied, &stranger, &store)
            .await
            .unwrap()
            .subject
            .to_string();
        store
            .add_drive_mapping("lost.example", &Value::AtomicUrl(gone.as_str().into()))
            .unwrap();
        let reset = "http://lost.example/bind-drive?reset=true";

        let err = call_on_node_owned_by(
            &store,
            Some(&owner),
            reset,
            Vec::new(),
            &ForAgent::AgentSubject(stranger.subject),
        )
        .await
        .err()
        .expect("a stranger must not clear a binding nobody can check");
        assert!(
            matches!(
                err.error_type,
                atomic_lib::AtomicErrorType::UnauthorizedError
            ),
            "got: {err}"
        );
        assert!(bound(&store, "lost.example").await.is_some());

        call_on_node_owned_by(&store, Some(&owner), reset, Vec::new(), &operator)
            .await
            .expect("the server owner may clear a binding whose drive is gone");
        assert_eq!(bound(&store, "lost.example").await, None);
    }
}
