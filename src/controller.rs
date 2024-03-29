use std::collections::btree_map::Iter as BTreeIter;
use std::collections::BTreeMap;
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;

use anyhow::Result as AnyResult;
use atomic_counter::{AtomicCounter, ConsistentCounter, RelaxedCounter};
use futures::stream::once;
use futures::{ready, stream_select, Future, FutureExt, Stream, StreamExt};
use itertools::Itertools;
use pin_project::pin_project;
use thiserror::Error;
use tokio::sync::{broadcast, RwLock, RwLockReadGuard};
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;
use tracing::{instrument, Instrument, Span};
use url::Url;

use crate::player::{
    Error as PlayerError, Event as PlayerEventKind, Event as PlayerEvent,
    EventEnvelope as PlayerEventEnvelope, OriginId, Player, QueueId, Receiver as PlayerReceiver,
    Request as PlayerRequest, Result as PlayerResult,
};
use crate::track_manager::{
    Event as ManagerEvent, EventEnvelope as ManagerEventEnvelope, Receiver as ManagerReceiver,
    TrackManager,
};
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

#[derive(Debug, Clone)]
pub struct Status {
    pub is_paused: bool,
    pub is_muted: bool,
    pub volume_level: u8,
    pub length: usize,
    pub now_playing: Option<Track>,
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
    TracksQueued { tracks: Vec<QueuedTrack> },
    Volume { level: u8 },
    NowPlaying { queue_id: QueueId, track: Track },
    DownloadError { track: Track, err: YoutubeError },
}

#[derive(Clone)]
pub struct EventEnvelope {
    pub span: Span,
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

#[derive(Debug)]
pub struct DataIterator<'a> {
    parent: &'a Data,
    iter: BTreeIter<'a, QueueId, QueueData>,
}

#[derive(Debug)]
pub struct DataItemView<'a> {
    pub queue_id: QueueId,
    pub state: QueuedTrackState,
    pub track: &'a Track,
}

#[derive(Debug, Clone, Copy)]
pub enum QueuedTrackState {
    NotReady,
    Downloaded,
    SentToPlayer,
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

#[derive(Debug)]
#[pin_project]
pub struct Receiver(#[pin] BroadcastStream<EventEnvelope>);

#[derive(Debug)]
#[pin_project]
pub struct ReceiverWrapped(#[pin] BroadcastStream<EventEnvelope>);

type EventTx = broadcast::Sender<EventEnvelope>;
type EventRx = broadcast::Receiver<EventEnvelope>;
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

enum MergedEvent {
    Player(PlayerEventEnvelope),
    Manager(ManagerEventEnvelope),
    Cancel,
}

impl Controller {
    #[instrument(skip(token, youtube))]
    pub async fn new(
        token: CancellationToken,
        youtube: Youtube,
        settings: Settings,
    ) -> AnyResult<(Self, impl Future<Output = AnyResult<()>>)> {
        let (tx, _) = broadcast::channel(BROADCAST_QUEUE_SIZE);
        let (manager, manager_receiver) =
            TrackManager::new(youtube.clone(), settings.track_cache_size);
        let (player, player_receiver) = Player::new().await?;
        let state = State::new(player, manager, youtube, settings.auto_play);

        let controller = Self {
            tx: tx.clone(),
            state: state.clone(),
        };
        let task = State::serve_forever(
            state,
            token,
            Broadcast(tx),
            player_receiver,
            manager_receiver,
        );

        Ok((controller, task))
    }

    #[instrument(skip_all)]
    pub fn subscribe(&self) -> (Sender, Receiver) {
        (
            Sender {
                tx: Broadcast(self.tx.clone()),
                state: self.state.clone(),
            },
            Receiver::new(self.tx.subscribe()),
        )
    }
}

impl Receiver {
    #[instrument(skip_all)]
    fn new(rx: EventRx) -> Self {
        Self(BroadcastStream::new(rx))
    }
}

impl Stream for Receiver {
    type Item = EventEnvelope;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        match ready!(self.project().0.poll_next(cx)) {
            Some(x) => match x {
                Ok(x) => Poll::Ready(Some(x)),
                Err(_) => Poll::Pending,
            },
            None => Poll::Ready(None),
        }
    }
}

impl Sender {
    #[instrument(skip(self))]
    pub async fn resolve(&self, urls: Vec<Url>) -> Result<Vec<Track>> {
        self.state.youtube.resolve(urls).await.map_err(|e| match e {
            YoutubeError::Channel(e) => Error::Channel(e),
            e => Error::Youtube(e),
        })
    }

    #[instrument(skip(self))]
    pub async fn queue(&self, tracks: Vec<Track>) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        let tracks = self.state.queue(origin_id, tracks).await?;
        self.tx.send(origin_id, Event::TracksQueued { tracks })?;
        Ok(origin_id)
    }

    #[instrument(skip_all)]
    pub async fn play(&self) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state
            .player
            .request(origin_id, PlayerRequest::Play)
            .await?;
        Ok(origin_id)
    }

