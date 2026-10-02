use self::decode::Error as DecodeError;
pub mod decode;
pub mod fps;

use crate::config::camera::{CameraConfig, FrameRateConfig, PixelConfig};
use crate::driver::capture::{
    self, Capture, CaptureSession, Frame, FrameData, FrameError, FrameSink,
};
use crate::driver::dvr::{Options, RecordState, SharedRecordState};
use crossbeam_utils::atomic::AtomicCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Состояние камеры (capture-потока).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureState {
    /// Поток жив, идёт инициализация (перебор устройств, открытие стрима).
    Starting,
    /// Стрим открыт, кадры идут.
    Live,
    /// Поток мёртв: камера не найдена, открытие не удалось или отвал.
    Dead,
}

/// Общий для потока и UI хэндл состояния камеры.
pub type SharedCaptureState = Arc<AtomicCell<CaptureState>>;

/// HARD HACK / ДИСКЛЕЙМЕР. Камера может отдавать кадры быстрее, чем
/// запрошено: macOS (AVFoundation) часто открывает поток на 60 fps при
/// запросе 30, драйвер вправе игнорировать запрошенный fps вообще. Вместо
/// форков биндингов мы троттлим поток на входе DVR-sink, но только когда
/// эффективный fps записи ≤ 30 (dvr.frame-rate, иначе camera.frame-rate):
/// при запросе больше 30 каждый кадр пишется как есть. Live view
/// обновляется на полном fps камеры. Интервал 25 мс (а не 33.3 мс) выбран
/// так, чтобы при ровном 60 fps источника записывался примерно каждый
/// второй кадр, то есть ~30 fps, не боясь пограничного джиттера.
const RECORD_THROTTLE_MIN_INTERVAL: Duration = Duration::from_millis(25);

/// Возвращает true, если кадр нужно отправить в DVR. Для эффективного
/// fps записи ≤ 30 дропаем кадры, пришедшие раньше 25 мс после последнего
/// записанного; при fps > 30 каждый кадр проходит.
fn should_record_frame(fps: FrameRateConfig, last_recorded_frame: &mut Instant) -> bool {
    if fps.0 > 30 {
        return true;
    }
    if last_recorded_frame.elapsed() >= RECORD_THROTTLE_MIN_INTERVAL {
        *last_recorded_frame = Instant::now();
        true
    } else {
        false
    }
}

type ImageSlot = Arc<Mutex<Option<Arc<capture::Frame>>>>;

/// Команды из UI-потока в capture-поток (управление записью).
enum Command {
    StartRecording(Options),
    StopRecording,
}

/// UI-sink. Latest-wins (P3a): хранит последний кадр, egui-поток забирает
/// его по своему темпу; медленный UI пропускает кадры, но capture-поток
/// его не ждёт — обратного давления нет.
struct UiSink {
    slot: ImageSlot,
    on_frame: Arc<dyn Fn() + Send + Sync>,
}

impl FrameSink for UiSink {
    fn on_frame(&mut self, frame: Arc<capture::Frame>) {
        match &frame.data {
            capture::FrameData::Rgba { .. } => {
                *self.slot.lock().unwrap() = Some(frame);
                (self.on_frame)();
            }
            _ => log::warn!("ui sink: unexpected non-RGBA frame"),
        }
    }
}

/// DVR-sink: держит Recorder, троттлит избыточный поток (HARD HACK выше),
/// конвертирует Instant→epoch millis якорем. Recorder живёт независимо от
/// жизни pipeline: drop sink'а (как и раньше drop recorder'а в потоке)
/// отпускает writer в фоновую финализацию.
struct DvrSink {
    recorder: Option<crate::driver::dvr::Recorder>,
    /// Эффективный fps записи (dvr.frame-rate, иначе camera.frame-rate).
    fps: FrameRateConfig,
    last_recorded_frame: Instant,
    negotiated: capture::CaptureFormat,
    state: SharedRecordState,
    /// Якорь Instant→epoch millis (ставится до цикла кадров).
    anchor: (Instant, u64),
}

