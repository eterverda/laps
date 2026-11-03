//! Транспорт DVR-режима экрана Flight: таймлайн, кнопки перемотки,
//! скролл-гейты и шкалы скоростей. Виджеты рисуют и возвращают
//! события-интенты; исполнение (транспорт, перемотка) — у экрана.

use std::time::Duration;
use std::time::Instant;

use super::laps::{LapTone, lap_glyph, paint_event_cell};
use crate::gui::ruler::{CELL_SIZE, cell_y};
use crate::model::flight::{Interval, LapEvent, Stamp};

/// База позиционного шага по тапу (клик): тап мотает на
/// speed × SEEK_TAP файла (10x → 1 с).
pub const SEEK_TAP: u64 = 100;
/// Задержка до старта hold: press короче неё — чистый клик, hold в
/// плеер не уходит вообще.
const SEEK_HOLD_DELAY: Duration = Duration::from_millis(100);
/// Отпускание раньше этого — клик: hold (если уже ушёл) отменяется,
/// даётся фиксированный позиционный шаг.
const SEEK_CLICK_MAX: Duration = Duration::from_millis(300);
/// Торможение по отпусканию кнопки перемотки при игре: возврат к
/// скорости воспроизведения за это время. Из паузы стоп мгновенный
/// (константа не участвует).
pub const SEEK_RELEASE_RAMP: Duration = Duration::from_millis(500);
/// Накопленный скролл, дающий один шаг по шкале скорости.
pub const SCROLL_STEP: f32 = 50.0;
/// Минимальная ширина сосиски повтора, физические пиксели. Уже —
/// растягиваем: внутри растянутой сосиски голова (и drag) идут по
/// локальному скейлу круга, а не записи.
const CIRCLE_MIN_WIDTH_PX: f32 = 24.0;
/// Непрерывная тишина скролла, после которой следующий жест снова
/// даёт шаг по шкале. 300 мс — по аналогии с double-tap time
/// (Android ViewConfiguration / Windows).
pub const SCROLL_REARM: Duration = Duration::from_millis(300);
/// Тишина, по которой скролл-скраб отпускает курсор. Стандарта нет,
/// 200 мс — общепринятый дебаунс «конца скролла» в скролл-библиотеках.
const HSCROLL_RELEASE: Duration = Duration::from_millis(200);

/// Масштаб скролл-скраба: миллисекунды файла на физический пиксель
/// пальца. Осознанно фиксирован: не зависит от длины записи (бар при
/// этом перестаёт отображать файл 1:1 — палец и кружок расходятся).
const HSCROLL_MS_PER_PX: f32 = 100.0;
/// Шкалы скоростей (скролл над контролом: вверх — быстрее/больше,
/// вниз — медленнее/меньше; списки по возрастанию, сдвиг единый).
/// Начальные: 10x, 100%, ±1.0s.
pub const SEEK_SPEEDS: [i32; 6] = [3, 5, 7, 10, 15, 20];
pub const PLAY_SPEEDS: [i32; 7] = [25, 50, 75, 100, 125, 150, 200];
pub const RPT_SIZES: [f32; 4] = [0.5, 1.0, 1.5, 2.0];

/// События таймлайна: команды хозяину (flight) на все плееры разом.
pub enum TimelineEvent {
    /// Захват/отпускание шкалы — hold (не пауза).
    SetDragHeld(bool),
    /// Перемотать на t мс от начала.
    Seek(u64),
}

/// Акцентный цвет каретки и активного фрагмента трека — слоновая кость
/// транспорта (своё имя: каретка таймлайна не обязана вечно следовать
/// кнопкам транспорта).
const ACCENT: egui::Color32 = crate::gui::style::TRANSPORT_ACTIVE;

