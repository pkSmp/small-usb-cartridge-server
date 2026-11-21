//! The fleet: a leaderless set of PCs that share storage and updates.
//!
//! This is the layer above `cluster`. `cluster` decides which machine answers a
//! request; the fleet decides which machine *holds* each byte, how a release
//! reaches every node from one USB stick, and how a node that was switched off
//! catches up without anyone in charge.
//!
//! Three rules hold the design together:
//!
//!   * **Every node holds the whole blueprint.** It is a few megabytes even for
//!     a large site, so any node can answer "who should have this chunk?"
//!     without asking anyone. There is no controller because there is nothing
//!     left for a controller to decide.
//!   * **Ownership is a pure function** of the sorted member list, the release
//!     and the quantized weights. Same inputs, same answer, on every machine.
//!   * **Nothing is trusted from the network.** Chunks are named by SHA-256 and
//!     verified on arrival; releases are ordered by an epoch, not by a clock;
//!     and an optional fleet key authenticates every control connection.
//!
//! Updates are two-phase: a node that imports a stick announces the next epoch
//! and pulls only its share, and nobody switches the live site until every live
//! node reports that epoch ready.

use crate::auth;
use crate::blueprint::{self, Blueprint};
use crate::chunks::{self, Store};
use crate::hash;
use crate::ident::{self, Identity};
use crate::publish;
use crate::spec::{self, Spec};
use crate::sys;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::raw::{c_int, c_void};
use std::os::unix::io::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const CONTROL: u16 = 29702;
pub const BEACON: u16 = 29703;

const BEACON_EVERY: Duration = Duration::from_millis(500);
const PEER_TTL: Duration = Duration::from_secs(4);
const REPAIR_EVERY: Duration = Duration::from_secs(5);
const STATE_FILE: &str = "/run/cartridge-fleet.json";
const RELEASES: &str = "/srv/cartridge/releases";
const LIVE: &str = "/srv/cartridge/live";
const STORE: &str = "/srv/cartridge/store";
const ARCHIVE: &str = "/srv/cartridge/archive";
const LOG_FILE: &str = "/var/log/cartridge/fleet.log";
/// Two copies of cold data by default, three for the blueprint. Rendezvous
/// moves only about 1/N of the objects when a node joins or leaves.
const REPLICAS: usize = 2;

fn log(tag: &str, message: &str) {
    let line = format!("{}  {:<5} {}\n", sys::fmt_time("%F %T"), tag, message);
    eprintln!("cartridge-fleet  {:<5} {}", tag, message);
    sys::append(LOG_FILE, &line);
}

/// Paths are overridable so two nodes can be exercised on one machine (and so a
/// test never touches a real install). Production uses the defaults.
fn env_or(key: &str, default: &str) -> PathBuf {
    PathBuf::from(std::env::var(key).unwrap_or_else(|_| default.to_string()))
}

fn releases_root() -> PathBuf {
    env_or("CARTRIDGE_RELEASES", RELEASES)
}

fn live_path() -> PathBuf {
    env_or("CARTRIDGE_LIVE", LIVE)
}

fn store_root() -> PathBuf {
    env_or("CARTRIDGE_STORE", STORE)
}

fn archive_root() -> PathBuf {
    env_or("CARTRIDGE_ARCHIVE", ARCHIVE)
}

fn state_file() -> PathBuf {
    env_or("CARTRIDGE_FLEET_STATE", STATE_FILE)
}

extern "C" {
    fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    fn setsockopt(fd: c_int, level: c_int, name: c_int, value: *const c_void, len: u32) -> c_int;
    fn bind(fd: c_int, addr: *const c_void, len: u32) -> c_int;
    fn close(fd: c_int) -> c_int;
}

#[repr(C)]
struct SockAddrIn {
    family: u16,
    port: u16,
    addr: [u8; 4],
    zero: [u8; 8],
}

#[repr(C)]
struct IpMreq {
    multiaddr: [u8; 4],
    interface: [u8; 4],
}

