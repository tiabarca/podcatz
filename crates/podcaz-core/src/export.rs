//! Exportación offline de una toma a varios ficheros, dentro de `<out_dir>/<nombre>/`:
//!
//! 1. `<nombre> - mixdown.wav / .mp3`: mezcla de todas las pistas con sus envolventes,
//!    recortada y masterizada (sonoridad objetivo + limitador).
//! 2. `<nombre> - voz N (<nombre de la voz>).wav`: una por cada pista de voz grabada, con el
//!    mismo recorte y fundidos que el mixdown, su volumen y su envolvente, sin masterizar.
//! 3. `<nombre> - midi.mid`: los disparos de pads, desplazados según el recorte.
//!
//! La mezcla trabaja por bloques con un fichero temporal, así el consumo de memoria no depende
//! de la duración del episodio.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use anyhow::{anyhow, bail, Context, Result};
use ebur128::{EbuR128, Mode};

pub use crate::automation::{
    Envelope, MixAutomation, ENV_MIX, NUM_ENVELOPES, NUM_STEMS, STEM_MUSIC, STEM_SAMPLER,
};
use crate::automation::EnvGain;
use crate::dsp::{db_to_lin, Limiter};
use crate::engine::{AudioClip, PadEvent};
use crate::master::PreMaster;
use crate::recorder::{TakeInfo, PAD_EVENTS_FILE};
use crate::MAX_VOICES;

const CHUNK_FRAMES: usize = 16_384;

#[derive(Clone, Debug)]
pub struct ExportSettings {
    pub take_dir: PathBuf,
    /// Carpeta de exportaciones de la sesión; los ficheros van en `out_dir/base_name/`.
    pub out_dir: PathBuf,
    pub base_name: String,
    /// Segundos a recortar del principio.
    pub trim_start_s: f64,
    /// Segundos a recortar del final.
    pub trim_end_s: f64,
    pub fade_in_ms: f32,
    pub fade_out_ms: f32,
    /// Ajuste por stem: voz1..voz4, música, sampler.
    pub stem_gain_db: [f32; NUM_STEMS],
    /// Subidas y bajadas de volumen por pista y de la mezcla completa.
    pub automation: MixAutomation,
    pub target_lufs: f32,
    pub ceiling_db: f32,
    pub bus_comp: bool,
    /// Exportar la mezcla (mixdown) masterizada.
    pub mixdown: bool,
    /// Exportar un WAV por cada pista de voz grabada.
    pub voice_files: bool,
    /// Exportar el MIDI de los pads.
    pub midi_file: bool,
    /// Formatos del mixdown.
    pub wav: bool,
    pub wav_bits: u16,
    pub mp3: bool,
    pub mp3_kbps: u32,
    pub title: String,
    pub artist: String,
}

impl ExportSettings {
    pub fn new(take_dir: PathBuf, out_dir: PathBuf, base_name: String) -> Self {
        Self {
            take_dir,
            out_dir,
            base_name,
            trim_start_s: 0.0,
            trim_end_s: 0.0,
            fade_in_ms: 20.0,
            fade_out_ms: 500.0,
            stem_gain_db: [0.0; NUM_STEMS],
            automation: MixAutomation::default(),
            target_lufs: -16.0,
            ceiling_db: -1.0,
            bus_comp: true,
            mixdown: true,
            voice_files: true,
            midi_file: true,
            wav: true,
            wav_bits: 24,
            mp3: true,
            mp3_kbps: 192,
            title: String::new(),
            artist: String::new(),
        }
    }

    /// Carpeta donde se escriben los ficheros de esta exportación.
    pub fn output_dir(&self) -> PathBuf {
        self.out_dir.join(self.base_name.trim())
    }
}

pub struct ExportProgress {
    /// 0..=1000
    pub permille: AtomicU32,
    pub cancel: AtomicBool,
    /// Tramo de `permille` que ocupa la fase en curso.
    lo: AtomicU32,
    hi: AtomicU32,
}

impl Default for ExportProgress {
    fn default() -> Self {
        Self {
            permille: AtomicU32::new(0),
            cancel: AtomicBool::new(false),
            lo: AtomicU32::new(0),
            hi: AtomicU32::new(1000),
        }
    }
}

impl ExportProgress {
    /// Las siguientes llamadas a `report(0..=1000)` avanzan solo entre `lo` y `hi`.
    fn set_range(&self, lo: u32, hi: u32) {
        self.lo.store(lo, Ordering::Relaxed);
        self.hi.store(hi.max(lo), Ordering::Relaxed);
    }

    fn report(&self, p: u32) {
        let (lo, hi) = (self.lo.load(Ordering::Relaxed), self.hi.load(Ordering::Relaxed));
        self.permille
            .store(lo + (hi - lo) * p.min(1000) / 1000, Ordering::Relaxed);
    }
}

#[derive(Clone, Debug, Default)]
pub struct ExportResult {
    pub files: Vec<PathBuf>,
    pub seconds: f64,
    pub input_lufs: f64,
    pub output_lufs: f64,
    pub true_peak_db: f64,
    pub gain_db: f64,
}

pub fn mp3_available() -> bool {
    cfg!(feature = "mp3")
}

// ------------------------------------------------------------ lectura de stems

struct Stem {
    reader: hound::WavReader<BufReader<File>>,
    channels: usize,
    scale: f32,
    float: bool,
    gain: f32,
    env: EnvGain,
    buf: Vec<f32>,
}

/// Mezcla secuencial de los stems de una toma a estéreo.
pub struct TakeMixer {
    stems: Vec<Stem>,
    mix_env: EnvGain,
    pub sample_rate: u32,
    pub total_frames: u64,
    pos: u64,
}

impl TakeMixer {
    pub fn open(
        take_dir: &Path,
        gains_db: &[f32; NUM_STEMS],
        automation: &MixAutomation,
    ) -> Result<TakeMixer> {
        let info = TakeInfo::load(take_dir)?;
        let mut files: Vec<(String, usize)> = Vec::new();
        for v in 0..MAX_VOICES {
            if info.voices.get(v).copied().unwrap_or(false) {
                files.push((format!("voice{}.wav", v + 1), v));
            }
        }
        files.push(("music.wav".into(), STEM_MUSIC));
        files.push(("sampler.wav".into(), STEM_SAMPLER));

        let mut stems = Vec::new();
        let mut total = 0u64;
        for (name, idx) in files {
            let path = take_dir.join(&name);
            if !path.is_file() {
                continue;
            }
            let reader = hound::WavReader::open(&path)
                .with_context(|| format!("no se puede abrir {}", path.display()))?;
            let spec = reader.spec();
            if spec.sample_rate != info.sample_rate {
                bail!(
                    "{name}: frecuencia {} distinta de la toma ({})",
                    spec.sample_rate,
                    info.sample_rate
                );
            }
            total = total.max(reader.duration() as u64);
            stems.push(Stem {
                channels: spec.channels.max(1) as usize,
                float: spec.sample_format == hound::SampleFormat::Float,
                scale: 1.0 / (1u64 << (spec.bits_per_sample.max(1) - 1)) as f32,
                gain: db_to_lin(gains_db[idx]),
                env: EnvGain::new(automation.env(idx)),
                reader,
                buf: Vec::new(),
            });
        }
        if stems.is_empty() {
            bail!("la toma no contiene ficheros de audio");
        }
        Ok(TakeMixer {
            stems,
            mix_env: EnvGain::new(automation.env(ENV_MIX)),
            sample_rate: info.sample_rate,
            total_frames: total,
            pos: 0,
        })
    }

