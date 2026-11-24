use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "aogara", about = "Aogara - Mahjong AI Assistant")]
pub struct Cli {
    #[arg(short, long, help = "Path to config.toml")]
    pub config: Option<PathBuf>,
}
