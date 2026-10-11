use std::time::Duration;

/// Сигнал старта: случайная пауза в [min, max) между последним бипом
/// отсчёта и гудком. Единственный вариант — рандом (как в laps-bar/
/// laps-trainer): точный момент старта заранее неизвестен.
pub struct StartSignalDelay {
    min: Duration,
    max: Duration,
}

impl StartSignalDelay {
    pub const fn random(min: Duration, max: Duration) -> Self {
        assert!(min.as_nanos() <= max.as_nanos(), "start signal: min > max");
        Self { min, max }
    }

    pub fn min(&self) -> Duration {
        self.min
    }

    pub fn max(&self) -> Duration {
        self.max
    }
}

impl Default for StartSignalDelay {
    fn default() -> Self {
        StartSignalDelay::random(Duration::from_secs(1), Duration::from_secs(3))
    }
}

pub struct RaceRules {
    pub start_signal_delay: StartSignalDelay,
    pub duration: Duration,
}

impl Default for RaceRules {
    fn default() -> Self {
        RaceRules {
            duration: Duration::from_secs(15),
            start_signal_delay: Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Конструктор держит границы; вырожденный диапазон min == max
    /// отдаёт само значение.
    #[test]
    fn delay_bounds() {
        let random = StartSignalDelay::random(Duration::from_secs(2), Duration::from_secs(4));
        assert_eq!(random.min(), Duration::from_secs(2));
        assert_eq!(random.max(), Duration::from_secs(4));

        let degenerate = StartSignalDelay::random(Duration::from_secs(7), Duration::from_secs(7));
        assert_eq!(degenerate.min(), Duration::from_secs(7));
        assert_eq!(degenerate.max(), Duration::from_secs(7));

        assert_eq!(StartSignalDelay::default().min(), Duration::from_secs(1));
    }
}
