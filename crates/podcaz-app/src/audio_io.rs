//! Entrada/salida de audio con cpal (WASAPI o ASIO).
//!
//! El motor vive dentro del callback de salida. La entrada llega por una cola sin bloqueos,
//! lo que sirve tanto para ASIO (mismo reloj) como para WASAPI con dispositivos separados.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, FromSample, Sample, SampleFormat, SizedSample, StreamConfig};
use podcaz_core::engine::{Engine, EngineRemote};

#[derive(Default)]
pub struct StreamStatus {
    pub xruns: AtomicU64,
    pub last_error: Mutex<Option<String>>,
}

pub struct AudioRunning {
    // el orden importa: primero se detienen los streams
    _input: Option<cpal::Stream>,
    _output: cpal::Stream,
    pub sample_rate: u32,
    pub in_channels: usize,
    pub description: String,
    pub status: Arc<StreamStatus>,
}

pub fn host_names() -> Vec<String> {
    cpal::available_hosts()
        .into_iter()
        .map(|h| h.name().to_string())
        .collect()
}

pub fn is_asio(host_name: &str) -> bool {
    host_name.eq_ignore_ascii_case("asio")
}

fn find_host(name: &str) -> Result<cpal::Host> {
    for id in cpal::available_hosts() {
        if id.name() == name {
            return cpal::host_from_id(id)
                .map_err(|e| anyhow!("no se puede abrir el sistema de audio {name}: {e}"));
        }
    }
    if name.is_empty() {
        return Ok(cpal::default_host());
    }
    bail!("sistema de audio «{name}» no disponible en esta compilación")
}

fn device_name(d: &cpal::Device) -> String {
    d.description()
        .map(|x| x.name().to_string())
        .unwrap_or_else(|_| d.to_string())
}

/// Devuelve (entradas, salidas) del host. En ASIO cada driver sirve para ambas cosas.
pub fn list_devices(host_name: &str) -> Result<(Vec<String>, Vec<String>)> {
    let host = find_host(host_name)?;
    if is_asio(host_name) {
        let all: Vec<String> = host
            .devices()
            .map_err(|e| anyhow!("{e}"))?
            .map(|d| device_name(&d))
            .collect();
        return Ok((all.clone(), all));
    }
    let ins = host
        .input_devices()
        .map(|it| it.map(|d| device_name(&d)).collect())
        .unwrap_or_default();
    let outs = host
        .output_devices()
        .map(|it| it.map(|d| device_name(&d)).collect())
        .unwrap_or_default();
    Ok((ins, outs))
}

fn find_device(host: &cpal::Host, name: &str, input: bool) -> Result<cpal::Device> {
    if !name.is_empty() {
        if let Ok(devs) = host.devices() {
            for d in devs {
                if device_name(&d) == name {
                    return Ok(d);
                }
            }
        }
        bail!("no se encuentra el dispositivo «{name}»");
    }
    let d = if input {
        host.default_input_device()
    } else {
        host.default_output_device()
    };
    d.ok_or_else(|| {
        anyhow!(
            "no hay dispositivo de {} por defecto",
            if input { "entrada" } else { "salida" }
        )
    })
}

fn on_error(status: &Arc<StreamStatus>) -> impl FnMut(cpal::Error) + Send + 'static {
    let status = status.clone();
    move |err| match err.kind() {
        cpal::ErrorKind::Xrun => {
            status.xruns.fetch_add(1, Ordering::Relaxed);
        }
        cpal::ErrorKind::RealtimeDenied => {}
        _ => {
            if let Ok(mut g) = status.last_error.lock() {
                *g = Some(err.to_string());
            }
        }
    }
}

fn build_in<T>(
    dev: &cpal::Device,
    cfg: StreamConfig,
    mut prod: rtrb::Producer<f32>,
    status: &Arc<StreamStatus>,
) -> Result<cpal::Stream>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let stream = dev.build_input_stream(
        cfg,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            for &s in data {
                // si la cola está llena la salida va retrasada: se descarta
                let _ = prod.push(f32::from_sample(s));
            }
        },
        on_error(status),
        None,
    )?;
    Ok(stream)
}

