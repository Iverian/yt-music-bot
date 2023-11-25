use std::collections::btree_map::Iter as BTreeIter;
use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result as AnyResult;
use atomic_counter::{AtomicCounter, ConsistentCounter};
use itertools::Itertools;
use tokio::sync::{broadcast, RwLock, RwLockReadGuard};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::player::{
    Event as PlayerEvent, Player, QueueId, Receiver as PlayerReceiver, Result as PlayerResult,
};
use crate::track_manager::{Event as ManagerEvent, Manager, Receiver as ManagerReceiver};
use crate::util::Handle;
use crate::youtube::{Error as YoutubeError, Track, TrackId, Youtube};

const CONTROLLER_QSIZE: usize = 8;

#[derive(Debug, Clone)]
pub struct Controller {
    tx: EventTx,
    yt: Youtube,
    state: Arc<State>,
}

#[derive(Debug)]
pub struct Receiver(EventRx);

#[derive(Debug, Clone)]
pub enum Event {
    PlaybackStarted,
    PlaybackPaused,
    PlaybackStopped,
    QueueFinished,
    NowPlaying { queue_id: QueueId, track: Track },
    DownloadError { err: YoutubeError },
}

#[derive(Debug, Clone)]
pub struct QueuedTrack {
    pub queue_id: QueueId,
    pub track: Track,
}

// TODO: handle wraparound
#[derive(Debug)]
pub struct Data {
    pub queue: Map<QueueId, QueueData>,
    pub tracks: Map<TrackId, TrackData>,
    counter: ConsistentCounter,
    player_queue_size: usize,
    auto_play: bool,
}

pub struct DataIterator<'a> {
    parent: &'a Data,
    iter: BTreeIter<'a, QueueId, QueueData>,
}

#[derive(Debug)]
pub struct TrackData {
    pub meta: Track,
    pub ready: bool,
}

#[derive(Debug)]
pub struct QueueData {
    pub track_id: TrackId,
    pub ready: bool,
}

type EventTx = broadcast::Sender<Event>;
type EventRx = broadcast::Receiver<Event>;
type Map<K, V> = BTreeMap<K, V>;

#[derive(Debug)]
struct State {
    player: Player,
    manager: Manager,
    data: RwLock<Data>,
}

impl Controller {
    pub fn new(
        token: CancellationToken,
        player: Player,
        player_receiver: PlayerReceiver,
        youtube: Youtube,
        auto_play: bool,
    ) -> (Self, Handle) {
        let (tx, _) = broadcast::channel(CONTROLLER_QSIZE);
        let (manager, manager_receiver) = Manager::new(youtube.clone());
        let state = State::new(player, manager, auto_play);
        let handle = tokio::spawn(State::serve_forever(
            state.clone(),
            token,
            tx.clone(),
            player_receiver,
            manager_receiver,
        ));
        let controller = Self {
            tx,
            yt: youtube,
            state,
        };

        (controller, handle)
    }

    pub fn subscribe(&self) -> Receiver {
        Receiver(self.tx.subscribe())
    }

    pub async fn queue(&self, urls: Vec<Url>) -> AnyResult<Vec<QueuedTrack>> {
        self.state.queue(self.yt.resolve(urls).await?).await
    }

    pub async fn play(&self) -> PlayerResult<()> {
        self.state.player.play().await
    }

    pub async fn pause(&self) -> PlayerResult<()> {
        self.state.player.pause().await
    }

    pub async fn play_toggle(&self) -> PlayerResult<()> {
        self.state.player.play_toggle().await
    }

    pub async fn stop(&self) -> PlayerResult<()> {
        self.state.player.stop().await
    }

    pub async fn skip(&self) -> PlayerResult<()> {
        self.state.player.skip().await
    }

    pub async fn view(&self) -> RwLockReadGuard<'_, Data> {
        self.state.data.read().await
    }
}

impl Receiver {
    pub async fn recv(&mut self) -> Option<Event> {
        self.0.recv().await.ok()
    }
}

impl State {
    fn new(player: Player, manager: Manager, auto_play: bool) -> Arc<Self> {
        Arc::new(Self {
            player,
            manager,
            data: RwLock::new(Data::new(auto_play)),
        })
    }

    async fn queue(&self, tracks: Vec<Track>) -> AnyResult<Vec<QueuedTrack>> {
        let mut data = self.data.write().await;
        let result = data.queue(&self.manager, tracks).await?;
        Ok(result)
    }

    async fn serve_forever_impl(
        &self,
        token: CancellationToken,
        tx: EventTx,
        prx: PlayerReceiver,
        mut mrx: ManagerReceiver,
    ) -> AnyResult<()> {
        loop {
            let e = tokio::select! {
                oe = prx.recv() => {
                    let Some(e) = oe else {
                        break;
                    };
                    self.handle_player_event(e).await?

                }
                oe = mrx.recv() => {
                    let Some(e) = oe else {
                        break;
                    };
                    self.handle_track_manager_event(e).await?
                }
                _ = token.cancelled() => {
                    return Ok(());
                }
            };
            if let Some(e) = e {
                tx.send(e)?;
            }
        }
        Ok(())
    }

