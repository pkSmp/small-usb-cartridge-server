//! The voice: every clip is named after the sentence it speaks, so the file
//! name is its own transcript. Callers only queue paths and return at once;
//! the whole job is added to the transcript immediately, while
//! `cartridge voice-serve` plays the queue in order.

use crate::sound;
use crate::sys;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::thread;
use std::time::Duration;

/// Where the cartridge is mounted read-only. The clips live here and nowhere
/// else: the server never keeps a copy of the audio.
pub const MOUNT: &str = "/run/cartridge-usb";
/// key -> absolute path under MOUNT, written at import from the config.
pub const MAP: &str = "/var/lib/cartridge/sound-map.json";
/// The boot report for this boot only, kept in RAM. It disappears on reboot,
/// and a cartridge insert + removal clears it immediately.
pub const START: &str = "/run/cartridge-start-test.txt";
pub const USAGE: &str = "/run/cartridge-usage.json";
pub const QUEUE: &str = "/run/cartridge-voice.queue";
pub const NOW: &str = "/run/cartridge-voice.now";
pub const LOG: &str = "/var/log/cartridge/voice.log";
const LIVE: &str = "/srv/cartridge/live";

extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

/// The default file name for a spoken sentence: lowercase with underscores.
pub fn default_file_name(text: &str) -> String {
    format!("{}.wav", slug(text))
}

fn slug(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut dash = false;
    for ch in text.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('_');
            dash = true;
        }
    }
    out.trim_end_matches('_').to_string()
}

/// One recorded line, if the cartridge supplied it and is still inserted. The
/// map written at import points into the read-only mount, so removing the
/// stick makes every line silent without leaving audio behind.
pub fn file(key: &str) -> Option<String> {
    if let Ok(text) = fs::read_to_string(MAP) {
        if let Ok(value) = crate::json::parse(&text) {
            if let Some(path) = value.get(key).and_then(|v| v.as_str()) {
                if Path::new(path).is_file() {
                    return Some(path.to_string());
                }
            }
        }
    }
    None
}

/// Numbers are spoken a digit at a time: 59 is "five nine". Ten small files
/// cover every count this machine can report.
pub fn digits(mut n: u64) -> Vec<String> {
    let mut ds: Vec<u64> = Vec::new();
    if n == 0 {
        ds.push(0);
    }
    while n > 0 {
        ds.push(n % 10);
        n /= 10;
    }
    ds.reverse();
    ds.into_iter()
        .filter_map(|d| file(&format!("DIGIT-{}", d)))
        .collect()
}

fn push(seq: &mut Vec<String>, name: &str) {
    if let Some(p) = file(name) {
        seq.push(p);
    }
}

/// Queue one named line. The name is a config key, so the clip it points at
/// must already be on the cartridge: a missing line stays silent rather than
/// being invented here. Returns false when nothing was queued.
pub fn say(name: &str) -> bool {
    match file(name) {
        Some(path) => enqueue(&[path]).is_ok(),
        None => false,
    }
}

