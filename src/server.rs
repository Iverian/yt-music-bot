use std::fmt::{Debug, Display};
use std::io;
use std::pin::pin;

use anyhow::Result as AnyResult;
use async_stream::try_stream;
use camino::Utf8PathBuf;
use futures::stream::once;
use futures::{stream_select, Future, FutureExt, Stream};
use itertools::Itertools;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::instrument;

use crate::controller::{
    Controller, Error as ControllerError, Event, QueuedTrackState, Receiver as ControllerReceiver,
    Sender as ControllerSender,
};
use crate::player::{Error as PlayerError, MAX_VOLUME};
use crate::util::channel::{ChannelError, ChannelResult};
use crate::youtube::{Error as YoutubeError, Track};

const VOLUME_ERROR_MSG: &str = "invalid volume level; volume must be between 0 and 10";

pub fn spawn(
    token: CancellationToken,
    path: Utf8PathBuf,
    controller: Controller,
) -> AnyResult<impl Future<Output = AnyResult<()>>> {
    let listener = UnixListener::bind(&path)?;
    tracing::info!(address = ?path, "admin server started");
    let server = Server { token, controller };
    let task = server.serve_forever(path, listener);
    Ok(task)
}

type WriterTx = mpsc::UnboundedSender<WriteMessage>;
type WriterRx = mpsc::UnboundedReceiver<WriteMessage>;
type Reader = BufReader<OwnedReadHalf>;
type Result<T> = core::result::Result<T, Error>;

#[derive(Debug)]
struct Server {
    token: CancellationToken,
    controller: Controller,
}

struct Connection {
    token: CancellationToken,
    tx: ControllerSender,
    writer: Writer,
}

