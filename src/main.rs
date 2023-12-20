#![warn(clippy::pedantic)]

mod bot;
mod cli;
mod controller;
mod player;
mod server;
mod track_manager;
mod util;
mod youtube;

use anyhow::Result as AnyResult;
use camino::Utf8PathBuf;
use dotenvy::dotenv;
use mimalloc::MiMalloc;

use crate::bot::{spawn as spawn_bot, Settings as BotSettings};
use crate::cli::Cli;
use crate::controller::{Controller, Settings as ControllerSettings};
use crate::server::spawn as spawn_server;
use crate::util::cancel::{make_cancel_token, run_tasks};
use crate::util::temp_dir::TempDir;
use crate::youtube::{Settings as YoutubeSettings, Youtube};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

async fn run(cli: Cli) -> AnyResult<()> {
    let controller_settings = cli.controller_settings();
    let bot_settings = cli.bot_settings();
    let youtube_settings = cli.youtube_settings();

    let dir = TempDir::new(cli.download_dir).await?;
    let download_dir = dir.path().to_owned();

    let r = run_inner(
        cli.unix_socket_path,
        cli.telegram_bot_token,
        download_dir,
        controller_settings,
        bot_settings,
        youtube_settings,
    )
    .await;

    dir.close().await;
    r
}

async fn run_inner(
    unix_socket_path: Option<Utf8PathBuf>,
    telegram_bot_token: Option<String>,
    download_dir: Utf8PathBuf,
    controller_settings: ControllerSettings,
    bot_settings: BotSettings,
    youtube_settings: YoutubeSettings,
) -> AnyResult<()> {
    let token = make_cancel_token().await;

    let mut tasks = Vec::with_capacity(3);
    let (controller, controller_task) = Controller::new(
        token.child_token(),
        Youtube::new(download_dir, youtube_settings).await?,
        controller_settings,
    )
    .await?;
    tasks.push(controller_task);
    if let Some(telegram_bot_token) = telegram_bot_token {
        tracing::info!("starting telegram bot");
        tasks.push(spawn_bot(
            token.child_token(),
            &telegram_bot_token,
            &controller,
            bot_settings,
        ));
    }
    if let Some(unix_socket_path) = unix_socket_path {
        tracing::info!(socket = ?unix_socket_path, "starting admin server");
        tasks.push(spawn_server(
            token.child_token(),
            unix_socket_path,
            controller,
        )?);
    }
    run_tasks(tasks, token).await
}

fn main() -> AnyResult<()> {
    let e = dotenv().err();
    let cli = Cli::parse();
    cli.configure_logging();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(cli.workers)
        .build()?
        .block_on(async move {
            if let Some(e) = e {
                tracing::warn!(error = ?e, "error reading dotenv");
            }
            run(cli).await
        })
}
