//! Desktop notifications: which events notify, how bursts are grouped, and the popup
//! text. Delivery to the OS lives behind the [`Notifier`] trait.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use puppy_core::types::DownloadId;
use puppy_proto::Event;
use tokio::sync::broadcast::{self, error::RecvError, error::TryRecvError};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::config::NotificationsConfig;

/// Every popup's title.
pub const TITLE: &str = "puppy";
/// A kind's popup is sent once this long passes with no new event of that kind.
pub const QUIET: Duration = Duration::from_secs(3);
/// ... or at the latest this long after the first event of the burst.
pub const MAX_HOLD: Duration = Duration::from_secs(10);
/// Failure reasons are cut to this many characters.
pub const MAX_REASON: usize = 160;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotifyKind {
    Completed,
    Failed,
    ChooseFiles,
    SeedingFinished,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: DownloadId,
    pub name: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub kind: NotifyKind,
    pub title: String,
    pub body: String,
}

/// Decides whether `ev` notifies. The returned item's name may be empty (the caller
/// fills it in).
pub fn classify(ev: &Event, cfg: &NotificationsConfig) -> Option<(NotifyKind, Item)> {
    if !cfg.enabled {
        return None;
    }
    match ev {
        Event::Completed { id, name } if cfg.completed => Some((
            NotifyKind::Completed,
            Item {
                id: *id,
                name: name.clone(),
                detail: None,
            },
        )),
        Event::Failed {
            id,
            reason,
            transient: false,
        } if cfg.failed => Some((
            NotifyKind::Failed,
            Item {
                id: *id,
                name: String::new(),
                detail: Some(reason.clone()),
            },
        )),
        Event::ChoosingFiles { id, name } if cfg.choose_files => Some((
            NotifyKind::ChooseFiles,
            Item {
                id: *id,
                name: name.clone(),
                detail: None,
            },
        )),
        Event::SeedingFinished { id } if cfg.seeding_finished => Some((
            NotifyKind::SeedingFinished,
            Item {
                id: *id,
                name: String::new(),
                detail: None,
            },
        )),
        _ => None,
    }
}

fn short_reason(s: &str) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.chars().count() > MAX_REASON {
        let mut out: String = line.chars().take(MAX_REASON).collect();
        out.push('…');
        out
    } else {
        line.to_string()
    }
}

/// The popup for a group of `items` of one kind (`items` must not be empty).
pub fn render(kind: NotifyKind, items: &[Item]) -> Notification {
    let body = if let [it] = items {
        match kind {
            NotifyKind::Completed => format!("Finished: {}", it.name),
            NotifyKind::Failed => match it.detail.as_deref().map(short_reason) {
                Some(r) if !r.is_empty() => format!("Failed: {} — {}", it.name, r),
                _ => format!("Failed: {}", it.name),
            },
            NotifyKind::ChooseFiles => format!(
                "Choose files: {} — run puppy files {} or press c in the dashboard",
                it.name, it.id
            ),
            NotifyKind::SeedingFinished => format!("Seeding finished: {}", it.name),
        }
    } else {
        let n = items.len();
        match kind {
            NotifyKind::Completed => format!("{n} downloads finished"),
            NotifyKind::Failed => format!("{n} downloads failed"),
            NotifyKind::ChooseFiles => {
                format!("{n} torrents waiting for files — run puppy ls or press c in the dashboard")
            }
            NotifyKind::SeedingFinished => format!("{n} torrents finished seeding"),
        }
    };
    Notification {
        kind,
        title: TITLE.to_string(),
        body,
    }
}

struct Group {
    kind: NotifyKind,
    items: Vec<Item>,
    first: Instant,
    last: Instant,
}

impl Group {
    fn deadline(&self) -> Instant {
        (self.last + QUIET).min(self.first + MAX_HOLD)
    }
}

/// Collects events per kind and releases one popup per burst.
#[derive(Default)]
pub struct Batcher {
    groups: Vec<Group>,
}

