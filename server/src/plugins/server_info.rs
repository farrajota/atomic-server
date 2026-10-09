//! Endpoint: describes the node you are talking to.
//!
//! This replaces the ad-hoc `/node-info` and `/iroh-node-id` JSON handlers. Those
//! returned bespoke JSON shapes that only our own data-browser could read, and
//! that nothing in the ontology described. A node is a thing worth naming, so it
//! is a [`urls::SERVER`] resource like anything else: self-describing, readable
//! by any Atomic client, and reachable through the normal store.
//!
//! The `managed` / `portalUrl` values are owned by [`crate::appstate::AppState`],
//! so the handler closes over them at registration time.

use std::sync::{atomic::AtomicBool, Arc, RwLock};

use atomic_lib::{
    endpoints::{BoxFuture, Endpoint, HandleGetContext},
    errors::AtomicResult,
    storelike::ResourceResponse,
    urls, Db, Resource, Value,
};

type PeerState = (
    String,
    Option<String>,
    bool,
    Option<i64>,
    Option<u32>,
    Option<u32>,
);

/// The node facts that live in AppState rather than in the store.
#[derive(Clone)]
pub struct ServerInfo {
    /// True if this node reports to a control plane. Flipped by the managed-node
    /// wrapper via `serve_with_hook`; the open server is always self-hosted.
    pub managed: Arc<AtomicBool>,
    /// The portal a managed node is administered from, learned from the control plane.
    pub managed_dashboard_url: Arc<RwLock<Option<String>>>,
    /// The Drive served as this server's front page (`ATOMIC_HOME_DRIVE`), if any.
    ///
    /// The browser learns this from the injected HTML rather than from here —
    /// it must know before the first render. This is for every other client:
    /// the desktop app, the SaaS portal, scripts asking "what does `/` show?".
    pub home_drive: Option<String>,
    /// Who may create a new Drive here. The welcome screen reads this to decide
    /// whether offering "create account" would be offering something that
    /// works.
    pub host_mode: crate::host_mode::HostModeConfig,
}

pub fn server_info_endpoint(info: ServerInfo) -> Endpoint {
    Endpoint::builder("/server")
        .description(
            "Describes this server node: its version, peer-to-peer node ID, and whether it is managed.",
        )
        .handle(move |context| handle_get(context, info.clone()))
        .build()
}

/// The devices this node syncs with directly, as nested resources.
///
/// A client is not a node: a browser tab cannot see who its server is paired
/// with, and so cannot show the phone that just paired with it. The node knows
/// — it logged the introduction — so it says.
///
/// Both the peers connected right now and the ones this node would dial again.
/// The union matters: a device that dials *us* is never written to the
/// known-peers table (F9 — an unsolicited connection has not earned a
/// permanent reconnect slot), so a phone that just paired appears only in the
/// live list. Listing known peers alone would answer "nobody is here" to the
/// person watching their phone connect.
///
/// Node-local state, not resources in a drive, so it is built per request.
fn peer_resources(store: &Db) -> Vec<atomic_lib::values::SubResource> {
    let live = atomic_lib::sync::peer::live_peer_ids();
    let known = atomic_lib::sync::peer::get_known_peers(store);

    // (node id, name, live, last synced). `last_synced` is already tracked on
    // the stored peer; reporting it is what lets a device card say when it last
    // exchanged anything instead of only whether a socket is open. "Connected"
    // alone cannot distinguish a healthy link from one that has moved nothing.
    let mut seen: Vec<PeerState> = Vec::new();

    let stored_for = |id: &str| -> Option<&atomic_lib::sync::peer::KnownPeer> {
        known
            .iter()
            .find(|p| atomic_lib::sync::peer::normalize_node_id(&p.node_id) == id)
    };

    for id in &live {
        let name = atomic_lib::sync::peer::live_peer_name(id).or_else(|| {
            known
                .iter()
                .find(|p| atomic_lib::sync::peer::normalize_node_id(&p.node_id) == *id)
                .map(|p| p.name.clone())
                .filter(|n| !n.is_empty())
        });
        let stored = stored_for(id);
        seen.push((
            id.clone(),
            name,
            true,
            stored.and_then(|p| p.last_synced),
            stored.and_then(|p| p.last_sent),
            stored.and_then(|p| p.last_received),
        ));
    }

    for peer in &known {
        let id = atomic_lib::sync::peer::normalize_node_id(&peer.node_id);

        if seen.iter().any(|(known_id, ..)| known_id == &id) {
            continue;
        }

        let name = Some(peer.name.clone()).filter(|n| !n.is_empty());
        seen.push((
            id,
            name,
            false,
            peer.last_synced,
            peer.last_sent,
            peer.last_received,
        ));
    }

    seen.into_iter()
        .map(
            |(node_id, name, is_live, last_synced, last_sent, last_received)| {
                let mut propvals = atomic_lib::resources::PropVals::new();
                propvals.insert(urls::IS_A.into(), vec![urls::PEER.to_string()].into());
                propvals.insert(
                    urls::PEER_NODE_ID.into(),
                    Value::String(atomic_lib::identifiers::node_subject(&node_id)),
                );

                if let Some(name) = name {
                    propvals.insert(urls::PEER_DEVICE_NAME.into(), Value::String(name));
                }

                propvals.insert(urls::PEER_LIVE.into(), Value::Boolean(is_live));

                if let Some(last_synced) = last_synced {
                    propvals.insert(urls::PEER_LAST_SEEN.into(), Value::Timestamp(last_synced));
                }

                // What the last sync moved, each way. A timestamp plus
                // "Connected" still cannot distinguish a link carrying data
                // from one being refused every subject; these can.
                if let Some(sent) = last_sent {
                    propvals.insert(urls::PEER_LAST_SENT.into(), Value::Integer(sent as i64));
                }

                if let Some(received) = last_received {
                    propvals.insert(
                        urls::PEER_LAST_RECEIVED.into(),
                        Value::Integer(received as i64),
                    );
                }

                atomic_lib::values::SubResource::Nested(propvals)
            },
        )
        .collect()
}

