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
use super::grid::IntoCell;
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
/// Период повтора перемотки при удержании. Шаг одного повтора —
/// speed × SEEK_REPEAT (10x → 330 мс файла за 33 мс стенки).
const SEEK_REPEAT: Duration = Duration::from_millis(33);
/// Торможение по отпусканию кнопки перемотки при игре: возврат к
/// скорости воспроизведения за это время. Из паузы стоп мгновенный
/// (константа не участвует).
const SEEK_RELEASE_RAMP: Duration = Duration::from_millis(500);
/// Накопленный скролл, дающий один шаг по шкале скорости.
const SCROLL_STEP: f32 = 50.0;
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

    /// Рисует линию и кружок позиции в rect, возвращает события кадра.
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
    ) -> Vec<TimelineEvent> {
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
        if response.hovered() || response.is_pointer_button_down_on() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
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
        events
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
    header: header::Header,
    setup: Setup,
    assignments: HashMap<String, Pilot>,
}

/// Кнопка перемотки: клик — позиционный шаг, удержание — разгон плеера
/// до ±скорости рампой, отпускание — возврат к прежнему режиму рампой.
/// Весь профиль рампы живёт в плеере (resume он запоминает сам).
struct SeekButton {
    down: bool,
}

impl SeekButton {
    fn new() -> Self {
        Self { down: false }
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

    /// Один кадр кнопки перемотки. Нажатие (is_pointer_button_down_on) —
    /// жест hold: плеер разворачивается на ±скорость рампой (длительность
    /// и старт от нуля при постоявшем — внутри плеера); отпускание по
    /// физическому указателю (egui отменяет потенциальный клик при
    /// дрожании — на него опираться нельзя) — жест release: при игре —
    /// возврат к запомненному плеером режиму рампой SEEK_RELEASE_RAMP,
    /// из паузы — мгновенный стоп на текущем кадре (иначе выкат позиции
    /// не даёт попасть в нужное место). Тап в один кадр (press не успели
    /// увидеть, release — clicked) — позиционный шаг, скорость не
    /// трогаем. back — REW назад.
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
                Event::Press
            } else if !state.down && response.clicked() {
                Event::Tap
            } else {
                Event::None
            }
        };
        match event {
            Event::Release => {
                for player in self.players.values() {
                    player.seek_release(SEEK_RELEASE_RAMP);
                }
            }
            Event::Press => {
                let speed = SEEK_SPEEDS[self.seek_speed] as f64 * if back { -1.0 } else { 1.0 };
                for player in self.players.values() {
                    player.seek_hold(speed);
                }
            }
            Event::Tap => {
                // Тап: нажатие и отпускание в одном кадре — просто шаг.
                let step = SEEK_SPEEDS[self.seek_speed] as i64 * SEEK_REPEAT.as_millis() as i64;
                self.seek_by(if back { -step } else { step });
            }
            Event::None => {}
        }
    }

    /// Перемотка всех плееров на delta мс от текущей позиции, с
    /// зажатием в границы файла.
    fn seek_by(&self, delta: i64) {
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

                // REW, PLAY, FF: перемотка ±SEEK_STEP_MS (клик — шаг,
                // удержание — повтор каждые SEEK_REPEAT, пока держим —
                // hold), PLAY тоглит всех плееров. Цвет перемотки: белый
                // пока нажата, иначе серый.
                let rew_rect = grid::cell(1, bottom_right.row).translate(1, -1).extrude(
                    grid::whole_cols(view::content_width(ui.ctx(), "\u{25c0}\u{25c0} REW")),
                    -2,
                );
                let rew_color = if self.rew.down {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::DARK_GRAY
                };
                let rew_response = view::BigButton::new("\u{25c0}\u{25c0} REW", rew_color).show(
                    ui,
                    rew_rect,
                    ui.make_persistent_id("status_rew"),
                );
                // Тап может уложиться в один кадр — виден только clicked(),
                // как на таймлайне.
                self.seek_tick(ui, &rew_response, true);

                let playing = self
                    .primary()
                    .is_some_and(|p| p.is_playing() && !p.is_seeking());
                let play_color = if playing {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::DARK_GRAY
                };
                let play_rect = rew_rect.right_bottom().into_cell().translate(2, 0).extrude(
                    grid::whole_cols(view::content_width(ui.ctx(), "\u{25b6} PLAY")),
                    -2,
                );
                let play_response = view::BigButton::new("\u{25b6} PLAY", play_color).show(
                    ui,
                    play_rect,
                    ui.make_persistent_id("status_play"),
                );
                if play_response.clicked() {
                    action = Action::TogglePlay;
                }

                let ff_rect = play_rect
                    .right_bottom()
                    .into_cell()
                    .translate(2, 0)
                    .extrude(
                        grid::whole_cols(view::content_width(ui.ctx(), "\u{25b6}\u{25b6} FF")),
                        -2,
                    );
                let ff_color = if self.ff.down {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::DARK_GRAY
                };
                let ff_response = view::BigButton::new("\u{25b6}\u{25b6} FF", ff_color).show(
                    ui,
                    ff_rect,
                    ui.make_persistent_id("status_ff"),
                );
                self.seek_tick(ui, &ff_response, false);

                // LOOP: пока некликабельна (повтор фрагмента — потом).
                let loop_rect = ff_rect.right_bottom().into_cell().translate(2, 0).extrude(
                    grid::whole_cols(view::content_width(ui.ctx(), "\u{F0456} RPT")),
                    -2,
                );
                let loop_response = view::BigButton::new("\u{F0456} RPT", egui::Color32::DARK_GRAY)
                    .show(ui, loop_rect, ui.make_persistent_id("status_loop"));

                // Скролл над контролом — шаг по его шкале: вверх — к
                // началу списка (быстрее/больше), вниз — к концу.
                self.scroll_acc += ui.input(|i| i.smooth_scroll_delta.y);
                if self.scroll_acc.abs() >= SCROLL_STEP {
                    let dir = if self.scroll_acc > 0.0 { -1i64 } else { 1 };
                    self.scroll_acc = 0.0;
                    let shift = |cur: &mut usize, len: usize| {
                        *cur = (*cur as i64 + dir).clamp(0, len as i64 - 1) as usize;
                    };
                    if rew_response.hovered() || ff_response.hovered() {
                        shift(&mut self.seek_speed, SEEK_SPEEDS.len());
                    } else if play_response.hovered() {
                        let before = self.play_speed;
                        shift(&mut self.play_speed, PLAY_SPEEDS.len());
                        if self.play_speed != before {
                            let speed = PLAY_SPEEDS[self.play_speed] as f32 / 100.0;
                            for player in self.players.values() {
                                player.set_speed(speed);
                            }
                        }
                    } else if loop_response.hovered() {
                        shift(&mut self.rpt_size, RPT_SIZES.len());
                    }
                }

                // Скорости скролла: подпись под каждым контролом, по центру
                // его текста (не иконки), строкой под кнопками.
                let label_y = grid::cell_y(grid::cell_at(play_rect.left_bottom()).row);
                let speed_label = |ui: &mut egui::Ui, rect: egui::Rect, head: &str, label: &str| {
                    let mut words = head.split(' ').filter(|w| !w.is_empty());
                    let icon = words.next().unwrap_or("");
                    let word = words.next().unwrap_or("");
                    let x = rect.left()
                        + view::content_width(ui.ctx(), icon)
                        + grid::cell_x(1)
                        + view::content_width(ui.ctx(), word) / 2.0;
                    ui.painter().text(
                        egui::pos2(x, label_y),
                        egui::Align2::CENTER_TOP,
                        label,
                        style::FONT_REGULAR,
                        egui::Color32::DARK_GRAY,
                    );
                };
                speed_label(
                    ui,
                    rew_rect,
                    "\u{25c0}\u{25c0} REW",
                    &format!("{}x", SEEK_SPEEDS[self.seek_speed]),
                );
                speed_label(
                    ui,
                    play_rect,
                    "\u{25b6} PLAY",
                    &format!("{}%", PLAY_SPEEDS[self.play_speed]),
                );
                speed_label(
                    ui,
                    ff_rect,
                    "\u{25b6}\u{25b6} FF",
                    &format!("{}x", SEEK_SPEEDS[self.seek_speed]),
                );
                speed_label(
                    ui,
                    loop_rect,
                    "\u{F0456} RPT",
                    &format!("±{:.1}s", RPT_SIZES[self.rpt_size]),
                );
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
                    let text_pos = egui::pos2(
                        loop_rect.right() + grid::cell_x(2),
                        play_rect.left_bottom().y,
                    );
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
                                    // ними.
                                    let row = grid::cell_at(play_rect.left_bottom()).row;
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
                                let rect = egui::Rect::from_min_max(
                                    egui::pos2(x0, play_rect.top()),
                                    egui::pos2(
                                        grid::cell_x(bottom_right.col - 3),
                                        play_rect.bottom(),
                                    ),
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
                                for event in self.timeline.show(ui, rect, frac, dur, window) {
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
