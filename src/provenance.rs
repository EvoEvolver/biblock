use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::bibtex::{Record, render};
use crate::catalog::BibliographicQuery;
use crate::integrity::content_hash;
use crate::providers::{FetchedRecord, FetchedSearch};
use crate::resolver::{self, ResolutionCandidate};

pub const SOURCE_FIELD: &str = "bibsource";
pub const SOURCE_TYPE: &str = "bibsource";

pub const CONTROLLED_FIELDS: &[&str] = &[
    "title",
    "author",
    "editor",
    "journal",
    "booktitle",
    "publisher",
    "year",
    "month",
    "volume",
    "number",
    "pages",
    "eid",
    "doi",
    "url",
    "isbn",
    "issn",
    "bibprovider",
    "bibproviderid",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceKind {
    Provider,
    Agent,
    Human,
}

#[derive(Clone, Debug, Serialize)]
pub struct SourceSummary {
    pub key: Option<String>,
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<ResolutionSummary>,
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ResolutionSummary {
    pub key: String,
    pub input_url: Option<String>,
    pub method: Option<String>,
    pub identifier_kind: Option<String>,
    pub identifier: Option<String>,
    pub confidence: Option<String>,
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct EvidenceTrace {
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<EvidenceNode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web: Option<EvidenceNode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<EvidenceNode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<EvidenceNode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search: Option<EvidenceNode>,
}

#[derive(Clone, Debug, Serialize)]
pub struct EvidenceNode {
    pub key: String,
    pub fields: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct SearchCandidateReceipt<'a> {
    id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    authors: Vec<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    year: Option<i32>,
    projection_sha256: String,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Agent => "agent",
            Self::Human => "human",
        }
    }
}

pub fn provider_source(fetched: &FetchedRecord) -> Record {
    provider_source_with_resolution(fetched, None)
}

pub fn provider_source_with_resolution(
    fetched: &FetchedRecord,
    resolution_key: Option<&str>,
) -> Record {
    provider_source_with_evidence(fetched, resolution_key, None)
}

pub fn provider_source_with_evidence(
    fetched: &FetchedRecord,
    resolution_key: Option<&str>,
    selection_key: Option<&str>,
) -> Record {
    let response_sha256 = sha256(&fetched.response);
    let projection = "literature-record-v1";
    let projection_sha256 = projection_hash(
        fetched.record.bibtex_type(),
        &fetched.record.bibtex_fields(),
    );
    let tool_version = env!("CARGO_PKG_VERSION");
    let identity = provider_identity(ProviderIdentity {
        provider: &fetched.record.provider,
        provider_id: &fetched.record.id,
        request_url: &fetched.request_url,
        request_method: Some(&fetched.request_method),
        request_body_sha256: fetched.request_body_sha256.as_deref(),
        media_type: &fetched.media_type,
        projection,
        tool_version,
        response_sha256: &response_sha256,
        projection_sha256: Some(&projection_sha256),
        resolution_key,
        selection_key,
    });
    let key = format!("bibsource:provider:{identity}");
    let mut fields = BTreeMap::from([
        ("kind".to_owned(), "provider".to_owned()),
        ("provider".to_owned(), fetched.record.provider.clone()),
        ("providerid".to_owned(), fetched.record.id.clone()),
        ("requesturl".to_owned(), fetched.request_url.clone()),
        ("requestmethod".to_owned(), fetched.request_method.clone()),
        ("mediatype".to_owned(), fetched.media_type.clone()),
        ("projection".to_owned(), projection.to_owned()),
        ("projectionsha256".to_owned(), projection_sha256),
        ("responsesha256".to_owned(), response_sha256),
        ("toolversion".to_owned(), tool_version.to_owned()),
    ]);
    if let Some(request_body_sha256) = &fetched.request_body_sha256 {
        fields.insert("requestbodysha256".to_owned(), request_body_sha256.clone());
    }
    if let Some(resolution_key) = resolution_key {
        fields.insert("resolution".to_owned(), resolution_key.to_owned());
    }
    if let Some(selection_key) = selection_key {
        fields.insert("selection".to_owned(), selection_key.to_owned());
    }
    Record {
        entry_type: SOURCE_TYPE.to_owned(),
        entry_key: key,
        fields,
    }
}

pub fn search_source(
    target: &Record,
    query: &BibliographicQuery,
    search: &FetchedSearch,
    provider: &str,
) -> Result<Record> {
    let candidates = search
        .candidates
        .iter()
        .map(|candidate| SearchCandidateReceipt {
            id: &candidate.record.id,
            score: candidate.score,
            title: candidate.record.title.as_deref(),
            authors: candidate
                .record
                .authors
                .iter()
                .map(|author| author.family.as_str())
                .collect(),
            year: candidate.record.issued.as_ref().map(|date| date.year),
            projection_sha256: projection_hash(
                candidate.record.bibtex_type(),
                &candidate.record.bibtex_fields(),
            ),
        })
        .collect::<Vec<_>>();
    let mut fields = BTreeMap::from([
        ("kind".to_owned(), "search".to_owned()),
        ("provider".to_owned(), provider.to_owned()),
        ("target".to_owned(), target.entry_key.clone()),
        ("inputsha256".to_owned(), content_hash(target)?),
        ("query".to_owned(), query.citation.clone()),
        ("candidates".to_owned(), serde_json::to_string(&candidates)?),
        ("requesturl".to_owned(), search.request_url.clone()),
        ("mediatype".to_owned(), search.media_type.clone()),
        ("responsesha256".to_owned(), sha256(&search.response)),
        (
            "toolversion".to_owned(),
            env!("CARGO_PKG_VERSION").to_owned(),
        ),
    ]);
    let identity = evidence_identity(&fields);
    Ok(Record {
        entry_type: SOURCE_TYPE.to_owned(),
        entry_key: format!("bibsource:search:{identity}"),
        fields: std::mem::take(&mut fields),
    })
}

pub fn selection_source(search: &Record, selected_id: &str, selected_by: &str) -> Result<Record> {
    selection_source_with_match(search, selected_id, selected_by, "agent-review", None)
}

#[derive(Clone, Copy, Debug)]
pub struct MatchEvidence {
    pub score: f64,
    pub title_score: f64,
    pub author_score: f64,
    pub threshold: f64,
}

pub fn selection_source_with_match(
    search: &Record,
    selected_id: &str,
    selected_by: &str,
    method: &str,
    matched: Option<MatchEvidence>,
) -> Result<Record> {
    let selected_by = selected_by.trim();
    if selected_by.is_empty() {
        bail!("selection actor cannot be empty");
    }
    if !search_candidate_ids(required(search, "candidates")?)?
        .iter()
        .any(|candidate_id| candidate_id.eq_ignore_ascii_case(selected_id))
    {
        bail!("selected provider id was not present in the recorded search candidates");
    }
    let mut fields = BTreeMap::from([
        ("kind".to_owned(), "selection".to_owned()),
        ("search".to_owned(), search.entry_key.clone()),
        ("selectedid".to_owned(), selected_id.to_owned()),
        ("selectedby".to_owned(), selected_by.to_owned()),
        ("method".to_owned(), method.to_owned()),
        (
            "toolversion".to_owned(),
            env!("CARGO_PKG_VERSION").to_owned(),
        ),
    ]);
    if let Some(matched) = matched {
        fields.insert("matchscore".to_owned(), matched.score.to_string());
        fields.insert("titlescore".to_owned(), matched.title_score.to_string());
        fields.insert("authorscore".to_owned(), matched.author_score.to_string());
        fields.insert("threshold".to_owned(), matched.threshold.to_string());
        fields.insert("matchrule".to_owned(), "0.7*title+0.3*author".to_owned());
    }
    let identity = evidence_identity(&fields);
    Ok(Record {
        entry_type: SOURCE_TYPE.to_owned(),
        entry_key: format!("bibsource:selection:{identity}"),
        fields: std::mem::take(&mut fields),
    })
}

pub fn web_source(target: &Record, evidence: &resolver::WebEvidence) -> Result<Record> {
    let mut fields = BTreeMap::from([
        ("kind".to_owned(), "web".to_owned()),
        ("target".to_owned(), target.entry_key.clone()),
        ("contenthash".to_owned(), content_hash(target)?),
        ("inputurl".to_owned(), evidence.input_url.clone()),
        ("requesturl".to_owned(), evidence.request_url.clone()),
        ("finalurl".to_owned(), evidence.final_url.clone()),
        ("mediatype".to_owned(), evidence.media_type.clone()),
        (
            "responsesha256".to_owned(),
            evidence.response_sha256.clone(),
        ),
        (
            "responsebytes".to_owned(),
            evidence.response_bytes.to_string(),
        ),
        (
            "toolversion".to_owned(),
            env!("CARGO_PKG_VERSION").to_owned(),
        ),
    ]);
    let identity = evidence_identity(&fields);
    Ok(Record {
        entry_type: SOURCE_TYPE.to_owned(),
        entry_key: format!("bibsource:web:{identity}"),
        fields: std::mem::take(&mut fields),
    })
}

pub fn resolution_source(candidate: &ResolutionCandidate) -> Result<Record> {
    let evidence = &candidate.evidence;
    let mut fields = BTreeMap::from([
        ("kind".to_owned(), "resolution".to_owned()),
        ("method".to_owned(), evidence.method.clone()),
        ("inputurl".to_owned(), evidence.input_url.clone()),
        ("finalurl".to_owned(), evidence.final_url.clone()),
        ("identifierkind".to_owned(), candidate.kind.clone()),
        ("identifier".to_owned(), candidate.value.clone()),
        (
            "confidence".to_owned(),
            candidate.confidence.as_str().to_owned(),
        ),
        (
            "signals".to_owned(),
            serde_json::to_string(&candidate.signals)?,
        ),
        (
            "toolversion".to_owned(),
            env!("CARGO_PKG_VERSION").to_owned(),
        ),
    ]);
    if let Some(request_url) = &evidence.request_url {
        fields.insert("requesturl".to_owned(), request_url.clone());
    }
    if let Some(media_type) = &evidence.media_type {
        fields.insert("mediatype".to_owned(), media_type.clone());
    }
    if let Some(response_sha256) = &evidence.response_sha256 {
        if evidence.response_bytes == 0 {
            bail!("resolution response hash has no response byte count");
        }
        fields.insert("responsesha256".to_owned(), response_sha256.clone());
        fields.insert(
            "responsebytes".to_owned(),
            evidence.response_bytes.to_string(),
        );
    }
    let identity = resolution_identity(&fields);
    Ok(Record {
        entry_type: SOURCE_TYPE.to_owned(),
        entry_key: format!("bibsource:resolution:{identity}"),
        fields,
    })
}

pub fn actor_source(kind: SourceKind, actor: &str, target: &Record) -> Result<Record> {
    if !matches!(kind, SourceKind::Agent | SourceKind::Human) {
        bail!("actor provenance must be agent or human");
    }
    let actor = actor.trim();
    if actor.is_empty() {
        bail!("source actor cannot be empty");
    }
    let snapshot = content_hash(target)?;
    let identity = sha256(
        format!(
            "{}\0{actor}\0{}\0{snapshot}",
            kind.as_str(),
            target.entry_key
        )
        .as_bytes(),
    );
    Ok(Record {
        entry_type: SOURCE_TYPE.to_owned(),
        entry_key: format!("bibsource:{}:{identity}", kind.as_str()),
        fields: BTreeMap::from([
            ("kind".to_owned(), kind.as_str().to_owned()),
            ("actor".to_owned(), actor.to_owned()),
            ("target".to_owned(), target.entry_key.clone()),
            ("contenthash".to_owned(), snapshot),
        ]),
    })
}

pub fn append_source(source: &str, record: &Record, records: &[Record]) -> Result<String> {
    if let Some(existing) = records
        .iter()
        .find(|item| item.entry_key == record.entry_key)
    {
        if existing == record {
            return Ok(source.to_owned());
        }
        bail!("provenance key collision: {}", record.entry_key);
    }
    let separator = if source.ends_with('\n') { "\n" } else { "\n\n" };
    Ok(format!(
        "{source}{separator}{}",
        render(std::slice::from_ref(record))?
    ))
}

pub fn validate<'a>(record: &Record, records: &'a [Record]) -> Result<&'a str> {
    let source_key = record
        .fields
        .get(SOURCE_FIELD)
        .filter(|value| !value.trim().is_empty())
        .context("missing provenance source reference")?;
    let source = records
        .iter()
        .find(|candidate| candidate.is_provenance() && candidate.entry_key == *source_key)
        .with_context(|| format!("referenced provenance entry not found: {source_key}"))?;
    let kind = required(source, "kind")?;
    match kind {
        "provider" => validate_provider(record, source, records)?,
        "agent" => validate_actor(record, source, "agent")?,
        "human" => validate_actor(record, source, "human")?,
        "web" => validate_web(record, source)?,
        "resolution" | "search" | "selection" => {
            bail!("supporting evidence cannot directly verify a BibTeX entry")
        }
        other => bail!("unknown provenance kind {other:?}"),
    }
    Ok(kind)
}

