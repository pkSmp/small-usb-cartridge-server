//! `cartridge status`: a short, colour-coded report of what the machine sees.
//! Read-only: it mounts an unmounted cartridge read-only and never writes.

use crate::config;
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

fn sound_present(name: &str) -> bool {
    Path::new(name).is_file()
}

fn audio_block(o: &Out) {
    o.head("audio");
    let cards = fs::read_to_string("/proc/asound/cards").unwrap_or_default();
    let mut named: Vec<String> = Vec::new();
    for line in cards.lines() {
        let t = line.trim_start();
        if t.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) && t.contains('[') {
            named.push(t.split_whitespace().collect::<Vec<_>>().join(" "));
        }
    }
    if named.is_empty() {
        o.kv("card", &o.p(Role::Warn, "none - the chime is skipped"));
    } else {
        for n in named.iter().take(2) {
            o.kv("card", n);
        }
        let pcm = fs::read_to_string("/proc/asound/pcm").unwrap_or_default();
        if let Some(line) = pcm.lines().find(|l| l.contains("playback")) {
            let name = line.split(':').nth(1).unwrap_or("").trim();
            if !name.is_empty() {
                o.kv("output", name);
            }
        }
    }
    let complete = crate::sound::builtin("complete");
    let failed = crate::sound::builtin("failed");
    let size = |p: &str| fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    if sound_present(&complete) && sound_present(&failed) {
        o.kv(
            "chime",
            &format!(
                "{} complete · {} failed",
                sys::human(size(&complete)),
                sys::human(size(&failed))
            ),
        );
    } else {
        o.kv("chime", &o.p(Role::Warn, "not generated yet (run an import)"));
    }
}

fn inspect_cartridge(o: &Out, _dev: &str, mount: &Path) -> bool {
    let conf = mount.join("config.json");
    if !conf.is_file() {
        return false;
    }
    let config = config::load(&conf).unwrap_or_default();
    o.line(&format!("{} cartridge", o.p(Role::Ok, "*")));
    o.kv(
        "limits",
        &format!("storage {} MB · RAM {} MB", config.storage_mb, config.ram_mb),
    );

    let mut ports: Vec<String> = Vec::new();
    let mut subtotal = 0u64;
    let mut sites = 0usize;
    let mut dirs: Vec<PathBuf> = fs::read_dir(mount)
        .map(|it| it.flatten().filter(|e| e.path().is_dir()).map(|e| e.path()).collect())
        .unwrap_or_default();
    dirs.sort();
    for dir in dirs {
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let kb = sys::apparent_kb(&dir);
        subtotal += kb;
        sites += 1;
        o.kv("site", &format!("port {:<6} {}", name, sys::human_kb(kb)));
        ports.push(name);
    }
    if mount.join("index.html").is_file() {
        sites += 1;
        o.kv("site", "port 80     (index.html at the root)");
        ports.push("80".into());
    }
    o.kv(
        "total",
        &format!("{} website(s), {}", sites, sys::human_kb(sys::apparent_kb(mount))),
    );
    if !ports.is_empty() {
        let ip = sys::lan_ip();
        for (i, port) in ports.iter().enumerate() {
            let url = o.p(Role::Accent, &site_url(&ip, port));
            if i == 0 {
                o.kv("test", &url);
            } else {
                o.line(&format!("     {}", url));
            }
        }
    }

    for (label, name) in [("complete", &config.complete_sound), ("failed", &config.failed_sound)] {
        let p = mount.join(name);
        if p.is_file() {
            let size = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            let supported = name.to_ascii_lowercase().ends_with(".wav");
            o.kv(
                label,
                &format!(
                    "{} {}{}",
                    name,
                    sys::human(size),
                    if supported { "" } else { " - not WAV, the built-in chime is used" }
                ),
            );
        } else {
            o.kv(label, &o.p(Role::Dim, &format!("{} (built-in chime used)", name)));
        }
    }

    let boot = mount.join("boot.png");
    if boot.is_file() {
        o.kv(
            "art",
            &format!("boot.png {}", sys::human(fs::metadata(&boot).map(|m| m.len()).unwrap_or(0))),
        );
    } else {
        o.kv("art", &o.p(Role::Dim, "no boot.png (keeping the current art)"));
    }

    let total_kb = sys::apparent_kb(mount);
    let total_mb = (total_kb + 1023) / 1024;
    let verdict = if config.ram_mb > config.storage_mb {
        format!("refuse: ram_mb > storage_mb")
    } else if total_mb > config.storage_mb {
        format!("refuse: {} MB > storage {} MB", total_mb, config.storage_mb)
    } else if total_mb > config.ram_mb {
        format!("refuse: {} MB > RAM {} MB", total_mb, config.ram_mb)
    } else {
        "accept".to_string()
    };
    if verdict.starts_with("accept") {
        o.kv("verdict", &o.p(Role::Ok, &verdict));
    } else {
        o.kv("verdict", &o.p(Role::Err, &verdict));
    }
    true
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
    audio_block(o);
    usb_block(o);
    serving_block(o);
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
