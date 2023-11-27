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
const QUEUE_ID_START: usize = 1;

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
pub struct Receiver(EventRx);

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

#[derive(Clone)]
pub struct EventWrapper {
    pub origin_id: OriginId,
    pub event: Event,
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

#[derive(Debug)]
struct State {
    youtube: Youtube,
    player: Player,
    manager: TrackManager,
    origin_id_counter: RelaxedCounter,
    data: RwLock<Data>,
}

#[derive(Clone)]
struct Broadcast(EventTx);

struct BroadcastProxy<'a>(&'a EventTx, OriginId);

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

    pub async fn subscribe(&self) -> Receiver {
        Receiver(self.tx.subscribe())
    }

    pub async fn queue(&self, urls: Vec<Url>) -> Result<(OriginId, Vec<QueuedTrack>)> {
        let tracks = self
            .state
            .youtube
            .resolve(urls)
            .await
            .map_err(|e| match e {
                YoutubeError::Channel(e) => Error::Channel(e),
                e => Error::Youtube(e),
            })?;
        let origin_id = self.state.origin_id_counter.inc();
        let tracks = self.state.queue(origin_id, tracks).await?;
        Ok((origin_id, tracks))
    }

    pub async fn play(&self) -> PlayerResult<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.play(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn pause(&self) -> PlayerResult<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.pause(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn play_toggle(&self) -> PlayerResult<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.play_toggle(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn stop(&self) -> PlayerResult<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.stop(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn skip(&self) -> PlayerResult<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.skip(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn mute(&self) -> PlayerResult<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.mute(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn unmute(&self) -> PlayerResult<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.unmute(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn mute_toggle(&self) -> PlayerResult<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.mute_toggle(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn get_volume(&self) -> PlayerResult<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.get_volume(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn set_volume(&self, level: u8) -> PlayerResult<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.set_volume(origin_id, level).await?;
        Ok(origin_id)
    }

    pub async fn increase_volume(&self) -> PlayerResult<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.increase_volume(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn decrease_volume(&self) -> PlayerResult<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.decrease_volume(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn view(&self) -> RwLockReadGuard<'_, Data> {
        self.state.data.read().await
    }
}

impl Broadcast {
    fn send(&self, origin_id: OriginId, event: Event) -> ChannelResult<()> {
        self.0.send(EventWrapper { origin_id, event })?;
        Ok(())
    }

    fn proxy(&self, origin_id: OriginId) -> BroadcastProxy<'_> {
        BroadcastProxy(&self.0, origin_id)
    }
}

impl<'a> BroadcastProxy<'a> {
    fn send(&self, event: Event) -> ChannelResult<()> {
        self.0.send(EventWrapper {
            origin_id: self.1,
            event,
        })?;
        Ok(())
    }
}

impl Receiver {
    pub async fn recv(&mut self) -> Option<Event> {
        self.0.recv().await.ok().map(|x| x.event)
    }

    pub async fn recv_wrapped(&mut self) -> Option<EventWrapper> {
        self.0.recv().await.ok()
    }
}

impl State {
    fn new(player: Player, manager: TrackManager, youtube: Youtube, auto_play: bool) -> Arc<Self> {
        Arc::new(Self {
            player,
            manager,
            youtube,
            origin_id_counter: RelaxedCounter::default(),
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
        let tx = tx.proxy(e.origin_id);
        match e.kind {
            PlayerEventKind::Muted => tx.send(Event::Muted)?,
            PlayerEventKind::Unmuted => tx.send(Event::Unmuted)?,
            PlayerEventKind::Volume(level) => tx.send(Event::Volume { level })?,
            PlayerEventKind::PlaybackStarted => tx.send(Event::PlaybackStarted)?,
            PlayerEventKind::PlaybackPaused => tx.send(Event::PlaybackPaused)?,
            PlayerEventKind::PlaybackStopped => {
                let mut data = self.data.write().await;
                data.stop(&self.manager)?;
                tx.send(Event::PlaybackStopped)?;
            }
            PlayerEventKind::TrackAdded(queue_id) => {
                let mut data = self.data.write().await;
                data.ready_queue(queue_id);
            }
            PlayerEventKind::TrackStarted(queue_id) => {
                let mut data = self.data.write().await;
                data.queue_player(&self.player).await?;
                tx.send(Event::NowPlaying {
                    queue_id,
                    track: data.get_queued_track(queue_id).meta.clone(),
                })?;
            }
            PlayerEventKind::TrackFinished(queue_id) => {
                let mut data = self.data.write().await;
                data.remove_queue_player(&self.manager, queue_id)?;
                if data.player_queue_size == 0 {
                    if data.queue.is_empty() {
                        tx.send(Event::QueueFinished)?;
                    } else {
                        tx.send(Event::WaitingForDownload)?;
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
                    tx.send(origin_id, Event::DownloadError { track, err })?;
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
            queue_counter: ConsistentCounter::new(QUEUE_ID_START),
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
            let queue_id = self.inc_queue_id();
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
        let Some((
            queue_id,
            QueueData {
                track_id: _,
                ready: _,
                origin_id,
            },
        )) = self.queue.iter().find(|(_, x)| !x.ready)
        else {
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
        self.reset_queue_counter();
        Ok(())
    }

    fn reset_queue_counter(&self) {
        self.queue_counter.reset();
        loop {
            let new_id = self.queue_counter.inc();
            if new_id + 1 == QUEUE_ID_START {
                break;
            }
        }
    }

    fn inc_queue_id(&self) -> QueueId {
        let new_id = self.queue_counter.inc();
        // TODO: replace with sane wraparound handling
        if new_id < QUEUE_ID_START {
            panic!("wraparound in queue id");
        }
        new_id
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
