//! The chimes, made and played by this binary: a small sine-bell synthesiser
//! writes 16-bit stereo WAVs, and they are handed to the sound card through
//! ALSA's PCM ioctls. `aplay` is only a fallback if the device refuses the
//! parameter set.

use std::fs;
use std::os::raw::c_void;
use std::path::Path;

const RATE: u32 = 44100;
const DIR: &str = "/var/lib/cartridge/sounds";

extern "C" {
    fn ioctl(fd: i32, request: u64, ...) -> i32;
}

fn wav_header(data_len: u32) -> Vec<u8> {
    let mut h = Vec::with_capacity(44);
    let byte_rate = RATE * 2 * 2;
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(36 + data_len).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes()); // PCM
    h.extend_from_slice(&2u16.to_le_bytes()); // stereo
    h.extend_from_slice(&RATE.to_le_bytes());
    h.extend_from_slice(&byte_rate.to_le_bytes());
    h.extend_from_slice(&4u16.to_le_bytes()); // block align
    h.extend_from_slice(&16u16.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data_len.to_le_bytes());
    h
}

fn envelope(t: f32, dur: f32, attack: f32, decay: f32) -> f32 {
    if t < attack {
        t / attack
    } else {
        let x = (t - attack) / dur.max(0.001);
        (-x * decay).exp()
    }
}

/// One bell: fundamental plus two quiet partials, exponential decay.
fn bell(freq: f32, dur: f32, amp: f32, decay: f32, out: &mut [f32]) {
    let n = (dur * RATE as f32) as usize;
    for i in 0..n {
        let t = i as f32 / RATE as f32;
        let env = envelope(t, dur, 0.006, decay);
        let phase = 2.0 * std::f32::consts::PI * freq * t;
        let sample = phase.sin() + 0.30 * (2.0 * phase).sin() + 0.10 * (3.0 * phase).sin();
        let idx = i;
        if idx < out.len() {
            out[idx] += sample * env * amp;
        }
    }
}

/// The completion chime: a rising major arpeggio, C5 E5 G5 C6, the last note
/// left to ring so it sounds finished rather than cut off.
fn complete_samples() -> Vec<f32> {
    let notes = [(523.25f32, 0.00f32), (659.25, 0.13), (783.99, 0.26), (1046.50, 0.39)];
    let total = 1.10;
    let mut out = vec![0.0f32; (total * RATE as f32) as usize];
    for (freq, at) in notes {
        let start = (at * RATE as f32) as usize;
        let dur = total - at;
        let mut part = vec![0.0f32; (dur * RATE as f32) as usize];
        bell(freq, dur, 0.55, 4.2, &mut part);
        for (i, s) in part.iter().enumerate() {
            if start + i < out.len() {
                out[start + i] += s;
            }
        }
    }
    out
}

/// The failure chime: two low, slightly detuned notes stepping down, with a
/// tremolo so it reads as "no" and not as another success sound.
fn failed_samples() -> Vec<f32> {
    let total = 0.75;
    let mut out = vec![0.0f32; (total * RATE as f32) as usize];
    let notes = [(196.00f32, 0.00f32, 0.22f32), (146.83, 0.24, 0.48)];
    for (freq, at, dur) in notes {
        let start = (at * RATE as f32) as usize;
        let n = (dur * RATE as f32) as usize;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            let env = envelope(t, dur, 0.008, 3.4);
            let phase = 2.0 * std::f32::consts::PI * freq * t;
            let trem = 0.82 + 0.18 * (2.0 * std::f32::consts::PI * 17.0 * t).sin();
            let sample = phase.sin() + 0.45 * (2.0 * phase).sin() + 0.20 * (3.0 * phase).sin();
            let v = sample * env * trem * 0.5;
            if start + i < out.len() {
                out[start + i] += v;
            }
        }
    }
    out
}

