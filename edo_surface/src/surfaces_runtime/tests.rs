  use super::*;
  use crate::rig::{load_named_rig, SlideRig, TapTempoRig, TrailRig};

  /// A real `GridThread` over the real =2-edogrids_ss-accrete_ss-pulse= rig's grid A, wired
  /// to a live voice map, for driving `handle_key` end to end without hardware.
  /// The socket binds an ephemeral port and nothing is ever sent or received on it;
  /// the accrete bank is momentary, exactly as `run()` builds it for this rig
  /// (no needs_holding control is bound anywhere).
  fn test_grid_thread() -> GridThread {
    let rig = load_named_rig("2-edogrids_ss-accrete_ss-pulse").expect("rig loads");
    let s = resolve_settings(&rig).expect("rig resolves");
    let num_grids = s.grids.len();
    let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));
    let live = Arc::new(Live::new(&s));
    let poly = {
      let mut p = PolyrhythmState::new(num_grids);
      p.set_fixed_tempo(1.0, Instant::now());
      Arc::new(Mutex::new(p))
    };
    let shared = Shared {
      selected: Arc::new(Mutex::new(vec![DEFAULT_SLOT; num_grids])),
      sounding: Arc::new(Mutex::new(vec![HashSet::new(); num_grids])),
      trail: Arc::new(Mutex::new(VecDeque::new())),
      volume_pos: Arc::new(Mutex::new(vec![0; num_grids])),
      gains: Arc::new(Mutex::new(vec![1.0; num_grids])),
      ring: Arc::new(Mutex::new(
        (0..num_grids).map(|_| GridRing::new(AccreteState::new_momentary())).collect(),
      )),
      held_all: Arc::new(Mutex::new(vec![HashMap::new(); num_grids])),
      distortion_on: Arc::new((0..num_grids).map(|_| AtomicBool::new(false)).collect()),
      slide_on: Arc::new((0..num_grids).map(|_| AtomicBool::new(false)).collect()),
      mono_on: Arc::new((0..num_grids).map(|_| AtomicBool::new(false)).collect()),
      pedal_slide_on: Arc::new((0..num_grids).map(|_| AtomicBool::new(false)).collect()),
      // NaN = "this pedal has never reported", exactly as `run()` initializes it.
      pedal_slide_frac: Arc::new(
        (0..num_grids).map(|_| AtomicU32::new(f32::NAN.to_bits())).collect(),
      ),
      poly,
      live,
      voices: Arc::clone(&voices),
      persist: None,
    };
    let g = &s.grids[0];
    GridThread {
      grid_index: 0,
      sock: UdpSocket::bind(("127.0.0.1", 0)).expect("bind an ephemeral socket"),
      prefix: g.prefix.clone(),
      listen_port: 0,
      device_id: "test".to_string(),
      device_port: 0,
      monobright: false,
      timbres: s.timbres,
      overlays: g.overlays,
      editmode_clear_down: false,
      editmode_accrete_down: false,
      tuning: Tuning {
        x_step: s.x_step,
        y_step: s.y_step,
        edo: s.edo,
        fund: s.fund,
        grid_w: s.grid_w,
        grid_h: s.grid_h,
      },
      knobs: Knobs {
        trail_clobber_radius: s.trail_clobber_radius,
        trails_max: s.trails_max,
        slide_window: s.slide_window,
        slide_duration_secs: s.slide_duration_secs,
        tap_window: s.tap_window,
        echo_input: false,
        controls_index: 0,
        volume_controls_index: 0,
      },
      shared,
      slide: SlideCandidates::new(),
      pedal_slide: pedal_slide::PedalSlideState::new(),
      fine: fine::FineTranspose::new(),
      started: Instant::now(),
      sink: SurfaceSink::new(
        0,
        voices,
        s.fund,
        s.edo,
        48000.0,
        s.attack,
        s.release,
        s.sustain_level,
        s.decay_secs,
        Arc::new(Mutex::new(vec![1.0; num_grids])),
        Arc::new(Mutex::new(vec![1.0; num_grids])),
      ),
    }
  }

  /// queues/branch-2.org "exit edit mode should not delete the voice", pinned as the
  /// exact hardware sequence through the REAL key handler: strike a note, enter edit
  /// via the handle below it, lift the finger (the note drones -- entering edit
  /// SUSTAINED it), then exit via the same handle. The drone must keep ringing: the
  /// voice's reason for existing is SUSTAIN (however it got there -- pedal or edit
  /// entry), edit membership is a pure selection, and exit only deselects. Ending
  /// the voice is the sustain-removal gestures' job (the toggle above, erase, the
  /// clears), never the exit's.
  #[test]
  fn the_per_voice_exit_gesture_ends_no_voice() {
    use crate::types::VoiceSource;
    let mut rt = test_grid_thread();
    let mut register = 0;
    let mut held = HashMap::new();
    let note = (5, 5); // a plain play cell in this rig (below the chord block)
    let handle = (5, 6); // the cell directly below it: the edit handle
    let pitch = step_for_cell(rt.tuning.x_step, rt.tuning.y_step, 0, note.0, note.1);

    handle_key(&mut rt, &mut register, &mut held, note, true); // strike
    handle_key(&mut rt, &mut register, &mut held, handle, true); // enter edit
    handle_key(&mut rt, &mut register, &mut held, handle, false);
    {
      let rings = rt.shared.ring.lock().unwrap();
      assert!(rings[0].store.has(Reason::Edit, pitch), "the note is selected");
      assert!(
        rings[0].store.has(Reason::Sustain, pitch),
        "and entering edit SUSTAINED it (edited ⊆ sustained)",
      );
    }
    handle_key(&mut rt, &mut register, &mut held, note, false); // lift: it drones
    let drone = VoiceSource::SurfaceDrone { grid: 0, pitch };
    assert_eq!(
      rt.shared.voices.lock().unwrap()[&drone].target_env,
      1.0,
      "the edited note drones after the finger lifts",
    );

    handle_key(&mut rt, &mut register, &mut held, handle, true); // exit edit
    handle_key(&mut rt, &mut register, &mut held, handle, false);
    {
      let rings = rt.shared.ring.lock().unwrap();
      assert!(!rings[0].store.has(Reason::Edit, pitch), "deselected");
      assert!(
        rings[0].store.has(Reason::Sustain, pitch),
        "but STILL sustained -- its reason for existing is sustain",
      );
    }
    assert_eq!(
      rt.shared.voices.lock().unwrap()[&drone].target_env,
      1.0,
      "exit edit mode does not delete the voice (queues/branch-2.org)",
    );

    // And the gesture that DOES end it still does: the sustain toggle (cell above).
    let above = (5, 4);
    handle_key(&mut rt, &mut register, &mut held, above, true);
    handle_key(&mut rt, &mut register, &mut held, above, false);
    assert_eq!(
      rt.shared.voices.lock().unwrap()[&drone].target_env,
      0.0,
      "removing its sustain is what ends it",
    );
  }

  /// queues/branch-2.org "for those [kill-sustain] controls, the origin doesn't
  /// matter": the LOCAL end-sustain (the handle above a note) is origin-blind. The
  /// full story through the real key handler: recall a chord, layer a finger on the
  /// same pitch, toggle sustain ON (the chord voice must survive -- only the END
  /// direction is a kill), lift (the piano note drones beside the chord voice),
  /// toggle again: OFF ends BOTH the drone and the chord voice at that pitch, and
  /// the emptied slot untoggles.
  #[test]
  fn the_local_end_sustain_also_ends_chord_voices_at_its_pitch() {
    use crate::surfaces_runtime::chords::{StoredChord, StoredVoice};
    use crate::types::{Timbre, VoiceSource};
    let mut rt = test_grid_thread();
    let mut register = 0;
    let mut held = HashMap::new();
    let note = (8, 8); // a plain play cell; its pitch is the chord voice's too
    let handle_above = (8, 7); // the sustain handle: the cell above the note
    let pitch = step_for_cell(rt.tuning.x_step, rt.tuning.y_step, 0, note.0, note.1);

    // The finger goes down FIRST, then the recall lands on the fingered pitch --
    // the direction that COEXISTS ("in the reverse situation, both voices should
    // coexist"; a finger landing on a preexisting chord voice instead restrikes
    // it -- see the separate restrike test). Slot 0 = the block's top-left, (5,0).
    handle_key(&mut rt, &mut register, &mut held, note, true);
    {
      let mut rings = rt.shared.ring.lock().unwrap();
      rings[0].chord.save(0, StoredChord { voices: vec![StoredVoice {
        pitch, timbre: Timbre::default(), fader_gain: 1.0, pedal_gain: 1.0,
        osc_phase: 0.0, pulse_factor: 0.0, pulse_phase: 0.0,
      }] });
    }
    handle_key(&mut rt, &mut register, &mut held, (5, 0), true);
    handle_key(&mut rt, &mut register, &mut held, (5, 0), false);
    let seq = *rt.shared.ring.lock().unwrap()[0].chord.live.keys().next().expect("recalled");
    let chord_voice = VoiceSource::SurfaceChord { grid: 0, seq };
    {
      let v = rt.shared.voices.lock().unwrap();
      assert_eq!(v[&chord_voice].target_env, 1.0, "the chord rings beside the finger");
      assert_eq!(
        v[&VoiceSource::SurfaceFinger { grid: 0, cell: note }].target_env, 1.0,
        "recall onto a fingered pitch coexists -- it never swallows the finger",
      );
    }

    // Toggling sustain ON for the fingered note spares the chord voice.
    handle_key(&mut rt, &mut register, &mut held, handle_above, true);
    handle_key(&mut rt, &mut register, &mut held, handle_above, false);
    assert!(rt.shared.ring.lock().unwrap()[0].store.has(Reason::Sustain, pitch), "toggled ON");
    assert_eq!(
      rt.shared.voices.lock().unwrap()[&chord_voice].target_env, 1.0,
      "the ON direction touches no chord voice",
    );

    // Lift: the piano note drones beside the chord voice.
    handle_key(&mut rt, &mut register, &mut held, note, false);
    let drone = VoiceSource::SurfaceDrone { grid: 0, pitch };
    assert_eq!(rt.shared.voices.lock().unwrap()[&drone].target_env, 1.0, "drone rings");

    // Toggle OFF: the origin-blind kill ends the drone AND the chord voice.
    handle_key(&mut rt, &mut register, &mut held, handle_above, true);
    handle_key(&mut rt, &mut register, &mut held, handle_above, false);
    {
      let v = rt.shared.voices.lock().unwrap();
      assert_eq!(v[&drone].target_env, 0.0, "the drone ends");
      assert_eq!(v[&chord_voice].target_env, 0.0, "and the chord voice ends with it");
    }
    let rings = rt.shared.ring.lock().unwrap();
    assert!(rings[0].chord.live.is_empty(), "the registry is pruned");
    assert!(!rings[0].chord.active[0], "the emptied slot untoggles");
    assert!(rings[0].chord.slots[0].is_some(), "the stored chord survives");
  }

  /// queues/branch-2.org: "re-fingering a sustained or chord pitch should
  /// retrigger it in the envelope sense but not in the polyrhythm pulse sense.
  /// That's what happens if a finger lands on a preexisting chord voice." Through
  /// the real key handler: pressing a chord voice's pitch restrikes IT -- no new
  /// voice, nothing entered into `held` (the later release releases nothing), the
  /// pulse untouched.
  #[test]
  fn a_finger_on_a_chord_pitch_restrikes_the_chord_voice_instead_of_playing() {
    use crate::surfaces_runtime::chords::{StoredChord, StoredVoice};
    use crate::types::{Timbre, VoiceSource};
    let mut rt = test_grid_thread();
    let mut register = 0;
    let mut held = HashMap::new();
    let note = (8, 8);
    let pitch = step_for_cell(rt.tuning.x_step, rt.tuning.y_step, 0, note.0, note.1);
    {
      let mut rings = rt.shared.ring.lock().unwrap();
      rings[0].chord.save(0, StoredChord { voices: vec![StoredVoice {
        pitch, timbre: Timbre::default(), fader_gain: 1.0, pedal_gain: 1.0,
        osc_phase: 0.3, pulse_factor: 3.0, pulse_phase: 0.6,
      }] });
    }
    handle_key(&mut rt, &mut register, &mut held, (5, 0), true); // recall slot 0
    handle_key(&mut rt, &mut register, &mut held, (5, 0), false);
    let seq = *rt.shared.ring.lock().unwrap()[0].chord.live.keys().next().expect("recalled");
    let chord_voice = VoiceSource::SurfaceChord { grid: 0, seq };

    // The finger lands ON the chord voice's pitch: restrike, not a note-on.
    handle_key(&mut rt, &mut register, &mut held, note, true);
    {
      let v = rt.shared.voices.lock().unwrap();
      let s = &v[&chord_voice];
      assert_eq!(s.target_env, 0.0, "the chord voice dips...");
      assert!(s.pending_attack.is_some(), "...into a fresh attack: the envelope retrigger");
      assert!((s.factored_pulse_freq - 3.0).abs() < 1e-6, "the pulse rate continues");
      assert_eq!(v.len(), 1, "no new voice spawned for the press");
    }
    assert!(held.is_empty(), "the press is a gesture, not a held note");
    // Its release is inert -- nothing was fingered.
    handle_key(&mut rt, &mut register, &mut held, note, false);
    assert_eq!(
      rt.shared.voices.lock().unwrap().len(), 1,
      "the restruck chord voice sails through the release",
    );
  }

  // ---- pedal slide: the WIRING-LEVEL invariant harness (TODO/pedal-slide/6_plan.org)
  //
  // The post-mortem's ranked cause 4: "the pure core was proven and the wiring was
  // not". 770 green tests pinned the anchored-segment math while all three bugs lived
  // in the hand-off between the engine, the voice map, and the render. So these tests
  // run the REAL path end to end -- real key handler, real engine step, real voice map,
  // real block renderer -- and assert what the EAR would check:
  //
  //   * an endpoint means the AUDIBLE frequency is exactly the endpoint's pitch;
  //   * the audible pitch never jumps, anywhere, including across a role swap;
  //   * after arrival, reversing reaches the pitch you came from, exactly.
  //
  // Each of those three would have failed on the reverted build.

  /// A pedal-slide rig under test: a real `GridThread` with one sustained + edited note
  /// at `h`, pedal slide ON, and the pedal parked at the heel.
  struct SlideRigUnderTest {
    rt: GridThread,
    register: i32,
    held: HashMap<(i32, i32), i32>,
    h: i32,
  }

  impl SlideRigUnderTest {
    /// Strike a note, edit it (which sustains it), lift the finger, turn pedal slide on.
    /// The pedal is treated as already resting at the heel, which is the ordinary case
    /// once a session has been played in.
    fn new() -> Self {
      let mut r = Self::new_untouched_pedal();
      // A real reading of 0.0 -- distinct from "never touched" (see
      // `the_first_ever_pedal_reading_settles_home_without_moving_any_pitch`).
      r.rt.shared.pedal_slide_frac[0].store(0.0_f32.to_bits(), Ordering::Relaxed);
      pedal_slide_step(&mut r.rt, &mut r.held);
      r
    }

    /// The same, but the pedal has never sent a CC -- so nothing is known about where
    /// the foot is, and the engine must not pretend otherwise.
    fn new_untouched_pedal() -> Self {
      let mut rt = test_grid_thread();
      let mut register = 0;
      let mut held = HashMap::new();
      let note = (5, 5);
      let handle = (5, 6); // the cell directly below: the edit handle
      let h = step_for_cell(rt.tuning.x_step, rt.tuning.y_step, 0, note.0, note.1);
      handle_key(&mut rt, &mut register, &mut held, note, true);
      handle_key(&mut rt, &mut register, &mut held, handle, true);
      handle_key(&mut rt, &mut register, &mut held, handle, false);
      handle_key(&mut rt, &mut register, &mut held, note, false); // drones, edited
      handle_key(&mut rt, &mut register, &mut held, (0, 15), true); // pedal slide ON
      assert!(rt.pedal_slide.mode(), "the engine entered slide mode");
      SlideRigUnderTest { rt, register, held, h }
    }

    fn press(&mut self, cell: (i32, i32)) {
      handle_key(&mut self.rt, &mut self.register, &mut self.held, cell, true);
      handle_key(&mut self.rt, &mut self.register, &mut self.held, cell, false);
    }

    /// The cell holding `pitch` under the current register (for picking targets).
    fn cell_for(&self, pitch: i32) -> (i32, i32) {
      *cells_for_pitch(&self.rt, self.register, pitch)
        .first()
        .unwrap_or_else(|| panic!("pitch {pitch} is on screen"))
    }

    /// Move the pedal to `f` and run ONE grid-thread step, then render 1 ms of audio so
    /// the render slew actually advances. Returns the audible pitch in EDO steps.
    fn pedal_to(&mut self, f: f32) -> f32 {
      self.rt.shared.pedal_slide_frac[0].store(f.to_bits(), Ordering::Relaxed);
      pedal_slide_step(&mut self.rt, &mut self.held);
      self.render_ms(1)
    }

    /// The loudest sample once the gain slew has settled. `peak_over_ms` alone takes
    /// the MAX over its window, so it reports whatever the level was on the way IN --
    /// useless for "is it quieter now". 200 ms is ~10 time constants of GAIN_SLEW_SECS.
    fn settled_peak(&mut self) -> f32 {
      self.render_ms(200);
      self.peak_over_ms(50)
    }

    /// The loudest sample over `ms` of real rendering -- what "silent" and "present"
    /// actually mean to an ear, as opposed to what a gain field claims.
    fn peak_over_ms(&mut self, ms: usize) -> f32 {
      let mut voices = self.rt.shared.voices.lock().unwrap();
      let mut data = vec![0.0_f32; 48 * ms];
      crate::voices::render_block(&mut voices, &mut data, 1, 48000.0);
      data.iter().fold(0.0_f32, |a, &x| a.max(x.abs()))
    }

    fn render_ms(&mut self, ms: usize) -> f32 {
      {
        let mut voices = self.rt.shared.voices.lock().unwrap();
        let mut data = vec![0.0_f32; 48 * ms];
        crate::voices::render_block(&mut voices, &mut data, 1, 48000.0);
      }
      self.audible()
    }

    /// The slid voice's pitch, in (fractional) EDO steps -- what the ear hears. Read
    /// through the pairing's own key, so this follows the voice across every re-file
    /// and re-key; once the slide has ended (a freeze), it falls back to the single
    /// remaining sounding voice. Release tails (`SurfaceRetired`) are never counted.
    fn audible(&self) -> f32 {
      let voices = self.rt.shared.voices.lock().unwrap();
      let key = self.rt.pedal_slide.pairings().first().map(|p| p.voice);
      let hz = match key.and_then(|k| voices.get(&k)) {
        Some(v) => v.freq,
        None => {
          let live: Vec<f32> = voices
            .iter()
            .filter(|(src, _)| !matches!(src, crate::types::VoiceSource::SurfaceRetired { .. }))
            .map(|(_, v)| v.freq)
            .collect();
          assert_eq!(live.len(), 1, "expected one un-retired voice, found {}", live.len());
          live[0]
        }
      };
      let (fund, edo) = (self.rt.tuning.fund, self.rt.tuning.edo);
      (hz as f64 / fund).log2() as f32 * edo as f32
    }

    /// Sweep the pedal from `from` to `to` in ~100 steps, letting the render advance at
    /// each one, and assert the audible pitch never JUMPS. Returns the settled pitch.
    fn sweep(&mut self, from: f32, to: f32) -> f32 {
      const STEPS: usize = 100;
      let mut last = self.render_ms(1);
      for i in 1..=STEPS {
        let f = from + (to - from) * (i as f32 / STEPS as f32);
        let now = self.pedal_to(f);
        assert!(
          (now - last).abs() < MAX_STEP_JUMP,
          "the audible pitch jumped {:.3} steps (from {last:.3} to {now:.3}) at f={f:.3} -- \
           a slide must never be discontinuous",
          (now - last).abs(),
        );
        last = now;
      }
      // Let the one-pole smoother settle onto wherever the map now points (300 ms is
      // ~10 time constants of SLIDE_SLEW_SECS -- the sweep has already walked the pitch
      // nearly there, so this only closes the last sliver).
      self.render_ms(300)
    }
  }

  /// The largest audible pitch change (EDO steps) tolerated between two 1 ms render
  /// blocks during a ~1 %-per-step pedal sweep. Generous enough not to be brittle,
  /// tight enough that any real discontinuity -- the reverted build froze mid-flight
  /// and its LEDs/pitch parted ways -- blows straight through it.
  const MAX_STEP_JUMP: f32 = 1.0;

  /// Slice 1, the whole of it: pick a target, sweep the pedal, and the note must ARRIVE
  /// -- exactly, audibly, through the real render.
  ///
  /// This is the assertion the reverted build failed. Its voice froze at
  /// MIDPOINT + HYSTERESIS_BAND (fraction 0.550) of the travel because the pairing's
  /// idea of the voice's key and the voice map's actual key parted ways at the role
  /// swap, and every drive afterwards was silently dropped (6_plan.org "diagnosis
  /// verification"). Nothing here can do that: the key moves only through a re-file
  /// this same thread has already applied.
  #[test]
  fn pedal_slide_reaches_its_target_exactly_through_the_real_render() {
    let mut r = SlideRigUnderTest::new();
    let t = r.h + 27; // the interval from Jeff's bug report
    let target_cell = r.cell_for(t);
    r.press(target_cell);
    assert_eq!(r.rt.pedal_slide.targets(), vec![t], "the press picked a target");
    assert!(
      r.rt.shared.ring.lock().unwrap()[0].store.has(Reason::Edit, r.h),
      "and did NOT drag the note -- the pedal does the moving",
    );

    let landed = r.sweep(0.0, 1.0);
    assert!(
      (landed - t as f32).abs() < 0.05,
      "the toe must land ON the target: wanted {t}, heard {landed:.3} \
       (the reverted build stopped at {:.3})",
      r.h as f32 + 0.55 * 27.0,
    );
  }

  /// Bug 3, pinned: arrival is a ROLE SWAP, not a completion. Having reached the
  /// target, the pedal must slide BACK to the pitch it came from -- and keep doing so,
  /// indefinitely. The reverted build's pedal went dead on arrival.
  #[test]
  fn after_arriving_the_pedal_slides_back_to_the_old_home_and_keeps_going() {
    let mut r = SlideRigUnderTest::new();
    let t = r.h + 27;
    let cell = r.cell_for(t);
    r.press(cell);

    for lap in 0..3 {
      let up = r.sweep(0.0, 1.0);
      assert!((up - t as f32).abs() < 0.05, "lap {lap}: toe reaches the target, heard {up:.3}");
      let down = r.sweep(1.0, 0.0);
      assert!(
        (down - r.h as f32).abs() < 0.05,
        "lap {lap}: heel returns to the pitch we came from, heard {down:.3} (wanted {})",
        r.h,
      );
    }
  }

  /// Arrival re-files the note, so the ring, the LEDs and the drone's KEY all move to
  /// the pitch that is now home -- together, in one thread, with the engine only
  /// learning of the move after it landed.
  #[test]
  fn arrival_refiles_the_note_and_swaps_the_led_roles() {
    let mut r = SlideRigUnderTest::new();
    let t = r.h + 27;
    let cell = r.cell_for(t);
    r.press(cell);
    r.sweep(0.0, 1.0);

    let rings = r.rt.shared.ring.lock().unwrap();
    assert!(rings[0].store.has(Reason::Edit, t), "the note is now filed at the target");
    assert!(rings[0].store.has(Reason::Sustain, t), "in both sets (edited ⊆ sustained)");
    assert!(!rings[0].store.has(Reason::Sustain, r.h), "and no longer at the pitch it left");
    drop(rings);

    let drone_at_target = crate::types::VoiceSource::SurfaceDrone { grid: 0, pitch: t };
    assert!(
      r.rt.shared.voices.lock().unwrap().contains_key(&drone_at_target),
      "a drone's key IS its filed pitch, so the voice re-keyed with the filing",
    );
    let (home, target) = r.rt.pedal_slide.led_roles();
    assert!(home.contains(&t), "the arrived-at pitch is lit and dancing as home");
    assert!(target.contains(&r.h), "and the pitch we came from now flashes as the target");
  }

  /// Retargeting mid-flight: the goalpost ahead moves, the pitch under the foot does
  /// not, and the NEW target is still reached exactly. (`1_vision`'s kink, at the
  /// wiring level rather than in the map's unit tests.)
  #[test]
  fn retargeting_mid_flight_never_jumps_and_still_lands_exactly() {
    let mut r = SlideRigUnderTest::new();
    let first = r.h + 27;
    let cell = r.cell_for(first);
    r.press(cell);
    r.sweep(0.0, 0.4);
    let before = r.render_ms(50);

    let second = r.h + 12;
    let cell = r.cell_for(second);
    r.press(cell);
    let after = r.render_ms(1);
    assert!(
      (after - before).abs() < MAX_STEP_JUMP,
      "picking a new target must not move the pitch under the foot ({before:.3} -> {after:.3})",
    );
    assert_eq!(r.rt.pedal_slide.targets(), vec![second], "the far goalpost moved");

    let landed = r.sweep(0.4, 1.0);
    assert!(
      (landed - second as f32).abs() < 0.05,
      "and the NEW target is reached exactly: wanted {second}, heard {landed:.3}",
    );
  }

  /// Slice 2's hysteresis, at the wiring level: reverse mid-flight and the way back is
  /// a FRESH straight line home, not a retrace of the way up -- so the pitch at a given
  /// pedal position is path-dependent, but the endpoints stay exact and nothing ever
  /// jumps. (The same trick as MIDI knob pickup; `2_discussion` calls this out as the
  /// point rather than a side effect.)
  #[test]
  fn reversing_after_a_mid_flight_retarget_still_lands_home_exactly() {
    let mut r = SlideRigUnderTest::new();
    let cell = r.cell_for(r.h + 12);
    r.press(cell);
    r.sweep(0.0, 0.5);

    // Retarget far away, pinning a big kink at f = 0.5, and climb a little.
    let far = r.cell_for(r.h + 40);
    r.press(far);
    let up = r.sweep(0.5, 0.75);
    assert!(up > r.h as f32 + 6.0, "the big new goalpost pulled the pitch up, at {up:.3}");

    // Now reverse all the way home. The descent is a new segment from here, and it
    // must still arrive exactly -- every step of it checked for jumps by `sweep`.
    let home = r.sweep(0.75, 0.0);
    assert!(
      (home - r.h as f32).abs() < 0.05,
      "the heel lands exactly home however kinked the way up was, heard {home:.3}",
    );
  }

  // ---- slice 3: the midpoint flip ----

  /// `1_vision`: "'home' switches from one side of the pedal to the other every time I
  /// reach it -- but actually before then. It has to switch when I cross the midpoint."
  /// The swap re-files the note and swaps the LED roles PART WAY UP, while the pitch is
  /// still in between -- and must move no pitch at all doing it.
  #[test]
  fn crossing_the_midpoint_swaps_the_roles_without_moving_the_pitch() {
    let mut r = SlideRigUnderTest::new();
    let t = r.h + 27;
    let cell = r.cell_for(t);
    r.press(cell);

    // Just below the band's far edge: nothing has swapped yet.
    r.sweep(0.0, 0.52);
    let before = r.render_ms(20);
    assert_eq!(r.rt.pedal_slide.home(), pedal_slide::Home::Low, "inside the band, no swap");
    assert!(
      r.rt.shared.ring.lock().unwrap()[0].store.has(Reason::Edit, r.h),
      "still filed at the pitch it came from",
    );

    // Cross it.
    let after = r.pedal_to(0.58);
    assert!(
      (after - before).abs() < MAX_STEP_JUMP,
      "the swap must move NO pitch -- the map does not mention home ({before:.3} -> {after:.3})",
    );
    assert_eq!(r.rt.pedal_slide.home(), pedal_slide::Home::High, "past the band: swapped");
    let rings = r.rt.shared.ring.lock().unwrap();
    assert!(rings[0].store.has(Reason::Edit, t), "the note re-filed to the target's pitch");
    assert!(!rings[0].store.has(Reason::Edit, r.h), "and left the one it came from");
    drop(rings);
    let (home, target) = r.rt.pedal_slide.led_roles();
    assert!(home.contains(&t) && target.contains(&r.h), "the colours swapped with it");

    // And the pitch still arrives exactly, having re-filed mid-flight.
    let landed = r.sweep(0.58, 1.0);
    assert!((landed - t as f32).abs() < 0.05, "still lands exactly, heard {landed:.3}");
  }

  /// The band exists so a foot RESTING near the middle cannot flicker the roles (and
  /// with them the LED colours and the end a new pick would replace) many times a
  /// second. Jitter across 0.5 must change nothing.
  #[test]
  fn a_foot_trembling_at_the_midpoint_does_not_flicker_the_roles() {
    let mut r = SlideRigUnderTest::new();
    let cell = r.cell_for(r.h + 27);
    r.press(cell);
    r.sweep(0.0, 0.5);
    let home_before = r.rt.pedal_slide.home();
    for f in [0.49, 0.52, 0.48, 0.53, 0.5, 0.47, 0.54] {
      r.pedal_to(f);
      assert_eq!(r.rt.pedal_slide.home(), home_before, "jitter at f={f} must not swap roles");
    }
  }

  /// A finger still DOWN on the note being slid. When it lifts, the voice moves from
  /// its cell key to its pitch key (`sustain_note`) -- and if the pairing does not
  /// follow, the pedal is left driving a key the voice map no longer has and the note
  /// freezes mid-glide. That is exactly the reverted build's failure, reached by a
  /// different route, so it is pinned here through the real key handler.
  #[test]
  fn lifting_the_finger_mid_slide_hands_the_pairing_to_the_drone() {
    let mut rt = test_grid_thread();
    let mut register = 0;
    let mut held = HashMap::new();
    let note = (5, 5);
    let handle = (5, 6);
    let h = step_for_cell(rt.tuning.x_step, rt.tuning.y_step, 0, note.0, note.1);
    handle_key(&mut rt, &mut register, &mut held, note, true); // strike, finger STAYS down
    handle_key(&mut rt, &mut register, &mut held, handle, true); // edit it (+ sustain)
    handle_key(&mut rt, &mut register, &mut held, handle, false);
    handle_key(&mut rt, &mut register, &mut held, (0, 15), true); // pedal slide ON

    let mut r = SlideRigUnderTest { rt, register, held, h };
    let t = h + 27;
    let cell = r.cell_for(t);
    r.press(cell);
    assert_eq!(
      r.rt.pedal_slide.pairings()[0].voice,
      crate::types::VoiceSource::SurfaceFinger { grid: 0, cell: note },
      "while fingered, the pairing addresses the CELL",
    );

    // Slide past the midpoint swap, which re-files a FINGERED voice's pitch (its key
    // does not move, but the ring and the held map must still follow, or the release
    // below looks up a pitch nobody is sustaining and cuts the note).
    r.sweep(0.0, 0.7);
    assert_eq!(r.held[&note], t, "the held map followed the re-file");

    // Now lift the finger.
    handle_key(&mut r.rt, &mut r.register, &mut r.held, note, false);
    assert_eq!(
      r.rt.pedal_slide.pairings()[0].voice,
      crate::types::VoiceSource::SurfaceDrone { grid: 0, pitch: t },
      "the pairing followed the voice to its drone key",
    );

    let landed = r.sweep(0.7, 1.0);
    assert!(
      (landed - t as f32).abs() < 0.05,
      "and the slide carries on to arrive exactly, heard {landed:.3}",
    );
  }

  /// The mirror gesture: a finger LANDS on a sliding drone to retrigger it. The voice
  /// becomes cell-keyed, and the slide must survive that too.
  #[test]
  fn retriggering_a_sliding_note_keeps_it_sliding() {
    let mut r = SlideRigUnderTest::new();
    let t = r.h + 27;
    let cell = r.cell_for(t);
    r.press(cell);
    r.sweep(0.0, 0.3);

    // Press the cell the note is filed at: a retrigger, not a pick (the pitch sounds).
    let home_cell = r.cell_for(r.h);
    handle_key(&mut r.rt, &mut r.register, &mut r.held, home_cell, true);
    assert_eq!(
      r.rt.pedal_slide.pairings()[0].voice,
      crate::types::VoiceSource::SurfaceFinger { grid: 0, cell: home_cell },
      "the retrigger took the voice over, and the pairing followed",
    );
    let landed = r.sweep(0.3, 1.0);
    assert!(
      (landed - t as f32).abs() < 0.05,
      "a retriggered note keeps sliding to its target, heard {landed:.3}",
    );
  }

  // ---- slice 4: the swell ----

  /// With NOTHING in edit mode, a pick makes the pedal a swell into that pitch --
  /// Jeff's chosen answer in `2_discussion`. Checked where it matters: the voice is
  /// really silent at the heel and really at full at the toe, measured as RENDERED
  /// AMPLITUDE, not as an engine number.
  #[test]
  fn a_pick_with_nothing_edited_swells_a_new_voice_in_from_silence() {
    let mut rt = test_grid_thread();
    let mut register = 0;
    let mut held = HashMap::new();
    handle_key(&mut rt, &mut register, &mut held, (0, 15), true); // pedal slide ON, nothing edited
    let mut r = SlideRigUnderTest { rt, register, held, h: 0 };

    let pitch = step_for_cell(r.rt.tuning.x_step, r.rt.tuning.y_step, 0, 7, 7);
    r.press((7, 7));
    let key = crate::types::VoiceSource::SurfaceDrone { grid: 0, pitch };
    assert!(
      r.rt.shared.voices.lock().unwrap().contains_key(&key),
      "the pick spawned a real drone voice",
    );
    assert!(
      r.rt.shared.ring.lock().unwrap()[0].store.has(Reason::Sustain, pitch),
      "and it joined the sustain set, so the clears and handles can reach it",
    );

    // At the heel it must be genuinely INAUDIBLE, envelope attack notwithstanding.
    let quiet = r.peak_over_ms(50);
    assert!(quiet < 1e-4, "silent at the heel, rendered peak {quiet:.6}");

    // Swell it in.
    for i in 1..=100 {
      r.pedal_to(i as f32 / 100.0);
    }
    let loud = r.peak_over_ms(50);
    assert!(loud > 0.01, "at the toe it is fully present, rendered peak {loud:.6}");
    assert!(
      (r.audible() - pitch as f32).abs() < 0.01,
      "and it sat at its own pitch throughout -- a swell moves volume, not pitch",
    );
  }

  // ---- slice 5: chords ----

  /// BUG 1, pinned: "sliding between two chords from chord storage, there is no pitch
  /// glide; it just crossfades between them" (`4_bugs-jeff-ran-into.org`).
  ///
  /// The cause was membership, not matching: the reverted build asked the EDIT
  /// SELECTION which voices the pedal managed, and a recalled chord's voices are never
  /// in it -- so the matcher saw nothing to pair, and every old voice became a fade-out
  /// and every new pitch a fade-in. Here the candidate list is the managed set, so the
  /// sounding chord's voices GLIDE into the stored chord's pitches.
  #[test]
  fn a_chord_recalled_under_slide_glides_the_sounding_voices_into_it() {
    use crate::surfaces_runtime::chords::{StoredChord, StoredVoice};
    let mut rt = test_grid_thread();
    let mut register = 0;
    let mut held = HashMap::new();

    // Two stored chords, three voices each, a clear interval apart.
    let sv = |pitch| StoredVoice {
      pitch,
      timbre: crate::types::Timbre::default(),
      fader_gain: 1.0,
      pedal_gain: 1.0,
      osc_phase: 0.0,
      pulse_factor: 0.0,
      pulse_phase: 0.0,
    };
    let low = [10, 20, 30];
    let high = [22, 32, 42];
    let spawned = {
      let mut rings = rt.shared.ring.lock().unwrap();
      rings[0].chord.save(0, StoredChord { voices: low.iter().map(|p| sv(*p)).collect() });
      rings[0].chord.save(1, StoredChord { voices: high.iter().map(|p| sv(*p)).collect() });
      rings[0].chord.begin_recall(0)
    };
    for (seq, v) in &spawned {
      rt.sink.spawn_chord_voice(*seq, v, 0.0);
    }
    handle_key(&mut rt, &mut register, &mut held, (0, 15), true); // pedal slide ON

    // Press slot 1: under slide mode this AIMS at the stored chord rather than
    // recalling it.
    let (sx, sy) = chords::slot_cell(rt.overlays.chord_rect, 1);
    handle_key(&mut rt, &mut register, &mut held, (sx, sy), true);

    let mut r = SlideRigUnderTest { rt, register, held, h: 0 };
    assert_eq!(r.rt.pedal_slide.target_slot(), Some(1), "slot 1 lights as the target chord");
    assert_eq!(
      r.rt.pedal_slide.pairings().len(),
      3,
      "all three SOUNDING chord voices are managed -- the ones the reverted build \
       could not see",
    );
    assert!(
      r.rt.pedal_slide.pairings().iter().all(|p| p.kind == pedal_slide::Kind::Pitch),
      "and every one of them is a PITCH slide, not a fade -- this is bug 1 exactly",
    );
    assert_eq!(
      r.rt.shared.voices.lock().unwrap().len(),
      3,
      "aiming at a chord spawns no voices: three in, three out, none doubled",
    );

    // Sweep, checking every voice arrives on its matched target. The ascending pass
    // pairs 10->22, 20->32, 30->42.
    for i in 1..=100 {
      r.pedal_to(i as f32 / 100.0);
    }
    r.render_ms(200);
    let voices = r.rt.shared.voices.lock().unwrap();
    let (fund, edo) = (r.rt.tuning.fund, r.rt.tuning.edo);
    let mut landed: Vec<i32> = voices
      .values()
      .map(|v| ((v.freq as f64 / fund).log2() * edo as f64).round() as i32)
      .collect();
    landed.sort_unstable();
    assert_eq!(landed, high, "every voice GLIDED onto its matched pitch");
  }

  /// The matcher's spare ends, through the real wiring: more target pitches than
  /// sounding voices means the extras swell in; the other way round means the leftovers
  /// fade out. Both are the vision's "just fade in the other voices" / "I guess
  /// similarly fade them out".
  #[test]
  fn a_bigger_target_chord_swells_the_extras_and_a_smaller_one_fades_the_leftovers() {
    let mut st = pedal_slide::PedalSlideState::new();
    st.enter(Some(0.0));
    let v = |p: i32| crate::types::VoiceSource::SurfaceDrone { grid: 0, pitch: p };

    // One voice, two targets: it takes the nearer, the other swells in.
    let out = st.match_targets(&[12, 40], &[(v(10), 10)]);
    assert_eq!(out.spawn_fade_ins, vec![40], "the voice takes 12 (nearer); 40 swells in");

    // Two voices, one target: the ascending pass gives it to the nearer, the other
    // fades out.
    let mut st = pedal_slide::PedalSlideState::new();
    st.enter(Some(0.0));
    let out = st.match_targets(&[11], &[(v(10), 10), (v(20), 20)]);
    assert!(out.spawn_fade_ins.is_empty(), "no spare targets");
    let kinds: Vec<_> = st.pairings().iter().map(|p| (p.voice, p.kind)).collect();
    assert!(
      kinds.contains(&(v(10), pedal_slide::Kind::Pitch)),
      "the matched voice slides: {kinds:?}",
    );
    assert!(
      kinds.contains(&(v(20), pedal_slide::Kind::Fade)),
      "the leftover fades out: {kinds:?}",
    );
  }

  /// "Already-satisfied pitches stay put" (`6_plan.org`'s slice-5 verify list): a voice
  /// already aimed at one of the new targets keeps that target rather than being
  /// re-shuffled onto a different one by the ascending pass.
  #[test]
  fn re_aiming_at_an_overlapping_chord_leaves_satisfied_voices_alone() {
    let mut st = pedal_slide::PedalSlideState::new();
    st.enter(Some(0.0));
    let v = |p: i32| crate::types::VoiceSource::SurfaceDrone { grid: 0, pitch: p };
    st.match_targets(&[20, 30], &[(v(10), 10), (v(25), 25)]);
    let aimed_before = st.targets();

    // The same chord again: nothing should move.
    st.match_targets(&[20, 30], &[(v(10), 10), (v(25), 25)]);
    assert_eq!(st.targets(), aimed_before, "re-aiming at the same chord is inert");
  }

  // ---- the untouched pedal (Jeff: "it seems to be assuming a certain point") ----

  /// An EX-P only sends CCs under a foot, so a session where the pedal has not been
  /// touched knows NOTHING about where it is resting. Assuming a position is not
  /// harmless: it decides which side is home, and so which endpoint holds each voice's
  /// current pitch. Assume the heel while the foot is at the toe and the first CC to
  /// arrive finds the map stretched between the wrong ends -- so the pitch LEAPS to
  /// wherever that CC falls. That is the discontinuity.
  ///
  /// Here the pedal is untouched, a target is picked, and then the pedal reports for
  /// the first time from the FAR side. The pitch must not move at all: that reading is
  /// a fact to adopt, not travel to follow.
  #[test]
  fn the_first_ever_pedal_reading_settles_home_without_moving_any_pitch() {
    let mut r = SlideRigUnderTest::new_untouched_pedal();
    assert!(!r.rt.pedal_slide.pedal_seen(), "no CC yet: the engine knows nothing");

    let t = r.h + 27;
    let cell = r.cell_for(t);
    r.press(cell);
    let before = r.render_ms(50);
    assert!((before - r.h as f32).abs() < 0.01, "still at its own pitch, heard {before:.3}");

    // The pedal speaks for the first time, from near the toe.
    let after = r.pedal_to(0.85);
    assert!(
      (after - before).abs() < 0.01,
      "the FIRST reading must move nothing ({before:.3} -> {after:.3}) -- it says where \
       the foot has been all along, it is not a sweep",
    );
    let settled = r.render_ms(200);
    assert!((settled - r.h as f32).abs() < 0.01, "and it stays put, heard {settled:.3}");

    // That side is now home, so the target is at the other end and travelling there
    // arrives exactly -- with no jump anywhere along the way.
    assert_eq!(r.rt.pedal_slide.home(), pedal_slide::Home::High, "the toe side became home");
    let landed = r.sweep(0.85, 0.0);
    assert!(
      (landed - t as f32).abs() < 0.05,
      "and the far (heel) end holds the target, heard {landed:.3} wanted {t}",
    );
  }

  /// The sentinel has to distinguish "never touched" from "resting at the heel" -- they
  /// are different facts and they choose different home sides. A pedal genuinely at 0.0
  /// makes the LOW side home, where an untouched one commits to nothing.
  #[test]
  fn a_pedal_resting_at_the_heel_is_not_the_same_as_one_never_touched() {
    let mut untouched = pedal_slide::PedalSlideState::new();
    untouched.enter(None);
    assert!(!untouched.pedal_seen());

    let mut at_heel = pedal_slide::PedalSlideState::new();
    at_heel.enter(Some(0.0));
    assert!(at_heel.pedal_seen(), "a real reading of 0.0 IS a reading");
    assert_eq!(at_heel.home(), pedal_slide::Home::Low);

    // The untouched one commits only when the pedal speaks -- and then to that side.
    untouched.on_pedal(0.9);
    assert!(untouched.pedal_seen());
    assert_eq!(untouched.home(), pedal_slide::Home::High, "the side it spoke from is home");
  }

  /// The wrong-way gate through the REAL render (Jeff's design, 2026-07-20): picked
  /// with the pedal parked mid-travel, going toward home holds the pitch and takes the
  /// note away in volume instead -- so the home pitch is never lost, and the wrong
  /// direction is audibly going nowhere.
  #[test]
  fn going_the_wrong_way_holds_the_pitch_and_fades_it_out_audibly() {
    let mut r = SlideRigUnderTest::new_untouched_pedal();
    // Park the pedal mid-travel and let the engine adopt that as its starting point.
    r.pedal_to(0.4);
    let t = r.h + 27;
    let cell = r.cell_for(t);
    r.press(cell);

    // The wrong way: the PITCH must not move at all, and the note must actually get
    // quieter -- measured as rendered peak, not as a gain field.
    let loud = r.settled_peak();
    for f in [0.3, 0.2, 0.1] {
      let pitch = r.pedal_to(f);
      assert!(
        (pitch - r.h as f32).abs() < 0.01,
        "at f={f} the pitch must hold at home, heard {pitch:.3}",
      );
    }
    let quiet = r.settled_peak();
    assert!(quiet < loud * 0.2, "going the wrong way must be audibly quieter: {loud} -> {quiet}");

    // Back to the pick point: the pitch is exactly home again, at full volume. The pin
    // did not follow the foot, so home was never lost.
    let back = r.pedal_to(0.4);
    assert!((back - r.h as f32).abs() < 0.01, "home recovered exactly, heard {back:.3}");
    // Compared against the FADED level, not the original: these notes are plucked, so
    // the envelope has decayed over the ~700 ms this test spends rendering, and an
    // absolute comparison would be measuring the pluck rather than the gate.
    let recovered = r.settled_peak();
    assert!(
      recovered > quiet * 5.0,
      "and the volume comes back at the pick point: {quiet} -> {recovered}",
    );

    // And onward the right way still arrives exactly.
    let landed = r.sweep(0.4, 1.0);
    assert!((landed - t as f32).abs() < 0.05, "arrives at the target, heard {landed:.3}");
  }

  /// Toggling pedal slide off mid-flight freezes the voice exactly where it is --
  /// smoothness above all -- and the pedal stops driving it.
  #[test]
  fn toggling_off_mid_flight_freezes_the_voice_where_it_is() {
    let mut r = SlideRigUnderTest::new();
    let cell = r.cell_for(r.h + 27);
    r.press(cell);
    let mid = r.sweep(0.0, 0.4);

    handle_key(&mut r.rt, &mut r.register, &mut r.held, (0, 15), false); // key-up: inert
    handle_key(&mut r.rt, &mut r.register, &mut r.held, (0, 15), true); // pedal slide OFF
    assert!(!r.rt.pedal_slide.mode(), "the engine left slide mode");
    assert!(
      !r.rt.shared.pedal_slide_on[0].load(Ordering::Relaxed),
      "and the pedal thread is told to take its volume back",
    );
    {
      let voices = r.rt.shared.voices.lock().unwrap();
      assert_eq!(
        voices.values().next().unwrap().slide_freq_target, 0.0,
        "the voice is frozen: nothing is driving its pitch any more",
      );
    }
    // Moving the pedal now must not move the pitch at all.
    let after = r.pedal_to(1.0);
    let after = r.render_ms(200).max(after);
    assert!(
      (after - mid).abs() < 0.05,
      "a frozen voice ignores the pedal ({mid:.3} -> {after:.3})",
    );
  }

  /// Clearing the edit selection mid-flight cancels the slide and freezes the voice
  /// (`2_discussion`: "deselection also cancels their slide pairing"). This is the call
  /// that, in the reverted build, fired SPURIOUSLY -- it read the ring while a re-key
  /// was still in flight and cancelled a pairing nobody had touched.
  #[test]
  fn deselecting_mid_flight_cancels_the_slide_but_a_live_one_survives_every_step() {
    let mut r = SlideRigUnderTest::new();
    let cell = r.cell_for(r.h + 27);
    r.press(cell);
    // A full round trip, crossing the role swap in both directions: the pairing must
    // survive every single step of it.
    r.sweep(0.0, 1.0);
    r.sweep(1.0, 0.0);
    assert!(!r.rt.pedal_slide.is_empty(), "the slide survived the whole round trip");

    // Now really deselect it, through the real editmode-clear button.
    handle_key(&mut r.rt, &mut r.register, &mut r.held, (12, 0), true);
    r.pedal_to(0.5);
    assert!(r.rt.pedal_slide.is_empty(), "deselection cancelled the pairing");
    let voices = r.rt.shared.voices.lock().unwrap();
    assert_eq!(
      voices.values().next().unwrap().slide_freq_target, 0.0,
      "and the voice was frozen where it stood",
    );
  }

  /// queues/branch-2.org "in edit mode, octave switchers should edit the edit-moded
  /// voices": with a live edit selection, the octave corners retune EVERY edited
  /// voice (both layers) by an octave -- down-corner lower, up-corner higher -- and
  /// the register does not move; with no selection they scroll as ever.
  #[test]
  fn octave_switchers_retune_the_edit_selection_instead_of_scrolling() {
    use crate::pitch::freq_for_pitch;
    use crate::surfaces_runtime::chords::{StoredChord, StoredVoice};
    use crate::types::{Timbre, VoiceSource};
    let mut rt = test_grid_thread();
    let mut register = 0;
    let mut held = HashMap::new();
    let edo = rt.tuning.edo;
    let (up_corner, down_corner) = ((15, 14), (13, 14)); // scroll pad [13,14,15,15]
    let note = (5, 5);
    let pitch = step_for_cell(rt.tuning.x_step, rt.tuning.y_step, 0, note.0, note.1);

    // A fingered note in edit mode, and an edit-flagged chord voice at another pitch.
    handle_key(&mut rt, &mut register, &mut held, note, true);
    handle_key(&mut rt, &mut register, &mut held, (5, 6), true); // edit handle
    handle_key(&mut rt, &mut register, &mut held, (5, 6), false);
    let chord_pitch = step_for_cell(rt.tuning.x_step, rt.tuning.y_step, 0, 8, 8);
    {
      let mut rings = rt.shared.ring.lock().unwrap();
      rings[0].chord.save(0, StoredChord { voices: vec![StoredVoice {
        pitch: chord_pitch, timbre: Timbre::default(), fader_gain: 1.0, pedal_gain: 1.0,
        osc_phase: 0.0, pulse_factor: 0.0, pulse_phase: 0.0,
      }] });
    }
    handle_key(&mut rt, &mut register, &mut held, (5, 0), true); // recall slot 0
    handle_key(&mut rt, &mut register, &mut held, (5, 0), false);
    let seq = *rt.shared.ring.lock().unwrap()[0].chord.live.keys().next().unwrap();
    rt.shared.ring.lock().unwrap()[0].chord.live.get_mut(&seq).unwrap().edited = true;

    // The octave-up corner: every edited voice a whole octave higher, register still.
    handle_key(&mut rt, &mut register, &mut held, up_corner, true);
    handle_key(&mut rt, &mut register, &mut held, up_corner, false);
    assert_eq!(register, 0, "the register does not move while a selection is live");
    assert_eq!(held[&note], pitch + edo, "the finger's held entry re-filed an octave up");
    {
      let rings = rt.shared.ring.lock().unwrap();
      assert!(rings[0].store.has(Reason::Edit, pitch + edo), "the selection followed");
      assert!(rings[0].store.has(Reason::Sustain, pitch + edo));
      assert!(!rings[0].store.has(Reason::Sustain, pitch), "and vacated the old pitch");
      assert_eq!(rings[0].chord.live[&seq].pitch, chord_pitch + edo, "the chord voice too");
    }
    {
      let v = rt.shared.voices.lock().unwrap();
      let finger = &v[&VoiceSource::SurfaceFinger { grid: 0, cell: note }];
      assert_eq!(
        finger.freq_target,
        freq_for_pitch(pitch + edo, rt.tuning.fund, edo),
        "the fingered voice glides an octave up, in place",
      );
      assert!(finger.glide_per_sample > 1.0, "gliding, not jumping");
      let chord = &v[&VoiceSource::SurfaceChord { grid: 0, seq }];
      assert_eq!(chord.freq_target, freq_for_pitch(chord_pitch + edo, rt.tuning.fund, edo));
    }

    // Deselect everything (the editmode-clear button at (12,0)); the corners scroll
    // again.
    handle_key(&mut rt, &mut register, &mut held, (12, 0), true);
    handle_key(&mut rt, &mut register, &mut held, (12, 0), false);
    handle_key(&mut rt, &mut register, &mut held, down_corner, true);
    handle_key(&mut rt, &mut register, &mut held, down_corner, false);
    assert_ne!(register, 0, "with no selection, the corner moves the register as ever");
  }

  /// queues/branch-2.org "fine transpose", end to end: toggle on at (0,15), press
  /// transpose keys (live scalar transpose of the whole selection, mono-style with
  /// snap-back, the last release keeping it), move the X with an octave corner,
  /// exit with the transpose still in effect and the grid playing again.
  #[test]
  fn fine_transpose_sets_a_live_scalar_transpose_of_the_selection() {
    use crate::pitch::freq_for_pitch;
    use crate::types::VoiceSource;
    let mut rt = test_grid_thread();
    let mut register = 0;
    let mut held = HashMap::new();
    let edo = rt.tuning.edo;
    let (xs, ys) = (rt.tuning.x_step, rt.tuning.y_step);
    let step = move |x: i32, y: i32| step_for_cell(xs, ys, 0, x, y);
    let toggle = (1, 15); // one cell right of the pedal-slide toggle, which holds the corner
    let note = (5, 5);
    let pitch = step(note.0, note.1);
    let freq_at = |rt: &GridThread, p: i32| freq_for_pitch(p, rt.tuning.fund, edo);

    // An edited drone to transpose: strike, edit via the handle, lift.
    handle_key(&mut rt, &mut register, &mut held, note, true);
    handle_key(&mut rt, &mut register, &mut held, (5, 6), true);
    handle_key(&mut rt, &mut register, &mut held, (5, 6), false);
    handle_key(&mut rt, &mut register, &mut held, note, false);

    // Enter fine transpose: the X seeds at the board center's pitch.
    handle_key(&mut rt, &mut register, &mut held, toggle, true);
    handle_key(&mut rt, &mut register, &mut held, toggle, false);
    assert!(rt.fine.on);
    let center = step(8, 8);
    assert_eq!(rt.fine.center, center, "the X seeds at the board center");

    // Press a key: the selection moves to (pressed - center), live.
    let key_a = (8, 11); // interval +3 from the center
    let int_a = step(key_a.0, key_a.1) - center;
    handle_key(&mut rt, &mut register, &mut held, key_a, true);
    let drone = |p: i32| VoiceSource::SurfaceDrone { grid: 0, pitch: p };
    {
      let v = rt.shared.voices.lock().unwrap();
      let s = &v[&drone(pitch + int_a)];
      assert_eq!(s.freq_target, freq_at(&rt, pitch + int_a), "gliding to +3, live");
      assert_eq!(v.len(), 1, "a transpose key sounds no note of its own");
    }
    assert!(
      rt.shared.ring.lock().unwrap()[0].store.has(Reason::Edit, pitch + int_a),
      "the selection re-filed at the transposed pitch",
    );

    // A second key (first still held) wins; releasing it snaps back; releasing the
    // last key keeps the transpose.
    let key_b = (8, 13); // interval +5
    let int_b = step(key_b.0, key_b.1) - center;
    handle_key(&mut rt, &mut register, &mut held, key_b, true);
    assert!(rt.shared.voices.lock().unwrap().contains_key(&drone(pitch + int_b)), "newest wins");
    handle_key(&mut rt, &mut register, &mut held, key_b, false);
    assert!(
      rt.shared.voices.lock().unwrap().contains_key(&drone(pitch + int_a)),
      "release snaps to the still-held key",
    );
    handle_key(&mut rt, &mut register, &mut held, key_a, false);
    assert_eq!(rt.fine.applied, int_a, "the LAST release keeps the transpose");

    // An octave corner moves the X, not the register and not the voices.
    handle_key(&mut rt, &mut register, &mut held, (13, 14), true); // octave-down corner
    handle_key(&mut rt, &mut register, &mut held, (13, 14), false);
    assert_eq!(rt.fine.center, center - edo, "the X moved an octave down");
    assert_eq!(register, 0, "the register held");
    assert!(rt.shared.voices.lock().unwrap().contains_key(&drone(pitch + int_a)), "voices held");

    // Exit: the transpose remains in effect; the grid plays again.
    handle_key(&mut rt, &mut register, &mut held, toggle, true);
    handle_key(&mut rt, &mut register, &mut held, toggle, false);
    assert!(!rt.fine.on);
    assert!(
      rt.shared.voices.lock().unwrap().contains_key(&drone(pitch + int_a)),
      "a nonzero transpose remains in effect on exit",
    );
    handle_key(&mut rt, &mut register, &mut held, (3, 12), true); // an ordinary note again
    assert!(
      rt.shared.voices.lock().unwrap().contains_key(
        &VoiceSource::SurfaceFinger { grid: 0, cell: (3, 12) },
      ),
      "after exit the grid plays notes again",
    );
  }

  /// Entering fine transpose with NO edit selection first selects everything
  /// sounding on that monome -- the editmode-accrete one-shot, "just as if they
  /// had pressed the kmss select-everything button" (Jeff by chat). A non-empty
  /// selection is left exactly as it is.
  #[test]
  fn entering_fine_transpose_with_nothing_selected_selects_everything_sounding() {
    use crate::surfaces_runtime::chords::{StoredChord, StoredVoice};
    use crate::types::Timbre;
    let mut rt = test_grid_thread();
    let mut register = 0;
    let mut held = HashMap::new();
    let toggle = (1, 15); // one cell right of the pedal-slide toggle, which holds the corner
    let step = {
      let (xs, ys) = (rt.tuning.x_step, rt.tuning.y_step);
      move |x: i32, y: i32| step_for_cell(xs, ys, 0, x, y)
    };

    // Sounding but UNSELECTED: a fingered note, a sustained drone, a chord voice.
    handle_key(&mut rt, &mut register, &mut held, (3, 12), true); // stays fingered
    handle_key(&mut rt, &mut register, &mut held, (5, 5), true);
    handle_key(&mut rt, &mut register, &mut held, (5, 4), true); // sustain handle
    handle_key(&mut rt, &mut register, &mut held, (5, 4), false);
    handle_key(&mut rt, &mut register, &mut held, (5, 5), false); // drones
    let chord_pitch = step(8, 8);
    {
      let mut rings = rt.shared.ring.lock().unwrap();
      rings[0].chord.save(0, StoredChord { voices: vec![StoredVoice {
        pitch: chord_pitch, timbre: Timbre::default(), fader_gain: 1.0, pedal_gain: 1.0,
        osc_phase: 0.0, pulse_factor: 0.0, pulse_phase: 0.0,
      }] });
    }
    handle_key(&mut rt, &mut register, &mut held, (5, 0), true); // recall slot 0
    handle_key(&mut rt, &mut register, &mut held, (5, 0), false);
    assert!(!rt.shared.ring.lock().unwrap()[0].store.any(Reason::Edit), "nothing selected yet");

    // Enter: everything sounding joins the selection.
    handle_key(&mut rt, &mut register, &mut held, toggle, true);
    handle_key(&mut rt, &mut register, &mut held, toggle, false);
    {
      let rings = rt.shared.ring.lock().unwrap();
      assert!(rings[0].store.has(Reason::Edit, step(3, 12)), "the fingered note is selected");
      assert!(rings[0].store.has(Reason::Edit, step(5, 5)), "the drone is selected");
      assert!(rings[0].chord.live.values().all(|v| v.edited), "the chord voice is selected");
    }

    // Exit; deselect only the drone's pitch via its handle, keeping the rest
    // selected -- a NON-empty selection must survive a re-entry untouched.
    handle_key(&mut rt, &mut register, &mut held, toggle, true);
    handle_key(&mut rt, &mut register, &mut held, toggle, false);
    handle_key(&mut rt, &mut register, &mut held, (5, 6), true); // exit-edit handle
    handle_key(&mut rt, &mut register, &mut held, (5, 6), false);
    assert!(!rt.shared.ring.lock().unwrap()[0].store.has(Reason::Edit, step(5, 5)));
    handle_key(&mut rt, &mut register, &mut held, toggle, true);
    handle_key(&mut rt, &mut register, &mut held, toggle, false);
    assert!(
      !rt.shared.ring.lock().unwrap()[0].store.has(Reason::Edit, step(5, 5)),
      "re-entry with a live selection selects nothing new",
    );
  }

  /// Serialises the mock-rig tests: they share the global `STOP` and the mock rig's
  /// listen ports, so they must not run concurrently.
  static MOCK_LOCK: Mutex<()> = Mutex::new(());

  /// A running `run()` under test, torn down on DROP -- so it happens even if the test
  /// panics.
  ///
  /// This matters more than it looks. The mock rig's grid ports are fixed (9102/9103),
  /// and a test that fails an assertion unwinds straight past any hand-written
  /// teardown, leaving `run()` alive with those ports still bound. `MOCK_LOCK` does not
  /// help: it is released by the unwind too, and it was never the thing holding the
  /// sockets. Every later mock test then dies on "Address already in use", so ONE real
  /// failure arrives as a cascade of unrelated ones with the cause buried at the top.
  /// That is exactly how a rig-layout mistake presented during the branch-2 merge.
  ///
  /// Drop order does the rest: declare the lock guard, then the `MockRig`, then this,
  /// and the runtime is stopped before the mock grids it talks to go away.
  struct MockRun(Option<thread::JoinHandle<()>>);

  impl Drop for MockRun {
    fn drop(&mut self) {
      STOP.store(true, Ordering::SeqCst);
      if let Some(handle) = self.0.take() {
        let _ = handle.join();
      }
      // Left false for the next test, which expects to start from a stopped world.
      STOP.store(false, Ordering::SeqCst);
    }
  }

  impl MockRun {
    /// Spawn `run()` against the mock detector and hold it until this guard drops.
    fn start(rig: Rig, detector_port: u16, what: &'static str) -> MockRun {
      STOP.store(false, Ordering::SeqCst);
      MockRun(Some(thread::spawn(move || {
        if let Err(e) = run(&rig, detector_port, true, None) {
          eprintln!("mock {what} run error: {e}");
        }
      })))
    }
  }

  #[test]
  fn selector_writes_the_controlled_grids_timbre_slot() {
    // A grid's selector re-timbres the grid at `controls_index`, leaving every other
    // entry untouched. Wire grid 0's strip to grid 1 (cross-control is still legal
    // rig even though the current rigs self-control): a press on grid 0's cell 3
    // must select slot 3 for grid 1 only.
    let selected = Arc::new(Mutex::new(vec![DEFAULT_SLOT; 2]));
    set_slot(&selected, 1, 3); // grid 0's selector -> grid 1
    assert_eq!(current_slot(&selected, 1), 3, "grid 1 (controlled) got slot 3");
    assert_eq!(current_slot(&selected, 0), DEFAULT_SLOT, "grid 0 unchanged");
  }

  #[test]
  fn trail_dedups_by_class_and_suppresses_neighbours() {
    let edo = 58;
    // The `[trail]` defaults: 1/27 octave (58/27 ~= 2.1, so classes within 2 steps are
    // neighbours) and up to 7 distinct classes.
    let clobber = 27;
    let trails_max = 7;
    let trail = Arc::new(Mutex::new(VecDeque::new()));
    let snap = |t: &Arc<Mutex<VecDeque<i32>>>| -> Vec<i32> { t.lock().unwrap().iter().copied().collect() };

    // Hammering one class (octaves collapse to the same class) never floods or evicts.
    for _ in 0..7 {
      push_trail(&trail, 20, edo, clobber, trails_max);
    }
    assert_eq!(snap(&trail), vec![20], "a repeated class stays a single entry");

    // Far-apart classes accumulate, newest first.
    push_trail(&trail, 30, edo, clobber, trails_max);
    push_trail(&trail, 40, edo, clobber, trails_max);
    assert_eq!(snap(&trail), vec![40, 30, 20], "far-apart classes coexist");

    // Playing a near neighbour (2 steps from 40) erases 40 from the trail.
    push_trail(&trail, 42, edo, clobber, trails_max);
    assert_eq!(snap(&trail), vec![42, 30, 20], "a neighbour within 1/27 octave is suppressed");

    // Wrap-around neighbours count too: classes 1 and 57 are 2 apart in 58-EDO.
    push_trail(&trail, 1, edo, clobber, trails_max);
    push_trail(&trail, 57, edo, clobber, trails_max);
    assert!(!snap(&trail).contains(&1), "1 is suppressed by its wrap-around neighbour 57");

    // Never exceeds `trails_max` distinct classes.
    for c in [4, 8, 12, 16, 24, 34, 44, 54] {
      push_trail(&trail, c, edo, clobber, trails_max);
    }
    assert!(snap(&trail).len() <= trails_max, "capped at {trails_max}");
  }

  #[test]
  fn resolves_two_grids_with_self_control() {
    let rig = load_named_rig("2-monomes_kmss-drums").expect("rig loads");
    let s = resolve_settings(&rig).expect("resolves without hardware");
    assert_eq!(s.grids.len(), 2, "two play grids");
    assert!(s.has_drums, "the KMSS drumkit is present");
    // Each grid's selector controls its OWN grid (per TODO/misc.org, 2026-07; the
    // looper-plus-edo rig keeps its cross-surface timbre editing, which is a
    // different mechanism entirely).
    assert_eq!(s.grids[0].controls_index, 0, "grid 0's strip re-timbres grid 0");
    assert_eq!(s.grids[1].controls_index, 1, "grid 1's strip re-timbres grid 1");
    // Both grids carry a scroll pad, a selector, the accrete trio, and the
    // toggles -- but NO volume strip (dropped per misc.org "drop the amplitude
    // row": [[timbres]] amplitude replaced it).
    for g in &s.grids {
      assert_ne!(g.overlays.scroll_rect, NO_RECT, "grid {:?} has a scroll pad", g.monome_id);
      assert_ne!(g.overlays.selector_rect, NO_RECT, "grid {:?} has a selector", g.monome_id);
      assert_eq!(g.overlays.volume_rect, NO_RECT, "grid {:?} has no volume strip", g.monome_id);
      assert_eq!(g.overlays.clear_rect, [0, 15, 0, 15], "grid {:?} clear button", g.monome_id);
      assert_eq!(g.overlays.needs_holding_rect, [1, 15, 1, 15], "grid {:?} needs-holding", g.monome_id);
      assert_eq!(g.overlays.accrete_rect, [2, 15, 2, 15], "grid {:?} accrete button", g.monome_id);
      assert_eq!(g.overlays.erase_rect, [1, 14, 1, 14], "grid {:?} erase button", g.monome_id);
      assert_eq!(g.overlays.distortion_rect, [0, 1, 0, 1], "grid {:?} distortion toggle", g.monome_id);
      assert_eq!(g.overlays.slide_rect, [1, 1, 1, 1], "grid {:?} slide toggle", g.monome_id);
      assert_eq!(g.overlays.mono_rect, [1, 2, 1, 2], "grid {:?} mono toggle", g.monome_id);
      assert_eq!(g.overlays.poly_rect, [13, 0, 15, 1], "grid {:?} polyrhythm pad", g.monome_id);
    }
    // Deliberately NO assertions on tunables here (the distortion curve, the trail,
    // the slide/tap windows). This test pins the rig's *architecture* -- which windows
    // exist and where -- so that retuning the instrument by ear never reddens it. The
    // rig -> Settings wiring for those knobs is tested with sentinels, just below.
  }

  /// Plumbing, not policy. Every knob a player retunes by ear -- the distortion curve
  /// and its makeup, the trail, the slide and tap windows -- must travel from the rig
  /// into `Settings`, whatever the rig happens to say today. So overwrite them with
  /// sentinels (all distinct, none equal to a default) and check each lands in its own
  /// field. That catches a dropped wire, a swapped pair, and a ms->seconds slip, while
  /// staying immune to edits of the shipped rig.
  #[test]
  fn surfaces_tunables_travel_from_the_rig_into_the_settings() {
    let mut rig = load_named_rig("2-monomes_kmss-drums").expect("rig loads");
    for sink in &mut rig.sinks {
      if let SinkRig::CpalSynth {
        distortion_scale,
        distortion_shape,
        distortion_makeup,
        distortion_auto_makeup,
        distortion_makeup_slew_ms,
        ..
      } = sink
      {
        *distortion_scale = 0.37;
        *distortion_shape = 4.25;
        *distortion_makeup = 0.83;
        *distortion_auto_makeup = false; // inverted from its default, so a dropped wire shows
        *distortion_makeup_slew_ms = 42.0;
      }
    }
    rig.trail = Some(TrailRig { clobber_radius: 13, max: 3 });
    rig.slide = Some(SlideRig { candidate_window_ms: 777, duration_ms: 55, pedal_smoother_ms: 42 });
    rig.tap_tempo = Some(TapTempoRig { window_ms: 1234 });

    let s = resolve_settings(&rig).expect("resolves without hardware");
    assert_eq!(s.distortion, Distortion { scale: 0.37, shape: 4.25 });
    assert_eq!(s.distortion_makeup, 0.83, "the makeup trim");
    assert!(!s.distortion_auto_makeup, "the auto-makeup flag");
    assert!((s.distortion_makeup_slew_secs - 0.042).abs() < 1e-9, "makeup slew, ms -> secs");
    assert_eq!(s.trail_clobber_radius, 13);
    assert_eq!(s.trails_max, 3);
    assert_eq!(s.slide_window, Duration::from_millis(777), "slide candidate window");
    assert_eq!(s.tap_window, Duration::from_millis(1234), "tap-tempo window");
    assert!((s.slide_duration_secs - 0.055).abs() < 1e-6, "slide duration, ms -> secs");
    assert!(
      (s.slide_pedal_smoother_secs - 0.042).abs() < 1e-6,
      "pedal-slide smoother travels rig -> Settings, ms -> secs",
    );
  }

  /// The distortion's makeup table must be usable for whatever curve the rig names --
  /// including the sub-1 shapes that bend from the origin. Properties, not numbers, so
  /// retuning by ear never reddens this either.
  #[test]
  fn the_rigs_makeup_table_is_usable_whatever_curve_it_names() {
    let rig = load_named_rig("2-monomes_kmss-drums").expect("rig loads");
    let s = resolve_settings(&rig).expect("resolves without hardware");
    let makeup = live_makeup(&s);
    // Silence needs no makeup; the makeup never attenuates; it rises with the bus; and
    // driving to the elbow needs real makeup.
    //
    // Note what is deliberately NOT asserted: that a *quiet* bus needs ~no makeup. That
    // is a `k >= 1` intuition. Since `f(y)/y ~ 1 - |y/s|^k / k`, a soft elbow bends at
    // every amplitude -- at k = 0.3 the makeup is still 1.26x at sigma = s/10000.
    assert_eq!(makeup.gain(0.0, s.distortion.scale), 1.0, "silence needs no makeup");
    let at_elbow = makeup.gain(s.distortion.scale, s.distortion.scale);
    assert!(at_elbow > 1.05, "at sigma = scale the clipper is biting: makeup {at_elbow}");
    let mut prev = 1.0;
    for i in 1..=40 {
      let g = makeup.gain(i as f32 * 0.05 * s.distortion.scale, s.distortion.scale);
      assert!(g >= prev - 1e-6, "makeup is monotone in sigma");
      assert!(g >= 1.0, "makeup never attenuates the distorted bus");
      prev = g;
    }
    assert!(prev > at_elbow, "and it keeps rising past the elbow");
  }

  #[test]
  fn surfaces_defaults_when_tables_absent() {
    // Omitting `[trail]` / `[slide]` / `[tap_tempo]` changes nothing: the built-in
    // defaults reach `Settings`. Built by REMOVING the tables from a real surfaces
    // rig, rather than leaning on some other rig happening not to declare them
    // (which a rig edit could undo).
    let mut rig = load_named_rig("2-monomes_kmss-drums").expect("rig loads");
    rig.trail = None;
    rig.slide = None;
    rig.tap_tempo = None;
    let s = resolve_settings(&rig).expect("resolves without hardware");
    let d_trail = TrailRig::default();
    let d_slide = SlideRig::default();
    let d_tap_tempo = TapTempoRig::default();
    assert_eq!((d_trail.clobber_radius, d_trail.max), (27, 7), "the code's own defaults");
    assert_eq!(s.trail_clobber_radius, d_trail.clobber_radius);
    assert_eq!(s.trails_max, d_trail.max);
    assert_eq!(s.slide_window, Duration::from_millis(d_slide.candidate_window_ms));
    assert!((s.slide_pedal_smoother_secs - d_slide.pedal_smoother_ms as f32 / 1000.0).abs() < 1e-9);
    assert_eq!(s.tap_window, Duration::from_millis(d_tap_tempo.window_ms));
  }

  /// A minimal self-controlling grid for the `plan_bringup` tests: `id`'s selector
  /// (present iff `has_selector`) re-timbres `controls_index`; no other overlays.
  fn gs(id: &str, controls_index: usize, has_selector: bool) -> GridSettings {
    GridSettings {
      select: crate::rig::MonomeSelect {
        size: Some([16, 16]),
        type_contains: None,
        id_contains: None,
      },
      monome_id: id.to_string(),
      listen_port: 9000,
      prefix: format!("/{id}"),
      controls_index,
      volume_controls_index: controls_index,
      overlays: Overlays {
        edo_rect: [0, 0, 15, 15],
        scroll_rect: NO_RECT,
        selector_rect: if has_selector { [0, 0, 3, 0] } else { NO_RECT },
        volume_rect: NO_RECT,
        clear_rect: NO_RECT,
        needs_holding_rect: NO_RECT,
        accrete_rect: NO_RECT,
        erase_rect: NO_RECT,
        distortion_rect: NO_RECT,
        slide_rect: NO_RECT,
        mono_rect: NO_RECT,
        pedal_slide_rect: NO_RECT,
        poly_rect: NO_RECT,
        editmode_clear_rect: NO_RECT,
        editmode_accrete_rect: NO_RECT,
        chord_rect: NO_RECT,
        fine_transpose_rect: NO_RECT,
      },
    }
  }

  #[test]
  fn plan_all_present_and_softstep_present_loads_everything_silently() {
    let grids = [gs("a", 0, true), gs("b", 1, true)];
    let plan = plan_bringup(&grids, &[true, true], true, true);
    assert!(plan.report.is_empty(), "nothing skipped -> empty report: {:?}", plan.report);
    assert!(plan.drums, "the SoftStep is present");
    assert!(!plan.drop_selector[0] && !plan.drop_selector[1], "self-control keeps both selectors");
  }

  #[test]
  fn plan_absent_grid_is_reported_but_the_present_one_still_loads() {
    // The common "one grid unplugged" case: grid b is gone; grid a (self-controlling)
    // still loads, and only b is reported.
    let grids = [gs("a", 0, true), gs("b", 1, true)];
    let plan = plan_bringup(&grids, &[true, false], false, false);
    assert!(plan.any_grid(), "grid a is present");
    assert!(!plan.drop_selector[0], "a self-controls a present grid -> keep its selector");
    assert_eq!(plan.report.len(), 1, "exactly one skip (grid b)");
    assert!(plan.report[0].contains("\"b\""), "names the absent grid: {:?}", plan.report[0]);
  }

  #[test]
  fn plan_cross_control_to_an_absent_grid_drops_the_selector() {
    // Grid a is present but its selector re-timbres grid b (absent): a keeps playing,
    // but the selector can't do anything, so it does not load -- and is reported.
    let grids = [gs("a", 1, true), gs("b", 1, true)];
    let plan = plan_bringup(&grids, &[true, false], false, false);
    assert!(plan.drop_selector[0], "a's selector controls absent b -> dropped");
    // Two report lines: b absent, and a's selector dropped.
    assert_eq!(plan.report.len(), 2, "{:?}", plan.report);
    assert!(
      plan.report.iter().any(|l| l.contains("waveform selector") && l.contains("\"b\"")),
      "reports the dead cross-control: {:?}",
      plan.report,
    );
  }

  #[test]
  fn plan_missing_softstep_drops_only_the_drumkit() {
    let grids = [gs("a", 0, true)];
    let plan = plan_bringup(&grids, &[true], true, false);
    assert!(!plan.drums, "no SoftStep -> no drums");
    assert!(plan.any_grid(), "the grid still loads");
    assert!(
      plan.report.iter().any(|l| l.contains("SoftStep") && l.contains("drumkit")),
      "reports the missing drumkit: {:?}",
      plan.report,
    );
  }

  #[test]
  fn plan_drums_stand_alone_when_no_grids_are_present() {
    // The SoftStep special case: no grids, but the SoftStep is present -> the drumkit
    // loads on its own (both grids reported absent). `run`'s no-gear guard passes
    // because drums load.
    let grids = [gs("a", 0, true), gs("b", 1, true)];
    let plan = plan_bringup(&grids, &[false, false], true, true);
    assert!(!plan.any_grid(), "no grids present");
    assert!(plan.drums, "the SoftStep alone still brings up the drumkit");
    assert_eq!(plan.report.len(), 2, "both grids reported absent: {:?}", plan.report);
  }

  /// End-to-end against two virtual grids (the monome mock) with null audio: the whole
  /// device layer -- discovery, both grids binding, LED output, key input routing --
  /// which the pure tests cannot cover. No hardware, no sound. See MOCK-MONOME.org.
  #[test]
  fn two_grids_run_against_mock_grids() {
    use crate::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockRig::start(0, &[GridSpec::grid_256("a"), GridSpec::grid_256("b")])
      .expect("start mock rig");
    let detector_port = mock.detector_port();
    let rig = load_named_rig("2-monomes_kmss-drums-mock").expect("mock rig loads");

    let _run = MockRun::start(rig.clone(), detector_port, "surfaces");

    let a = mock.grid(0);
    let b = mock.grid(1);
    let secs = Duration::from_secs;
    // Both grids register and get a first repaint.
    assert!(
      wait_until(secs(5), || a.registered() && b.registered()),
      "both grids should register against the surfaces runtime",
    );
    assert!(wait_until(secs(3), || a.generation() > 0 && b.generation() > 0), "first repaint");

    // Each grid's selector strip lights the DEFAULT (triangle) cell bright: cell (1,0).
    assert!(wait_until(secs(3), || a.level_at(1, 0) == 15), "grid a selector: triangle bright");
    assert!(wait_until(secs(3), || b.level_at(1, 0) == 15), "grid b selector: triangle bright");

    // Finger a note on grid a (open cell, away from overlays): it lights, dark on release.
    a.press(5, 5);
    assert!(wait_until(secs(3), || a.level_at(5, 5) == 15), "fingered note lights solid on grid a");
    // Cross-grid reflection (feature 3): grid a's note lights its octave-equivalents on
    // grid b too (both registers 0, so the same cell). Audio voices stay independent --
    // that is tested in synth.rs; here we check the shared LED reflection.
    assert!(wait_until(secs(3), || b.level_at(5, 5) == 15), "grid a's note reflects onto grid b");
    a.release(5, 5);
    // Trail (feature 4): a released note lingers *dim* (level 4) in the shared trail on
    // both grids, rather than going fully dark.
    assert!(wait_until(secs(3), || a.level_at(5, 5) == 4), "released note lingers dim on grid a");
    assert!(wait_until(secs(3), || b.level_at(5, 5) == 4), "released note lingers dim on grid b");

    // Grid a's selector sets grid a's OWN waveform to SAW (cell (3,0)) -> its strip
    // repaints to show saw selected; grid b's strip (b's own waveform) is untouched.
    a.press(3, 0);
    a.release(3, 0);
    assert!(wait_until(secs(3), || a.level_at(3, 0) == 15 && a.level_at(1, 0) == 4),
      "grid a strip now shows saw selected (triangle dims)");
    assert!(wait_until(secs(3), || b.level_at(1, 0) == 15), "grid b's strip still shows its own triangle");

    // The old volume strip is gone (misc.org "drop the amplitude row"): its cells
    // are ordinary play cells now -- pressing (10,0) sounds a note (lights bright)
    // and releases into the dim trail rather than moving any fader.
    a.press(10, 0);
    assert!(wait_until(secs(3), || a.level_at(10, 0) == 15), "(10,0) is a play cell now");
    a.release(10, 0);
    assert!(wait_until(secs(3), || a.level_at(10, 0) == 4), "released into the trail");

  }

  /// Robust-to-missing-gear (TODO.org): a two-grid rig with only ONE grid connected
  /// still brings up the present grid -- it discovers a single device, binds it as
  /// grid 0, and skips the absent grid 1 (named in the red report) instead of erroring
  /// out the whole run.
  #[test]
  fn one_grid_absent_still_runs_the_present_grid() {
    use crate::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Only grid "a" exists; the 2-monome mock rig rig wants two.
    let mock = MockRig::start(0, &[GridSpec::grid_256("a")]).expect("start one mock grid");
    let detector_port = mock.detector_port();
    let rig = load_named_rig("2-monomes_kmss-drums-mock").expect("mock rig loads");

    let _run = MockRun::start(rig.clone(), detector_port, "one-grid");

    let a = mock.grid(0);
    let secs = Duration::from_secs;
    // The present grid registers and repaints even though its sibling is absent.
    assert!(wait_until(secs(5), || a.registered()), "the present grid registers");
    assert!(wait_until(secs(3), || a.generation() > 0), "first repaint");
    // Its own selector still works (self-control, its target present): triangle lit.
    assert!(wait_until(secs(3), || a.level_at(1, 0) == 15), "grid a selector shows triangle");
    // And it plays: a fingered note lights, then lingers dim in the trail on release.
    a.press(5, 5);
    assert!(wait_until(secs(3), || a.level_at(5, 5) == 15), "fingered note lights on the present grid");
    a.release(5, 5);
    assert!(wait_until(secs(3), || a.level_at(5, 5) == 4), "released note lingers dim");

  }

  /// The shipped two-softstep rig must load and resolve. This is the rig's only
  /// automated check -- everything else about it (which pedal does what, whether the
  /// grids land left/right) is hardware, and hardware is Jeff's to confirm.
  #[test]
  fn the_two_softstep_rig_loads_and_pins_its_gear() {
    use crate::rig::{AccreteControlKind, PulseFactorRig, SoftstepWindowRig};
    let source = std::fs::read_to_string(
      crate::rig::rig_dir().join("2-edogrids_ss-accrete_ss-pulse.org"),
    )
    .expect("read the shipped rig");
    let rig = crate::rig_org::parse_org_rig(&source).expect("the shipped rig parses");

    // Grids pinned by SERIAL, not by enumeration order: every pedal below targets a
    // monome by name, so a replug that swapped them would invert the whole board.
    let pins: Vec<Option<&str>> =
      rig.monomes.iter().map(|m| m.select.id_contains.as_deref()).collect();
    assert_eq!(
      pins,
      [Some("m256-282"), Some("m0000102")],
      "a = the monobright/left grid, b = the varibright/right one",
    );

    // The two boards must select disjointly, or one binds twice and the other never.
    let subs: Vec<&str> = rig.softsteps.iter().map(|s| s.select.name_substring()).collect();
    assert_eq!(subs, ["SSCOM", "SoftStep"]);
    assert!(!"SSCOM MIDI 1".contains("SoftStep"), "the selectors must not overlap");
    assert!(!"SoftStep Control Surface".contains("SSCOM"));

    // Sustain: clear + accrete per grid, and momentary only -- no needs_holding or
    // erase is bound (the library still supports both; this rig just doesn't use them).
    let accretes: Vec<(&str, u8, &str)> = rig
      .softstep_windows
      .iter()
      .filter_map(|w| match w {
        SoftstepWindowRig::AccreteControl { pedal, monome, control, .. } => {
          Some((monome.as_str(), *pedal, match control {
            AccreteControlKind::Clear => "clear",
            AccreteControlKind::Accrete => "accrete",
            AccreteControlKind::NeedsHolding => "needs_holding",
            AccreteControlKind::Erase => "erase",
          }))
        }
        _ => None,
      })
      .collect();
    assert_eq!(
      accretes,
      [("a", 1, "clear"), ("a", 2, "accrete"), ("b", 4, "accrete"), ("b", 5, "clear")],
      "left buttons drive the left grid, right buttons the right",
    );

    // No tap pedal: Jeff retired it (he never set a tempo with it); the runtime's seeded
    // 1 Hz base gives the factor controls something to multiply regardless.
    let taps: Vec<u8> = rig
      .softstep_windows
      .iter()
      .filter_map(|w| match w {
        SoftstepWindowRig::TapTempoPedal { pedal, .. } => Some(*pedal),
        _ => None,
      })
      .collect();
    assert!(taps.is_empty(), "the tap pedal was retired: {taps:?}");

    // Each grid also carries the on-grid 3x2 factored-pulse pad, upper right (the
    // six-button x3/x2/tap over /3//2/=1 block, brought back from the kmss rig).
    for monome in ["a", "b"] {
      let pad = rig.monome_windows.iter().find_map(|w| match w {
        MonomeWindowRig::FactoredPulsePad { monome: m, rect, .. } if m == monome => Some(*rect),
        _ => None,
      });
      assert_eq!(pad, Some([13, 0, 15, 1]), "monome {monome} has the upper-right pad");
    }

    // Each grid gets a full set of five factored-pulse controls, all on the new board.
    for monome in ["a", "b"] {
      let mut factors: Vec<PulseFactorRig> = rig
        .softstep_windows
        .iter()
        .filter_map(|w| match w {
          SoftstepWindowRig::PulseFactorPedal { monome: m, factor, softstep, .. }
            if m == monome =>
          {
            assert_eq!(softstep, "new", "the factored pulse lives on the new board");
            Some(*factor)
          }
          _ => None,
        })
        .collect();
      factors.sort_by_key(|f| format!("{f:?}"));
      let mut want = vec![
        PulseFactorRig::Double,
        PulseFactorRig::Triple,
        PulseFactorRig::Half,
        PulseFactorRig::Third,
        PulseFactorRig::Unity,
      ];
      want.sort_by_key(|f| format!("{f:?}"));
      assert_eq!(factors, want, "monome {monome} needs all five factored-pulse controls");
    }

    // The grids carry ONLY these overlays (the factored-pulse pad and the
    // editmode-clear button came back / arrived by request after 2_discussion 2f
    // pared the grids down; the chord block is TODO/chord-storage-v2); everything
    // else is a note.
    let kinds: Vec<&str> = rig.monome_windows.iter().map(|w| w.kind_name()).collect();
    assert_eq!(
      kinds,
      [
        "edo_note_grid",
        "waveform_selector",
        "edo_shift_pad",
        "factored_pulse_pad",
        "editmode_control",
        "chord_block",
        "pedal_slide_toggle",
        "fine_transpose_toggle",
        "edo_note_grid",
        "waveform_selector",
        "edo_shift_pad",
        "factored_pulse_pad",
        "editmode_control",
        "chord_block",
        "pedal_slide_toggle",
        "fine_transpose_toggle"
      ],
      "no distortion/slide/mono/accrete windows on the grids (see 2_discussion 2f); \
       each grid additionally carries its own pedal_slide_toggle (TODO/pedal-slide) \
       and fine_transpose_toggle (queues/branch-2.org)",
    );

    // The editmode controls mirror the sustain row one row up (queue.org): OSS
    // outer pedals 6/0 clear, inner pedals 7/9 accrete, left pedals to the left
    // grid -- exactly the bottom row's 1/2 + 4/5 shape -- plus each grid's own
    // clear button at (12,0), beside the pad's x3.
    use crate::rig::EditmodeControlKind as Em;
    let mut editmodes: Vec<(u8, &str, Em)> = rig
      .softstep_windows
      .iter()
      .filter_map(|w| match w {
        SoftstepWindowRig::EditmodeControl { pedal, monome, softstep, control, .. } => {
          assert_eq!(softstep, "old", "editmode controls live on the old board");
          Some((*pedal, monome.as_str(), *control))
        }
        _ => None,
      })
      .collect();
    editmodes.sort();
    assert_eq!(
      editmodes,
      [(0, "b", Em::Clear), (6, "a", Em::Clear), (7, "a", Em::Accrete), (9, "b", Em::Accrete)],
    );
    for monome in ["a", "b"] {
      let rect = rig.monome_windows.iter().find_map(|w| match w {
        MonomeWindowRig::EditmodeControl { monome: m, rect, control, .. }
          if m == monome && *control == Em::Clear =>
        {
          Some(*rect)
        }
        _ => None,
      });
      assert_eq!(rect, Some([12, 0, 12, 0]), "monome {monome}'s editmode-clear button");
    }

    // The EX-P volume pedals: MPC-20 channel 1 -> grid a, channel 2 -> grid b,
    // resolving to pedal indices 0/1 on grid indices 0/1, carrying the default
    // taper (10% linear splice, 50 dB exponential remainder -- spelled out in the
    // rig so the parameter names are findable).
    let pedals: Vec<(u8, &str)> =
      rig.expression_pedals.iter().map(|p| (p.channel, p.monome.as_str())).collect();
    assert_eq!(pedals, [(1, "a"), (2, "b")]);
    let s = resolve_settings(&rig).expect("resolves");
    let curve = PedalVolumeCurve { lin_frac: 0.1, exp_db: 50.0 };
    assert_eq!(s.expression_pedals, [(0, 0, curve), (1, 1, curve)]);

    // The taper: exponential (dB-linear in travel) spliced to a linear fade over
    // the first lin_frac, so the ends are pinned at exact 0 and 1.
    assert_eq!(curve.gain(0.0), 0.0);
    assert_eq!(curve.gain(1.0), 1.0);
    let floor = 10f32.powf(-50.0 / 20.0); // where the splice meets the exponential
    assert!((curve.gain(0.1) - floor).abs() < 1e-6, "continuous at the splice");
    assert!((curve.gain(0.05) - floor / 2.0).abs() < 1e-6, "linear below it");
    // dB-linear above it: the middle of the exponential span sits at -25 dB.
    let mid = curve.gain(0.55);
    assert!((20.0 * mid.log10() - -25.0).abs() < 0.1, "dB-linear: {mid}");
    // Monotonic across the whole travel.
    let g: Vec<f32> = (0..=100).map(|i| curve.gain(i as f32 / 100.0)).collect();
    assert!(g.windows(2).all(|w| w[1] >= w[0]));

    // And it must resolve, not merely parse.
    resolve_settings(&rig).expect("the shipped rig resolves to Settings");
  }

  /// The pedal map the shipped rig actually produces. This is the layer where a typo
  /// is invisible: the rig says `pedal = 5, monome = "a", factor = "x2"`, and only
  /// this map decides that a press of 5 on the NEW board doubles the LEFT grid.
  #[test]
  fn the_two_softstep_rigs_pedals_resolve_to_the_right_actions() {
    let source = std::fs::read_to_string(
      crate::rig::rig_dir().join("2-edogrids_ss-accrete_ss-pulse.org"),
    )
    .expect("read the shipped rig");
    let rig = crate::rig_org::parse_org_rig(&source).expect("parses");
    // Grid 0 = "a" = LOM (left/old), grid 1 = "b" = RNM (right/new), as resolve does.
    let actions = rig_pedal_actions(&rig, |m| match m {
      "a" => Some(0),
      "b" => Some(1),
      _ => None,
    });
    let at = |dev: &str, pedal: u8| actions.get(&(dev.to_string(), pedal)).copied();

    // OLD board: sustain, left buttons -> left grid.
    assert_eq!(
      at("old", 1),
      Some(PedalAction::Accrete { grid: 0, control: AccreteControlKind::Clear }),
    );
    assert_eq!(
      at("old", 2),
      Some(PedalAction::Accrete { grid: 0, control: AccreteControlKind::Accrete }),
    );
    assert_eq!(
      at("old", 4),
      Some(PedalAction::Accrete { grid: 1, control: AccreteControlKind::Accrete }),
    );
    assert_eq!(
      at("old", 5),
      Some(PedalAction::Accrete { grid: 1, control: AccreteControlKind::Clear }),
    );
    // Editmode controls, mirroring the sustain row: outer pedals (6/0) clear,
    // inner (7/9) accrete, left pedals to the left grid.
    {
      use crate::rig::EditmodeControlKind as Em;
      assert_eq!(at("old", 6), Some(PedalAction::Editmode { grid: 0, control: Em::Clear }));
      assert_eq!(at("old", 7), Some(PedalAction::Editmode { grid: 0, control: Em::Accrete }));
      assert_eq!(at("old", 9), Some(PedalAction::Editmode { grid: 1, control: Em::Accrete }));
      assert_eq!(at("old", 0), Some(PedalAction::Editmode { grid: 1, control: Em::Clear }));
    }
    // Pedal 8 held the retired tap tempo; it and pedal 3 are free.
    for free in [3, 8] {
      assert_eq!(at("old", free), None, "old pedal {free} is deliberately unbound");
    }

    // NEW board (rotated 180): far row reads 5 4 3 2 1, near row 0 9 8 7 6.
    // Left columns -> left grid, right columns -> right grid.
    assert_eq!(at("new", 5), Some(PedalAction::FactoredPulse { grid: 0, factor: TempoFactorButton::Times2 }));
    assert_eq!(at("new", 4), Some(PedalAction::FactoredPulse { grid: 0, factor: TempoFactorButton::Times3 }));
    assert_eq!(at("new", 0), Some(PedalAction::FactoredPulse { grid: 0, factor: TempoFactorButton::Div2 }));
    assert_eq!(at("new", 9), Some(PedalAction::FactoredPulse { grid: 0, factor: TempoFactorButton::Div3 }));
    assert_eq!(at("new", 1), Some(PedalAction::FactoredPulse { grid: 1, factor: TempoFactorButton::Times2 }));
    assert_eq!(at("new", 2), Some(PedalAction::FactoredPulse { grid: 1, factor: TempoFactorButton::Times3 }));
    assert_eq!(at("new", 6), Some(PedalAction::FactoredPulse { grid: 1, factor: TempoFactorButton::Div2 }));
    assert_eq!(at("new", 7), Some(PedalAction::FactoredPulse { grid: 1, factor: TempoFactorButton::Div3 }));

    // The middle column splits by DISTANCE, not side: nearer (8) = left grid,
    // farther (3) = right grid. Jeff's rule, and the easiest thing to get backwards.
    assert_eq!(at("new", 8), Some(PedalAction::FactoredPulse { grid: 0, factor: TempoFactorButton::Unity }));
    assert_eq!(at("new", 3), Some(PedalAction::FactoredPulse { grid: 1, factor: TempoFactorButton::Unity }));

    // The same label means different things per board -- the reason the hook needs
    // the device id at all.
    assert_ne!(at("old", 1), at("new", 1));
    assert_ne!(at("old", 5), at("new", 5));
    assert_ne!(at("old", 8), at("new", 8));
  }

  /// A pedal naming a monome with no play grid (unplugged, say) is dropped rather
  /// than binding to the wrong grid or panicking -- the missing-gear path.
  #[test]
  fn a_pedal_whose_grid_is_absent_is_dropped() {
    let source = std::fs::read_to_string(
      crate::rig::rig_dir().join("2-edogrids_ss-accrete_ss-pulse.org"),
    )
    .expect("read");
    let rig = crate::rig_org::parse_org_rig(&source).expect("parses");
    // Only grid "a" is present.
    let actions = rig_pedal_actions(&rig, |m| if m == "a" { Some(0) } else { None });
    assert_eq!(
      actions.get(&("old".to_string(), 1)),
      Some(&PedalAction::Accrete { grid: 0, control: AccreteControlKind::Clear }),
      "a's pedals still bind",
    );
    assert!(actions.get(&("old".to_string(), 5)).is_none(), "b's clear is dropped");
    assert!(actions.get(&("new".to_string(), 1)).is_none(), "b's x2 is dropped");
    assert!(actions.get(&("old".to_string(), 8)).is_none(), "pedal 8 is free (tap retired)");
  }

  /// The bug Jeff hit on the hardware: the shipped rig binds accrete and clear but no
  /// `needs_holding` switch, so its banks came up in the toggle DEFAULT -- one tap and
  /// every later note sustained with his foot off the pedal. The rig's readme promises
  /// momentary; this asserts the rig actually delivers it.
  /// Jeff: "if I put it in edit mode, press another key to move it to a new pitch, and
  /// then take it out of edit mode while continuing to hold the new pitch, it stops.
  /// It should continue."
  ///
  /// The finger map said what each cell NOMINALLY means, not what its voice is
  /// actually sounding, so a drag left it stale. Everything downstream then asked
  /// about the wrong pitch: releasing looked up the pitch the note used to be, found
  /// it neither edited nor sustained, and cut a note that should have kept ringing.
  ///
  /// This pins the map itself, which is the thing that was wrong -- the release and
  /// exit decisions are one-line lookups into it.
  #[test]
  fn dragging_an_edited_note_re_files_the_finger_under_its_new_pitch() {
    let mut held: HashMap<(i32, i32), i32> = HashMap::new();
    held.insert((3, 3), 20); // a finger on cell (3,3), sounding pitch 20

    // The drag glides that finger's voice from 20 to 35 and must re-file it.
    let from = 20;
    let to = 35;
    let cell = held.iter().find(|(_, p)| **p == from).map(|(c, _)| *c).expect("fingered");
    held.insert(cell, to);

    assert_eq!(held.get(&(3, 3)), Some(&35), "the finger now sounds the new pitch");
    assert!(
      held.values().any(|p| *p == 35),
      "so exiting edit mode sees it as fingered, and leaves it ringing",
    );
    assert!(
      !held.values().any(|p| *p == 20),
      "and nothing still thinks the old pitch is under a finger",
    );
  }

  /// Jeff's repro, at the level it actually broke: sustain two notes, edit one, then
  /// press CLEAR. The clear silences both drones -- and used to leave the edited pitch
  /// in edit mode with no voice, dancing forever and forcing every press to drag.
  ///
  /// The unit test covers the state machine; this covers the wiring, which is where
  /// the bug was: `clear` reaching `EditState` at all.
  /// Jeff on the live rig: "if I press a monome key and then press sustain-accrete for
  /// that monome, those voices continue to sound after lifting my fingers." This walks
  /// exactly that through the pedal-hook path (drive_accrete -> capture -> release),
  /// so if it goes green the fault is in bringing the SoftStep up, not in the logic.
  #[test]
  fn accrete_pedal_captures_a_held_note_so_it_survives_the_lift() {
    use crate::types::{Timbre, VoiceSource};
    let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));
    // The two-softstep rig binds no needs_holding, so its banks are momentary.
    let ring = Arc::new(Mutex::new(vec![GridRing::new(AccreteState::new_momentary())]));
    let held_all = Arc::new(Mutex::new(vec![HashMap::new(); 1]));

    // A finger goes down: a voice sounds, and held_all carries it (what the grid
    // thread publishes on note-on, and what the pedal hook reads).
    let mut sink = SurfaceSink::new(0, Arc::clone(&voices), 80.0, 46, 48000.0, 0.003, 0.05, 1.0, 0.5, Arc::new(Mutex::new(vec![1.0; 2])), Arc::new(Mutex::new(vec![1.0; 2])));
    sink.note_on((3, 3), 20, Timbre::default(), None);
    held_all.lock().unwrap()[0].insert((3, 3), 20);

    // The accrete pedal goes down.
    assert!(drive_accrete(
      0, AccreteControlKind::Accrete, true, &ring, &held_all, &voices, 0.05, 48000.0,
    ));
    assert!(
      ring.lock().unwrap()[0].store.iter(Reason::Sustain).any(|p| p == 20),
      "pressing accrete must capture the already-held note",
    );

    // The finger lifts. release_cell's decision, reproduced: a captured note keeps
    // ringing.
    let keep = {
      let mut r = ring.lock().unwrap();
      let gr = &mut r[0];
      gr.accrete.note_released_sustains(20, &mut gr.store)
    };
    assert!(keep, "the lifted note is sustained, so it survives");
    if keep {
      sink.sustain_note((3, 3), 20);
    }
    // The voice is now a drone, still sounding.
    let v = voices.lock().unwrap();
    let drone_key = VoiceSource::SurfaceDrone { grid: 0, pitch: 20 };
    assert!(v.contains_key(&drone_key), "the voice moved to the sustain register");
    assert!(v[&drone_key].target_env > 0.0, "and it is still sounding after the lift");
  }

  #[test]
  fn sustain_clear_ends_and_deselects_edited_drones() {
    // SUPERSEDES `sustain_clear_spares_edited_drones_and_editmode_clear_then_ends_them`.
    // The old symmetric model spared an edited drone from the sustain clear and needed
    // BOTH clears to kill it. The branch-3 model (queue item 4) makes Sustain the only
    // life-support reason and edited ⊆ sustained: the sustain clear ends EVERY fingerless
    // drone -- edited or not -- and cascades the edit removal, so nothing silent stays
    // selected. It is the whole kill on its own now.
    use crate::types::{Timbre, VoiceSource};
    let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));
    let ring = Arc::new(Mutex::new(vec![
      GridRing::new(AccreteState::new_momentary()),
      GridRing::new(AccreteState::new_momentary()),
    ]));
    let held_all = Arc::new(Mutex::new(vec![HashMap::new(); 2]));

    // Two notes, both sustained on grid 0, one of them also in edit mode.
    let mut a = SurfaceSink::new(0, Arc::clone(&voices), 80.0, 46, 48000.0, 0.003, 0.05, 1.0, 0.5, Arc::new(Mutex::new(vec![1.0; 2])), Arc::new(Mutex::new(vec![1.0; 2])));
    for (cell, pitch) in [((1, 1), 10), ((2, 2), 20)] {
      a.note_on(cell, pitch, Timbre::default(), None);
      a.sustain_note(cell, pitch);
      ring.lock().unwrap()[0].store.add(Reason::Sustain, pitch);
    }
    {
      let mut r = ring.lock().unwrap();
      let gr = &mut r[0];
      gr.edit.enter(20, &mut gr.store); // also (re)asserts Sustain(20); the invariant
    }

    // Sustain-clear: the bank flushes and BOTH drones end -- the edited one is not spared.
    assert!(drive_accrete(
      0, AccreteControlKind::Clear, true, &ring, &held_all, &voices, 0.05, 48000.0,
    ));
    let drone = |pitch| VoiceSource::SurfaceDrone { grid: 0, pitch };
    {
      let v = voices.lock().unwrap();
      assert_eq!(v[&drone(10)].target_env, 0.0, "the plain drone releases");
      assert_eq!(v[&drone(20)].target_env, 0.0, "the edited drone releases too -- no longer spared");
    }
    let r = ring.lock().unwrap();
    assert!(r[0].store.iter(Reason::Sustain).next().is_none(), "the sustain set flushed");
    assert!(!r[0].store.any(Reason::Edit), "and the edit selection cascaded away: no silent ghost");
  }

  #[test]
  fn editmode_clear_ends_nothing_and_only_deselects() {
    // SUPERSEDES `editmode_clear_spares_sustained_drones`. Editmode-clear is now pure
    // deselection (branch-3 queue item 4): every edited note is still sustained (edited ⊆
    // sustained), so the clear silences NO voice -- both drones keep ringing -- and only
    // the edit set empties. The old test asserted an "edit-only drone" ended here; that
    // state no longer exists, because entering edit mode sustains the note.
    use crate::types::{Timbre, VoiceSource};
    let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));
    let ring = Arc::new(Mutex::new(vec![GridRing::new(AccreteState::new_momentary())]));
    let mut a = SurfaceSink::new(0, Arc::clone(&voices), 80.0, 46, 48000.0, 0.003, 0.05, 1.0, 0.5, Arc::new(Mutex::new(vec![1.0; 1])), Arc::new(Mutex::new(vec![1.0; 1])));
    // Both edited (so both sustained, via `enter`). They are fingerless drones.
    for (cell, pitch) in [((1, 1), 10), ((2, 2), 20)] {
      a.note_on(cell, pitch, Timbre::default(), None);
      a.sustain_note(cell, pitch);
      let mut r = ring.lock().unwrap();
      let gr = &mut r[0];
      gr.edit.enter(pitch, &mut gr.store);
    }

    editmode_clear(0, &ring);
    let drone = |pitch| VoiceSource::SurfaceDrone { grid: 0, pitch };
    {
      let v = voices.lock().unwrap();
      assert!(v[&drone(10)].target_env > 0.0, "still sustained -> keeps ringing");
      assert!(v[&drone(20)].target_env > 0.0, "still sustained -> keeps ringing");
    }
    let r = ring.lock().unwrap();
    assert!(!r[0].store.any(Reason::Edit), "the edit selection empties");
    let mut sustained: Vec<i32> = r[0].store.iter(Reason::Sustain).collect();
    sustained.sort();
    assert_eq!(sustained, [10, 20], "the sustain bank is untouched -- both notes still droning");
  }

  #[test]
  fn editmode_accrete_captures_and_sustains_fingered_and_sustained_voices() {
    // Editmode-accrete puts every sounding voice into edit mode; because entering edit
    // mode implies sustaining (edited ⊆ sustained, branch-3 queue item 4), a fingered-only
    // voice becomes sustained too.
    let ring = Arc::new(Mutex::new(vec![GridRing::new(AccreteState::new_momentary())]));
    let held_all = Arc::new(Mutex::new(vec![HashMap::from([((1, 1), 10)])]));
    ring.lock().unwrap()[0].store.add(Reason::Sustain, 20);

    editmode_accrete(0, &ring, &held_all);
    let r = ring.lock().unwrap();
    assert!(r[0].store.has(Reason::Edit, 10), "the fingered voice enters edit mode");
    assert!(r[0].store.has(Reason::Edit, 20), "the sustained voice too");
    assert_eq!(r[0].store.iter(Reason::Edit).count(), 2, "and nothing else");
    assert!(r[0].store.has(Reason::Sustain, 10), "the fingered-only voice is now sustained (edit implies sustain)");
  }

  #[test]
  fn sustain_accrete_activation_captures_edited_voices_too() {
    // queue.org: "accrete-sustain should add every fingered voice and every
    // edit-mode voice". Pitch 10 is fingered, pitch 20 rings only via edit mode;
    // stomping accrete captures both into the sustain bank.
    let ring = Arc::new(Mutex::new(vec![GridRing::new(AccreteState::new_momentary())]));
    let held_all = Arc::new(Mutex::new(vec![HashMap::from([((1, 1), 10)])]));
    {
      let mut r = ring.lock().unwrap();
      let gr = &mut r[0];
      gr.edit.enter(20, &mut gr.store);
    }

    assert!(ring.lock().unwrap()[0].accrete.press_accrete(), "activation edge");
    capture_grid_held_into(&held_all, &ring, 0);
    let r = ring.lock().unwrap();
    let captured: HashSet<i32> = r[0].store.iter(Reason::Sustain).collect();
    assert_eq!(captured, [10, 20].into(), "fingered AND edited voices join the bank");
  }

  /// Jeff: "a global clear will now clear even the ones that started sustaining
  /// without pedals." Per-note sustains and pedal accretes share one set, so clear
  /// reaches both -- this pins that rather than trusting it, since the last bug here
  /// was precisely clear failing to reach something that was ringing.
  #[test]
  fn clear_flushes_per_note_sustains_as_well_as_pedal_accretes() {
    let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));
    let ring = Arc::new(Mutex::new(vec![GridRing::new(AccreteState::new_momentary())]));
    let held_all = Arc::new(Mutex::new(vec![HashMap::new(); 1]));

    // One note sustained by the PEDAL (accrete live while it was played)...
    {
      let mut r = ring.lock().unwrap();
      let gr = &mut r[0];
      gr.accrete.press_accrete();
      gr.accrete.note_played(10, &mut gr.store);
      gr.accrete.release_accrete();
    }
    // ...and one sustained by the per-note button, with no pedal involved at all.
    ring.lock().unwrap()[0].store.add(Reason::Sustain, 20);
    assert_eq!(ring.lock().unwrap()[0].store.iter(Reason::Sustain).count(), 2);

    drive_accrete(
      0, AccreteControlKind::Clear, true, &ring, &held_all, &voices, 0.05, 48000.0,
    );
    assert!(
      ring.lock().unwrap()[0].store.iter(Reason::Sustain).next().is_none(),
      "clear must flush the button-sustained note too, not just the pedalled one",
    );
  }

  /// Editmode-clear is per-grid, so it must not dismiss the OTHER grid's edit mode.
  #[test]
  fn editmode_clear_leaves_the_other_grids_edit_mode_alone() {
    let ring = Arc::new(Mutex::new(vec![
      GridRing::new(AccreteState::new_momentary()),
      GridRing::new(AccreteState::new_momentary()),
    ]));
    {
      let mut r = ring.lock().unwrap();
      let (a, b) = r.split_at_mut(1);
      a[0].edit.enter(10, &mut a[0].store);
      b[0].edit.enter(20, &mut b[0].store);
    }

    editmode_clear(0, &ring);
    assert!(!ring.lock().unwrap()[0].store.any(Reason::Edit), "grid 0's edit mode cleared");
    assert!(ring.lock().unwrap()[1].store.any(Reason::Edit), "grid 1's edit mode is its own business");
  }

  #[test]
  fn the_two_softstep_rigs_accrete_is_momentary_not_toggle() {
    use crate::rig::{AccreteControlKind, SoftstepWindowRig};
    let source = std::fs::read_to_string(
      crate::rig::rig_dir().join("2-edogrids_ss-accrete_ss-pulse.org"),
    )
    .expect("read the shipped rig");
    let rig = crate::rig_org::parse_org_rig(&source).expect("parses");
    let s = resolve_settings(&rig).expect("resolves");

    // Premise: it really does bind no needs_holding control anywhere.
    assert!(
      !rig.softstep_windows.iter().any(|w| matches!(
        w,
        SoftstepWindowRig::AccreteControl { control: AccreteControlKind::NeedsHolding, .. }
      )),
      "this rig deliberately binds no needs_holding pedal",
    );
    for grid in &s.grids {
      assert_eq!(grid.overlays.needs_holding_rect, NO_RECT, "nor an on-grid one");
      assert!(
        !grid_has_needs_holding_control(&rig, grid),
        "so grid {:?} can never leave whatever mode it starts in",
        grid.monome_id,
      );
    }

    // Therefore every bank must come up momentary: hold to accrete, lift to stop.
    for grid in &s.grids {
      let mut bank = if grid_has_needs_holding_control(&rig, grid) {
        AccreteState::new()
      } else {
        AccreteState::new_momentary()
      };
      bank.press_accrete();
      bank.release_accrete();
      assert!(
        !bank.accreting(),
        "grid {:?}: a tap must not latch accrete on",
        grid.monome_id,
      );
    }
  }

  /// ...and the drums rig, which DOES bind the switch, keeps its toggle behavior.
  #[test]
  fn the_drums_rigs_accrete_still_toggles() {
    let source = std::fs::read_to_string(
      crate::rig::rig_dir().join("2-monomes_kmss-drums.org"),
    )
    .expect("read the drums rig");
    let rig = crate::rig_org::parse_org_rig(&source).expect("parses");
    let s = resolve_settings(&rig).expect("resolves");
    for grid in &s.grids {
      assert!(
        grid_has_needs_holding_control(&rig, grid),
        "the drums rig has an on-grid needs_holding button, so it stays switchable",
      );
    }
  }

  #[test]
  fn adopt_rig_swaps_the_live_parameters_and_bumps_the_generation() {
    // Start from the mock rig, then "edit" it (amplitude, tuning, a timbre, the
    // slide knobs) and reload: the live params reflect every change and the
    // generation moves, so grid threads and the audio callback pick them up.
    let base = load_named_rig("2-monomes_kmss-drums-mock").expect("rig loads");
    let s = resolve_settings(&base).expect("resolves");
    let live = Live::new(&s);

    let source = std::fs::read_to_string(
      crate::rig::mock_rig_dir().join("2-monomes_kmss-drums-mock.org"),
    )
    .expect("read mock org");
    // The rig is `.org` now: PARAM values still contain the `key = value` text these
    // replaces target, but an INJECTED field must be its own PARAM headline at the
    // timbre's depth (slot 2 = square, so the fields land in timbres[2]).
    //
    // Each replacement must actually apply. A bare `str::replace` no-ops silently when
    // the mock rig's value drifts, leaving the test asserting a value that nothing set
    // -- it would then fail far from its cause. Fail here instead, naming the culprit.
    fn must_replace(s: &str, from: &str, to: &str) -> String {
      assert!(s.contains(from), "reload fixture is stale: {from:?} is no longer in the mock rig");
      s.replace(from, to)
    }
    let edited = must_replace(&source, "amplitude = 0.15", "amplitude = 0.25");
    let edited = must_replace(&edited, "edo = 46", "edo = 41");
    let edited = must_replace(&edited, "x_step = 9", "x_step = 7");
    let edited = must_replace(
      &edited,
      WAVE_SQUARE,
      "waveform = \"square\"\n*** PARAM abs_fm_depth_cents = 25.0\n*** PARAM rel_fm_depth = 1.5",
    );
    let edited = must_replace(&edited, "duration_ms = 100", "duration_ms = 250");
    let edited = must_replace(&edited, "pedal_smoother_ms = 30", "pedal_smoother_ms = 90");
    // "Add" an expression pedal with a non-default taper: the pedal thread re-reads
    // Live every poll, so the curve_* knobs are exactly as live as the rest.
    let edited = format!(
      "{edited}\n** ELEM expression_pedals\n*** PARAM channel = 1\n\
       *** PARAM monome = \"a\"\n*** PARAM curve_remainder_exp_db = 40.0\n",
    );
    let rig = crate::rig_org::parse_org_rig(&edited).expect("edited rig parses");
    adopt_rig(&rig, &live).expect("adopts");

    assert_eq!(live.generation.load(Ordering::SeqCst), 1, "generation bumped");
    let p = live.params.lock().unwrap();
    assert_eq!(p.amplitude, 0.25);
    assert_eq!(p.edo, 41);
    assert_eq!(p.x_step, 7);
    assert_eq!(
      p.expression_pedals,
      [Some((0, PedalVolumeCurve { lin_frac: 0.1, exp_db: 40.0 })), None],
      "the pedal taper reloads (lin_frac keeps its default; exp_db moved)",
    );
    assert_eq!(p.timbres[2].fm.depth_cents, 25.0, "timbre slot 2 gained vibrato");
    assert_eq!(p.timbres[2].rel_fm.depth, 1.5, "and through-zero relative FM");
    assert!((p.slide_duration_secs - 0.25).abs() < 1e-6);
    assert!(
      (p.slide_pedal_smoother_secs - 0.09).abs() < 1e-6,
      "the pedal-slide smoother reloads (ms -> secs), so 'r' retunes the glide feel",
    );
  }

  /// The `[[timbres]]` square entry, replaced by the reload test.
  const WAVE_SQUARE: &str = "waveform = \"square\"";

  /// The pedal hook end to end, for a SECOND board's `accrete_control` bindings --
  /// the explicit replacement for the retired `softstep_accretes_toggle` mirror
  /// (TODO/cleaning/2_plan.org: "retiring softstep_accretes_toggle needs a pedal
  /// decision" -- Jeff's answer was a second KMSS carrying the six pedals). Same
  /// six-pedal mapping the old mirror hardcoded (1/2/3 -> grid 0's clear /
  /// needs_holding / accrete, 8/9/0 -> grid 1's), but through `rig_pedal_hook` /
  /// `drive_accrete` -- UNCONDITIONAL (no on-grid toggle gate) and keyed by device id,
  /// so a same-numbered pedal on a different device never crosses over.
  #[test]
  fn the_pedal_hook_drives_each_banks_trio_from_the_explicit_bindings() {
    use crate::types::{Timbre, VoiceSource};

    let ring = Arc::new(Mutex::new(vec![
      GridRing::new(AccreteState::new()),
      GridRing::new(AccreteState::new()),
    ]));
    let held_all = Arc::new(Mutex::new(vec![HashMap::new(); 2]));
    let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));
    let poly = Arc::new(Mutex::new(PolyrhythmState::new(2)));
    let mut actions: HashMap<(String, u8), PedalAction> = HashMap::new();
    for (pedal, grid, control) in [
      (1, 0, AccreteControlKind::Clear),
      (2, 0, AccreteControlKind::NeedsHolding),
      (3, 0, AccreteControlKind::Accrete),
      (8, 1, AccreteControlKind::Clear),
      (9, 1, AccreteControlKind::NeedsHolding),
      (0, 1, AccreteControlKind::Accrete),
    ] {
      actions.insert(("feet2".to_string(), pedal), PedalAction::Accrete { grid, control });
    }
    let hook = rig_pedal_hook(
      actions,
      Arc::clone(&ring),
      Arc::clone(&held_all),
      Arc::clone(&voices),
      Arc::clone(&poly),
      Duration::from_millis(2000),
      0.05,
      48000.0,
    );

    // A DIFFERENT device's pedal 3 must not touch this bank: the hook is keyed by
    // (device, pedal), so a same-numbered pedal on another device is simply unbound
    // (the caller's drumkit pedal map handles it, if anything does).
    assert!(!hook("other-board", 3, true), "another board's pedal 3 is unbound here");
    assert!(!ring.lock().unwrap()[0].accrete.accreting());
    // This board's own unmapped pedal (4) is unbound too -- only the six pedals above
    // are in the map, unconditionally (no toggle to flip first, unlike the old mirror).
    assert!(!hook("feet2", 4, true), "pedal 4 is not one of the six bound pedals");

    // Pedal 3 = grid 0's accrete: default needs-holding is OFF, so a tap toggles the
    // mode -- and the activation captures held notes from grid 0's registry ONLY.
    held_all.lock().unwrap()[0].insert((2, 3), 44);
    held_all.lock().unwrap()[1].insert((7, 7), 51);
    assert!(hook("feet2", 3, true), "pedal 3 is consumed");
    hook("feet2", 3, false);
    assert!(ring.lock().unwrap()[0].accrete.accreting(), "grid 0's accrete mode toggled by foot");
    assert!(!ring.lock().unwrap()[1].accrete.accreting(), "grid 1's bank untouched");
    assert!(
      {
        let mut r = ring.lock().unwrap();
        let gr = &mut r[0];
        gr.accrete.note_released_sustains(44, &mut gr.store)
      },
      "grid 0's held note was captured on activation",
    );
    assert!(
      ring.lock().unwrap()[1].store.classes(Reason::Sustain, 58).is_empty(),
      "grid 1's held note was NOT captured (banks are per-monome)",
    );

    // Accrete a note on grid 1 (pedal 0), then clear it (pedal 8): only grid 1's
    // bank and drone are cleared.
    assert!(hook("feet2", 0, true), "pedal 0 = grid 1's accrete, consumed");
    hook("feet2", 0, false);
    let mut a = SurfaceSink::new(0, Arc::clone(&voices), 80.0, 58, 48000.0, 0.003, 0.05, 1.0, 0.5, Arc::new(Mutex::new(vec![1.0; 2])), Arc::new(Mutex::new(vec![1.0; 2])));
    let mut b = SurfaceSink::new(1, Arc::clone(&voices), 80.0, 58, 48000.0, 0.003, 0.05, 1.0, 0.5, Arc::new(Mutex::new(vec![1.0; 2])), Arc::new(Mutex::new(vec![1.0; 2])));
    a.note_on((5, 5), 20, Timbre::default(), None);
    a.sustain_note((5, 5), 20);
    b.note_on((6, 6), 31, Timbre::default(), None);
    b.sustain_note((6, 6), 31);
    {
      let mut r = ring.lock().unwrap();
      let gr = &mut r[1];
      gr.accrete.note_played(31, &mut gr.store);
    }
    assert!(hook("feet2", 8, true), "pedal 8 = grid 1's clear, consumed");
    hook("feet2", 8, false);
    let v = voices.lock().unwrap();
    for (src, state) in v.iter() {
      let VoiceSource::SurfaceDrone { grid, .. } = src else { continue };
      match *grid {
        0 => assert_eq!(state.target_env, 1.0, "grid 0's drone keeps ringing"),
        1 => assert_eq!(state.target_env, 0.0, "grid 1's drone released by its clear"),
        g => panic!("unexpected sustained voice for grid {g}"),
      }
    }
    assert!(
      ring.lock().unwrap()[1].store.classes(Reason::Sustain, 58).is_empty(),
      "grid 1's set was flushed (accrete mode itself stays on -- clear never exits it)",
    );
    assert!(
      !ring.lock().unwrap()[0].store.classes(Reason::Sustain, 58).is_empty(),
      "grid 0's set survives grid 1's clear",
    );
  }

  /// The polyrhythm pad end-to-end: the factor cells rest dim while the tap cell
  /// blinks the seeded 1 Hz base from bring-up (no tap needed); two quick taps
  /// override the base with the ONE global tempo (the faster blink shows on both
  /// grids, caught mid-flash); the tempo-factor buttons and the =1 factored-pulse
  /// switch are PER-GRID -- a tempo-factor press lights only its own grid, a lone
  /// =1 tap turns that grid's cycling on (lit) and resets its tempo factor, and a
  /// fast =1 double-tap turns the cycling back off.
  #[test]
  fn factored_pulse_pad_blinks_globally_and_the_factored_pulse_switch_is_per_grid() {
    use crate::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockRig::start(0, &[GridSpec::grid_256("a"), GridSpec::grid_256("b")])
      .expect("start mock rig");
    let detector_port = mock.detector_port();
    let rig = load_named_rig("2-monomes_kmss-drums-mock").expect("mock rig loads");

    let _run = MockRun::start(rig.clone(), detector_port, "polyrhythm");

    let a = mock.grid(0);
    let b = mock.grid(1);
    let secs = Duration::from_secs;
    assert!(wait_until(secs(5), || a.registered() && b.registered()), "both grids register");
    // At rest: the tempo-factor cells glow dim, and the tap cell already blinks --
    // the base tempo is seeded at 1 Hz at bring-up, no tap required (10% duty =
    // 100 ms on per second; polling catches an on-flash within a few seconds).
    assert!(wait_until(secs(3), || a.level_at(14, 0) == 4), "x2 rests dim");
    assert!(wait_until(secs(5), || a.level_at(15, 0) == 15), "the tap cell blinks the seeded base");

    // Two taps ~200 ms apart set a 5 Hz tempo: the tap cell blinks at 10% duty
    // (20 ms on per 200 ms); polling catches an on-flash within a few seconds.
    a.press(15, 0);
    a.release(15, 0);
    thread::sleep(Duration::from_millis(200));
    a.press(15, 0);
    a.release(15, 0);
    assert!(wait_until(secs(5), || a.level_at(15, 0) == 15), "the tap cell blinks on grid a");
    assert!(wait_until(secs(5), || b.level_at(15, 0) == 15), "and on grid b (one shared tempo)");

    // x2 from grid b: PER-GRID -- b's cell lights, a's stays at rest.
    b.press(14, 0);
    b.release(14, 0);
    assert!(wait_until(secs(3), || b.level_at(14, 0) == 15), "grid b's x2 lit (its tempo factor leans up)");
    assert_ne!(a.level_at(14, 0), 15, "grid a's x2 stays unlit (tempo factors are per-grid; its resting dim now slow-flashes)");

    // The FIRST =1 press on grid b, with cycling off: it turns cycling ON (=1 lit)
    // and LEAVES the tempo factor alone -- x2 stays lit. Grid a's switch is untouched.
    b.press(15, 1);
    b.release(15, 1);
    assert!(wait_until(secs(3), || b.level_at(15, 1) == 15), "grid b's =1 lit: cycling on");
    assert_eq!(b.level_at(14, 0), 15, "the switch-on press KEEPS grid b's x2 tempo factor");
    assert_ne!(a.level_at(15, 1), 15, "grid a's =1 stays unlit (the switch is per-grid)");

    // Two more =1 presses, back to back after a >400 ms gap. The first of the pair is
    // a lone press on an already-cycling grid, so it zeroes the tempo factor (x2 -> dim)
    // and leaves cycling on; the second lands inside 400 ms of IT, so cycling goes OFF.
    // (The switch-on press above cannot be half of that pair -- it never armed the
    // detector -- which is why the >400 ms sleep is what separates the two phases.)
    thread::sleep(Duration::from_millis(500));
    b.press(15, 1);
    b.release(15, 1);
    b.press(15, 1);
    b.release(15, 1);
    assert!(wait_until(secs(3), || b.level_at(15, 1) == 4), "the fast second press: cycling off");
    assert!(wait_until(secs(3), || b.level_at(14, 0) == 4), "and the pair's first press zeroed x2");
    // The tempo DISPLAY survives, and blinks the unfactored tapped tempo on both grids.
    assert!(wait_until(secs(5), || b.level_at(15, 0) == 15), "the tap cell still blinks the tempo");
    assert!(wait_until(secs(5), || a.level_at(15, 0) == 15), "and grid a blinks it too");

  }

  /// The accrete (sustain) banks end-to-end: toggle accrete mode on grid a (its trio
  /// lights on grid a ONLY -- one bank per monome), play and release a note there --
  /// it keeps ringing (bright on BOTH grids: drones are sounding everywhere) -- while
  /// a note on grid b does NOT sustain, grid b's clear does NOT touch grid a's drone,
  /// and grid a's own clear finally drops it to the dim trail.
  #[test]
  fn accrete_banks_are_per_monome_and_sustain_until_their_own_clear() {
    use crate::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockRig::start(0, &[GridSpec::grid_256("a"), GridSpec::grid_256("b")])
      .expect("start mock rig");
    let detector_port = mock.detector_port();
    let rig = load_named_rig("2-monomes_kmss-drums-mock").expect("mock rig loads");

    let _run = MockRun::start(rig.clone(), detector_port, "accrete");

    let a = mock.grid(0);
    let b = mock.grid(1);
    let secs = Duration::from_secs;
    assert!(wait_until(secs(5), || a.registered() && b.registered()), "both grids register");

    // At rest all three buttons glow dim (findable), on both grids.
    for (x, name) in [(0, "clear"), (1, "needs_holding"), (2, "accrete")] {
      assert!(wait_until(secs(3), || a.level_at(x, 15) == 4), "grid a {name} rests dim");
      assert!(wait_until(secs(3), || b.level_at(x, 15) == 4), "grid b {name} rests dim");
    }

    // Tap accrete on grid a: needs_holding starts OFF, so key-down toggles accrete
    // mode on -- on grid a's bank ONLY (one bank per monome).
    a.press(2, 15);
    a.release(2, 15);
    assert!(wait_until(secs(3), || a.level_at(2, 15) == 15), "accrete lit on grid a");
    thread::sleep(Duration::from_millis(300)); // let grid b repaint before the negative check
    assert_ne!(b.level_at(2, 15), 15, "grid b's accrete stays unlit (its own bank is off)");

    // Play and release a note on grid a: sustained, it stays BRIGHT (not trail-dim)
    // on BOTH grids -- the drone is sounding, so it reflects everywhere.
    a.press(5, 5);
    assert!(wait_until(secs(3), || a.level_at(5, 5) == 15), "fingered note lights");
    a.release(5, 5);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(a.level_at(5, 5), 15, "released note keeps ringing bright on grid a");
    assert_eq!(b.level_at(5, 5), 15, "and reflects bright on grid b");

    // A note on grid b does NOT sustain: its bank is not accreting.
    b.press(8, 5);
    assert!(wait_until(secs(3), || b.level_at(8, 5) == 15), "grid b's note lights");
    b.release(8, 5);
    assert!(wait_until(secs(3), || b.level_at(8, 5) == 4), "and drops to the trail on release");

    // needs_holding on grid b: lights on grid b ONLY, and does NOT cancel grid a's
    // toggled mode (independent banks).
    b.press(1, 15);
    b.release(1, 15);
    assert!(wait_until(secs(3), || b.level_at(1, 15) == 15), "needs_holding lit on grid b");
    thread::sleep(Duration::from_millis(300));
    assert_ne!(a.level_at(1, 15), 15, "grid a's needs_holding stays unlit");
    assert_eq!(a.level_at(2, 15), 15, "grid a's accrete mode survives");

    // Clear from grid B: lights there while held, but grid a's drone keeps ringing.
    b.press(0, 15);
    assert!(wait_until(secs(3), || b.level_at(0, 15) == 15), "grid b's clear lit while held");
    thread::sleep(Duration::from_millis(300));
    assert_ne!(a.level_at(0, 15), 15, "grid a's clear stays unlit");
    b.release(0, 15);
    assert!(wait_until(secs(3), || b.level_at(0, 15) == 4), "clear dims on key-up");
    thread::sleep(Duration::from_millis(300));
    assert_eq!(a.level_at(5, 5), 15, "grid b's clear leaves grid a's drone ringing");

    // Clear from grid A: NOW the note falls back to the dim trail on both grids.
    a.press(0, 15);
    a.release(0, 15);
    assert!(wait_until(secs(3), || a.level_at(5, 5) == 4), "cleared note drops to the trail on a");
    assert!(wait_until(secs(3), || b.level_at(5, 5) == 4), "and on b");

  }

  /// The toggles end-to-end: every one (distortion / slide / mono) is PER-GRID --
  /// each rests dim, a press lights only that grid's cell, the two grids' switches
  /// are independent, and each turns off from its own grid.
  #[test]
  fn every_toggle_is_per_grid_and_independent() {
    use crate::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockRig::start(0, &[GridSpec::grid_256("a"), GridSpec::grid_256("b")])
      .expect("start mock rig");
    let detector_port = mock.detector_port();
    let rig = load_named_rig("2-monomes_kmss-drums-mock").expect("mock rig loads");

    let _run = MockRun::start(rig.clone(), detector_port, "distortion");

    let a = mock.grid(0);
    let b = mock.grid(1);
    let secs = Duration::from_secs;
    assert!(wait_until(secs(5), || a.registered() && b.registered()), "both grids register");
    // Distortion (0,1), slide (1,1), and mono (1,2) are all PER-GRID: grid a's press
    // lights grid a only, and grid b's toggle is a separate switch (b's press turns
    // b ON, not a off).
    for (x, y, name) in [(0, 1, "distortion"), (1, 1, "slide"), (1, 2, "mono")] {
      assert!(wait_until(secs(3), || a.level_at(x, y) == 4 && b.level_at(x, y) == 4),
        "{name} rests dim on both grids");
      a.press(x, y);
      a.release(x, y);
      assert!(wait_until(secs(3), || a.level_at(x, y) == 15), "{name} on: lit on grid a");
      thread::sleep(Duration::from_millis(300));
      assert_ne!(b.level_at(x, y), 15, "grid b's {name} stays unlit (per-grid switch)");
      b.press(x, y);
      b.release(x, y);
      assert!(wait_until(secs(3), || b.level_at(x, y) == 15), "grid b's own {name} turns b on");
      assert_eq!(a.level_at(x, y), 15, "grid a's {name} stays on -- independent switches");
      a.press(x, y);
      a.release(x, y);
      assert!(wait_until(secs(3), || a.level_at(x, y) == 4), "grid a's {name} off again");
      b.press(x, y);
      b.release(x, y);
      assert!(wait_until(secs(3), || b.level_at(x, y) == 4), "grid b's {name} off again");
    }

  }

  /// A monobright grid (old Series-256 serial id) can't dim a single LED, so the runtime
  /// fakes DIM by flashing binary quad frames; a varibright grid gets a steady level 4.
  /// Drive one of each and check the contrast on a scroll arrow (a DIM cell), plus that a
  /// note lights bright on the monobright grid through the binary-map path.
  #[test]
  fn monobright_grid_flashes_fake_dim() {
    use crate::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Grid 0's id is the classic Series-256 format -> detected monobright; grid 1's is the
    // newer format -> varibright. (Both report type "monome 256"; only the id differs.)
    let mock = MockRig::start(0, &[GridSpec::grid_256("m256-9"), GridSpec::grid_256("m0000777")])
      .expect("start mock rig");
    let detector_port = mock.detector_port();
    let rig = load_named_rig("2-monomes_kmss-drums-mock").expect("mock rig loads");

    let _run = MockRun::start(rig.clone(), detector_port, "monobright");

    let mono = mock.grid(0); // Series-256 serial -> fake-dim by flashing.
    let vari = mock.grid(1); // newer serial -> native levels.
    let secs = Duration::from_secs;
    assert!(wait_until(secs(5), || mono.registered() && vari.registered()), "both grids register");

    // The Down arrow (14,15) is a DIM cell. Varibright: steady native level 4.
    assert!(wait_until(secs(3), || vari.level_at(14, 15) == 4), "varibright arrow is a steady dim (level 4)");
    // Monobright: never steady 4 -- it flashes 0<->15 (binary), so we catch an on-frame.
    assert!(wait_until(secs(5), || mono.level_at(14, 15) == 15), "monobright arrow flashes on (fake dim)");
    // A note on the monobright grid lights solid via the binary-map path.
    mono.press(6, 6);
    assert!(wait_until(secs(3), || mono.level_at(6, 6) == 15), "monobright held note bright via map");

  }
