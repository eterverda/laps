mod grid;
mod guidelines;
mod live;
mod menu;
mod style;
mod view;
mod viewfinder;

enum State {
    Menu(menu::Menu),
    Live(live::Live),
}

#[derive(Default)]
pub struct Navigator(Option<State>);

impl Navigator {
    fn goto(&mut self, state: impl Into<State>) {
        self.0 = Some(state.into());
    }
}

struct App {
    state: State,
}

impl App {
    fn new() -> Self {
        Self {
            state: State::Live(live::Live::new()),
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(egui::Color32::BLACK))
            .show(ui, |ui| {
                let mut navigator = Navigator::default();

                match self.state {
                    State::Menu(ref mut menu) => menu.update(ui, &mut navigator),
                    State::Live(ref mut live) => live.update(ui, &mut navigator),
                }

                match navigator.0 {
                    Some(state) => self.state = state,
                    None => {}
                }
            });
    }
}

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "fira_regular".to_owned(),
        egui::FontData::from_static(crate::assets::FIRA_REGULAR).into(),
    );
    fonts.font_data.insert(
        "fira_bold".to_owned(),
        egui::FontData::from_static(crate::assets::FIRA_BOLD).into(),
    );
    fonts.families.insert(
        egui::FontFamily::Monospace,
        vec!["fira_regular".to_owned(), "fira_bold".to_owned()],
    );
    ctx.set_fonts(fonts);
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

fn measure_text(ctx: &egui::Context, font: egui::FontId, text: impl Into<String>) -> egui::Vec2 {
    let layout_job = egui::text::LayoutJob::single_section(
        text.into(),
        egui::TextFormat::simple(font, egui::Color32::WHITE),
    );
    let galley = ctx.fonts_mut(|f| f.layout_job(layout_job));
    galley.rect.size()
}
