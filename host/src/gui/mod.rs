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
    fn persist_egui_memory(&self) -> bool {
        false // this app has no egui state worth persisting between runs
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if fullscreen_pressed(&ctx) {
            let fullscreen = ctx.input(|i| i.viewport().fullscreen).unwrap_or(false);
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
        }
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

fn fullscreen_shortcuts() -> Vec<egui::KeyboardShortcut> {
    let mut shortcuts = vec![egui::KeyboardShortcut::new(
        egui::Modifiers::NONE,
        egui::Key::F11,
    )];
    if cfg!(target_os = "macos") {
        shortcuts.push(egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND | egui::Modifiers::CTRL,
            egui::Key::F,
        ));
    }
    shortcuts
}

fn fullscreen_pressed(ctx: &egui::Context) -> bool {
    let shortcuts = fullscreen_shortcuts();
    ctx.input_mut(|i| shortcuts.iter().any(|s| i.consume_shortcut(s)))
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
        persist_window: false,
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
            // We do our own zooming and theming; neutralize egui's automatics.
            cc.egui_ctx.options_mut(|o| {
                o.zoom_with_keyboard = false; // no Ctrl+/-/0 zoom
                o.theme_preference = egui::ThemePreference::Dark;
                o.sync_window_theme = false; // don't touch native window decorations
            });
            setup_fonts(&cc.egui_ctx);
            Ok(Box::new(App::new()))
        }),
    )
    .unwrap();
}

fn measure_text(ctx: &egui::Context, font: egui::FontId, text: impl Into<String>) -> egui::Vec2 {
    // Use raw font metrics, not galley layout: layout snaps row heights
    // to whole physical pixels, which drifts at fractional pixels_per_point.
    // glyph_width/row_height are scale-independent and deterministic.
    // Note: kerning and ligatures are ignored, single-line text only.
    let text = text.into();
    ctx.fonts_mut(|fonts| {
        let width: f32 = text.chars().map(|c| fonts.glyph_width(&font, c)).sum();
        egui::vec2(width, fonts.row_height(&font))
    })
}
