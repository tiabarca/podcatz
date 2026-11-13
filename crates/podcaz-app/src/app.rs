//! Interfaz (egui) y lógica de aplicación.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Color32, Key, RichText};
use midir::MidiInputConnection;
use podcaz_core::decode::{load_clip, AUDIO_EXTENSIONS};
use podcaz_core::dsp::lin_to_db;
use podcaz_core::engine::{AudioClip, Command, Meters};
use podcaz_core::export::{
    self, Envelope, ExportProgress, ExportResult, ExportSettings, MixAutomation,
    NUM_ENVELOPES, NUM_STEMS, STEM_MUSIC, STEM_SAMPLER,
};
use podcaz_core::master::LOUDNESS_TARGETS;
use podcaz_core::presets::PRESETS;
use podcaz_core::recorder::{Recorder, StartRequest, TakeInfo};
use podcaz_core::session::{list_takes, Session};
use podcaz_core::{MAX_VOICES, NUM_PADS};

use crate::audio_io::{self, AudioRunning};
use crate::midi_in::{self, MidiShared, TriggerSink};
use crate::theme::{self, Mode};

const PAD_KEYS: [(Key, &str); NUM_PADS] = [
    (Key::Num1, "1"),
    (Key::Num2, "2"),
    (Key::Num3, "3"),
    (Key::Num4, "4"),
    (Key::Q, "Q"),
    (Key::W, "W"),
    (Key::E, "E"),
    (Key::R, "R"),
    (Key::A, "A"),
    (Key::S, "S"),
    (Key::D, "D"),
    (Key::F, "F"),
    (Key::Z, "Z"),
    (Key::X, "X"),
    (Key::C, "C"),
    (Key::V, "V"),
];

const BUFFER_SIZES: [u32; 6] = [0, 64, 128, 256, 512, 1024];
const RED: Color32 = Color32::from_rgb(200, 50, 50);
const GREEN: Color32 = Color32::from_rgb(70, 170, 90);
const AMBER: Color32 = Color32::from_rgb(220, 170, 50);

struct Audio {
    // orden de destrucción: primero los streams, luego el grabador
    running: AudioRunning,
    cmd: rtrb::Producer<Command>,
    trash: rtrb::Consumer<Arc<AudioClip>>,
    meters: Arc<Meters>,
    recorder: Recorder,
}

enum Loaded {
    Music {
        index: usize,
        generation: u64,
        clip: Result<AudioClip, String>,
    },
    Pad {
        index: usize,
        generation: u64,
        clip: Result<AudioClip, String>,
    },
    Preview(Result<AudioClip, String>),
    /// Forma de onda de una pista para el editor de envolventes.
    Peaks {
        take: PathBuf,
        track: usize,
        peaks: Result<Vec<f32>, String>,
    },
}

#[derive(Default)]
struct Levels {
    voice: [f32; MAX_VOICES],
    music: f32,
    sampler: f32,
    master: [f32; 2],
}

/// Zona visible del editor y punto que se está tocando.
struct EnvView {
    start: f64,
    span: f64,
    selected: Option<usize>,
    dragging: Option<usize>,
}

/// Estado del editor de subidas y bajadas de volumen.
struct EnvEditor {
    open: bool,
    /// Pista que se edita: 0..NUM_ENVELOPES (voces, música, sampler, mezcla completa).
    track: usize,
    view: EnvView,
    /// Forma de onda por pista (picos cada 20 ms), de la toma seleccionada.
    peaks: HashMap<usize, Vec<f32>>,
    peaks_loading: HashSet<usize>,
}

impl Default for EnvEditor {
    fn default() -> Self {
        Self {
            open: false,
            track: STEM_MUSIC,
            view: EnvView {
                start: 0.0,
                span: 60.0,
                selected: None,
                dragging: None,
            },
            peaks: HashMap::new(),
            peaks_loading: HashSet::new(),
        }
    }
}

struct ExportUi {
    open: bool,
    takes: Vec<PathBuf>,
    selected: usize,
    info: Option<TakeInfo>,
    /// Subidas y bajadas de la toma seleccionada (se guardan en `mix.json` de la toma).
    auto: MixAutomation,
    /// Carpeta de la toma de la que se leyó `auto` (y donde se guarda).
    auto_take: Option<PathBuf>,
    auto_dirty: bool,
    editor: EnvEditor,
    mixdown: bool,
    voice_files: bool,
    midi_file: bool,
    trim_start: f64,
    trim_end: f64,
    fade_out_ms: f32,
    gains: [f32; NUM_STEMS],
    target: usize,
    bus_comp: bool,
    wav: bool,
    wav16: bool,
    mp3: bool,
    kbps: u32,
    name: String,
    title: String,
    artist: String,
    progress: Option<Arc<ExportProgress>>,
    rx: Option<Receiver<Result<ExportResult, String>>>,
    result: Option<Result<ExportResult, String>>,
}

impl Default for ExportUi {
    fn default() -> Self {
        Self {
            open: false,
            takes: Vec::new(),
            selected: 0,
            info: None,
            auto: MixAutomation::default(),
            auto_take: None,
            auto_dirty: false,
            editor: EnvEditor::default(),
            mixdown: true,
            voice_files: true,
            midi_file: true,
            trim_start: 0.0,
            trim_end: 0.0,
            fade_out_ms: 500.0,
            gains: [0.0; NUM_STEMS],
            target: 0,
            bus_comp: true,
            wav: true,
            wav16: false,
            mp3: export::mp3_available(),
            kbps: 192,
            name: String::new(),
            title: String::new(),
            artist: String::new(),
            progress: None,
            rx: None,
            result: None,
        }
    }
}

pub struct App {
    session: Session,
    session_dir: PathBuf,

    hosts: Vec<String>,
    in_devs: Vec<String>,
    out_devs: Vec<String>,
    audio: Option<Audio>,

    triggers: TriggerSink,
    midi: Option<MidiInputConnection<()>>,
    midi_ports: Vec<String>,
    midi_shared: Arc<MidiShared>,
    midi_seen: u32,
    midi_flash: f32,
    /// Pad que está esperando una nota MIDI («aprender»).
    learning_pad: Option<usize>,

    loader_tx: Sender<Loaded>,
    loader_rx: Receiver<Loaded>,
    generation: u64,
    music_gen: u64,

    music_selected: usize,
    music_current: Option<usize>,
    music_loading: bool,
    pad_loaded: [bool; NUM_PADS],
    pad_loading: [bool; NUM_PADS],

    levels: Levels,
    recording: bool,
    saving_take: bool,
    status: String,
    status_is_error: bool,
    export: ExportUi,

    theme: Mode,
    logo: egui::TextureHandle,
}

// ------------------------------------------------------------------ utilidades

fn config_file() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("PODCAZ").join("last_session.txt"))
}

fn default_sessions_root() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Documents").join("PODCAZ")
}

fn file_label(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| p.display().to_string())
}

fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

fn sanitize(name: &str) -> String {
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
    let s = s.trim().trim_matches('.').to_string();
    if s.is_empty() {
        "episodio".into()
    } else {
        s
    }
}

fn meter(ui: &mut egui::Ui, level: f32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 10.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, theme::pal(ui).meter_bg);
    let db = lin_to_db(level).clamp(-60.0, 0.0);
    let frac = (db + 60.0) / 60.0;
    if frac > 0.0 {
        let color = if db > -3.0 {
            RED
        } else if db > -12.0 {
            AMBER
        } else {
            GREEN
        };
        let mut r = rect;
        r.set_width(rect.width() * frac);
        painter.rect_filled(r, 2.0, color);
    }
}

fn db_slider(ui: &mut egui::Ui, value: &mut f32, range: std::ops::RangeInclusive<f32>) -> bool {
    ui.add(
        egui::Slider::new(value, range)
            .suffix(" dB")
            .fixed_decimals(1),
    )
    .changed()
}

