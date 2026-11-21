//! Single-socket Behringer XR12 control bridge. Set AUTOKJ's operator policy
//! at the call site. The device protocol itself has no login or request IDs.
//! Vendor protocol: https://mediadl.musictribe.com/download/software/behringer/XAIR/X%20AIR%20Remote%20Control%20Protocol.pdf
//! Parameter list: https://raw.githubusercontent.com/Jbithell/xr-meters/master/parameters.txt
pub mod analyzer;
pub mod calibration;
pub mod effects;
mod osc;
mod parameters;
pub(crate) mod preamp;

use osc::{message, Arg};
use parameters::{Catalog, Kind, Parameter};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    fmt, io,
    net::{Ipv4Addr, SocketAddrV4},
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::UdpSocket,
    sync::{broadcast, mpsc, oneshot, RwLock},
    time::{self, Instant},
};
use uuid::Uuid;

const IDENTITY_TTL: Duration = Duration::from_secs(4);
const VALUE_TTL: Duration = Duration::from_secs(20);
const METER_TTL: Duration = Duration::from_millis(450);
const READBACK_WAIT: Duration = Duration::from_millis(800);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const POLL_BATCH: usize = 4;
const RECOVERY_PROBE_INTERVAL: Duration = Duration::from_millis(750);

#[derive(Clone, Debug, serde::Deserialize, Serialize, PartialEq, Eq)]
pub struct MixerIdentity {
    pub address: Ipv4Addr,
    pub name: String,
    pub model: String,
    pub firmware: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Observed {
    pub value: Value,
    pub age_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct MixerSnapshot {
    pub analyzer: Option<analyzer::Reading>,
    pub frequency_map: Option<Vec<calibration::BandRange>>,
    pub connected: bool,
    pub epoch: String,
    pub identity: Option<MixerIdentity>,
    pub values: BTreeMap<String, Observed>,
    pub send_enabled: BTreeMap<String, SendAssignment>,
    pub effects: BTreeMap<u8, &'static effects::Effect>,
    /// X AIR bank numbers are preserved; age applies to the entire frame.
    pub meters: BTreeMap<String, Observed>,
}

#[derive(Debug, Serialize)]
pub struct SendAssignment {
    pub enabled: bool,
    pub can_enable: bool,
    pub level: f64,
    pub mode: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MixerError {
    Offline,
    StaleSession,
    Unavailable,
    InvalidValue,
    Busy,
    ReadbackTimeout,
}

impl fmt::Display for MixerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Offline => "XR12 is offline",
            Self::StaleSession => "Mixer session has changed; refresh the console",
            Self::Unavailable => "Mixer control is unavailable or its observation expired",
            Self::InvalidValue => {
                "Mixer value is outside the supported range or has the wrong type"
            }
            Self::Busy => "Mixer control is busy",
            Self::ReadbackTimeout => "Mixer did not confirm the requested value in time",
        })
    }
}
impl std::error::Error for MixerError {}

type Samples = BTreeMap<String, (Value, Instant)>;
struct State {
    detector: Option<analyzer::Detector>,
    identity: Option<MixerIdentity>,
    heartbeat: Option<Instant>,
    epoch: String,
    values: Samples,
    meters: Samples,
    saved_levels: BTreeMap<String, Value>,
}
impl State {
    fn analyzer_rate(&self) -> Option<u32> {
        let (value, when) = self.values.get("/-prefs/clockrate")?;
        if when.elapsed() >= Duration::from_millis(1600) {
            return None;
        }
        match value.as_u64()? {
            0 => Some(48_000),
            1 => Some(44_100),
            _ => None,
        }
    }
    fn analyzer_context(&self) -> Option<(u8, bool)> {
        let fresh = |key: &str| {
            self.values
                .get(key)
                .filter(|(_, t)| t.elapsed() < Duration::from_millis(1600))
                .and_then(|(v, _)| v.as_u64())
        };
        Some((
            fresh("/-stat/rta/source")? as u8,
            fresh("/-stat/rta/pos")? == 1,
        ))
    }
    fn effect(&self, slot: u8) -> Option<&'static effects::Effect> {
        let id = self.fresh(&format!("/fx/{slot}/type"))?.as_u64()? as usize;
        effects::ALGORITHMS.get(id)
    }
    fn parameter(&self, catalog: &Catalog, key: &str) -> Option<Parameter> {
        let mut param = *catalog.map.get(key)?;
        if preamp::input_from_key(key).is_some_and(|input| !self.has_preamp(input)) {
            return None;
        }
        if let Some((slot, index)) = effects::key(key) {
            let field = self.effect(slot)?.parameters.get(usize::from(index) - 1)?;
            param.kind = Kind::Steps(field.steps.unsigned_abs());
        }
        Some(param)
    }

    fn empty() -> Self {
        Self {
            detector: None,
            identity: None,
            heartbeat: None,
            epoch: Uuid::new_v4().to_string(),
            values: BTreeMap::new(),
            meters: BTreeMap::new(),
            saved_levels: BTreeMap::new(),
        }
    }
    fn connected(&self) -> bool {
        self.identity.is_some() && self.heartbeat.is_some_and(|t| t.elapsed() < IDENTITY_TTL)
    }
}
impl State {
    fn fresh(&self, key: &str) -> Option<&Value> {
        self.values
            .get(key)
            .filter(|(_, t)| t.elapsed() < VALUE_TTL)
            .map(|(v, _)| v)
    }

