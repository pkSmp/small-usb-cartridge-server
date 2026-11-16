# Small USB Cartridge Server

**One Rust binary that turns a small PC into a cartridge console.** Plug a USB stick
into the machine and the sites on it appear on your LAN; the server reads the stick,
copies the sites into RAM, rebuilds the boot art and plays a chime. Unplug it and the
last release keeps serving. Web server, import, art, sound and status are all one
0.9 MB executable with **zero dependencies** - not even a crate.

> **Built for a fanless Chromebox on a shelf.** This is the whole stack for one
> machine: an HP Chromebox G2 (2-core Celeron 3865U, 12 GB RAM, Debian 13) driving a
> TV as the console. It boots in 12.8 s, the web tier idles at ~3 MB RSS, and it
> serves **18k requests/s on loopback** while doing it. No nginx, no Python, no shell
> scripts at runtime.

![Platform](https://img.shields.io/badge/platform-Linux-lightgrey) ![Runtime](https://img.shields.io/badge/runtime-Rust%20std%20only-orange) ![Binary](https://img.shields.io/badge/binary-0.9%20MB-black) ![Throughput](https://img.shields.io/badge/throughput-18k%20rps-blue)

---
## What it does

| | |
|---|---|
| **Cartridge in, sites out** | A udev rule runs `cartridge import` on the new block device. The stick is mounted read-only, validated against `config.json`, and published as a new release. |
| **Served from RAM** | The live release is copied into a tmpfs and served from memory, so a page view never touches the disk. |
| **Folders are ports** | `3000/` becomes `http://host:3000/`, and a root `index.html` becomes port 80. No config needed. |
| **Boot art** | `boot.png` is converted to coloured half-block cells (50x32) and painted on the framebuffer console at every boot. |
| **Two chimes** | Synthesised in the binary and written straight to ALSA: a rising bell for success, two falling notes for a refusal. |
| **No wasted writes** | Files whose name and SHA-256 already match the running release are hard-linked into the new release instead of being written again. Replaying the same stick writes 0 bytes. |
| **Live status screen** | Under the art, in colours taken from the artwork: attached USB, sites, test URLs and the `Ctrl+Alt+F2` login hint. |
| **Refuses what will not fit** | `storage_mb` and `ram_mb` are checked before anything is published; a too-big cartridge plays the error chime and leaves the live release untouched. |

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
   systemctl daemon-reload && systemctl enable --now cartridge-web cartridge-console
   ```
3. **Make a cartridge.** Format a stick as FAT32 or ext4, add a `config.json` and one
   folder per port, then plug it in. The completion chime means it is live.
4. **Check it.**
   ```
   cartridge status
   ```

## The cartridge

```
config.json          { "storage_mb": 2048, "ram_mb": 512,
                       "sounds": { "complete": "complete.wav", "failed": "failed.wav" } }
3000/index.html      -> http://<host>:3000/
8080/                -> http://<host>:8080/
index.html           -> http://<host>/          (port 80)
boot.png             optional; becomes the TV art
*.wav                optional chimes, 16-bit PCM
README.txt           optional, ignored by the importer
```

`ram_mb` is the hard limit: the whole cartridge has to fit in memory, because that is
where it is served from. `storage_mb` caps the on-disk release. Both are checked
before publishing.

## Where things live

```
src/                     the whole server, module per job
  web.rs                 epoll + SO_REUSEPORT + sendfile file server
  import.rs              mount, validate, publish, prune, chime
  publish.rs             copy the live release into RAM
  art.rs  bootart.rs     PNG -> half-block art, framebuffer draw + verify
  console.rs             the status lines under the art
  sound.rs               chime synthesis + ALSA PCM writer
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
| Loopback rps, 1 connection | 516 | **12,074** |
| Loopback rps, 100 connections | 10,943 (36 errors) | **17,559** (0) |
| Loopback rps, 1000 connections | 9,258 (578 errors) | **18,278** (0) |
| Loopback rps, 5000 connections | 8,473 (3,293 errors) | **17,365** (0) |
| p99 at 100 connections | 227 ms | **10.7 ms** |
| 50 MB download, loopback | 194 MB/s | **1,665 MB/s** |

Over the box's Wi-Fi link the numbers are 7.8k-10k rps, and the 50 MB transfer is
link-bound at ~16-20 MB/s. Reproduce with `bench/`:
```
loadgen bench HOST PORT PATH CONNS REQUESTS
loadgen download HOST PORT PATH [CONNS]
```

## How it works

- **Web tier.** Two worker processes share the port with `SO_REUSEPORT`; each runs an
  edge-triggered epoll loop, accepts in batches with `accept4`, parses a capped header
  set, and answers with precomputed headers and `sendfile` from the RAM copy. Bodies
  are only allocated for error pages. It drops to `nobody` after binding.
- **Import.** Mounts read-only through the mount syscall, checks the limits, copies
  into `.staging`, then renames into `releases/<timestamp>` and flips a symlink. Old
  releases are pruned. Nothing is written to the stick.
- **Art.** `boot.png` is decoded by the built-in PNG reader, quantised to the console
  palette, written as half-block cells, and verified by reading the framebuffer back
  and comparing every sampled pixel with the expectation.
- **Sound.** Chimes are synthesised as WAV at import time and handed to the HDMI PCM
  device through `SNDRV_PCM_IOCTL_HW_PARAMS` + `write` + `DRAIN`; `aplay` exists only
  as a fallback.
- **Status.** Reads `/proc`, the release directory and the import log; the colours are
  matched to the boot art's palette by hue, so the report belongs to the picture.

## Commands

| Command | What it does |
|---|---|
| `cartridge web [--root DIR] [--workers N]` | serve a directory |
| `cartridge import DEVICE` | ingest a stick |
| `cartridge publish` | copy the live release into RAM |
| `cartridge status [--watch]` | card, stick, sites, URLs, last import |
| `cartridge console` | the two status lines under the art |
| `cartridge boot-art` | draw the boot mosaic |
| `cartridge art build IMAGE --out DIR` | convert a PNG to art |
| `cartridge art verify MAP` | compare the screen with the expectation |
| `cartridge sound install` / `play KIND` | write or play the chimes |
| `cartridge hash FILE...` | SHA-256 of a file |

## Requirements

- **Linux** with systemd, a framebuffer console and ALSA (tested on Debian 13)
- **Rust 1.85+** to build; the binary itself needs no runtime, no crates
- Optional: `setfont` (from `kbd`) for the console font - see Limitations

## Troubleshooting

- **Error chime, nothing changes** - the cartridge was refused; the reason is in
  `/var/log/cartridge/import.log` (too big, bad `config.json`, no site folders).
- **No chime** - check `cartridge status` shows a card and an output; the log says
  which chime was played and how.
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
- **Chimes must be WAV.** There is no MP3 decoder; a cartridge MP3 is skipped with a
  log line and the built-in chime plays instead.
- **No kernel/network sysctl tuning.** Loopback scales to 18k rps on two cores and
  the Wi-Fi link is the bottleneck, so no unmeasured knobs were added.

## License

This is free and unencumbered software released into the public domain - see
[LICENSE](LICENSE).
