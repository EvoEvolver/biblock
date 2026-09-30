# biblock

[![CI](https://github.com/EvoEvolver/biblock/actions/workflows/ci.yml/badge.svg)](https://github.com/EvoEvolver/biblock/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Safe, auditable BibTeX maintenance for humans and agents.**

`biblock` finds duplicate references, reconciles metadata with Crossref and
DOI.org, and records why every trusted change was made. Your bibliography stays
ordinary BibTeX. Provenance, integrity, and edit history live in a JSON sidecar
that agents and `jq` can inspect directly.

```sh
# Check verification state and get the next commands.
biblock diagnosis references.bib

# If no lockfile exists, create one. This does not verify entries.
biblock lock references.bib --sync

# Compare title/author matches, then freeze a chosen provider record.
biblock source match references.bib --key paper1
biblock source propose references.bib --key paper1 --id PROVIDER_ID --agent codex

# Read the saved diff and evidence. Adopt only if agentAdoptable is true
# and the agent confirms it is the same work; use the returned proposal ID.
biblock proposal list references.bib
biblock proposal adopt references.bib --id PROPOSAL_ID --agent codex --reviewed
```

Low-confidence matches and changes without provider support go to human review:
`biblock review references.bib`.

## Why biblock?

An agent can clean a bibliography quickly. The difficult question comes later:
which fields did it change, what source supported them, and which fuzzy match did
it choose?

`biblock` makes those decisions explicit:

- **No silent merges.** Title and author similarity produce review candidates,
  never automatic deletion or replacement.
- **Exact when possible.** Existing DOI and resolvable URL identifiers can be
  checked directly against literature providers.
- **Evidence-bearing edits.** Provider receipts, response hashes, selections,
  and attributed human or agent approvals are retained.
- **Clean BibTeX.** No custom entry types or workflow fields are added to the
  bibliography.
- **Reversible history.** In-place edits preserve the previous entry and can be
  inspected or restored.
- **Built for composition.** Stable JSON output works with `jq`, shell pipelines,
  CI, and agent harnesses.

The governing idea is simple: **agent edits should be evidence-bearing
transactions, while the canonical document remains clean and portable.**

## The two-file model

For a bibliography named `references.bib`, `biblock` maintains:

| File | Purpose |
| --- | --- |
| `references.bib` | Standard BibTeX used by LaTeX, editors, and submission systems |
| `references.bib.lock` | JSON provenance, integrity state, provider receipts, and edit history |

Commit both files during normal work. For a journal submission that only accepts
BibTeX, submit `references.bib` and leave out the lockfile. Deleting the lockfile
never invalidates the BibTeX; it only discards the audit trail.

## Install

### From source

The current main branch requires a recent Rust toolchain:

```sh
cargo install --git https://github.com/EvoEvolver/biblock --locked
biblock --version
```

### Prebuilt releases

Releases from `v0.11.0` onward provide binaries for Linux x86_64/ARM64 and macOS
Intel/Apple Silicon. The installer verifies the downloaded SHA-256 checksum:

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/EvoEvolver/biblock/main/install.sh | sh
```

Set `BIBLOCK_INSTALL_DIR` to choose the destination or `BIBLOCK_VERSION` to pin a
release. The default destination is `~/.local/bin`.

## Five-minute workflow

Start with `biblock diagnosis` to check all `.bib` files in the current directory,
or `biblock diagnosis references.bib` to check a specific file, for a local verification summary,
pending citation keys, and commands tailored to the current lockfile and trust
state. Its JSON includes executable argument arrays for agents. It makes no API
requests and writes nothing. Exit `0` means the lockfile is consistent, every
entry is verified, and no proposals are pending; `3` means work remains, and `2` means invalid input or an
operational error.

Start by inspecting the bibliography as JSON:

```sh
biblock inspect references.bib |
  jq '.[] | {id, title: .fields.title, status: .integrity.status}'
```

Find duplicate candidates across one or more files:

```sh
biblock dedupe references.bib imported.bib --min-score 0.8
```

`dedupe` exits with status `3` when candidates need review. Every candidate
contains both complete entries plus separate title and author scores. Duplicate
citation keys are rejected rather than treated as ambiguous references.

Verify the whole file against Crossref, with DOI content negotiation as fallback:

```sh
# Dry run: JSON report only.
biblock source verify references.bib --all

# Apply every exact match in one transaction.
biblock source verify references.bib --all --in-place
```

Search results are never selected just because they have the highest score. When
an entry is ambiguous, inspect its candidates and apply the chosen provider ID
explicitly:

```sh
biblock source match references.bib --key paper1 --min-score 0.9

biblock source propose references.bib \
  --key paper1 \
  --id 10.1234/chosen-record \
  --agent codex --min-score 0.9

# Read the saved replacement, diff, scores and provider receipts.
biblock proposal list references.bib
biblock proposal adopt references.bib --id proposal:... --agent codex --reviewed
```

`source match` searches a provider by title and reports separate title, author, and
combined scores (`0.7 * title + 0.3 * author`) plus field-level changes. The
threshold authorizes an agent to adopt a candidate after reading the comparison;
it does not trigger an automatic replacement. More than one qualifying candidate,
or any semantic doubt such as preprint versus published version, stays in review.

Crossref is the default. OpenReview is also available for public notes:

```sh
biblock source match references.bib --provider openreview --key paper1
biblock source propose references.bib --provider openreview --key paper1 \
  --id NOTE_ID --agent codex --min-score 0.9
```

Then inspect the evidence and validate the repository state:

```sh
biblock source trace references.bib --key paper1 | jq .
biblock review references.bib
biblock integrity status references.bib
biblock lock references.bib --frozen
```

`review` opens a local browser page showing every BibTeX field. Crossref and
OpenReview searches run only when their buttons are clicked, then show the full
post-adoption record beside the current entry. A reviewer can adopt a provider
record, paste and preview a complete replacement BibTeX entry, or approve the
existing entry. Pasted entries keep the current citation key and become verified
through a content-bound human approval. The reviewer label defaults to the
computer name, but can be changed or left blank. Each action is written
immediately and preserves the original provenance chain.

`integrity status` is the release gate: `verified` means the current content and
evidence chain are valid and the direct source is either an exact provider receipt
or an explicit human approval. Agent assertions and web receipts are `valid`, but
still require human approval.

## Designed for agent review

An agent can leave a complete replacement in the lockfile without changing the
bibliography. A human then compares every field and adopts or rejects it:

```sh
biblock proposal create references.bib --key paper1 --bibtex replacement.bib \
  --agent codex --reason 'Corrected metadata from the publisher'
biblock review references.bib
```

Freeform proposals retain the old entry, replacement, rationale, optional evidence,
and decision. Their adoption requires human approval and is reversible;
changed baselines are rejected. Pending proposals keep `diagnosis` from reporting the project ready.
See [workflows](docs/workflows.md) for hash-bound API previews and proposal details.

Provider-backed proposals follow the same saved-diff workflow, but an agent can
adopt a sufficiently matching, unambiguous candidate after reading it. Adoption
uses the frozen API evidence without another lookup and grants provider-backed
`verified` status, not a fabricated human approval.

The CLI includes the agent workflow and review boundaries in `biblock --help`,
`biblock source propose --help`, and `biblock proposal adopt --help`.
No separate agent skill is required.

`biblock` separates proposing a change from trusting it. An agent can inspect,
search, score, and prepare a field-level diff without gaining permission to make
an ambiguous choice. The final selection is explicit and remains visible in the
lockfile.

Commands accept citation keys directly or as newline-delimited input, so an agent
can operate on a narrow reviewed set:

```sh
biblock inspect references.bib |
  jq -r '.[] | select(.integrity.status == "invalid" and .integrity.source.key == null) | .id' |
  biblock integrity add references.bib --keys-from - \
    --source agent --agent codex --in-place
```

Agent and human approvals are attributed assertions. They are deliberately not
presented as provider verification or cryptographic identity.

## Core commands

| Command | What it does |
| --- | --- |
| `biblock diagnosis [FILE]` | Summarize local verification state and suggest next commands |
| `biblock source propose FILE --key KEY --id ID --agent AGENT` | Save a fixed provider replacement and evidence |
| `biblock proposal list FILE` | Read saved replacements, comparisons, scores and decisions |
| `biblock proposal adopt FILE --id ID --agent AGENT --reviewed` | Adopt an eligible provider proposal after agent review |
| `biblock inspect [FILE ...]` | Emit entries and trust state as JSON |
| `biblock dedupe [FILE ...]` | Generate title-and-author duplicate candidates |
| `biblock source verify FILE --all` | Check exact identifiers across a bibliography |
| `biblock source match FILE --key KEY` | Score Crossref title matches for agent review |
| `biblock source plan FILE --key KEY` | Show candidates and field-level changes |
| `biblock source apply FILE --key KEY` | Apply one explicit provider record |
| `biblock source trace FILE --key KEY` | Show the evidence chain for an entry |
| `biblock review FILE` | Review evidence and record content-bound human approval in a browser |
| `biblock integrity status FILE` | Require API verification or explicit human approval for every entry |
| `biblock lock FILE --frozen` | Fail if the lockfile is missing or out of sync |
| `biblock history log FILE --key KEY` | Show an entry's recorded revisions |
| `biblock history restore FILE --revision REV` | Restore a prior snapshot as a new edit |

Run `biblock --help` or any command with `--help` for the full option reference.

## What biblock proves

`biblock` makes workflow records tamper-evident and detects when bibliography
content changes after approval. Provider receipts bind the fields written by the
tool to the response and projection it processed.

It does not turn an agent label into a digital signature, and a response hash by
itself cannot prove that a remote API served those bytes. The lockfile preserves
the evidence available to a reviewer; it does not replace archival signatures or
independent source validation.

## Documentation

- [Workflows and command guide](docs/workflows.md)
- [Lockfile, integrity, history, and trust model](docs/lockfile.md)

`biblock` is pre-1.0. The CLI and lockfile are usable today, but the public
lockfile contract may still evolve before a stable 1.0 specification.

## Direction

The next milestones are a frozen, independently implementable lockfile schema;
semantic Git merge support; Zotero, editor, and CI integrations; and public case
studies measuring incorrect metadata, ambiguous matches, and review effort. The
goal is broader than one CLI: make evidence-bearing edits a normal expectation
for agents working on scholarly records.

Licensed under the [MIT License](LICENSE).
