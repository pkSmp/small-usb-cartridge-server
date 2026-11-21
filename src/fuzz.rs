//! A deterministic fuzz harness for every parser that reads untrusted bytes.
//!
//! No crates and no external fuzzer: a xorshift generator mutates real
//! fixtures (a gzip stream, a PNG, a JSON document, a release blueprint) and
//! feeds the result to the decoder. The point is not coverage but the two
//! things a static test misses: an index panic and an unbounded allocation,
//! both of which abort a serving process.
//!
//! Run with the default 2000 rounds in `cargo test`, or scale it with
//! `CARTRIDGE_FUZZ_ROUNDS=200000 cargo test --release fuzz -- --nocapture`.

use std::panic::{self, AssertUnwindSafe};

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    fn next(&mut self) -> u64 {
        // xorshift64*, the smallest generator with a decent period.
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }

    /// Random bytes, occasionally empty or one byte long.
    fn blob(&mut self, max: usize) -> Vec<u8> {
        let len = self.below(max + 1);
        (0..len).map(|_| (self.next() >> 24) as u8).collect()
    }
}

struct Seeds {
    gzip: Vec<u8>,
    png: Vec<u8>,
    json: Vec<u8>,
    blueprint: Vec<u8>,
}

#[derive(Default)]
pub struct Report {
    pub cases: u64,
    pub crashes: u64,
    pub first: Option<String>,
}

impl Report {
    fn record(&mut self, f: impl FnOnce() + std::panic::UnwindSafe) {
        self.cases += 1;
        let result = panic::catch_unwind(f);
        if let Err(payload) = result {
            self.crashes += 1;
            if self.first.is_none() {
                let text = payload
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panic without a message".to_string());
                self.first = Some(text);
            }
        }
    }
}

/// Flip, truncate, splice and extend a fixture. Garbage alone rarely reaches
/// the second half of a decoder; a damaged valid file does.
fn mutate(rng: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut out = seed.to_vec();
    if out.is_empty() {
        return rng.blob(32);
    }
    match rng.below(5) {
        0 => {
            for _ in 0..1 + rng.below(4) {
                let i = rng.below(out.len());
                out[i] ^= 1 << rng.below(8);
            }
        }
        1 => {
            let cut = rng.below(out.len());
            out.truncate(cut);
        }
        2 => {
            let i = rng.below(out.len());
            let byte = (rng.next() >> 32) as u8;
            out.insert(i, byte);
        }
        3 => {
            let i = rng.below(out.len());
            let n = 1 + rng.below(8).min(out.len() - i);
            for _ in 0..n {
                if i < out.len() {
                    out.remove(i);
                }
            }
        }
        _ => {
            out.extend_from_slice(&rng.blob(16));
        }
    }
    out
}

fn build_seeds() -> Seeds {
    let image = {
        let mut img = crate::png::Image::new(5, 3);
        for y in 0..3 {
            for x in 0..5 {
                img.put(x, y, ((x * 40) as u8, (y * 80) as u8, 200));
            }
        }
        crate::png::encode_rgb(&img)
    };
    let blueprint = {
        let dir = std::env::temp_dir().join(format!("cartridge-fuzz-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let site = dir.join("site/8080");
        std::fs::create_dir_all(&site).ok();
        std::fs::write(site.join("index.html"), b"<html>seed</html>").ok();
        std::fs::write(dir.join("site/data.json"), b"{\"a\":1}").ok();
        let store = crate::chunks::Store::at(dir.join("store"));
        match crate::chunks::ingest(&dir.join("site"), &store, None, 1) {
            Ok((bp, _)) => bp.text().into_bytes(),
            Err(_) => b"epoch 1\n".to_vec(),
        }
    };
    Seeds {
        gzip: crate::gzip::gzip(b"the quick brown fox jumps over the lazy dog, twice over"),
        png: image,
        json: br#"{"sites":[{"port":8080,"root":"a","flags":[true,false,null]}],"n":1.5e3}"#.to_vec(),
        blueprint,
    }
}

/// Every case is a full parse of hostile bytes. A returned error is a pass;
/// a panic or an abort is a failure.
pub fn run(rounds: u64) -> Report {
    let seeds = build_seeds();
    let mut rng = Rng::new(0x9E37_79B9_7F4A_7C15);
    let mut report = Report::default();

    for _ in 0..rounds {
        let case = rng.below(6);
        let data = match case {
            0 => mutate(&mut rng, &seeds.gzip),
            1 => mutate(&mut rng, &seeds.png),
            2 => mutate(&mut rng, &seeds.json),
            3 => mutate(&mut rng, &seeds.blueprint),
            _ => rng.blob(96),
        };
        match case {
            0 => {
                let copy = data.clone();
                report.record(AssertUnwindSafe(move || {
                    let _ = crate::inflate::gzip_decompress(&copy);
                }));
            }
            1 => {
                let copy = data.clone();
                report.record(AssertUnwindSafe(move || {
                    let _ = crate::png::decode(&copy);
                }));
            }
            2 => {
                let text = String::from_utf8_lossy(&data).into_owned();
                report.record(AssertUnwindSafe(move || {
                    let _ = crate::json::parse(&text);
                }));
            }
            3 => {
                let text = String::from_utf8_lossy(&data).into_owned();
                report.record(AssertUnwindSafe(move || {
                    let _ = crate::blueprint::Blueprint::parse(&text);
                }));
            }
            4 => {
                let copy = data.clone();
                report.record(AssertUnwindSafe(move || {
                    let _ = crate::inflate::zlib_decompress(&copy);
                }));
            }
            _ => {
                let text = String::from_utf8_lossy(&data).into_owned();
                report.record(AssertUnwindSafe(move || {
                    let _ = crate::json::parse(&text);
                }));
            }
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_parser_survives_mutated_input() {
        let rounds = std::env::var("CARTRIDGE_FUZZ_ROUNDS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(2_000);
        let report = run(rounds);
        assert_eq!(
            report.crashes, 0,
            "{} of {} fuzz cases panicked, first: {:?}",
            report.crashes, report.cases, report.first
        );
    }
}
