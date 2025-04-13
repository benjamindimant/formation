use crdts::{bft_reg::Update, BFTReg, Map};
use crate::db::{store_map, write_datastore};
use reqwest::Client;
use crate::datastore::{DataStore, PeerRequest, CidrRequest, DnsRequest, AssocRequest, DB_HANDLE, InstanceRequest}; 
use crate::network::{NetworkState, CrdtPeer, CrdtCidr, CrdtAssociation, CrdtDnsRecord};
use form_types::state::{Response, Success};
use serde::{Serialize, Deserialize};
use axum::{extract::{State, Path}, Json};
use std::sync::Arc;
use tokio::sync::Mutex;
use form_dns::{store::FormDnsRecord, api::{DomainResponse, DomainRequest}};
use trust_dns_proto::rr::RecordType;
use std::net::SocketAddr;
use std::net::IpAddr;
use url::Host;
use crate::instances::Instance;
use shared::{Cidr, Association, Peer};
use std::time::{SystemTime, UNIX_EPOCH};

pub type PeerMap = Map<String, BFTReg<CrdtPeer<String>, String>, String>;
pub type CidrMap = Map<String, BFTReg<CrdtCidr<String>, String>, String>;
pub type AssocMap = Map<String, BFTReg<CrdtAssociation<String>, String>, String>;
pub type DnsMap = Map<String, BFTReg<CrdtDnsRecord, String>, String>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeableNetworkState {
    peers: PeerMap,
    cidrs: CidrMap,
    assocs: AssocMap,
    dns: DnsMap,
}

impl From<NetworkState> for MergeableNetworkState {
    fn from(value: NetworkState) -> Self {
        MergeableNetworkState {
            peers: value.peers.clone(),
            cidrs: value.cidrs.clone(),
            assocs: value.associations.clone(),
            dns: value.dns_state.zones.clone()
        }
    }
}

pub async fn network_state(
    State(state): State<Arc<Mutex<DataStore>>>,
) -> Json<MergeableNetworkState> {
    log::info!("Received network state request, returning...");
    let network_state = state.lock().await.network_state.clone();
    Json(network_state.into())
}

pub async fn peer_state(
    State(state): State<Arc<Mutex<DataStore>>>, 
) -> Json<PeerMap> {
    log::info!("Received peer state request, returning...");
    let peer_state = state.lock().await.network_state.peers.clone();
    Json(peer_state)
}

pub async fn request_netwok_state(to_dial: String) -> Result<MergeableNetworkState, Box<dyn std::error::Error>> {
    let resp = Client::new()
        .get(format!("http://{to_dial}:3004/bootstrap/network_state"))
        .send().await?.json().await?;
    Ok(resp)
}

pub async fn request_peer_state(to_dial: String) -> Result<PeerMap, Box<dyn std::error::Error>> {
    let resp = Client::new()
        .get(format!("http://{to_dial}:3004/bootstrap/peer_state"))
        .send().await?.json().await?;
    Ok(resp)
}

pub async fn request_cidr_state(to_dial: String) -> Result<CidrMap, Box<dyn std::error::Error>> {
    let resp = Client::new()
        .get(format!("http://{to_dial}:3004/bootstrap/cidr_state"))
        .send().await?.json().await?;

    Ok(resp)
}

pub async fn request_associations_state(to_dial: String) -> Result<AssocMap, Box<dyn std::error::Error>> {
    let resp = Client::new()
        .get(format!("http://{to_dial}:3004/bootstrap/assoc_state"))
        .send().await?.json().await?;

    Ok(resp)
}

pub async fn cidr_state(
    State(state): State<Arc<Mutex<DataStore>>>, 
) -> Json<CidrMap> {
    log::info!("Received cidr state request, returning...");
    let cidr_state = state.lock().await.network_state.cidrs.clone();
    Json(cidr_state)
}

pub async fn assoc_state(
    State(state): State<Arc<Mutex<DataStore>>>, 
) -> Json<AssocMap> {
    log::info!("Received assoc state request, returning...");
    let assoc_state = state.lock().await.network_state.associations.clone();
    Json(assoc_state)
}

pub async fn create_user(
    State(state): State<Arc<Mutex<DataStore>>>,
    Json(user): Json<PeerRequest>
) -> Json<Response<Peer<String>>> {
    log::info!("Received create user request...");
    let mut datastore = state.lock().await;
    match user {
        PeerRequest::Op(map_op) => {
            log::info!("Create user request is an Op from another peer");
            match &map_op {
                crdts::map::Op::Up { ref key, ref op, .. } => {
                    datastore.network_state.peer_op(map_op.clone());
                    if let (true, v) = datastore.network_state.peer_op_success(key.clone(), op.clone()) {
                        log::info!("Peer Op succesffully applied...");
                        let _ = write_datastore(&DB_HANDLE, &datastore.clone());
                        return Json(Response::Success(Success::Some(v.into())))
                    } else {
                        log::info!("Peer Op rejected...");
                        return Json(Response::Failure { reason: Some("update was rejected".to_string()) })
                    }
                }
                crdts::map::Op::Rm { .. } => {
                    return Json(Response::Failure { reason: Some("Invalid Op type for Create User".into()) });
                }
            }
        }
        PeerRequest::Join(contents) => {
            log::info!("Create user request was a direct request...");
            log::info!("Building Map Op...");
            let map_op = datastore.network_state.update_peer_local(contents);
            log::info!("Map op created... Applying...");
            datastore.network_state.peer_op(map_op.clone());
            match &map_op {
                crdts::map::Op::Rm { .. } => {
                    return Json(Response::Failure { reason: Some("Map generated RM context instead of Add context on Join request".to_string()) });
                }
                crdts::map::Op::Up { ref key, ref op, .. } => {
                    if let (true, v) = datastore.network_state.peer_op_success(key.clone(), op.clone()) {
                        log::info!("Map Op was successful, broadcasting...");
                        let request = PeerRequest::Op(map_op);
                        match datastore.broadcast::<Response<Peer<String>>>(request, "/user/create").await {
                            Ok(()) => {
                                let _ = write_datastore(&DB_HANDLE, &datastore.clone());
                                return Json(Response::Success(Success::Some(v.into())))
                            }
                            Err(e) => eprintln!("Error broadcasting DeletePeerRequest: {e}")
                        }
                        return Json(Response::Success(Success::Some(v.into())))
                    } else {
                        return Json(Response::Failure { reason: Some("update was rejected".to_string()) })
                    }
                }
            }
        }
        _ => {
            return Json(Response::Failure { reason: Some("Invalid request for create user".into()) });
        }
    }
}

