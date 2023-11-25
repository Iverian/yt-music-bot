#![warn(clippy::pedantic)]

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

use crate::cli::Cli;
use crate::controller::{Controller, Settings as ControllerSettings};
use crate::player::Player;
use crate::server::Server;
use crate::util::cancel::{make_cancel_token, run_tasks};
use crate::util::temp_dir::TempDir;
use crate::youtube::Youtube;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

async fn run(cli: Cli) -> AnyResult<()> {
    let controller_settings = cli.controller_settings();
    let dir = TempDir::new(cli.download_dir).await?;

    let r = run_inner(
        cli.unix_socket_path,
        dir.path().to_owned(),
        controller_settings,
    )
    .await;

    dir.close().await;
    r
}

async fn run_inner(
    unix_socket_path: Utf8PathBuf,
    download_dir: Utf8PathBuf,
    controller_settings: ControllerSettings,
) -> AnyResult<()> {
    let token = make_cancel_token().await;

    let (player, player_receiver) = Player::new().await?;
    let youtube = Youtube::new(download_dir).await?;

    let (controller, controller_handle) = Controller::new(
        token.child_token(),
        player,
        player_receiver,
        youtube,
        controller_settings,
    );
    let server_handle = Server::spawn(token.child_token(), controller, unix_socket_path)?;

    run_tasks(vec![controller_handle, server_handle], token).await
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
