# Atomic Data Versioning

When Atomic Commits are applied to some Resource, the resource will change.
However, its identifier (the Subject) will often remain the same.

- Versioned representations should provide a link to the authority that might update it, and a link to where the latest version can be found.
- The latest version should have a link to its permanent version.
- Should [IPFS](../interoperability/ipfs.md) content-hash URLs be used for Versioned resources?

## Versioned Resources

Properties:

<!-- Maybe this is not required, if we assume that the subject URL should always show the latest? -->
- latest: (ResourceArray, optional)
- versions: (ResourceArray, optional)
- currentVersion: (ResourceURL, required)

## Static Resource

A static resource has a _content addressable_ URL, which means that its URL will never change.

## Hashing

- Serialize all Atoms of the Subject (the entire Resource) as Atomic-NDJSON
- Sort all lines (every atom) alphabetically


## Who signed a version

The Loro document of a resource holds every change, but not the proof of who
made it. That proof is the signed commit itself, so after applying a commit a
node can keep it. Kept commits live in a side tree on the node (internally
`Tree::Envelopes`, the code's name for a stored commit). They are not resources
and are not indexed: they never appear in queries or collections. By default, a
node retains the newest envelope per resource (`--envelope-retention latest`).
Set a resource's `retention` property to `audit` to keep every commit for it and
its descendants; `--envelope-retention all` retains every commit node-wide.

A kept commit is stored compactly when its signed `loroUpdate` can be reconstructed byte for byte: raw update bytes, signer and previous-commit ids, with signature, creation time and subject in the row key. Reading rebuilds the exact signed JSON-AD from the row alone, without the Loro document; otherwise the full JSON is retained. Signatures are checked on import and by `/history-attribution`, not on a plain row read. Undecodable rows are reported as `unreadable`, make history incomplete, and are never guessed, substituted for newer rows or pruned on a wrong assumption.

When a node applies a signed commit, it records the Loro op IDs (`peer`, `counter`, `length`) the commit's `loroUpdate` added, and which ops the server wrote while applying it (including derived values). This maps History versions to their signer; client-chosen change messages are not evidence. Each commit's change token is carried in its message and the kept commit that introduced it carries the same token inside `loroUpdate`.

A resource can request an audit log by setting `retention` to `audit` (default `compact`): the node keeps every commit of that resource and its descendants. The setting is inherited and only raises retention; `compact` cannot lower an audited ancestor, and `--envelope-retention all` audits everything. Only `compact` and `audit` are accepted. Audit protects commits from then on; already-dropped commits are not recovered. Removing audit or moving a resource out of an audited folder does not delete the audit-era commits; the lowering commit is retained with them, while subsequent commits again replace the newest row. Writers can change `retention`; who may lower it remains an open question for an owner level. A receiver with a newer commit still drops an older incoming one, and a node missing an ancestor retains incoming commits until it can read the whole chain. Retention applies independently on each node.

`GET /history-attribution?subject=<subject>` returns, for a resource the
caller may read:

```json
{
  "subject": "atomic:…",
  "retention": "all",
  "attribution_source": "change-ids",
  "complete": true,
  "attributions": [
    {
      "signer": "atomic:agent:…",
      "created_at": 1757060000000,
      "signature": "…",
      "commit_id": "did:ad:commit:…",
      "verified": true,
      "tokens": ["c-1a07140ba9b-uvdzz0"],
      "destroy": false,
      "genesis": false,
      "spans": [{ "peer": "7203840239481", "counter": 3, "length": 1 }],
      "server_spans": [{ "peer": "9930172045113", "counter": 0, "length": 1 }]
    }
  ],
  "changes": [
    {
      "peer": "7203840239481",
      "counter": 3,
      "length": 1,
      "lamport": 7,
      "timestamp": 1757060000000,
      "message": "c-1a07140ba9b-uvdzz0",
      "origin": "signed",
      "attribution": 0,
      "signer": "did:ad:agent:…"
    }
  ]
}
```

Each entry in `changes` is one Loro change of the stored document. `origin` is `signed` when all its ops came from one envelope whose signature verifies here, `unverified` when that envelope's signature does not verify, `server` for node-written ops, `unattributed` when no signed commit is recorded here, or `ambiguous` when more than one source covers it. `verified` means the answering node re-checked the signature using its commit-application verification. `complete` means every client-authored change in the oplog is claimed by a verified kept commit. `spans` is null for an envelope not applied locally (for example, imported via bulk push or vault pack); its ops are then unattributed on this node. `tokens` lists change messages for display only. A version no kept commit covers is shown as *Unattributed*; a signer is never guessed.