fn validate_web(record: &Record, source: &Record) -> Result<()> {
    if source.entry_key != format!("bibsource:web:{}", evidence_identity(&source.fields)) {
        bail!("web provenance key does not match its metadata");
    }
    if required(source, "target")? != record.entry_key {
        bail!("web provenance target does not match citation key");
    }
    if required(source, "contenthash")? != content_hash(record)? {
        bail!("web provenance content snapshot no longer matches entry");
    }
    let entry_url = record
        .fields
        .get("url")
        .context("web-verified entry has no URL")?;
    if required(source, "inputurl")? != entry_url {
        bail!("web provenance URL no longer matches entry URL");
    }
    required(source, "requesturl")?;
    required(source, "finalurl")?;
    required(source, "mediatype")?;
    required_sha256(source, "responsesha256")?;
    if required(source, "responsebytes")?
        .parse::<usize>()
        .unwrap_or(0)
        == 0
    {
        bail!("web provenance has an invalid response byte count");
    }
    Ok(())
}

pub fn summary(record: &Record, records: &[Record]) -> SourceSummary {
    let key = record.fields.get(SOURCE_FIELD).cloned();
    let source = key.as_deref().and_then(|key| {
        records
            .iter()
            .find(|candidate| candidate.is_provenance() && candidate.entry_key == key)
    });
    let validation = validate(record, records);
    let resolution = source
        .and_then(|source| source.fields.get("resolution"))
        .map(|key| resolution_summary(key, records));
    SourceSummary {
        key,
        kind: source.and_then(|source| source.fields.get("kind").cloned()),
        actor: source.and_then(|source| source.fields.get("actor").cloned()),
        provider: source.and_then(|source| source.fields.get("provider").cloned()),
        provider_id: source.and_then(|source| source.fields.get("providerid").cloned()),
        resolution,
        valid: validation.is_ok(),
        error: validation.err().map(|error| format!("{error:#}")),
    }
}

