//! Native Wayland drag-and-drop using wl_data_device protocol
//!
//! This module provides direct access to Wayland's drag-and-drop protocol,
//! bypassing GTK's abstraction layer which doesn't properly handle cross-window
//! drags within the same application.
//!
//! Currently only provides surface registration for window label tracking.
//! The event handling infrastructure exists but callback functionality
//! was removed as unused.

use std::collections::HashMap;
use std::os::unix::io::OwnedFd;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use wayland_client::{
    delegate_noop,
    protocol::{
        wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source,
        wl_registry, wl_seat, wl_surface,
    },
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};

/// Shared state for Wayland drag-and-drop (thread-safe)
/// This is shared between the event loop thread and the main thread
struct SharedDndState {
    /// Map from wl_surface ID to window label
    surface_to_label: HashMap<u32, String>,
}

impl SharedDndState {
    fn new() -> Self {
        Self {
            surface_to_label: HashMap::new(),
        }
    }
}

/// Global shared state - accessible from both event loop thread and main thread
fn global_shared_state() -> &'static Arc<Mutex<SharedDndState>> {
    static INSTANCE: OnceLock<Arc<Mutex<SharedDndState>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Arc::new(Mutex::new(SharedDndState::new())))
}

/// State for Wayland event handling (used by event loop thread)
struct WaylandEventState {
    /// Current data offer (for incoming drags)
    current_offer: Option<wl_data_offer::WlDataOffer>,
    /// Available MIME types in current offer
    offer_mime_types: Vec<String>,
    /// Data device manager
    data_device_manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    /// Data device for the seat
    data_device: Option<wl_data_device::WlDataDevice>,
    /// Current outgoing data source
    current_source: Option<wl_data_source::WlDataSource>,
    /// Data to provide when source is requested
    source_data: HashMap<String, Vec<u8>>,
}

impl WaylandEventState {
    fn new() -> Self {
        Self {
            current_offer: None,
            offer_mime_types: Vec::new(),
            data_device_manager: None,
            data_device: None,
            current_source: None,
            source_data: HashMap::new(),
        }
    }
}

/// App data passed to Wayland event handlers
struct WaylandApp {
    /// Event-specific state (only accessed by event loop thread)
    event_state: WaylandEventState,
    /// Shared state (accessed by both event loop and main thread)
    #[allow(dead_code)] // Kept for future event callback support
    shared_state: Arc<Mutex<SharedDndState>>,
}

// Implement Dispatch for wl_registry to handle global objects
impl Dispatch<wl_registry::WlRegistry, ()> for WaylandApp {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_data_device_manager" => {
                    eprintln!("[TiddlyDesktop] Wayland: Found wl_data_device_manager v{}", version);
                    let manager = registry.bind::<wl_data_device_manager::WlDataDeviceManager, _, _>(
                        name,
                        version.min(3), // Use version 3 for dnd_actions support
                        qh,
                        (),
                    );
                    state.event_state.data_device_manager = Some(manager);
                }
                "wl_seat" => {
                    eprintln!("[TiddlyDesktop] Wayland: Found wl_seat v{}", version);
                    let _seat = registry.bind::<wl_seat::WlSeat, _, _>(
                        name,
                        version.min(7),
                        qh,
                        (),
                    );
                    // We'll get the data device after we have both seat and manager
                }
                _ => {}
            }
        }
    }
}

// Implement Dispatch for wl_seat
impl Dispatch<wl_seat::WlSeat, ()> for WaylandApp {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            eprintln!("[TiddlyDesktop] Wayland: Seat capabilities: {:?}", capabilities);

            // Create data device for this seat
            if let Some(ref manager) = state.event_state.data_device_manager {
                let data_device = manager.get_data_device(seat, qh, ());
                state.event_state.data_device = Some(data_device);
                eprintln!("[TiddlyDesktop] Wayland: Created data device for seat");
            }
        }
    }
}

// Implement Dispatch for wl_data_device_manager
delegate_noop!(WaylandApp: ignore wl_data_device_manager::WlDataDeviceManager);

// Implement Dispatch for wl_data_device - this is where drag events come in
impl Dispatch<wl_data_device::WlDataDevice, ()> for WaylandApp {
    fn event(
        state: &mut Self,
        _proxy: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                eprintln!("[TiddlyDesktop] Wayland: Data offer received");
                state.event_state.current_offer = Some(id);
                state.event_state.offer_mime_types.clear();
            }
            wl_data_device::Event::Enter { serial: _, surface, x, y, id: _ } => {
                let surface_id = surface.id().protocol_id();
                eprintln!(
                    "[TiddlyDesktop] Wayland: Drag entered surface {} at ({}, {})",
                    surface_id, x, y
                );
            }
            wl_data_device::Event::Motion { time: _, x, y } => {
                eprintln!("[TiddlyDesktop] Wayland: Drag motion at ({}, {})", x, y);
            }
            wl_data_device::Event::Leave => {
                eprintln!("[TiddlyDesktop] Wayland: Drag left surface");
            }
            wl_data_device::Event::Drop => {
                eprintln!("[TiddlyDesktop] Wayland: Drop!");
            }
            wl_data_device::Event::Selection { id: _ } => {
                // Selection (clipboard) changed - not relevant for DnD
            }
            _ => {}
        }
    }
}

