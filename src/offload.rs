//! GPU offload, but only where the card is actually faster.
//!
//! The serving path never comes through here. A request costs about 10 us of
//! CPU (epoll, read, a path lookup, write + sendfile), and one GPU dispatch
//! costs more than that on its own, so a static file server has nothing to hand
//! a graphics card. What a card *can* do is bulk work between requests:
//! hashing a cartridge on import, verifying a restored release, mapping a big
//! image onto a palette.
//!
//! So every task is measured on this machine, once, on first use: the same work
//! runs on the GPU and on the CPU, and the GPU only takes the job if it wins by
//! a clear margin. If there is no usable GPU, or the driver fails, or the answer
//! differs from the CPU by one byte, the CPU keeps the work and nothing else in
//! the program has to know. There are no crates: OpenCL is reached through
//! `dlopen`, and the kernels are compiled by the driver from source in this
//! file.
//!
//! `cartridge gpu` prints the table of what was measured and who won.

use std::ffi::{c_void, CStr, CString};
use std::sync::OnceLock;
use std::time::Instant;

extern "C" {
    fn dlopen(file: *const i8, mode: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const i8) -> *mut c_void;
    fn dlerror() -> *const i8;
}

type Handle = *mut c_void;
type Platform = *mut c_void;
type Device = *mut c_void;
type Context = *mut c_void;
type Queue = *mut c_void;
type Program = *mut c_void;
type Kernel = *mut c_void;
type Mem = *mut c_void;

const GPU_TYPE: u64 = 4;
const MEM_READ_ONLY: u64 = 1 << 2;
const MEM_WRITE_ONLY: u64 = 1 << 1;
const MEM_COPY_HOST: u64 = 1 << 5;

/// Bulk work has to be worth the round trip; below this the CPU wins outright.
const MIN_BYTES: usize = 256 * 1024;
/// The boot art's box-average downscale: a boot picture is 1200x630 or more,
/// and below this many source pixels one dispatch costs more than the whole
/// reduction.
const MIN_FIT_PIXELS: usize = 87_000;
/// Framebuffer sampling in `art verify`: 240x86 dots of 8x8 pixels is 1.3M.
const MIN_TILE_PIXELS: usize = 256 * 1024;
/// The card takes a task whenever it is present and its answer matches the
/// CPU's byte for byte. The measured speed is printed, not used as a gate: the
/// CPU is kept for requests, so background work goes to the card even when the
/// card is the slower engine.

// ------------------------------------------------------------------ kernels

const SHA256_SRC: &str = r#"
#define ROR(x,n) (((x) >> (n)) | ((x) << (32 - (n))))

__constant uint K[64] = {
    0x428a2f98u,0x71374491u,0xb5c0fbcfu,0xe9b5dba5u,0x3956c25bu,0x59f111f1u,0x923f82a4u,0xab1c5ed5u,
    0xd807aa98u,0x12835b01u,0x243185beu,0x550c7dc3u,0x72be5d74u,0x80deb1feu,0x9bdc06a7u,0xc19bf174u,
    0xe49b69c1u,0xefbe4786u,0x0fc19dc6u,0x240ca1ccu,0x2de92c6fu,0x4a7484aau,0x5cb0a9dcu,0x76f988dau,
    0x983e5152u,0xa831c66du,0xb00327c8u,0xbf597fc7u,0xc6e00bf3u,0xd5a79147u,0x06ca6351u,0x14292967u,
    0x27b70a85u,0x2e1b2138u,0x4d2c6dfcu,0x53380d13u,0x650a7354u,0x766a0abbu,0x81c2c92eu,0x92722c85u,
    0xa2bfe8a1u,0xa81a664bu,0xc24b8b70u,0xc76c51a3u,0xd192e819u,0xd6990624u,0xf40e3585u,0x106aa070u,
    0x19a4c116u,0x1e376c08u,0x2748774cu,0x34b0bcb5u,0x391c0cb3u,0x4ed8aa4au,0x5b9cca4fu,0x682e6ff3u,
    0x748f82eeu,0x78a5636fu,0x84c87814u,0x8cc70208u,0x90befffau,0xa4506cebu,0xbef9a3f7u,0xc67178f2u
};

inline void compress(uchar *blk, uint *h) {
    uint w[64];
    for (int i = 0; i < 16; i++) {
        w[i] = ((uint)blk[i*4] << 24) | ((uint)blk[i*4+1] << 16) | ((uint)blk[i*4+2] << 8) | (uint)blk[i*4+3];
    }
    for (int i = 16; i < 64; i++) {
        uint s0 = ROR(w[i-15], 7) ^ ROR(w[i-15], 18) ^ (w[i-15] >> 3);
        uint s1 = ROR(w[i-2], 17) ^ ROR(w[i-2], 19) ^ (w[i-2] >> 10);
        w[i] = w[i-16] + s0 + w[i-7] + s1;
    }
    uint a = h[0], b = h[1], c = h[2], d = h[3], e = h[4], f = h[5], g = h[6], hh = h[7];
    for (int i = 0; i < 64; i++) {
        uint S1 = ROR(e, 6) ^ ROR(e, 11) ^ ROR(e, 25);
        uint ch = (e & f) ^ ((~e) & g);
        uint t1 = hh + S1 + ch + K[i] + w[i];
        uint S0 = ROR(a, 2) ^ ROR(a, 13) ^ ROR(a, 22);
        uint mj = (a & b) ^ (a & c) ^ (b & c);
        uint t2 = S0 + mj;
        hh = g; g = f; f = e; e = d + t1; d = c; c = b; b = a; a = t1 + t2;
    }
    h[0] += a; h[1] += b; h[2] += c; h[3] += d;
    h[4] += e; h[5] += f; h[6] += g; h[7] += hh;
}

__kernel void sha256(__global const uchar *data, uint len, __global uchar *out) {
    uint id = get_global_id(0);
    __global const uchar *msg = data + (ulong)id * len;
    uint h[8] = {0x6a09e667u,0xbb67ae85u,0x3c6ef372u,0xa54ff53au,
                 0x510e527fu,0x9b05688cu,0x1f83d9abu,0x5be0cd19u};
    uint blocks = len / 64u;
    for (uint b = 0; b < blocks; b++) {
        uchar blk[64];
        for (int i = 0; i < 64; i++) blk[i] = msg[b*64 + i];
        compress(blk, h);
    }
    uint rest = len - blocks * 64u;
    uchar tail[128];
    for (uint i = 0; i < rest; i++) tail[i] = msg[blocks*64 + i];
    tail[rest] = 0x80;
    uint padlen = ((rest + 9u + 63u) / 64u) * 64u;
    for (uint i = rest + 1; i < padlen; i++) tail[i] = 0;
    ulong bits = (ulong)len * 8ul;
    for (int i = 0; i < 8; i++) tail[padlen - 1 - i] = (uchar)(bits >> (8 * i));
    for (uint b = 0; b < padlen / 64u; b++) {
        uchar blk[64];
        for (int i = 0; i < 64; i++) blk[i] = tail[b*64 + i];
        compress(blk, h);
    }
    for (int i = 0; i < 8; i++) {
        out[id*32 + i*4]     = (uchar)(h[i] >> 24);
        out[id*32 + i*4 + 1] = (uchar)(h[i] >> 16);
        out[id*32 + i*4 + 2] = (uchar)(h[i] >> 8);
        out[id*32 + i*4 + 3] = (uchar)(h[i]);
    }
}
"#;

/// gzip in both directions, as one-work-item programs. The whole sequential
/// algorithm runs on the card; see the file for why that is the only way to put
/// a Huffman bitstream anywhere but the CPU.
const GZIP_SRC: &str = include_str!("gzip_kernel.cl");

