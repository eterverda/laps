use super::*;
use crate::config::camera::Camera;
use crate::config::{pilot::Pilot, setup::Setup};
use std::collections::HashMap;

const MAX_PADS: usize = 4;
const GRID_WIDTH: isize = 160;
const LEFT_MARGIN: isize = 2;
const COLUMN_GAP: isize = 6;

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
        // ctx.request_repaint(); // TODO concider remove (commented out to test frame-drop logging)

        self.res.get_or_insert_with(|| Res::new(ctx));

        // Per-camera viewfinder state: Texture once a frame is decoded,
        // Pending while the webcam runs but produced nothing yet. A camera
        // absent from this map means Off for its viewports.
        let mut contents: HashMap<String, viewfinder::ViewfinderContents> = HashMap::new();
        for (id, webcam) in self.webcams.iter_mut() {
            let state = match webcam.update(ctx) {
                Some(tex) => viewfinder::ViewfinderContents::Texture(tex.id()),
                None => viewfinder::ViewfinderContents::Pending,
            };
            contents.insert(id.clone(), state);
        }

        view::Letterbox::new(grid::cell(160, 45))
            .max_virtual_height(grid::cell_y(50))
            .outline(true)
            .show(ui, |ui| {
                if guidelines::ENABLED {
                    guidelines::marker_grid(ui, grid::cell(10, 5));

                    // Column separators, same geometry as the pad loop below.
                    let columns = self.setup.pads.len().min(MAX_PADS) as isize;
                    if columns > 0 {
                        let segment = (GRID_WIDTH - LEFT_MARGIN) / columns;
                        for i in 0..columns {
                            guidelines::dashed_line(
                                ui,
                                egui::Direction::TopDown,
                                grid::cell_x(LEFT_MARGIN + segment * i),
                            );
                        }
                    }
                }

                let bottom_right = grid::cell_at(ui.max_rect().max);

                const VIEWFINDER_ROWS: isize = 12;
                let pads: Vec<_> = self.setup.pads.iter().take(MAX_PADS).collect();
                let columns = pads.len() as isize;
                if columns > 0 {
                    // Columns split the 160-cell row between LEFT_MARGIN and
                    // the right edge.
                    let segment = (GRID_WIDTH - LEFT_MARGIN) / columns;
                    let viewfinder_width = segment - COLUMN_GAP;
                    let border_width = grid::cell_x(1) / 2.0;
                    for (i, (pad_id, pad)) in pads.into_iter().enumerate() {
                        let col = LEFT_MARGIN + COLUMN_GAP / 2 + segment * i as isize;
                        let color = pad.color.to_color32();
                        let viewfinder_rect =
                            grid::cell(col, 3).extrude(viewfinder_width, VIEWFINDER_ROWS);
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
                            .expand2(egui::vec2(grid::cell_x(1) / 2.0, 0.0))
                            .translate(egui::vec2(0.0, grid::cell_y(-1) / 2.0));
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

                        guidelines::dashed_rect(
                            ui,
                            grid::cell(col, 15 + VIEWFINDER_ROWS)
                                .extrude(viewfinder_width, bottom_right.row - 18 - VIEWFINDER_ROWS),
                        );
                    }
                }
                let text_rect = grid::cell(11, 15 + VIEWFINDER_ROWS).extrude(25, 1);
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
                    grid::cell_y(5 + VIEWFINDER_ROWS),
                );
                guidelines::dashed_line(
                    ui,
                    egui::Direction::RightToLeft,
                    grid::cell_y(9 + VIEWFINDER_ROWS),
                );
                guidelines::dashed_line(
                    ui,
                    egui::Direction::RightToLeft,
                    grid::cell_y(10 + VIEWFINDER_ROWS),
                );
                guidelines::dashed_line(
                    ui,
                    egui::Direction::RightToLeft,
                    grid::cell_y(11 + VIEWFINDER_ROWS),
                );
                guidelines::dashed_line(
                    ui,
                    egui::Direction::RightToLeft,
                    grid::cell_y(14 + VIEWFINDER_ROWS),
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

                {
                    let button = egui::Button::new(
                        egui::RichText::new(" Камера ")
                            .font(style::FONT_REGULAR)
                            .color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::BLUE)
                    .stroke(egui::Stroke::NONE)
                    .frame(false);

                    let button_rect = bottom_right
                        .translate(-2, -1)
                        .translate(-11, 0)
                        .extrude(-10, -1);

                    if ui.put(button_rect, button).clicked() {
                        action = Action::ToggleCamera;
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
            // A 1ms delay instead of an immediate repaint: egui renders twice
            // per `request_repaint` (outstanding = 1), and the second pass
            // always finds an empty slot. A tiny delay gives a single pass
            // per camera frame.
            let webcam = crate::driver::webcam::Webcam::start(camera, move || {
                ctx.request_repaint_after(std::time::Duration::from_millis(1));
            });
            if let Some(webcam) = webcam {
                self.webcams.insert(id.clone(), webcam);
            }
        }
        log::info!("webcams started: {}", self.webcams.len());
    }
}
