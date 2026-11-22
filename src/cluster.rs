//! Cluster mode: every cartridge on the switch finds the others, keeps the same
//! release, and shares the request load.
//!
//! Two jobs, two small protocols, no dependency and no configuration:
//!
//!   * discovery - a broadcast every half second on UDP 29700 carrying the node
//!     id, the live release, its content fingerprint, the worker count and how
//!     many connections that node is serving right now;
//!   * sync - a line protocol on TCP 29701 that hands a peer the release
//!     manifest and the files it does not already have, so a stick inserted in
//!     one box is served by all of them within a couple of seconds.
//!
//! In front of the local workers sits a relay that moves bytes with splice(2),
//! so a request never enters user space. A connection stays on the machine it
//! reached until that machine holds at least one per core; only then does a
//! quieter node get picked. One box with no peers therefore pays one loopback
//! hop and nothing else.
//!
//! The fleet is assumed to be mixed hardware on one switch, so nothing is taken
//! on trust. Every box times the responses it actually gets -- from its own
//! workers and from each peer -- and keeps an EWMA of the time to first byte.
//! That single number carries the CPU, the disk, the memory and the link in
//! front of a machine, which a worker count cannot: a four-core Celeron and a
//! four-core i7 stop looking identical the moment both have answered a request.
//! Placement is then the shorter of (queue length x measured response time), and
//! a peer on another release is never offered work it would serve stale.

use crate::hash;
use crate::placement;
use crate::sys;
use crate::web;
use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::os::raw::{c_int, c_void};
use std::os::unix::io::{FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_BEACON: u16 = 29700;
pub const DEFAULT_CONTROL: u16 = 29701;

const BEACON_EVERY: Duration = Duration::from_millis(500);
const PEER_TTL: Duration = Duration::from_secs(4);
const SYNC_EVERY: Duration = Duration::from_secs(2);
/// In `/var/lib`, not `/run`: the cluster may run unprivileged, and `/run` is
/// root-owned. `/var/lib/cartridge` is handed to the account it drops to.
const STATE_FILE: &str = "/var/lib/cartridge/cluster.json";
const RELEASES: &str = "/srv/cartridge/releases";
const LIVE: &str = "/srv/cartridge/live";
const LOG_FILE: &str = "/var/log/cartridge/import.log";
const KEEP_RELEASES: usize = 2;
const PIPE_CAP: usize = 65536;
const RELAY_IDLE: Duration = Duration::from_secs(300);
const MAX_CONNECTIONS: usize = 8192;
/// How long a fresh connection may hold its request back while the balancer
/// reads the URL out of it. Clients send at once; a bare pre-connect sends
/// nothing and is placed by load instead of pinning a relay.
const PEEK: Duration = Duration::from_millis(40);
const HEAD_CAP: usize = 2048;
/// Weight of each new response-time sample. Fast enough that a box which starts
/// to throttle loses its share inside a few connections, slow enough that one
/// slow request does not.
const TFA_ALPHA: f32 = 0.25;
/// A URL's owner keeps its traffic until it carries this many times the share
/// its budget entitles it to. Above that it is a hot spot, not a cache.
const OWNER_SLACK: f32 = 2.0;
/// Floor for a measured response time, in ms. Below this the number is loopback
/// and scheduler noise, not hardware: without it two boxes with identical cores
/// would take turns looking twice as fast as each other.
const TFA_FLOOR: f32 = 0.5;

extern "C" {
    fn epoll_create1(flags: c_int) -> c_int;
    fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut EpollEvent) -> c_int;
    fn epoll_wait(epfd: c_int, events: *mut EpollEvent, maxevents: c_int, timeout: c_int) -> c_int;
    fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    fn setsockopt(fd: c_int, level: c_int, name: c_int, value: *const c_void, len: u32) -> c_int;
    fn getsockopt(fd: c_int, level: c_int, name: c_int, value: *mut c_void, len: *mut u32) -> c_int;
    fn connect(fd: c_int, addr: *const c_void, len: u32) -> c_int;
    fn bind(fd: c_int, addr: *const c_void, len: u32) -> c_int;
    fn listen(fd: c_int, backlog: c_int) -> c_int;
    fn accept4(fd: c_int, addr: *mut c_void, len: *mut u32, flags: c_int) -> c_int;
    fn pipe2(fds: *mut c_int, flags: c_int) -> c_int;
    fn splice(
        fd_in: c_int,
        off_in: *mut i64,
        fd_out: c_int,
        off_out: *mut i64,
        len: usize,
        flags: u32,
    ) -> isize;
    fn shutdown(fd: c_int, how: c_int) -> c_int;
    fn close(fd: c_int) -> c_int;
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct EpollEvent {
    events: u32,
    data: u64,
}

/// sockaddr_in without the padding: the kernel reads and writes 16 bytes.
#[repr(C)]
struct SockAddrIn {
    family: u16,
    port: u16,
    addr: [u8; 4],
    zero: [u8; 8],
}

impl SockAddrIn {
    fn new(ip: Ipv4Addr, port: u16) -> SockAddrIn {
        SockAddrIn { family: AF_INET as u16, port: port.to_be(), addr: ip.octets(), zero: [0; 8] }
    }

    fn ip(&self) -> Ipv4Addr {
        Ipv4Addr::from(self.addr)
    }
}

const AF_INET: c_int = 2;
const SOCK_DGRAM: c_int = 2;
const SOCK_STREAM: c_int = 1;
const SOCK_NONBLOCK: c_int = 0x800;
const SOCK_CLOEXEC: c_int = 0x80000;
const O_NONBLOCK: c_int = 0x800;
const O_CLOEXEC: c_int = 0x80000;
const SOL_SOCKET: c_int = 1;
const SO_REUSEADDR: c_int = 2;
const SO_ERROR: c_int = 4;
const SO_REUSEPORT: c_int = 15;
const SHUT_WR: c_int = 1;
const EPOLLIN: u32 = 0x001;
const EPOLLOUT: u32 = 0x004;
const EPOLLRDHUP: u32 = 0x2000;
const EPOLL_CTL_ADD: c_int = 1;
const EPOLL_CTL_MOD: c_int = 3;
const EPOLL_CTL_DEL: c_int = 2;
const SPLICE_F_MOVE: u32 = 1;
const SPLICE_F_NONBLOCK: u32 = 2;
const EINTR: i32 = 4;
const EAGAIN: i32 = 11;
const EALREADY: i32 = 114;
const EINPROGRESS: i32 = 115;

fn log(tag: &str, message: &str) {
    let line = format!("{}  {:<5} {}\n", sys::fmt_time("%F %T"), tag, message);
    eprintln!("cartridge-cluster  {:<5} {}", tag, message);
    sys::append(LOG_FILE, &line);
}

// ------------------------------------------------------------------ peers

#[derive(Clone)]
struct Peer {
    id: String,
    ip: Ipv4Addr,
    control: u16,
    release: String,
    fp: String,
    workers: u32,
    /// Cores this peer may really use, in hundredths. A node that runs four
    /// workers under a half-core CPU quota is half a core of capacity, not four.
    capacity: u32,
    busy: u32,
    ports: Vec<u16>,
    seen: Instant,
}

#[derive(Clone, Default)]
struct ReleaseInfo {
    name: String,
    fp: String,
    files: usize,
    bytes: u64,
}

struct Shared {
    node: String,
    /// The fleet key, when one is provisioned. A peer that cannot sign its
    /// manifest is not trusted with this box's site content, and a beacon that
    /// is not signed is ignored: without this, anything on the switch could
    /// announce a newer release and every box would adopt its bytes.
    secret: Option<Vec<u8>>,
    workers: u32,
    capacity: u32,
    control: u16,
    ports: Mutex<Vec<u16>>,
    busy: AtomicU32,
    forwarded: AtomicU32,
    /// Connections this box has served since it started, its own and the ones
    /// peers handed it. The sum over the fleet is the whole job; the share is
    /// the split, which is the only way to see the balancer's decision.
    served: AtomicU64,
    peers: Mutex<Arc<Vec<Peer>>>,
    release: Mutex<ReleaseInfo>,
    /// This node's own key, the same one peers compute for it.
    key: u64,
    /// Measured time to first byte per node, keyed by that key.
    tfa: Mutex<HashMap<u64, f32>>,
}

impl Shared {
    fn snapshot(&self) -> Arc<Vec<Peer>> {
        self.peers.lock().unwrap().clone()
    }

    fn set_peers(&self, list: Vec<Peer>) {
        let mut slot = self.peers.lock().unwrap();
        *slot = Arc::new(list);
    }

    /// Measured response time for a target, in ms. The floor until this box has
    /// seen an answer from it, which makes the first decisions pure
    /// queueing decisions -- the right guess for a fleet nobody has measured.
    fn tfa(&self, key: u64) -> f32 {
        *self.tfa.lock().unwrap().get(&key).unwrap_or(&TFA_FLOOR)
    }

    fn note_tfa(&self, key: u64, ms: f32) {
        // A stalled connection must not poison a target for good, and a
        // sub-millisecond sample is loopback noise, not a slow machine.
        let ms = ms.clamp(TFA_FLOOR, 60_000.0);
        let mut map = self.tfa.lock().unwrap();
        let slot = map.entry(key).or_insert(ms);
        *slot = *slot * (1.0 - TFA_ALPHA) + ms * TFA_ALPHA;
    }
}

/// A short, stable name for this box. It comes from the same random 128-bit
/// install id the fleet layer uses, never from `/etc/machine-id`: cloned and
/// imaged PCs share that file, so two boxes would announce one name and each
/// would throw the other's beacon away as its own.
fn node_id() -> String {
    crate::ident::load_or_create().name
}

// ------------------------------------------------------------- release info

struct Entry {
    rel: String,
    size: u64,
    hash: String,
}

fn walk(root: &Path, rel: &Path, out: &mut Vec<Entry>) {
    let entries = match fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let child = rel.join(&name);
        let kind = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if kind.is_dir() {
            walk(&entry.path(), &child, out);
        } else if kind.is_file() {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let text = entry.path().to_string_lossy().into_owned();
            let h = hash::file(&text).unwrap_or_default();
            out.push(Entry { rel: child.to_string_lossy().into_owned(), size, hash: h });
        }
    }
}

