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
}

fn main() {
    let cli = <Cli as clap::Parser>::parse();

    match cli.command {
        Some(Commands::Hello) => {
            println!("Hello Laps!");
        }
        None => {
            gui::run();
        }
    }
}
