//! Экран Flight: совмещает эфир (трансляция/запись/гонка) и DVR
//! (просмотр записи: транспорт, таймлайн, круг повтора). Режим входа —
//! Live (сразу эфир) или Review (сразу просмотр, CLI `laps scrub`). Выход —
//! EXIT в меню. Гоночное состояние живёт в потоке ядра
//! (model::rules + service::race), экран — намерения и рендер снапшотов.

use super::*;
use crate::config::camera::CameraConfig;
use crate::config::setup::Setup;
use crate::driver::audio::Audio;
use crate::driver::camera::Camera;
use crate::driver::camera::CaptureState;
use crate::driver::dvr::{CAPTURES_DIR, RecordState};
#[cfg(target_os = "linux")]
use crate::driver::keyboard::ExclusiveKeyboard;
use crate::driver::keyboard::{KeyPressed, Keyboard, combo};
use crate::driver::player::{Player, PlayerState};
use crate::gui::ruler::{CELL_SIZE, IntoRuler, cell_x, cell_y};
use crate::gui::scrub::{
    CIRCLE_MIN_WIDTH_PX, HScroll, PLAY_SPEEDS, RPT_SIZES, SCROLL_REARM, SCROLL_STEP,
    SEEK_CLICK_MAX, SEEK_HOLD_DELAY, SEEK_RELEASE_RAMP, SEEK_SPEEDS, SEEK_TAP, ScrollGate,
    SeekButton, Timeline, TimelineEvent, latest_journal,
};
use crate::gui::view::{Letterbox, TransportButton, hint_badge, hint_badge_corner};
use crate::model::flight::{Journal, LapEvent, LapLine, RaceClip};
use crate::model::pilot::Pilot;
use crate::model::rules::RaceRules;
use crate::remote::Response;
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

/// Наборы цветов кнопок (см. colors-table.html): два цвета на кнопку —
/// яркий (активный фон) и неяркий (покой; выключенный = неяркий × 0.35
/// альфой). EXIT — холодный серый, транспорт — тёплый пластик, REC —
/// алый, RACE — кислотный.
const EXIT_ACTIVE: egui::Color32 = egui::Color32::from_rgb(0xAE, 0xBE, 0xC8);
const EXIT_IDLE: egui::Color32 = egui::Color32::from_rgb(0x4E, 0x5A, 0x63);
const TRANSPORT_ACTIVE: egui::Color32 = egui::Color32::from_rgb(0xE8, 0xE2, 0xCE);
const TRANSPORT_IDLE: egui::Color32 = egui::Color32::from_rgb(0x6A, 0x66, 0x56);
const REC_ACTIVE: egui::Color32 = egui::Color32::from_rgb(0xEA, 0x31, 0x23);
const REC_IDLE: egui::Color32 = egui::Color32::from_rgb(0x6E, 0x42, 0x3C);
const RACE_ACTIVE: egui::Color32 = egui::Color32::from_rgb(0xC8, 0xF5, 0x00);
const RACE_IDLE: egui::Color32 = egui::Color32::from_rgb(0x75, 0x77, 0x48);

/// Толеран отставания показанного кадра мастера от позиции транспорта,
/// в пределах которого циферблат верен картинке (лень кадра + декод на
/// обычных скоростях; шаттл уходит дальше — цифра честно = курсор).
const DIAL_LAG_MS: u64 = 150;

/// Единый «счётчик» перерисовок там, где кадры не гонят UI сами (мёртвый
/// эфир, счётчик гонки без картинки, автономный транспорт): минимум
/// ~23 Гц. Повторный вызов перевооружает таймер egui, состояние не
/// нужно.
const UI_TICK: Duration = Duration::from_millis(43);

/// Пара основного таймера: горящий и потухший. Потухший — 0.2
/// яркости (тот же сигнал мигания, что у таймера: вторая половина
/// секунды отсчёта).
const TIMER_ACTIVE: egui::Color32 = TRANSPORT_ACTIVE;
const TIMER_INACTIVE: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(0x2E, 0x2D, 0x29, 0x33);

/// Пара микро-таймеров отсчёта в пэдах — та же, что у основного
/// таймера.
const PAD_TIMER_ACTIVE: egui::Color32 = TIMER_ACTIVE;
const PAD_TIMER_INACTIVE: egui::Color32 = TIMER_INACTIVE;

/// Ненаступившие компоненты круга (DVR): номер/время до начала круга,
/// событие до его метки. Не путать с потухшим (мигание) — отдельный
/// оттенок.
const LAP_FUTURE: egui::Color32 = egui::Color32::from_rgb(0x60, 0x60, 0x60);

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
    laps: HashMap<String, Vec<(u64, LapLine, Vec<(u64, LapEvent)>)>>,
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
struct Review {
    /// Журнал полёта — владение в GUI (правки DVR — здесь).
    journal: Journal,
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
    hscroll: HScroll,
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
    /// Курсор кругов: позиция воспроизведения, мс журнала. None —
    /// эфир: всё в прошлом, раскраска не работает (без спецветок).
    laps_cursor: Option<u64>,
    /// Старт гонки, мс журнала: начало первого круга (для «ещё не
    /// начался»). 0 в эфире (не используется без курсора).
    laps_race_start: u64,
    /// Отсчёт в пэдах: Some(dim) — фаза Countdown, dim — тот же сигнал
    /// мигания, что у основного таймера (вторая половина секунды).
    /// None — не отсчёт, пэды рисуют круги/пустоту.
    countdown: Option<bool>,
}

