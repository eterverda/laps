use std::fs;
use std::path::PathBuf;
use std::process;

use crate::remote::Response;

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
    #[cfg(target_os = "linux")]
    ListKeyboards,
    ListCameras,
    ListInstances,
    /// Воспроизведение последней записи из captures/.
    Scrub,
}

/// Aligned table of exclusive keyboards: name, vid:pid, serial (empty
/// when the device has none). Attach — всегда grab: колонок режимов
/// нет. Column widths follow the longest cell; the serial column
/// carries no prefix.
#[cfg(target_os = "linux")]
fn print_keyboards(exclusive_keyboards: &[driver::keyboard::ExclusiveKeyboardInfo]) {
    let rows: Vec<[String; 3]> = exclusive_keyboards
        .iter()
        .map(|k| {
            [
                k.name.clone(),
                format!("{:04x}:{:04x}", k.vid, k.pid),
                k.serial.clone().unwrap_or_default(),
            ]
        })
        .collect();
    let mut widths = [0usize; 3];
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
            process::exit(2);
        }
        Some(Ok(Response { message, code: 0 })) => {
            if !message.is_empty() {
                println!("{message}");
            }
            process::exit(0);
        }
        Some(Ok(response)) => {
            if !response.message.is_empty() {
                eprintln!("{}", response.message);
            }
            process::exit(response.code);
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
        #[cfg(target_os = "linux")]
        Some(Commands::ListKeyboards) => match driver::keyboard::ExclusiveKeyboard::list() {
            Ok(keyboards) => print_keyboards(&keyboards),
            Err(e) => {
                println!("Error: {}", e);
            }
        },
        Some(Commands::Scrub) => {
            let captures = PathBuf::from("captures");
            // DVR живёт и без видео: журнала (yaml) хватает для полоски
            // и транспорта — пустой captures не пускаем, видео не требуем.
            let has_any = fs::read_dir(&captures)
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
                process::exit(1);
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
        None => {
            gui::run(None);
        }
    }
}
