//! Asynchronous request logging. The request thread formats its access line and hands it
//! to a writer thread, so a slow log consumer (a full pipe, a busy journald) never stalls
//! a request. Lines are written as soon as the queue runs dry, so an idle tail appears at
//! once; a crash may lose the last incomplete batch, as with any buffered access log.
//! The audit log is a different thing entirely and stays synchronous.

use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

/// Lines the queue holds; beyond that, lines are dropped and counted, never waited for.
const QUEUE: usize = 4_096;
/// Write a batch out once it has grown to this many bytes.
const BATCH: usize = 16 * 1024;

struct AsyncLog {
    tx: Option<SyncSender<String>>,
    dropped: AtomicU64,
}

static LOG: OnceLock<AsyncLog> = OnceLock::new();

fn log() -> &'static AsyncLog {
    LOG.get_or_init(|| {
        let (tx, rx) = sync_channel::<String>(QUEUE);
        let spawned = std::thread::Builder::new()
            .name("request-log".to_string())
            .stack_size(32 * 1024)
            .spawn(move || pump(rx, Stderr))
            .is_ok();
        AsyncLog {
            tx: spawned.then_some(tx),
            dropped: AtomicU64::new(0),
        }
    })
}

/// One access line, already formatted: queued for the writer thread, or dropped (and
/// counted) when the queue is full. Never blocks the caller.
pub fn line(s: String) {
    let l = log();
    match &l.tx {
        Some(tx) => {
            if let Err(TrySendError::Full(_)) = tx.try_send(s) {
                l.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        // no writer thread could be started: write synchronously after all
        None => eprintln!("{s}"),
    }
}

/// Access lines dropped because the queue was full (shown by `/metrics`).
pub fn dropped() -> u64 {
    log().dropped.load(Ordering::Relaxed)
}

/// Where batches go: the process's standard error, locked only while a batch is written,
/// so one-time messages printed by other threads are never held up.
struct Stderr;

impl Write for Stderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut err = std::io::stderr().lock();
        err.write_all(buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().lock().flush()
    }
}

fn push(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(s.as_bytes());
    buf.push(b'\n');
}

/// How long the writer waits between looks at the queue. The request thread never wakes
/// the writer (a queued line is picked up on the next look), so logging costs it nothing
/// but the queueing itself; an access line appears within this delay.
const POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// Poll the queue, write what is there in one batch, sleep, and repeat; when every
/// sender is gone, write what is left and stop. Never blocks, so a request thread
/// handing over a line never waits for the writer (or a slow log consumer behind it).
fn pump<W: Write>(rx: Receiver<String>, mut out: W) {
    let mut buf: Vec<u8> = Vec::with_capacity(BATCH);
    loop {
        let mut got = false;
        loop {
            match rx.try_recv() {
                Ok(s) => {
                    push(&mut buf, &s);
                    got = true;
                    if buf.len() >= BATCH {
                        break;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    let _ = out.write_all(&buf);
                    let _ = out.flush();
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }
        if got {
            let _ = out.write_all(&buf);
            let _ = out.flush();
            buf.clear();
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    #[test]
    fn writes_batches_and_drains_the_queue() {
        let (tx, rx) = sync_channel(8);
        tx.send("first".to_string()).unwrap();
        tx.send("second".to_string()).unwrap();
        drop(tx);
        let mut out = Vec::new();
        pump(rx, &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "first\nsecond\n");
    }

    #[test]
    fn a_full_batch_writes_even_with_senders_alive() {
        let (tx, rx) = sync_channel(QUEUE + 8);
        for i in 0..QUEUE + 8 {
            tx.send(format!("line {i}")).unwrap();
        }
        let mut out = Vec::new();
        // the first pump pass writes the first BATCH bytes and then sleeps; wake it by
        // dropping the sender so it flushes the rest and returns
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(tx);
        });
        pump(rx, &mut out);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), QUEUE + 8);
        assert!(text.starts_with("line 0\n"));
        assert!(text.ends_with(&format!("line {}\n", QUEUE + 7)));
    }
}
