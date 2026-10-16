//! Удалённое управление запущенным приложением с того же бинаря.
//!
//! Управляющий режим включается пре-парсингом argv: ведущие аргументы с `+`
//! — это адресат (`+pid`, один) и голова команды; первый аргумент без `+`
//! начинает хвост. Команда — склейка головы и хвоста пробелами (аргументы
//! с пробелами/кавычками квотируются обратно), запрос уходит одной строкой
//! на 127.0.0.1:<port> целевого инстанса. Ответ: `текст\n<код>` — код
//! выхода после последнего перевода строки, message может быть
//! многострочной, пустая — ответ без текста.
//!
//! Реестр инстансов — YAML-файлы `instance-<pid>.yaml` (`pid`, `port`) в
//! per-user каталоге; живость проверяется коннектом.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

const DEFAULT_HOST: &str = "127.0.0.1";
/// Таймаут сокетных операций на loopback (чтение запроса, ответ).
const SOCKET_TIMEOUT: Duration = Duration::from_secs(1);
/// Ожидание ответа GUI: путь остановки эфира занимает ~1 с.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    pub pid: u32,
    pub port: u16,
}

impl Instance {
    fn alive(&self) -> bool {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.port);
        TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
    }
}

fn registry_dir() -> PathBuf {
    dirs::cache_dir()
        .expect("cache dir is not defined")
        .join("laps")
}

/// Сервер удалённого управления. Listener крутит отсоединённый
/// accept-поток и обслуживает клиентов по очереди: один в обслуживании,
/// остальные ждут в backlog. Поток умирает с процессом. Хендл держит
/// запись в реестре: файл инстанса живёт ровно столько, сколько он.
pub struct Server {
    instance: Instance,
    dir: PathBuf,
}

impl Server {
    /// Поднимает listener на эфемерном порту 127.0.0.1 и регистрирует
    /// инстанс. Команды целиком отдаёт `execute` — колбэку, который кладёт
    /// команду GUI-потоку и будит его (иначе ответ ждал бы ближайшего
    /// кадра egui).
    pub fn start(
        execute: std::sync::Arc<dyn Fn(Vec<String>) -> Response + Send + Sync>,
    ) -> std::io::Result<Self> {
        cleanup();
        let listener = TcpListener::bind((DEFAULT_HOST, 0))?;
        let addr = listener.local_addr()?;
        let server = Self::register_in(registry_dir(), addr.port())?;
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => serve(stream, execute.as_ref()),
                    Err(e) => log::warn!("remote: accept failed: {e}"),
                }
            }
        });
        log::info!(
            "instance started, remote control on {DEFAULT_HOST}:{} (pid {})",
            addr.port(),
            std::process::id()
        );
        Ok(server)
    }

    /// Пишет файл инстанса текущего процесса (без listener — шов для тестов).
    fn register_in(dir: PathBuf, port: u16) -> std::io::Result<Self> {
        let me = Self {
            instance: Instance {
                pid: std::process::id(),
                port,
            },
            dir,
        };
        std::fs::create_dir_all(&me.dir)?;
        std::fs::write(me.path(), serde_yaml::to_string(&me.instance).unwrap())?;
        Ok(me)
    }

    #[cfg(test)]
    pub fn port(&self) -> u16 {
        self.instance.port
    }

    fn path(&self) -> PathBuf {
        self.dir
            .join(format!("instance-{}.yaml", self.instance.pid))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.path());
    }
}

/// Живые инстансы из реестра (list-instances; живость — TCP-коннект).
/// Подчистка мёртвых — `cleanup()`: старт сервера и list-instances.
pub fn discover() -> Vec<Instance> {
    discover_in(&registry_dir())
}

/// Удаляет файлы мёртвых и битых инстансов.
pub fn cleanup() {
    cleanup_in(&registry_dir());
}

/// Сканирование реестра: (разбираемые записи, битые файлы). Живость тут
/// не проверяется: TCP-проба — это подключение-клиент, а сервер принимает
/// одного клиента за раз.
fn scan(dir: &Path) -> (Vec<Instance>, Vec<PathBuf>) {
    let mut records = Vec::new();
    let mut broken = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (records, broken);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let Ok(yaml) = std::fs::read_to_string(&path) else {
            broken.push(path);
            continue;
        };
        match serde_yaml::from_str::<Instance>(&yaml) {
            Ok(inst) => records.push(inst),
            Err(_) => broken.push(path),
        }
    }
    records.sort_by_key(|i| i.pid);
    (records, broken)
}

