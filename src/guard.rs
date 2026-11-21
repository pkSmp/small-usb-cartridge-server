//! Rules for everything that arrives from outside the process.
//!
//! A release blueprint, a USB stick, a compressed file and an HTTP client are
//! all untrusted. This module holds the limits and the path rules so every
//! caller applies the same ones, and so they can be tested without a socket.
//!
//! Nothing here allocates beyond the string it returns: the checks run before
//! any buffer is sized from an untrusted number.

use std::path::{Path, PathBuf};

/// Longest relative path a release may contain, in bytes.
pub const MAX_PATH: usize = 512;
/// Longest single path component, matching the ext4/NAME_MAX limit.
pub const MAX_COMPONENT: usize = 255;
/// Deepest directory nesting a release may contain.
pub const MAX_DEPTH: usize = 24;
/// Most files one release may contain.
pub const MAX_FILES: usize = 20_000;
/// Largest single file a release may contain (512 MiB).
pub const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
/// Largest total a release may contain (16 GiB).
pub const MAX_TOTAL_BYTES: u64 = 16 * 1024 * 1024 * 1024;
/// Most bytes one decompression may produce (128 MiB).
pub const MAX_INFLATE: usize = 128 * 1024 * 1024;
/// Largest image side accepted from a PNG header.
pub const MAX_IMAGE_SIDE: usize = 16_384;
/// Largest image area accepted from a PNG header (24 megapixels).
pub const MAX_IMAGE_PIXELS: usize = 24 * 1024 * 1024;
/// Deepest JSON nesting accepted.
pub const MAX_JSON_DEPTH: usize = 128;

/// Reject a relative path that could leave the directory it is joined to, or
/// that a filesystem would interpret differently from the manifest.
///
/// Rejects, in order: empty input, absolute paths, Windows drive prefixes,
/// NUL and other control bytes, backslashes, empty or `.`/`..` components,
/// over-long components, over-deep nesting and over-long totals.
pub fn safe_rel(path: &str) -> Option<&str> {
    if path.is_empty() || path.len() > MAX_PATH {
        return None;
    }
    let bytes = path.as_bytes();
    if bytes[0] == b'/' {
        return None;
    }
    // `C:file` and `C:\file` are absolute on Windows and a manifest is text.
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return None;
    }
    for &b in bytes {
        if b == b'\\' || b == 0 || b < 0x20 || b == 0x7f {
            return None;
        }
    }
    let mut depth = 0usize;
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        if part.len() > MAX_COMPONENT {
            return None;
        }
        depth += 1;
    }
    if depth > MAX_DEPTH {
        return None;
    }
    Some(path)
}

/// Join an untrusted relative path onto a trusted root, or refuse it.
pub fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    safe_rel(rel).map(|ok| root.join(ok))
}

/// Counts one import has used so far. Keeping the counters in one place means
/// a stick cannot pass the file cap through one directory and the byte cap
/// through another.
#[derive(Debug, Default)]
pub struct Budget {
    pub files: usize,
    pub bytes: u64,
}

impl Budget {
    pub fn new() -> Budget {
        Budget::default()
    }

    /// Account for one file of `len` bytes before it is copied.
    pub fn file(&mut self, len: u64) -> Result<(), String> {
        if self.files >= MAX_FILES {
            return Err(format!("more than {} files", MAX_FILES));
        }
        if len > MAX_FILE_BYTES {
            return Err(format!(
                "a file of {} exceeds the {} limit",
                crate::sys::human(len),
                crate::sys::human(MAX_FILE_BYTES)
            ));
        }
        let total = self.bytes.saturating_add(len);
        if total > MAX_TOTAL_BYTES {
            return Err(format!(
                "the release exceeds {} in total",
                crate::sys::human(MAX_TOTAL_BYTES)
            ));
        }
        self.files += 1;
        self.bytes = total;
        Ok(())
    }
}

/// The only two entry kinds an import may copy. A stick can carry symlinks,
/// device nodes and FIFOs; following a symlink would read the host, a device
/// node would stream an endless zero source, and a FIFO would block forever.
pub fn copyable(ft: &std::fs::FileType) -> bool {
    ft.is_file() || ft.is_dir()
}

/// What the accept loop should do after `accept4` returned an error.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum AcceptAction {
    /// Interrupted by a signal: call accept again at once.
    Retry,
    /// Nothing left in the queue: go back to epoll. This is the common case
    /// and must never loop, or the worker spins on an empty listener.
    Stop,
    /// Out of descriptors or memory: stop accepting for a moment.
    Pause,
    /// A real error on the listener (bad fd, not a socket): drop it.
    Fatal,
}

