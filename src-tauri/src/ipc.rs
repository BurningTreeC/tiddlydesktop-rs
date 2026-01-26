//! Inter-Process Communication for multi-process wiki architecture
//!
//! The main process runs an IPC server that coordinates between wiki processes.
//! Each wiki file has a "wiki group" - the primary wiki window plus any tiddler windows.
//! Changes in one window are broadcast to all windows in the same group.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

/// Default port for IPC server (main process)
pub const IPC_PORT: u16 = 45678;

/// IPC message types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IpcMessage {
    /// Wiki process registering with the broker
    Register {
        wiki_path: String,
        pid: u32,
        is_tiddler_window: bool,
        tiddler_title: Option<String>,
    },
    /// Wiki process unregistering (closing)
    Unregister {
        wiki_path: String,
        pid: u32,
    },
    /// Request to open a wiki (forwarded to main process)
    OpenWiki {
        path: String,
    },
    /// Request to open a tiddler in a new window
    OpenTiddlerWindow {
        wiki_path: String,
        tiddler_title: String,
        /// Optional startup tiddler for the window
        startup_tiddler: Option<String>,
    },
    /// Request to focus an existing wiki window
    FocusWiki {
        wiki_path: String,
    },
    /// Tiddler content changed - broadcast to wiki group
    TiddlerChanged {
        wiki_path: String,
        tiddler_title: String,
        tiddler_json: String,
        /// PID of sender (so it doesn't echo back)
        sender_pid: u32,
    },
    /// Tiddler deleted - broadcast to wiki group
    TiddlerDeleted {
        wiki_path: String,
        tiddler_title: String,
        sender_pid: u32,
    },
    /// Full wiki sync request (new window joining)
    RequestSync {
        wiki_path: String,
        requester_pid: u32,
    },
    /// Full wiki state (response to sync request)
    SyncState {
        wiki_path: String,
        /// JSON array of all tiddlers
        tiddlers_json: String,
    },
    /// Acknowledgment
    Ack {
        success: bool,
        message: Option<String>,
    },
    /// Update favicon for a wiki (sent from wiki process to main process)
    UpdateFavicon {
        wiki_path: String,
        favicon: Option<String>,
    },
    /// Ping/keepalive
    Ping,
    Pong,
}

/// A connected wiki process
#[allow(dead_code)]
struct WikiClient {
    stream: TcpStream,
    wiki_path: String,
    pid: u32,
    is_tiddler_window: bool,
}

/// IPC Server state (runs in main process)
pub struct IpcServer {
    /// Connected clients grouped by wiki path
    wiki_groups: Arc<Mutex<HashMap<String, Vec<WikiClient>>>>,
    /// All clients by PID for quick lookup
    clients_by_pid: Arc<Mutex<HashMap<u32, TcpStream>>>,
    /// Callback for opening wikis
    open_wiki_callback: Arc<Mutex<Option<Box<dyn Fn(String) + Send + 'static>>>>,
    /// Callback for opening tiddler windows
    open_tiddler_callback: Arc<Mutex<Option<Box<dyn Fn(String, String, Option<String>) + Send + 'static>>>>,
    /// Callback for updating wiki favicon
    update_favicon_callback: Arc<Mutex<Option<Box<dyn Fn(String, Option<String>) + Send + 'static>>>>,
}

impl IpcServer {
    pub fn new() -> Self {
        Self {
            wiki_groups: Arc::new(Mutex::new(HashMap::new())),
            clients_by_pid: Arc::new(Mutex::new(HashMap::new())),
            open_wiki_callback: Arc::new(Mutex::new(None)),
            open_tiddler_callback: Arc::new(Mutex::new(None)),
            update_favicon_callback: Arc::new(Mutex::new(None)),
        }
    }

    /// Set callback for when a wiki open is requested
    pub fn on_open_wiki<F>(&self, callback: F)
    where
        F: Fn(String) + Send + 'static,
    {
        *self.open_wiki_callback.lock().unwrap() = Some(Box::new(callback));
    }

    /// Set callback for when a tiddler window open is requested
    pub fn on_open_tiddler<F>(&self, callback: F)
    where
        F: Fn(String, String, Option<String>) + Send + 'static,
    {
        *self.open_tiddler_callback.lock().unwrap() = Some(Box::new(callback));
    }

    /// Set callback for when a wiki favicon update is requested
    pub fn on_update_favicon<F>(&self, callback: F)
    where
        F: Fn(String, Option<String>) + Send + 'static,
    {
        *self.update_favicon_callback.lock().unwrap() = Some(Box::new(callback));
    }

