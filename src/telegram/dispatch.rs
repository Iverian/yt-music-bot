#![allow(clippy::unused_async)]

use std::fmt::Write;
use std::time::Duration;

use anyhow::{Error as AnyError, Result as AnyResult};
use teloxide::dispatching::{dialogue, UpdateHandler};
use teloxide::macros::BotCommands;
use teloxide::prelude::*;
use teloxide::types::{ChatAction, Message, ParseMode};
use teloxide::utils::command::BotCommands as _;
use teloxide::{filter_command, Bot};
use url::Url;

use super::defs::{CONFIRM_TEXT, TRACKS_IN_MESSAGE};
use super::display::{StatusFmt, TrackFmt};
use super::state::{Dialogue, DialogueData, DialogueStorage, State};
use super::Settings;
use crate::controller::{QueuedTrackState, Sender as ControllerSender};
use crate::util::parse::{ArgumentList, VolumeCommand};
use crate::youtube::Track;

type HandlerResult = AnyResult<()>;

#[derive(Debug, Clone, BotCommands)]
#[command(
    description = "Плеер бот, умеет воспроизводить треки с YouTube.",
    rename_rule = "lowercase"
)]
enum Command {
    #[command(description = "Запустить бота в этом чате")]
    Start,
    #[command(description = "Остановить бота")]
    Stop,
    #[command(description = "Показать это сообщение")]
    Help,
    #[command(description = "Запустить плеер")]
    Play,
    #[command(description = "Приостановить плеер")]
    Pause,
    #[command(description = "Переключить состояние плеера")]
    PlayToggle,
    #[command(description = "Остановить плеер и очистить очередь")]
    Clear,
    #[command(description = "Пропустить трек")]
    Skip,
    #[command(description = "Выключить звук")]
    Mute,
    #[command(description = "Включить звук")]
    Unmute,
    #[command(description = "Переключить состояние звука")]
    MuteToggle,
    #[command(
        description = "Изменить громкость: принимает как аргумент число от 1 до 20, + или -"
    )]
    Volume { cmd: VolumeCommand },
    #[command(
        description = "Добавить треки в очередь: принимает как аргумент ссылки на YouTube через пробел"
    )]
    Queue { urls: ArgumentList<Url> },
    #[command(description = "Показать статус плеера")]
    Status,
    #[command(description = "Показать очередь")]
    Show,
}

pub fn schema() -> UpdateHandler<AnyError> {
    use dptree::case;

    let command_handler = filter_command::<Command, _>()
        .branch(
            case![DialogueData::Start]
                .branch(case![Command::Start].endpoint(start))
                .branch(case![Command::Help].endpoint(help)),
        )
        .branch(
            case![DialogueData::Run]
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

    dialogue::enter::<Update, DialogueStorage, DialogueData, _>().branch(message_handler)
}

async fn start(bot: Bot, msg: Message, state: State, dialogue: Dialogue) -> HandlerResult {
    state.subscribe(msg.chat.id);
    dialogue.update(DialogueData::Run).await?;
    confirm_request(&bot, &msg).await?;
    Ok(())
}

async fn stop(bot: Bot, msg: Message, state: State, dialogue: Dialogue) -> HandlerResult {
    state.unsubscribe(msg.chat.id);
    dialogue.update(DialogueData::Start).await?;
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

    reply(&bot, &msg, "Ищу треки...").await?;

    tokio::spawn(async move {
        if let Err(err) = queue_impl(tx, urls, state, bot, msg).await {
            tracing::debug!(error=?err, "error queueing tracks");
        }
    });

    Ok(())
}

async fn queue_impl(
    tx: ControllerSender,
    urls: Vec<Url>,
    state: State,
    bot: Bot,
    msg: Message,
) -> AnyResult<()> {
    let tracks = match tx.resolve(urls).await {
        Ok(x) => x,
        Err(e) => {
            reply(&bot, &msg, format!("❗ Ошибка поиска: {e}")).await?;
            return Ok(());
        }
    };

    let before = tracks.len();
    let tracks = filter_tracks(tracks, state.settings());
    let filtered = before - tracks.len();

    if tracks.is_empty() {
        reply(&bot, &msg, "Все треки были отфильтрованы").await?;
        return Ok(());
    }

    state.request(&msg, tx.queue(tracks)).await?;

    reply(
        &bot,
        &msg,
        if filtered == 0 {
            CONFIRM_TEXT.to_owned()
        } else {
            format!("{filtered} треков было отфильтровано")
        },
    )
    .await?;

    Ok(())
}

async fn status(bot: Bot, msg: Message, tx: ControllerSender) -> HandlerResult {
    let status = tx.status().await?;
    reply_md(&bot, &msg, StatusFmt(&status).to_string()).await?;
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
    if msg.text().is_some_and(|x| x.starts_with('/')) {
        reply(
            &bot,
            &msg,
            "Неизвестная команда, наберите /help для вывода справки",
        )
        .await?;
    }
    tracing::debug!(msg = ?msg, "unknown message");
    Ok(())
}

async fn confirm_request(bot: &Bot, msg: &Message) -> HandlerResult {
    reply(bot, msg, CONFIRM_TEXT).await
}

async fn reply<S>(bot: &Bot, msg: &Message, text: S) -> HandlerResult
where
    S: Into<String>,
{
    bot.send_message(msg.chat.id, text)
        .reply_to_message_id(msg.id)
        .await?;
    Ok(())
}

async fn reply_md<S>(bot: &Bot, msg: &Message, text: S) -> HandlerResult
where
    S: Into<String>,
{
    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_to_message_id(msg.id)
        .await?;
    Ok(())
}

fn filter_tracks(tracks: Vec<Track>, settings: &Settings) -> Vec<Track> {
    let mut cur = Duration::ZERO;
    tracks
        .into_iter()
        .filter(|x| !settings.only_music_tracks || x.is_music_track)
        .take_while(|x| {
            cur += x.duration;
            cur <= settings.max_request_duration
        })
        .collect()
}