// ------------------------------------------------------------------ lógica

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let theme_mode = theme::load_mode();
        theme::install(&cc.egui_ctx, theme_mode);
        let logo = theme::icon_texture(&cc.egui_ctx, 96);
        let (loader_tx, loader_rx) = channel();
        let mut app = App {
            session: Session::default(),
            session_dir: default_sessions_root().join("Mi podcast"),
            hosts: audio_io::host_names(),
            in_devs: Vec::new(),
            out_devs: Vec::new(),
            audio: None,
            triggers: Arc::new(Mutex::new(None)),
            midi: None,
            midi_ports: midi_in::ports(),
            midi_shared: Arc::new(MidiShared::default()),
            midi_seen: 0,
            midi_flash: 0.0,
            learning_pad: None,
            loader_tx,
            loader_rx,
            generation: 0,
            music_gen: 0,
            music_selected: 0,
            music_current: None,
            music_loading: false,
            pad_loaded: [false; NUM_PADS],
            pad_loading: [false; NUM_PADS],
            levels: Levels::default(),
            recording: false,
            saving_take: false,
            status: "Elige dispositivo y pulsa «Iniciar audio».".into(),
            status_is_error: false,
            export: ExportUi::default(),
            theme: theme_mode,
            logo,
        };
        // última sesión usada
        if let Some(dir) = config_file()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .map(|s| PathBuf::from(s.trim()))
        {
            if dir.join("session.json").is_file() {
                match Session::load(&dir) {
                    Ok(s) => {
                        app.session = s;
                        app.session_dir = dir;
                    }
                    Err(e) => app.error(format!("No se pudo abrir la última sesión: {e:#}")),
                }
            }
        }
        app.after_session_change();
        app
    }

    fn info(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_is_error = false;
    }

    fn error(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_is_error = true;
    }

    fn push(&mut self, cmd: Command) {
        if let Some(a) = self.audio.as_mut() {
            let _ = a.cmd.push(cmd);
        }
    }

    fn sample_rate(&self) -> Option<u32> {
        self.audio.as_ref().map(|a| a.running.sample_rate)
    }

    fn refresh_devices(&mut self) {
        self.hosts = audio_io::host_names();
        if !self.hosts.contains(&self.session.audio.host) {
            // preferir ASIO si la compilación lo incluye
            self.session.audio.host = self
                .hosts
                .iter()
                .find(|h| audio_io::is_asio(h))
                .or(self.hosts.first())
                .cloned()
                .unwrap_or_default();
        }
        match audio_io::list_devices(&self.session.audio.host) {
            Ok((i, o)) => {
                self.in_devs = i;
                self.out_devs = o;
            }
            Err(e) => {
                self.in_devs.clear();
                self.out_devs.clear();
                self.error(format!("No se pueden listar dispositivos: {e:#}"));
            }
        }
        self.midi_ports = midi_in::ports();
    }

    /// Tras cargar/crear una sesión: refresca listas y reenvía todo al motor.
    fn after_session_change(&mut self) {
        self.session.normalize();
        self.cancel_learning();
        self.rebuild_midi_map();
        self.refresh_devices();
        self.music_selected = 0;
        self.music_current = None;
        self.generation += 1;
        self.music_gen += 1;
        self.music_loading = false;
        self.connect_midi();
        self.sync_engine();
        self.refresh_takes();
        if let Some(f) = config_file() {
            if let Some(parent) = f.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(f, self.session_dir.display().to_string());
        }
    }

    fn connect_midi(&mut self) {
        self.midi = None;
        let port = self.session.audio.midi_port.clone();
        if port.is_empty() {
            return;
        }
        match midi_in::connect(&port, self.triggers.clone(), self.midi_shared.clone()) {
            Ok(c) => self.midi = Some(c),
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    fn save_session(&mut self) -> bool {
        match self.session.save(&self.session_dir) {
            Ok(()) => true,
            Err(e) => {
                self.error(format!("No se pudo guardar la sesión: {e:#}"));
                false
            }
        }
    }

    fn send_voice(&mut self, i: usize) {
        let p = self.session.voices[i].params();
        self.push(Command::SetVoice(i, p));
    }

    fn send_music(&mut self) {
        let p = self.session.music.params();
        self.push(Command::SetMusic(p));
    }

    fn send_master(&mut self) {
        let (g, m) = (self.session.master_gain_db, self.session.monitor_voices);
        self.push(Command::SetMaster {
            gain_db: g,
            monitor_voices: m,
        });
    }

    /// Envía al motor el estado completo de la sesión (tras arrancar el audio o abrir sesión).
    fn sync_engine(&mut self) {
        if self.audio.is_none() {
            return;
        }
        for i in 0..MAX_VOICES {
            self.send_voice(i);
        }
        self.send_music();
        self.send_master();
        let v = self.session.sampler_volume_db;
        self.push(Command::SetSamplerVolume(v));
        self.push(Command::StopMusic);
        self.pad_loaded = [false; NUM_PADS];
        for i in 0..NUM_PADS {
            self.load_pad(i);
        }
    }

    fn load_pad(&mut self, index: usize) {
        let Some(sr) = self.sample_rate() else { return };
        let gain = self.session.pads[index].gain_db;
        match self.session.pads[index].path.clone() {
            None => {
                self.pad_loaded[index] = false;
                self.push(Command::SetPad {
                    index,
                    clip: None,
                    gain_db: gain,
                });
            }
            Some(path) => {
                self.pad_loading[index] = true;
                let tx = self.loader_tx.clone();
                let generation = self.generation;
                std::thread::spawn(move || {
                    let clip =
                        load_clip(&path, sr).map_err(|e| format!("{}: {e:#}", path.display()));
                    let _ = tx.send(Loaded::Pad {
                        index,
                        generation,
                        clip,
                    });
                });
            }
        }
    }

    fn play_music(&mut self, index: usize) {
        let Some(sr) = self.sample_rate() else {
            self.error("Inicia el audio antes de reproducir música.");
            return;
        };
        let Some(path) = self.session.music.setlist.get(index).cloned() else {
            return;
        };
        self.music_selected = index;
        self.music_loading = true;
        self.music_gen += 1;
        let generation = self.music_gen;
        let tx = self.loader_tx.clone();
        std::thread::spawn(move || {
            let clip = load_clip(&path, sr).map_err(|e| format!("{}: {e:#}", path.display()));
            let _ = tx.send(Loaded::Music {
                index,
                generation,
                clip,
            });
        });
    }

    fn start_audio(&mut self) {
        self.stop_audio();
        let a = self.session.audio.clone();
        match audio_io::start(&a.host, &a.input_device, &a.output_device, a.buffer_frames) {
            Ok((running, remote)) => {
                let recorder = Recorder::spawn(
                    remote.stems,
                    remote.events,
                    remote.meters.clone(),
                    remote.sample_rate,
                );
                *self.triggers.lock().unwrap() = Some(remote.triggers);
                let desc = running.description.clone();
                self.audio = Some(Audio {
                    running,
                    cmd: remote.commands,
                    trash: remote.trash,
                    meters: remote.meters,
                    recorder,
                });
                self.sync_engine();
                self.info(format!("Audio en marcha · {desc}"));
            }
            Err(e) => self.error(format!("No se pudo iniciar el audio: {e:#}")),
        }
    }

    fn stop_audio(&mut self) {
        if self.recording {
            self.stop_recording();
        }
        *self.triggers.lock().unwrap() = None;
        if let Some(audio) = self.audio.take() {
            let finished = if self.saving_take {
                // dar tiempo a que el motor confirme la parada antes de cerrar los streams
                std::thread::sleep(std::time::Duration::from_millis(150));
                audio
                    .recorder
                    .finished
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .ok()
            } else {
                None
            };
            drop(audio);
            if let Some(res) = finished {
                self.take_finished(res);
            }
            self.saving_take = false;
        }
        self.music_current = None;
        self.music_loading = false;
        self.pad_loaded = [false; NUM_PADS];
        self.levels = Levels::default();
    }

    fn start_recording(&mut self) {
        if self.audio.is_none() {
            self.error("Inicia el audio antes de grabar.");
            return;
        }
        if self.saving_take {
            self.error("Espera a que termine de guardarse la toma anterior.");
            return;
        }
        if !self.save_session() {
            return;
        }
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let dir = self.session_dir.join("takes").join(stamp);
        let mut voices = [false; MAX_VOICES];
        for (i, v) in self.session.voices.iter().enumerate() {
            voices[i] = v.enabled && i < self.session.num_voices;
        }
        let req = StartRequest {
            dir,
            voices,
            voice_names: self.session.voices.iter().map(|v| v.name.clone()).collect(),
            pad_notes: self.session.pad_notes(),
        };
        let res = self.audio.as_ref().unwrap().recorder.start(req);
        match res {
            Ok(()) => {
                self.push(Command::StartRecording);
                self.recording = true;
                self.info("Grabando…");
            }
            Err(e) => self.error(format!("No se pudo empezar a grabar: {e:#}")),
        }
    }

    fn stop_recording(&mut self) {
        if !self.recording {
            return;
        }
        self.push(Command::StopRecording);
        if let Some(a) = self.audio.as_ref() {
            a.recorder.stop();
        }
        self.recording = false;
        self.saving_take = true;
        self.info("Guardando la toma…");
    }

    fn take_finished(&mut self, res: Result<(PathBuf, TakeInfo), String>) {
        self.saving_take = false;
        match res {
            Ok((dir, info)) => {
                let mut msg = format!(
                    "Toma guardada ({}) en {}",
                    fmt_time(info.seconds()),
                    dir.display()
                );
                if info.dropped_frames > 0 {
                    msg += &format!(
                        " · AVISO: el disco no dio abasto y se perdieron {:.2} s",
                        info.dropped_frames as f64 / info.sample_rate as f64
                    );
                    self.error(msg);
                } else {
                    self.info(msg);
                }
                self.refresh_takes();
                self.export.selected = 0;
                self.select_take();
            }
            Err(e) => self.error(format!("Error al guardar la toma: {e}")),
        }
    }

    fn refresh_takes(&mut self) {
        self.export.takes = list_takes(&self.session_dir);
        if self.export.selected >= self.export.takes.len() {
            self.export.selected = 0;
        }
        self.select_take();
    }

    fn select_take(&mut self) {
        self.save_automation(); // lo pendiente de la toma anterior
        let e = &mut self.export;
        e.info = e.takes.get(e.selected).and_then(|d| TakeInfo::load(d).ok());
        e.auto = e
            .takes
            .get(e.selected)
            .map(|d| MixAutomation::load(d))
            .unwrap_or_default();
        e.auto_take = e.takes.get(e.selected).cloned();
        e.auto_dirty = false;
        e.editor.peaks.clear();
        e.editor.peaks_loading.clear();
        e.editor.view = EnvView {
            start: 0.0,
            span: e
                .info
                .as_ref()
                .map(|i| i.seconds())
                .unwrap_or(60.0)
                .clamp(10.0, 120.0),
            selected: None,
            dragging: None,
        };
        e.trim_start = 0.0;
        e.trim_end = 0.0;
        e.result = None;
        if let Some(dir) = e.takes.get(e.selected) {
            let stamp = dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            e.name = sanitize(&format!("{} {}", self.session.name, stamp));
            if e.title.is_empty() {
                e.title = self.session.name.clone();
            }
        }
    }

    fn export_settings(&self) -> Option<ExportSettings> {
        let e = &self.export;
        let take = e.takes.get(e.selected)?.clone();
        let mut s = ExportSettings::new(
            take,
            self.session_dir.join("export"),
            e.name.trim().to_string(),
        );
        s.trim_start_s = e.trim_start;
        s.trim_end_s = e.trim_end;
        s.fade_out_ms = e.fade_out_ms;
        s.stem_gain_db = e.gains;
        s.automation = e.auto.clone();
        s.mixdown = e.mixdown;
        s.voice_files = e.voice_files;
        s.midi_file = e.midi_file;
        s.target_lufs = LOUDNESS_TARGETS[e.target.min(LOUDNESS_TARGETS.len() - 1)].0;
        s.bus_comp = e.bus_comp;
        s.wav = e.wav;
        s.wav_bits = if e.wav16 { 16 } else { 24 };
        s.mp3 = e.mp3 && export::mp3_available();
        s.mp3_kbps = e.kbps;
        s.title = e.title.clone();
        s.artist = e.artist.clone();
        Some(s)
    }

    fn start_export(&mut self) {
        self.save_automation();
        let Some(settings) = self.export_settings() else {
            return;
        };
        let progress = Arc::new(ExportProgress::default());
        let (tx, rx) = channel();
        let p = progress.clone();
        std::thread::spawn(move || {
            let res = export::export_take(&settings, &p).map_err(|e| format!("{e:#}"));
            let _ = tx.send(res);
        });
        self.export.progress = Some(progress);
        self.export.rx = Some(rx);
        self.export.result = None;
    }

    fn start_preview(&mut self, from_end: bool) {
        let Some(sr) = self.sample_rate() else {
            self.error("Inicia el audio para poder escuchar la toma.");
            return;
        };
        let Some(info) = self.export.info.clone() else {
            return;
        };
        let Some(dir) = self.export.takes.get(self.export.selected).cloned() else {
            return;
        };
        let dur = 8.0;
        let total = info.seconds();
        let start = if from_end {
            (total - self.export.trim_end - dur).max(self.export.trim_start)
        } else {
            self.export.trim_start
        };
        let len = if from_end {
            (total - self.export.trim_end - start).max(0.0)
        } else {
            dur
        };
        self.spawn_preview(dir, start, len, sr);
    }

    /// Prepara en segundo plano un fragmento de la toma con los volúmenes y envolventes actuales.
    fn spawn_preview(&mut self, dir: PathBuf, start: f64, len: f64, sr: u32) {
        let gains = self.export.gains;
        let auto = self.export.auto.clone();
        let tx = self.loader_tx.clone();
        std::thread::spawn(move || {
            let clip = export::render_preview(&dir, &gains, &auto, start, len, sr)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Loaded::Preview(clip));
        });
    }

    /// Trabajo por frame: resultados de hilos, medidores, teclado.
    fn poll(&mut self, ctx: &egui::Context) {
        // clips cargados en segundo plano
        while let Ok(l) = self.loader_rx.try_recv() {
            match l {
                Loaded::Music {
                    index,
                    generation,
                    clip,
                } => {
                    if generation != self.music_gen {
                        continue; // el usuario eligió otra cosa mientras cargaba
                    }
                    self.music_loading = false;
                    match clip {
                        Ok(c) => {
                            self.push(Command::LoadMusic(Arc::new(c)));
                            self.push(Command::PlayMusic);
                            self.music_current = Some(index);
                        }
                        Err(e) => self.error(format!("No se pudo cargar la música: {e}")),
                    }
                }
                Loaded::Pad {
                    index,
                    generation,
                    clip,
                } => {
                    self.pad_loading[index] = false;
                    if generation != self.generation {
                        continue;
                    }
                    match clip {
                        Ok(c) => {
                            let gain_db = self.session.pads[index].gain_db;
                            self.push(Command::SetPad {
                                index,
                                clip: Some(Arc::new(c)),
                                gain_db,
                            });
                            self.pad_loaded[index] = true;
                        }
                        Err(e) => {
                            self.pad_loaded[index] = false;
                            self.error(format!("Pad {}: {e}", index + 1));
                        }
                    }
                }
                Loaded::Preview(clip) => match clip {
                    Ok(c) => self.push(Command::PlayPreview(Arc::new(c))),
                    Err(e) => self.error(format!("No se pudo preparar la escucha: {e}")),
                },
                Loaded::Peaks { take, track, peaks } => {
                    // se descarta si mientras tanto se eligió otra toma
                    if self.export.takes.get(self.export.selected) == Some(&take) {
                        self.export.editor.peaks_loading.remove(&track);
                        match peaks {
                            Ok(p) => {
                                self.export.editor.peaks.insert(track, p);
                            }
                            Err(e) => {
                                self.export.editor.peaks.insert(track, Vec::new());
                                self.error(format!("No se pudo leer la forma de onda: {e}"));
                            }
                        }
                    }
                }
            }
        }

        // estado del audio
        let mut finished = None;
        let mut stream_error = None;
        if let Some(a) = self.audio.as_mut() {
            while a.trash.pop().is_ok() {} // liberar clips fuera del hilo de audio
            if let Ok(res) = a.recorder.finished.try_recv() {
                finished = Some(res);
            }
            if let Ok(mut g) = a.running.status.last_error.try_lock() {
                stream_error = g.take();
            }
            let m = &a.meters;
            let decay = |old: f32, new: f32| new.max(old * 0.86);
            for v in 0..MAX_VOICES {
                self.levels.voice[v] =
                    decay(self.levels.voice[v], Meters::take_peak(&m.voice_peak[v]));
            }
            self.levels.music = decay(self.levels.music, Meters::take_peak(&m.music_peak));
            self.levels.sampler = decay(self.levels.sampler, Meters::take_peak(&m.sampler_peak));
            for c in 0..2 {
                self.levels.master[c] =
                    decay(self.levels.master[c], Meters::take_peak(&m.master_peak[c]));
            }
        }
        if let Some(res) = finished {
            self.take_finished(res);
        }
        if let Some(e) = stream_error {
            self.error(format!("Audio: {e}"));
        }

        // exportación
        let export_done = self.export.rx.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(res) = export_done {
            self.export.rx = None;
            self.export.progress = None;
            match &res {
                Ok(r) => self.info(format!(
                    "Exportado: {}",
                    r.files
                        .iter()
                        .map(|f| f.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                Err(e) => self.error(format!("Error al exportar: {e}")),
            }
            self.export.result = Some(res);
        }

        // nota MIDI aprendida para un pad
        if let Some((pad, note)) = self.midi_shared.take_learned() {
            self.learning_pad = None;
            // una nota solo puede disparar un pad: se la quitamos a quien la tuviera asignada
            for (j, p) in self.session.pads.iter_mut().enumerate() {
                if j != pad && p.midi_note == Some(note) {
                    p.midi_note = None;
                }
            }
            self.session.pads[pad].midi_note = Some(note);
            self.rebuild_midi_map();
            self.info(format!(
                "Pad {} → nota MIDI {} ({})",
                pad + 1,
                note,
                note_name(note)
            ));
        }
        if self.learning_pad.is_some() && ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.cancel_learning();
            self.info("Aprendizaje MIDI cancelado.");
        }

        // las subidas y bajadas editadas se guardan al soltar el ratón
        if self.export.auto_dirty && !ctx.input(|i| i.pointer.any_down()) {
            self.save_automation();
        }

        // actividad MIDI
        let act = self.midi_shared.activity.load(Ordering::Relaxed);
        if act != self.midi_seen {
            self.midi_seen = act;
            self.midi_flash = 1.0;
        } else {
            self.midi_flash *= 0.9;
        }

        // teclado QWERTY -> pads (solo si no se está escribiendo en un campo de texto)
        if !ctx.egui_wants_keyboard_input() && self.audio.is_some() {
            let pressed: Vec<usize> = ctx.input(|i| {
                if i.modifiers.any() {
                    return Vec::new();
                }
                PAD_KEYS
                    .iter()
                    .enumerate()
                    .filter(|(_, (k, _))| i.key_pressed(*k))
                    .map(|(n, _)| n)
                    .collect()
            });
            for index in pressed {
                self.push(Command::TriggerPad {
                    index,
                    velocity: 110,
                });
            }
            if ctx.input(|i| i.key_pressed(Key::Escape)) {
                self.push(Command::StopPads);
            }
        }

        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }
}

// ------------------------------------------------------------------ interfaz

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            self.ui_session_bar(ui);
            ui.separator();
            self.ui_audio_bar(ui);
            ui.separator();
            self.ui_transport(ui);
            ui.add_space(4.0);
        });

        egui::Panel::bottom("bottom")
            .resizable(true)
            .default_size(310.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                ui.columns(2, |cols| {
                    self.ui_setlist(&mut cols[0]);
                    self.ui_pads(&mut cols[1]);
                });
            });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::horizontal().show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    for i in 0..self.session.num_voices {
                        self.ui_voice(ui, i);
                    }
                    self.ui_music_strip(ui);
                    self.ui_master_strip(ui);
                });
            });
        });

        self.ui_export(&ctx);
        self.ui_env_editor(&ctx);
    }

    fn on_exit(&mut self) {
        self.stop_audio();
        let _ = self.session.save(&self.session_dir);
    }
}

