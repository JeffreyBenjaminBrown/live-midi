// grid_synth: press a button on the monome, hear a triangle wave.
//
// Pitch layout (parameterised):
//     f(x, y) = fundamental * 2^((x_step * x + y_step * y) / edo)
//   (0, 0) = fundamental Hz.
//
// Defaults: fundamental=220, edo=46, x_step=9, y_step=1  (so (0,0) is
// 220 Hz, +x = +9/46 oct, +y = +1/46 oct). Override via CLI args:
//     grid_synth [FUND [EDO [X_STEP [Y_STEP]]]]
// e.g. grid_synth 440 72 7 1  for 440 Hz root in a 72-EDO layout.
//
// Architecture:
//   - Main thread: discovers the grid via serialoscd's detector on
//     :12002, registers as OSC receiver, then loops reading OSC
//     messages. Grid presses/releases update a shared voices map;
//     presses also light the LED at (x, y), releases darken it.
//   - Audio thread (owned by cpal): each callback locks the voices
//     map briefly, advances phases, sums triangles, writes samples.
//
// Per-voice shape: linear 5 ms attack, hold, linear 50 ms release.
// Mixing: 0.15 amplitude per voice, sum clamped to ±0.95. Plenty of
// headroom up to ~6 simultaneous keys.
//
// Exit: Ctrl-C. (LEDs aren't cleared on exit; run it again and they'll
// update as you press.)

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use rosc::{decoder, encoder, OscMessage, OscPacket, OscType};
use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const PREFIX: &str = "/256-1-cable";
const DETECTOR_PORT: u16 = 12002;
const LISTEN_PORT: u16 = 9000;
const AMPLITUDE: f32 = 0.15;
const ATTACK_SECS: f32 = 0.003;
const RELEASE_SECS: f32 = 0.050;
// Accretion-born voices play at this fraction of full volume.
// Currently unused — wired up by the accretion state machine in a
// later commit. Kept here so the VoiceState envelope code can refer
// to it.
#[allow(dead_code)]
const ACCRETION_TARGET: f32 = 0.5;
// Hardcoded for the 256 (16×16) grid. If smaller/larger grids appear,
// query /sys/size from serialoscd at startup and replace these.
const GRID_W: i32 = 16;
const GRID_H: i32 = 16;

fn freq_for(x: i32, y: i32, fund: f64, edo: i32, x_step: i32, y_step: i32) -> f32 {
  freq_for_pitch(x_step * x + y_step * y, fund, edo)
}

// === Windows ============================================================

type Cell = (i32, i32);
// Inclusive corners.
type Rect = (Cell, Cell);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowId { Edo, Accretion2x2, EmitToggle1x1 }

#[derive(Debug, Clone, Copy)]
struct Window {
  id:   WindowId,
  rect: Rect,
}

fn rect_contains(rect: &Rect, cell: Cell) -> bool {
  let ((x0, y0), (x1, y1)) = *rect;
  let (x, y) = cell;
  x0 <= x && x <= x1 && y0 <= y && y <= y1
}

// Front-to-back: returns the first window whose rect contains `cell`.
fn window_for_cell(windows: &[Window], cell: Cell) -> Option<WindowId> {
  windows.iter().find(|w| rect_contains(&w.rect, cell)).map(|w| w.id)
}

// True iff window `from` "owns" `cell` — i.e. `from`'s rect contains
// `cell` AND no earlier (front-er) window's rect does. This is the
// compositor's only decision; pulled out as a pure fn for testing.
//
// PITFALL: the LedReasons map (later commit) is global state that
// every window writes into. Without this filter the EDO grid would
// quietly stomp the LEDs that control windows are managing. If you
// move windows around, also update what cells the EDO grid claims.
fn visible(windows: &[Window], from: WindowId, cell: Cell) -> bool {
  for w in windows {
    if w.id == from {
      return rect_contains(&w.rect, cell);
    }
    if rect_contains(&w.rect, cell) {
      return false;
    }
  }
  false
}

// LED-command compositor wrapper around send_osc. Drops writes for
// cells the calling window doesn't own.
fn set_led(
  windows: &[Window], from: WindowId, cell: Cell, on: bool,
  sock: &UdpSocket, device: SocketAddr, led_set: &str,
) {
  if !visible(windows, from, cell) { return; }
  send_osc(sock, device, led_set, vec![
    OscType::Int(cell.0), OscType::Int(cell.1),
    OscType::Int(if on { 1 } else { 0 }),
  ]);
}

// === Buttons ============================================================

// What a control-window cell does. Stored in per-window button maps
// on World; dispatched by ButtonRole, never by closure (closures
// fight Rust's borrow checker for &mut World).
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
enum Button {
  Toggle { state: bool, on: ButtonRole, off: ButtonRole },
  Nursed { state: bool, on: ButtonRole, off: ButtonRole },
  Fire   { fire: ButtonRole },
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
enum ButtonRole {
  AccreteOn, AccreteOff,
  EmitOn,    EmitOff,
  SilentFire,
  WipeFire,
  EmitIsToggleOn, EmitIsToggleOff,
}

// Per-grid pitch index. For each cell:
//   * its *absolute* pitch (in EDO units, octave preserved) — used
//     when storing a pitch in the accretion slot so emit can sound
//     it at the right frequency.
//   * its pitch *class* (absolute mod edo) — used to find the
//     enharmonic equivalents that should light together.
// Built once at startup; all lookups O(1).
struct PitchClass {
  key_to_pitch: HashMap<Cell, i32>,         // absolute
  key_to_class: HashMap<Cell, i32>,         // absolute mod edo
  class_to_keys: HashMap<i32, Vec<Cell>>,   // class -> all cells in that class
}

fn build_pitch_class(x_step: i32, y_step: i32, edo: i32, w: i32, h: i32) -> PitchClass {
  let mut k2p = HashMap::new();
  let mut k2c = HashMap::new();
  let mut c2k: HashMap<i32, Vec<Cell>> = HashMap::new();
  for x in 0..w {
    for y in 0..h {
      let abs = x_step * x + y_step * y;
      let cls = abs.rem_euclid(edo);
      k2p.insert((x, y), abs);
      k2c.insert((x, y), cls);
      c2k.entry(cls).or_default().push((x, y));
    }
  }
  PitchClass { key_to_pitch: k2p, key_to_class: k2c, class_to_keys: c2k }
}

// fund * 2^(pitch / edo). pitch is absolute (with octave).
fn freq_for_pitch(pitch: i32, fund: f64, edo: i32) -> f32 {
  (fund * 2.0_f64.powf(pitch as f64 / edo as f64)) as f32
}

// All cells whose LEDs reflect the same pitch class as `cell` —
// the pressed cell itself plus its enharmonic equivalents.
fn cells_for_pitch_of(pc: &PitchClass, cell: Cell) -> Vec<Cell> {
  pc.key_to_class.get(&cell)
    .and_then(|c| pc.class_to_keys.get(c))
    .cloned()
    .unwrap_or_else(|| vec![cell])
}

// All cells whose pitch class equals `pitch.rem_euclid(edo)`.
fn cells_for_pitch(pc: &PitchClass, pitch: i32, edo: i32) -> Vec<Cell> {
  let cls = pitch.rem_euclid(edo);
  pc.class_to_keys.get(&cls).cloned().unwrap_or_default()
}

// === Accretion slot =====================================================

// Pitch (absolute, with octave) → set of finger-voice ids that
// "first introduced" this pitch. The set is fixed on first
// insertion; later press events for the same pitch don't grow it.
// Wipe clears the whole map.
type PitchAccretion = HashMap<i32, HashSet<VoiceId>>;

// Is the voice with id `id` currently being held / sustained?
// "Alive" means target_env > 0 — voices ramping to 0 don't count;
// the user has lifted their finger (or emit went off) and the voice
// is on its way out, not blocking transform / spawn decisions.
fn voice_alive_with_id(voices: &VoiceMap, id: VoiceId) -> bool {
  voices.values().any(|v| v.id == id && v.target_env > 0.0)
}

// Spawn a fresh accretion voice for `pitch` at env=0 ramping to
// ACCRETION_TARGET over ATTACK_SECS. Overwrites any existing entry
// at Accreted{pitch}.
fn spawn_accretion_voice(
  voices: &mut VoiceMap, pitch: i32,
  fund: f64, edo: i32, next_voice_id: &mut VoiceId, sample_rate: f32,
) {
  let id = *next_voice_id;
  *next_voice_id += 1;
  voices.insert(VoiceSource::Accreted { pitch }, VoiceState {
    id,
    freq: freq_for_pitch(pitch, fund, edo),
    phase: 0.0,
    env: 0.0,
    target_env: ACCRETION_TARGET,
    ramp_per_sample: ACCRETION_TARGET / (ATTACK_SECS * sample_rate),
  });
}

// Set target_env=0 on every Accreted voice; ramp completes in
// RELEASE_SECS regardless of starting env.
fn ramp_all_accretion_to_zero(voices: &mut VoiceMap, sample_rate: f32) {
  for (src, v) in voices.iter_mut() {
    if matches!(src, VoiceSource::Accreted { .. }) {
      v.target_env = 0.0;
      v.ramp_per_sample = v.env / (RELEASE_SECS * sample_rate);
    }
  }
}

// === AppState ===========================================================

// One LED command: which window issued it, which cell it targets,
// and whether to turn the cell on. Goes through the compositor's
// visibility filter before reaching the device.
type LedCmd = (WindowId, Cell, bool);

struct AppState {
  voices:           Arc<Mutex<VoiceMap>>,
  pitch_accretion:  PitchAccretion,
  accrete_on:       bool,
  emit_on:          bool,
  emit_is_toggle:   bool,
  next_voice_id:    VoiceId,
  led_reasons:      LedReasons,
  control_buttons:  HashMap<Cell, Button>,  // (0,14), (1,14), (0,15), (1,15), (2,15)

