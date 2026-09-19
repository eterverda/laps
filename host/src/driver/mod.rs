pub mod dvr;
pub mod webcam;

use crossbeam_utils::atomic::AtomicCell;
use std::sync::Arc;

/// Состояние capture-потока: стрим открыт и тред жив.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureState {
    pub ok: bool,
}

/// Состояние записи: writer жив и пишет на диск.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordState {
    pub ok: bool,
}

/// Общий для потока и UI хэндл состояния захвата.
pub type SharedCaptureState = Arc<AtomicCell<CaptureState>>;

/// Общий для потока и UI хэндл состояния записи.
pub type SharedRecordState = Arc<AtomicCell<RecordState>>;
