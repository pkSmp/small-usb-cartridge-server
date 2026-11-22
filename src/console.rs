//! The report under the art: a Markdown table of every live site (port,
//! folder, size, URL), the machine and staging facts, and the newest import
//! and boot messages as plain text. Mirrors the import log to
//! /var/log/cartridge/console.log.

use crate::art;
use crate::machine;
use crate::sys;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
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


/// The last few kilobytes of a log, without the partial first line.
fn tail_lines(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(mut f) = fs::File::open(path) {
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let skip = len.saturating_sub(8192);
        if f.seek(SeekFrom::Start(skip)).is_ok() {
            let mut buf = String::new();
            if f.read_to_string(&mut buf).is_ok() {
                let mut lines: Vec<&str> = buf.lines().collect();
                if skip > 0 && !lines.is_empty() {
                    lines.remove(0); // the seek may have landed mid-line
                }
                out = lines.into_iter().map(|l| l.to_string()).collect();
            }
        }
    }
    out
}

/// Wrap one line to the screen width without breaking words.
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

/// The message feed: the newest import and boot lines, oldest first. Both logs
/// start with a timestamp, so merging by time keeps them in the real order.
/// Nothing here waits for a device: a line is on screen on the next redraw.
fn recent_messages(theme: &crate::theme::Theme, cols: usize) -> Vec<String> {
    use crate::theme::Role;
    let width = cols.saturating_sub(2).max(20);
    let mut raw: Vec<(String, String, String)> = Vec::new();
    for (path, default_tag) in [
        ("/var/log/cartridge/import.log", "info"),
        ("/var/log/cartridge/art.log", "art"),
    ] {
        for line in tail_lines(path) {
            let line = line.trim_end();
            if line.len() < 21 {
                continue;
            }
            let stamp = &line[..19];
            let rest = line[19..].trim_start();
            let (tag, text) = match rest.split_once(char::is_whitespace) {
                Some((t, m)) if matches!(t, "info" | "ok" | "warn" | "err" | "skip") => {
                    (t.to_string(), m.trim_start().to_string())
                }
                _ => (default_tag.to_string(), rest.to_string()),
            };
            // The site table is drawn above, so its log copy is not a message.
            if text.is_empty() || text.starts_with('|') || text == "websites found:" {
                continue;
            }
            raw.push((stamp.to_string(), tag, text));
        }
    }
    raw.sort_by(|a, b| a.0.cmp(&b.0));
    let start = raw.len().saturating_sub(40);
    let mut out = Vec::new();
    for (stamp, tag, text) in &raw[start..] {
        let role = match tag.as_str() {
            "ok" => Role::Ok,
            "warn" => Role::Warn,
            "err" => Role::Err,
            "skip" | "art" => Role::Dim,
            _ => Role::Text,
        };
        let clock = if stamp.len() >= 19 { &stamp[11..19] } else { "--:--:--" };
        for part in wrap(&format!("{} {:<5} {}", clock, tag, text), width) {
            out.push(theme.paint(role, &part));
        }
    }
    out
}

/// One line about the cluster, when the daemon is running: how many boxes are
/// on the switch, how many serve this release, and where the load went.
fn cluster_line(theme: &crate::theme::Theme) -> Option<String> {
    use crate::theme::Role;
    let state = crate::cluster::state()?;
    let num = |k: &str| state.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    let mut peers = 0usize;
    let mut synced = 0usize;
    if let Some(list) = state.get("peers").and_then(|v| v.as_arr()) {
        peers = list.len();
        for peer in list {
            if peer.get("in_sync").and_then(|v| v.as_bool()).unwrap_or(false) {
                synced += 1;
            }
        }
    }
    let fleet = if peers == 0 {
        "alone on the switch".to_string()
    } else {
        format!("{} peer(s), {} in sync", peers, synced)
    };
    Some(format!(
        "{}  {}  {}  {}",
        theme.paint(Role::Dim, "cluster"),
        theme.paint(Role::Text, &fleet),
        theme.paint(Role::Text, &format!("{} here", num("busy"))),
        theme.paint(Role::Accent, &format!("{} forwarded", num("forwarded")))
    ))
}

/// The expensive part of the report: a recursive size walk per site. It is
/// reused until the live release changes, or for ten seconds at most.
struct Sites {
    live: String,
    rows: Vec<[String; 4]>,
    total_kb: u64,
}

