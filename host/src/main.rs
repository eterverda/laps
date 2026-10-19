mod assets;
mod config;
mod driver;
mod gui;
mod model;
mod remote;
mod service;

#[derive(clap::Parser)]
#[command(name = "laps")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(clap::Subcommand)]
enum Commands {
    ListCameras,
    ListKeyboards,
    ListInstances,
    /// Воспроизведение последней записи из captures/.
    Scrub,
    /// Temporary: attach all setup keyboards and print raw key events.
    /// Replaced by `test-keyboard` from docs/keyboard.md.
    DebugKeyboards,
}

/// Modes column of the listing: `listen,grab`, `listen` or `--` (no
/// access at all). Pure presentation of the probed flags.
fn modes_column(k: &driver::keyboard::KeyboardInfo) -> &'static str {
    match (k.listenable, k.grabbable) {
        (true, true) => "listen,grab",
        (true, false) => "listen",
        (false, _) => "--",
    }
}

/// Aligned table of keyboards: name, vid:pid, serial (empty when the
/// device has none), modes. Modes are the probed access truth
/// (listen,grab / listen / --), not policy. Column widths follow the
/// longest cell; the serial column carries no prefix.
fn print_keyboards(keyboards: &[driver::keyboard::KeyboardInfo]) {
    let rows: Vec<[String; 4]> = keyboards
        .iter()
        .map(|k| {
            [
                k.name.clone(),
                format!("{:04x}:{:04x}", k.vid, k.pid),
                k.serial.clone().unwrap_or_default(),
                modes_column(k).to_owned(),
            ]
        })
        .collect();
    let mut widths = [0usize; 4];
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            let pad = widths[i] - cell.chars().count();
            print!("{cell}");
            if i + 1 < row.len() {
                print!("{}", " ".repeat(pad + 2));
            }
        }
        println!();
    }
}

fn setup_logging() {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .filter_module("laps", log::LevelFilter::Debug)
        .format_timestamp_millis()
        .parse_default_env() // RUST_LOG overrides, e.g. RUST_LOG=laps=trace,eframe=debug
        .init();
}

fn main() {
    setup_logging();

    // Управляющий режим решается до clap: ведущие `+`-аргументы — это
    // адресат (+pid, один) и команда. None — обычный запуск, argv идёт в clap.
    let args: Vec<String> = std::env::args().skip(1).collect();
    match remote::run(&args) {
        None => {}
        Some(Err(e)) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
        Some(Ok(remote::Response { message, code: 0 })) => {
            if !message.is_empty() {
                println!("{message}");
            }
            std::process::exit(0);
        }
        Some(Ok(response)) => {
            if !response.message.is_empty() {
                eprintln!("{}", response.message);
            }
            std::process::exit(response.code);
        }
    }

    let cli = <Cli as clap::Parser>::parse_from(std::iter::once("laps".to_owned()).chain(args));

    match cli.command {
        Some(Commands::ListCameras) => match driver::camera::list_cameras() {
            Ok(descriptions) => {
                for desc in descriptions {
                    println!("{}", desc);
                }
            }
            Err(e) => {
                println!("Error: {}", e);
            }
        },
        Some(Commands::ListKeyboards) => match driver::keyboard::scan() {
            Ok(keyboards) => print_keyboards(&keyboards),
            Err(e) => {
                println!("Error: {}", e);
            }
        },
        Some(Commands::Scrub) => {
            let captures = std::path::PathBuf::from("captures");
            // DVR живёт и без видео: журнала (yaml) хватает для полоски
            // и транспорта — пустой captures не пускаем, видео не требуем.
            let has_any = std::fs::read_dir(&captures)
                .map(|mut d| {
                    d.any(|e| {
                        e.map(|e| {
                            e.path()
                                .extension()
                                .and_then(|x| x.to_str())
                                .is_some_and(|x| matches!(x, "mkv" | "mp4" | "mov" | "yaml"))
                        })
                        .unwrap_or(false)
                    })
                })
                .unwrap_or(false);
            if !has_any {
                eprintln!("nothing in captures/");
                std::process::exit(1);
            }
            gui::run(Some(captures));
        }
        Some(Commands::ListInstances) => {
            remote::cleanup();
            let instances = remote::discover();
            if instances.is_empty() {
                println!("no running instances");
            }
            for inst in instances {
                println!("pid: {} port: {}", inst.pid, inst.port);
            }
        }
        Some(Commands::DebugKeyboards) => debug_keyboards(),
        None => {
            gui::run(None);
        }
    }
}

/// Temporary debug command (replaced by `test-keyboard` from
/// docs/keyboard.md): attach every keyboard configured in the embedded
/// setup and print each event. All attach failures are listed before
/// exiting 1; on success the command blocks until killed.
#[cfg(target_os = "linux")]
fn debug_keyboards() -> ! {
    let setup: config::setup::Setup = match serde_yaml::from_str(assets::SETUP_YAML) {
        Ok(setup) => setup,
        Err(e) => {
            eprintln!("embedded setup.yaml failed to parse: {e}");
            std::process::exit(1);
        }
    };
    let mut failures = 0usize;
    let mut streams = Vec::new();
    for (id, cfg) in &setup.keyboards {
        match driver::keyboard::open(id, cfg) {
            Ok(rx) => streams.push(rx),
            Err(e) => {
                failures += 1;
                eprintln!("{id}: {e}");
            }
        }
    }
    if failures > 0 {
        std::process::exit(1);
    }
    // Fan every stream into one channel; each event already carries its
    // keyboard_id, so the printer needs no per-stream bookkeeping.
    let (tx, rx) = crossbeam_channel::unbounded::<driver::keyboard::KeyEvent>();
    for stream in streams {
        let tx = tx.clone();
        std::thread::spawn(move || {
            while let Ok(event) = stream.recv() {
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
    }
    while let Ok(event) = rx.recv() {
        // Mod+Mod+Key spelling: modifiers prefix the key, joined by "+".
        let modifiers = driver::keyboard::ModifierTracker::names(event.modifiers);
        let key = driver::keyboard::keycode_to_str(event.keycode);
        let combo = if modifiers.is_empty() {
            key
        } else {
            format!("{modifiers}+{key}")
        };
        println!(
            "{}  {:<7} {}",
            event.keyboard_id,
            event.action.as_str(),
            combo
        );
    }
    std::process::exit(0); // all streams ended (devices unplugged)
}

#[cfg(not(target_os = "linux"))]
fn debug_keyboards() -> ! {
    eprintln!("debug-keyboards is not supported on this platform");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_column_follows_the_flags() {
        let dev = |listenable: bool, grabbable: bool| driver::keyboard::KeyboardInfo {
            name: String::new(),
            vid: 0,
            pid: 0,
            serial: None,
            listenable,
            grabbable,
        };
        assert_eq!(modes_column(&dev(true, true)), "listen,grab");
        assert_eq!(modes_column(&dev(true, false)), "listen");
        assert_eq!(modes_column(&dev(false, true)), "--");
        assert_eq!(modes_column(&dev(false, false)), "--");
    }
}
