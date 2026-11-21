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
//! quieter node get picked, by lowest connections per core. One box with no
//! peers therefore pays one loopback hop and nothing else.

use crate::hash;
use crate::sys;
use crate::web;
use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::os::raw::{c_int, c_void};
use std::os::unix::io::{FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_BEACON: u16 = 29700;
pub const DEFAULT_CONTROL: u16 = 29701;

const BEACON_EVERY: Duration = Duration::from_millis(500);
const PEER_TTL: Duration = Duration::from_secs(4);
const SYNC_EVERY: Duration = Duration::from_secs(2);
const STATE_FILE: &str = "/run/cartridge-cluster.json";
const RELEASES: &str = "/srv/cartridge/releases";
const LIVE: &str = "/srv/cartridge/live";
const LOG_FILE: &str = "/var/log/cartridge/import.log";
const KEEP_RELEASES: usize = 2;
const PIPE_CAP: usize = 65536;
const RELAY_IDLE: Duration = Duration::from_secs(300);
const MAX_CONNECTIONS: usize = 8192;

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
    workers: u32,
    capacity: u32,
    control: u16,
    ports: Mutex<Vec<u16>>,
    busy: AtomicU32,
    forwarded: AtomicU32,
    peers: Mutex<Arc<Vec<Peer>>>,
    release: Mutex<ReleaseInfo>,
}

impl Shared {
    fn snapshot(&self) -> Arc<Vec<Peer>> {
        self.peers.lock().unwrap().clone()
    }

    fn set_peers(&self, list: Vec<Peer>) {
        let mut slot = self.peers.lock().unwrap();
        *slot = Arc::new(list);
    }
}

/// A short, stable name for this box: the machine id when the system has one,
/// the hostname otherwise, hashed so beacons stay uniform.
fn node_id() -> String {
    let mut text = fs::read_to_string("/etc/machine-id").unwrap_or_default();
    if text.trim().is_empty() {
        text = sys::read_trim("/proc/sys/kernel/hostname").unwrap_or_default();
    }
    if text.trim().is_empty() {
        text = format!("{}-{}", sys::lan_ip(), std::process::id());
    }
    hash::bytes(text.trim().as_bytes())[..8].to_string()
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
    format!(
        "CART1 {} {} {} {} {} {} {} {}\n",
        shared.node,
        if info.name.is_empty() { "-" } else { &info.name },
        if info.fp.is_empty() { "-" } else { &info.fp },
        shared.workers,
        shared.busy.load(Ordering::Relaxed),
        shared.control,
        ports_text,
        shared.capacity
    )
}

fn parse_beacon(text: &str, from: Ipv4Addr) -> Option<Peer> {
    let fields: Vec<&str> = text.trim().split(' ').collect();
    if fields.len() < 8 || fields[0] != "CART1" {
        return None;
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
                    .and_then(|t| parse_beacon(t, *from.ip()))
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

fn release_dir(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name == "-" {
        return live_dir().map(|(_, d)| d);
    }
    if name.contains('/') || name.contains("..") {
        return None;
    }
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

/// Pull the peer's release: manifest first, then only the files this box does
/// not already hold. Identical files are hard-linked from the live release, so
/// a second sync of the same cartridge writes nothing.
fn pull(peer: &Peer, want: &str, mine_fp: &str) -> Result<SyncReport, String> {
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
    let mut head = String::new();
    reader.read_line(&mut head).map_err(|e| e.to_string())?;
    let head = head.trim_end().to_string();
    let rest = head
        .strip_prefix("OK ")
        .ok_or_else(|| format!("peer refused the manifest: {}", head))?;
    let mut parts = rest.splitn(2, ' ');
    let fingerprint = parts.next().unwrap_or("").to_string();
    let count: usize = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let mut entries: Vec<Entry> = Vec::with_capacity(count);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            return Err("the peer closed the manifest early".into());
        }
        let line = line.trim_end().to_string();
        if line == "END" {
            break;
        }
        let mut f = line.splitn(3, '\t');
        let size: u64 = f.next().unwrap_or("0").parse().unwrap_or(0);
        let hash = f.next().unwrap_or("").to_string();
        let rel = f.next().unwrap_or("").to_string();
        if rel.is_empty() || hash.len() != 64 {
            return Err("the peer sent a malformed manifest line".into());
        }
        entries.push(Entry { rel, size, hash });
    }
    if fingerprint == mine_fp {
        let _ = out.write_all(b"BYE\n");
        return Ok(SyncReport { files: entries.len(), fingerprint, ..Default::default() });
    }

    let target = Path::new(RELEASES).join(want);
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
        let mut line = String::new();
        reader.read_line(&mut line).map_err(|e| e.to_string())?;
        let n: usize = line
            .trim_end()
            .strip_prefix("SIZE ")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("peer refused {}: {}", entry.rel, line.trim_end()))?;
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
    crate::publish::run().map(|_| ())
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
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let line = line.trim_end().to_string();
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
                write!(out, "OK {} {}\n", fingerprint(&entries), entries.len())?;
                for e in &entries {
                    write!(out, "{}\t{}\t{}\n", e.size, e.hash, e.rel)?;
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
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let shared = shared.clone();
                std::thread::spawn(move || {
                    let _ = serve_peer(s, &shared);
                });
            }
            Err(_) => sys::sleep_ms(50),
        }
    }
}