fn to_wav(samples: &[f32]) -> Vec<u8> {
    let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs())).max(0.001);
    let gain = 0.94 / peak;
    let mut data = Vec::with_capacity(samples.len() * 4);
    for s in samples {
        let v = (s * gain).clamp(-1.0, 1.0);
        let x = (v * 32767.0) as i16;
        data.extend_from_slice(&x.to_le_bytes());
        data.extend_from_slice(&x.to_le_bytes());
    }
    let mut out = wav_header(data.len() as u32);
    out.extend_from_slice(&data);
    out
}

pub fn ensure_builtin() -> Result<(), String> {
    fs::create_dir_all(DIR).map_err(|e| format!("sounds: {}", e))?;
    let want = [
        ("complete.wav", true),
        ("failed.wav", false),
    ];
    for (name, ok) in want {
        let path = format!("{}/{}", DIR, name);
        if !Path::new(&path).is_file() {
            let samples = if ok { complete_samples() } else { failed_samples() };
            fs::write(&path, to_wav(&samples))
                .map_err(|e| format!("sounds: {}: {}", path, e))?;
        }
    }
    Ok(())
}

fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// struct snd_interval { min, max, flags } is 12 bytes; the mask set starts
/// after flags, mres and intervals, so the offsets below match the kernel's
/// 608-byte snd_pcm_hw_params.
fn set_interval(buf: &mut [u8], idx: usize, min: u32, max: u32, integer: bool) {
    let off = 260 + idx * 12;
    put_u32(buf, off, min);
    put_u32(buf, off + 4, max);
    put_u32(buf, off + 8, if integer { 4 } else { 0 });
}

fn set_mask(buf: &mut [u8], idx: usize, bit: u32) {
    let off = 4 + idx * 32;
    put_u32(buf, off, 1 << bit);
}

fn errno_text() -> String {
    std::io::Error::last_os_error().to_string()
}

fn open_device() -> Option<i32> {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir("/dev/snd") {
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("pcmC") && n.ends_with('p'))
            .collect();
        names.sort();
        for n in names {
            candidates.push(format!("/dev/snd/{}", n));
        }
    }
    for path in candidates {
        let file = match fs::OpenOptions::new().write(true).read(true).open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        let mut params = [0u8; 608];
        // ACCESS = RW_INTERLEAVED (3), FORMAT = S16_LE (2), SUBFORMAT = STD (0)
        set_mask(&mut params, 0, 3);
        set_mask(&mut params, 1, 2);
        set_mask(&mut params, 2, 0);
        for i in 0..12 {
            set_interval(&mut params, i, 0, u32::MAX, false);
        }
        set_interval(&mut params, 0, 16, 16, true); // SAMPLE_BITS
        set_interval(&mut params, 1, 32, 32, true); // FRAME_BITS
        set_interval(&mut params, 2, 2, 2, true); // CHANNELS
        set_interval(&mut params, 3, RATE, RATE, true); // RATE
        set_interval(&mut params, 5, 1024, 1024, true); // PERIOD_SIZE
        set_interval(&mut params, 9, 4096, 4096, true); // BUFFER_SIZE
        let rc = unsafe {
            ioctl(
                fd,
                0xC2604111u64, // SNDRV_PCM_IOCTL_HW_PARAMS (size 608)
                &mut params as *mut [u8; 608] as *mut c_void,
            )
        };
        let debug = std::env::var("CARTRIDGE_SOUND_DEBUG").is_ok();
        if rc < 0 {
            if debug {
                eprintln!("sound: hw_params on {} failed: {}", path, errno_text());
            }
            continue;
        }
        let prep = unsafe { ioctl(fd, 0x4140u64, 0 as *mut c_void) };
        if prep < 0 {
            if debug {
                eprintln!("sound: prepare on {} failed: {}", path, errno_text());
            }
            continue;
        }
        std::mem::forget(file);
        return Some(fd);
    }
    None
}

#[repr(C)]
struct Wav {
    channels: u16,
    rate: u32,
    data: Vec<u8>,
}

