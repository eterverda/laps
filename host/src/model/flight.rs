use std::collections::{BTreeMap, HashMap};
use std::mem;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::config::setup::PadConfig;
use crate::model::pilot::Pilot;
use serde_yaml::from_str as from_yaml_string;
use serde_yaml::to_string as to_yaml_string;
use std::fs;
use std::io::Result as IoResult;

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

impl JournalHeader {
    /// Стем файлов записи (видео + журнал) — по date_time старта.
    pub fn file_stem(&self) -> String {
        file_stem_of(self.date_time)
    }
}

/// Журнал полёта: заголовок + события с метками мс, файл yaml-стрима.
/// Метки — мс от момента создания (ноль шкалы — у производителя событий,
/// сервиса; журнал метки только хранит). Файлом журнал не владеет:
/// запись — методы `snapshot`/`append_to`.
pub struct Journal {
    events: Vec<(u64, JournalRecord)>,
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
    pub fn header(&self) -> &JournalHeader {
        &self.header
    }

    /// Итератор событий с метками мс (домен, в котором живёт скраббинг).
    pub fn iter_ms(&self) -> impl Iterator<Item = (u64, &JournalRecord)> {
        self.events.iter().map(|(ms, event)| (*ms, event))
    }

    /// Число событий (счётчик дописанных в файл — см. `append`).
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// Добавить событие (память; на диск уходит через `append`/`snapshot`
    /// — журнал файла не касается).
    pub fn push(&mut self, ms: u64, event: JournalRecord) {
        self.events.push((ms, event));
    }

    /// Удалить события по предикату (правки DVR): правка в памяти, файл
    /// переписывается снапшотом целиком. Первый эвент не удаляется
    /// (держит ноль шкалы).
    #[allow(dead_code)]
    pub fn retain<F: FnMut(&(u64, &JournalRecord)) -> bool>(&mut self, mut pred: F) {
        let mut i = 0;
        self.events.retain(|(ms, event)| {
            i += 1;
            i == 1 || pred(&(*ms, event))
        });
    }

