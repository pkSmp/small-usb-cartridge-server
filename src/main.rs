//! cartridge - one small binary for the whole cartridge console and web stack.
//!
//! Commands
//!   web [--root DIR] [--offset N]   static file server (replaces nginx)
//!   publish                        copy the live release into RAM
//!   art calibrate | verify-calibrate | build IMAGE --out DIR | draw ART | verify MAP
//!
//! Everything is std-only: no crates, no interpreter at runtime.

mod art;
mod bootart;
mod catalog;
mod config;
mod console;
mod font;
mod hash;
mod import;
mod inflate;
mod json;
mod png;
mod publish;
mod sound;
mod status;
mod sys;
mod theme;
mod voice;
mod web;

use std::path::PathBuf;
use std::process::ExitCode;

fn usage() -> ! {
    eprintln!(
        "usage: cartridge <command>\n\
         \n\
         commands:\n\
         \x20 web [--root DIR] [--offset N]   serve the cartridge sites\n\
         \x20 publish                        copy the live release into RAM\n\
         \x20 import DEVICE                  ingest a cartridge\n\
         \x20 status [--watch]               report what the machine sees\n\
         \x20 sites                          live sites and ports as a table\n\
         \x20 console                        status lines under the art\n\
         \x20 boot-art                       draw the boot mosaic\n\
         \x20 announce [--usage]             queue the machine and staging report\n\
         \x20 say [--now] NAME               queue (or play) one voice line\n\
         \x20 voice-serve                    play the voice queue in order\n\
         \x20 forget                         stop speech and clear what was said\n\
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
        "publish" => publish::run(),
        "import" => run_import(&args[2..]),
        "status" => status::run(args[2..].iter().any(|a| a == "--watch" || a == "-w")),
        "sites" => status::sites_cmd(),
        "console" => console::run(),
        "sound" => run_sound(&args[2..]),
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
        "announce" => {
            let full = !args[2..].iter().any(|a| a == "--usage");
            match voice::announce(full) {
                Ok(n) => {
                    println!("queued {} line(s): {}", n, voice::summary());
                    Ok(0)
                }
                Err(e) => Err(e),
            }
        }
        "say" => run_say(&args[2..]),
        "voice-serve" => voice::serve(),
        "forget" => {
            console::forget_dialogue();
            println!("dialogue cleared");
            Ok(0)
        }
        "mount-usb" => {
            if console::mount_cartridge() {
                println!("mounted at {}", voice::MOUNT);
            } else {
                println!("no cartridge to mount");
            }
            Ok(0)
        }
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

fn run_say(args: &[String]) -> Result<i32, String> {
    let now = args.iter().any(|a| a == "--now");
    let name = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or("say needs a line name, for example: say boot")?;
    let path = crate::voice::file(name)
        .ok_or_else(|| format!("{} is not on the inserted cartridge", name))?;
    if !std::path::Path::new(&path).is_file() {
        return Err(format!("{} is missing", path));
    }
    if now {
        crate::voice::play_now(&[path.clone()]);
        println!("played {}", path);
    } else {
        crate::voice::enqueue(&[path.clone()])?;
        println!("queued {} - the voice service plays it in order", path);
    }
    Ok(0)
}

fn run_sound(args: &[String]) -> Result<i32, String> {
    match args.first().map(String::as_str) {
        Some("install") => Err(
            "audio lives only on the cartridge; nothing is installed on this machine".into(),
        ),
        Some("play") => {
            let kind = args.get(1).map(|s| s.as_str()).unwrap_or("complete");
            let path = if kind.contains('/') {
                kind.to_string()
            } else {
                sound::builtin(kind)
            };
            match sound::play(&path) {
                Ok(how) => {
                    println!("played {} via {}", path, how);
                    Ok(0)
                }
                Err(e) => Err(e),
            }
        }
        _ => Err("sound: expected install or play [complete|failed|FILE]".into()),
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
    let mut workers: usize = 2;
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
                    workers = v.parse().unwrap_or(2).max(1);
                }
                i += 2;
            }
            _ => i += 1,
        }
    }
    web::run(root, offset, workers)?;
    Ok(0)
}

fn run_import(args: &[String]) -> Result<i32, String> {
    let dev = args.first().ok_or("import needs a device")?;
    import::run(dev)
}
