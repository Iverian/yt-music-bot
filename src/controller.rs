use std::collections::btree_map::Iter as BTreeIter;
use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result as AnyResult;
use atomic_counter::{AtomicCounter, ConsistentCounter, RelaxedCounter};
use itertools::Itertools;
use thiserror::Error;
use tokio::sync::{broadcast, RwLock, RwLockReadGuard};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::player::{
    Event as PlayerEvent, EventKind as PlayerEventKind, OriginId, Player, QueueId,
    Receiver as PlayerReceiver, Result as PlayerResult,
};
use crate::track_manager::{Event as ManagerEvent, Receiver as ManagerReceiver, TrackManager};
use crate::util::cancel::Handle;
use crate::util::channel::{ChannelError, ChannelResult};
use crate::youtube::{Error as YoutubeError, Track, TrackId, Youtube};

const BROADCAST_QUEUE_SIZE: usize = 8;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Clone)]
pub struct Controller {
    tx: EventTx,
    state: Arc<State>,
}

#[derive(Debug, Clone)]
pub struct Sender {
    origin_id: OriginId,
    state: Arc<State>,
}

#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub auto_play: bool,
    pub track_cache_size: usize,
}

#[derive(Debug, Clone, Error)]
pub enum Error {
    #[error(transparent)]
    Youtube(#[from] YoutubeError),
    #[error(transparent)]
    Channel(#[from] ChannelError),
}

#[derive(Debug)]
pub struct Receiver {
    origin_id: OriginId,
    rx: EventRx,
}

#[derive(Debug, Clone)]
pub enum Event {
    PlaybackStarted,
    PlaybackPaused,
    PlaybackStopped,
    QueueFinished,
    WaitingForDownload,
    Muted,
    Unmuted,
    Volume { level: u8 },
    NowPlaying { queue_id: QueueId, track: Track },
    DownloadError { track: Track, err: YoutubeError },
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
    queue_counter: ConsistentCounter,
    origin_counter: RelaxedCounter,
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
    origin_id: OriginId,
}

type EventTx = broadcast::Sender<EventWrapper>;
type EventRx = broadcast::Receiver<EventWrapper>;
type Map<K, V> = BTreeMap<K, V>;

#[derive(Clone)]
struct EventWrapper {
    origin_id: Option<OriginId>,
    event: Event,
}

#[derive(Debug)]
struct State {
    youtube: Youtube,
    player: Player,
    manager: TrackManager,
    data: RwLock<Data>,
}

#[derive(Clone)]
struct Broadcast(EventTx);

impl Controller {
    pub fn new(
        token: CancellationToken,
        player: Player,
        player_receiver: PlayerReceiver,
        youtube: Youtube,
        settings: Settings,
    ) -> (Self, Handle) {
        let (tx, _) = broadcast::channel(BROADCAST_QUEUE_SIZE);
        let (manager, manager_receiver) =
            TrackManager::new(youtube.clone(), settings.track_cache_size);
        let state = State::new(player, manager, youtube, settings.auto_play);
        let handle = tokio::spawn(State::serve_forever(
            state.clone(),
            token,
            Broadcast(tx.clone()),
            player_receiver,
            manager_receiver,
        ));
        let controller = Self { tx, state };

        (controller, handle)
    }

    pub async fn subscribe(&self) -> (Sender, Receiver) {
        let origin_id = self.state.data.write().await.origin_counter.inc();
        let proxy = Sender {
            origin_id,
            state: self.state.clone(),
        };
        let receiver = Receiver {
            origin_id,
            rx: self.tx.subscribe(),
        };
        (proxy, receiver)
    }
}

impl Sender {
    pub async fn queue(&self, urls: Vec<Url>) -> Result<Vec<QueuedTrack>> {
        let tracks = self
            .state
            .youtube
            .resolve(urls)
            .await
            .map_err(|e| match e {
                YoutubeError::Channel(e) => Error::Channel(e),
                e => Error::Youtube(e),
            })?;
        self.state.queue(self.origin_id, tracks).await
    }

    pub async fn play(&self) -> PlayerResult<()> {
        self.state.player.play(self.origin_id).await
    }

    pub async fn pause(&self) -> PlayerResult<()> {
        self.state.player.pause(self.origin_id).await
    }

    pub async fn play_toggle(&self) -> PlayerResult<()> {
        self.state.player.play_toggle(self.origin_id).await
    }

    pub async fn stop(&self) -> PlayerResult<()> {
        self.state.player.stop(self.origin_id).await
    }

    pub async fn skip(&self) -> PlayerResult<()> {
        self.state.player.skip(self.origin_id).await
    }

    pub async fn mute(&self) -> PlayerResult<()> {
        self.state.player.mute(self.origin_id).await
    }

    pub async fn unmute(&self) -> PlayerResult<()> {
        self.state.player.unmute(self.origin_id).await
    }

    pub async fn mute_toggle(&self) -> PlayerResult<()> {
        self.state.player.mute_toggle(self.origin_id).await
    }

    pub async fn get_volume(&self) -> PlayerResult<()> {
        self.state.player.get_volume(self.origin_id).await
    }

    pub async fn set_volume(&self, level: u8) -> PlayerResult<()> {
        self.state.player.set_volume(self.origin_id, level).await
    }

    pub async fn increase_volume(&self) -> PlayerResult<()> {
        self.state.player.increase_volume(self.origin_id).await
    }

    pub async fn decrease_volume(&self) -> PlayerResult<()> {
        self.state.player.decrease_volume(self.origin_id).await
    }

    pub async fn view(&self) -> RwLockReadGuard<'_, Data> {
        self.state.data.read().await
    }
}

impl Broadcast {
    fn broadcast(&self, event: Event) -> ChannelResult<()> {
        self.0.send(EventWrapper {
            origin_id: None,
            event,
        })?;
        Ok(())
    }