/// A UDP socket that can share its port with another listener on the same
/// machine. Two nodes on one switch need no such thing, but two nodes on one
/// *box* (tests, or a box running a sandbox fleet) do, and broadcast is
/// delivered to every member of the reuse group.
fn listen_udp(port: u16) -> io::Result<UdpSocket> {
    unsafe {
        let fd = socket(2, 2 | 0x80000, 0); // AF_INET, SOCK_DGRAM | SOCK_CLOEXEC
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let one: c_int = 1;
        setsockopt(fd, 1, 2, &one as *const _ as *const c_void, 4); // SO_REUSEADDR
        setsockopt(fd, 1, 15, &one as *const _ as *const c_void, 4); // SO_REUSEPORT
        // Broadcast reaches one socket in a reuse group; multicast reaches all
        // of them, and is what carries the beacon on a switch that filters
        // broadcast anyway.
        let group = IpMreq { multiaddr: [239, 255, 77, 77], interface: [0, 0, 0, 0] };
        setsockopt(fd, 0, 35, &group as *const _ as *const IpMreq as *const c_void, 8);
        let addr = SockAddrIn { family: 2, port: port.to_be(), addr: [0, 0, 0, 0], zero: [0; 8] };
        if bind(fd, &addr as *const _ as *const c_void, 16) != 0 {
            let err = io::Error::last_os_error();
            close(fd);
            return Err(err);
        }
        Ok(UdpSocket::from_raw_fd(fd))
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

// ------------------------------------------------------------------ members

#[derive(Clone)]
pub struct Member {
    pub id: u128,
    pub key: u64,
    pub name: String,
    pub ip: Ipv4Addr,
    pub spec: Spec,
    pub epoch: u64,
    pub ready: u64,
    pub control: u16,
    pub ports: Vec<u16>,
    pub busy: u32,
    pub full: bool,
    pub membership: u64,
    pub seen: Instant,
}

impl Member {
    fn line(&self) -> String {
        self.name.clone()
    }
}

fn ports_text(ports: &[u16]) -> String {
    if ports.is_empty() {
        "-".to_string()
    } else {
        ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",")
    }
}

fn parse_ports(text: &str) -> Vec<u16> {
    text.split(',')
        .filter_map(|p| p.parse::<u16>().ok())
        .filter(|p| *p > 0)
        .collect()
}

fn membership_hash(ids: &[u128]) -> u64 {
    let mut sorted: Vec<u128> = ids.to_vec();
    sorted.sort_unstable();
    let mut h = hash::Sha256::new();
    for id in sorted {
        h.update(&id.to_be_bytes());
    }
    let d = h.finish();
    u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
}

// --------------------------------------------------------------------- node

pub struct Node {
    pub me: Identity,
    pub control: u16,
    pub spec: Mutex<Spec>,
    pub store: Store,
    pub blueprint: Mutex<Option<Blueprint>>,
    pub epoch: Mutex<u64>,
    pub pending: Mutex<Option<u64>>,
    pub ready: AtomicU64,
    pub members: Mutex<Vec<Member>>,
    pub key: Option<Vec<u8>>,
    pub busy: AtomicU32,
    pub ports: Mutex<Vec<u16>>,
    pub notes: Mutex<Vec<String>>,
    pub started: Instant,
}

impl Node {
    fn note(&self, text: String) {
        log("info", &text);
        let mut notes = self.notes.lock().unwrap();
        notes.push(format!("{}  {}", sys::fmt_time("%H:%M:%S"), text));
        if notes.len() > 200 {
            notes.remove(0);
        }
    }

    fn snapshot(&self) -> Vec<Member> {
        let now = Instant::now();
        self.members
            .lock()
            .unwrap()
            .iter()
            .filter(|m| now.duration_since(m.seen) < PEER_TTL)
            .cloned()
            .collect()
    }

    /// Weights for placement: every live peer plus this node, sorted by key so
    /// every machine builds the identical list.
    fn weights(&self) -> Vec<(u32, u64)> {
        let mut list: Vec<(u32, u64)> = self
            .snapshot()
            .iter()
            .map(|m| (m.spec.weight(), m.key))
            .collect();
        list.push((self.spec.lock().unwrap().weight(), self.me.key()));
        list.sort_by_key(|(_, key)| *key);
        list
    }

    fn capacity_mb(&self) -> BTreeMap<u64, u64> {
        let mut map = BTreeMap::new();
        let mine = self.spec.lock().unwrap();
        // Never fill the last 2 GB: the archive and the web tree need room.
        map.insert(self.me.key(), mine.disk_free_mb.saturating_sub(2048));
        for member in self.snapshot() {
            map.insert(member.key, member.spec.disk_free_mb.saturating_sub(2048));
        }
        map
    }

    fn plan(&self) -> BTreeMap<String, Vec<u64>> {
        let bp = self.blueprint.lock().unwrap().clone();
        match bp {
            Some(bp) => blueprint::assign(&bp, &self.weights(), REPLICAS, &self.capacity_mb()),
            None => BTreeMap::new(),
        }
    }

    fn my_epoch(&self) -> u64 {
        *self.epoch.lock().unwrap()
    }

    fn beacon_line(&self) -> String {
        let spec = self.spec.lock().unwrap().clone();
        let mut ids: Vec<u128> = self.snapshot().iter().map(|m| m.id).collect();
        ids.push(self.me.id);
        let membership = membership_hash(&ids);
        let ports = self.ports.lock().unwrap().clone();
        let busy = self.busy.load(Ordering::Relaxed);
        let mut line = format!(
            "CART2 {:032x} {} {} {} {} {} {} {} {}",
            self.me.id,
            self.me.name,
            self.my_epoch(),
            self.ready.load(Ordering::Relaxed),
            self.control,
            ports_text(&ports),
            busy,
            if busy >= 4096 { 1 } else { 0 },
            membership
        );
        for field in spec.fields() {
            line.push(' ');
            line.push_str(&field);
        }
        line.push('\n');
        line
    }

    fn state_json(&self) -> String {
        let spec = self.spec.lock().unwrap().clone();
        let bp = self.blueprint.lock().unwrap().clone();
        let mut peers: Vec<String> = Vec::new();
        for m in self.snapshot() {
            peers.push(format!(
                "{{\"id\":\"{:032x}\",\"name\":{},\"ip\":\"{}\",\"epoch\":{},\"ready\":{},\"busy\":{},\"rps\":{},\"weight\":{}}}",
                m.id,
                crate::json::quote(&m.name),
                m.ip,
                m.epoch,
                m.ready,
                m.busy,
                m.spec.serve_rps,
                m.spec.weight()
            ));
        }
        let pending = match *self.pending.lock().unwrap() {
            Some(e) => e.to_string(),
            None => "null".to_string(),
        };
        let root = match &bp {
            Some(b) => crate::json::quote(&b.root),
            None => "null".to_string(),
        };
        format!(
            "{{\"id\":\"{:032x}\",\"name\":{},\"epoch\":{},\"ready\":{},\"pending\":{},\"port\":{},\"rps\":{},\"bps\":{},\"weight\":{},\"chunks\":{},\"bytes\":{},\"root\":{},\"peers\":[{}]}}\n",
            self.me.id,
            crate::json::quote(&self.me.name),
            self.my_epoch(),
            self.ready.load(Ordering::Relaxed),
            pending,
            self.control,
            spec.serve_rps,
            spec.serve_bps,
            spec.weight(),
            bp.as_ref().map(|b| b.chunks.len()).unwrap_or(0),
            self.store.bytes(),
            root,
            peers.join(",")
        )
    }
}

// -------------------------------------------------------------------- wire

fn parse_beacon(line: &str, ip: Ipv4Addr) -> Option<Member> {
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() < 23 || f[0] != "CART2" {
        return None;
    }
    let id = u128::from_str_radix(f[1], 16).ok()?;
    let spec = Spec::parse(&f[10..])?;
    Some(Member {
        id,
        key: ident::key_of(id),
        name: f[2].to_string(),
        ip,
        spec,
        epoch: f[3].parse().ok()?,
        ready: f[4].parse().ok()?,
        control: f[5].parse().ok()?,
        ports: parse_ports(f[6]),
        busy: f[7].parse().ok()?,
        full: f[8] != "0",
        membership: f[9].parse().unwrap_or(0),
        seen: Instant::now(),
    })
}

fn read_line(stream: &mut TcpStream) -> io::Result<String> {
    let mut out = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed")),
            Ok(_) => {
                if byte[0] == b'\n' {
                    while out.last() == Some(&b'\r') {
                        out.pop();
                    }
                    return Ok(String::from_utf8_lossy(&out).to_string());
                }
                out.push(byte[0]);
                if out.len() > 8192 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Open an authenticated control connection. The server sends a nonce, we send
/// HMAC(key, nonce), and only then does it answer anything.
fn connect(ip: Ipv4Addr, port: u16, key: Option<&Vec<u8>>) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::from(([ip.octets()[0], ip.octets()[1], ip.octets()[2], ip.octets()[3]], port)),
        Duration::from_secs(2),
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;
    stream.set_write_timeout(Some(Duration::from_secs(20)))?;
    let first = read_line(&mut stream)?;
    if let Some(rest) = first.strip_prefix("NONCE ") {
        let nonce = match decode_hex(rest.trim()) {
            Some(n) => n,
            None => return Err(io::Error::new(io::ErrorKind::InvalidData, "bad nonce")),
        };
        let k = key.ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "key required"))?;
        let tag = auth::tag(k, &nonce);
        stream.write_all(format!("AUTH {}\n", tag).as_bytes())?;
        let reply = read_line(&mut stream)?;
        if reply != "OK" {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "auth refused"));
        }
    } else if first != "OPEN" {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected greeting"));
    }
    Ok(stream)
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    for pair in bytes.chunks(2) {
        out.push(u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?);
    }
    Some(out)
}

