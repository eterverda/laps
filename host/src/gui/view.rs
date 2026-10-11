pub struct Letterbox {
    min_virtual_size: egui::Vec2,
    max_virtual_size: egui::Vec2,
    outline: bool,
}

impl Letterbox {
    #[inline]
    pub fn new(min_virual_size: impl Into<egui::Vec2>) -> Self {
        let min_virtual_size = min_virual_size.into();
        Self {
            min_virtual_size,
            max_virtual_size: min_virtual_size,
            outline: false,
        }
    }

    #[inline]
    pub fn max_virtual_height(mut self, height: f32) -> Self {
        self.max_virtual_size.y = height.max(self.min_virtual_size.y);
        self
    }

    #[inline]
    pub fn outline(mut self, on: bool) -> Self {
        self.outline = on;
        self
    }

    #[inline]
    pub fn show<R>(
        &self,
        ui: &mut egui::Ui,
        content: impl FnOnce(&mut egui::Ui) -> R,
    ) -> egui::InnerResponse<R> {
        let available = ui.available_size();
        let min_rect = ui.min_rect().min;

        let base_ratio = self.min_virtual_size.x / self.min_virtual_size.y;
        let min_ratio = self.min_virtual_size.x / self.max_virtual_size.y;
        let max_ratio = self.max_virtual_size.x / self.min_virtual_size.y;
        let available_ratio = available.x / available.y;

        let (virtual_size, scale, offset) = if available_ratio >= max_ratio {
            let s = available.y / self.min_virtual_size.y;
            (
                egui::vec2(self.max_virtual_size.x, self.min_virtual_size.y),
                s,
                egui::vec2((available.x - self.max_virtual_size.x * s) / 2.0, 0.0),
            )
        } else if available_ratio <= min_ratio {
            let s = available.x / self.min_virtual_size.x;
            (
                egui::vec2(self.min_virtual_size.x, self.max_virtual_size.y),
                s,
                egui::vec2(0.0, (available.y - self.max_virtual_size.y * s) / 2.0),
            )
        } else if available_ratio > base_ratio {
            let s = available.y / self.min_virtual_size.y;
            let w = available.x / s;
            (
                egui::vec2(w, self.min_virtual_size.y),
                s,
                egui::vec2(0.0, 0.0),
            )
        } else {
            let s = available.x / self.min_virtual_size.x;
            let h = available.y / s;
            (
                egui::vec2(self.min_virtual_size.x, h),
                s,
                egui::vec2(0.0, 0.0),
            )
        };

        let screen_offset = min_rect + offset;
        let rect = egui::Rect::from_min_size(screen_offset, virtual_size * scale);

        if self.outline {
            let window = ui.min_rect();

            if crate::gui::guidelines::ENABLED {
                if rect.height() < window.height() {
                    let top =
                        egui::Rect::from_min_max(window.min, egui::pos2(rect.max.x, rect.min.y));
                    let bottom =
                        egui::Rect::from_min_max(egui::pos2(rect.min.x, rect.max.y), window.max);

                    crate::gui::guidelines::hatch_rect(ui, top);
                    crate::gui::guidelines::hatch_rect(ui, bottom);

                    crate::gui::guidelines::solid_line(
                        ui,
                        egui::Direction::LeftToRight,
                        rect.min.y,
                    );
                    crate::gui::guidelines::solid_line(
                        ui,
                        egui::Direction::LeftToRight,
                        rect.max.y,
                    );
                } else if rect.width() < window.width() {
                    let left =
                        egui::Rect::from_min_max(window.min, egui::pos2(rect.min.x, rect.max.y));
                    let right =
                        egui::Rect::from_min_max(egui::pos2(rect.max.x, rect.min.y), window.max);

                    crate::gui::guidelines::hatch_rect(ui, left);
                    crate::gui::guidelines::hatch_rect(ui, right);

                    crate::gui::guidelines::solid_line(ui, egui::Direction::TopDown, rect.min.x);
                    crate::gui::guidelines::solid_line(ui, egui::Direction::TopDown, rect.max.x);
                }
            }
        }

        let area_id = ui.id().with("letterbox_area");
        let area = egui::Area::new(area_id)
            .movable(false)
            .interactable(true)
            .order(egui::Order::Middle);

        let layer_id = area.layer();
        let transform = egui::emath::TSTransform::new(screen_offset.to_vec2(), scale);
        ui.ctx().set_transform_layer(layer_id, transform);

        let response = area.show(ui.ctx(), |area_ui| {
            area_ui.set_clip_rect(egui::Rect::from_min_size(egui::Pos2::ZERO, virtual_size));
            area_ui.set_min_size(virtual_size);
            content(area_ui)
        });

        egui::InnerResponse::new(response.inner, response.response)
    }
}

