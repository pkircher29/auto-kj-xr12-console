use super::*;

fn hello(model: &str) -> Vec<u8> {
    message(
        "/xinfo",
        &[
            Arg::Text("127.0.0.1"),
            Arg::Text("Bench"),
            Arg::Text(model),
            Arg::Text("1.26"),
        ],
    )
}

async fn until<T>(mut check: impl AsyncFnMut() -> Option<T>) -> T {
    time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(found) = check().await {
                return found;
            }
            time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for mixer state")
}

async fn setup() -> (UdpSocket, Mixer, std::net::SocketAddr) {
    let fake = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = fake.local_addr().unwrap().port();
    let mixer = Mixer::start(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let mut data = [0; 128];
    let (n, who) = time::timeout(Duration::from_secs(2), fake.recv_from(&mut data))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(osc::parse(&data[..n]), Some(("/xinfo", Vec::new())));
    (fake, mixer, who)
}

async fn snapshot_with(mixer: &Mixer, key: &str) -> MixerSnapshot {
    until(async || {
        let s = mixer.snapshot().await;
        s.values.contains_key(key).then_some(s)
    })
    .await
}

const PATCH: &str = "/ch/11/config/insrc";
const GAIN: &str = "/headamp/02/gain";
const PHANTOM: &str = "/headamp/02/phantom";

#[test]
fn preamp_mapping_and_control_observations_expire_independently() {
    let mut state = State::empty();
    let edit = preamp::Edit {
        map: PATCH.into(),
        input: 2,
        expected: 0.into(),
    };
    let now = Instant::now();
    let stale = now - VALUE_TTL;
    let other = "/ch/03/config/insrc";
    state.values.insert(other.into(), (1.into(), now));
    state.values.insert(PATCH.into(), (1.into(), stale));
    state.values.insert(PHANTOM.into(), (0.into(), now));
    assert!(state.has_preamp(2)); // shared physical input, other patch is fresh
    assert!(!edit.valid(&state, PHANTOM)); // gesture still bound to channel 11
    state.values.insert(other.into(), (1.into(), stale));
    assert!(!state.has_preamp(2));
    assert!(state.parameter(&Catalog::new(), PHANTOM).is_none());
    state.values.insert(PATCH.into(), (1.into(), now));
    assert!(edit.valid(&state, PHANTOM));
    state.values.insert(PHANTOM.into(), (0.into(), stale));
    assert!(!edit.valid(&state, PHANTOM));
}

async fn muted_preamp_fixture() -> (UdpSocket, Mixer, std::net::SocketAddr, MixerSnapshot) {
    let (peer, mixer, address) = setup().await;
    peer.send_to(&hello("XR12"), address).await.unwrap();
    for (key, arg) in [
        ("/ch/11/mix/on", Arg::Int(0)),
        ("/ch/01/mix/on", Arg::Int(0)),
        ("/ch/01/config/insrc", Arg::Int(4)),  // line input 5
        (PATCH, Arg::Int(1)),                  // channel 11 patched to physical XLR 2
        ("/headamp/04/gain", Arg::Float(0.8)), // unsolicited, unmapped
        (GAIN, Arg::Float(0.5)),
        (PHANTOM, Arg::Int(0)),
    ] {
        peer.send_to(&message(key, &[arg]), address).await.unwrap();
    }
    let state = snapshot_with(&mixer, PHANTOM).await;
    (peer, mixer, address, state)
}

fn preamp_request(epoch: &str, control: &str, expected: Value, value: Value) -> Value {
    serde_json::json!({"epoch":epoch,"channel":11,"input":2,"control":control,"expected":expected,"value":value})
}

async fn preamp_action(
    mixer: Mixer,
    body: Value,
    allowed: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Result<Value, MixerError> {
    mixer
        .with_writer(Arc::new(move |socket, packet| {
            if allowed() { socket.try_send(packet).map(|_| true) } else { Ok(false) }
        }))
        .set_preamp(serde_json::from_value(body).unwrap())
        .await
}

#[tokio::test]
async fn mapped_preamp_controls_require_a_fresh_patch_and_specific_phantom_confirmation() {
    let (peer, mixer, address, state) = muted_preamp_fixture().await;
    assert_eq!(state.values[PATCH].value, 1);
    assert_eq!(state.values["/ch/11/mix/on"].value, 0);
    assert!(!state.values.contains_key("/headamp/04/gain"));
    assert_eq!(
        mixer.set(PHANTOM, 1.into(), &state.epoch).await,
        Err(MixerError::Unavailable)
    );
    assert_eq!(
        mixer
            .compare_and_set(GAIN, 0.5.into(), 0.625.into(), &state.epoch)
            .await,
        Err(MixerError::Unavailable)
    );
    let phantom = preamp_request(&state.epoch, "phantom", 0.into(), 1.into());
    let gain = preamp_request(&state.epoch, "gain", 0.5.into(), 0.625.into());
    let mut confirmed = phantom.clone();
    confirmed["confirmed_input"] = 2.into();
    for (mut body, key, val, expected_error) in [
        (
            phantom.clone(),
            "confirmed_input",
            Value::Null,
            MixerError::InvalidValue,
        ),
        (
            phantom,
            "confirmed_input",
            1.into(),
            MixerError::InvalidValue,
        ),
        (
            confirmed.clone(),
            "expected",
            1.into(),
            MixerError::Unavailable,
        ),
        (
            confirmed.clone(),
            "value",
            2.into(),
            MixerError::InvalidValue,
        ),
        (
            gain.clone(),
            "expected",
            0.25.into(),
            MixerError::Unavailable,
        ),
        (gain.clone(), "channel", 1.into(), MixerError::Unavailable),
        (gain.clone(), "input", 3.into(), MixerError::Unavailable),
        (gain.clone(), "input", 5.into(), MixerError::Unavailable),
        (
            gain.clone(),
            "epoch",
            "previous".into(),
            MixerError::StaleSession,
        ),
    ] {
        body[key] = val;
        assert_eq!(
            preamp_action(mixer.clone(), body, Arc::new(|| true)).await,
            Err(expected_error)
        );
    }
    let denied = tokio::spawn(preamp_action(mixer.clone(), confirmed.clone(), Arc::new(|| false)));
    for (key, value) in [(PATCH, Arg::Int(1)), (PHANTOM, Arg::Int(0)), (PATCH, Arg::Int(1))] {
        request_for(&peer, key, false).await;
        peer.send_to(&message(key, &[value]), address).await.unwrap();
    }
    assert_eq!(denied.await.unwrap(), Err(MixerError::Unavailable));
    // Targeted reads of the patch, physical pin, and patch again precede the
    // only parameter mutation. An unrelated pin cannot confirm that mutation.
    for (body, key, previous, target) in [
        (gain, GAIN, Arg::Float(0.5), Arg::Float(0.625)),
        (confirmed, PHANTOM, Arg::Int(0), Arg::Int(1)),
    ] {
        let result = tokio::spawn(preamp_action(mixer.clone(), body, Arc::new(|| true)));
        for (check, value) in [(PATCH, Arg::Int(1)), (key, previous), (PATCH, Arg::Int(1))] {
            request_for(&peer, check, false).await;
            peer.send_to(&message(check, &[value]), address)
                .await
                .unwrap();
        }
        let sent = request_for(&peer, key, true).await;
        assert_eq!(sent, message(key, std::slice::from_ref(&target)));
        peer.send_to(&message("/headamp/03/phantom", &[Arg::Int(1)]), address)
            .await
            .unwrap();
        assert!(!result.is_finished());
        peer.send_to(&sent, address).await.unwrap();
        assert!(result.await.unwrap().is_ok());
    }
    assert_eq!(mixer.snapshot().await.values["/ch/11/mix/on"].value, 0);
}

#[tokio::test]
async fn preamp_remap_external_edit_and_disarm_abort_the_wire_write() {
    use std::sync::atomic::{AtomicBool, Ordering};
    for interruption in ["map", "phantom", "disarm"] {
        let (peer, mixer, address, state) = muted_preamp_fixture().await;
        let armed = Arc::new(AtomicBool::new(true));
        let authority = armed.clone();
        let mut body = preamp_request(&state.epoch, "phantom", 0.into(), 1.into());
        body["confirmed_input"] = 2.into();
        let result = tokio::spawn(preamp_action(
            mixer.clone(),
            body,
            Arc::new(move || authority.load(Ordering::SeqCst)),
        ));
        for (key, arg) in [(PATCH, Arg::Int(1)), (PHANTOM, Arg::Int(0))] {
            request_for(&peer, key, false).await;
            peer.send_to(&message(key, &[arg]), address).await.unwrap();
        }
        request_for(&peer, PATCH, false).await;
        if interruption == "disarm" {
            armed.store(false, Ordering::SeqCst);
        }
        if interruption == "phantom" {
            // Even a rapid external change followed by a return to the old
            // value invalidates the original operator confirmation.
            for value in [1, 0] {
                peer.send_to(&message(PHANTOM, &[Arg::Int(value)]), address)
                    .await
                    .unwrap();
            }
        }
        let selected = if interruption == "map" { 16 } else { 1 }; // USB L has no physical preamp
        peer.send_to(&message(PATCH, &[Arg::Int(selected)]), address)
            .await
            .unwrap();
        assert_eq!(
            result.await.unwrap(),
            Err(MixerError::Unavailable),
            "{interruption}"
        );
        if interruption == "map" {
            let state = mixer.snapshot().await;
            assert!(!state.values.contains_key(GAIN));
            assert!(!state.values.contains_key(PHANTOM));
        }
        let mut buf = [0; 512];
        let deadline = Instant::now() + Duration::from_millis(65);
        while let Ok(Ok((n, _))) = time::timeout_at(deadline, peer.recv_from(&mut buf)).await {
            let (key, args) = osc::parse(&buf[..n]).unwrap();
            assert!(
                args.is_empty() || key == "/meters",
                "unexpected write after {interruption}: {key}"
            );
        }
    }
}

#[test]
fn wire_encoding_and_rejection() {
    assert_eq!(message("/xinfo", &[]), b"/xinfo\0\0,\0\0\0");
    assert_eq!(message("/a", &[Arg::Int(1)]), b"/a\0\0,i\0\0\0\0\0\x01");
    assert!(osc::parse(b"/a\0X,i\0\0\0\0\0\x01").is_none());
    assert!(osc::parse(b"/a\0\0,f\0\0\0\0\0").is_none());
    assert!(osc::parse(b"/a\0\0,f\0\0\0\0\0\0x").is_none());
    assert!(osc::parse(b"/a\0\0,b\0\0\xff\xff\xff\xff").is_none());
    let blob = [2, 0, 0, 0, 0, 0xff, 0, 0xf0];
    assert_eq!(osc::meter(&blob, 2), Some(vec![-1., -16.]));
    assert!(osc::meter(&blob, 40).is_none());
}

#[test]
fn supported_parameters_are_typed() {
    let c = Catalog::new();
    assert_eq!(c.order.len(), c.map.len());
    let fader = c.map["/ch/02/mix/fader"];
    assert!(fader.encode(&Value::from("0.5")).is_none());
    assert!(fader.encode(&Value::from(1.001)).is_none());
    assert!(fader.observed(&Arg::Float(f32::NAN)).is_none());
    assert!(fader.observed(&Arg::Int(1)).is_none());
    assert!(c.map["/ch/02/mix/on"].encode(&Value::from(2)).is_none());
    assert!(c.map["/ch/02/mix/01/grpon"]
        .encode(&Value::from(1))
        .is_none());
    assert!(!c.map.contains_key("/ch/02/mix/03/level"));
    assert!(!c.map.contains_key("/ch/02/eq/5/q"));
    assert!(!c.map.contains_key("/rtn/2/dyn/on"));
    assert!(c.map.contains_key("/bus/2/eq/6/g"));
    assert!(c.map.contains_key("/ch/12/mix/10/level"));
}

#[tokio::test]
async fn validates_identity_and_confirms_only_matching_readback() {
    let (fake, mixer, peer) = setup().await;
    let level = "/ch/02/mix/01/level";
    let main = "/ch/02/mix/fader";
    fake.send_to(&hello("XR18"), peer).await.unwrap();
    fake.send_to(&message(level, &[Arg::Float(0.4)]), peer)
        .await
        .unwrap();
    assert_eq!(
        mixer.set(level, Value::from(0.5), "bad").await,
        Err(MixerError::Offline)
    );
    fake.send_to(&hello("XR12"), peer).await.unwrap();
    fake.send_to(&message(main, &[Arg::Float(0.6)]), peer)
        .await
        .unwrap();
    fake.send_to(&message(level, &[Arg::Float(0.4)]), peer)
        .await
        .unwrap();
    let snapshot = snapshot_with(&mixer, level).await;
    assert_eq!(snapshot.identity.unwrap().firmware, "1.26");
    assert_eq!(
        mixer.set(level, Value::from(1.1), &snapshot.epoch).await,
        Err(MixerError::InvalidValue)
    );
    assert_eq!(
        mixer
            .set("/no/such/parameter", Value::Null, &snapshot.epoch)
            .await,
        Err(MixerError::Unavailable)
    );
    assert_eq!(
        mixer.set(level, Value::from(0.5), "different").await,
        Err(MixerError::StaleSession)
    );
    let writer = mixer.clone();
    let epoch = snapshot.epoch.clone();
    let result = tokio::spawn(async move { writer.set(level, Value::from(0.53), &epoch).await });
    let mut data = [0; 256];
    time::timeout(Duration::from_secs(1), async {
        loop {
            let (n, from) = fake.recv_from(&mut data).await.unwrap();
            let (path, args) = osc::parse(&data[..n]).unwrap();
            assert_eq!(from, peer);
            if path == "/meters" || args.is_empty() {
                continue;
            }
            assert_eq!((path, args), (level, vec![Arg::Float(0.53)]));
            break;
        }
    })
    .await
    .unwrap();
    fake.send_to(&message(main, &[Arg::Float(0.61)]), peer)
        .await
        .unwrap();
    fake.send_to(&message(level, &[Arg::Float(0.4)]), peer)
        .await
        .unwrap();
    time::sleep(Duration::from_millis(20)).await;
    assert!(!result.is_finished());
    assert_eq!(
        mixer.set(level, Value::from(0.2), &snapshot.epoch).await,
        Err(MixerError::Busy)
    );
    fake.send_to(&message(level, &[Arg::Float(0.53125)]), peer)
        .await
        .unwrap();
    assert_eq!(result.await.unwrap().unwrap(), Value::from(0.53125));
    assert_eq!(
        mixer.snapshot().await.values[main].value,
        Value::from(0.61f32)
    );
}

#[tokio::test]
async fn no_writes_on_discovery_or_reconnect_and_old_gestures_expire() {
    let (fake, mixer, peer) = setup().await;
    let path = "/ch/01/mix/fader";
    fake.send_to(&hello("XR12"), peer).await.unwrap();
    fake.send_to(&message(path, &[Arg::Float(0.5)]), peer)
        .await
        .unwrap();
    let old = snapshot_with(&mixer, path).await;
    let mut data = [0; 512];
    let next_peer = time::timeout(Duration::from_secs(8), async {
        loop {
            let (n, from) = fake.recv_from(&mut data).await.unwrap();
            let (key, args) = osc::parse(&data[..n]).unwrap();
            assert!(
                args.is_empty() || key == "/meters",
                "Unexpected mutation during monitoring: {key}"
            );
            if from != peer && key == "/xinfo" {
                break from;
            }
        }
    })
    .await
    .unwrap();
    assert!(!mixer.snapshot().await.connected);
    fake.send_to(&hello("XR12"), next_peer).await.unwrap();
    fake.send_to(&message(path, &[Arg::Float(0.75)]), next_peer)
        .await
        .unwrap();
    // An old echo in flight targets the retired socket.
    fake.send_to(&message(path, &[Arg::Float(0.1)]), peer)
        .await
        .unwrap();
    let new = snapshot_with(&mixer, path).await;
    assert_ne!(new.epoch, old.epoch);
    assert_eq!(new.values[path].value, Value::from(0.75));
    assert_eq!(
        mixer.set(path, Value::from(0.8), &old.epoch).await,
        Err(MixerError::StaleSession)
    );
}

#[tokio::test]
async fn independent_send_assignment_restores_only_its_level() {
    let (fake, mixer, peer) = setup().await;
    let level = "/ch/02/mix/01/level";
    let tap = "/ch/02/mix/01/tap";
    let house = "/ch/02/mix/fader";
    // Seed a hardware peer including an independent house mix and normal aux tap.
    fake.send_to(&hello("XR12"), peer).await.unwrap();
    fake.send_to(&message(level, &[Arg::Float(0.4)]), peer)
        .await
        .unwrap();
    fake.send_to(&message(tap, &[Arg::Int(3)]), peer)
        .await
        .unwrap();
    fake.send_to(&message(house, &[Arg::Float(0.6)]), peer)
        .await
        .unwrap();
    let rehearsal = tokio::spawn(async move {
        let mut aux = 0.4;
        let mut changes = Vec::new();
        let mut packet = [0; 512];
        loop {
            let (n, _) = fake.recv_from(&mut packet).await.unwrap();
            let (key, args) = osc::parse(&packet[..n]).unwrap();
            match (key, args.as_slice()) {
                ("/xinfo", []) => {
                    fake.send_to(&hello("XR12"), peer).await.unwrap();
                }
                (k, [Arg::Float(v)]) if k == level => {
                    aux = *v;
                    changes.push(aux);
                    fake.send_to(&message(level, &[Arg::Float(aux)]), peer)
                        .await
                        .unwrap();
                    if changes.len() == 3 {
                        return changes;
                    }
                }
                (k, []) if k == level => {
                    fake.send_to(&message(level, &[Arg::Float(aux)]), peer)
                        .await
                        .unwrap();
                }
                (k, []) if k == tap => {
                    fake.send_to(&message(tap, &[Arg::Int(3)]), peer)
                        .await
                        .unwrap();
                }
                (_, []) | ("/meters", _) => {}
                _ => panic!("assigned the wrong mixer path: {key}"),
            }
        }
    });
    let initial = until(async || {
        let s = mixer.snapshot().await;
        s.send_enabled
            .get(level)
            .is_some_and(|a| a.enabled)
            .then_some(s)
    })
    .await;
    let epoch = initial.epoch;
    assert_eq!(
        mixer.set(level, Value::from(0.375), &epoch).await.unwrap(),
        Value::from(0.375)
    );
    mixer.set_send_enabled(level, false, &epoch).await.unwrap();
    let off = mixer.snapshot().await;
    assert!(!off.send_enabled[level].enabled);
    assert!(off.send_enabled[level].can_enable);
    assert_eq!(off.send_enabled[level].level, 0.375);
    assert_eq!(off.values[level].value, Value::from(0.0));
    assert_eq!(off.values[house].value, Value::from(0.6f32));
    mixer.set_send_enabled(level, true, &epoch).await.unwrap();
    assert_eq!(rehearsal.await.unwrap(), vec![0.375, 0.0, 0.375]);
    assert!(mixer.snapshot().await.send_enabled[level].enabled);
}

#[tokio::test]
async fn zero_send_without_a_remembered_level_stays_silent() {
    let (fake, mixer, peer) = setup().await;
    let key = "/ch/01/mix/02/level";
    fake.send_to(&hello("XR12"), peer).await.unwrap();
    fake.send_to(&message(key, &[Arg::Float(0.0)]), peer)
        .await
        .unwrap();
    fake.send_to(&message("/ch/01/mix/02/tap", &[Arg::Int(3)]), peer)
        .await
        .unwrap();
    let s = until(async || {
        let s = mixer.snapshot().await;
        s.send_enabled.contains_key(key).then_some(s)
    })
    .await;
    assert!(!s.send_enabled[key].can_enable);
    let check = mixer.clone();
    let action = tokio::spawn(async move { check.set_send_enabled(key, true, &s.epoch).await });
    until(async || {
        let mut data = [0; 512];
        let (n, _) = fake.recv_from(&mut data).await.unwrap();
        let (path, args) = osc::parse(&data[..n]).unwrap();
        assert!(args.is_empty() || path == "/meters");
        (path == key).then_some(())
    })
    .await;
    fake.send_to(&message(key, &[Arg::Float(0.)]), peer)
        .await
        .unwrap();
    assert_eq!(action.await.unwrap(), Err(MixerError::Unavailable));
}

async fn request_for(fake: &UdpSocket, key: &str, writing: bool) -> Vec<u8> {
    time::timeout(Duration::from_secs(2), async {
        let mut buf = [0u8; 512];
        loop {
            let (n, _) = fake.recv_from(&mut buf).await.unwrap();
            let (path, args) = osc::parse(&buf[..n]).unwrap();
            if path == key && writing != args.is_empty() {
                return buf[..n].to_vec();
            }
            assert!(
                args.is_empty() || path == "/meters",
                "Unexpected mixer edit on {path}"
            );
        }
    })
    .await
    .expect("expected an OSC packet")
}

#[tokio::test]
async fn native_group_assignment_touches_only_the_group_switch() {
    let (fake, mixer, peer) = setup().await;
    let level = "/ch/02/mix/02/level";
    let group = "/ch/02/mix/02/grpon";
    fake.send_to(&hello("XR12"), peer).await.unwrap();
    for (k, a) in [
        (level, Arg::Float(0.375)),
        ("/ch/02/mix/02/tap", Arg::Int(5)),
        (group, Arg::Int(1)),
    ] {
        fake.send_to(&message(k, &[a]), peer).await.unwrap();
    }
    let s = snapshot_with(&mixer, group).await;
    let writer = mixer.clone();
    let job = tokio::spawn(async move { writer.set_send_enabled(level, false, &s.epoch).await });
    request_for(&fake, group, false).await;
    fake.send_to(&message(group, &[Arg::Int(1)]), peer)
        .await
        .unwrap();
    assert_eq!(
        request_for(&fake, group, true).await,
        message(group, &[Arg::Int(0)])
    );
    fake.send_to(&message(group, &[Arg::Int(0)]), peer)
        .await
        .unwrap();
    assert_eq!(job.await.unwrap(), Ok(Value::from(0)));
    let s = mixer.snapshot().await;
    assert_eq!(s.values[level].value, Value::from(0.375));
    assert!(!s.send_enabled[level].enabled);
    assert_eq!(s.send_enabled[level].mode, "group");
}

#[tokio::test]
async fn context_changes_invalidate_inflight_mutes_before_old_echo() {
    let level = "/ch/02/mix/01/level";
    let tap = "/ch/02/mix/01/tap";
    for (changed_key, external) in [(tap, Arg::Int(4)), (level, Arg::Float(0.8))] {
        let (fake, mixer, peer) = setup().await;
        fake.send_to(&hello("XR12"), peer).await.unwrap();
        fake.send_to(&message(tap, &[Arg::Int(3)]), peer)
            .await
            .unwrap();
        fake.send_to(&message(level, &[Arg::Float(0.5)]), peer)
            .await
            .unwrap();
        let s = snapshot_with(&mixer, level).await;
        let writer = mixer.clone();
        let job =
            tokio::spawn(async move { writer.set_send_enabled(level, false, &s.epoch).await });
        request_for(&fake, level, false).await;
        fake.send_to(&message(level, &[Arg::Float(0.5)]), peer)
            .await
            .unwrap();
        assert_eq!(
            request_for(&fake, level, true).await,
            message(level, &[Arg::Float(0.)])
        );
        fake.send_to(&message(changed_key, &[external]), peer)
            .await
            .unwrap();
        fake.send_to(&message(level, &[Arg::Float(0.)]), peer)
            .await
            .unwrap();
        assert_eq!(job.await.unwrap(), Err(MixerError::Unavailable));
        until(async || {
            let s = mixer.snapshot().await;
            let a = s.send_enabled.get(level)?;
            (!a.enabled && !a.can_enable).then_some(())
        })
        .await;
    }
}

#[tokio::test]
async fn both_scene_indicators_reset_the_send_memory_and_epoch() {
    let key = "/ch/01/mix/02/level";
    let tap = "/ch/01/mix/02/tap";
    for scene in ["/-snap/index", "/snap/index"] {
        let (fake, mixer, peer) = setup().await;
        fake.send_to(&hello("XR12"), peer).await.unwrap();
        for (k, a) in [
            (scene, Arg::Int(1)),
            (tap, Arg::Int(3)),
            (key, Arg::Float(0.55)),
        ] {
            fake.send_to(&message(k, &[a]), peer).await.unwrap();
        }
        let s = snapshot_with(&mixer, key).await;
        let epoch = s.epoch.clone();
        let writer = mixer.clone();
        let task = tokio::spawn(async move { writer.set_send_enabled(key, false, &s.epoch).await });
        request_for(&fake, key, false).await;
        fake.send_to(&message(key, &[Arg::Float(0.55)]), peer)
            .await
            .unwrap();
        request_for(&fake, key, true).await;
        fake.send_to(&message(key, &[Arg::Float(0.)]), peer)
            .await
            .unwrap();
        task.await.unwrap().unwrap();
        assert!(mixer.snapshot().await.send_enabled[key].can_enable);
        fake.send_to(&message(scene, &[Arg::Int(2)]), peer)
            .await
            .unwrap();
        let fresh = until(async || {
            let mut buf = [0; 512];
            let (n, from) = fake.recv_from(&mut buf).await.unwrap();
            let (path, args) = osc::parse(&buf[..n]).unwrap();
            assert!(args.is_empty() || path == "/meters");
            (path == "/xinfo" && from != peer).then_some(from)
        })
        .await;
        fake.send_to(&hello("XR12"), fresh).await.unwrap();
        fake.send_to(&message(tap, &[Arg::Int(3)]), fresh)
            .await
            .unwrap();
        fake.send_to(&message(key, &[Arg::Float(0.)]), fresh)
            .await
            .unwrap();
        let after = snapshot_with(&mixer, key).await;
        assert_ne!(epoch, after.epoch);
        assert!(!after.send_enabled[key].can_enable);
    }
}

#[tokio::test]
async fn processor_writes_are_bound_to_the_observed_algorithm() {
    let (fake, mixer, peer) = setup().await;
    let kind = "/fx/3/type";
    let knob = "/fx/3/par/05";
    fake.send_to(&hello("XR12"), peer).await.unwrap();
    fake.send_to(&message(kind, &[Arg::Int(30)]), peer)
        .await
        .unwrap();
    fake.send_to(&message(knob, &[Arg::Float(0.5)]), peer)
        .await
        .unwrap();
    let s = snapshot_with(&mixer, knob).await;
    assert_eq!(s.effects[&3].name, "LIM");
    assert_eq!(
        mixer.set(knob, Value::from(0.7), &s.epoch).await,
        Err(MixerError::Unavailable)
    );
    assert_eq!(
        mixer
            .set_fx_parameter(3, 0, 5, Value::from(0.7), &s.epoch)
            .await,
        Err(MixerError::Unavailable)
    );
    let writer = mixer.clone();
    let epoch = s.epoch.clone();
    let job = tokio::spawn(async move {
        writer
            .set_fx_parameter(3, 30, 5, Value::from(0.7), &epoch)
            .await
    });
    request_for(&fake, kind, false).await;
    fake.send_to(&message(kind, &[Arg::Int(30)]), peer)
        .await
        .unwrap();
    assert_eq!(
        request_for(&fake, knob, true).await,
        message(knob, &[Arg::Float(0.7)])
    );
    fake.send_to(&message(knob, &[Arg::Float(0.7)]), peer)
        .await
        .unwrap();
    job.await.unwrap().unwrap();
    let writer = mixer.clone();
    let job = tokio::spawn(async move {
        writer
            .compare_and_set(kind, 30.into(), 4.into(), &s.epoch)
            .await
    });
    request_for(&fake, kind, false).await;
    fake.send_to(&message(kind, &[Arg::Int(30)]), peer)
        .await
        .unwrap();
    assert_eq!(
        request_for(&fake, kind, true).await,
        message(kind, &[Arg::Int(4)])
    );
    fake.send_to(&message(kind, &[Arg::Int(4)]), peer)
        .await
        .unwrap();
    assert_eq!(job.await.unwrap().unwrap(), Value::from(4));
    until(async || {
        let s = mixer.snapshot().await;
        (!s.connected && s.values.is_empty()).then_some(())
    })
    .await;
}

#[tokio::test]
async fn lost_mute_acknowledgment_reconciles_without_replaying_a_write() {
    let key = "/ch/01/mix/01/level";
    let tap = "/ch/01/mix/01/tap";
    for scenario in ["delayed confirmation", "external edit", "scene changed"] {
        let (fake, mixer, peer) = setup().await;
        fake.send_to(&hello("XR12"), peer).await.unwrap();
        for (path, arg) in [
            ("/-snap/index", Arg::Int(2)),
            (tap, Arg::Int(3)),
            (key, Arg::Float(0.375)),
        ] {
            fake.send_to(&message(path, &[arg]), peer).await.unwrap();
        }
        let s = snapshot_with(&mixer, key).await;
        let writer = mixer.clone();
        let epoch = s.epoch.clone();
        let job = tokio::spawn(async move { writer.set_send_enabled(key, false, &epoch).await });
        request_for(&fake, key, false).await;
        fake.send_to(&message(key, &[Arg::Float(0.375)]), peer)
            .await
            .unwrap();
        assert_eq!(
            request_for(&fake, key, true).await,
            message(key, &[Arg::Float(0.)])
        );
        request_for(&fake, key, false).await;
        // Mixer applied zero. Its echo and response to the initial GET were lost.
        assert_eq!(job.await.unwrap(), Err(MixerError::ReadbackTimeout));
        request_for(&fake, key, false).await;
        if scenario == "delayed confirmation" {
            let sweep = sweep_time(&Catalog::new());
            let mut probes = 0;
            let start = Instant::now();
            time::timeout(sweep + Duration::from_secs(3), async {
                loop {
                    let mut data = [0; 256];
                    let (n, from) = fake.recv_from(&mut data).await.unwrap();
                    let (path, args) = osc::parse(&data[..n]).unwrap();
                    assert_eq!(from, peer);
                    assert!(
                        args.is_empty() || path == "/meters",
                        "replayed a write on {path}"
                    );
                    if path == "/xinfo" {
                        fake.send_to(&hello("XR12"), peer).await.unwrap();
                    }
                    if path == key {
                        probes += 1;
                        if start.elapsed() >= sweep {
                            break;
                        }
                    }
                }
            })
            .await
            .expect("expected same-session recovery GET after a complete ordinary sweep");
            assert!(probes >= 3, "expected multiple dropped read-only probes");
        }
        match scenario {
            "scene changed" => {
                fake.send_to(&message("/-snap/index", &[Arg::Int(3)]), peer)
                    .await
                    .unwrap();
                let next = until(async || {
                    let mut data = [0; 256];
                    let (n, from) = fake.recv_from(&mut data).await.unwrap();
                    let (path, args) = osc::parse(&data[..n]).unwrap();
                    assert!(args.is_empty() || path == "/meters");
                    (from != peer && path == "/xinfo").then_some(from)
                })
                .await;
                fake.send_to(&hello("XR12"), next).await.unwrap();
                fake.send_to(&message(tap, &[Arg::Int(3)]), next)
                    .await
                    .unwrap();
                fake.send_to(&message(key, &[Arg::Float(0.)]), next)
                    .await
                    .unwrap();
            }
            "external edit" => {
                fake.send_to(&message(key, &[Arg::Float(0.6)]), peer)
                    .await
                    .unwrap();
                fake.send_to(&message(key, &[Arg::Float(0.)]), peer)
                    .await
                    .unwrap();
            }
            _ => {
                fake.send_to(&message(key, &[Arg::Float(0.)]), peer)
                    .await
                    .unwrap();
            }
        }
        let final_state = until(async || {
            let s = mixer.snapshot().await;
            let a = s.send_enabled.get(key)?;
            (!a.enabled).then_some(s)
        })
        .await;
        assert_eq!(
            final_state.send_enabled[key].can_enable,
            scenario == "delayed confirmation"
        );
        if scenario == "delayed confirmation" {
            assert_eq!(final_state.send_enabled[key].level, 0.375);
        }
    }
}

#[tokio::test]
async fn bus_eq_mode_and_insert_target_are_guarded() {
    let (fake, mixer, peer) = setup().await;
    let root = "/bus/1/insert/on";
    let target = "/bus/1/insert/fxslot";
    let eq = "/bus/1/eq/1/g";
    fake.send_to(&hello("XR12"), peer).await.unwrap();
    for (key, arg) in [
        ("/bus/1/eq/mode", Arg::Int(1)),
        (eq, Arg::Float(0.5)),
        ("/fx/1/type", Arg::Int(30)),
        ("/fx/1/insert", Arg::Int(1)),
        (root, Arg::Int(0)),
        (target, Arg::Int(1)),
    ] {
        fake.send_to(&message(key, &[arg]), peer).await.unwrap();
    }
    let s = snapshot_with(&mixer, target).await;
    assert_eq!(
        mixer.set(eq, 0.7.into(), &s.epoch).await,
        Err(MixerError::Unavailable)
    );
    assert_eq!(
        mixer
            .compare_and_set(root, 0.into(), 1.into(), &s.epoch)
            .await,
        Err(MixerError::Unavailable)
    );
    let old = serde_json::json!({"value":0,"fxslot":1,"algorithm":30});
    let writer = mixer.clone();
    let epoch = s.epoch.clone();
    let job =
        tokio::spawn(async move { writer.compare_and_set(root, old, 1.into(), &epoch).await });
    request_for(&fake, target, false).await;
    fake.send_to(&message(target, &[Arg::Int(3)]), peer)
        .await
        .unwrap();
    assert_eq!(job.await.unwrap(), Err(MixerError::Unavailable));
    assert_eq!(mixer.snapshot().await.values[root].value, Value::from(0));
    // Restore original observed routing and verify the full set of read-only
    // preconditions on a valid explicit activation.
    fake.send_to(&message(target, &[Arg::Int(1)]), peer)
        .await
        .unwrap();
    until(async || (mixer.snapshot().await.values[target].value == 1).then_some(())).await;
    let writer = mixer.clone();
    let job = tokio::spawn(async move {
        writer
            .compare_and_set(
                root,
                serde_json::json!({"value":0,"fxslot":1,"algorithm":30}),
                1.into(),
                &s.epoch,
            )
            .await
    });
    for (k, v) in [
        (target, 1),
        ("/fx/1/type", 30),
        ("/fx/1/insert", 1),
        (root, 0),
    ] {
        request_for(&fake, k, false).await;
        fake.send_to(&message(k, &[Arg::Int(v)]), peer)
            .await
            .unwrap();
    }
    assert_eq!(
        request_for(&fake, root, true).await,
        message(root, &[Arg::Int(1)])
    );
    fake.send_to(&message(root, &[Arg::Int(1)]), peer)
        .await
        .unwrap();
    assert_eq!(job.await.unwrap(), Ok(1.into()));
}

#[tokio::test]
async fn captured_rta_preserves_scope_and_source_change_resets_window() {
    let (fake, mixer, peer) = setup().await;
    let mut frames = mixer.subscribe_analyzer();
    fake.send_to(&hello("XR12"), peer).await.unwrap();
    for (path, v) in [("/-stat/rta/source", 1), ("/-stat/rta/pos", 1)] {
        fake.send_to(&message(path, &[Arg::Int(v)]), peer)
            .await
            .unwrap();
    }
    let mut bins = vec![-70i16 * 256; 100];
    let spectrum = |b: &[i16]| {
        let mut blob = (b.len() as u32).to_le_bytes().to_vec();
        for v in b {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        message("/meters/4", &[Arg::Blob(&blob)])
    };
    for n in 0..12 {
        if n > 0 {
            bins[44] = (-52 + n) * 256;
        }
        fake.send_to(&spectrum(&bins), peer).await.unwrap();
        let f = time::timeout(Duration::from_secs(1), frames.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(f.source, Some(1));
        assert_eq!(f.post, Some(true));
        assert_eq!(f.bins_db[44], f32::from(bins[44]) / 256.);
        if n == 11 {
            let r = f.reading.unwrap();
            assert_eq!(r.bin, Some(44));
            assert_eq!(r.frame_count, 12);
            assert!(r.mean_interval_ms.unwrap() > 40. && r.window_ms >= 450);
            assert_eq!(r.candidates.len(), 1);
        }
        time::sleep(Duration::from_millis(50)).await;
    }
    fake.send_to(&message("/-stat/rta/source", &[Arg::Int(0)]), peer)
        .await
        .unwrap();
    fake.send_to(&spectrum(&bins), peer).await.unwrap();
    let frame = frames.recv().await.unwrap();
    assert_eq!(frame.source, Some(0));
    let reading = frame.reading.unwrap();
    assert!(reading.candidates.is_empty());
    assert_eq!(reading.frame_count, 1);
    time::sleep(Duration::from_millis(480)).await;
    assert!(mixer.snapshot().await.analyzer.is_none());
}
