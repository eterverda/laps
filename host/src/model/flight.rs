use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::config::setup::PadConfig;
use crate::model::pilot::Pilot;

/// Заголовок журнала: контекст полёта. Первый документ стрима (без
/// метки) — файл читается даже без единого события. date_time
/// авторитетен для имён файлов (стем видео и журнала один).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JournalHeader {
    #[serde(with = "time::serde::rfc3339")]
    pub date_time: OffsetDateTime,
    pub pads: BTreeMap<String, PadConfig>,
    pub assignments: HashMap<String, Pilot>,
}

/// Журнал полёта: заголовок + события с метками мс, файл yaml-стрима.
/// Метки — мс от момента создания (ноль шкалы — у производителя событий,
/// сервиса; журнал метки только хранит). Файл открывается (`open_file`)
/// только при старте гонки; до старта гонки журнал живёт в памяти, без
/// гонки файл не создаётся.
/// Журнал полёта — данные: заголовок + события с метками мс. Метки —
/// мс от момента создания (ноль шкалы — у производителя событий,
/// сервиса; журнал метки только хранит). Файлом журнал не владеет:
/// запись — свободные функции модуля (`snapshot`/`append`).
pub struct Journal {
    events: Vec<(u64, FlightEvent)>,
    /// Контекст полёта (заголовок). date_time — источник стема имён
    /// файлов записи.
    header: JournalHeader,
}

impl Journal {
    pub fn new(
        date_time: OffsetDateTime,
        pads: BTreeMap<String, PadConfig>,
        assignments: HashMap<String, Pilot>,
    ) -> Self {
        Self {
            events: Vec::new(),
            header: JournalHeader {
                date_time,
                pads,
                assignments,
            },
        }
    }

    /// date_time полёта — источник стема имён файлов записи.
    pub fn date_time(&self) -> OffsetDateTime {
        self.header.date_time
    }

    /// Итератор событий с метками мс (домен, в котором живёт скраббинг).
    pub fn iter_ms(&self) -> impl Iterator<Item = (u64, &FlightEvent)> {
        self.events.iter().map(|(ms, event)| (*ms, event))
    }

    /// Число событий (счётчик дописанных в файл — см. `append`).
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// Добавить событие (память; на диск уходит через `append`/`snapshot`
    /// — журнал файла не касается).
    pub fn push(&mut self, ms: u64, event: FlightEvent) {
        self.events.push((ms, event));
    }

    /// Удалить события по предикату (правки DVR): правка в памяти, файл
    /// переписывается снапшотом целиком. Первый эвент не удаляется
    /// (держит ноль шкалы).
    #[allow(dead_code)]
    pub fn retain<F: FnMut(&(u64, &FlightEvent)) -> bool>(&mut self, mut pred: F) {
        let mut i = 0;
        self.events.retain(|(ms, event)| {
            i += 1;
            i == 1 || pred(&(*ms, event))
        });
    }

    /// Один эвент — один yaml-документ: `{ms, <variant>: {...}}`.
    /// Документы стрима разделяются `---` (см. `stream`).
    pub fn event_doc(&self, ms: u64, event: &FlightEvent) -> String {
        #[derive(Serialize)]
        struct EventSer<'a> {
            ms: u64,
            #[serde(flatten)]
            event: &'a FlightEvent,
        }
        let ser = EventSer { ms, event };
        serde_yaml::to_string(&ser).unwrap_or_else(|e| panic!("journal yaml: {e}"))
    }

    /// Весь журнал — yaml-стрим: заголовок, затем документ на эвент,
    /// разделители `---`.
    pub fn stream(&self) -> String {
        let mut out = serde_yaml::to_string(&self.header)
            .unwrap_or_else(|e| panic!("journal header yaml: {e}"));
        for (ms, event) in &self.events {
            out.push_str("---\n");
            out.push_str(&self.event_doc(*ms, event));
        }
        out
    }

    /// Парсинг yaml-стрима (холодный вход в скраббинг): первый
    /// документ — заголовок, дальше события через `---`. Битые
    /// документы пропускаются с warn.
    pub fn parse_stream(yaml: &str) -> (Option<JournalHeader>, Vec<(u64, FlightEvent)>) {
        #[derive(Deserialize)]
        struct EventDoc {
            ms: u64,
            #[serde(flatten)]
            event: FlightEvent,
        }
        let mut docs = yaml.split("\n---\n").filter(|doc| !doc.trim().is_empty());
        let header = docs
            .next()
            .and_then(|doc| match serde_yaml::from_str::<JournalHeader>(doc) {
                Ok(header) => Some(header),
                Err(e) => {
                    log::warn!("flight journal: bad header: {e}");
                    None
                }
            });
        let events = docs
            .filter_map(|doc| match serde_yaml::from_str::<EventDoc>(doc) {
                Ok(event) => Some((event.ms, event.event)),
                Err(e) => {
                    log::warn!("flight journal: bad document skipped: {e}");
                    None
                }
            })
            .collect();
        (header, events)
    }
}

