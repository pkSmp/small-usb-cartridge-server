//! Minimal HTTP load generator: std only, so it builds on Windows and Linux
//! with the same source.
//!
//!   loadgen bench HOST PORT PATH CONNS REQUESTS_PER_CONN
//!   loadgen download HOST PORT PATH [CONNS]
//!
//! bench opens CONNS keep-alive connections at once and sends
//! REQUESTS_PER_CONN requests down each, reporting throughput and p50/p99.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

fn connect(host: &str, port: u16) -> Result<TcpStream, String> {
    let addr = format!("{}:{}", host, port);
    let stream = TcpStream::connect(&addr).map_err(|e| format!("connect {}: {}", addr, e))?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
    Ok(stream)
}

fn read_response(reader: &mut BufReader<TcpStream>) -> Result<(usize, usize), String> {
    let mut line = String::new();
    let mut status = 0usize;
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    let mut first = true;
    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed".into());
        }
        if first {
            let mut parts = line.split_whitespace();
            parts.next();
            status = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            first = false;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        let lower = trimmed.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().ok();
        } else if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            chunked = true;
        }
    }
    let mut body_len = 0usize;
    if chunked {
        loop {
            let mut size_line = String::new();
            reader.read_line(&mut size_line).map_err(|e| e.to_string())?;
            let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
            if size == 0 {
                let mut crlf = String::new();
                let _ = reader.read_line(&mut crlf);
                break;
            }
            let mut buf = vec![0u8; size + 2];
            reader.read_exact(&mut buf).map_err(|e| e.to_string())?;
            body_len += size;
        }
    } else if let Some(len) = content_length {
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).map_err(|e| e.to_string())?;
        body_len = len;
    }
    Ok((status, body_len))
}

fn bench(host: &str, port: u16, path: &str, conns: usize, requests: usize) {
    let barrier = Arc::new(Barrier::new(conns + 1));
    let mut handles = Vec::with_capacity(conns);
    let start_all = Instant::now();
    for _ in 0..conns {
        let barrier = barrier.clone();
        let host = host.to_string();
        let path = path.to_string();
        handles.push(std::thread::spawn(move || {
            std::thread::Builder::new()
                .stack_size(64 * 1024)
                .spawn(move || {
                    barrier.wait();
                    let mut latencies: Vec<u32> = Vec::with_capacity(requests);
                    let mut errors = 0usize;
                    let stream = match connect(&host, port) {
                        Ok(s) => s,
                        Err(_) => return (latencies, 1usize),
                    };
                    let clone = match stream.try_clone() {
                        Ok(c) => c,
                        Err(_) => return (latencies, 1usize),
                    };
                    let mut reader = BufReader::with_capacity(16 * 1024, clone);
                    let mut writer = stream;
                    let request = format!(
                        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: keep-alive\r\n\r\n",
                        path, host, port
                    );
                    for _ in 0..requests {
                        let t0 = Instant::now();
                        if writer.write_all(request.as_bytes()).is_err() {
                            errors += 1;
                            break;
                        }
                        match read_response(&mut reader) {
                            Ok(_) => latencies.push(t0.elapsed().as_micros().min(u32::MAX as u128) as u32),
                            Err(_) => {
                                errors += 1;
                                break;
                            }
                        }
                    }
                    (latencies, errors)
                })
                .map(|h| h.join().unwrap())
                .unwrap_or((Vec::new(), 1))
        }));
    }
    barrier.wait();
    let mut all: Vec<u32> = Vec::with_capacity(conns * requests);
    let mut errors = 0usize;
    for h in handles {
        match h.join() {
            Ok((mut lat, err)) => {
                all.append(&mut lat);
                errors += err;
            }
            Err(_) => errors += 1,
        }
    }
    let elapsed = start_all.elapsed();
    all.sort_unstable();
    let pct = |p: f64| -> f64 {
        if all.is_empty() {
            return 0.0;
        }
        let idx = ((all.len() as f64 * p) as usize).min(all.len() - 1);
        all[idx] as f64 / 1000.0
    };
    let rps = all.len() as f64 / elapsed.as_secs_f64();
    println!(
        "conns={} reqs/conn={} ok={} errors={} time={:.2}s rps={:.0} p50={:.2}ms p99={:.2}ms",
        conns,
        requests,
        all.len(),
        errors,
        elapsed.as_secs_f64(),
        rps,
        pct(0.50),
        pct(0.99)
    );
}

fn download(host: &str, port: u16, path: &str, conns: usize) {
    let mut total_bytes = 0usize;
    let start = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..conns {
        let host = host.to_string();
        let path = path.to_string();
        handles.push(std::thread::spawn(move || {
            let stream = match connect(&host, port) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("{}", e);
                    return 0usize;
                }
            };
            let clone = match stream.try_clone() {
                Ok(c) => c,
                Err(_) => return 0,
            };
            let mut reader = BufReader::with_capacity(256 * 1024, clone);
            let mut writer = stream;
            let request = format!(
                "GET {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n\r\n",
                path, host, port
            );
            if writer.write_all(request.as_bytes()).is_err() {
                return 0;
            }
            match read_response(&mut reader) {
                Ok((_, len)) => len,
                Err(e) => {
                    eprintln!("{}", e);
                    0
                }
            }
        }));
    }
    for h in handles {
        total_bytes += h.join().unwrap_or(0);
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "download {} x {}: {:.1} MB in {:.2}s = {:.1} MB/s",
        conns,
        path,
        total_bytes as f64 / 1_048_576.0,
        secs,
        (total_bytes as f64 / 1_048_576.0) / secs
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 6 {
        eprintln!("usage: loadgen bench HOST PORT PATH CONNS REQUESTS | loadgen download HOST PORT PATH [CONNS]");
        std::process::exit(2);
    }
    match args[1].as_str() {
        "bench" => {
            let port: u16 = args[3].parse().expect("port");
            let conns: usize = args[5].parse().expect("conns");
            let requests: usize = args.get(6).and_then(|v| v.parse().ok()).unwrap_or(100);
            bench(&args[2], port, &args[4], conns, requests);
        }
        "download" => {
            let port: u16 = args[3].parse().expect("port");
            let conns: usize = args.get(5).and_then(|v| v.parse().ok()).unwrap_or(1);
            download(&args[2], port, &args[4], conns);
        }
        _ => std::process::exit(2),
    }
}
