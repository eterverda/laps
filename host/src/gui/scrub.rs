//! Экран scrub: проигрывание записей. Отдельный режим (State), визуально
//! повторяет live: тот же заголовок и раскладка вьюфайндеров (копия,
//! общие — только утилиты live::viewfinder_rows/cols и константы сетки).
//! Плеер на камеру: имя камеры — из имени файла (`{ts}-{camera_id}-mjpeg`),
//! viewport пада берёт картинку своего camera_id, как в live.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::Navigator;
use super::grid;
use super::guidelines;
use super::header;
use super::live;
use super::menu;
use super::style;
use super::view;
use super::viewfinder;
use crate::config::setup::Setup;
use crate::driver::player::{Player, PlayerState};
use crate::model::pilot::Pilot;

const GRID_WIDTH: isize = 160;
const LEFT_MARGIN: isize = 4;
const RIGHT_MARGIN: isize = 4;
const TOP_BAR_ROWS: isize = 2;
const BOTTOM_BAR_ROWS: isize = 2;
const MAX_PADS: usize = 4;
/// Микро-прототип: отрезки записи до старта гонки и после финиша, мс.
const RACE_LEAD_MS: u64 = 15_000;
/// База позиционного шага по тапу (клик): тап мотает на
/// speed × SEEK_TAP файла (10x → 1 с).
const SEEK_TAP: u64 = 100;
/// Задержка до старта hold: press короче неё — чистый клик, hold в
/// плеер не уходит вообще.
const SEEK_HOLD_DELAY: std::time::Duration = std::time::Duration::from_millis(100);
/// Отпускание раньше этого — клик: hold (если уже ушёл) отменяется,
/// даётся фиксированный позиционный шаг.
const SEEK_CLICK_MAX: std::time::Duration = std::time::Duration::from_millis(300);
/// Торможение по отпусканию кнопки перемотки при игре: возврат к
/// скорости воспроизведения за это время. Из паузы стоп мгновенный
/// (константа не участвует).
const SEEK_RELEASE_RAMP: Duration = Duration::from_millis(500);
/// Накопленный скролл, дающий один шаг по шкале скорости.
const SCROLL_STEP: f32 = 50.0;
/// Непрерывная тишина скролла, после которой следующий жест снова
/// даёт шаг по шкале. 300 мс — по аналогии с double-tap time
/// (Android ViewConfiguration / Windows).
const SCROLL_REARM: std::time::Duration = std::time::Duration::from_millis(300);
/// Тишина, по которой скролл-скраб отпускает курсор. Стандарта нет,
/// 200 мс — общепринятый дебаунс «конца скролла» в скролл-библиотеках.
const HSCROLL_RELEASE: std::time::Duration = std::time::Duration::from_millis(200);
/// Масштаб скролл-скраба: миллисекунды файла на физический пиксель
/// пальца. Осознанно фиксирован: не зависит от длины записи (бар при
/// этом перестаёт отображать файл 1:1 — палец и кружок расходятся).
const HSCROLL_MS_PER_PX: f32 = 100.0;
/// Шкалы скоростей (скролл над контролом ходит по ним: вверх — к
/// началу списка, вниз — к концу). Начальные: 10x, 100%, ±1.0s.
const SEEK_SPEEDS: [i32; 6] = [20, 15, 10, 7, 5, 3];
const PLAY_SPEEDS: [i32; 7] = [25, 50, 75, 100, 125, 150, 200];
const RPT_SIZES: [f32; 3] = [0.5, 1.0, 1.5];

/// мм:сс.ммм — общий формат времени на экране scrub.
fn mmssmmm(ms: u64) -> String {
    format!(
        "{:02}:{:02}.{:03}",
        ms / 60_000,
        (ms / 1_000) % 60,
        ms % 1_000
    )
}

