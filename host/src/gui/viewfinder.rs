use crate::gui::ruler::{IntoRuler, cell_x, cell_y};
use crate::gui::style;
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

/// Метка, имя пилота и цветная рамка вокруг вьюфайндера. Виджету отводится
/// rect на 2 строки выше вьюфайндера: верхние 2 строки — метка с именем,
/// остальное передаётся замыканию под картинку. Рамка рисуется
/// StrokeKind::Outside — фактически за пределами rect виджета.
pub struct ViewfinderFrame<'a> {
    pub label: &'a str,
    pub name: &'a str,
    pub color: egui::Color32,
}

impl<'a> ViewfinderFrame<'a> {
    pub fn new(label: &'a str, name: &'a str, color: egui::Color32) -> Self {
        Self { label, name, color }
    }

    pub fn show(
        &self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        contents: impl FnOnce(&mut egui::Ui, egui::Rect),
    ) {
        let viewfinder_rect = rect.with_min_y(rect.min.y + cell_y(2));
        ui.painter().rect_stroke(
            viewfinder_rect,
            0.0,
            egui::Stroke::new(cell_x(1) / 2.0, self.color),
            egui::StrokeKind::Outside,
        );

        let label_rect: egui::Rect = viewfinder_rect
            .left_top()
            .into_ruler()
            .extrude(cell_x(self.label.len() as isize * 2), cell_y(-2))
            .into();
        ui.painter().rect_filled(
            label_rect.expand2(egui::vec2(cell_x(1) / 2.0, 0.0)),
            0.0,
            self.color,
        );
        ui.painter().text(
            label_rect.center(),
            egui::Align2::CENTER_CENTER,
            self.label,
            style::FONT_REGULAR_X2,
            egui::Color32::WHITE,
        );

        let name_rect: egui::Rect = label_rect
            .right_top()
            .into_ruler()
            .translate(cell_x(2), 0.0)
            .extrude(0.0, cell_y(1))
            .into();
        ui.painter().text(
            name_rect.left_top(),
            egui::Align2::LEFT_TOP,
            self.name,
            style::FONT_REGULAR,
            egui::Color32::WHITE,
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
