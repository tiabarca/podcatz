//! Hilo de grabación: saca los stems de la cola del motor y los escribe a disco.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::automation::{Envelope, MixAutomation, STEM_MUSIC};
use crate::engine::{Meters, PadEvent};
use crate::{default_pad_notes, midifile, MAX_VOICES, STEM_CHANNELS, STEM_DUCK_CHANNEL};

/// Eventos de pads de la toma (para volver a generar el MIDI recortado al exportar).
pub const PAD_EVENTS_FILE: &str = "pad_events.json";

/// Resolución de la curva de ducking guardada: 100 puntos por segundo.
const DUCK_STEPS_PER_SEC: u32 = 100;
/// Error máximo (dB) al convertir la curva de ducking en puntos editables.
const DUCK_TOLERANCE_DB: f32 = 0.75;

/// Metadatos de una toma (`take.json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TakeInfo {
    pub sample_rate: u32,
    pub frames: u64,
    /// Voces grabadas (existe `voiceN.wav`).
    pub voices: Vec<bool>,
    pub voice_names: Vec<String>,
    pub dropped_frames: u64,
    pub pad_events: usize,
    pub midi_base_note: u8,
    /// Nota MIDI de cada pad al grabar (vacío en tomas antiguas: `midi_base_note + pad`).
    pub pad_notes: Vec<u8>,
    /// `music.wav` está sin ducking y el ducking va como envolvente en `mix.json`.
    pub music_unducked: bool,
}

impl TakeInfo {
    pub fn load(take_dir: &Path) -> Result<TakeInfo> {
        let p = take_dir.join("take.json");
        let t: TakeInfo = serde_json::from_str(
            &std::fs::read_to_string(&p)
                .with_context(|| format!("no se puede leer {}", p.display()))?,
        )?;
        if t.sample_rate == 0 {
            return Err(anyhow!("take.json sin frecuencia de muestreo"));
        }
        Ok(t)
    }
    pub fn seconds(&self) -> f64 {
        self.frames as f64 / self.sample_rate.max(1) as f64
    }

    /// Nota MIDI de cada pad, completando con la nota base en tomas antiguas.
    pub fn notes(&self) -> Vec<u8> {
        if self.pad_notes.len() >= crate::NUM_PADS {
            self.pad_notes.clone()
        } else {
            default_pad_notes(if self.midi_base_note == 0 {
                36
            } else {
                self.midi_base_note
            })
            .to_vec()
        }
    }
}

pub struct StartRequest {
    pub dir: PathBuf,
    pub voices: [bool; MAX_VOICES],
    pub voice_names: Vec<String>,
    /// Nota MIDI de cada pad (`NUM_PADS` valores).
    pub pad_notes: Vec<u8>,
}

enum Ctl {
    Start(StartRequest),
    Stop,
    Quit,
}

pub struct Recorder {
    ctl: Sender<Ctl>,
    ack: Receiver<Result<(), String>>,
    /// Resultado de cada toma al terminar de escribirse.
    pub finished: Receiver<Result<(PathBuf, TakeInfo), String>>,
    thread: Option<JoinHandle<()>>,
}

type Wav = hound::WavWriter<BufWriter<File>>;

struct Active {
    req: StartRequest,
    voices: Vec<Option<Wav>>,
    music: Wav,
    sampler: Wav,
    events: Vec<PadEvent>,
    frames: u64,
    buf: Vec<f32>,
    /// Ganancia de ducking (lineal), el mínimo de cada bloque de 10 ms.
    duck_curve: Vec<f32>,
    duck_block: usize,
    duck_min: f32,
    duck_count: usize,
}

#[inline]
fn to_i24(x: f32) -> i32 {
    (x.clamp(-1.0, 1.0) * 8_388_607.0) as i32
}

