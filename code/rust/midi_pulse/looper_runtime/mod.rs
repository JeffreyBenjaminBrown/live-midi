use midi_pulse::config::Config;

#[allow(dead_code)]
mod sink;
#[allow(dead_code)]
mod device;
#[allow(dead_code)]
mod edo;

/// Phase 0 stub. The config has already been validated by `parse_config`; here we
/// just print the resolved two-grid window inventory and exit cleanly. The real
/// two-monome runtime (shared state, the pitch-keyed NoteSink, the loop store)
/// arrives in later phases -- see 6_plan.org.
pub fn run_from_config(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
  println!(
    "looper: {} windows across {} monomes",
    config.monome_windows.len(),
    config.monomes.len(),
  );
  for monome in &config.monomes {
    println!(
      "  monome {:?} (port {}, prefix {:?}):",
      monome.id, monome.listen_port, monome.prefix,
    );
    for window in config.monome_windows.iter().filter(|w| w.monome() == monome.id) {
      println!(
        "    {:<22} id {:?} rect {:?}",
        window.kind_name(),
        window.id(),
        window.rect(),
      );
    }
  }
  if let Some(looper) = &config.looper {
    println!(
      "  [looper] quantize_record_ms={} cluster_display_ms={} flash_ms={} remap_center={:?}",
      looper.quantize_record_ms,
      looper.cluster_display_ms,
      looper.flash_ms,
      looper.remap_center,
    );
  }
  println!("looper: two-monome runtime not yet implemented (Phase 0); exiting cleanly.");
  Ok(())
}
