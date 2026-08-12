use super::*;

#[allow(unused)]
const QUADRANT_UVS: [egui::Rect; 4] = [
    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(0.5, 0.5)),
    egui::Rect::from_min_max(egui::pos2(0.5, 0.0), egui::pos2(1.0, 0.5)),
    egui::Rect::from_min_max(egui::pos2(0.0, 0.5), egui::pos2(0.5, 1.0)),
    egui::Rect::from_min_max(egui::pos2(0.5, 0.5), egui::pos2(1.0, 1.0)),
];

#[allow(unused)]
const QUADRANT_UVS_NARROWER: [egui::Rect; 4] = [
    egui::Rect::from_min_max(egui::pos2(0.0625, 0.0), egui::pos2(0.4375, 0.5)),
    egui::Rect::from_min_max(egui::pos2(0.5625, 0.0), egui::pos2(0.9375, 0.5)),
    egui::Rect::from_min_max(egui::pos2(0.0625, 0.5), egui::pos2(0.4375, 1.0)),
    egui::Rect::from_min_max(egui::pos2(0.5625, 0.5), egui::pos2(0.9375, 1.0)),
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
        ctx.request_repaint(); // TODO concider remove

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

                    let xs = [
                        grid::cell_x(8),
                        grid::cell_x(46),
                        grid::cell_x(84),
                        grid::cell_x(122),
                    ];

                    for x in xs {
                        guidelines::dashed_line(ui, egui::Direction::TopDown, x);
                    }
                }

                let bottom_right = grid::cell_at(ui.max_rect().max);

                const VIEWFINDER_ROWS: isize = 12;
                const BORDER_COLORS: [egui::Color32; 4] = [
                    egui::Color32::from_rgb(0xFF, 0x33, 0x33),
                    egui::Color32::from_rgb(0x33, 0x33, 0xFF),
                    egui::Color32::from_rgb(0x33, 0xCC, 0x33),
                    egui::Color32::from_rgb(0xCC, 0x88, 0x00),
                ];
                const CHANNEL_NAMES: [&str; 4] = ["R1", "R3", "R4", "R6"];
                const PILOT_NAMES: [&str; 4] = [
                    "Скорострельников Генадий",
                    "Поэт Бездомный",
                    "Иванов Иван Иваныч",
                    "Цой Жив",
                ];
                let border_width = grid::cell_x(1) / 2.0;
                for (i, uv) in QUADRANT_UVS_NARROWER.into_iter().enumerate() {
                    let pilot_col = 7 + 38 * i as isize;
                    let viewfinder_rect = grid::cell(4, 2)
                        .translate(pilot_col, 1)
                        .extrude(32, VIEWFINDER_ROWS);
                    ui.painter().rect_stroke(
                        viewfinder_rect,
                        0.0,
                        egui::Stroke::new(border_width, BORDER_COLORS[i]),
                        egui::StrokeKind::Inside,
                    );
                    viewfinder::Viewfinder::new(uv).show(ui, webcam_contents, viewfinder_rect);

                    let label_rect = grid::cell(4, 1)
                        .translate(pilot_col, 0)
                        .extrude(4, 2)
                        .expand2(egui::vec2(grid::cell_x(1) / 2.0, 0.0))
                        .translate(egui::vec2(0.0, grid::cell_y(-1) / 2.0));
                    ui.painter().rect_filled(label_rect, 0.0, BORDER_COLORS[i]);
                    ui.painter().text(
                        label_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        CHANNEL_NAMES[i],
                        style::FONT_REGULAR_X2,
                        egui::Color32::WHITE,
                    );

                    let pilot_rect = grid::cell(10, 1).translate(pilot_col, 0).extrude(0, 1);
                    ui.painter().text(
                        pilot_rect.center(),
                        egui::Align2::LEFT_CENTER,
                        PILOT_NAMES[i],
                        style::FONT_REGULAR,
                        egui::Color32::WHITE,
                    );

                    guidelines::dashed_rect(
                        ui,
                        grid::cell(4, 15 + VIEWFINDER_ROWS)
                            .translate(pilot_col, 0)
                            .extrude(32, bottom_right.row - 18 - VIEWFINDER_ROWS),
                    );
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

                let timestamp = format_timestamp(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos() as i128,
                );
                let text_rect = grid::cell(11, bottom_right.row)
                    .translate(0, -1)
                    .extrude(23, -1);
                ui.painter().text(
                    text_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    timestamp,
                    style::FONT_REGULAR,
                    egui::Color32::WHITE,
                );
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
                let desc = crate::config::CameraDescription::new("C7-1", 1920, 1080, 30);
                self.webcam =
                    crate::driver::webcam::Webcam::start(&desc, move || ctx.request_repaint());
                log::info!("webcam started");
            }
        }
    }
}

fn format_timestamp(unix_time: i128) -> String {
    let dt = time::OffsetDateTime::from_unix_timestamp_nanos(unix_time)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
        .to_offset(time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC));
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.millisecond(),
    )
}
