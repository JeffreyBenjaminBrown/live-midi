//! The strings engine's listening harness: renders scripted scenes
//! to mono f32 WAV files, so string parameters can be judged by ear
//! without grids.
//!
//! cargo run -p tools --bin strings_harness -- [--rig NAME | --rig-file PATH]
//!   [--scene NAME] [--out DIR]
//!
//! '--rig' loads the named rig from 'rigs/', '--rig-file' a rig at
//! any path; the harness then renders with the first 'cpal_strings'
//! sink's parameters, sample rate and amplitude. Without either it
//! renders 'StringsParams::default()' at 48 kHz and amplitude 0.15.
//! Without '--scene' it renders every scene; without '--out' it
//! writes into 'target/strings-harness/' at the repository root. Each
//! scene lands in 'DIR/<scene>.wav'; the harness prints each file's
//! path and peak.
//!
//! A scene drives the engine as the surfaces runtime does: between
//! 128-frame blocks it inserts and edits 'VoiceState's in a
//! 'VoiceMap' (a strike, a release, a sustain, the cut of a sustained
//! pitch that a re-strike makes), and the engine's block render reads
//! the map. So what it renders is what a rig with the same
//! parameters plays.

use edo_surface::rig::{load_named_rig, load_rig_file, Rig, SinkRig};
use edo_surface::strings::scripted_voices::{release_voice_state, struck_voice_state};
use edo_surface::strings::{StringsParams, StringsRenderer};
use edo_surface::types::{VoiceId, VoiceMap, VoiceSource, VoiceState};
use hound::{SampleFormat, WavSpec, WavWriter};
use std::env;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::vec::IntoIter;

/// The sample rate without a rig.
const DEFAULT_SAMPLE_RATE: f32 = 48_000.0;

const BLOCK_FRAMES: usize = 128;

/// The sink amplitude without a rig.
const DEFAULT_AMPLITUDE: f32 = 0.15;

/// Every scene's voices are on this grid.
const GRID: usize = 0;

const SCENE_NAMES: [&str; 4] = ["single_notes", "chord", "restrike", "sympathy"];

/// The chord of 'chord' and 'restrike': a major triad with its octave,
/// 4:5:6:8 on 110 Hz.
const CHORD_HZ: [f32; 4] = [110.0, 137.5, 165.0, 220.0];

/// A scripted scene: its events, in time order, and its length.
struct Scene {
  name: &'static str,
  duration_seconds: f32,
  events: Vec<(f32, SceneEvent)>, }

/// What a scene does to the voice map at a moment, as the surfaces
/// runtime's grid threads would.
#[derive(Copy, Clone, Debug)]
enum SceneEvent {
  /// A finger strikes `cell` at `freq` Hz: a new voice, new id.
  Strike { cell: i32, freq: f32 },
  /// The finger on `cell` lifts with sustain off: its voice releases.
  Release { cell: i32 },
  /// The finger on `cell` lifts with sustain on: its voice moves to
  /// the drone key of `pitch`, keeping its id.
  Sustain { cell: i32, pitch: i32 },
  /// A re-strike of the sustained `pitch` cuts its drone: the voice
  /// moves to a retired key and releases.
  CutSustained { pitch: i32 }, }

struct Options {
  scene_names: Vec<String>,
  out_dir: PathBuf,
  rig_source: Option<RigSource>, }

/// Where '--rig' or '--rig-file' says the rig is.
enum RigSource {
  Named(String),
  File(PathBuf), }

/// What the scenes render with: a strings sink's parameters, its
/// sample rate and its amplitude.
struct Rendering {
  params: StringsParams,
  sample_rate: f32,
  amplitude: f32, }

