use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Url;
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::bibtex::parse;
use crate::catalog::{
    BibliographicQuery, Candidate, Contributor, LiteratureIdentifier, LiteratureRecord,
    PublicationDate,
};

use super::{FetchedRecord, FetchedSearch, LiteratureProvider};

const DEFAULT_BASE_URL: &str = "https://api2.openreview.net/";

pub struct OpenReviewProvider {
    client: Client,
    base_url: Url,
}

impl OpenReviewProvider {
    pub fn new() -> Result<Self> {
        let base_url = std::env::var("BIBLOCK_OPENREVIEW_API_BASE")
            .unwrap_or_else(|_| DEFAULT_BASE_URL.to_owned());
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(20))
                .user_agent(format!(
                    "biblock/{} (https://github.com/EvoEvolver/biblock)",
                    env!("CARGO_PKG_VERSION")
                ))
                .build()
                .context("could not initialize OpenReview client")?,
            base_url: Url::parse(&base_url).context("invalid OpenReview API base URL")?,
        })
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        self.base_url
            .join(path)
            .context("invalid OpenReview API endpoint")
    }

    fn get(&self, url: Url) -> Result<(String, Vec<u8>)> {
        let response = self
            .client
            .get(url.clone())
            .send()
            .with_context(|| format!("could not contact OpenReview at {url}"))?;
        let status = response.status();
        if !status.is_success() {
            bail!("OpenReview returned HTTP {status} for {url}");
        }
        let final_url = response.url().to_string();
        let body = response
            .bytes()
            .with_context(|| format!("could not read OpenReview response from {url}"))?;
        Ok((final_url, body.to_vec()))
    }
}

impl LiteratureProvider for OpenReviewProvider {
    fn name(&self) -> &'static str {
        "openreview"
    }

    fn lookup(&self, identifier: &LiteratureIdentifier) -> Result<FetchedRecord> {
        let url = self.endpoint("notes/search")?;
        let body = serde_json::to_vec(&serde_json::json!({"ids": [identifier.value()]}))?;
        let request_body_sha256 = format!("{:x}", Sha256::digest(&body));
        let response = self
            .client
            .post(url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .with_context(|| format!("could not contact OpenReview at {url}"))?;
        let status = response.status();
        if !status.is_success() {
            bail!("OpenReview returned HTTP {status} for {url}");
        }
        let request_url = response.url().to_string();
        let response = response
            .bytes()
            .context("could not read OpenReview lookup response")?
            .to_vec();
        Ok(FetchedRecord {
            record: record_from_response(identifier.value(), &response)?,
            request_url,
            media_type: "application/json".to_owned(),
            response,
            request_method: "POST".to_owned(),
            request_body_sha256: Some(request_body_sha256),
        })
    }

    fn search(&self, query: &BibliographicQuery, limit: usize) -> Result<FetchedSearch> {
        if query.citation.trim().is_empty() {
            bail!("cannot search OpenReview without a title");
        }
        let mut url = self.endpoint("notes/search")?;
        url.query_pairs_mut()
            .append_pair("term", &query.citation)
            .append_pair("content", "title")
            .append_pair("type", "exact");
        let (request_url, response) = self.get(url)?;
        let parsed: NotesResponse =
            serde_json::from_slice(&response).context("invalid OpenReview search response")?;
        let candidates = parsed
            .notes
            .iter()
            .take(limit.clamp(1, 20))
            .filter_map(|note| note_to_record(note).ok())
            .map(|record| Candidate {
                score: None,
                record,
            })
            .collect();
        Ok(FetchedSearch {
            candidates,
            request_url,
            media_type: "application/json".to_owned(),
            response,
        })
    }
}

#[derive(Deserialize)]
struct NotesResponse {
    #[serde(default)]
    notes: Vec<Value>,
}

pub(crate) fn record_from_response(id: &str, response: &[u8]) -> Result<LiteratureRecord> {
    let parsed: NotesResponse =
        serde_json::from_slice(response).context("invalid OpenReview note response")?;
    let note = parsed
        .notes
        .iter()
        .find(|note| note.get("id").and_then(Value::as_str) == Some(id))
        .with_context(|| format!("OpenReview response contains no matching note {id}"))?;
    note_to_record(note)
}

fn note_to_record(note: &Value) -> Result<LiteratureRecord> {
    let id = note
        .get("id")
        .and_then(Value::as_str)
        .context("OpenReview note has no id")?;
    let content = note
        .get("content")
        .and_then(Value::as_object)
        .context("OpenReview note has no content")?;
    if let Some(bibtex) = content_value(content, "_bibtex").and_then(Value::as_str)
        && let Some(record) = parse(bibtex)?
            .into_iter()
            .find(|record| !record.is_system())
    {
        let mut literature = LiteratureRecord::from_bibtex_record("openreview", id, &record);
        literature.url = Some(format!("https://openreview.net/forum?id={id}"));
        return Ok(literature);
    }

    let title = content_value(content, "title")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let authors = content_value(content, "authors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|name| Contributor {
            family: name.to_owned(),
            given: None,
            orcid: None,
        })
        .collect();
    let venue = content_value(content, "venue")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let year = venue.as_deref().and_then(extract_year);
    Ok(LiteratureRecord {
        provider: "openreview".to_owned(),
        id: id.to_owned(),
        record_type: "posted-content".to_owned(),
        title,
        authors,
        editors: vec![],
        container_title: venue,
        publisher: None,
        issued: year.map(|year| PublicationDate {
            year,
            month: None,
            day: None,
        }),
        volume: None,
        issue: None,
        pages: None,
        article_number: None,
        doi: None,
        url: Some(format!("https://openreview.net/forum?id={id}")),
        isbn: vec![],
        issn: vec![],
    })
}

fn content_value<'a>(
    content: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Option<&'a Value> {
    content.get(field)?.get("value")
}

fn extract_year(value: &str) -> Option<i32> {
    value
        .split(|character: char| !character.is_ascii_digit())
        .find(|part| part.len() == 4 && matches!(&part[..2], "19" | "20"))?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_v2_bibtex_note_to_literature_record() {
        let response = br#"{"notes":[{"id":"abc123","content":{"_bibtex":{"value":"@inproceedings{x, title={A Paper}, author={Doe, Jane}, year={2026}}"}}}]}"#;
        let record = record_from_response("abc123", response).unwrap();
        assert_eq!(record.provider, "openreview");
        assert_eq!(record.id, "abc123");
        assert_eq!(record.title.as_deref(), Some("A Paper"));
        assert_eq!(
            record.url.as_deref(),
            Some("https://openreview.net/forum?id=abc123")
        );
    }
}