impl Active {
    fn open(req: StartRequest, sample_rate: u32) -> Result<Active> {
        std::fs::create_dir_all(&req.dir)
            .with_context(|| format!("no se puede crear {}", req.dir.display()))?;
        let spec = |ch| hound::WavSpec {
            channels: ch,
            sample_rate,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        };
        let mut voices = Vec::new();
        for v in 0..MAX_VOICES {
            voices.push(if req.voices[v] {
                Some(hound::WavWriter::create(
                    req.dir.join(format!("voice{}.wav", v + 1)),
                    spec(1),
                )?)
            } else {
                None
            });
        }
        let music = hound::WavWriter::create(req.dir.join("music.wav"), spec(2))?;
        let sampler = hound::WavWriter::create(req.dir.join("sampler.wav"), spec(2))?;
        Ok(Active {
            req,
            voices,
            music,
            sampler,
            events: Vec::new(),
            frames: 0,
            buf: vec![0.0; STEM_CHANNELS * 4096],
            duck_curve: Vec::new(),
            duck_block: (sample_rate / DUCK_STEPS_PER_SEC).max(1) as usize,
            duck_min: 1.0,
            duck_count: 0,
        })
    }

    /// Vuelca lo que haya en las colas. Devuelve los frames escritos.
    fn drain(
        &mut self,
        stems: &mut rtrb::Consumer<f32>,
        events: &mut rtrb::Consumer<PadEvent>,
    ) -> Result<usize> {
        let mut total = 0;
        loop {
            let avail = (stems.slots() / STEM_CHANNELS).min(self.buf.len() / STEM_CHANNELS);
            if avail == 0 {
                break;
            }
            let buf = &mut self.buf[..avail * STEM_CHANNELS];
            stems
                .pop_entire_slice(buf)
                .map_err(|_| anyhow!("cola de stems inconsistente"))?;
            for fr in buf.chunks_exact(STEM_CHANNELS) {
                for (v, w) in self.voices.iter_mut().enumerate() {
                    if let Some(w) = w {
                        w.write_sample(to_i24(fr[v]))?;
                    }
                }
                self.music.write_sample(to_i24(fr[MAX_VOICES]))?;
                self.music.write_sample(to_i24(fr[MAX_VOICES + 1]))?;
                self.sampler.write_sample(to_i24(fr[MAX_VOICES + 2]))?;
                self.sampler.write_sample(to_i24(fr[MAX_VOICES + 3]))?;
                self.duck_min = self.duck_min.min(fr[STEM_DUCK_CHANNEL]);
                self.duck_count += 1;
                if self.duck_count >= self.duck_block {
                    self.duck_curve.push(self.duck_min);
                    self.duck_min = 1.0;
                    self.duck_count = 0;
                }
            }
            self.frames += avail as u64;
            total += avail;
        }
        while let Ok(e) = events.pop() {
            self.events.push(e);
        }
        Ok(total)
    }

    fn finish(mut self, sample_rate: u32, dropped: u64) -> Result<(PathBuf, TakeInfo)> {
        for w in self.voices.into_iter().flatten() {
            w.finalize()?;
        }
        self.music.finalize()?;
        self.sampler.finalize()?;
        let mut pad_notes = self.req.pad_notes.clone();
        pad_notes.resize(crate::NUM_PADS, 0);
        midifile::write_smf(
            &self.req.dir.join("pads.mid"),
            &self.events,
            sample_rate,
            &pad_notes,
        )?;
        std::fs::write(
            self.req.dir.join(PAD_EVENTS_FILE),
            serde_json::to_string(&self.events)?,
        )?;

        // El ducking pasa a ser la envolvente editable de la música.
        if self.duck_count > 0 {
            self.duck_curve.push(self.duck_min);
        }
        let mut auto = MixAutomation::default();
        let step_s = self.duck_block as f64 / sample_rate.max(1) as f64;
        auto.envelopes[STEM_MUSIC] =
            Envelope::from_gain_curve(&self.duck_curve, step_s, DUCK_TOLERANCE_DB);
        auto.save_original(&self.req.dir)?;
        auto.save(&self.req.dir)?;

        let info = TakeInfo {
            sample_rate,
            frames: self.frames,
            voices: self.req.voices.to_vec(),
            voice_names: self.req.voice_names.clone(),
            dropped_frames: dropped,
            pad_events: self.events.len(),
            midi_base_note: pad_notes.first().copied().unwrap_or(36),
            pad_notes,
            music_unducked: true,
        };
        std::fs::write(
            self.req.dir.join("take.json"),
            serde_json::to_string_pretty(&info)?,
        )?;
        Ok((self.req.dir, info))
    }
}