impl App {
    fn ui_session_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add(egui::Image::new((self.logo.id(), egui::vec2(30.0, 30.0))));
            ui.label(RichText::new("PODCAZ").size(20.0).strong().color(self.accent(ui)));
            ui.separator();
            ui.label("Sesión:");
            ui.add(egui::TextEdit::singleline(&mut self.session.name).desired_width(180.0));
            let busy = self.recording || self.saving_take;
            if ui
                .add_enabled(!busy, egui::Button::new("Nueva…"))
                .on_hover_text("Elige (o crea) una carpeta vacía para la sesión")
                .clicked()
            {
                let root = default_sessions_root();
                let _ = std::fs::create_dir_all(&root);
                if let Some(dir) = rfd::FileDialog::new()
                    .set_title("Carpeta para la nueva sesión")
                    .set_directory(&root)
                    .pick_folder()
                {
                    if dir.join("session.json").exists() {
                        self.error("Esa carpeta ya contiene una sesión: usa «Abrir…».");
                    } else {
                        let audio = self.session.audio.clone();
                        self.session = Session {
                            name: file_label(&dir),
                            audio,
                            ..Session::default()
                        };
                        self.session_dir = dir;
                        if self.save_session() {
                            self.after_session_change();
                            self.info("Sesión creada.");
                        }
                    }
                }
            }
            if ui.add_enabled(!busy, egui::Button::new("Abrir…")).clicked() {
                let root = default_sessions_root();
                if let Some(dir) = rfd::FileDialog::new()
                    .set_title("Carpeta de la sesión")
                    .set_directory(&root)
                    .pick_folder()
                {
                    match Session::load(&dir) {
                        Ok(s) => {
                            self.session = s;
                            self.session_dir = dir;
                            self.after_session_change();
                            self.info("Sesión abierta.");
                        }
                        Err(e) => self.error(format!("{e:#}")),
                    }
                }
            }
            if ui.button("Guardar").clicked() && self.save_session() {
                self.info(format!("Sesión guardada en {}", self.session_dir.display()));
            }
            ui.label(
                RichText::new(self.session_dir.display().to_string())
                    .weak()
                    .small(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (icon, tip) = match self.theme {
                    Mode::Dark => ("☀", "Cambiar a modo claro"),
                    Mode::Light => ("🌙", "Cambiar a modo oscuro"),
                };
                if ui
                    .add(egui::Button::new(RichText::new(icon).size(16.0)).min_size(egui::vec2(30.0, 26.0)))
                    .on_hover_text(tip)
                    .clicked()
                {
                    self.theme = self.theme.toggled();
                    theme::apply(ui.ctx(), self.theme);
                    theme::save_mode(self.theme);
                }
            });
        });
    }

    fn accent(&self, ui: &egui::Ui) -> Color32 {
        if ui.visuals().dark_mode {
            theme::ACCENT_DARK
        } else {
            theme::ACCENT_LIGHT
        }
    }

    fn ui_audio_bar(&mut self, ui: &mut egui::Ui) {
        let running = self.audio.is_some();
        ui.horizontal_wrapped(|ui| {
            ui.label("Audio:");
            ui.add_enabled_ui(!running, |ui| {
                let mut host_changed = false;
                egui::ComboBox::from_id_salt("host")
                    .selected_text(self.session.audio.host.clone())
                    .show_ui(ui, |ui| {
                        for h in self.hosts.clone() {
                            host_changed |= ui
                                .selectable_value(&mut self.session.audio.host, h.clone(), h)
                                .changed();
                        }
                    });
                if host_changed {
                    self.session.audio.input_device.clear();
                    self.session.audio.output_device.clear();
                    self.refresh_devices();
                }
                let asio = audio_io::is_asio(&self.session.audio.host);
                let dev_combo =
                    |ui: &mut egui::Ui, id: &str, value: &mut String, list: &[String]| {
                        let shown = if value.is_empty() {
                            "(por defecto)".to_string()
                        } else {
                            value.clone()
                        };
                        egui::ComboBox::from_id_salt(id)
                            .width(230.0)
                            .selected_text(shown)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(value, String::new(), "(por defecto)");
                                for d in list {
                                    ui.selectable_value(value, d.clone(), d);
                                }
                            });
                    };
                ui.label(if asio { "Driver:" } else { "Entrada:" });
                dev_combo(
                    ui,
                    "in",
                    &mut self.session.audio.input_device,
                    &self.in_devs,
                );
                if !asio {
                    ui.label("Salida:");
                    dev_combo(
                        ui,
                        "out",
                        &mut self.session.audio.output_device,
                        &self.out_devs,
                    );
                }
                ui.label("Buffer:");
                let b = self.session.audio.buffer_frames;
                egui::ComboBox::from_id_salt("buf")
                    .width(90.0)
                    .selected_text(if b == 0 {
                        "driver".to_string()
                    } else {
                        b.to_string()
                    })
                    .show_ui(ui, |ui| {
                        for s in BUFFER_SIZES {
                            ui.selectable_value(
                                &mut self.session.audio.buffer_frames,
                                s,
                                if s == 0 {
                                    "driver".to_string()
                                } else {
                                    s.to_string()
                                },
                            );
                        }
                    });
                if ui
                    .button("⟳")
                    .on_hover_text("Volver a buscar dispositivos")
                    .clicked()
                {
                    self.refresh_devices();
                }
            });
            if running {
                if ui
                    .add_enabled(!self.recording, egui::Button::new("Detener audio"))
                    .clicked()
                {
                    self.stop_audio();
                    self.info("Audio detenido.");
                }
            } else if ui.button(RichText::new("Iniciar audio").strong()).clicked() {
                self.start_audio();
            }

            ui.separator();
            ui.label("MIDI:");
            let mut midi_changed = false;
            let shown = if self.session.audio.midi_port.is_empty() {
                "(ninguno)".to_string()
            } else {
                self.session.audio.midi_port.clone()
            };
            egui::ComboBox::from_id_salt("midi")
                .width(170.0)
                .selected_text(shown)
                .show_ui(ui, |ui| {
                    midi_changed |= ui
                        .selectable_value(
                            &mut self.session.audio.midi_port,
                            String::new(),
                            "(ninguno)",
                        )
                        .changed();
                    for p in self.midi_ports.clone() {
                        midi_changed |= ui
                            .selectable_value(&mut self.session.audio.midi_port, p.clone(), p)
                            .changed();
                    }
                });
            if midi_changed {
                self.connect_midi();
            }
            ui.label("Nota base:");
            if ui
                .add(
                    egui::DragValue::new(&mut self.session.midi_base_note)
                        .range(0..=(127 - NUM_PADS as u8)),
                )
                .on_hover_text(
                    "Nota MIDI del pad 1 (los demás siguen en orden) cuando un pad no tiene nota \
                     propia. Para mapear un pad a otra nota: clic derecho sobre el pad.",
                )
                .changed()
            {
                self.rebuild_midi_map();
            }
            let dot = Color32::from_rgb(60, (60.0 + 180.0 * self.midi_flash) as u8, 60);
            ui.label(RichText::new("●").color(dot))
                .on_hover_text(format!(
                    "Última nota recibida: {}",
                    self.midi_shared.last_note.load(Ordering::Relaxed)
                ));
        });
        if let Some(a) = &self.audio {
            let x = a.running.status.xruns.load(Ordering::Relaxed);
            let extra = if x > 0 {
                format!(" · {x} cortes (xruns): sube el buffer")
            } else {
                String::new()
            };
            ui.label(
                RichText::new(format!("{}{}", a.running.description, extra))
                    .weak()
                    .small(),
            );
        }
    }

    fn ui_transport(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let (label, fill) = if self.recording {
                ("⏹  PARAR", RED)
            } else {
                ("⏺  GRABAR", theme::pal(ui).rec_idle)
            };
            let btn = egui::Button::new(
                RichText::new(label)
                    .size(18.0)
                    .strong()
                    .color(Color32::WHITE),
            )
            .fill(fill)
            .min_size(egui::vec2(150.0, 36.0));
            if ui
                .add_enabled(self.audio.is_some() && !self.saving_take, btn)
                .clicked()
            {
                if self.recording {
                    self.stop_recording();
                } else {
                    self.start_recording();
                }
            }
            let secs = match (&self.audio, self.recording) {
                (Some(a), true) => {
                    a.meters.rec_frames.load(Ordering::Relaxed) as f64
                        / a.running.sample_rate as f64
                }
                _ => 0.0,
            };
            ui.label(
                RichText::new(fmt_time(secs))
                    .size(26.0)
                    .monospace()
                    .color(if self.recording { RED } else { theme::pal(ui).timer_idle }),
            );
            if ui
                .add_enabled(
                    !self.recording,
                    egui::Button::new("Exportar…").min_size(egui::vec2(0.0, 36.0)),
                )
                .clicked()
            {
                self.refresh_takes();
                self.export.open = true;
            }
            ui.separator();
            ui.label("Voces:");
            let busy = self.recording || self.saving_take;
            let mut n = self.session.num_voices;
            if ui
                .add_enabled(!busy, egui::DragValue::new(&mut n).range(0..=MAX_VOICES))
                .on_hover_text(
                    "Número de pistas de voz (0 = solo música y sampler). \
                     No se puede cambiar mientras se graba.",
                )
                .changed()
            {
                self.session.set_num_voices(n);
                for i in 0..MAX_VOICES {
                    self.send_voice(i);
                }
            }
            ui.separator();
            let color = if self.status_is_error {
                theme::pal(ui).error_text
            } else {
                ui.visuals().text_color()
            };
            ui.add(egui::Label::new(RichText::new(&self.status).color(color)).wrap());
        });
    }

    fn ui_voice(&mut self, ui: &mut egui::Ui, i: usize) {
        let in_channels = self
            .audio
            .as_ref()
            .map(|a| a.running.in_channels)
            .unwrap_or(8)
            .max(1);
        let level = self.levels.voice[i];
        let gr = self
            .audio
            .as_ref()
            .map(|a| Meters::get_f32(&a.meters.voice_gr_db[i]))
            .unwrap_or(0.0);
        let mut changed = false;
        ui.group(|ui| {
            ui.set_width(178.0);
            ui.vertical(|ui| {
                let v = &mut self.session.voices[i];
                ui.add(egui::TextEdit::singleline(&mut v.name).desired_width(165.0));
                ui.scope(|ui| {
                    ui.horizontal(|ui| {
                        ui.label("Entrada");
                        egui::ComboBox::from_id_salt(("ch", i))
                            .width(80.0)
                            .selected_text(format!("Canal {}", v.input_channel + 1))
                            .show_ui(ui, |ui| {
                                for c in 0..in_channels.max(v.input_channel + 1) {
                                    changed |= ui
                                        .selectable_value(
                                            &mut v.input_channel,
                                            c,
                                            format!("Canal {}", c + 1),
                                        )
                                        .changed();
                                }
                            });
                    });
                    ui.label("Preset de voz");
                    let pr = PRESETS[v.preset.min(PRESETS.len() - 1)];
                    egui::ComboBox::from_id_salt(("preset", i))
                        .width(165.0)
                        .selected_text(pr.name)
                        .show_ui(ui, |ui| {
                            for (n, p) in PRESETS.iter().enumerate() {
                                changed |= ui
                                    .selectable_value(&mut v.preset, n, p.name)
                                    .on_hover_text(p.hint)
                                    .changed();
                            }
                        })
                        .response
                        .on_hover_text(pr.hint);
                    ui.label("Ganancia de entrada");
                    changed |= db_slider(ui, &mut v.gain_db, -12.0..=36.0);
                    ui.label("Volumen");
                    changed |= db_slider(ui, &mut v.fader_db, -40.0..=12.0);
                    let mute = egui::Button::new(RichText::new("MUTE").strong()).fill(if v.mute {
                        RED
                    } else {
                        theme::pal(ui).mute_off
                    });
                    if ui.add(mute).clicked() {
                        v.mute = !v.mute;
                        changed = true;
                    }
                    ui.add_space(4.0);
                    meter(ui, level, 165.0);
                    ui.label(
                        RichText::new(format!("Compresión: {:.1} dB", gr.max(0.0)))
                            .small()
                            .weak(),
                    );
                });
            });
        });
        if changed {
            self.send_voice(i);
        }
    }

    fn ui_music_strip(&mut self, ui: &mut egui::Ui) {
        let duck = self
            .audio
            .as_ref()
            .map(|a| Meters::get_f32(&a.meters.duck_gain))
            .unwrap_or(1.0);
        let (music_level, sampler_level) = (self.levels.music, self.levels.sampler);
        let mut changed = false;
        let mut sampler_changed = false;
        ui.group(|ui| {
            ui.set_width(200.0);
            ui.vertical(|ui| {
                let m = &mut self.session.music;
                ui.label(RichText::new("Música").strong());
                ui.label("Volumen");
                changed |= db_slider(ui, &mut m.volume_db, -40.0..=6.0);
                changed |= ui
                    .checkbox(&mut m.duck_enabled, "Ducking automático por voz")
                    .on_hover_text("La música baja sola cuando alguien habla")
                    .changed();
                ui.add_enabled_ui(m.duck_enabled, |ui| {
                    ui.label("Cuánto baja");
                    changed |= db_slider(ui, &mut m.duck_depth_db, -40.0..=0.0);
                    ui.label("Sensibilidad (umbral de voz)");
                    changed |= db_slider(ui, &mut m.duck_threshold_db, -60.0..=-10.0);
                    ui.label("Tiempo de recuperación");
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut m.duck_release_ms, 150.0..=3000.0)
                                .suffix(" ms")
                                .fixed_decimals(0),
                        )
                        .changed();
                });
                meter(ui, music_level, 185.0);
                let db = lin_to_db(duck);
                ui.label(
                    RichText::new(if db < -0.5 {
                        format!("Ducking: {db:.1} dB")
                    } else {
                        "Ducking: —".into()
                    })
                    .small()
                    .weak(),
                );
                ui.separator();
                ui.label(RichText::new("Sampler").strong());
                ui.label("Volumen");
                sampler_changed |= db_slider(ui, &mut self.session.sampler_volume_db, -40.0..=6.0);
                meter(ui, sampler_level, 185.0);
            });
        });
        if changed {
            self.send_music();
        }
        if sampler_changed {
            let v = self.session.sampler_volume_db;
            self.push(Command::SetSamplerVolume(v));
        }
    }

    fn ui_master_strip(&mut self, ui: &mut egui::Ui) {
        let lim = self
            .audio
            .as_ref()
            .map(|a| Meters::get_f32(&a.meters.limiter_gain))
            .unwrap_or(1.0);
        let levels = self.levels.master;
        let mut changed = false;
        ui.group(|ui| {
            ui.set_width(190.0);
            ui.vertical(|ui| {
                ui.label(RichText::new("Monitor (auriculares)").strong());
                ui.label("Volumen de escucha");
                changed |= db_slider(ui, &mut self.session.master_gain_db, -40.0..=6.0);
                changed |= ui
                    .checkbox(&mut self.session.monitor_voices, "Escuchar las voces")
                    .on_hover_text("Envía los micrófonos a la salida. Actívalo solo con auriculares: con altavoces se acopla.")
                    .changed();
                ui.add_space(4.0);
                meter(ui, levels[0], 175.0);
                meter(ui, levels[1], 175.0);
                let db = lin_to_db(lim);
                ui.label(RichText::new(if db < -0.2 { format!("Limitador: {db:.1} dB") } else { "Limitador: —".into() }).small().weak());
                ui.add_space(6.0);
                ui.label(RichText::new("La grabación no depende de este volumen: las pistas se guardan por separado y la mezcla final se hace al exportar.").small().weak());
            });
        });
        if changed {
            self.send_master();
        }
    }

    fn ui_setlist(&mut self, ui: &mut egui::Ui) {
        let (playing, pos, len) = match &self.audio {
            Some(a) => {
                let sr = a.running.sample_rate as f64;
                (
                    a.meters.music_playing.load(Ordering::Relaxed),
                    a.meters.music_pos_frames.load(Ordering::Relaxed) as f64 / sr,
                    a.meters.music_len_frames.load(Ordering::Relaxed) as f64 / sr,
                )
            }
            None => (false, 0.0, 0.0),
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new("Setlist de música").strong());
            if ui.button("＋ Añadir…").clicked() {
                if let Some(files) = rfd::FileDialog::new()
                    .add_filter("Audio", AUDIO_EXTENSIONS)
                    .pick_files()
                {
                    self.session.music.setlist.extend(files);
                }
            }
            let n = self.session.music.setlist.len();
            let sel = self.music_selected;
            if ui
                .add_enabled(sel < n, egui::Button::new("Quitar"))
                .clicked()
            {
                self.session.music.setlist.remove(sel);
                self.music_current = match self.music_current {
                    Some(c) if c == sel => None,
                    Some(c) if c > sel => Some(c - 1),
                    other => other,
                };
                self.music_selected = sel.min(n.saturating_sub(2));
            }
            if ui
                .add_enabled(sel > 0 && sel < n, egui::Button::new("⬆"))
                .clicked()
            {
                self.session.music.setlist.swap(sel, sel - 1);
                self.music_selected -= 1;
                self.music_current = None;
            }
            if ui
                .add_enabled(sel + 1 < n, egui::Button::new("⬇"))
                .clicked()
            {
                self.session.music.setlist.swap(sel, sel + 1);
                self.music_selected += 1;
                self.music_current = None;
            }
        });
        ui.horizontal(|ui| {
            let n = self.session.music.setlist.len();
            let has_audio = self.audio.is_some();
            if ui
                .add_enabled(has_audio && n > 0, egui::Button::new("▶ Reproducir"))
                .on_hover_text("Reproduce desde el principio el tema seleccionado")
                .clicked()
            {
                self.play_music(self.music_selected);
            }
            if playing {
                if ui
                    .button("⏸ Pausa")
                    .on_hover_text("Fundido de salida y pausa")
                    .clicked()
                {
                    self.push(Command::StopMusic);
                }
            } else if ui
                .add_enabled(
                    has_audio && self.music_current.is_some(),
                    egui::Button::new("⏵ Seguir"),
                )
                .clicked()
            {
                self.push(Command::PlayMusic);
            }
            if ui
                .add_enabled(has_audio && n > 0, egui::Button::new("⏭ Siguiente"))
                .clicked()
            {
                let next = self.music_current.map(|c| (c + 1) % n).unwrap_or(0);
                self.play_music(next);
            }
            if self.music_loading {
                ui.spinner();
                ui.label("cargando…");
            }
        });
        if len > 0.0 {
            ui.add(egui::ProgressBar::new((pos / len) as f32).text(format!(
                "{} / {}",
                fmt_time(pos),
                fmt_time(len)
            )));
        }
        let mut play = None;
        egui::ScrollArea::vertical().id_salt("setlist").auto_shrink([false, false]).show(ui, |ui| {
            if self.session.music.setlist.is_empty() {
                ui.label(RichText::new("Añade ficheros mp3, wav, flac, ogg o m4a. La lista se guarda con la sesión.").weak());
            }
            for (i, path) in self.session.music.setlist.iter().enumerate() {
                let current = self.music_current == Some(i);
                let mark = if current && playing { "▶ " } else if current { "⏸ " } else { "    " };
                let text = format!("{mark}{:02}  {}", i + 1, file_label(path));
                let resp = ui.selectable_label(self.music_selected == i, text).on_hover_text(path.display().to_string());
                if resp.clicked() {
                    self.music_selected = i;
                }
                if resp.double_clicked() {
                    play = Some(i);
                }
            }
        });
        if let Some(i) = play {
            self.play_music(i);
        }
    }

    fn ui_pads(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Sampler").strong());
            if ui
                .button("⏹ Parar todo")
                .on_hover_text("También con la tecla Esc")
                .clicked()
            {
                self.push(Command::StopPads);
            }
            ui.label(
                RichText::new("Clic o tecla para disparar · clic derecho: sonido y nota MIDI")
                    .weak()
                    .small(),
            );
        });
        let mut cancel_learn = false;
        if let Some(p) = self.learning_pad {
            ui.horizontal(|ui| {
                ui.colored_label(
                    AMBER,
                    format!("Pad {}: toca una nota en tu controlador MIDI…", p + 1),
                );
                if ui.button("Cancelar").clicked() {
                    cancel_learn = true;
                }
            });
        }
        let has_audio = self.audio.is_some();
        let mut trigger = None;
        let mut reload = None;
        let mut learn = None;
        let mut set_note: Option<(usize, Option<u8>)> = None;
        egui::Grid::new("pads").spacing([6.0, 6.0]).show(ui, |ui| {
            for i in 0..NUM_PADS {
                let playing = self
                    .audio
                    .as_ref()
                    .map(|a| a.meters.pad_playing[i].load(Ordering::Relaxed))
                    .unwrap_or(false);
                let assigned = self.session.pads[i].path.is_some();
                let name = match &self.session.pads[i].path {
                    Some(p) => {
                        let mut n = file_label(p);
                        if n.chars().count() > 16 {
                            n = n.chars().take(15).collect::<String>() + "…";
                        }
                        n
                    }
                    None => "—".into(),
                };
                let pal = theme::pal(ui);
                let idle = !(self.learning_pad == Some(i)
                    || playing
                    || self.pad_loading[i]
                    || (assigned && (self.pad_loaded[i] || !has_audio)));
                let fill = if self.learning_pad == Some(i) {
                    pal.pad_learn
                } else if playing {
                    GREEN
                } else if self.pad_loading[i] {
                    AMBER
                } else if assigned && (self.pad_loaded[i] || !has_audio) {
                    pal.pad_assigned
                } else {
                    pal.pad_idle
                };
                let text_color = if idle { pal.pad_idle_text } else { Color32::WHITE };
                let text = if self.learning_pad == Some(i) {
                    format!("{}\nesperando nota…", PAD_KEYS[i].1)
                } else {
                    format!("{} · MIDI {}\n{}", PAD_KEYS[i].1, self.session.pad_note(i), name)
                };
                let resp = ui.add(
                    egui::Button::new(RichText::new(text).color(text_color))
                        .fill(fill)
                        .min_size(egui::vec2(120.0, 52.0)),
                );
                if resp.clicked() {
                    trigger = Some(i);
                }
                resp.context_menu(|ui| {
                    if ui.button("Cargar sonido…").clicked() {
                        if let Some(f) = rfd::FileDialog::new()
                            .add_filter("Audio", AUDIO_EXTENSIONS)
                            .pick_file()
                        {
                            self.session.pads[i].path = Some(f);
                            reload = Some(i);
                        }
                        ui.close();
                    }
                    if assigned && ui.button("Quitar sonido").clicked() {
                        self.session.pads[i].path = None;
                        reload = Some(i);
                        ui.close();
                    }
                    ui.label("Volumen del pad");
                    if db_slider(ui, &mut self.session.pads[i].gain_db, -24.0..=12.0) {
                        reload = Some(i);
                    }
                    ui.separator();
                    ui.label("Nota MIDI de este pad");
                    ui.horizontal(|ui| {
                        let mut n = self.session.pad_note(i);
                        if ui.add(egui::DragValue::new(&mut n).range(0..=127)).changed() {
                            set_note = Some((i, Some(n)));
                        }
                        ui.label(RichText::new(note_name(n)).weak());
                    });
                    if ui
                        .button("Aprender del controlador…")
                        .on_hover_text("Después toca una tecla o pad de tu controlador MIDI")
                        .clicked()
                    {
                        learn = Some(i);
                        ui.close();
                    }
                    if self.session.pads[i].midi_note.is_some()
                        && ui.button("Nota predeterminada").clicked()
                    {
                        set_note = Some((i, None));
                        ui.close();
                    }
                });
                if i % 4 == 3 {
                    ui.end_row();
                }
            }
        });
        if let Some(index) = trigger {
            self.push(Command::TriggerPad {
                index,
                velocity: 110,
            });
        }
        if let Some(i) = reload {
            self.load_pad(i);
        }
        if cancel_learn {
            self.cancel_learning();
        }
        if let Some(i) = learn {
            self.start_learning(i);
        }
        if let Some((i, note)) = set_note {
            if let Some(n) = note {
                // una nota solo dispara un pad
                for (j, p) in self.session.pads.iter_mut().enumerate() {
                    if j != i && p.midi_note == Some(n) {
                        p.midi_note = None;
                    }
                }
            }
            self.session.pads[i].midi_note = note;
            self.rebuild_midi_map();
        }
    }

    fn ui_export(&mut self, ctx: &egui::Context) {
        if !self.export.open {
            return;
        }
        let mut open = true;
        let mut select = false;
        let mut do_export = false;
        let mut preview: Option<bool> = None;
        let mut stop_preview = false;
        let mut open_editor = false;
        let previewing = self
            .audio
            .as_ref()
            .map(|a| a.meters.preview_playing.load(Ordering::Relaxed))
            .unwrap_or(false);
        let export_dir = self.session_dir.join("export");
        egui::Window::new("Exportar episodio").open(&mut open).default_width(520.0).resizable(true).vscroll(true).show(ctx, |ui| {
            let e = &mut self.export;
            if e.takes.is_empty() {
                ui.label("Todavía no hay tomas grabadas en esta sesión.");
                return;
            }
            let busy = e.rx.is_some();
            ui.add_enabled_ui(!busy, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Toma:");
                    let label = |p: &PathBuf| p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    egui::ComboBox::from_id_salt("take").width(220.0).selected_text(e.takes.get(e.selected).map(label).unwrap_or_default()).show_ui(ui, |ui| {
                        for (i, t) in e.takes.iter().enumerate() {
                            select |= ui.selectable_value(&mut e.selected, i, label(t)).changed();
                        }
                    });
                });
                let Some(info) = e.info.clone() else {
                    ui.colored_label(RED, "No se puede leer take.json de esta toma.");
                    return;
                };
                let total = info.seconds();
                ui.label(format!("Duración grabada: {} · {} Hz · {} disparos de sampler (pads.mid)", fmt_time(total), info.sample_rate, info.pad_events));

                ui.separator();
                ui.label(RichText::new("Recorte").strong());
                egui::Grid::new("trim").num_columns(3).show(ui, |ui| {
                    ui.label("Quitar del principio");
                    ui.add(egui::DragValue::new(&mut e.trim_start).speed(0.1).range(0.0..=(total - e.trim_end - 0.5).max(0.0)).suffix(" s").fixed_decimals(1));
                    if ui.button("▶ Escuchar inicio").clicked() {
                        preview = Some(false);
                    }
                    ui.end_row();
                    ui.label("Quitar del final");
                    ui.add(egui::DragValue::new(&mut e.trim_end).speed(0.1).range(0.0..=(total - e.trim_start - 0.5).max(0.0)).suffix(" s").fixed_decimals(1));
                    if ui.button("▶ Escuchar final").clicked() {
                        preview = Some(true);
                    }
                    ui.end_row();
                    ui.label("Fundido de salida");
                    ui.add(egui::DragValue::new(&mut e.fade_out_ms).speed(10.0).range(0.0..=10000.0).suffix(" ms").fixed_decimals(0));
                    if ui.add_enabled(previewing, egui::Button::new("⏹ Parar escucha")).clicked() {
                        stop_preview = true;
                    }
                    ui.end_row();
                });
                ui.label(format!("Duración final: {}", fmt_time(total - e.trim_start - e.trim_end)));

                ui.separator();
                ui.label(RichText::new("Volumen por pista").strong());
                egui::Grid::new("gains").num_columns(2).show(ui, |ui| {
                    for v in 0..MAX_VOICES {
                        if info.voices.get(v).copied().unwrap_or(false) {
                            ui.label(info.voice_names.get(v).cloned().unwrap_or_else(|| format!("Voz {}", v + 1)));
                            db_slider(ui, &mut e.gains[v], -24.0..=12.0);
                            ui.end_row();
                        }
                    }
                    ui.label("Música");
                    db_slider(ui, &mut e.gains[STEM_MUSIC], -24.0..=12.0);
                    ui.end_row();
                    ui.label("Sampler");
                    db_slider(ui, &mut e.gains[STEM_SAMPLER], -24.0..=12.0);
                    ui.end_row();
                });
                ui.horizontal(|ui| {
                    if ui
                        .button("Editar subidas y bajadas de volumen…")
                        .on_hover_text("Curvas de volumen sobre la línea de tiempo: pistas sueltas y mezcla completa. El ducking grabado también está ahí.")
                        .clicked()
                    {
                        open_editor = true;
                    }
                    let n: usize = e.auto.envelopes.iter().map(|x| x.points.len()).sum();
                    ui.label(RichText::new(if n == 0 { "sin cambios".to_string() } else { format!("{n} puntos") }).weak().small());
                });

                ui.separator();
                ui.label(RichText::new("Masterizado").strong());
                egui::ComboBox::from_id_salt("lufs").width(330.0).selected_text(LOUDNESS_TARGETS[e.target].1).show_ui(ui, |ui| {
                    for (i, t) in LOUDNESS_TARGETS.iter().enumerate() {
                        ui.selectable_value(&mut e.target, i, t.1);
                    }
                });
                ui.checkbox(&mut e.bus_comp, "Compresión suave de mezcla (cohesiona voces y música)");
                ui.label(RichText::new("Siempre se aplica limitador con techo a -1 dB de pico real.").weak().small());

                ui.separator();
                ui.label(RichText::new("Ficheros a exportar").strong());
                ui.checkbox(&mut e.mixdown, "Mixdown: la mezcla masterizada");
                let n_voices = (0..MAX_VOICES).filter(|&v| info.voices.get(v).copied().unwrap_or(false)).count();
                ui.checkbox(&mut e.voice_files, format!("Una pista WAV por voz ({n_voices} grabadas), con su volumen y su curva"));
                ui.checkbox(&mut e.midi_file, format!("MIDI de los pads ({} disparos, ajustado al recorte)", info.pad_events));
                ui.label(RichText::new(format!("Se guardan juntos en export\\{}\\", e.name.trim())).weak().small());

                ui.separator();
                ui.label(RichText::new("Mixdown").strong());
                egui::Grid::new("out").num_columns(2).show(ui, |ui| {
                    ui.label("Nombre base");
                    ui.add(egui::TextEdit::singleline(&mut e.name).desired_width(300.0));
                    ui.end_row();
                    ui.label("Título (etiqueta MP3)");
                    ui.add(egui::TextEdit::singleline(&mut e.title).desired_width(300.0));
                    ui.end_row();
                    ui.label("Autor (etiqueta MP3)");
                    ui.add(egui::TextEdit::singleline(&mut e.artist).desired_width(300.0));
                    ui.end_row();
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut e.wav, "WAV");
                    ui.add_enabled_ui(e.wav, |ui| {
                        ui.selectable_value(&mut e.wav16, false, "24 bit");
                        ui.selectable_value(&mut e.wav16, true, "16 bit");
                    });
                    ui.separator();
                    if export::mp3_available() {
                        ui.checkbox(&mut e.mp3, "MP3");
                        ui.add_enabled_ui(e.mp3, |ui| {
                            egui::ComboBox::from_id_salt("kbps").width(90.0).selected_text(format!("{} kbps", e.kbps)).show_ui(ui, |ui| {
                                for k in [96u32, 128, 160, 192, 256, 320] {
                                    ui.selectable_value(&mut e.kbps, k, format!("{k} kbps"));
                                }
                            });
                        });
                    } else {
                        ui.label(RichText::new("MP3 no incluido en esta compilación").weak());
                    }
                });
                ui.add_space(6.0);
                let any = e.mixdown || e.voice_files || e.midi_file;
                let formats_ok = !e.mixdown || e.wav || e.mp3;
                let can = any && formats_ok && !e.name.trim().is_empty();
                if ui.add_enabled(can, egui::Button::new(RichText::new("Exportar").strong()).min_size(egui::vec2(140.0, 30.0))).clicked() {
                    do_export = true;
                }
            });

            if let Some(p) = &e.progress {
                let frac = p.permille.load(Ordering::Relaxed) as f32 / 1000.0;
                ui.horizontal(|ui| {
                    ui.add(egui::ProgressBar::new(frac).desired_width(360.0).show_percentage());
                    if ui.button("Cancelar").clicked() {
                        p.cancel.store(true, Ordering::Relaxed);
                    }
                });
            }
            match &e.result {
                Some(Ok(r)) => {
                    ui.separator();
                    ui.colored_label(GREEN, "Exportación terminada");
                    for f in &r.files {
                        ui.label(f.display().to_string());
                    }
                    ui.label(format!(
                        "Duración {} · sonoridad {:.1} → {:.1} LUFS · pico real {:.1} dBTP · ganancia aplicada {:+.1} dB",
                        fmt_time(r.seconds),
                        r.input_lufs,
                        r.output_lufs,
                        r.true_peak_db,
                        r.gain_db
                    ));
                    if ui.button("Abrir carpeta").clicked() {
                        let _ = std::process::Command::new("explorer").arg(&export_dir).spawn();
                    }
                }
                Some(Err(err)) => {
                    ui.separator();
                    ui.colored_label(RED, err);
                }
                None => {}
            }
        });
        self.export.open = open;
        if open_editor {
            self.export.editor.open = true;
        }
        if select {
            self.select_take();
        }
        if do_export {
            self.start_export();
        }
        if let Some(from_end) = preview {
            self.start_preview(from_end);
        }
        if stop_preview || !open {
            self.push(Command::StopPreview);
        }
    }
}

