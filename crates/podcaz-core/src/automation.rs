//! Envolventes de volumen editables a posteriori.
//!
//! Cada pista de una toma (voces, música, sampler) y la mezcla completa tienen una envolvente:
//! una lista de puntos (segundo, dB) unidos por rectas en dB. Sin puntos = sin cambios.
//!
//! El ducking de la música ya no queda fijo en el audio: el motor graba la música limpia y
//! el grabador guarda la curva de ducking como envolvente de la música (`mix.json`), que se
//! puede retocar al exportar. La copia original queda en `mix.auto.json`.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::dsp::db_to_lin;
use crate::MAX_VOICES;

/// Índices de pista: voz1..voz4, música, sampler.
pub const STEM_MUSIC: usize = MAX_VOICES;
pub const STEM_SAMPLER: usize = MAX_VOICES + 1;
pub const NUM_STEMS: usize = MAX_VOICES + 2;
/// Envolvente de la mezcla completa (se aplica a la suma de pistas, antes del máster).
pub const ENV_MIX: usize = NUM_STEMS;
pub const NUM_ENVELOPES: usize = NUM_STEMS + 1;

pub const MIN_DB: f32 = -60.0;
pub const MAX_DB: f32 = 24.0;

pub const MIX_FILE: &str = "mix.json";
pub const MIX_AUTO_FILE: &str = "mix.auto.json";

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    /// Segundos desde el inicio de la toma (antes de recortar).
    pub t: f64,
    pub db: f32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Envelope {
    /// Ordenados por tiempo.
    pub points: Vec<Point>,
}

impl Envelope {
    /// Sin puntos, o todos a 0 dB: no cambia el audio.
    pub fn is_flat(&self) -> bool {
        self.points.iter().all(|p| p.db.abs() < 0.01)
    }

    pub fn clear(&mut self) {
        self.points.clear();
    }

    /// Ordena y limita los valores (para JSON editado a mano).
    pub fn normalize(&mut self) {
        self.points.retain(|p| p.t.is_finite() && p.db.is_finite());
        for p in self.points.iter_mut() {
            p.t = p.t.max(0.0);
            p.db = p.db.clamp(MIN_DB, MAX_DB);
        }
        self.points
            .sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
    }

    /// Ganancia en dB en el instante `t`. Antes del primer punto y después del último se mantiene el valor.
    pub fn gain_db_at(&self, t: f64) -> f32 {
        eval_db(&self.points, &mut 0, t)
    }

    /// Inserta un punto manteniendo el orden. Devuelve su posición.
    pub fn insert(&mut self, t: f64, db: f32) -> usize {
        let p = Point {
            t: t.max(0.0),
            db: db.clamp(MIN_DB, MAX_DB),
        };
        let i = self.points.partition_point(|q| q.t <= p.t);
        self.points.insert(i, p);
        i
    }

    /// Mueve un punto sin saltarse a sus vecinos (el índice no cambia).
    pub fn move_point(&mut self, i: usize, t: f64, db: f32) {
        if i >= self.points.len() {
            return;
        }
        let lo = if i > 0 { self.points[i - 1].t } else { 0.0 };
        let hi = self
            .points
            .get(i + 1)
            .map(|p| p.t)
            .unwrap_or(f64::INFINITY);
        self.points[i] = Point {
            t: t.clamp(lo, hi),
            db: db.clamp(MIN_DB, MAX_DB),
        };
    }

    pub fn remove(&mut self, i: usize) {
        if i < self.points.len() {
            self.points.remove(i);
        }
    }

    /// Convierte una curva de ganancia lineal (una muestra cada `step_s` segundos) en una
    /// envolvente con pocos puntos. `tol_db` es el error máximo que se tolera.
    /// Una curva que no se aparta de 0 dB da una envolvente vacía.
    pub fn from_gain_curve(gains: &[f32], step_s: f64, tol_db: f32) -> Envelope {
        if gains.is_empty() {
            return Envelope::default();
        }
        let db: Vec<f32> = gains
            .iter()
            .map(|&g| (20.0 * g.max(1e-6).log10()).clamp(MIN_DB, MAX_DB))
            .collect();
        if db.iter().all(|d| d.abs() <= tol_db) {
            return Envelope::default();
        }
        let n = db.len();
        let mut keep = vec![false; n];
        keep[0] = true;
        keep[n - 1] = true;
        // Ramer–Douglas–Peucker iterativo (distancia vertical, el eje de tiempo es uniforme)
        let mut stack = vec![(0usize, n - 1)];
        while let Some((a, b)) = stack.pop() {
            if b <= a + 1 {
                continue;
            }
            let (ya, yb) = (db[a], db[b]);
            let (mut worst, mut at) = (0f32, a);
            for i in a + 1..b {
                let y = ya + (yb - ya) * (i - a) as f32 / (b - a) as f32;
                let d = (db[i] - y).abs();
                if d > worst {
                    worst = d;
                    at = i;
                }
            }
            if worst > tol_db {
                keep[at] = true;
                stack.push((a, at));
                stack.push((at, b));
            }
        }
        Envelope {
            points: (0..n)
                .filter(|&i| keep[i])
                .map(|i| Point {
                    t: i as f64 * step_s,
                    db: (db[i] * 10.0).round() / 10.0,
                })
                .collect(),
        }
    }
}