/// Every file in a release, path-sorted, with its size and SHA-256.
fn manifest(dir: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    walk(dir, Path::new(""), &mut out);
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

/// Content identity: two nodes with the same release content have the same
/// fingerprint, whatever their folders are called and whenever they imported.
fn fingerprint(entries: &[Entry]) -> String {
    let mut text = String::new();
    for e in entries {
        text.push_str(&format!("{} {} {}\n", e.hash, e.size, e.rel));
    }
    hash::bytes(text.as_bytes())
}

fn live_dir() -> Option<(String, PathBuf)> {
    let path = fs::canonicalize(LIVE).ok()?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    Some((name, path))
}

/// Recompute the live release identity when the symlink points somewhere new.
/// A release never changes after it is published, so each file is hashed once.
fn refresh_release(shared: &Shared) {
    let current = live_dir();
    {
        let info = shared.release.lock().unwrap();
        match &current {
            Some((name, _)) if info.name == *name && !info.fp.is_empty() => return,
            None if info.name.is_empty() => return,
            _ => {}
        }
    }
    let info = match &current {
        Some((name, dir)) => {
            let entries = manifest(dir);
            ReleaseInfo {
                name: name.clone(),
                fp: fingerprint(&entries),
                files: entries.len(),
                bytes: entries.iter().map(|e| e.size).sum(),
            }
        }
        None => ReleaseInfo::default(),
    };
    *shared.release.lock().unwrap() = info;
}

// ---------------------------------------------------------------- discovery

fn udp_socket(port: u16) -> io::Result<UdpSocket> {
    unsafe {
        let fd = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let one: c_int = 1;
        setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one as *const _ as *const c_void, 4);
        setsockopt(fd, SOL_SOCKET, SO_REUSEPORT, &one as *const _ as *const c_void, 4);
        let addr = SockAddrIn::new(Ipv4Addr::UNSPECIFIED, port);
        if bind(fd, &addr as *const _ as *const c_void, std::mem::size_of::<SockAddrIn>() as u32) != 0
        {
            let err = io::Error::last_os_error();
            close(fd);
            return Err(err);
        }
        Ok(UdpSocket::from_raw_fd(fd))
    }
}

fn beacon_text(shared: &Shared) -> String {
    let info = shared.release.lock().unwrap().clone();
    let ports = shared.ports.lock().unwrap().clone();
    let ports_text = ports
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let body = format!(
        "CART1 {} {} {} {} {} {} {} {}",
        shared.node,
        if info.name.is_empty() { "-" } else { &info.name },
        if info.fp.is_empty() { "-" } else { &info.fp },
        shared.workers,
        shared.busy.load(Ordering::Relaxed),
        shared.control,
        ports_text,
        shared.capacity
    );
    match &shared.secret {
        Some(key) => format!("{} {}\n", body, crate::auth::tag(key, body.as_bytes())),
        None => format!("{}\n", body),
    }
}

fn parse_beacon(text: &str, from: Ipv4Addr, secret: Option<&Vec<u8>>) -> Option<Peer> {
    let text = text.trim();
    let fields: Vec<&str> = text.split(' ').collect();
    if fields.len() < 8 || fields[0] != "CART1" {
        return None;
    }
    // With a fleet key, an unsigned beacon is somebody else on the switch and
    // must not be able to place work, advertise a release or shift the map.
    if let Some(key) = secret {
        if fields.len() != 10 {
            return None;
        }
        let body = fields[..9].join(" ");
        if !crate::auth::verify(key, body.as_bytes(), fields[9]) {
            return None;
        }
    }
    let ports: Vec<u16> = fields[7]
        .split(',')
        .filter_map(|p| p.parse::<u16>().ok())
        .collect();
    let workers: u32 = fields[4].parse().ok()?;
    // Older nodes send no capacity field; their core count is the best guess.
    let capacity: u32 = fields
        .get(8)
        .and_then(|v| v.parse().ok())
        .unwrap_or(workers * 100);
    Some(Peer {
        id: fields[1].to_string(),
        ip: from,
        control: fields[6].parse().ok()?,
        release: if fields[2] == "-" { String::new() } else { fields[2].to_string() },
        fp: if fields[3] == "-" { String::new() } else { fields[3].to_string() },
        workers,
        capacity: capacity.max(1),
        busy: fields[5].parse().ok()?,
        ports,
        seen: Instant::now(),
    })
}

fn beacon_send(shared: Arc<Shared>, port: u16) {
    let sock = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            log("warn", &format!("cluster: cannot open the beacon socket: {}", e));
            return;
        }
    };
    let _ = sock.set_broadcast(true);
    let target = SocketAddrV4::new(Ipv4Addr::BROADCAST, port);
    // Before the link has an address every send fails; say that once, not twice
    // a second for the whole boot.
    let mut warned: Option<Instant> = None;
    let mut last_code = 0;
    loop {
        let text = beacon_text(&shared);
        match sock.send_to(text.as_bytes(), target) {
            Ok(_) => last_code = 0,
            Err(e) => {
                let code = e.raw_os_error().unwrap_or(0);
                let due = warned
                    .map(|when| when.elapsed() > Duration::from_secs(60))
                    .unwrap_or(true);
                if code != last_code || due {
                    log("warn", &format!("cluster: beacon failed: {}", e));
                    warned = Some(Instant::now());
                    last_code = code;
                }
            }
        }
        sys::sleep_ms(BEACON_EVERY.as_millis() as u64);
    }
}

