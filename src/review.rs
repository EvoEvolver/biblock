use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::bibtex::{Record, parse};
use crate::catalog::{BibliographicQuery, LiteratureIdentifier, title_search_query};
use crate::dedupe::{self, Similarity};
use crate::history;
use crate::integrity::{
    APPROVAL_FIELDS, Status, replace_entry, status, update_entry_fields_exact, update_source,
};
use crate::provenance::{self, CONTROLLED_FIELDS, SOURCE_FIELD};
use crate::providers;

const PAGE: &str = include_str!("review_web/index.html");
const STYLE: &str = include_str!("review_web/styles.css");
const APP: &str = include_str!("review_web/app.js");

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewState {
    file: String,
    reviewer_default: String,
    total: usize,
    verified: usize,
    threshold: f64,
    entries: Vec<ReviewEntry>,
    proposals: Vec<crate::proposal::ProposalView>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewEntry {
    id: String,
    entry_type: String,
    status: Status,
    title: Option<String>,
    authors: Option<String>,
    year: Option<String>,
    doi: Option<String>,
    url: Option<String>,
    fields: std::collections::BTreeMap<String, String>,
    source: serde_json::Value,
    content_hash: String,
    approval: Option<history::ApprovalLock>,
}

#[derive(Deserialize)]
struct ApproveRequest {
    keys: Vec<String>,
    reviewer: Option<String>,
    hashes: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct CandidateRequest {
    key: String,
    provider: String,
}

#[derive(Deserialize)]
struct AdoptRequest {
    key: String,
    id: String,
    provider: String,
    reviewer: Option<String>,
    preview_hash: String,
}

#[derive(Deserialize)]
struct BibtexRequest {
    key: String,
    bibtex: String,
}

#[derive(Deserialize)]
struct ApplyBibtexRequest {
    key: String,
    bibtex: String,
    reviewer: Option<String>,
    baseline_hash: String,
    proposal_hash: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BibtexPreview {
    pasted_key: String,
    entry_type: String,
    comparison: Vec<FieldComparison>,
    baseline_hash: String,
    proposal_hash: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CandidateResponse {
    provider: String,
    query: String,
    threshold: f64,
    candidates: Vec<CandidateView>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CandidateView {
    provider_score: Option<f64>,
    similarity: Option<Similarity>,
    adoptable: bool,
    record: crate::catalog::LiteratureRecord,
    entry_type: String,
    comparison: Vec<FieldComparison>,
    preview_hash: String,
}

struct ReviewDraft {
    expected: String,
    proposed: String,
    key: String,
    provider: String,
    id: String,
}

#[derive(Deserialize)]
struct ProposalDecisionRequest {
    id: String,
    adopt: bool,
    reviewer: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FieldComparison {
    field: String,
    current: Option<String>,
    proposed: Option<String>,
    state: &'static str,
}

pub fn run(file: &Path, port: u16, open: bool, threshold: f64) -> Result<u8> {
    state(file, threshold)?;
    let server = Server::http(("127.0.0.1", port))
        .map_err(|error| anyhow::anyhow!("could not start review server: {error}"))?;
    let token = token(file)?;
    let base = format!("http://{}/{token}", server.server_addr());
    println!("Review {} at {base}/", file.display());
    if open && webbrowser::open(&format!("{base}/")).is_err() {
        eprintln!("biblock: could not open the browser automatically");
    }

    let mut finished = false;
    let mut drafts = BTreeMap::new();
    while !finished {
        let request = server
            .recv()
            .context("review server stopped unexpectedly")?;
        let path = request.url().split('?').next().unwrap_or(request.url());
        let root = format!("/{token}/");
        let state_path = format!("/{token}/api/state");
        let style_path = format!("/{token}/styles.css");
        let app_path = format!("/{token}/app.js");
        let approve_path = format!("/{token}/api/approve");
        let candidates_path = format!("/{token}/api/candidates");
        let adopt_path = format!("/{token}/api/adopt");
        let bibtex_preview_path = format!("/{token}/api/bibtex/preview");
        let bibtex_apply_path = format!("/{token}/api/bibtex/apply");
        let finish_path = format!("/{token}/api/finish");
        let proposal_path = format!("/{token}/api/proposal");
        match (request.method(), path) {
            (&Method::Get, value) if value == root => respond_html(request, PAGE)?,
            (&Method::Get, value) if value == style_path => {
                respond_asset(request, "text/css; charset=utf-8", STYLE)?
            }
            (&Method::Get, value) if value == app_path => {
                respond_asset(request, "text/javascript; charset=utf-8", APP)?
            }
            (&Method::Get, value) if value == state_path => {
                respond_json(request, StatusCode(200), &state(file, threshold)?)?
            }
            (&Method::Post, value) if value == approve_path => {
                handle_approve(request, file, threshold)?
            }
            (&Method::Post, value) if value == candidates_path => {
                handle_candidates(request, file, threshold, &mut drafts)?
            }
            (&Method::Post, value) if value == adopt_path => {
                handle_adopt(request, file, threshold, &drafts)?
            }
            (&Method::Post, value) if value == bibtex_preview_path => {
                handle_bibtex_preview(request, file)?
            }
            (&Method::Post, value) if value == bibtex_apply_path => {
                handle_bibtex_apply(request, file, threshold)?
            }
            (&Method::Post, value) if value == finish_path => {
                respond_json(request, StatusCode(200), &state(file, threshold)?)?;
                finished = true;
            }
            (&Method::Post, value) if value == proposal_path => {
                handle_proposal(request, file, threshold)?
            }
            _ => respond_text(request, StatusCode(404), "Not found")?,
        }
    }
    let final_state = state(file, threshold)?;
    Ok(
        if final_state.verified == final_state.total
            && !final_state
                .proposals
                .iter()
                .any(|proposal| proposal.proposal.decision.is_none())
        {
            0
        } else {
            3
        },
    )
}

fn handle_approve(mut request: Request, file: &Path, threshold: f64) -> Result<()> {
    let mut body = String::new();
    request
        .as_reader()
        .take(1024 * 1024)
        .read_to_string(&mut body)
        .context("could not read approval request")?;
    let input: ApproveRequest = match serde_json::from_str(&body) {
        Ok(input) => input,
        Err(error) => {
            return respond_text(
                request,
                StatusCode(400),
                &format!("Invalid request: {error}"),
            );
        }
    };
    let current = state(file, threshold)?;
    let allowed: BTreeSet<_> = current
        .entries
        .iter()
        .filter(|entry| entry.status != Status::Verified)
        .map(|entry| entry.id.as_str())
        .collect();
    let keys: BTreeSet<_> = input.keys.into_iter().collect();
    if keys.is_empty() || keys.iter().any(|key| !allowed.contains(key.as_str())) {
        return respond_text(
            request,
            StatusCode(400),
            "Select entries that still need review",
        );
    }
    let reviewer = input.reviewer.as_deref().map(str::trim);
    if current.proposals.iter().any(|proposal| {
        proposal.proposal.decision.is_none() && keys.contains(&proposal.proposal.target)
    }) {
        return respond_text(
            request,
            StatusCode(409),
            "Review the pending proposal instead of approving the old entry",
        );
    }
    if let Err(error) = history::approve_bound(file, &keys, reviewer, Some(&input.hashes)) {
        return respond_text(request, StatusCode(409), &format!("{error:#}"));
    }
    respond_json(request, StatusCode(200), &state(file, threshold)?)
}

fn handle_candidates(
    mut request: Request,
    file: &Path,
    threshold: f64,
    drafts: &mut BTreeMap<String, ReviewDraft>,
) -> Result<()> {
    let input: CandidateRequest = match read_json(&mut request) {
        Ok(input) => input,
        Err(error) => return respond_text(request, StatusCode(400), &format!("{error:#}")),
    };
    match search_candidates(file, &input.key, &input.provider, threshold, drafts) {
        Ok(result) => respond_json(request, StatusCode(200), &result),
        Err(error) => respond_text(request, StatusCode(502), &format!("{error:#}")),
    }
}

fn handle_adopt(
    mut request: Request,
    file: &Path,
    threshold: f64,
    drafts: &BTreeMap<String, ReviewDraft>,
) -> Result<()> {
    let input: AdoptRequest = match read_json(&mut request) {
        Ok(input) => input,
        Err(error) => return respond_text(request, StatusCode(400), &format!("{error:#}")),
    };
    match adopt_candidate(file, &input, drafts) {
        Ok(()) => respond_json(request, StatusCode(200), &state(file, threshold)?),
        Err(error) => respond_text(request, StatusCode(409), &format!("{error:#}")),
    }
}

fn handle_proposal(mut request: Request, file: &Path, threshold: f64) -> Result<()> {
    let input: ProposalDecisionRequest = match read_json(&mut request) {
        Ok(input) => input,
        Err(error) => return respond_text(request, StatusCode(400), &format!("{error:#}")),
    };
    match crate::proposal::decide(file, &input.id, input.adopt, input.reviewer.as_deref()) {
        Ok(()) => respond_json(request, StatusCode(200), &state(file, threshold)?),
        Err(error) => respond_text(request, StatusCode(409), &format!("{error:#}")),
    }
}

fn handle_bibtex_preview(mut request: Request, file: &Path) -> Result<()> {
    let input: BibtexRequest = match read_json(&mut request) {
        Ok(input) => input,
        Err(error) => return respond_text(request, StatusCode(400), &format!("{error:#}")),
    };
    match preview_bibtex(file, &input) {
        Ok(preview) => respond_json(request, StatusCode(200), &preview),
        Err(error) => respond_text(request, StatusCode(400), &format!("{error:#}")),
    }
}

fn handle_bibtex_apply(mut request: Request, file: &Path, threshold: f64) -> Result<()> {
    let input: ApplyBibtexRequest = match read_json(&mut request) {
        Ok(input) => input,
        Err(error) => return respond_text(request, StatusCode(400), &format!("{error:#}")),
    };
    match apply_bibtex(file, &input) {
        Ok(()) => respond_json(request, StatusCode(200), &state(file, threshold)?),
        Err(error) => respond_text(request, StatusCode(400), &format!("{error:#}")),
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(request: &mut Request) -> Result<T> {
    let mut body = String::new();
    request
        .as_reader()
        .take(1024 * 1024)
        .read_to_string(&mut body)
        .context("could not read request")?;
    serde_json::from_str(&body).context("invalid JSON request")
}

fn pasted_record(bibtex: &str, target_key: &str) -> Result<(String, Record)> {
    if bibtex.trim().is_empty() {
        bail!("Paste one BibTeX entry");
    }
    let mut records: Vec<_> = parse(bibtex)?
        .into_iter()
        .filter(|record| !record.is_system())
        .collect();
    if records.len() != 1 {
        bail!("Pasted BibTeX must contain exactly one entry");
    }
    let mut record = records.remove(0);
    let pasted_key = record.entry_key.clone();
    record.entry_key = target_key.to_owned();
    record.fields = bibliographic_fields(&record.fields);
    Ok((pasted_key, record))
}

fn preview_bibtex(file: &Path, input: &BibtexRequest) -> Result<BibtexPreview> {
    let source = history::hydrate_file(file)?;
    let records = parse(&source)?;
    let current = records
        .iter()
        .find(|record| !record.is_system() && record.entry_key == input.key)
        .with_context(|| format!("citation key not found: {}", input.key))?;
    let (pasted_key, pasted) = pasted_record(&input.bibtex, &input.key)?;
    Ok(BibtexPreview {
        pasted_key,
        entry_type: pasted.entry_type.clone(),
        comparison: compare_fields(&bibliographic_fields(&current.fields), &pasted.fields),
        baseline_hash: crate::integrity::hash(current)?,
        proposal_hash: crate::integrity::hash(&pasted)?,
    })
}

fn apply_bibtex(file: &Path, input: &ApplyBibtexRequest) -> Result<()> {
    let expected = history::hydrate_file(file)?;
    let records = parse(&expected)?;
    if !records
        .iter()
        .any(|record| !record.is_system() && record.entry_key == input.key)
    {
        bail!("citation key not found: {}", input.key);
    }
    let (_, replacement) = pasted_record(&input.bibtex, &input.key)?;
    let current = records
        .iter()
        .find(|record| record.entry_key == input.key)
        .context("entry not found")?;
    if crate::integrity::hash(current)? != input.baseline_hash
        || crate::integrity::hash(&replacement)? != input.proposal_hash
    {
        bail!("BibTeX changed since preview; preview again before approving");
    }
    let proposed = replace_entry(&expected, &replacement)?;
    history::commit_human_edit(
        file,
        &expected,
        &proposed,
        &input.key,
        input.reviewer.as_deref(),
        "human-paste",
    )?;
    Ok(())
}

fn search_candidates(
    file: &Path,
    key: &str,
    provider: &str,
    threshold: f64,
    drafts: &mut BTreeMap<String, ReviewDraft>,
) -> Result<CandidateResponse> {
    let source = history::hydrate_file(file)?;
    let records = parse(&source)?;
    let record = records
        .iter()
        .find(|record| !record.is_system() && record.entry_key == key)
        .with_context(|| format!("citation key not found: {key}"))?;
    let query = title_query(record)?;
    let backend = providers::open(provider, None)?;
    let search = backend.search(&query, 5)?;
    drafts.retain(|_, draft| draft.key != key || draft.provider != provider);
    let candidates = search
        .candidates
        .iter()
        .map(|candidate| {
            let fetched = backend.lookup(&LiteratureIdentifier::ProviderId(
                candidate.record.id.clone(),
            ))?;
            if !fetched.record.id.eq_ignore_ascii_case(&candidate.record.id) {
                bail!("provider lookup returned a different record id");
            }
            let similarity = dedupe::literature_similarity(record, &fetched.record);
            let current = bibliographic_fields(&record.fields);
            let proposed = adopted_bibliographic_fields(
                &current,
                &bibliographic_fields(&fetched.record.bibtex_fields()),
            );
            let input = AdoptRequest {
                key: key.to_owned(),
                id: candidate.record.id.clone(),
                provider: provider.to_owned(),
                reviewer: None,
                preview_hash: String::new(),
            };
            let draft = prepare_candidate(&source, record, &input, threshold, &search, &fetched)?;
            let preview_hash = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&(
                    &draft.expected,
                    &draft.proposed,
                    &draft.key,
                    &draft.provider,
                    &draft.id
                ))?)
            );
            drafts.insert(preview_hash.clone(), draft);
            Ok(CandidateView {
                provider_score: candidate.score,
                adoptable: similarity.is_some_and(|value| value.score >= threshold),
                entry_type: fetched.record.bibtex_type().to_owned(),
                comparison: compare_fields(&current, &proposed),
                similarity,
                record: fetched.record,
                preview_hash,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(CandidateResponse {
        provider: provider.to_owned(),
        query: query.citation,
        threshold,
        candidates,
    })
}

fn adopt_candidate(
    file: &Path,
    input: &AdoptRequest,
    drafts: &BTreeMap<String, ReviewDraft>,
) -> Result<()> {
    let draft = drafts
        .get(&input.preview_hash)
        .context("preview expired; search and review again")?;
    if draft.key != input.key || draft.id != input.id || draft.provider != input.provider {
        bail!("adoption does not match the reviewed preview");
    }
    history::commit_human_edit(
        file,
        &draft.expected,
        &draft.proposed,
        &draft.key,
        input.reviewer.as_deref(),
        "provider-adopt",
    )?;
    Ok(())
}

fn prepare_candidate(
    expected: &str,
    record: &Record,
    input: &AdoptRequest,
    threshold: f64,
    search: &providers::FetchedSearch,
    fetched: &providers::FetchedRecord,
) -> Result<ReviewDraft> {
    let query = title_query(record)?;
    let candidate = search
        .candidates
        .iter()
        .find(|candidate| candidate.record.id.eq_ignore_ascii_case(&input.id))
        .with_context(|| {
            format!(
                "Crossref result {} is no longer in the candidate set",
                input.id
            )
        })?;
    let similarity = dedupe::literature_similarity(record, &fetched.record);
    let actor = input
        .reviewer
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("anonymous-browser-review");
    let search_source = provenance::search_source(record, &query, search, &input.provider)?;
    let selection_source = provenance::selection_source_with_match(
        &search_source,
        &input.id,
        actor,
        "browser-review",
        similarity.map(|similarity| provenance::MatchEvidence {
            score: similarity.score,
            title_score: similarity.title_score,
            author_score: similarity.author_score,
            threshold,
        }),
    )?;
    let provider_source =
        provenance::provider_source_with_evidence(fetched, None, Some(&selection_source.entry_key));
    let mut proposed = update_entry_fields_exact(
        expected,
        &input.key,
        &record.entry_type,
        &std::collections::BTreeMap::new(),
        APPROVAL_FIELDS,
    )?;
    let mut fields = fetched.record.bibtex_fields();
    fields.insert(SOURCE_FIELD.to_owned(), provider_source.entry_key.clone());
    proposed = update_entry_fields_exact(
        &proposed,
        &input.key,
        fetched.record.bibtex_type(),
        &fields,
        CONTROLLED_FIELDS,
    )?;
    let mut proposed_records = parse(&proposed)?;
    proposed = provenance::append_source(&proposed, &search_source, &proposed_records)?;
    proposed_records = parse(&proposed)?;
    proposed = provenance::append_source(&proposed, &selection_source, &proposed_records)?;
    proposed_records = parse(&proposed)?;
    proposed = provenance::append_source(&proposed, &provider_source, &proposed_records)?;
    proposed_records = parse(&proposed)?;
    proposed = update_source(
        &proposed,
        &proposed_records,
        &BTreeSet::from([input.key.clone()]),
        false,
    )?;
    let _ = candidate;
    Ok(ReviewDraft {
        expected: expected.to_owned(),
        proposed,
        key: input.key.clone(),
        provider: input.provider.clone(),
        id: input.id.clone(),
    })
}

fn title_query(record: &Record) -> Result<BibliographicQuery> {
    let query = title_search_query(record);
    if query.citation.is_empty() {
        bail!("entry has no title for Crossref search");
    }
    Ok(query)
}

fn state(file: &Path, threshold: f64) -> Result<ReviewState> {
    let lock_status = history::status(file, None)?;
    if !lock_status.valid {
        bail!(
            "{} is not safely locked: {}",
            file.display(),
            lock_status.errors.join("; ")
        );
    }
    let source = history::hydrate_file(file)?;
    let lock = history::read_required_lock(file, None)?;
    let records = parse(&source)?;
    let mut entries = Vec::new();
    for record in records.iter().filter(|record| !record.is_system()) {
        let entry_status = status(record, &records)?;
        entries.push(ReviewEntry {
            id: record.entry_key.clone(),
            entry_type: record.entry_type.clone(),
            status: entry_status,
            title: field(record, "title"),
            authors: field(record, "author"),
            year: field(record, "year"),
            doi: field(record, "doi"),
            url: safe_url(record),
            fields: bibliographic_fields(&record.fields),
            source: serde_json::to_value(provenance::summary(record, &records))?,
            content_hash: crate::integrity::hash(record)?,
            approval: lock
                .entries
                .get(&record.entry_key)
                .and_then(|entry| entry.approval.clone()),
        });
    }
    let verified = entries
        .iter()
        .filter(|entry| entry.status == Status::Verified)
        .count();
    let reviewer_default = hostname::get()
        .ok()
        .and_then(|value| value.into_string().ok())
        .unwrap_or_default();
    Ok(ReviewState {
        file: file.display().to_string(),
        reviewer_default,
        total: entries.len(),
        verified,
        threshold,
        entries,
        proposals: crate::proposal::list(file)?,
    })
}

fn bibliographic_fields(
    fields: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    fields
        .iter()
        .filter(|(field, _)| {
            !matches!(
                field.as_str(),
                "integrity"
                    | "bibsource"
                    | "bibprovider"
                    | "bibproviderid"
                    | "bibprevious"
                    | "bibapprovalid"
                    | "bibapprovalkind"
                    | "bibapprovalmethod"
                    | "bibapprovalreviewer"
                    | "bibapprovalcontent"
                    | "bibapprovaltimestamp"
                    | "bibapprovalbatch"
            )
        })
        .map(|(field, value)| (field.clone(), value.clone()))
        .collect()
}

fn compare_fields(
    current: &std::collections::BTreeMap<String, String>,
    proposed: &std::collections::BTreeMap<String, String>,
) -> Vec<FieldComparison> {
    current
        .keys()
        .chain(proposed.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|field| {
            let current = current.get(field).cloned();
            let proposed = proposed.get(field).cloned();
            let state = match (&current, &proposed) {
                (Some(left), Some(right)) if left == right => "unchanged",
                (Some(_), Some(_)) => "changed",
                (None, Some(_)) => "added",
                (Some(_), None) => "removed",
                (None, None) => unreachable!(),
            };
            FieldComparison {
                field: field.clone(),
                current,
                proposed,
                state,
            }
        })
        .collect()
}

fn adopted_bibliographic_fields(
    current: &std::collections::BTreeMap<String, String>,
    provider: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    let mut adopted = current.clone();
    for field in CONTROLLED_FIELDS {
        adopted.remove(*field);
    }
    adopted.extend(provider.clone());
    adopted
}

fn safe_url(record: &crate::bibtex::Record) -> Option<String> {
    let candidate = field(record, "url")
        .or_else(|| field(record, "doi").map(|doi| format!("https://doi.org/{doi}")))?;
    reqwest::Url::parse(&candidate)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .map(|url| url.to_string())
}

fn field(record: &crate::bibtex::Record, name: &str) -> Option<String> {
    record
        .fields
        .get(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn token(file: &Path) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos();
    let digest = Sha256::digest(format!("{}:{now}:{}", std::process::id(), file.display()));
    Ok(format!("{digest:x}")[..32].to_owned())
}

fn respond_html(request: Request, body: &str) -> Result<()> {
    let mut response = Response::from_string(body)
        .with_header(header("Content-Type", "text/html; charset=utf-8")?);
    response.add_header(header("Content-Security-Policy", "default-src 'none'; style-src 'self'; script-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'")?);
    request.respond(response).context("could not send response")
}

fn respond_asset(request: Request, content_type: &str, body: &str) -> Result<()> {
    request
        .respond(Response::from_string(body).with_header(header("Content-Type", content_type)?))
        .context("could not send response")
}

fn respond_json<T: Serialize>(request: Request, status: StatusCode, value: &T) -> Result<()> {
    let body = serde_json::to_string(value)?;
    request
        .respond(
            Response::from_string(body)
                .with_status_code(status)
                .with_header(header("Content-Type", "application/json; charset=utf-8")?),
        )
        .context("could not send response")
}

fn respond_text(request: Request, status: StatusCode, body: &str) -> Result<()> {
    request
        .respond(
            Response::from_string(body)
                .with_status_code(status)
                .with_header(header("Content-Type", "text/plain; charset=utf-8")?),
        )
        .context("could not send response")
}

fn header(name: &str, value: &str) -> Result<Header> {
    Header::from_bytes(name, value).map_err(|_| anyhow::anyhow!("invalid HTTP header"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use super::*;

    #[test]
    fn pasted_apply_rejects_content_that_was_not_previewed() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("refs.bib");
        fs::write(&file, "@article{one, title={Old}}\n").unwrap();
        history::sync(&file, None, false, false, None).unwrap();
        let preview = preview_bibtex(
            &file,
            &BibtexRequest {
                key: "one".to_owned(),
                bibtex: "@book{x, title={Reviewed}}".to_owned(),
            },
        )
        .unwrap();
        let input = ApplyBibtexRequest {
            key: "one".to_owned(),
            bibtex: "@book{x, title={Not reviewed}}".to_owned(),
            reviewer: None,
            baseline_hash: preview.baseline_hash,
            proposal_hash: preview.proposal_hash,
        };
        assert!(apply_bibtex(&file, &input).is_err());
        assert!(fs::read_to_string(&file).unwrap().contains("Old"));
    }

    #[test]
    fn cached_browser_adoption_requires_the_exact_preview_and_baseline() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("refs.bib");
        fs::write(&file, "@article{one, title={Old}}\n").unwrap();
        history::sync(&file, None, false, false, None).unwrap();
        let expected = history::hydrate_file(&file).unwrap();
        let proposed = replace_entry(
            &expected,
            &parse("@article{one, title={Reviewed}}").unwrap()[0],
        )
        .unwrap();
        let drafts = BTreeMap::from([(
            "preview1".to_owned(),
            ReviewDraft {
                expected,
                proposed,
                key: "one".to_owned(),
                provider: "crossref".to_owned(),
                id: "record1".to_owned(),
            },
        )]);
        let input = AdoptRequest {
            key: "one".to_owned(),
            provider: "crossref".to_owned(),
            id: "record1".to_owned(),
            reviewer: None,
            preview_hash: "wrong-preview".to_owned(),
        };
        assert!(adopt_candidate(&file, &input, &drafts).is_err());
        let input = AdoptRequest {
            preview_hash: "preview1".to_owned(),
            ..input
        };
        fs::write(&file, "@article{one, title={External change}}\n").unwrap();
        history::sync(&file, None, false, false, None).unwrap();
        assert!(adopt_candidate(&file, &input, &drafts).is_err());
        assert!(
            fs::read_to_string(&file)
                .unwrap()
                .contains("External change")
        );
    }

    #[test]
    fn candidate_comparison_preserves_uncontrolled_bibtex_fields() {
        let current = BTreeMap::from([
            ("title".to_owned(), "Old".to_owned()),
            ("keywords".to_owned(), "memory".to_owned()),
        ]);
        let provider = BTreeMap::from([("title".to_owned(), "New".to_owned())]);
        let adopted = adopted_bibliographic_fields(&current, &provider);
        assert_eq!(adopted["title"], "New");
        assert_eq!(adopted["keywords"], "memory");
        let comparison = compare_fields(&current, &adopted);
        assert_eq!(
            comparison
                .iter()
                .find(|field| field.field == "keywords")
                .unwrap()
                .state,
            "unchanged"
        );
    }

    #[test]
    fn pasted_bibtex_replaces_the_record_and_adds_human_approval_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("references.bib");
        fs::write(
            &file,
            "@article{one, title={Old title}, author={Old Author}, year={2020}}\n",
        )
        .unwrap();
        history::sync(&file, None, false, false, None).unwrap();

        apply_bibtex(
            &file,
            &ApplyBibtexRequest {
                key: "one".to_owned(),
                bibtex: "@book{different, title={New title}, author={New Author}, year={2024}}"
                    .to_owned(),
                reviewer: Some("Researcher".to_owned()),
                baseline_hash: crate::integrity::hash(
                    &parse(&fs::read_to_string(&file).unwrap()).unwrap()[0],
                )
                .unwrap(),
                proposal_hash: crate::integrity::hash(
                    &pasted_record(
                        "@book{different, title={New title}, author={New Author}, year={2024}}",
                        "one",
                    )
                    .unwrap()
                    .1,
                )
                .unwrap(),
            },
        )
        .unwrap();

        let clean = fs::read_to_string(&file).unwrap();
        let records = parse(&clean).unwrap();
        assert_eq!(records[0].entry_key, "one");
        assert_eq!(records[0].entry_type, "book");
        assert_eq!(records[0].fields["title"], "New title");
        assert!(!clean.contains("bibapproval"));

        let hydrated = history::hydrate_file(&file).unwrap();
        let hydrated_records = parse(&hydrated).unwrap();
        assert_eq!(
            status(&hydrated_records[0], &hydrated_records).unwrap(),
            Status::Verified
        );

        let lock: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(history::default_path(&file)).unwrap())
                .unwrap();
        assert_eq!(lock["entries"]["one"]["approval"]["reviewer"], "Researcher");
        let revisions = lock["revisions"].as_object().unwrap();
        assert_eq!(revisions.len(), 1);
        assert_eq!(
            revisions.values().next().unwrap()["operation"],
            "human-paste"
        );
    }

    #[test]
    fn pasted_bibtex_requires_exactly_one_entry() {
        assert!(pasted_record("", "one").is_err());
        assert!(pasted_record("@article{a,title={A}} @article{b,title={B}}", "one").is_err());
    }
}
