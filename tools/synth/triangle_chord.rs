use std::env;
use std::f64::consts::PI;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::process::Command;

const SAMPLE_RATE: u32 = 48_000;
const DURATION_SECS: f64 = 2.0;
const AMPLITUDE: f64 = 0.18;
const ATTACK_SECS: f64 = 0.02;
const RELEASE_SECS: f64 = 0.3;
const OUT_PATH: &str = "/tmp/triangle_chord.wav";

fn triangle(phase: f64) -> f64 {
    // Normalize phase to [0, 1)
    let t = (phase / (2.0 * PI)).rem_euclid(1.0);
    if t < 0.5 {
        4.0 * t - 1.0
    } else {
        3.0 - 4.0 * t
    }
}

fn envelope(t: f64) -> f64 {
    if t < ATTACK_SECS {
        t / ATTACK_SECS
    } else if t > DURATION_SECS - RELEASE_SECS {
        (DURATION_SECS - t) / RELEASE_SECS
    } else {
        1.0
    }
}

fn write_le_u16(w: &mut impl Write, v: u16) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn write_le_u32(w: &mut impl Write, v: u32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn write_wav(freqs: &[f64]) -> io::Result<()> {
    let num_samples = (DURATION_SECS * SAMPLE_RATE as f64) as u32;
    let data_size = num_samples * 2; // 16-bit mono
    let file = File::create(OUT_PATH)?;
    let mut w = BufWriter::new(file);

    // WAV header
    w.write_all(b"RIFF")?;
    write_le_u32(&mut w, 36 + data_size)?;
    w.write_all(b"WAVE")?;

    // fmt chunk
    w.write_all(b"fmt ")?;
    write_le_u32(&mut w, 16)?;          // chunk size
    write_le_u16(&mut w, 1)?;           // PCM
    write_le_u16(&mut w, 1)?;           // mono
    write_le_u32(&mut w, SAMPLE_RATE)?; // sample rate
    write_le_u32(&mut w, SAMPLE_RATE * 2)?; // byte rate
    write_le_u16(&mut w, 2)?;           // block align
    write_le_u16(&mut w, 16)?;          // bits per sample

    // data chunk
    w.write_all(b"data")?;
    write_le_u32(&mut w, data_size)?;

    let mut phases = vec![0.0_f64; freqs.len()];
    let n_voices = freqs.len().max(1) as f64;

    for i in 0..num_samples {
        let t = i as f64 / SAMPLE_RATE as f64;
        let mut sample: f64 = 0.0;

        for (vi, &freq) in freqs.iter().enumerate() {
            phases[vi] += 2.0 * PI * freq / SAMPLE_RATE as f64;
            sample += triangle(phases[vi]);
        }

        sample /= n_voices;
        sample *= envelope(t);

        let value = (AMPLITUDE * 32767.0 * sample) as i16;
        w.write_all(&value.to_le_bytes())?;
    }

    w.flush()?;
    Ok(())
}

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 3 {
        eprintln!("Usage: {} FUND RATIO1 [RATIO2 ...]", args[0]);
        eprintln!("  Plays triangle waves at FUND*RATIO for each ratio.");
        eprintln!("  Example: {} 220 1 1.25 1.5", args[0]);
        std::process::exit(1);
    }

    let fund: f64 = args[1].parse().unwrap_or_else(|_| {
        eprintln!("Bad fundamental: {}", args[1]);
        std::process::exit(1);
    });

    let freqs: Vec<f64> = args[2..]
        .iter()
        .map(|s| {
            let ratio: f64 = s.parse().unwrap_or_else(|_| {
                eprintln!("Bad ratio: {}", s);
                std::process::exit(1);
            });
            fund * ratio
        })
        .collect();

    eprintln!(
        "Frequencies: {:?} Hz",
        freqs.iter().map(|f| (*f * 100.0).round() / 100.0).collect::<Vec<_>>()
    );

    write_wav(&freqs).unwrap_or_else(|e| {
        eprintln!("Failed to write {}: {}", OUT_PATH, e);
        std::process::exit(1);
    });

    eprintln!("Written to {}", OUT_PATH);

    // Try pw-play first, fall back to aplay
    let status = Command::new("pw-play")
        .arg(OUT_PATH)
        .status()
        .or_else(|_| Command::new("aplay").arg(OUT_PATH).status());

    match status {
        Ok(s) if s.success() => {}
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("Could not play audio: {}", e);
            std::process::exit(1);
        }
    }
}
