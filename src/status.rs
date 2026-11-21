//! `cartridge status`: a short, colour-coded report of what the machine sees.
//! Read-only: it mounts an unmounted cartridge read-only and never writes.

use crate::machine;
use crate::sys;
use crate::theme::{Role, Theme};
use std::fs;
use std::path::{Path, PathBuf};

const LOG: &str = "/var/log/cartridge/import.log";
const LIVE: &str = "/srv/cartridge/live";

extern "C" {
    fn isatty(fd: i32) -> i32;
    fn geteuid() -> u32;
}

fn use_color() -> bool {
    let mut on = unsafe { isatty(1) } == 1
        && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(false)
        && std::env::var("NO_COLOR").is_err();
    match std::env::var("CARTRIDGE_COLOR").ok().as_deref() {
        Some("always") => on = true,
        Some("never") => on = false,
        _ => {}
    }
    on
}

struct Out {
    t: Theme,
    on: bool,
}

impl Out {
    fn p(&self, role: Role, text: &str) -> String {
        if self.on {
            self.t.paint(role, text)
        } else {
            text.to_string()
        }
    }

    fn head(&self, name: &str) {
        println!();
        println!(
            "{} {}",
            self.p(Role::Title, &format!("{:<7}", name)),
            self.p(Role::Dim, &"-".repeat(52usize.saturating_sub(name.len())))
        );
    }

    fn kv(&self, key: &str, value: &str) {
        println!("  {} {}", self.p(Role::Dim, &format!("{:<6}", key)), value);
    }

    fn line(&self, text: &str) {
        println!("  {}", text);
    }
}

fn site_url(ip: &str, port: &str) -> String {
    if port == "80" {
        format!("http://{}/", ip)
    } else {
        format!("http://{}:{}", ip, port)
    }
}

/// Machine and staging facts: what the box has and where the sites were put.
fn machine_block(o: &Out) {
    o.head("machine");
    let (drives, capacity) = machine::hardware();
    let (store_total, store_used, store_left) = machine::storage();
    o.kv(
        "drives",
        &format!(
            "{} internal drive(s), {} internal storage",
            drives,
            sys::human(capacity)
        ),
    );
    o.kv(
        "storage",
        &if store_total > 0 {
            format!(
                "{} used, {} left of {} (OS and server included)",
                sys::human(store_used),
                sys::human(store_left),
                sys::human(store_total)
            )
        } else {
            o.p(Role::Warn, "usage unavailable")
        },
    );
    o.kv(
        "ram",
        &format!(
            "{} total, {} available",
            sys::human(machine::mem_total_kb() * 1024),
            sys::human(machine::mem_avail_kb() * 1024)
        ),
    );
    o.kv("staging", &machine::summary());
}

