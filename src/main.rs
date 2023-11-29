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

use crate::bot::spawn as spawn_bot;
use crate::cli::Cli;
use crate::controller::{Controller, Settings as ControllerSettings};
use crate::player::Player;
use crate::server::spawn as spawn_server;
use crate::util::cancel::{make_cancel_token, run_tasks};
use crate::util::temp_dir::TempDir;
use crate::youtube::Youtube;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

async fn run(cli: Cli) -> AnyResult<()> {
    let controller_settings = cli.controller_settings();
    let dir = TempDir::new(cli.download_dir).await?;
    let download_dir = dir.path().to_owned();

    let r = run_inner(
        cli.unix_socket_path,
        cli.telegram_bot_token,
        download_dir,
        cli.youtube_workers,
        controller_settings,
    )
    .await;

    dir.close().await;
    r
}

async fn run_inner(
    unix_socket_path: Utf8PathBuf,
    telegram_bot_token: String,
    download_dir: Utf8PathBuf,
    youtube_workers: usize,
    controller_settings: ControllerSettings,
) -> AnyResult<()> {
    let token = make_cancel_token().await;

    let (player, player_receiver) = Player::new().await?;
    let youtube = Youtube::new(download_dir, youtube_workers).await?;

    let (controller, controller_task) = Controller::new(
        token.child_token(),
        player,
        player_receiver,
        youtube,
        controller_settings,
    );
    let bot_task = spawn_bot(token.child_token(), &telegram_bot_token, &controller);
    let server_task = spawn_server(token.child_token(), unix_socket_path, controller)?;
    run_tasks(vec![controller_task, server_task, bot_task], token).await
}

fn main() -> AnyResult<()> {
    dotenv().ok();
    let cli = Cli::parse();
    cli.configure_logging();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(cli.workers)
        .build()?
        .block_on(run(cli))
}
