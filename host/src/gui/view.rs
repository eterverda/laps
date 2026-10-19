use crate::gui::guidelines;
use crate::gui::measure_text;
use crate::gui::ruler::{CELL_SIZE, cell_x, cell_y};
use crate::gui::style;

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

        // Виртуальный размер — всегда целое число клеток: виджеты внутри
        // якорятся к краям rect'а, флоор к клеткам за пределами Letterbox
        // не нужен. Границы (min/max) кратны клеткам, округление в них
        // не вылезает.
        let virtual_size = egui::vec2(
            (virtual_size.x / CELL_SIZE.x).round() * CELL_SIZE.x,
            (virtual_size.y / CELL_SIZE.y).round() * CELL_SIZE.y,
        );
        let screen_offset = min_rect + offset;
        let rect = egui::Rect::from_min_size(screen_offset, virtual_size * scale);

        if self.outline {
            let window = ui.min_rect();

            if guidelines::ENABLED {
                if rect.height() < window.height() {
                    // Граница штриховки — 1px наружу от границы letterbox:
                    // диагонали не касаются контента, как и solid-линии.
                    let top = egui::Rect::from_min_max(
                        window.min,
                        egui::pos2(rect.max.x, rect.min.y - 1.0),
                    );
                    let bottom = egui::Rect::from_min_max(
                        egui::pos2(rect.min.x, rect.max.y + 1.0),
                        window.max,
                    );

                    guidelines::hatch_rect(ui, top);
                    guidelines::hatch_rect(ui, bottom);

                    // Обводка — снаружи границы (полштриха наружу), не на ней.
                    guidelines::solid_line(
                        ui,
                        egui::Direction::LeftToRight,
                        rect.min.y,
                        -1.0,
                    );
                    guidelines::solid_line(
                        ui,
                        egui::Direction::LeftToRight,
                        rect.max.y,
                        1.0,
                    );
                } else if rect.width() < window.width() {
                    let left = egui::Rect::from_min_max(
                        window.min,
                        egui::pos2(rect.min.x - 1.0, rect.max.y),
                    );
                    let right = egui::Rect::from_min_max(
                        egui::pos2(rect.max.x + 1.0, rect.min.y),
                        window.max,
                    );

                    guidelines::hatch_rect(ui, left);
                    guidelines::hatch_rect(ui, right);

                    guidelines::solid_line(
                        ui,
                        egui::Direction::TopDown,
                        rect.min.x,
                        -1.0,
                    );
                    guidelines::solid_line(
                        ui,
                        egui::Direction::TopDown,
                        rect.max.x,
                        1.0,
                    );
                }
            }
        }

        let area_id = ui.scope_id().with("letterbox_area");
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

/// Маркер зеркалирования глифа: префикс слова в тексте кнопки — первый
/// глиф слова рисуется зеркально по горизонтали. Зеркальных пар у иконок
/// Nerd Fonts нет (exit-to-app, например, есть только со стрелкой
/// вправо), поэтому разворачиваем сами. Кодпоинт — свободная дыра PUA в
/// обоих начертаниях шрифта: глифа у него нет, в раскладку не попадает,
/// ширины не даёт.
pub const GLYPH_FLIP: char = '\u{e6c0}';

/// Слово без маркеров GLYPH_FLIP (измеряем и рисуем сами глифы).
fn strip_flip_markers(word: &str) -> String {
    word.chars().filter(|&c| c != GLYPH_FLIP).collect()
}

/// Округление позиции к физическим пикселям (паритет с тесселятором
/// текста, у него это метод emath::GuiRounding за пределами крейта).
fn round_to_pixels(pos: egui::Pos2, pixels_per_point: f32) -> egui::Pos2 {
    egui::pos2(
        (pos.x * pixels_per_point).round() / pixels_per_point,
        (pos.y * pixels_per_point).round() / pixels_per_point,
    )
}