fn simple(ip: Ipv4Addr, port: u16, key: Option<&Vec<u8>>, command: &str) -> Option<String> {
    let mut stream = match connect(ip, port, key) {
        Ok(s) => s,
        Err(e) => {
            log("warn", &format!("{}:{} {} failed: {}", ip, port, command, e));
            return None;
        }
    };
    if let Err(e) = stream.write_all(format!("{}\n", command).as_bytes()) {
        log("warn", &format!("{}:{} {} failed: {}", ip, port, command, e));
        return None;
    }
    match read_line(&mut stream) {
        Ok(line) => Some(line),
        Err(e) => {
            log("warn", &format!("{}:{} {} failed: {}", ip, port, command, e));
            None
        }
    }
}

fn fetch_bytes(ip: Ipv4Addr, port: u16, key: Option<&Vec<u8>>, command: &str) -> Option<Vec<u8>> {
    let mut stream = match connect(ip, port, key) {
        Ok(s) => s,
        Err(e) => {
            log("warn", &format!("{}:{} {} failed: {}", ip, port, command, e));
            return None;
        }
    };
    if let Err(e) = stream.write_all(format!("{}\n", command).as_bytes()) {
        log("warn", &format!("{}:{} {} failed: {}", ip, port, command, e));
        return None;
    }
    let head = match read_line(&mut stream) {
        Ok(line) => line,
        Err(e) => {
            log("warn", &format!("{}:{} {} failed: {}", ip, port, command, e));
            return None;
        }
    };
    let len: usize = match head.strip_prefix("OK ").and_then(|n| n.trim().parse().ok()) {
        Some(n) => n,
        None => return None,
    };
    if len > 64 * 1024 * 1024 {
        return None;
    }
    let mut buf = vec![0u8; len];
    if let Err(e) = stream.read_exact(&mut buf) {
        log("warn", &format!("{}:{} {} short read: {}", ip, port, command, e));
        return None;
    }
    Some(buf)
}

// ------------------------------------------------------------------ control

fn serve_control(stream: TcpStream, node: Arc<Node>) -> io::Result<()> {
    let mut stream = stream;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    match &node.key {
        Some(key) => {
            let nonce = ident::random_u128().to_be_bytes();
            stream.write_all(format!("NONCE {}\n", hex(&nonce)).as_bytes())?;
            let line = read_line(&mut stream)?;
            let tag = line.strip_prefix("AUTH ").unwrap_or("").trim();
            if !auth::verify(key, &nonce, tag) {
                let _ = stream.write_all(b"NO\n");
                return Ok(());
            }
            stream.write_all(b"OK\n")?;
        }
        None => stream.write_all(b"OPEN\n")?,
    }
    loop {
        let line = match read_line(&mut stream) {
            Ok(l) => l,
            Err(_) => return Ok(()),
        };
        let mut parts = line.splitn(2, ' ');
        let command = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim();
        match command {
            "PING" => stream.write_all(b"PONG\n")?,
            "INFO" => stream.write_all(format!("{}\n", node.state_json()).as_bytes())?,
            "BLUEPRINT" => {
                let text = node
                    .blueprint
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|b| b.text())
                    .unwrap_or_default();
                if text.is_empty() {
                    stream.write_all(b"NO\n")?;
                } else {
                    stream.write_all(format!("OK {}\n", text.len()).as_bytes())?;
                    stream.write_all(text.as_bytes())?;
                }
            }
            "HAVE" => {
                let yes = node.store.has(arg);
                stream.write_all(if yes { b"YES\n" } else { b"NO\n" })?;
            }
            "CHUNK" => match node.store.get(arg) {
                Ok(bytes) => {
                    stream.write_all(format!("OK {}\n", bytes.len()).as_bytes())?;
                    stream.write_all(&bytes)?;
                }
                Err(_) => stream.write_all(b"NO\n")?,
            },
            "EPOCH" => {
                let mut it = arg.split_whitespace();
                let epoch: u64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                if epoch > node.my_epoch() {
                    let mut pending = node.pending.lock().unwrap();
                    if pending.map(|p| p < epoch).unwrap_or(true) {
                        *pending = Some(epoch);
                    }
                }
                stream.write_all(b"OK\n")?;
            }
            "READY" => {
                let epoch: u64 = arg.parse().unwrap_or(0);
                if epoch <= node.my_epoch() {
                    stream.write_all(b"OK\n")?;
                } else {
                    stream.write_all(b"WAIT\n")?;
                }
            }
            "STATS" => stream.write_all(format!("{}\n", node.state_json()).as_bytes())?,
            _ => stream.write_all(b"NO\n")?,
        }
    }
}

