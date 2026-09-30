use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::bibtex::Record;
use crate::provenance;

pub const FIELD: &str = "integrity";
pub const PREVIOUS_FIELD: &str = "bibprevious";
pub const APPROVAL_ID_FIELD: &str = "bibapprovalid";
pub const APPROVAL_KIND_FIELD: &str = "bibapprovalkind";
pub const APPROVAL_METHOD_FIELD: &str = "bibapprovalmethod";
pub const APPROVAL_REVIEWER_FIELD: &str = "bibapprovalreviewer";
pub const APPROVAL_CONTENT_FIELD: &str = "bibapprovalcontent";
pub const APPROVAL_TIMESTAMP_FIELD: &str = "bibapprovaltimestamp";
pub const APPROVAL_BATCH_FIELD: &str = "bibapprovalbatch";
pub const APPROVAL_FIELDS: &[&str] = &[
    APPROVAL_ID_FIELD,
    APPROVAL_KIND_FIELD,
    APPROVAL_METHOD_FIELD,
    APPROVAL_REVIEWER_FIELD,
    APPROVAL_CONTENT_FIELD,
    APPROVAL_TIMESTAMP_FIELD,
    APPROVAL_BATCH_FIELD,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Verified,
    Valid,
    Stale,
    Invalid,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Verified => f.write_str("verified"),
            Self::Valid => f.write_str("valid"),
            Self::Stale => f.write_str("stale"),
            Self::Invalid => f.write_str("invalid"),
        }
    }
}

pub fn canonical_json(record: &Record) -> Result<String> {
    let mut payload = BTreeMap::new();
    payload.insert("ENTRYTYPE".to_owned(), record.entry_type.clone());
    payload.extend(
        record
            .fields
            .iter()
            .filter(|(key, _)| {
                !matches!(
                    key.to_ascii_lowercase().as_str(),
                    FIELD
                        | PREVIOUS_FIELD
                        | "bibsource"
                        | "bibprovider"
                        | "bibproviderid"
                        | APPROVAL_ID_FIELD
                        | APPROVAL_KIND_FIELD
                        | APPROVAL_METHOD_FIELD
                        | APPROVAL_REVIEWER_FIELD
                        | APPROVAL_CONTENT_FIELD
                        | APPROVAL_TIMESTAMP_FIELD
                        | APPROVAL_BATCH_FIELD
                )
            })
            .map(|(key, value)| (key.to_ascii_lowercase(), value.clone())),
    );
    serde_json::to_string(&payload).context("could not serialize integrity payload")
}

pub fn hash(record: &Record) -> Result<String> {
    if record.is_system() {
        bail!(
            "{} entries do not participate in integrity",
            record.entry_type
        );
    }
    let digest = Sha256::digest(canonical_json(record)?.as_bytes());
    Ok(format!("{digest:x}"))
}

pub fn approval_id(
    kind: &str,
    method: &str,
    reviewer: Option<&str>,
    target: &str,
    content_hash: &str,
    timestamp: u64,
    batch_id: &str,
) -> String {
    let payload = serde_json::to_vec(&(
        kind,
        method,
        reviewer,
        target,
        content_hash,
        timestamp,
        batch_id,
    ))
    .unwrap_or_default();
    format!("{:x}", Sha256::digest(payload))
}

pub fn content_hash(record: &Record) -> Result<String> {
    let mut snapshot = record.clone();
    snapshot.fields.remove(FIELD);
    snapshot.fields.remove(provenance::SOURCE_FIELD);
    hash(&snapshot)
}

