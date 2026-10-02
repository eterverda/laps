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

/// Статистика кадровых задержек и длительностей фаз: num/min/avg/max/p90.
/// Два источника замеров:
///
/// - `on_frame(&Frame)` — латентность: от таймстемпа кадра до «сейчас"
///   (захват → диск, захват → экран);
/// - `on_process(Duration)` — длительность фазы (yuyv→rgba, rgba→jpeg,
///   jpeg→rgba), НЕ от начала кадра.
///
/// p90 — t-digest: память O(1) на длину сеанса. Пишут потоки-замерщики,
/// логает один раз при завершении владелец через snapshot().
pub struct FrameStats {
    num: u64,
    sum: Duration,
    min: Duration,
    max: Duration,
    p90: tdigest::TDigest,
}

impl Default for FrameStats {
    fn default() -> Self {
        Self {
            num: 0,
            sum: Duration::ZERO,
            min: Duration::MAX,
            max: Duration::ZERO,
            p90: tdigest::TDigest::new_with_size(100),
        }
    }
}

impl FrameStats {
    /// Штатная точка входа: задержка от таймстемпа кадра до «сейчас».
    pub fn on_frame(&mut self, frame: &crate::driver::camera::capture::Frame) {
        self.on_frame_at(frame, Instant::now());
    }

    /// Задержка от таймстемпа кадра до заданного момента (для тестов).
    pub fn on_frame_at(&mut self, frame: &crate::driver::camera::capture::Frame, now: Instant) {
        self.on_process(now.saturating_duration_since(frame.timestamp));
    }

    /// Длительность фазы обработки (декод/перекод) — НЕ латентность
    /// от начала кадра.
    pub fn on_process(&mut self, dur: Duration) {
        self.num += 1;
        self.sum += dur;
        self.min = self.min.min(dur);
        self.max = self.max.max(dur);
        self.p90.push(dur.as_secs_f64());
    }

    /// None, если замеров не было — лог не нужен. Снимок состояния:
    /// накопитель можно продолжать использовать. Возвращает inline-yaml
    /// объект «{min: 12ms, avg: 19.3ms, max: 87ms, p90: 31ms}» — `ms` —
    /// суффикс каждого значения, avg — с десятыми (форматной строкой,
    /// без serde) как непрозрачный impl Display.
    pub fn snapshot(&mut self) -> Option<impl std::fmt::Display> {
        if self.num == 0 {
            return None;
        }
        self.p90.flush();
        let quantile =
            |q: f64| Duration::from_secs_f64(self.p90.estimate_quantile(q).unwrap_or(0.0));
        let avg_ms = self.sum.as_secs_f64() * 1000.0 / self.num as f64;
        Some(format!(
            "{{num: {}, min: {}ms, avg: {:.1}ms, max: {}ms, p90: {}ms}}",
            self.num,
            self.min.as_millis(),
            avg_ms,
            self.max.as_millis(),
            quantile(0.9).as_millis(),
        ))
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

    mod frame_stats {
        use super::*;
        use crate::driver::camera::capture::{Frame, FrameData};
        use crate::driver::camera::fps::FrameStats;

        fn ms(n: u64) -> Duration {
            Duration::from_millis(n)
        }

        /// Кадр с заданным таймстемпом (контент для статистики не важен).
        fn frame_at(ts: Instant) -> Frame {
            Frame {
                timestamp: ts,
                data: FrameData::Rgba {
                    rgba: egui::ColorImage::new([1, 1], vec![egui::Color32::BLACK]),
                },
            }
        }

        fn render(stats: &mut FrameStats) -> String {
            stats.snapshot().map(|s| s.to_string()).unwrap_or_default()
        }

        #[test]
        fn empty_returns_none() {
            let mut stats = FrameStats::default();
            assert!(stats.snapshot().is_none());
        }

        #[test]
        fn single_record_all_equal() {
            let base = Instant::now();
            let mut stats = FrameStats::default();
            stats.on_frame_at(&frame_at(base), base + ms(42));
            assert_eq!(
                render(&mut stats),
                "{num: 1, min: 42ms, avg: 42.0ms, max: 42ms, p90: 42ms}"
            );
        }

        #[test]
        fn min_avg_max_track_values() {
            let base = Instant::now();
            let mut stats = FrameStats::default();
            for lat in [10, 20, 30, 40] {
                stats.on_frame_at(&frame_at(base), base + ms(lat));
            }
            // p90 на 4 замерах нестабилен — проверяем только min/avg/max.
            assert!(
                render(&mut stats).starts_with("{num: 4, min: 10ms, avg: 25.0ms, max: 40ms, p90: ")
            );
        }

        #[test]
        fn avg_has_tenths() {
            let base = Instant::now();
            let mut stats = FrameStats::default();
            stats.on_frame_at(&frame_at(base), base + ms(10));
            stats.on_frame_at(&frame_at(base), base + ms(11));
            assert!(render(&mut stats).contains("avg: 10.5ms"));
        }

        #[test]
        fn on_process_records_phase_duration() {
            // Длительность фазы — НЕ латентность от начала кадра:
            // on_process берёт готовый Duration и кладёт его как есть.
            let mut stats = FrameStats::default();
            stats.on_process(ms(4));
            stats.on_process(ms(8));
            let s = render(&mut stats);
            assert!(s.starts_with("{num: 2, min: 4ms, avg: 6.0ms, max: 8ms, p90: "));
        }

        #[test]
        fn p90_close_to_empirical_quantile() {
            let base = Instant::now();
            let mut stats = FrameStats::default();
            // 100 замеров 10..=109 мс: эмпирический p90 ≈ 99-100 мс.
            for i in 0..100 {
                stats.on_frame_at(&frame_at(base), base + ms(10 + i));
            }
            let p90 = render(&mut stats);
            let p90: u64 = p90
                .strip_prefix("{num: 100, min: 10ms, avg: 59.5ms, max: 109ms, p90: ")
                .unwrap()
                .trim_end_matches("ms}")
                .parse()
                .unwrap();
            assert!((95..=104).contains(&p90), "p90 out of tolerance: {p90}");
        }

        #[test]
        fn record_at_measures_until_given_now() {
            let base = Instant::now();
            let mut stats = FrameStats::default();
            stats.on_frame_at(&frame_at(base), base + ms(7));
            stats.on_frame_at(&frame_at(base), base + ms(9));
            assert!(
                render(&mut stats).starts_with("{num: 2, min: 7ms, avg: 8.0ms, max: 9ms, p90: ")
            );
        }

        #[test]
        fn snapshot_does_not_consume() {
            let base = Instant::now();
            let mut stats = FrameStats::default();
            stats.on_frame_at(&frame_at(base), base + ms(5));
            assert!(stats.snapshot().is_some());
            // Накопитель жив: продолжаем писать после снимка.
            stats.on_frame_at(&frame_at(base), base + ms(15));
            assert!(render(&mut stats).starts_with("{num: 2, min: 5ms,"));
            assert!(render(&mut stats).contains("max: 15ms"));
        }
    }
}
