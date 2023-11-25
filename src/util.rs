use anyhow::{Error as AnyError, Result as AnyResult};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub type Handle = JoinHandle<AnyResult<()>>;

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
pub async fn run_tasks(mut handles: Vec<Handle>, token: CancellationToken) -> AnyResult<()> {
    while !handles.is_empty() {
        match wait_for_any_handle(handles).await {
            Ok(h) => {
                handles = h;
            }
            Err((handles, e)) => {
                token.cancel();
                if let Err(e) = wait_for_all_handles(handles).await {
                    tracing::warn!("suppressed error: {e}");
                }
                return Err(e);
            }
        }
    }
    Ok(())
}

#[tracing::instrument(skip_all)]
async fn wait_for_any_handle(handles: Vec<Handle>) -> Result<Vec<Handle>, (Vec<Handle>, AnyError)> {
    let (r, _, other) = futures::future::select_all(handles).await;
    match r {
        Ok(r) => match r {
            Ok(_) => Ok(other),
            Err(e) => Err((other, e)),
        },
        Err(e) => Err((other, e.into())),
    }
}

#[tracing::instrument(skip_all)]
async fn wait_for_all_handles(handles: Vec<Handle>) -> AnyResult<()> {
    futures::future::join_all(handles)
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    Ok(())
}
