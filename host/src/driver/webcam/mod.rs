use self::decode::Error as DecodeError;
pub mod decode;
pub mod fps;

use crate::config::camera::{CameraConfig, PixelConfig};
use crate::driver::dvr::{RecordState, SharedRecordState};
use crossbeam_utils::atomic::AtomicCell;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{CameraFormat, CameraInfo, RequestedFormat, RequestedFormatType};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

/// Состояние камеры (capture-потока).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CameraState {
    /// Поток жив, идёт инициализация (перебор устройств, открытие стрима).
    Starting,
    /// Стрим открыт, кадры идут.
    Live,
    /// Поток мёртв: камера не найдена, открытие не удалось или отвал.
    Dead,
}

/// Общий для потока и UI хэндл состояния камеры.
pub type SharedCameraState = Arc<AtomicCell<CameraState>>;
use std::time::{Duration, Instant};

/// nokhwa на V4L2 отдаёт `frame_raw()` — это весь mmap-буфер (его ёмкость,
/// равная размеру несжатого кадра), а не реальную длину кадра: драйвер
/// пишет jpeg в начало, а после EOI остаётся неиспользуемый хвост со
/// старыми данными. MJPEG-кадр — это ровно SOI..EOI; без обрезки в файл
/// попадает мусор, а jpegparse в GStreamer теряет синхронизацию.
/// Поиск EOI — через SIMD-поиск паттерна FFD9 (memmem): в entropy-данных
/// FF заэскейпен как FF00, поэтому первое вхождение FFD9 после SOI — EOI.
fn trim_mjpeg(frame: &[u8]) -> Option<&[u8]> {
    let start = if frame.starts_with(&[0xFF, 0xD8]) {
        0
    } else {
        frame.windows(2).position(|w| w == [0xFF, 0xD8])?
    };
    let end = memchr::memmem::find(&frame[start..], b"\xff\xd9")? + start + 2;
    Some(&frame[start..end])
}

/// HARD HACK / ДИСКЛЕЙМЕР. macOS (AVFoundation) часто открывает камеру на
/// 60 fps, даже когда в настройках запрошено 30 fps. Вместо форка биндингов
/// nokhwa мы дропаем кадры прямо в capture-потоке, но только на пути в DVR
/// и только при целевом fps == 30. Live view продолжает обновляться на
/// полном fps камеры. Интервал 25 мс (а не 33.3 мс) выбран так, чтобы при
/// ровном 60 fps источника записывался примерно каждый второй кадр, то есть
/// ~30 fps, не боясь пограничного джиттера.
const RECORD_THROTTLE_30FPS: Duration = Duration::from_millis(25);

/// Возвращает true, если кадр нужно отправить в DVR. Для целевого 30 fps
/// дропаем кадры, пришедшие быстрее 25 мс после последнего записанного;
/// для остальных fps каждый кадр проходит.
fn should_record_frame(fps: u32, last_recorded_frame: &mut Instant) -> bool {
    if fps != 30 {
        return true;
    }
    if last_recorded_frame.elapsed() >= RECORD_THROTTLE_30FPS {
        *last_recorded_frame = Instant::now();
        true
    } else {
        false
    }
}

type ImageSlot = Arc<Mutex<Option<egui::ColorImage>>>;

/// Команды из UI-потока в capture-поток (управление записью).
enum Command {
    StartRecording(crate::driver::dvr::Options),
    StopRecording,
}

/// Recorder живёт внутри capture-потока, но создаётся по команде, а не
/// при старте захвата: так CAM и REC можно включать независимо.
fn start_recorder(
    options: &crate::driver::dvr::Options,
    negotiated: &nokhwa::utils::CameraFormat,
    state: &SharedRecordState,
) -> Option<crate::driver::dvr::Recorder> {
    // Формат и контейнер ветвятся внутри Recorder::start.
    let result = crate::driver::dvr::Recorder::start(options, negotiated, state);
    match result {
        Ok(recorder) => Some(recorder),
        Err(e) => {
            log::error!("dvr: recording unavailable: {}", e);
            None
        }
    }
}

fn sort_and_dedup(formats: &mut Vec<CameraFormat>) {
    formats.sort_by(|a, b| {
        a.resolution()
            .width()
            .cmp(&b.resolution().width())
            .then_with(|| a.resolution().height().cmp(&b.resolution().height()))
            .then_with(|| a.frame_rate().cmp(&b.frame_rate()))
    });
    formats.dedup_by(|a, b| {
        a.resolution().width() == b.resolution().width()
            && a.resolution().height() == b.resolution().height()
            && a.frame_rate() == b.frame_rate()
            && a.format() == b.format()
    });
}