    async fn handle_player_event(&self, e: PlayerEvent) -> AnyResult<Option<Event>> {
        tracing::debug!(event = ?e, "player event");
        Ok(match e {
            PlayerEvent::PlaybackStarted => Some(Event::PlaybackStarted),
            PlayerEvent::PlaybackPaused => Some(Event::PlaybackPaused),
            PlayerEvent::PlaybackStopped => {
                let mut data = self.data.write().await;
                data.stop(&self.manager).await?;
                Some(Event::PlaybackStopped)
            }
            PlayerEvent::TrackAdded(queue_id) => {
                let mut data = self.data.write().await;
                data.ready_queue(queue_id);
                None
            }
            PlayerEvent::TrackStarted(queue_id) => {
                let mut data = self.data.write().await;
                data.queue_player(&self.player).await?;
                Some(Event::NowPlaying {
                    queue_id,
                    track: data.get_queued_track(queue_id).meta.clone(),
                })
            }
            PlayerEvent::TrackFinished(queue_id) => {
                let mut data = self.data.write().await;
                data.remove_queue_player(&self.manager, queue_id).await?;
                if data.player_queue_size == 0 {
                    Some(Event::QueueFinished)
                } else {
                    None
                }
            }
        })
    }

    async fn handle_track_manager_event(&self, e: ManagerEvent) -> AnyResult<Option<Event>> {
        tracing::debug!(event = ?e, "track manager event");
        Ok(match e {
            ManagerEvent::Acquired { id, err } => {
                let mut data = self.data.write().await;
                if let Some(err) = err {
                    data.remove_track(&id);
                    Some(Event::DownloadError { err })
                } else {
                    data.ready_track(&id);
                    data.queue_player(&self.player).await?;
                    None
                }
            }
            ManagerEvent::Removed { id } => {
                let mut data = self.data.write().await;
                data.unready_track(&id);
                None
            }
        })
    }

    async fn serve_forever(
        state: Arc<Self>,
        token: CancellationToken,
        tx: EventTx,
        prx: PlayerReceiver,
        mrx: ManagerReceiver,
    ) -> AnyResult<()> {
        state.serve_forever_impl(token, tx, prx, mrx).await
    }
}

impl Data {
    pub fn new(auto_play: bool) -> Self {
        Self {
            queue: Map::new(),
            tracks: Map::new(),
            counter: ConsistentCounter::default(),
            player_queue_size: 0,
            auto_play,
        }
    }

    pub fn iter(&self) -> DataIterator<'_> {
        DataIterator {
            parent: self,
            iter: self.queue.iter(),
        }
    }

    fn remove_track(&mut self, id: &TrackId) {
        let to_remove = self
            .queue
            .iter()
            .filter_map(|(k, v)| if &v.track_id == id { Some(*k) } else { None })
            .collect_vec();
        for i in to_remove {
            self.queue.remove(&i);
        }
        self.tracks.remove(id);
    }

    fn ready_track(&mut self, id: &TrackId) {
        self.tracks.get_mut(id).unwrap().set_ready(true);
    }

    fn unready_track(&mut self, id: &TrackId) {
        self.tracks.get_mut(id).unwrap().set_ready(false);
    }

    fn ready_queue(&mut self, id: QueueId) {
        self.queue.get_mut(&id).unwrap().set_ready(true);
        self.player_queue_size += 1;
    }

    fn get_queued_track(&self, id: QueueId) -> &TrackData {
        let url = &self.queue.get(&id).unwrap().track_id;
        self.tracks.get(url).unwrap()
    }

    async fn queue(
        &mut self,
        manager: &Manager,
        tracks: Vec<Track>,
    ) -> AnyResult<Vec<QueuedTrack>> {
        let mut result = Vec::with_capacity(tracks.len());

        for track in tracks {
            manager.claim(track.id.clone()).await?;
            let queue_id = self.counter.inc();
            self.queue.insert(
                queue_id,
                QueueData {
                    track_id: track.id.clone(),
                    ready: false,
                },
            );
            if !self.tracks.contains_key(&track.id) {
                self.tracks.insert(
                    track.id.clone(),
                    TrackData {
                        meta: track.clone(),
                        ready: false,
                    },
                );
            }
            result.push(QueuedTrack { queue_id, track });
        }

        Ok(result)
    }

    async fn queue_player(&mut self, player: &Player) -> PlayerResult<()> {
        let Some((queue_id, _)) = self.queue.iter().find(|(_, x)| !x.ready) else {
            return Ok(());
        };

        let track = self.get_queued_track(*queue_id);
        if !track.ready {
            return Ok(());
        }

        player.queue(*queue_id, track.meta.path.clone()).await?;
        if self.auto_play && self.player_queue_size == 0 {
            tracing::debug!("requesting auto play");
            player.play().await?;
        }

        Ok(())
    }

    async fn remove_queue_player(&mut self, manager: &Manager, id: QueueId) -> AnyResult<()> {
        let Some(x) = self.queue.remove(&id) else {
            return Ok(());
        };
        manager.unclaim(x.track_id).await?;
        self.player_queue_size -= 1;
        Ok(())
    }

    async fn stop(&mut self, manager: &Manager) -> AnyResult<()> {
        manager.clear().await?;
        self.queue.clear();
        self.tracks.clear();
        self.player_queue_size = 0;
        self.counter.reset();
        Ok(())
    }
}

impl<'a> IntoIterator for &'a Data {
    type Item = <DataIterator<'a> as Iterator>::Item;
    type IntoIter = DataIterator<'a>;

    fn into_iter(self) -> Self::IntoIter {
        todo!()
    }
}

impl<'a> Iterator for DataIterator<'a> {
    type Item = (&'a QueueId, &'a Track);

    fn next(&mut self) -> Option<Self::Item> {
        let (queue_id, QueueData { track_id, ready: _ }) = self.iter.next()?;
        let track = &self.parent.tracks.get(track_id).unwrap().meta;
        Some((queue_id, track))
    }
}

impl TrackData {
    fn set_ready(&mut self, value: bool) {
        self.ready = value;
    }
}

impl QueueData {
    fn set_ready(&mut self, value: bool) {
        self.ready = value;
    }
}
