use std::fs::File;
use std::io::BufReader;
use std::thread;

use anyhow::Result as AnyResult;
use camino::Utf8PathBuf;
use rodio::source::EmptyCallback;
use rodio::{Decoder, OutputStream, Sink};
use thiserror::Error;
use tokio::sync::oneshot;

pub type EventTx = async_channel::Sender<Event>;
pub type EventRx = async_channel::Receiver<Event>;
pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug)]
pub struct Player {
    tx: RequestTx,
}

#[derive(Debug)]
pub struct Receiver(EventRx);

#[derive(Debug, Clone, Error)]
pub enum Error {
    #[error("error sending or receiving")]
    Channel,
    #[error("player queue is empty")]
    QueueEmpty,
    #[error("unable to open track")]
    InvalidTrack,
}

#[derive(Debug, Clone, Copy)]
pub enum Event {
    PlaybackStarted,
    PlaybackPaused,
    PlaybackStopped,
    TrackAdded(QueueId),
    TrackStarted(QueueId),
    TrackFinished(QueueId),
}

pub type QueueId = usize;

type RequestTx = async_channel::Sender<RequestEnvelope>;
type RequestRx = async_channel::Receiver<RequestEnvelope>;
type ResponseTx = oneshot::Sender<Result<()>>;
type StartupTx = oneshot::Sender<AnyResult<()>>;
type OpenTrack = Decoder<BufReader<File>>;
type Callback = EmptyCallback<f32>;

struct RequestEnvelope {
    tx: ResponseTx,
    payload: Request,
}

#[derive(Debug)]
enum Request {
    Play,
    Pause,
    PlayToggle,
    Stop,
    Skip,
    Queue { id: QueueId, path: Utf8PathBuf },
}

struct Worker {
    sink: Sink,
    tx: EventTx,
}

impl Player {
    pub async fn new() -> AnyResult<(Self, Receiver)> {
        let (tx, rx) = async_channel::unbounded();
        let (etx, erx) = async_channel::unbounded();
        let (stx, srx) = oneshot::channel();
        thread::spawn(move || Worker::run(stx, etx, &rx));
        srx.await??;
        Ok((Self { tx }, Receiver(erx)))
    }

    pub async fn play(&self) -> Result<()> {
        self.request(Request::Play).await
    }

    pub async fn pause(&self) -> Result<()> {
        self.request(Request::Pause).await
    }

    pub async fn play_toggle(&self) -> Result<()> {
        self.request(Request::PlayToggle).await
    }

    pub async fn stop(&self) -> Result<()> {
        self.request(Request::Stop).await
    }

    pub async fn skip(&self) -> Result<()> {
        self.request(Request::Skip).await
    }

    pub async fn queue(&self, id: QueueId, path: Utf8PathBuf) -> Result<QueueId> {
        self.request(Request::Queue { id, path }).await?;
        Ok(id)
    }

    async fn request(&self, payload: Request) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(RequestEnvelope { tx, payload })
            .await
            .map_err(|_| Error::Channel)?;
        rx.await.map_err(|_| Error::Channel)?
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        tracing::info!("drop player");
        self.tx.close();
    }
}

impl Receiver {
    pub async fn recv(&self) -> Option<Event> {
        match self.0.recv().await {
            Ok(x) => Some(x),
            Err(_) => None,
        }
    }
}

impl Worker {
    fn run(stx: StartupTx, tx: EventTx, rx: &RequestRx) {
        let (_stream, sink) = match Self::init() {
            Ok(x) => x,
            Err(e) => {
                stx.send(Err(e)).unwrap();
                return;
            }
        };
        let worker = Worker { sink, tx };
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

    fn serve_forever(&self, rx: &RequestRx) {
        loop {
            let Ok(RequestEnvelope { tx, payload }) = rx.recv_blocking() else {
                return;
            };
            tracing::info!(request=?payload, "player request");
            tx.send(self.request_handler(payload)).ok();
        }
    }

    fn request_handler(&self, payload: Request) -> Result<()> {
        match payload {
            Request::Play => {
                if self.sink.empty() {
                    return Err(Error::QueueEmpty);
                }
                if self.sink.is_paused() {
                    self.send(Event::PlaybackStarted);
                }
                self.sink.play();
            }
            Request::Pause => {
                if self.sink.empty() {
                    return Err(Error::QueueEmpty);
                }
                if !self.sink.is_paused() {
                    self.send(Event::PlaybackPaused);
                }
                self.sink.pause();
            }
            Request::PlayToggle => {
                if self.sink.empty() {
                    return Err(Error::QueueEmpty);
                }
                if self.sink.is_paused() {
                    self.send(Event::PlaybackStarted);
                    self.sink.play();
                } else {
                    self.send(Event::PlaybackPaused);
                    self.sink.pause();
                }
            }
            Request::Stop => {
                if !self.sink.empty() {
                    self.send(Event::PlaybackStopped);
                }
                self.sink.clear();
            }
            Request::Skip => {
                if self.sink.empty() {
                    return Err(Error::QueueEmpty);
                }
                self.sink.skip_one();
            }
            Request::Queue { id, path } => {
                let track = open_track(path).map_err(|_| Error::InvalidTrack)?;
                self.send(Event::TrackAdded(id));
                if !self.sink.is_paused() && self.sink.empty() {
                    self.send(Event::PlaybackStarted);
                }
                self.sink.append(cb_track_started(self.tx.clone(), id));
                self.sink.append(track);
                self.sink.append(cb_track_finished(self.tx.clone(), id));
            }
        };
        Ok(())
    }

    fn send(&self, e: Event) {
        self.tx.send_blocking(e).unwrap();
    }
}

fn open_track(path: Utf8PathBuf) -> AnyResult<OpenTrack> {
    Ok(Decoder::new(BufReader::new(File::open(path)?))?)
}

fn cb_track_started(tx: EventTx, id: QueueId) -> Callback {
    cb_source(move || {
        tx.send_blocking(Event::TrackStarted(id)).ok();
    })
}

fn cb_track_finished(tx: EventTx, id: QueueId) -> Callback {
    cb_source(move || {
        tx.send_blocking(Event::TrackFinished(id)).ok();
    })
}

fn cb_source<F>(f: F) -> Callback
where
    F: Fn() + Send + 'static,
{
    EmptyCallback::new(Box::new(f))
}
