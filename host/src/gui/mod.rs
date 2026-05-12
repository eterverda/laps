use crate::gui::live::Live;

mod guidelines;
mod live;
mod view;

const FONT_REGULAR: egui::FontId = egui::FontId::new(13.0, egui::FontFamily::Monospace);
const FONT_REGULAR_X2: egui::FontId = egui::FontId::new(26.0, egui::FontFamily::Monospace);

enum State {
    None,
    Live(live::Live),
}

#[derive(Default)]
pub struct Navigator(Option<State>);

impl Navigator {
    fn goto(&mut self, state: State) {
        self.0 = Some(state);
    }
}

struct App {
    state: State,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        Self {
            state: State::Live(Live::new(ctx)),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::BLACK))
            .show(ctx, |ui| {
                let mut navigator = Navigator::default();

                match &mut self.state {
                    State::None => {
                        ui.vertical_centered(|ui| {
                            ui.add_space(ui.available_height() / 2.0 - 12.0);
                            if ui.button("Start").clicked() {
                                navigator.goto(State::Live(live::Live::new(ctx)));
                            }
                        });
                    }
                    State::Live(live) => live.update(ui, &mut navigator),
                }

                match navigator.0 {
                    Some(state) => self.state = state,
                    None => {}
                };
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
            Ok(Box::new(App::new(&cc.egui_ctx)))
        }),
    )
    .unwrap();
}
