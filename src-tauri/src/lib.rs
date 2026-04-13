// ─────────────────────────────────────────────────────────────────────────────
//  GimmeURL —  Hybrid transfer engine
//  Layers: LAN TCP → WebRTC DataChannel → HTTP chunk fallback
// ─────────────────────────────────────────────────────────────────────────────

use axum::{
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response, Sse},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use futures_util::stream::StreamExt;
use humansize::{format_size, BINARY};
use once_cell::sync::OnceCell;
use qrcode::{render::svg, QrCode};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tauri::{AppHandle, Emitter, Manager};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::{broadcast, RwLock},
};
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

// ── Constants ─────────────────────────────────────────────────────────────────
const CHUNK_SIZE: u64 = 256 * 1024;       // 256 KB per chunk
#[allow(dead_code)]
const MAX_PARALLEL_CHUNKS: usize = 8;     // parallel HTTP fallback streams
const PORT: u16 = 3737;
const LAN_PORT: u16 = 3738;              // raw LAN chunk server

// ── Data model ────────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
pub struct FileRecord {
    pub id: String,
    pub original_name: String,
    pub total_size: u64,
    pub chunk_count: u32,
    pub mime_type: String,
    pub created_at: DateTime<Utc>,
    pub path: String,
    pub download_count: AtomicWrap,
}

// Wrapper so we can derive Clone for AtomicU64
#[derive(Serialize, Deserialize)]
pub struct AtomicWrap(#[serde(with = "atomic_serde")] pub Arc<AtomicU64>);
impl Clone for AtomicWrap {
    fn clone(&self) -> Self { AtomicWrap(Arc::clone(&self.0)) }
}
mod atomic_serde {
    use super::*;
    use serde::{Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &Arc<AtomicU64>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(v.load(Ordering::Relaxed))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Arc<AtomicU64>, D::Error> {
        let n = u64::deserialize(d)?;
        Ok(Arc::new(AtomicU64::new(n)))
    }
}

// ── Peer / swarm registry ─────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct PeerInfo {
    pub peer_id: String,
    pub file_id: String,
    pub has_chunks: Vec<u32>,       // which chunk indices this peer has
    pub lan_addr: Option<String>,   // "192.168.x.x:3738" if on LAN
    pub joined_at: DateTime<Utc>,
}

// ── Signaling message types ────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalMsg {
    // Peer joined the swarm
    PeerJoined { peer_id: String, lan_addr: Option<String>, has_chunks: Vec<u32> },
    // WebRTC offer from initiator to a specific peer
    Offer  { from: String, to: String, sdp: String },
    // WebRTC answer back
    Answer { from: String, to: String, sdp: String },
    // ICE candidate
    Ice    { from: String, to: String, candidate: String },
    // Peer left / timed out
    PeerLeft { peer_id: String },
    // Chunk availability update
    ChunkUpdate { peer_id: String, new_chunks: Vec<u32> },
    // Server announces a new file is available
    FileReady {
        file_id: String,
        name: String,
        total_size: u64,
        chunk_count: u32,
        lan_addr: Option<String>,
    },
}

// ── App state ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub files: Arc<DashMap<String, FileRecord>>,
    pub peers: Arc<DashMap<String, PeerInfo>>,          // peer_id → info
    pub file_peers: Arc<DashMap<String, Vec<String>>>,  // file_id → [peer_ids]
    // per-file broadcast channel for signaling
    pub signal_bus: Arc<DashMap<String, broadcast::Sender<SignalMsg>>>,
    pub upload_dir: PathBuf,
    pub sandbox_dir: PathBuf,
    pub path_index: Arc<DashMap<String, String>>,        // full path → file_id
    pub public_url: Arc<RwLock<String>>,
    pub lan_ip: Arc<RwLock<Option<String>>>,
    pub bytes_up: Arc<AtomicU64>,
    pub bytes_down: Arc<AtomicU64>,
    pub app: Option<AppHandle>,
}

static STATE: OnceCell<AppState> = OnceCell::new();

// ── Tauri events ───────────────────────────────────────────────────────────────

#[derive(Clone, Serialize)]
struct SpeedEvent { up_mbps: f64, down_mbps: f64, active_peers: usize }

