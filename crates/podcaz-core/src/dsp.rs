//! Bloques DSP básicos. Todo es `f32`, sin reservas de memoria en `process`.

use std::collections::VecDeque;
use std::f32::consts::PI;

#[inline]
pub fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[inline]
pub fn lin_to_db(lin: f32) -> f32 {
    20.0 * lin.max(1e-9).log10()
}

/// Coeficiente de un filtro de un polo para una constante de tiempo en ms.
#[inline]
pub fn time_coef(ms: f32, sr: f32) -> f32 {
    if ms <= 0.0 {
        0.0
    } else {
        (-1.0 / (ms * 0.001 * sr)).exp()
    }
}

// ---------------------------------------------------------------- Biquad

/// Biquad (fórmulas RBJ), forma directa II transpuesta.
#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Default for Biquad {
    fn default() -> Self {
        Self::bypass()
    }
}

impl Biquad {
    pub fn bypass() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn from_coefs(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    pub fn highpass(sr: f32, freq: f32, q: f32) -> Self {
        let w = 2.0 * PI * (freq / sr).min(0.49);
        let (s, c) = w.sin_cos();
        let alpha = s / (2.0 * q);
        Self::from_coefs(
            (1.0 + c) / 2.0,
            -(1.0 + c),
            (1.0 + c) / 2.0,
            1.0 + alpha,
            -2.0 * c,
            1.0 - alpha,
        )
    }

    pub fn peaking(sr: f32, freq: f32, q: f32, gain_db: f32) -> Self {
        if gain_db.abs() < 0.01 {
            return Self::bypass();
        }
        let a = 10f32.powf(gain_db / 40.0);
        let w = 2.0 * PI * (freq / sr).min(0.49);
        let (s, c) = w.sin_cos();
        let alpha = s / (2.0 * q);
        Self::from_coefs(
            1.0 + alpha * a,
            -2.0 * c,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * c,
            1.0 - alpha / a,
        )
    }

    pub fn low_shelf(sr: f32, freq: f32, gain_db: f32) -> Self {
        if gain_db.abs() < 0.01 {
            return Self::bypass();
        }
        let a = 10f32.powf(gain_db / 40.0);
        let w = 2.0 * PI * (freq / sr).min(0.49);
        let (s, c) = w.sin_cos();
        let alpha = s / 2.0 * 2f32.sqrt(); // pendiente S = 1
        let k = 2.0 * a.sqrt() * alpha;
        Self::from_coefs(
            a * ((a + 1.0) - (a - 1.0) * c + k),
            2.0 * a * ((a - 1.0) - (a + 1.0) * c),
            a * ((a + 1.0) - (a - 1.0) * c - k),
            (a + 1.0) + (a - 1.0) * c + k,
            -2.0 * ((a - 1.0) + (a + 1.0) * c),
            (a + 1.0) + (a - 1.0) * c - k,
        )
    }

    pub fn high_shelf(sr: f32, freq: f32, gain_db: f32) -> Self {
        if gain_db.abs() < 0.01 {
            return Self::bypass();
        }
        let a = 10f32.powf(gain_db / 40.0);
        let w = 2.0 * PI * (freq / sr).min(0.49);
        let (s, c) = w.sin_cos();
        let alpha = s / 2.0 * 2f32.sqrt();
        let k = 2.0 * a.sqrt() * alpha;
        Self::from_coefs(
            a * ((a + 1.0) + (a - 1.0) * c + k),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
            a * ((a + 1.0) + (a - 1.0) * c - k),
            (a + 1.0) - (a - 1.0) * c + k,
            2.0 * ((a - 1.0) - (a + 1.0) * c),
            (a + 1.0) - (a - 1.0) * c - k,
        )
    }

    /// Copia los coeficientes de `other` conservando el estado (evita clics al cambiar de preset).
    pub fn set_coefs_from(&mut self, other: &Biquad) {
        self.b0 = other.b0;
        self.b1 = other.b1;
        self.b2 = other.b2;
        self.a1 = other.a1;
        self.a2 = other.a2;
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        // anti-denormales
        if self.z1.abs() < 1e-20 {
            self.z1 = 0.0;
        }
        if self.z2.abs() < 1e-20 {
            self.z2 = 0.0;
        }
        y
    }
}

// ---------------------------------------------------------------- Gate

/// Puerta de ruido / expansor suave: atenúa `range_db` cuando la señal cae bajo el umbral.
#[derive(Clone, Debug)]
pub struct Gate {
    pub threshold_db: f32,
    pub range_db: f32,
    env: f32,
    gain: f32,
    hold: u32,
    hold_samples: u32,
    env_rel: f32,
    att: f32,
    rel: f32,
    open: bool,
}

impl Gate {
    pub fn new(sr: f32) -> Self {
        Self {
            threshold_db: -90.0,
            range_db: 0.0,
            env: 0.0,
            gain: 1.0,
            hold: 0,
            hold_samples: (0.2 * sr) as u32,
            env_rel: time_coef(60.0, sr),
            att: time_coef(2.0, sr),
            rel: time_coef(180.0, sr),
            open: true,
        }
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        if self.range_db >= -0.01 {
            return x;
        }
        let a = x.abs();
        self.env = if a > self.env {
            a
        } else {
            a + self.env_rel * (self.env - a)
        };
        let env_db = lin_to_db(self.env);
        // histéresis de 4 dB
        if env_db > self.threshold_db {
            self.open = true;
            self.hold = self.hold_samples;
        } else if env_db < self.threshold_db - 4.0 {
            if self.hold > 0 {
                self.hold -= 1;
            } else {
                self.open = false;
            }
        }
        let target = if self.open {
            1.0
        } else {
            db_to_lin(self.range_db)
        };
        let c = if target > self.gain {
            self.att
        } else {
            self.rel
        };
        self.gain = target + c * (self.gain - target);
        x * self.gain
    }
}

// ---------------------------------------------------------------- Compresor

/// Compresor feed-forward con codo suave. `gain_for` permite enlazar canales.
#[derive(Clone, Debug)]
pub struct Compressor {
    pub threshold_db: f32,
    pub ratio: f32,
    pub knee_db: f32,
    pub makeup_db: f32,
    att: f32,
    rel: f32,
    gr_db: f32,
}

impl Compressor {
    pub fn new(
        sr: f32,
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        makeup_db: f32,
    ) -> Self {
        Self {
            threshold_db,
            ratio: ratio.max(1.0),
            knee_db: 6.0,
            makeup_db,
            att: time_coef(attack_ms, sr),
            rel: time_coef(release_ms, sr),
            gr_db: 0.0,
        }
    }

