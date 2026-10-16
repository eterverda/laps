//! Гоночный сервис: ядро течения гонки (`RaceCore` — состояния, политика
//! записи, журнал) и его поток-обёртка: исполнение эффектов (команды
//! рекордерам, звук), поллинг first_frame_at, трансляция состояния в
//! GUI. Правила гонки (сигнал старта, отсчёт, длительность) — в
//! `model::rules`. Модуль: `service::race`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use crate::config::camera::{CameraConfig, ContainerConfig as CameraConfigContainer};
use crate::config::setup::PadConfig;
use crate::driver::audio::{self, Audio};
use crate::driver::camera::Recorder;
use crate::driver::dvr::RecordParams;
use crate::model::flight::{FlightEvent, Journal, file_stem};
use crate::model::pilot::Pilot;
use crate::model::rules::{PRE_BEEP_SECS, RaceRules, race_time_start, sample_start_delay};

/// Гудок старт/стоп.
const BUZZER: audio::Sound = audio::Sound::from_bytes(crate::assets::BUZZER_WAV);
/// Бип отсчёта.
const STAGE: audio::Sound = audio::Sound::from_bytes(crate::assets::STAGE_WAV);

/// Интервал поллинга record_state (first_frame_at) и крышка сна
/// recv_timeout: события первых кадров заметны не позже этого.
const POLL: Duration = Duration::from_millis(30);

// --- Ядро гонки --------------------------------------------------------------

/// Намерения в ядро (GUI-кнопки, выход).
#[derive(Debug)]
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

/// Фазы эфира — домен ядра. DVR (экран) живёт в GUI.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AirPhase {
    LiveBefore,
    RecordHead,
    RaceCountdown,
    Race,
    RecordTail,
    /// Гонка закрыта, журнал передан: GUI переходит в DVR.
    Finished,
}

/// Снапшот состояния эфира для GUI (рендер кнопок/счётчиков).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AirState {
    pub phase: AirPhase,
    /// Конец отсчёта (старт отсчёта + задержка + длина гудка) — для
    /// мигания счётчика; GUI длину гудка не знает.
    pub countdown_end: Option<Instant>,
    /// Бегущая гонка: момент старта.
    pub race: Option<Instant>,
    /// Замороженное время гонки (после финиша).
    pub race_done: Option<Duration>,
}

impl AirState {
    /// REC активна (индикатор/цвет кнопки).
    pub fn rec_on(&self) -> bool {
        matches!(
            self.phase,
            AirPhase::RecordHead | AirPhase::RaceCountdown | AirPhase::Race | AirPhase::RecordTail
        )
    }

    /// Идёт отсчёт (мигание счётчика).
    pub fn countdown_active(&self) -> bool {
        self.countdown_end.is_some()
    }

    /// Время гонки для счётчика: бежит в Race, заморожено после.
    pub fn race_elapsed(&self, now: Instant) -> Duration {
        if let Some(done) = self.race_done {
            return done;
        }
        self.race
            .map_or(Duration::ZERO, |start| now.saturating_duration_since(start))
    }

    /// REC кликабельна: с эфира до конца хвоста.
    pub fn rec_available(&self) -> bool {
        !matches!(self.phase, AirPhase::Finished)
    }

    /// RACE кликабельна: нажатие до старта, отжатие в Countdown/Race.
    pub fn race_available(&self) -> bool {
        matches!(
            self.phase,
            AirPhase::LiveBefore | AirPhase::RecordHead | AirPhase::RaceCountdown | AirPhase::Race
        )
    }

    /// Гонка началась (или дальше): записи относятся к ней.
    pub fn race_started(&self) -> bool {
        matches!(
            self.phase,
            AirPhase::Race | AirPhase::RecordTail | AirPhase::Finished
        )
    }

    pub fn finished(&self) -> bool {
        matches!(self.phase, AirPhase::Finished)
    }
}

