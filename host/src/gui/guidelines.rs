pub const ENABLED: bool = true;

/// Ось-линия через весь available rect. `outward` — сдвиг перпендикулярно
/// оси наружу от контента (в пикселях, обычно половина толщины штриха):
/// линия рисуется целиком снаружи границы, а не на ней.
#[inline]
pub fn solid_line(ui: &mut egui::Ui, axis: egui::Direction, coordinate: f32, outward: f32) {
    let available = ui.available_size();
    let (start, end) = match axis {
        egui::Direction::LeftToRight | egui::Direction::RightToLeft => (
            egui::pos2(0.0, coordinate + outward),
            egui::pos2(available.x, coordinate + outward),
        ),
        _ => (
            egui::pos2(coordinate + outward, 0.0),
            egui::pos2(coordinate + outward, available.y),
        ),
    };
    ui.painter().line_segment(
        [start, end],
        egui::Stroke::new(2.0, egui::Color32::from_white_alpha(0x1e)),
    );
}

/// Пунктирный прямоугольник (гайдлайн-зона): стиль как у solid_line.
#[inline]
pub fn dashed_rect(ui: &mut egui::Ui, rect: egui::Rect) {
    let stroke = egui::Stroke::new(2.0, egui::Color32::from_white_alpha(0x1e));
    for [a, b] in [
        [rect.left_top(), rect.right_top()],
        [rect.left_bottom(), rect.right_bottom()],
        [rect.left_top(), rect.left_bottom()],
        [rect.right_top(), rect.right_bottom()],
    ] {
        ui.painter()
            .add(egui::epaint::Shape::dashed_line(&[a, b], stroke, 8.0, 4.0));
    }
}

#[inline]
pub fn hatch_rect(ui: &mut egui::Ui, rect: egui::Rect) {
    let stroke = egui::Stroke::new(2.0, egui::Color32::from_white_alpha(0x0e));
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
        .rect_filled(rect, 0.0, egui::Color32::from_white_alpha(0x02));
}
