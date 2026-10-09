//! Проигрывание своих записей (MJPEG в MKV/MP4/MOV). Фасад `Player`
//! зеркалит `Camera`: io и часы воспроизведения живут в worker-потоке
//! (тело свободной функции с хэндлами, как `capture_loop`), фасад держит
//! текстуру, слот кадров, атомики состояния и канал команд. Тайминги
//! между кадрами святы: темп задают timestamp'ы контейнера, быстрее
//! своих таймингов не играем, отставание — честная ресинхронизация.
//!
//! Открытие асинхронное: `open()` возвращает фасад сразу, индекс и
//! первый кадр строятся в потоке (`OpenState::Opening` → Ready/Failed).
//! Команды, пришедшие до готовности, ждут в очереди канала — готовность
//! ждут сами, прозрачно для вызывающего.
//!
//! Память постоянная: индекс при open даёт максимальный размер JPEG —
//! один буфер на все кадры; декод идёт in-place в `Arc<ColorImage>`
//! (один буфер на всю жизнь), в egui-дельту уходит клон Arc — копий
//! пикселей на кадр нет вовсе.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_utils::atomic::AtomicCell;

use crate::driver::camera::decode::decode_pixels_mjpeg_into;
use crate::driver::camera::fps::FrameStats;
use crate::driver::dvr::{VideoReader, open_reader};

/// Отставание от графика больше этого — не догоняем серией кадров,
/// а перепривязываем часы (резко, честно).
const RESYNC_AFTER: Duration = Duration::from_millis(66);

/// Пауза цикла worker'а, когда дедлайна следующего кадра нет
/// (пауза/hold/конец): чтобы видеть останов по `running`.
const IDLE_WAIT: Duration = Duration::from_millis(100);

/// Кадр из worker-потока → UI-поток (latest-wins), как слот у Camera.
/// Второй элемент — playback-кадр (считается в fps); seek/стоп-кадры false.
type FrameSlot = Arc<Mutex<Option<(Arc<egui::ColorImage>, bool)>>>;

/// Состояние открытия файла: наружу, для индикатора в scrub.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OpenState {
    /// Индекс и первый кадр строятся в worker-потоке.
    Opening,
    Ready,
    Failed(String),
}

/// Команды в worker: пришедшие до готовности ждут в очереди канала.
enum Command {
    Play,
    Pause,
    Seek(u64),
    SetHeld(bool),
}