// ------------------------------------------------------------------ MIDI de los pads y envolventes

/// Nombre de una nota MIDI (36 = C1, como en la mayoría de controladores de pads).
fn note_name(note: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!("{}{}", NAMES[(note % 12) as usize], (note / 12) as i32 - 2)
}

/// Rango de dB que enseña el editor de envolventes.
const ENV_VIEW_MIN_DB: f32 = -48.0;
const ENV_VIEW_MAX_DB: f32 = 12.0;

fn track_label(track: usize, info: &TakeInfo) -> String {
    if track < MAX_VOICES {
        info.voice_names
            .get(track)
            .filter(|n| !n.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| format!("Voz {}", track + 1))
    } else if track == STEM_MUSIC {
        "Música (ducking)".into()
    } else if track == STEM_SAMPLER {
        "Sampler".into()
    } else {
        "Mezcla completa".into()
    }
}

/// Separación de las marcas de tiempo según lo que se esté viendo.
fn nice_step(span: f64) -> f64 {
    const STEPS: [f64; 14] = [
        0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0,
    ];
    STEPS
        .iter()
        .copied()
        .find(|s| span / s <= 10.0)
        .unwrap_or(3600.0)
}

/// Dibuja y edita una envolvente. Devuelve `true` si cambió.
///
/// - clic en vacío: añade un punto (y se puede arrastrar sin soltar)
/// - arrastrar un punto: lo mueve; cerca de 0 dB se pega a 0
/// - clic derecho sobre un punto, o `Supr` con el punto seleccionado: lo borra
/// - rueda del ratón: zoom en el tiempo; rueda horizontal: desplazar
fn env_canvas(
    ui: &mut egui::Ui,
    env: &mut Envelope,
    peaks: Option<&[f32]>,
    view: &mut EnvView,
    total: f64,
    height: f32,
) -> bool {
    use egui::{pos2, Align2, FontId, Shape, Stroke};

    let mut changed = false;
    let width = ui.available_width().max(320.0);
    let (resp, painter) =
        ui.allocate_painter(egui::vec2(width, height), egui::Sense::click_and_drag());
    let rect = resp.rect;
    let painter = painter.with_clip_rect(rect);
    let total = total.max(2.0);
    let pointer = resp.hover_pos();

    // ---- zoom y desplazamiento con la rueda
    view.span = view.span.clamp(2.0, total);
    if resp.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta);
        if scroll.y != 0.0 {
            if let Some(pos) = pointer {
                let frac = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64;
                let t_at = view.start + frac * view.span;
                view.span = (view.span * (-scroll.y as f64 * 0.004).exp()).clamp(2.0, total);
                view.start = t_at - frac * view.span;
            }
        }
        if scroll.x != 0.0 {
            view.start -= scroll.x as f64 * view.span / rect.width() as f64;
        }
    }
    view.start = view.start.clamp(0.0, (total - view.span).max(0.0));

    let (v0, vs) = (view.start, view.span);
    let t_to_x = |t: f64| rect.left() + ((t - v0) / vs) as f32 * rect.width();
    let x_to_t = |x: f32| v0 + ((x - rect.left()) / rect.width()) as f64 * vs;
    let db_range = ENV_VIEW_MAX_DB - ENV_VIEW_MIN_DB;
    let db_to_y = |db: f32| rect.top() + (ENV_VIEW_MAX_DB - db) / db_range * rect.height();
    let y_to_db = |y: f32| ENV_VIEW_MAX_DB - (y - rect.top()) / rect.height() * db_range;

    // ---- fondo, rejilla de dB y de tiempo
    let pal = theme::pal(ui);
    painter.rect_filled(rect, 4.0, pal.canvas_bg);
    let grid = pal.grid;
    for db in [12.0f32, 6.0, 0.0, -6.0, -12.0, -24.0, -36.0, -48.0] {
        let y = db_to_y(db);
        let color = if db == 0.0 { pal.grid_zero } else { grid };
        painter.line_segment([pos2(rect.left(), y), pos2(rect.right(), y)], Stroke::new(1.0, color));
        painter.text(
            pos2(rect.left() + 4.0, y - 1.0),
            Align2::LEFT_BOTTOM,
            format!("{db:+.0}"),
            FontId::proportional(10.0),
            pal.axis_text,
        );
    }
    let step = nice_step(vs);
    let mut t = (v0 / step).ceil() * step;
    while t < v0 + vs {
        let x = t_to_x(t);
        painter.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(1.0, grid));
        painter.text(
            pos2(x + 3.0, rect.bottom() - 2.0),
            Align2::LEFT_BOTTOM,
            fmt_time(t),
            FontId::proportional(10.0),
            pal.axis_text,
        );
        t += step;
    }

    // ---- forma de onda de la pista (de fondo, para saber dónde se habla)
    if let Some(pk) = peaks {
        let cy = rect.center().y;
        let wave = pal.wave;
        let per_sec = export::PEAKS_PER_SEC as f64;
        let mut px = 0.0f32;
        while px < rect.width() {
            let i0 = (x_to_t(rect.left() + px) * per_sec).max(0.0) as usize;
            let i1 = ((x_to_t(rect.left() + px + 2.0) * per_sec).ceil() as usize).max(i0 + 1);
            if i0 >= pk.len() {
                break;
            }
            let m = pk[i0..i1.min(pk.len())].iter().fold(0f32, |a, &b| a.max(b));
            let h = m.min(1.0) * rect.height() * 0.45;
            let x = rect.left() + px + 1.0;
            painter.line_segment([pos2(x, cy - h), pos2(x, cy + h)], Stroke::new(2.0, wave));
            px += 2.0;
        }
    }

    // ---- interacción
    let near = |pos: egui::Pos2, env: &Envelope| -> Option<usize> {
        env.points
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let d = pos2(t_to_x(p.t), db_to_y(p.db)).distance(pos);
                (i, d)
            })
            .filter(|&(_, d)| d <= 9.0)
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
    };
    let snap = |db: f32| {
        let db = db.clamp(ENV_VIEW_MIN_DB, ENV_VIEW_MAX_DB);
        if db.abs() < 0.8 {
            0.0
        } else {
            (db * 10.0).round() / 10.0
        }
    };
    if resp.hovered() && ui.input(|i| i.pointer.primary_pressed()) {
        if let Some(pos) = pointer {
            match near(pos, env) {
                Some(i) => {
                    view.dragging = Some(i);
                    view.selected = Some(i);
                }
                None => {
                    let i = env.insert(x_to_t(pos.x).clamp(0.0, total), snap(y_to_db(pos.y)));
                    view.dragging = Some(i);
                    view.selected = Some(i);
                    changed = true;
                }
            }
        }
    }
    if let Some(i) = view.dragging {
        if ui.input(|inp| inp.pointer.primary_down()) {
            if let Some(pos) = ui.input(|inp| inp.pointer.interact_pos()) {
                env.move_point(i, x_to_t(pos.x).clamp(0.0, total), snap(y_to_db(pos.y)));
                changed = true;
            }
        } else {
            view.dragging = None;
        }
    }
    if resp.secondary_clicked() {
        if let Some(i) = pointer.and_then(|pos| near(pos, env)) {
            env.remove(i);
            view.selected = None;
            view.dragging = None;
            changed = true;
        }
    }
    if let Some(i) = view.selected {
        if ui.input(|inp| inp.key_pressed(Key::Delete)) {
            env.remove(i);
            view.selected = None;
            view.dragging = None;
            changed = true;
        }
    }
    if view.selected.map_or(false, |i| i >= env.points.len()) {
        view.selected = None;
    }

    // ---- curva y puntos
    let mut line = vec![pos2(rect.left(), db_to_y(env.gain_db_at(v0)))];
    for p in env.points.iter().filter(|p| p.t > v0 && p.t < v0 + vs) {
        line.push(pos2(t_to_x(p.t), db_to_y(p.db)));
    }
    line.push(pos2(rect.right(), db_to_y(env.gain_db_at(v0 + vs))));
    painter.add(Shape::line(line, Stroke::new(2.0, AMBER)));
    for (i, p) in env.points.iter().enumerate() {
        if p.t < v0 - 1.0 || p.t > v0 + vs + 1.0 {
            continue;
        }
        let c = pos2(t_to_x(p.t), db_to_y(p.db));
        let sel = view.selected == Some(i);
        painter.circle_filled(c, if sel { 6.0 } else { 4.5 }, if sel { pal.point_sel } else { AMBER });
        painter.circle_stroke(c, if sel { 6.0 } else { 4.5 }, Stroke::new(1.0, pal.point_outline));
    }

    // ---- lectura bajo el puntero
    if let Some(pos) = pointer {
        let (t, db) = match near(pos, env) {
            Some(i) => (env.points[i].t, env.points[i].db),
            None => (x_to_t(pos.x), y_to_db(pos.y)),
        };
        painter.text(
            pos2(rect.right() - 6.0, rect.top() + 4.0),
            Align2::RIGHT_TOP,
            format!("{} · {:+.1} dB", fmt_time(t), db),
            FontId::monospace(11.0),
            pal.readout,
        );
    }
    changed
}