pub const EINTR: i32 = 4;
pub const EAGAIN: i32 = 11;
pub const ENOMEM: i32 = 12;
pub const ENFILE: i32 = 23;
pub const EMFILE: i32 = 24;
pub const ENOBUFS: i32 = 105;

/// Classify an `accept` errno. `EMFILE`/`ENFILE`/`ENOBUFS`/`ENOMEM` mean the
/// box, not the listener, so the loop must back off instead of spinning: with
/// an edge-triggered listener a tight retry loop burns a core and never
/// recovers, because the ready edge has already been consumed.
pub fn accept_action(errno: i32) -> AcceptAction {
    match errno {
        EINTR => AcceptAction::Retry,
        EAGAIN => AcceptAction::Stop,
        EMFILE | ENFILE | ENOBUFS | ENOMEM => AcceptAction::Pause,
        _ => AcceptAction::Fatal,
    }
}

/// Back-off after `streak` consecutive pauses: 20 ms, doubling, capped at 1 s.
pub fn backoff_ms(streak: u32) -> u64 {
    let shift = streak.min(6);
    (20u64 << shift).min(1000)
}

/// True when a transfer has stalled: nothing accepted for `stall_ms`, or a
/// rate below `MIN_BYTES_PER_SEC` once `GRACE_MS` has passed.
///
/// A client that opens a connection and then neither reads nor writes is the
/// slowloris case. Reads already have a header deadline; writes need this.
pub const STALL_MS: u64 = 10_000;
pub const GRACE_MS: u64 = 5_000;
pub const MIN_BYTES_PER_SEC: u64 = 1024;