    pub fn set(
        &mut self,
        sr: f32,
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        makeup_db: f32,
    ) {
        self.threshold_db = threshold_db;
        self.ratio = ratio.max(1.0);
        self.makeup_db = makeup_db;
        self.att = time_coef(attack_ms, sr);
        self.rel = time_coef(release_ms, sr);
    }

    /// Reducción de ganancia actual en dB (>= 0).
    pub fn gain_reduction_db(&self) -> f32 {
        self.gr_db
    }

    /// Devuelve la ganancia lineal (incluye makeup) para un nivel de detector `level` (lineal).
    #[inline]
    pub fn gain_for(&mut self, level: f32) -> f32 {
        if self.ratio <= 1.001 {
            return db_to_lin(self.makeup_db);
        }
        let over = lin_to_db(level) - self.threshold_db;
        let slope = 1.0 - 1.0 / self.ratio;
        let target = if 2.0 * over.abs() <= self.knee_db {
            let t = over + self.knee_db / 2.0;
            slope * t * t / (2.0 * self.knee_db)
        } else if over > 0.0 {
            slope * over
        } else {
            0.0
        };
        let c = if target > self.gr_db {
            self.att
        } else {
            self.rel
        };
        self.gr_db = target + c * (self.gr_db - target);
        db_to_lin(self.makeup_db - self.gr_db)
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        x * self.gain_for(x.abs())
    }
}

// ---------------------------------------------------------------- Limitador

/// Limitador estéreo con anticipación (lookahead). Garantiza |salida| <= techo.
///
/// Introduce un retardo de `latency()` muestras.
pub struct Limiter {
    ceiling: f32,
    look: usize,
    delay: Vec<[f32; 2]>,
    pos: usize,
    rel: f32,
    r: f32,
    // mínimo deslizante (cola monótona) sobre look+1 muestras
    minq: VecDeque<(u64, f32)>,
    // media móvil sobre `look` muestras
    avg_buf: Vec<f32>,
    avg_pos: usize,
    avg_sum: f64,
    n: u64,
    gr_min: f32,
}

impl Limiter {
    pub fn new(sr: f32, ceiling_db: f32, lookahead_ms: f32, release_ms: f32) -> Self {
        let look = ((lookahead_ms * 0.001 * sr) as usize).max(1);
        Self {
            ceiling: db_to_lin(ceiling_db),
            look,
            delay: vec![[0.0; 2]; look],
            pos: 0,
            rel: 1.0 - time_coef(release_ms, sr),
            r: 1.0,
            minq: VecDeque::with_capacity(look + 4),
            avg_buf: vec![1.0; look],
            avg_pos: 0,
            avg_sum: look as f64,
            n: 0,
            gr_min: 1.0,
        }
    }

