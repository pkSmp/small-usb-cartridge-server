//! Thin layer over the few system calls libstd does not expose, plus small
//! helpers shared by every command. No crates: the whole binary is std only,
//! which keeps it small enough to start instantly on a 4 GB single-core box.
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::raw::{c_char, c_int, c_long, c_ulong, c_void};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

extern "C" {
    fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn mount(src: *const c_char, target: *const c_char, fstype: *const c_char, flags: c_ulong, data: *const c_void) -> c_int;
    fn umount2(target: *const c_char, flags: c_int) -> c_int;
    fn flock(fd: c_int, op: c_int) -> c_int;
    fn time(t: *mut i64) -> i64;
    fn localtime_r(t: *const i64, tm: *mut Tm) -> *mut Tm;
    fn utimensat(dirfd: c_int, path: *const c_char, times: *const Timespec, flags: c_int) -> c_int;
    fn statvfs(path: *const c_char, buf: *mut StatVfs) -> c_int;
}

/// The first fields of Linux `struct statvfs` on 64-bit targets. The trailing
/// space covers the fields this program does not read, so glibc can fill the
/// whole structure without overrunning this buffer.
#[repr(C)]
struct StatVfs {
    f_bsize: c_ulong,
    f_frsize: c_ulong,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    _rest: [u8; 128],
}

const AT_FDCWD: c_int = -100;

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Tm {
    tm_sec: c_int,
    tm_min: c_int,
    tm_hour: c_int,
    tm_mday: c_int,
    tm_mon: c_int,
    tm_year: c_int,
    tm_wday: c_int,
    tm_yday: c_int,
    tm_isdst: c_int,
    tm_gmtoff: c_long,
    tm_zone: *const c_char,
}

impl Default for Tm {
    fn default() -> Tm {
        Tm {
            tm_sec: 0,
            tm_min: 0,
            tm_hour: 0,
            tm_mday: 0,
            tm_mon: 0,
            tm_year: 0,
            tm_wday: 0,
            tm_yday: 0,
            tm_isdst: 0,
            tm_gmtoff: 0,
            tm_zone: std::ptr::null(),
        }
    }
}

pub fn read_fd(fd: c_int, buf: *mut u8, count: usize) -> isize {
    unsafe { read(fd, buf as *mut c_void, count) }
}

pub fn write_fd(fd: c_int, buf: *const u8, count: usize) -> isize {
    unsafe { write(fd, buf as *const c_void, count) }
}

pub fn sync() {
    extern "C" {
        fn sync();
    }
    unsafe { sync() }
}

/// The errno of the last failed call on this thread. `io::Error::last_os_error`
/// hides the number behind an `ErrorKind`, and `accept` needs the raw value to
/// tell `EMFILE` (back off) from `EINVAL` (the listener is broken).
#[cfg(target_os = "linux")]
pub fn errno() -> i32 {
    extern "C" {
        fn __errno_location() -> *mut i32;
    }
    unsafe { *__errno_location() }
}

/// Make a FIFO. Only used to prove the importer refuses one; serving never
/// creates them.
#[cfg(all(test, target_os = "linux"))]
pub fn mkfifo(path: &str) -> io::Result<()> {
    extern "C" {
        fn mkfifo(path: *const c_char, mode: u32) -> i32;
    }
    let c = std::ffi::CString::new(path).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    if unsafe { mkfifo(c.as_ptr(), 0o644) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub fn ioctl_ptr(fd: c_int, request: c_ulong, ptr: *mut c_void) -> io::Result<()> {
    let rc = unsafe { ioctl(fd, request, ptr) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn now_secs() -> i64 {
    unsafe { time(std::ptr::null_mut()) }
}

fn tm_now() -> Tm {
    let mut tm = Tm::default();
    let t = now_secs();
    unsafe {
        localtime_r(&t, &mut tm);
    }
    tm
}

/// strftime for the handful of formats the scripts used.
pub fn fmt_time(pattern: &str) -> String {
    let tm = tm_now();
    match pattern {
        "%F %T" => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        ),
        "%H:%M:%S" => format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec),
        "%Y-%m-%d %H:%M:%S" => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        ),
        "%Y%m%d-%H%M%S" => format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        ),
        _ => format!("{}", now_secs()),
    }
}

pub fn read_trim(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

pub fn write_all(path: impl AsRef<Path>, data: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.as_ref().parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(path, data)
}

pub fn append(path: impl AsRef<Path>, text: &str) {
    use std::io::Write;
    if let Some(parent) = path.as_ref().parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(text.as_bytes());
    }
}

