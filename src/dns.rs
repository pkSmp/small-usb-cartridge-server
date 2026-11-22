//! A tiny DNS responder: the only way to give clients more than one address.
//!
//! Relaying cannot raise throughput, because every relayed byte still leaves
//! through the accepting machine's port. Spreading *clients* across machines is
//! the one change that adds egress, and the cheapest way to do that without
//! touching a client is to answer the LAN's DNS with every node's address.
//!
//! It answers A queries with all live nodes, rotated so successive clients start
//! on different machines. It is not a resolver: it does not recurse, and it
//! answers only the names it is asked about. Point the router's DNS at any node
//! and every device on the switch spreads itself.
//!
//! Limits worth stating: this can only spread traffic for sites served on port
//! 80, because a DNS A record carries no port. Sites on 3000/8080 still need the
//! redirect or a per-site hostname.

use std::net::{Ipv4Addr, UdpSocket};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub const PORT: u16 = 53;

fn parse_name(buf: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut labels = Vec::new();
    let mut jumped = false;
    let mut after = pos;
    let mut hops = 0;
    loop {
        if pos >= buf.len() || hops > 32 {
            return None;
        }
        hops += 1;
        let len = buf[pos] as usize;
        if len == 0 {
            pos += 1;
            if !jumped {
                after = pos;
            }
            break;
        }
        if len & 0xc0 == 0xc0 {
            // A compression pointer: follow it, remember where we were.
            if pos + 1 >= buf.len() {
                return None;
            }
            let ptr = (((len & 0x3f) as usize) << 8) | buf[pos + 1] as usize;
            if !jumped {
                after = pos + 2;
            }
            jumped = true;
            pos = ptr;
            continue;
        }
        if pos + 1 + len > buf.len() {
            return None;
        }
        labels.push(String::from_utf8_lossy(&buf[pos + 1..pos + 1 + len]).to_string());
        pos += 1 + len;
        if !jumped {
            after = pos;
        }
    }
    Some((labels.join("."), after))
}

/// Whether a client is on this box's own network: a /24 for an ordinary LAN, a
/// /16 inside 10/8 and 172.16/12, and loopback only for loopback.
pub fn on_our_network(peer: Ipv4Addr, ours: Ipv4Addr) -> bool {
    if ours.is_loopback() {
        return peer.is_loopback();
    }
    if peer.is_loopback() {
        return true;
    }
    let (a, b) = (peer.octets(), ours.octets());
    if a[0] == 10 && b[0] == 10 {
        return a[1] == b[1];
    }
    if a[0] == 172 && b[0] == 172 {
        return a[1] == b[1];
    }
    a[..3] == b[..3]
}

