//! Dedicated operator surface. Singer and booth-remote credentials cannot open this service.
pub mod operator;
mod preamp;
mod sync;
use crate::{protocol::ServerToHostMessage, state::ServerState, xr12::Mixer};
use axum::{
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io,
    net::{Ipv4Addr, SocketAddr, TcpListener},
    sync::{Arc, Mutex},
    time::Instant,
};
use uuid::Uuid;

#[derive(Clone, Serialize)]
pub struct Playback {
    pub load_gen: u64,
    pub state: String,
    pub elapsed: f64,
    pub duration: f64,
}

#[derive(Clone, Default)]
pub struct TurnTargets {
    pub active: Option<String>,
    pub next: Option<String>,
    pub next_singers: Vec<String>,
    pub next_singer_ids: Vec<String>,
    pub next_title: String,
}

/// The desktop supplies its authoritative audio-engine state and existing pause/resume handlers.
pub trait StudioPlayer: Send + Sync {
    fn operator(
        &self,
        _action: operator::Action,
        _grant: Grant,
    ) -> futures::future::BoxFuture<'_, Result<Value, String>> {
        Box::pin(async { Err("Show operator unavailable".into()) })
    }
    fn playback(&self) -> Playback;
    fn break_status(&self) -> Value { Value::Null }
    fn turn_targets(&self, _rotation_revision: u64) -> TurnTargets {
        TurnTargets::default()
    }
    fn set_paused(&self, paused: bool) -> Result<(), String>;
}

struct Desk {
    host: Arc<ServerState>,
    player: Arc<dyn StudioPlayer>,
    mixer: Mixer,
    sessions: Mutex<HashMap<Uuid, Operator>>,
    commands: tokio::sync::Mutex<Commands>,
    // Serialize transport mutations while letting stations keep reading and mixing.
    transport: tokio::sync::Mutex<()>,
    operator: tokio::sync::Mutex<operator::Ledger>,
}

/// A desktop dispatch can outlive its HTTP authorization check. The main
/// window rechecks this capability when it claims the queued call.
#[derive(Clone)]
pub struct Grant {
    desk: std::sync::Weak<Desk>,
    station: Uuid,
}

impl Grant {
    pub fn live_flag(&self) -> Result<Arc<std::sync::atomic::AtomicBool>,String> {
        self.check()?;
        let desk = self.desk.upgrade().ok_or("Operator station closed")?;
        let sessions = desk.sessions.lock().unwrap_or_else(|e|e.into_inner());
        sessions.get(&self.station).map(|station|station.live.clone()).ok_or_else(||"Operator pairing revoked".into())
    }
    fn new(desk: &Arc<Desk>, station: Uuid) -> Self {
        Self { desk: Arc::downgrade(desk), station }
    }

    pub fn check(&self) -> Result<(), String> {
        let desk = self.desk.upgrade().ok_or("Operator station closed")?;
        if desk.host.is_closing()
            || !desk.sessions.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&self.station)
        {
            return Err("The operator pairing was revoked".into());
        }
        Ok(())
    }
}

#[derive(Default)]
struct Commands {
    requests: HashMap<(Uuid, Uuid), (Instant, Result<(), String>)>,
    pending: Option<(String, Instant)>,
    last_state: String,
    revision: Uuid,
    next_from: Option<String>,
}

impl Commands {
    fn observe(&mut self, hash: String, show: &Value) -> String {
        if self.last_state != hash {
            self.last_state = hash;
            self.revision = Uuid::new_v4();
            self.pending = None;
        }
        if self
            .next_from
            .as_ref()
            .is_some_and(|id| *id != current_turn(show))
        {
            self.next_from = None;
        }
        self.revision.to_string()
    }
}

fn current_turn(show: &Value) -> String {
    if let Some(id) = show["active_request"].as_str().or_else(|| show["next_request"].as_str()) {
        return id.to_owned();
    }
    let Some(slots) = show.get("slots").and_then(Value::as_array) else {
        return String::new();
    };
    slots
        .iter()
        .find(|s| s["is_current"] == true)
        .or_else(|| slots.first())
        .map(|s| format!("{}|{}", s["singer_id"], s["request_id"]))
        .unwrap_or_default()
}

