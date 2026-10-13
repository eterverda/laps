//! Воспроизведение записей (scrub): reader контейнера, декод JPEG →
//! текстура, worker-поток с дедлайнами кадров. Плеер — тупой
//! исполнитель: какой кадр показывать и когда — решает головка
//! (`playhead::Playhead`, чистый протестированный модуль: позиция =
//! base + ∫скорость, жесты с рампами), живущая в этом потоке; время
//! крутится здесь, а не на UI-потоке — квантование кадров UI на
//! воспроизведение не влияет. UI шлёт намерения командами и читает
//! позицию/флаги через атомики.

pub mod playhead;
pub use playhead::Playhead;

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_utils::atomic::AtomicCell;

use crate::driver::camera::decode::decode_pixels_mjpeg_into;
use crate::driver::camera::fps::FrameStats;
use crate::driver::dvr::{VideoReader, open_reader};

/// Клапан показа: на высоких скоростях due-кадры идут чаще, чем успевает
/// декодер — чаще одного показа за это время не декодируем вообще, индекс
/// едет за часами без декода.
const MIN_PRESENT_SPAN: Duration = Duration::from_millis(8);
/// Пока идёт рампа, не спим дольше этого между тиками: дедлайн обратно
/// пропорционален |speed|, возле нуля он раздувается до секунд, и
/// переход через 0 (смена направления) застревает.
const RAMP_POLL: Duration = Duration::from_millis(50);

/// Пауза цикла worker'а, когда дедлайна следующего кадра нет
/// (пауза/hold/конец): чтобы видеть останов по `running`.
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

/// Команды в worker: пришедшие до готовности ждут в очереди канала.
/// Все — намерения для головки; первые пять дискретны (мгновенный
/// эффект, жест отменяют), последние две — жест hold/release на
/// перемотке (единственный источник рамп скорости).
enum Command {
    Play,
    Pause,
    Seek(u64),
    SetDragHeld(bool),
    /// Скорость воспроизведения мгновенно, 1.0 — штатная.
    SetSpeed(f32),
    /// Шаттл: рампа к ±velocity, длительность — внутри головки.
    /// Стартует время, если стояло.
    Shuttle {
        velocity: f64,
    },
    /// Отпускание шаттла: возврат к запомненному resume — рампа за dur
    /// при игре, мгновенный стоп при удержании из паузы.
    ShuttleRelease {
        dur: Duration,
    },
}