fn main(
) -> ExitCode {
  let options: Options = match parse_options(env::args().skip(1).collect()) {
    Ok(options) => options,
    Err(message) => {
      eprintln!("strings_harness: {message}");
      eprintln!("usage: strings_harness [--rig NAME | --rig-file PATH] [--scene NAME] \
                 [--out DIR]; scenes: {}",
                SCENE_NAMES.join(", "));
      return ExitCode::from(2); }};
  let rendering: Rendering = match rendering_from(options.rig_source.as_ref()) {
    Ok(rendering) => rendering,
    Err(message) => {
      eprintln!("strings_harness: {message}");
      return ExitCode::FAILURE; }};
  if let Err(error) = fs::create_dir_all(&options.out_dir) {
    eprintln!("strings_harness: cannot create {}: {error}", options.out_dir.display());
    return ExitCode::FAILURE; }
  for scene_name in &options.scene_names {
    let scene: Scene = scene_named(scene_name).expect("parse_options checked the name");
    let rendered: Vec<f32> = render_scene(&scene, &rendering);
    let path: PathBuf = options.out_dir.join(format!("{}.wav", scene.name));
    if let Err(error) = write_mono_wav(&path, &rendered, rendering.sample_rate) {
      eprintln!("strings_harness: cannot write {}: {error}", path.display());
      return ExitCode::FAILURE; }
    let peak: f32 = rendered.iter().fold(0.0, |peak, sample| peak.max(sample.abs()));
    let shown_path: PathBuf = fs::canonicalize(&path).unwrap_or(path);
    println!("{}  peak {peak:.4}", shown_path.display()); }
  ExitCode::SUCCESS }

fn parse_options(
  arguments: Vec<String>,
) -> Result<Options, String> {
  let mut scene_names: Vec<String> = Vec::new();
  let mut out_dir: Option<PathBuf> = None;
  let mut rig_source: Option<RigSource> = None;
  let mut remaining: IntoIter<String> = arguments.into_iter();
  while let Some(argument) = remaining.next() {
    match argument.as_str() {
      "--scene" => {
        let name: String = remaining.next().ok_or("--scene needs a scene name")?;
        if scene_named(&name).is_none() {
          return Err(format!("no scene named {name}")); }
        scene_names.push(name); }
      "--out" => {
        out_dir = Some(PathBuf::from(remaining.next().ok_or("--out needs a directory")?)); }
      "--rig" | "--rig-file" => {
        if rig_source.is_some() {
          return Err("give at most one of --rig and --rig-file".to_string()); }
        let value: String =
          remaining.next().ok_or(format!("{argument} needs a rig name or path"))?;
        rig_source = Some(if argument == "--rig" {
          RigSource::Named(value)
        } else {
          RigSource::File(PathBuf::from(value)) }); }
      other => return Err(format!("unknown argument {other}")), }}
  if scene_names.is_empty() {
    scene_names = SCENE_NAMES.iter().map(|name| name.to_string()).collect(); }
  Ok(Options { scene_names, out_dir: out_dir.unwrap_or_else(default_out_dir), rig_source }) }

/// The rig's first 'cpal_strings' sink as a rendering, or the
/// defaults without a rig.
fn rendering_from(
  rig_source: Option<&RigSource>,
) -> Result<Rendering, String> {
  let Some(rig_source) = rig_source else {
    return Ok(Rendering {
      params: StringsParams::default(),
      sample_rate: DEFAULT_SAMPLE_RATE,
      amplitude: DEFAULT_AMPLITUDE, }); };
  let rig: Rig = match rig_source {
    RigSource::Named(name) => load_named_rig(name)?,
    RigSource::File(path) => load_rig_file(path)?, };
  rig.sinks.iter()
    .find_map(|sink| match sink {
      SinkRig::CpalStrings { sample_rate, amplitude, .. } => Some(Rendering {
        params: sink.strings_params()?,
        sample_rate: *sample_rate as f32,
        amplitude: *amplitude, }),
      _ => None, })
    .ok_or(format!("rig {:?} has no cpal_strings sink", rig.id)) }

