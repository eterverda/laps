//! Камера: источник кадров (capture.rs + бэкенды), pipeline (этот файл)
//! и общие утилиты (fps.rs). `Camera` — фасад для GUI: владеет
//! capture-потоком, slot'ом и текстурой.
//!
//! Правило слоя: бэкенды (`nokhwa.rs`, позже v4l2/mf/avf) импортируют
//! только контракты из capture.rs и не трогают pipeline-части.

use self::decode::Error as DecodeError;
pub mod capture;
pub mod decode;
pub mod fps;
// Linux: the native ioctl backend. macOS/Windows: nokhwa.
#[cfg(not(target_os = "linux"))]
mod nokhwa;
#[cfg(target_os = "linux")]
mod v4l2;

pub use capture::{
    Capture, CaptureFormat, CaptureSession, Frame, FrameData, FrameError, FrameSink,
};
#[cfg(not(target_os = "linux"))]
use nokhwa::NokhwaCapture as Backend;
#[cfg(target_os = "linux")]
use v4l2::V4l2Capture as Backend;

use crate::config::camera::{CameraConfig, PixelConfig};
use crate::driver::dvr::{DvrSink, RecordParams, RecordState, SharedRecordState};
use crossbeam_utils::atomic::AtomicCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Ошибки слоя источника (перечисление/открытие устройств). Варианты
/// различаются принадлежностью: системный сбой / диагноз нашего слоя /
/// ошибка бэкенда как есть.
#[derive(Debug)]
pub enum Error {
    /// Системный сбой (ioctl, открытие файла).
    Io(std::io::Error),
    /// Диагноз нашего слоя: по этому конфигу открыться нельзя —
    /// устройство не найдено по имени или отказало в переговорах
    /// формата. Текст различает причину.
    Config(String),
    /// Ошибка бэкенда, переданная как есть (без переклассификации).
    #[allow(dead_code)] // nokhwa-ветка (macOS/Windows) под Linux не компилируется
    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Config(msg) => write!(f, "{msg}"),
            Error::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Other(e) => Some(&**e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

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

type ImageSlot = Arc<Mutex<Option<Arc<Frame>>>>;

/// Команды записи из UI-потока в capture-поток.
enum RecordCommand {
    StartRecording(RecordParams),
    StopRecording,
}

/// Latest-wins sink (P3a): хранит ровно последний кадр, потребитель
/// забирает по своему темпу; медленный потребитель пропускает кадры, но
/// capture-поток его не ждёт — обратного давления нет.
struct LatestSink {
    slot: ImageSlot,
    on_frame: Arc<dyn Fn() + Send + Sync>,
}

impl FrameSink for LatestSink {
    fn on_frame(&mut self, frame: Arc<Frame>) {
        match &frame.data {
            FrameData::Rgba { .. } => {
                *self.slot.lock().unwrap() = Some(frame);
                (self.on_frame)();
            }
            other => log::error!("ui sink: unexpected frame variant {other:?}"),
        }
    }
}

pub fn list_cameras() -> std::result::Result<Vec<CameraConfig>, Error> {
    let mut devices = Backend::list_devices()?;
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

pub struct Camera {
    /// Кадр из capture-потока → UI-поток (latest-wins).
    slot: ImageSlot,
    /// Текстура 1×1 пустышка из open(); первый кадр расширит set'ом.
    /// TextureId стабилен всю жизнь Camera.
    texture: egui::TextureHandle,
    /// Состояние наружу: UI читает атомарно.
    capture_state: SharedCaptureState,
    record_state: SharedRecordState,
    /// Команды записи в capture-поток.
    commands: crossbeam_channel::Sender<RecordCommand>,
    /// Латентность live на выходе: пишет UI-поток, логает capture-поток
    /// в конце сеанса.
    ui_latency: Arc<Mutex<fps::FrameStats>>,
    /// Останов и join capture-потока (Drop).
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Camera {
    pub fn open(desc: CameraConfig, ctx: egui::Context) -> Self {
        // Пробуждение UI на каждый кадр. 1 мс вместо немедленного repaint: egui
        // рендерит дважды на request_repaint, второй проход находит пустой
        // слот; маленькая задержка даёт один проход на кадр.
        let on_frame = {
            let ctx = ctx.clone();
            move || ctx.request_repaint_after(std::time::Duration::from_millis(1))
        };
        let slot: ImageSlot = Arc::default();
        let running = Arc::new(AtomicBool::new(true));
        let capture_state: SharedCaptureState = Arc::new(AtomicCell::new(CaptureState::Starting));
        let record_state: SharedRecordState = Arc::new(AtomicCell::new(RecordState {
            ok: false,
            fps: 0.0,
        }));
        let (commands_tx, commands_rx) = crossbeam_channel::bounded(4);
        let ui_latency = Arc::new(Mutex::new(fps::FrameStats::default()));
        // 1×1-пустышка: id валиден с рождения, размер возьмёт первый кадр
        // (set перезаписывает мету). Контент не отрисовывается: до первого
        // кадра capture_state == Starting и экран показывает testcard.
        let texture = ctx.load_texture(
            "camera",
            egui::ColorImage::new([1, 1], vec![egui::Color32::TRANSPARENT]),
            egui::TextureOptions::NEAREST,
        );

        let slot_clone = Arc::clone(&slot);
        let capture_state_clone = Arc::clone(&capture_state);
        let record_state_clone = Arc::clone(&record_state);
        let running_clone = Arc::clone(&running);
        let ui_latency_thread = Arc::clone(&ui_latency);

        let thread = thread::spawn(move || {
            capture_loop(
                desc,
                slot_clone,
                capture_state_clone,
                record_state_clone,
                running_clone,
                commands_rx,
                ui_latency_thread,
                on_frame,
            );
        });

        Self {
            slot,
            texture,
            capture_state,
            record_state,
            commands: commands_tx,
            ui_latency,
            running,
            thread: Some(thread),
        }
    }

    /// Стрим открыт, поток камеры жив.
    pub fn capture_state(&self) -> CaptureState {
        self.capture_state.load()
    }

    /// Writer жив и пишет на диск.
    pub fn record_state(&self) -> RecordState {
        self.record_state.load()
    }

    /// Запустить запись. Команда применится перед следующим кадром;
    /// ошибка создания файла уйдёт в лог, RecordState останется false.
    pub fn start_recording(&self, params: RecordParams) {
        if self
            .commands
            .try_send(RecordCommand::StartRecording(params))
            .is_err()
        {
            log::warn!("dvr: start_recording ignored (capture thread gone)");
        }
    }

    /// Остановить запись; файл финализируется в фоне. Команда
    /// применится между кадрами, затем — drop recorder'а.
    pub fn stop_recording(&self) {
        if self
            .commands
            .try_send(RecordCommand::StopRecording)
            .is_err()
        {
            log::warn!("dvr: stop_recording ignored (capture thread gone)");
        }
    }

    /// Забрать новый кадр из слота, если есть, и обновить текстуру.
    /// true — из слота забран новый кадр.
    pub fn update_frame(&mut self) -> bool {
        let image = {
            let mut guard = self.slot.lock().unwrap();
            guard.take()
        };
        let fresh = image.is_some();

        if let Some(frame) = image {
            let FrameData::Rgba { rgba } = &frame.data else {
                unreachable!("ui slot holds only RGBA frames");
            };
            self.texture
                .set((*rgba).clone(), egui::TextureOptions::NEAREST);
            // Замер латентности на выходе: кадр реально отдан в текстуру.
            self.ui_latency.lock().unwrap().on_frame(&frame);
        }

        fresh
    }

    /// Текстура последнего кадра. Id стабилен с open(): 1×1-пустышка,
    /// первый кадр расширяет. Валидна, пока жива Camera.
    pub fn texture(&self) -> egui::TextureId {
        self.texture.id()
    }
}

/// Тело capture-потока: перебор устройств, открытие сессии, цикл
/// захвата. Живёт в отдельном потоке (io + ожидание кадров),
/// потому без self: хендлы получает по значению.
fn capture_loop(
    desc: CameraConfig,
    slot: ImageSlot,
    capture_state: SharedCaptureState,
    record_state: SharedRecordState,
    running: Arc<AtomicBool>,
    commands_rx: crossbeam_channel::Receiver<RecordCommand>,
    ui_latency: Arc<Mutex<fps::FrameStats>>,
    on_frame: impl Fn() + Send + Sync + 'static,
) {
    let (mut session, matched) = match open_session(&desc) {
        Ok(opened) => opened,
        Err(_already_logged) => {
            capture_state.store(CaptureState::Dead);
            return;
        }
    };
    let fmt = session.negotiated();
    log::info!("capture stream opened, format: {:?}", fmt);

    let width = fmt.resolution.width;
    let height = fmt.resolution.height;
    // The camera may open at a higher frame rate than requested
    // (e.g. 60 fps when 30 was asked for). Keep the negotiated
    // stream as-is and drop excess frames in the DVR sink so
    // recording runs at the requested rate; live view is not
    // throttled.
    let mut ui_sink = LatestSink {
        slot,
        on_frame: Arc::new(on_frame),
    };
    let mut dvr_sink = DvrSink::new(matched.frame_rate, fmt, record_state);

    let mut errors_since: Option<Instant> = None;
    let mut first_frame = true;
    let capture_t0 = Instant::now();
    let mut capture_frames = 0u32;
    let mut frame_interval = fps::FrameStats::default();
    let mut last_timestamp: Option<Instant> = None;
    let mut decode_jpeg = fps::FrameStats::default();
    let mut decode_yuyv = fps::FrameStats::default();

    loop {
        if !running.load(Ordering::Relaxed) {
            log::info!("capture thread stopping");
            break;
        }

        // Команды записи применяем между кадрами: dequeue
        // блокирует до ~периода кадра, задержка незаметна.
        for cmd in commands_rx.try_iter() {
            match cmd {
                RecordCommand::StartRecording(params) => dvr_sink.start(&params),
                RecordCommand::StopRecording => dvr_sink.stop(),
            }
        }

        let frame = match session.frame() {
            Ok(frame) => {
                errors_since = None;
                capture_frames += 1;
                if let Some(prev) = last_timestamp {
                    frame_interval.on_process(frame.timestamp - prev);
                }
                last_timestamp = Some(frame.timestamp);
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

        if !dispatch_frame(
            frame,
            width,
            height,
            &mut dvr_sink,
            &mut ui_sink,
            &mut decode_jpeg,
            &mut decode_yuyv,
        ) {
            break;
        }
        // Live — когда первый кадр реально пошёл, а не когда
        // открылось устройство: до первого кадра UI показывает
        // testcard (Starting), чёрного кадра нет по построению.
        if first_frame {
            first_frame = false;
            capture_state.store(CaptureState::Live);
        }
    }

    // Поток умирает (штатный стоп или потеря камеры) — гасим CAM,
    // иначе индикатор висит белым на мёртвой картинке. REC гаснет
    // сам: writer-поток — единственный владелец RecordState.
    capture_state.store(CaptureState::Dead);

    // Recorder дропается здесь (через on_stop sink'а): файл
    // финализируется в фоновом writer-потоке. Session дропается
    // здесь: Drop бэкенда делает stop_stream.
    dvr_sink.on_stop();
    ui_sink.on_stop();
    drop(dvr_sink);
    drop(ui_sink);

    log_session_stats(
        &ui_latency,
        capture_t0,
        capture_frames,
        &mut frame_interval,
        &mut decode_jpeg,
        &mut decode_yuyv,
    );
}

/// Открыть сессию или объяснить почему не вышло (перечисление
/// устройств, поиск формата, открытие потока). Контекст ошибки логает
/// здесь, на месте; решение «что делать» — за вызывающим.
fn open_session(desc: &CameraConfig) -> Result<(impl CaptureSession, CaptureFormat), Error> {
    // Перебор устройств и форматов — io, не должно висеть на UI-потоке.
    let devices = Backend::list_devices().map_err(|e| {
        log::error!("failed to list devices: {e}");
        e
    })?;

    let (name, matched) = devices
        .into_iter()
        .find_map(|dev| {
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
        })
        .ok_or_else(|| {
            let e = Error::Config(format!("no matching camera format for {desc}"));
            log::error!("{e}");
            e
        })?;

    log::info!("starting capture from {} at {:?}", name, matched);

    // open резолвит устройство по имени заново (P5): между
    // перечислением и открытием камеру могли переткнуть.
    let session = Backend::open(desc).map_err(|e| {
        log::error!("failed to open camera: {e}");
        e
    })?;
    Ok((session, matched))
}

/// Отчёт об остановленном сеансе: частота захвата, интервалы между
/// кадрами, латентность «захват → экран», длительности декода.
#[allow(clippy::too_many_arguments)]
fn log_session_stats(
    ui_latency: &Arc<Mutex<fps::FrameStats>>,
    capture_t0: Instant,
    capture_frames: u32,
    frame_interval: &mut fps::FrameStats,
    decode_jpeg: &mut fps::FrameStats,
    decode_yuyv: &mut fps::FrameStats,
) {
    let elapsed = capture_t0.elapsed();
    log::info!(
        "live: camera frames received: {} in {:?} -> {:.1} fps",
        capture_frames,
        elapsed,
        capture_frames as f32 / elapsed.as_secs_f32().max(0.001)
    );
    if let Some(s) = frame_interval.snapshot() {
        log::info!("live: camera frame interval: {s}");
    }
    if let Some(s) = ui_latency.lock().unwrap().snapshot() {
        log::info!("live: frame-to-display latency: {s}");
    }
    if let Some(s) = decode_jpeg.snapshot() {
        log::info!("live: jpeg→rgba decode duration: {s}");
    }
    if let Some(s) = decode_yuyv.snapshot() {
        log::info!("live: yuyv→rgba decode duration: {s}");
    }
}


/// Конверсия + fan-out по подпискам sink'ов (P3/P4). Единственный декод
/// на кадр: Jpeg → RGBA для UI, Yuyv → RGBA для обоих sink'ов. Вынесено
/// из тела цикла для тестируемости без камеры.
/// false — декод потребовал остановки потока (Unrecoverable).
#[allow(clippy::too_many_arguments)]
fn dispatch_frame(
    frame: Frame,
    width: u32,
    height: u32,
    dvr_sink: &mut DvrSink,
    ui_sink: &mut LatestSink,
    decode_jpeg: &mut fps::FrameStats,
    decode_yuyv: &mut fps::FrameStats,
) -> bool {
    let ts = frame.timestamp;
    match frame.data {
        FrameData::Jpeg { buf, len } => {
            // Декод до перемещения буфера: borrow, не копия. Пишем в DVR
            // до применения результата декода: jpeg-кадр валиден сам по
            // себе (SOI..EOI), а декод может отказать на кадре, который
            // плеер съел бы — экранный дроп не должен терять кадр в записи.
            let t0 = Instant::now();
            let decoded = decode::decode_frame(PixelConfig::Mjpeg, &buf, width, height);
            decode_jpeg.on_process(t0.elapsed());
            dvr_sink.on_frame(Arc::new(Frame {
                timestamp: ts,
                data: FrameData::Jpeg { buf, len },
            }));
            match decoded {
                Ok(image) => {
                    ui_sink.on_frame(Arc::new(Frame {
                        timestamp: ts,
                        data: FrameData::Rgba { rgba: image },
                    }));
                }
                Err(DecodeError::Recoverable(e)) => {
                    log::warn!("frame skipped: {}", e);
                }
                Err(DecodeError::Unrecoverable(e)) => {
                    log::error!("capture stopping: {}", e);
                    return false;
                }
            }
        }
        FrameData::Yuyv { buf } => {
            let t0 = Instant::now();
            let decoded = decode::decode_frame(PixelConfig::Yuyv, &buf, width, height);
            decode_yuyv.on_process(t0.elapsed());
            match decoded {
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
                    return false;
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
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::camera::{FrameRateConfig, ResolutionConfig};

    fn yuyv_frame(w: u32, h: u32) -> Frame {
        Frame {
            timestamp: Instant::now(),
            data: FrameData::Yuyv {
                buf: vec![128u8; (w * h * 2) as usize],
            },
        }
    }

    /// Pipeline без камеры: Yuyv-кадр конвертируется один раз и попадает
    /// в оба sink'а; jpeg-кадр идёт в DVR как есть и декодируется в UI.
    /// DVR на fps>60-конфиге (троттлинг выключен): записанные кадры
    /// считаем по появлению файла.
    #[test]
    fn dispatch_routes_frames_to_sinks() {
        let slot: ImageSlot = Arc::default();
        let mut ui = LatestSink {
            slot: Arc::clone(&slot),
            on_frame: Arc::new(|| {}),
        };
        let state: SharedRecordState = Arc::new(AtomicCell::new(RecordState {
            ok: false,
            fps: 0.0,
        }));
        let dir = std::env::temp_dir().join(format!("laps-pipe-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let negotiated = CaptureFormat {
            resolution: ResolutionConfig {
                width: 8,
                height: 8,
            },
            frame_rate: FrameRateConfig(60),
            format: PixelConfig::Yuyv,
        };
        let mut dvr = DvrSink::new(FrameRateConfig(60), negotiated, Arc::clone(&state));
        dvr.start(&RecordParams {
            dir: dir.clone(),
            camera_id: "camera-1".to_owned(),
            camera: CameraConfig::new("Test", 8, 8, 60, PixelConfig::Yuyv),
        });
        assert!(state.load().ok, "recorder must be live");

        let mut dj = fps::FrameStats::default();
        let mut dy = fps::FrameStats::default();
        assert!(dispatch_frame(
            yuyv_frame(8, 8),
            8,
            8,
            &mut dvr,
            &mut ui,
            &mut dj,
            &mut dy
        ));
        assert!(
            matches!(
                &slot.lock().unwrap().as_ref().map(|f| &f.data),
                Some(FrameData::Rgba { .. })
            ),
            "ui sink must hold decoded RGBA"
        );

        assert!(dispatch_frame(
            yuyv_frame(8, 8),
            8,
            8,
            &mut dvr,
            &mut ui,
            &mut dj,
            &mut dy
        ));
        assert!(
            matches!(
                &slot.lock().unwrap().as_ref().map(|f| &f.data),
                Some(FrameData::Rgba { .. })
            ),
            "ui sink must hold decoded RGBA from jpeg"
        );

        // latest-wins: следующий кадр заменяет предыдущий.
        let first = Arc::clone(slot.lock().unwrap().as_ref().unwrap());
        assert!(dispatch_frame(
            yuyv_frame(8, 8),
            8,
            8,
            &mut dvr,
            &mut ui,
            &mut dj,
            &mut dy
        ));
        let second = Arc::clone(slot.lock().unwrap().as_ref().unwrap());
        assert!(
            !Arc::ptr_eq(&first, &second),
            "slot must replace, not queue"
        );

        drop(dvr); // канал закрыт → writer finalize'ит файл в фоне
        let deadline = Instant::now() + Duration::from_secs(5);
        let mkv = loop {
            let found = std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .any(|p| p.extension().is_some_and(|e| e == "mkv"));
            if found || Instant::now() > deadline {
                break found;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(mkv, "dvr sink must write the container");
        std::fs::remove_dir_all(&dir).ok();
    }
}

impl Drop for Camera {
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
