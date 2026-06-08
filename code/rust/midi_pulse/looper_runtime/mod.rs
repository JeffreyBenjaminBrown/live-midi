use midi_pulse::config::Config;

#[allow(dead_code)]
mod sink;
mod device;
#[allow(dead_code)]
mod edo;
#[allow(dead_code)]
mod loop_store;

use midi_pulse::monome;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

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
  preflight_bind_grids(config)?;
  Ok(())
}

/// Phase 1 pre-flight: discover the grids, hand each config monome a distinct one
/// in config order (or fail loudly), then register and blank each. This is the
/// "we can actually connect two monomes" check; the full event loop comes next.
fn preflight_bind_grids(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
  let Some(first) = config.monomes.first() else {
    return Err("looper config needs monomes".into());
  };
  let size = first.select.size.unwrap_or([16, 16]);
  let needed = config.monomes.len();

  // Query serialosc on a socket bound to the first monome's listen port.
  let disco = UdpSocket::bind(("0.0.0.0", first.listen_port))
    .map_err(|e| format!("bind UDP :{}: {e}", first.listen_port))?;
  disco.set_read_timeout(Some(Duration::from_millis(100)))?;
  let devices = monome::discover_devices(&disco, first.listen_port);
  println!("looper: serialosc reported {} device(s).", devices.len());

  // Distinct-device assignment (errs loudly, naming what was seen).
  let assigned = device::assign_distinct_devices(&devices, size, needed)?;

  for (cfg, dev) in config.monomes.iter().zip(assigned.iter()) {
    let addr: SocketAddr = format!("127.0.0.1:{}", dev.port).parse()?;
    // Reuse the discovery socket for the first monome; bind fresh for the rest.
    let sock = if cfg.listen_port == first.listen_port {
      disco.try_clone()?
    } else {
      let s = UdpSocket::bind(("0.0.0.0", cfg.listen_port))
        .map_err(|e| format!("bind UDP :{}: {e}", cfg.listen_port))?;
      s.set_read_timeout(Some(Duration::from_millis(100)))?;
      s
    };
    monome::register(&sock, addr, &cfg.prefix, cfg.listen_port);
    monome::send_led_all(&sock, addr, &cfg.prefix, 0);
    println!(
      "  bound monome {:?} -> device id={:?} type={:?} port={} (prefix {:?})",
      cfg.id, dev.id, dev.type_name, dev.port, cfg.prefix,
    );
  }
  println!("looper: both grids bound and blanked. (Full event loop: later phases.)");
  Ok(())
}
