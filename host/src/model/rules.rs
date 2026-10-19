//! Правила гонки: чистые данные и вычисления старта/длительности.
//! Ничего про течение гонки (состояния, запись, журнал) — это сервис
//! `service::race`.

use std::time::{Duration, Instant};

/// Сигнал старта: случайная пауза в [min, max) между последним бипом
/// отсчёта и гудком. Единственный вариант — рандом (как в laps-bar/
/// laps-trainer): точный момент старта заранее неизвестен.
#[derive(Clone, Copy)]
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

#[derive(Clone, Copy)]
pub struct RaceRules {
    pub start_signal_delay: StartSignalDelay,
    /// Лимит длительности гонки. None — без лимита: гонка идёт, пока
    /// её не завершат (RACE/REC/EXIT).
    pub duration: Option<Duration>,
}

impl Default for RaceRules {
    fn default() -> Self {
        Self {
            start_signal_delay: Default::default(),
            duration: Some(Duration::from_secs(15)),
        }
    }
}

/// Число бипов отсчёта перед стартовой паузой.
pub const PRE_BEEP_SECS: u64 = 3;

/// Пауза для конкретного старта: равномерный выбор в [min_delay,
/// max_delay); вырожденный случай (min == max) — само значение.
pub fn sample_start_delay(signal: &StartSignalDelay) -> Duration {
    let min = signal.min();
    let span = signal.max().saturating_sub(min).as_millis() as u64;
    if span == 0 {
        return min;
    }
    min + Duration::from_millis(rand::random_range(0..span))
}

/// Момент фактического старта гонки: конец отсчёта + длина гудка
/// (счётчик стартует, когда гудок доиграл).
pub fn race_time_start(start: Instant, delay: Duration, buzzer_len: Duration) -> Instant {
    start + delay + buzzer_len
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
