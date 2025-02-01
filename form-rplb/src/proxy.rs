use crate::{backend::Backend, config::ProxyConfig, error::ProxyError, protocol::Protocol};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt}, net::TcpStream, sync::Mutex
};
use tokio_rustls::{rustls::{self, ServerConfig}, TlsAcceptor};
use tokio_rustls_acme::{caches::DirCache, tokio_rustls::server::TlsStream, AcmeConfig, Incoming};
use tokio_stream::wrappers::TcpListenerStream;
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::sync::RwLock;
use futures::future::try_join_all;
use futures::StreamExt;
use rand::seq::SliceRandom;

#[derive(Clone, Debug)]
pub struct ReverseProxy {
    routes: Arc<RwLock<HashMap<String, Backend>>>,
    config: ProxyConfig,
}

impl ReverseProxy {
    pub fn new(config: ProxyConfig) -> Self {
        Self {
            routes: Arc::new(RwLock::new(HashMap::new())),
            config,
        }
    }

    pub fn config(&self) -> ProxyConfig {
        self.config.clone()
    }

    pub async fn add_route(&self, domain: String, backend: Backend) {
        let mut routes = self.routes.write().await;
        routes.insert(domain, backend);
    }

    pub async fn remove_route(&self, domain: &str) -> Option<Backend> {
        let mut routes = self.routes.write().await;
        routes.remove(domain)
    }

    pub async fn get_route(&self, domain: &str) -> Option<Backend> {
        let routes = self.routes.read().await;
        routes.get(domain).cloned()
    }

    pub async fn select_backend(&self, domain: &str) -> Result<SocketAddr, ProxyError> {
        let routes = self.routes.read().await;
        let backend = routes.get(domain)
            .ok_or_else(|| ProxyError::NoBackend(domain.to_string()))?;
            
        backend.addresses()
            .choose(&mut rand::thread_rng())
            .copied()
            .ok_or_else(|| ProxyError::NoBackend(domain.to_string()))
    }

    async fn get_backend(&self, domain: &str) -> Result<Backend, ProxyError> {
        let routes = self.routes.read().await;
        if let Some(backend) = routes.get(domain) {
            Ok(backend.clone())
        } else {
            Err(ProxyError::NoBackend(domain.to_string()))
        }
    }

    pub async fn handle_http_connection(
        &self,
        mut client_stream: TcpStream,
    ) -> Result<(), ProxyError> {
        let mut buffer = vec![0; self.config.buffer_size];
        let n = client_stream.read(&mut buffer).await?;

        let request = String::from_utf8_lossy(&buffer[..n]);
        let domain = self.extract_domain(&request)?;

        let backend_addr = self.select_backend(&domain).await?;
        let mut backend_stream = tokio::time::timeout(
            self.config.connection_timeout,
            TcpStream::connect(backend_addr)
        ).await.map_err(|e| ProxyError::InvalidRequest(e.to_string()))??;

        backend_stream.write_all(&buffer[..n]).await.map_err(|e| {
            ProxyError::Io(e)
        })?;

        let (mut client_read, mut client_write) = client_stream.split();
        let (mut backend_read, mut backend_write) = backend_stream.split();

        let client_to_backend = tokio::io::copy(&mut client_read, &mut backend_write);
        let backend_to_client = tokio::io::copy(&mut backend_read, &mut client_write);

        try_join_all(vec![client_to_backend, backend_to_client]).await?;

        Ok(())
    }

    pub fn extract_domain(&self, request: &str) -> Result<String, ProxyError> {
        let host_line = request.lines()
            .find(|line| line.starts_with("Host: "))
            .ok_or_else(|| ProxyError::InvalidRequest("No Host header found".to_string()))?;
        
        Ok(host_line[6..].trim().to_string())
    }

    pub async fn handle_tls_connection(&self, mut stream: TlsStream<TcpStream>, domain: &str) -> Result<(), ProxyError> {
        let mut buffer = vec![0; self.config.buffer_size];
        let n = stream.read(&mut buffer).await?;

        let backend_addr = self.select_backend(domain).await?;
        let mut backend_stream = tokio::time::timeout(
            self.config.connection_timeout,
            TcpStream::connect(backend_addr)
        ).await.map_err(|e| ProxyError::InvalidRequest(e.to_string()))??;

        backend_stream.write_all(&buffer[..n]).await.map_err(|e| {
            ProxyError::Io(e)
        })?;

        let (mut client_read, mut client_write) = tokio::io::split(stream);
        let (mut backend_read, mut backend_write) = backend_stream.split();

        let client_to_backend = tokio::io::copy(&mut client_read, &mut backend_write);
        let backend_to_client = tokio::io::copy(&mut backend_read, &mut client_write);

        tokio::try_join!(
            client_to_backend,
            backend_to_client
        )?;

        Ok(())
    }
}

