//! A partial response map derived from controlled electrical tone measurements.
//! Every displayed interval must be bracketed by measured neighboring bins.
use super::{analyzer::BIN_COUNT, MixerIdentity};
#[cfg(test)]
mod tests;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashSet},
    fs::File,
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const DAY_MS: u64 = 86_400_000;
const GAP_US: u64 = 450_000;

fn invalid(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s)
}
pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    valid_for_days: u32,
    generator_tolerance_hz: f64,
    captures: Vec<Entry>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    file: PathBuf,
    /// Only for recorder versions preceding a per-frame clock field.
    legacy_observed_sample_rate_hz: Option<u32>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct BandRange {
    pub bin: usize,
    pub low_hz: f64,
    pub high_hz: f64,
    pub tested_low_hz: f64,
    pub tested_high_hz: f64,
}
#[derive(Serialize, Clone, Debug)]
pub struct Profile {
    identity: MixerIdentity,
    source: u8,
    post: bool,
    sample_rate_hz: u32,
    oldest_capture_ms: u64,
    latest_capture_ms: u64,
    expires_ms: u64,
    generator_tolerance_hz: f64,
    capture_count: usize,
    frame_count: u64,
    captures: Vec<Evidence>,
    ranges: Vec<BandRange>,
}
#[derive(Serialize, Clone, Debug)]
struct Evidence {
    tone_hz: Option<f64>,
    peak_bin: Option<usize>,
    frames: u64,
    span_us: u64,
    max_interval_us: u64,
    agreeing_frames: u64,
}
#[derive(Eq, PartialEq)]
struct Context {
    identity: MixerIdentity,
    source: u8,
    post: bool,
    rate: u32,
    target: String,
}
struct Run {
    context: Context,
    time_ms: u64,
    epoch: String,
    evidence: Evidence,
    peak_floor: f64,
    baseline_max: [f64; BIN_COUNT],
}

#[derive(Deserialize)]
struct Start {
    host_unix_time_ms: u64,
    stimulus_label: String,
    declared_tone_hz: Option<f64>,
    expected_source: u8,
    expected_post: bool,
    duration_s: u64,
    readback_target: String,
}
#[derive(Deserialize)]
struct Frame {
    epoch: String,
    identity: MixerIdentity,
    received_us: u64,
    source: u8,
    post: bool,
    bins_db: Vec<f64>,
}
#[derive(Deserialize)]
struct Summary {
    complete: bool,
    telemetry_complete: bool,
    readback_complete: bool,
    readback_target: String,
    required_readback_count: u64,
    missing_or_stale_readback_paths: Vec<String>,
    end_source_tap_fresh: bool,
    end_frame_age_us: u64,
    frames: u64,
    span_us: u64,
    max_interval_us: u64,
    receive_gaps_over_450ms: u64,
}

pub fn required_readback(target: &str) -> Option<Vec<String>> {
    let input = (1..=12).any(|n| target == format!("/ch/{n:02}"));
    if !input && !matches!(target, "/bus/1" | "/bus/2" | "/lr") {
        return None;
    }
    let mut paths = vec![
        format!("{target}/eq/on"),
        format!("{target}/mix/fader"),
        format!("{target}/mix/on"),
    ];
    if input {
        paths.push(format!("{target}/mix/lr"));
        for aux in 1..=2 {
            paths.push(format!("{target}/mix/{aux:02}/level"));
            paths.push(format!("{target}/mix/{aux:02}/tap"));
        }
    } else {
        paths.push(format!("{target}/eq/mode"));
    }
    for band in 1..=if input { 4 } else { 6 } {
        for property in ["type", "f", "g", "q"] {
            paths.push(format!("{target}/eq/{band}/{property}"));
        }
    }
    for aux in 1..=2 {
        paths.push(format!("/routing/aux/{aux:02}/src"));
        paths.push(format!("/routing/aux/{aux:02}/pos"));
    }
    Some(paths)
}

