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
    Error as PlayerError, Event as PlayerEvent, EventKind as PlayerEventKind, OriginId, Player,
    QueueId, Receiver as PlayerReceiver, Result as PlayerResult,
};
use crate::track_manager::{Event as ManagerEvent, Receiver as ManagerReceiver, TrackManager};
use crate::util::cancel::Task;
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
    tx: Broadcast,
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
    Player(#[from] PlayerError),
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
    TracksQueued { tracks: Vec<QueuedTrack> },
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

#[derive(Debug)]
pub struct Data {
    pub queue: Map<QueueId, QueueData>,
    pub tracks: Map<TrackId, TrackData>,
    queue_id_counter: ConsistentCounter,
}

pub struct DataIterator<'a> {
    parent: &'a Data,
    iter: BTreeIter<'a, QueueId, QueueData>,
}

#[derive(Debug)]
pub struct TrackData {
    pub meta: Track,
    pub downloaded: bool,
}

#[derive(Debug)]
pub struct QueueData {
    pub track_id: TrackId,
    pub sent_to_player: bool,
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
    auto_play: bool,
    data: RwLock<Data>,
}

#[derive(Debug, Clone)]
struct Broadcast(EventTx);

struct BroadcastProxy<'a>(&'a EventTx, OriginId);

enum QueueTrackState {
    NotPresent,
    NotReady,
    Downloaded,
    SentToPlayer,
}

impl Controller {
    pub fn new(
        token: CancellationToken,
        player: Player,
        player_receiver: PlayerReceiver,
        youtube: Youtube,
        settings: Settings,
    ) -> (Self, Task) {
        let (tx, _) = broadcast::channel(BROADCAST_QUEUE_SIZE);
        let (manager, manager_receiver) =
            TrackManager::new(youtube.clone(), settings.track_cache_size);
        let state = State::new(player, manager, youtube, settings.auto_play);

        let controller = Self {
            tx: tx.clone(),
            state: state.clone(),
        };
        let task = tokio::spawn(State::serve_forever(
            state,
            token,
            Broadcast(tx),
            player_receiver,
            manager_receiver,
        ));

        (controller, task)
    }

    pub fn subscribe(&self) -> (Sender, Receiver) {
        (
            Sender {
                tx: Broadcast(self.tx.clone()),
                state: self.state.clone(),
            },
            Receiver(self.tx.subscribe()),
        )
    }
}

impl Sender {
    pub async fn queue(&self, urls: Vec<Url>) -> Result<OriginId> {
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
        self.tx.send(origin_id, Event::TracksQueued { tracks })?;
        Ok(origin_id)
    }

