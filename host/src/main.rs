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
        .filter_level(log::LevelFilter::Debug)
        .filter_module("naga", log::LevelFilter::Warn)
        .filter_module("wgpu", log::LevelFilter::Warn)
        .format_timestamp_millis()
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
