//! Private inherited pipes between the tray and its disposable settings window.
//! No listening port, credentials on the wire, or independently writable control file.
use crate::{
    Error, Result, io_error,
    worker::{Command, Snapshot, WorkerHandle},
};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, Read, Write},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

#[derive(Serialize, Deserialize)]
pub enum Request {
    Snapshot,
    Control(Command),
}
#[derive(Serialize, Deserialize)]
pub struct Response {
    pub snapshot: Snapshot,
}
pub fn read_message<T: serde::de::DeserializeOwned>(
    reader: &mut impl BufRead,
    max: u64,
) -> Result<T> {
    let mut raw = Vec::new();
    reader
        .take(max + 1)
        .read_until(b'\n', &mut raw)
        .map_err(io_error)?;
    if raw.len() as u64 > max || raw.last() != Some(&b'\n') {
        return Err(Error::new(
            "Settings connection closed or exceeded its size limit.",
        ));
    }
    serde_json::from_slice(&raw).map_err(|_| Error::new("Invalid settings message."))
}
pub fn write_message(value: &impl Serialize, writer: &mut impl Write) -> Result<()> {
    serde_json::to_writer(&mut *writer, value).map_err(io_error)?;
    writer.write_all(b"\n").map_err(io_error)?;
    writer.flush().map_err(io_error)
}
pub fn serve(mut input: impl BufRead, mut output: impl Write, worker: WorkerHandle) {
    while let Ok(request) = read_message::<Request>(&mut input, 65536) {
        match request {
            Request::Snapshot => (),
            Request::Control(command) => {
                if matches!(
                    command,
                    Command::Stop | Command::FilesChanged | Command::WatchLost
                ) {
                    break;
                }
                if let Command::Apply(ref settings, _) = command
                    && settings.validate().is_err()
                {
                    break;
                }
                worker.send(command);
            }
        }
        let response = Response {
            snapshot: worker.snapshot.lock().unwrap().clone(),
        };
        if write_message(&response, &mut output).is_err() {
            break;
        }
    }
}
pub struct Frontend {
    pub snapshot: Arc<Mutex<Snapshot>>,
    tx: mpsc::Sender<Command>,
}
impl Frontend {
    pub fn connect() -> Result<Self> {
        let mut input = std::io::BufReader::new(std::io::stdin());
        let mut output = std::io::stdout();
        write_message(&Request::Snapshot, &mut output)?;
        let initial: Response = read_message(&mut input, 1024 * 1024)?;
        let snapshot = Arc::new(Mutex::new(initial.snapshot));
        let (tx, rx) = mpsc::channel();
        let status = snapshot.clone();
        std::thread::spawn(move || {
            loop {
                let request = match rx.recv_timeout(Duration::from_secs(1)) {
                    Ok(c) => Request::Control(c),
                    Err(mpsc::RecvTimeoutError::Timeout) => Request::Snapshot,
                    Err(_) => break,
                };
                let result = (|| {
                    write_message(&request, &mut output)?;
                    read_message::<Response>(&mut input, 1024 * 1024)
                })();
                match result {
                    Ok(response) => {
                        *status.lock().unwrap() = response.snapshot;
                    }
                    Err(_) => {
                        status.lock().unwrap().error =
                            "The tray app has stopped. Close this window and reopen RL Uploader."
                                .into();
                        break;
                    }
                }
            }
        });
        Ok(Self { snapshot, tx })
    }
    pub fn send(&self, command: Command) {
        let _ = self.tx.send(command);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn messages_are_bounded_and_require_complete_lines() {
        assert!(read_message::<Request>(&mut std::io::Cursor::new(b"\"Snapshot\"\n"), 32).is_ok());
        assert!(read_message::<Request>(&mut std::io::Cursor::new(b"\"Snapshot\""), 32).is_err());
        assert!(read_message::<Request>(&mut std::io::Cursor::new(vec![b'x'; 100]), 32).is_err());
    }
}
