use midi_pulse::monome_window;
use std::time::Instant;

use super::rig::RemapRig;
use super::layout::{
  edo_local_cell, grid_step, map_rect, record_control_cells, scale_control_cells,
  scale_slot_cells, scale_slots_rect, undo_cell, WindowId,
};
use super::record::{self, OutputSource, RecordControl, RecordRuntime, SharedOutputGate};
use super::remap::{apply_grid_press, apply_snapshot, preimage_for_step, undo_remap};
use super::scale::{self, ScaleControl};
use super::render::{self, LedPhases};
use super::state::{PreimageRowState, RemappableEdoState, SoundingPitchCounts};
use super::PREIMAGE_ROW_Y;

pub(crate) struct KeyContext<'a> {
  pub(crate) state: &'a mut RemappableEdoState,
  pub(crate) recorder: &'a mut RecordRuntime,
  pub(crate) output_gate: &'a SharedOutputGate,
  pub(crate) preimage_row: &'a mut PreimageRowState,
  pub(crate) now: Instant,
}

pub(crate) struct RenderContext<'a> {
  pub(crate) state: &'a RemappableEdoState,
  pub(crate) sounding: &'a SoundingPitchCounts,
  pub(crate) recorder: &'a RecordRuntime,
  pub(crate) preimage_row_counts: &'a [u16; 12],
  pub(crate) preimage_row_flash_until: &'a [Option<Instant>; 12],
  pub(crate) now: Instant,
  pub(crate) phases: LedPhases,
  pub(crate) windows: &'a [monome_window::Window<WindowId>],
  pub(crate) levels: &'a mut [u8],
}

pub(crate) trait WindowBehavior {
  fn kind_name(&self) -> &'static str;
  fn window(&self, rig: &RemapRig) -> Option<monome_window::Window<WindowId>>;
  fn key_down(&self, _ctx: &mut KeyContext<'_>, _x: i32, _y: i32) -> bool {
    false
  }
  fn key_up(&self, _ctx: &mut KeyContext<'_>, _x: i32, _y: i32) -> bool {
    false
  }
  fn render(&self, _ctx: &mut RenderContext<'_>) {}
}

#[derive(Clone, Copy)]
pub(crate) struct PreimageRowBehavior;

/// 12 keys from the first row represent the 12-edo space,
/// but shows what note is being pressed on the *monome*,
/// in the big 2d higher-edo grid.
impl WindowBehavior for PreimageRowBehavior {
  fn kind_name(&self) -> &'static str {
    "preimage_row"
  }

  // Owns the top pitch-class row, limited to the first twelve columns.
  fn window(&self, rig: &RemapRig
  ) -> Option<monome_window::Window<WindowId>> {
    if PREIMAGE_ROW_Y < 0
      || PREIMAGE_ROW_Y >= rig.grid_h
      || rig.grid_w <= 0 { return None; }
    Some(monome_window::Window {
      id: WindowId::PreimageRow,
      rect: ( (0, PREIMAGE_ROW_Y),
               ((12.min(rig.grid_w)) - 1,
                PREIMAGE_ROW_Y), ), } ) }

  // Draws pitch-class hints and transient flashes caused by grid presses.
  fn render(&self, ctx: &mut RenderContext<'_>) {
    render::render_preimage_row(
      ctx.state,
      ctx.preimage_row_counts,
      ctx.preimage_row_flash_until,
      ctx.now,
      ctx.phases,
      ctx.levels, ); }}

#[derive(Clone, Copy)]
pub(crate) struct RemappableUn12GridBehavior;