pub async fn update_user(
    State(state): State<Arc<Mutex<DataStore>>>,
    Json(user): Json<PeerRequest>
) -> Json<Response<Peer<String>>> {
    log::info!("Received update user request...");
    let mut datastore = state.lock().await;
    match user {
        PeerRequest::Op(map_op) => {
            log::info!("Update user request is an Op from another peer");
            match &map_op {
                crdts::map::Op::Up { ref key, ref op, .. } => {
                    datastore.network_state.peer_op(map_op.clone());
                    if let (true, v) = datastore.network_state.peer_op_success(key.clone(), op.clone()) {
                        let _ = write_datastore(&DB_HANDLE, &datastore.clone());
                        return Json(Response::Success(Success::Some(v.into())))
                    } else {
                        return Json(Response::Failure { reason: Some("update was rejected".to_string()) })
                    }
                }
                crdts::map::Op::Rm { .. } => {
                    return Json(Response::Failure { reason: Some("Invalid Op type for Create User".into()) });
                }
            }
        }
        PeerRequest::Update(contents) => {
            log::info!("Update user request was a direct request...");
            log::info!("Building Map Op...");
            let ctx = datastore.network_state.peers.get(&contents.name.to_string().clone());
            let mut existing_ip = None;
            if let Some(existing) = ctx.val {
                if let Some(peer) = existing.val() {
                    if let Some(endpoint) = peer.value().endpoint {
                        if let Ok(resolved) = endpoint.resolve() {
                            existing_ip = Some(resolved.ip())
                        }
                    }
                }
            };
            let map_op = datastore.network_state.update_peer_local(contents.clone());
            datastore.network_state.peer_op(map_op.clone());
            match &map_op {
                crdts::map::Op::Rm { .. } => {
                    return Json(Response::Failure { reason: Some("Map generated RM context instead of Add context on Join request".to_string()) });
                }
                crdts::map::Op::Up { ref key, ref op, .. } => {
                    if let (true, v) = datastore.network_state.peer_op_success(key.clone(), op.clone()) {
                        log::info!("Map Op was successful, broadcasting...");
                        let request = PeerRequest::Op(map_op);
                        match datastore.broadcast::<Response<Peer<String>>>(request, "/user/update").await {
                            Ok(()) => {
                                let _ = write_datastore(&DB_HANDLE, &datastore.clone());
                                return Json(Response::Success(Success::Some(v.into())));
                            }
                            Err(e) => eprintln!("Error broadcasting DeletePeerRequest: {e}")
                        }
                        drop(datastore);
                        if let Some(existing) = existing_ip {
                            if contents.is_admin {
                                if let Some(endpoint) = contents.endpoint {
                                    if let Ok(resolved) = endpoint.resolve() {
                                        if resolved.ip() == existing {
                                            let _ = update_dns_ip_addr(
                                                state.clone(),
                                                existing.to_string(),
                                                resolved.ip().to_string(),
                                            ).await;
                                        }
                                    }
                                }
                            }
                        }
                        return Json(Response::Success(Success::Some(v.into())))
                    } else {
                        return Json(Response::Failure { reason: Some("update was rejected".to_string()) })
                    }
                }
            }
        }
        _ => {
            return Json(Response::Failure { reason: Some("Invalid request for update user".into()) });
        }
    }
}

pub async fn disable_user(
    State(state): State<Arc<Mutex<DataStore>>>,
    Json(user): Json<PeerRequest>
) -> Json<Response<Peer<String>>> {
    log::info!("Received disable user request...");
    let mut datastore = state.lock().await;
    match user {
        PeerRequest::Op(map_op) => {
            log::info!("Disable user request is an Op from another peer");
            match &map_op {
                crdts::map::Op::Up { ref key, ref op, .. } => {
                    datastore.network_state.peer_op(map_op.clone());
                    if let (true, v) = datastore.network_state.peer_op_success(key.clone(), op.clone()) {
                        log::info!("Map Op was successful, broadcasting...");
                        let _ = write_datastore(&DB_HANDLE, &datastore.clone());
                        return Json(Response::Success(Success::Some(v.into())))
                    } else {
                        return Json(Response::Failure { reason: Some("update was rejected".to_string()) })
                    }
                }
                crdts::map::Op::Rm { .. } => {
                    return Json(Response::Failure { reason: Some("Invalid Op type for Create User".into()) });
                }
            }
        }
        PeerRequest::Update(contents) => {
            log::info!("Disable user request was a direct request...");
            log::info!("Building Map Op...");
            let map_op = datastore.network_state.update_peer_local(contents);
            datastore.network_state.peer_op(map_op.clone());
            match &map_op {
                crdts::map::Op::Rm { .. } => {
                    return Json(Response::Failure { reason: Some("Map generated RM context instead of Add context on Join request".to_string()) });
                }
                crdts::map::Op::Up { ref key, ref op, .. } => {
                    if let (true, v) = datastore.network_state.peer_op_success(key.clone(), op.clone()) {
                        log::info!("Map Op was successful, broadcasting...");
                        let request = PeerRequest::Op(map_op);
                        match datastore.broadcast::<Response<Peer<String>>>(request, "/user/disable").await {
                            Ok(()) => {
                                let _ = write_datastore(&DB_HANDLE, &datastore.clone());
                                return Json(Response::Success(Success::Some(v.into())))
                            }
                            Err(e) => eprintln!("Error broadcasting DeletePeerRequest: {e}")
                        }
                        return Json(Response::Success(Success::Some(v.into())))
                    } else {
                        return Json(Response::Failure { reason: Some("update was rejected".to_string()) })
                    }
                }
            }
        }
        _ => {
            return Json(Response::Failure { reason: Some("Invalid request for update user".into()) });
        }
    }
}

pub async fn redeem_invite(
    State(state): State<Arc<Mutex<DataStore>>>,
    Json(user): Json<PeerRequest>
) -> Json<Response<Peer<String>>> {
    log::info!("Received redeem invite user request...");
    let mut datastore = state.lock().await;
    match user {
        PeerRequest::Op(map_op) => {
            log::info!("Redeem invite user request is an Op from another peer");
            match &map_op {
                crdts::map::Op::Up { ref key, ref op, .. } => {
                    datastore.network_state.peer_op(map_op.clone());
                    if let (true, v) = datastore.network_state.peer_op_success(key.clone(), op.clone()) {
                        let _ = write_datastore(&DB_HANDLE, &datastore.clone());
                        return Json(Response::Success(Success::Some(v.into())))
                    } else {
                        return Json(Response::Failure { reason: Some("update was rejected".to_string()) })
                    }
                }
                crdts::map::Op::Rm { .. } => {
                    return Json(Response::Failure { reason: Some("Invalid Op type for Create User".into()) });
                }
            }
        }
        PeerRequest::Update(contents) => {
            log::info!("Redeem invite user request was a direct request...");
            log::info!("Building Map Op...");
            let map_op = datastore.network_state.update_peer_local(contents);
            datastore.network_state.peer_op(map_op.clone());
            match &map_op {
                crdts::map::Op::Rm { .. } => {
                    return Json(Response::Failure { reason: Some("Map generated RM context instead of Add context on Join request".to_string()) });
                }
                crdts::map::Op::Up { ref key, ref op, .. } => {
                    if let (true, v) = datastore.network_state.peer_op_success(key.clone(), op.clone()) {
                        log::info!("Map Op was successful, broadcasting...");
                        let request = PeerRequest::Op(map_op);
                        match datastore.broadcast::<Response<Peer<String>>>(request, "/user/redeem").await {
                            Ok(()) => {
                                let _ = write_datastore(&DB_HANDLE, &datastore.clone());
                                return Json(Response::Success(Success::Some(v.into())))
                            }
                            Err(e) => eprintln!("Error broadcasting DeletePeerRequest: {e}")
                        }
                        return Json(Response::Success(Success::Some(v.into())))
                    } else {
                        return Json(Response::Failure { reason: Some("update was rejected".to_string()) })
                    }
                }
            }
        }
        _ => {
            return Json(Response::Failure { reason: Some("Invalid request for update user".into()) });
        }
    }
}

