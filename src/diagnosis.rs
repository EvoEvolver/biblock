use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::bibtex::{Record, parse};
use crate::history::{self, HistoryStatus};
use crate::integrity::{Status, status};
use crate::provenance;

#[derive(Serialize)]
pub struct Diagnosis {
    pub file: String,
    pub ready: bool,
    pub network_checked: bool,
    pub lock: HistoryStatus,
    pub summary: Summary,
    pub status_policy: BTreeMap<&'static str, &'static str>,
    pub pending: Vec<PendingEntry>,
    pub actions: Vec<Action>,
    pub proposals: Vec<crate::proposal::ProposalView>,
}

#[derive(Serialize)]
pub struct DirectoryDiagnosis {
    pub ready: bool,
    pub network_checked: bool,
    pub files: Vec<Diagnosis>,
    pub errors: BTreeMap<String, String>,
}

pub fn directory_report(directory: &Path) -> Result<DirectoryDiagnosis> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory).context("could not scan the current directory")? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("bib"))
        {
            paths.push(path);
        }
    }
    paths.sort();
    if paths.is_empty() {
        bail!(
            "no .bib files found in {}; specify a file with `biblock diagnosis FILE`",
            directory.display()
        );
    }
    let mut files = Vec::new();
    let mut errors = BTreeMap::new();
    for path in paths {
        match report(&path) {
            Ok(report) => files.push(report),
            Err(error) => {
                errors.insert(path.display().to_string(), format!("{error:#}"));
            }
        }
    }
    Ok(DirectoryDiagnosis {
        ready: errors.is_empty() && files.iter().all(|file| file.ready),
        network_checked: false,
        files,
        errors,
    })
}

#[derive(Default, Serialize)]
pub struct Summary {
    pub total: usize,
    pub verified: usize,
    pub valid: usize,
    pub stale: usize,
    pub invalid: usize,
}

#[derive(Serialize)]
pub struct PendingEntry {
    pub key: String,
    pub title: Option<String>,
    pub status: Status,
    pub reason: String,
    pub recommended_path: &'static str,
    pub commands: Vec<Action>,
    pub templates: Vec<Action>,
}

#[derive(Serialize)]
pub struct Action {
    pub purpose: String,
    pub command: String,
    pub argv: Vec<String>,
    pub writes: bool,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub placeholders: BTreeMap<&'static str, &'static str>,
}

fn action(purpose: &str, arguments: &[&str], writes: bool) -> Action {
    let argv: Vec<_> = std::iter::once("biblock")
        .chain(arguments.iter().copied())
        .map(str::to_owned)
        .collect();
    let command = argv
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    Action {
        purpose: purpose.to_owned(),
        command,
        argv,
        writes,
        placeholders: BTreeMap::new(),
    }
}

fn recommended_path(record: &Record) -> &'static str {
    let has = |field| {
        record
            .fields
            .get(field)
            .is_some_and(|value| !value.trim().is_empty())
    };
    if has("doi") || crate::resolver::identifier_from_record(record).is_some() {
        "api_verify"
    } else if has("howpublished") || matches!(record.entry_type.as_str(), "online" | "www") {
        "human_review"
    } else if has("title") && has("author") {
        "candidate_review"
    } else {
        "human_review"
    }
}

