//! Воспроизведение записей (scrub): reader контейнера, декод JPEG →
//! текстура, worker-поток с дедлайнами кадров. Плеер — тупой
//! доставщик кадров: какую позицию показывать и с какой скоростью
//! решает транспорт (`service::transport::Transport`, чистый
//! протестированный модуль: позиция = base + ∫скорость, жесты с
//! рампами), экран шлёт ему синки — позицию файла и скорость; worker
//! экстраполирует между синками на своих часах, пейсинг не зависит
//! от частоты кадров UI. Позиция на экране — pts показанного кадра
//! (атомик `shown`), циферблат читает его.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel as crossbeam;
use crossbeam_utils::atomic::AtomicCell;

use crate::driver::camera::decode::decode_pixels_mjpeg_into;
use crate::driver::camera::fps::FrameStats;
use crate::driver::dvr::{Resolution, VideoReader, open_reader};

/// Клапан показа: на высоких скоростях due-кадры идут чаще, чем успевает
/// декодер — чаще одного показа за это время не декодируем вообще, индекс
/// едет за часами без декода.
const MIN_PRESENT_SPAN: Duration = Duration::from_millis(8);

/// Пауза цикла worker'а, когда дедлайна следующего кадра нет
/// (стоп-кадр/конец): чтобы видеть останов по `running`.
const IDLE_WAIT: Duration = Duration::from_millis(100);

/// Кадр из worker-потока → UI-поток (latest-wins), как слот у Camera.
type FrameSlot = Arc<Mutex<Option<Arc<egui::ColorImage>>>>;

/// Состояние открытия файла: наружу, для индикатора в scrub.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PlayerState {
    /// Индекс и первый кадр строятся в worker-потоке.
    Opening,
    Ready,
    Failed(String),
}

/// Холодные метаданные файла: выставляются один раз при открытии,
/// читаются каждый кадр — рядом с состоянием, под общим мьютексом
/// (отдельные атомики им ни к чему: пишутся единожды).
#[derive(Clone, Copy, Debug)]
pub struct Meta {
    /// Pts последнего кадра, мс.
    pub duration: u64,
    /// Интервал последнего кадра (pts[last] − pts[last−1]): последний
    /// кадр «держится» на экране столько же, сколько предпоследний —
    /// для расчёта гэпов покрытие файла = duration + last_step.
    pub last_step: u64,
    pub resolution: Resolution,
}

/// Состояние + метаданные: одна блокировка на холодные данные.
struct Inner {
    state: PlayerState,
    /// None — открытие не завершилось.
    meta: Option<Meta>,
}

/// Команды в worker: пришедшие до готовности ждут в очереди канала.
/// Единственная команда — транспортный синк: позиция файла и знаковая
/// скорость. Позиция — base на момент синка; между синками worker
/// экстраполирует на своих часах (пейсинг не зависит от частоты кадров
/// UI). Скорость 0 — стоп-кадр: worker показывает предшествующий
/// position кадр. Рампы, жесты, круг — модель транспорта, worker их не
/// видит: скачок позиции на синке — обычное дело (seek, заворот круга).
enum Command {
    Sync { position: u64, speed: f64 },
}