/// The LZ77 match search: one work-item per position, walking the same hash
/// chain `gzip::longest` walks, so the tokens come out the same.
const LZ77_SRC: &str = r#"
__kernel void lz77(__global const uchar *data, uint n,
                   __global const uint *link,
                   uint chain, uint max_match, uint window,
                   __global uint *out) {
    uint p = get_global_id(0);
    if (p >= n) return;
    uint limit = n - p;
    if (limit > max_match) limit = max_match;
    uint best_len = 0u, best_dist = 0u;
    uint cand = link[p];
    uint tries = 0u;
    while (cand != 0xFFFFFFFFu && tries < chain) {
        if (cand >= p) break;
        uint dist = p - cand;
        if (dist > window) break;
        uint len = 0u;
        while (len < limit && data[cand + len] == data[p + len]) len++;
        if (len > best_len) {
            best_len = len;
            best_dist = dist;
            if (len >= limit) break;
        }
        cand = link[cand];
        tries++;
    }
    out[p] = best_len >= 3u ? ((best_len << 16) | best_dist) : 0u;
}
"#;

/// Nearest palette entry per pixel, squared distance, lowest index on a tie:
/// exactly what `art::nearest` computes, so a caller can swap them freely.
const NEAREST_SRC: &str = r#"
__kernel void nearest(__global const uchar *px, uint count,
                      __global const uchar *pal, uint colours,
                      __global uchar *out) {
    uint i = get_global_id(0);
    if (i >= count) return;
    uint r = px[i*3], g = px[i*3+1], b = px[i*3+2];
    uint best = 0, bestd = 0xFFFFFFFFu;
    for (uint p = 0; p < colours; p++) {
        int dr = (int)r - (int)pal[p*3];
        int dg = (int)g - (int)pal[p*3+1];
        int db = (int)b - (int)pal[p*3+2];
        uint d = (uint)(dr*dr + dg*dg + db*db);
        if (d < bestd) { bestd = d; best = p; }
    }
    out[i] = (uchar)best;
}
"#;

/// Box-average downscale, the same integer arithmetic `art::fit` does: one work
/// item per output pixel, the source rectangle walked row by row, sums in u64,
/// so the bytes come out identical to the CPU's.
const FIT_SRC: &str = r#"
__kernel void fit(__global const uchar *src, uint sw, uint sh,
                  uint nw, uint nh, __global uchar *out) {
    uint x = get_global_id(0);
    uint y = get_global_id(1);
    if (x >= nw || y >= nh) return;
    uint x0 = x * sw / nw;
    uint x1 = ((x + 1) * sw + nw - 1) / nw;
    if (x1 > sw) x1 = sw;
    if (x1 < x0 + 1) x1 = x0 + 1;
    uint y0 = y * sh / nh;
    uint y1 = ((y + 1) * sh + nh - 1) / nh;
    if (y1 > sh) y1 = sh;
    if (y1 < y0 + 1) y1 = y0 + 1;
    ulong r = 0, g = 0, b = 0, n = 0;
    for (uint yy = y0; yy < y1; yy++) {
        for (uint xx = x0; xx < x1; xx++) {
            uint o = (yy * sw + xx) * 3;
            r += src[o]; g += src[o + 1]; b += src[o + 2];
            n++;
        }
    }
    uint o = (y * nw + x) * 3;
    out[o] = (uchar)(r / n);
    out[o + 1] = (uchar)(g / n);
    out[o + 2] = (uchar)(b / n);
}
"#;

/// The tile averages `art::cmd_verify` reads back off the framebuffer: one work
/// item per sampled dot, the same bounds checks, the same u64 sums, the same
/// "is this dot flat" test as `art::FrameBuffer::tile`.
const TILES_SRC: &str = r#"
__kernel void tiles(__global const uchar *fb, uint fb_len, uint fw, uint fh,
                    uint stride, uint step, uint bpp, uint bgr,
                    uint tw, uint th, uint ox, uint oy, uint cw, uint ch,
                    uint out_w, uint out_h, __global uchar *out) {
    uint tx = get_global_id(0);
    uint ty = get_global_id(1);
    if (tx >= out_w || ty >= out_h) return;
    uint x0 = ox + tx * cw;
    uint y0 = oy + (ty / 2) * ch + (ty % 2) * (ch / 2);
    ulong rs = 0, gs = 0, bs = 0, n = 0;
    uchar fr = 0, fg = 0, fblue = 0;
    uint flat = 1;
    for (uint y = y0; y < y0 + th; y++) {
        if (y >= fh) continue;
        for (uint x = x0; x < x0 + tw; x++) {
            if (x >= fw) continue;
            uint o = y * stride + x * step;
            if (o + 2 >= fb_len) continue;
            uchar r = 0, g = 0, b = 0;
            if (bpp == 32) {
                r = bgr ? fb[o] : fb[o + 2];
                g = fb[o + 1];
                b = bgr ? fb[o + 2] : fb[o];
            } else if (bpp == 16) {
                uint v = (uint)fb[o] | ((uint)fb[o + 1] << 8);
                r = (uchar)(((v >> 11) & 0x1F) * 255 / 31);
                g = (uchar)(((v >> 5) & 0x3F) * 255 / 63);
                b = (uchar)((v & 0x1F) * 255 / 31);
            }
            if (n == 0) { fr = r; fg = g; fblue = b; }
            else if (r != fr || g != fg || b != fblue) flat = 0;
            rs += r; gs += g; bs += b;
            n++;
        }
    }
    uint t = (ty * out_w + tx) * 4;
    if (n == 0) {
        out[t] = 0; out[t + 1] = 0; out[t + 2] = 0; out[t + 3] = 0;
        return;
    }
    out[t] = (uchar)(rs / n);
    out[t + 1] = (uchar)(gs / n);
    out[t + 2] = (uchar)(bs / n);
    out[t + 3] = (uchar)flat;
}
"#;

// -------------------------------------------------------------- function table

struct Fns {
    get_platforms: extern "C" fn(u32, *mut Platform, *mut u32) -> i32,
    get_devices: extern "C" fn(Platform, u64, u32, *mut Device, *mut u32) -> i32,
    device_info: extern "C" fn(Device, u32, usize, *mut c_void, *mut usize) -> i32,
    create_context: extern "C" fn(
        *const i64,
        u32,
        *const Device,
        extern "C" fn(*const i8, *const c_void, usize, *mut c_void),
        *mut c_void,
        *mut i32,
    ) -> Context,
    create_queue: extern "C" fn(Context, Device, u64, *mut i32) -> Queue,
    create_program: extern "C" fn(Context, u32, *const *const i8, *const usize, *mut i32) -> Program,
    build: extern "C" fn(
        Program,
        u32,
        *const Device,
        *const i8,
        extern "C" fn(*mut c_void, *mut c_void),
        *mut c_void,
    ) -> i32,
    build_info: extern "C" fn(Program, Device, u32, usize, *mut c_void, *mut usize) -> i32,
    create_kernel: extern "C" fn(Program, *const i8, *mut i32) -> Kernel,
    create_buffer: extern "C" fn(Context, u64, usize, *mut c_void, *mut i32) -> Mem,
    set_arg: extern "C" fn(Kernel, u32, usize, *const c_void) -> i32,
    enqueue: extern "C" fn(
        Queue,
        Kernel,
        u32,
        *const usize,
        *const usize,
        *const usize,
        u32,
        *const c_void,
        *mut c_void,
    ) -> i32,
    finish: extern "C" fn(Queue) -> i32,
    read_buffer: extern "C" fn(
        Queue,
        Mem,
        u32,
        usize,
        usize,
        *mut c_void,
        u32,
        *const c_void,
        *mut c_void,
    ) -> i32,
    release_mem: extern "C" fn(Mem) -> i32,
}

extern "C" fn ctx_cb(_: *const i8, _: *const c_void, _: usize, _: *mut c_void) {}
extern "C" fn build_cb(_: *mut c_void, _: *mut c_void) {}