    fn send(&self, level_key: &str) -> Option<SendAssignment> {
        let prefix = level_key.strip_suffix("/level")?;
        let level = self.fresh(level_key)?.as_f64()?;
        let tap = self.fresh(&format!("{prefix}/tap"))?.as_i64()?;
        if tap == 5 {
            let on = self.fresh(&format!("{prefix}/grpon"))?.as_i64()? == 1;
            return Some(SendAssignment {
                enabled: on,
                can_enable: true,
                level,
                mode: "group",
            });
        }
        let saved = self.saved_levels.get(level_key).and_then(Value::as_f64);
        Some(SendAssignment {
            enabled: level > 0.0,
            can_enable: level > 0.0 || saved.is_some(),
            level: if level > 0.0 {
                level
            } else {
                saved.unwrap_or(0.0)
            },
            mode: "level",
        })
    }
}

enum Intent {
    Value,
    Assign(bool),
    FxParameter { slot: u8, algorithm: u8 },
    Compare(Value),
    Preamp(preamp::Edit),
}

/// Dispatch one nonblocking datagram under the station's current authority.
/// False means that the station was disarmed or revoked.
pub(crate) type WireWriter = Arc<dyn Fn(&UdpSocket, &[u8]) -> io::Result<bool> + Send + Sync>;

struct Write {
    writer: Option<WireWriter>,
    key: String,
    intent: Intent,
    value: Value,
    epoch: String,
    reply: oneshot::Sender<Result<Value, MixerError>>,
    deadline: Instant,
}

#[derive(Clone)]
pub struct Mixer {
    calibration: Arc<std::sync::OnceLock<calibration::Profile>>,
    writer: Option<WireWriter>,
    state: Arc<RwLock<State>>,
    requests: mpsc::Sender<Write>,
    analyzer_frames: broadcast::Sender<analyzer::BenchFrame>,
}