/// Живые из разбираемых записей.
fn discover_in(dir: &Path) -> Vec<Instance> {
    scan(dir).0.into_iter().filter(|i| i.alive()).collect()
}

/// Записи реестра без проверки живости — путь подключения: живость решает
/// connect, мёртвая запись даст быстрый отказ.
fn records_in(dir: &Path) -> Vec<Instance> {
    scan(dir).0
}

fn cleanup_in(dir: &Path) {
    let (records, mut stale) = scan(dir);
    stale.extend(
        records
            .iter()
            .filter(|i| !i.alive())
            .map(|i| dir.join(format!("instance-{}.yaml", i.pid))),
    );
    for path in stale {
        let _ = std::fs::remove_file(&path);
    }
}

// --- Пре-парсинг argv ---

enum Startup {
    /// Управляющий запрос: адресат и строка команды.
    Remote { pid: Option<u32>, request: String },
    /// Обычный запуск.
    Args,
}

/// Ответ инстанса: текст для лога (пустая строка — молча) и код выхода.
#[derive(Debug, PartialEq)]
pub struct Response {
    pub message: String,
    pub code: i32,
}

/// Разбор argv (без argv[0]). Ведущие `+`-аргументы: числовые — pid (не
/// больше одного), один нечисловой — голова команды; первый аргумент без
/// `+` заканчивает управляющую часть, он и всё после — хвост команды.
fn parse_startup(args: &[String]) -> Result<Startup, String> {
    let mut pid: Option<u32> = None;
    let mut head: Option<String> = None;
    let mut i = 0;
    while i < args.len() && args[i].starts_with('+') {
        let arg = args[i][1..].to_owned();
        if !arg.is_empty() && arg.chars().all(|c| c.is_ascii_digit()) {
            if pid.is_some() {
                return Err("only one +pid is allowed".to_owned());
            }
            pid = Some(arg.parse().map_err(|_| format!("bad +pid: {arg}"))?);
        } else if head.is_some() {
            return Err("only one +command is allowed".to_owned());
        } else {
            head = Some(arg);
        }
        i += 1;
    }
    let Some(head) = head else {
        return if pid.is_some() {
            Err("+pid without a +command".to_owned())
        } else {
            Ok(Startup::Args)
        };
    };
    let mut parts = vec![quote(&head)];
    parts.extend(args[i..].iter().map(|a| quote(a)));
    Ok(Startup::Remote {
        pid,
        request: parts.join(" "),
    })
}


// --- Строка команды: квотинг и токенизация ---

/// Квотит аргумент, если в нём есть пробел, кавычка или обратный слеш.
/// Управляющие символы (\n, \r, \t) экранируются — провод построчный.
fn quote(arg: &str) -> String {
    if arg.is_empty() {
        return "\"\"".to_owned();
    }
    if !arg
        .chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\\')
    {
        return arg.to_owned();
    }
    let mut inner = String::with_capacity(arg.len());
    for c in arg.chars() {
        match c {
            '"' => inner.push_str("\\\""),
            '\\' => inner.push_str("\\\\"),
            '\n' => inner.push_str("\\n"),
            '\r' => inner.push_str("\\r"),
            '\t' => inner.push_str("\\t"),
            c => inner.push(c),
        }
    }
    format!("\"{inner}\"")
}

/// Обратная quote операция: разбивает строку команды на токены. Двойные
/// кавычки группируют, `\` экранирует следующий символ (`n`/`r`/`t` —
/// управляющие, остальное — литерально).
fn tokenize(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let escaped = match chars.next() {
                    Some('n') => '\n',
                    Some('r') => '\r',
                    Some('t') => '\t',
                    Some(c) => c,
                    None => continue,
                };
                cur.push(escaped);
                started = true;
            }
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    tokens.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        tokens.push(cur);
    }
    tokens
}

// --- Ответ ---

/// Разбор ответа сервера: `текст\n<код>` — после последнего перевода
/// строки идёт код выхода, всё перед ним (без перевода строки) — message.
/// `"ok\n0"` → `ok/0`, `"0"` → пусто/0, строка без кода → message/0.
fn parse_response(line: &str) -> Response {
    let line = line.trim_end_matches(['\r', '\n']);
    let bytes = line.as_bytes();
    let digits = bytes
        .iter()
        .rev()
        .take_while(|b| b.is_ascii_digit())
        .count();
    if digits > 0 && digits < bytes.len() && bytes[bytes.len() - digits - 1] == b'\n' {
        let split = bytes.len() - digits;
        if let Ok(code) = line[split..].parse() {
            return Response {
                message: line[..split - 1].to_owned(),
                code,
            };
        }
    }
    if digits > 0 && digits == bytes.len() {
        if let Ok(code) = line.parse() {
            return Response {
                message: String::new(),
                code,
            };
        }
    }
    Response {
        message: line.to_owned(),
        code: 0,
    }
}