fn parse_wav(bytes: &[u8]) -> Option<Wav> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12usize;
    let mut channels = 0u16;
    let mut rate = 0u32;
    let mut bits = 0u16;
    let mut data: Option<Vec<u8>> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]])
            as usize;
        let body = pos + 8;
        if body + len > bytes.len() {
            break;
        }
        if id == b"fmt " && len >= 16 {
            channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]);
            rate = u32::from_le_bytes([
                bytes[body + 4],
                bytes[body + 5],
                bytes[body + 6],
                bytes[body + 7],
            ]);
            bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
        } else if id == b"data" {
            data = Some(bytes[body..body + len].to_vec());
        }
        pos = body + len + (len & 1);
    }
    let data = data?;
    if channels == 0 || rate == 0 || bits != 16 {
        return None;
    }
    Some(Wav { channels, rate, data })
}

fn samples_of(wav: &Wav) -> Vec<i16> {
    wav.data
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// Mix to stereo 44.1 kHz: duplicate mono, drop extra channels, interpolate
/// when the file uses another rate.
fn to_playback(wav: &Wav) -> Vec<u8> {
    let src = samples_of(wav);
    let frames: Vec<(i16, i16)> = src
        .chunks(wav.channels as usize)
        .map(|f| {
            let l = f[0];
            let r = if f.len() > 1 { f[1] } else { f[0] };
            (l, r)
        })
        .collect();
    let ratio = RATE as f64 / wav.rate as f64;
    let out_frames = ((frames.len() as f64) * ratio).ceil() as usize;
    let mut out = Vec::with_capacity(out_frames * 4);
    for i in 0..out_frames {
        let src_pos = i as f64 / ratio;
        let idx = src_pos as usize;
        let frac = (src_pos - idx as f64) as f32;
        let a = frames[idx.min(frames.len() - 1)];
        let b = frames[(idx + 1).min(frames.len() - 1)];
        let l = a.0 as f32 * (1.0 - frac) + b.0 as f32 * frac;
        let r = a.1 as f32 * (1.0 - frac) + b.1 as f32 * frac;
        out.extend_from_slice(&(l as i16).to_le_bytes());
        out.extend_from_slice(&(r as i16).to_le_bytes());
    }
    out
}

fn play_alsa(pcm: &[u8]) -> bool {
    let fd = match open_device() {
        Some(fd) => fd,
        None => return false,
    };
    unsafe {
        extern "C" {
            fn write(fd: i32, buf: *const c_void, count: usize) -> isize;
            fn close(fd: i32) -> i32;
        }
        let mut off = 0usize;
        let mut ok = true;
        while off < pcm.len() {
            let n = unsafe { write(fd, pcm[off..].as_ptr() as *const c_void, pcm.len() - off) };
            if n <= 0 {
                ok = false;
                break;
            }
            off += n as usize;
        }
        // DRAIN, then release the device.
        let _ = unsafe { ioctl(fd, 0x4143u64, 0 as *mut c_void) };
        unsafe {
            close(fd);
        }
        ok
    }
}

fn play_aplay(path: &str) -> bool {
    std::process::Command::new("aplay")
        .args(["-q", path])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Play one chime. Returns how it was played, for the log.
pub fn play(path: &str) -> Result<String, String> {
    if !Path::new(path).is_file() {
        return Err(format!("{} is missing", path));
    }
    let bytes = fs::read(path).map_err(|e| format!("{}: {}", path, e))?;
    if let Some(wav) = parse_wav(&bytes) {
        let pcm = to_playback(&wav);
        if play_alsa(&pcm) {
            return Ok("ALSA".into());
        }
    }
    if play_aplay(path) {
        return Ok("aplay".into());
    }
    Err("the sound card did not accept the chime".into())
}

pub fn builtin(kind: &str) -> String {
    format!("{}/{}.wav", DIR, kind)
}