pub struct Player {
    /// Кадр из worker-потока → UI-поток (latest-wins).
    slot: FrameSlot,
    /// Текстура 1×1 пустышка из open(); первый кадр расширит set'ом.
    /// TextureId стабилен всю жизнь Player.
    texture: egui::TextureHandle,
    /// Состояние открытия + метаданные (холодные, пишутся один раз).
    inner: Arc<Mutex<Inner>>,
    /// Pts показанного кадра (мс от начала файла) — источник циферблата.
    /// Единственная горячая ячейка: пишет worker при публикации кадра,
    /// читает UI каждый кадр.
    shown: Arc<AtomicCell<u64>>,
    commands: crossbeam::Sender<Command>,
    /// Останов и join worker-потока (Drop).
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Player {
    /// Открыть файл. Фасад возвращается сразу; индекс, первый кадр
    /// и само воспроизведение — в worker-потоке.
    pub fn open(path: &std::path::Path, ctx: egui::Context) -> Self {
        let slot: FrameSlot = Arc::new(Mutex::new(None));
        let inner = Arc::new(Mutex::new(Inner {
            state: PlayerState::Opening,
            meta: None,
        }));
        let shown = Arc::new(AtomicCell::new(0u64));
        // Очередь синков с запасом: UI шлёт их каждый кадр (~60/с),
        // worker забирает по дедлайну; рывок UI-цикла не должен
        // дропать свежий синк (try_send, очередь полна — пропуск).
        let (commands, command_rx) = crossbeam::bounded::<Command>(256);
        let running = Arc::new(AtomicBool::new(true));
        // 1×1-пустышка, как у Camera: id валиден с рождения, размер
        // возьмёт первый кадр. Контент не отрисовывается: до первого
        // кадра scrub показывает индикатор открытия.
        let texture = ctx.load_texture(
            "player",
            egui::ColorImage::new([1, 1], vec![egui::Color32::TRANSPARENT]),
            egui::TextureOptions::NEAREST,
        );
        let thread = thread::spawn({
            let slot = Arc::clone(&slot);
            let inner = Arc::clone(&inner);
            let shown = Arc::clone(&shown);
            let running = Arc::clone(&running);
            let path = path.to_path_buf();
            move || {
                playback_loop(path, ctx, slot, inner, shown, running, command_rx);
            }
        });
        Self {
            slot,
            texture,
            inner,
            shown,
            commands,
            running,
            thread: Some(thread),
        }
    }

    /// Открытие файла: scrub рисует индикатор, пока Opening.
    pub fn open_state(&self) -> PlayerState {
        self.inner.lock().unwrap().state.clone()
    }

    /// Транспортный синк: позиция (мс по файлу) и знаковая скорость.
    /// Worker экстраполирует между синками на своих часах; скорость 0 —
    /// стоп-кадр на предшествующем position кадре.
    pub fn sync(&self, position: u64, speed: f64) {
        if self
            .commands
            .try_send(Command::Sync { position, speed })
            .is_err()
        {
            log::warn!("player: sync ignored (worker gone)");
        }
    }

    /// Pts показанного кадра, мс от начала файла — источник циферблата.
    pub fn shown(&self) -> u64 {
        self.shown.load()
    }

    /// Общая длительность: pts последнего кадра, мс. 0, пока идёт open.
    pub fn duration(&self) -> u64 {
        self.inner.lock().unwrap().meta.map_or(0, |m| m.duration)
    }

    /// Интервал последнего кадра, мс (0 — открытие не завершилось или
    /// кадр один). Покрытие файла для гэпов: duration + last_step —
    /// последний кадр демонстрируется столько же, сколько предпоследний.
    pub fn last_step(&self) -> u64 {
        self.inner.lock().unwrap().meta.map_or(0, |m| m.last_step)
    }

    /// Разрешение файла; None, пока идёт open.
    pub fn resolution(&self) -> Option<Resolution> {
        let meta = self.inner.lock().unwrap().meta;
        meta.and_then(|m| (m.resolution.height > 0).then_some(m.resolution))
    }

    /// Текстура последнего кадра. Id стабилен с open().
    pub fn texture(&self) -> egui::TextureId {
        self.texture.id()
    }

    /// Забрать новый кадр из слота, если есть, и обновить текстуру.
    pub fn update_frame(&mut self) {
        let item = self.slot.lock().unwrap().take();
        if let Some(image) = item {
            self.texture.set(image, egui::TextureOptions::NEAREST);
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Тело worker-потока: open (индекс, первый кадр), затем цикл
/// воспроизведения. Буферы и reader живут здесь (io + ожидание
/// дедлайнов), UI видит слот и атомики. Зеркало capture_loop: функция
/// без self, состояние — локальный тип.
fn playback_loop(
    path: std::path::PathBuf,
    ctx: egui::Context,
    slot: FrameSlot,
    inner: Arc<Mutex<Inner>>,
    shown: Arc<AtomicCell<u64>>,
    running: Arc<AtomicBool>,
    commands: crossbeam::Receiver<Command>,
) {
    /// Состояние воспроизведения: существует только внутри потока.
    struct Playback {
        ctx: egui::Context,
        slot: FrameSlot,
        shown: Arc<AtomicCell<u64>>,
        reader: Box<dyn VideoReader>,
        /// pts всех кадров — тайм-карта: позиция → индекс.
        pts: Vec<u64>,
        /// Длительность: pts последнего кадра (край экстраполяции).
        duration: u64,
        /// Буфер под JPEG кадр, размер — максимальный в файле.
        jpeg: Vec<u8>,
        /// Пиксельный буфер кадра; в слот уходит Arc-клон.
        image: Arc<egui::ColorImage>,
        dims: Resolution,
        /// Сейчас на экране (индекс в pts).
        shown_idx: usize,
        /// Транспортный синк: позиция (файл) на sync_at со скоростью;
        /// между синками позиция = sync_position + speed·(now − sync_at)
        /// — на часах worker'а, пейсинг не зависит от UI.
        sync_at: Instant,
        sync_position: u64,
        sync_speed: f64,
        /// Показали кадр с последнего стопа: граница/стоп-кадр пишут
        /// лог сеанса один раз (переход показ → стоп, не каждый тик).
        playing: bool,
        /// Позиция на прошлом тике: скачок против хода движения —
        /// заворот круга (позиция прыгнула на синке), включает догон shown.
        last_position: u64,
        /// Догон после заворота: shown отстаёт от позиции (прыгнула
        /// назад/вперёд), показываем каждый кадр до встречи с ней.
        catching: bool,
        /// Момент последнего показа — клапан MIN_PRESENT_SPAN.
        last_present: Instant,
        decode: FrameStats,
        read: FrameStats,
        /// Пропущено без декода (высокие скорости: индекс уехал
        /// дальше) — лог на паузе.
        skips: u64,
        /// Овердекоды: кадр опубликован, а UI предыдущий ещё не забрал
        /// (слот был занят) — декод в мусор. Тоже лог на паузе.
        over_decodes: u64,
    }

    impl Playback {
        /// Пробуждение UI на кадр. 1 мс вместо немедленного repaint:
        /// egui рендерит дважды на request_repaint, второй проход находит
        /// пустой слот; маленькая задержка даёт один проход на кадр.
        fn wake(&self) {
            self.ctx.request_repaint_after(Duration::from_millis(1));
        }

        /// Лог сеанса (декод/read/skips/over-decodes) и сброс счётчиков:
        /// каждый блок в логе — «за прогон».
        fn log_session(&mut self) {
            if let Some(s) = self.decode.snapshot() {
                log::info!("player: jpeg→rgba decode duration: {s}");
            }
            if let Some(s) = self.read.snapshot() {
                log::info!("player: frame read duration: {s}");
            }
            if self.skips > 0 {
                log::info!("player: {} frames skipped", self.skips);
            }
            if self.over_decodes > 0 {
                log::info!(
                    "player: {} frames over-decoded (UI behind)",
                    self.over_decodes
                );
            }
        }

        /// Остановка воспроизведения (конец файла, стоп-кадр по синку):
        /// лог сеанса, счётчики сброшены.
        fn stop(&mut self) {
            if self.playing {
                self.log_session();
                self.decode = FrameStats::default();
                self.read = FrameStats::default();
                self.skips = 0;
                self.over_decodes = 0;
            }
            self.playing = false;
        }

        /// Позиция на момент now: экстраполяция последнего синка на
        /// часах worker'а, прижатая к границам файла.
        fn pos_at(&self, now: Instant) -> u64 {
            let dt = now.saturating_duration_since(self.sync_at).as_millis() as f64;
            (self.sync_position as f64 + self.sync_speed * dt).clamp(0.0, self.duration as f64)
                as u64
        }

        /// Кадр i: read в постоянный JPEG-буфер, декод in-place в image.
        /// presented=false — стоп-кадр (клапан показа не трогаем).
        fn read_and_present(&mut self, i: usize, presented: bool) -> io::Result<()> {
            let t0 = Instant::now();
            let (_, len) = self.reader.read_frame_into(i, &mut self.jpeg)?;
            self.read.on_process(t0.elapsed());
            let data = std::mem::take(&mut self.jpeg);
            self.decode_and_publish(&data[..len], presented);
            self.jpeg = data;
            Ok(())
        }

        /// Декод в Arc-буфер in-place; в слот — клон Arc (копии пикселей
        /// нет). Буфер ещё жив в слоте (Arc с refcount > 1) — свежий
        /// буфер, редкий fallback, не горячий путь.
        fn decode_and_publish(&mut self, data: &[u8], presented: bool) {
            let (w, h) = (self.dims.width, self.dims.height);
            let t0 = Instant::now();
            let decoded = match Arc::get_mut(&mut self.image) {
                Some(image) => decode_pixels_mjpeg_into(data, w, h, &mut image.pixels).is_ok(),
                None => false,
            };
            self.decode.on_process(t0.elapsed());
            if !decoded {
                let mut image = egui::ColorImage::new(
                    [w as usize, h as usize],
                    vec![egui::Color32::TRANSPARENT; (w * h) as usize],
                );
                if decode_pixels_mjpeg_into(data, w, h, &mut image.pixels).is_ok() {
                    self.image = Arc::new(image);
                } else {
                    log::warn!("player: frame decode failed");
                    return;
                }
            }
            if presented {
                self.last_present = Instant::now();
            }
            let mut slot = self.slot.lock().unwrap();
            if slot.is_some() {
                // UI предыдущий кадр ещё не забрал — этот декод
                // никто не покажет.
                self.over_decodes += 1;
            }
            *slot = Some(Arc::clone(&self.image));
            self.wake();
        }

        /// Транспортный синк: новая база экстраполяции. Скорость 0 —
        /// стоп-кадр: показываем предшествующий position кадр (один
        /// декод на смену кадра, повторные синки на ту же позицию
        /// бесплатны).
        fn command_sync(&mut self, position: u64, speed: f64) {
            self.sync_at = Instant::now();
            self.sync_position = position;
            self.sync_speed = speed;
            if speed != 0.0 {
                return;
            }
            if self.playing {
                self.stop();
            }
            let next = self.pts.partition_point(|&p| p <= position).max(1);
            let i = (next - 1).min(self.pts.len() - 1);
            if i != self.shown_idx {
                match self.read_and_present(i, false) {
                    Ok(()) => {
                        self.shown_idx = i;
                        self.shown.store(self.pts[i]);
                    }
                    Err(e) => {
                        log::error!("player: {e}");
                        self.stop();
                    }
                }
            }
        }

        /// Один проход: показать не больше одного кадра — последний due
        /// по направлению движения. Возвращает время до ближайшего
        /// события (None — ждать синк: стоп-кадр/конец).
        fn tick(&mut self) -> Option<Duration> {
            let velocity = self.sync_speed;
            if velocity == 0.0 {
                return None;
            }
            let now = Instant::now();
            let position = self.pos_at(now);
            // Заворот круга (позиция прыгнула на синке) против хода
            // движения — «кадр впереди есть всегда», дедлайн по pts не
            // ждём.
            let wrapped = (velocity > 0.0 && position < self.last_position)
                || (velocity < 0.0 && position > self.last_position);
            if wrapped {
                // Заворот: shown догоняет позицию покадрово, монотонная
                // логика дедлайнов до конца круга не работает.
                self.catching = true;
            }
            self.last_position = position;
            let fwd = velocity > 0.0;
            let last = self.pts.len() - 1;
            // Границы: конец файла вперёд, ноль назад — стоп-кадр.
            if (fwd && self.shown_idx == last && position >= self.pts[last])
                || (!fwd && self.shown_idx == 0 && position <= self.pts[0])
            {
                self.stop();
                return None;
            }
            // Целевой кадр: последний с pts <= position.
            let target = (self.pts.partition_point(|&p| p <= position) as i64 - 1)
                .clamp(0, last as i64) as usize;
            // Клапан: показ due-не чаще MIN_PRESENT_SPAN — иначе индекс
            // едет дальше без декода, декодируем «устоявшийся» кадр.
            if self.last_present.elapsed() < MIN_PRESENT_SPAN {
                let due = if self.catching {
                    target != self.shown_idx
                } else if fwd {
                    target > self.shown_idx
                } else {
                    target < self.shown_idx
                };
                if due {
                    return Some(MIN_PRESENT_SPAN - self.last_present.elapsed());
                }
            }
            let wait_ms = |file_ms: u64| {
                Duration::from_millis((file_ms as f64 / velocity.abs().max(0.001)) as u64)
                    .max(Duration::from_millis(1))
            };
            let show = if fwd {
                if target <= self.shown_idx && !self.catching {
                    // Дальше shown ничего не due: ждём дедлайн кадра.
                    return Some(wait_ms(
                        self.pts[self.shown_idx + 1].saturating_sub(position),
                    ));
                }
                target
            } else {
                if target >= self.shown_idx && !self.catching {
                    // position ещё не ушёл ниже shown: ждём пересечения.
                    return Some(wait_ms(position.saturating_sub(self.pts[self.shown_idx])));
                }
                target
            };
            // Бэкпрессher: слот ещё занят — UI предыдущий кадр не
            // забрал (запоздал/окно закрыто), декодить некуда, кадр
            // ушёл бы в мусор. shown пусть догоняет позицию: как слот
            // освободится, покажем последний due. Синки этот клапан не
            // проходят — стоп-кадр по ним однозначен.
            if self.slot.lock().unwrap().is_some() {
                return Some(Duration::from_millis(8));
            }
            if let Err(e) = self.read_and_present(show, true) {
                log::error!("player: {e}");
                self.stop();
                return None;
            }
            // «Воспроизведение» = показали кадр: с этого момента стоп
            // (граница, стоп-кадр по синку) пишет лог сеанса. Стоим на
            // границе с бегущим транспортом — стоп повторный, без лога.
            self.playing = true;
            // Скачок при догоне заворота — переход кольца, не пропуск:
            // каждый кадр кольца всё равно показываем.
            if !self.catching {
                self.skips += self.shown_idx.abs_diff(show) as u64 - 1;
            }
            self.shown_idx = show;
            // Догон завершён: shown встретил позицию — до следующего
            // заворота работает монотонная логика дедлайнов.
            if self.catching && self.shown_idx == target {
                self.catching = false;
            }
            self.shown.store(self.pts[show]);
            if show == last && fwd {
                self.stop(); // конец файла — стоп-кадр
                return None;
            }
            // До следующего события: вперёд — дедлайн кадра, назад —
            // пересечение pts[shown] (уход к shown-1).
            let wait = if fwd {
                self.pts[show + 1].saturating_sub(position)
            } else {
                position.saturating_sub(self.pts[show])
            };
            Some(wait_ms(wait))
        }

        /// Цикл: дедлайн кадра или синк — что раньше.
        fn run(&mut self, commands: &crossbeam::Receiver<Command>, running: &AtomicBool) {
            loop {
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                let wait = self.tick().unwrap_or(IDLE_WAIT);
                match commands.recv_timeout(wait) {
                    Ok(Command::Sync { position, speed }) => self.command_sync(position, speed),
                    Err(crossbeam::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam::RecvTimeoutError::Disconnected) => break,
                }
            }
        }
    }

    // --- Open: индекс, первый кадр. Ошибки — в PlayerState::Failed. ---

    let reader = match open_reader(&path) {
        Ok(reader) => reader,
        Err(e) => {
            log::error!("player: {}: {e}", path.display());
            inner.lock().unwrap().state = PlayerState::Failed(e.to_string());
            ctx.request_repaint();
            return;
        }
    };
    let dims = reader.resolution();
    let pts: Vec<u64> = (0..reader.frame_count())
        .map(|i| reader.timestamp(i))
        .collect();
    let jpeg = vec![0u8; reader.max_frame_len()];
    let image = Arc::new(egui::ColorImage::new(
        [dims.width as usize, dims.height as usize],
        vec![egui::Color32::TRANSPARENT; (dims.width * dims.height) as usize],
    ));
    let mut pb = Playback {
        ctx,
        slot,
        shown,
        reader,
        duration: pts.last().copied().unwrap_or(0),
        pts,
        jpeg,
        image,
        dims,
        shown_idx: 0,
        sync_at: Instant::now(),
        sync_position: 0,
        sync_speed: 0.0,
        playing: false,
        last_position: 0,
        catching: false,
        last_present: Instant::now(),
        decode: FrameStats::default(),
        read: FrameStats::default(),
        skips: 0,
        over_decodes: 0,
    };
    if pb.pts.is_empty() {
        let e = io::Error::new(io::ErrorKind::InvalidData, "no frames");
        log::error!("player: {}: {e}", path.display());
        inner.lock().unwrap().state = PlayerState::Failed(e.to_string());
        pb.ctx.request_repaint();
        return;
    }
    // «Готов» = «первый кадр на экране», как у Camera.
    if let Err(e) = pb.read_and_present(0, false) {
        log::error!("player: {}: {e}", path.display());
        inner.lock().unwrap().state = PlayerState::Failed(e.to_string());
        pb.ctx.request_repaint();
        return;
    }
    pb.shown.store(pb.pts[0]);
    {
        let last_step = pb
            .pts
            .last()
            .zip(pb.pts.get(pb.pts.len().saturating_sub(2)))
            .map(|(last, prev)| last.saturating_sub(*prev))
            .unwrap_or(0);
        let mut guard = inner.lock().unwrap();
        guard.meta = Some(Meta {
            duration: pb.duration,
            last_step,
            resolution: dims,
        });
        guard.state = PlayerState::Ready;
    }
    log::info!(
        "player: {} frames, {} ms span, max frame {} bytes",
        pb.pts.len(),
        pb.duration,
        pb.reader.max_frame_len()
    );
    pb.ctx.request_repaint();
    pb.run(&commands, &running);
}