struct Operator {
    live: Arc<std::sync::atomic::AtomicBool>,
    previous: sync::Previous,
    id: Uuid,
    armed_epoch: Option<String>,
    arm_generation: u64,
    last_snapshot: Instant,
}
// Sync normally runs every 100 ms. Allow one full 3.5 s client timeout plus
// scheduling slack, then require fresh state and explicit rearming.
const CONTROL_FRESHNESS: std::time::Duration = std::time::Duration::from_secs(5);
impl Operator {
    // An edit carries the state the station saw when it started the gesture.
    // Renewing control on the same physical mixer invalidates that older state.
    fn control_epoch(&self, mixer_epoch: &str) -> String {
        format!("{mixer_epoch}:{}", self.arm_generation)
    }

    fn set_armed_epoch(&mut self, next: Option<String>) {
        if self.armed_epoch != next {
            self.arm_generation = self.arm_generation.wrapping_add(1);
            self.armed_epoch = next;
        }
    }

    fn restart_control(&mut self) {
        self.armed_epoch = None;
        self.arm_generation = self.arm_generation.wrapping_add(1);
    }

    fn observe_snapshot(&mut self) {
        if self.last_snapshot.elapsed() >= CONTROL_FRESHNESS {
            self.restart_control();
        }
        self.last_snapshot = Instant::now();
    }
}
impl Drop for Operator {
    fn drop(&mut self) { self.live.store(false,std::sync::atomic::Ordering::Release); }
}

pub struct Studio {
    desk: Arc<Desk>,
    http: axum_server::Handle,
    https: axum_server::Handle,
    local_addr: SocketAddr,
    secure_addr: SocketAddr,
    fingerprint: String,
}

#[derive(Serialize)]
pub struct Pairing {
    pub id: Uuid,
    pub desktop_url: String,
    pub tablet_url: String,
    pub certificate_sha256: String,
}

impl Studio {
    pub async fn start(
        host: Arc<ServerState>,
        player: Arc<dyn StudioPlayer>,
        mixer_ip: Ipv4Addr,
        interface: Ipv4Addr,
    ) -> io::Result<Self> {
        if interface.is_unspecified() || interface.is_multicast() || interface.is_broadcast() {
            return Err(io::Error::other(
                "Choose this computer’s venue-network IPv4 address",
            ));
        }
        let mixer = Mixer::connect(mixer_ip).await?.with_host_calibration();
        let desk = Arc::new(Desk {
            host,
            player,
            mixer,
            sessions: Mutex::default(),
            commands: Default::default(),
            transport: Default::default(),
            operator: Default::default(),
        });
        let local = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let secure = TcpListener::bind((interface, 0))?;
        local.set_nonblocking(true)?;
        secure.set_nonblocking(true)?;
        let local_addr = local.local_addr()?;
        let secure_addr = secure.local_addr()?;
        let cert = rcgen::generate_simple_self_signed(vec![interface.to_string()])
            .map_err(io::Error::other)?;
        let der = cert.cert.der().clone();
        let fingerprint = hex::encode(Sha256::digest(&der));
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(
            vec![der],
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()).into(),
        )
        .map_err(io::Error::other)?;
        let tls = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(config));
        let app = Router::new()
            .merge(operator::routes())
            .merge(preamp::routes())
            .route("/", get(|| async { Html(include_str!("studio/index.html")) }))
            .route("/console.js", get(|| async { ([(header::CONTENT_TYPE,"text/javascript")], include_str!("studio/console.js")) }))
            .route("/console.css", get(|| async { ([(header::CONTENT_TYPE,"text/css")], include_str!("studio/console.css")) }))
            .route("/api/state", get(snapshot))
            .route("/api/sync", post(sync::updates))
            .route("/api/meters", get(sync::meters))
            .route("/api/arm", post(arm))
            .route("/api/mix", post(mix))
            .route("/api/send", post(send_assignment))
            .route("/api/fx", post(fx))
            .route("/api/show", post(show))
            .layer(DefaultBodyLimit::max(4096))
            .layer(axum::middleware::map_response(|mut res: axum::response::Response| async {
                let h = res.headers_mut();
                h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
                h.insert("referrer-policy", "no-referrer".parse().unwrap());
                h.insert("content-security-policy", "default-src 'self'; frame-ancestors 'none'; base-uri 'none'; object-src 'none'; style-src 'self' 'unsafe-inline'".parse().unwrap());
                res
            }))
            .with_state(desk.clone());
        let http = axum_server::Handle::new();
        let https = axum_server::Handle::new();
        let local_server = axum_server::from_tcp(local).handle(http.clone());
        let secure_server = axum_server::from_tcp_rustls(secure, tls).handle(https.clone());
        let local_app = app.clone();
        tokio::spawn(async move {
            if let Err(err) = local_server.serve(local_app.into_make_service()).await {
                tracing::warn!("Desktop console listener stopped: {err}");
            }
        });
        tokio::spawn(async move {
            if let Err(err) = secure_server.serve(app.into_make_service()).await {
                tracing::warn!("Tablet console listener stopped: {err}");
            }
        });
        Ok(Self {
            desk,
            http,
            https,
            local_addr,
            secure_addr,
            fingerprint,
        })
    }

    pub fn pair(&self) -> Result<Pairing, String> {
        let mut sessions = self.desk.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if sessions.len() >= 16 {
            return Err("Revoke an operator pairing before adding another".into());
        }
        let secret = Uuid::new_v4();
        let id = Uuid::new_v4();
        sessions.insert(
            secret,
            Operator {
                live: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                previous: Default::default(),
                id,
                armed_epoch: None,
                arm_generation: 0,
                last_snapshot: Instant::now(),
            },
        );
        Ok(Pairing {
            id,
            desktop_url: format!("http://{}/#token={secret}", self.local_addr),
            tablet_url: format!(
                "https://{}/#token={secret}&pin={}",
                self.secure_addr, self.fingerprint
            ),
            certificate_sha256: self.fingerprint.clone(),
        })
    }

    pub fn revoke(&self, id: Uuid) {
        self.desk
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, op| op.id != id);
    }

    pub fn operator_ids(&self)->Vec<Uuid> {
        let mut ids:Vec<_>=self.desk.sessions.lock().unwrap_or_else(|e|e.into_inner()).values().map(|op|op.id).collect();
        ids.sort();ids
    }
}

