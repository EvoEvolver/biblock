use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use biblock_cli::bibtex::{Record, parse, render};
use biblock_cli::catalog::{
    BibliographicQuery, Candidate, FieldChange, LiteratureIdentifier, LiteratureRecord,
    PROVIDER_FIELD, PROVIDER_ID_FIELD, changes, title_search_query,
};
use biblock_cli::dedupe::{self, LocatedRecord};
use biblock_cli::diagnosis;
use biblock_cli::history;
use biblock_cli::inspect;
use biblock_cli::integrity::{
    APPROVAL_FIELDS, Status, hash, remove_entry_type_fields, status, update_entry_fields,
    update_entry_fields_exact, update_source,
};
use biblock_cli::proposal;
use biblock_cli::provenance::{self, CONTROLLED_FIELDS, SOURCE_FIELD, SOURCE_TYPE, SourceKind};
use biblock_cli::providers::{self, DEFAULT_PROVIDER};
use biblock_cli::resolver::{self, ResolutionReport};
use biblock_cli::review;
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

const LONG_ABOUT: &str = "Resolve literature URLs, reconcile BibTeX with pluggable metadata providers, inspect entries as JSON, find likely duplicates, and maintain source-bound verification in a JSON .bib.lock sidecar. The bibliography remains standard BibTeX. Crossref is the default provider; DOI content negotiation is also built in.";

const AFTER_HELP: &str = r#"DIAGNOSIS
  Summarize local lockfile and verification state, pending keys, and next commands:
    biblock diagnosis refs.bib

  This read-only JSON report makes no API requests. Exit 0 means the lockfile is
  consistent, every entry is verified, and no proposals are pending;
  exit 3 means work remains.

INSPECT AND PIPE
  Emit bibliography entries and integrity state as one JSON array:
    biblock inspect refs.bib

  Use an external JSON processor when selection or transformation is useful:
    biblock inspect refs.bib | jq -r '.[] | select(.integrity.status != "verified") | .id'

  Feed selected citation keys back to a controlled write command:
    biblock inspect refs.bib | jq -r '.[].id' |
      biblock integrity add refs.bib --keys-from - --source agent --agent MODEL --in-place

DEDUPLICATION
  Find likely duplicate pairs using normalized title and author similarity:
    biblock dedupe refs.bib

  Results include component scores and complete entry fields for agent review.
  biblock never merges or removes entries automatically.

AGENT PROPOSALS
  Start with diagnosis, then search and freeze an explicitly chosen API candidate:
    biblock source match refs.bib --key paper1 --min-score 0.9
    biblock source propose refs.bib --key paper1 --id DOI --agent codex
    biblock proposal list refs.bib

  Read the saved replacement, full diff, scores and receipts. Confirm the same
  work (including publication version), then adopt only if agentAdoptable is true:
    biblock proposal adopt refs.bib --id proposal:... --agent codex --reviewed

  Adoption uses frozen evidence without another API request and records API-backed
  verified status, not human approval. Missing provider support, low scores or
  multiple qualifying candidates require human review. Stale proposals must be
  recreated. Do not edit .bib directly: external edits fail the lockfile check;
  lock --sync records edits in history but does not verify them.

  Save a replacement only in FILE.lock, then let a human compare and decide:
    biblock proposal create refs.bib --key paper1 --bibtex replacement.bib \
      --agent codex --reason 'Corrected metadata'
    biblock proposal list refs.bib
    biblock review refs.bib

LITERATURE SOURCES
  Providers map their native metadata into one common literature record. Crossref is
  the default backend. Verify exact identifiers across a complete file, then inspect
  any ambiguous candidates in the JSON report:
    biblock source verify refs.bib --all
    biblock source verify refs.bib --all --in-place

  For explicit candidate review and key-by-key reconciliation:
    biblock source plan refs.bib --key paper1
    biblock source apply refs.bib --key paper1 --in-place

  Compare provider title-search results with a fixed title/author score before an
  agent explicitly adopts one candidate (use --provider openreview when needed):
    biblock source match refs.bib --key paper1 --min-score 0.9
    biblock source propose refs.bib --key paper1 --id DOI --agent AGENT
    biblock proposal list refs.bib
    biblock proposal adopt refs.bib --id proposal:... --agent AGENT --reviewed

  A URL is resolved to a stable DOI from the URL itself, redirects, publisher
  metadata, JSON-LD, or an official identifier API:
    biblock source resolve 'https://doi.org/10.1234/example'

  An entry with a DOI is looked up exactly. Without one, biblock tries its URL before
  returning ranked candidates. Review ambiguous results and pass an id explicitly:
    biblock source apply refs.bib --key paper1 --id 10.1234/example --in-place

  Apply writes request, response-hash, projection-hash, provider, and record-id
  evidence to FILE.lock. Response bodies and workflow fields are never embedded
  in BibTeX. To reconcile and seal an exact provider projection:
    biblock source apply refs.bib --key paper1 --add-integrity --in-place

  Use --provider doi for DOI content negotiation (raw application/x-bibtex) or
  --provider crossref for the Crossref works API (raw JSON).

SCOPE
  biblock deliberately does not provide arbitrary metadata editing or an embedded jq
  implementation. Use normal editors, domain tools, and shell pipelines for data
  processing. Source and integrity commands keep workflow state in FILE.lock;
  history restore only restores a recorded snapshot and records that edit again.

LOCKFILE
  FILE.lock is deterministic, JSON, and designed for jq and agents. The .bib is
  valid without it, but deleting it discards provenance, integrity, and history.
    biblock lock refs.bib --frozen
    biblock lock refs.bib --sync --actor alice

EDIT HISTORY
  Every --in-place edit writes the prior entry into FILE.lock. The current head
  is stored only in JSON; the BibTeX remains clean. Dry runs never write history.
    biblock history status refs.bib
    biblock history log refs.bib --key paper1
    biblock history restore refs.bib --revision 8f31c9d0 --in-place

HUMAN REVIEW
  Open a local review page. The reviewer label defaults to the computer name,
  but may be edited or left blank:
    biblock review refs.bib

INTEGRITY
  verified    Valid integrity backed by a provider API or explicit human approval.
  valid       Integrity and provenance are consistent, but the source is agent or web.
  stale       A marker exists, but the covered fields have changed.
  invalid     Integrity or provenance is missing, damaged, or inconsistent.

  Adding integrity always requires --source provider, --source agent --agent ID,
  or --source human --reviewer ID, plus --key, --keys-from, or --all.

EXIT STATUS
  0  Success (or every selected entry is verified for 'integrity status')
  2  Invalid input or operational error
  3  Review or selection is needed, or lock/integrity validation failed."#;

const INSPECT_AFTER_HELP: &str = r#"OUTPUT
  inspect writes one JSON array containing bibliography entries plus integrity and
  provenance summaries loaded from FILE.lock when present.

PIPELINE EXAMPLES
  biblock inspect refs.bib | jq -r '.[].id'
  biblock inspect refs.bib --compact |
    jq -r '.[] | select(.integrity.status != "verified") | .id'
  cat refs.bib | biblock inspect -

jq is optional and external. biblock itself does not evaluate filters or turn edited
JSON back into BibTeX."#;

const DEDUPE_AFTER_HELP: &str = r#"SCORING
  Candidate pairs are scored from normalized title and author similarity:
    score = 0.7 * title_score + 0.3 * author_score

  Author scoring emphasizes family-name overlap while tolerating differences in
  given-name formatting. TeX braces, commands, punctuation, case, and whitespace
  are normalized before comparison. Entries missing either title or author are
  not proposed as candidates.

OUTPUT
  Emit a JSON array of candidate pairs sorted by descending score. Every entry
  includes its input file, citation key, type, and fields so an agent can decide
  whether and how to merge it. biblock never merges or removes entries automatically.

EXIT STATUS
  0  No candidate pairs met the threshold
  2  Invalid input or operational error
  3  Candidate pairs were found and need review"#;

const SOURCE_AFTER_HELP: &str = r#"WORKFLOW
  Start with biblock diagnosis refs.bib. For search-based replacements, use the
  frozen proposal workflow instead of re-fetching a candidate when applying it:
    biblock source match refs.bib --key paper1 --min-score 0.9
    biblock source propose refs.bib --key paper1 --id DOI --agent codex
    biblock proposal list refs.bib
    biblock proposal adopt refs.bib --id proposal:... --agent codex --reviewed

  Read the fixed diff and receipts before adopting; require agentAdoptable=true
  and confirm semantic identity. Low scores, ambiguity or missing provider support
  require biblock review refs.bib. A matching score is not automatic approval.

  1. Batch exact DOI and URL verification with Crossref-to-DOI fallback. This
     writes nothing without --in-place and never selects search candidates:
       biblock source verify refs.bib --all
       biblock source verify refs.bib --all --in-place
  2. Plan replacements that still need explicit candidate review:
       biblock source plan refs.bib --key paper1
     Multiple keys may come from a newline-delimited pipeline:
       biblock inspect refs.bib | jq -r '.[].id' |
         biblock source plan refs.bib --keys-from -
  3. For a DOI-backed exact match, apply it directly. For search results, pass the
     chosen candidate id and selector identity. This records the search response,
     candidate set, selection, and exact provider lookup as one evidence chain:
       biblock source apply refs.bib --key paper1 --id 10.1234/example \
         --selected-by codex --baseline-hash BASELINE_HASH \
         --proposal-hash PROPOSAL_HASH --add-integrity --in-place
  4. Either add provider-backed integrity atomically with apply:
       biblock source apply refs.bib --key paper1 --add-integrity --in-place
     or record an attributed review separately:
       biblock integrity add refs.bib --key paper1 --source agent --agent MODEL --in-place

PROVENANCE
  Apply creates a compact receipt in FILE.lock with the request URL, media type,
  response SHA-256, and provider-projection SHA-256. It never embeds response
  bodies or workflow fields in the bibliography.

  Entries without a DOI are first resolved from their URL. Conflicting identifiers
  require review and are never selected by score. Crossref is the default; doi is
  exact-lookup-only. Set BIBLOCK_MAILTO or pass --mailto for polite API identification.
  Inspect the full evidence chain with:
    biblock source trace refs.bib --key paper1

  Entries whose authority is a product page, documentation page, or other URL can
  retain their existing fields while recording a response-bound web receipt:
    biblock source web refs.bib --key product --in-place"#;

