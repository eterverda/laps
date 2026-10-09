//! DVR: запись потока камеры на диск. MJPEG — пасsthrough готовых
//! JPEG-блобов без перекодировки. YUYV — декодированный RGBA кодируется
//! в JPEG и пишется в тот же контейнер (MKV/MP4/MOV). Один файл на запуск
//! камеры, финализация при остановке (по Drop). RecordState целиком
//! принадлежит writer-потоку (старт/окна fps/ошибка/выход) — вторых
//! писателей быть не должно.

mod encode;
mod mkv;
mod mov;
mod mp4;
mod sink;

pub use sink::DvrSink;

use crossbeam_utils::atomic::AtomicCell;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mkv::MkvWriter;
use mov::MovWriter;
use mp4::Mp4Writer;

use crate::config::camera::ContainerConfig;

/// Состояние записи: writer жив и пишет на диск.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RecordState {
    pub ok: bool,
    /// Скользящий fps фактически пишущихся кадров (окно ~0.5 с).
    pub fps: f32,
}

/// Общий для потока и UI хэндл состояния записи.
pub type SharedRecordState = Arc<AtomicCell<RecordState>>;

/// Общий интерфейс контейнеров. `ts_ms` — момент кадра, мс с Unix-эпохи;
/// пишется как реальный таймкод (равномерный таймлайн по fps никто не
/// строит). Конструктор `create` — inherent у каждого писателя, трейт
/// покрывает жизненный цикл записи. `Send`: writer живёт в своём потоке.
pub trait VideoWriter: Send {
    fn write_frame(&mut self, jpeg: &[u8], ts_ms: u64) -> io::Result<()>;
    fn sync_data(&mut self) -> io::Result<()>;
    /// Потребляет писателя: таблицы/индекс, патчи заголовков, fsync.
    fn finalize(self: Box<Self>) -> io::Result<()>;
}

/// Размеры кадра записи (реэкспорт, в dvr без суффикса Config).
pub use crate::config::camera::ResolutionConfig as Resolution;

/// Видео-читалка: индексированный доступ к кадрам записи. Зеркало
/// `VideoWriter`: `open` — у конкретных форматов, выбор формата —
/// фабрика по расширению (`open_reader`), как у писателей match на
/// `ContainerConfig`. Кадр читается в буфер вызывающего — постоянная
/// память на кадр не тратится.
pub trait VideoReader: Send {
    /// Число кадров.
    fn frame_count(&self) -> usize;
    /// pts кадра i, мс от начала файла.
    fn timestamp(&self, i: usize) -> u64;
    /// Максимальный размер кадра — минимальная длина буфера под read_frame_into.
    fn max_frame_len(&self) -> usize;
    /// Размеры кадра.
    fn resolution(&self) -> Resolution;
    /// Кадр i в буфер (длиной ≥ max_frame_len). Возвращает (pts, байт).
    fn read_frame_into(&mut self, i: usize, buf: &mut [u8]) -> io::Result<(u64, usize)>;
}

/// Открыть читалку по расширению файла.
pub fn open_reader(path: &std::path::Path) -> io::Result<Box<dyn VideoReader>> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("mkv") => Ok(Box::new(mkv::MkvReader::open(path)?)),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported recording format: {other:?}"),
        )),
    }
}

/// Каталог записей по умолчанию, относительно рабочей директории.
pub const CAPTURES_DIR: &str = "captures";

/// Параметры записи одной камеры.
pub struct RecordParams {
    pub dir: PathBuf,
    pub camera_id: String,
    /// Справочно: для логов и отчёта. Размеры для записи авторитетны
    /// из согласованного формата, см. Recorder::start.
    pub camera: crate::config::camera::CameraConfig,
}

const CHANNEL_CAP: usize = 64;
const SYNC_INTERVAL: Duration = Duration::from_secs(5);

