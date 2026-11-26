use async_speed_limit::Limiter;
use async_trait::async_trait;
use hbbs::{health::Health, transport_blocklist::{self, Service}};
use hbb_common::{
    allow_err, bail,
    bytes::Bytes,
    bytes_codec::BytesCodec,
    futures_util::{sink::SinkExt, stream::StreamExt, Sink, Stream},
    log,
    protobuf::Message as _,
    rendezvous_proto::*,
    sleep,
    timeout,
    tokio::{
        self,
        io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::{Mutex, RwLock},
        time::{interval, Duration},
    },
    tokio_util::codec::Framed,
    ResultType,
};
use sodiumoxide::crypto::sign;
use std::{
    collections::{HashMap, HashSet},
    io::prelude::*,
    io::Error,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    sync::{Arc, Mutex as StdMutex, PoisonError},
    time::Instant,
};

type Usage = (u64, u64, u64, u64);
type WaitingPeer = (Arc<()>, Box<dyn StreamTrait>, [IpAddr; 2]);

struct ActiveUsage {
    generation: Arc<()>,
    sample: Usage,
}

lazy_static::lazy_static! {
    static ref PEERS: Mutex<HashMap<String, WaitingPeer>> = Default::default();
    static ref USAGE: StdMutex<HashMap<String, ActiveUsage>> = Default::default();
    static ref BLACKLIST: RwLock<HashSet<String>> = Default::default();
    static ref BLOCKLIST: RwLock<HashSet<String>> = Default::default();
}

static DOWNGRADE_THRESHOLD_100: AtomicUsize = AtomicUsize::new(66); // 0.66
static DOWNGRADE_START_CHECK: AtomicUsize = AtomicUsize::new(1_800_000); // in ms
static LIMIT_SPEED: AtomicUsize = AtomicUsize::new(32 * 1024 * 1024); // in bit/s
static TOTAL_BANDWIDTH: AtomicUsize = AtomicUsize::new(1024 * 1024 * 1024); // in bit/s
static SINGLE_BANDWIDTH: AtomicUsize = AtomicUsize::new(128 * 1024 * 1024); // in bit/s
// Only the relay request that opens a pair is a framed message of ours; pairing lifts this
// for a leg that stays framed.
const MAX_RELAY_REQUEST_LEN: usize = 128 << 10;
// Fixed at the handshake, so it is sized for the session a websocket leg carries once paired.
const WS_BUFFER_SIZE: usize = 64 << 10;
// tungstenite's defaults, so lifting the bound restores the transport rather than widening it.
const WS_DEFAULT_MAX_MESSAGE_SIZE: usize = 64 << 20;
const WS_DEFAULT_MAX_FRAME_SIZE: usize = 16 << 20;
const BLACKLIST_FILE: &str = "blacklist.txt";
const BLOCKLIST_FILE: &str = "blocklist.txt";
const RELAY_WRITE_TIMEOUT_MS: u64 = 30_000;
const RELAY_IDLE_TIMEOUT_SECS: u64 = 30;

#[tokio::main(flavor = "multi_thread")]
pub async fn start_with_bind(
    bind_addr: Option<IpAddr>,
    port: &str,
    key: &str,
) -> ResultType<()> {
    let mut health =
        Health::bind("HBBR_HEALTH_PORT", &crate::common::get_arg("HBBR_HEALTH_PORT")).await?;
    let key = get_server_sk(key)?;
    if let Ok(mut file) = std::fs::File::open(BLACKLIST_FILE) {
        let mut contents = String::new();
        if file.read_to_string(&mut contents).is_ok() {
            for x in contents.split('\n') {
                if let Some(ip) = x.trim().split(' ').next() {
                    BLACKLIST.write().await.insert(ip.to_owned());
                }
            }
        }
    }
    log::info!(
        "#blacklist({}): {}",
        BLACKLIST_FILE,
        BLACKLIST.read().await.len()
    );
    if let Ok(mut file) = std::fs::File::open(BLOCKLIST_FILE) {
        let mut contents = String::new();
        if file.read_to_string(&mut contents).is_ok() {
            for x in contents.split('\n') {
                if let Some(ip) = x.trim().split(' ').next() {
                    BLOCKLIST.write().await.insert(ip.to_owned());
                }
            }
        }
    }
    log::info!(
        "#blocklist({}): {}",
        BLOCKLIST_FILE,
        BLOCKLIST.read().await.len()
    );
    let port: u16 = port.parse()?;
    log::info!("Listening on tcp :{}", port);
    let port2 = port + 2;
    log::info!("Listening on websocket :{}", port2);
    transport_blocklist::spawn_refresh(Service::Hbbr);
    let main_task = async move {
        loop {
            io_loop(
                crate::common::listen_tcp(bind_addr, port).await?,
                crate::common::listen_tcp(bind_addr, port2).await?,
                crate::common::listen_console(bind_addr, port).await?,
                &key,
                &mut health,
            )
            .await;
        }
    };
    let listen_signal = crate::common::listen_signal();
    tokio::select!(
        res = main_task => res,
        res = listen_signal => res,
    )
}

