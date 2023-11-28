use camino::Utf8PathBuf;
use clap::Parser;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;

use crate::controller::Settings as ControllerSettings;

const ENV_PREFIX: &str = "APP";

/// Youtube music bot
#[derive(Debug, Parser)]
pub struct Cli {
    /// Download dir
    #[arg(long, short = 'C', env = env("DOWNLOAD_DIR"), default_value="data")]
    pub download_dir: Utf8PathBuf,
    #[arg(long, env=env("UNIX_SOCKET_PATH"), default_value="music-server.sock")]
    pub unix_socket_path: Utf8PathBuf,
    #[arg(long, env=env("TELEGRAM_BOT_TOKEN"))]
    pub telegram_bot_token: String,
    /// Worker threads
    #[arg(long, short = 'w', env = env("WORKERS"), default_value_t = Self::default_workers())]
    pub workers: usize,
    /// Download this number of tracks beforehand
    #[arg(long, env=env("TRACK_CACHE_SIZE"), default_value="4")]
    track_cache_size: usize,
    /// Start playback automatically after adding tracks to empty queue
    #[arg(long, env=env("AUTO_PLAY"))]
    auto_play: bool,
    /// Logging level
    #[arg(long, env = env("LOG_LEVEL"), default_value = "INFO")]
    log_level: LevelFilter,
    /// Format logs as json
    #[arg(long, env = env("LOG_USE_JSON"))]
    log_use_json: bool,
}

impl Cli {
    pub fn parse() -> Self {
        <Self as Parser>::parse()
    }

    pub fn controller_settings(&self) -> ControllerSettings {
        ControllerSettings {
            auto_play: self.auto_play,
            track_cache_size: self.track_cache_size,
        }
    }

    pub fn configure_logging(&self) {
        let r = tracing_subscriber::registry().with(if self.log_use_json {
            tracing_subscriber::fmt::layer()
                .json()
                .with_filter(self.log_level)
                .boxed()
        } else {
            tracing_subscriber::fmt::layer()
                .compact()
                .without_time()
                .with_filter(self.log_level)
                .boxed()
        });
        r.init();
    }

    fn default_workers() -> usize {
        num_cpus::get()
    }
}

fn env(name: &'static str) -> String {
    format!("{ENV_PREFIX}_{name}")
}