impl Mixer {
    /// Opening a socket does not imply hardware presence. Inspect the snapshot.
    pub async fn connect(address: Ipv4Addr) -> io::Result<Self> {
        if address.is_unspecified() || address.is_broadcast() || address.is_multicast() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Expected a unicast XR12 address",
            ));
        }
        Self::start(SocketAddrV4::new(address, 10024)).await
    }

    async fn start(peer: SocketAddrV4) -> io::Result<Self> {
        let socket = open(peer).await?;
        let (requests, rx) = mpsc::channel(32);
        let state = Arc::new(RwLock::new(State::empty()));
        let (analyzer_frames, _) = broadcast::channel(64);
        tokio::spawn(serve(
            peer,
            socket,
            state.clone(),
            rx,
            analyzer_frames.clone(),
        ));
        Ok(Self {
            calibration: Default::default(),
            writer: None,
            state,
            requests,
            analyzer_frames,
        })
    }

    pub(crate) fn with_host_calibration(self) -> Self {
        if let Some(path) = std::env::var_os("AUTOKJ_XR12_RTA_CALIBRATION") {
            let profile = self.calibration.clone();
            tokio::task::spawn_blocking(move || {
                match calibration::Profile::load(std::path::Path::new(&path)) {
                    Ok(measured) => {
                        let _ = profile.set(measured);
                    }
                    Err(e) => tracing::warn!("XR12 RTA calibration unavailable: {e}"),
                }
            });
        }
        self
    }

    pub(crate) fn with_writer(&self, writer: WireWriter) -> Self {
        Self { writer: Some(writer), ..self.clone() }
    }

    /// Raw received spectra for the read-only calibration recorder. A lagged
    /// receiver must report its own lost frames. No source selection is sent.
    pub fn subscribe_analyzer(&self) -> broadcast::Receiver<analyzer::BenchFrame> {
        self.analyzer_frames.subscribe()
    }

    pub async fn meters(&self) -> (String, BTreeMap<String, Observed>) {
        let state = self.state.read().await;
        (
            state.epoch.clone(),
            if state.connected() {
                state
                    .meters
                    .iter()
                    .filter_map(|(key, (v, t))| {
                        let age = t.elapsed();
                        (age < METER_TTL).then(|| {
                            (
                                key.clone(),
                                Observed {
                                    value: v.clone(),
                                    age_ms: age.as_millis() as u64,
                                },
                            )
                        })
                    })
                    .collect()
            } else {
                BTreeMap::new()
            },
        )
    }

    pub async fn snapshot(&self) -> MixerSnapshot {
        let state = self.state.read().await;
        let connected = state.connected();
        let observed = |samples: &Samples, ttl: Duration| {
            samples
                .iter()
                .filter_map(|(key, (value, when))| {
                    let age = when.elapsed();
                    (connected
                        && age < ttl
                        && preamp::input_from_key(key).is_none_or(|input| state.has_preamp(input)))
                    .then(|| {
                        (
                            key.clone(),
                            Observed {
                                value: value.clone(),
                                age_ms: age.as_millis() as u64,
                            },
                        )
                    })
                })
                .collect()
        };
        let analyzer = state
            .analyzer_context()
            .filter(|_| connected)
            .and_then(|(source, post)| state.detector.as_ref()?.reading(source, post));
        let frequency_map = analyzer.as_ref().and_then(|reading| {
            self.calibration.get()?.ranges_for(
                state.identity.as_ref()?,
                reading.source,
                reading.post,
                state.analyzer_rate(),
                calibration::now_ms(),
            ).map(<[_]>::to_vec)
        });
        MixerSnapshot {
            analyzer,
            frequency_map,
            effects: (1..=4)
                .filter_map(|slot| {
                    let effect = state.effect(slot)?;
                    connected.then_some((slot, effect))
                })
                .collect(),
            connected,
            epoch: state.epoch.clone(),
            identity: state.identity.clone().filter(|_| connected),
            values: observed(&state.values, VALUE_TTL),
            send_enabled: if connected {
                state
                    .values
                    .keys()
                    .filter_map(|key| state.send(key).map(|send| (key.clone(), send)))
                    .collect()
            } else {
                BTreeMap::new()
            },
            meters: observed(&state.meters, METER_TTL),
        }
    }

    /// Single absolute edit. No queue survives a lost connection or caller.
    /// Acknowledgement is the device's observed (possibly quantized) value.
    pub async fn set(&self, key: &str, value: Value, epoch: &str) -> Result<Value, MixerError> {
        self.request(key, value, epoch, Intent::Value).await
    }

    pub async fn set_send_enabled(
        &self,
        level_key: &str,
        enabled: bool,
        epoch: &str,
    ) -> Result<Value, MixerError> {
        self.request(level_key, Value::Null, epoch, Intent::Assign(enabled))
            .await
    }

    pub async fn set_fx_parameter(
        &self,
        slot: u8,
        algorithm: u8,
        index: u8,
        value: Value,
        epoch: &str,
    ) -> Result<Value, MixerError> {
        if !(1..=4).contains(&slot)
            || index == 0
            || usize::from(index) > effects::fx_parameters(algorithm).len()
        {
            return Err(MixerError::Unavailable);
        }
        self.request(
            &format!("/fx/{slot}/par/{index:02}"),
            value,
            epoch,
            Intent::FxParameter { slot, algorithm },
        )
        .await
    }

    /// Fine level steps, algorithm replacement and insert configuration are compare-and-set.
    /// Only the small allowlist in the bridge can execute this operation.
    pub async fn compare_and_set(
        &self,
        key: &str,
        expected: Value,
        value: Value,
        epoch: &str,
    ) -> Result<Value, MixerError> {
        self.request(key, value, epoch, Intent::Compare(expected))
            .await
    }

    async fn request(
        &self,
        key: &str,
        value: Value,
        epoch: &str,
        intent: Intent,
    ) -> Result<Value, MixerError> {
        if key.len() > 96 || epoch.len() > 64 {
            return Err(MixerError::Unavailable);
        }
        let (reply, rx) = oneshot::channel();
        let deadline = Instant::now() + READBACK_WAIT;
        self.requests
            .try_send(Write {
                writer: self.writer.clone(),
                key: key.to_owned(),
                intent,
                value,
                epoch: epoch.to_owned(),
                reply,
                deadline,
            })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => MixerError::Busy,
                mpsc::error::TrySendError::Closed(_) => MixerError::Offline,
            })?;
        time::timeout_at(deadline, rx)
            .await
            .map_err(|_| MixerError::ReadbackTimeout)?
            .unwrap_or(Err(MixerError::Offline))
    }
}

async fn open(peer: SocketAddrV4) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    socket.connect(peer).await?;
    Ok(socket)
}

