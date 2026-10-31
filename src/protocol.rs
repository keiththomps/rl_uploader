//! Unofficial Match History protocol; public client identifiers from dank/rlapi (MIT).
//! Credentials and service response bodies must never be logged.
use crate::{Error, Result, api, io_error, net_error, platform};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tungstenite::{Message, client::IntoClientRequest, stream::MaybeTlsStream};
use zeroize::Zeroizing;

pub const HOST: &str = "ws.rlpp.psynet.gg";
pub const MAX_JSON: usize = 4 * 1024 * 1024;
pub const GAME_VERSION: &str = "261013.63066.530866";
const FEATURE_SET: &str = "PrimeUpdate60_1";
const BUILD_SECRET: &str = "76656a5b51ec57a13549eef732c8a8367638aa3a7989bade1f7e4e5f93d57625";
const SIGNING_KEY: &[u8] = b"c338bd36fb8c42b1a431d30add939fc7";
const EOS_CLIENT: &str = "xyza7891p5D7s9R6Gm6moTHWGloerp7B";
const EOS_SECRET: &str = "Knh18du4NVlFs+3uQ+ZPpDCVto0WYf4yXP8+OcwVt1o";
const DEPLOYMENT: &str = "da32ae9c12ae40e8a112c52e1f17f3ba";
const EOS_ROOT: &str = "https://api.epicgames.dev/epic/oauth/v2/";