// -------------------------------------------------------------------- state

fn write_state(shared: &Shared) {
    let info = shared.release.lock().unwrap().clone();
    let peers = shared.snapshot();
    let q = crate::json::quote;
    let mut text = format!(
        "{{\"node\":{},\"release\":{},\"fingerprint\":{},\"workers\":{},\"capacity\":{},\"busy\":{},\"forwarded\":{},\"peers\":[",
        q(&shared.node),
        q(&info.name),
        q(&info.fp),
        shared.workers,
        shared.capacity,
        shared.busy.load(Ordering::Relaxed),
        shared.forwarded.load(Ordering::Relaxed)
    );
    for (i, p) in peers.iter().enumerate() {
        if i > 0 {
            text.push(',');
        }
        let same = !info.fp.is_empty() && p.fp == info.fp;
        text.push_str(&format!(
            "{{\"ip\":{},\"release\":{},\"fingerprint\":{},\"workers\":{},\"capacity\":{},\"busy\":{},\"in_sync\":{}}}",
            q(&p.ip.to_string()),
            q(&p.release),
            q(&p.fp),
            p.workers,
            p.capacity,
            p.busy,
            if same { "true" } else { "false" }
        ));
    }
    text.push_str("]}\n");
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
    Peer(Ipv4Addr),
}

