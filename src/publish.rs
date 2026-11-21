//! Publish the live release: stage as much of it into RAM as fits safely and
//! leave the rest on storage through symlinks. The whole release stays on disk
//! for the next boot. The budget is read from the machine, never from a file.

use crate::machine;
use crate::sys;
use std::fs;
use std::path::{Path, PathBuf};

const LIVE: &str = "/srv/cartridge/live";
const RAM_DIR: &str = "/run/cartridge";
const PID_FILE: &str = "/run/cartridge-web.pid";
const MB: u64 = 1024 * 1024;

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

#[derive(Default)]
struct Staged {
    total: u64,
    ram: u64,
    disk: u64,
    ram_files: u64,
    disk_files: u64,
    sites: u64,
}

/// Copy while the RAM budget lasts, then link the rest to the release on disk.
fn fill(src: &Path, dst: &Path, root: bool, used: &mut u64, budget: u64, st: &mut Staged) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("{}: {}", dst.display(), e))?;
    let entries = match fs::read_dir(src) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if root && name == ".cartridge" {
            continue;
        }
        let path = entry.path();
        let target = dst.join(&name);
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            fill(&path, &target, false, used, budget, st)?;
        } else if file_type.is_file() {
            let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
            st.total += len;
            if used.saturating_add(len) <= budget {
                match fs::copy(&path, &target) {
                    Ok(_) => {
                        *used += len;
                        st.ram += len;
                        st.ram_files += 1;
                        continue;
                    }
                    Err(_) => {}
                }
            }
            // Out of budget (or the copy failed): keep it on disk and link it.
            let real = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            let _ = fs::remove_file(&target);
            match std::os::unix::fs::symlink(&real, &target) {
                Ok(()) => {
                    st.disk += len;
                    st.disk_files += 1;
                }
                Err(e) => log(&format!("warning: could not link {}: {}", name, e)),
            }
        }
    }
    Ok(())
}

fn ports_of(root: &Path) -> Vec<u16> {
    let mut ports: Vec<u16> = Vec::new();
    if root.join("index.html").is_file() {
        ports.push(80);
    }
    if let Ok(entries) = fs::read_dir(root) {
        let mut found: Vec<u16> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            // One rule for folders, shared with the importer's listing.
            .filter_map(|e| crate::guard::port_folder(&e.file_name().to_string_lossy()))
            .collect();
        found.sort_unstable();
        found.dedup();
        ports.extend(found);
    }
    ports.sort_unstable();
    ports.dedup();
    ports
}

pub fn run() -> Result<i32, String> {
    fn measure(src: &Path, root: bool) -> u64 {
        let mut total = 0u64;
        if let Ok(entries) = fs::read_dir(src) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if root && name == ".cartridge" {
                    continue;
                }
                match entry.file_type() {
                    Ok(t) if t.is_dir() => total += measure(&entry.path(), false),
                    Ok(t) if t.is_file() => {
                        total += entry.metadata().map(|m| m.len()).unwrap_or(0)
                    }
                    _ => {}
                }
            }
        }
        total
    }

    let live = Path::new(LIVE);
    let release = fs::canonicalize(live).unwrap_or_else(|_| live.to_path_buf());

    // Measure the sites. The whole tree stays on disk for the next boot; only
    // the part that fits safely is mirrored into RAM.
    let total = measure(&release, true);
    let sites = ports_of(&release).len() as u64;

    let total_kb = machine::mem_total_kb();
    let avail_kb = machine::mem_avail_kb();
    // Never take more than half of RAM, and always keep a quarter in reserve.
    let reserve_kb = (total_kb / 4).max(262_144);
    let mut budget_kb = avail_kb.saturating_sub(reserve_kb).min(total_kb / 2);
    budget_kb = budget_kb.max(4096);
    // Test hook: pin the budget so the disk-overflow path can be exercised.
    if let Ok(v) = std::env::var("CARTRIDGE_RAM_BUDGET_MB") {
        if let Ok(mb) = v.parse::<u64>() {
            budget_kb = mb.max(1) * 1024;
        }
    }
    let budget_bytes = budget_kb * 1024;
    let size_mb = ((total / MB) + 8).max(8);

    if sys::is_mountpoint(RAM_DIR) {
        if let Err(e) = sys::umount(RAM_DIR) {
            log(&format!("warning: could not unmount the previous RAM copy: {}", e));
        }
    }
    let _ = fs::create_dir_all(RAM_DIR);
    let root = PathBuf::from(RAM_DIR);
    let mount_mb = size_mb.min((budget_bytes / MB).max(4));
    let mut mode = format!("{} in RAM, rest linked to disk", sys::human(total));
    if sys::mount_tmpfs(RAM_DIR, mount_mb).is_err() {
        // /run is itself a tmpfs, so the plain directory is still RAM.
        log("warning: tmpfs mount failed, staging inside /run directly");
        mode = "inside /run (RAM)".into();
    }

    if release.is_dir() {
        let mut used = 0u64;
        let mut st = Staged { total, sites, ..Default::default() };
        match fill(&release, &root, true, &mut used, budget_bytes, &mut st) {
            Ok(()) => {
                let report = format!(
                    "{{\"total_bytes\":{},\"ram_bytes\":{},\"disk_bytes\":{},\"ram_files\":{},\"disk_files\":{},\"budget_bytes\":{},\"mem_total_kb\":{},\"mem_avail_kb\":{},\"sites\":{}}}\n",
                    total, st.ram, st.disk, st.ram_files, st.disk_files, budget_bytes, total_kb, avail_kb, sites
                );
                let _ = fs::write(machine::USAGE, report);
                log(&format!(
                    "{} total: {} staged in RAM ({}), {} linked to disk ({}), budget {}{}",
                    sys::human(total),
                    sys::human(st.ram),
                    st.ram_files,
                    sys::human(st.disk),
                    st.disk_files,
                    sys::human(budget_bytes),
                    if st.disk == 0 { ", all sites fit in RAM" } else { "" }
                ));
            }
            Err(e) => log(&format!("warning: staging stopped early: {}", e)),
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
        let _ = fs::write(root.join("index.html"), NO_CARTRIDGE);
        let _ = fs::write(
            machine::USAGE,
            "{\"total_bytes\":0,\"ram_bytes\":0,\"disk_bytes\":0,\"ram_files\":0,\"disk_files\":0,\"sites\":0}\n",
        );
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
    // A last safety check: RAM must still be comfortably available.
    let left = machine::mem_avail_kb();
    if left < total_kb / 8 {
        log(&format!(
            "warning: only {} RAM free after staging; the web server may be short",
            sys::human(left * 1024)
        ));
    }
    log(&format!("serving {} ({}) · {}", root.display(), mode, machine::summary()));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_real_ports_are_published() {
        let dir = std::env::temp_dir().join(format!("cartridge-ports-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for name in ["8080", "443", "www", "80", "99999"] {
            fs::create_dir_all(dir.join(name)).unwrap();
        }
        fs::write(dir.join("index.html"), b"<html>").unwrap();
        assert_eq!(ports_of(&dir), vec![80, 8080]);

        // `80/` on its own is the same site as a root index.html, once.
        let only = dir.join("only80");
        fs::create_dir_all(only.join("80")).unwrap();
        assert_eq!(ports_of(&only), vec![80]);

        let _ = fs::remove_dir_all(&dir);
    }
}
