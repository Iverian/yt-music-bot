use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use anyhow::Result as AnyResult;
use camino::Utf8PathBuf;
use tokio::sync::{mpsc, Mutex};

use crate::youtube::{Error as YoutubeError, TrackId, Youtube};

const MAX_TRACKS: usize = 5; // TODO: extract into settings

#[derive(Debug)]
pub struct Manager {
    tx: RequestTx,
}

#[derive(Debug, Clone)]
pub enum Event {
    Acquired {
        id: TrackId,
        err: Option<YoutubeError>,
    },
    Removed {
        id: TrackId,
    },
}

#[derive(Debug)]
pub struct Receiver(EventRx);

type RequestTx = async_channel::Sender<Request>;
type RequestRx = async_channel::Receiver<Request>;
type EventTx = mpsc::UnboundedSender<Event>;
type EventRx = mpsc::UnboundedReceiver<Event>;

struct Worker {
    tx: EventTx,
    downloader: Youtube,
    state: HashMap<TrackId, TrackLock>,
    queue: VecDeque<TrackId>,
    tracks: usize,
}

type TrackLock = Arc<Mutex<TrackState>>;

struct TrackState {
    path: Option<Utf8PathBuf>,
    claims: usize,
}

#[derive(Debug)]
enum Request {
    Claim { id: TrackId },
    Unclaim { id: TrackId },
    Clear,
}

impl Manager {
    pub fn new(downloader: Youtube) -> (Manager, Receiver) {
        let (tx, rx) = async_channel::unbounded();
        let (etx, erx) = mpsc::unbounded_channel();
        let w = Worker {
            tx: etx,
            downloader,
            state: HashMap::new(),
            queue: VecDeque::new(),
            tracks: 0,
        };
        tokio::task::spawn(w.serve_forever(rx));
        (Self { tx }, Receiver(erx))
    }

    pub async fn claim(&self, id: TrackId) -> AnyResult<()> {
        self.tx.send(Request::Claim { id }).await?;
        Ok(())
    }

    pub async fn unclaim(&self, id: TrackId) -> AnyResult<()> {
        self.tx.send(Request::Unclaim { id }).await?;
        Ok(())
    }

    pub async fn clear(&self) -> AnyResult<()> {
        self.tx.send(Request::Clear).await?;
        Ok(())
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        self.tx.close();
    }
}

impl Worker {
    async fn serve_forever(mut self, rx: RequestRx) {
        while let Ok(req) = rx.recv().await {
            self.request_handler(req).await;
        }
    }

    async fn request_handler(&mut self, req: Request) {
        tracing::debug!(request = ?req, "track manager request");
        match req {
            Request::Claim { id } => self.claim(id).await,
            Request::Unclaim { id } => self.unclaim(id).await,
            Request::Clear => self.clear().await,
        }
    }

    async fn claim(&mut self, id: TrackId) {
        if let Some(lock) = self.state.get(&id) {
            let mut state = lock.lock().await;
            state.claim();
            if state.needs_download() {
                if self.tracks >= MAX_TRACKS {
                    self.queue.push_back(id);
                } else {
                    self.tracks += 1;
                    self.download(id, lock.clone());
                }
            } else if state.present() {
                self.tx.send(Event::Acquired { id, err: None }).ok();
            }
        } else {
            let lock = TrackState::new_lock(1);
            if self.tracks >= MAX_TRACKS {
                self.state.insert(id.clone(), lock);
                self.queue.push_back(id);
            } else {
                self.tracks += 1;
                self.state.insert(id.clone(), lock.clone());
                self.download(id, lock);
            }
        }
    }

    async fn unclaim(&mut self, id: TrackId) {
        let Some(lock) = self.state.get(&id) else {
            return;
        };

        let mut state = lock.lock().await;
        state.unclaim();
        if !state.can_remove() {
            return;
        }

        self.tx.send(Event::Removed { id }).ok();
        state.remove().await;

        if let Some(id) = self.queue.pop_front() {
            let lock = self.state[&id].clone();
            self.download(id, lock);
        } else {
            self.tracks -= 1;
        }
    }

    async fn clear(&mut self) {
        for lock in self.state.values() {
            let mut state = lock.lock().await;
            state.remove().await;
        }
        self.state.clear();
    }

    fn download(&self, id: TrackId, lock: TrackLock) {
        let tx = self.tx.clone();
        let downloader = self.downloader.clone();
        tokio::task::spawn(async move {
            let path = match downloader.download_by_id(&id).await {
                Ok(x) => x,
                Err(e) => {
                    tx.send(Event::Acquired { id, err: Some(e) }).ok();
                    return;
                }
            };
            if lock.lock().await.try_set_path(path).await {
                tx.send(Event::Acquired { id, err: None }).ok();
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
        self.claims == 0 && self.path.is_none()
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
