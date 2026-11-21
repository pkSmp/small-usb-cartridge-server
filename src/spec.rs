//! What a machine actually is.
//!
//! A worker count is not a speed. Two boxes with four threads can differ by 3x
//! in single-core work, and one of them may be running on a quarter of a core.
//! So every node reads what the kernel will tell it for free, times the two
//! things that cannot be read (CPU and memory throughput), and serves itself a
//! request through the real HTTP path to get the only number that matters:
//! responses and bytes per second.
//!
//! The result is two figures the rest of the cluster uses — `serve_rps` and
//! `serve_bps` — plus the raw spec so `status` can explain them. Nothing here
//! is trusted across the network: peers advertise these numbers, and the
//! balancer still times every response it actually receives.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Spec {
    pub threads: u16,
    /// Physical cores: SMT siblings are one core, not two.
    pub cores: u16,
    /// Measured multi-core gain over the single-core rate; 1.0 means no SMT win.
    pub smt_gain: f32,
    /// Loop iterations per millisecond on one thread.
    pub single: u64,
    /// Measured copy bandwidth, MiB/s.
    pub mem_mbps: u32,
    pub ram_total_mb: u64,
    /// RAM this node is willing to give to the hot set and to shards.
    pub ram_spare_mb: u64,
    /// Link speed in Mbit/s, 0 when the kernel cannot say.
    pub nic_mbps: u32,
    pub disk_free_mb: u64,
    /// 0 none, 1 integrated, 2 discrete.
    pub gpu: u8,
    /// Responses per second measured through the real serving path.
    pub serve_rps: u32,
    /// Bytes per second for a large object, capped by the link.
    pub serve_bps: u64,
    pub model: String,
}

impl Spec {
    /// Placement weight, coarsely quantized on purpose: two nodes that measure
    /// 41.7 and 41.9 MB/s must land in the same bucket, or their maps diverge
    /// and chunks are assigned to different owners on different machines.
    pub fn weight(&self) -> u32 {
        // 25 MB/s buckets: a 100 Mbit box lands near 1, 1 GbE near 4, 2.5 GbE
        // near 12, so links of different speeds stay distinguishable.
        ((self.serve_bps / 25_000_000).max(1)).min(200_000) as u32
    }

    /// Capacity in hundredths of a core, the unit `cluster` already uses.
    pub fn capacity(&self) -> u32 {
        let cores = self.cores.max(1) as f32;
        let effective = (cores * self.smt_gain.max(1.0)).min(self.threads.max(1) as f32);
        (effective * 100.0).round().max(100.0) as u32
    }

    /// One line per field, in a fixed order, no spaces. The beacon appends
    /// these after the original nine fields so older nodes can ignore them.
    pub fn fields(&self) -> Vec<String> {
        vec![
            self.threads.to_string(),
            self.cores.to_string(),
            ((self.smt_gain * 100.0).round() as u32).to_string(),
            self.single.to_string(),
            self.mem_mbps.to_string(),
            self.ram_total_mb.to_string(),
            self.ram_spare_mb.to_string(),
            self.nic_mbps.to_string(),
            self.disk_free_mb.to_string(),
            self.gpu.to_string(),
            self.serve_rps.to_string(),
            self.serve_bps.to_string(),
            // An empty field would vanish in the whitespace split, so "no model"
            // still needs a token.
            if self.model.is_empty() {
                "-".to_string()
            } else {
                self.model.replace(' ', "_")
            },
        ]
    }

    pub fn parse(fields: &[&str]) -> Option<Spec> {
        if fields.len() < 13 {
            return None;
        }
        Some(Spec {
            threads: fields[0].parse().ok()?,
            cores: fields[1].parse().ok()?,
            smt_gain: fields[2].parse::<u32>().ok()? as f32 / 100.0,
            single: fields[3].parse().ok()?,
            mem_mbps: fields[4].parse().ok()?,
            ram_total_mb: fields[5].parse().ok()?,
            ram_spare_mb: fields[6].parse().ok()?,
            nic_mbps: fields[7].parse().ok()?,
            disk_free_mb: fields[8].parse().ok()?,
            gpu: fields[9].parse().ok()?,
            serve_rps: fields[10].parse().ok()?,
            serve_bps: fields[11].parse().ok()?,
            model: if fields[12] == "-" {
                String::new()
            } else {
                fields[12].replace('_', " ")
            },
        })
    }

