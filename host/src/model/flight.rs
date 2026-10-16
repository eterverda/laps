use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::config::setup::PadConfig;
use crate::model::pilot::Pilot;

/// Журнал полёта: события эфира с метками времени + файл yaml-стрима.
/// Первый эвент всегда `JournalStarted` (гарантия конструктора) — его
/// `date_time` авторитетен для имён файлов (стем видео и журнала один),
/// сам эвент — якорь меток `ms`. Файл открывается (`open_file`) только
/// при старте гонки; до старта гонки журнал живёт в памяти, без гонки
/// файл не создаётся.
pub struct Journal {
    events: Vec<(Instant, FlightEvent)>,
    /// Сериализованный стрим: всегда синхронен с events; файл, если
    /// открыт, — тоже.
    buf: String,
    /// Файл стрима: None — журнал только в памяти.
    path: Option<PathBuf>,
}

impl Journal {
    pub fn new(
        now: Instant,
        date_time: OffsetDateTime,
        pads: BTreeMap<String, PadConfig>,
        assignments: HashMap<String, Pilot>,
    ) -> Self {
        let mut this = Self {
            events: vec![(
                now,
                FlightEvent::JournalStarted {
                    date_time,
                    pads,
                    assignments,
                },
            )],
            buf: String::new(),
            path: None,
        };
        let doc = {
            let (at, event) = &this.events[0];
            this.event_doc(*at, event)
        };
        this.buf = doc;
        this
    }

    /// Журнал из распарсенного стрима (холодный вход в скраббинг):
    /// события с ms-метками разворачиваются от якоря. Первое событие
    /// обязано быть JournalStarted (гарантирует наш формат записи).
    pub fn from_ms_events(anchor: Instant, events: Vec<(u64, FlightEvent)>) -> Option<Self> {
        if events.is_empty() {
            return None;
        }
        let mut journal = Self {
            events: Vec::new(),
            buf: String::new(),
            path: None,
        };
        for (ms, event) in events {
            journal.push(anchor + std::time::Duration::from_millis(ms), event);
        }
        Some(journal)
    }

    /// Момент старта журнала — метка первого эвента.
    fn started(&self) -> Instant {
        self.events[0].0
    }

    /// date_time первого события — источник стема имён файлов записи.
    pub fn date_time(&self) -> OffsetDateTime {
        match &self.events[0].1 {
            FlightEvent::JournalStarted { date_time, .. } => *date_time,
            _ => unreachable!("первый эвент всегда JournalStarted"),
        }
    }

    /// Итератор событий с метками мс от старта журнала (домен,
    /// в котором живёт скраббинг).
    pub fn iter_ms(&self) -> impl Iterator<Item = (u64, &FlightEvent)> {
        self.events.iter().map(|(at, event)| {
            (
                at.saturating_duration_since(self.started()).as_millis() as u64,
                event,
            )
        })
    }

    /// Добавить событие: память + стрим; файл, если открыт, переписан
    /// атомарно (полный контент во временный файл, rename поверх).
    pub fn push(&mut self, at: Instant, event: FlightEvent) {
        self.events.push((at, event));
        let doc = {
            let (at, event) = self.events.last().unwrap();
            self.event_doc(*at, event)
        };
        if self.events.len() > 1 {
            self.buf.push_str("---\n");
        }
        self.buf.push_str(&doc);
        if let Err(e) = self.rewrite() {
            log::error!("flight journal: write failed: {e}");
        }
    }

    /// Открыть файл стрима `dir/<stem>.yaml` текущим содержимым.
    /// false — писать некуда (каталог/файл недоступны).
    pub fn open_file(&mut self, dir: &Path) -> bool {
        if let Err(e) = std::fs::create_dir_all(dir) {
            log::error!("flight journal: dir unavailable: {e}");
            return false;
        }
        self.path = Some(dir.join(format!("{}.yaml", file_stem(self.date_time()))));
        if let Err(e) = self.rewrite() {
            log::error!("flight journal: open failed: {e}");
            self.path = None;
            return false;
        }
        log::info!("flight journal: stream opened at {:?}", self.path);
        true
    }