pub fn trace(record: &Record, records: &[Record]) -> Result<EvidenceTrace> {
    validate(record, records)?;
    let source_key = record
        .fields
        .get(SOURCE_FIELD)
        .context("missing provenance source reference")?;
    let source = records
        .iter()
        .find(|candidate| candidate.is_provenance() && candidate.entry_key == *source_key)
        .with_context(|| format!("referenced provenance entry not found: {source_key}"))?;
    let source_kind = required(source, "kind")?;
    if source_kind == "web" {
        return Ok(EvidenceTrace {
            target: record.entry_key.clone(),
            provider: None,
            web: Some(EvidenceNode {
                key: source.entry_key.clone(),
                fields: public_evidence_fields(source),
            }),
            resolution: None,
            selection: None,
            search: None,
        });
    }
    if source_kind != "provider" {
        bail!("source trace is only available for provider or web provenance");
    }
    let resolution = source
        .fields
        .get("resolution")
        .map(|key| {
            let record = records
                .iter()
                .find(|candidate| candidate.is_provenance() && candidate.entry_key == *key)
                .with_context(|| format!("resolution evidence not found: {key}"))?;
            Ok::<EvidenceNode, anyhow::Error>(EvidenceNode {
                key: record.entry_key.clone(),
                fields: public_evidence_fields(record),
            })
        })
        .transpose()?;
    let selection = source
        .fields
        .get("selection")
        .map(|key| evidence_node(key, records, "selection"))
        .transpose()?;
    let search = selection
        .as_ref()
        .and_then(|selection| selection.fields.get("search"))
        .map(|key| evidence_node(key, records, "search"))
        .transpose()?;
    Ok(EvidenceTrace {
        target: record.entry_key.clone(),
        provider: Some(EvidenceNode {
            key: source.entry_key.clone(),
            fields: public_evidence_fields(source),
        }),
        web: None,
        resolution,
        selection,
        search,
    })
}

