mod guidelines;
mod view;

const FONT_REGULAR: egui::FontId = egui::FontId::new(13.0, egui::FontFamily::Monospace);

const FULL_UV: egui::Rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
const QUADRANT_UVS: [egui::Rect; 4] = [
    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(0.5, 0.5)),
    egui::Rect::from_min_max(egui::pos2(0.5, 0.0), egui::pos2(1.0, 0.5)),
    egui::Rect::from_min_max(egui::pos2(0.0, 0.5), egui::pos2(0.5, 1.0)),
    egui::Rect::from_min_max(egui::pos2(0.5, 0.5), egui::pos2(1.0, 1.0)),
];

struct App {
    font_size: Option<egui::Vec2>,
    webcam: Option<crate::driver::webcam::Webcam>,
    testcard: egui::TextureHandle,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        let ctx_clone = ctx.clone();
        let webcam = crate::driver::webcam::Webcam::start(Box::new(move || {
            ctx_clone.request_repaint();
        }));
        Self {
            font_size: Option::default(),
            webcam,
            testcard: ctx.load_texture("testcard", load_testcard(), egui::TextureOptions::NEAREST),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let font_size = self.font_size.get_or_insert_with(|| measure_font(ctx));

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::BLACK))
            .show(ctx, |ui| {
                let webcam_texture = match self.webcam.as_mut() {
                    Some(w) => w.update(ctx),
                    None => None,
                };

                let calc = view::GridCalc::new(font_size.x, font_size.y);

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
                            let (t, uv) = if let Some(tex) = webcam_texture {
                                (tex.id(), QUADRANT_UVS[i])
                            } else {
                                (self.testcard.id(), FULL_UV)
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
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(
                                            "The quick brown fox jumps over the lazy dog да выпей чаю!",
                                        )
                                        .font(FONT_REGULAR)
                                        .color(egui::Color32::WHITE),
                                    )
                                    .selectable(false)
                                    .wrap_mode(egui::TextWrapMode::Truncate)
                                    .halign(egui::Align::Min),
                                );
                            });
                    });
            });
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

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "fira_regular".to_owned(),
        egui::FontData::from_static(crate::assets::FIRA_REGULAR),
    );
    fonts.font_data.insert(
        "fira_bold".to_owned(),
        egui::FontData::from_static(crate::assets::FIRA_BOLD),
    );
    fonts.families.insert(
        egui::FontFamily::Monospace,
        vec!["fira_regular".to_owned(), "fira_bold".to_owned()],
    );
    ctx.set_fonts(fonts);
}

fn measure_font(ctx: &egui::Context) -> egui::Vec2 {
    let layout_job = egui::text::LayoutJob::single_section(
        "M".to_owned(),
        egui::TextFormat::simple(FONT_REGULAR, egui::Color32::WHITE),
    );
    let galley = ctx.fonts(|f| f.layout_job(layout_job));
    galley.rect.size()
}

pub fn run() {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size((1280.0, 720.0))
            .with_min_inner_size((960.0, 540.0))
            .with_icon(egui::IconData::default())
            .with_title_shown(false)
            .with_titlebar_shown(false)
            .with_titlebar_buttons_shown(true)
            .with_fullsize_content_view(true),
        ..Default::default()
    };

    eframe::run_native(
        "Laps",
        options,
        Box::new(|cc| {
            setup_fonts(&cc.egui_ctx);
            Ok(Box::new(App::new(&cc.egui_ctx)))
        }),
    )
    .unwrap();
}
