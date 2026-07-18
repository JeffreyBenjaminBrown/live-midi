//! Reading the DOREMiDi MPC-20 expression pedals as normalized 0.0..1.0 floats.
//!
//! These are the two M-Audio EX-P *expression* pedals (via the MPC-20), NOT the SoftStep's
//! foot pads (which the drumkit also calls "pedals"). The MPC-20's WCH CH345 chip makes no
//! ALSA MIDI device on Linux, so the pedals reach us via the host-side *bridge*
//! (`tools/pedals/bridge.py`; run it via `tools/pedals/bridge.sh` on the host), which
//! re-emits their MIDI on a virtual ALSA-seq port
//! named "MPC-20 pedals". This module connects to that port (midir) and tracks each pedal's
//! latest position. Background: `tools/pedals/{README,bridge-plan}.org`.
//!
//! The complex *use* of the pedals is not designed yet; this is just the library that lets
//! Rust SEE them (`PedalReader`) plus the pure normalization (`normalize`, `decode_pedal_cc`),
//! all unit-tested. The `pedal_monitor` bin proves it on screen.

use std::sync::{Arc, Mutex};

use midir::{MidiInput, MidiInputConnection, MidiInputPort};

/// Raw CC values at the ends of the EX-P pedals' travel (both trim pots opened all the way;
/// see `tools/pedals/README.org` "Observed range"). The pedals' real range is ~1..120,
/// not a full 0..127. Retune here (top-of-file consts, NOT per-rig) if the ends drift.
pub const PEDAL_RAW_BOTTOM: u8 = 1; // full heel -> 0.0
pub const PEDAL_RAW_TOP: u8 = 119; // full toe  -> 1.0

/// The MPC-20 sends both pedals on this Control Change number, one per MIDI channel.
pub const PEDAL_CC: u8 = 21;

/// Two pedals: MIDI channel 1 (0-based 0) = pedal 0; channel 2 (0-based 1) = pedal 1.
pub const NUM_PEDALS: usize = 2;

/// Default name substring of the bridge's virtual ALSA-seq port.
pub const DEFAULT_PORT: &str = "MPC-20 pedals";

/// Normalize a raw pedal CC (`PEDAL_RAW_BOTTOM`..`PEDAL_RAW_TOP`) to a continuous 0.0..1.0,
/// clamped -- so the pedal's real ~1..120 travel maps to a clean full-scale float.
pub fn normalize(raw: u8) -> f32 {
  let span = PEDAL_RAW_TOP as f32 - PEDAL_RAW_BOTTOM as f32;
  if span <= 0.0 {
    return 0.0;
  }
  ((raw as f32 - PEDAL_RAW_BOTTOM as f32) / span).clamp(0.0, 1.0)
}

/// If `message` is a pedal CC -- a Control Change on channel 0 or 1, controller `PEDAL_CC` --
/// return `(pedal index, raw value)`; otherwise `None`. Pure, the unit-testable core of the
/// decode. The bridge forwards one MIDI message per event (no running status), so a single
/// 3-byte CC is all we need to handle.
pub fn decode_pedal_cc(message: &[u8]) -> Option<(usize, u8)> {
  if message.len() < 3 {
    return None;
  }
  if message[0] & 0xF0 != 0xB0 {
    return None; // not a Control Change
  }
  let channel = (message[0] & 0x0F) as usize;
  if channel >= NUM_PEDALS || message[1] != PEDAL_CC {
    return None; // only ch 0/1, only CC 21
  }
  Some((channel, message[2] & 0x7F))
}

/// One pedal's latest reading.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pedal {
  /// Last raw CC value (0 until the first message).
  pub raw: u8,
  /// `normalize(raw)` -- the continuous 0.0..1.0 position.
  pub norm: f32,
  /// How many pedal CC messages this pedal has received (0 = never moved / not seen).
  pub updates: u64,
}

type Shared = Arc<Mutex<[Pedal; NUM_PEDALS]>>;

/// A live pedal reader: holds the midir input connection open and owns the shared state its
/// callback feeds. Drop it to disconnect. Cheap to poll via [`PedalReader::pedals`].
pub struct PedalReader {
  _conn: MidiInputConnection<()>,
  state: Shared,
  port_name: String,
}