impl WindowBehavior for RemappableUn12GridBehavior {
  fn kind_name(&self) -> &'static str {
    "remappable_un12_grid"
  }

  // In the first 12 columns, owns all rows below the 12-edo preimage row.
  fn window(&self, rig: &RemapRig
  ) -> Option<monome_window::Window<WindowId>> {
    let rect = map_rect(rig);
    if rect.x0 >= rect.x1
      || rect.y0 >= rect.y1
      { return None; }
    Some(monome_window::Window {
      id: WindowId::Edo,
      rect: ((rect.x0, rect.y0), (rect.x1 - 1, rect.y1 - 1)), } ) }

  // Moves or loosens the pitch preimage for the pressed EDO step.
  fn key_down(&self, ctx: &mut KeyContext<'_>, x: i32, y: i32) -> bool {
    let Some((local_x, local_y)) = edo_local_cell(&ctx.state.rig, x, y) else {
      return false; };
    let step = grid_step(&ctx.state.rig, local_x, local_y);
    let preimage_before = preimage_for_step(ctx.state, step);
    let changed = apply_grid_press(ctx.state, x, y);
    let preimage_row_preimage =
      preimage_before.or_else(
        || preimage_for_step(ctx.state, step));
    if let Some(preimage) = preimage_row_preimage {
      ctx.preimage_row.press((x, y), preimage, ctx.now);
      true
    } else { changed }}

  // Releases any preimage-row flash count associated with this held cell.
  fn key_up(&self, ctx: &mut KeyContext<'_>,
            x: i32, y: i32
  ) -> bool {
    ctx.preimage_row.release((x, y)) }

  // Draws sounding notes, anchor pitch classes, and mapped pitch classes.
  fn render(&self, ctx: &mut RenderContext<'_>) {
    render::render_remappable_un12_grid(ctx); }}

#[derive(Clone, Copy)]
pub(crate) struct RemapUndoButtonBehavior;

impl WindowBehavior for RemapUndoButtonBehavior {
  fn kind_name(&self) -> &'static str {
    "remap_undo_button" }

  // Owns the single undo cell in the lower-right corner.
  fn window(&self, rig: &RemapRig
  ) -> Option<monome_window::Window<WindowId>> {
    undo_cell(rig).map(|cell| monome_window::Window {
      id: WindowId::Undo,
      rect: (cell, cell), } ) }

  // Restores the previous remapping snapshot, if one exists.
  fn key_down(&self, ctx: &mut KeyContext<'_>, _x: i32, _y: i32) -> bool {
    undo_remap(ctx.state) }

  // Releases any preimage-row flash count associated with this held cell.
  fn key_up(&self, ctx: &mut KeyContext<'_>, x: i32, y: i32) -> bool {
    ctx.preimage_row.release((x, y)) }

  // Lights the undo cell only when there is history to undo.
  fn render(&self, ctx: &mut RenderContext<'_>) {
    render::render_remap_undo_button(ctx); }}

#[derive(Clone, Copy)]
pub(crate) struct RecordControlBehavior {
  control: RecordControl,
}

impl RecordControlBehavior {
  pub(crate) fn new(control: RecordControl) -> Self {
    RecordControlBehavior { control }
  }
}

impl WindowBehavior for RecordControlBehavior {
  // Internal behavior name shared by all recording control buttons.
  fn kind_name(&self) -> &'static str {
    "record_control"
  }

  // Owns the one configured cell for this particular recording control.
  fn window(&self, rig: &RemapRig
  ) -> Option<monome_window::Window<WindowId>> {
    record_control_cells(rig)
      .into_iter()
      .find(|(_, control)| *control == self.control)
      .map(|(cell, control)| monome_window::Window {
        id: WindowId::RecordControl(control),
        rect: (cell, cell), } ) }

  // Applies the recording control action and any snapshot/output side effects.
  fn key_down(&self, ctx: &mut KeyContext<'_>,
              x: i32, y: i32
  ) -> bool {
    let action = ctx.recorder.key_down(
      self.control, ctx.now, ctx.state.snapshot());
    for (original_note, output) in action.release_playback {
      if ctx . output_gate . release_source(
        OutputSource::Playback { original_note }, output)
      { record::trace_midi_event(
          "midi-output control-release",
          &[0x80 | output.channel, output.note, 0],
          ctx.recorder,
          ctx.now, ); }}
    if let Some(snapshot) = action.apply_snapshot {
      apply_snapshot(ctx.state, snapshot); }
    record::trace_runtime(
      &format!("control-down {:?} x={x} y={y}", self.control),
      ctx.recorder,
      ctx.now );
    true }

  // Finishes any press-and-hold recording control action.
  fn key_up(&self, ctx: &mut KeyContext<'_>, x: i32, y: i32) -> bool {
    let changed = ctx.recorder.key_up(self.control);
    if changed {
      record::trace_runtime(
        &format!("control-up {:?} x={x} y={y}", self.control),
        ctx.recorder,
        ctx.now, ); }
    changed }

  // Draws the LED state for this particular recording control.
  fn render(&self, ctx: &mut RenderContext<'_>) {
    render::render_record_control(ctx, self.control); }}

