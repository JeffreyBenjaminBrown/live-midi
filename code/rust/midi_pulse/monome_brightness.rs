use std::time::Duration;

pub struct PulseBrightness {
  pub period: Duration,
  pub on_time: Duration,
}

impl PulseBrightness {
  pub const fn fraction(period_micros: u64, denominator: u64) -> Self {
    PulseBrightness {
      period: Duration::from_micros(period_micros),
      on_time: Duration::from_micros(period_micros / denominator),
    }
  }

  pub const fn one_sixteenth(period_micros: u64) -> Self {
    Self::fraction(period_micros, 16)
  }

  pub const fn one_thirty_second(period_micros: u64) -> Self {
    Self::fraction(period_micros, 32)
  }
}
