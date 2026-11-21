//! Local playlist IDs and conditional commands for the paired studio console.
use super::*;
use dj_engine::{StudioDeckCommand, StudioDeckOperation};
use local_server::studio::{
    operator::{Action, BreakCommand},
    Grant,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

static PLAYLIST_EPOCH: AtomicU64 = AtomicU64::new(1);
pub(super) fn playlist_changed() {
    PLAYLIST_EPOCH.fetch_add(1, Ordering::SeqCst);
}

fn track_id(path: &str) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, path.as_bytes())
}

#[derive(Default)]
struct RunView {
    id: Option<Uuid>,
    phase: String,
    current: Option<String>,
    next: Option<String>,
}
fn run_view(id: Uuid, exec: &AutomixExecutor) -> RunView {
    let (phase, current, next) = exec.studio_identity();
    RunView {
        id: Some(id),
        phase: format!("{phase:?}"),
        current: current.map(str::to_owned),
        next: next.map(str::to_owned),
    }
}
struct Capture {
    revision: String,
    instance: u64,
    version: u64,
    states: [DeckPlayState; 2],
    deck: Option<usize>,
    current: Option<String>,
    current_title: String,
    next: Option<String>,
    epoch: u64,
    playlist: u64,
    automix: bool,
    phase: String,
}
impl Capture {
    fn public(&self, skip_allowed: bool) -> Value {
        json!({"revision":self.revision,"playing":self.deck.is_some(),"automix":self.automix,"phase":self.phase,"skip_allowed":skip_allowed,
            "current":self.current.as_deref().map(track_id),"title":self.current_title,"next":self.next.as_deref().map(track_id)})
    }
}
fn capture(state: &DjState, run: RunView) -> Capture {
    let snap = state
        .engine
        .lock_safe()
        .as_ref()
        .filter(|e| e.is_running())
        .map(|e| e.latest_snapshot());
    let states = snap.as_ref().map_or([DeckPlayState::Empty; 2], |s| {
        [s.decks[0].state, s.decks[1].state]
    });
    let deck = states.iter().position(|s| *s == DeckPlayState::Playing);
    let version = snap.as_ref().map_or(0, |s| s.transport_revision);
    let instance = snap.as_ref().map_or(0, |s| s.engine_instance);
    let meta = deck.and_then(|d| snap.as_ref()?.decks[d].track.as_ref());
    let current = run.current.or_else(|| meta.map(|m| m.source_key.clone()));
    let current_title = meta
        .filter(|m| Some(m.source_key.as_str()) == current.as_deref())
        .map(|m| m.title.clone())
        .or_else(|| {
            current
                .as_deref()
                .map(|p| parse_artist_title(Path::new(p)).1)
        })
        .unwrap_or_default();
    let epoch = KARAOKE_EPOCH.load(Ordering::SeqCst);
    let playlist = PLAYLIST_EPOCH.load(Ordering::SeqCst);
    let loads = [
        state.load_gen[0].load(Ordering::SeqCst),
        state.load_gen[1].load(Ordering::SeqCst),
    ];
    let material = json!([
        instance, version, states, current, run.id, run.phase, run.next, epoch, playlist, loads
    ]);
    let revision = format!("{:x}", Sha256::digest(material.to_string().as_bytes()));
    Capture {
        revision,
        instance,
        version,
        states,
        deck,
        current,
        current_title,
        next: run.next,
        epoch,
        playlist,
        automix: run.id.is_some(),
        phase: run.phase,
    }
}
fn observe(state: &DjState) -> Capture {
    let run = state.automix.lock_safe();
    let view = run
        .as_ref()
        .map(|r| run_view(r.studio_id, &r.exec.lock_safe()))
        .unwrap_or_default();
    capture(state, view)
}
pub(crate) fn status(app: &AppHandle, state: &DjState) -> Value {
    observe(state).public(!app.state::<crate::LicenseState>().is_free())
}

