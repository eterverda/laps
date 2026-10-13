//! Головка воспроизведения: модель путешествия по времени записи.
//! Чистая логика — ни потоков, ни декодера, ни UI: позиция = base +
//! ∫скорость (скорость кусочная: дискретные команды мгновенны, жест
//! hold/release на перемотке — с рампой ease-out). Единственный
//! источник времени для всех: плееру головка говорит, какую позицию
//! показывать, трекерам (таймер, шкала) — сколько сейчас времени.
//!
//! Время внешнее: ядро `_at` принимает `now: Instant` — приватное,
//! тесты живут в этом же модуле и крутят часы сами: сценарии
//! проверяются мгновенно и с точностью до миллисекунды, без сна и
//! допусков. Снаружи видны только реалтайм-обёртки (Instant::now()
//! внутри) — worker плеера зовёт их.

use std::time::{Duration, Instant};

/// Рампа перемотки: не быстрее чем за это и не жёстче предельного
/// ускорения (x/с) на больших дистанциях.
const SPEED_RAMP_MIN: Duration = Duration::from_millis(1500);
const SPEED_RAMP_A_MAX: f64 = 20.0;

/// Профиль рампы: ease-out от from к to за dur (быстрый старт, мягкое
/// приземление). Непрерывен: по завершении головка перепривязывается к
/// позиции на тот момент, скачков нет.
struct Ramp {
    from: f64,
    to: f64,
    start: Instant,
    dur: Duration,
}

/// Активный жест hold/release на перемотке: resume — скорость, к
/// которой возвращаемся на отпускании (0 — шаттл из паузы, возврат
/// — мгновенный стоп). releasing — идёт возврат после отпускания.
struct Gesture {
    resume: f64,
    releasing: bool,
}

/// Головка воспроизведения. Все моменты времени — мс от начала записи.
pub struct Playhead {
    /// pts на base_at при base_speed; дальше позиция = base + ∫скорость.
    base_at: Instant,
    base_position: u64,
    base_speed: f64,
    ramp: Option<Ramp>,
    /// Часы идут (позиция движется). Не путать с «PLAY подсвечен»:
    /// жест из паузы тоже крутит время.
    running: bool,
    /// Скорость воспроизведения (скролл PLAY): память для старта и
    /// возврата после жестов. Эффективная скорость посередине жестовой
    /// рампы сюда не попадает — иначе скорость оседавала бы между
    /// жестами (утечка, которую ловили дважды).
    resume_speed: f64,
    gesture: Option<Gesture>,
    /// Захват таймлайна мышью/скроллом: время заморожено, позицию
    /// двигают дискретными seek'ами; это не пауза — running сохраняется.
    drag_held: bool,
    duration: u64,
}

impl Playhead {
    pub fn new(now: Instant, duration: u64) -> Self {
        Self {
            base_at: now,
            base_position: 0,
            base_speed: 0.0,
            ramp: None,
            running: false,
            resume_speed: 1.0,
            gesture: None,
            drag_held: false,
            duration,
        }
    }

    // Реалтайм-обёртки: то же ядро `_at` на Instant::now(). Рабочий
    // путь (worker плеера) зовёт эти; тесты — `_at` с фиктивными
    // часами: сценарии мгновенные и точные до миллисекунды.

    pub fn position(&self) -> u64 {
        self.position_at(Instant::now())
    }

    pub fn velocity(&self) -> f64 {
        self.velocity_at(Instant::now())
    }

    pub fn sync(&mut self) {
        self.sync_at(Instant::now());
    }

    pub fn play(&mut self) {
        self.play_at(Instant::now());
    }

    /// PAUSE: стоп на текущей позиции, жест обрывается.
    pub fn pause(&mut self) {
        self.pause_at(Instant::now());
    }

    pub fn set_speed(&mut self, speed: f64) {
        self.set_speed_at(Instant::now(), speed);
    }

    pub fn shuttle(&mut self, velocity: f64) {
        self.shuttle_at(Instant::now(), velocity);
    }

    pub fn shuttle_release(&mut self, dur: Duration) {
        self.shuttle_release_at(Instant::now(), dur);
    }

