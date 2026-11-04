//! Экран Flight: совмещает эфир (трансляция/запись/гонка) и DVR
//! (просмотр записи: транспорт, таймлайн, круг повтора). Режим входа —
//! Live (сразу эфир) или Review (сразу просмотр, CLI `laps scrub`). Выход —
//! EXIT в меню. Гоночное состояние живёт в потоке ядра
//! (model::rules + service::race), экран — намерения и рендер снапшотов.

use super::*;

mod laps;
mod scrub;

use crate::config::camera::CameraConfig;
use crate::config::setup::Setup;
use crate::driver::audio::Audio;
use crate::driver::camera::Camera;
use crate::driver::camera::CaptureState;
use crate::driver::dvr::{CAPTURES_DIR, RecordState};
#[cfg(target_os = "linux")]
use crate::driver::keyboard::ExclusiveKeyboard;
use crate::driver::keyboard::{KeyPressed, Keyboard};
use crate::driver::player::{Player, PlayerState};
use crate::gui::flight::scrub::{
    PLAY_SPEEDS, RPT_SIZES, SCROLL_REARM, SCROLL_STEP, SEEK_RELEASE_RAMP, SEEK_SPEEDS, SEEK_TAP,
    ScrollGate, SeekButton, SeekEvent, Timeline, TimelineEvent, show_events,
};
use crate::gui::ruler::{CELL_SIZE, IntoRuler, cell_x, cell_y};
use crate::gui::view::{Letterbox, TransportButton, hint_badge, hint_badge_corner};
use crate::model::flight::{
    Feat, Interval, Journal, JournalEntry, LapEvent, LapLine, RaceClip, Stamp, compute_feat,
    feat_places, latest_journal,
};
use crate::model::pilot::Pilot;
use crate::model::rules::RaceRules;
use crate::remote::Response;
use crate::service::key_matcher::{KeyMatch, KeyMatcher};
use crate::service::race::{Notice, Race, RaceStage};
use crate::service::transport::{Circle, Transport};
use crossbeam_channel as crossbeam;
use std::collections::HashMap;
use std::fs;
use std::mem;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_PADS: usize = 4;
const GRID_WIDTH: isize = 160;
const LEFT_MARGIN: isize = 4;
const RIGHT_MARGIN: isize = 4;
const TOP_BAR_ROWS: isize = 2;

/// Палитра кнопок, таймера и лапсов — в style.rs (на местах — `style::`).

/// Толеран отставания показанного кадра мастера от позиции транспорта,
/// в пределах которого циферблат верен картинке (лень кадра + декод на
/// обычных скоростях; шаттл уходит дальше — цифра честно = курсор).
const DIAL_LAG_MS: u64 = 150;

/// Единый «счётчик» перерисовок там, где кадры не гонят UI сами (мёртвый
/// эфир, счётчик гонки без картинки, автономный транспорт): минимум
/// ~23 Гц. Повторный вызов перевооружает таймер egui, состояние не
/// нужно.
const UI_TICK: Duration = Duration::from_millis(43);

/// Окно вспышки свежего события, журнальные мс: (от метки, до метки),
/// знаковые. DVR — симметрично; в эфире — только вперёд и дольше:
/// латентность доставки дистиллята съедает начало окна. Окно в
/// домене воспроизведения — скорость DVR масштабирует его
/// реальную длительность (желаемо).
const LAP_FLASH_INTERVAL_DVR: (i64, i64) = (-100, 100);
const LAP_FLASH_INTERVAL_LIVE: (i64, i64) = (0, 200);

/// Машина состояний экрана: данные фаз инкапсулированы в вариантах.
/// Гоночные фазы эфира живут в ядре (service::race::RaceStage) — здесь
/// только разделение «эфир vs DVR».
enum Mode {
    /// Эфир без гоночного потока: захваты живы, REC/RACE ещё не нажаты.
    /// Журнал и ядро рождаются первым намерением (Idle → Live).
    Idle(Idle),
    Live(Live),
    Review(Review),
}

/// Данные Idle-эфира: только захваты (как в Live, но без race, state
/// и keepawake).
struct Idle {
    webcams: HashMap<String, Camera>,
    /// Матчер клавиатурных биндингов фазы (чистый: активный пад —
    /// из выделения экрана); события пока только логируются.
    key_matcher: KeyMatcher,
    /// Main-клавиатура (фокусная, из egui) и канал клавиатур сетапа.
    /// Живут с фазой: при Idle → Live уезжают в Race; новые строятся
    /// при каждом входе в Idle (старые в Race не возвращаются).
    main_keyboard: Keyboard,
    keyboard_rx: crossbeam::Receiver<KeyPressed>,
    /// Состояния подцепленных клавиатур (connected): опрос — у GUI,
    /// плашки дисконнекта — дальше.
    #[allow(dead_code)]
    #[cfg(target_os = "linux")]
    exclusive_keyboards: HashMap<String, ExclusiveKeyboard>,
}

impl Default for Idle {
    /// Пустышка для обмена через mem::replace: пустой канал (attach не
    /// зовём — потоков нет), немедленно заменяется вызывающим кодом.
    fn default() -> Self {
        let (keyboard_tx, keyboard_rx) = crossbeam::unbounded();
        Self {
            webcams: HashMap::new(),
            key_matcher: KeyMatcher::default(),
            main_keyboard: Keyboard::new(&keyboard_tx),
            keyboard_rx,
            #[cfg(target_os = "linux")]
            exclusive_keyboards: HashMap::new(),
        }
    }
}

/// Данные эфира: гоночный поток, последний снапшот его состояния,
/// захваты камер, клавиатуры. Дроп структуры гасит эфир целиком
/// (камеры останавливаются в своём Drop, keepawake отпускается,
/// поток гонки завершается по обрыву канала).
struct Live {
    /// Гоночный поток: намерения туда, уведомления оттуда.
    race: Race,
    /// pad_id → дистилят кругов (последний Notice::Laps): строки журнала
    /// + текущий круг (бегущее время рендер считает по `since`).
    laps: HashMap<String, Vec<(Stamp, LapLine, Vec<(Stamp, LapEvent)>)>>,
    /// Момент рождения (REC) на часах GUI: «сейчас» = elapsed.
    rec_at: Instant,
    /// Ноль оси журнала сессии (от spawn): кадрит elapsed в Stamp.
    axis_zero: Stamp,
    /// Последний снапшот состояния эфира (рендер кнопок/счётчиков).
    state: RaceStage,
    rules: RaceRules,
    webcams: HashMap<String, Camera>,
    /// Main-клавиатура: уехала сюда из Idle вместе с каналом клавиатур.
    main_keyboard: Keyboard,
    /// Состояния клавиатур — вместе с main из Idle.
    #[allow(dead_code)]
    #[cfg(target_os = "linux")]
    exclusive_keyboards: HashMap<String, ExclusiveKeyboard>,
    /// Подавление сна: держится ради Drop (inhibit снимается при
    /// разрушении), чтений нет.
    #[allow(dead_code)]
    awake: Option<keepawake::KeepAwake>,
}

/// Данные DVR: просмотр записи. «Открытие» — не состояние экрана:
/// интерактив включён ровно тогда, когда готовы плееры
/// (`players_ready()`), это производное от их состояния.
/// Выделение в DVR: пусто / пад / событие на паде. Инвариант в
/// типе: событие без пада не выражается.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Selection {
    None,
    Pad(String),
    Event(String, Stamp),
}

impl Selection {
    /// Пад выделения (Pad или Event) или None.
    fn pad(&self) -> Option<&str> {
        match self {
            Selection::Pad(p) | Selection::Event(p, _) => Some(p),
            Selection::None => None,
        }
    }
}

struct Review {
    /// Журнал полёта — владение в GUI (правки DVR — здесь).
    journal: Journal,
    /// Правила гонки из заголовка журнала: нужны вне ядра (имена
    /// кругов — холшот).
    rules: RaceRules,
    /// Матчер клавиатурных биндингов фазы: тогл активного пада и
    /// глобальные биндинги в Review; события пока только логируются
    /// (правки журнала — следующим шагом). При тёплом входе сеян
    /// активным падом из гонки.
    key_matcher: KeyMatcher,
    /// Main-клавиатура (фокусная, egui) и локальный канал её событий:
    /// ввод для редактирования, матчит матчер фазы.
    main_keyboard: Keyboard,
    keyboard_rx: crossbeam::Receiver<KeyPressed>,
    /// camera_id → плеер записи.
    players: HashMap<String, Player>,
    /// Транспорт DVR: общая головка в домене сессии записи. Плееры —
    /// доставщики: каждый кадр позиция и скорость уходят в плееры
    /// синками, worker'ы экстраполируют на своих часах.
    transport: Transport,
    /// Мастер-камера: её показанный кадр — источник позиции циферблата
    /// (жёсткое соответствие цифры и картинки). Первая по порядку сетапа
    /// камера с файлом; переключаемая в рантайме.
    master: String,
    scrub: Scrub,
}

/// Состояние скраббинга DVR: таймлайн, кнопки перемотки, шкалы
/// скоростей, гейты скролла.
struct Scrub {
    timeline: Timeline,
    /// Перемотка REW/FF: состояние удержания.
    rew: SeekButton,
    ff: SeekButton,
    /// Индексы шкал скоростей (скролл над кнопками).
    seek_speed: usize,
    play_speed: usize,
    rpt_size: usize,
    vscroll: ScrollGate,
}

/// Собранный кадр: рендер читает только его. prepare() заполняет по
/// фазе — в рендере ни одного фазового Option.
struct FrameContents {
    pads: Vec<PadContents>,
    transport: TransportContents,
    rec: RecContents,
    timer: TimerContents,
    /// Таймлайн (мс-домен; в дроби переводит рендер). None в эфире.
    timeline: Option<TimelineContents>,
    /// Курсор кругов: метка «сейчас» (последний дистиллят; всё, что
    /// было, активно; свежее событие вспыхивает на 100 мс).
    laps_cursor: Option<Stamp>,
    /// Старт гонки: начало первого круга (для «ещё не начался»).
    /// None в эфире (GUI оси журнала не видит; не используется
    /// без курсора).
    laps_race_start: Option<Stamp>,
    /// Отсчёт в пэдах: Some(dim) — фаза Countdown, dim — тот же сигнал
    /// мигания, что у основного таймера (вторая половина секунды).
    /// None — не отсчёт, пэды рисуют круги/пустоту.
    countdown: Option<bool>,
    /// Выделение кругов (DVR) — клик: голубая обводка события.
    laps_active: Selection,
    /// Окно вспышки свежего события (от метки, до метки), знаковые
    /// смещения мс: DVR симметрично, эфир — вперёд и дольше.
    laps_flash: (i64, i64),
    /// События выделенного пада для строки над таймлайном (DVR):
    /// пусто без выделенного пада.
    timeline_events: Vec<(Stamp, LapEvent)>,
    /// Клики по ячейкам кругов: только DVR.
    laps_clickable: bool,
    /// Холшот: первый круг нулевой («HS»); выключен — нумерация
    /// с «1)».
    holeshot_enabled: bool,
}

