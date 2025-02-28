#![doc = include_str!("../README.md")]
use console_api as proto;
use proto::resources::resource;
use serde::Serialize;
use std::{
    cell::RefCell,
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};
use thread_local::ThreadLocal;
use tokio::sync::{mpsc, oneshot};
use tracing_core::{
    dispatcher::{self, Dispatch},
    span,
    subscriber::{self, NoSubscriber, Subscriber},
    Metadata,
};
use tracing_subscriber::{layer::Context, registry::LookupSpan, Layer};

mod aggregator;
mod builder;
mod callsites;
mod record;
mod stack;
pub(crate) mod sync;
mod visitors;

use aggregator::Aggregator;
pub use builder::Builder;
use callsites::Callsites;
use stack::SpanStack;
use visitors::{AsyncOpVisitor, ResourceVisitor, ResourceVisitorResult, TaskVisitor, WakerVisitor};

pub use builder::{init, spawn};

use crate::aggregator::Id;
use crate::visitors::{PollOpVisitor, StateUpdateVisitor};

/// A [`ConsoleLayer`] is a [`tracing_subscriber::Layer`] that records [`tracing`]
/// spans and events emitted by the async runtime.
///
/// Runtimes emit [`tracing`] spans and events that represent specific operations
/// that occur in asynchronous Rust programs, such as spawning tasks and waker
/// operations. The `ConsoleLayer` collects and aggregates these events, and the
/// resulting diagnostic data is exported to clients by the corresponding gRPC
/// [`Server`] instance.
///
/// [`tracing`]: https://docs.rs/tracing
pub struct ConsoleLayer {
    current_spans: ThreadLocal<RefCell<SpanStack>>,
    tx: mpsc::Sender<Event>,
    shared: Arc<Shared>,
    /// When the channel capacity goes under this number, a flush in the aggregator
    /// will be triggered.
    flush_under_capacity: usize,

    /// Set of callsites for spans representing spawned tasks.
    ///
    /// For task spans, each runtime these will have like, 1-5 callsites in it, max, so
    /// 8 should be plenty. If several runtimes are in use, we may have to spill
    /// over into the backup hashmap, but it's unlikely.
    spawn_callsites: Callsites<8>,

    /// Set of callsites for events representing waker operations.
    ///
    /// 16 is probably a reasonable number of waker ops; it's a bit generous if
    /// there's only one async runtime library in use, but if there are multiple,
    /// they might all have their own sets of waker ops.
    waker_callsites: Callsites<16>,

    /// Set of callsites for spans representing resources
    ///
    /// TODO: Take some time to determine more reasonable numbers
    resource_callsites: Callsites<32>,

    /// Set of callsites for spans representing async operations on resources
    ///
    /// TODO: Take some time to determine more reasonable numbers
    async_op_callsites: Callsites<32>,

    /// Set of callsites for spans representing async op poll operations
    ///
    /// TODO: Take some time to determine more reasonable numbers
    async_op_poll_callsites: Callsites<32>,

    /// Set of callsites for events representing poll operation invocations on resources
    ///
    /// TODO: Take some time to determine more reasonable numbers
    poll_op_callsites: Callsites<32>,

    /// Set of callsites for events representing state attribute state updates on resources
    ///
    /// TODO: Take some time to determine more reasonable numbers
    resource_state_update_callsites: Callsites<32>,

    /// Set of callsites for events representing state attribute state updates on async resource ops
    ///
    /// TODO: Take some time to determine more reasonable numbers
    async_op_state_update_callsites: Callsites<32>,

    /// Used for unsetting the default dispatcher inside of span callbacks.
    no_dispatch: Dispatch,
}

