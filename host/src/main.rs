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
    use simplelog::*;
    use time::macros::format_description;
    TermLogger::init(
        LevelFilter::Debug,
        ConfigBuilder::new()
            .set_time_format_custom(format_description!(
                "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3][offset_hour sign:mandatory]:[offset_minute]"
            ))
            .set_time_offset_to_local()
            .unwrap()
            .build(),
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
