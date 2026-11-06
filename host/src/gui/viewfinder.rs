use crate::gui::ruler::{cell_x, cell_y};
use crate::gui::style;
use crate::gui::view;
use std::sync::OnceLock;

pub struct Viewfinder {
    pub uv: egui::Rect,
}

impl Viewfinder {
    pub fn new(uv: egui::Rect) -> Self {
        Self { uv }
    }

    pub fn show(&self, ui: &mut egui::Ui, contents: ViewfinderContents, rect: egui::Rect) {
        match contents {
            ViewfinderContents::Texture(id) => {
                ui.painter().image(id, rect, self.uv, egui::Color32::WHITE);
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
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewfinderContents {
    Texture(egui::TextureId),
    /// Нет картинки: заводится/открывается/разрыв таймлайна — серый глиф.
    Pending,
    Error,
    Off,
}

/// Метка, имя пилота и цветная рамка вокруг вьюфайндера. Виджету
/// отводится rect пада на клетку шире картинки: поля ½ клетки по
/// бокам — рамка (8 px, Inside), видео — внутри рамки, ничего не
/// вылезает за пределы rect. Таб: угол `corner`; нижние углы —
/// вертикальное зеркало верстки внутри того же rect.
pub struct ViewfinderFrame<'a> {
    pub label: &'a str,
    pub name: &'a str,
    pub color: egui::Color32,
    corner: view::Corner,
}

impl<'a> ViewfinderFrame<'a> {
    /// `corner` — угол таба; нижние углы — вертикальное зеркало
    /// верстки внутри того же rect.
    pub fn new(label: &'a str, name: &'a str, color: egui::Color32, corner: view::Corner) -> Self {
        Self {
            label,
            name,
            color,
            corner,
        }
    }

    pub fn show(
        &self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        contents: impl FnOnce(&mut egui::Ui, egui::Rect),
    ) {
        let inset = cell_x(1) / 2.0;
        let top = matches!(self.corner, view::Corner::TopLeft | view::Corner::TopRight);
        // Верстка — как на главном (верхние табы) либо её вертикальное
        // зеркало (нижние): рамка на 1½ клетки уже бокса по высоте
        // (под таб), текстура ровно на 2 меньше (таб 1¾ + полосы ¼+¼):
        // у верхних табов прижата ¼ к низу, у нижних — ¼ к верху. Все
        // размеры одинаковы, меняется только привязка.
        let frame_rect = if top {
            rect.with_min_y(rect.min.y + cell_y(3) / 2.0)
        } else {
            rect.with_max_y(rect.max.y - cell_y(3) / 2.0)
        };
        ui.painter().rect_stroke(
            frame_rect,
            0.0,
            egui::Stroke::new(inset, self.color),
            egui::StrokeKind::Inside,
        );
        // Видео — прежнего размера, на ¼ клетки ниже/выше полос рамки.
        let viewfinder_rect = frame_rect.shrink2(egui::vec2(inset, inset));

        // Таб: фон — текст плюс клетка паддинга с каждой стороны;
        // угол — corner (сторона рамки × привязка к краю), снаружи/
        // внутри — outside. Имя — сбоку таба (справа для левых
        // привязок, слева для правых).
        let tab_w = cell_x(self.label.len() as isize * 2 + 2);
        let tab_h = cell_y(7) / 4.0;
        let left = matches!(
            self.corner,
            view::Corner::TopLeft | view::Corner::BottomLeft
        );
        let tab_x = if left {
            rect.left()
        } else {
            rect.right() - tab_w
        };
        // Таб 1¾ клетки прижат к своей границе компонента; внутренняя
        // грань перекрывает полосу рамки на ¼. Нижний таб: низ =
        // низу компонента.
        let tab_y = if top {
            rect.top()
        } else {
            rect.bottom() - tab_h
        };
        let label_rect =
            egui::Rect::from_min_size(egui::pos2(tab_x, tab_y), egui::vec2(tab_w, tab_h));
        ui.painter().rect_filled(label_rect, 0.0, self.color);
        ui.painter().text(
            label_rect.center(),
            egui::Align2::CENTER_CENTER,
            self.label,
            style::FONT_REGULAR_X2,
            egui::Color32::WHITE,
        );
        // Имя — сбоку таба (справа для левых привязок, слева для
        // правых), строго по сетке: у верхних табов — верхняя клетка
        // бокса, у нижних — первая клетка под низом бокса (в пределах
        // висящего таба).
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
            self.name,
            style::FONT_REGULAR,
            style::TRANSPORT_ACTIVE,
        );

        contents(ui, viewfinder_rect);
    }
}

pub const FULL_UV: egui::Rect =
    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
static TESTCARD_TEXTURE: OnceLock<egui::TextureHandle> = OnceLock::new();

fn testcard_texture_id(ui: &mut egui::Ui) -> egui::TextureId {
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