fn inspect_cartridge(o: &Out, _dev: &str, mount: &Path) -> bool {
    let conf = mount.join("config.json");
    if !conf.is_file() {
        return false;
    }
    o.line(&format!("{} cartridge", o.p(Role::Ok, "*")));
    let (drives, capacity) = machine::hardware();
    let (store_total, store_used, store_left) = machine::storage();
    o.kv(
        "machine",
        &format!(
            "{} internal drive(s), {} internal storage, {} RAM (limits are automatic)",
            drives,
            sys::human(capacity),
            sys::human(machine::mem_total_kb() * 1024)
        ),
    );
    o.kv(
        "storage",
        &if store_total > 0 {
            format!(
                "{} used, {} left of {} (OS and server included)",
                sys::human(store_used),
                sys::human(store_left),
                sys::human(store_total)
            )
        } else {
            o.p(Role::Warn, "usage unavailable")
        },
    );

    // The live release is on the internal disk. Measure that, never the
    // cartridge mount: the cartridge is removable and is not storage here.
    let live = Path::new(LIVE);
    let mut sites = 0usize;
    let mut rows: Vec<[String; 4]> = Vec::new();
    let mut total_kb = 0u64;
    let ip = sys::lan_ip();

    let mut dirs: Vec<PathBuf> = fs::read_dir(live)
        .map(|it| it.flatten().filter(|e| e.path().is_dir()).map(|e| e.path()).collect())
        .unwrap_or_default();
    dirs.sort();
    for dir in dirs {
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let kb = sys::disk_kb(&dir);
        sites += 1;
        total_kb += kb;
        rows.push([
            format!("`{}`", name),
            format!("`{}/`", name),
            sys::human_kb(kb),
            site_url(&ip, &name),
        ]);
    }
    let index = live.join("index.html");
    if index.is_file() {
        sites += 1;
        let size = fs::metadata(&index).map(|m| m.len()).unwrap_or(0);
        total_kb += size.div_ceil(1024);
        rows.push([
            "`80`".into(),
            "`/`".into(),
            sys::human(size),
            site_url(&ip, "80"),
        ]);
    }
    o.kv(
        "total",
        &format!("{} website(s), {}", sites, sys::human_kb(total_kb)),
    );
    if !rows.is_empty() {
        site_table(o, &rows);
    }

    let boot = mount.join("boot.png");
    if boot.is_file() {
        o.kv(
            "art",
            "boot.png",
        );
    } else {
        o.kv("art", &o.p(Role::Dim, "no boot.png (keeping the current art)"));
    }

    let free_kb = sys::filesystem_bytes("/srv/cartridge")
        .map(|(_, _, free)| free / 1024)
        .unwrap_or(u64::MAX);
    // RAM never refuses: publish stages what fits and links the rest.
    let verdict = if total_kb + 131_072 > free_kb {
        format!(
            "refuse: {} > {} free on internal storage",
            sys::human_kb(total_kb),
            sys::human_kb(free_kb)
        )
    } else {
        "accept - RAM staging is automatic".to_string()
    };
    if verdict.starts_with("accept") {
        o.kv("verdict", &o.p(Role::Ok, &verdict));
    } else {
        o.kv("verdict", &o.p(Role::Err, &verdict));
    }
    true
}

/// Sites and their ports as a Markdown table: header, alignment row, then one
/// padded row per site with backticked values.
fn site_table(o: &Out, rows: &[[String; 4]]) {
    let head = ["Port".to_string(), "Folder".to_string(), "Size".to_string(), "URL".to_string()];
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
    o.line(&cell(&head));
    o.line(&format!(
        "|:{}|:{}|{}:|:{}|",
        "-".repeat(width[0] + 1),
        "-".repeat(width[1] + 1),
        "-".repeat(width[2] + 1),
        "-".repeat(width[3] + 1)
    ));
    for row in rows {
        o.line(&cell(row));
    }
}

/// `cartridge sites`: the release that is live right now, as the same table.
pub fn sites_cmd() -> Result<i32, String> {
    let o = Out { t: Theme::load(), on: use_color() };
    let live = Path::new(LIVE);
    let ip = sys::lan_ip();
    let mut rows: Vec<[String; 4]> = Vec::new();
    if let Ok(entries) = fs::read_dir(live) {
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.path())
            .collect();
        dirs.sort();
        for dir in dirs {
            let name = dir.file_name().unwrap().to_string_lossy().to_string();
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            rows.push([
                format!("`{}`", name),
                format!("`{}/`", name),
                sys::human_kb(sys::disk_kb(&dir)),
                site_url(&ip, &name),
            ]);
        }
    }
    if live.join("index.html").is_file() {
        let size = fs::metadata(live.join("index.html")).map(|m| m.len()).unwrap_or(0);
        rows.push([
            "`80`".into(),
            "`/`".into(),
            sys::human(size),
            site_url(&ip, "80"),
        ]);
    }
    o.head("sites");
    if rows.is_empty() {
        o.kv("sites", &o.p(Role::Warn, "none published"));
        return Ok(0);
    }
    site_table(&o, &rows);
    Ok(0)
}