    pub fn seek(&mut self, frame: u64) -> Result<()> {
        for s in self.stems.iter_mut() {
            let f = frame.min(s.reader.duration() as u64) as u32;
            s.reader.seek(f)?;
        }
        self.pos = frame;
        Ok(())
    }

    /// Rellena `out` (estéreo entrelazado) con hasta `frames` frames. Devuelve los leídos.
    pub fn read(&mut self, out: &mut Vec<f32>, frames: usize) -> Result<usize> {
        let n = frames.min(self.total_frames.saturating_sub(self.pos) as usize);
        out.clear();
        out.resize(n * 2, 0.0);
        let sr = self.sample_rate.max(1) as f64;
        let pos = self.pos;
        for s in self.stems.iter_mut() {
            s.buf.clear();
            let want = n * s.channels;
            if s.float {
                for x in s.reader.samples::<f32>().take(want) {
                    s.buf.push(x?);
                }
            } else {
                for x in s.reader.samples::<i32>().take(want) {
                    s.buf.push(x? as f32 * s.scale);
                }
            }
            let flat = s.env.is_flat();
            if s.channels == 1 {
                for (i, (o, &x)) in out.chunks_exact_mut(2).zip(s.buf.iter()).enumerate() {
                    let g = if flat {
                        s.gain
                    } else {
                        s.gain * s.env.gain((pos + i as u64) as f64 / sr)
                    };
                    let y = x * g;
                    o[0] += y;
                    o[1] += y;
                }
            } else {
                for (i, (o, x)) in out
                    .chunks_exact_mut(2)
                    .zip(s.buf.chunks_exact(s.channels))
                    .enumerate()
                {
                    let g = if flat {
                        s.gain
                    } else {
                        s.gain * s.env.gain((pos + i as u64) as f64 / sr)
                    };
                    o[0] += x[0] * g;
                    o[1] += x[1] * g;
                }
            }
        }
        // subidas y bajadas de la mezcla completa
        if !self.mix_env.is_flat() {
            for (i, o) in out.chunks_exact_mut(2).enumerate() {
                let g = self.mix_env.gain((pos + i as u64) as f64 / sr);
                o[0] *= g;
                o[1] *= g;
            }
        }
        self.pos += n as u64;
        Ok(n)
    }
}

/// Mezcla rápida (sin masterizar) de un fragmento, para escucharlo antes de exportar.
pub fn render_preview(
    take_dir: &Path,
    gains_db: &[f32; NUM_STEMS],
    automation: &MixAutomation,
    start_s: f64,
    dur_s: f64,
    engine_rate: u32,
) -> Result<AudioClip> {
    let mut mixer = TakeMixer::open(take_dir, gains_db, automation)?;
    let sr = mixer.sample_rate;
    let start = ((start_s.max(0.0) * sr as f64) as u64).min(mixer.total_frames);
    mixer.seek(start)?;
    let mut data = Vec::new();
    let mut chunk = Vec::new();
    let mut left = (dur_s.max(0.0) * sr as f64) as usize;
    while left > 0 {
        let n = mixer.read(&mut chunk, left.min(CHUNK_FRAMES))?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&chunk);
        left -= n;
    }
    // fundidos cortos para evitar clics
    let fade = (0.02 * sr as f64) as usize;
    let frames = data.len() / 2;
    for i in 0..fade.min(frames / 2) {
        let g = i as f32 / fade as f32;
        for c in 0..2 {
            data[i * 2 + c] *= g;
            data[(frames - 1 - i) * 2 + c] *= g;
        }
    }
    for x in data.iter_mut() {
        *x = x.clamp(-1.0, 1.0);
    }
    let data = if sr != engine_rate {
        crate::decode::resample_stereo(&data, sr, engine_rate)
    } else {
        data
    };
    Ok(AudioClip {
        name: "preescucha".into(),
        sample_rate: engine_rate,
        data,
    })
}

/// Picos por segundo de la forma de onda que se calcula para el editor de envolventes.
pub const PEAKS_PER_SEC: u32 = 50;

/// Pico (valor absoluto máximo) de cada 1/`PEAKS_PER_SEC` s de una pista de la toma, tal como
/// se grabó (sin volumen ni envolvente). `track` es el índice de pista (voz, música o sampler);
/// con `ENV_MIX` es la suma de todas. Una pista que no existe da un vector vacío.
pub fn waveform_peaks(take_dir: &Path, track: usize) -> Result<Vec<f32>> {
    let info = TakeInfo::load(take_dir)?;
    let block = (info.sample_rate / PEAKS_PER_SEC).max(1) as usize;
    let mut peaks = Vec::new();

    if track >= ENV_MIX {
        // suma de todas las pistas, sin ganancias ni envolventes
        let mut mixer = TakeMixer::open(take_dir, &[0.0; NUM_STEMS], &MixAutomation::default())?;
        let mut chunk = Vec::new();
        let step = (CHUNK_FRAMES / block).max(1) * block;
        loop {
            let n = mixer.read(&mut chunk, step)?;
            if n == 0 {
                break;
            }
            for blk in chunk.chunks(block * 2) {
                peaks.push(blk.iter().fold(0f32, |m, &x| m.max(x.abs())));
            }
        }
        return Ok(peaks);
    }

    let name = if track < MAX_VOICES {
        format!("voice{}.wav", track + 1)
    } else if track == STEM_MUSIC {
        "music.wav".to_string()
    } else {
        "sampler.wav".to_string()
    };
    let path = take_dir.join(name);
    if !path.is_file() {
        return Ok(peaks);
    }
    let mut reader = hound::WavReader::open(&path)
        .with_context(|| format!("no se puede abrir {}", path.display()))?;
    let spec = reader.spec();
    let ch = spec.channels.max(1) as usize;
    let float = spec.sample_format == hound::SampleFormat::Float;
    let scale = 1.0 / (1u64 << (spec.bits_per_sample.max(1) - 1)) as f32;
    let per_block = block * ch;
    let (mut max, mut count) = (0f32, 0usize);
    let push = |x: f32, max: &mut f32, count: &mut usize, peaks: &mut Vec<f32>| {
        *max = max.max(x.abs());
        *count += 1;
        if *count == per_block {
            peaks.push(*max);
            *max = 0.0;
            *count = 0;
        }
    };
    if float {
        for x in reader.samples::<f32>() {
            push(x?, &mut max, &mut count, &mut peaks);
        }
    } else {
        for x in reader.samples::<i32>() {
            push(x? as f32 * scale, &mut max, &mut count, &mut peaks);
        }
    }
    if count > 0 {
        peaks.push(max);
    }
    Ok(peaks)
}