fn beacon_recv(shared: Arc<Shared>, port: u16) {
    let sock = match udp_socket(port) {
        Ok(s) => s,
        Err(e) => {
            log("err", &format!("cluster: cannot listen for beacons on {}: {}", port, e));
            return;
        }
    };
    let _ = sock.set_read_timeout(Some(BEACON_EVERY));
    let mut buf = [0u8; 2048];
    let mut known: Vec<String> = Vec::new();
    loop {
        let mut list: Vec<Peer> = shared
            .snapshot()
            .iter()
            .filter(|p| p.seen.elapsed() < PEER_TTL)
            .cloned()
            .collect();
        list.retain(|p| p.id != shared.node);
        match sock.recv_from(&mut buf) {
            Ok((n, SocketAddr::V4(from))) => {
                if let Some(peer) = std::str::from_utf8(&buf[..n])
                    .ok()
                    .and_then(|t| parse_beacon(t, *from.ip(), shared.secret.as_ref()))
                {
                    if peer.id != shared.node {
                        if !known.contains(&peer.id) {
                            known.push(peer.id.clone());
                            log(
                                "ok",
                                &format!(
                                    "cluster: {} joined with {} worker(s), {} site(s)",
                                    peer.ip,
                                    peer.workers.max(1),
                                    peer.ports.len()
                                ),
                            );
                        }
                        list.retain(|p| p.id != peer.id);
                        list.push(peer);
                    }
                }
            }
            Ok(_) => {}
            Err(ref e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {}
            Err(_) => sys::sleep_ms(100),
        }
        known.retain(|id| list.iter().any(|p| &p.id == id));
        shared.set_peers(list);
    }
}

// --------------------------------------------------------------------- sync

/// The exact bytes of a manifest response. The signature is taken over this
/// one string on both ends, so the sender and the puller cannot drift apart.
fn manifest_body(fp: &str, entries: &[Entry]) -> String {
    let mut text = format!("OK {} {}\n", fp, entries.len());
    for e in entries {
        text.push_str(&format!("{}\t{}\t{}\n", e.size, e.hash, e.rel));
    }
    text
}

fn sync_log(tag: &str, message: &str) {
    log(tag, &format!("cluster: {}", message));
}

#[derive(Default)]
struct SyncReport {
    files: usize,
    copied: usize,
    reused: usize,
    bytes: u64,
    fingerprint: String,
}

/// A release folder is a timestamp this box made or synced: digits, dashes and
/// dots, never a path. A peer that names `/etc` or `../..` is a bug or an
/// attack, and joining it onto the releases root would delete or serve
/// something outside it.
fn release_name(name: &str) -> Option<&str> {
    if name.is_empty() || name.len() > 64 || name.starts_with('.') || name.contains("..") {
        return None;
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_digit() || b == b'-' || b == b'.')
    {
        return None;
    }
    Some(name)
}

fn release_dir(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name == "-" {
        return live_dir().map(|(_, d)| d);
    }
    let name = release_name(name)?;
    let path = Path::new(RELEASES).join(name);
    if path.is_dir() {
        Some(path)
    } else {
        None
    }
}

fn safe_join(dir: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.starts_with('/') || rel.contains('\0') || rel.len() > 1024 {
        return None;
    }
    for seg in rel.split('/') {
        if seg == ".." || seg == "." || seg.is_empty() {
            return None;
        }
    }
    Some(dir.join(rel))
}

/// One protocol line, with a hard cap. `BufRead::read_line` grows until it sees
/// a newline, so a peer that never sends one could ask this box for all its
/// memory; the cap is checked before anything is kept.
fn read_line_capped<R: BufRead>(reader: &mut R, cap: usize) -> Result<String, String> {
    let mut out: Vec<u8> = Vec::with_capacity(64);
    loop {
        let buf = reader.fill_buf().map_err(|e| e.to_string())?;
        if buf.is_empty() {
            if out.is_empty() {
                return Err("the peer closed the connection".into());
            }
            break;
        }
        if let Some(i) = buf.iter().position(|b| *b == b'\n') {
            if out.len() + i > cap {
                return Err(format!("a peer line exceeds {} bytes", cap));
            }
            out.extend_from_slice(&buf[..i]);
            reader.consume(i + 1);
            break;
        }
        if out.len() + buf.len() > cap {
            return Err(format!("a peer line exceeds {} bytes", cap));
        }
        let n = buf.len();
        out.extend_from_slice(buf);
        reader.consume(n);
    }
    while out.last() == Some(&b'\r') {
        out.pop();
    }
    Ok(String::from_utf8_lossy(&out).to_string())
}

/// Pull the peer's release: manifest first, then only the files this box does
/// not already hold. Identical files are hard-linked from the live release, so
/// a second sync of the same cartridge writes nothing.
fn pull(peer: &Peer, want: &str, mine_fp: &str) -> Result<SyncReport, String> {
    let want = release_name(want)
        .ok_or_else(|| format!("refusing release name {:?}", want))?
        .to_string();
    let addr = SocketAddrV4::new(peer.ip, peer.control);
    let stream = TcpStream::connect_timeout(&SocketAddr::V4(addr), Duration::from_secs(5))
        .map_err(|e| format!("cannot reach {}:{} ({})", peer.ip, peer.control, e))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    let mut out = stream.try_clone().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);

    out.write_all(format!("MANIFEST {}\n", want).as_bytes())
        .map_err(|e| e.to_string())?;
    let head = read_line_capped(&mut reader, 4096)?;
    let rest = head
        .strip_prefix("OK ")
        .ok_or_else(|| format!("peer refused the manifest: {}", head))?;
    let mut parts = rest.splitn(2, ' ');
    let fingerprint = parts.next().unwrap_or("").to_string();
    let count: usize = parts.next().unwrap_or("0").parse().unwrap_or(0);
    // Every number below comes off the wire. A manifest is a list of promises
    // and a hostile peer can promise a million files or a terabyte each, so the
    // same limits the importer applies are applied here, before allocating.
    if count > crate::guard::MAX_FILES {
        return Err(format!(
            "the peer offered {} files, over the {} limit",
            count,
            crate::guard::MAX_FILES
        ));
    }
    let mut signed = format!("{}\n", head);
    let mut entries: Vec<Entry> = Vec::with_capacity(count.min(4096));
    let mut total = 0u64;
    let mut tag: Option<String> = None;
    loop {
        let line = read_line_capped(&mut reader, 8192)?;
        if line == "END" {
            break;
        }
        if let Some(t) = line.strip_prefix("AUTH ") {
            tag = Some(t.trim().to_string());
            continue;
        }
        let mut f = line.splitn(3, '\t');
        let size: u64 = f.next().unwrap_or("0").parse().unwrap_or(0);
        let hash = f.next().unwrap_or("").to_string();
        let rel = f.next().unwrap_or("").to_string();
        if rel.is_empty() || !crate::chunks::valid_hash(&hash) {
            return Err("the peer sent a malformed manifest line".into());
        }
        if entries.len() >= crate::guard::MAX_FILES {
            return Err(format!("the peer sent more than {} files", crate::guard::MAX_FILES));
        }
        if size > crate::guard::MAX_FILE_BYTES {
            return Err(format!(
                "the peer offered a {} file",
                sys::human(size)
            ));
        }
        total = total.saturating_add(size);
        if total > crate::guard::MAX_TOTAL_BYTES {
            return Err(format!(
                "the peer offered {} in total",
                sys::human(total)
            ));
        }
        signed.push_str(&line);
        signed.push('\n');
        entries.push(Entry { rel, size, hash });
    }
    if entries.len() != count {
        return Err(format!(
            "the peer announced {} files but sent {}",
            count,
            entries.len()
        ));
    }
    // The manifest names the content this box is about to adopt. Signed, only a
    // holder of the fleet key can do that; unsigned, the newest epoch on the
    // switch is whatever any machine says it is.
    if let Some(key) = crate::auth::fleet_key() {
        let tag = tag.ok_or_else(|| "the peer did not sign its manifest".to_string())?;
        if !crate::auth::verify(&key, signed.as_bytes(), &tag) {
            return Err("the peer's manifest signature is wrong".into());
        }
    }
    if fingerprint == mine_fp {
        let _ = out.write_all(b"BYE\n");
        return Ok(SyncReport { files: entries.len(), fingerprint, ..Default::default() });
    }

    let target = Path::new(RELEASES).join(&want);
    if target.exists() {
        if live_dir().map(|(n, _)| n == want).unwrap_or(false) {
            return Err(format!("{} is already live here with different content", want));
        }
        let _ = fs::remove_dir_all(&target);
    }
    fs::create_dir_all(&target).map_err(|e| format!("{}: {}", target.display(), e))?;

    let mut report = SyncReport {
        files: entries.len(),
        fingerprint: fingerprint.clone(),
        ..Default::default()
    };
    let live = live_dir().map(|(_, d)| d);
    for entry in &entries {
        let dest = match safe_join(&target, &entry.rel) {
            Some(p) => p,
            None => {
                let _ = fs::remove_dir_all(&target);
                return Err(format!("refusing unsafe path {:?}", entry.rel));
            }
        };
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {}", parent.display(), e))?;
        }
        if let Some(live) = &live {
            let candidate = live.join(&entry.rel);
            let same = fs::metadata(&candidate).map(|m| m.len()).ok() == Some(entry.size)
                && hash::file(&candidate.to_string_lossy()).ok().as_deref()
                    == Some(entry.hash.as_str());
            if same && fs::hard_link(&candidate, &dest).is_ok() {
                report.reused += 1;
                continue;
            }
        }
        out.write_all(format!("GET {} {}\n", want, entry.rel).as_bytes())
            .map_err(|e| e.to_string())?;
        let line = read_line_capped(&mut reader, 4096)?;
        let n: usize = line
            .strip_prefix("SIZE ")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("peer refused {}: {}", entry.rel, line))?;
        // The manifest promised this file's length; a peer that announces a
        // different one is either broken or trying to make this box allocate.
        if n as u64 != entry.size {
            let _ = fs::remove_dir_all(&target);
            return Err(format!(
                "{} is {} bytes on the peer, {} in its manifest",
                entry.rel, n, entry.size
            ));
        }
        let mut body = vec![0u8; n];
        let mut got = 0;
        while got < n {
            match reader.read(&mut body[got..]) {
                Ok(0) => {
                    let _ = fs::remove_dir_all(&target);
                    return Err(format!("the peer stopped sending {}", entry.rel));
                }
                Ok(read) => got += read,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    let _ = fs::remove_dir_all(&target);
                    return Err(format!("{}: {}", entry.rel, e));
                }
            }
        }
        if hash::bytes(&body) != entry.hash {
            let _ = fs::remove_dir_all(&target);
            return Err(format!("checksum mismatch on {}", entry.rel));
        }
        if let Err(e) = fs::write(&dest, &body) {
            let _ = fs::remove_dir_all(&target);
            return Err(format!("{}: {}", dest.display(), e));
        }
        report.copied += 1;
        report.bytes += n as u64;
    }
    let _ = out.write_all(b"BYE\n");
    Ok(report)
}

fn prune(keep: &str) {
    let mut names: Vec<String> = match fs::read_dir(RELEASES) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("20") && n != keep)
            .collect(),
        Err(_) => return,
    };
    names.sort();
    names.reverse();
    for old in names.iter().skip(KEEP_RELEASES - 1) {
        let _ = fs::remove_dir_all(format!("{}/{}", RELEASES, old));
    }
}