    /// Start the IPC server (blocks, run in separate thread)
    pub fn start(&self) -> std::io::Result<()> {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", IPC_PORT))?;
        eprintln!("[IPC] Server listening on port {}", IPC_PORT);

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let wiki_groups = self.wiki_groups.clone();
                    let clients_by_pid = self.clients_by_pid.clone();
                    let open_wiki_cb = self.open_wiki_callback.clone();
                    let open_tiddler_cb = self.open_tiddler_callback.clone();
                    let update_favicon_cb = self.update_favicon_callback.clone();

                    thread::spawn(move || {
                        if let Err(e) = handle_client(
                            stream,
                            wiki_groups,
                            clients_by_pid,
                            open_wiki_cb,
                            open_tiddler_cb,
                            update_favicon_cb,
                        ) {
                            eprintln!("[IPC] Client handler error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    eprintln!("[IPC] Accept error: {}", e);
                }
            }
        }
        Ok(())
    }

    /// Send a focus window request to all clients for a specific wiki
    pub fn send_focus_window(&self, wiki_path: &str) -> std::io::Result<()> {
        let msg = IpcMessage::FocusWiki {
            wiki_path: wiki_path.to_string(),
        };
        let json = serde_json::to_string(&msg)?;

        let groups = self.wiki_groups.lock().unwrap();
        if let Some(clients) = groups.get(wiki_path) {
            for client in clients {
                let mut s = &client.stream;
                let _ = writeln!(s, "{}", json);
            }
        }
        Ok(())
    }
}

fn handle_client(
    stream: TcpStream,
    wiki_groups: Arc<Mutex<HashMap<String, Vec<WikiClient>>>>,
    clients_by_pid: Arc<Mutex<HashMap<u32, TcpStream>>>,
    open_wiki_cb: Arc<Mutex<Option<Box<dyn Fn(String) + Send + 'static>>>>,
    open_tiddler_cb: Arc<Mutex<Option<Box<dyn Fn(String, String, Option<String>) + Send + 'static>>>>,
    update_favicon_cb: Arc<Mutex<Option<Box<dyn Fn(String, Option<String>) + Send + 'static>>>>,
) -> std::io::Result<()> {
    let peer_addr = stream.peer_addr()?;
    eprintln!("[IPC] New connection from {}", peer_addr);

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut write_stream = stream.try_clone()?;
    let mut client_wiki_path: Option<String> = None;
    let mut client_pid: Option<u32> = None;

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // Connection closed
                eprintln!("[IPC] Connection closed from {}", peer_addr);
                break;
            }
            Ok(_) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                match serde_json::from_str::<IpcMessage>(line) {
                    Ok(msg) => {
                        match &msg {
                            IpcMessage::Register { wiki_path, pid, is_tiddler_window, .. } => {
                                eprintln!("[IPC] Register: wiki={}, pid={}, tiddler_window={}",
                                    wiki_path, pid, is_tiddler_window);

                                client_wiki_path = Some(wiki_path.clone());
                                client_pid = Some(*pid);

                                // Add to wiki group
                                let mut groups = wiki_groups.lock().unwrap();
                                let group = groups.entry(wiki_path.clone()).or_insert_with(Vec::new);
                                group.push(WikiClient {
                                    stream: stream.try_clone()?,
                                    wiki_path: wiki_path.clone(),
                                    pid: *pid,
                                    is_tiddler_window: *is_tiddler_window,
                                });

                                // Track by PID
                                clients_by_pid.lock().unwrap().insert(*pid, stream.try_clone()?);

                                // Send ack
                                let ack = IpcMessage::Ack { success: true, message: None };
                                let _ = writeln!(write_stream, "{}", serde_json::to_string(&ack)?);
                            }

                            IpcMessage::Unregister { wiki_path, pid } => {
                                eprintln!("[IPC] Unregister: wiki={}, pid={}", wiki_path, pid);

                                // Remove from wiki group
                                let mut groups = wiki_groups.lock().unwrap();
                                if let Some(group) = groups.get_mut(wiki_path) {
                                    group.retain(|c| c.pid != *pid);
                                    if group.is_empty() {
                                        groups.remove(wiki_path);
                                    }
                                }

                                // Remove from PID tracking
                                clients_by_pid.lock().unwrap().remove(pid);
                            }

                            IpcMessage::OpenWiki { path } => {
                                eprintln!("[IPC] OpenWiki request: {}", path);
                                if let Some(ref cb) = *open_wiki_cb.lock().unwrap() {
                                    cb(path.clone());
                                }
                                let ack = IpcMessage::Ack { success: true, message: None };
                                let _ = writeln!(write_stream, "{}", serde_json::to_string(&ack)?);
                            }

                            IpcMessage::OpenTiddlerWindow { wiki_path, tiddler_title, startup_tiddler } => {
                                eprintln!("[IPC] OpenTiddlerWindow request: wiki={}, tiddler={}",
                                    wiki_path, tiddler_title);
                                if let Some(ref cb) = *open_tiddler_cb.lock().unwrap() {
                                    cb(wiki_path.clone(), tiddler_title.clone(), startup_tiddler.clone());
                                }
                                let ack = IpcMessage::Ack { success: true, message: None };
                                let _ = writeln!(write_stream, "{}", serde_json::to_string(&ack)?);
                            }

                            IpcMessage::TiddlerChanged { wiki_path, sender_pid, .. } => {
                                // Broadcast to all other clients in the same wiki group
                                let groups = wiki_groups.lock().unwrap();
                                if let Some(clients) = groups.get(wiki_path) {
                                    for client in clients {
                                        if client.pid != *sender_pid {
                                            if let Ok(json) = serde_json::to_string(&msg) {
                                                let mut s = &client.stream;
                                                let _ = writeln!(s, "{}", json);
                                            }
                                        }
                                    }
                                }
                            }

                            IpcMessage::TiddlerDeleted { wiki_path, sender_pid, .. } => {
                                // Broadcast to all other clients in the same wiki group
                                let groups = wiki_groups.lock().unwrap();
                                if let Some(clients) = groups.get(wiki_path) {
                                    for client in clients {
                                        if client.pid != *sender_pid {
                                            if let Ok(json) = serde_json::to_string(&msg) {
                                                let mut s = &client.stream;
                                                let _ = writeln!(s, "{}", json);
                                            }
                                        }
                                    }
                                }
                            }

                            IpcMessage::RequestSync { wiki_path, requester_pid } => {
                                eprintln!("[IPC] SyncRequest from pid {} for {}", requester_pid, wiki_path);
                                // Find the primary (non-tiddler) window for this wiki and ask it to send state
                                let groups = wiki_groups.lock().unwrap();
                                if let Some(clients) = groups.get(wiki_path) {
                                    for client in clients {
                                        if !client.is_tiddler_window && client.pid != *requester_pid {
                                            // Ask this client to send sync state
                                            if let Ok(json) = serde_json::to_string(&msg) {
                                                let mut s = &client.stream;
                                                let _ = writeln!(s, "{}", json);
                                            }
                                            break;
                                        }
                                    }
                                }
                            }

                            IpcMessage::SyncState { wiki_path, .. } => {
                                // Forward to all tiddler windows that need sync
                                let groups = wiki_groups.lock().unwrap();
                                if let Some(clients) = groups.get(wiki_path) {
                                    for client in clients {
                                        if client.is_tiddler_window {
                                            if let Ok(json) = serde_json::to_string(&msg) {
                                                let mut s = &client.stream;
                                                let _ = writeln!(s, "{}", json);
                                            }
                                        }
                                    }
                                }
                            }

                            IpcMessage::UpdateFavicon { wiki_path, favicon } => {
                                eprintln!("[IPC] UpdateFavicon request: wiki={}", wiki_path);
                                if let Some(ref cb) = *update_favicon_cb.lock().unwrap() {
                                    cb(wiki_path.clone(), favicon.clone());
                                }
                                let ack = IpcMessage::Ack { success: true, message: None };
                                let _ = writeln!(write_stream, "{}", serde_json::to_string(&ack)?);
                            }

                            IpcMessage::Ping => {
                                let pong = IpcMessage::Pong;
                                let _ = writeln!(write_stream, "{}", serde_json::to_string(&pong)?);
                            }

                            _ => {}
                        }
                    }
                    Err(e) => {
                        eprintln!("[IPC] Parse error: {} for line: {}", e, line);
                    }
                }
            }
            Err(e) => {
                eprintln!("[IPC] Read error: {}", e);
                break;
            }
        }
    }

    // Clean up on disconnect
    if let (Some(wiki_path), Some(pid)) = (client_wiki_path, client_pid) {
        let mut groups = wiki_groups.lock().unwrap();
        if let Some(group) = groups.get_mut(&wiki_path) {
            group.retain(|c| c.pid != pid);
            if group.is_empty() {
                groups.remove(&wiki_path);
            }
        }
        clients_by_pid.lock().unwrap().remove(&pid);
        eprintln!("[IPC] Cleaned up client: wiki={}, pid={}", wiki_path, pid);
    }

    Ok(())
}

