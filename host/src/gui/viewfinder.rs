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
            ViewfinderContents::Pending => {
                let id = testcard_texture_id(ui);
                ui.painter().image(id, rect, FULL_UV, egui::Color32::WHITE);
            }
            ViewfinderContents::Off => {}
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewfinderContents {
    Texture(egui::TextureId),
    Pending,
    Off,
}

const FULL_UV: egui::Rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
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