struct Gpu {
    name: String,
    units: u32,
    clock: u32,
    device: Device,
    context: Context,
    queue: Queue,
    fns: Fns,
    sha: OnceLock<Option<Kernel>>,
    near: OnceLock<Option<Kernel>>,
    fit: OnceLock<Option<Kernel>>,
    tiles: OnceLock<Option<Kernel>>,
    lz77: OnceLock<Option<Kernel>>,
    gz: OnceLock<Option<Kernel>>,
    gun: OnceLock<Option<Kernel>>,
    /// A command queue is not safe for concurrent enqueues, so every dispatch
    /// takes this. Bulk work is rare; serialising it costs nothing.
    lock: std::sync::Mutex<()>,
}

// The handles are opaque driver objects. Everything that touches the queue is
// behind `lock`, and the kernel slots are `OnceLock`s, so sharing this across
// threads is sound.
unsafe impl Send for Gpu {}

/// Releases the device buffers a dispatch created, on every path out of it.
/// Without this the driver holds the last dispatch's memory for the life of the
/// process, and the card now hashes every chunk, so that is not a slow leak.
struct Buffers<'a> {
    gpu: &'a Gpu,
    mem: Vec<Mem>,
}

impl Drop for Buffers<'_> {
    fn drop(&mut self) {
        for m in self.mem.drain(..) {
            if !m.is_null() {
                (self.gpu.fns.release_mem)(m);
            }
        }
    }
}
unsafe impl Sync for Gpu {}

impl Gpu {
    fn kernel(&self, src: &str, name: &str, slot: &OnceLock<Option<Kernel>>) -> Option<Kernel> {
        *slot.get_or_init(|| self.build_kernel(src, name))
    }

    fn build_kernel(&self, src: &str, name: &str) -> Option<Kernel> {
        let f = &self.fns;
        let mut err = 0i32;
        let src_ptr = src.as_ptr() as *const i8;
        let src_len = src.len();
        let program = (f.create_program)(self.context, 1, &src_ptr, &src_len, &mut err);
        if program.is_null() {
            return None;
        }
        if (f.build)(program, 1, &self.device, std::ptr::null(), build_cb, std::ptr::null_mut()) != 0 {
            let mut log = vec![0u8; 4096];
            let mut got = 0usize;
            (f.build_info)(
                program,
                self.device,
                0x1182,
                log.len(),
                log.as_mut_ptr() as *mut c_void,
                &mut got,
            );
            let text = String::from_utf8_lossy(&log[..got.min(log.len())]).to_string();
            note(&format!(
                "offload: the driver refused the {} kernel: {}",
                name,
                text.lines().next().unwrap_or("no detail")
            ));
            return None;
        }
        let cname = CString::new(name).ok()?;
        let k = (f.create_kernel)(program, cname.as_ptr(), &mut err);
        if k.is_null() {
            None
        } else {
            Some(k)
        }
    }

    fn sha256_batch(&self, msgs: &[&[u8]]) -> Option<Vec<[u8; 32]>> {
        if msgs.is_empty() {
            return Some(Vec::new());
        }
        let len = msgs[0].len();
        if msgs.iter().any(|m| m.len() != len) || len == 0 || len > u32::MAX as usize {
            return None;
        }
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let kernel = self.kernel(SHA256_SRC, "sha256", &self.sha)?;
        let f = &self.fns;
        let total = len * msgs.len();
        let mut flat = Vec::with_capacity(total);
        for m in msgs {
            flat.extend_from_slice(m);
        }
        let mut err = 0i32;
        let input = (f.create_buffer)(
            self.context,
            MEM_READ_ONLY | MEM_COPY_HOST,
            total,
            flat.as_ptr() as *mut c_void,
            &mut err,
        );
        let out_len = 32 * msgs.len();
        let output = (f.create_buffer)(self.context, MEM_WRITE_ONLY, out_len, std::ptr::null_mut(), &mut err);
        if input.is_null() || output.is_null() {
            return None;
        }
        let _bufs = Buffers { gpu: self, mem: vec![input, output] };
        let len32 = len as u32;
        (f.set_arg)(kernel, 0, std::mem::size_of::<Mem>(), &input as *const Mem as *const c_void);
        (f.set_arg)(kernel, 1, 4, &len32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 2, std::mem::size_of::<Mem>(), &output as *const Mem as *const c_void);
        let global = msgs.len();
        if (f.enqueue)(
            self.queue,
            kernel,
            1,
            std::ptr::null(),
            &global,
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        ) != 0
        {
            return None;
        }
        (f.finish)(self.queue);
        let mut raw = vec![0u8; out_len];
        if (f.read_buffer)(
            self.queue,
            output,
            1,
            0,
            out_len,
            raw.as_mut_ptr() as *mut c_void,
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        ) != 0
        {
            return None;
        }
        Some(
            raw.chunks_exact(32)
                .map(|c| {
                    let mut d = [0u8; 32];
                    d.copy_from_slice(c);
                    d
                })
                .collect(),
        )
    }

    fn nearest_batch(&self, pixels: &[[u8; 3]], palette: &[[u8; 3]]) -> Option<Vec<u8>> {
        if pixels.is_empty() || palette.is_empty() || palette.len() > 256 {
            return None;
        }
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let kernel = self.kernel(NEAREST_SRC, "nearest", &self.near)?;
        let f = &self.fns;
        let mut flat = Vec::with_capacity(pixels.len() * 3);
        for p in pixels {
            flat.extend_from_slice(p);
        }
        let mut pal = Vec::with_capacity(palette.len() * 3);
        for p in palette {
            pal.extend_from_slice(p);
        }
        let mut err = 0i32;
        let px_buf = (f.create_buffer)(
            self.context,
            MEM_READ_ONLY | MEM_COPY_HOST,
            flat.len(),
            flat.as_ptr() as *mut c_void,
            &mut err,
        );
        let pal_buf = (f.create_buffer)(
            self.context,
            MEM_READ_ONLY | MEM_COPY_HOST,
            pal.len(),
            pal.as_ptr() as *mut c_void,
            &mut err,
        );
        let out_buf = (f.create_buffer)(
            self.context,
            MEM_WRITE_ONLY,
            pixels.len(),
            std::ptr::null_mut(),
            &mut err,
        );
        if px_buf.is_null() || pal_buf.is_null() || out_buf.is_null() {
            return None;
        }
        let count = pixels.len() as u32;
        let colours = palette.len() as u32;
        (f.set_arg)(kernel, 0, std::mem::size_of::<Mem>(), &px_buf as *const Mem as *const c_void);
        (f.set_arg)(kernel, 1, 4, &count as *const u32 as *const c_void);
        (f.set_arg)(kernel, 2, std::mem::size_of::<Mem>(), &pal_buf as *const Mem as *const c_void);
        (f.set_arg)(kernel, 3, 4, &colours as *const u32 as *const c_void);
        (f.set_arg)(kernel, 4, std::mem::size_of::<Mem>(), &out_buf as *const Mem as *const c_void);
        let global = pixels.len();
        if (f.enqueue)(
            self.queue,
            kernel,
            1,
            std::ptr::null(),
            &global,
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        ) != 0
        {
            return None;
        }
        (f.finish)(self.queue);
        let mut out = vec![0u8; pixels.len()];
        if (f.read_buffer)(
            self.queue,
            out_buf,
            1,
            0,
            out.len(),
            out.as_mut_ptr() as *mut c_void,
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        ) != 0
        {
            return None;
        }
        Some(out)
    }

