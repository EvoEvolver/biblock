use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::bibtex::{Record, parse};
use crate::history;
use crate::integrity::{hash, replace_entry};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub outcome: String,
    pub reviewer: Option<String>,
    pub timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Proposal {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderEvidence>,
    pub target: String,
    pub agent: String,
    pub reason: String,
    pub timestamp: u64,
    pub baseline_hash: String,
    pub proposal_hash: String,
    pub before: Record,
    pub after: Record,
    pub evidence: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<Decision>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposalView {
    pub match_score: Option<crate::dedupe::Similarity>,
    pub agent_adoptable: bool,
    pub agent_adoption_blocker: Option<String>,
    pub id: String,
    #[serde(flatten)]
    pub proposal: Proposal,
    pub stale: bool,
    pub comparison: Vec<crate::catalog::FieldChange>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderEvidence {
    pub record: crate::catalog::LiteratureRecord,
    pub candidates: Vec<crate::catalog::LiteratureRecord>,
    pub sources: Vec<Record>,
    pub threshold: f64,
}

impl Proposal {
    pub fn id(&self) -> Result<String> {
        let bytes = serde_json::to_vec(&(
            &self.target,
            &self.agent,
            &self.reason,
            self.timestamp,
            &self.baseline_hash,
            &self.proposal_hash,
            &self.before,
            &self.after,
            &self.evidence,
        ))?;
        let bytes = if let Some(provider) = &self.provider {
            serde_json::to_vec(&(bytes, provider))?
        } else {
            bytes
        };
        Ok(format!("proposal:{:x}", Sha256::digest(bytes)))
    }

    pub fn validate(&self, id: &str) -> Result<()> {
        if self.agent.trim().is_empty() || self.reason.trim().is_empty() || self.timestamp == 0 {
            bail!("agent, reason and timestamp are required");
        }
        if self.before.is_system()
            || self.after.is_system()
            || self.before.entry_key != self.target
            || self.after.entry_key != self.target
            || clean_record(&self.before)? != self.before
            || clean_record(&self.after)? != self.after
            || hash(&self.before)? != self.baseline_hash
            || hash(&self.after)? != self.proposal_hash
            || self.id()? != id
        {
            bail!("proposal content or identity does not match");
        }
        if let Some(decision) = &self.decision
            && (!matches!(decision.outcome.as_str(), "adopted" | "rejected")
                || decision.timestamp == 0)
        {
            bail!("invalid proposal decision");
        }
        if let Some(provider) = &self.provider {
            if !provider.threshold.is_finite()
                || !(0.0..=1.0).contains(&provider.threshold)
                || clean_record(&crate::catalog::proposed_record(
                    &self.before,
                    &provider.record,
                ))? != self.after
            {
                bail!("provider proposal does not match the frozen API projection");
            }
            let source = provider
                .sources
                .last()
                .context("missing provider receipt")?;
            let mut record = self.after.clone();
            record.fields.extend(
                provider
                    .record
                    .bibtex_fields()
                    .into_iter()
                    .filter(|(key, _)| history::is_workflow_field(key)),
            );
            record
                .fields
                .insert("bibsource".into(), source.entry_key.clone());
            crate::provenance::validate(&record, &provider.sources)?;
            let search = provider.sources.first().context("missing search receipt")?;
            if search.fields.get("inputsha256") != Some(&self.baseline_hash)
                || search.fields.get("target") != Some(&self.target)
                || search.fields.get("provider") != Some(&provider.record.provider)
            {
                bail!("provider search receipt does not match proposal baseline");
            }
            let candidates: serde_json::Value = serde_json::from_str(
                search
                    .fields
                    .get("candidates")
                    .context("missing candidates")?,
            )?;
            let candidates = candidates.as_array().context("invalid search candidates")?;
            if candidates.len() != provider.candidates.len() {
                bail!("search candidate count differs");
            }
            for (receipt, candidate) in candidates.iter().zip(&provider.candidates) {
                if candidate.provider != provider.record.provider
                    || receipt["id"].as_str() != Some(candidate.id.as_str())
                    || receipt["projection_sha256"].as_str()
                        != Some(
                            crate::provenance::projection_hash(
                                candidate.bibtex_type(),
                                &candidate.bibtex_fields(),
                            )
                            .as_str(),
                        )
                {
                    bail!("search candidate differs from its receipt");
                }
            }
        }
        Ok(())
    }
}

pub fn clean_record(record: &Record) -> Result<Record> {
    let clean = history::clean_source(&crate::bibtex::render(std::slice::from_ref(record))?)?;
    parse(&clean)?
        .into_iter()
        .next()
        .context("expected one bibliography entry")
}

pub fn replacement(input: &str, key: &str) -> Result<Record> {
    let records = parse(input)?;
    if records.len() != 1 || records[0].is_system() {
        bail!("proposal requires exactly one ordinary BibTeX entry");
    }
    let mut record = clean_record(&records[0])?;
    record.entry_key = key.to_owned();
    Ok(record)
}

pub fn create(
    file: &Path,
    key: &str,
    input: &str,
    agent: &str,
    reason: &str,
    evidence: serde_json::Value,
) -> Result<ProposalView> {
    let after = replacement(input, key)?;
    let records = parse(&fs::read_to_string(file)?)?;
    let before = clean_record(
        records
            .iter()
            .find(|record| record.entry_key == key && !record.is_system())
            .with_context(|| format!("citation key not found: {key}"))?,
    )?;
    let proposal = Proposal {
        provider: None,
        target: key.to_owned(),
        agent: agent.trim().to_owned(),
        reason: reason.trim().to_owned(),
        timestamp: now()?,
        baseline_hash: hash(&before)?,
        proposal_hash: hash(&after)?,
        before,
        after,
        evidence,
        decision: None,
    };
    let id = proposal.id()?;
    proposal.validate(&id)?;
    history::update_lock(file, |lock| {
        if lock.entries.get(key).map(|entry| &entry.content_hash) != Some(&proposal.baseline_hash) {
            bail!("entry changed while creating proposal");
        }
        lock.proposals.entry(id.clone()).or_insert(proposal.clone());
        Ok(())
    })?;
    Ok(view(id, proposal, false))
}

pub fn list(file: &Path) -> Result<Vec<ProposalView>> {
    let lock = history::read_required_lock(file, None)?;
    let records = parse(&fs::read_to_string(file)?)?;
    let current: BTreeMap<_, _> = records
        .iter()
        .filter(|record| !record.is_system())
        .map(|record| Ok((record.entry_key.clone(), hash(record)?)))
        .collect::<Result<_>>()?;
    Ok(lock
        .proposals
        .into_iter()
        .map(|(id, proposal)| {
            let stale = current.get(&proposal.target) != Some(&proposal.baseline_hash);
            view(id, proposal, stale)
        })
        .collect())
}

fn view(id: String, proposal: Proposal, stale: bool) -> ProposalView {
    let comparison = crate::catalog::record_changes(&proposal.before, &proposal.after);
    let blocker = adoption_blocker(&proposal)
        .or_else(|| stale.then(|| "proposal baseline is stale".to_owned()));
    ProposalView {
        match_score: proposal.provider.as_ref().and_then(|provider| {
            crate::dedupe::literature_similarity(&proposal.before, &provider.record)
        }),
        agent_adoptable: blocker.is_none() && proposal.decision.is_none(),
        agent_adoption_blocker: blocker,
        id,
        proposal,
        stale,
        comparison,
    }
}

pub fn decide(file: &Path, id: &str, adopt: bool, reviewer: Option<&str>) -> Result<()> {
    let lock = history::read_required_lock(file, None)?;
    let proposal = lock.proposals.get(id).context("proposal not found")?;
    if proposal.decision.is_some() {
        bail!("proposal has already been decided");
    }
    let decision = Decision {
        agent: None,
        outcome: if adopt { "adopted" } else { "rejected" }.to_owned(),
        reviewer: reviewer
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        timestamp: now()?,
        revision: None,
    };
    if adopt {
        let expected = history::hydrate_file(file)?;
        let records = parse(&expected)?;
        let current = records
            .iter()
            .find(|record| record.entry_key == proposal.target)
            .context("proposal target no longer exists")?;
        if hash(current)? != proposal.baseline_hash {
            bail!(
                "proposal is stale: current entry differs from the reviewed baseline; create a new proposal"
            );
        }
        let proposed = prepared(&expected, proposal)?;
        history::commit_proposal(file, &expected, &proposed, id, reviewer, decision)?;
    } else {
        history::update_lock(file, |lock| {
            lock.proposals
                .get_mut(id)
                .context("proposal not found")?
                .decision = Some(decision);
            Ok(())
        })?;
    }
    Ok(())
}

fn adoption_blocker(proposal: &Proposal) -> Option<String> {
    let Some(provider) = &proposal.provider else {
        return Some("no provider support; human approval required".into());
    };
    let score = crate::dedupe::literature_similarity(&proposal.before, &provider.record);
    if !score.is_some_and(|score| score.score >= provider.threshold) {
        return Some(
            "exact provider record is below the title/author threshold or cannot be scored".into(),
        );
    }
    if !provider.candidates.iter().any(|candidate| {
        candidate.id.eq_ignore_ascii_case(&provider.record.id)
            && crate::dedupe::literature_similarity(&proposal.before, candidate)
                .is_some_and(|score| score.score >= provider.threshold)
    }) {
        return Some("selected search candidate is below the title/author threshold".into());
    }
    if provider.candidates.iter().any(|candidate| {
        !candidate.id.eq_ignore_ascii_case(&provider.record.id)
            && crate::dedupe::literature_similarity(&proposal.before, candidate)
                .is_some_and(|score| score.score >= provider.threshold)
    }) {
        return Some(
            "multiple title-search candidates meet the threshold; human review required".into(),
        );
    }
    None
}

fn prepared(expected: &str, proposal: &Proposal) -> Result<String> {
    let mut record = proposal.after.clone();
    if let Some(provider) = &proposal.provider {
        record.fields.extend(
            provider
                .record
                .bibtex_fields()
                .into_iter()
                .filter(|(key, _)| history::is_workflow_field(key)),
        );
        record.fields.insert(
            "bibsource".into(),
            provider
                .sources
                .last()
                .context("missing provider receipt")?
                .entry_key
                .clone(),
        );
    }
    let mut output = replace_entry(expected, &record)?;
    if let Some(provider) = &proposal.provider {
        for source in &provider.sources {
            output = crate::provenance::append_source(&output, source, &parse(&output)?)?;
        }
        output = crate::integrity::update_source(
            &output,
            &parse(&output)?,
            &std::collections::BTreeSet::from([proposal.target.clone()]),
            false,
        )?;
    }
    Ok(output)
}

pub fn adopt(file: &Path, id: &str, agent: &str, reviewed: bool) -> Result<()> {
    if !reviewed || agent.trim().is_empty() {
        bail!("read the proposal diff first, then supply --reviewed and a nonempty --agent");
    }
    let lock = history::read_required_lock(file, None)?;
    let proposal = lock.proposals.get(id).context("proposal not found")?;
    if proposal.decision.is_some() {
        bail!("proposal has already been decided");
    }
    if let Some(blocker) = adoption_blocker(proposal) {
        bail!("{blocker}");
    }
    let expected = history::hydrate_file(file)?;
    let current = parse(&expected)?
        .into_iter()
        .find(|record| record.entry_key == proposal.target)
        .context("proposal target no longer exists")?;
    if hash(&current)? != proposal.baseline_hash {
        bail!("proposal baseline is stale; create a new proposal");
    }
    let proposed = prepared(&expected, proposal)?;
    history::commit_provider_proposal(
        file,
        &expected,
        &proposed,
        id,
        agent.trim(),
        Decision {
            outcome: "adopted".into(),
            agent: Some(agent.trim().into()),
            reviewer: None,
            timestamp: now()?,
            revision: None,
        },
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn create_provider(
    file: &Path,
    key: &str,
    provider_name: &str,
    id: &str,
    agent: &str,
    reason: &str,
    threshold: f64,
    limit: usize,
    mailto: Option<&str>,
) -> Result<ProposalView> {
    if !history::status(file, None)?.valid {
        bail!("lockfile is not current; inspect diagnosis before proposing");
    }
    let lock = history::read_required_lock(file, None)?;
    let before = lock
        .entries
        .get(key)
        .context("citation key not found")?
        .snapshot
        .clone();
    let before = clean_record(
        before
            .as_ref()
            .context("missing entry snapshot; synchronize lockfile first")?,
    )?;
    let backend = crate::providers::open(provider_name, mailto)?;
    let query = crate::catalog::title_search_query(&before);
    let search = backend.search(&query, limit)?;
    let fetched = backend.lookup(&crate::catalog::LiteratureIdentifier::ProviderId(id.into()))?;
    if !fetched.record.id.eq_ignore_ascii_case(id) {
        bail!("provider returned a different identifier");
    }
    let search_source = crate::provenance::search_source(&before, &query, &search, provider_name)?;
    let selection = crate::provenance::selection_source_with_match(
        &search_source,
        &fetched.record.id,
        agent,
        "agent-proposal",
        None,
    )?;
    let receipt = crate::provenance::provider_source_with_evidence(
        &fetched,
        None,
        Some(&selection.entry_key),
    );
    let after = clean_record(&crate::catalog::proposed_record(&before, &fetched.record))?;
    let proposal = Proposal {
        target: key.into(),
        agent: agent.trim().into(),
        reason: reason.trim().into(),
        timestamp: now()?,
        baseline_hash: hash(&before)?,
        proposal_hash: hash(&after)?,
        before,
        after,
        evidence: serde_json::Value::Null,
        decision: None,
        provider: Some(ProviderEvidence {
            record: fetched.record,
            candidates: search
                .candidates
                .into_iter()
                .map(|candidate| candidate.record)
                .collect(),
            sources: vec![search_source, selection, receipt],
            threshold,
        }),
    };
    let id = proposal.id()?;
    proposal.validate(&id)?;
    history::update_lock(file, |lock| {
        if lock.entries.get(key).map(|entry| &entry.content_hash) != Some(&proposal.baseline_hash) {
            bail!("entry changed while querying provider");
        }
        lock.proposals.insert(id.clone(), proposal.clone());
        Ok(())
    })?;
    Ok(view(id, proposal, false))
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}