    pub fn seek(&mut self, position: u64) {
        self.seek_at(Instant::now(), position);
    }

    pub fn set_drag_held(&mut self, held: bool) {
        self.set_drag_held_at(Instant::now(), held);
    }

    /// Позиция на момент now (трекеры читают это каждый кадр).
    fn position_at(&self, now: Instant) -> u64 {
        self.eval_at(now).1
    }

    /// Эффективная скорость на момент now (worker считает дедлайны
    /// кадров; во время рампы — профильная, не скоммиченная).
    fn velocity_at(&self, now: Instant) -> f64 {
        self.eval_at(now).0
    }

    /// Часы идут (время бежит: play или жест перемотки).
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Жест hold/release, начатый из паузы: PLAY при нём тёмный.
    pub fn is_seeking(&self) -> bool {
        self.gesture.as_ref().is_some_and(|g| g.resume == 0.0)
    }

    /// Идёт рампа скорости (жест разгоняется/тормозит): worker капает
    /// дедлайны по RAMP_POLL, а не по дальности кадра.
    pub fn is_ramping(&self) -> bool {
        self.ramp.is_some()
    }

    /// Продвинуть время до now: коммитить завершившиеся рампы,
    /// останавливаться на границах файла. Worker зовёт раз за цикл
    /// (между циклами позиция движется сама — формула замкнута).
    fn sync_at(&mut self, now: Instant) {
        self.commit_at(now);
        if !self.running {
            return;
        }
        // Границы файла: вперёд — конец, назад — ноль, стоп-кадр.
        // Среди активной рампы не стопаемся: скорость ещё меняется,
        // пусть прижимается к границе (см. eval: clamp).
        if self.ramp.is_none() {
            let (velocity, position) = self.eval_at(now);
            if (velocity > 0.0 && position >= self.duration)
                || (velocity < 0.0 && position == 0)
            {
                self.pause_at(now);
            }
        }
    }

    /// PLAY: строго с resume_speed, не с эффективной скоростью часов
    /// (там может лежать скорость отменённого жеста, 10x).
    fn play_at(&mut self, now: Instant) {
        if !self.running {
            // Позицию считаем до running=true: стоящие часы паркованы
            // со своей скоростью и дотекли бы промежуток между
            // set_speed и play.
            let position = self.position_at(now);
            self.running = true;
            self.gesture = None;
            if !self.drag_held {
                self.start_at(now, position, self.resume_speed);
            } else {
                // Время заморожено: позиция как есть, интеграции нет.
                self.base_position = position;
                self.base_at = now;
                self.base_speed = 0.0;
            }
        }
    }

    /// Дискретная смена скорости воспроизведения (скролл над PLAY):
    /// реанкор от текущей позиции, жест отменяет. Ноль — пауза.
    fn set_speed_at(&mut self, now: Instant, speed: f64) {
        self.gesture = None;
        if speed == 0.0 {
            self.pause_at(now);
        } else {
            self.resume_speed = speed;
            if self.running {
                self.start_at(now, self.position_at(now), speed);
            } else {
                // Пауза: часы не идут, позиция = base, скорость —
                // память для следующего play.
                self.base_position = self.position_at(now);
                self.base_at = now;
                self.base_speed = speed;
                self.ramp = None;
            }
        }
    }

    /// Жест шаттла (скоростная прокрутка с разгоном): рампа к
    /// ±velocity от текущей эффективной скорости. Часы стояли —
    /// стартуются от текущей позиции с нуля, отпускание вернёт их
    /// в стоп.
    fn shuttle_at(&mut self, now: Instant, velocity: f64) {
        let from = self.eval_at(now).0;
        if self.gesture.is_none() {
            self.gesture = Some(Gesture {
                resume: if self.running { self.resume_speed } else { 0.0 },
                releasing: false,
            });
        } else if let Some(g) = &mut self.gesture {
            g.releasing = false;
        }
        self.running = true;
        let dur = std::cmp::max(
            SPEED_RAMP_MIN,
            Duration::from_secs_f64((velocity - from).abs() / SPEED_RAMP_A_MAX),
        );
        self.ramp_to_at(now, velocity, dur);
    }

