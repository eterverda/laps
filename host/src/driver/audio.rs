//! Короткие звуки гонки (бип отсчёта, гудок старт/стоп). Звук — это
//! заэмбеженный WAV PCM, декод — rodio/hound.
//!
//! Плеер живёт в своём потоке: команды — неблокирующий try_send, UI не
//! ждёт вывода. Поддерживает программы — серию звуков с задержками
//! (отсчёт: бипы по секундам, гудок на старте) — с отменой: новая
//! программа обрывает предыдущую, включая уже играющий звук; есть и
//! явный cancel. Нет звукового устройства (headless) — немой режим с
//! предупреждением, команды игнорируются.

use std::io::Cursor;
use std::time::{Duration, Instant};

use crossbeam_channel as crossbeam;
use std::thread;

/// Короткий звук: WAV PCM целиком в памяти, со своей длительностью.
/// Конструктор const — строится из бинарей `assets`, файл проверяется и
/// разбирается на месте: кривой WAV падает на компиляции.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sound {
    bytes: &'static [u8],
    duration: Duration,
}

impl Sound {
    /// Гудок старта/финиша и бип отсчёта — уровень приложения,
    /// константы звуков, а не сервиса.
    pub const BUZZER: Sound = Sound::from_bytes(crate::assets::BUZZER_WAV);
    pub const STAGE: Sound = Sound::from_bytes(crate::assets::STAGE_WAV);

    pub const fn from_bytes(bytes: &'static [u8]) -> Self {
        assert!(
            bytes.len() > 44
                && bytes[0] == b'R'
                && bytes[1] == b'I'
                && bytes[2] == b'F'
                && bytes[3] == b'F'
                && bytes[8] == b'W'
                && bytes[9] == b'A'
                && bytes[10] == b'V'
                && bytes[11] == b'E',
            "sound: not a RIFF/WAVE file"
        );
        let byte_rate = u32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]);
        assert!(byte_rate != 0, "sound: zero byte_rate");
        // Длительность из заголовка: byte_rate (u32 LE @28) и размер
        // data-чанка (сигнатура "data", u32 LE следом). Каноничный PCM
        // (так пишет ffmpeg).
        let mut i = 12;
        let mut data_size = None;
        while i + 8 <= bytes.len() {
            if bytes[i] == b'd'
                && bytes[i + 1] == b'a'
                && bytes[i + 2] == b't'
                && bytes[i + 3] == b'a'
            {
                data_size = Some(u32::from_le_bytes([
                    bytes[i + 4],
                    bytes[i + 5],
                    bytes[i + 6],
                    bytes[i + 7],
                ]));
                break;
            }
            i += 1;
        }
        let Some(data_size) = data_size else {
            panic!("sound: no data chunk")
        };
        Sound {
            bytes,
            duration: Duration::from_micros(data_size as u64 * 1_000_000 / byte_rate as u64),
        }
    }

    pub const fn duration(&self) -> Duration {
        self.duration
    }
}

/// Команды в поток-плеер.
enum Command {
    /// Один звук немедленно, не трогая текущую программу.
    Play(Sound),
    /// Программа: (задержка от старта программы, звук). Новая программа
    /// отменяет предыдущую, включая уже играющий её звук.
    Sequence(Vec<(Duration, Sound)>),
    /// Отменить программу и её звук.
    Cancel,
}

/// Хэндл звукового плеера: клонируемо-хранимый отправитель команд.
/// Поток-плеер останавливается по drop последнего хэндла (канал
/// разрывается, поток выходит).
#[derive(Clone)]
pub struct Audio {
    commands: crossbeam::Sender<Command>,
}

impl Audio {
    pub fn spawn() -> Self {
        let (commands, rx) = crossbeam::bounded(16);
        thread::spawn(move || output_loop(rx));
        Self { commands }
    }

    /// Один звук немедленно. Не ждёт, не паникует: очередь полна или
    /// плеер молчит (headless) — звук просто теряется.
    pub fn play(&self, sound: Sound) {
        let _ = self.commands.try_send(Command::Play(sound));
    }

    /// Запустить программу звуков с задержками (от старта программы).
    /// Отменяет текущую программу вместе с играющим звуком.
    pub fn sequence(&self, steps: Vec<(Duration, Sound)>) {
        let _ = self.commands.try_send(Command::Sequence(steps));
    }

    /// Отменить текущую программу и её звук.
    pub fn cancel(&self) {
        let _ = self.commands.try_send(Command::Cancel);
    }
}

/// Очередь запланированных звуков. Чистая логика таймингов — без
/// звукового устройства, тестируется напрямую.
struct Schedule {
    /// Абсолютные моменты воспроизведения, отсортированы по времени.
    steps: Vec<(Instant, Sound)>,
}

impl Schedule {
    fn new() -> Self {
        Self { steps: Vec::new() }
    }

    /// Новая программа: задержки от старта программы (момента вызова),
    /// отсортированы; старое расписание выбрасывается.
    fn set(&mut self, steps: Vec<(Duration, Sound)>) {
        let start = Instant::now();
        self.steps = steps
            .into_iter()
            .map(|(delay, sound)| (start + delay, sound))
            .collect();
        self.steps.sort_by_key(|(at, _)| *at);
    }