pub async fn delete_user(
    State(state): State<Arc<Mutex<DataStore>>>,
    Json(user): Json<PeerRequest>
) -> Json<Response<Peer<String>>> {
    let mut datastore = state.lock().await;
    match user {
        PeerRequest::Op(map_op) => {
            log::info!("delete user request is an Op from another peer");
            match &map_op {
                crdts::map::Op::Up { .. } => {
                    return Json(Response::Failure { reason: Some("Invalid Op type for delete User".into()) });
                }
                crdts::map::Op::Rm { .. } => {
                    datastore.network_state.peer_op(map_op);
                    return Json(Response::Success(Success::None));
                }
            }
        }
        PeerRequest::Delete(contents) => {
            log::info!("Delete user request was a direct request...");
            log::info!("Building Map Op...");
            let map_op = datastore.network_state.remove_peer_local(contents);
            datastore.network_state.peer_op(map_op.clone());
            match &map_op {
                crdts::map::Op::Rm { .. } => {
                    let request = PeerRequest::Op(map_op);
                    log::info!("Map Op was successful, broadcasting...");
                    match datastore.broadcast::<Response<Peer<String>>>(request, "/user/delete").await {
                        Ok(()) => return Json(Response::Success(Success::None)),
                        Err(e) => eprintln!("Error broadcasting DeletePeerRequest: {e}")
                    }
                    return Json(Response::Success(Success::None));
                }
                crdts::map::Op::Up { .. } => {
                    return Json(Response::Failure { reason: Some("Map generated Add context instead of Rm context on Delete request".to_string()) });
                }
            }
        }
        _ => {
            return Json(Response::Failure { reason: Some("Invalid request for update user".into()) });
        }
    }
}


pub async fn get_user(
    State(state): State<Arc<Mutex<DataStore>>>,
    Path(id): Path<String>
) -> Json<Response<Peer<String>>> {
    log::info!("Request to get peer {id}");
    if let Some(peer) = state.lock().await.network_state.peers.get(&id).val {
        log::info!("Found register for peer {id}");
        if let Some(val) = peer.val() {
            log::info!("Found value for peer {id}");
            return Json(Response::Success(Success::Some(val.value().into())))
        } else {
            return Json(Response::Failure { reason: Some("Entry exists, but value is None".into()) })
        }
    } 
    Json(Response::Failure { reason: Some("Entry does not exist in the CrdtMap".to_string()) })
}

pub async fn get_user_from_ip( 
    State(state): State<Arc<Mutex<DataStore>>>,
    Path(ip): Path<String>
) -> Json<Response<Peer<String>>> {
    log::info!("Request to get peer by IP {ip}");
    if let Some(peer) = state.lock().await.network_state.get_peer_by_ip(ip.clone()) {
        log::info!("Found peer {} by IP {ip}", peer.id());
        return Json(Response::Success(Success::Some(peer.into())))
    } 

    return Json(Response::Failure { reason: Some(format!("Peer with IP {ip} does not exist")) })
}

pub async fn get_all_allowed(
    State(state): State<Arc<Mutex<DataStore>>>,
    Path(id): Path<String>,
) -> Json<Response<Peer<String>>> {
    log::info!("Requesting all allowed peers for peer {id}");
    let mut peers = state.lock().await.get_all_users();
    if let Some(peer) = state.lock().await.network_state.peers.get(&id).val {
        if let Some(node) = peer.val() {
            let peer = node.value();
            let cidr = peer.cidr();
            peers.retain(|_, v| v.cidr() == cidr);
            let all_allowed: Vec<Peer<String>> = peers.iter().map(|(_, v)| v.clone().into()).collect();
            log::info!("Retrieved all allowed peers for peer {id}. Total {}", all_allowed.len());
            return Json(Response::Success(Success::List(all_allowed)))
        }
    } 

    return Json(Response::Failure { reason: Some("Peer for which allowed peers was requested does not exist".into()) })
}

pub async fn list_users(
    State(state): State<Arc<Mutex<DataStore>>>,
) -> Json<Response<Peer<String>>> {
    log::info!("Requesting a list of all users in the network...");
    let peers = state.lock().await.get_all_users().iter().map(|(_, v)| v.clone().into()).collect();
    log::info!("Retrieved a list of all users in the network... Returning...");
    Json(Response::Success(Success::List(peers)))
}

pub async fn list_admin(
    State(state): State<Arc<Mutex<DataStore>>>,
) -> Json<Response<Peer<String>>> {
    log::info!("Requesting a list of all users in the network...");
    let peers = state.lock().await.get_all_active_admin().iter().map(|(_, v)| v.clone().into()).collect();
    log::info!("Retrieved a list of all users in the network... Returning...");
    Json(Response::Success(Success::List(peers)))
}

pub async fn list_by_cidr(
    State(state): State<Arc<Mutex<DataStore>>>,
    Path(cidr): Path<String>
) -> Json<Response<Peer<String>>> {
    log::info!("Retrieving a list of all users in the cidr {cidr}...");
    let mut peers = state.lock().await.get_all_users();
    peers.retain(|_, v| v.cidr() == cidr);
    log::info!("Retrieved a list of all users in the cidr {cidr}... Returning");
    Json(Response::Success(Success::List(peers.iter().map(|(_, v)| v.clone().into()).collect())))
}

pub async fn delete_expired(
    State(state): State<Arc<Mutex<DataStore>>>
) -> Json<Response<Peer<String>>> {
    log::info!("Deleting all users that are expired and not redeemed...");
    let all_peers = state.lock().await.get_all_users();
    let now = match SystemTime::now()
        .duration_since(UNIX_EPOCH) {
            Ok(n) => n.as_secs(),
            Err(_) => return Json(Response::Failure { reason: Some("Unable to get current timestamp".to_string()) }),
    };

    let mut removed_peers = all_peers.clone();
    removed_peers.retain(|_, v| {
        match v.invite_expires() {
            Some(expires) => {
                (expires < now) && (!v.is_redeemed())
            }
            None => false
        }
    });

    let mut datastore = state.lock().await;
    for (id, _) in &removed_peers {
        let op = datastore.network_state.remove_peer_local(id.clone());
        datastore.network_state.peer_op(op);
        let request = PeerRequest::Delete(id.clone()); 
        match datastore.broadcast::<Response<Peer<String>>>(request, "/user/delete").await {
            Ok(()) => return Json(Response::Success(Success::List(removed_peers.iter().map(|(_, v)| v.clone().into()).collect()))),
            Err(e) => eprintln!("Error broadcasting DeletePeerRequest: {e}")
        }
    }

    log::info!("Deleted all users that are expired and not redeemed...");
    Json(Response::Success(Success::List(removed_peers.iter().map(|(_, v)| v.clone().into()).collect())))
}