fn check_params() {
    let tmp = crate::common::get_arg("DOWNGRADE_THRESHOLD")
        .parse::<f64>()
        .unwrap_or(0.);
    if tmp > 0. {
        DOWNGRADE_THRESHOLD_100.store((tmp * 100.) as _, Ordering::SeqCst);
    }
    log::info!(
        "DOWNGRADE_THRESHOLD: {}",
        DOWNGRADE_THRESHOLD_100.load(Ordering::SeqCst) as f64 / 100.
    );
    let tmp = crate::common::get_arg("DOWNGRADE_START_CHECK")
        .parse::<usize>()
        .unwrap_or(0);
    if tmp > 0 {
        DOWNGRADE_START_CHECK.store(tmp * 1000, Ordering::SeqCst);
    }
    log::info!(
        "DOWNGRADE_START_CHECK: {}s",
        DOWNGRADE_START_CHECK.load(Ordering::SeqCst) / 1000
    );
    let tmp = crate::common::get_arg("LIMIT_SPEED")
        .parse::<f64>()
        .unwrap_or(0.);
    if tmp > 0. {
        LIMIT_SPEED.store((tmp * 1024. * 1024.) as usize, Ordering::SeqCst);
    }
    log::info!(
        "LIMIT_SPEED: {}Mb/s",
        LIMIT_SPEED.load(Ordering::SeqCst) as f64 / 1024. / 1024.
    );
    let tmp = crate::common::get_arg("TOTAL_BANDWIDTH")
        .parse::<f64>()
        .unwrap_or(0.);
    if tmp > 0. {
        TOTAL_BANDWIDTH.store((tmp * 1024. * 1024.) as usize, Ordering::SeqCst);
    }

    log::info!(
        "TOTAL_BANDWIDTH: {}Mb/s",
        TOTAL_BANDWIDTH.load(Ordering::SeqCst) as f64 / 1024. / 1024.
    );
    let tmp = crate::common::get_arg("SINGLE_BANDWIDTH")
        .parse::<f64>()
        .unwrap_or(0.);
    if tmp > 0. {
        SINGLE_BANDWIDTH.store((tmp * 1024. * 1024.) as usize, Ordering::SeqCst);
    }
    log::info!(
        "SINGLE_BANDWIDTH: {}Mb/s",
        SINGLE_BANDWIDTH.load(Ordering::SeqCst) as f64 / 1024. / 1024.
    )
}

async fn check_cmd(cmd: &str, limiter: Limiter) -> String {
    use std::fmt::Write;

    let mut res = "".to_owned();
    let mut fds = cmd.trim().split(' ');
    match fds.next() {
        Some("h") => {
            res = format!(
                "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
                "blacklist-add(ba) <ip>",
                "blacklist-remove(br) <ip>",
                "blacklist(b) <ip>",
                "blocklist-add(Ba) <ip>",
                "blocklist-remove(Br) <ip>",
                "blocklist(B) <ip>",
                "downgrade-threshold(dt) [value]",
                "downgrade-start-check(t) [value(second)]",
                "limit-speed(ls) [value(Mb/s)]",
                "total-bandwidth(tb) [value(Mb/s)]",
                "single-bandwidth(sb) [value(Mb/s)]",
                "usage(u)"
            )
        }
        Some("blacklist-add" | "ba") => {
            if let Some(ip) = fds.next() {
                for ip in ip.split('|') {
                    BLACKLIST.write().await.insert(ip.to_owned());
                }
            }
        }
        Some("blacklist-remove" | "br") => {
            if let Some(ip) = fds.next() {
                if ip == "all" {
                    BLACKLIST.write().await.clear();
                } else {
                    for ip in ip.split('|') {
                        BLACKLIST.write().await.remove(ip);
                    }
                }
            }
        }
        Some("blacklist" | "b") => {
            if let Some(ip) = fds.next() {
                res = format!("{}\n", BLACKLIST.read().await.get(ip).is_some());
            } else {
                for ip in BLACKLIST.read().await.clone().into_iter() {
                    let _ = writeln!(res, "{ip}");
                }
            }
        }
        Some("blocklist-add" | "Ba") => {
            if let Some(ip) = fds.next() {
                for ip in ip.split('|') {
                    BLOCKLIST.write().await.insert(ip.to_owned());
                }
            }
        }
        Some("blocklist-remove" | "Br") => {
            if let Some(ip) = fds.next() {
                if ip == "all" {
                    BLOCKLIST.write().await.clear();
                } else {
                    for ip in ip.split('|') {
                        BLOCKLIST.write().await.remove(ip);
                    }
                }
            }
        }
        Some("blocklist" | "B") => {
            if let Some(ip) = fds.next() {
                res = format!("{}\n", BLOCKLIST.read().await.get(ip).is_some());
            } else {
                for ip in BLOCKLIST.read().await.clone().into_iter() {
                    let _ = writeln!(res, "{ip}");
                }
            }
        }
        Some("downgrade-threshold" | "dt") => {
            if let Some(v) = fds.next() {
                if let Ok(v) = v.parse::<f64>() {
                    if v > 0. {
                        DOWNGRADE_THRESHOLD_100.store((v * 100.) as _, Ordering::SeqCst);
                    }
                }
            } else {
                res = format!(
                    "{}\n",
                    DOWNGRADE_THRESHOLD_100.load(Ordering::SeqCst) as f64 / 100.
                );
            }
        }
        Some("downgrade-start-check" | "t") => {
            if let Some(v) = fds.next() {
                if let Ok(v) = v.parse::<usize>() {
                    if v > 0 {
                        DOWNGRADE_START_CHECK.store(v * 1000, Ordering::SeqCst);
                    }
                }
            } else {
                res = format!("{}s\n", DOWNGRADE_START_CHECK.load(Ordering::SeqCst) / 1000);
            }
        }
        Some("limit-speed" | "ls") => {
            if let Some(v) = fds.next() {
                if let Ok(v) = v.parse::<f64>() {
                    if v > 0. {
                        LIMIT_SPEED.store((v * 1024. * 1024.) as _, Ordering::SeqCst);
                    }
                }
            } else {
                res = format!(
                    "{}Mb/s\n",
                    LIMIT_SPEED.load(Ordering::SeqCst) as f64 / 1024. / 1024.
                );
            }
        }
        Some("total-bandwidth" | "tb") => {
            if let Some(v) = fds.next() {
                if let Ok(v) = v.parse::<f64>() {
                    if v > 0. {
                        TOTAL_BANDWIDTH.store((v * 1024. * 1024.) as _, Ordering::SeqCst);
                        limiter.set_speed_limit(TOTAL_BANDWIDTH.load(Ordering::SeqCst) as _);
                    }
                }
            } else {
                res = format!(
                    "{}Mb/s\n",
                    TOTAL_BANDWIDTH.load(Ordering::SeqCst) as f64 / 1024. / 1024.
                );
            }
        }
        Some("single-bandwidth" | "sb") => {
            if let Some(v) = fds.next() {
                if let Ok(v) = v.parse::<f64>() {
                    if v > 0. {
                        SINGLE_BANDWIDTH.store((v * 1024. * 1024.) as _, Ordering::SeqCst);
                    }
                }
            } else {
                res = format!(
                    "{}Mb/s\n",
                    SINGLE_BANDWIDTH.load(Ordering::SeqCst) as f64 / 1024. / 1024.
                );
            }
        }
        Some("usage" | "u") => {
            let mut tmp: Vec<(String, Usage)> = USAGE
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .map(|(id, usage)| (id.clone(), usage.sample))
                .collect();
            tmp.sort_by(|a, b| ((b.1).1).partial_cmp(&(a.1).1).unwrap());
            for (ip, (elapsed, total, highest, speed)) in tmp {
                if elapsed == 0 {
                    continue;
                }
                let _ = writeln!(
                    res,
                    "{}: {}s {:.2}MB {}kb/s {}kb/s {}kb/s",
                    ip,
                    elapsed / 1000,
                    total as f64 / 1024. / 1024. / 8.,
                    highest,
                    total / elapsed,
                    speed
                );
            }
        }
        _ => {}
    }
    res
}

