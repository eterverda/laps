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

/// Метка на оси журнала: момент от нуля шкалы (REC). Тип непрозрачный:
/// конструируется только журналом, числового значения наружу нет —
/// только арифметика методами. Арифметика меток РАЗНЫХ журналов лишена
/// смысла: оси независимы, сравнения/разности между ними — мусор
/// (документация, не проверка). Display нет намеренно: метка не для
/// вывода, yaml ходит в мс-оффсетах. Ноль оси наружу не торчит
/// константой — только через `Journal::record_started()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Stamp(u64);

impl Stamp {
    /// Ноль оси (момент создания журнала, REC). Внутреннее дело
    /// журнала; снаружи ось берётся через `Journal::record_started()`.
    const ZERO: Stamp = Stamp(0);

    /// self − earlier, в мс, знаково: отрицательно — self раньше
    /// earlier. Единственный способ получить число из меток.
    pub fn ms_since(self, earlier: Stamp) -> i64 {
        self.0 as i64 - earlier.0 as i64
    }

    /// Сдвиг по оси на знаковые мс (кламп в [0, u64::MAX]).
    pub fn plus_ms(self, ms: i64) -> Stamp {
        Stamp(if ms >= 0 {
            self.0.saturating_add(ms as u64)
        } else {
            self.0.saturating_sub(ms.unsigned_abs())
        })
    }
}

/// Интервал на оси журнала [from, to): полуинтервал — from входит,
/// to нет. Метки одного журнала. Тупая пара: арифметика — методами
/// Stamp, тут — только то, что нужно интервалу.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interval {
    pub from: Stamp,
    pub to: Stamp,
}

impl Interval {
    pub fn new(from: Stamp, to: Stamp) -> Self {
        Self { from, to }
    }

    /// Длительность интервала, мс (0 — вырожденный или инвертированный).
    pub fn span_ms(&self) -> u64 {
        self.to.ms_since(self.from).max(0) as u64
    }

    /// Метка, клампнутная в интервал (вырожденный клампит к from).
    pub fn clamp(&self, at: Stamp) -> Stamp {
        at.clamp(self.from, self.to.max(self.from))
    }
}

/// Журнал полёта: заголовок + события с метками, файл yaml-стрима.
/// Ось журнала — мс от момента создания (REC); журнал — хозяин оси:
/// метки рождаются только у него (сдвиг от `record_started`), сырой мс
/// в домен не входит и не выходит. Файлом журнал не владеет: запись —
/// методы `snapshot`/`append_to`.
pub struct Journal {
    entries: Vec<(Stamp, JournalEntry)>,
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
            entries: Vec::new(),
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

    /// Начало записи (REC) — ноль оси журнала. Точка, от которой
    /// снаружи считаются все метки: `journal.record_started().plus_ms(x)`.
    /// Константой ноль наружу не торчит — только через этот метод.
    pub fn record_started(&self) -> Stamp {
        Stamp::ZERO
    }

    /// Начало вылета (первое RaceStarted), если гонка была.
    pub fn race_started(&self) -> Option<Stamp> {
        self.entries
            .iter()
            .find_map(|(at, entry)| matches!(entry, JournalEntry::RaceStarted {}).then_some(*at))
    }

    /// Итератор событий с метками (домен, в котором живёт скраббинг).
    pub fn iter(&self) -> impl Iterator<Item = (Stamp, &JournalEntry)> {
        self.entries.iter().map(|(ms, entry)| (*ms, entry))
    }

    /// Число событий (счётчик дописанных в файл — см. `append`).
    pub fn event_count(&self) -> usize {
        self.entries.len()
    }

    /// Добавить событие по метке (память; на диск уходит через
    /// `append`/`snapshot` — журнал файла не касается). Вставка по
    /// времени: позиция — бинарным поиском; среди равных меток — после
    /// равных (порядок вставки сохраняется). Вставка в конец (обычный
    /// путь хронологической записи) — без сдвига.
    pub fn push(&mut self, at: Stamp, entry: JournalEntry) {
        let pos = self.entries.partition_point(|(ms, _)| *ms <= at);
        self.entries.insert(pos, (at, entry));
    }