fn evidence_node(key: &str, records: &[Record], kind: &str) -> Result<EvidenceNode> {
    let record = records
        .iter()
        .find(|candidate| candidate.is_provenance() && candidate.entry_key == key)
        .with_context(|| format!("{kind} evidence not found: {key}"))?;
    Ok(EvidenceNode {
        key: record.entry_key.clone(),
        fields: public_evidence_fields(record),
    })
}

fn validate_provider(record: &Record, source: &Record, records: &[Record]) -> Result<()> {
    let provider = required(source, "provider")?;
    let provider_id = required(source, "providerid")?;
    if required(source, "projection")? != "literature-record-v1" {
        bail!("unsupported provider projection");
    }
    if let Some(method) = source.fields.get("requestmethod") {
        if !matches!(method.as_str(), "GET" | "POST") {
            bail!("unsupported provider request method");
        }
        if method == "POST" {
            required_sha256(source, "requestbodysha256")?;
        }
    }
    if source.fields.contains_key("requestbodysha256") {
        required_sha256(source, "requestbodysha256")?;
    }
    let expected_key = format!(
        "bibsource:provider:{}",
        provider_identity(ProviderIdentity {
            provider,
            provider_id,
            request_url: required(source, "requesturl")?,
            request_method: source.fields.get("requestmethod").map(String::as_str),
            request_body_sha256: source.fields.get("requestbodysha256").map(String::as_str),
            media_type: required(source, "mediatype")?,
            projection: required(source, "projection")?,
            tool_version: required(source, "toolversion")?,
            response_sha256: required_sha256(source, "responsesha256")?,
            projection_sha256: source.fields.get("projectionsha256").map(String::as_str),
            resolution_key: source.fields.get("resolution").map(String::as_str),
            selection_key: source.fields.get("selection").map(String::as_str),
        })
    );
    if source.entry_key != expected_key {
        bail!("provider provenance key does not match its metadata");
    }
    if let Some(resolution_key) = source.fields.get("resolution") {
        let resolution = records
            .iter()
            .find(|candidate| candidate.is_provenance() && candidate.entry_key == *resolution_key)
            .with_context(|| format!("resolution evidence not found: {resolution_key}"))?;
        validate_resolution(resolution)?;
        if required(resolution, "identifierkind")? == "doi"
            && !required(resolution, "identifier")?.eq_ignore_ascii_case(provider_id)
        {
            bail!("resolved DOI does not match provider record id");
        }
    }
    if let Some(selection_key) = source.fields.get("selection") {
        let selection = records
            .iter()
            .find(|candidate| candidate.is_provenance() && candidate.entry_key == *selection_key)
            .with_context(|| format!("selection evidence not found: {selection_key}"))?;
        validate_selection(selection, records)?;
        if !required(selection, "selectedid")?.eq_ignore_ascii_case(provider_id) {
            bail!("selected provider id does not match provider record id");
        }
    }
    if record.fields.get("bibprovider").map(String::as_str) != Some(provider)
        || record.fields.get("bibproviderid").map(String::as_str) != Some(provider_id)
    {
        bail!("BibTeX provider identity no longer matches provider receipt");
    }
    if source.fields.contains_key("projectionsha256") {
        let expected = required_sha256(source, "projectionsha256")?;
        let fields = CONTROLLED_FIELDS
            .iter()
            .filter_map(|field| {
                record
                    .fields
                    .get(*field)
                    .map(|value| ((*field).to_owned(), value.clone()))
            })
            .collect();
        if projection_hash(&record.entry_type, &fields) != *expected {
            bail!("BibTeX fields no longer match the recorded provider projection");
        }
    }
    Ok(())
}

