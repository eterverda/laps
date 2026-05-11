#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rounding {
    #[allow(dead_code)]
    Ceil,
    #[allow(dead_code)]
    Floor,
    #[allow(dead_code)]
    Round,
    #[allow(dead_code)]
    Trunk,
}

impl Rounding {
    #[inline]
    pub const fn apply(self, v: f32) -> f32 {
        match self {
            Self::Ceil => v.ceil(),
            Self::Floor => v.floor(),
            Self::Round => v.round(),
            Self::Trunk => v.trunc(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct GridCalc {
    cell_size: egui::Vec2,

    marker: egui::Pos2,
    pos: egui::Pos2,
}

impl GridCalc {
    #[inline]
    pub const fn new(cell_w: f32, cell_h: f32) -> Self {
        Self {
            cell_size: egui::vec2(cell_w, cell_h),
            marker: egui::Pos2::ZERO,
            pos: egui::Pos2::ZERO,
        }
    }

    #[inline]
    #[allow(dead_code)]
    pub const fn absolute(&mut self, col: usize, row: usize) -> &mut GridCalc {
        self.absolute_col(col).absolute_row(row)
    }

    #[inline]
    pub const fn absolute_col(&mut self, col: usize) -> &mut GridCalc {
        self.pos.x = col as f32 * self.cell_size.x;
        self
    }

    #[inline]
    pub const fn absolute_row(&mut self, row: usize) -> &mut GridCalc {
        self.pos.y = row as f32 * self.cell_size.y;
        self
    }

    #[inline]
    #[allow(dead_code)]
    pub fn absolute_pos2(&mut self, pos: impl Into<egui::Pos2>) -> &mut GridCalc {
        self.pos = pos.into();
        self
    }

    #[inline]
    #[allow(dead_code)]
    pub const fn absolute_px(&mut self, x: f32, y: f32) -> &mut GridCalc {
        self.pos.x = x;
        self.pos.y = y;
        self
    }

    #[inline]
    #[allow(dead_code)]
    pub const fn absolute_x(&mut self, x: f32) -> &mut GridCalc {
        self.pos.x = x;
        self
    }

    #[inline]
    #[allow(dead_code)]
    pub const fn absolute_y(&mut self, y: f32) -> &mut GridCalc {
        self.pos.y = y;
        self
    }

    #[inline]
    #[allow(dead_code)]
    pub const fn relative(&mut self, dcol: isize, drow: isize) -> &mut GridCalc {
        self.pos.x += dcol as f32 * self.cell_size.x;
        self.pos.y += drow as f32 * self.cell_size.y;
        self
    }

    #[inline]
    #[allow(dead_code)]
    pub const fn relative_px(&mut self, dx: f32, dy: f32) -> &mut GridCalc {
        self.pos.x += dx;
        self.pos.y += dy;
        self
    }

    #[inline]
    #[allow(dead_code)]
    pub fn relative_vec2(&mut self, pos: impl Into<egui::Vec2>) -> &mut GridCalc {
        self.pos += pos.into();
        self
    }

    #[inline]
    #[allow(dead_code)]
    pub const fn mark(&mut self) -> &mut GridCalc {
        self.marker = self.pos;
        self
    }

    #[inline]
    #[allow(dead_code)]
    pub const fn snap(&mut self, rounding: Rounding) -> &mut GridCalc {
        self.snap_col(rounding).snap_row(rounding)
    }

    #[inline]
    pub const fn snap_col(&mut self, rounding: Rounding) -> &mut GridCalc {
        self.pos.x =
            rounding.apply(rounding.apply(self.pos.x / self.cell_size.x) * self.cell_size.x);
        self
    }

    #[inline]
    pub const fn snap_row(&mut self, rounding: Rounding) -> &mut GridCalc {
        self.pos.y =
            rounding.apply(rounding.apply(self.pos.y / self.cell_size.y) * self.cell_size.y);
        self
    }

    #[inline]
    #[allow(dead_code)]
    pub const fn snap_px(&mut self, rounding: Rounding) -> &mut GridCalc {
        self.snap_x(rounding).snap_y(rounding)
    }

    #[inline]
    pub const fn snap_x(&mut self, rounding: Rounding) -> &mut GridCalc {
        self.pos.x = rounding.apply(self.pos.x);
        self
    }

    #[inline]
    pub const fn snap_y(&mut self, rounding: Rounding) -> &mut GridCalc {
        self.pos.y = rounding.apply(self.pos.y);
        self
    }

    #[inline]
    pub const fn size_px(&self, cols: usize, rows: usize) -> egui::Vec2 {
        egui::vec2(self.width_px(cols), self.height_px(rows))
    }

    #[inline]
    pub const fn width_px(&self, cols: usize) -> f32 {
        cols as f32 * self.cell_size.x
    }

    #[inline]
    pub const fn height_px(&self, rows: usize) -> f32 {
        rows as f32 * self.cell_size.y
    }

    #[inline]
    pub fn show<R>(
        &self,
        ui: &mut egui::Ui,
        content: impl FnOnce(&mut egui::Ui) -> R,
    ) -> egui::InnerResponse<R> {
        let rect = egui::Rect::from_min_max(self.marker, self.pos);

        let layout = egui::Layout::centered_and_justified(egui::Direction::LeftToRight);
        let mut child_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout));
        let inner = content(&mut child_ui);
        let response = child_ui.response();
        egui::InnerResponse::new(inner, response)
    }
}

#[derive(Default)]
pub struct Grid {
    marker_step: egui::Vec2,
    marker_size: f32,
    marker_color: egui::Color32,
}

impl Grid {
    #[inline]
    pub fn marker_step(mut self, spacing: impl Into<egui::Vec2>) -> Self {
        self.marker_step = spacing.into();
        self
    }

    #[inline]
    pub fn marker_size(mut self, size: f32) -> Self {
        self.marker_size = size;
        self
    }

    #[inline]
    pub fn marker_color(mut self, color: impl Into<egui::Color32>) -> Self {
        self.marker_color = color.into();
        self
    }

    #[inline]
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
    #[inline]
    pub fn new(min_virual_size: impl Into<egui::Vec2>) -> Self {
        let min_virtual_size = min_virual_size.into();
        Self {
            min_virtual_size,
            max_virtual_size: min_virtual_size,
            outline: egui::Stroke::NONE,
        }
    }

    #[inline]
    pub fn max_virtual_height(mut self, height: f32) -> Self {
        self.max_virtual_size.y = height.max(self.min_virtual_size.y);
        self
    }

    #[inline]
    pub fn outline(mut self, stroke: impl Into<egui::Stroke>) -> Self {
        self.outline = stroke.into();
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
