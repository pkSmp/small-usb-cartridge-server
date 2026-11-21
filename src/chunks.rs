//! The chunk store: every byte the fleet knows, addressed by its hash.
//!
//! Objects are immutable and named by SHA-256, so a chunk is never edited, only
//! added or deleted, and a copy can be verified without trusting the sender. A
//! new release is ingested into the same store, reusing the previous one with
//! hard links: an update that touches two files links the rest and moves only
//! the delta.
//!
//! Nothing here knows about the network. `fleet` decides what to fetch and from
//! whom; this module only reads, writes, verifies and prunes.

use crate::blueprint::{Blueprint, FileEntry, Piece, CHUNK, content_type};
use crate::hash;
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub fn valid_hash(text: &str) -> bool {
    text.len() == 64 && text.chars().all(|c| c.is_ascii_hexdigit())
}

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct Ingest {
    pub files: usize,
    pub chunks: usize,
    pub new_chunks: usize,
    pub linked_chunks: usize,
    pub bytes: u64,
}

impl Store {
    pub fn at(root: impl Into<PathBuf>) -> Store {
        Store { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `objects/ab/abcdef...`: sharded by the first byte so a big release does
    /// not put a million entries in one directory.
    pub fn path_for(&self, digest: &str) -> PathBuf {
        if !valid_hash(digest) {
            // Never let a hostile hash escape the store.
            return self.root.join("invalid");
        }
        self.root.join("objects").join(&digest[..2]).join(digest)
    }

    pub fn has(&self, digest: &str) -> bool {
        valid_hash(digest) && self.path_for(digest).is_file()
    }

    pub fn len(&self, digest: &str) -> Option<u64> {
        fs::metadata(self.path_for(digest)).ok().map(|m| m.len())
    }

    pub fn get(&self, digest: &str) -> io::Result<Vec<u8>> {
        if !valid_hash(digest) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad chunk hash"));
        }
        fs::read(self.path_for(digest))
    }

    /// Write a chunk if it is not already there. Returns its hash and whether
    /// it was new. The write goes to a temporary name in the same directory and
    /// is renamed into place, so a crash never leaves a truncated object under
    /// the real name.
    pub fn put(&self, bytes: &[u8]) -> io::Result<(String, bool)> {
        let digest = hash::bytes(bytes);
        let path = self.path_for(&digest);
        if path.is_file() {
            return Ok((digest, false));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(bytes)?;
            let _ = file.sync_all();
        }
        match fs::rename(&tmp, &path) {
            Ok(()) => Ok((digest, true)),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                // Another thread or process won the race: that is success.
                if path.is_file() {
                    Ok((digest, false))
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Reuse a chunk from another store (a previous release, or a peer's copy
    /// on the same disk) without copying it when the filesystem allows.
    pub fn link_from(&self, digest: &str, other: &Store) -> io::Result<bool> {
        if self.has(digest) {
            return Ok(false);
        }
        let source = other.path_for(digest);
        if !source.is_file() {
            return Ok(false);
        }
        let target = self.path_for(digest);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        match fs::hard_link(&source, &target) {
            Ok(()) => Ok(true),
            Err(_) => {
                // Different filesystems (or a filesystem without links).
                fs::copy(&source, &target)?;
                Ok(true)
            }
        }
    }

    /// Every chunk this store holds.
    pub fn held(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let objects = self.root.join("objects");
        if let Ok(dirs) = fs::read_dir(&objects) {
            for dir in dirs.flatten() {
                if let Ok(files) = fs::read_dir(dir.path()) {
                    for file in files.flatten() {
                        let name = file.file_name().to_string_lossy().to_string();
                        if valid_hash(&name) {
                            out.insert(name);
                        }
                    }
                }
            }
        }
        out
    }

    pub fn bytes(&self) -> u64 {
        let mut total = 0u64;
        let objects = self.root.join("objects");
        if let Ok(dirs) = fs::read_dir(&objects) {
            for dir in dirs.flatten() {
                if let Ok(files) = fs::read_dir(dir.path()) {
                    for file in files.flatten() {
                        total += file.metadata().map(|m| m.len()).unwrap_or(0);
                    }
                }
            }
        }
        total
    }

    /// Delete every chunk not in `keep`. Returns (removed, bytes freed).
    pub fn prune(&self, keep: &BTreeSet<String>) -> (usize, u64) {
        let mut removed = 0usize;
        let mut freed = 0u64;
        let objects = self.root.join("objects");
        if let Ok(dirs) = fs::read_dir(&objects) {
            for dir in dirs.flatten() {
                if let Ok(files) = fs::read_dir(dir.path()) {
                    for file in files.flatten() {
                        let name = file.file_name().to_string_lossy().to_string();
                        if valid_hash(&name) && !keep.contains(&name) {
                            let len = file.metadata().map(|m| m.len()).unwrap_or(0);
                            if fs::remove_file(file.path()).is_ok() {
                                removed += 1;
                                freed += len;
                            }
                        }
                    }
                }
            }
        }
        (removed, freed)
    }
}

/// Read a release tree into the store and produce its blueprint. Chunks that
/// already exist in `cache` (usually the live release's store) are hard-linked,
/// so an update moves only what changed.
pub fn ingest(
    dir: &Path,
    store: &Store,
    cache: Option<&Store>,
    epoch: u64,
) -> io::Result<(Blueprint, Ingest)> {
    let mut stats = Ingest::default();
    let mut files = Vec::new();
    let mut chunks: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    read_tree(dir, Path::new(""), store, cache, &mut files, &mut chunks, &mut stats)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let root = Blueprint::compute_root(&files);
    stats.files = files.len();
    stats.chunks = chunks.len();
    stats.bytes = chunks.values().sum();
    let bp = Blueprint {
        epoch,
        root,
        files,
        chunks: chunks
            .into_iter()
            .map(|(hash, len)| Piece { hash, len })
            .collect(),
    };
    // The blueprint itself lives beside the objects so a node can rebuild its
    // view from disk alone.
    fs::create_dir_all(store.root())?;
    fs::write(store.root().join("blueprint"), bp.text())?;
    Ok((bp, stats))
}

fn read_tree(
    root: &Path,
    rel: &Path,
    store: &Store,
    cache: Option<&Store>,
    files: &mut Vec<FileEntry>,
    chunks: &mut std::collections::BTreeMap<String, u64>,
    stats: &mut Ingest,
) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        // Fleet metadata on a stick is not part of the site.
        if name == "blueprint" || name == "blueprint.sig" || name == ".cartridge" {
            continue;
        }
        let child = rel.join(&name);
        let meta = entry.metadata()?;
        if meta.is_dir() {
            read_tree(&entry.path(), &child, store, cache, files, chunks, stats)?;
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        let mut file = fs::File::open(entry.path())?;
        let mut buf = vec![0u8; CHUNK];
        let mut pieces = Vec::new();
        let mut total = 0u64;
        loop {
            let mut filled = 0usize;
            while filled < buf.len() {
                match file.read(&mut buf[filled..])? {
                    0 => break,
                    n => filled += n,
                }
            }
            if filled == 0 {
                break;
            }
            let bytes = &buf[..filled];
            let digest = hash::bytes(bytes);
            if !store.has(&digest) {
                let linked = match cache {
                    Some(other) => other.has(&digest) && store.link_from(&digest, other).unwrap_or(false),
                    None => false,
                };
                if linked {
                    stats.linked_chunks += 1;
                } else {
                    store.put(bytes)?;
                    stats.new_chunks += 1;
                }
            }
            chunks.insert(digest.clone(), filled as u64);
            pieces.push(Piece { hash: digest, len: filled as u64 });
            total += filled as u64;
            if filled < buf.len() {
                break;
            }
        }
        if pieces.is_empty() {
            let digest = hash::bytes(b"");
            if !store.has(&digest) {
                store.put(b"")?;
                stats.new_chunks += 1;
            }
            chunks.entry(digest.clone()).or_insert(0);
            pieces.push(Piece { hash: digest, len: 0 });
        }
        let path = child.to_string_lossy().replace('\\', "/");
        files.push(FileEntry {
            ctype: content_type(&path).to_string(),
            path,
            len: total,
            pieces,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "cartridge-store-{}-{}-{}",
            tag,
            std::process::id(),
            crate::ident::random_u128() & 0xffff
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn put_get_and_dedupe() {
        let dir = tempdir("put");
        let store = Store::at(dir.join("store"));
        let (hash, new) = store.put(b"hello world").unwrap();
        assert!(new);
        assert!(store.has(&hash));
        assert_eq!(store.get(&hash).unwrap(), b"hello world");
        let (again, new) = store.put(b"hello world").unwrap();
        assert_eq!(again, hash);
        assert!(!new);
        assert_eq!(store.bytes(), 11);
        assert_eq!(store.held().len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hostile_hash_cannot_escape_the_store() {
        let dir = tempdir("traversal");
        let store = Store::at(dir.join("store"));
        assert!(!valid_hash("../../etc/passwd"));
        assert!(store.get("../../etc/passwd").is_err());
        assert!(store.path_for("../../etc/passwd").starts_with(dir.join("store")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ingest_reuses_the_previous_release_by_link() {
        let dir = tempdir("ingest");
        let one = dir.join("one");
        let two = dir.join("two");
        fs::create_dir_all(one.join("3000")).unwrap();
        fs::create_dir_all(two.join("3000")).unwrap();
        fs::write(one.join("3000/index.html"), b"<h1>one</h1>").unwrap();
        fs::write(one.join("3000/font.woff2"), vec![7u8; 9000]).unwrap();
        fs::write(two.join("3000/index.html"), b"<h1>one</h1>").unwrap();
        fs::write(two.join("3000/font.woff2"), vec![7u8; 9000]).unwrap();
        fs::write(two.join("3000/new.css"), b"body{}").unwrap();

        let store_a = Store::at(dir.join("a"));
        let (bp_a, stats_a) = ingest(&one, &store_a, None, 1).unwrap();
        assert_eq!(bp_a.files.len(), 2);
        assert_eq!(stats_a.new_chunks, 2);

        let store_b = Store::at(dir.join("b"));
        let (bp_b, stats_b) = ingest(&two, &store_b, Some(&store_a), 2).unwrap();
        assert_eq!(bp_b.files.len(), 3);
        assert_eq!(stats_b.new_chunks, 1, "only the new css should be written");
        assert_eq!(stats_b.linked_chunks, 2);
        // The shared chunk is the same object, so no bytes were copied twice.
        assert!(store_b.has(&bp_a.chunks[0].hash));
        assert!(bp_a.root != bp_b.root);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_keeps_only_what_the_blueprint_mentions() {
        let dir = tempdir("prune");
        let store = Store::at(dir.join("store"));
        let (one, _) = store.put(b"one").unwrap();
        let (_two, _) = store.put(b"two").unwrap();
        let mut keep = BTreeSet::new();
        keep.insert(one.clone());
        let (removed, freed) = store.prune(&keep);
        assert_eq!(removed, 1);
        assert_eq!(freed, 3);
        assert!(store.has(&one));
        assert_eq!(store.held().len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
