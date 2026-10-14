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

/// Маркер зеркалирования глифа: префикс слова в тексте кнопки — первый
/// глиф слова рисуется зеркально по горизонтали. Зеркальных пар у иконок
/// Nerd Fonts нет (exit-to-app, например, есть только со стрелкой
/// вправо), поэтому разворачиваем сами. Кодпоинт — свободная дыра PUA в
/// обоих начертаниях шрифта: глифа у него нет, в раскладку не попадает,
/// ширины не даёт.
pub const GLYPH_FLIP: char = '\u{e6c0}';

/// Слово без маркера GLYPH_FLIP (измеряем и рисуем сам глиф).
fn strip_flip_marker(word: &str) -> &str {
    word.strip_prefix(GLYPH_FLIP).unwrap_or(word)
}

/// Округление позиции к физическим пикселям (паритет с тесселятором
/// текста, у него это метод emath::GuiRounding за пределами крейта).
fn round_to_pixels(pos: egui::Pos2, pixels_per_point: f32) -> egui::Pos2 {
    egui::pos2(
        (pos.x * pixels_per_point).round() / pixels_per_point,
        (pos.y * pixels_per_point).round() / pixels_per_point,
    )
}

/// Слово кнопки на позиции (левый нижний угол): обычное — painter.text,
/// с маркером GLYPH_FLIP — первый глиф зеркально (paint_glyph_flipped),
/// остаток слова следом. Возвращает ширину слова (как measure_text у
/// обычного слова).
fn paint_word(ui: &mut egui::Ui, pos: egui::Pos2, word: &str, color: egui::Color32) -> f32 {
    let Some(rest) = word.strip_prefix(GLYPH_FLIP) else {
        ui.painter().text(
            pos,
            egui::Align2::LEFT_BOTTOM,
            word,
            super::style::FONT_REGULAR_X2,
            color,
        );
        return super::measure_text(ui.ctx(), super::style::FONT_REGULAR_X2, word).x;
    };
    paint_glyph_flipped(ui, pos, rest, color)
}

/// Первый глиф текста зеркально по горизонтали, остаток (если есть) —
/// обычным текстом со стандартным пробелом-ячейкой. Ширина — как у
/// обычного текста: квад того же глифа из шрифтового атласа
/// (TextureId::Managed(0)), u в uv отражена. Позиционирование повторяет
/// painter.text с LEFT_BOTTOM (включая округление к физпикселям).
fn paint_glyph_flipped(
    ui: &mut egui::Ui,
    pos: egui::Pos2,
    text: &str,
    color: egui::Color32,
) -> f32 {
    let font = super::style::FONT_REGULAR_X2;
    let mut chars = text.chars();
    let Some(chr) = chars.next() else {
        return 0.0;
    };
    let rest: String = chars.collect();
    let galley = ui
        .ctx()
        .fonts_mut(|fonts| fonts.layout_no_wrap(chr.to_string(), font.clone(), color));
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

    let advance = glyph.advance_width;
    if !rest.is_empty() {
        let rest_pos = egui::pos2(pos.x + advance + super::grid::cell_x(1), pos.y);
        ui.painter()
            .text(rest_pos, egui::Align2::LEFT_BOTTOM, rest, font, color);
    }
    advance
}

