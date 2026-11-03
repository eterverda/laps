//! Блок кругов под вьюфайндерами (подмодуль экрана Flight): рамки-зоны
//! (пунктир), символ дисконнекта, таблицы кругов, микро-таймер отсчёта,
//! рамка активного пада. Клик — интентом Selection (None — клика не
//! было): мутацию делает вызывающий (`Review::click` у фазы).

use super::{FrameContents, Selection};
use crate::gui::measure_text;
use crate::gui::ruler::{CELL_SIZE, cell_x, cell_y};
use crate::gui::style;
use crate::model::flight::{LapEvent, LapLine, Stamp, lap_label};
use std::time::Duration;

/// Событие в колонке кругов занимает 2 колонки сетки: глиф (1) и
/// разделитель после него (1). Fira Code моноширинный — advance любого
/// глифа и пробела = клетке (16 pt при 26 pt кегле).
const LAP_EVENT_COLS: isize = 2;

/// Таблицы кругов в нижних блоках колонок (по rect на колонку, в
/// порядке колонок): по строке на круг, сверху вниз; влезают
/// последние. Пустой блок в фазе отсчёта — микро-таймер первого круга
/// (та же пара цветов таймера, тот же сигнал мигания). Рамки-зоны и
/// плейсхолдер (средний блок) рисует экран. Клик по ячейке события
/// возвращает Some((pad_id, мс)); мутация выделения и seek — хозяина.
pub fn show(
    ui: &mut egui::Ui,
    frame: &FrameContents,
    rects: &[egui::Rect],
) -> Option<(String, Stamp)> {
    let mut lap_click = None;
    for (i, second) in rects.iter().enumerate() {
        let Some(pad) = frame.pads.get(i) else {
            continue;
        };
        if !pad.laps.is_empty() {
            let capacity = (second.height() / cell_y(1)).max(0.0) as usize;
            let pad_id = &pad.pad_id;
            let active_ms = match &frame.laps_active {
                Selection::Event(pad, ms) if pad == pad_id => Some(*ms),
                _ => None,
            };
            let rows = lap_table(
                &pad.laps,
                frame.laps_cursor,
                frame.laps_race_start,
                active_ms,
                frame.laps_flash,
                frame.holeshot_enabled,
            );
            let from = rows.len().saturating_sub(capacity);
            // Без клипа: по вертикали строки влезают по
            // построению (capacity), обводки подсветки
            // ничем не обрезаются, а широкая строка пусть
            // пересекает пунктир — лучше, чем съеденное
            // обрезанием время.
            let painter = ui.painter();
            // Колонка событий резервируется по самой
            // широкой строке пада, минимум — под
            // кружок (LAP_EVENT_COLS колонок сетки).
            // Блоки событий выровнены вправо: имя и
            // время между строками на одной линии,
            // по мере событий не прыгают.
            let max_events = rows
                .iter()
                .map(|r| lap_event_cells(r).len())
                .max()
                .unwrap_or(0)
                .max(1);
            for (row, cells) in rows[from..].iter().enumerate() {
                let full_row = from + row;
                let pos = egui::pos2(
                    second.left()
                        + cell_x(
                            LAP_EVENT_COLS * (max_events - lap_event_cells(cells).len()) as isize,
                        ),
                    second.top() + cell_y(row as isize),
                );
                // Геометрия ячеек: позиции для
                // заливок, обводок и кликов.
                let events_len = lap_event_cells(cells).len();
                let mut x = pos.x;
                let mut rects = Vec::new();
                let mut name_x = pos.x;
                for (i, cell) in cells.iter().enumerate() {
                    // Ячейка события — ровно клетка сетки;
                    // после каждой — зазор в клетку.
                    let size = if i < events_len {
                        CELL_SIZE
                    } else {
                        measure_text(ui.ctx(), style::FONT_REGULAR, &cell.text)
                    };
                    let rect = egui::Rect::from_min_size(egui::pos2(x, pos.y), size)
                        .expand2(egui::vec2(6.0, -2.0));
                    rects.push(rect);
                    x += size.x + cell_x(1);
                    if i + 1 == events_len {
                        name_x = x;
                    }
                }
                // События — общей живописью ячейки глифа;
                // имя/время — галлеей: смешанные тона и
                // зачёркивание живут только там.
                for (rect, cell) in rects.iter().zip(cells).take(events_len) {
                    paint_event_cell(
                        &painter,
                        *rect,
                        &cell.text,
                        cell.tone,
                        cell.highlight,
                        cell.active,
                    );
                }
                let galley = painter.layout_job(lap_row_job(&cells[events_len..]));
                painter.galley(egui::pos2(name_x, pos.y), galley, style::PAD_TIMER_ACTIVE);
                for (ci, (rect, cell)) in rects.iter().zip(cells).enumerate() {
                    // Клик: перемотка на время
                    // ячейки (play/RPT не трогаем —
                    // seek их сохраняет). Выделяется
                    // кружок: свой у события,
                    // предыдущий у имени/времени.
                    if frame.laps_clickable && !cell.text.is_empty() {
                        let response = ui.interact(
                            *rect,
                            egui::Id::unique(("lap", i, full_row, ci)),
                            egui::Sense::click(),
                        );
                        if response.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if response.clicked()
                            && let Some(seek) = cell.seek
                        {
                            lap_click = Some((pad_id.clone(), seek));
                        }
                    }
                }
            }
        } else if let Some(dim) = frame.countdown {
            // «0.000» — отсчёт первого круга:
            // та же пара цветов таймера, тот же
            // сигнал мигания. Раскладка как у
            // строки круга: пустые колонки
            // событий (мин. 2) и имени (мин. 2) +
            // время (мин. 6) — позиция совпадает
            // с первым кругом гонки. Имя —
            // предполагаемое первого круга
            // (HS с холшотом, «1)» без). Тон —
            // будущее: круг не завершён;
            // потухшая фаза — гаснущий сигнал
            // отсчёта.
            let tone = if dim {
                LapTone::Inactive
            } else {
                LapTone::Future
            };
            let cells = [
                LapCell::plain(String::new(), tone, None),
                LapCell::plain(String::new(), tone, None),
                // Имя первого круга: база нумерации из правил
                // (0 — «HS», 1 — «1)»).
                LapCell::plain(lap_label(u32::from(!frame.holeshot_enabled)), tone, None),
                LapCell::plain(lap_time(Duration::ZERO, 6), tone, None),
            ];
            let galley = ui.painter().layout_job(lap_row_job(&cells));
            ui.painter().galley(
                egui::pos2(second.left(), second.top()),
                galley,
                style::PAD_TIMER_ACTIVE,
            );
        }
    }
    lap_click
}