pub struct Player {
    /// Кадр из worker-потока → UI-поток (latest-wins).
    slot: FrameSlot,
    /// Текстура 1×1 пустышка из open(); первый кадр расширит set'ом.
    /// TextureId стабилен всю жизнь Player.
    texture: egui::TextureHandle,
    state: Arc<Mutex<PlayerState>>,
    /// Авторитет у worker'а: конец файла гасит кнопку сам.
    playing: Arc<AtomicCell<bool>>,
    /// Жест hold/release, начатый из паузы: PLAY при нём тёмный, хотя
    /// время крутится (playing выше — про «время бежит», не про UI).
    seeking: Arc<AtomicCell<bool>>,
    position: Arc<AtomicCell<u64>>,
    /// 0, пока идёт open.
    duration: Arc<AtomicCell<u64>>,
    /// Разрешение файла; 0×0, пока идёт open.
    resolution: Arc<AtomicCell<crate::driver::dvr::Resolution>>,
    commands: crossbeam_channel::Sender<Command>,
    /// Останов и join worker-потока (Drop).
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Player {
    /// Открыть файл. Фасад возвращается сразу; индекс, первый кадр
    /// и само воспроизведение — в worker-потоке.
    pub fn open(path: &std::path::Path, ctx: egui::Context) -> Self {
        let slot: FrameSlot = Arc::new(Mutex::new(None));
        let state = Arc::new(Mutex::new(PlayerState::Opening));
        let playing = Arc::new(AtomicCell::new(false));
        let seeking = Arc::new(AtomicCell::new(false));
        let position = Arc::new(AtomicCell::new(0u64));
        let duration = Arc::new(AtomicCell::new(0u64));
        let resolution = Arc::new(AtomicCell::new(crate::driver::dvr::Resolution {
            width: 0,
            height: 0,
        }));
        let (commands, command_rx) = crossbeam_channel::bounded::<Command>(32);
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
            let state = Arc::clone(&state);
            let playing = Arc::clone(&playing);
            let seeking = Arc::clone(&seeking);
            let position = Arc::clone(&position);
            let duration = Arc::clone(&duration);
            let resolution = Arc::clone(&resolution);
            let running = Arc::clone(&running);
            let path = path.to_path_buf();
            move || {
                playback_loop(
                    path,
                    ctx,
                    slot,
                    state,
                    playing,
                    seeking,
                    position,
                    duration,
                    resolution,
                    running,
                    command_rx,
                );
            }
        });
        Self {
            slot,
            texture,
            state,
            playing,
            seeking,
            position,
            duration,
            resolution,
            commands,
            running,
            thread: Some(thread),
        }
    }

    /// Открытие файла: scrub рисует индикатор, пока Opening.
    pub fn open_state(&self) -> PlayerState {
        self.state.lock().unwrap().clone()
    }

    pub fn play(&self) {
        if self.commands.try_send(Command::Play).is_err() {
            log::warn!("player: play ignored (worker gone)");
        }
    }

    pub fn pause(&self) {
        if self.commands.try_send(Command::Pause).is_err() {
            log::warn!("player: pause ignored (worker gone)");
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load()
    }

    /// Жест hold/release, начатый из паузы: PLAY при этом не
    /// подсвечен (во время play удержание FF/REW играющей кнопки
    /// не гасит).
    pub fn is_seeking(&self) -> bool {
        self.seeking.load()
    }

    /// Жест шаттла (удержание REW/FF): рампа к ±velocity (длительность
    /// — у головки). Если время стояло — стартует от показанного кадра,
    /// отпускание вернёт его в стоп.
    pub fn shuttle(&self, velocity: f64) {
        if self.commands.try_send(Command::Shuttle { velocity }).is_err() {
            log::warn!("player: shuttle ignored (worker gone)");
        }
    }

    /// Отпускание шаттла: возврат к режиму до него. Если шёл во время
    /// play — рампа назад за dur; если из паузы — мгновенный стоп на
    /// текущем кадре (dur не участвует).
    pub fn shuttle_release(&self, dur: Duration) {
        if self
            .commands
            .try_send(Command::ShuttleRelease { dur })
            .is_err()
        {
            log::warn!("player: shuttle_release ignored (worker gone)");
        }
    }

    /// Скорость воспроизведения, 1.0 — штатная. Дискретно: время
    /// перепривязывается к показанному кадру — смена бесшовная.
    pub fn set_speed(&self, speed: f32) {
        if self.commands.try_send(Command::SetSpeed(speed)).is_err() {
            log::warn!("player: set_speed ignored (worker gone)");
        }
    }

    /// Захват шкалы: пока держат — время заморожено (это не пауза:
    /// playing сохраняется), отпускание продолжает с показанного кадра.
    pub fn set_drag_held(&self, held: bool) {
        if self.commands.try_send(Command::SetDragHeld(held)).is_err() {
            log::warn!("player: set_drag_held ignored (worker gone)");
        }
    }

    /// Перемотка на позицию (мс от начала): показывается последний
    /// кадр с pts <= position. До готовности ждёт в очереди команд.
    pub fn seek(&self, position: u64) {
        if self.commands.try_send(Command::Seek(position)).is_err() {
            log::warn!("player: seek ignored (worker gone)");
        }
    }

    /// Позиция на экране: pts показанного кадра, мс от начала.
    pub fn position(&self) -> u64 {
        self.position.load()
    }

    /// Общая длительность: pts последнего кадра, мс. 0, пока идёт open.
    pub fn duration(&self) -> u64 {
        self.duration.load()
    }

    /// Разрешение файла; None, пока идёт open.
    pub fn resolution(&self) -> Option<crate::driver::dvr::Resolution> {
        let r = self.resolution.load();
        (r.height > 0).then_some(r)
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
/// воспроизведения. Головка, буферы и reader живут здесь (io +
/// ожидание дедлайнов), UI видит слот и атомики. Зеркало capture_loop:
/// функция без self, состояние — локальный тип.
fn playback_loop(
    path: std::path::PathBuf,
    ctx: egui::Context,
    slot: FrameSlot,
    state: Arc<Mutex<PlayerState>>,
    playing: Arc<AtomicCell<bool>>,
    seeking: Arc<AtomicCell<bool>>,
    position: Arc<AtomicCell<u64>>,
    duration: Arc<AtomicCell<u64>>,
    resolution: Arc<AtomicCell<crate::driver::dvr::Resolution>>,
    running: Arc<AtomicBool>,
    commands: crossbeam_channel::Receiver<Command>,
) {
    /// Состояние воспроизведения: существует только внутри потока.
    struct Playback {
        ctx: egui::Context,
        slot: FrameSlot,
        playing: Arc<AtomicCell<bool>>,
        seeking: Arc<AtomicCell<bool>>,
        position: Arc<AtomicCell<u64>>,
        reader: Box<dyn VideoReader>,
        /// pts всех кадров — тайм-карта: позиция → индекс.
        pts: Vec<u64>,
        /// Буфер под JPEG кадр, размер — максимальный в файле.
        jpeg: Vec<u8>,
        /// Пиксельный буфер кадра; в слот уходит Arc-клон.
        image: Arc<egui::ColorImage>,
        dims: crate::driver::dvr::Resolution,
        /// Сейчас на экране (индекс в pts).
        shown_idx: usize,
        /// Захват шкалы мышью/скроллом: время заморожено, само по
        /// себе не играет (playing сохраняется).
        held: bool,
        /// Головка: единственная модель времени (скорости, жесты,
        /// рампы — всё внутри, чистый модуль).
        head: Playhead,
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

        /// Остановка воспроизведения с логом сеанса. Единственная точка
        /// перехода playing → false (Pause, граница файла, рампа в ноль).
        /// Счётчики сеанса (декод/read/skips/over-decodes) после лога
        /// сбрасываются все: каждый блок в логе — «за прогон».
        fn stop(&mut self) {
            if self.playing.load() {
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
                self.decode = FrameStats::default();
                self.read = FrameStats::default();
                self.skips = 0;
                self.over_decodes = 0;
            }
            self.playing.store(false);
            self.seeking.store(false);
            self.head.pause();
        }

        /// Кадр i: read в постоянный JPEG-буфер, декод in-place в image.
        /// presented=false — стоп-кадр/seek (клапан показа не трогаем).
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
            *slot = Some(self.image.clone());
            self.wake();
        }

        /// Один проход головки: показать не больше одного кадра —
        /// последний due по направлению движения. Возвращает время до
        /// ближайшего события (None — ждать команду: пауза/hold/конец).
        fn tick(&mut self) -> Option<Duration> {
            if self.held {
                return None;
            }
            let was_playing = self.playing.load();
            self.head.sync();
            if was_playing && !self.head.is_running() {
                // Граница файла или рампа в ноль — фиксируем стоп.
                self.stop();
                return None;
            }
            if !self.head.is_running() {
                return None;
            }
            self.playing.store(true);
            self.seeking.store(self.head.is_seeking());
            let velocity = self.head.velocity();
            if velocity == 0.0 {
                // Рампа через ноль (разворот): позиция стоит, ждём
                // пересечение с капом RAMP_POLL.
                return Some(RAMP_POLL);
            }
            let position = self.head.position();
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
                let due = if fwd {
                    target > self.shown_idx
                } else {
                    target < self.shown_idx
                };
                if due {
                    return Some(MIN_PRESENT_SPAN - self.last_present.elapsed());
                }
            }
            let ramping = self.head.is_ramping();
            let wait_ms = |file_ms: u64| {
                let d = Duration::from_millis((file_ms as f64 / velocity.abs().max(0.001)) as u64)
                    .max(Duration::from_millis(1));
                // При рампе кап: см. RAMP_POLL.
                if ramping { d.min(RAMP_POLL) } else { d }
            };
            let show = if fwd {
                if target <= self.shown_idx {
                    // Дальше shown ничего не due: ждём дедлайн кадра.
                    return Some(wait_ms(
                        self.pts[self.shown_idx + 1].saturating_sub(position),
                    ));
                }
                target
            } else {
                if target >= self.shown_idx {
                    // position ещё не ушёл ниже shown: ждём пересечения.
                    return Some(wait_ms(position.saturating_sub(self.pts[self.shown_idx])));
                }
                target
            };
            // Бэкпрессher: слот ещё занят — UI предыдущий кадр не
            // забрал (запоздал/окно закрыто), декодить некуда, кадр
            // ушёл бы в мусор. shown пусть догоняет позицию: как слот
            // освободится, покажем последний due. Дискретные seek'и
            // (пауза) этот клапан не проходят — их результат однозначен.
            if self.slot.lock().unwrap().is_some() {
                return Some(Duration::from_millis(8));
            }
            if let Err(e) = self.read_and_present(show, true) {
                log::error!("player: {e}");
                self.stop();
                return None;
            }
            self.skips += self.shown_idx.abs_diff(show) as u64 - 1;
            self.shown_idx = show;
            self.position.store(self.pts[show]);
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

        /// Принимающая сторона команд: вся их логика — в этом матче.
        fn command(&mut self, cmd: Command) {
            match cmd {
                // Дискретные: жест отменяют, эффект мгновенный.
                Command::Play => {
                    if !self.playing.load() {
                        self.playing.store(true);
                        if !self.held {
                            self.head.play();
                        }
                    }
                }
                Command::Pause => self.stop(),
                Command::Seek(position) => {
                    // Перемотка на позицию: показывается последний
                    // кадр с pts <= position. При игре время
                    // перепривязывается с resume-скоростью; пока стоит
                    // drag — позиция просто переносится (перепривяжем
                    // по отпусканию).
                    self.head.seek(position);
                    let mut next = self.pts.partition_point(|&p| p <= position);
                    if next == 0 {
                        next = 1;
                    }
                    let i = (next - 1).min(self.pts.len().saturating_sub(1));
                    if i != self.shown_idx {
                        match self.read_and_present(i, false) {
                            Ok(()) => {
                                self.shown_idx = i;
                                self.position.store(self.pts[i]);
                            }
                            Err(e) => {
                                log::error!("player: {e}");
                                self.stop();
                            }
                        }
                    }
                }
                Command::SetSpeed(speed) => self.head.set_speed(speed as f64),
                Command::SetDragHeld(held) => {
                    if self.held != held {
                        self.held = held;
                        self.head.set_drag_held(held);
                    }
                }
                // Шаттл — единственный источник рамп скорости.
                Command::Shuttle { velocity } => self.head.shuttle(velocity),
                Command::ShuttleRelease { dur } => self.head.shuttle_release(dur),
            }
        }

        /// Цикл: крутим головку, команды влетают по дедлайну кадра.
        fn run(&mut self, commands: &crossbeam_channel::Receiver<Command>, running: &AtomicBool) {
            loop {
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                let wait = self.tick().unwrap_or(IDLE_WAIT);
                match commands.recv_timeout(wait) {
                    Ok(cmd) => self.command(cmd),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }
            }
        }
    }

    // --- Open: индекс, первый кадр. Ошибки — в PlayerState::Failed. ---

    let reader = match open_reader(&path) {
        Ok(reader) => reader,
        Err(e) => {
            log::error!("player: {}: {e}", path.display());
            *state.lock().unwrap() = PlayerState::Failed(e.to_string());
            ctx.request_repaint();
            return;
        }
    };
    let dims = reader.resolution();
    resolution.store(dims);
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
        playing,
        seeking,
        position,
        reader,
        pts,
        jpeg,
        image,
        dims,
        shown_idx: 0,
        held: false,
        head: Playhead::new(Instant::now(), 0),
        last_present: Instant::now(),
        decode: FrameStats::default(),
        read: FrameStats::default(),
        skips: 0,
        over_decodes: 0,
    };
    if pb.pts.is_empty() {
        let e = io::Error::new(io::ErrorKind::InvalidData, "no frames");
        log::error!("player: {}: {e}", path.display());
        *state.lock().unwrap() = PlayerState::Failed(e.to_string());
        pb.ctx.request_repaint();
        return;
    }
    // «Готов» = «первый кадр на экране», как у Camera.
    if let Err(e) = pb.read_and_present(0, false) {
        log::error!("player: {}: {e}", path.display());
        *state.lock().unwrap() = PlayerState::Failed(e.to_string());
        pb.ctx.request_repaint();
        return;
    }
    // Головка без длительности ничего не решает — ставим настоящую
    // (конец файла, стоп на границах, clamp позиции).
    pb.head = Playhead::new(Instant::now(), pb.pts.last().copied().unwrap_or(0));
    pb.position.store(pb.pts[0]);
    duration.store(pb.pts.last().copied().unwrap_or(0));
    *state.lock().unwrap() = PlayerState::Ready;
    log::info!(
        "player: {} frames, {} ms span, max frame {} bytes",
        pb.pts.len(),
        pb.pts.last().copied().unwrap_or(0),
        pb.reader.max_frame_len()
    );
    pb.ctx.request_repaint();
    pb.run(&commands, &running);
}