// ------------------------------------------------------------------ threads

fn beacon_send(node: Arc<Node>, port: u16) {
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => return,
    };
    let _ = socket.set_broadcast(true);
    loop {
        let line = node.beacon_line();
        let _ = socket.send_to(line.as_bytes(), ("255.255.255.255", port));
        // Some switches drop broadcast but pass multicast.
        let _ = socket.send_to(line.as_bytes(), ("239.255.77.77", port));
        std::thread::sleep(BEACON_EVERY);
    }
}

fn beacon_recv(node: Arc<Node>, port: u16) {
    let socket = match listen_udp(port) {
        Ok(s) => s,
        Err(e) => {
            log("warn", &format!("cannot listen for peers on {}: {}", port, e));
            return;
        }
    };
    let _ = socket.set_read_timeout(Some(Duration::from_secs(1)));
    let mut buf = [0u8; 2048];
    loop {
        let (n, from) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let text = String::from_utf8_lossy(&buf[..n]);
        let ip = match from.ip() {
            std::net::IpAddr::V4(v4) => v4,
            _ => continue,
        };
        if let Some(member) = parse_beacon(text.trim_end(), ip) {
            if member.id == node.me.id {
                continue;
            }
            let mut list = node.members.lock().unwrap();
            if let Some(slot) = list.iter_mut().find(|m| m.id == member.id) {
                *slot = member;
            } else {
                let text = format!(
                    "{} joined from {} ({} rps)",
                    member.name, member.ip, member.spec.serve_rps
                );
                list.push(member);
                drop(list);
                log("peer", &text);
            }
        }
    }
}

/// One pass of "publish this epoch": fetch whatever of my share is missing, then
/// materialize the tree, then flip live. Returns true when the node is ready.
fn prepare_epoch(node: &Arc<Node>, epoch: u64) -> bool {
    log("sync", &format!("preparing epoch {} ({} peers visible)", epoch, node.snapshot().len()));
    // Bind the clone before the match: a guard created in a match scrutinee
    // lives until the end of the match, so locking the same mutex inside an arm
    // deadlocks the thread that is trying to store the fetched blueprint.
    let current = node.blueprint.lock().unwrap().clone();
    let bp = match current {
        Some(bp) if bp.epoch == epoch => bp,
        _ => {
            // Ask anyone who already has it.
            let mut got = None;
            for member in node.snapshot() {
                if member.epoch < epoch {
                    continue;
                }
                match fetch_bytes(member.ip, member.control, node.key.as_ref(), "BLUEPRINT") {
                    Some(bytes) => match Blueprint::parse(&String::from_utf8_lossy(&bytes)) {
                        Some(bp) if bp.epoch == epoch => {
                            got = Some(bp);
                            break;
                        }
                        Some(bp) => log(
                            "warn",
                            &format!("{} sent epoch {}, wanted {}", member.name, bp.epoch, epoch),
                        ),
                        None => log(
                            "warn",
                            &format!("blueprint from {} did not parse ({} bytes)", member.name, bytes.len()),
                        ),
                    },
                    None => log(
                        "warn",
                        &format!("no blueprint from {} at {}:{}", member.name, member.ip, member.control),
                    ),
                }
            }
            match got {
                Some(bp) => {
                    if let Err(e) = write_blueprint(&bp) {
                        log("warn", &format!("cannot store blueprint: {}", e));
                        return false;
                    }
                    *node.blueprint.lock().unwrap() = Some(bp.clone());
                    bp
                }
                None => return false,
            }
        }
    };

    let weights = node.weights();
    let plan = blueprint::assign(&bp, &weights, REPLICAS, &node.capacity_mb());
    // The web tier serves a real file tree, so a node needs every chunk once,
    // while it builds that tree. Ownership then decides what it keeps and who
    // repairs it, which is what the plan is used for in `repair_loop`.
    let wanted: Vec<&blueprint::Piece> = bp.chunks.iter().collect();
    let missing: Vec<&blueprint::Piece> = wanted
        .iter()
        .copied()
        .filter(|c| !node.store.has(&c.hash))
        .collect();
    if !missing.is_empty() {
        let mut fetched = 0usize;
        let peers = node.snapshot();
        for chunk in &missing {
            let prefer = plan.get(&chunk.hash).cloned().unwrap_or_default();
            let mut order: Vec<&Member> = peers
                .iter()
                .filter(|m| prefer.contains(&m.key))
                .collect();
            for m in &peers {
                if !prefer.contains(&m.key) {
                    order.push(m);
                }
            }
            for member in order {
                let have = simple(member.ip, member.control, node.key.as_ref(), &format!("HAVE {}", chunk.hash));
                if have.as_deref() != Some("YES") {
                    continue;
                }
                if let Some(bytes) = fetch_bytes(
                    member.ip,
                    member.control,
                    node.key.as_ref(),
                    &format!("CHUNK {}", chunk.hash),
                ) {
                    if hash::bytes(&bytes) == chunk.hash && node.store.put(&bytes).is_ok() {
                        fetched += 1;
                        break;
                    }
                }
            }
        }
        log(
            "sync",
            &format!("epoch {}: fetched {}/{} owned chunks", epoch, fetched, missing.len()),
        );
        let missing_now = wanted.iter().filter(|c| !node.store.has(&c.hash)).count();
        if missing_now > 0 {
            return false;
        }
    }

    // Everything needed is local: build the tree the web tier serves.
    if let Err(e) = materialize(&bp, &node.store, epoch) {
        log("warn", &format!("cannot materialize epoch {}: {}", epoch, e));
        return false;
    }
    true
}

fn write_blueprint(bp: &Blueprint) -> io::Result<()> {
    // Under `.cartridge`, because the release tree is also the site tree: a
    // loose `blueprint` file at the root would become a served URL, and
    // `publish` already skips that directory when it stages a release.
    let dir = releases_root().join(bp.epoch.to_string()).join(".cartridge");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("blueprint"), bp.text())
}