/// Interpola `pts` (ordenados) en `t`. `idx` recuerda el tramo anterior: lo normal es leer en
/// orden de tiempo creciente, pero también funciona hacia atrás.
fn eval_db(pts: &[Point], idx: &mut usize, t: f64) -> f32 {
    let n = pts.len();
    if n == 0 {
        return 0.0;
    }
    if t <= pts[0].t {
        return pts[0].db;
    }
    if t >= pts[n - 1].t {
        return pts[n - 1].db;
    }
    if *idx >= n - 1 || pts[*idx].t > t {
        *idx = pts.partition_point(|p| p.t <= t).saturating_sub(1);
    }
    while *idx + 1 < n && pts[*idx + 1].t <= t {
        *idx += 1;
    }
    let (a, b) = (pts[*idx], pts[*idx + 1]);
    let span = b.t - a.t;
    if span <= 0.0 {
        return b.db;
    }
    a.db + (b.db - a.db) * ((t - a.t) / span) as f32
}

/// Ganancia lineal de una envolvente leída en orden de tiempo (para mezclar offline).
/// Recalcula la potencia solo cuando cambia el valor en dB.
#[derive(Clone, Debug)]
pub struct EnvGain {
    pts: Vec<Point>,
    idx: usize,
    last_db: f32,
    last_lin: f32,
}

impl EnvGain {
    pub fn new(env: &Envelope) -> Self {
        Self {
            pts: env.points.clone(),
            idx: 0,
            last_db: 0.0,
            last_lin: 1.0,
        }
    }

    /// Sin puntos que cambien el audio: no hace falta evaluarla.
    pub fn is_flat(&self) -> bool {
        self.pts.iter().all(|p| p.db.abs() < 0.01)
    }

    pub fn gain_db(&mut self, t: f64) -> f32 {
        eval_db(&self.pts, &mut self.idx, t)
    }