impl Recorder {
    pub fn spawn(
        mut stems: rtrb::Consumer<f32>,
        mut events: rtrb::Consumer<PadEvent>,
        meters: Arc<Meters>,
        sample_rate: u32,
    ) -> Recorder {
        let (ctl_tx, ctl_rx) = channel::<Ctl>();
        let (ack_tx, ack_rx) = channel();
        let (fin_tx, fin_rx) = channel();
        let thread = std::thread::Builder::new()
            .name("podcaz-recorder".into())
            .spawn(move || {
                let mut active: Option<Active> = None;
                loop {
                    if active.is_none() {
                        match ctl_rx.recv_timeout(Duration::from_millis(100)) {
                            Ok(Ctl::Start(req)) => match Active::open(req, sample_rate) {
                                Ok(a) => {
                                    active = Some(a);
                                    let _ = ack_tx.send(Ok(()));
                                }
                                Err(e) => {
                                    let _ = ack_tx.send(Err(format!("{e:#}")));
                                }
                            },
                            Ok(Ctl::Stop) | Err(RecvTimeoutError::Timeout) => {}
                            Ok(Ctl::Quit) | Err(RecvTimeoutError::Disconnected) => break,
                        }
                        continue;
                    }

                    let a = active.as_mut().unwrap();
                    let mut stop = false;
                    let mut quit = false;
                    let mut error = a.drain(&mut stems, &mut events).err();
                    match ctl_rx.try_recv() {
                        Ok(Ctl::Stop) => stop = true,
                        Ok(Ctl::Quit) | Err(TryRecvError::Disconnected) => quit = true,
                        Ok(Ctl::Start(_)) => {
                            let _ = ack_tx.send(Err("ya hay una grabación en curso".into()));
                        }
                        Err(TryRecvError::Empty) => {}
                    }
                    if stop || quit || error.is_some() {
                        // esperar a que el motor confirme que dejó de grabar y vaciar el resto
                        let t0 = Instant::now();
                        while meters.recording.load(Ordering::Acquire)
                            && t0.elapsed() < Duration::from_secs(2)
                            && !quit
                        {
                            std::thread::sleep(Duration::from_millis(5));
                            if error.is_none() {
                                error = a.drain(&mut stems, &mut events).err();
                            }
                        }
                        if error.is_none() {
                            error = a.drain(&mut stems, &mut events).err();
                        }
                        let a = active.take().unwrap();
                        let dropped = meters.rec_dropped_frames.load(Ordering::Relaxed);
                        let res = a.finish(sample_rate, dropped);
                        let res = match (res, error) {
                            (Ok(_), Some(e)) => {
                                Err(format!("error de escritura durante la grabación: {e:#}"))
                            }
                            (Ok(r), None) => Ok(r),
                            (Err(e), _) => Err(format!("{e:#}")),
                        };
                        let _ = fin_tx.send(res);
                        if quit {
                            break;
                        }
                    } else {
                        std::thread::sleep(Duration::from_millis(15));
                    }
                }
            })
            .expect("no se pudo crear el hilo de grabación");
        Recorder {
            ctl: ctl_tx,
            ack: ack_rx,
            finished: fin_rx,
            thread: Some(thread),
        }
    }

    /// Crea los ficheros de la toma. Llamar ANTES de enviar `Command::StartRecording` al motor.
    pub fn start(&self, req: StartRequest) -> Result<()> {
        self.ctl
            .send(Ctl::Start(req))
            .map_err(|_| anyhow!("el hilo de grabación no está activo"))?;
        match self.ack.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(anyhow!(e)),
            Err(_) => Err(anyhow!("el hilo de grabación no responde")),
        }
    }

    /// Cierra la toma. Llamar DESPUÉS de enviar `Command::StopRecording`. El resultado llega por `finished`.
    pub fn stop(&self) {
        let _ = self.ctl.send(Ctl::Stop);
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.ctl.send(Ctl::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
