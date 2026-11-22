//! cartridge - one small binary for the whole cartridge console and web stack.
//!
//! Commands
//!   web [--root DIR] [--offset N]   static file server (replaces nginx)
//!   publish                        copy the live release into RAM
//!   art calibrate | verify-calibrate | build IMAGE --out DIR | draw ART | verify MAP
//!
//! Everything is std-only: no crates, no interpreter at runtime.

mod archive;
mod art;
mod auth;
mod blueprint;
mod bootart;
mod chunks;
mod cluster;
mod console;
mod dns;
mod erasure;
mod fleet;
mod font;
#[cfg(test)]
mod fuzz;
mod guard;
mod gzip;
mod hash;
mod ident;
mod import;
mod inflate;
mod json;
mod machine;
mod placement;
mod png;
mod publish;
mod spec;
mod status;
mod sys;
mod theme;
mod web;

use std::path::PathBuf;
use std::process::ExitCode;

fn usage() -> ! {
    eprintln!(
        "usage: cartridge <command>\n\
         \n\
         commands:\n\
         \x20 web [--root DIR] [--offset N] [--workers N] [--bind ADDR]   serve the cartridge sites\n\
         \x20 cluster [--root DIR] [--name ID] [--offset N]   share sites and load with the boxes on the switch\n\
         \x20 fleet [--name ID] [--beacon PORT] [--dns PORT]   share storage and updates with the boxes on the switch\n\
         \x20 fleet status | store | key | archive | adopt   inspect or manage the fleet\n\
         \x20 fleet restore FILE               rebuild this box from one archive\n\
         \x20 publish                        copy the live release into RAM\n\
         \x20 import DEVICE                  ingest a cartridge\n\
         \x20 status [--watch]               report what the machine sees\n\
         \x20 sites                          live sites and ports as a table\n\
         \x20 console                        status lines under the art\n\
         \x20 boot-art                       draw the boot mosaic\n\
         \x20 art calibrate [--order rgb|bgr]\n\
         \x20 art verify-calibrate\n\
         \x20 art build IMAGE --out DIR\n\
         \x20 art draw ART.bin\n\
         \x20 art verify MAP.json"
    );
    std::process::exit(2);
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
    }
    let result = match args[1].as_str() {
        "art" => run_art(&args[2..]),
        "web" => run_web(&args[2..]),
        "cluster" => run_cluster(&args[2..]),
        "fleet" => run_fleet(&args[2..]),
        "publish" => publish::run(),
        "import" => run_import(&args[2..]),
        "status" => status::run(args[2..].iter().any(|a| a == "--watch" || a == "-w")),
        "sites" => status::sites_cmd(),
        "console" => console::run(),
        "hash" => {
            for p in &args[2..] {
                match hash::file(p) {
                    Ok(h) => println!("{}  {}", h, p),
                    Err(e) => eprintln!("cartridge: {}: {}", p, e),
                }
            }
            Ok(0)
        }
        "font-dump" => {
            match font::dump_table() {
                Ok(t) => {
                    for (i, v) in t.iter().enumerate().take(256) {
                        if *v != 0 {
                            println!("{:3} 0x{:04X} -> U+{:04X}", i, i, v);
                        }
                    }
                    Ok(0)
                }
                Err(e) => Err(e),
            }
        }
        "boot-art" => bootart::run(),
        "version" | "--version" => {
            println!("cartridge {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        other => {
            eprintln!("cartridge: unknown command {:?}", other);
            usage();
        }
    };
    match result {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("cartridge: {}", e);
            ExitCode::from(1)
        }
    }
}

