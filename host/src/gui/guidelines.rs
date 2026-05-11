pub const ENABLED: bool = true;

#[inline]
pub fn marker_grid(ui: &mut egui::Ui, step: egui::Vec2) {
    let size = ui.available_size();
    let x_count = (size.x / step.x).floor() as i32;
    let y_count = (size.y / step.y).floor() as i32;
    let half = 5.0;
    let stroke = egui::Stroke::new(1.0, egui::Color32::from_white_alpha(0x0f));

    for x in 0..=x_count {
        for y in 0..=y_count {
            let px = x as f32 * step.x;
            let py = y as f32 * step.y;

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

#[inline]
pub fn dashed_line(ui: &mut egui::Ui, axis: egui::Direction, coordinate: f32) {
    let available = ui.available_size();
    let (start, end) = match axis {
        egui::Direction::LeftToRight | egui::Direction::RightToLeft => (
            egui::pos2(0.0, coordinate - 0.5),
            egui::pos2(available.x, coordinate - 0.5),
        ),
        _ => (
            egui::pos2(coordinate - 0.5, 0.0),
            egui::pos2(coordinate - 0.5, available.y),
        ),
    };
    ui.painter().add(egui::epaint::Shape::dashed_line(
        &[start, end],
        egui::Stroke::new(1.0, egui::Color32::from_white_alpha(0x0f)),
        8.0,
        4.0,
    ));
}

#[inline]
pub fn solid_line(ui: &mut egui::Ui, axis: egui::Direction, coordinate: f32) {
    let available = ui.available_size();
    let (start, end) = match axis {
        egui::Direction::LeftToRight | egui::Direction::RightToLeft => (
            egui::pos2(0.0, coordinate),
            egui::pos2(available.x, coordinate),
        ),
        _ => (
            egui::pos2(coordinate, 0.0),
            egui::pos2(coordinate, available.y),
        ),
    };
    ui.painter().line_segment(
        [start, end],
        egui::Stroke::new(1.0, egui::Color32::from_white_alpha(0x0f)),
    );
}

#[inline]
pub fn hatch_rect(ui: &mut egui::Ui, rect: egui::Rect) {
    let stroke = egui::Stroke::new(1.0, egui::Color32::from_white_alpha(0x07));
    let step = 12.0;
    let w = rect.width();
    let h = rect.height();
    let count = ((w + h) / step).ceil() as i32;
    for i in 0..count {
        let off = i as f32 * step;
        let a = egui::pos2(rect.min.x, rect.min.y + off);
        let b = egui::pos2(rect.min.x + off, rect.min.y);
        let a = if a.y > rect.max.y {
            egui::pos2(a.x + (a.y - rect.max.y), rect.max.y)
        } else {
            a
        };
        let b = if b.x > rect.max.x {
            egui::pos2(rect.max.x, b.y + (b.x - rect.max.x))
        } else {
            b
        };
        ui.painter().line_segment([a, b], stroke);
    }

    ui.painter()
        .rect_filled(rect, 0.0, egui::Color32::from_white_alpha(0x01));
}