fn live_target() -> String {
    fs::read_link("/srv/cartridge/live")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Console cells a line really uses: an SGR escape paints no cell.
fn visible_len(text: &str) -> usize {
    let mut n = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c2 in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c2) {
                        break;
                    }
                }
            }
            continue;
        }
        n += 1;
    }
    n
}

/// Cut one line to the console width, keeping escapes whole. A line that runs
/// past the edge wraps, and a wrap scrolls the screen, which used to push the
/// whole boot art off the top and leave the report stranded in two places.
fn fit_width(text: &str, width: usize) -> String {
    if visible_len(text) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut n = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            out.push(c);
            out.push(chars.next().unwrap_or('['));
            for c2 in chars.by_ref() {
                out.push(c2);
                if ('\x40'..='\x7e').contains(&c2) {
                    break;
                }
            }
            continue;
        }
        if n >= width {
            break;
        }
        out.push(c);
        n += 1;
    }
    out.push_str("\x1b[0m");
    out
}

fn draw_status(cache: &mut Option<Sites>, refresh: bool) {
    let (rows, cols) = terminal_size();
    let heights = fs::read_to_string(ART_ROWS)
        .ok()
        .and_then(|t| t.lines().next().and_then(|v| v.trim().parse::<usize>().ok()))
        .unwrap_or_else(art_height);
    let h = heights;

    let theme = crate::theme::Theme::console();
    theme.install();
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

    let live = live_target();
    if refresh || cache.as_ref().map(|c| c.live != live).unwrap_or(true) {
        let (rows, total_kb) = live_rows();
        *cache = Some(Sites { live, rows, total_kb });
    }
    let sites = cache.as_ref().unwrap();
    if sites.rows.is_empty() {
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
                &format!(
                    "{} site(s), {} live",
                    sites.rows.len(),
                    sys::human_kb(sites.total_kb)
                )
            )
        ));
        for line in table_lines(&sites.rows) {
            block.push(theme.paint(Role::Text, &line));
        }
    }
    // The machine line is the long one: wrap it here, with the label on the
    // first line only, instead of letting it run off the edge of the console.
    let label = "machine";
    let room_for_text = cols.saturating_sub(2).max(20).saturating_sub(label.len() + 2).max(10);
    for (i, part) in wrap(&machine::summary(), room_for_text).into_iter().enumerate() {
        if i == 0 {
            block.push(format!(
                "{}  {}",
                theme.paint(Role::Dim, label),
                theme.paint(Role::Text, &part)
            ));
        } else {
            block.push(format!(
                "{}  {}",
                " ".repeat(label.len()),
                theme.paint(Role::Text, &part)
            ));
        }
    }
    if let Some(line) = cluster_line(&theme) {
        block.push(line);
    }

    // Fill what is left with the newest messages, oldest at the top. A line
    // appears on the next redraw, so the screen keeps up with the import.
    let messages = recent_messages(&theme, cols);
    if !messages.is_empty() {
        let available = rows.saturating_sub(h + 2);
        let room = available.saturating_sub(block.len() + 2);
        if room > 0 {
            let start = messages.len().saturating_sub(room);
            block.push(String::new());
            block.push(theme.paint(Role::Accent, "messages"));
            for line in &messages[start..] {
                block.push(line.clone());
            }
        }
    }
    block.push(theme.paint(Role::Dim, "login   Ctrl+Alt+F2"));

    // Every write stays inside the report area: the scroll region stops a wrap
    // from scrolling the art away, and the width cut means there is nothing
    // left to wrap in the first place.
    let width = cols.saturating_sub(1).max(20);
    let top = (h + 2).clamp(1, rows.saturating_sub(1).max(1));
    let mut out = String::new();
    out.push_str(&format!("\x1b[{};{}r", top, rows));
    let mut r = top;
    for line in &block {
        if r > rows {
            break;
        }
        out.push_str(&format!("\x1b[{};1H\x1b[2K{}", r, fit_width(line, width)));
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
    let mut sites: Option<Sites> = None;
    let mut tick: u64 = 0;

    loop {
        if !console_ok && probe_console() {
            console_ok = true;
        }
        // Redraw four times a second so a new message is on screen at once.
        if art_done() {
            draw_status(&mut sites, tick % 40 == 0);
        }
        tick = tick.wrapping_add(1);

        // Mirror new import-log lines into the console log (the old tail -F).
        if let Ok(meta) = fs::metadata(IMPORT_LOG) {
            if meta.len() < log_pos {
                log_pos = 0;
            }
            if meta.len() > log_pos {
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

        sys::sleep_ms(250);
    }
}
