use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::sync::{Mutex, broadcast};

const MAX_PAYLOAD_SIZE: usize = 15 * 1024 * 1024;

pub enum KnotStream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(UnixStream),
}

impl KnotStream {
    pub fn split(
        self,
    ) -> (
        Box<dyn AsyncRead + Send + Unpin>,
        Box<dyn AsyncWrite + Send + Unpin>,
    ) {
        match self {
            KnotStream::Tcp(s) => {
                let (r, w) = tokio::io::split(s);
                (Box::new(r), Box::new(w))
            }
            #[cfg(unix)]
            KnotStream::Unix(s) => {
                let (r, w) = tokio::io::split(s);
                (Box::new(r), Box::new(w))
            }
        }
    }
}

// ─────────────────────────────────────────────
// Commands
// ─────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(tag = "command", rename_all = "lowercase")]
pub enum KnotCommand {
    Version,
    Protocol,
    Status,
    Listeners,
    #[serde(rename = "register")]
    Register {
        app_id: u64,
        port: u16,
    },
    #[serde(rename = "getpeers")]
    GetPeers,
    #[serde(rename = "getpeerid")]
    GetPeerId,
    #[serde(rename = "getcommands")]
    GetCommands,
    // #[serde(rename = "newappname")] NewAppName {
    //     name: String,
    //     port: u16,
    // },
    #[serde(rename = "connect")]
    Connect {
        multiaddr: String,
    },
    #[serde(rename = "connectrelay")]
    ConnectRelay {
        relay_addr: String,
        relay_id: String,
    },
    Discover {
        peer_id: String,
    },
}

// ─────────────────────────────────────────────
// Inbound messages
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KnotMessage {
    pub command: Option<String>,
    pub response: Option<serde_json::Value>,
    pub error: Option<String>,
}

// ─────────────────────────────────────────────
// Error type
// ─────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum KnotError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Socket not connected")]
    NotConnected,
    #[error("AppId not found")]
    AppIdNotFound,
    #[error("Payload exceeds 15 MB limit ({0} bytes)")]
    PayloadTooLarge(usize),
    #[error("Peer parse error")]
    PeerParseError,
    #[error("{0}")]
    Custom(String),
}

// ─────────────────────────────────────────────
// Internal shared state
// ─────────────────────────────────────────────

struct Inner {
    json_writer: Mutex<Option<Box<dyn AsyncWrite + Send + Unpin>>>,
    byte_writer: Mutex<Option<Box<dyn AsyncWrite + Send + Unpin>>>,
    app_id: Mutex<Option<u64>>,
}

// ─────────────────────────────────────────────
// KnotClient
// ─────────────────────────────────────────────

#[derive(Clone)]
pub struct KnotClient {
    inner: Arc<Inner>,
    /// Subscribe to receive decoded JSON messages
    pub msg_tx: broadcast::Sender<KnotMessage>,
    /// Subscribe to receive raw byte payloads (as UTF-8 strings)
    pub byte_tx: broadcast::Sender<String>,
}