/// Большая кнопка статуса: текст слева (слова через одинарные пробелы —
/// по одной ячейке). Rect кнопки приходит снаружи целиком и должен
/// покрывать весь контент — ширину считает вызывающий код (см.
/// content_width). Ховер и клик — по пришедшему rect, Response
/// возвращается наружу — действие решает вызывающий код.
pub struct BigButton<'a> {
    /// Левая строка кнопки: разбивается по пробелам, слова рисуются
    /// в ряд, между ними — одинарный (1 ячейка) пробел.
    pub text: &'a str,
    pub color: egui::Color32,
}

impl<'a> BigButton<'a> {
    pub fn new(text: &'a str, color: egui::Color32) -> Self {
        Self { text, color }
    }

    pub fn show(&self, ui: &mut egui::Ui, rect: egui::Rect, id: egui::Id) -> egui::Response {
        let response = ui
            .interact(rect, id, egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);

        let painter = ui.painter();
        let mut pos = rect.left_bottom();
        for word in self.text.split(' ').filter(|word| !word.is_empty()) {
            painter.text(
                pos,
                egui::Align2::LEFT_BOTTOM,
                word,
                super::style::FONT_REGULAR_X2,
                self.color,
            );
            pos.x += super::measure_text(ui.ctx(), super::style::FONT_REGULAR_X2, word).x
                + super::grid::cell_x(1);
        }
        response
    }
}

/// Ширина текста кнопки в пикселях — чтобы вызывающий код мог
/// посчитать rect.
pub fn content_width(ctx: &egui::Context, text: &str) -> f32 {
    let words: Vec<&str> = text.split(' ').filter(|word| !word.is_empty()).collect();
    let mut width: f32 = words
        .iter()
        .map(|word| super::measure_text(ctx, super::style::FONT_REGULAR_X2, *word).x)
        .sum();
    if words.len() > 1 {
        width += super::grid::cell_x(1) * (words.len() - 1) as f32;
    }
    width
}

/// Угол полосы: определяет, с какой стороны обрезочка (с внешней) и
/// как читать пришедший rect. Полоса никуда сама не тянется —
/// прижатие к краям экрана делает вызывающий код через rect.
#[derive(Clone, Copy)]
pub enum Corner {
    TopLeft,
    TopRight,
    // Для нижних полос (подписи внизу экрана) — пока не используются.
    #[allow(dead_code)]
    BottomLeft,
    #[allow(dead_code)]
    BottomRight,
}

/// Фон-полоса высотой в одну клетку: обрезочка (1 клетка) с внешней
/// стороны и плашка. Rect приходит снаружи и должен уже включать всё:
/// обрезочку и по клетке поля с каждой стороны текста. Текст вызывающий
/// код рисует сам в замыкании — оно получает rect контента (плашка без
/// по клетке слева и справа; по вертикали отступов нет).
pub struct Stripe {
    color: egui::Color32,
    corner: Corner,
}

impl Stripe {
    pub fn new(color: egui::Color32, corner: Corner) -> Self {
        Self { color, corner }
    }

    pub fn show(
        &self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        contents: impl FnOnce(&mut egui::Ui, egui::Rect),
    ) {
        let content = stripe(ui, rect, self.color, self.corner, 1);
        contents(ui, content);
    }
}

/// То же, что Stripe, но высотой в две клетки и с обрезочкой в две
/// клетки (шрифт x2) — под заголовки уровня LAPS.
pub struct StripeX2 {
    color: egui::Color32,
    corner: Corner,
}

impl StripeX2 {
    pub fn new(color: egui::Color32, corner: Corner) -> Self {
        Self { color, corner }
    }

    pub fn show(
        &self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        contents: impl FnOnce(&mut egui::Ui, egui::Rect),
    ) {
        let content = stripe(ui, rect, self.color, self.corner, 2);
        contents(ui, content);
    }
}

