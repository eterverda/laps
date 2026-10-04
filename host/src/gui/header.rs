use super::grid::{self, IntoCell};
use super::style;
use super::view::{self, Corner};
use time::format_description::{Component, FormatItem};

/// Фон плашки LAPS и даты — colorSecondaryLight из laps-bar.
const TITLE_BG_LIGHT: egui::Color32 = egui::Color32::from_rgb(0x8C, 0xAD, 0xD1);
/// Фон временной плашки — colorSecondaryDark из laps-bar.
const TITLE_BG_DARK: egui::Color32 = egui::Color32::from_rgb(0x5B, 0x7F, 0xA9);

const DATE_FORMAT: &str = "[year]-[month]-[day] [weekday format:mn]";
const TIME_FORMAT: &str = "[hour]:[minute]:[second].[subsecond digits:3]";

/// Заголовок экрана: LAPS слева, дата и время с ms справа. Виджету
/// отдаётся rect во всю ширину контейнера и 2 клетки высотой — внутри
/// он сам раскладывает левую и правую части по краям.
pub struct Header {
    date_clock: Clock,
    time_clock: Clock,
}

impl Default for Header {
    fn default() -> Self {
        Self::new()
    }
}

impl Header {
    pub fn new() -> Self {
        Self {
            date_clock: Clock::new(DATE_FORMAT),
            time_clock: Clock::new(TIME_FORMAT),
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        // Левый край: LAPS — полоса 12 клеток (2 обрезочка + 1 поле +
        // 8 текст + 1 поле), две клетки высотой, от левого края rect'а.
        let title_rect = rect.left_top().into_cell().extrude(12, 2);
        view::StripeX2::new(TITLE_BG_LIGHT, Corner::TopLeft).show(ui, title_rect, |ui, rect| {
            ui.painter().text(
                rect.left_top(),
                egui::Align2::LEFT_TOP,
                "LAPS",
                style::FONT_REGULAR_X2,
                egui::Color32::BLACK,
            );
        });

        // Правый край: дата (ряд 0) и время с ms (ряд 1) у правого края
        // rect'а. Полные rect'ы полос: 1 обрезочка + 1 поле + текст +
        // 1 поле; текст рисуют часы в замыкании.
        let right = rect.right_top().into_cell().col;
        let row = rect.left_top().into_cell().row;
        let date_rect = grid::cell(right - 16, row).extrude(16, 1);
        let time_rect = grid::cell(right - 15, row + 1).extrude(15, 1);
        view::Stripe::new(TITLE_BG_LIGHT, Corner::TopRight).show(ui, date_rect, |ui, rect| {
            self.date_clock.show(ui, rect, egui::Color32::BLACK);
        });
        view::Stripe::new(TITLE_BG_DARK, Corner::TopRight).show(ui, time_rect, |ui, rect| {
            self.time_clock.show(ui, rect, egui::Color32::BLACK);
        });
    }
}

/// Плейсхолдер дня недели в форматной строке. Хак: `time` такого
/// компонента не знает, поэтому Clock вырезает его перед парсингом, а
/// после форматирования дописывает в конец двухбуквенное русское
/// сокращение дня (нижний регистр) из RU_DAYS. Держится в конце строки.
const WEEKDAY: &str = "[weekday format:mn]";

/// Двухбуквенные русские названия дней недели (нижний регистр), от
/// понедельника — для плейсхолдера WEEKDAY.
static RU_DAYS: [&str; 7] = ["пн", "вт", "ср", "чт", "пт", "сб", "вс"];

/// Часы с заданной форматной строкой `time`. Свой repaint взводится по
/// самому мелкому отображаемому делению формата (ms для `[subsecond]`,
/// секунда для `[second]`, минута для `[minute]` и т.д.) и показывает
/// время, сняпленное ровно на границу деления (чистые `.000` у секундных
/// форматов). Если repaint пришёл раньше своего тика (кадр камеры, ввод,
/// ...), показывают точное время этого repaint.
struct Clock {
    next_tick: Option<time::OffsetDateTime>,
    format: Box<[time::format_description::FormatItem<'static>]>,
    /// Самое мелкое деление формата, в миллисекундах.
    tick_ms: i64,
    /// В форматной строке был плейсхолдер дня недели.
    weekday: bool,
}

impl Clock {
    fn new(format: &'static str) -> Self {
        let (format, weekday) = match format.strip_suffix(WEEKDAY) {
            Some(head) => (head.trim_end(), true),
            None => (format, false),
        };
        let format = time::format_description::parse_borrowed::<1>(format)
            .expect("static format description");
        Self {
            next_tick: None,
            tick_ms: tick_ms(&format),
            format: format.into(),
            weekday,
        }
    }

    fn show(&mut self, ui: &mut egui::Ui, rect: egui::Rect, color: egui::Color32) {
        let now =
            time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());

        let shown = match self.next_tick {
            // Repainted earlier than our scheduled tick: someone else
            // (camera frame, input event) drove this repaint — show the
            // exact time of this repaint.
            Some(tick) if now < tick => now,
            // Our own scheduled repaint (or the very first one): show the
            // time snapped to the division boundary and schedule the next
            // boundary.
            _ => {
                let unix_ms = now.unix_timestamp_nanos() / 1_000_000;
                let tick = self.tick_ms as i128;
                // Снап по unix-времени возвращает UTC — возвращаем локальный
                // сдвиг now, иначе на своём тике часы показывают UTC.
                let boundary = time::OffsetDateTime::from_unix_timestamp_nanos(
                    unix_ms / tick * tick * 1_000_000,
                )
                .expect("tick boundary")
                .to_offset(now.offset());
                self.next_tick = Some(boundary + time::Duration::milliseconds(self.tick_ms));
                boundary
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

        let mut text = shown
            .format(self.format.as_ref())
            .expect("static format description");
        if self.weekday {
            let day = usize::from(shown.weekday().number_from_monday()) - 1;
            text = format!("{} {}", text, RU_DAYS[day]);
        }

        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            text,
            style::FONT_REGULAR,
            color,
        );
    }
}

/// Самое мелкое деление формата, в миллисекундах: формат меняется
/// только когда меняется его самое мелкое поле, чаще взводить repaint
/// бессмысленно.
fn tick_ms(format: &[FormatItem<'static>]) -> i64 {
    use time::format_description::modifier;
    let mut tick = i64::MAX;
    for item in format {
        let FormatItem::Component(component) = item else {
            continue;
        };
        let candidate = match component {
            Component::Hour12(_) | Component::Hour24(_) => 3_600_000,
            Component::Minute(_) => 60_000,
            Component::Second(_) => 1_000,
            Component::Subsecond(subsecond) => match subsecond.digits {
                modifier::SubsecondDigits::One => 100,
                modifier::SubsecondDigits::Two => 10,
                _ => 1,
            },
            // Период (AM/PM) меняется не чаще часа.
            Component::Period(_) => 3_600_000,
            // Всё остальное (день, год, месяц, день недели, номера недель,
            // будущие варианты) — не мельче суток.
            _ => 24 * 3_600_000,
        };
        tick = tick.min(candidate);
    }
    tick.max(1_000)
}
