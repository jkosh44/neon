//! WAL receiver manages an open connection to safekeeper, to get the WAL it streams into.
//! To do so, a current implementation needs to do the following:
//!
//! * acknowledge the timelines that it needs to stream WAL into.
//! Pageserver is able to dynamically (un)load tenants on attach and detach,
//! hence WAL receiver needs to react on such events.
//!
//! * get a broker subscription, stream data from it to determine that a timeline needs WAL streaming.
//! For that, it watches specific keys in storage_broker and pulls the relevant data periodically.
//! The data is produced by safekeepers, that push it periodically and pull it to synchronize between each other.
//! Without this data, no WAL streaming is possible currently.
//!
//! Only one active WAL streaming connection is allowed at a time.
//! The connection is supposed to be updated periodically, based on safekeeper timeline data.
//!
//! * handle the actual connection and WAL streaming
//!
//! Handling happens dynamically, by portions of WAL being processed and registered in the server.
//! Along with the registration, certain metadata is written to show WAL streaming progress and rely on that when considering safekeepers for connection.
//!
//! The current module contains high-level primitives used in the submodules; general synchronization, timeline acknowledgement and shutdown logic.

mod connection_manager;
mod walreceiver_connection;

use crate::context::{DownloadBehavior, RequestContext};
use crate::task_mgr::{self, TaskKind, WALRECEIVER_RUNTIME};
use crate::tenant::timeline::walreceiver::connection_manager::{
    connection_manager_loop_step, ConnectionManagerState,
};