pub fn stalled(transferred: u64, elapsed_ms: u64) -> bool {
    if elapsed_ms >= STALL_MS && transferred == 0 {
        return true;
    }
    if elapsed_ms >= GRACE_MS && transferred > 0 {
        let rate = transferred.saturating_mul(1000) / elapsed_ms.max(1);
        return rate < MIN_BYTES_PER_SEC;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_paths_that_leave_the_root() {
        for bad in [
            "..",
            "../etc/passwd",
            "a/../../b",
            "./x",
            "a/./b",
            "/etc/passwd",
            "//etc/passwd",
            "a//b",
            "a/",
            "",
            "C:/windows",
            "c:file",
        ] {
            assert!(safe_rel(bad).is_none(), "{:?} must be rejected", bad);
        }
    }

    #[test]
    fn rejects_bytes_a_manifest_cannot_mean() {
        assert!(safe_rel("a\\b").is_none(), "backslash must be rejected");
        assert!(safe_rel("a\0b").is_none(), "NUL must be rejected");
        assert!(safe_rel("a\nb").is_none(), "control bytes must be rejected");
        assert!(safe_rel("a/../b/../c").is_none());
    }

    #[test]
    fn rejects_oversized_shapes() {
        let deep = (0..MAX_DEPTH + 1)
            .map(|i| format!("d{}", i))
            .collect::<Vec<_>>()
            .join("/");
        assert!(safe_rel(&deep).is_none(), "too deep must be rejected");
        let long = "a".repeat(MAX_COMPONENT + 1);
        assert!(safe_rel(&long).is_none(), "long component must be rejected");
        let huge = format!("{}/x", "a".repeat(MAX_PATH));
        assert!(safe_rel(&huge).is_none(), "long path must be rejected");
    }

    #[test]
    fn accepts_ordinary_release_paths() {
        for good in [
            "index.html",
            "8080/index.html",
            "a/b/c/d.txt",
            "spaces and ünïcode/page.html",
            ".well-known/security.txt",
        ] {
            assert_eq!(safe_rel(good), Some(good), "{:?} must be accepted", good);
        }
    }

    #[test]
    fn safe_join_stays_inside_the_root() {
        let root = Path::new("/srv/cartridge/releases/1");
        assert_eq!(
            safe_join(root, "8080/index.html"),
            Some(PathBuf::from("/srv/cartridge/releases/1/8080/index.html"))
        );
        assert_eq!(safe_join(root, "../../etc/shadow"), None);
        assert_eq!(safe_join(root, "/etc/shadow"), None);
    }

    #[test]
    fn budget_stops_at_each_limit() {
        let mut b = Budget::new();
        assert!(b.file(100).is_ok());
        assert!(b.file(MAX_FILE_BYTES + 1).is_err());
        assert_eq!(b.files, 1, "a rejected file must not be counted");

        let mut many = Budget::new();
        for _ in 0..MAX_FILES {
            many.file(1).unwrap();
        }
        assert!(many.file(1).is_err(), "file count must be capped");

        let mut big = Budget::new();
        assert!(big.file(MAX_FILE_BYTES).is_ok());
        for _ in 0..(MAX_TOTAL_BYTES / MAX_FILE_BYTES) {
            let _ = big.file(MAX_FILE_BYTES);
        }
        assert!(
            big.bytes <= MAX_TOTAL_BYTES,
            "total must never exceed the cap"
        );
    }

    #[test]
    fn only_regular_files_and_directories_are_copyable() {
        use std::os::unix::fs::FileTypeExt;
        let dir = std::env::temp_dir().join(format!("cartridge-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let file = dir.join("index.html");
        std::fs::write(&file, b"<html>").unwrap();
        let sub = dir.join("8080");
        std::fs::create_dir_all(&sub).unwrap();
        let link = dir.join("passwd");
        std::os::unix::fs::symlink("/etc/passwd", &link).unwrap();
        let fifo = dir.join("pipe");
        crate::sys::mkfifo(fifo.to_str().unwrap()).unwrap();

        let kind = |p: &std::path::Path| std::fs::symlink_metadata(p).unwrap().file_type();
        assert!(copyable(&kind(&file)), "a regular file is copyable");
        assert!(copyable(&kind(&sub)), "a directory is copyable");
        assert!(!copyable(&kind(&link)), "a symlink must be refused");
        assert!(kind(&fifo).is_fifo(), "the fixture must be a real FIFO");
        assert!(!copyable(&kind(&fifo)), "a FIFO must be refused");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_decompression_bomb_stops_at_the_cap() {
        // 4 MiB of zeros compresses to a few kilobytes: the classic bomb.
        let zeros = vec![0u8; 4 * 1024 * 1024];
        let packed = crate::gzip::gzip(&zeros);
        assert!(
            packed.len() < 64 * 1024,
            "the fixture must be small, got {}",
            packed.len()
        );
        let err = crate::inflate::gzip_decompress_limited(&packed, 1024 * 1024)
            .expect_err("the bomb must not expand past the cap");
        assert!(err.contains("limit"), "unexpected error: {}", err);
        let out = crate::inflate::gzip_decompress_limited(&packed, 8 * 1024 * 1024)
            .expect("a cap above the real size must succeed");
        assert_eq!(out.len(), zeros.len());
    }

    #[test]
    fn oversized_png_dimensions_are_refused() {
        fn header(w: u32, h: u32) -> Vec<u8> {
            let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
            v.extend_from_slice(&13u32.to_be_bytes());
            v.extend_from_slice(b"IHDR");
            v.extend_from_slice(&w.to_be_bytes());
            v.extend_from_slice(&h.to_be_bytes());
            v.extend_from_slice(&[8, 2, 0, 0, 0]);
            v.extend_from_slice(&[0, 0, 0, 0]);
            v
        }
        let err = match crate::png::decode(&header(60_000, 60_000)) {
            Ok(_) => panic!("60k x 60k must be refused before allocating"),
            Err(e) => e,
        };
        assert!(err.contains("larger"), "unexpected error: {}", err);
        assert!(crate::png::decode(&header(0, 10)).is_err());
    }

    #[test]
    fn deep_json_is_refused_not_overflowed() {
        let deep = format!("{}{}", "[".repeat(400), "]".repeat(400));
        let err = crate::json::parse(&deep).expect_err("400 levels must be refused");
        assert!(err.contains("nested"), "unexpected error: {}", err);
        assert!(crate::json::parse("{\"a\":[1,2]}").is_ok());
    }

    #[test]
    fn accept_errors_are_classified() {
        assert_eq!(accept_action(EINTR), AcceptAction::Retry);
        assert_eq!(
            accept_action(EAGAIN),
            AcceptAction::Stop,
            "an empty queue must end the loop, not restart it"
        );
        assert_eq!(accept_action(EMFILE), AcceptAction::Pause);
        assert_eq!(accept_action(ENFILE), AcceptAction::Pause);
        assert_eq!(accept_action(ENOBUFS), AcceptAction::Pause);
        assert_eq!(accept_action(ENOMEM), AcceptAction::Pause);
        assert_eq!(accept_action(22), AcceptAction::Fatal);
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        assert_eq!(backoff_ms(0), 20);
        assert_eq!(backoff_ms(1), 40);
        assert_eq!(backoff_ms(3), 160);
        assert_eq!(backoff_ms(30), 1000);
    }

    #[test]
    fn a_silent_client_is_stalled() {
        assert!(!stalled(0, 1000), "a short pause is allowed");
        assert!(stalled(0, STALL_MS), "no bytes for 10 s is a stall");
        assert!(stalled(1, STALL_MS), "1 byte in 10 s is a stall");
        assert!(
            !stalled(1024 * 1024, 1000),
            "a fast transfer is never stalled"
        );
        assert!(!stalled(0, GRACE_MS - 1));
    }
}