/// Point `live` at the synced release and stage it into RAM, exactly as a
/// cartridge import does.
fn activate(name: &str) -> Result<(), String> {
    let name = release_name(name).ok_or_else(|| format!("refusing release name {:?}", name))?;
    let release = format!("{}/{}", RELEASES, name);
    if !Path::new(&release).is_dir() {
        return Err(format!("{} is missing", release));
    }
    let live_new = format!("{}.new", LIVE);
    let _ = fs::remove_file(&live_new);
    std::os::unix::fs::symlink(&release, &live_new).map_err(|e| e.to_string())?;
    let _ = fs::remove_file(LIVE);
    fs::rename(&live_new, LIVE).map_err(|e| e.to_string())?;
    prune(name);
    sys::sync();
    // Staging mounts a tmpfs, so it needs root. An unprivileged cluster leaves
    // a request flag and `cartridge-publish.path` runs publish as root.
    if crate::sys::is_root() {
        if let Err(e) = crate::publish::run() {
            log("warn", &format!("cluster: could not stage the release in RAM: {}", e));
        }
    } else {
        // `cartridge-publish.path` sees this flag and runs publish as root.
        let _ = fs::write("/var/lib/cartridge/restage", format!("{}\n", name));
    }
    Ok(())
}

fn sync_once(shared: &Arc<Shared>, tried: &mut HashMap<String, Instant>) {
    refresh_release(shared);
    let (mine_name, mine_fp) = {
        let info = shared.release.lock().unwrap();
        (info.name.clone(), info.fp.clone())
    };
    let mut peers: Vec<Peer> = shared
        .snapshot()
        .iter()
        .filter(|p| !p.release.is_empty() && !p.fp.is_empty())
        .cloned()
        .collect();
    if peers.is_empty() {
        return;
    }
    peers.sort_by(|a, b| b.release.cmp(&a.release));
    let newest = &peers[0];
    if newest.fp == mine_fp && !mine_fp.is_empty() {
        return;
    }
    if !(mine_name.is_empty() || newest.release.as_str() > mine_name.as_str()) {
        return;
    }
    let key = format!("{}@{}", newest.release, newest.ip);
    if let Some(when) = tried.get(&key) {
        if when.elapsed() < Duration::from_secs(30) {
            return;
        }
    }
    tried.insert(key, Instant::now());
    match pull(newest, &newest.release, &mine_fp) {
        Ok(report) => {
            if report.fingerprint == mine_fp && report.copied == 0 && report.reused == 0 {
                return;
            }
            match activate(&newest.release) {
                Ok(()) => {
                    refresh_release(shared);
                    sync_log(
                        "ok",
                        &format!(
                            "synced release {} from {} ({} file(s), {} copied {}{})",
                            newest.release,
                            newest.ip,
                            report.files,
                            report.copied,
                            sys::human(report.bytes),
                            if report.reused > 0 {
                                format!(", {} reused", report.reused)
                            } else {
                                String::new()
                            }
                        ),
                    );
                }
                Err(e) => sync_log(
                    "warn",
                    &format!("could not activate release {}: {}", newest.release, e),
                ),
            }
        }
        Err(e) => sync_log("warn", &format!("sync from {} failed: {}", newest.ip, e)),
    }
}

// ----------------------------------------------------------- control server

fn serve_peer(stream: TcpStream, shared: &Shared) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    let mut out = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    loop {
        let line = match read_line_capped(&mut reader, 8192) {
            Ok(l) => l,
            Err(_) => return Ok(()),
        };
        let mut it = line.splitn(3, ' ');
        match it.next().unwrap_or("") {
            "INFO" => {
                let info = shared.release.lock().unwrap().clone();
                write!(
                    out,
                    "OK {} {} {} {}\n",
                    shared.node,
                    if info.name.is_empty() { "-" } else { &info.name },
                    if info.fp.is_empty() { "-" } else { &info.fp },
                    shared.workers
                )?;
            }
            "MANIFEST" => {
                let want = it.next().unwrap_or("-");
                let dir = match release_dir(want) {
                    Some(d) => d,
                    None => {
                        write!(out, "ERR no such release\n")?;
                        continue;
                    }
                };
                let entries = manifest(&dir);
                let fp = fingerprint(&entries);
                let signed = manifest_body(&fp, &entries);
                out.write_all(signed.as_bytes())?;
                // The manifest is what names the content, so it is the part that
                // has to be signed; every file is verified by hash anyway.
                if let Some(key) = &shared.secret {
                    write!(out, "AUTH {}\n", crate::auth::tag(key, signed.as_bytes()))?;
                }
                write!(out, "END\n")?;
            }
            "GET" => {
                let want = it.next().unwrap_or("-");
                let rel = it.next().unwrap_or("");
                let path = release_dir(want).and_then(|d| safe_join(&d, rel));
                match path.map(fs::read) {
                    Some(Ok(bytes)) => {
                        write!(out, "SIZE {}\n", bytes.len())?;
                        out.write_all(&bytes)?;
                    }
                    Some(Err(_)) => write!(out, "ERR unreadable\n")?,
                    None => write!(out, "ERR no such file\n")?,
                }
            }
            // What this box is doing right now, for a peer, a script or
            // `cartridge status` on another machine. Same JSON the state file
            // holds, so there is one shape to read, not two.
            "STATS" => {
                let text = state_text(shared);
                write!(out, "{}", text)?;
            }
            "BYE" => return Ok(()),
            _ => {
                write!(out, "ERR unknown command\n")?;
            }
        }
    }
}

fn control_server(shared: Arc<Shared>, port: u16) {
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => {
            log("err", &format!("cluster: cannot listen for peers on {}: {}", port, e));
            return;
        }
    };
    // One thread per peer connection, but not one thread per packet: without a
    // cap a flood of connects is a memory exhaustion, one 8 MiB stack at a time.
    let live = Arc::new(AtomicU32::new(0));
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                if live.load(Ordering::Relaxed) >= 64 {
                    drop(s);
                    continue;
                }
                live.fetch_add(1, Ordering::Relaxed);
                let shared = shared.clone();
                let live = live.clone();
                std::thread::spawn(move || {
                    let _ = serve_peer(s, &shared);
                    live.fetch_sub(1, Ordering::Relaxed);
                });
            }
            Err(_) => sys::sleep_ms(50),
        }
    }
}

// -------------------------------------------------------------------- state

fn state_text(shared: &Shared) -> String {
    let info = shared.release.lock().unwrap().clone();
    let peers = shared.snapshot();
    let q = crate::json::quote;
    let mut text = format!(
        "{{\"node\":{},\"release\":{},\"fingerprint\":{},\"workers\":{},\"capacity\":{},\"tfa\":{},\"busy\":{},\"forwarded\":{},\"served\":{},\"peers\":[",
        q(&shared.node),
        q(&info.name),
        q(&info.fp),
        shared.workers,
        shared.capacity,
        (shared.tfa(shared.key) * 10.0).round() as u32,
        shared.busy.load(Ordering::Relaxed),
        shared.forwarded.load(Ordering::Relaxed),
        shared.served.load(Ordering::Relaxed)
    );
    for (i, p) in peers.iter().enumerate() {
        if i > 0 {
            text.push(',');
        }
        let same = !info.fp.is_empty() && p.fp == info.fp;
        text.push_str(&format!(
            "{{\"ip\":{},\"release\":{},\"fingerprint\":{},\"workers\":{},\"capacity\":{},\"tfa\":{},\"busy\":{},\"in_sync\":{}}}",
            q(&p.ip.to_string()),
            q(&p.release),
            q(&p.fp),
            p.workers,
            p.capacity,
            (shared.tfa(placement::node_key(&p.id)) * 10.0).round() as u32,
            p.busy,
            if same { "true" } else { "false" }
        ));
    }
    text.push_str("]}\n");
    text
}

fn write_state(shared: &Shared) {
    let text = state_text(shared);
    if let Some(parent) = Path::new(STATE_FILE).parent() {
        let _ = fs::create_dir_all(parent);
    }
    let temp = format!("{}.new", STATE_FILE);
    if fs::write(&temp, text).is_ok() {
        let _ = fs::rename(&temp, STATE_FILE);
    }
}

/// `cartridge status` and the console read this; nothing else has to know how
/// the cluster works.
pub fn state() -> Option<crate::json::Value> {
    let text = fs::read_to_string(STATE_FILE).ok()?;
    crate::json::parse(&text).ok()
}

// ------------------------------------------------------------------ balancer

struct Pipe {
    r: RawFd,
    w: RawFd,
    pending: usize,
}

