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
        id: TrackId,
    },
    Removed {
        id: TrackId,
        e: Option<(OriginId, YoutubeError)>,
    },
}

#[derive(Debug)]
pub struct Receiver(EventRx);

type RequestTx = mpsc::UnboundedSender<Request>;
type RequestRx = mpsc::UnboundedReceiver<Request>;
type EventTx = mpsc::UnboundedSender<Event>;
type EventRx = mpsc::UnboundedReceiver<Event>;

#[derive(Clone)]
struct Worker(Arc<State>);

struct State {
    tx: EventTx,
    youtube: Youtube,
    cache_size: usize,
    data: Mutex<Data>,
}

#[derive(Default)]
struct Data {
    tracks: HashMap<TrackId, TrackData>,
    queue: VecDeque<(OriginId, TrackId)>,
    size: usize,
}

#[derive(Debug, Default)]
struct TrackData {
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
    pub fn new(youtube: Youtube, cache_size: usize) -> (TrackManager, Receiver) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (etx, erx) = mpsc::unbounded_channel();
        let w = Worker::new(etx, youtube, cache_size);
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
    fn new(tx: EventTx, youtube: Youtube, cache_size: usize) -> Self {
        Self(Arc::new(State {
            tx,
            youtube,
            cache_size: 1 + cache_size,
            data: Mutex::new(Data::default()),
        }))
    }
    async fn serve_forever(self, mut rx: RequestRx) -> ChannelResult<()> {
        while let Some(req) = rx.recv().await {
            self.request_handler(req).await?;
        }
        Ok(())
    }

    async fn request_handler(&self, req: Request) -> ChannelResult<()> {
        tracing::debug!(request = ?req, "track manager request");
        match req {
            Request::Claim { origin_id, id } => {
                self.0.claim(origin_id, id).await?;
            }
            Request::Unclaim { id } => {
                self.0.unclaim(id, None).await?;
            }
            Request::Clear => {
                self.0.clear().await;
            }
        }
        Ok(())
    }
}

impl State {
    async fn claim(self: &Arc<Self>, origin_id: OriginId, id: TrackId) -> ChannelResult<()> {
        let mut data = self.data.lock().await;
        if let Some(track) = data.tracks.get_mut(&id) {
            track.claim();
            if track.needs_download() {
                self.try_download(&mut data, origin_id, id);
            } else if track.present() {
                self.tx.send(Event::Acquired { id })?;
            }
        } else {
            let mut track = TrackData::default();
            track.claim();
            data.tracks.insert(id.clone(), track);
            self.try_download(&mut data, origin_id, id);
        }
        Ok(())
    }

    async fn unclaim(
        self: &Arc<Self>,
        id: TrackId,
        e: Option<(OriginId, YoutubeError)>,
    ) -> ChannelResult<()> {
        let mut data = self.data.lock().await;
        let Some(track) = data.tracks.get_mut(&id) else {
            return Ok(());
        };
        track.unclaim();
        if !track.can_remove() {
            return Ok(());
        }
        track.remove().await;
        self.rotate_track(&mut data, &id);
        self.tx.send(Event::Removed { id, e })?;
        Ok(())
    }

    async fn clear(self: &Arc<Self>) {
        self.data.lock().await.clear();
    }

    fn download(self: &Arc<Self>, origin_id: OriginId, id: TrackId) {
        tokio::spawn(self.clone().download_impl(origin_id, id));
    }

    async fn download_impl(self: Arc<Self>, origin_id: OriginId, id: TrackId) {
        let path = match self.youtube.download_by_id(&id).await {
            Ok(x) => x,
            Err(e) => {
                self.download_error(origin_id, id, e).await.ok();
                return;
            }
        };
        let mut data = self.data.lock().await;
        let track = data.tracks.get_mut(&id).unwrap();
        track.try_set_path(path).await;
        self.tx.send(Event::Acquired { id }).ok();
    }

    async fn download_error(
        self: &Arc<Self>,
        origin_id: OriginId,
        id: TrackId,
        e: YoutubeError,
    ) -> ChannelResult<()> {
        let mut data = self.data.lock().await;
        self.rotate_track(&mut data, &id);
        self.tx.send(Event::Removed {
            id,
            e: Some((origin_id, e)),
        })?;
        Ok(())
    }

    fn try_download(self: &Arc<Self>, data: &mut Data, origin_id: usize, id: String) {
        if data.size == self.cache_size {
            data.queue.push_back((origin_id, id));
        } else {
            data.size += 1;
            self.download(origin_id, id);
        }
    }

    fn rotate_track(self: &Arc<Self>, data: &mut Data, id: &String) {
        data.tracks.remove(id);
        if let Some((origin_id, id)) = data.queue.pop_front() {
            self.download(origin_id, id);
        } else {
            data.size -= 1;
        }
    }
}

impl Data {
    fn clear(&mut self) {
        self.queue.clear();
        self.tracks.clear();
        self.size = 0;
    }
}

impl Receiver {
    pub async fn recv(&mut self) -> Option<Event> {
        self.0.recv().await
    }
}

impl TrackData {
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
