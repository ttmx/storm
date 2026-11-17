//! [`RemoteBackend`]: a [`Backend`] that talks to `stormd` over HTTP + WebSocket.
//!
//! Like the local engine, it runs its I/O on its own small tokio runtime, so its
//! futures can be awaited from any executor (gpui's included).

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures::{SinkExt, StreamExt};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use storm_proto::{
    self as proto, AddTorrentRequest, AddTorrentResponse, Backend, BackendInfo, BackendKind, DirListing, Error,
    ErrorKind, Event, EventStream, FilePriority, PeerInfo, SessionStats, Settings, SettingsPatch, StatsSample,
    TorrentDetails, TorrentId, TorrentSummary, UpdateServerRequest,
};
use tokio::io::AsyncWriteExt;
use tokio::runtime::{Handle, Runtime};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

/// Remote engine handle. Cheap to clone.
#[derive(Clone)]
pub struct RemoteBackend {
    inner: Arc<Inner>,
}

struct Inner {
    /// e.g. `http://host:9870/api/v1/`
    api: url::Url,
    /// What the user typed, normalized (shown in the UI).
    display_url: String,
    token: String,
    http: reqwest::Client,
    /// Re-read on every (re)subscribe: the server may have restarted on
    /// another version.
    info: Mutex<BackendInfo>,
    rt: RuntimeHandle,
}

/// Either a runtime we own (shut down in the background on drop, so dropping from
/// inside async code is fine) or a borrowed handle.
enum RuntimeHandle {
    Owned(Option<Runtime>, Handle),
    Borrowed(Handle),
}

impl RuntimeHandle {
    fn handle(&self) -> &Handle {
        match self {
            RuntimeHandle::Owned(_, h) | RuntimeHandle::Borrowed(h) => h,
        }
    }
}

impl Drop for RuntimeHandle {
    fn drop(&mut self) {
        if let RuntimeHandle::Owned(rt, _) = self
            && let Some(rt) = rt.take()
        {
            rt.shutdown_background();
        }
    }
}

impl RemoteBackend {
    /// Connects to `url` (e.g. `myserver`, `myserver:9870`, `https://torrents.example.com`)
    /// and verifies the token and API version. Runs on a runtime it creates itself, so
    /// it can be awaited from any executor.
    pub async fn connect(url: &str, token: &str) -> proto::Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("storm-remote")
            .enable_all()
            .build()
            .map_err(|e| Error::internal(format!("creating runtime: {e}")))?;
        let handle = rt.handle().clone();
        let (url, token) = (url.to_string(), token.to_string());
        let rt = RuntimeHandle::Owned(Some(rt), handle.clone());
        let fut = Self::connect_inner(url, token);
        let res = handle.spawn(fut).await;
        match res {
            Ok(Ok((api, display_url, token, http, info))) => {
                Ok(Self { inner: Arc::new(Inner { api, display_url, token, http, info: Mutex::new(info), rt }) })
            }
            Ok(Err(e)) => Err(e),
            Err(e) => Err(Error::internal(format!("connect task failed: {e}"))),
        }
    }

    /// Like [`connect`](Self::connect), but runs on the caller's tokio runtime.
    pub async fn connect_on_current_runtime(url: &str, token: &str) -> proto::Result<Self> {
        let (api, display_url, token, http, info) = Self::connect_inner(url.to_string(), token.to_string()).await?;
        let rt = RuntimeHandle::Borrowed(Handle::current());
        Ok(Self { inner: Arc::new(Inner { api, display_url, token, http, info: Mutex::new(info), rt }) })
    }

    async fn connect_inner(
        url: String,
        token: String,
    ) -> proto::Result<(url::Url, String, String, reqwest::Client, BackendInfo)> {
        // Both ring and aws-lc end up in the dependency graph; rustls refuses to guess,
        // which would panic on the first wss:// connection. Pick one explicitly.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let base = normalize_url(&url)?;
        let api = base.join("api/v1/").map_err(|e| Error::bad_request(e.to_string()))?;
        let http = reqwest::Client::builder()
            .user_agent(concat!("storm/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| Error::internal(e.to_string()))?;
        let display_url = base.as_str().trim_end_matches('/').to_string();

        let info = fetch_info(&http, &api, &token, &display_url).await?;
        Ok((api, display_url, token, http, info))
    }

    /// The URL shown to the user, e.g. `http://nas:9870`.
    pub fn url(&self) -> &str {
        &self.inner.display_url
    }

    async fn run<T, Fut>(&self, f: impl FnOnce(Arc<Inner>) -> Fut) -> proto::Result<T>
    where
        T: Send + 'static,
        Fut: Future<Output = proto::Result<T>> + Send + 'static,
    {
        let fut = f(self.inner.clone());
        self.inner.rt.handle().spawn(fut).await.map_err(|e| Error::internal(format!("request task failed: {e}")))?
    }

    async fn call<T: DeserializeOwned + Send + 'static>(
        &self,
        method: Method,
        path: String,
        body: Option<serde_json::Value>,
    ) -> proto::Result<T> {
        self.run(move |inner| async move {
            let mut req = inner.request(method, &path)?;
            if let Some(b) = body {
                req = req.json(&b);
            }
            decode(send(req).await?).await
        })
        .await
    }

    async fn get<T: DeserializeOwned + Send + 'static>(&self, path: String) -> proto::Result<T> {
        self.call(Method::GET, path, None).await
    }

    async fn send_json<T: DeserializeOwned + Send + 'static>(
        &self,
        method: Method,
        path: String,
        body: impl Serialize,
    ) -> proto::Result<T> {
        let v = serde_json::to_value(body).map_err(|e| Error::internal(e.to_string()))?;
        self.call(method, path, Some(v)).await
    }
}