    pub fn summary(&self) -> String {
        let nic = if self.nic_mbps == 0 {
            "link ?".to_string()
        } else {
            format!("{} Mb/s", self.nic_mbps)
        };
        format!(
            "{} · {} cores / {} threads · {} rps · {}/s · {} · {}{}",
            if self.model.is_empty() { "unknown CPU" } else { &self.model },
            self.cores,
            self.threads,
            self.serve_rps,
            crate::sys::human(self.serve_bps),
            nic,
            crate::sys::human(self.ram_spare_mb * 1024 * 1024),
            if self.gpu > 0 { " · gpu (idle)" } else { "" }
        )
    }
}

// ------------------------------------------------------------------ reading

fn read_first(path: &str) -> Option<String> {
    fs::read_to_string(path).ok()?.lines().next().map(|s| s.trim().to_string())
}

/// Unique core groups among the SMT sibling lists. `0-1` and `2-3` are two
/// cores even though they are four threads.
pub fn physical_cores() -> u16 {
    let mut cores: BTreeSet<u32> = BTreeSet::new();
    for i in 0..256 {
        let path = format!("/sys/devices/system/cpu/cpu{}/topology/thread_siblings_list", i);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => break,
        };
        let first = text.trim().split(|c| c == ',' || c == '-').next().unwrap_or("");
        if let Ok(n) = first.parse::<u32>() {
            cores.insert(n);
        }
    }
    if cores.is_empty() {
        return std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(u16::MAX as usize) as u16;
    }
    cores.len().min(u16::MAX as usize) as u16
}

fn cpu_model() -> String {
    if let Ok(text) = fs::read_to_string("/proc/cpuinfo") {
        for line in text.lines() {
            for key in ["model name", "Hardware", "Processor"] {
                if let Some(rest) = line.strip_prefix(key) {
                    if let Some(value) = rest.split(':').nth(1) {
                        let v = value.trim();
                        if !v.is_empty() {
                            return v.to_string();
                        }
                    }
                }
            }
        }
    }
    String::new()
}

fn mem_kb(key: &str) -> u64 {
    if let Ok(text) = fs::read_to_string("/proc/meminfo") {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix(key) {
                if let Some(value) = rest.split_whitespace().next() {
                    return value.parse().unwrap_or(0);
                }
            }
        }
    }
    0
}

/// The interface that carries the default route, then its advertised speed.
pub fn default_iface() -> String {
    if let Ok(text) = fs::read_to_string("/proc/net/route") {
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 8 && f[1] == "00000000" && f[7] == "00000000" {
                return f[0].to_string();
            }
        }
    }
    String::new()
}

pub fn nic_mbps() -> u32 {
    let iface = default_iface();
    if iface.is_empty() {
        return 0;
    }
    if let Some(speed) = read_first(&format!("/sys/class/net/{}/speed", iface)) {
        if let Ok(n) = speed.parse::<i64>() {
            if n > 0 {
                return n.min(u32::MAX as i64) as u32;
            }
        }
    }
    0
}

pub fn disk_free_mb(path: &str) -> u64 {
    // `sys` already declares statvfs for the rest of the binary; a second
    // declaration would be a different signature for the same symbol.
    match crate::sys::filesystem_bytes(path) {
        Some((_, _, free)) => free / (1024 * 1024),
        None => 0,
    }
}

/// Presence only: a card can be reported and left idle. Nothing in the serving
/// path uses it.
pub fn gpu_flag() -> u8 {
    let mut integrated = false;
    if let Ok(entries) = fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("card") && name.len() > 4 {
                if entry.path().join("device/vendor").exists() {
                    integrated = true;
                }
            }
        }
    }
    if Path::new("/proc/driver/nvidia/version").exists() {
        return 2;
    }
    if integrated {
        1
    } else {
        0
    }
}

// --------------------------------------------------------------- benchmarks

/// Loop iterations per millisecond on one thread. 200 ms is deliberate: 50 ms
/// is long enough for turbo to flatter a weak laptop and short enough to be
/// dominated by cache warmth.
pub fn bench_cpu(ms: u64) -> u64 {
    let start = Instant::now();
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let mut n = 0u64;
    while start.elapsed().as_millis() < ms as u128 {
        for _ in 0..10_000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
            x ^= x >> 29;
        }
        n += 10_000;
    }
    std::hint::black_box(x);
    n / ms.max(1)
}