fn validate_search(source: &Record) -> Result<()> {
    if required(source, "kind")? != "search" {
        bail!("referenced search evidence has the wrong kind");
    }
    if source.entry_key != format!("bibsource:search:{}", evidence_identity(&source.fields)) {
        bail!("search provenance key does not match its metadata");
    }
    required(source, "provider")?;
    required(source, "target")?;
    required_sha256(source, "inputsha256")?;
    required(source, "query")?;
    required(source, "requesturl")?;
    required(source, "mediatype")?;
    required_sha256(source, "responsesha256")?;
    if search_candidate_ids(required(source, "candidates")?)?.is_empty() {
        bail!("search evidence has no candidates");
    }
    Ok(())
}

fn validate_selection(source: &Record, records: &[Record]) -> Result<()> {
    if required(source, "kind")? != "selection" {
        bail!("referenced selection evidence has the wrong kind");
    }
    if source.entry_key != format!("bibsource:selection:{}", evidence_identity(&source.fields)) {
        bail!("selection provenance key does not match its metadata");
    }
    if !matches!(
        required(source, "method")?,
        "agent-review" | "browser-review" | "agent-proposal"
    ) {
        bail!("unsupported selection method");
    }
    required(source, "selectedby")?;
    let search_key = required(source, "search")?;
    let search = records
        .iter()
        .find(|candidate| candidate.is_provenance() && candidate.entry_key == search_key)
        .with_context(|| format!("search evidence not found: {search_key}"))?;
    validate_search(search)?;
    let selected_id = required(source, "selectedid")?;
    if !search_candidate_ids(required(search, "candidates")?)?
        .iter()
        .any(|candidate_id| candidate_id.eq_ignore_ascii_case(selected_id))
    {
        bail!("selected provider id is absent from search evidence");
    }
    let match_fields = [
        "matchscore",
        "titlescore",
        "authorscore",
        "threshold",
        "matchrule",
    ];
    let present = match_fields
        .iter()
        .filter(|field| source.fields.contains_key(**field))
        .count();
    if present != 0 && present != match_fields.len() {
        bail!("selection has incomplete match evidence");
    }
    if present == match_fields.len() {
        for field in &match_fields[..4] {
            let value: f64 = required(source, field)?
                .parse()
                .with_context(|| format!("selection {field} is not numeric"))?;
            if !(0.0..=1.0).contains(&value) {
                bail!("selection {field} is outside 0..1");
            }
        }
        if required(source, "matchrule")? != "0.7*title+0.3*author" {
            bail!("unsupported selection match rule");
        }
    }
    Ok(())
}

