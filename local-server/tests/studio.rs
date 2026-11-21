use local_server::{
    protocol::ServerToHostMessage,
    state::ServerState,
    studio::{Playback, Studio, StudioPlayer},
};
use serde_json::{json, Value};
use std::{
    net::Ipv4Addr,
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct TestPlayer {
    playback: Mutex<String>,
    transport: Mutex<Vec<local_server::studio::operator::Transport>>,
    breaks: Mutex<Vec<local_server::studio::operator::BreakCommand>>,
    fail_next: std::sync::atomic::AtomicBool,
    opening_track: tokio::sync::Notify,
    finish_open: tokio::sync::Notify,
}
impl StudioPlayer for TestPlayer {
    fn operator(
        &self,
        action: local_server::studio::operator::Action,
        grant: local_server::studio::Grant,
    ) -> futures::future::BoxFuture<'_, Result<Value, String>> {
        Box::pin(async move {
            use local_server::studio::operator::{Action, Transport};
            grant.check()?;
            if let Action::BreakControl {
                command, revision, ..
            } = action
            {
                let mut calls = self.breaks.lock().unwrap();
                if revision != format!("{:064x}", calls.len()) {
                    return Err("Break target changed".into());
                }
                calls.push(command);
                return Ok(json!(true));
            }
            let Action::Transport {
                command,
                generation: 1,
                current: Some(ref current),
                ..
            } = action
            else {
                return Err("Missing guarded transport".into());
            };
            if current != "test-turn" {
                return Err("Wrong turn".into());
            }
            self.transport.lock().unwrap().push(command);
            if command == Transport::Next && self.fail_next.load(std::sync::atomic::Ordering::Relaxed) {
                self.opening_track.notify_one();
                self.finish_open.notified().await;
                return Err("The previous performance completed. The next media failed to load.".into());
            }
            if matches!(command, Transport::Pause | Transport::Resume) {
                self.set_paused(command == Transport::Pause)?;
            }
            Ok(json!(true))
        })
    }
    fn turn_targets(&self, _: u64) -> local_server::studio::TurnTargets {
        local_server::studio::TurnTargets {
            active: Some("test-turn".into()),
            next: Some("successor".into()),
            ..Default::default()
        }
    }
    fn playback(&self) -> Playback {
        Playback {
            load_gen: 1,
            state: self.playback.lock().unwrap().clone(),
            elapsed: 37.,
            duration: 140.,
        }
    }
    fn break_status(&self) -> Value {
        json!({"revision":format!("{:064x}",self.breaks.lock().unwrap().len())})
    }
    fn set_paused(&self, paused: bool) -> Result<(), String> {
        *self.playback.lock().unwrap() = if paused { "paused" } else { "playing" }.into();
        Ok(())
    }
}

async fn request(address: &str, token: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    let method = if body.is_some() { "POST" } else { "GET" };
    let body = body.map(Value::to_string).unwrap_or_default();
    let wire=format!("{method} /api/{path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",body.len());
    socket.write_all(wire.as_bytes()).await.unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).await.unwrap();
    let (head, data) = response.split_once("\r\n\r\n").unwrap();
    (
        head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        serde_json::from_str(data).unwrap_or_else(|_| Value::String(data.to_owned())),
    )
}

