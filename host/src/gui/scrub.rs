//! Транспорт DVR-режима: таймлайн, кнопки перемотки, скролл-гейты и
//! шкалы скоростей. Экрана Scrub больше нет — всё это использует экран
//! Flight (режим DVR).

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::ruler;

/// Микро-прототип: отрезки записи до старта гонки и после финиша, мс.
pub const RACE_LEAD_MS: u64 = 15_000;
/// База позиционного шага по тапу (клик): тап мотает на
/// speed × SEEK_TAP файла (10x → 1 с).
pub const SEEK_TAP: u64 = 100;
/// Задержка до старта hold: press короче неё — чистый клик, hold в
/// плеер не уходит вообще.
pub const SEEK_HOLD_DELAY: std::time::Duration = std::time::Duration::from_millis(100);
/// Отпускание раньше этого — клик: hold (если уже ушёл) отменяется,
/// даётся фиксированный позиционный шаг.
pub const SEEK_CLICK_MAX: std::time::Duration = std::time::Duration::from_millis(300);
/// Торможение по отпусканию кнопки перемотки при игре: возврат к
/// скорости воспроизведения за это время. Из паузы стоп мгновенный
/// (константа не участвует).
pub const SEEK_RELEASE_RAMP: Duration = Duration::from_millis(500);
/// Накопленный скролл, дающий один шаг по шкале скорости.
pub const SCROLL_STEP: f32 = 50.0;
/// Непрерывная тишина скролла, после которой следующий жест снова
/// даёт шаг по шкале. 300 мс — по аналогии с double-tap time
/// (Android ViewConfiguration / Windows).
pub const SCROLL_REARM: std::time::Duration = std::time::Duration::from_millis(300);
/// Тишина, по которой скролл-скраб отпускает курсор. Стандарта нет,
/// 200 мс — общепринятый дебаунс «конца скролла» в скролл-библиотеках.
const HSCROLL_RELEASE: std::time::Duration = std::time::Duration::from_millis(200);

/// Масштаб скролл-скраба: миллисекунды файла на физический пиксель
/// пальца. Осознанно фиксирован: не зависит от длины записи (бар при
/// этом перестаёт отображать файл 1:1 — палец и кружок расходятся).
const HSCROLL_MS_PER_PX: f32 = 100.0;
/// Шкалы скоростей (скролл над контролом ходит по ним: вверх — к
/// началу списка, вниз — к концу). Начальные: 10x, 100%, ±1.0s.
pub const SEEK_SPEEDS: [i32; 6] = [20, 15, 10, 7, 5, 3];
pub const PLAY_SPEEDS: [i32; 7] = [25, 50, 75, 100, 125, 150, 200];
pub const RPT_SIZES: [f32; 3] = [0.5, 1.0, 1.5];

/// Последний по имени файл записи камеры: `{anything}-{camera_id}-mjpeg`
/// + любой из поддержанных контейнеров. Таймстамп-префикс сортируется
/// лексикографически.
pub fn latest_capture(dir: &Path, camera_id: &str) -> Option<PathBuf> {
    let stem = format!("-{camera_id}-mjpeg.");
    let exts = ["mkv", "mp4", "mov"];
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().is_some_and(|n| {
                let n = n.to_string_lossy();
                exts.iter().any(|ext| n.ends_with(&format!("{stem}{ext}")))
            })
        })
        .max()
}

/// События таймлайна: команды хозяину (flight) на все плееры разом.
pub enum TimelineEvent {
    /// Захват/отпускание шкалы — hold (не пауза).
    SetDragHeld(bool),
    /// Перемотать на t мс от начала.
    Seek(u64),
}

/// Таймлайн scrub: линия с кружком позиции, интерактив. Держит своё
/// состояние ввода (нажатие, точку последнего seek'а), наружу отдаёт
/// только события. Клик и таскание перематывают по временной шкале;
/// seek — только при реальном сдвиге указателя. Время рисует хозяин.
pub struct Timeline {
    /// Кнопка мыши на шкале зажата (на прошлом кадре).
    down: bool,
    /// Точка последнего seek'а: пока кнопка зажата, seek идёт только
    /// при реальном сдвиге — иначе каждый кадр UI слал бы команду
    /// впустую и крутил busy-loop.
    last_seek: Option<egui::Pos2>,
}

impl Timeline {
    pub fn new() -> Self {
        Self {
            down: false,
            last_seek: None,
        }
    }

