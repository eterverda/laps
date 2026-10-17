//! Транспорт DVR: общая модель путешествия по времени сессии записи
//! (REC on → REC off). Чистая логика — ни потоков, ни декодера, ни UI:
//! позиция = base + ∫скорость (скорость кусочная: дискретные команды
//! мгновенны, шаттл — с рампой ease-out). Живёт у экрана (Flight),
//! владеет навигацией по полоске; плееры — доставщики кадров, worker'ы
//! экстраполируют от последнего синка на своих часах.
//!
//! Время внешнее: ядро `_at` принимает `now: Instant` — приватное,
//! тесты живут в этом же модуле и крутят часы сами: сценарии
//! проверяются мгновенно и с точностью до миллисекунды, без сна и
//! допусков. Снаружи видны только реалтайм-обёртки (Instant::now()
//! внутри).

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

/// Активный жест шаттла: resume — скорость, к
/// которой возвращаемся на отпускании (0 — шаттл из паузы, возврат
/// — мгновенный стоп). releasing — идёт возврат после отпускания.
struct Gesture {
    resume: f64,
    releasing: bool,
}

/// Круг зацикливания: центр и радиус (мс) — в ВРЕМЕНИ ВИДЕО, не
/// в реальном: круг ±1.5 с покрывает 3 с записи при любой скорости
/// плеера, реальная длительность кольца масштабируется часами
/// (на 200% круг 3 с проигрывается за 1.5 с, на 25% — за 12 с).
/// Центр запоминается при включении и следует за позицией при любой
/// перемотке (seek, шаттл). Границы кольца прижимаются к границам
/// записи (у края файла «отскакивает чуть раньше»).
#[derive(Clone, Copy)]
pub struct Circle {
    pub center: u64,
    pub radius: u64,
}

/// Транспорт: все моменты времени — мс по шкале сессии записи.
pub struct Transport {
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
    /// рампы сюда не попадает — только коммиченная.
    resume_speed: f64,
    gesture: Option<Gesture>,
    /// Захват таймлайна мышью/скроллом: время заморожено, позицию
    /// двигают дискретными seek'ами; это не пауза — running сохраняется.
    drag_held: bool,
    /// Круг зацикливания: заворот позиции по кольцу. Работает только
    /// в чистом play (без шаттла, драга и рампы) — в остальных
    /// режимах позиция свободна, круг не отменяется.
    circle: Option<Circle>,
    duration: u64,
}

