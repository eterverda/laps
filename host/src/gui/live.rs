use crate::gui::viewfinder::ViewfinderContents;

use super::*;

const QUADRANT_UVS: [egui::Rect; 4] = [
    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(0.5, 0.5)),
    egui::Rect::from_min_max(egui::pos2(0.5, 0.0), egui::pos2(1.0, 0.5)),
    egui::Rect::from_min_max(egui::pos2(0.0, 0.5), egui::pos2(0.5, 1.0)),
    egui::Rect::from_min_max(egui::pos2(0.5, 0.5), egui::pos2(1.0, 1.0)),
];

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
    webcam: Option<crate::driver::webcam::Webcam>,
}

impl Into<State> for Live {
    fn into(self) -> State {
        State::Live(self)
    }
}

impl Live {
    pub fn new() -> Self {
        Self {
            res: None,
            webcam: None,
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
        let webcam_contents = match self.webcam {
            Some(ref mut webcam) => match webcam.update(ctx) {
                Some(tex) => viewfinder::ViewfinderContents::Texture(tex.id()),
                None => viewfinder::ViewfinderContents::Pending,
            },
            None => viewfinder::ViewfinderContents::Off,
        };

        view::Letterbox::new(grid::cell(160, 45))
            .max_virtual_height(grid::cell_y(50))
            .outline(true)
            .show(ui, |ui| {
                if guidelines::ENABLED {
                    guidelines::marker_grid(ui, grid::cell(10, 5));

                    let xs = [grid::cell_x(40), grid::cell_x(80), grid::cell_x(120)];
                    let ys = [grid::cell_y(2)];

                    for x in xs {
                        guidelines::dashed_line(ui, egui::Direction::TopDown, x);
                    }
                    for y in ys {
                        guidelines::dashed_line(ui, egui::Direction::LeftToRight, y);
                    }
                }

                let bottom_right = grid::cell_at(ui.max_rect().max);

                for (i, uv) in QUADRANT_UVS.into_iter().enumerate() {
                    let pilot_col = 40 * i as isize;
                    const VIEWFINDER_ROWS: isize = 12;
                    let viewfinder_rect = grid::cell(4, 2)
                        .translate(pilot_col, 1)
                        .extrude(32, VIEWFINDER_ROWS);
                    viewfinder::Viewfinder::new(uv).show(ui, webcam_contents, viewfinder_rect);
                    if guidelines::ENABLED {
                        if webcam_contents == ViewfinderContents::Off {
                            guidelines::dashed_rect(ui, viewfinder_rect);
                        }
                    }
                    guidelines::dashed_rect(
                        ui,
                        grid::cell(4, 4 + VIEWFINDER_ROWS)
                            .translate(pilot_col, 0)
                            .extrude(32, 4),
                    );
                    guidelines::dashed_rect(
                        ui,
                        grid::cell(4, 9 + VIEWFINDER_ROWS)
                            .translate(pilot_col, 0)
                            .extrude(32, 2),
                    );
                    guidelines::dashed_rect(
                        ui,
                        grid::cell(4, 12 + VIEWFINDER_ROWS)
                            .translate(pilot_col, 0)
                            .extrude(32, 2),
                    );
                    guidelines::dashed_rect(
                        ui,
                        grid::cell(4, 15 + VIEWFINDER_ROWS)
                            .translate(pilot_col, 0)
                            .extrude(32, bottom_right.row - 18 - VIEWFINDER_ROWS),
                    );
                }

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
        match self.webcam.take() {
            Some(_) => {
                log::info!("webcam stopped");
            }
            None => {
                let ctx = ctx.clone();
                self.webcam = crate::driver::webcam::Webcam::start(move || ctx.request_repaint());
                log::info!("webcam started");
            }
        }
    }
}