/// Принимает кадры из capture-потока (try_send, не блокирует захват),
/// пишет их writer-потоком. `state` — сигналинг в UI: ok=true, пока writer
/// жив; гасится при непоправимой ошибке записи. Drop разрывает канал:
/// writer финализирует файл в фоне (джойна намеренно нет — fsync
/// многогигабайтного файла дохлый для UI, устройство камеры writer
/// не держит, detach безопасен).
pub struct Recorder {
    state: SharedRecordState,
    sender: Option<
        crossbeam_channel::Sender<(std::sync::Arc<crate::driver::camera::capture::Frame>, u64)>,
    >,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Дропы в канале (try_send failed), считает push, читает
    /// writer-поток для отчёта.
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Recorder {
    pub fn start(
        params: &RecordParams,
        negotiated: &crate::driver::camera::capture::CaptureFormat,
        state: &SharedRecordState,
    ) -> io::Result<Self> {
        use crate::driver::camera::capture::FrameData;
        std::fs::create_dir_all(&params.dir)?;
        let stem = format!("{}-{}-mjpeg", timestamp_prefix(), params.camera_id);
        // Файл создаём здесь, а не в writer-потоке: ошибка (диск полон,
        // нет прав) уезжает вызывающему вместо молчаливой мёртвой записи.
        let resolution = negotiated.resolution;
        let extension = match params.camera.dvr.container {
            ContainerConfig::Mkv => "mkv",
            ContainerConfig::Mov => "mov",
            ContainerConfig::Mp4 => "mp4",
        };
        let path = params.dir.join(format!("{stem}.{extension}"));
        let mut writer: Box<dyn VideoWriter> = match params.camera.dvr.container {
            ContainerConfig::Mkv => Box::new(MkvWriter::create(&path, resolution)?),
            ContainerConfig::Mov => Box::new(MovWriter::create(&path, resolution)?),
            ContainerConfig::Mp4 => Box::new(Mp4Writer::create(&path, resolution)?),
        };
        state.store(RecordState { ok: true, fps: 0.0 });
        let writer_path = path.clone();
        let (sender, receiver) = crossbeam_channel::bounded::<(
            std::sync::Arc<crate::driver::camera::capture::Frame>,
            u64,
        )>(CHANNEL_CAP);
        let state_clone = state.clone();
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        // В поток уходят owned-копии: ссылки на параметры не 'static.
        let thread = std::thread::spawn(move || {
            let mut last_sync = Instant::now();
            // Замер пишущихся кадров для UI: второй, независимый от
            // capture-потока счётчик — отражает потери в канале.
            let mut fps_counter = crate::driver::camera::fps::FpsCounter::default();
            // Латентность кадра: от его таймстемпа до окончания записи
            // в файл (включая ожидание в канале). Один лог при финализации.
            let mut latency = crate::driver::camera::fps::FrameStats::default();
            // Длительность перекодирования rgba→jpeg (фаза, не латентность).
            let mut encode = crate::driver::camera::fps::FrameStats::default();
            // Канал закрывается по Drop отправителя → finalize и выход.
            while let Ok((frame, ts)) = receiver.recv() {
                // Перекодированный jpeg текущего кадра: живёт до конца
                // итерации, освобождается после write_frame.
                let encoded: Option<Vec<u8>>;
                let data: &[u8] = match &frame.data {
                    FrameData::Jpeg { buf, len } => {
                        let Some(len) = len else {
                            log::warn!("dvr: frame without EOI, skipped");
                            continue;
                        };
                        &buf[..*len]
                    }
                    FrameData::Rgba { rgba } => {
                        let t0 = Instant::now();
                        // Кадр общий (Arc), пиксели только заимствуем.
                        let jpeg = match encode::encode_rgba_to_jpeg(
                            bytemuck::cast_slice(&rgba.pixels),
                            resolution.width,
                            resolution.height,
                        ) {
                            Ok(jpeg) => jpeg,
                            Err(e) => {
                                log::error!("dvr: jpeg encode failed, recording aborted: {}", e);
                                state_clone.store(RecordState {
                                    ok: false,
                                    fps: 0.0,
                                });
                                return;
                            }
                        };
                        encode.on_process(t0.elapsed());
                        // jpeg живёт до write_frame в этом scope; хвост
                        // храним во временной переменной.
                        encoded = Some(jpeg);
                        encoded.as_ref().unwrap()
                    }
                    FrameData::Yuyv { .. } => {
                        // Инвариант pipeline: до sink'ов Yuyv не доходит.
                        log::error!("dvr: raw YUYV frame reached writer, skipped");
                        continue;
                    }
                };
                if let Err(e) = writer.write_frame(data, ts) {
                    log::error!("dvr: write failed, recording aborted: {}", e);
                    state_clone.store(RecordState {
                        ok: false,
                        fps: 0.0,
                    });
                    return;
                }
                // Кадр (Arc) ещё жив: замер от его таймстемпа до конца
                // записи.
                latency.on_frame(&frame);
                fps_counter.on_frame();
                // Пауза кадров не затирает последнее известное значение:
                // записываем только свежий замер.
                if let Some(fps) = fps_counter.fps() {
                    state_clone.store(RecordState {
                        ok: true,
                        fps: fps as f32,
                    });
                }
                if last_sync.elapsed() >= SYNC_INTERVAL {
                    if let Err(e) = writer.sync_data() {
                        log::error!("dvr: sync failed: {}", e);
                    }
                    last_sync = Instant::now();
                }
            }
            // Канал закрыт. REC гасим до (медленного) fsync, чтобы
            // индикатор не висел.
            state_clone.store(RecordState {
                ok: false,
                fps: 0.0,
            });
            match writer.finalize() {
                Ok(()) => log::info!("dvr: finalized {:?}", writer_path),
                Err(e) => log::error!("dvr: finalize failed for {:?}: {}", writer_path, e),
            }
            // Один лог на запись: латентность «захват → кадр на диске»
            // и длительность перекодирования.
            if let Some(stats) = latency.snapshot() {
                log::info!("dvr: frame-to-disk latency: {stats}");
            }
            if let Some(stats) = encode.snapshot() {
                log::info!("dvr: rgba→jpeg encode duration: {stats}");
            }
        });
        log::info!("dvr: recording to {:?}", path);
        Ok(Self {
            state: state.clone(),
            sender: Some(sender),
            thread: Some(thread),
            dropped,
        })
    }

    /// Из pipeline. `ts` — момент захвата кадра, мс с Unix-эпохи
    /// (Instant→epoch конверсия у якоря в pipeline). Writer мёртв — кадр
    /// просто выбрасывается (состояние уже отражено в RecordState, UI
    /// показал). Переполнение канала = дроп кадра + warn (захват важнее
    /// записи).
    pub fn push(&self, frame: std::sync::Arc<crate::driver::camera::capture::Frame>, ts: u64) {
        if !self.state.load().ok {
            return;
        }
        if let Some(sender) = &self.sender {
            if sender.try_send((frame, ts)).is_err() {
                self.dropped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                log::warn!("dvr: frame dropped (writer busy)");
            }
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.sender.take(); // разрыв канала → writer finalize'ит файл
        // Джойн намеренно отсутствует: finalize + sync_all идут в фоне,
        // UI/join capture-потока их не ждут. Writer держит только файл.
        self.thread.take();
    }
}

/// Миллисекунды с Unix-эпохи (для меток файла DVR).
pub fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Префикс имён файлов записи: `YYYY-mm-dd-HH-MM-SS.SSS` (локальное время).
pub fn timestamp_prefix() -> String {
    use time::macros::format_description;
    const FMT: &[time::format_description::FormatItem<'static>] =
        format_description!("[year]-[month]-[day]-[hour]-[minute]-[second].[subsecond digits:3]");
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| {
        log::warn!("dvr: local time unavailable, falling back to UTC");
        time::OffsetDateTime::now_utc()
    });
    now.format(&FMT).unwrap_or_else(|e| {
        log::error!("dvr: timestamp format failed: {}", e);
        epoch_millis().to_string()
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn timestamp_prefix_format() {
        let prefix = super::timestamp_prefix();
        assert!(
            lazy_regex::regex_is_match!(r"^\d{4}-\d{2}-\d{2}-\d{2}-\d{2}-\d{2}\.\d{3}$", &prefix),
            "bad timestamp prefix: {prefix}"
        );
    }

    /// YUYV: RGBA перекодируется в JPEG и пишется в контейнер по
    /// умолчанию (MKV); REC гаснет по выходу writer-потока.
    #[test]
    fn yuyv_reencode_records_to_container() {
        use super::{RecordState, SharedRecordState};
        use crate::config::camera::{CameraConfig, PixelConfig};
        use crossbeam_utils::atomic::AtomicCell;
        use std::sync::Arc;

        let dir = std::env::temp_dir().join(format!("laps-dvr-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state: SharedRecordState = Arc::new(AtomicCell::new(RecordState {
            ok: false,
            fps: 0.0,
        }));
        let params = super::RecordParams {
            dir: dir.clone(),
            camera_id: "camera-1".to_owned(),
            camera: CameraConfig::new("Test", 64, 48, 30, PixelConfig::Yuyv),
        };
        let negotiated = crate::driver::camera::capture::CaptureFormat {
            resolution: crate::config::camera::ResolutionConfig {
                width: 64,
                height: 48,
            },
            frame_rate: crate::config::camera::FrameRateConfig(30),
            format: PixelConfig::Yuyv,
        };
        {
            let recorder = super::Recorder::start(&params, &negotiated, &state).unwrap();
            for _ in 0..3 {
                let pixels = vec![egui::Color32::BLACK; 64 * 48];
                recorder.push(
                    std::sync::Arc::new(crate::driver::camera::capture::Frame {
                        timestamp: std::time::Instant::now(),
                        data: crate::driver::camera::capture::FrameData::Rgba {
                            rgba: egui::ColorImage::new([64, 48], pixels),
                        },
                    }),
                    super::epoch_millis(),
                );
            }
        } // drop: writer дописывает файл в фоне

        // Джойна нет по дизайну — ждём гашение REC (writer гасит его по
        // закрытию канала, до finalize/fsync).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.load().ok && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(!state.load().ok);

        // Видеофайл есть (контейнер по умолчанию — MKV), других файлов нет.
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert!(
            entries
                .iter()
                .any(|p| p.extension().is_some_and(|e| e == "mkv")),
            "no mkv in {entries:?}"
        );
        assert!(
            entries
                .iter()
                .all(|p| p.extension().is_some_and(|e| e == "mkv")),
            "unexpected files: {entries:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