/// A gRPC [`Server`] that implements the [`tokio-console` wire format][wire].
///
/// Client applications, such as the [`tokio-console CLI][cli] connect to the gRPC
/// server, and stream data about the runtime's history (such as a list of the
/// currently active tasks, or statistics summarizing polling times). A [`Server`] also
/// interprets commands from a client application, such a request to focus in on
/// a specific task, and translates that into a stream of details specific to
/// that task.
///
/// [wire]: https://docs.rs/console-api
/// [cli]: https://crates.io/crates/tokio-console
pub struct Server {
    subscribe: mpsc::Sender<Command>,
    addr: SocketAddr,
    aggregator: Option<Aggregator>,
    client_buffer: usize,
}

/// State shared between the `ConsoleLayer` and the `Aggregator` task.
#[derive(Debug, Default)]
struct Shared {
    /// Used to notify the aggregator task when the event buffer should be
    /// flushed.
    flush: aggregator::Flush,

    /// A counter of how many task events were dropped because the event buffer
    /// was at capacity.
    dropped_tasks: AtomicUsize,

    /// A counter of how many async op events were dropped because the event buffer
    /// was at capacity.
    dropped_async_ops: AtomicUsize,

    /// A counter of how many resource events were dropped because the event buffer
    /// was at capacity.
    dropped_resources: AtomicUsize,
}

struct Watch<T>(mpsc::Sender<Result<T, tonic::Status>>);

enum Command {
    Instrument(Watch<proto::instrument::Update>),
    WatchTaskDetail(WatchRequest<proto::tasks::TaskDetails>),
    Pause,
    Resume,
}

struct WatchRequest<T> {
    id: Id,
    stream_sender: oneshot::Sender<mpsc::Receiver<Result<T, tonic::Status>>>,
    buffer: usize,
}

#[derive(Debug)]
enum Event {
    Metadata(&'static Metadata<'static>),
    Spawn {
        id: span::Id,
        metadata: &'static Metadata<'static>,
        at: SystemTime,
        fields: Vec<proto::Field>,
        location: Option<proto::Location>,
    },
    Enter {
        id: span::Id,
        parent_id: Option<span::Id>,
        at: SystemTime,
    },
    Exit {
        id: span::Id,
        parent_id: Option<span::Id>,
        at: SystemTime,
    },
    Close {
        id: span::Id,
        at: SystemTime,
    },
    Waker {
        id: span::Id,
        op: WakeOp,
        at: SystemTime,
    },
    Resource {
        id: span::Id,
        parent_id: Option<span::Id>,
        metadata: &'static Metadata<'static>,
        at: SystemTime,
        concrete_type: String,
        kind: resource::Kind,
        location: Option<proto::Location>,
        is_internal: bool,
        inherit_child_attrs: bool,
    },
    PollOp {
        metadata: &'static Metadata<'static>,
        resource_id: span::Id,
        op_name: String,
        async_op_id: span::Id,
        task_id: span::Id,
        is_ready: bool,
    },
    StateUpdate {
        update_id: span::Id,
        update_type: UpdateType,
        update: AttributeUpdate,
    },
    AsyncResourceOp {
        id: span::Id,
        parent_id: Option<span::Id>,
        resource_id: span::Id,
        metadata: &'static Metadata<'static>,
        at: SystemTime,
        source: String,
        inherit_child_attrs: bool,
    },
}

#[derive(Debug, Clone)]
enum UpdateType {
    Resource,
    AsyncOp,
}

#[derive(Debug, Clone)]
struct AttributeUpdate {
    field: proto::Field,
    op: Option<AttributeUpdateOp>,
    unit: Option<String>,
}

#[derive(Debug, Clone)]
enum AttributeUpdateOp {
    Add,
    Override,
    Sub,
}

#[derive(Clone, Debug, Copy, Serialize)]
enum WakeOp {
    Wake { self_wake: bool },
    WakeByRef { self_wake: bool },
    Clone,
    Drop,
}

/// Marker type used to indicate that a span is actually tracked by the console.
#[derive(Debug)]
struct Tracked {}

impl ConsoleLayer {
    /// Returns a `ConsoleLayer` built with the default settings.
    ///
    /// Note: these defaults do *not* include values provided via the
    /// environment variables specified in [`Builder::with_default_env`].
    ///
    /// See also [`Builder::build`].
    pub fn new() -> (Self, Server) {
        Self::builder().build()
    }