pub fn is_mountpoint(path: &str) -> bool {
    let want = match fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let info = match fs::read_to_string("/proc/self/mountinfo") {
        Ok(s) => s,
        Err(_) => return false,
    };
    for line in info.lines() {
        let mut it = line.split(' ');
        if it.nth(4).map(|m| Path::new(m) == want).unwrap_or(false) {
            return true;
        }
    }
    false
}

pub fn mount_tmpfs(target: &str, size_mb: u64) -> io::Result<()> {
    let src = CString::new("tmpfs").unwrap();
    let tgt = CString::new(target).unwrap();
    let fst = CString::new("tmpfs").unwrap();
    // nosuid/nodev belong in the flags; tmpfs rejects them as data options.
    let data = CString::new(format!("size={}M,mode=0755", size_mb)).unwrap();
    const MS_NOSUID: c_ulong = 2;
    const MS_NODEV: c_ulong = 4;
    let rc = unsafe {
        mount(
            src.as_ptr(),
            tgt.as_ptr(),
            fst.as_ptr(),
            MS_NOSUID | MS_NODEV,
            data.as_ptr() as *const c_void,
        )
    };
    if rc != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// The filesystem type udev recorded for a block device, e.g. "vfat".
pub fn fstype_of(dev: &str) -> String {
    let name = Path::new(dev)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if name.is_empty() {
        return String::new();
    }
    let majmin = match read_trim(format!("/sys/class/block/{}/dev", name)) {
        Some(v) => v,
        None => return String::new(),
    };
    let text = fs::read_to_string(format!("/run/udev/data/b{}", majmin)).unwrap_or_default();
    text.lines()
        .find_map(|l| l.strip_prefix("E:ID_FS_TYPE=").map(|v| v.to_string()))
        .unwrap_or_default()
}

fn mount_type(dev: &str, target: &str, fstype: &str) -> io::Result<()> {
    let src = CString::new(dev).unwrap();
    let tgt = CString::new(target).unwrap();
    let fst = CString::new(fstype).unwrap();
    const MS_RDONLY: c_ulong = 1;
    const MS_NOSUID: c_ulong = 2;
    const MS_NODEV: c_ulong = 4;
    const MS_NOEXEC: c_ulong = 8;
    let rc = unsafe {
        mount(
            src.as_ptr(),
            tgt.as_ptr(),
            fst.as_ptr(),
            MS_RDONLY | MS_NOSUID | MS_NODEV | MS_NOEXEC,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Read-only mount, using the type udev detected and falling back to the
/// usual removable-media filesystems when udev has no opinion.
pub fn mount_ro(dev: &str, target: &str) -> io::Result<()> {
    let detected = fstype_of(dev);
    let mut candidates: Vec<&str> = Vec::new();
    if !detected.is_empty() {
        candidates.push(&detected);
    }
    for guess in ["vfat", "exfat", "ext4", "ext2", "ntfs3", "ntfs", "msdos", "iso9660", "udf"] {
        if !candidates.contains(&guess) {
            candidates.push(guess);
        }
    }
    let mut last: Option<io::Error> = None;
    for fstype in candidates {
        match mount_type(dev, target, fstype) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::from_raw_os_error(19)))
}

pub fn umount(target: &str) -> io::Result<()> {
    let tgt = CString::new(target).unwrap();
    let rc = unsafe { umount2(tgt.as_ptr(), 0) };
    if rc != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Detach a mount whose device is already gone (a pulled cartridge).
pub fn umount_lazy(target: &str) -> io::Result<()> {
    let tgt = CString::new(target).unwrap();
    let rc = unsafe { umount2(tgt.as_ptr(), 2) }; // MNT_DETACH
    if rc != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn try_lock(path: &str) -> Option<fs::File> {
    use std::os::unix::io::AsRawFd;
    let f = fs::OpenOptions::new().create(true).write(true).open(path).ok()?;
    let rc = unsafe { flock(f.as_raw_fd(), 2 | 4) }; // LOCK_EX | LOCK_NB
    if rc == 0 {
        Some(f)
    } else {
        None
    }
}

pub fn set_mtime(path: &Path, secs: i64) {
    let c = match CString::new(path.as_os_str().to_string_lossy().as_bytes()) {
        Ok(v) => v,
        Err(_) => return,
    };
    let times = [
        Timespec { tv_sec: secs, tv_nsec: 0 },
        Timespec { tv_sec: secs, tv_nsec: 0 },
    ];
    unsafe {
        utimensat(AT_FDCWD, c.as_ptr(), times.as_ptr(), 0);
    }
}

/// "1.5 KB" style, matching the awk formatters in the old scripts.
pub fn human(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1048576 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1073741824 {
        format!("{:.1} MB", bytes as f64 / 1048576.0)
    } else {
        format!("{:.1} GB", bytes as f64 / 1073741824.0)
    }
}

pub fn human_kb(kb: u64) -> String {
    if kb < 1024 {
        format!("{} KB", kb)
    } else if kb < 1048576 {
        format!("{:.1} MB", kb as f64 / 1024.0)
    } else {
        format!("{:.1} GB", kb as f64 / 1048576.0)
    }
}

/// Apparent size in KiB, the `du -sk --apparent-size` value.
pub fn apparent_kb(path: &Path) -> u64 {
    let mut total = 0u64;
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if meta.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for e in entries.flatten() {
                total += apparent_kb(&e.path());
            }
        }
    } else {
        total = meta.len();
    }
    (total + 1023) / 1024
}

/// (filesystem total, used, available) in bytes for the filesystem holding
/// `path`. Used includes the OS and every installed program, not just the
/// cartridge's sites; this is the number to quote as storage use.
pub fn filesystem_bytes(path: &str) -> Option<(u64, u64, u64)> {
    let path = CString::new(path).ok()?;
    let mut buf: StatVfs = unsafe { std::mem::zeroed() };
    if unsafe { statvfs(path.as_ptr(), &mut buf) } != 0 {
        return None;
    }
    let unit = if buf.f_frsize > 0 { buf.f_frsize } else { buf.f_bsize };
    if unit == 0 {
        return None;
    }
    let total = buf.f_blocks.saturating_mul(unit);
    let used = buf.f_blocks.saturating_sub(buf.f_bfree).saturating_mul(unit);
    let free = buf.f_bavail.saturating_mul(unit);
    Some((total, used, free))
}

/// Disk usage in KiB, the plain `du -sk` value.
pub fn disk_kb(path: &Path) -> u64 {
    let mut total = 0u64;
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if meta.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for e in entries.flatten() {
                total += disk_kb(&e.path());
            }
        }
        total += 4;
    } else {
        total = meta.blocks() * 512 / 1024;
        if total == 0 && meta.len() > 0 {
            total = 1;
        }
    }
    total
}

pub fn copy_tree(src: &Path, dst: &Path, skip: &[String], delete: bool) -> io::Result<u64> {
    let mut bytes = 0u64;
    copy_into(src, dst, skip, delete, &mut bytes)?;
    Ok(bytes)
}

fn copy_into(src: &Path, dst: &Path, skip: &[String], delete: bool, bytes: &mut u64) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    let mut keep: Vec<String> = Vec::new();
    let entries = match fs::read_dir(src) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if skip.iter().any(|s| s == &name) {
            continue;
        }
        keep.push(name.clone());
        let from = entry.path();
        let to = dst.join(&name);
        let meta = match fs::symlink_metadata(&from) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            copy_into(&from, &to, &[], delete, bytes)?;
            let _ = fs::set_permissions(&to, fs::Permissions::from_mode(meta.permissions().mode()));
        } else if meta.file_type().is_symlink() {
            let _ = fs::remove_file(&to);
            if let Ok(target) = fs::read_link(&from) {
                let _ = std::os::unix::fs::symlink(target, &to);
            }
        } else {
            if let Ok(copied) = fs::copy(&from, &to) {
                *bytes += copied;
                let _ = fs::set_permissions(&to, fs::Permissions::from_mode(meta.permissions().mode()));
                set_mtime(&to, meta.mtime());
            }
        }
    }
    if delete {
        if let Ok(entries) = fs::read_dir(dst) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !keep.contains(&name) {
                    let p = entry.path();
                    if p.is_dir() {
                        let _ = fs::remove_dir_all(&p);
                    } else {
                        let _ = fs::remove_file(&p);
                    }
                }
            }
        }
    }
    Ok(())
}

