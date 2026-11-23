//! The web tier: an edge-triggered epoll server, one worker per CPU thread,
//! sharing the ports with SO_REUSEPORT. Small files are kept as complete
//! responses in RAM and leave the machine with one write(2); larger ones stream
//! with sendfile(2). Headers are built once per file, the Date line is patched
//! in place once a second, and path lookups are answered from RAM for a second
//! at a time, so a hot request costs no syscalls except read and write.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::net::Ipv4Addr;
use std::os::raw::{c_int, c_long, c_void};
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_CONNECTIONS: usize = 4096;
const READ_BUF: usize = 16384;
const HEADER_CAP: usize = 16 * 1024;
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REQUESTS: u32 = 10_000;
/// Bodies at or below this size are copied into RAM and answered with a single
/// write; bigger files stream from the page cache with sendfile.
const RAM_LIMIT: u64 = 256 * 1024;
/// How long a cached path is trusted without a fresh stat.
const REVALIDATE: Duration = Duration::from_millis(1000);
/// "Sun, 06 Nov 1994 08:49:37 GMT"
const DATE_LEN: usize = 29;
/// Stop a pathological site from growing one worker's cache without bound.
const CACHE_ENTRIES: usize = 8192;
/// ...and stop it by bytes as well as by count. 8192 entries of a 256 KiB body
/// would be 2 GB in one worker, on a box that already holds the whole site in
/// tmpfs: two copies of every small file, per core, is how a quiet server ends
/// up swapping while it is supposed to be answering requests.
const CACHE_BYTES: usize = 64 * 1024 * 1024;
/// Connections one address may hold in one worker. A single host opening
/// thousands of sockets must not be able to take the whole worker's budget.
const PER_IP_LIMIT: u32 = 256;
/// Where a release is staged; `publish` symlinks the files that did not fit in
/// RAM back to this tree.
const LIVE: &str = "/srv/cartridge/live";

extern "C" {
    fn epoll_create1(flags: c_int) -> c_int;
    fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut EpollEvent) -> c_int;
    fn epoll_wait(epfd: c_int, events: *mut EpollEvent, maxevents: c_int, timeout: c_int) -> c_int;
    fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    fn setsockopt(fd: c_int, level: c_int, name: c_int, value: *const c_void, len: u32) -> c_int;
    fn bind(fd: c_int, addr: *const c_void, len: u32) -> c_int;
    fn listen(fd: c_int, backlog: c_int) -> c_int;
    fn accept4(fd: c_int, addr: *mut c_void, len: *mut u32, flags: c_int) -> c_int;
    fn sendfile(out: c_int, input: c_int, offset: *mut i64, count: usize) -> isize;
    fn close(fd: c_int) -> c_int;
    fn setuid(uid: u32) -> c_int;
    fn setgid(gid: u32) -> c_int;
    fn signal(sig: c_int, handler: usize) -> usize;
    fn geteuid() -> u32;
    fn sched_setaffinity(pid: c_int, len: usize, mask: *const u8) -> c_int;
}

// struct epoll_event is packed on x86_64: 4 bytes of events, 8 of data.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct EpollEvent {
    events: u32,
    data: u64,
}

const EPOLLIN: u32 = 0x001;
const EPOLLOUT: u32 = 0x004;
const EPOLLERR: u32 = 0x008;
const EPOLLHUP: u32 = 0x010;
const EPOLLRDHUP: u32 = 0x2000;
const EPOLLET: u32 = 1 << 31;
const EPOLL_CTL_ADD: c_int = 1;
const EPOLL_CTL_MOD: c_int = 3;
const EPOLL_CTL_DEL: c_int = 2;
const SOCK_NONBLOCK: c_int = 0x800;
const SOCK_CLOEXEC: c_int = 0x80000;
const SOL_SOCKET: c_int = 1;
const SO_REUSEADDR: c_int = 2;
const SO_REUSEPORT: c_int = 15;
const SO_SNDBUF: c_int = 7;
const IPPROTO_TCP: c_int = 6;
const TCP_NODELAY: c_int = 1;
const TCP_DEFER_ACCEPT: c_int = 9;
const TCP_FASTOPEN: c_int = 23;

pub struct Site {
    pub port: u16,
    pub root: PathBuf,
}

pub fn scan(root: &Path) -> Vec<Site> {
    let mut sites = Vec::new();
    let port80 = root.join("80");
    if port80.is_dir() {
        sites.push(Site { port: 80, root: port80 });
    } else if root.join("index.html").is_file() {
        sites.push(Site { port: 80, root: root.to_path_buf() });
    }
    let mut dirs: Vec<(u16, PathBuf)> = Vec::new();
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Ok(number) = name.parse::<u32>() {
                if (1024..=65535).contains(&number) {
                    dirs.push((number as u16, path));
                }
            }
        }
    }
    dirs.sort_by_key(|(p, _)| *p);
    for (port, root) in dirs {
        sites.push(Site { port, root });
    }
    sites
}

fn mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "application/javascript",
        "json" => "application/json",
        "txt" | "md" => "text/plain",
        "xml" => "text/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

