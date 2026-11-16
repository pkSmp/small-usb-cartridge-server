//! The status lines under the art, replacing cartridge-console. When the art is
//! on screen it writes exactly two lines below it and mirrors the import log to
//! /var/log/cartridge/console.log.

use crate::art;
use crate::sys;
use std::fs;
use std::path::Path;

const IMPORT_LOG: &str = "/var/log/cartridge/import.log";
const CONSOLE_LOG: &str = "/var/log/cartridge/console.log";
const ART_META: &str = "/var/lib/cartridge/art/art.meta";
const ART_DONE: &str = "/run/cartridge-art.done";
const ART_ROWS: &str = "/run/cartridge-art.rows";

fn log_line(text: &str) {
    sys::append(CONSOLE_LOG, &format!("{}\n", text));
}

fn art_height() -> usize {
    fs::read_to_string(ART_META)
        .ok()
        .and_then(|t| t.split_whitespace().nth(1).and_then(|v| v.parse::<usize>().ok()))
        .unwrap_or(0)
}

fn art_done() -> bool {
    Path::new(ART_DONE).exists() && art_height() > 0
}

fn text_sgr() -> String {
    crate::theme::Theme::load().sgr(crate::theme::Role::Accent)
}

fn tty_write(text: &str) {
    if let Ok(mut f) = fs::OpenOptions::new().write(true).open(art::TTY) {
        use std::io::Write;
        let _ = f.write_all(text.as_bytes());
    }
}

fn terminal_size() -> (usize, usize) {
    // stty size equivalent, read from the tty through the TIOCGWINSZ ioctl.
    #[repr(C)]
    struct Winsize {
        rows: u16,
        cols: u16,
        xpixel: u16,
        ypixel: u16,
    }
    extern "C" {
        fn ioctl(fd: i32, request: u64, ...) -> i32;
        fn open(path: *const i8, flags: i32, ...) -> i32;
        fn close(fd: i32) -> i32;
    }
    const TIOCGWINSZ: u64 = 0x5413;
    let path = std::ffi::CString::new(art::TTY).unwrap();
    let fd = unsafe { open(path.as_ptr(), 0) };
    if fd < 0 {
        return (67, 240);
    }
    let mut ws = Winsize { rows: 0, cols: 0, xpixel: 0, ypixel: 0 };
    let rc = unsafe { ioctl(fd, TIOCGWINSZ, &mut ws as *mut Winsize) };
    unsafe {
        close(fd);
    }
    if rc != 0 || ws.rows == 0 || ws.cols == 0 {
        return (67, 240);
    }
    (ws.rows as usize, ws.cols as usize)
}

fn probe_console() -> bool {
    let (rows, _cols) = terminal_size();
    let marker = format!("cp{}", std::process::id());
    tty_write(&format!("\x1b[{};1H{}", rows, marker));
    sys::sleep_ms(300);
    if let Ok(data) = fs::read("/dev/vcs1") {
        let text = String::from_utf8_lossy(&data);
        if text.contains(&marker) {
            tty_write(&format!("\x1b[{};1H\x1b[K", rows));
            return true;
        }
    }
    false
}

fn status_text() -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| "/usr/local/bin/cartridge".into());
    match std::process::Command::new(exe).arg("status").output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout).to_string(),
        Err(_) => String::new(),
    }
}

