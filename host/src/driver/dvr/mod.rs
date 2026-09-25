//! DVR: запись потока камеры на диск. MJPEG — пасsthrough готовых
//! JPEG-блобов без перекодировки. YUYV — декодированный RGBA кодируется
//! в JPEG и пишется в тот же контейнер (MKV/MP4/MOV). Один файл на запуск
//! камеры, финализация при остановке (по Drop). RecordState целиком
//! принадлежит writer-потоку (старт/окна fps/ошибка/выход) — вторых
//! писателей быть не должно.

mod mkv;
mod mov;
mod mp4;

use crossbeam_utils::atomic::AtomicCell;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mkv::MkvWriter;
use mov::MovWriter;
use mp4::Mp4Writer;

use crate::config::camera::{ContainerConfig, PixelConfig};

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
pub(crate) trait VideoWriter: Send {
    fn write_frame(&mut self, jpeg: &[u8], ts_ms: u64) -> io::Result<()>;
    fn sync_data(&mut self) -> io::Result<()>;
    /// Потребляет писателя: таблицы/индекс, патчи заголовков, fsync.
    fn finalize(self: Box<Self>) -> io::Result<()>;
}

/// Кодирует RGBA в JPEG. Для YUYV-источников capture-поток уже
/// декодировал кадр в RGBA; здесь он готовится к записи в MJPEG-контейнер.
const JPEG_QUALITY: u8 = 60;

fn encode_rgba_to_jpeg(rgba: &[u8], width: u32, height: u32) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut buf, JPEG_QUALITY);
    encoder
        .encode(
            rgba,
            width as u16,
            height as u16,
            jpeg_encoder::ColorType::Rgba,
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("jpeg encode: {e}")))?;
    Ok(buf)
}

/// Каталог записей по умолчанию, относительно рабочей директории.
pub const CAPTURES_DIR: &str = "captures";

/// Параметры записи одной камеры.
pub struct Options {
    pub dir: PathBuf,
    pub camera_id: String,
    /// Справочно: для логов и отчёта. Размеры для записи авторитетны
    /// из согласованного формата, см. Recorder::start.
    pub camera: crate::config::camera::CameraConfig,
}

/// Отчёт о записи: `{stem}-report.yaml` рядом с видео, пишется при
/// финализации. Три секции: что просили, что открыл драйвер, что реально
/// попало в файл. `actual.frame-rate` — заявление драйвера, сверять
/// с `recorded`.
#[derive(serde::Serialize)]
#[serde(rename_all = "kebab-case")]
struct Report<'a> {
    requested: &'a crate::config::camera::CameraConfig,
    actual: CameraReport<'a>,
    recorded: ReportRecorded,
    system: ReportSystem,
}

/// Реально согласованный формат (nokhwa CameraFormat) для отчёта.
struct CameraReport<'a>(&'a nokhwa::utils::CameraFormat);

impl serde::Serialize for CameraReport<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let fmt = self.0;
        let mut st = s.serialize_struct("CameraReport", 3)?;
        st.serialize_field(
            "resolution",
            &format!("{}x{}", fmt.resolution().width(), fmt.resolution().height()),
        )?;
        st.serialize_field("frame-rate", &format!("{}fps", fmt.frame_rate()))?;
        st.serialize_field("format", &pixel_format_str(fmt.format()))?;
        st.end()
    }
}

fn pixel_format_str(format: nokhwa::utils::FrameFormat) -> String {
    match format {
        nokhwa::utils::FrameFormat::MJPEG => "mjpeg".to_owned(),
        nokhwa::utils::FrameFormat::YUYV => "yuyv".to_owned(),
        other => format!("{other:?}").to_lowercase(),
    }
}

/// Сведения о машине, где шла запись: диагностика fps-просадок
/// часто упирается в железо/хост.
#[derive(serde::Serialize)]
#[serde(rename_all = "kebab-case")]
struct ReportSystem {
    hostname: String,
    os: String,
    cpu: String,
    gpu: String,
}