/// Общая разметка полосы: `rows` — высота в клетках (1 у Stripe, 2 у
/// StripeX2), ширина обрезочки — столько же клеток с внешней стороны.
/// Возвращает rect контента. На размеры окна не смотрим никогда —
/// пришедший rect — единственный источник геометрии.
///
/// Фон — один заполненный полигон (плашка + обрезочка): два соседних
/// примитива (rect + глиф) на дробном зуме давали волосинку фона на
/// стыке, а глиф-строка — швы между глифами. Обрезочка — диагональный
/// срез к внешнему нижнему краю, как у глифов \u{e0bc}/\u{e0be}.
fn stripe(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    color: egui::Color32,
    corner: Corner,
    rows: isize,
) -> egui::Rect {
    use super::grid::IntoCell;
    // Обрезочка с внешней стороны: у левых полос справа, у правых слева.
    let (cut, plate) = match corner {
        Corner::TopLeft | Corner::BottomLeft => {
            let cut = rect
                .right_top()
                .into_cell()
                .translate(-rows, 0)
                .extrude(rows, rows);
            (cut, rect.with_max_x(cut.left()))
        }
        Corner::TopRight | Corner::BottomRight => {
            let cut = rect.left_top().into_cell().extrude(rows, rows);
            (cut, rect.with_min_x(cut.right()))
        }
    };

    // Плашка и обрезочка — одна трапеция: общий контур без единого стыка.
    // Обрезочка — треугольник: диагональ от внешнего верхнего угла к
    // внешнему нижнему, по пропорции ячейки 1:2 (вся ширина обрезки на
    // всю высоту полосы).
    let outline = match corner {
        Corner::TopLeft | Corner::BottomLeft => vec![
            plate.min,
            egui::pos2(cut.max.x, cut.min.y),
            egui::pos2(cut.min.x, cut.max.y),
            plate.left_bottom(),
        ],
        Corner::TopRight | Corner::BottomRight => {
            vec![cut.min, plate.right_top(), plate.right_bottom(), cut.max]
        }
    };
    ui.painter().add(egui::Shape::convex_polygon(
        outline,
        color,
        egui::Stroke::NONE,
    ));

    // Контент: по клетке поля слева и справа от текста; по вертикали
    // отступов нет.
    plate
        .with_min_x(plate.min.x + super::grid::cell_x(1))
        .with_max_x(plate.max.x - super::grid::cell_x(1))
}

/// Фон-плашка-стрелка в стиле промпта PowerShell: высота две клетки,
/// справа наконечник — контур идёт от правого верхнего угла к середине
/// правой стороны и к правому нижнему углу, фон образует стрелку (не
/// глиф). Rect приходит целиком (вместе с наконечником и левой частью
/// под соседнюю плашку). Только рисует: текст вызывающий код кладёт по
/// своему rect'у сам.
pub fn arrow_plate(ui: &mut egui::Ui, rect: egui::Rect, color: egui::Color32) {
    let tip = super::grid::cell_x(2);
    let outline = vec![
        rect.left_top(),
        egui::pos2(rect.right() - tip, rect.top()),
        egui::pos2(rect.right(), rect.center().y),
        egui::pos2(rect.right() - tip, rect.bottom()),
        rect.left_bottom(),
    ];
    ui.painter().add(egui::Shape::convex_polygon(
        outline,
        color,
        egui::Stroke::NONE,
    ));
}

/// Подсказка-значок в правом нижнем углу anchor: правый нижний угол
/// текста — на клетку влево и вверх от угла anchor, вся плашка сдвинута
/// на пол-высоты клетки вниз (визуальная центровка на картинке).
pub fn hint_badge_corner(ui: &mut egui::Ui, anchor: egui::Rect, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    let row = super::grid::CELL_SIZE.y;
    let width = lines
        .iter()
        .map(|line| super::measure_text(ui.ctx(), super::style::FONT_REGULAR, line).x)
        .fold(0.0f32, f32::max);
    let right = anchor.right() - super::grid::cell_x(1);
    let bottom = anchor.bottom() - super::grid::cell_y(1);
    let text = egui::Rect::from_min_max(
        egui::pos2(right - width, bottom - row),
        egui::pos2(right, bottom),
    )
    .translate(egui::vec2(0.0, row * 0.5));
    hint_badge(ui, text, egui::Align2::RIGHT_BOTTOM, lines);
}

/// Подсказка-значок: текст выравнивается к краям text по align (координаты
/// — в границах текста: фон и обводка рисуются за его пределами). Фон —
/// скруглённый rect: поле и радиус — по половине ширины ячейки (и по
/// вертикали тоже), обводка белая 1 пункт прямо по границам фона
/// (снаружи), той же скруглённости.
pub fn hint_badge(ui: &mut egui::Ui, text: egui::Rect, align: egui::Align2, lines: &[String]) {
    let pad = super::grid::CELL_SIZE.x * 0.5;
    let row = super::grid::CELL_SIZE.y;
    let bg = text.expand(pad);
    ui.painter()
        .rect_filled(bg, pad, egui::Color32::from_rgb(0x1E, 0x1E, 0x2E));
    ui.painter().rect_stroke(
        bg,
        pad,
        egui::Stroke::new(1.0, egui::Color32::LIGHT_GRAY),
        egui::StrokeKind::Outside,
    );
    let x = match align {
        egui::Align2::LEFT_BOTTOM => text.left(),
        egui::Align2::RIGHT_BOTTOM => text.right(),
        other => unreachable!("hint_badge: unsupported align {other:?}"),
    };
    for (i, line) in lines.iter().enumerate() {
        ui.painter().text(
            egui::pos2(x, text.top() + row * (i + 1) as f32),
            align,
            line,
            super::style::FONT_REGULAR,
            egui::Color32::WHITE,
        );
    }
}
