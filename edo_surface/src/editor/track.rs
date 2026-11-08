//! A track: the notes it plays and the settings it plays them with, over time.
//!
//! A note is an integer pitch, read in the tuning in force where the note
//! starts. The settings are a timeline of changes: which rig the track sounds
//! through, its tuning, and the two parameters a player can overdub (the timbre
//! slot and the volume). The value of a setting at a time is that of its last
//! change at or before that time; before its first change a setting is unset.
//!
//! Time is integer milliseconds from the start of the song, so editing never
//! accumulates rounding and a saved track reloads exactly.

/// Milliseconds from the start of the song.
pub type Millis = i64;

/// What a track's integer pitches mean: pitch `p` sounds at
/// `fundamental_hz * 2^(p / edo)`. Unlike a rig's tuning it has no grid
/// layout, because a recorded pitch no longer depends on where it was played.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackTuning {
  pub edo: i32,
  pub fundamental_hz: f64,
}

impl TrackTuning {
  pub fn hz(
    &self,
    pitch: i32,
  ) -> f64 {
    self.fundamental_hz * 2f64.powf(pitch as f64 / self.edo as f64) }

  /// The pitch of this tuning nearest to `hz`.
  pub fn nearest_pitch(
    &self,
    hz: f64,
  ) -> i32 {
    (self.edo as f64 * (hz / self.fundamental_hz).log2()).round() as i32 }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Note {
  pub start: Millis,
  pub length: Millis,
  pub pitch: i32,
}

impl Note {
  pub fn end(&self) -> Millis { self.start + self.length }
}

/// One kind of setting, without its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SettingKind { Rig, Tuning, Timbre, Volume }

pub const SETTING_KINDS: [SettingKind; 4] =
  [SettingKind::Rig, SettingKind::Tuning, SettingKind::Timbre, SettingKind::Volume];

#[derive(Clone, Debug, PartialEq)]
pub enum Setting {
  /// The rig (a file name under `rigs/`, without `.org`) whose synth plays the
  /// track from here on.
  Rig(String),
  Tuning(TrackTuning),
  /// A slot of the rig's timbre selector, counted from 0 at the left.
  Timbre(usize),
  /// The volume in dB, 0 being the loudest the rig's volume keys allow.
  VolumeDb(f32),
}

impl Setting {
  pub fn kind(&self) -> SettingKind {
    match self {
      Setting::Rig(_) => SettingKind::Rig,
      Setting::Tuning(_) => SettingKind::Tuning,
      Setting::Timbre(_) => SettingKind::Timbre,
      Setting::VolumeDb(_) => SettingKind::Volume, } }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SettingChange {
  pub at: Millis,
  pub setting: Setting,
}

/// The value of every setting at one moment; `None` where it is still unset.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
  pub rig: Option<String>,
  pub tuning: Option<TrackTuning>,
  pub timbre: Option<usize>,
  pub volume_db: Option<f32>,
}

impl Settings {
  pub fn get(
    &self,
    kind: SettingKind,
  ) -> Option<Setting> {
    match kind {
      SettingKind::Rig => self.rig.clone().map(Setting::Rig),
      SettingKind::Tuning => self.tuning.map(Setting::Tuning),
      SettingKind::Timbre => self.timbre.map(Setting::Timbre),
      SettingKind::Volume => self.volume_db.map(Setting::VolumeDb), } }

  pub fn apply(
    &mut self,
    setting: &Setting,
  ) {
    match setting {
      Setting::Rig(rig) => self.rig = Some(rig.clone()),
      Setting::Tuning(tuning) => self.tuning = Some(*tuning),
      Setting::Timbre(slot) => self.timbre = Some(*slot),
      Setting::VolumeDb(db) => self.volume_db = Some(*db), } }

