//! Mezclador en tiempo real. `Engine::process` se llama desde el callback de audio:
//! no reserva memoria, no bloquea y no hace E/S.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::dsp::{db_to_lin, time_coef, Biquad, Compressor, Gate, Limiter};
use crate::presets;
use crate::{MAX_VOICES, NUM_PADS, STEM_CHANNELS, STEM_DUCK_CHANNEL};

const MAX_BLOCK: usize = 512;

/// Audio estéreo entrelazado ya convertido a la frecuencia del motor.
#[derive(Debug)]
pub struct AudioClip {
    pub name: String,
    pub sample_rate: u32,
    /// L R L R ...
    pub data: Vec<f32>,
}

impl AudioClip {
    pub fn frames(&self) -> usize {
        self.data.len() / 2
    }
    pub fn seconds(&self) -> f32 {
        self.frames() as f32 / self.sample_rate.max(1) as f32
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoiceParams {
    pub enabled: bool,
    /// Canal de entrada del dispositivo (0 = primero).
    pub input_channel: usize,
    /// Ganancia de entrada, antes de la cadena de efectos.
    pub gain_db: f32,
    /// Fader, después de la cadena de efectos.
    pub fader_db: f32,
    pub mute: bool,
    pub preset: usize,
}

impl Default for VoiceParams {
    fn default() -> Self {
        Self {
            enabled: false,
            input_channel: 0,
            gain_db: 0.0,
            fader_db: 0.0,
            mute: false,
            preset: 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MusicParams {
    pub volume_db: f32,
    pub duck_enabled: bool,
    /// Cuánto baja la música cuando hay voz (dB negativos).
    pub duck_depth_db: f32,
    /// Nivel de voz a partir del cual se activa el ducking.
    pub duck_threshold_db: f32,
    pub duck_release_ms: f32,
}

impl Default for MusicParams {
    fn default() -> Self {
        Self {
            volume_db: -6.0,
            duck_enabled: true,
            duck_depth_db: -14.0,
            duck_threshold_db: -38.0,
            duck_release_ms: 700.0,
        }
    }
}

pub enum Command {
    SetVoice(usize, VoiceParams),
    SetMusic(MusicParams),
    LoadMusic(Arc<AudioClip>),
    PlayMusic,
    StopMusic,
    SetPad {
        index: usize,
        clip: Option<Arc<AudioClip>>,
        gain_db: f32,
    },
    TriggerPad {
        index: usize,
        velocity: u8,
    },
    StopPads,
    SetSamplerVolume(f32),
    SetMaster {
        gain_db: f32,
        monitor_voices: bool,
    },
    PlayPreview(Arc<AudioClip>),
    StopPreview,
    StartRecording,
    StopRecording,
}

/// Disparo de pad que llega desde el hilo MIDI.
#[derive(Clone, Copy, Debug, Default)]
pub struct PadTrigger {
    pub index: u8,
    pub velocity: u8,
}

/// Evento de pad registrado durante la grabación (para el fichero MIDI).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PadEvent {
    pub frame: u64,
    pub pad: u8,
    pub velocity: u8,
}

/// Estado compartido motor -> interfaz (solo atómicos).
#[derive(Default)]
pub struct Meters {
    pub voice_peak: [AtomicU32; MAX_VOICES],
    pub voice_gr_db: [AtomicU32; MAX_VOICES],
    pub music_peak: AtomicU32,
    pub sampler_peak: AtomicU32,
    pub master_peak: [AtomicU32; 2],
    pub duck_gain: AtomicU32,
    pub limiter_gain: AtomicU32,
    pub music_playing: AtomicBool,
    pub music_pos_frames: AtomicU64,
    pub music_len_frames: AtomicU64,
    pub pad_playing: [AtomicBool; NUM_PADS],
    pub preview_playing: AtomicBool,
    pub recording: AtomicBool,
    pub rec_frames: AtomicU64,
    pub rec_dropped_frames: AtomicU64,
    pub input_underruns: AtomicU64,
}

impl Meters {
    #[inline]
    fn peak(a: &AtomicU32, v: f32) {
        // los f32 positivos conservan el orden al compararlos como bits
        a.fetch_max(v.abs().to_bits(), Ordering::Relaxed);
    }
    /// Lee un pico y lo reinicia (lo llama la interfaz).
    pub fn take_peak(a: &AtomicU32) -> f32 {
        f32::from_bits(a.swap(0, Ordering::Relaxed))
    }
    pub fn get_f32(a: &AtomicU32) -> f32 {
        f32::from_bits(a.load(Ordering::Relaxed))
    }
}

struct VoiceStrip {
    params: VoiceParams,
    in_gain: f32,
    fader: f32,
    fader_target: f32,
    hpf: Biquad,
    eq: [Biquad; 4],
    gate: Gate,
    comp: Compressor,
}

impl VoiceStrip {
    fn new(sr: f32) -> Self {
        let mut s = Self {
            params: VoiceParams::default(),
            in_gain: 1.0,
            fader: 0.0,
            fader_target: 0.0,
            hpf: Biquad::bypass(),
            eq: [Biquad::bypass(); 4],
            gate: Gate::new(sr),
            comp: Compressor::new(sr, 0.0, 1.0, 10.0, 100.0, 0.0),
        };
        s.apply(VoiceParams::default(), sr, true);
        s
    }

    fn apply(&mut self, p: VoiceParams, sr: f32, force: bool) {
        if force || p.preset != self.params.preset {
            let pr = presets::preset(p.preset);
            self.hpf.set_coefs_from(&if pr.hpf_hz > 25.0 {
                Biquad::highpass(sr, pr.hpf_hz, 0.707)
            } else {
                Biquad::bypass()
            });
            self.eq[0].set_coefs_from(&Biquad::low_shelf(sr, pr.low_shelf.0, pr.low_shelf.1));
            self.eq[1].set_coefs_from(&Biquad::peaking(sr, pr.mud.0, pr.mud.1, pr.mud.2));
            self.eq[2].set_coefs_from(&Biquad::peaking(
                sr,
                pr.presence.0,
                pr.presence.1,
                pr.presence.2,
            ));
            self.eq[3].set_coefs_from(&Biquad::high_shelf(sr, pr.high_shelf.0, pr.high_shelf.1));
            self.gate.threshold_db = pr.gate_threshold_db;
            self.gate.range_db = pr.gate_range_db;
            self.comp.set(
                sr,
                pr.comp_threshold_db,
                pr.comp_ratio,
                pr.comp_attack_ms,
                pr.comp_release_ms,
                pr.comp_makeup_db,
            );
        }
        self.in_gain = db_to_lin(p.gain_db);
        self.fader_target = if p.mute || !p.enabled {
            0.0
        } else {
            db_to_lin(p.fader_db)
        };
        self.params = p;
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let mut y = self.hpf.process(x * self.in_gain);
        y = self.gate.process(y);
        for b in self.eq.iter_mut() {
            y = b.process(y);
        }
        y = self.comp.process(y);
        self.fader += (self.fader_target - self.fader) * 0.002;
        y * self.fader
    }
}

struct Pad {
    clip: Option<Arc<AudioClip>>,
    gain: f32,
    pos: usize,
    vel: f32,
    playing: bool,
}

/// Extremos de las colas que usa el motor.
pub struct EngineIo {
    pub commands: rtrb::Consumer<Command>,
    pub triggers: rtrb::Consumer<PadTrigger>,
    pub stems: rtrb::Producer<f32>,
    pub events: rtrb::Producer<PadEvent>,
    pub trash: rtrb::Producer<Arc<AudioClip>>,
}

/// Extremos que se quedan fuera del hilo de audio.
pub struct EngineRemote {
    pub commands: rtrb::Producer<Command>,
    pub triggers: rtrb::Producer<PadTrigger>,
    pub stems: rtrb::Consumer<f32>,
    pub events: rtrb::Consumer<PadEvent>,
    pub trash: rtrb::Consumer<Arc<AudioClip>>,
    pub meters: Arc<Meters>,
    pub sample_rate: u32,
}

pub struct Engine {
    sr: f32,
    io: EngineIo,
    meters: Arc<Meters>,
    voices: Vec<VoiceStrip>,

    music: Option<Arc<AudioClip>>,
    music_params: MusicParams,
    music_vol: f32,
    music_pos: usize,
    music_playing: bool,
    music_fade: f32,
    music_fade_target: f32,
    music_fade_step: f32,

    duck_det: f32,
    duck_det_rel: f32,
    duck_gain: f32,
    duck_hold: u32,
    duck_att: f32,

    pads: Vec<Pad>,
    sampler_vol: f32,

    preview: Option<Arc<AudioClip>>,
    preview_pos: usize,

    master_gain: f32,
    monitor_voices: bool,
    limiter: Limiter,

    recording: bool,
    rec_frame: u64,
    stem_buf: Vec<f32>,
}

impl Engine {
    /// Crea el motor y los extremos remotos de sus colas.
    pub fn new(sample_rate: u32) -> (Engine, EngineRemote) {
        let sr = sample_rate as f32;
        let (cmd_tx, cmd_rx) = rtrb::RingBuffer::new(1024);
        let (trg_tx, trg_rx) = rtrb::RingBuffer::new(256);
        // ~6 s de margen para el hilo que escribe a disco
        let (stem_tx, stem_rx) = rtrb::RingBuffer::new(sample_rate as usize * STEM_CHANNELS * 6);
        let (ev_tx, ev_rx) = rtrb::RingBuffer::new(4096);
        let (trash_tx, trash_rx) = rtrb::RingBuffer::new(256);
        let meters = Arc::new(Meters::default());
        meters.duck_gain.store(1f32.to_bits(), Ordering::Relaxed);
        meters.limiter_gain.store(1f32.to_bits(), Ordering::Relaxed);

        let engine = Engine {
            sr,
            io: EngineIo {
                commands: cmd_rx,
                triggers: trg_rx,
                stems: stem_tx,
                events: ev_tx,
                trash: trash_tx,
            },
            meters: meters.clone(),
            voices: (0..MAX_VOICES).map(|_| VoiceStrip::new(sr)).collect(),
            music: None,
            music_params: MusicParams::default(),
            music_vol: db_to_lin(MusicParams::default().volume_db),
            music_pos: 0,
            music_playing: false,
            music_fade: 0.0,
            music_fade_target: 0.0,
            music_fade_step: 1.0 / (0.25 * sr),
            duck_det: 0.0,
            duck_det_rel: time_coef(40.0, sr),
            duck_gain: 1.0,
            duck_hold: 0,
            duck_att: time_coef(60.0, sr),
            pads: (0..NUM_PADS)
                .map(|_| Pad {
                    clip: None,
                    gain: 1.0,
                    pos: 0,
                    vel: 1.0,
                    playing: false,
                })
                .collect(),
            sampler_vol: 1.0,
            preview: None,
            preview_pos: 0,
            master_gain: 1.0,
            monitor_voices: false,
            limiter: Limiter::new(sr, -0.5, 1.5, 80.0),
            recording: false,
            rec_frame: 0,
            stem_buf: vec![0.0; MAX_BLOCK * STEM_CHANNELS],
        };
        let remote = EngineRemote {
            commands: cmd_tx,
            triggers: trg_tx,
            stems: stem_rx,
            events: ev_rx,
            trash: trash_rx,
            meters,
            sample_rate,
        };
        (engine, remote)
    }

    fn discard(&mut self, clip: Option<Arc<AudioClip>>) {
        // Liberar memoria en el hilo de audio no es deseable: se envía a la interfaz.
        if let Some(c) = clip {
            let _ = self.io.trash.push(c);
        }
    }

    fn trigger(&mut self, index: usize, velocity: u8) {
        if index >= NUM_PADS || self.pads[index].clip.is_none() {
            return;
        }
        let pad = &mut self.pads[index];
        pad.pos = 0;
        pad.vel = (velocity.max(1) as f32 / 127.0).sqrt();
        pad.playing = true;
        if self.recording {
            let _ = self.io.events.push(PadEvent {
                frame: self.rec_frame,
                pad: index as u8,
                velocity: velocity.max(1),
            });
        }
    }

    fn handle_commands(&mut self) {
        while let Ok(cmd) = self.io.commands.pop() {
            match cmd {
                Command::SetVoice(i, p) => {
                    if i < MAX_VOICES {
                        let sr = self.sr;
                        self.voices[i].apply(p, sr, false);
                    }
                }
                Command::SetMusic(p) => {
                    self.music_params = p;
                    self.music_vol = db_to_lin(p.volume_db);
                }
                Command::LoadMusic(clip) => {
                    let old = self.music.replace(clip);
                    self.discard(old);
                    self.music_pos = 0;
                    self.music_playing = false;
                    self.music_fade = 0.0;
                    self.music_fade_target = 0.0;
                }
                Command::PlayMusic => {
                    if self.music.is_some() {
                        if !self.music_playing {
                            self.music_fade = 0.0;
                        }
                        self.music_playing = true;
                        self.music_fade_target = 1.0;
                    }
                }
                Command::StopMusic => self.music_fade_target = 0.0,
                Command::SetPad {
                    index,
                    clip,
                    gain_db,
                } => {
                    if index < NUM_PADS {
                        self.pads[index].playing = false;
                        self.pads[index].gain = db_to_lin(gain_db);
                        let old = std::mem::replace(&mut self.pads[index].clip, clip);
                        self.discard(old);
                    }
                }
                Command::TriggerPad { index, velocity } => self.trigger(index, velocity),
                Command::StopPads => {
                    for p in self.pads.iter_mut() {
                        p.playing = false;
                    }
                }
                Command::SetSamplerVolume(db) => self.sampler_vol = db_to_lin(db),
                Command::SetMaster {
                    gain_db,
                    monitor_voices,
                } => {
                    self.master_gain = db_to_lin(gain_db);
                    self.monitor_voices = monitor_voices;
                }
                Command::PlayPreview(clip) => {
                    let old = self.preview.replace(clip);
                    self.discard(old);
                    self.preview_pos = 0;
                }
                Command::StopPreview => {
                    let old = self.preview.take();
                    self.discard(old);
                }
                Command::StartRecording => {
                    self.rec_frame = 0;
                    self.recording = true;
                    self.meters.rec_frames.store(0, Ordering::Relaxed);
                    self.meters.rec_dropped_frames.store(0, Ordering::Relaxed);
                    self.meters.recording.store(true, Ordering::Release);
                }
                Command::StopRecording => {
                    self.recording = false;
                    self.meters.recording.store(false, Ordering::Release);
                }
            }
        }
        while let Ok(t) = self.io.triggers.pop() {
            self.trigger(t.index as usize, t.velocity);
        }
    }

    /// Procesa un bloque. `input` y `output` están entrelazados.
    /// Si `input` trae menos frames que `output`, el resto se trata como silencio.
    pub fn process(
        &mut self,
        input: &[f32],
        in_channels: usize,
        output: &mut [f32],
        out_channels: usize,
    ) {
        self.handle_commands();
        if out_channels == 0 {
            return;
        }
        let frames = output.len() / out_channels;
        let in_frames = if in_channels == 0 {
            0
        } else {
            input.len() / in_channels
        };
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(MAX_BLOCK);
            let in_lo = done.min(in_frames);
            let in_hi = (done + n).min(in_frames);
            let inp = &input[in_lo * in_channels..in_hi * in_channels];
            let out = &mut output[done * out_channels..(done + n) * out_channels];
            self.process_block(inp, in_channels, out, out_channels, n);
            done += n;
        }
        if in_frames < frames {
            self.meters.input_underruns.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn process_block(
        &mut self,
        input: &[f32],
        in_ch: usize,
        output: &mut [f32],
        out_ch: usize,
        n: usize,
    ) {
        let in_frames = if in_ch == 0 { 0 } else { input.len() / in_ch };
        let mp = self.music_params;
        let duck_thr = db_to_lin(mp.duck_threshold_db);
        let duck_depth = db_to_lin(mp.duck_depth_db.min(0.0));
        let duck_rel = time_coef(mp.duck_release_ms.max(50.0), self.sr);
        let duck_hold_samples = (0.45 * self.sr) as u32;

        let mut vpk = [0f32; MAX_VOICES];
        let mut music_pk = 0f32;
        let mut sampler_pk = 0f32;
        let mut master_pk = [0f32; 2];

        for f in 0..n {
            let stem = &mut self.stem_buf[f * STEM_CHANNELS..(f + 1) * STEM_CHANNELS];

            // ---- voces
            let mut voice_sum = 0f32;
            let mut sidechain = 0f32;
            for (v, strip) in self.voices.iter_mut().enumerate() {
                let y = if strip.params.enabled {
                    let ch = strip.params.input_channel;
                    let x = if f < in_frames && ch < in_ch {
                        input[f * in_ch + ch]
                    } else {
                        0.0
                    };
                    strip.process(x)
                } else {
                    0.0
                };
                stem[v] = y;
                voice_sum += y;
                let a = y.abs();
                sidechain = sidechain.max(a);
                vpk[v] = vpk[v].max(a);
            }

            // ---- ducking
            self.duck_det = if sidechain > self.duck_det {
                sidechain
            } else {
                sidechain + self.duck_det_rel * (self.duck_det - sidechain)
            };
            let target = if !mp.duck_enabled {
                1.0
            } else if self.duck_det > duck_thr {
                self.duck_hold = duck_hold_samples;
                duck_depth
            } else if self.duck_hold > 0 {
                self.duck_hold -= 1;
                duck_depth
            } else {
                1.0
            };
            let c = if target < self.duck_gain {
                self.duck_att
            } else {
                duck_rel
            };
            self.duck_gain = target + c * (self.duck_gain - target);

            // ---- música
            let (mut ml, mut mr) = (0f32, 0f32);
            if self.music_playing {
                if let Some(clip) = &self.music {
                    let i = self.music_pos * 2;
                    if i + 1 < clip.data.len() {
                        if self.music_fade < self.music_fade_target {
                            self.music_fade =
                                (self.music_fade + self.music_fade_step * 8.0).min(1.0);
                        } else if self.music_fade > self.music_fade_target {
                            self.music_fade = (self.music_fade - self.music_fade_step).max(0.0);
                        }
                        // sin ducking: el stem grabado lleva la música limpia
                        let g = self.music_vol * self.music_fade;
                        ml = clip.data[i] * g;
                        mr = clip.data[i + 1] * g;
                        self.music_pos += 1;
                        if self.music_fade <= 0.0 && self.music_fade_target <= 0.0 {
                            self.music_playing = false; // parada con fundido completada (pausa)
                        }
                    } else {
                        self.music_playing = false;
                        self.music_pos = 0;
                    }
                } else {
                    self.music_playing = false;
                }
            }
            // El ducking se graba aparte como curva (editable al exportar); en directo se oye aplicado.
            stem[MAX_VOICES] = ml;
            stem[MAX_VOICES + 1] = mr;
            stem[STEM_DUCK_CHANNEL] = self.duck_gain;
            ml *= self.duck_gain;
            mr *= self.duck_gain;
            music_pk = music_pk.max(ml.abs()).max(mr.abs());

            // ---- sampler
            let (mut sl, mut sr_) = (0f32, 0f32);
            for pad in self.pads.iter_mut() {
                if !pad.playing {
                    continue;
                }
                match &pad.clip {
                    Some(clip) if pad.pos * 2 + 1 < clip.data.len() => {
                        let g = pad.gain * pad.vel;
                        sl += clip.data[pad.pos * 2] * g;
                        sr_ += clip.data[pad.pos * 2 + 1] * g;
                        pad.pos += 1;
                    }
                    _ => pad.playing = false,
                }
            }
            sl *= self.sampler_vol;
            sr_ *= self.sampler_vol;
            stem[MAX_VOICES + 2] = sl;
            stem[MAX_VOICES + 3] = sr_;
            sampler_pk = sampler_pk.max(sl.abs()).max(sr_.abs());

            // ---- preescucha de exportación (no se graba)
            let (mut pl, mut pr) = (0f32, 0f32);
            if let Some(clip) = &self.preview {
                let i = self.preview_pos * 2;
                if i + 1 < clip.data.len() {
                    pl = clip.data[i];
                    pr = clip.data[i + 1];
                    self.preview_pos += 1;
                }
            }

            // ---- máster (monitorización)
            let mon_voice = if self.monitor_voices { voice_sum } else { 0.0 };
            let l = (mon_voice + ml + sl + pl) * self.master_gain;
            let r = (mon_voice + mr + sr_ + pr) * self.master_gain;
            let (l, r) = self.limiter.process(l, r);
            master_pk[0] = master_pk[0].max(l.abs());
            master_pk[1] = master_pk[1].max(r.abs());

            let o = &mut output[f * out_ch..(f + 1) * out_ch];
            if out_ch == 1 {
                o[0] = 0.5 * (l + r);
            } else {
                o[0] = l;
                o[1] = r;
                for x in o[2..].iter_mut() {
                    *x = 0.0;
                }
            }
        }

        // ---- preescucha terminada
        if let Some(clip) = &self.preview {
            if self.preview_pos * 2 + 1 >= clip.data.len() {
                let old = self.preview.take();
                self.discard(old);
            }
        }

        // ---- grabación de stems
        if self.recording {
            let data = &self.stem_buf[..n * STEM_CHANNELS];
            if self.io.stems.push_entire_slice(data).is_ok() {
                self.rec_frame += n as u64;
                self.meters
                    .rec_frames
                    .store(self.rec_frame, Ordering::Relaxed);
            } else {
                // el disco no da abasto: se pierde el bloque pero el audio en directo sigue
                self.meters
                    .rec_dropped_frames
                    .fetch_add(n as u64, Ordering::Relaxed);
            }
        }

        // ---- medidores
        let m = &self.meters;
        for v in 0..MAX_VOICES {
            Meters::peak(&m.voice_peak[v], vpk[v]);
            m.voice_gr_db[v].store(
                self.voices[v].comp.gain_reduction_db().to_bits(),
                Ordering::Relaxed,
            );
        }
        Meters::peak(&m.music_peak, music_pk);
        Meters::peak(&m.sampler_peak, sampler_pk);
        Meters::peak(&m.master_peak[0], master_pk[0]);
        Meters::peak(&m.master_peak[1], master_pk[1]);
        m.duck_gain
            .store(self.duck_gain.to_bits(), Ordering::Relaxed);
        m.limiter_gain
            .store(self.limiter.take_min_gain().to_bits(), Ordering::Relaxed);
        m.music_playing.store(self.music_playing, Ordering::Relaxed);
        m.music_pos_frames
            .store(self.music_pos as u64, Ordering::Relaxed);
        m.music_len_frames.store(
            self.music.as_ref().map(|c| c.frames() as u64).unwrap_or(0),
            Ordering::Relaxed,
        );
        for (i, p) in self.pads.iter().enumerate() {
            m.pad_playing[i].store(p.playing, Ordering::Relaxed);
        }
        m.preview_playing
            .store(self.preview.is_some(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone_clip(sr: u32, secs: f32, amp: f32) -> Arc<AudioClip> {
        let n = (sr as f32 * secs) as usize;
        let mut data = Vec::with_capacity(n * 2);
        for i in 0..n {
            let s = (i as f32 * 2.0 * std::f32::consts::PI * 220.0 / sr as f32).sin() * amp;
            data.push(s);
            data.push(s);
        }
        Arc::new(AudioClip {
            name: "tone".into(),
            sample_rate: sr,
            data,
        })
    }

    fn run(engine: &mut Engine, input: &[f32], in_ch: usize, frames: usize) -> Vec<f32> {
        let mut out = vec![0f32; frames * 2];
        engine.process(input, in_ch, &mut out, 2);
        out
    }

    #[test]
    fn music_is_ducked_by_voice() {
        let sr = 48000;
        let (mut e, mut r) = Engine::new(sr);
        r.commands
            .push(Command::LoadMusic(tone_clip(sr, 10.0, 0.5)))
            .ok()
            .unwrap();
        r.commands
            .push(Command::SetMusic(MusicParams {
                volume_db: 0.0,
                ..Default::default()
            }))
            .ok()
            .unwrap();
        r.commands.push(Command::PlayMusic).ok().unwrap();
        r.commands
            .push(Command::SetVoice(
                0,
                VoiceParams {
                    enabled: true,
                    input_channel: 1,
                    preset: 0,
                    ..Default::default()
                },
            ))
            .ok()
            .unwrap();

        // 1 s sin voz
        let silence = vec![0f32; sr as usize * 2];
        let out = run(&mut e, &silence, 2, sr as usize);
        let peak_free = out[out.len() / 2..]
            .iter()
            .fold(0f32, |a, &b| a.max(b.abs()));
        assert!(peak_free > 0.4, "música sin ducking: {peak_free}");

        // 1 s con voz en el canal 1
        let mut voice = vec![0f32; sr as usize * 2];
        for i in 0..sr as usize {
            voice[i * 2 + 1] = (i as f32 * 0.03).sin() * 0.3;
        }
        let out = run(&mut e, &voice, 2, sr as usize);
        // no se monitoriza la voz: la salida es solo música atenuada
        let peak_duck = out[out.len() / 2..]
            .iter()
            .fold(0f32, |a, &b| a.max(b.abs()));
        assert!(
            peak_duck < peak_free * 0.3,
            "música con ducking: {peak_duck}"
        );
        assert!(Meters::get_f32(&r.meters.duck_gain) < 0.3);
    }

    #[test]
    fn recording_writes_stems_and_pad_events() {
        let sr = 48000;
        let (mut e, mut r) = Engine::new(sr);
        r.commands
            .push(Command::SetPad {
                index: 3,
                clip: Some(tone_clip(sr, 0.2, 0.5)),
                gain_db: 0.0,
            })
            .ok()
            .unwrap();
        r.commands.push(Command::StartRecording).ok().unwrap();
        let input = vec![0f32; 4800];
        run(&mut e, &input, 1, 4800);
        r.triggers
            .push(PadTrigger {
                index: 3,
                velocity: 100,
            })
            .unwrap();
        run(&mut e, &input, 1, 4800);
        r.commands.push(Command::StopRecording).ok().unwrap();
        run(&mut e, &input, 1, 480);

        assert_eq!(r.meters.rec_frames.load(Ordering::Relaxed), 9600);
        assert_eq!(r.stems.slots(), 9600 * STEM_CHANNELS);
        let ev = r.events.pop().unwrap();
        assert_eq!(
            ev,
            PadEvent {
                frame: 4800,
                pad: 3,
                velocity: 100
            }
        );
        // el stem del sampler tiene señal solo en la segunda mitad
        let mut all = vec![0f32; 9600 * STEM_CHANNELS];
        r.stems.pop_entire_slice(&mut all).unwrap();
        let first: f32 = (0..4800)
            .map(|f| all[f * STEM_CHANNELS + MAX_VOICES + 2].abs())
            .sum();
        let second: f32 = (4800..9600)
            .map(|f| all[f * STEM_CHANNELS + MAX_VOICES + 2].abs())
            .sum();
        assert_eq!(first, 0.0);
        assert!(second > 10.0);
    }
}