impl Transport {
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
            circle: None,
            duration,
        }
    }

    // Реалтайм-обёртки: то же ядро `_at` на Instant::now(). Рабочий
    // путь (экран, синки в плееры) зовёт эти; тесты — `_at` с
    // фиктивными часами: сценарии мгновенные и точные до миллисекунды.

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

    /// Зацикливание: Some(круг) включает, None выключает.
    pub fn set_circle(&mut self, circle: Option<Circle>) {
        self.set_circle_at(Instant::now(), circle);
    }

    /// Параметры круга (для индикатора на таймлайне).
    pub fn circle(&self) -> Option<Circle> {
        self.circle
    }

    /// Позиция на момент now (циферблат и синки читают это каждый кадр).
    fn position_at(&self, now: Instant) -> u64 {
        self.eval_at(now).1
    }

    /// Эффективная скорость на момент now (плееры считают дедлайны
    /// кадров; во время рампы — профильная, не скоммиченная).
    fn velocity_at(&self, now: Instant) -> f64 {
        self.eval_at(now).0
    }

    /// Часы идут (время бежит: play или жест перемотки).
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Шаттл, начатый из паузы: PLAY при нём тёмный.
    pub fn is_seeking(&self) -> bool {
        self.gesture.as_ref().is_some_and(|g| g.resume == 0.0)
    }

    /// Продвинуть время до now: коммитить завершившиеся рампы,
    /// останавливаться на границах шкалы. Зовут раз за цикл чтения
    /// (между циклами позиция движется сама — формула замкнута).
    fn sync_at(&mut self, now: Instant) {
        self.commit_at(now);
        if !self.running {
            return;
        }
        // Круг таскается целиком за перемоткой: пока идёт шаттл или
        // рампа, центр следует за позицией (чистый play держит центр).
        if (self.gesture.is_some() || self.ramp.is_some()) && !self.drag_held {
            let position = self.position_at(now);
            if let Some(circle) = &mut self.circle {
                circle.center = position;
            }
        }
        // Заворот: интеграл перепривязываем к завёрнутой позиции —
        // при выключении круга время продолжается с позиции курсора,
        // а не с накопленного «сырого» интеграла за кольца.
        if self.circle.is_some() && self.ramp.is_none() && self.gesture.is_none() && !self.drag_held
        {
            self.base_position = self.position_at(now);
            self.base_at = now;
        }
        // Границы файла: вперёд — конец, назад — ноль, стоп-кадр.
        // Среди активной рампы не стопаемся: скорость ещё меняется,
        // пусть прижимается к границе (см. eval: clamp).
        if self.ramp.is_none() {
            let (velocity, position) = self.eval_at(now);
            if (velocity > 0.0 && position >= self.duration) || (velocity < 0.0 && position == 0) {
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
        // Перемотка смещает центр круга.
        let position = self.position_at(now);
        if let Some(circle) = &mut self.circle {
            circle.center = position;
        }
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
        if let Some(circle) = &mut self.circle {
            // Перемотка смещает центр круга.
            circle.center = position;
        }
        if self.running && !self.drag_held {
            self.start_at(now, position, self.resume_speed);
        } else {
            self.base_position = position;
            self.base_at = now;
            self.base_speed = 0.0;
            self.ramp = None;
        }
    }

    /// Зацикливание: Some(круг) включает (центр — позиция now),
    /// None выключает.
    fn set_circle_at(&mut self, now: Instant, circle: Option<Circle>) {
        self.circle = circle.map(|c| Circle {
            center: self.position_at(now),
            radius: c.radius,
        });
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
        let position = sample.1.min(self.duration);
        (sample.0, self.circle_wrap(position))
    }

    /// Заворот позиции по кольцу круга. Работает только в чистом play:
    /// без рампы, жеста и драга — шаттл и скраб ходят свободно, круг
    /// ждёт их завершения. Границы кольца — center ± radius (времени
    /// видео), прижатые к записи (у края файла круг «отскакивает
    /// раньше»).
    fn circle_wrap(&self, position: u64) -> u64 {
        let Some(circle) = &self.circle else {
            return position;
        };
        if !self.running || self.ramp.is_some() || self.gesture.is_some() || self.drag_held {
            return position;
        }
        let lo = circle.center.saturating_sub(circle.radius);
        let hi = (circle.center + circle.radius).min(self.duration);
        if hi <= lo {
            return position;
        }
        let span = (hi - lo) as i64;
        lo + (position as i64 - lo as i64).rem_euclid(span) as u64
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
            if let Some(circle) = &mut self.circle {
                // Шаттл кончился: центр — там, где остановились.
                circle.center = self.base_position;
            }
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
        let mut p = Transport::new(t0, DUR);
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
        let mut p = Transport::new(t0, DUR);
        p.play_at(t0);
        clock.advance(300);
        p.shuttle_at(clock.now, 10.0);
        // Рампа hold: минимум 1500 мс.
        let t_land = clock.advance(1700);
        p.sync_at(t_land);
        assert_eq!(p.velocity_at(clock.now), 10.0, "рампа приземлилась");
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
        let mut p = Transport::new(t0, DUR);
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
        let mut p = Transport::new(t0, DUR);
        p.play_at(t0);
        clock.advance(300);
        p.shuttle_at(clock.now, -10.0);
        clock.advance(500);
        p.shuttle_release_at(clock.now, Duration::from_millis(500));
        assert!(p.is_running());
        // Возврат к resume-скорости 1x: рампа 500 мс, затем ровный ход.
        let t_land = clock.advance(600);
        p.sync_at(t_land);
        assert_eq!(p.velocity_at(clock.now), 1.0, "рампа вернулась к 1x");
        let at_land = p.position_at(t_land);
        clock.advance(300);
        assert_eq!(p.position_at(clock.now), at_land + 300);
    }

    #[test]
    fn reversal_ramp_passes_through_zero_without_sticking() {
        let mut clock = FakeClock::new();
        let t0 = clock.now;
        let mut p = Transport::new(t0, DUR);
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
        let mut p = Transport::new(t0, DUR);
        p.play_at(t0);
        clock.advance(200);
        p.set_speed_at(clock.now, 2.0);
        // Бесшовно: 2x с момента смены.
        assert_eq!(p.position_at(clock.advance(200)), 600);
        // Скорость на паузе (park) не двигает время, помнится для play.
        let mut q = Transport::new(t0, DUR);
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
        let mut p = Transport::new(t0, DUR);
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
        let mut p = Transport::new(t0, DUR);
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
        let mut p = Transport::new(t0, 400);
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
    fn circle_wraps_forward_between_edges() {
        let mut clock = FakeClock::new();
        let t0 = clock.now;
        let mut p = Transport::new(t0, DUR);
        p.seek_at(t0, 10_000);
        // Зона ±1 с на 1x: [9000, 11000). Центр — позиция включения.
        p.set_circle_at(
            t0,
            Some(Circle {
                center: 0,
                radius: 1000,
            }),
        );
        p.play_at(t0);
        assert_eq!(p.position_at(clock.advance(500)), 10_500);
        // +1500: сыграли за + край [9000, 11000) → заворот на − край.
        assert_eq!(p.position_at(clock.advance(1000)), 9_500);
        assert_eq!(p.position_at(clock.advance(500)), 10_000);
    }

    #[test]
    fn circle_center_follows_seek_and_shuttle() {
        let mut clock = FakeClock::new();
        let t0 = clock.now;
        let mut p = Transport::new(t0, DUR);
        p.set_circle_at(
            t0,
            Some(Circle {
                center: 0,
                radius: 1000,
            }),
        );
        p.play_at(t0);
        clock.advance(300);
        p.seek_at(clock.now, 20_000);
        // Центр сместился: новое кольцо [19000, 21000).
        assert_eq!(p.position_at(clock.advance(500)), 20_500);
        // +500: ровно + край — заворот на − край.
        assert_eq!(p.position_at(clock.advance(500)), 19_000);
        // Шаттл: позиция свободна (без заворота)…
        p.shuttle_at(clock.now, 5.0);
        clock.advance(2000);
        assert!(p.position_at(clock.now) > 25_000, "шаттл зажат кругом");
        // …и по завершении центр — там, где остановились.
        p.shuttle_release_at(clock.now, Duration::from_millis(500));
        clock.advance(700);
        p.sync_at(clock.now);
        let center = p.position_at(clock.now);
        // Часовой ход ровно в 30 колец: позиция на центре.
        assert_eq!(p.position_at(clock.advance(60_000)), center);
    }

    #[test]
    fn circle_clamps_to_file_edges() {
        let mut clock = FakeClock::new();
        let t0 = clock.now;
        // Файл 20 с, круг у самого края: «отскакивает чуть раньше».
        let mut p = Transport::new(t0, 20_000);
        p.seek_at(t0, 19_000);
        p.set_circle_at(
            t0,
            Some(Circle {
                center: 0,
                radius: 2000,
            }),
        );
        p.play_at(t0);
        // Кольцо [17000, 20000]: 2 с игры пересекает и его край, и
        // конец файла (сыграли бы до 21000) — завернулось на − край.
        assert_eq!(p.position_at(clock.advance(2000)), 17_000);
    }

    #[test]
    fn circle_covers_video_time_regardless_of_play_speed() {
        let mut clock = FakeClock::new();
        let t0 = clock.now;
        let mut p = Transport::new(t0, DUR);
        p.seek_at(t0, 10_000);
        // Круг — во времени видео: на 2x круг 3 с контента проигрывается
        // за 1.5 с реального времени.
        p.set_speed_at(t0, 2.0);
        p.set_circle_at(
            t0,
            Some(Circle {
                center: 0,
                radius: 1500,
            }),
        );
        p.play_at(t0);
        // Кольцо [8500, 11500): 1.5 с игры на 2x = 3 с контента = круг.
        assert_eq!(p.position_at(clock.advance(1500)), 10_000);
        // На 25% круг проигрывается за 12 с реального времени.
        // Свои часы: сценарий независим.
        let mut clock2 = FakeClock::new();
        let t1 = clock2.now;
        let mut q = Transport::new(t1, DUR);
        q.seek_at(t1, 10_000);
        q.set_speed_at(t1, 0.25);
        q.set_circle_at(
            t1,
            Some(Circle {
                center: 0,
                radius: 1500,
            }),
        );
        q.play_at(t1);
        assert_eq!(q.position_at(clock2.advance(12_000)), 10_000);
    }

    #[test]
    fn disabling_circle_continues_from_cursor_position() {
        let mut clock = FakeClock::new();
        let t0 = clock.now;
        let mut p = Transport::new(t0, DUR);
        p.seek_at(t0, 10_000);
        p.set_circle_at(
            t0,
            Some(Circle {
                center: 0,
                radius: 1000,
            }),
        );
        p.play_at(t0);
        // Три кольца: «сырой» интеграл уехал бы на +9 с за пределы.
        let t = clock.advance(9000);
        p.sync_at(t);
        let at_cursor = p.position_at(t);
        p.set_circle_at(t, None);
        // Продолжаем с позиции курсора: +300 мс = +300 мс, без скачка.
        assert_eq!(p.position_at(clock.advance(300)), at_cursor + 300);
    }

    #[test]
    fn drag_held_freezes_and_resumes_from_same_position() {
        let mut clock = FakeClock::new();
        let t0 = clock.now;
        let mut p = Transport::new(t0, DUR);
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
