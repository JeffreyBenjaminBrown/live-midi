//! Distinct-grid assignment now lives in the lib (`edo_surface::device_assign`) so
//! the surfaces runtime can share the two-grid bind. Re-exported here so the
//! looper's `device::assign_distinct_devices` call site is unchanged.

pub use edo_surface::device_assign::assign_distinct_devices;
