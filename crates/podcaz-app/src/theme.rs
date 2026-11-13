//! Aspecto visual: paletas claro/oscuro, preferencia persistente e icono de la aplicación.

use std::path::PathBuf;

use eframe::egui::{self, Color32, CornerRadius, Shadow, Stroke, Theme};

// ------------------------------------------------------------------ preferencia

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Dark,
    Light,
}

impl Mode {
    pub fn toggled(self) -> Self {
        match self {
            Mode::Dark => Mode::Light,
            Mode::Light => Mode::Dark,
        }
    }

    fn theme(self) -> Theme {
        match self {
            Mode::Dark => Theme::Dark,
            Mode::Light => Theme::Light,
        }
    }
}

fn pref_file() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("PODCAZ").join("theme.txt"))
}

/// Modo guardado; por defecto, oscuro.
pub fn load_mode() -> Mode {
    match pref_file().and_then(|f| std::fs::read_to_string(f).ok()) {
        Some(s) if s.trim() == "light" => Mode::Light,
        _ => Mode::Dark,
    }
}

pub fn save_mode(mode: Mode) {
    if let Some(f) = pref_file() {
        if let Some(dir) = f.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(f, if mode == Mode::Light { "light" } else { "dark" });
    }
}

// ------------------------------------------------------------------ colores

/// Color de acento: el rojo coral del piloto «ON AIR».
pub const ACCENT_DARK: Color32 = Color32::from_rgb(255, 112, 82);
pub const ACCENT_LIGHT: Color32 = Color32::from_rgb(222, 78, 52);

/// Colores semánticos de lo que la app dibuja a mano (vúmetros, pads, editor de curvas…).
pub struct Pal {
    pub meter_bg: Color32,
    pub canvas_bg: Color32,
    pub grid: Color32,
    pub grid_zero: Color32,
    pub axis_text: Color32,
    pub readout: Color32,
    pub wave: Color32,
    pub point_sel: Color32,
    pub point_outline: Color32,
    pub pad_idle: Color32,
    pub pad_idle_text: Color32,
    pub pad_assigned: Color32,
    pub pad_learn: Color32,
    pub mute_off: Color32,
    pub rec_idle: Color32,
    pub timer_idle: Color32,
    pub error_text: Color32,
}

const PAL_DARK: Pal = Pal {
    meter_bg: Color32::from_rgb(30, 32, 40),
    canvas_bg: Color32::from_rgb(18, 20, 26),
    grid: Color32::from_rgb(44, 48, 58),
    grid_zero: Color32::from_rgb(92, 98, 112),
    axis_text: Color32::from_rgb(120, 126, 140),
    readout: Color32::from_rgb(210, 214, 222),
    wave: Color32::from_rgba_unmultiplied_const(110, 150, 210, 70),
    point_sel: Color32::WHITE,
    point_outline: Color32::BLACK,
    pad_idle: Color32::from_rgb(44, 47, 57),
    pad_idle_text: Color32::from_rgb(200, 204, 214),
    pad_assigned: Color32::from_rgb(52, 84, 128),
    pad_learn: Color32::from_rgb(140, 80, 190),
    mute_off: Color32::from_rgb(52, 55, 66),
    rec_idle: Color32::from_rgb(128, 40, 36),
    timer_idle: Color32::from_rgb(130, 136, 150),
    error_text: Color32::from_rgb(255, 120, 110),
};

const PAL_LIGHT: Pal = Pal {
    meter_bg: Color32::from_rgb(222, 224, 230),
    canvas_bg: Color32::from_rgb(252, 252, 253),
    grid: Color32::from_rgb(226, 228, 234),
    grid_zero: Color32::from_rgb(150, 156, 170),
    axis_text: Color32::from_rgb(110, 116, 130),
    readout: Color32::from_rgb(40, 44, 54),
    wave: Color32::from_rgba_unmultiplied_const(60, 100, 170, 60),
    point_sel: Color32::from_rgb(30, 32, 40),
    point_outline: Color32::from_rgb(70, 74, 86),
    pad_idle: Color32::from_rgb(228, 230, 236),
    pad_idle_text: Color32::from_rgb(60, 64, 76),
    pad_assigned: Color32::from_rgb(64, 104, 168),
    pad_learn: Color32::from_rgb(140, 80, 190),
    mute_off: Color32::from_rgb(214, 216, 224),
    rec_idle: Color32::from_rgb(176, 58, 50),
    timer_idle: Color32::from_rgb(120, 126, 140),
    error_text: Color32::from_rgb(196, 40, 40),
};

