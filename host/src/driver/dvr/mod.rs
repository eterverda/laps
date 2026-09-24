//! DVR: запись потока камеры на диск. MJPEG — пасsthrough готовых
//! JPEG-блобов без перекодировки. YUYV — декодированный RGBA кодируется
//! в JPEG и пишется в тот же контейнер (AVI/MKV). Один файл на запуск
//! камеры, финализация при остановке (по Drop). Рядом пишется сайдкар
//! `<stem>-frames.yaml`: на каждый кадр — документ `{i, ms}`, где ms —
//! время с предыдущего кадра на входе записи (для первого — с запроса
//! на запись). RecordState целиком принадлежит writer-потоку
//! (старт/окна fps/ошибка/выход) — вторых писателей быть не должно.

mod avi;
mod mkv;

use std::io;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use avi::AviWriter;
use mkv::MkvWriter;

use super::{RecordState, SharedRecordState};
use crate::config::camera::{ContainerConfig, PixelConfig};

/// Закрытое множество контейнеров — enum вместо трейта: новый контейнер
/// добавляется рукой сюда и в Recorder::start, иначе не скомпилируется.
enum VideoWriter {
    Avi(AviWriter),
    Mkv(MkvWriter),
}

impl VideoWriter {
    /// `ts_ms` — момент кадра, мс с Unix-эпохи. AVI его игнорирует
    /// (равномерный таймлайн по fps), MKV пишет как реальный таймкод.
    fn write_frame(&mut self, jpeg: &[u8], ts_ms: u64) -> io::Result<()> {
        match self {
            VideoWriter::Avi(w) => w.write_frame(jpeg),
            VideoWriter::Mkv(w) => w.write_frame(jpeg, ts_ms),
        }
    }

    fn sync_data(&mut self) -> io::Result<()> {
        match self {
            VideoWriter::Avi(w) => w.sync_data(),
            VideoWriter::Mkv(w) => w.sync_data(),
        }
    }

