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
