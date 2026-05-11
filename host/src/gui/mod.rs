mod guidelines;
mod view;

const FONT_REGULAR: egui::FontId = egui::FontId::new(13.0, egui::FontFamily::Monospace);

struct App {
    font_size: Option<egui::Vec2>,
}

impl App {
    fn new() -> Self {
        Self {
            font_size: Option::default(),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let font_size = self.font_size.get_or_insert_with(|| measure_font(ctx));

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::BLACK))
            .show(ctx, |ui| {
                let calc = view::GridCalc::new(font_size.x, font_size.y);

                view::Letterbox::new(calc.size_px(160, 45))
                    .max_virtual_height(calc.height_px(50))
                    .outline(true)
                    .show(ui, |ui| {
                        if guidelines::ENABLED {
                            guidelines::marker_grid(ui, calc.size_px(10, 5));

                            let xs = [calc.width_px(40), calc.width_px(80), calc.width_px(120)];
                            let ys = [calc.height_px(2)];

                            for x in xs {
                                guidelines::dashed_line(ui, egui::Direction::TopDown, x);
                            }
                            for y in ys {
                                guidelines::dashed_line(ui, egui::Direction::LeftToRight, y);
                            }
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
            Ok(Box::new(App::new()))
        }),
    )
    .unwrap();
}