fn handle_get(
    context: HandleGetContext<'_>,
    info: ServerInfo,
) -> BoxFuture<'_, AtomicResult<ResourceResponse>> {
    Box::pin(async move {
        let can_create_drives = match context.for_agent {
            atomic_lib::agents::ForAgent::Sudo => true,
            atomic_lib::agents::ForAgent::Public => !info.host_mode.is_owner_mode(),
            atomic_lib::agents::ForAgent::AgentSubject(agent) => {
                !info.host_mode.is_owner_mode() || info.host_mode.is_owner(agent.as_str())
            }
        };
        let HandleGetContext { subject, .. } = context;

        // `set_unsafe` rather than `set`: the latter validates each value against
        // the Property resource in the store, and a store seeded before these
        // properties existed doesn't have them — so a node would answer "who are
        // you?" with a 500 until someone ran ATOMIC_REPOPULATE_DEFAULTS. The
        // datatypes are known statically here, and this resource is synthesized
        // per request and never committed, so there is nothing to validate against.
        let mut resource = Resource::new(subject.to_string());
        resource.set_unsafe(urls::IS_A.into(), vec![urls::SERVER.to_string()].into())?;
        resource.set_unsafe(
            urls::SERVER_VERSION.into(),
            Value::String(env!("CARGO_PKG_VERSION").into()),
        )?;

        // A gated node's device list is an inventory of the owner's machines and
        // a set of dial targets. The owner's own session still sees it — that is
        // what the Sync page and the pairing QR are built on — but a stranger
        // who can reach the origin has no business enumerating it.
        //
        // This hides a listing, it does not stand in for a check: someone who
        // learns the node ID elsewhere can still attempt a connection, and gets
        // refused by admission rather than by obscurity.
        let hide_node_inventory = info.host_mode.is_owner_mode()
            && matches!(context.for_agent, atomic_lib::agents::ForAgent::Public);

        if !hide_node_inventory {
            // Absent rather than null: a node with no p2p transport has no node ID.
            if let Some(node_id) = atomic_lib::sync::peer::get_node_id() {
                resource.set_unsafe(
                    urls::SERVER_NODE_ID.into(),
                    Value::String(atomic_lib::identifiers::node_subject(node_id)),
                )?;
            }

            let peers = peer_resources(context.store);

            if !peers.is_empty() {
                resource.set_unsafe(urls::SERVER_PEERS.into(), Value::ResourceArray(peers))?;
            }
        }

        // Said plainly and unconditionally, including on an open node: a client
        // that cannot tell "open" from "too old to say" would have to guess, and
        // the safe guess (assume gated) would hide account creation on every
        // node that predates this.
        resource.set_unsafe(
            urls::SERVER_HOST_MODE.into(),
            Value::String(info.host_mode.mode.as_str().to_string()),
        )?;
        resource.set_unsafe(
            urls::SERVER_ACCEPTS_NEW_DRIVES.into(),
            Value::Boolean(info.host_mode.mode.accepts_new_drives()),
        )?;
        resource.set_unsafe(
            urls::SERVER_OWNER_SET.into(),
            Value::Boolean(info.host_mode.owner_agent.is_some()),
        )?;
        resource.set_unsafe(
            urls::SERVER_CAN_CREATE_DRIVES.into(),
            Value::Boolean(can_create_drives),
        )?;

        let managed = info.managed.load(std::sync::atomic::Ordering::Relaxed);
        resource.set_unsafe(urls::SERVER_MANAGED.into(), Value::Boolean(managed))?;

        let portal_url = info
            .managed_dashboard_url
            .read()
            .ok()
            .and_then(|guard| guard.clone());

        if let Some(portal_url) = portal_url {
            resource.set_unsafe(urls::SERVER_PORTAL_URL.into(), Value::String(portal_url))?;
        }

        if let Some(home_drive) = info.home_drive.clone() {
            resource.set_unsafe(
                urls::SERVER_HOME_DRIVE.into(),
                Value::AtomicUrl(home_drive.into()),
            )?;
        }

        Ok(resource.into())
    })
}
#[cfg(test)]
mod tests {
    use super::{server_info_endpoint, ServerInfo};
    use atomic_lib::{agents::ForAgent, endpoints::HandleGetContext, urls, Value};
    use std::sync::{atomic::AtomicBool, Arc, RwLock};

