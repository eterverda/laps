//! Реализация capture-трейта поверх nokhwa (все платформы).
//!
//! nokhwa заархивирована и является переходным бэкендом: интерфейс
//! (`super::capture`) от неё не зависит, замена — отдельным модулем
//! (docs/capture-backend-plan.md). macOS-специфичное перечисление
//! форматов через AVCaptureDevice живёт здесь же, за cfg.
//!
//! Правило слоя: бэкенд не импортирует pipeline-части модуля camera
//! (mod.rs) — только контракты из capture.rs.

use super::Error;
use super::capture::{
    Capture, CaptureFormat, CaptureSession, DeviceInfo, Frame, FrameData, FrameError,
};
use crate::config::camera::CameraConfig;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType};
use std::time::Instant;

/// Map nokhwa's error onto CaptureError: open/stream failures are io,
/// property (negotiation) rejections are Unsupported; everything else
/// is passed through unaltered under Backend.
fn capture_err(e: nokhwa::NokhwaError) -> Error {
    use nokhwa::NokhwaError as E;
    match e {
        E::OpenDeviceError(_, msg) | E::OpenStreamError(msg) => {
            Error::Io(std::io::Error::other(msg))
        }
        E::GetPropertyError { error, .. } | E::SetPropertyError { error, .. } => {
            Error::Config(format!("unsupported: {error}"))
        }
        other => Error::Other(Box::new(other)),
    }
}

pub struct NokhwaCapture;

pub struct NokhwaSession {
    camera: nokhwa::Camera,
    negotiated: CaptureFormat,
}

impl Capture for NokhwaCapture {
    type Session = NokhwaSession;

    fn list_devices() -> std::result::Result<Vec<DeviceInfo>, Error> {
        let cameras = match nokhwa::query(nokhwa::utils::ApiBackend::Auto) {
            Ok(cameras) => cameras,
            Err(e) => return Err(capture_err(e)),
        };
        let mut devices = Vec::new();
        for cam in cameras {
            // Устройство с отвалившимся перечислением форматов пропускаем
            // целиком — открыть его всё равно не выйдет.
            let Ok(formats) = list_formats_for_index(cam.index()) else {
                continue;
            };
            devices.push(DeviceInfo {
                name: cam.human_name(),
                formats: formats.iter().filter_map(to_capture_format).collect(),
            });
        }
        Ok(devices)
    }

    fn open(config: &CameraConfig) -> std::result::Result<Self::Session, Error> {
        // Open directly with the requested Exact format, trying matching
        // devices in enumeration order — the first openable one wins.
        let cameras = match nokhwa::query(nokhwa::utils::ApiBackend::Auto) {
            Ok(cameras) => cameras,
            Err(e) => return Err(capture_err(e)),
        };
        let needle = config.name.to_lowercase();
        let mut last_err = Error::Config(format!("camera not found: {}", config.name));
        for cam in &cameras {
            // Та же семантика, что в CameraConfig::matches: точное имя
            // или contains (case-insensitive).
            let name = cam.human_name();
            if name != config.name && !name.to_lowercase().contains(&needle) {
                continue;
            }
            let format = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(
                CameraFormat::new_from(
                    config.resolution.width,
                    config.resolution.height,
                    to_frame_format(config.format),
                    config.frame_rate.0,
                ),
            ));
            let mut camera = match nokhwa::Camera::new(cam.index().clone(), format) {
                Ok(camera) => camera,
                Err(e) => {
                    last_err = capture_err(e);
                    continue;
                }
            };
            if let Err(e) = camera.open_stream() {
                return Err(capture_err(e));
            }
            let negotiated = match to_capture_format(&camera.camera_format()) {
                Some(format) => format,
                None => {
                    return Err(Error::Config(
                        "negotiated pixel format unsupported".to_string(),
                    ));
                }
            };
            return Ok(NokhwaSession { camera, negotiated });
        }
        Err(last_err)
    }
}

impl CaptureSession for NokhwaSession {
    fn negotiated(&self) -> CaptureFormat {
        self.negotiated
    }

