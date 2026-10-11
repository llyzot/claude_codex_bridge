//! Bounded provider input with immediate writer-failure observation.
use serde_json::Value;
use std::{
    io::Write,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
        Arc,
    },
};

pub(crate) fn spawn(
    mut stdin: impl Write + Send + 'static,
) -> (SyncSender<Value>, Arc<AtomicBool>) {
    let (commands, input) = mpsc::sync_channel::<Value>(16);
    let failed = Arc::new(AtomicBool::new(false));
    let writer_failed = failed.clone();
    std::thread::spawn(move || {
        for value in input {
            let Ok(mut bytes) = serde_json::to_vec(&value) else {
                writer_failed.store(true, Ordering::Release);
                break;
            };
            bytes.push(b'\n');
            if stdin.write_all(&bytes).and_then(|_| stdin.flush()).is_err() {
                writer_failed.store(true, Ordering::Release);
                break;
            }
        }
        // Dropping all senders is intentional stdin EOF, not a write failure.
    });
    (commands, failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io, sync::Mutex, time::Duration};
    struct Sink {
        bytes: Arc<Mutex<Vec<u8>>>,
        fail_write: bool,
        fail_flush: bool,
        done: mpsc::Sender<()>,
    }
    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.fail_write {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            let size = buf.len().min(3);
            self.bytes.lock().unwrap().extend_from_slice(&buf[..size]);
            Ok(size)
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                Err(io::ErrorKind::BrokenPipe.into())
            } else {
                Ok(())
            }
        }
    }
    impl Drop for Sink {
        fn drop(&mut self) {
            let _ = self.done.send(());
        }
    }
    #[test]
    fn intentional_close_drains_utf8_frames_without_failure() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (done, wait) = mpsc::channel();
        let (sender, failed) = spawn(Sink {
            bytes: bytes.clone(),
            fail_write: false,
            fail_flush: false,
            done,
        });
        let values = [
            serde_json::json!({"text":"中文\nsecond line"}),
            serde_json::json!({"type":"abort"}),
        ];
        for value in &values {
            sender.send(value.clone()).unwrap();
        }
        drop(sender);
        wait.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!failed.load(Ordering::Acquire));
        let content = bytes.lock().unwrap();
        let decoded: Vec<Value> = std::str::from_utf8(&content)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(decoded, values);
    }
    #[test]
    fn write_and_flush_failures_are_visible_without_a_second_enqueue() {
        for flush in [false, true] {
            let bytes = Arc::new(Mutex::new(Vec::new()));
            let (done, wait) = mpsc::channel();
            let (sender, failed) = spawn(Sink {
                bytes: bytes.clone(),
                fail_write: !flush,
                fail_flush: flush,
                done,
            });
            sender
                .send(serde_json::json!({"text":"first input"}))
                .unwrap();
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(failed.load(Ordering::Acquire));
            assert_eq!(!bytes.lock().unwrap().is_empty(), flush);
        }
    }
}
