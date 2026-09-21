pub mod dvr;
pub mod webcam;

use crossbeam_utils::atomic::AtomicCell;
use std::sync::Arc;

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

/// Состояние записи: writer жив и пишет на диск.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RecordState {
    pub ok: bool,
    /// Скользящий fps фактически пишущихся кадров (окно ~0.5 с).
    pub fps: f32,
}

/// Общий для потока и UI хэндл состояния камеры.
pub type SharedCameraState = Arc<AtomicCell<CameraState>>;

/// Общий для потока и UI хэндл состояния записи.
pub type SharedRecordState = Arc<AtomicCell<RecordState>>;
