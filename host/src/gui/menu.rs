use super::{State, header, jumbotron, style, view};
use crate::config::setup::PadConfig;
use crate::gui::ruler::{IntoRuler, cell_x, cell_y};
use crate::model::pilot::Pilot;
use indexmap::IndexMap;

pub struct Menu {
    header: header::Header,
    /// Поля имён по падам (индекс = пад − 1): пустое — пилота нет,
    /// вылет с меньшим числом участников.
    pilots: [String; 4],
    /// Плашки падов перед полями: белый label на цвете пада.
    labels: [(String, egui::Color32); 4],
}

pub enum Action {
    Start(IndexMap<String, Pilot>),
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
    pub fn new(pads: &IndexMap<String, PadConfig>) -> Self {
        // Поля всегда с дефолтами: правки не сохраняются между визитами
        // в меню — стартовый набор правится под конкретный вылет.
        let pilots: [String; 4] = std::array::from_fn(|i| format!("Пилот {}", i + 1));
        let labels: [(String, egui::Color32); 4] = std::array::from_fn(|i| {
            pads.get(&format!("pad-{}", i + 1)).map_or_else(
                || (String::new(), egui::Color32::BLACK),
                |pad| (pad.label.clone(), style::pad_color(pad.color)),
            )
        });
        Self {
            header: header::Header::new(),
            pilots,
            labels,
        }
    }

    pub fn update(&mut self, ui: &mut egui::Ui, jumbotron: &mut jumbotron::Jumbotron) -> Action {
        let mut action = Action::None;
        // Разметка — копия flight'овской: тот же Letterbox (160×45
        // клеток, виртуальный максимум 50 строк, обводка) и тот же
        // хедер на две клетки от верха.
        view::Letterbox::new(egui::vec2(cell_x(160), cell_y(45)))
            .max_virtual_height(cell_y(50))
            .outline(true)
            .show(ui, |ui| {
                let header_rect: egui::Rect = ui
                    .max_rect()
                    .into_ruler()
                    .collapse_top()
                    .extrude(0.0, cell_y(2))
                    .into();
                self.header.show(ui, header_rect);
                jumbotron::jumbo_button(ui, header_rect, jumbotron);

                // START — в центре площадки. Кнопка транспортного стиля
                // flight'а (interact → paint); ширина — контент плюс по
                // колонке поля с каждой стороны.
                let label = "\u{f070e} START";
                let w = view::content_width(ui.ctx(), label) + cell_x(4);
                let center = ui.max_rect().center();
                let start_rect: egui::Rect = egui::pos2(center.x - w / 2.0, center.y - cell_y(1))
                    .into_ruler()
                    .extrude(w, cell_y(2))
                    .into();
                // Плашки + поля имени — колонка ровно над кнопкой.
                // Плашка: label белым по цвету пада, буквы по клеткам,
                // подложка на полклетки шире с каждой стороны; ширина
                // всех плашек — по самому длинному label. Поле — 24
                // клетки (FONT_REGULAR: символ = клетке), строка =
                // клетке. Колонка прибита к сетке по центру.
                let label_cells = self
                    .labels
                    .iter()
                    .map(|(l, _)| l.chars().count() as isize)
                    .max()
                    .unwrap_or(1);
                let plate_w = cell_x(label_cells) + cell_x(1);
                let gap = cell_x(1);
                let field_w = cell_x(24);
                let total_w = plate_w + gap + field_w;
                let rows_h = cell_y(1) * self.pilots.len() as f32;
                let col = (center.x / cell_x(1)).round() as isize;
                let fields_rect = egui::Rect::from_min_size(
                    egui::pos2(
                        cell_x(col) - total_w / 2.0,
                        start_rect.top() - cell_y(1) - rows_h,
                    ),
                    egui::vec2(total_w, rows_h),
                );
                for (i, ((label, color), _)) in self.labels.iter().zip(&self.pilots).enumerate() {
                    let row = egui::Rect::from_min_size(
                        egui::pos2(fields_rect.left(), fields_rect.top() + cell_y(1) * i as f32),
                        egui::vec2(plate_w, cell_y(1)),
                    );
                    ui.painter()
                        .rect_filled(row.shrink2(egui::vec2(0.0, 2.0)), 0.0, *color);
                    ui.painter().text(
                        egui::pos2(row.left() + cell_x(1) / 2.0, row.center().y),
                        egui::Align2::LEFT_CENTER,
                        label.as_str(),
                        style::FONT_REGULAR,
                        egui::Color32::WHITE,
                    );
                }
                let edits_rect = egui::Rect::from_min_size(
                    egui::pos2(fields_rect.left() + plate_w + gap, fields_rect.top()),
                    egui::vec2(field_w, rows_h),
                );
                // Поля — по одному чилд-Ui на клеточный рект: та же
                // абсолютная система координат, что у плашек (вёрстка
                // подряд плывёт от spacing'ов).
                for (i, name) in self.pilots.iter_mut().enumerate() {
                    let row_rect = egui::Rect::from_min_size(
                        egui::pos2(edits_rect.left(), edits_rect.top() + cell_y(1) * i as f32),
                        egui::vec2(field_w, cell_y(1)),
                    );
                    let mut row_ui = ui.new_child(egui::UiBuilder::new().max_rect(row_rect));
                    let response = row_ui.add_sized(
                        [field_w, cell_y(1)],
                        egui::TextEdit::singleline(name)
                            .font(style::FONT_REGULAR)
                            .text_color(style::TRANSPORT_ACTIVE)
                            .frame(egui::Frame::NONE)
                            .margin(egui::Margin::ZERO)
                            .char_limit(24),
                    );
                    // Обводка — только у поля в фокусе, цветом
                    // активного пада.
                    if response.has_focus() {
                        ui.painter().rect_stroke(
                            response.rect.expand2(egui::vec2(cell_x(1) / 2.0, 0.0)),
                            0.0,
                            egui::Stroke::new(2.0, style::LAP_ACCENT),
                            egui::StrokeKind::Outside,
                        );
                    }
                }
                let response =
                    view::TransportButton::new(label, true, false, START_IDLE, START_ACTIVE)
                        .interact(ui, start_rect, ui.make_persistent_id("menu_start"));
                view::TransportButton::new(
                    label,
                    true,
                    response.clicked(),
                    START_IDLE,
                    START_ACTIVE,
                )
                .paint(ui, start_rect);
                if response.clicked() {
                    action = Action::Start(
                        self.pilots
                            .iter()
                            .enumerate()
                            .filter_map(|(i, name)| {
                                let name = name.trim();
                                (!name.is_empty())
                                    .then(|| (format!("pad-{}", i + 1), Pilot::new(name)))
                            })
                            .collect(),
                    );
                }
            });
        action
    }
}
