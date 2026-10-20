//! Гоночный сервис: ядро течения гонки (`RaceCore` — состояния, политика
//! записи, журнал) и его поток-обёртка: исполнение эффектов (команды
//! рекордерам, звук), поллинг first_frame_at, трансляция состояния в
//! GUI. Правила гонки (сигнал старта, отсчёт, длительность) — в
//! `model::rules`. Модуль: `service::race`.

use crate::config::camera::{CameraConfig, ContainerConfig as CameraConfigContainer};
use crate::config::setup::PadConfig;
use crate::driver::audio::{Audio, Sound};
use crate::driver::camera::Recorder;
use crate::driver::dvr::{RecordParams, RecordState};
use crate::driver::keyboard::KeyPressed;
use crate::model::flight::{Journal, JournalRecord};
use crate::model::pilot::Pilot;
use crate::model::rules::{PRE_BEEP_SECS, RaceRules, race_time_start, sample_start_delay};
use crossbeam_channel as crossbeam;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Интервал поллинга record_state (first_frame_at) и крышка сна
/// recv_timeout: события первых кадров заметны не позже этого.
const POLL: Duration = Duration::from_millis(30);

// --- Ядро гонки --------------------------------------------------------------

/// Намерения в ядро (GUI-кнопки, выход).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    RecPressed,
    RacePressed,
    Exit,
}

/// Эффекты из ядра: решения, которые исполняет поток-обёртка (команды
/// рекордерам, звук). Ядро чистое — никакого IO.
#[derive(Debug)]
pub enum Effect {
    /// Старт записи: (camera_id, готовые параметры) на камеру.
    Record {
        sessions: Vec<(String, RecordParams)>,
    },
    /// Останов записи всех камер; commit=false — файлы удалить (abort).
    StopRecording {
        commit: bool,
    },
    /// Старт отсчёта: звуковая программа длиной delay.
    Countdown {
        delay: Duration,
    },
    Buzzer,
    CancelAudio,
}

/// Фазы эфира — единственная машина состояний домена (снапшот для GUI —
/// сам же enum). Данные живут в вариантах; `Countdown.start` — момент
/// старта гонки (старт отсчёта + задержка + гудок), считается при входе —
/// GUI длину гудка не знает. DVR (экран) живёт в GUI. Жизнь ядра —
/// одна сессия REC: Before (REC, гонки нет) → Countdown → Running →
/// After → Finished; отмена до старта (REC-off из Before/Countdown) —
/// тоже Finished, но без гонки (ядро дропается, GUI в Idle).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RaceStage {
    /// REC включена, гонка не началась (первая запись — ядро рождается
    /// первым намерением сразу в этой фазе или в Countdown).
    Before,
    Countdown {
        start_at: Instant,
    },
    Running {
        start_at: Instant,
    },
    After {
        race: Duration,
    },
    /// Гонка закрыта, журнал передан: GUI переходит в DVR.
    Finished,
}

impl RaceStage {
    /// REC активна (индикатор/цвет кнопки).
    pub fn rec_on(&self) -> bool {
        matches!(
            self,
            RaceStage::Before
                | RaceStage::Countdown { .. }
                | RaceStage::Running { .. }
                | RaceStage::After { .. }
        )
    }

    /// Идёт отсчёт (мигание счётчика).
    pub fn countdown_active(&self) -> bool {
        matches!(self, RaceStage::Countdown { .. })
    }

    /// Время гонки для счётчика: бежит в Race, заморожено после.
    pub fn race_elapsed(&self, now: Instant) -> Duration {
        match *self {
            RaceStage::After { race } => race,
            RaceStage::Running { start_at } => now.saturating_duration_since(start_at),
            _ => Duration::ZERO,
        }
    }

    /// REC кликабельна: с эфира до конца хвоста.
    pub fn rec_available(&self) -> bool {
        !matches!(self, RaceStage::Finished)
    }

    /// RACE кликабельна: нажатие до старта, отжатие в Countdown/Race.
    pub fn race_available(&self) -> bool {
        matches!(
            self,
            RaceStage::Before | RaceStage::Countdown { .. } | RaceStage::Running { .. }
        )
    }

    /// Гонка началась (или дальше): записи относятся к ней.
    pub fn race_started(&self) -> bool {
        matches!(
            self,
            RaceStage::Running { .. } | RaceStage::After { .. } | RaceStage::Finished
        )
    }

    pub fn finished(&self) -> bool {
        matches!(self, RaceStage::Finished)
    }
}