async fn io_loop(
    listener: TcpListener,
    listener2: TcpListener,
    listener_console: Option<TcpListener>,
    key: &str,
    health: &mut Health,
) {
    check_params();
    let limiter = <Limiter>::new(TOTAL_BANDWIDTH.load(Ordering::SeqCst) as _);
    log::info!(target: "service.ready", "Start");
    loop {
        tokio::select! {
            probe = health.next_probe() => { let _ = probe.send(()); }
            res = listener.accept() => {
                match res {
                    Ok((stream, addr))  => {
                        stream.set_nodelay(true).ok();
                        handle_connection(stream, addr, &limiter, key, false).await;
                    }
                    Err(err) => {
                       log::error!("listener.accept failed: {}", err);
                       break;
                    }
                }
            }
            res = listener2.accept() => {
                match res {
                    Ok((stream, addr))  => {
                        stream.set_nodelay(true).ok();
                        handle_connection(stream, addr, &limiter, key, true).await;
                    }
                    Err(err) => {
                       log::error!("listener2.accept failed: {}", err);
                       break;
                    }
                }
            }
            res = crate::common::accept_or_pending(listener_console.as_ref()) => {
                match res {
                    Ok((stream, addr))  => {
                        stream.set_nodelay(true).ok();
                        handle_connection(stream, addr, &limiter, key, false).await;
                    }
                    Err(err) => {
                       log::error!("console listener.accept failed: {}", err);
                       break;
                    }
                }
            }
        }
    }
}

/// Accepts a relay connection and routes loopback console requests separately.
async fn handle_connection(
    stream: TcpStream,
    addr: SocketAddr,
    limiter: &Limiter,
    key: &str,
    ws: bool,
) {
    let ip = hbb_common::try_into_v4(addr).ip();
    if !ws && ip.is_loopback() {
        let limiter = limiter.clone();
        tokio::spawn(async move {
            let mut stream = stream;
            let mut buffer = [0; 1024];
            if let Ok(Ok(n)) = timeout(1000, stream.read(&mut buffer[..])).await {
                if let Ok(data) = std::str::from_utf8(&buffer[..n]) {
                    let res = check_cmd(data, limiter).await;
                    if let Err(err) = timeout(1000, stream.write_all(res.as_bytes()))
                        .await
                        .unwrap_or_else(|elapsed| Err(elapsed.into()))
                    {
                        log::warn!("Failed to send the console reply: {err}");
                    }
                }
            }
        });
        return;
    }
    let ip = ip.to_string();
    let reported = addr.ip().to_string();
    let listed = |list: &HashSet<String>| list.contains(&ip) || list.contains(&reported);
    if transport_blocklist::blocks_ip(addr.ip()) || listed(&*BLOCKLIST.read().await) {
        log::info!("{} blocked", ip);
        return;
    }
    let key = key.to_owned();
    let limiter = limiter.clone();
    tokio::spawn(async move {
        allow_err!(make_pair(stream, addr, &key, limiter, ws).await);
    });
}