/// IPC Client (runs in wiki processes)
pub struct IpcClient {
    stream: Option<TcpStream>,
    wiki_path: String,
    is_tiddler_window: bool,
    tiddler_title: Option<String>,
}

impl IpcClient {
    pub fn new(wiki_path: String, is_tiddler_window: bool, tiddler_title: Option<String>) -> Self {
        Self {
            stream: None,
            wiki_path,
            is_tiddler_window,
            tiddler_title,
        }
    }

    /// Connect to the IPC server
    pub fn connect(&mut self) -> std::io::Result<()> {
        let stream = TcpStream::connect(format!("127.0.0.1:{}", IPC_PORT))?;
        stream.set_nodelay(true)?;
        self.stream = Some(stream);

        // Register with the server
        let pid = std::process::id();
        let msg = IpcMessage::Register {
            wiki_path: self.wiki_path.clone(),
            pid,
            is_tiddler_window: self.is_tiddler_window,
            tiddler_title: self.tiddler_title.clone(),
        };
        self.send(&msg)?;

        eprintln!("[IPC Client] Connected and registered: wiki={}, pid={}", self.wiki_path, pid);
        Ok(())
    }

    /// Get a clone of the stream for the listener thread
    /// Returns None if not connected
    pub fn get_listener_stream(&self) -> Option<TcpStream> {
        self.stream.as_ref().and_then(|s| s.try_clone().ok())
    }

