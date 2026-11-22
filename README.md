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
| **Your logo stays put** | A cartridge supplies the boot picture only when the machine has none; a stick that still carries the demo art cannot take a chosen logo away on every insert. `CARTRIDGE_TAKE_BOOT=1 cartridge import DEV` asks for the cartridge's picture instead. |
| **Cartridge in, sites out** | A udev rule runs `cartridge import` on the new block device. The stick is mounted read-only, checked for `config.json`, and published as a new release. |
| **Served from RAM** | The live release is copied into a tmpfs and served from memory, so a page view never touches the disk. |
| **Folders are ports** | `3000/` becomes `http://host:3000/`, and a root `index.html` becomes port 80. No config needed. |
| **Boot art** | `boot.png` is converted to coloured half-block cells (two source rows per cell). A picture bigger than the console is shrunk to the largest size that still fits, keeping its shape and leaving rows for the report below, so any image size works; 1, 2, 4 and 8 bit PNGs are read. Near-black pixels are left blank (grey and dark grey are kept), near-identical shades are merged, and up to 15 non-black palette colours are used. |
| **No wasted writes** | Files whose name and SHA-256 already match the running release are hard-linked into the new release instead of being written again. Replaying the same stick writes 0 bytes. |
| **Cluster mode** | Several boxes on the same switch act as one: each broadcasts what it serves and how busy it is, copies a new release from a peer within seconds, and takes the load off a node that already has a connection per core - relayed with `splice(2)`, so the bytes never leave the kernel. |
| **The network on one page** | Every box writes `/srv/cartridge/netmap.txt`: plain text, uncompressed, the same bytes on every machine. It lists each computer with its auto-generated name, address, epoch, load and GPU presence, every file the cartridge carried, and which computers hold each chunk. Because it is rendered from the beacon list and the ownership plan, it needs no controller and no shared writable file. |
| **A zip you can open anywhere** | `cartridge fleet zip` writes `network-<epoch>.zip`: the network map, the blueprint and every site file, stored rather than deflated. The solid `.cart` archive stays the small one; the zip is the one a person can hand to someone else. |
| **The smallest backup it can be** | `archive/<epoch>.cart` is one solid deflate stream over every distinct chunk, and its index carries no hashes at all: a restore recomputes them from the bytes and checks the release root. A 300-page site of 142.5 KB becomes a 6.4 KB file; the live release of 13.6 KB becomes 4.0 KB. |
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
   install -m 0644 systemd/*.service systemd/*.path /etc/systemd/system/
   install -m 0644 systemd/90-cartridge.rules /etc/udev/rules.d/
   install -m 0644 systemd/99-cartridge-net.conf /etc/sysctl.d/ && sysctl -p /etc/sysctl.d/99-cartridge-net.conf
   # The cluster runs as this account, never as root, and owns what it syncs.
   useradd --system --no-create-home --shell /usr/sbin/nologin cartridge 2>/dev/null || true
   mkdir -p /srv/cartridge /var/lib/cartridge
   chown -R cartridge:cartridge /srv/cartridge /var/lib/cartridge
   systemctl daemon-reload
   systemctl enable --now cartridge-web cartridge-console cartridge-publish.path
   ```
   `cartridge-publish.path` re-stages the release when the cluster activates a
   new one (the cluster leaves a request flag; staging mounts a tmpfs, which only
   root may do), so a release pulled from a peer is served without anyone asking.
3. **Make a cartridge.** Format a stick as FAT32 or ext4, put a `config.json`
   holding `{}` at the root, add one digits-only folder per port and an optional
   `boot.png`, then plug it in. The new sites appear on screen and in the import
   log.
4. **Check it.**
   ```
   cartridge status
   ```

## The cartridge

```
config.json      required; must contain a JSON object, {} is enough
3000/index.html  -> http://<host>:3000/
8080/            -> http://<host>:8080/
index.html       -> http://<host>/            (port 80)
www/             not a port of its own
boot.png         optional; becomes the TV art when the machine has none
README.txt       optional; ignored by the importer
```

- **`config.json` must be a JSON object.** `{}` is enough. `null`, `[]`, a
  broken file or a file that is not there means the stick is not a cartridge:
  a missing file is left alone, an invalid one is refused. Nothing inside it is
  read yet, so the file itself is the marker.
- **A top-level folder is a port only if its name is all digits and the number
  is 80 or at least 1024.** `8080/` becomes port 8080, and `80/` is the same as
  a root `index.html`. `www/`, `443/` and `99999/` are not ports: they are
  copied with the release but have no URL of their own, so put them inside a
  port folder — `8080/img/` is served as `/img/`.
- **A top-level `index.html` makes a port 80 site** out of the root of the
  stick. Without it there is no port 80 site, and the port folders are served
  exactly the same.
- **Everything else at the root belongs to the port 80 site.** The importer
  skips only `config.json`, `boot.png`, `README.txt`, `readme.txt`,
  `System Volume Information`, `$RECYCLE.BIN` and `.Trashes`; any other stray
  file becomes a URL, so keep the stick clean.
- **The stick is never written.** It is mounted read-only, the sites are copied
  off it, and the server stages them into RAM itself; there are no `ram_mb` or
  `storage_mb` limits to configure.

The folders are the configuration. A stick without `config.json` is left alone,
so an ordinary USB drive is never imported by accident.

A working cartridge with exactly this layout is in `demo/`: three sites, a
boot picture and an empty `config.json`. Its contents are the release asset,
stored at the top level of the zip so the archive extracts straight onto the
root of a stick:

```
cd demo && zip -r -X ../usb-demo-cartridge.zip .
```

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
  either to a local worker or to a peer. Nothing about a fleet of mixed PCs is
  taken on trust: every box times the responses it actually gets -- from its own
  workers and from each peer -- and keeps an EWMA of the time to first byte.
  That one number carries the CPU, the disk, the memory and the link in front of
  a machine, which a worker count cannot: a throttled container with two workers
  and a modern core with two workers stop looking alike after a few requests.
  A connection stays local until the box holds one connection per core of
  budget. Then the choice is the shortest expected wait, `(connections + 1) x
  measured response time`. The budget is still the CPU the box may really use
  (its cgroup quota), so a container limited to half a core advertises half a
  core however many workers it starts, and it is the weight of the URL space a
  box owns. A peer on another release, or with no release yet, is never offered
  work it would answer with stale bytes. A request forwarded in by another box
  is always answered locally, so two boxes can never bounce one request between
  them, and if a chosen peer refuses the connection the request is served
  locally instead of being dropped.
- **Placement.** The balancer reads the request line before it commits a
  connection, so it knows which file is wanted. Every URL has one owner, chosen
  by weighted rendezvous hashing over the fleet: the boxes agree on the owner
  without asking each other, bigger/faster boxes own proportionally more of the
  URL space, and a box joining or leaving moves only its own share. A file then
  lives hot in one machine's page cache instead of every box in the rack holding
  a copy of every site. Ownership gives way to load -- an owner more than twice
  as slow as the best machine at that moment hands the URL back -- and a
  connection that only pre-connects, sending no request line within 40 ms, is
  placed by load instead. The relay is `splice(2)` in both directions after the
  first couple of kilobytes: only the request head is ever copied into user
  space.

`cartridge status` and the console show the peers, which of them serve the same
release, how long each takes to answer, and how much of the load this box took.

Measured on the 2-core box with three fake boxes on one switch, all serving the
same release, with one, a half and a quarter of a core and **two workers on each
of the small pair** (so worker count cannot explain the result): 64 connections
over 64 different URLs split **64 % / 31 % / 5 %**, against 57 / 29 / 14 by
budget and 65 / 22 / 12 by the raw throughput their own workers measured
(41.5k / 13.9k / 7.4k rps). Ten connections for a single URL left 8 or 10 of
them on that URL's one owner, and the eight URLs tested had owners on every node
in the fleet, not just the one the traffic entered. 12,800 requests, no errors.

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

## The network on one page

Every node keeps a plain-text map of the whole fleet at
`/srv/cartridge/netmap.txt`, and rewrites it when anything changes:

```
CARTNET 1
epoch 7
root 294455ec0805d1d2...
node 2593e760e5b35472ea3d184233f5857f uowpi-nickel-fox 192.168.1.173 7 7 0 4 1 11861 5390 5 13933 self
node 8a1f...                        b2x9m-slate-otter 192.168.1.174 7 7 3 3 0 8000 40000 3 8112 peer
file 4710 text/html;_charset=utf-8 3000/index.html
file 147 application/json 8080/data.json
own 75f218d31266ee36...  5366 2593e760...,8a1f...
```

- `node` — one line per computer: id, the auto-generated name (`k7f3q-amber-fox`),
  address, epoch it is serving, epoch it has ready, connections in flight,
  placement weight, GPU presence, RAM, free disk, the chunks it is responsible
  for, and whether that line is this machine.
- `file` — every file the release carries, with its length and content type.
- `own` — every chunk, and the ids of the computers that should hold it (two by
  default). This is what makes the map a storage map and not only a list.

It is rendered from inputs every box already has (the beacons, the blueprint and
`blueprint::assign`), so all boxes produce byte-identical text — `cmp` between two
machines is a valid check that they agree — and no file has to be shared or
written by a controller. A box that was off for a week catches up by rendering it
again. `cartridge fleet map` prints the summary and the path; on a single machine
with no daemon running it writes the one-box version first.

### The archive, and why it is this small

`archive/<epoch>.cart` is the last line of defence: one file that any survivor
can read, holding every distinct chunk once, concatenated and deflated as a
single solid stream so a page repeated across files is paid for once.

What is inside is chosen for size:

- **No hashes in the index.** A text blueprint spends 64 characters of hex on
every chunk, and hex is random, so it never compresses — on a 300-file site it
was 93% of the archive. The index stores chunk lengths and the file tree; a
restore recomputes every hash from the bytes it just inflated.
- **The root instead.** Those recomputed hashes rebuild the release root, which
the header carries, so the whole tree is verified against 64 characters of hex.
A damaged archive is refused whole rather than half-restored.
- **The index and the network map are deflated too**, because paths and content
types repeat.

Measured on this box: 300 generated pages (142.5 KB) went from a 64,038-byte file
to **6,472 bytes**; the live release (13.6 KB, 5 chunks) went from 5,398 bytes to
**4,011 bytes** and restored file for file. The remaining bytes are the deflated
pages (4.1 KB of the 6.4 KB) — there is no index left to squeeze.

`CARTARC 1` (text blueprint, per-chunk hex) and `CARTARC 2` (binary index with
hashes) are still read, so an archive written by an older build restores here —
and they are held to the same rules: a file that names a chunk the archive does
not carry, or a tree that no longer hashes to its root, is refused rather than
restored with the wrong bytes.

### The zip backup

`cartridge fleet zip [FILE]` writes `archive/network-<epoch>.zip` by default:
`netmap.txt`, `blueprint`, and every site file, **stored** (no deflate) so any
unzip opens it and nothing has to be decoded. The `.cart` archive remains the
compressed, solid copy for space; the zip is the one to hand to a person or to
carry off the box.

### Boot art across the fleet

A release that carries `boot.png` sets the picture on every box that adopts it:
after activation each node compares it with `/var/lib/cartridge/boot.png`, copies
it when it differs, and rebuilds and redraws the art. Insert the stick in one
machine and the logo follows the sites to the others. The file also lives in the
served tree as `/boot.png`.

## What is trusted

Every input is hostile until it is checked, and the limits live in one place
(`guard.rs`) so the importer, the archive and the wire cannot disagree about
them.

- **Request framing.** Only `GET` and `HEAD`, and no request body. A
  `Transfer-Encoding` header, or a `Content-Length` other than zero, is a 400
  and the connection closes: a body left in the buffer would be read as the next
  request, which is the request-smuggling shape.
- **Paths.** Decoded once, then checked: rooted, no control bytes (a decoded
  `%0d%0a` inside a `Location` header is response splitting), no dot-prefixed
  component. `..` never reaches the filesystem, in any encoding.
- **Ranges.** A range the file cannot satisfy is a 416, not the whole file.
- **Privileges.** The web tier binds its ports, drops to `nobody`, and refuses to
  run if the drop fails rather than serving as root.
- **The fleet key.** `/etc/cartridge/fleet.key` (created by `cartridge fleet
  key`, mode 0600) makes the fleet closed: a peer must sign its release manifest
  with HMAC-SHA256 over the exact bytes it sends, its beacon must carry a tag,
  and the control protocol (blueprint, chunks) already answers only after a
  nonce challenge. Unsigned manifests and beacons are ignored, so nothing else
  on the switch can hand a box a release. With no key the fleet stays open, for
  a single box or a lab.
- **Everything off the wire is bounded.** Manifest counts, file sizes, totals
  and every protocol line have the same caps as a stick, and the archive caps
  decompression at the length its header declares.

Known limits, stated rather than hidden: with no fleet key the fleet is open by
design, so set one before a box sits on a shared network; the DNS responder
answers any name with the fleet's addresses, to its own network only, and
belongs on a LAN rather than the internet; and a release tree may contain
symlinks (the RAM staging uses them), which the server follows only inside the
release and the served root, so a release directory must not be writable by
anyone else.

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
  guessing which is right. Run NTP on the boxes. The sync copies a release's
  `boot.png` as well, so a release with a picture sets the art on every node that
  adopts it; a node whose release has none keeps its own screen.

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