/// Prepares a TCP or WebSocket relay stream and begins pairing.
async fn make_pair(
    stream: TcpStream,
    mut addr: SocketAddr,
    key: &str,
    limiter: Limiter,
    ws: bool,
) -> ResultType<()> {
    let transport_ip = addr.ip();
    if ws {
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
        let callback = |req: &Request, response: Response| {
            let headers = req.headers();
            // X-Real-IP / X-Forwarded-For are trusted as-is so that the real
            // client IP is preserved when the WebSocket port runs behind a
            // reverse proxy (WSS). They are NOT validated: anyone who can reach
            // this port directly can spoof an arbitrary IP, bypassing IP-based
            // rate limiting / blocking and corrupting logged IPs. Do not expose
            // the WebSocket port directly to untrusted networks; only the
            // reverse proxy, which overwrites these headers, should be able to
            // connect to it.
            // https://github.com/rustdesk/rustdesk-server/issues/634
            let real_ip = headers
                .get("X-Real-IP")
                .or_else(|| headers.get("X-Forwarded-For"))
                .and_then(|header_value| header_value.to_str().ok());
            if let Some(ip) = real_ip {
                if let Ok(ip) = ip.parse() {
                    addr = SocketAddr::new(ip, addr.port());
                }
            }
            Ok(response)
        };
        // The message bound is set in make_pair_, once the handshake is done.
        let ws_config = tungstenite::protocol::WebSocketConfig::default()
            .read_buffer_size(WS_BUFFER_SIZE)
            .write_buffer_size(WS_BUFFER_SIZE);
        let ws_stream = timeout(
            30_000,
            tokio_tungstenite::accept_hdr_async_with_config(stream, callback, Some(ws_config)),
        )
        .await??;
        make_pair_(Box::new(ws_stream), addr, transport_ip, key, limiter).await;
    } else {
        let stream = Framed::new(stream, BytesCodec::new());
        make_pair_(Box::new(stream), addr, transport_ip, key, limiter).await;
    }
    Ok(())
}

/// Checks a configured relay key and pairs a request with a waiting connection.
async fn make_pair_(
    mut stream: Box<dyn StreamTrait>,
    addr: SocketAddr,
    transport_ip: IpAddr,
    key: &str,
    limiter: Limiter,
) {
    let addresses = [addr.ip(), transport_ip];
    if addresses.into_iter().any(transport_blocklist::blocks_ip) {
        return;
    }
    stream.set_max_packet_length(MAX_RELAY_REQUEST_LEN);
    if let Ok(Some(Ok(bytes))) = timeout(30_000, stream.recv()).await {
        if let Ok(msg_in) = RendezvousMessage::parse_from_bytes(&bytes) {
            if let Some(rendezvous_message::Union::RequestRelay(rf)) = msg_in.union {
                if !key.is_empty() && rf.licence_key != key {
                    log::warn!("Relay authentication failed from {} - invalid key", addr);
                    return;
                }
                if !rf.uuid.is_empty() {
                    let mut peers = PEERS.lock().await;
                    if addresses.into_iter().any(transport_blocklist::blocks_ip) {
                        return;
                    }
                    if let Some((_, mut peer, peer_addresses)) = peers.remove(&rf.uuid) {
                        drop(peers);
                        let pair_addresses = [addresses[0], addresses[1], peer_addresses[0], peer_addresses[1]];
                        if pair_addresses.into_iter().any(transport_blocklist::blocks_ip) {
                            return;
                        }
                        log::info!("Relayrequest {} from {} got paired", rf.uuid, addr);
                        let id = format!("{}:{}", addr.ip(), addr.port());
                        if !stream.is_ws() && !peer.is_ws() {
                            peer.set_raw();
                            stream.set_raw();
                            log::info!("Both are raw");
                        } else {
                            // A leg that stays framed now carries session data.
                            peer.set_max_packet_length(usize::MAX);
                            stream.set_max_packet_length(usize::MAX);
                        }
                        let result = tokio::select! {
                            biased;
                            _ = transport_blocklist::wait_until_blocked(pair_addresses) => {
                                log::info!("Active relay closed by managed blocklist");
                                Ok(())
                            }
                            result = relay(addr, stream.into_relay_leg(), peer.into_relay_leg(), limiter, &id) => result,
                        };
                        if let Err(err) = result {
                            log::info!("Relay of {} closed: {}", addr, err);
                        } else {
                            log::info!("Relay of {} closed", addr);
                        }
                    } else {
                        log::info!("New relay request {} from {}", rf.uuid, addr);
                        let generation = Arc::new(());
                        peers.insert(rf.uuid.clone(), (generation.clone(), stream, addresses));
                        drop(peers);
                        sleep(30.).await;
                        expire_waiter(&mut *PEERS.lock().await, &rf.uuid, &generation);
                    }
                }
            }
        }
    }
}

/// Removes a waiting connection only if it still owns the requested pair ID.
fn expire_waiter(peers: &mut HashMap<String, WaitingPeer>, id: &str, generation: &Arc<()>) {
    if peers.get(id).map(|(current, _, _)| Arc::ptr_eq(current, generation)).unwrap_or(false) {
        peers.remove(id);
    }
}