    fn direct(&self, origin_id: OriginId, event: Event) -> ChannelResult<()> {
        self.0.send(EventWrapper {
            origin_id: Some(origin_id),
            event,
        })?;
        Ok(())
    }
}

impl Receiver {
    pub async fn recv(&mut self) -> Option<Event> {
        loop {
            match self.rx.recv().await {
                Ok(EventWrapper {
                    origin_id: Some(id),
                    event,
                }) if id == self.origin_id => {
                    return Some(event);
                }
                Ok(EventWrapper {
                    origin_id: None,
                    event,
                }) => {
                    return Some(event);
                }
                Err(_) => {
                    return None;
                }
                _ => {}
            }
        }
    }
}

impl State {
    fn new(player: Player, manager: TrackManager, youtube: Youtube, auto_play: bool) -> Arc<Self> {
        Arc::new(Self {
            player,
            manager,
            youtube,
            data: RwLock::new(Data::new(auto_play)),
        })
    }

    async fn queue(&self, origin_id: OriginId, tracks: Vec<Track>) -> Result<Vec<QueuedTrack>> {
        let mut data = self.data.write().await;
        let result = data.queue(origin_id, &self.manager, tracks)?;
        Ok(result)
    }

    async fn serve_forever_impl(
        &self,
        token: CancellationToken,
        tx: Broadcast,
        mut prx: PlayerReceiver,
        mut mrx: ManagerReceiver,
    ) -> AnyResult<()> {
        loop {
            tokio::select! {
                oe = prx.recv() => {
                    let Some(e) = oe else {
                        break;
                    };
                    self.handle_player_event(&tx, e).await?;

                }
                oe = mrx.recv() => {
                    let Some(e) = oe else {
                        break;
                    };
                    self.handle_track_manager_event(&tx, e).await?;
                }
                () = token.cancelled() => {
                    return Ok(());
                }
            };
        }
        Ok(())
    }