fn read_first_line(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()?
        .lines()
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn system_info() -> ReportSystem {
    let hostname = read_first_line("/proc/sys/kernel/hostname")
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".into());
    let os = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|c| {
            c.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                .map(|s| s.trim_matches('"').to_owned())
        })
        .unwrap_or_else(|| std::env::consts::OS.to_owned());
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|c| {
            c.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(str::trim)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".into());
    // VGA-контроллер; на машинах без lspci (не-Linux) будет "unknown".
    let gpu = std::process::Command::new("lspci")
        .output()
        .ok()
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .find(|l| {
                    let l = l.to_lowercase();
                    ["vga", "3d controller", "display controller"]
                        .iter()
                        .any(|k| l.contains(k))
                })
                .and_then(|l| l.split(':').nth(2))
                .map(str::trim)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".into());
    ReportSystem {
        hostname,
        os,
        cpu,
        gpu,
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "kebab-case")]
struct ReportRecorded {
    frames_written: u64,
    /// Кадры, не влезшие в канал writer (try_send failed).
    frames_dropped: u64,
    #[serde(with = "humantime_serde")]
    duration: Duration,
    frame_rate_average: crate::config::camera::FrameRateFConfig,
    #[serde(with = "humantime_serde")]
    frame_interval_min: Duration,
    #[serde(with = "humantime_serde")]
    frame_interval_max: Duration,
    #[serde(with = "humantime_serde")]
    frame_interval_p90: Duration,
    /// Время перекодирования RGBA→JPEG; отсутствует для MJPEG-пайплайна
    /// (там перекода нет).
    #[serde(skip_serializing_if = "Option::is_none")]
    jpeg_encode: Option<ReportJpegEncode>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "kebab-case")]