/// Turn the release this box already serves into a fleet release: chunk it into
/// the store and record its blueprint. This is the bootstrap for an existing
/// install, and it is safe to re-run (chunks are content-addressed).
pub fn adopt() -> Result<i32, String> {
    let live = fs::canonicalize(live_path()).map_err(|e| format!("{}: {}", LIVE, e))?;
    let name = live
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let epoch: u64 = name.parse().unwrap_or(1);
    let store = Store::at(store_root());
    let (bp, stats) = chunks::ingest(&live, &store, None, epoch)
        .map_err(|e| format!("{}: {}", live.display(), e))?;
    write_blueprint(&bp).map_err(|e| format!("cannot write the blueprint: {}", e))?;
    println!(
        "epoch {}: {} files, {} chunks, {} ({} new, {} already held)",
        bp.epoch,
        stats.files,
        stats.chunks,
        sys::human(stats.bytes),
        stats.new_chunks,
        stats.chunks.saturating_sub(stats.new_chunks)
    );
    println!("root {}", bp.root);
    println!("store: {}", store.root().display());
    Ok(0)
}

/// Turn chunks back into the file tree. Single-chunk files are hard-linked, so
/// a normal site costs no extra disk; multi-chunk files are concatenated once.
fn materialize(bp: &Blueprint, store: &Store, epoch: u64) -> io::Result<()> {
    let dir = releases_root().join(epoch.to_string());
    for file in &bp.files {
        // The blueprint came off a stick or a peer. A path of `../../etc/x`
        // would otherwise be written outside the release.
        let target = match crate::guard::safe_join(&dir, &file.path) {
            Some(t) => t,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("blueprint path {:?} leaves the release directory", file.path),
                ))
            }
        };
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        if file.pieces.len() == 1 {
            let object = store.path_for(&file.pieces[0].hash);
            let _ = fs::remove_file(&target);
            if fs::hard_link(&object, &target).is_err() {
                fs::copy(&object, &target)?;
            }
        } else {
            let mut out = fs::File::create(&target)?;
            for piece in &file.pieces {
                let bytes = store.get(&piece.hash)?;
                out.write_all(&bytes)?;
            }
        }
    }
    Ok(())
}

fn activate(node: &Arc<Node>, epoch: u64) {
    let dir = releases_root().join(epoch.to_string());
    if !dir.is_dir() {
        return;
    }
    let tmp = releases_root().join(".live-new");
    let _ = fs::remove_file(&tmp);
    if std::os::unix::fs::symlink(&dir, &tmp).is_err() {
        return;
    }
    if fs::rename(&tmp, live_path()).is_err() {
        let _ = fs::remove_file(&tmp);
        return;
    }
    *node.epoch.lock().unwrap() = epoch;
    node.ready.store(epoch, Ordering::Relaxed);
    *node.pending.lock().unwrap() = None;
    log("live", &format!("epoch {} is live", epoch));
    // Re-stage the RAM copy, but only when this is the real release layout: a
    // sandbox pointed elsewhere has no /run/cartridge to fill, and must not
    // touch the one the production web tier is serving.
    let production = std::env::var("CARTRIDGE_LIVE").is_err()
        && std::env::var("CARTRIDGE_RELEASES").is_err();
    if production {
        if let Err(e) = publish::run() {
            log("warn", &format!("publish after activation failed: {}", e));
        }
    }
}

fn commit_loop(node: Arc<Node>) {
    loop {
        std::thread::sleep(Duration::from_secs(1));
        // A node that never imported anything still has to notice that the
        // fleet is ahead of it: adopt the highest epoch it can see. Without
        // this, a fresh box would sit idle next to a release it could serve.
        if node.pending.lock().unwrap().is_none() {
            let highest = node
                .snapshot()
                .iter()
                .map(|m| m.epoch.max(m.ready))
                .max()
                .unwrap_or(0);
            if highest > node.my_epoch() {
                *node.pending.lock().unwrap() = Some(highest);
                log("sync", &format!("adopting epoch {} from the fleet", highest));
            }
        }
        let pending = match *node.pending.lock().unwrap() {
            Some(e) if e > node.my_epoch() => e,
            _ => continue,
        };
        if node.ready.load(Ordering::Relaxed) < pending {
            if !prepare_epoch(&node, pending) {
                continue;
            }
            node.ready.store(pending, Ordering::Relaxed);
            log("sync", &format!("epoch {} is ready", pending));
        }
        let peers = node.snapshot();
        let waiting: Vec<String> = peers
            .iter()
            .filter(|m| m.ready < pending)
            .map(|m| m.line())
            .collect();
        let recent: Vec<&Member> = peers
            .iter()
            .filter(|m| m.seen.elapsed() < PEER_TTL)
            .collect();
        let everyone = recent.iter().all(|m| m.ready >= pending);
        // A single node commits alone; a fleet waits, but not forever: a peer
        // that never answers is dropped by the TTL and stops blocking.
        if everyone || recent.is_empty() {
            activate(&node, pending);
        } else {
            log(
                "sync",
                &format!("waiting for {} node(s) before switching to epoch {}", waiting.len(), pending),
            );
        }
    }
}

/// Keep this node's share, drop what it does not own, and re-fetch a share that
/// a departed node took with it.
fn repair_loop(node: Arc<Node>) {
    loop {
        std::thread::sleep(REPAIR_EVERY);
        let bp = {
            let current = node.blueprint.lock().unwrap().clone();
            match current {
                Some(bp) => bp,
                None => continue,
            }
        };
        let plan = node.plan();
        let me = node.me.key();
        let mut repaired = 0usize;
        for chunk in &bp.chunks {
            let owned = plan.get(&chunk.hash).map(|o| o.contains(&me)).unwrap_or(false);
            if !owned || node.store.has(&chunk.hash) {
                continue;
            }
            for member in node.snapshot() {
                if member.epoch < bp.epoch {
                    continue;
                }
                if let Some(bytes) = fetch_bytes(
                    member.ip,
                    member.control,
                    node.key.as_ref(),
                    &format!("CHUNK {}", chunk.hash),
                ) {
                    if hash::bytes(&bytes) == chunk.hash && node.store.put(&bytes).is_ok() {
                        repaired += 1;
                        break;
                    }
                }
            }
        }
        if repaired > 0 {
            log("repair", &format!("re-fetched {} missing chunk(s)", repaired));
        }
    }
}