type RelayReader = Pin<Box<dyn Stream<Item = Result<Bytes, Error>> + Send>>;
type RelayWriter = Pin<Box<dyn Sink<Bytes, Error = Error> + Send>>;

struct RelayLeg {
    reader: RelayReader,
    writer: RelayWriter,
}

struct RelayUsage<'a> {
    id: &'a str,
    generation: Arc<()>,
    started: Instant,
    idle_since_ms: AtomicU64,
    in_flight: AtomicUsize,
    total_bits: AtomicU64,
}

impl<'a> RelayUsage<'a> {
    fn new(id: &'a str) -> Self {
        let generation = Arc::new(());
        USAGE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                id.to_owned(),
                ActiveUsage {
                    generation: generation.clone(),
                    sample: Usage::default(),
                },
            );
        Self {
            id,
            generation,
            started: Instant::now(),
            idle_since_ms: AtomicU64::new(0),
            in_flight: AtomicUsize::new(0),
            total_bits: AtomicU64::new(0),
        }
    }

    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn packet_started(&self) -> InFlightPacket<'_> {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        InFlightPacket(self)
    }

    fn receive_timed_out(&self, elapsed_ms: u64) -> bool {
        let idle_ms = elapsed_ms.saturating_sub(self.idle_since_ms.load(Ordering::Relaxed));
        self.in_flight.load(Ordering::Relaxed) == 0 && idle_ms / 1_000 > RELAY_IDLE_TIMEOUT_SECS
    }

    fn sample(&self, usage: Usage) {
        if let Some(entry) = USAGE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(self.id)
            .filter(|entry| Arc::ptr_eq(&entry.generation, &self.generation))
        {
            entry.sample = usage;
        }
    }
}

impl Drop for RelayUsage<'_> {
    fn drop(&mut self) {
        let mut active = USAGE.lock().unwrap_or_else(PoisonError::into_inner);
        if active
            .get(self.id)
            .is_some_and(|entry| Arc::ptr_eq(&entry.generation, &self.generation))
        {
            active.remove(self.id);
        }
    }
}

struct InFlightPacket<'a>(&'a RelayUsage<'a>);

impl Drop for InFlightPacket<'_> {
    fn drop(&mut self) {
        if self.0.in_flight.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.0
                .idle_since_ms
                .store(self.0.elapsed_ms(), Ordering::Relaxed);
        }
    }
}

struct RelayLimiters {
    per_pair: Limiter,
    restricted: Limiter,
    total: Limiter,
    is_restricted: AtomicBool,
}

/// Forwards traffic while keeping backpressure local to each direction.
async fn relay(
    addr: SocketAddr,
    stream: RelayLeg,
    peer: RelayLeg,
    total_limiter: Limiter,
    id: &str,
) -> ResultType<()> {
    let usage = RelayUsage::new(id);
    let limiters = RelayLimiters {
        per_pair: Limiter::new(SINGLE_BANDWIDTH.load(Ordering::SeqCst) as f64),
        restricted: Limiter::new(LIMIT_SPEED.load(Ordering::SeqCst) as f64),
        total: total_limiter,
        is_restricted: AtomicBool::new(false),
    };

    // Each scoped future holds at most one incoming packet until its write finishes.
    // A close, timeout or policy cancellation drops both directions and both sockets.
    tokio::select! {
        result = forward(stream.reader, peer.writer, &usage, &limiters) => result,
        result = forward(peer.reader, stream.writer, &usage, &limiters) => result,
        result = supervise_relay(addr, &usage, &limiters) => result,
    }
}

async fn forward(
    mut reader: RelayReader,
    mut writer: RelayWriter,
    usage: &RelayUsage<'_>,
    limiters: &RelayLimiters,
) -> ResultType<()> {
    while let Some(Ok(bytes)) = reader.next().await {
        let _packet = usage.packet_started();
        let bits = bytes.len() as u64 * 8;
        let pair_limiter = if limiters.is_restricted.load(Ordering::Relaxed) {
            &limiters.restricted
        } else {
            &limiters.per_pair
        };
        consume_bits(pair_limiter, bits).await;
        consume_bits(&limiters.total, bits).await;
        usage.total_bits.fetch_add(bits, Ordering::Relaxed);
        if !bytes.is_empty() {
            timeout(RELAY_WRITE_TIMEOUT_MS, writer.send(bytes)).await??;
        }
    }
    Ok(())
}

async fn consume_bits(limiter: &Limiter, mut bits: u64) {
    // The limiter accepts usize. On 32-bit builds a framed input can exceed
    // usize::MAX bits even when its byte length fits.
    while usize::try_from(bits).is_err() {
        limiter.consume(usize::MAX).await;
        bits -= usize::MAX as u64;
    }
    limiter.consume(bits as usize).await;
}

