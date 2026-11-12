//! Modelo de sesión. Se guarda como `session.json` dentro de la carpeta de la sesión:
//!
//! ```text
//! MiPodcast/
//!   session.json
//!   takes/20261111-161700/{voice1.wav, music.wav, sampler.wav, pads.mid, mix.json, take.json}
//!   export/episodio/{episodio - mixdown.wav, episodio - voz 1 (Ana).wav, episodio - midi.mid}
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::engine::{MusicParams, VoiceParams};
use crate::{MAX_VOICES, NUM_PADS};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceCfg {
    pub name: String,
    pub enabled: bool,
    pub input_channel: usize,
    pub gain_db: f32,
    pub fader_db: f32,
    pub mute: bool,
    pub preset: usize,
}

impl Default for VoiceCfg {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: false,
            input_channel: 0,
            gain_db: 0.0,
            fader_db: 0.0,
            mute: false,
            preset: 1,
        }
    }
}

impl VoiceCfg {
    pub fn params(&self) -> VoiceParams {
        VoiceParams {
            enabled: self.enabled,
            input_channel: self.input_channel,
            gain_db: self.gain_db,
            fader_db: self.fader_db,
            mute: self.mute,
            preset: self.preset,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MusicCfg {
    pub volume_db: f32,
    pub duck_enabled: bool,
    pub duck_depth_db: f32,
    pub duck_threshold_db: f32,
    pub duck_release_ms: f32,
    /// Lista de reproducción de la sesión.
    pub setlist: Vec<PathBuf>,
}

impl Default for MusicCfg {
    fn default() -> Self {
        let p = MusicParams::default();
        Self {
            volume_db: p.volume_db,
            duck_enabled: p.duck_enabled,
            duck_depth_db: p.duck_depth_db,
            duck_threshold_db: p.duck_threshold_db,
            duck_release_ms: p.duck_release_ms,
            setlist: Vec::new(),
        }
    }
}

impl MusicCfg {
    pub fn params(&self) -> MusicParams {
        MusicParams {
            volume_db: self.volume_db,
            duck_enabled: self.duck_enabled,
            duck_depth_db: self.duck_depth_db,
            duck_threshold_db: self.duck_threshold_db,
            duck_release_ms: self.duck_release_ms,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PadCfg {
    pub path: Option<PathBuf>,
    pub gain_db: f32,
    /// Nota MIDI que dispara este pad. `None` = la predeterminada (nota base + nº de pad).
    pub midi_note: Option<u8>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioCfg {
    pub host: String,
    pub input_device: String,
    pub output_device: String,
    /// 0 = tamaño por defecto del driver.
    pub buffer_frames: u32,
    pub midi_port: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub name: String,
    /// Cuántas pistas de voz tiene la sesión (0..=MAX_VOICES). Las demás no se graban.
    pub num_voices: usize,
    pub voices: Vec<VoiceCfg>,
    pub music: MusicCfg,
    pub pads: Vec<PadCfg>,
    pub sampler_volume_db: f32,
    /// Nota MIDI que dispara el pad 1 (36 = C1, estándar de pads).
    pub midi_base_note: u8,
    pub master_gain_db: f32,
    pub monitor_voices: bool,
    pub audio: AudioCfg,
}

impl Default for Session {
    fn default() -> Self {
        let voices: Vec<VoiceCfg> = (0..MAX_VOICES)
            .map(|i| VoiceCfg {
                name: format!("Voz {}", i + 1),
                input_channel: i,
                ..Default::default()
            })
            .collect();
        let mut s = Self {
            name: "Nueva sesión".into(),
            num_voices: 2,
            voices,
            music: MusicCfg::default(),
            pads: vec![PadCfg::default(); NUM_PADS],
            sampler_volume_db: -3.0,
            midi_base_note: 36,
            master_gain_db: 0.0,
            monitor_voices: false,
            audio: AudioCfg::default(),
        };
        s.normalize();
        s
    }
}

impl Session {
    /// Garantiza tamaños fijos tras leer un JSON editado a mano o de otra versión.
    pub fn normalize(&mut self) {
        self.voices.resize_with(MAX_VOICES, VoiceCfg::default);
        self.pads.resize_with(NUM_PADS, PadCfg::default);
        self.num_voices = self.num_voices.min(MAX_VOICES);
        for (i, v) in self.voices.iter_mut().enumerate() {
            if v.name.trim().is_empty() {
                v.name = format!("Voz {}", i + 1);
            }
            v.preset = v.preset.min(crate::presets::PRESETS.len() - 1);
            // una pista está activa si entra en el número de voces de la sesión
            v.enabled = i < self.num_voices;
        }
        self.midi_base_note = self.midi_base_note.min(127 - NUM_PADS as u8);
        for p in self.pads.iter_mut() {
            p.midi_note = p.midi_note.map(|n| n.min(127));
        }
    }

    /// Cambia el número de voces (limitado a 0..=MAX_VOICES).
    pub fn set_num_voices(&mut self, n: usize) {
        self.num_voices = n.min(MAX_VOICES);
        self.normalize();
    }

    /// Nota MIDI efectiva de un pad: la asignada o `base + n.º de pad`.
    pub fn pad_note(&self, pad: usize) -> u8 {
        self.pads
            .get(pad)
            .and_then(|p| p.midi_note)
            .unwrap_or_else(|| (self.midi_base_note as usize + pad).min(127) as u8)
    }

    pub fn pad_notes(&self) -> Vec<u8> {
        (0..NUM_PADS).map(|i| self.pad_note(i)).collect()
    }

    pub fn load(dir: &Path) -> Result<Session> {
        let path = dir.join("session.json");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("no se puede leer {}", path.display()))?;
        let mut s: Session = serde_json::from_str(&text)
            .with_context(|| format!("JSON no válido en {}", path.display()))?;
        // Sesiones anteriores no tenían `num_voices`: se conservan todas las voces que usaban.
        let has_count = serde_json::from_str::<serde_json::Value>(&text)
            .map(|v| v.get("num_voices").is_some())
            .unwrap_or(false);
        if !has_count {
            s.num_voices = s
                .voices
                .iter()
                .rposition(|v| v.enabled)
                .map(|i| i + 1)
                .unwrap_or(0);
        }
        s.normalize();
        Ok(s)
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir.join("takes"))?;
        std::fs::create_dir_all(dir.join("export"))?;
        let path = dir.join("session.json");
        std::fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("no se puede escribir {}", path.display()))?;
        Ok(())
    }
}

/// Lista las tomas grabadas de una sesión (más reciente primero).
pub fn list_takes(session_dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(session_dir.join("takes"))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.join("take.json").is_file())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v.reverse();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_tolerant_load() {
        let dir = std::env::temp_dir().join(format!("podcaz-session-test-{}", std::process::id()));
        let mut s = Session::default();
        s.name = "Prueba".into();
        s.music.setlist.push(PathBuf::from("C:/musica/intro.mp3"));
        s.save(&dir).unwrap();
        let l = Session::load(&dir).unwrap();
        assert_eq!(l.name, "Prueba");
        assert_eq!(l.voices.len(), MAX_VOICES);
        assert_eq!(l.pads.len(), NUM_PADS);
        // un JSON mínimo también carga
        std::fs::write(
            dir.join("session.json"),
            r#"{"name":"x","voices":[{"enabled":true}]}"#,
        )
        .unwrap();
        let l = Session::load(&dir).unwrap();
        assert_eq!(l.voices.len(), MAX_VOICES);
        assert_eq!(l.voices[0].name, "Voz 1");
        // sesión anterior sin `num_voices`: se deduce de las voces activas
        assert_eq!(l.num_voices, 1);
        assert!(l.voices[0].enabled && !l.voices[1].enabled);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn voice_count_controls_enabled_tracks() {
        let mut s = Session::default();
        assert_eq!(s.num_voices, 2);
        s.set_num_voices(4);
        assert!(s.voices.iter().all(|v| v.enabled));
        s.set_num_voices(0);
        assert!(s.voices.iter().all(|v| !v.enabled));
        s.set_num_voices(99);
        assert_eq!(s.num_voices, MAX_VOICES);
        // y se conserva al guardar y cargar
        let dir = std::env::temp_dir().join(format!("podcaz-session-n-{}", std::process::id()));
        s.set_num_voices(3);
        s.save(&dir).unwrap();
        assert_eq!(Session::load(&dir).unwrap().num_voices, 3);
        s.set_num_voices(0);
        s.save(&dir).unwrap();
        assert_eq!(Session::load(&dir).unwrap().num_voices, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pad_notes_default_and_override() {
        let mut s = Session::default();
        assert_eq!(s.pad_note(0), 36);
        assert_eq!(s.pad_note(15), 51);
        s.pads[3].midi_note = Some(60);
        s.midi_base_note = 48;
        assert_eq!(s.pad_note(3), 60);
        assert_eq!(s.pad_note(4), 52);
        let notes = s.pad_notes();
        assert_eq!(notes.len(), NUM_PADS);
        assert_eq!(notes[3], 60);
    }
}
