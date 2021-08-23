use crate::{AttributeUpdate, WatchRequest};

use super::{AttributeUpdateOp, Event, Readiness, WakeOp, Watch, WatchKind};
use console_api as proto;
use proto::resources::resource;
use proto::resources::stats::Attribute;
use tokio::sync::{mpsc, Notify};

use futures::FutureExt;
use std::{
    collections::HashMap,
    convert::TryInto,
    ops::{Deref, DerefMut},
    sync::{
        atomic::{AtomicBool, Ordering::*},
        Arc,
    },
    time::{Duration, SystemTime},
};
use tracing_core::{span, Metadata};

use hdrhistogram::{
    serialization::{Serializer, V2SerializeError, V2Serializer},
    Histogram,
};

pub(crate) struct Aggregator {
    /// Channel of incoming events emitted by `TaskLayer`s.
    events: mpsc::Receiver<Event>,

    /// New incoming RPCs.
    rpcs: mpsc::Receiver<WatchKind>,

    /// The interval at which new data updates are pushed to clients.
    publish_interval: Duration,

    /// How long to keep task data after a task has completed.
    retention: Duration,

    /// Triggers a flush when the event buffer is approaching capacity.
    flush_capacity: Arc<Flush>,

    /// Currently active RPCs streaming task events.
    watchers: Vec<Watch<proto::instrument::InstrumentUpdate>>,

    /// Currently active RPCs streaming task details events, by task ID.
    details_watchers: HashMap<span::Id, Vec<Watch<proto::tasks::TaskDetails>>>,

    /// *All* metadata for task spans and user-defined spans that we care about.
    ///
    /// This is sent to new clients as part of the initial state.
    all_metadata: Vec<proto::register_metadata::NewMetadata>,

    /// *New* metadata that was registered since the last state update.
    ///
    /// This is emptied on every state update.
    new_metadata: Vec<proto::register_metadata::NewMetadata>,

    /// Map of task IDs to task static data.
    tasks: IdData<Task>,

    /// Map of task IDs to task stats.
    task_stats: IdData<TaskStats>,

    /// Map of resource IDs to resource static data.
    resources: IdData<Resource>,

    /// Map of resource IDs to resource stats.
    resource_stats: IdData<ResourceStats>,

    /// Map of AsyncOp IDs to AsyncOp static data.
    async_ops: IdData<AsyncOp>,

    /// Map of AsyncOp IDs to AsyncOp stats.
    async_op_stats: IdData<AsyncOpStats>,

    /// *All* PollOp events for AsyncOps on Resources.
    ///
    /// This is sent to new clients as part of the initial state.
    all_poll_ops: Vec<proto::resources::PollOp>,

    /// *New* PollOp events that whave occured since the last update
    ///
    /// This is emptied on every state update.
    new_poll_ops: Vec<proto::resources::PollOp>,
}

#[derive(Debug)]
pub(crate) struct Flush {
    pub(crate) should_flush: Notify,
    pub(crate) triggered: AtomicBool,
}

// An entity that at some point in time can be closed.
// This generally refers to spans that have been closed
// indicating that a task, async op or a resource is not
// in use anymore
trait Closable {
    fn closed_at(&self) -> Option<SystemTime>;
}

trait ToProto {
    type Output;
    fn to_proto(&self) -> Self::Output;
}

struct PollStats {
    // the number of polls in progress
    current_polls: u64,
    // the total number of polls
    polls: u64,
    first_poll: Option<SystemTime>,
    last_poll_started: Option<SystemTime>,
    last_poll_ended: Option<SystemTime>,
    busy_time: Duration,
}

// Represent static data for resources
struct Resource {
    id: span::Id,
    metadata: &'static Metadata<'static>,
    concrete_type: String,
    kind: resource::Kind,
}

#[derive(Hash, PartialEq, Eq)]
struct FieldKey {
    meta_id: u64,
    field_name: proto::field::Name,
}

