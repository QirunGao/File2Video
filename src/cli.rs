use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "yuv_ldpc_video")]
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
        #[arg(long, default_value_t = 24)]
        fps: u32,

        #[arg(long, default_value_t = 600)]
        frames: usize,

        #[arg(long, default_value_t = 5)]
        profile: u8,

        #[arg(long, default_value_t = true)]
        use_ffmpeg: bool,

        #[arg(long, default_value = "ffmpeg")]
        ffmpeg: String,

        #[arg(long, default_value = "veryfast")]
        x264_preset: String,

        #[arg(long, default_value_t = 23)]
        x264_crf: u8,

        #[arg(long, default_value_t = false)]
        fragmented_mp4: bool,

        #[arg(long, default_value_t = 120)]
        log_every: usize,
    },

    Decode {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        out: PathBuf,

        #[arg(long)]
        width: Option<usize>,
        #[arg(long)]
        height: Option<usize>,

        #[arg(long, default_value_t = true)]
        use_ffmpeg: bool,

        #[arg(long, default_value = "ffmpeg")]
        ffmpeg: String,

        #[arg(long, default_value = "ffprobe")]
        ffprobe: String,

        #[arg(long, default_value_t = 20000)]
        max_frames: usize,

        #[arg(long, default_value_t = 200)]
        log_every: usize,

        #[arg(long)]
        heavy_geom: bool,
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
            fps,
            frames,
            profile,
            use_ffmpeg,
            ffmpeg,
            x264_preset,
            x264_crf,
            fragmented_mp4,
            log_every,
        } => crate::core::encoder::encode(
            input,
            out,
            width,
            height,
            fps,
            frames,
            profile,
            use_ffmpeg,
            &ffmpeg,
            &x264_preset,
            x264_crf,
            fragmented_mp4,
            log_every,
        ),
        CommandCli::Decode {
            input,
            out,
            width,
            height,
            use_ffmpeg,
            ffmpeg,
            ffprobe,
            max_frames,
            log_every,
            heavy_geom,
        } => crate::core::decoder::decode(
            input,
            out,
            width,
            height,
            use_ffmpeg,
            &ffmpeg,
            &ffprobe,
            max_frames,
            log_every,
            heavy_geom,
        ),
    }
}