/// Тон ячейки: активный (пара таймера), потухший (мигание отсчёта),
/// будущий (ещё не случилось — раскраска DVR по курсору).
#[derive(Clone, Copy)]
pub enum LapTone {
    Active,
    Inactive,
    Future,
}

/// Ячейка строки круга: текст, тон, зачёркивание. `highlight` —
/// вспышка у курсора (окно LAP_FLASH_*, только события): кислотная
/// заливка, глиф чёрный поверх неё. `active` — выделенная
/// кликом ячейка (DVR): голубая обводка; `seek` — её время для
/// перемотки (мс журнала).
struct LapCell {
    text: String,
    tone: LapTone,
    strike: bool,
    highlight: bool,
    active: bool,
    seek: Option<Stamp>,
}

impl LapCell {
    /// Ячейка без зачёркивания и подсветок — таких большинство.
    fn plain(text: impl Into<String>, tone: LapTone, seek: Option<Stamp>) -> Self {
        Self {
            text: text.into(),
            tone,
            strike: false,
            highlight: false,
            active: false,
            seek,
        }
    }
}

/// Глиф события круга (таблица кругов и шкала над таймлайном).
pub fn lap_glyph(event: &LapEvent) -> char {
    match event {
        LapEvent::Fault => '\u{f0156}', // nf-md-close
        LapEvent::Correction => '\u{25a1}',
        LapEvent::Lap => '\u{25cb}',   // полый круг
        LapEvent::Warn => '\u{f0537}', // nf-md-triangle-outline
    }
}

/// Живопись ячейки события (глиф): кислотная заливка вспышки (глиф
/// чёрный поверх неё), голубая обводка выделения. Общая для таблицы
/// кругов и строки событий над таймлайном.
pub fn paint_event_cell(
    painter: &egui::Painter,
    rect: egui::Rect,
    glyph: &str,
    tone: LapTone,
    highlighted: bool,
    selected: bool,
) {
    if highlighted {
        painter.rect_filled(rect.shrink(1.0), 4.0, style::LAP_FLASH);
    }
    let color = if highlighted {
        egui::Color32::BLACK
    } else {
        match tone {
            LapTone::Active => style::PAD_TIMER_ACTIVE,
            LapTone::Inactive => style::PAD_TIMER_INACTIVE,
            LapTone::Future => style::LAP_FUTURE,
        }
    };
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph.to_owned(),
        style::FONT_REGULAR,
        color,
    );
    if selected {
        // Обводка выделения: голубой контур наружу + чёрный внутрь
        // от увеличенного прямоугольника; подсветка выше — без вариантов.
        let outline = rect.expand(1.0);
        painter.rect_stroke(
            outline,
            4.0,
            egui::Stroke::new(3.0, style::LAP_SELECTION),
            egui::StrokeKind::Outside,
        );
        painter.rect_stroke(
            outline,
            4.0,
            egui::Stroke::new(2.0, egui::Color32::BLACK),
            egui::StrokeKind::Inside,
        );
    }
}