impl App {
    /// Recalcula la tabla nota -> pad que usa el hilo MIDI.
    fn rebuild_midi_map(&mut self) {
        // la nota asignada a mano a un pad tiene prioridad sobre la predeterminada de otro
        let explicit: HashSet<u8> = self.session.pads.iter().filter_map(|p| p.midi_note).collect();
        let notes: Vec<u8> = (0..NUM_PADS)
            .map(|i| match self.session.pads[i].midi_note {
                Some(n) => n,
                None => {
                    let d = self.session.pad_note(i);
                    if explicit.contains(&d) {
                        u8::MAX // sin nota: la usa otro pad
                    } else {
                        d
                    }
                }
            })
            .collect();
        self.midi_shared.set_notes(&notes);
    }

    fn start_learning(&mut self, pad: usize) {
        if self.session.audio.midi_port.is_empty() {
            self.error("Elige primero un puerto MIDI en la barra de audio.");
            return;
        }
        self.learning_pad = Some(pad);
        self.midi_shared.start_learning(pad);
        self.info(format!(
            "Pad {}: toca una nota en tu controlador MIDI (Esc para cancelar).",
            pad + 1
        ));
    }

    fn cancel_learning(&mut self) {
        self.learning_pad = None;
        self.midi_shared.cancel_learning();
    }