const INTEGRITY_AFTER_HELP: &str = r#"SOURCE MODES
  provider  Reuse evidence in FILE.lock created by `biblock source apply`. Receipt key,
            response hash, provider identity, and projected fields are validated.
  agent     Record kind=agent in FILE.lock; requires --agent MODEL_OR_AGENT_ID.
  human     Record kind=human in FILE.lock; requires --reviewer REVIEWER_ID.

EXAMPLES
  biblock integrity add refs.bib --key paper1 --source provider --in-place
  biblock integrity add refs.bib --key draft1 --source agent --agent claude-code --in-place
  biblock integrity add refs.bib --key paper1 --source human --reviewer alice --in-place
  jq -r '.[].id' review.json | biblock integrity add refs.bib --keys-from - \
    --source human --reviewer alice --in-place

Agent and human modes are attributed assertions, not cryptographic identities.
Provider mode proves agreement with the stored projection receipt. Receipts are
tamper-evident workflow metadata, not proof that an API served particular bytes.

STATUS POLICY
  verified requires a valid provider source or explicit human approval. Valid
  agent and web evidence is reported as valid, not verified. Missing integrity or
  provenance is invalid. The status command exits 0 only when every selected
  entry is verified."#;

const RESOLVE_AFTER_HELP: &str = r#"SIGNALS
  DOI text in the input or final URL is exact. Publisher citation metadata and
  JSON-LD are strong signals. Supported repository APIs may provide exact signals.
  A unique candidate exits 0; no candidate or conflicting candidates exit 3.

  Network receipts include URLs, media type, byte count, and response SHA-256.
  Response bodies are used only during resolution and are never stored in BibTeX.
  Non-public hosts, non-default ports, and unsafe redirects are rejected."#;

const TRACE_AFTER_HELP: &str = r#"OUTPUT
  Emit one JSON object containing the provider receipt and, when present, the URL
  resolution receipt linked to it. The command validates the chain first and never
  performs a network request or exposes legacy embedded response bodies."#;

const VERIFY_AFTER_HELP: &str = r#"WORKFLOW
  Verify explicitly selected entries in one transaction. Existing DOIs and exact
  DOI URLs are looked up with each provider in order. arXiv URLs fall back to
  their DataCite DOI through the doi provider. Entries without exact identifiers
  return ranked candidates but are never selected automatically.

  Dry-run and inspect the JSON report:
    biblock source verify refs.bib --all

  Bind a write to the exact entries and replacements you reviewed:
    biblock source verify refs.bib --all > preview.json
    biblock source verify refs.bib --all --in-place --reviewed preview.json

  Reconcile exact records, add provider integrity, and write once:
    biblock source verify refs.bib --all --in-place

  The default provider order is crossref,doi. Override it with a comma-separated
  list such as --providers doi,crossref.

EXIT STATUS
  0  Every selected entry is provider-verified or ready to write
  2  Invalid input or operational error
  3  At least one entry needs selection, is unsupported, or failed lookup"#;

#[derive(Parser)]
#[command(
    name = "biblock",
    version,
    about = "Reconcile, inspect, and verify BibTeX",
    long_about = LONG_ABOUT,
    after_help = AFTER_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Save agent-proposed replacements in the lockfile for human review.
    Proposal(ProposalArgs),
    /// Diagnose local verification state and emit actionable commands as JSON.
    Diagnosis(DiagnosisArgs),
    /// Review and approve entries in a local browser.
    Review(ReviewArgs),
    /// Emit bibliography entries and their trust state as JSON.
    Inspect(InspectArgs),
    /// Find likely duplicate entries by title and author similarity.
    Dedupe(DedupeArgs),
    /// Find and apply records from a literature metadata provider.
    Source(SourceArgs),
    /// Inspect, add, or remove integrity markers.
    Integrity(IntegrityArgs),
    /// Inspect or restore the append-only edit history.
    History(HistoryArgs),
    /// Check or synchronize the JSON sidecar lockfile.
    Lock(LockArgs),
}

const PROPOSAL_AFTER_HELP: &str = r#"AGENT WORKFLOW
  biblock diagnosis refs.bib
  biblock source match refs.bib --key paper1 --min-score 0.9
  biblock source propose refs.bib --key paper1 --id DOI --agent codex
  biblock proposal list refs.bib
  biblock proposal adopt refs.bib --id proposal:... --agent codex --reviewed

Read the saved before/after records, comparison, matchScore, provider receipts and
agentAdoptionBlocker. Confirm the same work and publication version. Adopt only
when agentAdoptable is true; --reviewed records your explicit confirmation.
Adoption never re-fetches metadata and writes provider-backed verified status,
the adopting agent and reversible history, without adding human approval.

Missing provider support, low scores or multiple qualifying candidates require
human review: biblock review refs.bib. Freeform proposal create --evidence JSON
does not grant API authority. Stale baselines require a new proposal; decided
proposals cannot be adopted again. Check biblock diagnosis refs.bib afterward.
Do not hard-edit .bib or .lock: external changes fail the lockfile consistency
check. lock --sync records external edits but does not make them verified."#;

#[derive(Args)]
#[command(after_help = PROPOSAL_AFTER_HELP)]
struct ProposalArgs {
    #[command(subcommand)]
    command: ProposalCommand,
}

#[derive(Subcommand)]
enum ProposalCommand {
    /// Save one complete replacement without changing or approving the bibliography.
    Create {
        file: PathBuf,
        #[arg(long)]
        key: String,
        /// File containing exactly one replacement entry. Use - for stdin.
        #[arg(long)]
        bibtex: PathBuf,
        #[arg(long)]
        agent: String,
        #[arg(long)]
        reason: String,
        /// Optional JSON evidence; retained as supporting information, not API authority.
        #[arg(long)]
        evidence: Option<PathBuf>,
    },
    /// Show immutable proposals, complete differences, decisions and stale baselines.
    List { file: PathBuf },
    /// Adopt a frozen, unambiguous provider-backed proposal after reading its diff.
    #[command(after_help = PROPOSAL_AFTER_HELP)]
    Adopt {
        file: PathBuf,
        #[arg(long)]
        id: String,
        #[arg(long)]
        agent: String,
        /// Confirm that you have read the saved replacement and comparison.
        #[arg(long)]
        reviewed: bool,
    },
}

#[derive(Args)]
#[command(
    after_help = "Read-only local check: no API requests or file changes. JSON includes lock status, trust counts, pending entries, and commands with argv and writes flags. Exit 0 means the lockfile is valid and every entry is verified; exit 3 means work remains; exit 2 means invalid BibTeX or an operational error. Synchronizing a lockfile does not verify entries."
)]
struct DiagnosisArgs {
    /// BibTeX file to diagnose. Omit to check all .bib files in the current directory.
    file: Option<PathBuf>,
    /// Emit JSON without indentation.
    #[arg(long)]
    compact: bool,
}

#[derive(Args)]
struct ReviewArgs {
    file: PathBuf,
    /// Mark Crossref title matches at or above this score as agent-adoptable.
    #[arg(long, default_value_t = 0.9, value_parser = parse_score)]
    threshold: f64,
    /// Do not open the default browser automatically.
    #[arg(long)]
    no_open: bool,
    /// Local port. Uses an available random port by default.
    #[arg(long, default_value_t = 0)]
    port: u16,
}

#[derive(Args, Clone, Default)]
struct HistoryWriteArgs {
    /// Override the default FILE.lock path.
    #[arg(long = "lockfile", value_name = "FILE")]
    history: Option<PathBuf>,
    /// Actor recorded for the edit. Defaults to biblock/VERSION.
    #[arg(long = "lock-actor", env = "BIBLOCK_ACTOR")]
    history_actor: Option<String>,
}

#[derive(Args)]
struct LockArgs {
    file: PathBuf,
    /// Update or create the lockfile from the current bibliography.
    #[arg(long, conflicts_with = "frozen")]
    sync: bool,
    /// Fail if the lockfile is missing or out of sync.
    #[arg(long, conflicts_with = "sync")]
    frozen: bool,
    /// Preview lock status without writing.
    #[arg(long, requires = "sync")]
    dry_run: bool,
    /// Override the default FILE.lock path.
    #[arg(long, value_name = "FILE")]
    lockfile: Option<PathBuf>,
    /// Actor recorded for an external synchronization.
    #[arg(long, env = "BIBLOCK_ACTOR")]
    actor: Option<String>,
}

#[derive(Args)]
#[command(after_help = DEDUPE_AFTER_HELP)]
struct DedupeArgs {
    /// BibTeX files. Reads stdin when omitted or when FILE is `-`.
    files: Vec<PathBuf>,
    /// Minimum combined similarity score from 0 to 1.
    #[arg(long, default_value_t = 0.75, value_parser = parse_score)]
    min_score: f64,
    /// Emit the JSON array on one line.
    #[arg(short, long)]
    compact: bool,
}

#[derive(Args)]
#[command(after_help = INSPECT_AFTER_HELP)]
struct InspectArgs {
    /// BibTeX files. Reads stdin when omitted or when FILE is `-`.
    files: Vec<PathBuf>,
    /// Explicitly request JSON output (already the default).
    #[arg(long)]
    json: bool,
    /// Emit the JSON array on one line.
    #[arg(short, long)]
    compact: bool,
}

#[derive(Args)]
#[command(after_help = SOURCE_AFTER_HELP)]
struct SourceArgs {
    #[command(subcommand)]
    command: SourceCommand,
}