fn wants_charset(mime: &str) -> bool {
    mime.starts_with("text/") || mime == "application/javascript" || mime == "application/xml"
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        206 => "Partial Content",
        301 => "Moved Permanently",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        414 => "URI Too Long",
        416 => "Range Not Satisfiable",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

fn error_body(code: u16) -> Vec<u8> {
    let title = reason(code);
    format!(
        "<html>\r\n<head><title>{} {}</title></head>\r\n<body>\r\n<center><h1>{} {}</h1></center>\r\n<hr><center>cartridge</center>\r\n</body>\r\n</html>\r\n",
        code, title, code, title
    )
    .into_bytes()
}

fn http_date(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    const WD: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        WD[days.rem_euclid(7) as usize],
        day,
        MON[(month - 1) as usize],
        year,
        hour,
        minute,
        second
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn decode_path(target: &str) -> Option<String> {
    let raw = target.split('?').next().unwrap_or("/");
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

// ------------------------------------------------------------------- the clock

/// One Date string per second, shared by every response a worker sends. Each
/// cached response keeps a hole where its Date line belongs; `stamp` remembers
/// which second is already written into that hole.
struct Clock {
    sec: i64,
    text: [u8; DATE_LEN],
}

impl Clock {
    fn new() -> Clock {
        let mut text = [b' '; DATE_LEN];
        let sec = crate::sys::now_secs();
        clock_slot(&mut text, sec);
        Clock { sec, text }
    }

    fn tick(&mut self) -> (i64, [u8; DATE_LEN]) {
        let now = crate::sys::now_secs();
        if now != self.sec {
            self.sec = now;
            clock_slot(&mut self.text, now);
        }
        (self.sec, self.text)
    }
}

fn clock_slot(text: &mut [u8; DATE_LEN], sec: i64) {
    let s = http_date(sec);
    let bytes = s.as_bytes();
    let n = bytes.len().min(DATE_LEN);
    text[..n].copy_from_slice(&bytes[..n]);
}

// ------------------------------------------------------------- in-RAM responses

/// A finished response: header block with a Date hole, optionally the body
/// appended, so serving it is one slice copy into the connection buffer.
struct Cached {
    status: u16,
    path: PathBuf,
    mtime: i64,
    size: u64,
    etag: String,
    stamp: i64,
    date_off: usize,
    head_len: usize,
    checked: Instant,
    /// Body already lives inside `buf`.
    in_ram: bool,
    buf: Vec<u8>,
    ctype: String,
    gz: Option<Box<Cached>>,
}

/// Is this worker's cache holding more bytes than it should? Walks the entries
/// and stops at the first one over the line, so the common small cache costs a
/// few adds.
fn over_budget(cache: &HashMap<String, Cached>) -> bool {
    if cache.len() < 256 {
        return false;
    }
    let mut total = 0usize;
    for c in cache.values() {
        total += c.buf.len() + c.gz.as_ref().map(|g| g.buf.len()).unwrap_or(0);
        if total >= CACHE_BYTES {
            return true;
        }
    }
    false
}

fn head_start(code: u16, ctype: &str, length: u64, gz: bool) -> (Vec<u8>, usize) {
    let mut head = Vec::with_capacity(360);
    head.extend_from_slice(b"HTTP/1.1 ");
    head.extend_from_slice(code.to_string().as_bytes());
    head.push(b' ');
    head.extend_from_slice(reason(code).as_bytes());
    head.extend_from_slice(b"\r\nServer: cartridge\r\n");
    let date_off = head.len() + 6;
    head.extend_from_slice(b"Date: ");
    head.extend_from_slice(&[b'-'; DATE_LEN]);
    head.extend_from_slice(b"\r\n");
    head.extend_from_slice(b"Content-Type: ");
    head.extend_from_slice(ctype.as_bytes());
    head.extend_from_slice(b"\r\nContent-Length: ");
    head.extend_from_slice(length.to_string().as_bytes());
    // Never let a browser second-guess the type we just declared.
    head.extend_from_slice(b"\r\nX-Content-Type-Options: nosniff\r\n");
    if gz {
        head.extend_from_slice(b"Content-Encoding: gzip\r\n");
    }
    (head, date_off)
}

fn head_tail(head: &mut Vec<u8>, mtime: Option<i64>, etag: Option<&str>) {
    if let Some(m) = mtime {
        head.extend_from_slice(b"Last-Modified: ");
        head.extend_from_slice(http_date(m).as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    if let Some(e) = etag {
        head.extend_from_slice(b"ETag: ");
        head.extend_from_slice(e.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(
        b"Accept-Ranges: bytes\r\nVary: Accept-Encoding\r\nConnection: keep-alive\r\n\r\n",
    );
}

impl Cached {
    fn file(path: &Path, mtime: i64, size: u64, ctype: &str, want_gz: bool) -> Cached {
        let etag = format!("\"{:x}-{:x}\"", mtime.max(0), size);
        let (mut buf, date_off) = head_start(200, ctype, size, false);
        head_tail(&mut buf, Some(mtime), Some(&etag));
        let head_len = buf.len();
        let mut in_ram = false;
        if size <= RAM_LIMIT {
            if let Ok(body) = fs::read(path) {
                if body.len() as u64 == size {
                    buf.extend_from_slice(&body);
                    in_ram = true;
                }
            }
        }
        Cached {
            status: 200,
            path: path.to_path_buf(),
            mtime,
            size,
            etag,
            stamp: -1,
            date_off,
            head_len,
            checked: Instant::now(),
            in_ram,
            buf,
            ctype: ctype.to_string(),
            gz: if want_gz { gz_variant(path, mtime, ctype) } else { None },
        }
    }

    fn error(code: u16) -> Cached {
        let body = error_body(code);
        let ctype = "text/html; charset=utf-8";
        let (mut buf, date_off) = head_start(code, ctype, body.len() as u64, false);
        head_tail(&mut buf, None, None);
        let head_len = buf.len();
        buf.extend_from_slice(&body);
        Cached {
            status: code,
            path: PathBuf::new(),
            mtime: 0,
            size: body.len() as u64,
            etag: String::new(),
            stamp: -1,
            date_off,
            head_len,
            checked: Instant::now(),
            in_ram: true,
            buf,
            ctype: ctype.to_string(),
            gz: None,
        }
    }
}

fn gz_variant(path: &Path, mtime: i64, ctype: &str) -> Option<Box<Cached>> {
    let candidate = path.with_extension(format!(
        "{}.gz",
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    let meta = fs::metadata(&candidate).ok()?;
    if !meta.is_file() {
        return None;
    }
    let gz_mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if gz_mtime < mtime {
        return None;
    }
    let size = meta.len();
    let etag = format!("\"{:x}-{:x}-gz\"", mtime.max(0), size);
    let (mut buf, date_off) = head_start(200, ctype, size, true);
    head_tail(&mut buf, Some(mtime), Some(&etag));
    let head_len = buf.len();
    let mut in_ram = false;
    if size <= RAM_LIMIT {
        if let Ok(body) = fs::read(&candidate) {
            if body.len() as u64 == size {
                buf.extend_from_slice(&body);
                in_ram = true;
            }
        }
    }
    Some(Box::new(Cached {
        status: 200,
        path: candidate,
        mtime,
        size,
        etag,
        stamp: -1,
        date_off,
        head_len,
        checked: Instant::now(),
        in_ram,
        buf,
        ctype: ctype.to_string(),
        gz: None,
    }))
}

// --------------------------------------------------------------------- conn

struct Listener {
    fd: RawFd,
    root: PathBuf,
    site: usize,
}

struct Conn {
    fd: RawFd,
    peer: Ipv4Addr,
    root: PathBuf,
    site: usize,
    inbuf: Vec<u8>,
    outbuf: Vec<u8>,
    out_pos: usize,
    file: Option<File>,
    file_off: i64,
    file_left: u64,
    closing: bool,
    requests: u32,
    deadline: Instant,
    want_write: bool,
    /// Bytes of the current response already delivered, and when it started.
    /// A client that never reads must not pin a socket forever.
    sent: u64,
    started: Instant,
}

impl Conn {
    fn new(fd: RawFd, root: PathBuf, site: usize, peer: Ipv4Addr) -> Conn {
        Conn {
            fd,
            peer,
            root,
            site,
            inbuf: Vec::with_capacity(READ_BUF),
            outbuf: Vec::new(),
            out_pos: 0,
            file: None,
            file_off: 0,
            file_left: 0,
            closing: false,
            requests: 0,
            deadline: Instant::now() + HEADER_TIMEOUT,
            want_write: false,
            sent: 0,
            started: Instant::now(),
        }
    }

    fn reset_for_next(&mut self) {
        // Leftover bytes are the next (pipelined) request; keep them.
        self.outbuf.clear();
        self.out_pos = 0;
        self.file = None;
        self.file_left = 0;
        self.requests += 1;
        self.deadline = Instant::now() + KEEPALIVE_TIMEOUT;
        self.sent = 0;
        self.started = Instant::now();
    }

    fn queue(&mut self, head: Vec<u8>, file: Option<File>, offset: i64, len: u64) {
        self.outbuf = head;
        self.out_pos = 0;
        self.file = file;
        self.file_off = offset;
        self.file_left = len;
        self.sent = 0;
        self.started = Instant::now();
    }

    fn simple(&mut self, code: u16, keep: bool, clock: &mut Clock) {
        let body = if code == 304 { Vec::new() } else { error_body(code) };
        let (sec, date) = clock.tick();
        let mut head = Vec::with_capacity(200);
        head.extend_from_slice(b"HTTP/1.1 ");
        head.extend_from_slice(code.to_string().as_bytes());
        head.push(b' ');
        head.extend_from_slice(reason(code).as_bytes());
        head.extend_from_slice(b"\r\nServer: cartridge\r\nDate: ");
        head.extend_from_slice(&date);
        head.extend_from_slice(b"\r\n");
        let _ = sec;
        if code != 304 {
            head.extend_from_slice(b"Content-Type: text/html; charset=utf-8\r\nContent-Length: ");
            head.extend_from_slice(body.len().to_string().as_bytes());
            head.extend_from_slice(b"\r\nX-Content-Type-Options: nosniff\r\n");
        }
        head.extend_from_slice(if keep {
            &b"Connection: keep-alive\r\n\r\n"[..]
        } else {
            &b"Connection: close\r\n\r\n"[..]
        });
        self.outbuf = head;
        self.outbuf.extend_from_slice(&body);
        self.out_pos = 0;
        self.file = None;
        self.file_left = 0;
        self.sent = 0;
        self.started = Instant::now();
        if !keep {
            self.closing = true;
        }
    }
}

/// What is wrong with a decoded request path, if anything. Checked after
/// decoding, because `%2e%2e` is `..` to the filesystem.
///
/// Control bytes have no place in a path and a decoded `%0d%0a` would end up in
/// the `Location` header of a directory redirect: response splitting. A
/// dot-prefixed component is `.git`, `.env` or `..` and is never a site file.
fn path_error(decoded: &str) -> Option<u16> {
    if !decoded.starts_with('/') || decoded.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Some(400);
    }
    if decoded.split('/').any(|s| !s.is_empty() && s.starts_with('.')) {
        return Some(403);
    }
    None
}

/// sockaddr_in for `accept4`: 16 bytes, family and port in network order.
#[repr(C)]
struct PeerAddr {
    family: u16,
    port: u16,
    addr: [u8; 4],
    zero: [u8; 8],
}

/// Close a connection and give its address's slot back.
fn forget(conns: &mut HashMap<RawFd, Conn>, per_ip: &mut HashMap<Ipv4Addr, u32>, fd: RawFd) {
    if let Some(c) = conns.remove(&fd) {
        if let Some(n) = per_ip.get_mut(&c.peer) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                per_ip.remove(&c.peer);
            }
        }
        unsafe {
            close(fd);
        }
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// A `Range` header. `Err` means the client asked for something this file
/// cannot satisfy, which is a 416 and not a silent 200: sending the whole file
/// to a player that asked for the middle of it is how a seek looks broken.
fn parse_range(value: &str, size: u64) -> Result<(u64, u64), ()> {
    let spec = value.strip_prefix("bytes=").ok_or(())?;
    if spec.contains(',') {
        return Err(());
    }
    let (a, b) = spec.split_once('-').ok_or(())?;
    if a.is_empty() {
        let tail: u64 = b.trim().parse().map_err(|_| ())?;
        if tail == 0 || size == 0 {
            return Err(());
        }
        return Ok((size.saturating_sub(tail), size - 1));
    }
    let start: u64 = a.trim().parse().map_err(|_| ())?;
    if start >= size {
        return Err(());
    }
    let end = if b.trim().is_empty() {
        size - 1
    } else {
        b.trim().parse::<u64>().map_err(|_| ())?.min(size - 1)
    };
    if end < start {
        return Err(());
    }
    Ok((start, end))
}

struct Req {
    head_only: bool,
    keep: bool,
    if_none_match: bool,
    if_modified: Option<i64>,
    range: Option<(usize, usize)>,
    /// A Range header this file cannot satisfy: answer 416.
    range_bad: bool,
    accept_gzip: bool,
}

/// Hand a cached response to the connection: one Date patch, one copy, one
/// write (or sendfile for a body too big to keep in RAM).
fn deliver(c: &mut Cached, conn: &mut Conn, req: &Req, clock: &mut Clock) -> bool {
    let (sec, date) = clock.tick();
    if c.stamp != sec {
        c.buf[c.date_off..c.date_off + DATE_LEN].copy_from_slice(&date);
        c.stamp = sec;
    }

    if c.status == 304 {
        conn.simple(304, req.keep, clock);
        return req.keep;
    }

    if c.status == 200 {
        if req.if_none_match || date_matches(c, req.if_modified) {
            conn.simple(304, req.keep, clock);
            return req.keep;
        }
        if req.range_bad {
            conn.simple(416, req.keep, clock);
            return req.keep;
        }
        if let Some(range) = req.range {
            return partial(c, conn, range, req.head_only, req.keep, clock);
        }
        if req.accept_gzip {
            if let Some(gz) = c.gz.as_mut() {
                return deliver(gz, conn, req, clock);
            }
        }
    }

    conn.outbuf.clear();
    conn.out_pos = 0;
    if c.in_ram {
        conn.file = None;
        conn.file_left = 0;
        let end = if req.head_only { c.head_len } else { c.buf.len() };
        conn.outbuf.extend_from_slice(&c.buf[..end]);
        return req.keep;
    }
    // Too big for RAM: send the header, then stream the file.
    conn.outbuf.extend_from_slice(&c.buf[..c.head_len]);
    if req.head_only {
        conn.file = None;
        conn.file_left = 0;
        return req.keep;
    }
    match File::open(&c.path) {
        Ok(f) => {
            conn.file = Some(f);
            conn.file_off = 0;
            conn.file_left = c.size;
        }
        Err(_) => {
            conn.file = None;
            conn.file_left = 0;
        }
    }
    req.keep
}

fn partial(
    c: &Cached,
    conn: &mut Conn,
    (start, end): (usize, usize),
    head_only: bool,
    keep: bool,
    clock: &mut Clock,
) -> bool {
    let len = end - start + 1;
    let (sec, date) = clock.tick();
    let _ = sec;
    let mut head = Vec::with_capacity(384);
    head.extend_from_slice(b"HTTP/1.1 206 Partial Content\r\nServer: cartridge\r\nDate: ");
    head.extend_from_slice(&date);
    head.extend_from_slice(b"\r\nContent-Type: ");
    head.extend_from_slice(c.ctype.as_bytes());
    head.extend_from_slice(b"\r\nContent-Range: bytes ");
    head.extend_from_slice(start.to_string().as_bytes());
    head.push(b'-');
    head.extend_from_slice(end.to_string().as_bytes());
    head.push(b'/');
    head.extend_from_slice(c.size.to_string().as_bytes());
    head.extend_from_slice(b"\r\nContent-Length: ");
    head.extend_from_slice(len.to_string().as_bytes());
    head.extend_from_slice(b"\r\nETag: ");
    head.extend_from_slice(c.etag.as_bytes());
    head.extend_from_slice(b"\r\nAccept-Ranges: bytes\r\nVary: Accept-Encoding\r\n");
    head.extend_from_slice(if keep {
        &b"Connection: keep-alive\r\n\r\n"[..]
    } else {
        &b"Connection: close\r\n\r\n"[..]
    });
    conn.queue(head, None, start as i64, 0);
    if !keep {
        conn.closing = true;
    }
    if !head_only {
        if c.in_ram {
            conn.outbuf.extend_from_slice(&c.buf[c.head_len + start..c.head_len + end + 1]);
        } else if let Ok(f) = File::open(&c.path) {
            conn.file = Some(f);
            conn.file_off = start as i64;
            conn.file_left = len as u64;
        }
    }
    keep
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

fn bind_port(port: u16, bind_addr: [u8; 4]) -> io::Result<c_int> {
    unsafe {
        let fd = socket(2, 1 | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let one: c_int = 1;
        setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one as *const _ as *const c_void, 4);
        setsockopt(fd, SOL_SOCKET, SO_REUSEPORT, &one as *const _ as *const c_void, 4);
        let sndbuf: c_int = 1 << 20;
        setsockopt(fd, SOL_SOCKET, SO_SNDBUF, &sndbuf as *const _ as *const c_void, 4);
        let defer: c_int = 1;
        setsockopt(fd, IPPROTO_TCP, TCP_DEFER_ACCEPT, &defer as *const _ as *const c_void, 4);
        // Accept the data in the third packet of the handshake: one round trip
        // saved for every new connection. Ignored when the sysctl is off.
        let fastopen: c_int = 1024;
        setsockopt(fd, IPPROTO_TCP, TCP_FASTOPEN, &fastopen as *const _ as *const c_void, 4);
        #[repr(C)]
        struct SockAddrIn {
            family: u16,
            port: u16,
            addr: u32,
            zero: [u8; 8],
        }
        let addr = SockAddrIn {
            family: 2, // AF_INET, host order
            port: port.to_be(),
            addr: u32::from_ne_bytes(bind_addr),
            zero: [0; 8],
        };
        if bind(fd, &addr as *const _ as *const c_void, std::mem::size_of::<SockAddrIn>() as u32) != 0 {
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

fn flush(conn: &mut Conn) -> io::Result<bool> {
    // Headers first.
    while conn.out_pos < conn.outbuf.len() {
        let n = unsafe {
            crate::sys::write_fd(
                conn.fd,
                conn.outbuf[conn.out_pos..].as_ptr(),
                conn.outbuf.len() - conn.out_pos,
            )
        };
        if n > 0 {
            conn.out_pos += n as usize;
            conn.sent += n as u64;
        } else if n == -1 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                conn.want_write = true;
                return Ok(false);
            }
            return Err(err);
        } else {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0"));
        }
    }
    conn.outbuf.clear();
    conn.out_pos = 0;

    if let Some(file) = &conn.file {
        while conn.file_left > 0 {
            let mut offset = conn.file_off;
            let n = unsafe { sendfile(conn.fd, file.as_raw_fd(), &mut offset, conn.file_left as usize) };
            if n > 0 {
                conn.file_off = offset;
                conn.file_left -= n as u64;
                conn.sent += n as u64;
            } else if n == 0 {
                conn.file_left = 0;
            } else {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    conn.want_write = true;
                    return Ok(false);
                }
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
        }
        conn.file = None;
    }
    conn.want_write = false;
    Ok(true)
}

/// A file may only be served from inside the site root, or from the release the
/// RAM staging links back to. A symlink in a release must not turn into a way
/// to read the host.
fn contained(path: &Path, root: &Path) -> bool {
    let real = match fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => return false,
    };
    if let Ok(r) = fs::canonicalize(root) {
        if real.starts_with(&r) {
            return true;
        }
    }
    match fs::canonicalize(LIVE) {
        Ok(live) => real.starts_with(live),
        Err(_) => false,
    }
}

/// Resolve the request path into a file, following the one directory rule the
/// server has: `/` means `index.html`.
fn locate(root: &Path, decoded: &str) -> Locate {
    let mut path = root.to_path_buf();
    for segment in decoded.split('/').filter(|s| !s.is_empty()) {
        path.push(segment);
    }
    if decoded.ends_with('/') {
        for name in ["index.html", "index.htm"] {
            let candidate = path.join(name);
            if candidate.is_file() && contained(&candidate, root) {
                return Locate::File(candidate);
            }
        }
        if path.is_dir() {
            return Locate::Forbidden;
        }
        return Locate::Missing;
    }
    match fs::metadata(&path) {
        Ok(meta) if meta.is_dir() => Locate::Redirect,
        Ok(_) if contained(&path, root) => Locate::File(path),
        Ok(_) => Locate::Forbidden,
        Err(_) => Locate::Missing,
    }
}

enum Locate {
    File(PathBuf),
    Redirect,
    Forbidden,
    Missing,
}

fn file_stats(path: &Path) -> Option<(i64, u64)> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Some((mtime, meta.len()))
}

fn if_modified_secs(value: &str) -> Option<i64> {
    // Only the exact format we emit matters: "Sun, 06 Nov 1994 08:49:37 GMT".
    let b = value.as_bytes();
    if b.len() != DATE_LEN {
        return None;
    }
    let day: i64 = value.get(5..7)?.parse().ok()?;
    let mon = match value.get(8..11)? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = value.get(12..16)?.parse().ok()?;
    let hour: i64 = value.get(17..19)?.parse().ok()?;
    let minute: i64 = value.get(20..22)?.parse().ok()?;
    let second: i64 = value.get(23..25)?.parse().ok()?;
    let days = days_from_civil(year, mon, day as u32);
    Some(days * 86400 + hour * 3600 + minute * 60 + second)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Build the response for one request. Returns true when the connection can
/// carry another request.
fn handle_request(
    conn: &mut Conn,
    cache: &mut HashMap<String, Cached>,
    clock: &mut Clock,
) -> bool {
    let end = match find_header_end(&conn.inbuf) {
        Some(e) => e,
        None => {
            if conn.inbuf.len() > HEADER_CAP {
                conn.simple(414, false, clock);
                return false;
            }
            return true;
        }
    };
    let header_text = String::from_utf8_lossy(&conn.inbuf[..end]).to_string();
    conn.inbuf.drain(..end + 4);
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    let version = parts.next().unwrap_or("HTTP/1.1");
    let head_only = method == "HEAD";
    if method != "GET" && method != "HEAD" {
        conn.simple(405, false, clock);
        return false;
    }
    if target.len() > 4096 {
        conn.simple(414, false, clock);
        return false;
    }
    let mut keep = version == "HTTP/1.1" && !header_text.contains("Connection: close")
        && !header_text.to_ascii_lowercase().contains("connection: close");
    if version != "HTTP/1.1" {
        keep = false;
    }

    let mut if_none_match_etag: Option<&str> = None;
    let mut if_modified_raw: Option<&str> = None;
    let mut range_raw: Option<&str> = None;
    let mut if_range_raw: Option<&str> = None;
    let mut accept_gzip = false;
    // This server never accepts a request body. A message that carries one
    // would leave its bytes in the buffer, where they would be read as the next
    // request -- the request-smuggling shape -- so the framing headers are
    // refused outright instead of ignored.
    let mut bad_framing = false;
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("if-none-match:") {
            let _ = v;
            if_none_match_etag = line.split_once(':').map(|(_, v)| v.trim());
        } else if let Some(_v) = lower.strip_prefix("if-modified-since:") {
            if_modified_raw = line.split_once(':').map(|(_, v)| v.trim());
        } else if lower.starts_with("range:") {
            range_raw = line.split_once(':').map(|(_, v)| v.trim());
        } else if lower.starts_with("if-range:") {
            if_range_raw = line.split_once(':').map(|(_, v)| v.trim());
        } else if lower.starts_with("accept-encoding:") {
            accept_gzip = lower.contains("gzip");
        } else if lower.starts_with("transfer-encoding:") {
            bad_framing = true;
        } else if lower.starts_with("content-length:") {
            if line.split_once(':').map(|(_, v)| v.trim()) != Some("0") {
                bad_framing = true;
            }
        }
    }
    if bad_framing {
        conn.simple(400, false, clock);
        return false;
    }

    let decoded = match decode_path(target) {
        Some(d) => d,
        None => {
            conn.simple(400, false, clock);
            return false;
        }
    };
    if let Some(code) = path_error(&decoded) {
        conn.simple(code, false, clock);
        return false;
    }

    // Hot path: the same path inside the last second, already a finished
    // response, no stat and no file lookup.
    let fresh = match cache.get(decoded.as_str()) {
        Some(c) => c.checked.elapsed() < REVALIDATE,
        None => false,
    };
    if fresh {
        let c = cache.get_mut(decoded.as_str()).unwrap();
        let req = build_req(
            c,
            head_only,
            keep,
            if_none_match_etag,
            if_modified_raw,
            range_raw,
            if_range_raw,
            accept_gzip,
        );
        return deliver(c, conn, &req, clock);
    }

    let entry = match locate(&conn.root, &decoded) {
        Locate::File(path) => {
            let (mtime, size) = match file_stats(&path) {
                Some(s) => s,
                None => return deliver_error(conn, cache, decoded, 404, keep, head_only, clock),
            };
            match cache.get_mut(decoded.as_str()) {
                Some(c) if c.status == 200 && c.mtime == mtime && c.size == size => {
                    c.checked = Instant::now();
                    if accept_gzip && c.gz.is_none() {
                        let ctype = content_type(&path);
                        c.gz = gz_variant(&path, mtime, &ctype);
                    }
                    None
                }
                _ => {
                    let ctype = content_type(&path);
                    let c = Cached::file(&path, mtime, size, &ctype, accept_gzip);
                    Some((decoded.clone(), c))
                }
            }
        }
        Locate::Redirect => {
            let mut head = Vec::with_capacity(240);
            head.extend_from_slice(b"HTTP/1.1 301 Moved Permanently\r\nServer: cartridge\r\n");
            let date_off = head.len() + 6;
            head.extend_from_slice(b"Date: ");
            head.extend_from_slice(&[b'-'; DATE_LEN]);
            head.extend_from_slice(b"\r\nLocation: ");
            head.extend_from_slice(decoded.as_bytes());
            head.extend_from_slice(b"/\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n");
            let (sec, date) = clock.tick();
            head[date_off..date_off + DATE_LEN].copy_from_slice(&date);
            let _ = sec;
            conn.queue(head, None, 0, 0);
            return keep;
        }
        Locate::Forbidden => Some((decoded.clone(), Cached::error(403))),
        Locate::Missing => Some((decoded.clone(), Cached::error(404))),
    };

    if let Some((key, c)) = entry {
        if cache.len() >= CACHE_ENTRIES || over_budget(cache) {
            cache.clear();
        }
        cache.insert(key, c);
    }
    let c = cache.get_mut(decoded.as_str()).unwrap();
    let req = build_req(
        c,
        head_only,
        keep,
        if_none_match_etag,
        if_modified_raw,
        range_raw,
        if_range_raw,
        accept_gzip,
    );
    deliver(c, conn, &req, clock)
}

fn deliver_error(
    conn: &mut Conn,
    cache: &mut HashMap<String, Cached>,
    decoded: String,
    code: u16,
    keep: bool,
    head_only: bool,
    clock: &mut Clock,
) -> bool {
    if cache.len() >= CACHE_ENTRIES || over_budget(cache) {
        cache.clear();
    }
    cache.insert(decoded.clone(), Cached::error(code));
    let c = cache.get_mut(decoded.as_str()).unwrap();
    let req = Req {
        head_only,
        keep,
        if_none_match: false,
        if_modified: None,
        range: None,
        range_bad: false,
        accept_gzip: false,
    };
    deliver(c, conn, &req, clock)
}

fn content_type(path: &Path) -> String {
    let kind = mime(path);
    if wants_charset(kind) {
        format!("{}; charset=utf-8", kind)
    } else {
        kind.to_string()
    }
}

fn build_req(
    c: &Cached,
    head_only: bool,
    keep: bool,
    inm: Option<&str>,
    if_modified: Option<&str>,
    range: Option<&str>,
    if_range: Option<&str>,
    accept_gzip: bool,
) -> Req {
    // `If-Range` asks for the range only while the file is still the one the
    // client saw; a changed file means the whole thing, not a wrong slice.
    let range = match if_range {
        Some(v) if v.trim() != c.etag => None,
        _ => range,
    };
    let if_none_match = match inm {
        Some(v) => v.split(',').any(|t| t.trim() == c.etag),
        None => false,
    };
    let if_modified = match if_modified.and_then(if_modified_secs) {
        Some(t) => Some(t),
        None => None,
    };
    let (range, range_bad) = match range.map(|r| parse_range(r, c.size)) {
        Some(Ok((s, e))) => (Some((s as usize, e as usize)), false),
        Some(Err(())) => (None, true),
        None => (None, false),
    };
    Req { head_only, keep, if_none_match, if_modified, range, range_bad, accept_gzip }
}

fn date_matches(c: &Cached, if_modified: Option<i64>) -> bool {
    matches!(if_modified, Some(t) if t == c.mtime && c.mtime > 0)
}

fn worker(listeners: Vec<Listener>, slot: usize) {
    let epfd = unsafe { epoll_create1(SOCK_CLOEXEC) };
    if epfd < 0 {
        eprintln!("cartridge web: epoll_create1 failed");
        return;
    }
    for l in &listeners {
        add_epoll(epfd, l.fd, EPOLLIN | EPOLLET, (l.fd as u64) << 1);
    }

    let sites = listeners.len();
    let mut conns: HashMap<RawFd, Conn> = HashMap::new();
    let mut per_ip: HashMap<Ipv4Addr, u32> = HashMap::new();
    let mut caches: Vec<HashMap<String, Cached>> = (0..sites).map(|_| HashMap::new()).collect();
    let mut clock = Clock::new();
    let mut events = vec![EpollEvent { events: 0, data: 0 }; 512];
    let mut next_sweep = Instant::now() + Duration::from_secs(1);
    let _ = slot;

    loop {
        // epoll_wait must block indefinitely unless a connection has a deadline.
        let timeout_ms: c_int = if conns.is_empty() {
            -1
        } else {
            let now = Instant::now();
            let soonest = conns
                .values()
                .map(|c| c.deadline)
                .min()
                .unwrap_or(now);
            if soonest <= now {
                0
            } else {
                (soonest - now).as_millis().min(1000) as c_int
            }
        };

        let n = unsafe { epoll_wait(epfd, events.as_mut_ptr(), events.len() as c_int, timeout_ms) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            continue;
        }
        for ev in events.iter().take(n as usize) {
            let token = ev.data;
            if token & 1 == 0 {
                // Listener: accept a batch.
                let fd = (token >> 1) as RawFd;
                let listener = match listeners.iter().find(|l| l.fd == fd) {
                    Some(l) => l,
                    None => continue,
                };
                let mut pause_streak = 0u32;
                loop {
                    let mut addr = PeerAddr { family: 0, port: 0, addr: [0; 4], zero: [0; 8] };
                    let mut addr_len = std::mem::size_of::<PeerAddr>() as u32;
                    let client = unsafe {
                        accept4(
                            fd,
                            &mut addr as *mut _ as *mut c_void,
                            &mut addr_len,
                            SOCK_NONBLOCK | SOCK_CLOEXEC,
                        )
                    };
                    if client < 0 {
                        match crate::guard::accept_action(crate::sys::errno()) {
                            crate::guard::AcceptAction::Retry => continue,
                            // The queue is empty: that is the normal end of the
                            // accept batch, and looping here spins the worker.
                            crate::guard::AcceptAction::Stop => break,
                            // Out of descriptors: wait for one to come back
                            // instead of spinning on an edge-triggered
                            // listener whose readiness we just consumed.
                            crate::guard::AcceptAction::Pause => {
                                pause_streak += 1;
                                if pause_streak > 3 {
                                    break;
                                }
                                crate::sys::sleep_ms(crate::guard::backoff_ms(pause_streak));
                                continue;
                            }
                            crate::guard::AcceptAction::Fatal => break,
                        }
                    }
                    pause_streak = 0;
                    let one: c_int = 1;
                    unsafe {
                        setsockopt(client, IPPROTO_TCP, TCP_NODELAY, &one as *const _ as *const c_void, 4);
                    }
                    if conns.len() >= MAX_CONNECTIONS {
                        unsafe { close(client) };
                        break;
                    }
                    let peer = Ipv4Addr::from(addr.addr);
                    let held = per_ip.get(&peer).copied().unwrap_or(0);
                    if held >= PER_IP_LIMIT {
                        unsafe { close(client) };
                        continue;
                    }
                    *per_ip.entry(peer).or_insert(0) += 1;
                    let token = (client as u64) << 1 | 1;
                    add_epoll(epfd, client, EPOLLIN | EPOLLRDHUP | EPOLLET, token);
                    conns.insert(
                        client,
                        Conn::new(client, listener.root.clone(), listener.site, peer),
                    );
                }
            } else {
                let fd = (token >> 1) as RawFd;
                let mut drop_conn = false;
                if let Some(conn) = conns.get_mut(&fd) {
                    if ev.events & (EPOLLERR | EPOLLHUP) != 0 {
                        drop_conn = true;
                    }
                    if !drop_conn && ev.events & (EPOLLIN | EPOLLRDHUP) != 0 {
                        // Read everything available (edge triggered).
                        let mut chunk = [0u8; READ_BUF];
                        loop {
                            let r = unsafe {
                                crate::sys::read_fd(fd, chunk.as_mut_ptr(), chunk.len())
                            };
                            if r > 0 {
                                conn.inbuf.extend_from_slice(&chunk[..r as usize]);
                                if conn.inbuf.len() > HEADER_CAP + READ_BUF {
                                    drop_conn = true;
                                    break;
                                }
                            } else if r == 0 {
                                drop_conn = true;
                                break;
                            } else {
                                let err = io::Error::last_os_error();
                                if err.kind() == io::ErrorKind::WouldBlock {
                                    break;
                                }
                                if err.kind() == io::ErrorKind::Interrupted {
                                    continue;
                                }
                                drop_conn = true;
                                break;
                            }
                        }
                    }
                    // Process requests while there is a complete one buffered.
                    if !drop_conn {
                        let cache = &mut caches[conn.site];
                        while find_header_end(&conn.inbuf).is_some() {
                            let keep = handle_request(conn, cache, &mut clock);
                            match flush(conn) {
                                Ok(done) => {
                                    if !done {
                                        break;
                                    }
                                }
                                Err(_) => {
                                    drop_conn = true;
                                    break;
                                }
                            }
                            if !keep {
                                conn.closing = true;
                                break;
                            }
                            conn.reset_for_next();
                            if conn.requests >= MAX_REQUESTS {
                                conn.closing = true;
                                break;
                            }
                        }
                    }
                    // Flush anything left after EPOLLOUT.
                    if !drop_conn && ev.events & EPOLLOUT != 0 {
                        match flush(conn) {
                            Ok(done) => {
                                if done && conn.file_left == 0 && conn.out_pos >= conn.outbuf.len() {
                                    // Response finished; handle pipelined input.
                                    let cache = &mut caches[conn.site];
                                    while find_header_end(&conn.inbuf).is_some() {
                                        let keep = handle_request(conn, cache, &mut clock);
                                        match flush(conn) {
                                            Ok(true) => {}
                                            Ok(false) => break,
                                            Err(_) => {
                                                drop_conn = true;
                                                break;
                                            }
                                        }
                                        if !keep {
                                            conn.closing = true;
                                            break;
                                        }
                                        conn.reset_for_next();
                                    }
                                }
                            }
                            Err(_) => drop_conn = true,
                        }
                    }
                    if conn.closing && conn.file_left == 0 && conn.out_pos >= conn.outbuf.len() {
                        drop_conn = true;
                    }
                    let want = if conn.want_write || conn.file_left > 0 {
                        EPOLLOUT | EPOLLRDHUP | EPOLLET
                    } else {
                        EPOLLIN | EPOLLRDHUP | EPOLLET
                    };
                    mod_epoll(epfd, fd, want, token);
                }
                if drop_conn {
                    forget(&mut conns, &mut per_ip, fd);
                }
            }
        }

        // Connection deadlines: one sweep per second, no per-connection timers.
        if Instant::now() >= next_sweep {
            let now = Instant::now();
            let expired: Vec<RawFd> = conns
                .iter()
                .filter(|(_, c)| {
                    if c.deadline <= now && !c.want_write {
                        return true;
                    }
                    // A client that stopped reading must not hold a socket.
                    c.want_write
                        && crate::guard::stalled(
                            c.sent,
                            now.duration_since(c.started).as_millis() as u64,
                        )
                })
                .map(|(fd, _)| *fd)
                .collect();
            for fd in expired {
                forget(&mut conns, &mut per_ip, fd);
            }
            next_sweep = now + Duration::from_secs(1);
        }
    }
}

extern "C" {
    fn _exit(code: c_int) -> !;
}

/// SIGHUP means the port set changed: exit and let systemd start us again with
/// the new listeners. No signal-safe work beyond the syscall.
extern "C" fn on_hup(_: c_int) {
    unsafe { _exit(0) }
}

/// Pin a worker to one CPU so its response cache stays in that core's L2.
fn pin(slot: usize, cpus: usize) {
    if cpus <= 1 {
        return;
    }
    let cpu = slot % cpus;
    let mut set = [0u8; 128];
    set[cpu / 8] |= 1 << (cpu % 8);
    unsafe {
        sched_setaffinity(0, 128, set.as_ptr());
    }
}

/// CPU threads this machine can actually run: no point starting more workers
/// than there are hardware threads, and no point using fewer.
pub fn cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(1, 128)
}

/// Serve every site under `root`. One worker per CPU thread binds the same
/// ports with SO_REUSEPORT; the kernel spreads connections between them. The
/// bind address is loopback in cluster mode, where a balancer owns the public
/// ports and hands work to these workers.
pub fn run(
    root: PathBuf,
    offset: u16,
    workers: usize,
    bind_addr: [u8; 4],
) -> Result<(), String> {
    unsafe {
        signal(1, on_hup as usize);
    }
    let _ = fs::write("/run/cartridge-web.pid", format!("{}\n", std::process::id()));

    let cpus = cpu_count();
    let workers = if workers == 0 { cpus } else { workers.clamp(1, 128) };

    let mut sites = scan(&root);
    for site in sites.iter_mut() {
        site.port = site.port.saturating_add(offset);
    }
    if sites.is_empty() {
        eprintln!("cartridge web: no sites under {}, waiting for content", root.display());
    }
    let pairs: Vec<(u16, PathBuf)> = sites.iter().map(|s| (s.port, s.root.clone())).collect();

    // Bind every worker's listeners before giving up root: SO_REUSEPORT lets
    // each worker own its own socket on the same port.
    let mut sets: Vec<Vec<Listener>> = Vec::new();
    for id in 0..workers {
        let mut ls = Vec::new();
        for (site, (port, root)) in pairs.iter().enumerate() {
            match bind_port(*port, bind_addr) {
                Ok(fd) => {
                    if id == 0 {
                        eprintln!(
                            "cartridge web: listening on {}:{}",
                            Ipv4Addr::from(bind_addr),
                            port
                        );
                    }
                    ls.push(Listener { fd, root: root.clone(), site });
                }
                Err(e) => eprintln!("cartridge web: cannot listen on {}: {}", port, e),
            }
        }
        sets.push(ls);
    }
    eprintln!(
        "cartridge web: {} worker(s) on {} CPU(s), {} site(s) from RAM",
        workers, cpus, pairs.len()
    );

    drop_privileges();

    let mut sets = sets.into_iter();
    let first = sets.next().unwrap_or_default();
    let mut handles = Vec::new();
    for (i, ls) in sets.enumerate() {
        let slot = i + 1;
        handles.push(std::thread::spawn(move || {
            pin(slot, cpus);
            worker(ls, slot)
        }));
    }
    pin(0, cpus);
    worker(first, 0);
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

/// Give up root after the ports are bound. The check matters: a `setuid` that
/// fails would leave a network-facing server running as root, and the failure
/// is a return value, not a signal.
pub fn drop_privileges() {
    unsafe {
        if geteuid() == 0 {
            setgid(65534);
            setuid(65534);
            if geteuid() == 0 {
                eprintln!("cartridge web: could not drop root privileges; refusing to serve");
                _exit(1);
            }
        }
    }
}

#[allow(dead_code)]
fn unused(_: c_long) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_that_could_split_a_response_is_refused() {
        assert_eq!(path_error("/a\r\nSet-Cookie: x"), Some(400));
        assert_eq!(path_error("/a\0b"), Some(400));
        assert_eq!(path_error("a/b"), Some(400));
        assert_eq!(path_error("/a/../b"), Some(403));
        assert_eq!(path_error("/.git/config"), Some(403));
        assert_eq!(path_error("/a/b.html"), None);
        assert_eq!(path_error("/"), None);
    }

    #[test]
    fn a_path_is_decoded_once_and_checked_after() {
        // `%2e%2e` is `..` to the filesystem, so the check has to run on the
        // decoded form; a truncated escape is a bad request, not a filename.
        assert_eq!(decode_path("/%2e%2e/b").as_deref(), Some("/../b"));
        assert_eq!(path_error("/../b"), Some(403));
        assert_eq!(decode_path("/a%2"), None);
        assert_eq!(decode_path("/a?x=1").as_deref(), Some("/a"));
    }

    #[test]
    fn an_unsatisfiable_range_is_a_416_not_the_whole_file() {
        assert_eq!(parse_range("bytes=0-9", 100), Ok((0, 9)));
        assert_eq!(parse_range("bytes=90-", 100), Ok((90, 99)));
        assert_eq!(parse_range("bytes=-10", 100), Ok((90, 99)));
        assert_eq!(parse_range("bytes=100-", 100), Err(()));
        assert_eq!(parse_range("bytes=5-1", 100), Err(()));
        assert_eq!(parse_range("bytes=-0", 100), Err(()));
        assert_eq!(parse_range("items=0-1", 100), Err(()));
        assert_eq!(parse_range("bytes=0-1,3-4", 100), Err(()));
    }
}