#[derive(Default)]
struct ResourceStats {
    created_at: Option<SystemTime>,
    closed_at: Option<SystemTime>,
    attributes: HashMap<FieldKey, Attribute>,
}

/// Represents static data for tasks
struct Task {
    id: span::Id,
    metadata: &'static Metadata<'static>,
    fields: Vec<proto::Field>,
}

struct TaskStats {
    // task stats
    created_at: Option<SystemTime>,
    closed_at: Option<SystemTime>,

    // waker stats
    wakes: u64,
    waker_clones: u64,
    waker_drops: u64,
    last_wake: Option<SystemTime>,

    poll_times_histogram: Histogram<u64>,
    poll_stats: PollStats,
}

struct AsyncOp {
    id: span::Id,
    metadata: &'static Metadata<'static>,
    source: String,
}

#[derive(Default)]
struct AsyncOpStats {
    created_at: Option<SystemTime>,
    closed_at: Option<SystemTime>,
    resource_id: Option<span::Id>,
    task_id: Option<span::Id>,
    poll_stats: PollStats,
}

struct IdData<T> {
    data: HashMap<span::Id, (T, bool)>,
}

impl Closable for ResourceStats {
    fn closed_at(&self) -> Option<SystemTime> {
        self.closed_at
    }
}

impl Closable for TaskStats {
    fn closed_at(&self) -> Option<SystemTime> {
        self.closed_at
    }
}

impl Closable for AsyncOpStats {
    fn closed_at(&self) -> Option<SystemTime> {
        self.closed_at
    }
}

impl PollStats {
    fn update_on_span_enter(&mut self, timestamp: SystemTime) {
        if self.current_polls == 0 {
            self.last_poll_started = Some(timestamp);
            if self.first_poll == None {
                self.first_poll = Some(timestamp);
            }
            self.polls += 1;
        }
        self.current_polls += 1;
    }

    fn update_on_span_exit(&mut self, timestamp: SystemTime) {
        self.current_polls -= 1;
        if self.current_polls == 0 {
            if let Some(last_poll_started) = self.last_poll_started {
                let elapsed = timestamp.duration_since(last_poll_started).unwrap();
                self.last_poll_ended = Some(timestamp);
                self.busy_time += elapsed;
            }
        }
    }

    fn since_last_poll(&self, timestamp: SystemTime) -> Option<Duration> {
        self.last_poll_started
            .map(|lps| timestamp.duration_since(lps).unwrap())
    }
}

impl Default for PollStats {
    fn default() -> Self {
        PollStats {
            current_polls: 0,
            polls: 0,
            first_poll: None,
            last_poll_started: None,
            last_poll_ended: None,
            busy_time: Default::default(),
        }
    }
}

impl Default for TaskStats {
    fn default() -> Self {
        TaskStats {
            created_at: None,
            closed_at: None,
            wakes: 0,
            waker_clones: 0,
            waker_drops: 0,
            last_wake: None,
            // significant figures should be in the [0-5] range and memory usage
            // grows exponentially with higher a sigfig
            poll_times_histogram: Histogram::<u64>::new(2).unwrap(),
            poll_stats: PollStats::default(),
        }
    }
}

impl Aggregator {
    pub(crate) fn new(
        events: mpsc::Receiver<Event>,
        rpcs: mpsc::Receiver<WatchKind>,
        builder: &crate::Builder,
    ) -> Self {
        Self {
            flush_capacity: Arc::new(Flush {
                should_flush: Notify::new(),
                triggered: AtomicBool::new(false),
            }),
            rpcs,
            publish_interval: builder.publish_interval,
            retention: builder.retention,
            events,
            watchers: Vec::new(),
            details_watchers: HashMap::new(),
            all_metadata: Vec::new(),
            new_metadata: Vec::new(),
            tasks: IdData::default(),
            task_stats: IdData::default(),
            resources: IdData::default(),
            resource_stats: IdData::default(),
            async_ops: IdData::default(),
            async_op_stats: IdData::default(),
            all_poll_ops: Vec::default(),
            new_poll_ops: Vec::default(),
        }
    }