fn mount_target(dev: &str) -> Option<String> {
    let info = fs::read_to_string("/proc/self/mountinfo").ok()?;
    for line in info.lines() {
        let f: Vec<&str> = line.split(' ').collect();
        if f.len() > 4 && f[1] == dev {
            return Some(f[4].to_string());
        }
    }
    None
}

fn usb_block(o: &Out) {
    o.head("usb");
    let devices = sys::usb_list();
    if devices.is_empty() {
        o.line(&o.p(Role::Dim, "no removable device attached"));
        return;
    }
    let root = unsafe { geteuid() } == 0;
    for d in devices {
        o.kv(
            &d.dev,
            &format!("{}  {}  {}", d.size, d.label, d.fstype),
        );
        if let Some(target) = mount_target(&d.dev) {
            o.line(&format!("{} mounted at {}", d.dev, target));
            inspect_cartridge(o, &d.dev, Path::new(&target));
            continue;
        }
        if !root {
            o.line(&o.p(Role::Warn, "not mounted - run with sudo to inspect it"));
            continue;
        }
        let tmp = format!("/run/cartridge-status.{}", std::process::id());
        let _ = fs::remove_dir_all(&tmp);
        if fs::create_dir_all(&tmp).is_err() {
            continue;
        }
        match sys::mount_ro(&d.dev, &tmp) {
            Ok(()) => {
                if !inspect_cartridge(o, &d.dev, Path::new(&tmp)) {
                    o.line(&o.p(Role::Warn, "not a cartridge (no config.json)"));
                }
                let _ = sys::umount(&tmp);
            }
            Err(_) => o.line(&o.p(Role::Err, "could not be read")),
        }
        let _ = fs::remove_dir(&tmp);
    }
}

fn listening_ports() -> Vec<u16> {
    let mut ports: Vec<u16> = Vec::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(text) = fs::read_to_string(file) {
            for line in text.lines().skip(1) {
                let fields: Vec<&str> = line.split_whitespace().collect();
                if fields.len() > 3 && fields[3] == "0A" {
                    if let Some((_, port)) = fields[1].rsplit_once(':') {
                        if let Ok(p) = u16::from_str_radix(port, 16) {
                            ports.push(p);
                        }
                    }
                }
            }
        }
    }
    ports.sort_unstable();
    ports.dedup();
    ports
}

fn serving_block(o: &Out) {
    o.head("serving");
    let live = Path::new(LIVE);
    if live.exists() {
        let target = fs::read_link(live)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        o.kv("live", &o.p(Role::Accent, target.trim_end_matches('/')));
        let mut ports: Vec<String> = fs::read_dir(live)
            .map(|it| {
                it.flatten()
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
                    .collect()
            })
            .unwrap_or_default();
        ports.sort_by_key(|p| p.parse::<u32>().unwrap_or(0));
        o.kv(
            "ports",
            &if ports.is_empty() { "none".to_string() } else { ports.join("  ") },
        );
        if !ports.is_empty() {
            let ip = sys::lan_ip();
            let urls: Vec<String> = ports.iter().map(|p| site_url(&ip, p)).collect();
            o.kv("urls", &o.p(Role::Accent, &urls.join("  ")));
        }
    } else {
        o.kv("live", &o.p(Role::Warn, "nothing published"));
    }
    let pid = sys::read_trim("/run/cartridge-web.pid").and_then(|p| p.parse::<i32>().ok());
    let active = pid.map(|p| Path::new(&format!("/proc/{}", p)).exists()).unwrap_or(false);
    o.kv(
        "web",
        &if active {
            o.p(Role::Ok, "active")
        } else {
            o.p(Role::Err, "inactive")
        },
    );
    let ports: Vec<String> = listening_ports()
        .into_iter()
        .filter(|p| *p != 22 && *p != 53)
        .map(|p| p.to_string())
        .collect();
    if !ports.is_empty() {
        o.kv("listen", &ports.join("  "));
    }
}