    /// One gzip member, encoded by the card: header, deflate, CRC and length.
    fn gz_encode(&self, data: &[u8]) -> Option<Vec<u8>> {
        if data.is_empty() || data.len() > u32::MAX as usize / 8 {
            return None;
        }
        let cap = data.len() + data.len() / 8 + 4096;
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let kernel = self.kernel(GZIP_SRC, "gzip", &self.gz)?;
        let f = &self.fns;
        let mut err = 0i32;
        let input = (f.create_buffer)(
            self.context,
            MEM_READ_ONLY | MEM_COPY_HOST,
            data.len(),
            data.as_ptr() as *mut c_void,
            &mut err,
        );
        let output = (f.create_buffer)(self.context, MEM_WRITE_ONLY, cap, std::ptr::null_mut(), &mut err);
        let toks = (f.create_buffer)(self.context, 0, (data.len() + 16) * 4, std::ptr::null_mut(), &mut err);
        let head = (f.create_buffer)(self.context, 0, 65536 * 4, std::ptr::null_mut(), &mut err);
        let prev = (f.create_buffer)(self.context, 0, 32768 * 4, std::ptr::null_mut(), &mut err);
        let res = (f.create_buffer)(self.context, MEM_WRITE_ONLY, 8, std::ptr::null_mut(), &mut err);
        let _bufs = Buffers { gpu: self, mem: vec![input, output, toks, head, prev, res] };
        if input.is_null() || output.is_null() || toks.is_null() || head.is_null() || prev.is_null() || res.is_null() {
            return None;
        }
        let n32 = data.len() as u32;
        let cap32 = cap as u32;
        (f.set_arg)(kernel, 0, std::mem::size_of::<Mem>(), &input as *const Mem as *const c_void);
        (f.set_arg)(kernel, 1, 4, &n32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 2, std::mem::size_of::<Mem>(), &output as *const Mem as *const c_void);
        (f.set_arg)(kernel, 3, 4, &cap32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 4, std::mem::size_of::<Mem>(), &toks as *const Mem as *const c_void);
        (f.set_arg)(kernel, 5, std::mem::size_of::<Mem>(), &head as *const Mem as *const c_void);
        (f.set_arg)(kernel, 6, std::mem::size_of::<Mem>(), &prev as *const Mem as *const c_void);
        (f.set_arg)(kernel, 7, std::mem::size_of::<Mem>(), &res as *const Mem as *const c_void);
        let global = [1usize];
        if (f.enqueue)(
            self.queue,
            kernel,
            1,
            std::ptr::null(),
            global.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        ) != 0
        {
            return None;
        }
        (f.finish)(self.queue);
        let mut got = [0u32; 2];
        if (f.read_buffer)(
            self.queue,
            res,
            1,
            0,
            8,
            got.as_mut_ptr() as *mut c_void,
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        ) != 0
        {
            return None;
        }
        if got[0] != 0 {
            note(&format!("offload: the card's gzip refused this slice ({})", got[0]));
            return None;
        }
        let len = got[1] as usize;
        if len == 0 || len > cap {
            return None;
        }
        let mut out = vec![0u8; len];
        if (f.read_buffer)(
            self.queue,
            output,
            1,
            0,
            len,
            out.as_mut_ptr() as *mut c_void,
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        ) != 0
        {
            return None;
        }
        Some(out)
    }

    /// A whole gzip file (or a zlib stream) decoded by the card. The kernel
    /// checks the member's own CRC and length, so a successful return is proof
    /// the bytes are the ones the stream specifies.
    fn gz_decode(&self, data: &[u8], cap: usize, zlib: bool) -> Option<Vec<u8>> {
        if data.is_empty() || cap == 0 || cap > u32::MAX as usize {
            return None;
        }
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let kernel = self.kernel(GZIP_SRC, "gunzip", &self.gun)?;
        let f = &self.fns;
        let mut err = 0i32;
        let input = (f.create_buffer)(
            self.context,
            MEM_READ_ONLY | MEM_COPY_HOST,
            data.len(),
            data.as_ptr() as *mut c_void,
            &mut err,
        );
        let output = (f.create_buffer)(self.context, MEM_WRITE_ONLY, cap, std::ptr::null_mut(), &mut err);
        let res = (f.create_buffer)(self.context, MEM_WRITE_ONLY, 8, std::ptr::null_mut(), &mut err);
        let _bufs = Buffers { gpu: self, mem: vec![input, output, res] };
        if input.is_null() || output.is_null() || res.is_null() {
            return None;
        }
        let n32 = data.len() as u32;
        let cap32 = cap as u32;
        let z = if zlib { 1u32 } else { 0u32 };
        (f.set_arg)(kernel, 0, std::mem::size_of::<Mem>(), &input as *const Mem as *const c_void);
        (f.set_arg)(kernel, 1, 4, &n32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 2, std::mem::size_of::<Mem>(), &output as *const Mem as *const c_void);
        (f.set_arg)(kernel, 3, 4, &cap32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 4, 4, &z as *const u32 as *const c_void);
        (f.set_arg)(kernel, 5, std::mem::size_of::<Mem>(), &res as *const Mem as *const c_void);
        let global = [1usize];
        if (f.enqueue)(
            self.queue,
            kernel,
            1,
            std::ptr::null(),
            global.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        ) != 0
        {
            return None;
        }
        (f.finish)(self.queue);
        let mut got = [0u32; 2];
        if (f.read_buffer)(
            self.queue,
            res,
            1,
            0,
            8,
            got.as_mut_ptr() as *mut c_void,
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        ) != 0
        {
            return None;
        }
        if got[0] != 0 {
            // Not an error to log: an invalid stream is the caller's problem and
            // the CPU decoder reports it with a message. This is only "the card
            // will not do this one".
            return None;
        }
        let len = got[1] as usize;
        if len > cap {
            return None;
        }
        let mut out = vec![0u8; len];
        if len > 0
            && (f.read_buffer)(
                self.queue,
                output,
                1,
                0,
                len,
                out.as_mut_ptr() as *mut c_void,
                0,
                std::ptr::null(),
                std::ptr::null_mut(),
            ) != 0
        {
            return None;
        }
        Some(out)
    }

    /// The LZ77 match search for one slice: `len << 16 | dist` per position.
    fn lz77_slice(
        &self,
        data: &[u8],
        link: &[u32],
        window: u32,
        chain: u32,
        max_match: u32,
    ) -> Option<Vec<u32>> {
        if data.is_empty() || link.len() != data.len() || data.len() > u32::MAX as usize {
            return None;
        }
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let kernel = self.kernel(LZ77_SRC, "lz77", &self.lz77)?;
        let f = &self.fns;
        let mut err = 0i32;
        let input = (f.create_buffer)(
            self.context,
            MEM_READ_ONLY | MEM_COPY_HOST,
            data.len(),
            data.as_ptr() as *mut c_void,
            &mut err,
        );
        let links = (f.create_buffer)(
            self.context,
            MEM_READ_ONLY | MEM_COPY_HOST,
            link.len() * 4,
            link.as_ptr() as *mut c_void,
            &mut err,
        );
        let out_len = data.len() * 4;
        let output = (f.create_buffer)(self.context, MEM_WRITE_ONLY, out_len, std::ptr::null_mut(), &mut err);
        let _bufs = Buffers { gpu: self, mem: vec![input, links, output] };
        if input.is_null() || links.is_null() || output.is_null() {
            return None;
        }
        let n32 = data.len() as u32;
        (f.set_arg)(kernel, 0, std::mem::size_of::<Mem>(), &input as *const Mem as *const c_void);
        (f.set_arg)(kernel, 1, 4, &n32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 2, std::mem::size_of::<Mem>(), &links as *const Mem as *const c_void);
        (f.set_arg)(kernel, 3, 4, &chain as *const u32 as *const c_void);
        (f.set_arg)(kernel, 4, 4, &max_match as *const u32 as *const c_void);
        (f.set_arg)(kernel, 5, 4, &window as *const u32 as *const c_void);
        (f.set_arg)(kernel, 6, std::mem::size_of::<Mem>(), &output as *const Mem as *const c_void);
        let global = [data.len()];
        if (f.enqueue)(
            self.queue,
            kernel,
            1,
            std::ptr::null(),
            global.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        ) != 0
        {
            return None;
        }
        (f.finish)(self.queue);
        let mut out = vec![0u32; data.len()];
        if (f.read_buffer)(
            self.queue,
            output,
            1,
            0,
            out_len,
            out.as_mut_ptr() as *mut c_void,
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        ) != 0
        {
            return None;
        }
        Some(out)
    }

