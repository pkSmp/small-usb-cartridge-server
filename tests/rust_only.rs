//! The repository is Rust and nothing else.
//!
//! These tests encode the rule from the hardening list so a shell script, a
//! Python helper or a new `Command::new` cannot creep back in unnoticed.
//! Known violations are listed explicitly; the list is meant to shrink and
//! a test fails the moment anything new appears.

use std::fs;
use std::path::{Path, PathBuf};

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// Files that may ship in the repository, by extension.
const ALLOWED_EXT: [&str; 5] = ["rs", "toml", "md", "png", "lock"];

/// Extension-less files that are license and ignore metadata, not code.
const ALLOWED_NO_EXT: [&str; 2] = ["LICENSE", ".gitignore"];

/// Files that are still in another language, tracked so the count only falls.
/// Removing each one means moving its content into `install.rs`.
const KNOWN_PENDING: [&str; 9] = [
    "systemd/90-cartridge.rules",
    "systemd/99-cartridge-net.conf",
    "systemd/cartridge-boot-art.service",
    "systemd/cartridge-cluster.service",
    "systemd/cartridge-console.service",
    "systemd/cartridge-fleet.service",
    "systemd/cartridge-import@.service",
    "systemd/cartridge-publish.service",
    "systemd/cartridge-web.service",
];

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == ".git" || name == "target" {
            continue;
        }
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => walk(&path, out),
            Ok(meta) if meta.file_type().is_symlink() => {}
            Ok(_) => out.push(path),
            Err(_) => {}
        }
    }
}

fn relative(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[test]
fn the_tree_contains_only_rust() {
    let root = Path::new(ROOT);
    let mut files = Vec::new();
    walk(root, &mut files);

    let mut offenders = Vec::new();
    for path in &files {
        let rel = relative(path, root);
        if KNOWN_PENDING.contains(&rel.as_str()) || ALLOWED_NO_EXT.contains(&rel.as_str()) {
            continue;
        }
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        // `demo/` is a USB stick in a folder: its HTML, JSON and text are the
        // content the server serves, not code in another language.
        if rel.starts_with("demo/") && matches!(ext.as_str(), "html" | "json" | "txt" | "png") {
            continue;
        }
        if !ALLOWED_EXT.contains(&ext.as_str()) {
            offenders.push(rel);
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "non-Rust files with no pending entry: {:#?}\n\
         Rewrite them in Rust (see `install.rs`) or add them to KNOWN_PENDING \
         only while the replacement lands.",
        offenders
    );
}

#[test]
fn the_crate_has_no_dependencies() {
    let manifest = fs::read_to_string(Path::new(ROOT).join("Cargo.toml"))
        .expect("Cargo.toml must be readable");
    let mut section = String::new();
    let mut found = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            section = trimmed.to_string();
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if section.contains("dependencies") {
            found.push(format!("{} under {}", trimmed, section));
        }
    }
    assert!(
        found.is_empty(),
        "Cargo must have no dependencies at all, found: {:#?}",
        found
    );
    assert!(
        !Path::new(ROOT).join("build.rs").exists(),
        "a build script would be a second language; there must be none"
    );
}

#[test]
fn the_binary_starts_no_new_programs() {
    // `setfont` is the last external program. Everything else runs in-process.
    // Removing it means deleting this allow-list too.
    let allowed = [("src/bootart.rs", 1usize)];
    let src = Path::new(ROOT).join("src");
    let mut found = Vec::new();
    for entry in fs::read_dir(&src).expect("src/ must be readable").flatten() {
        let path = entry.path();
        if path.extension().map(|e| e != "rs").unwrap_or(true) {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let count = text.matches("Command::new(").count();
        if count == 0 {
            continue;
        }
        let rel = relative(&path, Path::new(ROOT));
        let limit = allowed
            .iter()
            .find(|(name, _)| *name == rel)
            .map(|(_, n)| *n)
            .unwrap_or(0);
        if count > limit {
            found.push(format!("{}: {} call(s), {} allowed", rel, count, limit));
        }
    }
    assert!(
        found.is_empty(),
        "the server must not launch other programs: {:#?}",
        found
    );
}