/// Контент одного пада: всё для его рамки и картинки.
struct PadContents {
    /// Идентификатор пада (ключ `Setup.pads`): выделение кругов
    /// адресуется (пад, мс).
    pad_id: String,
    /// Пад активен (глобальные клавиши): голубая рамка вокруг контента
    /// колонки (вьюфайндер и бокс кругов).
    active: bool,
    label: String,
    pilot: String,
    color: egui::Color32,
    /// UV-вьюпорт камеры.
    viewport: egui::Rect,
    image: viewfinder::ViewfinderContents,
    /// Alt: эффективное разрешение ("1080p") или "error".
    res_badge: Option<String>,
    /// Дистилят кругов: (метка конца круга, строка, события с метками);
    /// текущий круг идёт последним (бегущее время считается рендером
    /// по `since`). Метки — для раскраски «ещё не случилось» серым.
    laps: Vec<(Stamp, LapLine, Vec<(Stamp, LapEvent)>)>,
    /// Достижение (фит) пада — строка под кругами. None — не
    /// показывать (эфир без единого круга).
    feat: Option<Feat>,
    /// Место в гонке: (от, до) — диапазон при ничьей, равные —
    /// одинаковое. None — фита нет, в гонке не участвует.
    place: Option<(u32, u32)>,
}

/// Состояние одной кнопки для кадра. Иконки и цвета — часть вёрстки
/// (не меняются), какой цвет применится решает виджет по enabled/active.
/// На кадр клика рендер добавляет вспышку через response.clicked().
/// badge — alt-баббл над кнопкой (многострочность внутри строки,
/// разделитель '\n'; None — без баббла).
struct ButtonContents {
    enabled: bool,
    active: bool,
    badge: Option<String>,
}

impl ButtonContents {
    /// Мёртвая кнопка (не кликабельна, без баббла): эфирный транспорт,
    /// REC/RACE в DVR. Константа, а не fn: None конструируется без
    /// пейлоада, String в const-выражении не создаётся.
    const DEAD: ButtonContents = ButtonContents {
        enabled: false,
        active: false,
        badge: None,
    };
}

/// Кнопки транспорта DVR — по контенту на кнопку.
struct TransportContents {
    rew: ButtonContents,
    play: ButtonContents,
    ff: ButtonContents,
    rpt: ButtonContents,
}

/// REC/RACE — по контенту на кнопку (бейджи эфира висят на них).
struct RecContents {
    rec: ButtonContents,
    race: ButtonContents,
}

/// TIMER: готовый текст и признак приглушения (мигание/нули).
struct TimerContents {
    text: String,
    dim: bool,
}

/// Таймлайн в мс-домене (журнальном): окно записи — [start, end].
struct TimelineContents {
    /// Окно таймлайна (полоска записи): шкала, на которой ходит голова.
    window: Interval,
    pos: Stamp,
    /// Границы гонки (журнал): белый фрагмент трека.
    race: Option<Interval>,
    /// Момент RaceCountdown: точка на треке.
    marker: Option<Stamp>,
    /// Круг зацикливания: окно (центр ± радиус).
    circle: Option<Interval>,
}

pub struct Flight {
    setup: Setup,
    assignments: HashMap<String, Pilot>,
    /// Правила гонки: нужны в момент рождения потока (Idle → Live).
    rules: RaceRules,
    /// Аудио-плеер: уровень приложения (поток-плеер живёт, пока жив
    /// хэндл), гоночные сессии получают клон — без переоткрытия
    /// устройства на каждый REC.
    audio: Audio,
    /// Показываемые камеры: пады с пилотами (сейчас ≤ MAX_PADS) → их
    /// камеры, дедуп по camera_id (несколько падов смотрят в одну).
    /// Материализованы ради owned-среза для гоночного потока и дедупа
    /// захватов; снятие лимита MAX_PADS ролей не меняет — только фильтр
    /// в shown_pads.
    shown_cameras: HashMap<String, CameraConfig>,
    res: Option<Res>,
    /// Запрошен выход (remote/кнопка/⌘W): ядру Intent::Exit уходит
    /// сразу, флаг — для навигации в меню из update (нужен Navigator).
    exit_requested: bool,
    header: header::Header,
    /// Выделение экрана: пад и, возможно, событие на нём. Сессионное —
    /// переживает переходы фаз; в эфире — вид активного пада ядра
    /// (нотисами), в GUI-фазах — власть (клики, клавиши 1–8/0).
    selection: Selection,
    phase: Mode,
}

impl From<Box<Flight>> for State {
    fn from(val: Box<Flight>) -> Self {
        State::Flight(val)
    }
}

impl Flight {
    /// Главный экран — сразу в эфире: захват стартует в конструкторе.
    pub fn live(
        ctx: &egui::Context,
        setup: Setup,
        assignments: HashMap<String, Pilot>,
        rules: RaceRules,
    ) -> Self {
        let shown_cameras = filter_cameras(&setup, &assignments);
        // Захваты — данные фазы Idle; поток гонки и keepawake рождаются
        // первым REC/RACE (Idle → Live). Мапа shown_cameras уже
        // продедуплена по camera_id — открываем захват по одному на камеру.
        let mut webcams = HashMap::new();
        for (id, camera) in &shown_cameras {
            webcams.insert(id.clone(), Camera::open(camera.clone(), ctx.clone()));
        }
        log::info!("flight: webcams starting: {}", webcams.len());
        let (keyboard_tx, keyboard_rx) = crossbeam::unbounded();
        #[cfg(target_os = "linux")]
        let exclusive_keyboards = attach_exclusive_keyboards(&setup, &keyboard_tx);
        let main = Keyboard::new(&keyboard_tx);
        let key_matcher = KeyMatcher::new(&setup);
        Self {
            setup,
            assignments,
            rules,
            audio: Audio::spawn(),
            shown_cameras,
            res: None,
            exit_requested: false,
            header: header::Header::new(),
            selection: Selection::None,
            phase: Mode::Idle(Idle {
                webcams,
                key_matcher,
                main_keyboard: main,
                keyboard_rx,
                #[cfg(target_os = "linux")]
                #[cfg(target_os = "linux")]
                exclusive_keyboards,
            }),
        }
    }

    /// Idle → Live по первому REC/RACE: рождается гоночный поток (ядро
    /// рождается REC'ом; RACE-рождение — намерением сразу после spawn),
    /// keepawake.
    fn enter_live(&mut self, ctx: &egui::Context, start_race: bool) {
        if !matches!(self.phase, Mode::Idle(_)) {
            return;
        }
        // Пустышка для обмена: немедленно заменяется ниже.
        let Mode::Idle(idle) = mem::replace(&mut self.phase, Mode::Idle(Idle::default())) else {
            unreachable!()
        };
        let awake = keepawake::Builder::default()
            .display(true)
            .reason("flight")
            .app_name("Laps")
            .create()
            .map(Some)
            .unwrap_or_else(|e| {
                log::warn!("screen inhibit failed: {e}");
                None
            });
        let recorders: HashMap<_, _> = idle
            .webcams
            .iter()
            .map(|(id, camera)| (id.clone(), camera.recorder()))
            .collect();
        // Выделение экрана сеет ядро гонки (зеркало — нотисами).
        let active_pad = self.selection.pad().map(str::to_owned);
        // Пробуждение GUI по уведомлениям потока: без него state
        // обновлялся бы только по кадрам камер (мертвый эфир висел бы
        // в Before до первого repaint).
        let wake_ctx = ctx.clone();
        let wake = Arc::new(move || wake_ctx.request_repaint());
        let (race, axis_zero) = Race::spawn(
            self.rules,
            PathBuf::from(CAPTURES_DIR),
            self.setup.pads.clone(),
            self.assignments.clone(),
            self.shown_cameras.clone(),
            KeyMatcher::new(&self.setup),
            recorders,
            idle.keyboard_rx,
            wake,
            self.audio.clone(),
            active_pad.clone(),
        );
        // Рождение кнопкой RACE: отсчёт стартует сразу (намерение ждёт
        // поток в очереди на рождение).
        if start_race {
            race.race_pressed();
        }
        self.phase = Mode::Live(Live {
            race,
            rec_at: Instant::now(),
            axis_zero,
            laps: HashMap::new(),
            state: RaceStage::Before,
            rules: self.rules,
            webcams: idle.webcams,
            main_keyboard: idle.main_keyboard,
            #[cfg(target_os = "linux")]
            exclusive_keyboards: idle.exclusive_keyboards,
            awake,
        });
    }

    /// Live → Idle: REC-off до старта гонки (ядро дропнуто потоком,
    /// журнала на диске нет). Остаются только захваты.
    fn back_to_idle(&mut self) {
        if !matches!(self.phase, Mode::Live(_)) {
            return;
        }
        // Пустышка для обмена: немедленно заменяется ниже.
        let Mode::Live(live) = mem::replace(&mut self.phase, Mode::Idle(Idle::default())) else {
            unreachable!()
        };
        let (keyboard_tx, keyboard_rx) = crossbeam::unbounded();
        #[cfg(target_os = "linux")]
        let exclusive_keyboards = attach_exclusive_keyboards(&self.setup, &keyboard_tx);
        let main = Keyboard::new(&keyboard_tx);
        self.phase = Mode::Idle(Idle {
            webcams: live.webcams,
            key_matcher: KeyMatcher::new(&self.setup),
            main_keyboard: main,
            keyboard_rx,
            #[cfg(target_os = "linux")]
            exclusive_keyboards,
        });
    }

    /// Режим просмотра (CLI `laps scrub`) — без эфира, плееры открыты
    /// на старте, экран сразу в DVR (opening-транзит).
    pub fn dvr(ctx: &egui::Context, setup: Setup, assignments: HashMap<String, Pilot>) -> Self {
        // Холодный вход: последний журнал из captures/ → DVR; журнала
        // нет — деградация на последние записи камер (пустая рамка).
        let journal = latest_journal(Path::new(CAPTURES_DIR))
            .and_then(|path| fs::read_to_string(&path).ok())
            .and_then(|yaml| {
                let (header, entries) = Journal::parse_stream(&yaml);
                header.map(|header| {
                    let mut journal = Journal::new(
                        header.date_time,
                        header.pads,
                        header.assignments,
                        header.rules,
                    );
                    for (ms, entry) in entries {
                        journal.push(ms, entry);
                    }
                    journal
                })
            });
        let shown_cameras = filter_cameras(&setup, &assignments);
        let phase = Mode::Review(Review::open(ctx, journal, &setup));
        Self {
            setup,
            assignments,
            rules: RaceRules::default(),
            audio: Audio::spawn(),
            shown_cameras,
            res: None,
            exit_requested: false,
            header: header::Header::new(),
            selection: Selection::None,
            phase,
        }
    }

