use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use camino::Utf8PathBuf;
use tokio::sync::{mpsc, Mutex};

use crate::player::OriginId;
use crate::util::channel::ChannelResult;
use crate::youtube::{Error as YoutubeError, TrackId, Youtube};

#[derive(Debug)]
pub struct TrackManager {
    tx: RequestTx,
}

#[derive(Debug, Clone)]
pub enum Event {
    Acquired {
        origin_id: OriginId,
        id: TrackId,
        err: Option<YoutubeError>,
    },
    Removed {
        id: TrackId,
    },
}

#[derive(Debug)]
pub struct Receiver(EventRx);

type RequestTx = mpsc::UnboundedSender<Request>;
type RequestRx = mpsc::UnboundedReceiver<Request>;
type EventTx = mpsc::UnboundedSender<Event>;
type EventRx = mpsc::UnboundedReceiver<Event>;

struct Worker {
    tx: EventTx,
    downloader: Youtube,
    cache_size: usize,
    state: State,
}

#[derive(Default)]
struct State {
    tracks: HashMap<TrackId, TrackLock>,
    queue: VecDeque<(OriginId, TrackId)>,
    size: usize,
}

type TrackLock = Arc<Mutex<TrackState>>;

#[derive(Debug)]
struct TrackState {
    path: Option<Utf8PathBuf>,
    claims: usize,
}

#[derive(Debug)]
enum Request {
    Claim { origin_id: OriginId, id: TrackId },
    Unclaim { id: TrackId },
    Clear,
}

impl TrackManager {
    pub fn new(downloader: Youtube, cache_size: usize) -> (TrackManager, Receiver) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (etx, erx) = mpsc::unbounded_channel();
        let w = Worker {
            tx: etx,
            downloader,
            state: State::default(),
            cache_size: 1 + cache_size,
        };
        tokio::task::spawn(w.serve_forever(rx));
        (Self { tx }, Receiver(erx))
    }

    pub fn claim(&self, origin_id: OriginId, id: TrackId) -> ChannelResult<()> {
        self.tx.send(Request::Claim { origin_id, id })?;
        Ok(())
    }

    pub fn unclaim(&self, id: TrackId) -> ChannelResult<()> {
        self.tx.send(Request::Unclaim { id })?;
        Ok(())
    }

    pub fn clear(&self) -> ChannelResult<()> {
        self.tx.send(Request::Clear)?;
        Ok(())
    }
}

impl Worker {
    async fn serve_forever(mut self, mut rx: RequestRx) {
        while let Some(req) = rx.recv().await {
            self.request_handler(req).await;
        }
    }

    async fn request_handler(&mut self, req: Request) {
        tracing::debug!(request = ?req, "track manager request");
        match req {
            Request::Claim { origin_id, id } => self.claim(origin_id, id).await,
            Request::Unclaim { id } => self.unclaim(id).await,
            Request::Clear => self.clear().await,
        }
    }

    async fn claim(&mut self, origin_id: OriginId, id: TrackId) {
        if let Some(lock) = self.state.tracks.get(&id) {
            let mut state = lock.lock().await;
            tracing::info!(track_id = id, state = ?state, "existing claim");
            state.claim();
            if state.needs_download() {
                if self.state.size >= self.cache_size {
                    self.state.queue.push_back((origin_id, id));
                } else {
                    self.state.size += 1;
                    self.download(origin_id, id, lock.clone());
                }
            } else if state.present() {
                self.tx
                    .send(Event::Acquired {
                        origin_id,
                        id,
                        err: None,
                    })
                    .ok();
            }
        } else {
            let lock = TrackState::new_lock(1);
            if self.state.size >= self.cache_size {
                self.state.tracks.insert(id.clone(), lock);
                self.state.queue.push_back((origin_id, id));
            } else {
                self.state.size += 1;
                self.state.tracks.insert(id.clone(), lock.clone());
                self.download(origin_id, id, lock);
            }
        }
    }

    async fn unclaim(&mut self, id: TrackId) {
        let Some(lock) = self.state.tracks.get(&id) else {
            return;
        };

        let mut state = lock.lock().await;
        state.unclaim();
        if !state.can_remove() {
            return;
        }

        self.tx.send(Event::Removed { id }).ok();
        state.remove().await;

        if let Some((origin_id, id)) = self.state.queue.pop_front() {
            let lock = self.state.tracks[&id].clone();
            self.download(origin_id, id, lock);
        } else {
            self.state.size -= 1;
        }
    }

    async fn clear(&mut self) {
        for lock in self.state.tracks.values() {
            let mut state = lock.lock().await;
            state.remove().await;
        }
        self.state.tracks.clear();
        self.state.queue.clear();
        self.state.size = 0;
    }

    fn download(&self, origin_id: OriginId, id: TrackId, lock: TrackLock) {
        let tx = self.tx.clone();
        let downloader = self.downloader.clone();
        tokio::task::spawn(async move {
            let path = match downloader.download_by_id(&id).await {
                Ok(x) => x,
                Err(e) => {
                    tx.send(Event::Acquired {
                        origin_id,
                        id,
                        err: Some(e),
                    })
                    .ok();
                    return;
                }
            };
            if lock.lock().await.try_set_path(path).await {
                tx.send(Event::Acquired {
                    origin_id,
                    id,
                    err: None,
                })
                .ok();
            }
        });
    }
}

impl Receiver {
    pub async fn recv(&mut self) -> Option<Event> {
        self.0.recv().await
    }
}

impl TrackState {
    fn new_lock(claims: usize) -> TrackLock {
        Arc::new(Mutex::new(TrackState { path: None, claims }))
    }

    fn needs_download(&self) -> bool {
        self.claims <= 1 && self.path.is_none()
    }

    fn present(&self) -> bool {
        self.path.is_some()
    }

    fn can_remove(&self) -> bool {
        self.claims == 0 && self.path.is_some()
    }

    async fn try_set_path(&mut self, path: Utf8PathBuf) -> bool {
        let has_claims = self.claims != 0;
        if has_claims {
            self.path = Some(path);
        } else {
            tracing::debug!(path = ?path, "remove track");
            tokio::fs::remove_file(path).await.ok();
        }
        has_claims
    }

    async fn remove(&mut self) {
        if let Some(path) = &self.path {
            tracing::debug!(path = ?path, "remove track");
            tokio::fs::remove_file(path).await.ok();
        }
        self.path = None;
        self.claims = 0;
    }

    fn claim(&mut self) {
        self.claims += 1;
    }

    fn unclaim(&mut self) {
        if self.claims == 0 {
            return;
        }
        self.claims -= 1;
    }
}