fn identity(args: &[Arg<'_>], peer: Ipv4Addr) -> Option<MixerIdentity> {
    let [Arg::Text(ip), Arg::Text(name), Arg::Text(model), Arg::Text(firmware), ..] = args else {
        return None;
    };
    let address: Ipv4Addr = ip.parse().ok()?;
    if address != peer
        || !model.eq_ignore_ascii_case("XR12")
        || name.len() > 128
        || firmware.is_empty()
        || firmware.len() > 64
        || name.chars().chain(firmware.chars()).any(char::is_control)
    {
        return None;
    }
    Some(MixerIdentity {
        address,
        name: (*name).into(),
        model: (*model).into(),
        firmware: (*firmware).into(),
    })
}

async fn serve(
    peer: SocketAddrV4,
    mut socket: UdpSocket,
    state: Arc<RwLock<State>>,
    mut rx: mpsc::Receiver<Write>,
    frames: broadcast::Sender<analyzer::BenchFrame>,
) {
    let catalog = Catalog::new();
    loop {
        if !session(&socket, *peer.ip(), &state, &catalog, &mut rx, &frames).await {
            break;
        }
        *state.write().await = State::empty();
        // Rebind with the previous socket still open so delayed replies from
        // a disconnected session cannot be received on the next session's port.
        loop {
            tokio::select! {
                incoming=rx.recv()=>match incoming {Some(w)=>{let _=w.reply.send(Err(MixerError::Offline));},None=>return},
                _=time::sleep(Duration::from_millis(300))=>{
                    if let Ok(new_socket)=open(peer).await{socket=new_socket;break;}
                }
            }
        }
    }
}

/// Assignments first query the live parameter. A zero send which the bridge has
/// never cut has no known restore value. A scene change resets the whole epoch.
enum Phase {
    InspectAssignment,
    Check {
        expected: Value,
        remaining: VecDeque<(String, Value)>,
    },
    Confirm {
        target: Value,
        remember: Option<Value>,
    },
}

struct Pending {
    write: Write,
    phase: Phase,
}

struct SendRecovery {
    amount: Value,
    expires: Instant,
    next_probe: Instant,
}

fn sweep_time(catalog: &Catalog) -> Duration {
    POLL_INTERVAL * catalog.order.len().div_ceil(POLL_BATCH) as u32
}

struct Session<'a> {
    frames: &'a broadcast::Sender<analyzer::BenchFrame>,
    started: Instant,
    socket: &'a UdpSocket,
    peer: Ipv4Addr,
    state: &'a RwLock<State>,
    catalog: &'a Catalog,
    pending: BTreeMap<String, Pending>,
    // A silence command may have reached the mixer even if its reply was lost.
    // Keep the original amount only until a fresh same-session read resolves it.
    recoveries: BTreeMap<String, SendRecovery>,
    next_hello: Instant,
    next_sub: Instant,
    cursor: usize,
}

async fn session(
    socket: &UdpSocket,
    peer: Ipv4Addr,
    state: &RwLock<State>,
    catalog: &Catalog,
    rx: &mut mpsc::Receiver<Write>,
    frames: &broadcast::Sender<analyzer::BenchFrame>,
) -> bool {
    let mut bridge = Session {
        frames,
        started: Instant::now(),
        socket,
        peer,
        state,
        catalog,
        pending: BTreeMap::new(),
        recoveries: BTreeMap::new(),
        next_hello: Instant::now(),
        next_sub: Instant::now(),
        cursor: 0,
    };
    let mut poll = time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut bytes = [0; 4097];
    loop {
        let result = tokio::select! {
            _=poll.tick()=>bridge.tick().await,
            packet=socket.recv(&mut bytes)=> match packet {
                Ok(n) if n < bytes.len()=>bridge.receive(&bytes[..n]).await,
                Ok(_)=>Ok(()),
                Err(e)=>Err(e),
            },
            request=rx.recv()=>match request {
                Some(write)=>bridge.write(write).await,
                None=>return false,
            }
        };
        if result.is_err() {
            return true;
        }
    }
}

