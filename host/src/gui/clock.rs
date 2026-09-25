/// A clock that repaints itself once per second and shows a clean `.000`.
/// If a repaint happens to arrive before the scheduled tick (camera frame,
/// user input, ...), it shows the exact time of that repaint instead.
pub struct Clock {
    next_tick: Option<time::OffsetDateTime>,
}

impl Clock {
    pub fn new() -> Self {
        Self { next_tick: None }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        let now =
            time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());

        let millis = match self.next_tick {
            // Repainted earlier than our scheduled tick: someone else
            // (camera frame, input event) drove this repaint.
            Some(tick) if now < tick => now.millisecond(),
            // Our own scheduled repaint (or the very first one): show a
            // clean second and schedule the next second boundary.
            _ => {
                let next = now.replace_nanosecond(0).unwrap() + time::Duration::SECOND;
                self.next_tick = Some(next);
                0
            }
        };

        // egui forgets repaint delays at the start of every pass, so this
        // must be re-armed each frame, not only when we set a new tick.
        // egui also subtracts `predicted_dt` from the delay before arming the
        // timer (to avoid overshooting); add it back so the repaint lands on
        // the boundary instead of a frame earlier.
        if let Some(tick) = self.next_tick {
            let predicted = std::time::Duration::from_secs_f32(ui.ctx().input(|i| i.predicted_dt));
            let until_tick = std::time::Duration::try_from(tick - now)
                .unwrap_or(std::time::Duration::ZERO)
                .saturating_add(predicted);
            ui.ctx().request_repaint_after(until_tick);
        }

        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            format_timestamp(now, millis),
            super::style::FONT_REGULAR,
            egui::Color32::GRAY,
        );
    }
}

fn format_timestamp(dt: time::OffsetDateTime, millis: u16) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        millis,
    )
}