  // Immutable config:
  pitch_class:      PitchClass,
  fund:             f64,
  edo:              i32,
  sample_rate:      f32,
}

impl AppState {
  fn new(
    voices: Arc<Mutex<VoiceMap>>,
    pitch_class: PitchClass,
    fund: f64, edo: i32, sample_rate: f32,
  ) -> Self {
    let mut control_buttons = HashMap::new();
    control_buttons.insert((0, 14),
      Button::Toggle { state: false, on: ButtonRole::AccreteOn,
                                     off: ButtonRole::AccreteOff });
    control_buttons.insert((1, 14), Button::Fire { fire: ButtonRole::WipeFire });
    control_buttons.insert((0, 15), Button::Fire { fire: ButtonRole::SilentFire });
    // Emit defaults to Toggle (since emit_is_toggle defaults to true).
    control_buttons.insert((1, 15),
      Button::Toggle { state: false, on: ButtonRole::EmitOn,
                                     off: ButtonRole::EmitOff });
    control_buttons.insert((2, 15),
      Button::Toggle { state: true,  on: ButtonRole::EmitIsToggleOn,
                                     off: ButtonRole::EmitIsToggleOff });
    AppState {
      voices, pitch_accretion: HashMap::new(),
      accrete_on: false, emit_on: false, emit_is_toggle: true,
      next_voice_id: 0, led_reasons: HashMap::new(),
      control_buttons,
      pitch_class, fund, edo, sample_rate,
    }
  }

  // EDO window press.
  fn edo_press(&mut self, cell: Cell) -> Vec<LedCmd> {
    let abs_pitch = match self.pitch_class.key_to_pitch.get(&cell) {
      Some(&p) => p,
      None => return vec![],
    };
    let id = self.next_voice_id;
    self.next_voice_id += 1;
    {
      let mut vs = self.voices.lock().unwrap();
      vs.insert(VoiceSource::Fingered { xy: cell }, VoiceState {
        id,
        freq: freq_for_pitch(abs_pitch, self.fund, self.edo),
        phase: 0.0, env: 0.0,
        target_env: 1.0,
        ramp_per_sample: 1.0 / (ATTACK_SECS * self.sample_rate),
      });
    }
    let mut diffs = vec![];
    // Pitch-equivalent lighting.
    let r = LedReason::PitchEquivalent { source_xy: cell };
    for c in cells_for_pitch_of(&self.pitch_class, cell) {
      if let Some(true) = add_reason(&mut self.led_reasons, c, r) {
        diffs.push((WindowId::Edo, c, true));
      }
    }
    // If accrete is on AND this pitch is not yet in the slot:
    // introduce it with this voice's id as the originVoice, and (if
    // emit is on) light the corresponding cells.
    if self.accrete_on && !self.pitch_accretion.contains_key(&abs_pitch) {
      let mut ids = HashSet::new();
      ids.insert(id);
      self.pitch_accretion.insert(abs_pitch, ids);
      if self.emit_on {
        let r = LedReason::Accretion { pitch: abs_pitch };
        for c in cells_for_pitch(&self.pitch_class, abs_pitch, self.edo) {
          if let Some(true) = add_reason(&mut self.led_reasons, c, r) {
            diffs.push((WindowId::Edo, c, true));
          }
        }
      }
    }
    diffs
  }

  // EDO window release.
  fn edo_release(&mut self, cell: Cell) -> Vec<LedCmd> {
    let abs_pitch = match self.pitch_class.key_to_pitch.get(&cell) {
      Some(&p) => p,
      None => return vec![],
    };
    let mut diffs = vec![];
    // Decide the voice's fate.
    let mut vs = self.voices.lock().unwrap();
    if let Some(this_voice) = vs.get(&VoiceSource::Fingered { xy: cell }).copied() {
      let in_slot = self.pitch_accretion.contains_key(&abs_pitch);
      let acc_voice_exists =
        vs.contains_key(&VoiceSource::Accreted { pitch: abs_pitch });
      let (is_originvoice, last_alive_originvoice) =
        match self.pitch_accretion.get(&abs_pitch) {
          None => (false, false),
          Some(originvoices) => {
            let mine = originvoices.contains(&this_voice.id);
            // "alive" matches voice_alive_with_id: target_env > 0,
            // i.e., the user is still holding that key.
            let any_other_alive = originvoices.iter().any(|&id| {
              id != this_voice.id
                && vs.values().any(|v| v.id == id && v.target_env > 0.0)
            });
            (mine, mine && !any_other_alive)
          }
        };
      let should_transform = in_slot && self.emit_on && !acc_voice_exists
                          && is_originvoice && last_alive_originvoice;
      if should_transform {
        let v = vs.remove(&VoiceSource::Fingered { xy: cell }).unwrap();
        vs.insert(VoiceSource::Accreted { pitch: abs_pitch }, VoiceState {
          id: v.id, freq: v.freq, phase: v.phase, env: v.env,
          target_env: ACCRETION_TARGET,
          ramp_per_sample:
            (v.env - ACCRETION_TARGET).abs() / (RELEASE_SECS * self.sample_rate),
        });
      } else if let Some(v) = vs.get_mut(&VoiceSource::Fingered { xy: cell }) {
        v.target_env = 0.0;
        v.ramp_per_sample = v.env / (RELEASE_SECS * self.sample_rate);
      }
    }
    drop(vs);
    // Pitch-equivalent LED reasons go away with the press.
    let r = LedReason::PitchEquivalent { source_xy: cell };
    for c in cells_for_pitch_of(&self.pitch_class, cell) {
      if let Some(false) = remove_reason(&mut self.led_reasons, c, r) {
        diffs.push((WindowId::Edo, c, false));
      }
    }
    diffs
  }