/// Ширина текста кнопки в пикселях — чтобы вызывающий код мог
/// посчитать rect.
pub fn content_width(ctx: &egui::Context, text: &str) -> f32 {
    let words: Vec<&str> = text.split(' ').filter(|word| !word.is_empty()).collect();
    let mut width: f32 = words
        .iter()
        .map(|word| {
            super::measure_text(ctx, super::style::FONT_REGULAR_X2, strip_flip_marker(word)).x
        })
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

/// Отступ контура плашки-стрелки (arrow_plate) от краёв её rect:
/// вершина носа — в inset от края, тело впадины — в inset.
const PLATE_INSET: f32 = super::grid::CELL_SIZE.x / 8.0;

/// Толщина обводки PlatePaint::Outlined.
const PLATE_OUTLINE_WIDTH: f32 = super::grid::CELL_SIZE.x / 8.0;

/// Край плашки-стрелки: геометрия шеврона относительно плашки.
/// Convex — нос наружу, Concave — впадина внутрь; для любого края.
/// Стык цепочки — Convex слева (нос) и Concave справа (впадина).
#[derive(Clone, Copy)]
pub enum PlateEdge {
    ChevronConvex,
    ChevronConcave,
    Flat,
}

/// Как рисовать плашку-стрелку: заливка или только обводка.
pub enum PlatePaint {
    Filled(egui::Color32),
    Outlined(egui::Color32),
}

/// Фон-плашка-стрелка в стиле промпта PowerShell: высота две клетки,
/// края — по PlateEdge, заливка или обводка — по PlatePaint. Контур
/// невыпуклый: epaint заливает веером из первой вершины, поэтому обход
/// всегда начинается с левой средней точки — из неё весь контур виден
/// для любых краёв. Rect приходит целиком (вместе с наконечником
/// и левой частью под соседнюю плашку). Только рисует: текст вызывающий
/// код кладёт по своему rect'у сам.
pub fn arrow_plate(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    left: PlateEdge,
    right: PlateEdge,
    paint: PlatePaint,
) {
    const BUTT: f32 = super::grid::cell_x(1);
    const TIP: f32 = super::grid::cell_x(2);
    const INS: f32 = PLATE_INSET;
    const A_INS: f32 = PLATE_INSET * std::f32::consts::SQRT_2;

    let mid = rect.center().y;
    // (тело, вершина) края: нос — вершина близко к rect (inset), тело
    // глубоко (tip); впадина — наоборот.
    let (left_body, left_apex) = match left {
        PlateEdge::ChevronConvex => (rect.left() + A_INS + TIP, rect.left() + A_INS),
        PlateEdge::ChevronConcave => (rect.left() + A_INS, rect.left() + A_INS + TIP),
        PlateEdge::Flat => (rect.left() + INS + BUTT, rect.left() + INS + BUTT),
    };
    let (right_body, right_apex) = match right {
        PlateEdge::ChevronConvex => (rect.right() - A_INS - TIP, rect.right() - A_INS),
        PlateEdge::ChevronConcave => (rect.right() - A_INS, rect.right() - A_INS - TIP),
        PlateEdge::Flat => (rect.right() - INS - BUTT, rect.right() - INS - BUTT),
    };
    let outline = vec![
        egui::pos2(left_apex, mid),
        egui::pos2(left_body, rect.top()),
        egui::pos2(right_body, rect.top()),
        egui::pos2(right_apex, mid),
        egui::pos2(right_body, rect.bottom()),
        egui::pos2(left_body, rect.bottom()),
    ];
    // Обводка (Outlined) — внутрь: внешний край disabled-плашки
    // совпадает со залитой, контур не вылезает за геометрию цепочки.
    let (fill, stroke) = match paint {
        PlatePaint::Filled(color) => (color, egui::epaint::PathStroke::NONE),
        PlatePaint::Outlined(color) => (
            egui::Color32::TRANSPARENT,
            egui::epaint::PathStroke::new(PLATE_OUTLINE_WIDTH, color).inside(),
        ),
    };
    ui.painter()
        .add(egui::Shape::convex_polygon(outline, fill, stroke));
}

/// Состояние кнопки навигации: покой (неактивные цвета), активная
/// работа (насыщенный цвет), недоступна (контур и текст цветом
/// неактивного фона, клика нет).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NavState {
    Idle,
    Active,
    Disabled,
}

/// Кнопка навигации статус-бара: плашка-стрелка (с уголками) + верхний
/// текст + нижний текст. Верхний текст — слова через пробел-ячейку,
/// первое слово — иконка (поддерживает GLYPH_FLIP-маркер).
/// Нижний текст (доптаймер, "" = без) — по левому краю текста после
/// иконки (+3 колонки), цветом подложки. Базовая высота виджета —
/// 3 клетки: две верхнего текста, одна нижнего. Плашка накрывает
/// только верхние две; нижняя строка (доптекст) — без фона. Размещение
/// по границам текста ± 2 колонки (NAV_AIR); плашка шире ещё на 2 с
/// каждой стороны под уголки (симметрично: nose = tail = 2 колонки).
/// Клик-бласть — весь виджет (все три строки), уголки вне её.
/// Направления уголков приходят снаружи: цепочка рисует кнопки в
/// порядке следования.
pub struct NavButton<T: AsRef<str>, U: AsRef<str>> {
    text: T,
    sub: U,
    state: NavState,
    idle: egui::Color32,
    active: egui::Color32,
    left: PlateEdge,
    right: PlateEdge,
}

/// Rect виджета NavButton: текст ± 2 колонки, +1 клетка вниз (строка
/// sub).
pub fn nav_widget_rect(text: egui::Rect) -> egui::Rect {
    text.expand2(egui::vec2(super::grid::cell_x(2), 0.0))
        .with_max_y(text.bottom() + super::grid::CELL_SIZE.y)
}

