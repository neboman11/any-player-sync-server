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

Examples:

```bash
BIND_ADDRESS=0.0.0.0:8080 \
DB_HOST=127.0.0.1 \
DB_PORT=5432 \
DB_USER=postgres \
DB_PASSWORD=postgres \
DB_NAME=any_player_sync \
DB_SSLMODE=disable \
cargo run
```

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

## Integration guide

See [docs/app-integration.md](docs/app-integration.md) for full app integration flow.