    pub(crate) fn flush(&self) -> &Arc<Flush> {
        &self.flush_capacity
    }

    pub(crate) async fn run(mut self) {
        let mut publish = tokio::time::interval(self.publish_interval);
        loop {
            let should_send = tokio::select! {
                // if the flush interval elapses, flush data to the client
                _ = publish.tick() => {
                    true
                }

                // triggered when the event buffer is approaching capacity
                _ = self.flush_capacity.should_flush.notified() => {
                    self.flush_capacity.triggered.store(false, Release);
                    tracing::debug!("approaching capacity; draining buffer");
                    false
                }

                // a new client has started watching!
                subscription = self.rpcs.recv() => {
                    match subscription {
                        Some(WatchKind::Instrument(subscription)) => {
                            self.add_instrument_subscription(subscription);
                        },
                        Some(WatchKind::TaskDetail(watch_request)) => {
                            self.add_task_detail_subscription(watch_request);
                        },
                        _ => {
                            tracing::debug!("rpc channel closed, terminating");
                            return;
                        }
                    };

                    false
                }

            };

            // drain and aggregate buffered events.
            //
            // Note: we *don't* want to actually await the call to `recv` --- we
            // don't want the aggregator task to be woken on every event,
            // because it will then be woken when its own `poll` calls are
            // exited. that would result in a busy-loop. instead, we only want
            // to be woken when the flush interval has elapsed, or when the
            // channel is almost full.
            while let Some(event) = self.events.recv().now_or_never() {
                match event {
                    Some(event) => self.update_state(event),
                    // The channel closed, no more events will be emitted...time
                    // to stop aggregating.
                    None => {
                        tracing::debug!("event channel closed; terminating");
                        return;
                    }
                };
            }

            // flush data to clients, if there are any currently subscribed
            // watchers and we should send a new update.
            if !self.watchers.is_empty() && should_send {
                self.publish();
            }
            self.cleanup_closed();
        }
    }

    fn cleanup_closed(&mut self) {
        // drop all closed have that has completed *and* whose final data has already
        // been sent off.
        let now = SystemTime::now();
        let has_watchers = !self.watchers.is_empty();
        self.tasks
            .drop_closed(&mut self.task_stats, now, self.retention, has_watchers);
        self.resources
            .drop_closed(&mut self.resource_stats, now, self.retention, has_watchers);
        self.async_ops
            .drop_closed(&mut self.async_op_stats, now, self.retention, has_watchers);
    }

    /// Add the task subscription to the watchers after sending the first update
    fn add_instrument_subscription(
        &mut self,
        subscription: Watch<proto::instrument::InstrumentUpdate>,
    ) {
        tracing::debug!("new instrument subscription");
        let now = SystemTime::now();
        // Send the initial state --- if this fails, the subscription is already dead
        let update = &proto::instrument::InstrumentUpdate {
            task_update: Some(proto::tasks::TaskUpdate {
                new_tasks: self
                    .tasks
                    .all()
                    .map(|(_, value)| value.to_proto())
                    .collect(),
                stats_update: self.task_stats.as_proto(Include::All),
            }),
            resource_update: Some(proto::resources::ResourceUpdate {
                new_resources: self
                    .resources
                    .all()
                    .map(|(_, value)| value.to_proto())
                    .collect(),
                stats_update: self.resource_stats.as_proto(Include::All),
                new_poll_ops: self.all_poll_ops.clone(),
            }),
            async_op_update: Some(proto::async_ops::AsyncOpUpdate {
                new_async_ops: self
                    .async_ops
                    .all()
                    .map(|(_, value)| value.to_proto())
                    .collect(),
                stats_update: self.async_op_stats.as_proto(Include::All),
            }),
            now: Some(now.into()),
            new_metadata: Some(proto::RegisterMetadata {
                metadata: self.all_metadata.clone(),
            }),
        };

        if subscription.update(update) {
            self.watchers.push(subscription)
        }
    }

