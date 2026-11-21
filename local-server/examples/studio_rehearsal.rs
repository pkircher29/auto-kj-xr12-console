//! Starts the real operator service against a loopback XR12 simulator and a rehearsal transport.
//! Test harnesses receive short-lived private pairings over an inherited pipe; never print its stdout to logs.
use local_server::{
    protocol::{HostSlotView, ServerToHostMessage},
    state::ServerState,
    studio::{Playback, Studio, StudioPlayer},
};
use std::{
    io::{self, Write},
    net::Ipv4Addr,
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncBufReadExt, BufReader};

struct Rehearsal(Mutex<String>);
impl StudioPlayer for Rehearsal {
    fn operator(
        &self,
        action: local_server::studio::operator::Action,
        grant: local_server::studio::Grant,
    ) -> futures::future::BoxFuture<'_, Result<serde_json::Value, String>> {
        Box::pin(async move {
            use local_server::studio::operator::{Action, Transport};
            grant.check()?;
            match action {
                Action::Transport {
                    command: Transport::Pause,
                    generation: 1,
                    ..
                } => self.set_paused(true)?,
                Action::Transport {
                    command: Transport::Resume | Transport::Play,
                    generation: 1,
                    ..
                } => self.set_paused(false)?,
                Action::Transport {
                    command: Transport::Stop,
                    generation: 1,
                    ..
                } => *self.0.lock().unwrap() = "stopped".into(),
                Action::Transport {
                    command: Transport::Next,
                    generation: 1,
                    ..
                } => {}
                _ => return Err("Unavailable on rehearsal transport".into()),
            }
            Ok(serde_json::Value::Bool(true))
        })
    }
    fn turn_targets(&self, _: u64) -> local_server::studio::TurnTargets {
        let active = self.0.lock().unwrap().as_str() != "stopped";
        local_server::studio::TurnTargets {
            active: active.then(|| "bench-turn".into()),
            next: (!active).then(|| "bench-turn".into()),
            ..Default::default()
        }
    }
    fn playback(&self) -> Playback {
        Playback {
            load_gen: 1,
            state: self.0.lock().unwrap().clone(),
            elapsed: 37.0,
            duration: 180.0,
        }
    }
    fn set_paused(&self, paused: bool) -> Result<(), String> {
        *self.0.lock().unwrap() = if paused { "paused" } else { "playing" }.into();
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() != Some("--pairing-pipe") {
        eprintln!("Run scripts/studio-rehearsal.py to open an isolated console rehearsal.");
        return Ok(());
    }
    let host = ServerState::new();
    let mut engine = host.host_tx.subscribe();
    let player = Arc::new(Rehearsal(Mutex::new("playing".into())));
    let slot = HostSlotView {
        request_id: "bench-turn".into(),
        slot: 0,
        singer_name: "Bench vocal".into(),
        is_current: true,
        song_title: "Rehearsal track".into(),
        song_artist: "Test signal".into(),
        singer_id: "bench".into(),
    };
    host.set_last_host_rotation(ServerToHostMessage::RotationState {
        line: vec![],
        pause_queue: vec![],
        is_playing: true,
        is_break_playing: false,
        current_break_track: String::new(),
        host_slots: vec![slot],
    })
    .await;
    let studio = Studio::start(
        host,
        player.clone(),
        Ipv4Addr::LOCALHOST,
        Ipv4Addr::LOCALHOST,
    )
    .await?;
    let first = studio.pair()?;
    let second = studio.pair()?;
    println!(
        "{}",
        serde_json::to_string(&[first, second, studio.pair()?])?
    );
    io::stdout().flush()?;
    // The parent closing its pipe shuts down both listeners and releases the mixer socket.
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        tokio::select! {
            line=lines.next_line()=>match line? {
                None=>break,
                Some(s) if s=="quit"=>break,
                Some(s) if s=="pause"=>{player.set_paused(true)?;},
                Some(s) if s=="resume"=>{player.set_paused(false)?;},
                _=>{}
            },
            frame=engine.recv()=>match frame?{
                ServerToHostMessage::RemoteStopRequested=>*player.0.lock().unwrap()="stopped".into(),
                ServerToHostMessage::RemotePlayRequested=>player.set_paused(false)?,
                _=>{}
            }
        }
    }
    Ok(())
}