  // Press in any control window. Returns LED diffs (the button's
  // own light, plus any caused by the dispatched ButtonRole).
  fn control_press(&mut self, cell: Cell, win: WindowId) -> Vec<LedCmd> {
    let button = match self.control_buttons.get_mut(&cell) {
      Some(b) => b,
      None => return vec![],
    };
    let (role, new_state) = match button {
      Button::Toggle { state, on, off } => {
        *state = !*state;
        (Some(if *state { *on } else { *off }), Some(*state))
      }
      Button::Nursed { state, on, .. } => {
        *state = true;
        (Some(*on), Some(true))
      }
      Button::Fire { fire } => (Some(*fire), None),
    };
    let mut diffs = vec![];
    if let Some(lit) = new_state { diffs.push((win, cell, lit)); }
    if let Some(role) = role { diffs.extend(self.do_role(role)); }
    diffs
  }

  // Release in any control window. Only Nursed buttons produce an
  // off-event on release; Toggle and Fire ignore release.
  fn control_release(&mut self, cell: Cell, win: WindowId) -> Vec<LedCmd> {
    let button = match self.control_buttons.get_mut(&cell) {
      Some(b) => b,
      None => return vec![],
    };
    let (role, new_state) = match button {
      Button::Nursed { state, off, .. } => {
        *state = false;
        (Some(*off), Some(false))
      }
      _ => (None, None),
    };
    let mut diffs = vec![];
    if let Some(lit) = new_state { diffs.push((win, cell, lit)); }
    if let Some(role) = role { diffs.extend(self.do_role(role)); }
    diffs
  }

  // Apply one ButtonRole. Mutates state, returns LED diffs caused
  // by that mutation (from the EDO grid for Accretion reasons; the
  // button's own LED is the dispatcher's responsibility).
  fn do_role(&mut self, role: ButtonRole) -> Vec<LedCmd> {
    match role {
      ButtonRole::AccreteOn => {
        self.accrete_on = true;
        // Atomic snapshot per pitch.
        let vs = self.voices.lock().unwrap();
        let mut by_pitch: HashMap<i32, HashSet<VoiceId>> = HashMap::new();
        for (src, v) in vs.iter() {
          if let VoiceSource::Fingered { xy } = src {
            if let Some(&p) = self.pitch_class.key_to_pitch.get(xy) {
              by_pitch.entry(p).or_default().insert(v.id);
            }
          }
        }
        drop(vs);
        let mut newly_introduced: Vec<i32> = vec![];
        for (p, ids) in by_pitch {
          if !self.pitch_accretion.contains_key(&p) {
            self.pitch_accretion.insert(p, ids);
            newly_introduced.push(p);
          }
        }
        if self.emit_on {
          self.add_accretion_led_reasons(&newly_introduced)
        } else { vec![] }
      }
      ButtonRole::AccreteOff => {
        self.accrete_on = false;
        vec![]
      }
      ButtonRole::EmitOn => {
        self.emit_on = true;
        let pitches: Vec<i32> = self.pitch_accretion.keys().copied().collect();
        // Spawn accretion voices for pitches with no held originVoice.
        {
          let mut vs = self.voices.lock().unwrap();
          for &p in &pitches {
            let originvoices = &self.pitch_accretion[&p];
            let any_alive = originvoices.iter()
              .any(|&id| voice_alive_with_id(&vs, id));
            if !any_alive {
              spawn_accretion_voice(&mut vs, p, self.fund, self.edo,
                                    &mut self.next_voice_id, self.sample_rate);
            }
          }
        }
        self.add_accretion_led_reasons(&pitches)
      }
      // PITFALL: pressing silent while emit is in Nursed mode is
      // silly. The emit Button's `state` is still true (you're
      // holding the key); silent flips emit_on to false and the
      // accretion voices ramp out, but the next emit press / release
      // cycle restores them. To stop accretion in Nursed mode, just
      // release the emit key.
      ButtonRole::EmitOff | ButtonRole::SilentFire => {
        let was_on = self.emit_on;
        self.emit_on = false;
        if !was_on { return vec![]; }
        {
          let mut vs = self.voices.lock().unwrap();
          ramp_all_accretion_to_zero(&mut vs, self.sample_rate);
        }
        let pitches: Vec<i32> = self.pitch_accretion.keys().copied().collect();
        self.remove_accretion_led_reasons(&pitches)
      }
      ButtonRole::WipeFire => {
        let pitches: Vec<i32> = self.pitch_accretion.keys().copied().collect();
        self.pitch_accretion.clear();
        {
          let mut vs = self.voices.lock().unwrap();
          ramp_all_accretion_to_zero(&mut vs, self.sample_rate);
        }
        if self.emit_on {
          self.remove_accretion_led_reasons(&pitches)
        } else { vec![] }
      }
      ButtonRole::EmitIsToggleOn | ButtonRole::EmitIsToggleOff => {
        let new_mode_is_toggle = matches!(role, ButtonRole::EmitIsToggleOn);
        self.emit_is_toggle = new_mode_is_toggle;
        // Rebuild the emit Button at (1,15) with the new variant,
        // preserving its current `state` so that the audible /
        // visual emit state survives the mode flip (snapshot).
        let cur = self.control_buttons.get(&(1, 15)).copied();
        let cur_state = match cur {
          Some(Button::Toggle { state, .. }) | Some(Button::Nursed { state, .. }) => state,
          _ => false,
        };
        let new_btn = if new_mode_is_toggle {
          Button::Toggle { state: cur_state,
                           on: ButtonRole::EmitOn, off: ButtonRole::EmitOff }
        } else {
          Button::Nursed { state: cur_state,
                           on: ButtonRole::EmitOn, off: ButtonRole::EmitOff }
        };
        self.control_buttons.insert((1, 15), new_btn);
        vec![]
      }
    }
  }

  fn add_accretion_led_reasons(&mut self, pitches: &[i32]) -> Vec<LedCmd> {
    let mut diffs = vec![];
    for &p in pitches {
      let r = LedReason::Accretion { pitch: p };
      for c in cells_for_pitch(&self.pitch_class, p, self.edo) {
        if let Some(true) = add_reason(&mut self.led_reasons, c, r) {
          diffs.push((WindowId::Edo, c, true));
        }
      }
    }
    diffs
  }

  fn remove_accretion_led_reasons(&mut self, pitches: &[i32]) -> Vec<LedCmd> {
    let mut diffs = vec![];
    for &p in pitches {
      let r = LedReason::Accretion { pitch: p };
      for c in cells_for_pitch(&self.pitch_class, p, self.edo) {
        if let Some(false) = remove_reason(&mut self.led_reasons, c, r) {
          diffs.push((WindowId::Edo, c, false));
        }
      }
    }
    diffs
  }
}

// === LedReasons =========================================================

// Why a cell's LED is currently lit. A cell stays lit as long as it
// has ≥1 reason and goes dark when its reason set empties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum LedReason {
  PitchEquivalent { source_xy: Cell },   // a fingered key at source_xy is held
  Accretion       { pitch: i32 },        // pitch is in PitchAccretion AND emit_on
}