    /// Сборка кадра: единственный матч по фазе. Рендер читает только
    /// FrameContents; мутации драйверов (update_frame, синк транспорта) — здесь.
    fn prepare(&mut self, ctx: &egui::Context, alt: bool) -> FrameContents {
        // Уведомления гонки (Live) и переход в DVR по закрытию.
        let mut enter_review = None;
        let mut back_to_idle = false;
        if let Mode::Live(l) = &mut self.phase {
            while let Ok(notice) = l.race.notices.try_recv() {
                match notice {
                    Notice::State(state) => l.state = state,
                    Notice::EnterReview { journal } => enter_review = Some(journal),
                    Notice::BackToIdle => back_to_idle = true,
                    Notice::Laps { pad_id, lines } => {
                        l.laps.insert(pad_id, lines);
                    }
                    Notice::ActivePad(pad) => {
                        self.selection = match &pad {
                            Some(p) => Selection::Pad(p.clone()),
                            None => Selection::None,
                        };
                    }
                }
            }
            let streaming = l
                .webcams
                .values()
                .any(|w| w.capture_state() == CaptureState::Live);
            if !streaming
                && (l.state.countdown_active() || matches!(l.state, RaceStage::Running { .. }))
            {
                // Самовзвод перерисовок счётчика гонки — только в живых
                // фазах без кадров (в DVR кадры гонят перерисовки сами).
                ctx.request_repaint_after(UI_TICK);
            }
        }
        if let Some(journal) = enter_review {
            // Гонка закрыта: строим Review; дроп Live гасит эфир целиком.
            self.enter_review(ctx, journal);
        }
        if back_to_idle {
            // REC-off до старта гонки: поток завершился, остаёмся с
            // захватами в Idle (новая сессия — новое нажатие REC/RACE).
            self.back_to_idle();
        }
        match &mut self.phase {
            Mode::Idle(i) => Self::prepare_idle(
                i,
                &self.setup,
                &self.assignments,
                &self.shown_cameras,
                &mut self.selection,
                self.rules,
                ctx,
                alt,
            ),
            Mode::Live(l) => {
                let active_pad = self.selection.pad().map(str::to_owned);
                Self::prepare_live(
                    l,
                    &self.setup,
                    &self.assignments,
                    &self.shown_cameras,
                    active_pad.as_deref(),
                    alt,
                )
            }
            Mode::Review(d) => Self::prepare_dvr(
                d,
                &self.setup,
                &self.assignments,
                &mut self.selection,
                ctx,
                alt,
            ),
        }
    }

    /// Alt-баббл над REC: строка на камеру (shown_cameras продедуплены
    /// по camera_id — несколько падов на одну камеру дают одну строку).
    /// target — целевой fps (dvr.frame-rate, иначе camera); «error» при
    /// мёртвом потоке и при провале записи в REC-он.
    fn fps_lines(
        webcams: &HashMap<String, Camera>,
        shown_cameras: &HashMap<String, CameraConfig>,
        rec_on: bool,
    ) -> Vec<String> {
        shown_cameras
            .iter()
            .map(|(camera_id, cfg)| {
                let target = cfg.dvr.frame_rate.unwrap_or(cfg.frame_rate);
                match webcams.get(camera_id) {
                    None => format!("\u{f16df} {target}"),
                    Some(w) if w.capture_state() == CaptureState::Dead => {
                        "\u{f03d} error".to_owned()
                    }
                    Some(w) if rec_on && matches!(w.record_state(), RecordState::Failed) => {
                        "\u{f16df} error".to_owned()
                    }
                    Some(w) => match w.record_state() {
                        RecordState::Writing { fps, .. } if fps > 0.0 => {
                            format!("\u{f16df} {:.0}fps", fps)
                        }
                        _ => format!("\u{f16df} {target}"),
                    },
                }
            })
            .collect()
    }

    /// Контент падов из захватов: картинки (с update_frame) и alt-бейджи
    /// разрешений (только для камер активных падов). Рендерятся все пады
    /// (неназначенные — пустые рамки).
    #[allow(clippy::too_many_arguments)]
    fn prepare_pads(
        webcams: &mut HashMap<String, Camera>,
        setup: &Setup,
        assignments: &HashMap<String, Pilot>,
        shown_cameras: &HashMap<String, CameraConfig>,
        laps: &HashMap<String, Vec<(Stamp, LapLine, Vec<(Stamp, LapEvent)>)>>,
        active_pad: Option<&str>,
        alt: bool,
    ) -> Vec<PadContents> {
        setup
            .pads
            .iter()
            .take(MAX_PADS)
            .map(|(pad_id, pad)| {
                let image = match webcams.get_mut(&pad.fpv.camera_id) {
                    Some(webcam) => {
                        let capture_state = webcam.capture_state();
                        webcam.update_frame();
                        match capture_state {
                            CaptureState::Live => {
                                viewfinder::ViewfinderContents::Texture(webcam.texture())
                            }
                            CaptureState::Dead => viewfinder::ViewfinderContents::Error,
                            CaptureState::Starting => viewfinder::ViewfinderContents::Pending,
                        }
                    }
                    None => viewfinder::ViewfinderContents::Off,
                };
                // Alt-бейдж: разрешение показываем только для камер
                // активных падов (как раньше — по shown_cameras).
                let res_badge = if alt && shown_cameras.contains_key(&pad.fpv.camera_id) {
                    let res = match webcams.get(&pad.fpv.camera_id) {
                        Some(w) if w.capture_state() == CaptureState::Dead => None,
                        Some(w) => w.resolution().or_else(|| {
                            setup.cameras.get(&pad.fpv.camera_id).map(|c| c.resolution)
                        }),
                        None => setup.cameras.get(&pad.fpv.camera_id).map(|c| c.resolution),
                    };
                    Some(match res {
                        Some(r) => {
                            let lines =
                                r.height as f32 * (pad.fpv.viewport.bottom - pad.fpv.viewport.top);
                            format!("{}p", lines.round() as u32)
                        }
                        None => "error".to_owned(),
                    })
                } else {
                    None
                };
                PadContents {
                    pad_id: pad_id.clone(),
                    active: active_pad.is_some_and(|a| a == pad_id),
                    label: pad.label.clone(),
                    pilot: assignments
                        .get(pad_id)
                        .map_or("", |pilot| pilot.name.as_str())
                        .to_owned(),
                    color: pad.color.to_color32(),
                    viewport: pad.fpv.viewport.to_rect(),
                    image,
                    res_badge,
                    laps: laps.get(pad_id).cloned().unwrap_or_default(),
                    feat: None,
                    place: None,
                }
            })
            .collect()
    }

    /// Кадр эфира: контент из захватов, кнопки REC/RACE, счётчик гонки.
    fn prepare_live(
        l: &mut Live,
        setup: &Setup,
        assignments: &HashMap<String, Pilot>,
        shown_cameras: &HashMap<String, CameraConfig>,
        active_pad: Option<&str>,
        alt: bool,
    ) -> FrameContents {
        let state = l.state;
        let rec_on = state.rec_on();
        let race_on = state.countdown_active() || matches!(state, RaceStage::Running { .. });
        let now = Instant::now();
        let mut pads = Self::prepare_pads(
            &mut l.webcams,
            setup,
            assignments,
            shown_cameras,
            &l.laps,
            active_pad,
            alt,
        );
        // Фиты: в эфире — с первого успешного круга; после финиша —
        // всегда (рендер пишет «0/ -.---» при нуле).
        let race_finished = matches!(state, RaceStage::After { .. } | RaceStage::Finished);
        for pad in &mut pads {
            let has_success = pad
                .laps
                .iter()
                .any(|(_, line, _)| matches!(line, LapLine::Success { .. }));
            if race_finished || has_success {
                pad.feat = Some(compute_feat(l.rules.feat_kind, None, &pad.laps));
            }
        }
        Self::assign_places(&mut pads);
        let fps_lines = if alt {
            Self::fps_lines(&l.webcams, shown_cameras, rec_on)
        } else {
            Vec::new()
        };
        // Alt над RACE: Σ / остаток со знаком минус (без лимита нечего
        // показывать ни до, ни после старта).
        let duration = l.rules.max_duration;
        let race_hint = if alt {
            match (state, duration) {
                (RaceStage::Running { start_at }, Some(duration)) => Some(format!(
                    "-{}",
                    mmss(duration.saturating_sub(start_at.elapsed()))
                )),
                (_, Some(duration)) => Some(format!("Σ{}", mmss(duration))),
                _ => None,
            }
        } else {
            None
        };
        // Счётчик гонки: мигание последней секунды отсчёта, нули пристыжены.
        // Микро-таймеры пэдов: тот же сигнал мигания, что у основного
        // таймера.
        let countdown = match state {
            RaceStage::Countdown { start_at } => {
                Some(start_at.saturating_duration_since(now).as_millis() as u64 % 1000 >= 500)
            }
            _ => None,
        };
        let (text, dim) = {
            let blink_off = match state {
                RaceStage::Countdown { start_at } => {
                    start_at.saturating_duration_since(now).as_millis() as u64 % 1000 >= 500
                }
                _ => false,
            };
            let total = state.race_elapsed(now).as_millis() as u64;
            let text = format!(
                "{:02}:{:02}.{:03}",
                total / 60_000,
                (total / 1_000) % 60,
                total % 1_000
            );
            let zeros_dim = state.race_elapsed(now).is_zero() && !state.countdown_active();
            (text, blink_off || zeros_dim)
        };
        FrameContents {
            pads,
            transport: TransportContents {
                rew: ButtonContents::DEAD,
                play: ButtonContents::DEAD,
                ff: ButtonContents::DEAD,
                rpt: ButtonContents::DEAD,
            },
            rec: RecContents {
                rec: ButtonContents {
                    enabled: state.rec_available(),
                    active: rec_on,
                    badge: if fps_lines.is_empty() {
                        None
                    } else {
                        Some(fps_lines.join("\n"))
                    },
                },
                race: ButtonContents {
                    enabled: state.race_available(),
                    active: race_on,
                    badge: race_hint,
                },
            },
            timer: TimerContents { text, dim },
            timeline: None,
            // Курсор кругов: «сейчас» на оси журнала (часы GUI) — всё,
            // что было, активно; свежее событие вспыхивает на 100 мс.
            laps_cursor: Some(l.axis_zero.plus_ms(l.rec_at.elapsed().as_millis() as i64)),
            laps_race_start: None,
            countdown,
            laps_active: Selection::None,
            laps_flash: LAP_FLASH_INTERVAL_LIVE,
            timeline_events: Vec::new(),
            laps_clickable: false,
            holeshot_enabled: l.rules.holeshot_enabled,
        }
    }