async fn supervise_relay(
    addr: SocketAddr,
    usage: &RelayUsage<'_>,
    limiters: &RelayLimiters,
) -> ResultType<()> {
    let ip = addr.ip().to_string();
    // The dual-stack listener reports an IPv4 client as ::ffff:a.b.c.d.
    let unmapped = hbb_common::try_into_v4(addr).ip().to_string();
    let listed = |list: &HashSet<String>| list.contains(&ip) || list.contains(&unmapped);
    let threshold = (limiters.per_pair.speed_limit()
        * DOWNGRADE_THRESHOLD_100.load(Ordering::SeqCst) as f64
        / 100.
        / 1000.) as u64;
    let mut timer = interval(Duration::from_secs(1));
    let mut previous_ms = 0;
    let mut previous_bits = 0;
    let mut highest_speed = 0;
    let mut downgraded = false;

    loop {
        timer.tick().await;
        let elapsed_ms = usage.elapsed_ms();
        if usage.receive_timed_out(elapsed_ms) {
            bail!("Timeout");
        }

        let sample_ms = elapsed_ms - previous_ms;
        if sample_ms < 1_000 {
            continue;
        }
        if listed(&*BLOCKLIST.read().await) {
            log::info!("{} blocked", ip);
            return Ok(());
        }
        let blacklisted = listed(&*BLACKLIST.read().await);
        let total_bits = usage.total_bits.load(Ordering::Relaxed);
        let speed = total_bits.wrapping_sub(previous_bits) / sample_ms;
        highest_speed = highest_speed.max(speed);
        usage.sample((elapsed_ms, total_bits, highest_speed, speed));
        previous_bits = total_bits;
        previous_ms = elapsed_ms;

        if !downgraded
            && elapsed_ms > DOWNGRADE_START_CHECK.load(Ordering::SeqCst) as u64
            && total_bits > elapsed_ms.saturating_mul(threshold)
        {
            downgraded = true;
            log::info!(
                "Downgrade {}, exceed downgrade threshold {}bit/ms in {}ms",
                usage.id,
                threshold,
                elapsed_ms
            );
        }
        limiters
            .is_restricted
            .store(blacklisted || downgraded, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_leg(stream: tokio::io::DuplexStream) -> RelayLeg {
        let mut stream = Framed::new(stream, BytesCodec::new());
        stream.set_raw();
        Box::new(stream).into_relay_leg()
    }

    /// Checks that a blocked relay write ends with a timeout in the selected direction.
    async fn check_blocked_send(from_peer: bool) {
        let (mut source, relay_source) = tokio::io::duplex(16);
        let (_destination, relay_destination) = tokio::io::duplex(4);
        source.write_all(&[1; 16]).await.unwrap();
        let (stream, peer) = if from_peer {
            (raw_leg(relay_destination), raw_leg(relay_source))
        } else {
            (raw_leg(relay_source), raw_leg(relay_destination))
        };
        let id = if from_peer {
            "blocked-send-from-peer"
        } else {
            "blocked-send-from-stream"
        };
        let result = timeout(
            35_000,
            relay(
                "127.0.0.1:1".parse().unwrap(),
                stream,
                peer,
                Limiter::new(1_000_000.),
                id,
            ),
        )
        .await
        .expect("a blocked relay write must finish within its deadline");
        let error = result.expect_err("the blocked write must fail");
        assert!(error.downcast_ref::<tokio::time::error::Elapsed>().is_some());
    }

    /// Checks that stalled writes time out in either relay direction.
    #[tokio::test]
    async fn blocked_writes_in_both_directions_time_out() {
        tokio::join!(check_blocked_send(false), check_blocked_send(true));
    }

    #[tokio::test]
    async fn reverse_traffic_survives_a_blocked_write_and_abort_cleans_up() {
        let (mut source, relay_source) = tokio::io::duplex(16);
        let (mut destination, relay_destination) = tokio::io::duplex(4);
        let id = "reverse-while-write-blocked";
        let total_limiter = Limiter::new(f64::INFINITY);
        let task = tokio::spawn(relay(
            "127.0.0.1:1".parse().unwrap(),
            raw_leg(relay_source),
            raw_leg(relay_destination),
            total_limiter.clone(),
            id,
        ));

        source.write_all(&[7; 16]).await.unwrap();
        // The destination can buffer four of the sixteen bytes. Reading just one
        // proves forwarding started while leaving the write stalled.
        let mut first = [0];
        timeout(1_000, destination.read_exact(&mut first))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, [7]);

        destination.write_all(b"ok").await.unwrap();
        let mut reply = [0; 2];
        timeout(1_000, source.read_exact(&mut reply))
            .await
            .expect("the blocked destination stalled reverse traffic")
            .unwrap();
        assert_eq!(&reply, b"ok");
        assert_eq!(total_limiter.total_bytes_consumed(), (16 + 2) * 8);
        assert!(USAGE.lock().unwrap().contains_key(id));

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!USAGE.lock().unwrap().contains_key(id));
        assert_eq!(source.read(&mut reply).await.unwrap(), 0);
    }

    #[test]
    fn finishing_an_old_relay_preserves_replacement_usage() {
        let id = "reused-usage-id";
        let old = RelayUsage::new(id);
        let replacement = RelayUsage::new(id);
        replacement.sample((1, 2, 3, 4));
        old.sample((5, 6, 7, 8));
        drop(old);
        assert_eq!(USAGE.lock().unwrap().get(id).unwrap().sample, (1, 2, 3, 4));
        drop(replacement);
        assert!(!USAGE.lock().unwrap().contains_key(id));
    }

    #[test]
    fn receive_deadline_waits_for_both_packets_to_finish() {
        let usage = RelayUsage::new("receive-deadline");
        let expired_ms = (RELAY_IDLE_TIMEOUT_SECS + 1) * 1_000;
        assert!(usage.receive_timed_out(expired_ms));

        let outgoing = usage.packet_started();
        let incoming = usage.packet_started();
        assert!(!usage.receive_timed_out(expired_ms));
        drop(outgoing);
        assert!(!usage.receive_timed_out(expired_ms));
        drop(incoming);

        let idle_since_ms = usage.idle_since_ms.load(Ordering::Relaxed);
        assert!(!usage.receive_timed_out(idle_since_ms + expired_ms - 1));
        assert!(usage.receive_timed_out(idle_since_ms + expired_ms));
    }

    #[tokio::test]
    async fn only_a_blocklisted_ipv4_client_on_the_dual_stack_listener_is_cut_off() {
        // The plain form is what `ba` is usually given; the mapped one is what `usage` shows.
        // A client not listed must outlive the periodic list check.
        for (entry, client, cut_off) in [
            ("192.0.2.55", "[::ffff:192.0.2.55]:1", true),
            ("::ffff:192.0.2.55", "[::ffff:192.0.2.55]:1", true),
            ("192.0.2.55", "[::ffff:192.0.2.56]:1", false),
        ] {
            BLOCKLIST.write().await.insert(entry.to_owned());
            let (_client, stream) = tokio::io::duplex(8);
            let (_remote, peer) = tokio::io::duplex(8);
            let result = timeout(
                if cut_off { 10_000 } else { 4_000 },
                relay(
                    client.parse().unwrap(),
                    raw_leg(stream),
                    raw_leg(peer),
                    Limiter::new(1_000_000.),
                    "blocklist-test",
                ),
            )
            .await;
            BLOCKLIST.write().await.remove(entry);
            assert!(
                matches!((cut_off, &result), (true, Ok(Ok(()))) | (false, Err(_))),
                "client {client}, blocklist entry {entry}: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_client_blocklisted_in_either_form_is_refused_at_accept() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        for (entry, refused) in [
            ("192.0.2.57", true),
            ("::ffff:192.0.2.57", true),
            ("192.0.2.58", false),
        ] {
            BLOCKLIST.write().await.insert(entry.to_owned());
            let mut client = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (stream, _) = listener.accept().await.unwrap();
            let addr = "[::ffff:192.0.2.57]:1".parse().unwrap();
            handle_connection(stream, addr, &Limiter::new(1_000_000.), "", false).await;
            BLOCKLIST.write().await.remove(entry);
            let read = timeout(1_000, client.read(&mut [0; 1])).await;
            assert!(
                matches!((refused, &read), (true, Ok(Ok(0))) | (false, Err(_))),
                "blocklist entry {entry}: {read:?}"
            );
        }
    }
}

/// Returns the configured relay key or resolves a signing key to its public value.
fn get_server_sk(key: &str) -> ResultType<String> {
    let mut key = key.to_owned();
    if let Ok(sk) = base64::decode(&key) {
        if sk.len() == sign::SECRETKEYBYTES {
            log::info!("The key is a crypto private key");
            key = base64::encode(&sk[(sign::SECRETKEYBYTES / 2)..]);
        }
    }

    if key == "-" || key == "_" {
        let (pk, _) = crate::common::gen_sk(300)?;
        key = pk;
    }

    if !key.is_empty() {
        log::info!("Key: {}", key);
    }

    Ok(key)
}

#[async_trait]
trait StreamTrait: Send + Sync + 'static {
    async fn recv(&mut self) -> Option<Result<Bytes, Error>>;
    fn is_ws(&self) -> bool;
    fn set_raw(&mut self);
    fn set_max_packet_length(&mut self, n: usize);
    fn into_relay_leg(self: Box<Self>) -> RelayLeg;
}

