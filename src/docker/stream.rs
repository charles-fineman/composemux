#![allow(clippy::missing_docs_in_private_items)] // 17 left to document
//! Log streaming and container supervision.
//!
//! One task per container reads its log stream into a shared channel. A
//! supervisor task watches Docker events so that containers which restart (and
//! therefore get a *new* container ID) are reattached, and containers created
//! after startup are picked up.

use crate::docker::client::container_key;
use crate::docker::labels;
use crate::docker::DockerClient;
use anyhow::Result;
use bollard::query_parameters::{
    EventsOptionsBuilder, ListContainersOptionsBuilder, LogsOptionsBuilder,
};
use bollard::Docker;
use futures::StreamExt;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// How long to wait before re-subscribing after the event stream drops.
const EVENT_RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// Safety net: re-list periodically so a container is picked up even if its
/// event was missed entirely.
const RESYNC_INTERVAL: Duration = Duration::from_secs(5);

/// Largest slice of one log frame carried by a single [`SourceEvent::Output`].
///
/// The size of a frame is the daemon's decision, not ours, and bollard hands
/// one back already allocated -- that allocation is not something this code
/// can prevent. What it can prevent is the frame being multiplied: copied
/// whole into an event, queued in a 4096-slot channel, and copied again by a
/// consumer that bounds only what it *retains* (`MAX_PARTIAL` in the fallback
/// assembler, `MAX_RAW_BYTES` in `LogStore`). Splitting here is what makes
/// everything downstream of the frame finite, and lets the frame itself be
/// freed at the end of the iteration.
///
/// How big a frame gets depends on the service, and the reason is not the one
/// it looks like. The daemon's log copier splits a message at 16 KiB
/// *regardless of tty* -- reading the socket directly, a tty service and a
/// non-tty service writing the same 48 KiB both produce three 16384-byte
/// writes, at the same moments. What a tty removes is the stdcopy header on
/// each of those writes, so bollard cannot tell them apart and its decoder
/// re-joins them, cutting at newlines instead. Output the daemon had already
/// bounded and sent seconds earlier is held until a newline arrives:
///
/// ```text
///                 daemon sends                    bollard delivers
/// tty: false      16384 B at 4.65s 7.96s 11.34s   the same three, ~80ms later
/// tty: true       16384 B at 4.55s 7.87s 11.16s   nothing until 11.44s,
///                                                 then one 49154 B frame
/// ```
///
/// So without a tty a 5 MB newline-free line arrives as 306 frames, none over
/// 16384 bytes, and never reaches this bound. With one it arrives as a single
/// 5,010,002-byte frame, which is the case this bound exists for and the only
/// case where it fires. The re-joining is bollard's, not the daemon's, and is
/// a latency defect as much as a memory one; #38 tracks it, including the
/// upstream fix, since the daemon already announces the framing in a
/// `Content-Type` bollard never reads.
///
/// 64 KiB is four times the daemon's own 16 KiB message split, so the common
/// path keeps costing exactly one copy per frame, and it holds the most the
/// 4096-slot channel can carry to about 256 MiB, where before a single slot
/// was unbounded.
pub(crate) const MAX_CHUNK_BYTES: usize = 64 * 1024;

/// A message from the Docker layer to the UI.
#[derive(Debug)]
pub enum SourceEvent {
    /// Raw output from one service's container.
    Output {
        /// Compose service name, as the output is labelled with.
        service: String,
        /// The container's `com.docker.compose.container-number`, or 1 where
        /// compose omits it -- the default `resync` and `container_key` apply.
        replica: u32,
        /// ID of the container these bytes were read from.
        ///
        /// `(service, replica)` does not identify a container: compose
        /// recreates rather than restarts, and the replacement carries the
        /// same `container-number` label, so a consumer keyed on the pair
        /// alone cannot see that the container writing to it has changed.
        /// Carried per piece so a consumer holding bytes across writes can
        /// tell whose they are; the ID travels with the output rather than
        /// alongside it, so it cannot be raced by a separate notification.
        container: String,
        /// Which attach to that container the bytes were read from.
        ///
        /// The ID alone does not answer that. A container's log task can end
        /// while the container keeps running, and the next resync reattaches
        /// the same container from `since = ended_at` -- a new attach, no
        /// change of identity. `since` has one-second resolution, so the
        /// replay can restart an entry the consumer is holding an
        /// unterminated piece of, and a consumer that sees nothing change
        /// appends the replay to that piece: `GET /oneGET /one 200`, which is
        /// neither the line nor a duplicate of it. Stamped per attach so a
        /// consumer can end what it held, which is what keeps the cost of a
        /// reattach to the whole-line duplicate [`log_window`] accepts. The
        /// fallback compares it, and since #68 so does the TUI.
        attach: u64,
        /// One piece of a log frame, no larger than [`MAX_CHUNK_BYTES`].
        bytes: Vec<u8>,
    },
    /// Container topology or status changed; the UI should re-read services.
    Topology,
}

