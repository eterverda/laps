use self::decode::Error as DecodeError;
pub mod decode;

use crate::config::camera::{Camera, PixelFormat};
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{CameraFormat, CameraInfo, RequestedFormat, RequestedFormatType};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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
    _thread: thread::JoinHandle<()>,
}

impl Webcam {
    pub fn start(desc: &Camera, on_frame: impl Fn() + Send + Sync + 'static) -> Option<Self> {
        let cameras = match nokhwa::query(nokhwa::utils::ApiBackend::Auto) {
            Ok(c) => c,
            Err(e) => {
                log::error!("failed to query cameras: {}", e);
                return None;
            }
        };

        let (info, matched_fmt) = cameras.into_iter().find_map(|cam| {
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
        })?;

        let index = info.index().clone();
        let name = info.human_name();
        log::info!(
            "starting capture from [{}] {} at {:?}",
            index,
            name,
            matched_fmt
        );

        let slot: ImageSlot = Arc::default();
        let slot_clone = Arc::clone(&slot);
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = Arc::clone(&running);

        let thread = thread::spawn(move || {
            let format = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(
                nokhwa::utils::CameraFormat::new_from(
                    matched_fmt.resolution().width(),
                    matched_fmt.resolution().height(),
                    matched_fmt.format(),
                    matched_fmt.frame_rate(),
                ),
            ));
            let mut camera = match nokhwa::Camera::new(index, format) {
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

            let fmt = camera.camera_format();
            log::info!("capture stream opened, format: {:?}", fmt);

            let width = fmt.resolution().width();
            let height = fmt.resolution().height();
            let frame_format = fmt.format();

            loop {
                if !running_clone.load(Ordering::Relaxed) {
                    log::info!("capture thread stopping");
                    break;
                }

                let raw = match camera.frame_raw() {
                    Ok(r) => r,
                    Err(e) => {
                        log::error!("frame capture error: {}", e);
                        thread::sleep(Duration::from_millis(16));
                        continue;
                    }
                };

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

            if let Err(e) = camera.stop_stream() {
                log::error!("failed to stop stream: {}", e);
            }
        });

        Some(Self {
            slot,
            texture: None,
            running,
            _thread: thread,
        })
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
    }
}