    pub async fn play(&self) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.play(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn pause(&self) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.pause(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn play_toggle(&self) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.play_toggle(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn stop(&self) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.stop(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn skip(&self) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.skip(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn mute(&self) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.mute(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn unmute(&self) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.unmute(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn mute_toggle(&self) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.mute_toggle(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn get_volume(&self) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.get_volume(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn set_volume(&self, level: u8) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.set_volume(origin_id, level).await?;
        Ok(origin_id)
    }

    pub async fn increase_volume(&self) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state.player.increase_volume(origin_id).await?;
        Ok(origin_id)
    }

    pub async fn decrease_volume(&self) -> Result<OriginId> {
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
            auto_play,
            origin_id_counter: RelaxedCounter::default(),
            data: RwLock::new(Data::default()),
        })
    }

    async fn queue(&self, origin_id: OriginId, tracks: Vec<Track>) -> Result<Vec<QueuedTrack>> {
        let mut data = self.data.write().await;
        let was_empty = data.queue.is_empty();
        let result = data.queue(origin_id, &self.manager, tracks)?;
        if self.auto_play && was_empty {
            self.player.play(origin_id).await?;
        }
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
                data.set_queued(queue_id);
            }
            PlayerEventKind::TrackStarted(queue_id) => {
                let data = self.data.read().await;
                // data.queue_player(&self.player).await?;
                tx.send(Event::NowPlaying {
                    queue_id,
                    track: data.get_queued_track(queue_id).meta.clone(),
                })?;
            }
            PlayerEventKind::TrackFinished(queue_id) => {
                let mut data = self.data.write().await;
                data.remove_queue_player(&self.manager, queue_id)?;
                match data.first_track_state() {
                    QueueTrackState::NotPresent => {
                        tx.send(Event::QueueFinished)?;
                    }
                    QueueTrackState::NotReady => {
                        tx.send(Event::WaitingForDownload)?;
                    }
                    QueueTrackState::Downloaded => {
                        data.queue_player(&self.player).await?;
                    }
                    QueueTrackState::SentToPlayer => {}
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
                    if matches!(data.first_track_state(), QueueTrackState::Downloaded) {
                        data.queue_player(&self.player).await?;
                    }
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
        self.tracks.get_mut(id).unwrap().set_downloaded(true);
    }

    fn unready_track(&mut self, id: &TrackId) {
        self.tracks.get_mut(id).unwrap().set_downloaded(false);
    }

    fn set_queued(&mut self, id: QueueId) {
        self.queue.get_mut(&id).unwrap().set_sent_to_player(true);
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
                    sent_to_player: false,
                    origin_id,
                },
            );
            if !self.tracks.contains_key(&track.id) {
                self.tracks.insert(
                    track.id.clone(),
                    TrackData {
                        meta: track.clone(),
                        downloaded: false,
                    },
                );
            }
            result.push(QueuedTrack { queue_id, track });
        }

        Ok(result)
    }

    fn first_track_state(&self) -> QueueTrackState {
        if let Some((
            _,
            QueueData {
                track_id,
                sent_to_player: ready,
                origin_id: _,
            },
        )) = self.queue.first_key_value()
        {
            if *ready {
                QueueTrackState::SentToPlayer
            } else {
                let track = self.tracks.get(track_id).unwrap();
                if track.downloaded {
                    QueueTrackState::Downloaded
                } else {
                    QueueTrackState::NotReady
                }
            }
        } else {
            QueueTrackState::NotPresent
        }
    }

    async fn queue_player(&mut self, player: &Player) -> PlayerResult<()> {
        let Some((
            queue_id,
            QueueData {
                track_id,
                sent_to_player: _,
                origin_id,
            },
        )) = self.queue.iter().find(|(_, x)| !x.sent_to_player)
        else {
            return Ok(());
        };

        let track = self.tracks.get(track_id).unwrap();
        if !track.downloaded {
            return Ok(());
        }

        player
            .queue(*origin_id, *queue_id, track.meta.path.clone())
            .await?;

        Ok(())
    }

    fn remove_queue_player(&mut self, manager: &TrackManager, id: QueueId) -> ChannelResult<()> {
        let Some(x) = self.queue.remove(&id) else {
            return Ok(());
        };
        manager.unclaim(x.track_id)?;
        Ok(())
    }

    fn stop(&mut self, manager: &TrackManager) -> ChannelResult<()> {
        manager.clear()?;
        self.queue.clear();
        self.tracks.clear();
        self.reset_queue_counter();
        Ok(())
    }

    fn reset_queue_counter(&self) {
        self.queue_id_counter.reset();
        loop {
            let new_id = self.queue_id_counter.inc();
            if new_id + 1 == QUEUE_ID_START {
                break;
            }
        }
    }

    fn inc_queue_id(&self) -> QueueId {
        let new_id = self.queue_id_counter.inc();
        // TODO: replace with sane wraparound handling
        assert!(new_id >= QUEUE_ID_START, "queue id wraparound");
        new_id
    }
}

impl Default for Data {
    fn default() -> Self {
        Self {
            queue: Map::new(),
            tracks: Map::new(),
            queue_id_counter: ConsistentCounter::new(QUEUE_ID_START),
        }
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
                sent_to_player: _,
                origin_id: _,
            },
        ) = self.iter.next()?;
        let track = &self.parent.tracks.get(track_id).unwrap().meta;
        Some((queue_id, track))
    }
}

impl TrackData {
    fn set_downloaded(&mut self, value: bool) {
        self.downloaded = value;
    }
}

impl QueueData {
    fn set_sent_to_player(&mut self, value: bool) {
        self.sent_to_player = value;
    }
}
