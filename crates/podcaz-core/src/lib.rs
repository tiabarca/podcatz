//! PODCAZ core: todo lo que no depende del hardware.
//!
//! - `dsp`      filtros, puerta, compresor, limitador
//! - `presets`  cadenas de voz prefabricadas (EQ + dinámica)
//! - `engine`   mezclador en tiempo real (voces, música con ducking, sampler)
//! - `recorder` hilo que vuelca los stems a disco
//! - `decode`   carga de ficheros de audio (mp3/wav/flac/ogg/m4a) y remuestreo
//! - `master`   masterizado (loudness + limitador)
//! - `export`   mezcla offline de una toma -> WAV / MP3
//! - `midifile` escritura de ficheros MIDI estándar
//! - `session`  modelo de sesión (JSON)

//! - `automation` envolventes de volumen editables por pista (incluye el ducking grabado)

pub mod automation;
pub mod decode;
pub mod dsp;
pub mod engine;
pub mod export;
pub mod master;
pub mod midifile;
pub mod presets;
pub mod recorder;
pub mod session;

pub const MAX_VOICES: usize = 4;
pub const NUM_PADS: usize = 16;
/// Canales del flujo de stems: voz1..voz4, música L/R (sin ducking), sampler L/R y
/// la ganancia de ducking que se aplicó a la música en directo.
pub const STEM_CHANNELS: usize = MAX_VOICES + 5;
/// Posición del canal de ganancia de ducking dentro de un frame de stems.
pub const STEM_DUCK_CHANNEL: usize = MAX_VOICES + 4;

/// Nota MIDI de cada pad cuando no hay una asignada a mano: `base`, `base+1`, …
pub fn default_pad_notes(base: u8) -> [u8; NUM_PADS] {
    std::array::from_fn(|i| (base as usize + i).min(127) as u8)
}
