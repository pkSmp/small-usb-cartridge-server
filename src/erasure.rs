//! Reed-Solomon over GF(256): the space that redundancy actually costs.
//!
//! Keeping R copies of everything is simple and fast but expensive: two copies
//! of a 100 GB release is 200 GB. Erasure coding an object into k data and m
//! parity shards costs (k+m)/k of its size — 1.4x for 10+4 — and still survives
//! any m lost shards. It is CPU work, which is exactly why it belongs to the
//! idle class: it only ever runs when serving has nothing to do.
//!
//! The code is systematic: the first k shards are the data itself, so a healthy
//! fleet never has to decode anything to serve or verify.

/// The field, built once per call: 256 entries each for the log and exp tables.
pub struct Gf {
    exp: [u8; 512],
    log: [u8; 256],
}

impl Gf {
    pub fn new() -> Gf {
        let mut gf = Gf { exp: [0; 512], log: [0; 256] };
        let mut x: u16 = 1;
        for i in 0..255 {
            gf.exp[i] = x as u8;
            gf.log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 0x11d;
            }
        }
        for i in 255..512 {
            gf.exp[i] = gf.exp[i - 255];
        }
        gf
    }

    pub fn mul(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            return 0;
        }
        self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize]
    }

    pub fn inv(&self, a: u8) -> u8 {
        // Division by zero is a caller bug; returning zero keeps it local.
        if a == 0 {
            return 0;
        }
        self.exp[255 - self.log[a as usize] as usize]
    }

    pub fn div(&self, a: u8, b: u8) -> u8 {
        if a == 0 {
            return 0;
        }
        self.mul(a, self.inv(b))
    }

    fn pow(&self, a: u8, mut n: usize) -> u8 {
        let mut out = 1u8;
        let mut base = a;
        while n > 0 {
            if n & 1 == 1 {
                out = self.mul(out, base);
            }
            base = self.mul(base, base);
            n >>= 1;
        }
        out
    }
}

/// The n x k encoding matrix: identity on top, a Cauchy block below. Any k rows
/// are independent, which is what makes "lose any m shards" true.
fn matrix(k: usize, m: usize) -> Vec<Vec<u8>> {
    let gf = Gf::new();
    let mut rows = Vec::with_capacity(k + m);
    for i in 0..k {
        let mut row = vec![0u8; k];
        row[i] = 1;
        rows.push(row);
    }
    for i in 0..m {
        let x = (k + i) as u8;
        let mut row = Vec::with_capacity(k);
        for j in 0..k {
            let y = j as u8;
            // x and y never coincide because x >= k > j.
            row.push(gf.inv(x ^ y));
        }
        rows.push(row);
    }
    rows
}

fn invert(gf: &Gf, input: &[Vec<u8>]) -> Option<Vec<Vec<u8>>> {
    let n = input.len();
    let mut a: Vec<Vec<u8>> = input.to_vec();
    let mut inv: Vec<Vec<u8>> = (0..n)
        .map(|i| {
            let mut row = vec![0u8; n];
            row[i] = 1;
            row
        })
        .collect();
    for col in 0..n {
        let pivot = (col..n).find(|r| a[*r][col] != 0)?;
        if pivot != col {
            a.swap(pivot, col);
            inv.swap(pivot, col);
        }
        let p = a[col][col];
        let p_inv = gf.inv(p);
        for j in 0..n {
            a[col][j] = gf.mul(a[col][j], p_inv);
            inv[col][j] = gf.mul(inv[col][j], p_inv);
        }
        for r in 0..n {
            if r == col {
                continue;
            }
            let factor = a[r][col];
            if factor == 0 {
                continue;
            }
            for j in 0..n {
                a[r][j] ^= gf.mul(factor, a[col][j]);
                inv[r][j] ^= gf.mul(factor, inv[col][j]);
            }
        }
    }
    Some(inv)
}