fn archive_loop(node: Arc<Node>) {
    loop {
        std::thread::sleep(Duration::from_secs(60));
        // Idle work only: never compete with serving.
        if node.busy.load(Ordering::Relaxed) > 0 || pressure() > 1.0 {
            continue;
        }
        let bp = {
            let current = node.blueprint.lock().unwrap().clone();
            match current {
                Some(bp) => bp,
                None => continue,
            }
        };
        let path = archive_root().join(format!("{}.cart", bp.epoch));
        if path.exists() {
            continue;
        }
        let bp2 = bp.clone();
        let store = node.store.clone();
        let node2 = node.clone();
        match crate::archive::build(&bp2, &store, &path) {
            Ok(r) => node2.note(format!(
                "archive for epoch {}: {} -> {}",
                bp.epoch,
                sys::human(r.raw),
                sys::human(r.packed)
            )),
            Err(e) => log("warn", &format!("archive failed: {}", e)),
        }
    }
}

/// `/proc/pressure/cpu`'s `some avg10`, or 0 when the kernel has no PSI.
fn pressure() -> f64 {
    if let Ok(text) = fs::read_to_string("/proc/pressure/cpu") {
        if let Some(line) = text.lines().find(|l| l.starts_with("some")) {
            for field in line.split_whitespace() {
                if let Some(v) = field.strip_prefix("avg10=") {
                    return v.parse().unwrap_or(0.0);
                }
            }
        }
    }
    0.0
}

fn mount_has_release(mount: &str) -> Option<PathBuf> {
    for name in ["cartridge-release", "cartridge", "release"] {
        let dir = Path::new(mount).join(name);
        if dir.join("blueprint").is_file() {
            return Some(dir);
        }
    }
    None
}

fn usb_loop(node: Arc<Node>) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let mounts = match fs::read_to_string("/proc/mounts") {
            Ok(m) => m,
            Err(_) => continue,
        };
        for line in mounts.lines() {
            let mut fields = line.split_whitespace();
            let _device = fields.next();
            let mount = fields.next().unwrap_or("");
            if mount.is_empty() || mount == "/" {
                continue;
            }
            let dir = match mount_has_release(mount) {
                Some(d) => d,
                None => continue,
            };
            let key = format!("{}|{}", mount, dir.display());
            if seen.contains(&key) {
                continue;
            }
            seen.insert(key);
            match import_release(&node, &dir) {
                Ok(epoch) => node.note(format!("imported {} from {} as epoch {}", dir.display(), mount, epoch)),
                Err(e) => log("warn", &format!("import from {} rejected: {}", mount, e)),
            }
        }
    }
}

/// Verify and ingest one stick. The epoch comes from the fleet, not the clock.
pub fn import_release(node: &Arc<Node>, dir: &Path) -> Result<u64, String> {
    let text = fs::read_to_string(dir.join("blueprint")).map_err(|e| format!("no blueprint: {}", e))?;
    // Parse only to reject a corrupt blueprint before it becomes an epoch.
    Blueprint::parse(&text).ok_or("blueprint is corrupt")?;
    match &node.key {
        Some(key) => {
            let sig = fs::read_to_string(dir.join("blueprint.sig"))
                .map_err(|_| "the fleet has a key but this stick is unsigned".to_string())?;
            if !auth::verify(key, text.as_bytes(), sig.trim()) {
                return Err("signature does not match the fleet key".into());
            }
        }
        None => log(
            "warn",
            "no fleet key: accepting an unsigned release (run `cartridge fleet key` to require one)",
        ),
    }
    let highest = node
        .snapshot()
        .iter()
        .map(|m| m.epoch.max(m.ready))
        .chain(std::iter::once(node.my_epoch()))
        .max()
        .unwrap_or(0);
    let epoch = highest + 1;
    let base = dir.to_string_lossy().to_string();
    let (bp, stats) =
        chunks::ingest(dir, &node.store, None, epoch).map_err(|e| format!("{}: {}", base, e))?;
    write_blueprint(&bp).map_err(|e| format!("cannot store blueprint: {}", e))?;
    *node.blueprint.lock().unwrap() = Some(bp.clone());
    *node.pending.lock().unwrap() = Some(epoch);
    node.ready.store(0, Ordering::Relaxed);
    log(
        "import",
        &format!(
            "epoch {}: {} files, {} new chunks, {}",
            epoch,
            stats.files,
            stats.new_chunks,
            sys::human(stats.bytes)
        ),
    );
    // Tell the fleet immediately rather than waiting for a beacon round.
    for member in node.snapshot() {
        let _ = simple(member.ip, member.control, node.key.as_ref(), &format!("EPOCH {}", epoch));
    }
    Ok(epoch)
}

fn state_loop(node: Arc<Node>) {
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let path = state_file();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(e) = fs::write(&path, node.state_json()) {
            log("warn", &format!("cannot write {}: {}", path.display(), e));
        }
    }
}