#[derive(Clone, Serialize)]
struct FileReadyEvent {
    file_id: String, name: String, total_size: u64,
    chunk_count: u32, download_url: String,
    lan_url: Option<String>, qr_svg: String,
}

#[derive(Clone, Serialize)]
struct SwarmEvent {
    file_id: String, peer_count: usize, chunks_available: usize,
}

#[derive(Clone, Serialize)]
struct SandboxFileAddedEvent {
    file_id: String,
    name: String,
    total_size: u64,
    chunk_count: u32,
    mime_type: String,
    download_url: String,
}

#[derive(Clone, Serialize)]
struct SandboxFileRemovedEvent {
    file_id: String,
}

// ── Tauri commands ─────────────────────────────────────────────────────────────

#[tauri::command]
async fn set_tunnel_url(url: String) -> Result<String, String> {
    let st = STATE.get().ok_or("not ready")?;
    let clean = url.trim().trim_end_matches('/').to_string();
    *st.public_url.write().await = clean.clone();
    Ok(clean)
}

#[tauri::command]
async fn get_lan_ip() -> Option<String> {
    local_ip_address::local_ip().ok().map(|ip| ip.to_string())
}

#[tauri::command]
async fn get_active_files() -> Vec<serde_json::Value> {
    let st = match STATE.get() { Some(s) => s, None => return vec![] };
    let pub_url = st.public_url.read().await.clone();
    let lan_ip  = st.lan_ip.read().await.clone();
    let base = if pub_url.is_empty() { format!("http://localhost:{PORT}") } else { pub_url };

    st.files.iter().map(|f| {
        let peers = st.file_peers.get(&f.id).map(|p| p.len()).unwrap_or(0);
        serde_json::json!({
            "id": f.id,
            "name": f.original_name,
            "total_size": f.total_size,
            "size_human": format_size(f.total_size, BINARY),
            "chunk_count": f.chunk_count,
            "mime_type": f.mime_type,
            "download_url": format!("{}/get/{}", base, f.id),
            "lan_url": lan_ip.as_ref().map(|ip| format!("http://{}:{}/get/{}", ip, PORT, f.id)),
            "peers": peers,
            "downloads": f.download_count.0.load(Ordering::Relaxed),
        })
    }).collect()
}

#[tauri::command]
async fn delete_file(id: String) -> Result<(), String> {
    let st = STATE.get().ok_or("not ready")?;
    if let Some((_, rec)) = st.files.remove(&id) {
        let _ = tokio::fs::remove_file(&rec.path).await;
        st.path_index.remove(&rec.path);
        if let Some((_, peer_ids)) = st.file_peers.remove(&id) {
            for pid in peer_ids {
                st.peers.remove(&pid);
            }
        }
        st.signal_bus.remove(&id);
    }
    Ok(())
}

#[tauri::command]
async fn get_sandbox_folder() -> Result<String, String> {
    let st = STATE.get().ok_or("not ready")?;
    Ok(st.sandbox_dir.to_string_lossy().to_string())
}

#[tauri::command]
async fn open_sandbox_folder() -> Result<(), String> {
    let st = STATE.get().ok_or("not ready")?;
    open_folder(&st.sandbox_dir)
}

#[tauri::command]
async fn open_file_in_sandbox(file_id: String) -> Result<(), String> {
    let st = STATE.get().ok_or("not ready")?;
    let rec = st.files.get(&file_id).ok_or("not found")?;
    let p = std::path::PathBuf::from(rec.path.clone());
    if !p.exists() {
        return Err("file missing on disk".to_string());
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg("-R")
            .arg(&p)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(target_os = "windows")]
    {
        // Explorer can select the file with /select,
        Command::new("explorer")
            .arg("/select,")
            .arg(&p)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // Fall back to opening the containing folder.
        open_folder(p.parent().unwrap_or(&st.sandbox_dir))
    }
}

#[tauri::command]
async fn get_qr(url: String) -> Result<String, String> {
    make_qr(&url)
}