// Sparse: only cells with ≥1 reason appear here.
type LedReasons = HashMap<Cell, HashSet<LedReason>>;

// Mutate `reasons`; return Some(true) iff `cell` newly lit (was empty
// or absent, now has this reason). Returns None when the cell already
// had reasons, or when the reason was already present (no transition).
fn add_reason(reasons: &mut LedReasons, cell: Cell, r: LedReason) -> Option<bool> {
  let entry = reasons.entry(cell).or_default();
  let was_empty = entry.is_empty();
  let inserted = entry.insert(r);
  if was_empty && inserted { Some(true) } else { None }
}

// Mutate `reasons`; return Some(false) iff `cell` newly dark (had
// this reason, now has none). Returns None when no transition (cell
// wasn't lit, reason wasn't there, or other reasons remain).
fn remove_reason(reasons: &mut LedReasons, cell: Cell, r: LedReason) -> Option<bool> {
  let entry = match reasons.get_mut(&cell) {
    Some(s) => s,
    None => return None,
  };
  if !entry.remove(&r) { return None; }
  if entry.is_empty() {
    reasons.remove(&cell);
    Some(false)
  } else {
    None
  }
}

fn triangle(phase: f32) -> f32 {
  // phase is in [0, 1)
  if phase < 0.5 {
    4.0 * phase - 1.0
  } else {
    3.0 - 4.0 * phase
  }
}

// Per-voice audio state. The envelope is a simple "ramp env toward
// target_env by ramp_per_sample each sample, clamping at target." A
// voice is removed once env=0 AND target_env=0.
#[derive(Debug, Clone, Copy)]
struct VoiceState {
  id:              VoiceId,
  freq:            f32,
  phase:           f32,
  env:             f32,
  target_env:      f32,
  ramp_per_sample: f32,
}

type VoiceId = u64;

// What gave rise to this voice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum VoiceSource {
  Fingered { xy: (i32, i32) },
  Accreted { pitch: i32 },
}

type VoiceMap = HashMap<VoiceSource, VoiceState>;

// Render one cpal callback's worth of audio into `data` from `voices`.
// Pulled out of the closure so unit tests can exercise it without cpal
// or PipeWire.
fn render_block(
  voices: &mut VoiceMap,
  data: &mut [f32],
  channels: usize,
  sample_rate: f32,
) {
  for frame in data.chunks_mut(channels) {
    let mut mix = 0.0_f32;
    voices.retain(|_, v| {
      // Step env toward target_env.
      let delta = v.target_env - v.env;
      if delta.abs() <= v.ramp_per_sample {
        v.env = v.target_env;
      } else if delta > 0.0 {
        v.env += v.ramp_per_sample;
      } else {
        v.env -= v.ramp_per_sample;
      }
      // Voice is gone once env and target both reach 0.
      if v.env == 0.0 && v.target_env == 0.0 {
        return false;
      }
      v.phase += v.freq / sample_rate;
      if v.phase >= 1.0 {
        v.phase -= 1.0;
      }
      mix += triangle(v.phase) * v.env * AMPLITUDE;
      true
    });
    let s = mix.clamp(-0.95, 0.95);
    for out in frame.iter_mut() {
      *out = s;
    }
  }
}

fn send_osc(sock: &UdpSocket, dst: SocketAddr, addr: &str, args: Vec<OscType>) {
  let buf = encoder::encode(&OscPacket::Message(OscMessage {
    addr: addr.to_string(),
    args,
  }))
    .expect("encode OSC");
  // send_to can fail if the destination isn't up; ignore errors so a
  // transient hiccup doesn't crash the synth.
  let _ = sock.send_to(&buf, dst);
}

// Send everything a fresh device needs: route events here, set prefix,
// clear LEDs. Called on startup and again whenever serialoscd tells us
// the device has moved to a new port (which happens when serialoscd
// or the USB link restarts).
fn register(sock: &UdpSocket, device: SocketAddr) {
  send_osc(sock, device, "/sys/host", vec![OscType::String("127.0.0.1".into())]);
  send_osc(sock, device, "/sys/port", vec![OscType::Int(LISTEN_PORT as i32)]);
  send_osc(sock, device, "/sys/prefix", vec![OscType::String(PREFIX.into())]);
  send_osc(sock, device, &format!("{PREFIX}/grid/led/all"), vec![OscType::Int(0)]);
}

fn discover_device(sock: &UdpSocket) -> Option<u16> {
  let detector: SocketAddr = format!("127.0.0.1:{DETECTOR_PORT}").parse().ok()?;
  send_osc(
    sock,
    detector,
    "/serialosc/list",
    vec![
      OscType::String("127.0.0.1".into()),
      OscType::Int(LISTEN_PORT as i32),
    ],
  );
  let deadline = Instant::now() + Duration::from_secs(2);
  let mut buf = [0u8; 2048];
  while Instant::now() < deadline {
    if let Ok((n, _)) = sock.recv_from(&mut buf) {
      if let Ok((_, OscPacket::Message(m))) = decoder::decode_udp(&buf[..n]) {
        if m.addr == "/serialosc/device" && m.args.len() >= 3 {
          if let Some(OscType::Int(p)) = m.args.get(2) {
            return Some(*p as u16); }}} }}
  None }

