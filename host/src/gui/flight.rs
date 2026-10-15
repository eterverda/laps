//! Экран Flight: совмещает live (трансляция/запись/гонка) и scrub
//! (просмотр записи). Финал — режим DVR: запись остановлена, захват
//! погашен, вьюфайндеры показывают только что записанное, работают
//! транспорт и таймлайн. Вход — сразу в эфире (Idle нет), выход —
//! только EXIT (в меню).

use super::ruler::{CellsTall, IntoRuler};
use super::*;
use crate::config::camera::CameraConfig;
use crate::config::setup::Setup;
use crate::driver::audio;
use crate::driver::camera::CaptureState;
use crate::driver::player::{Player, PlayerState};
use crate::model::pilot::Pilot;
use crate::model::race::{RaceRules, StartSignalDelay};
use std::collections::HashMap;
use std::path::PathBuf;
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

/// Фазы экрана. rec_since RecordTail и live_since Dvr — задел под
/// таймеры DVR-режима, пока не читаются.
#[allow(dead_code)]
/// Живая часть — как Session у live (без Idle и LiveAfter):
/// отжатие записи ведёт в DVR, отмены до старта гонки шагают назад.
/// DVR — транзит загрузки плееров и режим просмотра (стейты плеера
/// внутри worker'ов, тут только факт открытия).
#[derive(Clone, Copy, Debug)]
enum Phase {
    /// Эфир без записи (стартовая фаза — экран входит сразу сюда).
    LiveBefore { live_since: Instant },
    RecordHead {
        live_since: Instant,
        rec_since: Instant,
    },
    RaceCountdown {
        live_since: Instant,
        rec_since: Instant,
        start: Instant,
        delay: Duration,
    },
    Race {
        live_since: Instant,
        rec_since: Instant,
        start: Instant,
    },
    /// Гонка кончилась, запись хвоста продолжается; race — замороженное
    /// время гонки для счётчика.
    RecordTail {
        live_since: Instant,
        rec_since: Instant,
        race: Duration,
    },
    /// Транзит: захват погашен, плееры открывают запись.
    DvrOpening { live_since: Instant },
    /// Просмотр записи: транспорт, таймлайн, счётчик позиции.
    Dvr { live_since: Instant },
}

impl Phase {
    fn rec_on(&self) -> bool {
        matches!(
            self,
            Phase::RecordHead { .. }
                | Phase::RaceCountdown { .. }
                | Phase::Race { .. }
                | Phase::RecordTail { .. }
        )
    }

    /// Отсчёт идёт (мигающий счётчик).
    fn countdown(&self) -> bool {
        matches!(self, Phase::RaceCountdown { .. })
    }

    /// REC кликабельна: с эфира до конца хвоста.
    fn rec_available(&self) -> bool {
        matches!(
            self,
            Phase::LiveBefore { .. }
                | Phase::RecordHead { .. }
                | Phase::RaceCountdown { .. }
                | Phase::Race { .. }
                | Phase::RecordTail { .. }
        )
    }

    /// RACE кликабельна: нажатие до старта, отжатие в Countdown/Race.
    fn race_available(&self) -> bool {
        matches!(
            self,
            Phase::LiveBefore { .. }
                | Phase::RecordHead { .. }
                | Phase::RaceCountdown { .. }
                | Phase::Race { .. }
        )
    }

    /// Время гонки для счётчика: бежит в Race, заморожено после.
    fn race_elapsed(&self, now: Instant) -> Duration {
        match *self {
            Phase::Race { start, .. } => now.saturating_duration_since(start),
            Phase::RecordTail { race, .. } => race,
            Phase::DvrOpening { .. } | Phase::Dvr { .. } => {
                // В Dvr счётчик — позиция плеера; гоночное время не нужно.
                Duration::ZERO
            }
            _ => Duration::ZERO,
        }
    }
}

/// Звуки гонки: бинари — в `assets`.
const STAGE: audio::Sound = audio::Sound::from_bytes(crate::assets::STAGE_WAV);
const BUZZER: audio::Sound = audio::Sound::from_bytes(crate::assets::BUZZER_WAV);

/// Фиксированная фаза бипов отсчёта: PRE_BEEP_SECS бипов с периодом
/// 1 с, затем случайный интервал из StartSignalDelay, затем гудок.
const PRE_BEEP_SECS: u64 = 3;