#[derive(Subcommand)]
enum SourceCommand {
    /// Freeze one provider replacement and its search receipts in the lockfile, without editing BibTeX.
    #[command(after_help = PROPOSAL_AFTER_HELP)]
    Propose {
        file: PathBuf,
        #[arg(long)]
        key: String,
        #[arg(long)]
        id: String,
        #[arg(long, default_value = "crossref")]
        provider: String,
        #[arg(long)]
        agent: String,
        #[arg(
            long,
            default_value = "Selected provider record for metadata reconciliation"
        )]
        reason: String,
        #[arg(long, default_value_t = 0.9, value_parser = parse_score)]
        min_score: f64,
        #[arg(long, default_value_t = 5, value_parser = parse_limit)]
        search_limit: usize,
        #[arg(long, env = "BIBLOCK_MAILTO")]
        mailto: Option<String>,
    },
    /// List installed literature metadata providers.
    Providers,
    /// Batch provider verification with Crossref-to-DOI fallback.
    #[command(after_help = VERIFY_AFTER_HELP)]
    Verify {
        file: PathBuf,
        /// Citation key to verify. Repeat for multiple entries.
        #[arg(short, long)]
        key: Vec<String>,
        /// Read citation keys, one per line. Use `-` for stdin.
        #[arg(long, value_name = "FILE")]
        keys_from: Option<PathBuf>,
        /// Verify every bibliography entry.
        #[arg(long, conflicts_with_all = ["key", "keys_from"])]
        all: bool,
        /// Provider fallback order.
        #[arg(long, value_delimiter = ',', default_value = "crossref,doi")]
        providers: Vec<String>,
        /// Maximum candidates for entries without an exact identifier.
        #[arg(long, default_value_t = 5, value_parser = parse_limit)]
        limit: usize,
        /// Atomically write all successful exact matches in one replacement.
        #[arg(short, long)]
        in_place: bool,
        /// JSON report from a previous dry run. Reject changes to its reviewed entries or replacements.
        #[arg(long, requires = "in_place")]
        reviewed: Option<PathBuf>,
        /// Emit compact JSON.
        #[arg(short, long)]
        compact: bool,
        /// Email sent to providers that support polite API identification.
        #[arg(long, env = "BIBLOCK_MAILTO")]
        mailto: Option<String>,
        #[command(flatten)]
        history_write: HistoryWriteArgs,
    },
    /// Verify existing entry contents against fetched web-source receipts.
    Web {
        file: PathBuf,
        /// Citation key to verify. Repeat for multiple entries.
        #[arg(short, long)]
        key: Vec<String>,
        /// Read citation keys, one per line. Use `-` for stdin.
        #[arg(long, value_name = "FILE")]
        keys_from: Option<PathBuf>,
        /// Verify every bibliography entry that has a URL.
        #[arg(long, conflicts_with_all = ["key", "keys_from"])]
        all: bool,
        /// Atomically write all successful receipts in one replacement.
        #[arg(short, long)]
        in_place: bool,
        /// Emit compact JSON.
        #[arg(short, long)]
        compact: bool,
        #[command(flatten)]
        history_write: HistoryWriteArgs,
    },
    /// Resolve a literature URL into evidence-backed identifier candidates.
    #[command(after_help = RESOLVE_AFTER_HELP)]
    Resolve {
        /// HTTP(S) literature page, DOI URL, or supported repository URL.
        url: String,
        /// Emit compact JSON.
        #[arg(short, long)]
        compact: bool,
    },
    /// Search a provider and return ranked records as JSON.
    Search {
        query: String,
        /// Metadata provider name.
        #[arg(long, default_value = DEFAULT_PROVIDER)]
        provider: String,
        /// Maximum candidate count.
        #[arg(long, default_value_t = 5, value_parser = parse_limit)]
        limit: usize,
        /// Emit compact JSON.
        #[arg(short, long)]
        compact: bool,
        /// Email sent to providers that support polite API identification.
        #[arg(long, env = "BIBLOCK_MAILTO")]
        mailto: Option<String>,
    },
    /// Compare entries with provider title-search candidates for agent review.
    Match {
        file: PathBuf,
        #[arg(long, default_value = "crossref")]
        provider: String,
        #[arg(short, long)]
        key: Vec<String>,
        #[arg(long, value_name = "FILE")]
        keys_from: Option<PathBuf>,
        #[arg(long, conflicts_with_all = ["key", "keys_from"])]
        all: bool,
        #[arg(long, default_value_t = 5, value_parser = parse_limit)]
        limit: usize,
        #[arg(long, default_value_t = 0.9, value_parser = parse_score)]
        min_score: f64,
        #[arg(short, long)]
        compact: bool,
        #[arg(long, env = "BIBLOCK_MAILTO")]
        mailto: Option<String>,
    },
    /// Plan provider replacements for explicitly selected BibTeX entries.
    Plan {
        file: PathBuf,
        /// Citation key to inspect. Repeat for multiple entries.
        #[arg(short, long)]
        key: Vec<String>,
        /// Read citation keys, one per line. Use `-` for stdin.
        #[arg(long, value_name = "FILE")]
        keys_from: Option<PathBuf>,
        /// Inspect every entry.
        #[arg(long, conflicts_with_all = ["key", "keys_from"])]
        all: bool,
        /// Metadata provider name. Defaults to stored provenance, then Crossref.
        #[arg(long)]
        provider: Option<String>,
        /// Maximum candidates for entries without a provider identifier.
        #[arg(long, default_value_t = 5, value_parser = parse_limit)]
        limit: usize,
        /// Emit compact JSON.
        #[arg(short, long)]
        compact: bool,
        /// Email sent to providers that support polite API identification.
        #[arg(long, env = "BIBLOCK_MAILTO")]
        mailto: Option<String>,
    },
    /// Apply one exact provider record to one BibTeX entry.
    Apply {
        file: PathBuf,
        /// Citation key to update.
        #[arg(short, long)]
        key: String,
        /// Exact provider id. If omitted, uses stored provenance, DOI, or a resolved URL.
        #[arg(long)]
        id: Option<String>,
        /// Record an auditable search-and-selection chain for an explicit id.
        #[arg(long, requires_all = ["id", "baseline_hash", "proposal_hash"], value_name = "ACTOR")]
        selected_by: Option<String>,
        /// Current entry hash from the candidate report you reviewed.
        #[arg(long, requires = "proposal_hash")]
        baseline_hash: Option<String>,
        /// Replacement hash from the candidate report. Rejects changed API metadata.
        #[arg(long, requires = "baseline_hash")]
        proposal_hash: Option<String>,
        /// Require the selected result to meet the title/author similarity threshold.
        #[arg(long, requires = "selected_by", value_parser = parse_score)]
        min_score: Option<f64>,
        /// Candidate count retained when recording a selection.
        #[arg(long, default_value_t = 5, requires = "selected_by", value_parser = parse_limit)]
        search_limit: usize,
        /// Metadata provider name. Defaults to stored provenance, then Crossref.
        #[arg(long)]
        provider: Option<String>,
        /// Atomically update FILE instead of writing the result to stdout.
        #[arg(short, long)]
        in_place: bool,
        /// Add provider-backed integrity after recording the provider receipt.
        #[arg(long)]
        add_integrity: bool,
        /// Email sent to providers that support polite API identification.
        #[arg(long, env = "BIBLOCK_MAILTO")]
        mailto: Option<String>,
        #[command(flatten)]
        history_write: HistoryWriteArgs,
    },
    /// Show the provider and URL-resolution evidence chain for one entry.
    #[command(after_help = TRACE_AFTER_HELP)]
    Trace {
        file: PathBuf,
        /// Citation key whose evidence chain should be shown.
        #[arg(short, long)]
        key: String,
        /// Emit compact JSON.
        #[arg(short, long)]
        compact: bool,
    },
    /// Remove response bodies embedded by biblock versions before 0.5.
    StripResponses {
        file: PathBuf,
        /// Atomically update FILE instead of writing the result to stdout.
        #[arg(short, long)]
        in_place: bool,
        #[command(flatten)]
        history_write: HistoryWriteArgs,
    },
}

#[derive(Args)]
#[command(after_help = INTEGRITY_AFTER_HELP)]
struct IntegrityArgs {
    #[command(subcommand)]
    command: IntegrityCommand,
}

#[derive(Subcommand)]
enum IntegrityCommand {
    /// Show whether entry contents match their stored integrity markers.
    Status {
        file: PathBuf,
        /// Return a JSON array suitable for agents and scripts.
        #[arg(long)]
        json: bool,
        /// Limit the report to these citation keys.
        #[arg(short, long)]
        key: Vec<String>,
        /// Read citation keys, one per line. Use `-` for stdin.
        #[arg(long, value_name = "FILE")]
        keys_from: Option<PathBuf>,
    },
    /// Print the expected integrity hash for one entry.
    Hash { file: PathBuf, key: String },
    /// Add or refresh integrity for explicitly reviewed entries.
    Add {
        file: PathBuf,
        /// Citation key to approve. Repeat for multiple entries.
        #[arg(short, long)]
        key: Vec<String>,
        /// Read citation keys, one per line. Use `-` for stdin.
        #[arg(long, value_name = "FILE")]
        keys_from: Option<PathBuf>,
        /// Approve every entry. This must be explicit.
        #[arg(long, conflicts_with_all = ["key", "keys_from"])]
        all: bool,
        /// Atomically update FILE instead of writing the result to stdout.
        #[arg(short, long)]
        in_place: bool,
        /// Required provenance kind.
        #[arg(long, value_enum)]
        source: IntegritySourceArg,
        /// Agent/model identifier, required with `--source agent`.
        #[arg(long)]
        agent: Option<String>,
        /// Human reviewer identifier, required with `--source human`.
        #[arg(long)]
        reviewer: Option<String>,
        #[command(flatten)]
        history_write: HistoryWriteArgs,
    },
    /// Remove integrity from selected entries.
    Remove {
        file: PathBuf,
        /// Citation key to unapprove. Repeat for multiple entries.
        #[arg(short, long)]
        key: Vec<String>,
        /// Read citation keys, one per line. Use `-` for stdin.
        #[arg(long, value_name = "FILE")]
        keys_from: Option<PathBuf>,
        /// Remove integrity from every entry.
        #[arg(long, conflicts_with_all = ["key", "keys_from"])]
        all: bool,
        /// Atomically update FILE instead of writing the result to stdout.
        #[arg(short, long)]
        in_place: bool,
        #[command(flatten)]
        history_write: HistoryWriteArgs,
    },
}