/// Encode `data` (k shards, all the same length) into k + m shards.
pub fn encode(data: &[Vec<u8>], m: usize) -> Vec<Vec<u8>> {
    let k = data.len();
    let len = data.first().map(|s| s.len()).unwrap_or(0);
    let gf = Gf::new();
    let rows = matrix(k, m);
    let mut out: Vec<Vec<u8>> = data.to_vec();
    for i in 0..m {
        let mut shard = vec![0u8; len];
        let row = &rows[k + i];
        for (j, piece) in data.iter().enumerate() {
            let coef = row[j];
            if coef == 0 || len == 0 {
                continue;
            }
            for t in 0..len {
                shard[t] ^= gf.mul(coef, piece[t]);
            }
        }
        out.push(shard);
    }
    out
}

/// Rebuild the k data shards from any k shards, given as `(index, bytes)`.
/// Indexes 0..k are data, k..k+m are parity.
pub fn decode(shards: &[(usize, Vec<u8>)], k: usize, m: usize, len: usize) -> Option<Vec<Vec<u8>>> {
    if shards.len() < k {
        return None;
    }
    let gf = Gf::new();
    let all = matrix(k, m);
    let picked = &shards[..k];
    if picked.iter().any(|(i, s)| *i >= k + m || s.len() != len) {
        return None;
    }
    let a: Vec<Vec<u8>> = picked.iter().map(|(i, _)| all[*i].clone()).collect();
    let inv = invert(&gf, &a)?;
    let mut data: Vec<Vec<u8>> = (0..k).map(|_| vec![0u8; len]).collect();
    for row in 0..k {
        for col in 0..k {
            let coef = inv[row][col];
            if coef == 0 {
                continue;
            }
            let shard = &picked[col].1;
            for t in 0..len {
                data[row][t] ^= gf.mul(coef, shard[t]);
            }
        }
    }
    Some(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(k: usize, len: usize) -> Vec<Vec<u8>> {
        (0..k)
            .map(|s| {
                let mut v = vec![0u8; len];
                let mut x = (s as u32 + 1).wrapping_mul(0x9e37_79b9);
                for b in v.iter_mut() {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    *b = (x & 0xff) as u8;
                }
                v
            })
            .collect()
    }

    #[test]
    fn any_two_of_six_shards_rebuild_the_data() {
        let data = sample(4, 97);
        let shards = encode(&data, 2);
        assert_eq!(shards.len(), 6);
        // The data shards are systematic: shard i is the input, unchanged.
        for i in 0..4 {
            assert_eq!(shards[i], data[i]);
        }
        // Every pair of failures, including two data shards, must recover.
        for a in 0..6 {
            for b in (a + 1)..6 {
                let survivors: Vec<(usize, Vec<u8>)> = (0..6)
                    .filter(|i| *i != a && *i != b)
                    .map(|i| (i, shards[i].clone()))
                    .collect();
                let back = decode(&survivors, 4, 2, 97).unwrap();
                assert_eq!(back, data, "failed dropping {} and {}", a, b);
            }
        }
    }

    #[test]
    fn ten_plus_four_survives_a_real_outage() {
        let data = sample(10, 1024);
        let shards = encode(&data, 4);
        for drop in [vec![0usize, 3, 7, 13], vec![1, 2, 11, 12], vec![5, 6, 8, 9]] {
            let survivors: Vec<(usize, Vec<u8>)> = (0..14)
                .filter(|i| !drop.contains(i))
                .map(|i| (i, shards[i].clone()))
                .collect();
            let back = decode(&survivors, 10, 4, 1024).unwrap();
            assert_eq!(back, data);
        }
    }

    #[test]
    fn too_few_shards_fail_instead_of_lying() {
        let data = sample(4, 32);
        let shards = encode(&data, 2);
        let survivors: Vec<(usize, Vec<u8>)> = shards.into_iter().take(3).enumerate().collect();
        assert!(decode(&survivors, 4, 2, 32).is_none());
    }

    #[test]
    fn field_arithmetic_behaves() {
        let gf = Gf::new();
        assert_eq!(gf.mul(0, 99), 0);
        assert_eq!(gf.mul(1, 99), 99);
        for a in 1..=255u8 {
            assert_eq!(gf.mul(a, gf.inv(a)), 1, "inverse of {}", a);
            assert_eq!(gf.div(gf.mul(a, 7), 7), a);
        }
    }
}