    #[instrument(skip_all)]
    pub async fn pause(&self) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state
            .player
            .request(origin_id, PlayerRequest::Pause)
            .await?;
        Ok(origin_id)
    }

    #[instrument(skip_all)]
    pub async fn play_toggle(&self) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state
            .player
            .request(origin_id, PlayerRequest::PlayToggle)
            .await?;
        Ok(origin_id)
    }

    #[instrument(skip_all)]
    pub async fn stop(&self) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state
            .player
            .request(origin_id, PlayerRequest::Stop)
            .await?;
        Ok(origin_id)
    }

    #[instrument(skip_all)]
    pub async fn skip(&self) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state
            .player
            .request(origin_id, PlayerRequest::Skip)
            .await?;
        Ok(origin_id)
    }

    #[instrument(skip_all)]
    pub async fn mute(&self) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state
            .player
            .request(origin_id, PlayerRequest::Mute)
            .await?;
        Ok(origin_id)
    }

    #[instrument(skip_all)]
    pub async fn unmute(&self) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state
            .player
            .request(origin_id, PlayerRequest::Unmute)
            .await?;
        Ok(origin_id)
    }

    #[instrument(skip_all)]
    pub async fn mute_toggle(&self) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state
            .player
            .request(origin_id, PlayerRequest::MuteToggle)
            .await?;
        Ok(origin_id)
    }

    #[instrument(skip(self))]
    pub async fn set_volume(&self, level: u8) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state
            .player
            .request(origin_id, PlayerRequest::SetVolume { level })
            .await?;
        Ok(origin_id)
    }

    #[instrument(skip_all)]
    pub async fn increase_volume(&self) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state
            .player
            .request(origin_id, PlayerRequest::IncreaseVolume)
            .await?;
        Ok(origin_id)
    }

    #[instrument(skip_all)]
    pub async fn decrease_volume(&self) -> Result<OriginId> {
        let origin_id = self.state.origin_id_counter.inc();
        self.state
            .player
            .request(origin_id, PlayerRequest::DecreaseVolume)
            .await?;
        Ok(origin_id)
    }

    #[instrument(skip_all)]
    pub async fn status(&self) -> Result<Status> {
        let status = self
            .state
            .player
            .request(usize::MAX, PlayerRequest::Status)
            .await?
            .unwrap();
        let data = self.state.data.read().await;
        let length = data.queue.len() - usize::from(status.length != 0);
        let now_playing = data.get_now_playing_track().cloned();
        Ok(Status {
            is_paused: status.is_paused,
            is_muted: status.is_muted,
            volume_level: status.volume_level,
            length,
            now_playing,
        })
    }

    #[instrument(skip_all)]
    pub async fn view(&self) -> RwLockReadGuard<'_, Data> {
        self.state.data.read().await
    }
}

impl Broadcast {
    #[instrument(skip(self))]
    fn send(&self, origin_id: OriginId, event: Event) -> ChannelResult<()> {
        let span = tracing::info_span!("broadcast controller message");
        self.0.send(EventEnvelope {
            span,
            origin_id,
            event,
        })?;
        Ok(())
    }