pub async fn create_cidr(
    State(state): State<Arc<Mutex<DataStore>>>,
    Json(cidr): Json<CidrRequest>,
) -> Json<Response<Cidr<String>>> {
    let mut datastore = state.lock().await;
    match cidr {
        CidrRequest::Op(map_op) => {
            match &map_op {
                crdts::map::Op::Up { ref key, ref op, .. } => {
                    datastore.network_state.cidr_op(map_op.clone());
                    if let (true, v) = datastore.network_state.cidr_op_success(key.clone(), op.clone()) {
                        let _ = write_datastore(&DB_HANDLE, &datastore.clone());
                        return Json(Response::Success(Success::Some(v.into())))
                    } else {
                        return Json(Response::Failure { reason: Some("update was rejected".to_string()) })
                    }
                }
                crdts::map::Op::Rm { .. } => {
                    return Json(Response::Failure { reason: Some("Invalid Op type for Create CIDR".into()) });
                }
            }
        }
        CidrRequest::Create(contents) => {
            let map_op = datastore.network_state.update_cidr_local(contents);
            datastore.network_state.cidr_op(map_op.clone());
            match &map_op {
                crdts::map::Op::Rm { .. } => {
                    return Json(Response::Failure { reason: Some("Map generated RM context instead of Add context on Create request".to_string()) });
                }
                crdts::map::Op::Up { ref key, ref op, .. } => {
                    if let (true, v) = datastore.network_state.cidr_op_success(key.clone(), op.clone()) {
                        let request = CidrRequest::Op(map_op);
                        match datastore.broadcast::<Response<Cidr<String>>>(request, "/cidr/create").await {
                            Ok(()) => {
                                let _ = write_datastore(&DB_HANDLE, &datastore.clone());
                                return Json(Response::Success(Success::Some(v.into())))
                            }
                            Err(e) => eprintln!("Error broadcasting CreateCidrRequest: {e}")
                        }
                        return Json(Response::Success(Success::Some(v.into())))
                    } else {
                        return Json(Response::Failure { reason: Some("update was rejected".to_string()) })
                    }
                }
            }
        }
        _ => {
            return Json(Response::Failure { reason: Some("Invalid request for create cidr".into()) });
        }
    }
} 

pub async fn update_cidr(
    State(state): State<Arc<Mutex<DataStore>>>,
    Json(cidr): Json<CidrRequest>,
) -> Json<Response<Cidr<String>>> {
    let mut datastore = state.lock().await;
    match cidr {
        CidrRequest::Op(map_op) => {
            match &map_op {
                crdts::map::Op::Up { ref key, ref op, .. } => {
                    datastore.network_state.cidr_op(map_op.clone());
                    if let (true, v) = datastore.network_state.cidr_op_success(key.clone(), op.clone()) {
                        let _ = write_datastore(&DB_HANDLE, &datastore.clone());
                        return Json(Response::Success(Success::Some(v.into())))
                    } else {
                        return Json(Response::Failure { reason: Some("update was rejected".to_string()) })
                    }
                }
                crdts::map::Op::Rm { .. } => {
                    return Json(Response::Failure { reason: Some("Invalid Op type for Update CIDR".into()) });
                }
            }
        }
        CidrRequest::Update(contents) => {
            let map_op = datastore.network_state.update_cidr_local(contents);
            datastore.network_state.cidr_op(map_op.clone());
            match &map_op {
                crdts::map::Op::Rm { .. } => {
                    return Json(Response::Failure { reason: Some("Map generated RM context instead of Add context on Update request".to_string()) });
                }
                crdts::map::Op::Up { ref key, ref op, .. } => {
                    if let (true, v) = datastore.network_state.cidr_op_success(key.clone(), op.clone()) {
                        let request = CidrRequest::Op(map_op);
                        match datastore.broadcast::<Response<Cidr<String>>>(request, "/cidr/update").await {
                            Ok(()) => {
                                let _ = write_datastore(&DB_HANDLE, &datastore.clone());
                                return Json(Response::Success(Success::Some(v.into())))
                            }
                            Err(e) => eprintln!("Error broadcasting UpdateCidrRequest: {e}")
                        }
                        return Json(Response::Success(Success::Some(v.into())))
                    } else {
                        return Json(Response::Failure { reason: Some("update was rejected".to_string()) })
                    }
                }
            }
        }
        _ => {
            return Json(Response::Failure { reason: Some("Invalid request for update cidr".into()) });
        }
    }
} 

pub async fn delete_cidr(
    State(state): State<Arc<Mutex<DataStore>>>,
    Json(cidr): Json<CidrRequest>,
) -> Json<Response<Cidr<String>>> {
    let mut datastore = state.lock().await;
    match cidr {
        CidrRequest::Op(map_op) => {
            match &map_op {
                crdts::map::Op::Up { .. } => {
                    return Json(Response::Failure { reason: Some("Invalid Op type for delete cidr".into()) });
                }
                crdts::map::Op::Rm { .. } => {
                    datastore.network_state.cidr_op(map_op);
                    return Json(Response::Success(Success::None));
                }
            }
        }
        CidrRequest::Delete(contents) => {
            let map_op = datastore.network_state.remove_cidr_local(contents);
            datastore.network_state.cidr_op(map_op.clone());
            match &map_op {
                crdts::map::Op::Rm { .. } => {
                    let request = CidrRequest::Op(map_op);
                    match datastore.broadcast::<Response<Cidr<String>>>(request, "/cidr/delete").await {
                        Ok(()) => return Json(Response::Success(Success::None)),
                        Err(e) => eprintln!("Error broadcasting DeleteCidrRequest: {e}")
                    }
                    return Json(Response::Success(Success::None));
                }
                crdts::map::Op::Up { .. } => {
                    return Json(Response::Failure { reason: Some("Map generated Add context instead of Rm context on Delete request".to_string()) });
                }
            }
        }
        _ => {
            return Json(Response::Failure { reason: Some("Invalid request for remove cidr".into()) });
        }
    }
} 

pub async fn get_cidr(
    State(state): State<Arc<Mutex<DataStore>>>,
    Path(id): Path<String>
) -> Json<Response<Cidr<String>>> {
    let guard = state.lock().await;
    log::info!("Request to get cidr: {id}");
    let keys: Vec<String> = guard.network_state.cidrs.keys().map(|ctx| ctx.val.clone()).collect();
    log::info!("Existing keys: {keys:?}");
    if let Some(cidr) = guard.network_state.cidrs.get(&id).val {
        if let Some(val) = cidr.val() {
            return Json(Response::Success(Success::Some(val.value().into())))
        } else {
            return Json(Response::Failure { reason: Some("Entry exists, but value is None".into()) })
        }
    } 
    Json(Response::Failure { reason: Some("Entry does not exist in the CrdtMap".to_string()) })
} 

pub async fn list_cidr(
    State(state): State<Arc<Mutex<DataStore>>>,
) -> Json<Response<Cidr<String>>> {
    log::info!("Received list cidr request");
    let cidrs = state.lock().await.get_all_cidrs().iter().map(|(_, v)| v.clone().into()).collect();
    Json(Response::Success(Success::List(cidrs)))
} 