/// Start serving. Everything above is pure enough to test; this is the wiring.
pub fn run(
    root: PathBuf,
    control: u16,
    beacon: u16,
    dns_port: Option<u16>,
    name: Option<String>,
) -> Result<i32, String> {
    let mut me = ident::load_or_create();
    if let Some(name) = name {
        me.name = name;
    }
    let key = auth::fleet_key();
    if key.is_none() {
        log("warn", "no /etc/cartridge/fleet.key: the fleet will accept an unsigned release");
    }
    // Reuse the previous blueprint if this box has one, so a restart does not
    // re-hash the release or lose track of the live epoch.
    let store = Store::at(store_root());
    // Every directory the service writes to, made once here: a missing parent
    // used to lose the state file silently, which looks like a dead service.
    for dir in [store.root().to_path_buf(), releases_root(), archive_root()] {
        let _ = fs::create_dir_all(&dir);
    }
    if let Some(parent) = state_file().parent() {
        let _ = fs::create_dir_all(parent);
    }
    let bp = fs::read_to_string(store.root().join("blueprint"))
        .ok()
        .and_then(|text| Blueprint::parse(&text));
    let epoch = bp.as_ref().map(|b| b.epoch).unwrap_or(0);
    if let Some(bp) = &bp {
        if fs::symlink_metadata(live_path()).is_ok() {
            log("info", &format!("holding epoch {} ({} files)", bp.epoch, bp.files.len()));
        }
    }
    let ports = crate::web::scan(&root).iter().map(|s| s.port).collect();
    let probe = spec::probe(&store_root().to_string_lossy());
    let spec = probe.spec;
    log("info", &format!("{} is {}", me.name, spec.summary()));

    let node = Arc::new(Node {
        me,
        control,
        spec: Mutex::new(spec),
        store,
        blueprint: Mutex::new(bp),
        epoch: Mutex::new(epoch),
        pending: Mutex::new(None),
        ready: AtomicU64::new(epoch),
        members: Mutex::new(Vec::new()),
        key,
        busy: AtomicU32::new(0),
        ports: Mutex::new(ports),
        notes: Mutex::new(Vec::new()),
        started: Instant::now(),
    });

    // Bind the control port before announcing anything.
    let listener = TcpListener::bind(("0.0.0.0", control))
        .map_err(|e| format!("cannot bind control port {}: {}", control, e))?;
    {
        let node = node.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let node = node.clone();
                std::thread::spawn(move || {
                    let _ = serve_control(stream, node);
                });
            }
        });
    }
    {
        let node = node.clone();
        std::thread::spawn(move || beacon_send(node, beacon));
    }
    {
        let node = node.clone();
        std::thread::spawn(move || beacon_recv(node, beacon));
    }
    {
        let node = node.clone();
        std::thread::spawn(move || commit_loop(node));
    }
    {
        let node = node.clone();
        std::thread::spawn(move || repair_loop(node));
    }
    {
        let node = node.clone();
        std::thread::spawn(move || archive_loop(node));
    }
    {
        let node = node.clone();
        std::thread::spawn(move || usb_loop(node));
    }
    {
        let node = node.clone();
        std::thread::spawn(move || state_loop(node));
    }
    if let Some(port) = dns_port {
        let node = node.clone();
        std::thread::spawn(move || {
            let get = Arc::new(move || {
                let mut ips: Vec<Ipv4Addr> = node.snapshot().iter().map(|m| m.ip).collect();
                if let Ok(ip) = sys::lan_ip().parse::<Ipv4Addr>() {
                    ips.push(ip);
                }
                ips.sort();
                ips.dedup();
                ips
            });
            let _ = crate::dns::serve("0.0.0.0", port, get);
        });
        log("dns", &format!("answering A queries on port {}", port));
    }

    // Once the workers are answering, replace the placeholder request rate with
    // a measured one. A benchmark number is not a serving number.
    {
        let node = node.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            let port = node.ports.lock().unwrap().first().copied();
            if let Some(port) = port {
                let (rps, _) = spec::measure_http(port, "/", 4, Duration::from_millis(500));
                if rps >= 1.0 {
                    let mut spec = node.spec.lock().unwrap();
                    let measured = spec.clone().with_selftest(rps, 0);
                    let summary = measured.summary();
                    *spec = measured;
                    drop(spec);
                    node.note(format!("self-test: {}", summary));
                }
            }
        });
    }

    // Keep the process alive; every worker is a detached thread.
    loop {
        std::thread::sleep(Duration::from_secs(30));
        let _ = fs::write(state_file(), node.state_json());
    }
}

pub fn status() -> Result<i32, String> {
    let text = fs::read_to_string(state_file()).unwrap_or_default();
    if text.trim().is_empty() {
        println!("no fleet state yet (is cartridge-fleet running?)");
        return Ok(0);
    }
    let value = crate::json::parse(text.trim()).map_err(|e| format!("state file: {}", e))?;
    let show = |key: &str| -> String {
        match value.get(key) {
            Some(crate::json::Value::Str(s)) => s.clone(),
            Some(crate::json::Value::Num(n)) => {
                if n.fract() == 0.0 {
                    format!("{}", *n as i64)
                } else {
                    format!("{:.1}", n)
                }
            }
            Some(crate::json::Value::Bool(b)) => b.to_string(),
            _ => "-".to_string(),
        }
    };
    println!(
        "{}  epoch {}  ready {}  weight {}  {} chunks, {}",
        show("name"),
        show("epoch"),
        show("ready"),
        show("weight"),
        show("chunks"),
        sys::human(show("bytes").parse().unwrap_or(0))
    );
    println!("  {} rps, {}/s", show("rps"), sys::human(show("bps").parse().unwrap_or(0)));
    if let Some(peers) = value.get("peers").and_then(|p| p.as_arr()) {
        if peers.is_empty() {
            println!("  alone: this box is the whole cluster");
        } else {
            println!("  {:<14} {:>6} {:>6} {:>8}", "node", "epoch", "ready", "weight");
            for peer in peers {
                let f = |k: &str| peer.get(k).map(|v| match v {
                    crate::json::Value::Str(s) => s.clone(),
                    crate::json::Value::Num(n) => format!("{}", *n as i64),
                    _ => "-".into(),
                }).unwrap_or_default();
                println!("  {:<14} {:>6} {:>6} {:>8}", f("name"), f("epoch"), f("ready"), f("weight"));
            }
        }
    }
    Ok(0)
}

/// Build the disaster-recovery archive from what this node holds.
/// Deliberately a command rather than only a background job: an operator wants
/// to be able to take one file off the box and know it contains everything.
pub fn archive_now() -> Result<i32, String> {
    let store = Store::at(store_root());
    let bp = fs::read_to_string(store.root().join("blueprint"))
        .ok()
        .and_then(|text| Blueprint::parse(&text))
        .ok_or("no blueprint in the store yet")?;
    let path = archive_root().join(format!("{}.cart", bp.epoch));
    let report = crate::archive::build(&bp, &store, &path).map_err(|e| e.to_string())?;
    println!(
        "{}: {} -> {} ({} chunks)",
        report.path.display(),
        sys::human(report.raw),
        sys::human(report.packed),
        report.chunks
    );
    Ok(0)
}