impl DvrSink {
    fn new(
        fps: FrameRateConfig,
        negotiated: capture::CaptureFormat,
        state: SharedRecordState,
    ) -> Self {
        Self {
            recorder: None,
            fps,
            last_recorded_frame: Instant::now(),
            negotiated,
            state,
            anchor: (Instant::now(), crate::driver::dvr::epoch_millis()),
        }
    }

    fn set_fps(&mut self, fps: FrameRateConfig) {
        self.fps = fps;
    }

    /// Файл создаётся здесь, а не в writer-потоке: ошибка (диск полон,
    /// нет прав) логируется, RecordState остаётся false — как раньше.
    fn start(&mut self, options: &Options) {
        self.set_fps(FrameRateConfig(options.camera.dvr_frame_rate().0));
        match crate::driver::dvr::Recorder::start(options, &self.negotiated, &self.state) {
            Ok(recorder) => self.recorder = Some(recorder),
            Err(e) => {
                log::error!("dvr: recording unavailable: {}", e);
                self.recorder = None;
            }
        }
    }

    fn stop(&mut self) {
        // drop: writer finalize'ит файл в фоне.
        self.recorder = None;
    }

    fn epoch_millis(&self, t: Instant) -> u64 {
        self.anchor.1 + t.saturating_duration_since(self.anchor.0).as_millis() as u64
    }
}

impl FrameSink for DvrSink {
    fn on_frame(&mut self, frame: Arc<capture::Frame>) {
        if self.recorder.is_none() {
            return;
        }
        if !should_record_frame(self.fps, &mut self.last_recorded_frame) {
            return;
        }
        let ts = self.epoch_millis(frame.timestamp);
        match &frame.data {
            // Инвариант pipeline: до sink'ов Yuyv не доходит.
            FrameData::Yuyv { .. } => log::error!("dvr sink: raw YUYV frame, skipped"),
            _ => self.recorder.as_ref().unwrap().push(frame, ts),
        }
    }

    fn on_stop(&mut self) {
        self.stop();
    }
}

pub fn list_cameras() -> Result<Vec<CameraConfig>, String> {
    let mut devices = capture::Backend::list_devices()?;
    devices.sort_by(|a, b| a.name.cmp(&b.name));
    let mut descriptions = Vec::new();
    for dev in devices {
        for fmt in dev.formats {
            descriptions.push(CameraConfig::new(
                &dev.name,
                fmt.resolution.width,
                fmt.resolution.height,
                fmt.frame_rate.0,
                fmt.format,
            ));
        }
    }
    Ok(descriptions)
}

pub struct Webcam {
    slot: ImageSlot,
    texture: Option<egui::TextureHandle>,
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    camera: SharedCaptureState,
    record: SharedRecordState,
    commands: crossbeam_channel::Sender<Command>,
}

impl Webcam {
    /// Стрим открыт, поток камеры жив.
    pub fn capture_state(&self) -> CaptureState {
        self.camera.load()
    }

    /// Writer жив и пишет на диск.
    pub fn record_state(&self) -> RecordState {
        self.record.load()
    }

    /// Запустить запись. Команда применится перед следующим кадром;
    /// ошибка создания файла уйдёт в лог, RecordState останется false.
    pub fn start_recording(&self, options: Options) {
        if self
            .commands
            .try_send(Command::StartRecording(options))
            .is_err()
        {
            log::warn!("dvr: start_recording ignored (capture thread gone)");
        }
    }

    /// Остановить запись; файл финализируется в фоне. Команда
    /// применится между кадрами, затем — drop recorder'а.
    pub fn stop_recording(&self) {
        if self.commands.try_send(Command::StopRecording).is_err() {
            log::warn!("dvr: stop_recording ignored (capture thread gone)");
        }
    }