pub async fn create_assoc(
    State(state): State<Arc<Mutex<DataStore>>>,
    Json(assoc): Json<AssocRequest>
) -> Json<Response<Association<String, (String, String)>>> {
    let mut datastore = state.lock().await;
    match assoc {
        AssocRequest::Op(map_op) => {
            match &map_op {
                crdts::map::Op::Up { ref key, ref op, .. } => {
                    datastore.network_state.associations_op(map_op.clone());
                    if let (true, v) = datastore.network_state.associations_op_success(key.clone(), op.clone()) {
                        let _ = write_datastore(&DB_HANDLE, &datastore.clone());
                        return Json(Response::Success(Success::Some(v.into())))
                    } else {
                        return Json(Response::Failure { reason: Some("update was rejected".to_string()) })
                    }
                }
                crdts::map::Op::Rm { .. } => {
                    return Json(Response::Failure { reason: Some("Invalid Op type for Create Association".into()) });
                }
            }
        }
        AssocRequest::Create(contents) => {
            let map_op = datastore.network_state.update_association_local(contents);
            datastore.network_state.associations_op(map_op.clone());
            match &map_op {
                crdts::map::Op::Rm { .. } => {
                    return Json(Response::Failure { reason: Some("Map generated RM context instead of Add context on Create Association request".to_string()) });
                }
                crdts::map::Op::Up { ref key, ref op, .. } => {
                    if let (true, v) = datastore.network_state.associations_op_success(key.clone(), op.clone()) {
                        let request = AssocRequest::Op(map_op);
                        match datastore.broadcast::<Response<Association<String, (String, String)>>>(request, "/assoc/create").await {
                            Ok(()) => {
                                let _ = write_datastore(&DB_HANDLE, &datastore.clone());
                                return Json(Response::Success(Success::Some(v.into())))
                            }
                            Err(e) => eprintln!("Error broadcasting CreateAssoc Request: {e}")
                        }
                        return Json(Response::Success(Success::Some(v.into())))
                    } else {
                        return Json(Response::Failure { reason: Some("update was rejected".to_string()) })
                    }
                }
            }
        }
        _ => {
            return Json(Response::Failure { reason: Some("Invalid request for create Association".into()) });
        }
    }
}

pub async fn delete_assoc(
    State(state): State<Arc<Mutex<DataStore>>>,
    Json(assoc): Json<AssocRequest>,
) -> Json<Response<Association<String, (String, String)>>> {
    let mut datastore = state.lock().await;
    match assoc {
        AssocRequest::Op(map_op) => {
            match &map_op {
                crdts::map::Op::Up { .. } => {
                    return Json(Response::Failure { reason: Some("Invalid Op type for delete association".into()) });
                }
                crdts::map::Op::Rm { .. } => {
                    datastore.network_state.associations_op(map_op);
                    return Json(Response::Success(Success::None));
                }
            }
        }
        AssocRequest::Delete(contents) => {
            let map_op = datastore.network_state.remove_association_local(contents);
            datastore.network_state.associations_op(map_op.clone());
            match &map_op {
                crdts::map::Op::Rm { .. } => {
                    let request = AssocRequest::Op(map_op);
                    match datastore.broadcast::<Response<Association<String, (String, String)>>>(request, "/assoc/delete").await {
                        Ok(()) => return Json(Response::Success(Success::None)),
                        Err(e) => eprintln!("Error broadcasting DeleteAssocRequest: {e}")
                    }
                    return Json(Response::Success(Success::None));
                }
                crdts::map::Op::Up { .. } => {
                    return Json(Response::Failure { reason: Some("Map generated Add context instead of Rm context on Delete request".to_string()) });
                }
            }
        }
        _ => {
            return Json(Response::Failure { reason: Some("Invalid request for remove association".into()) });
        }
    }
}

pub async fn list_assoc(
    State(state): State<Arc<Mutex<DataStore>>>,
) -> Json<Response<Association<String, (String, String)>>> {
    let assocs = state.lock().await.get_all_assocs().iter().map(|(_, v)| v.clone().into()).collect();
    Json(Response::Success(Success::List(assocs)))
}

pub async fn relationships(
    State(state): State<Arc<Mutex<DataStore>>>,
    Path(cidr_id): Path<String>
) -> Json<Response<Vec<(Cidr<String>, Cidr<String>)>>> {
    let ships = state.lock().await.get_relationships(cidr_id);
    Json(Response::Success(Success::Relationships(ships)))
}

pub async fn request_vanity(
    State(state): State<Arc<Mutex<DataStore>>>,
    Path((domain, build_id)): Path<(String, String)>,
) -> Json<Response<Host>> {
    let datastore = state.lock().await;
    let assigned = datastore.network_state.dns_state.zones.iter().any(|ctx| {
        let (d, _) = ctx.val;
        if *d == domain {
            true
        } else {
            false
        }
    });

    if assigned {
        return Json(
            Response::Failure { 
                reason: Some(
                    format!("Domain name requested is already assigned, if it is assigned to one of your instances run `form [OPTIONS] dns remove` first")
                ) 
            }
        )
    }

    let mut instances = datastore.instance_state.map.iter().filter_map(|ctx| {
        let (_, v) = ctx.val;
        if let Some(v) = v.val() {
            let instance = v.value();
            if instance.build_id == build_id {
                Some(instance.clone())
            } else {
                None
            }
        } else {
            None
        }
    }).collect::<Vec<Instance>>();

    let node_hosts = datastore.node_state.map.iter().filter_map(|ctx| {
        let (i, v) = ctx.val;
        let is_host = instances.iter().any(|inst| inst.node_id == *i);
        if is_host {
            if let Some(reg_node) = v.val() {
                Some(reg_node.value().host.clone())
            } else {
                None
            }
        } else {
            None
        }
    }).collect::<Vec<Host>>();

    let formnet_ip = instances.iter().filter_map(|inst| {
        inst.formnet_ip
    }).collect::<Vec<IpAddr>>();

    let dns_a_record = FormDnsRecord {
        domain,
        record_type: RecordType::A,
        formnet_ip: formnet_ip.iter().map(|ip| {
            SocketAddr::new(*ip, 80)
        }).collect(),
        public_ip: vec![],
        cname_target: None,
        ssl_cert: false,
        ttl: 3600,
        verification_status: None,
        verification_timestamp: None,
    };

    let request = DnsRequest::Create(dns_a_record.clone());

    match Client::new().post("http://127.0.0.1:3004/dns/create")
        .json(&request)
        .send().await {
            Ok(resp) => {
                match resp.json::<Response<FormDnsRecord>>().await {
                    Ok(r) => {
                        match r {
                            Response::Failure { reason } => {
                                return Json(Response::Failure { reason })
                            }
                            _ => {}
                        }
                    }
                    Err(e) => {
                        return Json(Response::Failure { reason: Some(e.to_string()) })
                    }
                }
            }
            Err(e) => {
                return Json(Response::Failure { reason: Some(e.to_string()) })
            }
        };

    instances.iter_mut().for_each(|inst| {
        inst.dns_record = Some(dns_a_record.clone());
    });

    for instance in instances {
        let request = InstanceRequest::Update(instance);
        match Client::new().post("http://127.0.0.1:3004/instance/update")
            .json(&request)
            .send().await {
                Ok(resp) => {
                    match resp.json::<Response<FormDnsRecord>>().await {
                        Ok(r) => {
                            match r {
                                Response::Failure { reason } => {
                                    return Json(Response::Failure { reason })
                                }
                                _ => {}
                            }
                        }
                        Err(e) => {
                            return Json(Response::Failure { reason: Some(e.to_string()) })
                        }
                    }
                }
                Err(e) => {
                    return Json(Response::Failure { reason: Some(e.to_string()) })
                }
            };
    }

    drop(datastore);

    Json(Response::Success(Success::List(node_hosts)))

}

