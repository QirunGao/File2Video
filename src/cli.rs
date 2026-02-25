use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "file2video-vnext7")]
struct Cli {
    #[command(subcommand)]
    cmd: CommandCli,
}

#[derive(Subcommand)]
enum CommandCli {
    Encode {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        out: PathBuf,

        #[arg(long, default_value_t = 1920)]
        width: usize,
        #[arg(long, default_value_t = 1088)]
        height: usize,

        #[arg(long, default_value_t = 5)]
        profile: u8,

        #[arg(long, default_value_t = 0.20)]
        max_overhead: f32,
    },

    Decode {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        out: PathBuf,

        #[arg(long, default_value_t = 256)]
        window_chunks: usize,

        #[arg(long)]
        heavy_geom: bool,

        #[arg(long)]
        best_effort: bool,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        CommandCli::Encode {
            input,
            out,
            width,
            height,
            profile,
            max_overhead,
        } => crate::core::encoder::encode(input, out, width, height, profile, max_overhead),
        CommandCli::Decode {
            input,
            out,
            window_chunks,
            heavy_geom,
            best_effort,
        } => crate::core::decoder::decode(input, out, window_chunks, heavy_geom, best_effort),
    }
}