fn main() {
  let args: Vec<String> = std::env::args().collect();
  // fund stays f64 (it's a continuous frequency in Hz). edo / x_step
  // / y_step are integer-only — without that, pitch math sees floating
  // -point jitter and accretion's "is this pitch in the slot?" check
  // gets racy. Reject non-integer here at startup.
  let fund: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(220.0);
  let edo: i32 = args.get(2).map(|s| s.parse::<i32>()
        .unwrap_or_else(|_| panic!("edo must be a positive integer, got {s:?}")))
    .unwrap_or(46);
  let x_step: i32 = args.get(3).map(|s| s.parse::<i32>()
        .unwrap_or_else(|_| panic!("x_step must be an integer, got {s:?}")))
    .unwrap_or(9);
  let y_step: i32 = args.get(4).map(|s| s.parse::<i32>()
        .unwrap_or_else(|_| panic!("y_step must be an integer, got {s:?}")))
    .unwrap_or(1);
  assert!(edo > 0, "edo must be positive, got {edo}");
  eprintln!("tuning: fund={fund} Hz  edo={edo}  x_step={x_step}  y_step={y_step}");

  // GRID_LISTEN_PORT overrides the default :9000 — useful for running
  // a second instance alongside the live one for testing.
  let listen_port: u16 = std::env::var("GRID_LISTEN_PORT").ok()
    .and_then(|s| s.parse().ok())
    .unwrap_or(LISTEN_PORT);
  let sock = UdpSocket::bind(("0.0.0.0", listen_port))
    .unwrap_or_else(|e| panic!("bind UDP :{listen_port}: {e}"));
  sock.set_read_timeout(Some(Duration::from_millis(50))).unwrap();

  // GRID_DEVICE_PORT skips serialoscd discovery — useful for running
  // the audio path without a monome plugged in (testing, etc.). LED
  // commands still get sent but go nowhere.
  let mut device_port = match std::env::var("GRID_DEVICE_PORT").ok()
    .and_then(|s| s.parse::<u16>().ok())
  {
    Some(p) => { eprintln!("GRID_DEVICE_PORT={p}; skipping discovery"); p }
    None => discover_device(&sock)
      .expect("no monome device found; is serialoscd running and a grid plugged in?"),
  };
  let mut device: SocketAddr = format!("127.0.0.1:{device_port}").parse().unwrap();
  eprintln!("device port: {device_port}");

  register(&sock, device);

  // Shared state: VoiceSource -> VoiceState. Both Fingered (a key is
  // held) and Accreted (the slot is sounding it) variants live here.
  let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));

  // --- Audio stream setup ---
  let host = cpal::default_host();
  let device_audio = host
    .default_output_device()
    .expect("no default output device");
  // Prefer a 48 kHz F32 config: PipeWire runs at 48 k natively, so
  // asking for 44.1 k forces an extra resampler in the chain. Match
  // the default config's channel count — picking the wrong one
  // (e.g. a mono variant PipeWire exposes but doesn't actually route)
  // results in a stream that opens but never fires its callback.
  let default_cfg = device_audio
    .default_output_config()
    .expect("no default output config");
  let default_channels = default_cfg.channels();
  let supported = device_audio
    .supported_output_configs()
    .expect("query output configs")
    .filter(|c| c.sample_format() == SampleFormat::F32
                && c.min_sample_rate().0 <= 48000
                && c.max_sample_rate().0 >= 48000)
    .max_by_key(|c| (c.channels() == default_channels, c.channels()))
    .map(|c| c.with_sample_rate(cpal::SampleRate(48000)))
    .unwrap_or_else(|| {
      eprintln!("no 48 kHz F32 config; falling back to default");
      default_cfg
    });
  let sample_format = supported.sample_format();
  let sample_rate = supported.sample_rate().0 as f32;
  let channels = supported.channels() as usize;
  let default_buf = supported.buffer_size().clone();
  // BUF env var picks buffer_size. Default is 128 frames (≈2.7 ms at
  // 48 kHz). Smaller → lower press-to-sound latency, but too small
  // can xrun. "default" → cpal's default; integer → that frame count.
  let buffer_size = match std::env::var("BUF").ok().as_deref() {
    Some("default") => cpal::BufferSize::Default,
    None => cpal::BufferSize::Fixed(128),
    Some(s) => match s.parse::<u32>() {
      Ok(n) => cpal::BufferSize::Fixed(n),
      Err(_) => {
        eprintln!("BUF={s:?} is neither 'default' nor an integer; using Fixed(128)");
        cpal::BufferSize::Fixed(128)
      }
    },
  };
  let config = cpal::StreamConfig {
    channels: supported.channels(),
    sample_rate: supported.sample_rate(),
    buffer_size,
  };
  eprintln!(
    "audio: {} Hz, {} channels, format={:?}, default_buf={:?}, requested_buf={:?}",
    sample_rate as u32, channels, sample_format, default_buf, config.buffer_size
  );
  assert!(
    sample_format == SampleFormat::F32,
    "this program assumes F32 samples; got {sample_format:?}"
  );

  let voices_audio = Arc::clone(&voices);
  let mut promoted = false;

  // Heartbeat counters. Read from the main loop ~1×/s; the audio
  // thread bumps them on every callback. Lets us tell three failure
  // modes apart when sound goes silent: (1) callbacks stopped firing
  // (PipeWire dropped the stream), (2) callbacks fire but peak=0 with
  // voices present (render bug), (3) callbacks fire and peak>0 but
  // you hear nothing (PipeWire routing — restart the graph).
  let cb_count       = Arc::new(AtomicU64::new(0));
  let sample_count   = Arc::new(AtomicU64::new(0));
  // Peak is f32; store it as bits so we can use AtomicU32. Race
  // between "load current" and "store new max" is fine — observability,
  // not correctness, and the worst case loses one sample's max.
  let peak_bits      = Arc::new(AtomicU32::new(0));
  let cb_count_audio     = Arc::clone(&cb_count);
  let sample_count_audio = Arc::clone(&sample_count);
  let peak_bits_audio    = Arc::clone(&peak_bits);

  let stream = device_audio
    .build_output_stream(
      &config,
      move |data: &mut [f32], _| {
        // On the very first callback, bump this thread to SCHED_FIFO
        // (realtime) via a direct syscall. Takes ~1 µs, no D-Bus.
        // Requires --cap-add=SYS_NICE --ulimit rtprio=99 on the
        // container (see docker.sh).
        if !promoted {
          promoted = true;
          unsafe {
            let param = libc::sched_param { sched_priority: 50 };
            let ret = libc::sched_setscheduler(0, libc::SCHED_FIFO, &param);
            if ret == 0 {
              eprintln!("audio thread: RT priority acquired (SCHED_FIFO 50)");
            } else {
              eprintln!(
                "audio thread: sched_setscheduler failed: {}; expect glitches at small buffers",
                std::io::Error::last_os_error()
              );
            }
          }
        }
        let mut voices = voices_audio.lock().unwrap();
        render_block(&mut voices, data, channels, sample_rate);
        // Heartbeat: count this callback, the frames we wrote, and
        // the peak |sample| we produced.
        cb_count_audio.fetch_add(1, Ordering::Relaxed);
        sample_count_audio.fetch_add((data.len() / channels) as u64, Ordering::Relaxed);
        let peak = data.iter().fold(0.0_f32, |a, &x| a.max(x.abs()));
        let peak_u = peak.to_bits();
        let cur = peak_bits_audio.load(Ordering::Relaxed);
        if peak_u > cur || f32::from_bits(cur) < peak {
          peak_bits_audio.store(peak_u, Ordering::Relaxed);
        }
      },
      |e| eprintln!("audio stream error: {e:?}"),
      None, )
    .expect("build output stream");
  stream.play().expect("start stream");

  eprintln!("ready. press keys on the grid. Ctrl-C to quit.");

  // Build pitch_class and the per-event state container.
  let pitch_class: PitchClass =
    build_pitch_class(x_step, y_step, edo, GRID_W, GRID_H);
  let mut state = AppState::new(
    Arc::clone(&voices), pitch_class, fund, edo, sample_rate,
  );

  // Windows, front-to-back. Smaller control windows occlude the EDO
  // grid below them.
  let windows: Vec<Window> = vec![
    Window { id: WindowId::Accretion2x2,   rect: ((0, 14), (1, 15)) },
    Window { id: WindowId::EmitToggle1x1,  rect: ((2, 15), (2, 15)) },
    Window { id: WindowId::Edo,            rect: ((0, 0),  (15, 15)) },
  ];

  // --- Main event loop: OSC from grid ---
  let key_addr = format!("{PREFIX}/grid/key");
  let led_set = format!("{PREFIX}/grid/led/set");
  let led_all = format!("{PREFIX}/grid/led/all");
  let mut buf = [0u8; 2048];

  // Push the initial LED state for any control button that boots
  // up "on" — e.g., emit-is-toggle starts in Toggle mode (state=true)
  // so its cell should be lit immediately. register() above already
  // cleared the whole grid; this paints the toggles back on.
  for (&cell, button) in &state.control_buttons {
    let lit = match button {
      Button::Toggle { state, .. } | Button::Nursed { state, .. } => *state,
      Button::Fire { .. } => false,
    };
    if lit {
      if let Some(win) = window_for_cell(&windows, cell) {
        set_led(&windows, win, cell, true, &sock, device, &led_set);
      }
    }
  }

  // SIGINT handler: just flip a flag the main loop watches. Avoids
  // running anything non-async-signal-safe from the handler itself.
  static STOP: AtomicBool = AtomicBool::new(false);
  extern "C" fn on_sigint(_: libc::c_int) { STOP.store(true, Ordering::SeqCst); }
  let handler: extern "C" fn(libc::c_int) = on_sigint;
  unsafe { libc::signal(libc::SIGINT, handler as libc::sighandler_t); }

  // Heartbeat cadence: poll the audio counters every HEARTBEAT_SECS
  // and log one summary line. STALL warning if no callbacks fired.
  const HEARTBEAT_SECS: f64 = 1.0;
  let mut last_heartbeat = Instant::now();
  loop {
    if STOP.load(Ordering::SeqCst) { break; }
    // Heartbeat — log once per HEARTBEAT_SECS regardless of OSC activity.
    let elapsed = last_heartbeat.elapsed();
    if elapsed.as_secs_f64() >= HEARTBEAT_SECS {
      let cb = cb_count.swap(0, Ordering::Relaxed);
      let frames = sample_count.swap(0, Ordering::Relaxed);
      let peak = f32::from_bits(peak_bits.swap(0, Ordering::Relaxed));
      let voice_n = state.voices.lock().unwrap().len();
      let elapsed_ms = elapsed.as_millis();
      eprintln!(
        "[hb] {elapsed_ms}ms cb={cb} frames={frames} voices={voice_n} peak={peak:.3}{}{}",
        if cb == 0 { " STALL no-callbacks" } else { "" },
        if voice_n > 0 && peak == 0.0 && cb > 0 {
          " WARN voices-but-silent"
        } else { "" },
      );
      last_heartbeat = Instant::now();
    }
    match sock.recv_from(&mut buf) {
      Ok((n, _)) => {
        let pkt = match decoder::decode_udp(&buf[..n]) {
          Ok((_, p)) => p,
          Err(e) => { eprintln!("  <- decode error: {e:?}"); continue; }
        };
        // Debug-log every packet (message or bundle) before filtering,
        // so we can tell arriving-but-wrong-address apart from nothing-at-all.
        match &pkt {
          OscPacket::Message(m) => eprintln!("  <- msg {} {:?}", m.addr, m.args),
          OscPacket::Bundle(b)  => eprintln!("  <- bundle ({} items)", b.content.len()),
        }
        if let OscPacket::Message(m) = pkt {
          // Device port may change under us when serialoscd restarts.
          // Any /serialosc/device announcement with a new port: re-register.
          if m.addr == "/serialosc/device" && m.args.len() >= 3 {
            if let Some(OscType::Int(p)) = m.args.get(2) {
              let p = *p as u16;
              if p != device_port {
                eprintln!("device port changed {device_port} -> {p}; re-registering");
                device_port = p;
                device = format!("127.0.0.1:{p}").parse().unwrap();
                register(&sock, device);
              }
            }
            continue;
          }
          if m.addr != key_addr || m.args.len() != 3 {
            continue;
          }
          let (x, y, s) = match (m.args.first(), m.args.get(1), m.args.get(2)) {
            (Some(OscType::Int(x)), Some(OscType::Int(y)), Some(OscType::Int(s))) => {
              (*x, *y, *s)
            }
            _ => continue,
          };
          let cell = (x, y);
          let win = match window_for_cell(&windows, cell) {
            Some(w) => w,
            None => continue,
          };
          let press = s == 1;
          let diffs: Vec<LedCmd> = match win {
            WindowId::Edo => {
              if press {
                let f = freq_for(x, y, fund, edo, x_step, y_step);
                eprintln!("press   x={x:>2} y={y:>2}  f={f:.2} Hz");
                state.edo_press(cell)
              } else {
                eprintln!("release x={x:>2} y={y:>2}");
                state.edo_release(cell)
              }
            }
            WindowId::Accretion2x2 | WindowId::EmitToggle1x1 => {
              eprintln!("{} control x={x:>2} y={y:>2}",
                        if press { "press  " } else { "release" });
              if press { state.control_press(cell, win) }
              else     { state.control_release(cell, win) }
            }
          };
          for (from, c, on) in diffs {
            set_led(&windows, from, c, on, &sock, device, &led_set);
          }
        }}
      Err(_) => { /* timeout, loop again */ }}}
  // Loop exited (Ctrl-C). Wipe the grid so we don't leave stale
  // LEDs lit on the device.
  send_osc(&sock, device, &led_all, vec![OscType::Int(0)]);
  eprintln!("monome cleared; bye.");
}

