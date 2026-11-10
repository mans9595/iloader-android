//! In-process replacement for usbmuxd on Android.
//!
//! Desktop platforms have a system service (usbmuxd / Apple Mobile Device Service) that owns
//! the USB connection to the iPhone and multiplexes TCP-like streams over it. Android has no
//! such service, so this module does both halves itself:
//!
//! * the device side: the USB multiplexing protocol, spoken over a usbfs file descriptor
//!   handed to us by Android's `UsbManager` (see `MainActivity.kt`), and
//! * the client side: the usbmuxd plist protocol on a private socket, so the rest of the
//!   app (and the `idevice` crate) can keep talking to "usbmuxd" as usual.

use std::{
    collections::HashMap,
    io,
    os::fd::{BorrowedFd, OwnedFd},
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU8, AtomicU16, AtomicU32, Ordering},
    },
    time::Duration,
};

use idevice::usbmuxd::UsbmuxdAddr;
use nusb::{
    MaybeFuture,
    descriptors::TransferType,
    transfer::{Buffer, Bulk, Direction, In, Out},
};
use tauri::{AppHandle, Emitter};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{Notify, mpsc, oneshot, watch},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

const MUX_PROTO_VERSION: u32 = 0;
const MUX_PROTO_CONTROL: u32 = 1;
const MUX_PROTO_SETUP: u32 = 2;
const MUX_PROTO_TCP: u32 = 6;

const TH_SYN: u8 = 0x02;
const TH_RST: u8 = 0x04;
const TH_ACK: u8 = 0x10;

/// Size of each bulk IN request, same as usbmuxd's USB_MRU.
const USB_MRU: usize = 16384;
/// Largest mux packet we accept from the device.
const MAX_PACKET: usize = 1 << 20;
/// Receive window we advertise to the device, same as usbmuxd.
const TX_WINDOW: u32 = 131072;
/// Payload bytes per outgoing TCP packet; keeps every packet within one USB_MRU.
const MAX_SEGMENT: usize = USB_MRU - 64;

const RESULT_OK: u64 = 0;
const RESULT_BADCOMMAND: u64 = 1;
const RESULT_BADDEV: u64 = 2;
const RESULT_CONNREFUSED: u64 = 3;

pub const STATE_NONE: u8 = 0;
pub const STATE_STARTING: u8 = 1;
pub const STATE_READY: u8 = 2;
pub const STATE_FAILED: u8 = 3;

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
static SOCKET_ADDR: OnceLock<UsbmuxdAddr> = OnceLock::new();
static APP: OnceLock<AppHandle> = OnceLock::new();
static DEVICE: Mutex<Option<Arc<MuxDevice>>> = Mutex::new(None);
static STATE: AtomicU8 = AtomicU8::new(STATE_NONE);
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);
static NEXT_DEVICE_ID: AtomicU32 = AtomicU32::new(1);

fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("usbmux")
            .enable_all()
            .build()
            .expect("failed to build usbmux runtime")
    })
}

fn set_state(state: u8, error: Option<String>) {
    STATE.store(state, Ordering::SeqCst);
    *LAST_ERROR.lock().unwrap() = error;
    if let Some(app) = APP.get() {
        let _ = app.emit("usb-changed", state);
    }
}

/// Current USB state, one of the `STATE_*` constants.
pub fn usb_state() -> u8 {
    STATE.load(Ordering::SeqCst)
}

/// Why the last USB attach failed, if it did.
pub fn last_error() -> Option<String> {
    LAST_ERROR.lock().unwrap().clone()
}

/// Address the rest of the app should use to reach "usbmuxd".
pub fn addr() -> Option<UsbmuxdAddr> {
    SOCKET_ADDR.get().cloned()
}

fn current_device() -> Option<Arc<MuxDevice>> {
    DEVICE
        .lock()
        .unwrap()
        .clone()
        .filter(|d| d.is_ready() && !d.is_dead())
}

enum Listener {
    Unix(std::os::unix::net::UnixListener),
    Tcp(std::net::TcpListener),
}