pub fn status(record: &Record, records: &[Record]) -> Result<Status> {
    let approval_values = APPROVAL_FIELDS
        .iter()
        .filter_map(|field| record.fields.get(*field))
        .count();
    if approval_values > 0 {
        let required = |field| record.fields.get(field).map(String::as_str);
        let timestamp = required(APPROVAL_TIMESTAMP_FIELD).and_then(|value| value.parse().ok());
        if required(APPROVAL_KIND_FIELD) != Some("human")
            || required(APPROVAL_METHOD_FIELD) != Some("browser-review")
            || required(APPROVAL_ID_FIELD).is_none()
            || required(APPROVAL_CONTENT_FIELD).is_none()
            || timestamp.is_none()
            || required(APPROVAL_BATCH_FIELD).is_none()
        {
            return Ok(Status::Invalid);
        }
        let expected_id = approval_id(
            "human",
            "browser-review",
            required(APPROVAL_REVIEWER_FIELD),
            &record.entry_key,
            required(APPROVAL_CONTENT_FIELD).unwrap_or_default(),
            timestamp.unwrap_or_default(),
            required(APPROVAL_BATCH_FIELD).unwrap_or_default(),
        );
        if required(APPROVAL_ID_FIELD) != Some(expected_id.as_str()) {
            return Ok(Status::Invalid);
        }
        if record.fields.get(APPROVAL_CONTENT_FIELD) == Some(&hash(record)?) {
            return Ok(Status::Verified);
        }
        return Ok(Status::Stale);
    }
    match record.fields.get(FIELD).map(|value| value.trim()) {
        None | Some("") => Ok(Status::Invalid),
        Some(stored) if stored == hash(record)? => {
            Ok(match provenance::validate(record, records) {
                Ok("provider" | "human") => Status::Verified,
                Ok(_) => Status::Valid,
                Err(_) => Status::Invalid,
            })
        }
        Some(_) => Ok(Status::Stale),
    }
}

pub fn update_source(
    source: &str,
    records: &[Record],
    selected: &BTreeSet<String>,
    remove: bool,
) -> Result<String> {
    let spans = scan_entries(source)?;
    let by_key: BTreeMap<_, _> = records
        .iter()
        .map(|record| (record.entry_key.as_str(), record))
        .collect();
    let present: BTreeSet<_> = spans.iter().map(|entry| entry.key.as_str()).collect();

    for key in selected {
        if !present.contains(key.as_str()) {
            bail!("citation key not found: {key}");
        }
    }

    let mut edits = Vec::new();
    for entry in spans.iter().filter(|entry| selected.contains(&entry.key)) {
        let record = by_key
            .get(entry.key.as_str())
            .with_context(|| format!("citation key was not parsed: {}", entry.key))?;
        let integrity_fields: Vec<_> = entry
            .fields
            .iter()
            .filter(|field| field.name.eq_ignore_ascii_case(FIELD))
            .collect();
        if integrity_fields.len() > 1 {
            bail!("entry {} has multiple integrity fields", entry.key);
        }

        if remove {
            if let Some(field) = integrity_fields.first() {
                edits.push(Edit {
                    start: field.segment_start,
                    end: field.remove_end,
                    replacement: String::new(),
                });
            }
        } else {
            provenance::validate(record, records)
                .with_context(|| format!("entry {} has invalid provenance", entry.key))?;
            let value = format!("integrity = {{{}}}", hash(record)?);
            if let Some(field) = integrity_fields.first() {
                edits.push(Edit {
                    start: field.trimmed_start,
                    end: field.trimmed_end,
                    replacement: value,
                });
            } else {
                edits.push(insertion_edit(
                    source,
                    entry,
                    value,
                    !entry.fields.is_empty(),
                ));
            }
        }
    }

    edits.sort_by_key(|edit| std::cmp::Reverse(edit.start));
    let mut output = source.to_owned();
    for edit in edits {
        output.replace_range(edit.start..edit.end, &edit.replacement);
    }
    Ok(output)
}

pub fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("could not create temporary file in {}", parent.display()))?;
    if let Ok(metadata) = fs::metadata(path) {
        temp.as_file_mut()
            .set_permissions(metadata.permissions())
            .context("could not preserve file permissions")?;
    }
    temp.write_all(contents.as_bytes())
        .context("could not write temporary file")?;
    temp.as_file_mut()
        .sync_all()
        .context("could not sync temporary file")?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("could not replace {}", path.display()))?;
    Ok(())
}

pub fn replace_entry(source: &str, replacement: &Record) -> Result<String> {
    let spans = scan_entries(source)?;
    let entry = spans
        .iter()
        .find(|entry| entry.key == replacement.entry_key)
        .with_context(|| format!("citation key not found: {}", replacement.entry_key))?;
    let mut output = source.to_owned();
    output.replace_range(
        entry.type_start.saturating_sub(1)..=entry.close,
        &crate::bibtex::render(std::slice::from_ref(replacement))?,
    );
    Ok(output)
}

pub fn atomic_write_if_unchanged(path: &Path, expected: &str, contents: &str) -> Result<()> {
    let current = fs::read_to_string(path)
        .with_context(|| format!("could not reread {} before writing", path.display()))?;
    if current != expected {
        bail!(
            "{} changed since it was read; retry the command",
            path.display()
        );
    }
    atomic_write(path, contents)
}

