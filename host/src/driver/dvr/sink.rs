//! DVR-sink: реализация `FrameSink` над Recorder. Контракт `FrameSink`
//! живёт в `camera::capture` (трейт), реализация — здесь, в dvr, рядом
//! с Recorder и политикой троттлинга; композиция — в pipeline camera,
//! который создаёт DvrSink и дёргает start/stop по командам UI.

use super::{RecordParams, Recorder, SharedRecordState};
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
/// записанного; при fps > 30 каждый кадр проходит. Сравниваются ДЕЛЬТЫ
/// таймстемпов кадров (не Instant::now()): сегодня штампы — время прихода
/// (nokhwa), бэкенды с device time (V4L2) автоматически дадут пейсинг по
/// сетке устройства, без пакетной выдачи буферов в решениях.
fn should_record_frame(
    fps: FrameRateConfig,
    frame_ts: Instant,
    next_eligible: &mut Instant,
) -> bool {
    if fps.0 > 30 {
        return true;
    }
    if frame_ts < *next_eligible {
        return false;
    }
    *next_eligible = frame_ts + RECORD_THROTTLE_MIN_INTERVAL;
    true
}

/// Держит Recorder, троттлит избыточный поток (HARD HACK выше),
/// конвертирует Instant→epoch millis якорем. Recorder живёт независимо от
/// жизни pipeline: drop sink'а (как и раньше drop recorder'а в потоке)
/// отпускает writer в фоновую финализацию.
pub struct DvrSink {
    recorder: Option<Recorder>,
    /// Эффективный fps записи (dvr.frame-rate, иначе camera.frame-rate).
    fps: FrameRateConfig,
    /// Момент, с которого следующий кадр можно писать (последний
    /// записанный + 25 мс). Стартовое now(): первый кадр записи штампится
    /// позже и проходит всегда — спец-значения не нужны.
    next_eligible: Instant,
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
            next_eligible: Instant::now(),
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
    pub fn start(&mut self, params: &RecordParams) {
        self.set_fps(FrameRateConfig(params.camera.dvr_frame_rate().0));
        self.next_eligible = Instant::now(); // первый кадр записи проходит всегда
        match Recorder::start(params, &self.negotiated, &self.state) {
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
        let Some(recorder) = &self.recorder else {
            return;
        };
        if !should_record_frame(self.fps, frame.timestamp, &mut self.next_eligible) {
            return;
        }
        let ts = self.epoch_millis(frame.timestamp);
        match &frame.data {
            // Известные к записи варианты; буфер не копируется (P4).
            FrameData::Jpeg { .. } | FrameData::Rgba { .. } => recorder.push(frame, ts),
            // Yuyv и любые будущие варианты сюда не должны доходить
            // (инвариант pipeline) — принципиально неизвестное отвергаем.
            other => log::error!("dvr sink: unexpected frame variant {other:?}, skipped"),
        }
    }

    fn on_stop(&mut self) {
        self.stop();
    }
}
