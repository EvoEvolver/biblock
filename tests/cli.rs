use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::thread;

use assert_cmd::Command;
use biblock_cli::history;
use predicates::prelude::*;

const SAMPLE: &str = r#"% retained comment
@string{conf = {Great Conf}}

@article{alpha,
  title = {Alpha},
  journal = conf,
  year = {2026},
}

@book{beta,
  title = {Beta},
  year = {2025},
}
"#;

#[test]
fn frozen_provider_proposals_require_review_and_adopt_offline_as_api_verified() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("refs.bib");
    let original =
        "@article{one, title={Example work}, author={Doe, Jane}, note={Local note}, pages={1--2}}";
    fs::write(&file, original).unwrap();
    history::sync(&file, None, false, false, None).unwrap();
    history::approve(&file, &BTreeSet::from(["one".into()]), Some("Alice")).unwrap();
    let before_proposal = fs::read_to_string(&file).unwrap();
    let base = mock_crossref_many(vec![
        r#"{"message":{"items":[{"DOI":"10.1234/test","type":"journal-article","title":["Example work"],"author":[{"family":"Doe","given":"Jane"}]}]}}"#,
        r#"{"message":{"DOI":"10.1234/test","type":"journal-article","title":["Example work"],"author":[{"family":"Doe","given":"Jane"}],"published":{"date-parts":[[2026]]}}}"#,
    ]);
    let output = Command::cargo_bin("biblock")
        .unwrap()
        .env("BIBLOCK_CROSSREF_API_BASE", base)
        .args(["source", "propose"])
        .arg(&file)
        .args(["--key", "one", "--id", "10.1234/test", "--agent", "codex"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let proposal: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let id = proposal["id"].as_str().unwrap();
    assert_eq!(proposal["agentAdoptable"], true);
    assert_eq!(proposal["matchScore"]["score"], 1.0);
    assert_eq!(fs::read_to_string(&file).unwrap(), before_proposal);
    assert_eq!(proposal["after"]["fields"]["note"], "Local note");
    assert!(proposal["after"]["fields"]["pages"].is_null());
    let mut tampered = biblock_cli::proposal::list(&file)
        .unwrap()
        .remove(0)
        .proposal;
    tampered
        .after
        .fields
        .insert("title".into(), "Invented replacement".into());
    assert!(tampered.validate(id).is_err());
    fs::write(
        &file,
        "@article{one, title={External edit}, author={Doe, Jane}}",
    )
    .unwrap();
    assert!(biblock_cli::proposal::adopt(&file, id, "codex", true).is_err());
    fs::write(&file, &before_proposal).unwrap();
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["proposal", "adopt"])
        .arg(&file)
        .args(["--id", id, "--agent", "codex"])
        .assert()
        .failure();
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["diagnosis"])
        .arg(&file)
        .assert()
        .code(3)
        .stdout(predicate::str::contains("--reviewed"));
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["proposal", "adopt"])
        .arg(&file)
        .args(["--id", id, "--agent", "codex", "--reviewed"])
        .assert()
        .success();
    let lock = read_lock(&file);
    assert!(lock["entries"]["one"]["approval"].is_null());
    assert_eq!(lock["entries"]["one"]["provider"], "crossref");
    assert_eq!(lock["proposals"][id]["decision"]["agent"], "codex");
    assert!(lock["proposals"][id]["decision"]["reviewer"].is_null());
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["diagnosis"])
        .arg(&file)
        .assert()
        .success();
    assert!(biblock_cli::proposal::adopt(&file, id, "codex", true).is_err());
}

#[test]
fn ambiguous_and_low_score_provider_proposals_require_human_review() {
    for (title, ambiguous) in [("Example work", true), ("Different subject", false)] {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("refs.bib");
        fs::write(
            &file,
            format!("@article{{one, title={{{title}}}, author={{Doe, Jane}}}}"),
        )
        .unwrap();
        history::sync(&file, None, false, false, None).unwrap();
        let search = if ambiguous {
            r#"{"message":{"items":[{"DOI":"10.1234/test","title":["Example work"],"author":[{"family":"Doe","given":"Jane"}]},{"DOI":"10.1234/other","title":["Example work"],"author":[{"family":"Doe","given":"Jane"}]}]}}"#
        } else {
            r#"{"message":{"items":[{"DOI":"10.1234/test","title":["Example work"],"author":[{"family":"Doe","given":"Jane"}]}]}}"#
        };
        let base = mock_crossref_many(vec![
            search,
            r#"{"message":{"DOI":"10.1234/test","title":["Example work"],"author":[{"family":"Doe","given":"Jane"}]}}"#,
        ]);
        let output = Command::cargo_bin("biblock")
            .unwrap()
            .env("BIBLOCK_CROSSREF_API_BASE", base)
            .args(["source", "propose"])
            .arg(&file)
            .args(["--key", "one", "--id", "10.1234/test", "--agent", "codex"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let proposal: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(proposal["agentAdoptable"], false);
        let id = proposal["id"].as_str().unwrap();
        assert!(biblock_cli::proposal::adopt(&file, id, "codex", true).is_err());
        biblock_cli::proposal::decide(&file, id, true, Some("Alice")).unwrap();
        Command::cargo_bin("biblock")
            .unwrap()
            .args(["diagnosis"])
            .arg(&file)
            .assert()
            .success();
    }
}

#[test]
fn agent_proposal_is_lock_only_and_human_adoption_is_reversible() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("refs.bib");
    let original = "@article{one, title={Old}, pages={1--2}, year={2020}}\n";
    fs::write(&file, original).unwrap();
    history::sync(&file, None, false, false, None).unwrap();
    let replacement = directory.path().join("new.bib");
    fs::write(
        &replacement,
        "@book{different, title={New}, author={Doe, Jane}, year={2026}}",
    )
    .unwrap();
    let output = Command::cargo_bin("biblock")
        .unwrap()
        .args(["proposal", "create"])
        .arg(&file)
        .args([
            "--key",
            "one",
            "--agent",
            "codex",
            "--reason",
            "Corrected against publisher",
        ])
        .arg("--bibtex")
        .arg(&replacement)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let proposal: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let id = proposal["id"].as_str().unwrap();
    assert!(!proposal["agentAdoptable"].as_bool().unwrap());
    assert!(biblock_cli::proposal::adopt(&file, id, "codex", true).is_err());
    assert_eq!(fs::read_to_string(&file).unwrap(), original);
    assert!(read_lock(&file)["entries"]["one"]["approval"].is_null());
    assert!(
        proposal["comparison"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["field"] == "pages" && change["proposed"].is_null())
    );
    assert_eq!(proposal["after"]["entry_key"], "one");
    biblock_cli::proposal::decide(&file, id, true, Some("Alice")).unwrap();
    let lock = read_lock(&file);
    assert_eq!(lock["proposals"][id]["decision"]["outcome"], "adopted");
    assert_eq!(lock["entries"]["one"]["approval"]["reviewer"], "Alice");
    assert!(!fs::read_to_string(&file).unwrap().contains("bibapproval"));
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["diagnosis"])
        .arg(&file)
        .assert()
        .success();
    assert!(biblock_cli::proposal::decide(&file, id, true, None).is_err());
    let revisions = history::log(&file, None, "one").unwrap();
    assert_eq!(revisions[0].operation, "proposal-adopt");
    assert_eq!(
        lock["proposals"][id]["decision"]["revision"],
        revisions[0].id
    );
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["history", "restore"])
        .arg(&file)
        .args(["--revision", &revisions[0].id, "--in-place"])
        .assert()
        .success();
    assert!(fs::read_to_string(&file).unwrap().contains("Old"));
    assert_eq!(
        read_lock(&file)["proposals"][id]["decision"]["outcome"],
        "adopted"
    );
}

