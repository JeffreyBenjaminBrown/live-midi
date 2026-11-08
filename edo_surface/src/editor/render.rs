//! Rendering a song to an mp3 file, offline: the tracks play through the
//! same playback and sound code as live, but into a buffer as fast as the
//! machine allows, and LAME encodes the buffer.
//!
//! Muted tracks stay silent; the speed is always 1.

use std::mem::MaybeUninit;
use std::sync::{Arc, Mutex};

use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, InterleavedPcm, Quality};

use crate::types::{AmShapeFamily, VoiceMap};
use crate::voices::BlockRenderer;

use super::playback::{PlaybackAction, PlayedTrack, Playback};
use super::song::SongTrack;
use super::sound::Sound;
use super::track::Millis;

pub const RENDER_SAMPLE_RATE: u32 = 48_000;
const CHANNELS: usize = 2;
/// Frames rendered between playback steps: about 1.3 ms at 48 kHz.
const BLOCK_FRAMES: usize = 64;
/// How long the render runs past the song's end, at most, for release tails.
const TAIL_MILLIS: Millis = 3000;

/// The song as interleaved stereo samples at `RENDER_SAMPLE_RATE`.
pub fn render_samples(tracks: &[SongTrack]) -> Vec<f32> {
  let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(VoiceMap::new()));
  let mut sound: Sound = Sound::new(Arc::clone(&voices), RENDER_SAMPLE_RATE as f32);
  let played: Vec<PlayedTrack> =
    tracks.iter().map(|t| PlayedTrack { id: t.id, track: &t.track }).collect();
  let end: Millis = tracks.iter().map(|t| t.track.end()).max().unwrap_or(0);
  let (mut playback, actions) = Playback::start(&played, 0);
  apply(&mut sound, &actions);
  let mut renderer: BlockRenderer = BlockRenderer::new(1);
  let mut out: Vec<f32> = vec![];
  let mut block: Vec<f32> = vec![0.0; BLOCK_FRAMES * CHANNELS];
  let mut frames: u64 = 0;
  loop {
    let at: Millis = (frames * 1000 / RENDER_SAMPLE_RATE as u64) as Millis;
    let actions: Vec<PlaybackAction> = playback.advance(&played, at);
    apply(&mut sound, &actions);
    let silent: bool = voices.lock().unwrap_or_else(|e| e.into_inner()).is_empty();
    if at >= end && (silent || at >= end + TAIL_MILLIS) { break; }
    {
      let mut voices = voices.lock().unwrap_or_else(|e| e.into_inner());
      renderer.render(&mut voices, &mut block, CHANNELS, RENDER_SAMPLE_RATE as f32, 1.0,
        AmShapeFamily::default()); }
    out.extend_from_slice(&block);
    frames += BLOCK_FRAMES as u64; }
  out }

fn apply(
  sound: &mut Sound,
  actions: &[PlaybackAction],
) {
  for action in actions { sound.playback(action); } }

/// Interleaved stereo samples at `sample_rate` as an mp3 file's bytes.
pub fn encode_mp3(
  samples: &[f32],
  sample_rate: u32,
) -> Result<Vec<u8>, String> {
  let mut builder: Builder = Builder::new().ok_or("LAME did not start")?;
  builder.set_num_channels(CHANNELS as u8).map_err(|e| format!("{e:?}"))?;
  builder.set_sample_rate(sample_rate).map_err(|e| format!("{e:?}"))?;
  builder.set_brate(Bitrate::Kbps256).map_err(|e| format!("{e:?}"))?;
  builder.set_quality(Quality::Best).map_err(|e| format!("{e:?}"))?;
  let mut encoder = builder.build().map_err(|e| format!("{e:?}"))?;
  let mut out: Vec<u8> = vec![];
  for chunk in samples.chunks(CHANNELS * 4096) {
    out.reserve(mp3lame_encoder::max_required_buffer_size(chunk.len() / CHANNELS));
    encoder.encode_to_vec(InterleavedPcm(chunk), &mut out).map_err(|e| format!("{e:?}"))?; }
  out.reserve(7200);
  let tail: &mut [MaybeUninit<u8>] = out.spare_capacity_mut();
  let flushed: usize = encoder.flush::<FlushNoGap>(tail).map_err(|e| format!("{e:?}"))?;
  unsafe { out.set_len(out.len() + flushed); } // flush wrote that many bytes into the spare room
  Ok(out) }

/// Render `tracks` and write them to `path` as an mp3.
pub fn render_mp3(
  tracks: &[SongTrack],
  path: &std::path::Path,
) -> Result<(), String> {
  let samples: Vec<f32> = render_samples(tracks);
  let bytes: Vec<u8> = encode_mp3(&samples, RENDER_SAMPLE_RATE)?;
  std::fs::write(path, bytes).map_err(|e| format!("write {}: {e}", path.display())) }

#[cfg(test)]
mod tests {
  use super::*;
  use crate::editor::track::{Note, Setting, SettingChange, Track, TrackTuning};

  const PLAIN: &str = "timbres-volume-scroll_41edo";

  fn one_note_song(muted: bool) -> Vec<SongTrack> {
    let mut track: Track = Track::new("t");
    track.muted = muted;
    track.changes = vec![
      SettingChange { at: 0, setting: Setting::Rig(PLAIN.into()) },
      SettingChange { at: 0, setting: Setting::Tuning(
        TrackTuning { edo: 41, fundamental_hz: 220.0 }) },
      SettingChange { at: 0, setting: Setting::Timbre(1) },
      SettingChange { at: 0, setting: Setting::VolumeDb(-6.0) }, ];
    track.notes = vec![Note { start: 100, length: 400, pitch: 0 }];
    vec![SongTrack { id: 0, track }] }

  #[test]
  fn a_note_renders_where_it_plays_and_a_muted_one_does_not() {
    let samples: Vec<f32> = render_samples(&one_note_song(false));
    let rms = |from_ms: usize, to_ms: usize| -> f32 {
      let span = &samples[from_ms * 96..to_ms * 96]; // 48 frames of 2 samples per ms
      (span.iter().map(|s| s * s).sum::<f32>() / span.len() as f32).sqrt() };
    assert!(rms(0, 90) < 1e-6, "silence before the note");
    assert!(rms(150, 450) > 0.005, "sound during it");
    assert!(samples.len() / 96 < 500 + 1000, "the render stops soon after the tail dies");
    let muted: Vec<f32> = render_samples(&one_note_song(true));
    assert!(muted.iter().all(|s| *s == 0.0)); }

  #[test]
  fn the_mp3_has_frames() {
    let samples: Vec<f32> = render_samples(&one_note_song(false));
    let bytes: Vec<u8> = encode_mp3(&samples, RENDER_SAMPLE_RATE).unwrap();
    assert!(bytes.len() > 1000);
    let has_frame_sync: bool = bytes.windows(2).any(|w| w[0] == 0xFF && w[1] & 0xE0 == 0xE0);
    assert!(has_frame_sync); }
}
