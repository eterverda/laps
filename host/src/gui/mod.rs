use crate::config::setup::Setup;
use crate::model::pilot::Pilot;

mod grid;
mod guidelines;
mod header;
mod live;
mod menu;
mod style;
mod view;
mod viewfinder;

enum State {
    Menu(menu::Menu),
    Live(Box<live::Live>),
}

#[derive(Default)]
pub struct Navigator(Option<State>);

impl Navigator {
    fn goto(&mut self, state: impl Into<State>) {
        self.0 = Some(state.into());
    }
}

struct App {
    remote_rx: std::sync::mpsc::Receiver<(
        Vec<String>,
        std::sync::mpsc::Sender<crate::remote::Response>,
    )>,
    state: State,
    setup: Setup,
    assignments: std::collections::HashMap<String, Pilot>,
    // Живёт с приложением: Drop останавливает accept-поток и снимает
    // регистрацию инстанса.
    _server: Option<crate::remote::Server>,
}

fn hardcoded_assignments() -> std::collections::HashMap<String, Pilot> {
    ["pad-1", "pad-2", "pad-3", "pad-4"]
        .into_iter()
        .map(|pad_id| (pad_id.to_owned(), Pilot::random()))
        .collect()
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (remote_tx, remote_rx) = std::sync::mpsc::channel();
        // Колбэк исполнения команд: кладёт задачу в очередь GUI и будит
        // его (иначе ответ ждал бы ближайшего кадра egui), затем ждёт
        // ответа.
        let ctx = cc.egui_ctx.clone();
        let execute: std::sync::Arc<dyn Fn(Vec<String>) -> crate::remote::Response + Send + Sync> =
            std::sync::Arc::new(move |command| {
                let (reply_tx, reply_rx) = std::sync::mpsc::channel();
                if remote_tx.send((command, reply_tx)).is_err() {
                    return crate::remote::Response {
                        message: "gui unavailable".to_owned(),
                        code: 1,
                    };
                }
                ctx.request_repaint();
                match reply_rx.recv_timeout(crate::remote::RESPONSE_TIMEOUT) {
                    Ok(response) => response,
                    Err(_) => crate::remote::Response {
                        message: "no response".to_owned(),
                        code: 1,
                    },
                }
            });
        let server = match crate::remote::Server::start(execute) {
            Ok(server) => Some(server),
            Err(e) => {
                log::warn!("remote control unavailable: {e}");
                None
            }
        };
        // Ошибка сетапа фатальна: без него экраны не построить. Пишем
        // ERROR в лог и выходим с ненулевым кодом (fatal по смыслу — у
        // log-крейта уровня fatal нет, error! + exit(1) принятое замещение).
        let setup: Setup = match serde_yaml::from_str(crate::assets::SETUP_YAML) {
            Ok(setup) => setup,
            Err(e) => {
                log::error!("embedded setup.yaml failed to parse: {e}");
                std::process::exit(1);
            }
        };
        let assignments = hardcoded_assignments();
        Self {
            state: State::Live(Box::new(live::Live::new(
                setup.clone(),
                assignments.clone(),
            ))),
            setup,
            assignments,
            remote_rx,
            _server: server,
        }
    }

    /// Обработка команды удалённого управления: сначала команду получает
    /// текущий экран; отклонённое (None дошло до верха) ловит App — echo
    /// отвечает здесь же, остальное превращается в «unknown command».
    fn handle_remote(
        &mut self,
        ctx: &egui::Context,
        command: &[String],
    ) -> crate::remote::Response {
        let response = match &mut self.state {
            State::Live(live) => live.handle_remote(ctx, command),
            State::Menu(_) => None,
        };
        response.unwrap_or_else(|| {
            // Экраны отклонили: echo — команда уровня приложения,
            // остальное неизвестно.
            if command.first().map(String::as_str) == Some("echo") {
                return crate::remote::Response {
                    message: command[1..].join(" "),
                    code: 0,
                };
            }
            let message = match command.first() {
                Some(head) => format!("unknown command {head:?}"),
                None => "empty command".to_owned(),
            };
            crate::remote::Response { message, code: 1 }
        })
    }
}

impl eframe::App for App {
    fn persist_egui_memory(&self) -> bool {
        false // this app has no egui state worth persisting between runs
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        while let Ok((tokens, reply)) = self.remote_rx.try_recv() {
            let _ = reply.send(self.handle_remote(&ctx, &tokens));
        }
        if fullscreen_pressed(&ctx) {
            let fullscreen = ctx.input(|i| i.viewport().fullscreen).unwrap_or(false);
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(egui::Color32::BLACK))
            .show(ui, |ui| {
                let mut navigator = Navigator::default();

                match self.state {
                    State::Menu(ref mut menu) => match menu.update(ui) {
                        menu::Action::Start => navigator.goto(Box::new(live::Live::new(
                            self.setup.clone(),
                            self.assignments.clone(),
                        ))),
                        menu::Action::None => {}
                    },
                    State::Live(ref mut live) => live.update(ui, &mut navigator),
                }

                if let Some(state) = navigator.0 {
                    self.state = state
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
    // Жирная семья отдельно: у egui::FontId нет флага bold, только семья.
    fonts.families.insert(
        egui::FontFamily::Name("bold".into()),
        vec!["fira_bold".to_owned(), "fira_regular".to_owned()],
    );
    ctx.set_fonts(fonts);
}

pub fn run() {
    let options = eframe::NativeOptions {
        persist_window: false,
        viewport: egui::ViewportBuilder::default()
            .with_app_id("ru.fpvladder.laps")
            .with_inner_size((1280.0, 720.0))
            .with_min_inner_size((960.0, 540.0))
            .with_icon(egui::IconData::default())
            .with_title("LAPS")
            .with_title_shown(true)
            .with_titlebar_shown(true)
            .with_titlebar_buttons_shown(true)
            .with_fullsize_content_view(false),
        ..Default::default()
    };

    eframe::run_native(
        "LAPS",
        options,
        Box::new(|cc| {
            // We do our own zooming and theming; neutralize egui's automatics.
            cc.egui_ctx.options_mut(|o| {
                o.zoom_with_keyboard = false; // no Ctrl+/-/0 zoom
                o.theme_preference = egui::ThemePreference::Dark;
                o.sync_window_theme = false; // don't touch native window decorations
            });
            setup_fonts(&cc.egui_ctx);
            Ok(Box::new(App::new(cc)))
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