pub type Guard = Arc<dyn Fn() -> bool + Send + Sync>;
pub fn check(guard: &Guard) -> Result<()> {
    if guard() {
        Ok(())
    } else {
        Err(Error::new(
            "Game state changed or capture paused. Capture deferred.",
        ))
    }
}
pub fn build_id(version: &str) -> i32 {
    let mut crc = u32::MAX;
    for b in version.encode_utf16().flat_map(u16::to_le_bytes) {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            crc = (crc << 1) ^ if crc & 0x80000000 != 0 { 0x04C11DB7 } else { 0 };
        }
    }
    (crc ^ u32::MAX) as i32
}
pub fn sign(body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(SIGNING_KEY).unwrap();
    mac.update(b"-");
    mac.update(body);
    STANDARD.encode(mac.finalize().into_bytes())
}
pub fn machine_error(v: &Value) -> &str {
    for key in ["error", "errorCode"] {
        let s = v[key]
            .as_str()
            .unwrap_or_default()
            .trim_start_matches("errors.com.epicgames.account.oauth.");
        if [
            "authorization_pending",
            "slow_down",
            "access_denied",
            "expired_token",
            "invalid_grant",
            "invalid_client",
            "invalid_token",
        ]
        .contains(&s)
        {
            return s;
        }
    }
    ""
}
fn response_json(response: reqwest::blocking::Response) -> Result<Value> {
    let status = response.status().as_u16();
    let data = Zeroizing::new(api::read_limited(response, MAX_JSON)?);
    let value: Value = serde_json::from_slice(&data)
        .map_err(|_| Error::new("Cloud service returned invalid JSON."))?;
    if !value.is_object() {
        return Err(Error::new("Cloud service returned an invalid response."));
    }
    if status != 200 {
        let code = machine_error(&value).to_string();
        let pending = ["authorization_pending", "slow_down"].contains(&code.as_str());
        return Err(Error {
            message: if pending {
                "Waiting for Epic sign-in.".into()
            } else {
                format!("Cloud authentication returned HTTP {status}. Reconnect Epic if needed.")
            },
            code,
            permanent: !pending && [400, 401, 403, 404].contains(&status),
        });
    }
    Ok(value)
}
#[derive(Clone)]
pub struct EpicAuth {
    path: PathBuf,
}
impl EpicAuth {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("epic.dpapi"),
        }
    }
    pub fn connected(&self) -> bool {
        self.path.exists()
    }
    fn save(&self, mut value: Value) -> Result<Value> {
        let account = value["account_id"].as_str().unwrap_or_default();
        if account.len() != 32
            || !account.bytes().all(|b| b.is_ascii_hexdigit())
            || value["access_token"]
                .as_str()
                .unwrap_or_default()
                .is_empty()
            || value["refresh_token"]
                .as_str()
                .unwrap_or_default()
                .is_empty()
        {
            return Err(Error::permanent(
                "Epic returned an incomplete account token.",
            ));
        }
        value["saved_at"] = json!(crate::now());
        let raw = Zeroizing::new(serde_json::to_vec(&value).map_err(io_error)?);
        platform::atomic_write(&self.path, &platform::dpapi(&raw, true)?)?;
        Ok(value)
    }
    fn request_token(&self, fields: &[(&str, &str)]) -> Result<Value> {
        let mut values = fields.to_vec();
        values.extend([("deployment_id", DEPLOYMENT), ("scope", "basic_profile")]);
        response_json(
            api::client(20)?
                .post(format!("{EOS_ROOT}token"))
                .basic_auth(EOS_CLIENT, Some(EOS_SECRET))
                .form(&values)
                .send()
                .map_err(net_error)?,
        )
    }
    pub fn begin(&self) -> Result<Value> {
        let v = response_json(
            api::client(20)?
                .post(format!("{EOS_ROOT}deviceAuthorization"))
                .form(&[("client_id", EOS_CLIENT)])
                .send()
                .map_err(net_error)?,
        )?;
        let uri = url::Url::parse(v["verification_uri"].as_str().unwrap_or_default())
            .map_err(|_| Error::new("Unexpected Epic sign-in address."))?;
        let code = v["user_code"].as_str().unwrap_or_default();
        if uri.scheme() != "https"
            || ![Some("epicgames.com"), Some("www.epicgames.com")].contains(&uri.host_str())
            || !uri.username().is_empty()
            || uri.password().is_some()
            || uri.port_or_known_default() != Some(443)
        {
            return Err(Error::permanent("Unexpected Epic sign-in address."));
        }
        if !(4..=24).contains(&code.len())
            || !code.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            || v["device_code"].as_str().unwrap_or_default().is_empty()
        {
            return Err(Error::new("Epic returned an invalid device code."));
        }
        Ok(v)
    }
    pub fn finish(&self, device: &Value, cancel: &AtomicBool) -> Result<()> {
        let mut interval = device["interval"].as_u64().unwrap_or(5).clamp(5, 60);
        let deadline = Instant::now()
            + Duration::from_secs(device["expires_in"].as_u64().unwrap_or(600).clamp(1, 1800));
        while Instant::now() < deadline {
            let next = (Instant::now() + Duration::from_secs(interval)).min(deadline);
            while Instant::now() < next {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::new("Epic sign-in cancelled."));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::new("Epic sign-in cancelled."));
            }
            if Instant::now() >= deadline {
                break;
            }
            match self.request_token(&[
                ("grant_type", "device_code"),
                (
                    "device_code",
                    device["device_code"].as_str().unwrap_or_default(),
                ),
            ]) {
                Ok(token) => {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(Error::new("Epic sign-in cancelled."));
                    }
                    self.save(token)?;
                    return Ok(());
                }
                Err(e) if e.code == "authorization_pending" => (),
                Err(e) if e.code == "slow_down" => interval = interval.saturating_add(5),
                Err(e) => return Err(e),
            }
        }
        Err(Error::new(
            "This code expired. Choose Get a new code to continue.",
        ))
    }
    pub fn token(&self) -> Result<Value> {
        let raw = platform::dpapi(
            &std::fs::read(&self.path)
                .map_err(|_| Error::permanent("Epic credentials unavailable. Reconnect Epic."))?,
            false,
        )?;
        let value: Value = serde_json::from_slice(&raw)
            .map_err(|_| Error::permanent("Epic credentials unavailable. Reconnect Epic."))?;
        if value["saved_at"].as_f64().unwrap_or(0.0) + value["expires_in"].as_f64().unwrap_or(0.0)
            > crate::now() + 120.0
        {
            return Ok(value);
        }
        let refresh = value["refresh_token"]
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Error::permanent("Epic credentials unavailable. Reconnect Epic."))?;
        self.save(
            self.request_token(&[("grant_type", "refresh_token"), ("refresh_token", refresh)])?,
        )
    }
}
pub fn history_request(player: &str, id: &str) -> String {
    let body = serde_json::to_string(&json!({"PlayerID": player})).unwrap();
    format!(
        "PsyService: Matches/GetMatchHistory v1\r\nPsyRequestID: {id}\r\nPsySig: {}\r\n\r\n{body}",
        sign(body.as_bytes())
    )
}
pub fn split_response(raw: &str) -> Option<(&str, &str)> {
    let (headers, body) = raw.split_once("\r\n\r\n")?;
    let id = headers
        .split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| *key == "PsyResponseID")?
        .1
        .trim();
    Some((id, body))
}
pub fn parse_matches(body: &str) -> Result<Vec<Value>> {
    if body.len() > MAX_JSON {
        return Err(Error::new("Match History exceeded the size limit."));
    }
    let value: Value =
        serde_json::from_str(body).map_err(|_| Error::new("Invalid Match History response."))?;
    if value
        .get("Error")
        .is_some_and(|e| !e.is_null() && e != &Value::Bool(false))
    {
        return Err(Error::permanent("Match History request was rejected."));
    }
    value["Result"]["Matches"]
        .as_array()
        .filter(|v| v.len() <= 100)
        .cloned()
        .ok_or_else(|| Error::new("Unexpected Match History format."))
}
pub fn offline_history(auth: &EpicAuth, guard: &Guard) -> Result<(String, Vec<Value>)> {
    check(guard)?;
    let token = auth.token()?;
    check(guard)?;
    let account = token["account_id"]
        .as_str()
        .ok_or_else(|| Error::permanent("Reconnect Epic."))?;
    let access = token["access_token"]
        .as_str()
        .ok_or_else(|| Error::permanent("Reconnect Epic."))?;
    let player = format!("Epic|{account}|0");
    let body = Zeroizing::new(serde_json::to_string(&json!({"Platform":"Epic", "PlayerName":"", "PlayerID":account,"Language":"INT","AuthTicket":access,"BuildRegion":"","FeatureSet":FEATURE_SET,"Device":"PC","LocalFirstPlayerID":player,"bSkipAuth":false,"bSetAsPrimaryAccount":true,"EpicAuthTicket":access,"EpicAccountID":account})).map_err(io_error)?);
    let response = api::client(20)?
        .post("https://api.rlpp.psynet.gg/rpc/Auth/AuthPlayer/v2")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header(
            "User-Agent",
            format!("RL Win/{GAME_VERSION} gzip (x86_64-pc-win32) curl-7.67.0 Schannel"),
        )
        .header("PsyBuildID", build_id(GAME_VERSION).to_string())
        .header("PsyEnvironment", "Prod")
        .header("PsyRequestID", "PsyNetMessage_X_0")
        .header("PsySig", sign(body.as_bytes()))
        .header("PsyBuildSecret", BUILD_SECRET)
        .body(body.to_string())
        .send()
        .map_err(net_error)?;
    let wrapper = response_json(response)?;
    if !wrapper["Error"].is_null() || !wrapper["Result"].is_object() {
        return Err(Error::permanent(
            "Rocket League cloud login was rejected. Protocol/build may need updating.",
        ));
    }
    let result = &wrapper["Result"];
    let url = url::Url::parse(result["PerConURLv2"].as_str().unwrap_or_default())
        .map_err(|_| Error::new("Unexpected cloud WebSocket destination."))?;
    if url.scheme() != "wss"
        || url.host_str() != Some(HOST)
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(Error::permanent("Unexpected cloud WebSocket destination."));
    }
    let mut request = url.as_str().into_client_request().map_err(net_error)?;
    for (key, value) in [
        ("PsyBuildID", build_id(GAME_VERSION).to_string()),
        ("User-Agent", format!("RL Win/{GAME_VERSION} gzip")),
        ("PsyEnvironment", "Prod".into()),
        (
            "PsyToken",
            result["PsyToken"].as_str().unwrap_or_default().into(),
        ),
        (
            "PsySessionID",
            result["SessionID"].as_str().unwrap_or_default().into(),
        ),
    ] {
        if value.is_empty() {
            return Err(Error::new("Cloud returned incomplete session headers."));
        }
        request.headers_mut().insert(
            tungstenite::http::HeaderName::from_bytes(key.as_bytes()).map_err(net_error)?,
            value.parse().map_err(net_error)?,
        );
    }
    check(guard)?;
    // connect_with_config max_redirects=0; no session headers cross destinations.
    let cfg = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_JSON))
        .max_frame_size(Some(MAX_JSON));
    let (mut ws, _) =
        tungstenite::client::connect_with_config(request, Some(cfg), 0).map_err(net_error)?;
    let stream = match ws.get_mut() {
        MaybeTlsStream::Plain(s) => s,
        MaybeTlsStream::Rustls(s) => &mut s.sock,
        _ => return Err(Error::new("Unexpected WebSocket transport.")),
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(net_error)?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(net_error)?;
    let answer = (|| {
        check(guard)?;
        let id = "PsyNetMessage_X_1";
        ws.send(Message::Text(history_request(&player, id).into()))
            .map_err(net_error)?;
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            check(guard)?;
            let raw = match ws.read() {
                Ok(v) => v,
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(e) => return Err(net_error(e)),
            };
            if let Message::Text(text) = raw {
                if text.starts_with("PsyPing:") {
                    ws.send(Message::Text("PsyPong:\r\n\r\n".into()))
                        .map_err(net_error)?;
                    continue;
                }
                if let Some((response_id, data)) = split_response(&text)
                    && response_id == id
                {
                    return Ok((account.into(), parse_matches(data)?));
                }
            }
        }
        Err(Error::new("Match History request timed out."))
    })();
    let _ = ws.close(None);
    answer
}
