//! DVR: запись потока камеры на диск. MJPEG — пасsthrough готовых
//! JPEG-блобов без перекодировки. YUYV — декодированный RGBA кодируется
//! в JPEG и пишется в тот же контейнер (MKV/MP4/MOV). Один файл на запуск
//! камеры, финализация при остановке (по Drop). RecordState целиком
//! принадлежит writer-потоку (старт/окна fps/ошибка/выход) — вторых
//! писателей быть не должно.

mod encode;
mod mkv;
mod mp4;
mod sink;

pub use sink::DvrSink;

use crate::config::camera::{CameraConfig, ResolutionConfig};
use crate::driver::camera::capture::{CaptureFormat, Frame, FrameData};
use crate::driver::camera::fps::{FpsCounter, FrameStats};

use crossbeam_utils::atomic::AtomicCell;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mkv::MkvWriter;
use mp4::IsobmffWriter;

use crate::config::camera::ContainerConfig;

/// Состояние записи: жизненный цикл writer-потока.
/// Idle → Preparing (файл создан) → Writing (первый кадр был — метка
/// внутри варианта, не Option) → Finalizing (стоп, финализация в фоне)
/// → Done (guard по ВСЕМ выходам потока). Из Preparing/Writing → Failed
/// (encode/write-ошибка). Тёплый DVR ждёт Done перед открытием файлов —
/// иначе open попадает в дописываемый файл.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RecordState {
    /// Письма нет: начальное, между сессиями.
    Idle,
    /// Файл создан, первого кадра ещё не было.
    Preparing,
    /// Пишет: первый кадр был.
    Writing { first_frame_at: Instant, fps: f32 },
    /// Умер по ошибке (encode/write fail) — запись оборвана.
    Failed,
    /// Стоп: финализируется в фоне (Cues, патчи, sync).
    Finalizing,
    /// Финализация завершена — файл можно открывать.
    Done,
}

/// Guard: поток writer'а завершился (по ЛЮБОМУ выходу, включая ранние
/// return по ошибкам) — ждущие тёплый DVR отпускает.
struct SetDone(SharedRecordState);

impl Drop for SetDone {
    fn drop(&mut self) {
        self.0.store(RecordState::Done);
    }
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
    fn resolution(&self) -> ResolutionConfig;
    /// Кадр i в буфер (длиной ≥ max_frame_len). Возвращает (pts, байт).
    fn read_frame_into(&mut self, i: usize, buf: &mut [u8]) -> io::Result<(u64, usize)>;
}

/// Открыть читалку по расширению файла.
pub fn open_reader(path: &std::path::Path) -> io::Result<Box<dyn VideoReader>> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("mkv") => Ok(Box::new(mkv::MkvReader::open(path)?)),
        // MP4 и MOV читаются одним ридером ISOBMFF (бренды ftyp для
        // чтения не важны).
        Some("mp4") | Some("mov") => Ok(Box::new(mp4::IsobmffReader::open(path)?)),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported recording format: {other:?}"),
        )),
    }
}

/// Каталог записей по умолчанию, относительно рабочей директории.
pub const CAPTURES_DIR: &str = "captures";

/// Параметры записи одной камеры.
#[derive(Debug, Clone)]
pub struct RecordParams {
    pub dir: PathBuf,
    /// Стем имени файла целиком (журнал полёта + camera_id + порядковый
    /// номер сессии записи) — Record добавляет только `-mjpeg.<ext>`.
    pub stem: String,
    /// Справочно: для логов и отчёта. Размеры для записи авторитетны
    /// из согласованного формата, см. Record::start.
    pub camera: CameraConfig,
}

const CHANNEL_CAP: usize = 64;
const SYNC_INTERVAL: Duration = Duration::from_secs(5);