/// What a Docker event action means for us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventDecision {
    /// Container set may have changed: re-list and attach as needed.
    Resync,
    /// Only status changed: refresh the UI, don't touch attachments.
    NotifyOnly,
    Ignore,
}

/// Classifies a Docker container event.
///
/// `create` and `start` both bring a new container ID into play — compose
/// replaces containers rather than restarting them in place — so both resync.
pub fn event_decision(action: &str) -> EventDecision {
    match action {
        "create" | "start" | "destroy" | "rename" => EventDecision::Resync,
        "die" | "kill" | "stop" | "restart" | "pause" | "unpause" | "health_status" => {
            EventDecision::NotifyOnly
        }
        _ => EventDecision::Ignore,
    }
}

/// Which slice of a container's log history to request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogWindow<'a> {
    /// Unix seconds to resume from, when reattaching.
    pub since: Option<i32>,
    pub tail: &'a str,
}

/// Chooses the log window for an attach.
///
/// A fresh attach takes the configured tail. A reattach resumes from where the
/// previous stream stopped and asks for everything since, so recovering from a
/// dropped connection neither replays the tail nor leaves a hole.
///
/// `since` has one-second resolution, so a reattach can replay a line the dying
/// task had already delivered, showing it twice. That is deliberate: the only
/// alternative is to resume a second later and risk dropping output, and a
/// duplicated line is far cheaper than a missing one.
///
/// What the same resolution does to a line the consumer was still holding is
/// worse than a duplicate: the replay runs on from the middle of that line.
/// `SourceEvent::Output` carries the attach it was read from so a consumer can
/// end the held line when the reattach's first bytes arrive, which leaves it
/// with only the duplicate above. The fallback does; the TUI holds its partial
/// line as a row in an emulator and does not yet -- #68.
pub fn log_window(since: Option<i64>, tail: &str) -> LogWindow<'_> {
    match since {
        // The Engine API models this field as a 32-bit count of seconds, so
        // bollard's builder takes i32. Clamp instead of casting, so a skewed
        // clock can't wrap a future timestamp into the distant past.
        Some(t) => LogWindow {
            since: Some(t.clamp(0, i32::MAX as i64) as i32),
            tail: "all",
        },
        None => LogWindow { since: None, tail },
    }
}

/// Hands out an id per attach, so a consumer can tell one attach to a
/// container from the next attach to the same container.
///
/// Counted across the supervisor rather than per container: the requirement is
/// only that two attaches never share an id, and one counter cannot fall out
/// of step with itself. It starts at one, so zero stays available to a
/// consumer as "no attach seen yet".
#[derive(Debug, Default)]
struct AttachIds(u64);

impl AttachIds {
    /// The id for the next attach. Not named `next`, which would read as
    /// `Iterator`'s.
    fn next_id(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

/// A container we are (or were) streaming.
#[derive(Debug)]
struct Attachment {
    cancel: CancellationToken,
    /// True once the streaming task has exited, for any reason.
    finished: bool,
    /// Wall-clock seconds at which the stream ended, so a reattach can resume
    /// from there instead of replaying the whole tail.
    ended_at: Option<i64>,
}

/// One container as seen in a list response, reduced to what we act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerDesc {
    pub id: String,
    pub service: String,
    pub replica: u32,
    pub running: bool,
}

/// Decides which containers to attach to and which attachments to drop.
///
/// A container is (re)attached when it is running and has no live task: either
/// we have never attached it, or its task has since exited. A container that is
/// *not* running is attached only once, to pull its history — reattaching would
/// spin, because a finished container's log stream returns EOF immediately.
pub fn plan_attachments(
    attached: &HashMap<String, Attach>,
    seen: &[ContainerDesc],
) -> (Vec<ContainerDesc>, Vec<String>) {
    let to_attach = seen
        .iter()
        .filter(|c| match attached.get(&c.id) {
            None => true,
            Some(state) => c.running && state.finished,
        })
        .cloned()
        .collect();
    let to_drop = attached
        .keys()
        .filter(|id| !seen.iter().any(|c| &c.id == *id))
        .cloned()
        .collect();
    (to_attach, to_drop)
}

/// The subset of attachment state `plan_attachments` needs, so it can be tested
/// without constructing cancellation tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attach {
    pub finished: bool,
}

