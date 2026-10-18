//! Экран Flight: совмещает эфир (трансляция/запись/гонка) и DVR
//! (просмотр записи: транспорт, таймлайн, круг повтора). Режим входа —
//! Live (сразу эфир) или Dvr (сразу просмотр, CLI `laps scrub`). Выход —
//! EXIT в меню. Гоночное состояние живёт в потоке ядра
//! (model::rules + service::race), экран — намерения и рендер снапшотов.

use super::ruler::{self, IntoRuler};
use super::*;
use crate::config::camera::CameraConfig;
use crate::config::setup::{PadConfig, Setup};
use crate::driver::camera::CaptureState;
use crate::driver::keyboard::{AttachError, KeyAction, KeyEvent};
use crate::driver::player::{Player, PlayerState};
use crate::model::flight::{Journal, RaceClip, race_clip};
use crate::model::pilot::Pilot;
use crate::model::rules::RaceRules;
use crate::service::race::{self, AirPhase, AirState, Intent, Notice, Race};
use crate::service::transport::{Circle, Transport};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_PADS: usize = 4;
const GRID_WIDTH: isize = 160;
const LEFT_MARGIN: isize = 4;
const RIGHT_MARGIN: isize = 4;
const TOP_BAR_ROWS: isize = 2;

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

/// Экранный режим: эфир (зеркало состояния гоночного ядра) или DVR
/// (транзит открытия плееров и просмотр). Гоночные фазы живут в ядре
/// (service::race), здесь — только отображение.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Air,
    DvrOpening,
    Dvr,
}

/// Режим входа: Live — сразу в эфире (главный экран), Dvr — сразу
/// просмотр записи (CLI `laps scrub`).
#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Live,
    Dvr,
}

pub struct Flight {
    /// Экранный режим (эфир/DVR). Гоночные фазы — в ядре, здесь только
    /// последний снапшот для рендера.
    screen: Screen,
    air: AirState,
    /// Гоночный поток: намерения туда, уведомления оттуда. None —
    /// холодный режим DVR (гонки не было).
    race: Option<Race>,
    webcams: HashMap<String, crate::driver::camera::Camera>,
    awake: Option<keepawake::KeepAwake>,
    rules: RaceRules,
    /// Каталог записей (None — без CLI-аргумента scrub): для DVR-режима.
    captures: Option<PathBuf>,
    /// camera_id → плеер записи (DVR-режим).
    players: HashMap<String, Player>,
    timeline: scrub::Timeline,
    /// Перемотка REW/FF: состояние удержания.
    rew: scrub::SeekButton,
    ff: scrub::SeekButton,
    /// Индексы шкал скоростей (скролл над кнопками).
    seek_speed: usize,
    play_speed: usize,
    rpt_size: usize,
    vscroll: scrub::ScrollGate,
    hscroll: scrub::HScroll,
    res: Option<Res>,
    /// Запрошен выход (remote/кнопка/⌘W): ядру Intent::Exit уходит
    /// сразу, флаг — для навигации в меню из update (нужен Navigator).
    exit_requested: bool,
    header: header::Header,
    setup: Setup,
    assignments: HashMap<String, Pilot>,
    active_cameras: HashMap<String, CameraConfig>,
    /// Транспорт DVR (только DVR-экран): общая головка в домене сессии
    /// записи. Плееры — доставщики: каждый кадр позиция и скорость
    /// уходят в плееры синками, worker'ы экстраполируют на своих часах.
    transport: Option<Transport>,
    /// Мастер-камера: её показанный кадр — источник позиции циферблата
    /// (жёсткое соответствие цифры и картинки). Первая по порядку сетапа
    /// камера с файлом; переключаемая в рантайме.
    master: String,
    /// Журнал полёта — владение в GUI: приходит с EnterDvr от ядра
    /// (прямой путь) или парсится из файла (холодный вход). Правки DVR
    /// (добавление/удаление событий) — здесь.
    journal: Option<Journal>,
    /// Аттач настроенных клавиатур (Live): Ok — дрейнит поток гонки,
    /// Err — причина (плашка дисконнекта). В DVR-режиме пусто.
    keyboards: HashMap<String, Result<(), AttachError>>,
    /// Очередь клавиатурных событий из потока гонки (дренится раз в
    /// кадр в held_keys).
    key_events: Arc<Mutex<Vec<KeyEvent>>>,
    /// Клавиатуры с оборвавшимся каналом (поток гонки кладёт id, GUI
    /// переводит аттач в Err и снимает удерживаемые символы).
    key_dead: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Удерживаемые клавиши по падам (символы `key_name`) для нижнего
    /// прямоугольника: Press добавляет, Release убирает; закрытый канал
    /// клавиатуры снимает все её символы.
    held_keys: HashMap<String, Vec<String>>,
}

impl From<Box<Flight>> for State {
    fn from(val: Box<Flight>) -> Self {
        State::Flight(val)
    }
}

impl Flight {
    /// Дрен клавиатурных событий из общей очереди потока гонки в
    /// per-pad состояние удерживаемых клавиш. Обрыв канала (id в
    /// key_dead) снимает все символы клавиатуры и переводит аттач в Err
    /// — плашка дисконнекта. Возвращает true, если состояние могло
    /// измениться, — caller просит перерисовку.
    fn drain_keyboards(&mut self) -> bool {
        let events = std::mem::take(&mut *self.key_events.lock().expect("key events"));
        for event in &events {
            Self::apply_key_event(&self.setup.pads, &mut self.held_keys, event);
        }
        let dead: Vec<String> = std::mem::take(&mut *self.key_dead.lock().expect("key dead"))
            .into_iter()
            .collect();
        for keyboard_id in &dead {
            if let Some(slot) = self.keyboards.get_mut(keyboard_id) {
                *slot = Err(AttachError::NoMatch); // канал закрылся — как "не найдена"
            }
            Self::drop_keyboard_keys(&self.setup.pads, &mut self.held_keys, keyboard_id);
        }
        !events.is_empty() || !dead.is_empty()
    }

    /// Событие → per-pad удерживаемые клавиши: совпали бинды пада по
    /// keyboard_id и usage — символ клавиши добавляется (Press) или
    /// убирается (Release). Незабинженные клавиши ни на что не влияют.
    fn apply_key_event(
        pads: &BTreeMap<String, PadConfig>,
        held: &mut HashMap<String, Vec<String>>,
        event: &KeyEvent,
    ) {
        for (pad_id, pad) in pads {
            let bound = pad
                .keybinds
                .iter()
                .any(|b| b.keyboard_id == event.keyboard_id && b.key == event.keycode);
            if !bound {
                continue;
            }
            let symbols = held.entry(pad_id.clone()).or_default();
            let symbol = crate::driver::keyboard::key_name(event.keycode);
            match event.action {
                KeyAction::Press => {
                    if !symbols.contains(&symbol) {
                        symbols.push(symbol);
                    }
                }
                KeyAction::Release => {
                    symbols.retain(|s| *s != symbol);
                }
            }
        }
    }

    /// Снять все символы клавиатуры: канал закрылся — удерживаемые ею
    /// клавиши не должны зависать в плейсхолдерах.
    fn drop_keyboard_keys(
        pads: &BTreeMap<String, PadConfig>,
        held: &mut HashMap<String, Vec<String>>,
        keyboard_id: &str,
    ) {
        for (pad_id, pad) in pads {
            let usages: Vec<u32> = pad
                .keybinds
                .iter()
                .filter(|b| b.keyboard_id == keyboard_id)
                .map(|b| b.key)
                .collect();
            let Some(symbols) = held.get_mut(pad_id) else {
                continue;
            };
            for usage in usages {
                let symbol = crate::driver::keyboard::key_name(usage);
                symbols.retain(|s| *s != symbol);
            }
        }
    }