// ── QR ─────────────────────────────────────────────────────────────────────────
fn make_qr(url: &str) -> Result<String, String> {
    let code = QrCode::new(url.as_bytes()).map_err(|e| e.to_string())?;
    Ok(code.render::<svg::Color>()
        .min_dimensions(180, 180)
        .dark_color(svg::Color("#0a0a0f"))
        .light_color(svg::Color("#ffffff"))
        .build())
}

fn open_folder(path: &std::path::Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(target_os = "windows")]
    {
        Command::new("explorer")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

fn safe_file_name(name: &str) -> String {
    std::path::Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "file".to_string())
}

fn unique_path_in(dir: &std::path::Path, name: &str) -> PathBuf {
    let base = safe_file_name(name);
    let first = dir.join(&base);
    if !first.exists() {
        return first;
    }

    let p = std::path::Path::new(&base);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("");
    for n in 1..=999u32 {
        let candidate = if ext.is_empty() {
            format!("{} ({})", stem, n)
        } else {
            format!("{} ({}).{}", stem, n, ext)
        };
        let path = dir.join(candidate);
        if !path.exists() {
            return path;
        }
    }

    dir.join(format!("{}-{}", Uuid::new_v4(), base))
}

fn guess_mime(name: &str) -> String {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "json" => "application/json",
        "zip" => "application/zip",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn import_existing_sandbox_files_sync(st: &AppState, lan_ip: &Option<String>) {
    let rd = match std::fs::read_dir(&st.sandbox_dir) {
        Ok(r) => r,
        Err(_) => return,
    };

    for entry in rd.flatten() {
        let ft = match entry.file_type() { Ok(t) => t, Err(_) => continue };
        if !ft.is_file() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }

        let path = entry.path();
        let key = path.to_string_lossy().to_string();
        if st.path_index.contains_key(&key) {
            continue;
        }

        let md = match entry.metadata() { Ok(m) => m, Err(_) => continue };
        let size = md.len();
        let nchunks = std::cmp::max(1, ((size + CHUNK_SIZE - 1) / CHUNK_SIZE) as u32);
        let mime = guess_mime(&name);

        let id = Uuid::new_v4().to_string();
        let rec = FileRecord {
            id: id.clone(),
            original_name: name.clone(),
            total_size: size,
            chunk_count: nchunks,
            mime_type: mime,
            created_at: Utc::now(),
            path: key.clone(),
            download_count: AtomicWrap(Arc::new(AtomicU64::new(0))),
        };

        let (tx, _) = broadcast::channel::<SignalMsg>(256);
        st.signal_bus.insert(id.clone(), tx.clone());
        st.file_peers.insert(id.clone(), vec![]);

        let sender_peer = PeerInfo {
            peer_id: format!("sender-{}", &id[..8]),
            file_id: id.clone(),
            has_chunks: (0..nchunks).collect(),
            lan_addr: lan_ip.as_ref().map(|ip| format!("{}:{}", ip, LAN_PORT)),
            joined_at: Utc::now(),
        };
        st.peers.insert(sender_peer.peer_id.clone(), sender_peer.clone());
        if let Some(mut peers) = st.file_peers.get_mut(&id) {
            peers.push(sender_peer.peer_id.clone());
        }

        st.files.insert(id.clone(), rec);
        st.path_index.insert(key, id);
    }
}