pub fn lan_ip() -> String {
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("1.1.1.1:53").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                return addr.ip().to_string();
            }
        }
    }
    read_trim("/proc/sys/kernel/hostname").unwrap_or_else(|| "127.0.0.1".into())
}

/// The uid/gid that own a path, if it exists.
pub fn owner(path: &str) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path).ok()?;
    Some((meta.uid(), meta.gid()))
}

/// A system account's ids, from `/etc/passwd`.
pub fn user_id(name: &str) -> Option<(u32, u32)> {
    let text = fs::read_to_string("/etc/passwd").ok()?;
    for line in text.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() >= 4 && f[0] == name {
            return Some((f[2].parse().ok()?, f[3].parse().ok()?));
        }
    }
    None
}

/// Give a tree to another user. The importer runs as root because it mounts,
/// so without this every release it writes is root-owned and an unprivileged
/// cluster could not replace or prune it.
pub fn chown_tree(path: &str, uid: u32, gid: u32) {
    chown_walk(Path::new(path), uid, gid);
}

fn chown_walk(path: &Path, uid: u32, gid: u32) {
    extern "C" {
        fn lchown(path: *const std::os::raw::c_char, uid: u32, gid: u32) -> std::os::raw::c_int;
    }
    if let Ok(c) = std::ffi::CString::new(path.to_string_lossy().as_bytes()) {
        unsafe {
            lchown(c.as_ptr(), uid, gid);
        }
    }
    let entries = match fs::read_dir(path) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let child = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => chown_walk(&child, uid, gid),
            _ => {
                if let Ok(c) = std::ffi::CString::new(child.to_string_lossy().as_bytes()) {
                    unsafe {
                        lchown(c.as_ptr(), uid, gid);
                    }
                }
            }
        }
    }
}