#[cfg(test)]
mod tests {
  use super::*;

  // Render N frames (mono) for a single voice with the given initial state.
  fn render_voice(v: VoiceState, n: usize, sample_rate: f32) -> Vec<f32> {
    let mut voices: VoiceMap = HashMap::new();
    voices.insert(VoiceSource::Fingered { xy: (0, 0) }, v);
    let mut data = vec![0.0_f32; n];
    render_block(&mut voices, &mut data, 1, sample_rate);
    data
  }

  #[test]
  fn empty_voices_produce_silence() {
    let mut voices: VoiceMap = HashMap::new();
    let mut data = vec![0.123_f32; 64]; // pre-fill nonzero
    render_block(&mut voices, &mut data, 1, 48000.0);
    assert!(data.iter().all(|&s| s == 0.0), "expected all zeros");
  }

  #[test]
  fn sustained_voice_produces_triangle_output() {
    let sr = 48000.0;
    let v = VoiceState {
      id: 0, freq: 440.0, phase: 0.0, env: 1.0,
      target_env: 1.0, ramp_per_sample: 0.0,
    };
    let data = render_voice(v, 1024, sr);
    let peak = data.iter().fold(0.0_f32, |a, &x| a.max(x.abs()));
    // Capped by AMPLITUDE=0.15.
    assert!(peak > 0.10 && peak < 0.20,
      "triangle peak out of range: {peak} (want 0.10..0.20)");
  }

  #[test]
  fn pitch_class_46_9_1_groups_x1_yminus9_together() {
    // Jeff's default layout: edo=46, x_step=9, y_step=1.
    // (x, y) and (x+1, y-9) should share a pitch class.
    let pc = build_pitch_class(9, 1, 46, 16, 16);
    let c_4_10 = pc.key_to_class[&(4, 10)];
    let c_5_1 = pc.key_to_class[&(5, 1)];
    assert_eq!(c_4_10, c_5_1, "(4,10) and (5,1) should be enharmonic");
    let group = &pc.class_to_keys[&c_4_10];
    assert!(group.contains(&(4, 10)));
    assert!(group.contains(&(5, 1)));
  }

  #[test]
  fn cells_for_pitch_of_with_pc_returns_full_equivalence_group() {
    let pc = build_pitch_class(9, 1, 46, 16, 16);
    let g = cells_for_pitch_of(&pc, (4, 10));
    assert!(g.contains(&(4, 10)));
    assert!(g.contains(&(5, 1)));
  }

  #[test]
  fn cells_for_pitch_of_with_unknown_cell_returns_just_itself() {
    let pc = build_pitch_class(9, 1, 46, 16, 16);
    // (-1,-1) isn't in the grid, so the lookup falls through to vec![cell].
    assert_eq!(cells_for_pitch_of(&pc, (-1, -1)), vec![(-1, -1)]);
  }

  #[test]
  fn add_then_remove_same_reason_returns_lit_then_dark_transition() {
    let mut reasons: LedReasons = HashMap::new();
    let r = LedReason::PitchEquivalent { source_xy: (3, 3) };
    assert_eq!(add_reason(&mut reasons, (4, 4), r), Some(true), "newly lit");
    assert_eq!(add_reason(&mut reasons, (4, 4), r), None, "already lit, same reason");
    assert_eq!(remove_reason(&mut reasons, (4, 4), r), Some(false), "newly dark");
    assert!(!reasons.contains_key(&(4, 4)));
  }