async fn register_sandbox_path(st: &AppState, path: PathBuf, name: String) -> Option<SandboxFileAddedEvent> {
    let key = path.to_string_lossy().to_string();
    if st.path_index.contains_key(&key) {
        return None;
    }

    let md = tokio::fs::metadata(&path).await.ok()?;
    if !md.is_file() {
        return None;
    }
    let size = md.len();
    let nchunks = std::cmp::max(1, ((size + CHUNK_SIZE - 1) / CHUNK_SIZE) as u32);
    let mime = guess_mime(&name);

    let id = Uuid::new_v4().to_string();
    let rec = FileRecord {
        id: id.clone(),
        original_name: name.clone(),
        total_size: size,
        chunk_count: nchunks,
        mime_type: mime.clone(),
        created_at: Utc::now(),
        path: key.clone(),
        download_count: AtomicWrap(Arc::new(AtomicU64::new(0))),
    };

    let (tx, _) = broadcast::channel::<SignalMsg>(256);
    st.signal_bus.insert(id.clone(), tx.clone());
    st.file_peers.insert(id.clone(), vec![]);

    let lan_ip = st.lan_ip.read().await.clone();
    let sender_peer = PeerInfo {
        peer_id: format!("sender-{}", &id[..8]),
        file_id: id.clone(),
        has_chunks: (0..nchunks).collect(),
        lan_addr: lan_ip.as_ref().map(|ip| format!("{}:{}", ip, LAN_PORT)),
        joined_at: Utc::now(),
    };
    st.peers.insert(sender_peer.peer_id.clone(), sender_peer.clone());
    if let Some(mut peers) = st.file_peers.get_mut(&id) {
        peers.push(sender_peer.peer_id.clone());
    }

    let pub_url = st.public_url.read().await.clone();
    let base = if pub_url.is_empty() { format!("http://localhost:{PORT}") } else { pub_url };
    let dl_url = format!("{}/get/{}", base, id);

    st.files.insert(id.clone(), rec);
    st.path_index.insert(key, id.clone());

    Some(SandboxFileAddedEvent {
        file_id: id,
        name,
        total_size: size,
        chunk_count: nchunks,
        mime_type: mime,
        download_url: dl_url,
    })
}

async fn sandbox_watch_loop(st: AppState) {
    let mut pending: HashMap<String, (u64, u8)> = HashMap::new(); // path -> (last_size, stable_ticks)
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(900));
    loop {
        interval.tick().await;

        let mut current: HashSet<String> = HashSet::new();
        let mut rd = match tokio::fs::read_dir(&st.sandbox_dir).await {
            Ok(r) => r,
            Err(_) => continue,
        };

        while let Ok(Some(ent)) = rd.next_entry().await {
            let name = ent.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let md = match ent.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !md.is_file() {
                continue;
            }
            let path = ent.path();
            let key = path.to_string_lossy().to_string();
            current.insert(key.clone());

            let size = md.len();
            let entry = pending.entry(key.clone()).or_insert((size, 0));
            if entry.0 == size {
                entry.1 = entry.1.saturating_add(1);
            } else {
                entry.0 = size;
                entry.1 = 0;
            }
            if entry.1 < 2 {
                continue;
            }

            if !st.path_index.contains_key(&key) {
                if let Some(ev) = register_sandbox_path(&st, path, name).await {
                    if let Some(app) = &st.app {
                        let _ = app.emit("sandbox-file-added", ev);
                    }
                }
            }
        }

        pending.retain(|k, _| current.contains(k));

        let mut to_remove: Vec<(String, String)> = vec![]; // (path, file_id)
        for it in st.path_index.iter() {
            if !current.contains(it.key()) {
                to_remove.push((it.key().clone(), it.value().clone()));
            }
        }

        for (path_key, file_id) in to_remove {
            if tokio::fs::metadata(&path_key).await.is_ok() {
                continue;
            }
            st.path_index.remove(&path_key);
            st.files.remove(&file_id);
            if let Some((_, peer_ids)) = st.file_peers.remove(&file_id) {
                for pid in peer_ids {
                    st.peers.remove(&pid);
                }
            }
            st.signal_bus.remove(&file_id);
            if let Some(app) = &st.app {
                let _ = app.emit("sandbox-file-removed", SandboxFileRemovedEvent { file_id });
            }
        }
    }
}

// ── HTTP handlers ──────────────────────────────────────────────────────────────