/// Контент одного пада: всё для его рамки и картинки.
struct PadContents {
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
    laps: Vec<(u64, LapLine, Vec<(u64, LapEvent)>)>,
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
    start: u64,
    pos: u64,
    end: u64,
    /// Границы гонки (журнал): белый фрагмент трека.
    window: Option<(u64, u64)>,
    /// Момент RaceCountdown: точка на треке.
    marker: Option<u64>,
    /// Круг зацикливания: (центр, радиус) в мс.
    circle: Option<(u64, u64)>,
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
        Self {
            setup,
            assignments,
            rules,
            audio: Audio::spawn(),
            shown_cameras,
            res: None,
            exit_requested: false,
            header: header::Header::new(),
            phase: Mode::Idle(Idle {
                webcams,
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
        // Пробуждение GUI по уведомлениям потока: без него state
        // обновлялся бы только по кадрам камер (мертвый эфир висел бы
        // в Before до первого repaint).
        let wake_ctx = ctx.clone();
        let wake = Arc::new(move || wake_ctx.request_repaint());
        let race = Race::spawn(
            self.rules,
            PathBuf::from(CAPTURES_DIR),
            self.setup.pads.clone(),
            self.assignments.clone(),
            self.shown_cameras.clone(),
            recorders,
            idle.keyboard_rx,
            wake,
            self.audio.clone(),
        );
        // Рождение кнопкой RACE: отсчёт стартует сразу (намерение ждёт
        // поток в очереди на рождение).
        if start_race {
            race.race_pressed();
        }
        self.phase = Mode::Live(Live {
            race,
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
                let (header, events) = Journal::parse_stream(&yaml);
                header.map(|header| {
                    let mut journal =
                        Journal::new(header.date_time, header.pads, header.assignments);
                    for (ms, event) in events {
                        journal.push(ms, event);
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
                ctx,
                alt,
            ),
            Mode::Live(l) => {
                Self::prepare_live(l, &self.setup, &self.assignments, &self.shown_cameras, alt)
            }
            Mode::Review(d) => Self::prepare_dvr(d, &self.setup, &self.assignments, ctx, alt),
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
        laps: &HashMap<String, Vec<(u64, LapLine, Vec<(u64, LapEvent)>)>>,
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
        alt: bool,
    ) -> FrameContents {
        let state = l.state;
        let rec_on = state.rec_on();
        let race_on = state.countdown_active() || matches!(state, RaceStage::Running { .. });
        let now = Instant::now();
        let pads = Self::prepare_pads(
            &mut l.webcams,
            setup,
            assignments,
            shown_cameras,
            &l.laps,
            alt,
        );
        let fps_lines = if alt {
            Self::fps_lines(&l.webcams, shown_cameras, rec_on)
        } else {
            Vec::new()
        };
        // Alt над RACE: Σ / остаток со знаком минус (без лимита нечего
        // показывать ни до, ни после старта).
        let duration = l.rules.duration;
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
            laps_cursor: None,
            laps_race_start: 0,
            countdown,
        }
    }

    /// Кадр Idle-эфира: как Live, но REC/RACE живы (первое нажатие
    /// родит поток), счётчик погашен.
    fn prepare_idle(
        i: &mut Idle,
        setup: &Setup,
        assignments: &HashMap<String, Pilot>,
        shown_cameras: &HashMap<String, CameraConfig>,
        ctx: &egui::Context,
        alt: bool,
    ) -> FrameContents {
        // Клавиатуры (Idle): эвенты (устройства + main) только
        // логируются; в Live канал уехал в Race, там его дренит поток.
        while let Ok(event) = i.keyboard_rx.try_recv() {
            log::info!("keyboard: {event}");
            // Матчинг по биндингам сетапа: сматчившийся аккорд и его
            // action в лог. Код намеренно не обобщён с Live-дрейном.
            for (pad_id, pad) in &setup.pads {
                for bind in &pad.keybinds {
                    if bind.matches(&event) {
                        log::info!("keybind: {pad_id} {} -> {}", combo(&bind.keys), bind.action);
                    }
                }
            }
        }
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
            laps_race_start: 0,
            countdown: None,
        }
    }

    /// Контент падов DVR: кадры плееров (с гэп-покрытием), камеры без
    /// файла — Error. Плееры здесь же получают update_frame.
    fn prepare_dvr_pads(
        d: &mut Review,
        setup: &Setup,
        assignments: &HashMap<String, Pilot>,
        alt: bool,
    ) -> Vec<PadContents> {
        let clip = Some(RaceClip::from_events(d.journal.iter_ms()));
        let pos = d.transport.position();
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
                    laps: d.journal.lap_infos(pad_id),
                }
            })
            .collect()
    }

    /// Кадр DVR: контент из плееров, транспорт, шкала, позиция.
    fn prepare_dvr(
        d: &mut Review,
        setup: &Setup,
        assignments: &HashMap<String, Pilot>,
        ctx: &egui::Context,
        alt: bool,
    ) -> FrameContents {
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
        let pos = d.transport.position();
        let speed = d.transport.velocity();
        let clip = Some(RaceClip::from_events(d.journal.iter_ms()));
        // Курсор кругов — позиция транспорта; начало первого круга —
        // старт гонки.
        let laps_pos = pos;
        let laps_race_start = clip.as_ref().and_then(|c| c.race_started_ms).unwrap_or(0);
        for (id, player) in d.players.iter() {
            let file = Self::clip_rec_ms(&clip, id).map_or(pos, |rec| pos.saturating_sub(rec));
            player.sync(file.min(player.duration()), speed);
        }
        // Интерактив транспорта: все плееры открылись (Ready/Failed) —
        // единый enabled четырёх кнопок; таймлайн читает его же.
        let interactive = d.players_ready();
        let (start, clock_pos, end) = Self::dvr_clock(d, &clip);
        // Знаковое отступление от границ гонки (журнал): минус — голова
        // до старта, плюс — хвост после финиша. Рамки нет — чистая позиция
        // без знака; знак занимает позицию десятков минут — все варианты
        // одной ширины: -0:05.234 / 00:00.000 / +0:03.100.
        let (sign, shown) = match Self::race_bounds(&clip) {
            Some((lo, _)) if clock_pos < lo => (Some('-'), lo - clock_pos),
            Some((_, hi)) if clock_pos > hi => (Some('+'), clock_pos - hi),
            Some((lo, _)) => (None, clock_pos.saturating_sub(lo)),
            None => (None, clock_pos.saturating_sub(start)),
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
        let pads = Self::prepare_dvr_pads(d, setup, assignments, alt);
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
        // Таймлайн (мс-домен; в дроби переводит рендер).
        let timeline = TimelineContents {
            start,
            pos: clock_pos,
            end,
            window: Self::race_bounds(&clip),
            marker: clip.as_ref().and_then(|c| c.race_countdown_ms),
            circle: d.transport.circle().map(|c| (c.center, c.radius)),
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
            laps_race_start: laps_race_start,
            countdown: None,
        }
    }

    pub fn update(&mut self, ui: &mut egui::Ui, navigator: &mut Navigator) {
        // Main-клавиатура (Idle/Live): события egui уходят в общий
        // канал клавиатур — тот, куда смотрит текущий хаб (локальный
        // в Idle, в потоке гонки в Live). В Review нет ни хаба, ни main.
        match &mut self.phase {
            Mode::Idle(i) => i.main_keyboard.pump(ui),
            Mode::Live(l) => l.main_keyboard.pump(ui),
            Mode::Review(_) => {}
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
                    let span = t.end.saturating_sub(t.start).max(1);
                    let pos_frac = t.pos.saturating_sub(t.start) as f32 / span as f32;
                    let to_frac =
                        |ms: u64| (ms.saturating_sub(t.start) as f32 / span as f32).clamp(0.0, 1.0);
                    // Круг зацикливания: растяжение сосиски до минимума
                    // (реальный круг уже минимума — голова по общему
                    // скейлу; иначе капсула тянется до CIRCLE_MIN_WIDTH_PX,
                    // а внутри неё голова (и drag) течёт по локальному
                    // скейлу круга).
                    let min_w =
                        (CIRCLE_MIN_WIDTH_PX / ui.ctx().pixels_per_point() / timeline_rect.width())
                            .min(1.0);
                    let circle_raw = t.circle.map(|(center, radius)| {
                        let lo = center.saturating_sub(radius).saturating_sub(t.start);
                        let hi = (center + radius).min(t.end).saturating_sub(t.start);
                        (lo as f32 / span as f32, hi as f32 / span as f32)
                    });
                    let (circle, circle_real, knob) = match circle_raw {
                        Some((lo, hi)) if hi - lo < min_w => {
                            let center = ((lo + hi) / 2.0).clamp(min_w / 2.0, 1.0 - min_w / 2.0);
                            let nlo = center - min_w / 2.0;
                            let nhi = center + min_w / 2.0;
                            let inner =
                                ((pos_frac - lo) / (hi - lo).max(f32::EPSILON)).clamp(0.0, 1.0);
                            (Some((nlo, nhi)), Some((lo, hi)), nlo + inner * (nhi - nlo))
                        }
                        other => (other, None, pos_frac),
                    };
                    // Активный фрагмент (вылет) — границы гонки из журнала;
                    // без рамки вся линия серая. Точка отсчёта — момент
                    // RaceCountdown. Каретка — цветом активного транспорта.
                    let window = t
                        .window
                        .map(|(lo, hi)| (to_frac(lo), to_frac(hi.min(t.end))));
                    let marker = t.marker.map(to_frac);
                    if let Mode::Review(d) = &mut self.phase {
                        let (response, events) = d.scrub.timeline.show(
                            ui,
                            timeline_rect,
                            knob,
                            span,
                            window,
                            circle,
                            circle_real,
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
                                    d.transport.seek(t.start + t_ms);
                                }
                            }
                        }
                        let dx = if response.hovered() {
                            ui.input(|i| i.smooth_scroll_delta.x)
                        } else {
                            0.0
                        };
                        if let Some(ms) = d.scrub.hscroll.feed(dx, ui.ctx().pixels_per_point()) {
                            Self::seek_by(d, &Some(RaceClip::from_events(d.journal.iter_ms())), ms);
                        }
                        if d.scrub.hscroll.active() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                            ui.ctx().request_repaint();
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
                // (4 клетки от низа). Горизонтальные линии не рисуем.
                if guidelines::ENABLED {
                    let screen = ui.max_rect().into_ruler();
                    let top = screen.collapse_top().translate(0.0, cell_y(2));
                    let bottom = screen.collapse_bottom().translate(0.0, -cell_y(4));
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
                    // Квадраты под вьюфайндерами (шириной во вьюфайндер):
                    // первый — клетка ниже картинки (рамка рисуется
                    // Outside — не в счёт), высота 4 клетки; второй —
                    // клетка ниже первого, низ — на нижней границе.
                    let vf_bottom = cell_y(TOP_BAR_ROWS + 1 + viewfinder_rows + 2);
                    let first_top = vf_bottom + cell_y(1);
                    let placeholder = |ui: &mut egui::Ui, rect: egui::Rect| {
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "\u{ead0}",
                            style::FONT_REGULAR_X2,
                            egui::Color32::from_rgb(0x30, 0x30, 0x30),
                        );
                    };
                    for (i, &(zone_start, zone_width)) in column_geometry.iter().enumerate() {
                        // Ширина — как у вьюфайндера: центр той же зоны.
                        let x = cell_x(zone_start + (zone_width - viewfinder_cols) / 2);
                        let w = cell_x(viewfinder_cols);
                        let first: egui::Rect = egui::pos2(x, first_top)
                            .into_ruler()
                            .extrude(w, cell_y(4))
                            .into();
                        let second: egui::Rect = egui::pos2(x, first.bottom() + cell_y(1))
                            .into_ruler()
                            .extrude(w, bottom.bottom_left().y - cell_y(1) - first.bottom())
                            .into();
                        guidelines::dashed_rect(ui, first);
                        guidelines::dashed_rect(ui, second);
                        placeholder(ui, first);
                        // Второй прямоугольник — круги пада: по строке на
                        // круг, сверху вниз; влезают последние.
                        // Второй прямоугольник без кругов: отсчёт —
                        // микро-таймер первого круга (мигает в такт
                        // основному таймеру); иначе пусто (символ
                        // дисконнекта — только в первом).
                        match frame.pads.get(i) {
                            Some(_) if !frame.pads[i].laps.is_empty() => {
                                let capacity = (second.height() / cell_y(1)).max(0.0) as usize;
                                let rows = lap_table(
                                    &frame.pads[i].laps,
                                    frame.laps_cursor,
                                    frame.laps_race_start,
                                );
                                let from = rows.len().saturating_sub(capacity);
                                // Клип по прямоугольнику: строка любой
                                // ширины не пересечёт рамку (справа
                                // отступ как у hint_badge_corner).
                                let painter = ui.painter().with_clip_rect(second);
                                for (row, cells) in rows[from..].iter().enumerate() {
                                    let galley = painter.layout_job(lap_row_job(cells));
                                    let pos = egui::pos2(
                                        second.left(),
                                        second.top() + cell_y(row as isize),
                                    );
                                    painter.galley(pos, galley, egui::Color32::WHITE);
                                }
                            }
                            Some(_) => {
                                if let Some(dim) = frame.countdown {
                                    // «0.000» — отсчёт первого круга:
                                    // та же пара цветов таймера, тот же
                                    // сигнал мигания. Раскладка как у
                                    // строки круга: пустая колонка имени
                                    // (мин. 2) + пробел + время (мин. 6) —
                                    // чтобы позиция совпала с первым
                                    // кругом гонки.
                                    let tone = if dim {
                                        LapTone::Inactive
                                    } else {
                                        LapTone::Active
                                    };
                                    let cells = [
                                        LapCell {
                                            text: "  ".to_owned(),
                                            tone,
                                            strike: false,
                                        },
                                        LapCell {
                                            text: lap_time(Duration::ZERO, 6),
                                            tone,
                                            strike: false,
                                        },
                                    ];
                                    let galley = ui.painter().layout_job(lap_row_job(&cells));
                                    ui.painter().galley(
                                        egui::pos2(second.left(), second.top()),
                                        galley,
                                        PAD_TIMER_ACTIVE,
                                    );
                                }
                            }
                            None => {}
                        }
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

                // Нижний ряд: EXIT · REW · PLAY · FF · RPT · REC · RACE ·
                // TIMER. Ряд — линейкой от края: каждая кнопка extrude
                // ТОЛЬКО по горизонтали, отдать rect, свернуть вправо,
                // переместиться на зазор. Ширины и зазоры (колонки):
                // EXIT 6 · 3 · REW 8 · 1 · PLAY 8 · 1 · FF 8 · 2 · RPT 6 ·
                // 3 · REC 8 · 1 · RACE 8.
                let y = ui.max_rect().bottom();

                let exit_label = "\u{e6c0}\u{f0206}";
                let btn_h = cell_y(2);
                let mut ruler = egui::pos2(0.0, y).into_ruler().extrude(0.0, -btn_h);
                let mut btn = |w: isize, gap: isize| -> egui::Rect {
                    let next = ruler.extrude(cell_x(w), 0.0);
                    ruler = next.collapse_right().translate(cell_x(gap), 0.0);
                    next.into()
                };
                let exit_rect = btn(6, 3);
                let rew_rect = btn(8, 1);
                let play_rect = btn(8, 1);
                let ff_rect = btn(8, 2);
                let rpt_rect = btn(6, 3);
                let rec_rect = btn(8, 1);
                let race_rect = btn(8, 0);

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
                let exit_response =
                    TransportButton::new(exit_label, true, false, EXIT_IDLE, EXIT_ACTIVE).interact(
                        ui,
                        exit_rect,
                        ui.make_persistent_id("flight_exit"),
                    );
                TransportButton::new(
                    exit_label,
                    true,
                    exit_response.clicked(),
                    EXIT_IDLE,
                    EXIT_ACTIVE,
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
                        TRANSPORT_IDLE,
                        TRANSPORT_ACTIVE,
                    ),
                    (
                        &frame.transport.play,
                        play_rect,
                        "flight_play",
                        "\u{f04b}",
                        TRANSPORT_IDLE,
                        TRANSPORT_ACTIVE,
                    ),
                    (
                        &frame.transport.ff,
                        ff_rect,
                        "flight_ff",
                        "\u{f04e}",
                        TRANSPORT_IDLE,
                        TRANSPORT_ACTIVE,
                    ),
                    (
                        &frame.transport.rpt,
                        rpt_rect,
                        "flight_rpt",
                        "\u{F0456}",
                        TRANSPORT_IDLE,
                        TRANSPORT_ACTIVE,
                    ),
                ];
                if let Mode::Review(d) = &mut self.phase {
                    let clip = Some(RaceClip::from_events(d.journal.iter_ms()));
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
                    // REW/FF: кадр кнопки перемотки (tap/hold/shuttle).
                    if let Some(r) = &responses[0] {
                        Self::seek_tick(ui, d, r, true, &clip);
                    }
                    if let Some(r) = &responses[2] {
                        Self::seek_tick(ui, d, r, false, &clip);
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
                        TransportButton::new("\u{f044a}", true, false, REC_IDLE, REC_ACTIVE)
                            .interact(ui, rec_rect, ui.make_persistent_id("flight_rec")),
                    )
                } else {
                    None
                };
                paint_button(
                    ui,
                    &frame.rec.rec,
                    "\u{f044a}",
                    REC_IDLE,
                    REC_ACTIVE,
                    rec_rect,
                    rec_response.as_ref().is_some_and(|r| r.clicked()),
                );
                badge(ui, rec_rect, &frame.rec.rec.badge);
                if rec_response.as_ref().is_some_and(|r| r.clicked()) {
                    action = Action::Rec;
                }

                let race_response = if frame.rec.race.enabled {
                    Some(
                        TransportButton::new("\u{f140b}", true, false, RACE_IDLE, RACE_ACTIVE)
                            .interact(ui, race_rect, ui.make_persistent_id("flight_race")),
                    )
                } else {
                    None
                };
                paint_button(
                    ui,
                    &frame.rec.race,
                    "\u{f140b}",
                    RACE_IDLE,
                    RACE_ACTIVE,
                    race_rect,
                    race_response.as_ref().is_some_and(|r| r.clicked()),
                );
                badge(ui, race_rect, &frame.rec.race.badge);
                if race_response.as_ref().is_some_and(|r| r.clicked()) {
                    action = Action::Race;
                }

                // TIMER: в live-фазах — счётчик гонки (цвет с учётом
                // мигания/нулей), в DVR — позиция.
                let timer_pos = egui::pos2(egui::Rect::from(ruler).right() + cell_x(2), y);
                let timer_icon = "\u{f43a}";
                let icon_w = measure_text(ui.ctx(), style::FONT_REGULAR_X2, timer_icon).x;
                let timer_color = if frame.timer.dim {
                    TIMER_INACTIVE
                } else {
                    TIMER_ACTIVE
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
        let dvr = Review::open(ctx, Some(journal), &self.setup);
        self.phase = Mode::Review(dvr);
    }

    /// Часы DVR в журнальном домене: (старт, позиция, конец). Полоска
    /// — окно записи (REC on → REC off); файлы лежат на ней по первым
    /// кадрам. Дегенеративная рамка (нет ни окна, ни файлов) — файловый
    /// домен (0, позиция, длительность).
    fn dvr_clock(d: &Review, clip: &Option<RaceClip>) -> (u64, u64, u64) {
        let degenerate = clip
            .as_ref()
            .is_some_and(|c| c.recording.is_none() && c.files.is_empty());
        match clip {
            Some(clip) if !degenerate => {
                let (lo, hi) = clip.recording.unwrap_or_else(|| {
                    let lo = clip.files.values().map(|(_, ms)| *ms).min().unwrap_or(0);
                    (lo, clip.end_ms().unwrap_or(lo))
                });
                // Позиция циферблата: показанный кадр мастер-камеры
                // (маппинг в сессию через ноль её файла), пока он близок
                // к позиции транспорта — жёсткое соответствие картинке.
                // Расстояние больше толеранса (гэп, мастер недоступен,
                // быстрый шаттл, где shown отстаёт на секунды) — цифра =
                // позиция транспорта. Критерий — дистанция, не окно:
                // границы как точки переключения не существует, обе
                // ветки на стыке равны — фликер невозможен.
                let tp = Some(d.transport.position());
                let pos = match (d.players.get(&d.master), clip.files.get(&d.master)) {
                    (Some(p), Some((_, rec))) => {
                        let shown_session = rec + p.shown();
                        match tp {
                            Some(tp) if tp.abs_diff(shown_session) <= DIAL_LAG_MS => shown_session,
                            _ => tp.unwrap_or(shown_session),
                        }
                    }
                    _ => tp.unwrap_or(lo),
                };
                (lo, pos.clamp(lo, hi.max(lo)), hi)
            }
            _ => {
                let pos = d.players.values().map(|p| p.shown()).max().unwrap_or(0);
                let dur = d.players.values().map(|p| p.duration()).max().unwrap_or(0);
                (0, pos.min(dur), dur)
            }
        }
    }

    /// journal-ms первого кадра камеры (ноль её файла).
    fn clip_rec_ms(clip: &Option<RaceClip>, camera_id: &str) -> Option<u64> {
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
        journal_pos: u64,
    ) -> bool {
        match clip {
            Some(clip) => match (clip.files.get(camera_id), players.get(camera_id)) {
                (Some((_, file_ms)), Some(p)) => {
                    // Покрытие сдвинуто на период кадра в обе стороны:
                    // первый и последний кадры демонстрируются интервалом
                    // (last_step), а не мгновением — иначе голова записи
                    // (record-started раньше первого кадра на доли периода)
                    // и хвост до record-finished читались бы как гэпы.
                    journal_pos + p.last_step() < *file_ms
                        || journal_pos >= *file_ms + p.duration() + p.last_step()
                }
                _ => false,
            },
            None => false,
        }
    }

    /// Границы гонки в журнальном домене (None — рамки нет).
    fn race_bounds(clip: &Option<RaceClip>) -> Option<(u64, u64)> {
        let clip = clip.as_ref()?;
        Some((clip.race_started_ms?, clip.race_finished_ms?))
    }

    /// Перемотка транспорта на delta мс в журнальном домене: база —
    /// конверт таймлайна. Плееры получат новую позицию следующим синком.
    fn seek_by(d: &mut Review, clip: &Option<RaceClip>, delta: i64) {
        let (start, pos, end) = Self::dvr_clock(d, clip);
        let target = (pos as i64 + delta).clamp(start as i64, end as i64) as u64;
        d.transport.seek(target);
    }

    /// Один кадр кнопки перемотки: press ждёт SEEK_HOLD_DELAY, отпускание
    /// до SEEK_CLICK_MAX — фиксированный шаг с вспышкой, иначе release.
    fn seek_tick(
        ui: &egui::Ui,
        d: &mut Review,
        response: &egui::Response,
        back: bool,
        clip: &Option<RaceClip>,
    ) {
        enum Event {
            Release,
            Tap,
            None,
        }
        let pointer_down = ui.input(|i| i.pointer.primary_down());
        let event = {
            let state = if back {
                &mut d.scrub.rew
            } else {
                &mut d.scrub.ff
            };
            if state.down && !pointer_down {
                state.down = false;
                Event::Release
            } else if !state.down && response.is_pointer_button_down_on() {
                state.down = true;
                state.down_since = Instant::now();
                state.hold_sent = false;
                Event::None
            } else if !state.down && response.clicked() {
                Event::Tap
            } else {
                Event::None
            }
        };
        match event {
            Event::Release => {
                let quick = if back {
                    d.scrub.rew.down_since
                } else {
                    d.scrub.ff.down_since
                }
                .elapsed()
                    < SEEK_CLICK_MAX;
                if quick {
                    if if back {
                        d.scrub.rew.hold_sent
                    } else {
                        d.scrub.ff.hold_sent
                    } {
                        d.transport.shuttle_release(SEEK_RELEASE_RAMP);
                    }
                    let state = if back {
                        &mut d.scrub.rew
                    } else {
                        &mut d.scrub.ff
                    };
                    state.flash = true;
                    let step = SEEK_SPEEDS[d.scrub.seek_speed] as i64 * SEEK_TAP as i64;
                    Self::seek_by(d, clip, if back { -step } else { step });
                } else {
                    d.transport.shuttle_release(SEEK_RELEASE_RAMP);
                }
            }
            Event::Tap => {
                let state = if back {
                    &mut d.scrub.rew
                } else {
                    &mut d.scrub.ff
                };
                state.flash = true;
                let step = SEEK_SPEEDS[d.scrub.seek_speed] as i64 * SEEK_TAP as i64;
                Self::seek_by(d, clip, if back { -step } else { step });
            }
            Event::None => {}
        }
        let arm = {
            let state = if back {
                &mut d.scrub.rew
            } else {
                &mut d.scrub.ff
            };
            if state.down && !state.hold_sent {
                ui.ctx().request_repaint();
            }
            state.down && !state.hold_sent && state.down_since.elapsed() >= SEEK_HOLD_DELAY
        };
        if arm {
            let state = if back {
                &mut d.scrub.rew
            } else {
                &mut d.scrub.ff
            };
            state.hold_sent = true;
            let speed = SEEK_SPEEDS[d.scrub.seek_speed] as f64 * if back { -1.0 } else { 1.0 };
            d.transport.shuttle(speed);
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
                let clip = Some(RaceClip::from_events(d.journal.iter_ms()));
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
    /// Полный набор данных DVR из журнала (None — пустой DVR: никаких
    /// чужих записей, только файлы журнала). Плееры открываются
    /// асинхронно — транзит, пока все не открылись (Ready/Failed).
    fn open(ctx: &egui::Context, journal: Option<Journal>, setup: &Setup) -> Self {
        let journal = journal.unwrap_or_else(|| {
            let date_time = time::OffsetDateTime::now_local().unwrap_or_else(|_| {
                log::warn!("flight: local time unavailable, journal falls back to UTC");
                time::OffsetDateTime::now_utc()
            });
            Journal::new(date_time, setup.pads.clone(), HashMap::new())
        });
        let clip = RaceClip::from_events(journal.iter_ms());
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
        // (каждая камера покажет свой предшествующий кадр).
        let span = clip
            .recording
            .map(|(_, hi)| hi)
            .or_else(|| clip.end_ms())
            .unwrap_or(0);
        let mut transport = Transport::new(Instant::now(), span);
        if let Some(cd) = clip.race_countdown_ms {
            transport.seek(cd.min(span));
        }
        Self {
            journal,
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
                hscroll: HScroll::new(),
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
pub fn viewfinder_rows(count: usize) -> isize {
    match count {
        1 | 2 => 21,
        3 => 15,
        _ => 12,
    }
}

/// Ширина вьюфайндера из соотношения 16:9 (8 клеток на 3 строки).
pub fn viewfinder_cols(count: usize) -> isize {
    viewfinder_rows(count) * 8 / 3
}

/// мм:ss от длительности.
fn mmss(dur: Duration) -> String {
    let secs = dur.as_secs();
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

/// Тон ячейки: активный (пара таймера), потухший (мигание отсчёта),
/// будущий (ещё не случилось — раскраска DVR по курсору).
#[derive(Clone, Copy)]
enum LapTone {
    Active,
    Inactive,
    Future,
}

/// Ячейка строки круга: текст, тон, зачёркивание.
struct LapCell {
    text: String,
    tone: LapTone,
    strike: bool,
}

/// Таблица кругов: одна строка на круг, колонки «имя время события»
/// через пробел (ширины — один проход по всем строкам). Имя и время
/// выравнены вправо; минимальные ширины — 2 и 6 (без разделителей).
/// Имя — из дистилятора («HS», «1)» — скобку ставит он); текущий
/// несёт предполагаемое имя (своё «HS»/«N)», даже с крестом);
/// проваленный отбит пробелами. Время: формат по наибольшему кругу
/// включая текущий: SS.mmm, далее M:SS.mmm, далее MM:SS.mmm; ведущие
/// нули — пробелами, ширина резервирована. События: fault ✕,
/// correct □, lap ○, через пробел, влево.
///
/// Раскраска: `cursor` (мс журнала) — элемент серым, пока его метка
/// в будущем: имя и время круга — до начала круга (старт = конец
/// предыдущего, первый — `race_start`), событие — до его метки.
/// None — без раскраски (эфир: всё в прошлом).
fn lap_table(
    lines: &[(u64, LapLine, Vec<(u64, LapEvent)>)],
    cursor: Option<u64>,
    race_start: u64,
) -> Vec<Vec<LapCell>> {
    // Время строки; у текущего — бегущее (он участвует и в выборе
    // формата колонки).
    let duration = |line: &LapLine| match line {
        LapLine::Success { duration, .. } | LapLine::Fail { duration } => *duration,
        LapLine::Current { since } => since.elapsed(),
    };
    let max_ms = lines
        .iter()
        .map(|(_, line, _)| duration(line))
        .map(|d| d.as_millis())
        .max()
        .unwrap_or(0);
    // Минимум 6 («SS.mmm») — время всегда в полной ширине колонки.
    let time_width = if max_ms >= 600_000 {
        9 // MM:SS.mmm
    } else if max_ms >= 60_000 {
        8 // M:SS.mmm
    } else {
        6 // SS.mmm
    };
    // Текущий круг — предполагаемое имя: что получит, если завершится
    // успехом (HS, если успешных ещё не было; иначе следующий номер).
    // Нумерация совпадает с дистилятором: fail номер не съедает.
    let successes = lines
        .iter()
        .filter(|(_, line, _)| matches!(line, LapLine::Success { .. }))
        .count();
    let current_name = if successes == 0 {
        "HS".to_owned()
    } else {
        format!("{successes})")
    };
    let future = |at: u64| cursor.is_some_and(|c| c < at);
    let has_events = lines.iter().any(|(_, _, ev)| !ev.is_empty());
    let mut rows = Vec::new();
    let mut lap_start = race_start;
    for (end, line, events) in lines {
        // Имя — из дистилятора целиком (HS без скобки, «1)» с ней).
        let name = match line {
            LapLine::Success { num, .. } => num.clone(),
            LapLine::Current { .. } => current_name.clone(),
            LapLine::Fail { .. } => String::new(), // зафейленный отбит пробелами
        };
        let tone = |future_mark: bool| {
            if future_mark {
                LapTone::Future
            } else {
                LapTone::Active
            }
        };
        let mut cells = vec![
            LapCell {
                text: name,
                tone: tone(future(lap_start)),
                strike: false,
            },
            // Номер и время белеют вместе: когда круг начался. События
            // — по своим меткам.
            LapCell {
                text: lap_time(duration(line), time_width),
                tone: tone(future(lap_start)),
                strike: matches!(line, LapLine::Fail { .. }),
            },
        ];
        if has_events {
            for (at, event) in events {
                cells.push(LapCell {
                    text: match event {
                        LapEvent::Fault => "\u{f0156}".to_owned(),
                        LapEvent::Correction => "□".to_owned(),
                        LapEvent::Lap => "\u{25cb}".to_owned(), // полый круг
                    },
                    tone: tone(future(*at)),
                    strike: false,
                });
            }
        }
        rows.push(cells);
        lap_start = *end;
    }
    // Минимум 2 («HS») — узкие номера («1)») отбиваются пробелами.
    let name_width = rows
        .iter()
        .flat_map(|r| r.first())
        .map(|c| c.text.chars().count())
        .max()
        .unwrap_or(0)
        .max(2);
    // Выравнивание: имя и время вправо (пробелами), события — как есть.
    for cells in &mut rows {
        for (i, cell) in cells.iter_mut().enumerate() {
            let width = match i {
                0 => name_width,
                1 => time_width,
                _ => continue,
            };
            let pad = width.saturating_sub(cell.text.chars().count());
            cell.text = format!("{}{}", " ".repeat(pad), cell.text);
        }
    }
    rows
}

/// LayoutJob строки круга из ячеек: разделитель — пробел, серое —
/// «ещё не случилось», зачёркнутое время провала.
fn lap_row_job(cells: &[LapCell]) -> egui::text::LayoutJob {
    let format = |cell: &LapCell| {
        // Зачёркивание — в цвет текста ячейки.
        let color = match cell.tone {
            LapTone::Active => PAD_TIMER_ACTIVE,
            LapTone::Inactive => PAD_TIMER_INACTIVE,
            LapTone::Future => LAP_FUTURE,
        };
        egui::TextFormat {
            font_id: style::FONT_REGULAR,
            color,
            strikethrough: if cell.strike {
                egui::Stroke::new(1.5, color)
            } else {
                egui::Stroke::NONE
            },
            ..Default::default()
        }
    };
    let mut job = egui::text::LayoutJob::default();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            job.append(" ", 0.0, format(cell));
        }
        job.append(&cell.text, 0.0, format(cell));
    }
    job
}

/// Время круга. Порядок: рендер «как есть» (круг короче минуты — без
/// «:» даже в минутном формате), затем ведущие нули целой части —
/// пробелами (последний ноль нуля-секунд сохраняем), затем отбивка
/// пробелами до ширины (минимум 6 — «SS.mmm»).
fn lap_time(dur: Duration, width: usize) -> String {
    let millis = dur.as_millis() as u64;
    let (m, sec, ms) = (millis / 60_000, millis / 1000 % 60, millis % 1000);
    let s = if m > 0 {
        if width >= 9 {
            format!("{m:02}:{sec:02}.{ms:03}")
        } else {
            format!("{m}:{sec:02}.{ms:03}")
        }
    } else {
        format!("{sec:02}.{ms:03}")
    };
    let dot = s.find('.').unwrap();
    let mut chars: Vec<char> = s.chars().collect();
    for c in chars[..dot].iter_mut() {
        if *c == '0' {
            *c = ' ';
        } else {
            break;
        }
    }
    // ноль-секунд: «  .123» не бывает — возвращаем ноль перед точкой
    if chars[..dot].iter().all(|c| *c == ' ') {
        chars[dot - 1] = '0';
    }
    format!(
        "{:>w$}",
        chars.into_iter().collect::<String>(),
        w = width.max(6)
    )
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