    /// Кадр Idle-эфира: как Live, но REC/RACE живы (первое нажатие
    /// родит поток), счётчик погашен.
    fn prepare_idle(
        i: &mut Idle,
        setup: &Setup,
        assignments: &HashMap<String, Pilot>,
        shown_cameras: &HashMap<String, CameraConfig>,
        selection: &mut Selection,
        rules: RaceRules,
        ctx: &egui::Context,
        alt: bool,
    ) -> FrameContents {
        // Клавиатуры (Idle): нажатия матчит матчер фазы — активный пад
        // и глобальные биндинги работают уже здесь; события пока только
        // логируются (исполнение — в гонке). В Live канал уехал в Race,
        // там его дренит поток.
        let _ = drain_keyboard(&i.keyboard_rx, &i.key_matcher, selection);
        // Клавиатурный дрен выше живёт на этом счётчике, пока эфир
        // мёртв (живой — кадры гонят перерисовки сами).
        if !i
            .webcams
            .values()
            .any(|w| w.capture_state() == CaptureState::Live)
        {
            ctx.request_repaint_after(UI_TICK);
        }
        let no_laps = HashMap::new();
        let pads = Self::prepare_pads(
            &mut i.webcams,
            setup,
            assignments,
            shown_cameras,
            &no_laps,
            selection.pad(),
            alt,
        );
        let fps_lines = if alt {
            Self::fps_lines(&i.webcams, shown_cameras, false)
        } else {
            Vec::new()
        };
        FrameContents {
            pads,
            transport: TransportContents {
                rew: ButtonContents::DEAD,
                play: ButtonContents::DEAD,
                ff: ButtonContents::DEAD,
                rpt: ButtonContents::DEAD,
            },
            rec: RecContents {
                rec: ButtonContents {
                    enabled: true,
                    active: false,
                    badge: if fps_lines.is_empty() {
                        None
                    } else {
                        Some(fps_lines.join("\n"))
                    },
                },
                race: ButtonContents {
                    enabled: true,
                    active: false,
                    badge: None,
                },
            },
            timer: TimerContents {
                text: "00:00.000".to_owned(),
                dim: true,
            },
            timeline: None,
            laps_cursor: None,
            laps_race_start: None,
            countdown: None,
            laps_active: Selection::None,
            laps_flash: LAP_FLASH_INTERVAL_LIVE,
            timeline_events: Vec::new(),
            laps_clickable: false,
            holeshot_enabled: rules.holeshot_enabled,
        }
    }

    /// Контент падов DVR: кадры плееров (с гэп-покрытием), камеры без
    /// файла — Error. Плееры здесь же получают update_frame.
    fn prepare_dvr_pads(
        d: &mut Review,
        setup: &Setup,
        assignments: &HashMap<String, Pilot>,
        active_pad: Option<&str>,
        alt: bool,
    ) -> Vec<PadContents> {
        let clip = Some(RaceClip::from_events(d.journal.iter()));
        let feat_kind = d.rules.feat_kind;
        let race_start = d.journal.race_started();
        // Позиция транспорта переносится на ось журнала (нули совпадают:
        // REC родил и журнал, и шкалу сессии).
        let pos = d
            .journal
            .record_started()
            .plus_ms(d.transport.position() as i64);
        for player in d.players.values_mut() {
            player.update_frame();
        }
        setup
            .pads
            .iter()
            .take(MAX_PADS)
            .map(|(pad_id, pad)| {
                let camera_id = &pad.fpv.camera_id;
                let image = match d.players.get(camera_id) {
                    Some(p) => match p.open_state() {
                        // Разрывы записи в журнальном домене: файл не
                        // начался/закончился — серый глиф (Pending).
                        PlayerState::Ready => {
                            if Self::clip_gap(&clip, &d.players, camera_id, pos) {
                                viewfinder::ViewfinderContents::Pending
                            } else {
                                viewfinder::ViewfinderContents::Texture(p.texture())
                            }
                        }
                        PlayerState::Failed(_) => viewfinder::ViewfinderContents::Error,
                        PlayerState::Opening => viewfinder::ViewfinderContents::Pending,
                    },
                    // Камера без файла (запись не состоялась): красный
                    // плюс по центру, как у мёртвой камеры.
                    None => viewfinder::ViewfinderContents::Error,
                };
                let res_badge = if alt {
                    let res = match d.players.get(camera_id) {
                        Some(p) => p
                            .resolution()
                            .or_else(|| setup.cameras.get(camera_id).map(|c| c.resolution)),
                        None => setup.cameras.get(camera_id).map(|c| c.resolution),
                    };
                    res.map(|r| {
                        let lines =
                            r.height as f32 * (pad.fpv.viewport.bottom - pad.fpv.viewport.top);
                        format!("{}p", lines.round() as u32)
                    })
                } else {
                    None
                };
                PadContents {
                    pad_id: pad_id.clone(),
                    active: active_pad.is_some_and(|a| a == pad_id),
                    label: pad.label.clone(),
                    pilot: assignments
                        .get(pad_id)
                        .map_or("", |pilot| pilot.name.as_str())
                        .to_owned(),
                    color: pad.color.to_color32(),
                    viewport: pad.fpv.viewport.to_rect(),
                    image,
                    res_badge,
                    // Скраббинг: чистый дистилят (без текущего круга).
                    // Фит — всегда.
                    laps: d.journal.lap_infos(pad_id),
                    feat: Some(compute_feat(
                        feat_kind,
                        race_start,
                        &d.journal.lap_infos(pad_id),
                    )),
                    place: None,
                }
            })
            .collect()
    }

    /// Места: сравнение фитов показанных падов (feat_places),
    /// результат — в pad.place. Пады без фита не участвуют.
    fn assign_places(pads: &mut [PadContents]) {
        let feats: Vec<&Feat> = pads.iter().filter_map(|p| p.feat.as_ref()).collect();
        let places = feat_places(&feats);
        let mut places = places.into_iter();
        for pad in pads {
            if pad.feat.is_some() {
                pad.place = places.next();
            }
        }
    }

