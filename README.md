# GimmeURL

GimmeURL is a hybrid file-transfer app that turns a local file into a shareable URL. Receivers download directly from the sender using the fastest available path:

- LAN direct (same network)
- WebRTC data channels (P2P swarm)
- HTTP fallback (sender-hosted chunk and full-file endpoints)

Files are never uploaded to a central storage service. A lightweight signaling/metadata service is used only to coordinate peers.

## Live URL (signaling + web receiver)

The default public signaling/receiver instance is:

- https://gimmeurl.vercel.app/

The desktop app uses this by default for metadata and WebRTC signaling.

## Download (desktop apps)

Prebuilt desktop apps are published as GitHub Release assets for tagged versions (tags like `v2.0.0`).

- macOS: `.dmg` (and a `.zip` of the `.app` bundle)
- Windows: `.msi` and NSIS `.exe`
- Linux: `.AppImage` and `.deb`

Download the latest build from:

- [GitHub Releases](../../releases)

## What’s in this repository

This repo contains the desktop app:

- Tauri v2 shell
- A Rust (Axum) local server for uploads, chunk serving, and local signaling
- A minimal HTML/CSS/JS UI in `src/`

The hosted signaling/receiver service is a separate deployment (the app points at it via `SIGNAL_BASE`).

## How it works

### Components

1. **Desktop app (sender)**
   - Stores files on disk (in an OS-visible sandbox folder).
   - Splits files into fixed-size chunks (256 KB).
   - Registers metadata with the signaling server.
   - Seeds chunks to receivers and other peers over WebRTC.

2. **Signaling/metadata service (hosted)**
   - Stores a small metadata record per file (name, size, chunk count, MIME, thumbnail, sender addresses).
   - Tracks whether the sender is online.
   - Relays signaling messages (offer/answer/ICE) between peers.

3. **Receiver UI (web)**
   - Loads metadata for a `fileId`.
   - Connects to the swarm.
   - Requests and verifies chunks; re-requests missing chunks when needed.
   - Falls back to HTTP if WebRTC is unavailable.

### Sender flow (desktop app)

At a high level:

1. A file is added either by drag-and-drop in the app UI or by dropping it into the OS sandbox folder.
2. The Rust backend stores the file on disk and computes `chunk_count` using a fixed chunk size (256 KB).
3. The frontend registers metadata with the hosted signaling server (file name, size, MIME, chunk count, sender URLs, sandbox ID, optional thumbnail).
4. The frontend starts seeding:
  - It polls the hosted signaling endpoint for that `fileId`.
  - When a receiver joins, it creates a WebRTC connection and serves requested chunks.

### Receiver flow (web)

1. Open a link like `/?get=<fileId>`.
2. The receiver fetches `/api/meta/<fileId>` to load name/size/chunk count and to confirm the sender is online.
3. The receiver joins signaling for the file and negotiates WebRTC.
4. The receiver requests chunks by index and assembles the final file.
5. If WebRTC cannot connect, the receiver falls back to HTTP using the sender’s advertised URL(s).

### Transfer paths

GimmeURL prefers the fastest available path:

- **WebRTC P2P swarm** (default)
  - Receivers connect to the sender and can also exchange chunks with each other.
  - Chunks are requested by index; peers report which chunks they have.

- **LAN direct**
  - When a LAN address is available, receivers can fetch directly on the local network.

- **HTTP fallback**
  - The sender’s local server exposes chunk and full-file endpoints.
  - Used as a fallback when WebRTC negotiation fails or is too slow.

### Metadata, presence, and link validity

A file link is considered valid only while the sender is online.

- The sender registers metadata for a file ID.
- The signaling service marks the sender “online” with a short TTL.
- The sender keeps that presence alive by periodically polling the signaling endpoint for each active file.
- If the sender stops refreshing presence (app closed, offline), metadata requests return `sender_offline`.

Practically: keep the app running while others download.

### Hosted API contract (what the app expects)

The default hosted instance (`https://gimmeurl.vercel.app/`) provides endpoints used by the app and the web receiver:

- `GET /api/health` — health check
- `POST /api/meta/:fileId` — register file metadata (includes `sandbox_id` and optional `thumb_data`)
- `GET /api/meta/:fileId` — fetch file metadata (returns 404 if the sender is offline)
- `DELETE /api/meta/:fileId` — remove metadata when unsharing
- `GET /api/signal/:fileId` — fetch signaling events for a `peer_id` (clients reconnect/poll)
- `POST /api/signal/:fileId` — send signaling messages (offer/answer/ICE, etc.)
- `GET /api/sandbox/:sandboxId` — list files currently online for a sandbox

### OS-synced sandbox folder

The desktop app uses a real folder on your machine as the source of truth:

- Folder name: `GimmeURL Sandbox`
- Location: your system Documents directory (falls back to your home directory if needed)

Behavior:

- Dropping a file into the folder automatically adds it to the app’s sandbox list.
- The app automatically registers metadata and starts seeding so the file becomes shareable immediately.
- Deleting a file from the folder removes it from the app and unshares it (stops seeding and deletes hosted metadata).

To open the folder from the app UI, use the **Open folder** button.

### Thumbnails

The sender generates a small thumbnail (data URL) and includes it in metadata:

- Images: a resized preview is generated.
- Non-images: a simple generated tile is used.

This thumbnail is displayed consistently across devices (sandbox tiles and receiver screens).

## Using the app

### Share a file

1. Launch the app.
2. Drag-and-drop a file onto the upload area (or select files).
3. Copy the generated link.
4. Send the link to the receiver.

The link looks like:

- `https://gimmeurl.vercel.app/?get=<fileId>`

### Share a sandbox

Each app instance has a sandbox ID. Share the sandbox URL to expose your current online sandbox contents:

- `https://gimmeurl.vercel.app/?sandbox=<sandboxId>`

Receivers can browse available files and open a file’s link (with auto-start enabled).

### Remove/unshare

- Delete from the app UI, or delete the file from the `GimmeURL Sandbox` folder.
- The app stops seeding and removes hosted metadata.

## Local server (desktop app)

The desktop app runs a local HTTP server on port `3737` (bound to `0.0.0.0` so other devices on the same network can reach it when allowed by your firewall).

Key endpoints (local):

- `POST /upload` — stream-upload multipart files directly to disk
- `GET /chunk/:fid/:cidx` — fetch a single chunk (HTTP fallback)
- `GET /get/:fid` — download the full file (HTTP fallback)
- `GET /meta/:fid` — file metadata and peer summaries (local)
- `GET /signal/:fid` — signaling stream (SSE)
- `POST /signal/:fid` — send signaling messages
- `GET /peers/:fid` — list peers and their chunk maps
- `GET /health` — local health check

## Development

### Prerequisites

- Node.js (for the Tauri CLI wrapper scripts)
- Rust toolchain (stable)
- Tauri CLI v2 (installed via this repo’s devDependencies)

### Run in development

From the repository root:

```bash
npm install
npm run dev
```

### Build a release bundle

```bash
npm run build
```

This runs the Tauri bundler and produces a platform-specific application bundle.

### Project layout

- `src/` — Tauri frontend (HTML/CSS/JS)
- `src-tauri/` — Rust backend (Axum server, filesystem sandbox, signaling)

## Configuration

### Signaling server base URL

By default, the app uses:

- `https://gimmeurl.vercel.app`

In the sender UI, the signaling base is read from:

- `localStorage.gimmeurl_signal_base` (if present)

If you self-host a compatible signaling service, set this value to point to your deployment.

## Troubleshooting

- **“Sender offline / file not found”**: the sender app is not running, or it lost connectivity. The link only works while the sender is online.
- **WebRTC fails**: restrictive NAT/firewalls can prevent peer connections. The receiver should fall back to HTTP when needed.
- **File does not appear immediately after dropping into the folder**: the sandbox watcher waits for file size to stabilize before registering it.

## Author

Maintained by `aryansrao`.
