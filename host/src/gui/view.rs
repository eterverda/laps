#[derive(Default)]
pub struct Grid {
    marker_step: egui::Vec2,
    marker_size: f32,
    marker_color: egui::Color32,
}

impl Grid {
    pub fn marker_step(mut self, spacing: impl Into<egui::Vec2>) -> Self {
        self.marker_step = spacing.into();
        self
    }

    pub fn marker_size(mut self, size: f32) -> Self {
        self.marker_size = size;
        self
    }

    pub fn marker_color(mut self, color: impl Into<egui::Color32>) -> Self {
        self.marker_color = color.into();
        self
    }

    pub fn show(&self, ui: &mut egui::Ui) {
        if self.marker_step.x <= 0.0
            || self.marker_step.y <= 0.0
            || self.marker_size <= 0.0
            || self.marker_color == egui::Color32::TRANSPARENT
        {
            return;
        }

        let size = ui.available_size();
        let x_count = (size.x / self.marker_step.x).floor() as i32;
        let y_count = (size.y / self.marker_step.y).floor() as i32;
        let half = self.marker_size / 2.0;
        let stroke = egui::Stroke::new(1.0, self.marker_color);

        for x in 0..=x_count {
            for y in 0..=y_count {
                let px = x as f32 * self.marker_step.x;
                let py = y as f32 * self.marker_step.y;

                ui.painter().line_segment(
                    [egui::pos2(px - half, py), egui::pos2(px + half, py)],
                    stroke,
                );
                ui.painter().line_segment(
                    [egui::pos2(px, py - half), egui::pos2(px, py + half)],
                    stroke,
                );
            }
        }
    }
}

pub struct Letterbox {
    min_virtual_size: egui::Vec2,
    max_virtual_size: egui::Vec2,
    outline: egui::Stroke,
}

impl Letterbox {
    pub fn new(min_virual_size: impl Into<egui::Vec2>) -> Self {
        let min_virtual_size = min_virual_size.into();
        Self {
            min_virtual_size,
            max_virtual_size: min_virtual_size,
            outline: egui::Stroke::NONE,
        }
    }

    pub fn max_virtual_height(mut self, height: f32) -> Self {
        self.max_virtual_size.y = height.max(self.min_virtual_size.y);
        self
    }

    pub fn outline(mut self, stroke: impl Into<egui::Stroke>) -> Self {
        self.outline = stroke.into();
        self
    }

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

        if !self.outline.is_empty() {
            ui.painter().rect_stroke(rect, 0.0, self.outline);
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
