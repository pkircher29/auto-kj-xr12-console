//! Private show-operator API. Commands carry one-use tickets for the view the operator saw.
//!
//! # Host action parity
//!
//! These controls require an explicitly paired studio operator and a connected desktop
//! host. All listed controls use the local show network and local catalog offline.
//! Desktop-handler errors and license/seat refusals are returned to the operator.
//!
//! | Console page | Host implementation | Confirmation and limits |
//! |---|---|---|
//! | Rotation | `line_cmds::{get_singers, get_line_view, add_singer, rename_singer}`, `queue_ops::{move_singer, pause_singer, unpause_singer}`, `requests::delete_singer` | Read current stable singer IDs, refresh the line after each change. The host enforces moves and removal during a turn. |
//! | Singer queue | `queue_ops::{get_singer_queue, swap_queue_entries}`, `requests::{delete_request, replace_request_song}` | Scope swaps to the selected singer. Replace resolves a catalog ID on the host. Group and current-turn restrictions remain active. |
//! | Library | `library_cmds::search_songs_cmd`, `catalog::db::get_song_by_id`, `requests::{add_song_request,add_group_song_request}` | Twenty songs per page. Assign to one singer or one shared group of two to four. File paths stay on the host. A duplicate override requires an explicit decision on the operator station. |
//! | Requests | Main window `approveRequest`, `rejectRequest`, `handleToggleAccepting` | Address each card by its stable key. Confirm it left the host inbox. Existing duplicate, stage-name, group and blocklist policy applies; interactive policy exceptions can need a booth decision. |
//! | Soundboard | `soundboardAudio::{PADS, triggerSoundboardFx}` | Use the host's assigned samples and audio routing. The desktop acknowledges the pad action. |
//! | Key | `audio_cmds::audio_set_pitch`, main `handleKnobsChanged` | Absolute semitones in the host's −6 to +6 range. The audio thread checks the loaded track generation before applying and acknowledges the change. |
//! | Show settings | `line_cmds::start_new_show`, main `handleStop`, `handleToggleAccepting`, `dj::break_wave_stop` | Starting a show requires confirmation and stopped karaoke. Ending performances closes intake and stops playback, keeps the remaining line, and preserves local mixer control. |
//! | Pause / Resume | `studio::show`, native `AudioCmd::StudioPause` | The audio worker checks station authorization, playback state and loaded track before applying and acknowledging the change. |
//! | Stop / Next | `studio::show`, `queue_ops::{complete_current,start_playing}`, `audio_stop_if` | Recheck the current turn and load under lock. Next also rechecks the displayed successor before completing the current turn and again before starting from idle. A concurrent newer load is left playing. |
//! | Play | `studio::show`, main `startGuarded`, native `queue_ops::start_playing` | Starts the displayed request only when the queue is idle, the load generation matches and the originating station still owns the claim. |
//! | Break music | `dj::studio_break`, Break-Wave playlist, DJ mixer callback and running Automix executor | Start a selected playlist track, stop the observed decks with their short declick pause, or skip on Full. An existing automix accepts Skip once the next track is cued. Set creation, the configured long fade, and playlist editing remain at the desktop. |
//!
//! The desktop still owns venue/network teardown, production deployment, custom sound-pad
//! assignment, catalog maintenance, claim moderation, advanced rotation/group operations,
//! and full host configuration. Physical Windows/Linux/tablet rehearsals and per-action
//! entitlement checks are required before claiming complete live-show parity.
use super::{authenticate, conflict, Desk, Failure};
use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    BreakView {
        page: u16,
    },
    BreakControl {
        command: BreakCommand,
        track: Option<Uuid>,
        revision: String,
    },
    Overview,
    Queue {
        singer: Uuid,
    },
    Search {
        query: String,
        page: u16,
    },
    AddSinger {
        name: String,
    },
    RenameSinger {
        singer: Uuid,
        name: String,
    },
    MoveSinger {
        singer: Uuid,
        slot: u16,
        from: u16,
        target: Uuid,
    },
    RemoveSinger {
        singer: Uuid,
    },
    SetPaused {
        singer: Uuid,
        paused: bool,
    },
    Assign {
        singer: Uuid,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        partners: Vec<Uuid>,
        song: i64,
        #[serde(default)]
        override_duplicate: bool,
    },
    Replace {
        singer: Uuid,
        request: Uuid,
        song: i64,
        #[serde(default)]
        override_duplicate: bool,
    },
    Swap {
        singer: Uuid,
        first: Uuid,
        second: Uuid,
    },
    RemoveSong {
        request: Uuid,
        #[serde(default)]
        confirm_group: bool,
    },
    Review {
        key: String,
        approve: bool,
    },
    Key {
        semitones: i8,
        track: u64,
    },
    Pad {
        pad: String,
    },
    AcceptRequests {
        enabled: bool,
    },
    NewShow {
        confirmed: bool,
    },
    EndPerformances {
        confirmed: bool,
    },
    /// Constructed only after the dedicated transport endpoint checked the displayed revision.
    #[serde(skip_deserializing)]
    Transport {
        command: Transport,
        current: Option<String>,
        next: Option<String>,
        generation: u64,
        loaded: bool,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Play,
    Pause,
    Resume,
    Stop,
    Next,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum BreakCommand {
    Play,
    Stop,
    Skip,
}

impl Action {
    fn read(&self) -> bool {
        matches!(
            self,
            Self::Overview | Self::Queue { .. } | Self::Search { .. } | Self::BreakView { .. }
        )
    }
    fn valid(&self) -> bool {
        fn text(s: &str, max: usize) -> bool {
            !s.trim().is_empty() && s.chars().count() <= max && !s.chars().any(char::is_control)
        }
        match self {
            Self::BreakView { page } => (1..=1000).contains(page),
            Self::BreakControl {
                revision,
                command,
                track,
            } => {
                revision.len() == 64
                    && revision.bytes().all(|b| b.is_ascii_hexdigit())
                    && (track.is_none() || *command == BreakCommand::Play)
            }
            Self::Search { query, page } => {
                query.chars().count() <= 200
                    && !query.chars().any(char::is_control)
                    && (1..=1000).contains(page)
            }
            Self::AddSinger { name } | Self::RenameSinger { name, .. } => {
                text(name, crate::protocol::MAX_SINGER_NAME_LEN)
            }
            Self::Assign { singer, partners, song, .. } => {
                *song > 0 && partners.len() <= 3 && !partners.contains(singer)
                    && partners.iter().enumerate().all(|(i, id)| !partners[..i].contains(id))
            }
            Self::Replace { song, .. } => *song > 0,
            Self::Review { key, .. } => text(key, 128),
            Self::Pad { pad } => text(pad, 64),
            Self::Key { semitones, track } => *track != 0 && (-6..=6).contains(semitones),
            Self::NewShow { confirmed } | Self::EndPerformances { confirmed } => *confirmed,
            Self::Swap { first, second, .. } => first != second,
            _ => true,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    action: Action,
    ticket: Option<Uuid>,
}

struct Permit {
    ticket: Uuid,
    query: Action,
    observed: Value,
    expires: Instant,
}

impl Permit {
    fn covers_group_assignment(&self, singer: Uuid, partners: &[Uuid], song: i64) -> bool {
        if !matches!(self.query, Action::Search { .. }) {
            return false;
        }
        let Some(singers) = self.observed[0]["singers"].as_array() else { return false };
        let Some(songs) = self.observed[1]["songs"].as_array() else { return false };
        songs.iter().any(|row| row["id"].as_i64() == Some(song))
            && std::iter::once(&singer).chain(partners).all(|id| {
                singers.iter().any(|row| row["id"] == json!(id))
            })
    }
}
const VIEW_LIFETIME: Duration = Duration::from_secs(90);

#[derive(Default)]
pub(super) struct Ledger {
    permits: HashMap<Uuid, Permit>,
}

impl Ledger {
    fn issue(&mut self, operator: Uuid, query: Action, observed: Value) -> Uuid {
        let now = Instant::now();
        self.permits.retain(|_, p| p.expires > now);
        let ticket = Uuid::new_v4();
        self.permits.insert(
            operator,
            Permit {
                ticket,
                query,
                observed,
                expires: now + VIEW_LIFETIME,
            },
        );
        ticket
    }
    fn consume(&mut self, operator: Uuid, ticket: Option<Uuid>) -> Result<Permit, Failure> {
        let valid = self
            .permits
            .get(&operator)
            .is_some_and(|permit| Some(permit.ticket) == ticket && permit.expires > Instant::now());
        if !valid {
            return Err(conflict(
                "This operator view expired. Refresh before making another change.",
            ));
        }
        self.permits
            .remove(&operator)
            .ok_or_else(|| conflict("Refresh the operator view"))
    }
}

pub(super) fn routes() -> Router<Arc<Desk>> {
    Router::new().route("/api/operator", post(run)).route(
        "/operator.js",
        get(|| async {
            (
                [(header::CONTENT_TYPE, "text/javascript")],
                include_str!("operator.js"),
            )
        }),
    )
}

async fn read(desk: &Desk, query: Action, grant: &super::Grant) -> Result<Value, Failure> {
    desk.player
        .operator(query, grant.clone())
        .await
        .map_err(conflict)
}

async fn view_and_revision(
    desk: &Desk,
    query: Action,
    grant: &super::Grant,
) -> Result<(Value, Value), Failure> {
    let view = read(desk, query.clone(), grant).await?;
    // A catalog page or a singer's pocket alone cannot tell whether the selected
    // stage name, house rotation, or current track changed on the desktop.
    let revision = if query == Action::Overview {
        view.clone()
    } else {
        json!([read(desk, Action::Overview, grant).await?, view])
    };
    Ok((view, revision))
}

async fn run(
    State(desk): State<Arc<Desk>>,
    headers: HeaderMap,
    Json(req): Json<Request>,
) -> Result<Json<Value>, Failure> {
    let identity = authenticate(&desk, &headers)?;
    let grant = super::Grant::new(&desk, identity);
    if !req.action.valid() {
        return Err((StatusCode::BAD_REQUEST, "Invalid operator action".into()));
    }
    // Serialize commands across stations through the actual desktop acknowledgement.
    // Recheck authorization after waiting for other stations.
    let mut ledger = desk.operator.lock().await;
    authenticate(&desk, &headers)?;
    if !desk.host.has_host() {
        return Err(conflict("The desktop show host is offline"));
    }
    if req.action.read() {
        let (view, revision) = view_and_revision(&desk, req.action.clone(), &grant).await?;
        let overview = if req.action == Action::Overview {
            view.clone()
        } else {
            revision[0].clone()
        };
        let ticket = ledger.issue(identity, req.action, revision);
        return Ok(Json(
            json!({"ticket":ticket,"valid_for_ms":VIEW_LIFETIME.as_millis(),"view":view,"overview":overview}),
        ));
    }
    let permit = ledger.consume(identity, req.ticket)?;
    if let Action::Assign { singer, partners, song, .. } = &req.action {
        if !partners.is_empty() && !permit.covers_group_assignment(*singer, partners, *song) {
            return Err(conflict("Refresh the Library and select the group and song again."));
        }
    }
    if view_and_revision(&desk, permit.query, &grant).await?.1 != permit.observed {
        return Err(conflict(
            "The host changed this view. Refresh and check the target.",
        ));
    }
    authenticate(&desk, &headers)?;
    let result = desk
        .player
        .operator(req.action, grant)
        .await
        .map_err(conflict)?;
    Ok(Json(json!({"result":result})))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_view_ticket_is_bound_to_the_station_and_consumed_once() {
        let mut ledger = Ledger::default();
        let station = Uuid::new_v4();
        let first = ledger.issue(station, Action::Overview, json!([]));
        let latest = ledger.issue(station, Action::Overview, json!(["singer"]));
        assert!(ledger.consume(Uuid::new_v4(), Some(latest)).is_err());
        assert!(ledger.consume(station, Some(first)).is_err());
        assert_eq!(
            ledger.consume(station, Some(latest)).unwrap().observed,
            json!(["singer"])
        );
        assert!(ledger.consume(station, Some(latest)).is_err());
    }
    #[test]
    fn validates_catalog_page_ids_and_explicit_confirmation() {
        assert!(serde_json::from_value::<Action>(json!({
            "type":"transport","command":"next","current":null,"next":null,"generation":1,"loaded":true
        })).is_err());
        assert!(!Action::Search {
            query: "x".repeat(201),
            page: 1
        }
        .valid());
        assert!(!Action::NewShow { confirmed: false }.valid());
        assert!(!Action::Assign {
            partners: Vec::new(),
            singer: Uuid::new_v4(),
            song: 0,
            override_duplicate: false
        }
        .valid());
        assert!(!Action::Review {
            key: "\0".into(),
            approve: true
        }
        .valid());
    }

    #[test]
    fn shared_assignments_require_distinct_performers_and_their_catalog_view() {
        let lead = Uuid::new_v4();
        let guest = Uuid::new_v4();
        let assign = |partners: Vec<Uuid>| Action::Assign {
            singer: lead, partners, song: 9, override_duplicate: false,
        };
        assert!(assign(vec![guest]).valid());
        assert!(assign(vec![guest, Uuid::new_v4(), Uuid::new_v4()]).valid());
        assert!(!assign(vec![lead]).valid());
        assert!(!assign(vec![guest, guest]).valid());
        assert!(!assign((0..4).map(|_| Uuid::new_v4()).collect()).valid());
        let mut permit = Permit {
            ticket: Uuid::new_v4(),
            query: Action::Search { query: String::new(), page: 1 },
            observed: json!([{"singers":[{"id":lead},{"id":guest}]},{"songs":[{"id":9}]}]),
            expires: Instant::now() + VIEW_LIFETIME,
        };
        assert!(permit.covers_group_assignment(lead, &[guest], 9));
        assert!(!permit.covers_group_assignment(lead, &[Uuid::new_v4()], 9));
        assert!(!permit.covers_group_assignment(lead, &[guest], 10));
        permit.query = Action::Overview;
        assert!(!permit.covers_group_assignment(lead, &[guest], 9));
    }

    #[tokio::test]
    async fn operator_route_rejects_stale_views_and_replayed_or_revoked_tickets() {
        use crate::studio::{Operator, Playback, StudioPlayer};
        use axum::body::{to_bytes, Body};
        use axum::http::Request as HttpRequest;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tower::ServiceExt;

        #[derive(Default)]
        struct Host(AtomicUsize, tokio::sync::Notify, tokio::sync::Notify);
        impl StudioPlayer for Host {
            fn playback(&self) -> Playback {
                Playback {
                    load_gen: 1,
                    state: "stopped".into(),
                    elapsed: 0.,
                    duration: 0.,
                }
            }
            fn set_paused(&self, _: bool) -> Result<(), String> {
                Ok(())
            }
            fn operator(
                &self,
                action: Action,
                grant: crate::studio::Grant,
            ) -> futures::future::BoxFuture<'_, Result<Value, String>> {
                Box::pin(async move {
                    match action {
                        Action::Overview => Ok(json!({
                            "revision":self.0.load(Ordering::SeqCst),
                            "singers":[{"id":Uuid::from_u128(1)},{"id":Uuid::from_u128(2)}]
                        })),
                        Action::Search { .. } => Ok(json!({"songs":[{"id":9}]})),
                        Action::Assign { .. } => {
                            grant.check()?;
                            Ok(json!(self.0.fetch_add(1, Ordering::SeqCst)))
                        }
                        Action::AddSinger { name } => {
                            if name == "deferred" {
                                self.1.notify_one();
                                self.2.notified().await;
                            }
                            grant.check()?;
                            Ok(json!(self.0.fetch_add(1, Ordering::SeqCst)))
                        }
                        _ => Err("Unexpected test action".into()),
                    }
                })
            }
        }
        let host = crate::state::ServerState::new();
        let _desktop = host.host_tx.subscribe();
        let player = Arc::new(Host::default());
        let secret = Uuid::new_v4();
        let desk = Arc::new(Desk {
            host,
            player: player.clone(),
            mixer: crate::xr12::Mixer::connect(std::net::Ipv4Addr::LOCALHOST)
                .await
                .unwrap(),
            sessions: std::sync::Mutex::new(HashMap::from([(
                secret,
                Operator {
                    live: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                    previous: Default::default(),
                    id: Uuid::new_v4(),
                    armed_epoch: None,
                    arm_generation: 0,
                    last_snapshot: std::time::Instant::now(),
                },
            )])),
            commands: Default::default(),
            transport: Default::default(),
            operator: Default::default(),
        });
        let app = routes().with_state(desk.clone());
        let call = |body: Value| {
            let router = app.clone();
            async move {
                let req = HttpRequest::post("/api/operator")
                    .header("Authorization", format!("Bearer {secret}"))
                    .header("Content-Type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap();
                let res = router.oneshot(req).await.unwrap();
                let status = res.status();
                let bytes = to_bytes(res.into_body(), 4096).await.unwrap();
                (
                    status,
                    serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
                )
            }
        };
        let overview = || call(json!({"action":{"type":"overview"}}));
        let add = |ticket: Value| {
            call(json!({"action":{"type":"add_singer","name":"DJ"},"ticket":ticket}))
        };
        let library = || call(json!({"action":{"type":"search","query":"Duet","page":1}}));
        let group = |ticket: Value| call(json!({
            "action":{"type":"assign","singer":Uuid::from_u128(1),"partners":[Uuid::from_u128(2)],"song":9},
            "ticket":ticket
        }));
        let (_, old_library) = library().await;
        player.0.store(1, Ordering::SeqCst);
        assert_eq!(group(old_library["ticket"].clone()).await.0, StatusCode::CONFLICT);
        let (_, current_library) = library().await;
        assert_eq!(group(current_library["ticket"].clone()).await.0, StatusCode::OK);
        assert_eq!(group(current_library["ticket"].clone()).await.0, StatusCode::CONFLICT);
        assert_eq!(player.0.load(Ordering::SeqCst), 2);
        let (ok, first) = overview().await;
        assert_eq!(ok, StatusCode::OK);
        player.0.store(3, Ordering::SeqCst);
        assert_eq!(add(first["ticket"].clone()).await.0, StatusCode::CONFLICT);
        assert_eq!(player.0.load(Ordering::SeqCst), 3);
        let (_, next) = overview().await;
        assert_eq!(add(next["ticket"].clone()).await.0, StatusCode::OK);
        assert_eq!(add(next["ticket"].clone()).await.0, StatusCode::CONFLICT);
        assert_eq!(player.0.load(Ordering::SeqCst), 4);
        let (_, last) = overview().await;
        let deferred =
            call(json!({"action":{"type":"add_singer","name":"deferred"},"ticket":last["ticket"]}));
        let revoke = async {
            player.1.notified().await;
            desk.sessions.lock().unwrap().clear();
            player.2.notify_one();
        };
        let ((deferred_status, _), ()) = tokio::join!(deferred, revoke);
        assert_eq!(deferred_status, StatusCode::CONFLICT);
        assert_eq!(player.0.load(Ordering::SeqCst), 4);
        assert_eq!(
            add(last["ticket"].clone()).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
}
