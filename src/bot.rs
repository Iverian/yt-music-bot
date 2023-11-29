#![allow(clippy::unused_async)]
use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Error as AnyError, Result as AnyResult};
use futures::Future;
use lru_cache::LruCache;
use teloxide::dispatching::dialogue::{self, InMemStorage};
use teloxide::dispatching::{Dispatcher, UpdateHandler};
use teloxide::prelude::*;
use teloxide::types::{ChatAction, Message, MessageId};
use teloxide::utils::command::BotCommands;
use teloxide::{filter_command, Bot};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::controller::{
    Controller, Event, Receiver as ControllerReceiver, Sender as ControllerSender,
};
use crate::player::OriginId;
use crate::util::cancel::Task;
use crate::util::parse::{ArgumentList, VolumeCommand};

pub fn spawn(token: CancellationToken, bot_token: &str, controller: &Controller) -> Task {
    let (tx, rx) = controller.subscribe();
    let state = State::new();
    let bot = Bot::new(bot_token);
    let mut dispatcher = Dispatcher::builder(bot.clone(), schema())
        .dependencies(dptree::deps![
            InMemStorage::<DialogueState>::new(),
            tx,
            state.clone()
        ])
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

const REQUEST_STORAGE_CAPACITY: usize = 1000;

type HandlerResult = AnyResult<()>;
type DialogueStorage = InMemStorage<DialogueState>;
type Dialogue = teloxide::dispatching::dialogue::Dialogue<DialogueState, DialogueStorage>;
type RequestStorage = LruCache<OriginId, RequestData>;

/// These commands are supported.
#[derive(BotCommands, Debug, Clone)]
#[command(rename_rule = "lowercase")]
enum Command {
    /// start bot.
    Start,
    /// stop bot.
    Stop,
    /// show this message.
    Help,
    /// start playback.
    Play,
    /// pause playback.
    Pause,
    /// toggle playback.
    PlayToggle,
    /// stop playback and clear queue.
    Clear,
    /// skip one track.
    Skip,
    /// mute playback.
    Mute,
    /// unmute playback.
    Unmute,
    /// toggle mute.
    MuteToggle,
    /// set volume.
    Volume { cmd: VolumeCommand },
    /// queue tracks.
    Queue { urls: ArgumentList<Url> },
    /// show status.
    Status,
}

#[derive(Debug, Clone, Copy, Default)]
enum DialogueState {
    #[default]
    Start,
    Run,
}

#[derive(Debug, Clone)]
struct State(Arc<Mutex<StateData>>);

#[derive(Debug, Clone)]
struct StateData {
    chats: HashSet<ChatId>,
    requests: RequestStorage,
}

#[derive(Debug, Clone)]
struct RequestData {
    chat_id: ChatId,
    message_id: MessageId,
    username: Option<String>,
}

fn schema() -> UpdateHandler<AnyError> {
    use dptree::case;

    let command_handler = filter_command::<Command, _>()
        .branch(
            case![DialogueState::Start]
                .branch(case![Command::Start].endpoint(start))
                .branch(case![Command::Help].endpoint(help)),
        )
        .branch(
            case![DialogueState::Run]
                .branch(case![Command::Stop].endpoint(stop))
                .branch(case![Command::Help].endpoint(help))
                .branch(case![Command::Play].endpoint(play))
                .branch(case![Command::Pause].endpoint(pause))
                .branch(case![Command::PlayToggle].endpoint(play_toggle))
                .branch(case![Command::Clear].endpoint(clear))
                .branch(case![Command::Skip].endpoint(skip))
                .branch(case![Command::Mute].endpoint(mute))
                .branch(case![Command::Unmute].endpoint(unmute))
                .branch(case![Command::MuteToggle].endpoint(mute_toggle))
                .branch(case![Command::Volume { cmd }].endpoint(volume))
                .branch(case![Command::Queue { urls }].endpoint(queue))
                .branch(case![Command::Status].endpoint(status)),
        );

    let message_handler = Update::filter_message()
        .branch(command_handler)
        .branch(dptree::endpoint(error));

    dialogue::enter::<Update, DialogueStorage, DialogueState, _>().branch(message_handler)
}

async fn start(bot: Bot, msg: Message, state: State, dialogue: Dialogue) -> HandlerResult {
    state.subscribe(msg.chat.id).await;
    dialogue.update(DialogueState::Run).await?;
    confirm_request(&bot, &msg).await?;
    Ok(())
}

async fn stop(bot: Bot, msg: Message, state: State, dialogue: Dialogue) -> HandlerResult {
    state.unsubscribe(msg.chat.id).await;
    dialogue.update(DialogueState::Start).await?;
    confirm_request(&bot, &msg).await?;
    Ok(())
}

async fn help(bot: Bot, msg: Message) -> HandlerResult {
    bot.send_message(msg.chat.id, Command::descriptions().to_string())
        .reply_to_message_id(msg.id)
        .await?;
    Ok(())
}

async fn play(bot: Bot, msg: Message, state: State, tx: ControllerSender) -> HandlerResult {
    state.request(&msg, tx.play()).await?;
    confirm_request(&bot, &msg).await?;
    Ok(())
}

async fn pause(bot: Bot, msg: Message, state: State, tx: ControllerSender) -> HandlerResult {
    state.request(&msg, tx.pause()).await?;
    confirm_request(&bot, &msg).await?;
    Ok(())
}

async fn play_toggle(bot: Bot, msg: Message, state: State, tx: ControllerSender) -> HandlerResult {
    state.request(&msg, tx.play_toggle()).await?;
    confirm_request(&bot, &msg).await?;
    Ok(())
}

async fn clear(bot: Bot, msg: Message, state: State, tx: ControllerSender) -> HandlerResult {
    state.request(&msg, tx.stop()).await?;
    confirm_request(&bot, &msg).await?;
    Ok(())
}

async fn skip(bot: Bot, msg: Message, state: State, tx: ControllerSender) -> HandlerResult {
    state.request(&msg, tx.skip()).await?;
    confirm_request(&bot, &msg).await?;
    Ok(())
}

async fn mute(bot: Bot, msg: Message, state: State, tx: ControllerSender) -> HandlerResult {
    state.request(&msg, tx.mute()).await?;
    confirm_request(&bot, &msg).await?;
    Ok(())
}

async fn unmute(bot: Bot, msg: Message, state: State, tx: ControllerSender) -> HandlerResult {
    state.request(&msg, tx.unmute()).await?;
    confirm_request(&bot, &msg).await?;
    Ok(())
}

async fn mute_toggle(bot: Bot, msg: Message, state: State, tx: ControllerSender) -> HandlerResult {
    state.request(&msg, tx.mute_toggle()).await?;
    confirm_request(&bot, &msg).await?;
    Ok(())
}

async fn volume(
    bot: Bot,
    msg: Message,
    state: State,
    tx: ControllerSender,
    cmd: VolumeCommand,
) -> HandlerResult {
    match cmd {
        VolumeCommand::Increase => state.request(&msg, tx.increase_volume()).await,
        VolumeCommand::Decrease => state.request(&msg, tx.decrease_volume()).await,
        VolumeCommand::Set(level) => state.request(&msg, tx.set_volume(level)).await,
    }?;
    confirm_request(&bot, &msg).await?;
    Ok(())
}

async fn queue(
    bot: Bot,
    msg: Message,
    state: State,
    tx: ControllerSender,
    urls: ArgumentList<Url>,
) -> HandlerResult {
    let urls = urls.into_vec();
    bot.send_chat_action(msg.chat.id, ChatAction::Typing)
        .await?;
    state.request(&msg, tx.queue(urls)).await?;
    confirm_request(&bot, &msg).await?;
    Ok(())
}

async fn status(bot: Bot, msg: Message, tx: ControllerSender) -> HandlerResult {
    // TODO: prettify
    let status = tx.status().await?;
    bot.send_message(msg.chat.id, format!("{status:?}"))
        .reply_to_message_id(msg.id)
        .await?;
    Ok(())
}

async fn error(bot: Bot, msg: Message) -> HandlerResult {
    bot.send_message(msg.chat.id, "Uh oh")
        .reply_to_message_id(msg.id)
        .await?;
    tracing::info!(msg = ?msg, "unknown message");
    Ok(())
}

async fn confirm_request(bot: &Bot, msg: &Message) -> HandlerResult {
    bot.send_message(msg.chat.id, "OK!")
        .reply_to_message_id(msg.id)
        .await?;
    Ok(())
}

impl State {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(StateData::default())))
    }

    async fn request<F>(&self, msg: &Message, f: F) -> AnyResult<()>
    where
        F: Future<Output = crate::controller::Result<OriginId>>,
    {
        let origin_id = f.await?;
        let mut state = self.0.lock().await;
        state.requests.insert(
            origin_id,
            RequestData {
                chat_id: msg.chat.id,
                message_id: msg.id,
                username: msg.from().and_then(|x| x.username.clone()),
            },
        );
        Ok(())
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

        let _request = state.requests.remove(&origin_id);

        for chat_id in &state.chats {
            bot.send_message(*chat_id, &msg).await?;
        }
        Ok(())
    }
}

impl Default for StateData {
    fn default() -> Self {
        Self {
            chats: HashSet::default(),
            requests: LruCache::new(REQUEST_STORAGE_CAPACITY),
        }
    }
}
