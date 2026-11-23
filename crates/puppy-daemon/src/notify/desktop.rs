//! The real desktop sink: a bounded queue drained by one worker thread, so a slow or
//! missing notification server can never block the manager.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::debug;

use super::{Notification, Notifier};

/// Popups waiting for the worker; more are dropped.
const QUEUE: usize = 16;

pub struct DesktopNotifier {
    tx: Mutex<Option<SyncSender<Notification>>>,
    done: Mutex<Option<Receiver<()>>>,
    /// Set once the first backend failure has been logged (read by tests).
    #[allow(dead_code)]
    logged: Arc<AtomicBool>,
}

impl DesktopNotifier {
    pub fn new() -> Self {
        Self::with_backend(|n| {
            notify_rust::Notification::new()
                .appname("puppy")
                .summary(&n.title)
                .body(&n.body)
                .show()
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }

    pub(crate) fn with_backend(
        send: impl Fn(&Notification) -> Result<(), String> + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Notification>(QUEUE);
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let logged = Arc::new(AtomicBool::new(false));
        let worker_logged = logged.clone();
        let spawned = std::thread::Builder::new()
            .name("puppy-notify".into())
            .spawn(move || {
                while let Ok(n) = rx.recv() {
                    if let Err(e) = send(&n)
                        && !worker_logged.swap(true, Ordering::Relaxed)
                    {
                        debug!("desktop notifications unavailable: {e}");
                    }
                }
                let _ = done_tx.send(());
            });
        if spawned.is_err() {
            debug!("desktop notifications unavailable: cannot start worker thread");
            return Self {
                tx: Mutex::new(None),
                done: Mutex::new(None),
                logged,
            };
        }
        Self {
            tx: Mutex::new(Some(tx)),
            done: Mutex::new(Some(done_rx)),
            logged,
        }
    }
}

impl Default for DesktopNotifier {
    fn default() -> Self {
        Self::new()
    }
}

impl Notifier for DesktopNotifier {
    fn notify(&self, n: &Notification) {
        let guard = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = guard.as_ref() {
            match tx.try_send(n.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => debug!("notification queue full; dropping popup"),
                Err(TrySendError::Disconnected(_)) => {}
            }
        }
    }

    fn flush(&self, within: Duration) {
        // Closing the queue lets the worker drain what is queued and exit. Waiting is
        // bounded: a backend stuck in a D-Bus call must not hold the daemon's shutdown.
        drop(self.tx.lock().unwrap_or_else(|e| e.into_inner()).take());
        let done = self.done.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(done) = done {
            let _ = done.recv_timeout(within);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::NotifyKind;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    fn note(body: &str) -> Notification {
        Notification {
            kind: NotifyKind::Completed,
            title: "puppy".into(),
            body: body.into(),
        }
    }

    #[test]
    fn failing_backend_is_quiet() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let d = DesktopNotifier::with_backend(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
            Err("no D-Bus session".into())
        });
        for i in 0..3 {
            d.notify(&note(&format!("n{i}")));
        }
        d.flush(Duration::from_secs(1));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(d.logged.load(Ordering::SeqCst));
    }

    #[test]
    fn full_queue_drops_instead_of_blocking() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let d = DesktopNotifier::with_backend(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
            let _ = release_rx.recv();
            Ok(())
        });
        let start = Instant::now();
        for i in 0..100 {
            d.notify(&note(&format!("n{i}")));
        }
        assert!(start.elapsed() < Duration::from_millis(100));
        drop(release_tx); // every blocked and later recv returns at once
        d.flush(Duration::from_secs(1));
        assert!(calls.load(Ordering::SeqCst) <= QUEUE + 1);
    }

    #[test]
    fn flush_waits_for_queued_popups() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        let d = DesktopNotifier::with_backend(move |n| {
            std::thread::sleep(Duration::from_millis(50));
            s.lock().unwrap().push(n.body.clone());
            Ok(())
        });
        for i in 0..3 {
            d.notify(&note(&format!("n{i}")));
        }
        d.flush(Duration::from_secs(2));
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[test]
    fn flush_returns_within_deadline_when_backend_hangs() {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let d = DesktopNotifier::with_backend(move |_| {
            let _ = release_rx.recv();
            Ok(())
        });
        d.notify(&note("stuck"));
        let start = Instant::now();
        d.flush(Duration::from_millis(200));
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(150), "{took:?}");
        assert!(took < Duration::from_secs(1), "{took:?}");
        drop(release_tx);
    }
}
