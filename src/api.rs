use crate::{Error, Result, net_error};
use reqwest::{
    blocking::{Client, Response, multipart},
    header::HeaderMap,
};
use serde_json::Value;
use std::{
    io::Read,
    time::{Duration, UNIX_EPOCH},
};
use zeroize::Zeroizing;

pub fn client(timeout: u64) -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(timeout))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("RLUploader/0.4")
        .build()
        .map_err(net_error)
}
pub fn read_limited(mut r: Response, max: usize) -> Result<Vec<u8>> {
    if r.content_length().is_some_and(|n| n > max as u64) {
        return Err(Error::new("Service response exceeded the size limit."));
    }
    let mut data = Vec::new();
    r.by_ref()
        .take(max as u64 + 1)
        .read_to_end(&mut data)
        .map_err(net_error)?;
    if data.len() > max {
        return Err(Error::new("Service response exceeded the size limit."));
    }
    Ok(data)
}
pub fn retry_after(value: Option<&str>, now: f64, fallback: f64) -> f64 {
    value
        .and_then(|s| {
            s.parse::<f64>().ok().filter(|v| v.is_finite()).or_else(|| {
                httpdate::parse_http_date(s)
                    .ok()
                    .and_then(|v| v.duration_since(UNIX_EPOCH).ok())
                    .map(|v| v.as_secs_f64() - now)
            })
        })
        .unwrap_or(fallback)
        .clamp(1.0, 7.0 * 86400.0)
}
pub struct UploadResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub value: Value,
}
impl UploadResponse {
    pub fn replay_id(&self) -> Option<&str> {
        self.value.get("id").and_then(Value::as_str).filter(|s| {
            !s.is_empty()
                && s.len() <= 100
                && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
    }
    pub fn delay(&self, now: f64, default: f64) -> f64 {
        retry_after(
            self.headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok()),
            now,
            default,
        )
    }
}
pub trait Upload: Send {
    fn upload(&self, data: Vec<u8>, visibility: &str) -> Result<UploadResponse>;
}
pub struct Ballchasing {
    client: Client,
    token: Zeroizing<String>,
    root: String,
}
impl Ballchasing {
    pub fn new(token: Zeroizing<String>) -> Result<Self> {
        Self::with_root(token, "https://ballchasing.com/api")
    }
    pub fn with_root(token: Zeroizing<String>, root: &str) -> Result<Self> {
        if !crate::platform::valid_token(&token) {
            return Err(Error::new("Invalid ballchasing token."));
        }
        Ok(Self {
            client: client(30)?,
            token,
            root: root.into(),
        })
    }
    pub fn ping(&self) -> Result<Value> {
        let r = self
            .client
            .get(format!("{}/", self.root))
            .header("Authorization", self.token.as_str())
            .send()
            .map_err(net_error)?;
        let status = r.status().as_u16();
        if status != 200 {
            return Err(Error::new(format!("Token test returned HTTP {status}.")));
        }
        serde_json::from_slice(&read_limited(r, 1024 * 1024)?)
            .map_err(|_| Error::new("Invalid token-test response."))
    }
}
impl Upload for Ballchasing {
    fn upload(&self, data: Vec<u8>, visibility: &str) -> Result<UploadResponse> {
        let form = multipart::Form::new().part(
            "file",
            multipart::Part::bytes(data)
                .file_name("replay.replay")
                .mime_str("application/octet-stream")
                .map_err(net_error)?,
        );
        let r = self
            .client
            .post(format!("{}/v2/upload", self.root))
            .query(&[("visibility", visibility)])
            .header("Authorization", self.token.as_str())
            .multipart(form)
            .send()
            .map_err(net_error)?;
        let status = r.status().as_u16();
        let headers = r.headers().clone();
        let value = serde_json::from_slice(&read_limited(r, 1024 * 1024)?).unwrap_or(Value::Null);
        Ok(UploadResponse {
            status,
            headers,
            value,
        })
    }
}
