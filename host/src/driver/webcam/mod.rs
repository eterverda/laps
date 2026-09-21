use self::decode::Error as DecodeError;
pub mod decode;

use crate::config::camera::{Camera, PixelFormat};
use crate::driver::{CameraState, RecordState, SharedCameraState, SharedRecordState};
use crossbeam_utils::atomic::AtomicCell;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{CameraFormat, CameraInfo, RequestedFormat, RequestedFormatType};
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// nokhwa на V4L2 отдаёт `frame_raw()` — это весь mmap-буфер (его ёмкость,
/// равная размеру несжатого кадра), а не реальную длину кадра: драйвер
/// пишет jpeg в начало, а после EOI остаётся неиспользуемый хвост со
/// старыми данными. MJPEG-кадр — это ровно SOI..EOI; без обрезки в AVI
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
    width: u32,
    height: u32,
    fps: u32,
    frame_format: nokhwa::utils::FrameFormat,
    state: &SharedRecordState,
) -> Option<crate::driver::dvr::Recorder> {
    if frame_format != nokhwa::utils::FrameFormat::MJPEG {
        // Запись — пасsthrough MJPEG; YUYV требовал бы JPEG-кодирования.
        log::warn!("dvr: {} is not MJPEG, recording disabled", frame_format);
        return None;
    }
    match crate::driver::dvr::Recorder::start(options, width, height, fps, state) {
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

pub fn list_cameras() -> Result<Vec<Camera>, String> {
    let mut cameras = nokhwa::query(nokhwa::utils::ApiBackend::Auto).map_err(|e| e.to_string())?;
    cameras.sort_by(|a, b| a.human_name().cmp(&b.human_name()));
    let mut descriptions = Vec::new();
    for cam in cameras {
        let formats = list_formats_for_cam(&cam).unwrap_or_default();
        for fmt in formats {
            // Other formats (GRAY, RGB, ...) are not offered in descriptions.
            let pixel_format = match fmt.format() {
                nokhwa::utils::FrameFormat::YUYV => PixelFormat::Yuyv,
                nokhwa::utils::FrameFormat::MJPEG => PixelFormat::Mjpeg,
                _ => continue,
            };
            descriptions.push(Camera::new(
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
    /// Сайдкар показанных кадров (формат как у rec). None — файл не
    /// создался, показ продолжается без лога.
    frames_file: Option<std::io::BufWriter<std::fs::File>>,
    // ts предыдущего показанного кадра (старт — момент запроса на cam).
    frames_prev_ts: u64,
    frames_next: u64,
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

    pub fn start(
        camera_id: &str,
        desc: Camera,
        on_frame: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        // Момент запроса на cam: первый кадр сайдкара считаем от него.
        let start_request_ms = crate::driver::dvr::epoch_millis();
        let (frames_file, frames_prev_ts) = match Self::create_frames_log(camera_id, &desc) {
            Ok(file) => (Some(file), start_request_ms),
            Err(e) => {
                log::error!("cam frames log unavailable: {}", e);
                (None, start_request_ms)
            }
        };
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
            let fps = fmt.frame_rate();

            // Recorder появляется и исчезает по командам из UI.
            let mut recorder: Option<crate::driver::dvr::Recorder> = None;

            // Счётчик пишущихся кадров: fps показываем в UI (окно ~0.5 с).
            let mut rec_frames = 0u32;
            let mut rec_window = std::time::Instant::now();

            // Счётчик непрерывных ошибок захвата: nokhwa не отличает
            // транзиентный сбой от отвала устройства (всё — ReadFrameError),
            // поэтому критерий — время без единого кадра.
            let mut errors_since: Option<std::time::Instant> = None;

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
                            recorder = start_recorder(
                                &options,
                                width,
                                height,
                                fps,
                                frame_format,
                                &record_clone,
                            );
                            // Окно замера сбрасываем: fps считаем от старта
                            // записи, иначе первое окно тянет elapsed со
                            // старта потока и даёт мгновенный "0 fps".
                            rec_frames = 0;
                            rec_window = std::time::Instant::now();
                        }
                        Command::StopRecording => {
                            recorder = None; // drop: writer finalize'ит в фоне
                            record_clone.store(RecordState {
                                ok: false,
                                fps: 0.0,
                            });
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

                // Запись идёт до декода: кадр валиден сам по себе (SOI..EOI),
                // а декод может отказать на кадре, который ffmpeg/GStreamer
                // съели бы — экранный дроп не должен терять кадр в DVR.
                if let Some(recorder) = &recorder {
                    match trim_mjpeg(&raw) {
                        Some(frame) => {
                            recorder.push_frame(frame.to_vec(), crate::driver::dvr::epoch_millis());
                            rec_frames += 1;
                            if rec_window.elapsed() >= Duration::from_millis(500) {
                                // Пустое окно (старт, пауза кадров) не
                                // затирает последнее известное значение.
                                if rec_frames > 0 {
                                    record_clone.store(RecordState {
                                        ok: true,
                                        fps: rec_frames as f32 / rec_window.elapsed().as_secs_f32(),
                                    });
                                }
                                rec_frames = 0;
                                rec_window = std::time::Instant::now();
                            }
                        }
                        None => log::warn!("dvr: frame without EOI, skipped"),
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

                *slot_clone.lock().unwrap() = Some(image);
                on_frame();
            }

            // Поток умирает (штатный стоп или потеря камеры) — гасим
            // индикаторы, иначе CAM/REC висят белым/красным на мёртвой
            // картинке.
            state_clone.store(CameraState::Dead);
            record_clone.store(RecordState {
                ok: false,
                fps: 0.0,
            });

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
            frames_file,
            frames_prev_ts,
            frames_next: 0,
        }
    }

    /// Создать сайдкар показанных кадров, формат как у rec: каждый кадр
    /// документом `{i, ms}` (время с предыдущего показанного).
    fn create_frames_log(
        camera_id: &str,
        camera: &Camera,
    ) -> std::io::Result<std::io::BufWriter<std::fs::File>> {
        let dir = std::path::Path::new(crate::driver::dvr::CAPTURES_DIR);
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!(
            "{}-cam-{}-frames.yaml",
            crate::driver::dvr::timestamp_prefix(),
            camera_id
        ));
        log::info!("cam frames log: {:?}", path);
        let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
        // Шапка — для читающего файл глазами: спека камеры, семантика полей.
        writeln!(file, "# camera: {}", camera)?;
        writeln!(
            file,
            "# i — порядковый номер кадра, ms — время с предыдущего кадра в мс (у i=0 — от запроса на cam)"
        )?;
        Ok(file)
    }

    /// Записать интервал показа. Вызывается из UI-потока ровно на
    /// показанных кадрах (той же выборкой считается shown fps).
    fn log_shown_frame(&mut self, ts: u64) {
        let Some(file) = &mut self.frames_file else {
            return;
        };
        // ms — время с предыдущего показанного кадра (у первого — с
        // запроса на cam).
        if let Err(e) = writeln!(
            file,
            "--- {{i: {}, ms: {}}}",
            self.frames_next,
            ts.saturating_sub(self.frames_prev_ts)
        ) {
            log::error!("cam frames log failed: {}", e);
            self.frames_file = None;
            return;
        }
        self.frames_prev_ts = ts;
        self.frames_next += 1;
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
        if new_frame {
            self.log_shown_frame(crate::driver::dvr::epoch_millis());
        }

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
        // Финализация AVI не входит в это ожидание — recorder уже
        // отпущен детачем и дописывает файл в фоне.
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                log::error!("capture thread panicked");
            }
        }
        // Финализируем сайдкар cam: все кадры задокументированы,
        // закрываем поток YAML-документов.
        if let Some(file) = &mut self.frames_file {
            let _ = writeln!(file, "...");
            if let Err(e) = file.flush() {
                log::error!("cam frames log flush failed: {}", e);
            }
        }
    }
}