    /// Главный экран — сразу в эфире: захват стартует в конструкторе.
    pub fn live(
        ctx: &egui::Context,
        setup: Setup,
        assignments: HashMap<String, Pilot>,
        rules: RaceRules,
        captures: Option<PathBuf>,
    ) -> Self {
        Self::build(ctx, setup, assignments, rules, captures, Mode::Live)
    }

    /// Режим просмотра (CLI `laps scrub`) — без эфира, плееры открыты
    /// на старте, экран сразу в DVR.
    pub fn dvr(
        ctx: &egui::Context,
        setup: Setup,
        assignments: HashMap<String, Pilot>,
        captures: PathBuf,
    ) -> Self {
        Self::build(
            ctx,
            setup,
            assignments,
            RaceRules::default(),
            Some(captures),
            Mode::Dvr,
        )
    }

    fn build(
        ctx: &egui::Context,
        setup: Setup,
        assignments: HashMap<String, Pilot>,
        rules: RaceRules,
        captures: Option<PathBuf>,
        mode: Mode,
    ) -> Self {
        let active_cameras: HashMap<String, CameraConfig> = setup
            .pads
            .iter()
            .filter(|(pad_id, _)| assignments.contains_key(*pad_id))
            .filter_map(|(_, pad)| {
                setup
                    .cameras
                    .get(&pad.fpv.camera_id)
                    .map(|camera| (pad.fpv.camera_id.clone(), camera.clone()))
            })
            .take(MAX_PADS)
            .collect();
        let mut this = Self {
            screen: if mode == Mode::Dvr {
                Screen::DvrOpening
            } else {
                Screen::Air
            },
            air: AirState {
                phase: AirPhase::LiveBefore,
                countdown_end: None,
                race: None,
                race_done: None,
            },
            race: None,
            transport: None,
            master: String::new(),
            webcams: HashMap::new(),
            awake: None,
            rules,
            captures,
            players: HashMap::new(),
            timeline: scrub::Timeline::new(),
            rew: scrub::SeekButton::new(),
            ff: scrub::SeekButton::new(),
            seek_speed: scrub::SEEK_SPEEDS.iter().position(|&v| v == 10).unwrap(),
            play_speed: scrub::PLAY_SPEEDS.iter().position(|&v| v == 100).unwrap(),
            rpt_size: scrub::RPT_SIZES.iter().position(|&v| v == 1.0).unwrap(),
            vscroll: scrub::ScrollGate::new(),
            hscroll: scrub::HScroll::new(),
            res: None,
            exit_requested: false,
            header: header::Header::new(),
            setup,
            assignments,
            active_cameras,
            journal: None,
            keyboards: HashMap::new(),
            key_events: Arc::new(Mutex::new(Vec::new())),
            key_dead: Arc::new(Mutex::new(std::collections::HashSet::new())),
            held_keys: HashMap::new(),
        };
        if mode == Mode::Live {
            this.start_captures(ctx);
            this.screen_awake();
            // Клавиатуры: один аттач на запись сетапа (дедуп — сама
            // карта). Ошибки старту не мешают: падам с keybinds
            // рисуется плашка дисконнекта. Ok-каналы уходят в поток
            // гонки — он дрейнит их в key_events и будит UI. open —
            // платформенный фасад (не-linux отвечает NoMatch).
            let mut key_streams = HashMap::new();
            this.keyboards = this
                .setup
                .keyboards
                .iter()
                .map(|(id, cfg)| {
                    let attached = match crate::driver::keyboard::open(id, cfg) {
                        Ok(rx) => {
                            key_streams.insert(id.clone(), rx);
                            Ok(())
                        }
                        Err(e) => Err(e),
                    };
                    (id.clone(), attached)
                })
                .collect();
            for (id, result) in &this.keyboards {
                if let Err(e) = result {
                    log::warn!("keyboard {id} not attached: {e}");
                }
            }
            // Гоночный поток: ядро (часы, политика записи, журнал) плюс
            // исполнение эффектов (команды рекордерам, звук) и дрен
            // клавиатур.
            let recorders: HashMap<_, _> = this
                .webcams
                .iter()
                .map(|(id, camera)| (id.clone(), camera.recorder()))
                .collect();
            let key_events = this.key_events.clone();
            let key_dead = this.key_dead.clone();
            let ctx = ctx.clone();
            this.race = Some(race::spawn(
                this.rules,
                std::path::PathBuf::from(crate::driver::dvr::CAPTURES_DIR),
                this.setup.pads.clone(),
                this.assignments.clone(),
                this.active_cameras.clone(),
                recorders,
                key_streams,
                key_events,
                key_dead,
                Arc::new(move || ctx.request_repaint()),
            ));
        } else {
            // Холодный вход в скраббинг: рамка гонки — из последнего
            // Cold-вход: журнал парсится из файла, дальше — как warm.
            // Журнала нет: деградация на последние записи камер.
            if !this.open_dvr_cold(ctx) {
                this.open_dvr_players(ctx, &RaceClip::default());
                this.screen = Screen::DvrOpening;
            }
        }
        this
    }