    /// Удалить события по предикату (правки DVR). Первый эвент — якорь,
    /// не удаляется. Стрим пересобирается, файл переписан атомарно.
    /// Пока без потребителя — правки журнала в DVR следующий этап.
    #[allow(dead_code)]
    pub fn retain<F: FnMut(&(u64, &FlightEvent)) -> bool>(&mut self, mut pred: F) {
        let started = self.started();
        let mut i = 0;
        self.events.retain(|(at, event)| {
            i += 1;
            i == 1
                || pred(&(
                    at.saturating_duration_since(started).as_millis() as u64,
                    event,
                ))
        });
        self.buf = self.stream();
        if let Err(e) = self.rewrite() {
            log::error!("flight journal: rewrite after retain failed: {e}");
        }
    }

    /// Один эвент — один yaml-документ: `{ms, <variant>: {...}}`, метка —
    /// мс от старта журнала (точность — до мс). Документы стрима
    /// разделяются `---` (см. `stream`).
    pub fn event_doc(&self, at: Instant, event: &FlightEvent) -> String {
        #[derive(Serialize)]
        struct EventSer<'a> {
            /// Мс от старта журнала.
            ms: u64,
            #[serde(flatten)]
            event: &'a FlightEvent,
        }
        let ser = EventSer {
            ms: at.saturating_duration_since(self.started()).as_millis() as u64,
            event,
        };
        serde_yaml::to_string(&ser).unwrap_or_else(|e| panic!("journal yaml: {e}"))
    }

    /// Весь журнал — yaml-стрим: документ на эвент, разделители `---`.
    fn stream(&self) -> String {
        let mut out = String::new();
        for (i, (at, event)) in self.events.iter().enumerate() {
            if i > 0 {
                out.push_str("---\n");
            }
            out.push_str(&self.event_doc(*at, event));
        }
        out
    }

    /// Полный контент во временный файл `<stem>.yaml~` рядом, затем
    /// rename поверх финального имени (POSIX rename атомарен: читатель
    /// видит либо старую, либо новую полную версию). Бесхозный `~` от
    /// убитого краша перед записью чистим; latest_journal его игнорирует.
    fn rewrite(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let tmp = path.with_file_name(format!("{}~", path.file_name().unwrap().to_string_lossy()));
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(&tmp, &self.buf)?;
        std::fs::rename(&tmp, path)
    }

    /// Парсинг yaml-стрима (холодный вход в скраббинг): документы через
    /// `---`. Битые документы пропускаются с warn.
    pub fn parse_stream(yaml: &str) -> Vec<(u64, FlightEvent)> {
        #[derive(Deserialize)]
        struct EventDoc {
            ms: u64,
            #[serde(flatten)]
            event: FlightEvent,
        }
        yaml.split("\n---\n")
            .filter(|doc| !doc.trim().is_empty())
            .filter_map(|doc| match serde_yaml::from_str::<EventDoc>(doc) {
                Ok(event) => Some((event.ms, event.event)),
                Err(e) => {
                    log::warn!("flight journal: bad document skipped: {e}");
                    None
                }
            })
            .collect()
    }

    /// Весь стрим (тождественен файлу). Продакшн пишет инкрементально
    /// (push), целиком стрим нужен только тестам.
    #[cfg(test)]
    pub fn to_yaml(&self) -> String {
        self.stream()
    }
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
    JournalStarted {
        #[serde(with = "time::serde::rfc3339")]
        date_time: OffsetDateTime,
        pads: BTreeMap<String, PadConfig>,
        assignments: HashMap<String, Pilot>,
    },
    /// Парный Instant — время первого кадра, записанного на диск
    /// (фактически — добавленного в очередь на запись), из
    /// `Frame::timestamp`.
    RecordingStarted {
        camera_id: String,
        filename: String,
    },
    /// Структурный вариант без полей: в yaml пишется `{}` (unit-вариант
    /// сериализовался бы null).
    RaceStarted {},
    RaceFinished {
        prematurely: bool,
    },
}