    /// Returns a [`Builder`] for configuring a `ConsoleLayer`.
    ///
    /// Note that the returned builder does *not* include values provided via
    /// the environment variables specified in [`Builder::with_default_env`].
    /// To extract those, you can call that method on the returned builder.
    pub fn builder() -> Builder {
        Builder::default()
    }

    fn build(config: Builder) -> (Self, Server) {
        // The `cfg` value *appears* to be a constant to clippy, but it changes
        // depending on the build-time configuration...
        #![allow(clippy::assertions_on_constants)]
        assert!(
            cfg!(tokio_unstable),
            "task tracing requires Tokio to be built with RUSTFLAGS=\"--cfg tokio_unstable\"!"
        );
        tracing::debug!(
            config.event_buffer_capacity,
            config.client_buffer_capacity,
            ?config.publish_interval,
            ?config.retention,
            ?config.server_addr,
            ?config.recording_path,
            "configured console subscriber"
        );

        let (tx, events) = mpsc::channel(config.event_buffer_capacity);
        let (subscribe, rpcs) = mpsc::channel(256);
        let shared = Arc::new(Shared::default());
        let aggregator = Aggregator::new(events, rpcs, &config, shared.clone());
        // Conservatively, start to trigger a flush when half the channel is full.
        // This tries to reduce the chance of losing events to a full channel.
        let flush_under_capacity = config.event_buffer_capacity / 2;

        let server = Server {
            aggregator: Some(aggregator),
            addr: config.server_addr,
            subscribe,
            client_buffer: config.client_buffer_capacity,
        };
        let layer = Self {
            current_spans: ThreadLocal::new(),
            tx,
            shared,
            flush_under_capacity,
            spawn_callsites: Callsites::default(),
            waker_callsites: Callsites::default(),
            resource_callsites: Callsites::default(),
            async_op_callsites: Callsites::default(),
            async_op_poll_callsites: Callsites::default(),
            poll_op_callsites: Callsites::default(),
            resource_state_update_callsites: Callsites::default(),
            async_op_state_update_callsites: Callsites::default(),
            no_dispatch: Dispatch::new(NoSubscriber::default()),
        };
        (layer, server)
    }
}

impl ConsoleLayer {
    /// Default maximum capacity for the channel of events sent from a
    /// [`ConsoleLayer`] to a [`Server`].
    ///
    /// When this capacity is exhausted, additional events will be dropped.
    /// Decreasing this value will reduce memory usage, but may result in
    /// events being dropped more frequently.
    ///
    /// See also [`Builder::event_buffer_capacity`].
    pub const DEFAULT_EVENT_BUFFER_CAPACITY: usize = 1024 * 100;
    /// Default maximum capacity for th echannel of events sent from a
    /// [`Server`] to each subscribed client.
    ///
    /// When this capacity is exhausted, the client is assumed to be inactive,
    /// and may be disconnected.
    ///
    /// See also [`Builder::client_buffer_capacity`].
    pub const DEFAULT_CLIENT_BUFFER_CAPACITY: usize = 1024 * 4;

    /// Default frequency for publishing events to clients.
    ///
    /// Note that methods like [`init`][`crate::init`] and [`spawn`][`crate::spawn`] will take the value
    /// from the `TOKIO_CONSOLE_PUBLISH_INTERVAL` [environment variable] before falling
    /// back on this default.
    ///
    /// See also [`Builder::publish_interval`].
    ///
    /// [environment variable]: `Builder::with_default_env`
    pub const DEFAULT_PUBLISH_INTERVAL: Duration = Duration::from_secs(1);

    /// By default, completed spans are retained for one hour.
    ///
    /// Note that methods like [`init`][`crate::init`] and
    /// [`spawn`][`crate::spawn`] will take the value from the
    /// `TOKIO_CONSOLE_RETENTION` [environment variable] before falling back on
    /// this default.
    ///
    /// See also [`Builder::retention`].
    ///
    /// [environment variable]: `Builder::with_default_env`
    pub const DEFAULT_RETENTION: Duration = Duration::from_secs(60 * 60);