fn adoption_template(path: &str, key: &str, provider: &str) -> Action {
    let mut template = action(
        "Only after reading the candidate and confirming it is the same work, fill both placeholders and adopt; a match score alone is not approval",
        &[
            "source",
            "propose",
            path,
            "--key",
            key,
            "--provider",
            provider,
            "--id",
            "PROVIDER_RECORD_ID",
            "--agent",
            "AGENT_ID",
            "--min-score",
            "0.9",
        ],
        true,
    );
    template.placeholders = BTreeMap::from([
        (
            "PROVIDER_RECORD_ID",
            "Exact record.id from the candidate you read and selected",
        ),
        (
            "AGENT_ID",
            "Identity of the agent that reviewed and selected the candidate",
        ),
    ]);
    template
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:-".contains(c))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

pub fn report(file: &Path) -> Result<Diagnosis> {
    let raw =
        fs::read_to_string(file).with_context(|| format!("could not read {}", file.display()))?;
    // Validate the input independently so a broken lockfile still yields a diagnosis.
    parse(&raw)?;
    let lock = history::status(file, None).unwrap_or_else(|error| HistoryStatus {
        valid: false,
        state: "invalid",
        lockfile: history::default_path(file).display().to_string(),
        revisions: 0,
        referenced: 0,
        orphaned: 0,
        errors: vec![format!("{error:#}")],
    });
    let hydration = history::hydrate_file(file);
    let hydration_failed = hydration.is_err();
    let source = hydration.unwrap_or(raw);
    let records = parse(&source)?;
    let path = file.to_string_lossy();
    let proposals = crate::proposal::list(file).unwrap_or_default();
    let proposal_targets: std::collections::BTreeSet<_> = proposals
        .iter()
        .filter(|proposal| proposal.proposal.decision.is_none())
        .map(|proposal| proposal.proposal.target.as_str())
        .collect();
    let mut summary = Summary::default();
    let mut pending = Vec::new();
    for record in records.iter().filter(|record| !record.is_system()) {
        let entry_status = if hydration_failed {
            Status::Invalid
        } else {
            status(record, &records)?
        };
        summary.total += 1;
        match entry_status {
            Status::Verified => summary.verified += 1,
            Status::Valid => summary.valid += 1,
            Status::Stale => summary.stale += 1,
            Status::Invalid => summary.invalid += 1,
        }
        if entry_status == Status::Verified {
            continue;
        }
        let reason = match entry_status {
            Status::Valid => "Consistent evidence is backed by an agent or web receipt; API verification or explicit human approval is still required.".to_owned(),
            Status::Stale => "Content changed after integrity or human approval was recorded; inspect the edit history and verify the current content again.".to_owned(),
            _ if hydration_failed => "The lockfile cannot be loaded; verification evidence is unavailable.".to_owned(),
            _ => provenance::summary(record, &records).error
                .unwrap_or_else(|| "Integrity or approval evidence is missing or invalid.".to_owned()),
        };
        let key = record.entry_key.as_str();
        let eligible_proposal = proposals
            .iter()
            .find(|proposal| proposal.proposal.target == key && proposal.agent_adoptable);
        let recommended_path = if eligible_proposal.is_some() {
            "proposal_adopt"
        } else if proposal_targets.contains(key) {
            "proposal_review"
        } else {
            recommended_path(record)
        };
        let mut commands = Vec::new();
        let mut templates = Vec::new();
        if let Some(proposal) = eligible_proposal {
            commands.push(action(
                "Read the complete saved provider proposal before deciding",
                &["proposal", "list", &path],
                false,
            ));
            let mut template = action(
                "After reading and confirming the same work, adopt this frozen API-backed replacement",
                &[
                    "proposal",
                    "adopt",
                    &path,
                    "--id",
                    &proposal.id,
                    "--agent",
                    "AGENT_ID",
                    "--reviewed",
                ],
                true,
            );
            template
                .placeholders
                .insert("AGENT_ID", "Identity of the agent that read the proposal");
            templates.push(template);
        } else if recommended_path == "api_verify" {
            commands.push(action(
                "Preview exact API verification for this entry; inspect metadata changes and lookup errors",
                &["source", "verify", &path, "--key", key], false,
            ));
            commands.push(action(
                "After inspecting the preview, apply the exact API record and provider integrity; unresolved candidates are never adopted automatically",
                &["source", "verify", &path, "--key", key, "--in-place"], true,
            ));
        } else if recommended_path == "human_review" || recommended_path == "proposal_review" {
            commands.push(action(
                "Ask a human to inspect this entry and explicitly approve or paste corrections; agent or web receipts alone cannot make it verified",
                &["review", &path], true,
            ));
        }
        if matches!(recommended_path, "api_verify" | "candidate_review") {
            commands.push(action(
                "Inspect provider candidates and field changes; use if exact API lookup is unavailable or fails",
                &["source", "plan", &path, "--key", key], false,
            ));
        }
        if matches!(recommended_path, "api_verify" | "candidate_review")
            && record.fields.contains_key("title")
            && record.fields.contains_key("author")
        {
            commands.push(action(
                "Score title and author matches; read candidates before explicitly adopting",
                &["source", "match", &path, "--key", key, "--min-score", "0.9"],
                false,
            ));
            templates.push(adoption_template(&path, key, "crossref"));
            if record
                .fields
                .get("url")
                .is_some_and(|url| url.contains("openreview.net/"))
            {
                commands.push(action(
                    "Search OpenReview title and author candidates for explicit adoption",
                    &[
                        "source",
                        "match",
                        &path,
                        "--key",
                        key,
                        "--provider",
                        "openreview",
                        "--min-score",
                        "0.9",
                    ],
                    false,
                ));
                templates.push(adoption_template(&path, key, "openreview"));
            }
        }
        if record.fields.contains_key("bibsource") {
            commands.push(action(
                "Inspect the stored provenance chain",
                &["source", "trace", &path, "--key", key],
                false,
            ));
        }
        if entry_status == Status::Stale {
            commands.push(action(
                "Inspect recorded edits",
                &["history", "log", &path, "--key", key],
                false,
            ));
        }
        pending.push(PendingEntry {
            key: key.to_owned(),
            title: record.fields.get("title").cloned(),
            status: entry_status,
            reason,
            recommended_path,
            commands,
            templates,
        });
    }
    let mut actions = Vec::new();
    let pending_proposals = proposals
        .iter()
        .any(|proposal| proposal.proposal.decision.is_none());
    if pending_proposals {
        for proposal in proposals.iter().filter(|proposal| proposal.agent_adoptable) {
            let mut template = action(
                "Read this saved proposal and its provider evidence, then explicitly adopt without another API lookup",
                &[
                    "proposal",
                    "adopt",
                    &path,
                    "--id",
                    &proposal.id,
                    "--agent",
                    "AGENT_ID",
                    "--reviewed",
                ],
                true,
            );
            template.placeholders.insert(
                "AGENT_ID",
                "Identity of the agent that read and approved this provider replacement",
            );
            actions.push(template);
        }
        if proposals
            .iter()
            .any(|proposal| proposal.proposal.decision.is_none() && !proposal.agent_adoptable)
        {
            actions.push(action(
                "Compare agent proposals and explicitly adopt or reject them",
                &["review", &path],
                true,
            ));
        }
    }
    if !lock.valid {
        actions.push(action(
            "Inspect lockfile problems; syncing does not verify entries",
            &["lock", &path, "--frozen"],
            false,
        ));
        if lock.state != "invalid" {
            actions.push(action("After reviewing external edits, create or synchronize the lockfile and record them in history", &["lock", &path, "--sync"], true));
        }
    }
    if !pending.is_empty() {
        let mut verify_arguments = vec!["source", "verify", &path];
        for entry in pending
            .iter()
            .filter(|entry| matches!(entry.recommended_path, "api_verify" | "candidate_review"))
        {
            verify_arguments.extend(["--key", entry.key.as_str()]);
        }
        if verify_arguments.len() > 3 {
            actions.push(action(
                "Preview batch API verification; inspect the report before writing",
                &verify_arguments,
                false,
            ));
            verify_arguments.push("--in-place");
            actions.push(action("Apply exact API records and provider integrity to pending literature entries; ambiguous candidates still require explicit selection", &verify_arguments, true));
        }
        actions.push(action(
            "Let a human compare, paste corrections, or explicitly approve unresolved entries",
            &["review", &path],
            true,
        ));
        actions.push(action(
            "Save the selected provider candidate with source propose, read the frozen comparison, then use proposal adopt --agent AGENT_ID --reviewed if agentAdoptable is true; otherwise request human review",
            &["source", "propose", "--help"],
            false,
        ));
    }
    actions.push(action(
        "Check that every current entry is API-backed or explicitly human-approved",
        &["integrity", "status", &path],
        false,
    ));
    actions.push(action(
        "Recheck this complete local diagnosis",
        &["diagnosis", &path],
        false,
    ));
    Ok(Diagnosis {
        file: path.into_owned(),
        ready: lock.valid && summary.total == summary.verified && !pending_proposals,
        network_checked: false,
        lock,
        summary,
        status_policy: BTreeMap::from([
            (
                "verified",
                "Current content has consistent provider API evidence or explicit human approval.",
            ),
            (
                "valid",
                "Integrity and provenance are consistent, but agent or web evidence alone does not count as verified.",
            ),
            (
                "stale",
                "Content changed after integrity or approval was recorded.",
            ),
            (
                "invalid",
                "Integrity, approval, or provenance is missing, damaged, or inconsistent.",
            ),
        ]),
        pending,
        actions,
        proposals,
    })
}