#[async_trait]
impl<T> StreamTrait for Framed<T, BytesCodec>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
{
    async fn recv(&mut self) -> Option<Result<Bytes, Error>> {
        self.next().await.map(|frame| frame.map(|bytes| bytes.freeze()))
    }

    fn is_ws(&self) -> bool {
        false
    }

    fn set_raw(&mut self) {
        self.codec_mut().set_raw();
    }

    fn set_max_packet_length(&mut self, n: usize) {
        self.codec_mut().set_max_packet_length(n);
    }

    fn into_relay_leg(self: Box<Self>) -> RelayLeg {
        let (writer, reader) = StreamExt::split(*self);
        RelayLeg {
            reader: Box::pin(reader.map(|frame| frame.map(|bytes| bytes.freeze()))),
            writer: Box::pin(writer),
        }
    }
}

fn websocket_bytes(message: Result<tungstenite::Message, tungstenite::Error>) -> Result<Bytes, Error> {
    match message {
        Ok(tungstenite::Message::Binary(bytes)) => Ok(bytes),
        Ok(_) => Ok(Bytes::new()),
        Err(error) => Err(Error::other(error)),
    }
}

#[async_trait]
impl StreamTrait for tokio_tungstenite::WebSocketStream<TcpStream> {
    async fn recv(&mut self) -> Option<Result<Bytes, Error>> {
        self.next().await.map(websocket_bytes)
    }

    fn is_ws(&self) -> bool {
        true
    }

    fn set_raw(&mut self) {}

    fn set_max_packet_length(&mut self, n: usize) {
        self.set_config(|c| {
            c.max_message_size = Some(n.min(WS_DEFAULT_MAX_MESSAGE_SIZE));
            c.max_frame_size = Some(n.min(WS_DEFAULT_MAX_FRAME_SIZE));
        });
    }