    /// Add the task details subscription to the watchers after sending the first update,
    /// if the task is found.
    fn add_task_detail_subscription(
        &mut self,
        watch_request: WatchRequest<proto::tasks::TaskDetails>,
    ) {
        let WatchRequest {
            id,
            stream_sender,
            buffer,
        } = watch_request;
        tracing::debug!(id = ?id, "new task details subscription");
        let task_id: span::Id = id.into();
        if let Some(stats) = self.task_stats.get(&task_id) {
            let (tx, rx) = mpsc::channel(buffer);
            let subscription = Watch(tx);
            let now = SystemTime::now();
            // Send back the stream receiver.
            // Then send the initial state --- if this fails, the subscription is already dead.
            if stream_sender.send(rx).is_ok()
                && subscription.update(&proto::tasks::TaskDetails {
                    task_id: Some(task_id.clone().into()),
                    now: Some(now.into()),
                    poll_times_histogram: serialize_histogram(&stats.poll_times_histogram).ok(),
                })
            {
                self.details_watchers
                    .entry(task_id)
                    .or_insert_with(Vec::new)
                    .push(subscription);
            }
        }
        // If the task is not found, drop `stream_sender` which will result in a not found error
    }

    /// Publish the current state to all active watchers.
    ///
    /// This drops any watchers which have closed the RPC, or whose update
    /// channel has filled up.
    fn publish(&mut self) {
        let new_metadata = if !self.new_metadata.is_empty() {
            Some(proto::RegisterMetadata {
                metadata: std::mem::take(&mut self.new_metadata),
            })
        } else {
            None
        };

        let new_poll_ops = if !self.new_poll_ops.is_empty() {
            std::mem::take(&mut self.new_poll_ops)
        } else {
            Vec::default()
        };

        let now = SystemTime::now();
        let update = proto::instrument::InstrumentUpdate {
            now: Some(now.into()),
            new_metadata,
            task_update: Some(proto::tasks::TaskUpdate {
                new_tasks: self
                    .tasks
                    .since_last_update()
                    .map(|(_, value)| value.to_proto())
                    .collect(),
                stats_update: self.task_stats.as_proto(Include::UpdatedOnly),
            }),
            resource_update: Some(proto::resources::ResourceUpdate {
                new_resources: self
                    .resources
                    .since_last_update()
                    .map(|(_, value)| value.to_proto())
                    .collect(),
                stats_update: self.resource_stats.as_proto(Include::UpdatedOnly),
                new_poll_ops,
            }),
            async_op_update: Some(proto::async_ops::AsyncOpUpdate {
                new_async_ops: self
                    .async_ops
                    .since_last_update()
                    .map(|(_, value)| value.to_proto())
                    .collect(),
                stats_update: self.async_op_stats.as_proto(Include::UpdatedOnly),
            }),
        };

        self.watchers
            .retain(|watch: &Watch<proto::instrument::InstrumentUpdate>| watch.update(&update));

        let stats = &self.task_stats;
        // Assuming there are much fewer task details subscribers than there are
        // stats updates, iterate over `details_watchers` and compact the map.
        self.details_watchers.retain(|id, watchers| {
            if let Some(task_stats) = stats.get(id) {
                let details = proto::tasks::TaskDetails {
                    task_id: Some(id.clone().into()),
                    now: Some(now.into()),
                    poll_times_histogram: serialize_histogram(&task_stats.poll_times_histogram)
                        .ok(),
                };
                watchers.retain(|watch| watch.update(&details));
                !watchers.is_empty()
            } else {
                false
            }
        });
    }