/// Extracts the Server Name Indication (SNI) from a TLS ClientHello message.
/// 
/// The TLS ClientHello message structure is defined in RFC 5246 (TLS 1.2) and RFC 8446 (TLS 1.3).
/// The SNI extension is defined in RFC 6066 Section 3.
/// 
/// Structure of a TLS ClientHello (simplified):
/// Byte   0       - Record Type (0x16 for Handshake)
/// Bytes  1-2     - TLS Version
/// Bytes  3-4     - Record Length
/// Byte   5       - Handshake Type (0x01 for ClientHello)
/// Bytes  6-8     - Handshake Length
/// Bytes  9-10    - Protocol Version
/// Bytes  11-42   - Random (32 bytes)
/// Byte   43      - Session ID Length
/// Variable       - Session ID
/// 2 bytes        - Cipher Suites Length
/// Variable       - Cipher Suites
/// 1 byte         - Compression Methods Length
/// Variable       - Compression Methods
/// 2 bytes        - Extensions Length
/// Variable       - Extensions
pub fn extract_sni(client_hello: &[u8]) -> Result<String, rustls::Error> {
    // First, verify we have enough data for the basic TLS header
    if client_hello.len() < 5 {
        return Err(rustls::Error::General("ClientHello too short".into()));
    }

    // Validate this is a TLS handshake
    if client_hello[0] != 0x16 {  // Record Type
        return Err(rustls::Error::General("Not a TLS handshake".into()));
    }

    // Skip record header (5 bytes) and validate handshake type
    if client_hello.len() < 6 || client_hello[5] != 0x01 {  // ClientHello type
        return Err(rustls::Error::General("Not a ClientHello".into()));
    }

    // Start after the fixed portion (random + protocol version)
    let mut pos = 43;  // 5 (record) + 4 (handshake) + 2 (version) + 32 (random)
    
    if pos >= client_hello.len() {
        return Err(rustls::Error::General("Message too short for session ID".into()));
    }

    // Skip session ID
    let session_id_len = client_hello[pos] as usize;
    pos += 1 + session_id_len;

    if pos + 2 > client_hello.len() {
        return Err(rustls::Error::General("Message too short for cipher suites".into()));
    }

    // Skip cipher suites
    let cipher_suites_len = ((client_hello[pos] as usize) << 8) | (client_hello[pos + 1] as usize);
    pos += 2 + cipher_suites_len;

    if pos + 1 > client_hello.len() {
        return Err(rustls::Error::General("Message too short for compression methods".into()));
    }

    // Skip compression methods
    let compression_methods_len = client_hello[pos] as usize;
    pos += 1 + compression_methods_len;

    if pos + 2 > client_hello.len() {
        return Err(rustls::Error::General("Message too short for extensions".into()));
    }

    // Process extensions
    let extensions_len = ((client_hello[pos] as usize) << 8) | (client_hello[pos + 1] as usize);
    pos += 2;
    let extensions_end = pos + extensions_len;

    if extensions_end > client_hello.len() {
        return Err(rustls::Error::General("Message too short for extensions data".into()));
    }

    // Search for the SNI extension (type 0)
    while pos + 4 <= extensions_end {
        let extension_type = ((client_hello[pos] as u16) << 8) | (client_hello[pos + 1] as u16);
        let extension_len = ((client_hello[pos + 2] as usize) << 8) | (client_hello[pos + 3] as usize);
        pos += 4;

        if extension_type == 0 {  // SNI extension type
            if pos + 2 > extensions_end {
                return Err(rustls::Error::General("SNI extension truncated".into()));
            }

            // Parse SNI extension
            let sni_list_len = ((client_hello[pos] as usize) << 8) | (client_hello[pos + 1] as usize);
            pos += 2;

            if pos + sni_list_len > extensions_end {
                return Err(rustls::Error::General("SNI extension data truncated".into()));
            }

            let mut sni_pos = pos;
            while sni_pos + 3 <= pos + sni_list_len {
                let name_type = client_hello[sni_pos];
                let name_len = ((client_hello[sni_pos + 1] as usize) << 8) | 
                              (client_hello[sni_pos + 2] as usize);
                sni_pos += 3;

                if sni_pos + name_len > pos + sni_list_len {
                    return Err(rustls::Error::General("SNI hostname truncated".into()));
                }

                // name_type 0 is hostname
                if name_type == 0 {
                    return String::from_utf8(client_hello[sni_pos..sni_pos + name_len].to_vec())
                        .map_err(|_| rustls::Error::General("Invalid UTF-8 in SNI hostname".into()));
                }

                sni_pos += name_len;
            }
        }

        pos += extension_len;
    }

    Err(rustls::Error::General("No SNI extension found".into()))
}