pub async fn request_public(
    State(state): State<Arc<Mutex<DataStore>>>,
    Path((domain, build_id)): Path<(String, String)>,
) -> Json<Response<Host>> {
    let datastore = state.lock().await;
    let assigned = datastore.network_state.dns_state.zones.iter().any(|ctx| {
        let (d, _) = ctx.val;
        if *d == domain {
            true
        } else {
            false
        }
    });

    if assigned {
        return Json(
            Response::Failure { 
                reason: Some(
                    format!("Domain name requested is already assigned, if it is assigned to one of your instances run `form [OPTIONS] dns remove` first")
                ) 
            }
        )
    }

    let mut instances = datastore.instance_state.map.iter().filter_map(|ctx| {
        let (_, v) = ctx.val;
        if let Some(v) = v.val() {
            let instance = v.value();
            if instance.build_id == build_id {
                Some(instance.clone())
            } else {
                None
            }
        } else {
            None
        }
    }).collect::<Vec<Instance>>();

    let node_hosts = datastore.node_state.map.iter().filter_map(|ctx| {
        let (i, v) = ctx.val;
        let is_host = instances.iter().any(|inst| inst.node_id == *i);
        if is_host {
            if let Some(reg_node) = v.val() {
                Some(reg_node.value().host.clone())
            } else {
                None
            }
        } else {
            None
        }
    }).collect::<Vec<Host>>();

    let formnet_ip = instances.iter().filter_map(|inst| {
        inst.formnet_ip
    }).collect::<Vec<IpAddr>>();

    let cname_target = node_hosts.iter().find_map(|h| {
        match h {
            Host::Domain(domain) => Some(domain), 
            _ => None
        }
    }).cloned();

    let a_record_target = node_hosts.iter().filter_map(|h| {
        match h {
            Host::Ipv4(ipv4) => Some(IpAddr::V4(ipv4.clone())),
            _ => None,
        }
    }).collect::<Vec<IpAddr>>();

    let dns_a_record = FormDnsRecord {
        domain: domain.clone(),
        record_type: RecordType::A,
        formnet_ip: formnet_ip.iter().map(|ip| {
            SocketAddr::new(*ip, 80)
        }).collect(),
        public_ip: a_record_target.iter().map(|ip| {
            SocketAddr::new(*ip, 80)
        }).collect(),
        cname_target,
        ssl_cert: false,
        ttl: 3600,
        verification_status: None,
        verification_timestamp: None
    };

    let request = DnsRequest::Create(dns_a_record.clone());

    match Client::new().post("http://127.0.0.1:3004/dns/create")
        .json(&request)
        .send().await {
            Ok(resp) => {
                match resp.json::<Response<FormDnsRecord>>().await {
                    Ok(r) => {
                        match r {
                            Response::Failure { reason } => {
                                return Json(Response::Failure { reason })
                            }
                            _ => {}
                        }
                    }
                    Err(e) => {
                        return Json(Response::Failure { reason: Some(e.to_string()) })
                    }
                }
            }
            Err(e) => {
                return Json(Response::Failure { reason: Some(e.to_string()) })
            }
        };

    instances.iter_mut().for_each(|inst| {
        inst.dns_record = Some(dns_a_record.clone());
    });

    for instance in instances {
        let request = InstanceRequest::Update(instance);
        match Client::new().post("http://127.0.0.1:3004/instance/update")
            .json(&request)
            .send().await {
                Ok(resp) => {
                    match resp.json::<Response<FormDnsRecord>>().await {
                        Ok(r) => {
                            match r {
                                Response::Failure { reason } => {
                                    return Json(Response::Failure { reason })
                                }
                                _ => {}
                            }
                        }
                        Err(e) => {
                            return Json(Response::Failure { reason: Some(e.to_string()) })
                        }
                    }
                }
                Err(e) => {
                    return Json(Response::Failure { reason: Some(e.to_string()) })
                }
            };
    }

    drop(datastore);

    Json(Response::Success(Success::List(node_hosts)))

}

pub async fn create_dns(
    State(state): State<Arc<Mutex<DataStore>>>,
    Json(request): Json<DnsRequest>
) -> Json<Response<FormDnsRecord>> {
    log::info!("Received create user request...");
    let mut datastore = state.lock().await;
    match request {
        DnsRequest::Op(map_op) => {
            log::info!("Create DNS Record request is an Op from another peer");
            match &map_op {
                crdts::map::Op::Up { ref key, ref op, .. } => {
                    datastore.network_state.dns_op(map_op.clone());
                    return Json(handle_create_dns_op(&datastore.network_state, key, op.clone()).await)
                }
                crdts::map::Op::Rm { .. } => {
                    return Json(Response::Failure { reason: Some("Invalid Op type for Create DNS Record".into()) });
                }
            }
        }
        DnsRequest::Create(contents) => {
            log::info!("Create user request was a direct request...");
            log::info!("Building Map Op...");
            let map_op = datastore.network_state.update_dns_local(contents);
            log::info!("Map op created... Applying...");
            datastore.network_state.dns_op(map_op.clone());
            match &map_op {
                crdts::map::Op::Rm { .. } => {
                    return Json(Response::Failure { reason: Some("Map generated RM context instead of Add context on Create DNS Record request".to_string()) });
                }
                crdts::map::Op::Up { ref key, ref op, .. } => {
                    return Json(handle_create_dns_op(&datastore.network_state, key, op.clone()).await);
                }
            }
        }
        _ => {
            return Json(Response::Failure { reason: Some("Invalid request for create DNS".into()) });
        }
    }
}

pub async fn update_dns(
    State(state): State<Arc<Mutex<DataStore>>>,
    Json(request): Json<DnsRequest>
) -> Json<Response<FormDnsRecord>> {
    log::info!("Received create user request...");
    let mut datastore = state.lock().await;
    match request {
        DnsRequest::Op(map_op) => {
            log::info!("Update DNS Request from an Op from another peer");
            match &map_op {
                crdts::map::Op::Up { ref key, ref op, .. } => {
                    datastore.network_state.dns_op(map_op.clone());
                    return Json(handle_update_dns_op(&datastore.network_state, key, op.clone()).await);
                }
                crdts::map::Op::Rm { .. } => {
                    return Json(Response::Failure { reason: Some("Invalid Op type for Update DNS".into()) });
                }
            }
        }
        DnsRequest::Update(contents) => {
            log::info!("Create user request was a direct request...");
            log::info!("Building Map Op...");
            let map_op = datastore.network_state.update_dns_local(contents);
            log::info!("Map op created... Applying...");
            datastore.network_state.dns_op(map_op.clone());
            match &map_op {
                crdts::map::Op::Rm { .. } => {
                    return Json(Response::Failure { reason: Some("Map generated RM context instead of Add context on Update request".to_string()) });
                }
                crdts::map::Op::Up { ref key, ref op, .. } => {
                    return Json(handle_update_dns_op(&datastore.network_state, key, op.clone()).await)
                }
            }
        }
        _ => {
            return Json(Response::Failure { reason: Some("Invalid request for update dns".into()) });
        }
    }
}

