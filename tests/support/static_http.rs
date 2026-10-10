//! Static in-memory HTTP server for end-to-end tests and benches (WP-13): playlists and guides
//! served from 127.0.0.1 like a real host serves them (conditional `ETag` → 304, bytes sent as
//! they are, so `.xml.gz` arrives gzip-compressed without `Content-Encoding`). Runs on its own
//! thread and current-thread runtime so blocking tests and benches can use it and its work
//! never competes with the job runtime's workers. Not part of the frozen `support` module list:
//! include it with `#[path = "support/static_http.rs"] mod static_http;` (benches:
//! `"../tests/support/static_http.rs"`).
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

#[derive(Clone)]
struct File {
    body: Bytes,
    content_type: &'static str,
    etag: String,
}

#[derive(Default)]
struct State {
    files: HashMap<String, File>,
    /// Requested paths (without query), in order.
    hits: Vec<String>,
    /// Bumped by every `put`, so a replaced file gets a new ETag.
    version: u64,
}

pub struct StaticServer {
    /// `http://127.0.0.1:<port>`
    pub base: String,
    state: Arc<Mutex<State>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl StaticServer {
    pub fn start() -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let shared = state.clone();
        let thread = std::thread::Builder::new()
            .name("static-http".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("server runtime");
                rt.block_on(async move {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind 127.0.0.1");
                    addr_tx.send(listener.local_addr().expect("local addr")).expect("report address");
                    let app = axum::Router::new().fallback(move |uri: Uri, headers: HeaderMap| {
                        let state = shared.clone();
                        async move { reply(&state, &uri, &headers) }
                    });
                    let _ = axum::serve(listener, app)
                        .with_graceful_shutdown(async move {
                            let _ = stop_rx.await;
                        })
                        .await;
                });
            })
            .expect("spawn server thread");
        let addr = addr_rx.recv().expect("server address");
        Self { base: format!("http://{addr}"), state, stop: Some(stop_tx), thread: Some(thread) }
    }

    /// Serve `body` at `path` (leading `/`), replacing any previous file there.
    pub fn put(&self, path: &str, body: impl Into<Bytes>, content_type: &'static str) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.version += 1;
        let etag = format!("\"v{}\"", s.version);
        s.files.insert(path.to_owned(), File { body: body.into(), content_type, etag });
    }

    pub fn url(&self, path: &str) -> String { format!("{}{path}", self.base) }

    /// How many requests reached `path` (any status).
    pub fn hits(&self, path: &str) -> usize { self.state.lock().unwrap_or_else(|e| e.into_inner()).hits.iter().filter(|p| *p == path).count() }
}

impl Drop for StaticServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn reply(state: &Mutex<State>, uri: &Uri, headers: &HeaderMap) -> Response {
    let file = {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.hits.push(uri.path().to_owned());
        s.files.get(uri.path()).cloned()
    };
    let Some(f) = file else { return (StatusCode::NOT_FOUND, "").into_response() };
    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(f.etag.as_str()) {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, f.etag)]).into_response();
    }
    ([(header::CONTENT_TYPE, f.content_type.to_owned()), (header::ETAG, f.etag)], f.body).into_response()
}