struct OutputState {
    engine: Engine,
    input: rtrb::Consumer<f32>,
    in_channels: usize,
    out_channels: usize,
    in_buf: Vec<f32>,
    out_buf: Vec<f32>,
    max_lag: usize,
    keep_lag: usize,
}

impl OutputState {
    fn render(&mut self, frames: usize) {
        let need = frames * self.in_channels;
        if self.in_buf.len() < need {
            self.in_buf.resize(need, 0.0);
        }
        if self.out_buf.len() < frames * self.out_channels {
            self.out_buf.resize(frames * self.out_channels, 0.0);
        }
        let mut got = 0;
        if self.in_channels > 0 {
            // limitar la latencia acumulada si la entrada va adelantada
            let slots = self.input.slots();
            if slots > need + self.max_lag {
                let excess = (slots - need - self.keep_lag) / self.in_channels * self.in_channels;
                if let Ok(chunk) = self.input.read_chunk(excess) {
                    chunk.commit_all();
                }
            }
            got = self.input.slots().min(need) / self.in_channels * self.in_channels;
            if got > 0
                && self
                    .input
                    .pop_entire_slice(&mut self.in_buf[..got])
                    .is_err()
            {
                got = 0;
            }
        }
        let out = &mut self.out_buf[..frames * self.out_channels];
        self.engine.process(
            &self.in_buf[..got],
            self.in_channels,
            out,
            self.out_channels,
        );
    }
}

fn build_out<T>(
    dev: &cpal::Device,
    cfg: StreamConfig,
    mut st: OutputState,
    status: &Arc<StreamStatus>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let stream = dev.build_output_stream(
        cfg,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let frames = data.len() / st.out_channels;
            st.render(frames);
            for (o, &x) in data.iter_mut().zip(st.out_buf.iter()) {
                *o = T::from_sample(x);
            }
        },
        on_error(status),
        None,
    )?;
    Ok(stream)
}

macro_rules! by_format {
    ($fmt:expr, $func:ident, $($arg:expr),*) => {
        match $fmt {
            SampleFormat::I8 => $func::<i8>($($arg),*),
            SampleFormat::I16 => $func::<i16>($($arg),*),
            SampleFormat::I24 => $func::<cpal::I24>($($arg),*),
            SampleFormat::I32 => $func::<i32>($($arg),*),
            SampleFormat::I64 => $func::<i64>($($arg),*),
            SampleFormat::U8 => $func::<u8>($($arg),*),
            SampleFormat::U16 => $func::<u16>($($arg),*),
            SampleFormat::U32 => $func::<u32>($($arg),*),
            SampleFormat::U64 => $func::<u64>($($arg),*),
            SampleFormat::F32 => $func::<f32>($($arg),*),
            SampleFormat::F64 => $func::<f64>($($arg),*),
            other => Err(anyhow!("formato de muestra no soportado: {other}")),
        }
    };
}

