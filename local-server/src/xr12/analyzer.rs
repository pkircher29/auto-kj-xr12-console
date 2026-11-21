//! Monitor one received XR12 spectrum at a time. The shared bank carries neither
//! a per-frame source nor calibrated bin centers. Source and tap labels come
//! from separate contemporaneous device reads and must remain advisory.
use serde::Serialize;
use tokio::time::{Duration, Instant};

pub const BIN_COUNT: usize = 100;
pub const MAX_GAP: Duration = Duration::from_millis(450);

#[derive(Clone, Debug, Serialize)]
pub struct BenchFrame {
    #[serde(skip)]
    pub received_at: Instant,
    pub epoch: String,
    pub identity: super::MixerIdentity,
    /// Monotonic host receive offset within this connection, not a mixer timestamp.
    pub received_us: u64,
    pub source: Option<u8>,
    pub post: Option<bool>,
    pub sample_rate_hz: Option<u32>,
    pub bins_db: Vec<f32>,
    pub reading: Option<Reading>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    pub bin: usize,
    pub level_db: f32,
    pub prominence_db: f32,
    pub above_floor_db: f32,
    pub rise_db: f32,
    pub duration_ms: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct Reading {
    pub source: u8,
    pub post: bool,
    pub bin: Option<usize>,
    pub competing: bool,
    pub candidates: Vec<Candidate>,
    pub window_ms: u64,
    pub age_ms: u64,
    pub frame_count: u64,
    pub mean_interval_ms: Option<f64>,
    pub max_interval_ms: u64,
}

#[derive(Clone, Copy)]
struct Peak {
    start: Instant,
    last_growth: Instant,
    first: f32,
    highest: f32,
    rises: u16,
}

pub struct Detector {
    context: (u8, bool),
    start: Instant,
    last: Instant,
    count: u64,
    max_gap: Duration,
    floor: [f32; BIN_COUNT],
    peaks: [Option<Peak>; BIN_COUNT],
    candidates: Vec<Candidate>,
}

impl Detector {
    // Callers validate bank width and sample encoding at the OSC boundary.
    pub fn new(source: u8, post: bool, frame: &[f32; BIN_COUNT], now: Instant) -> Self {
        let mut floor = [0.; BIN_COUNT];
        floor.copy_from_slice(frame);
        Self {
            context: (source, post),
            start: now,
            last: now,
            count: 1,
            max_gap: Duration::ZERO,
            floor,
            peaks: [None; BIN_COUNT],
            candidates: Vec::new(),
        }
    }

    pub fn update(&mut self, source: u8, post: bool, frame: &[f32; BIN_COUNT], now: Instant) {
        let elapsed = now.saturating_duration_since(self.last);
        if self.context != (source, post) || elapsed >= MAX_GAP || now < self.last {
            *self = Self::new(source, post, frame, now);
            return;
        }
        if elapsed.is_zero() {
            return;
        }
        self.max_gap = self.max_gap.max(elapsed);
        self.last = now;
        self.count = self.count.saturating_add(1);
        self.candidates.clear();
        let floor_weight = 1. - (-elapsed.as_secs_f32() / 1.2).exp();
        for i in 2..BIN_COUNT - 2 {
            let level = frame[i];
            let neighbors = frame[i - 2]
                .max(frame[i - 1])
                .max(frame[i + 1])
                .max(frame[i + 2]);
            let prominence = level - neighbors;
            let above_floor = level - self.floor[i];
            if level <= -64. || prominence < 10. || above_floor < 6. {
                self.peaks[i] = None;
                self.floor[i] += (level - self.floor[i]) * floor_weight;
                continue;
            }
            let p = self.peaks[i].get_or_insert(Peak {
                start: now,
                last_growth: now,
                first: level,
                highest: level,
                rises: 0,
            });
            if level >= p.highest + 0.75 {
                p.highest = level;
                p.last_growth = now;
                p.rises = p.rises.saturating_add(1);
            }
            let duration = now.saturating_duration_since(p.start);
            if duration >= Duration::from_millis(450)
                && p.last_growth.duration_since(p.start) >= Duration::from_millis(300)
                && p.rises >= 3
                && level - p.first >= 4.
            {
                self.candidates.push(Candidate {
                    bin: i,
                    level_db: level,
                    prominence_db: prominence,
                    above_floor_db: above_floor,
                    rise_db: level - p.first,
                    duration_ms: duration.as_millis() as u64,
                });
            }
        }
    }
    pub fn reading(&self, source: u8, post: bool) -> Option<Reading> {
        self.reading_at(source, post, Instant::now())
    }
    pub fn reading_at(&self, source: u8, post: bool, now: Instant) -> Option<Reading> {
        let age = now.saturating_duration_since(self.last);
        let window = self.last.saturating_duration_since(self.start);
        (self.context == (source, post) && age < MAX_GAP).then(|| Reading {
            source,
            post,
            bin: if let [one] = self.candidates.as_slice() {
                Some(one.bin)
            } else {
                None
            },
            competing: self.candidates.len() > 1,
            candidates: self.candidates.clone(),
            frame_count: self.count,
            window_ms: window.as_millis() as u64,
            age_ms: age.as_millis() as u64,
            mean_interval_ms: (self.count > 1)
                .then(|| window.as_secs_f64() * 1000. / (self.count - 1) as f64),
            max_interval_ms: self.max_gap.as_millis() as u64,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn isolated_growth_marks_one_bin_and_ambiguous_or_steady_tones_do_not() {
        let now = Instant::now();
        let floor = [-70.; 100];
        let mut d = Detector::new(1, false, &floor, now);
        for frame in 1..=12 {
            let mut bins = floor;
            bins[43] = -52. + frame as f32;
            bins[19] = -35.;
            d.update(1, false, &bins, now + Duration::from_millis(frame * 70));
        }
        assert_eq!(
            d.reading_at(d.context.0, d.context.1, d.last).unwrap().bin,
            Some(43)
        );
        for frame in 13..=24 {
            let mut bins = floor;
            bins[43] = -35.;
            bins[63] = -50. + (frame - 13) as f32;
            d.update(1, false, &bins, now + Duration::from_millis(frame * 70));
        }
        assert!(
            d.reading_at(d.context.0, d.context.1, d.last)
                .unwrap()
                .competing
        );
        assert_eq!(
            d.reading_at(d.context.0, d.context.1, d.last).unwrap().bin,
            None
        );
        d.update(2, false, &floor, now + Duration::from_secs(2));
        assert_eq!(
            d.reading_at(d.context.0, d.context.1, d.last).unwrap().bin,
            None
        );
        assert!(
            !d.reading_at(d.context.0, d.context.1, d.last)
                .unwrap()
                .competing
        );
    }
}
