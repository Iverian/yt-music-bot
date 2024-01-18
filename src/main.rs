#![warn(clippy::pedantic)]

mod cli;
mod controller;
mod player;
mod run;
mod server;
mod telegram;
mod track_manager;
mod util;
mod youtube;

use anyhow::Result as AnyResult;
use dotenvy::dotenv;
use mimalloc::MiMalloc;

use self::cli::Cli;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() -> AnyResult<()> {
    let e = dotenv().err();
    let cli = Cli::parse();
    cli.configure();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(cli.workers)
        .build()?
        .block_on(async move {
            if let Some(e) = e {
                tracing::warn!(error = ?e, "error reading dotenv");
            }
            run::run(cli).await
        })
}
