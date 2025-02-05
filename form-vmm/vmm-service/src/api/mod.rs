use axum::{
    routing::{get, post},
    Router,
    Json,
    extract::State,
};
use form_p2p::queue::{QueueRequest, QueueResponse, QUEUE_PORT};
use reqwest::Client;
use serde::{de::DeserializeOwned, Serialize}; 
use tiny_keccak::{Hasher, Sha3};
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use vmm::api::{VmInfo, VmmPingResponse};
use std::{sync::Arc, time::Duration};
use std::net::SocketAddr;

use crate::VmmError;
use form_types::{BootCompleteRequest, CreateVmRequest, DeleteVmRequest, GetVmRequest, PingVmmRequest, StartVmRequest, StopVmRequest, VmResponse, VmmEvent, VmmResponse};

pub struct VmmApiChannel {
    event_sender: mpsc::Sender<VmmEvent>,
    response_receiver: mpsc::Receiver<String>
}

impl VmmApiChannel {
    pub fn new(
        tx: mpsc::Sender<VmmEvent>,
        rx: mpsc::Receiver<String>
    ) -> Self {
        Self{
            event_sender: tx,
            response_receiver: rx
        }
    }

    pub async fn send(
        &self,
        event: VmmEvent
    ) -> Result<(), mpsc::error::SendError<VmmEvent>> {
        self.event_sender.send(event).await
    }

    pub async fn recv<T: DeserializeOwned>(
        &mut self
    ) -> Option<T> {
        match self.response_receiver.recv().await {
            Some(value) => {
                match serde_json::from_str::<T>(&value) {
                    Ok(resp) => return Some(resp),
                    Err(e) => {
                        log::error!("{e}");
                        return None
                    }
                }
            }
            None => return None
        }
    }
}

/// API server that allows direct interaction with the VMM service
pub struct VmmApi {
    /// Channels to send events to the service and receive responses
    channel: Arc<Mutex<VmmApiChannel>>,
    /// Server address
    addr: SocketAddr,
}

impl VmmApi {
    pub fn new(
        event_sender: mpsc::Sender<VmmEvent>,
        response_receiver: mpsc::Receiver<String>,
        addr: SocketAddr
    ) -> Self {
        let api_channel = Arc::new(Mutex::new(VmmApiChannel::new(
            event_sender,
            response_receiver
        )));
        Self {
            channel: api_channel, addr
        }
    }

    pub async fn start_queue_reader(&self, channel: Arc<Mutex<VmmApiChannel>>) -> Result<(), VmmError> { 
        let mut n = 0;
        loop {
            if let Ok(messages) = Self::read_from_queue(Some(n), None).await {
                for message in &messages {
                    Self::handle_message(message.to_vec(), channel.clone()).await;
                }
                n += messages.len();
            }
        }
        Ok(())
    }

    pub async fn handle_message(message: Vec<u8>, channel: Arc<Mutex<VmmApiChannel>>) -> Result<(), VmmError> {
        let subtopic = message[0];
        let msg = &message[1..];
        match subtopic {
            1 => Self::handle_create_vm_message(msg, channel.clone()).await?,
            2 => Self::handle_boot_vm_message(msg, channel.clone()).await?, 
            3 => Self::handle_delete_vm_message(msg, channel.clone()).await?,
            4 => Self::handle_stop_vm_message(msg, channel.clone()).await?,
            5 => Self::handle_reboot_vm_message(msg, channel.clone()).await?,
            6 => Self::handle_start_vm_message(msg, channel.clone()).await?,
            _ => unreachable!()
        }
        Ok(())
    }

    pub async fn handle_create_vm_message(msg: &[u8], channel: Arc<Mutex<VmmApiChannel>>) -> Result<(), VmmError> {
        let request: CreateVmRequest = serde_json::from_slice(msg).map_err(|e| {
            VmmError::Config(e.to_string())
        })?;
        let event = VmmEvent::Create { 
            formfile: request.formfile, 
            name: request.name, 
        };

        channel.lock().await.send(event).await.map_err(|e| {
            VmmError::SystemError(e.to_string())
        })?;

        Ok(())
    }

