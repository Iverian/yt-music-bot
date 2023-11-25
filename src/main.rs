#![warn(clippy::pedantic)]

mod cli;
mod controller;
mod player;
mod server;
mod track_manager;
mod util;
mod youtube;

use anyhow::Result as AnyResult;
use dotenvy::dotenv;
use mimalloc::MiMalloc;

use crate::cli::Cli;
use crate::controller::Controller;
use crate::player::Player;
use crate::server::Server;
use crate::util::{make_cancel_token, run_tasks};
use crate::youtube::Youtube;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

async fn run(cli: Cli) -> AnyResult<()> {
    tokio::fs::create_dir_all(&cli.download_dir).await?;

    let token = make_cancel_token().await;

    let (player, rx) = Player::new().await?;
    let youtube = Youtube::new(
        &cli.download_dir.canonicalize_utf8()?,
        cli.yt_download_threads,
    )
    .await?;

    let (controller, controller_handle) =
        Controller::new(token.child_token(), player, rx, youtube, true);
    let server_handle = Server::spawn(
        token.child_token(),
        controller,
        cli.unix_socket_path.clone(),
    )?;

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