// POST /upload  — multipart, no full-file buffering per chunk
async fn upload_handler(
    State(st): State<AppState>,
    mut mp: Multipart,
) -> impl IntoResponse {
    let mut results = vec![];

    while let Some(field) = mp.next_field().await.unwrap_or(None) {
        let name  = match field.file_name() { Some(n) => n.to_string(), None => continue };
        let mime  = field.content_type().unwrap_or("application/octet-stream").to_string();

        let id    = Uuid::new_v4().to_string();
        let fpath = unique_path_in(&st.upload_dir, &name);

        // Stream upload directly to disk (no full-file buffering).
        let mut out = match File::create(&fpath).await {
            Ok(f) => f,
            Err(_) => continue,
        };
        let mut size: u64 = 0;
        let mut field = field;
        while let Some(chunk) = field.chunk().await.unwrap_or(None) {
            size += chunk.len() as u64;
            if out.write_all(&chunk).await.is_err() {
                size = 0;
                break;
            }
        }
        let _ = out.flush().await;
        if size == 0 && std::fs::metadata(&fpath).is_err() {
            continue;
        }

        let nchunks = std::cmp::max(1, ((size + CHUNK_SIZE - 1) / CHUNK_SIZE) as u32);

        let rec = FileRecord {
            id: id.clone(),
            original_name: name.clone(),
            total_size: size,
            chunk_count: nchunks,
            mime_type: mime,
            created_at: Utc::now(),
            path: fpath.to_string_lossy().to_string(),
            download_count: AtomicWrap(Arc::new(AtomicU64::new(0))),
        };

        st.path_index.insert(rec.path.clone(), id.clone());

        // Create signaling bus for this file
        let (tx, _) = broadcast::channel::<SignalMsg>(256);
        st.signal_bus.insert(id.clone(), tx.clone());
        st.file_peers.insert(id.clone(), vec![]);

        // Register sender as a full-seeder peer
        let lan_ip = st.lan_ip.read().await.clone();
        let sender_peer = PeerInfo {
            peer_id: format!("sender-{}", &id[..8]),
            file_id: id.clone(),
            has_chunks: (0..nchunks).collect(),
            lan_addr: lan_ip.as_ref().map(|ip| format!("{}:{}", ip, LAN_PORT)),
            joined_at: Utc::now(),
        };
        st.peers.insert(sender_peer.peer_id.clone(), sender_peer.clone());
        if let Some(mut peers) = st.file_peers.get_mut(&id) {
            peers.push(sender_peer.peer_id.clone());
        }

        let pub_url = st.public_url.read().await.clone();
        let base    = if pub_url.is_empty() { format!("http://localhost:{PORT}") } else { pub_url.clone() };
        let dl_url  = format!("{}/get/{}", base, id);
        let qr_svg  = make_qr(&dl_url).unwrap_or_default();

        // Broadcast file-ready signal
        let _ = tx.send(SignalMsg::FileReady {
            file_id: id.clone(),
            name: name.clone(),
            total_size: size,
            chunk_count: nchunks,
            lan_addr: lan_ip.as_ref().map(|ip| format!("{}:{}", ip, PORT)),
        });

        if let Some(app) = &st.app {
            let _ = app.emit("file-ready", FileReadyEvent {
                file_id: id.clone(),
                name: name.clone(),
                total_size: size,
                chunk_count: nchunks,
                download_url: dl_url.clone(),
                lan_url: lan_ip.as_ref().map(|ip| format!("http://{}:{}/get/{}", ip, PORT, id)),
                qr_svg,
            });
        }

        st.files.insert(id.clone(), rec);
        st.bytes_up.fetch_add(size, Ordering::Relaxed);

        results.push(serde_json::json!({
            "id": id, "name": name,
            "size_human": format_size(size, BINARY),
            "chunk_count": nchunks,
            "download_url": dl_url,
        }));
    }

    Json(serde_json::json!({ "files": results }))
}

// GET /meta/:file_id  — returns file metadata + chunk list
async fn meta_handler(
    Path(fid): Path<String>,
    State(st): State<AppState>,
) -> impl IntoResponse {
    match st.files.get(&fid) {
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error":"not found"}))).into_response(),
        Some(f) => {
            let peers: Vec<serde_json::Value> = st.file_peers
                .get(&fid).map(|ids| ids.iter().filter_map(|pid| {
                    st.peers.get(pid).map(|p| serde_json::json!({
                        "peer_id": p.peer_id,
                        "has_chunks": p.has_chunks,
                        "lan_addr": p.lan_addr,
                    }))
                }).collect()).unwrap_or_default();

            Json(serde_json::json!({
                "id": f.id,
                "name": f.original_name,
                "total_size": f.total_size,
                "chunk_count": f.chunk_count,
                "mime_type": f.mime_type,
                "peers": peers,
                "lan_port": LAN_PORT,
            })).into_response()
        }
    }
}