    async fn handle_player_event(&self, tx: &Broadcast, e: PlayerEvent) -> AnyResult<()> {
        tracing::debug!(event = ?e, "player event");
        match e.kind {
            PlayerEventKind::Muted => tx.broadcast(Event::Muted)?,
            PlayerEventKind::Unmuted => tx.broadcast(Event::Unmuted)?,
            PlayerEventKind::Volume(level) => tx.broadcast(Event::Volume { level })?,
            PlayerEventKind::PlaybackStarted => tx.broadcast(Event::PlaybackStarted)?,
            PlayerEventKind::PlaybackPaused => tx.broadcast(Event::PlaybackPaused)?,
            PlayerEventKind::PlaybackStopped => {
                let mut data = self.data.write().await;
                data.stop(&self.manager)?;
                tx.broadcast(Event::PlaybackStopped)?;
            }
            PlayerEventKind::TrackAdded(queue_id) => {
                let mut data = self.data.write().await;
                data.ready_queue(queue_id);
            }
            PlayerEventKind::TrackStarted(queue_id) => {
                let mut data = self.data.write().await;
                data.queue_player(&self.player).await?;
                tx.broadcast(Event::NowPlaying {
                    queue_id,
                    track: data.get_queued_track(queue_id).meta.clone(),
                })?;
            }
            PlayerEventKind::TrackFinished(queue_id) => {
                let mut data = self.data.write().await;
                data.remove_queue_player(&self.manager, queue_id)?;
                if data.player_queue_size == 0 {
                    if data.queue.is_empty() {
                        tx.broadcast(Event::QueueFinished)?;
                    } else {
                        tx.broadcast(Event::WaitingForDownload)?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle_track_manager_event(&self, tx: &Broadcast, e: ManagerEvent) -> AnyResult<()> {
        tracing::debug!(event = ?e, "track manager event");
        match e {
            ManagerEvent::Acquired { origin_id, id, err } => {
                let mut data = self.data.write().await;
                if let Some(err) = err {
                    let track = data.remove_track(&id);
                    tx.direct(origin_id, Event::DownloadError { track, err })?;
                } else {
                    data.ready_track(&id);
                    data.queue_player(&self.player).await?;
                }
            }
            ManagerEvent::Removed { id } => {
                let mut data = self.data.write().await;
                data.unready_track(&id);
            }
        }
        Ok(())
    }

    async fn serve_forever(
        state: Arc<Self>,
        token: CancellationToken,
        tx: Broadcast,
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
            queue_counter: ConsistentCounter::default(),
            origin_counter: RelaxedCounter::default(),
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

    fn remove_track(&mut self, id: &TrackId) -> Track {
        let to_remove = self
            .queue
            .iter()
            .filter_map(|(k, v)| if &v.track_id == id { Some(*k) } else { None })
            .collect_vec();
        for i in to_remove {
            self.queue.remove(&i);
        }
        let t = self.tracks.remove(id).unwrap();
        t.meta
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

    fn queue(
        &mut self,
        origin_id: OriginId,
        manager: &TrackManager,
        tracks: Vec<Track>,
    ) -> ChannelResult<Vec<QueuedTrack>> {
        let mut result = Vec::with_capacity(tracks.len());

        for track in tracks {
            manager.claim(origin_id, track.id.clone())?;
            let queue_id = self.queue_counter.inc();
            self.queue.insert(
                queue_id,
                QueueData {
                    track_id: track.id.clone(),
                    ready: false,
                    origin_id,
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
        let Some((queue_id, QueueData{track_id: _, ready: _, origin_id})) = self.queue.iter().find(|(_, x)| !x.ready) else {
            return Ok(());
        };

        let track = self.get_queued_track(*queue_id);
        if !track.ready {
            return Ok(());
        }

        player
            .queue(*origin_id, *queue_id, track.meta.path.clone())
            .await?;
        if self.auto_play && self.player_queue_size == 0 {
            tracing::debug!("requesting auto play");
            player.play(*origin_id).await?;
        }

        Ok(())
    }

    fn remove_queue_player(&mut self, manager: &TrackManager, id: QueueId) -> ChannelResult<()> {
        let Some(x) = self.queue.remove(&id) else {
            return Ok(());
        };
        manager.unclaim(x.track_id)?;
        self.player_queue_size -= 1;
        Ok(())
    }

    fn stop(&mut self, manager: &TrackManager) -> ChannelResult<()> {
        manager.clear()?;
        self.queue.clear();
        self.tracks.clear();
        self.player_queue_size = 0;
        self.queue_counter.reset();
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
        let (
            queue_id,
            QueueData {
                track_id,
                ready: _,
                origin_id: _,
            },
        ) = self.iter.next()?;
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