    /// Send a message to the server
    pub fn send(&mut self, msg: &IpcMessage) -> std::io::Result<()> {
        if let Some(ref mut stream) = self.stream {
            let json = serde_json::to_string(msg)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            writeln!(stream, "{}", json)?;
            stream.flush()?;
        }
        Ok(())
    }

    /// Notify that a tiddler changed
    pub fn notify_tiddler_changed(&mut self, tiddler_title: &str, tiddler_json: &str) -> std::io::Result<()> {
        let msg = IpcMessage::TiddlerChanged {
            wiki_path: self.wiki_path.clone(),
            tiddler_title: tiddler_title.to_string(),
            tiddler_json: tiddler_json.to_string(),
            sender_pid: std::process::id(),
        };
        self.send(&msg)
    }

    /// Notify that a tiddler was deleted
    pub fn notify_tiddler_deleted(&mut self, tiddler_title: &str) -> std::io::Result<()> {
        let msg = IpcMessage::TiddlerDeleted {
            wiki_path: self.wiki_path.clone(),
            tiddler_title: tiddler_title.to_string(),
            sender_pid: std::process::id(),
        };
        self.send(&msg)
    }

    /// Request to open a tiddler window
    pub fn request_open_tiddler(&mut self, tiddler_title: &str, startup_tiddler: Option<&str>) -> std::io::Result<()> {
        let msg = IpcMessage::OpenTiddlerWindow {
            wiki_path: self.wiki_path.clone(),
            tiddler_title: tiddler_title.to_string(),
            startup_tiddler: startup_tiddler.map(|s| s.to_string()),
        };
        self.send(&msg)
    }

    /// Request full wiki sync (for new tiddler windows)
    pub fn request_sync(&mut self) -> std::io::Result<()> {
        let msg = IpcMessage::RequestSync {
            wiki_path: self.wiki_path.clone(),
            requester_pid: std::process::id(),
        };
        self.send(&msg)
    }

    /// Send full wiki state (response to sync request)
    pub fn send_sync_state(&mut self, tiddlers_json: &str) -> std::io::Result<()> {
        let msg = IpcMessage::SyncState {
            wiki_path: self.wiki_path.clone(),
            tiddlers_json: tiddlers_json.to_string(),
        };
        self.send(&msg)
    }

    /// Send favicon update to main process
    pub fn send_update_favicon(&mut self, wiki_path: &str, favicon: Option<String>) -> std::io::Result<()> {
        let msg = IpcMessage::UpdateFavicon {
            wiki_path: wiki_path.to_string(),
            favicon,
        };
        self.send(&msg)
    }
}

impl Drop for IpcClient {
    fn drop(&mut self) {
        // Unregister on drop
        let msg = IpcMessage::Unregister {
            wiki_path: self.wiki_path.clone(),
            pid: std::process::id(),
        };
        let _ = self.send(&msg);
    }
}

/// Try to connect to existing IPC server, returns None if server not running
pub fn try_connect(wiki_path: &str, is_tiddler_window: bool, tiddler_title: Option<String>) -> Option<IpcClient> {
    let mut client = IpcClient::new(wiki_path.to_string(), is_tiddler_window, tiddler_title);
    match client.connect() {
        Ok(_) => Some(client),
        Err(_) => None,
    }
}

/// Run a listener loop on a stream (blocking, for use in a separate thread)
/// This allows wiki processes to receive messages from the IPC server
pub fn run_listener<F>(stream: TcpStream, mut callback: F)
where
    F: FnMut(IpcMessage),
{
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                eprintln!("[IPC Listener] Server disconnected");
                break;
            }
            Ok(_) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<IpcMessage>(line) {
                    Ok(msg) => callback(msg),
                    Err(e) => eprintln!("[IPC Listener] Parse error: {}", e),
                }
            }
            Err(e) => {
                eprintln!("[IPC Listener] Read error: {}", e);
                break;
            }
        }
    }
}
