//! Блок кругов под вьюфайндерами (подмодуль экрана Flight): рамки-зоны
//! (пунктир), символ дисконнекта, таблицы кругов, микро-таймер отсчёта,
//! рамка активного пада. Клик — интентом Selection (None — клика не
//! было): мутацию делает вызывающий (`Review::click` у фазы).

use super::{FrameContents, Selection};
use crate::gui::measure_text;
use crate::gui::ruler::{CELL_SIZE, IntoRuler, cell_x, cell_y};
use crate::gui::style;
use crate::model::flight::{LapEvent, LapLine, Stamp};
use std::time::Duration;

/// Событие в колонке кругов занимает 2 колонки сетки: глиф (1) по
/// центру двухклеточной ячейки. Разделителей между событиями нет,
/// до текста (номера круга) — пробел. Fira Code моноширинный —
/// advance любого глифа и пробела = клетке (16 pt при 26 pt кегле).
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
            // Две нижние строки занимает полоса места.
            let capacity = (second.height() / cell_y(1) - 2.0).max(0.0) as usize;
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
                let events_len = lap_event_cells(cells).len();
                // Блок событий прижат вправо (резерв по самой широкой
                // строке): 2 клетки на событие, вплотную; до текста —
                // пробел (галлея ниже стартует на клетку правее).
                let start_col = LAP_EVENT_COLS * (max_events - events_len) as isize;
                let name_x = second.left() + cell_x(start_col + 2 * events_len as isize);
                let row_top = second.top() + cell_y(row as isize);
                // Геометрия ячеек: позиции для
                // заливок, обводок и кликов.
                let mut rects = Vec::with_capacity(cells.len());
                let mut x = name_x;
                for (i, cell) in cells.iter().enumerate() {
                    let rect: egui::Rect = if i < events_len {
                        // Событие — клеточный слот (2 клетки), линейкой.
                        second
                            .into_ruler()
                            .collapse_top()
                            .collapse_left()
                            .translate(cell_x(start_col + 2 * i as isize), cell_y(row as isize))
                            .extrude(cell_x(2), CELL_SIZE.y)
                            .into()
                    } else {
                        // Имя/время — ширина измеренная, шаг — клетка.
                        let size = measure_text(ui.ctx(), style::FONT_REGULAR, &cell.text);
                        let rect = egui::Rect::from_min_size(egui::pos2(x, row_top), size);
                        x += size.x + cell_x(1);
                        rect
                    };
                    rects.push(rect);
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
                // Пробел до текста: события вплотную, зазор — здесь.
                painter.galley(
                    egui::pos2(name_x + cell_x(1), row_top),
                    galley,
                    style::PAD_TIMER_ACTIVE,
                );
                for (ci, (rect, cell)) in rects.iter().zip(cells).enumerate() {
                    // Клик: перемотка на время
                    // ячейки (play/RPT не трогаем —
                    // seek их сохраняет). Выделяется
                    // кружок: свой у события,
                    // предыдущий у имени/времени.
                    if frame.laps_clickable && !cell.text.is_empty() {
                        // Клик-зона — рект ячейки (база).
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
            // Имя первой строки блока: с холшотом это будет строка
            // холшота («HS»), без — первый круг («1)»).
            let first = if frame.holeshot_enabled { "HS" } else { "1)" };
            let cells = [
                LapCell::plain(String::new(), tone, None),
                LapCell::plain(String::new(), tone, None),
                LapCell::plain(first, tone, None),
                LapCell::plain(lap_time(Duration::ZERO, 6), tone, None),
            ];
            let galley = ui.painter().layout_job(lap_row_job(&cells));
            // Пробел до текста — как у строк кругов (события вплотную).
            ui.painter().galley(
                egui::pos2(second.left() + cell_x(1), second.top()),
                galley,
                style::PAD_TIMER_ACTIVE,
            );
        }
        // Место — планка у правого нижнего края зоны: 1 символ
        // поля с каждой стороны, высота 1¾ клетки, место двойным
        // шрифтом; цвет — медаль спорного места (большего в
        // диапазоне), текст чёрный. Полоса цвета во всю ширину зоны
        // (= вьюфайндер) на уровне верха планки, высотой четверть
        // клетки. Подсчёт — у левого края планки, пластиковым, база
        // общая с местом. Планка и полоса — во всех состояниях: без
        // данных — темно-серые, без текста и расчётов.
        let place = match (&pad.feat, pad.place) {
            (Some(feat), Some((lo, hi))) => {
                let text = if lo == hi {
                    format!("{lo}")
                } else {
                    format!("{lo}-{hi}")
                };
                let count = format!("{}/", feat.count);
                let time = if feat.count > 0 {
                    lap_time(Duration::from_millis(feat.raw_time_ms()), 6)
                } else {
                    " -.---".to_owned()
                };
                Some((text, format!("{count} {time}"), place_color(hi)))
            }
            _ => None,
        };
        {
            let text_cells = place
                .as_ref()
                .map(|(text, _, _)| text.chars().count() as isize * 2)
                .unwrap_or(0);
            // Ширина планки — в клетках: текст (2 клетки на символ)
            // плюс 2 клетки отступа с каждой стороны, минимум 6;
            // правый край — у правого края зоны, текст по центру
            // планки.
            let place_cells = (text_cells + 4).max(6);
            let plate: egui::Rect = second
                .right_bottom()
                .into_ruler()
                .translate(-cell_x(place_cells), -cell_y(7) / 4.0)
                .extrude(cell_x(place_cells), cell_y(7) / 4.0)
                .into();
            // Полоса цвета места во всю ширину зоны (= вьюфайндер)
            // на уровне верха планки, высотой четверть клетки.
            let bar: egui::Rect = second
                .into_ruler()
                .collapse_bottom()
                .translate(0.0, plate.top() - second.bottom())
                .extrude(0.0, cell_y(1) / 4.0)
                .into();
            let color = place
                .as_ref()
                .map(|(_, _, color)| *color)
                .unwrap_or(egui::Color32::from_rgb(0x30, 0x30, 0x30));
            ui.painter().rect_filled(bar, 0.0, color);
            ui.painter().rect_filled(plate, 0.0, color);
            if let Some((place_text, result_text, _)) = &place {
                ui.painter().text(
                    egui::pos2(
                        plate.left() + cell_x((place_cells - text_cells) / 2),
                        plate.bottom() + cell_y(1) / 8.0,
                    ),
                    egui::Align2::LEFT_BOTTOM,
                    place_text,
                    style::FONT_REGULAR_X2,
                    egui::Color32::BLACK,
                );
                ui.painter().text(
                    egui::pos2(plate.left() - cell_x(2), plate.bottom()),
                    egui::Align2::RIGHT_BOTTOM,
                    result_text,
                    style::FONT_REGULAR,
                    style::TRANSPORT_ACTIVE,
                );
            }
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
/// кругов и строки событий над таймлайном. Обводка и подсветка
/// опираются на rect ячейки: на 2 px меньше со всех сторон, без
/// скругления.
pub fn paint_event_cell(
    painter: &egui::Painter,
    rect: egui::Rect,
    glyph: &str,
    tone: LapTone,
    highlighted: bool,
    selected: bool,
) {
    if selected {
        painter.rect_filled(rect.shrink(2.0), 0.0, egui::Color32::BLACK);
    }
    if highlighted {
        painter.rect_filled(rect.shrink(2.0), 0.0, style::LAP_FLASH);
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
    if selected {
        // Чёрный 1 px внутрь, голубой 2 px наружу (итого 3).
        let outline = rect.shrink(2.0);
        painter.rect_stroke(
            outline,
            0.0,
            egui::Stroke::new(2.0, egui::Color32::BLACK),
            egui::StrokeKind::Inside,
        );
        painter.rect_stroke(
            outline,
            0.0,
            egui::Stroke::new(3.0, style::LAP_SELECTION),
            egui::StrokeKind::Outside,
        );
    }
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph.to_owned(),
        style::FONT_REGULAR,
        color,
    );
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
    // Открытая строка: «HS», пока холшот включён и первый интервал
    // не закрыт; иначе — «успешные + 1)» (холшот и fail номер не
    // съедают).
    let successes = lines
        .iter()
        .filter(|(_, line, _)| matches!(line, LapLine::Success { .. }))
        .count();
    let closed = lines
        .iter()
        .filter(|(_, line, _)| !matches!(line, LapLine::Current { .. }))
        .count();
    let current_name = if holeshot_enabled && closed == 0 {
        "HS".to_owned()
    } else {
        format!("{})", successes + 1)
    };
    let future = |at: Stamp| cursor.is_some_and(|c| c < at);
    let mut rows = Vec::new();
    for (end, line, events) in lines {
        // Имя — из дистиллятора целиком («HS» у холшота, «1)» с
        // скобкой у нумерованных).
        let name = match line {
            LapLine::Holeshot { .. } => "HS".to_owned(),
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

/// Медаль планки места: по большему месту в диапазоне (спорное
/// «1-2» — серебро).
fn place_color(place: u32) -> egui::Color32 {
    match place {
        1 => style::PLACE_GOLD,
        2 => style::PLACE_SILVER,
        3 => style::PLACE_BRONZE,
        _ => style::PLACE_GRAY,
    }
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
