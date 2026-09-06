# Any Player Sync Server

A fast, lightweight backend for syncing Any Player app state across clients.

This service stores and syncs:
- app state
- playlists
- provider configuration
- general settings

## Tech stack

- Rust (`axum`, `tokio`)
- PostgreSQL (`sqlx`)
- WebSocket push notifications (`/v1/ws`)

## Run locally

```bash
cargo run
```

Environment variables:
- `BIND_ADDRESS` (default: `127.0.0.1:8080`)
- `DB_HOST` (default: `127.0.0.1`)
- `DB_PORT` (default: `5432`)
- `DB_USER` (default: `postgres`)
- `DB_PASSWORD` (default: `postgres`)
- `DB_NAME` (default: `any_player_sync`)
- `DB_SSLMODE` (default: `prefer`)
- `ADMIN_BOOTSTRAP_NAME` (default: `admin`)
- `ADMIN_BOOTSTRAP_TOKEN` (optional; if set, this token is activated for the bootstrap admin account)
- `DJ_MODEL_PATH` (optional; absolute path to the on-device AI DJ model `.task` file - see "AI DJ model hosting" below)
- `DJ_MODEL_VERSION` (default: `unversioned`; a label for the configured model, used by clients for cache-busting)
- `DJ_VOICE_MODELS_MANIFEST_PATH` (optional; path to the operator-owned AI DJ voice catalog JSON - see "AI DJ neural voice hosting" below)
- `DJ_VOICE_MODEL_PATH` (optional single-voice compatibility fallback; absolute path to an AI DJ Piper/VITS voice bundle `.zip`)
- `DJ_VOICE_MODEL_VERSION` (default: `unversioned`; a label for the configured voice bundle, used by clients for cache-busting)

Examples:

```bash
BIND_ADDRESS=0.0.0.0:8080 \
DB_HOST=127.0.0.1 \
DB_PORT=5432 \
DB_USER=postgres \
DB_PASSWORD=postgres \
DB_NAME=any_player_sync \
DB_SSLMODE=disable \
ADMIN_BOOTSTRAP_TOKEN=replace-with-strong-token \
cargo run
```

## Authentication and user isolation

All `/v1/*` sync endpoints require:

```http
Authorization: Bearer <token>
```

Each token is tied to a user. Snapshots and namespace updates are isolated per user, so one user's token cannot read or modify another user's sync state.

WebSocket auth:
- Preferred: `Authorization: Bearer <token>` header
- Browser fallback: `GET /v1/ws?token=<token>` (**avoid in production** — the token appears in access logs, browser history, and reverse-proxy logs)

## API summary

### Health

- `GET /health`

### Snapshot (all synced domains)

- `GET /v1/snapshot`
- `GET /v1/snapshot?since_version=<number>` returns `304 Not Modified` when unchanged
- `PUT /v1/snapshot`

`PUT /v1/snapshot` request body:

```json
{
  "expected_version": 2,
  "client_id": "desktop-abcd",
  "app_state": {},
  "playlists": [],
  "provider_configuration": {},
  "settings": {}
}
```

### Per-domain state

- `GET /v1/state/app-state`
- `PUT /v1/state/app-state`
- `GET /v1/state/playlists`
- `PUT /v1/state/playlists`
- `GET /v1/state/provider-configuration`
- `PUT /v1/state/provider-configuration`
- `GET /v1/state/settings`
- `PUT /v1/state/settings`

`PUT /v1/state/*` request body:

```json
{
  "expected_version": 2,
  "client_id": "android-xyz",
  "data": {}
}
```

If `expected_version` is provided and does not match current server version, server returns `409`.

### Realtime updates

- `GET /v1/ws` (WebSocket)

Message format:

```json
{
  "event_type": "state_updated",
  "namespace": "playlists",
  "version": 12,
  "updated_at": "2026-02-26T16:00:00Z",
  "source_client_id": "desktop-main"
}
```

## Admin UI

- `GET /admin` serves a basic admin web UI.
- Admin API endpoints are under `/v1/admin/*` and require an **admin** bearer token.

Supported admin operations:
- List users/tokens
- Create users
- Create tokens
- Revoke tokens
- Enable/disable users

## AI DJ model hosting

