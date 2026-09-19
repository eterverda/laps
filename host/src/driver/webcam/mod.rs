use self::decode::Error as DecodeError;
pub mod decode;

use crate::config::camera::{Camera, PixelFormat};
use crate::driver::{CaptureState, RecordState, SharedCaptureState, SharedRecordState};
use crossbeam_utils::atomic::AtomicCell;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{CameraFormat, CameraInfo, RequestedFormat, RequestedFormatType};
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
    capture: SharedCaptureState,
    record: SharedRecordState,
}

impl Webcam {
    /// Стрим открыт и capture-поток жив.
    pub fn capture_state(&self) -> CaptureState {
        self.capture.load()
    }

    /// Writer жив и пишет на диск.
    pub fn record_state(&self) -> RecordState {
        self.record.load()
    }

    pub fn start(
        desc: Camera,
        dvr_options: crate::driver::dvr::Options,
        on_frame: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let slot: ImageSlot = Arc::default();
        let slot_clone = Arc::clone(&slot);
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = Arc::clone(&running);
        let capture: SharedCaptureState = Arc::new(AtomicCell::new(CaptureState { ok: false }));
        let capture_clone = Arc::clone(&capture);
        let record: SharedRecordState = Arc::new(AtomicCell::new(RecordState { ok: false }));
        let record_clone = Arc::clone(&record);

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
            capture_clone.store(CaptureState { ok: true });

            let fmt = camera.camera_format();
            log::info!("capture stream opened, format: {:?}", fmt);

            let width = fmt.resolution().width();
            let height = fmt.resolution().height();
            let frame_format = fmt.format();

            // DVR записывает сырой MJPEG-поток (до декода) в AVI. YUYV
            // требовал бы JPEG-кодирования — пока не поддерживается.
            let recorder = match frame_format {
                nokhwa::utils::FrameFormat::MJPEG => {
                    match crate::driver::dvr::Recorder::start(
                        &dvr_options,
                        width,
                        height,
                        fmt.frame_rate(),
                        &record_clone,
                    ) {
                        Ok(recorder) => Some(recorder),
                        Err(e) => {
                            log::error!("dvr: recording unavailable: {}", e);
                            None
                        }
                    }
                }
                other => {
                    log::warn!("dvr: {} is not MJPEG, recording disabled", other);
                    None
                }
            };

            // Счётчик непрерывных ошибок захвата: nokhwa не отличает
            // транзиентный сбой от отвала устройства (всё — ReadFrameError),
            // поэтому критерий — время без единого кадра.
            let mut errors_since: Option<std::time::Instant> = None;

            loop {
                if !running_clone.load(Ordering::Relaxed) {
                    log::info!("capture thread stopping");
                    break;
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
                            recorder.push_frame(frame.to_vec(), std::time::Instant::now())
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
            // индикаторы, иначе LIVE/REC висят белым/красным на мёртвой
            // картинке.
            capture_clone.store(CaptureState { ok: false });
            record_clone.store(RecordState { ok: false });

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
            capture,
            record,
        }
    }

    pub fn update(&mut self, ctx: &egui::Context) -> Option<&egui::TextureHandle> {
        let image = {
            let mut guard = self.slot.lock().unwrap();
            guard.take()
        };

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

        self.texture.as_ref()
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
    }
}