    pub fn latency(&self) -> usize {
        self.look
    }

    /// Menor ganancia aplicada desde la última llamada (1.0 = sin limitar). Reinicia el valor.
    pub fn take_min_gain(&mut self) -> f32 {
        std::mem::replace(&mut self.gr_min, 1.0)
    }

    #[inline]
    pub fn process(&mut self, l: f32, r: f32) -> (f32, f32) {
        let peak = l.abs().max(r.abs());
        let req = if peak > self.ceiling {
            self.ceiling / peak
        } else {
            1.0
        };
        // liberación suave, ataque instantáneo (r <= req siempre)
        self.r = (self.r + (1.0 - self.r) * self.rel).min(req);

        // mínimo deslizante sobre [n-look, n]
        while let Some(&(_, v)) = self.minq.back() {
            if v >= self.r {
                self.minq.pop_back();
            } else {
                break;
            }
        }
        self.minq.push_back((self.n, self.r));
        while let Some(&(i, _)) = self.minq.front() {
            if i + (self.look as u64) < self.n {
                self.minq.pop_front();
            } else {
                break;
            }
        }
        let m = self.minq.front().map(|x| x.1).unwrap_or(1.0);

        // media móvil del mínimo -> rampa de ganancia sin saltos
        self.avg_sum += (m - self.avg_buf[self.avg_pos]) as f64;
        self.avg_buf[self.avg_pos] = m;
        self.avg_pos = (self.avg_pos + 1) % self.look;
        if self.avg_pos == 0 {
            // recalcular periódicamente para no acumular error numérico
            self.avg_sum = self.avg_buf.iter().map(|&v| v as f64).sum();
        }
        let g = ((self.avg_sum / self.look as f64) as f32).min(m);

        let d = self.delay[self.pos];
        self.delay[self.pos] = [l, r];
        self.pos = (self.pos + 1) % self.look;
        self.n += 1;
        if g < self.gr_min {
            self.gr_min = g;
        }
        (d[0] * g, d[1] * g)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_never_exceeds_ceiling() {
        let sr = 48000.0;
        let mut lim = Limiter::new(sr, -1.0, 5.0, 80.0);
        let ceil = db_to_lin(-1.0) * 1.0001;
        let mut seed = 12345u32;
        let mut max_out = 0f32;
        for i in 0..200_000 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let noise = (seed >> 8) as f32 / (1 << 23) as f32 - 1.0;
            let burst = if (i / 5000) % 3 == 0 { 4.0 } else { 0.3 };
            let x = noise * burst;
            let (l, r) = lim.process(x, -x * 0.7);
            max_out = max_out.max(l.abs()).max(r.abs());
        }
        assert!(max_out <= ceil, "pico {max_out} > techo {ceil}");
        assert!(max_out > 0.5);
    }

    #[test]
    fn limiter_transparent_below_ceiling() {
        let mut lim = Limiter::new(48000.0, -1.0, 5.0, 80.0);
        let lat = lim.latency();
        let input: Vec<f32> = (0..2000).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        let out: Vec<f32> = input.iter().map(|&x| lim.process(x, x).0).collect();
        for i in lat..2000 {
            assert!((out[i] - input[i - lat]).abs() < 1e-5);
        }
    }

    #[test]
    fn highpass_removes_dc_and_keeps_highs() {
        let sr = 48000.0;
        let mut hp = Biquad::highpass(sr, 80.0, 0.707);
        let mut last = 1.0;
        for _ in 0..48000 {
            last = hp.process(1.0);
        }
        assert!(last.abs() < 1e-3);
        let mut hp = Biquad::highpass(sr, 80.0, 0.707);
        let mut peak = 0f32;
        for i in 0..48000 {
            let y = hp.process((2.0 * PI * 1000.0 * i as f32 / sr).sin());
            if i > 24000 {
                peak = peak.max(y.abs());
            }
        }
        assert!((peak - 1.0).abs() < 0.02);
    }

    #[test]
    fn compressor_reduces_loud_signal() {
        let sr = 48000.0;
        let mut c = Compressor::new(sr, -20.0, 4.0, 5.0, 100.0, 0.0);
        let mut peak = 0f32;
        for i in 0..48000 {
            let y = c.process((2.0 * PI * 440.0 * i as f32 / sr).sin());
            if i > 24000 {
                peak = peak.max(y.abs());
            }
        }
        // 0 dBFS con umbral -20 y 4:1 -> aprox. -15 dBFS
        let db = lin_to_db(peak);
        assert!(db < -10.0 && db > -18.0, "nivel {db}");
    }
}