The Android app's optional "AI DJ" feature runs a small on-device LLM (e.g. Gemma 3 1B,
converted to a MediaPipe `.task` bundle) entirely on-device, but that file is too large
to ship in the app itself and requires accepting the model's own license. This server
never downloads, converts, or redistributes the model on your behalf - you place your
own license-accepted `.task` file on disk and point `DJ_MODEL_PATH` at it:

```bash
DJ_MODEL_PATH=/srv/any-player/dj-models/gemma3-1b-it-int4.task \
DJ_MODEL_VERSION=gemma3-1b-it-int4-v1 \
cargo run
```

Endpoints (both require the same `Authorization: Bearer <token>` as the sync API):
- `GET /v1/dj-model/info` - returns `{ "version", "size_bytes", "sha256" }`, or `404` if `DJ_MODEL_PATH` isn't set/readable.
- `GET /v1/dj-model/download` - streams the model file, honoring `Range` requests so an interrupted client download can resume.

The file's sha256 is hashed once at server startup, not per-request.

### AI DJ neural voice hosting

The Android app synthesizes the DJ's speech on-device using an offline neural TTS
engine ([sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) running a Piper/VITS
voice), for a more natural voice than the system TTS engine. As with the LLM model,
this server just hosts the file - you provide a zip containing `<voice>.onnx` +
`tokens.txt` (the shared `espeak-ng-data` phoneme tables ship inside the app itself,
since they're identical across voices) and point `DJ_VOICE_MODEL_PATH` at it:

```bash
DJ_VOICE_MODEL_PATH=/srv/any-player/dj-models/en_US-hfc_male-medium.zip \
DJ_VOICE_MODEL_VERSION=en_US-hfc_male-medium-v1 \
cargo run
```

The app keeps the downloaded bundle matching `DJ_VOICE_MODEL_VERSION` active, so changing
that version lets operators switch voices without an app release. The example uses the
single-speaker `vits-piper-en_US-hfc_male-medium` bundle; it uses speaker ID `0` and is
male-oriented. Validate the perceived depth on target hardware before deploying it.

Endpoints (same auth, same shape as the LLM model endpoints above):
- `GET /v1/dj-voice-model/info`
- `GET /v1/dj-voice-model/download`

#### Voice catalog deployment

Set `DJ_VOICE_MODELS_MANIFEST_PATH` to an operator-owned JSON file:

```json
{
  "default_id": "baritone",
  "voices": [
    {
      "id": "baritone",
      "name": "Deep Baritone",
      "version": "2026-09-06",
      "path": "/srv/any-player/dj-voices/baritone.zip"
    }
  ]
}
```

Every `path` is an absolute path on the server to an existing regular zip file.
Each zip is a complete voice bundle containing a `.onnx` model and
`tokens.txt`. Each `id`, `version`, and optional `default_id` is 1-128 ASCII
bytes, starts with an ASCII letter or digit, and otherwise contains only ASCII
letters, digits, `.`, `_`, or `-`. IDs are unique, and `default_id`, when set,
must match an entry. Omitting `default_id` is valid and leaves selection to the
user. The server reads, validates, and hashes the catalog at startup; restart it
after changing the manifest or a bundle.

The operator is responsible for obtaining and complying with each bundle's
license. The catalog contains only configured bundles; other bundles are not
automatically available. Clients cannot provide arbitrary download URLs,
filesystem paths, or speaker-ID configuration.

On Android, open Settings, tap **Refresh voices**, select a catalog voice, tap
**Download** explicitly, wait until it is ready, then play. Refreshing or
selecting does not download the bundle.

If `DJ_VOICE_MODELS_MANIFEST_PATH` is unset, the existing
`DJ_VOICE_MODEL_PATH` and `DJ_VOICE_MODEL_VERSION` configuration remains a
compatible single voice with ID `default`. The legacy
`GET /v1/dj-voice-model/info` and `GET /v1/dj-voice-model/download` endpoints
remain available. With a manifest, those endpoints serve the optional default
voice and return `404` when no default is configured.

Catalog endpoints use the same bearer authentication as the legacy endpoints:

- `GET /v1/dj-voice-models`
- `GET /v1/dj-voice-models/{voice_id}/download`

## Integration guide

See [docs/app-integration.md](docs/app-integration.md) for full app integration flow.
