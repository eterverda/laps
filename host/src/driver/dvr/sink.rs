//! DVR-sink: `FrameSink`-адаптер над Recorder. Живёт в dvr, а не в camera:
//! он знает про Recorder/Options/RecordState, и зависимость направлена
//! `dvr → camera::capture` (контракт sink'а), а не наоборот. Camera
//! pipeline владеет экземпляром и дёргает start/stop по командам UI.

use super::{Options, Recorder, SharedRecordState};
use crate::config::camera::FrameRateConfig;
use crate::driver::camera::capture::{Frame, FrameData, FrameSink};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// HARD HACK / ДИСКЛЕЙМЕР. Камера может отдавать кадры быстрее, чем
/// запрошено: macOS (AVFoundation) часто открывает поток на 60 fps при
/// запросе 30, драйвер вправе игнорировать запрошенный fps вообще. Вместо
/// форков биндингов мы троттлим поток на входе DVR-sink, но только когда
/// эффективный fps записи ≤ 30 (dvr.frame-rate, иначе camera.frame-rate):
/// при запросе больше 30 каждый кадр пишется как есть. Live view
/// обновляется на полном fps камеры. Интервал 25 мс (а не 33.3 мс) выбран
/// так, чтобы при ровном 60 fps источника записывался примерно каждый
/// второй кадр, то есть ~30 fps, не боясь пограничного джиттера.
const RECORD_THROTTLE_MIN_INTERVAL: Duration = Duration::from_millis(25);

/// Возвращает true, если кадр нужно отправить в DVR. Для эффективного
/// fps записи ≤ 30 дропаем кадры, пришедшие раньше 25 мс после последнего
/// записанного; при fps > 30 каждый кадр проходит.
fn should_record_frame(fps: FrameRateConfig, last_recorded_frame: &mut Instant) -> bool {
    if fps.0 > 30 {
        return true;
    }
    if last_recorded_frame.elapsed() >= RECORD_THROTTLE_MIN_INTERVAL {
        *last_recorded_frame = Instant::now();
        true
    } else {
        false
    }
}

/// Держит Recorder, троттлит избыточный поток (HARD HACK выше),
/// конвертирует Instant→epoch millis якорем. Recorder живёт независимо от
/// жизни pipeline: drop sink'а (как и раньше drop recorder'а в потоке)
/// отпускает writer в фоновую финализацию.
pub struct DvrSink {
    recorder: Option<Recorder>,
    /// Эффективный fps записи (dvr.frame-rate, иначе camera.frame-rate).
    fps: FrameRateConfig,
    last_recorded_frame: Instant,
    negotiated: crate::driver::camera::CaptureFormat,
    state: SharedRecordState,
    /// Якорь Instant→epoch millis (ставится до цикла кадров).
    anchor: (Instant, u64),
}

impl DvrSink {
    pub fn new(
        fps: FrameRateConfig,
        negotiated: crate::driver::camera::CaptureFormat,
        state: SharedRecordState,
    ) -> Self {
        Self {
            recorder: None,
            fps,
            last_recorded_frame: Instant::now(),
            negotiated,
            state,
            anchor: (Instant::now(), super::epoch_millis()),
        }
    }

    pub fn set_fps(&mut self, fps: FrameRateConfig) {
        self.fps = fps;
    }

    /// Файл создаётся здесь, а не в writer-потоке: ошибка (диск полон,
    /// нет прав) логируется, RecordState остаётся false — как раньше.
    pub fn start(&mut self, options: &Options) {
        self.set_fps(FrameRateConfig(options.camera.dvr_frame_rate().0));
        match Recorder::start(options, &self.negotiated, &self.state) {
            Ok(recorder) => self.recorder = Some(recorder),
            Err(e) => {
                log::error!("dvr: recording unavailable: {}", e);
                self.recorder = None;
            }
        }
    }

    pub fn stop(&mut self) {
        // drop: writer finalize'ит файл в фоне.
        self.recorder = None;
    }

    fn epoch_millis(&self, t: Instant) -> u64 {
        self.anchor.1 + t.saturating_duration_since(self.anchor.0).as_millis() as u64
    }
}

impl FrameSink for DvrSink {
    fn on_frame(&mut self, frame: Arc<Frame>) {
        if self.recorder.is_none() {
            return;
        }
        if !should_record_frame(self.fps, &mut self.last_recorded_frame) {
            return;
        }
        let ts = self.epoch_millis(frame.timestamp);
        match &frame.data {
            // Известные к записи варианты; буфер не копируется (P4).
            FrameData::Jpeg { .. } | FrameData::Rgba { .. } => {
                self.recorder.as_ref().unwrap().push(frame, ts)
            }
            // Yuyv и любые будущие варианты сюда не должны доходить
            // (инвариант pipeline) — принципиально неизвестное отвергаем.
            other => log::error!("dvr sink: unexpected frame variant {other:?}, skipped"),
        }
    }

    fn on_stop(&mut self) {
        self.stop();
    }
}
