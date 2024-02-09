use std::time::Duration;

use camino::Utf8PathBuf;
use clap::Parser;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::runtime::{self};
use opentelemetry_sdk::trace::{BatchConfig, Tracer};
use opentelemetry_sdk::Resource;
use opentelemetry_semantic_conventions::resource::{
    DEPLOYMENT_ENVIRONMENT, SERVICE_NAME, SERVICE_VERSION,
};
use opentelemetry_semantic_conventions::SCHEMA_URL;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;
use url::Url;

use crate::controller::Settings as ControllerSettings;
use crate::telegram::Settings as BotSettings;
use crate::youtube::Settings as YoutubeSettings;

/// Youtube music bot
#[derive(Debug, Parser)]
#[allow(clippy::struct_excessive_bools)]
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
    /// Youtube download timeout in seconds
    #[arg(long, default_value = "30", env = "APP_YOUTUBE_DOWNLOAD_TIMEOUT_S")]
    pub youtube_download_timeout_s: u64,
    /// Runtime worker threads
    #[arg(long, short = 'w', env = "APP_WORKERS", default_value_t = Self::default_workers())]
    pub workers: usize,
    /// Download this number of tracks beforehand
    #[arg(long, env = "APP_TRACK_CACHE_SIZE", default_value = "4")]
    track_cache_size: usize,
    /// Start playback automatically after adding tracks to empty queue
    #[arg(long, env = "APP_AUTO_PLAY")]
    auto_play: bool,
    /// Maximum track duration for telegram bot
    #[arg(
        long,
        default_value = "3600",
        env = "APP_TELEGRAM_BOT_REQUEST_MAX_DURATION_S"
    )]
    bot_request_max_duration_s: u64,
    /// Allow only music tracks recognized by Youtube
    #[arg(long, env = "APP_TELEGRAM_BOT_ONLY_MUSIC_TRACKS")]
    bot_only_music_tracks: bool,
    /// Logging level
    #[arg(long, env = "APP_LOG_LEVEL", default_value = "INFO")]
    log_level: LevelFilter,
    /// Enable opentelemetry
    #[arg(long, env = "APP_OTLP_ENDPOINT")]
    otlp_endpoint: Option<Url>,
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

    pub fn bot_settings(&self) -> BotSettings {
        BotSettings {
            max_request_duration: Duration::from_secs(self.bot_request_max_duration_s),
            only_music_tracks: self.bot_only_music_tracks,
        }
    }

    pub fn youtube_settings(&self) -> YoutubeSettings {
        YoutubeSettings {
            jobs: self.youtube_workers,
            download_timeout: Duration::from_secs(self.youtube_download_timeout_s),
        }
    }

    pub fn configure(&self) {
        let log = tracing_subscriber::fmt::layer()
            .compact()
            .without_time()
            .with_writer(std::io::stderr)
            .with_filter(self.log_level);

        if let Some(url) = self.otlp_endpoint.as_ref() {
            tracing_subscriber::registry()
                .with(log)
                .with(OpenTelemetryLayer::new(init_tracer(url.clone())).with_filter(self.log_level))
                .init();
        } else {
            tracing_subscriber::registry().with(log).init();
        }
    }

    fn default_workers() -> usize {
        num_cpus::get()
    }
}

fn init_tracer(url: Url) -> Tracer {
    opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_trace_config(opentelemetry_sdk::trace::Config::default().with_resource(resource()))
        .with_batch_config(BatchConfig::default())
        .with_exporter(
            opentelemetry_otlp::new_exporter()
                .tonic()
                .with_endpoint(url),
        )
        .install_batch(runtime::TokioCurrentThread)
        .unwrap()
}

fn resource() -> Resource {
    Resource::from_schema_url(
        [
            KeyValue::new(SERVICE_NAME, env!("CARGO_PKG_NAME")),
            KeyValue::new(SERVICE_VERSION, env!("CARGO_PKG_VERSION")),
            KeyValue::new(DEPLOYMENT_ENVIRONMENT, "develop"),
        ],
        SCHEMA_URL,
    )
}
