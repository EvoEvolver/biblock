use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use percent_encoding::percent_decode_str;
use quick_xml::Reader;
use quick_xml::events::Event;
use regex::Regex;
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, LOCATION};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_REDIRECTS: usize = 5;
const MAX_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_WEB_EVIDENCE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResolutionConfidence {
    Exact,
    Strong,
}

impl ResolutionConfidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Strong => "strong",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MatchSignal {
    pub kind: String,
    pub value: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ResolutionEvidence {
    pub method: String,
    pub input_url: String,
    pub final_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_sha256: Option<String>,
    pub response_bytes: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ResolutionCandidate {
    pub kind: String,
    pub value: String,
    pub confidence: ResolutionConfidence,
    pub signals: Vec<MatchSignal>,
    pub evidence: ResolutionEvidence,
}

#[derive(Clone, Debug, Serialize)]
pub struct ResolutionReport {
    pub input: String,
    pub status: &'static str,
    pub candidates: Vec<ResolutionCandidate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WebEvidence {
    pub input_url: String,
    pub request_url: String,
    pub final_url: String,
    pub media_type: String,
    pub response_sha256: String,
    pub response_bytes: usize,
}

impl ResolutionReport {
    pub fn exact_candidate(&self) -> Result<&ResolutionCandidate> {
        match self.candidates.as_slice() {
            [candidate] => Ok(candidate),
            [] => bail!("URL did not yield a stable literature identifier"),
            _ => bail!("URL yielded conflicting literature identifiers; review candidates"),
        }
    }
}

struct FetchedPage {
    request_url: String,
    final_url: String,
    media_type: String,
    body: Vec<u8>,
}

pub fn resolve_url(input: &str) -> Result<ResolutionReport> {
    let input_url = normalize_input_url(input)?;
    let input_string = input_url.to_string();
    let decoded_input = percent_decode_str(input_url.as_str()).decode_utf8_lossy();
    let direct_dois = find_dois(&decoded_input);
    if !direct_dois.is_empty() {
        return Ok(report_from_dois(
            &input_string,
            &input_string,
            direct_dois,
            "url-doi",
            "doi-in-url",
            ResolutionConfidence::Exact,
            None,
        ));
    }

    if let Some(arxiv_id) = find_arxiv_id(&decoded_input) {
        return resolve_arxiv(&input_string, &arxiv_id);
    }

    let fetched = fetch_page(input_url, MAX_RESPONSE_BYTES)?;
    let decoded_final = percent_decode_str(&fetched.final_url).decode_utf8_lossy();
    let mut discovered: BTreeMap<String, (ResolutionConfidence, BTreeSet<String>)> =
        BTreeMap::new();
    for doi in find_dois(&decoded_final) {
        add_discovery(
            &mut discovered,
            doi,
            ResolutionConfidence::Exact,
            "doi-in-final-url",
        );
    }

    let is_html = fetched.media_type.contains("text/html")
        || fetched.media_type.contains("application/xhtml+xml");
    if !is_html {
        bail!(
            "URL returned unsupported media type {}; expected HTML",
            fetched.media_type
        );
    }
    let html = String::from_utf8_lossy(&fetched.body);
    for (doi, signal) in dois_from_html(&html)? {
        add_discovery(&mut discovered, doi, ResolutionConfidence::Strong, &signal);
    }

    Ok(report_from_discoveries(
        &input_string,
        &fetched.final_url,
        discovered,
        page_evidence(&input_string, "html-metadata", &fetched),
    ))
}

pub fn identifiers_in_url(input_url: &str) -> BTreeSet<(String, String)> {
    let decoded = percent_decode_str(input_url).decode_utf8_lossy();
    find_dois(&decoded)
        .into_iter()
        .map(|doi| ("doi".into(), doi))
        .collect()
}

pub fn arxiv_id_in_url(input_url: &str) -> Option<String> {
    find_arxiv_id(input_url)
}

pub fn identifier_from_record(
    record: &crate::bibtex::Record,
) -> Option<(crate::catalog::LiteratureIdentifier, ResolutionCandidate)> {
    for (field, value) in &record.fields {
        if crate::history::is_workflow_field(field) {
            continue;
        }
        let input = value.replace("\\_", "_");
        let arxiv_input = if field == "eprint" {
            format!("https://arxiv.org/abs/{}", input.trim())
        } else {
            input.clone()
        };
        let (kind, id) = if let Some((_, doi)) = identifiers_in_url(&input).into_iter().next() {
            ("doi", doi)
        } else if let Some(arxiv) = arxiv_id_in_url(&arxiv_input) {
            ("arxiv", arxiv)
        } else {
            continue;
        };
        let identifier = crate::catalog::LiteratureIdentifier::Doi(if kind == "arxiv" {
            format!("10.48550/arXiv.{id}")
        } else {
            id.clone()
        });
        let evidence_input = if field == "eprint" && kind == "arxiv" {
            format!("eprint:{arxiv_input}")
        } else {
            format!("{field}:{input}")
        };
        return Some((
            identifier,
            ResolutionCandidate {
                kind: kind.to_owned(),
                value: id.clone(),
                confidence: ResolutionConfidence::Exact,
                signals: vec![MatchSignal {
                    kind: format!("{kind}-in-bibtex-field:{field}"),
                    value: id,
                }],
                evidence: ResolutionEvidence {
                    method: "bibtex-field".to_owned(),
                    input_url: evidence_input.clone(),
                    final_url: evidence_input,
                    request_url: None,
                    media_type: None,
                    response_sha256: None,
                    response_bytes: 0,
                },
            },
        ));
    }
    None
}

pub fn fetch_web_evidence(input: &str) -> Result<WebEvidence> {
    let input_url = normalize_input_url(input)?;
    let fetched = fetch_page(input_url, MAX_WEB_EVIDENCE_BYTES)?;
    Ok(WebEvidence {
        input_url: input.trim().to_owned(),
        request_url: fetched.request_url,
        final_url: fetched.final_url,
        media_type: fetched.media_type,
        response_sha256: sha256(&fetched.body),
        response_bytes: fetched.body.len(),
    })
}

fn resolve_arxiv(input_url: &str, arxiv_id: &str) -> Result<ResolutionReport> {
    let mut api_url = Url::parse("https://export.arxiv.org/api/query")?;
    api_url.query_pairs_mut().append_pair("id_list", arxiv_id);
    let fetched = fetch_page(api_url, MAX_RESPONSE_BYTES)?;
    let mut discovered = BTreeMap::new();
    for doi in dois_from_arxiv_atom(&fetched.body)? {
        add_discovery(
            &mut discovered,
            doi,
            ResolutionConfidence::Exact,
            "arxiv-api-doi",
        );
    }
    let evidence = page_evidence(input_url, "arxiv-atom", &fetched);
    if discovered.is_empty() {
        return Ok(ResolutionReport {
            input: input_url.to_owned(),
            status: "resolved",
            candidates: vec![ResolutionCandidate {
                kind: "arxiv".into(),
                value: arxiv_id.to_owned(),
                confidence: ResolutionConfidence::Exact,
                signals: vec![MatchSignal {
                    kind: "arxiv-id-in-url".into(),
                    value: arxiv_id.to_owned(),
                }],
                evidence,
            }],
            warnings: vec![
                "arXiv record has no DOI; no DOI-backed provider can be selected".into(),
            ],
        });
    }
    Ok(report_from_discoveries(
        input_url,
        &fetched.final_url,
        discovered,
        evidence,
    ))
}

fn report_from_dois(
    input: &str,
    final_url: &str,
    dois: BTreeSet<String>,
    method: &str,
    signal: &str,
    confidence: ResolutionConfidence,
    evidence: Option<ResolutionEvidence>,
) -> ResolutionReport {
    let mut discoveries = BTreeMap::new();
    for doi in dois {
        add_discovery(&mut discoveries, doi, confidence, signal);
    }
    let evidence = evidence.unwrap_or_else(|| ResolutionEvidence {
        method: method.into(),
        input_url: input.into(),
        final_url: final_url.into(),
        request_url: None,
        media_type: None,
        response_sha256: None,
        response_bytes: 0,
    });
    report_from_discoveries(input, final_url, discoveries, evidence)
}

fn report_from_discoveries(
    input: &str,
    _final_url: &str,
    discoveries: BTreeMap<String, (ResolutionConfidence, BTreeSet<String>)>,
    evidence: ResolutionEvidence,
) -> ResolutionReport {
    let candidates = discoveries
        .into_iter()
        .map(|(doi, (confidence, signals))| ResolutionCandidate {
            kind: "doi".into(),
            value: doi.clone(),
            confidence,
            signals: signals
                .into_iter()
                .map(|kind| MatchSignal {
                    kind,
                    value: doi.clone(),
                })
                .collect(),
            evidence: evidence.clone(),
        })
        .collect::<Vec<_>>();
    let status = match candidates.len() {
        0 => "unresolved",
        1 => "resolved",
        _ => "ambiguous",
    };
    let warnings = (candidates.len() > 1)
        .then(|| "conflicting DOI values were found; explicit review is required".into())
        .into_iter()
        .collect();
    ResolutionReport {
        input: input.into(),
        status,
        candidates,
        warnings,
    }
}

fn add_discovery(
    discovered: &mut BTreeMap<String, (ResolutionConfidence, BTreeSet<String>)>,
    doi: String,
    confidence: ResolutionConfidence,
    signal: &str,
) {
    let entry = discovered
        .entry(doi)
        .or_insert((confidence, BTreeSet::new()));
    if confidence == ResolutionConfidence::Exact {
        entry.0 = confidence;
    }
    entry.1.insert(signal.to_owned());
}

fn page_evidence(input_url: &str, method: &str, fetched: &FetchedPage) -> ResolutionEvidence {
    ResolutionEvidence {
        method: method.into(),
        input_url: input_url.into(),
        final_url: fetched.final_url.clone(),
        request_url: Some(fetched.request_url.clone()),
        media_type: Some(fetched.media_type.clone()),
        response_sha256: Some(sha256(&fetched.body)),
        response_bytes: fetched.body.len(),
    }
}

fn normalize_input_url(input: &str) -> Result<Url> {
    let value = input.trim();
    let mut url = Url::parse(value).with_context(|| format!("invalid URL: {value}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("URL scheme must be http or https");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("URLs containing credentials are not allowed");
    }
    url.set_fragment(None);
    Ok(url)
}

fn fetch_page(mut url: Url, max_response_bytes: u64) -> Result<FetchedPage> {
    let initial = url.to_string();
    for redirect in 0..=MAX_REDIRECTS {
        let (host, addresses) = public_addresses(&url)?;
        let client = Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(format!(
                "biblock/{} (https://github.com/EvoEvolver/biblock)",
                env!("CARGO_PKG_VERSION")
            ))
            .resolve_to_addrs(&host, &addresses)
            .build()
            .context("could not initialize URL resolver HTTP client")?;
        let response = client
            .get(url.clone())
            .header(
                ACCEPT,
                "text/html,application/xhtml+xml,application/atom+xml;q=0.9",
            )
            .send()
            .with_context(|| format!("could not fetch literature URL {url}"))?;
        if response.status().is_redirection() {
            if redirect == MAX_REDIRECTS {
                bail!("URL exceeded {MAX_REDIRECTS} redirects");
            }
            let location = response
                .headers()
                .get(LOCATION)
                .context("redirect response is missing Location")?
                .to_str()
                .context("redirect Location is not valid text")?;
            url = normalize_input_url(url.join(location)?.as_str())?;
            continue;
        }
        if !response.status().is_success() {
            bail!(
                "literature URL returned HTTP {} for {url}",
                response.status()
            );
        }
        if response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|length| length > max_response_bytes)
        {
            bail!("URL response exceeds {max_response_bytes} bytes");
        }
        let media_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .split(';')
            .next()
            .unwrap_or("application/octet-stream")
            .trim()
            .to_ascii_lowercase();
        let mut body = Vec::new();
        response
            .take(max_response_bytes + 1)
            .read_to_end(&mut body)
            .with_context(|| format!("could not read literature URL response from {url}"))?;
        if body.len() as u64 > max_response_bytes {
            bail!("URL response exceeds {max_response_bytes} bytes");
        }
        return Ok(FetchedPage {
            request_url: initial,
            final_url: url.to_string(),
            media_type,
            body,
        });
    }
    unreachable!("redirect loop always returns or fails")
}

fn public_addresses(url: &Url) -> Result<(String, Vec<SocketAddr>)> {
    let host = url.host_str().context("URL is missing a host")?.to_owned();
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        bail!("local URL hosts are not allowed");
    }
    let port = url
        .port_or_known_default()
        .context("URL has no known port")?;
    if !matches!((url.scheme(), port), ("http", 80) | ("https", 443)) {
        bail!("only the default HTTP and HTTPS ports are allowed");
    }
    let addresses = (host.as_str(), port)
        .to_socket_addrs()
        .with_context(|| format!("could not resolve URL host {host}"))?
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        bail!("URL host {host} did not resolve to an address");
    }
    if addresses.iter().any(|address| !is_public_ip(address.ip())) {
        bail!("URL host {host} resolves to a non-public address");
    }
    Ok((host, addresses))
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => is_public_v6(ip),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || a == 0
        || a >= 240
        || (a == 100 && (64..=127).contains(&b))
        || (a == 198 && (18..=19).contains(&b)))
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}