#[derive(Debug)]
struct EntrySpan {
    key: String,
    type_start: usize,
    type_end: usize,
    open: usize,
    close: usize,
    fields: Vec<FieldSpan>,
}

#[derive(Debug)]
struct FieldSpan {
    name: String,
    segment_start: usize,
    remove_end: usize,
    trimmed_start: usize,
    trimmed_end: usize,
}

struct Edit {
    start: usize,
    end: usize,
    replacement: String,
}

pub fn update_entry_fields(
    source: &str,
    key: &str,
    entry_type: &str,
    fields: &BTreeMap<String, String>,
) -> Result<String> {
    update_entry_fields_impl(source, key, entry_type, fields, &[])
}

pub fn update_entry_fields_exact(
    source: &str,
    key: &str,
    entry_type: &str,
    fields: &BTreeMap<String, String>,
    controlled_fields: &[&str],
) -> Result<String> {
    update_entry_fields_impl(source, key, entry_type, fields, controlled_fields)
}

pub fn remove_entry_type_fields(
    source: &str,
    entry_type: &str,
    field_names: &[&str],
) -> Result<(String, usize)> {
    let spans = scan_entries(source)?;
    let mut edits = Vec::new();
    for entry in spans
        .iter()
        .filter(|entry| source[entry.type_start..entry.type_end].eq_ignore_ascii_case(entry_type))
    {
        for field in &entry.fields {
            if field_names
                .iter()
                .any(|name| field.name.eq_ignore_ascii_case(name))
            {
                edits.push(Edit {
                    start: field.segment_start,
                    end: field.remove_end,
                    replacement: String::new(),
                });
            }
        }
    }
    let removed = edits.len();
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.start));
    let mut output = source.to_owned();
    for edit in edits {
        output.replace_range(edit.start..edit.end, &edit.replacement);
    }
    Ok((output, removed))
}

pub fn remove_fields(source: &str, field_names: &[&str]) -> Result<String> {
    let spans = scan_entries(source)?;
    let mut edits = Vec::new();
    for entry in &spans {
        for field in &entry.fields {
            if field_names
                .iter()
                .any(|name| field.name.eq_ignore_ascii_case(name))
            {
                edits.push(Edit {
                    start: field.segment_start,
                    end: field.remove_end,
                    replacement: String::new(),
                });
            }
        }
    }
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.start));
    let mut output = source.to_owned();
    for edit in edits {
        output.replace_range(edit.start..edit.end, &edit.replacement);
    }
    Ok(output)
}

pub fn remove_entry_types(source: &str, entry_types: &[&str]) -> Result<String> {
    let spans = scan_entries(source)?;
    let mut edits = spans
        .iter()
        .filter(|entry| {
            entry_types
                .iter()
                .any(|name| source[entry.type_start..entry.type_end].eq_ignore_ascii_case(name))
        })
        .map(|entry| Edit {
            start: entry.type_start.saturating_sub(1),
            end: entry.close + 1,
            replacement: String::new(),
        })
        .collect::<Vec<_>>();
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.start));
    let mut output = source.to_owned();
    for edit in edits {
        output.replace_range(edit.start..edit.end, &edit.replacement);
    }
    Ok(output)
}

fn update_entry_fields_impl(
    source: &str,
    key: &str,
    entry_type: &str,
    fields: &BTreeMap<String, String>,
    controlled_fields: &[&str],
) -> Result<String> {
    let spans = scan_entries(source)?;
    let entry = spans
        .iter()
        .find(|entry| entry.key == key)
        .with_context(|| format!("citation key not found: {key}"))?;
    let mut edits = Vec::new();

    let current_type = &source[entry.type_start..entry.type_end];
    if !current_type.eq_ignore_ascii_case(entry_type) {
        edits.push(Edit {
            start: entry.type_start,
            end: entry.type_end,
            replacement: entry_type.to_owned(),
        });
    }

    let mut missing = Vec::new();
    for (name, value) in fields {
        let matching: Vec<_> = entry
            .fields
            .iter()
            .filter(|field| field.name.eq_ignore_ascii_case(name))
            .collect();
        if matching.len() > 1 {
            bail!("entry {key} has multiple {name} fields");
        }
        let replacement = format!("{name} = {{{value}}}");
        if let Some(field) = matching.first() {
            if source[field.trimmed_start..field.trimmed_end] != replacement {
                edits.push(Edit {
                    start: field.trimmed_start,
                    end: field.trimmed_end,
                    replacement,
                });
            }
        } else {
            missing.push(replacement);
        }
    }

    for field in &entry.fields {
        if controlled_fields
            .iter()
            .any(|name| field.name.eq_ignore_ascii_case(name))
            && !fields.contains_key(&field.name)
        {
            edits.push(Edit {
                start: field.segment_start,
                end: field.remove_end,
                replacement: String::new(),
            });
        }
    }

    edits.sort_by_key(|edit| std::cmp::Reverse(edit.start));
    let mut output = source.to_owned();
    for edit in edits {
        output.replace_range(edit.start..edit.end, &edit.replacement);
    }
    if !missing.is_empty() {
        // Rescan after deletions so insertion uses the remaining comma and field layout.
        let spans = scan_entries(&output)?;
        let entry = spans
            .iter()
            .find(|entry| entry.key == key)
            .context("updated entry missing")?;
        let edit = insertion_edit(
            &output,
            entry,
            missing.join(",\n"),
            !entry.fields.is_empty(),
        );
        output.replace_range(edit.start..edit.end, &edit.replacement);
    }
    Ok(output)
}

