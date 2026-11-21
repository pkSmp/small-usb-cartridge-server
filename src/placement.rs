//! Which machine owns a URL.
//!
//! The cluster asks two different questions about every connection. "Which
//! machine is fastest right now" is a queueing question, answered in cluster.rs
//! from measured response times. "Which machine owns this file" is a placement
//! question, answered here.
//!
//! Placement is weighted rendezvous hashing (highest random weight): every node
//! hashes the path with its own key and stands behind the highest score, so all
//! nodes agree on an owner without talking to each other, bigger/faster boxes
//! take proportionally more of the URL space through their weight, and when a
//! box joins or leaves only its share of the URLs moves. Pinning a file to one
//! node is what keeps that file's pages in one machine's cache instead of
//! asking every box in the rack to hold a copy.

/// The path a request asks for, read from the first line of the HTTP head.
///
/// `None` means the head is not complete yet, which the balancer treats as
/// "no preference" rather than waiting forever for a client that is only
/// pre-connecting.
pub fn request_path(head: &[u8]) -> Option<String> {
    let end = head.iter().position(|b| *b == b'\n')?;
    let line = String::from_utf8_lossy(&head[..end]);
    let mut parts = line.split_whitespace();
    let _method = parts.next()?;
    let target = parts.next()?;
    // origin-form is the normal case; absolute-form arrives through a proxy.
    let path = if let Some(rest) = target.strip_prefix("http://") {
        rest.find('/').map(|i| &rest[i..]).unwrap_or("/")
    } else if let Some(rest) = target.strip_prefix("https://") {
        rest.find('/').map(|i| &rest[i..]).unwrap_or("/")
    } else if target.starts_with('/') {
        target
    } else {
        return None;
    };
    // A cache-busting query string names the same file; it must not split one
    // asset over several nodes. The fragment never reaches the server.
    let path = path.split(|c| c == '?' || c == '#').next().unwrap_or(path);
    if path.is_empty() {
        return None;
    }
    Some(path.to_string())
}

/// A stable 64-bit key for a node: the same on every box for the same peer.
pub fn node_key(text: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in text.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^ (h >> 29)
}

/// A uniform value in (0, 1) from a path and a node key. One bar of a quiet
/// SipHash-shaped mixer is plenty here: placement only needs the values to be
/// spread and repeatable, not unpredictable.
fn unit(path: &str, key: u64) -> f64 {
    let mut h = key ^ 0x9e37_79b9_7f4a_7c15;
    for b in path.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 29;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^= h >> 32;
    // The +1 keeps the value strictly positive, so ln() is finite and a weight
    // of 0 (or u == 1) can never win or divide by zero.
    ((h >> 11) as f64 + 1.0) / 9_007_199_254_740_993.0
}

/// The owner of `path` among `nodes`, given as `(weight, node key)`. Returns the
/// position in `nodes`, or `None` for an empty slice.
pub fn owner(path: &str, nodes: &[(u32, u64)]) -> Option<usize> {
    let mut best: Option<usize> = None;
    let mut best_score = 0.0f64;
    for (i, (weight, key)) in nodes.iter().enumerate() {
        // Highest random weight: w / -ln(u). Bigger weight -> higher score,
        // while u keeps the order pseudo-random per path.
        let score = (*weight).max(1) as f64 / -(unit(path, *key)).ln();
        if best.is_none() || score > best_score {
            best = Some(i);
            best_score = score;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_path_from_a_request_line() {
        assert_eq!(request_path(b"GET /a/b.css HTTP/1.1\r\n").unwrap(), "/a/b.css");
        assert_eq!(request_path(b"HEAD / HTTP/1.0\r\n").unwrap(), "/");
        assert_eq!(request_path(b"GET /x?v=2 HTTP/1.1\r\n").unwrap(), "/x");
        assert_eq!(
            request_path(b"GET http://box:3000/site/ HTTP/1.1\r\n").unwrap(),
            "/site/"
        );
        // Incomplete head: no decision yet.
        assert!(request_path(b"GET /slow").is_none());
        assert!(request_path(b"").is_none());
        // CONNECT and other odd targets are not files.
        assert!(request_path(b"CONNECT box:443 HTTP/1.1\r\n").is_none());
    }

    #[test]
    fn owner_is_stable_and_spreads_by_weight() {
        let nodes = [
            (100u32, node_key("10.0.0.1")),
            (200, node_key("10.0.0.2")),
            (400, node_key("10.0.0.3")),
        ];
        let mut hits = [0usize; 3];
        for i in 0..30_000 {
            let path = format!("/asset/{}.js", i);
            let a = owner(&path, &nodes).unwrap();
            let b = owner(&path, &nodes).unwrap();
            assert_eq!(a, b, "placement must be repeatable");
            hits[a] += 1;
        }
        // Roughly 1/7, 2/7, 4/7 of the URL space.
        let share = |n: usize| hits[n] as f64 / 30_000.0;
        assert!((share(0) - 0.143).abs() < 0.03, "small node got {}", share(0));
        assert!((share(1) - 0.286).abs() < 0.03, "middle node got {}", share(1));
        assert!((share(2) - 0.571).abs() < 0.03, "big node got {}", share(2));
    }

    #[test]
    fn joining_moves_only_the_new_share() {
        let a = [(100u32, node_key("a")), (100, node_key("b"))];
        let b = [(100u32, node_key("a")), (100, node_key("b")), (100, node_key("c"))];
        let mut moved = 0;
        for i in 0..20_000 {
            let path = format!("/f{}", i);
            match owner(&path, &b) {
                // The newcomer won this URL: it moved.
                Some(2) => moved += 1,
                // Anything else must still belong to the node it had before.
                other => assert_eq!(other.map(|p| p), owner(&path, &a)),
            }
        }
        // A third node should claim about a third, and disturb nothing else.
        assert!(moved > 5_500 && moved < 8_000, "moved {}", moved);
    }

    #[test]
    fn empty_cluster_has_no_owner() {
        assert!(owner("/", &[]).is_none());
    }
}