#[derive(Args)]
struct HistoryArgs {
    #[command(subcommand)]
    command: HistoryCommand,
}

#[derive(Subcommand)]
enum HistoryCommand {
    /// Validate revision hashes, snapshots, and current history links.
    Status {
        file: PathBuf,
        #[arg(long = "lockfile", value_name = "FILE")]
        history: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Show the revision chain for one entry as JSON.
    Log {
        file: PathBuf,
        #[arg(short, long)]
        key: String,
        #[arg(long = "lockfile", value_name = "FILE")]
        history: Option<PathBuf>,
        #[arg(short, long)]
        compact: bool,
    },
    /// Print the complete BibTeX snapshot stored by a revision.
    Show {
        file: PathBuf,
        #[arg(long)]
        revision: String,
        #[arg(long = "lockfile", value_name = "FILE")]
        history: Option<PathBuf>,
    },
    /// Compare a stored revision with the current entry as JSON.
    Diff {
        file: PathBuf,
        #[arg(long)]
        revision: String,
        #[arg(long = "lockfile", value_name = "FILE")]
        history: Option<PathBuf>,
        #[arg(short, long)]
        compact: bool,
    },
    /// Restore a stored snapshot; the restore is itself recorded.
    Restore {
        file: PathBuf,
        #[arg(long)]
        revision: String,
        #[arg(short, long)]
        in_place: bool,
        #[command(flatten)]
        history_write: HistoryWriteArgs,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum IntegritySourceArg {
    Provider,
    Agent,
    Human,
}

#[derive(Serialize)]
struct StatusRow<'a> {
    id: &'a str,
    status: Status,
    expected: String,
    stored: Option<&'a str>,
    source: provenance::SourceSummary,
}

#[derive(Serialize)]
struct PlannedCandidate {
    provider_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    match_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    author_score: Option<f64>,
    record: LiteratureRecord,
    changes: Vec<FieldChange>,
    baseline_hash: String,
    proposal_hash: String,
    proposed: Record,
}

#[derive(Serialize)]
struct MatchCandidate {
    adoptable: bool,
    #[serde(flatten)]
    candidate: PlannedCandidate,
}

#[derive(Serialize)]
struct MatchRow {
    id: String,
    query: String,
    min_score: f64,
    unique_adoptable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    candidates: Vec<MatchCandidate>,
}

#[derive(Serialize)]
struct PlanRow {
    id: String,
    provider: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution: Option<ResolutionReport>,
    candidates: Vec<PlannedCandidate>,
}

#[derive(Serialize)]
struct VerifyRow {
    id: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution: Option<ResolutionReport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    candidates: Vec<PlannedCandidate>,
}

#[derive(Serialize)]
struct WebRow {
    id: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence: Option<resolver::WebEvidence>,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("biblock: {error:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<u8> {
    match cli.command {
        Command::Proposal(args) => {
            match args.command {
                ProposalCommand::Create {
                    file,
                    key,
                    bibtex,
                    agent,
                    reason,
                    evidence,
                } => {
                    let mut input = String::new();
                    if bibtex == Path::new("-") {
                        io::stdin().read_to_string(&mut input)?;
                    } else {
                        input = fs::read_to_string(bibtex)?;
                    }
                    let evidence = evidence
                        .map(|path| -> Result<serde_json::Value> {
                            Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
                        })
                        .transpose()?
                        .unwrap_or(serde_json::Value::Null);
                    print_serializable(
                        &proposal::create(&file, &key, &input, &agent, &reason, evidence)?,
                        false,
                    )?;
                }
                ProposalCommand::List { file } => {
                    print_serializable(&proposal::list(&file)?, false)?
                }
                ProposalCommand::Adopt {
                    file,
                    id,
                    agent,
                    reviewed,
                } => {
                    proposal::adopt(&file, &id, &agent, reviewed)?;
                    print_serializable(&proposal::list(&file)?, false)?;
                }
            }
            Ok(0)
        }
        Command::Diagnosis(args) => {
            if let Some(file) = args.file {
                let report = diagnosis::report(&file)?;
                print_serializable(&report, args.compact)?;
                Ok(if report.ready { 0 } else { 3 })
            } else {
                let report = diagnosis::directory_report(Path::new("."))?;
                print_serializable(&report, args.compact)?;
                Ok(if !report.errors.is_empty() {
                    2
                } else if report.ready {
                    0
                } else {
                    3
                })
            }
        }
        Command::Review(args) => review::run(&args.file, args.port, !args.no_open, args.threshold),
        Command::Inspect(args) => {
            let InspectArgs {
                files,
                json: _,
                compact,
            } = args;
            let records = read_bib_inputs(&files)?;
            print_serializable(&inspect::document(&records)?, compact)?;
            Ok(0)
        }
        Command::Dedupe(args) => {
            let records = read_bib_inputs_with_origins(&args.files)?;
            let candidates = dedupe::candidates(&records, args.min_score);
            let needs_review = !candidates.is_empty();
            print_serializable(&candidates, args.compact)?;
            Ok(if needs_review { 3 } else { 0 })
        }
        Command::Source(args) => run_source(args.command),
        Command::Integrity(args) => run_integrity(args.command),
        Command::History(args) => run_history(args.command),
        Command::Lock(args) => run_lock(args),
    }
}

fn run_lock(args: LockArgs) -> Result<u8> {
    let report = if args.sync && !args.frozen {
        history::sync(
            &args.file,
            args.lockfile.as_deref(),
            false,
            args.dry_run,
            args.actor.as_deref(),
        )?
    } else {
        history::status(&args.file, args.lockfile.as_deref())?
    };
    print_serializable(&report, false)?;
    Ok(if report.valid { 0 } else { 3 })
}

fn run_source(command: SourceCommand) -> Result<u8> {
    match command {
        SourceCommand::Propose {
            file,
            key,
            id,
            provider,
            agent,
            reason,
            min_score,
            search_limit,
            mailto,
        } => {
            print_serializable(
                &proposal::create_provider(
                    &file,
                    &key,
                    &provider,
                    &id,
                    &agent,
                    &reason,
                    min_score,
                    search_limit,
                    mailto.as_deref(),
                )?,
                false,
            )?;
            Ok(0)
        }
        SourceCommand::Providers => {
            for name in providers::names() {
                println!(
                    "{name}{}",
                    if *name == DEFAULT_PROVIDER {
                        "\tdefault"
                    } else {
                        ""
                    }
                );
            }
            Ok(0)
        }
        SourceCommand::Verify {
            file,
            key,
            keys_from,
            all,
            providers,
            limit,
            in_place,
            reviewed,
            compact,
            mailto,
            history_write,
        } => run_verify(
            &file,
            merge_keys(key, keys_from)?,
            all,
            &providers,
            limit,
            in_place,
            reviewed.as_deref(),
            compact,
            mailto.as_deref(),
            &history_write,
        ),
        SourceCommand::Web {
            file,
            key,
            keys_from,
            all,
            in_place,
            compact,
            history_write,
        } => run_web(
            &file,
            merge_keys(key, keys_from)?,
            all,
            in_place,
            compact,
            &history_write,
        ),
        SourceCommand::Resolve { url, compact } => {
            let report = resolver::resolve_url(&url)?;
            let resolved = report.candidates.len() == 1;
            print_serializable(&report, compact)?;
            Ok(if resolved { 0 } else { 3 })
        }
        SourceCommand::Search {
            query,
            provider,
            limit,
            compact,
            mailto,
        } => {
            let backend = providers::open(&provider, mailto.as_deref())?;
            let search = backend.search(
                &BibliographicQuery {
                    citation: query,
                    title_only: false,
                },
                limit,
            )?;
            print_serializable(&search.candidates, compact)?;
            Ok(if search.candidates.is_empty() { 3 } else { 0 })
        }
        SourceCommand::Match {
            file,
            provider,
            key,
            keys_from,
            all,
            limit,
            min_score,
            compact,
            mailto,
        } => {
            let source = read_file(&file)?;
            let records = parse(&source)?;
            let selected = select_records(&records, merge_keys(key, keys_from)?, all)?;
            let backend = providers::open(&provider, mailto.as_deref())?;
            let mut rows = Vec::new();
            for record in selected {
                let query = title_query(record);
                if query.citation.trim().is_empty() {
                    rows.push(MatchRow {
                        id: record.entry_key.clone(),
                        query: String::new(),
                        min_score,
                        unique_adoptable: false,
                        error: Some("entry has no title".to_owned()),
                        candidates: vec![],
                    });
                    continue;
                }
                match backend.search(&query, limit) {
                    Ok(search) => {
                        let candidates: Vec<_> = search
                            .candidates
                            .into_iter()
                            .map(|candidate| -> Result<_> {
                                let fetched = backend.lookup(&LiteratureIdentifier::ProviderId(
                                    candidate.record.id.clone(),
                                ))?;
                                if !fetched.record.id.eq_ignore_ascii_case(&candidate.record.id) {
                                    bail!("provider returned a different record id");
                                }
                                let candidate = planned_candidate(
                                    record,
                                    Candidate {
                                        score: candidate.score,
                                        record: fetched.record,
                                    },
                                );
                                Ok(MatchCandidate {
                                    adoptable: candidate
                                        .match_score
                                        .is_some_and(|score| score >= min_score),
                                    candidate,
                                })
                            })
                            .collect::<Result<_>>()?;
                        let unique_adoptable =
                            candidates.iter().filter(|item| item.adoptable).count() == 1;
                        rows.push(MatchRow {
                            id: record.entry_key.clone(),
                            query: query.citation,
                            min_score,
                            unique_adoptable,
                            error: None,
                            candidates,
                        });
                    }
                    Err(error) => rows.push(MatchRow {
                        id: record.entry_key.clone(),
                        query: query.citation,
                        min_score,
                        unique_adoptable: false,
                        error: Some(format!("{error:#}")),
                        candidates: vec![],
                    }),
                }
            }
            print_serializable(&rows, compact)?;
            Ok(0)
        }
        SourceCommand::Plan {
            file,
            key,
            keys_from,
            all,
            provider,
            limit,
            compact,
            mailto,
        } => {
            let source = read_file(&file)?;
            let records = parse(&source)?;
            let selected = select_records(&records, merge_keys(key, keys_from)?, all)?;
            let mut rows = Vec::new();
            let mut needs_review = false;
            let mut backends = BTreeMap::new();
            for record in selected {
                let (provider_name, stored_id) = source_identity(record, provider.as_deref());
                if !backends.contains_key(&provider_name) {
                    match providers::open(&provider_name, mailto.as_deref()) {
                        Ok(backend) => {
                            backends.insert(provider_name.clone(), backend);
                        }
                        Err(error) => {
                            needs_review = true;
                            rows.push(PlanRow {
                                id: record.entry_key.clone(),
                                provider: provider_name.clone(),
                                status: "error",
                                query: None,
                                error: Some(format!("{error:#}")),
                                resolution: None,
                                candidates: vec![],
                            });
                            continue;
                        }
                    }
                }
                let Some(backend) = backends.get(&provider_name) else {
                    unreachable!("provider was inserted above")
                };
                let mut resolution = None;
                let mut identifier =
                    stored_id.map(LiteratureIdentifier::ProviderId).or_else(|| {
                        record
                            .fields
                            .get("doi")
                            .cloned()
                            .map(LiteratureIdentifier::Doi)
                    });
                if identifier.is_none()
                    && let Some(url) = record.fields.get("url")
                {
                    match resolver::resolve_url(url) {
                        Ok(report) => match report.exact_candidate() {
                            Ok(candidate) if candidate.kind == "doi" => {
                                identifier =
                                    Some(LiteratureIdentifier::Doi(candidate.value.clone()));
                                resolution = Some(report);
                            }
                            Ok(candidate) => {
                                needs_review = true;
                                rows.push(PlanRow {
                                    id: record.entry_key.clone(),
                                    provider: provider_name.clone(),
                                    status: "unsupported-identifier",
                                    query: None,
                                    error: Some(format!(
                                        "resolved {} identifier {}, but {provider_name} cannot look it up",
                                        candidate.kind, candidate.value
                                    )),
                                    resolution: Some(report),
                                    candidates: vec![],
                                });
                                continue;
                            }
                            Err(error) => {
                                needs_review = true;
                                rows.push(PlanRow {
                                    id: record.entry_key.clone(),
                                    provider: provider_name.clone(),
                                    status: "needs-resolution-review",
                                    query: None,
                                    error: Some(format!("{error:#}")),
                                    resolution: Some(report),
                                    candidates: vec![],
                                });
                                continue;
                            }
                        },
                        Err(error) => {
                            needs_review = true;
                            rows.push(PlanRow {
                                id: record.entry_key.clone(),
                                provider: provider_name.clone(),
                                status: "resolution-error",
                                query: None,
                                error: Some(format!("{error:#}")),
                                resolution: None,
                                candidates: vec![],
                            });
                            continue;
                        }
                    }
                }
                if let Some(identifier) = identifier {
                    match backend.lookup(&identifier) {
                        Ok(fetched) => rows.push(PlanRow {
                            id: record.entry_key.clone(),
                            provider: provider_name.clone(),
                            status: if resolution.is_some() {
                                "resolved-exact"
                            } else {
                                "exact"
                            },
                            query: None,
                            error: None,
                            resolution,
                            candidates: vec![planned_candidate(
                                record,
                                Candidate {
                                    score: None,
                                    record: fetched.record,
                                },
                            )],
                        }),
                        Err(error) => {
                            needs_review = true;
                            rows.push(PlanRow {
                                id: record.entry_key.clone(),
                                provider: provider_name.clone(),
                                status: "error",
                                query: None,
                                error: Some(format!("{error:#}")),
                                resolution,
                                candidates: vec![],
                            });
                        }
                    }
                } else {
                    needs_review = true;
                    let query = title_query(record);
                    match backend.search(&query, limit) {
                        Ok(search) => rows.push(PlanRow {
                            id: record.entry_key.clone(),
                            provider: provider_name.clone(),
                            status: "needs-selection",
                            query: Some(query.citation),
                            error: None,
                            resolution: None,
                            candidates: search
                                .candidates
                                .into_iter()
                                .map(|candidate| planned_candidate(record, candidate))
                                .collect(),
                        }),
                        Err(error) => rows.push(PlanRow {
                            id: record.entry_key.clone(),
                            provider: provider_name.clone(),
                            status: "error",
                            query: Some(query.citation),
                            error: Some(format!("{error:#}")),
                            resolution: None,
                            candidates: vec![],
                        }),
                    }
                }
            }
            print_serializable(&rows, compact)?;
            Ok(if needs_review { 3 } else { 0 })
        }
        SourceCommand::Apply {
            file,
            key,
            id,
            selected_by,
            baseline_hash,
            proposal_hash,
            min_score,
            search_limit,
            provider,
            in_place,
            add_integrity,
            mailto,
            history_write,
        } => {
            let source = read_file(&file)?;
            let records = parse(&source)?;
            let record = records
                .iter()
                .find(|record| record.entry_key == key)
                .with_context(|| format!("citation key not found: {key}"))?;
            if baseline_hash
                .as_ref()
                .is_some_and(|expected| hash(record).ok().as_ref() != Some(expected))
            {
                bail!("entry changed since candidate review; review it again");
            }
            let (provider_name, stored_id) = source_identity(record, provider.as_deref());
            let mut resolution_candidate = None;
            let explicit_id = id.clone();
            let identifier = id
                .map(LiteratureIdentifier::ProviderId)
                .or_else(|| stored_id.map(LiteratureIdentifier::ProviderId))
                .or_else(|| {
                    record
                        .fields
                        .get("doi")
                        .cloned()
                        .map(LiteratureIdentifier::Doi)
                })
                .or_else(|| {
                    let url = record.fields.get("url")?;
                    match resolver::resolve_url(url) {
                        Ok(report) => match report.exact_candidate() {
                            Ok(candidate) if candidate.kind == "doi" => {
                                resolution_candidate = Some(candidate.clone());
                                Some(LiteratureIdentifier::Doi(candidate.value.clone()))
                            }
                            Ok(candidate) => {
                                eprintln!(
                                    "URL resolved to unsupported {} identifier {}; pass --id for an exact provider record",
                                    candidate.kind, candidate.value
                                );
                                None
                            }
                            Err(error) => {
                                eprintln!("URL resolution needs review: {error:#}");
                                None
                            }
                        },
                        Err(error) => {
                            eprintln!("could not resolve entry URL: {error:#}");
                            None
                        }
                    }
                })
                .or_else(|| {
                    let (identifier, candidate) = identifier_from_bibtex_fields(record)?;
                    resolution_candidate = Some(candidate);
                    Some(identifier)
                })
                .context(
                    "no exact provider id or uniquely resolved DOI; use source plan/resolve, then pass --id",
                )?;
            let backend = providers::open(&provider_name, mailto.as_deref())?;
            let selection_evidence = if let Some(actor) = selected_by.as_deref() {
                let selected_id = explicit_id
                    .as_deref()
                    .context("--selected-by requires an explicit --id")?;
                let query = title_query(record);
                let search = backend.search(&query, search_limit)?;
                let search_source =
                    provenance::search_source(record, &query, &search, &provider_name)?;
                let selected_candidate = search
                    .candidates
                    .iter()
                    .find(|candidate| candidate.record.id.eq_ignore_ascii_case(selected_id))
                    .with_context(|| {
                        format!(
                            "selected provider id {selected_id} was not returned by title search"
                        )
                    })?;
                let similarity = dedupe::literature_similarity(record, &selected_candidate.record)
                    .context("title/author scoring requires both fields on both records")?;
                if let Some(threshold) = min_score
                    && similarity.score < threshold
                {
                    bail!(
                        "selected match score {:.4} is below --min-score {:.4}",
                        similarity.score,
                        threshold
                    );
                }
                let selection_source = provenance::selection_source_with_match(
                    &search_source,
                    selected_id,
                    actor,
                    "agent-review",
                    min_score.map(|threshold| provenance::MatchEvidence {
                        score: similarity.score,
                        title_score: similarity.title_score,
                        author_score: similarity.author_score,
                        threshold,
                    }),
                )?;
                Some(SelectionEvidence {
                    search: search_source,
                    selection: selection_source,
                })
            } else {
                None
            };
            let fetched = backend.lookup(&identifier)?;
            if let Some(threshold) = min_score {
                let similarity = dedupe::literature_similarity(record, &fetched.record)
                    .context("exact provider record cannot be scored by title and author")?;
                if similarity.score < threshold {
                    bail!("exact provider record is below --min-score; review it again");
                }
            }
            let provider_id = fetched.record.id.clone();
            let replacement = biblock_cli::catalog::proposed_record(record, &fetched.record);
            if proposal_hash
                .as_ref()
                .is_some_and(|expected| hash(&replacement).ok().as_ref() != Some(expected))
            {
                bail!(
                    "provider metadata changed since candidate review; review the new diff before applying"
                );
            }
            let (output, receipt_key) = reconcile_fetched(
                &source,
                &key,
                &fetched,
                resolution_candidate.as_ref(),
                selection_evidence.as_ref(),
                add_integrity,
            )?;
            if in_place {
                commit_edit(&file, &source, &output, "source-apply", &history_write)?;
                eprintln!(
                    "updated {key} from {provider_name}:{} in {}; receipt recorded as {}{}",
                    provider_id,
                    file.display(),
                    receipt_key,
                    if add_integrity {
                        " and provider integrity added"
                    } else {
                        "; review it before adding integrity"
                    }
                );
            } else {
                print!("{}", history::clean_source(&output)?);
            }
            Ok(0)
        }
        SourceCommand::Trace { file, key, compact } => {
            let source = read_file(&file)?;
            let records = parse(&source)?;
            let record = records
                .iter()
                .find(|record| !record.is_system() && record.entry_key == key)
                .with_context(|| format!("citation key not found: {key}"))?;
            let trace = provenance::trace(record, &records)
                .with_context(|| format!("provider evidence for {key} is not valid"))?;
            print_serializable(&trace, compact)?;
            Ok(0)
        }
        SourceCommand::StripResponses {
            file,
            in_place,
            history_write,
        } => {
            let source = read_file(&file)?;
            let (output, removed) =
                remove_entry_type_fields(&source, SOURCE_TYPE, &["response", "responseencoding"])?;
            if in_place {
                commit_edit(
                    &file,
                    &source,
                    &output,
                    "source-strip-responses",
                    &history_write,
                )?;
                eprintln!(
                    "removed {removed} legacy response field(s) from {}",
                    file.display()
                );
            } else {
                print!("{}", history::clean_source(&output)?);
            }
            Ok(0)
        }
    }
}

fn run_web(
    file: &Path,
    keys: Vec<String>,
    all: bool,
    in_place: bool,
    compact: bool,
    history_write: &HistoryWriteArgs,
) -> Result<u8> {
    let original = read_file(file)?;
    let initial_records = parse(&original)?;
    let selected = selected_keys(&initial_records, keys, all)?;
    let mut output = original.clone();
    let mut rows = Vec::new();
    let mut needs_review = false;

    for key in selected {
        let records = parse(&output)?;
        let record = records
            .iter()
            .find(|record| !record.is_system() && record.entry_key == key)
            .cloned()
            .with_context(|| format!("citation key not found: {key}"))?;
        if status(&record, &records)? == Status::Verified {
            rows.push(WebRow {
                id: key,
                status: "already-verified",
                error: None,
                evidence: None,
            });
            continue;
        }
        let Some(url) = record.fields.get("url") else {
            needs_review = true;
            rows.push(WebRow {
                id: key,
                status: "missing-url",
                error: Some("entry has no URL".to_owned()),
                evidence: None,
            });
            continue;
        };
        match resolver::fetch_web_evidence(url) {
            Ok(evidence) => {
                let source = provenance::web_source(&record, &evidence)?;
                output = provenance::append_source(&output, &source, &records)?;
                output = update_entry_fields(
                    &output,
                    &key,
                    &record.entry_type,
                    &BTreeMap::from([(SOURCE_FIELD.to_owned(), source.entry_key)]),
                )?;
                let records = parse(&output)?;
                output = update_source(&output, &records, &BTreeSet::from([key.clone()]), false)?;
                let records = parse(&output)?;
                let target = records
                    .iter()
                    .find(|record| record.entry_key == key)
                    .context("updated entry missing")?;
                let trust = status(target, &records)?;
                needs_review |= trust != Status::Verified;
                rows.push(WebRow {
                    id: key,
                    status: if !in_place {
                        "ready"
                    } else {
                        match trust {
                            Status::Verified => "verified",
                            Status::Valid => "valid",
                            Status::Stale => "stale",
                            Status::Invalid => "invalid",
                        }
                    },
                    error: None,
                    evidence: Some(evidence),
                });
            }
            Err(error) => {
                needs_review = true;
                rows.push(WebRow {
                    id: key,
                    status: "fetch-error",
                    error: Some(format!("{error:#}")),
                    evidence: None,
                });
            }
        }
    }

    if in_place && output != original {
        commit_edit(file, &original, &output, "source-web", history_write)?;
    }
    print_serializable(&rows, compact)?;
    Ok(if needs_review { 3 } else { 0 })
}

#[allow(clippy::too_many_arguments)]
fn run_verify(
    file: &Path,
    keys: Vec<String>,
    all: bool,
    provider_order: &[String],
    limit: usize,
    in_place: bool,
    reviewed: Option<&Path>,
    compact: bool,
    mailto: Option<&str>,
    history_write: &HistoryWriteArgs,
) -> Result<u8> {
    if provider_order.is_empty() {
        bail!("--providers must contain at least one provider");
    }
    let mut backends = BTreeMap::new();
    for name in provider_order {
        let normalized = name.to_ascii_lowercase();
        if !backends.contains_key(&normalized) {
            backends.insert(normalized.clone(), providers::open(&normalized, mailto)?);
        }
    }

    let original = read_file(file)?;
    let initial_records = parse(&original)?;
    let selected = selected_keys(&initial_records, keys, all)?;
    let mut output = original.clone();
    let mut rows = Vec::new();
    let mut needs_review = false;
    let reviewed: Option<serde_json::Value> = reviewed
        .map(|path| -> Result<_> {
            let value: serde_json::Value = serde_json::from_str(&fs::read_to_string(path)?)?;
            if !value.is_array() {
                bail!("--reviewed requires a source verify JSON report");
            }
            Ok(value)
        })
        .transpose()?;

    for key in selected {
        let records = parse(&output)?;
        let record = records
            .iter()
            .find(|record| !record.is_system() && record.entry_key == key)
            .cloned()
            .with_context(|| format!("citation key not found: {key}"))?;
        let summary = provenance::summary(&record, &records);
        if status(&record, &records)? == Status::Verified {
            rows.push(VerifyRow {
                id: key,
                status: "already-verified",
                provider: summary.provider,
                provider_id: summary.provider_id,
                error: None,
                resolution: None,
                candidates: vec![],
            });
            continue;
        }

        let mut resolution = None;
        let identifier = record
            .fields
            .get("doi")
            .map(|doi| LiteratureIdentifier::Doi(doi.replace("\\_", "_")))
            .or_else(|| {
                let mut non_url = record.clone();
                non_url.fields.remove("url");
                let (identifier, candidate) = identifier_from_bibtex_fields(&non_url)?;
                resolution = Some(ResolutionReport {
                    input: candidate.evidence.input_url.clone(),
                    status: "resolved",
                    candidates: vec![candidate],
                    warnings: vec![],
                });
                Some(identifier)
            })
            .or_else(|| {
                let url = record.fields.get("url")?;
                if let Some(arxiv_id) = resolver::arxiv_id_in_url(url) {
                    let candidate = resolver::ResolutionCandidate {
                        kind: "arxiv".to_owned(),
                        value: arxiv_id.clone(),
                        confidence: resolver::ResolutionConfidence::Exact,
                        signals: vec![resolver::MatchSignal {
                            kind: "arxiv-id-in-url".to_owned(),
                            value: arxiv_id.clone(),
                        }],
                        evidence: resolver::ResolutionEvidence {
                            method: "arxiv-url".to_owned(),
                            input_url: url.clone(),
                            final_url: url.clone(),
                            request_url: None,
                            media_type: None,
                            response_sha256: None,
                            response_bytes: 0,
                        },
                    };
                    resolution = Some(ResolutionReport {
                        input: url.clone(),
                        status: "resolved",
                        candidates: vec![candidate],
                        warnings: vec![],
                    });
                    return Some(LiteratureIdentifier::Doi(format!(
                        "10.48550/arXiv.{arxiv_id}"
                    )));
                }
                match resolver::resolve_url(url) {
                    Ok(report) => match report.exact_candidate() {
                        Ok(candidate) if candidate.kind == "doi" => {
                            let identifier = LiteratureIdentifier::Doi(candidate.value.clone());
                            resolution = Some(report);
                            Some(identifier)
                        }
                        Ok(candidate) if candidate.kind == "arxiv" => {
                            let identifier = LiteratureIdentifier::Doi(format!(
                                "10.48550/arXiv.{}",
                                candidate.value
                            ));
                            resolution = Some(report);
                            Some(identifier)
                        }
                        _ => {
                            resolution = Some(report);
                            None
                        }
                    },
                    Err(error) => {
                        rows.push(VerifyRow {
                            id: key.clone(),
                            status: "resolution-error",
                            provider: None,
                            provider_id: None,
                            error: Some(format!("{error:#}")),
                            resolution: None,
                            candidates: vec![],
                        });
                        needs_review = true;
                        None
                    }
                }
            })
            .or_else(|| {
                let (identifier, candidate) = identifier_from_bibtex_fields(&record)?;
                resolution = Some(ResolutionReport {
                    input: candidate.evidence.input_url.clone(),
                    status: "resolved",
                    candidates: vec![candidate],
                    warnings: vec![],
                });
                Some(identifier)
            });

        if rows.last().is_some_and(|row| row.id == key) {
            continue;
        }

        if let Some(identifier) = identifier {
            let mut errors = Vec::new();
            let mut applied = false;
            for provider_name in provider_order {
                let normalized = provider_name.to_ascii_lowercase();
                let backend = backends
                    .get(&normalized)
                    .expect("provider was opened before verification");
                match backend.lookup(&identifier) {
                    Ok(fetched) => {
                        if let Some(reviewed) = &reviewed {
                            let candidate = reviewed
                                .as_array()
                                .expect("checked array")
                                .iter()
                                .find(|row| row["id"].as_str() == Some(key.as_str()))
                                .and_then(|row| row["candidates"].as_array())
                                .and_then(|rows| rows.first())
                                .context("reviewed report has no exact replacement for this key")?;
                            let after =
                                biblock_cli::catalog::proposed_record(&record, &fetched.record);
                            if candidate["baseline_hash"].as_str() != Some(hash(&record)?.as_str())
                                || candidate["proposal_hash"].as_str()
                                    != Some(hash(&after)?.as_str())
                            {
                                bail!(
                                    "entry {key} or API metadata changed since dry run; review again"
                                );
                            }
                        }
                        let provider_id = fetched.record.id.clone();
                        match reconcile_fetched(
                            &output,
                            &key,
                            &fetched,
                            resolution
                                .as_ref()
                                .and_then(|report| report.exact_candidate().ok()),
                            None,
                            true,
                        ) {
                            Ok((updated, _)) => {
                                let updated_records = parse(&updated)?;
                                let updated_record = updated_records
                                    .iter()
                                    .find(|record| record.entry_key == key)
                                    .context("updated entry missing")?;
                                let verified =
                                    status(updated_record, &updated_records)? == Status::Verified;
                                needs_review |= !verified;
                                output = updated;
                                rows.push(VerifyRow {
                                    id: key.clone(),
                                    status: if !verified {
                                        "needs-review"
                                    } else if in_place {
                                        "verified"
                                    } else {
                                        "ready"
                                    },
                                    provider: Some(normalized),
                                    provider_id: Some(provider_id),
                                    error: None,
                                    resolution: resolution.clone(),
                                    candidates: vec![planned_candidate(
                                        &record,
                                        Candidate {
                                            score: None,
                                            record: fetched.record.clone(),
                                        },
                                    )],
                                });
                                applied = true;
                                break;
                            }
                            Err(error) => errors.push(format!("{normalized}: {error:#}")),
                        }
                    }
                    Err(error) => errors.push(format!("{normalized}: {error:#}")),
                }
            }
            if !applied {
                needs_review = true;
                rows.push(VerifyRow {
                    id: key,
                    status: "lookup-error",
                    provider: None,
                    provider_id: None,
                    error: Some(errors.join("; ")),
                    resolution,
                    candidates: vec![],
                });
            }
            continue;
        }

        if let Some(report) = resolution {
            needs_review = true;
            rows.push(VerifyRow {
                id: key,
                status: "unsupported-identifier",
                provider: None,
                provider_id: None,
                error: report
                    .exact_candidate()
                    .err()
                    .map(|error| format!("{error:#}")),
                resolution: Some(report),
                candidates: vec![],
            });
            continue;
        }

        let Some(search_provider) = provider_order
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .find(|name| name != providers::DOI_PROVIDER)
        else {
            needs_review = true;
            rows.push(VerifyRow {
                id: key,
                status: "unsupported",
                provider: None,
                provider_id: None,
                error: Some("no search-capable provider configured".to_owned()),
                resolution: None,
                candidates: vec![],
            });
            continue;
        };
        let query = title_query(&record);
        match backends[&search_provider].search(&query, limit) {
            Ok(search) => {
                needs_review = true;
                rows.push(VerifyRow {
                    id: key,
                    status: "needs-selection",
                    provider: Some(search_provider),
                    provider_id: None,
                    error: None,
                    resolution: None,
                    candidates: search
                        .candidates
                        .into_iter()
                        .map(|candidate| planned_candidate(&record, candidate))
                        .collect(),
                });
            }
            Err(error) => {
                needs_review = true;
                rows.push(VerifyRow {
                    id: key,
                    status: "search-error",
                    provider: Some(search_provider),
                    provider_id: None,
                    error: Some(format!("{error:#}")),
                    resolution: None,
                    candidates: vec![],
                });
            }
        }
    }

    if in_place && output != original {
        commit_edit(file, &original, &output, "source-verify", history_write)?;
        eprintln!(
            "provider-verified {} of {} selected entries in {}",
            rows.iter()
                .filter(|row| matches!(row.status, "verified" | "already-verified"))
                .count(),
            rows.len(),
            file.display()
        );
    }
    print_serializable(&rows, compact)?;
    Ok(if needs_review { 3 } else { 0 })
}

struct SelectionEvidence {
    search: Record,
    selection: Record,
}

fn reconcile_fetched(
    source: &str,
    key: &str,
    fetched: &providers::FetchedRecord,
    resolution_candidate: Option<&resolver::ResolutionCandidate>,
    selection_evidence: Option<&SelectionEvidence>,
    add_integrity: bool,
) -> Result<(String, String)> {
    if let Some(candidate) = resolution_candidate
        && candidate.kind == "doi"
        && !candidate.value.eq_ignore_ascii_case(&fetched.record.id)
    {
        bail!(
            "resolved DOI {} does not match provider record id {}",
            candidate.value,
            fetched.record.id
        );
    }
    let resolution_source = resolution_candidate
        .map(provenance::resolution_source)
        .transpose()?;
    let provider_source = provenance::provider_source_with_evidence(
        fetched,
        resolution_source
            .as_ref()
            .map(|source| source.entry_key.as_str()),
        selection_evidence.map(|evidence| evidence.selection.entry_key.as_str()),
    );
    let mut fields = fetched.record.bibtex_fields();
    fields.insert(SOURCE_FIELD.to_owned(), provider_source.entry_key.clone());
    let without_approval = update_entry_fields_exact(
        source,
        key,
        fetched.record.bibtex_type(),
        &BTreeMap::new(),
        APPROVAL_FIELDS,
    )?;
    let mut output = update_entry_fields_exact(
        &without_approval,
        key,
        fetched.record.bibtex_type(),
        &fields,
        CONTROLLED_FIELDS,
    )?;
    let mut records =
        parse(&output).context("provider update produced invalid BibTeX; file was not changed")?;
    if let Some(resolution_source) = &resolution_source {
        output = provenance::append_source(&output, resolution_source, &records)?;
        records = parse(&output)?;
    }
    if let Some(evidence) = selection_evidence {
        output = provenance::append_source(&output, &evidence.search, &records)?;
        records = parse(&output)?;
        output = provenance::append_source(&output, &evidence.selection, &records)?;
        records = parse(&output)?;
    }
    output = provenance::append_source(&output, &provider_source, &records)?;
    if add_integrity {
        let records = parse(&output)?;
        output = update_source(&output, &records, &BTreeSet::from([key.to_owned()]), false)?;
    }
    Ok((output, provider_source.entry_key))
}

fn identifier_from_bibtex_fields(
    record: &Record,
) -> Option<(LiteratureIdentifier, resolver::ResolutionCandidate)> {
    resolver::identifier_from_record(record)
}

fn planned_candidate(record: &Record, candidate: Candidate) -> PlannedCandidate {
    let similarity = dedupe::literature_similarity(record, &candidate.record);
    let proposed = biblock_cli::catalog::proposed_record(record, &candidate.record);
    let proposed = proposal::clean_record(&proposed).expect("serializable bibliography record");
    PlannedCandidate {
        provider_score: candidate.score,
        match_score: similarity.map(|value| value.score),
        title_score: similarity.map(|value| value.title_score),
        author_score: similarity.map(|value| value.author_score),
        changes: changes(record, &candidate.record),
        record: candidate.record,
        baseline_hash: hash(record).expect("serializable bibliography record"),
        proposal_hash: hash(&proposed).expect("serializable bibliography record"),
        proposed,
    }
}

fn source_identity(record: &Record, requested: Option<&str>) -> (String, Option<String>) {
    let stored = record
        .fields
        .get(PROVIDER_FIELD)
        .zip(record.fields.get(PROVIDER_ID_FIELD))
        .map(|(provider, id)| (provider.as_str(), id.as_str()));
    match requested {
        Some(provider) => (
            provider.to_owned(),
            stored
                .and_then(|(stored_provider, id)| (stored_provider == provider).then_some(id))
                .map(str::to_owned),
        ),
        None => stored.map_or_else(
            || (DEFAULT_PROVIDER.to_owned(), None),
            |(provider, id)| (provider.to_owned(), Some(id.to_owned())),
        ),
    }
}

fn title_query(record: &Record) -> BibliographicQuery {
    title_search_query(record)
}

fn select_records(records: &[Record], keys: Vec<String>, all: bool) -> Result<Vec<&Record>> {
    let selected: BTreeSet<_> = keys.into_iter().collect();
    if !all && selected.is_empty() {
        bail!("no entries selected; pass --key KEY or --all");
    }
    ensure_keys_exist(records, &selected)?;
    Ok(records
        .iter()
        .filter(|record| !record.is_system() && (all || selected.contains(&record.entry_key)))
        .collect())
}

fn print_serializable(value: &impl Serialize, compact: bool) -> Result<()> {
    if compact {
        println!("{}", serde_json::to_string(value)?);
    } else {
        println!("{}", serde_json::to_string_pretty(value)?);
    }
    Ok(())
}

fn run_integrity(command: IntegrityCommand) -> Result<u8> {
    match command {
        IntegrityCommand::Status {
            file,
            json,
            key,
            keys_from,
        } => {
            let source = read_file(&file)?;
            let records = parse(&source)?;
            let selected: BTreeSet<_> = merge_keys(key, keys_from)?.into_iter().collect();
            ensure_keys_exist(&records, &selected)?;
            let bibliography: Vec<_> = records
                .iter()
                .filter(|record| {
                    !record.is_system()
                        && (selected.is_empty() || selected.contains(&record.entry_key))
                })
                .collect();
            let rows = bibliography
                .iter()
                .map(|record| {
                    Ok(StatusRow {
                        id: &record.entry_key,
                        status: status(record, &records)?,
                        expected: hash(record)?,
                        stored: record.fields.get("integrity").map(String::as_str),
                        source: provenance::summary(record, &records),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                for row in &rows {
                    let origin = row
                        .source
                        .kind
                        .as_deref()
                        .map(|kind| format!("\t{kind}"))
                        .unwrap_or_default();
                    println!("{}\t{}{}", row.status, row.id, origin);
                }
            }
            let lock = history::status(&file, None)?;
            if !lock.valid {
                eprintln!("lockfile validation failed: {}", lock.errors.join("; "));
            }
            Ok(
                if lock.valid && rows.iter().all(|row| row.status == Status::Verified) {
                    0
                } else {
                    3
                },
            )
        }
        IntegrityCommand::Hash { file, key } => {
            let source = read_file(&file)?;
            let records = parse(&source)?;
            let record = records
                .iter()
                .find(|record| !record.is_system() && record.entry_key == key)
                .with_context(|| format!("citation key not found: {key}"))?;
            println!("{}", hash(record)?);
            Ok(0)
        }
        IntegrityCommand::Add {
            file,
            key,
            keys_from,
            all,
            in_place,
            source,
            agent,
            reviewer,
            history_write,
        } => add_integrity(
            &file,
            merge_keys(key, keys_from)?,
            all,
            in_place,
            source,
            agent,
            reviewer,
            &history_write,
        ),
        IntegrityCommand::Remove {
            file,
            key,
            keys_from,
            all,
            in_place,
            history_write,
        } => remove_integrity(
            &file,
            merge_keys(key, keys_from)?,
            all,
            in_place,
            &history_write,
        ),
    }
}

fn run_history(command: HistoryCommand) -> Result<u8> {
    match command {
        HistoryCommand::Status {
            file,
            history: history_path,
            json,
        } => {
            let report = history::status(&file, history_path.as_deref())?;
            if json {
                print_serializable(&report, false)?;
            } else {
                println!(
                    "{}\t{} revisions\t{} referenced\t{} orphaned\t{}",
                    report.state,
                    report.revisions,
                    report.referenced,
                    report.orphaned,
                    report.lockfile
                );
                for error in &report.errors {
                    println!("error\t{error}");
                }
            }
            Ok(if report.valid { 0 } else { 3 })
        }
        HistoryCommand::Log {
            file,
            key,
            history: history_path,
            compact,
        } => {
            let revisions = history::log(&file, history_path.as_deref(), &key)?;
            print_serializable(&revisions, compact)?;
            Ok(0)
        }
        HistoryCommand::Show {
            file,
            revision,
            history: history_path,
        } => {
            let snapshot = history::snapshot(&file, history_path.as_deref(), &revision)?;
            print!("{}", render(&[snapshot])?);
            Ok(0)
        }
        HistoryCommand::Diff {
            file,
            revision,
            history: history_path,
            compact,
        } => {
            let snapshot = history::snapshot(&file, history_path.as_deref(), &revision)?;
            let revision_view = history::revision_view(&file, history_path.as_deref(), &revision)?;
            let source = read_file(&file)?;
            let records = parse(&source)?;
            let current = records
                .iter()
                .find(|record| record.entry_key == snapshot.entry_key)
                .with_context(|| format!("citation key not found: {}", snapshot.entry_key))?;
            let mut field_names: BTreeSet<_> = snapshot.fields.keys().cloned().collect();
            field_names.extend(current.fields.keys().cloned());
            let field_changes: Vec<_> = field_names
                .into_iter()
                .filter_map(|field| {
                    let old = snapshot.fields.get(&field);
                    let new = current.fields.get(&field);
                    (old != new)
                        .then(|| serde_json::json!({"field": field, "old": old, "new": new}))
                })
                .collect();
            let report = serde_json::json!({
                "revision": revision_view,
                "current_type": current.entry_type,
                "snapshot_type": snapshot.entry_type,
                "changes": field_changes,
            });
            print_serializable(&report, compact)?;
            Ok(0)
        }
        HistoryCommand::Restore {
            file,
            revision,
            in_place,
            history_write,
        } => {
            let original = read_file(&file)?;
            let snapshot = history::snapshot(&file, history_write.history.as_deref(), &revision)?;
            let output = history::restore_record(
                &file,
                history_write.history.as_deref(),
                &revision,
                &original,
            )?;
            if in_place {
                commit_edit(&file, &original, &output, "history-restore", &history_write)?;
                eprintln!(
                    "restored {} from {revision} in {}",
                    snapshot.entry_key,
                    file.display()
                );
            } else {
                print!("{output}");
            }
            Ok(0)
        }
    }
}

fn commit_edit(
    file: &Path,
    expected: &str,
    output: &str,
    operation: &str,
    history_write: &HistoryWriteArgs,
) -> Result<()> {
    history::commit_edit(
        file,
        history_write.history.as_deref(),
        expected,
        output,
        operation,
        history_write.history_actor.as_deref(),
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn add_integrity(
    file: &Path,
    keys: Vec<String>,
    all: bool,
    in_place: bool,
    source_kind: IntegritySourceArg,
    agent: Option<String>,
    reviewer: Option<String>,
    history_write: &HistoryWriteArgs,
) -> Result<u8> {
    let actor = match source_kind {
        IntegritySourceArg::Provider => {
            if agent.is_some() || reviewer.is_some() {
                bail!("--source provider does not accept --agent or --reviewer");
            }
            None
        }
        IntegritySourceArg::Agent => {
            if reviewer.is_some() {
                bail!("--source agent does not accept --reviewer");
            }
            Some((
                SourceKind::Agent,
                agent.context("--source agent requires --agent ID")?,
            ))
        }
        IntegritySourceArg::Human => {
            if agent.is_some() {
                bail!("--source human does not accept --agent");
            }
            Some((
                SourceKind::Human,
                reviewer.context("--source human requires --reviewer ID")?,
            ))
        }
    };
    let original = read_file(file)?;
    let mut source = original.clone();
    let mut records = parse(&source)?;
    let selected = selected_keys(&records, keys, all)?;
    if let Some((kind, actor)) = actor {
        for key in &selected {
            let target = records
                .iter()
                .find(|record| record.entry_key == *key)
                .cloned()
                .with_context(|| format!("citation key not found: {key}"))?;
            let evidence = provenance::actor_source(kind, &actor, &target)?;
            source = provenance::append_source(&source, &evidence, &records)?;
            source = update_entry_fields(
                &source,
                key,
                &target.entry_type,
                &BTreeMap::from([(SOURCE_FIELD.to_owned(), evidence.entry_key)]),
            )?;
            records = parse(&source)?;
        }
    }
    let output = update_source(&source, &records, &selected, false)?;
    write_changed(
        file,
        &original,
        &output,
        &selected,
        in_place,
        "updated",
        "integrity-add",
        history_write,
    )
}

fn remove_integrity(
    file: &Path,
    keys: Vec<String>,
    all: bool,
    in_place: bool,
    history_write: &HistoryWriteArgs,
) -> Result<u8> {
    let source = read_file(file)?;
    let records = parse(&source)?;
    let selected = selected_keys(&records, keys, all)?;
    let mut output = update_source(&source, &records, &selected, true)?;
    for key in &selected {
        let record = records
            .iter()
            .find(|record| record.entry_key == *key)
            .with_context(|| format!("citation key not found: {key}"))?;
        output = update_entry_fields_exact(
            &output,
            key,
            &record.entry_type,
            &BTreeMap::new(),
            APPROVAL_FIELDS,
        )?;
    }
    write_changed(
        file,
        &source,
        &output,
        &selected,
        in_place,
        "removed",
        "integrity-remove",
        history_write,
    )
}

fn selected_keys(records: &[Record], keys: Vec<String>, all: bool) -> Result<BTreeSet<String>> {
    let selected = if all {
        records
            .iter()
            .filter(|record| !record.is_system())
            .map(|record| record.entry_key.clone())
            .collect()
    } else {
        let selected: BTreeSet<_> = keys.into_iter().collect();
        if selected.is_empty() {
            bail!("no entries selected; pass --key KEY or --all after review");
        }
        selected
    };
    ensure_keys_exist(records, &selected)?;
    Ok(selected)
}

#[allow(clippy::too_many_arguments)]
fn write_changed(
    file: &Path,
    expected: &str,
    output: &str,
    selected: &BTreeSet<String>,
    in_place: bool,
    action: &str,
    operation: &str,
    history_write: &HistoryWriteArgs,
) -> Result<u8> {
    if in_place {
        commit_edit(file, expected, output, operation, history_write)?;
        eprintln!(
            "{action} integrity for {} entr{} in {}",
            selected.len(),
            if selected.len() == 1 { "y" } else { "ies" },
            file.display()
        );
    } else {
        let preview = history::preview(file, history_write.history.as_deref(), output)?;
        print_serializable(&preview, false)?;
    }
    Ok(0)
}

fn ensure_keys_exist(records: &[Record], selected: &BTreeSet<String>) -> Result<()> {
    let existing: BTreeSet<_> = records
        .iter()
        .filter(|record| !record.is_system())
        .map(|record| &record.entry_key)
        .collect();
    for key in selected {
        if !existing.contains(key) {
            bail!("citation key not found: {key}");
        }
    }
    Ok(())
}

fn merge_keys(mut keys: Vec<String>, keys_from: Option<PathBuf>) -> Result<Vec<String>> {
    let Some(path) = keys_from else {
        return Ok(keys);
    };
    let source = if path == Path::new("-") {
        read_stdin("citation keys")?
    } else {
        fs::read_to_string(&path)
            .with_context(|| format!("could not read citation keys from {}", path.display()))?
    };
    keys.extend(
        source
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned),
    );
    if keys.is_empty() {
        bail!("no citation keys found in {}", path.display());
    }
    Ok(keys)
}

fn read_bib_inputs(files: &[PathBuf]) -> Result<Vec<Record>> {
    Ok(read_bib_inputs_with_origins(files)?
        .into_iter()
        .map(|item| item.record)
        .collect())
}

fn read_bib_inputs_with_origins(files: &[PathBuf]) -> Result<Vec<LocatedRecord>> {
    if files.is_empty() {
        return Ok(parse(&read_stdin("BibTeX")?)?
            .into_iter()
            .map(|record| LocatedRecord {
                file: "stdin".to_owned(),
                record,
            })
            .collect());
    }
    let mut records = Vec::new();
    let mut citation_origins = BTreeMap::new();
    for file in files {
        let origin = if file == Path::new("-") {
            "stdin".to_owned()
        } else {
            file.display().to_string()
        };
        let source = if file == Path::new("-") {
            read_stdin("BibTeX")?
        } else {
            read_file(file)?
        };
        let parsed = parse(&source)?;
        for record in &parsed {
            if record.is_system() {
                continue;
            }
            if let Some(previous) =
                citation_origins.insert(record.entry_key.clone(), origin.clone())
            {
                bail!(
                    "duplicate citation key: {} (found in {previous} and {origin})",
                    record.entry_key
                );
            }
        }
        records.extend(parsed.into_iter().map(|record| LocatedRecord {
            file: origin.clone(),
            record,
        }));
    }
    Ok(records)
}

fn parse_score(value: &str) -> Result<f64, String> {
    let score = value
        .parse::<f64>()
        .map_err(|_| "score must be a number from 0 to 1".to_owned())?;
    (score.is_finite() && (0.0..=1.0).contains(&score))
        .then_some(score)
        .ok_or_else(|| "score must be a number from 0 to 1".to_owned())
}

fn read_file(path: &Path) -> Result<String> {
    history::hydrate_file(path)
}

fn read_stdin(description: &str) -> Result<String> {
    if io::stdin().is_terminal() {
        bail!("no {description} on stdin");
    }
    let mut source = String::new();
    io::stdin()
        .read_to_string(&mut source)
        .context("could not read stdin")?;
    Ok(source)
}

fn parse_limit(value: &str) -> Result<usize, String> {
    let limit = value
        .parse::<usize>()
        .map_err(|_| "limit must be an integer from 1 to 20".to_owned())?;
    (1..=20)
        .contains(&limit)
        .then_some(limit)
        .ok_or_else(|| "limit must be an integer from 1 to 20".to_owned())
}
