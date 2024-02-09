use anyhow::Result as AnyResult;
use camino::Utf8PathBuf;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::instrument;

use crate::cli::Cli;
use crate::controller::{Controller, Settings as ControllerSettings};
use crate::server::spawn as spawn_server;
use crate::telegram::{spawn as spawn_bot, Settings as BotSettings};
use crate::util::temp_dir::TempDir;
use crate::youtube::{Settings as YoutubeSettings, Youtube};

#[instrument(skip_all)]
pub async fn run(cli: Cli) -> AnyResult<()> {
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

#[instrument(skip_all)]
async fn run_inner(
    unix_socket_path: Option<Utf8PathBuf>,
    telegram_bot_token: Option<String>,
    download_dir: Utf8PathBuf,
    controller_settings: ControllerSettings,
    bot_settings: BotSettings,
    youtube_settings: YoutubeSettings,
) -> AnyResult<()> {
    let token = CancellationToken::new();
    let mut join_set = JoinSet::new();

    let (controller, controller_task) = Controller::new(
        token.child_token(),
        Youtube::new(download_dir, youtube_settings).await?,
        controller_settings,
    )
    .await?;
    join_set.spawn(controller_task);
    if let Some(telegram_bot_token) = telegram_bot_token {
        tracing::info!("starting telegram bot");
        join_set.spawn(spawn_bot(
            token.child_token(),
            &telegram_bot_token,
            &controller,
            bot_settings,
        ));
    }
    if let Some(unix_socket_path) = unix_socket_path {
        tracing::info!(socket = ?unix_socket_path, "starting admin server");
        join_set.spawn(spawn_server(
            token.child_token(),
            unix_socket_path,
            controller,
        )?);
    }

    wait_for_ctrlc(token.clone());
    wait_for_tasks(token, join_set).await
}

#[instrument(skip_all)]
pub async fn wait_for_tasks(
    token: CancellationToken,
    mut join_set: JoinSet<AnyResult<()>>,
) -> AnyResult<()> {
    let mut result = Ok(());

    while let Some(x) = join_set.join_next().await {
        if let Err(e) = x.map_err(Into::into).and_then(|x| x) {
            if result.is_err() {
                tracing::warn!(error = ?e, "suppressed error");
            } else {
                tracing::warn!("cancelling tasks on first error");
                token.cancel();
                result = Err(e);
            }
        }
    }

    result
}

#[instrument(skip_all)]
pub fn wait_for_ctrlc(token: CancellationToken) {
    tokio::spawn(async move {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("error awaiting cancel signal: {e}");
        } else {
            tracing::info!("received CTRL + C");
        }
        token.cancel();
    });
}
