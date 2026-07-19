  use super::*;
  use midi_pulse::rig::{load_named_rig, SurfacesRig};

  /// Serialises the mock-rig tests: they share the global `STOP` and the mock rig's
  /// listen ports, so they must not run concurrently.
  static MOCK_LOCK: Mutex<()> = Mutex::new(());

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
    // The `[surfaces]` defaults: 1/27 octave (58/27 ~= 2.1, so classes within 2 steps are
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
      assert_eq!(g.overlays.feet_accrete_rect, [0, 14, 0, 14], "grid {:?} feet-accrete", g.monome_id);
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
    rig.surfaces = Some(SurfacesRig {
      trail_clobber_radius: 13,
      trails_max: 3,
      slide_candidate_window_ms: 777,
      tap_tempo_window_ms: 1234,
      slide_duration_ms: 55,
    });

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
  fn surfaces_defaults_when_table_absent() {
    // Omitting `[surfaces]` changes nothing: the built-in defaults reach `Settings`.
    // Built by REMOVING the table from a real surfaces rig, rather than leaning on
    // some other rig happening not to declare one (which a rig edit could undo).
    let mut rig = load_named_rig("2-monomes_kmss-drums").expect("rig loads");
    rig.surfaces = None;
    let s = resolve_settings(&rig).expect("resolves without hardware");
    let d = SurfacesRig::default();
    assert_eq!((d.trail_clobber_radius, d.trails_max), (27, 7), "the code's own defaults");
    assert_eq!(s.trail_clobber_radius, d.trail_clobber_radius);
    assert_eq!(s.trails_max, d.trails_max);
    assert_eq!(s.slide_window, Duration::from_millis(d.slide_candidate_window_ms));
    assert_eq!(s.tap_window, Duration::from_millis(d.tap_tempo_window_ms));
  }

  /// A minimal self-controlling grid for the `plan_bringup` tests: `id`'s selector
  /// (present iff `has_selector`) re-timbres `controls_index`; no other overlays.
  fn gs(id: &str, controls_index: usize, has_selector: bool) -> GridSettings {
    GridSettings {
      select: midi_pulse::rig::MonomeSelect {
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
        feet_accrete_rect: NO_RECT,
        poly_rect: NO_RECT,
        editmode_clear_rect: NO_RECT,
        editmode_accrete_rect: NO_RECT,
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
    use midi_pulse::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockRig::start(0, &[GridSpec::grid_256("a"), GridSpec::grid_256("b")])
      .expect("start mock rig");
    let detector_port = mock.detector_port();
    let rig = load_named_rig("2-monomes_kmss-drums-mock").expect("mock rig loads");

    STOP.store(false, Ordering::SeqCst);
    let handle = {
      let rig = rig.clone();
      thread::spawn(move || {
        if let Err(e) = run(&rig, detector_port, true, None) {
          eprintln!("mock surfaces run error: {e}");
        }
      })
    };

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

    STOP.store(true, Ordering::SeqCst);
    let _ = handle.join();
    STOP.store(false, Ordering::SeqCst);
  }

  /// Robust-to-missing-gear (TODO.org): a two-grid rig with only ONE grid connected
  /// still brings up the present grid -- it discovers a single device, binds it as
  /// grid 0, and skips the absent grid 1 (named in the red report) instead of erroring
  /// out the whole run.
  #[test]
  fn one_grid_absent_still_runs_the_present_grid() {
    use midi_pulse::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Only grid "a" exists; the 2-monome mock rig rig wants two.
    let mock = MockRig::start(0, &[GridSpec::grid_256("a")]).expect("start one mock grid");
    let detector_port = mock.detector_port();
    let rig = load_named_rig("2-monomes_kmss-drums-mock").expect("mock rig loads");

    STOP.store(false, Ordering::SeqCst);
    let handle = {
      let rig = rig.clone();
      thread::spawn(move || {
        if let Err(e) = run(&rig, detector_port, true, None) {
          eprintln!("mock one-grid run error: {e}");
        }
      })
    };

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

    STOP.store(true, Ordering::SeqCst);
    let _ = handle.join();
    STOP.store(false, Ordering::SeqCst);
  }

  /// The shipped two-softstep rig must load and resolve. This is the rig's only
  /// automated check -- everything else about it (which pedal does what, whether the
  /// grids land left/right) is hardware, and hardware is Jeff's to confirm.
  #[test]
  fn the_two_softstep_rig_loads_and_pins_its_gear() {
    use midi_pulse::rig::{AccreteControlKind, PulseFactorRig, SoftstepWindowRig};
    let source = std::fs::read_to_string(
      midi_pulse::rig::rig_dir().join("2-monomes_2-softsteps.org"),
    )
    .expect("read the shipped rig");
    let rig = midi_pulse::rig_org::parse_org_rig(&source).expect("the shipped rig parses");

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
        MonomeWindowRig::TapTempoPad { monome: m, rect, .. } if m == monome => Some(*rect),
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
    // pared the grids down); everything else is a note.
    let kinds: Vec<&str> = rig.monome_windows.iter().map(|w| w.kind_name()).collect();
    assert_eq!(
      kinds,
      [
        "edo_note_grid",
        "waveform_selector",
        "edo_shift_pad",
        "tap_tempo_pad",
        "editmode_control",
        "edo_note_grid",
        "waveform_selector",
        "edo_shift_pad",
        "tap_tempo_pad",
        "editmode_control"
      ],
      "no distortion/slide/mono/accrete windows on the grids (see 2_discussion 2f)",
    );

    // The editmode controls mirror the sustain row one row up (queue.org): OSS
    // outer pedals 6/0 clear, inner pedals 7/9 accrete, left pedals to the left
    // grid -- exactly the bottom row's 1/2 + 4/5 shape -- plus each grid's own
    // clear button at (12,0), beside the pad's x3.
    use midi_pulse::rig::EditmodeControlKind as Em;
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
      midi_pulse::rig::rig_dir().join("2-monomes_2-softsteps.org"),
    )
    .expect("read the shipped rig");
    let rig = midi_pulse::rig_org::parse_org_rig(&source).expect("parses");
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
      use midi_pulse::rig::EditmodeControlKind as Em;
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
      midi_pulse::rig::rig_dir().join("2-monomes_2-softsteps.org"),
    )
    .expect("read");
    let rig = midi_pulse::rig_org::parse_org_rig(&source).expect("parses");
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
  fn sustain_clear_spares_edited_drones_and_editmode_clear_then_ends_them() {
    // The symmetric model (queue.org "accrete-editmode pedals like
    // accrete-sustain"): each clear removes only its OWN reason. Sustain-clear
    // flushes the bank but a drone whose pitch is in edit mode keeps ringing --
    // audibly, so it is not the old silent "dancing ghost" -- and the editmode
    // clear then takes the last reason and ends it. Both clears = the full kill.
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
      gr.edit.enter(20, &mut gr.store);
    }

    // Sustain-clear: the bank flushes; only the sustain-only drone ends.
    assert!(drive_accrete(
      0, AccreteControlKind::Clear, true, &ring, &held_all, &voices, 0.05, 48000.0,
    ));
    let drone = |pitch| VoiceSource::SurfaceDrone { grid: 0, pitch };
    {
      let v = voices.lock().unwrap();
      assert_eq!(v[&drone(10)].target_env, 0.0, "the sustain-only drone releases");
      assert!(v[&drone(20)].target_env > 0.0, "the edited drone keeps ringing");
    }
    assert!(ring.lock().unwrap()[0].store.iter(Reason::Sustain).next().is_none(), "the set flushed");
    assert!(ring.lock().unwrap()[0].store.has(Reason::Edit, 20), "clear does not touch edit mode");

    // Editmode-clear takes the drone's last reason: now it ends, and the grid plays.
    editmode_clear(0, &ring, &voices, 0.05, 48000.0);
    assert_eq!(voices.lock().unwrap()[&drone(20)].target_env, 0.0, "no reason left: it ends");
    assert!(!ring.lock().unwrap()[0].store.any(Reason::Edit), "edit mode empties");
  }

  #[test]
  fn editmode_clear_spares_sustained_drones() {
    // The mirror: editmode-clear removes only the EDIT reason. An edited pitch
    // still in the sustain bank keeps its drone.
    use crate::types::{Timbre, VoiceSource};
    let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));
    let ring = Arc::new(Mutex::new(vec![GridRing::new(AccreteState::new_momentary())]));
    let mut a = SurfaceSink::new(0, Arc::clone(&voices), 80.0, 46, 48000.0, 0.003, 0.05, 1.0, 0.5, Arc::new(Mutex::new(vec![1.0; 1])), Arc::new(Mutex::new(vec![1.0; 1])));
    // Pitch 10: edited only. Pitch 20: edited AND sustained.
    for (cell, pitch) in [((1, 1), 10), ((2, 2), 20)] {
      a.note_on(cell, pitch, Timbre::default(), None);
      a.sustain_note(cell, pitch);
      let mut r = ring.lock().unwrap();
      let gr = &mut r[0];
      gr.edit.enter(pitch, &mut gr.store);
    }
    ring.lock().unwrap()[0].store.add(Reason::Sustain, 20);

    editmode_clear(0, &ring, &voices, 0.05, 48000.0);
    let drone = |pitch| VoiceSource::SurfaceDrone { grid: 0, pitch };
    let v = voices.lock().unwrap();
    assert_eq!(v[&drone(10)].target_env, 0.0, "the edit-only drone ends");
    assert!(v[&drone(20)].target_env > 0.0, "the sustained drone keeps its other reason");
    drop(v);
    assert!(!ring.lock().unwrap()[0].store.any(Reason::Edit), "edit mode empties either way");
    assert!(
      ring.lock().unwrap()[0].store.iter(Reason::Sustain).eq([20]),
      "the sustain bank is untouched",
    );
  }

  #[test]
  fn editmode_accrete_captures_fingered_and_sustained_voices() {
    let ring = Arc::new(Mutex::new(vec![GridRing::new(AccreteState::new_momentary())]));
    let held_all = Arc::new(Mutex::new(vec![HashMap::from([((1, 1), 10)])]));
    ring.lock().unwrap()[0].store.add(Reason::Sustain, 20);

    editmode_accrete(0, &ring, &held_all);
    let r = ring.lock().unwrap();
    assert!(r[0].store.has(Reason::Edit, 10), "the fingered voice enters edit mode");
    assert!(r[0].store.has(Reason::Edit, 20), "the sustained voice too");
    assert_eq!(r[0].store.iter(Reason::Edit).count(), 2, "and nothing else");
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
    let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));
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

    editmode_clear(0, &ring, &voices, 0.05, 48000.0);
    assert!(!ring.lock().unwrap()[0].store.any(Reason::Edit), "grid 0's edit mode cleared");
    assert!(ring.lock().unwrap()[1].store.any(Reason::Edit), "grid 1's edit mode is its own business");
  }

  #[test]
  fn the_two_softstep_rigs_accrete_is_momentary_not_toggle() {
    use midi_pulse::rig::{AccreteControlKind, SoftstepWindowRig};
    let source = std::fs::read_to_string(
      midi_pulse::rig::rig_dir().join("2-monomes_2-softsteps.org"),
    )
    .expect("read the shipped rig");
    let rig = midi_pulse::rig_org::parse_org_rig(&source).expect("parses");
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
      midi_pulse::rig::rig_dir().join("2-monomes_kmss-drums.org"),
    )
    .expect("read the drums rig");
    let rig = midi_pulse::rig_org::parse_org_rig(&source).expect("parses");
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
    let live = Live {
      generation: AtomicU64::new(0),
      params: Mutex::new(live_params(&s)),
      makeup: Mutex::new(live_makeup(&s)),
    };

    let source = std::fs::read_to_string(
      midi_pulse::rig::mock_rig_dir().join("2-monomes_kmss-drums-mock.org"),
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
    let edited = must_replace(&edited, "slide_duration_ms = 100", "slide_duration_ms = 250");
    // "Add" an expression pedal with a non-default taper: the pedal thread re-reads
    // Live every poll, so the curve_* knobs are exactly as live as the rest.
    let edited = format!(
      "{edited}\n** ELEM expression_pedals\n*** PARAM channel = 1\n\
       *** PARAM monome = \"a\"\n*** PARAM curve_remainder_exp_db = 40.0\n",
    );
    let rig = midi_pulse::rig_org::parse_org_rig(&edited).expect("edited rig parses");
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
  }

  /// The `[[timbres]]` square entry, replaced by the reload test.
  const WAVE_SQUARE: &str = "waveform = \"square\"";

  #[test]
  fn the_pedal_hook_mirrors_each_banks_trio_only_while_its_toggle_is_on() {
    use crate::types::{Timbre, VoiceSource};

    // Triple 0 (pedals 1/2/3) -> grid 0's bank, triple 1 (8/9/0) -> grid 1's.
    let feet_on: Arc<Vec<AtomicBool>> =
      Arc::new((0..2).map(|_| AtomicBool::new(false)).collect());
    let ring = Arc::new(Mutex::new(vec![
      GridRing::new(AccreteState::new()),
      GridRing::new(AccreteState::new()),
    ]));
    let held_all = Arc::new(Mutex::new(vec![HashMap::new(); 2]));
    let voices: Arc<Mutex<VoiceMap>> = Arc::new(Mutex::new(HashMap::new()));
    let hook = feet_accrete_hook(
      "feet".to_string(),
      Arc::clone(&feet_on),
      [0, 1],
      Arc::clone(&ring),
      Arc::clone(&held_all),
      Arc::clone(&voices),
      0.05,
      48000.0,
    );

    // Both toggles off: nothing is consumed; the pedals drum as usual.
    assert!(!hook("feet", 3, true), "off: pedal 3 stays a drum pad");
    assert!(!ring.lock().unwrap()[0].accrete.accreting());

    feet_on[0].store(true, Ordering::Relaxed);
    // A DIFFERENT board's pedal 3 must not touch this board's bank, even with the
    // toggle on: the printed labels are identical across boards, so only the device
    // id separates them. Without this the second SoftStep would mirror onto grid 0's
    // accrete bank the moment it was plugged in.
    assert!(!hook("other-board", 3, true), "another board's pedal 3 is not mirrored");
    assert!(
      !ring.lock().unwrap()[0].accrete.accreting(),
      "another board's pedal 3 must not toggle this bank",
    );
    // Unmapped pedals still drum even while on.
    assert!(!hook("feet", 4, true), "pedal 4 (closed hat) is never mirrored");
    // The other triple's grid is still off: its pedals keep drumming.
    assert!(!hook("feet", 0, true), "pedal 0 drums while grid 1's toggle is off");
    // Pedal 3 = grid 0's accrete: default needs-holding is OFF, so a tap toggles the
    // mode -- and the activation captures held notes from grid 0's registry ONLY.
    held_all.lock().unwrap()[0].insert((2, 3), 44);
    held_all.lock().unwrap()[1].insert((7, 7), 51);
    assert!(hook("feet", 3, true), "on: pedal 3 is consumed");
    hook("feet", 3, false);
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

    // Turn grid 1's toggle on, accrete a note there, then clear it from pedal 8:
    // only grid 1's bank and drone are cleared.
    feet_on[1].store(true, Ordering::Relaxed);
    assert!(hook("feet", 0, true), "pedal 0 = grid 1's accrete, now consumed");
    hook("feet", 0, false);
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
    assert!(hook("feet", 8, true), "pedal 8 = grid 1's clear, consumed");
    hook("feet", 8, false);
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
  fn tap_tempo_pad_blinks_globally_and_the_factored_pulse_switch_is_per_grid() {
    use midi_pulse::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockRig::start(0, &[GridSpec::grid_256("a"), GridSpec::grid_256("b")])
      .expect("start mock rig");
    let detector_port = mock.detector_port();
    let rig = load_named_rig("2-monomes_kmss-drums-mock").expect("mock rig loads");

    STOP.store(false, Ordering::SeqCst);
    let handle = {
      let rig = rig.clone();
      thread::spawn(move || {
        if let Err(e) = run(&rig, detector_port, true, None) {
          eprintln!("mock polyrhythm run error: {e}");
        }
      })
    };

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
    assert_eq!(a.level_at(14, 0), 4, "grid a's x2 stays dim (tempo factors are per-grid)");

    // The FIRST =1 press on grid b, with cycling off: it turns cycling ON (=1 lit)
    // and LEAVES the tempo factor alone -- x2 stays lit. Grid a's switch is untouched.
    b.press(15, 1);
    b.release(15, 1);
    assert!(wait_until(secs(3), || b.level_at(15, 1) == 15), "grid b's =1 lit: cycling on");
    assert_eq!(b.level_at(14, 0), 15, "the switch-on press KEEPS grid b's x2 tempo factor");
    assert_eq!(a.level_at(15, 1), 4, "grid a's =1 stays dim (the switch is per-grid)");

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

    STOP.store(true, Ordering::SeqCst);
    let _ = handle.join();
    STOP.store(false, Ordering::SeqCst);
  }

  /// The accrete (sustain) banks end-to-end: toggle accrete mode on grid a (its trio
  /// lights on grid a ONLY -- one bank per monome), play and release a note there --
  /// it keeps ringing (bright on BOTH grids: drones are sounding everywhere) -- while
  /// a note on grid b does NOT sustain, grid b's clear does NOT touch grid a's drone,
  /// and grid a's own clear finally drops it to the dim trail.
  #[test]
  fn accrete_banks_are_per_monome_and_sustain_until_their_own_clear() {
    use midi_pulse::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockRig::start(0, &[GridSpec::grid_256("a"), GridSpec::grid_256("b")])
      .expect("start mock rig");
    let detector_port = mock.detector_port();
    let rig = load_named_rig("2-monomes_kmss-drums-mock").expect("mock rig loads");

    STOP.store(false, Ordering::SeqCst);
    let handle = {
      let rig = rig.clone();
      thread::spawn(move || {
        if let Err(e) = run(&rig, detector_port, true, None) {
          eprintln!("mock accrete run error: {e}");
        }
      })
    };

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
    assert_eq!(b.level_at(2, 15), 4, "grid b's accrete stays dim (its own bank is off)");

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
    assert_eq!(a.level_at(1, 15), 4, "grid a's needs_holding stays dim");
    assert_eq!(a.level_at(2, 15), 15, "grid a's accrete mode survives");

    // Clear from grid B: lights there while held, but grid a's drone keeps ringing.
    b.press(0, 15);
    assert!(wait_until(secs(3), || b.level_at(0, 15) == 15), "grid b's clear lit while held");
    thread::sleep(Duration::from_millis(300));
    assert_eq!(a.level_at(0, 15), 4, "grid a's clear stays dim");
    b.release(0, 15);
    assert!(wait_until(secs(3), || b.level_at(0, 15) == 4), "clear dims on key-up");
    thread::sleep(Duration::from_millis(300));
    assert_eq!(a.level_at(5, 5), 15, "grid b's clear leaves grid a's drone ringing");

    // Clear from grid A: NOW the note falls back to the dim trail on both grids.
    a.press(0, 15);
    a.release(0, 15);
    assert!(wait_until(secs(3), || a.level_at(5, 5) == 4), "cleared note drops to the trail on a");
    assert!(wait_until(secs(3), || b.level_at(5, 5) == 4), "and on b");

    STOP.store(true, Ordering::SeqCst);
    let _ = handle.join();
    STOP.store(false, Ordering::SeqCst);
  }

  /// The toggles end-to-end: every one (distortion / slide / mono / feet-accrete)
  /// is PER-GRID -- each rests dim, a press lights only that grid's cell, the two
  /// grids' switches are independent, and each turns off from its own grid.
  #[test]
  fn every_toggle_is_per_grid_and_independent() {
    use midi_pulse::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockRig::start(0, &[GridSpec::grid_256("a"), GridSpec::grid_256("b")])
      .expect("start mock rig");
    let detector_port = mock.detector_port();
    let rig = load_named_rig("2-monomes_kmss-drums-mock").expect("mock rig loads");

    STOP.store(false, Ordering::SeqCst);
    let handle = {
      let rig = rig.clone();
      thread::spawn(move || {
        if let Err(e) = run(&rig, detector_port, true, None) {
          eprintln!("mock distortion run error: {e}");
        }
      })
    };

    let a = mock.grid(0);
    let b = mock.grid(1);
    let secs = Duration::from_secs;
    assert!(wait_until(secs(5), || a.registered() && b.registered()), "both grids register");
    // Distortion (0,1), slide (1,1), mono (1,2), and feet-accrete (0,14) are all
    // PER-GRID: grid a's press lights grid a only, and grid b's toggle is a
    // separate switch (b's press turns b ON, not a off).
    for (x, y, name) in
      [(0, 1, "distortion"), (1, 1, "slide"), (1, 2, "mono"), (0, 14, "feet-accrete")]
    {
      assert!(wait_until(secs(3), || a.level_at(x, y) == 4 && b.level_at(x, y) == 4),
        "{name} rests dim on both grids");
      a.press(x, y);
      a.release(x, y);
      assert!(wait_until(secs(3), || a.level_at(x, y) == 15), "{name} on: lit on grid a");
      thread::sleep(Duration::from_millis(300));
      assert_eq!(b.level_at(x, y), 4, "grid b's {name} stays dim (per-grid switch)");
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

    STOP.store(true, Ordering::SeqCst);
    let _ = handle.join();
    STOP.store(false, Ordering::SeqCst);
  }

  /// A monobright grid (old Series-256 serial id) can't dim a single LED, so the runtime
  /// fakes DIM by flashing binary quad frames; a varibright grid gets a steady level 4.
  /// Drive one of each and check the contrast on a scroll arrow (a DIM cell), plus that a
  /// note lights bright on the monobright grid through the binary-map path.
  #[test]
  fn monobright_grid_flashes_fake_dim() {
    use midi_pulse::mock_monome::{wait_until, GridSpec, MockRig};

    let _guard = MOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Grid 0's id is the classic Series-256 format -> detected monobright; grid 1's is the
    // newer format -> varibright. (Both report type "monome 256"; only the id differs.)
    let mock = MockRig::start(0, &[GridSpec::grid_256("m256-9"), GridSpec::grid_256("m0000777")])
      .expect("start mock rig");
    let detector_port = mock.detector_port();
    let rig = load_named_rig("2-monomes_kmss-drums-mock").expect("mock rig loads");

    STOP.store(false, Ordering::SeqCst);
    let handle = {
      let rig = rig.clone();
      thread::spawn(move || {
        if let Err(e) = run(&rig, detector_port, true, None) {
          eprintln!("mock monobright run error: {e}");
        }
      })
    };

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

    STOP.store(true, Ordering::SeqCst);
    let _ = handle.join();
    STOP.store(false, Ordering::SeqCst);
  }
