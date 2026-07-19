// The live core (rig loader, monome, device_assign, edo_play, expression_pedals, midi,
// mock_monome, monome_brightness) was unburied into the `edo_surface` crate in cleaning
// phase 7. What stays here still refers to it: `use edo_surface::rig` etc.
pub mod mapping;
pub mod monome_window;
pub mod piano_runtime;
pub mod piano_transform;
