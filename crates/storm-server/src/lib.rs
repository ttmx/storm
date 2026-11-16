//! HTTP + WebSocket API around a [`LocalBackend`], spoken by `storm-client`.
//!
//! All routes live under `/api/v1` and require `Authorization: Bearer <token>`.
//! Bodies are the JSON forms of the `storm-proto` types; errors are a JSON
//! [`storm_proto::Error`] with a matching HTTP status.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use futures::StreamExt;
use serde::Deserialize;
use storm_engine::LocalBackend;
use storm_proto::{
    AddTorrentRequest, Backend, BackendInfo, DirListing, Error, ErrorKind, FilePriority, PeerInfo, SessionStats,
    Settings, SettingsPatch, StatsSample, TorrentDetails, TorrentId, TorrentSummary,
};

#[derive(Clone)]
struct AppState {
    backend: LocalBackend,
    token: Arc<str>,
}

/// Builds the API router. `token` is required on every request.
pub fn router(backend: LocalBackend, token: impl Into<Arc<str>>) -> Router {
    let state = AppState { backend, token: token.into() };
    let api = Router::new()
        .route("/info", get(info))
        .route("/events", get(events))
        .route("/torrents", get(list_torrents).post(add_torrent))
        .route("/torrents/{id}", get(torrent_details).delete(remove_torrent))
        .route("/torrents/{id}/peers", get(torrent_peers))
        .route("/torrents/{id}/pause", post(pause))
        .route("/torrents/{id}/resume", post(resume))
        .route("/torrents/{id}/files", put(set_included_files))
        .route("/torrents/{id}/files/priority", put(set_file_priority))
        .route("/torrents/{id}/sequential", put(set_sequential))
        .route("/torrents/{id}/files/{index}/content", get(file_content))
        .route("/stats", get(session_stats))
        .route("/stats/history", get(stats_history))
        .route("/settings", get(settings).patch(update_settings))
        .route("/fs/dirs", get(list_dirs).post(create_dir))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state);
    Router::new().nest("/api/v1", api)
}

/// Generates a random 256-bit token, hex-encoded.
pub fn generate_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("OS randomness unavailable");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// auth + errors
// ---------------------------------------------------------------------------