    /// Один эвент — один yaml-документ: `{ms, <variant>: {...}}`.
    /// Документы стрима разделяются `---` (см. `stream`).
    pub fn event_doc(&self, ms: u64, event: &JournalRecord) -> String {
        #[derive(Serialize)]
        struct EventSer<'a> {
            ms: u64,
            #[serde(flatten)]
            event: &'a JournalRecord,
        }
        let ser = EventSer { ms, event };
        to_yaml_string(&ser).unwrap_or_else(|e| panic!("journal yaml: {e}"))
    }

    /// Весь журнал — yaml-стрим: заголовок, затем документ на эвент,
    /// разделители `---`.
    pub fn stream(&self) -> String {
        let mut out =
            to_yaml_string(&self.header).unwrap_or_else(|e| panic!("journal header yaml: {e}"));
        for (ms, event) in &self.events {
            out.push_str("---\n");
            out.push_str(&self.event_doc(*ms, event));
        }
        out
    }

    /// Парсинг yaml-стрима (холодный вход в скраббинг): первый
    /// документ — заголовок, дальше события через `---`. Битые
    /// документы пропускаются с warn.
    pub fn parse_stream(yaml: &str) -> (Option<JournalHeader>, Vec<(u64, JournalRecord)>) {
        #[derive(Deserialize)]
        struct EventDoc {
            ms: u64,
            #[serde(flatten)]
            event: JournalRecord,
        }
        let mut docs = yaml.split("\n---\n").filter(|doc| !doc.trim().is_empty());
        let header = docs
            .next()
            .and_then(|doc| match from_yaml_string::<JournalHeader>(doc) {
                Ok(header) => Some(header),
                Err(e) => {
                    log::warn!("flight journal: bad header: {e}");
                    None
                }
            });
        let events = docs
            .filter_map(|doc| match from_yaml_string::<EventDoc>(doc) {
                Ok(event) => Some((event.ms, event.event)),
                Err(e) => {
                    log::warn!("flight journal: bad document skipped: {e}");
                    None
                }
            })
            .collect();
        (header, events)
    }
    /// Снапшот журнала в `dir/<stem>.yaml` (стем из date_time): полный
    /// стрим через tmp + rename. Первое создание файла и перезапись
    /// после правок — одна операция. Путь — для последующих append_to.
    pub fn snapshot(&self, dir: &Path) -> IoResult<PathBuf> {
        fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.yaml", self.header().file_stem()));
        write_atomic(&path, &self.stream())?;
        Ok(path)
    }

    /// Дописать события, появившиеся после `written`: прочитать файл,
    /// добавить новые документы, tmp + rename. Потерянный/битый файл —
    /// деградация на полный снапшот. `written` — сквозной счётчик
    /// (состояние держит писатель, сервис).
    pub fn append_to(&self, path: &Path, written: &mut usize) -> IoResult<()> {
        let count = self.event_count();
        if *written >= count {
            return Ok(());
        }
        match fs::read_to_string(path) {
            Ok(mut contents) => {
                for (ms, event) in self.iter_ms().skip(*written) {
                    contents.push_str("---\n");
                    contents.push_str(&self.event_doc(ms, event));
                }
                *written = count;
                write_atomic(path, &contents)
            }
            Err(e) => {
                log::warn!("flight journal: append lost file ({e}), full snapshot");
                let dir = path.parent().unwrap_or(Path::new("."));
                *written = self.event_count();
                self.snapshot(dir).map(|_| ())
            }
        }
    }

    /// Дистилят кругов пэда: (метка конца строки, строка, события
    /// круга). Только окно гонки (RaceStarted → RaceFinished);
    /// текущий круг сюда не входит — его докидывает RaceCore.
    /// Успешный круг: последним действием перед Lap не был Fault;
    /// первый успешный — «HS», дальше 1, 2, … (fail номер не съедает).
    pub fn lap_infos(&self, pad_id: &str) -> Vec<(u64, LapLine, Vec<LapEvent>)> {
        let mut lines = Vec::new();
        let mut in_race = false;
        let mut lap_start = 0;
        let mut last_action: Option<LapEvent> = None;
        let mut successful = 0u32;
        let mut lap_events = Vec::new();
        for (ms, event) in self.iter_ms() {
            match event {
                JournalRecord::RaceStarted {} => {
                    in_race = true;
                    lap_start = ms;
                }
                JournalRecord::RaceFinished { .. } => in_race = false,
                JournalRecord::RaceEvent { pad_id: id, name } if in_race && id == pad_id => {
                    lap_events.push(*name);
                    if *name == LapEvent::Lap {
                        let failed = last_action == Some(LapEvent::Fault);
                        let duration = Duration::from_millis(ms - lap_start);
                        let line = if failed {
                            LapLine::Fail { duration }
                        } else {
                            // Имя круга — целиком от дистилятора:
                            // HS без скобки, нумерованные — с («1)»).
                            let num = match successful {
                                0 => "HS".to_owned(),
                                n => format!("{n})"),
                            };
                            successful += 1;
                            LapLine::Success { num, duration }
                        };
                        lines.push((ms, line, mem::take(&mut lap_events)));
                        lap_start = ms;
                        last_action = None;
                    } else {
                        last_action = Some(*name);
                    }
                }
                _ => {}
            }
        }
        lines
    }
}