/// Слово кнопки на позиции (левый нижний угол). Обычные глифы — прогонами
/// через painter.text; глиф после маркера GLYPH_FLIP — зеркально
/// (paint_glyph_flipped). Возвращает ширину слова (как measure_text у
/// обычного слова).
fn paint_word(ui: &mut egui::Ui, pos: egui::Pos2, word: &str, color: egui::Color32) -> f32 {
    let font = style::FONT_REGULAR_X2;
    let mut x = pos.x;
    // Прогон обычных глифов накапливается в run, маркер его сбрасывает
    // (прогон рисуется) и переворачивает следующий глиф.
    let mut run = String::new();
    let mut chars = word.chars();
    while let Some(c) = chars.next() {
        if c == GLYPH_FLIP {
            if !run.is_empty() {
                ui.painter().text(
                    egui::pos2(x, pos.y),
                    egui::Align2::LEFT_BOTTOM,
                    &run,
                    font.clone(),
                    color,
                );
                x += measure_text(ui.ctx(), font.clone(), &run).x;
                run.clear();
            }
            if let Some(g) = chars.next() {
                x += paint_glyph_flipped(ui, egui::pos2(x, pos.y), g, color);
            }
        } else {
            run.push(c);
        }
    }
    if !run.is_empty() {
        ui.painter().text(
            egui::pos2(x, pos.y),
            egui::Align2::LEFT_BOTTOM,
            &run,
            font.clone(),
            color,
        );
        x += measure_text(ui.ctx(), font, &run).x;
    }
    x - pos.x
}

/// Один глиф зеркально по горизонтали. Ширина — как у обычного текста:
/// квад того же глифа из шрифтового атласа (TextureId::Managed(0)), u в
/// uv отражена. Позиционирование повторяет painter.text с LEFT_BOTTOM
/// (включая округление к физпикселям).
fn paint_glyph_flipped(ui: &mut egui::Ui, pos: egui::Pos2, chr: char, color: egui::Color32) -> f32 {
    let font = style::FONT_REGULAR_X2;
    let galley = ui
        .ctx()
        .fonts_mut(|fonts| fonts.layout_no_wrap(chr.to_string(), font, color));
    let Some(row) = galley.rows.first() else {
        return 0.0;
    };
    let Some(glyph) = row.row.glyphs.first() else {
        return 0.0;
    };
    let uv = glyph.uv_rect;
    // Якорь LEFT_BOTTOM: галлея ставится origin'ом ниже pos на её высоту
    // (округление к физпикселям — как тесселятор текста).
    let ppp = ui.ctx().pixels_per_point();
    let origin = round_to_pixels(egui::pos2(pos.x, pos.y - galley.rect.height()), ppp);
    let min = origin + row.pos.to_vec2() + (glyph.pos + uv.offset).to_vec2();
    let tex = ui.ctx().fonts(|fonts| fonts.font_image_size());
    let u_norm = |u: u16, w: usize| u as f32 / w.max(1) as f32;
    let (u_min, u_max) = (u_norm(uv.min[0], tex[0]), u_norm(uv.max[0], tex[0]));
    let (v_min, v_max) = (u_norm(uv.min[1], tex[1]), u_norm(uv.max[1], tex[1]));
    let rect = egui::Rect::from_min_size(min, uv.size);
    let mut mesh = egui::Mesh::with_texture(egui::TextureId::Managed(0));
    // Порядок вершин и пар треугольников — как у текста в атласе, u зеркалим.
    for (p, u, v) in [
        (rect.left_top(), u_max, v_min),
        (rect.right_top(), u_min, v_min),
        (rect.left_bottom(), u_max, v_max),
        (rect.right_bottom(), u_min, v_max),
    ] {
        mesh.vertices.push(egui::epaint::Vertex {
            pos: p,
            uv: egui::pos2(u, v),
            color,
        });
    }
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(2, 1, 3);
    ui.painter().add(egui::Shape::mesh(mesh));
    glyph.advance_width
}