fn run_art(args: &[String]) -> Result<i32, String> {
    match args.first().map(String::as_str) {
        Some("calibrate") => {
            let mut order: Option<&str> = None;
            let mut i = 1;
            while i < args.len() {
                if args[i] == "--order" {
                    order = args.get(i + 1).map(|s| s.as_str());
                    i += 2;
                } else {
                    i += 1;
                }
            }
            art::cmd_calibrate(order)
        }
        Some("verify-calibrate") => art::cmd_verify_calibrate(),
        Some("build") => {
            let image = args.get(1).ok_or("art build needs an image")?;
            let mut out: Option<String> = None;
            let mut i = 2;
            while i < args.len() {
                if args[i] == "--out" {
                    out = args.get(i + 1).cloned();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            let out = out.ok_or("art build needs --out DIR")?;
            art::cmd_build(image, &out)
        }
        Some("draw") => art::cmd_draw(args.get(1).ok_or("art draw needs art.bin")?),
        Some("verify") => art::cmd_verify(args.get(1).ok_or("art verify needs map.json")?),
        _ => Err("art: expected calibrate, verify-calibrate, build, draw or verify".into()),
    }
}

fn run_web(args: &[String]) -> Result<i32, String> {
    let mut root = PathBuf::from("/run/cartridge");
    let mut offset: u16 = 0;
    let mut workers: usize = 0; // 0 = one worker per CPU thread
    let mut bind = [0u8; 4];
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                if let Some(v) = args.get(i + 1) {
                    root = PathBuf::from(v);
                }
                i += 2;
            }
            "--offset" => {
                if let Some(v) = args.get(i + 1) {
                    offset = v.parse().unwrap_or(0);
                }
                i += 2;
            }
            "--workers" => {
                if let Some(v) = args.get(i + 1) {
                    workers = v.parse().unwrap_or(0).min(128);
                }
                i += 2;
            }
            "--bind" => {
                if let Some(v) = args.get(i + 1) {
                    match v.parse::<std::net::Ipv4Addr>() {
                        Ok(ip) => bind = ip.octets(),
                        Err(_) => {
                            return Err(format!("web: {:?} is not an IPv4 address", v));
                        }
                    }
                }
                i += 2;
            }
            _ => i += 1,
        }
    }
    web::run(root, offset, workers, bind)?;
    Ok(0)
}

fn run_cluster(args: &[String]) -> Result<i32, String> {
    let mut root = PathBuf::from("/run/cartridge");
    let mut control = cluster::DEFAULT_CONTROL;
    let mut beacon = cluster::DEFAULT_BEACON;
    let mut workers: usize = 0;
    let mut offset: u16 = 0;
    let mut name: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                if let Some(v) = args.get(i + 1) {
                    root = PathBuf::from(v);
                }
                i += 2;
            }
            "--control" => {
                if let Some(v) = args.get(i + 1).and_then(|v| v.parse().ok()) {
                    control = v;
                }
                i += 2;
            }
            "--beacon" => {
                if let Some(v) = args.get(i + 1).and_then(|v| v.parse().ok()) {
                    beacon = v;
                }
                i += 2;
            }
            "--workers" => {
                if let Some(v) = args.get(i + 1).and_then(|v| v.parse().ok()) {
                    workers = v;
                }
                i += 2;
            }
            "--offset" => {
                if let Some(v) = args.get(i + 1).and_then(|v| v.parse().ok()) {
                    offset = v;
                }
                i += 2;
            }
            "--name" => {
                name = args.get(i + 1).cloned();
                i += 2;
            }
            _ => i += 1,
        }
    }
    cluster::run(root, control, beacon, workers, name, offset)?;
    Ok(0)
}

/// `fleet` either runs the service or answers a question about the last run.
fn run_fleet(args: &[String]) -> Result<i32, String> {
    match args.first().map(String::as_str) {
        Some("status") => return fleet::status(),
        Some("store") => return fleet::sites_on_disk(),
        Some("archive") => return fleet::archive_now(),
        Some("adopt") => return fleet::adopt(),
        Some("restore") => {
            let file = args.get(1).ok_or("fleet restore needs an archive path")?;
            return fleet::restore_from(file);
        }
        Some("key") => {
            let created = auth::ensure_key()?;
            if created {
                println!("wrote a fleet key to {}", auth::KEY_FILE);
                println!("copy it to every node; without it, releases are accepted unsigned");
            } else {
                println!("a fleet key already exists at {}", auth::KEY_FILE);
            }
            return Ok(0);
        }
        Some("id") => {
            let me = ident::load_or_create();
            println!("{}  {}", me.name, me.hex());
            return Ok(0);
        }
        _ => {}
    }
    let mut root = PathBuf::from("/run/cartridge");
    let mut beacon = fleet::BEACON;
    let mut control = fleet::CONTROL;
    let mut dns_port: Option<u16> = None;
    let mut name: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                if let Some(v) = args.get(i + 1) {
                    root = PathBuf::from(v);
                }
                i += 2;
            }
            "--control" => {
                if let Some(v) = args.get(i + 1).and_then(|v| v.parse().ok()) {
                    control = v;
                }
                i += 2;
            }
            "--beacon" => {
                if let Some(v) = args.get(i + 1).and_then(|v| v.parse().ok()) {
                    beacon = v;
                }
                i += 2;
            }
            "--dns" => {
                if let Some(v) = args.get(i + 1).and_then(|v| v.parse().ok()) {
                    dns_port = Some(v);
                }
                i += 2;
            }
            "--name" => {
                name = args.get(i + 1).cloned();
                i += 2;
            }
            _ => i += 1,
        }
    }
    fleet::run(root, control, beacon, dns_port, name)
}

fn run_import(args: &[String]) -> Result<i32, String> {
    let dev = args.first().ok_or("import needs a device")?;
    import::run(dev)
}