fn search_candidate_ids(value: &str) -> Result<Vec<String>> {
    let candidates: serde_json::Value =
        serde_json::from_str(value).context("search candidates are not valid JSON")?;
    let candidates = candidates
        .as_array()
        .context("search candidates must be a JSON array")?;
    candidates
        .iter()
        .map(|candidate| {
            candidate
                .get("id")
                .or_else(|| candidate.get("record").and_then(|record| record.get("id")))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .context("search candidate is missing its provider id")
        })
        .collect()
}

fn resolution_summary(key: &str, records: &[Record]) -> ResolutionSummary {
    let source = records
        .iter()
        .find(|candidate| candidate.is_provenance() && candidate.entry_key == key);
    let validation = source
        .context("resolution evidence entry not found")
        .and_then(validate_resolution);
    ResolutionSummary {
        key: key.to_owned(),
        input_url: source.and_then(|source| source.fields.get("inputurl").cloned()),
        method: source.and_then(|source| source.fields.get("method").cloned()),
        identifier_kind: source.and_then(|source| source.fields.get("identifierkind").cloned()),
        identifier: source.and_then(|source| source.fields.get("identifier").cloned()),
        confidence: source.and_then(|source| source.fields.get("confidence").cloned()),
        valid: validation.is_ok(),
        error: validation.err().map(|error| format!("{error:#}")),
    }
}