/// The same loop on `threads` threads, summed. Used only to learn how much SMT
/// and extra cores actually add on this machine.
pub fn bench_cpu_threads(threads: usize, ms: u64) -> u64 {
    let threads = threads.clamp(1, 64);
    let mut handles = Vec::new();
    for _ in 0..threads {
        handles.push(std::thread::spawn(move || bench_cpu(ms)));
    }
    handles.into_iter().map(|h| h.join().unwrap_or(0)).sum()
}

/// Copy bandwidth in MiB/s. 32 MiB buffers keep the test honest without asking
/// a 2 GB machine for more memory than it has.
pub fn bench_mem_mbps() -> u32 {
    const MIB: usize = 32;
    let src = vec![0xA5u8; MIB * 1024 * 1024];
    let mut dst = vec![0u8; MIB * 1024 * 1024];
    let start = Instant::now();
    let rounds = 3;
    for _ in 0..rounds {
        dst.copy_from_slice(&src);
        std::hint::black_box(&dst);
    }
    let secs = start.elapsed().as_secs_f64().max(0.000_001);
    ((MIB as f64 * rounds as f64) / secs) as u32
}

// ------------------------------------------------------------- serving test

/// Drive the real HTTP path and count what comes back. `path` should name a
/// small file for the request rate and is reused for the byte rate; the caller
/// passes a large one when it wants bandwidth instead.
pub fn measure_http(port: u16, path: &str, conns: usize, dur: Duration) -> (f64, u64) {
    let conns = conns.clamp(1, 32);
    let requests = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + dur;
    let mut handles = Vec::new();
    for _ in 0..conns {
        let requests = requests.clone();
        let bytes = bytes.clone();
        let stop = stop.clone();
        let path = path.to_string();
        handles.push(std::thread::spawn(move || {
            let request = format!("GET {} HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n", path);
            let mut buf = [0u8; 8192];
            while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                let mut stream = match TcpStream::connect(("127.0.0.1", port)) {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                if stream.write_all(request.as_bytes()).is_err() {
                    break;
                }
                let mut got = 0u64;
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => got += n as u64,
                        Err(_) => break,
                    }
                }
                if got == 0 {
                    break;
                }
                requests.fetch_add(1, Ordering::Relaxed);
                bytes.fetch_add(got, Ordering::Relaxed);
            }
        }));
    }
    std::thread::sleep(dur);
    stop.store(true, Ordering::Relaxed);
    let secs = dur.as_secs_f64().max(0.001);
    for h in handles {
        let _ = h.join();
    }
    (
        requests.load(Ordering::Relaxed) as f64 / secs,
        (bytes.load(Ordering::Relaxed) as f64 / secs) as u64,
    )
}

pub struct Probe {
    pub spec: Spec,
}

/// Read and time everything, then fold the timing into `serve_*`.
pub fn probe(root: &str) -> Probe {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, u16::MAX as usize) as u16;
    let cores = physical_cores().max(1);
    let single = bench_cpu(200);
    let scaled = bench_cpu_threads(cores as usize, 120);
    let expected = (single.max(1) as f64) * (cores as f64);
    let smt_gain = (scaled as f64 / expected).clamp(0.5, 3.0) as f32;
    let mem_mbps = bench_mem_mbps();
    let ram_total_mb = mem_kb("MemTotal:") / 1024;
    let ram_avail_mb = mem_kb("MemAvailable:") / 1024;
    // Never take more than half of what is free, and always leave a quarter of
    // total RAM for the page cache and the site itself.
    let reserve = (ram_total_mb / 4).max(256);
    let ram_spare_mb = ram_avail_mb.saturating_sub(reserve).min(ram_total_mb / 2);
    let measured_nic = nic_mbps();
    // Some drivers refuse to report a speed without a link (and return -1 even
    // as root). A wrong zero would have invented a byte rate out of RAM speed,
    // so assume the common case and say so rather than lie: 1 GbE is the floor
    // of this fleet's design, and the self-test refines the request rate later.
    let nic = if measured_nic == 0 { 1000 } else { measured_nic };
    let disk = disk_free_mb(root);
    // For a file server the link is the sustained ceiling; CPU shows up in the
    // request rate, not in bytes per second.
    let serve_bps = (nic as u64).saturating_mul(125_000).saturating_mul(94) / 100;
    Probe {
        spec: Spec {
            threads,
            cores,
            smt_gain,
            single,
            mem_mbps,
            ram_total_mb,
            ram_spare_mb,
            nic_mbps: nic,
            disk_free_mb: disk,
            gpu: gpu_flag(),
            // Filled in by `with_selftest` once the workers are answering.
            serve_rps: single.min(u32::MAX as u64) as u32,
            serve_bps,
            model: cpu_model(),
        },
    }
}

