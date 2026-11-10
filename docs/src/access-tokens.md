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
- Tokens are **read-only**. A scope's `right` must be `read`, and the rights
  check refuses every write and append through a token, even on a resource
  whose ACL lets anyone write.
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

### Signed requests

`POST /tokens`, `GET /tokens` and a key-signed `DELETE /tokens/{id}` must be
signed by the agent's own key with a **version 2** request signature
(`x-atomic-signature-version: 2`), which covers the method and the body.
Version 1 proofs, cookies and bearer tokens are refused with 401. On top of the
usual checks:

- **Bound to this node's host.** The URL the signature must cover uses the host
  the node is configured for (`--domain`, or the host of `--server-url`), with
  the port and scheme of the request (`X-Forwarded-Proto` honoured). Behind a
  proxy, send `X-Forwarded-Host` with that host. A request for any other host,
  including `localhost` and served-domain-suffix names that other endpoints
  accept, is checked against the configured origin and fails. So a request
  signed for a laptop's `http://localhost:9883/tokens` cannot be replayed to a
  production node of the same agent.
- **In-cluster callers.** `/tokens` is served only on the configured host.
  A caller that connects to the in-cluster Service (for example
  `http://atomic-server.home-lab.svc.cluster.local:9883`) sends
  `X-Forwarded-Host: atomic.zay.home` and `X-Forwarded-Proto: https`, and
  signs `https://atomic.zay.home/tokens…`. Without those headers the server
  checks the signature against its configured origin
  (`http://atomic.zay.home:9883` when it does not terminate TLS itself), which
  a signature for the public URL does not match.
- **Charged first.** Every signed request, a list included, spends the agent's
  write rate budget (`--write-rate-limit`, keyed on the canonical
  `atomic:agent:` spelling) before anything else is done; over budget is 429.
- **Good once.** Then the signature is used up: the server remembers it in
  memory until it could no longer verify (its timestamp plus 5 minutes and 10
  seconds), and the same signed request sent again gets 401 ("already used"),
  whatever happened to it: minted, refused or failed. Sign a new request to
  retry. At most 250 000 signatures are remembered at once; when that many are
  still within their five minutes, further signed requests get 429 until some
  age out. A restart forgets them: within five minutes after a restart, a
  request signed before it could be sent again. A repeated mint that had
  minted then meets its nonce (409), a repeated revoke is idempotent and a
  repeated list only lists; a mint that had been refused (rate limit, 403)
  could mint.

There is no per-address limit. The server sees only the socket peer, which
behind the ingress is the ingress for every client, including the OpenBao
plugin, so such a limit would throttle all of them together.

### `POST /tokens`: mint

Signed as above.

```json
{
  "ttl_seconds": 90000,
  "scopes": [{ "target": "atomic:…", "right": "read" }],
  "label": "openbao:atomic:agent-sandbox:lease-0001",
  "nonce": "0123456789abcdef"
}
```

- `ttl_seconds`: from 1 up to the server maximum (`--access-token-max-ttl`,
  env `ATOMIC_ACCESS_TOKEN_MAX_TTL`, default 90000 = 25 h, at least 300). A
  larger value is refused with 400. It is not shortened.
- `scopes`: 1 to 32 entries. `right` must be `read`; `write`, `append` or
  anything else is refused with 400. An empty list is refused with 400. A
  target is at most 512 bytes. Targets are stored with the `atomic:` scheme.
- `label`: optional, up to 256 bytes.
- `nonce`: required, 1 to 128 bytes, no control characters.

**409** for a nonce this agent already used (see below) is answered before the
checks that follow, so a retried mint learns about its earlier token even if
the agent lost read on a target since.

**403 Forbidden** unless the agent can read every target now, through rights
of its own. A token only narrows rights its agent holds, so a target it cannot
read, one that does not exist on this node, or one hosted elsewhere refuses
the whole request. So does a target anyone may read (a public drive, and every
agent's own resource): a token over public data adds nothing, so a key that
was never granted anything cannot mint.

**429 Too Many Requests** when the agent already holds 200 live (unrevoked,
unexpired) tokens, or when its write rate budget is spent. The budget is keyed on the agent's
canonical spelling, so `did:ad:agent:…` and `atomic:agent:…` share it.

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
counts here for as long as it is retained: until it expires, and never less
than 5 minutes and 10 seconds after it was minted, whether it is still live
or revoked. The nonce is written in the same batch as the token, so a mint
either happened with its nonce or not at all. The body is the earlier token's view (as for
`GET /tokens/self`) plus an `error` string. It never includes the token value,
which is not stored. So a retried mint cannot create a second token.

### `GET /tokens/self`: introspect

`Authorization: Bearer atk_…`. **200** with the token's view: `token_id`,
`agent`, `scopes`, `label`, `nonce`, `created_at`, `expires_at`.
**401** if the token is unknown, revoked or expired. The message names which.

### `GET /tokens`: list

Signed as above. A bearer token gets 401.
**200** with an array of the agent's **live** tokens, oldest first. Each entry is
the same view as `/tokens/self`. Token values are never included. Revoked and
expired tokens are not listed.

### `DELETE /tokens/{token_id}`: revoke

Either:

- signed by the token's agent as above (empty body), or
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

A record is kept until the token expires (and at least 5 minutes and 10
seconds after it was minted), then dropped by the next mint, which reads only
the expired part of an expiry-ordered index. Mints and revokes are serialized
by one lock and run on actix's blocking pool, not on the workers that serve
requests. Used signatures are kept in memory, not in the store.

## Known costs and limits

- **Uncached rights walks.** A rights check through a token runs the agent's
  check and the scope walk without the per-request rights cache (the cache
  holds one agent's verdicts, and this check asks as two). A large `/query`
  through a token walks more parents than the same query signed by the key.
- **What other code sees.** A token's identity displays as
  `urn:atomic-access-token:<token_id>`, not as its agent. Code that reads the
  agent as a string sees that pseudo-subject, for example WASM class
  extenders. It is in no ACL, so such code grants it nothing.
- **Earlier registrations on a reused socket.** A token `AUTH` on a socket
  that already registered Loro-sync or presence subscriptions as a key-signed
  agent re-checks its commit subscriptions (`SUB`) against the token, but keeps
  those earlier Loro-sync and presence registrations. Open a new socket for a
  token session.
- **`/tokens` paths.** `/tokens`, `/tokens/self` and `/tokens/{id}` are routed
  to these endpoints before resources, so a resource stored at one of those
  paths can no longer be read by its path.