    pub fn update(&mut self, ui: &mut egui::Ui, navigator: &mut Navigator) {
        enum Action {
            Exit,
            None,
        }
        let mut action = Action::None;
        if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::W)) {
            action = Action::Exit;
        }
        // Уведомления гоночного ядра: снапшоты состояния и закрытие
        // гонки (журнал переходит нам для DVR).
        let mut pending = Vec::new();
        if let Some(race) = &self.race {
            while let Ok(notice) = race.notices.try_recv() {
                pending.push(notice);
            }
        }
        for notice in pending {
            match notice {
                Notice::State(air) => self.air = air,
                Notice::EnterDvr { journal } => self.open_dvr_warm(ui.ctx(), journal),
            }
        }
        self.dvr_tick();
        self.arm_race_repaint(ui.ctx());
        self.arm_transport_repaint(ui.ctx());
        if self.drain_keyboards() {
            // Удерживаемые клавиши изменились — следующий кадр по
            // собственному расписанию может не наступить (нет видео).
            ui.ctx().request_repaint();
        }
        self.rew.flash = false;
        self.ff.flash = false;

        let ctx = ui.ctx();
        self.res.get_or_insert_with(|| Res::new(ctx));

        // Рамка гонки — производное от журнала (без отдельного кэша:
        // правки журнала видны скраббингу немедленно).
        let clip = self
            .journal
            .as_ref()
            .map(|journal| race_clip(journal.iter_ms()));
        // Время DVR — до отрисовки: шкала под кнопками. Журнальный
        // домен: таймлайн — от старта первой камеры до финиша
        // последней; без рамки журнала — файловый домен (старт 0).
        let (dvr_start, dvr_pos, dvr_end) = self.dvr_clock(&clip);
        let dvr_span = dvr_end.saturating_sub(dvr_start).max(1);
        let dvr_ready = self.dvr_ready();

        // Транспорт → плееры: позиция (сессия) и скорость синком на
        // каждый кадр UI; worker'ы экстраполируют между синками на
        // своих часах (пейсинг не зависит от частоты кадров UI).
        if let Some(t) = &mut self.transport {
            t.sync();
            let pos = t.position();
            let speed = t.velocity();
            for (id, player) in self.players.iter() {
                let file = Self::clip_rec_ms(&clip, id).map_or(pos, |rec| pos.saturating_sub(rec));
                player.sync(file.min(player.duration()), speed);
            }
        }

        view::Letterbox::new(egui::vec2(ruler::cell_x(GRID_WIDTH), ruler::cell_y(45)))
            .max_virtual_height(ruler::cell_y(50))
            .outline(true)
            .show(ui, |ui| {
                let header_rect: egui::Rect = ui
                    .max_rect()
                    .into_ruler()
                    .collapse_top()
                    .extrude(0.0, ruler::cell_y(TOP_BAR_ROWS))
                    .into();
                self.header.show(ui, header_rect);

                // Таймлайн (только DVR): рисуется раньше кнопок — под
                // ними. Две клетки высотой, вся ширина, поля по 4
                // колонки. Головка ходит центрами от края до края.
                let dvr = matches!(self.screen, Screen::Dvr);
                let timeline_rect: egui::Rect = ui
                    .max_rect()
                    .into_ruler()
                    .collapse_bottom()
                    .translate(0.0, -ruler::cell_y(4))
                    .extrude(0.0, ruler::cell_y(2))
                    .shrink(ruler::cell_x(LEFT_MARGIN), 0.0)
                    .shrink(-ruler::cell_x(RIGHT_MARGIN), 0.0)
                    .into();
                if dvr && dvr_ready {
                    let frac = dvr_pos.saturating_sub(dvr_start) as f32 / dvr_span as f32;
                    // Круг зацикливания: доли (начало, конец) — для
                    // индикатора-капсулы на головке. Транспорт уже в
                    // домене сессии — маппинг не нужен.
                    let circle = self.transport.as_ref().and_then(|t| t.circle()).map(|c| {
                        let lo = c.center.saturating_sub(c.radius).saturating_sub(dvr_start);
                        let hi = (c.center + c.radius).min(dvr_end).saturating_sub(dvr_start);
                        (lo as f32 / dvr_span as f32, hi as f32 / dvr_span as f32)
                    });
                    // Растяжение сосиски до минимума: реальный круг уже
                    // минимума — капсула и голова идут по общему скейлу;
                    // иначе капсула растягивается до CIRCLE_MIN_WIDTH_PX,
                    // а внутри неё голова (и drag) течёт по локальному
                    // скейлу круга.
                    let min_w = (scrub::CIRCLE_MIN_WIDTH_PX
                        / ui.ctx().pixels_per_point()
                        / timeline_rect.width())
                    .min(1.0);
                    let (circle, circle_real, knob) = match circle {
                        Some((lo, hi)) if hi - lo < min_w => {
                            let center = ((lo + hi) / 2.0).clamp(min_w / 2.0, 1.0 - min_w / 2.0);
                            let nlo = center - min_w / 2.0;
                            let nhi = center + min_w / 2.0;
                            let inner = ((frac - lo) / (hi - lo).max(f32::EPSILON)).clamp(0.0, 1.0);
                            (Some((nlo, nhi)), Some((lo, hi)), nlo + inner * (nhi - nlo))
                        }
                        other => (other, None, frac),
                    };
                    // Активный фрагмент (вылет) — границы гонки из
                    // журнала: белым, края трека 40%. Без рамки вся
                    // линия серая.
                    let window = Self::race_bounds(&clip).map(|(lo, hi)| {
                        (
                            lo.saturating_sub(dvr_start) as f32 / dvr_span as f32,
                            hi.min(dvr_end).saturating_sub(dvr_start) as f32 / dvr_span as f32,
                        )
                    });
                    // Точка отсчёта: момент RaceCountdown на полоске.
                    let marker = clip
                        .as_ref()
                        .and_then(|clip| clip.race_countdown_ms)
                        .map(|cd| {
                            (cd.saturating_sub(dvr_start) as f32 / dvr_span as f32).clamp(0.0, 1.0)
                        });
                    // Каретка — цветом активного транспортного контролла.
                    let (response, events) = self.timeline.show(
                        ui,
                        timeline_rect,
                        knob,
                        dvr_span,
                        window,
                        circle,
                        circle_real,
                        marker,
                    );
                    for event in events {
                        match event {
                            scrub::TimelineEvent::SetDragHeld(held) => {
                                if let Some(t) = &mut self.transport {
                                    t.set_drag_held(held);
                                }
                            }
                            scrub::TimelineEvent::Seek(t_ms) => {
                                // t — мс от начала таймлайна: в домен
                                // сессии (транспорт сам клампит к шкале).
                                if let Some(t) = &mut self.transport {
                                    t.seek(dvr_start + t_ms);
                                }
                            }
                        }
                    }
                    let dx = if response.hovered() {
                        ui.input(|i| i.smooth_scroll_delta.x)
                    } else {
                        0.0
                    };
                    if let Some(ms) = self.hscroll.feed(dx, ui.ctx().pixels_per_point()) {
                        self.seek_by(&clip, ms);
                    }
                    if self.hscroll.active() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                        ui.ctx().request_repaint();
                    }
                }

                // Картинка каждого плеера/камеры — в viewport его камеры.
                let mut contents: HashMap<String, viewfinder::ViewfinderContents> = HashMap::new();
                for (id, webcam) in self.webcams.iter_mut() {
                    let capture_state = webcam.capture_state();
                    webcam.update_frame();
                    let state = match capture_state {
                        CaptureState::Live => {
                            viewfinder::ViewfinderContents::Texture(webcam.texture())
                        }
                        // Мёртвый поток — ошибка камеры, не ожидание.
                        CaptureState::Dead => viewfinder::ViewfinderContents::Error,
                        CaptureState::Starting => viewfinder::ViewfinderContents::Pending,
                    };
                    contents.insert(id.clone(), state);
                }
                // Разрывы записи в журнальном домене: где камера в этой
                // точке таймлайна не пишет (файл не начался/закончился) —
                // Pending (серый глиф).
                let gaps: HashMap<String, bool> = self
                    .players
                    .keys()
                    .map(|id| {
                        (
                            id.clone(),
                            Self::clip_gap(&clip, &self.players, id, dvr_pos),
                        )
                    })
                    .collect();
                for (id, player) in self.players.iter_mut() {
                    player.update_frame();
                    let kind = match player.open_state() {
                        PlayerState::Ready => {
                            if *gaps.get(id).unwrap_or(&false) {
                                viewfinder::ViewfinderContents::Pending
                            } else {
                                viewfinder::ViewfinderContents::Texture(player.texture())
                            }
                        }
                        PlayerState::Failed(_) => viewfinder::ViewfinderContents::Error,
                        PlayerState::Opening => viewfinder::ViewfinderContents::Pending,
                    };
                    contents.insert(id.clone(), kind);
                }
                // Камеры без файла в DVR (запись не состоялась): плеера
                // нет — красный плюс по центру, как у мёртвой камеры.
                if matches!(self.screen, Screen::Dvr) {
                    for id in self.active_cameras.keys() {
                        if !contents.contains_key(id) {
                            contents.insert(id.clone(), viewfinder::ViewfinderContents::Error);
                        }
                    }
                }

                // Alt: разрешения для значков вьюфайндеров (мёртвый
                // поток/нет плеера → error).
                let alt = ui.input(|i| i.modifiers.alt);
                let frame_res: HashMap<String, Option<crate::config::camera::ResolutionConfig>> =
                    if alt {
                        self.active_cameras
                            .iter()
                            .map(|(id, cfg)| {
                                let res = match (self.webcams.get(id), self.players.get(id)) {
                                    (Some(w), _) if w.capture_state() == CaptureState::Dead => None,
                                    (Some(w), _) => Some(w.resolution().unwrap_or(cfg.resolution)),
                                    (None, Some(p)) => {
                                        p.resolution().map_or(Some(cfg.resolution), Some)
                                    }
                                    _ => Some(cfg.resolution),
                                };
                                (id.clone(), res)
                            })
                            .collect()
                    } else {
                        HashMap::new()
                    };

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
                    let top = screen.collapse_top().translate(0.0, ruler::cell_y(2));
                    let bottom = screen.collapse_bottom().translate(0.0, -ruler::cell_y(4));
                    let stroke = egui::Stroke::new(2.0, egui::Color32::from_white_alpha(0x1e));
                    for i in 1..columns {
                        let x = ruler::cell_x(LEFT_MARGIN + segment * i);
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
                    let vf_bottom = ruler::cell_y(TOP_BAR_ROWS + 1 + viewfinder_rows + 2);
                    let first_top = vf_bottom + ruler::cell_y(1);
                    let dvr = matches!(self.screen, Screen::Dvr);
                    // Плейсхолдер: плюс двойного размера, тёмно-серый.
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
                        let x = ruler::cell_x(zone_start + (zone_width - viewfinder_cols) / 2);
                        let w = ruler::cell_x(viewfinder_cols);
                        let first: egui::Rect = egui::pos2(x, first_top)
                            .into_ruler()
                            .extrude(w, ruler::cell_y(4))
                            .into();
                        let second: egui::Rect = egui::pos2(x, first.bottom() + ruler::cell_y(1))
                            .into_ruler()
                            .extrude(
                                w,
                                bottom.bottom_left().y - ruler::cell_y(1) - first.bottom(),
                            )
                            .into();
                        guidelines::dashed_rect(ui, first);
                        guidelines::dashed_rect(ui, second);
                        // Верхний прямоугольник: плейсхолдер как раньше.
                        placeholder(ui, first);
                        // Нижний: в DVR — как раньше; в эфире — по биндам
                        // пада (колонка привязана к pads[i]).
                        if dvr {
                            placeholder(ui, second);
                            continue;
                        }
                        let (pad_id, pad) = pads[i];
                        if pad.keybinds.is_empty() {
                            placeholder(ui, second);
                            continue;
                        }
                        let attached = pad.keybinds.iter().all(|b| {
                            self.keyboards
                                .get(&b.keyboard_id)
                                .is_some_and(|r| r.is_ok())
                        });
                        if !attached {
                            // Клавиатура не аттачена — красный дисконнект.
                            ui.painter().text(
                                second.center(),
                                egui::Align2::CENTER_CENTER,
                                "\u{f0319}", // mdi-lan-disconnect
                                style::FONT_REGULAR_X2,
                                egui::Color32::from_rgb(0xFF, 0x33, 0x33),
                            );
                        } else if let Some(symbols) =
                            self.held_keys.get(pad_id).filter(|s| !s.is_empty())
                        {
                            // Удерживаемые клавиши — белым одинарным
                            // шрифтом, в левом верхнем углу с 1-клеточным
                            // инсетом, через ", ".
                            ui.painter().text(
                                second.min + egui::vec2(ruler::cell_x(1), ruler::cell_y(1)),
                                egui::Align2::LEFT_TOP,
                                symbols.join(", "),
                                style::FONT_REGULAR,
                                egui::Color32::WHITE,
                            );
                        }
                    }
                }

                if columns > 0 {
                    for (i, (pad_id, pad)) in pads.into_iter().enumerate() {
                        let (zone_start, zone_width) = column_geometry[i];
                        let col = zone_start + (zone_width - viewfinder_cols) / 2;
                        let ruler = egui::pos2(ruler::cell_x(col), ruler::cell_y(TOP_BAR_ROWS + 1))
                            .into_ruler();
                        let viewfinder = ruler.extrude(
                            ruler::cell_x(viewfinder_cols),
                            ruler::cell_y(viewfinder_rows + 2),
                        );
                        let frame_rect: egui::Rect = viewfinder.into();
                        let image_contents = contents
                            .get(&pad.fpv.camera_id)
                            .copied()
                            .unwrap_or(viewfinder::ViewfinderContents::Off);
                        let pilot_name = self
                            .assignments
                            .get(pad_id)
                            .map_or("", |pilot| pilot.name.as_str());
                        viewfinder::ViewfinderFrame::new(
                            &pad.label,
                            pilot_name,
                            pad.color.to_color32(),
                        )
                        .show(ui, frame_rect, |ui, rect| {
                            viewfinder::Viewfinder::new(pad.fpv.viewport.to_rect()).show(
                                ui,
                                image_contents,
                                rect,
                            );
                            // Alt: эффективное разрешение вьюпорта
                            // (в правом нижнем углу картинки).
                            if let Some(res) = frame_res.get(&pad.fpv.camera_id) {
                                let val = res
                                    .map(|r| {
                                        let lines = r.height as f32
                                            * (pad.fpv.viewport.bottom - pad.fpv.viewport.top);
                                        format!("{}p", lines.round() as u32)
                                    })
                                    .unwrap_or_else(|| "error".to_owned());
                                view::hint_badge_corner(ui, rect, &[val]);
                            }
                        });
                    }
                }

                // Нижний ряд: EXIT · REW · PLAY · FF · RPT · REC · RACE ·
                // TIMER. Кнопки простым стилем транспорта, только иконки
                // по центру; EXIT — плашка с иконкой.
                let y = ui.max_rect().bottom();

                let rec_on = self.air.rec_on();
                let race_on = self.air.countdown_active() || self.air.race.is_some();
                // Иконки не меняются между стейтами — состояние видно
                // цветом кнопки.
                let rec_icon = "\u{f044a}";
                let race_icon = "\u{f140b}";

                // Ряд — линейкой от края. Якорь — полоса высоты кнопки;
                // дальше каждая кнопка: extrude ТОЛЬКО по горизонтали,
                // отдать rect, свернуть вправо, переместить на зазор.
                // Ряд — линейкой от края. Ширины и зазоры (колонки):
                // EXIT 4 · 3 · REW 8 · 1 · PLAY 8 · 1 · FF 8 · 2 · RPT 6 ·
                // 3 · REC 8 · 1 · RACE 8.
                let exit_label = "\u{e6c0}\u{f0206}";
                let btn_h = ruler::cell_y(2);
                let mut ruler = egui::pos2(0.0, y).into_ruler().extrude(0.0, -btn_h);
                let mut btn = |w: isize, gap: isize| -> egui::Rect {
                    let next = ruler.extrude(ruler::cell_x(w), 0.0);
                    ruler = next.collapse_right().translate(ruler::cell_x(gap), 0.0);
                    next.into()
                };
                let exit_rect = btn(6, 3);
                let rew_rect = btn(8, 1);
                let play_rect = btn(8, 1);
                let ff_rect = btn(8, 2);
                let rpt_rect = btn(6, 3);
                let rec_rect = btn(8, 1);
                let race_rect = btn(8, 0);

                // Кнопки: interact (если жива) → состояние (жива,
                // активна || клик в этом кадре — тап подсвечивается) →
                // paint всегда. Три стейта на обе пары цветов.
                let exit_response = view::TransportButton::new(
                    exit_label,
                    view::TransportState::Idle,
                    EXIT_IDLE,
                    EXIT_ACTIVE,
                )
                .interact(ui, exit_rect, ui.make_persistent_id("flight_exit"));
                view::TransportButton::new(
                    exit_label,
                    tstate(true, exit_response.clicked()),
                    EXIT_IDLE,
                    EXIT_ACTIVE,
                )
                .paint(ui, exit_rect);
                if exit_response.clicked() {
                    action = Action::Exit;
                }

                let idle_btn = |icon: &'static str| {
                    view::TransportButton::new(
                        icon,
                        view::TransportState::Idle,
                        TRANSPORT_IDLE,
                        TRANSPORT_ACTIVE,
                    )
                };

                // REW/PLAY/FF/RPT — живы только в DVR.
                let rew_response = if dvr {
                    Some(idle_btn("\u{f04a}").interact(
                        ui,
                        rew_rect,
                        ui.make_persistent_id("flight_rew"),
                    ))
                } else {
                    None
                };
                if let Some(r) = &rew_response {
                    self.seek_tick(ui, r, true, &clip);
                }
                let rew_lit = self.rew.lit() || rew_response.as_ref().is_some_and(|r| r.clicked());
                view::TransportButton::new(
                    "\u{f04a}",
                    tstate(dvr, rew_lit),
                    TRANSPORT_IDLE,
                    TRANSPORT_ACTIVE,
                )
                .paint(ui, rew_rect);

                let playing = self
                    .transport
                    .as_ref()
                    .is_some_and(|t| t.is_running() && !t.is_seeking());
                let play_response = if dvr {
                    Some(idle_btn("\u{f04b}").interact(
                        ui,
                        play_rect,
                        ui.make_persistent_id("flight_play"),
                    ))
                } else {
                    None
                };
                view::TransportButton::new(
                    "\u{f04b}",
                    tstate(
                        dvr,
                        playing || play_response.as_ref().is_some_and(|r| r.clicked()),
                    ),
                    TRANSPORT_IDLE,
                    TRANSPORT_ACTIVE,
                )
                .paint(ui, play_rect);

                let ff_response = if dvr {
                    Some(idle_btn("\u{f04e}").interact(
                        ui,
                        ff_rect,
                        ui.make_persistent_id("flight_ff"),
                    ))
                } else {
                    None
                };
                if let Some(r) = &ff_response {
                    self.seek_tick(ui, r, false, &clip);
                }
                let ff_lit = self.ff.lit() || ff_response.as_ref().is_some_and(|r| r.clicked());
                view::TransportButton::new(
                    "\u{f04e}",
                    tstate(dvr, ff_lit),
                    TRANSPORT_IDLE,
                    TRANSPORT_ACTIVE,
                )
                .paint(ui, ff_rect);

                let circled = self
                    .transport
                    .as_ref()
                    .is_some_and(|t| t.circle().is_some());
                let loop_response = if dvr {
                    Some(idle_btn("\u{F0456}").interact(
                        ui,
                        rpt_rect,
                        ui.make_persistent_id("flight_rpt"),
                    ))
                } else {
                    None
                };
                view::TransportButton::new(
                    "\u{F0456}",
                    tstate(
                        dvr,
                        circled || loop_response.as_ref().is_some_and(|r| r.clicked()),
                    ),
                    TRANSPORT_IDLE,
                    TRANSPORT_ACTIVE,
                )
                .paint(ui, rpt_rect);

                // REC: цвета записи; жива с эфира до конца хвоста. В
                // холодном DVR нет гоночного потока — мертва.
                let rec_ok = self.race.is_some() && self.air.rec_available();
                let rec_response = if rec_ok {
                    Some(idle_btn(rec_icon).interact(
                        ui,
                        rec_rect,
                        ui.make_persistent_id("flight_rec"),
                    ))
                } else {
                    None
                };
                view::TransportButton::new(
                    rec_icon,
                    tstate(
                        rec_ok,
                        rec_on || rec_response.as_ref().is_some_and(|r| r.clicked()),
                    ),
                    REC_IDLE,
                    REC_ACTIVE,
                )
                .paint(ui, rec_rect);
                if rec_response.as_ref().is_some_and(|r| r.clicked()) {
                    if let Some(race) = &self.race {
                        race.intent(Intent::RecPressed);
                    }
                }

                // RACE: кислотная когда отсчёт/гонка. В холодном DVR
                // нет гоночного потока — мертва.
                let race_ok = self.race.is_some() && self.air.race_available();
                let race_response = if race_ok {
                    Some(idle_btn(race_icon).interact(
                        ui,
                        race_rect,
                        ui.make_persistent_id("flight_race"),
                    ))
                } else {
                    None
                };
                view::TransportButton::new(
                    race_icon,
                    tstate(
                        race_ok,
                        race_on || race_response.as_ref().is_some_and(|r| r.clicked()),
                    ),
                    RACE_IDLE,
                    RACE_ACTIVE,
                )
                .paint(ui, race_rect);
                if race_response.as_ref().is_some_and(|r| r.clicked()) {
                    if let Some(race) = &self.race {
                        race.intent(Intent::RacePressed);
                    }
                }
                if play_response.as_ref().is_some_and(|r| r.clicked()) {
                    if let Some(t) = &mut self.transport {
                        if t.is_running() {
                            t.pause();
                        } else {
                            t.play();
                        }
                    }
                }
                if loop_response.as_ref().is_some_and(|r| r.clicked()) {
                    let radius = (scrub::RPT_SIZES[self.rpt_size] * 1000.0) as u64;
                    if let Some(t) = &mut self.transport {
                        t.set_circle(if circled {
                            None
                        } else {
                            Some(Circle { center: 0, radius })
                        });
                    }
                }
                // Alt (живая часть): над REC — fps записи по камерам
                // (целевой, пока не пишем; error при провале), над RACE —
                // длительность гонки Σ / остаток со знаком минус.
                if !dvr && alt {
                    let row = ruler::CELL_SIZE.y;
                    let mut bubble = |rect: egui::Rect, lines: &[String]| {
                        if lines.is_empty() {
                            return;
                        }
                        let w = lines
                            .iter()
                            .map(|l| super::measure_text(ui.ctx(), style::FONT_REGULAR, l).x)
                            .fold(0.0f32, f32::max);
                        let text = egui::Rect::from_min_max(
                            egui::pos2(rect.center().x - w / 2.0, rect.top() - row * 2.0),
                            egui::pos2(rect.center().x + w / 2.0, rect.top() - row),
                        )
                        .translate(egui::vec2(0.0, row * 0.5));
                        view::hint_badge(ui, text, egui::Align2::LEFT_BOTTOM, lines);
                    };
                    let rec_lines: Vec<String> = self
                        .active_cameras
                        .iter()
                        .map(|(id, cfg)| {
                            let target = cfg.dvr.frame_rate.unwrap_or(cfg.frame_rate);
                            match self.webcams.get(id) {
                                Some(w) if w.capture_state() == CaptureState::Dead => {
                                    "\u{f03d} error".to_owned()
                                }
                                Some(w)
                                    if rec_on
                                        && matches!(
                                            w.record_state(),
                                            crate::driver::dvr::RecordState::Failed
                                        ) =>
                                {
                                    "\u{f16df} error".to_owned()
                                }
                                Some(w) => match w.record_state() {
                                    crate::driver::dvr::RecordState::Writing { fps, .. }
                                        if fps > 0.0 =>
                                    {
                                        format!("\u{f16df} {:.0}fps", fps)
                                    }
                                    _ => format!("\u{f16df} {target}"),
                                },
                                None => format!("\u{f16df} {target}"),
                            }
                        })
                        .collect();
                    bubble(rec_rect, &rec_lines);
                    // Без лимита длительности баббл не нужен: нечего
                    // показывать ни до, ни после старта.
                    let race_hint = match (self.air.race, self.rules.duration) {
                        (Some(start), Some(duration)) => {
                            let left = duration.saturating_sub(start.elapsed());
                            Some(format!("-{}", mmss(left)))
                        }
                        (None, Some(duration)) => Some(format!("Σ{}", mmss(duration))),
                        _ => None,
                    };
                    if let Some(hint) = race_hint {
                        bubble(race_rect, &[hint]);
                    }
                }

                // Alt: значения шкал кнопок (вторые строки) — в бабблах
                // над кнопками.
                if dvr && ui.input(|i| i.modifiers.alt) {
                    let row = ruler::CELL_SIZE.y;
                    let labels = [
                        (
                            rew_rect,
                            format!("\u{00d7}{}", scrub::SEEK_SPEEDS[self.seek_speed]),
                        ),
                        (
                            play_rect,
                            format!("{}%", scrub::PLAY_SPEEDS[self.play_speed]),
                        ),
                        (
                            ff_rect,
                            format!("\u{00d7}{}", scrub::SEEK_SPEEDS[self.seek_speed]),
                        ),
                        (
                            rpt_rect,
                            format!("±{:.1}s", scrub::RPT_SIZES[self.rpt_size]),
                        ),
                    ];
                    for (rect, label) in labels {
                        let w = super::measure_text(ui.ctx(), style::FONT_REGULAR, &label).x;
                        let text = egui::Rect::from_min_max(
                            egui::pos2(rect.center().x - w / 2.0, rect.top() - row * 2.0),
                            egui::pos2(rect.center().x + w / 2.0, rect.top() - row),
                        )
                        .translate(egui::vec2(0.0, row * 0.5));
                        view::hint_badge(ui, text, egui::Align2::LEFT_BOTTOM, &[label]);
                    }
                }

                // Скролл над кнопками — шаг по их шкалам (DVR).
                let scroll_delta = ui.input(|i| i.smooth_scroll_delta.y);
                if dvr {
                    if let Some(up) =
                        self.vscroll
                            .step(scroll_delta, scrub::SCROLL_STEP, scrub::SCROLL_REARM)
                    {
                        let dir = if up { -1 } else { 1 };
                        let shift = |cur: &mut usize, len: usize, dir: i64| {
                            *cur = (*cur as i64 + dir).clamp(0, len as i64 - 1) as usize;
                        };
                        // Скролл вверх — к большей скорости (шкалы
                        // по возрастанию, сдвиг единый со всеми).
                        if let Some(r) = &rew_response {
                            if r.hovered() {
                                shift(&mut self.seek_speed, scrub::SEEK_SPEEDS.len(), dir);
                            }
                        }
                        if let Some(r) = &ff_response {
                            if r.hovered() {
                                shift(&mut self.seek_speed, scrub::SEEK_SPEEDS.len(), dir);
                            }
                        }
                        if let Some(r) = &loop_response {
                            if r.hovered() {
                                let before = self.rpt_size;
                                shift(&mut self.rpt_size, scrub::RPT_SIZES.len(), dir);
                                // Круг включён — переприменить радиус
                                // (транспорт центр переставит сам).
                                if self.rpt_size != before
                                    && self
                                        .transport
                                        .as_ref()
                                        .is_some_and(|t| t.circle().is_some())
                                {
                                    let radius = (scrub::RPT_SIZES[self.rpt_size] * 1000.0) as u64;
                                    if let Some(t) = &mut self.transport {
                                        t.set_circle(Some(Circle { center: 0, radius }));
                                    }
                                }
                            }
                        }
                        if let Some(r) = play_response {
                            if r.hovered() {
                                let before = self.play_speed;
                                shift(&mut self.play_speed, scrub::PLAY_SPEEDS.len(), dir);
                                if self.play_speed != before {
                                    let speed = scrub::PLAY_SPEEDS[self.play_speed] as f64 / 100.0;
                                    if let Some(t) = &mut self.transport {
                                        t.set_speed(speed);
                                    }
                                }
                            }
                        }
                    }
                }

                // TIMER: в live-фазах — счётчик гонки, в DVR — позиция.
                let timer_pos = egui::pos2(egui::Rect::from(ruler).right() + ruler::cell_x(2), y);
                if dvr {
                    // Знаковое отступление от границ гонки (журнал):
                    // минус — голова до старта, плюс — хвост после финиша.
                    // Рамки нет — чистая позиция без знака. Знак занимает
                    // позицию десятков минут — все варианты одной ширины:
                    // -0:05.234 / 00:00.000 / +0:03.100.
                    let (sign, shown) = match Self::race_bounds(&clip) {
                        Some((lo, _)) if dvr_pos < lo => (Some('-'), lo - dvr_pos),
                        Some((_, hi)) if dvr_pos > hi => (Some('+'), dvr_pos - hi),
                        Some((lo, _)) => (None, dvr_pos.saturating_sub(lo)),
                        None => (None, dvr_pos.saturating_sub(dvr_start)),
                    };
                    let pos_text = match sign {
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
                    let timer_icon = "\u{f43a}";
                    let icon_w =
                        super::measure_text(ui.ctx(), style::FONT_REGULAR_X2, timer_icon).x;
                    ui.painter().text(
                        timer_pos,
                        egui::Align2::LEFT_BOTTOM,
                        timer_icon,
                        style::FONT_REGULAR_X2,
                        TRANSPORT_ACTIVE,
                    );
                    ui.painter().text(
                        egui::pos2(timer_pos.x + icon_w + ruler::cell_x(1), timer_pos.y),
                        egui::Align2::LEFT_BOTTOM,
                        &pos_text,
                        style::FONT_REGULAR_X2,
                        TRANSPORT_ACTIVE,
                    );
                } else {
                    let now = Instant::now();
                    let blink_off = self.air.countdown_end.is_some_and(|end| {
                        end.saturating_duration_since(now).as_millis() as u64 % 1000 >= 500
                    });
                    let race_timer = {
                        let total = self.air.race_elapsed(now).as_millis() as u64;
                        format!(
                            "{:02}:{:02}.{:03}",
                            total / 60_000,
                            (total / 1_000) % 60,
                            total % 1_000
                        )
                    };
                    let zeros_dim =
                        self.air.race_elapsed(now).is_zero() && !self.air.countdown_active();
                    // Бегущий счётчик — слоновой костью (цвет активного
                    // транспорта); мигание и нули — 20% её.
                    let timer_color = if blink_off || zeros_dim {
                        TRANSPORT_ACTIVE.gamma_multiply(0.2)
                    } else {
                        TRANSPORT_ACTIVE
                    };
                    let timer_icon = "\u{f43a}";
                    let icon_w =
                        super::measure_text(ui.ctx(), style::FONT_REGULAR_X2, timer_icon).x;
                    ui.painter().text(
                        timer_pos,
                        egui::Align2::LEFT_BOTTOM,
                        timer_icon,
                        style::FONT_REGULAR_X2,
                        timer_color,
                    );
                    ui.painter().text(
                        egui::pos2(timer_pos.x + icon_w + ruler::cell_x(1), timer_pos.y),
                        egui::Align2::LEFT_BOTTOM,
                        race_timer,
                        style::FONT_REGULAR_X2,
                        timer_color,
                    );
                }
            });

        if self.exit_requested {
            self.exit_requested = false;
            action = Action::Exit;
        }
        match action {
            Action::Exit => {
                // Ядро доводит гонку до целостности (финиш/abort),
                // экран закрываем не дожидаясь.
                if let Some(race) = &self.race {
                    race.intent(Intent::Exit);
                }
                navigator.goto(menu::Menu::new())
            }
            Action::None => {}
        }
    }

    /// Часы DVR в журнальном домене: (старт, позиция, конец). Полоска
    /// — окно записи (REC on → REC off); файлы лежат на ней по первым
    /// кадрам. Без журнала — файловый домен (0, позиция, длительность);
    /// журнал без окна записи (старые форматы) — экстент (0, 0, end).
    fn dvr_clock(&self, clip: &Option<RaceClip>) -> (u64, u64, u64) {
        match clip {
            Some(clip) => {
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
                let tp = self.transport.as_ref().map(|t| t.position());
                let pos = match (self.players.get(&self.master), clip.files.get(&self.master)) {
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
            None => {
                let pos = self.players.values().map(|p| p.shown()).max().unwrap_or(0);
                let dur = self
                    .players
                    .values()
                    .map(|p| p.duration())
                    .max()
                    .unwrap_or(0);
                (0, pos.min(dur), dur)
            }
        }
    }

    /// Таймлайн интерактивен, когда все плееры открылись (Ready/Failed) —
    /// та же проверка, что гейтит DvrOpening→Dvr в phase_tick.
    fn dvr_ready(&self) -> bool {
        self.players
            .values()
            .all(|p| matches!(p.open_state(), PlayerState::Ready | PlayerState::Failed(_)))
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
                    // Покрытие — до КОНЦА последнего кадра (duration +
                    // last_step): кадр демонстрируется интервал, а не
                    // мгновение, иначе хвост до record-finished (~период
                    // кадра) читался бы как гэп с плейсхолдером.
                    journal_pos < *file_ms || journal_pos >= *file_ms + p.duration() + p.last_step()
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
    fn seek_by(&mut self, clip: &Option<RaceClip>, delta: i64) {
        let (start, pos, end) = self.dvr_clock(clip);
        let target = (pos as i64 + delta).clamp(start as i64, end as i64) as u64;
        if let Some(t) = &mut self.transport {
            t.seek(target);
        }
    }

    /// Один кадр кнопки перемотки: press ждёт SEEK_HOLD_DELAY, отпускание
    /// до SEEK_CLICK_MAX — фиксированный шаг с вспышкой, иначе release.
    fn seek_tick(
        &mut self,
        ui: &egui::Ui,
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
            let state = if back { &mut self.rew } else { &mut self.ff };
            if state.down && !pointer_down {
                state.down = false;
                Event::Release
            } else if !state.down && response.is_pointer_button_down_on() {
                state.down = true;
                state.down_since = std::time::Instant::now();
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
                    self.rew.down_since
                } else {
                    self.ff.down_since
                }
                .elapsed()
                    < scrub::SEEK_CLICK_MAX;
                if quick {
                    if if back {
                        self.rew.hold_sent
                    } else {
                        self.ff.hold_sent
                    } {
                        if let Some(t) = &mut self.transport {
                            t.shuttle_release(scrub::SEEK_RELEASE_RAMP);
                        }
                    }
                    let state = if back { &mut self.rew } else { &mut self.ff };
                    state.flash = true;
                    let step = scrub::SEEK_SPEEDS[self.seek_speed] as i64 * scrub::SEEK_TAP as i64;
                    self.seek_by(clip, if back { -step } else { step });
                } else {
                    if let Some(t) = &mut self.transport {
                        t.shuttle_release(scrub::SEEK_RELEASE_RAMP);
                    }
                }
            }
            Event::Tap => {
                let state = if back { &mut self.rew } else { &mut self.ff };
                state.flash = true;
                let step = scrub::SEEK_SPEEDS[self.seek_speed] as i64 * scrub::SEEK_TAP as i64;
                self.seek_by(clip, if back { -step } else { step });
            }
            Event::None => {}
        }
        let arm = {
            let state = if back { &mut self.rew } else { &mut self.ff };
            if state.down && !state.hold_sent {
                ui.ctx().request_repaint();
            }
            state.down && !state.hold_sent && state.down_since.elapsed() >= scrub::SEEK_HOLD_DELAY
        };
        if arm {
            let state = if back { &mut self.rew } else { &mut self.ff };
            state.hold_sent = true;
            let speed = scrub::SEEK_SPEEDS[self.seek_speed] as f64 * if back { -1.0 } else { 1.0 };
            if let Some(t) = &mut self.transport {
                t.shuttle(speed);
            }
        }
    }

    /// Самовзвод перерисовок счётчика гонки — только в живых фазах без
    /// кадров (в DVR кадры гонят перерисовки сами).
    fn arm_race_repaint(&self, ctx: &egui::Context) {
        if !self.air.countdown_active() && self.air.race.is_none() {
            return;
        }
        let streaming = self
            .webcams
            .values()
            .any(|w| w.capture_state() == CaptureState::Live);
        if !streaming {
            ctx.request_repaint_after(Duration::from_millis(43));
        }
    }

    /// Будильник автономного транспорта: без плееров некому будить UI
    /// на каждый «кадр» — пока время бежит, будим сами (~30 Гц,
    /// эквивалент потока кадров); стоит — тишина, ни одного лишнего
    /// репейнта. С плеерами их публикации кадров будят сами, этот
    /// будильник — дубль, egui склеивает.
    fn arm_transport_repaint(&self, ctx: &egui::Context) {
        let Some(t) = &self.transport else {
            return;
        };
        if t.is_running() {
            ctx.request_repaint_after(Duration::from_millis(33));
        }
    }

    /// Транзит DvrOpening→Dvr: готовность плееров (все Ready/Failed;
    /// пустой набор — готовым: иначе транзит вечный). Гоночные тики
    /// живут в ядре, здесь — только экранный транзит.
    fn dvr_tick(&mut self) {
        if !matches!(self.screen, Screen::DvrOpening) {
            return;
        }
        let ready = self
            .players
            .values()
            .all(|p| matches!(p.open_state(), PlayerState::Ready | PlayerState::Failed(_)));
        if ready {
            self.screen = Screen::Dvr;
        }
    }

    /// Режимы входа в DVR:
    /// - cold: журнал парсится из файла (`open_dvr_cold`) → warm;
    /// - warm: готовый журнал в памяти, видеоиндексы плееры строят
    ///   при открытии (этот путь — воронка для обоих);
    /// - hot (будущее): журнал и индексы уже в памяти, открытие
    ///   файлов не нужно.
    ///
    /// Warm: журнал от ядра (гонка закрыта) или реконструированный
    /// cold-входом. Рамка — из журнала, эфир гасим, плееры открываем.
    /// Журнал остаётся в GUI: правки DVR (добавление/удаление событий)
    /// — здесь, ядро к этому моменту отдало владение и завершилось.
    fn open_dvr_warm(&mut self, ctx: &egui::Context, journal: Journal) {
        let clip = race_clip(journal.iter_ms());
        self.stop_all();
        self.open_dvr_players(ctx, &clip);
        // Мастер — первая по порядку сетапа камера с файлом.
        self.master = self
            .setup
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
        self.transport = Some(transport);
        self.journal = Some(journal);
        self.screen = Screen::DvrOpening;
    }

    /// Cold-вход: последний журнал из `captures/` парсится и
    /// реконструируется в память → warm. false — журнала нет.
    fn open_dvr_cold(&mut self, ctx: &egui::Context) -> bool {
        let Some((header, events)) =
            scrub::latest_journal(std::path::Path::new(crate::driver::dvr::CAPTURES_DIR))
                .and_then(|path| std::fs::read_to_string(&path).ok())
                .map(|yaml| Journal::parse_stream(&yaml))
        else {
            return false;
        };
        let Some(header) = header else {
            return false;
        };
        let mut journal = Journal::new(header.date_time, header.pads, header.assignments);
        for (ms, event) in events {
            journal.push(ms, event);
        }
        self.open_dvr_warm(ctx, journal);
        true
    }

    /// Плееры записей гонки из рамки; пустая рамка — деградация на
    /// последние файлы камер (fallback без журнала).
    fn open_dvr_players(&mut self, ctx: &egui::Context, clip: &crate::model::flight::RaceClip) {
        let dir = std::path::Path::new(crate::driver::dvr::CAPTURES_DIR);
        let mut opened = 0;
        for (camera_id, (filename, _)) in &clip.files {
            let path = dir.join(filename);
            if path.exists() {
                self.players
                    .insert(camera_id.clone(), Player::open(&path, ctx.clone()));
                opened += 1;
            } else {
                log::warn!("flight: recording of {camera_id} missing: {path:?}");
            }
        }
        if opened > 0 || !clip.files.is_empty() {
            // Рамка есть: открываем ровно её файлы. Камеры без файла —
            // без плеера (GUI рисует Error).
            return;
        }
        if let Some(captures) = &self.captures {
            for camera_id in self.active_cameras.keys() {
                if let Some(path) = scrub::latest_capture(captures, camera_id) {
                    self.players
                        .insert(camera_id.clone(), Player::open(&path, ctx.clone()));
                }
            }
        }
    }

    // --- Эфир -------------------------------------------------------------

    fn start_captures(&mut self, ctx: &egui::Context) {
        for (id, camera) in &self.active_cameras {
            let webcam = crate::driver::camera::Camera::open(camera.clone(), ctx.clone());
            self.webcams.insert(id.clone(), webcam);
        }
        log::info!("flight: webcams starting: {}", self.webcams.len());
    }

    /// Выключить эфир целиком: захват, подавление сна.
    fn stop_all(&mut self) {
        self.webcams.clear();
        self.awake = None;
    }

    fn screen_awake(&mut self) {
        if self.awake.is_some() {
            return;
        }
        match keepawake::Builder::default()
            .display(true)
            .reason("flight")
            .app_name("Laps")
            .create()
        {
            Ok(awake) => self.awake = Some(awake),
            Err(e) => log::warn!("screen inhibit failed: {e}"),
        }
    }

    /// Команда удалённого управления — нажатия кнопок экрана: rec, race,
    /// play, ff, rew, exit повторяют клики по кнопкам, seek <ms> —
    /// абсолютную перемотку всех плееров. Some — обработана.
    pub fn handle_remote(
        &mut self,
        ctx: &egui::Context,
        command: &[String],
    ) -> Option<crate::remote::Response> {
        let head = command.first()?.as_str();
        let clip = self
            .journal
            .as_ref()
            .map(|journal| race_clip(journal.iter_ms()));
        let ok = |message: String| Some(crate::remote::Response { message, code: 0 });
        match head {
            "rec" => {
                if self.race.is_some() && self.air.rec_available() {
                    if let Some(race) = &self.race {
                        race.intent(Intent::RecPressed);
                    }
                }
                ok(String::new())
            }
            "race" => {
                if self.race.is_some() && self.air.race_available() {
                    if let Some(race) = &self.race {
                        race.intent(Intent::RacePressed);
                    }
                }
                ok(String::new())
            }
            "play" => {
                if let Some(t) = &mut self.transport {
                    if t.is_running() {
                        t.pause();
                    } else {
                        t.play();
                    }
                }
                ok(String::new())
            }
            "ff" => {
                let step = scrub::SEEK_SPEEDS[self.seek_speed] as i64 * scrub::SEEK_TAP as i64;
                self.seek_by(&clip, step);
                ok(String::new())
            }
            "rew" => {
                let step = scrub::SEEK_SPEEDS[self.seek_speed] as i64 * scrub::SEEK_TAP as i64;
                self.seek_by(&clip, -step);
                ok(String::new())
            }
            "seek" => {
                // Абсолютная позиция в домене сессии (мс от нуля
                // журнала); транспорт клампит к шкале.
                let ms: u64 = command.get(1).and_then(|a| a.parse().ok())?;
                if let Some(t) = &mut self.transport {
                    t.seek(ms);
                }
                ok(String::new())
            }
            "exit" => {
                // Ядру — немедленно (инварианты целостности), флаг —
                // только для навигации (нужен Navigator из update).
                if let Some(race) = &self.race {
                    race.intent(Intent::Exit);
                }
                self.exit_requested = true;
                ctx.request_repaint();
                ok(String::new())
            }
            _ => None,
        }
    }
}

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