fn draw_status(cache: &str, _sgr: &str) {
    let (rows, cols) = terminal_size();
    let heights = fs::read_to_string(ART_ROWS)
        .ok()
        .and_then(|t| t.lines().next().and_then(|v| v.trim().parse::<usize>().ok()))
        .unwrap_or_else(art_height);
    let h = heights;

    // Read the published release itself: status wording can change, the
    // folders cannot.
    let mut port_list: Vec<u32> = Vec::new();
    let live = "/srv/cartridge/live";
    if Path::new(&format!("{}/index.html", live)).is_file() {
        port_list.push(80);
    }
    if let Ok(entries) = fs::read_dir(live) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            if let Ok(p) = entry.file_name().to_string_lossy().parse::<u32>() {
                if (1024..=65535).contains(&p) {
                    port_list.push(p);
                }
            }
        }
    }
    port_list.sort_unstable();
    port_list.dedup();
    let ports = port_list
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let _ = cache;
    let ip = sys::lan_ip();
    let mut urls = String::new();
    for p in ports.split_whitespace() {
        if p == "80" {
            urls.push_str(&format!("http://{} ", ip));
        } else {
            urls.push_str(&format!("http://{}:{} ", ip, p));
        }
    }

    let usb = sys::usb_list();
    let theme = crate::theme::Theme::load();
    use crate::theme::Role;
    let stamp = theme.paint(Role::Dim, &sys::fmt_time("%H:%M:%S"));
    let line1 = if usb.is_empty() {
        format!("{}  {}  no cartridge inserted", stamp, theme.paint(Role::Warn, "USB"))
    } else {
        let first = &usb[0];
        format!(
            "{}  {}  {}  {}  {}",
            stamp,
            theme.paint(Role::Ok, "USB"),
            theme.paint(Role::Text, &first.dev),
            first.size,
            if first.label == "(no label)" { String::new() } else { theme.paint(Role::Accent, &first.label) }
        )
    };
    let port_text = if ports.is_empty() { "none".to_string() } else { ports.clone() };
    let line2 = format!(
        "{}  {}   {}",
        theme.paint(Role::Dim, "sites"),
        theme.paint(Role::Text, &port_text),
        if urls.is_empty() { String::new() } else { theme.paint(Role::Accent, urls.trim_end()) }
    );

    let blank = " ".repeat(cols);
    let mut out = String::new();
    for r in 2..=5 {
        if h + r > rows {
            break;
        }
        out.push_str(&format!("\x1b[{};1H{}", h + r, blank));
    }
    if h + 2 <= rows {
        out.push_str(&format!("\x1b[{};1H{}\x1b[0m", h + 2, line1));
    }
    if h + 3 <= rows {
        out.push_str(&format!("\x1b[{};1H{}\x1b[0m", h + 3, line2));
    }
    if h + 4 <= rows {
        let hint = theme.paint(Role::Dim, "login   Ctrl+Alt+F2");
        out.push_str(&format!("\x1b[{};1H{}\x1b[0m", h + 4, hint));
    }
    out.push_str("\x1b[0m");
    tty_write(&out);
}

pub fn run() -> Result<i32, String> {
    let mut console_ok = false;
    log_line(&format!(
        "{} start pid={} dev={} art={}",
        sys::fmt_time("%F %T"),
        std::process::id(),
        art::TTY,
        art_height()
    ));

    for _ in 0..120 {
        if probe_console() {
            console_ok = true;
            break;
        }
        sys::sleep_ms(1000);
    }

    if !art_done() {
        for _ in 0..20 {
            if art_done() {
                break;
            }
            sys::sleep_ms(1000);
        }
    }

    let mut cache = String::new();
    let mut cache_at = 0i64;
    let mut last_usb = String::new();
    let mut last_print = 0i64;
    let mut log_pos = fs::metadata(IMPORT_LOG).map(|m| m.len()).unwrap_or(0);

    loop {
        if !console_ok && probe_console() {
            console_ok = true;
        }
        let now = sys::now_secs();
        if cache.is_empty() || now - cache_at >= 10 {
            cache = status_text();
            cache_at = now;
        }
        let usb_sig: Vec<String> = sys::usb_list()
            .iter()
            .map(|u| format!("{} {} {}", u.dev, u.size, u.label))
            .collect();
        let signature = usb_sig.join("|");

        if signature != last_usb || now - last_print >= 25 {
            if art_done() {
                let sgr = text_sgr();
                draw_status(&cache, &sgr);
                log_line(&format!("{} USB: {}", sys::fmt_time("%F %T"), if signature.is_empty() { "none".into() } else { signature.clone() }));
            }
            last_usb = signature;
            last_print = now;
        }

        // Mirror new import-log lines into the console log (the old tail -F).
        if let Ok(meta) = fs::metadata(IMPORT_LOG) {
            if meta.len() < log_pos {
                log_pos = 0;
            }
            if meta.len() > log_pos {
                use std::io::{Read, Seek, SeekFrom};
                if let Ok(mut f) = fs::File::open(IMPORT_LOG) {
                    if f.seek(SeekFrom::Start(log_pos)).is_ok() {
                        let mut buf = String::new();
                        if f.read_to_string(&mut buf).is_ok() {
                            for line in buf.lines() {
                                log_line(line);
                            }
                            log_pos = meta.len();
                        }
                    }
                }
            }
        }

        sys::sleep_ms(1000);
        let _ = console_ok;
    }
}
