# Workflows and command guide

This guide covers the operational details intentionally omitted from the main
README. Run any command with `--help` for its complete option reference.

## Diagnose verification state

```sh
biblock diagnosis
biblock diagnosis references.bib
biblock diagnosis references.bib --compact | jq '{ready, lock, summary, actions}'
```

Without a filename, the command discovers all `.bib` files directly in the
current directory, sorted by filename. The JSON contains overall `ready`,
per-file reports in `files`, and parse or read failures in `errors`. Other files
are still diagnosed if one fails. Subdirectories are not scanned. An empty
directory or any unreadable or invalid bibliography exits `2`.

The command checks local integrity, approval, provenance, and lockfile history.
It never fetches metadata or changes files. `network_checked` is always false:
the report validates recorded evidence, not the current contents of a remote API.

`summary` counts all ordinary entries by trust state. `pending` identifies each
entry that is not verified, explains why, and supplies entry-specific commands.
Each pending entry's `recommended_path` distinguishes `api_verify` for DOI or
eprint references, `candidate_review` for title/author matches, and `human_review`
for web references or entries without enough metadata. Web receipts and agent
assertions alone do not make an entry verified. `templates` supplies complete
adoption commands with named `placeholders`; fill them only after reading and
selecting a candidate. Templates are not ready-to-run commands.

`actions` gives the next steps for the file; each action has a shell-quoted
`command`, an executable `argv` array, a `purpose`, and a `writes` flag. Batch API
commands select only pending literature keys, excluding human-review web entries.
Read candidates before explicitly adopting a
record with `source apply`; a similarity threshold alone is not approval.

`ready` requires a valid lockfile, every entry being verified, and no pending proposals. Exit `0`
means ready, `3` means further work is required, and `2` means unreadable or invalid
BibTeX. A missing or damaged lockfile is included in the JSON report. Review
external edits before running a suggested `lock --sync`: synchronization records
history but does not confer verification. Repair a damaged lockfile from its
recorded backup or version control before proceeding.

## Inspect and compose

`biblock inspect` emits one JSON array. Multiple files are combined into the same
array; use `-` as a filename, or omit filenames, to read BibTeX from stdin.

```sh
# List entries that need review.
biblock inspect references.bib |
  jq -r '.[] | select(.integrity.status != "verified") | .id'

# Build a compact agent review packet.
biblock inspect references.bib --compact |
  jq -c '[.[] | select(.integrity.status != "verified") |
    {id, type, title: .fields.title, doi: .fields.doi}]'

# Inspect several files together.
biblock inspect references.bib imported.bib

# Read from stdin.
cat references.bib | biblock inspect -
```

Duplicate citation keys, including duplicates across input files, are rejected.
`inspect` never modifies its input.

## Find duplicate candidates

`dedupe` compares entries that have both a title and an author. It normalizes TeX
commands, braces, punctuation, case, whitespace, and common author-name forms.
The score is:

```text
score = 0.7 * title_score + 0.3 * author_score
```

```sh
biblock dedupe references.bib imported.bib --min-score 0.8 |
  jq '.[] | {score, entries: [.entries[] | {file, id, fields}]}'
```

The output is a candidate packet, not a merge instruction. `biblock` never
deletes or merges entries automatically.

## Verify exact identifiers

Crossref is the default provider. The `doi` provider uses DOI content negotiation
with `Accept: application/x-bibtex`. List the installed providers with:

```sh
biblock source providers
```

Batch verification first tries exact identifiers such as an existing DOI, a DOI
URL, or a supported repository URL. Entries without an exact identifier return
ranked candidates but are never selected automatically.

```sh
# Report only.
biblock source verify references.bib --all

# Write all successful exact matches atomically.
biblock source verify references.bib --all --in-place

# Change provider order.
biblock source verify references.bib --all --providers doi,crossref
```

Set an email address for providers that support polite API identification:

```sh
export BIBLOCK_MAILTO=researcher@example.org
```

## Review ambiguous records

Plan a replacement before writing it:

```sh
biblock source plan references.bib --key paper1
```

The plan contains provider candidates and field-level changes. Compare title,
authors, publication year, venue, record type, and identifiers. Apply only the
chosen stable provider ID:

```sh
biblock source apply references.bib \
  --key paper1 \
  --id 10.1234/chosen-record \
  --selected-by alice \
  --baseline-hash BASELINE_HASH --proposal-hash PROPOSAL_HASH \
  --add-integrity \
  --in-place
```

With `--selected-by`, `apply` reruns the search and requires the chosen ID to be
present among its candidates. It also requires `baseline_hash` and `proposal_hash`
from the reviewed candidate report. A changed entry or exact provider replacement
is rejected without writing. `source match` fetches exact records before showing
the complete replacement, including removed fields. The lockfile records the query, candidate set,
selection actor, exact lookup, and resulting projection.

Provider-controlled fields are replaced by the deterministic provider
projection. Local fields such as `file`, `keywords`, `note`, and annotations are
preserved.

## Frozen provider proposals

```sh
biblock source propose references.bib --key paper1 --provider crossref \
  --id 10.1234/selected --agent codex --min-score 0.9
biblock proposal list references.bib | jq .
# Only after reading the saved comparison and confirming this is the same work:
biblock proposal adopt references.bib --id proposal:... --agent codex --reviewed
```

`source propose` performs title search and exact lookup, then saves the fixed
replacement, search/selection/provider receipts, candidates and threshold in the
lockfile. It does not change the bibliography. Adoption makes no API requests:
it applies exactly the saved replacement and records API-backed integrity, the
adopting agent and edit history, without fabricating a human approval.

Both the search candidate and exact lookup must meet the saved title/author
threshold. Another qualifying candidate blocks agent adoption. Missing title or
author, low scores, stale baselines and unsupported freeform proposals cannot be
agent-adopted. Use `biblock review references.bib` for human decisions. A finite
search window does not establish global uniqueness; agents must still assess
semantic identity, including preprints versus published versions.

Provider-controlled fields come from the API projection; existing local fields
such as notes are retained unchanged, not claimed to be API-authenticated.
`--reviewed` is an explicit recorded confirmation, not proof of cognition.

## Freeform agent proposals and human adoption

Create a current lockfile first with `biblock lock references.bib --sync` after
reviewing any external edits. Submit exactly one complete replacement entry:

```sh
biblock proposal create references.bib --key paper1 --bibtex replacement.bib \
  --agent codex --reason 'Corrected title, authors and venue' --evidence evidence.json
biblock proposal list references.bib | jq .
biblock review references.bib
```

Use `--bibtex -` to read the replacement from stdin. Its citation key is rewritten
to the selected target. Workflow fields are discarded. `--evidence` is optional
JSON supporting information, not a provider receipt or independent approval.
Creating a proposal requires a valid current lockfile and changes only that file.
It neither changes `.bib` nor grants any verification status.

The browser shows the original and replacement entry types and every field,
including unchanged and deleted fields, agent identity, rationale and evidence.
Adopt writes the fixed replacement, records a content-bound human approval and
preserves the previous state in history. Reject records a decision without
changing the bibliography. Both decisions remain in `proposals`; decided
proposals cannot be adopted again. If the current target differs from the saved
baseline, adoption fails and the agent must submit a fresh proposal. Review and
approve-all never implicitly adopt proposals or approve their old entries.

Pending proposals appear in `diagnosis` and prevent `ready`, even when all current
entries are already verified. CLI creation and listing are available to agents;
adoption and rejection are explicit browser review actions.

## Bind a batch write to a reviewed preview

```sh
biblock source verify references.bib --all > verify-preview.json
biblock source verify references.bib --all --in-place --reviewed verify-preview.json
```

The dry run includes exact proposed entries, full diffs and hashes. `--reviewed`
requires the current entry and replacement to match the saved preview; any mismatch
aborts the transaction before files are written. Without `--reviewed`, exact API
verification is a fresh operation, not a replay of an earlier preview. Current
API-backed and human-approved verified entries are skipped.