fn find_dois(text: &str) -> BTreeSet<String> {
    static DOI: OnceLock<Regex> = OnceLock::new();
    let regex = DOI.get_or_init(|| {
        Regex::new(r"(?i)10\.\d{4,9}/[-._;()/:a-z0-9]+")
            .expect("DOI regex is a compile-time constant")
    });
    regex
        .find_iter(text)
        .filter_map(|value| normalize_doi(value.as_str()))
        .collect()
}

fn normalize_doi(value: &str) -> Option<String> {
    let mut value = value.trim().trim_end_matches(['.', ',', ';', ':']);
    while value.ends_with(')')
        && value.chars().filter(|character| *character == ')').count()
            > value.chars().filter(|character| *character == '(').count()
    {
        value = &value[..value.len() - 1];
    }
    (!value.is_empty()).then(|| value.to_ascii_lowercase())
}

fn find_arxiv_id(text: &str) -> Option<String> {
    static ARXIV: OnceLock<Regex> = OnceLock::new();
    let regex = ARXIV.get_or_init(|| {
        Regex::new(
            r"(?i)(?:arxiv\.org/(?:abs|pdf|html)/|arxiv:)([a-z-]+/\d{7}|\d{4}\.\d{4,5})(?:v\d+)?",
        )
        .expect("arXiv regex is a compile-time constant")
    });
    regex
        .captures(text)
        .and_then(|capture| capture.get(1))
        .map(|value| value.as_str().to_ascii_lowercase())
}