// --- Клиент ---

/// Разбирает argv и выполняет управляющий запрос, если это он. Внешний
/// `Option` отвечает «это remote-вызов?»: None — обычный запуск
/// приложения (argv разбирает вызывающий). Внутренний `Result` — исход
/// вызова: Err — ошибка разбора (usage), Ok — ответ инстанса.
pub fn run(args: &[String]) -> Option<Result<Response, String>> {
    let (pid, request) = match parse_startup(args) {
        Ok(Startup::Remote { pid, request }) => (pid, request),
        Ok(Startup::Args) => return None,
        Err(e) => return Some(Err(e)),
    };

    let instances = records_in(&registry_dir());
    let fail = |message: String| Response { message, code: 1 };
    let target = match pid {
        Some(p) => match instances.iter().find(|i| i.pid == p) {
            Some(i) => i.clone(),
            None => return Some(Ok(fail(format!("no running instance with pid {p}")))),
        },
        None => match instances.as_slice() {
            [one] => one.clone(),
            [] => return Some(Ok(fail("no running Laps instance".to_owned()))),
            many => {
                let list = many
                    .iter()
                    .map(|i| format!("  +{} (port {})", i.pid, i.port))
                    .collect::<Vec<_>>()
                    .join("\n");
                return Some(Ok(fail(format!(
                    "multiple running instances, pick one with +pid:\n{list}"
                ))));
            }
        },
    };

    let mut stream = match TcpStream::connect((DEFAULT_HOST, target.port)) {
        Ok(s) => s,
        Err(e) => {
            return Some(Ok(fail(format!(
                "connect to pid {} failed: {e}",
                target.pid
            ))));
        }
    };
    let _ = stream.set_read_timeout(Some(SOCKET_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SOCKET_TIMEOUT));
    if let Err(e) = writeln!(stream, "{request}") {
        return Some(Ok(fail(format!("send failed: {e}"))));
    }
    // Ответ читаем до закрытия соединения сервером: message может
    // содержать переводы строк, разделитель — последний из них.
    let mut reader = BufReader::new(stream);
    let mut text = String::new();
    let response = match reader.read_to_string(&mut text) {
        Err(_) => Response {
            message: format!("no response from pid {}", target.pid),
            code: 1,
        },
        Ok(_) => parse_response(&text),
    };
    Some(Ok(response))
}

// --- Сервер ---

/// Упаковка ответа в строку провода: `текст\n<код>`. Пустой текст
/// допустим — строка начинается с перевода строки.
fn wire(response: &Response) -> String {
    format!("{}\n{}", response.message, response.code)
}

/// Разбор команды, полученной по проводу: токенизация и исполнение
/// колбэком, ответ — в провод.
fn dispatch(line: &str, execute: &(dyn Fn(Vec<String>) -> Response + Send + Sync)) -> String {
    wire(&execute(tokenize(line)))
}