/// Принимает кадры из capture-потока (try_send, не блокирует захват),
/// пишет их writer-потоком. `state` — сигналинг в UI: ok=true, пока writer
/// жив; гасится при непоправимой ошибке записи. Drop разрывает канал:
/// writer финализирует файл в фоне (джойна намеренно нет — fsync
/// многогигабайтного файла дохлый для UI, устройство камеры writer
/// не держит, detach безопасен).
pub struct Record {
    state: SharedRecordState,
    sender: Option<crossbeam_channel::Sender<(std::sync::Arc<Frame>, u64)>>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Дропы в канале (try_send failed), считает push, читает
    /// writer-поток для отчёта.
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Флаг «запись вне гонки»: writer после finalize удаляет файл.
    /// Ставится abort() до drop — поток видит флаг при финализации.
    abort: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Record {
    pub fn start(
        params: &RecordParams,
        negotiated: &CaptureFormat,
        state: &SharedRecordState,
    ) -> io::Result<Self> {
        use FrameData;
        std::fs::create_dir_all(&params.dir)?;
        // Стем полный (с camera_id и номером сессии) — см. RecordParams.
        let stem = format!("{}-mjpeg", params.stem);
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
            ContainerConfig::Mov => Box::new(IsobmffWriter::create_mov(&path, resolution)?),
            ContainerConfig::Mp4 => Box::new(IsobmffWriter::create_mp4(&path, resolution)?),
        };
        state.store(RecordState::Preparing);
        let writer_path = path.clone();
        let (sender, receiver) =
            crossbeam_channel::bounded::<(std::sync::Arc<Frame>, u64)>(CHANNEL_CAP);
        let state_clone = state.clone();
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let abort = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let abort_clone = std::sync::Arc::clone(&abort);
        // В поток уходят owned-копии: ссылки на параметры не 'static.
        let thread = std::thread::spawn(move || {
            // Финализация (включая ранние return по ошибкам) завершена —
            // будим ждущих тёплый DVR.
            let _done = SetDone(state_clone.clone());
            let mut last_sync = Instant::now();
            // Замер пишущихся кадров для UI: второй, независимый от
            // capture-потока счётчик — отражает потери в канале.
            let mut fps_counter = FpsCounter::default();
            // Латентность кадра: от его таймстемпа до окончания записи
            // в файл (включая ожидание в канале). Один лог при финализации.
            let mut latency = FrameStats::default();
            // Длительность перекодирования rgba→jpeg (фаза, не латентность).
            let mut encode = FrameStats::default();
            // Канал закрывается по Drop отправителя → finalize и выход.
            while let Ok((frame, ts)) = receiver.recv() {
                // Первый принятый кадр — запись началась: метка старта
                // для журнала и UI (таймстемп кадра, не момент приёма).
                if matches!(state_clone.load(), RecordState::Preparing) {
                    state_clone.store(RecordState::Writing {
                        first_frame_at: frame.timestamp,
                        fps: 0.0,
                    });
                }
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
                                state_clone.store(RecordState::Failed);
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
                    state_clone.store(RecordState::Failed);
                    return;
                }
                // Кадр (Arc) ещё жив: замер от его таймстемпа до конца
                // записи.
                latency.on_frame(&frame);
                fps_counter.on_frame();
                // Пауза кадров не затирает последнее известное значение:
                // записываем только свежий замер.
                if let Some(fps) = fps_counter.fps() {
                    let first_frame_at = match state_clone.load() {
                        RecordState::Writing { first_frame_at, .. } => first_frame_at,
                        other => panic!("dvr: fps update in {other:?}"),
                    };
                    state_clone.store(RecordState::Writing {
                        first_frame_at,
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
            state_clone.store(RecordState::Finalizing);
            match writer.finalize() {
                Ok(()) => log::info!("dvr: finalized {:?}", writer_path),
                Err(e) => log::error!("dvr: finalize failed for {:?}: {}", writer_path, e),
            }
            // Прерванная (вне гонки) запись: файл корректно закрыт
            // выше — теперь удаляем.
            if abort_clone.load(std::sync::atomic::Ordering::Relaxed) {
                match std::fs::remove_file(&writer_path) {
                    Ok(()) => log::info!("dvr: aborted recording deleted {:?}", writer_path),
                    Err(e) => log::error!("dvr: abort delete failed for {:?}: {}", writer_path, e),
                }
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
            abort,
        })
    }

    /// Прервать запись: файл будет финализирован и удалён (запись вне
    /// гонки). Вызывается до drop.
    pub fn abort(&self) {
        self.abort.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Из pipeline. `ts` — момент захвата кадра, мс с Unix-эпохи
    /// (Instant→epoch конверсия у якоря в pipeline). Writer мёртв — кадр
    /// просто выбрасывается (состояние уже отражено в RecordState, UI
    /// показал). Переполнение канала = дроп кадра + warn (захват важнее
    /// записи).
    pub fn push(&self, frame: std::sync::Arc<Frame>, ts: u64) {
        if !matches!(
            self.state.load(),
            RecordState::Preparing | RecordState::Writing { .. }
        ) {
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

impl Drop for Record {
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

#[cfg(test)]
mod tests {
    /// YUYV: RGBA перекодируется в JPEG и пишется в контейнер по
    /// умолчанию (MKV); REC гаснет по выходу writer-потока.
    #[test]
    fn yuyv_reencode_records_to_container() {
        use super::{
            CaptureFormat, Frame, FrameData, Record, RecordParams, RecordState, SharedRecordState,
            epoch_millis,
        };
        use crate::config::camera::{CameraConfig, PixelConfig};
        use crate::config::camera::{FrameRateConfig, ResolutionConfig};
        use crossbeam_utils::atomic::AtomicCell;
        use std::sync::Arc;

        let dir = std::env::temp_dir().join(format!("laps-dvr-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state: SharedRecordState = Arc::new(AtomicCell::new(RecordState::Idle));
        let params = RecordParams {
            dir: dir.clone(),
            stem: "test-stem".to_owned(),
            camera: CameraConfig::new("Test", 64, 48, 30, PixelConfig::Yuyv),
        };
        let negotiated = CaptureFormat {
            resolution: ResolutionConfig {
                width: 64,
                height: 48,
            },
            frame_rate: FrameRateConfig(30),
            format: PixelConfig::Yuyv,
        };
        {
            let recorder = Record::start(&params, &negotiated, &state).unwrap();
            for _ in 0..3 {
                let pixels = vec![egui::Color32::BLACK; 64 * 48];
                recorder.push(
                    std::sync::Arc::new(Frame {
                        timestamp: std::time::Instant::now(),
                        data: FrameData::Rgba {
                            rgba: egui::ColorImage::new([64, 48], pixels),
                        },
                    }),
                    epoch_millis(),
                );
            }
        } // drop: writer дописывает файл в фоне

        // Джойна нет по дизайну — ждём гашение REC (writer гасит его по
        // закрытию канала, до finalize/fsync).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !matches!(state.load(), RecordState::Done) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(state.load(), RecordState::Done);

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

    /// Record → mp4/mov → open_reader: контейнеры семейства ISOBMFF
    /// читаются обратно (индекс, кадры побайтово, таймкоды). Кадры —
    /// настоящие JPEG (как из MJPEG-потока), чтобы путь был полным.
    #[test]
    fn records_isobmff_and_reads_back() {
        use super::{
            CaptureFormat, Frame, FrameData, Record, RecordParams, RecordState, SharedRecordState,
            open_reader,
        };
        use crate::config::camera::{CameraConfig, ContainerConfig, PixelConfig};
        use crate::config::camera::{FrameRateConfig, ResolutionConfig};
        use crate::driver::dvr::encode;
        use crossbeam_utils::atomic::AtomicCell;
        use std::sync::Arc;

        for container in [ContainerConfig::Mp4, ContainerConfig::Mov] {
            let dir = std::env::temp_dir().join(format!(
                "laps-dvr-test-{}-{container:?}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let state: SharedRecordState = Arc::new(AtomicCell::new(RecordState::Idle));
            let mut camera = CameraConfig::new("Test", 64, 48, 30, PixelConfig::Mjpeg);
            camera.dvr.container = container;
            let params = RecordParams {
                dir: dir.clone(),
                stem: "test-stem".to_owned(),
                camera,
            };
            let negotiated = CaptureFormat {
                resolution: ResolutionConfig {
                    width: 64,
                    height: 48,
                },
                frame_rate: FrameRateConfig(30),
                format: PixelConfig::Mjpeg,
            };
            // Два разных по содержимому JPEG-кадра: читаемость проверяем
            // побайтово.
            let mut jpegs = Vec::new();
            for color in [egui::Color32::BLACK, egui::Color32::WHITE] {
                let rgba: Vec<u8> = egui::ColorImage::new([64, 48], vec![color; 64 * 48])
                    .pixels
                    .iter()
                    .flat_map(|c| c.to_array())
                    .collect();
                jpegs.push(encode::encode_rgba_to_jpeg(&rgba, 64, 48).unwrap());
            }
            let base = 8_000_000u64;
            {
                let recorder = Record::start(&params, &negotiated, &state).unwrap();
                for i in 0..6u64 {
                    let jpeg = &jpegs[i as usize % 2];
                    recorder.push(
                        std::sync::Arc::new(Frame {
                            timestamp: std::time::Instant::now(),
                            data: FrameData::Jpeg {
                                buf: jpeg.clone(),
                                len: Some(jpeg.len()),
                            },
                        }),
                        base + i * 33,
                    );
                }
            } // drop: writer финализирует в фоне

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !matches!(state.load(), RecordState::Done) && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            assert_eq!(state.load(), RecordState::Done);

            let ext = match container {
                ContainerConfig::Mkv => "mkv",
                ContainerConfig::Mov => "mov",
                ContainerConfig::Mp4 => "mp4",
            };
            let path = std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .find(|p| p.extension().is_some_and(|e| e == ext))
                .unwrap_or_else(|| panic!("no {ext} recording in {dir:?}"));
            let mut reader = open_reader(&path).unwrap();
            assert_eq!(reader.frame_count(), 6);
            assert_eq!(reader.resolution().width, 64);
            let mut buf = vec![0u8; reader.max_frame_len()];
            for i in 0..6u64 {
                let (pts, len) = reader.read_frame_into(i as usize, &mut buf).unwrap();
                assert_eq!(pts, i * 33, "pts кадра {i}");
                assert_eq!(&buf[..len], jpegs[i as usize % 2].as_slice(), "кадр {i}");
            }

            std::fs::remove_dir_all(&dir).ok();
        }
    }

    /// Abort: файл финализируется и удаляется (запись вне гонки).
    #[test]
    fn abort_recording_deletes_file() {
        use super::{
            CaptureFormat, Frame, FrameData, Record, RecordParams, RecordState, SharedRecordState,
        };
        use crate::config::camera::{CameraConfig, PixelConfig};
        use crate::config::camera::{FrameRateConfig, ResolutionConfig};
        use crossbeam_utils::atomic::AtomicCell;
        use std::sync::Arc;
        use crate::driver::dvr::encode;

        let dir = std::env::temp_dir().join(format!("laps-dvr-abort-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state: SharedRecordState = Arc::new(AtomicCell::new(RecordState::Idle));
        let params = RecordParams {
            dir: dir.clone(),
            stem: "abort-stem".to_owned(),
            camera: CameraConfig::new("Test", 64, 48, 30, PixelConfig::Mjpeg),
        };
        let negotiated = CaptureFormat {
            resolution: ResolutionConfig {
                width: 64,
                height: 48,
            },
            frame_rate: FrameRateConfig(30),
            format: PixelConfig::Mjpeg,
        };
        // Стем полный (camera_id уже в нём — см. RecordParams).
        let path = dir.join("abort-stem-mjpeg.mkv");
        {
            let recorder = Record::start(&params, &negotiated, &state).unwrap();
            let rgba: Vec<u8> =
                egui::ColorImage::new([64, 48], vec![egui::Color32::BLACK; 64 * 48])
                    .pixels
                    .iter()
                    .flat_map(|c| c.to_array())
                    .collect();
            let jpeg = encode::encode_rgba_to_jpeg(&rgba, 64, 48).unwrap();
            recorder.push(
                Arc::new(Frame {
                    timestamp: std::time::Instant::now(),
                    data: FrameData::Jpeg {
                        buf: jpeg.clone(),
                        len: Some(jpeg.len()),
                    },
                }),
                1_000,
            );
            recorder.abort();
        } // drop: writer finalize'ит и удаляет файл в фоне

        // Writer detached — поллим исчезновение файла.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            !path.exists(),
            "aborted recording must be deleted: {path:?}"
        );
        assert!(
            std::fs::read_dir(&dir).unwrap().flatten().next().is_none(),
            "aborted recording leaves no files in {dir:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
