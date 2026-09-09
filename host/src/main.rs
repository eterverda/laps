mod assets;
mod config;
mod driver;
mod gui;

#[derive(clap::Parser)]
#[command(name = "laps")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(clap::Subcommand)]
enum Commands {
    Hello,
    ListCameras,
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

    let cli = <Cli as clap::Parser>::parse();

    match cli.command {
        Some(Commands::Hello) => {
            log::info!("Hello Laps!");
            println!("Hello Laps!");
        }
        Some(Commands::ListCameras) => match driver::webcam::list_cameras() {
            Ok(descriptions) => {
                for desc in descriptions {
                    println!("{}", desc);
                }
            }
            Err(e) => {
                println!("Error: {}", e);
            }
        },
        None => {
            gui::run();
        }
    }
}