#[derive(Clone, Copy)]
pub(crate) struct ScaleSlotsBehavior;

impl WindowBehavior for ScaleSlotsBehavior {
  fn kind_name(&self) -> &'static str {
    "scale_slots"
  }

  // Owns the flexible rig-defined slot rect.
  fn window(&self, rig: &RemapRig
  ) -> Option<monome_window::Window<WindowId>> {
    scale_slots_rect(rig).map(|rect| monome_window::Window {
      id: WindowId::ScaleSlots,
      rect, } ) }

  // Stores, empties, or recalls the pressed slot depending on the armed button.
  fn key_down(&self, ctx: &mut KeyContext<'_>, x: i32, y: i32) -> bool {
    let Some(index) = scale_slot_cells(&ctx.state.rig)
      .into_iter()
      .position(|cell| cell == (x, y))
    else {
      return false; };
    scale::press_slot(ctx.state, index) }

  // Draws saved slots: active solid, other written slots flashing dimly.
  fn render(&self, ctx: &mut RenderContext<'_>) {
    render::render_scale_slots(ctx); }}

#[derive(Clone, Copy)]
pub(crate) struct ScaleControlBehavior {
  control: ScaleControl,
}

impl ScaleControlBehavior {
  pub(crate) fn new(control: ScaleControl) -> Self {
    ScaleControlBehavior { control }
  }
}

impl WindowBehavior for ScaleControlBehavior {
  fn kind_name(&self) -> &'static str {
    "scale_control"
  }

  // Owns the one configured cell for this particular scale arm button.
  fn window(&self, rig: &RemapRig
  ) -> Option<monome_window::Window<WindowId>> {
    scale_control_cells(rig)
      .into_iter()
      .find(|(_, control)| *control == self.control)
      .map(|(cell, control)| monome_window::Window {
        id: WindowId::ScaleControl(control),
        rect: (cell, cell), } ) }

  // Toggles this arm button (arming it disarms the sibling).
  fn key_down(&self, ctx: &mut KeyContext<'_>, _x: i32, _y: i32) -> bool {
    scale::toggle_arm(ctx.state, self.control) }

  // Draws the arm button: flashing brightly when armed, dim otherwise.
  fn render(&self, ctx: &mut RenderContext<'_>) {
    render::render_scale_control(ctx, self.control); }}

#[derive(Clone, Copy)]
pub(crate) enum RemapWindowBehavior {
  PreimageRow(PreimageRowBehavior),
  RemappableUn12Grid(RemappableUn12GridBehavior),
  RemapUndoButton(RemapUndoButtonBehavior),
  RecordControl(RecordControlBehavior),
  ScaleSlots(ScaleSlotsBehavior),
  ScaleControl(ScaleControlBehavior),
}