/// Abre los dispositivos y arranca el motor.
///
/// `input_name` / `output_name` vacíos = dispositivo por defecto. `buffer_frames` 0 = el del driver.
pub fn start(
    host_name: &str,
    input_name: &str,
    output_name: &str,
    buffer_frames: u32,
) -> Result<(AudioRunning, EngineRemote)> {
    let host = find_host(host_name)?;
    let asio = is_asio(host_name);
    // En ASIO solo puede haber un driver cargado: entrada y salida usan el mismo.
    let out_name = if asio { input_name } else { output_name };
    let out_dev = find_device(&host, out_name, false).context("salida")?;
    let out_sup = out_dev
        .default_output_config()
        .map_err(|e| anyhow!("la salida no ofrece configuración: {e}"))?;
    let sample_rate = out_sup.sample_rate();
    let out_channels = out_sup.channels() as usize;
    let buffer_size = if buffer_frames > 0 {
        BufferSize::Fixed(buffer_frames)
    } else {
        BufferSize::Default
    };

    let status = Arc::new(StreamStatus::default());
    let (engine, remote) = Engine::new(sample_rate);

    // ---- entrada (opcional: sin entrada se puede seguir usando música y sampler)
    let mut input_stream = None;
    let mut in_channels = 0usize;
    let mut in_desc = "sin entrada".to_string();
    let mut in_cons = rtrb::RingBuffer::<f32>::new(8).1;
    let in_dev = find_device(&host, input_name, true);
    let mut in_warning = None;
    match in_dev {
        Ok(dev) => {
            let pick = || -> Result<cpal::SupportedStreamConfig> {
                let def = dev
                    .default_input_config()
                    .map_err(|e| anyhow!("la entrada no ofrece configuración: {e}"))?;
                if def.sample_rate() == sample_rate {
                    return Ok(def);
                }
                // buscar una configuración a la frecuencia de la salida, con el máximo de canales
                let mut best: Option<cpal::SupportedStreamConfig> = None;
                for range in dev.supported_input_configs().map_err(|e| anyhow!("{e}"))? {
                    if range.min_sample_rate() <= sample_rate
                        && sample_rate <= range.max_sample_rate()
                    {
                        let c = range.with_sample_rate(sample_rate);
                        if best.as_ref().map_or(true, |b| c.channels() > b.channels()) {
                            best = Some(c);
                        }
                    }
                }
                best.ok_or_else(|| {
                    anyhow!(
                        "la entrada trabaja a {} Hz y la salida a {} Hz: pon ambos dispositivos a la misma frecuencia",
                        def.sample_rate(),
                        sample_rate
                    )
                })
            };
            match pick() {
                Ok(sup) => {
                    in_channels = sup.channels() as usize;
                    let cfg = StreamConfig {
                        channels: sup.channels(),
                        sample_rate,
                        buffer_size,
                    };
                    let (prod, cons) =
                        rtrb::RingBuffer::<f32>::new(sample_rate as usize * in_channels.max(1));
                    in_cons = cons;
                    let s = by_format!(sup.sample_format(), build_in, &dev, cfg, prod, &status)
                        .context("no se pudo abrir la entrada")?;
                    in_desc = format!("{} ({} can.)", device_name(&dev), in_channels);
                    input_stream = Some(s);
                }
                Err(e) => in_warning = Some(format!("{e:#}")),
            }
        }
        Err(e) => in_warning = Some(format!("{e:#}")),
    }

    // ---- salida
    let cfg = StreamConfig {
        channels: out_sup.channels(),
        sample_rate,
        buffer_size,
    };
    let st = OutputState {
        engine,
        input: in_cons,
        in_channels,
        out_channels,
        in_buf: vec![0.0; 8192 * in_channels.max(1)],
        out_buf: vec![0.0; 8192 * out_channels.max(1)],
        max_lag: (sample_rate as usize / 20) * in_channels, // 50 ms
        keep_lag: (sample_rate as usize / 100) * in_channels, // 10 ms
    };
    let output_stream = by_format!(
        out_sup.sample_format(),
        build_out,
        &out_dev,
        cfg,
        st,
        &status
    )
    .context("no se pudo abrir la salida")?;

    if let Some(s) = &input_stream {
        s.play()
            .map_err(|e| anyhow!("no arranca la entrada: {e}"))?;
    }
    output_stream
        .play()
        .map_err(|e| anyhow!("no arranca la salida: {e}"))?;

    if let Some(w) = in_warning {
        *status.last_error.lock().unwrap() = Some(format!("Sin entrada de audio: {w}"));
    }
    let description = format!(
        "{} Hz · entrada: {} · salida: {} ({} can.)",
        sample_rate,
        in_desc,
        device_name(&out_dev),
        out_channels
    );
    Ok((
        AudioRunning {
            _input: input_stream,
            _output: output_stream,
            sample_rate,
            in_channels,
            description,
            status,
        },
        remote,
    ))
}
