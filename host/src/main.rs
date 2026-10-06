mod assets;
mod config;
mod driver;
mod gui;
mod model;
mod remote;

#[derive(clap::Parser)]
#[command(name = "laps")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(clap::Subcommand)]
enum Commands {
    ListCameras,
    ListInstances,
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
            gui::run();
        }
    }
}