    #[instrument(skip(self))]
    fn proxy(&self, origin_id: OriginId) -> BroadcastProxy<'_> {
        BroadcastProxy(&self.0, origin_id)
    }
}

impl<'a> BroadcastProxy<'a> {
    #[instrument(skip(self))]
    fn send(&self, event: Event) -> ChannelResult<()> {
        let span = tracing::info_span!("broadcast controller message");
        self.0.send(EventEnvelope {
            span,
            origin_id: self.1,
            event,
        })?;
        Ok(())
    }
}

impl State {
    #[instrument(skip(player, manager, youtube))]
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

    #[instrument(skip(self))]
    async fn queue(&self, origin_id: OriginId, tracks: Vec<Track>) -> Result<Vec<QueuedTrack>> {
        let mut data = self.data.write().await;
        let was_empty = data.queue.is_empty();
        let result = data.queue(origin_id, &self.manager, tracks)?;
        if self.auto_play && was_empty {
            self.player.request(origin_id, PlayerRequest::Play).await?;
        }
        Ok(result)
    }

    #[instrument(skip_all)]
    async fn serve_forever_impl(
        &self,
        token: CancellationToken,
        tx: Broadcast,
        prx: PlayerReceiver,
        mrx: ManagerReceiver,
    ) -> AnyResult<()> {
        let cancel = pin!(token.cancelled_owned().fuse());
        let mut stream = stream_select!(
            prx.map(MergedEvent::Player),
            mrx.map(MergedEvent::Manager),
            once(cancel).map(|()| MergedEvent::Cancel),
        );

        while let Some(e) = stream.next().await {
            match e {
                MergedEvent::Player(PlayerEventEnvelope {
                    span,
                    origin_id,
                    event,
                }) => {
                    self.handle_player_event(&tx, origin_id, event)
                        .instrument(span)
                        .await?;
                }
                MergedEvent::Manager(ManagerEventEnvelope { span, event }) => {
                    self.handle_track_manager_event(&tx, event)
                        .instrument(span)
                        .await?;
                }
                MergedEvent::Cancel => {
                    break;
                }
            }
        }

        Ok(())
    }