    pub fn gain(&mut self, t: f64) -> f32 {
        let db = self.gain_db(t);
        if db != self.last_db {
            self.last_db = db;
            self.last_lin = db_to_lin(db);
        }
        self.last_lin
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MixAutomation {
    /// `NUM_ENVELOPES` envolventes: voces, música, sampler y mezcla completa.
    pub envelopes: Vec<Envelope>,
}

impl Default for MixAutomation {
    fn default() -> Self {
        Self {
            envelopes: vec![Envelope::default(); NUM_ENVELOPES],
        }
    }
}

impl MixAutomation {
    pub fn normalize(&mut self) {
        self.envelopes.resize_with(NUM_ENVELOPES, Envelope::default);
        self.envelopes.truncate(NUM_ENVELOPES);
        for e in self.envelopes.iter_mut() {
            e.normalize();
        }
    }

    pub fn env(&self, i: usize) -> &Envelope {
        &self.envelopes[i.min(NUM_ENVELOPES - 1)]
    }

    pub fn is_flat(&self) -> bool {
        self.envelopes.iter().all(|e| e.is_flat())
    }

    /// Lee `mix.json` de la toma. Si no existe (tomas antiguas) devuelve una automatización vacía.
    pub fn load(take_dir: &Path) -> MixAutomation {
        Self::load_file(&take_dir.join(MIX_FILE)).unwrap_or_default()
    }

    /// Copia original grabada (`mix.auto.json`), para poder volver atrás.
    pub fn load_original(take_dir: &Path) -> MixAutomation {
        Self::load_file(&take_dir.join(MIX_AUTO_FILE)).unwrap_or_default()
    }

    fn load_file(path: &Path) -> Option<MixAutomation> {
        let text = std::fs::read_to_string(path).ok()?;
        let mut m: MixAutomation = serde_json::from_str(&text).ok()?;
        m.normalize();
        Some(m)
    }

    pub fn save(&self, take_dir: &Path) -> Result<()> {
        self.save_as(&take_dir.join(MIX_FILE))
    }

    pub fn save_original(&self, take_dir: &Path) -> Result<()> {
        self.save_as(&take_dir.join(MIX_AUTO_FILE))
    }

    fn save_as(&self, path: &Path) -> Result<()> {
        std::fs::write(path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("no se puede escribir {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(points: &[(f64, f32)]) -> Envelope {
        Envelope {
            points: points.iter().map(|&(t, db)| Point { t, db }).collect(),
        }
    }

    #[test]
    fn interpolates_in_db_and_holds_the_ends() {
        let e = env(&[(1.0, 0.0), (3.0, -20.0), (4.0, -20.0)]);
        assert_eq!(e.gain_db_at(0.0), 0.0);
        assert_eq!(e.gain_db_at(1.0), 0.0);
        assert!((e.gain_db_at(2.0) - -10.0).abs() < 1e-4);
        assert_eq!(e.gain_db_at(3.5), -20.0);
        assert_eq!(e.gain_db_at(99.0), -20.0);
        assert_eq!(Envelope::default().gain_db_at(5.0), 0.0);
    }

    #[test]
    fn cursor_matches_random_access_even_going_backwards() {
        let e = env(&[(0.0, 0.0), (1.0, -6.0), (2.0, -6.0), (3.0, 3.0)]);
        let mut c = EnvGain::new(&e);
        for &t in &[0.2, 0.9, 1.5, 2.5, 2.9, 0.1, 3.5, 1.0] {
            assert!((c.gain_db(t) - e.gain_db_at(t)).abs() < 1e-5, "t={t}");
        }
        assert!((c.gain(1.5) - db_to_lin(-6.0)).abs() < 1e-4);
    }

    #[test]
    fn edit_operations_keep_order() {
        let mut e = env(&[(1.0, 0.0), (5.0, 0.0)]);
        let i = e.insert(3.0, -10.0);
        assert_eq!(i, 1);
        assert_eq!(e.points.len(), 3);
        // un punto no puede saltarse a su vecino
        e.move_point(1, 9.0, -12.0);
        assert_eq!(e.points[1].t, 5.0);
        assert_eq!(e.points[1].db, -12.0);
        e.move_point(1, -4.0, 100.0);
        assert_eq!(e.points[1].t, 1.0);
        assert_eq!(e.points[1].db, MAX_DB);
        e.remove(1);
        assert_eq!(e.points.len(), 2);
    }

    #[test]
    fn gain_curve_is_simplified_to_few_points() {
        // 10 s a 100 puntos/s: baja a -14 dB entre 2 s y 6 s, con rampas de 0,2 s
        let step = 0.01;
        let g: Vec<f32> = (0..1000)
            .map(|i| {
                let t = i as f64 * step;
                let db = if t < 2.0 {
                    0.0
                } else if t < 2.2 {
                    -14.0 * (t - 2.0) / 0.2
                } else if t < 6.0 {
                    -14.0
                } else if t < 6.2 {
                    -14.0 + 14.0 * (t - 6.0) / 0.2
                } else {
                    0.0
                };
                db_to_lin(db as f32)
            })
            .collect();
        let e = Envelope::from_gain_curve(&g, step, 0.5);
        assert!(e.points.len() <= 8, "{} puntos", e.points.len());
        for (i, &gain) in g.iter().enumerate() {
            let real = 20.0 * gain.log10();
            let got = e.gain_db_at(i as f64 * step);
            assert!((real - got).abs() < 0.8, "i={i} {real} vs {got}");
        }
        // sin ducking -> envolvente vacía
        assert!(Envelope::from_gain_curve(&vec![1.0; 500], step, 0.5)
            .points
            .is_empty());
    }

    #[test]
    fn automation_roundtrip_and_tolerant_load() {
        let dir = std::env::temp_dir().join(format!("podcaz-auto-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(MixAutomation::load(&dir).is_flat());
        let mut m = MixAutomation::default();
        m.envelopes[STEM_MUSIC].insert(2.0, -12.0);
        m.save(&dir).unwrap();
        let l = MixAutomation::load(&dir);
        assert_eq!(l, m);
        assert!(!l.is_flat());
        // un JSON recortado y desordenado se normaliza
        std::fs::write(
            dir.join(MIX_FILE),
            r#"{"envelopes":[{"points":[{"t":5,"db":-3},{"t":1,"db":-90}]}]}"#,
        )
        .unwrap();
        let l = MixAutomation::load(&dir);
        assert_eq!(l.envelopes.len(), NUM_ENVELOPES);
        assert_eq!(l.envelopes[0].points[0].t, 1.0);
        assert_eq!(l.envelopes[0].points[0].db, MIN_DB);
        std::fs::remove_dir_all(&dir).ok();
    }
}
