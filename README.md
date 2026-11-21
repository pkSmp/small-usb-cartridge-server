# Small USB Cartridge Server

**One Rust binary that turns a small PC into a cartridge console.** Plug a USB stick
into the machine and the sites on it appear on your LAN; the server reads the stick,
copies the sites into RAM and rebuilds the boot art. Unplug it and the last release
keeps serving. Web server, import, art and status are all one
1.0 MB executable with **zero dependencies** - not even a crate.

> **Built for a fanless Chromebox on a shelf.** This is the whole stack for one
> machine: an HP Chromebox G2 (2-core Celeron 3865U, 12 GB RAM, Debian 13) driving a
> TV as the console. It boots in 6.5 s, the web tier idles at ~3 MB RSS, and it
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
| **Cluster mode** | Several boxes on the same switch act as one: each broadcasts what it serves and how busy it is, copies a new release from a peer within seconds, and takes the load off a node that already has a connection per core - relayed with `splice(2)`, so the bytes never leave the kernel. |
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

## Cluster mode

Several of these boxes on one switch behave as a single server: insert the
cartridge in one and every box serves it, and no box is asked for more than it
can answer at full speed.

```
echo CARTRIDGE_BIND=127.0.0.1 > /etc/default/cartridge   # workers go on loopback
systemctl daemon-reload
systemctl enable --now cartridge-cluster
systemctl restart cartridge-web
```

That is the whole configuration. Nothing has to be listed, paired or named.

- **Discovery.** Each box broadcasts twice a second on UDP 29700: node id, the
  live release, a fingerprint of that release's content, its worker count and
  how many connections it is serving. A peer that goes quiet for four seconds
  drops off the list. Nothing outside the switch is involved.
- **Sync.** Every two seconds, a box with an older release asks the peer with
  the newest one for its manifest over TCP 29701, and pulls only files whose
  SHA-256 it does not already hold; files it does hold are hard-linked, not
  copied. Checked bytes land in `releases/<timestamp>`, which becomes `live`
  and is staged into RAM exactly as an import is. A second sync of the same
  cartridge transfers nothing, so a stick inserted in one box is on all of them
  in about as long as it takes to copy it once.
- **Load.** `cartridge cluster` owns the public ports and hands each connection
  either to a local worker or to a peer. A connection stays local until the box
  holds one connection per core of budget; only then does it go to a peer that
  is at least 20 % quieter, measured in connections per core of budget. That
  budget is the CPU the box may really use, so a container limited to half a
  core advertises half a core however many workers it starts. A request
  forwarded in by another box is always answered locally, so two boxes   can never bounce one request between them. A burst that arrives between two
  beacons is spread over the quieter peers at random, weighted by their budget,
  so it lands in proportion to what each box can serve instead of on whichever
  one happens to be quietest, and if a chosen peer refuses the connection the
  request is served locally instead of being dropped. The relay is `splice(2)` in both
  directions: bytes move from socket to socket through the kernel, costing two
  syscalls per 64 KiB and no copies into user space.

`cartridge status` and the console show the peers, which of them serve the same
release, and how much of the load this box took.

What the relay costs, measured on this box with the balancer in front of two
workers and no peers: **30,060 rps** (p50 0.52 ms at 16 connections, 1.07 ms at
32) against **54,948 rps** straight to the workers (p50 0.58 ms). Two more
sockets per request halve the ceiling; a second box on the switch more than
makes that back, and a page view never notices half a millisecond. A box that
has nobody to share with can be run without the daemon, serving its ports
directly as before.

Tested with four fake nodes on one PC (two workers each, so every node behaves
like a two-core box) and with the real box plus two fake nodes on it. The nodes
find each other, a node that starts empty pulls the live release from the real
server and serves it on its own ports, and load arriving on the real server's
public port is spread: 32 connections went 2 local, 20 to one node and 10 to the
other. Adding nodes on the *same* machine does not raise the total - one 24
thread PC went from 81,000 rps on a single node to 76,000 with four, and the
real box stayed at 12,300 both alone and with two fake nodes, because the relay
is an extra hop that competes for the same cores. The gain comes from separate
boxes, where each node brings its own cores; that is the case the switch is for.

Mixed hardware, all on one PC: four real userlands in chroots (Ubuntu 24.04,
Debian 13, Fedora 43, Arch), one worker per core of budget (1, 2, 3, 4 cores),
and four different release histories - two old releases, one empty store, and
the newest release on the Arch node only. All four found each other (three
peers each) and converged on the newest release in one sync round. Straight to
their workers they measured 84,000 / 141,000 / 119,000 / 135,000 rps. Through
the slowest node's public port, 16 connections split 1 / 3 / 5 / 7 - in
proportion to the 1 / 2 / 3 / 4 cores of budget - and 160,000 requests went
through with no errors at p50 0.38 ms.

## Where things live

```
src/                     the whole server, module per job
  web.rs                 epoll + SO_REUSEPORT + sendfile file server
  cluster.rs             beacon, release sync, splice load balancer
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
| Boot to multi-user | 36.0 s | **6.5 s** |
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
| `cartridge web [--root DIR] [--workers N] [--bind ADDR]` | serve a directory; `--workers 0` (default) is one per CPU thread, `--bind 127.0.0.1` keeps the ports for the cluster daemon |
| `cartridge cluster [--root DIR] [--name ID] [--offset N]` | find the boxes on the switch, keep one release between them, split the load; `--offset` mirrors `web --offset` for a second copy on one machine |
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

- **Cluster mode trusts the clocks.** Releases are ordered by the timestamp in
  their name, so a box whose clock is badly behind will not adopt a newer
  release; `cartridge status` shows the peers that disagree. Two boxes that both
  hold content published in the same second keep their own copy instead of
  guessing which is right. Run NTP on the boxes; the sync copies sites but not
  `boot.png`, because the art is each machine's own screen.

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