impl Batcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, kind: NotifyKind, item: Item, now: Instant) {
        match self.groups.iter_mut().find(|g| g.kind == kind) {
            Some(g) => {
                if g.items.iter().any(|i| i.id == item.id) {
                    return;
                }
                g.items.push(item);
                g.last = now;
            }
            None => self.groups.push(Group {
                kind,
                items: vec![item],
                first: now,
                last: now,
            }),
        }
    }

    /// Popups whose burst has ended by `now`, in order of first event.
    pub fn due(&mut self, now: Instant) -> Vec<Notification> {
        let mut out = Vec::new();
        self.groups.retain(|g| {
            if g.deadline() <= now {
                out.push(render(g.kind, &g.items));
                false
            } else {
                true
            }
        });
        out
    }

    /// Everything pending, regardless of deadlines (shutdown).
    pub fn drain_all(&mut self) -> Vec<Notification> {
        self.groups
            .drain(..)
            .map(|g| render(g.kind, &g.items))
            .collect()
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.groups.iter().map(Group::deadline).min()
    }
}

pub trait Notifier: Send + Sync + 'static {
    /// Must return at once.
    fn notify(&self, n: &Notification);
    /// Waits at most `within` for queued popups to be handed to the system (blocking;
    /// called from spawn_blocking).
    fn flush(&self, _within: Duration) {}
}

/// Test sink that records every popup.
#[doc(hidden)]
#[derive(Default, Clone)]
pub struct RecordingNotifier(Arc<Mutex<Vec<Notification>>>);

impl RecordingNotifier {
    /// Takes everything recorded so far.
    pub fn take(&self) -> Vec<Notification> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl Notifier for RecordingNotifier {
    fn notify(&self, n: &Notification) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(n.clone());
    }
}

/// The running notifier task (see [`spawn`]).
pub struct NotifyTask {
    stop: CancellationToken,
    handle: JoinHandle<()>,
    sink: Arc<dyn Notifier>,
}

/// Subscribes to the daemon's events and delivers popups to `sink`: `lookup` gives the
/// name of a download whose event carries none (called from the task, so it must be
/// quick). One task; sending never blocks the manager.
pub fn spawn(
    rx: broadcast::Receiver<Event>,
    lookup: impl Fn(DownloadId) -> Option<String> + Send + 'static,
    cfg: NotificationsConfig,
    sink: Arc<dyn Notifier>,
) -> NotifyTask {
    let stop = CancellationToken::new();
    let handle = tokio::spawn(run(rx, lookup, cfg, sink.clone(), stop.clone()));
    NotifyTask { stop, handle, sink }
}

impl NotifyTask {
    /// Stops the task, sending whatever is still pending, then waits (at most `within`
    /// in all) for the sink to hand the popups to the system.
    pub async fn finish(self, within: Duration) {
        self.stop.cancel();
        let work = async {
            let _ = self.handle.await;
            let sink = self.sink;
            let _ = tokio::task::spawn_blocking(move || sink.flush(within)).await;
        };
        if tokio::time::timeout(within, work).await.is_err() {
            debug!("notifications were not delivered in time");
        }
    }
}

fn enqueue(
    ev: &Event,
    cfg: &NotificationsConfig,
    lookup: &impl Fn(DownloadId) -> Option<String>,
    batcher: &mut Batcher,
) {
    if let Some((kind, mut item)) = classify(ev, cfg) {
        if item.name.is_empty() {
            item.name = lookup(item.id).unwrap_or_else(|| format!("download {}", item.id));
        }
        batcher.push(kind, item, Instant::now());
    }
}