/// Owns the per-container log tasks and the event subscription.
pub struct LogSupervisor {
    docker: Docker,
    project: String,
    tail: usize,
    tx: mpsc::Sender<SourceEvent>,
    attached: HashMap<String, Attachment>,
    /// Ids stamped onto the output of the attaches this supervisor starts.
    attaches: AttachIds,
    /// Container IDs whose streaming task has exited, with the time it ended.
    done_tx: mpsc::UnboundedSender<(String, i64)>,
    done_rx: mpsc::UnboundedReceiver<(String, i64)>,
}

impl LogSupervisor {
    pub fn new(
        client: &DockerClient,
        project: impl Into<String>,
        tail: usize,
        tx: mpsc::Sender<SourceEvent>,
    ) -> Self {
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        Self {
            docker: client.raw().clone(),
            project: project.into(),
            tail,
            tx,
            attached: HashMap::new(),
            attaches: AttachIds::default(),
            done_tx,
            done_rx,
        }
    }

    /// Runs until `cancel` fires.
    pub async fn run(mut self, cancel: CancellationToken) {
        loop {
            if cancel.is_cancelled() {
                break;
            }
            match self.watch_events(&cancel).await {
                Ok(()) => break, // cancelled
                Err(err) => {
                    log_debug(&format!("event stream ended: {err}"));
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = tokio::time::sleep(EVENT_RECONNECT_DELAY) => {}
                    }
                }
            }
        }
        for (_, attachment) in self.attached.drain() {
            attachment.cancel.cancel();
        }
    }

    /// Attaches to every project container that needs it, and drops attachments
    /// for containers that no longer exist.
    async fn resync(&mut self) -> Result<()> {
        let mut filters = HashMap::new();
        filters.insert(
            "label".to_string(),
            vec![format!("{}={}", labels::PROJECT, self.project)],
        );
        let options = ListContainersOptionsBuilder::default()
            .all(true)
            .filters(&filters)
            .build();
        let containers = self.docker.list_containers(Some(options)).await?;

        let seen: Vec<ContainerDesc> = containers
            .iter()
            .filter_map(|c| {
                // The identity comes from `container_key` rather than being
                // read off the labels again here. What this attaches to is
                // what the fallback's reclaim compares its listing against, so
                // a second derivation is a second thing to keep in step.
                let (service, replica) = container_key(c)?;
                Some(ContainerDesc {
                    id: c.id.clone()?,
                    service,
                    replica,
                    running: matches!(
                        c.state,
                        Some(bollard::models::ContainerSummaryStateEnum::RUNNING)
                    ),
                })
            })
            .collect();

        let view: HashMap<String, Attach> = self
            .attached
            .iter()
            .map(|(id, a)| {
                (
                    id.clone(),
                    Attach {
                        finished: a.finished,
                    },
                )
            })
            .collect();
        let (to_attach, to_drop) = plan_attachments(&view, &seen);

        for id in to_drop {
            if let Some(attachment) = self.attached.remove(&id) {
                attachment.cancel.cancel();
            }
        }
        for desc in to_attach {
            let since = self.attached.get(&desc.id).and_then(|a| a.ended_at);
            self.attach(desc, since);
        }
        Ok(())
    }

    /// Starts streaming `desc`, returning the id its output is stamped with.
    ///
    /// The id is returned because nothing else can see it: everything else
    /// this does is inside a spawned task that needs a daemon. `resync` has no
    /// use for it.
    fn attach(&mut self, desc: ContainerDesc, since: Option<i64>) -> u64 {
        let cancel = CancellationToken::new();
        // A fresh id even when the container is one we have streamed before:
        // a reattach replays at `since`'s one-second resolution, so its first
        // bytes must not be read as a continuation of the last attach's.
        let attach = self.attaches.next_id();
        self.attached.insert(
            desc.id.clone(),
            Attachment {
                cancel: cancel.clone(),
                finished: false,
                ended_at: None,
            },
        );

        let docker = self.docker.clone();
        let tx = self.tx.clone();
        let done = self.done_tx.clone();
        // On a reattach, resume from where the previous stream stopped rather
        // than replaying the tail and duplicating output.
        let tail = self.tail.to_string();
        tokio::spawn(async move {
            let result = stream_container(&docker, &desc, &tail, since, attach, &tx, &cancel).await;
            if let Err(err) = result {
                log_debug(&format!("log stream for {} ended: {err}", desc.service));
            }
            let _ = done.send((desc.id, now_seconds()));
        });
        attach
    }

    /// Subscribes to container events for this project. Returns `Ok` only on
    /// cancellation; any stream error is returned so the caller can reconnect.
    async fn watch_events(&mut self, cancel: &CancellationToken) -> Result<()> {
        // Anchor the subscription before listing. The daemon replays events from
        // this instant, so a container starting between the list snapshot and
        // our first poll is still delivered rather than silently missed.
        let since = now_seconds();

        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
        filters.insert("type".to_string(), vec!["container".to_string()]);
        filters.insert(
            "label".to_string(),
            vec![format!("{}={}", labels::PROJECT, self.project)],
        );
        let options = EventsOptionsBuilder::default()
            .since(&since.to_string())
            .filters(&filters)
            .build();
        let mut stream = self.docker.events(Some(options));

        if let Err(err) = self.resync().await {
            log_debug(&format!("initial resync failed: {err}"));
        }
        let _ = self.tx.send(SourceEvent::Topology).await;

        let mut ticker = tokio::time::interval(RESYNC_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await; // the first tick resolves immediately

        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),

                // A streaming task exited. Clear its liveness so the next resync
                // can reattach if the container is still running.
                Some((id, ended_at)) = self.done_rx.recv() => {
                    if let Some(attachment) = self.attached.get_mut(&id) {
                        attachment.finished = true;
                        attachment.ended_at = Some(ended_at);
                    }
                    let _ = self.tx.send(SourceEvent::Topology).await;
                }

                _ = ticker.tick() => {
                    if let Err(err) = self.resync().await {
                        log_debug(&format!("periodic resync failed: {err}"));
                    }
                }

                next = stream.next() => {
                    let Some(message) = next else {
                        anyhow::bail!("event stream closed by the daemon");
                    };
                    let message = message?;
                    let Some(action) = message.action.as_deref() else { continue };
                    match event_decision(action) {
                        EventDecision::Resync => {
                            if let Err(err) = self.resync().await {
                                log_debug(&format!("resync on '{action}' failed: {err}"));
                            }
                            let _ = self.tx.send(SourceEvent::Topology).await;
                        }
                        EventDecision::NotifyOnly => {
                            let _ = self.tx.send(SourceEvent::Topology).await;
                        }
                        EventDecision::Ignore => {}
                    }
                }
            }
        }
    }
}

