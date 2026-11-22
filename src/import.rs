//! Ingest a cartridge volume: mount it read-only, validate it, copy it into a
//! new release, switch the live symlink, prune old releases, rebuild the boot
//! art. Replaces cartridge-import.

use crate::json::{self, Value};
use crate::sys;
use std::fs;
use std::path::{Path, PathBuf};

const STORE: &str = "/srv/cartridge";
const RELEASES: &str = "/srv/cartridge/releases";
const LIVE: &str = "/srv/cartridge/live";
const STATE: &str = "/var/lib/cartridge";
const USB_MNT: &str = "/run/cartridge-usb";
const LOG_FILE: &str = "/var/log/cartridge/import.log";
const KEEP_RELEASES: usize = 2;

fn emit(tag: &str, message: &str) {
    let line = format!("{}  {:<5} {}\n", sys::fmt_time("%F %T"), tag, message);
    eprintln!("cartridge-import  {:<5} {}", tag, message);
    sys::append(LOG_FILE, &line);
}

fn log(message: &str) {
    emit("info", message);
}

fn ok(message: &str) {
    emit("ok", message);
}

fn warn(message: &str) {
    emit("warn", message);
}

fn err(message: &str) {
    emit("err", message);
}

fn skip(message: &str) {
    emit("skip", message);
}

/// config.json marks a cartridge: any JSON object is accepted, so the stick can
/// carry its own notes without the server reading them.
fn check_config(path: &Path) -> Result<(), String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    match json::parse(&text).map_err(|e| format!("{}: {}", path.display(), e))? {
        Value::Obj(_) => Ok(()),
        _ => Err("config.json must contain a JSON object".into()),
    }
}

fn rotate_log() {
    if let Ok(meta) = fs::metadata(LOG_FILE) {
        if meta.len() > 1_048_576 {
            if let Ok(text) = fs::read_to_string(LOG_FILE) {
                let lines: Vec<&str> = text.lines().collect();
                let start = lines.len().saturating_sub(500);
                let _ = fs::write(LOG_FILE, lines[start..].join("\n") + "\n");
            }
        }
    }
}

/// Rebuild the console art from the stored boot.png and hand it to the boot
/// service, which draws it and checks the framebuffer.
fn rebuild_art() {
    match crate::art::cmd_build("/var/lib/cartridge/boot.png", "/var/lib/cartridge/art") {
        Ok(0) => {
            ok("boot art rebuilt from boot.png");
            // Draw in-process: this binary *is* the boot-art program, so a
            // udev-triggered import has no reason to fork a second copy of it.
            let _ = crate::bootart::run();
        }
        _ => {
            log("warning: boot.png could not be converted");
        }
    }
}

fn free_kb(path: &str) -> u64 {
    // `df` was an external program for one number the kernel already answers.
    crate::sys::filesystem_bytes(path)
        .map(|(_, _, free)| free / 1024)
        .unwrap_or(u64::MAX)
}

fn release_device(mnt: &str, mounted: bool) {
    if mounted && sys::is_mountpoint(mnt) {
        sys::sync();
        if let Err(e) = sys::umount(mnt) {
            log(&format!("warning: could not unmount {}: {}", mnt, e));
        }
    }
    if mnt != USB_MNT {
        let _ = fs::remove_dir(mnt);
    }
}

fn existing_mount(dev: &str) -> Option<String> {
    let info = fs::read_to_string("/proc/self/mountinfo").ok()?;
    for line in info.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        if fields.len() < 5 {
            continue;
        }
        if fields[1] == dev {
            return Some(fields[4].to_string());
        }
    }
    None
}

fn free_name(template: &str) -> String {
    let base = format!(
        "{}.{:05}",
        template,
        (sys::now_secs().unsigned_abs() % 100000) as u32
    );
    base
}

fn fmt_bytes(bytes: u64) -> String {
    sys::human(bytes)
}