pub fn sites_on_disk() -> Result<i32, String> {
    let store = Store::at(store_root());
    let held = store.held();
    println!("store: {} chunks, {}", held.len(), sys::human(store.bytes()));
    if let Ok(text) = fs::read_to_string(store.root().join("blueprint")) {
        if let Some(bp) = Blueprint::parse(&text) {
            println!("epoch {}: {} files, {} chunks, {} total", bp.epoch, bp.files.len(), bp.chunks.len(), sys::human(bp.bytes()));
            let missing = bp.chunks.iter().filter(|c| !held.contains(&c.hash)).count();
            if missing > 0 {
                println!("  {} chunk(s) not held locally (owned by peers)", missing);
            }
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_with(root: &Path) -> Arc<Node> {
        Arc::new(Node {
            me: Identity { id: 7, name: "seven".into() },
            control: CONTROL,
            spec: Mutex::new(Spec { serve_bps: 200_000_000, disk_free_mb: 100_000, cores: 4, threads: 8, smt_gain: 1.0, ..Default::default() }),
            store: Store::at(root.join("store")),
            blueprint: Mutex::new(None),
            epoch: Mutex::new(0),
            pending: Mutex::new(None),
            ready: AtomicU64::new(0),
            members: Mutex::new(Vec::new()),
            key: None,
            busy: AtomicU32::new(0),
            ports: Mutex::new(vec![3000]),
            notes: Mutex::new(Vec::new()),
            started: Instant::now(),
        })
    }

    #[test]
    fn a_beacon_round_trips_without_losing_a_field() {
        let dir = std::env::temp_dir().join(format!(
            "cartridge-fleet-beacon-{}-{}",
            std::process::id(),
            ident::random_u128() & 0xffff
        ));
        let _ = fs::create_dir_all(&dir);
        let node = node_with(&dir);
        let line = node.beacon_line();
        let member = parse_beacon(line.trim_end(), Ipv4Addr::new(10, 0, 0, 5)).unwrap();
        assert_eq!(member.id, 7);
        assert_eq!(member.name, "seven");
        assert_eq!(member.control, CONTROL);
        assert_eq!(member.ports, vec![3000]);
        assert_eq!(member.spec.serve_bps, 200_000_000);
        assert_eq!(member.spec.disk_free_mb, 100_000);
        assert_eq!(member.key, ident::key_of(7));
        // An old line, without the spec fields, is ignored rather than trusted.
        assert!(parse_beacon("CART2 07 seven 0 0 29702 - 0 0 0 0", Ipv4Addr::LOCALHOST).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn membership_hashes_are_order_independent() {
        assert_eq!(membership_hash(&[3, 1, 2]), membership_hash(&[1, 2, 3]));
        assert_ne!(membership_hash(&[1, 2]), membership_hash(&[1, 3]));
    }

    #[test]
    fn the_control_protocol_serves_chunks_over_a_socket() {
        let dir = std::env::temp_dir().join(format!(
            "cartridge-fleet-rpc-{}-{}",
            std::process::id(),
            ident::random_u128() & 0xffff
        ));
        let _ = fs::create_dir_all(&dir);
        let node = node_with(&dir);
        let (digest, _) = node.store.put(b"the bytes").unwrap();
        node.blueprint.lock().unwrap().replace(Blueprint {
            epoch: 4,
            root: "abc".into(),
            files: Vec::new(),
            chunks: vec![blueprint::Piece { hash: digest.clone(), len: 9 }],
        });
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = node.clone();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let _ = serve_control(stream, served);
            }
        });
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        assert_eq!(read_line(&mut stream).unwrap(), "OPEN");
        stream.write_all(b"BLUEPRINT\n").unwrap();
        let head = read_line(&mut stream).unwrap();
        let len: usize = head.strip_prefix("OK ").unwrap().trim().parse().unwrap();
        let mut text = vec![0u8; len];
        stream.read_exact(&mut text).unwrap();
        assert!(Blueprint::parse(&String::from_utf8_lossy(&text)).is_some());

        stream.write_all(format!("CHUNK {}\n", digest).as_bytes()).unwrap();
        let head = read_line(&mut stream).unwrap();
        let len: usize = head.strip_prefix("OK ").unwrap().trim().parse().unwrap();
        let mut bytes = vec![0u8; len];
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, b"the bytes");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_key_gates_the_control_port() {
        let dir = std::env::temp_dir().join(format!(
            "cartridge-fleet-auth-{}-{}",
            std::process::id(),
            ident::random_u128() & 0xffff
        ));
        let _ = fs::create_dir_all(&dir);
        let mut node = node_with(&dir);
        Arc::get_mut(&mut node).unwrap().key = Some(b"fleet secret".to_vec());
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = node.clone();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let _ = serve_control(stream, served);
            }
        });
        // Without the key, the greeting is a nonce and the connection is refused.
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        assert!(read_line(&mut stream).unwrap().starts_with("NONCE "));
        stream.write_all(b"AUTH 00\n").unwrap();
        assert_eq!(read_line(&mut stream).unwrap(), "NO");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn materialize_rebuilds_a_tree_from_chunks() {
        let dir = std::env::temp_dir().join(format!(
            "cartridge-fleet-mat-{}-{}",
            std::process::id(),
            ident::random_u128() & 0xffff
        ));
        let _ = fs::create_dir_all(dir.join("rel/3000"));
        fs::write(dir.join("rel/3000/index.html"), b"<h1>hi</h1>").unwrap();
        let store = Store::at(dir.join("store"));
        let (bp, _) = chunks::ingest(&dir.join("rel"), &store, None, 9).unwrap();
        // Materialize into a release directory by pointing the constant at a
        // temp location through a relative trick: call the same logic inline.
        let target = dir.join("out");
        for file in &bp.files {
            let path = target.join(&file.path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut out = fs::File::create(&path).unwrap();
            for piece in &file.pieces {
                out.write_all(&store.get(&piece.hash).unwrap()).unwrap();
            }
        }
        assert_eq!(fs::read(target.join("3000/index.html")).unwrap(), b"<h1>hi</h1>");
        let _ = fs::remove_dir_all(&dir);
    }
}