fn serve(stream: TcpStream, execute: &(dyn Fn(Vec<String>) -> Response + Send + Sync)) {
    let _ = stream.set_read_timeout(Some(SOCKET_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SOCKET_TIMEOUT));
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut line = String::new();
    if BufReader::new(stream).read_line(&mut line).is_err() {
        return;
    }
    let response = dispatch(&line, execute);
    let _ = writeln!(writer, "{response}");
    let _ = writer.flush();
    // Выход закрывает оба handle сокета — клиент получает EOF.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("laps-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Контракт провода: quote→tokenize возвращает аргумент целиком,
    /// включая управляющие символы (провод построчный).
    #[test]
    fn quote_roundtrip() {
        for arg in [
            "",
            "live",
            "hello world",
            "say \"hi\"",
            "back\\slash",
            "a b\"c\\d",
            "line\nbreak",
            "carriage\rreturn",
            "tab\there",
        ] {
            let tokens = tokenize(&quote(arg));
            assert_eq!(tokens, vec![arg.to_owned()], "roundtrip of {arg:?}");
        }
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("two words"), "\"two words\"");
        // провод построчный: управляющие экранируются, не сырые
        assert_eq!(quote("line\nbreak"), "\"line\\nbreak\"");
        assert_eq!(quote("a\tb\rc"), "\"a\\tb\\rc\"");
    }

    #[test]
    fn tokenize_basics() {
        assert_eq!(tokenize(""), Vec::<String>::new());
        assert_eq!(tokenize("   "), Vec::<String>::new());
        assert_eq!(tokenize("live"), vec!["live"]);
        assert_eq!(
            tokenize("set \"race name\" now"),
            vec!["set", "race name", "now"]
        );
        assert_eq!(tokenize("a \\\"q\\\" b"), vec!["a", "\"q\"", "b"]);
        assert_eq!(tokenize("x \\\\ y"), vec!["x", "\\", "y"]);
        assert_eq!(tokenize("a\\nb\\tc"), vec!["a\nb\tc"]);
        assert_eq!(tokenize("a\\rb"), vec!["a\rb"]);
    }

    #[test]
    fn parse_response_basics() {
        let r = |message: &str, code| Response {
            message: message.to_owned(),
            code,
        };
        assert_eq!(parse_response("ok\n0"), r("ok", 0));
        assert_eq!(parse_response("0"), r("", 0));
        assert_eq!(parse_response("42"), r("", 42));
        assert_eq!(parse_response("no such target\n3"), r("no such target", 3));
        assert_eq!(parse_response("just text"), r("just text", 0));
        assert_eq!(parse_response(""), r("", 0));
        assert_eq!(parse_response("\n0"), r("", 0));
        assert_eq!(parse_response("multi\nline\n7"), r("multi\nline", 7));
    }

    #[test]
    fn startup_parsing() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(matches!(parse_startup(&s(&[])), Ok(Startup::Args)));
        assert!(matches!(parse_startup(&s(&["hello"])), Ok(Startup::Args)));
        let Ok(Startup::Remote { pid, request }) = parse_startup(&s(&["+live"])) else {
            panic!("+live must be control");
        };
        assert_eq!(pid, None);
        assert_eq!(request, "live");
        let Ok(Startup::Remote { pid, request }) =
            parse_startup(&s(&["+4217", "+live", "extra arg"]))
        else {
            panic!("+pid form must be control");
        };
        assert_eq!(pid, Some(4217));
        assert_eq!(request, "live \"extra arg\"");
        assert!(parse_startup(&s(&["+1", "+2", "+live"])).is_err());
        assert!(parse_startup(&s(&["+live", "+rec"])).is_err());
        assert!(parse_startup(&s(&["+4217"])).is_err());
    }

    #[test]
    fn registry_roundtrip() {
        let dir = temp_dir("registry");
        let listener = TcpListener::bind((DEFAULT_HOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = Server::register_in(dir.clone(), port).unwrap();
        assert_eq!(
            discover_in(&dir),
            vec![Instance {
                pid: std::process::id(),
                port,
            }]
        );
        drop(listener); // порт мёртв — инстанс тоже
        assert!(discover_in(&dir).is_empty());
        drop(server); // Drop снимает регистрацию
    }

    /// Колбэк-заглушка, имитирующая App: echo — эхо, остальное — unknown.
    fn echo_execute() -> std::sync::Arc<dyn Fn(Vec<String>) -> Response + Send + Sync> {
        std::sync::Arc::new(|tokens| {
            if tokens.first().map(String::as_str) == Some("echo") {
                Response {
                    message: tokens[1..].join(" "),
                    code: 0,
                }
            } else {
                let message = match tokens.first() {
                    Some(head) => format!("unknown command {head:?}"),
                    None => "empty command".to_owned(),
                };
                Response { message, code: 1 }
            }
        })
    }

    #[test]
    fn server_echo() {
        let server = Server::start(echo_execute()).unwrap();
        let request = |line: &str| {
            let mut stream = TcpStream::connect((DEFAULT_HOST, server.port())).unwrap();
            writeln!(stream, "{line}").unwrap();
            let mut text = String::new();
            BufReader::new(stream).read_to_string(&mut text).unwrap();
            parse_response(&text)
        };
        let r = |message: &str, code| Response {
            message: message.to_owned(),
            code,
        };
        assert_eq!(request("echo hello world"), r("hello world", 0));
        assert_eq!(request("echo"), r("", 0));
        assert_eq!(request("echo \"a b\""), r("a b", 0));
        // Неэхо команды решает исполнитель (как App: unknown command).
        for cmd in ["frobnicate", "toggle-live"] {
            assert_eq!(request(cmd), r(&format!("unknown command {cmd:?}"), 1));
        }
    }
}