    pub async fn handle_boot_vm_message(msg: &[u8], channel: Arc<Mutex<VmmApiChannel>>) -> Result<(), VmmError> {
        let request: StartVmRequest = serde_json::from_slice(msg).map_err(|e| {
            VmmError::Config(e.to_string())
        })?;

        let event = VmmEvent::Start { id: request.id };
        channel.lock().await.send(event).await.map_err(|e| {
            VmmError::SystemError(e.to_string())
        })?;

        Ok(())
    }

    pub async fn handle_delete_vm_message(msg: &[u8], channel: Arc<Mutex<VmmApiChannel>>) -> Result<(), VmmError> {
        let request: DeleteVmRequest = serde_json::from_slice(msg).map_err(|e| {
            VmmError::Config(e.to_string())
        })?;

        let event = VmmEvent::Delete { id: request.id };

        channel.lock().await.send(event).await.map_err(|e| {
            VmmError::SystemError(e.to_string())
        })?;

        Ok(())
    }

    pub async fn handle_stop_vm_message(msg: &[u8], channel: Arc<Mutex<VmmApiChannel>>) -> Result<(), VmmError> {
        let request: StopVmRequest = serde_json::from_slice(msg).map_err(|e| {
            VmmError::Config(e.to_string())
        })?;

        let event = VmmEvent::Stop { id: request.id };

        channel.lock().await.send(event).await.map_err(|e| {
            VmmError::SystemError(e.to_string())
        })?;
        
        Ok(())
    }

    pub async fn handle_reboot_vm_message(msg: &[u8], channel: Arc<Mutex<VmmApiChannel>>) -> Result<(), VmmError> {
        let request: StopVmRequest = serde_json::from_slice(msg).map_err(|e| {
            VmmError::Config(e.to_string())
        })?;

        let event = VmmEvent::Stop { id: request.id.clone() };

        channel.lock().await.send(event).await.map_err(|e| {
            VmmError::SystemError(e.to_string())
        })?;

        let event = VmmEvent::Start { id: request.id };
        
        channel.lock().await.send(event).await.map_err(|e| {
            VmmError::SystemError(e.to_string())
        })?;

        Ok(())
    }

    pub async fn handle_start_vm_message(msg: &[u8], channel: Arc<Mutex<VmmApiChannel>>) -> Result<(), VmmError> {
        let request: StartVmRequest = serde_json::from_slice(msg).map_err(|e| {
            VmmError::Config(e.to_string())
        })?;

        let event = VmmEvent::Start { id: request.id };

        channel.lock().await.send(event).await.map_err(|e| {
            VmmError::SystemError(e.to_string())
        })?;

        Ok(())
    }

    pub async fn write_to_queue(
        message: impl Serialize + Clone,
        sub_topic: u8,
        topic: &str
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut hasher = Sha3::v256();
        let mut topic_hash = [0u8; 32];
        hasher.update(topic.as_bytes());
        hasher.finalize(&mut topic_hash);
        let mut message_code = vec![sub_topic];
        message_code.extend(serde_json::to_vec(&message)?);
        let request = QueueRequest::Write { 
            content: message_code, 
            topic: topic_hash 
        };

        match Client::new()
            .post(format!("http://127.0.0.1:{}/queue/write_local", QUEUE_PORT))
            .json(&request)
            .send().await?
            .json::<QueueResponse>().await? {
                QueueResponse::OpSuccess => return Ok(()),
                QueueResponse::Failure { reason } => return Err(Box::new(std::io::Error::new(std::io::ErrorKind::Other, format!("{reason:?}")))),
                _ => return Err(Box::new(std::io::Error::new(std::io::ErrorKind::Other, "Invalid response variant for write_local endpoint")))
        }
    }