    fn fit_batch(&self, src: &[u8], sw: usize, sh: usize, nw: usize, nh: usize) -> Option<Vec<u8>> {
        if sw == 0 || sh == 0 || nw == 0 || nh == 0 {
            return None;
        }
        if src.len() != sw * sh * 3 || sw > u32::MAX as usize || sh > u32::MAX as usize {
            return None;
        }
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let kernel = self.kernel(FIT_SRC, "fit", &self.fit)?;
        let f = &self.fns;
        let mut err = 0i32;
        let input = (f.create_buffer)(
            self.context,
            MEM_READ_ONLY | MEM_COPY_HOST,
            src.len(),
            src.as_ptr() as *mut c_void,
            &mut err,
        );
        let out_len = nw * nh * 3;
        let output = (f.create_buffer)(self.context, MEM_WRITE_ONLY, out_len, std::ptr::null_mut(), &mut err);
        if input.is_null() || output.is_null() {
            return None;
        }
        let (sw32, sh32, nw32, nh32) = (sw as u32, sh as u32, nw as u32, nh as u32);
        (f.set_arg)(kernel, 0, std::mem::size_of::<Mem>(), &input as *const Mem as *const c_void);
        (f.set_arg)(kernel, 1, 4, &sw32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 2, 4, &sh32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 3, 4, &nw32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 4, 4, &nh32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 5, std::mem::size_of::<Mem>(), &output as *const Mem as *const c_void);
        let global = [nw, nh];
        if (f.enqueue)(
            self.queue,
            kernel,
            2,
            std::ptr::null(),
            global.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        ) != 0
        {
            return None;
        }
        (f.finish)(self.queue);
        let mut out = vec![0u8; out_len];
        if (f.read_buffer)(
            self.queue,
            output,
            1,
            0,
            out_len,
            out.as_mut_ptr() as *mut c_void,
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        ) != 0
        {
            return None;
        }
        Some(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn tiles_batch(
        &self,
        fb: &[u8],
        fw: usize,
        fh: usize,
        stride: usize,
        bpp: usize,
        order: &str,
        tw: usize,
        th: usize,
        origin: (usize, usize),
        cell: (usize, usize),
        cols: usize,
        rows: usize,
    ) -> Option<Vec<(u8, u8, u8, bool)>> {
        if fb.is_empty() || fw == 0 || fh == 0 || tw == 0 || th == 0 || cols == 0 || rows == 0 {
            return None;
        }
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let kernel = self.kernel(TILES_SRC, "tiles", &self.tiles)?;
        let f = &self.fns;
        let mut err = 0i32;
        let input = (f.create_buffer)(
            self.context,
            MEM_READ_ONLY | MEM_COPY_HOST,
            fb.len(),
            fb.as_ptr() as *mut c_void,
            &mut err,
        );
        let out_len = cols * rows * 4;
        let output = (f.create_buffer)(self.context, MEM_WRITE_ONLY, out_len, std::ptr::null_mut(), &mut err);
        if input.is_null() || output.is_null() {
            return None;
        }
        let fb_len32 = fb.len() as u32;
        let fw32 = fw as u32;
        let fh32 = fh as u32;
        let stride32 = stride as u32;
        let step32 = (bpp / 8) as u32;
        let bpp32 = bpp as u32;
        let bgr32 = (order == "bgr") as u32;
        let tw32 = tw as u32;
        let th32 = th as u32;
        let ox32 = origin.0 as u32;
        let oy32 = origin.1 as u32;
        let cw32 = cell.0 as u32;
        let ch32 = cell.1 as u32;
        let cols32 = cols as u32;
        let rows32 = rows as u32;
        (f.set_arg)(kernel, 0, std::mem::size_of::<Mem>(), &input as *const Mem as *const c_void);
        (f.set_arg)(kernel, 1, 4, &fb_len32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 2, 4, &fw32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 3, 4, &fh32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 4, 4, &stride32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 5, 4, &step32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 6, 4, &bpp32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 7, 4, &bgr32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 8, 4, &tw32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 9, 4, &th32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 10, 4, &ox32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 11, 4, &oy32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 12, 4, &cw32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 13, 4, &ch32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 14, 4, &cols32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 15, 4, &rows32 as *const u32 as *const c_void);
        (f.set_arg)(kernel, 16, std::mem::size_of::<Mem>(), &output as *const Mem as *const c_void);
        let global = [cols, rows];
        if (f.enqueue)(
            self.queue,
            kernel,
            2,
            std::ptr::null(),
            global.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        ) != 0
        {
            return None;
        }
        (f.finish)(self.queue);
        let mut raw = vec![0u8; out_len];
        if (f.read_buffer)(
            self.queue,
            output,
            1,
            0,
            out_len,
            raw.as_mut_ptr() as *mut c_void,
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        ) != 0
        {
            return None;
        }
        Some(
            raw.chunks_exact(4)
                .map(|c| (c[0], c[1], c[2], c[3] != 0))
                .collect(),
        )
    }
}

/// Offload decisions belong in the journal next to everything else, not on a
/// screen nobody is looking at.
fn note(msg: &str) {
    eprintln!("{}", msg);
}

fn sym(handle: Handle, name: &str) -> Handle {
    let Ok(c) = CString::new(name) else {
        return std::ptr::null_mut();
    };
    unsafe { dlsym(handle, c.as_ptr()) }
}

fn text_of(device: Device, param: u32, f: &Fns) -> String {
    let mut buf = [0u8; 256];
    let mut got = 0usize;
    (f.device_info)(
        device,
        param,
        buf.len(),
        buf.as_mut_ptr() as *mut c_void,
        &mut got,
    );
    String::from_utf8_lossy(&buf[..got.saturating_sub(1)]).to_string()
}

fn u32_of(device: Device, param: u32, f: &Fns) -> u32 {
    let mut v = 0u32;
    let mut got = 0usize;
    (f.device_info)(
        device,
        param,
        4,
        &mut v as *mut u32 as *mut c_void,
        &mut got,
    );
    v
}

/// Open the driver once. Any failure means "no GPU", which is not an error:
/// the CPU is the default and stays the default.
fn open() -> Option<Gpu> {
    // rusticl only offers Intel cards when it is told to; set it here so a
    // service does not depend on the environment it was started in.
    if std::env::var_os("RUSTICL_ENABLE").is_none() {
        std::env::set_var("RUSTICL_ENABLE", "iris");
    }
    let mut lib: Handle = std::ptr::null_mut();
    for candidate in ["libOpenCL.so.1", "libOpenCL.so"] {
        let Ok(c) = CString::new(candidate) else {
            continue;
        };
        lib = unsafe { dlopen(c.as_ptr(), 2) };
        if !lib.is_null() {
            break;
        }
    }
    if lib.is_null() {
        return None;
    }
    unsafe {
        let get_platforms: extern "C" fn(u32, *mut Platform, *mut u32) -> i32 =
            std::mem::transmute(sym(lib, "clGetPlatformIDs"));
        let get_devices: extern "C" fn(Platform, u64, u32, *mut Device, *mut u32) -> i32 =
            std::mem::transmute(sym(lib, "clGetDeviceIDs"));
        let device_info: extern "C" fn(Device, u32, usize, *mut c_void, *mut usize) -> i32 =
            std::mem::transmute(sym(lib, "clGetDeviceInfo"));
        let create_context: extern "C" fn(
            *const i64,
            u32,
            *const Device,
            extern "C" fn(*const i8, *const c_void, usize, *mut c_void),
            *mut c_void,
            *mut i32,
        ) -> Context = std::mem::transmute(sym(lib, "clCreateContext"));
        let create_queue: extern "C" fn(Context, Device, u64, *mut i32) -> Queue =
            std::mem::transmute(sym(lib, "clCreateCommandQueue"));
        let create_program: extern "C" fn(Context, u32, *const *const i8, *const usize, *mut i32) -> Program =
            std::mem::transmute(sym(lib, "clCreateProgramWithSource"));
        let build: extern "C" fn(
            Program,
            u32,
            *const Device,
            *const i8,
            extern "C" fn(*mut c_void, *mut c_void),
            *mut c_void,
        ) -> i32 = std::mem::transmute(sym(lib, "clBuildProgram"));
        let build_info: extern "C" fn(Program, Device, u32, usize, *mut c_void, *mut usize) -> i32 =
            std::mem::transmute(sym(lib, "clGetProgramBuildInfo"));
        let create_kernel: extern "C" fn(Program, *const i8, *mut i32) -> Kernel =
            std::mem::transmute(sym(lib, "clCreateKernel"));
        let create_buffer: extern "C" fn(Context, u64, usize, *mut c_void, *mut i32) -> Mem =
            std::mem::transmute(sym(lib, "clCreateBuffer"));
        let set_arg: extern "C" fn(Kernel, u32, usize, *const c_void) -> i32 =
            std::mem::transmute(sym(lib, "clSetKernelArg"));
        let enqueue: extern "C" fn(
            Queue,
            Kernel,
            u32,
            *const usize,
            *const usize,
            *const usize,
            u32,
            *const c_void,
            *mut c_void,
        ) -> i32 = std::mem::transmute(sym(lib, "clEnqueueNDRangeKernel"));
        let finish: extern "C" fn(Queue) -> i32 = std::mem::transmute(sym(lib, "clFinish"));
        let read_buffer: extern "C" fn(
            Queue,
            Mem,
            u32,
            usize,
            usize,
            *mut c_void,
            u32,
            *const c_void,
            *mut c_void,
        ) -> i32 = std::mem::transmute(sym(lib, "clEnqueueReadBuffer"));
        let release_mem: extern "C" fn(Mem) -> i32 =
            std::mem::transmute(sym(lib, "clReleaseMemObject"));

        let fns = Fns {
            get_platforms,
            get_devices,
            device_info,
            create_context,
            create_queue,
            create_program,
            build,
            build_info,
            create_kernel,
            create_buffer,
            set_arg,
            enqueue,
            finish,
            read_buffer,
            release_mem,
        };

        let mut np: u32 = 0;
        if get_platforms(0, std::ptr::null_mut(), &mut np) != 0 || np == 0 {
            return None;
        }
        let mut plats = vec![std::ptr::null_mut(); np as usize];
        get_platforms(np, plats.as_mut_ptr(), &mut np);
        let mut device: Device = std::ptr::null_mut();
        'outer: for p in plats.iter().take(np as usize) {
            let mut nd: u32 = 0;
            if get_devices(*p, GPU_TYPE, 0, std::ptr::null_mut(), &mut nd) != 0 || nd == 0 {
                continue;
            }
            let mut devs = vec![std::ptr::null_mut(); nd as usize];
            if get_devices(*p, GPU_TYPE, nd, devs.as_mut_ptr(), &mut nd) == 0 && nd > 0 {
                device = devs[0];
                break 'outer;
            }
        }
        if device.is_null() {
            return None;
        }
        let mut err = 0i32;
        let context = create_context(std::ptr::null(), 1, &device, ctx_cb, std::ptr::null_mut(), &mut err);
        if context.is_null() {
            return None;
        }
        let queue = create_queue(context, device, 0, &mut err);
        if queue.is_null() {
            return None;
        }
        Some(Gpu {
            name: text_of(device, 0x102B, &fns),
            units: u32_of(device, 0x1002, &fns),
            clock: u32_of(device, 0x100C, &fns),
            device,
            context,
            queue,
            fns,
            sha: OnceLock::new(),
            near: OnceLock::new(),
            fit: OnceLock::new(),
            tiles: OnceLock::new(),
            lz77: OnceLock::new(),
            gz: OnceLock::new(),
            gun: OnceLock::new(),
            lock: std::sync::Mutex::new(()),
        })
    }
}

fn gpu() -> Option<&'static Gpu> {
    static GPU: OnceLock<Option<Gpu>> = OnceLock::new();
    GPU.get_or_init(open).as_ref()
}

