//! The web tier: an edge-triggered epoll server, one worker per core, sharing
//! the ports with SO_REUSEPORT. Bodies leave the machine with sendfile(2);
//! headers are built once per file and reused; nothing wakes up while idle.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::os::raw::{c_int, c_long, c_void};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const MAX_CONNECTIONS: usize = 4096;
const READ_BUF: usize = 8192;
const HEADER_CAP: usize = 16 * 1024;
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REQUESTS: u32 = 1000;

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

// ------------------------------------------------------------- compile-time cache

struct HeaderSet {
    mtime: i64,
    size: u64,
    head: Vec<u8>,
    gz: Option<Vec<u8>>,
    gz_len: u64,
}

fn build_head(code: u16, extra_len: Option<u64>, mtime: i64, size: u64, ctype: &str, etag: &str, gz: bool) -> Vec<u8> {
    let date = http_date(crate::sys::now_secs());
    let lm = http_date(mtime);
    let mut head = Vec::with_capacity(320);
    head.extend_from_slice(format!("HTTP/1.1 {} {}\r\n", code, reason(code)).as_bytes());
    head.extend_from_slice(b"Server: cartridge\r\n");
    head.extend_from_slice(format!("Date: {}\r\n", date).as_bytes());
    head.extend_from_slice(format!("Content-Type: {}\r\n", ctype).as_bytes());
    head.extend_from_slice(format!("Content-Length: {}\r\n", extra_len.unwrap_or(size)).as_bytes());
    head.extend_from_slice(format!("Last-Modified: {}\r\n", lm).as_bytes());
    head.extend_from_slice(format!("ETag: {}\r\n", etag).as_bytes());
    head.extend_from_slice(b"Accept-Ranges: bytes\r\n");
    if gz {
        head.extend_from_slice(b"Content-Encoding: gzip\r\n");
        head.extend_from_slice(b"Vary: Accept-Encoding\r\n");
    }
    head.extend_from_slice(b"Connection: keep-alive\r\n\r\n");
    head
}

// --------------------------------------------------------------------- server

struct Listener {
    fd: RawFd,
    root: PathBuf,
    port: u16,
}

struct Conn {
    fd: RawFd,
    root: PathBuf,
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
}