impl Drop for Studio {
    fn drop(&mut self) {
        self.desk
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.http.shutdown();
        self.https.shutdown();
    }
}

type Failure = (StatusCode, String);
fn conflict(message: impl Into<String>) -> Failure {
    (StatusCode::CONFLICT, message.into())
}
fn authenticate(desk: &Desk, headers: &HeaderMap) -> Result<Uuid, Failure> {
    if desk.host.is_closing() {
        return Err(conflict("Show host closed"));
    }
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .and_then(|s| s.parse::<Uuid>().ok());
    token
        .filter(|id| {
            desk.sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(id)
        })
        .ok_or((
            StatusCode::UNAUTHORIZED,
            "Pair this station in Auto-KJ Settings".into(),
        ))
}

async fn show_state(desk: &Desk) -> (Value, Playback, String) {
    let playback = desk.player.playback();
    let (serial, rotation) = if desk.host.has_host() {
        desk.host.versioned_host_rotation().await
    } else {
        (0, None)
    };
    let mut view = Value::Null;
    if let Some(ServerToHostMessage::RotationState {
        host_slots,
        is_break_playing,
        current_break_track,
        ..
    }) = rotation
    {
        let turns = desk.player.turn_targets(serial);
        view = json!({"slots":host_slots,"break_playing":is_break_playing,"break_track":current_break_track,
            "active_request":turns.active,"next_request":turns.next,"next_singers":turns.next_singers,
            "next_singer_ids":turns.next_singer_ids,"next_title":turns.next_title});
        view["break_status"] = desk.player.break_status();
    }
    let revision = hex::encode(Sha256::digest(
        format!("{}|{}|{}", view, playback.state, playback.load_gen).as_bytes(),
    ));
    (view, playback, revision)
}

async fn snapshot(
    State(desk): State<Arc<Desk>>,
    headers: HeaderMap,
) -> Result<Json<Value>, Failure> {
    let token = authenticate(&desk, &headers)?;
    let mut mixer = desk.mixer.snapshot().await;
    let (show, playback, revision) = {
        let mut commands = desk.commands.lock().await;
        let (view, player, hash) = show_state(&desk).await;
        let revision = commands.observe(hash, &view);
        (view, player, revision)
    };
    let mut sessions = desk.sessions.lock().unwrap_or_else(|e| e.into_inner());
    let op = sessions.get_mut(&token)
        .ok_or((StatusCode::UNAUTHORIZED, "Pairing revoked".into()))?;
    op.observe_snapshot();
    let armed = mixer.connected && op.armed_epoch.as_ref() == Some(&mixer.epoch);
    mixer.epoch = op.control_epoch(&mixer.epoch);
    drop(sessions);
    Ok(Json(
        json!({"mixer":mixer,"armed":armed,"show":show,"playback":playback,"revision":revision}),
    ))
}

