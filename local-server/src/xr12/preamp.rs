use super::{
    parameters::{Kind, Parameter},
    Intent, Mixer, MixerError, State,
};
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Control {
    Gain,
    Phantom,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreampRequest {
    pub epoch: String,
    pub channel: u8,
    pub input: u8,
    pub control: Control,
    pub expected: Value,
    pub value: Value,
    pub confirmed_input: Option<u8>,
}

pub(super) struct Edit {
    pub map: String,
    pub input: u8,
    pub expected: Value,
}
impl Edit {
    pub fn valid(&self, state: &State, physical_key: &str) -> bool {
        state.fresh(&self.map).and_then(Value::as_u64) == Some(u64::from(self.input - 1))
            && state.fresh(physical_key) == Some(&self.expected)
    }
}

pub(super) fn input_from_key(key: &str) -> Option<u8> {
    let suffix = key.strip_prefix("/headamp/")?;
    let (input, param) = suffix.split_once('/')?;
    if !matches!(param, "gain" | "phantom") {
        return None;
    }
    input.parse().ok().filter(|n| (1..=4).contains(n))
}

impl State {
    pub(super) fn has_preamp(&self, input: u8) -> bool {
        (1..=12).any(|n| {
            self.fresh(&format!("/ch/{n:02}/config/insrc"))
                .and_then(Value::as_u64)
                == Some(u64::from(input - 1))
        })
    }
}

impl Mixer {
    pub(crate) async fn set_preamp(
        &self,
        req: PreampRequest,
    ) -> Result<Value, MixerError> {
        if !(1..=12).contains(&req.channel) || !(1..=4).contains(&req.input) {
            return Err(MixerError::Unavailable);
        }
        let (suffix, kind) = match req.control {
            Control::Gain => ("gain", Kind::Steps(145)),
            Control::Phantom => {
                if req.confirmed_input != Some(req.input) {
                    return Err(MixerError::InvalidValue);
                }
                ("phantom", Kind::Integer(1))
            }
        };
        let param = Parameter {
            kind,
            writable: true,
        };
        let expected = param
            .encode(&req.expected)
            .and_then(|arg| param.observed(&arg))
            .ok_or(MixerError::InvalidValue)?;
        param.encode(&req.value).ok_or(MixerError::InvalidValue)?;
        let edit = Edit {
            map: format!("/ch/{:02}/config/insrc", req.channel),
            input: req.input,
            expected,
        };
        self.request(
            &format!("/headamp/{:02}/{suffix}", req.input),
            req.value,
            &req.epoch,
            Intent::Preamp(edit),
        )
        .await
    }
}