Browser candidate searches retain the exact provider records shown in the page.
Adoption consumes that snapshot without another API request. Pasted-entry and
ordinary approval requests are also bound to the displayed content hashes.

Deletion followed by `lock --sync` retains the deleted entry and its revision chain
under `deletedEntries`. `history log --key` works for deleted keys, and
`history restore --revision ... --in-place` can reinsert them.

## Resolve URLs

URL resolution extracts stable identifiers from the input URL, redirects,
publisher metadata, JSON-LD, and supported repository APIs:

```sh
biblock source resolve 'https://doi.org/10.1038/171737a0'
```

Conflicting identifiers require review and produce exit status `3`. Resolution
follows at most five redirects, limits ordinary responses to 2 MiB, and rejects
credentials, non-default ports, localhost, and non-public IP addresses.

For software, documentation, product pages, and other sources without a
literature-provider record, preserve the existing fields and bind them to the web
source:

```sh
biblock source web references.bib --key product-page --in-place
```

The receipt stores URLs, media type, byte count, response hash, and a content
hash. It does not embed the page body or claim provider verification.

## Inspect provenance

```sh
biblock source trace references.bib --key paper1 --compact | jq .
```

`trace` validates and emits the stored evidence chain without making another
network request.

## Add attributed integrity

Provider-backed integrity reuses an exact provider receipt:

```sh
biblock integrity add references.bib --key paper1 \
  --source provider --in-place
```

Human and agent review can be recorded explicitly:

```sh
biblock integrity add references.bib --key paper1 \
  --source human --reviewer alice --in-place

biblock integrity add references.bib --key draft1 \
  --source agent --agent codex --in-place
```

Selection is always explicit: pass one or more `--key` arguments,
`--keys-from FILE`, or `--all`. An empty `--keys-from -` selection is rejected.

```sh
biblock integrity status references.bib --json
biblock integrity hash references.bib paper1
biblock integrity remove references.bib --key paper1 --in-place
```

## CI

Commit the bibliography and lockfile, then fail CI if they diverge or any entry
lacks an exact provider API source or explicit human approval:

```sh
biblock lock references.bib --frozen
biblock integrity status references.bib
```

`integrity status` exits `0` when every selected entry is verified, `3` when any
entry is valid but not verified, stale, or invalid, and `2` for invalid input or an
operational error. Review-producing commands also use exit status `3` when human
or agent judgment is required.

`verified` requires valid integrity whose direct source is `provider` or `human`.
An internally consistent `agent` or `web` source is `valid`, but does not satisfy
the release gate. Missing integrity or provenance is `invalid`; a later content
change is `stale`.

## Command index

| Command | Purpose |
| --- | --- |
| `inspect [FILE ...]` | Emit entries and trust state as JSON |
| `dedupe [FILE ...]` | Find title-and-author duplicate candidates |
| `source providers` | List metadata providers |
| `source verify FILE --all` | Batch exact verification with provider fallback |
| `source web FILE --all` | Bind current entries to web-source receipts |
| `source resolve URL` | Resolve a URL into identifier candidates |
| `source search QUERY` | Search a provider |
| `source plan FILE --key KEY` | Produce candidates and field-level changes |
| `source apply FILE --key KEY` | Apply one exact provider record |
| `source trace FILE --key KEY` | Emit a stored evidence chain |
| `integrity status FILE` | Inspect integrity state |
| `integrity add FILE --key KEY` | Add provider, human, or agent approval |
| `integrity remove FILE --key KEY` | Remove approval state |
| `lock FILE --frozen` | Require an up-to-date lockfile |
| `lock FILE --sync` | Record an external BibTeX edit |
| `history status FILE` | Validate revision objects and links |
| `history log FILE --key KEY` | Show an entry's revision chain |
| `history show FILE --revision REV` | Print a stored snapshot |
| `history diff FILE --revision REV` | Compare a snapshot with the current entry |
| `history restore FILE --revision REV` | Restore a snapshot as a new edit |
