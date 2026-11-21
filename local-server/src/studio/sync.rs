//! Per-station delta cursors. A dropped response or changed mixer epoch starts a fresh snapshot.
use super::{authenticate, snapshot, Desk, Failure};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Default)]
pub(super) struct Previous {
    cursor: Uuid,
    epoch: String,
    values: Map<String, Value>,
    sends: Value,
    effects: Value,
    frequency_map: Value,
    show: Value,
}

#[derive(Deserialize)]
pub(super) struct Cursor {
    cursor: Option<Uuid>,
}

impl Previous {
    fn encode(&mut self, mut frame: Value, requested: Option<Uuid>) -> Value {
        let mix = &mut frame["mixer"];
        let epoch = mix["epoch"].as_str().unwrap_or_default().to_owned();
        let incremental =
            requested == Some(self.cursor) && epoch == self.epoch && !self.cursor.is_nil();
        let values = mix["values"]
            .as_object_mut()
            .expect("serialized mixer values are a map");
        let now: Map<_, _> = values
            .iter()
            .map(|(key, v)| (key.clone(), v["value"].clone()))
            .collect();
        if incremental {
            let removed: Vec<_> = self
                .values
                .keys()
                .filter(|key| !now.contains_key(*key))
                .cloned()
                .collect();
            values.retain(|key, v| self.values.get(key) != Some(&v["value"]));
            mix["removed"] = json!(removed);
        }
        self.values = now;
        self.epoch = epoch;
        for (key, previous) in [
            ("effects", &mut self.effects),
            ("frequency_map", &mut self.frequency_map),
            ("send_enabled", &mut self.sends),
        ] {
            if incremental && *previous == mix[key] {
                mix.as_object_mut().unwrap().remove(key);
            } else {
                *previous = mix[key].clone();
            }
        }
        if incremental && self.show == frame["show"] {
            frame.as_object_mut().unwrap().remove("show");
        } else {
            self.show = frame["show"].clone();
        }
        self.cursor = Uuid::new_v4();
        frame["cursor"] = json!(self.cursor);
        frame["delta"] = json!(incremental);
        frame
    }
}

pub(super) async fn updates(
    State(desk): State<Arc<Desk>>,
    headers: HeaderMap,
    Json(query): Json<Cursor>,
) -> Result<Json<Value>, Failure> {
    let token = authenticate(&desk, &headers)?;
    if query.cursor.is_none() {
        // A station starting over after sleep or a network outage must acquire
        // fresh control. Invalidate even an enable still in transit.
        let mut sessions = desk.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let op = sessions.get_mut(&token)
            .ok_or((StatusCode::UNAUTHORIZED, "Pairing revoked".into()))?;
        op.restart_control();
    }
    let Json(mut data) = snapshot(State(desk.clone()), headers).await?;
    data["mixer"].as_object_mut().unwrap().remove("meters");
    let mut sessions = desk.sessions.lock().unwrap_or_else(|e| e.into_inner());
    let op = sessions
        .get_mut(&token)
        .ok_or((StatusCode::UNAUTHORIZED, "Pairing revoked".into()))?;
    Ok(Json(op.previous.encode(data, query.cursor)))
}

pub(super) async fn meters(
    State(desk): State<Arc<Desk>>,
    headers: HeaderMap,
) -> Result<Json<Value>, Failure> {
    let token = authenticate(&desk, &headers)?;
    let (epoch, meters) = desk.mixer.meters().await;
    let sessions = desk.sessions.lock().unwrap_or_else(|e| e.into_inner());
    let op = sessions.get(&token)
        .ok_or((StatusCode::UNAUTHORIZED, "Pairing revoked".into()))?;
    let epoch = op.control_epoch(&epoch);
    Ok(Json(json!({"epoch":epoch,"meters":meters})))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_sample_expires_and_lost_cursor_forces_full_snapshot() {
        let frame = |values: Value| json!({"show":null,"mixer":{"epoch":"a","values":values,"send_enabled":{},"effects":{}}});
        let mut prev = Previous::default();
        let first = prev.encode(
            frame(json!({"a":{"value":0,"age_ms":0},"b":{"value":1,"age_ms":0}})),
            None,
        );
        let cursor = serde_json::from_value(first["cursor"].clone()).unwrap();
        let next = frame(json!({"a":{"value":0,"age_ms":200}}));
        let second = prev.encode(next.clone(), Some(cursor));
        assert_eq!(second["mixer"]["values"], json!({}));
        assert_eq!(second["mixer"]["removed"], json!(["b"]));
        assert!(second.get("show").is_none());
        let third = prev.encode(next, Some(cursor));
        assert_eq!(third["delta"], false);
        assert_eq!(third["mixer"]["values"]["a"]["value"], 0);
    }
}
