use super::*;
use crate::config::camera::Camera;
use crate::config::{pilot::Pilot, setup::Setup};
use std::collections::HashMap;

const MAX_PADS: usize = 4;
const GRID_WIDTH: isize = 160;
const LEFT_MARGIN: isize = 4;
const RIGHT_MARGIN: isize = 4;
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

pub struct Live {
    res: Option<Res>,
    webcams: HashMap<String, crate::driver::webcam::Webcam>,
    clock: clock::Clock,
    setup: Setup,
    assignments: HashMap<String, Pilot>,
    active_cameras: HashMap<String, Camera>,
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
            clock: clock::Clock::new(),
            setup,
            assignments,
            active_cameras,
        }
    }

    pub fn update(&mut self, ui: &mut egui::Ui, navigator: &mut Navigator) {
        ui.ctx().viewport_id();
        enum Action {
            ToggleCamera,
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
        for (id, webcam) in self.webcams.iter_mut() {
            // Поток мёртв (камера не найдена, отвалилась) — testcard вместо
            // замершей последней текстуры: состояние не отличить по ней.
            let state = if webcam.capture_state().ok {
                match webcam.update(ctx) {
                    Some(tex) => viewfinder::ViewfinderContents::Texture(tex.id()),
                    None => viewfinder::ViewfinderContents::Pending,
                }
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

                        let label_rect = grid::cell(col, 1)
                            .extrude(4, 2)
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
                        let pilot_rect = grid::cell(col + 6, 1).extrude(0, 1);
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
                            egui::Stroke::new(1.0, egui::Color32::GRAY),
                            egui::StrokeKind::Inside,
                        );
                        guidelines::dashed_rect(
                            ui,
                            grid::cell(col, 14 + VIEWFINDER_ROWS)
                                .extrude(VIEWFINDER_COLS, bottom_right.row - 18 - VIEWFINDER_ROWS),
                        );
                    }
                }
                // LIVE и REC — индикаторы одного общего состояния: клик по
                // любому включает/выключает трансляцию + запись вместе.
                // Активны: LIVE белый, REC красный; неактивны — серые.
                let status_row = 10 + VIEWFINDER_ROWS; // на строку выше прежнего
                let right_edge = GRID_WIDTH - RIGHT_MARGIN;
                let widget_w = 8; // 12 клеток текста X2 + по клетке с боков
                let live_active = self.webcams.values().any(|w| w.capture_state().ok);
                let rec_active = self.webcams.values().any(|w| w.record_state().ok);
                for (i, &(label, active_color, active)) in [
                    ("LIVE", egui::Color32::WHITE, live_active),
                    ("REC", egui::Color32::RED, rec_active),
                ]
                .iter()
                .enumerate()
                {
                    let col = right_edge - widget_w - (widget_w + 2) * (1 - i) as isize;
                    let rect = grid::cell(col, status_row).extrude(widget_w, 2);
                    let response = ui
                        .interact(
                            rect,
                            ui.make_persistent_id(format!("status_{label}")),
                            egui::Sense::click(),
                        )
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    if response.clicked() {
                        action = Action::ToggleCamera;
                    }
                    let color = if active {
                        active_color
                    } else {
                        egui::Color32::DARK_GRAY
                    };
                    ui.painter().text(
                        rect.left_center(),
                        egui::Align2::LEFT_CENTER,
                        label,
                        style::FONT_REGULAR_X2,
                        color,
                    );
                }

                let text_rect = grid::cell(LEFT_MARGIN + 3, 14 + VIEWFINDER_ROWS).extrude(25, 1);
                ui.painter().text(
                    text_rect.right_top(),
                    egui::Align2::RIGHT_TOP,
                    "01) 2:34:56.789 \u{f0537} \u{f00d} \u{f00d} \u{ea72} \u{f4aa}",
                    style::FONT_REGULAR,
                    egui::Color32::WHITE,
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
            Action::ToggleCamera => self.toggle_webcam(ui.ctx()),
            Action::None => {}
        }
    }

    fn toggle_webcam(&mut self, ctx: &egui::Context) {
        if !self.webcams.is_empty() {
            self.webcams.clear();
            log::info!("webcams stopped");
            return;
        }
        for (id, camera) in &self.active_cameras {
            let ctx = ctx.clone();
            let dvr_options = crate::driver::dvr::Options {
                dir: crate::driver::dvr::CAPTURES_DIR.into(),
                camera_id: id.clone(),
            };
            // A 1ms delay instead of an immediate repaint: egui renders twice
            // per `request_repaint` (outstanding = 1), and the second pass
            // always finds an empty slot. A tiny delay gives a single pass
            // per camera frame.
            let webcam =
                crate::driver::webcam::Webcam::start(camera.clone(), dvr_options, move || {
                    ctx.request_repaint_after(std::time::Duration::from_millis(1));
                });
            self.webcams.insert(id.clone(), webcam);
        }
        log::info!("webcams starting: {}", self.webcams.len());
    }
}