    /// Guarda las subidas y bajadas editadas en `mix.json` de su toma.
    fn save_automation(&mut self) {
        if !self.export.auto_dirty {
            return;
        }
        self.export.auto_dirty = false;
        if let Some(dir) = self.export.auto_take.clone() {
            if let Err(e) = self.export.auto.save(&dir) {
                self.error(format!("No se pudieron guardar las curvas de volumen: {e:#}"));
            }
        }
    }

    /// Pide en segundo plano la forma de onda de una pista, si aún no se tiene.
    fn request_peaks(&mut self, take: &Path, track: usize) {
        let ed = &mut self.export.editor;
        if ed.peaks.contains_key(&track) || ed.peaks_loading.contains(&track) {
            return;
        }
        ed.peaks_loading.insert(track);
        let tx = self.loader_tx.clone();
        let take = take.to_path_buf();
        std::thread::spawn(move || {
            let peaks = export::waveform_peaks(&take, track).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Loaded::Peaks { take, track, peaks });
        });
    }

    fn ui_env_editor(&mut self, ctx: &egui::Context) {
        if !self.export.editor.open {
            return;
        }
        let (Some(info), Some(take)) = (
            self.export.info.clone(),
            self.export.takes.get(self.export.selected).cloned(),
        ) else {
            self.export.editor.open = false;
            return;
        };
        let total = info.seconds();
        let tracks: Vec<usize> = (0..NUM_ENVELOPES)
            .filter(|&i| i >= MAX_VOICES || info.voices.get(i).copied().unwrap_or(false))
            .collect();
        if !tracks.contains(&self.export.editor.track) {
            self.export.editor.track = STEM_MUSIC;
        }
        let track = self.export.editor.track;
        self.request_peaks(&take, track);

        let sample_rate = self.sample_rate();
        let previewing = self
            .audio
            .as_ref()
            .map(|a| a.meters.preview_playing.load(Ordering::Relaxed))
            .unwrap_or(false);
        let mut open = true;
        let mut changed = false;
        let mut preview_from: Option<f64> = None;
        let mut stop_preview = false;

        egui::Window::new("Subidas y bajadas de volumen")
            .open(&mut open)
            .default_width(940.0)
            .resizable(true)
            .show(ctx, |ui| {
                let e = &mut self.export;
                ui.horizontal_wrapped(|ui| {
                    ui.label("Pista:");
                    for &t in &tracks {
                        let n = e.auto.env(t).points.len();
                        let mut label = track_label(t, &info);
                        if n > 0 {
                            label += &format!(" ({n})");
                        }
                        if ui.selectable_label(e.editor.track == t, label).clicked() {
                            e.editor.track = t;
                            e.editor.view.selected = None;
                            e.editor.view.dragging = None;
                        }
                    }
                });
                ui.label(
                    RichText::new(
                        "Clic: añadir punto · arrastrar: mover · clic derecho o Supr: borrar · \
                         rueda: zoom. La curva sube y baja el volumen de la pista en cada momento \
                         (0 dB = sin cambio).",
                    )
                    .weak()
                    .small(),
                );
                let track = e.editor.track;
                if track == STEM_MUSIC {
                    ui.label(
                        RichText::new(
                            "El ducking grabado aparece aquí como curva: puedes suavizarlo, \
                             quitarlo o subir la música en un tramo concreto.",
                        )
                        .weak()
                        .small(),
                    );
                }
                if e.editor.peaks_loading.contains(&track) {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("calculando forma de onda…").weak().small());
                    });
                }
                let peaks = e.editor.peaks.get(&track).map(|v| v.as_slice());
                changed |= env_canvas(
                    ui,
                    &mut e.auto.envelopes[track],
                    peaks,
                    &mut e.editor.view,
                    total,
                    300.0,
                );

                let view = &mut e.editor.view;
                ui.horizontal(|ui| {
                    if ui.button("－").on_hover_text("Alejar").clicked() {
                        let mid = view.start + view.span / 2.0;
                        view.span = (view.span * 2.0).min(total.max(2.0));
                        view.start = (mid - view.span / 2.0).clamp(0.0, (total - view.span).max(0.0));
                    }
                    if ui.button("＋").on_hover_text("Acercar").clicked() {
                        let mid = view.start + view.span / 2.0;
                        view.span = (view.span / 2.0).max(2.0);
                        view.start = (mid - view.span / 2.0).clamp(0.0, (total - view.span).max(0.0));
                    }
                    if ui.button("Toda la toma").clicked() {
                        view.start = 0.0;
                        view.span = total.max(2.0);
                    }
                    ui.add(
                        egui::Slider::new(&mut view.start, 0.0..=(total - view.span).max(0.0))
                            .show_value(false),
                    );
                    ui.label(format!(
                        "{} – {} de {}",
                        fmt_time(view.start),
                        fmt_time(view.start + view.span),
                        fmt_time(total)
                    ));
                });
                let view_start = e.editor.view.start;
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            sample_rate.is_some(),
                            egui::Button::new("▶ Escuchar 10 s desde el inicio de la vista"),
                        )
                        .on_hover_text("Suena la mezcla con todas las curvas actuales (hace falta el audio iniciado)")
                        .clicked()
                    {
                        preview_from = Some(view_start);
                    }
                    if ui
                        .add_enabled(previewing, egui::Button::new("⏹ Parar"))
                        .clicked()
                    {
                        stop_preview = true;
                    }
                    ui.separator();
                    if ui.button("Quitar los puntos de esta pista").clicked() {
                        e.auto.envelopes[track].clear();
                        e.editor.view.selected = None;
                        e.editor.view.dragging = None;
                        changed = true;
                    }
                    if ui
                        .button("Restaurar lo grabado")
                        .on_hover_text("Vuelve a la curva original de esta pista (el ducking tal como se grabó)")
                        .clicked()
                    {
                        let orig = MixAutomation::load_original(&take);
                        e.auto.envelopes[track] = orig.envelopes[track].clone();
                        e.editor.view.selected = None;
                        e.editor.view.dragging = None;
                        changed = true;
                    }
                    if ui.button("Restaurar todo").clicked() {
                        e.auto = MixAutomation::load_original(&take);
                        e.editor.view.selected = None;
                        e.editor.view.dragging = None;
                        changed = true;
                    }
                });
                ui.label(
                    RichText::new(format!(
                        "Los cambios se guardan solos en la toma ({}) y se aplican al exportar.",
                        take.file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default()
                    ))
                    .weak()
                    .small(),
                );
            });

        if changed {
            self.export.auto_dirty = true;
        }
        if let (Some(start), Some(sr)) = (preview_from, sample_rate) {
            self.spawn_preview(take, start, 10.0, sr);
        }
        if stop_preview || !open {
            self.push(Command::StopPreview);
        }
        if !open {
            self.save_automation();
        }
        self.export.editor.open = open;
    }
}