struct Relay {
    client: RawFd,
    server: RawFd,
    target: Target,
    port: u16,
    connecting: bool,
    tried_local: bool,
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

enum Flow {
    Again,
    End,
    Blocked,
    Fail,
}

enum Next {
    Keep,
    RetryLocal,
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

fn step(r: &mut Relay, epfd: c_int, id: u64, from_server: bool) -> Next {
    let token = token_of(id, false);
    let server_token = token_of(id, true);
    if r.connecting {
        if !from_server {
            // Wait for the connect to finish; the client side is parked.
            return Next::Keep;
        }
        match connect_state(r.server) {
            1 => return Next::Keep,
            2 => return if r.tried_local { Next::Close } else { Next::RetryLocal },
            _ => r.connecting = false,
        }
    }
    if !r.client_done && r.up.pending < PIPE_CAP {
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
        match pull_bytes(r.server, &mut r.down) {
            Flow::End => r.server_done = true,
            Flow::Fail => return Next::Close,
            _ => {}
        }
    }
    if r.down.pending > 0 && matches!(push_bytes(&mut r.down, r.client), Flow::Fail) {
        return Next::Close;
    }
    if r.client_done && r.up.pending == 0 && !r.server_shut {
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

    let client_ev = (if !r.client_done && r.up.pending < PIPE_CAP { EPOLLIN } else { 0 })
        | (if r.down.pending > 0 { EPOLLOUT } else { 0 })
        | EPOLLRDHUP;
    let server_ev = (if !r.server_done && r.down.pending < PIPE_CAP { EPOLLIN } else { 0 })
        | (if r.up.pending > 0 { EPOLLOUT } else { 0 })
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

fn retarget_local(r: &mut Relay, epfd: c_int, id: u64, shared: &Shared) -> bool {
    del_epoll(epfd, r.server);
    unsafe {
        close(r.server);
    }
    r.tried_local = true;
    match connect_fd(Ipv4Addr::LOCALHOST, r.port) {
        Ok(fd) => {
            r.server = fd;
            r.target = Target::Local;
            r.connecting = true;
            r.client_shut = false;
            r.server_shut = false;
            shared.forwarded.fetch_sub(1, Ordering::Relaxed);
            shared.busy.fetch_add(1, Ordering::Relaxed);
            r.client_mask = EPOLLRDHUP;
            r.server_mask = EPOLLOUT | EPOLLRDHUP;
            mod_epoll(epfd, r.client, EPOLLRDHUP, token_of(id, false));
            add_epoll(epfd, fd, EPOLLOUT | EPOLLRDHUP, token_of(id, true));
            true
        }
        Err(_) => false,
    }
}

fn score(busy: u32, capacity: u32) -> f32 {
    (busy * 100 + 100) as f32 / capacity.max(1) as f32
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

/// Which machine should serve this connection. Local until this box has one
/// connection per core, then the peer with the fewest connections per core, but
/// only when it is clearly quieter (20 %), so the cluster never bounces a
/// request for a marginal gain.
fn choose(shared: &Shared, src: Ipv4Addr, index: usize, rng: &mut Rng) -> (Target, u16) {
    let local_busy = shared.busy.load(Ordering::Relaxed);
    if local_busy * 100 < shared.capacity {
        return (Target::Local, 0);
    }
    let local = score(local_busy, shared.capacity);
    let peers = shared.snapshot();
    if peers.iter().any(|p| p.ip == src) {
        // The request came from another node, so this is already the second
        // machine it has visited. Serve it here: forwarding a forwarded request
        // would build a chain and add a hop for every node on the switch.
        return (Target::Local, 0);
    }
    // Every peer that is clearly quieter than this box is a candidate, weighted
    // by the CPU it can really spend. Sending a whole burst to the single
    // quietest peer would decide the split before any beacon has reported the
    // load, so a fast box would take every connection in the burst.
    let mut total: u64 = 0;
    let mut picks: Vec<(u64, Ipv4Addr, u16)> = Vec::new();
    for peer in peers.iter() {
        let port = match peer.ports.get(index) {
            Some(p) => *p,
            None => continue,
        };
        if !(score(peer.busy, peer.capacity) * 1.25 < local) {
            continue;
        }
        total += peer.capacity.max(1) as u64;
        picks.push((total, peer.ip, port));
    }
    if picks.is_empty() {
        return (Target::Local, 0);
    }
    let ticket = rng.next() % total;
    for (reaches, ip, port) in picks {
        if ticket < reaches {
            return (Target::Peer(ip), port);
        }
    }
    (Target::Local, 0)
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

fn close_relay(id: u64, relays: &mut HashMap<u64, Relay>, shared: &Shared) {
    if let Some(r) = relays.remove(&id) {
        match r.target {
            Target::Local => {
                shared.busy.fetch_sub(1, Ordering::Relaxed);
            }
            Target::Peer(_) => {
                shared.forwarded.fetch_sub(1, Ordering::Relaxed);
            }
        }
        r.up.close();
        r.down.close();
        unsafe {
            close(r.client);
            close(r.server);
        }
    }
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

    loop {
        let n = unsafe { epoll_wait(epfd, events.as_mut_ptr(), events.len() as c_int, 500) };
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
                    let (mut target, port) = choose(&shared, addr.ip(), site_index, &mut rng);
                    let server = match target {
                        Target::Local => connect_fd(Ipv4Addr::LOCALHOST, ports[site_index]),
                        Target::Peer(ip) => match connect_fd(ip, port) {
                            Ok(fd) => Ok(fd),
                            // The peer went away between its beacon and this
                            // connect (restart, cable, reboot): keep the request
                            // instead of dropping the browser's connection.
                            Err(_) => {
                                target = Target::Local;
                                connect_fd(Ipv4Addr::LOCALHOST, ports[site_index])
                            }
                        },
                    };
                    let server = match server {
                        Ok(fd) => fd,
                        Err(_) => {
                            unsafe {
                                close(client);
                            }
                            continue;
                        }
                    };
                    let (up, down) = match (Pipe::new(), Pipe::new()) {
                        (Ok(a), Ok(b)) => (a, b),
                        _ => {
                            unsafe {
                                close(client);
                                close(server);
                            }
                            continue;
                        }
                    };
                    let id = next_id;
                    next_id += 1;
                    add_epoll(epfd, client, EPOLLRDHUP, token_of(id, false));
                    add_epoll(epfd, server, EPOLLOUT | EPOLLRDHUP, token_of(id, true));
                    match target {
                        Target::Local => {
                            shared.busy.fetch_add(1, Ordering::Relaxed);
                        }
                        Target::Peer(_) => {
                            shared.forwarded.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    relays.insert(
                        id,
                        Relay {
                            client,
                            server,
                            target,
                            port: ports[site_index],
                            connecting: true,
                            tried_local: matches!(target, Target::Local),
                            up,
                            down,
                            client_mask: EPOLLRDHUP,
                            server_mask: EPOLLOUT | EPOLLRDHUP,
                            client_done: false,
                            server_done: false,
                            client_shut: false,
                            server_shut: false,
                            last: Instant::now(),
                        },
                    );
                }
            } else {
                let id = token >> 2;
                let from_server = token & 2 != 0;
                let mut close_it = false;
                if let Some(relay) = relays.get_mut(&id) {
                    match step(relay, epfd, id, from_server) {
                        Next::Keep => {}
                        Next::RetryLocal => {
                            if !retarget_local(relay, epfd, id, &shared) {
                                close_it = true;
                            }
                        }
                        Next::Close => close_it = true,
                    }
                }
                if close_it {
                    close_relay(id, &mut relays, &shared);
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
                close_relay(id, &mut relays, &shared);
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
        peers: Mutex::new(Arc::new(Vec::new())),
        release: Mutex::new(ReleaseInfo::default()),
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

    // Relay on every core, sharing the public ports with SO_REUSEPORT the same
    // way the web tier spreads its workers: the kernel balances the accepted
    // connections, so the relay is never one thread's bottleneck.
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