    /// Update the current state with data from a single event.
    fn update_state(&mut self, event: Event) {
        // do state update
        match event {
            Event::Metadata(meta) => {
                self.all_metadata.push(meta.into());
                self.new_metadata.push(meta.into());
            }
            Event::Spawn {
                id,
                metadata,
                at,
                fields,
                ..
            } => {
                self.tasks.insert(
                    id.clone(),
                    Task {
                        id: id.clone(),
                        metadata,
                        fields,
                        // TODO: parents
                    },
                );
                self.task_stats.insert(
                    id,
                    TaskStats {
                        created_at: Some(at),
                        ..Default::default()
                    },
                );
            }
            Event::Enter { id, at } => {
                if let Some(mut task_stats) = self.task_stats.update(&id) {
                    task_stats.poll_stats.update_on_span_enter(at);
                }

                if let Some(mut async_op_stats) = self.async_op_stats.update(&id) {
                    async_op_stats.poll_stats.update_on_span_enter(at);
                }
            }

            Event::Exit { id, at } => {
                if let Some(mut task_stats) = self.task_stats.update(&id) {
                    task_stats.poll_stats.update_on_span_exit(at);
                    if let Some(since_last_poll) = task_stats.poll_stats.since_last_poll(at) {
                        task_stats
                            .poll_times_histogram
                            .record(since_last_poll.as_nanos().try_into().unwrap_or(u64::MAX))
                            .unwrap();
                    }
                }

                if let Some(mut async_op_stats) = self.async_op_stats.update(&id) {
                    async_op_stats.poll_stats.update_on_span_exit(at);
                }
            }

            Event::Close { id, at } => {
                if let Some(mut task_stats) = self.task_stats.update(&id) {
                    task_stats.closed_at = Some(at);
                }

                if let Some(mut resource_stats) = self.resource_stats.update(&id) {
                    resource_stats.closed_at = Some(at);
                }

                if let Some(mut async_op_stats) = self.async_op_stats.update(&id) {
                    async_op_stats.closed_at = Some(at);
                }
            }

            Event::Waker { id, op, at } => {
                // It's possible for wakers to exist long after a task has
                // finished. We don't want those cases to create a "new"
                // task that isn't closed, just to insert some waker stats.
                //
                // It may be useful to eventually be able to report about
                // "wasted" waker ops, but we'll leave that for another time.
                if let Some(mut task_stats) = self.task_stats.update(&id) {
                    match op {
                        WakeOp::Wake | WakeOp::WakeByRef => {
                            task_stats.wakes += 1;
                            task_stats.last_wake = Some(at);

                            // Note: `Waker::wake` does *not* call the `drop`
                            // implementation, so waking by value doesn't
                            // trigger a drop event. so, count this as a `drop`
                            // to ensure the task's number of wakers can be
                            // calculated as `clones` - `drops`.
                            //
                            // see
                            // https://github.com/rust-lang/rust/blob/673d0db5e393e9c64897005b470bfeb6d5aec61b/library/core/src/task/wake.rs#L211-L212
                            if let WakeOp::Wake = op {
                                task_stats.waker_drops += 1;
                            }
                        }
                        WakeOp::Clone => {
                            task_stats.waker_clones += 1;
                        }
                        WakeOp::Drop => {
                            task_stats.waker_drops += 1;
                        }
                    }
                }
            }

            Event::Resource {
                at,
                id,
                metadata,
                kind,
                concrete_type,
                ..
            } => {
                self.resources.insert(
                    id.clone(),
                    Resource {
                        id: id.clone(),
                        kind,
                        metadata,
                        concrete_type,
                    },
                );

                self.resource_stats.insert(
                    id,
                    ResourceStats {
                        created_at: Some(at),
                        ..Default::default()
                    },
                );
            }

            Event::PollOp {
                metadata,
                at,
                resource_id,
                op_name,
                async_op_id,
                task_id,
                readiness,
            } => {
                let mut async_op_stats = self.async_op_stats.update_or_default(async_op_id.clone());
                async_op_stats.poll_stats.polls += 1;
                async_op_stats.task_id.get_or_insert(task_id.clone());
                async_op_stats
                    .resource_id
                    .get_or_insert(resource_id.clone());

                if matches!(readiness, Readiness::Pending)
                    && async_op_stats.poll_stats.first_poll.is_none()
                {
                    async_op_stats.poll_stats.first_poll = Some(at);
                }

                let poll_op = proto::resources::PollOp {
                    metadata: Some(metadata.into()),
                    resource_id: Some(resource_id.into()),
                    name: op_name,
                    task_id: Some(task_id.into()),
                    async_op_id: Some(async_op_id.into()),
                    readiness: match readiness {
                        Readiness::Pending => proto::Readiness::Pending,
                        Readiness::Ready => proto::Readiness::Ready,
                    } as i32,
                };

                self.all_poll_ops.push(poll_op.clone());
                self.new_poll_ops.push(poll_op);
            }

            Event::StateUpdate {
                resource_id,
                update,
                ..
            } => {
                if let Some(mut stats) = self.resource_stats.update(&resource_id) {
                    let upd_key = (&update.val).into();
                    match stats.attributes.get_mut(&upd_key) {
                        Some(attr) => update_attribute(attr, update),
                        None => {
                            stats.attributes.insert(upd_key, update.into());
                        }
                    }
                }
            }

            Event::AsyncResourceOp {
                at,
                id,
                source,
                metadata,
                ..
            } => {
                self.async_ops.insert(
                    id.clone(),
                    AsyncOp {
                        id: id.clone(),
                        metadata,
                        source,
                    },
                );

                self.async_op_stats.insert(
                    id,
                    AsyncOpStats {
                        created_at: Some(at),
                        ..Default::default()
                    },
                );
            }
        }
    }
}