    #[instrument(skip(self, tx))]
    async fn handle_player_event(
        &self,
        tx: &Broadcast,
        origin_id: OriginId,
        e: PlayerEvent,
    ) -> AnyResult<()> {
        tracing::debug!(event = ?e, "player event");
        let tx = tx.proxy(origin_id);
        match e {
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
                tx.send(Event::NowPlaying {
                    queue_id,
                    track: data.get_queued_track(queue_id).meta.clone(),
                })?;
            }
            PlayerEventKind::TrackFinished(queue_id) => {
                let mut data = self.data.write().await;
                data.remove_queue_player(&self.manager, queue_id)?;
                match data.first_track_state() {
                    Some(QueuedTrackState::NotReady) => {
                        tx.send(Event::WaitingForDownload)?;
                    }
                    Some(QueuedTrackState::Downloaded) => {
                        data.queue_player(&self.player).await?;
                    }
                    Some(QueuedTrackState::SentToPlayer) => {}
                    None => {
                        tx.send(Event::QueueFinished)?;
                    }
                }
            }
        }
        Ok(())
    }

    #[instrument(skip(self, tx))]
    async fn handle_track_manager_event(&self, tx: &Broadcast, e: ManagerEvent) -> AnyResult<()> {
        match e {
            ManagerEvent::Acquired { id } => {
                let mut data = self.data.write().await;
                data.ready_track(&id);
                if matches!(data.first_track_state(), Some(QueuedTrackState::Downloaded)) {
                    data.queue_player(&self.player).await?;
                }
            }
            ManagerEvent::Removed { id, e } => {
                let mut data = self.data.write().await;
                if let Some((origin_id, err)) = e {
                    let track = data.remove_track(&id);
                    tx.send(origin_id, Event::DownloadError { track, err })?;
                } else {
                    data.unready_track(&id);
                }
            }
        }
        Ok(())
    }

    #[instrument(skip_all)]
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
    #[instrument(skip_all)]
    pub fn iter(&self) -> DataIterator<'_> {
        DataIterator {
            parent: self,
            iter: self.queue.iter(),
        }
    }

    #[instrument(skip(self))]
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

    #[instrument(skip(self))]
    fn ready_track(&mut self, id: &TrackId) {
        self.tracks.get_mut(id).unwrap().set_downloaded(true);
    }

    #[instrument(skip(self))]
    fn unready_track(&mut self, id: &TrackId) {
        self.tracks.get_mut(id).unwrap().set_downloaded(false);
    }

    #[instrument(skip(self))]
    fn set_queued(&mut self, id: QueueId) {
        self.queue.get_mut(&id).unwrap().set_sent_to_player(true);
    }

    #[instrument(skip(self))]
    fn get_queued_track(&self, id: QueueId) -> &TrackData {
        let url = &self.queue.get(&id).unwrap().track_id;
        self.tracks.get(url).unwrap()
    }

    #[instrument(skip_all)]
    fn get_now_playing_track(&self) -> Option<&Track> {
        let Some((
            _,
            QueueData {
                track_id,
                sent_to_player,
                origin_id: _,
            },
        )) = self.queue.first_key_value()
        else {
            return None;
        };
        if !sent_to_player {
            return None;
        }
        let track = self.tracks.get(track_id).unwrap();
        Some(&track.meta)
    }

    #[instrument(skip(self, manager))]
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

    #[instrument(skip_all)]
    pub fn first_track_state(&self) -> Option<QueuedTrackState> {
        self.queue
            .first_key_value()
            .map(|(_, v)| self.get_track_state(v, None))
    }

    #[instrument(skip(self))]
    fn get_track_state(
        &self,
        queue_data: &QueueData,
        track_data: Option<&TrackData>,
    ) -> QueuedTrackState {
        let QueueData {
            track_id,
            sent_to_player,
            origin_id: _,
        } = queue_data;

        if *sent_to_player {
            QueuedTrackState::SentToPlayer
        } else {
            let track = track_data.or_else(|| self.tracks.get(track_id)).unwrap();
            if track.downloaded {
                QueuedTrackState::Downloaded
            } else {
                QueuedTrackState::NotReady
            }
        }
    }

    #[instrument(skip_all)]
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
            .request(
                *origin_id,
                PlayerRequest::Queue {
                    id: *queue_id,
                    path: track.meta.path.clone(),
                },
            )
            .await?;

        Ok(())
    }

    #[instrument(skip(self, manager))]
    fn remove_queue_player(&mut self, manager: &TrackManager, id: QueueId) -> ChannelResult<()> {
        let Some(x) = self.queue.remove(&id) else {
            return Ok(());
        };
        manager.unclaim(x.track_id)?;
        Ok(())
    }

    #[instrument(skip_all)]
    fn stop(&mut self, manager: &TrackManager) -> ChannelResult<()> {
        manager.clear()?;
        self.queue.clear();
        self.tracks.clear();
        self.reset_queue_counter();
        Ok(())
    }

    #[instrument(skip_all)]
    fn reset_queue_counter(&self) {
        self.queue_id_counter.reset();
        loop {
            let new_id = self.queue_id_counter.inc();
            if new_id + 1 == QUEUE_ID_START {
                break;
            }
        }
    }

    #[instrument(skip_all)]
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
        DataIterator {
            parent: self,
            iter: self.queue.iter(),
        }
    }
}

impl<'a> Iterator for DataIterator<'a> {
    type Item = DataItemView<'a>;

    #[instrument(skip_all)]
    fn next(&mut self) -> Option<Self::Item> {
        let (&queue_id, queue_data) = self.iter.next()?;

        let track = self.parent.tracks.get(&queue_data.track_id).unwrap();
        let state = self.parent.get_track_state(queue_data, Some(track));
        Some(DataItemView {
            queue_id,
            state,
            track: &track.meta,
        })
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
