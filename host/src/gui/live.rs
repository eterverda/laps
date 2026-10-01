use super::*;
use crate::config::camera::CameraConfig;
use crate::config::setup::Setup;
use crate::driver::webcam::CaptureState;
use crate::gui::grid::IntoCell;
use crate::model::pilot::Pilot;
use std::collections::HashMap;

const MAX_PADS: usize = 4;
const GRID_WIDTH: isize = 160;
const LEFT_MARGIN: isize = 4;
const RIGHT_MARGIN: isize = 4;
const TOP_BAR_ROWS: isize = 2;
const BOTTOM_BAR_ROWS: isize = 2;

fn viewfinder_rows(count: usize) -> isize {
    match count {
        1 | 2 => 21,
        3 => 15,
        _ => 12,
    }
}

fn viewfinder_cols(count: usize) -> isize {
    viewfinder_rows(count) * 8 / 3
}

/// Фон плашки заголовка — colorSecondaryDark из laps-bar.
const TITLE_BG: egui::Color32 = egui::Color32::from_rgb(0x5B, 0x7F, 0xA9);

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

/// Состояние потока: Rec возможен только внутри Live (REC-on стартует
/// захват и запись; LIVE-off гасит всё; REC тогглится независимо внутри
/// захвата).
#[derive(Clone, Copy, PartialEq)]
enum FeedState {
    Off,
    Live,
    Rec,
}

pub struct Live {
    res: Option<Res>,
    webcams: HashMap<String, crate::driver::webcam::Webcam>,
    feed: FeedState,
    clock: clock::Clock,
    setup: Setup,
    assignments: HashMap<String, Pilot>,
    active_cameras: HashMap<String, CameraConfig>,
    // Показываемый fps: считаем на UI по забранным кадрам, только по
    // первой камере (как и остальные цифры статуса).
    shown: crate::driver::webcam::fps::FpsCounter,
    /// Последний измеренный fps; 0.0 = замера ещё не было, UI покажет
    /// "-- fps". Пауза кадров значение не затирает.
    shown_fps: f32,
}

impl From<Box<Live>> for State {
    fn from(val: Box<Live>) -> Self {
        State::Live(val)
    }
}