/// Starts the usbmuxd-compatible socket server. Call once at app startup.
pub fn start(app: AppHandle, data_dir: PathBuf) {
    let _ = APP.set(app);
    let _ = std::fs::create_dir_all(data_dir.join("lockdown"));
    let socket_path = data_dir.join("usbmuxd.sock");
    let _ = DATA_DIR.set(data_dir);
    if SOCKET_ADDR.get().is_some() {
        return;
    }

    let _ = std::fs::remove_file(&socket_path);
    let listener = match std::os::unix::net::UnixListener::bind(&socket_path) {
        Ok(l) => {
            let _ = SOCKET_ADDR.set(UsbmuxdAddr::UnixSocket(
                socket_path.to_string_lossy().to_string(),
            ));
            Listener::Unix(l)
        }
        Err(e) => {
            warn!("Could not bind usbmux unix socket ({e}), falling back to loopback TCP");
            match std::net::TcpListener::bind(("127.0.0.1", 0)) {
                Ok(l) => {
                    let Ok(local) = l.local_addr() else {
                        error!("Could not read usbmux TCP listener address");
                        return;
                    };
                    let _ = SOCKET_ADDR.set(UsbmuxdAddr::TcpSocket(local));
                    Listener::Tcp(l)
                }
                Err(e) => {
                    error!("Could not bind usbmux TCP socket: {e}");
                    return;
                }
            }
        }
    };

    runtime().spawn(async move {
        match listener {
            Listener::Unix(l) => {
                if let Err(e) = l.set_nonblocking(true) {
                    error!("usbmux listener: {e}");
                    return;
                }
                let l = match tokio::net::UnixListener::from_std(l) {
                    Ok(l) => l,
                    Err(e) => {
                        error!("usbmux listener: {e}");
                        return;
                    }
                };
                loop {
                    match l.accept().await {
                        Ok((stream, _)) => {
                            tokio::spawn(handle_client(stream));
                        }
                        Err(e) => {
                            warn!("usbmux accept failed: {e}");
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                    }
                }
            }
            Listener::Tcp(l) => {
                if let Err(e) = l.set_nonblocking(true) {
                    error!("usbmux listener: {e}");
                    return;
                }
                let l = match tokio::net::TcpListener::from_std(l) {
                    Ok(l) => l,
                    Err(e) => {
                        error!("usbmux listener: {e}");
                        return;
                    }
                };
                loop {
                    match l.accept().await {
                        Ok((stream, _)) => {
                            let _ = stream.set_nodelay(true);
                            tokio::spawn(handle_client(stream));
                        }
                        Err(e) => {
                            warn!("usbmux accept failed: {e}");
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                    }
                }
            }
        }
    });
}

/// Called (via JNI) when Android has opened an Apple USB device for us.
///
/// `fd` stays owned by the Java `UsbDeviceConnection`; we work on a duplicate.
pub fn usb_attached(fd: i32, serial: String, product_id: u16) {
    // SAFETY: the Java side keeps its UsbDeviceConnection (and so this fd) open for the
    // duration of this call.
    let fd = match unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned() {
        Ok(fd) => fd,
        Err(e) => {
            set_state(
                STATE_FAILED,
                Some(format!("Could not duplicate USB file descriptor: {e}")),
            );
            return;
        }
    };

    usb_detached();
    set_state(STATE_STARTING, None);
    info!("Apple USB device attached (product {product_id:#06x}), setting up usbmux");

    runtime().spawn(async move {
        match setup_device(fd, serial, product_id).await {
            Ok(dev) => {
                info!("usbmux ready for device {}", dev.udid);
                *DEVICE.lock().unwrap() = Some(dev);
                set_state(STATE_READY, None);
            }
            Err(e) => {
                error!("USB setup failed: {e}");
                set_state(STATE_FAILED, Some(e));
            }
        }
    });
}

/// Called (via JNI) when the device was unplugged or the connection was closed.
pub fn usb_detached() {
    let old = DEVICE.lock().unwrap().take();
    if let Some(dev) = old {
        info!("usbmux device {} detached", dev.udid);
        dev.kill();
    }
    set_state(STATE_NONE, None);
}

struct UsbHandles {
    // Keep the device and interface alive for as long as the endpoints are in use.
    _device: nusb::Device,
    _interface: nusb::Interface,
    ep_in: nusb::Endpoint<Bulk, In>,
    ep_out: nusb::Endpoint<Bulk, Out>,
    serial: Option<String>,
}

fn open_usb(fd: OwnedFd) -> Result<UsbHandles, String> {
    let device = nusb::Device::from_fd(fd)
        .wait()
        .map_err(|e| format!("Could not open USB device: {e}"))?;

    // (configuration value, interface number, IN endpoint, OUT endpoint)
    let mut candidates: Vec<(u8, u8, u8, u8)> = Vec::new();
    for config in device.configurations() {
        for alt in config.interface_alt_settings() {
            if alt.class() != 0xff || alt.subclass() != 0xfe || alt.protocol() != 2 {
                continue;
            }
            let mut ep_in = None;
            let mut ep_out = None;
            for ep in alt.endpoints() {
                if ep.transfer_type() != TransferType::Bulk {
                    continue;
                }
                match ep.direction() {
                    Direction::In => ep_in = Some(ep.address()),
                    Direction::Out => ep_out = Some(ep.address()),
                }
            }
            if let (Some(i), Some(o)) = (ep_in, ep_out) {
                candidates.push((config.configuration_value(), alt.interface_number(), i, o));
            }
        }
    }

    if candidates.is_empty() {
        return Err(
            "This USB device has no usbmux interface. Is it an iPhone/iPad in normal mode?"
                .to_string(),
        );
    }

    let active = device
        .active_configuration()
        .ok()
        .map(|c| c.configuration_value());
    debug!("usbmux candidates {candidates:?}, active configuration {active:?}");

    let chosen = candidates
        .iter()
        .find(|c| Some(c.0) == active)
        .or_else(|| candidates.iter().max_by_key(|c| c.0))
        .copied()
        .expect("candidates is not empty");

    if Some(chosen.0) != active {
        info!(
            "Switching USB configuration from {active:?} to {}",
            chosen.0
        );
        device.set_configuration(chosen.0).wait().map_err(|e| {
            format!(
                "Could not switch the iPhone to USB configuration {}: {e}. Unplug the cable, close other apps that use the device and try again.",
                chosen.0
            )
        })?;
    }

    let interface = match device.claim_interface(chosen.1).wait() {
        Ok(i) => i,
        Err(first) => device
            .detach_and_claim_interface(chosen.1)
            .wait()
            .map_err(|e| format!("Could not claim the usbmux interface: {first}; {e}"))?,
    };

    let ep_in = interface
        .endpoint::<Bulk, In>(chosen.2)
        .map_err(|e| format!("Could not open usbmux IN endpoint: {e}"))?;
    let ep_out = interface
        .endpoint::<Bulk, Out>(chosen.3)
        .map_err(|e| format!("Could not open usbmux OUT endpoint: {e}"))?;

    let serial = device
        .device_descriptor()
        .serial_number_string_index()
        .and_then(|idx| {
            device
                .get_string_descriptor(idx, 0x0409, Duration::from_secs(1))
                .wait()
                .ok()
        });

    Ok(UsbHandles {
        _device: device,
        _interface: interface,
        ep_in,
        ep_out,
        serial,
    })
}

/// Turns the USB serial number string into the UDID usbmuxd would report.
fn serial_to_udid(serial: &str) -> String {
    let serial: String = serial
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    if serial.len() == 24 && !serial.contains('-') {
        format!("{}-{}", &serial[..8], &serial[8..])
    } else {
        serial
    }
}

async fn setup_device(
    fd: OwnedFd,
    serial: String,
    product_id: u16,
) -> Result<Arc<MuxDevice>, String> {
    let handles = tokio::task::spawn_blocking(move || open_usb(fd))
        .await
        .map_err(|e| format!("USB setup task failed: {e}"))??;

    let serial = if serial.trim().is_empty() {
        handles.serial.clone().unwrap_or_default()
    } else {
        serial
    };
    let udid = serial_to_udid(&serial);
    if udid.is_empty() {
        return Err("Could not read the iPhone's serial number over USB".to_string());
    }

    let UsbHandles {
        _device,
        _interface,
        ep_in,
        ep_out,
        ..
    } = handles;

    let (ready_tx, mut ready_rx) = watch::channel(false);
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let dev = Arc::new(MuxDevice {
        id: NEXT_DEVICE_ID.fetch_add(1, Ordering::SeqCst),
        udid,
        product_id,
        out: out_tx,
        version: AtomicU32::new(0),
        rx_seq: AtomicU16::new(0xFFFF),
        conns: Mutex::new(HashMap::new()),
        next_sport: AtomicU16::new(1),
        ready: ready_tx,
        cancel: CancellationToken::new(),
        _usb: (_device, _interface),
    });

    tokio::spawn(reader_loop(dev.clone(), ep_in));
    tokio::spawn(writer_loop(dev.clone(), ep_out, out_rx));

    // Version handshake: we offer protocol 2.0, the device answers with what it speaks.
    let mut version_payload = Vec::with_capacity(12);
    version_payload.extend_from_slice(&2u32.to_be_bytes());
    version_payload.extend_from_slice(&0u32.to_be_bytes());
    version_payload.extend_from_slice(&0u32.to_be_bytes());
    dev.send_packet(MUX_PROTO_VERSION, &version_payload, &[])
        .map_err(|e| format!("Could not send usbmux version packet: {e}"))?;

    let wait_ready = async {
        while !*ready_rx.borrow_and_update() {
            if ready_rx.changed().await.is_err() {
                return false;
            }
        }
        true
    };
    match tokio::time::timeout(Duration::from_secs(5), wait_ready).await {
        Ok(true) if !dev.is_dead() => Ok(dev),
        _ => {
            dev.kill();
            Err("The iPhone did not answer the usbmux handshake. Unlock it, replug the cable and try again.".to_string())
        }
    }
}

struct OutPacket {
    proto: u32,
    header: Vec<u8>,
    data: Vec<u8>,
}

struct MuxDevice {
    id: u32,
    udid: String,
    product_id: u16,
    /// Packets queued for the USB writer task, in wire order.
    out: mpsc::UnboundedSender<OutPacket>,
    /// Negotiated mux protocol version; 0 until the handshake completes.
    version: AtomicU32,
    rx_seq: AtomicU16,
    conns: Mutex<HashMap<u16, Arc<Conn>>>,
    next_sport: AtomicU16,
    ready: watch::Sender<bool>,
    cancel: CancellationToken,
    _usb: (nusb::Device, nusb::Interface),
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum ConnPhase {
    Connecting,
    Connected,
    Dead,
}

struct ConnState {
    phase: ConnPhase,
    tx_seq: u32,
    tx_ack: u32,
    rx_ack: u32,
    rx_win: u32,
}

enum ConnEvent {
    Data(Vec<u8>),
    Reset,
}

struct Conn {
    sport: u16,
    dport: u16,
    state: Mutex<ConnState>,
    to_client: mpsc::UnboundedSender<ConnEvent>,
    window: Notify,
    connected: Mutex<Option<oneshot::Sender<bool>>>,
}

impl Conn {
    fn mark_dead(&self) {
        self.state.lock().unwrap().phase = ConnPhase::Dead;
        if let Some(tx) = self.connected.lock().unwrap().take() {
            let _ = tx.send(false);
        }
        let _ = self.to_client.send(ConnEvent::Reset);
        self.window.notify_waiters();
    }
}

impl MuxDevice {
    fn is_ready(&self) -> bool {
        *self.ready.borrow()
    }

    fn is_dead(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Marks the device as gone and tears down every open connection.
    fn kill(&self) {
        self.cancel.cancel();
        let conns: Vec<Arc<Conn>> = self.conns.lock().unwrap().drain().map(|(_, c)| c).collect();
        for conn in conns {
            conn.mark_dead();
        }
    }

    fn attached_dict(&self) -> plist::Dictionary {
        let mut props = plist::Dictionary::new();
        props.insert("ConnectionSpeed".into(), 480000000u64.into());
        props.insert("ConnectionType".into(), "USB".into());
        props.insert("DeviceID".into(), (self.id as u64).into());
        props.insert("LocationID".into(), 0u64.into());
        props.insert("ProductID".into(), (self.product_id as u64).into());
        props.insert("SerialNumber".into(), self.udid.clone().into());

        let mut dict = plist::Dictionary::new();
        dict.insert("MessageType".into(), "Attached".into());
        dict.insert("DeviceID".into(), (self.id as u64).into());
        dict.insert("Properties".into(), plist::Value::Dictionary(props));
        dict
    }

    /// Queues one mux packet. `header` is the protocol header (version or TCP), `data` the payload.
    fn send_packet(&self, proto: u32, header: &[u8], data: &[u8]) -> io::Result<()> {
        if self.is_dead() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "device disconnected",
            ));
        }
        self.out
            .send(OutPacket {
                proto,
                header: header.to_vec(),
                data: data.to_vec(),
            })
            .map_err(|_| io::Error::new(io::ErrorKind::NotConnected, "device disconnected"))
    }

    /// Queues a TCP segment for `conn`. `ack_add` bytes are acknowledged before the header is built.
    ///
    /// The segment is queued while the connection state is locked, so sequence numbers
    /// always match the order on the wire.
    fn send_tcp(&self, conn: &Conn, flags: u8, data: &[u8], ack_add: u32) -> io::Result<()> {
        let mut st = conn.state.lock().unwrap();
        st.tx_ack = st.tx_ack.wrapping_add(ack_add);
        let mut th = [0u8; 20];
        th[0..2].copy_from_slice(&conn.sport.to_be_bytes());
        th[2..4].copy_from_slice(&conn.dport.to_be_bytes());
        th[4..8].copy_from_slice(&st.tx_seq.to_be_bytes());
        th[8..12].copy_from_slice(&st.tx_ack.to_be_bytes());
        th[12] = 5 << 4;
        th[13] = flags;
        th[14..16].copy_from_slice(&((TX_WINDOW >> 8) as u16).to_be_bytes());
        self.send_packet(MUX_PROTO_TCP, &th, data)?;
        st.tx_seq = st.tx_seq.wrapping_add(data.len() as u32);
        Ok(())
    }

    fn open_conn(
        &self,
        dport: u16,
    ) -> (
        Arc<Conn>,
        mpsc::UnboundedReceiver<ConnEvent>,
        oneshot::Receiver<bool>,
    ) {
        let (to_client, from_device) = mpsc::unbounded_channel();
        let (connected_tx, connected_rx) = oneshot::channel();
        let mut conns = self.conns.lock().unwrap();
        let sport = loop {
            let candidate = self.next_sport.fetch_add(1, Ordering::SeqCst);
            if candidate != 0 && !conns.contains_key(&candidate) {
                break candidate;
            }
        };
        let conn = Arc::new(Conn {
            sport,
            dport,
            state: Mutex::new(ConnState {
                phase: ConnPhase::Connecting,
                tx_seq: 0,
                tx_ack: 0,
                rx_ack: 0,
                rx_win: 0,
            }),
            to_client,
            window: Notify::new(),
            connected: Mutex::new(Some(connected_tx)),
        });
        conns.insert(sport, conn.clone());
        (conn, from_device, connected_rx)
    }

    fn remove_conn(&self, sport: u16) {
        self.conns.lock().unwrap().remove(&sport);
    }

    fn handle_packet(&self, packet: &[u8]) {
        let proto = u32::from_be_bytes(packet[0..4].try_into().unwrap());
        let version = self.version.load(Ordering::SeqCst);
        let mux_header_len = if version >= 2 { 16 } else { 8 };
        if packet.len() < mux_header_len {
            warn!("usbmux packet shorter than its header");
            return;
        }
        if version >= 2 {
            self.rx_seq.store(
                u16::from_be_bytes(packet[14..16].try_into().unwrap()),
                Ordering::SeqCst,
            );
        }
        let body = &packet[mux_header_len..];

        match proto {
            MUX_PROTO_VERSION => {
                if body.len() < 12 {
                    warn!("usbmux version packet too short");
                    return;
                }
                let major = u32::from_be_bytes(body[0..4].try_into().unwrap());
                let minor = u32::from_be_bytes(body[4..8].try_into().unwrap());
                if major != 1 && major != 2 {
                    error!("Unsupported usbmux protocol version {major}.{minor}");
                    self.kill();
                    return;
                }
                info!("usbmux protocol version {major}.{minor}");
                self.version.store(major, Ordering::SeqCst);
                if major >= 2
                    && let Err(e) = self.send_packet(MUX_PROTO_SETUP, &[], &[0x07])
                {
                    error!("Could not send usbmux setup packet: {e}");
                    return;
                }
                let _ = self.ready.send(true);
            }
            MUX_PROTO_CONTROL => {
                if let Some((kind, text)) = body.split_first() {
                    let text = String::from_utf8_lossy(text);
                    match kind {
                        3 => warn!("usbmux device error: {text}"),
                        _ => debug!("usbmux device message ({kind}): {text}"),
                    }
                }
            }
            MUX_PROTO_TCP => {
                if body.len() < 20 {
                    warn!("usbmux TCP packet too short");
                    return;
                }
                self.handle_tcp(&body[..20], &body[20..]);
            }
            other => debug!("Ignoring usbmux packet with protocol {other}"),
        }
    }

    fn handle_tcp(&self, th: &[u8], payload: &[u8]) {
        // The device's destination port is our source port.
        let sport = u16::from_be_bytes(th[2..4].try_into().unwrap());
        let ack = u32::from_be_bytes(th[8..12].try_into().unwrap());
        let flags = th[13];
        let window = (u16::from_be_bytes(th[14..16].try_into().unwrap()) as u32) << 8;

        let Some(conn) = self.conns.lock().unwrap().get(&sport).cloned() else {
            debug!("usbmux TCP packet for unknown port {sport}");
            return;
        };

        let phase = {
            let mut st = conn.state.lock().unwrap();
            st.rx_ack = ack;
            st.rx_win = window;
            st.phase
        };
        conn.window.notify_waiters();

        match phase {
            ConnPhase::Connecting => {
                if flags == (TH_SYN | TH_ACK) {
                    {
                        let mut st = conn.state.lock().unwrap();
                        st.tx_seq = st.tx_seq.wrapping_add(1);
                        st.tx_ack = st.tx_ack.wrapping_add(1);
                        st.phase = ConnPhase::Connected;
                    }
                    if self.send_tcp(&conn, TH_ACK, &[], 0).is_err() {
                        conn.mark_dead();
                        self.remove_conn(sport);
                        return;
                    }
                    if let Some(tx) = conn.connected.lock().unwrap().take() {
                        let _ = tx.send(true);
                    }
                } else {
                    conn.mark_dead();
                    self.remove_conn(sport);
                }
            }
            ConnPhase::Connected => {
                if flags & TH_RST != 0 {
                    conn.mark_dead();
                    self.remove_conn(sport);
                } else if !payload.is_empty() {
                    let _ = conn.to_client.send(ConnEvent::Data(payload.to_vec()));
                }
            }
            ConnPhase::Dead => {}
        }
    }
}

async fn reader_loop(dev: Arc<MuxDevice>, mut ep: nusb::Endpoint<Bulk, In>) {
    let max_packet = ep.max_packet_size().max(1);
    let request_len = USB_MRU.div_ceil(max_packet) * max_packet;
    for _ in 0..3 {
        ep.submit(Buffer::new(request_len));
    }

    let mut pending: Vec<u8> = Vec::new();
    loop {
        let completion = tokio::select! {
            c = ep.next_complete() => c,
            _ = dev.cancel.cancelled() => break,
        };
        if let Err(e) = completion.status {
            warn!("usbmux read failed: {e}");
            break;
        }
        pending.extend_from_slice(&completion.buffer[..]);
        ep.submit(completion.buffer);

        loop {
            if pending.len() < 8 {
                break;
            }
            let length = u32::from_be_bytes(pending[4..8].try_into().unwrap()) as usize;
            if !(8..=MAX_PACKET).contains(&length) {
                error!("usbmux stream out of sync (packet length {length})");
                dev.kill();
                break;
            }
            if pending.len() < length {
                break;
            }
            let packet: Vec<u8> = pending.drain(..length).collect();
            dev.handle_packet(&packet);
        }
        if dev.is_dead() {
            break;
        }
    }

    ep.cancel_all();
    let was_current = {
        let mut current = DEVICE.lock().unwrap();
        match current.as_ref() {
            Some(cur) if Arc::ptr_eq(cur, &dev) => {
                *current = None;
                true
            }
            _ => false,
        }
    };
    dev.kill();
    if was_current {
        info!("usbmux device {} disconnected", dev.udid);
        set_state(
            STATE_FAILED,
            Some("The USB connection to the iPhone was lost".to_string()),
        );
    }
}

/// Owns the bulk OUT endpoint and writes queued packets one at a time, so a packet (and
/// the empty transfer that may have to follow it) is never interleaved or cut short.
async fn writer_loop(
    dev: Arc<MuxDevice>,
    mut ep: nusb::Endpoint<Bulk, Out>,
    mut queue: mpsc::UnboundedReceiver<OutPacket>,
) {
    let max_packet = ep.max_packet_size().max(1);
    let mut tx_seq: u16 = 0;

    loop {
        let packet = tokio::select! {
            p = queue.recv() => match p {
                Some(p) => p,
                None => break,
            },
            _ = dev.cancel.cancelled() => break,
        };

        let version = dev.version.load(Ordering::SeqCst);
        let mux_header_len = if version >= 2 { 16 } else { 8 };
        let total = mux_header_len + packet.header.len() + packet.data.len();

        let mut buf = Vec::with_capacity(total);
        buf.extend_from_slice(&packet.proto.to_be_bytes());
        buf.extend_from_slice(&(total as u32).to_be_bytes());
        if version >= 2 {
            buf.extend_from_slice(&0xfeedfaceu32.to_be_bytes());
            if packet.proto == MUX_PROTO_SETUP {
                tx_seq = 0;
                dev.rx_seq.store(0xFFFF, Ordering::SeqCst);
            }
            buf.extend_from_slice(&tx_seq.to_be_bytes());
            buf.extend_from_slice(&dev.rx_seq.load(Ordering::SeqCst).to_be_bytes());
            tx_seq = tx_seq.wrapping_add(1);
        }
        buf.extend_from_slice(&packet.header);
        buf.extend_from_slice(&packet.data);

        let mut transfers = vec![buf];
        if total % max_packet == 0 {
            // A transfer that ends on a full packet must be terminated by an empty one.
            transfers.push(Vec::with_capacity(1));
        }

        let mut failed = false;
        for transfer in transfers {
            ep.submit(Buffer::from(transfer));
            match tokio::time::timeout(Duration::from_secs(15), ep.next_complete()).await {
                Ok(completion) => {
                    if let Err(e) = completion.status {
                        warn!("usbmux write failed: {e}");
                        failed = true;
                    }
                }
                Err(_) => {
                    warn!("usbmux write timed out");
                    failed = true;
                }
            }
            if failed {
                break;
            }
        }
        if failed {
            break;
        }
    }

    ep.cancel_all();
    dev.kill();
}

async fn read_request<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> io::Result<(u32, plist::Dictionary)> {
    let mut header = [0u8; 16];
    stream.read_exact(&mut header).await?;
    let size = u32::from_le_bytes(header[0..4].try_into().unwrap()) as usize;
    let tag = u32::from_le_bytes(header[12..16].try_into().unwrap());
    if !(16..=MAX_PACKET * 4).contains(&size) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad usbmuxd request size",
        ));
    }
    let mut body = vec![0u8; size - 16];
    stream.read_exact(&mut body).await?;
    let dict: plist::Dictionary = plist::from_bytes(&body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    Ok((tag, dict))
}