// GET /chunk/:file_id/:chunk_index  — serve a single raw chunk (HTTP fallback)
async fn chunk_handler(
    Path((fid, cidx)): Path<(String, u32)>,
    State(st): State<AppState>,
) -> Response {
    let rec = match st.files.get(&fid) {
        Some(r) => r.clone(),
        None => return (StatusCode::NOT_FOUND, "file not found").into_response(),
    };

    if rec.total_size == 0 {
        if cidx != 0 {
            return (StatusCode::NOT_FOUND, "chunk not found").into_response();
        }
        let mut headers = HeaderMap::new();
        headers.insert("Content-Type", HeaderValue::from_static("application/octet-stream"));
        headers.insert("Content-Length", HeaderValue::from_static("0"));
        headers.insert("X-Chunk-Index", HeaderValue::from_static("0"));
        headers.insert("Cache-Control", HeaderValue::from_static("no-store"));
        headers.insert("Access-Control-Allow-Origin", HeaderValue::from_static("*"));
        return (StatusCode::OK, headers, Vec::<u8>::new()).into_response();
    }

    let offset = (cidx as u64) * CHUNK_SIZE;
    if offset >= rec.total_size {
        return (StatusCode::NOT_FOUND, "chunk not found").into_response();
    }
    let size = CHUNK_SIZE.min(rec.total_size - offset);

    let mut file = match File::open(&rec.path).await {
        Ok(f) => f,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "disk error").into_response(),
    };
    file.seek(std::io::SeekFrom::Start(offset)).await.ok();

    let mut buf = vec![0u8; size as usize];
    if file.read_exact(&mut buf).await.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, "read error").into_response();
    }

    st.bytes_down.fetch_add(size, Ordering::Relaxed);

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("application/octet-stream"));
    headers.insert("Content-Length", size.to_string().parse().unwrap());
    headers.insert("X-Chunk-Index", cidx.to_string().parse().unwrap());
    headers.insert("Cache-Control", HeaderValue::from_static("no-store"));
    headers.insert("Access-Control-Allow-Origin", HeaderValue::from_static("*"));

    (StatusCode::OK, headers, buf).into_response()
}

// GET /get/:file_id  — full-file HTTP fallback (assembles chunks on the fly)
async fn fullfile_handler(
    Path(fid): Path<String>,
    State(st): State<AppState>,
) -> Response {
    let rec = match st.files.get(&fid) {
        Some(r) => r.clone(),
        None => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    rec.download_count.0.fetch_add(1, Ordering::Relaxed);
    st.bytes_down.fetch_add(rec.total_size, Ordering::Relaxed);

    let data = tokio::fs::read(&rec.path).await.unwrap_or_default();
    let mut headers = HeaderMap::new();
    headers.insert("Content-Disposition",
        format!("attachment; filename=\"{}\"", rec.original_name).parse().unwrap());
    headers.insert("Content-Type", rec.mime_type.parse()
        .unwrap_or_else(|_| "application/octet-stream".parse().unwrap()));
    headers.insert("Content-Length", data.len().to_string().parse().unwrap());
    headers.insert("Cache-Control", HeaderValue::from_static("no-store"));
    headers.insert("X-Powered-By", HeaderValue::from_static("GimmeURL-HybridEngine"));

    (StatusCode::OK, headers, data).into_response()
}

// GET /signal/:file_id?peer_id=xxx&lan_addr=yyy  — SSE signaling stream
#[derive(Deserialize)]
struct SignalQuery { peer_id: Option<String>, lan_addr: Option<String> }

async fn signal_handler(
    Path(fid): Path<String>,
    Query(q): Query<SignalQuery>,
    State(st): State<AppState>,
) -> Response {
    let tx = match st.signal_bus.get(&fid) {
        Some(t) => t.clone(),
        None => return (StatusCode::NOT_FOUND, "no such file").into_response(),
    };
    let rx = tx.subscribe();

    let peer_id  = q.peer_id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let lan_addr = q.lan_addr;

    // Register peer
    let rec = st.files.get(&fid).map(|f| f.chunk_count).unwrap_or(0);
    let peer = PeerInfo {
        peer_id: peer_id.clone(),
        file_id: fid.clone(),
        has_chunks: vec![],
        lan_addr: lan_addr.clone(),
        joined_at: Utc::now(),
    };
    st.peers.insert(peer_id.clone(), peer.clone());
    if let Some(mut plist) = st.file_peers.get_mut(&fid) {
        if !plist.contains(&peer_id) { plist.push(peer_id.clone()); }
    }

    // Announce new peer to others
    let _ = tx.send(SignalMsg::PeerJoined {
        peer_id: peer_id.clone(),
        lan_addr: lan_addr.clone(),
        has_chunks: vec![],
    });

    // Also emit swarm update to GUI
    if let Some(app) = &st.app {
        let peer_count = st.file_peers.get(&fid).map(|p| p.len()).unwrap_or(0);
        let _ = app.emit("swarm-update", SwarmEvent {
            file_id: fid.clone(),
            peer_count,
            chunks_available: rec as usize,
        });
    }

    // SSE stream from broadcast channel
    let stream = BroadcastStream::new(rx).filter_map(|msg| async move {
        msg.ok().and_then(|m| {
            serde_json::to_string(&m).ok().map(|json| {
                Ok::<_, std::convert::Infallible>(
                    axum::response::sse::Event::default().data(json)
                )
            })
        })
    });

    let mut headers = HeaderMap::new();
    headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));
    headers.insert("X-Accel-Buffering", HeaderValue::from_static("no"));
    headers.insert("Access-Control-Allow-Origin", HeaderValue::from_static("*"));

    Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("ping"))
        .into_response()
}