/// Момент начала времени гонки: конец отсчёта + длина гудка. До него
/// счётчик мигает, на нём — гаснет последняя фаза ПОГАС и дальше ГОРИТ
/// И ТИКАЕТ.
fn race_time_start(start: Instant, delay: Duration) -> Instant {
    start + delay + BUZZER.duration()
}

/// Режим входа: Live — сразу в эфире (главный экран), Dvr — сразу
/// просмотр записи (CLI `laps scrub`).
#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Live,
    Dvr,
}

pub struct Flight {
    phase: Phase,
    webcams: HashMap<String, crate::driver::camera::Camera>,
    awake: Option<keepawake::KeepAwake>,
    rules: RaceRules,
    audio: crate::driver::audio::Audio,
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
    /// Удалённая команда exit — нажатие EXIT между кадрами: update
    /// переводит её в Action::Exit.
    exit_requested: bool,
    header: header::Header,
    setup: Setup,
    assignments: HashMap<String, Pilot>,
    active_cameras: HashMap<String, CameraConfig>,
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
            phase: if mode == Mode::Dvr {
                Phase::DvrOpening {
                    live_since: Instant::now(),
                }
            } else {
                Phase::LiveBefore {
                    live_since: Instant::now(),
                }
            },
            webcams: HashMap::new(),
            awake: None,
            rules,
            audio: crate::driver::audio::Audio::spawn(),
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
        };
        if mode == Mode::Live {
            this.start_captures(ctx);
            this.screen_awake();
        } else {
            this.enter_dvr(ctx, Instant::now());
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
        self.phase_tick();
        self.arm_race_repaint(ui.ctx());
        self.rew.flash = false;
        self.ff.flash = false;

        let ctx = ui.ctx();
        self.res.get_or_insert_with(|| Res::new(ctx));

        // Время первичного плеера (DVR) — до отрисовки: шкала под
        // кнопками.
        let (dvr_pos, dvr_dur, dvr_state) =
            self.primary().map_or((0, 0, PlayerState::Opening), |p| {
                (p.position(), p.duration(), p.open_state())
            });

        view::Letterbox::new(grid::cell(GRID_WIDTH, 45))
            .max_virtual_height(grid::cell_y(50))
            .outline(true)
            .show(ui, |ui| {
                let bottom_right = grid::cell_at(ui.max_rect().max);
                let header_rect = grid::cell(0, 0).extrude(bottom_right.col, TOP_BAR_ROWS);
                self.header.show(ui, header_rect);

                // Таймлайн (только DVR): рисуется раньше кнопок — под
                // ними. Две клетки высотой, вся ширина, поля по 4
                // колонки. Головка ходит центрами от края до края.
                let dvr = matches!(self.phase, Phase::Dvr { .. });
                let timeline_rect = egui::Rect::from_min_max(
                    egui::pos2(
                        grid::cell_x(LEFT_MARGIN),
                        grid::cell_y(bottom_right.row - 4),
                    ),
                    egui::pos2(
                        grid::cell_x(GRID_WIDTH - RIGHT_MARGIN),
                        grid::cell_y(bottom_right.row - 2),
                    ),
                );
                if dvr && dvr_state == PlayerState::Ready {
                    let frac = if dvr_dur > 0 {
                        dvr_pos as f32 / dvr_dur as f32
                    } else {
                        0.0
                    };
                    // Круг зацикливания: доли (начало, конец) — для
                    // индикатора-капсулы на головке.
                    let circle = self.primary().and_then(|p| p.circle()).map(|c| {
                        let lo = c.center.saturating_sub(c.radius) as f32 / dvr_dur.max(1) as f32;
                        let hi = (c.center + c.radius).min(dvr_dur) as f32 / dvr_dur.max(1) as f32;
                        (lo, hi)
                    });
                    // Активный фрагмент (вылет) — всё, кроме отрезков
                    // по RACE_LEAD_MS по краям: белым, края трека 40%.
                    let window = (dvr_dur > 2 * scrub::RACE_LEAD_MS).then(|| {
                        (
                            scrub::RACE_LEAD_MS as f32 / dvr_dur as f32,
                            (dvr_dur - scrub::RACE_LEAD_MS) as f32 / dvr_dur as f32,
                        )
                    });
                    // Каретка — цветом активного транспортного контролла.
                    let (response, events) = self.timeline.show(
                        ui,
                        timeline_rect,
                        frac,
                        dvr_dur,
                        window,
                        circle,
                        TRANSPORT_ACTIVE,
                    );
                    for event in events {
                        match event {
                            scrub::TimelineEvent::SetDragHeld(held) => {
                                for player in self.players.values() {
                                    player.set_drag_held(held);
                                }
                            }
                            scrub::TimelineEvent::Seek(t) => {
                                for player in self.players.values() {
                                    player.seek(t);
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
                        self.seek_by(ms);
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
                    let state = if capture_state == CaptureState::Live {
                        viewfinder::ViewfinderContents::Texture(webcam.texture())
                    } else {
                        viewfinder::ViewfinderContents::Pending
                    };
                    contents.insert(id.clone(), state);
                }
                for (id, player) in self.players.iter_mut() {
                    player.update_frame();
                    let kind = match player.open_state() {
                        PlayerState::Ready => {
                            viewfinder::ViewfinderContents::Texture(player.texture())
                        }
                        _ => viewfinder::ViewfinderContents::Pending,
                    };
                    contents.insert(id.clone(), kind);
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

                if columns > 0 {
                    for (i, (pad_id, pad)) in pads.into_iter().enumerate() {
                        let (zone_start, zone_width) = column_geometry[i];
                        let col = zone_start + (zone_width - viewfinder_cols) / 2;
                        let ruler = egui::pos2(grid::cell_x(col), grid::cell_y(TOP_BAR_ROWS + 1))
                            .into_ruler();
                        let viewfinder = ruler.extrude(
                            grid::cell_x(viewfinder_cols),
                            (viewfinder_rows as i32 + 2).cells_tall(),
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
                let y = grid::cell_y(bottom_right.row);

                let rec_on = self.phase.rec_on();
                let race_on = self.phase.countdown() || matches!(self.phase, Phase::Race { .. });
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
                let btn_h = 2.cells_tall();
                let mut ruler = egui::pos2(grid::cell_x(0), y)
                    .into_ruler()
                    .extrude(0.0, -btn_h);
                let mut btn = |w: isize, gap: isize| -> egui::Rect {
                    let next = ruler.extrude(grid::cell_x(w), 0.0);
                    ruler = next.collapse_right().translate(grid::cell_x(gap), 0.0);
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
                    self.seek_tick(ui, r, true);
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
                    .primary()
                    .is_some_and(|p| p.is_playing() && !p.is_seeking());
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
                    self.seek_tick(ui, r, false);
                }
                let ff_lit = self.ff.lit() || ff_response.as_ref().is_some_and(|r| r.clicked());
                view::TransportButton::new(
                    "\u{f04e}",
                    tstate(dvr, ff_lit),
                    TRANSPORT_IDLE,
                    TRANSPORT_ACTIVE,
                )
                .paint(ui, ff_rect);

                let circled = self.primary().is_some_and(|p| p.is_circled());
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

                // REC: цвета записи; жива с эфира до конца хвоста.
                let rec_ok = self.phase.rec_available();
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
                    self.toggle_rec(ui.ctx());
                }

                // RACE: кислотная когда отсчёт/гонка.
                let race_ok = self.phase.race_available();
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
                    self.toggle_race(ui.ctx());
                }
                if play_response.as_ref().is_some_and(|r| r.clicked()) {
                    for player in self.players.values() {
                        if player.is_playing() {
                            player.pause();
                        } else {
                            player.play();
                        }
                    }
                }
                if loop_response.as_ref().is_some_and(|r| r.clicked()) {
                    let radius = (scrub::RPT_SIZES[self.rpt_size] * 1000.0) as u64;
                    for player in self.players.values() {
                        player.set_circle(if circled {
                            None
                        } else {
                            Some(crate::driver::player::Circle { center: 0, radius })
                        });
                    }
                }
                // Alt (живая часть): над REC — fps записи по камерам
                // (целевой, пока не пишем; error при провале), над RACE —
                // длительность гонки Σ / остаток со знаком минус.
                if !dvr && alt {
                    let row = grid::CELL_SIZE.y;
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
                                Some(w) if rec_on && !w.record_state().ok => {
                                    "\u{f16df} error".to_owned()
                                }
                                Some(w) if w.record_state().ok && w.record_state().fps > 0.0 => {
                                    format!("\u{f16df} {:.0}fps", w.record_state().fps)
                                }
                                _ => format!("\u{f16df} {target}"),
                            }
                        })
                        .collect();
                    bubble(rec_rect, &rec_lines);
                    let race_hint = match self.phase {
                        Phase::Race { start, .. } => {
                            let left = self.rules.duration.saturating_sub(start.elapsed());
                            format!("-{}", mmss(left))
                        }
                        _ => format!("Σ{}", mmss(self.rules.duration)),
                    };
                    bubble(race_rect, &[race_hint]);
                }

                // Alt: значения шкал кнопок (вторые строки) — в бабблах
                // над кнопками.
                if dvr && ui.input(|i| i.modifiers.alt) {
                    let row = grid::CELL_SIZE.y;
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
                        let shift = |cur: &mut usize, len: usize, dir: i64| {
                            *cur = (*cur as i64 + dir).clamp(0, len as i64 - 1) as usize;
                        };
                        if let Some(r) = play_response {
                            if r.hovered() {
                                let before = self.play_speed;
                                shift(
                                    &mut self.play_speed,
                                    scrub::PLAY_SPEEDS.len(),
                                    if up { -1 } else { 1 },
                                );
                                if self.play_speed != before {
                                    let speed = scrub::PLAY_SPEEDS[self.play_speed] as f32 / 100.0;
                                    for player in self.players.values() {
                                        player.set_speed(speed);
                                    }
                                }
                            }
                        }
                    }
                }

                // TIMER: в live-фазах — счётчик гонки, в DVR — позиция.
                let timer_pos = egui::pos2(egui::Rect::from(ruler).right() + grid::cell_x(2), y);
                if dvr {
                    // Знаковое отступление от границ вылета. Знак
                    // занимает позицию десятков минут — все варианты
                    // одной ширины: -0:05.234 / 00:00.000 / +0:03.100.
                    let (sign, shown) = if dvr_pos < scrub::RACE_LEAD_MS {
                        (Some('-'), scrub::RACE_LEAD_MS - dvr_pos)
                    } else if dvr_dur >= 2 * scrub::RACE_LEAD_MS
                        && dvr_pos > dvr_dur - scrub::RACE_LEAD_MS
                    {
                        (Some('+'), dvr_pos - (dvr_dur - scrub::RACE_LEAD_MS))
                    } else {
                        (
                            None,
                            dvr_pos
                                .saturating_sub(scrub::RACE_LEAD_MS)
                                .min(dvr_dur.saturating_sub(2 * scrub::RACE_LEAD_MS)),
                        )
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
                        egui::pos2(timer_pos.x + icon_w + grid::cell_x(1), timer_pos.y),
                        egui::Align2::LEFT_BOTTOM,
                        &pos_text,
                        style::FONT_REGULAR_X2,
                        TRANSPORT_ACTIVE,
                    );
                } else {
                    let now = Instant::now();
                    let blink_off = matches!(
                        self.phase,
                        Phase::RaceCountdown { start, delay, .. }
                            if race_time_start(start, delay)
                                .saturating_duration_since(now)
                                .as_millis() as u64
                                % 1000
                                >= 500
                    );
                    let race_timer = {
                        let total = self.phase.race_elapsed(now).as_millis() as u64;
                        format!(
                            "{:02}:{:02}.{:03}",
                            total / 60_000,
                            (total / 1_000) % 60,
                            total % 1_000
                        )
                    };
                    let zeros_dim =
                        self.phase.race_elapsed(now).is_zero() && !self.phase.countdown();
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
                        egui::pos2(timer_pos.x + icon_w + grid::cell_x(1), timer_pos.y),
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
            Action::Exit => navigator.goto(menu::Menu),
            Action::None => {}
        }
    }

    /// Первый по порядку сетапа плеер — «первичный»: его позиция — на
    /// счётчике, его кадры считают shown.
    fn primary(&self) -> Option<&Player> {
        self.setup
            .cameras
            .keys()
            .find_map(|id| self.players.get(id))
    }

    /// Перемотка всех плееров на delta мс от текущей позиции.
    fn seek_by(&self, delta: i64) {
        if let Some(p) = self.primary() {
            let t = (p.position() as i64 + delta).clamp(0, p.duration() as i64);
            for player in self.players.values() {
                player.seek(t as u64);
            }
        }
    }

    /// Один кадр кнопки перемотки: press ждёт SEEK_HOLD_DELAY, отпускание
    /// до SEEK_CLICK_MAX — фиксированный шаг с вспышкой, иначе release.
    fn seek_tick(&mut self, ui: &egui::Ui, response: &egui::Response, back: bool) {
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
                        for player in self.players.values() {
                            player.shuttle_release(scrub::SEEK_RELEASE_RAMP);
                        }
                    }
                    let state = if back { &mut self.rew } else { &mut self.ff };
                    state.flash = true;
                    let step = scrub::SEEK_SPEEDS[self.seek_speed] as i64 * scrub::SEEK_TAP as i64;
                    self.seek_by(if back { -step } else { step });
                } else {
                    for player in self.players.values() {
                        player.shuttle_release(scrub::SEEK_RELEASE_RAMP);
                    }
                }
            }
            Event::Tap => {
                let state = if back { &mut self.rew } else { &mut self.ff };
                state.flash = true;
                let step = scrub::SEEK_SPEEDS[self.seek_speed] as i64 * scrub::SEEK_TAP as i64;
                self.seek_by(if back { -step } else { step });
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
            for player in self.players.values() {
                player.shuttle(speed);
            }
        }
    }

    // --- Живая часть (REC/RACE) -----------------------------------------

    fn toggle_rec(&mut self, ctx: &egui::Context) {
        match self.phase {
            Phase::LiveBefore { live_since } => {
                self.start_recording_all();
                self.phase = Phase::RecordHead {
                    live_since,
                    rec_since: Instant::now(),
                };
            }
            Phase::RecordHead { live_since, .. } => {
                self.stop_recording_all();
                self.phase = Phase::LiveBefore { live_since };
            }
            Phase::RaceCountdown { live_since, .. } => {
                self.audio.cancel();
                self.stop_recording_all();
                self.phase = Phase::LiveBefore { live_since };
            }
            Phase::Race { live_since, .. } | Phase::RecordTail { live_since, .. } => {
                // Запись остановлена — сразу в DVR.
                self.stop_recording_all();
                self.enter_dvr(ctx, live_since);
            }
            _ => {}
        }
    }

    fn toggle_race(&mut self, ctx: &egui::Context) {
        match self.phase {
            Phase::LiveBefore { live_since } => {
                self.start_recording_all();
                self.start_countdown(live_since, Instant::now());
            }
            Phase::RecordHead {
                live_since,
                rec_since,
            } => self.start_countdown(live_since, rec_since),
            Phase::RaceCountdown {
                live_since,
                rec_since,
                ..
            } => {
                self.audio.cancel();
                self.phase = Phase::RecordHead {
                    live_since,
                    rec_since,
                };
            }
            Phase::Race { .. } => {
                // Финиш кнопкой — гудок, запись хвоста продолжается.
                self.audio.play(BUZZER);
                let race = match self.phase {
                    Phase::Race { start, .. } => start.elapsed(),
                    _ => Duration::ZERO,
                };
                if let Phase::Race {
                    live_since,
                    rec_since,
                    ..
                } = self.phase
                {
                    self.phase = Phase::RecordTail {
                        live_since,
                        rec_since,
                        race,
                    };
                }
                let _ = ctx;
            }
            _ => {}
        }
    }

    /// Старт отсчёта: звуковая программа и состояние RaceCountdown.
    fn start_countdown(&mut self, live_since: Instant, rec_since: Instant) {
        let interval = sample_start_delay(&self.rules.start_signal_delay);
        let (steps, delay) = if interval.is_zero() {
            (vec![(Duration::ZERO, BUZZER)], Duration::ZERO)
        } else {
            let mut steps: Vec<(Duration, audio::Sound)> = (0..PRE_BEEP_SECS)
                .map(Duration::from_secs)
                .map(|t| (t, STAGE))
                .collect();
            let delay = Duration::from_secs(PRE_BEEP_SECS) + interval;
            steps.push((delay, BUZZER));
            (steps, delay)
        };
        self.audio.sequence(steps);
        self.phase = Phase::RaceCountdown {
            live_since,
            rec_since,
            start: Instant::now(),
            delay,
        };
    }

    /// Перевод часов: конец отсчёта — в Race, лимит — в RecordTail,
    /// готовность плееров — из DvrOpening в Dvr.
    fn phase_tick(&mut self) {
        let now = Instant::now();
        match self.phase {
            Phase::RaceCountdown {
                live_since,
                rec_since,
                start,
                delay,
            } if now >= race_time_start(start, delay) => {
                self.phase = Phase::Race {
                    live_since,
                    rec_since,
                    start: race_time_start(start, delay),
                };
            }
            Phase::Race {
                live_since,
                rec_since,
                start,
            } => {
                let elapsed = now.saturating_duration_since(start);
                if elapsed >= self.rules.duration {
                    self.audio.play(BUZZER);
                    self.phase = Phase::RecordTail {
                        live_since,
                        rec_since,
                        race: elapsed,
                    };
                }
            }
            Phase::DvrOpening { live_since } => {
                // Готово = все плееры дошли до Ready/Failed (и пустой
                // набор считается готовым — иначе транзит вечный).
                let ready = self
                    .players
                    .values()
                    .all(|p| matches!(p.open_state(), PlayerState::Ready | PlayerState::Failed(_)));
                if ready {
                    self.phase = Phase::Dvr { live_since };
                }
            }
            _ => {}
        }
    }

    /// Самовзвод перерисовок счётчика гонки — только в живых фазах без
    /// кадров (в DVR кадры гонят перерисовки сами).
    fn arm_race_repaint(&self, ctx: &egui::Context) {
        if !self.phase.countdown() && !matches!(self.phase, Phase::Race { .. }) {
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

    /// Запись остановлена: гасим эфир, открываем плееры свежих записей.
    fn enter_dvr(&mut self, ctx: &egui::Context, live_since: Instant) {
        self.stop_all();
        if let Some(captures) = &self.captures {
            for camera_id in self.active_cameras.keys() {
                if let Some(path) = scrub::latest_capture(captures, camera_id) {
                    self.players
                        .insert(camera_id.clone(), Player::open(&path, ctx.clone()));
                }
            }
        }
        self.phase = Phase::DvrOpening { live_since };
    }

    // --- Эфир -------------------------------------------------------------

    fn start_captures(&mut self, ctx: &egui::Context) {
        for (id, camera) in &self.active_cameras {
            let webcam = crate::driver::camera::Camera::open(camera.clone(), ctx.clone());
            self.webcams.insert(id.clone(), webcam);
        }
        log::info!("flight: webcams starting: {}", self.webcams.len());
    }

    fn start_recording_all(&mut self) {
        for (id, camera) in &self.active_cameras {
            if let Some(webcam) = self.webcams.get(id) {
                webcam.start_recording(crate::driver::dvr::RecordParams {
                    dir: crate::driver::dvr::CAPTURES_DIR.into(),
                    camera_id: id.clone(),
                    camera: camera.clone(),
                });
            }
        }
    }

    fn stop_recording_all(&mut self) {
        for webcam in self.webcams.values() {
            webcam.stop_recording();
        }
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
        let ok = |message: String| Some(crate::remote::Response { message, code: 0 });
        match head {
            "rec" => {
                if self.phase.rec_available() {
                    self.toggle_rec(ctx);
                }
                ok(String::new())
            }
            "race" => {
                if self.phase.race_available() {
                    self.toggle_race(ctx);
                }
                ok(String::new())
            }
            "play" => {
                for player in self.players.values() {
                    if player.is_playing() {
                        player.pause();
                    } else {
                        player.play();
                    }
                }
                ok(String::new())
            }
            "ff" => {
                let step = scrub::SEEK_SPEEDS[self.seek_speed] as i64 * scrub::SEEK_TAP as i64;
                self.seek_by(step);
                ok(String::new())
            }
            "rew" => {
                let step = scrub::SEEK_SPEEDS[self.seek_speed] as i64 * scrub::SEEK_TAP as i64;
                self.seek_by(-step);
                ok(String::new())
            }
            "seek" => {
                let ms: u64 = command.get(1).and_then(|a| a.parse().ok())?;
                for player in self.players.values() {
                    player.seek(ms.min(player.duration()));
                }
                ok(String::new())
            }
            "exit" => {
                self.exit_requested = true;
                ctx.request_repaint();
                ok(String::new())
            }
            _ => None,
        }
    }
}

/// Наборы цветов кнопок (см. colors-table.html): два цвета на кнопку —
/// яркий (активный фон) и неяркий (покой; выключенный = неяркий × 0.5
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

/// Пауза для конкретного старта: равномерный выбор в
/// [min_delay, max_delay); вырожденный случай (min == max) — само значение.
fn sample_start_delay(signal: &StartSignalDelay) -> Duration {
    let min = signal.min();
    let span = signal.max().saturating_sub(min).as_millis() as u64;
    if span == 0 {
        return min;
    }
    min + Duration::from_millis(rand::random_range(0..span))
}

struct Res;

impl Res {
    fn new(ctx: &egui::Context) -> Self {
        let symbol_size = measure_text(ctx, style::FONT_REGULAR, "M");
        assert!(
            symbol_size == grid::CELL_SIZE,
            "Font size mismatch: expected {:?}, got {:?}. Update grid::CELL_SIZE or check font assets.",
            grid::CELL_SIZE,
            symbol_size,
        );
        Self
    }
}
