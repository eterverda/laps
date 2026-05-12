mod assets;
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
    use simplelog::*;
    TermLogger::init(
        LevelFilter::Debug,
        ConfigBuilder::new().set_time_format_rfc3339().build(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )
    .unwrap();
}

fn main() {
    setup_logging();

    let cli = <Cli as clap::Parser>::parse();

    match cli.command {
        Some(Commands::Hello) => {
            log::info!("Hello Laps!");
            println!("Hello Laps!");
        }
        Some(Commands::ListCameras) => {
            log::info!("list-cameras command invoked");
            println!("Cameras:");
            match nokhwa::query(nokhwa::utils::ApiBackend::Auto) {
                Ok(cameras) => {
                    for cam in cameras {
                        println!("  [{}] {}", cam.index(), cam.human_name());
                    }
                }
                Err(e) => {
                    println!("  Error: {}", e);
                }
            }
        }
        None => {
            gui::run();
        }
    }
}
