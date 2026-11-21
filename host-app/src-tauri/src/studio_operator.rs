//! Bridge between the dedicated operator service and the main show window.
use super::*;
use local_server::studio::operator::Action;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{Emitter, Manager};
use tokio::sync::oneshot;
use uuid::Uuid;

type Reply = Result<Value, String>;

#[derive(Default)]
pub(super) struct Pending {
    calls: Mutex<HashMap<Uuid, Call>>,
}
struct Call {
    claimed: bool,
    reply: oneshot::Sender<Reply>,
    grant: local_server::studio::Grant,
}

pub(super) fn check_claim(app: &tauri::AppHandle, id: Uuid) -> Result<(), String> {
    let pending = app.state::<Pending>();
    let calls = pending.calls.lock_safe();
    let call = calls
        .get(&id)
        .filter(|call| call.claimed && !call.reply.is_closed())
        .ok_or("The studio request expired")?;
    call.grant.check()
}

#[tauri::command]
pub(super) fn finish_studio_operator(
    window: tauri::WebviewWindow,
    pending: tauri::State<'_, Pending>,
    id: Uuid,
    claim: Option<bool>,
    result: Option<Value>,
    error: Option<String>,
) -> Result<(), String> {
    if window.label() != "main" {
        return Err("Only the show window can confirm an operator action".into());
    }
    let mut calls = pending.calls.lock_safe();
    let call = calls.get_mut(&id).ok_or("Operator request expired")?;
    if claim.unwrap_or(false) {
        if let Err(error) = call.grant.check() {
            let pending_call = calls.remove(&id).expect("pending call checked above");
            let _ = pending_call.reply.send(Err(error.clone()));
            return Err(error);
        }
        if call.claimed || call.reply.is_closed() {
            return Err("Operator request was already claimed or cancelled".into());
        }
        call.claimed = true;
        return Ok(());
    }
    if !call.claimed {
        return Err("Operator request was never claimed by the desktop".into());
    }
    let sender = calls.remove(&id).ok_or("Operator request expired")?.reply;
    let reply = match error {
        Some(e) => Err(e.chars().take(300).collect()),
        None => Ok(result.unwrap_or(Value::Null)),
    };
    let _ = sender.send(reply);
    Ok(())
}

struct PendingGuard<'a>(&'a Pending, Uuid);
impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.0.calls.lock_safe().remove(&self.1);
    }
}

pub(super) async fn dispatch(
    app: &tauri::AppHandle,
    action: Action,
    grant: local_server::studio::Grant,
) -> Reply {
    grant.check()?;
    if matches!(
        action,
        Action::BreakView { .. } | Action::BreakControl { .. }
    ) {
        let app = app.clone();
        return tauri::async_runtime::spawn_blocking(move || {
            crate::dj::studio_break::dispatch(&app, action, grant)
        })
        .await
        .map_err(|e| format!("Break-Wave task failed: {e}"))?;
    }
    if let Action::Transport {
        command,
        generation,
        ..
    } = &action
    {
        use local_server::studio::operator::Transport;
        if matches!(command, Transport::Pause | Transport::Resume) {
            let fade_secs = app
                .state::<Mutex<BreakTrackState>>()
                .lock_safe()
                .crossfade_secs;
            let (reply, result) = oneshot::channel();
            app.state::<AudioState>()
                .cmd_tx
                .send(AudioCmd::StudioPause {
                    gen: *generation,
                    paused: *command == Transport::Pause,
                    fade_secs,
                    grant,
                    reply,
                })
                .map_err(|e| e.to_string())?;
            tokio::time::timeout(Duration::from_secs(3), result)
                .await
                .map_err(|_| "The audio engine did not confirm in time. Refresh playback.")?
                .map_err(|_| "Audio engine disconnected")??;
            return Ok(json!(true));
        }
    }
    if let Action::Search { query, page } = &action {
        let page = search_songs_cmd(app.state(), query.clone(), i64::from(*page), 20).await?;
        let songs: Vec<_> = page
            .songs
            .into_iter()
            .map(|s| {
                json!({
                    "id":s.id,"title":s.title,"artist":s.artist,"manufacturer":s.manufacturer_code
                })
            })
            .collect();
        return Ok(json!({"songs":songs,"page":page.page,"more":page.has_more}));
    }
    // A new file can require a first-play scan. Leave time for the audio worker's receipt
    // inside the station's 30-second transport deadline.
    use local_server::studio::operator::Transport;
    let response_secs = if matches!(&action, Action::Transport { command: Transport::Play | Transport::Next, .. }) {
        25
    } else {
        7
    };
    // Catalog identifiers cross the LAN; host paths are resolved locally and delivered only to main.
    let mut payload = json!({"action":action});
    if let Action::Assign { song, .. } | Action::Replace { song, .. } = action {
        let track = with_catalog_db(&app.state(), move |conn| {
            catalog::db::get_song_by_id(conn, song).map_err(|e| e.to_string())
        })
        .await?
        .ok_or("This song is no longer in the catalog")?;
        payload["track"] =
            json!({"title":track.title,"artist":track.artist,"filepath":track.filepath});
    }
    let pending = app.state::<Pending>();
    let id = Uuid::new_v4();
    let (sender, receiver) = oneshot::channel();
    pending.calls.lock_safe().insert(
        id,
        Call {
            claimed: false,
            reply: sender,
            grant,
        },
    );
    let _guard = PendingGuard(&pending, id);
    payload["id"] = json!(id);
    payload["deadline"] = json!(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
            + (response_secs - 1) * 1_000
    );
    app.emit_to("main", "studio-operator", payload)
        .map_err(|e| e.to_string())?;
    tokio::time::timeout(Duration::from_secs(response_secs), receiver)
        .await
        .map_err(|_| {
            "The desktop did not confirm in time. Refresh and verify the result before trying again.".to_string()
        })?
        .map_err(|_| "The desktop show window closed".to_string())?
}
