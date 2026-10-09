//! Экран scrub: проигрывание записей. Отдельный режим (State), визуально
//! повторяет live: тот же заголовок и раскладка вьюфайндеров (копия,
//! общие — только утилиты live::viewfinder_rows/cols и константы сетки).
//! Плеер на камеру: имя камеры — из имени файла (`{ts}-{camera_id}-mjpeg`),
//! viewport пада берёт картинку своего camera_id, как в live.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
use crate::driver::camera::fps::FpsCounter;
use crate::driver::player::Player;
use crate::model::pilot::Pilot;

const GRID_WIDTH: isize = 160;
const LEFT_MARGIN: isize = 4;
const RIGHT_MARGIN: isize = 4;
const TOP_BAR_ROWS: isize = 2;
const BOTTOM_BAR_ROWS: isize = 2;
const MAX_PADS: usize = 4;

/// Последний по имени .mkv для камеры: `{anything}-{camera_id}-mjpeg.mkv`.
/// Таймстамп-префикс сортируется лексикографически.
fn latest_capture(dir: &Path, camera_id: &str) -> Option<PathBuf> {
    let suffix = format!("-{camera_id}-mjpeg.mkv");
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(&suffix))
        })
        .max()
}

pub struct Scrub {
    /// camera_id → плеер записи этой камеры.
    players: HashMap<String, Player>,
    shown: FpsCounter,
    shown_fps: f32,
    /// Кнопка мыши на таймлайне зажата (на прошлом кадре).
    timeline_down: bool,
    header: header::Header,
    setup: Setup,
    assignments: HashMap<String, Pilot>,
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
            match Player::open(&path, ctx.clone()) {
                Ok(player) => {
                    players.insert(camera_id.clone(), player);
                }
                Err(e) => log::error!("player: {}: {e}", path.display()),
            }
        }
        Self {
            players,
            shown: FpsCounter::default(),
            shown_fps: 0.0,
            timeline_down: false,
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
        let primary_id = self
            .setup
            .cameras
            .keys()
            .find(|id| self.players.contains_key(*id))
            .cloned();
        for (id, player) in self.players.iter_mut() {
            let fresh = match player.update_frame() {
                Ok(fresh) => fresh,
                Err(e) => {
                    log::error!("player: {e}");
                    player.pause();
                    false
                }
            };
            if fresh && Some(id) == primary_id.as_ref() {
                self.shown.on_frame();
            }
            contents.insert(
                id.clone(),
                viewfinder::ViewfinderContents::Texture(player.texture()),
            );
        }
        if let Some(fps) = self.shown.fps() {
            self.shown_fps = fps as f32;
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
                        });
                    }
                }

                // PLAY: цвет и иконка как LIVE у live-экрана. Управляет
                // всеми плеерами разом (старт с общей кнопки — синхронно).
                let playing = self.primary().is_some_and(Player::is_playing);
                let play_color = if playing {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::DARK_GRAY
                };
                let play_text = if playing {
                    if self.shown_fps > 0.0 {
                        format!("{:.0}fps", self.shown_fps)
                    } else {
                        "--fps".to_owned()
                    }
                } else {
                    String::new()
                };
                // Ширина фиксированная, по самому широкому варианту
                // мелкого текста ("--fps"), а не по текущему.
                let play_rect = grid::cell(1, bottom_right.row).translate(1, -1).extrude(
                    grid::whole_cols(view::content_width(ui.ctx(), "\u{e602} PLAY", "--fps")),
                    -2,
                );
                if view::BigButton::new("\u{e602} PLAY", "", &play_text, play_color)
                    .show(ui, play_rect, ui.make_persistent_id("status_play"))
                    .clicked()
                {
                    action = Action::TogglePlay;
                }
                self.show_timeline(ui, play_rect, bottom_right);

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

    /// Нижняя строка scrub: позиция первичного плеера крупно (мм:сс.ммм),
    /// таймлайн до правого края с кружком позиции. Захват мышью — hold
    /// (не пауза: playing сохраняется), клик и таскание перематывают
    /// все плееры по временной шкале.
    fn show_timeline(
        &mut self,
        ui: &mut egui::Ui,
        play_rect: egui::Rect,
        bottom_right: grid::Cell,
    ) {
        let Some((pos, dur)) = self
            .primary()
            .map(|player| (player.position_ms(), player.duration_ms()))
        else {
            return;
        };
        let pos_text = format!(
            "{:02}:{:02}.{:03}",
            pos / 60_000,
            (pos / 1_000) % 60,
            pos % 1_000
        );
        let text_pos = play_rect
            .right_center()
            .into_cell()
            .translate(2, 0)
            .to_pos2();
        ui.painter().text(
            text_pos,
            egui::Align2::LEFT_CENTER,
            &pos_text,
            style::FONT_REGULAR_X2,
            egui::Color32::WHITE,
        );
        let x0 = text_pos.x + view::content_width(ui.ctx(), &pos_text, "") + grid::cell_x(3);
        let x1 = grid::cell_x(bottom_right.col - 3);
        let y = play_rect.center().y;
        ui.painter().line_segment(
            [egui::pos2(x0, y), egui::pos2(x1, y)],
            egui::Stroke::new(2.0, egui::Color32::from_gray(96)),
        );
        let frac = if dur > 0 {
            pos as f32 / dur as f32
        } else {
            0.0
        };
        ui.painter().circle(
            egui::pos2(x0 + (x1 - x0) * frac, y),
            grid::CELL_SIZE.y * 0.24,
            egui::Color32::WHITE,
            egui::Stroke::NONE,
        );
        let timeline_rect = egui::Rect::from_center_size(
            egui::pos2((x0 + x1) / 2.0, y),
            egui::vec2(x1 - x0, grid::CELL_SIZE.y),
        );
        let response = ui.interact(
            timeline_rect,
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
        if down && !self.timeline_down {
            for player in self.players.values_mut() {
                player.set_held(true);
            }
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
            let t = (((pointer.x - x0) / (x1 - x0)).clamp(0.0, 1.0) * dur as f32) as u64;
            for player in self.players.values_mut() {
                if let Err(e) = player.seek_ms(t) {
                    log::error!("player: {e}");
                }
            }
            ui.ctx().request_repaint();
        }
        if !down && self.timeline_down {
            for player in self.players.values_mut() {
                player.set_held(false);
            }
        }
        self.timeline_down = down;
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
            _ => None,
        }
    }
}