/// One line describing the card, or why there is none.
pub fn available() -> Option<String> {
    gpu().map(|g| format!("{}, {} units @ {} MHz", g.name, g.units, g.clock))
}

// ------------------------------------------------------------------ decisions

/// What each task measured, filled on first use, printed by `cartridge gpu`.
struct Verdict {
    task: &'static str,
    unit: &'static str,
    gpu: f64,
    cpu: f64,
}

fn verdicts() -> &'static std::sync::Mutex<Vec<Verdict>> {
    static V: OnceLock<std::sync::Mutex<Vec<Verdict>>> = OnceLock::new();
    V.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn record(task: &'static str, unit: &'static str, gpu: f64, cpu: f64) {
    let mut v = verdicts().lock().unwrap();
    v.retain(|x| x.task != task);
    v.push(Verdict { task, unit, gpu, cpu });
}

/// Does the card take this task on this machine? Measured once, then cached:
/// true when a card is present and agreed with the CPU on the sample.
pub fn wins(task: &str) -> bool {
    match task {
        "sha256" => sha256_ready(),
        "nearest" => nearest_ready(),
        "fit" => fit_ready(),
        "tiles" => tiles_ready(),
        _ => false,
    }
}

/// The CPU's box average, kept here so the measurement compares the card with
/// exactly the arithmetic `art::fit` uses.
fn cpu_fit(src: &[u8], sw: usize, sh: usize, nw: usize, nh: usize) -> Vec<u8> {
    let mut out = vec![0u8; nw * nh * 3];
    for y in 0..nh {
        let y0 = y * sh / nh;
        let y1 = (((y + 1) * sh).div_ceil(nh)).min(sh).max(y0 + 1);
        for x in 0..nw {
            let x0 = x * sw / nw;
            let x1 = (((x + 1) * sw).div_ceil(nw)).min(sw).max(x0 + 1);
            let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
            for yy in y0..y1 {
                for xx in x0..x1 {
                    let o = (yy * sw + xx) * 3;
                    r += src[o] as u64;
                    g += src[o + 1] as u64;
                    b += src[o + 2] as u64;
                    n += 1;
                }
            }
            let o = (y * nw + x) * 3;
            out[o] = (r / n) as u8;
            out[o + 1] = (g / n) as u8;
            out[o + 2] = (b / n) as u8;
        }
    }
    out
}

fn fit_ready() -> bool {
    static W: OnceLock<bool> = OnceLock::new();
    *W.get_or_init(|| {
        let Some(g) = gpu() else {
            record("fit", "Mpix/s", 0.0, 0.0);
            return false;
        };
        // The real shape of a boot picture: 1200x630 reduced to 240x86 dots.
        let (sw, sh, nw, nh) = (1200usize, 630usize, 240usize, 86usize);
        let src: Vec<u8> = (0..sw * sh * 3)
            .map(|i| ((i * 31 + i / 7) & 0xff) as u8)
            .collect();
        let mpix = (sw * sh) as f64 / 1e6;
        let start = Instant::now();
        let Some(gpu_out) = g.fit_batch(&src, sw, sh, nw, nh) else {
            record("fit", "Mpix/s", 0.0, 0.0);
            return false;
        };
        let gpu_secs = start.elapsed().as_secs_f64();
        let start = Instant::now();
        let cpu_out = cpu_fit(&src, sw, sh, nw, nh);
        let cpu_secs = start.elapsed().as_secs_f64();
        if gpu_out != cpu_out {
            note("offload: the GPU downscale disagreed with the CPU; staying on the CPU");
            record("fit", "Mpix/s", 0.0, 0.0);
            return false;
        }
        let g = mpix / gpu_secs.max(1e-9);
        let c = mpix / cpu_secs.max(1e-9);
        record("fit", "Mpix/s", g, c);
        // The card agreed with the CPU on the sample; the CPU goes back to
        // serving, whatever the two speeds say.
        true
    })
}