impl Conn {
    fn new(fd: RawFd, root: PathBuf) -> Conn {
        Conn {
            fd,
            root,
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
        }
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_range(value: &str, size: u64) -> Option<(u64, u64)> {
    let spec = value.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    if a.is_empty() {
        let tail: u64 = b.trim().parse().ok()?;
        if tail == 0 || size == 0 {
            return None;
        }
        return Some((size.saturating_sub(tail), size - 1));
    }
    let start: u64 = a.trim().parse().ok()?;
    if start >= size {
        return None;
    }
    let end = if b.trim().is_empty() {
        size - 1
    } else {
        b.trim().parse::<u64>().ok()?.min(size - 1)
    };
    if end < start {
        return None;
    }
    Some((start, end))
}

impl Conn {
    fn reset_for_next(&mut self) {
        // Leftover bytes are the next (pipelined) request; keep them.
        self.outbuf.clear();
        self.out_pos = 0;
        self.file = None;
        self.file_left = 0;
        self.requests += 1;
        self.deadline = Instant::now() + KEEPALIVE_TIMEOUT;
    }

    fn queue(&mut self, head: Vec<u8>, file: Option<File>, offset: i64, len: u64) {
        self.outbuf = head;
        self.out_pos = 0;
        self.file = file;
        self.file_off = offset;
        self.file_left = len;
    }

    fn simple(&mut self, code: u16, keep: bool) {
        let body = if code == 304 { Vec::new() } else { error_body(code) };
        let date = http_date(crate::sys::now_secs());
        let mut head = Vec::with_capacity(200);
        head.extend_from_slice(format!("HTTP/1.1 {} {}\r\n", code, reason(code)).as_bytes());
        head.extend_from_slice(b"Server: cartridge\r\n");
        head.extend_from_slice(format!("Date: {}\r\n", date).as_bytes());
        if code != 304 {
            head.extend_from_slice(b"Content-Type: text/html; charset=utf-8\r\n");
            head.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
        }
        head.extend_from_slice(if keep { &b"Connection: keep-alive\r\n\r\n"[..] } else { &b"Connection: close\r\n\r\n"[..] });
        self.outbuf = head;
        self.outbuf.extend_from_slice(&body);
        self.out_pos = 0;
        if !keep {
            self.closing = true;
        }
    }
}

/// Build the response for one request. Returns true when the connection can
/// carry another request.
fn handle_request(conn: &mut Conn, cache: &mut HashMap<PathBuf, HeaderSet>) -> bool {
    let end = match find_header_end(&conn.inbuf) {
        Some(e) => e,
        None => {
            if conn.inbuf.len() > HEADER_CAP {
                conn.simple(414, false);
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
        conn.simple(405, false);
        return false;
    }
    if target.len() > 4096 {
        conn.simple(414, false);
        return false;
    }
    let keep = version == "HTTP/1.1"
        && !header_text
            .split("\r\n")
            .any(|l| l.to_ascii_lowercase().starts_with("connection:") && l.to_ascii_lowercase().contains("close"));

    // Conditional and range headers.
    let mut if_none_match: Option<&str> = None;
    let mut if_modified: Option<&str> = None;
    let mut range: Option<&str> = None;
    let mut accept_gzip = false;
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("if-none-match:") {
            let _ = v;
            if_none_match = line.split_once(':').map(|(_, v)| v.trim());
        } else if let Some(_v) = lower.strip_prefix("if-modified-since:") {
            if_modified = line.split_once(':').map(|(_, v)| v.trim());
        } else if lower.starts_with("range:") {
            range = line.split_once(':').map(|(_, v)| v.trim());
        } else if lower.starts_with("accept-encoding:") {
            accept_gzip = lower.contains("gzip");
        }
    }

    let decoded = match decode_path(target) {
        Some(d) => d,
        None => {
            conn.simple(400, false);
            return false;
        }
    };
    if !decoded.starts_with('/') {
        conn.simple(400, false);
        return false;
    }
    for segment in decoded.split('/') {
        if (!segment.is_empty() && segment.starts_with('.')) || segment.contains('\0') {
            conn.simple(403, false);
            return false;
        }
    }
    let mut path = conn.root.clone();
    for segment in decoded.split('/').filter(|s| !s.is_empty()) {
        if segment == ".." {
            conn.simple(403, false);
            return false;
        }
        path.push(segment);
    }

    let meta = match fs::metadata(&path) {
        Ok(m) => m,
        Err(_) => {
            conn.simple(404, false);
            return false;
        }
    };
    if meta.is_dir() {
        if !decoded.ends_with('/') {
            let date = http_date(crate::sys::now_secs());
            let head = format!(
                "HTTP/1.1 301 Moved Permanently\r\nServer: cartridge\r\nDate: {}\r\nLocation: {}/\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n",
                date, decoded
            );
            conn.queue(head.into_bytes(), None, 0, 0);
            return true;
        }
        let mut found = None;
        for name in ["index.html", "index.htm"] {
            let candidate = path.join(name);
            if candidate.is_file() {
                found = Some(candidate);
                break;
            }
        }
        match found {
            Some(p) => path = p,
            None => {
                conn.simple(403, false);
                return false;
            }
        }
    }

    let meta = match fs::metadata(&path) {
        Ok(m) => m,
        Err(_) => {
            conn.simple(404, false);
            return false;
        }
    };
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let etag = format!("\"{:x}-{:x}\"", mtime.max(0), size);

    if let Some(inm) = if_none_match {
        if inm.split(',').any(|t| t.trim() == etag)
            || (if_modified == Some(http_date(mtime).as_str()) && mtime > 0)
        {
            conn.simple(304, keep);
            return keep;
        }
    } else if if_modified == Some(http_date(mtime).as_str()) && mtime > 0 {
        conn.simple(304, keep);
        return keep;
    }

    let kind = mime(&path);
    let ctype = if wants_charset(kind) {
        format!("{}; charset=utf-8", kind)
    } else {
        kind.to_string()
    };

    // Cached header block, rebuilt when the file changes.
    let entry = cache.entry(path.clone()).or_insert_with(|| HeaderSet {
        mtime: -1,
        size: 0,
        head: Vec::new(),
        gz: None,
        gz_len: 0,
    });
    if entry.mtime != mtime || entry.size != size || entry.head.is_empty() {
        entry.mtime = mtime;
        entry.size = size;
        entry.head = build_head(200, None, mtime, size, &ctype, &etag, false);
        entry.gz = None;
        entry.gz_len = 0;
    }

    let gz_candidate = path.with_extension(format!(
        "{}.gz",
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    if accept_gzip && entry.gz.is_none() && gz_candidate.is_file() {
        if let Ok(gz_file) = File::open(&gz_candidate) {
            if let Ok(m) = gz_file.metadata() {
                let gz_mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                if gz_mtime >= mtime {
                    let gz_etag = format!("\"{:x}-{:x}-gz\"", mtime.max(0), m.len());
                    entry.gz = Some(build_head(200, None, mtime, m.len(), &ctype, &gz_etag, true));
                    entry.gz_len = m.len();
                }
            }
        }
    }

    if let Some((start, end)) = range.and_then(|r| parse_range(r, size)) {
        let len = end - start + 1;
        let head = format!(
            "HTTP/1.1 206 Partial Content\r\nServer: cartridge\r\nDate: {}\r\nContent-Type: {}\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nLast-Modified: {}\r\nETag: {}\r\nAccept-Ranges: bytes\r\nConnection: keep-alive\r\n\r\n",
            http_date(crate::sys::now_secs()),
            ctype,
            start,
            end,
            size,
            len,
            http_date(mtime),
            etag
        );
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(_) => {
                conn.simple(403, false);
                return false;
            }
        };
        conn.queue(head.into_bytes(), Some(file), start as i64, if head_only { 0 } else { len });
        if !head_only {
            conn.file_left = len;
        }
        if head_only {
            conn.file = None;
        }
        return true;
    }

    // gzip variant when the client asked for it.
    if accept_gzip {
        if let Some(head) = &entry.gz {
            let file = File::open(&gz_candidate).ok();
            let len = entry.gz_len;
            let head = head.clone();
            conn.queue(head, file, 0, if head_only { 0 } else { len });
            if head_only {
                conn.file = None;
            }
            return true;
        }
    }

    let head = entry.head.clone();
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(_) => {
            conn.simple(403, false);
            return false;
        }
    };
    conn.queue(head, Some(file), 0, if head_only { 0 } else { size });
    if head_only {
        conn.file = None;
    }
    let _ = keep;
    true
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

fn bind_port(port: u16) -> io::Result<c_int> {
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
            addr: 0,
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

fn worker(listeners: Vec<Listener>) {
    let epfd = unsafe { epoll_create1(SOCK_CLOEXEC) };
    if epfd < 0 {
        eprintln!("cartridge web: epoll_create1 failed");
        return;
    }
    for l in &listeners {
        add_epoll(epfd, l.fd, EPOLLIN | EPOLLET, (l.fd as u64) << 1);
    }

    let mut conns: HashMap<RawFd, Conn> = HashMap::new();
    let mut cache: HashMap<PathBuf, HeaderSet> = HashMap::new();
    let mut events = vec![EpollEvent { events: 0, data: 0 }; 256];
    let mut next_sweep = Instant::now() + Duration::from_secs(1);

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
                loop {
                    let client = unsafe { accept4(fd, std::ptr::null_mut(), std::ptr::null_mut(), SOCK_NONBLOCK | SOCK_CLOEXEC) };
                    if client < 0 {
                        break;
                    }
                    let one: c_int = 1;
                    unsafe {
                        setsockopt(client, IPPROTO_TCP, TCP_NODELAY, &one as *const _ as *const c_void, 4);
                    }
                    if conns.len() >= MAX_CONNECTIONS {
                        unsafe { close(client) };
                        break;
                    }
                    let root = listeners
                        .iter()
                        .find(|l| l.fd == fd)
                        .map(|l| l.root.clone())
                        .unwrap_or_default();
                    let token = (client as u64) << 1 | 1;
                    add_epoll(epfd, client, EPOLLIN | EPOLLRDHUP | EPOLLET, token);
                    conns.insert(client, Conn::new(client, root));
                    let _ = client;
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
                        while find_header_end(&conn.inbuf).is_some() {
                            let keep = handle_request(conn, &mut cache);
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
                                    while find_header_end(&conn.inbuf).is_some() {
                                        let keep = handle_request(conn, &mut cache);
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
                    conns.remove(&fd);
                    unsafe {
                        close(fd);
                    }
                }
            }
        }

        // Connection deadlines: one sweep per second, no per-connection timers.
        if Instant::now() >= next_sweep {
            let now = Instant::now();
            let expired: Vec<RawFd> = conns
                .iter()
                .filter(|(_, c)| c.deadline <= now && !c.want_write)
                .map(|(fd, _)| *fd)
                .collect();
            for fd in expired {
                conns.remove(&fd);
                unsafe {
                    close(fd);
                }
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

static RELOAD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Serve every site under `root`. Two workers bind the same ports with
/// SO_REUSEPORT; the kernel spreads connections between them.
pub fn run(root: PathBuf, offset: u16, workers: usize) -> Result<(), String> {
    unsafe {
        signal(1, on_hup as usize);
    }
    let _ = fs::write("/run/cartridge-web.pid", format!("{}\n", std::process::id()));

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
    for id in 0..workers.max(1) {
        let mut ls = Vec::new();
        for (port, root) in &pairs {
            match bind_port(*port) {
                Ok(fd) => {
                    if id == 0 {
                        eprintln!("cartridge web: listening on {}", port);
                    }
                    ls.push(Listener { fd, root: root.clone(), port: *port });
                }
                Err(e) => eprintln!("cartridge web: cannot listen on {}: {}", port, e),
            }
        }
        sets.push(ls);
    }

    drop_privileges();

    let mut sets = sets.into_iter();
    let first = sets.next().unwrap_or_default();
    let mut handles = Vec::new();
    for ls in sets {
        handles.push(std::thread::spawn(move || worker(ls)));
    }
    worker(first);
    let _ = RELOAD;
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

pub fn drop_privileges() {
    unsafe {
        if geteuid() == 0 {
            setgid(65534);
            setuid(65534);
        }
    }
}

#[allow(dead_code)]
fn unused(_: c_long) {}