fn playlist_page(app: &AppHandle, state: &DjState, page: u16) -> Result<Value, String> {
    let tracks = load_playlist_rows(&db_path_of(app))?;
    let live = observe(state);
    let next = live
        .next
        .as_ref()
        .and_then(|path| tracks.iter().find(|t| &t.filepath == path))
        .or_else(|| {
            if live.automix {
                return None;
            }
            if let Some(path) = &live.current {
                return tracks
                    .iter()
                    .position(|t| &t.filepath == path)
                    .and_then(|i| tracks.get(i + 1));
            }
            tracks.first()
        });
    let mut public = live.public(!app.state::<crate::LicenseState>().is_free());
    public["next"] = json!(next.map(|t| track_id(&t.filepath)));
    public["next_title"] = json!(next.map(|t| &t.title));
    let offset = usize::from(page.saturating_sub(1)) * 20;
    let rows: Vec<_> = tracks
        .iter()
        .skip(offset)
        .take(20)
        .map(|t| {
            json!({
                "id":track_id(&t.filepath),"title":t.title,"artist":t.artist
            })
        })
        .collect();
    Ok(json!({"status":public,"tracks":rows,"more":offset+20<tracks.len(),"page":page}))
}

fn submit(
    engine: &DjEngineHandle,
    observed: &Capture,
    op: StudioDeckOperation,
    grant: &Grant,
) -> Result<(), String> {
    let command = Arc::new(StudioDeckCommand {
        instance: (observed.instance != 0).then_some(observed.instance),
        revision: observed.version,
        states: observed.states,
        live: grant.live_flag()?,
        room_epoch: &KARAOKE_EPOCH,
        observed_epoch: observed.epoch,
        playlist_epoch: &PLAYLIST_EPOCH,
        observed_playlist: observed.playlist,
        operation: op,
        result: std::sync::atomic::AtomicU8::new(0),
    });
    engine
        .send(EngineCmd::Studio(command.clone()))
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match command.result.load(Ordering::Acquire) {
            1 => return Ok(()),
            2 | 3 => return Err("Break-Wave changed. Refresh before retrying.".into()),
            _ if Instant::now() >= deadline => {
                let _ = command
                    .result
                    .compare_exchange(0, 3, Ordering::AcqRel, Ordering::Acquire);
                return Err("The break engine did not confirm in time. Refresh playback.".into());
            }
            _ => thread::sleep(Duration::from_millis(4)),
        }
    }
}

