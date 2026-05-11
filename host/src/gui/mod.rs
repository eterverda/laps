mod view;

struct App;

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::BLACK))
            .show(ctx, |ui| {
                let calc = view::GridCalc::new(12.0, 24.0);

                view::Letterbox::new(calc.size_px(160, 45))
                    .max_virtual_height(calc.height_px(50))
                    .outline((1.0, egui::Color32::from_white_alpha(0x0f)))
                    .show(ui, |ui| {
                        view::Grid::default()
                            .marker_step([120.0, 120.0])
                            .marker_size(10.0)
                            .marker_color(egui::Color32::from_white_alpha(0x0f))
                            .show(ui);

                        calc.clone()
                            .absolute(2, 1)
                            .mark()
                            .relative(4, 2)
                            .show(ui, |ui| ui.button("text"));

                        calc.clone()
                            .absolute_y(ui.available_height())
                            .snap_row(view::Rounding::Floor)
                            .relative(2, -3)
                            .mark()
                            .relative(4, 2)
                            .show(ui, |ui| ui.button("text"));
                    });
            });
    }
}

pub fn run() {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0])
            .with_icon(egui::IconData::default())
            .with_title_shown(false)
            .with_titlebar_shown(false)
            .with_titlebar_buttons_shown(true)
            .with_title_shown(false)
            .with_fullsize_content_view(true),
        ..Default::default()
    };

    eframe::run_native("Laps", options, Box::new(|_cc| Ok(Box::new(App)))).unwrap();
}