fn dois_from_html(html: &str) -> Result<Vec<(String, String)>> {
    let document = Html::parse_document(html);
    let meta_selector = Selector::parse("meta").expect("static selector is valid");
    let script_selector =
        Selector::parse("script[type='application/ld+json']").expect("static selector is valid");
    let mut found = Vec::new();
    for element in document.select(&meta_selector) {
        let name = element
            .value()
            .attr("name")
            .or_else(|| element.value().attr("property"))
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if !matches!(
            name.as_str(),
            "citation_doi" | "dc.identifier.doi" | "dc.identifier" | "prism.doi"
        ) {
            continue;
        }
        if let Some(content) = element.value().attr("content") {
            for doi in find_dois(content) {
                found.push((doi, format!("html-meta:{name}")));
            }
        }
    }
    for element in document.select(&script_selector) {
        let value: serde_json::Value = match serde_json::from_str(&element.inner_html()) {
            Ok(value) => value,
            Err(_) => continue,
        };
        collect_json_ld_dois(&value, &mut found);
    }
    Ok(found)
}

fn collect_json_ld_dois(value: &serde_json::Value, found: &mut Vec<(String, String)>) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                collect_json_ld_dois(value, found);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if matches!(
                    key.to_ascii_lowercase().as_str(),
                    "doi" | "identifier" | "sameas" | "@id" | "value"
                ) {
                    collect_json_strings(value, "json-ld", found);
                }
                if matches!(key.as_str(), "@graph" | "mainEntity" | "mainEntityOfPage") {
                    collect_json_ld_dois(value, found);
                }
            }
        }
        _ => {}
    }
}

