//! Numeric control ranges from the XR12 firmware 1.14 parameter table. Values
//! are usable only after the connected mixer reports the relevant OSC address.
use serde::Serialize;
use std::sync::LazyLock;

#[derive(Serialize, Debug)]
pub struct FxParameter {
    pub name: &'static str,
    pub min: f64,
    pub max: f64,
    /// A negative step count specifies a logarithmic display axis.
    pub steps: i16,
    pub unit: &'static str,
}

#[derive(Serialize, Debug)]
pub struct Effect {
    pub algorithm: u8,
    pub name: &'static str,
    pub parameters: Vec<FxParameter>,
}

pub(super) static ALGORITHMS: LazyLock<Vec<Effect>> = LazyLock::new(|| {
    // This is a build-time embedded data file. The catalog integrity test verifies
    // every row, including firmware aliases for PQ52 and FAC1M.
    let mut all: Vec<Effect> = Vec::new();
    for line in include_str!("fx-parameters.tsv").lines() {
        let Some((name, range)) = line.split_once('\t') else {
            let algorithm = all.len() as u8;
            let name = match algorithm {
                47 => "PQ52",
                49 => "FAC1M",
                _ => line,
            };
            all.push(Effect {
                algorithm,
                name,
                parameters: Vec::new(),
            });
            continue;
        };
        let mut fields = range.split('\t');
        let mut next = || fields.next().expect("invalid embedded XR12 parameter row");
        let min = next().parse().expect("invalid embedded minimum");
        let max = next().parse().expect("invalid embedded maximum");
        let steps = next().parse().expect("invalid embedded step count");
        let unit = next();
        all.last_mut()
            .expect("missing embedded XR12 algorithm")
            .parameters
            .push(FxParameter {
                name,
                min,
                max,
                steps,
                unit,
            });
    }
    all
});

pub fn fx_parameters(algorithm: u8) -> &'static [FxParameter] {
    ALGORITHMS
        .get(algorithm as usize)
        .map_or(&[], |a| &a.parameters)
}

pub(super) fn key(address: &str) -> Option<(u8, u8)> {
    let mut fields = address.strip_prefix("/fx/")?.split('/');
    let slot = fields.next()?.parse().ok()?;
    if fields.next() != Some("par") {
        return None;
    }
    let index = fields.next()?.parse().ok()?;
    (fields.next().is_none() && (1..=4).contains(&slot) && (1..=64).contains(&index))
        .then_some((slot, index))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_embedded_firmware_ranges() {
        assert_eq!(ALGORITHMS.len(), 61);
        let mut count = 0;
        for effect in ALGORITHMS.iter() {
            assert!(!effect.name.is_empty());
            assert!((1..=64).contains(&effect.parameters.len()));
            for p in &effect.parameters {
                assert!(!p.name.is_empty() && p.min < p.max && p.steps.unsigned_abs() >= 2);
                assert!(p.steps > 0 || (p.min > 0.0 && p.max > 0.0));
                count += 1;
            }
        }
        assert_eq!(count, 898);
        let limiter = &ALGORITHMS[30];
        assert_eq!(limiter.name, "LIM");
        assert_eq!(limiter.parameters[5].name, "Release");
        assert_eq!(limiter.parameters[5].max, 2000.0);
    }
}
