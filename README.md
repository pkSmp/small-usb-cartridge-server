# Small USB Cartridge Server

**One Rust binary that turns a small PC into a cartridge console.** Plug a USB stick
into the machine and the sites on it appear on your LAN; the server reads the stick,
copies the sites into RAM and rebuilds the boot art. Unplug it and the last release
keeps serving. Web server, import, art and status are all one
1.0 MB executable with **zero dependencies** - not even a crate.

> **Built for a fanless Chromebox on a shelf.** This is the whole stack for one
> machine: an HP Chromebox G2 (2-core Celeron 3865U, 12 GB RAM, Debian 13) driving a
> TV as the console. It boots in 12.8 s, the web tier idles at ~3 MB RSS, and it
> serves **55k requests/s on loopback** with a 3.5 ms p99 at 128 connections. No
> nginx, no Python, no shell scripts at runtime.

![Platform](https://img.shields.io/badge/platform-Linux-lightgrey) ![Runtime](https://img.shields.io/badge/runtime-Rust%20std%20only-orange) ![Binary](https://img.shields.io/badge/binary-1.0%20MB-black) ![Throughput](https://img.shields.io/badge/throughput-55k%20rps-blue)

---
## What it does

| | |
|---|---|
| **Cartridge in, sites out** | A udev rule runs `cartridge import` on the new block device. The stick is mounted read-only, checked for `config.json`, and published as a new release. |
| **Served from RAM** | The live release is copied into a tmpfs and served from memory, so a page view never touches the disk. |
| **Folders are ports** | `3000/` becomes `http://host:3000/`, and a root `index.html` becomes port 80. No config needed. |
| **Boot art** | `boot.png` is converted to coloured half-block cells (two source rows per cell). A picture bigger than the console is shrunk to the largest size that still fits, keeping its shape and leaving rows for the report below, so any image size works; 1, 2, 4 and 8 bit PNGs are read. Near-black pixels are left blank (grey and dark grey are kept), near-identical shades are merged, and up to 15 non-black palette colours are used. |
| **No wasted writes** | Files whose name and SHA-256 already match the running release are hard-linked into the new release instead of being written again. Replaying the same stick writes 0 bytes. |
| **Live status screen** | Under the art: the Markdown site table, the machine and staging facts, and the newest import and boot messages as plain text, redrawn four times a second so a line appears as it is logged. Big text uses the brightest colour from the art; small labels use a reserved bright red so they stay readable, falling back to a saturated red or green from the art when no palette slot is free. Storage reports the internal filesystem's actual used and free space, so the OS, the server binary and every installed program are included; the removable cartridge is never counted. |

## Setup (5 minutes)

1. **Build it.** Rust 1.85+ and nothing else - there are no crates, so the build works
   offline:
   ```
   cargo build --release          # target/release/cartridge
   ```
2. **Install it.**
   ```
   install -m 0755 target/release/cartridge /usr/local/bin/cartridge
   install -m 0644 systemd/*.service /etc/systemd/system/
   install -m 0644 systemd/90-cartridge.rules /etc/udev/rules.d/
   install -m 0644 systemd/99-cartridge-net.conf /etc/sysctl.d/ && sysctl -p /etc/sysctl.d/99-cartridge-net.conf
   systemctl daemon-reload && systemctl enable --now cartridge-web cartridge-console
   ```
3. **Make a cartridge.** Format a stick as FAT32 or ext4, add a `config.json`, one
   folder per port and an optional `boot.png`, then plug it in. The new sites appear
   on screen and in the import log.
4. **Check it.**
   ```
   cartridge status
   ```

## The cartridge

```
config.json     {} - marks the stick as a cartridge
3000/index.html -> http://<host>:3000/
8080/           -> http://<host>:8080/
index.html      -> http://<host>/          (port 80)
boot.png        optional; becomes the TV art
README.txt      optional, ignored by the importer
```

The folders are the configuration: a numeric folder name is the port. A stick without
`config.json` is left alone, so an ordinary USB drive is never imported by accident.
The server stages the sites into RAM automatically; there are no `ram_mb` or
`storage_mb` limits to configure.

## Where things live

```
src/                     the whole server, module per job
  web.rs                 epoll + SO_REUSEPORT + sendfile file server
  import.rs              mount, validate, publish
  publish.rs             copy the live release into RAM
  machine.rs             RAM, internal disks and staging facts
  art.rs  bootart.rs     PNG -> half-block art, framebuffer draw + verify
  console.rs             site table and machine facts under the art
  hash.rs                SHA-256, used to skip identical files
  inflate.rs  png.rs  json.rs   own decoders, no crates
  status.rs  theme.rs    report, coloured from the art palette
systemd/                 units + the udev rule that imports a stick
bench/                   the Rust load generator used for the numbers below
art/boot.png             the default boot picture
```

## Measured

Same machine, same 1360-byte file, before (nginx + Python + shell) and after:

| Metric | Before | Cartridge |
|---|---|---|
| Boot to multi-user | 36.0 s | **12.8 s** |
| Installed packages | 820 | **571** |
| Web RSS idle | 2.9 MB | 3.0 MB |
| Loopback rps, 1 connection | 516 | **29,724** |
| Loopback rps, 32 connections | 10,943 (36 errors) | **55,372** (0) |
| Loopback rps, 512 connections | 8,473 (3,293 errors) | **47,558** (0) |
| p99 at 128 connections | 227 ms | **3.8 ms** |
| 50 MB download, loopback | 194 MB/s | **1,665 MB/s** |

Serving the same file as the previous build of this server, on the same two cores,
with the load generator sharing those cores:

| Metric | Previous build | This build |
|---|---|---|
| 1 connection | 13,649 rps | **30,696 rps** |
| 8 connections | 22,401 rps, p99 0.44 ms | **50,509 rps**, p99 0.24 ms |
| 32 connections | 22,742 rps, p99 1.66 ms | **55,372 rps**, p99 0.83 ms |
| 128 connections | 24,007 rps, p99 8.45 ms | **52,409 rps**, p99 3.58 ms |
| 48k misses (404 storm), 32 conns | 3,735 rps, connection closed | **56,532 rps**, kept alive |
| 1 MB x 50 downloads | 884 MB/s | **939 MB/s** |

Over the box's Wi-Fi link both builds sit at ~1.6k rps with a 4.5 ms p50: the radio,
not the server, is the limit there, so the gain only appears once a client is fast
enough to make the box the bottleneck. Reproduce with `bench/`:
```
loadgen bench HOST PORT PATH CONNS REQUESTS
loadgen sweep HOST PORT PATH MAX_CONNS REQUESTS   # ramps until the knee
loadgen download HOST PORT PATH [CONNS]
```

## How it works

- **Web tier.** One worker per CPU thread (`--workers 0`, the default, is detected at
  start; this box starts two), each pinned to its own core and sharing the port with
  `SO_REUSEPORT`, so the kernel spreads connections without a lock. Every worker runs
  an edge-triggered epoll loop and accepts in batches with `accept4`. A file at or
  below 256 KiB becomes one finished response in RAM - header, Date hole and body - so
  a hot request costs a read, a 29-byte Date patch and one `write(2)`, with no `stat`,
  no `open` and no allocation; a path is trusted for one second before it is checked
  again, and 404s are cached the same way. Larger files keep the `sendfile` path from
  the RAM copy. It drops to `nobody` after binding.
- **Import.** Mounts read-only through the mount syscall, copies the sites into
  `.staging`, then renames into `releases/<timestamp>` and flips a symlink. Old
  releases are pruned. Nothing is written to the stick.
- **Art.** `boot.png` is decoded by the built-in PNG reader, near-black pixels are
  left blank, near-identical shades are merged, and the remaining colours fill up to
  15 non-black palette slots. The half-block cells are verified by reading the
  framebuffer back and comparing every sampled pixel with the expectation.
- **Status.** Reads `/proc`, the release directory and the import log. The console
  draws the Markdown site table and the machine facts; site sizes and storage totals
  come from the internal filesystem, including the OS and server, never the cartridge.

## Commands

| Command | What it does |
|---|---|
| `cartridge web [--root DIR] [--workers N]` | serve a directory; `--workers 0` (default) is one per CPU thread |
| `cartridge import DEVICE` | ingest a stick |
| `cartridge publish` | copy the live release into RAM |
| `cartridge status [--watch]` | machine, stick, staging, sites, URLs |
| `cartridge sites` | live sites and ports as a Markdown table |
| `cartridge console` | site table and machine facts under the art |
| `cartridge boot-art` | draw the boot mosaic |
| `cartridge art build IMAGE --out DIR` | convert a PNG to art |
| `cartridge art verify MAP` | compare the screen with the expectation |
| `cartridge hash FILE...` | SHA-256 of a file |

## Requirements

- **Linux** with systemd and a framebuffer console (tested on Debian 13)
- **Rust 1.85+** to build; the binary itself needs no runtime, no crates
- Optional: `setfont` (from `kbd`) for the console font - see Limitations

## Troubleshooting

- **Error line, nothing changes** - the cartridge was refused; the reason is in
  `/var/log/cartridge/import.log` (bad `config.json`, no site folders).
- **No login on the TV** - the art owns tty1; press `Ctrl+Alt+F2`, and `Ctrl+Alt+F1`
  to go back.
- **Art looks wrong** - `cartridge art verify /var/lib/cartridge/art/map.json` compares
  the framebuffer with `boot.png` pixel by pixel.
- **Web not answering** - `systemctl status cartridge-web`; the pid lives in
  `/run/cartridge-web.pid`, and a `SIGHUP` makes it re-read the port set.

## Limitations

Honest list, all measured rather than assumed:

- **The console font is still loaded by `setfont`.** The kernel refuses the
  `KDFONTOP` unicode-table ioctls here (`EINVAL`), and glyphs without the table map
  the half blocks to the wrong cells, so the code falls back to `kbd`.
- **The Wi-Fi link, not the server, limits a browser on the network.** Loopback
  reaches 55k rps on two cores; over the radio the same box answers ~1.6k rps at a
  4.5 ms p50, which is the access point's latency, not the server's. The only sysctls
  touched are the four in `systemd/99-cartridge-net.conf` (fast open, listen backlog
  and TIME_WAIT reuse), each of which was measured here.

## License

This is free and unencumbered software released into the public domain - see
[LICENSE](LICENSE).
