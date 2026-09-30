use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::bibtex::{Record, parse, render};
use crate::catalog::{PROVIDER_FIELD, PROVIDER_ID_FIELD};
use crate::integrity::{
    APPROVAL_BATCH_FIELD, APPROVAL_CONTENT_FIELD, APPROVAL_FIELDS, APPROVAL_ID_FIELD,
    APPROVAL_KIND_FIELD, APPROVAL_METHOD_FIELD, APPROVAL_REVIEWER_FIELD, APPROVAL_TIMESTAMP_FIELD,
    FIELD as INTEGRITY_FIELD, approval_id, atomic_write, hash, remove_entry_types, remove_fields,
    update_entry_fields,
};
use crate::provenance::{SOURCE_FIELD, SOURCE_TYPE};

const LOCKFILE_VERSION: &str = "1.1";
const WORKFLOW_FIELDS: &[&str] = &[
    INTEGRITY_FIELD,
    SOURCE_FIELD,
    PROVIDER_FIELD,
    PROVIDER_ID_FIELD,
    "bibprevious",
    APPROVAL_ID_FIELD,
    APPROVAL_KIND_FIELD,
    APPROVAL_METHOD_FIELD,
    APPROVAL_REVIEWER_FIELD,
    APPROVAL_CONTENT_FIELD,
    APPROVAL_TIMESTAMP_FIELD,
    APPROVAL_BATCH_FIELD,
];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalLock {
    pub id: String,
    pub kind: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
    pub target: String,
    pub content_hash: String,
    pub timestamp: u64,
    pub batch_id: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BibliographyLock {
    pub content_hash: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrityLock {
    pub content_hash: String,
    pub source: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryLock {
    pub content_hash: String,
    pub snapshot: Option<Record>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integrity: Option<IntegrityLock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalLock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Revision {
    pub target: String,
    pub previous: Option<String>,
    pub operation: String,
    pub actor: String,
    pub timestamp: u64,
    pub snapshot: Record,
    pub state: EntryLock,
    pub snapshot_sha256: String,
    pub revision_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LockFile {
    pub lockfile_version: String,
    pub tool_version: String,
    pub bibliography: BibliographyLock,
    pub entries: BTreeMap<String, EntryLock>,
    pub sources: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
    pub revisions: BTreeMap<String, Revision>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub deleted_entries: BTreeMap<String, EntryLock>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub proposals: BTreeMap<String, crate::proposal::Proposal>,
}

impl Default for LockFile {
    fn default() -> Self {
        Self {
            lockfile_version: LOCKFILE_VERSION.to_owned(),
            tool_version: env!("CARGO_PKG_VERSION").to_owned(),
            bibliography: BibliographyLock::default(),
            entries: BTreeMap::new(),
            sources: BTreeMap::new(),
            revisions: BTreeMap::new(),
            deleted_entries: BTreeMap::new(),
            proposals: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct HistoryStatus {
    pub valid: bool,
    pub state: &'static str,
    pub lockfile: String,
    pub revisions: usize,
    pub referenced: usize,
    pub orphaned: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RevisionView {
    pub id: String,
    pub target: String,
    pub previous: Option<String>,
    pub operation: String,
    pub actor: String,
    pub timestamp: u64,
    pub snapshot_sha256: String,
}

pub fn default_path(file: &Path) -> PathBuf {
    let name = file.file_name().unwrap_or_default().to_string_lossy();
    file.with_file_name(format!("{name}.lock"))
}

pub fn hydrate_file(file: &Path) -> Result<String> {
    hydrate_file_with_override(file, None)
}

pub fn clean_source(source: &str) -> Result<String> {
    let without_entries = remove_entry_types(source, &[SOURCE_TYPE, "bibversion"])?;
    remove_fields(&without_entries, WORKFLOW_FIELDS)
}

pub fn is_workflow_field(field: &str) -> bool {
    WORKFLOW_FIELDS.contains(&field)
}

pub fn preview(file: &Path, lock_override: Option<&Path>, proposed: &str) -> Result<LockFile> {
    let (mut lock, _) = read_lock(file, lock_override)?;
    import_embedded(proposed, &mut lock)?;
    Ok(capture(proposed, lock)?.1)
}

pub fn commit_edit(
    file: &Path,
    lock_override: Option<&Path>,
    expected: &str,
    proposed: &str,
    operation: &str,
    actor: Option<&str>,
) -> Result<bool> {
    commit_edit_inner(
        file,
        lock_override,
        expected,
        proposed,
        operation,
        actor,
        false,
        None,
    )
}

pub fn approve(file: &Path, keys: &BTreeSet<String>, reviewer: Option<&str>) -> Result<usize> {
    approve_bound(file, keys, reviewer, None)
}

pub fn approve_bound(
    file: &Path,
    keys: &BTreeSet<String>,
    reviewer: Option<&str>,
    hashes: Option<&BTreeMap<String, String>>,
) -> Result<usize> {
    if keys.is_empty() {
        return Ok(0);
    }
    let expected = hydrate_file(file)?;
    if let Some(hashes) = hashes {
        let records = parse(&expected)?;
        for key in keys {
            let current = records
                .iter()
                .find(|record| record.entry_key == *key)
                .context("entry not found")?;
            if hashes.get(key) != Some(&hash(current)?) {
                bail!("entry {key} changed since review; refresh and review it again");
            }
        }
    }
    let proposed = add_approvals(file, &expected, keys, reviewer)?;
    let actor = reviewer
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("anonymous-browser-review");
    commit_edit(
        file,
        None,
        &expected,
        &proposed,
        "human-approve",
        Some(actor),
    )?;
    Ok(keys.len())
}

pub fn commit_human_edit(
    file: &Path,
    expected: &str,
    proposed: &str,
    key: &str,
    reviewer: Option<&str>,
    operation: &str,
) -> Result<bool> {
    let keys = BTreeSet::from([key.to_owned()]);
    let approved = add_approvals(file, proposed, &keys, reviewer)?;
    let actor = reviewer
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("anonymous-browser-review");
    commit_edit(file, None, expected, &approved, operation, Some(actor))
}

pub(crate) fn commit_proposal(
    file: &Path,
    expected: &str,
    proposed: &str,
    id: &str,
    reviewer: Option<&str>,
    decision: crate::proposal::Decision,
) -> Result<bool> {
    let lock = read_required_lock(file, None)?;
    let key = &lock.proposals.get(id).context("proposal not found")?.target;
    let approved = add_approvals(file, proposed, &BTreeSet::from([key.clone()]), reviewer)?;
    commit_edit_inner(
        file,
        None,
        expected,
        &approved,
        "proposal-adopt",
        Some(
            reviewer
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("anonymous-browser-review"),
        ),
        false,
        Some((id, decision)),
    )
}

pub(crate) fn commit_provider_proposal(
    file: &Path,
    expected: &str,
    proposed: &str,
    id: &str,
    agent: &str,
    decision: crate::proposal::Decision,
) -> Result<bool> {
    commit_edit_inner(
        file,
        None,
        expected,
        proposed,
        "proposal-adopt",
        Some(agent),
        false,
        Some((id, decision)),
    )
}

pub(crate) fn update_lock(
    file: &Path,
    update: impl FnOnce(&mut LockFile) -> Result<()>,
) -> Result<()> {
    let report = status(file, None)?;
    if !report.valid {
        bail!(
            "lockfile is not current: {}; run `biblock lock FILE --sync` after reviewing external edits",
            report.errors.join("; ")
        );
    }
    let (mut lock, raw) = read_lock(file, None)?;
    update(&mut lock)?;
    lock.lockfile_version = LOCKFILE_VERSION.to_owned();
    validate_lock(&lock)?;
    let path = default_path(file);
    if fs::read_to_string(&path).ok() != raw || !status(file, None)?.valid {
        bail!("bibliography or lockfile changed; retry the command");
    }
    atomic_write(
        &path,
        &format!("{}\n", serde_json::to_string_pretty(&lock)?),
    )
}

fn add_approvals(
    file: &Path,
    source: &str,
    keys: &BTreeSet<String>,
    reviewer: Option<&str>,
) -> Result<String> {
    let records = parse(source)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs();
    let reviewer = reviewer.map(str::trim).filter(|value| !value.is_empty());
    let batch_id =
        sha256(serde_json::to_string(&(file.display().to_string(), timestamp, keys))?.as_bytes())
            [..16]
            .to_owned();
    let mut proposed = source.to_owned();
    for key in keys {
        let record = records
            .iter()
            .find(|record| !record.is_system() && record.entry_key == *key)
            .with_context(|| format!("citation key not found: {key}"))?;
        let approval = ApprovalLock {
            id: String::new(),
            kind: "human".to_owned(),
            method: "browser-review".to_owned(),
            reviewer: reviewer.map(str::to_owned),
            target: key.clone(),
            content_hash: hash(record)?,
            timestamp,
            batch_id: batch_id.clone(),
        };
        let approval = ApprovalLock {
            id: approval_id_for(&approval),
            ..approval
        };
        proposed = update_entry_fields(
            &proposed,
            key,
            &record.entry_type,
            &approval_fields(&approval),
        )?;
    }
    Ok(proposed)
}

pub fn sync(
    file: &Path,
    lock_override: Option<&Path>,
    frozen: bool,
    dry_run: bool,
    actor: Option<&str>,
) -> Result<HistoryStatus> {
    let report = status(file, lock_override)?;
    if frozen || report.state == "locked" || dry_run {
        return Ok(report);
    }
    let source = hydrate_file_with_override(file, lock_override)?;
    commit_edit_inner(
        file,
        lock_override,
        &source,
        &source,
        "lock-sync",
        actor,
        true,
        None,
    )?;
    status(file, lock_override)
}

#[allow(clippy::too_many_arguments)]
fn commit_edit_inner(
    file: &Path,
    lock_override: Option<&Path>,
    expected: &str,
    proposed: &str,
    operation: &str,
    actor: Option<&str>,
    allow_stale: bool,
    decision: Option<(&str, crate::proposal::Decision)>,
) -> Result<bool> {
    let lock_path = lock_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_path(file));
    if lock_path == file {
        bail!("lockfile must differ from the bibliography file");
    }
    let raw_main = fs::read_to_string(file)
        .with_context(|| format!("could not reread {} before writing", file.display()))?;
    let (mut old_lock, old_lock_raw) = read_lock(file, lock_override)?;
    let current_virtual = hydrate(&raw_main, &old_lock)?;
    if current_virtual != expected {
        bail!(
            "{} changed since it was read; retry the command",
            file.display()
        );
    }
    let current_clean = clean_source(&current_virtual)?;
    let current_document_hash = document_hash(&current_clean)?;
    if !allow_stale
        && old_lock_raw.is_some()
        && old_lock.bibliography.content_hash != current_document_hash
    {
        bail!(
            "{} is out of sync with {}; run `biblock lock {} --sync` first",
            lock_path.display(),
            file.display(),
            file.display()
        );
    }

    import_embedded(expected, &mut old_lock)?;
    let (clean, mut next_lock) = capture(proposed, old_lock.clone())?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs();
    let actor = actor.unwrap_or(concat!("biblock/", env!("CARGO_PKG_VERSION")));
    let keys: BTreeSet<_> = old_lock
        .entries
        .keys()
        .chain(next_lock.entries.keys())
        .cloned()
        .collect();
    let mut changed = false;
    for key in keys {
        let old = old_lock
            .entries
            .get(&key)
            .or_else(|| old_lock.deleted_entries.get(&key));
        let new = next_lock.entries.get(&key);
        if equivalent_state(old, new) && !old_lock.deleted_entries.contains_key(&key) {
            continue;
        }
        changed = true;
        if let Some(old) = old
            && let Some(snapshot) = old.snapshot.clone()
        {
            let (revision_id, revision) = make_revision(
                &key,
                snapshot,
                old.clone(),
                operation,
                actor,
                timestamp,
                &next_lock.revisions,
            )?;
            next_lock.revisions.insert(revision_id.clone(), revision);
            if let Some(entry) = next_lock.entries.get_mut(&key) {
                entry.head = Some(revision_id);
                next_lock.deleted_entries.remove(&key);
            } else {
                let mut deleted = old.clone();
                deleted.head = Some(revision_id);
                next_lock.deleted_entries.insert(key.clone(), deleted);
            }
        }
    }
    if old_lock.sources != next_lock.sources {
        changed = true;
    }
    if let Some((id, mut decision)) = decision {
        let proposal = next_lock
            .proposals
            .get_mut(id)
            .context("proposal not found")?;
        if proposal.decision.is_some() {
            bail!("proposal has already been decided");
        }
        let next_head = next_lock
            .entries
            .get(&proposal.target)
            .and_then(|entry| entry.head.clone());
        let previous_head = old_lock
            .entries
            .get(&proposal.target)
            .and_then(|entry| entry.head.clone());
        if next_head != previous_head {
            decision.revision = next_head;
        }
        proposal.decision = Some(decision);
        changed = true;
    }
    next_lock.lockfile_version = LOCKFILE_VERSION.to_owned();
    if !changed && raw_main == clean && old_lock_raw.is_some() {
        return Ok(false);
    }

    validate_lock(&next_lock)?;
    let lock_json = format!("{}\n", serde_json::to_string_pretty(&next_lock)?);
    let current_lock_raw = fs::read_to_string(&lock_path).ok();
    if current_lock_raw != old_lock_raw {
        bail!(
            "{} changed since it was read; retry the command",
            lock_path.display()
        );
    }

    // The lockfile is the commit marker. If the second write fails, its old
    // bibliography hash makes the interrupted update detectable as stale.
    if raw_main != clean {
        atomic_write(file, &clean)?;
    }
    atomic_write(&lock_path, &lock_json)?;
    Ok(true)
}

pub fn status(file: &Path, lock_override: Option<&Path>) -> Result<HistoryStatus> {
    let lock_path = lock_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_path(file));
    let source =
        fs::read_to_string(file).with_context(|| format!("could not read {}", file.display()))?;
    let (lock, raw) = read_lock(file, lock_override)?;
    if raw.is_none() {
        return Ok(HistoryStatus {
            valid: false,
            state: "unlocked",
            lockfile: lock_path.display().to_string(),
            revisions: 0,
            referenced: 0,
            orphaned: 0,
            errors: vec!["lockfile is missing".to_owned()],
        });
    }
    let clean = clean_source(&source)?;
    let mut errors = validation_errors(&lock);
    let structural_invalid = !errors.is_empty();
    if lock.bibliography.content_hash != document_hash(&clean)? {
        errors.push("bibliography content hash does not match".to_owned());
    }
    let records = parse(&clean)?;
    let by_key: BTreeMap<_, _> = records
        .iter()
        .filter(|record| !record.is_system())
        .map(|record| (record.entry_key.as_str(), record))
        .collect();
    for (key, entry) in &lock.entries {
        match by_key.get(key.as_str()) {
            Some(record) if hash(record).ok().as_ref() == Some(&entry.content_hash) => {}
            Some(_) => errors.push(format!("entry {key} content hash does not match")),
            None => errors.push(format!("entry {key} is missing from bibliography")),
        }
    }
    for key in by_key.keys() {
        if !lock.entries.contains_key(*key) {
            errors.push(format!("entry {key} is missing from lockfile"));
        }
    }
    errors.sort();
    errors.dedup();
    let referenced = referenced_revisions(&lock);
    let state = if errors.is_empty() {
        "locked"
    } else if structural_invalid {
        "invalid"
    } else {
        "stale"
    };
    Ok(HistoryStatus {
        valid: errors.is_empty(),
        state,
        lockfile: lock_path.display().to_string(),
        revisions: lock.revisions.len(),
        referenced: referenced.len(),
        orphaned: lock.revisions.len().saturating_sub(referenced.len()),
        errors,
    })
}

pub fn log(file: &Path, lock_override: Option<&Path>, key: &str) -> Result<Vec<RevisionView>> {
    let lock = read_required_lock(file, lock_override)?;
    let entry = lock
        .entries
        .get(key)
        .or_else(|| lock.deleted_entries.get(key))
        .with_context(|| format!("citation key not found: {key}"))?;
    let mut next = entry.head.as_deref();
    let mut seen = BTreeSet::new();
    let mut output = Vec::new();
    while let Some(id) = next {
        if !seen.insert(id) {
            bail!("history cycle at {id}");
        }
        let revision = lock
            .revisions
            .get(id)
            .with_context(|| format!("missing revision {id}"))?;
        if revision.target != key {
            bail!("revision {id} belongs to another target");
        }
        output.push(view(id, revision));
        next = revision.previous.as_deref();
    }
    Ok(output)
}

pub fn snapshot(file: &Path, lock_override: Option<&Path>, revision: &str) -> Result<Record> {
    Ok(
        resolve_revision(&read_required_lock(file, lock_override)?, revision)?
            .1
            .snapshot
            .clone(),
    )
}

pub fn revision_view(
    file: &Path,
    lock_override: Option<&Path>,
    revision: &str,
) -> Result<RevisionView> {
    let lock = read_required_lock(file, lock_override)?;
    let (id, revision) = resolve_revision(&lock, revision)?;
    Ok(view(id, revision))
}

pub fn restore_record(
    file: &Path,
    lock_override: Option<&Path>,
    revision: &str,
    source: &str,
) -> Result<String> {
    let lock = read_required_lock(file, lock_override)?;
    let (_, revision) = resolve_revision(&lock, revision)?;
    let mut replacement = revision.snapshot.clone();
    apply_state_fields(&mut replacement, &revision.state);
    if parse(source)?
        .iter()
        .any(|record| record.entry_key == replacement.entry_key)
    {
        crate::integrity::replace_entry(source, &replacement)
    } else {
        Ok(format!("{source}\n{}", render(&[replacement])?))
    }
}

fn hydrate_file_with_override(file: &Path, lock_override: Option<&Path>) -> Result<String> {
    let source =
        fs::read_to_string(file).with_context(|| format!("could not read {}", file.display()))?;
    let (lock, _) = read_lock(file, lock_override)?;
    hydrate(&source, &lock)
}

fn hydrate(source: &str, lock: &LockFile) -> Result<String> {
    let mut imported = lock.clone();
    import_embedded(source, &mut imported)?;
    let mut output = clean_source(source)?;
    let records = parse(&output)?;
    for record in records.iter().filter(|record| !record.is_system()) {
        if let Some(state) = imported.entries.get(&record.entry_key) {
            let mut fields = BTreeMap::new();
            if let Some(value) = &state.source {
                fields.insert(SOURCE_FIELD.to_owned(), value.clone());
            }
            if let Some(value) = &state.provider {
                fields.insert(PROVIDER_FIELD.to_owned(), value.clone());
            }
            if let Some(value) = &state.provider_id {
                fields.insert(PROVIDER_ID_FIELD.to_owned(), value.clone());
            }
            if let Some(value) = &state.integrity {
                let mut decorated = record.clone();
                decorated.fields.extend(fields.clone());
                let marker = if value.content_hash == hash(record)? {
                    hash(&decorated)?
                } else {
                    value.content_hash.clone()
                };
                fields.insert(INTEGRITY_FIELD.to_owned(), marker);
            }
            if let Some(approval) = &state.approval {
                fields.extend(approval_fields(approval));
            }
            if !fields.is_empty() {
                output =
                    update_entry_fields(&output, &record.entry_key, &record.entry_type, &fields)?;
            }
        }
    }
    for (id, fields) in &imported.sources {
        let separator = if output.ends_with('\n') { "\n" } else { "\n\n" };
        output.push_str(separator);
        output.push_str(&render(&[Record {
            entry_type: SOURCE_TYPE.to_owned(),
            entry_key: id.clone(),
            fields: decode_source_fields(fields)?,
        }])?);
    }
    Ok(output)
}

fn import_embedded(source: &str, lock: &mut LockFile) -> Result<()> {
    let records = parse(source)?;
    let clean = clean_source(source)?;
    let clean_records = parse(&clean)?;
    let clean_by_key: BTreeMap<_, _> = clean_records
        .iter()
        .map(|record| (record.entry_key.as_str(), record))
        .collect();
    for record in records.iter().filter(|record| record.is_provenance()) {
        lock.sources.insert(
            record.entry_key.clone(),
            encode_source_fields(&record.fields),
        );
    }
    for record in records.iter().filter(|record| !record.is_system()) {
        let clean_record = clean_by_key[record.entry_key.as_str()];
        let entry = lock.entries.entry(record.entry_key.clone()).or_default();
        if entry.content_hash.is_empty() {
            entry.content_hash = hash(clean_record)?;
            entry.snapshot = Some(clean_record.clone());
        }
        if let Some(value) = record.fields.get(SOURCE_FIELD) {
            entry.source = Some(value.clone());
        }
        if let Some(value) = record.fields.get(PROVIDER_FIELD) {
            entry.provider = Some(value.clone());
        }
        if let Some(value) = record.fields.get(PROVIDER_ID_FIELD) {
            entry.provider_id = Some(value.clone());
        }
        if let Some(marker) = record.fields.get(INTEGRITY_FIELD)
            && entry.integrity.is_none()
        {
            entry.integrity = Some(IntegrityLock {
                content_hash: if marker == &hash(record)? || marker == &legacy_hash(record)? {
                    hash(clean_record)?
                } else {
                    marker.clone()
                },
                source: record.fields.get(SOURCE_FIELD).cloned().unwrap_or_default(),
            });
        }
    }
    Ok(())
}

fn capture(source: &str, mut lock: LockFile) -> Result<(String, LockFile)> {
    let records = parse(source)?;
    for record in records.iter().filter(|record| record.is_provenance()) {
        lock.sources.insert(
            record.entry_key.clone(),
            encode_source_fields(&record.fields),
        );
    }
    let clean = clean_source(source)?;
    let clean_records = parse(&clean)?;
    let virtual_by_key: BTreeMap<_, _> = records
        .iter()
        .filter(|record| !record.is_system())
        .map(|record| (record.entry_key.as_str(), record))
        .collect();
    let mut entries = BTreeMap::new();
    for record in clean_records.iter().filter(|record| !record.is_system()) {
        let virtual_record = virtual_by_key[record.entry_key.as_str()];
        let old = lock
            .entries
            .get(&record.entry_key)
            .cloned()
            .unwrap_or_default();
        let marker = virtual_record.fields.get(INTEGRITY_FIELD);
        let expected_marker = hash(virtual_record)?;
        let integrity = match marker {
            Some(marker) if marker == &expected_marker => Some(IntegrityLock {
                content_hash: hash(record)?,
                source: virtual_record
                    .fields
                    .get(SOURCE_FIELD)
                    .cloned()
                    .unwrap_or_default(),
            }),
            Some(_) => old.integrity,
            None => None,
        };
        let approval = decode_approval(virtual_record)?;
        entries.insert(
            record.entry_key.clone(),
            EntryLock {
                content_hash: hash(record)?,
                snapshot: Some(record.clone()),
                source: virtual_record.fields.get(SOURCE_FIELD).cloned(),
                provider: virtual_record.fields.get(PROVIDER_FIELD).cloned(),
                provider_id: virtual_record.fields.get(PROVIDER_ID_FIELD).cloned(),
                integrity,
                approval,
                head: old.head,
            },
        );
    }
    lock.entries = entries;
    lock.bibliography.content_hash = document_hash(&clean)?;
    Ok((clean, lock))
}

fn approval_fields(approval: &ApprovalLock) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::from([
        (APPROVAL_ID_FIELD.to_owned(), approval.id.clone()),
        (APPROVAL_KIND_FIELD.to_owned(), approval.kind.clone()),
        (APPROVAL_METHOD_FIELD.to_owned(), approval.method.clone()),
        (
            APPROVAL_CONTENT_FIELD.to_owned(),
            approval.content_hash.clone(),
        ),
        (
            APPROVAL_TIMESTAMP_FIELD.to_owned(),
            approval.timestamp.to_string(),
        ),
        (APPROVAL_BATCH_FIELD.to_owned(), approval.batch_id.clone()),
    ]);
    if let Some(reviewer) = &approval.reviewer {
        fields.insert(APPROVAL_REVIEWER_FIELD.to_owned(), reviewer.clone());
    }
    fields
}

fn decode_approval(record: &Record) -> Result<Option<ApprovalLock>> {
    if !APPROVAL_FIELDS
        .iter()
        .any(|field| record.fields.contains_key(*field))
    {
        return Ok(None);
    }
    let required = |field: &str| {
        record
            .fields
            .get(field)
            .cloned()
            .with_context(|| format!("entry {} has incomplete approval", record.entry_key))
    };
    Ok(Some(ApprovalLock {
        id: required(APPROVAL_ID_FIELD)?,
        kind: required(APPROVAL_KIND_FIELD)?,
        method: required(APPROVAL_METHOD_FIELD)?,
        reviewer: record.fields.get(APPROVAL_REVIEWER_FIELD).cloned(),
        target: record.entry_key.clone(),
        content_hash: required(APPROVAL_CONTENT_FIELD)?,
        timestamp: required(APPROVAL_TIMESTAMP_FIELD)?
            .parse()
            .with_context(|| {
                format!("entry {} has invalid approval timestamp", record.entry_key)
            })?,
        batch_id: required(APPROVAL_BATCH_FIELD)?,
    }))
}

fn approval_id_for(approval: &ApprovalLock) -> String {
    approval_id(
        &approval.kind,
        &approval.method,
        approval.reviewer.as_deref(),
        &approval.target,
        &approval.content_hash,
        approval.timestamp,
        &approval.batch_id,
    )
}

fn apply_state_fields(record: &mut Record, state: &EntryLock) {
    let clean_hash = hash(record).ok();
    if let Some(value) = &state.source {
        record.fields.insert(SOURCE_FIELD.to_owned(), value.clone());
    }
    if let Some(value) = &state.provider {
        record
            .fields
            .insert(PROVIDER_FIELD.to_owned(), value.clone());
    }
    if let Some(value) = &state.provider_id {
        record
            .fields
            .insert(PROVIDER_ID_FIELD.to_owned(), value.clone());
    }
    if let Some(value) = &state.integrity {
        let marker = if clean_hash.as_ref() == Some(&value.content_hash) {
            hash(record).unwrap_or_else(|_| value.content_hash.clone())
        } else {
            value.content_hash.clone()
        };
        record.fields.insert(INTEGRITY_FIELD.to_owned(), marker);
    }
    if let Some(approval) = &state.approval {
        record.fields.extend(approval_fields(approval));
    }
}

fn equivalent_state(old: Option<&EntryLock>, new: Option<&EntryLock>) -> bool {
    match (old, new) {
        (None, None) => true,
        (Some(old), Some(new)) => {
            old.snapshot == new.snapshot
                && old.source == new.source
                && old.provider == new.provider
                && old.provider_id == new.provider_id
                && serde_json::to_value(&old.integrity).ok()
                    == serde_json::to_value(&new.integrity).ok()
                && old.approval == new.approval
        }
        _ => false,
    }
}

fn make_revision(
    target: &str,
    snapshot: Record,
    mut state: EntryLock,
    operation: &str,
    actor: &str,
    timestamp: u64,
    existing: &BTreeMap<String, Revision>,
) -> Result<(String, Revision)> {
    let previous = state.head.take();
    let snapshot_sha256 = sha256(&serde_json::to_vec(&(snapshot.clone(), state.clone()))?);
    let payload = (
        target,
        &previous,
        operation,
        actor,
        timestamp,
        &snapshot,
        &state,
        &snapshot_sha256,
    );
    let revision_sha256 = sha256(&serde_json::to_vec(&payload)?);
    let mut length = 8;
    let id = loop {
        let candidate = format!("rev:{}", &revision_sha256[..length]);
        if !existing.contains_key(&candidate) {
            break candidate;
        }
        length += 1;
        if length > revision_sha256.len() {
            bail!("could not allocate a unique revision id");
        }
    };
    Ok((
        id,
        Revision {
            target: target.to_owned(),
            previous,
            operation: operation.to_owned(),
            actor: actor.to_owned(),
            timestamp,
            snapshot,
            state,
            snapshot_sha256,
            revision_sha256,
        },
    ))
}

fn read_lock(file: &Path, lock_override: Option<&Path>) -> Result<(LockFile, Option<String>)> {
    let path = lock_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_path(file));
    match fs::read_to_string(&path) {
        Ok(source) => {
            let lock: LockFile = serde_json::from_str(&source)
                .with_context(|| format!("could not parse {} as JSON", path.display()))?;
            Ok((lock, Some(source)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok((LockFile::default(), None))
        }
        Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
    }
}

pub(crate) fn read_required_lock(file: &Path, lock_override: Option<&Path>) -> Result<LockFile> {
    let (lock, raw) = read_lock(file, lock_override)?;
    raw.context("lockfile is missing")?;
    validate_lock(&lock)?;
    Ok(lock)
}

fn validate_lock(lock: &LockFile) -> Result<()> {
    let errors = validation_errors(lock);
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("invalid lockfile: {}", errors.join("; "))
    }
}

fn validation_errors(lock: &LockFile) -> Vec<String> {
    let mut errors = Vec::new();
    if !matches!(lock.lockfile_version.as_str(), "1.0" | LOCKFILE_VERSION) {
        errors.push(format!(
            "unsupported lockfileVersion {}",
            lock.lockfile_version
        ));
    }
    if lock.tool_version.trim().is_empty() {
        errors.push("toolVersion is empty".to_owned());
    }
    for (key, entry) in lock.entries.iter().chain(&lock.deleted_entries) {
        if lock.entries.contains_key(key) && lock.deleted_entries.contains_key(key) {
            errors.push(format!("entry {key} is both active and deleted"));
        }
        if let Some(head) = &entry.head {
            match lock.revisions.get(head) {
                Some(revision) if revision.target == *key => {}
                Some(_) => errors.push(format!("entry {key} head belongs to another target")),
                None => errors.push(format!("entry {key} references missing revision {head}")),
            }
            let mut seen = BTreeSet::new();
            let mut next = Some(head.as_str());
            while let Some(id) = next {
                if !seen.insert(id) {
                    errors.push(format!("entry {key} history cycle at {id}"));
                    break;
                }
                next = lock
                    .revisions
                    .get(id)
                    .and_then(|revision| revision.previous.as_deref());
            }
        }
        if !is_sha256(&entry.content_hash) {
            errors.push(format!("entry {key} has invalid contentHash"));
        }
        if let Some(snapshot) = &entry.snapshot
            && (snapshot.entry_key != *key
                || hash(snapshot).ok().as_ref() != Some(&entry.content_hash))
        {
            errors.push(format!("entry {key} snapshot hash does not match"));
        }
        if let Some(source) = &entry.source
            && !lock.sources.contains_key(source)
        {
            errors.push(format!("entry {key} references missing source {source}"));
        }
        if let Some(integrity) = &entry.integrity {
            if !is_sha256(&integrity.content_hash) {
                errors.push(format!("entry {key} has invalid integrity contentHash"));
            }
            if entry.source.as_ref() != Some(&integrity.source) {
                errors.push(format!("entry {key} integrity source does not match"));
            }
        }
        if let Some(approval) = &entry.approval {
            if approval.kind != "human" || approval.method != "browser-review" {
                errors.push(format!("entry {key} has invalid approval type"));
            }
            if approval.target != *key || !is_sha256(&approval.content_hash) {
                errors.push(format!(
                    "entry {key} has invalid approval target or contentHash"
                ));
            }
            if approval.timestamp == 0 || approval.batch_id.trim().is_empty() {
                errors.push(format!("entry {key} has incomplete approval evidence"));
            }
            if approval.id != approval_id_for(approval) {
                errors.push(format!("entry {key} has invalid approval id"));
            }
        }
    }
    for (id, revision) in &lock.revisions {
        if revision.snapshot.entry_key != revision.target {
            errors.push(format!("revision {id} snapshot belongs to another target"));
        }
        if let Some(source) = &revision.state.source
            && !lock.sources.contains_key(source)
        {
            errors.push(format!("revision {id} references missing source {source}"));
        }
        let snapshot_hash = sha256(
            &serde_json::to_vec(&(revision.snapshot.clone(), revision.state.clone()))
                .unwrap_or_default(),
        );
        if snapshot_hash != revision.snapshot_sha256 {
            errors.push(format!("revision {id} snapshot hash does not match"));
        }
        let payload = (
            revision.target.as_str(),
            &revision.previous,
            revision.operation.as_str(),
            revision.actor.as_str(),
            revision.timestamp,
            &revision.snapshot,
            &revision.state,
            revision.snapshot_sha256.as_str(),
        );
        let full = sha256(&serde_json::to_vec(&payload).unwrap_or_default());
        if full != revision.revision_sha256
            || !id
                .strip_prefix("rev:")
                .is_some_and(|prefix| prefix.len() >= 8 && full.starts_with(prefix))
        {
            errors.push(format!("revision {id} hash does not match"));
        }
        if let Some(previous) = &revision.previous {
            match lock.revisions.get(previous) {
                Some(parent) if parent.target == revision.target => {}
                Some(_) => errors.push(format!(
                    "revision {id} links to another target through {previous}"
                )),
                None => errors.push(format!("revision {id} references missing {previous}")),
            }
        }
    }
    for (id, proposal) in &lock.proposals {
        if let Err(error) = proposal.validate(id) {
            errors.push(format!("proposal {id}: {error:#}"));
        }
        if let Some(revision_id) = proposal
            .decision
            .as_ref()
            .and_then(|decision| decision.revision.as_ref())
        {
            match lock.revisions.get(revision_id) {
                Some(revision)
                    if revision.target == proposal.target
                        && revision.snapshot == proposal.before => {}
                _ => errors.push(format!(
                    "proposal {id} decision references an invalid adoption revision"
                )),
            }
        }
    }
    errors.sort();
    errors.dedup();
    errors
}

fn referenced_revisions(lock: &LockFile) -> BTreeSet<&str> {
    let mut referenced = BTreeSet::new();
    for entry in lock.entries.values().chain(lock.deleted_entries.values()) {
        let mut next = entry.head.as_deref();
        while let Some(id) = next {
            if !referenced.insert(id) {
                break;
            }
            next = lock
                .revisions
                .get(id)
                .and_then(|revision| revision.previous.as_deref());
        }
    }
    referenced
}

fn resolve_revision<'a>(lock: &'a LockFile, revision: &str) -> Result<(&'a str, &'a Revision)> {
    if !revision.starts_with("rev:") && revision.len() < 8 {
        bail!("revision prefix must contain at least 8 hex digits");
    }
    let matches: Vec<_> = lock
        .revisions
        .iter()
        .filter(|(id, _)| {
            id.as_str() == revision
                || id
                    .strip_prefix("rev:")
                    .is_some_and(|value| value.starts_with(revision))
        })
        .collect();
    match matches.as_slice() {
        [(id, value)] => Ok((id.as_str(), value)),
        [] => bail!("revision not found: {revision}"),
        _ => bail!("revision is ambiguous: {revision}"),
    }
}

fn view(id: &str, revision: &Revision) -> RevisionView {
    RevisionView {
        id: id.to_owned(),
        target: revision.target.clone(),
        previous: revision.previous.clone(),
        operation: revision.operation.clone(),
        actor: revision.actor.clone(),
        timestamp: revision.timestamp,
        snapshot_sha256: revision.snapshot_sha256.clone(),
    }
}

fn document_hash(source: &str) -> Result<String> {
    let records: Vec<_> = parse(source)?
        .into_iter()
        .filter(|record| !record.is_system())
        .collect();
    Ok(sha256(&serde_json::to_vec(&records)?))
}

fn legacy_hash(record: &Record) -> Result<String> {
    let mut payload = BTreeMap::new();
    payload.insert("ENTRYTYPE".to_owned(), record.entry_type.clone());
    payload.extend(
        record
            .fields
            .iter()
            .filter(|(field, _)| {
                !field.eq_ignore_ascii_case(INTEGRITY_FIELD)
                    && !field.eq_ignore_ascii_case("bibprevious")
            })
            .map(|(field, value)| (field.to_ascii_lowercase(), value.clone())),
    );
    Ok(sha256(serde_json::to_string(&payload)?.as_bytes()))
}

fn encode_source_fields(fields: &BTreeMap<String, String>) -> BTreeMap<String, serde_json::Value> {
    fields
        .iter()
        .map(|(field, value)| {
            let encoded = if matches!(field.as_str(), "candidates" | "projection" | "signals") {
                serde_json::from_str(value)
                    .unwrap_or_else(|_| serde_json::Value::String(value.clone()))
            } else {
                serde_json::Value::String(value.clone())
            };
            (field.clone(), encoded)
        })
        .collect()
}

fn decode_source_fields(
    fields: &BTreeMap<String, serde_json::Value>,
) -> Result<BTreeMap<String, String>> {
    fields
        .iter()
        .map(|(field, value)| {
            let decoded = match value {
                serde_json::Value::String(value) => value.clone(),
                value => serde_json::to_string(value)?,
            };
            Ok((field.clone(), decoded))
        })
        .collect()
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
