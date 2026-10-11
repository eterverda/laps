//! Проигрывание своих записей (MJPEG в MKV/MP4/MOV). Фасад `Player`
//! зеркалит `Camera`: io и часы воспроизведения живут в worker-потоке
//! (тело свободной функции с хэндлами, как `capture_loop`), фасад держит
//! текстуру, слот кадров, атомики состояния и канал команд. Тайминги
//! между кадрами святы: темп задают timestamp'ы контейнера, быстрее
//! своих таймингов не играем, отставание — честная ресинхронизация.
//!
//! Открытие асинхронное: `open()` возвращает фасад сразу, индекс и
//! первый кадр строятся в потоке (`PlayerState::Opening` → Ready/Failed).
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

/// Клапан показа: на высоких скоростях due-кадры идут чаще, чем успевает
/// декодер — чаще одного показа за это время не декодируем вообще, индекс
/// едет за часами без декода.
const MIN_PRESENT_SPAN: Duration = Duration::from_millis(8);
/// Рампа перемотки: не быстрее чем за это и не жёстче предельного
/// ускорения (x/с) на больших дистанциях.
const SPEED_RAMP_MIN: Duration = Duration::from_millis(1500);
const SPEED_RAMP_A_MAX: f64 = 20.0;
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
/// Первые пять — дискретные (мгновенный эффект, жест отменяют),
/// последние две — жест hold/release на перемотке (единственный
/// источник рамп скорости).
enum Command {
    Play,
    Pause,
    Seek(u64),
    SetHeld(bool),
    /// Скорость воспроизведения мгновенно, 1.0 — штатная.
    SetSpeed(f32),
    /// Жест hold: рампа на ±speed, длительность — внутри плеера
    /// (константы рампы). Стартует часы, если стояли.
    SeekHold {
        speed: f64,
    },
    /// Жест release: возврат к запомненному resume — рампа за dur при
    /// игре, мгновенный стоп при удержании из паузы. Без активного
    /// жеста — no-op.
    SeekRelease {
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
    /// часы крутятся (playing выше — про «время бежит», не про UI).
    seeking: Arc<AtomicCell<bool>>,
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
        let state = Arc::new(Mutex::new(PlayerState::Opening));
        let playing = Arc::new(AtomicCell::new(false));
        let seeking = Arc::new(AtomicCell::new(false));
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
            let seeking = Arc::clone(&seeking);
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
                    seeking,
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
            seeking,
            position_ms,
            duration_ms,
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

    /// Жест hold/release, начатый из паузы: PLAY при этом не
    /// подсвечен (во время play удержание FF/REW играющей кнопки
    /// не гасит).
    pub fn is_seeking(&self) -> bool {
        self.seeking.load()
    }

    /// Жест hold на перемотке: рампа на ±speed (длительность — константы
    /// плеера). Если часы стояли — стартуются от показанного кадра,
    /// отпускание вернёт их в стоп.
    pub fn seek_hold(&self, speed: f64) {
        if self.commands.try_send(Command::SeekHold { speed }).is_err() {
            log::warn!("player: seek_hold ignored (worker gone)");
        }
    }

    /// Жест release: возврат к режиму, бывшему до hold. Если hold шёл
    /// во время play — рампа назад за dur; если из паузы — мгновенный
    /// стоп на текущем кадре (dur не участвует).
    pub fn seek_release(&self, dur: Duration) {
        if self
            .commands
            .try_send(Command::SeekRelease { dur })
            .is_err()
        {
            log::warn!("player: seek_release ignored (worker gone)");
        }
    }

    /// Скорость воспроизведения, 1.0 — штатная. Дискретно: часы
    /// перепривязываются к показанному кадру — смена бесшовная.
    pub fn set_speed(&self, speed: f32) {
        if self.commands.try_send(Command::SetSpeed(speed)).is_err() {
            log::warn!("player: set_speed ignored (worker gone)");
        }
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

/// Часы воспроизведения: позиция = base + ∫скорость. Скорость
/// коммитится мгновенно (дискретные команды) или рампой — только жест
/// hold/release на перемотке. Рампа непрерывна: по завершении часы
/// перепривязываются к позиции на тот момент, скачков нет. Рампа в
/// ноль — остановка: running() false.
struct Clock {
    /// pts на момент started (или последняя позиция, если стоят).
    base: u64,
    /// Момент старта текущего отрезка; None — часы стоят.
    started: Option<Instant>,
    /// Скоммиченная скорость; 0 вместе с started=None — стоп.
    speed: f64,
    /// Активный профиль рампы (жест).
    ramp: Option<Ramp>,
}

/// Профиль рампы: ease-out от from к to за dur (быстрый старт,
/// мягкое приземление).
struct Ramp {
    from: f64,
    to: f64,
    start: Instant,
    dur: Duration,
}

impl Clock {
    fn stopped(base: u64) -> Self {
        Self {
            base,
            started: None,
            speed: 0.0,
            ramp: None,
        }
    }

    fn running(&self) -> bool {
        self.started.is_some()
    }

    /// Старт/перезапуск от позиции со скоростью (рампу снимает).
    fn start(&mut self, base: u64, speed: f64) {
        self.base = base;
        self.started = Some(Instant::now());
        self.speed = speed;
        self.ramp = None;
    }

    /// Стоп: позиция остаётся в base для возможного старта от неё.
    fn stop(&mut self) {
        let (_, pos) = self.eval();
        self.base = pos;
        self.started = None;
        self.ramp = None;
    }

    /// (скорость, позиция) сейчас, без коммита. Позиция — интеграл
    /// скорости, не elapsed × текущая: иначе профиль рампы перелетает
    /// или недолетает цель.
    fn eval(&self) -> (f64, u64) {
        let Some(started) = self.started else {
            return (0.0, self.base);
        };
        let t = started.elapsed().as_millis() as f64;
        match &self.ramp {
            None => (self.speed, (self.base as f64 + self.speed * t) as u64),
            Some(r) => {
                let u = (r.start.elapsed().as_millis() as f64 / r.dur.as_millis() as f64).min(1.0);
                // ∫f, f = 2u − u² (ease-out): T·(u² − u³/3), всё в мс.
                let span = r.from * t
                    + (r.to - r.from) * r.dur.as_millis() as f64 * (u * u - u * u * u / 3.0);
                let speed = r.from + (r.to - r.from) * (2.0 * u - u * u);
                (speed, (self.base as f64 + span).max(0.0) as u64)
            }
        }
    }

    /// Завершившуюся рампу зафиксировать: скорость = to, часы
    /// перепривязываются к позиции на момент завершения. to == 0 —
    /// остановка.
    fn commit(&mut self) {
        let Some(r) = &self.ramp else {
            return;
        };
        if r.start.elapsed() < r.dur {
            return;
        }
        let to = r.to;
        let (_, pos) = self.eval();
        self.speed = to;
        self.ramp = None;
        self.base = pos;
        self.started = (to != 0.0).then(Instant::now);
    }

    /// Дискретная смена скорости: реанкор от текущей позиции, рампу
    /// снимает. Ноль — стоп.
    fn set_speed(&mut self, speed: f64) {
        let (_, pos) = self.eval();
        self.base = pos;
        self.speed = speed;
        self.ramp = None;
        self.started = (speed != 0.0).then(Instant::now);
    }

    /// Запомнить скорость без старта часов: скорость на стоящем плеере
    /// (скролл play_speed во время паузы) — следующий play возьмёт её.
    fn park(&mut self, speed: f64) {
        let (_, pos) = self.eval();
        self.base = pos;
        self.started = None;
        self.speed = speed;
        self.ramp = None;
    }

    /// Рампа к цели за dur, от текущей эффективной скорости. Часы
    /// стояли — стартуются от текущей позиции с нуля.
    fn ramp_to(&mut self, to: f64, dur: Duration) {
        let (from, pos) = self.eval();
        self.base = pos;
        self.speed = from;
        self.started = Some(Instant::now());
        self.ramp = Some(Ramp {
            from,
            to,
            start: Instant::now(),
            dur,
        });
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
    state: Arc<Mutex<PlayerState>>,
    playing: Arc<AtomicCell<bool>>,
    seeking: Arc<AtomicCell<bool>>,
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
        seeking: Arc<AtomicCell<bool>>,
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
        /// Часы воспроизведения (позиция = base + ∫скорость).
        clock: Clock,
        /// Активный жест hold/release на перемотке: resume — скорость,
        /// к которой возвращаемся на release (0 — был стоп).
        seek: Option<SeekGesture>,
        /// Момент последнего показа — клапан MIN_PRESENT_SPAN.
        last_present: Instant,
        decode: FrameStats,
        read: FrameStats,
        /// Пропущено без показа (высокие скорости) — лог на паузе.
        skips: u64,
    }

    /// Жест hold/release: рампа в часах, resume — режим до жеста.
    /// releasing — идёт возврат после отпускания: его коммит закрывает
    /// жест (коммит разгона жест не закрывает — держим на полной
    /// скорости, пока пользователь не отпустил).
    struct SeekGesture {
        resume: f64,
        releasing: bool,
    }

    impl Playback {
        /// Пробуждение UI на кадр. 1 мс вместо немедленного repaint:
        /// egui рендерит дважды на request_repaint, второй проход находит
        /// пустой слот; маленькая задержка даёт один проход на кадр.
        fn wake(&self) {
            self.ctx.request_repaint_after(Duration::from_millis(1));
        }

        /// Отмена жеста (дискретной командой или завершением release).
        fn gesture_end(&mut self) {
            self.seek = None;
            self.seeking.store(false);
        }

        /// Остановка воспроизведения с логом сеанса. Единственная точка
        /// перехода playing → false (Pause, граница файла, рампа в ноль).
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
                    self.skips = 0;
                }
            }
            self.playing.store(false);
            self.clock.stop();
            self.gesture_end();
        }

        /// Перезапуск часов от показанного кадра (после seek/hold).
        fn resume(&mut self) {
            let (speed, _) = self.clock.eval();
            let speed = if speed == 0.0 { 1.0 } else { speed };
            self.clock.start(self.pts[self.shown_idx], speed);
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
            *self.slot.lock().unwrap() = Some(self.image.clone());
            self.wake();
        }

        /// Один проход часов: показать не больше одного кадра — последний
        /// due по направлению движения. Возвращает время до ближайшего
        /// события (None — ждать команду: пауза/hold/конец).
        fn tick(&mut self) -> Option<Duration> {
            if self.held || !self.playing.load() {
                return None;
            }
            self.clock.commit();
            if !self.clock.running() {
                // Рампа сошла в ноль (release к стопу) — фиксируем стоп.
                self.stop();
                return None;
            }
            if self
                .seek
                .as_ref()
                .is_some_and(|g| g.releasing && self.clock.ramp.is_none())
            {
                // Release-рампа догнала resume — жест закрыт, PLAY снова
                // подсвечен (playing остался выше).
                self.gesture_end();
            }
            let (speed, now_pts) = self.clock.eval();
            if speed == 0.0 {
                // Рампа через ноль (разворот): позиция стоит, ждём
                // пересечение с капом RAMP_POLL.
                return Some(RAMP_POLL);
            }
            let fwd = speed > 0.0;
            let last = self.pts.len() - 1;
            // Границы: конец файла вперёд, ноль назад — стоп-кадр.
            if (fwd && self.shown_idx == last && now_pts >= self.pts[last])
                || (!fwd && self.shown_idx == 0 && now_pts <= self.pts[0])
            {
                self.stop();
                return None;
            }
            // Целевой кадр: последний с pts <= now_pts.
            let target = (self.pts.partition_point(|&p| p <= now_pts) as i64 - 1)
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
            let ramping = self.clock.ramp.is_some();
            let wait_ms = |file_ms: u64| {
                let d = Duration::from_millis((file_ms as f64 / speed.abs().max(0.001)) as u64)
                    .max(Duration::from_millis(1));
                // При рампе кап: см. RAMP_POLL.
                if ramping { d.min(RAMP_POLL) } else { d }
            };
            let show = if fwd {
                if target <= self.shown_idx {
                    // Дальше shown ничего не due: ждём дедлайн кадра.
                    return Some(wait_ms(
                        self.pts[self.shown_idx + 1].saturating_sub(now_pts),
                    ));
                }
                target
            } else {
                if target >= self.shown_idx {
                    // now_pts ещё не ушёл ниже shown: ждём пересечения.
                    return Some(wait_ms(now_pts.saturating_sub(self.pts[self.shown_idx])));
                }
                target
            };
            if let Err(e) = self.read_and_present(show, true) {
                log::error!("player: {e}");
                self.stop();
                return None;
            }
            self.skips += self.shown_idx.abs_diff(show) as u64 - 1;
            self.shown_idx = show;
            self.cursor = show + 1;
            self.position_ms.store(self.pts[show]);
            if show == last && fwd {
                self.stop(); // конец файла — стоп-кадр
                return None;
            }
            // До следующего события: вперёд — дедлайн кадра, назад —
            // пересечение pts[shown] (уход к shown-1).
            let wait = if fwd {
                self.pts[show + 1].saturating_sub(now_pts)
            } else {
                now_pts.saturating_sub(self.pts[show])
            };
            Some(wait_ms(wait))
        }

        /// Принимающая сторона команд: вся их логика — в этом матче.
        fn command(&mut self, cmd: Command) {
            match cmd {
                // Дискретные: жест отменяют, эффект мгновенный.
                Command::Play => {
                    if !self.playing.load() && self.cursor < self.pts.len() {
                        // Возобновление всегда вперёд: после перемотки
                        // назад скорость отрицательная, поднимаем до 1.
                        let speed = if self.clock.speed <= 0.0 {
                            1.0
                        } else {
                            self.clock.speed
                        };
                        self.playing.store(true);
                        self.gesture_end();
                        if !self.held {
                            self.clock.start(self.pts[self.shown_idx], speed);
                        }
                    }
                }
                Command::Pause => self.stop(),
                Command::Seek(t) => {
                    // Перемотка на время t: показывается последний
                    // кадр с pts <= t. При игре часы перепривязываются
                    // (пока стоит hold — не нужно: перепривяжем по
                    // отпусканию).
                    self.gesture_end();
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
                                    self.resume();
                                }
                            }
                            Err(e) => {
                                log::error!("player: {e}");
                                self.stop();
                            }
                        }
                    }
                }
                Command::SetSpeed(speed) => {
                    self.gesture_end();
                    if speed == 0.0 {
                        // Нулевая скорость дискретно — пауза.
                        self.stop();
                    } else if self.playing.load() {
                        self.clock.set_speed(speed as f64);
                    } else {
                        self.clock.park(speed as f64);
                    }
                }
                Command::SetHeld(held) => {
                    if self.held != held {
                        self.held = held;
                        if held {
                            self.clock.stop(); // hold ≠ pause: playing остаётся
                        } else if self.playing.load() {
                            self.resume();
                        }
                    }
                }
                // Жест hold/release на перемотке — единственный источник
                // рамп скорости.
                Command::SeekHold { speed } => {
                    let from = self.clock.eval().0;
                    if self.seek.is_none() {
                        let was_playing = self.playing.load();
                        let resume = if was_playing { from } else { 0.0 };
                        self.seek = Some(SeekGesture {
                            resume,
                            releasing: false,
                        });
                        // Тёмный PLAY только у жеста из паузы: во время
                        // play удержание FF/REW — это всё ещё «играет».
                        self.seeking.store(!was_playing);
                    } else if let Some(gesture) = &mut self.seek {
                        gesture.releasing = false;
                    }
                    self.playing.store(true);
                    let dur = std::cmp::max(
                        SPEED_RAMP_MIN,
                        Duration::from_secs_f64((speed - from).abs() / SPEED_RAMP_A_MAX),
                    );
                    self.clock.ramp_to(speed, dur);
                }
                Command::SeekRelease { dur } => {
                    if let Some(gesture) = &mut self.seek {
                        gesture.releasing = true;
                        let to = gesture.resume;
                        if to == 0.0 {
                            // Держали без play — жест был точным скрабом:
                            // стоп мгновенно на текущем кадре, иначе
                            // выкат позиции не даёт попасть в нужное место.
                            self.stop();
                        } else {
                            self.clock.ramp_to(to, dur);
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
        position_ms,
        reader,
        pts,
        jpeg,
        image,
        dims,
        cursor: 0,
        shown_idx: 0,
        held: false,
        clock: Clock::stopped(0),
        seek: None,
        last_present: Instant::now(),
        decode: FrameStats::default(),
        read: FrameStats::default(),
        skips: 0,
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
    pb.position_ms.store(pb.pts[0]);
    duration_ms.store(pb.pts.last().copied().unwrap_or(0));
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

#[cfg(test)]
mod tests {
    use super::*;

    const SLACK: u64 = 30; // допуск на планировщик, мс

    fn approx(a: u64, b: u64) {
        assert!(
            (a as i64 - b as i64).abs() <= SLACK as i64,
            "{a} != {b} ±{SLACK}"
        );
    }

    #[test]
    fn linear_run() {
        let mut c = Clock::stopped(1000);
        c.start(1000, 2.0);
        assert!(c.running());
        std::thread::sleep(Duration::from_millis(100));
        let (speed, pos) = c.eval();
        assert_eq!(speed, 2.0);
        approx(pos, 1200);
    }

    #[test]
    fn ramp_lands_continuously() {
        let mut c = Clock::stopped(0);
        c.start(0, 0.0);
        c.ramp_to(10.0, Duration::from_millis(200));
        std::thread::sleep(Duration::from_millis(250));
        // Коммит непрерывен: позиция в момент коммита не скачет.
        // Полный интеграл ease-out 0→10 за 200 мс: 10·200·(1 − 1/3).
        let (_, before) = c.eval();
        approx(before, 1333);
        c.commit();
        let (_, after) = c.eval();
        approx(before, after);
        // Скоммичено в цель, дальше линейно от неё.
        assert_eq!(c.speed, 10.0);
        assert!(c.ramp.is_none());
        std::thread::sleep(Duration::from_millis(100));
        let (speed, pos) = c.eval();
        assert_eq!(speed, 10.0);
        assert!(pos > after, "после коммита часы идут вперёд");
    }

    #[test]
    fn ramp_to_zero_stops() {
        let mut c = Clock::stopped(5000);
        c.start(5000, 10.0);
        std::thread::sleep(Duration::from_millis(50));
        c.ramp_to(0.0, Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(150));
        c.commit();
        assert!(!c.running());
        let (_, pos) = c.eval();
        assert!(pos < 7000, "за время жизни не улетели далеко: {pos}");
        std::thread::sleep(Duration::from_millis(50));
        let (_, pos2) = c.eval();
        approx(pos, pos2); // стоят: позиция замерзла
    }

    #[test]
    fn reversal_ramp_through_zero() {
        let mut c = Clock::stopped(5000);
        c.start(5000, 1.0);
        std::thread::sleep(Duration::from_millis(20));
        c.ramp_to(-10.0, Duration::from_millis(200));
        // Сразу после старта рампы позиция ещё около старта.
        let (_, pos) = c.eval();
        assert!(pos <= 5100, "разворот ещё не увёз позицию: {pos}");
        std::thread::sleep(Duration::from_millis(250));
        c.commit();
        assert_eq!(c.speed, -10.0);
        let (_, pos) = c.eval();
        assert!(pos < 5000, "после разворота позиция назад: {pos}");
        std::thread::sleep(Duration::from_millis(50));
        let (_, pos2) = c.eval();
        assert!(pos2 < pos, "едем назад");
    }

    #[test]
    fn set_speed_reanchors_without_jump() {
        let mut c = Clock::stopped(0);
        c.start(0, 1.0);
        std::thread::sleep(Duration::from_millis(100));
        let (_, before) = c.eval();
        c.set_speed(3.0);
        let (_, after) = c.eval();
        approx(before, after); // реанкор без скачка
        std::thread::sleep(Duration::from_millis(100));
        let (speed, pos) = c.eval();
        assert_eq!(speed, 3.0);
        approx(pos, before + 300);
    }

    #[test]
    fn ramp_from_stop_starts_at_base() {
        let mut c = Clock::stopped(700);
        assert!(!c.running());
        c.ramp_to(5.0, Duration::from_millis(100));
        assert!(c.running());
        // Сразу после старта: ease-out из нуля — скорость ещё около нуля,
        // позиция строго от base и никуда не уехала.
        let (_, pos) = c.eval();
        approx(pos, 700);
        std::thread::sleep(Duration::from_millis(150));
        c.commit();
        assert!(c.running());
        assert_eq!(c.speed, 5.0);
        let (_, pos) = c.eval();
        assert!(pos > 700);
    }

    #[test]
    fn park_keeps_speed_without_running() {
        let mut c = Clock::stopped(0);
        c.park(1.5);
        assert!(!c.running());
        assert_eq!(c.speed, 1.5);
        // Старт с запаркованной скоростью — продолжаем с неё.
        c.start(0, if c.speed <= 0.0 { 1.0 } else { c.speed });
        std::thread::sleep(Duration::from_millis(100));
        let (speed, pos) = c.eval();
        assert_eq!(speed, 1.5);
        approx(pos, 150);
    }
}
