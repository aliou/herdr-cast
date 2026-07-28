use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

pub struct SocketClient {
    path: String,
    timeout: Option<Duration>,
}

#[derive(Serialize)]
struct Request<'a, P> {
    id: &'a str,
    method: &'a str,
    params: P,
}

impl SocketClient {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            timeout: None,
        }
    }

    pub fn with_timeout(path: impl Into<String>, timeout: Duration) -> Self {
        Self {
            path: path.into(),
            timeout: Some(timeout),
        }
    }

    pub fn send<P: Serialize>(&self, id: &str, method: &str, params: P) -> Result<Value, String> {
        let request = Request { id, method, params };
        let mut payload = serde_json::to_vec(&request)
            .map_err(|error| format!("failed to encode {method} request: {error}"))?;
        payload.push(b'\n');

        let Some(timeout) = self.timeout else {
            return send_blocking(&self.path, method, &payload, None);
        };
        let path = self.path.clone();
        let method_name = method.to_owned();
        let worker_method = method_name.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = sender.send(send_blocking(
                &path,
                &worker_method,
                &payload,
                Some(timeout),
            ));
        });
        receiver
            .recv_timeout(timeout)
            .map_err(|_| format!("{method_name} request timed out after {timeout:?}"))?
    }
}

fn send_blocking(
    path: &str,
    method: &str,
    payload: &[u8],
    timeout: Option<Duration>,
) -> Result<Value, String> {
    let mut stream = UnixStream::connect(path)
        .map_err(|error| format!("failed to connect to Herdr socket: {error}"))?;
    if let Some(timeout) = timeout {
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|error| format!("failed to set {method} read timeout: {error}"))?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|error| format!("failed to set {method} write timeout: {error}"))?;
    }
    stream
        .write_all(payload)
        .map_err(|error| format!("failed to write {method} request: {error}"))?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|error| format!("failed to finish {method} request: {error}"))?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| format!("failed to read {method} response: {error}"))?;
    let value: Value = serde_json::from_str(&response)
        .map_err(|error| format!("invalid {method} response: {error}"))?;
    if let Some(error) = value.get("error") {
        return Err(format!("{method} request failed: {error}"));
    }
    Ok(value)
}