/// 'target/strings-harness/' under the repository root (the parent
/// of this package's directory).
fn default_out_dir(
) -> PathBuf {
  let package_dir: &Path = Path::new(env!("CARGO_MANIFEST_DIR"));
  package_dir.parent().unwrap_or(package_dir).join("target").join("strings-harness") }

fn scene_named(
  name: &str,
) -> Option<Scene> {
  match name {
    "single_notes" => Some(single_notes_scene()),
    "chord" => Some(chord_scene()),
    "restrike" => Some(restrike_scene()),
    "sympathy" => Some(sympathy_scene()),
    _ => None, }}

/// 55, 110, 220, 440, 880 and 1760 Hz one after another, each 2 s,
/// the damper (the release) at 1.5 s.
fn single_notes_scene(
) -> Scene {
  let pitches_hz: [f32; 6] = [55.0, 110.0, 220.0, 440.0, 880.0, 1760.0];
  let events: Vec<(f32, SceneEvent)> = pitches_hz.iter()
    .enumerate()
    .flat_map(|(index, freq)| {
      let start: f32 = 2.0 * index as f32;
      let cell: i32 = index as i32;
      [ (start, SceneEvent::Strike { cell, freq: *freq }),
        (start + 1.5, SceneEvent::Release { cell }) ] })
    .collect();
  Scene { name: "single_notes", duration_seconds: 12.0, events }}

/// A major chord with its octave (110, 137.5, 165, 220 Hz: 4:5:6:8)
/// held 4 s, then released.
fn chord_scene(
) -> Scene {
  let mut events: Vec<(f32, SceneEvent)> = chord_strikes(0.0);
  events.extend((0 .. CHORD_HZ.len() as i32).map(|cell| (4.0, SceneEvent::Release { cell })));
  Scene { name: "chord", duration_seconds: 4.5, events }}

/// The chord struck and sustained (its fingers lift at 0.5 s with
/// sustain on), and its fifth struck again at 2 s: the runtime cuts
/// the sustained drone (it releases from a retired key) and strikes a
/// new voice, whose finger lifts, sustained, at 2.5 s.
fn restrike_scene(
) -> Scene {
  let mut events: Vec<(f32, SceneEvent)> = chord_strikes(0.0);
  events.extend((0 .. CHORD_HZ.len() as i32).map(|cell| {
    (0.5, SceneEvent::Sustain { cell, pitch: cell }) }));
  let restruck_cell: i32 = 2;
  events.push((2.0, SceneEvent::CutSustained { pitch: restruck_cell }));
  events.push((2.0, SceneEvent::Strike {
    cell: restruck_cell, freq: CHORD_HZ[restruck_cell as usize] }));
  events.push((2.5, SceneEvent::Sustain { cell: restruck_cell, pitch: restruck_cell }));
  Scene { name: "restrike", duration_seconds: 6.0, events }}

/// Four notes struck at 0 s and held (165, 220, 330 and 440 Hz: the
/// 3rd, 4th, 6th and 8th harmonics of 55 Hz), ringing quietly by 3 s
/// when a fresh, loud 110 Hz is struck, whose partials fall on all
/// four.
fn sympathy_scene(
) -> Scene {
  let quiet_hz: [f32; 4] = [165.0, 220.0, 330.0, 440.0];
  let mut events: Vec<(f32, SceneEvent)> = quiet_hz.iter()
    .enumerate()
    .map(|(index, freq)| (0.0, SceneEvent::Strike { cell: index as i32, freq: *freq }))
    .collect();
  events.push((3.0, SceneEvent::Strike { cell: quiet_hz.len() as i32, freq: 110.0 }));
  Scene { name: "sympathy", duration_seconds: 8.0, events }}

/// The chord's four strikes at `time`, one cell each.
fn chord_strikes(
  time: f32,
) -> Vec<(f32, SceneEvent)> {
  CHORD_HZ.iter()
    .enumerate()
    .map(|(cell, freq)| (time, SceneEvent::Strike { cell: cell as i32, freq: *freq }))
    .collect() }