/// Рамка гонки для скраббинга: какие записи открывать и границы гонки.
/// Собирается из событий журнала (прямой путь — из памяти, холодный —
/// из распарсенного файла).
#[derive(Default)]
pub struct RaceClip {
    /// camera_id → (имя файла, journal-ms первого кадра = ноль файла).
    pub recordings: HashMap<String, (String, u64)>,
    pub race_started_ms: Option<u64>,
    pub race_finished_ms: Option<u64>,
}

impl RaceClip {
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.recordings.is_empty()
    }
}

/// Сбор рамки гонки из событий журнала (мс журнала).
pub fn race_clip<'a>(events: impl IntoIterator<Item = (u64, &'a FlightEvent)>) -> RaceClip {
    let mut clip = RaceClip::default();
    for (ms, event) in events {
        match event {
            FlightEvent::RecordingStarted {
                camera_id,
                filename,
            } => {
                clip.recordings
                    .insert(camera_id.clone(), (filename.clone(), ms));
            }
            FlightEvent::RaceStarted {} => clip.race_started_ms = Some(ms),
            FlightEvent::RaceFinished { .. } => clip.race_finished_ms = Some(ms),
            FlightEvent::JournalStarted { .. } => {}
        }
    }
    clip
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_journal(start: Instant) -> Journal {
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
            },
        );
        let mut assignments = HashMap::new();
        assignments.insert("pad-1".to_owned(), Pilot::new("Цой Жив"));
        Journal::new(start, date_time, pads, assignments)
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
        let start = Instant::now();
        let mut journal = sample_journal(start);
        journal.push(
            start + std::time::Duration::from_millis(1500),
            FlightEvent::RaceStarted {},
        );
        journal.push(
            start + std::time::Duration::from_millis(1500),
            FlightEvent::RecordingStarted {
                camera_id: "camera-1".to_owned(),
                filename: "2026-01-01-00-00-00.000-camera-1-mjpeg.mkv".to_owned(),
            },
        );
        journal.push(
            start + std::time::Duration::from_millis(42_000),
            FlightEvent::RaceFinished { prematurely: true },
        );
        let yaml = journal.to_yaml();
        assert!(yaml.contains("---\n"), "multi-document stream: {yaml}");
        assert!(yaml.contains("ms: 0"), "{yaml}");
        assert!(yaml.contains("ms: 1500"), "{yaml}");
        assert!(yaml.contains("ms: 42000"), "{yaml}");
        assert!(yaml.contains("journal-started:"), "{yaml}");
        assert!(
            yaml.contains("race-started: {}"),
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
        let start = Instant::now();
        let mut journal = sample_journal(start);
        journal.push(
            start + std::time::Duration::from_millis(1500),
            FlightEvent::RaceStarted {},
        );
        journal.push(
            start + std::time::Duration::from_millis(1500),
            FlightEvent::RecordingStarted {
                camera_id: "camera-1".to_owned(),
                filename: "2026-01-01-00-00-00.000-camera-1-mjpeg.mkv".to_owned(),
            },
        );
        journal.push(
            start + std::time::Duration::from_millis(42_000),
            FlightEvent::RaceFinished { prematurely: false },
        );
        let parsed = Journal::parse_stream(&journal.to_yaml());
        assert_eq!(parsed.len(), 4, "{parsed:?}");
        assert!(matches!(parsed[0].1, FlightEvent::JournalStarted { .. }));
        assert_eq!(parsed[0].0, 0);
        assert_eq!(parsed[1].0, 1500);
        assert!(matches!(parsed[1].1, FlightEvent::RaceStarted {}));
        assert!(matches!(parsed[2].1, FlightEvent::RecordingStarted { .. }));
        assert_eq!(parsed[3].0, 42000);
        assert_eq!(
            parsed[3].1,
            FlightEvent::RaceFinished { prematurely: false }
        );
    }

    /// Стрим с битым документом: битое пропускается, остальное живо.
    #[test]
    fn parse_stream_skips_bad_documents() {
        let yaml = "ms: 0\njournal-started:\n  date_time: 2023-11-14T22:13:20Z\n  pads: {}\n  assignments: {}\n---\nnot: an: event\n---\nms: 100\nrace-started: {}\n";
        let parsed = Journal::parse_stream(yaml);
        assert_eq!(parsed.len(), 2, "{parsed:?}");
        assert!(matches!(parsed[0].1, FlightEvent::JournalStarted { .. }));
        assert_eq!(parsed[1].0, 100);
        assert!(matches!(parsed[1].1, FlightEvent::RaceStarted {}));
    }

    /// Атомарный стрим: open_file + push пишут финальный файл целиком,
    /// временный `~` после rename не остаётся.
    #[test]
    fn journal_file_writes_atomically() {
        let dir =
            std::env::temp_dir().join(format!("laps-journal-file-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let start = Instant::now();
        let mut journal = sample_journal(start);
        assert!(journal.open_file(&dir), "stream created");
        journal.push(
            start + std::time::Duration::from_millis(1500),
            FlightEvent::RaceStarted {},
        );

        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        assert_eq!(files.len(), 1, "only the final file: {files:?}");
        assert!(files[0].ends_with(".yaml"), "{files:?}");

        let content = std::fs::read_to_string(dir.join(&files[0])).unwrap();
        assert_eq!(content, journal.to_yaml(), "file mirrors the stream");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Правка журнала (retain): событие удалено, стрим и файл пересобраны.
    #[test]
    fn retain_removes_and_rewrites() {
        let dir =
            std::env::temp_dir().join(format!("laps-journal-retain-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let start = Instant::now();
        let mut journal = sample_journal(start);
        assert!(journal.open_file(&dir));
        journal.push(
            start + std::time::Duration::from_millis(100),
            FlightEvent::RaceStarted {},
        );
        journal.push(
            start + std::time::Duration::from_millis(200),
            FlightEvent::RaceFinished { prematurely: true },
        );
        journal.retain(|(ms, _)| *ms != 200);

        let yaml = journal.to_yaml();
        assert!(!yaml.contains("race-finished"), "{yaml}");
        assert!(yaml.contains("race-started"), "{yaml}");
        let stem = file_stem(journal.date_time());
        let content = std::fs::read_to_string(dir.join(format!("{stem}.yaml"))).unwrap();
        assert_eq!(content, yaml, "file mirrors the edited stream");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn date_time_getter_matches_first_event() {
        let journal = sample_journal(Instant::now());
        assert_eq!(
            journal.date_time(),
            OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
        );
    }

    /// Рамка гонки: записи, старт/финиш (журнальный домен).
    #[test]
    fn race_clip_window_in_file_positions() {
        let events = vec![
            (
                20_000,
                FlightEvent::RecordingStarted {
                    camera_id: "camera-1".to_owned(),
                    filename: "f-camera-1-mjpeg.mkv".to_owned(),
                },
            ),
            (23_500, FlightEvent::RaceStarted {}),
            (68_000, FlightEvent::RaceFinished { prematurely: true }),
        ];
        let clip = race_clip(events.iter().map(|(ms, e)| (*ms, e)));
        assert!(!clip.is_empty());
        assert_eq!(clip.recordings.len(), 1);
        assert_eq!(
            clip.recordings["camera-1"],
            ("f-camera-1-mjpeg.mkv".to_owned(), 20_000)
        );
        // Границы гонки — в журнальном домене (файловые позиции — на
        // стороне плееров: ноль файла = первый кадр).
        assert_eq!(clip.race_started_ms, Some(23_500));
        assert_eq!(clip.race_finished_ms, Some(68_000));
    }
}