    fn is_spawn(&self, meta: &'static Metadata<'static>) -> bool {
        self.spawn_callsites.contains(meta)
    }

    fn is_resource(&self, meta: &'static Metadata<'static>) -> bool {
        self.resource_callsites.contains(meta)
    }

    fn is_async_op(&self, meta: &'static Metadata<'static>) -> bool {
        self.async_op_callsites.contains(meta)
    }

    fn is_id_spawned<S>(&self, id: &span::Id, cx: &Context<'_, S>) -> bool
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        cx.span(id)
            .map(|span| self.is_spawn(span.metadata()))
            .unwrap_or(false)
    }

    fn is_id_resource<S>(&self, id: &span::Id, cx: &Context<'_, S>) -> bool
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        cx.span(id)
            .map(|span| self.is_resource(span.metadata()))
            .unwrap_or(false)
    }

    fn is_id_async_op<S>(&self, id: &span::Id, cx: &Context<'_, S>) -> bool
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        cx.span(id)
            .map(|span| self.is_async_op(span.metadata()))
            .unwrap_or(false)
    }

    fn is_id_tracked<S>(&self, id: &span::Id, cx: &Context<'_, S>) -> bool
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        cx.span(id)
            .map(|span| span.extensions().get::<Tracked>().is_some())
            .unwrap_or(false)
    }

    fn first_entered<P>(&self, stack: &SpanStack, p: P) -> Option<span::Id>
    where
        P: Fn(&span::Id) -> bool,
    {
        stack
            .stack()
            .iter()
            .rev()
            .find(|id| p(id.id()))
            .map(|id| id.id())
            .cloned()
    }

    fn send(&self, dropped: &AtomicUsize, event: Event) -> bool {
        use mpsc::error::TrySendError;

        // Return whether or not we actually sent the event.
        let sent = match self.tx.try_reserve() {
            Ok(permit) => {
                permit.send(event);
                true
            }
            Err(TrySendError::Closed(_)) => {
                // we should warn here eventually, but nop for now because we
                // can't trigger tracing events...
                false
            }
            Err(TrySendError::Full(_)) => {
                // this shouldn't happen, since we trigger a flush when
                // approaching the high water line...but if the executor wait
                // time is very high, maybe the aggregator task hasn't been
                // polled yet. so... eek?!
                dropped.fetch_add(1, Ordering::Release);
                false
            }
        };

        let capacity = self.tx.capacity();
        if capacity <= self.flush_under_capacity {
            self.shared.flush.trigger();
        }

        sent
    }
}

impl<S> Layer<S> for ConsoleLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn register_callsite(&self, meta: &'static Metadata<'static>) -> subscriber::Interest {
        let dropped = match (meta.name(), meta.target()) {
            ("runtime.spawn", _) | ("task", "tokio::task") => {
                self.spawn_callsites.insert(meta);
                &self.shared.dropped_tasks
            }
            (_, "runtime::waker") | (_, "tokio::task::waker") => {
                self.waker_callsites.insert(meta);
                &self.shared.dropped_tasks
            }
            (ResourceVisitor::RES_SPAN_NAME, _) => {
                self.resource_callsites.insert(meta);
                &self.shared.dropped_resources
            }
            (AsyncOpVisitor::ASYNC_OP_SPAN_NAME, _) => {
                self.async_op_callsites.insert(meta);
                &self.shared.dropped_async_ops
            }
            ("runtime.resource.async_op.poll", _) => {
                self.async_op_poll_callsites.insert(meta);
                &self.shared.dropped_async_ops
            }
            (_, PollOpVisitor::POLL_OP_EVENT_TARGET) => {
                self.poll_op_callsites.insert(meta);
                &self.shared.dropped_async_ops
            }
            (_, StateUpdateVisitor::RE_STATE_UPDATE_EVENT_TARGET) => {
                self.resource_state_update_callsites.insert(meta);
                &self.shared.dropped_resources
            }
            (_, StateUpdateVisitor::AO_STATE_UPDATE_EVENT_TARGET) => {
                self.async_op_state_update_callsites.insert(meta);
                &self.shared.dropped_async_ops
            }
            (_, _) => &self.shared.dropped_tasks,
        };