pub fn pal(ui: &egui::Ui) -> &'static Pal {
    if ui.visuals().dark_mode {
        &PAL_DARK
    } else {
        &PAL_LIGHT
    }
}

// ------------------------------------------------------------------ estilos de egui

fn dark_visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    let bg = Color32::from_rgb(22, 24, 30);
    let surface = Color32::from_rgb(30, 33, 41);
    let raised = Color32::from_rgb(40, 44, 54);
    let border = Color32::from_rgb(52, 56, 68);
    let text = Color32::from_rgb(222, 225, 232);

    v.panel_fill = bg;
    v.window_fill = surface;
    v.window_stroke = Stroke::new(1.0, border);
    v.extreme_bg_color = Color32::from_rgb(15, 16, 21);
    v.faint_bg_color = Color32::from_rgb(27, 29, 36);
    v.code_bg_color = raised;
    v.hyperlink_color = Color32::from_rgb(120, 180, 255);
    v.warn_fg_color = Color32::from_rgb(255, 180, 60);
    v.error_fg_color = Color32::from_rgb(255, 110, 100);
    v.selection.bg_fill = Color32::from_rgb(150, 62, 44);
    v.selection.stroke = Stroke::new(1.0, Color32::from_rgb(255, 214, 200));

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = surface;
    w.noninteractive.weak_bg_fill = surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, text);
    w.inactive.bg_fill = raised;
    w.inactive.weak_bg_fill = raised;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, text);
    w.hovered.bg_fill = Color32::from_rgb(52, 57, 70);
    w.hovered.weak_bg_fill = Color32::from_rgb(52, 57, 70);
    w.hovered.bg_stroke = Stroke::new(1.0, ACCENT_DARK.gamma_multiply(0.7));
    w.hovered.fg_stroke = Stroke::new(1.5, Color32::WHITE);
    w.active.bg_fill = Color32::from_rgb(62, 68, 84);
    w.active.weak_bg_fill = Color32::from_rgb(62, 68, 84);
    w.active.bg_stroke = Stroke::new(1.0, ACCENT_DARK);
    w.active.fg_stroke = Stroke::new(2.0, Color32::WHITE);
    w.open.bg_fill = raised;
    w.open.weak_bg_fill = raised;
    w.open.bg_stroke = Stroke::new(1.0, border);
    w.open.fg_stroke = Stroke::new(1.0, text);
    v
}

fn light_visuals() -> egui::Visuals {
    let mut v = egui::Visuals::light();
    let bg = Color32::from_rgb(243, 242, 239);
    let surface = Color32::from_rgb(252, 251, 249);
    let raised = Color32::from_rgb(230, 230, 234);
    let border = Color32::from_rgb(212, 212, 218);
    let text = Color32::from_rgb(34, 36, 44);

    v.panel_fill = bg;
    v.window_fill = surface;
    v.window_stroke = Stroke::new(1.0, border);
    v.extreme_bg_color = Color32::WHITE;
    v.faint_bg_color = Color32::from_rgb(236, 235, 232);
    v.code_bg_color = raised;
    v.hyperlink_color = Color32::from_rgb(30, 100, 200);
    v.warn_fg_color = Color32::from_rgb(190, 110, 0);
    v.error_fg_color = Color32::from_rgb(196, 40, 40);
    v.selection.bg_fill = Color32::from_rgb(255, 206, 192);
    v.selection.stroke = Stroke::new(1.0, Color32::from_rgb(150, 40, 20));

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = surface;
    w.noninteractive.weak_bg_fill = surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, text);
    w.inactive.bg_fill = raised;
    w.inactive.weak_bg_fill = raised;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, text);
    w.hovered.bg_fill = Color32::from_rgb(220, 220, 226);
    w.hovered.weak_bg_fill = Color32::from_rgb(220, 220, 226);
    w.hovered.bg_stroke = Stroke::new(1.0, ACCENT_LIGHT.gamma_multiply(0.7));
    w.hovered.fg_stroke = Stroke::new(1.5, Color32::BLACK);
    w.active.bg_fill = Color32::from_rgb(206, 206, 214);
    w.active.weak_bg_fill = Color32::from_rgb(206, 206, 214);
    w.active.bg_stroke = Stroke::new(1.0, ACCENT_LIGHT);
    w.active.fg_stroke = Stroke::new(2.0, Color32::BLACK);
    w.open.bg_fill = raised;
    w.open.weak_bg_fill = raised;
    w.open.bg_stroke = Stroke::new(1.0, border);
    w.open.fg_stroke = Stroke::new(1.0, text);
    v
}

