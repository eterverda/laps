use crate::egui_ext::UiExt;
use crate::gui::ruler::{cell_x, cell_y};
use crate::gui::style;
use crate::gui::view;
use std::sync::OnceLock;

/// Содержимое картинки пада: текстура кадра с UV-вьюпортом камеры
/// (кадр текстуры, не экрана) либо состояние без картинки.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewfinderContents {
    Texture {
        id: egui::TextureId,
        uv: egui::Rect,
    },
    /// Камера заводится/плеер открывается/разрыв таймлайна — серый глиф.
    Pending,
    Error,
    Off,
}

/// Виджет-картинка пада над контентом. Rect — max_rect вырезанного
/// коллером Ui (обмер — снаружи, у хостящего фрейма).
#[derive(Clone, Copy)]
pub struct Viewfinder {
    content: ViewfinderContents,
}

impl Viewfinder {
    pub fn new(content: ViewfinderContents) -> Self {
        Self { content }
    }
}

impl egui::Widget for Viewfinder {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        let response = ui.allocate_rect(ui.max_rect(), egui::Sense::hover());
        let rect = response.rect;
        match self.content {
            ViewfinderContents::Texture { id, uv } => {
                ui.painter().image(id, rect, uv, egui::Color32::WHITE);
            }
            // Нет картинки (камера заводится, плеер открывается, разрыв
            // записи на таймлайне): тесткард-заглушка.
            ViewfinderContents::Pending => {
                let id = testcard_texture_id(ui);
                ui.painter().image(id, rect, FULL_UV, egui::Color32::WHITE);
            }
            // Ошибка камеры: плейсхолдер-плюс по центру, красный.
            ViewfinderContents::Error => {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "\u{ead0}",
                    style::FONT_REGULAR_X2,
                    egui::Color32::from_rgb(0xEA, 0x31, 0x23),
                );
            }
            ViewfinderContents::Off => {}
        }
        response
    }
}

/// Вьюфайндер с рамкой. Rect компонента — max_rect Ui, вырезанного
/// коллером. Рисует цветную обводку, таб в углу `corner` (нижние углы
/// — вертикальное зеркало верстки), имя пилота; содержимое видео
/// хостит замыканием (чилдрн получает дочерний Ui под video_rect).
/// Обмер снаружи: бокс = видео (+1 колонка рамки, +2 строки: таб 1¾ +
/// полоса ¼).
#[derive(Clone)]
pub struct ViewfinderFrameContents {
    pub label: String,
    pub name: String,
    pub color: egui::Color32,
}

pub struct ViewfinderFrame {
    content: ViewfinderFrameContents,
    corner: view::Corner,
}

impl ViewfinderFrame {
    pub fn new(content: ViewfinderFrameContents) -> Self {
        Self {
            content,
            corner: view::Corner::TopLeft,
        }
    }

    /// Позиция таба: угол рамки. По умолчанию TopLeft. Для нижних
    /// углов вёрстка зеркалится: таб к нижнему краю, рамка — к верхнему.
    pub fn corner(mut self, corner: view::Corner) -> Self {
        self.corner = corner;
        self
    }

    pub fn show<R>(
        &self,
        ui: &mut egui::Ui,
        add_contents: impl FnOnce(&mut egui::Ui) -> R,
    ) -> egui::InnerResponse<R> {
        let rect = ui.max_rect();
        let top = matches!(self.corner, view::Corner::TopLeft | view::Corner::TopRight);
        let left = matches!(
            self.corner,
            view::Corner::TopLeft | view::Corner::BottomLeft
        );
        let inset = cell_x(1) / 2.0;
        // Верстка — как на главном (верхние табы) либо её вертикальное
        // зеркало (нижние): рамка на 1½ клетки уже бокса по высоте (под
        // таб), текстура ровно на 2 меньше (таб 1¾ + полосы ¼+¼): у
        // верхних табов прижата ¼ к низу, у нижних — ¼ к верху. Все
        // размеры одинаковы, меняется только привязка.
        let frame_rect = if top {
            rect.with_min_y(rect.min.y + cell_y(3) / 2.0)
        } else {
            rect.with_max_y(rect.max.y - cell_y(3) / 2.0)
        };
        ui.painter().rect_stroke(
            frame_rect,
            0.0,
            egui::Stroke::new(inset, self.content.color),
            egui::StrokeKind::Inside,
        );
        // Видео — внутри рамки, 8 px по кругу от полос.
        let video_rect = frame_rect.shrink2(egui::vec2(inset, inset));

        // Таб: фон — текст плюс клетка паддинга с каждой стороны.
        // Имя — сбоку таба (справа для левых привязок, слева для
        // правых), строго по сетке: у верхних табов — верхняя клетка
        // бокса, у нижних — первая клетка под низом бокса.
        let tab_w = cell_x(self.content.label.len() as isize * 2 + 2);
        let tab_h = cell_y(7) / 4.0;
        let tab_x = if left {
            rect.left()
        } else {
            rect.right() - tab_w
        };
        let tab_y = if top {
            rect.top()
        } else {
            rect.bottom() - tab_h
        };
        let label_rect =
            egui::Rect::from_min_size(egui::pos2(tab_x, tab_y), egui::vec2(tab_w, tab_h));
        ui.painter()
            .rect_filled(label_rect, 0.0, self.content.color);
        ui.painter().text(
            label_rect.center(),
            egui::Align2::CENTER_CENTER,
            self.content.label.as_str(),
            style::FONT_REGULAR_X2,
            egui::Color32::WHITE,
        );
        let name_y = if top {
            label_rect.top()
        } else {
            rect.bottom() - cell_y(1)
        };
        let (name_pos, name_align) = if left {
            (
                egui::pos2(label_rect.right() + cell_x(2), name_y),
                egui::Align2::LEFT_TOP,
            )
        } else {
            (
                egui::pos2(label_rect.left() - cell_x(2), name_y),
                egui::Align2::RIGHT_TOP,
            )
        };
        ui.painter().text(
            // Имя пилота: пластиковым, ровно по клетке.
            name_pos,
            name_align,
            self.content.name.as_str(),
            style::FONT_REGULAR,
            style::TRANSPORT_ACTIVE,
        );
        // Видео — дочерний Ui, ограниченный video_rect.
        ui.place_ui(video_rect, add_contents)
    }
}

const FULL_UV: egui::Rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));

fn testcard_texture_id(ui: &mut egui::Ui) -> egui::TextureId {
    static TESTCARD_TEXTURE: OnceLock<egui::TextureHandle> = OnceLock::new();

    TESTCARD_TEXTURE
        .get_or_init(|| {
            let tree = resvg::usvg::Tree::from_data(
                crate::assets::TESTCARD_SVG,
                &resvg::usvg::Options::default(),
            )
            .expect("failed to parse testcard.svg");

            let size = tree.size();
            let w = size.width() as u32;
            let h = size.height() as u32;

            let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h).expect("failed to create pixmap");
            resvg::render(
                &tree,
                resvg::tiny_skia::Transform::default(),
                &mut pixmap.as_mut(),
            );

            let pixels: Vec<egui::Color32> = bytemuck::cast_slice(pixmap.data()).to_vec();

            ui.ctx().load_texture(
                "stripes",
                egui::ColorImage::new([w as usize, h as usize], pixels),
                egui::TextureOptions::NEAREST,
            )
        })
        .id()
}
