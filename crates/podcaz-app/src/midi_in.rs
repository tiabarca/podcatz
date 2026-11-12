//! Entrada MIDI: cada nota se asigna a un pad del sampler mediante una tabla compartida
//! (`note_map`) que la interfaz mantiene al día. También implementa «aprender nota».

use std::array;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use midir::{Ignore, MidiInput, MidiInputConnection};
use podcaz_core::engine::PadTrigger;

/// Productor compartido: el motor se recrea cada vez que se reinicia el audio.
pub type TriggerSink = Arc<Mutex<Option<rtrb::Producer<PadTrigger>>>>;

/// Valor de `note_map` para una nota que no dispara ningún pad.
pub const NO_PAD: u8 = 0xFF;
/// Marca de `learned` cuando hay una nota aprendida (el resto de bits: pad << 8 | nota).
const LEARNED_FLAG: u32 = 0x8000_0000;

pub struct MidiShared {
    /// Pad que dispara cada nota MIDI (0..=127), o `NO_PAD`.
    pub note_map: [AtomicU8; 128],
    /// Contador de mensajes note-on (para el indicador de actividad).
    pub activity: AtomicU32,
    /// Última nota recibida.
    pub last_note: AtomicU8,
    /// Pad que está esperando una nota («aprender»), o -1.
    learn_target: AtomicI32,
    /// Resultado del aprendizaje: `LEARNED_FLAG | pad << 8 | nota`, o 0.
    learned: AtomicU32,
}

impl Default for MidiShared {
    fn default() -> Self {
        Self {
            note_map: array::from_fn(|_| AtomicU8::new(NO_PAD)),
            activity: AtomicU32::new(0),
            last_note: AtomicU8::new(0),
            learn_target: AtomicI32::new(-1),
            learned: AtomicU32::new(0),
        }
    }
}

impl MidiShared {
    /// Sustituye la tabla nota -> pad. `notes[pad]` es la nota de cada pad; si dos pads
    /// comparten nota gana el de número más bajo.
    pub fn set_notes(&self, notes: &[u8]) {
        for n in &self.note_map {
            n.store(NO_PAD, Ordering::Relaxed);
        }
        for (pad, &note) in notes.iter().enumerate().rev() {
            if let Some(slot) = self.note_map.get(note as usize) {
                slot.store(pad as u8, Ordering::Relaxed);
            }
        }
    }

    /// La próxima nota que llegue se asigna a `pad` en vez de disparar nada.
    pub fn start_learning(&self, pad: usize) {
        self.learned.store(0, Ordering::Relaxed);
        self.learn_target.store(pad as i32, Ordering::Release);
    }

    pub fn cancel_learning(&self) {
        self.learn_target.store(-1, Ordering::Release);
    }

    /// Devuelve `(pad, nota)` si se ha aprendido una nota desde la última llamada.
    pub fn take_learned(&self) -> Option<(usize, u8)> {
        let v = self.learned.swap(0, Ordering::AcqRel);
        (v & LEARNED_FLAG != 0).then(|| (((v >> 8) & 0xFF) as usize, (v & 0xFF) as u8))
    }

    /// Procesa una nota (note-on). Es lo que hace el callback de MIDI.
    fn note_on(&self, note: u8, velocity: u8, sink: &TriggerSink) {
        self.last_note.store(note, Ordering::Relaxed);
        self.activity.fetch_add(1, Ordering::Relaxed);
        let target = self.learn_target.swap(-1, Ordering::AcqRel);
        if target >= 0 {
            self.learned.store(
                LEARNED_FLAG | ((target as u32 & 0xFF) << 8) | note as u32,
                Ordering::Release,
            );
            return;
        }
        let pad = self
            .note_map
            .get(note as usize)
            .map(|p| p.load(Ordering::Relaxed))
            .unwrap_or(NO_PAD);
        if pad != NO_PAD {
            if let Ok(mut g) = sink.lock() {
                if let Some(p) = g.as_mut() {
                    let _ = p.push(PadTrigger {
                        index: pad,
                        velocity,
                    });
                }
            }
        }
    }
}

pub fn ports() -> Vec<String> {
    match MidiInput::new("PODCAZ") {
        Ok(m) => m
            .ports()
            .iter()
            .filter_map(|p| m.port_name(p).ok())
            .collect(),
        Err(_) => Vec::new(),
    }
}

pub fn connect(
    port_name: &str,
    sink: TriggerSink,
    shared: Arc<MidiShared>,
) -> Result<MidiInputConnection<()>> {
    let mut input = MidiInput::new("PODCAZ").map_err(|e| anyhow!("MIDI no disponible: {e}"))?;
    input.ignore(Ignore::All);
    let port = input
        .ports()
        .into_iter()
        .find(|p| input.port_name(p).map(|n| n == port_name).unwrap_or(false))
        .ok_or_else(|| anyhow!("no se encuentra el puerto MIDI «{port_name}»"))?;
    input
        .connect(
            &port,
            "podcaz-in",
            move |_ts, msg, _| {
                if msg.len() >= 3 && msg[0] & 0xF0 == 0x90 && msg[2] > 0 {
                    shared.note_on(msg[1], msg[2], &sink);
                }
            },
            (),
        )
        .map_err(|e| anyhow!("no se puede abrir el puerto MIDI: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink() -> (TriggerSink, rtrb::Consumer<PadTrigger>) {
        let (tx, rx) = rtrb::RingBuffer::new(16);
        (Arc::new(Mutex::new(Some(tx))), rx)
    }

    #[test]
    fn notes_trigger_the_mapped_pad() {
        let m = MidiShared::default();
        let (sink, mut rx) = sink();
        let mut notes: Vec<u8> = (36..52).collect();
        notes[2] = 70; // pad 3 con una nota suelta
        m.set_notes(&notes);

        m.note_on(36, 100, &sink);
        m.note_on(70, 90, &sink);
        m.note_on(38, 80, &sink); // la nota 38 ya no es de ningún pad
        m.note_on(10, 80, &sink);
        let a = rx.pop().unwrap();
        assert_eq!((a.index, a.velocity), (0, 100));
        let b = rx.pop().unwrap();
        assert_eq!((b.index, b.velocity), (2, 90));
        assert!(rx.pop().is_err());
        assert_eq!(m.last_note.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn learning_captures_the_next_note_without_triggering() {
        let m = MidiShared::default();
        let (sink, mut rx) = sink();
        m.set_notes(&(36..52).collect::<Vec<u8>>());
        assert_eq!(m.take_learned(), None);

        m.start_learning(5);
        m.note_on(36, 100, &sink); // dispararía el pad 0, pero se está aprendiendo
        assert_eq!(m.take_learned(), Some((5, 36)));
        assert!(rx.pop().is_err());
        assert_eq!(m.take_learned(), None);

        // tras aprender, vuelve a disparar normalmente
        m.note_on(36, 100, &sink);
        assert_eq!(rx.pop().unwrap().index, 0);

        // cancelar el aprendizaje
        m.start_learning(1);
        m.cancel_learning();
        m.note_on(37, 100, &sink);
        assert_eq!(m.take_learned(), None);
        assert_eq!(rx.pop().unwrap().index, 1);
    }

    #[test]
    fn shared_note_goes_to_the_lowest_pad() {
        let m = MidiShared::default();
        let mut notes: Vec<u8> = (36..52).collect();
        notes[7] = 36;
        m.set_notes(&notes);
        assert_eq!(m.note_map[36].load(Ordering::Relaxed), 0);
    }
}