    pub fn start(desc: CameraConfig, on_frame: impl Fn() + Send + Sync + 'static) -> Self {
        let slot: ImageSlot = Arc::default();
        let slot_clone = Arc::clone(&slot);
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = Arc::clone(&running);
        let state: SharedCaptureState = Arc::new(AtomicCell::new(CaptureState::Starting));
        let state_clone = Arc::clone(&state);
        let record: SharedRecordState = Arc::new(AtomicCell::new(RecordState {
            ok: false,
            fps: 0.0,
        }));
        let record_clone = Arc::clone(&record);
        let (commands_tx, commands_rx) = crossbeam_channel::bounded(4);

        let thread = thread::spawn(move || {
            // Перебор устройств и форматов — io, не должно висеть на UI-потоке.
            let devices = match capture::Backend::list_devices() {
                Ok(d) => d,
                Err(e) => {
                    log::error!("failed to list devices: {}", e);
                    return;
                }
            };

            let (name, matched) = match devices.into_iter().find_map(|dev| {
                dev.formats
                    .into_iter()
                    .find(|f| {
                        desc.matches(
                            &dev.name,
                            f.resolution.width,
                            f.resolution.height,
                            f.frame_rate.0,
                            f.format.as_str(),
                        )
                    })
                    .map(|f| (dev.name, f))
            }) {
                Some(found) => found,
                None => {
                    log::error!("no matching camera format for {}", desc);
                    return;
                }
            };

            log::info!("starting capture from {} at {:?}", name, matched);

            // open резолвит устройство по имени заново (P5): между
            // перечислением и открытием камеру могли переткнуть.
            let mut session = match capture::Backend::open(&desc) {
                Ok(s) => s,
                Err(e) => {
                    log::error!("failed to open camera: {}", e);
                    return;
                }
            };
            state_clone.store(CaptureState::Live);

            let fmt = session.negotiated();
            log::info!("capture stream opened, format: {:?}", fmt);

            let width = fmt.resolution.width;
            let height = fmt.resolution.height;
            // The camera may open at a higher frame rate than requested
            // (e.g. 60 fps when 30 was asked for). Keep the negotiated
            // stream as-is and drop excess frames in the DVR sink so
            // recording runs at the requested rate; live view is not
            // throttled.
            let mut ui_sink = UiSink {
                slot: slot_clone,
                on_frame: Arc::new(on_frame),
            };
            let mut dvr_sink = DvrSink::new(matched.frame_rate, fmt, record_clone);

            // Для определения ошибки захвата используем не счётчик,
            // потому что frame() может виснуть на несколько секунд,
            // поэтому критерий — время без единого кадра.
            let mut errors_since: Option<Instant> = None;

            loop {
                if !running_clone.load(Ordering::Relaxed) {
                    log::info!("capture thread stopping");
                    break;
                }

                // Команды записи применяем между кадрами: dequeue
                // блокирует до ~периода кадра, задержка незаметна.
                for cmd in commands_rx.try_iter() {
                    match cmd {
                        Command::StartRecording(options) => dvr_sink.start(&options),
                        Command::StopRecording => dvr_sink.stop(),
                    }
                }

                let frame = match session.frame() {
                    Ok(frame) => {
                        errors_since = None;
                        frame
                    }
                    Err(FrameError::Recoverable(e)) => {
                        let since = errors_since.get_or_insert_with(Instant::now);
                        if since.elapsed() >= Duration::from_secs(1) {
                            log::error!("camera lost: no frames for 1s ({}), stopping", e);
                            break;
                        }
                        log::warn!("frame capture error: {}", e);
                        // backoff: ошибка возвращается немедленно, без sleep
                        // был бы busy-loop по ioctl.
                        thread::sleep(Duration::from_millis(16));
                        continue;
                    }
                    Err(FrameError::Unrecoverable(e)) => {
                        log::error!("camera lost: {}, stopping", e);
                        break;
                    }
                };
                let ts = frame.timestamp;

                // Конверсия + fan-out по подпискам sink'ов (P3/P4).
                // Единственный декод на кадр: Jpeg → RGBA для UI,
                // Yuyv → RGBA для обоих sink'ов.
                match frame.data {
                    FrameData::Jpeg { buf, len } => {
                        // Декод до перемещения буфера: borrow, не копия.
                        // Пишем в DVR до применения результата декода:
                        // jpeg-кадр валиден сам по себе (SOI..EOI), а декод
                        // может отказать на кадре, который плеер съел бы —
                        // экранный дроп не должен терять кадр в записи.
                        let decoded = decode::decode_frame(PixelConfig::Mjpeg, &buf, width, height);
                        dvr_sink.on_frame(Arc::new(Frame {
                            timestamp: ts,
                            data: FrameData::Jpeg { buf, len },
                        }));
                        match decoded {
                            Ok(image) => ui_sink.on_frame(Arc::new(Frame {
                                timestamp: ts,
                                data: FrameData::Rgba { rgba: image },
                            })),
                            Err(DecodeError::Recoverable(e)) => {
                                log::warn!("frame skipped: {}", e);
                            }
                            Err(DecodeError::Unrecoverable(e)) => {
                                log::error!("capture stopping: {}", e);
                                break;
                            }
                        }
                    }
                    FrameData::Yuyv { buf } => {
                        match decode::decode_frame(PixelConfig::Yuyv, &buf, width, height) {
                            Ok(image) => {
                                // Один декод — обоим sink'ам бампом счётчика.
                                let frame = Arc::new(Frame {
                                    timestamp: ts,
                                    data: FrameData::Rgba { rgba: image },
                                });
                                dvr_sink.on_frame(frame.clone());
                                ui_sink.on_frame(frame);
                            }
                            Err(DecodeError::Recoverable(e)) => {
                                log::warn!("frame skipped: {}", e);
                            }
                            Err(DecodeError::Unrecoverable(e)) => {
                                log::error!("capture stopping: {}", e);
                                break;
                            }
                        }
                    }
                    // Нативный RGBA от будущих бэкендов: без декода.
                    FrameData::Rgba { rgba } => {
                        let frame = Arc::new(Frame {
                            timestamp: ts,
                            data: FrameData::Rgba { rgba },
                        });
                        dvr_sink.on_frame(frame.clone());
                        ui_sink.on_frame(frame);
                    }
                }
            }

            // Поток умирает (штатный стоп или потеря камеры) — гасим CAM,
            // иначе индикатор висит белым на мёртвой картинке. REC гаснет
            // сам: writer-поток — единственный владелец RecordState.
            state_clone.store(CaptureState::Dead);

            // Recorder дропается здесь (через on_stop sink'а): файл
            // финализируется в фоновом writer-потоке. Session дропается
            // здесь: Drop бэкенда делает stop_stream.
            dvr_sink.on_stop();
            ui_sink.on_stop();
            drop(dvr_sink);
            drop(ui_sink);
        });

        Self {
            slot,
            texture: None,
            running,
            thread: Some(thread),
            camera: state,
            record,
            commands: commands_tx,
        }
    }