        self.send(dropped, Event::Metadata(meta));
        subscriber::Interest::always()
    }

    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let metadata = attrs.metadata();
        let sent = if self.is_spawn(metadata) {
            let at = SystemTime::now();
            let mut task_visitor = TaskVisitor::new(metadata.into());
            attrs.record(&mut task_visitor);
            let (fields, location) = task_visitor.result();
            self.send(
                &self.shared.dropped_tasks,
                Event::Spawn {
                    id: id.clone(),
                    at,
                    metadata,
                    fields,
                    location,
                },
            )
        } else if self.is_resource(metadata) {
            let mut resource_visitor = ResourceVisitor::default();
            attrs.record(&mut resource_visitor);
            if let Some(result) = resource_visitor.result() {
                let ResourceVisitorResult {
                    concrete_type,
                    kind,
                    location,
                    is_internal,
                    inherit_child_attrs,
                } = result;
                let at = SystemTime::now();
                let parent_id = self.current_spans.get().and_then(|stack| {
                    self.first_entered(&stack.borrow(), |id| self.is_id_resource(id, &ctx))
                });
                self.send(
                    &self.shared.dropped_resources,
                    Event::Resource {
                        id: id.clone(),
                        parent_id,
                        metadata,
                        at,
                        concrete_type,
                        kind,
                        location,
                        is_internal,
                        inherit_child_attrs,
                    },
                )
            } else {
                // else unknown resource span format
                false
            }
        } else if self.is_async_op(metadata) {
            let mut async_op_visitor = AsyncOpVisitor::default();
            attrs.record(&mut async_op_visitor);
            if let Some((source, inherit_child_attrs)) = async_op_visitor.result() {
                let at = SystemTime::now();
                let resource_id = self.current_spans.get().and_then(|stack| {
                    self.first_entered(&stack.borrow(), |id| self.is_id_resource(id, &ctx))
                });

                let parent_id = self.current_spans.get().and_then(|stack| {
                    self.first_entered(&stack.borrow(), |id| self.is_id_async_op(id, &ctx))
                });

                if let Some(resource_id) = resource_id {
                    self.send(
                        &self.shared.dropped_async_ops,
                        Event::AsyncResourceOp {
                            id: id.clone(),
                            parent_id,
                            resource_id,
                            at,
                            metadata,
                            source,
                            inherit_child_attrs,
                        },
                    )
                } else {
                    false
                }
            } else {
                // else async op span needs to have a source field
                false
            }
        } else {
            false
        };

        // If we were able to record the span, add a marker extension indicating
        // that it's tracked by the console.
        if sent {
            if let Some(span) = ctx.span(id) {
                span.extensions_mut().insert(Tracked {});
            } else {
                debug_assert!(
                    false,
                    "span should exist if `on_new_span` was called for its ID ({:?})",
                    id
                );
            }
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let metadata = event.metadata();
        if self.waker_callsites.contains(metadata) {
            let at = SystemTime::now();
            let mut visitor = WakerVisitor::default();
            event.record(&mut visitor);
            if let Some((id, mut op)) = visitor.result() {
                if op.is_wake() {
                    // Are we currently inside the task's span? If so, the task
                    // has woken itself.
                    let self_wake = self
                        .current_spans
                        .get()
                        .map(|spans| spans.borrow().iter().any(|span| span == &id))
                        .unwrap_or(false);
                    op = op.self_wake(self_wake);
                }
                self.send(&self.shared.dropped_tasks, Event::Waker { id, op, at });
            }
            // else unknown waker event... what to do? can't trace it from here...
            return;
        }

        if self.poll_op_callsites.contains(metadata) {
            let resource_id = self.current_spans.get().and_then(|stack| {
                self.first_entered(&stack.borrow(), |id| self.is_id_resource(id, &ctx))
            });
            // poll op event should have a resource span parent
            if let Some(resource_id) = resource_id {
                let mut poll_op_visitor = PollOpVisitor::default();
                event.record(&mut poll_op_visitor);
                if let Some((op_name, is_ready)) = poll_op_visitor.result() {
                    let task_and_async_op_ids = self.current_spans.get().and_then(|stack| {
                        let stack = stack.borrow();
                        let task_id =
                            self.first_entered(&stack, |id| self.is_id_spawned(id, &ctx))?;
                        let async_op_id =
                            self.first_entered(&stack, |id| self.is_id_async_op(id, &ctx))?;
                        Some((task_id, async_op_id))
                    });

                    // poll op event should be emitted in the context of an async op and task spans
                    if let Some((task_id, async_op_id)) = task_and_async_op_ids {
                        self.send(
                            &self.shared.dropped_async_ops,
                            Event::PollOp {
                                metadata,
                                op_name,
                                resource_id,
                                async_op_id,
                                task_id,
                                is_ready,
                            },
                        );
                    }
                }
            }
            return;
        }

        if self.resource_state_update_callsites.contains(metadata) {
            // state update event should have a resource span parent
            let resource_id = self.current_spans.get().and_then(|stack| {
                self.first_entered(&stack.borrow(), |id| self.is_id_resource(id, &ctx))
            });

            if let Some(resource_id) = resource_id {
                let meta_id = event.metadata().into();
                let mut state_update_visitor = StateUpdateVisitor::new(meta_id);
                event.record(&mut state_update_visitor);
                if let Some(update) = state_update_visitor.result() {
                    self.send(
                        &self.shared.dropped_resources,
                        Event::StateUpdate {
                            update_id: resource_id,
                            update_type: UpdateType::Resource,
                            update,
                        },
                    );
                }
            }
            return;
        }

        if self.async_op_state_update_callsites.contains(metadata) {
            let async_op_id = self.current_spans.get().and_then(|stack| {
                self.first_entered(&stack.borrow(), |id| self.is_id_async_op(id, &ctx))
            });
            if let Some(async_op_id) = async_op_id {
                let meta_id = event.metadata().into();
                let mut state_update_visitor = StateUpdateVisitor::new(meta_id);
                event.record(&mut state_update_visitor);
                if let Some(update) = state_update_visitor.result() {
                    self.send(
                        &self.shared.dropped_async_ops,
                        Event::StateUpdate {
                            update_id: async_op_id,
                            update_type: UpdateType::AsyncOp,
                            update,
                        },
                    );
                }
            }
        }
    }

    fn on_enter(&self, id: &span::Id, cx: Context<'_, S>) {
        if !self.is_id_tracked(id, &cx) {
            return;
        }
        let _default = dispatcher::set_default(&self.no_dispatch);
        let parent_id = cx.span(id).and_then(|s| s.parent().map(|p| p.id()));
        let sent = self.send(
            &self.shared.dropped_tasks,
            Event::Enter {
                at: SystemTime::now(),
                id: id.clone(),
                parent_id,
            },
        );

        // if we were able to record the send successfully, track entering the
        // span. if not, ignore the enter, to avoid inconsistent data.
        if sent {
            self.current_spans
                .get_or_default()
                .borrow_mut()
                .push(id.clone());
        }
    }

    fn on_exit(&self, id: &span::Id, cx: Context<'_, S>) {
        if !self.is_id_tracked(id, &cx) {
            return;
        }

        let _default = dispatcher::set_default(&self.no_dispatch);
        if let Some(spans) = self.current_spans.get() {
            if !spans.borrow_mut().pop(id) {
                // we did not actually pop the span --- entering it may not have
                // been successfully recorded. in this case, ignore the exit,
                // since the aggregator was never informed of the entry.
                return;
            }
        }

        let parent_id = cx.span(id).and_then(|s| s.parent().map(|p| p.id()));

        self.send(
            &self.shared.dropped_tasks,
            Event::Exit {
                id: id.clone(),
                parent_id,
                at: SystemTime::now(),
            },
        );
    }

    fn on_close(&self, id: span::Id, cx: Context<'_, S>) {
        if !self.is_id_tracked(&id, &cx) {
            return;
        }

        let _default = dispatcher::set_default(&self.no_dispatch);
        self.send(
            &self.shared.dropped_tasks,
            Event::Close {
                at: SystemTime::now(),
                id,
            },
        );
    }
}

