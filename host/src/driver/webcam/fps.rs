//! Счётчик fps на скользящем окне таймстемпов. Хранит кадры за
//! последние `window` (окно сглаживания), fps = (N-1) интервалов / span
//! от первого до последнего кадра в окне. Первый отчёт — когда span
//! достигает `report_after` (1/4 с): на высоком fps это четверть секунды,
//! на низком — соответственно дольше. Пауза длиннее окна вытесняет старые кадры и
//! span падает ниже порога — fps() снова None, вызывающий код обычно
//! держит последнее известное значение. В тестах время подаётся снаружи
//! через on_frame_at.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub struct FpsCounter {
    report_after: Duration,
    window: Duration,
    frames: VecDeque<Instant>,
}

impl Default for FpsCounter {
    /// Стандартный счётчик: порог 1/4 с, окно 1 с.
    fn default() -> Self {
        Self::new(Duration::from_millis(250), Duration::from_secs(1))
    }
}

impl FpsCounter {
    pub fn new(report_after: Duration, window: Duration) -> Self {
        Self {
            report_after,
            window,
            frames: VecDeque::new(),
        }
    }

    /// Штатная точка входа: штамп «сейчас».
    pub fn on_frame(&mut self) {
        self.on_frame_at(Instant::now());
    }

    /// Штамп кадра в заданный момент (для тестов). Вытесняет кадры
    /// старше окна; один последний всегда остаётся.
    pub fn on_frame_at(&mut self, now: Instant) {
        self.frames.push_back(now);
        if let Some(cutoff) = now.checked_sub(self.window) {
            while self.frames.len() > 1 && *self.frames.front().unwrap() < cutoff {
                self.frames.pop_front();
            }
        }
    }

    /// fps по окну: (N-1) интервалов / span первого-последнего кадра.
    /// None, пока в окне меньше двух кадров или span не достиг
    /// `report_after`, и при вырожденном нулевом span.
    pub fn fps(&self) -> Option<u32> {
        if self.frames.len() < 2 {
            return None;
        }
        let first = *self.frames.front().unwrap();
        let last = *self.frames.back().unwrap();
        let span = last.saturating_duration_since(first);
        if span < self.report_after || span.is_zero() {
            return None;
        }
        Some(((self.frames.len() - 1) as f64 / span.as_secs_f64()).round() as u32)
    }

    /// Очистка: до следующего порога по `report_after` fps() снова None.
    pub fn reset(&mut self) {
        self.frames.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT: Duration = Duration::from_millis(250);
    const WINDOW: Duration = Duration::from_secs(1);

    fn at(base: Instant, us: u64) -> Instant {
        base + Duration::from_micros(us)
    }

    /// Гоняем steady-поток с шагом step_us, всего frames кадров.
    fn steady(base: Instant, step_us: u64, frames: u32) -> FpsCounter {
        let mut counter = FpsCounter::new(REPORT, WINDOW);
        for i in 0..frames {
            counter.on_frame_at(at(base, i as u64 * step_us));
        }
        counter
    }

    #[test]
    fn empty_returns_none() {
        let counter = FpsCounter::new(REPORT, WINDOW);
        assert_eq!(counter.fps(), None);
    }

    #[test]
    fn below_threshold_returns_none() {
        // 0.24 с ровного 30 fps — span ещё не достиг 0.25 с.
        let base = Instant::now();
        let mut counter = FpsCounter::new(REPORT, WINDOW);
        for i in 0..9 {
            counter.on_frame_at(at(base, i * 26_666));
        }
        assert_eq!(counter.fps(), None);
    }

    #[test]
    fn first_report_after_250ms() {
        let base = Instant::now();
        // ~0.27 с ровного 30 fps.
        let counter = steady(base, 33_333, 9);
        assert_eq!(counter.fps(), Some(30));
    }

    #[test]
    fn steady_60fps() {
        let base = Instant::now();
        let counter = steady(base, 16_666, 70); // ~1.1 с, окно заполнено
        assert_eq!(counter.fps(), Some(60));
    }

    #[test]
    fn window_caps_at_1s() {
        // Длинный ровный поток: окно вытесняет всё старше 1 с, fps считается
        // только по последней секунде — не по всей истории.
        let base = Instant::now();
        let counter = steady(base, 33_333, 300); // 10 с
        assert_eq!(counter.fps(), Some(30));
    }

    #[test]
    fn late_frame_dips_mildly_and_recovers() {
        let base = Instant::now();
        let mut counter = steady(base, 33_333, 40);

        // Пауза 200 мс: в окне секунды не хватает ~6 кадров из 30.
        counter.on_frame_at(at(base, 40 * 33_333 + 200_000));
        let dipped = counter.fps().unwrap();
        assert!((20..=27).contains(&dipped), "dipped to {dipped}");

        // Восстановление мгновенное: следующая секунда ровного потока
        // заполняет окно обратно.
        let resume = 40 * 33_333 + 200_000;
        for i in 1..=35 {
            counter.on_frame_at(at(base, resume + i * 33_333));
        }
        assert_eq!(counter.fps(), Some(30));
    }

    #[test]
    fn pause_longer_than_window_returns_none() {
        let base = Instant::now();
        let mut counter = steady(base, 33_333, 40);
        assert_eq!(counter.fps(), Some(30));

        // Пауза 2 с: всё окно вытеснено, остался один штамп.
        counter.on_frame_at(at(base, 40 * 33_333 + 2_000_000));
        assert_eq!(counter.fps(), None);
    }

    #[test]
    fn zero_interval_is_none_not_panic() {
        let base = Instant::now();
        let mut counter = FpsCounter::new(REPORT, WINDOW);
        for _ in 0..5 {
            counter.on_frame_at(base);
        }
        assert_eq!(counter.fps(), None);
    }

    #[test]
    fn reset_clears_measurements() {
        let base = Instant::now();
        let mut counter = steady(base, 33_333, 40);
        assert_eq!(counter.fps(), Some(30));

        counter.reset();
        assert_eq!(counter.fps(), None);

        // Порог снова по span: 8 интервалов по 33.3 мс = 0.23 с — None,
        // 9-й кадр доводит span до 0.27 с — отчёт.
        for i in 1..=8 {
            counter.on_frame_at(at(base, 2_000_000 + i * 33_333));
        }
        assert_eq!(counter.fps(), None);
        counter.on_frame_at(at(base, 2_000_000 + 9 * 33_333));
        assert_eq!(counter.fps(), Some(30));
    }
}
