//! Private socket-activated HAT audio tests. Never exposes recorded audio.
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const ALSA_CARD: &str = "/proc/asound/aic3104";
const DEVICE: &str = "hw:aic3104,0";
const STATE: &str = "/run/duck-audio-test/lock";
const RATE: usize = 48_000;
const MIC_BYTES: usize = RATE * 4;
const WAIT: Duration = Duration::from_secs(3);
const COOLDOWN_MS: u64 = 3_000;

fn result(state: &str) -> Value {
    json!({"version": 1, "state": state})
}

fn pcm_status(direction: char) -> &'static str {
    match std::fs::metadata(format!("{ALSA_CARD}/pcm0{direction}/info")) {
        Ok(_) => "ready",
        Err(e) if e.kind() == io::ErrorKind::NotFound => "unavailable",
        Err(_) => "unknown",
    }
}

fn now_ms() -> u64 {
    duck_ipc_proto::clock::monotonic_ns() / 1_000_000
}

fn acquire() -> Result<File, &'static str> {
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(STATE)
        .map_err(|_| "error")?;
    match f.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Err("busy"),
        Err(_) => return Err("error"),
    }
    let mut last = String::new();
    (&mut f)
        .take(64)
        .read_to_string(&mut last)
        .map_err(|_| "error")?;
    let now = now_ms();
    if last
        .trim()
        .parse::<u64>()
        .is_ok_and(|previous| previous <= now && now - previous < COOLDOWN_MS)
    {
        return Err("busy");
    }
    f.set_len(0).map_err(|_| "error")?;
    f.rewind().map_err(|_| "error")?;
    write!(f, "{now}").map_err(|_| "error")?;
    Ok(f)
}

fn diagnose(speaker: bool) -> Value {
    let _lock = match acquire() {
        Ok(lock) => lock,
        Err(state) => return result(state),
    };
    if pcm_status(if speaker { 'p' } else { 'c' }) == "unavailable" {
        return result("unavailable");
    }
    let program = if speaker {
        "/usr/bin/aplay"
    } else {
        "/usr/bin/arecord"
    };
    let mut command = Command::new(program);
    command.env("LC_ALL", "C").args([
        "-q", "-N", "-D", DEVICE, "-t", "raw", "-f", "S16_LE", "-c", "2", "-r", "48000",
    ]);
    if speaker {
        command.stdin(Stdio::piped()).stdout(Stdio::null());
    } else {
        command
            .args(["-d", "1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped());
    }
    let mut child = match command.stderr(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return result("unavailable"),
        Err(_) => return result("error"),
    };
    let stderr = child.stderr.take().expect("piped");
    let err_reader = thread::spawn(move || read_bounded(stderr, 4096));
    let input = child
        .stdin
        .take()
        .map(|mut stdin| thread::spawn(move || stdin.write_all(&tone())));
    let output = child
        .stdout
        .take()
        .map(|stdout| thread::spawn(move || read_bounded(stdout, MIC_BYTES + 1)));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if start.elapsed() < WAIT => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let err = err_reader.join().unwrap_or_default();
    let input_ok = input.is_none_or(|t| t.join().is_ok_and(|r| r.is_ok()));
    let samples = output.map(|t| t.join().unwrap_or_default());
    match status {
        Some(s) if s.success() && input_ok && speaker => result("accepted"),
        Some(s) if s.success() && !speaker => samples.as_deref().and_then(level_percent)
            .map_or_else(|| result("error"), |pct| json!({
                "version": 1, "state": if pct == 0 {"quiet"} else {"ok"}, "level_percent": pct
            })),
        Some(s) if !s.success() && String::from_utf8_lossy(&err).contains("Device or resource busy") => result("busy"),
        _ => result("error"),
    }
}

fn read_bounded(src: impl Read, cap: usize) -> Vec<u8> {
    let mut result = Vec::new();
    let _ = src.take(cap as u64).read_to_end(&mut result);
    result
}

fn level_percent(samples: &[u8]) -> Option<u32> {
    if samples.len() != MIC_BYTES {
        return None;
    }
    // Only the HAT's wired right channel. AC RMS removes DC bias; 100% means
    // full-scale RMS, not acoustic dB or a calibrated loudness measurement.
    let (sum, squares) = samples
        .chunks_exact(4)
        .fold((0.0, 0.0), |(sum, squares), frame| {
            let right = f64::from(i16::from_le_bytes([frame[2], frame[3]]));
            (sum + right, squares + right * right)
        });
    let count = RATE as f64;
    let rms = (squares / count - (sum / count).powi(2)).max(0.0).sqrt();
    Some((100.0 * rms / 32768.0).round() as u32)
}

fn tone() -> Vec<u8> {
    let frames = RATE / 4;
    let fade = RATE / 100;
    let mut raw = Vec::with_capacity(frames * 4);
    for i in 0..frames {
        let envelope = i.min(frames - 1 - i).min(fade) as f64 / fade as f64;
        let t = std::f64::consts::TAU * 660.0 * i as f64 / RATE as f64;
        // About -24 dBFS peak, no mixer changes.
        let word = ((t.sin() * envelope * 2048.0) as i16).to_le_bytes();
        raw.extend_from_slice(&word);
        raw.extend_from_slice(&word);
    }
    raw
}

fn dispatch(bytes: &[u8]) -> Value {
    match bytes {
        b"speaker\n" => diagnose(true),
        b"mic\n" => diagnose(false),
        b"status\n" => {
            json!({"version":1,"speaker":{"state":pcm_status('p')},"mic":{"state":pcm_status('c')}})
        }
        _ => result("error"),
    }
}

fn main() -> io::Result<()> {
    // Read at most one fixed command, capped below any input chosen by the peer.
    // The socket-activated Type=oneshot unit supplies the absolute input deadline.
    let mut input = Vec::with_capacity(8);
    use std::io::BufRead;
    io::BufReader::new(io::stdin().take(8)).read_until(b'\n', &mut input)?;
    writeln!(io::stdout(), "{}", dispatch(&input))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_soft_stereo_tone() {
        let pcm = tone();
        assert_eq!(pcm.len(), RATE);
        assert_eq!(&pcm[..4], &[0; 4]);
        assert_eq!(&pcm[pcm.len() - 4..], &[0; 4]);
        for f in pcm.chunks_exact(4) {
            assert_eq!(f[..2], f[2..]);
            assert!(i16::from_le_bytes([f[0], f[1]]).unsigned_abs() <= 2048);
        }
    }

    #[test]
    fn rms_uses_only_right_channel_and_removes_dc() {
        let mut pcm = Vec::new();
        for i in 0..RATE {
            pcm.extend_from_slice(&(if i % 2 == 0 { 16384i16 } else { -16384i16 }).to_le_bytes());
            pcm.extend_from_slice(&1000i16.to_le_bytes());
        }
        assert_eq!(level_percent(&pcm), Some(0));
        for (i, f) in pcm.chunks_exact_mut(4).enumerate() {
            f[2..].copy_from_slice(&(if i % 2 == 0 { 8192i16 } else { -8192i16 }).to_le_bytes());
        }
        assert_eq!(level_percent(&pcm), Some(25));
        assert_eq!(level_percent(&pcm[1..]), None);
    }

    #[test]
    fn rejects_noncanonical_requests() {
        for input in [b"mic".as_slice(), b"MIC\n", b"speaker", b"status\nextra"] {
            assert_eq!(dispatch(input), result("error"));
        }
    }
}
