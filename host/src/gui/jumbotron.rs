//! Джамботрон: окно трансляции — вьюфайндеры 2×2. Транслируется
//! контент экрана flight (Idle/Live — камеры, Review — плееры DVR);
//! None (меню) — «Please Stand By».

use crate::gui::flight;
use crate::gui::ruler::{IntoRuler, cell_x, cell_y};
use crate::gui::style;
use crate::gui::view::{self, Letterbox};
use crate::gui::viewfinder::{self, ViewfinderContents};

/// Видео слота сетки: целые клетки (4:3 в px, 32:12 = 512×384).
/// Рамка — фиксированная надбавка: +1 колонка (½+½ по бокам) и +2
/// строки (таб 1¾ + полоса ¼).
const VIDEO_COLS: isize = 32;
const VIDEO_ROWS: isize = 12;
/// Виртуальный кадр: 132 × 37.125 клеток = 2112×1188 px (Letterbox
/// держит виртуальный размер в целых пикселях, клеточного снапа
/// нет). Максимум высоты — 41.25 клеток.
const FRAME_COLS: isize = 132;
const FRAME_ROWS: f32 = 37.125;
const FRAME_MAX_ROWS: f32 = 41.25;
/// Зазор между слотами по горизонтали; блок центрируется по кадру.
/// По вертикали — привязка к верху: поле до первого слота — клетка;
/// зазор между рядами — 3 клетки (посередине — таймер, по пол-клетки
/// до рамок обоих рядов).
const GAP_X: isize = 2;
const TOP_MARGIN: isize = 1;
const GAP_Y: isize = 3;

/// Вход джамботрона: один видимый пад (проекция без лишнего — во
/// flight минимум открытой информации).
pub struct Feed<'a> {
    pub label: &'a str,
    pub pilot: &'a str,
    pub color: egui::Color32,
    pub viewport: egui::Rect,
    pub image: ViewfinderContents,
}

/// Кнопка-тоггл для топ-бара (flight и menu): слот 4×2 клетки у
/// правого края (слева от плашек даты/времени, зазор 2 колонки),
/// раскраска как у EXIT. Только кнопка — на кнопки нижнего ряда
/// flight не влияет. Задуманный глиф
/// (\u{3042e}) в шрифте нет — ближайший по смыслу nf-md-monitor-share.
pub fn jumbo_button(ui: &mut egui::Ui, header: egui::Rect, jumbotron: &mut Jumbotron) {
    // Плашки даты/времени занимают 16 колонок от правого края;
    // кнопка — левее них с зазором в 2 колонки.
    let rect: egui::Rect = header
        .right_top()
        .into_ruler()
        .translate(-cell_x(16 + 2 + 4), 0.0)
        .extrude(cell_x(4), cell_y(2))
        .into();
    let response = view::TransportButton::new(
        "\u{f1483}",
        true,
        false,
        style::EXIT_IDLE,
        style::EXIT_ACTIVE,
    )
    .interact(ui, rect, ui.make_persistent_id("jumbotron_toggle"));
    if response.clicked() {
        jumbotron.toggle();
    }
    view::TransportButton::new(
        "\u{f1483}",
        true,
        response.clicked() || jumbotron.is_open(),
        style::EXIT_IDLE,
        style::EXIT_ACTIVE,
    )
    .paint(ui, rect);
}

/// Окно трансляции. `open` — тоггл из топ-бара: открывается и
/// закрывается только вручную (крестик ОС закрывает).
pub struct Jumbotron {
    open: bool,
}