    /// Кадр DVR: контент из плееров, транспорт, шкала, позиция.
    fn prepare_dvr(
        d: &mut Review,
        setup: &Setup,
        assignments: &HashMap<String, Pilot>,
        selection: &mut Selection,
        ctx: &egui::Context,
        alt: bool,
    ) -> FrameContents {
        // Клавиатуры (Review): ввод — main-клавиатура (пумп в update),
        // матчит матчер фазы (пад — из выделения); события пока только
        // логируются (правки журнала — следующим шагом).
        for matched in drain_keyboard(&d.keyboard_rx, &d.key_matcher, selection) {
            match matched {
                KeyMatch::Delete | KeyMatch::Backspace => {
                    if let Selection::Event(pad, ms) = selection.clone() {
                        if d.delete_selected(&pad, ms) {
                            *selection = Selection::Pad(pad);
                        }
                    }
                }
                // Судейский сигнал — правка журнала (DVR): события пада
                // в окне вспышки заменяются одним по курсору.
                KeyMatch::Signal { pad, event } => {
                    let cursor = d
                        .journal
                        .record_started()
                        .plus_ms(d.transport.position() as i64);
                    d.apply_signal(&pad, event, cursor, selection);
                }
                KeyMatch::SetActivePad(_) | KeyMatch::ResetActivePad => unreachable!(),
            }
        }
        // Вспышка тапа живёт ровно кадр: ставится в seek_tick на рендере,
        // гасится здесь перед сборкой следующего кадра.
        d.scrub.rew.flash = false;
        d.scrub.ff.flash = false;
        if d.transport.is_running() {
            // Будильник автономного транспорта: без плееров некому будить
            // UI на каждый «кадр» — пока время бежит, будим сами (~30 Гц);
            // стоит — тишина. С плеерами их публикации кадров будят сами,
            // этот будильник — дубль (egui склеивает).
            ctx.request_repaint_after(UI_TICK);
        }
        // Транспорт → плееры: позиция (сессия) и скорость синком на каждый
        // кадр; worker'ы экстраполируют между синками на своих часах.
        d.transport.sync();
        let zero = d.journal.record_started();
        // Позиция транспорта переносится на ось журнала; кадр дальше
        // живёт в Stamp. Мс-значение держим рядом: обратный перенос
        // (stamp.ms_since(zero)) — это оно же, пересчитывать нечего.
        let pos_ms = d.transport.position();
        let pos = zero.plus_ms(pos_ms as i64);
        let speed = d.transport.velocity();
        let clip = Some(RaceClip::from_events(d.journal.iter()));
        // Курсор кругов — позиция транспорта; начало первого круга —
        // старт гонки.
        let laps_pos = pos;
        let laps_race_start = d.journal.race_started().unwrap_or(zero);
        for (id, player) in d.players.iter() {
            let file = Self::clip_rec_ms(&clip, id).map_or(pos_ms, |rec| {
                pos_ms.saturating_sub(rec.ms_since(zero) as u64)
            });
            player.sync(file.min(player.duration()), speed);
        }
        // Интерактив транспорта: все плееры открылись (Ready/Failed) —
        // единый enabled четырёх кнопок; таймлайн читает его же.
        let interactive = d.players_ready();
        let (window, clock_pos) = Self::dvr_clock(d, &clip);
        // Знаковое отступление от границ гонки (журнал): минус — голова
        // до старта, плюс — хвост после финиша. Рамки нет — чистая позиция
        // без знака; знак занимает позицию десятков минут — все варианты
        // одной ширины: -0:05.234 / 00:00.000 / +0:03.100.
        let (sign, shown) = match Self::race_bounds(&clip) {
            Some(b) if clock_pos < b.from => (Some('-'), b.from.ms_since(clock_pos) as u64),
            Some(b) if clock_pos > b.to => (Some('+'), clock_pos.ms_since(b.to) as u64),
            Some(b) => (None, clock_pos.ms_since(b.from) as u64),
            None => (None, clock_pos.ms_since(window.from) as u64),
        };
        let timer_text = match sign {
            Some(sign) => format!(
                "{}{}:{:02}.{:03}",
                sign,
                shown / 60_000,
                (shown / 1_000) % 60,
                shown % 1_000
            ),
            None => format!(
                "{:02}:{:02}.{:03}",
                shown / 60_000,
                (shown / 1_000) % 60,
                shown % 1_000
            ),
        };
        let selection_pad = selection.pad().map(str::to_owned);
        let timeline_events = selection
            .pad()
            .map(|pad| {
                d.journal
                    .iter()
                    .filter_map(|(ms, entry)| match entry {
                        JournalEntry::RaceEvent { pad_id, name } if pad_id == pad => {
                            Some((ms, *name))
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut pads = Self::prepare_dvr_pads(d, setup, assignments, selection_pad.as_deref(), alt);
        Self::assign_places(&mut pads);
        let transport = TransportContents {
            rew: ButtonContents {
                enabled: interactive,
                active: d.scrub.rew.lit(),
                badge: None,
            },
            play: ButtonContents {
                enabled: interactive,
                active: d.transport.is_running() && !d.transport.is_seeking(),
                badge: None,
            },
            ff: ButtonContents {
                enabled: interactive,
                active: d.scrub.ff.lit(),
                badge: None,
            },
            rpt: ButtonContents {
                enabled: interactive,
                active: d.transport.circle().is_some(),
                badge: None,
            },
        };
        // Alt-бейджи транспорта — значения шкал на своих кнопках.
        let transport = if alt {
            let mut t = transport;
            t.rew.badge = Some(format!("\u{00d7}{}", SEEK_SPEEDS[d.scrub.seek_speed]));
            t.play.badge = Some(format!("{}%", PLAY_SPEEDS[d.scrub.play_speed]));
            t.ff.badge = Some(format!("\u{00d7}{}", SEEK_SPEEDS[d.scrub.seek_speed]));
            t.rpt.badge = Some(format!("±{:.1}s", RPT_SIZES[d.scrub.rpt_size]));
            t
        } else {
            transport
        };
        let rec = RecContents {
            rec: ButtonContents::DEAD,
            race: ButtonContents::DEAD,
        };
        // Таймлайн (журнальный домен; в дроби переводит рендер).
        let timeline = TimelineContents {
            window,
            pos: clock_pos,
            race: Self::race_bounds(&clip),
            marker: clip.as_ref().and_then(|c| c.race_countdown),
            circle: d.transport.circle().map(|c| {
                let center = d.journal.record_started().plus_ms(c.center as i64);
                Interval::new(
                    center.plus_ms(-(c.radius as i64)),
                    center.plus_ms(c.radius as i64),
                )
            }),
        };
        FrameContents {
            pads,
            transport,
            rec,
            timer: TimerContents {
                text: timer_text,
                dim: false,
            },
            timeline: Some(timeline),
            laps_cursor: Some(laps_pos),
            laps_race_start: Some(laps_race_start),
            countdown: None,
            laps_active: selection.clone(),
            laps_flash: LAP_FLASH_INTERVAL_DVR,
            timeline_events,
            laps_clickable: true,
            holeshot_enabled: d.rules.holeshot_enabled,
        }
    }

    pub fn update(&mut self, ui: &mut egui::Ui, navigator: &mut Navigator) {
        // Main-клавиатура (Idle/Live/Review): события egui уходят в
        // канал текущей фазы — локальный хаб (Idle, Review) или гоночный
        // поток (Live).
        match &mut self.phase {
            Mode::Idle(i) => i.main_keyboard.pump(ui),
            Mode::Live(l) => l.main_keyboard.pump(ui),
            Mode::Review(d) => d.main_keyboard.pump(ui),
        }
        enum Action {
            Exit,
            Rec,
            Race,
            None,
        }
        let mut action = Action::None;
        if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::W)) {
            action = Action::Exit;
        }
        let alt = ui.input(|i| i.modifiers.alt);
        let frame = self.prepare(ui.ctx(), alt);
        self.res.get_or_insert_with(|| Res::new(ui.ctx()));

        Letterbox::new(egui::vec2(cell_x(GRID_WIDTH), cell_y(45)))
            .max_virtual_height(cell_y(50))
            .outline(true)
            .show(ui, |ui| {
                let header_rect: egui::Rect = ui
                    .max_rect()
                    .into_ruler()
                    .collapse_top()
                    .extrude(0.0, cell_y(TOP_BAR_ROWS))
                    .into();
                self.header.show(ui, header_rect);

                // Таймлайн (только DVR): рисуется раньше кнопок — под
                // ними. Две клетки высотой, вся ширина, поля по 4
                // колонки. Головка ходит центрами от края до края.
                let timeline_rect: egui::Rect = ui
                    .max_rect()
                    .into_ruler()
                    .collapse_bottom()
                    .translate(0.0, -cell_y(4))
                    .extrude(0.0, cell_y(2))
                    .shrink(cell_x(LEFT_MARGIN), 0.0)
                    .shrink(-cell_x(RIGHT_MARGIN), 0.0)
                    .into();
                // Таймлайн жив ровно тогда же, когда кнопки транспорта
                // (enabled общий для всех четырёх — из players_ready).
                if let (Some(t), true) = (&frame.timeline, frame.transport.rew.enabled) {
                    let span = t.window.span_ms().max(1);
                    let to_frac = |at: Stamp| {
                        (at.ms_since(t.window.from) as f32 / span as f32).clamp(0.0, 1.0)
                    };
                    // Активный фрагмент (вылет) — границы гонки из журнала;
                    // без рамки вся линия серая. Точка отсчёта — момент
                    // RaceCountdown. Каретка — цветом активного транспорта.
                    let race = t
                        .race
                        .map(|b| (to_frac(b.from), to_frac(b.to.min(t.window.to))));
                    let marker = t.marker.map(to_frac);
                    // Круг зацикливания — реальные доли; растяжение
                    // сосиски до минимума — внутри виджета.
                    let circle = t.circle.map(|w| {
                        // Кламп в 0: круг может начинаться раньше окна
                        // таймлайна (отрицательное ms_since нельзя
                        // кастить в u64 — обернётся).
                        let lo = w.from.ms_since(t.window.from).max(0) as u64;
                        let hi = w.to.min(t.window.to).ms_since(t.window.from).max(0) as u64;
                        (lo as f32 / span as f32, hi as f32 / span as f32)
                    });
                    if let Mode::Review(d) = &mut self.phase {
                        let pos_frac = to_frac(t.pos);
                        let clip = Some(RaceClip::from_events(d.journal.iter()));
                        let (response, events) = d.scrub.timeline.show(
                            ui,
                            timeline_rect,
                            pos_frac,
                            span,
                            race,
                            circle,
                            marker,
                        );
                        for event in events {
                            match event {
                                TimelineEvent::SetDragHeld(held) => {
                                    d.transport.set_drag_held(held);
                                }
                                TimelineEvent::Seek(t_ms) => {
                                    // t — мс от начала таймлайна: в домен
                                    // сессии (транспорт сам клампит к шкале).
                                    d.transport.seek(
                                        t.window.from.ms_since(d.journal.record_started()) as u64
                                            + t_ms,
                                    );
                                }
                            }
                        }
                        // Ховер-скролл над шкалой — относительный скраб.
                        let dx = ui.input(|i| i.smooth_scroll_delta.x);
                        if let Some(ms) = d.scrub.timeline.hscroll(ui, &response, dx) {
                            Self::seek_by(d, &clip, ms);
                        }
                    }
                    // События выделенного пада — строка над таймлайном
                    // (освобождённая от списка кругов). Текущее —
                    // выделенное, иначе самое свежее ≤ курсора. Клик —
                    // выделение события (+ seek, как по ячейке таблицы);
                    // события строки — пада из выделения.
                    let (current, selected) = match &frame.laps_active {
                        Selection::Event(_, ms) => (Some(*ms), Some(*ms)),
                        _ => (
                            frame
                                .timeline_events
                                .iter()
                                .map(|(at, _)| *at)
                                .filter(|at| *at <= t.pos)
                                .max(),
                            None,
                        ),
                    };
                    if let Some(at) = show_events(
                        ui,
                        timeline_rect,
                        t.window,
                        t.pos,
                        &frame.timeline_events,
                        current,
                        selected,
                        LAP_FLASH_INTERVAL_DVR,
                    ) {
                        if let Some(pad) = self.selection.pad() {
                            let pad = pad.to_owned();
                            self.selection = Selection::Event(pad, at);
                            if let Mode::Review(d) = &mut self.phase {
                                let zero = d.journal.record_started();
                                d.transport.seek(at.ms_since(zero) as u64);
                            }
                        }
                    }
                }

                let pads: Vec<_> = self.setup.pads.iter().take(MAX_PADS).collect();
                let columns = pads.len() as isize;
                let viewfinder_rows = viewfinder_rows(pads.len());
                let viewfinder_cols = viewfinder_cols(pads.len());
                let segment = (GRID_WIDTH - LEFT_MARGIN - RIGHT_MARGIN) / columns.max(1);
                let column_geometry: Vec<(isize, isize)> = (0..columns)
                    .map(|i| (LEFT_MARGIN + segment * i, segment))
                    .collect();

                // Гайдлайны: вертикальные между колонками — от границы под
                // хедером (2 клетки от верха) до границы над футером
                // (5 клеток от низа — на клетку короче блока кругов,
                // не достают до его низа). Горизонтальные линии не
                // рисуем.
                if guidelines::ENABLED {
                    let screen = ui.max_rect().into_ruler();
                    let top = screen.collapse_top().translate(0.0, cell_y(2));
                    let bottom = screen.collapse_bottom().translate(0.0, -cell_y(5));
                    let stroke = egui::Stroke::new(2.0, egui::Color32::from_white_alpha(0x1e));
                    for i in 1..columns {
                        let x = cell_x(LEFT_MARGIN + segment * i);
                        let line = egui::pos2(x, top.bottom_left().y)
                            .into_ruler()
                            .extrude(0.0, bottom.bottom_left().y - top.bottom_left().y);
                        ui.painter().add(egui::epaint::Shape::dashed_line(
                            &[line.top_left(), line.bottom_left()],
                            stroke,
                            8.0,
                            4.0,
                        ));
                    }
                }

                // Зоны под вьюфайндерами: плейсхолдер под засечку
                // (рамка) и круги (без рамки, шириной во вьюфайндер).
                // Рамки — до контента.
                let zones = pad_zones(ui, &column_geometry, viewfinder_cols, pads.len());
                for (first, _) in &zones {
                    guidelines::dashed_rect(ui, *first);
                }
                let placeholder = |ui: &mut egui::Ui, rect: egui::Rect| {
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "\u{ead0}",
                        style::FONT_REGULAR_X2,
                        egui::Color32::from_rgb(0x30, 0x30, 0x30),
                    );
                };
                for (i, &(first, second)) in zones.iter().enumerate() {
                    placeholder(ui, first);
                    // Область пада кликабельна (контент колонки целиком):
                    // выделяет пад, курсор не меняем. Ячейки кругов ниже —
                    // их клики приоритетнее в пересечении (регистрируются
                    // позже, egui: позднее = поверх).
                    // Область пада — по зоне кругов (ширина
                    // вьюфайндера): обводка симметрична относительно
                    // центра колонки.
                    let content = egui::Rect::from_min_max(
                        egui::pos2(second.left(), cell_y(TOP_BAR_ROWS + 1)),
                        second.right_bottom(),
                    );
                    let response =
                        ui.interact(content, egui::Id::unique(("pad", i)), egui::Sense::click());
                    if response.clicked()
                        && let Some(pad) = frame.pads.get(i)
                    {
                        self.selection = Selection::Pad(pad.pad_id.clone());
                        if let Mode::Live(l) = &mut self.phase {
                            l.race.set_active_pad(Some(pad.pad_id.clone()));
                        }
                    }
                    // Рамка активного пада: вокруг контента колонки; толще
                    // обводки выделения, цвет тот же.
                    if frame.pads.get(i).is_some_and(|p| p.active) {
                        ui.painter().rect_stroke(
                            // Ширина зоны кругов (= вьюфайндер),
                            // +2 px со всех сторон; 3 px наружу, без
                            // скругления.
                            content
                                .expand2(egui::vec2(CELL_SIZE.x / 2.0, 0.0))
                                .expand(6.0),
                            0.0,
                            egui::Stroke::new(3.0, style::LAP_ACCENT),
                            egui::StrokeKind::Outside,
                        );
                    }
                }

                // Блоки кругов пэдов: таблицы, микро-таймер отсчёта.
                // Клик по ячейке — мутацию делает вызывающий; побочка
                // по фазе одна — перемотка по ячейке в DVR.
                let laps_rects: Vec<egui::Rect> = zones.iter().map(|&(_, second)| second).collect();
                if let Some((pad, ms)) = laps::show(ui, &frame, &laps_rects) {
                    self.selection = Selection::Event(pad, ms);
                    if let Mode::Review(d) = &mut self.phase {
                        let zero = d.journal.record_started();
                        d.transport.seek(ms.ms_since(zero) as u64);
                    }
                }

                if columns > 0 {
                    for (i, content) in frame.pads.iter().enumerate() {
                        let (zone_start, zone_width) = column_geometry[i];
                        let col = zone_start + (zone_width - viewfinder_cols) / 2;
                        let ruler = egui::pos2(cell_x(col), cell_y(TOP_BAR_ROWS + 1)).into_ruler();
                        let viewfinder =
                            ruler.extrude(cell_x(viewfinder_cols), cell_y(viewfinder_rows + 2));
                        let frame_rect: egui::Rect = viewfinder.into();
                        viewfinder::ViewfinderFrame::new(
                            &content.label,
                            &content.pilot,
                            content.color,
                        )
                        .show(ui, frame_rect, |ui, rect| {
                            viewfinder::Viewfinder::new(content.viewport).show(
                                ui,
                                content.image,
                                rect,
                            );
                            // Alt: эффективное разрешение вьюпорта
                            // (в правом нижнем углу картинки).
                            if let Some(badge) = &content.res_badge {
                                hint_badge_corner(ui, rect, std::slice::from_ref(badge));
                            }
                        });
                    }
                }

                // Нижний ряд: EXIT слева; по центру экрана — TIMER,
                // его серединная вертикаль совпадает с серединой сетки
                // (край таймера округляется в меньшую сторону — центр
                // не правее середины). REC ·1· RACE ·5· — влево от
                // таймера, транспорт REW ·1· PLAY ·1· FF ·2· RPT —
                // вправо (зазор от таймера тот же, 5 клеток). Все
                // кнопки (кроме EXIT) привязаны к таймеру.
                let y = ui.max_rect().bottom();

                let exit_label = "\u{e6c0}\u{f0206}";
                let btn_h = cell_y(2);
                let row_rect = |left: f32, cols: isize| -> egui::Rect {
                    egui::Rect::from_min_max(
                        egui::pos2(left, y - btn_h),
                        egui::pos2(left + cell_x(cols), y),
                    )
                };
                let exit_rect = row_rect(0.0, 6);
                let timer_icon = "\u{f43a}";
                let icon_w = measure_text(ui.ctx(), style::FONT_REGULAR_X2, timer_icon).x;
                let timer_w = icon_w
                    + cell_x(1)
                    + measure_text(ui.ctx(), style::FONT_REGULAR_X2, &frame.timer.text).x;
                // Центр таймера — центр сетки; край таймера — вниз,
                // к колонке.
                let timer_left =
                    ((cell_x(GRID_WIDTH) / 2.0 - timer_w / 2.0) / cell_x(1)).floor() * cell_x(1);
                let race_left = timer_left - cell_x(13); // 8 ширина + 5 зазор
                let rec_rect = row_rect(race_left - cell_x(9), 8);
                let race_rect = row_rect(race_left, 8);
                let rew_left = timer_left + timer_w + cell_x(5);
                let rew_rect = row_rect(rew_left, 8);
                let play_rect = row_rect(rew_left + cell_x(9), 8);
                let ff_rect = row_rect(rew_left + cell_x(18), 8);
                let rpt_rect = row_rect(rew_left + cell_x(28), 6);

                // Alt-баббл над кнопкой: многострочность — '\n' внутри
                // строки.
                let badge = |ui: &mut egui::Ui, rect: egui::Rect, badge: &Option<String>| {
                    let Some(badge) = badge else { return };
                    let lines: Vec<String> = badge.split('\n').map(str::to_owned).collect();
                    let row = CELL_SIZE.y;
                    let w = lines
                        .iter()
                        .map(|l| measure_text(ui.ctx(), style::FONT_REGULAR, l).x)
                        .fold(0.0f32, f32::max);
                    let text = egui::Rect::from_min_max(
                        egui::pos2(rect.center().x - w / 2.0, rect.top() - row * 2.0),
                        egui::pos2(rect.center().x + w / 2.0, rect.top() - row),
                    )
                    .translate(egui::vec2(0.0, row * 0.5));
                    hint_badge(ui, text, egui::Align2::LEFT_BOTTOM, &lines);
                };
                // Иконки и цвета — часть вёрстки ряда, состояние приходит
                // в ButtonContents.
                let paint_button = |ui: &mut egui::Ui,
                                    content: &ButtonContents,
                                    icon: &'static str,
                                    idle: egui::Color32,
                                    lit: egui::Color32,
                                    rect: egui::Rect,
                                    clicked: bool| {
                    TransportButton::new(
                        icon,
                        content.enabled,
                        content.active || clicked,
                        idle,
                        lit,
                    )
                    .paint(ui, rect);
                };

                // EXIT: всегда жива.
                let exit_response = TransportButton::new(
                    exit_label,
                    true,
                    false,
                    style::EXIT_IDLE,
                    style::EXIT_ACTIVE,
                )
                .interact(ui, exit_rect, ui.make_persistent_id("flight_exit"));
                TransportButton::new(
                    exit_label,
                    true,
                    exit_response.clicked(),
                    style::EXIT_IDLE,
                    style::EXIT_ACTIVE,
                )
                .paint(ui, exit_rect);
                if exit_response.clicked() {
                    action = Action::Exit;
                }

                // Транспорт: четыре кнопки по контенту. В эфире контент
                // мёртв (enabled=false) — рисуются выключенные.
                let transport = [
                    (
                        &frame.transport.rew,
                        rew_rect,
                        "flight_rew",
                        "\u{f04a}",
                        style::TRANSPORT_IDLE,
                        style::TRANSPORT_ACTIVE,
                    ),
                    (
                        &frame.transport.play,
                        play_rect,
                        "flight_play",
                        "\u{f04b}",
                        style::TRANSPORT_IDLE,
                        style::TRANSPORT_ACTIVE,
                    ),
                    (
                        &frame.transport.ff,
                        ff_rect,
                        "flight_ff",
                        "\u{f04e}",
                        style::TRANSPORT_IDLE,
                        style::TRANSPORT_ACTIVE,
                    ),
                    (
                        &frame.transport.rpt,
                        rpt_rect,
                        "flight_rpt",
                        "\u{F0456}",
                        style::TRANSPORT_IDLE,
                        style::TRANSPORT_ACTIVE,
                    ),
                ];
                if let Mode::Review(d) = &mut self.phase {
                    let clip = Some(RaceClip::from_events(d.journal.iter()));
                    let mut responses: Vec<Option<egui::Response>> = Vec::new();
                    for (content, rect, id, icon, idle, lit) in &transport {
                        responses.push(if content.enabled {
                            Some(
                                TransportButton::new(*icon, content.enabled, false, *idle, *lit)
                                    .interact(ui, *rect, ui.make_persistent_id(*id)),
                            )
                        } else {
                            None
                        });
                    }
                    // REW/FF: кадр кнопки перемотки (tap/hold/shuttle) —
                    // события кнопок, исполнение — ниже.
                    let mut seek_events: Vec<(bool, SeekEvent)> = Vec::new();
                    if let Some(r) = &responses[0] {
                        if let Some(ev) = d.scrub.rew.tick(ui, r) {
                            seek_events.push((true, ev));
                        }
                    }
                    if let Some(r) = &responses[2] {
                        if let Some(ev) = d.scrub.ff.tick(ui, r) {
                            seek_events.push((false, ev));
                        }
                    }
                    for (back, event) in seek_events {
                        Self::apply_seek_event(d, event, back, &clip);
                    }
                    for ((content, rect, _, icon, idle, lit), response) in
                        transport.iter().zip(&responses)
                    {
                        paint_button(
                            ui,
                            content,
                            *icon,
                            *idle,
                            *lit,
                            *rect,
                            response.as_ref().is_some_and(|r| r.clicked()),
                        );
                        badge(ui, *rect, &content.badge);
                    }
                    if responses[1].as_ref().is_some_and(|r| r.clicked()) {
                        if d.transport.is_running() {
                            d.transport.pause();
                        } else {
                            d.transport.play();
                        }
                    }
                    if responses[3].as_ref().is_some_and(|r| r.clicked()) {
                        let radius = (RPT_SIZES[d.scrub.rpt_size] * 1000.0) as u64;
                        d.transport.set_circle(if frame.transport.rpt.active {
                            None
                        } else {
                            Some(Circle { center: 0, radius })
                        });
                    }
                    // Скролл над кнопками — шаг по их шкалам.
                    let scroll_delta = ui.input(|i| i.smooth_scroll_delta.y);
                    if frame.transport.rew.enabled {
                        if let Some(up) =
                            d.scrub
                                .vscroll
                                .step(scroll_delta, SCROLL_STEP, SCROLL_REARM)
                        {
                            let dir = if up { -1 } else { 1 };
                            let shift = |cur: &mut usize, len: usize, dir: i64| {
                                *cur = (*cur as i64 + dir).clamp(0, len as i64 - 1) as usize;
                            };
                            // Скролл вверх — к большей скорости (шкалы
                            // по возрастанию, сдвиг единый со всеми).
                            if responses[0].as_ref().is_some_and(|r| r.hovered()) {
                                shift(&mut d.scrub.seek_speed, SEEK_SPEEDS.len(), dir);
                            }
                            if responses[2].as_ref().is_some_and(|r| r.hovered()) {
                                shift(&mut d.scrub.seek_speed, SEEK_SPEEDS.len(), dir);
                            }
                            if responses[3].as_ref().is_some_and(|r| r.hovered()) {
                                let before = d.scrub.rpt_size;
                                shift(&mut d.scrub.rpt_size, RPT_SIZES.len(), dir);
                                // Круг включён — переприменить радиус
                                // (транспорт центр переставит сам).
                                if d.scrub.rpt_size != before && d.transport.circle().is_some() {
                                    let radius = (RPT_SIZES[d.scrub.rpt_size] * 1000.0) as u64;
                                    d.transport.set_circle(Some(Circle { center: 0, radius }));
                                }
                            }
                            if responses[1].as_ref().is_some_and(|r| r.hovered()) {
                                let before = d.scrub.play_speed;
                                shift(&mut d.scrub.play_speed, PLAY_SPEEDS.len(), dir);
                                if d.scrub.play_speed != before {
                                    let speed = PLAY_SPEEDS[d.scrub.play_speed] as f64 / 100.0;
                                    d.transport.set_speed(speed);
                                }
                            }
                        }
                    }
                } else {
                    for (content, rect, _, icon, idle, lit) in &transport {
                        paint_button(ui, content, *icon, *idle, *lit, *rect, false);
                    }
                }

                // REC/RACE: живость из фазы (в DVR мертвы), намерения —
                // в гоночный поток.
                let rec_response = if frame.rec.rec.enabled {
                    Some(
                        TransportButton::new(
                            "\u{f044a}",
                            true,
                            false,
                            style::REC_IDLE,
                            style::REC_ACTIVE,
                        )
                        .interact(
                            ui,
                            rec_rect,
                            ui.make_persistent_id("flight_rec"),
                        ),
                    )
                } else {
                    None
                };
                paint_button(
                    ui,
                    &frame.rec.rec,
                    "\u{f044a}",
                    style::REC_IDLE,
                    style::REC_ACTIVE,
                    rec_rect,
                    rec_response.as_ref().is_some_and(|r| r.clicked()),
                );
                badge(ui, rec_rect, &frame.rec.rec.badge);
                if rec_response.as_ref().is_some_and(|r| r.clicked()) {
                    action = Action::Rec;
                }

                let race_response = if frame.rec.race.enabled {
                    Some(
                        TransportButton::new(
                            "\u{f140b}",
                            true,
                            false,
                            style::RACE_IDLE,
                            style::RACE_ACTIVE,
                        )
                        .interact(
                            ui,
                            race_rect,
                            ui.make_persistent_id("flight_race"),
                        ),
                    )
                } else {
                    None
                };
                paint_button(
                    ui,
                    &frame.rec.race,
                    "\u{f140b}",
                    style::RACE_IDLE,
                    style::RACE_ACTIVE,
                    race_rect,
                    race_response.as_ref().is_some_and(|r| r.clicked()),
                );
                badge(ui, race_rect, &frame.rec.race.badge);
                if race_response.as_ref().is_some_and(|r| r.clicked()) {
                    action = Action::Race;
                }

                // TIMER: в live-фазах — счётчик гонки (цвет с учётом
                // мигания/нулей), в DVR — позиция.
                let timer_pos = egui::pos2(timer_left, y);
                let timer_icon = "\u{f43a}";
                let icon_w = measure_text(ui.ctx(), style::FONT_REGULAR_X2, timer_icon).x;
                let timer_color = if frame.timer.dim {
                    style::TIMER_INACTIVE
                } else {
                    style::TIMER_ACTIVE
                };
                ui.painter().text(
                    timer_pos,
                    egui::Align2::LEFT_BOTTOM,
                    timer_icon,
                    style::FONT_REGULAR_X2,
                    timer_color,
                );
                ui.painter().text(
                    egui::pos2(timer_pos.x + icon_w + cell_x(1), timer_pos.y),
                    egui::Align2::LEFT_BOTTOM,
                    &frame.timer.text,
                    style::FONT_REGULAR_X2,
                    timer_color,
                );
            });

        if self.exit_requested {
            self.exit_requested = false;
            action = Action::Exit;
        }
        match action {
            Action::Exit => {
                // Ядро доводит гонку до целостности (финиш/abort),
                // экран закрываем не дожидаясь.
                if let Mode::Live(l) = &self.phase {
                    l.race.exit();
                }
                navigator.goto(menu::Menu::new())
            }
            Action::Rec => {
                // Первое нажатие в Idle рождает поток; в Live уходит
                // в ядро как есть.
                if matches!(self.phase, Mode::Idle(_)) {
                    self.enter_live(ui.ctx(), false);
                } else if let Mode::Live(l) = &self.phase {
                    l.race.rec_pressed();
                }
            }
            Action::Race => {
                if matches!(self.phase, Mode::Idle(_)) {
                    self.enter_live(ui.ctx(), true);
                } else if let Mode::Live(l) = &self.phase {
                    l.race.race_pressed();
                }
            }
            Action::None => {}
        }
    }

    /// Переход эфир → DVR по закрытию гонки: строим Review, дроп Live
    /// гасит эфир (камеры, keepawake, гоночный поток).
    fn enter_review(&mut self, ctx: &egui::Context, journal: Journal) {
        self.phase = Mode::Review(Review::open(ctx, Some(journal), &self.setup));
    }

    /// Часы DVR в журнальном домене: (окно, позиция). Полоска — окно
    /// записи (REC on → REC off); файлы лежат на ней по первым кадрам.
    /// Дегенеративная рамка (нет ни окна, ни файлов) — файловый домен:
    /// окно [ноль, длительность], позиция — max показанного.
    fn dvr_clock(d: &Review, clip: &Option<RaceClip>) -> (Interval, Stamp) {
        let zero = d.journal.record_started();
        let degenerate = clip
            .as_ref()
            .is_some_and(|c| c.recording.is_none() && c.files.is_empty());
        match clip {
            Some(clip) if !degenerate => {
                let window = clip.recording.unwrap_or_else(|| {
                    let from = clip.files.values().map(|(_, ms)| *ms).min().unwrap_or(zero);
                    Interval::new(from, clip.end().unwrap_or(from))
                });
                // Позиция циферблата: показанный кадр мастер-камеры
                // (маппинг в сессию через ноль её файла), пока он близок
                // к позиции транспорта — жёсткое соответствие картинке.
                // Расстояние больше толеранса (гэп, мастер недоступен,
                // быстрый шаттл, где shown отстаёт на секунды) — цифра =
                // позиция транспорта. Критерий — дистанция, не окно:
                // границы как точки переключения не существует, обе
                // ветки на стыке равны — фликер невозможен.
                let tp = Some(
                    d.journal
                        .record_started()
                        .plus_ms(d.transport.position() as i64),
                );
                let pos = match (d.players.get(&d.master), clip.files.get(&d.master)) {
                    (Some(p), Some((_, rec))) => {
                        let shown_session = rec.plus_ms(p.shown() as i64);
                        match tp {
                            Some(tp) if tp.ms_since(shown_session).abs() as u64 <= DIAL_LAG_MS => {
                                shown_session
                            }
                            _ => tp.unwrap_or(shown_session),
                        }
                    }
                    _ => tp.unwrap_or(window.from),
                };
                (window, window.clamp(pos))
            }
            _ => {
                let pos = d.players.values().map(|p| p.shown()).max().unwrap_or(0);
                let dur = d.players.values().map(|p| p.duration()).max().unwrap_or(0);
                (
                    Interval::new(zero, zero.plus_ms(dur as i64)),
                    zero.plus_ms(pos.min(dur) as i64),
                )
            }
        }
    }

    /// Метка первого кадра камеры (ноль её файла).
    fn clip_rec_ms(clip: &Option<RaceClip>, camera_id: &str) -> Option<Stamp> {
        clip.as_ref()?
            .files
            .get(camera_id)
            .map(|(_, file_ms)| *file_ms)
    }

    /// В этой точке таймлайна у камеры нет записи: файл ещё не начался
    /// или уже закончился (журнальный домен).
    fn clip_gap(
        clip: &Option<RaceClip>,
        players: &HashMap<String, Player>,
        camera_id: &str,
        journal_pos: Stamp,
    ) -> bool {
        match clip {
            Some(clip) => match (clip.files.get(camera_id), players.get(camera_id)) {
                (Some((_, file_ms)), Some(p)) => {
                    // Покрытие сдвинуто на период кадра в обе стороны:
                    // первый и последний кадры демонстрируются интервалом
                    // (last_step), а не мгновением — иначе голова записи
                    // (record-started раньше первого кадра на доли периода)
                    // и хвост до record-finished читались бы как гэпы.
                    (journal_pos.ms_since(*file_ms) + p.last_step() as i64) < 0
                        || journal_pos.ms_since(*file_ms)
                            >= p.duration() as i64 + p.last_step() as i64
                }
                _ => false,
            },
            None => false,
        }
    }

    /// Границы гонки в журнальном домене (None — рамки нет).
    fn race_bounds(clip: &Option<RaceClip>) -> Option<Interval> {
        let clip = clip.as_ref()?;
        Some(Interval::new(clip.race_started?, clip.race_finished?))
    }

    /// Перемотка транспорта на delta мс в журнальном домене: база —
    /// конверт таймлайна. Плееры получат новую позицию следующим синком.
    fn seek_by(d: &mut Review, clip: &Option<RaceClip>, delta: i64) {
        let zero = d.journal.record_started();
        let (window, pos) = Self::dvr_clock(d, clip);
        let target = (pos.ms_since(zero) + delta)
            .clamp(window.from.ms_since(zero), window.to.ms_since(zero));
        d.transport.seek(target as u64);
    }

    /// Исполнение события кнопки перемотки (машина — в SeekButton):
    /// шаг — шкала SEEK_SPEEDS × SEEK_TAP с направлением кнопки, шаттл и
    /// рампа возврата — на транспорте.
    fn apply_seek_event(d: &mut Review, event: SeekEvent, back: bool, clip: &Option<RaceClip>) {
        let dir = if back { -1 } else { 1 };
        match event {
            SeekEvent::Step { from_hold } => {
                if from_hold {
                    d.transport.shuttle_release(SEEK_RELEASE_RAMP);
                }
                let step = dir * SEEK_SPEEDS[d.scrub.seek_speed] as i64 * SEEK_TAP as i64;
                Self::seek_by(d, clip, step);
            }
            SeekEvent::Shuttle => {
                let speed = SEEK_SPEEDS[d.scrub.seek_speed] as f64 * dir as f64;
                d.transport.shuttle(speed);
            }
            SeekEvent::Release => {
                d.transport.shuttle_release(SEEK_RELEASE_RAMP);
            }
        }
    }

    /// Команда удалённого управления — нажатия кнопок экрана: rec, race,
    /// play, ff, rew, exit повторяют клики по кнопкам, seek <ms> —
    /// абсолютную перемотку всех плееров. Some — обработана.
    pub fn handle_remote(&mut self, ctx: &egui::Context, command: &[String]) -> Option<Response> {
        let head = command.first()?.as_str();
        let ok = |message: String| Some(Response { message, code: 0 });
        match (&head, &mut self.phase) {
            (&"rec", Mode::Idle(_)) => {
                self.enter_live(ctx, false);
                ok(String::new())
            }
            (&"rec", Mode::Live(l)) => {
                if l.state.rec_available() {
                    l.race.rec_pressed();
                }
                ok(String::new())
            }
            (&"race", Mode::Idle(_)) => {
                self.enter_live(ctx, true);
                ok(String::new())
            }
            (&"race", Mode::Live(l)) => {
                if l.state.race_available() {
                    l.race.race_pressed();
                }
                ok(String::new())
            }
            (&"play", Mode::Review(d)) => {
                if d.transport.is_running() {
                    d.transport.pause();
                } else {
                    d.transport.play();
                }
                ok(String::new())
            }
            (&"ff" | &"rew", Mode::Review(d)) => {
                let clip = Some(RaceClip::from_events(d.journal.iter()));
                let dir: i64 = if head == "ff" { 1 } else { -1 };
                let step = dir * SEEK_SPEEDS[d.scrub.seek_speed] as i64 * SEEK_TAP as i64;
                Self::seek_by(d, &clip, step);
                ok(String::new())
            }
            (&"seek", Mode::Review(d)) => {
                // Абсолютная позиция в домене сессии (мс от нуля
                // журнала); транспорт клампит к шкале.
                let ms: u64 = command.get(1).and_then(|a| a.parse().ok())?;
                d.transport.seek(ms);
                ok(String::new())
            }
            (&"exit", Mode::Live(l)) => {
                // Ядру — немедленно (инварианты целостности).
                l.race.exit();
                self.exit_requested = true;
                ctx.request_repaint();
                ok(String::new())
            }
            (&"exit", Mode::Idle(_) | Mode::Review(_)) => {
                self.exit_requested = true;
                ctx.request_repaint();
                ok(String::new())
            }
            // Команда неприменима к фазе — молча принята.
            (&"rec" | &"race" | &"play" | &"ff" | &"rew" | &"seek", _) => ok(String::new()),
            _ => None,
        }
    }
}

impl Review {
    /// Удаление события (Delete/Backspace): запись убирается из
    /// журнала, файл переписывается снапшотом целиком. true — событие
    /// было и удалено.
    fn delete_selected(&mut self, pad: &str, at: Stamp) -> bool {
        // Полуинтервал [at, at + 1 мс): ровно одна запись по метке.
        if self.journal.remove(pad, at, at.plus_ms(1)) == 0 {
            return false;
        }
        self.snapshot_after_edit();
        true
    }

    /// Снапшот журнала после правки (файл переписывается целиком).
    fn snapshot_after_edit(&self) {
        if let Err(e) = self.journal.snapshot(Path::new(CAPTURES_DIR)) {
            log::error!("flight journal: snapshot after edit failed: {e}");
        }
    }

    /// Применение судейского сигнала клавишами (правка журнала в DVR):
    /// события пада в окне вспышки ([cursor−100, cursor+100)) убираются,
    /// нажатое встаёт по курсору; событий в окне нет — просто вставка по
    /// курсору. Выделение события снимается, выделение пада остаётся.
    /// Выделенное событие здесь не используется — оно для удаления.
    fn apply_signal(
        &mut self,
        pad: &str,
        event: LapEvent,
        cursor: Stamp,
        selection: &mut Selection,
    ) {
        let zero = self.journal.record_started();
        let cursor_ms = cursor.ms_since(zero);
        let (from, to) = LAP_FLASH_INTERVAL_DVR;
        self.journal.remove(
            pad,
            zero.plus_ms(cursor_ms + from),
            zero.plus_ms(cursor_ms + to),
        );
        self.journal.push(
            cursor,
            JournalEntry::RaceEvent {
                pad_id: pad.to_owned(),
                name: event,
            },
        );
        if let Selection::Event(p, _) = selection.clone() {
            *selection = Selection::Pad(p);
        }
        self.snapshot_after_edit();
    }

    /// Полный набор данных DVR из журнала (None — пустой DVR: никаких
    /// чужих записей, только файлы журнала). Плееры открываются
    /// асинхронно — транзит, пока все не открылись (Ready/Failed).
    fn open(ctx: &egui::Context, journal: Option<Journal>, setup: &Setup) -> Self {
        let journal = journal.unwrap_or_else(|| {
            // Пустой DVR (файла нет): правил нет — дефолт.
            let date_time = time::OffsetDateTime::now_local().unwrap_or_else(|_| {
                log::warn!("flight: local time unavailable, journal falls back to UTC");
                time::OffsetDateTime::now_utc()
            });
            Journal::new(
                date_time,
                setup.pads.clone(),
                HashMap::new(),
                RaceRules::default(),
            )
        });
        let clip = RaceClip::from_events(journal.iter());
        let mut players = HashMap::new();
        let dir = Path::new(CAPTURES_DIR);
        for (camera_id, (filename, _)) in &clip.files {
            let path = dir.join(filename);
            if path.exists() {
                players.insert(camera_id.clone(), Player::open(&path, ctx.clone()));
            } else {
                log::warn!("flight: recording of {camera_id} missing: {path:?}");
            }
        }
        // Мастер — первая по порядку сетапа камера с файлом.
        let master = setup
            .pads
            .values()
            .map(|pad| pad.fpv.camera_id.clone())
            .find(|cam| clip.files.contains_key(cam))
            .unwrap_or_default();
        // Транспорт: шкала — окно записи, курсор — на момент отсчёта
        // (каждая камера покажет свой предшествующий кадр). Транспорт
        // живёт в мс сессии; ноль совпадает с нулём оси журнала.
        let zero = journal.record_started();
        let span = clip
            .recording
            .map(|w| w.to)
            .or_else(|| clip.end())
            .unwrap_or(zero)
            .ms_since(zero) as u64;
        let mut transport = Transport::new(Instant::now(), span);
        if let Some(cd) = clip.race_countdown {
            transport.seek((cd.ms_since(zero) as u64).min(span));
        }
        let (keyboard_tx, keyboard_rx) = crossbeam::unbounded();
        // Правила — из заголовка журнала (холодный вход читает те же,
        // что были в эфире).
        let rules = journal.header().rules;
        Self {
            journal,
            rules,
            key_matcher: KeyMatcher::new(setup),
            main_keyboard: Keyboard::new(&keyboard_tx),
            keyboard_rx,
            players,
            transport,
            master,
            scrub: Scrub {
                timeline: Timeline::new(),
                rew: SeekButton::new(),
                ff: SeekButton::new(),
                seek_speed: SEEK_SPEEDS.iter().position(|&v| v == 10).unwrap(),
                play_speed: PLAY_SPEEDS.iter().position(|&v| v == 100).unwrap(),
                rpt_size: RPT_SIZES.iter().position(|&v| v == 1.0).unwrap(),
                vscroll: ScrollGate::new(),
            },
        }
    }

    /// Готовность плееров (все Ready/Failed; пустой набор — готовым:
    /// иначе транзит открытия вечен).
    fn players_ready(&self) -> bool {
        self.players
            .values()
            .all(|p| matches!(p.open_state(), PlayerState::Ready | PlayerState::Failed(_)))
    }
}

/// Дрен клавиатурных событий из общей очереди потока гонки в per-pad
/// состояние удерживаемых клавиш. Обрыв канала (id в key_dead) снимает
/// все символы клавиатуры и переводит аттач в Err — плашка дисконнекта.
/// Возвращает true, если состояние могло измениться, — caller просит
/// перерисовку.
/// Аттач клавиатур сетапа в канал: warn на каждую неподцепленную,
#[cfg(target_os = "linux")]
/// состояния (connected) — вызывающему.
fn attach_exclusive_keyboards(
    setup: &Setup,
    tx: &crossbeam::Sender<KeyPressed>,
) -> HashMap<String, ExclusiveKeyboard> {
    let mut keyboards = HashMap::new();
    for (id, cfg) in &setup.exclusive_keyboards {
        match ExclusiveKeyboard::new(tx, id, cfg) {
            Ok(keyboard) => {
                keyboards.insert(id.clone(), keyboard);
            }
            Err(e) => log::warn!("exclusive keyboard {id}: attach failed: {e}"),
        }
    }
    keyboards
}

/// Срез камер активных падов (дедуп по camera_id): захваты открываются
/// по одному на камеру, гоночному потоку нужен owned срез. «Активные» =
/// пады с пилотами, в пределах MAX_PADS (рендер падов идёт по всем —
/// семантика иная).
fn filter_cameras(
    setup: &Setup,
    assignments: &HashMap<String, Pilot>,
) -> HashMap<String, CameraConfig> {
    setup
        .pads
        .iter()
        .filter(|(pad_id, _)| assignments.contains_key(*pad_id))
        .take(MAX_PADS)
        .filter_map(|(_, pad)| {
            setup
                .cameras
                .get(&pad.fpv.camera_id)
                .map(|camera| (pad.fpv.camera_id.clone(), camera.clone()))
        })
        .collect()
}

/// Число строк вьюфайндера по числу пилотов (копия live'овской раскладки).
fn viewfinder_rows(count: usize) -> isize {
    match count {
        1 | 2 => 21,
        3 => 15,
        _ => 12,
    }
}

/// Ширина вьюфайндера из соотношения 16:9 (8 клеток на 3 строки).
fn viewfinder_cols(count: usize) -> isize {
    viewfinder_rows(count) * 8 / 3
}

/// Зоны колонки под вьюфайндером (шириной во вьюфайндер): первый блок
/// — плейсхолдер под засечку (клетка ниже картинки, высота 4 клетки),
/// второй — круги (клетка ниже плейсхолдера, низ — на 2 клетки выше
/// нижней границы). Рамки рисуются Outside — не в счёт.
fn pad_zones(
    ui: &egui::Ui,
    column_geometry: &[(isize, isize)],
    viewfinder_cols: isize,
    pads: usize,
) -> Vec<(egui::Rect, egui::Rect)> {
    let screen = ui.max_rect().into_ruler();
    let bottom = screen.collapse_bottom().translate(0.0, -cell_y(4));
    let vf_bottom = cell_y(TOP_BAR_ROWS + 1 + viewfinder_rows(pads) + 2);
    let first_top = vf_bottom + cell_y(1);
    column_geometry
        .iter()
        .map(|&(zone_start, zone_width)| {
            // Ширина — как у вьюфайндера: центр той же зоны.
            let x = cell_x(zone_start + (zone_width - viewfinder_cols) / 2);
            let w = cell_x(viewfinder_cols);
            let first: egui::Rect = egui::pos2(x, first_top)
                .into_ruler()
                .extrude(w, cell_y(4))
                .into();
            // Список кругов — шириной во вьюфайндер.
            let second: egui::Rect = egui::pos2(x, first.bottom() + cell_y(1))
                .into_ruler()
                .extrude(
                    w,
                    bottom.bottom_left().y - cell_y(2) - first.bottom(),
                )
                .into();
            (first, second)
        })
        .collect()
}

/// Дрен клавиатурного канала фазы: нажатия матчит (активный пад —
/// из выделения, матчер чистый) и применяет к выделению: Set/Reset
/// меняют пад. Остальные действия возвращает — исполнение у фазы
/// (сигналы — в гонке, удаление — в Review).
fn drain_keyboard(
    rx: &crossbeam::Receiver<KeyPressed>,
    key_matcher: &KeyMatcher,
    selection: &mut Selection,
) -> Vec<KeyMatch> {
    let mut pending = Vec::new();
    while let Ok(event) = rx.try_recv() {
        log::info!("keyboard: {event}");
        let Some(matched) = key_matcher.match_key(&event, selection.pad()) else {
            continue;
        };
        log::info!("keybind: {matched}");
        match matched {
            KeyMatch::SetActivePad(p) => *selection = Selection::Pad(p),
            KeyMatch::ResetActivePad => *selection = Selection::None,
            other => pending.push(other),
        }
    }
    pending
}

/// мм:ss от длительности.
fn mmss(dur: Duration) -> String {
    let secs = dur.as_secs();
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

struct Res;

impl Res {
    fn new(ctx: &egui::Context) -> Self {
        let symbol_size = measure_text(ctx, style::FONT_REGULAR, "M");
        assert!(
            symbol_size == CELL_SIZE,
            "Font size mismatch: expected {:?}, got {:?}. Update ruler::CELL_SIZE or check font assets.",
            CELL_SIZE,
            symbol_size,
        );
        Self
    }
}