async fn write_response<S: AsyncWrite + Unpin>(
    stream: &mut S,
    tag: u32,
    dict: plist::Dictionary,
) -> io::Result<()> {
    let mut body = Vec::new();
    plist::to_writer_xml(&mut body, &plist::Value::Dictionary(dict))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let mut packet = Vec::with_capacity(16 + body.len());
    packet.extend_from_slice(&((body.len() + 16) as u32).to_le_bytes());
    packet.extend_from_slice(&1u32.to_le_bytes());
    packet.extend_from_slice(&8u32.to_le_bytes());
    packet.extend_from_slice(&tag.to_le_bytes());
    packet.extend_from_slice(&body);
    stream.write_all(&packet).await?;
    stream.flush().await
}

fn result_dict(number: u64) -> plist::Dictionary {
    let mut dict = plist::Dictionary::new();
    dict.insert("MessageType".into(), "Result".into());
    dict.insert("Number".into(), number.into());
    dict
}

fn pair_record_path(udid: &str) -> Option<PathBuf> {
    let safe: String = udid
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    if safe.is_empty() {
        return None;
    }
    Some(
        DATA_DIR
            .get()?
            .join("lockdown")
            .join(format!("{safe}.plist")),
    )
}

fn system_buid() -> String {
    let Some(dir) = DATA_DIR.get() else {
        return uuid::Uuid::new_v4().to_string().to_uppercase();
    };
    let path = dir.join("lockdown").join("SystemBUID");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return existing.to_string();
        }
    }
    let buid = uuid::Uuid::new_v4().to_string().to_uppercase();
    let _ = std::fs::write(&path, &buid);
    buid
}

