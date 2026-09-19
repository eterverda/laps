//! DVR: запись сырого MJPEG-потока камеры на диск без перекодировки.
//! Формат — AVI-MJPEG. Один файл на запуск камеры, финализация
//! при остановке (по Drop).

mod avi;

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use avi::AviWriter;

use super::{RecordState, SharedRecordState};

/// Каталог записей по умолчанию, относительно рабочей директории.
pub const CAPTURES_DIR: &str = "captures";

/// Параметры записи одной камеры. Будет расширяться (ротация, sidecar
/// таймстемпы и т.п.).
pub struct Options {
    pub dir: PathBuf,
    pub camera_id: String,
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
    sender: Option<crossbeam_channel::Sender<(Vec<u8>, Instant)>>,
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
        std::fs::create_dir_all(&options.dir)?;
        let path = options
            .dir
            .join(format!("{}-{}.avi", epoch_secs(), options.camera_id));
        // Файл создаём здесь, а не в writer-потоке: ошибка (диск полон,
        // нет прав) уезжает вызывающему вместо молчаливой мёртвой записи.
        let mut writer = AviWriter::create(&path, width, height, fps, *b"MJPG")?;
        state.store(RecordState { ok: true });
        let writer_path = path.clone();
        let (sender, receiver) = crossbeam_channel::bounded::<(Vec<u8>, Instant)>(CHANNEL_CAP);
        let state_clone = state.clone();
        let thread = std::thread::spawn(move || {
            let mut last_sync = Instant::now();
            // Канал закрывается по Drop отправителя → finalize и выход.
            while let Ok((jpeg, _ts)) = receiver.recv() {
                if let Err(e) = writer.write_frame(&jpeg) {
                    log::error!("dvr: write failed, recording aborted: {}", e);
                    state_clone.store(RecordState { ok: false });
                    return;
                }
                if last_sync.elapsed() >= SYNC_INTERVAL {
                    if let Err(e) = writer.sync_data() {
                        log::error!("dvr: sync failed: {}", e);
                    }
                    last_sync = Instant::now();
                }
            }
            match writer.finalize() {
                Ok(()) => log::info!("dvr: finalized {:?}", writer_path),
                Err(e) => log::error!("dvr: finalize failed for {:?}: {}", writer_path, e),
            }
        });
        log::info!("dvr: recording to {:?}", path);
        Ok(Self {
            state: state.clone(),
            sender: Some(sender),
            thread: Some(thread),
        })
    }

    /// Из capture-потока. Writer мёртв — кадр просто выбрасывается
    /// (состояние уже отражено в RecordState, UI показал). Переполнение
    /// канала = дроп кадра + warn (захват важнее записи).
    pub fn push_frame(&self, jpeg: Vec<u8>, ts: Instant) {
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

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