// ==== impl Flush ===

impl Flush {
    pub(crate) fn trigger(&self) {
        if self
            .triggered
            .compare_exchange(false, true, AcqRel, Acquire)
            .is_ok()
        {
            self.should_flush.notify_one();
            tracing::trace!("flush triggered");
        } else {
            // someone else already did it, that's fine...
            tracing::trace!("flush already triggered");
        }
    }
}

enum Include {
    All,
    UpdatedOnly,
}

impl<T> IdData<T> {
    fn update_or_default(&mut self, id: span::Id) -> Updating<'_, T>
    where
        T: Default,
    {
        Updating(self.data.entry(id).or_default())
    }

    fn update(&mut self, id: &span::Id) -> Option<Updating<'_, T>> {
        self.data.get_mut(id).map(Updating)
    }

    fn insert(&mut self, id: span::Id, data: T) {
        self.data.insert(id, (data, true));
    }

    fn since_last_update(&mut self) -> impl Iterator<Item = (&span::Id, &mut T)> {
        self.data.iter_mut().filter_map(|(id, (data, dirty))| {
            if *dirty {
                *dirty = false;
                Some((id, data))
            } else {
                None
            }
        })
    }

    fn all(&self) -> impl Iterator<Item = (&span::Id, &T)> {
        self.data.iter().map(|(id, (data, _))| (id, data))
    }

    fn get(&self, id: &span::Id) -> Option<&T> {
        self.data.get(id).map(|(data, _)| data)
    }

    fn as_proto(&mut self, include: Include) -> HashMap<u64, T::Output>
    where
        T: ToProto,
    {
        match include {
            Include::UpdatedOnly => self
                .since_last_update()
                .map(|(id, d)| (id.into_u64(), d.to_proto()))
                .collect(),
            Include::All => self
                .all()
                .map(|(id, d)| (id.into_u64(), d.to_proto()))
                .collect(),
        }
    }

    fn drop_closed<R: Closable>(
        &mut self,
        stats: &mut IdData<R>,
        now: SystemTime,
        retention: Duration,
        has_watchers: bool,
    ) {
        let _span = tracing::debug_span!(
            "drop_closed",
            entity = %std::any::type_name::<T>(),
            stats = %std::any::type_name::<R>(),
        )
        .entered();

        // drop closed entities
        tracing::trace!(?retention, has_watchers, "dropping closed");

        let stats_len_0 = stats.data.len();
        stats.data.retain(|id, (stats, dirty)| {
            if let Some(closed) = stats.closed_at() {
                let closed_for = now.duration_since(closed).unwrap_or_default();
                let should_drop =
                        // if there are any clients watching, retain all dirty tasks regardless of age
                        (*dirty && has_watchers)
                        || closed_for > retention;
                tracing::trace!(
                    stats.id = ?id,
                    stats.closed_at = ?closed,
                    stats.closed_for = ?closed_for,
                    stats.dirty = *dirty,
                    should_drop,
                );
                return !should_drop;
            }

            true
        });

        let stats_len_1 = stats.data.len();

        // drop closed entities which no longer have stats.
        let entities_len_0 = self.data.len();
        self.data.retain(|id, (_, _)| stats.data.contains_key(id));
        let entities_len_1 = self.data.len();
        let dropped_stats = stats_len_0 - stats_len_1;

        let stats_len_1 = stats.data.len();
        if dropped_stats > 0 {
            tracing::debug!(
                tasks.dropped = entities_len_0 - entities_len_1,
                tasks.len = entities_len_1,
                stats.dropped = dropped_stats,
                stats.tasks = stats_len_1,
                "dropped closed entities"
            );
        } else {
            tracing::trace!(
                entities.len = entities_len_1,
                stats.len = stats_len_1,
                "no closed entities were droppable"
            );
        }
    }
}