/// Copy a cartridge tree, but when the live release already holds a file with
/// the same name and the same SHA-256, hard-link it instead of writing the
/// bytes again. Returns (reused, written, bytes not rewritten).
fn copy_dedup(
    src: &Path,
    dst: &Path,
    rel: &Path,
    skip: &[String],
    live: Option<&str>,
    budget: &mut crate::guard::Budget,
) -> Result<(u64, u64, u64), String> {
    fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    let entries = fs::read_dir(src).map_err(|e| e.to_string())?;
    let mut reused = 0u64;
    let mut written = 0u64;
    let mut saved = 0u64;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if crate::guard::safe_rel(&name).is_none() {
            continue;
        }
        if rel.as_os_str().is_empty() && skip.iter().any(|s| *s == name) {
            continue;
        }
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        // Symlinks, device nodes and FIFOs are not site files: refuse them
        // rather than follow them into the host or block on them forever.
        if !crate::guard::copyable(&file_type) {
            continue;
        }
        let target = dst.join(&name);
        let child_rel = rel.join(&name);
        if file_type.is_dir() {
            let (r, w, s) = copy_dedup(&path, &target, &child_rel, skip, live, budget)?;
            reused += r;
            written += w;
            saved += s;
        } else if file_type.is_file() {
            let src_text = path.to_string_lossy().into_owned();
            let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
            budget.file(len)?;
            if let Some(live_root) = live {
                let candidate = format!("{}/{}", live_root, child_rel.to_string_lossy());
                if crate::hash::same_file(&src_text, &candidate) {
                    let dst_text = target.to_string_lossy().into_owned();
                    let _ = fs::remove_file(&dst_text);
                    if fs::hard_link(&candidate, &dst_text).is_ok() {
                        reused += 1;
                        saved += len;
                        continue;
                    }
                }
            }
            fs::copy(&path, &target).map_err(|e| format!("{}: {}", src_text, e))?;
            written += 1;
        }
    }
    Ok((reused, written, saved))
}

pub fn run(dev: &str) -> Result<i32, String> {
    if dev.is_empty() {
        log("usage: cartridge-import <device>");
        return Ok(2);
    }
    let meta = match fs::metadata(dev) {
        Ok(m) => m,
        Err(_) => {
            log(&format!("{} is not a block device", dev));
            return Ok(2);
        }
    };
    use std::os::unix::fs::FileTypeExt;
    if !meta.file_type().is_block_device() {
        log(&format!("{} is not a block device", dev));
        return Ok(2);
    }

    rotate_log();

    let _lock = match sys::try_lock("/run/cartridge-import.lock") {
        Some(f) => f,
        None => {
            log("another import is already running; ignoring this device");
            return Ok(0);
        }
    };

    // Import reads through its own short-lived mount. The console keeps the
    // persistent read-only mount at USB_MNT, because this unit runs in a
    // private mount namespace (PrivateTmp=yes) that would discard it.
    let mnt = free_name("/run/cartridge-import");
    let _ = fs::create_dir_all(&mnt);
    let mut mounted_here = false;

    if sys::mount_ro(dev, &mnt).is_ok() {
        mounted_here = true;
    } else if let Some(existing) = existing_mount(dev) {
        log(&format!("{} is already mounted at {}; using that mount", dev, existing));
        let _ = fs::remove_dir(&mnt);
        return import_from(dev, &existing, false);
    } else {
        log(&format!("cannot mount {}", dev));
        let _ = fs::remove_dir(&mnt);
        return Ok(1);
    }

    let code = import_from(dev, &mnt, mounted_here);
    release_device(&mnt, mounted_here);
    code
}