#[test]
fn stale_proposals_cannot_be_adopted_and_rejections_are_retained() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("refs.bib");
    fs::write(&file, "@article{one, title={Old}}").unwrap();
    history::sync(&file, None, false, false, None).unwrap();
    history::approve(&file, &BTreeSet::from(["one".to_owned()]), Some("Alice")).unwrap();
    let proposal = biblock_cli::proposal::create(
        &file,
        "one",
        "@book{x, title={New}}",
        "codex",
        "Correction",
        serde_json::json!({"url":"https://example.org"}),
    )
    .unwrap();
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["diagnosis"])
        .arg(&file)
        .assert()
        .code(3);
    fs::write(&file, "@article{one, title={External change}}").unwrap();
    history::sync(&file, None, false, false, None).unwrap();
    assert!(biblock_cli::proposal::list(&file).unwrap()[0].stale);
    assert!(biblock_cli::proposal::decide(&file, &proposal.id, true, None).is_err());
    biblock_cli::proposal::decide(&file, &proposal.id, false, Some(" ")).unwrap();
    let lock = read_lock(&file);
    assert_eq!(
        lock["proposals"][&proposal.id]["decision"]["outcome"],
        "rejected"
    );
    assert!(lock["proposals"][&proposal.id]["decision"]["reviewer"].is_null());
    assert!(
        fs::read_to_string(&file)
            .unwrap()
            .contains("External change")
    );
}

#[test]
fn history_checks_heads_and_preserves_deleted_entries_for_restore() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("refs.bib");
    fs::write(
        &file,
        "@article{one, title={One}}\n@article{two, title={Two}}\n",
    )
    .unwrap();
    history::sync(&file, None, false, false, None).unwrap();
    history::approve(
        &file,
        &BTreeSet::from(["one".to_owned(), "two".to_owned()]),
        Some("Alice"),
    )
    .unwrap();
    let valid_lock = fs::read_to_string(lock_path(&file)).unwrap();
    let mut tampered = read_lock(&file);
    tampered["entries"]["one"]["head"] = "rev:deadbeef".into();
    fs::write(lock_path(&file), serde_json::to_string(&tampered).unwrap()).unwrap();
    assert!(!history::status(&file, None).unwrap().valid);
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["diagnosis"])
        .arg(&file)
        .assert()
        .code(3);
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(&file)
        .assert()
        .code(3);
    fs::write(lock_path(&file), valid_lock).unwrap();
    fs::write(&file, "@article{two, title={Two}}\n").unwrap();
    history::sync(&file, None, false, false, Some("agent")).unwrap();
    assert!(history::status(&file, None).unwrap().valid);
    assert!(!read_lock(&file)["deletedEntries"]["one"].is_null());
    let revisions = history::log(&file, None, "one").unwrap();
    assert_eq!(revisions.len(), 2);
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["history", "restore"])
        .arg(&file)
        .args(["--revision", &revisions[0].id, "--in-place"])
        .assert()
        .success();
    assert!(read_lock(&file)["deletedEntries"]["one"].is_null());
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(&file)
        .assert()
        .success();
}

#[test]
fn provider_apply_supersedes_human_approval_and_verify_keeps_current_approvals() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("refs.bib");
    fs::write(&file, "@article{one, title={Old}, author={Doe, Jane}}").unwrap();
    history::sync(&file, None, false, false, None).unwrap();
    history::approve(&file, &BTreeSet::from(["one".to_owned()]), Some("Alice")).unwrap();
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["source", "verify"])
        .arg(&file)
        .args(["--all", "--in-place"])
        .assert()
        .success()
        .stdout(predicate::str::contains("already-verified"));
    let base = mock_crossref_many(vec![
        r#"{"message":{"DOI":"10.1234/test","type":"journal-article","title":["New"],"author":[{"family":"Doe","given":"Jane"}]}}"#,
    ]);
    Command::cargo_bin("biblock")
        .unwrap()
        .env("BIBLOCK_CROSSREF_API_BASE", base)
        .args(["source", "apply"])
        .arg(&file)
        .args([
            "--key",
            "one",
            "--provider",
            "crossref",
            "--id",
            "10.1234/test",
            "--add-integrity",
            "--in-place",
        ])
        .assert()
        .success();
    assert!(read_lock(&file)["entries"]["one"]["approval"].is_null());
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(&file)
        .assert()
        .success();
}

#[test]
fn verification_previews_removals_and_rejects_changed_reviewed_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("refs.bib");
    let original = "@article{one, title={Old}, doi={10.1234/test}, pages={1--2}}";
    fs::write(&file, original).unwrap();
    let base = mock_crossref_many(vec![
        r#"{"message":{"DOI":"10.1234/test","type":"journal-article","title":["New"]}}"#,
        r#"{"message":{"DOI":"10.1234/test","type":"journal-article","title":["Changed again"]}}"#,
    ]);
    let output = Command::cargo_bin("biblock")
        .unwrap()
        .env("BIBLOCK_CROSSREF_API_BASE", &base)
        .args(["source", "verify"])
        .arg(&file)
        .args(["--all", "--providers", "crossref"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert!(
        report[0]["candidates"][0]["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["field"] == "pages" && change["proposed"].is_null())
    );
    let preview = directory.path().join("preview.json");
    fs::write(&preview, output).unwrap();
    Command::cargo_bin("biblock")
        .unwrap()
        .env("BIBLOCK_CROSSREF_API_BASE", &base)
        .args(["source", "verify"])
        .arg(&file)
        .args([
            "--all",
            "--providers",
            "crossref",
            "--in-place",
            "--reviewed",
        ])
        .arg(&preview)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("changed since dry run"));
    assert_eq!(fs::read_to_string(&file).unwrap(), original);
    assert!(!lock_path(&file).exists());
}

#[test]
fn verify_resolves_eprint_and_openreview_can_search() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("refs.bib");
    fs::write(
        &file,
        "@article{one, title={Paper}, author={Doe, Jane}, eprint={2301.12345}}",
    )
    .unwrap();
    let base = mock_crossref_many(vec![
        r#"{"message":{"DOI":"10.48550/arXiv.2301.12345","type":"journal-article","title":["Paper"],"author":[{"family":"Doe","given":"Jane"}]}}"#,
    ]);
    Command::cargo_bin("biblock")
        .unwrap()
        .env("BIBLOCK_CROSSREF_API_BASE", base)
        .args(["source", "verify"])
        .arg(&file)
        .args(["--all", "--providers", "crossref"])
        .assert()
        .success()
        .stdout(predicate::str::contains("10.48550/arXiv.2301.12345"));
    fs::write(&file, "@article{one, title={Paper}, author={Doe, Jane}}").unwrap();
    let base = mock_crossref_many(vec![
        r#"{"notes":[{"id":"note1","content":{"title":{"value":"Paper"},"authors":{"value":["Jane Doe"]},"year":{"value":"2026"}}}]}"#,
    ]);
    Command::cargo_bin("biblock")
        .unwrap()
        .env("BIBLOCK_OPENREVIEW_API_BASE", base)
        .args(["source", "verify"])
        .arg(&file)
        .args(["--all", "--providers", "openreview"])
        .assert()
        .code(3)
        .stdout(predicate::str::contains("needs-selection"));
}