// ------------------------------------------------------------ fichero temporal f32

struct TempAudio {
    path: PathBuf,
}

impl TempAudio {
    fn writer(&self) -> Result<BufWriter<File>> {
        Ok(BufWriter::new(File::create(&self.path)?))
    }
    fn reader(&self) -> Result<BufReader<File>> {
        let mut f = File::open(&self.path)?;
        f.seek(SeekFrom::Start(0))?;
        Ok(BufReader::new(f))
    }
}

impl Drop for TempAudio {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn write_f32(w: &mut impl Write, data: &[f32], scratch: &mut Vec<u8>) -> std::io::Result<()> {
    scratch.clear();
    for x in data {
        scratch.extend_from_slice(&x.to_le_bytes());
    }
    w.write_all(scratch)
}

/// Lee hasta `out.len()` muestras; devuelve las leídas.
fn read_f32(r: &mut impl Read, out: &mut [f32], scratch: &mut Vec<u8>) -> std::io::Result<usize> {
    scratch.resize(out.len() * 4, 0);
    let mut got = 0;
    while got < scratch.len() {
        let n = r.read(&mut scratch[got..])?;
        if n == 0 {
            break;
        }
        got += n;
    }
    let samples = got / 4;
    for (o, b) in out.iter_mut().zip(scratch.chunks_exact(4)).take(samples) {
        *o = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    }
    Ok(samples)
}

// ------------------------------------------------------------ MP3

#[cfg(feature = "mp3")]
struct Mp3Out {
    enc: mp3lame_encoder::Encoder,
    file: BufWriter<File>,
    buf: Vec<u8>,
}

#[cfg(feature = "mp3")]
impl Mp3Out {
    fn new(path: &Path, sample_rate: u32, kbps: u32, title: &str, artist: &str) -> Result<Self> {
        use mp3lame_encoder::{Bitrate, Builder, Id3Tag, Quality};
        let brate = match kbps {
            0..=96 => Bitrate::Kbps96,
            97..=128 => Bitrate::Kbps128,
            129..=160 => Bitrate::Kbps160,
            161..=192 => Bitrate::Kbps192,
            193..=256 => Bitrate::Kbps256,
            _ => Bitrate::Kbps320,
        };
        let e = |what: &str, err: String| anyhow!("LAME: {what}: {err}");
        let mut b = Builder::new().ok_or_else(|| anyhow!("no se pudo inicializar LAME"))?;
        b.set_num_channels(2)
            .map_err(|x| e("canales", format!("{x:?}")))?;
        b.set_sample_rate(sample_rate)
            .map_err(|x| e("frecuencia", format!("{x:?}")))?;
        b.set_brate(brate)
            .map_err(|x| e("bitrate", format!("{x:?}")))?;
        b.set_quality(Quality::NearBest)
            .map_err(|x| e("calidad", format!("{x:?}")))?;
        if !title.is_empty() || !artist.is_empty() {
            // ID3 en Latin-1 no admite todos los caracteres: se ignora el error
            let _ = b.set_id3_tag(Id3Tag {
                title: title.as_bytes(),
                artist: artist.as_bytes(),
                album: &[],
                album_art: &[],
                year: &[],
                comment: b"PODCAZ",
            });
        }
        let enc = b
            .build()
            .map_err(|x| e("configuración", format!("{x:?}")))?;
        Ok(Self {
            enc,
            file: BufWriter::new(File::create(path)?),
            buf: Vec::new(),
        })
    }

    fn write(&mut self, stereo: &[f32]) -> Result<()> {
        use mp3lame_encoder::InterleavedPcm;
        self.buf.clear();
        self.buf
            .reserve(mp3lame_encoder::max_required_buffer_size(stereo.len() / 2));
        self.enc
            .encode_to_vec(InterleavedPcm(stereo), &mut self.buf)
            .map_err(|x| anyhow!("LAME: error codificando: {x:?}"))?;
        self.file.write_all(&self.buf)?;
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        use mp3lame_encoder::FlushNoGap;
        self.buf.clear();
        self.buf.reserve(7200);
        self.enc
            .flush_to_vec::<FlushNoGap>(&mut self.buf)
            .map_err(|x| anyhow!("LAME: error al cerrar: {x:?}"))?;
        self.file.write_all(&self.buf)?;
        self.file.flush()?;
        Ok(())
    }
}

// ------------------------------------------------------------ exportación

/// Una pasada del máster: ganancia + limitador sobre el temporal. `sink` recibe el audio final.
fn master_pass(
    temp: &TempAudio,
    sample_rate: u32,
    frames: u64,
    gain_db: f64,
    ceiling_db: f32,
    measure_true_peak: bool,
    progress: &ExportProgress,
    prog_range: (u32, u32),
    mut sink: impl FnMut(&[f32]) -> Result<()>,
) -> Result<(f64, f64)> {
    let mode = if measure_true_peak {
        Mode::I | Mode::TRUE_PEAK
    } else {
        Mode::I
    };
    let mut meter =
        EbuR128::new(2, sample_rate, mode).map_err(|e| anyhow!("medidor de sonoridad: {e}"))?;
    let mut lim = Limiter::new(sample_rate as f32, ceiling_db, 5.0, 120.0);
    let mut skip = lim.latency();
    let mut tail = lim.latency();
    let g = db_to_lin(gain_db as f32);
    let mut r = temp.reader()?;
    let mut scratch = Vec::new();
    let mut buf = vec![0f32; CHUNK_FRAMES * 2];
    let mut out: Vec<f32> = Vec::with_capacity(CHUNK_FRAMES * 2);
    let mut done = 0u64;
    loop {
        if progress.cancel.load(Ordering::Relaxed) {
            bail!("exportación cancelada");
        }
        let mut n = read_f32(&mut r, &mut buf, &mut scratch)? / 2;
        if n == 0 {
            if tail == 0 {
                break;
            }
            // vaciar la línea de retardo del limitador
            n = tail.min(CHUNK_FRAMES);
            buf[..n * 2].fill(0.0);
            tail -= n;
        } else {
            done += n as u64;
        }
        out.clear();
        for fr in buf[..n * 2].chunks_exact(2) {
            let (l, r_) = lim.process(fr[0] * g, fr[1] * g);
            if skip > 0 {
                skip -= 1;
            } else {
                out.push(l);
                out.push(r_);
            }
        }
        if !out.is_empty() {
            meter
                .add_frames_f32(&out)
                .map_err(|e| anyhow!("medidor de sonoridad: {e}"))?;
            sink(&out)?;
        }
        let p = prog_range.0 + ((prog_range.1 - prog_range.0) as u64 * done / frames.max(1)) as u32;
        progress.report(p);
    }
    let lufs = meter.loudness_global().unwrap_or(f64::NEG_INFINITY);
    let tp = if measure_true_peak {
        let a = meter.true_peak(0).unwrap_or(0.0);
        let b = meter.true_peak(1).unwrap_or(0.0);
        20.0 * a.max(b).max(1e-9).log10()
    } else {
        f64::NAN
    };
    Ok((lufs, tp))
}

struct Tpdf(u32);
impl Tpdf {
    #[inline]
    fn next(&mut self) -> f32 {
        let mut r = || {
            self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
            (self.0 >> 8) as f32 / (1u32 << 24) as f32
        };
        r() - r()
    }
}

/// Rango de frames de la toma que se exporta, según el recorte.
struct Range {
    start: u64,
    frames: u64,
    fade_in: u64,
    fade_out: u64,
}

impl Range {
    fn new(s: &ExportSettings, sr: u32, total_frames: u64) -> Result<Range> {
        let start = (s.trim_start_s.max(0.0) * sr as f64) as u64;
        let end = total_frames.saturating_sub((s.trim_end_s.max(0.0) * sr as f64) as u64);
        if end <= start || end - start < (sr as u64) / 2 {
            bail!("tras el recorte no queda audio suficiente (menos de medio segundo)");
        }
        let frames = end - start;
        Ok(Range {
            start,
            frames,
            fade_in: ((s.fade_in_ms.max(0.0) * 0.001 * sr as f32) as u64).min(frames / 2),
            fade_out: ((s.fade_out_ms.max(0.0) * 0.001 * sr as f32) as u64).min(frames / 2),
        })
    }