    fn frame(&mut self) -> std::result::Result<Frame, FrameError> {
        // Stamp after frame_raw(): the call blocks until the next frame
        // arrives, so stamping before the wait would inflate the measured
        // latency by one frame period (same anchor as the v4l2 backend).
        let buf = match self.camera.frame_raw() {
            Ok(data) => data.into_owned(),
            Err(e) => return Err(FrameError::Recoverable(e.to_string())),
        };
        let timestamp = Instant::now();
        // Буфер может быть больше самого кадра (ёмкость буфера vs
        // длина кадра) — длина jpeg (SOI..EOI) вычисляется здесь.
        let data = match self.negotiated.format {
            crate::config::camera::PixelConfig::Mjpeg => FrameData::Jpeg {
                len: jpeg_len(&buf),
                buf,
            },
            crate::config::camera::PixelConfig::Yuyv => FrameData::Yuyv { buf },
        };
        Ok(Frame { timestamp, data })
    }
}

impl Drop for NokhwaSession {
    fn drop(&mut self) {
        if let Err(e) = self.camera.stop_stream() {
            log::error!("failed to stop stream: {}", e);
        }
    }
}

/// Длина jpeg-кадра (конец EOI, включительно) в буфере. Поиск FFD9 —
/// через SIMD memmem: в entropy-данных FF заэскейпен как FF00, поэтому
/// первое вхождение FFD9 после SOI — EOI. `None` — кадр без EOI.
fn jpeg_len(buf: &[u8]) -> Option<usize> {
    let start = if buf.starts_with(&[0xFF, 0xD8]) {
        0
    } else {
        buf.windows(2).position(|w| w == [0xFF, 0xD8])?
    };
    Some(memchr::memmem::find(&buf[start..], b"\xff\xd9")? + start + 2)
}

fn to_capture_format(f: &CameraFormat) -> Option<CaptureFormat> {
    use crate::config::camera::{FrameRateConfig, PixelConfig, ResolutionConfig};
    let format = match f.format() {
        FrameFormat::YUYV => PixelConfig::Yuyv,
        FrameFormat::MJPEG => PixelConfig::Mjpeg,
        _ => return None,
    };
    Some(CaptureFormat {
        resolution: ResolutionConfig {
            width: f.resolution().width(),
            height: f.resolution().height(),
        },
        frame_rate: FrameRateConfig(f.frame_rate()),
        format,
    })
}

fn to_frame_format(f: crate::config::camera::PixelConfig) -> FrameFormat {
    match f {
        crate::config::camera::PixelConfig::Yuyv => FrameFormat::YUYV,
        crate::config::camera::PixelConfig::Mjpeg => FrameFormat::MJPEG,
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
fn list_formats_for_index(index: &CameraIndex) -> std::result::Result<Vec<CameraFormat>, String> {
    use nokhwa_bindings_macos::AVCaptureDevice;
    let device = match AVCaptureDevice::new(index) {
        Ok(device) => device,
        Err(e) => return Err(e.to_string()),
    };
    let mut formats = match device.supported_formats() {
        Ok(formats) => formats,
        Err(e) => return Err(e.to_string()),
    };
    sort_and_dedup(&mut formats);
    Ok(formats)
}

#[cfg(not(target_os = "macos"))]
fn list_formats_for_index(index: &CameraIndex) -> std::result::Result<Vec<CameraFormat>, String> {
    let format = RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestResolution);
    let mut camera = match nokhwa::Camera::new(index.clone(), format) {
        Ok(camera) => camera,
        Err(e) => return Err(e.to_string()),
    };
    let mut formats = match camera.compatible_camera_formats() {
        Ok(formats) => formats,
        Err(e) => return Err(e.to_string()),
    };
    sort_and_dedup(&mut formats);
    Ok(formats)
}

#[cfg(test)]
mod tests {
    use super::jpeg_len;

    #[test]
    fn jpeg_len_finds_eoi() {
        let buf = [0xFF, 0xD8, 1, 2, 0xFF, 0x00, 3, 0xFF, 0xD9, 9, 9];
        assert_eq!(jpeg_len(&buf), Some(9));
    }

    #[test]
    fn jpeg_len_skips_escaped_ff() {
        // FF 00 — escape, не EOI; первое настоящее FFD9 — в конце.
        let buf = [0xFF, 0xD8, 0xFF, 0x00, 0xFF, 0xD9];
        assert_eq!(jpeg_len(&buf), Some(6));
    }

    #[test]
    fn jpeg_len_none_without_markers() {
        assert_eq!(jpeg_len(&[1, 2, 3]), None);
        assert_eq!(jpeg_len(&[0xFF, 0xD8, 1, 2]), None); // SOI без EOI
    }
}