pub struct Player {
    /// Кадр из worker-потока → UI-поток (latest-wins).
    slot: FrameSlot,
    /// Текстура 1×1 пустышка из open(); первый кадр расширит set'ом.
    /// TextureId стабилен всю жизнь Player.
    texture: egui::TextureHandle,
    state: Arc<Mutex<OpenState>>,
    /// Авторитет у worker'а: конец файла гасит кнопку сам.
    playing: Arc<AtomicCell<bool>>,
    position_ms: Arc<AtomicCell<u64>>,
    /// 0, пока идёт open.
    duration_ms: Arc<AtomicCell<u64>>,
    commands: crossbeam_channel::Sender<Command>,
    /// Останов и join worker-потока (Drop).
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Player {
    /// Открыть файл. Фасад возвращается сразу; индекс, первый кадр
    /// и само воспроизведение — в worker-потоке.
    pub fn open(path: &Path, ctx: egui::Context) -> Self {
        let slot: FrameSlot = Arc::new(Mutex::new(None));
        let state = Arc::new(Mutex::new(OpenState::Opening));
        let playing = Arc::new(AtomicCell::new(false));
        let position_ms = Arc::new(AtomicCell::new(0u64));
        let duration_ms = Arc::new(AtomicCell::new(0u64));
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
            let position_ms = Arc::clone(&position_ms);
            let duration_ms = Arc::clone(&duration_ms);
            let running = Arc::clone(&running);
            let path = path.to_path_buf();
            move || {
                playback_loop(
                    path,
                    ctx,
                    slot,
                    state,
                    playing,
                    position_ms,
                    duration_ms,
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
            position_ms,
            duration_ms,
            commands,
            running,
            thread: Some(thread),
        }
    }

    /// Открытие файла: scrub рисует индикатор, пока Opening.
    pub fn open_state(&self) -> OpenState {
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

    pub fn toggle(&self) {
        if self.is_playing() {
            self.pause();
        } else {
            self.play();
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load()
    }

    /// Захват шкалы: пока держат — сам не играет (это не пауза:
    /// playing сохраняется), отпускание продолжает с показанного кадра.
    pub fn set_held(&self, held: bool) {
        if self.commands.try_send(Command::SetHeld(held)).is_err() {
            log::warn!("player: set_held ignored (worker gone)");
        }
    }

    /// Перемотка на время t (мс от начала): показывается последний кадр
    /// с pts <= t. До готовности ждёт в очереди команд.
    pub fn seek(&self, t: u64) {
        if self.commands.try_send(Command::Seek(t)).is_err() {
            log::warn!("player: seek ignored (worker gone)");
        }
    }

    /// Позиция в файле: pts кадра на экране, мс от начала.
    pub fn position_ms(&self) -> u64 {
        self.position_ms.load()
    }

    /// Общая длительность: pts последнего кадра, мс. 0, пока идёт open.
    pub fn duration_ms(&self) -> u64 {
        self.duration_ms.load()
    }

    /// Текстура последнего кадра. Id стабилен с open().
    pub fn texture(&self) -> egui::TextureId {
        self.texture.id()
    }

    /// Забрать новый кадр из слота, если есть, и обновить текстуру.
    /// true — playback-кадр (UI считает по нему fps); seek/стоп-кадры false.
    pub fn update_frame(&mut self) -> bool {
        let item = self.slot.lock().unwrap().take();
        match item {
            Some((image, presented)) => {
                self.texture.set(image, egui::TextureOptions::NEAREST);
                presented
            }
            None => false,
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
/// воспроизведения. Часы, буферы и reader живут здесь (io + ожидание
/// дедлайнов), UI видит слот и атомики. Зеркало capture_loop: функция
/// без self, состояние — локальный тип.
fn playback_loop(
    path: std::path::PathBuf,
    ctx: egui::Context,
    slot: FrameSlot,
    state: Arc<Mutex<OpenState>>,
    playing: Arc<AtomicCell<bool>>,
    position_ms: Arc<AtomicCell<u64>>,
    duration_ms: Arc<AtomicCell<u64>>,
    running: Arc<AtomicBool>,
    commands: crossbeam_channel::Receiver<Command>,
) {
    /// Состояние воспроизведения: существует только внутри потока.
    struct Playback {
        ctx: egui::Context,
        slot: FrameSlot,
        playing: Arc<AtomicCell<bool>>,
        position_ms: Arc<AtomicCell<u64>>,
        reader: Box<dyn VideoReader>,
        /// pts всех кадров — копия индекса для борьбы с borrow'ами.
        pts: Vec<u64>,
        /// Буфер под JPEG кадр, размер — максимальный в файле.
        jpeg: Vec<u8>,
        /// Пиксельный буфер кадра; в слот уходит Arc-клон.
        image: Arc<egui::ColorImage>,
        dims: crate::driver::dvr::Resolution,
        /// Следующий к показу / сейчас на экране (индексы в pts).
        cursor: usize,
        shown_idx: usize,
        /// Захват шкалы мышью: само по себе не играет.
        held: bool,
        /// (момент старта, pts на экране на тот момент) — часы.
        anchor: Option<(Instant, u64)>,
        /// Показано playback-кадров за сеанс (лог на паузе).
        presented: u32,
        presented_t0: Instant,
        decode: FrameStats,
        read: FrameStats,
        resyncs: u32,
    }

    impl Playback {
        /// Пробуждение UI на кадр. 1 мс вместо немедленного repaint:
        /// egui рендерит дважды на request_repaint, второй проход находит
        /// пустой слот; маленькая задержка даёт один проход на кадр.
        fn wake(&self) {
            self.ctx.request_repaint_after(Duration::from_millis(1));
        }

        /// Часы воспроизведения заново от показанного кадра.
        fn reanchor(&mut self) {
            self.anchor = Some((Instant::now(), self.pts[self.shown_idx]));
        }

        /// Кадр i: read в постоянный JPEG-буфер, декод in-place в image.
        /// presented=false — стоп-кадр/seek (не считается в fps).
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
                self.presented += 1;
            }
            *self.slot.lock().unwrap() = Some((self.image.clone(), presented));
            self.wake();
        }

        /// Один проход часов: показать не больше одного кадра — следующий,
        /// если его дедлайн наступил. Возвращает время до дедлайна
        /// следующего кадра (None — ждать команду: пауза/hold/конец).
        fn tick(&mut self) -> Option<Duration> {
            if self.held {
                return None;
            }
            let Some((wall, base_pts)) = self.anchor else {
                return None;
            };
            if self.cursor >= self.pts.len() {
                self.command(Command::Pause); // конец файла — стоп-кадр
                return None;
            }
            let now_pts = base_pts + wall.elapsed().as_millis() as u64;
            let next_pts = self.pts[self.cursor];
            if next_pts > now_pts {
                // Рано: recv_timeout разбудит ровно на дедлайн.
                return Some(Duration::from_millis(next_pts - now_pts));
            }
            if Duration::from_millis(now_pts.saturating_sub(self.pts[self.shown_idx]))
                > RESYNC_AFTER
            {
                // Отставание: пропускаем устаревшее без показа, часы заново.
                while self.cursor + 1 < self.pts.len()
                    && self.pts[self.cursor + 1].saturating_add(33) <= now_pts
                {
                    self.cursor += 1;
                }
                self.reanchor();
                self.resyncs += 1;
            }

            let i = self.cursor;
            if let Err(e) = self.read_and_present(i, true) {
                log::error!("player: {e}");
                self.command(Command::Pause);
                return None;
            }
            self.shown_idx = i;
            self.cursor = i + 1;
            self.position_ms.store(self.pts[i]);
            if self.cursor >= self.pts.len() {
                self.command(Command::Pause); // конец файла — стоп-кадр
                return None;
            }
            Some(Duration::from_millis(
                self.pts[self.cursor].saturating_sub(now_pts),
            ))
        }

        /// Принимающая сторона команд: вся их логика — в этом матче.
        fn command(&mut self, cmd: Command) {
            match cmd {
                Command::Play => {
                    if !self.playing.load() && self.cursor < self.pts.len() {
                        self.playing.store(true);
                        self.presented = 0;
                        self.presented_t0 = Instant::now();
                        if !self.held {
                            self.reanchor();
                        }
                    }
                }
                // Останов с логом сеанса (fps, decode, read, resyncs).
                Command::Pause => {
                    if self.playing.load() {
                        let elapsed = self.presented_t0.elapsed();
                        log::info!(
                            "player: {} frames in {:?} -> {:.1} fps",
                            self.presented,
                            elapsed,
                            self.presented as f32 / elapsed.as_secs_f32().max(0.001)
                        );
                        if let Some(s) = self.decode.snapshot() {
                            log::info!("player: jpeg→rgba decode duration: {s}");
                        }
                        if let Some(s) = self.read.snapshot() {
                            log::info!("player: frame read duration: {s}");
                        }
                        if self.resyncs > 0 {
                            log::info!("player: {} clock resyncs", self.resyncs);
                        }
                    }
                    self.playing.store(false);
                    self.anchor = None;
                }
                Command::Seek(t) => {
                    // Перемотка на время t: показывается последний
                    // кадр с pts <= t. При игре часы перепривязываются
                    // (пока стоит hold — не нужно: перепривяжем по
                    // отпусканию).
                    let mut next = self.pts.partition_point(|&p| p <= t);
                    if next == 0 {
                        next = 1;
                    }
                    let i = (next - 1).min(self.pts.len().saturating_sub(1));
                    if i != self.shown_idx {
                        match self.read_and_present(i, false) {
                            Ok(()) => {
                                self.shown_idx = i;
                                self.cursor = (i + 1).min(self.pts.len());
                                self.position_ms.store(self.pts[i]);
                                if self.playing.load() && !self.held {
                                    self.reanchor();
                                }
                            }
                            Err(e) => {
                                log::error!("player: {e}");
                                self.command(Command::Pause);
                            }
                        }
                    }
                }
                Command::SetHeld(held) => {
                    if self.held != held {
                        self.held = held;
                        if !held && self.playing.load() {
                            self.reanchor();
                        }
                    }
                }
            }
        }

        /// Цикл: крутим часы, команды влетают по дедлайну кадра.
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

    // --- Open: индекс, первый кадр. Ошибки — в OpenState::Failed. ---

    let reader = match open_reader(&path) {
        Ok(reader) => reader,
        Err(e) => {
            log::error!("player: {}: {e}", path.display());
            *state.lock().unwrap() = OpenState::Failed(e.to_string());
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
        playing,
        position_ms,
        reader,
        pts,
        jpeg,
        image,
        dims,
        cursor: 0,
        shown_idx: 0,
        held: false,
        anchor: None,
        presented: 0,
        presented_t0: Instant::now(),
        decode: FrameStats::default(),
        read: FrameStats::default(),
        resyncs: 0,
    };
    if pb.pts.is_empty() {
        let e = io::Error::new(io::ErrorKind::InvalidData, "no frames");
        log::error!("player: {}: {e}", path.display());
        *state.lock().unwrap() = OpenState::Failed(e.to_string());
        pb.ctx.request_repaint();
        return;
    }
    // «Готов» = «первый кадр на экране», как у Camera.
    if let Err(e) = pb.read_and_present(0, false) {
        log::error!("player: {}: {e}", path.display());
        *state.lock().unwrap() = OpenState::Failed(e.to_string());
        pb.ctx.request_repaint();
        return;
    }
    pb.position_ms.store(pb.pts[0]);
    duration_ms.store(pb.pts.last().copied().unwrap_or(0));
    *state.lock().unwrap() = OpenState::Ready;
    log::info!(
        "player: {} frames, {} ms span, max frame {} bytes",
        pb.pts.len(),
        pb.pts.last().copied().unwrap_or(0),
        pb.reader.max_frame_len()
    );
    pb.ctx.request_repaint();
    pb.run(&commands, &running);
}