/// Ширина текста кнопки в пикселях — чтобы вызывающий код мог
/// посчитать rect.
pub fn content_width(ctx: &egui::Context, text: &str) -> f32 {
    let words: Vec<&str> = text.split(' ').filter(|word| !word.is_empty()).collect();
    let mut width: f32 = words
        .iter()
        .map(|word| {
            measure_text(ctx, style::FONT_REGULAR_X2, strip_flip_markers(word)).x
        })
        .sum();
    if words.len() > 1 {
        width += cell_x(1) * (words.len() - 1) as f32;
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
    use crate::gui::ruler::IntoRuler;
    // Обрезочка с внешней стороны: у левых полос справа, у правых слева.
    let (cut, plate) = match corner {
        Corner::TopLeft | Corner::BottomLeft => {
            let cut: egui::Rect = rect
                .right_top()
                .into_ruler()
                .translate(-cell_x(rows), 0.0)
                .extrude(cell_x(rows), cell_y(rows))
                .into();
            (cut, rect.with_max_x(cut.left()))
        }
        Corner::TopRight | Corner::BottomRight => {
            let cut: egui::Rect = rect
                .left_top()
                .into_ruler()
                .extrude(cell_x(rows), cell_y(rows))
                .into();
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
        .with_min_x(plate.min.x + cell_x(1))
        .with_max_x(plate.max.x - cell_x(1))
}

/// Кнопка транспорта (scrub): залитый прямоугольник 3 клетки высотой,
/// верхний текст (иконка) по центру верхних двух клеток, нижний текст —
/// в третьей клетке, с отступом в колонку с каждой стороны (не касается
/// краёв кнопки). Текст всегда чёрный, цвета заливки приходят снаружи.
/// Клик-бласть — rect целиком (уголков нет). interact и paint раздельны:
/// между ними кладётся логика нажатия, чтобы кадр клика рисовался уже
/// нажатым (иначе вспышка нажатия теряется между кадрами).
pub struct TransportButton<T: AsRef<str>> {
    text: T,
    enabled: bool,
    active: bool,
    idle: egui::Color32,
    active_color: egui::Color32,
}

impl<T: AsRef<str>> TransportButton<T> {
    /// enabled — кликабельна; active — нажата (включая тап) или
    /// активирована. Внешние состояния не нужны: решения о виде —
    /// внутри (disabled = !enabled).
    pub fn new(text: T, enabled: bool, active: bool, idle: egui::Color32, active_color: egui::Color32) -> Self {
        Self {
            text,
            enabled,
            active,
            idle,
            active_color,
        }
    }

    /// Принять клик по rect целиком.
    pub fn interact(&self, ui: &mut egui::Ui, rect: egui::Rect, id: egui::Id) -> egui::Response {
        ui.interact(rect, id, egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    /// Покрасить кнопку.
    pub fn paint(&self, ui: &mut egui::Ui, rect: egui::Rect) {
        let text = self.text.as_ref();
        let disabled_color = self.idle.gamma_multiply(0.35);
        let (fill, icon_color) = match (self.enabled, self.active) {
            (true, true) => (self.active_color, egui::Color32::BLACK),
            (true, false) => (self.idle, egui::Color32::BLACK),
            // Выключенная: чёрный фон, обводка и глиф вдвое бледнее
            // (альфой, новый цвет не нужен).
            (false, _) => (egui::Color32::BLACK, disabled_color),
        };
        ui.painter().rect_filled(rect, 0.0, fill);
        if !self.enabled {
            // Полная обводка поверх чёрного фона.
            ui.painter().rect_stroke(
                rect,
                0.0,
                egui::Stroke::new(2.0, disabled_color),
                egui::StrokeKind::Inside,
            );
        }
        // Текст пословно (первое слово — иконка, GLYPH_FLIP поддержан):
        // font space шире ячейки, центруем по content_width. Базлайн —
        // на низе кнопки (глиф X2 — две клетки, встаёт точно).
        let w = content_width(ui.ctx(), text);
        let mut pos = egui::pos2(rect.center().x - w / 2.0, rect.bottom());
        for word in text.split(' ').filter(|word| !word.is_empty()) {
            pos.x += paint_word(ui, pos, word, icon_color) + cell_x(1);
        }
    }
}

/// Подсказка-значок в правом нижнем углу anchor: правый нижний угол
/// текста — на клетку влево и вверх от угла anchor, вся плашка сдвинута
/// на пол-высоты клетки вниз (визуальная центровка на картинке).
pub fn hint_badge_corner(ui: &mut egui::Ui, anchor: egui::Rect, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    let row = CELL_SIZE.y;
    let width = lines
        .iter()
        .map(|line| measure_text(ui.ctx(), style::FONT_REGULAR, line).x)
        .fold(0.0f32, f32::max);
    let right = anchor.right() - cell_x(1);
    let bottom = anchor.bottom() - cell_y(1);
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
    let pad = CELL_SIZE.x * 0.5;
    let row = CELL_SIZE.y;
    let bg = text.expand(pad);
    // Чёрная подложка на 1 px шире контура (во все стороны), поверх —
    // контурный скруглённый прямоугольник. Полупрозрачная заливка не
    // используется.
    ui.painter()
        .rect_filled(bg.expand(4.0), pad + 4.0, egui::Color32::BLACK);
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
            style::FONT_REGULAR,
            egui::Color32::WHITE,
        );
    }
}