#[test]
fn browser_approval_is_content_bound_and_preserves_clean_bibtex() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("refs.bib");
    fs::write(&file, "@article{one, title={One}, year={2026}}\n").unwrap();
    history::sync(&file, None, false, false, None).unwrap();
    history::approve(
        &file,
        &BTreeSet::from(["one".to_owned()]),
        Some("reviewer label"),
    )
    .unwrap();

    let clean = fs::read_to_string(&file).unwrap();
    assert!(!clean.contains("bibapproval"));
    let lock = read_lock(&file);
    assert_eq!(lock["entries"]["one"]["approval"]["kind"], "human");
    assert_eq!(
        lock["entries"]["one"]["approval"]["reviewer"],
        "reviewer label"
    );
    assert_eq!(lock["revisions"].as_object().unwrap().len(), 1);

    let hydrated = history::hydrate_file(&file).unwrap();
    let records = biblock_cli::bibtex::parse(&hydrated).unwrap();
    assert_eq!(
        biblock_cli::integrity::status(&records[0], &records).unwrap(),
        biblock_cli::integrity::Status::Verified
    );

    fs::write(&file, clean.replace("One", "Changed")).unwrap();
    let hydrated = history::hydrate_file(&file).unwrap();
    let records = biblock_cli::bibtex::parse(&hydrated).unwrap();
    assert_eq!(
        biblock_cli::integrity::status(&records[0], &records).unwrap(),
        biblock_cli::integrity::Status::Stale
    );
}

#[test]
fn blank_browser_reviewer_is_allowed_and_integrity_remove_clears_approval() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("refs.bib");
    fs::write(&file, "@article{one, title={One}}\n").unwrap();
    history::sync(&file, None, false, false, None).unwrap();
    history::approve(&file, &BTreeSet::from(["one".to_owned()]), Some("   ")).unwrap();
    assert!(read_lock(&file)["entries"]["one"]["approval"]["reviewer"].is_null());

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "remove"])
        .arg(&file)
        .args(["--all", "--in-place"])
        .assert()
        .success();
    assert!(read_lock(&file)["entries"]["one"]["approval"].is_null());
}

fn lock_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.lock",
        path.file_name().unwrap().to_string_lossy()
    ))
}

fn read_lock(path: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(lock_path(path)).unwrap()).unwrap()
}

#[test]
fn diagnosis_reports_pending_entries_and_commands_without_writing() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("research notes.bib");
    fs::write(&file, "@article{one, title={One}, author={An Author}}\n").unwrap();
    let before = fs::read_to_string(&file).unwrap();
    let output = Command::cargo_bin("biblock")
        .unwrap()
        .arg("diagnosis")
        .arg(&file)
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["ready"], false);
    assert_eq!(report["network_checked"], false);
    assert_eq!(report["lock"]["state"], "unlocked");
    assert_eq!(report["summary"]["invalid"], 1);
    assert_eq!(report["pending"][0]["key"], "one");
    assert_eq!(
        report["pending"][0]["commands"][0]["argv"][3],
        file.to_str().unwrap()
    );
    assert!(
        report["pending"][0]["commands"][0]["command"]
            .as_str()
            .unwrap()
            .contains("'")
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), before);
    assert!(!lock_path(&file).exists());
}

#[test]
fn diagnosis_requires_current_approval_and_a_valid_lock() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("refs.bib");
    fs::write(&file, "@article{one, title={One}}\n").unwrap();
    history::sync(&file, None, false, false, None).unwrap();
    history::approve(&file, &BTreeSet::from(["one".to_owned()]), Some("reviewer")).unwrap();
    let output = Command::cargo_bin("biblock")
        .unwrap()
        .arg("diagnosis")
        .arg(&file)
        .arg("--compact")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["ready"], true);
    assert_eq!(report["summary"]["verified"], 1);
    assert!(report["pending"].as_array().unwrap().is_empty());
    fs::write(&file, "@article{one, title={Changed}}\n").unwrap();
    let output = Command::cargo_bin("biblock")
        .unwrap()
        .arg("diagnosis")
        .arg(&file)
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["ready"], false);
    assert_eq!(report["lock"]["state"], "stale");
    assert_eq!(report["summary"]["stale"], 1);
}

#[test]
fn diagnosis_reports_unreadable_evidence_and_rejects_duplicate_keys() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("refs.bib");
    fs::write(&file, "@article{one, title={One}}\n").unwrap();
    fs::write(lock_path(&file), "not JSON").unwrap();
    let output = Command::cargo_bin("biblock")
        .unwrap()
        .arg("diagnosis")
        .arg(&file)
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["lock"]["state"], "invalid");
    assert_eq!(report["summary"]["invalid"], 1);
    assert!(!report["lock"]["errors"].as_array().unwrap().is_empty());
    assert!(!report["actions"].as_array().unwrap().iter().any(|action| {
        action["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|arg| arg == "--sync")
    }));
    assert_eq!(fs::read_to_string(lock_path(&file)).unwrap(), "not JSON");
    fs::write(&file, "@article{one,title={A}} @article{one,title={B}}").unwrap();
    Command::cargo_bin("biblock")
        .unwrap()
        .arg("diagnosis")
        .arg(&file)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("duplicate citation key"));
}

#[test]
fn diagnosis_batch_commands_select_only_unverified_keys() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("refs.bib");
    fs::write(
        &file,
        "@article{approved,title={Approved}} @article{pending,title={Pending},author={An Author}}",
    )
    .unwrap();
    history::sync(&file, None, false, false, None).unwrap();
    history::approve(&file, &BTreeSet::from(["approved".to_owned()]), None).unwrap();
    let output = Command::cargo_bin("biblock")
        .unwrap()
        .arg("diagnosis")
        .arg(&file)
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["summary"]["verified"], 1);
    assert_eq!(report["summary"]["invalid"], 1);
    let batch = report["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["argv"][2] == "verify")
        .unwrap();
    let argv = batch["argv"].as_array().unwrap();
    assert!(argv.iter().any(|argument| argument == "pending"));
    assert!(
        !argv
            .iter()
            .any(|argument| argument == "approved" || argument == "--all")
    );
}

#[test]
fn diagnosis_without_a_file_discovers_bibliographies_and_keeps_per_file_errors() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("a.bib"), "@article{one,title={One}}").unwrap();
    fs::write(
        directory.path().join("b.bib"),
        "@article{one,title={Another}}",
    )
    .unwrap();
    fs::write(
        directory.path().join("ignored.bib.lock"),
        "not a bibliography",
    )
    .unwrap();
    let output = Command::cargo_bin("biblock")
        .unwrap()
        .current_dir(directory.path())
        .arg("diagnosis")
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["files"].as_array().unwrap().len(), 2);
    assert_eq!(report["files"][0]["file"], "./a.bib");
    assert_eq!(report["files"][1]["file"], "./b.bib");
    assert_eq!(report["ready"], false);
    fs::write(directory.path().join("broken.bib"), "@article{").unwrap();
    let output = Command::cargo_bin("biblock")
        .unwrap()
        .current_dir(directory.path())
        .arg("diagnosis")
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["files"].as_array().unwrap().len(), 2);
    assert!(report["errors"]["./broken.bib"].is_string());
}

#[test]
fn diagnosis_without_a_file_explains_an_empty_directory() {
    let directory = tempfile::tempdir().unwrap();
    Command::cargo_bin("biblock")
        .unwrap()
        .current_dir(directory.path())
        .arg("diagnosis")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no .bib files found"));
}