/// Снапшот журнала в `dir/<stem>.yaml` (стем из date_time): полный
/// стрим через tmp + rename. Первое создание файла и перезапись после
/// правок — одна операция. Путь — для последующих `append`.
pub fn snapshot(journal: &Journal, dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.yaml", file_stem(journal.date_time())));
    write_atomic(&path, &journal.stream())?;
    Ok(path)
}

/// Дописать события, появившиеся после `written`: прочитать файл,
/// добавить новые документы, tmp + rename. Потерянный/битый файл —
/// деградация на полный снапшот. `written` — сквозной счётчик
/// (состояние держит писатель, сервис).
pub fn append(journal: &Journal, path: &Path, written: &mut usize) -> std::io::Result<()> {
    let count = journal.event_count();
    if *written >= count {
        return Ok(());
    }
    match std::fs::read_to_string(path) {
        Ok(mut contents) => {
            for (ms, event) in journal.iter_ms().skip(*written) {
                contents.push_str("---\n");
                contents.push_str(&journal.event_doc(ms, event));
            }
            *written = count;
            write_atomic(path, &contents)
        }
        Err(e) => {
            log::warn!("flight journal: append lost file ({e}), full snapshot");
            let dir = path.parent().unwrap_or(Path::new("."));
            *written = journal.event_count();
            snapshot(journal, dir).map(|_| ())
        }
    }
}

/// Полный контент во временный файл `<stem>.yaml~` рядом, затем
/// rename поверх финального имени (POSIX rename атомарен: читатель
/// видит либо старую, либо новую полную версию). Бесхозный `~` от
/// убитого краша перед записью чистим; latest_journal его игнорирует.
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_file_name(format!("{}~", path.file_name().unwrap().to_string_lossy()));
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

