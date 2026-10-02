//! Реализация capture-трейта поверх nokhwa (все платформы).
//!
//! nokhwa заархивирована и является переходным бэкендом: интерфейс
//! (`super`) от неё не зависит, замена — отдельным модулем
//! (docs/capture-backend-plan.md). macOS-специфичное перечисление
//! форматов через AVCaptureDevice живёт здесь же, за cfg.

use super::{Capture, CaptureFormat, CaptureSession, DeviceInfo, Frame, FrameData, FrameError};
use crate::config::camera::CameraConfig;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType};
use std::time::Instant;

pub struct NokhwaCapture;

pub struct NokhwaSession {
    camera: nokhwa::Camera,
    negotiated: CaptureFormat,
}

impl NokhwaCapture {
    /// Резолвит имя в устройство заново: перечисляет и ищет по имени.
    /// Точное совпадение предпочтительнее, иначе первое contains
    /// (case-insensitive) — та же семантика, что в `CameraConfig::matches`.
    /// Одинаковые близнецы неразличимы без серийника (см. V4L2-бэкенд
    /// в docs/capture-backend-plan.md).
    fn resolve_index(name: &str) -> Result<CameraIndex, String> {
        let cameras = nokhwa::query(nokhwa::utils::ApiBackend::Auto).map_err(|e| e.to_string())?;
        let needle = name.to_lowercase();
        cameras
            .iter()
            .find(|cam| cam.human_name() == name)
            .or_else(|| {
                cameras
                    .iter()
                    .find(|cam| cam.human_name().to_lowercase().contains(&needle))
            })
            .map(|cam| cam.index().clone())
            .ok_or_else(|| format!("camera not found: {name}"))
    }
}

impl Capture for NokhwaCapture {
    type Session = NokhwaSession;

    fn list_devices() -> Result<Vec<DeviceInfo>, String> {
        let cameras = nokhwa::query(nokhwa::utils::ApiBackend::Auto).map_err(|e| e.to_string())?;
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

    fn open(config: &CameraConfig) -> Result<Self::Session, String> {
        let index = Self::resolve_index(&config.name)?;
        let format =
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(CameraFormat::new_from(
                config.resolution.width,
                config.resolution.height,
                to_frame_format(config.format),
                config.frame_rate.0,
            )));
        let mut camera = nokhwa::Camera::new(index, format).map_err(|e| e.to_string())?;
        camera.open_stream().map_err(|e| e.to_string())?;
        let negotiated = to_capture_format(&camera.camera_format())
            .ok_or_else(|| "negotiated unsupported pixel format".to_string())?;
        Ok(NokhwaSession { camera, negotiated })
    }
}

impl CaptureSession for NokhwaSession {
    fn negotiated(&self) -> CaptureFormat {
        self.negotiated
    }

    fn frame(&mut self) -> Result<Frame, FrameError> {
        // nokhwa не отдаёт время захвата с устройства — штампуем по приходу.
        let timestamp = Instant::now();
        let buf = self
            .camera
            .frame_raw()
            .map(|data| data.into_owned())
            .map_err(|e| FrameError::Recoverable(e.to_string()))?;
        // nokhwa на V4L2 отдаёт весь mmap-буфер (его ёмкость равна размеру
        // несжатого кадра), а не реальную длину кадра: драйвер пишет jpeg
        // в начало, после EOI — хвост со старыми данными. Длина jpeg-кадра
        // (SOI..EOI) — ответственность источника (P4); буфер не копируем.
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
fn list_formats_for_index(index: &CameraIndex) -> Result<Vec<CameraFormat>, String> {
    use nokhwa_bindings_macos::AVCaptureDevice;
    let device = AVCaptureDevice::new(index).map_err(|e| e.to_string())?;
    let mut formats = device.supported_formats().map_err(|e| e.to_string())?;
    sort_and_dedup(&mut formats);
    Ok(formats)
}

#[cfg(not(target_os = "macos"))]
fn list_formats_for_index(index: &CameraIndex) -> Result<Vec<CameraFormat>, String> {
    let format = RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestResolution);
    let mut camera = nokhwa::Camera::new(index.clone(), format).map_err(|e| e.to_string())?;
    let mut formats = camera
        .compatible_camera_formats()
        .map_err(|e| e.to_string())?;
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
