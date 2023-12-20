#![allow(clippy::unused_async)]
use std::collections::HashSet;
use std::fmt::{Display, Write};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Error as AnyError, Result as AnyResult};
use futures::Future;
use lru_cache::LruCache;
use teloxide::dispatching::dialogue::{self, InMemStorage};
use teloxide::dispatching::{Dispatcher, UpdateHandler};
use teloxide::prelude::*;
use teloxide::types::{ChatAction, Message, MessageId, ParseMode};
use teloxide::utils::command::BotCommands;
use teloxide::utils::markdown;
use teloxide::{filter_command, Bot};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::controller::{
    Controller, Event, QueuedTrackState, Receiver as ControllerReceiver,
    Sender as ControllerSender, Status,
};
use crate::player::OriginId;
use crate::util::cancel::Task;
use crate::util::parse::{ArgumentList, VolumeCommand};
use crate::youtube::Track;

#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub max_request_duration: Duration,
}

pub fn spawn(
    token: CancellationToken,
    bot_token: &str,
    controller: &Controller,
    settings: Settings,
) -> Task {
    let (tx, rx) = controller.subscribe();
    let state = State::new();
    let bot = Bot::new(bot_token);
    let mut dispatcher = Dispatcher::builder(bot.clone(), schema())
        .dependencies(dptree::deps![
            InMemStorage::<DialogueState>::new(),
            tx,
            state.clone(),
            settings
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
const TRACKS_IN_MESSAGE: usize = 20;

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
    /// show queue.
    Show,
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
    // username: Option<String>,
}

#[derive(Debug, Clone)]
struct ReplyTo {
    chat_id: ChatId,
    message_id: MessageId,
}

struct TrackFmt<'a>(&'a Track);

struct DurationFmt<'a>(&'a Duration);

struct StatusFmt<'a>(&'a Status);

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
                .branch(case![Command::Status].endpoint(status))
                .branch(case![Command::Show].endpoint(show)),
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
    settings: Settings,
    tx: ControllerSender,
    urls: ArgumentList<Url>,
) -> HandlerResult {
    let urls = urls.into_vec();
    bot.send_chat_action(msg.chat.id, ChatAction::Typing)
        .await?;
    let tracks = filter_tracks(tx.resolve(urls).await?, &settings);
    state.request(&msg, tx.queue(tracks)).await?;
    confirm_request(&bot, &msg).await?;
    Ok(())
}

async fn status(bot: Bot, msg: Message, tx: ControllerSender) -> HandlerResult {
    let status = tx.status().await?;
    bot.send_message(msg.chat.id, format!("{}", StatusFmt(&status)))
        .reply_to_message_id(msg.id)
        .await?;
    Ok(())
}