    /// Рисует линию и кружок позиции в rect, возвращает события кадра
    /// и response шкалы (hover — хозяину, для горизонтального скролла).
    /// frac — доля позиции (0..1), dur_ms — для перевода доли в мс.
    /// window — доли (начало, конец) фрагмента активного вылета: рисуем
    /// белым, края полоски — серым; None — вся линия серая. circle —
    /// доли (начало, конец) круга зацикливания: вместо кружка головки
    /// — белая капсула (сосиска) на весь круг, точная позиция —
    /// чёрный кружок меньшего диаметра поверх.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        frac: f32,
        dur_ms: u64,
        window: Option<(f32, f32)>,
        circle: Option<(f32, f32)>,
        // Акцентный цвет: каретка и активный фрагмент трека. Белый у
        // scrub, слоновая кость (активный транспортный) у flight.
        knob: egui::Color32,
    ) -> (egui::Response, Vec<TimelineEvent>) {
        let mut events = Vec::new();
        let y = rect.center().y;
        let x = |f: f32| rect.left() + rect.width() * f;
        // Трек — 40% акцентного, активный фрагмент — акцентным
        // (параметр knob: белый у scrub, слоновая кость у flight).
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
        let knob_r = ruler::CELL_SIZE.y * 0.24;
        match circle {
            Some((lo, hi)) => {
                // Круг: белая капсула между его границами (крупнее
                // одиночного кружка), головка — чёрный кружок поверх:
                // больше точки-«пипки», меньше высоты капсулы. Драг
                // таскает всю конструкцию: seek ставит позицию под
                // курсор, центр круга встаёт на позицию.
                let cap_r = knob_r * 1.4;
                // Стадион: центры полуовалов — в точках круга lo/hi
                // (расстояние центров = диаметр круга), овалы выпирают
                // за них на радиус. Вырождение lo==hi — ровно кружок,
                // отдельный минимум не нужен. Сначала — чёрное «эхо»
                // чуть большего диаметра: выделяет каретку на треке.
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
                let t = (((pointer.x - rect.left()) / rect.width()).clamp(0.0, 1.0) * dur_ms as f32)
                    as u64;
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
}

/// Гистерезис скролла «один жест — один шаг»: накопитель дельты +
/// перевооружение после непрерывной тишины. Трэкпад шлёт дельты
/// рывками, поэтому концом жеста считается не кадр без дельты, а
/// тишина длиннее rearm. Возвращает направление шага (true — вверх).
pub struct ScrollGate {
    acc: f32,
    armed: bool,
    quiet_since: std::time::Instant,
}

impl ScrollGate {
    pub fn new() -> Self {
        Self {
            acc: 0.0,
            armed: true,
            quiet_since: std::time::Instant::now(),
        }
    }

    pub fn step(&mut self, delta: f32, threshold: f32, rearm: std::time::Duration) -> Option<bool> {
        if delta == 0.0 {
            if self.quiet_since.elapsed() >= rearm {
                self.armed = true;
                self.acc = 0.0;
            }
            return None;
        }
        self.quiet_since = std::time::Instant::now();
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
pub struct HScroll {
    acc: f32,
    active: bool,
    quiet_since: std::time::Instant,
}

impl HScroll {
    pub fn new() -> Self {
        Self {
            acc: 0.0,
            active: false,
            quiet_since: std::time::Instant::now(),
        }
    }

    /// Скраб активен (hold): курсор Grabbing, тикает repaint.
    pub fn active(&self) -> bool {
        self.active
    }

    /// Дельта кадра (0 — скролла нет) → миллисекунды шага, если скраб
    /// двигается. Выход по тишине проверяется на каждом вызове,
    /// включая нулевые дельты.
    pub fn feed(&mut self, dx: f32, pixels_per_point: f32) -> Option<i64> {
        if dx == 0.0 {
            if self.active && self.quiet_since.elapsed() >= HSCROLL_RELEASE {
                self.active = false;
                self.acc = 0.0;
            }
            return None;
        }
        self.quiet_since = std::time::Instant::now();
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

/// Кнопка перемотки: клик — позиционный шаг, удержание — разгон плеера
/// до ±скорости рампой, отпускание — возврат к прежнему режиму рампой.
/// Весь профиль рампы живёт в плеере (resume он запоминает сам).
///
/// Семантика клика против hold'а: press ждёт SEEK_HOLD_DELAY (100 мс)
/// — удержали дольше, в плеер уходит hold; отпускание раньше
/// SEEK_CLICK_MAX (300 мс) отменяет его и даёт фиксированный шаг.
/// Иначе короткий клик со щелчком (2–3 кадра) неотличим от тапа.
pub struct SeekButton {
    pub down: bool,
    /// Момент press'а: окна SEEK_HOLD_DELAY / SEEK_CLICK_MAX.
    pub down_since: std::time::Instant,
    /// Hold уже послан в плеер (после SEEK_HOLD_DELAY): отпускание до
    /// SEEK_CLICK_MAX должно его отменить, а не просто игнорировать.
    pub hold_sent: bool,
    /// Тап/клик: вспышка подсветки. Липкая: горит, пока её не погасит
    /// следующая перерисовка (сброс в начале апдейта; событие ставит
    /// флаг заново в своём кадре).
    pub flash: bool,
}

impl SeekButton {
    pub fn new() -> Self {
        Self {
            down: false,
            down_since: std::time::Instant::now(),
            hold_sent: false,
            flash: false,
        }
    }

    /// Подсвечена: жмут или тапнули и ещё не было перерисовки.
    pub fn lit(&self) -> bool {
        self.down || self.flash
    }
}
