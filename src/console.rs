//! The report under the art: a Markdown table of every live site (port,
//! folder, size, URL), the machine and staging facts, and the line being
//! spoken typed out as it is heard. Mirrors the import log to
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

fn site_url(ip: &str, port: &str) -> String {
    if port == "80" {
        format!("http://{}/", ip)
    } else {
        format!("http://{}:{}", ip, port)
    }
}

/// Every numeric folder in the live release, plus a root index.html on port 80.
fn live_rows() -> (Vec<[String; 4]>, u64) {
    let live = "/srv/cartridge/live";
    let ip = sys::lan_ip();
    let mut rows: Vec<[String; 4]> = Vec::new();
    let mut total_kb = 0u64;
    let mut dirs: Vec<std::path::PathBuf> = fs::read_dir(live)
        .map(|it| it.flatten().filter(|e| e.path().is_dir()).map(|e| e.path()).collect())
        .unwrap_or_default();
    dirs.sort();
    for dir in dirs {
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let kb = sys::disk_kb(&dir);
        total_kb += kb;
        rows.push([
            format!("`{}`", name),
            format!("`{}/`", name),
            sys::human_kb(kb),
            site_url(&ip, &name),
        ]);
    }
    let index = Path::new(live).join("index.html");
    if index.is_file() {
        let size = fs::metadata(&index).map(|m| m.len()).unwrap_or(0);
        total_kb += size.div_ceil(1024);
        rows.push([
            "`80`".into(),
            "`/`".into(),
            sys::human(size),
            site_url(&ip, "80"),
        ]);
    }
    (rows, total_kb)
}

/// The same Markdown table `cartridge sites` prints: header, alignment row,
/// then one padded row per site.
fn table_lines(rows: &[[String; 4]]) -> Vec<String> {
    let head = [
        "Port".to_string(),
        "Folder".to_string(),
        "Size".to_string(),
        "URL".to_string(),
    ];
    let mut width = [head[0].len(), head[1].len(), head[2].len(), head[3].len()];
    for row in rows {
        for i in 0..4 {
            width[i] = width[i].max(row[i].chars().count());
        }
    }
    let cell = |row: &[String; 4]| -> String {
        format!(
            "| {:<w0$} | {:<w1$} | {:>w2$} | {:<w3$} |",
            row[0], row[1], row[2], row[3],
            w0 = width[0], w1 = width[1], w2 = width[2], w3 = width[3]
        )
    };
    let mut out = vec![cell(&head)];
    out.push(format!(
        "|:{}|:{}|{}:|:{}|",
        "-".repeat(width[0] + 1),
        "-".repeat(width[1] + 1),
        "-".repeat(width[2] + 1),
        "-".repeat(width[3] + 1)
    ));
    for row in rows {
        out.push(cell(row));
    }
    out
}

/// Everything the voice has said, newest last. The log keeps the lines after
/// they are spoken, so this is a history, not a now-playing line.
fn said_lines() -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut out: Vec<String> = Vec::new();
    if let Ok(mut f) = fs::File::open(crate::voice::LOG) {
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if f.seek(SeekFrom::Start(len.saturating_sub(16384))).is_ok() {
            let mut buf = String::new();
            if f.read_to_string(&mut buf).is_ok() {
                for line in buf.lines() {
                    let t = line.trim_start();
                    let text = if t.len() >= 8 && t.as_bytes()[2] == b':' && t.as_bytes()[5] == b':' {
                        t[8..].trim_start()
                    } else {
                        t
                    };
                    if !text.is_empty() {
                        out.push(text.to_string());
                    }
                }
            }
        }
    }
    if out.is_empty() {
        if let Ok(now) = fs::read_to_string(crate::voice::NOW) {
            let t = now.trim();
            if !t.is_empty() {
                out.push(t.to_string());
            }
        }
    }
    out
}