impl fmt::Debug for ConsoleLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConsoleLayer")
            // mpsc::Sender debug impl is not very useful
            .field("tx", &format_args!("<...>"))
            .field("tx.capacity", &self.tx.capacity())
            .field("shared", &self.shared)
            .field("spawn_callsites", &self.spawn_callsites)
            .field("waker_callsites", &self.waker_callsites)
            .finish()
    }
}

impl Server {
    // XXX(eliza): why is `SocketAddr::new` not `const`???
    /// A [`Server`] by default binds socket address 127.0.0.1 to service remote
    /// procedure calls.
    ///
    /// Note that methods like [`init`][`crate::init`] and
    /// [`spawn`][`crate::spawn`] will parse the socket address from the
    /// `TOKIO_CONSOLE_BIND` [environment variable] before falling back on
    /// constructing a socket address from this default.
    ///
    /// See also [`Builder::server_addr`].
    ///
    /// [environment variable]: `Builder::with_default_env`
    pub const DEFAULT_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

    /// A [`Server`] by default binds port 6669 to service remote procedure
    /// calls.
    ///
    /// Note that methods like [`init`][`crate::init`] and
    /// [`spawn`][`crate::spawn`] will parse the socket address from the
    /// `TOKIO_CONSOLE_BIND` [environment variable] before falling back on
    /// constructing a socket address from this default.
    ///
    /// See also [`Builder::server_addr`].
    ///
    /// [environment variable]: `Builder::with_default_env`
    pub const DEFAULT_PORT: u16 = 6669;