fn collect_json_strings(
    value: &serde_json::Value,
    signal: &str,
    found: &mut Vec<(String, String)>,
) {
    match value {
        serde_json::Value::String(value) => {
            for doi in find_dois(value) {
                found.push((doi, signal.into()));
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_json_strings(value, signal, found);
            }
        }
        serde_json::Value::Object(map) => {
            for value in map.values() {
                collect_json_strings(value, signal, found);
            }
        }
        _ => {}
    }
}

fn dois_from_arxiv_atom(response: &[u8]) -> Result<BTreeSet<String>> {
    let mut reader = Reader::from_reader(response);
    reader.config_mut().trim_text(true);
    let mut in_doi = false;
    let mut found = BTreeSet::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                in_doi = event.local_name().as_ref().eq_ignore_ascii_case(b"doi")
            }
            Ok(Event::Text(text)) if in_doi => {
                let value = text.decode().context("invalid text in arXiv response")?;
                found.extend(find_dois(&value));
            }
            Ok(Event::End(event)) if event.local_name().as_ref().eq_ignore_ascii_case(b"doi") => {
                in_doi = false
            }
            Ok(Event::Eof) => break,
            Err(error) => return Err(error).context("invalid arXiv Atom response"),
            _ => {}
        }
    }
    Ok(found)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_and_normalizes_doi_from_urls() {
        let report =
            resolve_url("https://publisher.example/doi/10.1234/ABC.Def?download=1").unwrap();
        let candidate = report.exact_candidate().unwrap();
        assert_eq!(candidate.kind, "doi");
        assert_eq!(candidate.value, "10.1234/abc.def");
        assert_eq!(candidate.confidence, ResolutionConfidence::Exact);
    }

    #[test]
    fn html_metadata_reports_conflicting_dois() {
        let found = dois_from_html(
            r#"<meta name="citation_doi" content="10.1000/one">
               <script type="application/ld+json">{"doi":"10.1000/two"}</script>"#,
        )
        .unwrap();
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|(doi, _)| doi == "10.1000/one"));
        assert!(found.iter().any(|(doi, _)| doi == "10.1000/two"));
    }

    #[test]
    fn finds_identifiers_in_direct_urls_without_network_access() {
        let identifiers = identifiers_in_url("https://doi.org/10.5555/Example");
        assert!(identifiers.contains(&("doi".into(), "10.5555/example".into())));
    }

    #[test]
    fn rejects_private_network_targets() {
        let error = resolve_url("http://127.0.0.1/paper").unwrap_err();
        assert!(error.to_string().contains("non-public address"));
    }
}