fn finish(v: &mut egui::Visuals) {
    let r = CornerRadius::same(6);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = r;
    }
    let shadow = if v.dark_mode { 110 } else { 40 };
    v.window_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(shadow),
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 12,
        spread: 0,
        color: Color32::from_black_alpha(shadow),
    };
}

/// Registra los estilos claro y oscuro y activa `mode`. Se llama una vez al arrancar.
pub fn install(ctx: &egui::Context, mode: Mode) {
    let mut dark = dark_visuals();
    let mut light = light_visuals();
    finish(&mut dark);
    finish(&mut light);
    ctx.set_visuals_of(Theme::Dark, dark);
    ctx.set_visuals_of(Theme::Light, light);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 4.0);
        s.spacing.interact_size.y = 24.0;
    });
    apply(ctx, mode);
}

pub fn apply(ctx: &egui::Context, mode: Mode) {
    ctx.set_theme(mode.theme());
}

// ------------------------------------------------------------------ icono

/// Icono de la app: micrófono de estudio con orejas de gato y ondas de sonido,
/// sobre un cuadrado redondeado coral. Se dibuja por código (sin ficheros de imagen).
pub fn icon_rgba(size: usize) -> Vec<u8> {
    let s = size as f32;
    let px = 1.0 / s; // tamaño de un píxel en coordenadas normalizadas
    let mut out = vec![0u8; size * size * 4];

    let top = [255.0, 128.0, 88.0];
    let bottom = [214.0, 58.0, 62.0];
    let white = [255.0, 255.0, 255.0];

    for y in 0..size {
        for x in 0..size {
            let p = ((x as f32 + 0.5) / s, (y as f32 + 0.5) / s);
            // color premultiplicado acumulado
            let mut acc = [0.0f32; 4];

            // fondo
            let bg_cov = cov(sd_round_box(p, (0.5, 0.5), (0.44, 0.44), 0.19), px);
            let t = ((p.1 - 0.06) / 0.88).clamp(0.0, 1.0);
            let bg = lerp3(top, bottom, t);
            over(&mut acc, bg, bg_cov);

            // ondas de sonido (a izquierda y derecha)
            for (r, a) in [(0.27, 0.95), (0.345, 0.6)] {
                let d = sd_side_arcs(p, (0.5, 0.40), r, 0.62) - 0.017;
                over(&mut acc, white, cov(d, px) * a);
            }

            // cabeza del micro + orejas de gato
            let head = sd_capsule(p, (0.5, 0.31), (0.5, 0.47), 0.115);
            let ear_l = sd_triangle(p, (0.39, 0.27), (0.48, 0.215), (0.375, 0.145)) - 0.012;
            let ear_r = sd_triangle(p, (0.61, 0.27), (0.52, 0.215), (0.625, 0.145)) - 0.012;
            let head_all = head.min(ear_l).min(ear_r);
            over(&mut acc, white, cov(head_all, px));

            // rejilla
            for gy in [0.35, 0.405, 0.46] {
                let d = sd_capsule(p, (0.44, gy), (0.56, gy), 0.011);
                over(&mut acc, bg, cov(d, px) * cov(head, px));
            }

            // horquilla, pie y base
            let yoke = sd_lower_ring(p, (0.5, 0.45), 0.175) - 0.02;
            let stem = sd_capsule(p, (0.5, 0.63), (0.5, 0.75), 0.02);
            let base = sd_capsule(p, (0.39, 0.765), (0.61, 0.765), 0.024);
            over(&mut acc, white, cov(yoke.min(stem).min(base), px));

            // todo queda recortado al cuadrado de fondo
            let i = (y * size + x) * 4;
            let a = (acc[3] * bg_cov.max(0.0)).clamp(0.0, 1.0);
            let k = if acc[3] > 0.0 { 1.0 / acc[3] } else { 0.0 };
            out[i] = (acc[0] * k).round().clamp(0.0, 255.0) as u8;
            out[i + 1] = (acc[1] * k).round().clamp(0.0, 255.0) as u8;
            out[i + 2] = (acc[2] * k).round().clamp(0.0, 255.0) as u8;
            out[i + 3] = (a * 255.0).round() as u8;
        }
    }
    out
}