/// Ячейки событий строки: последние две ячейки — имя и время.
fn lap_event_cells(cells: &[LapCell]) -> &[LapCell] {
    cells.split_at(cells.len().saturating_sub(2)).0
}

/// Таблица кругов: одна строка на круг, колонки «события имя время»
/// через пробел. События — первая колонка: по глифу на событие, в
/// порядке возникновения слева направо. Рендер резервирует колонку
/// событий по самому широкому блоку пада (минимум — под кружок:
/// глиф + разделитель, 2 колонки сетки) и выравнивает блоки вправо —
/// имя и время между строками на одной линии, по мере событий не
/// прыгают. Имя и время выравнены вправо; минимальные ширины — 2 и 6
/// (без разделителей). Имя — из дистилятора («HS», «1)» — скобку
/// ставит он); текущий несёт предполагаемое имя (своё «HS»/«N)», даже
/// с крестом); проваленный и незакрытый пост-полётный отбиты
/// пробелами (имя пустое, колонка резервируется). Время: формат по
/// наибольшему кругу включая текущий: SS.mmm, далее M:SS.mmm, далее
/// MM:SS.mmm; ведущие нули — пробелами, ширина резервирована.
/// События: fault ✕, correct □, lap ○, warn △.
///
/// Раскраска: `cursor` (мс журнала) — элемент серым, пока его метка
/// в будущем: имя и время круга — до конца круга (белеют одновременно
/// с его кружком; текущий круг не завершён никогда), событие — до
/// его метки. None — без раскраски (эфир: всё в прошлом).
fn lap_table(
    lines: &[(Stamp, LapLine, Vec<(Stamp, LapEvent)>)],
    cursor: Option<Stamp>,
    _race_start: Option<Stamp>,
    active_ms: Option<Stamp>,
    flash: (i64, i64),
    holeshot_enabled: bool,
) -> Vec<Vec<LapCell>> {
    // Время строки; у текущего — бегущее (он участвует и в выборе
    // формата колонки).
    let max_ms = lines
        .iter()
        .map(|(_, line, _)| line.duration())
        .map(|d| d.as_millis())
        .max()
        .unwrap_or(0);
    // Минимум 6 («SS.mmm») — время всегда в полной ширине колонки.
    let time_width = if max_ms >= 600_000 {
        9 // MM:SS.mmm
    } else if max_ms >= 60_000 {
        8 // M:SS.mmm
    } else {
        6 // SS.mmm
    };
    // Текущий круг — предполагаемый номер: завершённые успешные + база
    // нумерации из правил (0 с холшотом — следующий «HS», без — с 1).
    // Fail номер не съедает. Отображение — `lap_label`, как у
    // дистиллятора.
    let successes = lines
        .iter()
        .filter(|(_, line, _)| matches!(line, LapLine::Success { .. }))
        .count();
    let current_name = lap_label(successes as u32 + u32::from(!holeshot_enabled));
    let future = |at: Stamp| cursor.is_some_and(|c| c < at);
    let mut rows = Vec::new();
    for (end, line, events) in lines {
        // Имя — из дистиллятора целиком (HS без скобки, «1)» с ней).
        let name = match line {
            LapLine::Success { num, .. } => num.clone(),
            LapLine::Current { .. } => current_name.clone(),
            // Незакрытый (пост-полёт): без имени — колонка
            // резервируется пустой (как у провала).
            LapLine::Unfinished { .. } => String::new(),
            LapLine::Fail { .. } => String::new(), // зафейленный отбит пробелами
        };
        let tone = |future_mark: bool| {
            if future_mark {
                LapTone::Future
            } else {
                LapTone::Active
            }
        };
        let highlighted = |at: Stamp| {
            cursor.is_some_and(|c| {
                let d = c.ms_since(at);
                d >= flash.0 && d < flash.1
            })
        };
        // Колонка событий — первая: по глифу на событие, в порядке
        // возникновения. Имя и время завершают строку: белеют по
        // метке конца круга (одновременно с его кружком; у текущего
        // круга конца нет — серые всегда). Клик по имени/времени, как
        // и по кружку, — на конец круга. Выделяются только события.
        let mut cells: Vec<LapCell> = events
            .iter()
            .map(|(at, event)| LapCell {
                text: lap_glyph(event).to_string(),
                tone: tone(future(*at)),
                strike: false,
                highlight: highlighted(*at),
                active: active_ms == Some(*at),
                seek: Some(*at),
            })
            .collect();
        let finished = !matches!(line, LapLine::Current { .. } | LapLine::Unfinished { .. });
        // Имя и время белеют по метке конца круга (одновременно с его
        // кружком); у незавершённого конца нет — серые всегда.
        let done_tone = if finished {
            tone(future(*end))
        } else {
            LapTone::Future
        };
        cells.push(LapCell::plain(name, done_tone, Some(*end)));
        cells.push(LapCell {
            strike: matches!(line, LapLine::Fail { .. }),
            ..LapCell::plain(lap_time(line.duration(), time_width), done_tone, Some(*end))
        });
        rows.push(cells);
    }
    // Минимум 2 («HS») — узкие номера («1)») отбиваются пробелами.
    // Имя — предпоследняя ячейка (после колонки событий); колонку
    // событий рендер выравнивает вправо сам — по ширине блоков.
    let name_width = rows
        .iter()
        .flat_map(|r| r.iter().rev().nth(1))
        .map(|c| c.text.chars().count())
        .max()
        .unwrap_or(0)
        .max(2);
    // Выравнивание: имя и время вправо (пробелами).
    for cells in &mut rows {
        let len = cells.len();
        for (i, cell) in cells.iter_mut().enumerate() {
            let width = match len - i {
                2 => name_width,
                1 => time_width,
                _ => continue,
            };
            let pad = width.saturating_sub(cell.text.chars().count());
            cell.text = format!("{}{}", " ".repeat(pad), cell.text);
        }
    }
    rows
}