impl WindowBehavior for RemapWindowBehavior {
  // Delegates the rig/etags name to the wrapped behavior.
  fn kind_name(&self) -> &'static str {
    match self {
      RemapWindowBehavior::PreimageRow(behavior) => behavior.kind_name(),
      RemapWindowBehavior::RemappableUn12Grid(behavior) => behavior.kind_name(),
      RemapWindowBehavior::RemapUndoButton(behavior) => behavior.kind_name(),
      RemapWindowBehavior::RecordControl(behavior) => behavior.kind_name(),
      RemapWindowBehavior::ScaleSlots(behavior) => behavior.kind_name(),
      RemapWindowBehavior::ScaleControl(behavior) => behavior.kind_name(),
    }
  }

  // Delegates cell ownership to the wrapped behavior.
  fn window(&self, rig: &RemapRig) -> Option<monome_window::Window<WindowId>> {
    match self {
      RemapWindowBehavior::PreimageRow(behavior) => behavior.window(rig),
      RemapWindowBehavior::RemappableUn12Grid(behavior) => behavior.window(rig),
      RemapWindowBehavior::RemapUndoButton(behavior) => behavior.window(rig),
      RemapWindowBehavior::RecordControl(behavior) => behavior.window(rig),
      RemapWindowBehavior::ScaleSlots(behavior) => behavior.window(rig),
      RemapWindowBehavior::ScaleControl(behavior) => behavior.window(rig),
    }
  }

  // Delegates key-down behavior after dispatch has selected a window.
  fn key_down(&self, ctx: &mut KeyContext<'_>, x: i32, y: i32) -> bool {
    match self {
      RemapWindowBehavior::PreimageRow(behavior) => behavior.key_down(ctx, x, y),
      RemapWindowBehavior::RemappableUn12Grid(behavior) => behavior.key_down(ctx, x, y),
      RemapWindowBehavior::RemapUndoButton(behavior) => behavior.key_down(ctx, x, y),
      RemapWindowBehavior::RecordControl(behavior) => behavior.key_down(ctx, x, y),
      RemapWindowBehavior::ScaleSlots(behavior) => behavior.key_down(ctx, x, y),
      RemapWindowBehavior::ScaleControl(behavior) => behavior.key_down(ctx, x, y),
    }
  }

  // Delegates key-up behavior after dispatch has selected a window.
  fn key_up(&self, ctx: &mut KeyContext<'_>, x: i32, y: i32) -> bool {
    match self {
      RemapWindowBehavior::PreimageRow(behavior) => behavior.key_up(ctx, x, y),
      RemapWindowBehavior::RemappableUn12Grid(behavior) => behavior.key_up(ctx, x, y),
      RemapWindowBehavior::RemapUndoButton(behavior) => behavior.key_up(ctx, x, y),
      RemapWindowBehavior::RecordControl(behavior) => behavior.key_up(ctx, x, y),
      RemapWindowBehavior::ScaleSlots(behavior) => behavior.key_up(ctx, x, y),
      RemapWindowBehavior::ScaleControl(behavior) => behavior.key_up(ctx, x, y),
    }
  }

  // Delegates rendering to the wrapped behavior.
  fn render(&self, ctx: &mut RenderContext<'_>) {
    match self {
      RemapWindowBehavior::PreimageRow(behavior) => behavior.render(ctx),
      RemapWindowBehavior::RemappableUn12Grid(behavior) => behavior.render(ctx),
      RemapWindowBehavior::RemapUndoButton(behavior) => behavior.render(ctx),
      RemapWindowBehavior::RecordControl(behavior) => behavior.render(ctx),
      RemapWindowBehavior::ScaleSlots(behavior) => behavior.render(ctx),
      RemapWindowBehavior::ScaleControl(behavior) => behavior.render(ctx),
    }
  }
}

pub(crate) fn behaviors(rig: &RemapRig) -> Vec<RemapWindowBehavior> {
  let mut behaviors = vec![
    RemapWindowBehavior::PreimageRow(PreimageRowBehavior),
    RemapWindowBehavior::RemapUndoButton(RemapUndoButtonBehavior),
    RemapWindowBehavior::RemappableUn12Grid(RemappableUn12GridBehavior),
  ];
  behaviors.extend(
    record_control_cells(rig)
      .into_iter()
      .map(|(_, control)| {
        RemapWindowBehavior::RecordControl(RecordControlBehavior::new(control))
      }),
  );
  if scale_slots_rect(rig).is_some() {
    behaviors.push(RemapWindowBehavior::ScaleSlots(ScaleSlotsBehavior));
  }
  behaviors.extend(
    scale_control_cells(rig)
      .into_iter()
      .map(|(_, control)| {
        RemapWindowBehavior::ScaleControl(ScaleControlBehavior::new(control))
      }),
  );
  behaviors
}

pub(crate) fn windows(rig: &RemapRig) -> Vec<monome_window::Window<WindowId>> {
  behaviors(rig)
    .into_iter()
    .filter_map(|behavior| behavior.window(rig))
    .collect()
}

pub(crate) fn behavior_for_cell(
  rig: &RemapRig,
  x: i32,
  y: i32,
) -> Option<RemapWindowBehavior> {
  let windows = windows(rig);
  let id = monome_window::window_for_cell(&windows, (x, y))?;
  behaviors(rig)
    .into_iter()
    .find(|behavior| behavior.window(rig).is_some_and(|window| window.id == id))
}

pub(crate) fn render_all(ctx: &mut RenderContext<'_>) {
  for behavior in behaviors(&ctx.state.rig) {
    behavior.render(ctx);
  }
}