    /// Удалить события пада в полуинтервале меток [from, to) (правки
    /// DVR). Возвращает число удалённых.
    pub fn remove(&mut self, pad_id: &str, from: Stamp, to: Stamp) -> usize {
        let before = self.event_count();
        self.retain(|(ms, entry)| {
            !(*ms >= from
                && *ms < to
                && matches!(entry, JournalEntry::RaceEvent { pad_id: id, .. } if *id == pad_id))
        });
        before - self.event_count()
    }

    /// Удалить события по предикату (правки DVR): правка в памяти, файл
    /// переписывается снапшотом целиком. Первый эвент не удаляется
    /// (держит ноль шкалы).
    pub fn retain<F: FnMut(&(Stamp, &JournalEntry)) -> bool>(&mut self, mut pred: F) {
        let mut i = 0;
        self.entries.retain(|(ms, entry)| {
            i += 1;
            i == 1 || pred(&(*ms, entry))
        });
    }

    /// Один эвент — один yaml-документ: `{ms, <variant>: {...}}`.
    /// Документы стрима разделяются `---` (см. `stream`). Метка
    /// сериализуется мс-оффсетом (формат файла — оффсеты).
    pub fn entry_doc(&self, at: Stamp, entry: &JournalEntry) -> String {
        #[derive(Serialize)]
        struct EventSer<'a> {
            ms: u64,
            #[serde(flatten)]
            entry: &'a JournalEntry,
        }
        let ser = EventSer { ms: at.0, entry };
        to_yaml_string(&ser).unwrap_or_else(|e| panic!("journal yaml: {e}"))
    }

    /// Весь журнал — yaml-стрим: заголовок, затем документ на эвент,
    /// разделители `---`.
    pub fn stream(&self) -> String {
        let mut out =
            to_yaml_string(&self.header).unwrap_or_else(|e| panic!("journal header yaml: {e}"));
        for (ms, entry) in &self.entries {
            out.push_str("---\n");
            out.push_str(&self.entry_doc(*ms, entry));
        }
        out
    }

    /// Парсинг yaml-стрима (холодный вход в скраббинг): первый
    /// документ — заголовок, дальше события через `---`. Битые
    /// документы пропускаются с warn.
    pub fn parse_stream(yaml: &str) -> (Option<JournalHeader>, Vec<(Stamp, JournalEntry)>) {
        #[derive(Deserialize)]
        struct EventDoc {
            ms: u64,
            #[serde(flatten)]
            event: JournalEntry,
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
        let entries = docs
            .filter_map(|doc| match from_yaml_string::<EventDoc>(doc) {
                Ok(event) => Some((Stamp(event.ms), event.event)),
                Err(e) => {
                    log::warn!("flight journal: bad document skipped: {e}");
                    None
                }
            })
            .collect();
        (header, entries)
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
                for (ms, entry) in self.iter().skip(*written) {
                    contents.push_str("---\n");
                    contents.push_str(&self.entry_doc(ms, entry));
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
    /// круга). Только окно гонки (RaceStarted → RaceFinished); текущий
    /// круг в эфире сюда не входит — его докидывает RaceCore, а после
    /// RaceFinished выдаётся здесь же незакрытый круг (`Unfinished`).
    /// Успешный круг: последним действием перед Lap не был Fault;
    /// первый успешный — «HS», дальше 1, 2, … (fail номер не съедает).
    pub fn lap_infos(&self, pad_id: &str) -> Vec<(Stamp, LapLine, Vec<(Stamp, LapEvent)>)> {
        let mut lines = Vec::new();
        let mut in_race = false;
        let mut lap_start = Stamp::ZERO;
        let mut last_action: Option<LapEvent> = None;
        let mut successful = 0u32;
        let mut lap_events = Vec::new();
        let mut race_finished = None;
        for (ms, entry) in self.iter() {
            match entry {
                JournalEntry::RaceStarted {} => {
                    in_race = true;
                    lap_start = ms;
                }
                JournalEntry::RaceFinished { .. } => {
                    in_race = false;
                    race_finished = Some(ms);
                }
                JournalEntry::RaceEvent { pad_id: id, name } if in_race && id == pad_id => {
                    lap_events.push((ms, *name));
                    if *name == LapEvent::Lap {
                        let failed = last_action == Some(LapEvent::Fault);
                        let duration = Duration::from_millis(ms.ms_since(lap_start).max(0) as u64);
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
                    } else if *name != LapEvent::Warn {
                        // Warn расчётно нейтрален: в события круга
                        // попал выше, на fail/success не влияет (fault
                        // перед ним не снимает).
                        last_action = Some(*name);
                    }
                }
                _ => {}
            }
        }
        // Незакрытый круг гонки (пост-полётный «текущий»): после
        // RaceFinished остаток событий не пристроен ни одному кругу.
        // Строка выдаётся всегда — с застывшим временем (от начала
        // круга до финиша), как Current в эфире показывается у каждого
        // пада без единого события.
        if let Some(finish) = race_finished {
            lines.push((
                finish,
                LapLine::Unfinished {
                    duration: Duration::from_millis(finish.ms_since(lap_start).max(0) as u64),
                },
                mem::take(&mut lap_events),
            ));
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
pub enum JournalEntry {
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
    pub recording: Option<Interval>,
    /// camera_id → (имя файла, метка первого кадра = ноль файла):
    /// файл кладётся на полоску по первому кадру.
    pub files: HashMap<String, (String, Stamp)>,
    /// Момент нажатия RACE (начало отсчёта): стартовая позиция
    /// курсора DVR и точка на таймлайне.
    pub race_countdown: Option<Stamp>,
    pub race_started: Option<Stamp>,
    pub race_finished: Option<Stamp>,
}

impl RaceClip {
    /// Экстент журнала: последняя известная метка (финиш гонки, конец
    /// записи, последний файл). Запасной домен для таймлайна.
    pub fn end(&self) -> Option<Stamp> {
        self.race_finished
            .or_else(|| self.recording.map(|w| w.to))
            .or_else(|| self.files.values().map(|(_, ms)| *ms).max())
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.recording.is_none() && self.files.is_empty()
    }
}

/// Сбор рамки полёта из событий журнала.
impl RaceClip {
    pub fn from_events<'a>(
        entries: impl IntoIterator<Item = (Stamp, &'a JournalEntry)>,
    ) -> RaceClip {
        let mut clip = RaceClip::default();
        for (ms, entry) in entries {
            match entry {
                JournalEntry::RecordStarted { .. } => match &mut clip.recording {
                    Some(w) => w.from = w.from.min(ms),
                    None => clip.recording = Some(Interval::new(ms, ms)),
                },
                JournalEntry::RecordFinished {} => match &mut clip.recording {
                    Some(w) => w.to = ms,
                    None => clip.recording = Some(Interval::new(ms, ms)),
                },
                JournalEntry::VideoAttached {
                    camera_id,
                    filename,
                } => {
                    clip.files.insert(camera_id.clone(), (filename.clone(), ms));
                }
                JournalEntry::RaceCountdown {} => clip.race_countdown = Some(ms),
                JournalEntry::RaceStarted {} => clip.race_started = Some(ms),
                JournalEntry::RaceFinished { .. } => clip.race_finished = Some(ms),
                JournalEntry::RaceEvent { .. } => {}
            }
        }
        clip
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LapLine {
    Success {
        num: String,
        duration: Duration,
    },
    Fail {
        duration: Duration,
    },
    Current {
        since: Instant,
    },
    /// Пост-полётный незакрытый круг (живой «текущий» после
    /// RaceFinished): время застывшее — от начала круга до финиша.
    /// Рендерится как Current (серый), но без имени — колонка
    /// резервируется пустой.
    Unfinished {
        duration: Duration,
    },
}

impl LapLine {
    /// Длительность круга: зафиксированная у завершённых и у
    /// незакрытого пост-полётного, бегущая у текущего (эфир).
    pub fn duration(&self) -> Duration {
        match self {
            LapLine::Success { duration, .. }
            | LapLine::Fail { duration }
            | LapLine::Unfinished { duration } => *duration,
            LapLine::Current { since } => since.elapsed(),
        }
    }
}

/// Сигнал судьи. В yaml — lower-case (`lap`/`fault`/`correct`/`warn`).
/// Warn расчётно нейтрален: отображается с меткой, на fail/success
/// круга не влияет.
#[derive(Debug, Eq, PartialEq, Deserialize, Serialize, Clone, Copy)]
pub enum LapEvent {
    #[serde(rename = "fault")]
    Fault,
    #[serde(rename = "correct")]
    Correction,
    /// alias — старые журналы (до rename сериализовали `Lap`).
    #[serde(rename = "lap", alias = "Lap")]
    Lap,
    #[serde(rename = "warn")]
    Warn,
}

impl LapEvent {
    /// Имя action/yaml варианта — единый источник: сериализация
    /// (serde-rename'ы выше).
    pub fn action(&self) -> String {
        serde_yaml::to_string(self)
            .map(|s| s.trim().to_owned())
            .unwrap_or_default()
    }

    /// Action биндинга → сигнал. Неизвестное действие — None.
    pub fn from_action(action: &str) -> Option<Self> {
        [Self::Lap, Self::Fault, Self::Correction, Self::Warn]
            .into_iter()
            .find(|e| e.action() == action)
    }
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
        journal.push(Stamp(0), JournalEntry::RecordStarted {});
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
            Stamp(1500),
            JournalEntry::VideoAttached {
                camera_id: "camera-1".to_owned(),
                filename: "2026-01-01-00-00-00.000-camera-1-mjpeg.mkv".to_owned(),
            },
        );
        journal.push(Stamp(1600), JournalEntry::RaceStarted {});
        journal.push(
            Stamp(42_000),
            JournalEntry::RaceFinished { prematurely: true },
        );
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
            Stamp(1500),
            JournalEntry::VideoAttached {
                camera_id: "camera-1".to_owned(),
                filename: "2026-01-01-00-00-00.000-camera-1-mjpeg.mkv".to_owned(),
            },
        );
        journal.push(
            Stamp(42_000),
            JournalEntry::RaceFinished { prematurely: false },
        );
        let (header, entries) = Journal::parse_stream(&journal.stream());
        let header = header.expect("header roundtrip");
        assert_eq!(
            header.date_time,
            OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
        );
        assert_eq!(entries.len(), 3, "{entries:?}");
        assert!(matches!(entries[0].1, JournalEntry::RecordStarted {}));
        assert_eq!(entries[0].0, Stamp::ZERO);
        assert_eq!(entries[1].0, Stamp(1500));
        assert!(matches!(entries[1].1, JournalEntry::VideoAttached { .. }));
        assert_eq!(entries[2].0, Stamp(42_000));
        assert_eq!(
            entries[2].1,
            JournalEntry::RaceFinished { prematurely: false }
        );
    }

    /// Стрим с битым документом: битое пропускается, остальное живо.
    #[test]
    fn parse_stream_skips_bad_documents() {
        // Битый заголовок — пропускается, события за ним живы.
        let yaml = "garbage: true\n---\nms: 0\nrecord-started: {}\n---\nnot: an: event\n---\nms: 100\nrace-started: {}\n";
        let (header, entries) = Journal::parse_stream(yaml);
        assert!(header.is_none(), "{header:?}");
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert!(matches!(entries[0].1, JournalEntry::RecordStarted {}));
        assert_eq!(entries[1].0, Stamp(100));
        assert!(matches!(entries[1].1, JournalEntry::RaceStarted {}));
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
        journal.push(Stamp(1500), JournalEntry::RaceStarted {});
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
        journal.push(Stamp(100), JournalEntry::RaceStarted {});
        journal.push(Stamp(200), JournalEntry::RaceFinished { prematurely: true });
        let path = journal.snapshot(&dir).expect("snapshot written");
        journal.retain(|(ms, _)| *ms != Stamp(200));
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
        journal.push(Stamp(10_000), JournalEntry::RaceStarted {});
        // Круг 1: lap без fault — HS.
        journal.push(
            Stamp(25_000),
            JournalEntry::RaceEvent {
                pad_id: "pad-1".to_owned(),
                name: LapEvent::Lap,
            },
        );
        // Круг 2: fault, затем lap без correct — fail, номер не жжёт.
        journal.push(
            Stamp(30_000),
            JournalEntry::RaceEvent {
                pad_id: "pad-1".to_owned(),
                name: LapEvent::Fault,
            },
        );
        journal.push(
            Stamp(40_000),
            JournalEntry::RaceEvent {
                pad_id: "pad-1".to_owned(),
                name: LapEvent::Lap,
            },
        );
        // Круг 3: fault → correct → lap — успех «1».
        journal.push(
            Stamp(45_000),
            JournalEntry::RaceEvent {
                pad_id: "pad-1".to_owned(),
                name: LapEvent::Fault,
            },
        );
        journal.push(
            Stamp(46_000),
            JournalEntry::RaceEvent {
                pad_id: "pad-1".to_owned(),
                name: LapEvent::Correction,
            },
        );
        journal.push(
            Stamp(60_000),
            JournalEntry::RaceEvent {
                pad_id: "pad-1".to_owned(),
                name: LapEvent::Lap,
            },
        );
        journal.push(
            Stamp(70_000),
            JournalEntry::RaceFinished { prematurely: false },
        );

        let lines = journal.lap_infos("pad-1");
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert_eq!(lines[0].0, Stamp(25_000));
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
        // События круга прикреплены (с метками): у fail-круга fault + lap.
        assert_eq!(
            lines[1].2,
            vec![
                (Stamp(30_000), LapEvent::Fault),
                (Stamp(40_000), LapEvent::Lap)
            ]
        );
        // Незакрытый хвост после финиша: застывшее время, событий нет.
        assert_eq!(lines[3].0, Stamp(70_000));
        assert_eq!(
            lines[3].1,
            LapLine::Unfinished {
                duration: Duration::from_millis(10_000) // 60→70
            }
        );
        assert!(lines[3].2.is_empty());
        // Пэд без единого события видит только незакрытый круг гонки.
        let pad2 = journal.lap_infos("pad-2");
        assert_eq!(pad2.len(), 1, "{pad2:?}");
        assert_eq!(
            pad2[0].1,
            LapLine::Unfinished {
                duration: Duration::from_millis(60_000) // 10→70
            }
        );
        // Дистилят ниже уровня гонки: только окно RaceStarted..Finished.
    }

    /// Warn: прикрепляется к событию круга с меткой, но на расчёт не
    /// влияет — fault перед ним не снимает, номер не жжёт.
    #[test]
    fn lap_infos_warn_is_display_only() {
        let mut journal = sample_journal();
        journal.push(Stamp(10_000), JournalEntry::RaceStarted {});
        // warn до и после fault: круг всё равно fail.
        journal.push(
            Stamp(20_000),
            JournalEntry::RaceEvent {
                pad_id: "pad-1".to_owned(),
                name: LapEvent::Warn,
            },
        );
        journal.push(
            Stamp(30_000),
            JournalEntry::RaceEvent {
                pad_id: "pad-1".to_owned(),
                name: LapEvent::Fault,
            },
        );
        journal.push(
            Stamp(35_000),
            JournalEntry::RaceEvent {
                pad_id: "pad-1".to_owned(),
                name: LapEvent::Warn,
            },
        );
        journal.push(
            Stamp(40_000),
            JournalEntry::RaceEvent {
                pad_id: "pad-1".to_owned(),
                name: LapEvent::Lap,
            },
        );
        journal.push(
            Stamp(50_000),
            JournalEntry::RaceFinished { prematurely: false },
        );

        let lines = journal.lap_infos("pad-1");
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(matches!(lines[0].1, LapLine::Fail { .. }), "{lines:?}");
        // Оба warn — в событиях круга, с метками.
        assert_eq!(
            lines[0].2,
            vec![
                (Stamp(20_000), LapEvent::Warn),
                (Stamp(30_000), LapEvent::Fault),
                (Stamp(35_000), LapEvent::Warn),
                (Stamp(40_000), LapEvent::Lap),
            ]
        );
        // Незакрытый хвост (40→50) без событий — всё равно строка.
        assert_eq!(lines[1].0, Stamp(50_000));
        assert_eq!(
            lines[1].1,
            LapLine::Unfinished {
                duration: Duration::from_millis(10_000)
            }
        );
        assert!(lines[1].2.is_empty());
    }

    /// Незакрытый круг после RaceFinished: строка с застывшим
    /// временем (от начала круга до финиша) и событиями круга.
    /// Без событий после последнего завершённого круга строки нет.
    #[test]
    fn lap_infos_appends_unfinished_lap_after_finish() {
        let mut journal = sample_journal();
        journal.push(Stamp(10_000), JournalEntry::RaceStarted {});
        journal.push(
            Stamp(25_000),
            JournalEntry::RaceEvent {
                pad_id: "pad-1".to_owned(),
                name: LapEvent::Lap,
            },
        );
        // Незакрытый круг: fault + warn, затем финиш.
        journal.push(
            Stamp(30_000),
            JournalEntry::RaceEvent {
                pad_id: "pad-1".to_owned(),
                name: LapEvent::Fault,
            },
        );
        journal.push(
            Stamp(32_000),
            JournalEntry::RaceEvent {
                pad_id: "pad-1".to_owned(),
                name: LapEvent::Warn,
            },
        );
        journal.push(
            Stamp(40_000),
            JournalEntry::RaceFinished { prematurely: false },
        );

        let lines = journal.lap_infos("pad-1");
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[1].0, Stamp(40_000));
        assert_eq!(
            lines[1].1,
            LapLine::Unfinished {
                duration: Duration::from_millis(15_000) // 25→40
            }
        );
        assert_eq!(
            lines[1].2,
            vec![
                (Stamp(30_000), LapEvent::Fault),
                (Stamp(32_000), LapEvent::Warn)
            ]
        );
    }

    /// from_action читает имена вариантов — та же таблица, что yaml.
    #[test]
    fn lap_event_from_action_matches_yaml_names() {
        for (action, event) in [
            ("lap", LapEvent::Lap),
            ("fault", LapEvent::Fault),
            ("correct", LapEvent::Correction),
            ("warn", LapEvent::Warn),
        ] {
            assert_eq!(LapEvent::from_action(action), Some(event));
        }
        assert_eq!(LapEvent::from_action("banana"), None);
    }

    /// Вставка по времени: позиция бинарным поиском; равные метки —
    /// после равных (порядок вставки сохраняется).
    #[test]
    fn journal_remove_by_range() {
        let mut journal = sample_journal();
        for (ms, pad) in [
            (100, "pad-1"),
            (200, "pad-1"),
            (200, "pad-2"),
            (300, "pad-1"),
        ] {
            journal.push(
                Stamp(ms),
                JournalEntry::RaceEvent {
                    pad_id: pad.to_owned(),
                    name: LapEvent::Fault,
                },
            );
        }
        // Половинчатый интервал: 200 входит, 300 — нет.
        assert_eq!(journal.remove("pad-1", Stamp(100), Stamp(300)), 2);
        let rest: Vec<(Stamp, &str)> = journal
            .iter()
            .filter_map(|(ms, e)| match e {
                JournalEntry::RaceEvent { pad_id, .. } => Some((ms, pad_id.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(rest, vec![(Stamp(200), "pad-2"), (Stamp(300), "pad-1")]);
    }

    #[test]
    fn journal_push_inserts_by_time() {
        let mut journal = sample_journal();
        journal.push(Stamp(500), JournalEntry::RaceStarted {});
        journal.push(Stamp(100), JournalEntry::RecordStarted {});
        journal.push(Stamp(300), JournalEntry::RaceCountdown {});
        journal.push(
            Stamp(300),
            JournalEntry::RaceFinished { prematurely: false },
        );
        let marks: Vec<(Stamp, &str)> = journal
            .iter()
            .map(|(ms, e)| {
                let mark = match e {
                    JournalEntry::RecordStarted {} => "rec",
                    JournalEntry::RaceCountdown {} => "cd",
                    JournalEntry::RaceStarted {} => "go",
                    JournalEntry::RaceFinished { .. } => "finish",
                    _ => "?",
                };
                (ms, mark)
            })
            .collect();
        assert_eq!(
            marks,
            vec![
                (Stamp::ZERO, "rec"),
                (Stamp(100), "rec"),
                (Stamp(300), "cd"),
                (Stamp(300), "finish"),
                (Stamp(500), "go"),
            ]
        );
    }

    #[test]
    fn lap_event_serializes_lowercase() {
        let yaml = to_yaml_string(&LapEvent::Correction).unwrap();
        assert!(yaml.contains("correct"), "{yaml}");
        assert!(!yaml.contains("Correction"), "{yaml}");
        let yaml = to_yaml_string(&LapEvent::Warn).unwrap();
        assert!(yaml.contains("warn"), "{yaml}");
        assert!(!yaml.contains("Warn"), "{yaml}");
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
            (20_000, JournalEntry::RecordStarted {}),
            (
                20_500,
                JournalEntry::VideoAttached {
                    camera_id: "camera-1".to_owned(),
                    filename: "f-camera-1-mjpeg.mkv".to_owned(),
                },
            ),
            (23_000, JournalEntry::RaceCountdown {}),
            (23_500, JournalEntry::RaceStarted {}),
            (60_000, JournalEntry::RecordFinished {}),
            (68_000, JournalEntry::RaceFinished { prematurely: true }),
        ];
        let clip = RaceClip::from_events(events.iter().map(|(ms, e)| (Stamp(*ms), e)));
        assert!(!clip.is_empty());
        // Полоска — окно записи; файл лежит на нём со сдвигом.
        assert_eq!(clip.recording, Some(Interval::new(Stamp(20_000), Stamp(60_000))));
        assert_eq!(clip.files.len(), 1);
        assert_eq!(
            clip.files["camera-1"],
            ("f-camera-1-mjpeg.mkv".to_owned(), Stamp(20_500))
        );
        // Отсчёт собран: курсор DVR встаёт на него.
        assert_eq!(clip.race_countdown, Some(Stamp(23_000)));
        // Экстент журнала — финиш гонки.
        assert_eq!(clip.end(), Some(Stamp(68_000)));
        // Границы гонки — в журнальном домене (файловые позиции — на
        // стороне плееров: ноль файла = первый кадр).
        assert_eq!(clip.race_started, Some(Stamp(23_500)));
        assert_eq!(clip.race_finished, Some(Stamp(68_000)));
    }
}

/// Последний журнал полёта в каталоге: стемы сортируются лексикографически.
pub fn latest_journal(dir: &Path) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "yaml"))
        .max()
}
