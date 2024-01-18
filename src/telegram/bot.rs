use teloxide::dispatching::dialogue::InMemStorage;
use teloxide::dispatching::Dispatcher;
use teloxide::prelude::*;
use teloxide::Bot;
use tokio_util::sync::CancellationToken;

use super::dispatch::schema;
use super::state::{DialogueData, Settings, State};
use crate::controller::Controller;
use crate::util::cancel::Task;

pub fn spawn(
    token: CancellationToken,
    bot_token: &str,
    controller: &Controller,
    settings: Settings,
) -> Task {
    let (tx, rx) = controller.subscribe();
    let bot = Bot::new(bot_token);
    let (state, task) = State::new(settings, token.clone(), bot.clone(), rx);
    let mut dispatcher = Dispatcher::builder(bot.clone(), schema())
        .dependencies(dptree::deps![
            InMemStorage::<DialogueData>::new(),
            tx,
            state,
            settings
        ])
        .build();

    let shutdown = dispatcher.shutdown_token();
    tokio::spawn(async move {
        tracing::info!("dispatcing telegram bot requests");
        dispatcher.dispatch().await;
    });
    tokio::spawn(async move {
        token.cancelled().await;
        shutdown.shutdown().unwrap().await;
    });
    task
}