// Implement Dispatch for wl_data_offer - handles MIME type announcements
impl Dispatch<wl_data_offer::WlDataOffer, ()> for WaylandApp {
    fn event(
        state: &mut Self,
        _proxy: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_offer::Event::Offer { mime_type } => {
                eprintln!("[TiddlyDesktop] Wayland: Offer has MIME type: {}", mime_type);
                state.event_state.offer_mime_types.push(mime_type);
            }
            wl_data_offer::Event::SourceActions { source_actions } => {
                eprintln!("[TiddlyDesktop] Wayland: Source actions: {:?}", source_actions);
            }
            wl_data_offer::Event::Action { dnd_action } => {
                eprintln!("[TiddlyDesktop] Wayland: Selected action: {:?}", dnd_action);
            }
            _ => {}
        }
    }
}

// Implement Dispatch for wl_data_source - handles outgoing drag requests
impl Dispatch<wl_data_source::WlDataSource, ()> for WaylandApp {
    fn event(
        state: &mut Self,
        _proxy: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Target { mime_type } => {
                eprintln!("[TiddlyDesktop] Wayland: Target accepted MIME type: {:?}", mime_type);
            }
            wl_data_source::Event::Send { mime_type, fd } => {
                eprintln!("[TiddlyDesktop] Wayland: Send requested for MIME type: {}", mime_type);
                // Write data to the file descriptor
                if let Some(data) = state.event_state.source_data.get(&mime_type) {
                    let fd = OwnedFd::from(fd);
                    use std::io::Write;
                    let mut file = std::fs::File::from(fd);
                    if let Err(e) = file.write_all(data) {
                        eprintln!("[TiddlyDesktop] Wayland: Failed to write data: {}", e);
                    }
                }
            }
            wl_data_source::Event::Cancelled => {
                eprintln!("[TiddlyDesktop] Wayland: Drag cancelled");
                state.event_state.current_source = None;
                state.event_state.source_data.clear();
            }
            wl_data_source::Event::DndDropPerformed => {
                eprintln!("[TiddlyDesktop] Wayland: DnD drop performed");
            }
            wl_data_source::Event::DndFinished => {
                eprintln!("[TiddlyDesktop] Wayland: DnD finished");
                state.event_state.current_source = None;
                state.event_state.source_data.clear();
            }
            wl_data_source::Event::Action { dnd_action } => {
                eprintln!("[TiddlyDesktop] Wayland: Action selected: {:?}", dnd_action);
            }
            _ => {}
        }
    }
}

// Implement Dispatch for wl_surface (we don't handle events, just need to track them)
delegate_noop!(WaylandApp: ignore wl_surface::WlSurface);

/// Run the Wayland event loop in a background thread
fn run_event_loop(
    mut event_queue: EventQueue<WaylandApp>,
    mut app: WaylandApp,
) {
    eprintln!("[TiddlyDesktop] Wayland: Starting event loop thread");

    loop {
        // Block until we have events to process
        match event_queue.blocking_dispatch(&mut app) {
            Ok(_) => {
                // Events processed successfully
            }
            Err(e) => {
                eprintln!("[TiddlyDesktop] Wayland: Event loop error: {}", e);
                // Connection errors are typically fatal - exit the loop
                eprintln!("[TiddlyDesktop] Wayland: Connection error, exiting event loop");
                break;
            }
        }
    }

    eprintln!("[TiddlyDesktop] Wayland: Event loop thread exiting");
}

/// Initialize the Wayland DnD system
/// Returns Ok(true) if Wayland is available, Ok(false) if not, Err on error
pub fn init() -> Result<bool, String> {
    // Check if we're on Wayland
    if std::env::var("WAYLAND_DISPLAY").is_err() {
        eprintln!("[TiddlyDesktop] Wayland: WAYLAND_DISPLAY not set, skipping Wayland DnD init");
        return Ok(false);
    }

    eprintln!("[TiddlyDesktop] Wayland: Initializing native DnD protocol handler");

    // Connect to Wayland display
    let conn = Connection::connect_to_env()
        .map_err(|e| format!("Failed to connect to Wayland: {}", e))?;

    let display = conn.display();

    // Get the global shared state
    let shared_state = global_shared_state().clone();

    // Create event queue
    let mut event_queue: EventQueue<WaylandApp> = conn.new_event_queue();
    let qh = event_queue.handle();

    // Get registry
    let _registry = display.get_registry(&qh, ());

    // Create app state for dispatching
    let mut app = WaylandApp {
        event_state: WaylandEventState::new(),
        shared_state: shared_state.clone(),
    };

    // Roundtrip to get globals
    event_queue.roundtrip(&mut app)
        .map_err(|e| format!("Wayland roundtrip failed: {}", e))?;

    // Another roundtrip to get data device after seat capabilities
    event_queue.roundtrip(&mut app)
        .map_err(|e| format!("Wayland roundtrip 2 failed: {}", e))?;

    eprintln!("[TiddlyDesktop] Wayland: Native DnD protocol handler initialized");

    // Start event loop in background thread
    thread::Builder::new()
        .name("wayland-dnd-events".to_string())
        .spawn(move || {
            run_event_loop(event_queue, app);
        })
        .map_err(|e| format!("Failed to spawn Wayland event thread: {}", e))?;

    eprintln!("[TiddlyDesktop] Wayland: Background event loop started");

    Ok(true)
}

/// Register a surface with its window label
pub fn register_surface(surface_id: u32, label: &str) {
    if let Ok(mut shared) = global_shared_state().lock() {
        shared.surface_to_label.insert(surface_id, label.to_string());
        eprintln!(
            "[TiddlyDesktop] Wayland: Registered surface {} for '{}'",
            surface_id, label
        );
    }
}