#[test]
fn diagnosis_routes_doi_candidates_and_web_references_to_specific_workflows() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("refs.bib");
    fs::write(&file, r#"
        @article{doi, title={Paper}, author={An Author}, doi={10.1/example}}
        @article{candidate, title={Paper}, author={An Author}}
        @misc{website, title={Product}, author={{Company}}, howpublished={\url{https://example.org}}}
    "#).unwrap();
    let report = biblock_cli::diagnosis::report(&file).unwrap();
    let report = serde_json::to_value(report).unwrap();
    let entries = report["pending"].as_array().unwrap();
    assert_eq!(entries[0]["recommended_path"], "api_verify");
    assert_eq!(entries[0]["commands"][0]["argv"][2], "verify");
    assert_eq!(entries[1]["recommended_path"], "candidate_review");
    let template = &entries[1]["templates"][0];
    assert_eq!(template["writes"], true);
    assert!(template["placeholders"]["PROVIDER_RECORD_ID"].is_string());
    assert!(template["placeholders"]["AGENT_ID"].is_string());
    assert!(
        template["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|arg| arg == "propose")
    );
    assert_eq!(entries[2]["recommended_path"], "human_review");
    assert_eq!(entries[2]["commands"][0]["argv"][1], "review");
    assert!(entries[2]["templates"].as_array().unwrap().is_empty());
    let batch = report["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["argv"][2] == "verify")
        .unwrap();
    assert!(
        !batch["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|arg| arg == "website")
    );
}

#[test]
fn top_level_help_documents_inspect_pipe_and_review_workflow() {
    Command::cargo_bin("biblock")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("INSPECT AND PIPE")
                .and(predicate::str::contains("LITERATURE SOURCES"))
                .and(predicate::str::contains("DEDUPLICATION"))
                .and(predicate::str::contains("SCOPE"))
                .and(predicate::str::contains("INTEGRITY"))
                .and(predicate::str::contains("EXIT STATUS"))
                .and(predicate::str::contains("agentAdoptable"))
                .and(predicate::str::contains("--reviewed"))
                .and(predicate::str::contains(
                    "biblock integrity add refs.bib --keys-from - --source agent --agent MODEL --in-place",
                ))
                .and(predicate::str::contains(
                    "does not provide arbitrary metadata editing",
                )),
        );
}

#[test]
fn inspect_help_defines_external_pipeline_contract() {
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["inspect", "--help"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("one JSON array")
                .and(predicate::str::contains("biblock inspect refs.bib | jq"))
                .and(predicate::str::contains("does not evaluate filters")),
        );
}

#[test]
fn source_help_explains_provider_and_review_workflow() {
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["source", "--help"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("biblock source verify refs.bib --all")
                .and(predicate::str::contains("biblock source plan refs.bib"))
                .and(predicate::str::contains("FILE.lock"))
                .and(predicate::str::contains("never embeds response"))
                .and(predicate::str::contains("biblock source trace"))
                .and(predicate::str::contains("biblock integrity add refs.bib")),
        );
}

#[test]
fn proposal_help_embeds_the_complete_agent_contract() {
    for args in [
        vec!["proposal", "--help"],
        vec!["source", "propose", "--help"],
        vec!["proposal", "adopt", "--help"],
    ] {
        Command::cargo_bin("biblock")
            .unwrap()
            .args(args)
            .assert()
            .success()
            .stdout(
                predicate::str::contains("biblock diagnosis refs.bib")
                    .and(predicate::str::contains("agentAdoptable"))
                    .and(predicate::str::contains("agentAdoptionBlocker"))
                    .and(predicate::str::contains("--reviewed"))
                    .and(predicate::str::contains("never re-fetches"))
                    .and(predicate::str::contains("does not grant API authority"))
                    .and(predicate::str::contains("biblock review refs.bib"))
                    .and(predicate::str::contains("lock --sync")),
            );
    }
}

#[test]
fn provider_apply_preserves_local_content_and_requires_later_review() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(
        &path,
        r#"% retain me
@string{venue = {Old Journal}}
@article{paper1,
  title = {Old title},
  journal = venue,
  file = {/local/paper.pdf},
}
"#,
    )
    .unwrap();
    let base_url = mock_crossref(
        r#"{"message":{"DOI":"10.1234/example","type":"journal-article","title":["Provider title"],"author":[{"given":"Jane","family":"Doe"}],"container-title":["Provider Journal"],"issued":{"date-parts":[[2026]]}}}"#,
    );

    Command::cargo_bin("biblock")
        .unwrap()
        .env("BIBLOCK_CROSSREF_API_BASE", base_url)
        .args(["source", "apply"])
        .arg(&path)
        .args(["--key", "paper1", "--id", "10.1234/example", "--in-place"])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "review it before adding integrity",
        ));

    let output = fs::read_to_string(&path).unwrap();
    assert!(output.starts_with("% retain me\n@string{venue"));
    assert!(output.contains("title = {Provider title}"));
    assert!(output.contains("journal = {Provider Journal}"));
    assert!(output.contains("file = {/local/paper.pdf}"));
    assert!(!output.contains("bibprovider"));
    assert!(!output.contains("bibsource"));
    assert!(!output.contains("integrity ="));
    let lock = read_lock(&path);
    assert_eq!(lock["entries"]["paper1"]["provider"], "crossref");
    assert_eq!(lock["entries"]["paper1"]["providerId"], "10.1234/example");
}

#[test]
fn provider_apply_does_not_write_invalid_bibtex() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    let original = "@article{paper1, title={Original}}\n";
    fs::write(&path, original).unwrap();
    let base_url = mock_crossref(
        r#"{"message":{"DOI":"10.1234/broken","type":"journal-article","title":["Unbalanced } title"]}}"#,
    );

    Command::cargo_bin("biblock")
        .unwrap()
        .env("BIBLOCK_CROSSREF_API_BASE", base_url)
        .args(["source", "apply"])
        .arg(&path)
        .args(["--key", "paper1", "--id", "10.1234/broken", "--in-place"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "provider update produced invalid BibTeX",
        ));

    assert_eq!(fs::read_to_string(path).unwrap(), original);
}

#[test]
fn inspect_emits_pipeline_ready_json_from_stdin() {
    let output = Command::cargo_bin("biblock")
        .unwrap()
        .args(["inspect", "-", "--json", "--compact"])
        .write_stdin(SAMPLE)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let entries: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(entries.as_array().unwrap().len(), 2);
    assert_eq!(entries[0]["id"], "alpha");
    assert_eq!(entries[0]["type"], "article");
    assert_eq!(entries[0]["integrity"]["status"], "invalid");
}

#[test]
fn inspect_rejects_duplicate_keys_in_one_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(
        &path,
        "@article{alpha, title={First}}\n@book{alpha, title={Second}}\n",
    )
    .unwrap();

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["inspect"])
        .arg(&path)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("duplicate citation key: alpha"));
}

#[test]
fn inspect_rejects_duplicate_keys_across_files() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first.bib");
    let second = directory.path().join("second.bib");
    fs::write(&first, "@article{alpha, title={First}}\n").unwrap();
    fs::write(&second, "@book{alpha, title={Second}}\n").unwrap();

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["inspect"])
        .arg(&first)
        .arg(&second)
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("duplicate citation key: alpha")
                .and(predicate::str::contains(first.display().to_string()))
                .and(predicate::str::contains(second.display().to_string())),
        );
}

#[test]
fn dedupe_emits_scored_pairs_for_agent_review() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first.bib");
    let second = directory.path().join("second.bib");
    fs::write(
        &first,
        "@article{smith, title={A Great Paper}, author={Smith, John and Doe, Jane}, year={2025}}\n",
    )
    .unwrap();
    fs::write(
        &second,
        "@article{smith-alt, title={A {Great} Paper}, author={John Smith and Jane Doe}, doi={10.1/example}}\n",
    )
    .unwrap();

    let output = Command::cargo_bin("biblock")
        .unwrap()
        .args(["dedupe"])
        .arg(&first)
        .arg(&second)
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let candidates: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(candidates.as_array().unwrap().len(), 1);
    assert_eq!(candidates[0]["score"], 1.0);
    assert_eq!(candidates[0]["title_score"], 1.0);
    assert_eq!(candidates[0]["author_score"], 1.0);
    assert_eq!(
        candidates[0]["entries"][0]["file"],
        first.display().to_string()
    );
    assert_eq!(candidates[0]["entries"][0]["id"], "smith");
    assert_eq!(candidates[0]["entries"][1]["fields"]["doi"], "10.1/example");
}