  #[test]
  fn two_reasons_must_both_be_removed_before_dark_transition() {
    let mut reasons: LedReasons = HashMap::new();
    let r1 = LedReason::PitchEquivalent { source_xy: (3, 3) };
    let r2 = LedReason::PitchEquivalent { source_xy: (5, 5) };
    add_reason(&mut reasons, (4, 4), r1);
    assert_eq!(add_reason(&mut reasons, (4, 4), r2), None,
               "second reason on already-lit cell: no transition");
    assert_eq!(remove_reason(&mut reasons, (4, 4), r1), None,
               "removing one of two: still lit");
    assert_eq!(remove_reason(&mut reasons, (4, 4), r2), Some(false),
               "removing the last: now dark");
  }

  #[test]
  fn pitch_class_group_press_release_via_led_reasons() {
    let pc = build_pitch_class(9, 1, 46, 16, 16);
    let mut reasons: LedReasons = HashMap::new();

    // Press (4,10): every cell in the pitch-class group transitions to lit.
    let r_410 = LedReason::PitchEquivalent { source_xy: (4, 10) };
    let mut newly_lit_410 = vec![];
    for c in cells_for_pitch_of(&pc, (4, 10)) {
      if let Some(true) = add_reason(&mut reasons, c, r_410) {
        newly_lit_410.push(c);
      }
    }
    assert!(newly_lit_410.contains(&(4, 10)));
    assert!(newly_lit_410.contains(&(5, 1)));

    // Press (5,1) — same group; no further LED transitions.
    let r_51 = LedReason::PitchEquivalent { source_xy: (5, 1) };
    for c in cells_for_pitch_of(&pc, (5, 1)) {
      assert_eq!(add_reason(&mut reasons, c, r_51), None,
                 "cell {c:?} should already be lit");
    }

    // Release (5,1) while (4,10) is still pressed — no dark transitions.
    for c in cells_for_pitch_of(&pc, (5, 1)) {
      assert_eq!(remove_reason(&mut reasons, c, r_51), None,
                 "cell {c:?} still lit by (4,10)'s reason");
    }

    // Release (4,10) — every group cell transitions to dark.
    let mut newly_dark = vec![];
    for c in cells_for_pitch_of(&pc, (4, 10)) {
      if let Some(false) = remove_reason(&mut reasons, c, r_410) {
        newly_dark.push(c);
      }
    }
    assert!(newly_dark.contains(&(4, 10)));
    assert!(newly_dark.contains(&(5, 1)));
  }

  fn standard_windows() -> Vec<Window> {
    vec![
      Window { id: WindowId::Accretion2x2,  rect: ((0, 14), (1, 15)) },
      Window { id: WindowId::EmitToggle1x1, rect: ((2, 15), (2, 15)) },
      Window { id: WindowId::Edo,           rect: ((0, 0),  (15, 15)) },
    ]
  }

  #[test]
  fn event_in_2x2_goes_to_accretion_window() {
    let ws = standard_windows();
    assert_eq!(window_for_cell(&ws, (0, 14)), Some(WindowId::Accretion2x2));
    assert_eq!(window_for_cell(&ws, (1, 15)), Some(WindowId::Accretion2x2));
  }

  #[test]
  fn event_in_1x1_goes_to_emit_toggle_window() {
    let ws = standard_windows();
    assert_eq!(window_for_cell(&ws, (2, 15)), Some(WindowId::EmitToggle1x1));
  }

  #[test]
  fn event_outside_smalls_goes_to_edo_window() {
    let ws = standard_windows();
    assert_eq!(window_for_cell(&ws, (0, 0)),  Some(WindowId::Edo));
    assert_eq!(window_for_cell(&ws, (8, 8)),  Some(WindowId::Edo));
    assert_eq!(window_for_cell(&ws, (15, 15)), Some(WindowId::Edo));
    assert_eq!(window_for_cell(&ws, (3, 15)), Some(WindowId::Edo)); // just past 1x1
  }

  #[test]
  fn edo_writing_into_2x2_cell_is_dropped() {
    let ws = standard_windows();
    assert!(!visible(&ws, WindowId::Edo, (0, 14)));
    assert!(!visible(&ws, WindowId::Edo, (1, 15)));
  }

  #[test]
  fn edo_writing_into_owned_cell_passes() {
    let ws = standard_windows();
    assert!(visible(&ws, WindowId::Edo, (0, 0)));
    assert!(visible(&ws, WindowId::Edo, (15, 14)));
  }

  #[test]
  fn accretion_window_writes_pass_for_its_own_cells() {
    let ws = standard_windows();
    assert!(visible(&ws, WindowId::Accretion2x2, (0, 14)));
    assert!(visible(&ws, WindowId::Accretion2x2, (1, 15)));
    // …but not into cells outside its rect.
    assert!(!visible(&ws, WindowId::Accretion2x2, (2, 15)));
    assert!(!visible(&ws, WindowId::Accretion2x2, (5, 5)));
  }

  #[test]
  fn release_decays_to_zero_and_drops_voice() {
    let sr = 48000.0;
    let release_samples = (RELEASE_SECS * sr) as usize; // 2400
    let v = VoiceState {
      id: 0, freq: 220.0, phase: 0.0, env: 1.0,
      target_env: 0.0,
      ramp_per_sample: 1.0 / (RELEASE_SECS * sr),
    };
    let mut voices: VoiceMap = HashMap::new();
    voices.insert(VoiceSource::Fingered { xy: (0, 0) }, v);
    let mut data = vec![0.0_f32; release_samples + 200];
    render_block(&mut voices, &mut data, 1, sr);
    assert!(voices.is_empty(), "voice should have been dropped after release");
    let tail = &data[release_samples + 100..];
    assert!(tail.iter().all(|&s| s == 0.0),
      "tail after release should be silent");
  }

  // === Accretion state-machine tests ===================================

  fn fresh_state() -> AppState {
    let voices = Arc::new(Mutex::new(HashMap::new()));
    let pc = build_pitch_class(9, 1, 46, 16, 16);
    AppState::new(voices, pc, 220.0, 46, 48000.0)
  }

  fn voice_keys(state: &AppState) -> HashSet<VoiceSource> {
    state.voices.lock().unwrap().keys().copied().collect()
  }

  #[test]
  fn press_then_release_with_accrete_on_emit_on_transforms_to_accretion() {
    let mut s = fresh_state();
    s.do_role(ButtonRole::AccreteOn);
    s.do_role(ButtonRole::EmitOn);
    let cell = (0, 0);  // pitch 0
    s.edo_press(cell);
    assert!(s.pitch_accretion.contains_key(&0));
    // Before release: only fingered voice exists.
    assert!(voice_keys(&s).contains(&VoiceSource::Fingered { xy: cell }));
    assert!(!voice_keys(&s).contains(&VoiceSource::Accreted { pitch: 0 }));
    s.edo_release(cell);
    // After release: voice transformed to Accreted.
    assert!(!voice_keys(&s).contains(&VoiceSource::Fingered { xy: cell }));
    assert!(voice_keys(&s).contains(&VoiceSource::Accreted { pitch: 0 }));
    let v = s.voices.lock().unwrap()[&VoiceSource::Accreted { pitch: 0 }];
    assert_eq!(v.target_env, ACCRETION_TARGET);
  }

  #[test]
  fn press_then_release_with_accrete_on_emit_off_leaves_no_accretion_voice() {
    let mut s = fresh_state();
    s.do_role(ButtonRole::AccreteOn);
    let cell = (0, 0);
    s.edo_press(cell);
    assert!(s.pitch_accretion.contains_key(&0));
    s.edo_release(cell);
    // Voice ramps to 0 normally; no accretion voice exists.
    assert!(!voice_keys(&s).contains(&VoiceSource::Accreted { pitch: 0 }));
    let vs = s.voices.lock().unwrap();
    let v = &vs[&VoiceSource::Fingered { xy: cell }];
    assert_eq!(v.target_env, 0.0);
  }