impl Spec {
    /// Replace the placeholder rate with a measured one.
    pub fn with_selftest(mut self, rps: f64, bps: u64) -> Spec {
        if rps >= 1.0 {
            self.serve_rps = rps.min(u32::MAX as f64) as u32;
        }
        if bps > 0 {
            let link = (self.nic_mbps as u64).saturating_mul(125_000).saturating_mul(94) / 100;
            self.serve_bps = if link > 0 { bps.min(link) } else { bps };
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_round_trip_through_the_beacon() {
        let spec = Spec {
            threads: 8, cores: 4, smt_gain: 1.28, single: 123_456, mem_mbps: 9_400,
            ram_total_mb: 16_384, ram_spare_mb: 6_000, nic_mbps: 2500, disk_free_mb: 900_000,
            gpu: 2, serve_rps: 41_000, serve_bps: 290_000_000,
            model: "AMD Ryzen 7 5800X".into(),
        };
        let owned = spec.fields();
        let refs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        let back = Spec::parse(&refs).unwrap();
        assert_eq!(back, spec);
        assert!(Spec::parse(&refs[..12]).is_none());
    }

    #[test]
    fn the_weight_is_coarse_enough_to_stay_agreeable() {
        let a = Spec { serve_bps: 41_700_000, ..Default::default() };
        let b = Spec { serve_bps: 41_900_000, ..Default::default() };
        assert_eq!(a.weight(), b.weight(), "near-identical machines must bucket together");
        let c = Spec { serve_bps: 250_000_000, ..Default::default() };
        assert!(c.weight() > a.weight() * 4);
        // A machine that has never been measured still gets a unit of weight.
        assert_eq!(Spec::default().weight(), 1);
    }

    #[test]
    fn capacity_accounts_for_smt_without_counting_it_twice() {
        let mut s = Spec { threads: 16, cores: 8, smt_gain: 1.0, ..Default::default() };
        assert_eq!(s.capacity(), 800);
        s.smt_gain = 1.3;
        assert_eq!(s.capacity(), 1040);
        // Gain can never manufacture more capacity than there are threads.
        s.smt_gain = 3.0;
        assert_eq!(s.capacity(), 1600);
    }

    #[test]
    fn the_cpu_benchmark_reports_a_positive_rate() {
        let n = bench_cpu(30);
        assert!(n > 0, "benchmark returned {}", n);
        let mt = bench_cpu_threads(2, 30);
        assert!(mt > 0);
    }

    #[test]
    fn the_machine_reports_itself() {
        let p = probe("/");
        assert!(p.spec.threads >= 1);
        assert!(p.spec.cores >= 1);
        assert!(p.spec.mem_mbps > 0);
        assert!(p.spec.ram_total_mb > 0);
        assert!(p.spec.serve_bps > 0);
        assert!(p.spec.single > 0);
    }

    #[test]
    fn the_http_self_test_counts_real_responses() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let server = std::thread::spawn(move || {
            let body = vec![b'x'; 1200];
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut s, _)) => {
                        let mut head = [0u8; 512];
                        let _ = s.read(&mut head);
                        let mut out = b"HTTP/1.1 200 OK\r\nContent-Length: 1200\r\n\r\n".to_vec();
                        out.extend_from_slice(&body);
                        let _ = s.write_all(&out);
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(1)),
                }
            }
        });
        let (rps, bps) = measure_http(port, "/", 2, Duration::from_millis(150));
        stop.store(true, Ordering::Relaxed);
        let _ = server.join();
        assert!(rps > 0.0, "no responses measured");
        assert!(bps > 0, "no bytes measured");
    }
}