#[derive(Deserialize)]
struct Arm {
    epoch: String,
    enabled: bool,
}
async fn arm(
    State(desk): State<Arc<Desk>>,
    headers: HeaderMap,
    Json(req): Json<Arm>,
) -> Result<StatusCode, Failure> {
    let token = authenticate(&desk, &headers)?;
    let mix = desk.mixer.snapshot().await;
    let mut sessions = desk.sessions.lock().unwrap_or_else(|e| e.into_inner());
    let op = sessions
        .get_mut(&token)
        .ok_or((StatusCode::UNAUTHORIZED, "Pairing revoked".into()))?;
    if req.epoch != op.control_epoch(&mix.epoch)
        || (req.enabled && (!mix.connected || op.last_snapshot.elapsed() >= CONTROL_FRESHNESS))
    {
        return Err(conflict("Refresh mixer state before changing control"));
    }
    op.set_armed_epoch(req.enabled.then_some(mix.epoch));
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct MixWrite {
    expected: Option<Value>,
    epoch: String,
    key: String,
    value: Value,
}
async fn mix(
    State(desk): State<Arc<Desk>>,
    headers: HeaderMap,
    Json(req): Json<MixWrite>,
) -> Result<Json<Value>, Failure> {
    let (mixer, epoch) = armed(&desk, &headers, &req.epoch)?;
    let result = match req.expected {
        Some(before) => {
            mixer
                .compare_and_set(&req.key, before, req.value, &epoch)
                .await
        }
        None => mixer.set(&req.key, req.value, &epoch).await,
    };
    result.map(Json).map_err(|e| conflict(e.to_string()))
}

fn armed(desk: &Arc<Desk>, headers: &HeaderMap, epoch: &str) -> Result<(Mixer, String), Failure> {
    let token = authenticate(desk, headers)?;
    let (generation, mixer_epoch) = desk.sessions.lock().unwrap_or_else(|e| e.into_inner())
        .get(&token)
        .and_then(|op| {
            let device = op.armed_epoch.as_ref()?;
            (op.last_snapshot.elapsed() < CONTROL_FRESHNESS && op.control_epoch(device) == epoch)
                .then(|| (op.arm_generation, device.clone()))
        })
        .ok_or_else(|| conflict("Mixer control changed. Check the current level and control status before retrying."))?;
    let weak = Arc::downgrade(desk);
    let mixer = desk.mixer.with_writer(Arc::new(move |socket, packet| {
        let Some(desk) = weak.upgrade() else { return Ok(false); };
        // Revocation and disarm take this same lock. Never hold it while awaiting I/O.
        let stations = desk.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if desk.host.is_closing() || stations.get(&token).is_none_or(|op|
            op.arm_generation != generation || op.last_snapshot.elapsed() >= CONTROL_FRESHNESS
        ) {
            return Ok(false);
        }
        socket.try_send(packet).map(|_| true)
    }));
    Ok((mixer, mixer_epoch))
}

#[derive(Deserialize)]
struct FxWrite {
    epoch: String,
    slot: u8,
    algorithm: u8,
    index: u8,
    value: Value,
}
async fn fx(
    State(desk): State<Arc<Desk>>,
    headers: HeaderMap,
    Json(req): Json<FxWrite>,
) -> Result<Json<Value>, Failure> {
    let (mixer, epoch) = armed(&desk, &headers, &req.epoch)?;
    mixer.set_fx_parameter(req.slot, req.algorithm, req.index, req.value, &epoch)
        .await
        .map(Json)
        .map_err(|e| conflict(e.to_string()))
}

#[derive(Deserialize)]
struct SendWrite {
    epoch: String,
    key: String,
    enabled: bool,
}
async fn send_assignment(
    State(desk): State<Arc<Desk>>,
    headers: HeaderMap,
    Json(req): Json<SendWrite>,
) -> Result<Json<Value>, Failure> {
    let (mixer, epoch) = armed(&desk, &headers, &req.epoch)?;
    mixer.set_send_enabled(&req.key, req.enabled, &epoch)
        .await
        .map(Json)
        .map_err(|e| conflict(e.to_string()))
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Play,
    Pause,
    Resume,
    Stop,
    Next,
    BreakPlay,
    BreakStop,
    BreakSkip,
}
#[derive(Deserialize)]
struct ShowWrite {
    id: Uuid,
    revision: String,
    action: Action,
}
async fn show(
    State(desk): State<Arc<Desk>>,
    headers: HeaderMap,
    Json(req): Json<ShowWrite>,
) -> Result<impl IntoResponse, Failure> {
    let _transport = desk.transport.lock().await;
    let mut commands = desk.commands.lock().await;
    let station = authenticate(&desk, &headers)?;
    let now = Instant::now();
    commands
        .requests
        .retain(|_, (at, _)| now.duration_since(*at).as_secs() < 300);
    let request_key = (station, req.id);
    if let Some((_, result)) = commands.requests.get(&request_key) {
        return result.clone().map(|()| StatusCode::ACCEPTED).map_err(conflict);
    }
    let (view, playback, hash) = show_state(&desk).await;
    let revision = commands.observe(hash, &view);
    if view.is_null() || req.revision != revision {
        return Err(conflict(
            "The show changed; refresh before sending a command",
        ));
    }
    if commands
        .pending
        .as_ref()
        .is_some_and(|(r, t)| *r == revision && now.duration_since(*t).as_secs() < 4)
    {
        return Err(conflict(
            "Waiting for the show host to confirm the previous change",
        ));
    }
    let target = match req.action {
        Action::Play | Action::Pause | Action::Resume | Action::Stop | Action::Next => {
            use operator::Transport;
            let command = match req.action {
                Action::Play if view["active_request"].is_null() && view["next_request"].is_string() && playback.state == "stopped" => Transport::Play,
                Action::Pause if playback.state == "playing" => Transport::Pause,
                Action::Resume if playback.state == "paused" => Transport::Resume,
                Action::Stop => Transport::Stop,
                Action::Next if commands.next_from.is_none() => Transport::Next,
                _ => return Err(conflict("The track or next singer changed. Refresh the console.")),
            };
            operator::Action::Transport {
                command,
                current: view["active_request"].as_str().map(String::from),
                next: view["next_request"].as_str().map(String::from),
                loaded: playback.state != "stopped",
                generation: playback.load_gen,
            }
        }
        Action::BreakPlay | Action::BreakStop | Action::BreakSkip => {
            use operator::BreakCommand;
            let command = match req.action { Action::BreakPlay => BreakCommand::Play, Action::BreakStop => BreakCommand::Stop, _ => BreakCommand::Skip };
            let revision = view["break_status"]["revision"].as_str().ok_or_else(||conflict("Break-Wave is unavailable"))?;
            operator::Action::BreakControl {command,track:None,revision:revision.to_owned()}
        }
    };
    drop(commands);
    let result = desk.player.operator(target, Grant::new(&desk, station)).await;
    let mut commands = desk.commands.lock().await;
    if let Err(error) = result {
        // A failed Next may already have completed the outgoing turn.
        commands.requests.insert(request_key, (Instant::now(), Err(error.clone())));
        return Err(conflict(error));
    }
    if matches!(req.action, Action::Next) {
        commands.next_from = Some(current_turn(&view));
    }
    commands.requests.insert(request_key, (Instant::now(), Ok(())));
    commands.pending = Some((revision, now));
    Ok(StatusCode::ACCEPTED)
}

#[cfg(test)]
mod station_control_tests {
    use super::*;

    #[test]
    fn a_snapshot_after_a_gap_retires_armed_and_unarmed_requests() {
        let mut op = Operator {
            live: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            previous: Default::default(),
            id: Uuid::new_v4(),
            armed_epoch: None,
            arm_generation: 0,
            last_snapshot: Instant::now(),
        };
        for next in [Some("desk".to_owned()), None] {
            op.set_armed_epoch(next);
            let old = op.control_epoch("desk");
            op.observe_snapshot();
            assert_eq!(op.control_epoch("desk"), old);
            op.last_snapshot = Instant::now() - CONTROL_FRESHNESS;
            op.observe_snapshot();
            assert!(op.armed_epoch.is_none());
            assert_ne!(op.control_epoch("desk"), old);
        }
    }
}