#[test]
fn dedupe_succeeds_with_empty_json_when_no_pairs_match() {
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["dedupe", "--compact"])
        .write_stdin(
            "@article{one, title={Alpha}, author={Smith, John}}\n@article{two, title={Beta}, author={Jones, Jane}}\n",
        )
        .assert()
        .success()
        .stdout("[]\n");
}

#[test]
fn integrity_lifecycle_has_scriptable_exit_codes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(&path, SAMPLE).unwrap();

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(&path)
        .args(["--key", "alpha"])
        .assert()
        .code(3)
        .stdout(predicate::str::contains("invalid\talpha"));

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "add"])
        .arg(&path)
        .args([
            "--all",
            "--source",
            "agent",
            "--agent",
            "test-agent",
            "--in-place",
        ])
        .assert()
        .success();

    let sealed = fs::read_to_string(&path).unwrap();
    assert!(sealed.starts_with("% retained comment\n@string"));

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(&path)
        .args(["--key", "alpha"])
        .assert()
        .code(3)
        .stdout(predicate::str::contains("valid\talpha"));

    fs::write(
        &path,
        sealed.replace("title = {Alpha}", "title = {Changed}"),
    )
    .unwrap();
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(&path)
        .args(["--key", "alpha"])
        .assert()
        .code(3)
        .stdout("stale\talpha\tagent\n");
}

#[test]
fn integrity_status_reserves_verified_for_provider_or_human_sources() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(&path, SAMPLE).unwrap();
    let base_url = mock_crossref(
        r#"{"message":{"DOI":"10.1234/example","type":"journal-article","title":["Alpha"],"issued":{"date-parts":[[2026]]}}}"#,
    );

    Command::cargo_bin("biblock")
        .unwrap()
        .env("BIBLOCK_CROSSREF_API_BASE", base_url)
        .args(["source", "apply"])
        .arg(&path)
        .args([
            "--key",
            "alpha",
            "--id",
            "10.1234/example",
            "--add-integrity",
            "--in-place",
        ])
        .assert()
        .success();

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "add"])
        .arg(&path)
        .args([
            "--key",
            "beta",
            "--source",
            "agent",
            "--agent",
            "test-agent",
            "--in-place",
        ])
        .assert()
        .success();

    let output = Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(&path)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(rows[0]["id"], "alpha");
    assert_eq!(rows[0]["status"], "verified");
    assert_eq!(rows[0]["source"]["kind"], "provider");
    assert_eq!(rows[1]["id"], "beta");
    assert_eq!(rows[1]["status"], "valid");
    assert_eq!(rows[1]["source"]["kind"], "agent");

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "add"])
        .arg(&path)
        .args([
            "--key",
            "beta",
            "--source",
            "human",
            "--reviewer",
            "alice",
            "--in-place",
        ])
        .assert()
        .success();

    let output = Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(&path)
        .arg("--json")
        .output()
        .unwrap();
    assert!(output.status.success());
    let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .all(|row| row["status"] == "verified")
    );
    assert_eq!(rows[1]["source"]["kind"], "human");
}

#[test]
fn in_place_edits_create_a_valid_history_chain() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    let history_path = lock_path(&path);
    fs::write(&path, SAMPLE).unwrap();

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "add"])
        .arg(&path)
        .args([
            "--key",
            "alpha",
            "--source",
            "agent",
            "--agent",
            "test-agent",
            "--lock-actor",
            "codex/test",
            "--in-place",
        ])
        .assert()
        .success();

    let edited = fs::read_to_string(&path).unwrap();
    assert!(!edited.contains("bibprevious"));
    assert!(!edited.contains("integrity"));
    assert!(!edited.contains("bibsource"));
    assert!(history_path.exists());
    let ledger = fs::read_to_string(&history_path).unwrap();
    let lock: serde_json::Value = serde_json::from_str(&ledger).unwrap();
    let revision = lock["entries"]["alpha"]["head"].as_str().unwrap();
    assert_eq!(revision.len(), "rev:".len() + 8);
    assert!(lock["revisions"][revision]["revisionSha256"].is_string());
    assert!(lock["revisions"][revision]["snapshotSha256"].is_string());
    assert_eq!(lock["revisions"][revision]["actor"], "codex/test");

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "add"])
        .arg(&path)
        .args([
            "--key",
            "alpha",
            "--source",
            "agent",
            "--agent",
            "test-agent",
            "--in-place",
        ])
        .assert()
        .success();
    assert_eq!(fs::read_to_string(&history_path).unwrap(), ledger);

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(&path)
        .args(["--key", "alpha"])
        .assert()
        .code(3)
        .stdout(predicate::str::contains("valid\talpha"));

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["history", "status"])
        .arg(&path)
        .args(["--json"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("\"valid\": true")
                .and(predicate::str::contains("\"revisions\": 1"))
                .and(predicate::str::contains("\"orphaned\": 0")),
        );

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "remove"])
        .arg(&path)
        .args(["--key", "alpha", "--in-place"])
        .assert()
        .success();

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["history", "log"])
        .arg(&path)
        .args(["--key", "alpha", "--compact"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("integrity-remove")
                .and(predicate::str::contains("integrity-add")),
        );
}

#[test]
fn history_restore_is_itself_reversible() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(&path, SAMPLE).unwrap();

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "add"])
        .arg(&path)
        .args([
            "--key",
            "alpha",
            "--source",
            "agent",
            "--agent",
            "test-agent",
            "--in-place",
        ])
        .assert()
        .success();
    let first = read_lock(&path)["entries"]["alpha"]["head"]
        .as_str()
        .unwrap()
        .to_owned();

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["history", "restore"])
        .arg(&path)
        .args(["--revision", &first, "--in-place"])
        .assert()
        .success();

    let restored = fs::read_to_string(&path).unwrap();
    let records = biblock_cli::bibtex::parse(&restored).unwrap();
    let alpha = records
        .iter()
        .find(|record| record.entry_key == "alpha")
        .unwrap();
    assert!(!alpha.fields.contains_key("integrity"));
    assert!(!alpha.fields.contains_key("bibsource"));
    assert!(!alpha.fields.contains_key("bibprevious"));
    assert_ne!(
        read_lock(&path)["entries"]["alpha"]["head"],
        serde_json::Value::String(first)
    );

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["history", "log"])
        .arg(&path)
        .args(["--key", "alpha", "--compact"])
        .assert()
        .success()
        .stdout(predicate::str::contains("history-restore"));
}

#[test]
fn dry_run_does_not_create_history() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(&path, SAMPLE).unwrap();

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "add"])
        .arg(&path)
        .args([
            "--key",
            "alpha",
            "--source",
            "agent",
            "--agent",
            "test-agent",
        ])
        .assert()
        .success();

    assert_eq!(fs::read_to_string(&path).unwrap(), SAMPLE);
    assert!(!lock_path(&path).exists());
}