/// Таймлайн DVR: линия с кружком позиции, интерактив. Держит своё
/// состояние ввода (нажатие, точку последнего seek'а, скролл-скраб),
/// наружу отдаёт только события. Клик и таскание перематывают по
/// временной шкале; seek — только при реальном сдвиге указателя.
/// Время (доли) рисует хозяин: домен журнала в виджет не протекает.
pub struct Timeline {
    /// Кнопка мыши на шкале зажата (на прошлом кадре).
    down: bool,
    /// Точка последнего seek'а: пока кнопка зажата, seek идёт только
    /// при реальном сдвиге — иначе каждый кадр UI слал бы команду
    /// впустую и крутил busy-loop.
    last_seek: Option<egui::Pos2>,
    /// Скролл-скраб над шкалой (состояние гейта — у виджета).
    hscroll: HScroll,
}

impl Timeline {
    pub fn new() -> Self {
        Self {
            down: false,
            last_seek: None,
            hscroll: HScroll::new(),
        }
    }

    /// Рисует линию и кружок позиции в rect, возвращает события кадра
    /// и response шкалы (hover — хозяину, для горизонтального скролла).
    /// frac — доля позиции (0..1), dur_ms — для перевода доли в мс.
    /// window — доли (начало, конец) фрагмента активного вылета: рисуем
    /// белым, края полоски — серым; None — вся линия серая. circle —
    /// доли (начало, конец) реального круга зацикливания: вместо
    /// кружка головки — белая капсула (сосиска) на весь круг, точная
    /// позиция — чёрный кружок меньшего диаметра поверх. Круг уже
    /// CIRCLE_MIN_WIDTH_PX растягивается здесь: внутри растянутой
    /// капсулы голова (и drag) течёт по локальному скейлу круга.
    /// marker — доля 0..1 момента отсчёта: белая точка 2px на линии
    /// (RaceCountdown: стартовая позиция курсора DVR).
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        frac: f32,
        dur_ms: u64,
        window: Option<(f32, f32)>,
        circle: Option<(f32, f32)>,
        marker: Option<f32>,
    ) -> (egui::Response, Vec<TimelineEvent>) {
        // Акцент: каретка и активный фрагмент трека — цвет активного
        // транспортного контролла (единственная роль виджета).
        let knob = ACCENT;
        let mut events = Vec::new();
        let y = rect.center().y;
        let x = |f: f32| rect.left() + rect.width() * f;
        // Растяжение сосиски до минимума (реальный круг уже минимума —
        // голова по общему скейлу; иначе капсула тянется до
        // CIRCLE_MIN_WIDTH_PX, а внутри неё голова (и drag) идут по
        // локальному скейлу круга).
        let min_w = (CIRCLE_MIN_WIDTH_PX / ui.ctx().pixels_per_point() / rect.width()).min(1.0);
        let (circle, circle_real, frac) = match circle {
            Some((lo, hi)) if hi - lo < min_w => {
                let center = ((lo + hi) / 2.0).clamp(min_w / 2.0, 1.0 - min_w / 2.0);
                let nlo = center - min_w / 2.0;
                let nhi = center + min_w / 2.0;
                let inner = ((frac - lo) / (hi - lo).max(f32::EPSILON)).clamp(0.0, 1.0);
                (Some((nlo, nhi)), Some((lo, hi)), nlo + inner * (nhi - nlo))
            }
            other => (other, None, frac),
        };
        // Трек — 40% акцентного, активный фрагмент — акцентным.
        let track = egui::Stroke::new(2.0, knob.gamma_multiply(0.4));
        let white = egui::Stroke::new(2.0, knob);
        match window {
            Some((start, end)) => {
                ui.painter()
                    .line_segment([egui::pos2(rect.left(), y), egui::pos2(x(start), y)], track);
                ui.painter()
                    .line_segment([egui::pos2(x(start), y), egui::pos2(x(end), y)], white);
                ui.painter()
                    .line_segment([egui::pos2(x(end), y), egui::pos2(rect.right(), y)], track);
            }
            None => {
                ui.painter().line_segment(
                    [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                    track,
                );
            }
        }
        // Маркер (отсчёт): белая точка 2px прямо на линии.
        if let Some(f) = marker {
            ui.painter().circle(
                egui::pos2(x(f.clamp(0.0, 1.0)), y),
                1.0,
                egui::Color32::WHITE,
                egui::Stroke::NONE,
            );
        }
        let knob_r = CELL_SIZE.y * 0.24;
        match circle {
            Some((lo, hi)) => {
                // Круг: белая капсула между его границами (крупнее
                // одиночного кружка), головка — чёрный кружок поверх:
                // больше точки-«пипки», меньше высоты капсулы. Драг
                // таскает всю конструкцию: seek ставит позицию под
                // курсор, центр круга встаёт на позицию.
                let cap_r = knob_r * 1.4;
                // Стадион: центры полуовалов — в точках круга lo/hi,
                // овалы выпирают за них на радиус. Уже минимума круг
                // растянут выше, так что lo==hi здесь бывает лишь если
                // таймлайн уже минимума — вырождение в кружок.
                // Сначала — чёрное «эхо» чуть большего
                // диаметра: выделяет каретку на треке.
                let echo = knob_r * 0.4;
                let x0 = x(lo);
                let x1 = x(hi);
                ui.painter().rect(
                    egui::Rect::from_center_size(
                        egui::pos2((x0 + x1) / 2.0, y),
                        egui::vec2((x1 - x0).max(0.0) + echo * 2.0, (cap_r + echo) * 2.0),
                    ),
                    egui::CornerRadius::ZERO,
                    egui::Color32::BLACK,
                    egui::Stroke::NONE,
                    egui::StrokeKind::Inside,
                );
                for cx in [x0, x1] {
                    ui.painter().circle(
                        egui::pos2(cx, y),
                        cap_r + echo,
                        egui::Color32::BLACK,
                        egui::Stroke::NONE,
                    );
                }
                ui.painter().rect(
                    egui::Rect::from_center_size(
                        egui::pos2((x0 + x1) / 2.0, y),
                        egui::vec2((x1 - x0).max(0.0), cap_r * 2.0),
                    ),
                    egui::CornerRadius::ZERO,
                    knob,
                    egui::Stroke::NONE,
                    egui::StrokeKind::Inside,
                );
                for cx in [x0, x1] {
                    ui.painter()
                        .circle(egui::pos2(cx, y), cap_r, knob, egui::Stroke::NONE);
                }
                ui.painter().circle(
                    egui::pos2(x(frac), y),
                    knob_r * 0.95,
                    egui::Color32::BLACK,
                    egui::Stroke::NONE,
                );
            }
            None => {
                // Круглая каретка с чёрным «эхом» чуть большего диаметра.
                ui.painter().circle(
                    egui::pos2(x(frac), y),
                    knob_r * 1.4,
                    egui::Color32::BLACK,
                    egui::Stroke::NONE,
                );
                ui.painter()
                    .circle(egui::pos2(x(frac), y), knob_r, knob, egui::Stroke::NONE);
            }
        }
        let response = ui.interact(
            rect,
            ui.make_persistent_id("scrub_timeline"),
            egui::Sense::click_and_drag(),
        );
        // Курсор drag'а: открытая ладонь в покое, зажатая — пока таскают.
        if response.is_pointer_button_down_on() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
        // Факт нажатия отслеживаем вручную: у egui на драг есть порог
        // движения, а тап может уложиться в один фрейм — тогда виден
        // только clicked().
        let down = response.is_pointer_button_down_on();
        if down && !self.down {
            events.push(TimelineEvent::SetDragHeld(true));
        }
        // Позиция указателя: пока зажато — interact_pointer_pos, клик —
        // hover_pos.
        let pointer = response.interact_pointer_pos().or_else(|| {
            if response.clicked() {
                response.hover_pos()
            } else {
                None
            }
        });
        if let Some(pointer) = pointer {
            // Seek — только при реальном сдвиге: порог — физический
            // пиксель (points пересчитываем по pixels_per_point, наш
            // виртуальный зум letterbox'а так отсекается). Иначе
            // микродрожание сенсора гоняло бы декод каждый кадр.
            let moved = self
                .last_seek
                .is_none_or(|last| (pointer - last).length() > 1.0 / ui.ctx().pixels_per_point());
            if moved || !down {
                let p = ((pointer.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
                let mut t = (p * dur_ms as f32) as u64;
                // Растянутая сосиска: внутри неё seek идёт по скейлу
                // круга (обратный маппинг), а не записи.
                if let (Some((nlo, nhi)), Some((rlo, rhi))) = (circle, circle_real) {
                    if p >= nlo && p <= nhi && (nhi - nlo) > f32::EPSILON {
                        let inner = ((p - nlo) / (nhi - nlo)).clamp(0.0, 1.0);
                        t = ((rlo + inner * (rhi - rlo)).clamp(0.0, 1.0) * dur_ms as f32) as u64;
                    }
                }
                events.push(TimelineEvent::Seek(t));
                ui.ctx().request_repaint();
                self.last_seek = Some(pointer);
            }
        }
        if !down && self.down {
            events.push(TimelineEvent::SetDragHeld(false));
        }
        if !down {
            self.last_seek = None;
        }
        self.down = down;
        (response, events)
    }

    /// Горизонтальный скролл над шкалой — относительный скраб. Дельта
    /// кадра (0 — скролла нет) → миллисекунды шага (хозяин мотает);
    /// курсор и repaint — на себе. Вход жёстче, чтобы микро-дрожание
    /// не будило скраб; выход — по тишине (HScroll_RELEASE).
    pub fn hscroll(&mut self, ui: &egui::Ui, response: &egui::Response, dx: f32) -> Option<i64> {
        let dx = if response.hovered() { dx } else { 0.0 };
        let ms = self.hscroll.feed(dx, ui.ctx().pixels_per_point())?;
        if self.hscroll.active() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            ui.ctx().request_repaint();
        }
        Some(ms)
    }
}

/// События кнопки перемотки за кадр: команды хозяину (транспорту и
/// перемотке). Семантика клика против hold'а: press ждёт
/// SEEK_HOLD_DELAY (100 мс) — удержали дольше, уходит hold;
/// отпускание раньше SEEK_CLICK_MAX (300 мс) отменяет его и даёт
/// фиксированный шаг. Иначе короткий клик со щелчком (2–3 кадра)
/// неотличим от тапа.
pub enum SeekEvent {
    /// Тап или короткий клик: позиционный шаг; величину (шкала
    /// SEEK_SPEEDS × SEEK_TAP) и знак (направление кнопки) знает
    /// хозяин. `from_hold` — hold успел уйти в плеер: сначала рампа
    /// возврата (SEEK_RELEASE_RAMP), потом шаг. Вспышка подсветки
    /// уже поставлена.
    Step { from_hold: bool },
    /// Hold: разгон до скорости шаттла (знак — направление кнопки;
    /// величину — шкала SEEK_SPEEDS — знает хозяин).
    Shuttle,
    /// Долгое отпускание после hold'а: рампа возврата
    /// (SEEK_RELEASE_RAMP).
    Release,
}

/// Кнопка перемотки: клик — позиционный шаг, удержание — разгон плеера
/// до ±скорости рампой, отпускание — возврат к прежнему режиму рампой.
/// Весь профиль рампы живёт в плеере (resume он запоминает сам).
///
/// Состояние ввода кнопки; машина press/hold/release — метод tick:
/// один кадр (response кнопки) → событие-интент хозяину.
pub struct SeekButton {
    down: bool,
    /// Момент press'а: окна SEEK_HOLD_DELAY / SEEK_CLICK_MAX.
    down_since: Instant,
    /// Hold уже послан в плеер (после SEEK_HOLD_DELAY): отпускание до
    /// SEEK_CLICK_MAX должно его отменить, а не просто игнорировать.
    hold_sent: bool,
    /// Тап/клик: вспышка подсветки. Липкая: горит, пока её не погасит
    /// следующая перерисовка (сброс в начале апдейта; событие ставит
    /// флаг заново в своём кадре).
    pub flash: bool,
}

impl SeekButton {
    pub fn new() -> Self {
        Self {
            down: false,
            down_since: Instant::now(),
            hold_sent: false,
            flash: false,
        }
    }

    /// Подсвечена: жмут или тапнули и ещё не было перерисовки.
    pub fn lit(&self) -> bool {
        self.down || self.flash
    }

    /// Один кадр кнопки. Возвращает событие кадра (максимум одно);
    /// исполнение (шаг/шаттл/рампа на транспорте) — хозяина.
    pub fn tick(&mut self, ui: &egui::Ui, response: &egui::Response) -> Option<SeekEvent> {
        let pointer_down = ui.input(|i| i.pointer.primary_down());
        let edge = if self.down && !pointer_down {
            self.down = false;
            Some(Edge::Release)
        } else if !self.down && response.is_pointer_button_down_on() {
            self.down = true;
            self.down_since = Instant::now();
            self.hold_sent = false;
            None
        } else if !self.down && response.clicked() {
            Some(Edge::Tap)
        } else {
            None
        };
        match edge {
            Some(Edge::Release) => {
                if self.down_since.elapsed() < SEEK_CLICK_MAX {
                    let from_hold = self.hold_sent;
                    self.flash = true;
                    return Some(SeekEvent::Step { from_hold });
                }
                return Some(SeekEvent::Release);
            }
            Some(Edge::Tap) => {
                self.flash = true;
                return Some(SeekEvent::Step { from_hold: false });
            }
            None => {}
        }
        if self.down && !self.hold_sent {
            ui.ctx().request_repaint();
        }
        if self.down && !self.hold_sent && self.down_since.elapsed() >= SEEK_HOLD_DELAY {
            self.hold_sent = true;
            return Some(SeekEvent::Shuttle);
        }
        None
    }
}

/// Фронт кнопки перемотки за кадр (внутренний термин tick'а).
enum Edge {
    Release,
    Tap,
}

/// Гистерезис скролла «один жест — один шаг»: накопитель дельты +
/// перевооружение после непрерывной тишины. Трэкпад шлёт дельты
/// рывками, поэтому концом жеста считается не кадр без дельты, а
/// тишина длиннее rearm. Возвращает направление шага (true — вверх).
pub struct ScrollGate {
    acc: f32,
    armed: bool,
    quiet_since: Instant,
}

impl ScrollGate {
    pub fn new() -> Self {
        Self {
            acc: 0.0,
            armed: true,
            quiet_since: Instant::now(),
        }
    }

    pub fn step(&mut self, delta: f32, threshold: f32, rearm: Duration) -> Option<bool> {
        if delta == 0.0 {
            if self.quiet_since.elapsed() >= rearm {
                self.armed = true;
                self.acc = 0.0;
            }
            return None;
        }
        self.quiet_since = Instant::now();
        self.acc += delta;
        if self.armed && self.acc.abs() >= threshold {
            let up = self.acc > 0.0;
            self.armed = false;
            self.acc = 0.0;
            return Some(up);
        }
        None
    }
}

/// Горизонтальный скролл над таймлайном — относительный скраб. Всё в
/// физических пикселях пальца: и пороги, и масштаб — длина записи в
/// расчёт не входит (бар перестаёт отображать файл 1:1 — осознанно).
/// Вход жёстче, чтобы микро-дрожание не будило скраб; выход — по тишине.
struct HScroll {
    acc: f32,
    active: bool,
    quiet_since: Instant,
}

impl HScroll {
    fn new() -> Self {
        Self {
            acc: 0.0,
            active: false,
            quiet_since: Instant::now(),
        }
    }

    /// Скраб активен (hold): курсор Grabbing, тикает repaint.
    fn active(&self) -> bool {
        self.active
    }

    /// Дельта кадра (0 — скролла нет) → миллисекунды шага, если скраб
    /// двигается. Выход по тишине проверяется на каждом вызове,
    /// включая нулевые дельты.
    fn feed(&mut self, dx: f32, pixels_per_point: f32) -> Option<i64> {
        if dx == 0.0 {
            if self.active && self.quiet_since.elapsed() >= HSCROLL_RELEASE {
                self.active = false;
                self.acc = 0.0;
            }
            return None;
        }
        self.quiet_since = Instant::now();
        self.acc += dx;
        let px = self.acc * pixels_per_point;
        let enter = if self.active { 1.0 } else { 2.0 };
        if px.abs() < enter {
            return None;
        }
        self.active = true;
        let ms = (px * HSCROLL_MS_PER_PX).round() as i64;
        self.acc = 0.0;
        Some(ms)
    }
}

/// События выделенного пада — строка над таймлайном (освобождённая от
/// списка кругов клетка): глиф по центру над своим временем; подсветка
/// и выделение — как в таблице кругов. Два прохода: будущее (после
/// текущего) с конца, потом прошлое включительно с начала — ближайшие
/// к текущему рисуются поверх. Текущее — выделенное, иначе самое
/// свежее ≤ курсора (current приходит готовым — хозяина знает
/// выделение). Строка на полстроки ниже границы: верхняя половина —
/// в освобождённой клетке, нижняя заходит на таймлайн. В пересечении
/// клик забирает событие: его интерактив регистрируется позже
/// (egui: позднее = поверх). Возвращает мс кликнутого события; мутация
/// выделения и seek — хозяина.
#[allow(clippy::too_many_arguments)]
pub fn show_events(
    ui: &mut egui::Ui,
    timeline_rect: egui::Rect,
    window: Interval,
    pos: Stamp,
    events: &[(Stamp, LapEvent)],
    current: Option<Stamp>,
    selected: Option<Stamp>,
    flash: (i64, i64),
) -> Option<Stamp> {
    if events.is_empty() {
        return None;
    }
    let span = window.span_ms().max(1);
    let row = egui::Rect::from_min_max(
        egui::pos2(timeline_rect.left(), timeline_rect.top() - cell_y(1) / 2.0),
        egui::pos2(timeline_rect.right(), timeline_rect.top() + cell_y(1) / 2.0),
    );
    let mut order: Vec<(Stamp, LapEvent)> = events
        .iter()
        .rev()
        .take_while(|(at, _)| current.is_none_or(|c| *at > c))
        .copied()
        .collect();
    if let Some(c) = current {
        order.extend(events.iter().take_while(|(at, _)| *at <= c).copied());
    }
    let (before, after) = flash;
    let mut clicked = None;
    for (at, event) in &order {
        let at_off = at.ms_since(window.from);
        if !(at_off >= 0 && at_off <= span as i64) {
            continue;
        }
        let frac = at_off as f32 / span as f32;
        let center = egui::pos2(
            timeline_rect.left() + frac * timeline_rect.width(),
            row.center().y,
        );
        // Ячейка события — ровно клетка сетки, позиционирование —
        // по центру (суб-клетно).
        let rect = egui::Rect::from_center_size(center, CELL_SIZE).expand2(egui::vec2(6.0, -2.0));
        let highlighted = {
            let d = pos.ms_since(*at);
            d >= before && d < after
        };
        paint_event_cell(
            &ui.painter(),
            rect,
            &lap_glyph(event).to_string(),
            if pos < *at {
                LapTone::Future
            } else {
                LapTone::Active
            },
            highlighted,
            selected == Some(*at),
        );
        // Клик — выделение события (+ seek, как по ячейке таблицы) —
        // хозяина. События строки — пада из выделения.
        let response = ui.interact(
            rect,
            egui::Id::unique(("tl_event", *at)),
            egui::Sense::click(),
        );
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if response.clicked() {
            clicked = Some(*at);
        }
    }
    clicked
}