    /// Ganancia del fundido de entrada/salida en el frame `f` (relativo al inicio del recorte).
    #[inline]
    fn fade(&self, f: u64) -> f32 {
        let mut g = 1.0f32;
        if f < self.fade_in {
            g *= f as f32 / self.fade_in as f32;
        }
        if self.frames - f <= self.fade_out {
            g *= (self.frames - f) as f32 / self.fade_out as f32;
        }
        g
    }
}

/// Exporta la toma según `s`. Pensado para ejecutarse en un hilo aparte.
pub fn export_take(s: &ExportSettings, progress: &ExportProgress) -> Result<ExportResult> {
    if !s.mixdown && !s.voice_files && !s.midi_file {
        bail!("elige qué exportar: mixdown, pistas de voz o MIDI");
    }
    if s.mixdown && !s.wav && !s.mp3 {
        bail!("elige al menos un formato para el mixdown (WAV o MP3)");
    }
    if s.mixdown && s.mp3 && !mp3_available() {
        bail!("esta compilación no incluye el codificador MP3 (feature `mp3`)");
    }
    let base = s.base_name.trim();
    if base.is_empty() {
        bail!("falta el nombre del fichero");
    }
    if base.chars().any(|c| "<>:\"/\\|?*".contains(c)) {
        bail!("el nombre contiene caracteres no válidos para un fichero");
    }
    progress.set_range(0, 1000);
    progress.report(0);

    let info = TakeInfo::load(&s.take_dir)?;
    let dir = s.output_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("no se puede crear {}", dir.display()))?;

    // El mixdown es lo más largo: se reparte el progreso según lo que se pida.
    let voices_to_write: Vec<usize> = if s.voice_files {
        (0..MAX_VOICES)
            .filter(|&v| {
                info.voices.get(v).copied().unwrap_or(false)
                    && s.take_dir.join(format!("voice{}.wav", v + 1)).is_file()
            })
            .collect()
    } else {
        Vec::new()
    };
    let mix_share = if s.mixdown {
        if voices_to_write.is_empty() {
            1000
        } else {
            700
        }
    } else {
        0
    };

    let mut res = ExportResult::default();
    if s.mixdown {
        progress.set_range(0, mix_share);
        export_mixdown(s, &dir, progress, &mut res)?;
    }
    if !voices_to_write.is_empty() {
        let per = (1000 - mix_share) / voices_to_write.len() as u32;
        for (n, &v) in voices_to_write.iter().enumerate() {
            let lo = mix_share + per * n as u32;
            progress.set_range(lo, lo + per);
            let path = export_voice(s, &info, v, &dir, progress)?;
            res.files.push(path);
        }
    }
    if s.midi_file {
        res.files.push(export_midi(s, &info, &dir)?);
    }
    progress.set_range(0, 1000);
    progress.report(1000);
    Ok(res)
}

