use super::*;
use crate::config::camera::CameraConfig;
use crate::config::setup::Setup;
use crate::driver::audio;
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
    // Эфир: состояние кнопок LIVE/REC и потоки камер за ними.
    feed: FeedState,
    webcams: HashMap<String, crate::driver::camera::Camera>,
    /// Запрет засыпания экрана, пока идёт эфир.
    awake: Option<keepawake::KeepAwake>,
    // Гонка: правила (пока default на месте вызова), состояние, звук.
    rules: RaceRules,
    race: RaceState,
    audio: crate::driver::audio::Audio,
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
            feed: FeedState::Off,
            webcams: HashMap::new(),
            awake: None,
            rules,
            race: RaceState::Idle,
            audio: crate::driver::audio::Audio::spawn(),
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
            None,
        }
        let mut action = Action::None;
        self.race_tick();
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

                // Статус-бар: LIVE, REC, RACE. LIVE и REC — независимые
                // виджеты: клик по LIVE тогглит захват, клик по REC —
                // запись. Время сессии — под плашкой: одинарным шрифтом
                // по центру, цветом подложки; пока сессии нет — не
                // рисуется. Кликабельна вся область плашки.
                let live_state = self.webcams.values().next().map(|w| w.capture_state());
                let rec_active = self.webcams.values().any(|w| w.record_state().ok);
                let feed_on = !matches!(self.feed, FeedState::Off);

                // Шкала Status — напрямую из FeedState × CaptureState:
                // Dead при включённом эфире — error; кадры идут или поток
                // ещё поднимается (Starting — не отвал) — active; иначе idle.
                let live_status = match live_state {
                    Some(CaptureState::Dead) if feed_on => Status::Error,
                    Some(CaptureState::Live) => Status::Active,
                    _ if feed_on => Status::Active,
                    _ => Status::Idle,
                };
                let live_timer = session_timer(self.feed.live_since());
                let rec_timer = session_timer(self.feed.rec_since());
                // Плашки-стрелки цепочкой (PowerShell): каждая правая
                // наползает на наконечник левой, поэтому левая часть
                // плашки начинается в 2 клетках левее текста и рисуются
                // они справа налево — правая под левой. Вокруг текста —
                // по две колонки воздуха с каждой стороны (левая часть
                // уходит под наконечник соседа, тексты разнесены на 6
                // колонок, чтобы воздух не съедался). Состояние — пара
                // (подложка, текст):
                // тусклая + чёрный в покое, насыщенная + белый в активе.
                // Счётчик RACE — дальше, без плашки.
                let tip = grid::cell_x(2);
                let overlap = grid::cell_x(2);
                let air = grid::cell_x(2);
                let live_text_w = view::content_width(ui.ctx(), "\u{25b6} LIVE");
                let live_rect = grid::cell(1, bottom_right.row)
                    .translate(1, -1)
                    .extrude(grid::whole_cols(live_text_w), -2);
                let live_plate = plate_rect(live_rect, tip, air, overlap);
                let (live_plate_color, live_text_color) =
                    plate_colors(live_status, PLATE_LIVE_IDLE, PLATE_LIVE);

                let feed_rec = matches!(self.feed, FeedState::Rec { .. });
                let rec_status =
                    if feed_rec && !rec_active && live_state != Some(CaptureState::Starting) {
                        Status::Error
                    } else if rec_active {
                        Status::Active
                    } else {
                        Status::Idle
                    };
                let rec_text_w = view::content_width(ui.ctx(), "\u{f044a} REC");
                let rec_rect = live_rect
                    .right_bottom()
                    .into_cell()
                    .translate(6, 0)
                    .extrude(grid::whole_cols(rec_text_w), -2);
                let rec_plate = plate_rect(rec_rect, tip, air, overlap);
                let (rec_plate_color, rec_text_color) =
                    plate_colors(rec_status, PLATE_REC_IDLE, PLATE_REC);

                // RACE и счётчик — один контрол: кликабельны плашка и
                // счётчик. Ширина плашки — по слову RACE (счётчик вне её).
                let race_active = self.race.active();
                let race_text_w = view::content_width(ui.ctx(), "\u{f140b} RACE");
                let race_rect = rec_rect
                    .right_bottom()
                    .into_cell()
                    .translate(6, 0)
                    .extrude(grid::whole_cols(race_text_w), -2);
                let race_plate = plate_rect(race_rect, tip, air, overlap);
                let (race_plate_color, race_text_color) = plate_colors(
                    if race_active {
                        Status::Active
                    } else {
                        Status::Idle
                    },
                    PLATE_RACE_IDLE,
                    PLATE_RACE,
                );
                // Мигание привязано к началу времени гонки (фазы по
                // 500 мс назад от него): последняя фаза ПОГАС кончается
                // ровно на нём — дальше счётчик ГОРИТ И ТИКАЕТ. ПОГАС —
                // пригашение на 80% (яркость 20%), не исчезновение.
                let blink_off = matches!(
                    self.race,
                    RaceState::Countdown { start, delay }
                        if race_time_start(start, delay)
                            .saturating_duration_since(Instant::now())
                            .as_millis() as u64
                            % 1000
                            >= 500
                );
                let race_timer = race_text(&self.race, Instant::now());
                let timer_pos = egui::pos2(
                    race_plate.right() + grid::cell_x(2),
                    race_rect.left_bottom().y,
                );
                // Плашки — справа налево: правая уходит под левую.
                view::arrow_plate(ui, race_plate, race_plate_color);
                view::arrow_plate(ui, rec_plate, rec_plate_color);
                view::arrow_plate(ui, live_plate, live_plate_color);

                let live_id = ui.make_persistent_id("status_live");
                // Клик-бласть — плашка без наконечника: уголок не кликабелен.
                let live_hit = live_plate.with_max_x(live_plate.right() - tip);
                let live_response = view::BigButton::new("\u{25b6} LIVE", live_text_color)
                    .show(ui, live_rect, live_id)
                    .union(
                        ui.interact(live_hit, live_id.with("hit"), egui::Sense::click())
                            .on_hover_cursor(egui::CursorIcon::PointingHand),
                    );
                if live_response.clicked() {
                    action = Action::ToggleLive;
                }
                // Время сессии — под плашкой, по левому краю текста (на 3
                // колонки правее края плашки: иконка + пробел), цветом
                // подложки.
                if !live_timer.is_empty() {
                    ui.painter().text(
                        egui::pos2(live_rect.left() + grid::cell_x(3), live_plate.bottom()),
                        egui::Align2::LEFT_TOP,
                        &live_timer,
                        style::FONT_REGULAR,
                        live_plate_color,
                    );
                }

                let rec_id = ui.make_persistent_id("status_rec");
                // Клик-бласть — видимое тело плашки: без наконечника
                // справа и без левого хвоста под наконечником соседа
                // (начинается от левого воздуха).
                let rec_hit = rec_plate
                    .with_min_x(rec_rect.left() - air)
                    .with_max_x(rec_plate.right() - tip);
                let rec_response = view::BigButton::new("\u{f044a} REC", rec_text_color)
                    .show(ui, rec_rect, rec_id)
                    .union(
                        ui.interact(rec_hit, rec_id.with("hit"), egui::Sense::click())
                            .on_hover_cursor(egui::CursorIcon::PointingHand),
                    );
                if rec_response.clicked() {
                    action = Action::ToggleRec;
                }
                if !rec_timer.is_empty() {
                    ui.painter().text(
                        egui::pos2(rec_rect.left() + grid::cell_x(3), rec_plate.bottom()),
                        egui::Align2::LEFT_TOP,
                        &rec_timer,
                        style::FONT_REGULAR,
                        rec_plate_color,
                    );
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
                    let race_hint = match self.race {
                        RaceState::Running { .. } => {
                            let left = self
                                .rules
                                .duration
                                .saturating_sub(self.race.elapsed(Instant::now()));
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

                // Клик-бласть — видимое тело плашки (без наконечника и без
                // левого хвоста под наконечником соседа) плюс счётчик.
                let race_body = race_plate
                    .with_min_x(race_rect.left() - air)
                    .with_max_x(race_plate.right() - tip);
                let race_timer_hit = egui::Rect::from_min_max(
                    egui::pos2(timer_pos.x, race_plate.top()),
                    egui::pos2(
                        timer_pos.x
                            + super::measure_text(ui.ctx(), style::FONT_REGULAR_X2, &race_timer).x,
                        race_plate.bottom(),
                    ),
                );
                let race_id = ui.make_persistent_id("status_race");
                let race_response = view::BigButton::new("\u{f140b} RACE", race_text_color)
                    .show(ui, race_rect, race_id)
                    .union(
                        ui.interact(race_body, race_id.with("hit"), egui::Sense::click())
                            .on_hover_cursor(egui::CursorIcon::PointingHand),
                    )
                    .union(
                        ui.interact(race_timer_hit, race_id.with("timer"), egui::Sense::click())
                            .on_hover_cursor(egui::CursorIcon::PointingHand),
                    );
                if race_response.clicked() {
                    action = Action::ToggleRace;
                }
                // Цвет счётчика: бежит белым; фаза ПОГАС мигания и нули
                // вне отсчёта (покой, обнулённый финиш) — 20% белого.
                let zeros_dim = matches!(self.race, RaceState::Idle)
                    || matches!(&self.race, RaceState::Finished { elapsed } if elapsed.is_zero());
                let timer_color = if blink_off || zeros_dim {
                    egui::Color32::WHITE.gamma_multiply(0.2)
                } else {
                    egui::Color32::WHITE
                };
                ui.painter().text(
                    timer_pos,
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
            Action::ToggleLive => self.toggle_live(ui.ctx()),
            Action::ToggleRec => self.toggle_rec(ui.ctx()),
            Action::ToggleRace => self.toggle_race(ui.ctx()),
            Action::None => {}
        }
    }

    // --- Эфир (LIVE/REC) -------------------------------------------------

    fn toggle_live(&mut self, ctx: &egui::Context) {
        // Любое нажатие LIVE/REC замирает время гонки.
        self.stop_race();
        match self.feed {
            FeedState::Off => {
                self.start_captures(ctx);
                self.screen_awake();
                self.feed = FeedState::Live {
                    since: Instant::now(),
                };
            }
            FeedState::Live { .. } | FeedState::Rec { .. } => {
                self.webcams.clear();
                log::info!("webcams stopped");
                self.awake = None;
                self.feed = FeedState::Off;
            }
        }
    }

    fn toggle_rec(&mut self, ctx: &egui::Context) {
        self.stop_race();
        match self.feed {
            FeedState::Off | FeedState::Live { .. } => self.ensure_recording(ctx),
            FeedState::Rec { live_since, .. } => {
                for webcam in self.webcams.values() {
                    webcam.stop_recording();
                }
                self.feed = FeedState::Live { since: live_since };
            }
        }
    }

    /// REC-on: с выключенного — захват + запись (+ inhibit), с LIVE —
    /// только запись. Уже пишем — no-op.
    fn ensure_recording(&mut self, ctx: &egui::Context) {
        match self.feed {
            FeedState::Off => {
                self.start_captures(ctx);
                self.start_recording_all();
                self.screen_awake();
                let now = Instant::now();
                self.feed = FeedState::Rec {
                    live_since: now,
                    rec_since: now,
                };
            }
            FeedState::Live { since } => {
                self.start_recording_all();
                self.feed = FeedState::Rec {
                    live_since: since,
                    rec_since: Instant::now(),
                };
            }
            FeedState::Rec { .. } => {}
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

    // --- Гонка (RACE) ----------------------------------------------------

    /// Старт/отмена гонки. Старт: RACE «нажимает» REC (с выключенного —
    /// транзитивно захват, LIVE); PRE_BEEP_SECS бипов с периодом 1 с,
    /// затем интервал из start_signal_delay, затем гудок. Отмена: звуки
    /// обрываем, время замирает.
    fn toggle_race(&mut self, ctx: &egui::Context) {
        match self.race {
            RaceState::Idle | RaceState::Finished { .. } => {
                self.ensure_recording(ctx);
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
                self.race = RaceState::Countdown {
                    start: Instant::now(),
                    delay,
                };
            }
            RaceState::Countdown { .. } | RaceState::Running { .. } => self.stop_race(),
        }
    }

    /// Отжать гонку (кнопкой RACE или транзитивно REC/LIVE). До начала
    /// времени гонки: звуки отменяем, счётчик сбрасываем в 0. После:
    /// финишный гудок, время замирает и остаётся гореть кислотно-жёлтым,
    /// кнопка серая. Из Idle/Finished — no-op.
    fn stop_race(&mut self) {
        match self.race {
            RaceState::Countdown { .. } => {
                self.audio.cancel();
                self.race = RaceState::Finished {
                    elapsed: Duration::ZERO,
                };
            }
            RaceState::Running { .. } => {
                self.audio.play(BUZZER);
                self.race = RaceState::Finished {
                    elapsed: self.race.elapsed(Instant::now()),
                };
            }
            _ => {}
        }
    }

    /// Перевод часов гонки: конец отсчёта (звук гудка уже в программе
    /// отсчёта — здесь только переход состояния), предупреждение о лимите,
    /// финиш по лимиту.
    fn race_tick(&mut self) {
        let now = Instant::now();
        match self.race {
            RaceState::Countdown { start, delay } if now >= race_time_start(start, delay) => {
                self.race = RaceState::Running {
                    start: race_time_start(start, delay),
                    warned: false,
                };
            }
            RaceState::Running {
                start,
                ref mut warned,
            } => {
                let elapsed = now.saturating_duration_since(start);
                let limit = self.rules.duration;
                if !*warned && limit > BUZZER.duration() && elapsed >= limit - BUZZER.duration() {
                    self.audio.play(BUZZER);
                    *warned = true;
                }
                if elapsed >= limit {
                    self.race = RaceState::Finished { elapsed: limit };
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
        if !self.race.active() {
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

/// Состояние потока: Rec возможен только внутри Live (REC-on стартует
/// захват и запись; LIVE-off гасит всё; REC тогглится независимо внутри
/// захвата). В вариантах — момент старта: таймеры кнопок читают его
/// отсюда, отдельных полей не требуется.
#[derive(Clone, Copy)]
enum FeedState {
    Off,
    Live {
        since: Instant,
    },
    Rec {
        live_since: Instant,
        rec_since: Instant,
    },
}

impl FeedState {
    /// Момент старта эфира (таймер LIVE); в Rec эфир не прерывался.
    fn live_since(&self) -> Option<Instant> {
        match self {
            FeedState::Off => None,
            FeedState::Live { since } => Some(*since),
            FeedState::Rec { live_since, .. } => Some(*live_since),
        }
    }

    /// Момент старта записи (таймер REC).
    fn rec_since(&self) -> Option<Instant> {
        match self {
            FeedState::Rec { rec_since, .. } => Some(*rec_since),
            _ => None,
        }
    }
}

/// Гонка: отсчёт → таймер → финиш.
enum RaceState {
    /// Кнопка серая, счётчик на нулях.
    Idle,
    /// start — нажатие RACE, delay — длительность отсчёта
    /// (бипы + интервал из RaceRules.start_signal_delay).
    Countdown { start: Instant, delay: Duration },
    /// start — старт таймера: конец гудка; пока гудок не
    /// отыграл, elapsed == 0. warned — предупреждающий гудок до лимита
    /// уже сыгран (один раз на прогон).
    Running { start: Instant, warned: bool },
    /// Время замерло (лимит или отмена), кнопка серая.
    Finished { elapsed: Duration },
}

impl RaceState {
    /// Текущее время гонки; у Countdown таймер ещё не шёл — 0.
    fn elapsed(&self, now: Instant) -> Duration {
        match self {
            RaceState::Idle | RaceState::Countdown { .. } => Duration::ZERO,
            RaceState::Running { start, .. } => now.saturating_duration_since(*start),
            RaceState::Finished { elapsed } => *elapsed,
        }
    }

    /// Активная фаза: кнопка кислотно-жёлтая.
    fn active(&self) -> bool {
        matches!(
            self,
            RaceState::Countdown { .. } | RaceState::Running { .. }
        )
    }
}

/// Звуки гонки: live их и проигрывает. Бинари — в `assets`.
const STAGE: audio::Sound = audio::Sound::from_bytes(crate::assets::STAGE_WAV);
const BUZZER: audio::Sound = audio::Sound::from_bytes(crate::assets::BUZZER_WAV);

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

/// Плашка вокруг текстового rect: воздух по колонке с каждой стороны,
/// наконечник справа, левый хвост — под наконечник соседа (overlap).
fn plate_rect(text: egui::Rect, tip: f32, air: f32, overlap: f32) -> egui::Rect {
    text.with_min_x(text.left() - overlap - air)
        .with_max_x(text.right() + air + tip)
}

/// (подложка, текст) кнопки-стрелки: в покое пригашенный оттенок, в
/// активе насыщенный; текст всегда чёрный.
fn plate_colors(
    status: Status,
    idle: egui::Color32,
    active: egui::Color32,
) -> (egui::Color32, egui::Color32) {
    match status {
        Status::Idle => (idle, egui::Color32::BLACK),
        _ => (active, egui::Color32::BLACK),
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
/// после гудка — время гонки бежит вверх, в покое и на финише — как есть.
fn race_text(state: &RaceState, now: Instant) -> String {
    let total = state.elapsed(now).as_millis() as u64;
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
}