pub(crate) fn dispatch(app: &AppHandle, action: Action, grant: Grant) -> Result<Value, String> {
    let state = app.state::<Arc<DjState>>();
    grant.check()?;
    if let Action::BreakView { page } = action {
        return playlist_page(app, &state, page);
    }
    let Action::BreakControl {
        command,
        track,
        revision,
    } = action
    else {
        return Err("Invalid break control".into());
    };
    let _serial = state.break_cmd_lock.lock_safe();
    let before = observe(&state);
    if before.revision != revision {
        return Err("Break-Wave changed. Refresh the playlist.".into());
    }
    let paid = !app.state::<crate::LicenseState>().is_free();
    if command != BreakCommand::Stop {
        crate::queue_ops::refuse_if_seat_halted()?;
        if karaoke_active(app) {
            return Err(KARAOKE_ACTIVE_MSG.into());
        }
    }
    if command == BreakCommand::Skip && !paid {
        return Err("Skipping break tracks needs Automix (Full version)".into());
    }
    if command == BreakCommand::Play && track.is_none() && before.deck.is_some() {
        return Ok(json!("Break music is already playing"));
    }
    if command == BreakCommand::Stop || before.automix {
        let step = state.studio_automix_step.lock_safe();
        let mut runs = state.automix.lock_safe();
        let mut exec = runs.as_ref().map(|r| r.exec.lock_safe());
        let view = runs
            .as_ref()
            .zip(exec.as_ref())
            .map(|(r, e)| run_view(r.studio_id, e))
            .unwrap_or_default();
        let actual = capture(&state, view);
        if actual.revision != revision {
            return Err("The current Break-Wave track changed".into());
        }
        grant.check()?;
        if command == BreakCommand::Skip {
            let live = grant.live_flag()?;
            let result = Arc::new(std::sync::atomic::AtomicU8::new(0));
            if exec.as_mut().is_some_and(|e| {
                e.studio_skip_next(live, actual.instance, actual.version, result.clone())
            }) {
                drop(exec);
                drop(runs);
                drop(step);
                let deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    match result.load(Ordering::Acquire) {
                        1 => return Ok(json!("Automix accepted the observed next track")),
                        2 | 3 => {
                            return Err("The automix target changed. Refresh the playlist.".into())
                        }
                        _ if Instant::now() >= deadline => {
                            if result
                                .compare_exchange(0, 3, Ordering::AcqRel, Ordering::Acquire)
                                .is_ok()
                            {
                                return Err("Automix did not accept the transition".into());
                            }
                        }
                        _ => thread::sleep(Duration::from_millis(5)),
                    }
                }
            }
            return Err("There is no upcoming automix transition available".into());
        }
        if command != BreakCommand::Stop {
            return Err(
                "Stop the running automix before starting a selected playlist track".into(),
            );
        }
        let restore = exec.as_ref().map(|e| {
            let (reset_rate, crossfader) = e.studio_release_settings();
            dj_engine::StudioDeckRestore {
                low: *state.host_eq_low.lock_safe(),
                kills: [state.host_kills(0), state.host_kills(1)],
                fx: [state.host_fx(0), state.host_fx(1)],
                reset_rate,
                crossfader,
            }
        });
        if let Ok(engine) = engine_handle(&state) {
            submit(&engine, &actual, StudioDeckOperation::Stop(restore), &grant)?;
        } else if actual.automix || actual.deck.is_some() {
            return Err("Break-Wave audio output is unavailable".into());
        }
        if let Some(run) = runs.as_ref() {
            run.stop.store(true, Ordering::SeqCst);
        }
        drop(exec);
        let stopped = runs.take();
        drop(runs);
        drop(step);
        if let Some(run) = stopped {
            finish_stopped_run(state.inner(), run, false, false);
        }
        return Ok(json!("Break music stopped"));
    }

    if before
        .states
        .iter()
        .filter(|s| **s == DeckPlayState::Playing)
        .count()
        > 1
    {
        return Err(
            "Both break decks are playing. Stop the current mix before choosing a track.".into(),
        );
    }
    let playlist = load_playlist_rows(&db_path_of(app))?;
    let source = match (command, track) {
        (BreakCommand::Play, Some(id)) => playlist.iter().find(|t| track_id(&t.filepath) == id),
        (BreakCommand::Play, None) => playlist.first(),
        (BreakCommand::Skip, _) => before
            .current
            .as_ref()
            .and_then(|path| playlist.iter().position(|t| &t.filepath == path))
            .and_then(|i| playlist.get(i + 1))
            .or_else(|| {
                if before.current.is_none() {
                    playlist.first()
                } else {
                    None
                }
            }),
        _ => None,
    }
    .ok_or("The selected or next track is unavailable in the break playlist")?;
    if before.deck.is_some() && !paid {
        return Err("Stop the current track before starting a selection".into());
    }
    let mut decoded = decode_local_file(Path::new(&source.filepath))
        .map_err(|_| "The selected break track could not be decoded")?;
    dj_engine::source::normalize_track(&mut decoded);
    let meta = meta_from_row(
        &source.filepath,
        source.artist.clone(),
        source.title.clone(),
        decoded.duration_secs(),
        None,
    );
    if observe(&state).revision != revision || karaoke_active(app) {
        return Err("The room or break playlist changed while preparing this track".into());
    }
    grant.check()?;
    let engine = ensure_engine_started(app, &state)?;
    let mut paths = state.deck_paths.lock_safe();
    let runs = state.automix.lock_safe();
    crate::queue_ops::refuse_if_seat_halted()?;
    if runs.is_some() || karaoke_active(app) {
        return Err("The host took over Break-Wave".into());
    }
    let snap = engine.latest_snapshot();
    if before.instance != 0 && snap.engine_instance != before.instance {
        return Err("Break-Wave restarted".into());
    }
    let deck = before.deck.unwrap_or(usize::from(snap.crossfader > 0.5));
    if !engine.is_running() {
        return Err("Break-Wave audio output is unavailable".into());
    }
    submit(
        &engine,
        &before,
        StudioDeckOperation::LoadPlay {
            deck,
            track: Arc::new(decoded),
            meta: Arc::new(meta),
        },
        &grant,
    )?;
    paths[deck] = Some(source.filepath.clone());
    Ok(json!(format!("Playing {}", source.title)))
}