/// The scene rendered in `BLOCK_FRAMES`-frame mono blocks, each
/// event applied to the map before the first block that starts at or
/// after its time.
fn render_scene(
  scene: &Scene,
  rendering: &Rendering,
) -> Vec<f32> {
  let sample_rate: f32 = rendering.sample_rate;
  let total_frames: usize = (scene.duration_seconds * sample_rate).round() as usize;
  let mut renderer: StringsRenderer = StringsRenderer::new(rendering.params, sample_rate);
  let mut voices: VoiceMap = VoiceMap::new();
  let mut next_voice_id: VoiceId = 0;
  let mut next_event: usize = 0;
  let mut rendered: Vec<f32> = Vec::with_capacity(total_frames + BLOCK_FRAMES);
  let mut block: [f32; BLOCK_FRAMES] = [0.0; BLOCK_FRAMES];
  while rendered.len() < total_frames {
    let block_start_seconds: f32 = rendered.len() as f32 / sample_rate;
    while next_event < scene.events.len() && scene.events[next_event].0 <= block_start_seconds {
      apply_event(&mut voices, &mut next_voice_id, scene.events[next_event].1, sample_rate);
      next_event += 1; }
    renderer.render_block(
      &mut voices, &mut block, 1, sample_rate, rendering.amplitude, &rendering.params,
      f32::NAN);
    rendered.extend_from_slice(&block); }
  rendered.truncate(total_frames);
  rendered }

/// `event` done to `voices` as the runtime's 'SurfaceSink' does it
/// ('note_on', 'note_off', 'sustain_note', 'cut_sustained').
fn apply_event(
  voices: &mut VoiceMap,
  next_voice_id: &mut VoiceId,
  event: SceneEvent,
  sample_rate: f32,
) {
  match event {
    SceneEvent::Strike { cell, freq } => {
      let voice: VoiceState = struck_voice_state(take_id(next_voice_id), freq, sample_rate);
      voices.insert(finger_key(cell), voice); }
    SceneEvent::Release { cell } => {
      if let Some(voice) = voices.get_mut(&finger_key(cell)) {
        release_voice_state(voice, sample_rate); }}
    SceneEvent::Sustain { cell, pitch } => {
      if let Some(voice) = voices.remove(&finger_key(cell)) {
        voices.insert(drone_key(pitch), voice); }}
    SceneEvent::CutSustained { pitch } => {
      if let Some(mut voice) = voices.remove(&drone_key(pitch)) {
        release_voice_state(&mut voice, sample_rate);
        let retired_key: VoiceSource =
          VoiceSource::SurfaceRetired { grid: GRID, seq: take_id(next_voice_id) };
        voices.insert(retired_key, voice); }}}}

/// The next id of the scene's counter, which, like the sink's, also
/// uniquifies retired keys.
fn take_id(
  next_voice_id: &mut VoiceId,
) -> VoiceId {
  let id: VoiceId = *next_voice_id;
  *next_voice_id += 1;
  id }

fn finger_key(
  cell: i32,
) -> VoiceSource {
  VoiceSource::SurfaceFinger { grid: GRID, cell: (cell, 0) }}

fn drone_key(
  pitch: i32,
) -> VoiceSource {
  VoiceSource::SurfaceDrone { grid: GRID, pitch }}

fn write_mono_wav(
  path: &Path,
  samples: &[f32],
  sample_rate: f32,
) -> Result<(), hound::Error> {
  let spec: WavSpec = WavSpec {
    channels: 1,
    sample_rate: sample_rate as u32,
    bits_per_sample: 32,
    sample_format: SampleFormat::Float, };
  let mut writer: WavWriter<BufWriter<File>> = WavWriter::create(path, spec)?;
  for sample in samples {
    writer.write_sample(*sample)?; }
  writer.finalize() }
