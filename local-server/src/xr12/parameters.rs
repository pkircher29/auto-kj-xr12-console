//! Fixed addresses from the manufacturer's X AIR Parameters.txt (XR12 columns).
//! No operator-supplied path is forwarded to the device.
use super::osc::Arg;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Copy, Clone)]
pub(super) enum Kind {
    Steps(u16),
    Integer(i32),
    Name,
}

#[derive(Clone, Copy)]
pub(super) struct Parameter {
    pub kind: Kind,
    pub writable: bool,
}

impl Parameter {
    pub fn observed(&self, arg: &Arg<'_>) -> Option<Value> {
        match (self.kind, arg) {
            (Kind::Steps(_), Arg::Float(v)) if (0.0..=1.0).contains(v) => Some(Value::from(*v)),
            (Kind::Integer(max), Arg::Int(v)) if (0..=max).contains(v) => Some(Value::from(*v)),
            (Kind::Name, Arg::Text(s)) if s.len() <= 64 && !s.chars().any(char::is_control) => {
                Some(Value::from(*s))
            }
            _ => None,
        }
    }

    pub fn encode(&self, value: &Value) -> Option<Arg<'static>> {
        if !self.writable {
            return None;
        }
        match self.kind {
            Kind::Steps(_) => {
                let v = value.as_f64()?;
                (0.0..=1.0).contains(&v).then_some(Arg::Float(v as f32))
            }
            Kind::Integer(max) => {
                let v = i32::try_from(value.as_i64()?).ok()?;
                (0..=max).contains(&v).then_some(Arg::Int(v))
            }
            Kind::Name => None,
        }
    }

    pub fn matches(&self, target: &Value, actual: &Value) -> bool {
        if let Kind::Steps(steps) = self.kind {
            // Silence and full travel are exact endpoint values.
            if matches!(target.as_f64(), Some(0.0 | 1.0)) {
                return target.as_f64() == actual.as_f64();
            }
            return target
                .as_f64()
                .zip(actual.as_f64())
                .is_some_and(|(a, b)| (a - b).abs() <= 0.501 / f64::from(steps - 1));
        }
        target == actual
    }
}

pub(super) struct Catalog {
    pub map: BTreeMap<String, Parameter>,
    pub order: Vec<String>,
}

impl Catalog {
    fn add(&mut self, root: &str, suffix: &str, kind: Kind, writable: bool) {
        let key = format!("{root}/{suffix}");
        self.map.insert(key.clone(), Parameter { kind, writable });
        self.order.push(key);
    }

    pub fn new() -> Self {
        use Kind::{Integer, Name, Steps};
        let mut result = Self {
            map: BTreeMap::new(),
            order: Vec::new(),
        };
        let inputs: Vec<_> = (1..=12).map(|i| format!("/ch/{i:02}")).collect();
        let returns: Vec<_> = (1..=4).map(|i| format!("/rtn/{i}")).collect();
        let mut outputs: Vec<_> = (1..=2).map(|i| format!("/bus/{i}")).collect();
        outputs.push("/lr".into());
        let sends: Vec<_> = (1..=4).map(|i| format!("/fxsend/{i}")).collect();
        // Fetch surface values before channel processing on initial connection.
        result.add("/-snap", "index", Integer(64), false);
        result.add("/-snap", "name", Name, false);
        result.add("/snap", "index", Integer(64), false);
        result.add("/snap", "name", Name, false);
        result.add("/-stat/rta", "source", Integer(32), false);
        result.add("/-stat/rta", "pos", Integer(1), false);
        result.add("/-prefs", "clockrate", Integer(1), false);
        for root in &inputs {
            result.add(root, "config/insrc", Integer(18), false);
        }
        for input in 1..=4 {
            let root = format!("/headamp/{input:02}");
            result.add(&root, "gain", Steps(145), false);
            result.add(&root, "phantom", Integer(1), false);
        }
        for root in inputs.iter().chain(&outputs).chain(&returns).chain(&sends) {
            result.add(root, "mix/fader", Steps(1024), true);
            result.add(root, "mix/on", Integer(1), true);
            result.add(root, "config/name", Name, false);
        }
        for root in inputs.iter().chain(&returns).chain(&outputs) {
            result.add(root, "mix/pan", Steps(101), true);
            if root != "/lr" {
                result.add(root, "mix/lr", Integer(1), true);
            }
        }
        for root in inputs.iter().chain(&returns) {
            let buses: &[u8] = if root.starts_with("/ch/") {
                &[1, 2, 7, 8, 9, 10]
            } else {
                &[1, 2]
            };
            for bus in buses {
                let prefix = format!("{root}/mix/{bus:02}");
                result.add(&prefix, "level", Steps(161), true);
                result.add(&prefix, "tap", Integer(5), false);
                // Group mode has distinct routing semantics. Observe this flag;
                // never reinterpret it as a universal aux-send mute.
                result.add(&prefix, "grpon", Integer(1), false);
            }
        }
        for i in (1..=11).step_by(2) {
            result.add(
                "/config/chlink",
                &format!("{i}-{}", i + 1),
                Integer(1),
                false,
            );
        }
        for i in 1..=2 {
            let prefix = format!("/routing/aux/{i:02}");
            result.add(&prefix, "src", Integer(55), false);
            result.add(&prefix, "pos", Integer(10), false);
        }
        for i in 1..=4 {
            let prefix = format!("/fx/{i}");
            result.add(&prefix, "type", Integer(60), false);
            result.add(&prefix, "insert", Integer(1), false);
            for n in 1..=64 {
                // Device-observed normalized floats only. The active algorithm
                // supplies the exact step count when validating a control.
                result.add(&prefix, &format!("par/{n:02}"), Steps(1024), false);
            }
        }
        for root in inputs.iter().chain(&returns).chain(&outputs) {
            result.add(root, "eq/on", Integer(1), true);
            let bands = if outputs.contains(root) { 6 } else { 4 };
            if bands == 6 {
                result.add(root, "eq/mode", Integer(2), false);
            }
            for band in 1..=bands {
                let prefix = format!("{root}/eq/{band}");
                result.add(&prefix, "type", Integer(5), true);
                result.add(&prefix, "f", Steps(201), true);
                result.add(&prefix, "g", Steps(121), true);
                result.add(&prefix, "q", Steps(72), true);
            }
        }
        for root in inputs.iter().chain(&outputs) {
            result.add(root, "insert/on", Integer(1), false);
            result.add(
                root,
                "insert/fxslot",
                Integer(if root == "/lr" { 4 } else { 8 }),
                false,
            );
            result.add(root, "dyn/on", Integer(1), true);
            result.add(root, "dyn/mode", Integer(1), false);
            result.add(root, "dyn/ratio", Integer(11), true);
            for (name, steps) in [
                ("thr", 121),
                ("mgain", 49),
                ("attack", 121),
                ("release", 101),
                ("hold", 101),
                ("mix", 51),
            ] {
                result.add(root, &format!("dyn/{name}"), Steps(steps), true);
            }
        }
        result
    }
}
