use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::bibtex::Record;

pub const PROVIDER_FIELD: &str = "bibprovider";
pub const PROVIDER_ID_FIELD: &str = "bibproviderid";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LiteratureIdentifier {
    ProviderId(String),
    Doi(String),
}

impl LiteratureIdentifier {
    pub fn value(&self) -> &str {
        match self {
            Self::ProviderId(value) | Self::Doi(value) => value,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Contributor {
    pub family: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub given: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orcid: Option<String>,
}

impl Contributor {
    fn to_bibtex(&self) -> String {
        match self.given.as_deref().filter(|value| !value.is_empty()) {
            Some(given) if self.family.is_empty() => given.to_owned(),
            Some(given) => format!("{}, {}", self.family, given),
            None => self.family.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PublicationDate {
    pub year: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub month: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub day: Option<u8>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LiteratureRecord {
    pub provider: String,
    pub id: String,
    pub record_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<Contributor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub editors: Vec<Contributor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issued: Option<PublicationDate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pages: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub article_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doi: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub isbn: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issn: Vec<String>,
}

impl LiteratureRecord {
    pub fn from_bibtex_record(provider: &str, id: &str, record: &Record) -> Self {
        let fields = &record.fields;
        Self {
            provider: provider.to_owned(),
            id: id.to_owned(),
            record_type: match record.entry_type.as_str() {
                "article" => "journal-article",
                "inproceedings" => "proceedings-article",
                "incollection" => "book-chapter",
                "book" => "book",
                "phdthesis" => "dissertation",
                "techreport" => "report",
                other => other,
            }
            .to_owned(),
            title: fields.get("title").cloned(),
            authors: parse_contributors(fields.get("author")),
            editors: parse_contributors(fields.get("editor")),
            container_title: fields
                .get("journal")
                .or_else(|| fields.get("booktitle"))
                .cloned(),
            publisher: fields.get("publisher").cloned(),
            issued: fields
                .get("year")
                .and_then(|year| year.trim().parse().ok())
                .map(|year| PublicationDate {
                    year,
                    month: fields
                        .get("month")
                        .and_then(|month| month.trim().parse().ok()),
                    day: None,
                }),
            volume: fields.get("volume").cloned(),
            issue: fields.get("number").cloned(),
            pages: fields.get("pages").cloned(),
            article_number: fields.get("eid").cloned(),
            doi: fields.get("doi").cloned(),
            url: fields.get("url").cloned(),
            isbn: fields.get("isbn").cloned().into_iter().collect(),
            issn: fields.get("issn").cloned().into_iter().collect(),
        }
    }

    pub fn bibtex_type(&self) -> &'static str {
        match self.record_type.as_str() {
            "journal-article" => "article",
            "proceedings-article" => "inproceedings",
            "book-chapter" | "reference-entry" => "incollection",
            "book" | "edited-book" | "monograph" | "reference-book" => "book",
            "dissertation" => "phdthesis",
            "report" => "techreport",
            _ => "misc",
        }
    }

    pub fn bibtex_fields(&self) -> BTreeMap<String, String> {
        let mut fields = BTreeMap::new();
        insert_option(&mut fields, "title", self.title.clone());
        if !self.authors.is_empty() {
            fields.insert(
                "author".into(),
                self.authors
                    .iter()
                    .map(Contributor::to_bibtex)
                    .collect::<Vec<_>>()
                    .join(" and "),
            );
        }
        if !self.editors.is_empty() {
            fields.insert(
                "editor".into(),
                self.editors
                    .iter()
                    .map(Contributor::to_bibtex)
                    .collect::<Vec<_>>()
                    .join(" and "),
            );
        }
        let container_field = match self.bibtex_type() {
            "article" => Some("journal"),
            "inproceedings" | "incollection" => Some("booktitle"),
            _ => None,
        };
        if let Some(field) = container_field {
            insert_option(&mut fields, field, self.container_title.clone());
        }
        insert_option(&mut fields, "publisher", self.publisher.clone());
        if let Some(date) = &self.issued {
            fields.insert("year".into(), date.year.to_string());
            if let Some(month) = date.month {
                fields.insert("month".into(), month.to_string());
            }
        }
        insert_option(&mut fields, "volume", self.volume.clone());
        insert_option(&mut fields, "number", self.issue.clone());
        insert_option(&mut fields, "pages", self.pages.clone());
        insert_option(&mut fields, "eid", self.article_number.clone());
        insert_option(&mut fields, "doi", self.doi.clone());
        insert_option(&mut fields, "url", self.url.clone());
        if let Some(isbn) = self.isbn.first() {
            fields.insert("isbn".into(), isbn.clone());
        }
        if let Some(issn) = self.issn.first() {
            fields.insert("issn".into(), issn.clone());
        }
        fields.insert(PROVIDER_FIELD.into(), self.provider.clone());
        fields.insert(PROVIDER_ID_FIELD.into(), self.id.clone());
        fields
    }
}

fn parse_contributors(value: Option<&String>) -> Vec<Contributor> {
    value
        .into_iter()
        .flat_map(|value| value.split(" and "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| match value.split_once(',') {
            Some((family, given)) => Contributor {
                family: family.trim().to_owned(),
                given: Some(given.trim().to_owned()).filter(|value| !value.is_empty()),
                orcid: None,
            },
            None => Contributor {
                family: value.to_owned(),
                given: None,
                orcid: None,
            },
        })
        .collect()
}

fn insert_option(fields: &mut BTreeMap<String, String>, name: &str, value: Option<String>) {
    if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
        fields.insert(name.to_owned(), value);
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Candidate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    pub record: LiteratureRecord,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct BibliographicQuery {
    pub citation: String,
    #[serde(default)]
    pub title_only: bool,
}

impl BibliographicQuery {
    pub fn from_record(record: &Record) -> Self {
        let fields = &record.fields;
        let citation = [
            fields.get("author"),
            fields.get("title"),
            fields.get("journal").or_else(|| fields.get("booktitle")),
            fields.get("year"),
        ]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(". ");
        Self {
            citation,
            title_only: false,
        }
    }
}

pub fn title_search_query(record: &Record) -> BibliographicQuery {
    let title = record.fields.get("title").map(String::as_str).unwrap_or("");
    let mut plain = String::with_capacity(title.len());
    for character in title.chars() {
        match character {
            '{' | '}' | '\\' => {}
            '~' => plain.push(' '),
            character => plain.push(character),
        }
    }
    BibliographicQuery {
        citation: plain.split_whitespace().collect::<Vec<_>>().join(" "),
        title_only: true,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FieldChange {
    pub field: String,
    pub current: Option<String>,
    pub proposed: Option<String>,
}

pub fn changes(current: &Record, proposed: &LiteratureRecord) -> Vec<FieldChange> {
    record_changes(current, &proposed_record(current, proposed))
}

pub fn proposed_record(current: &Record, proposed: &LiteratureRecord) -> Record {
    let mut after = current.clone();
    after.entry_type = proposed.bibtex_type().to_owned();
    for field in crate::provenance::CONTROLLED_FIELDS {
        after.fields.remove(*field);
    }
    after.fields.extend(proposed.bibtex_fields());
    after
}

pub fn record_changes(current: &Record, proposed: &Record) -> Vec<FieldChange> {
    let mut output = Vec::new();
    if !current
        .entry_type
        .eq_ignore_ascii_case(&proposed.entry_type)
    {
        output.push(FieldChange {
            field: "ENTRYTYPE".into(),
            current: Some(current.entry_type.clone()),
            proposed: Some(proposed.entry_type.clone()),
        });
    }
    for field in current
        .fields
        .keys()
        .chain(proposed.fields.keys())
        .collect::<std::collections::BTreeSet<_>>()
    {
        if crate::history::is_workflow_field(field) {
            continue;
        }
        let value = proposed.fields.get(field).cloned();
        if current.fields.get(field) != value.as_ref() {
            output.push(FieldChange {
                field: field.clone(),
                current: current.fields.get(field).cloned(),
                proposed: value,
            });
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_provider_neutral_bibtex_fields() {
        let record = LiteratureRecord {
            provider: "test".into(),
            id: "work-1".into(),
            record_type: "journal-article".into(),
            title: Some("A result".into()),
            authors: vec![Contributor {
                family: "Doe".into(),
                given: Some("Jane".into()),
                orcid: None,
            }],
            editors: vec![],
            container_title: Some("A Journal".into()),
            publisher: None,
            issued: Some(PublicationDate {
                year: 2026,
                month: None,
                day: None,
            }),
            volume: None,
            issue: None,
            pages: None,
            article_number: None,
            doi: Some("10.1/example".into()),
            url: None,
            isbn: vec![],
            issn: vec![],
        };
        let fields = record.bibtex_fields();
        assert_eq!(fields["author"], "Doe, Jane");
        assert_eq!(fields["journal"], "A Journal");
        assert_eq!(fields[PROVIDER_FIELD], "test");
        assert_eq!(fields[PROVIDER_ID_FIELD], "work-1");
    }

    #[test]
    fn title_search_removes_bibtex_grouping_without_changing_the_record() {
        let record = Record {
            entry_type: "article".into(),
            entry_key: "one".into(),
            fields: BTreeMap::from([(
                "title".into(),
                "{Writing and {Working} Memory}: \\LaTeX~Notes".into(),
            )]),
        };
        assert_eq!(
            title_search_query(&record).citation,
            "Writing and Working Memory: LaTeX Notes"
        );
        assert_eq!(
            record.fields["title"],
            "{Writing and {Working} Memory}: \\LaTeX~Notes"
        );
    }
}