    fn into_relay_leg(self: Box<Self>) -> RelayLeg {
        let (writer, reader) = StreamExt::split(*self);
        let writer = writer.sink_map_err(Error::other).with(|bytes| {
            std::future::ready(Ok(tungstenite::Message::Binary(bytes)))
        });
        RelayLeg {
            reader: Box::pin(reader.map(websocket_bytes)),
            writer: Box::pin(writer),
        }
    }
}

#[cfg(test)]
mod message_size_tests {
    use super::*;
    use hbb_common::{bytes::BytesMut, tcp::FramedStream, tokio_util::codec::Encoder};

    async fn serve_one(ws: bool) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, addr) = listener.accept().await.unwrap();
            allow_err!(make_pair(stream, addr, "", <Limiter>::new(f64::INFINITY), ws).await);
        });
        addr
    }

    fn relay_request(uuid: &str) -> Vec<u8> {
        let mut msg = RendezvousMessage::new();
        msg.set_request_relay(RequestRelay {
            uuid: uuid.to_owned(),
            ..Default::default()
        });
        msg.write_to_bytes().unwrap()
    }

    #[hbb_common::tokio::test]
    async fn relay_request_over_the_bound_is_refused_on_its_header() {
        let addr = serve_one(false).await;
        let mut client = TcpStream::connect(addr).await.unwrap();
        let payload_len = MAX_RELAY_REQUEST_LEN + 1;
        let mut frame = BytesMut::new();
        BytesCodec::new()
            .encode(Bytes::from(vec![0u8; payload_len]), &mut frame)
            .unwrap();
        client.write_all(&frame[..frame.len() - payload_len]).await.unwrap();
        let mut byte = [0u8; 1];
        let read = timeout(5_000, client.read(&mut byte))
            .await
            .expect("hbbr waited for the payload of an oversized relay request");
        assert_eq!(read.unwrap(), 0);
    }

    async fn ws_client(addr: SocketAddr) -> tokio_tungstenite::WebSocketStream<TcpStream> {
        let stream = TcpStream::connect(addr).await.unwrap();
        tokio_tungstenite::client_async(format!("ws://{addr}"), stream)
            .await
            .unwrap()
            .0
    }

    #[hbb_common::tokio::test]
    async fn ws_relay_request_over_the_bound_is_refused_on_its_header() {
        let addr = serve_one(true).await;
        let mut ws = ws_client(addr).await;
        let client = ws.get_mut();
        let mut header = vec![0x82, 0xff];
        header.extend_from_slice(&(MAX_RELAY_REQUEST_LEN as u64 + 1).to_be_bytes());
        header.extend_from_slice(&[1, 2, 3, 4]);
        client.write_all(&header).await.unwrap();
        let mut buf = [0u8; 256];
        let closed = timeout(5_000, async {
            while client.read(&mut buf).await.unwrap_or(0) > 0 {}
        })
        .await;
        assert!(closed.is_ok(), "hbbr waited for the payload of an oversized relay request");
    }

    #[hbb_common::tokio::test]
    async fn paired_legs_carry_frames_past_the_relay_request_bound() {
        let uuid = "paired-legs";
        let ws_addr = serve_one(true).await;
        let tcp_addr = serve_one(false).await;
        let mut ws = ws_client(ws_addr).await;
        ws.send(tungstenite::Message::Binary(relay_request(uuid).into()))
            .await
            .unwrap();
        // Park the websocket leg first, so the tcp leg is the one that pairs.
        timeout(5_000, async {
            while !PEERS.lock().await.contains_key(uuid) {
                sleep(0.01).await;
            }
        })
        .await
        .expect("websocket leg never parked");
        let mut tcp = FramedStream::from(TcpStream::connect(tcp_addr).await.unwrap(), tcp_addr);
        tcp.send_bytes(relay_request(uuid).into()).await.unwrap();
        let big = vec![7u8; MAX_RELAY_REQUEST_LEN * 2];
        tcp.send_bytes(big.clone().into()).await.unwrap();
        let got = timeout(5_000, ws.next())
            .await
            .expect("relay did not forward the tcp frame")
            .expect("relay closed the websocket leg")
            .unwrap();
        assert_eq!(got.into_data(), big);
        ws.send(tungstenite::Message::Binary(big.clone().into()))
            .await
            .unwrap();
        let got = tcp
            .next_timeout(5_000)
            .await
            .expect("relay did not forward the websocket message")
            .unwrap();
        assert_eq!(&got[..], &big[..]);

        ws.close(None).await.unwrap();
        let closed = timeout(5_000, tcp.next())
            .await
            .expect("the websocket close did not end the TCP leg");
        assert!(closed.is_none());
    }
}

#[cfg(test)]
mod waiter_tests {
    use super::*;

    /// Checks that an old timeout cannot remove a replacement waiter.
    #[test]
    fn old_waiter_cannot_expire_a_reused_pair_id() {
        let old = Arc::new(());
        let current = Arc::new(());
        let mut peers: HashMap<String, WaitingPeer> = HashMap::new();
        let local = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        let (_client, stream) = tokio::io::duplex(8);
        let stream = Framed::new(stream, BytesCodec::new());
        peers.insert("pair".to_owned(), (current.clone(), Box::new(stream), [local; 2]));
        expire_waiter(&mut peers, "pair", &old);
        assert!(peers.contains_key("pair"));
        expire_waiter(&mut peers, "pair", &current);
        assert!(!peers.contains_key("pair"));
    }
}
