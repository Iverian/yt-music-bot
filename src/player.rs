use std::fs::File;
use std::io::BufReader;
use std::thread;

use anyhow::Result as AnyResult;
use camino::Utf8PathBuf;
use rodio::source::EmptyCallback;
use rodio::{Decoder, OutputStream, Sink};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::util::channel::ChannelError;

pub const MIN_VOLUME: u8 = 1;
pub const MAX_VOLUME: u8 = 20;

pub type Result<T> = core::result::Result<T, Error>;
pub type EventTx = mpsc::UnboundedSender<Event>;
pub type OriginId = usize;
pub type QueueId = usize;
pub type Response = Option<Status>;
pub type Receiver = UnboundedReceiverStream<Event>;

#[derive(Debug)]
pub struct Player {
    tx: RequestTx,
}

#[derive(Debug, Clone, Error)]
pub enum Error {
    #[error("player queue is empty")]
    QueueEmpty,
    #[error("unable to open track")]
    InvalidTrack,
    #[error("invalid volume level, expected value from 0 to 10")]
    InvalidVolumeLevel,
    #[error(transparent)]
    Channel(#[from] ChannelError),
}

#[derive(Debug, Clone, Copy)]
pub struct Status {
    pub is_paused: bool,
    pub is_muted: bool,
    pub length: usize,
    pub volume_level: u8,
}

#[derive(Debug, Clone, Copy)]
pub struct Event {
    pub origin_id: OriginId,
    pub kind: EventKind,
}

#[derive(Debug, Clone, Copy)]
pub enum EventKind {
    PlaybackStarted,
    PlaybackPaused,
    PlaybackStopped,
    Muted,
    Unmuted,
    Volume(u8),
    TrackAdded(QueueId),
    TrackStarted(QueueId),
    TrackFinished(QueueId),
}

#[derive(Debug)]
pub enum Request {
    Play,
    Pause,
    PlayToggle,
    Stop,
    Skip,
    Mute,
    Unmute,
    MuteToggle,
    IncreaseVolume,
    DecreaseVolume,
    SetVolume { level: u8 },
    Queue { id: QueueId, path: Utf8PathBuf },
    Status,
}

type RequestTx = mpsc::UnboundedSender<RequestEnvelope>;
type RequestRx = mpsc::UnboundedReceiver<RequestEnvelope>;
type ResponseTx = oneshot::Sender<Result<Response>>;
type StartupTx = oneshot::Sender<AnyResult<()>>;
type OpenTrack = Decoder<BufReader<File>>;
type Callback = EmptyCallback<f32>;

struct RequestEnvelope {
    tx: ResponseTx,
    origin_id: OriginId,
    payload: Request,
}

struct Worker {
    sink: Sink,
    tx: EventTx,
    volume_level: u8,
    is_muted: bool,
}

impl Player {
    pub async fn new() -> AnyResult<(Self, Receiver)> {
        let (tx, rx) = mpsc::unbounded_channel();
        let (etx, erx) = mpsc::unbounded_channel();
        let (stx, srx) = oneshot::channel();
        thread::spawn(move || Worker::run(stx, etx, rx));
        srx.await??;
        Ok((Self { tx }, Receiver::new(erx)))
    }

    pub async fn request(&self, origin_id: OriginId, payload: Request) -> Result<Response> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(RequestEnvelope {
                tx,
                origin_id,
                payload,
            })
            .map_err(|_| ChannelError)?;
        let r = rx.await.map_err(|_| ChannelError)??;
        Ok(r)
    }
}

impl Worker {
    fn run(stx: StartupTx, tx: EventTx, rx: RequestRx) {
        let (_stream, sink) = match Self::init() {
            Ok(x) => x,
            Err(e) => {
                stx.send(Err(e)).unwrap();
                return;
            }
        };
        let worker = Worker {
            sink,
            tx,
            volume_level: 10u8,
            is_muted: false,
        };
        stx.send(Ok(())).unwrap();
        worker.serve_forever(rx);
        tracing::info!("player finished");
    }

    fn init() -> AnyResult<(OutputStream, Sink)> {
        let (stream, stream_handle) = OutputStream::try_default()?;
        let sink = Sink::try_new(&stream_handle)?;
        sink.pause();
        Ok((stream, sink))
    }

    fn serve_forever(mut self, mut rx: RequestRx) {
        loop {
            let Some(RequestEnvelope {
                tx,
                origin_id,
                payload,
            }) = rx.blocking_recv()
            else {
                return;
            };
            tracing::info!(request=?payload, "player request");
            tx.send(self.request_handler(origin_id, payload)).ok();
        }
    }

    fn request_handler(&mut self, origin_id: OriginId, payload: Request) -> Result<Response> {
        match payload {
            Request::Play => {
                self.play_handler(origin_id);
            }
            Request::Pause => {
                self.pause_handler(origin_id);
            }
            Request::PlayToggle => {
                self.play_toggle_handler(origin_id);
            }
            Request::Stop => {
                self.stop_handler(origin_id);
            }
            Request::Skip => {
                self.skip_handler()?;
            }
            Request::Mute => {
                self.mute_handler(origin_id);
            }
            Request::Unmute => {
                self.unmute_handler(origin_id);
            }
            Request::MuteToggle => {
                self.mute_toggle_handler(origin_id);
            }
            Request::SetVolume { level } => {
                self.set_volume_handler(origin_id, level)?;
            }
            Request::IncreaseVolume => {
                self.change_volume_handler(origin_id, 1);
            }
            Request::DecreaseVolume => {
                self.change_volume_handler(origin_id, -1);
            }
            Request::Queue { id, path } => {
                self.queue_handler(origin_id, path, id)?;
            }
            Request::Status => {
                return Ok(Some(self.status_handler()));
            }
        }
        Ok(None)
    }