    /// Отпускание шаттла: возврат к режиму до него. Из паузы —
    /// мгновенный стоп на позиции now (точный скраб: выкат не даст
    /// попасть в нужное место); из play — рампа назад за dur.
    fn shuttle_release_at(&mut self, now: Instant, dur: Duration) {
        let resume = self.gesture.as_mut().map(|g| {
            g.releasing = true;
            g.resume
        });
        if let Some(resume) = resume {
            if resume == 0.0 {
                self.pause_at(now);
            } else {
                self.ramp_to_at(now, resume, dur);
            }
        }
    }

    /// Перемотка на позицию (мс): жест отменяет. При игре время
    /// перепривязывается к ней с resume-скоростью; на паузе — стоящие
    /// часы. При зажатом таймлайне время не идёт: позиция просто
    /// переносится (scrab таскает seek'ами каждый кадр).
    fn seek_at(&mut self, now: Instant, position: u64) {
        self.gesture = None;
        let position = position.min(self.duration);
        if self.running && !self.drag_held {
            self.start_at(now, position, self.resume_speed);
        } else {
            self.base_position = position;
            self.base_at = now;
            self.base_speed = 0.0;
            self.ramp = None;
        }
    }

    /// Захват таймлайна: true — время заморожено (позицию двигают
    /// seek'ами), false — продолжить с замороженной позиции.
    fn set_drag_held_at(&mut self, now: Instant, held: bool) {
        if self.drag_held != held {
            self.drag_held = held;
            if held {
                self.base_position = self.position_at(now);
                self.base_at = now;
                self.base_speed = 0.0;
                self.ramp = None;
            } else if self.running {
                self.start_at(now, self.base_position, self.resume_speed);
            }
        }
    }

    // --- Внутренности часов ------------------------------------------------

    /// Старт/перезапуск от позиции со скоростью (рампу снимает).
    fn start_at(&mut self, now: Instant, base: u64, velocity: f64) {
        self.base_at = now;
        self.base_position = base;
        self.base_speed = velocity;
        self.ramp = None;
        self.running = velocity != 0.0;
    }

    /// Стоп на позиции now: позиция остаётся в base для возможного
    /// старта от неё, жест обрывается.
    fn pause_at(&mut self, now: Instant) {
        let (_, position) = self.eval_at(now);
        self.base_position = position;
        self.base_at = now;
        self.base_speed = 0.0;
        self.ramp = None;
        self.running = false;
        self.gesture = None;
    }

    /// (скорость, позиция) на момент now, без коммита. Скорость
    /// знаковая: 1D-velocity вдоль оси записи (минус = реверс). Позиция
    /// — интеграл скорости, не elapsed × текущая: иначе профиль рампы
    /// перелетает или недолетает цель.
    fn eval_at(&self, now: Instant) -> (f64, u64) {
        if !self.running {
            return (0.0, self.base_position);
        }
        let elapsed_ms = now.saturating_duration_since(self.base_at).as_millis() as f64;
        let sample = match &self.ramp {
            None => (
                self.base_speed,
                (self.base_position as f64 + self.base_speed * elapsed_ms) as u64,
            ),
            Some(r) => {
                let u = (now.saturating_duration_since(r.start).as_millis() as f64
                    / r.dur.as_millis() as f64)
                    .min(1.0);
                // ∫f, f = 2u − u² (ease-out): T·(u² − u³/3), всё в мс.
                let span = r.from * elapsed_ms
                    + (r.to - r.from) * r.dur.as_millis() as f64 * (u * u - u * u * u / 3.0);
                (
                    r.from + (r.to - r.from) * (2.0 * u - u * u),
                    (self.base_position as f64 + span) as u64,
                )
            }
        };
        (sample.0, sample.1.min(self.duration))
    }

    /// Завершившуюся рампу зафиксировать: скорость = to, головка
    /// перепривязывается к позиции на момент завершения. to == 0 —
    /// остановка; завершившаяся release-рампа закрывает жест.
    fn commit_at(&mut self, now: Instant) {
        let Some(r) = &self.ramp else {
            return;
        };
        if now.saturating_duration_since(r.start) < r.dur {
            return;
        }
        let to = r.to;
        let releasing = self.gesture.as_ref().is_some_and(|g| g.releasing);
        let pos = self.eval_at(now).1;
        self.base_speed = to;
        self.ramp = None;
        self.base_position = pos;
        self.base_at = now;
        self.running = to != 0.0;
        if !self.running || releasing {
            self.gesture = None;
        }
    }