/// How many configured lines currently resolve to a file on the cartridge.
pub fn count() -> usize {
    fs::read_to_string(MAP)
        .ok()
        .and_then(|text| crate::json::parse(&text).ok())
        .map(|value| {
            value
                .as_obj()
                .map(|items| items.iter().filter(|(_, v)| v.as_str().is_some()).count())
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

// ------------------------------------------------------------------- machine

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

fn gb(bytes: u64) -> u64 {
    (bytes + (1u64 << 29)) / (1u64 << 30)
}

fn mb(bytes: u64) -> u64 {
    (bytes + (1u64 << 20) - 1) / (1u64 << 20)
}

fn amount(
    seq: &mut Vec<String>,
    bytes: u64,
    big: &str,
    big_one: &str,
    small: &str,
    small_one: &str,
) {
    if bytes >= 1 << 30 {
        let n = gb(bytes);
        seq.extend(digits(n));
        push(seq, if n == 1 { big_one } else { big });
    } else {
        let n = mb(bytes);
        seq.extend(digits(n));
        push(seq, if n == 1 { small_one } else { small });
    }
}

// --------------------------------------------------------------------- queue

fn lock_exclusive(file: &fs::File) -> bool {
    unsafe { flock(file.as_raw_fd(), 2) == 0 }
}

fn unlock(file: &fs::File) {
    unsafe {
        flock(file.as_raw_fd(), 8);
    }
}

/// Add a sequence to the queue and return immediately: nothing that speaks
/// ever waits for the sound card. The complete sentence is logged now, not
/// typed while the audio plays, so the console shows what is coming next.
pub fn enqueue(paths: &[String]) -> Result<(), String> {
    let paths: Vec<String> = paths
        .iter()
        .filter(|p| Path::new(p).is_file())
        .cloned()
        .collect();
    if paths.is_empty() {
        return Ok(());
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(QUEUE)
        .map_err(|e| format!("{}: {}", QUEUE, e))?;
    if !lock_exclusive(&file) {
        return Err("could not lock the voice queue".into());
    }
    let line = paths
        .iter()
        .map(|p| p.as_str())
        .collect::<Vec<_>>()
        .join("\t")
        + "\n";
    let result = (|| -> Result<(), String> {
        file.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
        file.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        file.flush().map_err(|e| e.to_string())
    })();
    unlock(&file);
    if result.is_ok() {
        log_job(&job_text(&paths));
    }
    result
}

fn pop_job() -> Option<Vec<String>> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(QUEUE)
        .ok()?;
    if !lock_exclusive(&file) {
        return None;
    }
    let mut text = String::new();
    if file.read_to_string(&mut text).is_err() {
        unlock(&file);
        return None;
    }
    let (job, rest) = match text.split_once('\n') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => {
            unlock(&file);
            return None;
        }
    };
    let _ = file.set_len(0);
    let _ = file.seek(SeekFrom::Start(0));
    let _ = file.write_all(rest.as_bytes());
    let _ = file.flush();
    unlock(&file);
    let paths: Vec<String> = job
        .split('\t')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

// ----------------------------------------------------------------- transcript

fn stem(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Digit clips are named `1.wav` ... `9.wav`; a digit file's whole transcript is
/// the digit, with no sentence punctuation.
fn is_digit(stem: &str) -> bool {
    stem.len() == 1 && stem.as_bytes()[0].is_ascii_digit()
}

/// Unit clips are fragments such as "drive" or "out of"; they belong inside
/// a sentence, so they stay lowercase and do not get a full stop.
fn is_fragment(stem: &str) -> bool {
    matches!(
        stem,
        "drive"
            | "drives"
            | "gigabytes"
            | "gigabytes_storage"
            | "gigabytes_ram"
            | "out_of"
            | "gigabytes_of_sites_in_ram"
            | "gigabyte_of_sites_in_ram"
            | "megabytes_of_sites_in_ram"
            | "megabyte_of_sites_in_ram"
            | "and"
            | "gigabytes_of_storage_used"
            | "gigabyte_of_storage_used"
            | "megabytes_of_storage_used"
            | "megabyte_of_storage_used"
            | "gigabytes_of_storage_left"
            | "gigabyte_of_storage_left"
            | "megabytes_of_storage_left"
            | "megabyte_of_storage_left"
    )
}

/// The file name is the sentence: `the_computer_started_correctly.wav` reads
/// back as "The computer started correctly."
fn sentence(path: &str) -> String {
    let stem = stem(path);
    if is_digit(&stem) {
        return stem;
    }
    let mut words = stem
        .split('_')
        .map(|word| match word {
            "ram" => "RAM",
            "usb" => "USB",
            "ok" => "OK",
            "gb" => "GB",
            "mb" => "MB",
            "url" => "URL",
            _ => word,
        })
        .collect::<Vec<_>>()
        .join(" ");
    if is_fragment(&stem) {
        return words;
    }
    if let Some(first) = words.chars().next() {
        words.replace_range(0..first.len_utf8(), &first.to_uppercase().to_string());
    }
    if !words.ends_with('.') && !words.ends_with('?') && !words.ends_with('!') {
        words.push('.');
    }
    words
}

/// The whole spoken sequence as one line. Numbers join with no space, because
/// their files are single digits: `1.wav` + `2.wav` reads as `12`, while every
/// other file is separated by a space.
pub fn job_text(paths: &[String]) -> String {
    let mut out = String::new();
    let mut prev_digit = false;
    let mut prev_fragment: Option<String> = None;
    for p in paths {
        let stem = stem(p);
        let digit = is_digit(&stem);
        let fragment = is_fragment(&stem);
        let text = sentence(p);
        let separator = if out.is_empty() {
            ""
        } else if digit && prev_digit {
            ""
        } else if let Some(prev) = prev_fragment.as_deref() {
            if digit {
                match prev {
                    "out_of" | "and" => " ",
                    "gigabytes" => ". ",
                    _ => ", ",
                }
            } else if fragment {
                " "
            } else {
                ". "
            }
        } else {
            " "
        };
        out.push_str(separator);
        out.push_str(&text);
        prev_digit = digit;
        prev_fragment = if fragment { Some(stem) } else { None };
    }
    if !out.is_empty() && !out.ends_with('.') && !out.ends_with('?') && !out.ends_with('!') {
        out.push('.');
    }
    out
}

/// Append the whole line to the spoken history. The console reads this log, so
/// the text appears as soon as a job is queued rather than word by word.
fn log_job(text: &str) {
    if text.trim().is_empty() {
        return;
    }
    let _ = fs::create_dir_all("/var/log/cartridge");
    let mut log = match fs::OpenOptions::new().create(true).append(true).open(LOG) {
        Ok(f) => f,
        Err(_) => return,
    };
    let stamp = format!("{}  ", sys::fmt_time("%H:%M:%S"));
    let _ = log.write_all(format!("{}{}\n", stamp, text).as_bytes());
    let _ = log.flush();
}

/// Play a sequence here. Queued jobs were logged by `enqueue`; direct
/// `say --now` calls log here so they also appear immediately.
fn play(paths: &[String], log: bool) {
    let text = job_text(paths);
    let _ = fs::write(NOW, format!("{}\n", text));
    if log {
        log_job(&text);
    }
    let result = sound::play_many(paths);
    let _ = fs::write(NOW, "\n");
    match result {
        Ok(how) => eprintln!("cartridge voice: {} line(s) via {}", paths.len(), how),
        Err(e) => eprintln!("cartridge voice: playback failed: {}", e),
    }
}

pub fn play_now(paths: &[String]) {
    play(paths, true);
}

/// The player service: ever waiting, one line at a time, FIFO.
pub fn serve() -> Result<i32, String> {
    eprintln!("cartridge voice: waiting for lines in {}", QUEUE);
    loop {
        match pop_job() {
            Some(paths) => play(&paths, false),
            None => thread::sleep(Duration::from_millis(150)),
        }
    }
}

// ---------------------------------------------------------------- announcing

/// Say what the machine started with, then where the sites are staged. The
/// whole report is queued, so the caller never waits for it.
pub fn announce(full: bool) -> Result<usize, String> {
    let (drives, capacity) = hardware();
    let ram_total = mem_total_kb() * 1024;
    let (_total, ram, disk, _sites) = staging();
    let (store_total, store_used, store_left) = storage();
    let shown_total = if store_total > 0 { store_total } else { capacity };

    let mut seq: Vec<String> = Vec::new();
    if full {
        push(&mut seq, "BOOT");
        seq.extend(digits(drives));
        push(&mut seq, if drives == 1 { "UNIT-DRIVE" } else { "UNIT-DRIVES" });
        seq.extend(digits(gb(shown_total)));
        push(&mut seq, "UNIT-GB-STORAGE");
        seq.extend(digits(gb(ram_total)));
        push(&mut seq, "UNIT-GB-RAM");
    }
    push(&mut seq, if disk == 0 { "ALL-FIT" } else { "NOT-ALL-FIT" });
    amount(
        &mut seq,
        ram,
        "UNIT-RAM-SITES",
        "UNIT-RAM-SITE",
        "UNIT-MB-RAM-SITES",
        "UNIT-MB-RAM-SITE",
    );
    push(&mut seq, "UNIT-OUT-OF");
    seq.extend(digits(gb(ram_total)));
    push(&mut seq, "UNIT-GB");
    if store_total > 0 {
        amount(
            &mut seq,
            store_used,
            "UNIT-STORAGE-USED",
            "UNIT-STORAGE-USED-ONE",
            "UNIT-MB-STORAGE-USED",
            "UNIT-MB-STORAGE-USED-ONE",
        );
        push(&mut seq, "UNIT-AND");
        amount(
            &mut seq,
            store_left,
            "UNIT-STORAGE-LEFT",
            "UNIT-STORAGE-LEFT-ONE",
            "UNIT-MB-STORAGE-LEFT",
            "UNIT-MB-STORAGE-LEFT-ONE",
        );
    }

    if seq.is_empty() {
        return Err("no voice lines are installed".into());
    }
    let lines = seq.len();
    if full {
        let text = job_text(&seq);
        if !text.is_empty() {
            let _ = fs::write(START, format!("{}\n", text));
        }
    }
    enqueue(&seq)?;
    Ok(lines)
}

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
    format!(
        "{} internal drive(s), {} · {} RAM · sites {} ({} in RAM, {} on disk) · {} line(s) · {}",
        drives,
        storage,
        sys::human(ram_total),
        sys::human(total),
        sys::human(ram),
        sys::human(disk),
        count(),
        if disk == 0 { "all sites fit in RAM" } else { "not all sites fit in RAM" }
    )
}
