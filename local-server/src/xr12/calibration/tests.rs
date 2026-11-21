use super::*;
use serde_json::json;
use std::io::Cursor;

const NOW: u64 = 1_850_000_000_000;
// Simulated reference measurements. Exactly 31 independent frames per run.
fn records(i: usize) -> Vec<Value> {
    let hz = [0, 940, 940, 1000, 1000, 1060, 1060][i];
    let peak = [0, 39, 39, 40, 40, 41, 41][i];
    let epoch = format!("session-{i}");
    let target = "/ch/01";
    let paths = required_readback(target).unwrap();
    let parameters: serde_json::Map<_, _> = paths
        .iter()
        .map(|k| (k.clone(), json!({"value":0,"age_ms":10})))
        .collect();
    let mut records = vec![
        json!({"kind":"start","host_unix_time_ms":NOW-60_000+5000*i as u64,
       "stimulus_label":if hz==0{"baseline".into()}else{hz.to_string()},"declared_tone_hz":(hz!=0).then_some(hz),
       "expected_post":false,"expected_source":0,"duration_s":3,"readback_target":target}),
    ];
    for n in 0..31 {
        let mut bins = vec![-90.; 100];
        if hz != 0 {
            bins[peak] = -20.;
        }
        records.push(json!({"kind":"frame","data":{"epoch":epoch,"source":0,"post":false,"sample_rate_hz":48000,
          "identity":{"name":"Muted bench","address":"127.0.0.1","model":"XR12","firmware":"1.26"},
          "received_us":n*100_000,"bins_db":bins}}));
    }
    records.push(json!({"kind":"end_readback","epoch":epoch,"parameters":parameters}));
    records.push(json!({"kind":"summary","complete":true,"telemetry_complete":true,"readback_complete":true,
       "readback_target":target,"required_readback_count":paths.len(),"missing_or_stale_readback_paths":[],
       "end_source_tap_fresh":true,"end_frame_age_us":1000,"frames":31,"span_us":3000000,"max_interval_us":100000,
       "receive_gaps_over_450ms":0}));
    records
}
fn read(data: &[Value], legacy: Option<u32>) -> io::Result<Run> {
    let s = data
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    analyze(Cursor::new(s), legacy)
}
fn runs() -> Vec<Run> {
    (0..7).map(|n| read(&records(n), None).unwrap()).collect()
}

#[test]
fn repeated_tones_bracket_only_measured_bins_and_match_only_live_context() {
    let p = Profile::derive(runs(), 3, 0.25, NOW).unwrap();
    assert_eq!(p.frame_count, 217);
    assert_eq!(
        p.ranges,
        vec![BandRange {
            bin: 40,
            low_hz: 939.75,
            high_hz: 1060.25,
            tested_low_hz: 1000.,
            tested_high_hz: 1000.
        }]
    );
    let id = &p.identity;
    assert!(p.ranges_for(id, 0, false, Some(48000), NOW).is_some());
    for mut different in [id.clone(), id.clone(), id.clone()].into_iter().enumerate() {
        match different.0 {
            0 => different.1.firmware = "1.27".into(),
            1 => different.1.address = "127.0.0.2".parse().unwrap(),
            _ => different.1.name = "Another bench".into(),
        }
        assert!(p
            .ranges_for(&different.1, 0, false, Some(48000), NOW)
            .is_none());
    }
    assert!(p.ranges_for(id, 1, false, Some(48000), NOW).is_none());
    assert!(p.ranges_for(id, 0, true, Some(48000), NOW).is_none());
    assert!(p.ranges_for(id, 0, false, None, NOW).is_none());
    assert!(p.ranges_for(id, 0, false, Some(44100), NOW).is_none());
    assert!(p
        .ranges_for(id, 0, false, Some(48000), p.expires_ms)
        .is_none());
    assert!(p
        .ranges_for(id, 0, false, Some(48000), p.latest_capture_ms - 1)
        .is_none());
}

#[test]
fn mismatched_frame_scope_holes_and_poor_tones_invalidate_the_file() {
    let original = records(3);
    for mode in [
        "clock",
        "missing_clock",
        "source",
        "tap",
        "firmware",
        "epoch",
        "gap",
        "ambiguous",
        "clipped",
        "weak",
        "unstable",
        "missing_readback",
        "stale_readback",
        "tail",
        "incomplete",
    ] {
        let mut rows = original.clone();
        match mode {
            "clock" => rows[5]["data"]["sample_rate_hz"] = 44100.into(),
            "missing_clock" => rows[5]["data"]["sample_rate_hz"] = Value::Null,
            "source" => rows[6]["data"]["source"] = 1.into(),
            "tap" => rows[6]["data"]["post"] = true.into(),
            "firmware" => rows[6]["data"]["identity"]["firmware"] = "unknown".into(),
            "epoch" => rows[6]["data"]["epoch"] = "other".into(),
            "gap" => {
                rows.drain(9..15);
            }
            "missing_readback" => {
                rows[32]["parameters"]
                    .as_object_mut()
                    .unwrap()
                    .remove("/ch/01/eq/1/q");
            }
            "stale_readback" => rows[32]["parameters"]["/ch/01/eq/1/q"]["age_ms"] = 5000.into(),
            "tail" => rows[33]["end_frame_age_us"] = 500000.into(),
            "incomplete" => rows[33]["complete"] = false.into(),
            _ => {
                for (n, row) in rows[1..32].iter_mut().enumerate() {
                    let bins = &mut row["data"]["bins_db"];
                    match mode {
                        "ambiguous" => bins[47] = (-23).into(),
                        "clipped" => bins[40] = 0.into(),
                        "weak" => bins[40] = (-68).into(),
                        "unstable" if n % 3 == 0 => {
                            bins[40] = (-90).into();
                            bins[43] = (-19).into();
                        }
                        _ => {}
                    }
                }
            }
        }
        assert!(read(&rows, None).is_err(), "{mode}");
    }
    let mut legacy = original.clone();
    for row in &mut legacy[1..32] {
        row["data"]
            .as_object_mut()
            .unwrap()
            .remove("sample_rate_hz");
    }
    assert!(read(&legacy, None).is_err());
    assert!(read(&legacy, Some(48000)).is_ok());
    assert!(read(&original, Some(48000)).is_err());
}

#[test]
fn missing_repeats_insufficient_baseline_clearance_and_foreign_measurements_fail() {
    for mode in [
        "single",
        "overlap",
        "disagreement",
        "foreign",
        "unbracketed",
        "baseline",
        "aged",
        "future",
        "reused_epoch",
    ] {
        let mut records = runs();
        match mode {
            "single" => {
                records.pop();
            }
            "overlap" => records[1].time_ms = records[0].time_ms + 40,
            "disagreement" => records[3].evidence.peak_bin = Some(42),
            "foreign" => records[2].context.identity.name = "other".into(),
            "unbracketed" => {
                for r in &mut records[5..] {
                    r.evidence.peak_bin = Some(42);
                }
            }
            "baseline" => records[0].baseline_max[40] = -28.,
            "aged" => records[0].time_ms -= 31 * DAY_MS,
            "future" => records[6].time_ms = NOW + 10,
            _ => records[1].epoch = records[0].epoch.clone(),
        }
        assert!(Profile::derive(records, 3, 0.25, NOW).is_err(), "{mode}");
    }
}
