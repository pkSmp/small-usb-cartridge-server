//! The report under the art: a Markdown table of every live site (port,
//! folder, size, URL) and the machine and staging facts. Mirrors the import
//! log to /var/log/cartridge/console.log.

use crate::art;
use crate::machine;
use crate::sys;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

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

/// What the report re-reads, and how often. The frame is drawn once a second,
/// but the release tree, the theme and the cluster state change far more slowly
/// than that, and every one of those reads is CPU the web tier should have.
struct Snapshot {
    theme: crate::theme::Theme,
    theme_stamp: (Option<SystemTime>, Option<SystemTime>),
    sites: Vec<[String; 4]>,
    total_kb: u64,
    sites_at: Option<Instant>,
    usb: Vec<sys::Usb>,
    usb_at: Option<Instant>,
    cluster: Option<String>,
    cluster_at: Option<Instant>,
}

const SITES_TTL: Duration = Duration::from_secs(5);
const USB_TTL: Duration = Duration::from_secs(2);
const CLUSTER_TTL: Duration = Duration::from_secs(2);

fn stamps() -> (Option<SystemTime>, Option<SystemTime>) {
    let at = |p: &str| fs::metadata(p).and_then(|m| m.modified()).ok();
    (
        at("/var/lib/cartridge/art/map.json"),
        at("/var/lib/cartridge/calib.json"),
    )
}

impl Snapshot {
    fn new() -> Snapshot {
        Snapshot {
            theme: crate::theme::Theme::console(),
            theme_stamp: stamps(),
            sites: Vec::new(),
            total_kb: 0,
            sites_at: None,
            usb: Vec::new(),
            usb_at: None,
            cluster: None,
            cluster_at: None,
        }
    }

    fn aged(at: Option<Instant>, now: Instant, ttl: Duration) -> bool {
        at.map(|t| now.duration_since(t) >= ttl).unwrap_or(true)
    }

    fn refresh(&mut self) {
        let now = Instant::now();
        // The palette only changes when the art is rebuilt; a stat pair is a
        // hundred times cheaper than parsing map.json and calib.json again.
        let stamp = stamps();
        if stamp != self.theme_stamp {
            self.theme = crate::theme::Theme::console();
            self.theme_stamp = stamp;
        }
        if Snapshot::aged(self.sites_at, now, SITES_TTL) {
            let (rows, kb) = live_rows();
            self.sites = rows;
            self.total_kb = kb;
            self.sites_at = Some(now);
        }
        if Snapshot::aged(self.usb_at, now, USB_TTL) {
            self.usb = sys::usb_list();
            self.usb_at = Some(now);
        }
        if Snapshot::aged(self.cluster_at, now, CLUSTER_TTL) {
            self.cluster = cluster_line(&self.theme);
            self.cluster_at = Some(now);
        }
    }
}

fn draw_status(s: &mut Snapshot) {
    let (rows, _cols) = terminal_size();
    let heights = fs::read_to_string(ART_ROWS)
        .ok()
        .and_then(|t| t.lines().next().and_then(|v| v.trim().parse::<usize>().ok()))
        .unwrap_or_else(art_height);
    let h = heights;

    s.refresh();
    let theme = &s.theme;
    theme.install();
    use crate::theme::Role;
    let stamp = theme.paint(Role::Dim, &sys::fmt_time("%H:%M:%S"));
    let usb = &s.usb;
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

    let sites = &s.sites;
    let total_kb = s.total_kb;
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
        theme.paint(Role::Text, &machine::summary())
    ));
    if let Some(line) = &s.cluster {
        block.push(line.clone());
    }
    block.push(theme.paint(Role::Dim, "login   Ctrl+Alt+F2"));

    // Invisible guard, no change to what is drawn: the report keeps its own
    // scroll region, so a long line wrapping at the bottom cannot scroll the
    // boot art off the top of the screen.
    let top = (h + 2).clamp(1, rows.saturating_sub(1).max(1));
    let mut out = String::new();
    out.push_str(&format!("\x1b[{};{}r", top, rows));
    let mut r = top;
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
    let mut timed = false;
    let mut snap = Snapshot::new();

    loop {
        if !console_ok && probe_console() {
            console_ok = true;
        }
        // Redraw every second so USB and release changes appear.
        if art_done() {
            let start = Instant::now();
            draw_status(&mut snap);
            if !timed {
                timed = true;
                log_line(&format!(
                    "text: one status frame took {} us on the cpu",
                    start.elapsed().as_micros()
                ));
            }
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
