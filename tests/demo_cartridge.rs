//! The shipped demo cartridge must be one the server actually accepts, and it
//! must not carry anything the server cannot use. This test is the ratchet: the
//! first version of the demo shipped 47 WAV files, a `sounds` map in
//! `config.json` and a voice section in its README for a server that has no
//! audio output at all, and it shipped them inside a `usb/` wrapper folder, so
//! "extract to the root of a stick" produced a stick with no cartridge on it.

use std::fs;
use std::path::PathBuf;

fn demo() -> PathBuf {
    match std::env::var("CARTRIDGE_DEMO") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("demo"),
    }
}

#[test]
fn demo_is_a_cartridge_the_server_accepts() {
    let root = demo();

    // config.json must parse as a JSON object; the importer refuses anything else.
    let conf = fs::read_to_string(root.join("config.json"))
        .unwrap_or_else(|e| panic!("{}: config.json: {}", root.display(), e));
    let text = conf.trim();
    assert!(
        text.starts_with('{') && text.ends_with('}'),
        "config.json must hold a JSON object, got {:?}",
        text
    );

    // The folder is the stick: nothing else may sit at its root, or it would be
    // published as part of the port 80 site.
    let mut roots: Vec<String> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("{}: {}", root.display(), e))
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    roots.sort();
    assert_eq!(
        roots,
        vec!["3000", "8080", "9090", "README.txt", "boot.png", "config.json"],
        "the demo root must hold only the cartridge"
    );

    let mut ports: Vec<u16> = Vec::new();
    let mut entries: Vec<_> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("{}: {}", root.display(), e))
        .flatten()
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        assert!(
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()),
            "top-level folder {:?} is not a port, so it would be copied but never served",
            name
        );
        let port: u16 = name.parse().expect("digits must fit a port");
        assert!(
            port == 80 || port >= 1024,
            "{} is outside the range the web tier binds (80, or 1024 and above)",
            name
        );
        ports.push(port);
    }
    ports.sort_unstable();
    assert_eq!(ports, vec![3000, 8080, 9090], "the demo advertises these three sites");

    assert!(root.join("boot.png").is_file(), "boot.png is the point of the picture");

    // Nothing on the stick may claim a feature the server does not have. There
    // is no speaker, no voice, no text-to-speech and no config key is read.
    for crumb in ["sounds", "audio", "voice", "usb"] {
        assert!(
            !root.join(crumb).exists(),
            "{:?} does not belong on the demo cartridge",
            crumb
        );
    }
}