use anyhow::Context;
use std::future::Future;
use std::num::NonZeroU64;
use std::ops::ControlFlow;
use std::sync::atomic::{self, AtomicBool};
use std::sync::{Arc, Weak};
use std::time::Duration;
use storage_broker::BrokerClientChannel;
use tokio::select;
use tokio::sync::{watch, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::*;

use utils::id::TenantTimelineId;

use self::connection_manager::ConnectionManagerStatus;

use super::Timeline;

#[derive(Clone)]
pub struct WalReceiverConf {
    /// The timeout on the connection to safekeeper for WAL streaming.
    pub wal_connect_timeout: Duration,
    /// The timeout to use to determine when the current connection is "stale" and reconnect to the other one.
    pub lagging_wal_timeout: Duration,
    /// The Lsn lag to use to determine when the current connection is lagging to much behind and reconnect to the other one.
    pub max_lsn_wal_lag: NonZeroU64,
    pub auth_token: Option<Arc<String>>,
    pub availability_zone: Option<String>,
}

pub struct WalReceiver {
    timeline: TenantTimelineId,
    timeline_ref: Weak<Timeline>,
    conf: WalReceiverConf,
    started: AtomicBool,
    manager_status: Arc<RwLock<Option<ConnectionManagerStatus>>>,
}

impl WalReceiver {
    pub fn new(
        timeline: TenantTimelineId,
        timeline_ref: Weak<Timeline>,
        conf: WalReceiverConf,
    ) -> Self {
        Self {
            timeline,
            timeline_ref,
            conf,
            started: AtomicBool::new(false),
            manager_status: Arc::new(RwLock::new(None)),
        }
    }

    pub fn start(
        &self,
        ctx: &RequestContext,
        mut broker_client: BrokerClientChannel,
    ) -> anyhow::Result<()> {
        if self.started.load(atomic::Ordering::Acquire) {
            anyhow::bail!("Wal receiver is already started");
        }

        let timeline = self.timeline_ref.upgrade().with_context(|| {
            format!("walreceiver start on a dropped timeline {}", self.timeline)
        })?;

        let tenant_id = timeline.tenant_id;
        let timeline_id = timeline.timeline_id;
        let walreceiver_ctx =
            ctx.detached_child(TaskKind::WalReceiverManager, DownloadBehavior::Error);
        let wal_receiver_conf = self.conf.clone();
        let loop_status = Arc::clone(&self.manager_status);
        task_mgr::spawn(
            WALRECEIVER_RUNTIME.handle(),
            TaskKind::WalReceiverManager,
            Some(tenant_id),
            Some(timeline_id),
            &format!("walreceiver for timeline {tenant_id}/{timeline_id}"),
            false,
            async move {
                info!("WAL receiver manager started, connecting to broker");
                let mut connection_manager_state = ConnectionManagerState::new(
                    timeline,
                    wal_receiver_conf,
                );
                loop {
                    select! {
                        _ = task_mgr::shutdown_watcher() => {
                            info!("WAL receiver shutdown requested, shutting down");
                            break;
                        },
                        loop_step_result = connection_manager_loop_step(
                            &mut broker_client,
                            &mut connection_manager_state,
                            &walreceiver_ctx,
                            &loop_status,
                        ) => match loop_step_result {
                            ControlFlow::Continue(()) => continue,
                            ControlFlow::Break(()) => {
                                info!("Connection manager loop ended, shutting down");
                                break;
                            }
                        },
                    }
                }

                connection_manager_state.shutdown().await;
                *loop_status.write().await = None;
                Ok(())
            }
            .instrument(info_span!(parent: None, "wal_connection_manager", tenant = %tenant_id, timeline = %timeline_id))
        );

        self.started.store(true, atomic::Ordering::Release);

        Ok(())
    }

    pub async fn stop(&self) {
        task_mgr::shutdown_tasks(
            Some(TaskKind::WalReceiverManager),
            Some(self.timeline.tenant_id),
            Some(self.timeline.timeline_id),
        )
        .await;
        self.started.store(false, atomic::Ordering::Release);
    }

    pub(super) async fn status(&self) -> Option<ConnectionManagerStatus> {
        self.manager_status.read().await.clone()
    }
}

/// A handle of an asynchronous task.
/// The task has a channel that it can use to communicate its lifecycle events in a certain form, see [`TaskEvent`]
/// and a cancellation token that it can listen to for earlier interrupts.
///
/// Note that the communication happens via the `watch` channel, that does not accumulate the events, replacing the old one with the never one on submission.
/// That may lead to certain events not being observed by the listener.
#[derive(Debug)]
struct TaskHandle<E> {
    join_handle: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    events_receiver: watch::Receiver<TaskStateUpdate<E>>,
    cancellation: CancellationToken,
}

enum TaskEvent<E> {
    Update(TaskStateUpdate<E>),
    End(anyhow::Result<()>),
}

#[derive(Debug, Clone)]
enum TaskStateUpdate<E> {
    Started,
    Progress(E),
}

impl<E: Clone> TaskHandle<E> {
    /// Initializes the task, starting it immediately after the creation.
    fn spawn<Fut>(
        task: impl FnOnce(watch::Sender<TaskStateUpdate<E>>, CancellationToken) -> Fut + Send + 'static,
    ) -> Self
    where
        Fut: Future<Output = anyhow::Result<()>> + Send,
        E: Send + Sync + 'static,
    {
        let cancellation = CancellationToken::new();
        let (events_sender, events_receiver) = watch::channel(TaskStateUpdate::Started);

        let cancellation_clone = cancellation.clone();
        let join_handle = WALRECEIVER_RUNTIME.spawn(async move {
            events_sender.send(TaskStateUpdate::Started).ok();
            task(events_sender, cancellation_clone).await
            // events_sender is dropped at some point during the .await above.
            // But the task is still running on WALRECEIVER_RUNTIME.
            // That is the window when `!jh.is_finished()`
            // is true inside `fn next_task_event()` below.
        });

        TaskHandle {
            join_handle: Some(join_handle),
            events_receiver,
            cancellation,
        }
    }

    async fn next_task_event(&mut self) -> TaskEvent<E> {
        match self.events_receiver.changed().await {
            Ok(()) => TaskEvent::Update((self.events_receiver.borrow()).clone()),
            Err(_task_channel_part_dropped) => {
                TaskEvent::End(match self.join_handle.as_mut() {
                    Some(jh) => {
                        if !jh.is_finished() {
                            // Barring any implementation errors in this module, we can
                            // only arrive here while the task that executes the future
                            // passed to `Self::spawn()` is still execution. Cf the comment
                            // in Self::spawn().
                            //
                            // This was logging at warning level in earlier versions, presumably
                            // to leave some breadcrumbs in case we had an implementation
                            // error that would would make us get stuck in `jh.await`.
                            //
                            // There hasn't been such a bug so far.
                            // But in a busy system, e.g., during pageserver restart,
                            // we arrive here often enough that the warning-level logs
                            // became a distraction.
                            // So, tone them down to info-level.
                            //
                            // XXX: rewrite this module to eliminate the race condition.
                            info!("sender is dropped while join handle is still alive");
                        }

                        let res = jh
                            .await
                            .map_err(|e| anyhow::anyhow!("Failed to join task: {e}"))
                            .and_then(|x| x);

                        // For cancellation-safety, drop join_handle only after successful .await.
                        self.join_handle = None;

                        res
                    }
                    None => {
                        // Another option is to have an enum, join handle or result and give away the reference to it
                        Err(anyhow::anyhow!("Task was joined more than once"))
                    }
                })
            }
        }
    }

    /// Aborts current task, waiting for it to finish.
    async fn shutdown(self) {
        if let Some(jh) = self.join_handle {
            self.cancellation.cancel();
            match jh.await {
                Ok(Ok(())) => debug!("Shutdown success"),
                Ok(Err(e)) => error!("Shutdown task error: {e:?}"),
                Err(join_error) => {
                    if join_error.is_cancelled() {
                        error!("Shutdown task was cancelled");
                    } else {
                        error!("Shutdown task join error: {join_error}")
                    }
                }
            }
        }
    }
}
