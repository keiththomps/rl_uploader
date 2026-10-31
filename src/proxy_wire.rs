//! Raw WebSocket framing. Game frames are forwarded byte for byte, including fragmentation.
use crate::{
    Error, Result,
    protocol::{self, Guard},
};
use rand::RngCore;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

pub const MAX_FRAME: usize = 16 * 1024 * 1024;
pub const MAX_MESSAGE: usize = 32 * 1024 * 1024;
#[derive(Debug)]
pub struct Frame {
    pub raw: Vec<u8>,
    pub payload: Vec<u8>,
    pub opcode: u8,
    pub final_frame: bool,
}
pub fn next_frame(buffer: &mut Vec<u8>, masked: bool) -> Result<Option<Frame>> {
    if buffer.len() < 2 {
        return Ok(None);
    }
    let opcode = buffer[0] & 15;
    let fin = buffer[0] & 128 != 0;
    if buffer[0] & 112 != 0
        || ![0, 1, 2, 8, 9, 10].contains(&opcode)
        || (buffer[1] & 128 != 0) != masked
    {
        return Err(Error::new("Invalid WebSocket frame."));
    }
    let small = buffer[1] & 127;
    let (size, mut offset) = match small {
        126 => {
            if buffer.len() < 4 {
                return Ok(None);
            }
            let n = u16::from_be_bytes(buffer[2..4].try_into().unwrap()) as u64;
            if n < 126 {
                return Err(Error::new("Noncanonical WebSocket frame length."));
            }
            (n, 4)
        }
        127 => {
            if buffer.len() < 10 {
                return Ok(None);
            }
            let n = u64::from_be_bytes(buffer[2..10].try_into().unwrap());
            if n < 65536 {
                return Err(Error::new("Noncanonical WebSocket frame length."));
            }
            (n, 10)
        }
        n => (u64::from(n), 2),
    };
    if size > MAX_FRAME as u64 || (opcode >= 8 && (!fin || size > 125)) {
        return Err(Error::new("WebSocket frame exceeds limits."));
    }
    let mut mask = [0u8; 4];
    if masked {
        if buffer.len() < offset + 4 {
            return Ok(None);
        }
        mask.copy_from_slice(&buffer[offset..offset + 4]);
        offset += 4;
    }
    let end = offset + size as usize;
    if buffer.len() < end {
        return Ok(None);
    }
    let raw: Vec<_> = buffer.drain(..end).collect();
    let mut payload = raw[offset..].to_vec();
    if masked {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
    }
    Ok(Some(Frame {
        raw,
        payload,
        opcode,
        final_frame: fin,
    }))
}
pub fn encode_frame(payload: &[u8], opcode: u8, fin: bool, masked: bool) -> Vec<u8> {
    let mut result = vec![opcode | if fin { 128 } else { 0 }];
    let bit = if masked { 128 } else { 0 };
    match payload.len() {
        0..=125 => result.push(bit | payload.len() as u8),
        126..=65535 => {
            result.push(bit | 126);
            result.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        _ => {
            result.push(bit | 127);
            result.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
    }
    if masked {
        let mut mask = [0u8; 4];
        rand::rng().fill_bytes(&mut mask);
        result.extend_from_slice(&mask);
        result.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    } else {
        result.extend_from_slice(payload);
    }
    result
}
pub struct Request {
    pub id: String,
    pub payload: Vec<u8>,
    pub active: Arc<AtomicBool>,
}
type Reply = mpsc::Sender<Result<Vec<Value>>>;
pub struct Session {
    pub prefix: String,
    pub alive: AtomicBool,
    counter: AtomicU64,
    requests: mpsc::Sender<Request>,
    pending: Mutex<HashMap<String, Reply>>,
}
impl Session {
    pub fn new() -> (Arc<Self>, mpsc::Receiver<Request>) {
        let (tx, rx) = mpsc::channel();
        let nonce: [u8; 16] = rand::random();
        (
            Arc::new(Self {
                prefix: format!("RLUploader_{}_", hex::encode(nonce)),
                alive: AtomicBool::new(true),
                counter: AtomicU64::new(0),
                requests: tx,
                pending: Mutex::new(HashMap::new()),
            }),
            rx,
        )
    }
    pub fn history(&self, player: &str, guard: &Guard, timeout: Duration) -> Result<Vec<Value>> {
        protocol::check(guard)?;
        if !self.alive.load(Ordering::Relaxed) {
            return Err(Error::new(
                "Game session disconnected. Waiting for a new session.",
            ));
        }
        let id = format!(
            "{}{}",
            self.prefix,
            self.counter.fetch_add(1, Ordering::Relaxed)
        );
        let (tx, rx) = mpsc::channel();
        let active = Arc::new(AtomicBool::new(true));
        self.pending.lock().unwrap().insert(id.clone(), tx);
        let request = Request {
            id: id.clone(),
            payload: encode_frame(
                protocol::history_request(player, &id).as_bytes(),
                1,
                true,
                true,
            ),
            active: active.clone(),
        };
        let result = (|| {
            self.requests
                .send(request)
                .map_err(|_| Error::new("Game session disconnected."))?;
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                protocol::check(guard)?;
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(v) => return v,
                    Err(mpsc::RecvTimeoutError::Timeout) => (),
                    Err(_) => return Err(Error::new("Game session disconnected.")),
                }
            }
            Err(Error::new(
                "Live Match History request timed out. The game session remains connected.",
            ))
        })();
        active.store(false, Ordering::Relaxed);
        self.pending.lock().unwrap().remove(&id);
        result
    }
    /// Responses using our unique prefix never reach the game, including late replies.
    pub fn consume(&self, payload: &[u8]) -> bool {
        let Ok(text) = std::str::from_utf8(payload) else {
            return false;
        };
        let Some((id, body)) = protocol::split_response(text) else {
            return false;
        };
        if !id.starts_with(&self.prefix) {
            return false;
        }
        if let Some(tx) = self.pending.lock().unwrap().remove(id) {
            let _ = tx.send(protocol::parse_matches(body));
        }
        true
    }
    pub fn close(&self) {
        self.alive.store(false, Ordering::Relaxed);
        for (_, tx) in self.pending.lock().unwrap().drain() {
            let _ = tx.send(Err(Error::new(
                "Game session disconnected. Capture deferred.",
            )));
        }
    }
}
/// A fragmented server message is withheld until we can identify its owner.
#[derive(Default)]
pub struct RelayFrames {
    game_fragment: bool,
    game_size: usize,
    server_opcode: Option<u8>,
    server_raw: Vec<u8>,
    server_payload: Vec<u8>,
}
impl RelayFrames {
    pub fn can_inject(&self) -> bool {
        !self.game_fragment
    }
    pub fn from_game(&mut self, frame: Frame) -> Result<Vec<u8>> {
        if frame.opcode < 8 {
            if (frame.opcode == 0) != self.game_fragment {
                return Err(Error::new("Invalid game fragmentation."));
            }
            self.game_size += frame.payload.len();
            if self.game_size > MAX_MESSAGE {
                return Err(Error::new("Game message exceeds limits."));
            }
            self.game_fragment = !frame.final_frame;
            if frame.final_frame {
                self.game_size = 0;
            }
        }
        Ok(frame.raw)
    }
    pub fn from_server(&mut self, frame: Frame, session: &Session) -> Result<Option<Vec<u8>>> {
        if frame.opcode >= 8 {
            return Ok(Some(frame.raw));
        }
        if frame.opcode == 0 {
            if self.server_opcode.is_none() {
                return Err(Error::new("Unexpected server continuation."));
            }
        } else {
            if self.server_opcode.is_some() {
                return Err(Error::new("Invalid server fragmentation."));
            }
            self.server_opcode = Some(frame.opcode);
        }
        if self.server_raw.len() + frame.raw.len() > MAX_MESSAGE
            || self.server_payload.len() + frame.payload.len() > MAX_MESSAGE
        {
            return Err(Error::new("Server message exceeds limits."));
        }
        self.server_raw.extend_from_slice(&frame.raw);
        self.server_payload.extend_from_slice(&frame.payload);
        if !frame.final_frame {
            return Ok(None);
        }
        let mine = self.server_opcode == Some(1) && session.consume(&self.server_payload);
        self.server_opcode = None;
        self.server_payload.clear();
        let raw = std::mem::take(&mut self.server_raw);
        Ok(if mine { None } else { Some(raw) })
    }
}
#[derive(Default)]
pub struct Broker {
    current: Mutex<Option<Arc<Session>>>,
}
impl Broker {
    pub fn attach(&self, s: Arc<Session>) {
        *self.current.lock().unwrap() = Some(s);
    }
    pub fn detach(&self, s: &Arc<Session>) {
        let mut current = self.current.lock().unwrap();
        if current.as_ref().is_some_and(|v| Arc::ptr_eq(v, s)) {
            *current = None;
        }
    }
    pub fn session(&self) -> Option<Arc<Session>> {
        self.current
            .lock()
            .unwrap()
            .as_ref()
            .filter(|v| v.alive.load(Ordering::Relaxed))
            .cloned()
    }
}