#[tokio::test]
async fn operator_sessions_observe_playback_and_revoke_independently() {
    let host = ServerState::new();
    let mut receiver = host.host_tx.subscribe();
    host.set_last_host_rotation(ServerToHostMessage::RotationState {
        line: vec![],
        pause_queue: vec![],
        is_playing: true,
        is_break_playing: false,
        current_break_track: String::new(),
        host_slots: vec![],
    })
    .await;
    let player = Arc::new(TestPlayer {
        playback: Mutex::new("playing".into()),
        ..Default::default()
    });
    let studio = Studio::start(
        host.clone(),
        player.clone(),
        // A separate loopback address leaves the rehearsal OSC simulator on 127.0.0.1 alone.
        Ipv4Addr::new(127, 0, 0, 93),
        Ipv4Addr::LOCALHOST,
    )
    .await
    .unwrap();
    let first = studio.pair().unwrap();
    let second = studio.pair().unwrap();
    let (address, a) = first
        .desktop_url
        .strip_prefix("http://")
        .unwrap()
        .split_once("/#token=")
        .unwrap();
    let (_, b) = second.desktop_url.split_once("#token=").unwrap();
    assert_ne!(a, b);
    assert_eq!(
        request(address, &host.remote_token(), "state", None)
            .await
            .0,
        401
    );
    let (status, snapshot) = request(address, a, "state", None).await;
    assert_eq!(status, 200);
    assert_eq!(snapshot["playback"]["state"], "playing");
    assert_eq!(snapshot["mixer"]["connected"], false);
    let stale_break =
        json!({"action":"break_play","revision":snapshot["revision"],"id":uuid::Uuid::new_v4()});
    for action in ["break_play", "break_skip", "break_stop"] {
        let (_, state) = request(address, a, "state", None).await;
        let write = json!({"action":action,"revision":state["revision"],"id":uuid::Uuid::new_v4()});
        assert_eq!(request(address, a, "show", Some(&write)).await.0, 202);
    }
    assert_eq!(request(address, a, "show", Some(&stale_break)).await.0, 409);
    assert!(receiver.try_recv().is_err());
    assert_eq!(player.breaks.lock().unwrap().len(), 3);
    let (_, snapshot) = request(address, a, "state", None).await;
    let pause = json!({"action":"pause","id":uuid::Uuid::new_v4(),"revision":snapshot["revision"]});
    assert_eq!(request(address, a, "show", Some(&pause)).await.0, 202);
    assert_eq!(request(address, a, "show", Some(&pause)).await.0, 202);
    let (_, paused) = request(address, b, "state", None).await;
    assert_eq!(paused["playback"]["state"], "paused");
    let stale = json!({"action":"next","id":uuid::Uuid::new_v4(),"revision":snapshot["revision"]});
    assert_eq!(request(address, b, "show", Some(&stale)).await.0, 409);
    let next = json!({"action":"next","id":uuid::Uuid::new_v4(),"revision":paused["revision"]});
    player.fail_next.store(true, std::sync::atomic::Ordering::Relaxed);
    let failed = json!({"action":"next","id":uuid::Uuid::new_v4(),"revision":paused["revision"]});
    let pending_next = request(address, b, "show", Some(&failed));
    let peer_sync = async {
        player.opening_track.notified().await;
        let cursor = json!({"cursor":null});
        let state = tokio::time::timeout(std::time::Duration::from_secs(2),
            request(address, a, "sync", Some(&cursor))).await;
        player.finish_open.notify_one();
        let (status, state) = state.expect("mixing station blocked behind the other station's decoder");
        assert_eq!(status, 200);
        assert!(state["mixer"]["epoch"].is_string());
    };
    let (first_failure, ()) = tokio::join!(pending_next, peer_sync);
    assert_eq!(first_failure.0, 409);
    assert!(first_failure.1.to_string().contains("previous performance completed"));
    player.fail_next.store(false, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(request(address, b, "show", Some(&failed)).await, first_failure);
    assert_eq!(request(address, b, "show", Some(&next)).await.0, 202);
    assert_eq!(request(address, b, "show", Some(&next)).await.0, 202);
    use local_server::studio::operator::Transport;
    assert_eq!(
        *player.transport.lock().unwrap(),
        vec![Transport::Pause, Transport::Next, Transport::Next]
    );
    assert!(receiver.try_recv().is_err());
    studio.revoke(first.id);
    assert_eq!(request(address, a, "state", None).await.0, 401);
    drop(receiver);
    assert_eq!(
        request(address, b, "state", None).await.1["show"],
        Value::Null
    );
}