  /// One change per set setting, all at `at`.
  pub fn as_changes_at(
    &self,
    at: Millis,
  ) -> Vec<SettingChange> {
    SETTING_KINDS.iter()
      .filter_map(|kind| self.get(*kind))
      .map(|setting| SettingChange { at, setting })
      .collect() }
}

/// What a track holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TrackKind {
  /// Notes and the settings they sound with.
  #[default]
  Notes,
  /// Only section boundaries: moments that divide the song into scale
  /// ghost sections (see `Track::boundaries`).
  Boundaries,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Track {
  /// The file name's stem; the editor shows it in the track's header.
  pub name: String,
  pub kind: TrackKind,
  /// A muted track makes no sound, and so lights nothing on the monome.
  pub muted: bool,
  /// Sorted by start, then pitch.
  pub notes: Vec<Note>,
  /// Sorted by time, then kind; at most one change per kind and time, and
  /// none that repeats the value already in force (see `normalize`).
  pub changes: Vec<SettingChange>,
  /// A boundaries track's boundaries, ascending, without repeats. Each
  /// adjacent pair bounds a scale ghost section: while the music is in it,
  /// the grids show dim ghosts of every pitch any track plays in it. Empty
  /// on a notes track.
  pub boundaries: Vec<Millis>,
}

impl Track {
  pub fn new(name: &str) -> Track {
    Track { name: name.to_string(), ..Track::default() } }

  pub fn new_boundaries(name: &str) -> Track {
    Track { name: name.to_string(), kind: TrackKind::Boundaries, ..Track::default() } }

  /// The scale ghost section around `at`: the adjacent boundaries
  /// `[from, to)` with `from <= at < to`, if `at` lies between two.
  pub fn section_around(
    &self,
    at: Millis,
  ) -> Option<(Millis, Millis)> {
    let after: usize = self.boundaries.partition_point(|b| *b <= at);
    if after == 0 || after == self.boundaries.len() { return None; }
    Some((self.boundaries[after - 1], self.boundaries[after])) }

  /// Each note with its frequency in the tuning in force where it starts.
  pub fn notes_with_hz(&self) -> Vec<(Note, f64)> {
    let mut settings: Settings = Settings::default();
    let mut changes = self.changes.iter().peekable();
    let mut out: Vec<(Note, f64)> = Vec::with_capacity(self.notes.len());
    for note in &self.notes {
      while let Some(change) = changes.next_if(|change| change.at <= note.start) {
        settings.apply(&change.setting); }
      if let Some(tuning) = settings.tuning { out.push((*note, tuning.hz(note.pitch))); } }
    out }

  /// The settings in force at `at`, counting a change made exactly at `at`.
  pub fn settings_at(
    &self,
    at: Millis,
  ) -> Settings {
    self.settings_where(|change_at| change_at <= at) }

  /// The settings in force just before `at`, not counting a change at `at`.
  pub fn settings_before(
    &self,
    at: Millis,
  ) -> Settings {
    self.settings_where(|change_at| change_at < at) }

  fn settings_where(
    &self,
    counts: impl Fn(Millis) -> bool,
  ) -> Settings {
    let mut settings: Settings = Settings::default();
    for change in self.changes.iter().filter(|change| counts(change.at)) {
      settings.apply(&change.setting); }
    settings }

  /// When the track's last note or change ends.
  pub fn end(&self) -> Millis {
    let last_note: Millis = self.notes.iter().map(Note::end).max().unwrap_or(0);
    let last_change: Millis = self.changes.last().map(|change| change.at).unwrap_or(0);
    let last_boundary: Millis = self.boundaries.last().copied().unwrap_or(0);
    last_note.max(last_change).max(last_boundary) }

  /// Restore the invariants documented on `notes` and `changes`. Where two
  /// changes of one kind share a time, the later in the vector wins, so a
  /// caller can override by pushing.
  pub fn normalize(&mut self) {
    self.notes.retain(|note| note.length > 0);
    self.notes.sort_by_key(|note| (note.start, note.pitch, note.length));
    self.notes.dedup();
    self.boundaries.sort();
    self.boundaries.dedup();
    { // the last change pushed for a (time, kind) wins; stable sort keeps push order
      let mut indexed: Vec<(usize, SettingChange)> =
        self.changes.drain(..).enumerate().collect();
      indexed.sort_by_key(|(index, change)| (change.at, change.setting.kind(), *index));
      let mut kept: Vec<SettingChange> = Vec::with_capacity(indexed.len());
      for (_, change) in indexed {
        match kept.last() {
          Some(last) if last.at == change.at
            && last.setting.kind() == change.setting.kind() => {
            *kept.last_mut().unwrap() = change; }
          _ => kept.push(change), } }
      self.changes = kept; }
    { // drop a change that repeats the value in force
      let mut in_force: Settings = Settings::default();
      let mut kept: Vec<SettingChange> = Vec::with_capacity(self.changes.len());
      for change in self.changes.drain(..) {
        if in_force.get(change.setting.kind()).as_ref() != Some(&change.setting) {
          in_force.apply(&change.setting);
          kept.push(change); } }
      self.changes = kept; } }
}