fn validate_resolution(source: &Record) -> Result<()> {
    if required(source, "kind")? != "resolution" {
        bail!("referenced resolution evidence has the wrong kind");
    }
    if source.entry_key
        != format!(
            "bibsource:resolution:{}",
            resolution_identity(&source.fields)
        )
    {
        bail!("resolution provenance key does not match its metadata");
    }
    if !matches!(required(source, "confidence")?, "exact" | "strong") {
        bail!("resolution evidence has an invalid confidence");
    }
    let expected = (
        required(source, "identifierkind")?.to_owned(),
        required(source, "identifier")?.to_ascii_lowercase(),
    );
    match required(source, "method")? {
        "url-doi" => {
            if !resolver::identifiers_in_url(required(source, "inputurl")?).contains(&expected) {
                bail!("resolution URL does not contain the recorded identifier");
            }
        }
        "arxiv-url" => {
            if required(source, "identifierkind")? != "arxiv"
                || resolver::arxiv_id_in_url(required(source, "inputurl")?).as_deref()
                    != Some(required(source, "identifier")?)
            {
                bail!("resolution URL does not contain the recorded arXiv identifier");
            }
        }
        "bibtex-field" => match required(source, "identifierkind")? {
            "doi" => {
                if !resolver::identifiers_in_url(required(source, "inputurl")?).contains(&expected)
                {
                    bail!("BibTeX field does not contain the recorded DOI");
                }
            }
            "arxiv" => {
                if resolver::arxiv_id_in_url(required(source, "inputurl")?).as_deref()
                    != Some(required(source, "identifier")?)
                {
                    bail!("BibTeX field does not contain the recorded arXiv identifier");
                }
            }
            other => bail!("unsupported BibTeX field identifier kind: {other}"),
        },
        "html-metadata" | "arxiv-atom" => {
            required(source, "requesturl")?;
            required(source, "finalurl")?;
            required(source, "mediatype")?;
            required_sha256(source, "responsesha256")?;
            if required(source, "responsebytes")?
                .parse::<usize>()
                .unwrap_or(0)
                == 0
            {
                bail!("resolution receipt has an invalid response byte count");
            }
        }
        other => bail!("unsupported resolution method: {other}"),
    }
    let signals: Vec<resolver::MatchSignal> = serde_json::from_str(required(source, "signals")?)
        .context("resolution signals are not valid JSON")?;
    if signals.is_empty()
        || signals.iter().any(|signal| {
            signal.value.to_ascii_lowercase() != expected.1 || signal.kind.trim().is_empty()
        })
    {
        bail!("resolution signals do not match the recorded identifier");
    }
    Ok(())
}

fn validate_actor(record: &Record, source: &Record, kind: &str) -> Result<()> {
    let actor = required(source, "actor")?;
    if required(source, "target")? != record.entry_key {
        bail!("{kind} provenance target does not match citation key");
    }
    if required(source, "contenthash")? != content_hash(record)? {
        bail!("{kind} provenance content snapshot no longer matches entry");
    }
    let expected_identity = sha256(
        format!(
            "{kind}\0{actor}\0{}\0{}",
            required(source, "target")?,
            required(source, "contenthash")?
        )
        .as_bytes(),
    );
    if source.entry_key != format!("bibsource:{kind}:{expected_identity}") {
        bail!("{kind} provenance key does not match its metadata");
    }
    Ok(())
}

struct ProviderIdentity<'a> {
    provider: &'a str,
    provider_id: &'a str,
    request_url: &'a str,
    request_method: Option<&'a str>,
    request_body_sha256: Option<&'a str>,
    media_type: &'a str,
    projection: &'a str,
    tool_version: &'a str,
    response_sha256: &'a str,
    projection_sha256: Option<&'a str>,
    resolution_key: Option<&'a str>,
    selection_key: Option<&'a str>,
}

