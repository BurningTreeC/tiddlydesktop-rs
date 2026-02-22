//! UDP broadcast discovery for LAN sync.
//!
//! Periodically broadcasts a beacon on UDP port 45699 and listens for beacons
//! from other TiddlyDesktop instances on the same LAN. Much simpler and more
//! reliable than mDNS — works without MulticastLock on Android and without
//! Avahi on Linux.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// UDP port for discovery beacons
const DISCOVERY_PORT: u16 = 45699;

/// How often to send a beacon (2s for fast discovery)
const BEACON_INTERVAL: Duration = Duration::from_secs(2);

/// How long before a peer is considered lost (no beacon received)
const PEER_TIMEOUT: Duration = Duration::from_secs(10);

/// Events from the discovery system
#[derive(Debug, Clone)]
pub enum DiscoveryEvent {
    /// A TiddlyDesktop instance was found on the LAN
    PeerDiscovered {
        device_id: String,
        device_name: String,
        addr: String,
        port: u16,
        /// Room codes the peer is currently in (cleartext, for backward compat)
        rooms: Vec<String>,
        /// HMAC-SHA256 hashes of room codes (preferred for matching)
        room_hashes: Vec<String>,
    },
    /// A TiddlyDesktop instance went away
    PeerLost { device_id: String },
}

/// Beacon packet sent/received via UDP broadcast
#[derive(serde::Serialize, serde::Deserialize)]
struct Beacon {
    /// Protocol marker
    td: u8,
    /// Device UUID
    id: String,
    /// Human-readable device name
    name: String,
    /// WebSocket sync server port
    port: u16,
    /// Room codes this device is currently in (cleartext, for backward compat with old clients)
    #[serde(default)]
    rooms: Vec<String>,
    /// HMAC-SHA256 hashes of room codes (preferred for matching — doesn't reveal codes to observers)
    #[serde(default)]
    room_hashes: Vec<String>,
}

