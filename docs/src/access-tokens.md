# Access tokens

An agent can ask the server for a bearer token that acts as that agent, narrowed
to scopes it chooses, for a limited time. The token lets a process read without
holding the agent's private key. A token cannot sign commits, so writes still
need the key.

The server stores only a SHA-256 of each token. Every request looks the token up
again: there is no cache, so a revoke, an expiry or an ACL change applies to the
next request.

## What a token may do

At each request, the effective right is the intersection of the agent's
current rights and the token's scopes, plus whatever the public agent may do
anyway (the bearer could simply ask anonymously):

- A scope `{target, right}` covers `target` and its descendants. It uses the
  same `drive` and `parent` walk as the rights check
  ([Hierarchy](hierarchy.md)). A Commit is covered where its target is.
- `write` covers read, write and append, as a `write` ACL entry does. `read` and
  `append` cover only themselves.
- A scope never adds a right the agent lacks. Removing the agent from an ACL
  also removes the right from its live tokens.

Tokens are accepted only in these places:

- **Resource reads**: `GET /resource?subject=…`, path subjects, and the endpoints
  served through them, such as `/query`. Pass the token as
  `Authorization: Bearer atk_…`.
- **WebSocket `AUTH`**: see below.
- **`/tokens/self`** and **self-revoke**.

Every other endpoint answers 401 to an access token (for example `/search`,
`/export`, `/commit` and the plugin endpoints). The older
`Authorization: Bearer <base64 auth proof>` keeps working where it worked before.
The `atk_` prefix tells the two apart.

## Endpoints

All bodies are JSON. Times are Unix milliseconds.

### `POST /tokens`: mint

The request must be signed by the agent's own key with a **version 2** request
signature (`x-atomic-signature-version: 2`), which covers the method and the
body. Version 1, cookies and bearer tokens are refused with 401.

```json
{
  "ttl_seconds": 90000,
  "scopes": [{ "target": "atomic:…", "right": "read" }],
  "label": "openbao:atomic:agent-sandbox:lease-0001",
  "nonce": "0123456789abcdef"
}
```

- `ttl_seconds`: from 1 up to the server maximum (`--access-token-max-ttl`,
  env `ATOMIC_ACCESS_TOKEN_MAX_TTL`, default 90000 = 25 h). A larger value is
  refused with 400. It is not shortened.
- `scopes`: 1 to 32 entries. `right` is `read`, `write` or `append`. An empty
  list is refused with 400. Targets are stored with the `atomic:` scheme.
- `label`: optional, up to 256 bytes.
- `nonce`: required, 1 to 128 bytes.

**201 Created**:

```json
{
  "token_id": "32 hex chars",
  "token": "atk_…",
  "agent": "atomic:agent:…",
  "scopes": [{ "target": "atomic:…", "right": "read" }],
  "label": "…",
  "nonce": "…",
  "created_at": 1794000000000,
  "expires_at": 1794090000000
}
```

This is the only time the token value is ever returned. `expires_at` is
`created_at + ttl_seconds * 1000`. From that instant on the token is refused.

**409 Conflict**: the agent already minted a token with this `nonce`. A token
counts here until the TTL cap has passed since it was minted, whether it is
still live, revoked or expired. The body is the earlier token's view (as for
`GET /tokens/self`) plus an `error` string. It never includes the token value,
which is not stored. So a retried mint cannot create a second token.

### `GET /tokens/self`: introspect

`Authorization: Bearer atk_…`. **200** with the token's view: `token_id`,
`agent`, `scopes`, `label`, `nonce`, `created_at`, `expires_at`.
**401** if the token is unknown, revoked or expired. The message names which.

### `GET /tokens`: list

Signed by the agent's key, with version 1 or 2. A bearer token gets 401.
**200** with an array of the agent's **live** tokens, oldest first. Each entry is
the same view as `/tokens/self`. Token values are never included. Revoked and
expired tokens are not listed.

### `DELETE /tokens/{token_id}`: revoke

Either:

- signed by the token's agent with a version 2 signature (empty body), or
- `Authorization: Bearer` with the token itself. A token can only revoke
  itself.

**204** when a live token was revoked. **404** when there is no live token with
this id that belongs to the caller: unknown, another agent's, already revoked,
or expired. Revoking closes the token's WebSocket sessions at once.

Mint and revoke spend the agent's write rate budget, the same budget as its
signed commits.

## WebSocket

Send `AUTH` (0x01) with the JSON payload `{"accessToken": "atk_…"}` instead of a
signed proof. The answer is `AUTH_OK`, or `ERROR` with `AUTH_FAILED` (8).

A token session is read-only. It may send `AUTH`, `GET`, `GET_MANY`, `SUB`,
`UNSUB`, `KEEPALIVE` and `HELLO`, plus the text frames that subscribe or
unsubscribe to index status and unsubscribe from Loro sync and presence. Any
other frame gets an `ERROR`: `UNAUTHORIZED_WRITE` (3) for binary frames and
`UNAUTHORIZED_READ` (7) for text frames. Subscriptions are checked with the
token's narrowing, also when a parent subscription is fanned out.

The server closes a token session with close code 1008 (policy) when:

- the token is revoked (at once),
- the token expires (a timer set to `expires_at`; no frame is sent after it),
- or a liveness check, run every heartbeat (5 s), finds the token no longer
  valid.

A signed `AUTH` on the same socket replaces the token session.

## Commits

A token cannot commit. Commits are self-authenticating: each one is signed by
the key of its `signer`. A new resource's identifier is its creator's signature
over a genesis certificate (`atomic:<sig>`). ACL, parent and destroy commits are
kept as signed envelopes that replicas verify. None of that can be produced
without the key. An unsigned commit sent with a bearer token is refused like any
other unsigned commit.

## Storage, restart and backup

Tokens live in their own store tree, `access_tokens_v1`, in the same redb file
as everything else. They survive a restart, and startup compaction (redb's
in-place compact) keeps them. A backup that copies the database file includes
them.
Only hashes are stored, so a copied backup holds no usable token.

They are **not** included in `atomic-server export` (the JSON-AD backup of
resources written under the config directory's `backups/`), in `/export` (CSV
of resources) or in the recovery
export (`POST /recovery/export`, a bundle of selected resources and their Loro
state): a token is not a resource. An agent that
needs tokens on a restored node mints new ones.

Records are dropped when the TTL cap has passed since they were minted and they
have expired. That happens on the next mint.