async fn show(bot: Bot, msg: Message, tx: ControllerSender) -> HandlerResult {
    bot.send_chat_action(msg.chat.id, ChatAction::Typing)
        .await?;

    let view = tx.view().await;

    let mut messages = Vec::new();
    let mut message = String::new();
    let q: usize = (!matches!(
        view.first_track_state(),
        Some(QueuedTrackState::SentToPlayer)
    ))
    .into();

    for (i, j) in view.into_iter().enumerate() {
        let index = if matches!(j.state, QueuedTrackState::SentToPlayer) {
            "🎷".to_owned()
        } else {
            format!("{:2}", q + i)
        };
        let status = match j.state {
            QueuedTrackState::NotReady => "🔴",
            QueuedTrackState::Downloaded => "🟡",
            QueuedTrackState::SentToPlayer => "🟢",
        };

        writeln!(
            &mut message,
            "\\({index}\\) {} {}",
            status,
            TrackFmt(j.track)
        )?;

        if (1 + i) % TRACKS_IN_MESSAGE == 0 {
            messages.push(message);
            message = String::new();
        }
    }
    if !message.is_empty() {
        messages.push(message);
    }

    for text in messages {
        bot.send_message(msg.chat.id, &text)
            .disable_web_page_preview(true)
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
    }

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

fn filter_tracks(tracks: Vec<Track>, settings: &Settings) -> Vec<Track> {
    let mut cur = Duration::ZERO;
    tracks
        .into_iter()
        .take_while(|x| {
            cur += x.duration;
            cur <= settings.max_request_duration
        })
        .collect()
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
                // username: msg.from().and_then(|x| x.username.clone()),
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
        tracing::debug!(event = ?e, origin_id = origin_id, "bot notification");

        let mut state = self.0.lock().await;
        let request = state.requests.remove(&origin_id);
        let (messages, reply_to) = Self::format_event(e, request.as_ref())?;

        if let Some(ReplyTo {
            chat_id,
            message_id,
        }) = reply_to
        {
            for msg in &messages {
                bot.send_message(chat_id, msg)
                    .reply_to_message_id(message_id)
                    .disable_web_page_preview(true)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
        } else {
            for chat_id in &state.chats {
                for msg in &messages {
                    bot.send_message(*chat_id, msg)
                        .disable_web_page_preview(true)
                        .parse_mode(ParseMode::MarkdownV2)
                        .await?;
                }
            }
        }
        Ok(())
    }

    fn format_event(
        e: Event,
        request: Option<&RequestData>,
    ) -> AnyResult<(Vec<String>, Option<ReplyTo>)> {
        let mut result = Vec::new();
        let mut message = String::new();
        let mut reply_to = None;

        match e {
            Event::PlaybackStarted => message.push_str("▶️ Плеер запущен"),
            Event::PlaybackPaused => message.push_str("⏸️ Плеер на паузе"),
            Event::PlaybackStopped => message.push_str("⏹️ Плеер остановлен"),
            Event::QueueFinished => message.push_str("⏏️ Треки закончились"),
            Event::WaitingForDownload => message.push_str("🗿 Ждем загрузки трека"),
            Event::Muted => message.push_str("🔇 Без звука"),
            Event::Unmuted => message.push_str("🔊 Со звуком"),
            Event::TracksQueued { tracks } => {
                writeln!(&mut message, "⚡ Добавлены треки\n")?;
                for (i, track) in tracks.into_iter().enumerate() {
                    let i = 1 + i;

                    writeln!(&mut message, "\\({i:2}\\) {}", TrackFmt(&track.track))?;

                    if i % TRACKS_IN_MESSAGE == 0 {
                        result.push(message);
                        message = String::new();
                    }
                }
            }
            Event::Volume { level } => {
                writeln!(&mut message, "🔊 {level:2}")?;
            }
            Event::NowPlaying { queue_id: _, track } => {
                writeln!(&mut message, "🎷 {}", TrackFmt(&track))?;
            }
            Event::DownloadError { track, err } => {
                writeln!(
                    &mut message,
                    "❗ Ошибка загрузки\\: {}\n{}",
                    markdown::escape(&err.to_string()),
                    TrackFmt(&track)
                )?;
                if let Some(RequestData {
                    chat_id,
                    message_id,
                    // username: _,
                }) = request
                {
                    reply_to = Some(ReplyTo {
                        chat_id: *chat_id,
                        message_id: *message_id,
                    });
                }
            }
        }

        if !message.is_empty() {
            result.push(message);
        }

        Ok((result, reply_to))
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

impl<'a> Display for TrackFmt<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let webpage_url = markdown::escape_link_url(self.0.webpage_url.as_str());

        if let Some(artist) = self.0.artist.first() {
            let title = markdown::escape(&format!("{} - {}", artist, self.0.title));
            write!(
                f,
                "[{}]({}) {}",
                title,
                webpage_url,
                DurationFmt(&self.0.duration)
            )
        } else {
            let title = markdown::escape(&self.0.title);
            write!(
                f,
                "[{}]({}) {}",
                title,
                webpage_url,
                DurationFmt(&self.0.duration)
            )
        }
    }
}

impl<'a> Display for DurationFmt<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let total = self.0.as_secs();
        let minutes = total / 60;
        let seconds = total % 60;
        write!(f, "{minutes}:{seconds:02}")
    }
}

impl<'a> Display for StatusFmt<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:1} : {:1} {:2} : {} треков в очереди",
            if self.0.is_paused { "⏸️" } else { "▶️" },
            if self.0.is_muted { "🔇" } else { "🔊" },
            self.0.volume_level,
            self.0.length
        )
    }
}