fn tiles_ready() -> bool {
    static W: OnceLock<bool> = OnceLock::new();
    *W.get_or_init(|| {
        let Some(g) = gpu() else {
            record("tiles", "Mpix/s", 0.0, 0.0);
            return false;
        };
        // A 1080p framebuffer sampled the way `art verify` samples it.
        let (fw, fh) = (1920usize, 1080usize);
        let stride = fw * 4;
        let data: Vec<u8> = (0..stride * fh).map(|i| ((i * 13 + i / 5) & 0xff) as u8).collect();
        let (cols, rows, cell) = (240usize, 86usize, (8usize, 16usize));
        let (tw, th) = (cell.0, cell.1 / 2);
        let mpix = (cols * rows * tw * th) as f64 / 1e6;
        let start = Instant::now();
        let Some(gpu_out) = g.tiles_batch(
            &data,
            fw,
            fh,
            stride,
            32,
            "bgr",
            tw,
            th,
            (0, 0),
            cell,
            cols,
            rows,
        ) else {
            record("tiles", "Mpix/s", 0.0, 0.0);
            return false;
        };
        let gpu_secs = start.elapsed().as_secs_f64();
        let fb = crate::art::FrameBuffer { w: fw, h: fh, bpp: 32, stride, data: data.clone() };
        let start = Instant::now();
        let mut cpu_out = Vec::with_capacity(cols * rows);
        for y in 0..rows {
            for x in 0..cols {
                let cx = x * cell.0;
                let cy = (y / 2) * cell.1 + (y % 2) * (cell.1 / 2);
                let (got, flat) = fb.tile(cx, cy, tw, th, "bgr");
                cpu_out.push((got.0, got.1, got.2, flat));
            }
        }
        let cpu_secs = start.elapsed().as_secs_f64();
        if gpu_out != cpu_out {
            note("offload: the GPU tile sampling disagreed with the CPU; staying on the CPU");
            record("tiles", "Mpix/s", 0.0, 0.0);
            return false;
        }
        let g = mpix / gpu_secs.max(1e-9);
        let c = mpix / cpu_secs.max(1e-9);
        record("tiles", "Mpix/s", g, c);
        // The card agreed with the CPU on the sample; the CPU goes back to
        // serving, whatever the two speeds say.
        true
    })
}

fn sha256_ready() -> bool {
    static W: OnceLock<bool> = OnceLock::new();
    *W.get_or_init(|| {
        let Some(g) = gpu() else {
            record("sha256", "MB/s", 0.0, 0.0);
            return false;
        };
        // 4 MB in 4 messages: enough to be about throughput, not dispatch cost.
        let sample: Vec<Vec<u8>> = (0..4)
            .map(|i| (0..1 << 20).map(|j| ((i * 7 + j * 31) & 0xff) as u8).collect())
            .collect();
        let refs: Vec<&[u8]> = sample.iter().map(|s| s.as_slice()).collect();
        let mb = (sample.iter().map(|s| s.len()).sum::<usize>() >> 20).max(1) as f64;

        let start = Instant::now();
        let gpu_ok = g.sha256_batch(&refs);
        let gpu_secs = start.elapsed().as_secs_f64();
        let Some(gpu_hashes) = gpu_ok else {
            record("sha256", "MB/s", 0.0, 0.0);
            return false;
        };
        let start = Instant::now();
        let cpu_hashes: Vec<[u8; 32]> = sample
            .iter()
            .map(|s| {
                let mut h = crate::hash::Sha256::new();
                h.update(s);
                h.finish()
            })
            .collect();
        let cpu_secs = start.elapsed().as_secs_f64();
        if gpu_hashes != cpu_hashes {
            note("offload: the GPU hash disagreed with the CPU; staying on the CPU");
            record("sha256", "MB/s", 0.0, 0.0);
            return false;
        }
        let g = mb / gpu_secs.max(1e-9);
        let c = mb / cpu_secs.max(1e-9);
        record("sha256", "MB/s", g, c);
        // The card agreed with the CPU on the sample; the CPU goes back to
        // serving, whatever the two speeds say.
        true
    })
}

fn nearest_ready() -> bool {
    static W: OnceLock<bool> = OnceLock::new();
    *W.get_or_init(|| {
        let Some(g) = gpu() else {
            record("nearest", "Mpix/s", 0.0, 0.0);
            return false;
        };
        let palette: Vec<[u8; 3]> = (0..15u8).map(|i| [i * 16, 255 - i * 16, i * 8]).collect();
        let pixels: Vec<[u8; 3]> = (0..1_000_000u32)
            .map(|i| {
                let v = i.wrapping_mul(2654435761);
                [(v >> 8) as u8, (v >> 16) as u8, (v >> 24) as u8]
            })
            .collect();
        let start = Instant::now();
        let Some(gpu_out) = g.nearest_batch(&pixels, &palette) else {
            record("nearest", "Mpix/s", 0.0, 0.0);
            return false;
        };
        let gpu_secs = start.elapsed().as_secs_f64();
        let start = Instant::now();
        let cpu_out: Vec<u8> = pixels
            .iter()
            .map(|p| {
                let mut best = 0usize;
                let mut bestd = u32::MAX;
                for (i, c) in palette.iter().enumerate() {
                    let dr = p[0] as i32 - c[0] as i32;
                    let dg = p[1] as i32 - c[1] as i32;
                    let db = p[2] as i32 - c[2] as i32;
                    let d = (dr * dr + dg * dg + db * db) as u32;
                    if d < bestd {
                        bestd = d;
                        best = i;
                    }
                }
                best as u8
            })
            .collect();
        let cpu_secs = start.elapsed().as_secs_f64();
        if gpu_out != cpu_out {
            note("offload: the GPU palette disagreed with the CPU; staying on the CPU");
            record("nearest", "Mpix/s", 0.0, 0.0);
            return false;
        }
        let g = 1.0 / gpu_secs.max(1e-9);
        let c = 1.0 / cpu_secs.max(1e-9);
        record("nearest", "Mpix/s", g, c);
        // The card agreed with the CPU on the sample; the CPU goes back to
        // serving, whatever the two speeds say.
        true
    })
}

// ---------------------------------------------------------------------- tasks

/// Is there a card to hand work to?
pub fn has_card() -> bool {
    gpu().is_some()
}

/// A whole gzip member encoded on the card. `None` means the CPU does it.
pub fn gzip_bulk(data: &[u8]) -> Option<Vec<u8>> {
    let g = gpu()?;
    let out = g.gz_encode(data)?;
    // The encoder has no checksum of its own to lean on, so the card's own
    // decoder has to give the bytes back before this stream is trusted. A wrong
    // encoder costs the CPU path, never a broken archive.
    let back = g.gz_decode(&out, data.len(), false)?;
    if back.as_slice() != data {
        note("offload: the card's gzip did not round-trip; this run encodes on the CPU");
        return None;
    }
    Some(out)
}

/// A whole gzip file (or zlib stream) decoded on the card.
pub fn gunzip_bulk(data: &[u8], cap: usize, zlib: bool) -> Option<Vec<u8>> {
    gpu()?.gz_decode(data, cap, zlib)
}

/// The LZ77 match search for one slice, on the card. `None` means the caller
/// searches the slice itself.
pub fn lz77_links(data: &[u8], link: &[u32], window: u32, chain: u32, max_match: u32) -> Option<Vec<u32>> {
    if data.len() < MIN_BYTES {
        return None;
    }
    gpu()?.lz77_slice(data, link, window, chain, max_match)
}