/// LayoutJob строки круга из ячеек: разделитель — пробел, серое —
/// «ещё не случилось», зачёркнутое время провала.
fn lap_row_job(cells: &[LapCell]) -> egui::text::LayoutJob {
    let format = |cell: &LapCell, strike: bool| {
        // Глиф на кислотной заливке (свежее событие) — чёрный
        // поверх неё, остальные — в обычной паре тонов.
        // Зачёркивание — в цвет текста ячейки.
        let color = if cell.highlight {
            egui::Color32::BLACK
        } else {
            match cell.tone {
                LapTone::Active => style::PAD_TIMER_ACTIVE,
                LapTone::Inactive => style::PAD_TIMER_INACTIVE,
                LapTone::Future => style::LAP_FUTURE,
            }
        };
        egui::TextFormat {
            font_id: style::FONT_REGULAR,
            color,
            strikethrough: if strike && cell.strike {
                egui::Stroke::new(1.5, color)
            } else {
                egui::Stroke::NONE
            },
            ..Default::default()
        }
    };
    let mut job = egui::text::LayoutJob::default();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            job.append(" ", 0.0, format(cell, false));
        }
        // Разделитель — без зачёркивания; паддинг ячейки — с ним.
        job.append(&cell.text, 0.0, format(cell, true));
    }
    job
}

/// Время круга. Порядок: рендер «как есть» (круг короче минуты — без
/// «:» даже в минутном формате), затем ведущие нули целой части —
/// пробелами (последний ноль нуля-секунд сохраняем), затем отбивка
/// пробелами до ширины (минимум 6 — «SS.mmm»).
fn lap_time(dur: Duration, width: usize) -> String {
    let millis = dur.as_millis() as u64;
    let (m, sec, ms) = (millis / 60_000, millis / 1000 % 60, millis % 1000);
    let s = if m > 0 {
        if width >= 9 {
            format!("{m:02}:{sec:02}.{ms:03}")
        } else {
            format!("{m}:{sec:02}.{ms:03}")
        }
    } else {
        format!("{sec:02}.{ms:03}")
    };
    let dot = s.find('.').unwrap();
    let mut chars: Vec<char> = s.chars().collect();
    for c in chars[..dot].iter_mut() {
        if *c == '0' {
            *c = ' ';
        } else {
            break;
        }
    }
    // ноль-секунд: «  .123» не бывает — возвращаем ноль перед точкой
    if chars[..dot].iter().all(|c| *c == ' ') {
        chars[dot - 1] = '0';
    }
    format!(
        "{:>w$}",
        chars.into_iter().collect::<String>(),
        w = width.max(6)
    )
}
