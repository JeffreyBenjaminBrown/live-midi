//! Distinct-grid assignment now lives in the lib (`midi_pulse::device_assign`) so
//! the surfaces runtime can share the two-grid bind. Re-exported here so the
//! looper's `device::assign_distinct_devices` call site is unchanged.

pub use midi_pulse::device_assign::assign_distinct_devices;