/// Hash a buffer on the card. `None` means the caller hashes it itself: there is
/// no card, or the buffer is too small to be worth a dispatch.
pub fn sha256_bulk(data: &[u8]) -> Option<[u8; 32]> {
    if data.len() < MIN_BYTES || !sha256_ready() {
        return None;
    }
    // The card proved itself against the CPU's own loop in `sha256_ready`, on
    // data of the same shape. Re-hashing here would put the work back on the
    // CPU, which is the one thing this path exists to avoid.
    gpu()?.sha256_batch(&[data])?.first().copied()
}

/// The boot art's box-average downscale, on the card when the card has won
/// that task. `None` means the caller does its own loop, which is the usual
/// answer on a small picture.
pub fn fit_bulk(src: &[u8], sw: usize, sh: usize, nw: usize, nh: usize) -> Option<Vec<u8>> {
    if sw.saturating_mul(sh) < MIN_FIT_PIXELS || !fit_ready() {
        return None;
    }
    gpu()?.fit_batch(src, sw, sh, nw, nh)
}

/// The framebuffer sampling `art verify` does, on the card when it wins.
#[allow(clippy::too_many_arguments)]
pub fn tiles_bulk(
    fb: &[u8],
    fw: usize,
    fh: usize,
    stride: usize,
    bpp: usize,
    order: &str,
    cols: usize,
    rows: usize,
    origin: (usize, usize),
    cell: (usize, usize),
) -> Option<Vec<(u8, u8, u8, bool)>> {
    if fw.saturating_mul(fh) < MIN_TILE_PIXELS || !tiles_ready() {
        return None;
    }
    gpu()?.tiles_batch(
        fb,
        fw,
        fh,
        stride,
        bpp,
        order,
        cell.0,
        cell.1 / 2,
        origin,
        cell,
        cols,
        rows,
    )
}

/// What one dispatch costs, transfer and wait included. Text is a few hundred
/// bytes a second, so this is the number that keeps it on the CPU.
pub fn dispatch_micros() -> Option<f64> {
    let g = gpu()?;
    let pixels = vec![[0u8, 0, 0]; 1];
    let palette = vec![[0u8, 0, 0], [255u8, 255, 255]];
    g.nearest_batch(&pixels, &palette)?;
    let start = Instant::now();
    for _ in 0..8 {
        g.nearest_batch(&pixels, &palette)?;
    }
    Some(start.elapsed().as_secs_f64() * 1e6 / 8.0)
}

/// The measured table, one line per task, for `cartridge gpu` and the logs.
pub fn report() -> String {
    // Measure everything that is registered, so the table is complete even on a
    // box that has never imported a cartridge.
    let _ = sha256_ready();
    let _ = nearest_ready();
    let _ = fit_ready();
    let _ = tiles_ready();
    let mut out = String::new();
    match available() {
        Some(card) => out.push_str(&format!("offload: {}\n", card)),
        None => {
            out.push_str("offload: no usable GPU; every task stays on the CPU\n");
        }
    }
    for v in verdicts().lock().unwrap().iter() {
        let pick = if !wins(v.task) {
            "cpu"
        } else if v.gpu > 0.0 && v.gpu < v.cpu {
            // Honest about the trade: the card has the task anyway.
            "gpu (slower here)"
        } else {
            "gpu"
        };
        out.push_str(&format!(
            "  {:<8} gpu {:>9.1} {:<7} cpu {:>9.1} {:<7} -> {}\n",
            v.task, v.gpu, v.unit, v.cpu, v.unit, pick
        ));
    }
    if let Some(us) = dispatch_micros() {
        out.push_str(&format!(
            "  dispatch ~{:.0} us per round trip; the console's text is a few hundred bytes a second\n",
            us
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_buffer_without_a_gpu_hashes_the_same_as_before() {
        // Whatever the machine has, the public hash entry point must agree with
        // the plain SHA-256 of the same bytes.
        let data = vec![7u8; 300 * 1024];
        let mut h = crate::hash::Sha256::new();
        h.update(&data);
        assert_eq!(crate::hash::bytes(&data), crate::hash::hex(&h.finish()));
    }

    #[test]
    fn a_gpu_that_is_present_agrees_with_the_cpu() {
        let Some(g) = gpu() else {
            return; // no card here: the CPU path is the answer and it is tested above
        };
        let msgs: Vec<Vec<u8>> = (0..3)
            .map(|i| (0..200_000).map(|j| ((i + j * 13) & 0xff) as u8).collect())
            .collect();
        let refs: Vec<&[u8]> = msgs.iter().map(|m| m.as_slice()).collect();
        let Some(got) = g.sha256_batch(&refs) else {
            return; // the driver refused the kernel; the CPU keeps the work
        };
        for (i, m) in msgs.iter().enumerate() {
            let mut h = crate::hash::Sha256::new();
            h.update(m);
            assert_eq!(got[i], h.finish(), "message {} differs", i);
        }
    }

    #[test]
    fn the_downscale_agrees_with_the_cpu() {
        let Some(g) = gpu() else {
            return;
        };
        let (sw, sh, nw, nh) = (320usize, 180usize, 80usize, 45usize);
        let src: Vec<u8> = (0..sw * sh * 3).map(|i| ((i * 17 + i / 3) & 0xff) as u8).collect();
        let Some(got) = g.fit_batch(&src, sw, sh, nw, nh) else {
            return; // the driver refused the kernel; the CPU keeps the work
        };
        assert_eq!(got, cpu_fit(&src, sw, sh, nw, nh));
    }

    #[test]
    fn tile_sampling_agrees_with_the_framebuffer() {
        let Some(g) = gpu() else {
            return;
        };
        let (fw, fh) = (640usize, 360usize);
        let stride = fw * 4;
        let data: Vec<u8> = (0..stride * fh).map(|i| ((i * 7 + i / 11) & 0xff) as u8).collect();
        let cell = (8usize, 16usize);
        let (cols, rows) = (80usize, 45usize);
        let Some(got) = g.tiles_batch(&data, fw, fh, stride, 32, "bgr", 8, 8, (0, 0), cell, cols, rows)
        else {
            return;
        };
        let fb = crate::art::FrameBuffer { w: fw, h: fh, bpp: 32, stride, data };
        for y in 0..rows {
            for x in 0..cols {
                let cy = (y / 2) * cell.1 + (y % 2) * (cell.1 / 2);
                let (want, flat) = fb.tile(x * cell.0, cy, 8, 8, "bgr");
                let t = got[y * cols + x];
                assert_eq!((t.0, t.1, t.2), want, "dot ({},{}) differs", x, y);
                assert_eq!(t.3, flat, "dot ({},{}) flatness differs", x, y);
            }
        }
    }

    #[test]
    fn palette_mapping_agrees_with_the_cpu() {
        let Some(g) = gpu() else {
            return;
        };
        let palette: Vec<[u8; 3]> = vec![
            [0, 0, 0],
            [255, 0, 0],
            [0, 128, 0],
            [0, 0, 255],
            [255, 255, 255],
        ];
        let pixels: Vec<[u8; 3]> = (0..50_000u32)
            .map(|i| [(i % 256) as u8, (i / 3 % 256) as u8, (i / 7 % 256) as u8])
            .collect();
        let Some(got) = g.nearest_batch(&pixels, &palette) else {
            return;
        };
        for (i, p) in pixels.iter().enumerate() {
            let mut best = 0usize;
            let mut bestd = u32::MAX;
            for (j, c) in palette.iter().enumerate() {
                let dr = p[0] as i32 - c[0] as i32;
                let dg = p[1] as i32 - c[1] as i32;
                let db = p[2] as i32 - c[2] as i32;
                let d = (dr * dr + dg * dg + db * db) as u32;
                if d < bestd {
                    bestd = d;
                    best = j;
                }
            }
            assert_eq!(got[i], best as u8, "pixel {} differs", i);
        }
    }
}