pub fn icon_data(size: usize) -> egui::IconData {
    egui::IconData {
        rgba: icon_rgba(size),
        width: size as u32,
        height: size as u32,
    }
}

pub fn icon_texture(ctx: &egui::Context, size: usize) -> egui::TextureHandle {
    let img = egui::ColorImage::from_rgba_unmultiplied([size, size], &icon_rgba(size));
    ctx.load_texture("podcaz-icon", img, egui::TextureOptions::LINEAR)
}

type V2 = (f32, f32);

fn cov(d: f32, px: f32) -> f32 {
    (0.5 - d / px).clamp(0.0, 1.0)
}

fn over(acc: &mut [f32; 4], rgb: [f32; 3], a: f32) {
    if a <= 0.0 {
        return;
    }
    for c in 0..3 {
        acc[c] = rgb[c] * a + acc[c] * (1.0 - a);
    }
    acc[3] = a + acc[3] * (1.0 - a);
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

fn len(v: V2) -> f32 {
    (v.0 * v.0 + v.1 * v.1).sqrt()
}

fn sd_round_box(p: V2, c: V2, half: V2, r: f32) -> f32 {
    let q = ((p.0 - c.0).abs() - half.0 + r, (p.1 - c.1).abs() - half.1 + r);
    len((q.0.max(0.0), q.1.max(0.0))) + q.0.max(q.1).min(0.0) - r
}

fn sd_segment(p: V2, a: V2, b: V2) -> f32 {
    let pa = (p.0 - a.0, p.1 - a.1);
    let ba = (b.0 - a.0, b.1 - a.1);
    let h = ((pa.0 * ba.0 + pa.1 * ba.1) / (ba.0 * ba.0 + ba.1 * ba.1)).clamp(0.0, 1.0);
    len((pa.0 - ba.0 * h, pa.1 - ba.1 * h))
}

fn sd_capsule(p: V2, a: V2, b: V2, r: f32) -> f32 {
    sd_segment(p, a, b) - r
}

fn sd_triangle(p: V2, a: V2, b: V2, c: V2) -> f32 {
    let d = sd_segment(p, a, b).min(sd_segment(p, b, c)).min(sd_segment(p, c, a));
    let side = |u: V2, v: V2| (v.0 - u.0) * (p.1 - u.1) - (v.1 - u.1) * (p.0 - u.0);
    let (s1, s2, s3) = (side(a, b), side(b, c), side(c, a));
    let inside = (s1 >= 0.0 && s2 >= 0.0 && s3 >= 0.0) || (s1 <= 0.0 && s2 <= 0.0 && s3 <= 0.0);
    if inside {
        -d
    } else {
        d
    }
}

/// Distancia a dos arcos de radio `r` centrados en `c`, abiertos hacia los lados
/// (±`half_angle` radianes respecto a la horizontal).
fn sd_side_arcs(p: V2, c: V2, r: f32, half_angle: f32) -> f32 {
    let q = ((p.0 - c.0).abs(), p.1 - c.1); // simetría izquierda/derecha
    let ang = q.1.atan2(q.0);
    if ang.abs() <= half_angle {
        (len(q) - r).abs()
    } else {
        let s = if ang > 0.0 { half_angle } else { -half_angle };
        len((q.0 - r * s.cos(), q.1 - r * s.sin()))
    }
}

/// Distancia a la mitad inferior de una circunferencia (la horquilla del micro).
fn sd_lower_ring(p: V2, c: V2, r: f32) -> f32 {
    let q = (p.0 - c.0, p.1 - c.1);
    if q.1 >= 0.0 {
        (len(q) - r).abs()
    } else {
        len((q.0.abs() - r, q.1))
    }
}