    pub async fn read_from_queue(
        last: Option<usize>,
        n: Option<usize>,
    ) -> Result<Vec<Vec<u8>>, Box<dyn std::error::Error + Send + Sync>> {
        let mut endpoint = format!("http://127.0.0.1:{}/queue/vmm", QUEUE_PORT);
        if let Some(idx) = last {
            let idx = idx + 1;
            endpoint.push_str(&format!("/{idx}"));
            if let Some(n) = n {
                endpoint.push_str(&format!("/{n}/get_n_after"));
            } else {
                endpoint.push_str("/get_after");
            }
        } else {
            if let Some(n) = n {
                endpoint.push_str(&format!("/{n}/get_n"))
            } else {
                endpoint.push_str("/get")
            }
        }

        match Client::new()
            .get(endpoint.clone())
            .send().await?
            .json::<QueueResponse>().await? {
                QueueResponse::List(list) => Ok(list),
                QueueResponse::Failure { reason } => Err(Box::new(std::io::Error::new(std::io::ErrorKind::Other, format!("{reason:?}")))),
                _ => Err(Box::new(std::io::Error::new(std::io::ErrorKind::Other, format!("Invalid response variant for {endpoint}")))) 
        }
    }

    pub async fn start(&self) -> Result<(), VmmError> {
        log::info!("Attempting to start API server");
        let app_state = self.channel.clone();

        log::info!("Establishing Routes");
        let app = Router::new()
            .route("/health", get(health_check))
            .route("/vm/create", post(create))
            .route("/vm/:id/boot_complete", post(boot_complete))
            .route("/vm/:id/boot", post(start))
            .route("/vm/:id/delete", post(delete))
            .route("/vm/:id/pause", post(stop))
            .route("/vm/:id/stop", post(stop))
            .route("/vm/:id/reboot", post(reboot))
            .route("/vm/:id/resume", post(start))
            .route("/vm/:id/start", post(start))
            .route("/vm/:id/on", post(start))
            .route("/vm/:id/power_button", post(power_button))
            .route("/vm/:id/commit", post(commit))
            .route("/vm/:id/update", post(commit))
            .route("/vm/:id/snapshot", post(snapshot))
            .route("/vm/:id/coredump", post(coredump))
            .route("/vm/:id/restore", post(restore))
            .route("/vm/:id/resize_vcpu", post(resize_vcpu))
            .route("/vm/:id/resize_memory", post(resize_memory))
            .route("/vm/:id/add_device", post(add_device))
            .route("/vm/:id/add_disk", post(add_disk))
            .route("/vm/:id/add_fs", post(add_fs))
            .route("/vm/:id/remove_device", post(remove_device))
            .route("/vm/:id/migrate_to", post(migrate_to))
            .route("/vm/:id/migrate_from", post(migrate_from))
            .route("/vm/:id/ping", post(ping))
            .route("/vm/:id/info", get(get_vm))
            .route("/vm/:id", get(get_vm))
            .route("/vms/list", get(list))
            .with_state(app_state);

        log::info!("Established route, binding to {}", &self.addr);
        let listener = tokio::net::TcpListener::bind(
            self.addr.clone()).await
            .map_err(|e| {
                VmmError::SystemError(
                    format!(
                        "Failed to bind listener to address {}: {e}",
                        self.addr.clone()
                    )
                )
            })?;
            
        // Start the API server
        log::info!("Starting server");
        axum::serve(listener, app).await
            .map_err(|e| VmmError::SystemError(format!("Failed to serve API server {e}")))?;


        Ok(())
    }

    pub fn addr(&self) -> &SocketAddr {
        &self.addr
    }
}

async fn health_check() -> &'static str {
    "OK"
}

async fn ping(
    State(channel): State<Arc<Mutex<VmmApiChannel>>>,
    Json(request): Json<PingVmmRequest>
) -> Result<Json<VmmPingResponse>, String> {
    let event = VmmEvent::Ping { name: request.name.to_string() };
    request_receive(channel, event).await
}

async fn create(
    State(channel): State<Arc<Mutex<VmmApiChannel>>>,
    Json(request): Json<CreateVmRequest>,
) -> Json<VmmResponse> {
    log::info!(
        "Received VM create request: name={}",
        request.name
    );
    let event = VmmEvent::Create {
        formfile: request.formfile,
        name: request.name.clone(),
    };

    if let Err(e) = channel.lock().await
        .send(event.clone())
        .await {
            log::info!("Error sending {event:?}: {e}");
            return Json(
                VmmResponse::Failure(
                    format!(
                        "Error sending event {event:?} across VmmApiChannel to request creation of vm {}",
                        request.name
                    )
                )
            )
    }

    Json(VmmResponse::Success(VmResponse {
        id: "pending".to_string(),
        name: request.name,
        state: "PENDING".to_string()
    }))

}