impl Session<'_> {
    async fn send(&self, key: &str, args: &[Arg<'_>]) -> io::Result<()> {
        self.socket.send(&message(key, args)).await?;
        Ok(())
    }

    async fn send_edit(&self, w: &Write, key: &str, arg: Arg<'_>) -> io::Result<bool> {
        let packet = message(key, &[arg]);
        loop {
            self.socket.writable().await?;
            if w.reply.is_closed() || Instant::now() >= w.deadline { return Ok(false); }
            let result = match &w.writer {
                Some(send) => send(self.socket, &packet),
                None => self.socket.try_send(&packet).map(|_| true),
            };
            match result {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                other => return other,
            }
        }
    }

    async fn tick(&mut self) -> io::Result<()> {
        let now = Instant::now();
        self.recoveries.retain(|_, record| now < record.expires);
        let connected = self.state.read().await.connected();
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, p)| p.write.reply.is_closed() || now >= p.write.deadline)
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            if let Some(p) = self.pending.remove(&key) {
                if let Phase::Confirm {
                    remember: Some(amount),
                    ..
                } = p.phase
                {
                    if connected {
                        // Allow two normal sweeps for intermittent loss, including
                        // when this path has only just passed the polling cursor.
                        self.recoveries.insert(
                            key.clone(),
                            SendRecovery {
                                amount,
                                expires: now + 2 * sweep_time(self.catalog) + READBACK_WAIT,
                                next_probe: now,
                            },
                        );
                    }
                }
                let _ = p.write.reply.send(Err(MixerError::ReadbackTimeout));
            }
        }
        let state = self.state.read().await;
        let connected = state.connected();
        if state.identity.is_some() && !connected {
            return Err(io::Error::other("XR12 heartbeat expired"));
        }
        drop(state);
        if now >= self.next_hello {
            self.send("/xinfo", &[]).await?;
            if connected {
                self.send("/-stat/rta/source", &[]).await?;
                self.send("/-stat/rta/pos", &[]).await?;
                self.send("/-prefs/clockrate", &[]).await?;
            }
            self.next_hello = now + Duration::from_secs(1);
        }
        if !connected {
            return Ok(());
        }
        let probes: Vec<_> = self
            .recoveries
            .iter_mut()
            .filter(|(_, r)| now >= r.next_probe)
            .take(POLL_BATCH)
            .map(|(key, r)| {
                r.next_probe = now + RECOVERY_PROBE_INTERVAL;
                key.clone()
            })
            .collect();
        for key in probes {
            self.send(&key, &[]).await?;
        }
        if now >= self.next_sub {
            self.send("/xremote", &[]).await?;
            for bank in ["/meters/1", "/meters/4", "/meters/6"] {
                self.send("/meters", &[Arg::Text(bank)]).await?;
            }
            self.next_sub = now + Duration::from_secs(5);
        }
        for _ in 0..POLL_BATCH {
            let key = &self.catalog.order[self.cursor];
            self.cursor = (self.cursor + 1) % self.catalog.order.len();
            if let Some(input) = preamp::input_from_key(key) {
                if !self.state.read().await.has_preamp(input) {
                    continue;
                }
            }
            if !self.pending.contains_key(key) {
                self.send(key, &[]).await?;
            }
        }
        Ok(())
    }

    async fn receive(&mut self, packet: &[u8]) -> io::Result<()> {
        let Some((key, args)) = osc::parse(packet) else {
            return Ok(());
        };
        let mut state = self.state.write().await;
        if key == "/xinfo" {
            let new_identity = identity(&args, self.peer);
            if state.identity.is_some() && (new_identity != state.identity || !state.connected()) {
                return Err(io::Error::other("XR12 identity changed"));
            }
            if let Some(id) = new_identity {
                state.identity = Some(id);
                state.heartbeat = Some(Instant::now());
            }
            return Ok(());
        }
        if !state.connected() {
            return Ok(());
        }
        let [arg] = args.as_slice() else {
            return Ok(());
        };
        if let Arg::Blob(blob) = arg {
            let count = match key {
                "/meters/1" => 40,
                "/meters/4" => 100,
                "/meters/6" => 39,
                _ => return Ok(()),
            };
            if let Some(levels) = osc::meter(blob, count) {
                if key == "/meters/4" {
                    let Ok(bins) = levels.as_slice().try_into() else {
                        return Ok(());
                    };
                    let now = Instant::now();
                    if let Some((source, post)) = state.analyzer_context() {
                        if let Some(detector) = state.detector.as_mut() {
                            detector.update(source, post, bins, now);
                        } else {
                            state.detector = Some(analyzer::Detector::new(source, post, bins, now));
                        }
                    } else {
                        state.detector = None;
                    }
                    if self.frames.receiver_count() > 0 {
                        if let Some(id) = &state.identity {
                            let ctx = state.analyzer_context();
                            let _ = self.frames.send(analyzer::BenchFrame {
                                received_at: now,
                                epoch: state.epoch.clone(),
                                identity: id.clone(),
                                received_us: now.duration_since(self.started).as_micros() as u64,
                                source: ctx.map(|p| p.0),
                                post: ctx.map(|p| p.1),
                                sample_rate_hz: state.analyzer_rate(),
                                bins_db: levels.clone(),
                                reading: ctx
                                    .and_then(|(s, p)| state.detector.as_ref()?.reading(s, p)),
                            });
                        }
                    }
                }
                state
                    .meters
                    .insert(key.into(), (Value::from(levels), Instant::now()));
            }
            return Ok(());
        }
        let Some(param) = state.parameter(self.catalog, key) else {
            return Ok(());
        };
        let Some(value) = param.observed(arg) else {
            return Ok(());
        };
        let previous = state
            .values
            .insert(key.into(), (value.clone(), Instant::now()));
        let changed = previous.as_ref().is_some_and(|(old, _)| old != &value);
        if changed && (key.ends_with("/config/insrc") || preamp::input_from_key(key).is_some()) {
            // Retire a gesture even if another operator subsequently puts the
            // patch or control back. Once dispatched, await actual pin readback.
            let interrupted = self.pending.extract_if(.., |_, pending| {
                matches!(&pending.write.intent, Intent::Preamp(edit) if edit.map==key ||
                    (pending.write.key == key && matches!(pending.phase, Phase::Check{..})))
            });
            for (_, pending) in interrupted {
                let _ = pending.write.reply.send(Err(MixerError::Unavailable));
            }
        }
        if changed && key.ends_with("/config/insrc") {
            if let Some(input) = previous
                .as_ref()
                .and_then(|(v, _)| v.as_u64())
                .filter(|&n| n < 4)
                .map(|n| n as u8 + 1)
            {
                if !state.has_preamp(input) {
                    state.values.remove(&format!("/headamp/{input:02}/gain"));
                    state.values.remove(&format!("/headamp/{input:02}/phantom"));
                }
            }
        }
        if changed && matches!(key, "/-stat/rta/source" | "/-stat/rta/pos" | "/-prefs/clockrate")
        {
            state.detector = None;
            state.meters.remove("/meters/4");
        }
        if matches!(
            key,
            "/-snap/index" | "/-snap/name" | "/snap/index" | "/snap/name"
        ) && changed
        {
            return Err(io::Error::other("XR12 scene changed"));
        }
        if let Some(prefix) = key.strip_suffix("/tap") {
            if changed {
                state.saved_levels.remove(&format!("{prefix}/level"));
                self.recoveries.remove(&format!("{prefix}/level"));
                for suffix in ["level", "grpon"] {
                    if let Some(p) = self.pending.remove(&format!("{prefix}/{suffix}")) {
                        let _ = p.write.reply.send(Err(MixerError::Unavailable));
                    }
                }
            }
        }
        let changed_algorithm = key.starts_with("/fx/") && key.ends_with("/type") && changed;
        if changed_algorithm {
            // A delayed reply for the previous algorithm could otherwise update a
            // parameter with a completely different meaning under the new algorithm.
            if let Some(p) = self.pending.remove(key) {
                let answer = if matches!(p.write.intent, Intent::Compare(_))
                    && matches!(&p.phase,Phase::Confirm{target,..} if target==&value)
                {
                    Ok(value)
                } else {
                    Err(MixerError::StaleSession)
                };
                let _ = p.write.reply.send(answer);
            }
            return Err(io::Error::other("XR12 effect algorithm changed"));
        }
        if value.as_f64().is_some_and(|v| v > 0.) {
            state.saved_levels.remove(key);
        }
        if let Some(record) = self.recoveries.remove(key) {
            if Instant::now() < record.expires
                && value.as_f64() == Some(0.0)
                && allows_fx(&state, key)
            {
                state.saved_levels.insert(key.into(), record.amount);
            }
        }
        let Some(mut pending) = self.pending.remove(key) else {
            return Ok(());
        };
        if pending.write.reply.is_closed() || Instant::now() >= pending.write.deadline {
            if let Phase::Confirm {
                remember: Some(amount),
                ..
            } = pending.phase
            {
                if value.as_f64() == Some(0.0) && allows_fx(&state, key) {
                    state.saved_levels.insert(key.into(), amount);
                }
            }
            let _ = pending.write.reply.send(Err(MixerError::ReadbackTimeout));
            return Ok(());
        }
        match &mut pending.phase {
            Phase::Confirm { target, remember } => {
                if !param.matches(target, &value) {
                    if remember
                        .as_ref()
                        .is_some_and(|prior| !param.matches(prior, &value))
                    {
                        let _ = pending.write.reply.send(Err(MixerError::Unavailable));
                        return Ok(());
                    }
                    self.pending.insert(key.into(), pending);
                    return Ok(());
                }
                if let Some(saved) = remember {
                    state.saved_levels.insert(key.into(), saved.clone());
                }
                let _ = pending.write.reply.send(Ok(value));
            }
            Phase::Check {
                expected,
                remaining,
            } => {
                let w = &pending.write;
                if *expected != value || !edit_context(&state, w) {
                    let _ = pending.write.reply.send(Err(MixerError::Unavailable));
                    return Ok(());
                }
                if let Some((next, check)) = remaining.pop_front() {
                    if self.pending.contains_key(&next) {
                        let _ = pending.write.reply.send(Err(MixerError::Busy));
                        return Ok(());
                    }
                    *expected = check;
                    drop(state);
                    self.send(&next, &[]).await?;
                    self.pending.insert(next, pending);
                    return Ok(());
                }
                let Some(mut field) = state.parameter(self.catalog, &w.key) else {
                    let _ = pending.write.reply.send(Err(MixerError::Unavailable));
                    return Ok(());
                };
                field.writable = true;
                let Some(arg) = field.encode(&w.value) else {
                    let _ = pending.write.reply.send(Err(MixerError::InvalidValue));
                    return Ok(());
                };
                drop(state);
                if !self.send_edit(w, &w.key, arg).await? {
                    let _ = pending.write.reply.send(Err(MixerError::Unavailable));
                    return Ok(());
                }
                self.send(&w.key, &[]).await?;
                pending.phase = Phase::Confirm {
                    target: w.value.clone(),
                    remember: None,
                };
                self.pending.insert(w.key.clone(), pending);
            }
            Phase::InspectAssignment => {
                let w = &pending.write;
                let Intent::Assign(enabled) = w.intent else {
                    return Err(io::Error::other("invalid bridge state"));
                };
                let Some(assignment) = state.send(&w.key) else {
                    let _ = pending.write.reply.send(Err(MixerError::Unavailable));
                    return Ok(());
                };
                let native = assignment.mode == "group";
                if native != key.ends_with("/grpon")
                    || !allows_fx(&state, &w.key)
                    || (enabled && !assignment.can_enable)
                {
                    let _ = pending.write.reply.send(Err(MixerError::Unavailable));
                    return Ok(());
                }
                if enabled == assignment.enabled {
                    let _ = pending.write.reply.send(Ok(value));
                    return Ok(());
                }
                let (target, remember) = if native {
                    (Value::from(i32::from(enabled)), None)
                } else if enabled {
                    (Value::from(assignment.level), None)
                } else {
                    (Value::from(0.0), Some(value))
                };
                let arg = if native {
                    Arg::Int(i32::from(enabled))
                } else {
                    let Some(arg) = param.encode(&target) else {
                        let _ = pending.write.reply.send(Err(MixerError::InvalidValue));
                        return Ok(());
                    };
                    arg
                };
                drop(state);
                if !self.send_edit(w, key, arg).await? {
                    let _ = pending.write.reply.send(Err(MixerError::Unavailable));
                    return Ok(());
                }
                self.send(key, &[]).await?;
                pending.phase = Phase::Confirm { target, remember };
                self.pending.insert(key.into(), pending);
            }
        }
        Ok(())
    }

    async fn write(&mut self, w: Write) -> io::Result<()> {
        if w.reply.is_closed() || w.deadline <= Instant::now() {
            return Ok(());
        }
        let state = self.state.read().await;
        let ready = self.prepare(&state, &w);
        drop(state);
        let (key, arg, phase) = match ready {
            Ok(work) => work,
            Err(e) => {
                let _ = w.reply.send(Err(e));
                return Ok(());
            }
        };
        if self.pending.contains_key(&key) {
            let _ = w.reply.send(Err(MixerError::Busy));
            return Ok(());
        }
        if let Some(arg) = arg {
            if !self.send_edit(&w, &key, arg).await? {
                let _ = w.reply.send(Err(MixerError::Unavailable));
                return Ok(());
            }
        }
        self.send(&key, &[]).await?;
        self.pending.insert(key, Pending { write: w, phase });
        Ok(())
    }

    fn prepare(
        &self,
        state: &State,
        w: &Write,
    ) -> Result<(String, Option<Arg<'static>>, Phase), MixerError> {
        if !state.connected() {
            return Err(MixerError::Offline);
        }
        if state.epoch != w.epoch {
            return Err(MixerError::StaleSession);
        }
        if self.recoveries.contains_key(&w.key)
            || self.pending.values().any(|p| p.write.key == w.key)
        {
            return Err(MixerError::Busy);
        }
        if state.fresh(&w.key).is_none() || !allows_fx(state, &w.key) {
            return Err(MixerError::Unavailable);
        }
        let param = self
            .catalog
            .map
            .get(&w.key)
            .ok_or(MixerError::Unavailable)?;
        match &w.intent {
            Intent::Preamp(edit) => {
                if !edit.valid(state, &w.key) {
                    return Err(MixerError::Unavailable);
                }
                let selection = Value::from(edit.input - 1);
                let remaining = VecDeque::from([
                    (w.key.clone(), edit.expected.clone()),
                    (edit.map.clone(), selection.clone()),
                ]);
                Ok((
                    edit.map.clone(),
                    None,
                    Phase::Check {
                        expected: selection,
                        remaining,
                    },
                ))
            }
            Intent::FxParameter { slot, algorithm } => {
                if state
                    .effect(*slot)
                    .is_none_or(|e| e.algorithm != *algorithm)
                {
                    return Err(MixerError::Unavailable);
                }
                Ok((
                    format!("/fx/{slot}/type"),
                    None,
                    Phase::Check {
                        expected: Value::from(*algorithm),
                        remaining: VecDeque::new(),
                    },
                ))
            }
            Intent::Compare(expected) => {
                if !(is_deliberate_edit(&w.key) || (param.writable && is_mix_level(&w.key)))
                    || !edit_context(state, w) {
                    return Err(MixerError::Unavailable);
                }
                if is_mix_level(&w.key) {
                    let expected = param.encode(expected).and_then(|arg| param.observed(&arg))
                        .ok_or(MixerError::InvalidValue)?;
                    return Ok((w.key.clone(), None, Phase::Check { expected, remaining: VecDeque::new() }));
                }
                if let Some(root) = w
                    .key
                    .strip_suffix("/insert/on")
                    .filter(|_| w.value.as_i64() == Some(1))
                {
                    let raw = expected["fxslot"]
                        .as_u64()
                        .ok_or(MixerError::InvalidValue)?;
                    let slot = if root == "/lr" { raw } else { raw.div_ceil(2) };
                    let remaining = VecDeque::from([
                        (format!("/fx/{slot}/type"), expected["algorithm"].clone()),
                        (format!("/fx/{slot}/insert"), Value::from(1)),
                        (w.key.clone(), expected["value"].clone()),
                    ]);
                    return Ok((
                        format!("{root}/insert/fxslot"),
                        None,
                        Phase::Check {
                            expected: Value::from(raw),
                            remaining,
                        },
                    ));
                }
                Ok((
                    w.key.clone(),
                    None,
                    Phase::Check {
                        expected: expected.clone(),
                        remaining: VecDeque::new(),
                    },
                ))
            }
            Intent::Value => {
                if !edit_context(state, w) {
                    return Err(MixerError::Unavailable);
                }
                if !param.writable || state.saved_levels.contains_key(&w.key) {
                    return Err(MixerError::Unavailable);
                }
                let arg = param.encode(&w.value).ok_or(MixerError::InvalidValue)?;
                Ok((
                    w.key.clone(),
                    Some(arg),
                    Phase::Confirm {
                        target: w.value.clone(),
                        remember: None,
                    },
                ))
            }
            Intent::Assign(_) => {
                let current = state.send(&w.key).ok_or(MixerError::Unavailable)?;
                let key = if current.mode == "group" {
                    format!(
                        "{}/grpon",
                        w.key
                            .strip_suffix("/level")
                            .ok_or(MixerError::Unavailable)?
                    )
                } else {
                    w.key.clone()
                };
                Ok((key, None, Phase::InspectAssignment))
            }
        }
    }
}