async fn auth(State(state): State<AppState>, headers: HeaderMap, req: Request, next: Next) -> Response {
    let presented =
        headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(t) if constant_time_eq(t.as_bytes(), state.token.as_bytes()) => next.run(req).await,
        _ => ApiError(Error::new(ErrorKind::Unauthorized, "missing or invalid token")).into_response(),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

struct ApiError(Error);

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        Self(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.0.kind {
            ErrorKind::NotFound => StatusCode::NOT_FOUND,
            ErrorKind::BadRequest => StatusCode::BAD_REQUEST,
            ErrorKind::Unauthorized => StatusCode::UNAUTHORIZED,
            ErrorKind::Connection => StatusCode::BAD_GATEWAY,
            ErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(self.0)).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

async fn info(State(s): State<AppState>) -> Json<BackendInfo> {
    Json(s.backend.info())
}

async fn events(State(s): State<AppState>, ws: WebSocketUpgrade) -> Result<Response, ApiError> {
    let stream = s.backend.subscribe().await?;
    Ok(ws.on_upgrade(move |socket| forward_events(socket, stream)))
}

async fn forward_events(mut socket: WebSocket, mut stream: storm_proto::EventStream) {
    loop {
        tokio::select! {
            ev = stream.next() => {
                let Some(ev) = ev else { break };
                let Ok(text) = serde_json::to_string(&ev) else { continue };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => match msg {
                // Clients don't send anything meaningful; pings are answered by axum.
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            }
        }
    }
}

async fn list_torrents(State(s): State<AppState>) -> ApiResult<Vec<TorrentSummary>> {
    Ok(Json(s.backend.list_torrents().await?))
}

async fn add_torrent(
    State(s): State<AppState>,
    Json(req): Json<AddTorrentRequest>,
) -> ApiResult<storm_proto::AddTorrentResponse> {
    Ok(Json(s.backend.add_torrent(req).await?))
}

async fn torrent_details(State(s): State<AppState>, Path(id): Path<TorrentId>) -> ApiResult<TorrentDetails> {
    Ok(Json(s.backend.torrent_details(id).await?))
}

async fn torrent_peers(State(s): State<AppState>, Path(id): Path<TorrentId>) -> ApiResult<Vec<PeerInfo>> {
    Ok(Json(s.backend.torrent_peers(id).await?))
}

async fn pause(State(s): State<AppState>, Path(id): Path<TorrentId>) -> ApiResult<()> {
    Ok(Json(s.backend.pause(id).await?))
}

async fn resume(State(s): State<AppState>, Path(id): Path<TorrentId>) -> ApiResult<()> {
    Ok(Json(s.backend.resume(id).await?))
}

#[derive(Deserialize)]
struct RemoveQuery {
    #[serde(default)]
    delete_files: bool,
}

async fn remove_torrent(
    State(s): State<AppState>,
    Path(id): Path<TorrentId>,
    Query(q): Query<RemoveQuery>,
) -> ApiResult<()> {
    Ok(Json(s.backend.remove(id, q.delete_files).await?))
}

async fn set_included_files(
    State(s): State<AppState>,
    Path(id): Path<TorrentId>,
    Json(files): Json<Vec<usize>>,
) -> ApiResult<()> {
    Ok(Json(s.backend.set_included_files(id, files).await?))
}

#[derive(Deserialize)]
struct SequentialBody {
    sequential: bool,
}

async fn set_sequential(
    State(s): State<AppState>,
    Path(id): Path<TorrentId>,
    Json(b): Json<SequentialBody>,
) -> ApiResult<()> {
    Ok(Json(s.backend.set_sequential(id, b.sequential).await?))
}

#[derive(Deserialize)]
struct FilePriorityBody {
    files: Vec<usize>,
    priority: FilePriority,
}

async fn set_file_priority(
    State(s): State<AppState>,
    Path(id): Path<TorrentId>,
    Json(b): Json<FilePriorityBody>,
) -> ApiResult<()> {
    Ok(Json(s.backend.set_file_priority(id, b.files, b.priority).await?))
}

async fn file_content(
    State(s): State<AppState>,
    Path((id, index)): Path<(TorrentId, usize)>,
) -> Result<Response, ApiError> {
    let (name, len, reader) = s.backend.open_file(id, index).await?;
    let body = Body::from_stream(tokio_util::io::ReaderStream::with_capacity(reader, 256 * 1024));
    let mut resp = Response::new(body);
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    if let Ok(v) = HeaderValue::from_str(&content_disposition(&name)) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(resp)
}

/// `attachment` with an ASCII fallback plus the RFC 5987 UTF-8 form.
fn content_disposition(name: &str) -> String {
    let ascii: String =
        name.chars().map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' }).collect();
    let encoded: String = name
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

async fn session_stats(State(s): State<AppState>) -> ApiResult<SessionStats> {
    Ok(Json(s.backend.session_stats().await?))
}

#[derive(Deserialize)]
struct HistoryQuery {
    #[serde(default)]
    since: i64,
}

async fn stats_history(State(s): State<AppState>, Query(q): Query<HistoryQuery>) -> ApiResult<Vec<StatsSample>> {
    Ok(Json(s.backend.stats_history(q.since).await?))
}

async fn settings(State(s): State<AppState>) -> ApiResult<Settings> {
    Ok(Json(s.backend.settings().await?))
}

async fn update_settings(State(s): State<AppState>, Json(patch): Json<SettingsPatch>) -> ApiResult<Settings> {
    Ok(Json(s.backend.update_settings(patch).await?))
}

#[derive(Deserialize)]
struct DirsQuery {
    path: Option<String>,
}

async fn list_dirs(State(s): State<AppState>, Query(q): Query<DirsQuery>) -> ApiResult<DirListing> {
    Ok(Json(s.backend.list_dirs(q.path).await?))
}

#[derive(Deserialize)]
struct CreateDirBody {
    path: String,
}

async fn create_dir(State(s): State<AppState>, Json(b): Json<CreateDirBody>) -> ApiResult<()> {
    Ok(Json(s.backend.create_dir(b.path).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_hex() {
        let (a, b) = (generate_token(), generate_token());
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn content_disposition_escapes() {
        let v = content_disposition("Ünïcode \"x\".mkv");
        assert!(v.starts_with("attachment; filename=\"_n_code _x_.mkv\""), "{v}");
        assert!(v.contains("filename*=UTF-8''%C3%9Cn%C3%AFcode%20%22x%22.mkv"), "{v}");
    }
}