/// Стем файлов записи (видео + журнал) по date_time старта.
pub fn file_stem(date_time: OffsetDateTime) -> String {
    use time::macros::format_description;
    const FMT: &[time::format_description::FormatItem<'static>] =
        format_description!("[year]-[month]-[day]-[hour]-[minute]-[second].[subsecond digits:3]");
    date_time.format(&FMT).unwrap_or_else(|e| {
        log::error!("flight journal: timestamp format failed: {e}");
        "journal".to_owned()
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FlightEvent {
    /// Пользователь включил запись (REC, или RACE из эфира без записи).
    /// Без камер: запись — сессия, по её времени строится полоска DVR.
    /// Контекст полёта — в заголовке стрима (JournalHeader).
    RecordStarted {},
    /// Файл записи камеры появился: метка — момент первого кадра
    /// (ноль файла), по ней файл кладётся на полоску записи.
    VideoAttached {
        camera_id: String,
        filename: String,
    },
    /// Пользователь нажал RACE: начался отсчёт (до старта гонки).
    /// Метка — момент нажатия. Курсор DVR встаёт сюда при открытии.
    RaceCountdown {},
    /// Структурный вариант без полей: в yaml пишется `{}` (unit-вариант
    /// сериализовался бы null).
    RaceStarted {},
    RaceFinished {
        prematurely: bool,
    },
    /// Пользователь выключил запись (коммит). Метка — момент закрытия
    /// сессии. Идёт последним: гонка (если была) уже закрыта.
    RecordFinished {},
}

/// Рамка полёта для скраббинга: окно записи, файлы камер на нём,
/// границы гонки. Собирается из событий журнала (прямой путь — из
/// памяти, холодный — из распарсенного файла).
#[derive(Default)]
pub struct RaceClip {
    /// Окно записи (REC on → REC off), журнальный домен: полоска DVR.
    pub recording: Option<(u64, u64)>,
    /// camera_id → (имя файла, journal-ms первого кадра = ноль файла):
    /// файл кладётся на полоску по первому кадру.
    pub files: HashMap<String, (String, u64)>,
    /// Момент нажатия RACE (начало отсчёта): стартовая позиция
    /// курсора DVR и точка на таймлайне.
    pub race_countdown_ms: Option<u64>,
    pub race_started_ms: Option<u64>,
    pub race_finished_ms: Option<u64>,
}

impl RaceClip {
    /// Экстент журнала: последняя известная метка (финиш гонки, конец
    /// записи, последний файл). Запасной домен для таймлайна.
    pub fn end_ms(&self) -> Option<u64> {
        self.race_finished_ms
            .or_else(|| self.recording.map(|(_, hi)| hi))
            .or_else(|| self.files.values().map(|(_, ms)| *ms).max())
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.recording.is_none() && self.files.is_empty()
    }
}

/// Сбор рамки полёта из событий журнала (мс журнала).
pub fn race_clip<'a>(events: impl IntoIterator<Item = (u64, &'a FlightEvent)>) -> RaceClip {
    let mut clip = RaceClip::default();
    for (ms, event) in events {
        match event {
            FlightEvent::RecordStarted { .. } => match &mut clip.recording {
                Some((lo, _)) => *lo = (*lo).min(ms),
                None => clip.recording = Some((ms, ms)),
            },
            FlightEvent::RecordFinished {} => match &mut clip.recording {
                Some((_, hi)) => *hi = ms,
                None => clip.recording = Some((ms, ms)),
            },
            FlightEvent::VideoAttached {
                camera_id,
                filename,
            } => {
                clip.files.insert(camera_id.clone(), (filename.clone(), ms));
            }
            FlightEvent::RaceCountdown {} => clip.race_countdown_ms = Some(ms),
            FlightEvent::RaceStarted {} => clip.race_started_ms = Some(ms),
            FlightEvent::RaceFinished { .. } => clip.race_finished_ms = Some(ms),
        }
    }
    clip
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_journal() -> Journal {
        let date_time = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let mut pads = BTreeMap::new();
        pads.insert(
            "pad-1".to_owned(),
            PadConfig {
                label: "A1".to_owned(),
                color: crate::config::setup::ColorConfig::Red,
                fpv: crate::config::setup::FpvConfig {
                    camera_id: "camera-1".to_owned(),
                    viewport: crate::config::camera::ViewportConfig::default(),
                },
                keybinds: Vec::new(),
            },
        );
        let mut assignments = HashMap::new();
        assignments.insert("pad-1".to_owned(), Pilot::new("Цой Жив"));
        let mut journal = Journal::new(date_time, pads, assignments);
        journal.push(0, FlightEvent::RecordStarted {});
        journal
    }

    #[test]
    fn file_stem_format() {
        let date_time = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let stem = file_stem(date_time);
        assert!(
            lazy_regex::regex_is_match!(r"^\d{4}-\d{2}-\d{2}-\d{2}-\d{2}-\d{2}\.\d{3}$", &stem),
            "bad file stem: {stem}"
        );
    }

    #[test]
    fn yaml_ms_offsets_and_variants() {
        let mut journal = sample_journal();
        journal.push(
            1500,
            FlightEvent::VideoAttached {
                camera_id: "camera-1".to_owned(),
                filename: "2026-01-01-00-00-00.000-camera-1-mjpeg.mkv".to_owned(),
            },
        );
        journal.push(1600, FlightEvent::RaceStarted {});
        journal.push(42_000, FlightEvent::RaceFinished { prematurely: true });
        let yaml = journal.stream();
        assert!(yaml.contains("---\n"), "multi-document stream: {yaml}");
        assert!(yaml.contains("ms: 0"), "{yaml}");
        assert!(yaml.contains("ms: 1500"), "{yaml}");
        assert!(yaml.contains("ms: 42000"), "{yaml}");
        // Заголовок — первый документ, без ms.
        assert!(yaml.starts_with("date_time: 2023-11-14T22:13:20"), "{yaml}");
        assert!(
            yaml.contains("record-started: {}"),
            "empty variant as map: {yaml}"
        );
        assert!(yaml.contains("prematurely: true"), "{yaml}");
        assert!(
            yaml.contains("filename: 2026-01-01-00-00-00.000-camera-1-mjpeg.mkv"),
            "{yaml}"
        );
    }

    /// Roundtrip: сериализованный стрим парсится обратно с теми же
    /// метками и событиями (холодный вход в скраббинг).
    #[test]
    fn yaml_stream_roundtrip() {
        let mut journal = sample_journal();
        journal.push(
            1500,
            FlightEvent::VideoAttached {
                camera_id: "camera-1".to_owned(),
                filename: "2026-01-01-00-00-00.000-camera-1-mjpeg.mkv".to_owned(),
            },
        );
        journal.push(42_000, FlightEvent::RaceFinished { prematurely: false });
        let (header, events) = Journal::parse_stream(&journal.stream());
        let header = header.expect("header roundtrip");
        assert_eq!(
            header.date_time,
            OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
        );
        assert_eq!(events.len(), 3, "{events:?}");
        assert!(matches!(events[0].1, FlightEvent::RecordStarted {}));
        assert_eq!(events[0].0, 0);
        assert_eq!(events[1].0, 1500);
        assert!(matches!(events[1].1, FlightEvent::VideoAttached { .. }));
        assert_eq!(events[2].0, 42000);
        assert_eq!(
            events[2].1,
            FlightEvent::RaceFinished { prematurely: false }
        );
    }

    /// Стрим с битым документом: битое пропускается, остальное живо.
    #[test]
    fn parse_stream_skips_bad_documents() {
        // Битый заголовок — пропускается, события за ним живы.
        let yaml = "garbage: true\n---\nms: 0\nrecord-started: {}\n---\nnot: an: event\n---\nms: 100\nrace-started: {}\n";
        let (header, events) = Journal::parse_stream(yaml);
        assert!(header.is_none(), "{header:?}");
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(matches!(events[0].1, FlightEvent::RecordStarted {}));
        assert_eq!(events[1].0, 100);
        assert!(matches!(events[1].1, FlightEvent::RaceStarted {}));
    }

    /// Снапшот: полный стрим в финальном файле, временный `~` после
    /// rename не остаётся; append дописывает новые события.
    #[test]
    fn journal_snapshot_and_append() {
        let dir =
            std::env::temp_dir().join(format!("laps-journal-file-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut journal = sample_journal();
        let path = snapshot(&journal, &dir).expect("snapshot written");
        let mut written = journal.event_count();
        journal.push(1500, FlightEvent::RaceStarted {});
        append(&journal, &path, &mut written).expect("append");

        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        assert_eq!(files.len(), 1, "only the final file: {files:?}");
        assert!(files[0].ends_with(".yaml"), "{files:?}");

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, journal.stream(), "file mirrors the stream");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Правка журнала (retain): событие удалено из памяти; правка
    /// уходит на диск полным снапшотом.
    #[test]
    fn retain_removes_and_rewrites() {
        let dir =
            std::env::temp_dir().join(format!("laps-journal-retain-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut journal = sample_journal();
        journal.push(100, FlightEvent::RaceStarted {});
        journal.push(200, FlightEvent::RaceFinished { prematurely: true });
        let path = snapshot(&journal, &dir).expect("snapshot written");
        journal.retain(|(ms, _)| *ms != 200);
        snapshot(&journal, &dir).expect("rewrite after edit");

        let yaml = journal.stream();
        assert!(!yaml.contains("race-finished"), "{yaml}");
        assert!(yaml.contains("race-started"), "{yaml}");
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, yaml, "file mirrors the edited stream");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn date_time_getter_matches_constructor() {
        let journal = sample_journal();
        assert_eq!(
            journal.date_time(),
            OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
        );
    }

    /// Рамка полёта: окно записи (полоска), файлы на нём, границы
    /// гонки (журнальный домен).
    #[test]
    fn race_clip_window_in_file_positions() {
        let events = vec![
            (20_000, FlightEvent::RecordStarted {}),
            (
                20_500,
                FlightEvent::VideoAttached {
                    camera_id: "camera-1".to_owned(),
                    filename: "f-camera-1-mjpeg.mkv".to_owned(),
                },
            ),
            (23_000, FlightEvent::RaceCountdown {}),
            (23_500, FlightEvent::RaceStarted {}),
            (60_000, FlightEvent::RecordFinished {}),
            (68_000, FlightEvent::RaceFinished { prematurely: true }),
        ];
        let clip = race_clip(events.iter().map(|(ms, e)| (*ms, e)));
        assert!(!clip.is_empty());
        // Полоска — окно записи; файл лежит на нём со сдвигом.
        assert_eq!(clip.recording, Some((20_000, 60_000)));
        assert_eq!(clip.files.len(), 1);
        assert_eq!(
            clip.files["camera-1"],
            ("f-camera-1-mjpeg.mkv".to_owned(), 20_500)
        );
        // Отсчёт собран: курсор DVR встаёт на него.
        assert_eq!(clip.race_countdown_ms, Some(23_000));
        // Экстент журнала — финиш гонки.
        assert_eq!(clip.end_ms(), Some(68_000));
        // Границы гонки — в журнальном домене (файловые позиции — на
        // стороне плееров: ноль файла = первый кадр).
        assert_eq!(clip.race_started_ms, Some(23_500));
        assert_eq!(clip.race_finished_ms, Some(68_000));
    }
}