#[test]
fn history_status_detects_ledger_tampering() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    let history_path = lock_path(&path);
    fs::write(&path, SAMPLE).unwrap();

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "add"])
        .arg(&path)
        .args([
            "--key",
            "alpha",
            "--source",
            "agent",
            "--agent",
            "test-agent",
            "--in-place",
        ])
        .assert()
        .success();
    let ledger = fs::read_to_string(&history_path).unwrap();
    fs::write(
        &history_path,
        ledger.replace("\"revisionSha256\": \"", "\"revisionSha256\": \"0"),
    )
    .unwrap();

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["history", "status"])
        .arg(&path)
        .assert()
        .code(3)
        .stdout(
            predicate::str::contains("revision")
                .and(predicate::str::contains("hash does not match")),
        );
}

#[test]
fn json_lockfile_supports_sync_frozen_and_external_edits() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(&path, SAMPLE).unwrap();

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["lock"])
        .arg(&path)
        .arg("--frozen")
        .assert()
        .code(3)
        .stdout(predicate::str::contains("\"state\": \"unlocked\""));

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["lock"])
        .arg(&path)
        .args(["--sync", "--actor", "test-human"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"state\": \"locked\""));

    let lock = read_lock(&path);
    assert_eq!(lock["lockfileVersion"], "1.1");
    assert_eq!(lock["toolVersion"], env!("CARGO_PKG_VERSION"));
    assert!(lock["bibliography"]["contentHash"].is_string());
    assert!(lock["entries"]["alpha"]["contentHash"].is_string());
    assert_eq!(fs::read_to_string(&path).unwrap(), SAMPLE);

    fs::write(
        &path,
        SAMPLE.replace("title = {Alpha}", "title = {Changed}"),
    )
    .unwrap();
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["lock"])
        .arg(&path)
        .arg("--frozen")
        .assert()
        .code(3)
        .stdout(predicate::str::contains("\"state\": \"stale\""));

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "add"])
        .arg(&path)
        .args([
            "--key",
            "alpha",
            "--source",
            "agent",
            "--agent",
            "test-agent",
            "--in-place",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("run `biblock lock"));

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["lock"])
        .arg(&path)
        .args(["--sync", "--actor", "test-human"])
        .assert()
        .success();
    let lock = read_lock(&path);
    let head = lock["entries"]["alpha"]["head"].as_str().unwrap();
    assert_eq!(head.len(), "rev:".len() + 8);
    assert_eq!(lock["revisions"][head]["operation"], "lock-sync");
    assert_eq!(lock["revisions"][head]["actor"], "test-human");
}

#[test]
fn sync_migrates_legacy_embedded_workflow_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    let original = "@article{alpha, title={Alpha}}\n";
    let records = biblock_cli::bibtex::parse(original).unwrap();
    let evidence = biblock_cli::provenance::actor_source(
        biblock_cli::provenance::SourceKind::Agent,
        "legacy-agent",
        &records[0],
    )
    .unwrap();
    let with_source =
        biblock_cli::provenance::append_source(original, &evidence, &records).unwrap();
    let linked = biblock_cli::integrity::update_entry_fields(
        &with_source,
        "alpha",
        "article",
        &std::collections::BTreeMap::from([("bibsource".to_owned(), evidence.entry_key)]),
    )
    .unwrap();
    let records = biblock_cli::bibtex::parse(&linked).unwrap();
    let legacy = biblock_cli::integrity::update_source(
        &linked,
        &records,
        &BTreeSet::from(["alpha".to_owned()]),
        false,
    )
    .unwrap();
    fs::write(&path, legacy).unwrap();

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["lock"])
        .arg(&path)
        .arg("--sync")
        .assert()
        .success();

    let clean = fs::read_to_string(&path).unwrap();
    assert!(!clean.contains("bibsource"));
    assert!(!clean.contains("integrity"));
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(&path)
        .assert()
        .code(3)
        .stdout("valid\talpha\tagent\n");

    fs::remove_file(lock_path(&path)).unwrap();
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(&path)
        .assert()
        .code(3)
        .stdout("invalid\talpha\n");
}

#[test]
fn add_requires_an_explicit_review_selection() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(&path, SAMPLE).unwrap();

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "add"])
        .arg(&path)
        .args(["--source", "agent", "--agent", "test-agent"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "pass --key KEY or --all after review",
        ));
}

#[test]
fn integrity_add_requires_explicit_provenance_kind() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(&path, SAMPLE).unwrap();

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "add"])
        .arg(&path)
        .args(["--key", "alpha"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--source <SOURCE>"));
}

#[test]
fn agent_integrity_creates_attributed_source_entry() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(&path, SAMPLE).unwrap();

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "add"])
        .arg(&path)
        .args([
            "--key",
            "alpha",
            "--source",
            "agent",
            "--agent",
            "claude-code/test",
            "--in-place",
        ])
        .assert()
        .success();

    let output = fs::read_to_string(&path).unwrap();
    assert!(!output.contains("bibsource"));
    assert!(!output.contains("integrity"));
    let lock_text = fs::read_to_string(lock_path(&path)).unwrap();
    assert!(lock_text.contains("\"kind\": \"agent\""));
    assert!(lock_text.contains("\"actor\": \"claude-code/test\""));
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(&path)
        .args(["--key", "alpha"])
        .assert()
        .code(3)
        .stdout("valid\talpha\tagent\n");

    let inspected = Command::cargo_bin("biblock")
        .unwrap()
        .args(["inspect"])
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let entries: serde_json::Value = serde_json::from_slice(&inspected).unwrap();
    assert_eq!(entries.as_array().unwrap().len(), 2);
    assert_eq!(entries[0]["id"], "alpha");
    assert_eq!(entries[1]["id"], "beta");

    fs::write(
        lock_path(&path),
        lock_text.replace("claude-code/test", "other-agent"),
    )
    .unwrap();
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(&path)
        .args(["--key", "alpha"])
        .assert()
        .code(3)
        .stdout("invalid\talpha\tagent\n");
}

#[test]
fn integrity_add_reads_agent_selected_keys_from_stdin() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(&path, SAMPLE).unwrap();

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "add"])
        .arg(&path)
        .args([
            "--keys-from",
            "-",
            "--source",
            "agent",
            "--agent",
            "pipeline-agent",
            "--in-place",
        ])
        .write_stdin("alpha\n\n")
        .assert()
        .success();

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(&path)
        .args(["--keys-from", "-"])
        .write_stdin("alpha\n")
        .assert()
        .code(3)
        .stdout("valid\talpha\tagent\n");

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(&path)
        .args(["--key", "beta"])
        .assert()
        .code(3)
        .stdout("invalid\tbeta\n");

    let keys_path = directory.path().join("approved.keys");
    fs::write(&keys_path, "alpha\n").unwrap();
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "remove"])
        .arg(&path)
        .arg("--keys-from")
        .arg(&keys_path)
        .arg("--in-place")
        .assert()
        .success();
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(&path)
        .args(["--key", "alpha"])
        .assert()
        .code(3)
        .stdout("invalid\talpha\tagent\n");
}

#[test]
fn explicit_empty_keys_pipeline_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(&path, SAMPLE).unwrap();

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "add"])
        .arg(&path)
        .args([
            "--keys-from",
            "-",
            "--source",
            "agent",
            "--agent",
            "pipeline-agent",
        ])
        .write_stdin("\n")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no citation keys found in -"));
}