fn import_from(dev: &str, mnt: &str, mounted_here: bool) -> Result<i32, String> {
    let conf_path = Path::new(mnt).join("config.json");
    if !conf_path.is_file() {
        log(&format!("{} holds no config.json - not a cartridge, leaving it alone", dev));
        if mounted_here {
            release_device(mnt, true);
        }
        return Ok(0);
    }
    if let Err(e) = check_config(&conf_path) {
        log(&format!("config.json is not valid JSON - refusing this volume ({})", e));
        if mounted_here {
            release_device(mnt, true);
        }
        return Ok(1);
    }

    let _ = fs::create_dir_all(STATE);

    // Only touch boot.png when the cartridge really carries a different image.
    let mut boot_png_new = false;
    let mut art_stale = false;
    let boot_src = Path::new(mnt).join("boot.png");
    if boot_src.is_file() {
        let current = format!("{}/boot.png", STATE);
        let same = crate::hash::same_file(&boot_src.to_string_lossy(), &current);
        if same {
            // Same picture on the stick is not proof the screen shows it: a
            // failed or older build leaves stale art behind, so check the art
            // itself and rebuild when it was not made from this file.
            if crate::art::is_current(&current, "/var/lib/cartridge/art") {
                skip("boot.png unchanged - the art already matches it");
            } else {
                warn("boot.png unchanged but the art is stale - rebuilding it");
                art_stale = true;
            }
        } else if Path::new(&current).is_file()
            && std::env::var("CARTRIDGE_TAKE_BOOT").map(|v| v != "1").unwrap_or(true)
        {
            // The machine already has its own picture. A cartridge that still
            // carries the demo art must not take it away on every insert;
            // CARTRIDGE_TAKE_BOOT=1 asks for the cartridge's picture instead.
            skip("boot.png kept - this machine already has its own picture");
        } else if fs::copy(&boot_src, format!("{}/boot.png.new", STATE)).is_ok() {
            boot_png_new = true;
        } else {
            warn("boot.png could not be copied");
        }
    }

    let refuse = |reason: &str| -> Result<i32, String> {
        err(&format!("refusing this cartridge: {}", reason));
        if mounted_here {
            release_device(mnt, true);
        }
        Ok(1)
    };

    let _ = fs::create_dir_all(RELEASES);
    let staging = format!("{}/.staging.{}", RELEASES, std::process::id());
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| e.to_string())?;

    log(&format!("importing cartridge from {}", dev));
    let skip_names: Vec<String> = vec![
        "config.json".into(),
        "README.txt".into(),
        "readme.txt".into(),
        "boot.png".into(),
        "System Volume Information".into(),
        "$RECYCLE.BIN".into(),
        ".Trashes".into(),
    ];
    let live_now = fs::read_link(LIVE).ok().map(|p| p.to_string_lossy().into_owned());
    let mut budget = crate::guard::Budget::new();
    match copy_dedup(
        Path::new(mnt),
        Path::new(&staging),
        Path::new(""),
        &skip_names,
        live_now.as_deref(),
        &mut budget,
    ) {
        Ok((reused, written, saved)) => {
            if reused > 0 {
                skip(&format!(
                    "{} file(s) unchanged - linked instead of rewritten ({} saved)",
                    reused,
                    fmt_bytes(saved)
                ));
            }
            log(&format!("{} file(s) written", written));
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            return refuse(&format!("copy from {} failed: {}", dev, e));
        }
    }

    let mut sites = 0usize;
    let mut subtotal_kb = 0u64;
    let mut ports: Vec<String> = Vec::new();
    // Sites as a Markdown table, so the log and the terminal match.
    let mut table: Vec<[String; 3]> = Vec::new();
    log("websites found:");
    log("| Port | Folder | Size |");
    log("|-----:|:-------|-----:|");
    if let Ok(entries) = fs::read_dir(&staging) {
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.path())
            .collect();
        dirs.sort();
        for dir in dirs {
            let name = dir.file_name().unwrap().to_string_lossy().to_string();
            // The same rule the stager uses: 80 or 1024 and above.
            let Some(port) = crate::guard::port_folder(&name) else {
                continue;
            };
            let kb = sys::disk_kb(&dir);
            subtotal_kb += kb;
            table.push([
                format!("`{}`", port),
                format!("`{}/`", name),
                fmt_bytes(kb * 1024),
            ]);
            ports.push(port.to_string());
            sites += 1;
        }
    }
    let total_kb = sys::disk_kb(Path::new(&staging));
    if Path::new(&staging).join("index.html").is_file() {
        table.push([
            "`80`".into(),
            "`/`".into(),
            fmt_bytes(total_kb.saturating_sub(subtotal_kb) * 1024),
        ]);
        ports.push("80".into());
        sites += 1;
    }
    for row in &table {
        log(&format!("| {} | {} | {} |", row[0], row[1], row[2]));
    }
    log(&format!(
        "{} website(s), {} in total",
        sites,
        fmt_bytes(total_kb * 1024)
    ));
    if !ports.is_empty() {
        let lan = sys::lan_ip();
        for (i, p) in ports.iter().enumerate() {
            let url = if p == "80" {
                format!("http://{}/", lan)
            } else {
                format!("http://{}:{}", lan, p)
            };
            if i == 0 {
                log(&format!("  test: {}", url));
            } else {
                log(&format!("        {}", url));
            }
        }
    }

    let empty = fs::read_dir(&staging)
        .map(|mut it| it.next().is_none())
        .unwrap_or(true);
    if empty {
        let _ = fs::remove_dir_all(&staging);
        return refuse("the cartridge carries no site folders");
    }

    // Limits are measured from the machine now: RAM is staged by publish, and
    // storage only has to hold the release with room to spare.
    let (store_total, store_used, store_left) = crate::machine::storage();
    let free = if store_total > 0 {
        store_left / 1024
    } else {
        free_kb("/srv/cartridge")
    };
    let need = total_kb + 131_072;
    log(&format!(
        "storage: {} used, {} left of {} (OS and server included); sites are {}; RAM staging is automatic",
        if store_total > 0 { fmt_bytes(store_used) } else { "?".into() },
        fmt_bytes(free * 1024),
        if store_total > 0 { fmt_bytes(store_total) } else { "?".into() },
        fmt_bytes(total_kb * 1024)
    ));
    if need > free {
        let _ = fs::remove_dir_all(&staging);
        return refuse(
            &format!(
                "the sites need {} but only {} is free",
                fmt_bytes(total_kb * 1024),
                fmt_bytes(free * 1024)
            ),
        );
    }

    fs::create_dir_all(Path::new(&staging).join(".cartridge")).map_err(|e| e.to_string())?;
    let _ = fs::copy(&conf_path, Path::new(&staging).join(".cartridge/config.json"));
    sys::sync();

    let stamp = sys::fmt_time("%Y%m%d-%H%M%S");
    let release = format!("{}/{}", RELEASES, stamp);
    fs::rename(&staging, &release).map_err(|e| e.to_string())?;
    // The cluster may run unprivileged, and it is the one that prunes and
    // replaces releases; a root-owned tree it cannot touch would stop syncing.
    crate::fleet::hand_to_cluster(Path::new(&release));
    let live_new = format!("{}.new", LIVE);
    let _ = fs::remove_file(&live_new);
    std::os::unix::fs::symlink(&release, &live_new).map_err(|e| e.to_string())?;
    let _ = fs::remove_file(LIVE);
    fs::rename(&live_new, LIVE).map_err(|e| e.to_string())?;
    ok(&format!("published release {}", stamp));

    let mut releases: Vec<String> = match fs::read_dir(RELEASES) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("20"))
            .collect(),
        Err(_) => Vec::new(),
    };
    releases.sort();
    releases.reverse();
    for old in releases.iter().skip(KEEP_RELEASES) {
        log(&format!("pruning old release {}", old));
        let _ = fs::remove_dir_all(format!("{}/{}", RELEASES, old));
    }

    if boot_png_new || art_stale {
        if boot_png_new {
            let _ = fs::rename(format!("{}/boot.png.new", STATE), format!("{}/boot.png", STATE));
        }
        rebuild_art();
    }

    // Publish in-process instead of asking systemd for a restart.
    if let Err(e) = crate::publish::run() {
        log(&format!("warning: cartridge-publish did not run: {}", e));
    }
    // Keep the plain-text network map current on a box with no fleet daemon.
    let _ = crate::fleet::refresh_map();
    ok("done");
    let _ = STORE;
    Ok(0)
}