  #[test]
  fn emit_on_with_held_finger_does_not_spawn_accretion() {
    let mut s = fresh_state();
    s.do_role(ButtonRole::AccreteOn);
    let cell = (0, 0);
    s.edo_press(cell);  // pitch 0 enters slot via this press
    s.do_role(ButtonRole::EmitOn);
    // The originVoice is still alive: emit-on must skip the spawn.
    assert!(!voice_keys(&s).contains(&VoiceSource::Accreted { pitch: 0 }));
  }

  #[test]
  fn emit_on_with_dead_originvoice_spawns_accretion() {
    let mut s = fresh_state();
    s.do_role(ButtonRole::AccreteOn);
    let cell = (0, 0);
    s.edo_press(cell);
    s.edo_release(cell);
    // Voice is now mid-release, but it's a Fingered voice, not in
    // pitch_accretion's originVoice set as alive — wait, the id IS
    // still in voices. Let's drain it manually with render_block.
    let sr = 48000.0;
    let mut buf = vec![0.0_f32; (RELEASE_SECS * sr) as usize + 200];
    {
      let mut vs = s.voices.lock().unwrap();
      render_block(&mut vs, &mut buf, 1, sr);
    }
    assert!(voice_keys(&s).is_empty(), "originVoice should have decayed");
    s.do_role(ButtonRole::EmitOn);
    // Now no live originVoice; emit-on spawns a fresh accretion voice.
    assert!(voice_keys(&s).contains(&VoiceSource::Accreted { pitch: 0 }));
  }

  #[test]
  fn second_press_at_same_pitch_after_accretion_voice_sums_then_ramps_to_zero() {
    let mut s = fresh_state();
    // Get an accretion voice for pitch 0 going (no held originVoice).
    s.pitch_accretion.insert(0, [99].into_iter().collect()); // dead origin id
    s.do_role(ButtonRole::EmitOn);
    assert!(voice_keys(&s).contains(&VoiceSource::Accreted { pitch: 0 }));
    let cell = (0, 0);
    s.edo_press(cell);
    // Both voices coexist (the deliberate press always wins / sums).
    assert!(voice_keys(&s).contains(&VoiceSource::Fingered { xy: cell }));
    assert!(voice_keys(&s).contains(&VoiceSource::Accreted { pitch: 0 }));
    s.edo_release(cell);
    // Accretion voice exists, so finger ramps to 0 (no transform).
    let vs = s.voices.lock().unwrap();
    let f = &vs[&VoiceSource::Fingered { xy: cell }];
    assert_eq!(f.target_env, 0.0);
  }

  #[test]
  fn wipe_with_emit_on_clears_accretion_and_ramps_voices() {
    let mut s = fresh_state();
    s.do_role(ButtonRole::AccreteOn);
    s.do_role(ButtonRole::EmitOn);
    s.edo_press((0, 0));
    s.edo_release((0, 0));
    assert!(voice_keys(&s).contains(&VoiceSource::Accreted { pitch: 0 }));
    s.do_role(ButtonRole::WipeFire);
    assert!(s.pitch_accretion.is_empty());
    let vs = s.voices.lock().unwrap();
    let v = &vs[&VoiceSource::Accreted { pitch: 0 }];
    assert_eq!(v.target_env, 0.0, "wipe should ramp accretion voices to 0");
  }

  #[test]
  fn silent_with_emit_on_ramps_voices_and_sets_emit_off() {
    let mut s = fresh_state();
    s.pitch_accretion.insert(0, [99].into_iter().collect());
    s.do_role(ButtonRole::EmitOn);
    assert!(s.emit_on);
    s.do_role(ButtonRole::SilentFire);
    assert!(!s.emit_on);
    let vs = s.voices.lock().unwrap();
    let v = &vs[&VoiceSource::Accreted { pitch: 0 }];
    assert_eq!(v.target_env, 0.0);
  }

  #[test]
  fn silent_with_emit_off_is_noop() {
    let mut s = fresh_state();
    s.do_role(ButtonRole::SilentFire);
    assert!(!s.emit_on);
    assert!(voice_keys(&s).is_empty());
  }

  #[test]
  fn multi_enharmonic_originvoice_keeps_p_alive_until_last_release() {
    // (4,10) and (5,1) are enharmonic in 46/9/1 — same absolute pitch.
    let mut s = fresh_state();
    let p = s.pitch_class.key_to_pitch[&(4, 10)];
    assert_eq!(p, s.pitch_class.key_to_pitch[&(5, 1)]);
    // Hold both, then accrete (atomic snapshot).
    s.edo_press((4, 10));
    s.edo_press((5, 1));
    s.do_role(ButtonRole::AccreteOn);
    s.do_role(ButtonRole::EmitOn);
    assert_eq!(s.pitch_accretion[&p].len(), 2,
               "both finger voices should be co-originVoices");
    // Release one: not the last alive originVoice → ramp to 0, no transform.
    s.edo_release((4, 10));
    assert!(!voice_keys(&s).contains(&VoiceSource::Accreted { pitch: p }));
    // Release the other: now the last alive originVoice → transform.
    s.edo_release((5, 1));
    assert!(voice_keys(&s).contains(&VoiceSource::Accreted { pitch: p }));
  }

  #[test]
  fn accrete_on_snapshot_captures_held_keys_atomically_per_pitch() {
    let mut s = fresh_state();
    // Hold both enharmonic keys before accrete-on.
    s.edo_press((4, 10));
    s.edo_press((5, 1));
    let p = s.pitch_class.key_to_pitch[&(4, 10)];
    s.do_role(ButtonRole::AccreteOn);
    assert_eq!(s.pitch_accretion[&p].len(), 2);
  }

  #[test]
  fn accrete_on_snapshot_does_not_grow_existing_pitch_accretion_entry() {
    let mut s = fresh_state();
    // Put pitch 0 into the slot already (manually, with a dead id).
    s.pitch_accretion.insert(0, [99].into_iter().collect());
    // Hold a key for pitch 0, then snapshot via accrete-on.
    s.edo_press((0, 0));
    s.do_role(ButtonRole::AccreteOn);
    // Snapshot must NOT grow the originVoice set — leave the dead id alone.
    let set = &s.pitch_accretion[&0];
    assert_eq!(set.len(), 1);
    assert!(set.contains(&99));
  }

  #[test]
  fn emit_is_toggle_flip_does_not_disturb_emit_on() {
    let mut s = fresh_state();
    s.pitch_accretion.insert(0, [99].into_iter().collect());
    // Press the emit button itself, not just the role: the button's
    // own `state` field (UI on/off) gets flipped by the dispatcher,
    // and that's what the rebuild reads.
    s.control_press((1, 15), WindowId::Accretion2x2);
    assert!(s.emit_on);
    // Press emit-is-toggle (initially Toggle/state=true → state=false).
    s.control_press((2, 15), WindowId::EmitToggle1x1);
    assert!(s.emit_on, "emit state preserved across mode flip");
    assert!(!s.emit_is_toggle);
    // The emit Button at (1,15) is now a Nursed variant with state preserved.
    match s.control_buttons[&(1, 15)] {
      Button::Nursed { state, .. } => assert!(state),
      _ => panic!("emit button should be Nursed after flip"),
    }
  }
}
