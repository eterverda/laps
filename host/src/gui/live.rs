use super::*;

const FULL_UV: egui::Rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
const QUADRANT_UVS: [egui::Rect; 4] = [
    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(0.5, 0.5)),
    egui::Rect::from_min_max(egui::pos2(0.5, 0.0), egui::pos2(1.0, 0.5)),
    egui::Rect::from_min_max(egui::pos2(0.0, 0.5), egui::pos2(0.5, 1.0)),
    egui::Rect::from_min_max(egui::pos2(0.5, 0.5), egui::pos2(1.0, 1.0)),
];

struct Res {
    symbol_size: egui::Vec2,
    testcard: egui::TextureHandle,
}

impl Res {
    fn new(ctx: &egui::Context) -> Self {
        Self {
            symbol_size: measure_text(ctx, style::FONT_REGULAR, "M"),
            testcard: ctx.load_texture("testcard", load_testcard(), egui::TextureOptions::NEAREST),
        }
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
        let res = self.res.get_or_insert_with(|| Res::new(ctx));
        let webcam_texture = self.webcam.as_mut().and_then(|it| it.update(ui.ctx()));

        let calc = view::GridCalc::new(res.symbol_size.x, res.symbol_size.y);

        view::Letterbox::new(calc.size_of(160, 45))
            .max_virtual_height(calc.height_of(50))
            .outline(true)
            .show(ui, |ui| {
                if guidelines::ENABLED {
                    guidelines::marker_grid(ui, calc.size_of(10, 5));

                    let xs = [calc.width_of(40), calc.width_of(80), calc.width_of(120)];
                    let ys = [calc.height_of(2)];

                    for x in xs {
                        guidelines::dashed_line(ui, egui::Direction::TopDown, x);
                    }
                    for y in ys {
                        guidelines::dashed_line(ui, egui::Direction::LeftToRight, y);
                    }
                }

                for i in 0..QUADRANT_UVS.len() {
                    let (t, uv) = if let Some(webcam) = webcam_texture {
                        (webcam.id(), QUADRANT_UVS[i])
                    } else {
                        (res.testcard.id(), FULL_UV)
                    };
                    ui.painter().image(
                        t,
                        calc.clone()
                            .absolute(4, 3)
                            .relative(40 * i as isize, 0)
                            .mark()
                            .relative(32, 9)
                            .rect(),
                        uv,
                        egui::Color32::WHITE,
                    );
                }

                calc.clone()
                    .absolute_y(ui.available_height())
                    .snap(view::Rounding::Floor)
                    .relative(2, -2)
                    .mark()
                    .relative(100, 1)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(
                                        "The quick brown fox jumps over the lazy dog да выпей чаю!",
                                    )
                                    .font(style::FONT_REGULAR)
                                    .color(egui::Color32::WHITE),
                                )
                                .selectable(false)
                                .wrap_mode(egui::TextWrapMode::Truncate)
                                .halign(egui::Align::Min),
                            );
                        });
                    });
                calc.clone()
                    .absolute_pos2(ui.available_size().to_pos2())
                    .snap(view::Rounding::Floor)
                    .mark()
                    .relative(-10, -1)
                    .relative(-2, -1)
                    .show(ui, |ui| {
                        let button = egui::Button::new(
                            egui::RichText::new(" Выход ⌘W ")
                                .font(style::FONT_REGULAR)
                                .color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::BLUE)
                        .stroke(egui::Stroke::NONE)
                        .frame(false);

                        if ui.add(button).clicked() {
                            action = Action::GotoMenu;
                        }
                    });
                calc.clone()
                    .absolute_pos2(ui.available_size().to_pos2())
                    .snap(view::Rounding::Floor)
                    .mark()
                    .relative(-8, -1)
                    .relative(-13, -1)
                    .show(ui, |ui| {
                        let button = egui::Button::new(
                            egui::RichText::new(" Камера ")
                                .font(style::FONT_REGULAR)
                                .color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::BLUE)
                        .stroke(egui::Stroke::NONE)
                        .frame(false);

                        if ui.add(button).clicked() {
                            action = Action::ToggleCamera;
                        }
                    });
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

fn load_testcard() -> egui::ColorImage {
    let tree = resvg::usvg::Tree::from_data(
        crate::assets::TESTCARD_SVG,
        &resvg::usvg::Options::default(),
    )
    .expect("failed to parse testcard.svg");

    let size = tree.size();
    let w = size.width() as u32;
    let h = size.height() as u32;

    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h).expect("failed to create pixmap");
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::default(),
        &mut pixmap.as_mut(),
    );

    let pixels: Vec<egui::Color32> = pixmap
        .data()
        .chunks_exact(4)
        .map(|p| egui::Color32::from_rgba_premultiplied(p[0], p[1], p[2], p[3]))
        .collect();

    egui::ColorImage {
        size: [w as usize, h as usize],
        pixels,
    }
}