/// Гоночное ядро: часы гонки, политика записи, журнал. Чистое — решения
/// возвращаются эффектами, IO исполняет поток-обёртка. Ядро — единственный
/// писатель журнала; порядок «журнал раньше уведомления GUI» обеспечивает
/// обёртка (эффекты применяются после записи в журнал).
struct RaceCore {
    phase: RaceStage,
    rules: RaceRules,
    journal: Journal,
    captures_dir: PathBuf,
    /// Ноль меток журнала: мс события = at − started. Журнал метки
    /// только хранит — считает производитель (это ядро).
    started: Instant,
    /// Длина гудка: старт гонки — конец отсчёта + гудок (звук —
    /// презентация, но часы гонки завязаны на его длительность).
    buzzer_len: Duration,
    /// camera_id → конфиг (стемы, расширения файлов записи).
    cameras: HashMap<String, CameraConfig>,
    /// Имена файлов открытых сессий по камерам (FIFO: сессия привязывается
    /// к первому кадру по порядку стартов).
    rec_pending: HashMap<String, VecDeque<String>>,
    /// Отложенные события (REC-on и VideoAttached головы до старта
    /// гонки): пишутся в журнал в момент старта гонки, при отмене —
    /// выбрасываются.
    rec_deferred: Vec<(u64, JournalRecord)>,
    /// Последнее известное first_frame_at по камерам (edge-детект).
    rec_seen: HashMap<String, Option<Instant>>,
    /// Гонка состоялась (Running или дальше): отличает финиш после гонки
    /// (журнал отдаём в DVR) от отмены записи до старта (ядро дропается,
    /// GUI возвращается в Idle).
    raced: bool,
}

impl RaceCore {
    /// Конструктор = REC нажали: ядро рождается с записью (Before) —
    /// журнал, ноль меток, сессии записи и отложенный RecordStarted
    /// стартуют от этого момента (файлы называются временем первой
    /// записи, а не запуска экрана). Первое действие RACE — обычный
    /// race_pressed поверх new(). Жизнь ядра — одна сессия REC.
    pub fn new(
        now: Instant,
        rules: RaceRules,
        captures_dir: PathBuf,
        buzzer_len: Duration,
        pads: BTreeMap<String, PadConfig>,
        assignments: HashMap<String, Pilot>,
        cameras: HashMap<String, CameraConfig>,
    ) -> (Self, Vec<Effect>) {
        let date_time = time::OffsetDateTime::now_local().unwrap_or_else(|_| {
            log::warn!("flight: local time unavailable, journal falls back to UTC");
            time::OffsetDateTime::now_utc()
        });
        let mut core = Self {
            phase: RaceStage::Before,
            rules,
            journal: Journal::new(date_time, pads, assignments),
            captures_dir,
            started: now,
            buzzer_len,
            cameras,
            rec_pending: HashMap::new(),
            rec_deferred: Vec::new(),
            rec_seen: HashMap::new(),
            raced: false,
        };
        core.rec_deferred
            .push((core.ms(now), JournalRecord::RecordStarted {}));
        let effects = vec![Effect::Record {
            sessions: core.start_sessions(),
        }];
        (core, effects)
    }