/// Quita del nombre lo que Windows no admite en un fichero.
fn file_safe(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if "<>:\"/\\|?*".contains(c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    s.trim().trim_matches('.').to_string()
}

/// Un WAV mono por pista de voz: mismo recorte y fundidos que el mixdown, con el volumen y la
/// envolvente de la pista, y sin masterizar (para poder mezclarlo en otro editor).
fn export_voice(
    s: &ExportSettings,
    info: &TakeInfo,
    v: usize,
    dir: &Path,
    progress: &ExportProgress,
) -> Result<PathBuf> {
    let src = s.take_dir.join(format!("voice{}.wav", v + 1));
    let mut reader =
        hound::WavReader::open(&src).with_context(|| format!("no se puede abrir {}", src.display()))?;
    let spec = reader.spec();
    if spec.sample_rate != info.sample_rate {
        bail!(
            "voice{}.wav: frecuencia {} distinta de la toma ({})",
            v + 1,
            spec.sample_rate,
            info.sample_rate
        );
    }
    let channels = spec.channels.max(1) as usize;
    let float = spec.sample_format == hound::SampleFormat::Float;
    let scale = 1.0 / (1u64 << (spec.bits_per_sample.max(1) - 1)) as f32;
    let range = Range::new(s, info.sample_rate, reader.duration() as u64)?;
    let sr = info.sample_rate as f64;
    let gain = db_to_lin(s.stem_gain_db[v]);
    let mut env = EnvGain::new(s.automation.env(v));
    let flat = env.is_flat();

    let name = info
        .voice_names
        .get(v)
        .map(|n| file_safe(n))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("Voz {}", v + 1));
    let path = dir.join(format!("{} - voz {} ({}).wav", s.base_name.trim(), v + 1, name));
    let bits = if s.wav_bits == 16 { 16 } else { 24 };
    let mut w = hound::WavWriter::create(
        &path,
        hound::WavSpec {
            channels: 1,
            sample_rate: info.sample_rate,
            bits_per_sample: bits,
            sample_format: hound::SampleFormat::Int,
        },
    )
    .with_context(|| format!("no se puede crear {}", path.display()))?;

    let result = (|| -> Result<()> {
        reader.seek(range.start.min(reader.duration() as u64) as u32)?;
        let mut dither = Tpdf(0x9e37_79b9 ^ v as u32);
        let mut buf: Vec<f32> = Vec::new();
        let mut done = 0u64;
        while done < range.frames {
            if progress.cancel.load(Ordering::Relaxed) {
                bail!("exportación cancelada");
            }
            let want = ((range.frames - done) as usize).min(CHUNK_FRAMES);
            buf.clear();
            let n = want * channels;
            if float {
                for x in reader.samples::<f32>().take(n) {
                    buf.push(x?);
                }
            } else {
                for x in reader.samples::<i32>().take(n) {
                    buf.push(x? as f32 * scale);
                }
            }
            for i in 0..want {
                let f = done + i as u64;
                // si la pista acaba antes que la toma, silencio
                let x = buf.get(i * channels).copied().unwrap_or(0.0);
                let g = if flat {
                    gain
                } else {
                    gain * env.gain((range.start + f) as f64 / sr)
                };
                let y = (x * g * range.fade(f)).clamp(-1.0, 1.0);
                if bits == 16 {
                    let q = (y * 32767.0 + dither.next()).round().clamp(-32768.0, 32767.0);
                    w.write_sample(q as i16)?;
                } else {
                    w.write_sample((y * 8_388_607.0) as i32)?;
                }
            }
            done += want as u64;
            progress.report((1000 * done / range.frames) as u32);
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            w.finalize()?;
            Ok(path)
        }
        Err(e) => {
            drop(w);
            let _ = std::fs::remove_file(&path);
            Err(e)
        }
    }
}

/// El MIDI de los pads con los tiempos desplazados por el recorte.
fn export_midi(s: &ExportSettings, info: &TakeInfo, dir: &Path) -> Result<PathBuf> {
    let path = dir.join(format!("{} - midi.mid", s.base_name.trim()));
    let sr = info.sample_rate.max(1);
    let events: Vec<PadEvent> = match std::fs::read_to_string(s.take_dir.join(PAD_EVENTS_FILE)) {
        Ok(text) => serde_json::from_str(&text).context("pad_events.json no es válido")?,
        Err(_) => {
            // toma antigua sin eventos guardados: se copia el MIDI tal cual, sin recortar
            let src = s.take_dir.join("pads.mid");
            std::fs::copy(&src, &path)
                .with_context(|| format!("no se puede copiar {}", src.display()))?;
            return Ok(path);
        }
    };
    let start = (s.trim_start_s.max(0.0) * sr as f64) as u64;
    let end = info
        .frames
        .saturating_sub((s.trim_end_s.max(0.0) * sr as f64) as u64);
    let shifted: Vec<PadEvent> = events
        .into_iter()
        .filter(|e| e.frame >= start && e.frame < end)
        .map(|e| PadEvent {
            frame: e.frame - start,
            ..e
        })
        .collect();
    crate::midifile::write_smf(&path, &shifted, sr, &info.notes())
        .with_context(|| format!("no se puede escribir {}", path.display()))?;
    Ok(path)
}

/// Mezcla, recorta, aplica envolventes y masteriza a `<dir>/<nombre> - mixdown.(wav|mp3)`.
fn export_mixdown(
    s: &ExportSettings,
    dir: &Path,
    progress: &ExportProgress,
    out: &mut ExportResult,
) -> Result<()> {
    let base = format!("{} - mixdown", s.base_name.trim());
    let base = base.as_str();
    progress.report(0);

    let mut mixer = TakeMixer::open(&s.take_dir, &s.stem_gain_db, &s.automation)?;
    let sr = mixer.sample_rate;
    let range = Range::new(s, sr, mixer.total_frames)?;
    let (start, frames) = (range.start, range.frames);

    // ---- pasada 1: mezcla + recorte + fundidos + pre-máster -> temporal
    let temp = TempAudio {
        path: dir.join(format!(".{base}.{}.tmp", std::process::id())),
    };
    let mut meter =
        EbuR128::new(2, sr, Mode::I).map_err(|e| anyhow!("medidor de sonoridad: {e}"))?;
    {
        let mut pre = PreMaster::new(sr, s.bus_comp);
        mixer.seek(start)?;
        let mut w = temp.writer()?;
        let mut chunk = Vec::new();
        let mut scratch = Vec::new();
        let mut done = 0u64;
        while done < frames {
            if progress.cancel.load(Ordering::Relaxed) {
                bail!("exportación cancelada");
            }
            let want = ((frames - done) as usize).min(CHUNK_FRAMES);
            mixer.read(&mut chunk, want)?;
            chunk.resize(want * 2, 0.0); // si la toma acaba antes, silencio
            for (i, fr) in chunk.chunks_exact_mut(2).enumerate() {
                let g = range.fade(done + i as u64);
                fr[0] *= g;
                fr[1] *= g;
            }
            pre.process(&mut chunk);
            meter
                .add_frames_f32(&chunk)
                .map_err(|e| anyhow!("medidor de sonoridad: {e}"))?;
            write_f32(&mut w, &chunk, &mut scratch)?;
            done += want as u64;
            progress.report((300 * done / frames) as u32);
        }
        w.flush()?;
    }
    let input_lufs = meter.loudness_global().unwrap_or(f64::NEG_INFINITY);

    // ---- pasadas de medida: ajustar ganancia y techo del limitador para acertar la
    // sonoridad objetivo y respetar el techo en pico real (true peak, entre muestras)
    let target = s.target_lufs as f64;
    let ceiling = s.ceiling_db as f64;
    let mut gain_db = if input_lufs.is_finite() && input_lufs > -70.0 {
        (target - input_lufs).clamp(-30.0, 30.0)
    } else {
        0.0
    };
    let mut lim_ceiling_db = ceiling;
    for it in 0..3u32 {
        let (lufs, tp) = master_pass(
            &temp,
            sr,
            frames,
            gain_db,
            lim_ceiling_db as f32,
            true,
            progress,
            (300 + it * 100, 400 + it * 100),
            |_| Ok(()),
        )?;
        let miss = target - lufs;
        let over = tp - ceiling;
        let loud_ok = !lufs.is_finite() || lufs < -70.0 || miss.abs() < 0.2;
        let peak_ok = over <= 0.05;
        if loud_ok && peak_ok {
            break;
        }
        if !loud_ok {
            gain_db = (gain_db + miss).clamp(-30.0, 30.0);
        }
        if !peak_ok {
            lim_ceiling_db = (lim_ceiling_db - over - 0.05).max(ceiling - 6.0);
        }
    }
    progress.report(600);

    // ---- pasada final: escribir ficheros
    let wav_path = dir.join(format!("{base}.wav"));
    let mp3_path = dir.join(format!("{base}.mp3"));
    let bits = if s.wav_bits == 16 { 16 } else { 24 };
    let mut wav = if s.wav {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: sr,
            bits_per_sample: bits,
            sample_format: hound::SampleFormat::Int,
        };
        Some(
            hound::WavWriter::create(&wav_path, spec)
                .with_context(|| format!("no se puede crear {}", wav_path.display()))?,
        )
    } else {
        None
    };
    #[cfg(feature = "mp3")]
    let mut mp3 = if s.mp3 {
        Some(Mp3Out::new(&mp3_path, sr, s.mp3_kbps, &s.title, &s.artist)?)
    } else {
        None
    };

    let mut dither = Tpdf(0x1234_5678);
    let result = master_pass(
        &temp,
        sr,
        frames,
        gain_db,
        lim_ceiling_db as f32,
        true,
        progress,
        (600, 1000),
        |out| {
            if let Some(w) = wav.as_mut() {
                if bits == 16 {
                    for &x in out {
                        let v = (x * 32767.0 + dither.next())
                            .round()
                            .clamp(-32768.0, 32767.0);
                        w.write_sample(v as i16)?;
                    }
                } else {
                    for &x in out {
                        w.write_sample((x.clamp(-1.0, 1.0) * 8_388_607.0) as i32)?;
                    }
                }
            }
            #[cfg(feature = "mp3")]
            if let Some(m) = mp3.as_mut() {
                m.write(out)?;
            }
            Ok(())
        },
    );

    let (output_lufs, true_peak_db) = match result {
        Ok(v) => v,
        Err(e) => {
            // no dejar ficheros a medias
            drop(wav);
            #[cfg(feature = "mp3")]
            drop(mp3);
            if s.wav {
                let _ = std::fs::remove_file(&wav_path);
            }
            if s.mp3 {
                let _ = std::fs::remove_file(&mp3_path);
            }
            return Err(e);
        }
    };
    if let Some(w) = wav {
        w.finalize()?;
        out.files.push(wav_path);
    }
    #[cfg(feature = "mp3")]
    if let Some(m) = mp3 {
        m.finish()?;
        out.files.push(mp3_path);
    }
    progress.report(1000);

    out.seconds = frames as f64 / sr as f64;
    out.input_lufs = input_lufs;
    out.output_lufs = output_lufs;
    out.true_peak_db = true_peak_db;
    out.gain_db = gain_db;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::PadEvent;
    use crate::recorder::TakeInfo;

    /// Crea una toma sintética: voz (tono a ráfagas) + música floja.
    fn make_take(dir: &Path, sr: u32, secs: u32) {
        std::fs::create_dir_all(dir).unwrap();
        let spec = |ch| hound::WavSpec {
            channels: ch,
            sample_rate: sr,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        };
        let n = (sr * secs) as usize;
        let mut v = hound::WavWriter::create(dir.join("voice1.wav"), spec(1)).unwrap();
        let mut m = hound::WavWriter::create(dir.join("music.wav"), spec(2)).unwrap();
        let mut sp = hound::WavWriter::create(dir.join("sampler.wav"), spec(2)).unwrap();
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let talking = (t * 0.7).fract() < 0.75;
            let voice = if talking {
                (t * 2.0 * std::f32::consts::PI * 180.0).sin() * 0.12
                    + (t * 2.0 * std::f32::consts::PI * 950.0).sin() * 0.05
            } else {
                0.0
            };
            v.write_sample((voice * 8_388_607.0) as i32).unwrap();
            let mu = (t * 2.0 * std::f32::consts::PI * 330.0).sin() * 0.02;
            m.write_sample((mu * 8_388_607.0) as i32).unwrap();
            m.write_sample((mu * 8_388_607.0) as i32).unwrap();
            // un golpe fuerte en el sampler para forzar al limitador
            let hit = if (sr as usize * 3..sr as usize * 3 + 2000).contains(&i) {
                0.9
            } else {
                0.0
            };
            sp.write_sample((hit * 8_388_607.0) as i32).unwrap();
            sp.write_sample((hit * 8_388_607.0) as i32).unwrap();
        }
        v.finalize().unwrap();
        m.finalize().unwrap();
        sp.finalize().unwrap();
        let pad_events = [PadEvent {
            frame: sr as u64 * 3,
            pad: 0,
            velocity: 100,
        }];
        crate::midifile::write_smf(
            &dir.join("pads.mid"),
            &pad_events,
            sr,
            &crate::default_pad_notes(36),
        )
        .unwrap();
        std::fs::write(
            dir.join(PAD_EVENTS_FILE),
            serde_json::to_string(&pad_events).unwrap(),
        )
        .unwrap();
        let info = TakeInfo {
            sample_rate: sr,
            frames: n as u64,
            voices: vec![true, false, false, false],
            voice_names: vec!["Ana".into(), String::new(), String::new(), String::new()],
            ..Default::default()
        };
        std::fs::write(dir.join("take.json"), serde_json::to_string(&info).unwrap()).unwrap();
    }

    /// Pico absoluto (0..1) de un WAV entre dos segundos.
    fn wav_peak(path: &Path, from_s: f64, to_s: f64) -> f32 {
        let mut r = hound::WavReader::open(path).unwrap();
        let spec = r.spec();
        let ch = spec.channels as usize;
        let sr = spec.sample_rate as f64;
        let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
        r.seek((from_s * sr) as u32).unwrap();
        let n = ((to_s - from_s) * sr) as usize * ch;
        r.samples::<i32>()
            .take(n)
            .map(|x| (x.unwrap() as f32 * scale).abs())
            .fold(0.0, f32::max)
    }

    #[test]
    fn export_hits_loudness_target_and_ceiling() {
        let root = std::env::temp_dir().join(format!("podcaz-export-test-{}", std::process::id()));
        let take = root.join("takes").join("t1");
        make_take(&take, 48000, 12);

        let mut s = ExportSettings::new(take.clone(), root.join("export"), "episodio".into());
        s.trim_start_s = 1.0;
        s.trim_end_s = 1.0;
        s.mp3 = mp3_available();
        let progress = ExportProgress::default();
        let res = export_take(&s, &progress).unwrap();
        let out = root.join("export").join("episodio");

        assert!((res.seconds - 10.0).abs() < 0.01);
        assert!(
            (res.output_lufs - (-16.0)).abs() < 0.6,
            "LUFS {}",
            res.output_lufs
        );
        assert!(res.true_peak_db < -0.8, "true peak {}", res.true_peak_db);
        assert_eq!(progress.permille.load(Ordering::Relaxed), 1000);

        // el WAV existe, dura lo esperado y respeta el techo en pico de muestra
        let mut r = hound::WavReader::open(out.join("episodio - mixdown.wav")).unwrap();
        assert_eq!(r.duration(), 480_000);
        let peak = r
            .samples::<i32>()
            .map(|x| x.unwrap().unsigned_abs())
            .max()
            .unwrap() as f32
            / 8_388_608.0;
        assert!(peak <= db_to_lin(-1.0) * 1.001, "pico {peak}");

        if mp3_available() {
            let mp3 = out.join("episodio - mixdown.mp3");
            let size = std::fs::metadata(&mp3).unwrap().len();
            // 10 s a 192 kbps ~ 240 kB
            assert!(size > 200_000 && size < 300_000, "tamaño mp3 {size}");
            // y se puede volver a decodificar
            let (data, rate) = crate::decode::decode_file(&mp3).unwrap();
            assert_eq!(rate, 48000);
            assert!((data.len() as i64 / 2 - 480_000).abs() < 5000);
        }
        // no queda temporal
        let leftovers: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());

        // preescucha
        let auto = MixAutomation::default();
        let clip = render_preview(&take, &s.stem_gain_db, &auto, 2.0, 1.5, 44100).unwrap();
        assert!((clip.frames() as i64 - 66150).abs() < 4);

        // recorte excesivo -> error claro
        s.trim_start_s = 11.8;
        assert!(export_take(&s, &progress).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn export_writes_mixdown_voices_and_midi() {
        let root = std::env::temp_dir().join(format!("podcaz-multi-test-{}", std::process::id()));
        let take = root.join("takes").join("t1");
        make_take(&take, 48000, 12);

        let mut s = ExportSettings::new(take, root.join("export"), "ep".into());
        s.trim_start_s = 2.0; // el golpe del sampler (3 s) queda a 1 s del nuevo inicio
        s.trim_end_s = 1.0;
        s.mp3 = false;
        let res = export_take(&s, &ExportProgress::default()).unwrap();
        let out = root.join("export").join("ep");

        let names: Vec<String> = res
            .files
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "ep - mixdown.wav",
                "ep - voz 1 (Ana).wav",
                "ep - midi.mid"
            ]
        );
        for f in &res.files {
            assert!(f.is_file(), "{}", f.display());
        }

        // la pista de voz es mono, con la misma duración que el mixdown y sin masterizar
        let v = hound::WavReader::open(out.join("ep - voz 1 (Ana).wav")).unwrap();
        assert_eq!(v.spec().channels, 1);
        assert_eq!(v.duration(), 9 * 48000);
        let mix = hound::WavReader::open(out.join("ep - mixdown.wav")).unwrap();
        assert_eq!(mix.spec().channels, 2);
        assert_eq!(mix.duration(), 9 * 48000);
        // voz original ~0,17 de pico; sin masterizar sigue por debajo de 0,3
        let vp = wav_peak(&out.join("ep - voz 1 (Ana).wav"), 0.5, 8.0);
        assert!(vp > 0.1 && vp < 0.3, "pico de voz {vp}");

        // MIDI: el disparo de 3 s pasa a 1 s (960 ticks = 0x87 0x40)
        let mid = std::fs::read(out.join("ep - midi.mid")).unwrap();
        assert!(mid.windows(5).any(|w| w == [0x87, 0x40, 0x90, 36, 100]));

        // se puede pedir solo una parte
        let mut s2 = s.clone();
        s2.base_name = "solo-voces".into();
        s2.mixdown = false;
        s2.midi_file = false;
        let res = export_take(&s2, &ExportProgress::default()).unwrap();
        assert_eq!(res.files.len(), 1);
        s2.voice_files = false;
        assert!(export_take(&s2, &ExportProgress::default()).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn waveform_peaks_cover_the_whole_take() {
        let root = std::env::temp_dir().join(format!("podcaz-peaks-test-{}", std::process::id()));
        let take = root.join("takes").join("t1");
        make_take(&take, 48000, 12);
        let voice = waveform_peaks(&take, 0).unwrap();
        assert_eq!(voice.len(), 12 * PEAKS_PER_SEC as usize);
        let top = voice.iter().cloned().fold(0.0, f32::max);
        assert!(top > 0.15 && top < 0.2, "{top}");
        // el golpe del sampler (a los 3 s) se ve en su pista y en la suma
        let sampler = waveform_peaks(&take, STEM_SAMPLER).unwrap();
        assert!(sampler[3 * 50 + 1] > 0.8 && sampler[10] == 0.0);
        let mix = waveform_peaks(&take, ENV_MIX).unwrap();
        assert_eq!(mix.len(), voice.len());
        assert!(mix[3 * 50 + 1] > 0.8);
        // una voz que no se grabó
        assert!(waveform_peaks(&take, 2).unwrap().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn midi_export_drops_events_outside_the_trim() {
        let root = std::env::temp_dir().join(format!("podcaz-midi-test-{}", std::process::id()));
        let take = root.join("takes").join("t1");
        make_take(&take, 48000, 12);
        let mut s = ExportSettings::new(take, root.join("export"), "ep".into());
        s.mixdown = false;
        s.voice_files = false;
        s.trim_start_s = 5.0; // el golpe a los 3 s queda fuera
        export_take(&s, &ExportProgress::default()).unwrap();
        let mid = std::fs::read(root.join("export/ep/ep - midi.mid")).unwrap();
        assert!(!mid.windows(2).any(|w| w == [0x90, 36]));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn envelopes_change_the_exported_audio() {
        let root = std::env::temp_dir().join(format!("podcaz-env-test-{}", std::process::id()));
        let take = root.join("takes").join("t1");
        make_take(&take, 48000, 12);

        let mut s = ExportSettings::new(take, root.join("export"), "ep".into());
        s.mp3 = false;
        s.midi_file = false;
        s.fade_in_ms = 0.0;
        s.fade_out_ms = 0.0;
        // la voz baja 20 dB a partir de los 6 s; a los 3 s está a 0 dB
        s.automation.envelopes[0].insert(5.0, 0.0);
        s.automation.envelopes[0].insert(6.0, -20.0);
        export_take(&s, &ExportProgress::default()).unwrap();
        let voice = root.join("export/ep/ep - voz 1 (Ana).wav");
        let before = wav_peak(&voice, 1.0, 4.0);
        let after = wav_peak(&voice, 7.0, 11.0);
        assert!(before > 0.1, "antes {before}");
        assert!(
            after < before * 0.13 && after > before * 0.07,
            "tras bajar 20 dB: {before} -> {after}"
        );

        // la envolvente de la mezcla completa también se oye en el mixdown (sin masterizar)
        let mut m = ExportSettings::new(s.take_dir.clone(), root.join("export"), "mez".into());
        m.mp3 = false;
        m.midi_file = false;
        m.voice_files = false;
        m.bus_comp = false;
        m.target_lufs = -16.0;
        m.automation.envelopes[ENV_MIX].insert(0.0, 0.0);
        m.automation.envelopes[ENV_MIX].insert(6.0, 0.0);
        m.automation.envelopes[ENV_MIX].insert(7.0, -18.0);
        let res = export_take(&m, &ExportProgress::default()).unwrap();
        assert!(res.output_lufs.is_finite());
        let mix = root.join("export/mez/mez - mixdown.wav");
        let loud = wav_peak(&mix, 1.0, 4.0);
        let quiet = wav_peak(&mix, 8.0, 11.0);
        assert!(quiet < loud * 0.3, "mezcla: {loud} -> {quiet}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ducking_is_recorded_as_an_editable_envelope() {
        use crate::engine::{Command, Engine, MusicParams, VoiceParams};
        use crate::recorder::{Recorder, StartRequest};
        use std::sync::Arc;

        let root = std::env::temp_dir().join(format!("podcaz-duck-test-{}", std::process::id()));
        let take = root.join("takes").join("t1");
        let sr = 48000;
        let (mut engine, remote) = Engine::new(sr);
        let rec = Recorder::spawn(remote.stems, remote.events, remote.meters.clone(), sr);

        let n = sr as usize * 10;
        let mut data = Vec::with_capacity(n * 2);
        for i in 0..n {
            let x = (i as f32 * 0.05).sin() * 0.5;
            data.push(x);
            data.push(x);
        }
        let mut cmds = remote.commands;
        let mut push = |c: Command| {
            cmds.push(c).ok().unwrap();
        };
        push(
                        Command::LoadMusic(Arc::new(AudioClip {
                name: "m".into(),
                sample_rate: sr,
                data,
            })),
        );
        push(
                        Command::SetMusic(MusicParams {
                volume_db: 0.0,
                duck_depth_db: -14.0,
                ..Default::default()
            }),
        );
        push(Command::PlayMusic);
        push(
                        Command::SetVoice(
                0,
                VoiceParams {
                    enabled: true,
                    preset: 0,
                    ..Default::default()
                },
            ),
        );
        rec.start(StartRequest {
            dir: take.clone(),
            voices: [true, false, false, false],
            voice_names: vec!["v".into(); 4],
            pad_notes: crate::default_pad_notes(36).to_vec(),
        })
        .unwrap();
        push(Command::StartRecording);

        // 3 s sin voz, 4 s con voz, 3 s sin voz (bloques de 480 frames)
        let quiet = vec![0f32; 480];
        let talk: Vec<f32> = (0..480).map(|i| (i as f32 * 0.1).sin() * 0.3).collect();
        let mut out = vec![0f32; 960];
        for block in 0..1000 {
            let inp = if (300..700).contains(&block) { &talk } else { &quiet };
            engine.process(inp, 1, &mut out, 2);
        }
        push(Command::StopRecording);
        engine.process(&quiet, 1, &mut out, 2);
        rec.stop();
        let (dir, info) = rec
            .finished
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert!(info.music_unducked);

        // music.wav lleva la música limpia: el pico no baja mientras habla la voz
        let music = dir.join("music.wav");
        let during = wav_peak(&music, 4.0, 6.0);
        let free = wav_peak(&music, 0.5, 2.0);
        assert!(during > free * 0.9, "la música grabada no debe llevar ducking: {during} vs {free}");

        // y la curva de ducking está guardada como envolvente de la música
        let auto = MixAutomation::load(&dir);
        let env = auto.env(STEM_MUSIC);
        assert!(!env.points.is_empty());
        assert!(env.points.len() < 40, "{} puntos", env.points.len());
        assert!(env.gain_db_at(1.0) > -1.0);
        assert!(env.gain_db_at(5.0) < -10.0, "{}", env.gain_db_at(5.0));
        assert!(env.gain_db_at(9.5) > -3.0, "se recupera: {}", env.gain_db_at(9.5));
        assert_eq!(MixAutomation::load_original(&dir), auto);

        // exportando con esa envolvente la música suena con ducking...
        let mut s = ExportSettings::new(dir.clone(), root.join("export"), "d".into());
        s.mp3 = false;
        s.midi_file = false;
        s.voice_files = false;
        s.bus_comp = false;
        s.fade_in_ms = 0.0;
        s.fade_out_ms = 0.0;
        s.target_lufs = -30.0; // sin compresión de nivel apreciable: comparamos relativos
        s.automation = auto.clone();
        s.stem_gain_db[0] = -80.0; // sin la voz, solo música
        export_take(&s, &ExportProgress::default()).unwrap();
        let mix = root.join("export/d/d - mixdown.wav");
        let free_x = wav_peak(&mix, 0.5, 2.0);
        let duck_x = wav_peak(&mix, 5.0, 6.0);
        assert!(duck_x < free_x * 0.35, "ducking al exportar: {free_x} -> {duck_x}");

        // ...y si el usuario lo anula, no
        s.base_name = "nd".into();
        s.automation.envelopes[STEM_MUSIC].clear();
        export_take(&s, &ExportProgress::default()).unwrap();
        let mix = root.join("export/nd/nd - mixdown.wav");
        let duck_x = wav_peak(&mix, 5.0, 6.0);
        let free_x = wav_peak(&mix, 0.5, 2.0);
        assert!(duck_x > free_x * 0.8, "sin ducking: {free_x} -> {duck_x}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn recorder_writes_a_complete_take() {
        use crate::engine::{Command, Engine, PadTrigger, VoiceParams};
        use crate::recorder::{Recorder, StartRequest};
        use std::sync::Arc;

        let root = std::env::temp_dir().join(format!("podcaz-rec-test-{}", std::process::id()));
        let take = root.join("takes").join("t1");
        let sr = 48000;
        let (mut engine, mut remote) = Engine::new(sr);
        let rec = Recorder::spawn(remote.stems, remote.events, remote.meters.clone(), sr);

        let clip = Arc::new(AudioClip {
            name: "s".into(),
            sample_rate: sr,
            data: vec![0.5; 9600],
        });
        remote
            .commands
            .push(Command::SetPad {
                index: 2,
                clip: Some(clip),
                gain_db: 0.0,
            })
            .ok()
            .unwrap();
        remote
            .commands
            .push(Command::SetVoice(
                1,
                VoiceParams {
                    enabled: true,
                    input_channel: 0,
                    preset: 0,
                    ..Default::default()
                },
            ))
            .ok()
            .unwrap();
        rec.start(StartRequest {
            dir: take.clone(),
            voices: [false, true, false, false],
            voice_names: vec!["a".into(); 4],
            // el pad 2 está mapeado a mano a la nota 60
            pad_notes: {
                let mut n = crate::default_pad_notes(36).to_vec();
                n[2] = 60;
                n
            },
        })
        .unwrap();
        remote.commands.push(Command::StartRecording).ok().unwrap();

        let input: Vec<f32> = (0..480).map(|i| (i as f32 * 0.1).sin() * 0.25).collect();
        let mut out = vec![0f32; 960];
        for block in 0..200 {
            if block == 100 {
                remote
                    .triggers
                    .push(PadTrigger {
                        index: 2,
                        velocity: 90,
                    })
                    .unwrap();
            }
            engine.process(&input, 1, &mut out, 2);
        }
        remote.commands.push(Command::StopRecording).ok().unwrap();
        engine.process(&input, 1, &mut out, 2);
        rec.stop();
        let (dir, info) = rec
            .finished
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(dir, take);
        assert_eq!(info.frames, 96_000);
        assert_eq!(info.pad_events, 1);
        assert_eq!(info.dropped_frames, 0);
        assert!(take.join("voice2.wav").is_file() && !take.join("voice1.wav").exists());
        assert_eq!(
            hound::WavReader::open(take.join("music.wav"))
                .unwrap()
                .duration(),
            96_000
        );
        let mid = std::fs::read(take.join("pads.mid")).unwrap();
        assert!(mid.windows(3).any(|w| w == [0x90, 60, 90]), "nota mapeada");
        assert_eq!(info.pad_notes[2], 60);
        assert!(take.join(PAD_EVENTS_FILE).is_file());
        assert!(take.join(crate::automation::MIX_FILE).is_file());
        assert!(take.join(crate::automation::MIX_AUTO_FILE).is_file());

        // y la toma se puede exportar
        let mut s = ExportSettings::new(take, root.join("export"), "x".into());
        s.mp3 = false;
        let res = export_take(&s, &ExportProgress::default()).unwrap();
        assert!((res.seconds - 2.0).abs() < 0.01);
        // mixdown + la voz 2 + midi
        assert_eq!(res.files.len(), 3);
        std::fs::remove_dir_all(&root).ok();
    }
}
