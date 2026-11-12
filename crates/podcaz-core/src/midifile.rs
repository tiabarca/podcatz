//! Escritura de un fichero MIDI estándar (SMF formato 0) con los disparos de pads.

use std::io::Write;
use std::path::Path;

use crate::engine::PadEvent;

const PPQ: u16 = 480;
const TEMPO_US_PER_QN: u32 = 500_000; // 120 bpm -> 960 ticks por segundo
const NOTE_LEN_TICKS: u64 = 96; // 100 ms

fn varlen(mut v: u64, out: &mut Vec<u8>) {
    let mut bytes = [0u8; 10];
    let mut n = 0;
    loop {
        bytes[n] = (v & 0x7f) as u8;
        n += 1;
        v >>= 7;
        if v == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        out.push(if i > 0 { bytes[i] | 0x80 } else { bytes[i] });
    }
}

/// Construye los bytes del fichero. El pad `p` se escribe con la nota `pad_notes[p]`.
pub fn build_smf(events: &[PadEvent], sample_rate: u32, pad_notes: &[u8]) -> Vec<u8> {
    let ticks_per_sec = PPQ as f64 * 1_000_000.0 / TEMPO_US_PER_QN as f64;
    // (tick, es_note_on, nota, velocidad)
    let mut msgs: Vec<(u64, bool, u8, u8)> = Vec::with_capacity(events.len() * 2);
    for e in events {
        let tick = (e.frame as f64 / sample_rate.max(1) as f64 * ticks_per_sec).round() as u64;
        let note = pad_notes
            .get(e.pad as usize)
            .copied()
            .unwrap_or(36u8.saturating_add(e.pad))
            .min(127);
        msgs.push((tick, true, note, e.velocity.clamp(1, 127)));
        msgs.push((tick + NOTE_LEN_TICKS, false, note, 0));
    }
    // a igualdad de tick, los note-off primero
    msgs.sort_by_key(|m| (m.0, m.1));

    let mut track = Vec::new();
    // tempo
    track.extend_from_slice(&[0x00, 0xFF, 0x51, 0x03]);
    track.extend_from_slice(&TEMPO_US_PER_QN.to_be_bytes()[1..]);
    // nombre de pista
    let name = b"PODCAZ pads";
    track.extend_from_slice(&[0x00, 0xFF, 0x03, name.len() as u8]);
    track.extend_from_slice(name);
    let mut last = 0u64;
    for (tick, on, note, vel) in msgs {
        varlen(tick - last, &mut track);
        last = tick;
        track.extend_from_slice(&[if on { 0x90 } else { 0x80 }, note, vel]);
    }
    track.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);

    let mut out = Vec::with_capacity(track.len() + 22);
    out.extend_from_slice(b"MThd");
    out.extend_from_slice(&6u32.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // formato 0
    out.extend_from_slice(&1u16.to_be_bytes()); // 1 pista
    out.extend_from_slice(&PPQ.to_be_bytes());
    out.extend_from_slice(b"MTrk");
    out.extend_from_slice(&(track.len() as u32).to_be_bytes());
    out.extend_from_slice(&track);
    out
}

pub fn write_smf(
    path: &Path,
    events: &[PadEvent],
    sample_rate: u32,
    pad_notes: &[u8],
) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(&build_smf(events, sample_rate, pad_notes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varlen_encoding() {
        let mut v = Vec::new();
        varlen(0, &mut v);
        varlen(0x7f, &mut v);
        varlen(0x80, &mut v);
        varlen(0x3fff, &mut v);
        varlen(0x4000, &mut v);
        assert_eq!(
            v,
            vec![0x00, 0x7f, 0x81, 0x00, 0xff, 0x7f, 0x81, 0x80, 0x00]
        );
    }

    #[test]
    fn smf_structure() {
        let ev = [
            PadEvent {
                frame: 48000,
                pad: 0,
                velocity: 100,
            },
            PadEvent {
                frame: 96000,
                pad: 5,
                velocity: 64,
            },
        ];
        let b = build_smf(&ev, 48000, &crate::default_pad_notes(36));
        assert_eq!(&b[0..4], b"MThd");
        assert_eq!(&b[14..18], b"MTrk");
        let len = u32::from_be_bytes([b[18], b[19], b[20], b[21]]) as usize;
        assert_eq!(b.len(), 22 + len);
        assert_eq!(&b[b.len() - 3..], &[0xFF, 0x2F, 0x00]);
        // primera nota: delta 960 ticks (1 s) = 0x87 0x40, note on 36 vel 100
        let pos = b.windows(5).position(|w| w == [0x87, 0x40, 0x90, 36, 100]);
        assert!(pos.is_some());
        // segunda nota: pad 5 -> nota 41
        assert!(b.windows(3).any(|w| w == [0x90, 41, 64]));
    }

    #[test]
    fn custom_pad_mapping_is_written() {
        let mut notes = crate::default_pad_notes(36).to_vec();
        notes[5] = 60; // pad 5 mapeado a mano
        let ev = [PadEvent {
            frame: 0,
            pad: 5,
            velocity: 80,
        }];
        let b = build_smf(&ev, 48000, &notes);
        assert!(b.windows(3).any(|w| w == [0x90, 60, 80]));
        assert!(!b.windows(3).any(|w| w == [0x90, 41, 80]));
    }
}
