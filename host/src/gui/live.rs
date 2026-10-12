use super::*;
use crate::config::camera::CameraConfig;
use crate::config::setup::Setup;
use crate::driver::audio::{Audio, Sound};
use crate::driver::camera::CaptureState;
use crate::gui::grid::IntoCell;
use crate::model::pilot::Pilot;
use crate::model::race::{RaceRules, StartSignalDelay};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const MAX_PADS: usize = 4;
const GRID_WIDTH: isize = 160;
const LEFT_MARGIN: isize = 4;
const RIGHT_MARGIN: isize = 4;
const TOP_BAR_ROWS: isize = 2;
const BOTTOM_BAR_ROWS: isize = 2;

/// Число строк вьюфайндера по числу пилотов. Общие со scrub (его
/// раскладка — копия нашей).
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

pub struct Live {
    // Сессия: единая машина состояний эфира/записи/гонки за кнопками
    // LIVE/REC/RACE; потоки камер живут за ней.
    session: Session,
    webcams: HashMap<String, crate::driver::camera::Camera>,
    /// Запрет засыпания экрана, пока идёт эфир.
    awake: Option<keepawake::KeepAwake>,
    // Гонка: правила (пока default на месте вызова) и звук.
    rules: RaceRules,
    audio: Audio,
    // Экран: ресурсы, заголовок, сетап и раскладка.
    res: Option<Res>,
    header: header::Header,
    setup: Setup,
    assignments: HashMap<String, Pilot>,
    active_cameras: HashMap<String, CameraConfig>,
}

impl From<Box<Live>> for State {
    fn from(val: Box<Live>) -> Self {
        State::Live(val)
    }
}

impl Live {
    pub fn new(setup: Setup, assignments: HashMap<String, Pilot>, rules: RaceRules) -> Self {
        // Cameras feeding viewports of occupied pads (the screen shows at
        // most MAX_PADS columns); built once — assignments are fixed for the
        // lifetime of this screen.
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
        Self {
            session: Session::Idle,
            webcams: HashMap::new(),
            awake: None,
            rules,
            audio: Audio::spawn(),
            res: None,
            header: header::Header::new(),
            setup,
            assignments,
            active_cameras,
        }
    }