/// Rect плашки NavButton: текст ± (2 воздух + 2 под уголки) по
/// горизонтали; по вертикали — только две верхние клетки виджета
/// (нижняя строка с доптекстом без плашки).
pub fn nav_plate_rect(text: egui::Rect) -> egui::Rect {
    text.expand2(egui::vec2(super::grid::cell_x(4), 0.0))
}

impl<T: AsRef<str>, U: AsRef<str>> NavButton<T, U> {
    pub fn new(
        text: T,
        sub: U,
        state: NavState,
        idle: egui::Color32,
        active: egui::Color32,
        left: PlateEdge,
        right: PlateEdge,
    ) -> Self {
        Self {
            text,
            sub,
            state,
            idle,
            active,
            left,
            right,
        }
    }

    /// Покрасить и, если кнопка не Disabled, принять клик. None —
    /// недоступна (только покраска).
    pub fn show(
        &self,
        ui: &mut egui::Ui,
        text_rect: egui::Rect,
        id: egui::Id,
    ) -> Option<egui::Response> {
        let text = self.text.as_ref();
        let sub = self.sub.as_ref();
        let widget = nav_widget_rect(text_rect);
        let plate = nav_plate_rect(text_rect);
        let (paint, text_color) = match self.state {
            NavState::Idle => (PlatePaint::Filled(self.idle), egui::Color32::BLACK),
            NavState::Active => (PlatePaint::Filled(self.active), egui::Color32::BLACK),
            NavState::Disabled => (PlatePaint::Outlined(self.idle), self.idle),
        };
        arrow_plate(ui, plate, self.left, self.right, paint);
        // Верхний текст — пословно (как считает content_width): иконка,
        // затем ровно колонка пробела, затем слово. Одним painter.text
        // нельзя: пробел шрифта шире ячейки и текст не ложится в rect.
        let mut pos = text_rect.left_bottom();
        for word in text.split(' ').filter(|word| !word.is_empty()) {
            pos.x += paint_word(ui, pos, word, text_color) + super::grid::cell_x(1);
        }
        if !sub.is_empty() {
            let sub_color = match self.state {
                NavState::Active => self.active,
                _ => self.idle,
            };
            ui.painter().text(
                egui::pos2(text_rect.left() + super::grid::cell_x(3), widget.bottom()),
                egui::Align2::LEFT_BOTTOM,
                sub,
                super::style::FONT_REGULAR,
                sub_color,
            );
        }
        if self.state == NavState::Disabled {
            return None;
        }
        Some(
            ui.interact(widget, id, egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand),
        )
    }
}

/// Состояние заливки транспортной кнопки: покой / активна (нажата).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TransportState {
    Idle,
    Active,
}

/// Кнопка транспорта (scrub): залитый прямоугольник 3 клетки высотой,
/// верхний текст (иконка) по центру верхних двух клеток, нижний текст —
/// в третьей клетке, с отступом в колонку с каждой стороны (не касается
/// краёв кнопки). Текст всегда чёрный, цвета заливки приходят снаружи.
/// Клик-бласть — rect целиком (уголков нет). interact и paint раздельны:
/// между ними кладётся логика нажатия, чтобы кадр клика рисовался уже
/// нажатым (иначе вспышка нажатия теряется между кадрами).
pub struct TransportButton<T: AsRef<str>, U: AsRef<str>> {
    text: T,
    sub: U,
    state: TransportState,
    idle: egui::Color32,
    active: egui::Color32,
}

impl<T: AsRef<str>, U: AsRef<str>> TransportButton<T, U> {
    pub fn new(
        text: T,
        sub: U,
        state: TransportState,
        idle: egui::Color32,
        active: egui::Color32,
    ) -> Self {
        Self {
            text,
            sub,
            state,
            idle,
            active,
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
        let sub = self.sub.as_ref();
        ui.painter().rect_filled(
            rect,
            0.0,
            match self.state {
                TransportState::Idle => self.idle,
                TransportState::Active => self.active,
            },
        );
        let icon_w = super::measure_text(ui.ctx(), super::style::FONT_REGULAR_X2, text).x;
        ui.painter().text(
            egui::pos2(
                rect.center().x - icon_w / 2.0,
                rect.top() + super::grid::cell_y(1),
            ),
            egui::Align2::LEFT_CENTER,
            text,
            super::style::FONT_REGULAR_X2,
            egui::Color32::BLACK,
        );
        if !sub.is_empty() {
            let sub_rect = rect.shrink2(egui::vec2(super::grid::cell_x(1), 0.0));
            ui.painter().text(
                egui::pos2(sub_rect.center().x, rect.bottom()),
                egui::Align2::CENTER_BOTTOM,
                sub,
                super::style::FONT_REGULAR,
                egui::Color32::BLACK,
            );
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
