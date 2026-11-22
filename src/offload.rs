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
/// The GPU has to be this much faster to be trusted with a task.
const MARGIN: f64 = 1.2;

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
    /// A command queue is not safe for concurrent enqueues, so every dispatch
    /// takes this. Bulk work is rare; serialising it costs nothing.
    lock: std::sync::Mutex<()>,
}

// The handles are opaque driver objects. Everything that touches the queue is
// behind `lock`, and the kernel slots are `OnceLock`s, so sharing this across
// threads is sound.
unsafe impl Send for Gpu {}
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

/// Did the GPU win this task on this machine? Measured once, then cached.
pub fn wins(task: &str) -> bool {
    match task {
        "sha256" => sha256_wins(),
        "nearest" => nearest_wins(),
        _ => false,
    }
}

fn sha256_wins() -> bool {
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
        g > c * MARGIN
    })
}

fn nearest_wins() -> bool {
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
        g > c * MARGIN
    })
}

// ---------------------------------------------------------------------- tasks

/// Hash a buffer on the GPU when the card has won the hashing task. Returns
/// `None` when the caller should use the CPU, which is the common case.
pub fn sha256_bulk(data: &[u8]) -> Option<[u8; 32]> {
    if data.len() < MIN_BYTES || !sha256_wins() {
        return None;
    }
    let g = gpu()?;
    let hashes = g.sha256_batch(&[data])?;
    // The decision was measured on a sample; this makes sure a driver that
    // misbehaves on unusual input cannot corrupt a store.
    let mut cpu = crate::hash::Sha256::new();
    cpu.update(data);
    if hashes[0] != cpu.finish() {
        note("offload: GPU hash mismatch on real data; this run uses the CPU");
        return None;
    }
    Some(hashes[0])
}

/// The measured table, one line per task, for `cartridge gpu` and the logs.
pub fn report() -> String {
    // Measure everything that is registered, so the table is complete even on a
    // box that has never imported a cartridge.
    let _ = sha256_wins();
    let _ = nearest_wins();
    let mut out = String::new();
    match available() {
        Some(card) => out.push_str(&format!("offload: {}\n", card)),
        None => {
            out.push_str("offload: no usable GPU; every task stays on the CPU\n");
        }
    }
    for v in verdicts().lock().unwrap().iter() {
        let pick = if wins(v.task) { "gpu" } else { "cpu" };
        out.push_str(&format!(
            "  {:<8} gpu {:>9.1} {:<7} cpu {:>9.1} {:<7} -> {}\n",
            v.task, v.gpu, v.unit, v.cpu, v.unit, pick
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
