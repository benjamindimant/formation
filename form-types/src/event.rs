use std::net::SocketAddr;

use alloy::primitives::Address;
use serde::{Serialize, Deserialize};
use uuid::Uuid;
use form_traits::{Event as EventTrait, IntoEvent};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    FormnetEvent(FormnetMessage),
    NetworkEvent(NetworkEvent),
    QuorumEvent(QuorumEvent),
    VmmEvent(VmmEvent),
}

impl Event {
    pub fn inner_to_string(&self) -> std::io::Result<String> {
        match self {
            Event::NetworkEvent(network_event) => {
                serde_json::to_string(&network_event).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::Other, e)
                })
            }
            Event::QuorumEvent(quorum_event) => {
                serde_json::to_string(&quorum_event).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::Other, e)
                })
            }
            Event::VmmEvent(vmm_event) => {
                serde_json::to_string(&vmm_event).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::Other, e)
                })
            }
            Event::FormnetEvent(formnet_event) => {
                serde_json::to_string(&formnet_event).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::Other, e)
                })
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum FormnetMessage {
    AddPeer {
        peer_type: PeerType,
        peer_id: String,
        callback: SocketAddr
    },
    DisablePeer,
    EnablePeer,
    SetListenPort,
    OverrideEndpoint,
}

impl FormnetMessage {
    #[cfg(not(test))]
    pub const INTERFACE_NAME: &'static str = "formnet";
    #[cfg(test)]
    pub const INTERFACE_NAME: &'static str = "test-net";
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PeerType {
    Operator,
    User,
    Instance,
}

impl IntoEvent for FormnetMessage {
    type Event = Event;

    fn into_event(&self) -> Self::Event {
        Event::FormnetEvent(self.clone())
    }

    fn to_inner(self: Box<Self>) -> Self::Event {
        Event::FormnetEvent(*self)
    }
}


impl EventTrait for Event {}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum NetworkEvent {
    Heartbeat {
        node_id: Address,
        node_address: SocketAddr,
        timestamp: i64,
        sig: String,
        recovery_id: u32,
        dst: SocketAddr
    },
    Join {
        node_id: Address,
        node_address: SocketAddr,
        sig: String,
        recovery_id: u32,
        to_dial: Vec<SocketAddr>,
        forwarded: bool
    },
}


impl IntoEvent for NetworkEvent {
    type Event = Event;

    fn into_event(&self) -> Self::Event {
        Event::NetworkEvent(self.clone())
    }

    fn to_inner(self: Box<Self>) -> Self::Event {
        Event::NetworkEvent(*self)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum QuorumEvent {
    NewPeer {
        node_id: Address,
        node_address: SocketAddr, 
        new_peer_signature: String,
        new_peer_recovery_id: u8,
        sender_signature: Option<String>,
        sender_recovery_id: Option<u32> 
    },
    Heartbeat {
        node_id: Address,
        node_address: SocketAddr,
        node_signature: String,
        node_recovery_id: u8,
        timestamp: i64
    },
    QuorumGossip {
        node_id: Address,
        node_address: SocketAddr,
        node_signature: String,
        node_recovery_id: u8,
        timestamp: i64,
        request_type: i32,
        payload: String,
    },
    NetworkGossip {
        node_id: Address,
        node_address: SocketAddr,
        node_signature: String,
        node_recovery_id: u8,
        timestamp: i64,
        request_type: i32,
        payload: String,
    },
    DirectMessage {
        node_id: Address,
        node_address: SocketAddr,
        node_signature: String,
        node_recovery_id: u8,
        timestamp: i64,
        message_type: i32,
        payload: String,
    },
    UserRequest {
        user_signature: String,
        user_recovery_id: u8,
        message_id: String,
        timestamp: i64,
        request_type: i32,
        payload: String,
    },
    RemovePeer {
        node_id: Address,
    },
    RemoveInstance {
        instance_id: Uuid
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum VmmEvent {
    Ping {
        name: String,
    },
    Create { 
        owner: String,
        recovery_id: u32,
        requestor: String,
        distro: String,
        version: String,
        user_data: Option<String>,
        meta_data: Option<String>,
        memory_mb: u64,
        vcpu_count: u8,
        name: String,
        custom_cmdline: Option<String>,
        rng_source: Option<String>,
        console_type: Option<String>, 
    },
    Start {
        owner: String,
        recovery_id: u32,
        id: String,
        requestor: String,
    },
    Stop {
        owner: String,
        recovery_id: u32, 
        id: String,
        requestor: String,
    },
    Delete {
        owner: String,
        recovery_id: u32,
        id: String,
        requestor: String,
    },
    Get {
        owner: String,
        recovery_id: u32,
        id: String,
        requestor: String,
    },
    GetList {
        requestor: String,
        recovery_id: u32,
    },
    NetworkSetupComplete {
        invite: String
    },
    Migrate,
    Copy,
    Snapshot,
}

impl IntoEvent for VmmEvent {
    type Event = Event;

    fn into_event(&self) -> Self::Event {
        Event::VmmEvent(self.clone())
    }

    fn to_inner(self: Box<Self>) -> Self::Event {
        Event::VmmEvent(*self)
    }
}