    const CAN_CREATE_DRIVES: &str = "https://atomicdata.dev/properties/server/canCreateDrives";
    const OWNER: &str = "atomic:agent:metadata-owner";
    const OWNER_ALIAS: &str = "did:ad:agent:metadata-owner";
    const NONOWNER: &str = "atomic:agent:metadata-nonowner";

    fn info(mode: crate::host_mode::HostMode, owner_agent: Option<&str>) -> ServerInfo {
        ServerInfo {
            managed: Arc::new(AtomicBool::new(false)),
            managed_dashboard_url: Arc::new(RwLock::new(None)),
            home_drive: None,
            host_mode: crate::host_mode::HostModeConfig {
                mode,
                owner_agent: owner_agent.map(str::to_owned),
            },
        }
    }

    async fn request(
        endpoint: &atomic_lib::endpoints::Endpoint,
        store: &atomic_lib::Db,
        for_agent: &ForAgent,
    ) -> atomic_lib::storelike::ResourceResponse {
        let context = HandleGetContext {
            subject: url::Url::parse("http://localhost/server").unwrap(),
            store,
            for_agent,
        };
        endpoint.handle.as_ref().unwrap()(context).await.unwrap()
    }

    fn capability(response: &atomic_lib::storelike::ResourceResponse) -> Option<bool> {
        match response.to_single().get(CAN_CREATE_DRIVES) {
            Ok(Value::Boolean(value)) => Some(*value),
            _ => None,
        }
    }

    #[actix_rt::test]
    async fn server_metadata_reports_caller_specific_drive_creation() {
        let store = atomic_lib::Db::init_temp(&atomic_lib::utils::random_string(12))
            .await
            .unwrap();
        let open = server_info_endpoint(info(crate::host_mode::HostMode::Open, None));
        let public = request(&open, &store, &ForAgent::Public).await;
        assert_eq!(capability(&public), Some(true));
        assert!(matches!(
            public.to_single().get(urls::SERVER_ACCEPTS_NEW_DRIVES),
            Ok(Value::Boolean(true))
        ));
        for for_agent in [
            ForAgent::from(OWNER),
            ForAgent::from(OWNER_ALIAS),
            ForAgent::from(NONOWNER),
            ForAgent::Sudo,
        ] {
            let response = request(&open, &store, &for_agent).await;
            assert_eq!(capability(&response), Some(true));
        }

        let owner_host = server_info_endpoint(info(crate::host_mode::HostMode::Owner, Some(OWNER)));
        let callers = [
            (ForAgent::Public, false),
            (ForAgent::from(OWNER), true),
            (ForAgent::from(OWNER_ALIAS), true),
            (ForAgent::from(NONOWNER), false),
            (ForAgent::Sudo, true),
        ];

        for (for_agent, expected) in callers {
            let response = request(&owner_host, &store, &for_agent).await;
            assert_eq!(capability(&response), Some(expected));
            assert!(matches!(
                response.to_single().get(urls::SERVER_ACCEPTS_NEW_DRIVES),
                Ok(Value::Boolean(false))
            ));
            assert!(matches!(
                response.to_single().get(urls::SERVER_OWNER_SET),
                Ok(Value::Boolean(true))
            ));
        }
    }
}