impl Pipe {
    fn new() -> io::Result<Pipe> {
        let mut fds = [0 as c_int; 2];
        if unsafe { pipe2(fds.as_mut_ptr(), O_NONBLOCK | O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Pipe { r: fds[0], w: fds[1], pending: 0 })
    }

    fn close(&self) {
        unsafe {
            close(self.r);
            close(self.w);
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Target {
    Local,
    /// A peer, plus the key that identifies it. Two boxes can share an address
    /// -- containers on one host, anything behind a NAT, three test nodes on one
    /// laptop -- so measurements are keyed by node identity and never by IP,
    /// which would merge their queues and their speeds into one figure.
    Peer(Ipv4Addr, u64),
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    /// Accepted, reading the request line to learn the URL.
    Peek,
    /// Dialing the worker that should serve it.
    Connect,
    /// Both sockets open, bytes flowing.
    Flow,
}

struct Relay {
    client: RawFd,
    /// -1 until a target has been chosen.
    server: RawFd,
    target: Target,
    /// The port to dial on the chosen target.
    port: u16,
    /// This site's loopback port, for serving locally or falling back to it.
    local_port: u16,
    site: usize,
    src: Ipv4Addr,
    phase: Phase,
    /// The first bytes the client sent, held until the worker can take them:
    /// the URL is in here and it must arrive in order.
    head: Vec<u8>,
    head_pos: usize,
    /// Set when the request reaches the worker; one response-time sample per
    /// connection is taken from it.
    sent: Option<Instant>,
    timed: bool,
    /// No other connection was open on the target when this one was placed, so
    /// its response time is the machine's, not the queue's.
    solo: bool,
    counted: bool,
    tried_local: bool,
    accepted: Instant,
    up: Pipe,
    down: Pipe,
    /// The event mask currently registered for each side, so a steady relay
    /// never calls epoll_ctl just to say the same thing again.
    client_mask: u32,
    server_mask: u32,
    client_done: bool,
    server_done: bool,
    client_shut: bool,
    server_shut: bool,
    last: Instant,
}

impl Relay {
    /// Which measured response time this connection feeds: this box's own
    /// workers, or the peer it was handed to.
    fn key(&self, shared: &Shared) -> u64 {
        match self.target {
            Target::Local => shared.key,
            Target::Peer(_, key) => key,
        }
    }
}

/// What a balancer thread needs while it moves a relay along.
struct Ctx<'a> {
    shared: &'a Shared,
    rng: &'a mut Rng,
}

enum Flow {
    Again,
    End,
    Blocked,
    Fail,
}

enum Next {
    Keep,
    Close,
}

fn pull_bytes(src: RawFd, p: &mut Pipe) -> Flow {
    while p.pending < PIPE_CAP {
        let n = unsafe {
            splice(
                src,
                std::ptr::null_mut(),
                p.w,
                std::ptr::null_mut(),
                PIPE_CAP - p.pending,
                SPLICE_F_MOVE | SPLICE_F_NONBLOCK,
            )
        };
        if n > 0 {
            p.pending += n as usize;
            if (n as usize) < 4096 {
                // The socket is drained for now; do not pay a second splice to
                // be told it would block.
                return Flow::Blocked;
            }
        } else if n == 0 {
            return Flow::End;
        } else {
            match io::Error::last_os_error().raw_os_error() {
                Some(EINTR) => continue,
                Some(EAGAIN) => return Flow::Blocked,
                _ => return Flow::Fail,
            }
        }
    }
    Flow::Again
}

fn push_bytes(p: &mut Pipe, dst: RawFd) -> Flow {
    while p.pending > 0 {
        let n = unsafe {
            splice(
                p.r,
                std::ptr::null_mut(),
                dst,
                std::ptr::null_mut(),
                p.pending,
                SPLICE_F_MOVE | SPLICE_F_NONBLOCK,
            )
        };
        if n > 0 {
            p.pending -= n as usize;
        } else if n == 0 {
            return Flow::Blocked;
        } else {
            match io::Error::last_os_error().raw_os_error() {
                Some(EINTR) => continue,
                Some(EAGAIN) => return Flow::Blocked,
                _ => return Flow::Fail,
            }
        }
    }
    Flow::Again
}

fn connect_fd(ip: Ipv4Addr, port: u16) -> io::Result<RawFd> {
    unsafe {
        let fd = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let addr = SockAddrIn::new(ip, port);
        if connect(fd, &addr as *const _ as *const c_void, std::mem::size_of::<SockAddrIn>() as u32)
            != 0
        {
            let err = io::Error::last_os_error();
            let code = err.raw_os_error().unwrap_or(0);
            if code != EINPROGRESS && code != EALREADY {
                close(fd);
                return Err(err);
            }
        }
        Ok(fd)
    }
}

fn add_epoll(epfd: c_int, fd: RawFd, events: u32, token: u64) {
    let mut ev = EpollEvent { events, data: token };
    unsafe {
        epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &mut ev);
    }
}

fn mod_epoll(epfd: c_int, fd: RawFd, events: u32, token: u64) {
    let mut ev = EpollEvent { events, data: token };
    unsafe {
        epoll_ctl(epfd, EPOLL_CTL_MOD, fd, &mut ev);
    }
}

fn del_epoll(epfd: c_int, fd: RawFd) {
    let mut ev = EpollEvent { events: 0, data: 0 };
    unsafe {
        epoll_ctl(epfd, EPOLL_CTL_DEL, fd, &mut ev);
    }
}

/// 0 when the connect has completed, 1 while it is still in flight, 2 when the
/// peer refused it.
fn connect_state(fd: RawFd) -> i32 {
    let mut err: c_int = 0;
    let mut len: u32 = 4;
    unsafe {
        getsockopt(fd, SOL_SOCKET, SO_ERROR, &mut err as *mut _ as *mut c_void, &mut len);
    }
    if err == 0 {
        0
    } else if err == EINPROGRESS || err == EALREADY {
        1
    } else {
        2
    }
}

/// Relay tokens carry the relay id and which side fired: bit 0 marks a relay,
/// bit 1 marks its server socket.
fn token_of(id: u64, server: bool) -> u64 {
    (id << 2) | 1 | if server { 2 } else { 0 }
}

fn step(r: &mut Relay, epfd: c_int, id: u64, from_server: bool, ctx: &mut Ctx) -> Next {
    let token = token_of(id, false);
    let server_token = token_of(id, true);
    if r.phase == Phase::Peek {
        if from_server {
            return Next::Keep;
        }
        loop {
            let room = HEAD_CAP.saturating_sub(r.head.len());
            if room == 0 {
                break;
            }
            let mut chunk = [0u8; 512];
            let want = room.min(chunk.len());
            let n = sys::read_fd(r.client, chunk.as_mut_ptr(), want);
            if n > 0 {
                r.head.extend_from_slice(&chunk[..n as usize]);
            } else if n == 0 {
                // Hung up before asking for anything.
                return Next::Close;
            } else {
                match io::Error::last_os_error().raw_os_error() {
                    Some(EINTR) => continue,
                    Some(EAGAIN) => break,
                    _ => return Next::Close,
                }
            }
        }
        // Commit as soon as the URL is known. A client that only pre-connected
        // sends nothing; it is placed by load when the grace period runs out.
        if placement::request_path(&r.head).is_some()
            || r.head.len() >= HEAD_CAP
            || Instant::now() >= r.accepted + PEEK
        {
            return begin(r, epfd, id, ctx);
        }
        return Next::Keep;
    }

    if r.phase == Phase::Connect {
        if !from_server {
            // Wait for the connect to finish; the client side is parked.
            return Next::Keep;
        }
        match connect_state(r.server) {
            1 => return Next::Keep,
            2 => {
                return if r.tried_local {
                    Next::Close
                } else {
                    retarget_local(r, epfd, id, ctx)
                }
            }
            _ => {
                r.phase = Phase::Flow;
                r.sent = None;
                r.timed = false;
            }
        }
    }

    // Bytes the client sent before a target existed go first: the worker must
    // see the request in the order the client wrote it. After that the sockets
    // are joined and splice(2) never enters user space again.
    if r.head_pos < r.head.len() {
        let rest = &r.head[r.head_pos..];
        let n = sys::write_fd(r.server, rest.as_ptr(), rest.len());
        if n > 0 {
            r.head_pos += n as usize;
            if r.head_pos >= r.head.len() {
                r.sent = Some(Instant::now());
            }
        } else if n < 0 {
            match io::Error::last_os_error().raw_os_error() {
                Some(EINTR) | Some(EAGAIN) => {}
                _ => return Next::Close,
            }
        }
    }
    let head_done = r.head_pos >= r.head.len();

    if head_done && !r.client_done && r.up.pending < PIPE_CAP {
        match pull_bytes(r.client, &mut r.up) {
            Flow::End => r.client_done = true,
            Flow::Fail => return Next::Close,
            _ => {}
        }
    }
    if r.up.pending > 0 && matches!(push_bytes(&mut r.up, r.server), Flow::Fail) {
        return Next::Close;
    }
    if !r.server_done && r.down.pending < PIPE_CAP {
        let before = r.down.pending;
        match pull_bytes(r.server, &mut r.down) {
            Flow::End => r.server_done = true,
            Flow::Fail => return Next::Close,
            _ => {}
        }
        // One sample per connection: how long this target took to start
        // answering. This is the number that separates the hardware.
        if before == 0 && r.down.pending > 0 && !r.timed {
            r.timed = true;
            // Only an uncontended request measures the machine. A queued one
            // would report the queue as if it were slow hardware, and a node
            // that was once busy would then be avoided for good.
            if r.solo {
                if let Some(at) = r.sent {
                    let ms = at.elapsed().as_secs_f32() * 1000.0;
                    ctx.shared.note_tfa(r.key(ctx.shared), ms);
                }
            }
        }
    }
    if r.down.pending > 0 && matches!(push_bytes(&mut r.down, r.client), Flow::Fail) {
        return Next::Close;
    }
    if r.client_done && head_done && r.up.pending == 0 && !r.server_shut {
        unsafe {
            shutdown(r.server, SHUT_WR);
        }
        r.server_shut = true;
    }
    if r.server_done && r.down.pending == 0 && !r.client_shut {
        unsafe {
            shutdown(r.client, SHUT_WR);
        }
        r.client_shut = true;
    }
    r.last = Instant::now();
    if r.client_done && r.server_done && r.up.pending == 0 && r.down.pending == 0 {
        return Next::Close;
    }

    let client_ev = (if head_done && !r.client_done && r.up.pending < PIPE_CAP {
        EPOLLIN
    } else {
        0
    }) | (if r.down.pending > 0 { EPOLLOUT } else { 0 })
        | EPOLLRDHUP;
    let server_ev = (if head_done && !r.server_done && r.down.pending < PIPE_CAP {
        EPOLLIN
    } else {
        0
    }) | (if !head_done || r.up.pending > 0 { EPOLLOUT } else { 0 })
        | EPOLLRDHUP;
    if client_ev == EPOLLRDHUP && server_ev == EPOLLRDHUP {
        return Next::Close;
    }
    if r.client_mask != client_ev {
        mod_epoll(epfd, r.client, client_ev, token);
        r.client_mask = client_ev;
    }
    if r.server_mask != server_ev {
        mod_epoll(epfd, r.server, server_ev, server_token);
        r.server_mask = server_ev;
    }
    Next::Keep
}

/// Dial the chosen target, falling back to this box's own worker when a peer
/// that was alive a moment ago is not answering now.
fn begin(r: &mut Relay, epfd: c_int, id: u64, ctx: &mut Ctx) -> Next {
    let path = placement::request_path(&r.head);
    let (mut target, mut port) = choose(ctx.shared, r.src, r.site, ctx.rng, path.as_deref());
    if target == Target::Local {
        // choose() answers with the machine, not the port; a local connection
        // always goes to this site's own worker.
        port = r.local_port;
    }
    let mut fd = dial(target, port);
    if fd.is_err() && target != Target::Local {
        // Gone between its beacon and this connect (restart, cable, reboot):
        // keep the request instead of dropping the browser's connection.
        target = Target::Local;
        port = r.local_port;
        fd = dial(target, port);
    }
    let server = match fd {
        Ok(fd) => fd,
        Err(_) => return Next::Close,
    };
    r.solo = match target {
        Target::Local => ctx.shared.busy.load(Ordering::Relaxed) == 0,
        Target::Peer(_, key) => ctx
            .shared
            .snapshot()
            .iter()
            .any(|p| placement::node_key(&p.id) == key && p.busy == 0),
    };
    r.server = server;
    r.target = target;
    r.port = port;
    r.phase = Phase::Connect;
    r.tried_local = target == Target::Local;
    r.sent = None;
    r.timed = false;
    r.counted = true;
    match target {
        Target::Local => {
            ctx.shared.busy.fetch_add(1, Ordering::Relaxed);
            ctx.shared.served.fetch_add(1, Ordering::Relaxed);
        }
        Target::Peer(_, _) => {
            ctx.shared.forwarded.fetch_add(1, Ordering::Relaxed);
        }
    }
    add_epoll(epfd, server, EPOLLOUT | EPOLLRDHUP, token_of(id, true));
    r.server_mask = EPOLLOUT | EPOLLRDHUP;
    Next::Keep
}

fn dial(target: Target, port: u16) -> io::Result<RawFd> {
    match target {
        Target::Local => connect_fd(Ipv4Addr::LOCALHOST, port),
        Target::Peer(ip, _) => connect_fd(ip, port),
    }
}

fn retarget_local(r: &mut Relay, epfd: c_int, id: u64, ctx: &mut Ctx) -> Next {
    del_epoll(epfd, r.server);
    unsafe {
        close(r.server);
    }
    r.tried_local = true;
    match connect_fd(Ipv4Addr::LOCALHOST, r.local_port) {
        Ok(fd) => {
            r.server = fd;
            r.target = Target::Local;
            r.port = r.local_port;
            r.phase = Phase::Connect;
            r.sent = None;
            r.timed = false;
            r.solo = ctx.shared.busy.load(Ordering::Relaxed) == 0;
            if r.counted {
                ctx.shared.forwarded.fetch_sub(1, Ordering::Relaxed);
                ctx.shared.busy.fetch_add(1, Ordering::Relaxed);
                ctx.shared.served.fetch_add(1, Ordering::Relaxed);
            }
            r.client_mask = EPOLLRDHUP;
            r.server_mask = EPOLLOUT | EPOLLRDHUP;
            mod_epoll(epfd, r.client, EPOLLRDHUP, token_of(id, false));
            add_epoll(epfd, fd, EPOLLOUT | EPOLLRDHUP, token_of(id, true));
            Next::Keep
        }
        Err(_) => Next::Close,
    }
}

/// Expected wait for a new connection at a target, in milliseconds: the queue
/// that machine already holds, divided by how much of it runs in parallel, times
/// how long one request takes there. Both halves matter and neither is trusted
/// alone: the core budget is what the kernel enforces (a half-core container is
/// half a core), and the measured response time is what the hardware and the
/// wire actually do. A 30 % core and a 140 % core stop looking alike even before
/// either has answered anything.
fn cost(shared: &Shared, key: u64, busy: u32, capacity: u32) -> f32 {
    let cores = capacity.max(1) as f32 / 100.0;
    (busy as f32 + 1.0) * shared.tfa(key) / cores
}

/// Cores this process may actually use, in hundredths, from the cgroup CPU
/// quota. A node in a container limited to half a core but started with four
/// workers has half a core of capacity; without a quota this returns None and
/// the node advertises its worker count.
fn cgroup_capacity() -> Option<u32> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = cgroup.lines().find_map(|l| l.strip_prefix("0::"))?.trim().to_string();
    let text = std::fs::read_to_string(format!("/sys/fs/cgroup{}/cpu.max", path)).ok()?;
    let mut it = text.split_whitespace();
    let quota = it.next()?;
    let period: u64 = it.next()?.parse().ok()?;
    if quota == "max" || period == 0 {
        return None;
    }
    let quota: u64 = quota.parse().ok()?;
    Some(((quota.saturating_mul(100) / period).max(1)) as u32)
}

/// xorshift64, only used to break ties between equally quiet peers: a burst of
/// connections that all arrive before the next beacon should spread over the
/// whole cluster instead of piling onto whichever peer happens to be first.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(if seed == 0 { 0x9e37_79b9_7f4a_7c15 } else { seed })
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// Which machine should serve this connection.
///
/// Local until this box has one connection per core, because a loopback hop is
/// cheaper than any wire. After that the choice is the shortest expected wait,
/// from measured response times, and a known URL is pinned to its owner unless
/// that owner is badly overloaded.
fn choose(
    shared: &Shared,
    src: Ipv4Addr,
    index: usize,
    rng: &mut Rng,
    path: Option<&str>,
) -> (Target, u16) {
    let local_busy = shared.busy.load(Ordering::Relaxed);
    if local_busy * 100 < shared.capacity {
        return (Target::Local, 0);
    }
    let peers = shared.snapshot();
    if peers.iter().any(|p| p.ip == src) {
        // The request came from another node, so this is already the second
        // machine it has visited. Serve it here: forwarding a forwarded request
        // would build a chain and add a hop for every node on the switch.
        return (Target::Local, 0);
    }
    let our_fp = shared.release.lock().unwrap().fp.clone();
    let local_cost = cost(shared, shared.key, local_busy, shared.capacity);
    let mut best = (Target::Local, 0u16);
    let mut best_cost = local_cost;
    let mut targets: Vec<(Target, u16)> = vec![(Target::Local, 0)];
    let mut weights: Vec<(u32, u64)> = vec![(shared.capacity, shared.key)];
    for peer in peers.iter() {
        let port = match peer.ports.get(index) {
            Some(p) => *p,
            None => continue,
        };
        // A peer on another release would answer with stale bytes, and a peer
        // with no release has nothing to answer with. Neither may take work.
        if peer.fp.is_empty() || (!our_fp.is_empty() && peer.fp != our_fp) {
            continue;
        }
        let key = placement::node_key(&peer.id);
        let c = cost(shared, key, peer.busy, peer.capacity);
        if c < best_cost {
            best_cost = c;
            best = (Target::Peer(peer.ip, key), port);
        }
        targets.push((Target::Peer(peer.ip, key), port));
        weights.push((peer.capacity.max(1), key));
    }
    if targets.len() == 1 {
        return best;
    }

    // A URL has one owner, so its pages stay hot in one machine's cache instead
    // of every box in the rack holding a copy. Ownership is judged by the share
    // of the fleet's connections its own budget entitles it to -- not by how it
    // compares with the biggest box, which would hand every small box's URLs to
    // the biggest one the moment the cluster was quiet. Only a real hot spot,
    // twice its fair share, hands a URL back.
    if let Some(path) = path {
        if let Some(i) = placement::owner(path, &weights) {
            let (target, port) = targets[i];
            let busy = match target {
                Target::Local => local_busy,
                Target::Peer(_, key) => peers
                    .iter()
                    .find(|p| placement::node_key(&p.id) == key)
                    .map(|p| p.busy)
                    .unwrap_or(0),
            };
            let total_weight: u32 = weights.iter().map(|w| w.0).sum();
            let conns: u32 = local_busy + peers.iter().map(|p| p.busy).sum::<u32>();
            let fair = weights[i].0 as f32 / total_weight.max(1) as f32 * conns as f32;
            if busy as f32 <= OWNER_SLACK * fair.max(1.0) {
                return (target, port);
            }
        }
        return best;
    }

    // No URL yet (a bare pre-connect): draw weighted by how much work each
    // machine can finish per unit of its measured response time, so a burst
    // that arrives before anything has been timed does not stack on one box.
    let mut total: u64 = 0;
    let mut pick = best;
    for (i, (target, port)) in targets.iter().enumerate() {
        let c = match target {
            Target::Local => local_cost,
            Target::Peer(_, key) => peers
                .iter()
                .find(|p| placement::node_key(&p.id) == *key)
                .map(|p| cost(shared, *key, p.busy, p.capacity))
                .unwrap_or(f32::MAX),
        };
        let w = ((weights[i].0 as f64 * 1000.0) / c as f64).max(1.0) as u64;
        total += w;
        if rng.next() % total < w {
            pick = (*target, *port);
        }
    }
    pick
}

fn bind_public(port: u16) -> io::Result<RawFd> {
    unsafe {
        let fd = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let one: c_int = 1;
        setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one as *const _ as *const c_void, 4);
        setsockopt(fd, SOL_SOCKET, SO_REUSEPORT, &one as *const _ as *const c_void, 4);
        let addr = SockAddrIn::new(Ipv4Addr::UNSPECIFIED, port);
        if bind(fd, &addr as *const _ as *const c_void, std::mem::size_of::<SockAddrIn>() as u32) != 0
        {
            let err = io::Error::last_os_error();
            close(fd);
            return Err(err);
        }
        if listen(fd, 4096) != 0 {
            let err = io::Error::last_os_error();
            close(fd);
            return Err(err);
        }
        Ok(fd)
    }
}

