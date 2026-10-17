use super::ruler::{self, IntoRuler};
use super::{State, header, view};

pub struct Menu {
    header: header::Header,
}

pub enum Action {
    Start,
    None,
}

/// Кнопка START — кислотный зелёный, как RACE у flight'а.
const START_IDLE: egui::Color32 = egui::Color32::from_rgb(0x75, 0x77, 0x48);
const START_ACTIVE: egui::Color32 = egui::Color32::from_rgb(0xC8, 0xF5, 0x00);

impl From<Menu> for State {
    fn from(val: Menu) -> Self {
        State::Menu(val)
    }
}

impl Menu {
    pub fn new() -> Self {
        Self {
            header: header::Header::new(),
        }
    }

    pub fn update(&mut self, ui: &mut egui::Ui) -> Action {
        let mut action = Action::None;
        // Разметка — копия flight'овской: тот же Letterbox (160×45
        // клеток, виртуальный максимум 50 строк, обводка) и тот же
        // хедер на две клетки от верха.
        view::Letterbox::new(egui::vec2(ruler::cell_x(160), ruler::cell_y(45)))
            .max_virtual_height(ruler::cell_y(50))
            .outline(true)
            .show(ui, |ui| {
                let header_rect: egui::Rect = ui
                    .max_rect()
                    .into_ruler()
                    .collapse_top()
                    .extrude(0.0, ruler::cell_y(2))
                    .into();
                self.header.show(ui, header_rect);

                // START — в центре площадки. Кнопка транспортного стиля
                // flight'а (interact → paint); ширина — контент плюс по
                // колонке поля с каждой стороны.
                let label = "\u{f070e} START";
                let w = view::content_width(ui.ctx(), label) + ruler::cell_x(4);
                let center = ui.max_rect().center();
                let start_rect: egui::Rect =
                    egui::pos2(center.x - w / 2.0, center.y - ruler::cell_y(1))
                        .into_ruler()
                        .extrude(w, ruler::cell_y(2))
                        .into();
                let response = view::TransportButton::new(
                    label,
                    view::TransportState::Idle,
                    START_IDLE,
                    START_ACTIVE,
                )
                .interact(ui, start_rect, ui.make_persistent_id("menu_start"));
                view::TransportButton::new(
                    label,
                    if response.clicked() {
                        view::TransportState::Active
                    } else {
                        view::TransportState::Idle
                    },
                    START_IDLE,
                    START_ACTIVE,
                )
                .paint(ui, start_rect);
                if response.clicked() {
                    action = Action::Start;
                }
            });
        action
    }
}