    fn status_handler(&mut self) -> Status {
        let sink_length = self.sink.len();

        Status {
            is_paused: self.sink.is_paused(),
            is_muted: self.is_muted,
            length: sink_length / 3 + usize::from(sink_length % 3 > 1),
            volume_level: self.volume_level,
        }
    }

    fn queue_handler(&mut self, origin_id: OriginId, path: Utf8PathBuf, id: usize) -> Result<()> {
        let track = open_track(path).map_err(|_| Error::InvalidTrack)?;
        self.send(origin_id, EventKind::TrackAdded(id));
        self.sink
            .append(cb_track_started(self.tx.clone(), origin_id, id));
        self.sink.append(track);
        self.sink
            .append(cb_track_finished(self.tx.clone(), origin_id, id));
        Ok(())
    }

    fn change_volume_handler(&mut self, origin_id: OriginId, modifier: i8) {
        self.volume_level = change_level(self.volume_level, modifier);
        self.send(origin_id, EventKind::Volume(self.volume_level));
        if !self.is_muted {
            self.sink.set_volume(level_to_volume(self.volume_level));
        }
    }

    fn set_volume_handler(&mut self, origin_id: OriginId, level: u8) -> Result<()> {
        if !(MIN_VOLUME..=MAX_VOLUME).contains(&level) {
            return Err(Error::InvalidVolumeLevel);
        }
        self.volume_level = level;
        self.send(origin_id, EventKind::Volume(self.volume_level));
        if !self.is_muted {
            self.sink.set_volume(level_to_volume(self.volume_level));
        }
        Ok(())
    }

    fn mute_toggle_handler(&mut self, origin_id: OriginId) {
        if self.is_muted {
            self.send(origin_id, EventKind::Unmuted);
            self.is_muted = false;
            self.sink.set_volume(level_to_volume(self.volume_level));
        } else {
            self.send(origin_id, EventKind::Muted);
            self.is_muted = true;
            self.sink.set_volume(0.);
        }
    }

    fn unmute_handler(&mut self, origin_id: OriginId) {
        if !self.is_muted {
            return;
        }
        self.send(origin_id, EventKind::Unmuted);
        self.is_muted = false;
        self.sink.set_volume(level_to_volume(self.volume_level));
    }

    fn mute_handler(&mut self, origin_id: OriginId) {
        if self.is_muted {
            return;
        }
        self.send(origin_id, EventKind::Muted);
        self.is_muted = true;
        self.sink.set_volume(0.);
    }

    fn skip_handler(&mut self) -> Result<()> {
        if self.sink.empty() {
            return Err(Error::QueueEmpty);
        }
        self.sink.skip_one();
        Ok(())
    }

    fn stop_handler(&mut self, origin_id: OriginId) {
        if !self.sink.empty() {
            self.send(origin_id, EventKind::PlaybackStopped);
        }
        self.sink.clear();
        self.sink.pause();
    }

    fn play_toggle_handler(&mut self, origin_id: OriginId) {
        if self.sink.is_paused() {
            self.send(origin_id, EventKind::PlaybackStarted);
            self.sink.play();
        } else {
            self.send(origin_id, EventKind::PlaybackPaused);
            self.sink.pause();
        }
    }

    fn pause_handler(&mut self, origin_id: OriginId) {
        if !self.sink.is_paused() {
            self.send(origin_id, EventKind::PlaybackPaused);
        }
        self.sink.pause();
    }

    fn play_handler(&mut self, origin_id: OriginId) {
        if self.sink.is_paused() {
            self.send(origin_id, EventKind::PlaybackStarted);
        }
        self.sink.play();
    }

    fn send(&self, origin_id: OriginId, e: EventKind) {
        self.tx.send(Event { origin_id, kind: e }).unwrap();
    }
}

fn open_track(path: Utf8PathBuf) -> AnyResult<OpenTrack> {
    Ok(Decoder::new(BufReader::new(File::open(path)?))?)
}

fn cb_track_started(tx: EventTx, origin_id: OriginId, id: QueueId) -> Callback {
    cb_source(move || {
        tx.send(Event {
            origin_id,
            kind: EventKind::TrackStarted(id),
        })
        .ok();
    })
}

fn cb_track_finished(tx: EventTx, origin_id: OriginId, id: QueueId) -> Callback {
    cb_source(move || {
        tx.send(Event {
            origin_id,
            kind: EventKind::TrackFinished(id),
        })
        .ok();
    })
}

fn cb_source<F>(f: F) -> Callback
where
    F: Fn() + Send + 'static,
{
    EmptyCallback::new(Box::new(f))
}

fn level_to_volume(level: u8) -> f32 {
    f32::from(level) / f32::from(MAX_VOLUME)
}

fn change_level(level: u8, modifier: i8) -> u8 {
    if (level == MIN_VOLUME && modifier < 0) || (level == MAX_VOLUME && modifier > 0) {
        return level;
    }
    level.wrapping_add_signed(modifier)
}