/// Returns true when the relay was still peeking, so the balancer can keep its
/// count of connections waiting for a request line.
fn close_relay(id: u64, relays: &mut HashMap<u64, Relay>, shared: &Shared) -> bool {
    let r = match relays.remove(&id) {
        Some(r) => r,
        None => return false,
    };
    let peeking = r.phase == Phase::Peek;
    if r.counted {
        match r.target {
            Target::Local => {
                shared.busy.fetch_sub(1, Ordering::Relaxed);
            }
            Target::Peer(_, _) => {
                shared.forwarded.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
    r.up.close();
    r.down.close();
    unsafe {
        close(r.client);
        if r.server >= 0 {
            close(r.server);
        }
    }
    peeking
}

fn balancer(
    listeners: Vec<(RawFd, usize)>,
    ports: Vec<u16>,
    shared: Arc<Shared>,
    root: PathBuf,
    offset: u16,
) {
    let epfd = unsafe { epoll_create1(SOCK_CLOEXEC) };
    if epfd < 0 {
        log("err", "cluster: epoll_create1 failed");
        return;
    }
    for (i, (fd, _)) in listeners.iter().enumerate() {
        add_epoll(epfd, *fd, EPOLLIN, (i as u64) << 2);
    }
    let mut relays: HashMap<u64, Relay> = HashMap::new();
    let mut next_id: u64 = 1;
    let mut rng = Rng::new(
        (std::process::id() as u64).wrapping_mul(0x2545_f491_4f6c_dd1d)
            ^ sys::now_secs() as u64
            ^ (ports.first().copied().unwrap_or(1) as u64) << 32,
    );
    let mut events = vec![EpollEvent { events: 0, data: 0 }; 256];
    let mut next_sweep = Instant::now() + Duration::from_secs(2);
    // Connections still waiting for their first request line. While any exist
    // the loop wakes often enough to place them when their grace period ends,
    // and sleeps for half a second when the balancer is otherwise idle.
    let mut peeking: usize = 0;

    loop {
        let timeout = if peeking > 0 { 5 } else { 500 };
        let n = unsafe { epoll_wait(epfd, events.as_mut_ptr(), events.len() as c_int, timeout) };
        if n <= 0 {
            if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
        }
        for ev in events.iter().take(n.max(0) as usize) {
            let token = ev.data;
            if token & 1 == 0 {
                let index = (token >> 2) as usize;
                let (fd, site_index) = match listeners.get(index) {
                    Some(v) => *v,
                    None => continue,
                };
                for _ in 0..64 {
                    let mut addr = SockAddrIn::new(Ipv4Addr::UNSPECIFIED, 0);
                    let mut len = std::mem::size_of::<SockAddrIn>() as u32;
                    let client = unsafe {
                        accept4(
                            fd,
                            &mut addr as *mut _ as *mut c_void,
                            &mut len,
                            SOCK_NONBLOCK | SOCK_CLOEXEC,
                        )
                    };
                    if client < 0 {
                        break;
                    }
                    if relays.len() >= MAX_CONNECTIONS {
                        unsafe {
                            close(client);
                        }
                        break;
                    }
                    // Nothing is dialed yet: the request line is read first so
                    // the URL can be placed on the machine that owns it.
                    let (up, down) = match (Pipe::new(), Pipe::new()) {
                        (Ok(a), Ok(b)) => (a, b),
                        _ => {
                            unsafe {
                                close(client);
                            }
                            continue;
                        }
                    };
                    let id = next_id;
                    next_id += 1;
                    add_epoll(epfd, client, EPOLLIN | EPOLLRDHUP, token_of(id, false));
                    relays.insert(
                        id,
                        Relay {
                            client,
                            server: -1,
                            target: Target::Local,
                            port: ports[site_index],
                            local_port: ports[site_index],
                            site: site_index,
                            src: addr.ip(),
                            phase: Phase::Peek,
                            head: Vec::with_capacity(HEAD_CAP),
                            head_pos: 0,
                            sent: None,
                            timed: false,
                            solo: false,
                            counted: false,
                            tried_local: false,
                            accepted: Instant::now(),
                            up,
                            down,
                            client_mask: EPOLLIN | EPOLLRDHUP,
                            server_mask: 0,
                            client_done: false,
                            server_done: false,
                            client_shut: false,
                            server_shut: false,
                            last: Instant::now(),
                        },
                    );
                    peeking += 1;
                }
            } else {
                let id = token >> 2;
                let from_server = token & 2 != 0;
                let mut close_it = false;
                let was_peek = matches!(relays.get(&id).map(|r| r.phase), Some(Phase::Peek));
                let mut ctx = Ctx { shared: shared.as_ref(), rng: &mut rng };
                if let Some(relay) = relays.get_mut(&id) {
                    match step(relay, epfd, id, from_server, &mut ctx) {
                        Next::Keep => {}
                        Next::Close => close_it = true,
                    }
                }
                if close_it {
                    if close_relay(id, &mut relays, &shared) {
                        peeking = peeking.saturating_sub(1);
                    }
                } else if was_peek {
                    if relays.get(&id).map(|r| r.phase) != Some(Phase::Peek) {
                        peeking = peeking.saturating_sub(1);
                    }
                }
            }
        }

        // A connection that sent nothing cannot be placed by URL; once its
        // grace period is spent it is placed by load, like a pre-connect.
        if peeking > 0 {
            let now = Instant::now();
            let due: Vec<u64> = relays
                .iter()
                .filter(|(_, r)| r.phase == Phase::Peek && now >= r.accepted + PEEK)
                .map(|(id, _)| *id)
                .collect();
            for id in due {
                let mut close_it = false;
                let mut ctx = Ctx { shared: shared.as_ref(), rng: &mut rng };
                if let Some(relay) = relays.get_mut(&id) {
                    match step(relay, epfd, id, false, &mut ctx) {
                        Next::Keep => {}
                        Next::Close => close_it = true,
                    }
                }
                let gone = close_it && close_relay(id, &mut relays, &shared);
                let placed = !close_it
                    && relays.get(&id).map(|r| r.phase) != Some(Phase::Peek);
                if gone || placed {
                    peeking = peeking.saturating_sub(1);
                }
            }
        }

        if Instant::now() >= next_sweep {
            let now = Instant::now();
            let stale: Vec<u64> = relays
                .iter()
                .filter(|(_, r)| now.duration_since(r.last) > RELAY_IDLE)
                .map(|(id, _)| *id)
                .collect();
            for id in stale {
                if close_relay(id, &mut relays, &shared) {
                    peeking = peeking.saturating_sub(1);
                }
            }
            // A new cartridge changes the port set: let systemd start us again,
            // the same way a SIGHUP restarts the web tier.
            let now_ports: Vec<u16> = web::scan(&root)
                .iter()
                .map(|s| s.port.saturating_add(offset))
                .collect();
            if now_ports != ports {
                log("ok", "cluster: the site set changed; restarting to pick up the new ports");
                std::process::exit(0);
            }
            next_sweep = now + Duration::from_secs(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_name_cannot_be_a_path() {
        assert_eq!(release_name("20261122-102923"), Some("20261122-102923"));
        assert_eq!(release_name("1"), Some("1"));
        for bad in [
            "",
            "/etc",
            "../..",
            "a/b",
            ".hidden",
            "..",
            "x y",
            "2026..",
            "-x",
            "releases/../../etc",
        ] {
            assert_eq!(release_name(bad), None, "{:?} must be refused", bad);
        }
    }

    #[test]
    fn a_peer_line_is_bounded() {
        let mut input = std::io::Cursor::new(b"OK 1 2\n".to_vec());
        assert_eq!(read_line_capped(&mut input, 64).unwrap(), "OK 1 2");
        let mut crlf = std::io::Cursor::new(b"END\r\n".to_vec());
        assert_eq!(read_line_capped(&mut crlf, 64).unwrap(), "END");
        let mut long = std::io::Cursor::new(vec![b'x'; 4096]);
        assert!(
            read_line_capped(&mut long, 64).is_err(),
            "a line with no newline must not grow the buffer"
        );
        let mut huge = std::io::Cursor::new(format!("{}x\n", "y".repeat(9000)).into_bytes());
        assert!(read_line_capped(&mut huge, 8192).is_err());
    }

    #[test]
    fn the_signed_manifest_is_the_text_the_puller_checks() {
        // The sender signs `manifest_body`; the puller rebuilds the same bytes
        // from the lines it read. If those ever disagree, every signed sync
        // would fail, so the two forms are compared here.
        let entries = vec![
            Entry { rel: "index.html".into(), size: 12, hash: "ab".repeat(32) },
            Entry { rel: "8080/a.txt".into(), size: 3, hash: "cd".repeat(32) },
        ];
        let fp = fingerprint(&entries);
        let body = manifest_body(&fp, &entries);
        let key = b"fleet".to_vec();
        let tag = crate::auth::tag(&key, body.as_bytes());

        let mut seen = format!("OK {} {}\n", fp, entries.len());
        for e in &entries {
            seen.push_str(&format!("{}\t{}\t{}\n", e.size, e.hash, e.rel));
        }
        assert_eq!(seen, body, "the puller must rebuild exactly these bytes");
        assert!(crate::auth::verify(&key, seen.as_bytes(), &tag));
        assert!(!crate::auth::verify(b"other", seen.as_bytes(), &tag));
    }

    #[test]
    fn a_beacon_must_be_signed_once_a_key_exists() {
        let key = b"fleet".to_vec();
        let body = "CART1 node1 20261122-102923 abc 4 0 29701 8080 400";
        let signed = format!("{} {}", body, crate::auth::tag(&key, body.as_bytes()));
        let from = Ipv4Addr::new(10, 0, 0, 2);
        assert!(parse_beacon(&signed, from, Some(&key)).is_some());
        assert!(
            parse_beacon(body, from, Some(&key)).is_none(),
            "an unsigned beacon must not be believed"
        );
        let forged = format!("{} {}", body, "0".repeat(64));
        assert!(parse_beacon(&forged, from, Some(&key)).is_none());
        // With no key provisioned the fleet stays open, as documented.
        assert!(parse_beacon(body, from, None).is_some());
        assert!(parse_beacon(&signed, from, None).is_some());
    }
}

// ---------------------------------------------------------------------- run

pub fn run(
    root: PathBuf,
    control: u16,
    beacon: u16,
    workers: usize,
    name: Option<String>,
    offset: u16,
) -> Result<(), String> {
    let node = name.unwrap_or_else(node_id);
    let worker_count = if workers == 0 {
        web::cpu_count() as u32
    } else {
        workers.clamp(1, 128) as u32
    };
    // What peers should compare against: the CPU this box can really spend,
    // not the number of threads it happens to run.
    let capacity = cgroup_capacity()
        .unwrap_or(worker_count * 100)
        .min(worker_count * 100)
        .max(1);
    let sites = web::scan(&root);
    // The same offset the workers were started with, so a second copy on the
    // same machine can serve the same release on a different port set.
    let ports: Vec<u16> = sites.iter().map(|s| s.port.saturating_add(offset)).collect();

    let mut listeners: Vec<(RawFd, usize)> = Vec::new();
    for (i, port) in ports.iter().enumerate() {
        match bind_public(*port) {
            Ok(fd) => listeners.push((fd, i)),
            Err(e) => {
                log(
                    "err",
                    &format!(
                        "cluster: cannot listen on {}: {} - in cluster mode the web tier must be started with --bind 127.0.0.1",
                        port, e
                    ),
                );
                // Back off instead of spinning: a unit that restarts every
                // second hides the one line that explains the problem.
                sys::sleep_ms(10_000);
                return Err(format!("port {} is already in use", port));
            }
        }
    }

    let shared = Arc::new(Shared {
        node: node.clone(),
        workers: worker_count,
        capacity,
        control,
        ports: Mutex::new(ports.clone()),
        busy: AtomicU32::new(0),
        forwarded: AtomicU32::new(0),
        served: AtomicU64::new(0),
        peers: Mutex::new(Arc::new(Vec::new())),
        release: Mutex::new(ReleaseInfo::default()),
        secret: crate::auth::fleet_key(),
        key: placement::node_key(&node),
        tfa: Mutex::new(HashMap::new()),
    });
    refresh_release(&shared);

    let info = shared.release.lock().unwrap().clone();
    log(
        "ok",
        &format!(
            "cluster: node {} at {} with {} worker(s) on {}.{:02} core(s) of CPU budget, {} site(s) on port(s) {}",
            node,
            sys::lan_ip(),
            worker_count,
            capacity / 100,
            capacity % 100,
            ports.len(),
            ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(" ")
        ),
    );
    if !info.name.is_empty() {
        log(
            "info",
            &format!(
                "cluster: serving release {} ({} file(s), {})",
                info.name,
                info.files,
                sys::human(info.bytes)
            ),
        );
    }

    // Relay on every core, sharing the public ports with SO_REUSEPORT the same
    // way the web tier spreads its workers: the kernel balances the accepted
    // connections, so the relay is never one thread's bottleneck. Bound before
    // the privilege drop, because a site on port 80 can only be bound as root.
    let mut sets: Vec<Vec<(RawFd, usize)>> = vec![listeners];
    for _ in 1..worker_count {
        let mut set = Vec::new();
        for (i, port) in ports.iter().enumerate() {
            if let Ok(fd) = bind_public(*port) {
                set.push((fd, i));
            }
        }
        if set.len() == ports.len() {
            sets.push(set);
        } else {
            for (fd, _) in set {
                unsafe {
                    close(fd);
                }
            }
        }
    }

    // Give up root now that the ports are bound. It keeps the store, the state
    // file and the release directories, so those have to belong to the account
    // it drops to; if they do not, running as root is the only way to keep
    // syncing and the warning says exactly what to chown.
    let account = std::env::var("CARTRIDGE_USER").unwrap_or_else(|_| "cartridge".to_string());
    let _ = fs::create_dir_all("/var/lib/cartridge");
    match crate::sys::user_id(&account) {
        Some((uid, gid)) if crate::sys::owner(RELEASES).map(|(u, _)| u) == Some(uid) => {
            if !crate::sys::drop_to(uid, gid) {
                return Err("cluster: could not drop root privileges".into());
            }
            log("ok", &format!("cluster: running as {}, not root", account));
        }
        Some(_) => log(
            "warn",
            &format!(
                "cluster: {} is not owned by {} - running as root; chown -R {} {} to drop",
                RELEASES, account, account, RELEASES
            ),
        ),
        None if crate::sys::is_root() => log(
            "warn",
            &format!(
                "cluster: no {} account and this runs as root; create it and chown {} to it",
                account, RELEASES
            ),
        ),
        None => {}
    }

    {
        let s = shared.clone();
        std::thread::spawn(move || beacon_send(s, beacon));
    }
    {
        let s = shared.clone();
        std::thread::spawn(move || beacon_recv(s, beacon));
    }
    {
        let s = shared.clone();
        std::thread::spawn(move || control_server(s, control));
    }
    {
        let s = shared.clone();
        std::thread::spawn(move || {
            let mut tried: HashMap<String, Instant> = HashMap::new();
            loop {
                sync_once(&s, &mut tried);
                write_state(&s);
                sys::sleep_ms(SYNC_EVERY.as_millis() as u64);
            }
        });
    }
    let mut iter = sets.into_iter();
    let first = iter.next().unwrap_or_default();
    for set in iter {
        let shared = shared.clone();
        let root = root.clone();
        let ports = ports.clone();
        std::thread::spawn(move || balancer(set, ports, shared, root, offset));
    }
    balancer(first, ports, shared, root, offset);
    Ok(())
}