    /// Метка мс от нуля журнала.
    fn ms(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.started).as_millis() as u64
    }

    pub fn phase(&self) -> RaceStage {
        self.phase
    }

    /// Журнал для передачи GUI при закрытии гонки (EnterReview) и для
    /// записи на диск (поток-обёртка — единственный писатель файла).
    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    /// Журнал для передачи GUI при закрытии гонки (EnterReview).
    pub fn into_journal(self) -> Journal {
        self.journal
    }

    /// Гонка состоялась: финиш после гонки (DVR) vs отмена записи (Idle).
    pub fn has_raced(&self) -> bool {
        self.raced
    }

    /// Ближайший таймер ядра: конец отсчёта или лимит гонки.
    pub fn next_deadline(&self) -> Option<Instant> {
        match self.phase {
            RaceStage::Countdown { start_at } => Some(start_at),
            RaceStage::Running { start_at } => self.rules.duration.map(|d| start_at + d),
            _ => None,
        }
    }

    /// Перевод часов: конец отсчёта — в Running, лимит — в After.
    pub fn tick(&mut self, now: Instant) -> Vec<Effect> {
        match self.phase {
            RaceStage::Countdown { start_at } if now >= start_at => {
                // Журнал появляется только если гонка состоялась:
                // отложенная голова уходит в память, файл создаёт
                // писатель (поток-обёртка) снапшотом на этом же тике.
                self.flush_rec_deferred();
                self.journal
                    .push(self.ms(start_at), JournalRecord::RaceStarted {});
                self.phase = RaceStage::Running { start_at };
                self.raced = true;
                Vec::new()
            }
            RaceStage::Running { start_at } => {
                let elapsed = now.saturating_duration_since(start_at);
                if self.rules.duration.is_some_and(|limit| elapsed >= limit) {
                    self.journal.push(
                        self.ms(now),
                        JournalRecord::RaceFinished { prematurely: false },
                    );
                    self.phase = RaceStage::After { race: elapsed };
                    vec![Effect::Buzzer]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    /// Намерение → эффекты: единственная точка входа ядра (поток-обёртка
    /// и тесты). Разбор по вариантам — в приватных обработчиках ниже.
    pub fn intent(&mut self, intent: Intent, now: Instant) -> Vec<Effect> {
        match intent {
            Intent::RecPressed => self.rec_pressed(now),
            Intent::RacePressed => self.race_pressed(now),
            Intent::Exit => self.exit(now),
        }
    }

    /// Намерение REC (ядро живо).
    fn rec_pressed(&mut self, now: Instant) -> Vec<Effect> {
        match self.phase {
            RaceStage::Before | RaceStage::Countdown { .. } => {
                // REC-off до старта гонки: отмена сессии. Поток увидит
                // Finished без raced и дропнет ядро (GUI — в Idle).
                self.rec_deferred.clear();
                self.rec_pending.clear();
                self.phase = RaceStage::Finished;
                vec![Effect::CancelAudio, Effect::StopRecording { commit: false }]
            }
            RaceStage::Running { .. } => {
                // REC гасит запись посреди гонки — гонка финиширует здесь.
                self.journal.push(
                    self.ms(now),
                    JournalRecord::RaceFinished { prematurely: false },
                );
                self.finish_recording(now);
                self.phase = RaceStage::Finished;
                vec![Effect::Buzzer, Effect::StopRecording { commit: true }]
            }
            RaceStage::After { .. } => {
                self.finish_recording(now);
                self.phase = RaceStage::Finished;
                vec![Effect::StopRecording { commit: true }]
            }
            RaceStage::Finished => Vec::new(),
        }
    }

    /// Намерение RACE (ядро живо).
    fn race_pressed(&mut self, now: Instant) -> Vec<Effect> {
        match self.phase {
            RaceStage::Before => {
                // Запись уже идёт (new = REC-on): отсчёт к ней.
                let delay = self.countdown_delay();
                self.defer_countdown(now);
                self.phase = RaceStage::Countdown {
                    start_at: race_time_start(now, delay, self.buzzer_len),
                };
                vec![Effect::Countdown { delay }]
            }
            RaceStage::Countdown { .. } => {
                // Отмена: отложенный отсчёт не состоялся — выбрасываем.
                self.rec_deferred
                    .retain(|(_, ev)| !matches!(ev, JournalRecord::RaceCountdown {}));
                self.phase = RaceStage::Before;
                vec![Effect::CancelAudio]
            }
            RaceStage::Running { start_at } => {
                // Финиш кнопкой — гудок, запись хвоста продолжается.
                let race = now.saturating_duration_since(start_at);
                self.journal.push(
                    self.ms(now),
                    JournalRecord::RaceFinished { prematurely: true },
                );
                self.phase = RaceStage::After { race };
                vec![Effect::Buzzer]
            }
            RaceStage::After { .. } | RaceStage::Finished => Vec::new(),
        }
    }

    /// Выход с экрана: инварианты целостности — race-started всегда с
    /// парным race-finished, записи вне гонки не остаются на диске.
    /// Ядро после вызова уничтожается — состояние не переводим.
    fn exit(&mut self, now: Instant) -> Vec<Effect> {
        match self.phase {
            RaceStage::Running { .. } => {
                self.journal.push(
                    self.ms(now),
                    JournalRecord::RaceFinished { prematurely: true },
                );
                self.finish_recording(now);
                vec![Effect::Buzzer, Effect::StopRecording { commit: true }]
            }
            RaceStage::After { .. } => {
                self.finish_recording(now);
                vec![Effect::StopRecording { commit: true }]
            }
            RaceStage::Before | RaceStage::Countdown { .. } => {
                self.rec_deferred.clear();
                self.rec_pending.clear();
                vec![Effect::CancelAudio, Effect::StopRecording { commit: false }]
            }
            RaceStage::Finished => Vec::new(),
        }
    }

    /// Коммит: закрыть сессию записи RecordFinished. Имена сессий
    /// без первого кадра молча снимаются (файлов нет — событий нет:
    /// GUI показывает Error для камер без VideoAttached).
    fn finish_recording(&mut self, now: Instant) {
        for (camera_id, pending) in mem::take(&mut self.rec_pending) {
            if !pending.is_empty() {
                log::warn!(
                    "flight: recording of {camera_id} produced no file; \
                     journal entry dropped"
                );
            }
        }
        self.journal
            .push(self.ms(now), JournalRecord::RecordFinished {});
    }

    /// Свежий first_frame_at камеры из поллинга record_state. Новое
    /// значение (новый Instant после старта сессии) — событие первого
    /// кадра: имя файла из FIFO сессий; до старта гонки событие
    /// откладывается, после — пишется сразу.
    pub fn note_frame(&mut self, camera_id: &str, cur: Option<Instant>) {
        let seen = self.rec_seen.insert(camera_id.to_owned(), cur);
        let Some(at) = cur else {
            return;
        };
        if seen.flatten() == Some(at) {
            return;
        }
        let Some(pending) = self.rec_pending.get_mut(camera_id) else {
            log::warn!("flight: first frame of {camera_id} without pending filename");
            return;
        };
        let Some(filename) = pending.pop_front() else {
            log::warn!("flight: first frame of {camera_id} without pending filename");
            return;
        };
        let event = JournalRecord::VideoAttached {
            camera_id: camera_id.to_owned(),
            filename,
        };
        if self.phase().race_started() {
            self.journal.push(self.ms(at), event);
        } else {
            self.rec_deferred.push((self.ms(at), event));
        }
    }

    /// Отложенные события (запись головы до старта гонки) — в журнал
    /// (момент старта гонки, метки исходные, порядок хронологический).
    fn flush_rec_deferred(&mut self) {
        for (ms, event) in mem::take(&mut self.rec_deferred) {
            self.journal.push(ms, event);
        }
    }

    /// Отложенный Countdown: повторное нажатие RACE (REC-циклы
    /// до гонки) не плодит дубликаты — старый выбрасываем.
    fn defer_countdown(&mut self, now: Instant) {
        self.rec_deferred
            .retain(|(_, ev)| !matches!(ev, JournalRecord::RaceCountdown {}));
        self.rec_deferred
            .push((self.ms(now), JournalRecord::RaceCountdown {}));
    }

    fn countdown_delay(&self) -> Duration {
        Duration::from_secs(PRE_BEEP_SECS) + sample_start_delay(&self.rules.start_signal_delay)
    }

    /// Сессии записи всех камер: одно имя на камеру (повторный старт
    /// после abort переиспользует стем — прошлый файл удалён), имена —
    /// в FIFO ожидания первых кадров. Параметры готовы к исполнению —
    /// обёртка их не собирает.
    fn start_sessions(&mut self) -> Vec<(String, RecordParams)> {
        let flight_stem = self.journal.header().file_stem();
        let mut sessions = Vec::new();
        for (id, camera) in &self.cameras {
            let stem = format!("{flight_stem}-{id}");
            let ext = match camera.dvr.container {
                CameraConfigContainer::Mkv => "mkv",
                CameraConfigContainer::Mov => "mov",
                CameraConfigContainer::Mp4 => "mp4",
            };
            let filename = format!("{stem}-mjpeg.{ext}");
            let pending = self.rec_pending.entry(id.clone()).or_default();
            if !pending.is_empty() {
                log::warn!(
                    "flight: previous recording of {id} reported no first frame; \
                     journal entry dropped"
                );
                pending.clear();
            }
            pending.push_back(filename.clone());
            sessions.push((
                id.clone(),
                RecordParams {
                    dir: self.captures_dir.clone(),
                    stem,
                    camera: camera.clone(),
                },
            ));
        }
        sessions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::camera::PixelConfig;
    use crate::model::rules::StartSignalDelay;
    use std::env::temp_dir;

    /// Длительность гонки для сценариев.
    const DURATION: Duration = Duration::from_secs(60);

    /// Фиктивные часы: старт + ручной шаг (по образцу тестов Transport).
    struct FakeClock {
        now: Instant,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                now: Instant::now(),
            }
        }
        fn advance(&mut self, ms: u64) -> Instant {
            self.now += Duration::from_millis(ms);
            self.now
        }
    }

    fn test_camera() -> CameraConfig {
        CameraConfig::new("Test", 8, 8, 30, PixelConfig::Mjpeg)
    }

    fn test_core(clock: &FakeClock) -> RaceCore {
        RaceCore::new(
            clock.now,
            RaceRules {
                duration: Some(DURATION),
                start_signal_delay: StartSignalDelay::random(Duration::ZERO, Duration::ZERO),
            },
            temp_dir(),
            Duration::ZERO, // гудок нулевой: числа сценариев проще
            BTreeMap::new(),
            HashMap::new(),
            HashMap::from([("camera-1".to_owned(), test_camera())]),
        )
        .0
    }

    /// События журнала как (ms, короткая метка) для asserts.
    fn journal_marks(core: &RaceCore) -> Vec<(u64, &'static str)> {
        core.journal
            .iter_ms()
            .map(|(ms, event)| {
                let mark = match event {
                    JournalRecord::RecordStarted {} => "rec",
                    JournalRecord::RecordFinished {} => "rec-end",
                    JournalRecord::VideoAttached { .. } => "file",
                    JournalRecord::RaceCountdown {} => "cd",
                    JournalRecord::RaceStarted {} => "go",
                    JournalRecord::RaceFinished { prematurely } => {
                        if *prematurely {
                            "finish-user"
                        } else {
                            "finish-auto"
                        }
                    }
                };
                (ms, mark)
            })
            .collect()
    }

    /// Полный цикл: запись головы → RACE → отсчёт → старт → круги →
    /// авто-финиш на лимите → REC → DVR.
    #[test]
    fn full_race_cycle() {
        let mut clock = FakeClock::new();
        // new = REC-on: запись головы уже включена (REC до гонки).
        let mut core = test_core(&clock);
        assert!(core.phase().rec_on() && !core.phase().race_started());

        // Первый кадр головы — событие откладывается (гонки ещё нет),
        // журнал до старта гонки пуст.
        core.note_frame("camera-1", Some(clock.advance(500)));
        assert!(journal_marks(&core).is_empty());

        // RACE: отсчёт, запись продолжается (та же сессия).
        let effects = core.intent(Intent::RacePressed, clock.now);
        let delay = match effects.as_slice() {
            [Effect::Countdown { delay }] => *delay,
            other => panic!("expected countdown, got {other:?}"),
        };
        assert_eq!(
            delay,
            Duration::from_secs(PRE_BEEP_SECS),
            "вырожденный сигнал — только бипы"
        );

        // Отсчёт тикает: старт гонки, отложенная голова — перед ним.
        // Отсчёт начался в t0+500, старт = t0+500+delay.
        let t_start = clock.advance(delay.as_millis() as u64);
        assert!(core.tick(t_start).is_empty());
        assert!(core.has_raced(), "факт старта гонки — гейт журнала и DVR");
        let marks = journal_marks(&core);
        assert_eq!(marks[0], (0, "rec"), "REC-on головы: {marks:?}");
        assert_eq!(marks[1], (500, "file"), "файл головы: {marks:?}");
        assert_eq!(marks[2], (500, "cd"), "отсчёт: {marks:?}");
        assert_eq!(
            marks[3],
            (500 + delay.as_millis() as u64, "go"),
            "{marks:?}"
        );

        // Лимит: авто-финиш ровно через DURATION от старта.
        let t_finish = clock.advance(DURATION.as_millis() as u64);
        assert!(matches!(core.tick(t_finish).as_slice(), [Effect::Buzzer]));
        assert_eq!(
            journal_marks(&core)[4],
            (
                500 + delay.as_millis() as u64 + DURATION.as_millis() as u64,
                "finish-auto"
            ),
            "{:?}",
            journal_marks(&core)
        );
        assert_eq!(
            core.phase().race_elapsed(t_finish),
            DURATION,
            "счётчик заморожен на лимите"
        );

        // REC после гонки — коммит и закрытие сессии.
        assert!(matches!(
            core.intent(Intent::RecPressed, clock.now).as_slice(),
            [Effect::StopRecording { commit: true }]
        ));
        let marks = journal_marks(&core);
        assert_eq!(
            marks.last(),
            Some(&(
                500 + delay.as_millis() as u64 + DURATION.as_millis() as u64,
                "rec-end"
            )),
            "сессия закрыта: {marks:?}"
        );
        assert!(core.phase().finished());
    }

    /// Отмена отсчёта RACE-кнопкой: назад в Before, записи живы.
    #[test]
    fn cancel_countdown_race_button() {
        let clock = FakeClock::new();
        // new = REC-on: запись уже идёт.
        let mut core = test_core(&clock);
        let t = clock.now;
        core.intent(Intent::RacePressed, t);
        assert!(core.phase().countdown_active());
        assert!(matches!(
            core.intent(Intent::RacePressed, t).as_slice(),
            [Effect::CancelAudio]
        ));
        assert!(!core.phase().countdown_active());
        assert!(core.phase().rec_on(), "запись продолжается");
        assert!(!core.phase().race_started());
    }

    /// Отмена отсчёта REC'ом: запись abort, отложенные события выброшены.
    #[test]
    fn cancel_countdown_rec_aborts() {
        let mut clock = FakeClock::new();
        // new = REC-on: запись уже идёт.
        let mut core = test_core(&clock);
        core.note_frame("camera-1", Some(clock.advance(300)));
        core.intent(Intent::RacePressed, clock.now);
        assert!(matches!(
            core.intent(Intent::RecPressed, clock.now).as_slice(),
            [Effect::CancelAudio, Effect::StopRecording { commit: false }]
        ));
        assert!(journal_marks(&core).is_empty(), "голова не журналирована");
    }

    /// REC посреди гонки: гудок, честный финиш, коммит, Finished.
    #[test]
    fn rec_stop_mid_race_finishes() {
        let mut clock = FakeClock::new();
        let mut core = test_core(&clock);
        core.intent(Intent::RacePressed, clock.now);
        let t_start = clock.advance(Duration::from_secs(PRE_BEEP_SECS).as_millis() as u64);
        core.tick(t_start);
        // Камера пишет — иначе файла не будет (сессия снимется молча).
        core.note_frame("camera-1", Some(clock.advance(1)));
        let t_rec = clock.advance(20_000);
        assert!(matches!(
            core.intent(Intent::RecPressed, t_rec).as_slice(),
            [Effect::Buzzer, Effect::StopRecording { commit: true }]
        ));
        let marks = journal_marks(&core);
        assert_eq!(
            marks.last(),
            Some(&(23_001, "rec-end")),
            "коммит закрывает запись: {marks:?}"
        );
        assert!(core.phase().finished());
    }

    /// RACE-кнопка в Race: финиш пользователя, запись продолжается (хвост).
    #[test]
    fn race_button_finishes_to_tail() {
        let mut clock = FakeClock::new();
        let mut core = test_core(&clock);
        core.intent(Intent::RacePressed, clock.now);
        let t_start = clock.advance(Duration::from_secs(PRE_BEEP_SECS).as_millis() as u64);
        core.tick(t_start);
        let t = clock.advance(12_345);
        assert!(matches!(
            core.intent(Intent::RacePressed, t).as_slice(),
            [Effect::Buzzer]
        ));
        let marks = journal_marks(&core);
        assert_eq!(marks.last(), Some(&(15_345, "finish-user")), "{marks:?}");
        assert!(core.phase().rec_on(), "хвост продолжается");
        assert!(!core.phase().finished());
    }

    /// EXIT из каждой фазы: инварианты целостности.
    #[test]
    fn exit_from_every_phase() {
        // Из Race: гудок + финиш + коммит.
        let mut clock = FakeClock::new();
        let mut core = test_core(&clock);
        core.intent(Intent::RacePressed, clock.now);
        let t_start = clock.advance(Duration::from_secs(PRE_BEEP_SECS).as_millis() as u64);
        core.tick(t_start);
        // Камера пишет — иначе файла не будет (сессия снимется молча).
        core.note_frame("camera-1", Some(clock.advance(1)));
        let t = clock.advance(5_000);
        assert!(matches!(
            core.intent(Intent::Exit, t).as_slice(),
            [Effect::Buzzer, Effect::StopRecording { commit: true }]
        ));
        assert_eq!(
            journal_marks(&core).last(),
            Some(&(8_001, "rec-end")),
            "коммит закрывает запись"
        );

        // Из After: только коммит (финиш уже в журнале).
        let mut clock = FakeClock::new();
        let mut core = test_core(&clock);
        core.intent(Intent::RacePressed, clock.now);
        let t_start = clock.advance(Duration::from_secs(PRE_BEEP_SECS).as_millis() as u64);
        core.tick(t_start);
        core.note_frame("camera-1", Some(clock.advance(1)));
        core.intent(Intent::RacePressed, clock.now);
        assert!(matches!(
            core.intent(Intent::Exit, clock.now).as_slice(),
            [Effect::StopRecording { commit: true }]
        ));

        // Из Before/Countdown: cancel + abort, без гонки файла нет.
        for race_first in [false, true] {
            let clock = FakeClock::new();
            // new = REC-on: ядро уже в Before.
            let mut core = test_core(&clock);
            if race_first {
                core.intent(Intent::RacePressed, clock.now);
            }
            assert!(matches!(
                core.intent(Intent::Exit, clock.now).as_slice(),
                [Effect::CancelAudio, Effect::StopRecording { commit: false }]
            ));
            assert!(
                journal_marks(&core).is_empty(),
                "записи вне гонки не журналируются"
            );
        }
    }

    /// Камера без файла: при коммите — ни события, ни имени (файла
    /// просто нет). Сессия закрывается обычным rec-end, живая
    /// запись — своим file.
    #[test]
    fn fileless_camera_leaves_no_trace() {
        let mut clock = FakeClock::new();
        let mut cameras = HashMap::from([("camera-1".to_owned(), test_camera())]);
        cameras.insert("camera-2".to_owned(), test_camera());
        let mut core = RaceCore::new(
            clock.now,
            RaceRules {
                duration: Some(DURATION),
                start_signal_delay: StartSignalDelay::random(Duration::ZERO, Duration::ZERO),
            },
            temp_dir(),
            Duration::ZERO,
            BTreeMap::new(),
            HashMap::new(),
            cameras,
        )
        .0;
        core.intent(Intent::RacePressed, clock.now);
        let t_start = clock.advance(Duration::from_secs(PRE_BEEP_SECS).as_millis() as u64);
        core.tick(t_start);
        // Первая камера пишет, вторая — нет: note_frame для неё не дёргаем.
        core.note_frame("camera-1", Some(clock.advance(100)));
        let t_rec = clock.advance(20_000);
        assert!(matches!(
            core.intent(Intent::RecPressed, t_rec).as_slice(),
            [Effect::Buzzer, Effect::StopRecording { commit: true }]
        ));
        let marks = journal_marks(&core);
        assert_eq!(
            marks,
            vec![
                (0, "rec"),
                (0, "cd"),
                (3_000, "go"),
                (3_100, "file"),
                (23_100, "finish-auto"),
                (23_100, "rec-end"),
            ],
            "камера-2 — ни события, ни следа: {marks:?}"
        );
    }

    /// Гонка без лимита идёт, пока её не завершат: авто-финиша нет.
    #[test]
    fn race_without_limit_needs_user_finish() {
        let mut clock = FakeClock::new();
        let mut core = RaceCore::new(
            clock.now,
            RaceRules {
                duration: None,
                start_signal_delay: StartSignalDelay::random(Duration::ZERO, Duration::ZERO),
            },
            temp_dir(),
            Duration::ZERO,
            BTreeMap::new(),
            HashMap::new(),
            HashMap::from([("camera-1".to_owned(), test_camera())]),
        )
        .0;
        core.intent(Intent::RacePressed, clock.now);
        let t_start = clock.advance(Duration::from_secs(PRE_BEEP_SECS).as_millis() as u64);
        core.tick(t_start);
        // Час гонки — никакого авто-финиша.
        let t_later = clock.advance(3_600_000);
        assert!(core.tick(t_later).is_empty());
        assert!(
            matches!(core.phase(), RaceStage::Running { .. }),
            "гонка без лимита жива"
        );
        assert!(!core.phase().race_started() || matches!(core.phase(), RaceStage::Running { .. }));
        // Пользователь завершает кнопкой.
        assert!(matches!(
            core.intent(Intent::RacePressed, t_later).as_slice(),
            [Effect::Buzzer]
        ));
    }

    /// REC-off до старта гонки завершает сессию: cancel + abort,
    /// журнал пуст (на диск он не попадал — снапшот только со старта
    /// гонки). Новая сессия — новое ядро (new), новый стем.
    #[test]
    fn rec_off_before_race_finishes_session() {
        let mut clock = FakeClock::new();
        let mut core = test_core(&clock);
        core.note_frame("camera-1", Some(clock.advance(300)));
        assert!(matches!(
            core.intent(Intent::RecPressed, clock.now).as_slice(),
            [Effect::CancelAudio, Effect::StopRecording { commit: false }]
        ));
        assert!(core.phase().finished());
        assert!(!core.has_raced());
        assert!(
            journal_marks(&core).is_empty(),
            "отменённая сессия не журналирована"
        );
    }
}

/// Сообщения ядра → GUI.
pub enum Notice {
    /// Состояние эфира изменилось (GUI держит последний).
    State(RaceStage),
    /// Гонка закрыта: журнал переходит GUI (открытие плееров — GUI,
    /// там egui::Context). Поток после этого завершается.
    EnterReview { journal: Journal },
    /// REC-off до старта гонки: сессия отменена, журнала на диске нет.
    /// GUI возвращается в Idle (поток завершился).
    BackToIdle,
}

/// Хэндл гоночного потока для GUI: шлём намерения, забираем уведомления.
pub struct Race {
    intents: crossbeam::Sender<Intent>,
    pub notices: crossbeam::Receiver<Notice>,
}

impl Race {
    /// Намерения в ядро — типизированные обёртки над каналом.
    pub fn rec_pressed(&self) {
        let _ = self.intents.try_send(Intent::RecPressed);
    }

    pub fn race_pressed(&self) {
        let _ = self.intents.try_send(Intent::RacePressed);
    }

    pub fn exit(&self) {
        let _ = self.intents.try_send(Intent::Exit);
    }

    /// Рождает гоночный поток и возвращает хэндл. new = REC нажали:
    /// журнал и ноль меток — от этого момента. Жизнь потока — сессия
    /// REC: REC-off до старта гонки отменяет её (ядро дропается,
    /// GUI — в Idle), после финиша отдаём журнал в DVR. Рождение
    /// кнопкой RACE: GUI досылает намерение RACE сразу после
    /// spawn — поток стартует с ним в очереди.
    pub fn spawn(
        rules: RaceRules,
        captures_dir: PathBuf,
        pads: BTreeMap<String, PadConfig>,
        assignments: HashMap<String, Pilot>,
        cameras: HashMap<String, CameraConfig>,
        recorders: HashMap<String, Recorder>,
        keyboard_rx: crossbeam::Receiver<KeyPressed>,
        wake: Arc<dyn Fn() + Send + Sync>,
        audio: Audio,
    ) -> Race {
        let (intent_tx, intent_rx) = crossbeam::bounded(16);
        let (notice_tx, notice_rx) = crossbeam::bounded(64);
        let (core, effects) = RaceCore::new(
            Instant::now(),
            rules,
            captures_dir.clone(),
            Sound::BUZZER.duration(),
            pads,
            assignments,
            cameras,
        );
        thread::spawn(move || {
            let mut core = core;
            apply(&effects, &recorders, &audio);
            let mut last_air = core.phase();
            let _ = notice_tx.try_send(Notice::State(last_air));
            wake();
            // Журнал на диске: снапшот на старте гонки, дальше дописываем.
            // Путь и счётчик дописанных событий держит писатель (этот цикл).
            let mut journal_path: Option<PathBuf> = None;
            let mut journal_written = 0usize;
            let sync_journal = |active: &RaceCore,
                                journal_path: &mut Option<PathBuf>,
                                journal_written: &mut usize| {
                // Гейт — факт старта гонки, не фаза: аварийное отжатие
                // уходит в Finished без гонки (has_raced == false), и писать
                // пустую шапку на диск не нужно.
                if !active.has_raced() {
                    return;
                }
                let journal = active.journal();
                match journal_path {
                    None => match journal.snapshot(&captures_dir) {
                        Ok(path) => {
                            log::info!("flight journal: stream opened at {path:?}");
                            *journal_written = journal.event_count();
                            *journal_path = Some(path);
                        }
                        Err(e) => log::error!("flight journal: snapshot failed: {e}"),
                    },
                    Some(path) => {
                        if let Err(e) = journal.append_to(path, journal_written) {
                            log::error!("flight journal: append failed: {e}");
                        }
                    }
                }
            };

            let mut keyboard_rx = keyboard_rx;
            loop {
                let now = Instant::now();
                let until_deadline = core
                    .next_deadline()
                    .map(|d| d.saturating_duration_since(now))
                    .unwrap_or(POLL);
                // Ожидание: намерение GUI, клавиатура или дедлайн ядра
                // (таймаут — тот же min(deadline, POLL)).
                let effects = crossbeam::select! {
                    recv(&intent_rx) -> intent => match intent {
                        Ok(intent) => core.intent(intent, Instant::now()),
                        Err(_) => break, // GUI ушёл без выхода
                    },
                    recv(&keyboard_rx) -> event => match event {
                        // Регистрация нажатий — в RaceCore (пока лог).
                        Ok(event) => {
                            log::info!("keyboard: {event}");
                            Vec::new()
                        }
                        // Отвал клавиатур отключает ветку: never не
                        // готов никогда, цикл живёт.
                        Err(_) => {
                            keyboard_rx = crossbeam::never();
                            Vec::new()
                        }
                    },
                    default(until_deadline.min(POLL)) => core.tick(Instant::now()),
                };
                apply(&effects, &recorders, &audio);

                // Поллинг first_frame_at: edge-детект — в ядре.
                for (id, recorder) in &recorders {
                    let first_frame_at = match recorder.record_state() {
                        RecordState::Writing { first_frame_at, .. } => Some(first_frame_at),
                        _ => None,
                    };
                    core.note_frame(id, first_frame_at);
                }
                sync_journal(&core, &mut journal_path, &mut journal_written);

                // State — только при изменении: опрос каждые POLL не должен
                // гнать уведомления.
                let air = core.phase();
                if air != last_air {
                    last_air = air;
                    let _ = notice_tx.try_send(Notice::State(air));
                    wake();
                }
                if air.finished() {
                    if core.has_raced() {
                        // Финализация writer'ов (Cues, патчи, sync) — до
                        // того, как GUI (тёплый Review) откроет файлы на чтение.
                        for recorder in recorders.values() {
                            recorder.wait_finalized(Duration::from_secs(5));
                        }
                        let _ = notice_tx.try_send(Notice::EnterReview {
                            journal: core.into_journal(),
                        });
                        wake();
                    } else {
                        // REC-off/EXIT до старта гонки: сессия отменена,
                        // файлы удаляются в фоне, GUI их не читает — ждать
                        // нечего: BackToIdle немедленно, кнопки оживают в
                        // Idle без провисания в мёртвом Finished.
                        drop(core);
                        let _ = notice_tx.try_send(Notice::BackToIdle);
                        wake();
                    }
                    break;
                }
            }
        });
        Race {
            intents: intent_tx,
            notices: notice_rx,
        }
    }
}

/// Звуковая программа отсчёта: бипы по секундам, гудок на старте.
/// Задержка до гудка — из ядра (там же сэмплирована пауза сигнала).
fn countdown_steps(delay: Duration) -> Vec<(Duration, Sound)> {
    let interval = delay.saturating_sub(Duration::from_secs(PRE_BEEP_SECS));
    if interval.is_zero() {
        vec![(Duration::ZERO, Sound::BUZZER)]
    } else {
        let mut steps: Vec<(Duration, Sound)> = (0..PRE_BEEP_SECS)
            .map(Duration::from_secs)
            .map(|t| (t, Sound::STAGE))
            .collect();
        steps.push((delay, Sound::BUZZER));
        steps
    }
}

/// Исполнение эффектов ядра: команды рекордерам, звук.
fn apply(effects: &[Effect], recorders: &HashMap<String, Recorder>, audio: &Audio) {
    for effect in effects {
        match effect {
            Effect::Record { sessions } => {
                for (id, params) in sessions {
                    if let Some(recorder) = recorders.get(id) {
                        recorder.start_recording(params.clone());
                    }
                }
            }
            Effect::StopRecording { commit } => {
                for recorder in recorders.values() {
                    if *commit {
                        recorder.stop_recording();
                    } else {
                        recorder.abort_recording();
                    }
                }
            }
            Effect::Countdown { delay } => audio.sequence(countdown_steps(*delay)),
            Effect::Buzzer => audio.play(Sound::BUZZER),
            Effect::CancelAudio => audio.cancel(),
        }
    }
}