/// Hash a room code for use in discovery beacons.
/// Uses HMAC-SHA256 with a fixed label, truncated to first 8 bytes (16 hex chars).
/// This prevents passive LAN observers from learning room codes while still
/// allowing peers to match rooms by hashing their own codes and comparing.
pub fn hash_room_code(room_code: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(b"tiddlydesktop-discovery-room-hash")
        .expect("HMAC can take key of any size");
    mac.update(room_code.as_bytes());
    let result = mac.finalize().into_bytes();
    result[..8]
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

/// The UDP broadcast discovery manager
pub struct DiscoveryManager {
    shutdown: Arc<AtomicBool>,
}

impl DiscoveryManager {
    /// Start the discovery manager: broadcast our presence and listen for peers.
    /// `connected_peers` is checked before emitting PeerLost — peers with active
    /// WebSocket connections are never timed out by the discovery layer.
    /// `active_room_codes` is a shared list of room codes we're currently in,
    /// included in each beacon so peers can auto-connect for shared rooms.
    pub fn new(
        device_id: &str,
        device_name: &str,
        port: u16,
        event_tx: mpsc::UnboundedSender<DiscoveryEvent>,
        connected_peers: Arc<std::sync::RwLock<HashSet<String>>>,
        active_room_codes: Arc<std::sync::RwLock<Vec<String>>>,
    ) -> Result<Self, String> {
        let shutdown = Arc::new(AtomicBool::new(false));

        // Create UDP socket with SO_REUSEADDR + SO_BROADCAST
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
            .map_err(|e| format!("Failed to create discovery socket: {}", e))?;
        socket
            .set_reuse_address(true)
            .map_err(|e| format!("Failed to set SO_REUSEADDR: {}", e))?;
        // SO_REUSEPORT on platforms that support it
        #[cfg(all(not(target_os = "windows"), not(target_os = "android")))]
        {
            // socket2 0.5 doesn't expose set_reuse_port directly,
            // but SO_REUSEADDR on Linux/macOS is sufficient for UDP broadcast
        }
        socket
            .set_broadcast(true)
            .map_err(|e| format!("Failed to set SO_BROADCAST: {}", e))?;
        socket
            .set_nonblocking(true)
            .map_err(|e| format!("Failed to set nonblocking: {}", e))?;

        let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT);
        socket
            .bind(&bind_addr.into())
            .map_err(|e| format!("Failed to bind discovery socket to port {}: {}", DISCOVERY_PORT, e))?;

        let socket: std::net::UdpSocket = socket.into();

        let our_id = device_id.to_string();
        let our_name = device_name.to_string();
        let our_port = port;

        let our_device_id = device_id.to_string();
        let shutdown_clone = shutdown.clone();

        eprintln!(
            "[LAN Sync] UDP discovery started on port {} (beacon every {}s, timeout {}s)",
            DISCOVERY_PORT,
            BEACON_INTERVAL.as_secs(),
            PEER_TIMEOUT.as_secs()
        );

        // Spawn discovery thread
        std::thread::spawn(move || {
            let broadcast_addr: SocketAddr =
                SocketAddrV4::new(Ipv4Addr::BROADCAST, DISCOVERY_PORT).into();
            let mut peers: HashMap<String, Instant> = HashMap::new();
            // Track when we last emitted PeerDiscovered per peer, for throttled re-emission
            let mut last_emitted: HashMap<String, Instant> = HashMap::new();
            let mut buf = [0u8; 2048]; // Larger buffer for room codes

            // Helper to serialize the beacon with current room codes + hashes
            let make_beacon_data = |td: u8| -> Vec<u8> {
                let rooms = active_room_codes.read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let room_hashes = rooms.iter().map(|r| hash_room_code(r)).collect();
                let beacon = Beacon {
                    td,
                    id: our_id.clone(),
                    name: our_name.clone(),
                    port: our_port,
                    rooms,
                    room_hashes,
                };
                serde_json::to_vec(&beacon).unwrap_or_default()
            };

            // Send a burst of beacons on startup so peers detect us quickly
            // (UDP is unreliable — a single beacon could be lost)
            for i in 0..3 {
                let beacon_data = make_beacon_data(1);
                let _ = socket.send_to(&beacon_data, broadcast_addr);
                if i < 2 {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            let mut last_broadcast = Instant::now();

            loop {
                if shutdown_clone.load(Ordering::Relaxed) {
                    // Send goodbye beacon so peers remove us immediately
                    let goodbye_data = make_beacon_data(0);
                    for _ in 0..3 {
                        let _ = socket.send_to(&goodbye_data, broadcast_addr);
                    }
                    break;
                }

                // Send beacon periodically (re-serialize each time to pick up room changes)
                if last_broadcast.elapsed() >= BEACON_INTERVAL {
                    let beacon_data = make_beacon_data(1);
                    let _ = socket.send_to(&beacon_data, broadcast_addr);
                    last_broadcast = Instant::now();

                    // Check for timed-out peers (skip peers with active WebSocket connections)
                    let now = Instant::now();
                    let active = connected_peers.read().unwrap_or_else(|e| e.into_inner());
                    let timed_out: Vec<String> = peers
                        .iter()
                        .filter(|(id, last_seen)| {
                            now.duration_since(**last_seen) > PEER_TIMEOUT
                                && !active.contains(*id)
                        })
                        .map(|(id, _)| id.clone())
                        .collect();
                    drop(active);
                    for id in timed_out {
                        peers.remove(&id);
                        eprintln!("[LAN Sync] Peer timed out: {}", id);
                        let _ = event_tx.send(DiscoveryEvent::PeerLost {
                            device_id: id,
                        });
                    }
                }

                // Receive beacons from other devices (drain all buffered packets)
                match socket.recv_from(&mut buf) {
                    Ok((len, _src_addr)) => {
                        if let Ok(beacon) = serde_json::from_slice::<Beacon>(&buf[..len]) {
                            // Ignore our own beacons
                            if beacon.id == our_device_id {
                                continue;
                            }

                            // td: 0 = goodbye beacon — remove peer immediately
                            if beacon.td == 0 {
                                if peers.remove(&beacon.id).is_some() {
                                    eprintln!("[LAN Sync] Peer said goodbye: {} ({})", beacon.name, beacon.id);
                                    let _ = event_tx.send(DiscoveryEvent::PeerLost {
                                        device_id: beacon.id,
                                    });
                                }
                                continue;
                            }

                            // td: 1 = normal presence beacon
                            if beacon.td != 1 {
                                continue;
                            }

                            let is_new = !peers.contains_key(&beacon.id);
                            peers.insert(beacon.id.clone(), Instant::now());

                            // Emit PeerDiscovered for new peers, and re-emit
                            // periodically for known peers that aren't connected
                            // via WebSocket (so the sync manager can retry).
                            let should_emit = if is_new {
                                true
                            } else {
                                let active = connected_peers.read()
                                    .unwrap_or_else(|e| e.into_inner());
                                if !active.contains(&beacon.id) {
                                    // Re-emit every 10s for unconnected peers
                                    match last_emitted.get(&beacon.id) {
                                        Some(t) if t.elapsed() < Duration::from_secs(10) => false,
                                        _ => true,
                                    }
                                } else {
                                    // Connected — clean up tracking
                                    last_emitted.remove(&beacon.id);
                                    false
                                }
                            };

                            if should_emit {
                                last_emitted.insert(beacon.id.clone(), Instant::now());
                                let addr = match _src_addr {
                                    SocketAddr::V4(v4) => v4.ip().to_string(),
                                    SocketAddr::V6(v6) => v6.ip().to_string(),
                                };
                                if is_new {
                                    eprintln!(
                                        "[LAN Sync] Discovered peer: {} ({}) at {}:{} rooms={:?}",
                                        beacon.name, beacon.id, addr, beacon.port, beacon.rooms
                                    );
                                }
                                let _ = event_tx.send(DiscoveryEvent::PeerDiscovered {
                                    device_id: beacon.id,
                                    device_name: beacon.name,
                                    addr,
                                    port: beacon.port,
                                    rooms: beacon.rooms,
                                    room_hashes: beacon.room_hashes,
                                });
                            }
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // No data available — sleep briefly to avoid busy-spinning
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }

            eprintln!("[LAN Sync] UDP discovery thread stopped");
        });

        Ok(Self { shutdown })
    }

    /// Stop discovery
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        eprintln!("[LAN Sync] UDP discovery shut down");
    }
}

impl Drop for DiscoveryManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}