    /// Starts the gRPC service with the default gRPC settings.
    ///
    /// To configure gRPC server settings before starting the server, use
    /// [`serve_with`] instead. This method is equivalent to calling [`serve_with`]
    /// and providing the default gRPC server settings:
    ///
    /// ```rust
    /// # async fn docs() -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    /// # let (_, server) = console_subscriber::ConsoleLayer::new();
    /// server.serve_with(tonic::transport::Server::default()).await
    /// # }
    /// ```
    /// [`serve_with`]: Server::serve_with
    pub async fn serve(self) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        self.serve_with(tonic::transport::Server::default()).await
    }

    /// Starts the gRPC service with the given [`tonic`] gRPC transport server
    /// `builder`.
    ///
    /// The `builder` parameter may be used to configure gRPC-specific settings
    /// prior to starting the server.
    ///
    /// This spawns both the server task and the event aggregation worker
    /// task on the current async runtime.
    ///
    /// [`tonic`]: https://docs.rs/tonic/
    pub async fn serve_with(
        mut self,
        mut builder: tonic::transport::Server,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        let aggregate = self
            .aggregator
            .take()
            .expect("cannot start server multiple times");
        let aggregate = spawn_named(aggregate.run(), "console::aggregate");
        let addr = self.addr;
        let serve = builder
            .add_service(proto::instrument::instrument_server::InstrumentServer::new(
                self,
            ))
            .serve(addr);
        let res = spawn_named(serve, "console::serve").await;
        aggregate.abort();
        res?.map_err(Into::into)
    }
}