fn analyze(reader: impl BufRead, legacy: Option<u32>) -> io::Result<Run> {
    let mut start: Option<Start> = None;
    let mut context: Option<Context> = None;
    let mut epoch = String::new();
    let (mut first, mut last, mut gap, mut frames) = (0u64, 0u64, 0u64, 0u64);
    let mut votes = [0u64; BIN_COUNT];
    let mut minima = [f64::INFINITY; BIN_COUNT];
    let mut maxima = [f64::NEG_INFINITY; BIN_COUNT];
    let mut end_count = None;
    let mut summary: Option<Summary> = None;
    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        if line.len() > 1024 * 1024 {
            return Err(invalid("oversized capture record"));
        }
        let doc: Value = serde_json::from_str(&line)?;
        match doc["kind"].as_str() {
            Some("start") if index == 0 => {
                let s: Start = serde_json::from_value(doc)?;
                let valid_label = match s.declared_tone_hz {
                    Some(hz) => {
                        (20.0..=24_000.).contains(&hz)
                            && s.stimulus_label.parse::<f64>().ok() == Some(hz)
                    }
                    None => s.stimulus_label == "baseline",
                };
                if !valid_label || !(3..=120).contains(&s.duration_s) || s.expected_source > 32 {
                    return Err(invalid(
                        "expected a 3–120 second labeled reference tone or baseline",
                    ));
                }
                start = Some(s);
            }
            Some("frame") if start.is_some() && end_count.is_none() && summary.is_none() => {
                let s = start.as_ref().unwrap();
                let data = &doc["data"];
                let rate = if let Some(r) = data.get("sample_rate_hz") {
                    // A null wire observation is unknown. A legacy annotation
                    // cannot override a recorded clock value or a missing reply.
                    if legacy.is_some() {
                        return Err(invalid(
                            "legacy rate cannot override recorder clock evidence",
                        ));
                    }
                    r.as_u64().and_then(|n| u32::try_from(n).ok())
                } else {
                    legacy
                };
                let rate = rate
                    .filter(|r| matches!(r, 44_100 | 48_000))
                    .ok_or_else(|| invalid("missing sample-clock evidence"))?;
                let f: Frame = serde_json::from_value(data.clone())?;
                if !f.identity.model.eq_ignore_ascii_case("XR12")
                    || f.identity.firmware.is_empty()
                    || f.identity.name.is_empty()
                    || f.identity.address.is_unspecified()
                    || f.identity.address.is_multicast()
                    || f.identity.address.is_broadcast()
                    || f.source != s.expected_source
                    || f.post != s.expected_post
                    || f.bins_db.len() != BIN_COUNT
                    || f.bins_db.iter().any(|&v| !(-128.0..128.).contains(&v))
                    || f.epoch.is_empty()
                {
                    return Err(invalid("invalid mixer identity, scope or spectrum"));
                }
                let current = Context {
                    identity: f.identity,
                    source: f.source,
                    post: f.post,
                    rate,
                    target: s.readback_target.clone(),
                };
                if frames == 0 {
                    first = f.received_us;
                    epoch = f.epoch.clone();
                    context = Some(current);
                } else if context.as_ref() != Some(&current)
                    || epoch != f.epoch
                    || f.received_us <= last
                    || f.received_us - last >= GAP_US
                {
                    return Err(invalid(
                        "changed mixer, source, clock, epoch, or discontinuous frame sequence",
                    ));
                } else {
                    gap = gap.max(f.received_us - last);
                }
                frames += 1;
                last = f.received_us;
                let mut best = (0, f64::NEG_INFINITY);
                let mut second = f64::NEG_INFINITY;
                for (i, &db) in f.bins_db.iter().enumerate() {
                    maxima[i] = maxima[i].max(db);
                    if db > best.1 {
                        second = best.1;
                        best = (i, db);
                    } else {
                        second = second.max(db);
                    }
                }
                if best.1 > -64. && best.1 < -1. && best.1 - second >= 6. {
                    votes[best.0] += 1;
                    minima[best.0] = minima[best.0].min(best.1);
                }
            }
            Some("end_readback") if frames > 0 && end_count.is_none() && summary.is_none() => {
                if doc["epoch"].as_str() != Some(&epoch) {
                    return Err(invalid("readback belongs to another session"));
                }
                let parameters = doc["parameters"]
                    .as_object()
                    .ok_or_else(|| invalid("missing control readback"))?;
                let s = start.as_ref().unwrap();
                let mut paths = required_readback(&s.readback_target)
                    .ok_or_else(|| invalid("unknown readback target"))?;
                if s.readback_target.starts_with("/ch/") {
                    for aux in 1..=2 {
                        let prefix = format!("{}/mix/{aux:02}", s.readback_target);
                        if parameters
                            .get(&format!("{prefix}/tap"))
                            .and_then(|v| v["value"].as_i64())
                            == Some(5)
                        {
                            paths.push(format!("{prefix}/grpon"));
                        }
                    }
                }
                let max_age_ms = 20_000.min(s.duration_s * 1000);
                if paths.iter().any(|p| {
                    parameters.get(p).is_none_or(|v| {
                        v["value"].as_f64().is_none()
                            || v["age_ms"].as_u64().is_none_or(|t| t > max_age_ms)
                    })
                }) {
                    return Err(invalid("missing or stale final EQ/routing evidence"));
                }
                end_count = Some(paths.len() as u64);
            }
            Some("summary") if end_count.is_some() && summary.is_none() => {
                summary = Some(serde_json::from_value(doc)?);
            }
            _ => return Err(invalid("missing, unordered or unexpected capture record")),
        }
    }
    let (Some(s), Some(context), Some(summary)) = (start, context, summary) else {
        return Err(invalid("incomplete capture"));
    };
    let span = last - first;
    if frames < 20
        || !(summary.complete
            && summary.telemetry_complete
            && summary.readback_complete
            && summary.end_source_tap_fresh)
        || summary.readback_target != s.readback_target
        || !summary.missing_or_stale_readback_paths.is_empty()
        || end_count != Some(summary.required_readback_count)
        || summary.receive_gaps_over_450ms != 0
        || summary.end_frame_age_us >= GAP_US
        || summary.max_interval_us != gap
        || summary.frames != frames
        || summary.span_us != span
        || span.abs_diff(s.duration_s * 1_000_000) >= GAP_US
    {
        return Err(invalid(
            "insufficient frame coverage or mismatched recorder summary",
        ));
    }
    let best = votes.iter().enumerate().max_by_key(|(_, v)| *v).unwrap().0;
    if s.declared_tone_hz.is_some() && votes[best] * 10 < frames * 9 {
        return Err(invalid(
            "ambiguous, clipped, weak or unstable reference tone",
        ));
    }
    let evidence = Evidence {
        tone_hz: s.declared_tone_hz,
        peak_bin: s.declared_tone_hz.map(|_| best),
        frames,
        span_us: span,
        max_interval_us: gap,
        agreeing_frames: votes[best],
    };
    Ok(Run {
        context,
        time_ms: s.host_unix_time_ms,
        epoch,
        evidence,
        peak_floor: minima[best],
        baseline_max: maxima,
    })
}

