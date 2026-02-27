# App Integration Guide

This document defines how Any Player apps (desktop + android) should integrate with the sync backend.

## Goals

- Keep app state consistent across clients.
- Avoid overwriting newer remote changes.
- Enable near-real-time updates between active clients.

## Synced domains

The server stores four JSON domains:

- `app_state`
- `playlists`
- `provider_configuration`
- `settings`

Each write increments a global `version`.

## Client startup flow

1. Generate a stable `client_id` per install/device.
2. Open WebSocket connection to `GET /v1/ws`.
3. Fetch snapshot with `GET /v1/snapshot`.
4. Hydrate local state from response.
5. Store returned `version` as `lastSyncedVersion`.

## Write flow (optimistic concurrency)

When writing any domain:

1. Send `PUT /v1/state/<namespace>` with:
   - `expected_version = lastSyncedVersion`
   - `client_id`
   - `data`
2. If response is `200`, update local domain and set `lastSyncedVersion = response.version`.
3. If response is `409`, a newer write exists:
   - fetch `GET /v1/snapshot`
   - merge or prefer server state based on domain policy
   - retry write if still needed

Recommended merge policy:
- `app_state`: prefer latest server by default
- `playlists`: merge by playlist ID where possible
- `provider_configuration`: server wins unless local user is actively editing credentials
- `settings`: key-level merge, server wins on conflict timestamp ties

## Realtime sync flow

On WebSocket event `state_updated`:

1. Ignore if `source_client_id == this_client_id`.
2. If `event.version <= lastSyncedVersion`, ignore.
3. Otherwise fetch `GET /v1/snapshot?since_version=<lastSyncedVersion>`:
   - if `304`, no-op
   - if `200`, apply snapshot and update `lastSyncedVersion`

## Payload contracts

### `PUT /v1/state/<namespace>`

```json
{
  "expected_version": 10,
  "client_id": "desktop-main",
  "data": { "...": "..." }
}
```

### `PUT /v1/snapshot`

```json
{
  "expected_version": 10,
  "client_id": "android-phone",
  "app_state": {},
  "playlists": [],
  "provider_configuration": {},
  "settings": {}
}
```

## Error handling

- `400`: invalid input/namespace.
- `409`: optimistic concurrency conflict.
- `500`: backend/storage issue.

All errors follow:

```json
{
  "error": {
    "code": "version_conflict",
    "message": "expected version 4, but current version is 5"
  }
}
```

## Security notes (for production)

Current backend is intentionally minimal, does not enforce auth, and uses a single global sync state. On its own it is **not safe to expose** to any untrusted network.

You MUST only:
- run this backend directly on `localhost`/a strictly trusted local network **or**
- place it behind an authenticated, access-controlled reverse proxy that enforces API authentication and per-user account scoping.

For any non-localhost or production-like deployment, you MUST ensure at least:
- TLS termination
- API authentication (token/session or equivalent) enforced either by the backend or the proxy
- Per-user account scoping (separate rows/documents per user or tenant, not a single global state)
- Rate limiting and request size limits (set `MAX_BODY_SIZE` and proxy-level limits)

To restrict allowed CORS origins, set the `CORS_ALLOWED_ORIGINS` environment variable to a comma-separated list of allowed origins (e.g. `http://localhost:3000,http://localhost:4200`). If unset, all origins are permitted.

## Suggested app abstractions

Both apps should implement a sync client module with:

- `fetchSnapshot(): Promise<Snapshot>`
- `putNamespace(namespace, data, expectedVersion): Promise<UpdateResponse>`
- `subscribeUpdates(onEvent): Unsubscribe`
- `resolveConflict(local, remote, namespace): MergedData`

This keeps sync behavior consistent across desktop and android codebases.