impl Jumbotron {
    pub fn new() -> Self {
        Self { open: false }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    /// Отрисовать окно (вызывать каждый кадр). `content` — пады
    /// экрана flight (Idle/Live — камеры, Review — плееры); None — вне
    /// flight (меню): «Please Stand By». Закрытие окна (крестик ОС)
    /// гасит тоггл.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        content: Option<&[Feed]>,
        timer: Option<&flight::TimerContents>,
    ) {
        if !self.open {
            return;
        }
        let mut open = true;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("jumbotron"),
            egui::ViewportBuilder::default()
                .with_title("LAPS.Jumbo")
                .with_inner_size([960.0, 540.0]),
            |ui, _class| {
                // Фон окна — черный (полосы вокруг кадра тоже).
                ui.painter()
                    .rect_filled(ui.max_rect(), 0.0, egui::Color32::BLACK);
                // Свой id: Letterbox рисует Area с id от scope_id —
                // у корневого ui он общий с главным окном, без изоляции
                // два леттербокса делят один Area и transform-слой.
                ui.push_id("jumbotron", |ui| {
                    Letterbox::new(egui::vec2(cell_x(FRAME_COLS), cell_y(1) * FRAME_ROWS))
                        .max_virtual_height(cell_y(1) * FRAME_MAX_ROWS)
                        .outline(true)
                        .show(ui, |ui| {
                            match content {
                                // Вне эфира (Review/меню) — «Please Stand
                                // By» по центру кадра: виртуальные
                                // координаты, двойной шрифт.
                                None => {
                                    ui.painter().text(
                                        egui::pos2(
                                            cell_x(FRAME_COLS / 2),
                                            cell_y(1) * FRAME_ROWS / 2.0,
                                        ),
                                        egui::Align2::CENTER_CENTER,
                                        "\u{f0422} Please Stand By",
                                        style::FONT_REGULAR_X2,
                                        style::LAP_FUTURE,
                                    );
                                }
                                Some(feeds) => {
                                    // Сетка 2×2 вьюфайндеров с рамками:
                                    // по горизонтали блок по центру,
                                    // по вертикали — от верха; порядок
                                    // 1 2 / 3 4, пустые четверти — без
                                    // рамок. Поля и зазоры — константы
                                    // выше (между рядами таймер).
                                    let box_cols = VIDEO_COLS + 1;
                                    let box_rows = VIDEO_ROWS + 2;
                                    let x = (FRAME_COLS - (box_cols * 2 + GAP_X)) / 2;
                                    for (i, feed) in feeds.iter().take(4).enumerate() {
                                        let y = TOP_MARGIN + (box_rows + GAP_Y) * (i / 2) as isize;
                                        // Табы — по диагонали от
                                        // центра сетки: левый верхний
                                        // — TopLeft, правый нижний —
                                        // BottomRight и т.д.
                                        let corner = match (i % 2, i / 2) {
                                            (0, 0) => view::Corner::TopLeft,
                                            (1, 0) => view::Corner::TopRight,
                                            (0, 1) => view::Corner::BottomLeft,
                                            _ => view::Corner::BottomRight,
                                        };
                                        let rect: egui::Rect = ui
                                            .max_rect()
                                            .into_ruler()
                                            .collapse_top()
                                            .collapse_left()
                                            .translate(
                                                cell_x(x + (box_cols + GAP_X) * (i % 2) as isize),
                                                cell_y(y),
                                            )
                                            .extrude(cell_x(box_cols), cell_y(box_rows))
                                            .into();
                                        viewfinder::ViewfinderFrame::new(
                                            feed.label, feed.pilot, feed.color, corner,
                                        )
                                        .show(
                                            ui,
                                            rect,
                                            |ui, rect| {
                                                viewfinder::Viewfinder::new(feed.viewport)
                                                    .show(ui, feed.image, rect);
                                            },
                                        );
                                    }
                                    // Таймер — посередине зазора между
                                    // рядами: база по центру зазора +
                                    // полклетки (текст X2 две клетки) —
                                    // по пол-клетки до рамок обоих рядов.
                                    // Горизонтально — центр кадра, флоор
                                    // до клетки. Виджет и контент — из
                                    // flight.
                                    if let Some(timer) = timer {
                                        let w = flight::timer_width(ui.ctx(), timer);
                                        let left = ((cell_x(FRAME_COLS) - w) / 2.0 / cell_x(1))
                                            .floor()
                                            * cell_x(1);
                                        let base_y = cell_y(TOP_MARGIN + box_rows)
                                            + cell_y(GAP_Y) / 2.0
                                            + cell_y(1);
                                        flight::paint_timer(
                                            ui.painter(),
                                            ui.ctx(),
                                            egui::pos2(left, base_y),
                                            timer,
                                        );
                                    }
                                }
                            }
                        });
                });
                if ui.ctx().input(|i| i.viewport().close_requested()) {
                    open = false;
                }
            },
        );
        if !open {
            self.open = false;
        }
    }
}