async fn boot_complete(
    State(channel): State<Arc<Mutex<VmmApiChannel>>>,
    Json(request): Json<BootCompleteRequest>,
) -> Json<VmmResponse> {

    log::info!("Received BootCompleteRequest for VM {}", request.name);
    let event = VmmEvent::BootComplete {
        id: request.name.clone(),
        formnet_ip: request.formnet_ip,
    };

    log::info!("Built BootComplete VmmEvent, sending across api channel");
    if let Err(e) = channel.lock().await.send(event.clone()).await {
        log::info!("Error receiving response back from API channel: {e}");
        return Json(
            VmmResponse::Failure(
                format!("Error recording BootComplete event {event:?}: {e}")
            )
        )
    }
    log::info!("BootCompleteRequest handled succesfully, responding...");
    Json(VmmResponse::Success(
        VmResponse { 
            id: request.name.clone(), 
            name: request.name,
            state: "complete".to_string() 
        }
    ))
}

async fn start(
    State(channel): State<Arc<Mutex<VmmApiChannel>>>,
    Json(request): Json<StartVmRequest>,
) -> Json<VmmResponse> {
    let event = VmmEvent::Start {
        id: request.id.clone(),
    };
    if let Err(e) = request_receive::<()>(channel, event).await {
        return Json(VmmResponse::Failure(e.to_string()))
    }
    Json(VmmResponse::Success(
        VmResponse {
            id: request.id, 
            name: request.name,
            state: "pending".to_string()
    }))
}

async fn stop(
    State(channel): State<Arc<Mutex<VmmApiChannel>>>,
    Json(request): Json<StopVmRequest>,
) -> Json<VmmResponse> {
    let event = VmmEvent::Stop {
        id: request.id.clone(),
    };

    if let Err(e) = request_receive::<()>(channel, event).await {
        return Json(VmmResponse::Failure(e.to_string()))
    }
    Json(VmmResponse::Success(
        VmResponse {
            id: request.id, 
            name: request.name,
            state: "pending".to_string()
    }))
}

async fn delete(
    State(channel): State<Arc<Mutex<VmmApiChannel>>>,
    Json(request): Json<DeleteVmRequest>,
) -> Json<VmmResponse> {
    let event = VmmEvent::Delete {
        id: request.id.clone(),
    };

    if let Err(e) = request_receive::<()>(channel, event).await {
        return Json(VmmResponse::Failure(e.to_string()))
    }

    Json(VmmResponse::Success(
        VmResponse {
            id: request.id, 
            name: request.name,
            state: "pending".to_string()
    }))
}

async fn get_vm(
    State(channel): State<Arc<Mutex<VmmApiChannel>>>,
    Json(request): Json<GetVmRequest>,
) -> Result<Json<VmInfo>, String>  {
    let event = VmmEvent::Get {
        id: request.id.clone(),
    };

    request_receive(channel, event).await
}
async fn list(
    State(channel): State<Arc<Mutex<VmmApiChannel>>>,
) -> Result<Json<Vec<VmInfo>>, String> {

    let event = VmmEvent::GetList {
        requestor: "test".to_string(),
    };

    request_receive(channel, event).await
}

async fn power_button() {}
async fn reboot() {}
async fn commit() {}
async fn snapshot() {}
async fn coredump() {}
async fn restore() {}
async fn resize_vcpu() {}
async fn resize_memory() {}
async fn add_device() {}
async fn add_disk() {}
async fn add_fs() {}
async fn remove_device() {}
async fn migrate_to() {}
async fn migrate_from() {}

async fn request_receive<T: DeserializeOwned>(
    channel: Arc<Mutex<VmmApiChannel>>,
    event: VmmEvent,
) -> Result<Json<T>, String> {
    let mut channel = channel.lock().await; 
    channel.send(event.clone()).await.map_err(|e| e.to_string())?;
    tokio::select! {
        Some(resp) = channel.recv() => {
            Ok(Json(resp))
        }
        _ = tokio::time::sleep(Duration::from_secs(5)) => {
            Err(format!("Request {event:?} timed out awaiting response"))
        }
    }
}