fn insertion_edit(
    source: &str,
    entry: &EntrySpan,
    value: String,
    has_remaining_fields: bool,
) -> Edit {
    let bytes = source.as_bytes();
    let mut body_end = entry.close;
    while body_end > entry.open + 1 && bytes[body_end - 1].is_ascii_whitespace() {
        body_end -= 1;
    }
    let has_trailing_comma = body_end > entry.open + 1 && bytes[body_end - 1] == b',';
    let indent = entry
        .fields
        .first()
        .and_then(|field| line_indent(source, field.trimmed_start))
        .unwrap_or("  ");
    let prefix = if has_trailing_comma || !has_remaining_fields {
        ""
    } else {
        ","
    };
    let value = value.replace('\n', &format!("\n{indent}"));
    Edit {
        start: body_end,
        end: body_end,
        replacement: format!("{prefix}\n{indent}{value},"),
    }
}

fn line_indent(source: &str, position: usize) -> Option<&str> {
    let line_start = source[..position].rfind('\n').map_or(0, |index| index + 1);
    let indent = &source[line_start..position];
    indent.chars().all(char::is_whitespace).then_some(indent)
}

fn scan_entries(source: &str) -> Result<Vec<EntrySpan>> {
    let bytes = source.as_bytes();
    let mut entries = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            index = skip_line(bytes, index);
            continue;
        }
        if bytes[index] != b'@' {
            index += 1;
            continue;
        }

        let mut cursor = index + 1;
        skip_space(bytes, &mut cursor);
        let type_start = cursor;
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
        {
            cursor += 1;
        }
        let entry_type = source[type_start..cursor].to_ascii_lowercase();
        skip_space(bytes, &mut cursor);
        if cursor >= bytes.len() || !matches!(bytes[cursor], b'{' | b'(') {
            index += 1;
            continue;
        }
        let open = cursor;
        let close = find_closing(bytes, open)
            .with_context(|| format!("unclosed @{entry_type} entry at byte {index}"))?;
        if matches!(entry_type.as_str(), "string" | "preamble" | "comment") {
            index = close + 1;
            continue;
        }

        let comma = find_top_level(bytes, open + 1, close, b',')
            .with_context(|| format!("entry at byte {index} has no citation-key comma"))?;
        let key = source[open + 1..comma].trim().to_owned();
        if key.is_empty() {
            bail!("entry at byte {index} has an empty citation key");
        }
        let fields = scan_fields(source, comma + 1, close)?;
        entries.push(EntrySpan {
            key,
            type_start,
            type_end: cursor,
            open,
            close,
            fields,
        });
        index = close + 1;
    }
    Ok(entries)
}

fn scan_fields(source: &str, start: usize, end: usize) -> Result<Vec<FieldSpan>> {
    let bytes = source.as_bytes();
    let mut fields = Vec::new();
    let mut segment_start = start;
    loop {
        let comma = find_top_level(bytes, segment_start, end, b',');
        let segment_end = comma.unwrap_or(end);
        let mut trimmed_start = segment_start;
        let mut trimmed_end = segment_end;
        while trimmed_start < trimmed_end && bytes[trimmed_start].is_ascii_whitespace() {
            trimmed_start += 1;
        }
        while trimmed_end > trimmed_start && bytes[trimmed_end - 1].is_ascii_whitespace() {
            trimmed_end -= 1;
        }
        if trimmed_start < trimmed_end {
            let equals = find_top_level(bytes, trimmed_start, trimmed_end, b'=')
                .with_context(|| format!("malformed field at byte {trimmed_start}"))?;
            let name = source[trimmed_start..equals].trim().to_ascii_lowercase();
            fields.push(FieldSpan {
                name,
                segment_start,
                remove_end: comma.map_or(segment_end, |position| position + 1),
                trimmed_start,
                trimmed_end,
            });
        }
        match comma {
            Some(position) => segment_start = position + 1,
            None => break,
        }
    }
    Ok(fields)
}