    pub fn update(&mut self, ui: &mut egui::Ui, navigator: &mut Navigator) {
        ui.ctx().viewport_id();
        enum Action {
            ToggleLive,
            ToggleRec,
            ToggleRace,
            GotoMenu,
            GotoScrub,
            None,
        }
        let mut action = Action::None;
        self.session_tick();
        self.arm_race_repaint(ui.ctx());

        if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::W)) {
            action = Action::GotoMenu;
        }

        let ctx = ui.ctx();
        self.res.get_or_insert_with(|| Res::new(ctx));

        // Per-camera viewfinder state: Texture once a frame is decoded,
        // Pending while the webcam runs but produced nothing yet. A camera
        // absent from this map means Off for its viewports.
        let mut contents: HashMap<String, viewfinder::ViewfinderContents> = HashMap::new();
        for (id, webcam) in self.webcams.iter_mut() {
            let capture_state = webcam.capture_state();
            webcam.update_frame();
            // Поток мёртв (камера не найдена, отвалилась) — testcard вместо
            // замершей последней текстуры. Starting тоже testcard: кадров
            // ещё нет (Live ставится по первому кадру).
            let state = if capture_state == CaptureState::Live {
                viewfinder::ViewfinderContents::Texture(webcam.texture())
            } else {
                viewfinder::ViewfinderContents::Pending
            };
            contents.insert(id.clone(), state);
        }

        view::Letterbox::new(grid::cell(160, 45))
            .max_virtual_height(grid::cell_y(50))
            .outline(true)
            .show(ui, |ui| {
                let pads: Vec<_> = self.setup.pads.iter().take(MAX_PADS).collect();
                let columns = pads.len() as isize;
                let viewfinder_rows = viewfinder_rows(pads.len());
                let viewfinder_cols = viewfinder_cols(pads.len());
                // Единый источник геометрии: строка от LEFT_MARGIN до
                // GRID_WIDTH - RIGHT_MARGIN делится на columns зон. Размер
                // вьюфайндера фиксирован — зона только центрирует его.
                let segment = (GRID_WIDTH - LEFT_MARGIN - RIGHT_MARGIN) / columns.max(1);
                let column_geometry: Vec<(isize, isize)> = (0..columns)
                    .map(|i| (LEFT_MARGIN + segment * i, segment))
                    .collect();

                if guidelines::ENABLED {
                    guidelines::marker_grid(ui, grid::cell(10, 5));

                    // Разделители на границах зон (windows(2): между
                    // колонками, не по краям). При центрированных
                    // вьюфайндерах граница зон = середина разделения.
                    for pair in column_geometry.windows(2) {
                        let boundary = pair[0].0 + pair[0].1;
                        guidelines::dashed_line(
                            ui,
                            egui::Direction::TopDown,
                            grid::cell_x(boundary),
                        );
                    }
                }

                // Alt: рамка вьюпорта каждого вьюфайндера — в линиях
                // (эффективная высота = высота кадра × высота viewport'а).
                // Мёртвый поток → error. Разрешение кадра берём здесь, до
                // заимствований letterbox-замыкания.
                let alt = ui.input(|i| i.modifiers.alt);
                let frame_res: HashMap<String, Option<crate::config::camera::ResolutionConfig>> =
                    if alt {
                        self.active_cameras
                            .iter()
                            .map(|(id, cfg)| {
                                let res = match self.webcams.get(id) {
                                    Some(w) if w.capture_state() == CaptureState::Dead => None,
                                    Some(w) => Some(w.resolution().unwrap_or(cfg.resolution)),
                                    None => Some(cfg.resolution),
                                };
                                (id.clone(), res)
                            })
                            .collect()
                    } else {
                        HashMap::new()
                    };

                let bottom_right = grid::cell_at(ui.max_rect().max);

                // Заголовок: rect во всю ширину контейнера, 2 клетки
                // высотой; левую/правую части раскладывает сам виджет.
                let header_rect = grid::cell(0, 0).extrude(bottom_right.col, 2);
                self.header.show(ui, header_rect);

                if columns > 0 {
                    for (i, (pad_id, pad)) in pads.into_iter().enumerate() {
                        // Вьюфайндер по горизонтали в середине зоны
                        // (с точностью до клетки).
                        let (zone_start, zone_width) = column_geometry[i];
                        let col = zone_start + (zone_width - viewfinder_cols) / 2;
                        let viewfinder_rect = grid::cell(col, TOP_BAR_ROWS)
                            .translate(0, 1)
                            .extrude(viewfinder_cols, viewfinder_rows + 2);
                        // Off: no webcam feeds this viewport; Pending: webcam
                        // runs but no frame decoded yet; Texture: live feed.
                        let webcam_contents = contents
                            .get(&pad.fpv.camera_id)
                            .copied()
                            .unwrap_or(viewfinder::ViewfinderContents::Off);
                        let pilot_name = self
                            .assignments
                            .get(pad_id)
                            .map_or("", |pilot| pilot.name.as_str());
                        let frame_rect = viewfinder_rect.with_min_y(grid::cell_y(TOP_BAR_ROWS + 1));
                        viewfinder::ViewfinderFrame::new(
                            &pad.label,
                            pilot_name,
                            pad.color.to_color32(),
                        )
                        .show(ui, frame_rect, |ui, rect| {
                            viewfinder::Viewfinder::new(pad.fpv.viewport.to_rect()).show(
                                ui,
                                webcam_contents,
                                rect,
                            );
                            // Alt: эффективное разрешение вьюпорта
                            // (значок — в правом нижнем углу картинки).
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

                        let graph_rect = viewfinder_rect
                            .left_bottom()
                            .into_cell()
                            .translate(0, 1)
                            .extrude(viewfinder_cols, 4);
                        ui.painter().rect(
                            graph_rect,
                            0.0,
                            egui::Color32::TRANSPARENT,
                            egui::Stroke::new(2.0, egui::Color32::from_gray(48)),
                            egui::StrokeKind::Inside,
                        );
                        let laps_rect = graph_rect
                            .left_bottom()
                            .into_cell()
                            .translate(0, 1)
                            .extrude(viewfinder_cols, 0)
                            .with_max_y(bottom_right.translate(0, -4).to_pos2().y);
                        guidelines::dashed_rect(ui, laps_rect);
                        ui.painter().text(
                            laps_rect.left_top(),
                            egui::Align2::LEFT_TOP,
                            "1) 4:56.789 \u{f0537} \u{f00d} \u{f00d} \u{ea72} \u{f4aa}",
                            style::FONT_REGULAR,
                            egui::Color32::WHITE,
                        );
                    }
                }

                // Статус-бар: кнопки-стрелки EXIT, DVR, LIVE, REC, RACE —
                // отражение машины состояний сессии (см. Session): клик
                // шлёт тоггл, сессия решает переход. Порядок блоков:
                // входы и шкалы статусов, раскладка цепочки, счётчик
                // гонки, кнопки, подсказки, отрисовка счётчика.

                // Входы: состояние потоков (шкалы статусов) и эфира.
                let live_state = self.webcams.values().next().map(|w| w.capture_state());
                let rec_active = self.webcams.values().any(|w| w.record_state().ok);
                let feed_on = self.session.live_on();

                // Шкалы статусов. LIVE — из сессии × CaptureState: Dead
                // при включённом эфире — error; кадры идут или поток ещё
                // поднимается (Starting — не отвал) — active; иначе idle.
                let live_status = match live_state {
                    Some(CaptureState::Dead) if feed_on => Status::Error,
                    Some(CaptureState::Live) => Status::Active,
                    _ if feed_on => Status::Active,
                    _ => Status::Idle,
                };
                let live_timer = session_timer(self.session.live_since());
                let rec_timer = session_timer(self.session.rec_since());
                // REC: включена запись, но кадров нет (поток мёртв или
                // стартовал слишком недавно) — error; иначе по факту.
                let feed_rec = self.session.rec_on();
                let rec_status =
                    if feed_rec && !rec_active && live_state != Some(CaptureState::Starting) {
                        Status::Error
                    } else if rec_active {
                        Status::Active
                    } else {
                        Status::Idle
                    };
                // RACE: активна фаза отсчёта или гонки (кислотный цвет).
                let race_active = self.session.race_active();

                // Раскладка цепочки. Кнопки-стрелки (PowerShell) —
                // виджеты NavButton: плашка с уголками + верхний текст +
                // нижний текст-доптаймер, 3 состояния (Idle/Active/
                // Disabled), высота виджета 3 клетки. Rect'ы текстов —
                // частная раскладка status_chain (scrub держит свою копию
                // — там EXIT только иконка). Гонка началась (Race и
                // дальше): все наконечники плашек разворачиваются влево,
                // иконки LIVE/REC/RACE меняются на стоп.
                let running = self.session.race_started();
                let (live_label, rec_label, race_label) = if running {
                    let stop = "\u{f04d}";
                    (
                        format!("{stop} LIVE"),
                        format!("{stop} REC"),
                        format!("{stop} RACE"),
                    )
                } else {
                    (
                        "\u{f04b} LIVE".to_owned(),
                        "\u{f044a} REC".to_owned(),
                        "\u{f140b} RACE".to_owned(),
                    )
                };
                let StatusChain {
                    exit: exit_rect,
                    dvr: dvr_rect,
                    live: live_rect,
                    rec: rec_rect,
                    race: race_rect,
                } = status_chain(
                    ui.ctx(),
                    bottom_right.row,
                    [
                        "\u{e6c0}\u{f0206} EXIT",
                        "\u{f16df} DVR",
                        &live_label,
                        &rec_label,
                        &race_label,
                    ],
                );
                // Плашки-якоря подсказок (по Alt) — рисует NavButton, тут
                // только rect для позиционирования.
                let live_plate = view::nav_plate_rect(live_rect);
                let rec_plate = view::nav_plate_rect(rec_rect);
                let race_plate = view::nav_plate_rect(race_rect);

                // Счётчик гонки — правее цепочки, без плашки. RACE и
                // счётчик — один контрол (кликабельны оба).
                // Мигание привязано к началу времени гонки (фазы по
                // 500 мс назад от него): последняя фаза ПОГАС кончается
                // ровно на нём — дальше счётчик ГОРИТ И ТИКАЕТ. ПОГАС —
                // пригашение на 80% (яркость 20%), не исчезновение.
                let blink_off = matches!(
                    self.session,
                    Session::RaceCountdown { start, delay, .. }
                        if race_time_start(start, delay)
                            .saturating_duration_since(Instant::now())
                            .as_millis() as u64
                            % 1000
                            >= 500
                );
                let race_timer = race_text(&self.session, Instant::now());
                let timer_pos = egui::pos2(
                    race_plate.right() + grid::cell_x(2),
                    race_rect.left_bottom().y,
                );

                // Края плашек: обычно цепочка смотрит вправо (EXIT — нос
                // влево, дальше стык «нос соседа → впадина»); в бегущей
                // гонке все наконечники — влево (стык тот же, развёрнутый).
                // Кнопки рисуем в порядке следования: соседние плашки
                // наползают друг на друга.
                let leftward = (
                    view::PlateEdge::ChevronConvex,
                    view::PlateEdge::ChevronConcave,
                );
                let rightward = if running {
                    leftward
                } else {
                    (
                        view::PlateEdge::ChevronConcave,
                        view::PlateEdge::ChevronConvex,
                    )
                };
                let live_edges = if running {
                    leftward
                } else {
                    (
                        view::PlateEdge::ChevronConvex,
                        view::PlateEdge::ChevronConvex,
                    )
                };
                // Доступность → состояние NavButton: недоступна —
                // Disabled (контур), иначе Idle/Active (error — активный:
                // включено, но поток мёртв).
                let nav_state = |available: bool, status: Status| {
                    if !available {
                        view::NavState::Disabled
                    } else {
                        match status {
                            Status::Idle => view::NavState::Idle,
                            _ => view::NavState::Active,
                        }
                    }
                };

                // Кнопки цепочки, в порядке следования.
                // EXIT — выход из режима Live (в меню).
                if view::NavButton::new(
                    "\u{e6c0}\u{f0206} EXIT",
                    "",
                    view::NavState::Idle,
                    PLATE_EXIT,
                    PLATE_EXIT,
                    leftward.0,
                    leftward.1,
                )
                .show(ui, exit_rect, ui.make_persistent_id("status_exit"))
                .is_some_and(|r| r.clicked())
                {
                    action = Action::GotoMenu;
                }

                // DVR: недоступна до LiveAfter; с LiveAfter (запись
                // закончена) — обычная залитая кнопка, клик ведёт в scrub.
                // «Сколько записано» — нижним текстом (фиксируется в
                // момент отжатия REC).
                if view::NavButton::new(
                    "\u{f16df} DVR",
                    self.session.recorded().map(mmss).unwrap_or_default(),
                    if self.session.dvr_available() {
                        view::NavState::Idle
                    } else {
                        view::NavState::Disabled
                    },
                    PLATE_REC_IDLE,
                    PLATE_REC_IDLE,
                    leftward.0,
                    leftward.1,
                )
                .show(ui, dvr_rect, ui.make_persistent_id("status_dvr"))
                .is_some_and(|r| r.clicked())
                {
                    action = Action::GotoScrub;
                }

                // LIVE; время сессии — нижним текстом.
                if view::NavButton::new(
                    &live_label,
                    &live_timer,
                    nav_state(self.session.live_available(), live_status),
                    PLATE_LIVE_IDLE,
                    PLATE_LIVE,
                    live_edges.0,
                    live_edges.1,
                )
                .show(ui, live_rect, ui.make_persistent_id("status_live"))
                .is_some_and(|r| r.clicked())
                {
                    action = Action::ToggleLive;
                }

                // REC; пока пишем — живой таймер нижним текстом.
                if view::NavButton::new(
                    &rec_label,
                    &rec_timer,
                    nav_state(self.session.rec_available(), rec_status),
                    PLATE_REC_IDLE,
                    PLATE_REC,
                    rightward.0,
                    rightward.1,
                )
                .show(ui, rec_rect, ui.make_persistent_id("status_rec"))
                .is_some_and(|r| r.clicked())
                {
                    action = Action::ToggleRec;
                }

                // RACE. Счётчику — отдельная клик-бласть (иконка + текст).
                let timer_icon = "\u{f43a}";
                let timer_icon_w =
                    super::measure_text(ui.ctx(), style::FONT_REGULAR_X2, timer_icon).x;
                let race_timer_hit = egui::Rect::from_min_max(
                    egui::pos2(timer_pos.x, race_plate.top()),
                    egui::pos2(
                        timer_pos.x
                            + timer_icon_w
                            + grid::cell_x(1)
                            + super::measure_text(ui.ctx(), style::FONT_REGULAR_X2, &race_timer).x,
                        race_plate.bottom(),
                    ),
                );
                let race_id = ui.make_persistent_id("status_race");
                if let Some(race_response) = view::NavButton::new(
                    &race_label,
                    "",
                    nav_state(
                        self.session.race_available(),
                        if race_active {
                            Status::Active
                        } else {
                            Status::Idle
                        },
                    ),
                    PLATE_RACE_IDLE,
                    PLATE_RACE,
                    rightward.0,
                    rightward.1,
                )
                .show(ui, race_rect, race_id)
                {
                    let race_response = race_response.union(
                        ui.interact(race_timer_hit, race_id.with("timer"), egui::Sense::click())
                            .on_hover_cursor(egui::CursorIcon::PointingHand),
                    );
                    if race_response.clicked() {
                        action = Action::ToggleRace;
                    }
                }

                // Alt: fps-подсказки над LIVE/REC — перечисление камер
                // (первая — ближе всех к кнопке, дальше растём вверх).
                // Трансляция/запись не идут — целевой fps камеры; поток
                // мёртв или запись провалилась — error.
                if ui.input(|i| i.modifiers.alt) {
                    // Значок камеры (\u{f03d}) — в каждой строке LIVE;
                    // значок записи (\u{f16df}) — в каждой строке
                    // REC, а при ошибке именно камеры к ней добавляется и
                    // значок камеры. Подсказки на 2 колонки левее колонки
                    // текста кнопки — под эти значки.
                    let live_lines: Vec<String> = self
                        .active_cameras
                        .iter()
                        .map(|(id, cfg)| match self.webcams.get(id) {
                            Some(w) if w.capture_state() == CaptureState::Dead => {
                                "\u{f03d} error".to_owned()
                            }
                            Some(w) => w
                                .fps()
                                .map(|f| format!("\u{f03d} {f}fps"))
                                .unwrap_or_else(|| format!("\u{f03d} {}", cfg.frame_rate)),
                            None => format!("\u{f03d} {}", cfg.frame_rate),
                        })
                        .collect();
                    fps_hint(
                        ui,
                        live_plate,
                        live_rect.left() + grid::cell_x(1),
                        &live_lines,
                    );

                    let rec_lines: Vec<String> = self
                        .active_cameras
                        .iter()
                        .map(|(id, cfg)| {
                            let target = cfg.dvr.frame_rate.unwrap_or(cfg.frame_rate);
                            match self.webcams.get(id) {
                                // Мёртвый захват тянет за собой ошибку
                                // записи: error независимо от того,
                                // стартовал ли writer (иначе до этой
                                // ветки не доходит проверка и рисуется
                                // целевой fps).
                                Some(w) if w.capture_state() == CaptureState::Dead => {
                                    "\u{f03d} error".to_owned()
                                }
                                Some(w) if rec_active && !w.record_state().ok => {
                                    "\u{f16df} error".to_owned()
                                }
                                Some(w) if w.record_state().ok && w.record_state().fps > 0.0 => {
                                    format!("\u{f16df} {:.0}fps", w.record_state().fps)
                                }
                                _ => format!("\u{f16df} {target}"),
                            }
                        })
                        .collect();
                    fps_hint(ui, rec_plate, rec_rect.left() + grid::cell_x(1), &rec_lines);

                    // RACE: не идёт — длина гонки с заглавной сигмой
                    // (суммарное время); идёт — остаток со знаком минус.
                    let race_hint = match self.session {
                        Session::Race { .. } => {
                            let left = self
                                .rules
                                .duration
                                .saturating_sub(self.session.race_elapsed(Instant::now()));
                            format!("-{}", mmss(left))
                        }
                        _ => format!("Σ{}", mmss(self.rules.duration)),
                    };
                    fps_hint(
                        ui,
                        race_plate,
                        race_rect.left() + grid::cell_x(3),
                        &[race_hint],
                    );
                }

                // Отрисовка счётчика. Цвет: бежит белым; фаза ПОГАС
                // мигания и нули вне отсчёта (покой, отменённый старт) —
                // 20% белого.
                let zeros_dim = self.session.race_elapsed(Instant::now()).is_zero()
                    && !self.session.countdown();
                let timer_color = if blink_off || zeros_dim {
                    egui::Color32::WHITE.gamma_multiply(0.2)
                } else {
                    egui::Color32::WHITE
                };
                // Иконка и текст счётчика — одним цветом, зазор ровно
                // колонка (font space шире ячейки — позиционируем руками).
                ui.painter().text(
                    timer_pos,
                    egui::Align2::LEFT_BOTTOM,
                    timer_icon,
                    style::FONT_REGULAR_X2,
                    timer_color,
                );
                ui.painter().text(
                    egui::pos2(timer_pos.x + timer_icon_w + grid::cell_x(1), timer_pos.y),
                    egui::Align2::LEFT_BOTTOM,
                    race_timer,
                    style::FONT_REGULAR_X2,
                    timer_color,
                );

                guidelines::dashed_line(
                    ui,
                    egui::Direction::RightToLeft,
                    grid::cell_y(TOP_BAR_ROWS),
                );
                guidelines::dashed_line(
                    ui,
                    egui::Direction::RightToLeft,
                    grid::cell_y(bottom_right.row - 1 - BOTTOM_BAR_ROWS),
                );
            });

        match action {
            Action::GotoMenu => navigator.goto(menu::Menu),
            Action::GotoScrub => navigator.goto(Box::new(scrub::Scrub::new(
                ui.ctx(),
                self.setup.clone(),
                self.assignments.clone(),
                crate::driver::dvr::CAPTURES_DIR.into(),
            ))),
            Action::ToggleLive => self.toggle_live(ui.ctx()),
            Action::ToggleRec => self.toggle_rec(ui.ctx()),
            Action::ToggleRace => self.toggle_race(ui.ctx()),
            Action::None => {}
        }
    }

    // --- Сессия (LIVE/REC/RACE) ------------------------------------------

    /// LIVE: из Idle — включить эфир. Отжатие до старта гонки — назад
    /// (в Idle), после старта — сразу Done (хвост не дописываем).
    fn toggle_live(&mut self, ctx: &egui::Context) {
        match self.session {
            Session::Idle => {
                self.start_captures(ctx);
                self.screen_awake();
                self.session = Session::LiveBefore {
                    live_since: Instant::now(),
                };
            }
            Session::LiveBefore { .. } | Session::RaceCountdown { .. } => {
                self.audio.cancel();
                self.stop_all();
                self.session = Session::Idle;
            }
            Session::RecordHead { .. } => {
                self.stop_recording_all();
                self.stop_all();
                self.session = Session::Idle;
            }
            Session::Race { .. } => {
                let (_, rec_since, race) = self.finish_race();
                self.stop_recording_all();
                self.stop_all();
                self.session = Session::Done {
                    recorded: rec_since.elapsed(),
                    race,
                };
            }
            Session::RecordTail {
                rec_since, race, ..
            } => {
                self.stop_recording_all();
                self.stop_all();
                self.session = Session::Done {
                    recorded: rec_since.elapsed(),
                    race,
                };
            }
            Session::LiveAfter { recorded, race, .. } => {
                self.stop_all();
                self.session = Session::Done { recorded, race };
            }
            Session::Done { .. } => {}
        }
    }

    /// REC: нажатие из Idle стартует и захват (REC-on транзитивно
    /// нажимает LIVE), из LiveBefore — только запись; отжатие — до
    /// LiveAfter. В Race+ отжатие стопает запись и фиксирует её длину;
    /// в Race переход сразу в LiveAfter (RecordTail — только через
    /// финиш RACE: клик или лимит).
    fn toggle_rec(&mut self, ctx: &egui::Context) {
        match self.session {
            Session::Idle => {
                self.start_captures(ctx);
                self.start_recording_all();
                self.screen_awake();
                let now = Instant::now();
                self.session = Session::RecordHead {
                    live_since: now,
                    rec_since: now,
                };
            }
            Session::LiveBefore { live_since } => {
                self.start_recording_all();
                self.session = Session::RecordHead {
                    live_since,
                    rec_since: Instant::now(),
                };
            }
            Session::RecordHead { live_since, .. } => {
                self.stop_recording_all();
                self.session = Session::LiveBefore { live_since };
            }
            Session::RaceCountdown { live_since, .. } => {
                // Отжатие REC отменяет и отсчёт (ход назад).
                self.audio.cancel();
                self.stop_recording_all();
                self.session = Session::LiveBefore { live_since };
            }
            Session::Race { .. } => {
                // Отжатие REC в гонке: запись стоп, сразу LiveAfter.
                let (live_since, rec_since, race) = self.finish_race();
                self.stop_recording_all();
                self.session = Session::LiveAfter {
                    live_since,
                    recorded: rec_since.elapsed(),
                    race,
                };
            }
            Session::RecordTail {
                live_since,
                rec_since,
                race,
            } => {
                self.stop_recording_all();
                self.session = Session::LiveAfter {
                    live_since,
                    recorded: rec_since.elapsed(),
                    race,
                };
            }
            _ => {}
        }
    }

    /// RACE: нажатие из Idle/LiveBefore/RecordHead — старт отсчёта
    /// (REC транзитивно нажат, с Idle — и LIVE). Отжатие в RaceCountdown
    /// — отмена (назад в RecordHead), в Race — финиш (вперёд в
    /// RecordTail, кнопка дальше недоступна).
    fn toggle_race(&mut self, ctx: &egui::Context) {
        match self.session {
            Session::Idle => {
                self.start_captures(ctx);
                self.start_recording_all();
                self.screen_awake();
                let now = Instant::now();
                self.start_countdown(now, now);
            }
            Session::LiveBefore { live_since } => {
                self.start_recording_all();
                self.start_countdown(live_since, Instant::now());
            }
            Session::RecordHead {
                live_since,
                rec_since,
            } => self.start_countdown(live_since, rec_since),
            Session::RaceCountdown {
                live_since,
                rec_since,
                ..
            } => {
                self.audio.cancel();
                self.session = Session::RecordHead {
                    live_since,
                    rec_since,
                };
            }
            Session::Race { .. } => {
                self.finish_race();
            }
            _ => {}
        }
    }

    /// Старт отсчёта: звуковая программа (бипы + случайная пауза +
    /// гудок) и состояние RaceCountdown.
    fn start_countdown(&mut self, live_since: Instant, rec_since: Instant) {
        let interval = sample_start_delay(&self.rules.start_signal_delay);
        let (steps, delay) = if interval.is_zero() {
            (vec![(Duration::ZERO, BUZZER)], Duration::ZERO)
        } else {
            let mut steps: Vec<(Duration, Sound)> = (0..PRE_BEEP_SECS)
                .map(Duration::from_secs)
                .map(|t| (t, STAGE))
                .collect();
            let delay = Duration::from_secs(PRE_BEEP_SECS) + interval;
            steps.push((delay, BUZZER));
            (steps, delay)
        };
        self.audio.sequence(steps);
        self.session = Session::RaceCountdown {
            live_since,
            rec_since,
            start: Instant::now(),
            delay,
        };
    }

    /// Финиш гонки: гудок, время замирает, сессия — в RecordTail
    /// (запись хвоста продолжается). Возвращает (эфир с, запись с,
    /// замороженное время). Из не-Race — no-op.
    fn finish_race(&mut self) -> (Instant, Instant, Duration) {
        if let Session::Race {
            live_since,
            rec_since,
            start,
            ..
        } = self.session
        {
            self.audio.play(BUZZER);
            let race = start.elapsed();
            self.session = Session::RecordTail {
                live_since,
                rec_since,
                race,
            };
            (live_since, rec_since, race)
        } else {
            (Instant::now(), Instant::now(), Duration::ZERO)
        }
    }

    fn start_captures(&mut self, ctx: &egui::Context) {
        for (id, camera) in &self.active_cameras {
            let webcam = crate::driver::camera::Camera::open(camera.clone(), ctx.clone());
            self.webcams.insert(id.clone(), webcam);
        }
        log::info!("webcams starting: {}", self.webcams.len());
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
        log::info!("webcams stopped");
        self.awake = None;
    }

    /// Запретить засыпание экрана на время эфира; ошибка inhibit — warn,
    /// не причина отказа от показа.
    fn screen_awake(&mut self) {
        if self.awake.is_some() {
            return;
        }
        match keepawake::Builder::default()
            .display(true)
            .reason("live view")
            .app_name("Laps")
            .create()
        {
            Ok(awake) => self.awake = Some(awake),
            Err(e) => log::warn!("screen inhibit failed: {e}"),
        }
    }

    // --- Часы сессии ------------------------------------------------------

    /// Перевод часов: конец отсчёта — в Race (звук гудка уже в программе
    /// отсчёта — здесь только переход состояния), лимит времени вылета —
    /// в RecordTail (RACE отжимается сама). Предупреждение о лимите —
    /// один гудок за прогон.
    fn session_tick(&mut self) {
        let now = Instant::now();
        match self.session {
            Session::RaceCountdown {
                live_since,
                rec_since,
                start,
                delay,
            } if now >= race_time_start(start, delay) => {
                self.session = Session::Race {
                    live_since,
                    rec_since,
                    start: race_time_start(start, delay),
                    warned: false,
                };
            }
            Session::Race {
                start,
                ref mut warned,
                ..
            } => {
                let elapsed = now.saturating_duration_since(start);
                let limit = self.rules.duration;
                if !*warned && limit > BUZZER.duration() && elapsed >= limit - BUZZER.duration() {
                    self.audio.play(BUZZER);
                    *warned = true;
                }
                if elapsed >= limit {
                    self.finish_race();
                }
            }
            _ => {}
        }
    }

    /// Самовзвод перерисовок счётчика гонки — только когда нет живой
    /// трансляции: с ней счётчик едет на кадрах вьюфайндеров, и гнать
    /// repaint асап значит войти в клинч с feed по частоте. Заводим с
    /// полом частоты (не чаще ~23 fps): счётчику хватает, CPU бережём.
    /// Зависшая трансляция — зависший счётчик, осознанно.
    fn arm_race_repaint(&self, ctx: &egui::Context) {
        if !self.session.race_active() {
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

    // --- Удалённое управление ---------------------------------------------

    /// Команда удалённого управления. Some — обработана, ответ готов;
    /// None — не моя, экран отдаёт её уровнем выше.
    pub fn handle_remote(
        &mut self,
        ctx: &egui::Context,
        command: &[String],
    ) -> Option<crate::remote::Response> {
        let head = command.first()?.as_str();
        match head {
            "toggle-live" | "toggle-rec" | "toggle-race" => {
                if command.len() > 1 {
                    return Some(crate::remote::Response {
                        message: format!("{head} takes no arguments"),
                        code: 2,
                    });
                }
                if head == "toggle-live" {
                    self.toggle_live(ctx);
                } else if head == "toggle-rec" {
                    self.toggle_rec(ctx);
                } else {
                    self.toggle_race(ctx);
                }
                Some(crate::remote::Response {
                    message: String::new(),
                    code: 0,
                })
            }
            _ => None,
        }
    }
}

/// Сессия эфира: единая машина состояний за кнопками LIVE/REC/RACE.
/// Основная последовательность:
/// Idle → LiveBefore → RecordHead → RaceCountdown → Race → RecordTail →
/// LiveAfter → Done. До старта гонки отжатия кнопок шагают назад по
/// цепочке, после старта — только вперёд. Done терминальное (сброса
/// нет). В вариантах — моменты старта для таймеров кнопок и часы гонки.
#[derive(Clone, Copy, Debug)]
enum Session {
    Idle,
    /// Эфир без записи.
    LiveBefore {
        live_since: Instant,
    },
    /// Эфир + запись (голова записи).
    RecordHead {
        live_since: Instant,
        rec_since: Instant,
    },
    /// Отсчёт до старта; start/delay — программа звуков (см.
    /// race_time_start).
    RaceCountdown {
        live_since: Instant,
        rec_since: Instant,
        start: Instant,
        delay: Duration,
    },
    /// Гонка идёт; start — начало времени гонки.
    Race {
        live_since: Instant,
        rec_since: Instant,
        start: Instant,
        warned: bool,
    },
    /// Гонка кончилась, запись хвоста продолжается; race — замороженное
    /// время гонки для счётчика.
    RecordTail {
        live_since: Instant,
        rec_since: Instant,
        race: Duration,
    },
    /// Запись отжата, эфир жив; recorded — зафиксированная длина
    /// записи (момент отжатия REC): таймер «сколько записано» живёт
    /// под DVR.
    LiveAfter {
        live_since: Instant,
        recorded: Duration,
        race: Duration,
    },
    /// Всё отжато; recorded/race — как в LiveAfter.
    Done {
        recorded: Duration,
        race: Duration,
    },
}

impl Session {
    /// Эфир включён (LIVE нажата): всё, кроме Idle и Done.
    fn live_on(&self) -> bool {
        !matches!(self, Session::Idle | Session::Done { .. })
    }

    /// Запись идёт: RecordHead … RecordTail.
    fn rec_on(&self) -> bool {
        matches!(
            self,
            Session::RecordHead { .. }
                | Session::RaceCountdown { .. }
                | Session::Race { .. }
                | Session::RecordTail { .. }
        )
    }

    /// Гонка началась (Race и дальше): хвостики плашек влево, иконки
    /// stop, отжатия ходят только вперёд.
    fn race_started(&self) -> bool {
        matches!(
            self,
            Session::Race { .. }
                | Session::RecordTail { .. }
                | Session::LiveAfter { .. }
                | Session::Done { .. }
        )
    }

    /// Идёт отсчёт (счётчик мигает, RACE кликабельна на отмену).
    fn countdown(&self) -> bool {
        matches!(self, Session::RaceCountdown { .. })
    }

    /// Отсчёт или гонка: период активного счётчика (самовзвод repaint).
    fn race_active(&self) -> bool {
        matches!(self, Session::RaceCountdown { .. } | Session::Race { .. })
    }

    /// LIVE кликабельна: всё, кроме Done.
    fn live_available(&self) -> bool {
        !matches!(self, Session::Done { .. })
    }

    /// REC кликабельна: из Idle (REC-on стартует и захват) и с эфира до
    /// конца хвоста. LiveAfter+ — недоступна.
    fn rec_available(&self) -> bool {
        matches!(
            self,
            Session::Idle
                | Session::LiveBefore { .. }
                | Session::RecordHead { .. }
                | Session::RaceCountdown { .. }
                | Session::Race { .. }
                | Session::RecordTail { .. }
        )
    }

    /// RACE кликабельна: нажатие до старта (Idle/LiveBefore/RecordHead),
    /// отжатие в Countdown/Race. RecordTail+ — недоступна.
    fn race_available(&self) -> bool {
        matches!(
            self,
            Session::Idle
                | Session::LiveBefore { .. }
                | Session::RecordHead { .. }
                | Session::RaceCountdown { .. }
                | Session::Race { .. }
        )
    }

    /// DVR кликабельна: с LiveAfter (клик пока без действия).
    fn dvr_available(&self) -> bool {
        matches!(self, Session::LiveAfter { .. } | Session::Done { .. })
    }

    /// Момент старта эфира (таймер LIVE).
    fn live_since(&self) -> Option<Instant> {
        match *self {
            Session::Idle | Session::Done { .. } => None,
            Session::LiveBefore { live_since } => Some(live_since),
            Session::RecordHead { live_since, .. }
            | Session::RaceCountdown { live_since, .. }
            | Session::Race { live_since, .. }
            | Session::RecordTail { live_since, .. }
            | Session::LiveAfter { live_since, .. } => Some(live_since),
        }
    }

    /// Момент начала записи (живой таймер REC; только пока пишем).
    fn rec_since(&self) -> Option<Instant> {
        match *self {
            Session::RecordHead { rec_since, .. }
            | Session::RaceCountdown { rec_since, .. }
            | Session::Race { rec_since, .. }
            | Session::RecordTail { rec_since, .. } => Some(rec_since),
            _ => None,
        }
    }

    /// Зафиксированная длина записи (фиксируется в момент отжатия REC):
    /// таймер «сколько записано» под DVR. None, пока записи не было.
    fn recorded(&self) -> Option<Duration> {
        match *self {
            Session::LiveAfter { recorded, .. } | Session::Done { recorded, .. } => Some(recorded),
            _ => None,
        }
    }

    /// Время гонки для счётчика: 0 до старта, бежит в Race, заморожено
    /// после.
    fn race_elapsed(&self, now: Instant) -> Duration {
        match *self {
            Session::Race { start, .. } => now.saturating_duration_since(start),
            Session::RecordTail { race, .. }
            | Session::LiveAfter { race, .. }
            | Session::Done { race, .. } => race,
            _ => Duration::ZERO,
        }
    }
}

/// Звуки гонки: live их и проигрывает. Бинари — в `assets`.
const STAGE: Sound = Sound::from_bytes(crate::assets::STAGE_WAV);
const BUZZER: Sound = Sound::from_bytes(crate::assets::BUZZER_WAV);

/// Подложки кнопок-стрелок. Активные: LIVE — ярко-голубой, REC —
/// gruvbox red, RACE — кислотный yellow-green: тройка далеко по кругу.
/// В покое — пригашенные светлые оттенки. Текст всегда чёрный.
const PLATE_LIVE_IDLE: egui::Color32 = egui::Color32::from_rgb(0x54, 0x70, 0x7D);
const PLATE_LIVE: egui::Color32 = egui::Color32::from_rgb(0x4F, 0xB3, 0xE0);
const PLATE_REC_IDLE: egui::Color32 = egui::Color32::from_rgb(0x7E, 0x4C, 0x46);
const PLATE_REC: egui::Color32 = egui::Color32::from_rgb(0xE0, 0x30, 0x24);
const PLATE_RACE_IDLE: egui::Color32 = egui::Color32::from_rgb(0x7A, 0x7C, 0x49);
/// Активный RACE — кислотно-жёлтый гоночного режима (изначальный
/// RACE_COLOR, см. git history).
const PLATE_RACE: egui::Color32 = egui::Color32::from_rgb(0xCC, 0xFF, 0x00);
const PLATE_EXIT: egui::Color32 = egui::Color32::from_rgb(0x70, 0x70, 0x70);

/// Rect'ы текстов цепочки кнопок статуса внизу слева (EXIT, DVR, LIVE,
/// REC, RACE), слева направо. Раскладка частная для live; scrub держит
/// свою копию (там EXIT — только иконка) — при изменении цепочки
/// пересчитать и там.
struct StatusChain {
    exit: egui::Rect,
    dvr: egui::Rect,
    live: egui::Rect,
    rec: egui::Rect,
    race: egui::Rect,
}

/// Цепочка с колонки 2: текстовые rect'ы 2 клетки высотой, прижаты
/// низом к нижней строке; виджет кнопки 3 клетки — нижняя строка
/// (доптекст) уходит ниже текста. Тексты разнесены на 6
/// колонок (воздух по 2 колонки с каждой стороны у каждой плашки).
/// Подписи пяти кнопок приходят снаружи — иконки зависят от состояния
/// (в гонке LIVE/REC/RACE — стоп).
fn status_chain(ctx: &egui::Context, bottom_row: isize, labels: [&str; 5]) -> StatusChain {
    let text_rect = |prev: Option<egui::Rect>, text: &str| {
        let cols = grid::whole_cols(view::content_width(ctx, text));
        match prev {
            None => grid::cell(1, bottom_row).translate(1, -1).extrude(cols, -2),
            Some(prev) => prev
                .right_bottom()
                .into_cell()
                .translate(6, 0)
                .extrude(cols, -2),
        }
    };
    let [exit_t, dvr_t, live_t, rec_t, race_t] = labels;
    let exit = text_rect(None, exit_t);
    let dvr = text_rect(Some(exit), dvr_t);
    let live = text_rect(Some(dvr), live_t);
    let rec = text_rect(Some(live), rec_t);
    let race = text_rect(Some(rec), race_t);
    StatusChain {
        exit,
        dvr,
        live,
        rec,
        race,
    }
}

/// Фиксированная фаза бипов отсчёта: PRE_BEEP_SECS бипов с периодом
/// 1 с, затем случайный интервал из StartSignalDelay, затем гудок.
const PRE_BEEP_SECS: u64 = 3;

/// Момент начала времени гонки: конец отсчёта + длина гудка. До него
/// счётчик мигает, на нём — гаснет последняя фаза ПОГАС и дальше ГОРИТ
/// И ТИКАЕТ.
fn race_time_start(start: Instant, delay: Duration) -> Instant {
    start + delay + BUZZER.duration()
}

/// Текст счётчика гонки как последнее «слово» кнопки RACE. Формат
/// строго mm:ss.mmm. Во время отсчёта — нули (мигают, см. blink ниже),
/// после гудка — время гонки бежит вверх, в покое и после финиша —
/// замороженное.
fn race_text(session: &Session, now: Instant) -> String {
    let total = session.race_elapsed(now).as_millis() as u64;
    format!(
        "{:02}:{:02}.{:03}",
        total / 60_000,
        (total / 1_000) % 60,
        total % 1_000
    )
}

/// Пауза для конкретного старта: равномерный выбор в
/// [min_delay, max_delay); вырожденный случай (min == max) — само
/// значение.
fn sample_start_delay(signal: &StartSignalDelay) -> Duration {
    let min = signal.min();
    let span = signal.max().saturating_sub(min).as_millis() as u64;
    if span == 0 {
        return min;
    }
    min + Duration::from_millis(rand::random_range(0..span))
}

/// Статус кнопки статус-бара одной шкалой: что рисовать мелким текстом
/// и каким цветом — вместо пары пересекающихся булевых (error влечёт
/// !active). Каждая кнопка раскладывает шкалу в свои цвета.
#[derive(Clone, Copy)]
enum Status {
    /// Покой: серый, текст — заявленный fps.
    Idle,
    /// Работает: цвет «активный» кнопки, текст — измеренный fps.
    Active,
    /// Включено, но поток мёртв: цвет «активный» (не серый!), текст «error».
    Error,
}

/// Таймер сессии: полные минуты и секунды, минуты не сбрасываются
/// за час (78:00, 132:30 — штатно). Пусто, если сессии нет.
fn session_timer(since: Option<Instant>) -> String {
    let Some(since) = since else {
        return String::new();
    };
    mmss(since.elapsed())
}

/// mm:ss от длительности: полные минуты и секунды.
fn mmss(dur: Duration) -> String {
    let secs = dur.as_secs();
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

/// Блок fps-подсказки над кнопкой (по Alt): строки по камерам, первая —
/// через одну ячейку над плашкой, остальные растут вверх. Левый край
/// текста — по колонке текста кнопки.
fn fps_hint(ui: &mut egui::Ui, anchor: egui::Rect, text_x: f32, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    let row = grid::CELL_SIZE.y;
    let bottom = anchor.top() - row;
    let top = bottom - row * lines.len() as f32;
    let width = lines
        .iter()
        .map(|line| super::measure_text(ui.ctx(), style::FONT_REGULAR, line).x)
        .fold(0.0f32, f32::max);
    // Пол-высоты клетки вниз — визуальная центровка значка.
    let text =
        egui::Rect::from_min_max(egui::pos2(text_x, top), egui::pos2(text_x + width, bottom))
            .translate(egui::vec2(0.0, row * 0.5));
    view::hint_badge(ui, text, egui::Align2::LEFT_BOTTOM, lines);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Сэмпл паузы старта: вырожденный диапазон (min == max) — само
    /// значение, обычный — внутри [min, max).
    #[test]
    fn sample_start_delay_semantics() {
        assert_eq!(
            sample_start_delay(&StartSignalDelay::random(
                Duration::from_secs(7),
                Duration::from_secs(7)
            )),
            Duration::from_secs(7)
        );
        let delay = sample_start_delay(&StartSignalDelay::random(
            Duration::from_secs(2),
            Duration::from_secs(4),
        ));
        assert!(
            delay >= Duration::from_secs(2) && delay < Duration::from_secs(4),
            "random delay {delay:?} outside [2s, 4s)"
        );
    }

    /// Таблица доступности кнопок и флагов по фазам сессии — спека
    /// машины состояний (см. комментарий к Session). Парами флагов:
    /// (live_on, rec_on, race_started) и (live, rec, race, dvr) —
    /// кликабельность.
    #[test]
    fn session_flags_and_availability() {
        let now = Instant::now();
        let zero = Duration::ZERO;
        let phases = [
            (
                Session::Idle,
                (false, false, false),
                (true, true, true, false),
            ),
            (
                Session::LiveBefore { live_since: now },
                (true, false, false),
                (true, true, true, false),
            ),
            (
                Session::RecordHead {
                    live_since: now,
                    rec_since: now,
                },
                (true, true, false),
                (true, true, true, false),
            ),
            (
                Session::RaceCountdown {
                    live_since: now,
                    rec_since: now,
                    start: now,
                    delay: zero,
                },
                (true, true, false),
                (true, true, true, false),
            ),
            (
                Session::Race {
                    live_since: now,
                    rec_since: now,
                    start: now,
                    warned: false,
                },
                (true, true, true),
                (true, true, true, false),
            ),
            (
                Session::RecordTail {
                    live_since: now,
                    rec_since: now,
                    race: zero,
                },
                (true, true, true),
                (true, true, false, false),
            ),
            (
                Session::LiveAfter {
                    live_since: now,
                    recorded: zero,
                    race: zero,
                },
                (true, false, true),
                (true, false, false, true),
            ),
            (
                Session::Done {
                    recorded: zero,
                    race: zero,
                },
                (false, false, true),
                (false, false, false, true),
            ),
        ];
        for (session, (on, rec, started), (live, rec_a, race, dvr)) in phases {
            assert_eq!(
                (session.live_on(), session.rec_on(), session.race_started()),
                (on, rec, started),
                "flags {session:?}"
            );
            assert_eq!(
                (
                    session.live_available(),
                    session.rec_available(),
                    session.race_available(),
                    session.dvr_available()
                ),
                (live, rec_a, race, dvr),
                "availability {session:?}"
            );
        }
    }
}