#[derive(Debug, Error)]
enum Error {
    #[error("invalid url: {0:?}")]
    InvalidYoutubeUrl(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error(transparent)]
    Player(#[from] PlayerError),
    #[error(transparent)]
    Youtube(#[from] YoutubeError),
    #[error(transparent)]
    Channel(#[from] ChannelError),
}

#[derive(Debug, Clone, Copy)]
enum Command {
    Exit,
    Play,
    Pause,
    PlayToggle,
    Stop,
    Queue,
    Skip,
    Volume,
    Mute,
    Unmute,
    MuteToggle,
    Info,
}

enum VolumeCommand {
    Increase,
    Decrease,
    Set(u8),
}

enum WriteMessage {
    SetPrompt,
    ClearPrompt,
    Exit,
    Lines(Vec<String>),
}

#[derive(Clone)]
struct Writer(WriterTx);

struct WriterWorker {
    prompt: bool,
    writer: BufWriter<OwnedWriteHalf>,
}

enum ListenEvent {
    Accept(UnixStream),
    Cancel,
}

struct TrackFmt<'a>(&'a Track);

impl Server {
    async fn serve_forever(self, path: Utf8PathBuf, listener: UnixListener) -> AnyResult<()> {
        let r = self.serve_forever_impl(listener).await;
        tokio::fs::remove_file(path).await.ok();
        r
    }

    async fn serve_forever_impl(self, listener: UnixListener) -> AnyResult<()> {
        let cancel = pin!(self.token.cancelled().fuse());
        let accept = pin!(accept_stream(listener).map(|x| x.map(ListenEvent::Accept)));
        let mut stream = stream_select!(
            accept,
            once(cancel).map(|()| Ok(ListenEvent::Cancel) as io::Result<_>)
        );

        while let Some(x) = stream.next().await {
            match x? {
                ListenEvent::Accept(stream) => {
                    tracing::info!("accepted connection");
                    Connection::spawn(self.token.child_token(), &self.controller, stream);
                }
                ListenEvent::Cancel => {
                    tracing::info!("admin server shutdown");
                    break;
                }
            }
        }

        Ok(())
    }
}

impl Connection {
    fn spawn(token: CancellationToken, controller: &Controller, stream: UnixStream) {
        let (reader, writer) = stream.into_split();
        let reader = BufReader::new(reader);
        let writer = Writer::new(writer);
        let (tx, rx) = controller.subscribe();
        tokio::spawn(Connection::serve_notifications(rx, writer.clone()));
        let result = Self { token, tx, writer };
        tokio::spawn(result.serve_requests(reader));
    }

    #[instrument(skip_all)]
    async fn serve_requests(mut self, rdr: Reader) {
        if let Err(e) = self.serve_forever_impl(rdr).await {
            tracing::debug!(error = ?e, "unhandled error in connection");
        }
        tracing::debug!("closing admin connection");
        self.writer.write("Bye!").ok();
        self.writer.close();
    }

    async fn serve_forever_impl(&mut self, mut rdr: Reader) -> AnyResult<()> {
        let mut buf = String::new();

        self.writer.write("Hello!")?;
        loop {
            buf.clear();
            self.writer.set_prompt()?;
            tokio::select! {
                r = rdr.read_line(&mut buf) => {
                    if r? == 0 {
                        break;
                    }
                    self.writer.clear_prompt()?;
                    if self.handle_wrapper(&buf).await {
                        break;
                    }
                }
                () = self.token.cancelled() => {
                    break;
                }
            }
        }

        Ok(())
    }
    #[instrument(skip(self))]
    async fn handle_wrapper(&mut self, request: &str) -> bool {
        self.handle_request(request)
            .await
            .or_else(|e| self.handle_error(e))
            .unwrap_or(true)
    }

    #[instrument(skip(self))]
    fn handle_error(&mut self, e: Error) -> Result<bool> {
        match e {
            Error::Channel(_) => {
                return Ok(true);
            }
            Error::Player(e) => {
                self.writer
                    .write_many(vec!["[x] Player error".to_owned(), format!("... {e}")])?;
            }
            Error::Youtube(e) => {
                self.writer.write_many(vec![
                    "[x] Youtube download error".to_owned(),
                    format!("... {e}"),
                ])?;
            }
            e => {
                self.writer.write(format!("[x] {e}"))?;
            }
        }
        Ok(false)
    }

    async fn handle_request(&mut self, request: &str) -> Result<bool> {
        tracing::debug!("received admin request");

        let args = shell_words::split(request)
            .map_err(|e| Error::InvalidRequest(format!("Unable to parse request: {e}")))?;
        if args.is_empty() {
            return Ok(false);
        }

        let (command, args) = args.split_first().unwrap();
        let command = Self::get_command(command)?;
        tracing::info!(command=?command, args=?args, "admin request");

        match command {
            Command::Exit => {
                return Ok(true);
            }
            Command::Play => {
                self.tx.play().await?;
            }
            Command::Pause => {
                self.tx.pause().await?;
            }
            Command::PlayToggle => {
                self.tx.play_toggle().await?;
            }
            Command::Stop => {
                self.tx.stop().await?;
            }
            Command::Skip => {
                self.tx.skip().await?;
            }
            Command::Queue => {
                self.queue_request(args).await?;
            }
            Command::Volume => match Self::extract_volume_subcommand(args)? {
                VolumeCommand::Increase => {
                    self.tx.increase_volume().await?;
                }
                VolumeCommand::Decrease => {
                    self.tx.decrease_volume().await?;
                }
                VolumeCommand::Set(level) => {
                    self.tx.set_volume(level).await?;
                }
            },
            Command::Mute => {
                self.tx.mute().await?;
            }
            Command::Unmute => {
                self.tx.unmute().await?;
            }
            Command::MuteToggle => {
                self.tx.mute_toggle().await?;
            }
            Command::Info => {
                let status = self.tx.status().await?;
                self.writer.write(format!(
                    "... [{:1}] {:6} {:2} : {} in queue",
                    if status.is_paused { "⏸" } else { "▶️" },
                    if status.is_muted { "muted" } else { "volume" },
                    status.volume_level,
                    status.length,
                ))?;
            }
        }

        Ok(false)
    }

    #[instrument]
    fn get_command(command: &str) -> Result<Command> {
        Ok(match command {
            "exit" => Command::Exit,
            "play" => Command::Play,
            "pause" => Command::Pause,
            "play-toggle" | "p" => Command::PlayToggle,
            "stop" => Command::Stop,
            "skip" | "s" => Command::Skip,
            "queue" | "q" => Command::Queue,
            "mute" => Command::Mute,
            "unmute" => Command::Unmute,
            "mute-toggle" | "m" => Command::MuteToggle,
            "volume" | "v" => Command::Volume,
            "info" | "i" => Command::Info,
            _ => {
                return Err(Error::InvalidRequest(format!(
                    "Unknown command {command:?}"
                )));
            }
        })
    }

    fn extract_volume_subcommand(args: &[String]) -> Result<VolumeCommand> {
        if args.is_empty() {
            return Err(Error::InvalidRequest(
                "One argument expected +/-/num".to_owned(),
            ));
        }
        match args[0].as_str() {
            "+" => Ok(VolumeCommand::Increase),
            "-" => Ok(VolumeCommand::Decrease),
            level => {
                let level: u8 = level
                    .parse()
                    .map_err(|_| Error::InvalidRequest(VOLUME_ERROR_MSG.to_owned()))?;
                if level > MAX_VOLUME {
                    return Err(Error::InvalidRequest(VOLUME_ERROR_MSG.to_owned()));
                }
                Ok(VolumeCommand::Set(level))
            }
        }
    }

    #[instrument(skip(self))]
    async fn queue_request(&mut self, args: &[String]) -> Result<()> {
        if args.is_empty() {
            self.list_queue_request().await
        } else {
            self.queue_tracks_request(args).await
        }
    }

    #[instrument(skip(self))]
    async fn queue_tracks_request(&mut self, args: &[String]) -> Result<()> {
        let urls = args
            .iter()
            .map(|x| x.parse().map_err(|_| Error::InvalidYoutubeUrl(x.clone())))
            .try_collect()?;
        let tracks = self.tx.resolve(urls).await?;
        self.tx.queue(tracks).await?;
        Ok(())
    }

    #[instrument(skip(self))]
    async fn list_queue_request(&mut self) -> Result<()> {
        let view = self.tx.view().await;
        if view.queue.is_empty() {
            self.writer.write("... Queue is empty")?;
        } else {
            let queue = view
                .iter()
                .enumerate()
                .map(|(i, j)| {
                    let idx = if i == 0 {
                        "*".to_owned()
                    } else {
                        format!("{i}")
                    };
                    let state = match j.state {
                        QueuedTrackState::NotReady => 'x',
                        QueuedTrackState::Downloaded => 'd',
                        QueuedTrackState::SentToPlayer => 's',
                    };
                    format!("... ({idx:02}) [{state:1}] {}", TrackFmt(j.track))
                })
                .collect_vec();
            self.writer.write_many(queue)?;
        }
        Ok(())
    }

    #[instrument(skip_all)]
    async fn serve_notifications(mut rx: ControllerReceiver, wrt: Writer) {
        while let Some(e) = rx.next().await {
            if Self::handle_notification(e, &wrt).is_err() {
                break;
            }
        }
    }

    #[instrument(skip(writer))]
    fn handle_notification(e: Event, writer: &Writer) -> ChannelResult<()> {
        match e {
            Event::Muted => writer.write("[*] Muted"),
            Event::Unmuted => writer.write("[*] Unmuted"),
            Event::Volume { level } => writer.write(format!("[*] Volume level is {level}")),
            Event::PlaybackStarted => writer.write("[*] Started"),
            Event::PlaybackPaused => writer.write("[*] Paused"),
            Event::PlaybackStopped => writer.write("[*] Stopped"),
            Event::QueueFinished => writer.write("[*] Queue completed"),
            Event::WaitingForDownload => writer.write("[*] Waiting for track download"),
            Event::NowPlaying { queue_id: _, track } => {
                writer.write(format!("[*] Now playing: {}", TrackFmt(&track)))
            }
            Event::DownloadError { track, err } => writer.write(format!(
                "[*] Error downloading track {}: {}",
                TrackFmt(&track),
                err
            )),
            Event::TracksQueued { tracks } => {
                writer.write(format!("[*] {} tracks queued", tracks.len()))
            }
        }
    }
}

impl Writer {
    #[instrument(skip_all)]
    fn new(writer: OwnedWriteHalf) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let result = Self(tx);
        let worker = WriterWorker {
            writer: BufWriter::new(writer),
            prompt: false,
        };
        tokio::spawn(worker.serve_forever(rx));
        result
    }

    #[instrument(skip_all)]
    fn close(&self) {
        self.0.send(WriteMessage::Exit).ok();
    }

    #[instrument(skip_all)]
    fn set_prompt(&self) -> ChannelResult<()> {
        self.0.send(WriteMessage::SetPrompt)?;
        Ok(())
    }

    #[instrument(skip_all)]
    fn clear_prompt(&self) -> ChannelResult<()> {
        self.0.send(WriteMessage::ClearPrompt)?;
        Ok(())
    }

    #[instrument(skip(self))]
    fn write<S>(&self, line: S) -> ChannelResult<()>
    where
        S: Into<String> + Debug,
    {
        self.write_many(vec![line.into()])
    }

    #[instrument(skip(self))]
    fn write_many(&self, lines: Vec<String>) -> ChannelResult<()> {
        self.0.send(WriteMessage::Lines(lines))?;
        Ok(())
    }
}

impl WriterWorker {
    #[instrument(skip_all)]
    async fn serve_forever(mut self, mut rx: WriterRx) {
        while let Some(x) = rx.recv().await {
            let stop = match x {
                WriteMessage::Exit => true,
                WriteMessage::SetPrompt => self.handle_prompt().await.is_err(),
                WriteMessage::ClearPrompt => {
                    self.prompt = false;
                    false
                }
                WriteMessage::Lines(lines) => self.handle_message(lines).await.is_err(),
            };
            if stop {
                self.close(rx).await;
                break;
            }
        }
    }

