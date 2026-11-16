//! Publish the current cartridge release: copy it into RAM and let the web
//! service pick it up. Replaces cartridge-publish + render-nginx.py.

use crate::config::Config;
use crate::sys;
use std::fs;
use std::path::{Path, PathBuf};

const STORE: &str = "/srv/cartridge";
const LIVE: &str = "/srv/cartridge/live";
const RAM_DIR: &str = "/run/cartridge";
const PID_FILE: &str = "/run/cartridge-web.pid";

const NO_CARTRIDGE: &str = "<!doctype html>\n<html lang=\"en\">\n<head><meta charset=\"utf-8\"><title>No cartridge</title></head>\n<body style=\"font-family:system-ui,sans-serif;background:#111;color:#eee;display:grid;place-items:center;height:100vh;margin:0\">\n<div style=\"text-align:center\">\n<h1 style=\"font-weight:600;margin:0 0 .4rem\">No cartridge inserted</h1>\n<p style=\"color:#888;margin:0\">Insert a cartridge to publish a site.</p>\n</div>\n</body>\n</html>\n";

fn log(text: &str) {
    eprintln!("cartridge-publish: {}", text);
}

fn notify_web() {
    if let Some(pid) = sys::read_trim(PID_FILE).and_then(|p| p.parse::<i32>().ok()) {
        unsafe {
            extern "C" {
                fn kill(pid: i32, sig: i32) -> i32;
            }
            kill(pid, 1); // SIGHUP
        }
    }
}

pub fn run() -> Result<i32, String> {
    let live = Path::new(LIVE);
    let config = crate::config::load(&live.join(".cartridge/config.json")).unwrap_or_default();
    let size_mb = config.ram_mb.max(1);
    let mut mode = format!("RAM copy ({}MB tmpfs)", size_mb);

    if sys::is_mountpoint(RAM_DIR) {
        if let Err(e) = sys::umount(RAM_DIR) {
            log(&format!("warning: could not unmount the previous RAM copy: {}", e));
        }
    }
    let _ = fs::create_dir_all(RAM_DIR);

    let root = PathBuf::from(RAM_DIR);
    match sys::mount_tmpfs(RAM_DIR, size_mb) {
        Ok(()) => {}
        Err(e) => {
            // /run is itself a tmpfs, so the plain directory is still RAM.
            log(&format!("warning: tmpfs mount failed ({}), copying into /run directly", e));
            mode = "RAM copy (/run)".into();
        }
    }

    if live.is_dir() {
        if let Err(e) = sys::copy_tree(live, &root, &[".cartridge".to_string()], true) {
            log(&format!("warning: cartridge is larger than the {}MB RAM copy: {}", size_mb, e));
        }
    } else if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let _ = fs::remove_dir_all(&path);
            } else {
                let _ = fs::remove_file(&path);
            }
        }
    }

    let empty = fs::read_dir(&root)
        .map(|mut it| it.next().is_none())
        .unwrap_or(true);
    if empty {
        true_or_false(&root);
    }

    let ports = ports_of(&root);
    let ports_text = ports
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let previous = sys::read_trim("/run/cartridge-web.ports").unwrap_or_default();
    if previous != ports_text {
        let _ = fs::write("/run/cartridge-web.ports", format!("{}\n", ports_text));
        notify_web();
    }
    log(&format!("serving {} ({})", root.display(), mode));
    Ok(0)
}

/// The same rule the web server uses: a numeric folder is a port, and port 80
/// comes from a folder named 80 or an index.html at the root.
fn ports_of(root: &Path) -> Vec<u16> {
    let mut ports: Vec<u16> = Vec::new();
    if root.join("80").is_dir() || root.join("index.html").is_file() {
        ports.push(80);
    }
    if let Ok(entries) = fs::read_dir(root) {
        let mut found: Vec<u16> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_string_lossy().parse::<u16>().ok())
            .filter(|p| (1024..=65535).contains(p))
            .collect();
        found.sort_unstable();
        ports.extend(found);
    }
    ports
}

fn true_or_false(root: &Path) {
    let _ = fs::write(root.join("index.html"), NO_CARTRIDGE);
}
