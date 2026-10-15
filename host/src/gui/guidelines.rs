pub const ENABLED: bool = true;

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
        egui::Stroke::new(2.0, egui::Color32::from_white_alpha(0x0f)),
    );
}

#[inline]
pub fn hatch_rect(ui: &mut egui::Ui, rect: egui::Rect) {
    let stroke = egui::Stroke::new(2.0, egui::Color32::from_white_alpha(0x07));
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
