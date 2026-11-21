use super::*;
use crate::xr12::preamp::PreampRequest;

pub(super) fn routes() -> Router<Arc<Desk>> {
    Router::new().route("/api/preamp", post(preamp))
}

async fn preamp(
    State(desk): State<Arc<Desk>>,
    headers: HeaderMap,
    Json(mut req): Json<PreampRequest>,
) -> Result<Json<Value>, Failure> {
    let (mixer, epoch) = armed(&desk, &headers, &req.epoch)?;
    req.epoch = epoch;
    mixer.set_preamp(req)
        .await
        .map(Json)
        .map_err(|e| conflict(e.to_string()))
}
