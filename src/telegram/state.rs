use std::fmt::Write;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result as AnyResult;
use dashmap::DashSet;
use futures::stream::once;
use futures::{stream_select, FutureExt, StreamExt};
use lru_cache::LruCache;
use teloxide::dispatching::dialogue::InMemStorage;
use teloxide::payloads::SendMessageSetters;
use teloxide::requests::Requester;
use teloxide::types::{ChatId, Message, MessageId, ParseMode};
use teloxide::utils::markdown;
use teloxide::Bot;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;

use super::defs::{REQUEST_STORAGE_CAPACITY, TRACKS_IN_MESSAGE};
use super::display::TrackFmt;
use crate::controller::{Event, EventWrapper, Receiver as ControllerReceiver};
use crate::player::OriginId;

pub type DialogueStorage = InMemStorage<DialogueData>;
pub type Dialogue = teloxide::dispatching::dialogue::Dialogue<DialogueData, DialogueStorage>;

#[derive(Debug, Clone, Copy, Default)]
pub enum DialogueData {
    #[default]
    Start,
    Run,
}

#[derive(Debug, Clone)]
pub struct State {
    tx: RequestTx,
    chats: ChatSet,
    settings: Settings,
}

#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub max_request_duration: Duration,
    pub only_music_tracks: bool,
}

#[derive(Debug, Clone)]
struct Handler {
    bot: Bot,
    chats: ChatSet,
    requests: RequestStorage,
}

#[derive(Debug, Clone)]
struct RequestData {
    pub chat_id: ChatId,
    pub message_id: MessageId,
}

type RequestStorage = LruCache<OriginId, RequestData>;
type ChatSet = Arc<dashmap::DashSet<ChatId>>;
type RequestTx = mpsc::UnboundedSender<RequestEntry>;
type RequestRx = mpsc::UnboundedReceiver<RequestEntry>;

#[derive(Clone)]
struct RequestEntry {
    origin_id: OriginId,
    data: RequestData,
}

#[allow(clippy::large_enum_variant)]
enum MergedEvent {
    Request(RequestEntry),
    Controller(EventWrapper),
    Cancel,
}

#[derive(Debug, Clone)]
struct ReplyTo {
    chat_id: ChatId,
    message_id: MessageId,
}

impl State {
    pub fn new(
        settings: Settings,
        token: CancellationToken,
        bot: Bot,
        rx: ControllerReceiver,
    ) -> (Self, impl Future<Output = AnyResult<()>>) {
        let (rtx, rrx) = mpsc::unbounded_channel();

        let chats = Arc::new(DashSet::new());
        let task = Handler::new(bot, chats.clone()).serve_forever(token, rrx, rx);
        let result = Self {
            tx: rtx,
            chats,
            settings,
        };

        (result, task)
    }

    pub async fn request<F>(&self, msg: &Message, f: F) -> AnyResult<()>
    where
        F: Future<Output = crate::controller::Result<OriginId>>,
    {
        let origin_id = f.await?;
        self.tx.send(RequestEntry {
            origin_id,
            data: RequestData {
                chat_id: msg.chat.id,
                message_id: msg.id,
                // username: msg.from().and_then(|x| x.username.clone()),
            },
        })?;

        Ok(())
    }

    pub fn subscribe(&self, chat_id: ChatId) {
        self.chats.insert(chat_id);
    }

    pub fn unsubscribe(&self, chat_id: ChatId) {
        self.chats.remove(&chat_id);
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }
}

impl Handler {
    fn new(bot: Bot, chats: ChatSet) -> Self {
        Self {
            bot,
            chats,
            requests: LruCache::new(REQUEST_STORAGE_CAPACITY),
        }
    }

    async fn serve_forever(
        mut self,
        token: CancellationToken,
        request_receiver: RequestRx,
        controller_receiver: ControllerReceiver,
    ) -> AnyResult<()> {
        tracing::info!("sending bot notifications to telegram");

        let cancel = pin!(token.cancelled_owned().fuse());
        let mut stream = stream_select!(
            UnboundedReceiverStream::new(request_receiver).map(MergedEvent::Request),
            controller_receiver.wrap().map(MergedEvent::Controller),
            once(cancel).map(|()| MergedEvent::Cancel),
        );

        while let Some(x) = stream.next().await {
            match x {
                MergedEvent::Request(RequestEntry { origin_id, data }) => {
                    self.requests.insert(origin_id, data);
                }
                MergedEvent::Controller(x) => {
                    self.handle(x.origin_id, x.event).await?;
                }
                MergedEvent::Cancel => {
                    break;
                }
            }
        }

        Ok(())
    }

    async fn handle(&mut self, origin_id: OriginId, e: Event) -> AnyResult<()> {
        tracing::debug!(event = ?e, origin_id = origin_id, "bot notification");

        let request = self.requests.remove(&origin_id);
        let (messages, reply_to) = format_event(e, request.as_ref())?;

        if let Some(ReplyTo {
            chat_id,
            message_id,
        }) = reply_to
        {
            for msg in &messages {
                self.bot
                    .send_message(chat_id, msg)
                    .reply_to_message_id(message_id)
                    .disable_web_page_preview(true)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
        } else {
            for chat_id in self.chats.iter() {
                for msg in &messages {
                    self.bot
                        .send_message(*chat_id, msg)
                        .disable_web_page_preview(true)
                        .parse_mode(ParseMode::MarkdownV2)
                        .await?;
                }
            }
        }

        Ok(())
    }
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
