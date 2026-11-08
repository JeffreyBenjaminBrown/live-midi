//! The multitrack editor. Player reference: rigs/editor_readme.org.
//!
//!   pw-jack cargo run --bin editor -- [SONG_DIR] [--rig RIG] [--headless]
//!   cargo run --release --bin editor -- SONG_DIR --render OUT.mp3
//!
//! SONG_DIR defaults to songs/untitled at the repository root; RIG (a name
//! under rigs/) to the plain synth. --headless opens no window and reads
//! commands from stdin instead (play, stop, record, overdub, quit), for
//! running against the monome mock. --render writes the song to an mp3 and
//! quits, touching neither grids nor sound card. MIDI_PULSE_NO_AUDIO=1 opens
//! no sound card.

use std::io::BufRead;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use edo_surface::audio_output::{open_output, OutputStream};
use edo_surface::editor::grid_io::start_grids;
use edo_surface::editor::recorder::RecordMode;
use edo_surface::editor::render::render_mp3;
use edo_surface::editor::rig_layout::{editor_rig, EditorRig};
use edo_surface::editor::session::Session;
use edo_surface::editor::song::Song;
use edo_surface::editor::sound::Sound;
use edo_surface::editor::window::run_window;
use edo_surface::monome;
use edo_surface::rig::{load_named_rig, Rig, SinkRig};
use edo_surface::types::{AmShapeFamily, VoiceMap};
use edo_surface::voices::BlockRenderer;

const DEFAULT_RIG: &str = "timbres-volume-scroll_41edo";

/// Set by Ctrl-C.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_interrupt(_: libc::c_int) { INTERRUPTED.store(true, Ordering::SeqCst); }

struct Arguments {
  song_dir: PathBuf,
  rig: String,
  headless: bool,
  render_to: Option<PathBuf>,
}

fn main() {
  if let Err(error) = run(arguments()) {
    eprintln!("editor: {error}");
    std::process::exit(1); } }

fn arguments() -> Arguments {
  let mut arguments: Arguments = Arguments {
    song_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../songs/untitled"),
    rig: DEFAULT_RIG.into(),
    headless: false,
    render_to: None, };
  let mut words = std::env::args().skip(1);
  while let Some(word) = words.next() {
    match word.as_str() {
      "--rig" => arguments.rig = words.next().unwrap_or_else(|| DEFAULT_RIG.into()),
      "--headless" => arguments.headless = true,
      "--render" => arguments.render_to = words.next().map(PathBuf::from),
      dir => arguments.song_dir = PathBuf::from(dir), } }
  arguments }

fn run(arguments: Arguments) -> Result<(), String> {
  unsafe { libc::signal(libc::SIGINT, on_interrupt as *const () as libc::sighandler_t); }
  let rig: Rig = load_named_rig(&arguments.rig)?;
  let editor: EditorRig = editor_rig(&arguments.rig, &rig)?;
  for ignored in &editor.ignored { println!("editor: ignoring this rig's {ignored}"); }
  let song: Song = Song::load(&arguments.song_dir)?;
  if let Some(path) = &arguments.render_to {
    render_mp3(&song.tracks, path)?;
    println!("editor: rendered {}", path.display());
    return Ok(()); }
  println!("editor: song {} ({} tracks)", arguments.song_dir.display(), song.tracks.len());
  let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(VoiceMap::new()));
  let (sample_rate, buffer_frames): (u32, u32) = stream_request(&rig);
  let audio: Option<OutputStream> = if std::env::var_os("MIDI_PULSE_NO_AUDIO").is_some() {
    None } else { Some(start_audio(&voices, sample_rate, buffer_frames)?) };
  let real_rate: f32 = audio.as_ref().map_or(sample_rate as f32, |a| a.sample_rate);
  let session: Arc<Mutex<Session>> =
    Arc::new(Mutex::new(Session::new(song, editor, Sound::new(Arc::clone(&voices), real_rate))));
  let stop: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
  let grids = match start_grids(&session, &stop, monome::detector_port()) {
    Ok((threads, report)) => {
      for line in report { println!("editor: {line}"); }
      threads }
    Err(error) => {
      eprintln!("editor: no grids ({error})");
      vec![] } };
  let ticker = { // playback, and Ctrl-C
    let session: Arc<Mutex<Session>> = Arc::clone(&session);
    let stop: Arc<AtomicBool> = Arc::clone(&stop);
    thread::spawn(move || while !stop.load(Ordering::SeqCst) {
      if INTERRUPTED.load(Ordering::SeqCst) { stop.store(true, Ordering::SeqCst); }
      session.lock().unwrap_or_else(|e| e.into_inner()).tick(Instant::now());
      thread::sleep(Duration::from_millis(2)); }) };
  let result: Result<(), String> =
    if arguments.headless { run_headless(&session, &stop) } else { run_window(&session, &stop) };
  stop.store(true, Ordering::SeqCst);
  { // end a recording properly, so it is kept
    let mut session = session.lock().unwrap_or_else(|e| e.into_inner());
    session.stop(Instant::now());
    session.sound.silence(); }
  for grid in grids { let _ = grid.join(); }
  let _ = ticker.join();
  thread::sleep(Duration::from_millis(80)); // let the release tails finish
  drop(audio);
  result }

/// The sample rate and buffer size the rig's first synth sink asks for.
fn stream_request(rig: &Rig) -> (u32, u32) {
  rig.sinks.iter().find_map(|sink| match sink {
    SinkRig::CpalSynth { sample_rate, buffer_frames, .. } => Some((*sample_rate, *buffer_frames)),
    _ => None, }).unwrap_or((48000, 128)) }

fn start_audio(
  voices: &Arc<Mutex<VoiceMap>>,
  sample_rate: u32,
  buffer_frames: u32,
) -> Result<OutputStream, String> {
  let voices: Arc<Mutex<VoiceMap>> = Arc::clone(voices);
  open_output(sample_rate, buffer_frames, move |rate, channels| {
    let mut renderer: BlockRenderer = BlockRenderer::new(1);
    move |data: &mut [f32]| {
      let mut voices = voices.lock().unwrap_or_else(|e| e.into_inner());
      // each voice carries its rig's amplitude, so the master is unity
      renderer.render(&mut voices, data, channels, rate, 1.0, AmShapeFamily::default()); } }) }

/// No window: one command per stdin line.
fn run_headless(
  session: &Arc<Mutex<Session>>,
  stop: &Arc<AtomicBool>,
) -> Result<(), String> {
  println!("editor: headless; commands: play stop record overdub new quit");
  for line in std::io::stdin().lock().lines() {
    let Ok(line) = line else { break; };
    let now: Instant = Instant::now();
    let mut session = session.lock().unwrap_or_else(|e| e.into_inner());
    match line.trim() {
      "play" => session.play(now),
      "stop" => session.stop(now),
      "record" => session.record(now, RecordMode::Notes),
      "overdub" => session.record(now, RecordMode::Parameters),
      "new" => session.new_track(now),
      "quit" | "q" => break,
      other => println!("editor: unknown command {other:?}"), }
    println!("editor: {}", session.status);
    if stop.load(Ordering::SeqCst) { break; } }
  Ok(()) }
