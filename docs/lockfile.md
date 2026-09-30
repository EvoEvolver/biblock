# Lockfile, integrity, and trust model

`biblock` keeps bibliography content and workflow state separate. This document
describes the current `1.1` lockfile format and its guarantees. The project is
still pre-1.0, so this is an implementation contract rather than a permanently
frozen external specification.

## Files and invariants

For `references.bib`, the default sidecar is `references.bib.lock`.

The bibliography remains standard BibTeX. It never contains `integrity`,
`bibsource`, `bibprovider`, `bibprevious`, `@bibsource`, or `@bibversion` workflow
metadata. The lockfile is deterministic JSON with key-sorted maps.

Its top-level sections are:

- `lockfileVersion` and `toolVersion`, which identify the format and writer;
- `bibliography.contentHash`, which binds the complete clean bibliography;
- `entries`, keyed by citation key, containing canonical content, current source,
  integrity state, provider identity, human approval, a snapshot, and the history head;
- `sources`, containing content-addressed provider, web, resolution, search,
  selection, human, and agent evidence;
- `revisions`, containing content-addressed prior entry states.
- `deletedEntries`, retaining deleted entry state and its history head;
- `proposals`, retaining immutable agent replacements and human decisions.

```json
{
  "lockfileVersion": "1.1",
  "toolVersion": "0.12.0",
  "bibliography": { "contentHash": "..." },
  "entries": {
    "turing1936": {
      "contentHash": "...",
      "snapshot": {
        "entry_type": "article",
        "entry_key": "turing1936",
        "fields": { "title": "..." }
      },
      "source": "bibsource:provider:...",
      "integrity": {
        "contentHash": "...",
        "source": "bibsource:provider:..."
      },
      "approval": {
        "id": "...",
        "kind": "human",
        "method": "browser-review",
        "reviewer": "workstation-name",
        "target": "turing1936",
        "contentHash": "...",
        "timestamp": 1789516800,
        "batchId": "..."
      },
      "head": "rev:8f31c9d0"
    }
  },
  "sources": {},
  "revisions": {}
}
```

Deleting the lockfile leaves valid BibTeX, but discards provenance, integrity,
and history. Commit it during normal development; omit it only from submission
artifacts that do not accept sidecar files.

## Lifecycle

Create or synchronize the lockfile after an external edit:

```sh
biblock lock references.bib --sync --actor alice
```

Synchronization records changed entry states but does not approve the new
content or silently inherit previous integrity. If an existing lockfile is out of
sync, mutating commands stop until this synchronization is explicit.

Require a current lockfile without writing:

```sh
biblock lock references.bib --frozen
```

This is the intended CI check. An interrupted two-file update is detected through
the bibliography hash: the bibliography is replaced first and the lockfile is
written last as the commit marker.

## Edit history

Every effective in-place edit stores the prior entry snapshot and workflow state.
A revision contains:

- the citation key it belongs to;
- the previous revision ID;
- operation and actor labels;
- a Unix timestamp;
- the prior clean BibTeX record and its hash;
- the prior lockfile entry state;
- the full revision SHA-256.

Revision IDs begin with an eight-hex-digit content-hash prefix. If a prefix
collides, the new ID is extended. The complete digest remains in the object.

```sh
biblock history status references.bib
biblock history log references.bib --key paper1
biblock history show references.bib --revision 8f31c9d0
biblock history diff references.bib --revision 8f31c9d0
biblock history restore references.bib --revision 8f31c9d0 --in-place
```

Restore is itself a recorded edit, so it can be reversed. Dry runs, stdout-only
output, and no-op writes do not create revisions.

Deleted entries keep their last snapshot and revision head in `deletedEntries`.
Restoring a deleted snapshot reinserts the citation key and moves it back to
`entries`. Validation checks history heads, target ownership and chain links.

## Agent proposals

The optional `proposals` map is keyed by a full content-addressed `proposal:...`
ID. Each proposal stores `target`, `agent`, `reason`, `timestamp`, `baselineHash`,
`proposalHash`, clean `before` and `after` records, and optional JSON `evidence`.
The ID covers this immutable payload, not the eventual `decision`. Neither
snapshot includes workflow fields. Citation keys must match the target.

A decision contains `outcome` (`adopted` or `rejected`), `reviewer`, `timestamp`,
and the adoption `revision` when an entry state changed. That link is validated
against the proposal's original snapshot.
No decision means pending; proposals and decisions are retained after review.
Supporting evidence is not automatically treated as API authority. Adoption
of a freeform proposal requires a matching current baseline and grants an explicit content-bound human
approval while recording the previous entry in normal edit history. Pending
proposals do not alter entry trust, but prevent the diagnosis readiness gate.