/// Состояние ядра (приватное; наружу — AirState).
#[derive(Clone, Copy)]
enum State {
    LiveBefore,
    RecordHead,
    RaceCountdown { start: Instant, delay: Duration },
    Race { start: Instant },
    RecordTail { race: Duration },
    Finished,
}

/// Гоночное ядро: часы гонки, политика записи, журнал. Чистое — решения
/// возвращаются эффектами, IO исполняет поток-обёртка. Ядро — единственный
/// писатель журнала; порядок «журнал раньше уведомления GUI» обеспечивает
/// обёртка (эффекты применяются после записи в журнал).
pub struct RaceCore {
    state: State,
    rules: RaceRules,
    journal: Journal,
    captures_dir: PathBuf,
    /// Длина гудка: старт гонки — конец отсчёта + гудок (звук —
    /// презентация, но часы гонки завязаны на его длительность).
    buzzer_len: Duration,
    /// camera_id → конфиг (стемы, расширения файлов записи).
    cameras: HashMap<String, CameraConfig>,
    /// Порядковый номер сессии записи по камерам (-r2, -r3, … у повторных).
    rec_count: HashMap<String, u32>,
    /// Имена файлов открытых сессий по камерам (FIFO: сессия привязывается
    /// к первому кадру по порядку стартов).
    rec_pending: HashMap<String, VecDeque<String>>,
    /// Отложенные RecordingStarted (запись головы до старта гонки):
    /// пишутся в журнал в момент старта гонки, при отмене — выбрасываются.
    rec_deferred: Vec<(Instant, FlightEvent)>,
    /// Последнее известное first_frame_at по камерам (edge-детект).
    rec_seen: HashMap<String, Option<Instant>>,
}

impl RaceCore {
    pub fn new(
        now: Instant,
        rules: RaceRules,
        captures_dir: PathBuf,
        buzzer_len: Duration,
        pads: BTreeMap<String, PadConfig>,
        assignments: HashMap<String, Pilot>,
        cameras: HashMap<String, CameraConfig>,
    ) -> Self {
        let date_time = time::OffsetDateTime::now_local().unwrap_or_else(|_| {
            log::warn!("flight: local time unavailable, journal falls back to UTC");
            time::OffsetDateTime::now_utc()
        });
        Self {
            state: State::LiveBefore,
            rules,
            journal: Journal::new(now, date_time, pads, assignments),
            captures_dir,
            buzzer_len,
            cameras,
            rec_count: HashMap::new(),
            rec_pending: HashMap::new(),
            rec_deferred: Vec::new(),
            rec_seen: HashMap::new(),
        }
    }

    pub fn state(&self) -> AirState {
        let (countdown_end, race, race_done) = match self.state {
            State::LiveBefore | State::RecordHead | State::Finished => (None, None, None),
            State::RaceCountdown { start, delay } => (
                Some(race_time_start(start, delay, self.buzzer_len)),
                None,
                None,
            ),
            State::Race { start } => (None, Some(start), None),
            State::RecordTail { race } => (None, None, Some(race)),
        };
        AirState {
            phase: match self.state {
                State::LiveBefore => AirPhase::LiveBefore,
                State::RecordHead => AirPhase::RecordHead,
                State::RaceCountdown { .. } => AirPhase::RaceCountdown,
                State::Race { .. } => AirPhase::Race,
                State::RecordTail { .. } => AirPhase::RecordTail,
                State::Finished => AirPhase::Finished,
            },
            countdown_end,
            race,
            race_done,
        }
    }

    /// Журнал для передачи GUI при закрытии гонки (EnterDvr).
    pub fn into_journal(self) -> Journal {
        self.journal
    }

    /// Ближайший таймер ядра: конец отсчёта или лимит гонки.
    pub fn next_deadline(&self) -> Option<Instant> {
        match self.state {
            State::RaceCountdown { start, delay } => {
                Some(race_time_start(start, delay, self.buzzer_len))
            }
            State::Race { start } => self.rules.duration.map(|d| start + d),
            _ => None,
        }
    }