impl PedalReader {
  /// Connect to the first MIDI input whose name contains `substring` (e.g. [`DEFAULT_PORT`])
  /// and start tracking the pedals. Errors (listing the available ports) if none match --
  /// usually meaning the host bridge is not running.
  pub fn connect(substring: &str) -> Result<PedalReader, String> {
    let midi_in = MidiInput::new("pedal-reader").map_err(|e| e.to_string())?;
    let port = select_port(&midi_in, substring)?;
    let port_name = midi_in.port_name(&port).unwrap_or_else(|_| "<unknown>".to_string());
    let state: Shared = Arc::new(Mutex::new([Pedal::default(); NUM_PEDALS]));
    let state_cb = Arc::clone(&state);
    let conn = midi_in
      .connect(
        &port,
        "pedal-reader-in",
        move |_timestamp, message, _| {
          if let Some((idx, raw)) = decode_pedal_cc(message) {
            let mut st = state_cb.lock().unwrap_or_else(|e| e.into_inner());
            st[idx] = Pedal { raw, norm: normalize(raw), updates: st[idx].updates + 1 };
          }
        },
        (),
      )
      .map_err(|e| format!("connect to {port_name:?}: {e}"))?;
    Ok(PedalReader { _conn: conn, state, port_name })
  }

  /// Snapshot both pedals' latest readings (index 0 = ch1, 1 = ch2).
  pub fn pedals(&self) -> [Pedal; NUM_PEDALS] {
    *self.state.lock().unwrap_or_else(|e| e.into_inner())
  }

  /// The full name of the connected port.
  pub fn port_name(&self) -> &str {
    &self.port_name
  }
}

/// Pick the first MIDI input port whose name contains `substring`.
fn select_port(midi_in: &MidiInput, substring: &str) -> Result<MidiInputPort, String> {
  let ports = midi_in.ports();
  if let Some(p) = ports.iter().find(|p| midi_in.port_name(p).is_ok_and(|n| n.contains(substring))) {
    return Ok(p.clone());
  }
  let available: Vec<String> = ports.iter().filter_map(|p| midi_in.port_name(p).ok()).collect();
  Err(format!(
    "no MIDI input matching {substring:?}; available: {available:?}. Is the bridge running? \
     (run tools/pedals/bridge.sh on the host)"
  ))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn normalize_maps_the_real_range_to_full_scale_and_clamps() {
    assert_eq!(normalize(PEDAL_RAW_BOTTOM), 0.0, "heel end -> 0.0");
    assert_eq!(normalize(PEDAL_RAW_TOP), 1.0, "toe end -> 1.0");
    assert_eq!(normalize(0), 0.0, "below the bottom clamps to 0.0");
    assert_eq!(normalize(127), 1.0, "above the top clamps to 1.0");
    let mid = normalize((PEDAL_RAW_BOTTOM + PEDAL_RAW_TOP) / 2);
    assert!((mid - 0.5).abs() < 0.02, "midpoint ~0.5: {mid}");
    // Monotonic across the travel.
    assert!(normalize(90) > normalize(30));
  }

  #[test]
  fn decode_accepts_only_pedal_ccs_on_the_two_channels() {
    assert_eq!(decode_pedal_cc(&[0xB0, 21, 60]), Some((0, 60)), "ch1 CC21 -> pedal 0");
    assert_eq!(decode_pedal_cc(&[0xB1, 21, 119]), Some((1, 119)), "ch2 CC21 -> pedal 1");
    assert_eq!(decode_pedal_cc(&[0xB2, 21, 60]), None, "ch3 is not a pedal");
    assert_eq!(decode_pedal_cc(&[0xB0, 22, 60]), None, "wrong controller");
    assert_eq!(decode_pedal_cc(&[0x90, 21, 60]), None, "note-on is not a CC");
    assert_eq!(decode_pedal_cc(&[0xB0, 21]), None, "truncated CC");
    assert_eq!(decode_pedal_cc(&[0xB0, 21, 200]), Some((0, 72)), "value masked to 7 bits");
  }
}