struct ReportJpegEncode {
    frames: u64,
    #[serde(with = "humantime_serde")]
    total: Duration,
    #[serde(with = "humantime_serde")]
    min: Duration,
    #[serde(with = "humantime_serde")]
    max: Duration,
    #[serde(with = "humantime_serde")]
    avg: Duration,
    #[serde(with = "humantime_serde")]
    p90: Duration,
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
    sender: Option<crossbeam_channel::Sender<(Vec<u8>, u64)>>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Дропы в канале (try_send failed), считает push_frame, читает
    /// writer-поток для отчёта.
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Recorder {
    pub fn start(
        options: &Options,
        negotiated: &nokhwa::utils::CameraFormat,
        state: &SharedRecordState,
    ) -> io::Result<Self> {
        std::fs::create_dir_all(&options.dir)?;
        let stem = format!("{}-{}-mjpeg", timestamp_prefix(), options.camera_id);
        // Файл создаём здесь, а не в writer-потоке: ошибка (диск полон,
        // нет прав) уезжает вызывающему вместо молчаливой мёртвой записи.
        let width = negotiated.resolution().width();
        let height = negotiated.resolution().height();
        let format = options.camera.format;
        let report_path = options.dir.join(format!("{stem}-report.yaml"));
        let extension = match options.camera.dvr.container {
            ContainerConfig::Mkv => "mkv",
            ContainerConfig::Mov => "mov",
            ContainerConfig::Mp4 => "mp4",
        };
        let path = options.dir.join(format!("{stem}.{extension}"));
        let mut writer: Box<dyn VideoWriter> = match options.camera.dvr.container {
            ContainerConfig::Mkv => Box::new(MkvWriter::create(&path, width, height)?),
            ContainerConfig::Mov => Box::new(MovWriter::create(&path, width, height)?),
            ContainerConfig::Mp4 => Box::new(Mp4Writer::create(&path, width, height)?),
        };
        state.store(RecordState { ok: true, fps: 0.0 });
        let writer_path = path.clone();
        let (sender, receiver) = crossbeam_channel::bounded::<(Vec<u8>, u64)>(CHANNEL_CAP);
        let state_clone = state.clone();
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let dropped_clone = dropped.clone();
        // В поток уходят owned-копии: ссылки на параметры не 'static.
        let camera_owned = options.camera.clone();
        let negotiated_owned = *negotiated;
        let thread = std::thread::spawn(move || {
            let mut last_sync = Instant::now();
            // Замер пишущихся кадров для UI: второй, независимый от
            // capture-потока счётчик — отражает потери в канале.
            let mut fps_counter = crate::driver::webcam::fps::FpsCounter::default();
            // Статистика для отчёта.
            let mut frames: u64 = 0;
            let mut first_ts: u64 = 0;
            let mut last_ts: u64 = 0;
            let mut min_interval: u64 = u64::MAX;
            let mut max_interval: u64 = 0;
            // Интервалы между кадрами — t-digest для перцентиля в отчёте
            // (приближённо, но память O(1) вне зависимости от длины записи).
            let mut interval_digest = tdigest::TDigest::new_with_size(100);
            let mut enc_frames: u64 = 0;
            let mut enc_total_ms: u64 = 0;
            let mut enc_max_ms: u64 = 0;
            let mut enc_min_ms: u64 = u64::MAX;
            // Времена кодирования — t-digest для p90 в отчёте.
            let mut enc_digest = tdigest::TDigest::new_with_size(100);
            // Канал закрывается по Drop отправителя → finalize и выход.
            while let Ok((data, ts)) = receiver.recv() {
                let frame = match format {
                    PixelConfig::Mjpeg => data,
                    PixelConfig::Yuyv => {
                        let t0 = Instant::now();
                        let jpeg = match encode_rgba_to_jpeg(&data, width, height) {
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
                        enc_frames += 1;
                        let ms = t0.elapsed().as_millis() as u64;
                        enc_total_ms += ms;
                        enc_max_ms = enc_max_ms.max(ms);
                        enc_min_ms = enc_min_ms.min(ms);
                        enc_digest.push(ms as f64);
                        jpeg
                    }
                };
                if let Err(e) = writer.write_frame(&frame, ts) {
                    log::error!("dvr: write failed, recording aborted: {}", e);
                    state_clone.store(RecordState {
                        ok: false,
                        fps: 0.0,
                    });
                    return;
                }
                if frames > 0 {
                    let d = ts.saturating_sub(last_ts);
                    min_interval = min_interval.min(d);
                    max_interval = max_interval.max(d);
                    interval_digest.push(d as f64);
                } else {
                    first_ts = ts;
                }
                last_ts = ts;
                frames += 1;
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
            let duration_ms = if frames > 1 {
                last_ts.saturating_sub(first_ts)
            } else {
                0
            };
            let avg_fps = if duration_ms > 0 {
                round1((frames - 1) as f64 / duration_ms as f64 * 1000.0)
            } else {
                0.0
            };
            enc_digest.flush();
            let jpeg_encode =
                (format == PixelConfig::Yuyv && enc_frames > 0).then(|| ReportJpegEncode {
                    frames: enc_frames,
                    total: Duration::from_millis(enc_total_ms),
                    avg: Duration::from_nanos(enc_total_ms * 1_000_000 / enc_frames),
                    min: Duration::from_millis(enc_min_ms),
                    max: Duration::from_millis(enc_max_ms),
                    p90: Duration::from_millis(
                        enc_digest.estimate_quantile(0.9).unwrap_or(0.0) as u64
                    ),
                });
            // Буферизованные значения нужно слить в центроиды перед
            // оценкой квантиля.
            interval_digest.flush();
            let report = Report {
                requested: &camera_owned,
                actual: CameraReport(&negotiated_owned),
                recorded: ReportRecorded {
                    frames_written: frames,
                    duration: Duration::from_millis(duration_ms),
                    frame_rate_average: crate::config::camera::FrameRateFConfig(avg_fps),
                    frame_interval_min: Duration::from_millis(if frames > 1 {
                        min_interval
                    } else {
                        0
                    }),
                    frame_interval_max: Duration::from_millis(max_interval),
                    frame_interval_p90: Duration::from_millis(
                        interval_digest.estimate_quantile(0.9).unwrap_or(0.0) as u64,
                    ),
                    frames_dropped: dropped_clone.load(std::sync::atomic::Ordering::Relaxed),
                    jpeg_encode,
                },
                system: system_info(),
            };
            // serde_yaml пишет валидные plain-скаляры ("64x48",
            // "30fps") без кавычек.
            match serde_yaml::to_string(&report) {
                Ok(yaml) => {
                    if let Err(e) = std::fs::write(&report_path, yaml) {
                        log::error!("dvr: report write failed for {:?}: {}", report_path, e);
                    }
                }
                Err(e) => log::error!("dvr: report serialize failed: {}", e),
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

    /// Из capture-потока. `ts` — момент поступления кадра на запись, мс
    /// с Unix-эпохи. Writer мёртв — кадр просто выбрасывается
    /// (состояние уже отражено в RecordState, UI показал). Переполнение
    /// канала = дроп кадра + warn (захват важнее записи).
    pub fn push_frame(&self, jpeg: Vec<u8>, ts: u64) {
        if !self.state.load().ok {
            return;
        }
        if let Some(sender) = &self.sender {
            if sender.try_send((jpeg, ts)).is_err() {
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

/// Округление до одного знака (для отчёта).
fn round1(x: f64) -> f32 {
    ((x * 10.0).round() / 10.0) as f32
}

pub(crate) fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Префикс имён файлов записи: `YYYY-mm-dd-HH-MM-SS.SSS` (локальное время).
pub(crate) fn timestamp_prefix() -> String {
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

    #[test]
    fn report_duration_serialization() {
        use super::ReportRecorded;
        use crate::config::camera::FrameRateFConfig;
        let rec = ReportRecorded {
            frames_written: 3,
            frames_dropped: 0,
            duration: std::time::Duration::from_millis(72_500),
            frame_rate_average: FrameRateFConfig(27.59),
            frame_interval_min: std::time::Duration::from_millis(33),
            frame_interval_max: std::time::Duration::from_millis(1_500),
            frame_interval_p90: std::time::Duration::from_millis(40),
            jpeg_encode: None,
        };
        let yaml = serde_yaml::to_string(&rec).unwrap();
        assert!(
            yaml.contains("duration: 1m 12s 500ms"),
            "unexpected duration serialization:\n{yaml}"
        );
        assert!(yaml.contains("frame-interval-min: 33ms"), "\n{yaml}");
        assert!(yaml.contains("frame-interval-max: 1s 500ms"), "\n{yaml}");
        assert!(yaml.contains("frame-interval-p90: 40ms"), "\n{yaml}");
    }

    #[test]
    fn interval_digest_p90() {
        // Малый ряд: оценка интерполирует между центроидами, но должна
        // остаться внутри диапазона и далеко от выбросов.
        let mut d = tdigest::TDigest::new_with_size(100);
        for _ in 0..20 {
            d.push(33.0);
        }
        d.push(500.0);
        d.push(900.0);
        d.flush();
        let p90 = d.estimate_quantile(0.9).unwrap();
        assert!((33.0..500.0).contains(&p90), "p90 = {p90}");
        // Большой ряд: p90 должен сойтись к 33ms, а не к выбросам.
        let mut big = tdigest::TDigest::new_with_size(100);
        for _ in 0..10_000 {
            big.push(33.0);
        }
        for v in [500.0, 900.0].iter().cycle().take(200) {
            big.push(*v);
        }
        big.flush();
        let p90 = big.estimate_quantile(0.9).unwrap();
        assert!((30.0..40.0).contains(&p90), "p90 = {p90}");
        // Пустой дайджест — None, отчёт пишет 0ms.
        assert!(
            tdigest::TDigest::new_with_size(100)
                .estimate_quantile(0.9)
                .is_none()
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
        let options = super::Options {
            dir: dir.clone(),
            camera_id: "camera-1".to_owned(),
            camera: CameraConfig::new("Test", 64, 48, 30, PixelConfig::Yuyv),
        };
        let negotiated =
            nokhwa::utils::CameraFormat::new_from(64, 48, nokhwa::utils::FrameFormat::YUYV, 30);
        {
            let recorder = super::Recorder::start(&options, &negotiated, &state).unwrap();
            for _ in 0..3 {
                recorder.push_frame(vec![0u8; 64 * 48 * 4], super::epoch_millis());
            }
        } // drop: writer дописывает файл в фоне

        // Джойна нет по дизайну — ждём гашение REC (writer гасит его по
        // закрытию канала, до finalize/fsync).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.load().ok && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(!state.load().ok);

        // REC погас до finalize — ждём отчёт: он пишется самым последним
        // действием writer-потока.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let report_path = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.to_string_lossy().ends_with("-report.yaml"));
        let mut report_path = report_path;
        while report_path.is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
            report_path = std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| p.to_string_lossy().ends_with("-report.yaml"));
        }

        // Видеофайл есть (контейнер по умолчанию — MKV), рядом отчёт,
        // других файлов нет.
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
        let report_path = report_path.expect("no report written");
        let yaml = std::fs::read_to_string(&report_path).unwrap();
        assert!(yaml.contains("requested:"), "report: {yaml}");
        assert!(yaml.contains("actual:"), "report: {yaml}");
        assert!(yaml.contains("recorded:"), "report: {yaml}");
        assert!(yaml.contains("system:"), "report: {yaml}");
        assert!(yaml.contains("jpeg-encode:"), "report: {yaml}");
        assert!(
            entries
                .iter()
                .all(|p| { p.extension().is_some_and(|e| e == "mkv") || p == &report_path }),
            "unexpected files: {entries:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