    /// Перевод часов: конец отсчёта — в Race, лимит — в RecordTail.
    pub fn tick(&mut self, now: Instant) -> Vec<Effect> {
        match self.state {
            State::RaceCountdown { start, delay }
                if now >= race_time_start(start, delay, self.buzzer_len) =>
            {
                // Журнал появляется только если гонка состоялась: файл
                // открывается текущим содержимым, голова — перед стартом.
                self.journal.open_file(&self.captures_dir);
                self.flush_rec_deferred();
                self.journal.push(
                    race_time_start(start, delay, self.buzzer_len),
                    FlightEvent::RaceStarted {},
                );
                self.state = State::Race {
                    start: race_time_start(start, delay, self.buzzer_len),
                };
                Vec::new()
            }
            State::Race { start } => {
                let elapsed = now.saturating_duration_since(start);
                if self.rules.duration.is_some_and(|limit| elapsed >= limit) {
                    self.journal
                        .push(now, FlightEvent::RaceFinished { prematurely: false });
                    self.state = State::RecordTail { race: elapsed };
                    vec![Effect::Buzzer]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    /// Намерение REC.
    pub fn rec_pressed(&mut self, now: Instant) -> Vec<Effect> {
        match self.state {
            State::LiveBefore => {
                self.state = State::RecordHead;
                vec![Effect::Record {
                    sessions: self.start_sessions(),
                }]
            }
            State::RecordHead => {
                // Запись без гонки — её отложенные события не нужны.
                self.rec_deferred.clear();
                self.state = State::LiveBefore;
                vec![Effect::StopRecording { commit: false }]
            }
            State::RaceCountdown { .. } => {
                self.rec_deferred.clear();
                self.state = State::LiveBefore;
                vec![Effect::CancelAudio, Effect::StopRecording { commit: false }]
            }
            State::Race { .. } => {
                // REC гасит запись посреди гонки — гонка финиширует здесь.
                self.journal
                    .push(now, FlightEvent::RaceFinished { prematurely: false });
                self.state = State::Finished;
                vec![Effect::Buzzer, Effect::StopRecording { commit: true }]
            }
            State::RecordTail { .. } => {
                self.state = State::Finished;
                vec![Effect::StopRecording { commit: true }]
            }
            State::Finished => Vec::new(),
        }
    }

    /// Намерение RACE.
    pub fn race_pressed(&mut self, now: Instant) -> Vec<Effect> {
        match self.state {
            State::LiveBefore => {
                let delay = self.countdown_delay();
                self.state = State::RaceCountdown { start: now, delay };
                vec![
                    Effect::Record {
                        sessions: self.start_sessions(),
                    },
                    Effect::Countdown { delay },
                ]
            }
            State::RecordHead => {
                let delay = self.countdown_delay();
                self.state = State::RaceCountdown { start: now, delay };
                vec![Effect::Countdown { delay }]
            }
            State::RaceCountdown { .. } => {
                self.state = State::RecordHead;
                vec![Effect::CancelAudio]
            }
            State::Race { start } => {
                // Финиш кнопкой — гудок, запись хвоста продолжается.
                let race = now.saturating_duration_since(start);
                self.journal
                    .push(now, FlightEvent::RaceFinished { prematurely: true });
                self.state = State::RecordTail { race };
                vec![Effect::Buzzer]
            }
            State::RecordTail { .. } | State::Finished => Vec::new(),
        }
    }

    /// Выход с экрана: инварианты целостности — race-started всегда с
    /// парным race-finished, записи вне гонки не остаются на диске.
    /// Ядро после вызова уничтожается — состояние не переводим.
    pub fn exit(&mut self, now: Instant) -> Vec<Effect> {
        match self.state {
            State::Race { .. } => {
                self.journal
                    .push(now, FlightEvent::RaceFinished { prematurely: true });
                vec![Effect::Buzzer, Effect::StopRecording { commit: true }]
            }
            State::RecordTail { .. } => vec![Effect::StopRecording { commit: true }],
            State::RecordHead | State::RaceCountdown { .. } => {
                self.rec_deferred.clear();
                vec![Effect::CancelAudio, Effect::StopRecording { commit: false }]
            }
            State::LiveBefore | State::Finished => Vec::new(),
        }
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
        let event = FlightEvent::RecordingStarted {
            camera_id: camera_id.to_owned(),
            filename,
        };
        if self.state().race_started() {
            self.journal.push(at, event);
        } else {
            self.rec_deferred.push((at, event));
        }
    }

    /// Отложенные RecordingStarted — в журнал (момент старта гонки,
    /// таймстампы исходные, порядок хронологический).
    fn flush_rec_deferred(&mut self) {
        for (at, event) in std::mem::take(&mut self.rec_deferred) {
            self.journal.push(at, event);
        }
    }

    fn countdown_delay(&self) -> Duration {
        Duration::from_secs(PRE_BEEP_SECS) + sample_start_delay(&self.rules.start_signal_delay)
    }

    /// Сессии записи всех камер: уникальные имена (-r2, -r3, … у
    /// повторных), имена — в FIFO ожидания первых кадров. Параметры
    /// готовы к исполнению — обёртка их не собирает.
    fn start_sessions(&mut self) -> Vec<(String, RecordParams)> {
        let flight_stem = file_stem(self.journal.date_time());
        let mut sessions = Vec::new();
        for (id, camera) in &self.cameras {
            let n = self.rec_count.get(id).copied().unwrap_or(0) + 1;
            self.rec_count.insert(id.clone(), n);
            let stem = if n == 1 {
                format!("{flight_stem}-{id}")
            } else {
                format!("{flight_stem}-{id}-r{n}")
            };
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
    use crate::model::rules::StartSignalDelay;

    /// Длительность гонки для сценариев.
    const DURATION: Duration = Duration::from_secs(60);

    /// Фиктивные часы: старт + ручной шаг (по образцу тестов Playhead).
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
        CameraConfig::new("Test", 8, 8, 30, crate::config::camera::PixelConfig::Mjpeg)
    }

    fn test_core(clock: &FakeClock) -> RaceCore {
        RaceCore::new(
            clock.now,
            RaceRules {
                duration: Some(DURATION),
                start_signal_delay: StartSignalDelay::random(Duration::ZERO, Duration::ZERO),
            },
            std::env::temp_dir(),
            Duration::ZERO, // гудок нулевой: числа сценариев проще
            BTreeMap::new(),
            HashMap::new(),
            HashMap::from([("camera-1".to_owned(), test_camera())]),
        )
    }

    /// События журнала как (ms, короткая метка) для asserts.
    fn journal_marks(core: &RaceCore) -> Vec<(u64, &'static str)> {
        core.journal
            .iter_ms()
            .map(|(ms, event)| {
                let mark = match event {
                    FlightEvent::JournalStarted { .. } => "started",
                    FlightEvent::RecordingStarted { .. } => "rec",
                    FlightEvent::RaceStarted {} => "go",
                    FlightEvent::RaceFinished { prematurely } => {
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
        let mut core = test_core(&clock);
        let t0 = clock.now;

        // Запись головы (REC до гонки).
        let effects = core.rec_pressed(t0);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Record { sessions }] if sessions.len() == 1
        ));
        assert!(core.state().rec_on() && !core.state().race_started());

        // Первый кадр головы — событие откладывается (гонки ещё нет).
        core.note_frame("camera-1", Some(clock.advance(500)));
        assert!(journal_marks(&core) == vec![(0, "started")]);

        // RACE: отсчёт, запись продолжается (та же сессия).
        let effects = core.race_pressed(clock.now);
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
        let marks = journal_marks(&core);
        assert_eq!(marks[1], (500, "rec"), "{marks:?}");
        assert_eq!(
            marks[2],
            (500 + delay.as_millis() as u64, "go"),
            "{marks:?}"
        );

        // Лимит: авто-финиш ровно через DURATION от старта.
        let t_finish = clock.advance(DURATION.as_millis() as u64);
        assert!(matches!(core.tick(t_finish).as_slice(), [Effect::Buzzer]));
        assert_eq!(
            journal_marks(&core)[3],
            (
                500 + delay.as_millis() as u64 + DURATION.as_millis() as u64,
                "finish-auto"
            ),
            "{:?}",
            journal_marks(&core)
        );
        assert_eq!(
            core.state().race_elapsed(t_finish),
            DURATION,
            "счётчик заморожен на лимите"
        );

        // REC после гонки — коммит и закрытие.
        assert!(matches!(
            core.rec_pressed(clock.now).as_slice(),
            [Effect::StopRecording { commit: true }]
        ));
        assert!(core.state().finished());
    }

    /// Отмена отсчёта RACE-кнопкой: назад в RecordHead, записи живы.
    #[test]
    fn cancel_countdown_race_button() {
        let clock = FakeClock::new();
        let mut core = test_core(&clock);
        core.rec_pressed(clock.now);
        let t = clock.now;
        core.race_pressed(t);
        assert!(core.state().countdown_active());
        assert!(matches!(
            core.race_pressed(t).as_slice(),
            [Effect::CancelAudio]
        ));
        assert!(!core.state().countdown_active());
        assert!(core.state().rec_on(), "запись продолжается");
        assert!(!core.state().race_started());
    }

    /// Отмена отсчёта REC'ом: запись abort, отложенные события выброшены.
    #[test]
    fn cancel_countdown_rec_aborts() {
        let mut clock = FakeClock::new();
        let mut core = test_core(&clock);
        core.rec_pressed(clock.now);
        core.note_frame("camera-1", Some(clock.advance(300)));
        core.race_pressed(clock.now);
        assert!(matches!(
            core.rec_pressed(clock.now).as_slice(),
            [Effect::CancelAudio, Effect::StopRecording { commit: false }]
        ));
        assert!(
            journal_marks(&core) == vec![(0, "started")],
            "голова не журналирована"
        );
    }

    /// REC посреди гонки: гудок, честный финиш, коммит, Finished.
    #[test]
    fn rec_stop_mid_race_finishes() {
        let mut clock = FakeClock::new();
        let mut core = test_core(&clock);
        core.race_pressed(clock.now);
        let t_start = clock.advance(Duration::from_secs(PRE_BEEP_SECS).as_millis() as u64);
        core.tick(t_start);
        let t_rec = clock.advance(20_000);
        assert!(matches!(
            core.rec_pressed(t_rec).as_slice(),
            [Effect::Buzzer, Effect::StopRecording { commit: true }]
        ));
        let marks = journal_marks(&core);
        assert_eq!(marks.last(), Some(&(23_000, "finish-auto")), "{marks:?}");
        assert!(core.state().finished());
    }

    /// RACE-кнопка в Race: финиш пользователя, запись продолжается (хвост).
    #[test]
    fn race_button_finishes_to_tail() {
        let mut clock = FakeClock::new();
        let mut core = test_core(&clock);
        core.race_pressed(clock.now);
        let t_start = clock.advance(Duration::from_secs(PRE_BEEP_SECS).as_millis() as u64);
        core.tick(t_start);
        let t = clock.advance(12_345);
        assert!(matches!(core.race_pressed(t).as_slice(), [Effect::Buzzer]));
        let marks = journal_marks(&core);
        assert_eq!(marks.last(), Some(&(15_345, "finish-user")), "{marks:?}");
        assert!(core.state().rec_on(), "хвост продолжается");
        assert!(!core.state().finished());
    }

    /// EXIT из каждой фазы: инварианты целостности.
    #[test]
    fn exit_from_every_phase() {
        // Из Race: гудок + финиш + коммит.
        let mut clock = FakeClock::new();
        let mut core = test_core(&clock);
        core.race_pressed(clock.now);
        let t_start = clock.advance(Duration::from_secs(PRE_BEEP_SECS).as_millis() as u64);
        core.tick(t_start);
        let t = clock.advance(5_000);
        assert!(matches!(
            core.exit(t).as_slice(),
            [Effect::Buzzer, Effect::StopRecording { commit: true }]
        ));
        assert_eq!(journal_marks(&core).last(), Some(&(8_000, "finish-user")));

        // Из RecordTail: только коммит (финиш уже в журнале).
        let mut clock = FakeClock::new();
        let mut core = test_core(&clock);
        core.race_pressed(clock.now);
        let t_start = clock.advance(Duration::from_secs(PRE_BEEP_SECS).as_millis() as u64);
        core.tick(t_start);
        core.race_pressed(clock.now);
        assert!(matches!(
            core.exit(clock.now).as_slice(),
            [Effect::StopRecording { commit: true }]
        ));

        // Из RecordHead/Countdown: cancel + abort, без гонки файла нет.
        for race_first in [false, true] {
            let clock = FakeClock::new();
            let mut core = test_core(&clock);
            core.rec_pressed(clock.now);
            if race_first {
                core.race_pressed(clock.now);
            }
            assert!(matches!(
                core.exit(clock.now).as_slice(),
                [Effect::CancelAudio, Effect::StopRecording { commit: false }]
            ));
            assert!(
                journal_marks(&core) == vec![(0, "started")],
                "записи вне гонки не журналируются"
            );
        }
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
            std::env::temp_dir(),
            Duration::ZERO,
            BTreeMap::new(),
            HashMap::new(),
            HashMap::from([("camera-1".to_owned(), test_camera())]),
        );
        core.race_pressed(clock.now);
        let t_start = clock.advance(Duration::from_secs(PRE_BEEP_SECS).as_millis() as u64);
        core.tick(t_start);
        // Час гонки — никакого авто-финиша.
        let t_later = clock.advance(3_600_000);
        assert!(core.tick(t_later).is_empty());
        assert!(core.state().race.is_some(), "гонка без лимита жива");
        assert!(!core.state().race_started() || core.state().race.is_some());
        // Пользователь завершает кнопкой.
        assert!(matches!(
            core.race_pressed(t_later).as_slice(),
            [Effect::Buzzer]
        ));
    }

    /// Повторная запись той же камеры: имя с -r2/r3, все сессии
    /// журналируются.
    #[test]
    fn repeat_recording_gets_suffix() {
        let mut clock = FakeClock::new();
        let mut core = test_core(&clock);
        // Три сессии = пять нажатий (вкл/выкл/вкл/выкл/вкл); стемы
        // собираем только из Record-эффектов.
        let mut stems = Vec::new();
        for _ in 0..5 {
            for effect in core.rec_pressed(clock.now) {
                if let Effect::Record { sessions } = effect {
                    stems.push(sessions[0].1.stem.clone());
                }
            }
        }
        assert_eq!(stems.len(), 3, "три сессии записи");
        assert!(stems[0].ends_with("-camera-1"), "{stems:?}");
        assert!(stems[1].ends_with("-camera-1-r2"), "{stems:?}");
        assert!(stems[2].ends_with("-camera-1-r3"), "{stems:?}");

        // Первый кадр живой сессии привязывается к её имени. До гонки
        // событие откладывается — старт гонки сбрасывает его в журнал.
        // (Aborted-сессии имен не дождались — ядро их выбрасывает с warn.)
        core.note_frame("camera-1", Some(clock.advance(300)));
        core.race_pressed(clock.now);
        let t_start = clock.advance(Duration::from_secs(PRE_BEEP_SECS).as_millis() as u64);
        core.tick(t_start);
        let names: Vec<_> = core
            .journal
            .iter_ms()
            .filter_map(|(_, event)| match event {
                FlightEvent::RecordingStarted { filename, .. } => Some(filename.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            names.len(),
            1,
            "журналируется только живая сессия: {names:?}"
        );
        assert!(names[0].contains("-r3-mjpeg"), "{names:?}");
    }
}

/// Сообщения ядра → GUI.
pub enum Notice {
    /// Состояние эфира изменилось (GUI держит последний).
    State(AirState),
    /// Гонка закрыта: журнал переходит GUI (открытие плееров — GUI,
    /// там egui::Context). Поток после этого завершается.
    EnterDvr { journal: Journal },
}

/// Хэндл гоночного потока для GUI: шлём намерения, забираем уведомления.
pub struct Race {
    intents: Sender<Intent>,
    pub notices: Receiver<Notice>,
}

impl Race {
    pub fn intent(&self, intent: Intent) {
        let _ = self.intents.try_send(intent);
    }
}

pub fn spawn(
    rules: RaceRules,
    captures_dir: std::path::PathBuf,
    pads: BTreeMap<String, PadConfig>,
    assignments: HashMap<String, Pilot>,
    cameras: HashMap<String, CameraConfig>,
    recorders: HashMap<String, Recorder>,
) -> Race {
    let (intent_tx, intent_rx) = crossbeam_channel::bounded(16);
    let (notice_tx, notice_rx) = crossbeam_channel::bounded(64);
    std::thread::spawn(move || {
        let audio = Audio::spawn();
        let mut core = Some(RaceCore::new(
            Instant::now(),
            rules,
            captures_dir,
            BUZZER.duration(),
            pads,
            assignments,
            cameras.clone(),
        ));
        let mut last_air = core.as_ref().unwrap().state();
        let _ = notice_tx.try_send(Notice::State(last_air));

        while let Some(active) = core.as_mut() {
            let now = Instant::now();
            let until_deadline = active
                .next_deadline()
                .map(|d| d.saturating_duration_since(now))
                .unwrap_or(POLL);
            let effects = match intent_rx.recv_timeout(until_deadline.min(POLL)) {
                Ok(Intent::RecPressed) => active.rec_pressed(Instant::now()),
                Ok(Intent::RacePressed) => active.race_pressed(Instant::now()),
                Ok(Intent::Exit) => {
                    let effects = active.exit(Instant::now());
                    apply(&effects, &recorders, &audio);
                    break;
                }
                Err(RecvTimeoutError::Timeout) => active.tick(Instant::now()),
                Err(RecvTimeoutError::Disconnected) => break,
            };
            apply(&effects, &recorders, &audio);

            // Поллинг first_frame_at: edge-детект — в ядре.
            for (id, recorder) in &recorders {
                active.note_frame(id, recorder.record_state().first_frame_at);
            }

            // State — только при изменении: опрос каждые POLL не должен
            // гнать уведомления.
            let air = active.state();
            if air != last_air {
                last_air = air;
                let _ = notice_tx.try_send(Notice::State(air));
            }
            if air.finished() {
                let journal = core.take().unwrap().into_journal();
                let _ = notice_tx.try_send(Notice::EnterDvr { journal });
                break;
            }
        }
    });
    Race {
        intents: intent_tx,
        notices: notice_rx,
    }
}

/// Звуковая программа отсчёта: бипы по секундам, гудок на старте.
/// Задержка до гудка — из ядра (там же сэмплирована пауза сигнала).
fn countdown_steps(delay: Duration) -> Vec<(Duration, audio::Sound)> {
    let interval = delay.saturating_sub(Duration::from_secs(PRE_BEEP_SECS));
    if interval.is_zero() {
        vec![(Duration::ZERO, BUZZER)]
    } else {
        let mut steps: Vec<(Duration, audio::Sound)> = (0..PRE_BEEP_SECS)
            .map(Duration::from_secs)
            .map(|t| (t, STAGE))
            .collect();
        steps.push((delay, BUZZER));
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
            Effect::Buzzer => audio.play(BUZZER),
            Effect::CancelAudio => audio.cancel(),
        }
    }
}