pub fn is_root() -> bool {
    extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() == 0 }
}

/// Drop root for good. Returns true when the process really is unprivileged.
pub fn drop_to(uid: u32, gid: u32) -> bool {
    extern "C" {
        fn setgroups(n: usize, list: *const u32) -> std::os::raw::c_int;
        fn setgid(gid: u32) -> std::os::raw::c_int;
        fn setuid(uid: u32) -> std::os::raw::c_int;
        fn geteuid() -> u32;
    }
    unsafe {
        setgroups(0, std::ptr::null());
        setgid(gid);
        setuid(uid);
        geteuid() != 0
    }
}

pub struct Usb {
    pub dev: String,
    pub size: String,
    pub label: String,
    pub fstype: String,
}

/// Removable partitions, with the label and filesystem from udev's database
/// (the same source lsblk reads, without forking it every second).
pub fn usb_list() -> Vec<Usb> {
    let mut out = Vec::new();
    let blocks = match fs::read_dir("/sys/block") {
        Ok(b) => b,
        Err(_) => return out,
    };
    let mut disks: Vec<String> = blocks
        .flatten()
        .filter(|e| read_trim(e.path().join("removable")).as_deref() == Some("1"))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    disks.sort();
    for disk in disks {
        let mut parts: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = fs::read_dir(format!("/sys/block/{}", disk)) {
            for e in entries.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with(&disk) && n != disk {
                    parts.push(e.path());
                }
            }
        }
        parts.sort();
        for part in parts {
            let name = part.file_name().unwrap().to_string_lossy().to_string();
            let dev = format!("/dev/{}", name);
            let (label, fstype) = udev_fs(&name);
            let size = block_size(&part);
            out.push(Usb {
                dev,
                size,
                label: if label.is_empty() { "(no label)".into() } else { label },
                fstype,
            });
        }
    }
    out
}

fn udev_fs(part: &str) -> (String, String) {
    let sys = Path::new("/sys/class/block").join(part).join("dev");
    let majmin = match read_trim(&sys) {
        Some(v) => v,
        None => return (String::new(), String::new()),
    };
    let file = format!("/run/udev/data/b{}", majmin);
    let text = fs::read_to_string(file).unwrap_or_default();
    let mut label = String::new();
    let mut fstype = String::new();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("E:ID_FS_LABEL=") {
            label = v.to_string();
        } else if let Some(v) = line.strip_prefix("E:ID_FS_TYPE=") {
            fstype = v.to_string();
        }
    }
    (label, fstype)
}

fn block_size(part: &Path) -> String {
    let sectors: u64 = read_trim(part.join("size"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let bytes = sectors * 512;
    let gb = bytes as f64 / 1e9;
    if gb >= 1.0 {
        format!("{:.1}G", gb)
    } else {
        format!("{:.0}M", bytes as f64 / 1e6)
    }
}

pub fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

pub fn kmsg(text: &str) {
    append("/dev/kmsg", &format!("cartridge: {}\n", text));
}

pub fn exists(path: &str) -> bool {
    Path::new(path).exists()
}