pub async fn delete_dns(
    State(state): State<Arc<Mutex<DataStore>>>,
    Path(domain): Path<String>,
    Json(request): Json<DnsRequest>,
) -> Json<Response<FormDnsRecord>> {
    let mut datastore = state.lock().await;
    match request {
        DnsRequest::Op(map_op) => {
            log::info!("Delete DNS request is an Op from another peer");
            match &map_op {
                crdts::map::Op::Up { .. } => {
                    return Json(Response::Failure { reason: Some("Invalid Op type for delete dns".into()) });
                }
                crdts::map::Op::Rm { .. } => {
                    datastore.network_state.dns_op(map_op);
                    return Json(send_dns_delete_request(&domain).await)
                }
            }
        }
        DnsRequest::Delete(domain) => {
            log::info!("Delete DNS request was a direct request...");
            log::info!("Building Map Op...");
            let map_op = datastore.network_state.remove_dns_local(domain.clone());
            log::info!("Map op created... Applying...");
            datastore.network_state.dns_op(map_op.clone());
            match &map_op {
                crdts::map::Op::Rm { .. } => {
                    let request = DnsRequest::Op(map_op);
                    match datastore.broadcast::<Response<FormDnsRecord>>(request, &format!("/dns/{}/delete", domain.clone())).await {
                        Ok(()) => return Json(Response::Success(Success::None)),
                        Err(e) => eprintln!("Error broadcasting Delete DNS request: {e}")
                    }
                    return Json(send_dns_delete_request(&domain).await);
                }
                crdts::map::Op::Up { .. } => {
                    return Json(Response::Failure { reason: Some("Map generated Add context instead of Rm context on Delete request".to_string()) });
                }
            }
        }
        _ => {
            return Json(Response::Failure { reason: Some("Invalid request for create user".into()) });
        }
    }
}

pub async fn get_dns_record(
    State(state): State<Arc<Mutex<DataStore>>>,
    Path(domain): Path<String>,
) -> Json<Response<FormDnsRecord>>{
    let datastore = state.lock().await;
    if let Some(record) = datastore.network_state.dns_state.zones.get(&domain).val {
        if let Some(val) = record.val() {
            let dns_record = val.value();
            return Json(Response::Success(Success::Some(dns_record.into())))
        }
    };

    return Json(Response::Failure { reason: Some(format!("Record does not exist for domain {domain}")) }) 
}

pub async fn update_dns_ip_addr(
    state: Arc<Mutex<DataStore>>,
    node_ip: String,
    new_ip: String,
) -> Result<(), Box<dyn std::error::Error>> {
    if node_ip == new_ip {
        return Ok(())
    }

    let records = get_dns_records_by_node_ip(
        State(state),
        Path(node_ip.clone()),
    ).await;

    if records.is_empty() {
        return Err(
            Box::new(
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("0 Dns Records found with public ip of {node_ip}")
                )
            )
        )
    }

    Ok(())
}

pub async fn get_dns_records_by_node_ip(
    State(state): State<Arc<Mutex<DataStore>>>,
    Path(node_ip): Path<String>
) -> Json<Vec<FormDnsRecord>> {
    if node_ip.parse::<IpAddr>().is_ok() {
        match list_dns_records(State(state)).await {
            Json(Response::Success(Success::List(dns_records))) => {
                let node_is_host = dns_records.iter().filter_map(|rec| {
                    if rec.public_ip.iter().any(|addr| {
                        node_ip == addr.ip().to_string() 
                    }) {
                        Some(rec.clone())
                    } else {
                        None
                    }
                }).collect::<Vec<FormDnsRecord>>();
                return Json(node_is_host);
            }
            Json(Response::Failure { .. }) => return Json(vec![]),
            _ => unreachable!()        
        }
    }

    Json(vec![])
}

pub async fn list_dns_records(
    State(state): State<Arc<Mutex<DataStore>>>,
) -> Json<Response<FormDnsRecord>> {
    let datastore = state.lock().await;
    let dns_record_list = datastore.network_state.dns_state.zones.iter().filter_map(|ctx|{ 
        let (_domain, reg) = ctx.val;
        match reg.val() {
            Some(node) => Some(node.value().into()),
            None => None,
        }
    }).collect::<Vec<FormDnsRecord>>();

    if !dns_record_list.is_empty() {
        return Json(Response::Success(Success::List(dns_record_list)))
    } else {
        return Json(Response::Failure { reason: Some("Unable to find any valid DNS records".to_string()) }) 
    }

}

pub async fn build_dns_request(v: Option<CrdtDnsRecord>, op_type: &str) -> (DomainRequest, Option<Response<FormDnsRecord>>) {
    if op_type == "create" {
        if v.is_none() {
            let request = DomainRequest::Create {
                domain: "".to_string(), 
                record_type: RecordType::NULL,
                ip_addr: vec![],
                cname_target: None,
                ssl_cert: false
            };
            return (request, Some(Response::Failure { reason: Some("Create request requires a record".into()) }))
        }
        return build_create_request(v.unwrap()).await
    }

    if op_type == "update" {
        if v.is_none() {
            let request = DomainRequest::Update {
                replace: false, 
                record_type: RecordType::NULL,
                ip_addr: vec![],
                cname_target: None,
                ssl_cert: false,
            };
            return (request, Some(Response::Failure { reason: Some("Update request requires a record".into()) }))
        }
        return build_update_request(v.unwrap()).await
    }
    let request = DomainRequest::Update {
        replace: false, 
        record_type: RecordType::NULL,
        ip_addr: vec![],
        cname_target: None,
        ssl_cert: false,
    };
    return (request, Some(Response::Failure { reason: Some("Update request requires a record".into()) }))
}

pub async fn build_create_request(v: CrdtDnsRecord) -> (DomainRequest, Option<Response<FormDnsRecord>>) {
    if let RecordType::A = v.record_type() { 
        return build_create_a_record_request(v).await
    } else if let RecordType::AAAA = v.record_type() {
        return build_create_aaaa_record_request(v).await
    } else if let RecordType::CNAME = v.record_type() {
        return build_create_cname_record_request(v).await
    } else {
        let request = DomainRequest::Create {
            domain: v.domain(), 
            record_type: RecordType::NULL,
            ip_addr: vec![],
            cname_target: None,
            ssl_cert: v.ssl_cert()
        };
        return (request, Some(Response::Failure { reason: Some("Only A, AAAA and CNAME records are supported".to_string()) }));
    };
}

pub async fn build_update_request(v: CrdtDnsRecord) -> (DomainRequest, Option<Response<FormDnsRecord>>) {
    if let RecordType::A = v.record_type() { 
        return build_update_a_record_request(v).await
    } else if let RecordType::AAAA = v.record_type() {
        return build_update_aaaa_record_request(v).await
    } else if let RecordType::CNAME = v.record_type() {
        return build_update_cname_record_request(v).await
    } else {
        let request = DomainRequest::Update {
            replace: false, 
            record_type: RecordType::NULL,
            ip_addr: vec![],
            cname_target: None,
            ssl_cert: v.ssl_cert()
        };
        return (request, Some(Response::Failure { reason: Some("Only A, AAAA and CNAME records are supported".to_string()) }));
    }
}