impl<T> Default for IdData<T> {
    fn default() -> Self {
        IdData {
            data: HashMap::<span::Id, (T, bool)>::new(),
        }
    }
}

struct Updating<'a, T>(&'a mut (T, bool));

impl<'a, T> Deref for Updating<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.0 .0
    }
}

impl<'a, T> DerefMut for Updating<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0 .0
    }
}

impl<'a, T> Drop for Updating<'a, T> {
    fn drop(&mut self) {
        self.0 .1 = true;
    }
}

impl<T: Clone> Watch<T> {
    fn update(&self, update: &T) -> bool {
        if let Ok(reserve) = self.0.try_reserve() {
            reserve.send(Ok(update.clone()));
            true
        } else {
            false
        }
    }
}

impl ToProto for PollStats {
    type Output = proto::PollStats;

    fn to_proto(&self) -> Self::Output {
        proto::PollStats {
            polls: self.polls,
            first_poll: self.first_poll.map(Into::into),
            last_poll_started: self.last_poll_started.map(Into::into),
            last_poll_ended: self.last_poll_ended.map(Into::into),
            busy_time: Some(self.busy_time.into()),
        }
    }
}

impl ToProto for Task {
    type Output = proto::tasks::Task;

    fn to_proto(&self) -> Self::Output {
        proto::tasks::Task {
            id: Some(self.id.clone().into()),
            // TODO: more kinds of tasks...
            kind: proto::tasks::task::Kind::Spawn as i32,
            metadata: Some(self.metadata.into()),
            parents: Vec::new(), // TODO: implement parents nicely
            fields: self.fields.clone(),
        }
    }
}

impl ToProto for TaskStats {
    type Output = proto::tasks::Stats;

    fn to_proto(&self) -> Self::Output {
        proto::tasks::Stats {
            poll_stats: Some(self.poll_stats.to_proto()),
            created_at: self.created_at.map(Into::into),
            total_time: total_time(self.created_at, self.closed_at).map(Into::into),
            wakes: self.wakes,
            waker_clones: self.waker_clones,
            waker_drops: self.waker_drops,
            last_wake: self.last_wake.map(Into::into),
        }
    }
}