impl KnotClient {
    /// Create and connect the client.
    /// Spawns background tasks for JSON socket, byte socket, and byte-server listener.
    pub async fn new(local_port: i32) -> Result<Self, KnotError> {
        let (msg_tx, _) = broadcast::channel::<KnotMessage>(64);
        let (byte_tx, _) = broadcast::channel::<String>(64);

        let inner = Arc::new(Inner {
            json_writer: Mutex::new(None),
            byte_writer: Mutex::new(None),
            app_id: Mutex::new(None),
        });

        let client = KnotClient {
            inner: inner.clone(),
            msg_tx: msg_tx.clone(),
            byte_tx: byte_tx.clone(),
        };

        // ── JSON socket (port 12012) ──
        let json_stream = connect_dynamic(12012, "managed").await?;
        let (json_read, json_write) = json_stream.split();
        *inner.json_writer.lock().await = Some(json_write);

        // Background task: read newline-delimited JSON
        let inner_clone = inner.clone();
        let msg_tx_clone = msg_tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(json_read);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        eprintln!("[Knot] JSON socket closed by server");
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<KnotMessage>(trimmed) {
                            Ok(msg) => {
                                // Handle "register" command → store appId
                                if msg.command.as_deref() == Some("register") {
                                    if let Some(ref v) = msg.response {
                                        let id: Option<u64> = v
                                            .as_u64()
                                            .or_else(|| v.as_str().and_then(|s| s.parse().ok()));
                                        if let Some(id) = id {
                                            *inner_clone.app_id.lock().await = Some(id);
                                        }
                                    }
                                }
                                let _ = msg_tx_clone.send(msg);
                            }
                            Err(e) => {
                                eprintln!(
                                    "[Knot] Failed to parse JSON message: {e} — raw: {trimmed}"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[Knot] JSON read error: {e}");
                        break;
                    }
                }
            }
        });

        // ── Byte socket (port 12812) ──
        let byte_stream = connect_dynamic(12812, "binary").await?;
        let (_byte_read, byte_write) = byte_stream.split();
        *inner.byte_writer.lock().await = Some(byte_write);

        // ── Byte server (port 8124) — receives messages from peers ──
        let byte_tx_clone = byte_tx.clone();
        tokio::spawn(async move {
            start_dynamic_byte_server(local_port, byte_tx_clone).await;
        });

        // Initial status handshake (mirrors TS constructor)
        //client.send_json(KnotCommand::Status).await?;

        Ok(client)
    }

    // ── Public API ──────────────────────────────

    pub async fn send_json(&self, command: KnotCommand) -> Result<String, KnotError> {
        let json = serde_json::to_string(&command)?;

        #[cfg(debug_assertions)]
        println!("{json}");

        let mut guard = self.inner.json_writer.lock().await;
        let writer = guard.as_mut().ok_or(KnotError::NotConnected)?;
        writer.write_all((json.clone() + "\n").as_bytes()).await?;
        Ok(json)
    }

    pub async fn send_bytes(
        &self,
        peer_input: &str,
        payload: &[u8],
        app_id: u64,
    ) -> Result<(), KnotError> {
        if payload.len() > MAX_PAYLOAD_SIZE {
            return Err(KnotError::PayloadTooLarge(payload.len()));
        }

        //let app_id = self.inner.app_id.lock().await.ok_or(KnotError::AppIdNotFound)?;

        let peer_id = get_peer_id_u64(peer_input)?;

        // 24-byte header — mirrors TS: >BBQQIH
        // Offset  Size  Field
        //  0       1    version (u8)
        //  1       1    flag    (u8)
        //  2       8    peer_id (u64 BE)
        // 10       8    app_id  (u64 BE)
        // 18       4    payload_len (u32 BE)
        // 22       2    reserved (u16 BE)
        let mut header = [0u8; 24];
        header[0] = 1; // version
        header[1] = 1; // flag
        header[2..10].copy_from_slice(&peer_id.to_be_bytes());
        header[10..18].copy_from_slice(&app_id.to_be_bytes());
        header[18..22].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        // header[22..24] already 0 (reserved)

        let mut packet = Vec::with_capacity(24 + payload.len());
        packet.extend_from_slice(&header);
        packet.extend_from_slice(payload);

        let mut guard = self.inner.byte_writer.lock().await;
        let writer = guard.as_mut().ok_or(KnotError::NotConnected)?;
        writer.write_all(&packet).await?;
        Ok(())
    }

    /// Convenience: subscribe to JSON messages
    pub fn subscribe_messages(&self) -> broadcast::Receiver<KnotMessage> {
        self.msg_tx.subscribe()
    }

    /// Convenience: subscribe to incoming byte payloads
    pub fn subscribe_bytes(&self) -> broadcast::Receiver<String> {
        self.byte_tx.subscribe()
    }
}

pub fn get_peer_id_u64(peer_input: &str) -> Result<u64, KnotError> {
    let decoded = bs58::decode(peer_input)
        .into_vec()
        .map_err(|_| KnotError::PeerParseError)?;

    let mut arr = [0u8; 8];
    if decoded.len() >= 8 {
        arr.copy_from_slice(&decoded[decoded.len() - 8..]);
    } else {
        let offset = 8 - decoded.len();
        arr[offset..].copy_from_slice(&decoded);
    }

    Ok(u64::from_be_bytes(arr))
}

async fn connect_dynamic(port: u16, name: &str) -> Result<KnotStream, KnotError> {
    #[cfg(unix)]
    {
        let path = format!("/tmp/knot_{}_{}.sock", name, port);
        if std::path::Path::new(&path).exists() {
            if let Ok(s) = UnixStream::connect(&path).await {
                return Ok(KnotStream::Unix(s));
            }
        }
    }
    // Fallback a TCP (o default en Windows)
    let s = TcpStream::connect(format!("127.0.0.1:{}", port)).await?;
    Ok(KnotStream::Tcp(s))
}

async fn start_dynamic_byte_server(port: i32, tx: broadcast::Sender<String>) {
    #[cfg(unix)]
    {
        let path = format!("/tmp/knot_app_{}.sock", port);
        let _ = std::fs::remove_file(&path);
        if let Ok(listener) = tokio::net::UnixListener::bind(&path) {
            loop {
                if let Ok((s, _)) = listener.accept().await {
                    let tx_c = tx.clone();
                    tokio::spawn(async move {
                        handle_any_stream(Box::new(s), tx_c).await;
                    });
                }
            }
        }
    }

    #[cfg(not(unix))]
    {
        use tokio::net::TcpListener;
        let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        loop {
            if let Ok((s, _)) = listener.accept().await {
                let tx_c = tx.clone();
                tokio::spawn(async move {
                    handle_any_stream(Box::new(s), tx_c).await;
                });
            }
        }
    }
}

// Handler genérico que acepta CUALQUIER cosa que lea
async fn handle_any_stream(
    mut reader: Box<dyn AsyncRead + Send + Unpin>,
    tx: broadcast::Sender<String>,
) {
    let mut buf = vec![0u8; 65536];
    while let Ok(n) = reader.read(&mut buf).await {
        if n == 0 {
            break;
        }
        let _ = tx.send(String::from_utf8_lossy(&buf[..n]).to_string());
    }
}
