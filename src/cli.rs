use camino::Utf8PathBuf;
use clap::Parser;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;

use crate::controller::Settings as ControllerSettings;

/// Youtube music bot
#[derive(Debug, Parser)]
pub struct Cli {
    /// Download dir
    #[arg(long, short = 'C', env = "APP_DOWNLOAD_DIR", default_value = "data")]
    pub download_dir: Utf8PathBuf,
    /// Admin server Unix socket
    #[arg(long, env = "APP_UNIX_SOCKET_PATH")]
    pub unix_socket_path: Option<Utf8PathBuf>,
    /// Telegram bot token
    #[arg(long, env = "APP_TELEGRAM_BOT_TOKEN")]
    pub telegram_bot_token: Option<String>,
    /// Youtube downloader workers
    #[arg(long, env = "APP_YOUTUBE_WORKERS", default_value = "2")]
    pub youtube_workers: usize,
    /// Runtime worker threads
    #[arg(long, short = 'w', env = "APP_WORKERS", default_value_t = Self::default_workers())]
    pub workers: usize,
    /// Download this number of tracks beforehand
    #[arg(long, env = "APP_TRACK_CACHE_SIZE", default_value = "4")]
    track_cache_size: usize,
    /// Start playback automatically after adding tracks to empty queue
    #[arg(long, env = "APP_AUTO_PLAY")]
    auto_play: bool,
    /// Logging level
    #[arg(long, env = "APP_LOG_LEVEL", default_value = "INFO")]
    log_level: LevelFilter,
    /// Format logs as json
    #[arg(long, env = "APP_LOG_USE_JSON")]
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