fn find_closing(bytes: &[u8], open: usize) -> Option<usize> {
    let opening = bytes[open];
    let closing = if opening == b'{' { b'}' } else { b')' };
    let mut depth = 1usize;
    let mut brace_depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    let mut index = open + 1;
    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if opening == b'(' && !quoted && byte == b'{' {
            brace_depth += 1;
        } else if opening == b'(' && !quoted && byte == b'}' {
            brace_depth = brace_depth.saturating_sub(1);
        } else if byte == b'"' && brace_depth == 0 {
            quoted = !quoted;
        } else if !quoted && brace_depth == 0 && byte == opening {
            depth += 1;
        } else if !quoted && brace_depth == 0 && byte == closing {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
        index += 1;
    }
    None
}

fn find_top_level(bytes: &[u8], start: usize, end: usize, needle: u8) -> Option<usize> {
    let mut brace_depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    let mut index = start;
    while index < end {
        let byte = bytes[index];
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'"' && brace_depth == 0 {
            quoted = !quoted;
        } else if !quoted && byte == b'{' {
            brace_depth += 1;
        } else if !quoted && byte == b'}' {
            brace_depth = brace_depth.saturating_sub(1);
        } else if !quoted && brace_depth == 0 && byte == needle {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn skip_space(bytes: &[u8], index: &mut usize) {
    while *index < bytes.len() && bytes[*index].is_ascii_whitespace() {
        *index += 1;
    }
}

fn skip_line(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index] != b'\n' {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::bibtex::parse;

    use super::*;

    const SAMPLE: &str = r#"% keep this comment
@string{conf = {Great Conf}}

@Article{One,
  title = {A {Nested, Great} Paper},
  journal = conf,
  year = 2026,
}

@misc(Two, title = "Quoted, title")
"#;

    fn add_agent_integrity(source: &str, key: &str) -> String {
        let records = parse(source).unwrap();
        let target = records
            .iter()
            .find(|record| record.entry_key == key)
            .unwrap();
        let evidence =
            provenance::actor_source(provenance::SourceKind::Agent, "test-agent", target).unwrap();
        let with_evidence = provenance::append_source(source, &evidence, &records).unwrap();
        let with_link = update_entry_fields(
            &with_evidence,
            key,
            &target.entry_type,
            &BTreeMap::from([(provenance::SOURCE_FIELD.to_owned(), evidence.entry_key)]),
        )
        .unwrap();
        let records = parse(&with_link).unwrap();
        update_source(
            &with_link,
            &records,
            &BTreeSet::from([key.to_owned()]),
            false,
        )
        .unwrap()
    }

    #[test]
    fn adding_and_removing_preserves_unrelated_source() {
        let selected = BTreeSet::from(["One".to_owned()]);
        let added = add_agent_integrity(SAMPLE, "One");
        assert!(added.starts_with("% keep this comment\n@string"));
        assert!(added.contains("integrity = {"));
        assert!(added.contains("@bibsource"));
        let records = parse(&added).unwrap();
        assert!(matches!(status(&records[0], &records), Ok(Status::Valid)));

        let removed = update_source(&added, &records, &selected, true).unwrap();
        assert!(!removed.contains("integrity ="));
        assert!(removed.contains("@bibsource"));
    }

    #[test]
    fn changed_content_makes_integrity_stale() {
        let added = add_agent_integrity(SAMPLE, "Two");
        let changed = added.replace("Quoted, title", "Different title");
        let records = parse(&changed).unwrap();
        assert_eq!(status(&records[1], &records).unwrap(), Status::Stale);
    }

    #[test]
    fn history_link_is_not_covered_by_integrity() {
        let record = parse("@article{key, title={A title}}").unwrap().remove(0);
        let expected = hash(&record).unwrap();
        let mut linked = record;
        linked
            .fields
            .insert(PREVIOUS_FIELD.to_owned(), "bibversion:12345678".to_owned());
        assert_eq!(hash(&linked).unwrap(), expected);
    }

    #[test]
    fn workflow_entries_do_not_participate_in_integrity() {
        for entry_type in ["bibsource", "bibversion"] {
            let record = parse(&format!("@{entry_type}{{key, target={{paper}}}}"))
                .unwrap()
                .remove(0);
            assert!(hash(&record).is_err());
        }
    }

    #[test]
    fn hash_matches_url2bibtex_python_implementation() {
        let record = parse(
            "@Article{Key, Title={Hello {World}}, AUTHOR={Doe, Jane and Smith, John}, year=2025}",
        )
        .unwrap()
        .remove(0);
        assert_eq!(
            hash(&record).unwrap(),
            "b46c23357cd2a600fc9ab6607e90b77e7d48d88fd3d68666c532be50777a8a77"
        );
    }

    #[test]
    fn legacy_marker_without_provenance_is_invalid() {
        let mut record = parse("@article{key, title={A title}}").unwrap().remove(0);
        record
            .fields
            .insert("integrity".to_owned(), hash(&record).unwrap());
        let records = vec![record];
        assert_eq!(status(&records[0], &records).unwrap(), Status::Invalid);
    }

    #[test]
    fn scans_parenthesized_entry_with_closing_paren_in_braces() {
        let source = "@misc(Key, title={A title (revised)}, note={contains ) safely})\n";
        let added = add_agent_integrity(source, "Key");
        let records = parse(&added).unwrap();
        assert_eq!(status(&records[0], &records).unwrap(), Status::Valid);
    }

    #[test]
    fn updating_fields_preserves_local_source_structure() {
        let fields = BTreeMap::from([
            ("title".to_owned(), "Replacement".to_owned()),
            ("doi".to_owned(), "10.1/example".to_owned()),
        ]);
        let output = update_entry_fields(SAMPLE, "One", "article", &fields).unwrap();
        assert!(output.starts_with("% keep this comment\n@string{conf"));
        assert!(output.contains("journal = conf"));
        assert!(output.contains("title = {Replacement}"));
        assert!(output.contains("doi = {10.1/example}"));
        assert!(output.contains("@misc(Two"));
    }

    #[test]
    fn exact_update_can_replace_the_only_controlled_field() {
        let source = "@article{alpha, url={https://arxiv.org/abs/2401.01234}}\n";
        let fields = BTreeMap::from([
            ("bibprovider".to_owned(), "doi".to_owned()),
            (
                "bibproviderid".to_owned(),
                "10.48550/arXiv.2401.01234".to_owned(),
            ),
            ("bibsource".to_owned(), "bibsource:provider:abc".to_owned()),
            ("doi".to_owned(), "10.48550/arXiv.2401.01234".to_owned()),
            ("title".to_owned(), "Raw DOI title".to_owned()),
        ]);
        let output = update_entry_fields_exact(
            source,
            "alpha",
            "article",
            &fields,
            &["url", "title", "doi", "bibprovider", "bibproviderid"],
        )
        .unwrap();

        parse(&output).unwrap_or_else(|error| panic!("{error:#}\n{output}"));
        assert!(!output.contains("arxiv.org"));
        assert!(output.contains("doi = {10.48550/arXiv.2401.01234}"));
    }

    #[test]
    fn removes_legacy_response_fields_without_reformatting() {
        let source = r#"% keep
@bibsource{receipt,
  kind = {provider},
  responseencoding = {base64},
  response = {eyJ0ZXN0Ijp0cnVlfQ==},
  responsesha256 = {abc},
}
"#;
        let (output, removed) =
            remove_entry_type_fields(source, "bibsource", &["response", "responseencoding"])
                .unwrap();
        assert_eq!(removed, 2);
        assert!(output.starts_with("% keep\n@bibsource"));
        assert!(!output.contains("responseencoding"));
        assert!(!output.contains("response ="));
        assert!(output.contains("responsesha256 = {abc}"));
    }

    #[test]
    fn conditional_atomic_write_rejects_a_stale_read() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("references.bib");
        fs::write(&path, "original").unwrap();
        fs::write(&path, "changed elsewhere").unwrap();

        let error = atomic_write_if_unchanged(&path, "original", "our update").unwrap_err();
        assert!(format!("{error:#}").contains("changed since it was read"));
        assert_eq!(fs::read_to_string(path).unwrap(), "changed elsewhere");
    }
}