/// `GET /info`, checking the API version.
async fn fetch_info(
    http: &reqwest::Client,
    api: &url::Url,
    token: &str,
    display_url: &str,
) -> proto::Result<BackendInfo> {
    let resp = send(http.get(api.join("info").unwrap()).bearer_auth(token)).await?;
    let mut info: BackendInfo = decode(resp).await?;
    if info.api_version != proto::API_VERSION {
        return Err(Error::bad_request(format!(
            "server speaks API v{}, this app speaks v{}; update the older one",
            info.api_version,
            proto::API_VERSION
        )));
    }
    info.kind = BackendKind::Remote { url: display_url.to_owned() };
    Ok(info)
}

impl Inner {
    fn request(&self, method: Method, path: &str) -> proto::Result<RequestBuilder> {
        let url = self.api.join(path).map_err(|e| Error::bad_request(e.to_string()))?;
        Ok(self.http.request(method, url).bearer_auth(&self.token))
    }

    fn ws_url(&self) -> url::Url {
        let mut u = self.api.join("events").unwrap();
        let scheme = if u.scheme() == "https" { "wss" } else { "ws" };
        u.set_scheme(scheme).expect("http(s) -> ws(s) is always valid");
        u
    }
}

#[async_trait::async_trait]
impl Backend for RemoteBackend {
    fn info(&self) -> BackendInfo {
        self.inner.info.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    async fn subscribe(&self) -> proto::Result<EventStream> {
        self.run(|inner| async move {
            let info = fetch_info(&inner.http, &inner.api, &inner.token, &inner.display_url).await?;
            *inner.info.lock().unwrap_or_else(|e| e.into_inner()) = info;
            let mut req =
                inner.ws_url().as_str().into_client_request().map_err(|e| Error::bad_request(e.to_string()))?;
            let auth = format!("Bearer {}", inner.token)
                .parse()
                .map_err(|_| Error::bad_request("token contains invalid characters"))?;
            req.headers_mut().insert("authorization", auth);
            let (ws, _) = tokio_tungstenite::connect_async(req).await.map_err(|e| match e {
                tungstenite::Error::Http(r) if r.status() == StatusCode::UNAUTHORIZED.as_u16() => {
                    Error::new(ErrorKind::Unauthorized, "invalid token")
                }
                e => Error::new(ErrorKind::Connection, format!("event stream: {e}")),
            })?;

            // Pump the socket on the client runtime; hand events over a channel so the
            // returned stream doesn't need tokio to be polled.
            let (tx, rx) = futures::channel::mpsc::channel::<Event>(16);
            tokio::spawn(async move {
                let (mut sink, mut stream) = ws.split();
                let mut tx = tx;
                while let Some(msg) = stream.next().await {
                    match msg {
                        Ok(tungstenite::Message::Text(t)) => match serde_json::from_str::<Event>(&t) {
                            Ok(ev) => {
                                if tx.send(ev).await.is_err() {
                                    break; // subscriber dropped
                                }
                            }
                            Err(e) => tracing::warn!("bad event from server: {e}"),
                        },
                        Ok(tungstenite::Message::Close(_)) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
                let _ = sink.close().await;
            });
            Ok(Box::pin(rx) as EventStream)
        })
        .await
    }

    async fn list_torrents(&self) -> proto::Result<Vec<TorrentSummary>> {
        self.get("torrents".into()).await
    }

    async fn torrent_details(&self, id: TorrentId) -> proto::Result<TorrentDetails> {
        self.get(format!("torrents/{id}")).await
    }

    async fn torrent_peers(&self, id: TorrentId) -> proto::Result<Vec<PeerInfo>> {
        self.get(format!("torrents/{id}/peers")).await
    }

    async fn add_torrent(&self, req: AddTorrentRequest) -> proto::Result<AddTorrentResponse> {
        self.send_json(Method::POST, "torrents".into(), req).await
    }

    async fn update_server(&self, version: String) -> proto::Result<()> {
        self.send_json(Method::POST, "server/update".into(), UpdateServerRequest { version }).await
    }

    async fn pause(&self, id: TorrentId) -> proto::Result<()> {
        self.call(Method::POST, format!("torrents/{id}/pause"), None).await
    }

    async fn resume(&self, id: TorrentId) -> proto::Result<()> {
        self.call(Method::POST, format!("torrents/{id}/resume"), None).await
    }

    async fn remove(&self, id: TorrentId, delete_files: bool) -> proto::Result<()> {
        self.call(Method::DELETE, format!("torrents/{id}?delete_files={delete_files}"), None).await
    }

    async fn set_included_files(&self, id: TorrentId, files: Vec<usize>) -> proto::Result<()> {
        self.send_json(Method::PUT, format!("torrents/{id}/files"), files).await
    }

    async fn set_sequential(&self, id: TorrentId, sequential: bool) -> proto::Result<()> {
        self.send_json(
            Method::PUT,
            format!("torrents/{id}/sequential"),
            serde_json::json!({ "sequential": sequential }),
        )
        .await
    }

    async fn set_file_priority(&self, id: TorrentId, files: Vec<usize>, priority: FilePriority) -> proto::Result<()> {
        self.send_json(
            Method::PUT,
            format!("torrents/{id}/files/priority"),
            serde_json::json!({ "files": files, "priority": priority }),
        )
        .await
    }

    async fn session_stats(&self) -> proto::Result<SessionStats> {
        self.get("stats".into()).await
    }

    async fn stats_history(&self, since: i64) -> proto::Result<Vec<StatsSample>> {
        self.get(format!("stats/history?since={since}")).await
    }

    async fn settings(&self) -> proto::Result<Settings> {
        self.get("settings".into()).await
    }

    async fn update_settings(&self, patch: SettingsPatch) -> proto::Result<Settings> {
        self.send_json(Method::PATCH, "settings".into(), patch).await
    }

    async fn list_dirs(&self, path: Option<String>) -> proto::Result<DirListing> {
        self.run(move |inner| async move {
            let mut req = inner.request(Method::GET, "fs/dirs")?;
            if let Some(p) = path {
                req = req.query(&[("path", p)]);
            }
            decode(send(req).await?).await
        })
        .await
    }

    async fn create_dir(&self, path: String) -> proto::Result<()> {
        self.send_json(Method::POST, "fs/dirs".into(), serde_json::json!({ "path": path })).await
    }

    async fn download_file(&self, id: TorrentId, file_index: usize, dest: PathBuf) -> proto::Result<()> {
        // Unlike other calls, dropping this future aborts the transfer (the UI's
        // "Cancel"); the partial file is left for the caller to remove.
        let inner = self.inner.clone();
        let task = self.inner.rt.handle().spawn(async move {
            let resp = send(inner.request(Method::GET, &format!("torrents/{id}/files/{file_index}/content"))?).await?;
            let resp = check(resp).await?;
            // Write to a temp file next to the destination, then rename: no half files.
            let tmp = partial_path(&dest);
            let io = |e: std::io::Error| Error::internal(format!("writing {}: {e}", dest.display()));
            let mut file = tokio::fs::File::create(&tmp).await.map_err(io)?;
            let mut body = resp.bytes_stream();
            let result = async {
                while let Some(chunk) = body.next().await {
                    let chunk = chunk.map_err(|e| Error::new(ErrorKind::Connection, e.to_string()))?;
                    file.write_all(&chunk).await.map_err(io)?;
                }
                file.flush().await.map_err(io)?;
                drop(file);
                tokio::fs::rename(&tmp, &dest).await.map_err(io)
            }
            .await;
            if result.is_err() {
                let _ = tokio::fs::remove_file(&tmp).await;
            }
            result
        });
        let mut guard = AbortOnDrop(task);
        (&mut guard.0).await.map_err(|e| Error::internal(format!("download task failed: {e}")))?
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Where [`Backend::download_file`](proto::Backend::download_file) streams `dest`
/// while the copy is in progress (renamed to `dest` when complete). The UI watches
/// its size to show byte progress without changing the `Backend` trait.
pub fn partial_path(dest: &std::path::Path) -> PathBuf {
    dest.with_extension(match dest.extension() {
        Some(e) => format!("{}.part", e.to_string_lossy()),
        None => "part".into(),
    })
}

/// `host` → `http://host:9870/`; keeps explicit scheme/port/path.
fn normalize_url(input: &str) -> proto::Result<url::Url> {
    let input = input.trim();
    if input.is_empty() {
        return Err(Error::bad_request("server address is empty"));
    }
    let with_scheme = if input.contains("://") { input.to_string() } else { format!("http://{input}") };
    let mut url = url::Url::parse(&with_scheme).map_err(|e| Error::bad_request(format!("invalid address: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::bad_request("address must be http:// or https://"));
    }
    // Only plain http with no port gets the stormd default; https is assumed to be a proxy.
    if url.port().is_none() && url.scheme() == "http" {
        url.set_port(Some(proto::DEFAULT_PORT)).ok();
    }
    if !url.path().ends_with('/') {
        let p = format!("{}/", url.path());
        url.set_path(&p);
    }
    Ok(url)
}

async fn send(req: RequestBuilder) -> proto::Result<reqwest::Response> {
    req.send().await.map_err(|e| {
        let msg = if e.is_connect() {
            "couldn't connect to the server".to_string()
        } else if e.is_timeout() {
            "the server took too long to respond".to_string()
        } else {
            e.to_string()
        };
        Error::new(ErrorKind::Connection, msg)
    })
}

/// Turns non-2xx responses into the server's `Error` (or a generic one).
async fn check(resp: reqwest::Response) -> proto::Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let text = resp.text().await.unwrap_or_default();
    Err(serde_json::from_str::<Error>(&text).unwrap_or_else(|_| {
        let kind = match status {
            StatusCode::UNAUTHORIZED => ErrorKind::Unauthorized,
            StatusCode::NOT_FOUND => ErrorKind::NotFound,
            s if s.is_client_error() => ErrorKind::BadRequest,
            _ => ErrorKind::Internal,
        };
        Error::new(kind, format!("server returned {status}: {text}"))
    }))
}

async fn decode<T: DeserializeOwned>(resp: reqwest::Response) -> proto::Result<T> {
    let resp = check(resp).await?;
    resp.json::<T>().await.map_err(|e| Error::internal(format!("bad response from server: {e}")))
}

#[cfg(test)]
mod tests {
    use super::normalize_url;

    #[test]
    fn normalizes_addresses() {
        assert_eq!(normalize_url("nas").unwrap().as_str(), "http://nas:9870/");
        assert_eq!(normalize_url("nas:1234").unwrap().as_str(), "http://nas:1234/");
        assert_eq!(normalize_url("http://10.0.0.2").unwrap().as_str(), "http://10.0.0.2:9870/");
        assert_eq!(normalize_url("https://t.example.com").unwrap().as_str(), "https://t.example.com/");
        assert_eq!(normalize_url("https://x.com/storm").unwrap().as_str(), "https://x.com/storm/");
        assert!(normalize_url("ftp://x").is_err());
        assert!(normalize_url("  ").is_err());
    }
}