/// Wrap one said line to the screen width without breaking words.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if line.is_empty() {
            line.push_str(word);
        } else if line.chars().count() + 1 + word.chars().count() <= width {
            line.push(' ');
            line.push_str(word);
        } else {
            out.push(line.clone());
            line.clear();
            line.push_str(word);
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

/// The pinned boot report: text only, kept across reboots until a cartridge is
/// inserted and removed.
fn start_test() -> Vec<String> {
    fs::read_to_string(crate::voice::START)
        .ok()
        .map(|text| {
            text.lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn draw_status() {
    let (rows, cols) = terminal_size();
    let heights = fs::read_to_string(ART_ROWS)
        .ok()
        .and_then(|t| t.lines().next().and_then(|v| v.trim().parse::<usize>().ok()))
        .unwrap_or_else(art_height);
    let h = heights;

    let theme = crate::theme::Theme::load();
    use crate::theme::Role;
    let stamp = theme.paint(Role::Dim, &sys::fmt_time("%H:%M:%S"));
    let usb = sys::usb_list();
    let mut block: Vec<String> = Vec::new();
    if usb.is_empty() {
        block.push(format!(
            "{}  {}  no cartridge inserted",
            stamp,
            theme.paint(Role::Warn, "USB")
        ));
    } else {
        let first = &usb[0];
        block.push(format!(
            "{}  {}  {}  {}  {}",
            stamp,
            theme.paint(Role::Ok, "USB"),
            theme.paint(Role::Text, &first.dev),
            theme.paint(Role::Text, &first.size),
            if first.label == "(no label)" {
                String::new()
            } else {
                theme.paint(Role::Accent, &first.label)
            }
        ));
    }

    let (sites, total_kb) = live_rows();
    if sites.is_empty() {
        block.push(format!(
            "{}  {}",
            theme.paint(Role::Dim, "sites"),
            theme.paint(Role::Text, "none published")
        ));
    } else {
        block.push(format!(
            "{}  {}",
            theme.paint(Role::Dim, "sites"),
            theme.paint(
                Role::Text,
                &format!("{} site(s), {} live", sites.len(), sys::human_kb(total_kb))
            )
        ));
        for line in table_lines(&sites) {
            block.push(theme.paint(Role::Text, &line));
        }
    }
    block.push(format!(
        "{}  {}",
        theme.paint(Role::Dim, "machine"),
        theme.paint(Role::Text, &crate::voice::summary())
    ));
    let start = start_test();
    if !start.is_empty() {
        block.push(String::new());
        block.push(theme.paint(Role::Accent, "start test"));
        for line in &start {
            for wrapped in wrap(line, cols.saturating_sub(2).max(20)) {
                block.push(theme.paint(Role::Text, &wrapped));
            }
        }
    }

    // Fill what is left under the art with the spoken history, newest last;
    // lines stay after they finish and only scroll when they no longer fit.
    let mut history: Vec<String> = Vec::new();
    for line in said_lines() {
        history.extend(wrap(&line, cols.saturating_sub(2).max(20)));
    }
    if !history.is_empty() {
        block.push(String::new());
        block.push(theme.paint(Role::Accent, "what it said"));
        let available = rows.saturating_sub(h + 2);
        let fixed = block.len() + 1;
        let room = available.saturating_sub(fixed);
        let start = history.len().saturating_sub(room);
        for line in &history[start..] {
            block.push(theme.paint(Role::Text, line));
        }
    }
    block.push(theme.paint(Role::Dim, "login   Ctrl+Alt+F2"));

    let mut out = String::new();
    let mut r = h + 2;
    for line in &block {
        if r > rows {
            break;
        }
        out.push_str(&format!("\x1b[{};1H\x1b[2K{}\x1b[0m", r, line));
        r += 1;
    }
    // Clear any rows a previous, longer report used.
    while r <= rows {
        out.push_str(&format!("\x1b[{};1H\x1b[2K", r));
        r += 1;
    }
    out.push_str("\x1b[0m");
    tty_write(&out);
}

/// Keep the cartridge mounted read-only while it is inserted. The audio is
/// read straight off it; the server never keeps a copy.
pub fn mount_cartridge() -> bool {
    if sys::is_mountpoint(crate::voice::MOUNT) {
        return true;
    }
    let _ = fs::create_dir_all(crate::voice::MOUNT);
    let dev = match sys::usb_list().into_iter().next() {
        Some(u) => u.dev,
        None => return false,
    };
    match sys::mount_ro(&dev, crate::voice::MOUNT) {
        Ok(()) => {
            log_line(&format!(
                "{} mounted {} at {}",
                sys::fmt_time("%F %T"),
                dev,
                crate::voice::MOUNT
            ));
            true
        }
        Err(e) => {
            log_line(&format!(
                "{} could not mount {}: {}",
                sys::fmt_time("%F %T"),
                dev,
                e
            ));
            false
        }
    }
}

/// The cartridge was pulled: stop speaking, drop the queue and erase every
/// spoken line. The boot/start text survives until this first removal.
pub fn forget_dialogue() {
    let _ = fs::write(crate::voice::QUEUE, "");
    let _ = fs::write(crate::voice::NOW, "\n");
    let _ = fs::remove_file(crate::voice::LOG);
    let _ = fs::remove_file(crate::voice::MAP);
    let _ = fs::remove_file(crate::voice::START);
    let _ = fs::remove_dir_all("/var/lib/cartridge/sounds");
    let _ = fs::remove_dir_all("/var/lib/cartridge/voice");
    for kind in ["complete", "failed", "copying"] {
        let _ = fs::remove_file(format!("/var/lib/cartridge/current-{}.wav", kind));
    }
    let _ = sys::umount_lazy(crate::voice::MOUNT);
    let _ = std::process::Command::new("systemctl")
        .args(["kill", "--signal=SIGTERM", "cartridge-voiced.service"])
        .status();
    log_line(&format!(
        "{} cartridge removed; dialogue cleared",
        sys::fmt_time("%F %T")
    ));
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

    let mut log_pos = fs::metadata(IMPORT_LOG).map(|m| m.len()).unwrap_or(0);
    let mut was_present = !sys::usb_list().is_empty();

    loop {
        if !console_ok && probe_console() {
            console_ok = true;
        }
        let present = !sys::usb_list().is_empty();
        if was_present && !present {
            forget_dialogue();
        }
        if present {
            mount_cartridge();
        }
        was_present = present;
        // Redraw every second so the spoken transcript grows as it is heard.
        if art_done() {
            draw_status();
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
    }
}