/// Последний по имени файл записи камеры: `{anything}-{camera_id}-mjpeg`
/// + любой из поддержанных контейнеров. Таймстамп-префикс сортируется
/// лексикографически.
fn latest_capture(dir: &Path, camera_id: &str) -> Option<PathBuf> {
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

/// События таймлайна: команды хозяину (scrub) на все плееры разом.
enum TimelineEvent {
    /// Захват/отпускание шкалы — hold (не пауза).
    SetHeld(bool),
    /// Перемотать на t мс от начала.
    Seek(u64),
}

/// Таймлайн scrub: линия с кружком позиции, интерактив. Держит своё
/// состояние ввода (нажатие, точку последнего seek'а), наружу отдаёт
/// только события. Клик и таскание перематывают по временной шкале;
/// seek — только при реальном сдвиге указателя. Время рисует хозяин.
struct Timeline {
    /// Кнопка мыши на шкале зажата (на прошлом кадре).
    down: bool,
    /// Точка последнего seek'а: пока кнопка зажата, seek идёт только
    /// при реальном сдвиге — иначе каждый кадр UI слал бы команду
    /// впустую и крутил busy-loop.
    last_seek: Option<egui::Pos2>,
}

impl Timeline {
    fn new() -> Self {
        Self {
            down: false,
            last_seek: None,
        }
    }

    /// Рисует линию и кружок позиции в rect, возвращает события кадра
    /// и response шкалы (hover — хозяину, для горизонтального скролла).
    /// frac — доля позиции (0..1), dur_ms — для перевода доли в мс.
    /// window — доли (начало, конец) фрагмента активного вылета: рисуем
    /// белым, края полоски — серым; None — вся линия серая.
    fn show(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        frac: f32,
        dur_ms: u64,
        window: Option<(f32, f32)>,
    ) -> (egui::Response, Vec<TimelineEvent>) {
        let mut events = Vec::new();
        let y = rect.center().y;
        let x = |f: f32| rect.left() + rect.width() * f;
        let gray = egui::Stroke::new(2.0, egui::Color32::from_gray(96));
        let white = egui::Stroke::new(2.0, egui::Color32::WHITE);
        match window {
            Some((start, end)) => {
                ui.painter()
                    .line_segment([egui::pos2(rect.left(), y), egui::pos2(x(start), y)], gray);
                ui.painter()
                    .line_segment([egui::pos2(x(start), y), egui::pos2(x(end), y)], white);
                ui.painter()
                    .line_segment([egui::pos2(x(end), y), egui::pos2(rect.right(), y)], gray);
            }
            None => {
                ui.painter().line_segment(
                    [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                    gray,
                );
            }
        }
        ui.painter().circle(
            egui::pos2(x(frac), y),
            grid::CELL_SIZE.y * 0.24,
            egui::Color32::WHITE,
            egui::Stroke::NONE,
        );
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
            events.push(TimelineEvent::SetHeld(true));
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
            events.push(TimelineEvent::SetHeld(false));
        }
        if !down {
            self.last_seek = None;
        }
        self.down = down;
        (response, events)
    }
}

pub struct Scrub {
    /// camera_id → плеер записи этой камеры.
    players: HashMap<String, Player>,
    timeline: Timeline,
    /// Перемотка REW/FF: состояние удержания.
    rew: SeekButton,
    ff: SeekButton,
    /// Скорости контролов (индексы в шкалах); скролл над контролом
    /// меняет значение.
    seek_speed: usize,
    play_speed: usize,
    rpt_size: usize,
    /// Накопитель скролла для шага по шкале.
    scroll_acc: f32,
    /// Накопитель горизонтального скролла над таймлайном (мс файла,
    /// дробный остаток между кадрами).
    hscroll_acc: f32,
    /// Скролл-скраб активен: введён реальной дельтой (с порогом), не
    /// выводится микро-скроллами — только тишиной (как hold).
    hscroll_active: bool,
    /// Начало текущего непрерывного промежутка без горизонтального
    /// скролла.
    hscroll_quiet_since: std::time::Instant,
    /// Скролл вооружён (может дать шаг). После выстрела разоружается
    /// до SCROLL_REARM непрерывной тишины — трэкпад шлёт дельты
    /// рывками, кадр без дельты посреди жеста не считается концом.
    scroll_armed: bool,
    /// Начало текущего непрерывного промежутка без скролла.
    scroll_quiet_since: std::time::Instant,
    header: header::Header,
    setup: Setup,
    assignments: HashMap<String, Pilot>,
}

/// Кнопка перемотки: клик — позиционный шаг, удержание — разгон плеера
/// до ±скорости рампой, отпускание — возврат к прежнему режиму рампой.
/// Весь профиль рампы живёт в плеере (resume он запоминает сам).
///
/// Семантика клика против hold'а: press ждёт SEEK_HOLD_DELAY (100 мс)
/// — удержали дольше, в плеер уходит hold; отпускание раньше
/// SEEK_CLICK_MAX (300 мс) отменяет его и даёт фиксированный шаг.
/// Иначе короткий клик со щелчком (2–3 кадра) неотличим от тапа.
struct SeekButton {
    down: bool,
    /// Момент press'а: окна SEEK_HOLD_DELAY / SEEK_CLICK_MAX.
    down_since: std::time::Instant,
    /// Hold уже послан в плеер (после SEEK_HOLD_DELAY): отпускание до
    /// SEEK_CLICK_MAX должно его отменить, а не просто игнорировать.
    hold_sent: bool,
    /// Тап/клик: вспышка подсветки. Липкая: горит, пока её не погасит
    /// следующая перерисовка (сброс в начале апдейта; событие ставит
    /// флаг заново в своём кадре).
    flash: bool,
}

impl SeekButton {
    fn new() -> Self {
        Self {
            down: false,
            down_since: std::time::Instant::now(),
            hold_sent: false,
            flash: false,
        }
    }

    /// Подсвечена: жмут или тапнули и ещё не было перерисовки.
    fn lit(&self) -> bool {
        self.down || self.flash
    }
}

impl Scrub {
    /// Открыть по каталогу записей: для каждой камеры сетапа — последний
    /// её файл; у камеры без записи viewport останется Off (как в live
    /// без камеры).
    pub fn new(
        ctx: &egui::Context,
        setup: Setup,
        assignments: HashMap<String, Pilot>,
        captures: PathBuf,
    ) -> Self {
        let mut players = HashMap::new();
        for camera_id in setup.cameras.keys() {
            let Some(path) = latest_capture(&captures, camera_id) else {
                continue;
            };
            // open асинхронный: фасад всегда возвращается, ошибки файла
            // придут в PlayerState::Failed (viewport останется пустым).
            players.insert(camera_id.clone(), Player::open(&path, ctx.clone()));
        }
        Self {
            players,
            timeline: Timeline::new(),
            rew: SeekButton::new(),
            ff: SeekButton::new(),
            seek_speed: SEEK_SPEEDS.iter().position(|&v| v == 10).unwrap(),
            play_speed: PLAY_SPEEDS.iter().position(|&v| v == 100).unwrap(),
            rpt_size: RPT_SIZES.iter().position(|&v| v == 1.0).unwrap(),
            scroll_acc: 0.0,
            hscroll_acc: 0.0,
            hscroll_active: false,
            hscroll_quiet_since: std::time::Instant::now(),
            scroll_armed: true,
            scroll_quiet_since: std::time::Instant::now(),
            header: header::Header::new(),
            setup,
            assignments,
        }
    }

    /// Первый по порядку сетапа плеер — «первичный»: его fps и позиция
    /// на кнопке PLAY, его кадры считают shown.
    fn primary(&self) -> Option<&Player> {
        self.setup
            .cameras
            .keys()
            .find_map(|id| self.players.get(id))
    }

    /// Один кадр кнопки перемотки. Press ждёт SEEK_HOLD_DELAY: удержали
    /// дольше — в плеер уходит hold (разгон рампой). Отпускание раньше
    /// SEEK_CLICK_MAX — отмена: release в плеер (из паузы — мгновенный
    /// стоп) и фиксированный позиционный шаг с вспышкой. Отпускание
    /// дольше — обычный release (при игре — возврат рампой
    /// SEEK_RELEASE_RAMP). Так тап трэкпадом (press+release в одном
    /// кадре) и короткий клик со щелчком (2–3 кадра) ведут себя
    /// одинаково. back — REW назад.
    fn seek_tick(&mut self, ui: &egui::Ui, response: &egui::Response, back: bool) {
        enum Event {
            Press,
            Release,
            Tap,
            None,
        }
        let pointer_down = ui.input(|i| i.pointer.primary_down());
        let event = {
            let state = if back { &mut self.rew } else { &mut self.ff };
            if state.down && !pointer_down {
                state.down = false;
                Event::Release
            } else if !state.down && response.is_pointer_button_down_on() {
                state.down = true;
                state.down_since = std::time::Instant::now();
                state.hold_sent = false;
                log::info!("scrub: {} PRESS", if back { "REW" } else { "FF" });
                Event::Press
            } else if !state.down && response.clicked() {
                Event::Tap
            } else {
                Event::None
            }
        };
        match event {
            Event::Release => {
                let held_for = if back { self.rew.down_since } else { self.ff.down_since }.elapsed();
                log::info!(
                    "scrub: {} RELEASE held={held_for:?} hold_sent={}",
                    if back { "REW" } else { "FF" },
                    if back { self.rew.hold_sent } else { self.ff.hold_sent }
                );
                let quick = held_for < SEEK_CLICK_MAX;
                if quick {
                    // Отмена: release только если hold успел уйти.
                    if if back { self.rew.hold_sent } else { self.ff.hold_sent } {
                        for player in self.players.values() {
                            player.seek_release(SEEK_RELEASE_RAMP);
                        }
                    }
                    let state = if back { &mut self.rew } else { &mut self.ff };
                    state.flash = true;
                    let step = SEEK_SPEEDS[self.seek_speed] as i64 * SEEK_TAP as i64;
                    self.seek_by(if back { -step } else { step });
                } else {
                    for player in self.players.values() {
                        player.seek_release(SEEK_RELEASE_RAMP);
                    }
                }
            }
            Event::Press => {}
            Event::Tap => {
                log::info!("scrub: {} TAP", if back { "REW" } else { "FF" });
                let state = if back { &mut self.rew } else { &mut self.ff };
                state.flash = true;
                let step = SEEK_SPEEDS[self.seek_speed] as i64 * SEEK_TAP as i64;
                self.seek_by(if back { -step } else { step });
            }
            Event::None => {}
        }
        // Держат дольше SEEK_HOLD_DELAY — рождается hold. Пока жмут и
        // hold не отправлен, крутим перерисовку: иначе во время паузы
        // следующий кадр придёт по секундному тику часов и 100 мс
        // превратятся в секунду.
        let arm = {
            let state = if back { &mut self.rew } else { &mut self.ff };
            if state.down && !state.hold_sent {
                ui.ctx().request_repaint();
            }
            if state.down && !state.hold_sent && state.down_since.elapsed() >= SEEK_HOLD_DELAY {
                state.hold_sent = true;
                log::info!("scrub: {} ARM (hold sent)", if back { "REW" } else { "FF" });
                true
            } else {
                false
            }
        };
        if arm {
            let speed = SEEK_SPEEDS[self.seek_speed] as f64 * if back { -1.0 } else { 1.0 };
            for player in self.players.values() {
                player.seek_hold(speed);
            }
        }
    }

    /// Перемотка всех плееров на delta мс от текущей позиции, с
    /// зажатием в границы файла.
    fn seek_by(&self, delta: i64) {
        // Временный лог для разбора дрожания скролл-скраба.
        log::info!("scrub: seek_by {delta:+} ms");
        for player in self.players.values() {
            let t = (player.position_ms() as i64 + delta).clamp(0, player.duration_ms() as i64);
            player.seek(t as u64);
        }
    }

    pub fn update(&mut self, ui: &mut egui::Ui, navigator: &mut Navigator) {
        enum Action {
            TogglePlay,
            GotoMenu,
            None,
        }
        let mut action = Action::None;
        if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::W)) {
            action = Action::GotoMenu;
        }
        // Вспышки тапов живут до следующей перерисовки: гасим в начале
        // апдейта, seek_tick поставит заново, если тап был в этом кадре.
        self.rew.flash = false;
        self.ff.flash = false;

        // Картинка каждого плеера — в viewport его камеры (та же мапа
        // contents, что в live). fps считаем по первичному.
        let mut contents: HashMap<String, viewfinder::ViewfinderContents> = HashMap::new();
        for (id, player) in self.players.iter_mut() {
            player.update_frame();
            // До готовности — testcard, как у камеры в Starting.
            let kind = match player.open_state() {
                PlayerState::Ready => viewfinder::ViewfinderContents::Texture(player.texture()),
                _ => viewfinder::ViewfinderContents::Pending,
            };
            contents.insert(id.clone(), kind);
        }

        view::Letterbox::new(grid::cell(160, 45))
            .max_virtual_height(grid::cell_y(50))
            .outline(true)
            .show(ui, |ui| {
                let pads: Vec<_> = self.setup.pads.iter().take(MAX_PADS).collect();
                let columns = pads.len() as isize;
                let viewfinder_rows = live::viewfinder_rows(pads.len());
                let viewfinder_cols = live::viewfinder_cols(pads.len());
                let segment = (GRID_WIDTH - LEFT_MARGIN - RIGHT_MARGIN) / columns.max(1);
                let column_geometry: Vec<(isize, isize)> = (0..columns)
                    .map(|i| (LEFT_MARGIN + segment * i, segment))
                    .collect();

                if guidelines::ENABLED {
                    guidelines::marker_grid(ui, grid::cell(10, 5));
                    for pair in column_geometry.windows(2) {
                        let boundary = pair[0].0 + pair[0].1;
                        guidelines::dashed_line(
                            ui,
                            egui::Direction::TopDown,
                            grid::cell_x(boundary),
                        );
                    }
                }

                let bottom_right = grid::cell_at(ui.max_rect().max);
                let header_rect = grid::cell(0, 0).extrude(bottom_right.col, 2);
                self.header.show(ui, header_rect);

                if columns > 0 {
                    let alt = ui.input(|i| i.modifiers.alt);
                    for (i, (pad_id, pad)) in pads.into_iter().enumerate() {
                        let (zone_start, zone_width) = column_geometry[i];
                        let col = zone_start + (zone_width - viewfinder_cols) / 2;
                        let viewfinder_rect = grid::cell(col, TOP_BAR_ROWS)
                            .translate(0, 1)
                            .extrude(viewfinder_cols, viewfinder_rows + 2);
                        let frame_contents = contents
                            .get(&pad.fpv.camera_id)
                            .copied()
                            .unwrap_or(viewfinder::ViewfinderContents::Off);
                        let pilot_name = self
                            .assignments
                            .get(pad_id)
                            .map_or("", |pilot| pilot.name.as_str());
                        let frame_rect = viewfinder_rect.with_min_y(grid::cell_y(TOP_BAR_ROWS + 1));
                        viewfinder::ViewfinderFrame::new(
                            &pad.label,
                            pilot_name,
                            pad.color.to_color32(),
                        )
                        .show(ui, frame_rect, |ui, rect| {
                            viewfinder::Viewfinder::new(pad.fpv.viewport.to_rect()).show(
                                ui,
                                frame_contents,
                                rect,
                            );
                            // Alt: эффективное разрешение вьюпорта в
                            // линиях (высота файла × высота viewport'а).
                            // Правый нижний угол текста — на клетку влево
                            // и вверх от правого нижнего угла картинки.
                            // Пока файл открывается — без значка.
                            if alt {
                                let val =
                                    self.players.get(&pad.fpv.camera_id).and_then(|p| {
                                        match p.open_state() {
                                            PlayerState::Ready => p.resolution().map(|r| {
                                                let lines = r.height as f32
                                                    * (pad.fpv.viewport.bottom
                                                        - pad.fpv.viewport.top);
                                                format!("{}p", lines.round() as u32)
                                            }),
                                            PlayerState::Failed(_) => Some("error".to_owned()),
                                            PlayerState::Opening => None,
                                        }
                                    });
                                if let Some(val) = val {
                                    let row = grid::CELL_SIZE.y;
                                    let width =
                                        super::measure_text(ui.ctx(), style::FONT_REGULAR, &val).x;
                                    let right = rect.right() - grid::cell_x(1);
                                    let bottom = rect.bottom() - grid::cell_y(1);
                                    // Пол-высоты клетки вниз — визуальная
                                    // центровка значка на картинке.
                                    let text = egui::Rect::from_min_max(
                                        egui::pos2(right - width, bottom - row),
                                        egui::pos2(right, bottom),
                                    )
                                    .translate(egui::vec2(0.0, row * 0.5));
                                    view::hint_badge(ui, text, egui::Align2::RIGHT_BOTTOM, &[val]);
                                }
                            }
                        });
                    }
                }

                // Транспорт: плашка 3 клетки высотой (иконка по центру
                // верхних двух + подпись шкалы в нижней), фон — серый в
                // покое, белый пока нажата; текст всегда чёрный. Клик —
                // вся плашка. Ширина: 8 колонок (у RPT 6), зазор 1
                // колонка (перед RPT — 2), левый край — колонка 1.
                let btn_rect = |col: isize, cols: isize| {
                    let x = grid::cell_x(col);
                    let y = grid::cell_y(bottom_right.row - 1);
                    egui::Rect::from_min_max(
                        egui::pos2(x, y - grid::cell_y(2)),
                        egui::pos2(x + grid::cell_x(cols), y + grid::CELL_SIZE.y),
                    )
                };
                // Кнопка — interact и paint раздельно: seek_tick между
                // ними, чтобы кадр нажатия рисовался уже нажатым
                // (иначе белая вспышка терялась между кадрами).
                let transport_interact = |ui: &mut egui::Ui, rect: egui::Rect, id: egui::Id| {
                    ui.interact(rect, id, egui::Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                };
                let transport_paint = |ui: &mut egui::Ui,
                                       rect: egui::Rect,
                                       icon: &str,
                                       pressed: bool,
                                       label: &str| {
                    // Слоновая кость: активный — светлая, покой —
                    // тёмная. Текст всегда чёрный.
                    ui.painter().rect_filled(
                        rect,
                        0.0,
                        if pressed {
                            egui::Color32::from_rgb(0xF5, 0xF0, 0xDC)
                        } else {
                            egui::Color32::from_rgb(0x6E, 0x6E, 0x62)
                        },
                    );
                    let icon_w = super::measure_text(ui.ctx(), style::FONT_REGULAR_X2, icon).x;
                    ui.painter().text(
                        egui::pos2(rect.center().x - icon_w / 2.0, rect.top() + grid::cell_y(1)),
                        egui::Align2::LEFT_CENTER,
                        icon,
                        style::FONT_REGULAR_X2,
                        egui::Color32::BLACK,
                    );
                    ui.painter().text(
                        egui::pos2(rect.center().x, rect.bottom()),
                        egui::Align2::CENTER_BOTTOM,
                        label,
                        style::FONT_REGULAR,
                        egui::Color32::BLACK,
                    );
                };

                let rew_rect = btn_rect(2, 8);
                let rew_id = ui.make_persistent_id("status_rew");
                let rew_response = transport_interact(ui, rew_rect, rew_id);
                // Тап может уложиться в один кадр — виден только clicked(),
                // как на таймлайне.
                self.seek_tick(ui, &rew_response, true);
                transport_paint(
                    ui,
                    rew_rect,
                    "\u{25c0}\u{25c0}",
                    self.rew.lit(),
                    &format!("\u{00d7}{}", SEEK_SPEEDS[self.seek_speed]),
                );

                let playing = self
                    .primary()
                    .is_some_and(|p| p.is_playing() && !p.is_seeking());
                let play_rect = btn_rect(11, 8);
                let play_id = ui.make_persistent_id("status_play");
                let play_response = transport_interact(ui, play_rect, play_id);
                if play_response.clicked() {
                    action = Action::TogglePlay;
                }
                transport_paint(
                    ui,
                    play_rect,
                    "\u{25b6}",
                    playing,
                    &format!("{}%", PLAY_SPEEDS[self.play_speed]),
                );

                let ff_rect = btn_rect(20, 8);
                let ff_id = ui.make_persistent_id("status_ff");
                let ff_response = transport_interact(ui, ff_rect, ff_id);
                self.seek_tick(ui, &ff_response, false);
                transport_paint(
                    ui,
                    ff_rect,
                    "\u{25b6}\u{25b6}",
                    self.ff.lit(),
                    &format!("\u{00d7}{}", SEEK_SPEEDS[self.seek_speed]),
                );

                // RPT: пока некликабельна (повтор фрагмента — потом).
                let loop_rect = btn_rect(30, 6);
                let loop_id = ui.make_persistent_id("status_loop");
                let loop_response = transport_interact(ui, loop_rect, loop_id);
                transport_paint(
                    ui,
                    loop_rect,
                    "\u{F0456}",
                    false,
                    &format!("±{:.1}s", RPT_SIZES[self.rpt_size]),
                );

                // Скролл над контролом — шаг по его шкале. Вверх всегда
                // «больше» (натуральный скролл: знак дельты как есть).
                // Один жест — один шаг: после выстрела разрешаем
                // следующий только после SCROLL_REARM непрерывной тишины.
                let scroll_delta = ui.input(|i| i.smooth_scroll_delta.y);
                if scroll_delta == 0.0 {
                    if self.scroll_quiet_since.elapsed() >= SCROLL_REARM {
                        self.scroll_armed = true;
                        self.scroll_acc = 0.0;
                    }
                } else {
                    self.scroll_quiet_since = std::time::Instant::now();
                    self.scroll_acc += scroll_delta;
                    if self.scroll_armed && self.scroll_acc.abs() >= SCROLL_STEP {
                        self.scroll_armed = false;
                        self.scroll_acc = 0.0;
                        let up = scroll_delta > 0.0;
                        let shift = |cur: &mut usize, len: usize, dir: i64| {
                            *cur = (*cur as i64 + dir).clamp(0, len as i64 - 1) as usize;
                        };
                        if rew_response.hovered() || ff_response.hovered() {
                            // SEEK_SPEEDS убывающий (20x…3x): вверх — к
                            // большей скорости, т.е. к концу списка.
                            shift(
                                &mut self.seek_speed,
                                SEEK_SPEEDS.len(),
                                if up { 1 } else { -1 },
                            );
                        } else if play_response.hovered() {
                            let before = self.play_speed;
                            shift(
                                &mut self.play_speed,
                                PLAY_SPEEDS.len(),
                                if up { -1 } else { 1 },
                            );
                            if self.play_speed != before {
                                let speed = PLAY_SPEEDS[self.play_speed] as f32 / 100.0;
                                for player in self.players.values() {
                                    player.set_speed(speed);
                                }
                            }
                        } else if loop_response.hovered() {
                            shift(&mut self.rpt_size, RPT_SIZES.len(), if up { -1 } else { 1 });
                        }
                    }
                }

                // Время первичного плеера крупно (мм:сс.ммм) — отдельно
                // от таймлайна; таймлайну достаётся rect под линию.
                // Позиция свойственна scrub'у: за правым краем ряда
                // контролов (после RPT). Расчёт намеренно разведён с
                // таймером RACE в live — сведём обратно, когда
                // унифицируем ui.
                if let Some(primary) = self.primary() {
                    let (pos, dur, state) = (
                        primary.position_ms(),
                        primary.duration_ms(),
                        primary.open_state(),
                    );
                    // Позиция — как у таймера RACE в live (колонка
                    // снята с запущенного live, не прикидка): базлайн на
                    // строку выше низа экрана.
                    let text_pos = egui::pos2(grid::cell_x(51), grid::cell_y(bottom_right.row - 1));
                    match &state {
                        PlayerState::Failed(msg) => {
                            ui.painter().text(
                                text_pos,
                                egui::Align2::LEFT_BOTTOM,
                                format!("OPEN FAILED: {:.40}", msg),
                                style::FONT_REGULAR_X2,
                                egui::Color32::LIGHT_RED,
                            );
                        }
                        _ => {
                            // Микро-прототип: гонка внутри записи — отрезки
                            // по RACE_LEAD_MS до и после. Основной таймер —
                            // время гонки: 00:00.000 до 15-й секунды записи,
                            // затем бежит, замирает за 15 с до конца.
                            let race_pos = pos
                                .saturating_sub(RACE_LEAD_MS)
                                .min(dur.saturating_sub(2 * RACE_LEAD_MS));
                            let pos_text = format!(
                                "{:02}:{:02}.{:03}",
                                race_pos / 60_000,
                                (race_pos / 1_000) % 60,
                                race_pos % 1_000
                            );
                            ui.painter().text(
                                text_pos,
                                egui::Align2::LEFT_BOTTOM,
                                &pos_text,
                                style::FONT_REGULAR_X2,
                                egui::Color32::GRAY,
                            );
                            // В отрезках до/после гонки — отступление от
                            // старта/финиша, строкой ниже по центру,
                            // одинарным шрифтом.
                            if state == PlayerState::Ready {
                                let offset = if pos < RACE_LEAD_MS {
                                    Some(format!("-{}", mmssmmm(RACE_LEAD_MS - pos)))
                                } else if dur >= 2 * RACE_LEAD_MS && pos > dur - RACE_LEAD_MS {
                                    Some(format!("+{}", mmssmmm(pos - (dur - RACE_LEAD_MS))))
                                } else {
                                    None
                                };
                                if let Some(offset) = offset {
                                    // Под основным таймером, по центру его
                                    // строки: глифы основного висят выше
                                    // базлайна, доп-таймер — в строке под
                                    // ними. Строка — под базлайном основного
                                    // (play_rect выше кнопки на клетку).
                                    let row = grid::cell_at(play_rect.left_bottom()).row - 1;
                                    let x =
                                        text_pos.x + view::content_width(ui.ctx(), &pos_text) / 2.0;
                                    ui.painter().text(
                                        egui::pos2(x, grid::cell_y(row)),
                                        egui::Align2::CENTER_TOP,
                                        offset,
                                        style::FONT_REGULAR,
                                        egui::Color32::GRAY,
                                    );
                                }
                            }
                            // Шкала — только у готового плеера.
                            if state == PlayerState::Ready {
                                let x0 = text_pos.x
                                    + view::content_width(ui.ctx(), &pos_text)
                                    + grid::cell_x(3);
                                // По вертикали — на уровне таймера:
                                // центр rect = середина его глифов.
                                let rect = egui::Rect::from_min_max(
                                    egui::pos2(x0, text_pos.y - grid::cell_y(2)),
                                    egui::pos2(grid::cell_x(bottom_right.col - 3), text_pos.y),
                                );
                                let frac = if dur > 0 {
                                    pos as f32 / dur as f32
                                } else {
                                    0.0
                                };
                                // Фрагмент активного вылета — всё, кроме
                                // отрезков по RACE_LEAD_MS по краям.
                                let window = (dur > 2 * RACE_LEAD_MS).then(|| {
                                    (
                                        RACE_LEAD_MS as f32 / dur as f32,
                                        (dur - RACE_LEAD_MS) as f32 / dur as f32,
                                    )
                                });
                                // Команды ждут готовности в очереди
                                // worker'а — прозрачно.
                                let (response, events) =
                                    self.timeline.show(ui, rect, frac, dur, window);
                                for event in events {
                                    match event {
                                        TimelineEvent::SetHeld(held) => {
                                            for player in self.players.values() {
                                                player.set_held(held);
                                            }
                                        }
                                        TimelineEvent::Seek(t) => {
                                            for player in self.players.values() {
                                                player.seek(t);
                                            }
                                        }
                                    }
                                }
                                // Горизонтальный скролл над полоской —
                                // относительный скраб: без прыжка под
                                // курсор, дельта двигает позицию от
                                // текущей, как при драге за кружок.
                                // Гистерезис: вход — по накопленной
                                // дельте с порогом, выход — только по
                                // тишине. Микро-скроллы не вводят в
                                // скраб и не выводят из него.
                                // Дельта берётся только над полоской, а
                                // тишина/выход — всегда: иначе увод
                                // курсора с полоски оставляет скраб
                                // активным навсегда (и крутит repaint).
                                let dx = if response.hovered() {
                                    ui.input(|i| i.smooth_scroll_delta.x)
                                } else {
                                    0.0
                                };
                                {
                                    if dx == 0.0 {
                                        // Выход из скраба — по тишине
                                        // короче реарма шкал: иначе
                                        // Grabbing «залипает» на 200 мс.
                                        if self.hscroll_quiet_since.elapsed() >= HSCROLL_RELEASE {
                                            self.hscroll_active = false;
                                            self.hscroll_acc = 0.0;
                                        }
                                    } else {
                                        self.hscroll_quiet_since = std::time::Instant::now();
                                        self.hscroll_acc += dx;
                                        // Всё в физических пикселях пальца:
                                        // и пороги, и масштаб — длина
                                        // записи в расчёт не входит.
                                        // Вход жёстче, чтобы микро-дрожание
                                        // не будило скраб (гистерезис).
                                        let px = self.hscroll_acc * ui.ctx().pixels_per_point();
                                        let enter = if self.hscroll_active { 1.0 } else { 2.0 };
                                        if px.abs() >= enter {
                                            self.hscroll_active = true;
                                            self.seek_by((px * HSCROLL_MS_PER_PX).round() as i64);
                                            self.hscroll_acc = 0.0;
                                        }
                                    }
                                }
                                // Курсор — на весь жест, а не только на
                                // кадры со сиком: иначе Grab/Grabbing
                                // мигали каждый кадр. Пока скраб активен,
                                // крутим перерисовку: иначе во время паузы
                                // таймаут отпускания не сработает — кадров
                                // нет. Крутим вне зависимости от hover:
                                // иначе увод с полоски залипает навсегда.
                                if self.hscroll_active {
                                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                                    ui.ctx().request_repaint();
                                }
                            }
                        }
                    }
                }

                guidelines::dashed_line(
                    ui,
                    egui::Direction::RightToLeft,
                    grid::cell_y(TOP_BAR_ROWS),
                );
                guidelines::dashed_line(
                    ui,
                    egui::Direction::RightToLeft,
                    grid::cell_y(bottom_right.row - 1 - BOTTOM_BAR_ROWS),
                );
            });

        match action {
            Action::GotoMenu => navigator.goto(menu::Menu),
            Action::TogglePlay => {
                for player in self.players.values_mut() {
                    player.toggle();
                }
            }
            Action::None => {}
        }
    }

    /// Команда удалённого управления. Some — обработана; None — не моя,
    /// App ответит unknown.
    pub fn handle_remote(&mut self, command: &[String]) -> Option<crate::remote::Response> {
        match command.first()?.as_str() {
            "toggle-play" => {
                if command.len() > 1 {
                    return Some(crate::remote::Response {
                        message: "play takes no arguments".to_owned(),
                        code: 2,
                    });
                }
                for player in self.players.values_mut() {
                    player.toggle();
                }
                Some(crate::remote::Response {
                    message: String::new(),
                    code: 0,
                })
            }
            // Скорость воспроизведения: set-play-speed <процент>.
            "set-play-speed" => {
                let pct: f32 = match command.get(1).and_then(|a| a.parse().ok()) {
                    Some(p) => p,
                    None => {
                        return Some(crate::remote::Response {
                            message: "set-play-speed takes a percent argument".to_owned(),
                            code: 2,
                        });
                    }
                };
                for player in self.players.values() {
                    player.set_speed(pct / 100.0);
                }
                Some(crate::remote::Response {
                    message: String::new(),
                    code: 0,
                })
            }
            // Жест перемотки для прогона сценариев: seek-hold <x> (знак —
            // направление), seek-release. Дублируют удержание кнопок.
            "seek-hold" => {
                let speed: f64 = match command.get(1).and_then(|a| a.parse().ok()) {
                    Some(s) => s,
                    None => {
                        return Some(crate::remote::Response {
                            message: "seek-hold takes a speed argument".to_owned(),
                            code: 2,
                        });
                    }
                };
                for player in self.players.values() {
                    player.seek_hold(speed);
                }
                Some(crate::remote::Response {
                    message: String::new(),
                    code: 0,
                })
            }
            "seek-release" => {
                for player in self.players.values() {
                    player.seek_release(SEEK_RELEASE_RAMP);
                }
                Some(crate::remote::Response {
                    message: String::new(),
                    code: 0,
                })
            }
            _ => None,
        }
    }
}