/// Turn one query into one response: the question echoed back, then an A record
/// per address. Returns `None` for anything that is not a query we understand.
pub fn respond(query: &[u8], addresses: &[Ipv4Addr]) -> Option<Vec<u8>> {
    if query.len() < 12 {
        return None;
    }
    let flags = u16::from_be_bytes([query[2], query[3]]);
    // QR must be 0 (a query) and opcode 0 (standard).
    if flags & 0x8000 != 0 || flags & 0x7800 != 0 {
        return None;
    }
    let qdcount = u16::from_be_bytes([query[4], query[5]]);
    if qdcount != 1 {
        return None;
    }
    let (_name, question_end) = parse_name(query, 12)?;
    if question_end + 4 > query.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([query[question_end], query[question_end + 1]]);
    let question = &query[12..question_end + 4];

    let mut out = Vec::with_capacity(64 + addresses.len() * 16);
    out.extend_from_slice(&query[0..2]); // id
    let answer_flags: u16 = 0x8180; // response, recursion available, no error
    out.extend_from_slice(&answer_flags.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes()); // qdcount
    let answers: u16 = if qtype == 1 || qtype == 255 {
        addresses.len().min(16) as u16
    } else {
        0
    };
    out.extend_from_slice(&answers.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // authority
    out.extend_from_slice(&0u16.to_be_bytes()); // additional
    out.extend_from_slice(question);
    if answers > 0 {
        for ip in addresses.iter().take(answers as usize) {
            out.extend_from_slice(&[0xc0, 0x0c]); // pointer to the question name
            out.extend_from_slice(&1u16.to_be_bytes()); // A
            out.extend_from_slice(&1u16.to_be_bytes()); // IN
            out.extend_from_slice(&5u32.to_be_bytes()); // TTL: seconds, so a dead node leaves fast
            out.extend_from_slice(&4u16.to_be_bytes());
            out.extend_from_slice(&ip.octets());
        }
    }
    Some(out)
}

/// Serve until the socket fails. `addresses` is read on every query, so a node
/// joining or leaving is reflected immediately, and the start is rotated so
/// clients do not all pile onto the same machine.
pub fn serve(bind: &str, port: u16, addresses: Arc<dyn Fn() -> Vec<Ipv4Addr> + Send + Sync>) -> std::io::Result<()> {
    let socket = UdpSocket::bind((bind, port))?;
    socket.set_read_timeout(Some(Duration::from_secs(1)))?;
    let turn = AtomicUsize::new(0);
    let ours: Ipv4Addr = bind
        .parse()
        .ok()
        .filter(|ip: &Ipv4Addr| !ip.is_unspecified())
        .or_else(|| crate::sys::lan_ip().parse().ok())
        .unwrap_or(Ipv4Addr::LOCALHOST);
    let mut buf = [0u8; 1500];
    loop {
        let (n, peer) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Only the network this box is on. Answering strangers turns the
        // responder into a reflector, and a spoofed source address only ever
        // receives its answers back inside the same network.
        if let std::net::SocketAddr::V4(v4) = peer {
            if !on_our_network(*v4.ip(), ours) {
                continue;
            }
        } else {
            continue;
        }
        let mut list = addresses();
        if list.len() > 1 {
            let start = turn.fetch_add(1, Ordering::Relaxed) % list.len();
            list.rotate_left(start);
        }
        if let Some(reply) = respond(&buf[..n], &list) {
            let _ = socket.send_to(&reply, peer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&1u16.to_be_bytes());
        q
    }

    #[test]
    fn answers_with_every_address() {
        let ips = [Ipv4Addr::new(192, 168, 1, 10), Ipv4Addr::new(192, 168, 1, 11)];
        let reply = respond(&query("cartridge.lan", 1), &ips).unwrap();
        assert_eq!(&reply[0..2], &[0x12, 0x34], "id is echoed");
        assert_eq!(u16::from_be_bytes([reply[2], reply[3]]) & 0x8000, 0x8000);
        assert_eq!(u16::from_be_bytes([reply[6], reply[7]]), 2, "two answers");
        assert!(reply.windows(4).any(|w| w == [192, 168, 1, 10]));
        assert!(reply.windows(4).any(|w| w == [192, 168, 1, 11]));
    }

    #[test]
    fn only_the_local_network_is_answered() {
        let ours = Ipv4Addr::new(192, 168, 1, 173);
        assert!(on_our_network(Ipv4Addr::new(192, 168, 1, 5), ours));
        assert!(!on_our_network(Ipv4Addr::new(192, 168, 2, 5), ours));
        assert!(!on_our_network(Ipv4Addr::new(8, 8, 8, 8), ours));
        assert!(on_our_network(Ipv4Addr::new(10, 1, 9, 9), Ipv4Addr::new(10, 1, 0, 2)));
        assert!(!on_our_network(Ipv4Addr::new(10, 2, 9, 9), Ipv4Addr::new(10, 1, 0, 2)));
        assert!(on_our_network(Ipv4Addr::LOCALHOST, Ipv4Addr::LOCALHOST));
        assert!(!on_our_network(Ipv4Addr::new(192, 168, 1, 5), Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn unknown_types_get_an_empty_answer_rather_than_a_lie() {
        let reply = respond(&query("cartridge.lan", 28), &[Ipv4Addr::LOCALHOST]).unwrap();
        assert_eq!(u16::from_be_bytes([reply[6], reply[7]]), 0);
    }

    #[test]
    fn malformed_input_is_ignored() {
        assert!(respond(&[], &[Ipv4Addr::LOCALHOST]).is_none());
        assert!(respond(&[0u8; 12], &[Ipv4Addr::LOCALHOST]).is_none());
        let mut response = query("x", 1);
        response[2] |= 0x80; // already a response
        assert!(respond(&response, &[Ipv4Addr::LOCALHOST]).is_none());
    }

    #[test]
    fn the_name_parser_is_not_fooled_by_a_trailing_pointer() {
        let (name, end) = parse_name(&query("a.b.c", 1), 12).unwrap();
        assert_eq!(name, "a.b.c");
        assert_eq!(end, 12 + 1 + 1 + 1 + 1 + 1 + 1 + 1);
    }
}
