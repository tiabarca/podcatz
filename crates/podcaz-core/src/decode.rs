//! Carga de ficheros de audio (mp3, wav, flac, ogg, m4a/aac, aiff) a estéreo `f32`
//! en la frecuencia de muestreo del motor.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use crate::engine::AudioClip;

pub const AUDIO_EXTENSIONS: &[&str] = &["mp3", "wav", "flac", "ogg", "m4a", "aac", "aiff", "aif"];

/// Decodifica el fichero completo. Devuelve (estéreo entrelazado, frecuencia original).
pub fn decode_file(path: &Path) -> Result<(Vec<f32>, u32)> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("no se puede abrir {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| anyhow!("formato no reconocido ({e})"))?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow!("el fichero no tiene pista de audio"))?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| anyhow!("pista de audio sin parámetros de códec"))?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map_err(|e| anyhow!("códec no soportado ({e})"))?;

    let mut out: Vec<f32> = Vec::new();
    let mut tmp: Vec<f32> = Vec::new();
    let mut rate = 0u32;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(SymError::IoError(_)) => break, // fin de fichero truncado
            Err(e) => {
                if out.is_empty() {
                    return Err(anyhow!("error leyendo el fichero ({e})"));
                }
                break;
            }
        };
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                let ch = buf.spec().channels().count();
                if ch == 0 {
                    continue;
                }
                rate = buf.spec().rate();
                tmp.resize(buf.samples_interleaved(), 0.0);
                buf.copy_to_slice_interleaved(&mut tmp[..]);
                if ch == 1 {
                    for &s in &tmp {
                        out.push(s);
                        out.push(s);
                    }
                } else {
                    for fr in tmp.chunks_exact(ch) {
                        out.push(fr[0]);
                        out.push(fr[1]);
                    }
                }
            }
            Err(SymError::DecodeError(_)) | Err(SymError::IoError(_)) => continue,
            Err(e) => {
                if out.is_empty() {
                    return Err(anyhow!("error decodificando ({e})"));
                }
                break;
            }
        }
    }
    if out.is_empty() || rate == 0 {
        return Err(anyhow!("el fichero no contiene audio"));
    }
    Ok((out, rate))
}

/// Carga un fichero listo para el motor.
pub fn load_clip(path: &Path, engine_rate: u32) -> Result<AudioClip> {
    let (data, rate) = decode_file(path)?;
    let data = if rate == engine_rate {
        data
    } else {
        resample_stereo(&data, rate, engine_rate)
    };
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(AudioClip {
        name,
        sample_rate: engine_rate,
        data,
    })
}

/// Remuestreo offline con sinc enventanado (Blackman-Harris), estéreo entrelazado.
pub fn resample_stereo(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.len() < 2 || from == 0 || to == 0 {
        return input.to_vec();
    }
    const PHASES: usize = 256;
    let in_frames = input.len() / 2;
    let ratio = to as f64 / from as f64;
    // al bajar de frecuencia hay que filtrar por debajo del nuevo Nyquist
    let fc = ratio.min(1.0) * 0.96;
    let half = (16.0 / ratio.min(1.0)).ceil() as isize;
    let taps = (2 * half) as usize;

    // tabla[fase][tap], tap k corresponde al desplazamiento (k - half + 1) - frac
    let mut table = vec![0f32; (PHASES + 1) * taps];
    for p in 0..=PHASES {
        let frac = p as f64 / PHASES as f64;
        let row = &mut table[p * taps..(p + 1) * taps];
        let mut sum = 0f64;
        for (k, w) in row.iter_mut().enumerate() {
            let x = (k as isize - half + 1) as f64 - frac;
            let sinc = if x.abs() < 1e-9 {
                1.0
            } else {
                (std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * fc * x)
            };
            let u = (x / half as f64 + 1.0) * 0.5; // 0..1
            let win = if (0.0..=1.0).contains(&u) {
                let a = 2.0 * std::f64::consts::PI * u;
                0.35875 - 0.48829 * a.cos() + 0.14128 * (2.0 * a).cos() - 0.01168 * (3.0 * a).cos()
            } else {
                0.0
            };
            let v = fc * sinc * win;
            *w = v as f32;
            sum += v;
        }
        if sum.abs() > 1e-9 {
            for w in row.iter_mut() {
                *w = (*w as f64 / sum) as f32;
            }
        }
    }

    let out_frames = (in_frames as f64 * ratio).floor() as usize;
    let mut out = vec![0f32; out_frames * 2];
    for (j, o) in out.chunks_exact_mut(2).enumerate() {
        let t = j as f64 / ratio;
        let base = t.floor();
        let p = ((t - base) * PHASES as f64).round() as usize;
        let row = &table[p * taps..(p + 1) * taps];
        let start = base as isize - half + 1;
        let (mut l, mut r) = (0f32, 0f32);
        if start >= 0 && (start as usize + taps) <= in_frames {
            let s = &input[start as usize * 2..(start as usize + taps) * 2];
            for (k, &w) in row.iter().enumerate() {
                l += s[2 * k] * w;
                r += s[2 * k + 1] * w;
            }
        } else {
            for (k, &w) in row.iter().enumerate() {
                let i = start + k as isize;
                if i >= 0 && (i as usize) < in_frames {
                    l += input[i as usize * 2] * w;
                    r += input[i as usize * 2 + 1] * w;
                }
            }
        }
        o[0] = l;
        o[1] = r;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, freq: f32, secs: f32) -> Vec<f32> {
        let n = (rate as f32 * secs) as usize;
        let mut v = Vec::with_capacity(n * 2);
        for i in 0..n {
            let s = (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin() * 0.5;
            v.push(s);
            v.push(-s);
        }
        v
    }

    #[test]
    fn resample_preserves_tone() {
        for (from, to) in [
            (44100u32, 48000u32),
            (48000, 44100),
            (96000, 48000),
            (22050, 48000),
        ] {
            let input = sine(from, 1000.0, 0.5);
            let out = resample_stereo(&input, from, to);
            let frames = out.len() / 2;
            assert!(
                (frames as i64 - (to as i64 / 2)).abs() <= 2,
                "{from}->{to}: {frames}"
            );
            // comparar con el seno ideal en la zona central
            let mut err = 0f32;
            for j in frames / 4..frames * 3 / 4 {
                let ideal =
                    (2.0 * std::f32::consts::PI * 1000.0 * j as f32 / to as f32).sin() * 0.5;
                err = err
                    .max((out[j * 2] - ideal).abs())
                    .max((out[j * 2 + 1] + ideal).abs());
            }
            assert!(err < 0.01, "{from}->{to}: error {err}");
        }
    }

    #[test]
    fn decode_wav_roundtrip() {
        let path =
            std::env::temp_dir().join(format!("podcaz-decode-test-{}.wav", std::process::id()));
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..44100 {
            w.write_sample(((i as f32 * 0.05).sin() * 16000.0) as i16)
                .unwrap();
        }
        w.finalize().unwrap();
        let clip = load_clip(&path, 48000).unwrap();
        assert_eq!(clip.sample_rate, 48000);
        assert!((clip.frames() as i64 - 48000).abs() <= 2);
        let peak = clip.data.iter().fold(0f32, |a, &b| a.max(b.abs()));
        assert!(peak > 0.45 && peak < 0.52, "pico {peak}");
        std::fs::remove_file(&path).ok();
    }
}
