use super::*;
use crate::config::camera::Camera;
use crate::config::{pilot::Pilot, setup::Setup};
use crate::driver::CameraState;
use std::collections::HashMap;

const MAX_PADS: usize = 4;
const GRID_WIDTH: isize = 160;
const LEFT_MARGIN: isize = 2;
const RIGHT_MARGIN: isize = 2;
// Вьюфайндер фиксированный, высота задаёт ширину. Ячейки 8x16 pt,
// поэтому физический 4:3 — это 32x12 клеток (256x192 pt).
const VIEWFINDER_ROWS: isize = 12;
const VIEWFINDER_COLS: isize = VIEWFINDER_ROWS * 8 / 3; // 32

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
/// захват и запись; CAM-off гасит всё; REC тогглится независимо внутри
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
    active_cameras: HashMap<String, Camera>,
    // Показываемый fps: считаем на UI по забранным кадрам, только по
    // первой камере (как и остальные цифры статуса).
    shown_frames: u32,
    shown_window: std::time::Instant,
    shown_fps: f32,
}

impl Into<State> for Live {
    fn into(self) -> State {
        State::Live(self)
    }
}

impl Live {
    pub fn new(setup: Setup, assignments: HashMap<String, Pilot>) -> Self {
        // Cameras feeding viewports of occupied pads (the screen shows at
        // most MAX_PADS columns); built once — assignments are fixed for the
        // lifetime of this screen.
        let active_cameras: HashMap<String, Camera> = setup
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
            shown_frames: 0,
            shown_window: std::time::Instant::now(),
            // 0.0 = замера ещё не было, UI покажет "-- fps".
            shown_fps: 0.0,
        }
    }

    pub fn update(&mut self, ui: &mut egui::Ui, navigator: &mut Navigator) {
        ui.ctx().viewport_id();
        enum Action {
            ToggleCam,
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
            let camera_state = webcam.camera_state();
            let (texture, new_frame) = webcam.update(ctx);
            if new_frame && Some(id) == primary_id.as_ref() {
                self.shown_frames += 1;
            }
            // Поток мёртв (камера не найдена, отвалилась) — testcard вместо
            // замершей последней текстуры: состояние не отличить по ней.
            // Starting тоже testcard: кадров ещё нет.
            let state = if camera_state == CameraState::Live {
                match texture {
                    Some(tex) => viewfinder::ViewfinderContents::Texture(tex.id()),
                    None => viewfinder::ViewfinderContents::Pending,
                }
            } else {
                viewfinder::ViewfinderContents::Pending
            };
            contents.insert(id.clone(), state);
        }
        // Пассивный замер: окно 0.5 с, считается только по поступающим
        // кадрам; пустое окно последнее значение не затирает.
        if self.shown_window.elapsed() >= std::time::Duration::from_millis(500) {
            if self.shown_frames > 0 {
                self.shown_fps =
                    self.shown_frames as f32 / self.shown_window.elapsed().as_secs_f32();
            }
            self.shown_frames = 0;
            self.shown_window = std::time::Instant::now();
        }

        view::Letterbox::new(grid::cell(160, 45))
            .max_virtual_height(grid::cell_y(50))
            .outline(true)
            .show(ui, |ui| {
                let pads: Vec<_> = self.setup.pads.iter().take(MAX_PADS).collect();
                let columns = pads.len() as isize;
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

                if columns > 0 {
                    let border_width = grid::cell_x(1) / 2.0;
                    for (i, (pad_id, pad)) in pads.into_iter().enumerate() {
                        // Вьюфайндер по горизонтали в середине зоны
                        // (с точностью до клетки).
                        let (zone_start, zone_width) = column_geometry[i];
                        let col = zone_start + (zone_width - VIEWFINDER_COLS) / 2;
                        let color = pad.color.to_color32();
                        let viewfinder_rect =
                            grid::cell(col, 3).extrude(VIEWFINDER_COLS, VIEWFINDER_ROWS);
                        ui.painter().rect_stroke(
                            viewfinder_rect,
                            0.0,
                            egui::Stroke::new(border_width, color),
                            egui::StrokeKind::Outside,
                        );
                        // Off: no webcam feeds this viewport; Pending: webcam
                        // runs but no frame decoded yet; Texture: live feed.
                        let webcam_contents = contents
                            .get(&pad.fpv.camera_id)
                            .copied()
                            .unwrap_or(viewfinder::ViewfinderContents::Off);
                        viewfinder::Viewfinder::new(pad.fpv.viewport.to_rect()).show(
                            ui,
                            webcam_contents,
                            viewfinder_rect,
                        );

                        let label_len = pad.label.len() as isize;
                        let label_rect = grid::cell(col, 1)
                            .extrude(label_len * 2, 2)
                            .expand2(egui::vec2(grid::cell_x(1) / 2.0, 0.0));
                        ui.painter().rect_filled(label_rect, 0.0, color);
                        ui.painter().text(
                            label_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            &pad.label,
                            style::FONT_REGULAR_X2,
                            egui::Color32::WHITE,
                        );

                        let pilot_name = self
                            .assignments
                            .get(pad_id)
                            .map_or("", |pilot| pilot.name.as_str());
                        let pilot_rect = grid::cell(col + label_len * 2 + 2, 1).extrude(0, 1);
                        ui.painter().text(
                            pilot_rect.center(),
                            egui::Align2::LEFT_CENTER,
                            pilot_name,
                            style::FONT_REGULAR,
                            egui::Color32::WHITE,
                        );
                        ui.painter().rect(
                            grid::cell(col, VIEWFINDER_ROWS + 4).extrude(VIEWFINDER_COLS, 4),
                            0.0,
                            egui::Color32::TRANSPARENT,
                            egui::Stroke::new(1.0, egui::Color32::from_gray(48)),
                            egui::StrokeKind::Inside,
                        );
                        guidelines::dashed_rect(
                            ui,
                            grid::cell(col, 14 + VIEWFINDER_ROWS)
                                .extrude(VIEWFINDER_COLS, bottom_right.row - 18 - VIEWFINDER_ROWS),
                        );

                        let text_rect = grid::cell(col, 14 + VIEWFINDER_ROWS).to_pos2();
                        ui.painter().text(
                            text_rect,
                            egui::Align2::LEFT_TOP,
                            "1) 4:56.789 \u{f0537} \u{f00d} \u{f00d} \u{ea72} \u{f4aa}",
                            style::FONT_REGULAR,
                            egui::Color32::WHITE,
                        );
                    }
                }
                // CAM и REC — независимые виджеты: клик по CAM тогглит
                // захват, клик по REC — запись. Подпись под кнопкой: error
                // при отвале, измеренный fps после замера, "-- fps" до
                // замера, заявленный fps в покое. Кликабельна вся область.
                let status_row = 9 + VIEWFINDER_ROWS;
                let right_edge = GRID_WIDTH - RIGHT_MARGIN;
                let cam_state = self.webcams.values().next().map(|w| w.camera_state());
                let cam_active = cam_state == Some(CameraState::Live);
                let cam_dead = cam_state == Some(CameraState::Dead);
                // Starting: поток жив, камера инициализируется — для статуса
                // это «active», а не отвал (error рисуем только по Dead).
                let starting = cam_state == Some(CameraState::Starting);
                let cam_show = cam_active || (self.feed != FeedState::Off && !cam_dead);
                let rec_active = self.webcams.values().any(|w| w.record_state().ok);
                let cfg_fps = self
                    .active_cameras
                    .values()
                    .next()
                    .map(|camera| camera.frame_rate.0);
                let rec_fps = self.webcams.values().next().map(|w| w.record_state().fps);

                let rec_rect = grid::cell(right_edge, status_row).extrude(-6, 3);
                let rec_response = ui
                    .interact(
                        rec_rect,
                        ui.make_persistent_id("status_rec"),
                        egui::Sense::click(),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if rec_response.clicked() {
                    action = Action::ToggleRec;
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
                        format!("{:.0} fps", rec_fps.unwrap_or_default())
                    } else {
                        "-- fps".to_owned()
                    }
                } else if rec_active && rec_fps.unwrap_or_default() > 0.0 {
                    format!("{:.0} fps", rec_fps.unwrap_or_default())
                } else {
                    format!("{} fps", cfg_fps.unwrap_or_default())
                };
                ui.painter().text(
                    rec_rect.left_center(),
                    egui::Align2::LEFT_CENTER,
                    "REC",
                    style::FONT_REGULAR_X2,
                    if rec_active {
                        egui::Color32::RED
                    } else {
                        egui::Color32::DARK_GRAY
                    },
                );
                ui.painter().text(
                    grid::cell(right_edge, status_row + 2)
                        .extrude(-6, 1)
                        .center(),
                    egui::Align2::CENTER_CENTER,
                    rec_text,
                    style::FONT_REGULAR,
                    rec_color,
                );

                let cam_rect = grid::cell(right_edge, status_row)
                    .translate(-8, 0)
                    .extrude(-6, 3);
                let cam_response = ui
                    .interact(
                        cam_rect,
                        ui.make_persistent_id("status_cam"),
                        egui::Sense::click(),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if cam_response.clicked() {
                    action = Action::ToggleCam;
                }
                let (cam_text, cam_color) = if self.feed != FeedState::Off && cam_dead {
                    ("error".to_owned(), egui::Color32::WHITE)
                } else if cam_show {
                    let text = if self.shown_fps > 0.0 {
                        format!("{:.0} fps", self.shown_fps)
                    } else {
                        "-- fps".to_owned()
                    };
                    (text, egui::Color32::WHITE)
                } else {
                    (
                        format!("{} fps", cfg_fps.unwrap_or_default()),
                        egui::Color32::DARK_GRAY,
                    )
                };
                ui.painter().text(
                    cam_rect.left_center(),
                    egui::Align2::LEFT_CENTER,
                    "CAM",
                    style::FONT_REGULAR_X2,
                    if cam_show {
                        egui::Color32::WHITE
                    } else {
                        egui::Color32::DARK_GRAY
                    },
                );
                ui.painter().text(
                    grid::cell(right_edge, status_row + 2)
                        .translate(-8, 0)
                        .extrude(-6, 1)
                        .center(),
                    egui::Align2::CENTER_CENTER,
                    cam_text,
                    style::FONT_REGULAR,
                    cam_color,
                );

                guidelines::dashed_line(
                    ui,
                    egui::Direction::RightToLeft,
                    grid::cell_y(4 + VIEWFINDER_ROWS),
                );
                guidelines::dashed_line(
                    ui,
                    egui::Direction::RightToLeft,
                    grid::cell_y(9 + VIEWFINDER_ROWS),
                );
                guidelines::dashed_line(
                    ui,
                    egui::Direction::RightToLeft,
                    grid::cell_y(13 + VIEWFINDER_ROWS),
                );

                let text_rect = grid::cell(11, bottom_right.row)
                    .translate(0, -1)
                    .extrude(23, -1);
                self.clock.show(ui, text_rect);
                {
                    let button = egui::Button::new(
                        egui::RichText::new(" Выход ⌘W ")
                            .font(style::FONT_REGULAR)
                            .color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::BLUE)
                    .stroke(egui::Stroke::NONE)
                    .frame(false);

                    let button_rect = bottom_right.translate(-2, -1).extrude(-10, -1);

                    if ui.put(button_rect, button).clicked() {
                        action = Action::GotoMenu;
                    }
                }
            });

        match action {
            Action::GotoMenu => navigator.goto(menu::Menu),
            Action::ToggleCam => self.toggle_cam(ui.ctx()),
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
            let webcam = crate::driver::webcam::Webcam::start(id, camera.clone(), move || {
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

    // Сброс окна замера показа: fps считаем от старта захвата, иначе
    // первое окно тянет elapsed с момента создания экрана и даёт
    // мгновенный "0 fps". Пока замера нет — "-- fps".
    fn reset_shown_fps(&mut self) {
        self.shown_frames = 0;
        self.shown_window = std::time::Instant::now();
        self.shown_fps = 0.0;
    }

    fn toggle_cam(&mut self, ctx: &egui::Context) {
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