impl Profile {
    /// Rebuild from the underlying read-only recordings each time. No editable
    /// frequency lookup table is accepted as a substitute for the measurements.
    pub fn load(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        if file.metadata()?.len() > 1024 * 1024 {
            return Err(invalid("oversized manifest"));
        }
        let manifest: Manifest = serde_json::from_reader(file)?;
        if manifest.version != 1 || !(7..=1024).contains(&manifest.captures.len()) {
            return Err(invalid(
                "expected a version 1 manifest with a baseline and repeated tones",
            ));
        }
        let base = path.parent().unwrap_or(Path::new("."));
        let mut total = 0u64;
        let mut paths = HashSet::new();
        let mut runs = Vec::new();
        for e in &manifest.captures {
            let file = base.join(&e.file).canonicalize()?;
            if !paths.insert(file.clone()) {
                return Err(invalid("duplicate recording"));
            }
            let input = File::open(&file)?;
            let size = input.metadata()?.len();
            total = total.saturating_add(size);
            if size > 32 * 1024 * 1024 || total > 1024 * 1024 * 1024 {
                return Err(invalid("capture set too large"));
            }
            runs.push(
                analyze(
                    BufReader::new(input.take(size)),
                    e.legacy_observed_sample_rate_hz,
                )
                .map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", file.display())))?,
            );
        }
        Self::derive(
            runs,
            manifest.valid_for_days,
            manifest.generator_tolerance_hz,
            now_ms(),
        )
    }
    fn derive(mut runs: Vec<Run>, days: u32, tolerance: f64, now: u64) -> io::Result<Self> {
        if !(1..=30).contains(&days)
            || !(0.0..=10.).contains(&tolerance)
            || tolerance == 0.
            || runs.is_empty()
        {
            return Err(invalid("invalid expiry or generator uncertainty"));
        }
        runs.sort_by_key(|r| r.time_ms);
        if runs.windows(2).any(|pair| {
            pair[0]
                .time_ms
                .saturating_add(pair[0].evidence.span_us.div_ceil(1000))
                > pair[1].time_ms
        }) {
            return Err(invalid("reference captures overlap in time"));
        }
        let first = &runs[0].context;
        let mut epochs = BTreeSet::new();
        let mut baseline = None::<[f64; BIN_COUNT]>;
        let oldest = runs.iter().map(|r| r.time_ms).min().unwrap();
        let latest = runs
            .iter()
            .map(|r| r.time_ms.saturating_add(r.evidence.span_us.div_ceil(1000)))
            .max()
            .unwrap();
        let expires = oldest.saturating_add(u64::from(days) * DAY_MS);
        for r in &runs {
            if r.context != *first
                || !epochs.insert(r.epoch.clone())
                || now < latest
                || now >= expires
            {
                return Err(invalid(
                    "foreign, duplicate, expired or future-dated recording",
                ));
            }
            if r.evidence.tone_hz.is_none() {
                let max = baseline.get_or_insert([f64::NEG_INFINITY; BIN_COUNT]);
                for (m, &value) in max.iter_mut().zip(&r.baseline_max) {
                    *m = m.max(value);
                }
            }
        }
        let baseline = baseline.ok_or_else(|| invalid("missing baseline"))?;
        let identity = first.identity.clone();
        let (source, post, rate) = (first.source, first.post, first.rate);
        runs.sort_by(|a, b| {
            a.evidence
                .tone_hz
                .unwrap_or(0.)
                .total_cmp(&b.evidence.tone_hz.unwrap_or(0.))
        });
        let mut tones = Vec::<(f64, usize, usize)>::new();
        for r in &runs {
            let (Some(hz), Some(bin)) = (r.evidence.tone_hz, r.evidence.peak_bin) else {
                continue;
            };
            if hz + tolerance >= f64::from(rate) / 2. || r.peak_floor - baseline[bin] < 12. {
                return Err(invalid(
                    "tone exceeds Nyquist or lacks 12 dB clearance above baseline",
                ));
            }
            if let Some((prev, old_bin, repeats)) = tones.last_mut() {
                if hz == *prev {
                    if *old_bin != bin {
                        return Err(invalid("repeat tone disagrees on dominant bin"));
                    }
                    *repeats += 1;
                    continue;
                }
                if bin < *old_bin || hz - *prev <= 2. * tolerance {
                    return Err(invalid("nonmonotonic or overlapping reference tones"));
                }
            }
            tones.push((hz, bin, 1));
        }
        if tones.iter().any(|(_, _, n)| *n < 2) {
            return Err(invalid(
                "each frequency needs two independent recording sessions",
            ));
        }
        let mut ranges = Vec::new();
        for bin in 1..BIN_COUNT - 1 {
            let Some(lo) = tones.iter().rev().find(|t| t.1 == bin - 1) else {
                continue;
            };
            let Some(hi) = tones.iter().find(|t| t.1 == bin + 1) else {
                continue;
            };
            let mut inside = tones.iter().filter(|t| t.1 == bin);
            let Some(center) = inside.next() else {
                continue;
            };
            let last = inside.next_back().unwrap_or(center);
            ranges.push(BandRange {
                bin,
                low_hz: lo.0 - tolerance,
                high_hz: hi.0 + tolerance,
                tested_low_hz: center.0,
                tested_high_hz: last.0,
            });
        }
        if ranges.is_empty() {
            return Err(invalid(
                "no bin bracketed by measurements in both adjacent bins",
            ));
        }
        Ok(Self {
            identity,
            source,
            post,
            sample_rate_hz: rate,
            oldest_capture_ms: oldest,
            latest_capture_ms: latest,
            expires_ms: expires,
            generator_tolerance_hz: tolerance,
            capture_count: runs.len(),
            frame_count: runs.iter().map(|r| r.evidence.frames).sum(),
            captures: runs.into_iter().map(|r| r.evidence).collect(),
            ranges,
        })
    }
    pub(super) fn ranges_for(
        &self,
        id: &MixerIdentity,
        source: u8,
        post: bool,
        rate: Option<u32>,
        at: u64,
    ) -> Option<&[BandRange]> {
        (id == &self.identity
            && source == self.source
            && post == self.post
            && rate == Some(self.sample_rate_hz)
            && (self.latest_capture_ms..self.expires_ms).contains(&at))
        .then_some(&self.ranges)
    }
}