/// What the cluster daemon reports: this node, the peers on the switch, and how
/// the load is being shared right now. Silent when cluster mode is off.
fn cluster_block(o: &Out) {
    let state = match crate::cluster::state() {
        Some(v) => v,
        None => return,
    };
    let text = |k: &str| state.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let num = |k: &str| state.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    o.head("cluster");
    let release = text("release");
    o.kv(
        "node",
        &format!(
            "`{}` · {} worker(s) · release {}",
            text("node"),
            num("workers"),
            if release.is_empty() { "none".to_string() } else { release }
        ),
    );
    o.kv(
        "load",
        &format!(
            "{} served now, {} handed to peers, {} served since boot",
            num("busy"),
            num("forwarded"),
            num("served")
        ),
    );
    o.kv("speed", &format!("{:.1} ms to first byte", num("tfa") as f64 / 10.0));
    let empty: Vec<crate::json::Value> = Vec::new();
    let peers = state.get("peers").and_then(|v| v.as_arr()).unwrap_or(&empty);
    if peers.is_empty() {
        o.kv("peers", &o.p(Role::Dim, "none on the switch yet"));
        return;
    }
    let mut synced = 0usize;
    o.kv("peers", &format!("{} on the switch", peers.len()));
    for peer in peers {
        let ip = peer.get("ip").and_then(|v| v.as_str()).unwrap_or("?");
        let in_sync = peer.get("in_sync").and_then(|v| v.as_bool()).unwrap_or(false);
        if in_sync {
            synced += 1;
        }
        let release = peer.get("release").and_then(|v| v.as_str()).unwrap_or("");
        o.line(&format!(
            "{:<15} {} · {} worker(s) · {} busy · {:.1} ms · {}",
            ip,
            if release.is_empty() { "no release" } else { release },
            peer.get("workers").and_then(|v| v.as_i64()).unwrap_or(0),
            peer.get("busy").and_then(|v| v.as_i64()).unwrap_or(0),
            peer.get("tfa").and_then(|v| v.as_i64()).unwrap_or(0) as f64 / 10.0,
            if in_sync {
                o.p(Role::Ok, "in sync")
            } else {
                o.p(Role::Warn, "out of sync")
            }
        ));
    }
    o.kv(
        "synced",
        &format!("{} of {} peer(s) serve the same release", synced, peers.len()),
    );
}

fn history_block(o: &Out) {
    o.head("last import");
    match fs::read_to_string(LOG) {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().collect();
            let start = lines.len().saturating_sub(10);
            for line in &lines[start..] {
                let role = if line.contains("err") || line.contains("refus") {
                    Role::Err
                } else if line.contains("warn") {
                    Role::Warn
                } else if line.contains("ok") {
                    Role::Ok
                } else {
                    Role::Text
                };
                if o.on {
                    println!("  {}", o.p(role, line));
                } else {
                    println!("  {}", line);
                }
            }
        }
        Err(_) => o.line(&o.p(Role::Dim, "no import log yet")),
    }
}

fn report(o: &Out) {
    println!(
        "\n{} {}",
        o.p(Role::Title, "cartridge"),
        o.p(Role::Dim, &sys::fmt_time("%Y-%m-%d %H:%M:%S"))
    );
    machine_block(o);
    usb_block(o);
    serving_block(o);
    cluster_block(o);
    history_block(o);
    println!();
}

pub fn run(watch: bool) -> Result<i32, String> {
    let o = Out {
        t: Theme::load(),
        on: use_color(),
    };
    if !watch {
        report(&o);
        return Ok(0);
    }
    println!("watching for USB and release changes - Ctrl-C to stop");
    let mut last = String::new();
    loop {
        let signature = format!(
            "{:?}|{:?}",
            sys::usb_list().iter().map(|u| u.dev.clone()).collect::<Vec<_>>(),
            fs::read_link(LIVE).ok()
        );
        if signature != last {
            report(&o);
            last = signature;
        }
        sys::sleep_ms(2000);
    }
}
