#![allow(clippy::unused_async)]
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Error as AnyError, Result as AnyResult};
use teloxide::dispatching::{Dispatcher, UpdateHandler};
use teloxide::prelude::*;
use teloxide::types::{Message, MessageId};
use teloxide::utils::command::BotCommands;
use teloxide::Bot;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::controller::{Controller, Event, Receiver as ControllerReceiver};
use crate::player::OriginId;
use crate::util::cancel::Task;

pub fn spawn(token: CancellationToken, bot_token: &str, controller: &Controller) -> Task {
    let (tx, rx) = controller.subscribe();
    let state = State::new();
    let bot = Bot::new(bot_token);
    let mut dispatcher = Dispatcher::builder(bot.clone(), schema())
        .dependencies(dptree::deps![tx, state.clone()])
        .build();

    let shutdown = dispatcher.shutdown_token();
    let task = tokio::spawn(state.serve_notifications(token.clone(), bot, rx));
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

type HandlerResult = AnyResult<()>;

#[derive(BotCommands, Clone)]
#[command(
    rename_rule = "lowercase",
    description = "These commands are supported:"
)]
enum Command {
    #[command(description = "subscribe to notifications.")]
    Start,
    #[command(description = "stop bot.")]
    Stop,
    #[command(description = "show this message.")]
    Help,
}

#[derive(Debug, Clone)]
struct State(Arc<Mutex<StateData>>);

#[derive(Default, Debug, Clone)]
struct StateData {
    chats: HashSet<ChatId>,
    direct_reply: HashMap<OriginId, ReplyTo>,
}

#[derive(Debug, Clone, Copy)]
struct ReplyTo {
    chat_id: ChatId,
    message_id: MessageId,
}

fn schema() -> UpdateHandler<AnyError> {
    use dptree::case;

    let command_handler = teloxide::filter_command::<Command, _>()
        .branch(case![Command::Start].endpoint(start))
        .branch(case![Command::Stop].endpoint(stop))
        .branch(case![Command::Help].endpoint(help));
    let message_handler = Update::filter_message()
        .branch(command_handler)
        .branch(dptree::endpoint(echo));

    message_handler
}

async fn start(bot: Bot, msg: Message, state: State) -> HandlerResult {
    state.subscribe(msg.chat.id).await;
    bot.send_message(msg.chat.id, "OK!")
        .reply_to_message_id(msg.id)
        .await?;
    Ok(())
}

async fn stop(bot: Bot, msg: Message, state: State) -> HandlerResult {
    state.unsubscribe(msg.chat.id).await;
    bot.send_message(msg.chat.id, "OK!")
        .reply_to_message_id(msg.id)
        .await?;
    Ok(())
}

async fn help(bot: Bot, msg: Message) -> HandlerResult {
    bot.send_message(msg.chat.id, Command::descriptions().to_string())
        .reply_to_message_id(msg.id)
        .send()
        .await?;
    Ok(())
}

async fn echo(_bot: Bot, msg: Message) -> HandlerResult {
    tracing::info!(message = ?msg, "received telegram message");
    Ok(())
}

impl State {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(StateData::default())))
    }

    async fn subscribe(&self, chat_id: ChatId) {
        let mut state = self.0.lock().await;
        state.chats.insert(chat_id);
    }

    async fn unsubscribe(&self, chat_id: ChatId) {
        let mut state = self.0.lock().await;
        state.chats.remove(&chat_id);
    }

    async fn serve_notifications(
        self,
        token: CancellationToken,
        bot: Bot,
        mut rx: ControllerReceiver,
    ) -> AnyResult<()> {
        tracing::info!("sending bot notifications to telegram");
        loop {
            tokio::select! {
                r = rx.recv_wrapped() => {
                    let Some(e) = r else {
                        break;
                    };
                    self.notification_handler(e.origin_id, e.event, &bot).await?;
                }
                () = token.cancelled() => {
                    break;
                }
            }
        }
        Ok(())
    }

    async fn notification_handler(
        &self,
        origin_id: OriginId,
        e: Event,
        bot: &Bot,
    ) -> AnyResult<()> {
        let mut state = self.0.lock().await;

        // TODO: add extra metadata and prettify
        let mut msg = format!("{e:?}");
        if msg.len() > 255 {
            msg = msg[..255].to_owned();
        }

        if let Some(ReplyTo {
            chat_id,
            message_id,
        }) = state.direct_reply.remove(&origin_id)
        {
            bot.send_message(chat_id, msg)
                .reply_to_message_id(message_id)
                .await?;
            return Ok(());
        }

        for chat_id in &state.chats {
            bot.send_message(*chat_id, &msg).await?;
        }
        Ok(())
    }
}
