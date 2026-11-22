//! What the machine has: RAM, internal disks, storage and how much of the live
//! release is staged in RAM. Everything is read from /proc and /sys; none of it
//! is configured on the cartridge.

use crate::sys;
use std::fs;
use std::path::Path;

/// Written by publish, read by status and the console: how the release was split
/// between RAM and disk for this boot.
pub const USAGE: &str = "/run/cartridge-usage.json";
const LIVE: &str = "/srv/cartridge/live";

fn mem_kb(key: &str) -> u64 {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|t| {
            t.lines().find(|l| l.starts_with(key)).and_then(|l| {
                l.split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse::<u64>().ok())
            })
        })
        .unwrap_or(0)
}

pub fn mem_total_kb() -> u64 {
    mem_kb("MemTotal:")
}

pub fn mem_avail_kb() -> u64 {
    mem_kb("MemAvailable:")
}

/// True for a block disk that can be detached: USB buses, card readers and
/// anything the kernel marks removable. The cartridge itself is removable, so
/// it is excluded here too and can never inflate the machine's storage count.
fn removable_disk(name: &str) -> bool {
    let base = format!("/sys/block/{}", name);
    for flag in ["removable", "device/removable"] {
        if fs::read_to_string(format!("{}/{}", base, flag))
            .map(|v| v.trim() == "1")
            .unwrap_or(false)
        {
            return true;
        }
    }
    if let Ok(target) = fs::read_link(format!("{}/device", base)) {
        if target.to_string_lossy().to_ascii_lowercase().contains("usb") {
            return true;
        }
    }
    fs::read_to_string(format!("{}/device/transport", base))
        .map(|v| v.trim().eq_ignore_ascii_case("usb"))
        .unwrap_or(false)
}

/// Count internal disks only. Removable media (USB sticks, microSD and SD
/// cards, card readers) is deliberately left out of both the drive count and
/// the capacity total.
pub fn hardware() -> (u64, u64) {
    let mut count = 0u64;
    let mut bytes = 0u64;
    if let Ok(entries) = fs::read_dir("/sys/block") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let skip = ["loop", "ram", "zram", "sr", "fd", "dm-", "md", "nbd"]
                .iter()
                .any(|p| name.starts_with(p));
            if skip || removable_disk(&name) {
                continue;
            }
            let size = fs::read_to_string(entry.path().join("size"))
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);
            if size == 0 {
                continue;
            }
            count += 1;
            bytes += size * 512;
        }
    }
    (count, bytes)
}

fn dir_bytes(path: &Path) -> u64 {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if meta.is_dir() {
        let mut total = 0u64;
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                total += dir_bytes(&entry.path());
            }
        }
        total
    } else if meta.file_type().is_symlink() {
        fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    } else {
        meta.len()
    }
}

/// (storage total, used, left) in bytes for the internal filesystem. `used`
/// includes the OS, this binary and the site release, not just the sites.
pub fn storage() -> (u64, u64, u64) {
    sys::filesystem_bytes("/")
        .or_else(|| sys::filesystem_bytes("/srv/cartridge"))
        .unwrap_or((0, 0, 0))
}

/// (all site bytes, bytes staged in RAM, bytes left on storage, sites)
pub fn staging() -> (u64, u64, u64, u64) {
    if let Ok(text) = fs::read_to_string(USAGE) {
        if let Ok(v) = crate::json::parse(&text) {
            let num = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0) as u64;
            return (num("total_bytes"), num("ram_bytes"), num("disk_bytes"), num("sites"));
        }
    }
    let total = fs::canonicalize(LIVE).map(|p| dir_bytes(&p)).unwrap_or(0);
    let ram = dir_bytes(Path::new("/run/cartridge"));
    (total, ram, total.saturating_sub(ram), 0)
}

/// Every network interface except loopback, with its link state and the speed
/// the kernel negotiated: "eno0 1.0 Gb/s", "wlp2s0 up", "eno0 down".
fn links() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir("/sys/class/net")
        .map(|it| {
            it.flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let mut out = Vec::new();
    for name in names {
        if name == "lo" {
            continue;
        }
        let base = format!("/sys/class/net/{}", name);
        let state = fs::read_to_string(format!("{}/operstate", base))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        // A wired link only reports its speed while it is up, and wifi has no
        // speed file at all, so the link state is what is left to show there.
        let speed = fs::read_to_string(format!("{}/speed", base))
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|m| *m > 0);
        out.push(match (state.as_str(), speed) {
            ("up", Some(m)) => format!("{} {}", name, speed_text(m)),
            ("up", None) => format!("{} up", name),
            ("", _) => name,
            (s, _) => format!("{} {}", name, s),
        });
    }
    out
}

/// 1000 Mb/s reads as 1 Gb/s; anything slower stays in Mb/s.
fn speed_text(mbps: i64) -> String {
    if mbps >= 1000 {
        let gb = mbps as f64 / 1000.0;
        if (gb - gb.round()).abs() < 0.05 {
            format!("{:.0} Gb/s", gb)
        } else {
            format!("{:.1} Gb/s", gb)
        }
    } else {
        format!("{} Mb/s", mbps)
    }
}

/// One line for the log, the console and `status`.
pub fn summary() -> String {
    let (drives, capacity) = hardware();
    let ram_total = mem_total_kb() * 1024;
    let (total, ram, disk, _) = staging();
    let (store_total, store_used, store_left) = storage();
    let storage = if store_total > 0 {
        format!(
            "{} storage ({} used, {} left; OS and server included)",
            sys::human(store_total),
            sys::human(store_used),
            sys::human(store_left)
        )
    } else {
        format!("{} internal storage (usage unavailable)", sys::human(capacity))
    };
    let net = links();
    format!(
        "{} internal drive(s), {} · {} RAM · sites {} ({} in RAM, {} on disk) · {}{}",
        drives,
        storage,
        sys::human(ram_total),
        sys::human(total),
        sys::human(ram),
        sys::human(disk),
        if disk == 0 { "all sites fit in RAM" } else { "not all sites fit in RAM" },
        if net.is_empty() {
            String::new()
        } else {
            format!(" · {}", net.join(", "))
        }
    )
}

#[cfg(test)]
mod tests {
    use super::speed_text;

    #[test]
    fn wired_speeds_read_in_the_right_unit() {
        assert_eq!(speed_text(100), "100 Mb/s");
        assert_eq!(speed_text(1000), "1 Gb/s");
        assert_eq!(speed_text(2500), "2.5 Gb/s");
        assert_eq!(speed_text(10000), "10 Gb/s");
    }
}