impl Live {
    pub fn new(setup: Setup, assignments: HashMap<String, Pilot>) -> Self {
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
            res: None,
            webcams: HashMap::new(),
            feed: FeedState::Off,
            clock: clock::Clock::new(),
            setup,
            assignments,
            active_cameras,
            shown: crate::driver::webcam::fps::FpsCounter::default(),
            shown_fps: 0.0,
        }
    }

    pub fn update(&mut self, ui: &mut egui::Ui, navigator: &mut Navigator) {
        ui.ctx().viewport_id();
        enum Action {
            ToggleLive,
            ToggleRec,
            GotoMenu,
            None,
        }
        let mut action = Action::None;

        if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::W)) {
            action = Action::GotoMenu;
        }

        let ctx = ui.ctx();
        self.res.get_or_insert_with(|| Res::new(ctx));

        // Per-camera viewfinder state: Texture once a frame is decoded,
        // Pending while the webcam runs but produced nothing yet. A camera
        // absent from this map means Off for its viewports.
        let mut contents: HashMap<String, viewfinder::ViewfinderContents> = HashMap::new();
        let primary_id = self.active_cameras.keys().next().cloned();
        for (id, webcam) in self.webcams.iter_mut() {
            let capture_state = webcam.capture_state();
            let (texture, new_frame) = webcam.update(ctx);
            if new_frame && Some(id) == primary_id.as_ref() {
                self.shown.on_frame();
            }
            // Поток мёртв (камера не найдена, отвалилась) — testcard вместо
            // замершей последней текстуры: состояние не отличить по ней.
            // Starting тоже testcard: кадров ещё нет.
            let state = if capture_state == CaptureState::Live {
                match texture {
                    Some(tex) => viewfinder::ViewfinderContents::Texture(tex.id()),
                    None => viewfinder::ViewfinderContents::Pending,
                }
            } else {
                viewfinder::ViewfinderContents::Pending
            };
            contents.insert(id.clone(), state);
        }
        // Пассивный замер: считается только по поступающим кадрам,
        // пустое окно последнее значение не затирает.
        if let Some(fps) = self.shown.fps() {
            self.shown_fps = fps as f32;
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

                let bottom_right = grid::cell_at(ui.max_rect().max);

                let title_rect = grid::cell(0, 0).translate(1, 0).extrude(9, 2);
                ui.painter().text(
                    title_rect.right_top().into_cell().extrude(2, 2).left_top(),
                    egui::Align2::LEFT_TOP,
                    "\u{e0bc}",
                    style::FONT_REGULAR_X2,
                    TITLE_BG,
                );
                ui.painter()
                    .rect_filled(title_rect.with_min_x(0.0), 0.0, TITLE_BG);
                ui.painter().text(
                    title_rect.left_top(),
                    egui::Align2::LEFT_TOP,
                    "LAPS",
                    style::FONT_REGULAR_X2,
                    egui::Color32::BLACK,
                );

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
                            egui::Stroke::new(1.0, egui::Color32::from_gray(48)),
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
                // LIVE и REC — независимые виджеты: клик по LIVE тогглит
                // захват, клик по REC — запись. Мелкий текст справа от
                // названия: error при отвале, измеренный fps после замера,
                // "-- fps" до замера, заявленный fps в покое. Кликабельна
                // вся область.
                let live_state = self.webcams.values().next().map(|w| w.capture_state());
                let live_active = live_state == Some(CaptureState::Live);
                let live_dead = live_state == Some(CaptureState::Dead);
                // Starting: поток жив, камера инициализируется — для статуса
                // это «active», а не отвал (error рисуем только по Dead).
                let starting = live_state == Some(CaptureState::Starting);
                let live_show = live_active || (self.feed != FeedState::Off && !live_dead);
                let rec_active = self.webcams.values().any(|w| w.record_state().ok);
                let cfg_fps = self
                    .active_cameras
                    .values()
                    .next()
                    .map(|camera| camera.frame_rate);
                let dvr_fps = self
                    .active_cameras
                    .values()
                    .next()
                    .map(|camera| camera.dvr.frame_rate.unwrap_or(camera.frame_rate));
                let rec_fps = self.webcams.values().next().map(|w| w.record_state().fps);

                let (live_text, live_color) = if self.feed != FeedState::Off && live_dead {
                    ("error".to_owned(), egui::Color32::WHITE)
                } else if live_show {
                    let text = if self.shown_fps > 0.0 {
                        format!("{:.0}fps", self.shown_fps)
                    } else {
                        "--fps".to_owned()
                    };
                    (text, egui::Color32::WHITE)
                } else {
                    (
                        cfg_fps.map(|f| f.to_string()).unwrap_or_default(),
                        egui::Color32::DARK_GRAY,
                    )
                };
                let live_rect = grid::cell(1, bottom_right.row).translate(1, -1).extrude(
                    grid::whole_cols(view::content_width(ui.ctx(), " LIVE", &live_text)),
                    -2,
                );
                let live_response = view::BigButton::new(" LIVE", "", &live_text, live_color)
                    .show(ui, live_rect, ui.make_persistent_id("status_live"));
                if live_response.clicked() {
                    action = Action::ToggleLive;
                }

                let rec_color = if rec_active {
                    egui::Color32::RED
                } else {
                    egui::Color32::DARK_GRAY
                };
                let rec_text = if self.feed == FeedState::Rec {
                    if !rec_active && !starting {
                        "error".to_owned()
                    } else if rec_fps.unwrap_or_default() > 0.0 {
                        format!("{:.0}fps", rec_fps.unwrap())
                    } else {
                        "--fps".to_owned()
                    }
                } else if rec_active && rec_fps.unwrap_or_default() > 0.0 {
                    format!("{:.0}fps", rec_fps.unwrap())
                } else {
                    dvr_fps.map(|f| f.to_string()).unwrap_or_default()
                };
                let rec_rect = live_rect
                    .right_bottom()
                    .into_cell()
                    .translate(2, 0)
                    .extrude(
                        grid::whole_cols(view::content_width(ui.ctx(), "󰑊 REC", &rec_text)),
                        -2,
                    );
                let rec_response = view::BigButton::new("󰑊 REC", "", &rec_text, rec_color).show(
                    ui,
                    rec_rect,
                    ui.make_persistent_id("status_rec"),
                );
                if rec_response.clicked() {
                    action = Action::ToggleRec;
                }

                let race_rect = rec_rect.right_bottom().into_cell().translate(2, 0).extrude(
                    grid::whole_cols(view::content_width(
                        ui.ctx(),
                        "\u{f140b} RACE 00:00.000",
                        "",
                    )),
                    -2,
                );
                view::BigButton::new("\u{f140b} RACE 00:00.000", "", "", egui::Color32::DARK_GRAY)
                    .show(ui, race_rect, ui.make_persistent_id("status_race"));

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

                let text_rect = grid::cell(bottom_right.col, bottom_right.row)
                    .translate(0, -1)
                    .translate(-1, 0)
                    .extrude(-25, -1);
                self.clock.show(ui, text_rect);
            });

        match action {
            Action::GotoMenu => navigator.goto(menu::Menu),
            Action::ToggleLive => self.toggle_live(ui.ctx()),
            Action::ToggleRec => self.toggle_rec(ui.ctx()),
            Action::None => {}
        }
    }

    fn start_captures(&mut self, ctx: &egui::Context) {
        for (id, camera) in &self.active_cameras {
            let ctx = ctx.clone();
            // A 1ms delay instead of an immediate repaint: egui renders twice
            // per `request_repaint` (outstanding = 1), and the second pass
            // always finds an empty slot. A tiny delay gives a single pass
            // per camera frame.
            let webcam = crate::driver::webcam::Webcam::start(camera.clone(), move || {
                ctx.request_repaint_after(std::time::Duration::from_millis(1));
            });
            self.webcams.insert(id.clone(), webcam);
        }
        log::info!("webcams starting: {}", self.webcams.len());
    }

    fn start_recording_all(&mut self) {
        for (id, camera) in &self.active_cameras {
            if let Some(webcam) = self.webcams.get(id) {
                webcam.start_recording(crate::driver::dvr::Options {
                    dir: crate::driver::dvr::CAPTURES_DIR.into(),
                    camera_id: id.clone(),
                    camera: camera.clone(),
                });
            }
        }
    }

    // Сброс замера показа: fps считаем от старта захвата, иначе первые
    // штампы тянут интервал с момента создания экрана. Пока замера нет —
    // "-- fps".
    fn reset_shown_fps(&mut self) {
        self.shown.reset();
        self.shown_fps = 0.0;
    }

    fn toggle_live(&mut self, ctx: &egui::Context) {
        match self.feed {
            FeedState::Off => {
                self.start_captures(ctx);
                self.reset_shown_fps();
                self.feed = FeedState::Live;
            }
            FeedState::Live | FeedState::Rec => {
                self.webcams.clear();
                log::info!("webcams stopped");
                self.feed = FeedState::Off;
            }
        }
    }

    fn toggle_rec(&mut self, ctx: &egui::Context) {
        match self.feed {
            FeedState::Off => {
                self.start_captures(ctx);
                self.start_recording_all();
                self.reset_shown_fps();
                self.feed = FeedState::Rec;
            }
            FeedState::Live => {
                self.start_recording_all();
                self.feed = FeedState::Rec;
            }
            FeedState::Rec => {
                for webcam in self.webcams.values() {
                    webcam.stop_recording();
                }
                self.feed = FeedState::Live;
            }
        }
    }
}
