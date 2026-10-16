use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::config::setup::PadConfig;
use crate::model::pilot::Pilot;

/// Журнал полёта: события эфира с метками времени. Первый эвент всегда
/// `JournalStarted` (гарантия конструктора) — его `date_time` авторитетен
/// для имён файлов: видео и yaml-файл журнала носят один стем, см.
/// `file_stem`.
pub struct FlightJournal {
    events: Vec<(Instant, FlightEvent)>,
}

impl FlightJournal {
    pub fn new(
        now: Instant,
        date_time: OffsetDateTime,
        pads: BTreeMap<String, PadConfig>,
        assignments: HashMap<String, Pilot>,
    ) -> Self {
        Self {
            events: vec![(
                now,
                FlightEvent::JournalStarted {
                    date_time,
                    pads,
                    assignments,
                },
            )],
        }
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

    pub fn push(&mut self, at: Instant, event: FlightEvent) {
        self.events.push((at, event));
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

    /// Один эвент — один yaml-документ: `{ms, <variant>: {...}}`, метка —
    /// мс от старта журнала (точность — до мс). Документы стрима
    /// разделяются `---` (см. `to_yaml`).
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
    /// Тот же текст, что инкрементально пишется в файл по мере эвентов.
    /// Продакшн пишет инкрементально (journal_push) — целиком стрим
    /// нужен только тестам.
    #[cfg(test)]
    pub fn to_yaml(&self) -> String {
        let mut out = String::new();
        for (i, (at, event)) in self.events.iter().enumerate() {
            if i > 0 {
                out.push_str("---\n");
            }
            out.push_str(&self.event_doc(*at, event));
        }
        out
    }

    /// Документ первого эвента (JournalStarted; конструктор гарантирует
    /// его наличие). Файл журнала открывается этим документом.
    pub fn first_document(&self) -> String {
        let (at, event) = &self.events[0];
        self.event_doc(*at, event)
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

/// Файл yaml-стрима журнала: создаётся первым документом (JournalStarted),
/// события дописываются по одному (`---` между документами). Каждая
/// мутация — атомарно: полный контент пишется во временный файл
/// `<stem>.yaml~`, затем rename поверх финального имени (POSIX rename
/// атомарен: читатель видит либо старую, либо новую полную версию).
/// Краш посередине оставляет бесхозный `~` — latest_journal его
/// игнорирует (фильтр по расширению `yaml`).
pub struct JournalFile {
    path: std::path::PathBuf,
    /// Накопленный контент стрима (идентичен файлу).
    buf: String,
}

impl JournalFile {
    /// Создать стрим `dir/<stem>.yaml`, первым документом — JournalStarted
    /// переданного журнала. None — писать некуда (каталог/файл недоступны).
    pub fn create(dir: &std::path::Path, journal: &FlightJournal) -> Option<Self> {
        if let Err(e) = std::fs::create_dir_all(dir) {
            log::error!("flight journal: dir unavailable: {e}");
            return None;
        }
        let path = dir.join(format!("{}.yaml", file_stem(journal.date_time())));
        let buf = journal.first_document();
        let file = Self { path, buf };
        if let Err(e) = file.write_atomic() {
            log::error!("flight journal: create failed for {:?}: {e}", file.path);
            return None;
        }
        log::info!("flight journal: stream opened at {:?}", file.path);
        Some(file)
    }

    /// Дописать документ эвента. Ошибка пишется в лог, стрим живёт дальше.
    pub fn append(&mut self, journal: &FlightJournal, at: Instant, event: &FlightEvent) {
        self.buf.push_str("---\n");
        self.buf.push_str(&journal.event_doc(at, event));
        if let Err(e) = self.write_atomic() {
            log::error!("flight journal: append failed for {:?}: {e}", self.path);
        }
    }

    /// Полный контент во временный файл рядом, затем rename поверх.
    fn write_atomic(&self) -> std::io::Result<()> {
        let tmp = self.path.with_file_name(format!(
            "{}~",
            self.path.file_name().unwrap().to_string_lossy()
        ));
        // Бесхозный ~ от убитого предыдущего краша не мешает, но чистим.
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(&tmp, &self.buf)?;
        std::fs::rename(&tmp, &self.path)
    }
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

/// Рамка гонки для скраббинга: какие записи открывать и где в них гонка.
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

    fn sample_journal(start: Instant) -> FlightJournal {
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
        FlightJournal::new(start, date_time, pads, assignments)
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

    #[test]
    fn date_time_getter_matches_first_event() {
        let journal = sample_journal(Instant::now());
        assert_eq!(
            journal.date_time(),
            OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
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
        let parsed = FlightJournal::parse_stream(&journal.to_yaml());
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
        let parsed = FlightJournal::parse_stream(yaml);
        assert_eq!(parsed.len(), 2, "{parsed:?}");
        assert!(matches!(parsed[0].1, FlightEvent::JournalStarted { .. }));
        assert_eq!(parsed[1].0, 100);
        assert!(matches!(parsed[1].1, FlightEvent::RaceStarted {}));
    }

    /// Атомарный стрим: create + append пишут финальный файл целиком,
    /// временный `~` после rename не остаётся.
    #[test]
    fn journal_file_writes_atomically() {
        let dir =
            std::env::temp_dir().join(format!("laps-journal-file-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let start = Instant::now();
        let mut journal = sample_journal(start);
        let mut file = JournalFile::create(&dir, &journal).expect("stream created");
        file.append(
            &journal,
            start + std::time::Duration::from_millis(1500),
            &FlightEvent::RaceStarted {},
        );
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