pub async fn build_create_a_record_request(v: CrdtDnsRecord) -> (DomainRequest, Option<Response<FormDnsRecord>>) {
    if !v.formnet_ip().is_empty() {
        let mut ips = v.formnet_ip();
        if !v.public_ip().is_empty() {
            ips.extend(v.public_ip());
        }
        let request = DomainRequest::Create { 
            domain: v.domain().clone(),
            record_type: v.record_type(), 
            ip_addr: ips,
            cname_target: None,
            ssl_cert: v.ssl_cert()
        };
        return (request, None)
    } else {
        let request = DomainRequest::Create { 
            domain: v.domain().clone(), 
            record_type: v.record_type(),
            ip_addr: v.public_ip(), 
            cname_target: None,
            ssl_cert: v.ssl_cert()
        }; 
        return (request, None)
    }
}

pub async fn build_update_a_record_request(v: CrdtDnsRecord) -> (DomainRequest, Option<Response<FormDnsRecord>>) {
    if !v.formnet_ip().is_empty() {
        let mut ips = v.formnet_ip();
        if !v.public_ip().is_empty() {
            ips.extend(v.public_ip());
        }
        let request = DomainRequest::Update { 
            replace: true,
            record_type: v.record_type(), 
            ip_addr: ips, 
            cname_target: None,
            ssl_cert: v.ssl_cert()
        };
        return(request, None);
    } else {
        let request = DomainRequest::Create { 
            domain: v.domain().clone(), 
            record_type: v.record_type(),
            ip_addr: v.public_ip(), 
            cname_target: None,
            ssl_cert: v.ssl_cert()
        }; 
        (request, None)
    }
}

pub async fn build_create_aaaa_record_request(v: CrdtDnsRecord) -> (DomainRequest, Option<Response<FormDnsRecord>>) {
    if !v.public_ip().is_empty() {
        let request = DomainRequest::Create { 
            domain: v.domain().clone(), 
            record_type: v.record_type(),
            ip_addr: v.public_ip(), 
            cname_target: None,
            ssl_cert: v.ssl_cert()
        }; 
        return (request, None)
    } else {
        let request = DomainRequest::Create {
            domain: v.domain().clone(),
            record_type: v.record_type(),
            ip_addr: v.public_ip(), 
            cname_target: None,
            ssl_cert: v.ssl_cert()
        };
        return (request, Some(Response::Failure { reason: Some("AAAA Record Updates require a public IP V6 address".to_string()) }))
    }
}

pub async fn build_update_aaaa_record_request(v: CrdtDnsRecord) -> (DomainRequest, Option<Response<FormDnsRecord>>) {
    let request = DomainRequest::Update { 
        replace: true, 
        record_type: v.record_type(),
        ip_addr: v.public_ip(), 
        cname_target: None,
        ssl_cert: v.ssl_cert()
    }; 
    (request, None)
}

pub async fn build_create_cname_record_request(v: CrdtDnsRecord) -> (DomainRequest, Option<Response<FormDnsRecord>>) {
    let request = DomainRequest::Create {
        domain: v.domain().clone(),
        record_type: v.record_type(),
        ip_addr: {
            let mut ips = v.formnet_ip();
            ips.extend(v.public_ip());
            ips
        },
        cname_target: v.cname_target().clone(),
        ssl_cert: v.ssl_cert()
    };
    (request, None)
}

pub async fn build_update_cname_record_request(v: CrdtDnsRecord) -> (DomainRequest, Option<Response<FormDnsRecord>>) {
    let request = DomainRequest::Update {
        replace: true,
        record_type: v.record_type(),
        ip_addr: {
            let mut ips = v.formnet_ip();
            ips.extend(v.public_ip());
            ips
        },
        cname_target: v.cname_target().clone(),
        ssl_cert: v.ssl_cert()
    };
    (request, None)
}

pub async fn send_dns_create_request(r: DomainRequest) -> Option<Response<FormDnsRecord>> {
    match reqwest::Client::new()
        .post("http://127.0.0.1:3005/record/create")
        .json(&r)
        .send().await {

        Ok(resp) => match resp.json::<DomainResponse>().await {
            Ok(r) => match r {
                DomainResponse::Success(_) => {}
                DomainResponse::Failure(reason) => {
                    return Some(Response::Failure { reason })
                }
                _ => unreachable!() 
            }
            Err(e) => return Some(Response::Failure { reason: Some(e.to_string())})
        }
        Err(e) => {
            return Some(Response::Failure { reason: Some(e.to_string())})
        }
    }
    None
}

pub async fn send_dns_update_request(r: DomainRequest, domain: &str) -> Option<Response<FormDnsRecord>> {
    match reqwest::Client::new()
        .post(format!("http://127.0.0.1:3005/record/{}/update", domain))
        .json(&r)
        .send().await {

        Ok(resp) => match resp.json::<DomainResponse>().await {
            Ok(r) => match r {
                DomainResponse::Success(_) => {}
                DomainResponse::Failure(reason) => {
                    return Some(Response::Failure { reason })
                }
                _ => unreachable!() 
            }
            Err(e) => return Some(Response::Failure { reason: Some(e.to_string()) })
        }
        Err(e) => {
            return Some(Response::Failure { reason: Some(e.to_string())})
        }
    }
    None
}

pub async fn send_dns_delete_request(domain: &str) -> Response<FormDnsRecord> {
    match reqwest::Client::new()
        .post(format!("http://127.0.0.1:3005/record/{}/delete", domain))
        .send().await {
        Ok(resp) => match resp.json::<DomainResponse>().await {
            Ok(r) => match r {
                DomainResponse::Success(_) => return Response::Success(Success::None), 
                DomainResponse::Failure(reason) => {
                    return Response::Failure { reason }
                }
                _ => unreachable!() 
            }
            Err(e) => return Response::Failure { reason: Some(e.to_string()) }
        }
        Err(e) => {
            return Response::Failure { reason: Some(e.to_string())}
        }
    }
}


pub async fn handle_create_dns_op(network_state: &NetworkState, key: &str, op: Update<CrdtDnsRecord, String>) -> Response<FormDnsRecord> {
    if let (true, v) = network_state.dns_op_success(key.to_string(), op.clone()) {
        log::info!("DNS Op succesfully applied... Attempting to build dns request with {v:?}");
        let (request, failure) = build_dns_request(Some(v.clone()), "create").await;
        if let Some(failure) = failure {
            return failure
        }

        let failure = send_dns_create_request(request.clone()).await;
        if let Some(failure) = failure {
            return failure;
        }
        let _ = store_map(&DB_HANDLE, "network_state/dns", &network_state.dns_state.zones.clone());
        return Response::Success(Success::Some(v.into()))
    } else {
        log::info!("DNS Op rejected...");
        return Response::Failure { reason: Some("update was rejected".to_string()) }
    }
}

pub async fn handle_update_dns_op(network_state: &NetworkState, key: &str, op: Update<CrdtDnsRecord, String>) -> Response<FormDnsRecord> {
    if let (true, v) = network_state.dns_op_success(key.to_string(), op.clone()) {
        log::info!("Peer Op succesffully applied...");
        let (request, failure) = build_dns_request(Some(v.clone()), "create").await;
        if let Some(failure) = failure {
            return failure
        }
        let failure = send_dns_update_request(request.clone(), &v.domain()).await;
        if let Some(failure) = failure {
            return failure;
        }
        let _ = store_map(&DB_HANDLE, "network_state/dns", &network_state.dns_state.zones.clone());
        return Response::Success(Success::Some(v.into()))
    } else {
        log::info!("Peer Op rejected...");
        return Response::Failure { reason: Some("update was rejected".to_string()) }
    }
}