/// An insert engine does not have a normal send/return signal path.
fn is_deliberate_edit(key: &str) -> bool {
    (key.starts_with("/fx/") && (key.ends_with("/type") || key.ends_with("/insert")))
        || key.ends_with("/insert/fxslot")
        || key.ends_with("/insert/on")
}

fn is_mix_level(key: &str) -> bool {
    key.ends_with("/mix/fader") || (key.contains("/mix/") && key.ends_with("/level"))
}

fn edit_context(state: &State, w: &Write) -> bool {
    if let Intent::Preamp(edit) = &w.intent {
        return edit.valid(state, &w.key);
    }
    let key = &w.key;
    let value = &w.value;
    if is_mix_level(key) {
        return !state.saved_levels.contains_key(key) && allows_fx(state, key);
    }
    if let Some((root, suffix)) = key.split_once("/eq/") {
        if (root == "/lr" || root.starts_with("/bus/")) && suffix.contains('/') {
            return state
                .fresh(&format!("{root}/eq/mode"))
                .and_then(Value::as_i64)
                == Some(0);
        }
    }
    if let Some(root) = key.strip_suffix("/insert/fxslot") {
        if state
            .fresh(&format!("{root}/insert/on"))
            .and_then(Value::as_i64)
            != Some(0)
        {
            return false;
        }
    }
    if let Some(root) = key.strip_suffix("/insert/on") {
        if value.as_i64() == Some(1) {
            let Some(slot) = state
                .fresh(&format!("{root}/insert/fxslot"))
                .and_then(Value::as_u64)
                .filter(|&n| n > 0)
            else {
                return false;
            };
            let Intent::Compare(expected) = &w.intent else {
                return false;
            };
            if expected.get("fxslot").and_then(Value::as_u64) != Some(slot)
                || expected.get("value").and_then(Value::as_i64) != Some(0)
            {
                return false;
            }
            let slot = if root == "/lr" {
                slot
            } else {
                slot.div_ceil(2)
            };
            if expected.get("algorithm").and_then(Value::as_u64)
                != state.effect(slot as u8).map(|e| u64::from(e.algorithm))
                || state.effect(slot as u8).is_none()
            {
                return false;
            }
            return state
                .fresh(&format!("/fx/{slot}/insert"))
                .and_then(Value::as_i64)
                == Some(1);
        }
    }
    true
}

fn allows_fx(state: &State, key: &str) -> bool {
    let mut parts = key.trim_start_matches('/').split('/');
    let family = parts.next();
    let slot = match family {
        Some("rtn" | "fxsend") => parts.next().and_then(|s| s.parse::<u8>().ok()),
        Some("ch") => {
            let _channel = parts.next();
            if parts.next() != Some("mix") {
                return true;
            }
            parts
                .next()
                .and_then(|s| s.parse::<u8>().ok())
                .filter(|i| (7..=10).contains(i))
                .map(|i| i - 6)
        }
        _ => None,
    };
    slot.is_none_or(|i| {
        state
            .fresh(&format!("/fx/{i}/insert"))
            .and_then(Value::as_i64)
            == Some(0)
    })
}

#[cfg(test)]
mod tests;