    #[instrument(skip_all)]
    async fn handle_prompt(&mut self) -> io::Result<()> {
        self.writer.write_all(b"> ").await?;
        self.writer.flush().await?;
        self.prompt = true;
        Ok(())
    }

    #[instrument(skip(self))]
    async fn handle_message(&mut self, lines: Vec<String>) -> io::Result<()> {
        if self.prompt {
            self.writer.write_u8(b'\n').await?;
            self.prompt = false;
        }
        for i in lines {
            self.writer.write_all(i.as_bytes()).await?;
            self.writer.write_u8(b'\n').await?;
            self.writer.flush().await?;
        }
        Ok(())
    }

    #[instrument(skip_all)]
    async fn close(self, mut rx: WriterRx) {
        self.writer.into_inner().shutdown().await.ok();
        rx.close();
    }
}

impl<'a> Display for TrackFmt<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // We really do not care about all artists here
        if let Some(artist) = self.0.artist.first() {
            write!(f, "{} - {}", artist, self.0.title)
        } else {
            write!(f, "{}", self.0.title)
        }
    }
}

impl From<ControllerError> for Error {
    fn from(value: ControllerError) -> Self {
        match value {
            ControllerError::Youtube(e) => Error::Youtube(e),
            ControllerError::Channel(e) => Error::Channel(e),
            ControllerError::Player(e) => Error::Player(e),
        }
    }
}

#[instrument(skip_all)]
fn accept_stream(listener: UnixListener) -> impl Stream<Item = io::Result<UnixStream>> {
    try_stream! {
        loop {
            let (stream, _) = listener.accept().await?;
            yield stream;
        }
    }
}
