//! Bloques de masterizado usados por la exportación.

use crate::dsp::{Biquad, Compressor};

/// Etapa previa a la normalización: filtro subsónico + compresión de bus opcional.
pub struct PreMaster {
    hp: [Biquad; 2],
    comp: Compressor,
    bus_comp: bool,
}

impl PreMaster {
    pub fn new(sample_rate: u32, bus_comp: bool) -> Self {
        let sr = sample_rate as f32;
        Self {
            hp: [Biquad::highpass(sr, 30.0, 0.707); 2],
            // "pegamento" suave: 2:1, ataque lento para respetar transitorios
            comp: Compressor::new(sr, -18.0, 2.0, 30.0, 300.0, 0.0),
            bus_comp,
        }
    }

    /// Procesa estéreo entrelazado en sitio.
    pub fn process(&mut self, buf: &mut [f32]) {
        for fr in buf.chunks_exact_mut(2) {
            let mut l = self.hp[0].process(fr[0]);
            let mut r = self.hp[1].process(fr[1]);
            if self.bus_comp {
                let g = self.comp.gain_for(l.abs().max(r.abs()));
                l *= g;
                r *= g;
            }
            fr[0] = l;
            fr[1] = r;
        }
    }
}

/// Objetivos de sonoridad habituales.
pub const LOUDNESS_TARGETS: &[(f32, &str)] = &[
    (-16.0, "-16 LUFS · podcast estéreo (Apple, Spotify)"),
    (-14.0, "-14 LUFS · streaming / YouTube"),
    (-19.0, "-19 LUFS · podcast mono"),
    (-23.0, "-23 LUFS · radiodifusión EBU R128"),
];