    fn finalize(self) -> io::Result<()> {
        match self {
            VideoWriter::Avi(w) => w.finalize(),
            VideoWriter::Mkv(w) => w.finalize(),
        }
    }
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
    /// Справочно: для шапки сайдкара и логов. Размеры/fps для записи
    /// авторитетны из согласованного формата, см. Recorder::start.
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
    sender: Option<crossbeam_channel::Sender<(Vec<u8>, u64)>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Recorder {
    pub fn start(
        options: &Options,
        width: u32,
        height: u32,
        fps: u32,
        state: &SharedRecordState,
    ) -> io::Result<Self> {
        // Момент запроса на запись: первый кадр сайдкара считаем от него.
        let start_request_ms = epoch_millis();
        std::fs::create_dir_all(&options.dir)?;
        let stem = format!("{}-rec-{}", timestamp_prefix(), options.camera_id);
        // Файл создаём здесь, а не в writer-потоке: ошибка (диск полон,
        // нет прав) уезжает вызывающему вместо молчаливой мёртвой записи.
        let format = options.camera.format;
        let (path, mut writer) = match options.camera.dvr.container {
            ContainerConfig::Avi => {
                let path = options.dir.join(format!("{stem}.avi"));
                let writer = AviWriter::create(&path, width, height, fps, *b"MJPG")?;
                (Some(path), Some(VideoWriter::Avi(writer)))
            }
            ContainerConfig::Mkv => {
                let path = options.dir.join(format!("{stem}.mkv"));
                let writer = MkvWriter::create(&path, width, height)?;
                (Some(path), Some(VideoWriter::Mkv(writer)))
            }
        };
        let frames_path = options.dir.join(format!("{}-frames.yaml", stem));
        let mut frames_file = io::BufWriter::new(std::fs::File::create(&frames_path)?);
        // Шапка — для читающего файл глазами: спека камеры, семантика полей.
        writeln!(frames_file, "# camera: {}", options.camera)?;
        writeln!(
            frames_file,
            "# i — порядковый номер кадра, ms — время с предыдущего кадра в мс (у i=0 — от запроса на запись)"
        )?;
        state.store(RecordState { ok: true, fps: 0.0 });
        let writer_path = path.clone();
        let (sender, receiver) = crossbeam_channel::bounded::<(Vec<u8>, u64)>(CHANNEL_CAP);
        let state_clone = state.clone();
        let thread = std::thread::spawn(move || {
            let mut last_sync = Instant::now();
            // ms кадра — время с предыдущего на входе записи (у первого —
            // с start_request_ms), документ пишется при приходе кадра. i
            // совпадает с порядком в контейнере: сюда доходят только
            // реально записанные кадры.
            let mut frame_no: u64 = 0;
            let mut prev_ts = start_request_ms;
            // Замер пишущихся кадров для UI (окно ~0.5 с): это второй,
            // независимый от capture-потока счётчик — отражает потери
            // в канале.
            let mut fps_frames = 0u32;
            let mut fps_window = Instant::now();
            // Канал закрывается по Drop отправителя → finalize и выход.
            while let Ok((data, ts)) = receiver.recv() {
                if let Some(w) = &mut writer {
                    let frame = match format {
                        PixelConfig::Mjpeg => data,
                        PixelConfig::Yuyv => match encode_rgba_to_jpeg(&data, width, height) {
                            Ok(jpeg) => jpeg,
                            Err(e) => {
                                log::error!("dvr: jpeg encode failed, recording aborted: {}", e);
                                state_clone.store(RecordState {
                                    ok: false,
                                    fps: 0.0,
                                });
                                let _ = writeln!(frames_file, "...");
                                return;
                            }
                        },
                    };
                    if let Err(e) = w.write_frame(&frame, ts) {
                        log::error!("dvr: write failed, recording aborted: {}", e);
                        state_clone.store(RecordState {
                            ok: false,
                            fps: 0.0,
                        });
                        let _ = writeln!(frames_file, "...");
                        return;
                    }
                }
                if let Err(e) = writeln!(
                    frames_file,
                    "--- {{i: {}, ms: {}}}",
                    frame_no,
                    ts.saturating_sub(prev_ts)
                ) {
                    log::error!("dvr: frames sidecar failed, recording aborted: {}", e);
                    state_clone.store(RecordState {
                        ok: false,
                        fps: 0.0,
                    });
                    let _ = writeln!(frames_file, "...");
                    return;
                }
                prev_ts = ts;
                frame_no += 1;
                fps_frames += 1;
                if fps_window.elapsed() >= Duration::from_millis(500) {
                    // Пустое окно (пауза кадров) не затирает последнее
                    // известное значение.
                    if fps_frames > 0 {
                        let fps = fps_frames as f32 / fps_window.elapsed().as_secs_f32();
                        state_clone.store(RecordState { ok: true, fps });
                    }
                    fps_frames = 0;
                    fps_window = Instant::now();
                }
                if last_sync.elapsed() >= SYNC_INTERVAL {
                    if let Some(w) = &mut writer {
                        if let Err(e) = w.sync_data() {
                            log::error!("dvr: sync failed: {}", e);
                        }
                    }
                    last_sync = Instant::now();
                }
            }
            // Канал закрыт: все кадры задокументированы. REC гасим до
            // (медленного) fsync, чтобы индикатор не висел.
            state_clone.store(RecordState {
                ok: false,
                fps: 0.0,
            });
            let _ = writeln!(frames_file, "...");
            if let Err(e) = frames_file.flush() {
                log::error!("dvr: frames sidecar flush failed: {}", e);
            }
            if let Some(w) = writer {
                match w.finalize() {
                    Ok(()) => log::info!("dvr: finalized {:?}", writer_path),
                    Err(e) => log::error!("dvr: finalize failed for {:?}: {}", writer_path, e),
                }
            }
        });
        log::info!("dvr: recording to {:?}", path);
        Ok(Self {
            state: state.clone(),
            sender: Some(sender),
            thread: Some(thread),
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

    /// YUYV-заглушка: видеофайла нет, сайдкар пишется, состояние гаснет
    /// по выходу writer-потока.
    #[test]
    fn yuyv_stub_records_sidecar_only() {
        use crate::config::camera::{CameraConfig, PixelConfig};
        use crate::driver::{RecordState, SharedRecordState};
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
        {
            let recorder = super::Recorder::start(&options, 64, 48, 30, &state).unwrap();
            for _ in 0..3 {
                recorder.push_frame(vec![0u8; 64 * 48 * 4], super::epoch_millis());
            }
        } // drop: writer дописывает сайдкар в фоне

        // Джойна нет по дизайну — ждём финализированный сайдкар.
        let sidecar = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.to_string_lossy().ends_with("-frames.yaml"))
            .expect("no sidecar");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let text = std::fs::read_to_string(&sidecar).unwrap();
            if text.contains("...") {
                assert_eq!(text.matches("--- {i:").count(), 3);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "sidecar not finalized"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // Видеофайл есть (по умолчанию AVI), REC погашен.
        assert!(
            std::fs::read_dir(&dir)
                .unwrap()
                .any(|p| { p.unwrap().path().extension().is_some_and(|e| e == "avi") })
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.load().ok && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(!state.load().ok);

        std::fs::remove_dir_all(&dir).ok();
    }
}
