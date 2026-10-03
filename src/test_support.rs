use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::api::SocketClient;

/// Serializes tests that still mutate process-global environment variables.
pub static ENV_MUTEX: Mutex<()> = Mutex::new(());

/// Private filesystem state for a test, removed even when an assertion panics.
pub struct TestDir(PathBuf);

impl TestDir {
    pub fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // Keep Unix socket paths below macOS's 104-byte limit, even when
        // nix-shell sets a long TMPDIR.
        let path = PathBuf::from("/tmp").join(format!(
            "cast-{}-{nonce:x}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A scripted local socket, never a Herdr server. Each response consumes one
/// connection and finish returns the actual requests observed on the wire.
pub struct SocketFixture {
    directory: TestDir,
    worker: Option<JoinHandle<Vec<Value>>>,
}

impl SocketFixture {
    pub fn new(responses: Vec<Value>) -> Self {
        let directory = TestDir::new();
        let listener = UnixListener::bind(directory.path().join("socket")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let worker = std::thread::spawn(move || {
            responses
                .into_iter()
                .map(|response| {
                    let mut stream = accept(&listener);
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut payload = String::new();
                    stream.read_to_string(&mut payload).unwrap();
                    let request = serde_json::from_str(&payload).unwrap();
                    serde_json::to_writer(&mut stream, &response).unwrap();
                    stream.write_all(b"\n").unwrap();
                    request
                })
                .collect()
        });
        Self {
            directory,
            worker: Some(worker),
        }
    }

    pub fn client(&self) -> SocketClient {
        SocketClient::with_timeout(
            self.directory.path().join("socket").to_str().unwrap(),
            Duration::from_secs(1),
        )
    }

    pub fn finish(mut self) -> Vec<Value> {
        self.worker.take().unwrap().join().unwrap()
    }
}

impl Drop for SocketFixture {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn accept(listener: &UnixListener) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("fixture accept failed: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "fixture did not receive its expected request"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