impl ToProto for Resource {
    type Output = proto::resources::Resource;

    fn to_proto(&self) -> Self::Output {
        proto::resources::Resource {
            id: Some(self.id.clone().into()),
            kind: Some(self.kind.clone()),
            metadata: Some(self.metadata.into()),
            concrete_type: self.concrete_type.clone(),
        }
    }
}

impl ToProto for ResourceStats {
    type Output = proto::resources::Stats;

    fn to_proto(&self) -> Self::Output {
        let attributes = self.attributes.values().cloned().collect();
        proto::resources::Stats {
            created_at: self.created_at.map(Into::into),
            total_time: total_time(self.created_at, self.closed_at).map(Into::into),
            attributes,
        }
    }
}

impl ToProto for AsyncOp {
    type Output = proto::async_ops::AsyncOp;

    fn to_proto(&self) -> Self::Output {
        proto::async_ops::AsyncOp {
            id: Some(self.id.clone().into()),
            metadata: Some(self.metadata.into()),
            source: self.source.clone(),
        }
    }
}

impl ToProto for AsyncOpStats {
    type Output = proto::async_ops::Stats;

    fn to_proto(&self) -> Self::Output {
        proto::async_ops::Stats {
            poll_stats: Some(self.poll_stats.to_proto()),
            created_at: self.created_at.map(Into::into),
            total_time: total_time(self.created_at, self.closed_at).map(Into::into),

            resource_id: self.resource_id.clone().map(Into::into),
            task_id: self.task_id.clone().map(Into::into),
        }
    }
}

impl From<&proto::Field> for FieldKey {
    fn from(field: &proto::Field) -> Self {
        let meta_id = field
            .metadata_id
            .as_ref()
            .expect("field misses metadata id")
            .id;
        let field_name = field.name.clone().expect("field misses name");
        FieldKey {
            meta_id,
            field_name,
        }
    }
}

impl From<AttributeUpdate> for Attribute {
    fn from(upd: AttributeUpdate) -> Self {
        Attribute {
            value: Some(upd.val),
            unit: upd.unit,
        }
    }
}

fn serialize_histogram(histogram: &Histogram<u64>) -> Result<Vec<u8>, V2SerializeError> {
    let mut serializer = V2Serializer::new();
    let mut buf = Vec::new();
    serializer.serialize(histogram, &mut buf)?;
    Ok(buf)
}

fn total_time(created_at: Option<SystemTime>, closed_at: Option<SystemTime>) -> Option<Duration> {
    let end = closed_at?;
    let start = created_at?;
    end.duration_since(start).ok()
}

fn update_attribute(attribute: &mut Attribute, update: AttributeUpdate) {
    use proto::field::Value::*;
    let attribute_val = attribute.value.as_mut().and_then(|a| a.value.as_mut());
    let update_val = update.val.value;

    match (attribute_val, update_val) {
        (Some(BoolVal(v)), Some(BoolVal(upd))) => *v = upd,

        (Some(StrVal(v)), Some(StrVal(upd))) => *v = upd,

        (Some(DebugVal(v)), Some(DebugVal(upd))) => *v = upd,

        (Some(U64Val(v)), Some(U64Val(upd))) => match update.op {
            AttributeUpdateOp::Add => *v += upd,

            AttributeUpdateOp::Sub => *v -= upd,

            AttributeUpdateOp::Ovr => *v = upd,
        },

        (Some(I64Val(v)), Some(I64Val(upd))) => match update.op {
            AttributeUpdateOp::Add => *v += upd,

            AttributeUpdateOp::Sub => *v -= upd,

            AttributeUpdateOp::Ovr => *v = upd,
        },

        (val, update) => {
            tracing::warn!(
                "attribute {:?} cannot be updated by update {:?}",
                val,
                update
            );
        }
    }
}