async fn handle_client<S>(mut stream: S)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    loop {
        let (tag, request) = match read_request(&mut stream).await {
            Ok(r) => r,
            Err(_) => return,
        };
        let message_type = request
            .get("MessageType")
            .and_then(|v| v.as_string())
            .unwrap_or_default()
            .to_string();

        let response = match message_type.as_str() {
            "ListDevices" => {
                let devices: Vec<plist::Value> = current_device()
                    .map(|d| vec![plist::Value::Dictionary(d.attached_dict())])
                    .unwrap_or_default();
                let mut dict = plist::Dictionary::new();
                dict.insert("DeviceList".into(), plist::Value::Array(devices));
                dict
            }
            "Listen" => {
                if write_response(&mut stream, tag, result_dict(RESULT_OK))
                    .await
                    .is_err()
                {
                    return;
                }
                match current_device() {
                    Some(dev) => dev.attached_dict(),
                    None => continue,
                }
            }
            "ReadBUID" => {
                let mut dict = plist::Dictionary::new();
                dict.insert("BUID".into(), system_buid().into());
                dict
            }
            "ReadPairRecord" => {
                let udid = request
                    .get("PairRecordID")
                    .and_then(|v| v.as_string())
                    .unwrap_or_default();
                match pair_record_path(udid).and_then(|p| std::fs::read(p).ok()) {
                    Some(data) => {
                        let mut dict = plist::Dictionary::new();
                        dict.insert("PairRecordData".into(), plist::Value::Data(data));
                        dict
                    }
                    None => result_dict(RESULT_BADDEV),
                }
            }
            "SavePairRecord" => {
                let udid = request
                    .get("PairRecordID")
                    .and_then(|v| v.as_string())
                    .unwrap_or_default();
                let data = request.get("PairRecordData").and_then(|v| v.as_data());
                match (pair_record_path(udid), data) {
                    (Some(path), Some(data)) => match std::fs::write(&path, data) {
                        Ok(()) => result_dict(RESULT_OK),
                        Err(e) => {
                            error!("Could not save pair record: {e}");
                            result_dict(RESULT_BADCOMMAND)
                        }
                    },
                    _ => result_dict(RESULT_BADCOMMAND),
                }
            }
            "DeletePairRecord" => {
                let udid = request
                    .get("PairRecordID")
                    .and_then(|v| v.as_string())
                    .unwrap_or_default();
                if let Some(path) = pair_record_path(udid) {
                    let _ = std::fs::remove_file(path);
                }
                result_dict(RESULT_OK)
            }
            "Connect" => {
                let device_id = request
                    .get("DeviceID")
                    .and_then(|v| v.as_unsigned_integer());
                let port = request
                    .get("PortNumber")
                    .and_then(|v| {
                        v.as_unsigned_integer()
                            .or_else(|| v.as_signed_integer().map(|s| s as u64))
                    })
                    // The client sends the port in network byte order.
                    .map(|raw| u16::from_be(raw as u16));
                let device = current_device().filter(|d| Some(d.id as u64) == device_id);
                match (device, port) {
                    (Some(dev), Some(port)) => {
                        tunnel(dev, stream, tag, port).await;
                        return;
                    }
                    (None, _) => result_dict(RESULT_BADDEV),
                    (_, None) => result_dict(RESULT_BADCOMMAND),
                }
            }
            other => {
                debug!("Unsupported usbmuxd request {other:?}");
                result_dict(RESULT_BADCOMMAND)
            }
        };

        if write_response(&mut stream, tag, response).await.is_err() {
            return;
        }
    }
}