// POST /signal/:file_id  — send a signal message (offer/answer/ICE/chunk-update)
async fn signal_post_handler(
    Path(fid): Path<String>,
    State(st): State<AppState>,
    Json(msg): Json<SignalMsg>,
) -> impl IntoResponse {
    // If it's a chunk update, also update peer registry
    if let SignalMsg::ChunkUpdate { ref peer_id, ref new_chunks } = msg {
        if let Some(mut p) = st.peers.get_mut(peer_id) {
            let existing: HashSet<u32> = p.has_chunks.iter().cloned().collect();
            for c in new_chunks {
                if !existing.contains(c) { p.has_chunks.push(*c); }
            }
        }
    }
    match st.signal_bus.get(&fid) {
        Some(tx) => { let _ = tx.send(msg); StatusCode::OK }
        None     => StatusCode::NOT_FOUND,
    }
}

// GET /peers/:file_id  — list current peers and their chunk maps
async fn peers_handler(
    Path(fid): Path<String>,
    State(st): State<AppState>,
) -> impl IntoResponse {
    let peer_ids = st.file_peers.get(&fid).map(|p| p.clone()).unwrap_or_default();
    let peers: Vec<serde_json::Value> = peer_ids.iter().filter_map(|pid| {
        st.peers.get(pid).map(|p| serde_json::json!({
            "peer_id": p.peer_id,
            "has_chunks": p.has_chunks,
            "lan_addr": p.lan_addr,
            "age_secs": Utc::now().signed_duration_since(p.joined_at).num_seconds(),
        }))
    }).collect();
    Json(serde_json::json!({ "file_id": fid, "peers": peers }))
}

// ── Speed telemetry loop ───────────────────────────────────────────────────────
async fn telemetry_loop(st: AppState) {
    let mut prev_up   = 0u64;
    let mut prev_down = 0u64;
    let mut interval  = tokio::time::interval(std::time::Duration::from_millis(250));
    loop {
        interval.tick().await;
        let up   = st.bytes_up.load(Ordering::Relaxed);
        let down = st.bytes_down.load(Ordering::Relaxed);
        let up_mbps   = ((up   - prev_up)   as f64) / (0.25 * 1_048_576.0);
        let down_mbps = ((down - prev_down) as f64) / (0.25 * 1_048_576.0);
        prev_up   = up;
        prev_down = down;
        let peers = st.peers.len();
        if let Some(app) = &st.app {
            let _ = app.emit("speed", SpeedEvent { up_mbps, down_mbps, active_peers: peers });
        }
    }
}