/// мм:ss от длительности.
fn mmss(dur: Duration) -> String {
    let secs = dur.as_secs();
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

/// TransportState по паре (жива, активна): живая — Idle/Active,
/// мёртвая — Disabled.
fn tstate(enabled: bool, active: bool) -> view::TransportState {
    match (enabled, active) {
        (true, true) => view::TransportState::Active,
        (true, false) => view::TransportState::Idle,
        (false, _) => view::TransportState::Disabled,
    }
}

struct Res;

impl Res {
    fn new(ctx: &egui::Context) -> Self {
        let symbol_size = measure_text(ctx, style::FONT_REGULAR, "M");
        assert!(
            symbol_size == ruler::CELL_SIZE,
            "Font size mismatch: expected {:?}, got {:?}. Update ruler::CELL_SIZE or check font assets.",
            ruler::CELL_SIZE,
            symbol_size,
        );
        Self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::keyboard::KeyBind;
    use crate::config::setup::{ColorConfig, FpvConfig};
    use std::collections::BTreeMap;

    fn bind(keyboard_id: &str, key: u32) -> KeyBind {
        KeyBind {
            keyboard_id: keyboard_id.to_owned(),
            key,
            action: "lap".to_owned(),
        }
    }

    fn pad(keybinds: Vec<KeyBind>) -> PadConfig {
        PadConfig {
            label: "CH".to_owned(),
            color: ColorConfig::Red,
            fpv: FpvConfig {
                camera_id: "camera-1".to_owned(),
                viewport: Default::default(),
            },
            keybinds,
        }
    }

    fn event(key: u32, action: KeyAction) -> KeyEvent {
        KeyEvent {
            keyboard_id: "key-1".to_owned(),
            modifiers: 0,
            keycode: key,
            action,
            at: Instant::now(),
        }
    }

    #[test]
    fn held_keys_follow_pad_binds() {
        let mut pads = BTreeMap::new();
        pads.insert(
            "pad-1".to_owned(),
            pad(vec![bind("key-1", 0x1E), bind("key-1", 0x1F)]),
        );
        pads.insert("pad-4".to_owned(), pad(vec![bind("key-1", 0x21)]));
        let mut held = HashMap::new();
        // Press добавляет символ во все пады с таким биндом (0x1E — "1").
        Flight::apply_key_event(&pads, &mut held, &event(0x1E, KeyAction::Press));
        assert_eq!(held["pad-1"], vec!["1".to_owned()]);
        // Второй бинд того же пада копится (0x1F — "2").
        Flight::apply_key_event(&pads, &mut held, &event(0x1F, KeyAction::Press));
        assert_eq!(held["pad-1"], vec!["1".to_owned(), "2".to_owned()]);
        // Клавиша другого пада (0x21 — "4") туда не попадает.
        Flight::apply_key_event(&pads, &mut held, &event(0x21, KeyAction::Press));
        assert_eq!(held["pad-1"], vec!["1".to_owned(), "2".to_owned()]);
        assert_eq!(held["pad-4"], vec!["4".to_owned()]);
        // Release убирает только свой символ.
        Flight::apply_key_event(&pads, &mut held, &event(0x1E, KeyAction::Release));
        assert_eq!(held["pad-1"], vec!["2".to_owned()]);
        // Незабинженная клавиша ни на что не влияет.
        Flight::apply_key_event(&pads, &mut held, &event(0x70, KeyAction::Press));
        assert_eq!(held["pad-1"], vec!["2".to_owned()]);
        assert_eq!(held["pad-4"], vec!["4".to_owned()]);
        // Обрыв канала клавиатуры снимает все её символы.
        Flight::drop_keyboard_keys(&pads, &mut held, "key-1");
        assert!(held["pad-1"].is_empty());
        assert!(held["pad-4"].is_empty());
    }
}
