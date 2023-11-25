use camino::Utf8PathBuf;
use clap::Parser;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;

const ENV_PREFIX: &str = "APP";

#[derive(Debug, Parser)]
pub struct Cli {
    /// Download dir
    #[arg(long, short = 'C', env = env("DOWNLOAD_DIR"), default_value="data")]
    pub download_dir: Utf8PathBuf,
    #[arg(long, env=env("UNIX_SOCKET_PATH"), default_value="music-server.sock")]
    pub unix_socket_path: Utf8PathBuf,
    /// Youtube downloader worker threads
    #[arg(long, env=env("YT_DOWNLOAD_THREADS"), default_value="1")]
    pub yt_download_threads: usize,
    /// Worker threads
    #[arg(long, short = 'w', env = env("WORKERS"), default_value_t = Self::default_workers())]
    pub workers: usize,
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