Provider-backed proposals additionally store a typed `provider` object containing
the exact projected record, title-search candidates, immutable receipts and the
matching threshold. This object is covered by the proposal ID and validated
against the replacement and receipt projection hashes. Existing freeform proposal
IDs are unchanged. Eligible agent adoption writes `decision.agent`, provider
integrity and history, without a human approval. Human review remains available
for low-score or ambiguous provider proposals. No API lookup happens on adoption.

Both new maps default to empty when reading existing `1.0` lockfiles. Writes use
version `1.1` so older writers reject the file instead of silently discarding
proposals or deleted-entry history. Existing `1.0` files require no manual conversion.

## Integrity calculation

Entry integrity is deterministic:

1. Parse the entry and resolve BibTeX string macros.
2. Lowercase the entry type and field names.
3. Exclude the citation key and every workflow metadata field.
4. Serialize the remaining fields plus `ENTRYTYPE` as compact, key-sorted UTF-8
   JSON.
5. Hash the serialization with SHA-256.
6. Store the digest with the source object that authorized it.

The citation key is excluded so a pure key rename does not alter bibliographic
content. Workflow metadata is excluded so evidence about an entry cannot change
the content being evidenced.

Integrity states are:

| State | Meaning |
| --- | --- |
| `verified` | Integrity is valid and backed by a provider API or explicit human approval |
| `valid` | Integrity and provenance are consistent, but backed only by agent or web evidence |
| `stale` | An integrity record exists, but covered content changed |
| `invalid` | Integrity or source history is missing or inconsistent |

## Evidence chains

Provider receipts retain the provider and stable record ID, request URL and
method, optional POST body SHA-256, media type, response SHA-256, deterministic
projection, and projection hash. The current provider-controlled BibTeX fields
must match that projection. Built-in providers include Crossref, DOI content
negotiation, and public OpenReview notes.

When a URL was resolved first, the provider receipt links to resolution evidence
containing the original and final URLs, matched identifier, signals, and any
network-response hash. When a search candidate was selected, it also links to the
query, candidate set, response hash, chosen ID, and selector identity.

Threshold-authorized agent selection also records the combined match score,
separate title and author scores, the threshold, and the fixed rule
`0.7*title+0.3*author`. The threshold does not select a record by itself: the agent
must read the comparison and explicitly adopt the provider ID. Browser selection
uses the same search-to-selection-to-provider chain with method `browser-review`.

Web receipts bind an existing entry to a fetched page without claiming that a
literature provider verified it. Human and agent sources bind an actor label and
the entry content they approved.

Browser approval is an independent overlay on the entry rather than a replacement
source. It records a content-addressed approval ID, target key, current content
hash, timestamp, batch ID, and optional reviewer label. The label defaults to the
computer name in the UI, but may be changed or omitted and is not an authenticated
identity. Editing the entry invalidates the approval while revision history keeps
the prior evidence.

The review UI can also replace an entry from pasted BibTeX. It accepts exactly
one regular entry, preserves the current citation key, previews every field
change, and records the replacement plus its content-bound human approval as one
atomic `human-paste` revision.

Structured values such as candidates, signals, and projections are native JSON,
not JSON strings, so they remain directly queryable with `jq`.

## Trust boundary

The lockfile provides tamper evidence, attribution, reproducible projections,
and a durable decision trail. It lets a reviewer distinguish exact provider
verification from a human or agent assertion and detect later content changes.

It does not provide:

- a cryptographic identity for a human or agent actor label;
- proof that a remote service served particular bytes merely because their hash
  was recorded;
- permanent availability of the remote record;
- proof that provider metadata is factually correct;
- automatic correctness for a human-selected fuzzy match.

Those stronger guarantees require signed receipts, archived responses, or
independent validation. `biblock` preserves the evidence it actually has rather
than presenting these claims as equivalent.

## Git and concurrent edits

The lockfile is designed for version control and deterministic diffs. If two
branches edit different references, resolve both the `.bib` and `.bib.lock`
changes rather than discarding one audit trail. After resolving the bibliography,
run `biblock lock FILE --frozen`; use `--sync --actor ACTOR` only when intentionally
recording an external state.

Automatic semantic lockfile merging is not yet part of the CLI. A future stable
format must define cross-implementation merge and migration rules before the
lockfile can be treated as a broad interoperability standard.

## Legacy migration

Files written by versions before 0.9 may contain embedded workflow fields and
`@bibsource` entries. `biblock lock FILE --sync` or the next successful in-place
edit moves that state into JSON and removes workflow metadata from the `.bib`.

Old response bodies can be stripped while retaining compact receipt metadata:

```sh
biblock source strip-responses references.bib --in-place
```
