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

Every applied commit leaves its signed JSON-AD on the resource it changed, in
a side tree on the node (`Tree::Envelopes`). It is not a resource and it is
not indexed: it never appears in queries or collections. A node keeps either
the envelope that produced the current state (`--envelope-retention latest`,
the default) or every envelope (`all`), which turns the Loro history into a
signed audit log.

When a node applies a signed commit, it records which Loro op IDs
(`peer`, `counter`, `length`) the commit's `loroUpdate` added to the stored
document, and which ops the server wrote on top while applying it (the
`lastCommit` stamp, the derived `drive`). That is how a version in History
maps to its signer. Change messages are chosen by the client, so they are
never used as evidence: a change without a message, or one reusing someone
else's message, is still attributed to the envelope that brought it in.

`GET /history-attribution?subject=<subject>` returns, for a resource the
caller may read:

```json
{
  "subject": "did:ad:…",
  "retention": "all",
  "attribution_source": "change-ids",
  "complete": true,
  "attributions": [
    {
      "signer": "did:ad:agent:…",
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

Each entry in `changes` is one Loro change of the stored document, with its
`origin`: `signed` (all its ops came in with one envelope whose signature
verifies here), `unverified` (one envelope, signature does not verify),
`server` (written by the node while applying envelopes), `unattributed` (some
op came in without a signed commit recorded here) or `ambiguous` (covered by
more than one source). `verified` means the answering node re-checked the
signature with the same code it applies commits with. `complete` means every
change is `signed` or `server`. `spans` is `null` for an envelope the node did
not apply itself (it arrived with a bulk push or a vault pack); its ops are
then unattributed on that node. `tokens` lists the messages of the changes an
envelope introduced, for display only. A version no envelope covers is shown
as *Unattributed*; a signer is never guessed.