/// Opens a mux connection to `port` on the device and relays bytes until either side closes.
async fn tunnel<S>(dev: Arc<MuxDevice>, mut stream: S, tag: u32, port: u16)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (conn, mut from_device, connected) = dev.open_conn(port);

    let established = match dev.send_tcp(&conn, TH_SYN, &[], 0) {
        Ok(()) => matches!(
            tokio::time::timeout(Duration::from_secs(10), connected).await,
            Ok(Ok(true))
        ),
        Err(e) => {
            warn!("usbmux connect to port {port} failed: {e}");
            false
        }
    };

    if !established {
        dev.remove_conn(conn.sport);
        conn.mark_dead();
        let _ = write_response(&mut stream, tag, result_dict(RESULT_CONNREFUSED)).await;
        return;
    }
    if write_response(&mut stream, tag, result_dict(RESULT_OK))
        .await
        .is_err()
    {
        let _ = dev.send_tcp(&conn, TH_RST, &[], 0);
        dev.remove_conn(conn.sport);
        return;
    }

    let (mut reader, mut writer) = tokio::io::split(stream);

    let client_to_device = async {
        let mut buf = vec![0u8; MAX_SEGMENT];
        loop {
            let n = match reader.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            let mut offset = 0;
            while offset < n {
                // Respect the device's receive window before sending more.
                let chunk = loop {
                    let notified = conn.window.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    {
                        let st = conn.state.lock().unwrap();
                        if st.phase != ConnPhase::Connected {
                            return;
                        }
                        let in_flight = st.tx_seq.wrapping_sub(st.rx_ack);
                        let sendable = st.rx_win.saturating_sub(in_flight) as usize;
                        if sendable > 0 {
                            break sendable.min(n - offset);
                        }
                    }
                    notified.await;
                };
                if dev
                    .send_tcp(&conn, TH_ACK, &buf[offset..offset + chunk], 0)
                    .is_err()
                {
                    return;
                }
                offset += chunk;
            }
        }
    };

    let device_to_client = async {
        while let Some(event) = from_device.recv().await {
            match event {
                ConnEvent::Data(data) => {
                    if writer.write_all(&data).await.is_err() {
                        return;
                    }
                    // Acknowledge only what the client has actually taken, so a slow
                    // client slows the device down instead of filling our memory.
                    if dev.send_tcp(&conn, TH_ACK, &[], data.len() as u32).is_err() {
                        return;
                    }
                }
                ConnEvent::Reset => return,
            }
        }
    };

    tokio::select! {
        _ = client_to_device => {}
        _ = device_to_client => {}
    }

    let still_open = {
        let mut st = conn.state.lock().unwrap();
        let open = st.phase == ConnPhase::Connected;
        st.phase = ConnPhase::Dead;
        open
    };
    dev.remove_conn(conn.sport);
    if still_open && !dev.is_dead() {
        let _ = dev.send_tcp(&conn, TH_RST, &[], 0);
    }
    let _ = writer.shutdown().await;
}