/// Стем файлов записи (видео + журнал) по date_time старта.
fn file_stem_of(date_time: OffsetDateTime) -> String {
    use time::macros::format_description;
    const FMT: &[time::format_description::FormatItem<'static>] =
        format_description!("[year]-[month]-[day]-[hour]-[minute]-[second].[subsecond digits:3]");
    date_time.format(&FMT).unwrap_or_else(|e| {
        log::error!("flight journal: timestamp format failed: {e}");
        "journal".to_owned()
    })
}

/// Полный контент во временный файл `<stem>.yaml~` рядом, затем
/// rename поверх финального имени (POSIX rename атомарен: читатель
/// видит либо старую, либо новую полную версию). Бесхозный `~` от
/// убитого краша перед записью чистим; latest_journal его игнорирует.
fn write_atomic(path: &Path, contents: &str) -> IoResult<()> {
    let tmp = path.with_file_name(format!("{}~", path.file_name().unwrap().to_string_lossy()));
    let _ = fs::remove_file(&tmp);
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalRecord {
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
    RaceEvent {
        pad_id: String,
        name: LapEvent,
    },
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
impl RaceClip {
    pub fn from_events<'a>(events: impl IntoIterator<Item = (u64, &'a JournalRecord)>) -> RaceClip {
        let mut clip = RaceClip::default();
        for (ms, event) in events {
            match event {
                JournalRecord::RecordStarted { .. } => match &mut clip.recording {
                    Some((lo, _)) => *lo = (*lo).min(ms),
                    None => clip.recording = Some((ms, ms)),
                },
                JournalRecord::RecordFinished {} => match &mut clip.recording {
                    Some((_, hi)) => *hi = ms,
                    None => clip.recording = Some((ms, ms)),
                },
                JournalRecord::VideoAttached {
                    camera_id,
                    filename,
                } => {
                    clip.files.insert(camera_id.clone(), (filename.clone(), ms));
                }
                JournalRecord::RaceCountdown {} => clip.race_countdown_ms = Some(ms),
                JournalRecord::RaceStarted {} => clip.race_started_ms = Some(ms),
                JournalRecord::RaceFinished { .. } => clip.race_finished_ms = Some(ms),
                JournalRecord::RaceEvent { .. } => {}
            }
        }
        clip
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LapLine {
    Success { num: String, duration: Duration },
    Fail { duration: Duration },
    Current { since: Instant },
}

/// Сигнал судьи. В yaml — lower-case (`lap`/`fault`/`correct`).
#[derive(Debug, Eq, PartialEq, Deserialize, Serialize, Clone, Copy)]
pub enum LapEvent {
    #[serde(rename = "fault")]
    Fault,
    #[serde(rename = "correct")]
    Correction,
    Lap,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::camera::ViewportConfig;
    use crate::config::setup::{ColorConfig, FpvConfig};
    use std::env::temp_dir;
    use std::process;

    fn sample_journal() -> Journal {
        let date_time = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let mut pads = BTreeMap::new();
        pads.insert(
            "pad-1".to_owned(),
            PadConfig {
                label: "A1".to_owned(),
                color: ColorConfig::Red,
                fpv: FpvConfig {
                    camera_id: "camera-1".to_owned(),
                    viewport: ViewportConfig::default(),
                },
                keybinds: Vec::new(),
            },
        );
        let mut assignments = HashMap::new();
        assignments.insert("pad-1".to_owned(), Pilot::new("Цой Жив"));
        let mut journal = Journal::new(date_time, pads, assignments);
        journal.push(0, JournalRecord::RecordStarted {});
        journal
    }

    #[test]
    fn file_stem_format() {
        let date_time = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let stem = file_stem_of(date_time);
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
            JournalRecord::VideoAttached {
                camera_id: "camera-1".to_owned(),
                filename: "2026-01-01-00-00-00.000-camera-1-mjpeg.mkv".to_owned(),
            },
        );
        journal.push(1600, JournalRecord::RaceStarted {});
        journal.push(42_000, JournalRecord::RaceFinished { prematurely: true });
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
            JournalRecord::VideoAttached {
                camera_id: "camera-1".to_owned(),
                filename: "2026-01-01-00-00-00.000-camera-1-mjpeg.mkv".to_owned(),
            },
        );
        journal.push(42_000, JournalRecord::RaceFinished { prematurely: false });
        let (header, events) = Journal::parse_stream(&journal.stream());
        let header = header.expect("header roundtrip");
        assert_eq!(
            header.date_time,
            OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
        );
        assert_eq!(events.len(), 3, "{events:?}");
        assert!(matches!(events[0].1, JournalRecord::RecordStarted {}));
        assert_eq!(events[0].0, 0);
        assert_eq!(events[1].0, 1500);
        assert!(matches!(events[1].1, JournalRecord::VideoAttached { .. }));
        assert_eq!(events[2].0, 42000);
        assert_eq!(
            events[2].1,
            JournalRecord::RaceFinished { prematurely: false }
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
        assert!(matches!(events[0].1, JournalRecord::RecordStarted {}));
        assert_eq!(events[1].0, 100);
        assert!(matches!(events[1].1, JournalRecord::RaceStarted {}));
    }

    /// Снапшот: полный стрим в финальном файле, временный `~` после
    /// rename не остаётся; append дописывает новые события.
    #[test]
    fn journal_snapshot_and_append() {
        let dir = temp_dir().join(format!("laps-journal-file-test-{}", process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut journal = sample_journal();
        let path = journal.snapshot(&dir).expect("snapshot written");
        let mut written = journal.event_count();
        journal.push(1500, JournalRecord::RaceStarted {});
        journal.append_to(&path, &mut written).expect("append");

        let mut files: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        assert_eq!(files.len(), 1, "only the final file: {files:?}");
        assert!(files[0].ends_with(".yaml"), "{files:?}");

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, journal.stream(), "file mirrors the stream");

        fs::remove_dir_all(&dir).ok();
    }

    /// Правка журнала (retain): событие удалено из памяти; правка
    /// уходит на диск полным снапшотом.
    #[test]
    fn retain_removes_and_rewrites() {
        let dir = temp_dir().join(format!("laps-journal-retain-test-{}", process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut journal = sample_journal();
        journal.push(100, JournalRecord::RaceStarted {});
        journal.push(200, JournalRecord::RaceFinished { prematurely: true });
        let path = journal.snapshot(&dir).expect("snapshot written");
        journal.retain(|(ms, _)| *ms != 200);
        journal.snapshot(&dir).expect("rewrite after edit");

        let yaml = journal.stream();
        assert!(!yaml.contains("race-finished"), "{yaml}");
        assert!(yaml.contains("race-started"), "{yaml}");
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, yaml, "file mirrors the edited stream");

        fs::remove_dir_all(&dir).ok();
    }

    /// Дистилят кругов: HS у первого успешного, fail не съедает
    /// номер, duration — весь круг (до нажатия Lap, fail в том числе).
    #[test]
    fn lap_infos_distills_success_fail_and_correct() {
        let mut journal = sample_journal();
        journal.push(10_000, JournalRecord::RaceStarted {});
        // Круг 1: lap без fault — HS.
        journal.push(
            25_000,
            JournalRecord::RaceEvent {
                pad_id: "pad-1".to_owned(),
                name: LapEvent::Lap,
            },
        );
        // Круг 2: fault, затем lap без correct — fail, номер не жжёт.
        journal.push(
            30_000,
            JournalRecord::RaceEvent {
                pad_id: "pad-1".to_owned(),
                name: LapEvent::Fault,
            },
        );
        journal.push(
            40_000,
            JournalRecord::RaceEvent {
                pad_id: "pad-1".to_owned(),
                name: LapEvent::Lap,
            },
        );
        // Круг 3: fault → correct → lap — успех «1».
        journal.push(
            45_000,
            JournalRecord::RaceEvent {
                pad_id: "pad-1".to_owned(),
                name: LapEvent::Fault,
            },
        );
        journal.push(
            46_000,
            JournalRecord::RaceEvent {
                pad_id: "pad-1".to_owned(),
                name: LapEvent::Correction,
            },
        );
        journal.push(
            60_000,
            JournalRecord::RaceEvent {
                pad_id: "pad-1".to_owned(),
                name: LapEvent::Lap,
            },
        );
        journal.push(70_000, JournalRecord::RaceFinished { prematurely: false });

        let lines = journal.lap_infos("pad-1");
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0].0, 25_000);
        assert_eq!(
            lines[0].1,
            LapLine::Success {
                num: "HS".to_owned(),
                duration: Duration::from_millis(15_000)
            }
        );
        assert_eq!(
            lines[1].1,
            LapLine::Fail {
                duration: Duration::from_millis(15_000) // весь круг 25→40
            }
        );
        assert_eq!(
            lines[2].1,
            LapLine::Success {
                num: "1)".to_owned(),
                duration: Duration::from_millis(20_000)
            }
        );
        // События круга прикреплены: у fail-круга fault + lap.
        assert_eq!(lines[1].2, vec![LapEvent::Fault, LapEvent::Lap]);
        // Другой пэд не видит чужих событий.
        assert!(journal.lap_infos("pad-2").is_empty());
        // Дистилят ниже уровня гонки: только окно RaceStarted..Finished.
    }

    #[test]
    fn lap_event_serializes_lowercase() {
        let yaml = to_yaml_string(&LapEvent::Correction).unwrap();
        assert!(yaml.contains("correct"), "{yaml}");
        assert!(!yaml.contains("Correction"), "{yaml}");
    }

    #[test]
    fn date_time_getter_matches_constructor() {
        let journal = sample_journal();
        assert_eq!(
            journal.header().date_time,
            OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
        );
    }

    /// Рамка полёта: окно записи (полоска), файлы на нём, границы
    /// гонки (журнальный домен).
    #[test]
    fn race_clip_window_in_file_positions() {
        let events = vec![
            (20_000, JournalRecord::RecordStarted {}),
            (
                20_500,
                JournalRecord::VideoAttached {
                    camera_id: "camera-1".to_owned(),
                    filename: "f-camera-1-mjpeg.mkv".to_owned(),
                },
            ),
            (23_000, JournalRecord::RaceCountdown {}),
            (23_500, JournalRecord::RaceStarted {}),
            (60_000, JournalRecord::RecordFinished {}),
            (68_000, JournalRecord::RaceFinished { prematurely: true }),
        ];
        let clip = RaceClip::from_events(events.iter().map(|(ms, e)| (*ms, e)));
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