#[tonic::async_trait]
impl proto::instrument::instrument_server::Instrument for Server {
    type WatchUpdatesStream =
        tokio_stream::wrappers::ReceiverStream<Result<proto::instrument::Update, tonic::Status>>;
    type WatchTaskDetailsStream =
        tokio_stream::wrappers::ReceiverStream<Result<proto::tasks::TaskDetails, tonic::Status>>;
    async fn watch_updates(
        &self,
        req: tonic::Request<proto::instrument::InstrumentRequest>,
    ) -> Result<tonic::Response<Self::WatchUpdatesStream>, tonic::Status> {
        match req.remote_addr() {
            Some(addr) => tracing::debug!(client.addr = %addr, "starting a new watch"),
            None => tracing::debug!(client.addr = %"<unknown>", "starting a new watch"),
        }
        let permit = self.subscribe.reserve().await.map_err(|_| {
            tonic::Status::internal("cannot start new watch, aggregation task is not running")
        })?;
        let (tx, rx) = mpsc::channel(self.client_buffer);
        permit.send(Command::Instrument(Watch(tx)));
        tracing::debug!("watch started");
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(tonic::Response::new(stream))
    }

    async fn watch_task_details(
        &self,
        req: tonic::Request<proto::instrument::TaskDetailsRequest>,
    ) -> Result<tonic::Response<Self::WatchTaskDetailsStream>, tonic::Status> {
        let task_id = req
            .into_inner()
            .id
            .ok_or_else(|| tonic::Status::invalid_argument("missing task_id"))?;
        let permit = self.subscribe.reserve().await.map_err(|_| {
            tonic::Status::internal("cannot start new watch, aggregation task is not running")
        })?;

        // Check with the aggregator task to request a stream if the task exists.
        let (stream_sender, stream_recv) = oneshot::channel();
        permit.send(Command::WatchTaskDetail(WatchRequest {
            id: task_id.into(),
            stream_sender,
            buffer: self.client_buffer,
        }));
        // If the aggregator drops the sender, the task doesn't exist.
        let rx = stream_recv.await.map_err(|_| {
            tracing::warn!(id = ?task_id, "requested task not found");
            tonic::Status::not_found("task not found")
        })?;

        tracing::debug!(id = ?task_id, "task details watch started");
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(tonic::Response::new(stream))
    }

    async fn pause(
        &self,
        _req: tonic::Request<proto::instrument::PauseRequest>,
    ) -> Result<tonic::Response<proto::instrument::PauseResponse>, tonic::Status> {
        self.subscribe.send(Command::Pause).await.map_err(|_| {
            tonic::Status::internal("cannot pause, aggregation task is not running")
        })?;
        Ok(tonic::Response::new(proto::instrument::PauseResponse {}))
    }

    async fn resume(
        &self,
        _req: tonic::Request<proto::instrument::ResumeRequest>,
    ) -> Result<tonic::Response<proto::instrument::ResumeResponse>, tonic::Status> {
        self.subscribe.send(Command::Resume).await.map_err(|_| {
            tonic::Status::internal("cannot resume, aggregation task is not running")
        })?;
        Ok(tonic::Response::new(proto::instrument::ResumeResponse {}))
    }
}

impl WakeOp {
    /// Returns `true` if `self` is a `Wake` or `WakeByRef` event.
    fn is_wake(self) -> bool {
        matches!(self, Self::Wake { .. } | Self::WakeByRef { .. })
    }

    fn self_wake(self, self_wake: bool) -> Self {
        match self {
            Self::Wake { .. } => Self::Wake { self_wake },
            Self::WakeByRef { .. } => Self::WakeByRef { self_wake },
            x => x,
        }
    }
}

#[track_caller]
pub(crate) fn spawn_named<T>(
    task: impl std::future::Future<Output = T> + Send + 'static,
    _name: &str,
) -> tokio::task::JoinHandle<T>
where
    T: Send + 'static,
{
    #[cfg(tokio_unstable)]
    return tokio::task::Builder::new().name(_name).spawn(task);

    #[cfg(not(tokio_unstable))]
    tokio::spawn(task)
}
