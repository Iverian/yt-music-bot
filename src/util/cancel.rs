use anyhow::{Error as AnyError, Result as AnyResult};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub type Task = JoinHandle<AnyResult<()>>;

pub async fn make_cancel_token() -> CancellationToken {
    let token = CancellationToken::new();
    let t = token.clone();
    tokio::spawn(async move {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("error awaiting cancel signal: {e}");
        } else {
            tracing::info!("received CTRL + C");
        }
        t.cancel();
    });
    token
}

#[tracing::instrument(skip_all)]
pub async fn run_tasks(mut tasks: Vec<Task>, token: CancellationToken) -> AnyResult<()> {
    while !tasks.is_empty() {
        match wait_for_first_completed_task(tasks).await {
            Ok(h) => {
                tasks = h;
            }
            Err((handles, e)) => {
                token.cancel();
                if let Err(e) = wait_for_all_tasks(handles).await {
                    tracing::warn!("suppressed error: {e}");
                }
                return Err(e);
            }
        }
    }
    Ok(())
}

#[tracing::instrument(skip_all)]
async fn wait_for_first_completed_task(
    tasks: Vec<Task>,
) -> Result<Vec<Task>, (Vec<Task>, AnyError)> {
    let (r, _, other) = futures::future::select_all(tasks).await;
    match r {
        Ok(r) => match r {
            Ok(()) => Ok(other),
            Err(e) => Err((other, e)),
        },
        Err(e) => Err((other, e.into())),
    }
}

#[tracing::instrument(skip_all)]
async fn wait_for_all_tasks(tasks: Vec<Task>) -> AnyResult<()> {
    futures::future::join_all(tasks)
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    Ok(())
}