    fn cancel(&mut self) {
        self.steps.clear();
    }

    /// Время до ближайшего звука (None — расписание пусто).
    fn next_deadline(&self) -> Option<Duration> {
        self.steps
            .first()
            .map(|(at, _)| at.saturating_duration_since(Instant::now()))
    }

    /// Забрать все звуки, чей момент наступил.
    fn pop_due(&mut self) -> Vec<Sound> {
        let now = Instant::now();
        let due = self.steps.iter().take_while(|(at, _)| *at <= now).count();
        self.steps.drain(..due).map(|(_, sound)| sound).collect()
    }
}

/// Тело потока-плеера: открывает вывод один раз, дальше крутит
/// расписание и команды.
fn output_loop(commands: crossbeam::Receiver<Command>) {
    let Ok(stream) = rodio::DeviceSinkBuilder::open_default_sink() else {
        log::warn!("audio: no output device, sounds disabled");
        // Канал держим открытым: отправители не должны видеть ошибку,
        // просто звуков нет.
        for _ in commands {}
        return;
    };

    let mut schedule = Schedule::new();
    // Играющий звук программы: держим player, чтобы отмена могла его
    // оборвать. `play()` вне программ играет самостоятельно (detach).
    let mut current: Option<rodio::Player> = None;

    let start = |stream: &rodio::MixerDeviceSink, sound: Sound| -> Option<rodio::Player> {
        match rodio::Decoder::new(Cursor::new(sound.bytes)) {
            Ok(source) => {
                let player = rodio::Player::connect_new(stream.mixer());
                player.append(source);
                Some(player)
            }
            Err(e) => {
                log::warn!("audio: decode failed: {e}");
                None
            }
        }
    };

    loop {
        // Отжали играющий звук программы — доиграл.
        if current.as_ref().is_some_and(rodio::Player::empty) {
            current = None;
        }
        let wait = schedule
            .next_deadline()
            .unwrap_or(Duration::from_millis(100));
        match commands.recv_timeout(wait) {
            Ok(Command::Play(sound)) => {
                if let Some(player) = start(&stream, sound) {
                    player.detach();
                }
            }
            Ok(Command::Sequence(steps)) => {
                if let Some(player) = current.take() {
                    player.stop();
                }
                schedule.set(steps);
            }
            Ok(Command::Cancel) => {
                if let Some(player) = current.take() {
                    player.stop();
                }
                schedule.cancel();
            }
            Err(crossbeam::RecvTimeoutError::Timeout) => {
                for sound in schedule.pop_due() {
                    current = start(&stream, sound).or(current);
                }
            }
            Err(crossbeam::RecvTimeoutError::Disconnected) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Расписанию содержимое не важно — два разных звука для сравнений.
    const A: Sound = Sound::from_bytes(crate::assets::STAGE_WAV);
    const B: Sound = Sound::from_bytes(crate::assets::BUZZER_WAV);

    /// Заэмбеженные файлы декодируются (без звукового устройства);
    /// длительности из заголовков — против значений из спецификаций
    /// laps-bar/laps-trainer (stage 571, buzzer 783 мс, округлены вверх).
    /// Ломается, если парсер врёт или файл подменили чужим.
    #[test]
    fn embedded_sounds_decode() {
        use rodio::Source;
        for (sound, want_ms) in [(A, 571), (B, 783)] {
            let decoder = rodio::Decoder::new(Cursor::new(sound.bytes)).unwrap();
            let decoded_ms = decoder.total_duration().unwrap().as_millis() as i64;
            let header_ms = sound.duration().as_millis() as i64;
            assert!(
                (header_ms - want_ms).abs() <= 2,
                "header {header_ms} ms, expected ~{want_ms}"
            );
            assert!(
                (decoded_ms - want_ms).abs() <= 2,
                "decoded {decoded_ms} ms, expected ~{want_ms}"
            );
        }
    }

    /// Расписание: программа с задержками, просроченное забирается,
    /// будущее ждёт, отмена чистит.
    #[test]
    fn schedule_due_and_cancel() {
        let mut s = Schedule::new();
        // Нулевые задержки — due немедленно, порядок по времени держим.
        s.set(vec![
            (Duration::ZERO, A),
            (Duration::ZERO, B),
            (Duration::from_secs(60), A),
        ]);
        assert_eq!(s.pop_due(), vec![A, B]);
        assert!(s.next_deadline().is_some());
        // Будущее ещё не due.
        assert!(s.pop_due().is_empty());
        s.cancel();
        assert!(s.next_deadline().is_none());
        assert!(s.pop_due().is_empty());
    }

    /// Расписание: задержки сортируются, новая программа стирает старую.
    #[test]
    fn schedule_sorts_and_replaces() {
        let mut s = Schedule::new();
        s.set(vec![(Duration::from_secs(5), B), (Duration::ZERO, A)]);
        assert_eq!(s.pop_due(), vec![A]);
        s.set(vec![(Duration::ZERO, B)]);
        assert_eq!(s.pop_due(), vec![B]);
        assert!(s.next_deadline().is_none());
    }
}