    /// Рампа к цели за dur, от текущей эффективной скорости.
    fn ramp_to_at(&mut self, now: Instant, to: f64, dur: Duration) {
        let (from, pos) = self.eval_at(now);
        self.base_position = pos;
        self.base_at = now;
        self.base_speed = 0.0;
        self.running = true;
        self.ramp = Some(Ramp {
            from,
            to,
            start: now,
            dur,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Длительность для сценариев: пробеги в секунды никогда не
    /// упираются в границы.
    const DUR: u64 = 600_000;

    /// Фиктивные часы: старт + ручной шаг. Мгновенно и с точностью
    /// до миллисекунды — без сна и допусков.
    struct FakeClock {
        now: Instant,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                now: Instant::now(),
            }
        }
        /// Шаг на ms миллисекунд, возвращает новый момент.
        fn advance(&mut self, ms: u64) -> Instant {
            self.now += Duration::from_millis(ms);
            self.now
        }
    }

    #[test]
    fn linear_run_and_pause_freezes() {
        let mut clock = FakeClock::new();
        let t0 = clock.now;
        let mut p = Playhead::new(t0, DUR);
        p.play_at(t0);
        assert!(p.is_running());
        // 300 мс × 1x = 300 мс, ровно.
        assert_eq!(p.position_at(clock.advance(300)), 300);
        p.pause_at(clock.now);
        assert!(!p.is_running());
        // Стоп замораживает: сколько ни жди — позиция та же.
        assert_eq!(p.position_at(clock.advance(1000)), 300);
        // Продолжение с той же позиции.
        p.play_at(clock.now);
        assert_eq!(p.position_at(clock.advance(200)), 500);
    }

    #[test]
    fn ramp_lands_continuously_at_exact_speed() {
        let mut clock = FakeClock::new();
        let t0 = clock.now;
        let mut p = Playhead::new(t0, DUR);
        p.play_at(t0);
        clock.advance(300);
        p.shuttle_at(clock.now, 10.0);
        // Рампа hold: минимум 1500 мс.
        let t_land = clock.advance(1700);
        p.sync_at(t_land);
        assert!(!p.is_ramping());
        // Приземление бесшовно: позиция на стыке = интеграл, без скачка.
        let at_land = p.position_at(t_land);
        clock.advance(300);
        // Полный ход после приземления: ровно 10x.
        assert_eq!(p.position_at(clock.now), at_land + 3000);
    }

    #[test]
    fn release_from_pause_stops_instantly_on_frame() {
        let mut clock = FakeClock::new();
        let t0 = clock.now;
        let mut p = Playhead::new(t0, DUR);
        p.seek_at(t0, 1000);
        p.shuttle_at(clock.now, 5.0);
        clock.advance(500);
        assert!(p.is_running() && p.is_seeking());
        let at = p.position_at(clock.now);
        p.shuttle_release_at(clock.now, Duration::from_millis(500));
        assert!(!p.is_running());
        // Мгновенный стоп: сколько ни жди — позиция на моменте отпускания.
        assert_eq!(p.position_at(clock.advance(1000)), at);
    }

    #[test]
    fn release_during_play_ramps_back_to_play_speed() {
        let mut clock = FakeClock::new();
        let t0 = clock.now;
        let mut p = Playhead::new(t0, DUR);
        p.play_at(t0);
        clock.advance(300);
        p.shuttle_at(clock.now, -10.0);
        clock.advance(500);
        p.shuttle_release_at(clock.now, Duration::from_millis(500));
        assert!(p.is_running());
        // Возврат к resume-скорости 1x: рампа 500 мс, затем ровный ход.
        let t_land = clock.advance(600);
        p.sync_at(t_land);
        assert!(!p.is_ramping());
        let at_land = p.position_at(t_land);
        clock.advance(300);
        assert_eq!(p.position_at(clock.now), at_land + 300);
    }

    #[test]
    fn reversal_ramp_passes_through_zero_without_sticking() {
        let mut clock = FakeClock::new();
        let t0 = clock.now;
        let mut p = Playhead::new(t0, DUR);
        // Стартуем подальше от нуля: профиль рампы от 1x к −5x за
        // 1500 мс интегрально уходит ниже стартовой позиции — при
        // старте с 2000 мс упёрся бы в границу (стоп, не залипание).
        p.seek_at(t0, 20_000);
        p.play_at(t0);
        clock.advance(2000);
        let before = p.position_at(clock.now);
        p.shuttle_at(clock.now, -5.0);
        // Рампа через ноль: позиция не уходит в минус и не залипает.
        clock.advance(3000);
        p.sync_at(clock.now);
        assert!(p.position_at(clock.now) < before, "разворот не пошёл назад");
        assert_eq!(p.velocity_at(clock.now), -5.0);
    }

    #[test]
    fn set_speed_reanchors_without_jump() {
        let mut clock = FakeClock::new();
        let t0 = clock.now;
        let mut p = Playhead::new(t0, DUR);
        p.play_at(t0);
        clock.advance(200);
        p.set_speed_at(clock.now, 2.0);
        // Бесшовно: 2x с момента смены.
        assert_eq!(p.position_at(clock.advance(200)), 600);
        // Скорость на паузе (park) не двигает время, помнится для play.
        let mut q = Playhead::new(t0, DUR);
        q.set_speed_at(t0, 0.5);
        assert!(!q.is_running());
        assert_eq!(q.position_at(clock.advance(100)), 0);
        let t_play = clock.now;
        q.play_at(t_play);
        assert_eq!(q.position_at(clock.advance(200)), 100);
        assert_eq!(q.resume_speed, 0.5);
    }

    #[test]
    fn hold_from_stop_starts_at_base() {
        let mut clock = FakeClock::new();
        let t0 = clock.now;
        let mut p = Playhead::new(t0, DUR);
        p.seek_at(t0, 1000);
        p.shuttle_at(clock.now, 5.0);
        assert!(p.is_running());
        clock.advance(200);
        assert!(
            p.position_at(clock.now) >= 1000,
            "старт hold не отмотал назад"
        );
    }

    #[test]
    fn seek_during_play_continues_at_play_speed() {
        let mut clock = FakeClock::new();
        let t0 = clock.now;
        let mut p = Playhead::new(t0, DUR);
        p.play_at(t0);
        clock.advance(100);
        // Жест перемотки внес бы эффективную скорость в базу — seek
        // обязан продолжить именно с play-скорости.
        p.shuttle_at(clock.now, 10.0);
        clock.advance(100);
        p.seek_at(clock.now, 5000);
        assert_eq!(p.position_at(clock.advance(100)), 5100);
    }

    #[test]
    fn edges_stop_playback() {
        let mut clock = FakeClock::new();
        let t0 = clock.now;
        let mut p = Playhead::new(t0, 400);
        p.play_at(t0);
        clock.advance(600);
        p.sync_at(clock.now);
        assert!(!p.is_running());
        assert_eq!(p.position_at(clock.now), 400);
        // Назад до нуля.
        p.shuttle_at(clock.now, -10.0);
        clock.advance(3000);
        p.sync_at(clock.now);
        assert!(!p.is_running());
        assert_eq!(p.position_at(clock.now), 0);
    }

    #[test]
    fn drag_held_freezes_and_resumes_from_same_position() {
        let mut clock = FakeClock::new();
        let t0 = clock.now;
        let mut p = Playhead::new(t0, DUR);
        p.play_at(t0);
        clock.advance(1000);
        p.set_drag_held_at(clock.now, true);
        clock.advance(500);
        // Время заморожено, хотя running сохраняется.
        assert!(p.is_running());
        assert_eq!(p.position_at(clock.now), 1000);
        // Seek'и таскают позицию.
        p.seek_at(clock.now, 3000);
        assert_eq!(p.position_at(clock.advance(500)), 3000);
        // Отпускание продолжает с замороженной позиции.
        let t_free = clock.now;
        p.set_drag_held_at(t_free, false);
        assert_eq!(p.position_at(clock.advance(100)), 3100);
    }
}
