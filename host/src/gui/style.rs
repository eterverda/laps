/// Кегль регулярного начертания; начертание X2 — ровно вдвое (кегль
/// заголовков уровня LAPS: глиф вдвое выше клетки, ширина = клетке).
const FONT_SIZE_REGULAR: f32 = 26.0;

pub const FONT_REGULAR: egui::FontId =
    egui::FontId::new(FONT_SIZE_REGULAR, egui::FontFamily::Monospace);
pub const FONT_REGULAR_X2: egui::FontId =
    egui::FontId::new(FONT_SIZE_REGULAR * 2.0, egui::FontFamily::Monospace);

/// Палитра интерфейса (см. colors-table.html). Схема имён: пары кнопок —
/// `<РОЛЬ>_ACTIVE` / `<РОЛЬ>_IDLE` (яркий — активный фон, неяркий —
/// покой; выключенная = неяркий × 0.35 альфой); таймер — пара
/// `TIMER_ACTIVE` / `TIMER_INACTIVE` (потухшая фаза мигания); лапсы —
/// `LAP_*`. Имена семантические: совпадение цвета у разных имён — не
/// повод для схлопывания (тона лапсов не обязаны вечно следовать таймеру
/// и т.п.) — равенство выражается алиасами прямо здесь.
///
/// EXIT — холодный серый, транспорт — тёплый пластик, REC — алый,
/// RACE — кислотный.
pub const EXIT_ACTIVE: egui::Color32 = egui::Color32::from_rgb(0xAE, 0xBE, 0xC8);
pub const EXIT_IDLE: egui::Color32 = egui::Color32::from_rgb(0x4E, 0x5A, 0x63);
pub const TRANSPORT_ACTIVE: egui::Color32 = egui::Color32::from_rgb(0xE8, 0xE2, 0xCE);
pub const TRANSPORT_IDLE: egui::Color32 = egui::Color32::from_rgb(0x6A, 0x66, 0x56);
pub const REC_ACTIVE: egui::Color32 = egui::Color32::from_rgb(0xEA, 0x31, 0x23);
pub const REC_IDLE: egui::Color32 = egui::Color32::from_rgb(0x6E, 0x42, 0x3C);
pub const RACE_ACTIVE: egui::Color32 = egui::Color32::from_rgb(0xC8, 0xF5, 0x00);
pub const RACE_IDLE: egui::Color32 = egui::Color32::from_rgb(0x75, 0x77, 0x48);

/// Пара таймера: горящий и потухший. Потухший — активный × 0.2 при
/// ПОЛНОЙ альфе: с прозрачностью поверх тёмного фона гаснет насовсем
/// (тот же сигнал мигания: вторая половина секунды отсчёта).
pub const TIMER_ACTIVE: egui::Color32 = TRANSPORT_ACTIVE;
pub const TIMER_INACTIVE: egui::Color32 = egui::Color32::from_rgb(0x2E, 0x2D, 0x29);

/// Пара микро-таймеров отсчёта в пэдах — та же пара, что у основного
/// таймера (свои имена: тон лапсов не обязан вечно следовать таймеру).
pub const PAD_TIMER_ACTIVE: egui::Color32 = TIMER_ACTIVE;
pub const PAD_TIMER_INACTIVE: egui::Color32 = TIMER_INACTIVE;

/// Лапсы.
/// Ненаступившие компоненты круга: номер/время до конца круга (белеют
/// одновременно с его кружком), событие до его метки. Не путать с
/// потухшим (мигание) — отдельный оттенок. Производен:
/// TRANSPORT_ACTIVE.gamma_multiply(0.6), тот же приём, что у
/// TIMER_INACTIVE (× 0.2).
pub const LAP_FUTURE: egui::Color32 = egui::Color32::from_rgb(0x8B, 0x88, 0x7C);
/// Акцент кругов: голубой.
pub const LAP_ACCENT: egui::Color32 = egui::Color32::from_rgb(0x5A, 0xC1, 0xFE);
/// Обводка выделения кликом — акцент выделения, не любой акцент.
pub const LAP_SELECTION: egui::Color32 = LAP_ACCENT;
/// Вспышка свежего события у курсора: заливка цветом активной гонки.
pub const LAP_FLASH: egui::Color32 = RACE_ACTIVE;

/// Лимонадный жёлтый строки достижения (фита) под кругами: светлый,
/// между белым и пластиковым айвори.
pub const FEAT_LEMON: egui::Color32 = egui::Color32::from_rgb(0xF7, 0xED, 0x68);