#[cfg(target_os = "macos")]
fn list_formats_for_cam(cam: &CameraInfo) -> Result<Vec<CameraFormat>, String> {
    use nokhwa_bindings_macos::AVCaptureDevice;
    let index = cam.index().clone();
    let device = AVCaptureDevice::new(&index).map_err(|e| e.to_string())?;
    let mut formats = device.supported_formats().map_err(|e| e.to_string())?;
    sort_and_dedup(&mut formats);
    Ok(formats)
}

#[cfg(not(target_os = "macos"))]
fn list_formats_for_cam(cam: &CameraInfo) -> Result<Vec<CameraFormat>, String> {
    let index = cam.index().clone();
    let format = RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestResolution);
    let mut camera = nokhwa::Camera::new(index, format).map_err(|e| e.to_string())?;
    let mut formats = camera
        .compatible_camera_formats()
        .map_err(|e| e.to_string())?;
    sort_and_dedup(&mut formats);
    Ok(formats)
}

pub fn list_cameras() -> Result<Vec<CameraConfig>, String> {
    let mut cameras = nokhwa::query(nokhwa::utils::ApiBackend::Auto).map_err(|e| e.to_string())?;
    cameras.sort_by(|a, b| a.human_name().cmp(&b.human_name()));
    let mut descriptions = Vec::new();
    for cam in cameras {
        let formats = list_formats_for_cam(&cam).unwrap_or_default();
        for fmt in formats {
            // Other formats (GRAY, RGB, ...) are not offered in descriptions.
            let pixel_format = match fmt.format() {
                nokhwa::utils::FrameFormat::YUYV => PixelConfig::Yuyv,
                nokhwa::utils::FrameFormat::MJPEG => PixelConfig::Mjpeg,
                _ => continue,
            };
            descriptions.push(CameraConfig::new(
                &cam.human_name(),
                fmt.resolution().width(),
                fmt.resolution().height(),
                fmt.frame_rate(),
                pixel_format,
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
    camera: SharedCameraState,
    record: SharedRecordState,
    commands: crossbeam_channel::Sender<Command>,
}

impl Webcam {
    /// Стрим открыт, поток камеры жив.
    pub fn camera_state(&self) -> CameraState {
        self.camera.load()
    }

    /// Writer жив и пишет на диск.
    pub fn record_state(&self) -> RecordState {
        self.record.load()
    }

    /// Запустить запись. Команда применится перед следующим кадром;
    /// ошибка создания файла уйдёт в лог, RecordState останется false.
    pub fn start_recording(&self, options: crate::driver::dvr::Options) {
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
        let state: SharedCameraState = Arc::new(AtomicCell::new(CameraState::Starting));
        let state_clone = Arc::clone(&state);
        let record: SharedRecordState = Arc::new(AtomicCell::new(RecordState {
            ok: false,
            fps: 0.0,
        }));
        let record_clone = Arc::clone(&record);
        let (commands_tx, commands_rx) = crossbeam_channel::bounded(4);

        let thread = thread::spawn(move || {
            // Перебор устройств и форматов — io, не должно висеть на UI-потоке.
            let cameras = match nokhwa::query(nokhwa::utils::ApiBackend::Auto) {
                Ok(c) => c,
                Err(e) => {
                    log::error!("failed to query cameras: {}", e);
                    return;
                }
            };

            let (info, matched_fmt) = match cameras.into_iter().find_map(|cam| {
                let fmt = match list_formats_for_cam(&cam) {
                    Ok(f) => f,
                    Err(_) => return None,
                };
                fmt.into_iter()
                    .find(|f| {
                        desc.matches(
                            &cam.human_name(),
                            f.resolution().width(),
                            f.resolution().height(),
                            f.frame_rate(),
                            &f.format().to_string(),
                        )
                    })
                    .map(|f| (cam, f))
            }) {
                Some(found) => found,
                None => {
                    log::error!("no matching camera format for {}", desc);
                    return;
                }
            };

            log::info!(
                "starting capture from [{}] {} at {:?}",
                info.index(),
                info.human_name(),
                matched_fmt
            );

            let format = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(
                nokhwa::utils::CameraFormat::new_from(
                    matched_fmt.resolution().width(),
                    matched_fmt.resolution().height(),
                    matched_fmt.format(),
                    matched_fmt.frame_rate(),
                ),
            ));
            let mut camera = match nokhwa::Camera::new(info.index().clone(), format) {
                Ok(cam) => cam,
                Err(e) => {
                    log::error!("failed to open camera: {}", e);
                    return;
                }
            };

            if let Err(e) = camera.open_stream() {
                log::error!("failed to open stream: {}", e);
                return;
            }
            state_clone.store(CameraState::Live);

            let fmt = camera.camera_format();
            log::info!("capture stream opened, format: {:?}", fmt);

            let width = fmt.resolution().width();
            let height = fmt.resolution().height();
            let frame_format = fmt.format();
            // The camera may open at a higher frame rate than requested (e.g. 60 fps
            // when 30 was asked for). Keep the negotiated stream as-is and drop excess
            // frames below so recording/display run at the requested rate.
            let fps = matched_fmt.frame_rate();

            // Recorder появляется и исчезает по командам из UI.
            let mut recorder: Option<crate::driver::dvr::Recorder> = None;

            // Для определения ошибки захвата используем не счётчик,
            // потому что frame() может виснуть на несколько секунд,
            // поэтому критерий — время без единого кадра.
            let mut errors_since: Option<std::time::Instant> = None;
            let mut last_recorded_frame = Instant::now();

            loop {
                if !running_clone.load(Ordering::Relaxed) {
                    log::info!("capture thread stopping");
                    break;
                }

                // Команды записи применяем между кадрами: dequeue
                // блокирует до ~периода кадра, задержка незаметна.
                for cmd in commands_rx.try_iter() {
                    match cmd {
                        Command::StartRecording(options) => {
                            recorder = start_recorder(&options, &fmt, &record_clone);
                        }
                        Command::StopRecording => {
                            recorder = None; // drop: writer finalize'ит в фоне
                        }
                    }
                }

                let raw = match camera.frame_raw() {
                    Ok(r) => {
                        errors_since = None;
                        r
                    }
                    Err(e) => {
                        let since = errors_since.get_or_insert_with(std::time::Instant::now);
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
                };

                // MJPEG пишем до декода: кадр валиден сам по себе (SOI..EOI),
                // а декод может отказать на кадре, который ffmpeg/GStreamer
                // съели бы — экранный дроп не должен терять кадр в DVR.
                // При целевом 30 fps дропаем кадры, пришедшие быстрее 25 мс
                // (см. HARD HACK выше), чтобы не писать 60 fps с камеры.
                if frame_format == nokhwa::utils::FrameFormat::MJPEG {
                    if let Some(recorder) = &recorder {
                        if should_record_frame(fps, &mut last_recorded_frame) {
                            match trim_mjpeg(&raw) {
                                Some(frame) => {
                                    recorder.push_frame(
                                        frame.to_vec(),
                                        crate::driver::dvr::epoch_millis(),
                                    );
                                }
                                None => log::warn!("dvr: frame without EOI, skipped"),
                            }
                        }
                    }
                }

                let image = match decode::decode_frame(frame_format, raw.as_ref(), width, height) {
                    Ok(image) => image,
                    Err(DecodeError::Recoverable(e)) => {
                        log::warn!("frame skipped: {}", e);
                        continue;
                    }
                    Err(DecodeError::Unrecoverable(e)) => {
                        log::error!("capture stopping: {}", e);
                        break;
                    }
                };

                // YUYV в DVR уходит декодированным RGBA. При целевом 30 fps
                // дропаем кадры, пришедшие быстрее 25 мс, чтобы не писать 60 fps
                // с камеры (см. HARD HACK выше). Live view всё равно обновляется.
                if frame_format != nokhwa::utils::FrameFormat::MJPEG {
                    if let Some(recorder) = &recorder {
                        if should_record_frame(fps, &mut last_recorded_frame) {
                            let rgba: &[u8] = bytemuck::cast_slice(&image.pixels);
                            recorder.push_frame(rgba.to_vec(), crate::driver::dvr::epoch_millis());
                        }
                    }
                }

                *slot_clone.lock().unwrap() = Some(image);
                on_frame();
            }

            // Поток умирает (штатный стоп или потеря камеры) — гасим CAM,
            // иначе индикатор висит белым на мёртвой картинке. REC гаснет
            // сам: writer-поток — единственный владелец RecordState.
            state_clone.store(CameraState::Dead);

            if let Err(e) = camera.stop_stream() {
                log::error!("failed to stop stream: {}", e);
            }
            // Recorder дропается здесь: файл финализируется в фоновом
            // writer-потоке (Recorder::drop его не джойнит).
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

        if let Some(image) = image {
            match &mut self.texture {
                Some(texture) => {
                    texture.set(image, egui::TextureOptions::NEAREST);
                }
                None => {
                    self.texture =
                        Some(ctx.load_texture("camera", image, egui::TextureOptions::NEAREST));
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