#[test]
fn crossref_pipeline_records_compact_receipt_and_adds_valid_integrity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(
        &path,
        "@article{paper1, doi={10.1234/example}, volume={stale}, pages={1--2}, note={local}}\n",
    )
    .unwrap();
    let body = r#"{"message":{"DOI":"10.1234/example","type":"journal-article","title":["Provider title"],"author":[{"given":"Jane","family":"Doe"}],"issued":{"date-parts":[[2026]]}}}"#;
    let base_url = mock_crossref(body);

    Command::cargo_bin("biblock")
        .unwrap()
        .env("BIBLOCK_CROSSREF_API_BASE", base_url)
        .args(["source", "apply"])
        .arg(&path)
        .args(["--key", "paper1", "--add-integrity", "--in-place"])
        .assert()
        .success();

    let output = fs::read_to_string(&path).unwrap();
    let lock_text = fs::read_to_string(lock_path(&path)).unwrap();
    assert!(lock_text.contains("\"kind\": \"provider\""));
    assert!(lock_text.contains("\"provider\": \"crossref\""));
    assert!(lock_text.contains("application/vnd.crossref-api-message+json"));
    assert!(lock_text.contains("\"responsesha256\""));
    assert!(lock_text.contains("\"projectionsha256\""));
    assert!(!output.contains("bibsource"));
    assert!(!output.contains("integrity"));
    assert!(!output.contains("responseencoding ="));
    assert!(!output.contains("response ="));
    assert!(!output.contains("volume = {stale}"));
    assert!(!output.contains("pages = {1--2}"));
    let parsed = biblock_cli::bibtex::parse(&output).unwrap();
    let paper = parsed
        .iter()
        .find(|record| record.entry_key == "paper1")
        .unwrap();
    assert_eq!(paper.fields.get("note").map(String::as_str), Some("local"));
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(&path)
        .assert()
        .success()
        .stdout("verified\tpaper1\tprovider\n");

    let trace = Command::cargo_bin("biblock")
        .unwrap()
        .args(["source", "trace"])
        .arg(&path)
        .args(["--key", "paper1", "--compact"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let trace: serde_json::Value = serde_json::from_slice(&trace).unwrap();
    assert_eq!(trace["target"], "paper1");
    assert_eq!(trace["provider"]["fields"]["provider"], "crossref");
    assert!(trace["provider"]["fields"].get("response").is_none());

    fs::write(
        lock_path(&path),
        lock_text.replace("\"projectionsha256\": \"", "\"projectionsha256\": \"0"),
    )
    .unwrap();
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(&path)
        .assert()
        .code(3)
        .stdout("invalid\tpaper1\tprovider\n");
}

#[test]
fn verify_batches_exact_dois_into_one_write() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(
        &path,
        "@article{alpha, doi={10.1234/a}}\n@article{beta, doi={10.1234/b}}\n",
    )
    .unwrap();
    let base_url = mock_crossref_many(vec![
        r#"{"message":{"DOI":"10.1234/a","type":"journal-article","title":["Alpha"]}}"#,
        r#"{"message":{"DOI":"10.1234/b","type":"journal-article","title":["Beta"]}}"#,
    ]);

    let output = Command::cargo_bin("biblock")
        .unwrap()
        .env("BIBLOCK_CROSSREF_API_BASE", base_url)
        .args(["source", "verify"])
        .arg(&path)
        .args([
            "--all",
            "--providers",
            "crossref",
            "--in-place",
            "--compact",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .all(|row| row["status"] == "verified")
    );

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(path)
        .assert()
        .success()
        .stdout("verified\talpha\tprovider\nverified\tbeta\tprovider\n");
}

#[test]
fn verify_falls_back_from_crossref_to_doi_provider() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(
        &path,
        "@article{alpha, url={https://arxiv.org/abs/2401.01234}}\n",
    )
    .unwrap();
    let crossref_url = mock_status_response("404 Not Found", "application/json", "{}");
    let doi_url = mock_response(
        "application/x-bibtex",
        "@article{x, title={Raw DOI title}, doi={10.48550/arXiv.2401.01234}}",
    );

    let output = Command::cargo_bin("biblock")
        .unwrap()
        .env("BIBLOCK_CROSSREF_API_BASE", crossref_url)
        .env("BIBLOCK_DOI_API_BASE", doi_url)
        .args(["source", "verify"])
        .arg(&path)
        .args(["--all", "--in-place", "--compact"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(rows[0]["provider"], "doi");
    assert_eq!(rows[0]["status"], "verified");
    let updated = fs::read_to_string(&path).unwrap();
    assert!(!updated.contains("bibprovider"));
    let lock_text = fs::read_to_string(lock_path(&path)).unwrap();
    assert!(lock_text.contains("\"provider\": \"doi\""));
    assert!(lock_text.contains("\"method\": \"arxiv-url\""));
}

#[test]
fn resolves_doi_url_without_network_access() {
    let output = Command::cargo_bin("biblock")
        .unwrap()
        .args([
            "source",
            "resolve",
            "https://publisher.example/article/10.1234/Example",
            "--compact",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["status"], "resolved");
    assert_eq!(report["candidates"][0]["kind"], "doi");
    assert_eq!(report["candidates"][0]["value"], "10.1234/example");
    assert_eq!(report["candidates"][0]["evidence"]["method"], "url-doi");
}

#[test]
fn strip_responses_migrates_legacy_evidence_without_reformatting() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(
        &path,
        "% keep\n@bibsource{receipt, kind={provider}, responseencoding={base64}, response={WA==}, responsesha256={abc}}\n",
    )
    .unwrap();

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["source", "strip-responses"])
        .arg(&path)
        .arg("--in-place")
        .assert()
        .success()
        .stderr(predicate::str::contains("removed 2 legacy response field"));

    let output = fs::read_to_string(&path).unwrap();
    assert!(output.starts_with("% keep\n"));
    assert!(!output.contains("@bibsource"));
    assert!(!output.contains("responseencoding"));
    assert!(!output.contains("response={"));
    let lock_text = fs::read_to_string(lock_path(&path)).unwrap();
    assert!(lock_text.contains("\"responsesha256\": \"abc\""));
    assert!(!lock_text.contains("responseencoding"));
}

#[test]
fn apply_resolves_entry_url_and_records_linked_receipts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(
        &path,
        "@article{paper1, url={https://doi.org/10.1234/Example}, note={local}}\n",
    )
    .unwrap();
    let body = r#"{"message":{"DOI":"10.1234/example","type":"journal-article","title":["Resolved title"],"issued":{"date-parts":[[2026]]}}}"#;
    let base_url = mock_crossref(body);

    Command::cargo_bin("biblock")
        .unwrap()
        .env("BIBLOCK_CROSSREF_API_BASE", base_url)
        .args(["source", "apply"])
        .arg(&path)
        .args(["--key", "paper1", "--add-integrity", "--in-place"])
        .assert()
        .success();

    let output = fs::read_to_string(&path).unwrap();
    assert!(!output.contains("bibsource"));
    let lock_text = fs::read_to_string(lock_path(&path)).unwrap();
    assert!(lock_text.contains("\"kind\": \"resolution\""));
    assert!(lock_text.contains("\"method\": \"url-doi\""));
    assert!(lock_text.contains("\"identifier\": \"10.1234/example\""));
    assert!(lock_text.contains("bibsource:resolution:"));
    assert!(lock_text.contains("\"projectionsha256\""));
    assert!(!output.contains("responseencoding ="));
    assert!(!output.contains("response ="));

    let trace = Command::cargo_bin("biblock")
        .unwrap()
        .args(["source", "trace"])
        .arg(&path)
        .args(["--key", "paper1", "--compact"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let trace: serde_json::Value = serde_json::from_slice(&trace).unwrap();
    assert_eq!(trace["resolution"]["fields"]["method"], "url-doi");
    assert_eq!(
        trace["resolution"]["fields"]["identifier"],
        "10.1234/example"
    );

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(path)
        .assert()
        .success()
        .stdout("verified\tpaper1\tprovider\n");
}

#[test]
fn apply_records_search_selection_and_provider_chain() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(
        &path,
        "@article{paper1, title={Selected title}, author={Doe, Jane}, year={2026}}\n",
    )
    .unwrap();
    let base_url = mock_crossref_many(vec![
        r#"{"message":{"items":[{"score":123.0,"DOI":"10.1234/selected","type":"journal-article","title":["Selected title"],"author":[{"family":"Doe","given":"Jane"}],"issued":{"date-parts":[[2026]]}}]}}"#,
        r#"{"message":{"DOI":"10.1234/selected","type":"journal-article","title":["Selected title"],"author":[{"family":"Doe","given":"Jane"}],"issued":{"date-parts":[[2026]]}}}"#,
    ]);
    let record = biblock_cli::bibtex::parse(&fs::read_to_string(&path).unwrap())
        .unwrap()
        .remove(0);
    let reviewed_hash = biblock_cli::integrity::hash(&record).unwrap();
    let fetched = biblock_cli::providers::record_from_evidence("crossref", "10.1234/selected",
        br#"{"message":{"DOI":"10.1234/selected","type":"journal-article","title":["Selected title"],"author":[{"family":"Doe","given":"Jane"}],"issued":{"date-parts":[[2026]]}}}"#).unwrap();
    let proposal_hash =
        biblock_cli::integrity::hash(&biblock_cli::catalog::proposed_record(&record, &fetched))
            .unwrap();

    Command::cargo_bin("biblock")
        .unwrap()
        .env("BIBLOCK_CROSSREF_API_BASE", base_url)
        .args(["source", "apply"])
        .arg(&path)
        .args([
            "--key",
            "paper1",
            "--id",
            "10.1234/selected",
            "--selected-by",
            "codex",
            "--baseline-hash",
            &reviewed_hash,
            "--proposal-hash",
            &proposal_hash,
            "--add-integrity",
            "--in-place",
        ])
        .assert()
        .success();

    let output = fs::read_to_string(&path).unwrap();
    assert!(!output.contains("bibsource"));
    let lock_text = fs::read_to_string(lock_path(&path)).unwrap();
    assert!(lock_text.contains("\"kind\": \"search\""));
    assert!(lock_text.contains("\"kind\": \"selection\""));
    assert!(lock_text.contains("\"selectedby\": \"codex\""));
    assert!(lock_text.contains("bibsource:selection:"));
    let lock: serde_json::Value = serde_json::from_str(&lock_text).unwrap();
    let search = lock["sources"]
        .as_object()
        .unwrap()
        .values()
        .find(|source| source["kind"] == "search")
        .unwrap();
    assert!(search["candidates"].is_array());

    let trace = Command::cargo_bin("biblock")
        .unwrap()
        .args(["source", "trace"])
        .arg(&path)
        .args(["--key", "paper1", "--compact"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let trace: serde_json::Value = serde_json::from_slice(&trace).unwrap();
    assert_eq!(
        trace["selection"]["fields"]["selectedid"],
        "10.1234/selected"
    );
    assert_eq!(trace["selection"]["fields"]["selectedby"], "codex");
    assert_eq!(trace["search"]["fields"]["provider"], "crossref");

    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(path)
        .assert()
        .success()
        .stdout("verified\tpaper1\tprovider\n");
}

#[test]
fn plan_resolves_entry_url_before_exact_provider_lookup() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(
        &path,
        "@article{paper1, url={https://doi.org/10.1234/Example}}\n",
    )
    .unwrap();
    let base_url = mock_crossref(
        r#"{"message":{"DOI":"10.1234/example","type":"journal-article","title":["Resolved title"]}}"#,
    );

    let output = Command::cargo_bin("biblock")
        .unwrap()
        .env("BIBLOCK_CROSSREF_API_BASE", base_url)
        .args(["source", "plan"])
        .arg(&path)
        .args(["--key", "paper1", "--compact"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(rows[0]["status"], "resolved-exact");
    assert_eq!(
        rows[0]["resolution"]["candidates"][0]["value"],
        "10.1234/example"
    );
    assert_eq!(rows[0]["candidates"][0]["record"]["id"], "10.1234/example");
}

#[test]
fn apply_rejects_provider_record_that_disagrees_with_resolved_doi() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    let original = "@article{paper1, url={https://doi.org/10.1234/Expected}}\n";
    fs::write(&path, original).unwrap();
    let base_url = mock_crossref(
        r#"{"message":{"DOI":"10.1234/different","type":"journal-article","title":["Wrong record"]}}"#,
    );

    Command::cargo_bin("biblock")
        .unwrap()
        .env("BIBLOCK_CROSSREF_API_BASE", base_url)
        .args(["source", "apply"])
        .arg(&path)
        .args(["--key", "paper1", "--add-integrity", "--in-place"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "does not match provider record id",
        ));

    assert_eq!(fs::read_to_string(path).unwrap(), original);
}

#[test]
fn doi_content_negotiation_pipeline_is_provider_backed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(&path, "@article{paper1, doi={10.1234/example}}\n").unwrap();
    let base_url = mock_response(
        "application/x-bibtex",
        "@article{remote, title={DOI title}, author={Doe, Jane}, year={2026}, doi={10.1234/example}}",
    );

    Command::cargo_bin("biblock")
        .unwrap()
        .env("BIBLOCK_DOI_API_BASE", base_url)
        .args(["source", "apply"])
        .arg(&path)
        .args([
            "--key",
            "paper1",
            "--provider",
            "doi",
            "--add-integrity",
            "--in-place",
        ])
        .assert()
        .success();

    let output = fs::read_to_string(&path).unwrap();
    assert!(output.contains("title = {DOI title}"));
    assert!(!output.contains("bibsource"));
    let lock_text = fs::read_to_string(lock_path(&path)).unwrap();
    assert!(lock_text.contains("\"provider\": \"doi\""));
    assert!(lock_text.contains("application/x-bibtex"));
    Command::cargo_bin("biblock")
        .unwrap()
        .args(["integrity", "status"])
        .arg(path)
        .assert()
        .success()
        .stdout("verified\tpaper1\tprovider\n");
}

#[test]
fn apply_extracts_doi_from_a_nonstandard_bibtex_field() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("references.bib");
    fs::write(
        &path,
        "@article{paper1, title={Preprint}, note={https://doi.org/10.48550/arXiv.2404.02060}}\n",
    )
    .unwrap();
    let base_url = mock_response(
        "application/x-bibtex",
        "@article{remote, title={Published preprint}, doi={10.48550/arXiv.2404.02060}}",
    );

    Command::cargo_bin("biblock")
        .unwrap()
        .env("BIBLOCK_DOI_API_BASE", base_url)
        .args(["source", "apply"])
        .arg(&path)
        .args([
            "--key",
            "paper1",
            "--provider",
            "doi",
            "--add-integrity",
            "--in-place",
        ])
        .assert()
        .success();

    let output = fs::read_to_string(&path).unwrap();
    assert!(!output.contains("bibsource"));
    let lock_text = fs::read_to_string(lock_path(&path)).unwrap();
    assert!(lock_text.contains("\"method\": \"bibtex-field\""));
    assert!(lock_text.contains("\"identifierkind\": \"doi\""));
    assert!(lock_text.contains("\"provider\": \"doi\""));
}

fn mock_crossref(body: &'static str) -> String {
    mock_response("application/json", body)
}

fn mock_crossref_many(bodies: Vec<&'static str>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        for body in bodies {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        }
    });
    format!("http://{address}/")
}

fn mock_response(content_type: &'static str, body: &'static str) -> String {
    mock_status_response("200 OK", content_type, body)
}

fn mock_status_response(
    status: &'static str,
    content_type: &'static str,
    body: &'static str,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).unwrap();
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });
    format!("http://{address}/")
}