async fn run(
    mut rx: broadcast::Receiver<Event>,
    lookup: impl Fn(DownloadId) -> Option<String>,
    cfg: NotificationsConfig,
    sink: Arc<dyn Notifier>,
    stop: CancellationToken,
) {
    let mut batcher = Batcher::new();
    loop {
        let deadline = batcher.next_deadline();
        tokio::select! {
            _ = stop.cancelled() => break,
            r = rx.recv() => match r {
                Ok(ev) => enqueue(&ev, &cfg, &lookup, &mut batcher),
                Err(RecvError::Lagged(n)) => debug!(missed = n, "notifier lagged behind events"),
                Err(RecvError::Closed) => break,
            },
            _ = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending().await,
                }
            } => {
                for n in batcher.due(Instant::now()) {
                    sink.notify(&n);
                }
            }
        }
    }
    // Events already sent but not yet read count too.
    loop {
        match rx.try_recv() {
            Ok(ev) => enqueue(&ev, &cfg, &lookup, &mut batcher),
            Err(TryRecvError::Lagged(_)) => {}
            Err(_) => break,
        }
    }
    for n in batcher.drain_all() {
        sink.notify(&n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: DownloadId, name: &str) -> Item {
        Item {
            id,
            name: name.into(),
            detail: None,
        }
    }

    fn failed(id: DownloadId, name: &str, why: &str) -> Item {
        Item {
            id,
            name: name.into(),
            detail: Some(why.into()),
        }
    }

    fn body(kind: NotifyKind, items: &[Item]) -> String {
        render(kind, items).body
    }

    #[test]
    fn single_item_bodies() {
        assert_eq!(
            render(NotifyKind::Completed, &[item(1, "ubuntu-24.04.iso")]),
            Notification {
                kind: NotifyKind::Completed,
                title: "puppy".into(),
                body: "Finished: ubuntu-24.04.iso".into()
            }
        );
        assert_eq!(
            body(
                NotifyKind::Failed,
                &[failed(2, "big.tar", "connection reset")]
            ),
            "Failed: big.tar — connection reset"
        );
        assert_eq!(
            body(NotifyKind::ChooseFiles, &[item(4, "Big.Show.S01")]),
            "Choose files: Big.Show.S01 — run puppy files 4 or press c in the dashboard"
        );
        assert_eq!(
            body(NotifyKind::SeedingFinished, &[item(1, "x.iso")]),
            "Seeding finished: x.iso"
        );
    }

    #[test]
    fn group_bodies() {
        let three = [item(1, "a"), item(2, "b"), item(3, "c")];
        let two = [item(1, "a"), item(2, "b")];
        assert_eq!(body(NotifyKind::Completed, &three), "3 downloads finished");
        assert_eq!(body(NotifyKind::Failed, &two), "2 downloads failed");
        assert_eq!(
            body(NotifyKind::ChooseFiles, &two),
            "2 torrents waiting for files — run puppy ls or press c in the dashboard"
        );
        assert_eq!(
            body(NotifyKind::SeedingFinished, &two),
            "2 torrents finished seeding"
        );
    }

    #[test]
    fn long_reason_is_shortened() {
        let long = format!("{}\nsecond line", "x".repeat(500));
        let b = body(NotifyKind::Failed, &[failed(1, "a", &long)]);
        assert_eq!(b, format!("Failed: a — {}…", "x".repeat(160)));
        assert!(!b.contains("second line"));
        assert_eq!(
            body(NotifyKind::Failed, &[failed(1, "a", "short\nmore")]),
            "Failed: a — short"
        );
    }

    #[test]
    fn burst_within_quiet_window_is_one_popup() {
        let t0 = Instant::now();
        let mut b = Batcher::new();
        for (i, s) in [0u64, 1, 2].into_iter().enumerate() {
            b.push(
                NotifyKind::Completed,
                item(i as u64 + 1, "f"),
                t0 + Duration::from_secs(s),
            );
        }
        assert!(b.due(t0 + Duration::from_millis(4900)).is_empty());
        assert_eq!(b.next_deadline(), Some(t0 + Duration::from_secs(5)));
        let got = b.due(t0 + Duration::from_secs(5));
        assert_eq!(
            got,
            vec![render(
                NotifyKind::Completed,
                &[item(1, "f"), item(2, "f"), item(3, "f")]
            )]
        );
        assert_eq!(got[0].body, "3 downloads finished");
        assert_eq!(b.next_deadline(), None);
    }

    #[test]
    fn kinds_group_separately() {
        let t0 = Instant::now();
        let mut b = Batcher::new();
        b.push(NotifyKind::Completed, item(1, "d1"), t0);
        b.push(NotifyKind::Completed, item(2, "d2"), t0);
        b.push(
            NotifyKind::Failed,
            failed(4, "d4", "boom"),
            t0 + Duration::from_millis(500),
        );
        b.push(
            NotifyKind::Completed,
            item(3, "d3"),
            t0 + Duration::from_secs(1),
        );
        let got = b.due(t0 + Duration::from_secs(10));
        let bodies: Vec<_> = got.iter().map(|n| n.body.as_str()).collect();
        assert_eq!(bodies, ["3 downloads finished", "Failed: d4 — boom"]);
    }

    #[test]
    fn trickle_is_capped() {
        let t0 = Instant::now();
        let mut b = Batcher::new();
        let mut popups = Vec::new();
        for s in 0..30u64 {
            let now = t0 + Duration::from_secs(s);
            b.push(NotifyKind::Completed, item(s, &format!("f{s}")), now);
            popups.extend(b.due(now));
        }
        popups.extend(b.due(t0 + Duration::from_secs(40)));
        assert!(popups.len() <= 3, "{popups:?}");
        let total: usize = popups
            .iter()
            .map(|n| {
                n.body
                    .split_once(" downloads finished")
                    .map_or(1, |(c, _)| c.parse().unwrap())
            })
            .sum();
        assert_eq!(total, 30);
    }

    #[test]
    fn same_download_twice_counts_once() {
        let t0 = Instant::now();
        let mut b = Batcher::new();
        b.push(NotifyKind::Completed, item(7, "a"), t0);
        b.push(NotifyKind::Completed, item(7, "a"), t0);
        let got = b.due(t0 + Duration::from_secs(3));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "Finished: a");
    }

    #[test]
    fn drain_all_flushes_everything_now() {
        let t0 = Instant::now();
        let mut b = Batcher::new();
        b.push(NotifyKind::Completed, item(1, "a"), t0);
        b.push(NotifyKind::Failed, failed(2, "b", "x"), t0);
        assert_eq!(b.drain_all().len(), 2);
        assert_eq!(b.next_deadline(), None);
    }

    #[test]
    fn classify_respects_config() {
        let cfg = NotificationsConfig::default();
        let done = Event::Completed {
            id: 1,
            name: "a".into(),
        };
        assert_eq!(
            classify(&done, &cfg),
            Some((NotifyKind::Completed, item(1, "a")))
        );
        let fail = |transient| Event::Failed {
            id: 3,
            reason: "boom".into(),
            transient,
        };
        let (k, it) = classify(&fail(false), &cfg).unwrap();
        assert_eq!(k, NotifyKind::Failed);
        assert_eq!(it.detail.as_deref(), Some("boom"));
        assert_eq!(classify(&fail(true), &cfg), None);
        let choose = Event::ChoosingFiles {
            id: 4,
            name: "b".into(),
        };
        assert_eq!(
            classify(&choose, &cfg),
            Some((NotifyKind::ChooseFiles, item(4, "b")))
        );
        let seeded = Event::SeedingFinished { id: 2 };
        assert_eq!(classify(&seeded, &cfg), None);
        let on = NotificationsConfig {
            seeding_finished: true,
            ..cfg.clone()
        };
        assert_eq!(
            classify(&seeded, &on),
            Some((NotifyKind::SeedingFinished, item(2, "")))
        );
        let off = NotificationsConfig {
            completed: false,
            ..cfg.clone()
        };
        assert_eq!(classify(&done, &off), None);
        for ev in [
            Event::Added { id: 1 },
            Event::Removed { id: 1 },
            Event::NeedsAttention {
                id: 1,
                reason: puppy_proto::WireAttention::RemoteChanged,
            },
            Event::Unknown,
        ] {
            assert_eq!(classify(&ev, &cfg), None);
        }
        let disabled = NotificationsConfig {
            enabled: false,
            ..on
        };
        for ev in [done, fail(false), choose, seeded] {
            assert_eq!(classify(&ev, &disabled), None);
        }
    }

    fn bodies(rec: &RecordingNotifier) -> Vec<String> {
        rec.take().into_iter().map(|n| n.body).collect()
    }

    /// Lets the notifier task run.
    async fn settle() {
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
    }

    /// Lets the task take the pending events, then moves the clock.
    async fn advance(d: Duration) {
        settle().await;
        tokio::time::advance(d).await;
        settle().await;
    }

    #[tokio::test(start_paused = true)]
    async fn task_groups_and_names() {
        let (tx, rx) = broadcast::channel(16);
        let rec = RecordingNotifier::default();
        let lookup = |id| (id == 2).then(|| "b".to_string());
        let _task = spawn(
            rx,
            lookup,
            NotificationsConfig::default(),
            Arc::new(rec.clone()),
        );
        tx.send(Event::Completed {
            id: 1,
            name: "a".into(),
        })
        .unwrap();
        tx.send(Event::Completed {
            id: 2,
            name: String::new(),
        })
        .unwrap();
        tx.send(Event::Failed {
            id: 7,
            reason: "boom".into(),
            transient: false,
        })
        .unwrap();
        advance(Duration::from_secs(2)).await;
        assert!(rec.take().is_empty());
        advance(Duration::from_secs(2)).await;
        assert_eq!(
            bodies(&rec),
            ["2 downloads finished", "Failed: download 7 — boom"]
        );
    }

    /// A failure carries no name of its own: the lookup supplies the real one.
    #[tokio::test(start_paused = true)]
    async fn failed_popup_carries_the_looked_up_name() {
        let (tx, rx) = broadcast::channel(16);
        let rec = RecordingNotifier::default();
        let lookup = |id| (id == 7).then(|| "big.tar".to_string());
        let _task = spawn(
            rx,
            lookup,
            NotificationsConfig::default(),
            Arc::new(rec.clone()),
        );
        tx.send(Event::Failed {
            id: 7,
            reason: "boom".into(),
            transient: false,
        })
        .unwrap();
        advance(Duration::from_secs(4)).await;
        assert_eq!(bodies(&rec), ["Failed: big.tar — boom"]);
    }

    #[tokio::test(start_paused = true)]
    async fn task_skips_transient_and_disabled() {
        let (tx, rx) = broadcast::channel(16);
        let rec = RecordingNotifier::default();
        let _task = spawn(
            rx,
            |_| None,
            NotificationsConfig::default(),
            Arc::new(rec.clone()),
        );
        tx.send(Event::Failed {
            id: 1,
            reason: "x".into(),
            transient: true,
        })
        .unwrap();
        tx.send(Event::Added { id: 2 }).unwrap();
        advance(Duration::from_secs(15)).await;
        assert!(rec.take().is_empty());

        let (tx, rx) = broadcast::channel(16);
        let cfg = NotificationsConfig {
            enabled: false,
            ..NotificationsConfig::default()
        };
        let _task = spawn(rx, |_| None, cfg, Arc::new(rec.clone()));
        tx.send(Event::Completed {
            id: 3,
            name: "c".into(),
        })
        .unwrap();
        advance(Duration::from_secs(15)).await;
        assert!(rec.take().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn finish_sends_what_is_pending() {
        let (tx, rx) = broadcast::channel(16);
        let rec = RecordingNotifier::default();
        let task = spawn(
            rx,
            |_| None,
            NotificationsConfig::default(),
            Arc::new(rec.clone()),
        );
        tx.send(Event::Completed {
            id: 1,
            name: "a".into(),
        })
        .unwrap();
        task.finish(Duration::from_secs(1)).await;
        assert_eq!(bodies(&rec), ["Finished: a"]);
    }
}
