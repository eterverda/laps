//! Контракты слоя источника: трейты [`Capture`]/[`CaptureSession`]/
//! [`FrameSink`], типы кадра. Реализации-бэкенды — соседние модули
//! (`nokhwa.rs`, позже v4l2/mf/avf); выбор бэкенда — в `mod.rs`
//! родительского модуля. Pipeline и sink'и зависят только от этого файла.
//!
//! Терминология: один кадр — `jpeg` ([`FrameData::Jpeg`]); формат потока
//! камеры — `MJPEG` (`PixelConfig::Mjpeg`); совокупность jpeg-кадров в
//! файле — `mjpeg`-поток. Никаких «MJPEG-кадров».

use crate::config::camera::{CameraConfig, FrameRateConfig, PixelConfig, ResolutionConfig};
use std::sync::Arc;
use std::time::Instant;

/// Формат потока = то, из чего собирается CameraConfig без dvr-блока.
/// Типы полей — из config::camera, дублей нет.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureFormat {
    pub resolution: ResolutionConfig,
    pub frame_rate: FrameRateConfig,
    pub format: PixelConfig,
}

/// Снимок для UI/CLI: одно устройство и его форматы. Не ключ для open:
/// `open` резолвит устройство по имени конфига заново (см. [`Capture`]).
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    pub formats: Vec<CaptureFormat>,
}

/// Ошибка захвата кадра.
#[derive(Debug)]
pub enum FrameError {
    /// Пропустить кадр и продолжать (временный сбой; при подряд идущих
    /// ошибках снаружи работает watchdog по времени без кадров).
    Recoverable(String),
    /// Камера потеряна окончательно — остановить capture-поток.
    /// Пока не конструируется (nokhwa всё отдаёт как Recoverable);
    /// для будущих бэкендов (V4L2: EIO/ENODEV) — см. docs/capture-backend-plan.md.
    #[allow(dead_code)]
    Unrecoverable(String),
}

/// Кадр: таймстемп + контент. Таймстемп штампует бэкенд каждый кадр —
/// из низлежащих данных устройства, если они есть (V4L2
/// `v4l2_buffer.timestamp`, приведённый к Instant), иначе время прихода.
#[derive(Debug)]
pub struct Frame {
    pub timestamp: Instant,
    pub data: FrameData,
}

/// Контент кадра. Источник выдаёт `Jpeg`/`Yuyv` и НЕ обрезает буфер
/// (P4: срез по `len` делает потребитель). `Rgba` — только от pipeline
/// (после конверсии) или от нативно-RGBA бэкенда будущего: sink'и `Yuyv`
/// не получают никогда — это инвариант pipeline, а не типа.
/// Общность для нескольких потребителей выражается снаружи:
/// `Arc<Frame>` (см. FrameSink), а не полями внутри вариантов.
#[derive(Debug)]
pub enum FrameData {
    /// `buf` — весь полученный буфер; `len` — длина jpeg-кадра SOI..EOI,
    /// вычисленная источником (FFD9-поиск). `None` — EOI не найден
    /// (битый кадр: на экран идём, в файл — нет).
    Jpeg { buf: Vec<u8>, len: Option<usize> },
    /// Сырые Y0 U Y1 V. Дальше pipeline не проходит без конверсии.
    Yuyv { buf: Vec<u8> },
    /// Декодированная картинка.
    Rgba { rgba: egui::ColorImage },
}

/// Фабрика сессий захвата. Два метода: перечисление устройств (с
/// форматами, одним проходом) и открытие по `CameraConfig`.
pub trait Capture {
    type Session: CaptureSession;

    fn list_devices() -> Result<Vec<DeviceInfo>, String>;
    /// Резолвит устройство по имени конфига свежим перечислением:
    /// индексы V4L2 нестабильны при переподключении, снимок
    /// `list_devices` — не ключ.
    fn open(config: &CameraConfig) -> Result<Self::Session, String>;
}

/// Открытая камера: negotiated-формат и блокирующее чтение кадров.
pub trait CaptureSession {
    /// Фактически согласованный формат (после open/stream-on).
    fn negotiated(&self) -> CaptureFormat;
    /// Блокирует до следующего кадра. Ошибки — через [`FrameError`].
    fn frame(&mut self) -> Result<Frame, FrameError>;
}

/// Потребитель кадров pipeline. Инвариант: sink получает только `Jpeg`
/// (DVR) или `Rgba` (любой); `Yuyv` pipeline конвертировал до fan-out.
/// Кадр приходит как `Arc<Frame>`: несколько sink'ов получают один кадр
/// бампом счётчика, без копий; sink вправе хранить Arc за пределами
/// коллбэка (UI-slot, канал writer'а).
pub trait FrameSink {
    fn on_frame(&mut self, frame: Arc<Frame>);
    /// Pipeline завершается (камера потеряна, стоп) — sink подчищается.
    fn on_stop(&mut self) {}
}