fn provider_identity(value: ProviderIdentity<'_>) -> String {
    let ProviderIdentity {
        provider,
        provider_id,
        request_url,
        request_method,
        request_body_sha256,
        media_type,
        projection,
        tool_version,
        response_sha256,
        projection_sha256,
        resolution_key,
        selection_key,
    } = value;
    let mut identity = format!(
        "provider\0{provider}\0{provider_id}\0{request_url}\0{media_type}\0{projection}\0{tool_version}\0{response_sha256}"
    );
    if let Some(request_method) = request_method {
        identity.push_str("\0requestmethod\0");
        identity.push_str(request_method);
    }
    if let Some(request_body_sha256) = request_body_sha256 {
        identity.push_str("\0requestbodysha256\0");
        identity.push_str(request_body_sha256);
    }
    if let Some(projection_sha256) = projection_sha256 {
        identity.push_str("\0projectionsha256\0");
        identity.push_str(projection_sha256);
    }
    if let Some(resolution_key) = resolution_key {
        identity.push_str("\0resolution\0");
        identity.push_str(resolution_key);
    }
    if let Some(selection_key) = selection_key {
        identity.push_str("\0selection\0");
        identity.push_str(selection_key);
    }
    sha256(identity.as_bytes())
}

fn resolution_identity(fields: &BTreeMap<String, String>) -> String {
    evidence_identity(fields)
}

fn evidence_identity(fields: &BTreeMap<String, String>) -> String {
    let mut input = Vec::new();
    for (field, value) in fields {
        if matches!(
            field.as_str(),
            "response" | "responseencoding" | crate::integrity::PREVIOUS_FIELD
        ) {
            continue;
        }
        input.extend_from_slice(field.len().to_string().as_bytes());
        input.push(0);
        input.extend_from_slice(field.as_bytes());
        input.extend_from_slice(value.len().to_string().as_bytes());
        input.push(0);
        input.extend_from_slice(value.as_bytes());
    }
    sha256(&input)
}

pub(crate) fn projection_hash(entry_type: &str, fields: &BTreeMap<String, String>) -> String {
    let payload = (entry_type.to_ascii_lowercase(), fields);
    sha256(&serde_json::to_vec(&payload).expect("string projection is always serializable"))
}

fn public_evidence_fields(record: &Record) -> BTreeMap<String, String> {
    record
        .fields
        .iter()
        .filter(|(field, _)| {
            !matches!(
                field.as_str(),
                "response" | "responseencoding" | crate::integrity::PREVIOUS_FIELD
            )
        })
        .map(|(field, value)| (field.clone(), value.clone()))
        .collect()
}

fn required<'a>(record: &'a Record, field: &str) -> Result<&'a str> {
    record
        .fields
        .get(field)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("provenance entry {} is missing {field}", record.entry_key))
}

fn required_sha256<'a>(record: &'a Record, field: &str) -> Result<&'a str> {
    let value = required(record, field)?;
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("provenance entry {} has invalid {field}", record.entry_key);
    }
    Ok(value)
}

pub fn provenance_keys(records: &[Record]) -> BTreeSet<&str> {
    records
        .iter()
        .filter(|record| record.is_provenance())
        .map(|record| record.entry_key.as_str())
        .collect()
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_receipt_binds_url_and_entry_contents() {
        let mut target = Record {
            entry_type: "misc".to_owned(),
            entry_key: "product".to_owned(),
            fields: BTreeMap::from([
                ("title".to_owned(), "Product page".to_owned()),
                ("url".to_owned(), "https://example.com/product".to_owned()),
            ]),
        };
        let evidence = resolver::WebEvidence {
            input_url: "https://example.com/product".to_owned(),
            request_url: "https://example.com/product".to_owned(),
            final_url: "https://www.example.com/product".to_owned(),
            media_type: "text/html".to_owned(),
            response_sha256: sha256(b"response"),
            response_bytes: 8,
        };
        let source = web_source(&target, &evidence).unwrap();
        target
            .fields
            .insert(SOURCE_FIELD.to_owned(), source.entry_key.clone());
        let records = vec![target.clone(), source];

        validate(&target, &records).unwrap();
        let trace = trace(&target, &records).unwrap();
        assert!(trace.web.is_some());
        assert!(trace.provider.is_none());
        target
            .fields
            .insert("title".to_owned(), "Changed".to_owned());
        assert!(validate(&target, &records).is_err());
    }
}