    /// Забрать новый кадр из слота, если есть, и обновить текстуру.
    /// Второй элемент tuple — true, если кадр реально забран (один за
    /// коллбэк): UI по нему считает показываемый fps.
    pub fn update(&mut self, ctx: &egui::Context) -> (Option<&egui::TextureHandle>, bool) {
        let image = {
            let mut guard = self.slot.lock().unwrap();
            guard.take()
        };
        let new_frame = image.is_some();

        if let Some(frame) = image {
            let capture::FrameData::Rgba { rgba } = &frame.data else {
                unreachable!("ui slot holds only RGBA frames");
            };
            match &mut self.texture {
                Some(texture) => {
                    texture.set((*rgba).clone(), egui::TextureOptions::NEAREST);
                }
                None => {
                    self.texture = Some(ctx.load_texture(
                        "camera",
                        (*rgba).clone(),
                        egui::TextureOptions::NEAREST,
                    ));
                }
            }
        }

        (self.texture.as_ref(), new_frame)
    }
}

impl Drop for Webcam {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        // Джойним capture-поток: он ждёт текущий кадр и stop_stream.
        // Финализация контейнера не входит в это ожидание — recorder уже
        // отпущен детачем и дописывает файл в фоне.
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                log::error!("capture thread panicked");
            }
        }
    }
}
