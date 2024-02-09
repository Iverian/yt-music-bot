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
use tracing::instrument;

use self::cli::Cli;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[instrument]
fn main() -> AnyResult<()> {
    let e = dotenv().err();
    let cli = Cli::parse();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(cli.workers)
        .build()?
        .block_on(async move {
            cli.configure();
            let root = tracing::trace_span!("app_start");
            let _enter = root.enter();

            if let Some(e) = e {
                tracing::debug!(error = ?e, "error reading dotenv");
            }

            run::run(cli).await
        })
}