// ── Cleanup loop ───────────────────────────────────────────────────────────────
async fn cleanup_loop(st: AppState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        interval.tick().await;
        let now = Utc::now();
        // Remove stale peers (> 5 min no activity)
        let stale: Vec<String> = st.peers.iter()
            .filter(|p| now.signed_duration_since(p.joined_at).num_minutes() > 5)
            .filter(|p| !p.peer_id.starts_with("sender-"))
            .map(|p| p.peer_id.clone()).collect();
        for pid in stale { st.peers.remove(&pid); }
    }
}

// ── Tauri entrypoint ───────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
                .setup(|app| {
            let handle = app.handle().clone();

            // Resolve sandbox folder (visible to the user) and ensure it exists.
            let sandbox_dir = app.path().document_dir()
                .or_else(|_| app.path().home_dir())
                .map(|mut p| { p.push("GimmeURL Sandbox"); p })
                .unwrap_or_else(|_| {
                    let mut p = std::env::temp_dir();
                    p.push("gimmeurl_sandbox");
                    p
                });
            if let Err(e) = std::fs::create_dir_all(&sandbox_dir) {
                eprintln!("⚠️  Could not create sandbox dir {:?}: {}", sandbox_dir, e);
            }

            let lan_ip = local_ip_address::local_ip().ok().map(|ip| ip.to_string());
            println!("GimmeURL | LAN IP: {:?} | Sandbox dir: {:?}", lan_ip, sandbox_dir);

            let state = AppState {
                files:      Arc::new(DashMap::new()),
                peers:      Arc::new(DashMap::new()),
                file_peers: Arc::new(DashMap::new()),
                signal_bus: Arc::new(DashMap::new()),
                upload_dir: sandbox_dir.clone(),
                sandbox_dir: sandbox_dir.clone(),
                path_index: Arc::new(DashMap::new()),
                public_url: Arc::new(RwLock::new(String::new())),
                lan_ip:     Arc::new(RwLock::new(lan_ip.clone())),
                bytes_up:   Arc::new(AtomicU64::new(0)),
                bytes_down: Arc::new(AtomicU64::new(0)),
                app: Some(handle),
            };

            // STATE.set only fails if called twice — safe to ignore
            let _ = STATE.set(state.clone());

            // Emit LAN IP to GUI immediately
            if let Some(ref app_handle) = state.app {
                let _ = app_handle.emit("lan-ip", serde_json::json!({ "ip": lan_ip }));
            }

            // Import any existing files already present in the sandbox folder.
            import_existing_sandbox_files_sync(&state, &lan_ip);

            // Watch for new files added/removed in the sandbox folder.
            tauri::async_runtime::spawn(sandbox_watch_loop(state.clone()));

            tauri::async_runtime::spawn(async move {
                let cors = tower_http::cors::CorsLayer::new()
                    .allow_origin(tower_http::cors::Any)
                    .allow_methods(tower_http::cors::Any)
                    .allow_headers(tower_http::cors::Any);

                let router = Router::new()
                    .route("/upload",             post(upload_handler))
                    .route("/meta/:fid",          get(meta_handler))
                    .route("/chunk/:fid/:cidx",   get(chunk_handler))
                    .route("/get/:fid",           get(fullfile_handler))
                    .route("/signal/:fid",        get(signal_handler).post(signal_post_handler))
                    .route("/peers/:fid",         get(peers_handler))
                    .route("/health",             get(|| async { "gimmeurl" }))
                    .with_state(state.clone())
                    .layer(cors)
                    .layer(DefaultBodyLimit::disable());

                tokio::spawn(telemetry_loop(state.clone()));
                tokio::spawn(cleanup_loop(state));

                let addr = SocketAddr::from(([0, 0, 0, 0], PORT));
                match tokio::net::TcpListener::bind(addr).await {
                    Ok(listener) => {
                        println!("⚡ Server on :{PORT}");
                        if let Err(e) = axum::serve(listener, router).await {
                            eprintln!("⚠️  Server error: {}", e);
                        }
                    }
                    Err(e) => {
                        eprintln!("⚠️  Could not bind to port {PORT}: {}", e);
                    }
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            set_tunnel_url, get_lan_ip, get_active_files, delete_file, get_qr,
            get_sandbox_folder, open_sandbox_folder,
            open_file_in_sandbox,
        ])
        .run(tauri::generate_context!())
        .expect("tauri error");
}