/// Reads one container's log stream until it ends or is cancelled.
async fn stream_container(
    docker: &Docker,
    desc: &ContainerDesc,
    tail: &str,
    since: Option<i64>,
    attach: u64,
    tx: &mpsc::Sender<SourceEvent>,
    cancel: &CancellationToken,
) -> Result<()> {
    let window = log_window(since, tail);
    let mut builder = LogsOptionsBuilder::default()
        .stdout(true)
        .stderr(true)
        .follow(true)
        .tail(window.tail);
    if let Some(since) = window.since {
        builder = builder.since(since);
    }
    let mut stream = docker.logs(&desc.id, Some(builder.build()));

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            next = stream.next() => {
                let Some(chunk) = next else { return Ok(()) };
                // stdout and stderr both land in the same emulator, exactly as
                // they would in a terminal attached to the container.
                if !forward_frame(tx, desc, attach, &chunk?.into_bytes(), cancel).await {
                    // Receiver gone, or this container was cancelled part way
                    // through a frame.
                    return Ok(());
                }
            }
        }
    }
}

/// Sends one frame downstream in pieces no larger than [`MAX_CHUNK_BYTES`],
/// returning `false` once the receiver has gone away or `cancel` has fired.
///
/// It watches `cancel` per piece rather than relying on the caller's select.
/// That select has already committed to this branch, so nothing polls the
/// token while a send waits on a full channel -- and splitting a frame turned
/// one such wait into one per piece, so a container removed during a burst
/// would otherwise sit here until the UI drained.
///
/// Cutting at a fixed offset is safe because both consumers are stateful
/// across writes: the fallback assembler holds a partial line until its
/// newline arrives, and `LogStore` carries a pending `\r` and its emulator's
/// parse state between writes. A cut mid-line, mid-CRLF, mid-escape or
/// mid-character therefore reads the same as no cut at all, so there is
/// nothing to gain by preferring to cut at a newline.
///
/// An empty frame yields no pieces, which is why there is no explicit guard
/// for one: forwarding it would replace a pane's "waiting" placeholder with a
/// blank pane.
async fn forward_frame(
    tx: &mpsc::Sender<SourceEvent>,
    desc: &ContainerDesc,
    attach: u64,
    frame: &[u8],
    cancel: &CancellationToken,
) -> bool {
    for piece in frame.chunks(MAX_CHUNK_BYTES) {
        let event = SourceEvent::Output {
            service: desc.service.clone(),
            replica: desc.replica,
            // One ID clone per piece, next to a service-name clone that was
            // already here, against a piece of up to `MAX_CHUNK_BYTES`.
            container: desc.id.clone(),
            attach,
            bytes: piece.to_vec(),
        };
        tokio::select! {
            // Biased so a token that is already cancelled wins deterministically
            // rather than depending on whether the channel happens to have room.
            biased;
            () = cancel.cancelled() => return false,
            sent = tx.send(event) => {
                if sent.is_err() {
                    return false;
                }
            }
        }
    }
    true
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Diagnostics go to a file rather than stderr: the alternate screen is active,
/// so printing would corrupt the display.
pub(crate) fn log_debug(message: &str) {
    if std::env::var_os("COMPOSEMUX_DEBUG").is_none() {
        return;
    }
    let path = std::env::temp_dir().join("composemux.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = writeln!(file, "{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The attach the forwarding tests stream under. Any id will do; a
    /// distinctive one makes a stamp that was defaulted rather than carried
    /// visible.
    const ATTACH: u64 = 7;

    /// One event, to fill a capacity-one channel so that the next send has
    /// nowhere to go.
    fn filler() -> SourceEvent {
        SourceEvent::Output {
            service: "filler".to_string(),
            replica: 1,
            container: "filler-id".to_string(),
            attach: 1,
            bytes: vec![b'x'],
        }
    }

    fn desc(id: &str, running: bool) -> ContainerDesc {
        ContainerDesc {
            id: id.to_string(),
            service: format!("svc-{id}"),
            replica: 1,
            running,
        }
    }

    /// A frame that fills a full channel must not pin the task once its
    /// container is gone.
    ///
    /// The outer select has already committed to the stream branch by the time
    /// a send blocks, so nothing there polls the token. Splitting frames made
    /// this worse rather than better: one wait per piece instead of one per
    /// frame.
    ///
    /// Paused so the clock is an assertion rather than a delay. It only
    /// advances while every task is idle, so a forward that completes without
    /// moving it completed because the cancel woke it, and not because
    /// something it was waiting on came round on a timer the outer bound
    /// outlives. Waiting on the bound alone cannot tell those apart: it passes
    /// against a `forward_frame` whose token arm is a plain `sleep`.
    #[tokio::test(start_paused = true)]
    async fn a_cancelled_container_stops_forwarding_into_a_full_channel() {
        // Capacity one, and already full, so the very first send blocks.
        let (tx, _rx) = mpsc::channel::<SourceEvent>(1);
        tx.send(filler()).await.unwrap();

        let cancel = CancellationToken::new();
        let container = desc("a", true);
        // Two pieces, so it cannot finish without a send completing.
        let frame = vec![b'x'; MAX_CHUNK_BYTES + 1];

        let forwarding = forward_frame(&tx, &container, ATTACH, &frame, &cancel);
        tokio::pin!(forwarding);

        // It is genuinely stuck: nothing drains the channel, so this bound has
        // to expire. Without it the test would pass against a forward that
        // simply completed its sends.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut forwarding)
                .await
                .is_err(),
            "the send should be waiting on a full channel"
        );

        let at_cancel = tokio::time::Instant::now();
        cancel.cancel();
        let finished = tokio::time::timeout(Duration::from_secs(5), forwarding).await;
        assert!(
            !finished.expect("cancelling should release the forward"),
            "a cancelled forward reports that it stopped early"
        );
        assert_eq!(
            tokio::time::Instant::now(),
            at_cancel,
            "time passed between the cancel and the forward returning, so \
             something other than the token released it"
        );
    }

    /// The `biased` in that select, which the test above cannot see: with the
    /// channel full the send is pending either way, so the token wins with or
    /// without the keyword. What it buys is that an already-cancelled token
    /// wins *even when the channel has room* -- the piece in hand is dropped
    /// rather than delivered to a consumer that is going away.
    ///
    /// Both arms are made ready for one poll: draining the filler frees the
    /// permit and wakes the parked send, and the cancel follows with no
    /// `await` between them, so nothing polls the select until both are ready.
    /// An unbiased select then picks at random, which is why this runs rounds
    /// -- correct code passes every one, and deleting the keyword fails about
    /// half of them.
    #[tokio::test(start_paused = true)]
    async fn an_already_cancelled_forward_drops_the_piece_rather_than_sending_it() {
        /// Enough that an unbiased select failing half the time is caught with
        /// certainty for any practical purpose, and cheap because the waits
        /// are virtual.
        const ROUNDS: usize = 20;

        for round in 0..ROUNDS {
            let (tx, mut rx) = mpsc::channel::<SourceEvent>(1);
            tx.send(filler()).await.unwrap();

            let cancel = CancellationToken::new();
            let container = desc("a", true);
            let frame = vec![b'x'; MAX_CHUNK_BYTES + 1];
            let forwarding = forward_frame(&tx, &container, ATTACH, &frame, &cancel);
            tokio::pin!(forwarding);

            // Park the send on the full channel, so the permit it is waiting
            // for is the only thing between it and a completed send.
            assert!(
                tokio::time::timeout(Duration::from_millis(50), &mut forwarding)
                    .await
                    .is_err(),
                "round {round}: the send should be waiting on a full channel"
            );

            rx.try_recv()
                .expect("the filler is what filled the channel");
            cancel.cancel();

            let finished = tokio::time::timeout(Duration::from_secs(5), forwarding).await;
            assert!(
                !finished.expect("cancelling should release the forward"),
                "round {round}: a cancelled forward reports that it stopped early"
            );
            assert!(
                rx.try_recv().is_err(),
                "round {round}: a piece was delivered after the cancel"
            );
        }
    }

    /// The allocation itself, at the one place that does it. A reattach that
    /// was handed the id it is replacing is invisible to every consumer, which
    /// is #57 restored in full, and no test downstream of here can see that:
    /// they are all given their ids rather than allocating them.
    ///
    /// No daemon is contacted. The handle points at a closed loopback port
    /// rather than at the local socket, so the task `attach` spawns fails its
    /// first request instead of reaching a daemon that may or may not be
    /// running; what is asserted happens before the spawn either way.
    #[tokio::test]
    async fn the_supervisor_gives_every_attach_a_fresh_id() {
        let (tx, _rx) = mpsc::channel(8);
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        let mut supervisor = LogSupervisor {
            docker: Docker::connect_with_http("127.0.0.1:1", 1, bollard::API_DEFAULT_VERSION)
                .expect("building a handle makes no request"),
            project: "p".to_string(),
            tail: 10,
            tx,
            attached: HashMap::new(),
            attaches: AttachIds::default(),
            done_tx,
            done_rx,
        };

        // The same container twice: a fresh attach, then the reattach a
        // resync makes when the first one's task has ended.
        let first = supervisor.attach(desc("a", true), None);
        let second = supervisor.attach(desc("a", true), Some(1));

        assert_ne!(
            first, second,
            "a reattach was stamped with the id of the attach it replaces"
        );
    }

    /// Ids are what tell one attach from the next, so they must not repeat --
    /// a reattach handed the previous id is invisible to a consumer, which is
    /// the whole of #57. Zero is never handed out, so a consumer that starts
    /// at its default cannot read the first attach as one it has seen.
    #[test]
    fn attach_ids_are_distinct_and_never_zero() {
        let mut ids = AttachIds::default();
        let mut seen = Vec::new();
        for _ in 0..4 {
            let id = ids.next_id();
            assert_ne!(id, 0, "zero is the consumers' 'no attach seen yet'");
            assert!(!seen.contains(&id), "attach id {id} was handed out twice");
            seen.push(id);
        }
    }

    fn attached(entries: &[(&str, bool)]) -> HashMap<String, Attach> {
        entries
            .iter()
            .map(|(id, finished)| {
                (
                    id.to_string(),
                    Attach {
                        finished: *finished,
                    },
                )
            })
            .collect()
    }

    /// Drains everything queued, checking each event still carries the identity
    /// of the container and the attach it came from.
    fn drain(rx: &mut mpsc::Receiver<SourceEvent>, attach: u64) -> Vec<Vec<u8>> {
        let mut pieces = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                SourceEvent::Output {
                    service,
                    replica,
                    container,
                    attach: from,
                    bytes,
                } => {
                    assert_eq!(service, "svc-a", "a piece lost its service");
                    assert_eq!(replica, 1, "a piece lost its replica");
                    // The container ID, not the service name or the label:
                    // it is the one part of the identity that a compose
                    // recreate changes, which is the whole reason the
                    // fallback is given it.
                    assert_eq!(container, "a", "a piece lost its container ID");
                    // The container ID cannot separate two attaches to one
                    // container; this is what the consumer ends a held line
                    // on, so losing it downgrades #57's mangled line to
                    // nothing being noticed at all.
                    assert_eq!(from, attach, "a piece lost its attach id");
                    pieces.push(bytes);
                }
                other => panic!("expected output, got {other:?}"),
            }
        }
        pieces
    }

    /// The bound the daemon does not give us. Nothing upstream limits how much
    /// one frame contains, and one event used to carry all of it: into a
    /// 4096-slot channel, then into a consumer that bounds only what it keeps.
    #[tokio::test]
    async fn an_oversized_frame_is_forwarded_in_bounded_pieces() {
        let (tx, mut rx) = mpsc::channel(64);
        // Non-uniform, so reassembly is checked byte for byte rather than by
        // length alone; the remainder makes the last piece a short one.
        let size = 4 * MAX_CHUNK_BYTES + 7;
        let frame: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();

        assert!(
            forward_frame(
                &tx,
                &desc("a", true),
                ATTACH,
                &frame,
                &CancellationToken::new()
            )
            .await
        );

        let pieces = drain(&mut rx, ATTACH);
        for piece in &pieces {
            assert!(
                piece.len() <= MAX_CHUNK_BYTES,
                "forwarded {} bytes in one event, past the {MAX_CHUNK_BYTES}-byte bound",
                piece.len()
            );
        }
        assert_eq!(pieces.len(), 5, "expected the frame to be cut into pieces");
        let back: Vec<u8> = pieces.concat();
        assert!(back == frame, "the pieces do not reassemble to the frame");
    }

    /// Real Docker output arrives well under the bound, so the common path
    /// must be exactly what it was: one frame, one event, one copy.
    #[tokio::test]
    async fn a_frame_at_the_bound_is_forwarded_in_one_piece() {
        let (tx, mut rx) = mpsc::channel(8);
        let frame = vec![b'x'; MAX_CHUNK_BYTES];

        assert!(
            forward_frame(
                &tx,
                &desc("a", true),
                ATTACH,
                &frame,
                &CancellationToken::new()
            )
            .await
        );

        let pieces = drain(&mut rx, ATTACH);
        assert_eq!(pieces.len(), 1, "a frame at the bound should not be split");
        assert_eq!(pieces[0].len(), MAX_CHUNK_BYTES);
    }

    /// The other side of the boundary, where an off-by-one would live.
    #[tokio::test]
    async fn one_byte_past_the_bound_is_split() {
        let (tx, mut rx) = mpsc::channel(8);
        let frame = vec![b'x'; MAX_CHUNK_BYTES + 1];

        assert!(
            forward_frame(
                &tx,
                &desc("a", true),
                ATTACH,
                &frame,
                &CancellationToken::new()
            )
            .await
        );

        let pieces = drain(&mut rx, ATTACH);
        assert_eq!(pieces.len(), 2);
        assert_eq!(pieces[0].len(), MAX_CHUNK_BYTES);
        assert_eq!(pieces[1].len(), 1);
    }

    /// An empty frame must stay unsent: `LogStore` treats any write as output,
    /// so forwarding one would replace a pane's "waiting" placeholder with a
    /// blank pane.
    #[tokio::test]
    async fn an_empty_frame_is_not_forwarded() {
        let (tx, mut rx) = mpsc::channel(8);

        assert!(
            forward_frame(
                &tx,
                &desc("a", true),
                ATTACH,
                b"",
                &CancellationToken::new()
            )
            .await
        );

        assert!(
            drain(&mut rx, ATTACH).is_empty(),
            "an empty frame was forwarded"
        );
    }

    /// A departed receiver has to be reported, so the reading loop stops
    /// rather than draining the rest of the container's log into a channel
    /// nobody is holding.
    #[tokio::test]
    async fn forwarding_reports_a_departed_receiver() {
        let (tx, rx) = mpsc::channel::<SourceEvent>(8);
        drop(rx);

        assert!(
            !forward_frame(
                &tx,
                &desc("a", true),
                ATTACH,
                b"anything",
                &CancellationToken::new()
            )
            .await,
            "the caller must be told to stop reading"
        );
    }

    /// Every piece carries the attach it was read from. Two attaches to one
    /// container are the pair the container ID cannot separate, and the stamp
    /// has to travel with the bytes rather than alongside them, for the reason
    /// the container ID does: a notification sent separately races the output
    /// it describes. What allocates the two ids is
    /// [`the_supervisor_gives_every_attach_a_fresh_id`]; this is the wire.
    ///
    /// Both frames here fit in one piece. That a *split* frame stamps every
    /// piece is `drain`'s business, which the three chunking tests run.
    #[tokio::test]
    async fn forward_frame_stamps_the_attach_it_was_given() {
        let (tx, mut rx) = mpsc::channel(8);
        let container = desc("a", true);
        let mut ids = AttachIds::default();
        let cancel = CancellationToken::new();

        // The dying attach delivers a line it never terminates, and the
        // reattach replays that entry from the start -- `since` resolves to
        // the second, so it can only resume at or before where the line began.
        let first = ids.next_id();
        assert!(forward_frame(&tx, &container, first, b"GET /one", &cancel).await);
        let second = ids.next_id();
        assert!(forward_frame(&tx, &container, second, b"GET /one 200\n", &cancel).await);

        let mut stamps = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                SourceEvent::Output {
                    container, attach, ..
                } => {
                    assert_eq!(container, "a", "both attaches are to one container");
                    stamps.push(attach);
                }
                other => panic!("expected output, got {other:?}"),
            }
        }
        assert_eq!(
            stamps,
            vec![first, second],
            "the reattach's replay is indistinguishable from the line it splices onto"
        );
    }

    #[test]
    fn container_lifecycle_events_trigger_a_resync() {
        for action in ["create", "start", "destroy", "rename"] {
            assert_eq!(event_decision(action), EventDecision::Resync, "{action}");
        }
    }

    #[test]
    fn status_only_events_refresh_without_reattaching() {
        for action in [
            "die",
            "kill",
            "stop",
            "restart",
            "pause",
            "unpause",
            "health_status",
        ] {
            assert_eq!(
                event_decision(action),
                EventDecision::NotifyOnly,
                "{action}"
            );
        }
    }

    #[test]
    fn unrelated_events_are_ignored() {
        for action in ["exec_create", "attach", "top", "resize", ""] {
            assert_eq!(event_decision(action), EventDecision::Ignore, "{action}");
        }
    }

    #[test]
    fn a_fresh_attach_uses_the_configured_tail() {
        let window = log_window(None, "200");
        assert_eq!(window.since, None);
        assert_eq!(window.tail, "200");
    }

    #[test]
    fn a_reattach_resumes_from_where_the_stream_stopped() {
        // Replaying the tail here would duplicate output the pane already shows.
        let window = log_window(Some(1_700_000_000), "200");
        assert_eq!(window.since, Some(1_700_000_000));
        assert_eq!(window.tail, "all", "everything since the cut, not a tail");
    }

    #[test]
    fn a_resume_timestamp_is_clamped_rather_than_wrapped() {
        // The API takes i32 seconds; a bad clock must not wrap into the past.
        assert_eq!(log_window(Some(i64::MAX), "200").since, Some(i32::MAX));
        assert_eq!(log_window(Some(-5), "200").since, Some(0));
    }

    #[test]
    fn a_new_container_is_attached() {
        let (attach, drop) = plan_attachments(&HashMap::new(), &[desc("a", true)]);
        assert_eq!(attach.len(), 1);
        assert!(drop.is_empty());
    }

    #[test]
    fn a_live_attachment_is_left_alone() {
        let (attach, drop) = plan_attachments(&attached(&[("a", false)]), &[desc("a", true)]);
        assert!(attach.is_empty(), "must not attach twice");
        assert!(drop.is_empty());
    }

    #[test]
    fn a_running_container_whose_task_died_is_reattached() {
        // The blocking bug: a stream that errors leaves the container running
        // with no reader, and it must be picked back up.
        let (attach, _) = plan_attachments(&attached(&[("a", true)]), &[desc("a", true)]);
        assert_eq!(attach.len(), 1, "a dead task must be replaced");
        assert_eq!(attach[0].id, "a");
    }

    #[test]
    fn a_stopped_container_is_not_reattached_after_its_stream_ends() {
        // Reattaching here would spin: a finished container's follow stream
        // returns its history and closes immediately.
        let (attach, _) = plan_attachments(&attached(&[("a", true)]), &[desc("a", false)]);
        assert!(attach.is_empty());
    }

    #[test]
    fn a_stopped_container_is_still_attached_once_for_its_history() {
        let (attach, _) = plan_attachments(&HashMap::new(), &[desc("a", false)]);
        assert_eq!(attach.len(), 1);
    }

    #[test]
    fn a_removed_container_is_dropped() {
        let (attach, drop) = plan_attachments(&attached(&[("a", false)]), &[]);
        assert!(attach.is_empty());
        assert_eq!(drop, vec!["a".to_string()]);
    }

    #[test]
    fn a_replaced_container_swaps_ids() {
        // Compose recreates rather than restarting, so the old ID vanishes and
        // a new one appears in the same pass.
        let (attach, drop) = plan_attachments(&attached(&[("old", false)]), &[desc("new", true)]);
        assert_eq!(attach.len(), 1);
        assert_eq!(attach[0].id, "new");
        assert_eq!(drop, vec!["old".to_string()]);
    }
}
