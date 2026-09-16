//! Local-only synthetic TaskNotes setup/startup benchmark. See docs/large-collection-benchmark.md.
use mdbase::runtime::{
    ChangeFeedOwnerId, FilesystemRuntime, OperationContext, OperationDeadline, OperationKind,
    OperationRequest,
};
use mdbase::v03::{CollectionSetup, CollectionSetupApplyOptions, OperationResult};
use mdbase::{Collection, OperationCancellation};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    root: PathBuf,
    manifest: PathBuf,
    notes: usize,
    task_percent: usize,
    noise_files: usize,
    noise_kind: String,
    body_bytes: usize,
    noise_bytes: usize,
    deadline_ms: u64,
    page_size: usize,
}

fn main() {
    if let Err(error) = run() {
        emit(json!({"event": "error", "message": error.to_string()}));
        std::process::exit(1);
    }
}

fn emit(value: Value) {
    println!("{value}");
    io::stdout().flush().expect("flush benchmark event");
}

fn memory() -> Value {
    // VmHWM is process-lifetime high water, NOT the peak of the current phase.
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    let kb = |key: &str| -> Option<u64> {
        status.lines().find_map(|line| {
            line.strip_prefix(key)?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
    };
    json!({"rss_kib": kb("VmRSS:"), "process_hwm_kib": kb("VmHWM:")})
}

fn timed<T>(name: &str, work: impl FnOnce() -> Result<T>) -> Result<T> {
    emit(json!({"event": "start", "phase": name, "memory": memory()}));
    let start = Instant::now();
    let result = work();
    emit(json!({
        "event": "finish", "phase": name, "elapsed_ms": start.elapsed().as_secs_f64() * 1000.0,
        "ok": result.is_ok(), "error": result.as_ref().err().map(ToString::to_string),
        "memory": memory(),
    }));
    result
}

fn checked(result: OperationResult) -> Result<Value> {
    if !result.valid {
        return Err(format!("invalid operation: {:?}", result.diagnostics).into());
    }
    Ok(result.result)
}

fn context(ms: u64) -> OperationContext {
    OperationContext::new(
        &OperationCancellation::new(),
        OperationDeadline::after(Duration::from_millis(ms)),
    )
}

fn sha(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn setup(manifest: &Value) -> Result<CollectionSetup> {
    // Same portable engine shape as Connect; no grants, credentials or remote requests.
    let packs = manifest["provisions"]["type_packs"]
        .as_array()
        .ok_or("missing type packs")?;
    let required = manifest["requirements"]["contracts"]
        .as_array()
        .ok_or("missing required contracts")?;
    // Match Connect's required-contract setup selection. Scratchpad packs with
    // no provided contracts are optional and not part of initial authorization.
    let packs = packs
        .iter()
        .filter(|pack| {
            pack["provides"].as_array().is_some_and(|provided| {
                provided.iter().any(|contract| {
                    required
                        .iter()
                        .any(|r| r["id"] == contract["id"] && r["version"] == contract["version"])
                })
            })
        })
        .collect::<Vec<_>>();
    if packs.is_empty() {
        return Err("no type packs provide required contracts".into());
    }
    Ok(serde_json::from_value(json!({
        "application_id": manifest["id"],
        "declaration_digest": sha(&serde_jcs::to_vec(manifest)?),
        "requirements": {"configuration": manifest["requirements"]["configuration"]},
        "provisions": {
            "configuration": manifest["provisions"]["configuration"],
            "type_packs": packs.iter().map(|pack| json!({
                "provision": {"manifest": pack["manifest"], "resources": pack["resources"]},
                "options": {}
            })).collect::<Vec<_>>()
        }
    }))?)
}

fn upgrade(manifest: &Value) -> Result<Value> {
    // Deliberately synthetic managed-contract edit, not a claim about a historical upgrade.
    // Body-only change keeps the contract semantics/bindings valid while changing managed bytes.
    let mut next = manifest.clone();
    let packs = next["provisions"]["type_packs"]
        .as_array_mut()
        .ok_or("missing type packs")?;
    let pack = packs
        .iter_mut()
        .find(|p| p["manifest"]["id"] == "tasknotes.task")
        .ok_or("missing tasknotes.task pack")?;
    let resources = pack["resources"]
        .as_array_mut()
        .ok_or("missing resources")?;
    let resource = resources
        .iter_mut()
        .find(|r| {
            r["source"]
                .as_str()
                .is_some_and(|s| s.starts_with("contracts/"))
        })
        .ok_or("missing task contract")?;
    let source = resource["source"].clone();
    let document = format!(
        "{}\nSynthetic benchmark managed-contract revision.\n",
        resource["document"].as_str().ok_or("missing document")?
    );
    let digest = sha(document.as_bytes());
    resource["document"] = json!(document);
    let entry = pack["manifest"]["resources"]
        .as_array_mut()
        .ok_or("missing manifest resources")?
        .iter_mut()
        .find(|r| r["source"] == source)
        .ok_or("unlisted contract resource")?;
    entry["digest"] = json!(digest);
    pack["manifest"]["version"] = json!("99.0.0");
    Ok(next)
}

fn assess(
    runtime: &FilesystemRuntime,
    setup: &CollectionSetup,
    phase: &str,
    current: bool,
) -> Result<Value> {
    let value = timed(phase, || {
        let value = checked(runtime.provider().with_collection_read(|c| {
            Ok::<_, mdbase::runtime::ProviderError>(c.assess_collection_setup(setup))
        })?)?;
        if value["applicable"] != true || (value["status"] == "current") != current {
            return Err(format!(
                "unexpected assessment status={} applicable={}",
                value["status"], value["applicable"]
            )
            .into());
        }
        if value["introduced_diagnostic_count"].as_u64() != Some(0) {
            return Err(format!(
                "setup introduced {} diagnostics",
                value["introduced_diagnostic_count"]
            )
            .into());
        }
        Ok(value)
    })?;
    emit(
        json!({"event": "assessment", "phase": phase, "status": value["status"],
        "baseline_diagnostic_count": value["baseline_diagnostic_count"],
        "introduced_diagnostic_count": value["introduced_diagnostic_count"]}),
    );
    Ok(value)
}

fn apply(
    runtime: &FilesystemRuntime,
    setup: &CollectionSetup,
    assessment: &Value,
    ms: u64,
    phase: &str,
) -> Result<()> {
    let options: CollectionSetupApplyOptions = serde_json::from_value(json!({
        "expected_assessment_digest": assessment["assessment_digest"],
        "expected_collection_revision": assessment["collection_revision"],
        "expected_provision_digest": assessment["provision_digest"],
    }))?;
    timed(phase, || {
        checked(runtime.execute_with_context(
            &OperationRequest::new(
                OperationKind::ApplyCollectionSetup,
                json!({"setup": setup, "options": options}),
            ),
            &context(ms),
        )?)?;
        Ok(())
    })
}

fn query(
    runtime: &FilesystemRuntime,
    config: &Config,
    expected: usize,
    include_body: bool,
) -> Result<()> {
    let label = if include_body {
        "query_with_bodies"
    } else {
        "query_without_bodies_warm"
    };
    let all_start = Instant::now();
    let mut page = timed(&format!("{label}.first_page"), || {
        Ok(runtime.open_read(
        &OperationRequest::new(OperationKind::Query, json!({
            "types": ["task"], "include_body": include_body, "frontmatter_mode": "effective",
            "timezone": "UTC", "limit": config.page_size,
        })), &context(config.deadline_ms),
    )?)
    })?;
    let mut seen = BTreeSet::new();
    let mut pages = 0;
    let mut response_bytes = 0;
    let lease = page.next.clone();
    loop {
        let result = checked(page.outcome.result)?;
        response_bytes += serde_json::to_vec(&result)?.len();
        for row in result["results"]
            .as_array()
            .ok_or("missing query results")?
        {
            let path = row["path"].as_str().ok_or("missing result path")?;
            if include_body
                && row["body"]
                    .as_str()
                    .is_none_or(|body| body.len() < config.body_bytes)
            {
                return Err(format!("missing/truncated requested body: {path}").into());
            }
            if !include_body && row.get("body").is_some() {
                return Err(format!("unexpected body in metadata-only query: {path}").into());
            }
            if !seen.insert(path.to_string()) {
                return Err(format!("duplicate query path: {path}").into());
            }
        }
        pages += 1;
        let Some(cursor) = page.next else { break };
        page = timed(&format!("{label}.page_{}", pages + 1), || {
            Ok(runtime.read_page(&cursor, &context(config.deadline_ms))?)
        })?;
    }
    if let Some(cursor) = lease {
        runtime.release_read(cursor, &context(config.deadline_ms))?;
    }
    if seen.len() != expected {
        return Err(format!("expected {expected} task rows, received {}", seen.len()).into());
    }
    emit(
        json!({"event": "query_summary", "phase": label, "elapsed_ms": all_start.elapsed().as_secs_f64() * 1000.0,
        "rows": seen.len(), "pages": pages, "response_bytes": response_bytes, "memory": memory()}),
    );
    Ok(())
}

fn write_fixture(
    root: &Path,
    relative: &str,
    bytes: &[u8],
    hashes: &mut Vec<(String, String)>,
) -> Result<()> {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().ok_or("no parent")?)?;
    fs::write(path, bytes)?;
    hashes.push((relative.to_string(), sha(bytes)));
    Ok(())
}

fn run() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: large-collection-benchmark CONFIG.json")?;
    let config: Config = serde_json::from_slice(&fs::read(path)?)?;
    emit(
        json!({"event": "build", "debug_assertions": cfg!(debug_assertions), "package_version": env!("CARGO_PKG_VERSION")}),
    );
    if config.notes == 0
        || config.task_percent > 100
        || config.page_size == 0
        || config.deadline_ms == 0
    {
        return Err("invalid benchmark counts or deadline".into());
    }
    if !matches!(
        config.noise_kind.as_str(),
        "binary" | "json" | "excluded-json"
    ) {
        return Err("noise_kind must be binary, json or excluded-json".into());
    }
    // Fail closed: never reuse or mutate a caller's existing collection.
    fs::create_dir(&config.root)?;
    let manifest: Value = serde_json::from_slice(&fs::read(&config.manifest)?)?;
    let initial = setup(&manifest)?;
    let updated = setup(&upgrade(&manifest)?)?;
    let mut hashes = Vec::new();
    let mut task_count = 0;
    timed("generate_fixture", || {
        let body = "x".repeat(config.body_bytes);
        for i in 0..config.notes {
            let task = (i + 1) * config.task_percent / 100 != i * config.task_percent / 100;
            task_count += usize::from(task);
            let tags = if task { "[task]" } else { "[note]" };
            let text = format!("---\nid: bench-{i:08}\ntitle: Synthetic note {i}\ntags: {tags}\nstatus: open\npriority: normal\ndateCreated: '2026-01-01T00:00:00Z'\ndateModified: '2026-01-01T00:00:00Z'\n---\n{body}\n");
            write_fixture(
                &config.root,
                &format!("notes/{:04}/note-{i:08}.md", i / 100),
                text.as_bytes(),
                &mut hashes,
            )?;
        }
        let noise = if config.noise_kind == "binary" {
            vec![0xff; config.noise_bytes]
        } else {
            serde_json::to_vec(&json!({"synthetic": "x".repeat(config.noise_bytes)}))?
        };
        let extension = if config.noise_kind == "binary" {
            "bin"
        } else {
            "json"
        };
        for i in 0..config.noise_files {
            write_fixture(
                &config.root,
                &format!("noise/{:04}/file-{i:08}.{extension}", i / 100),
                &noise,
                &mut hashes,
            )?;
        }
        Ok(())
    })?;
    timed("initialize_collection", || {
        let mut settings = json!({"timezone": "UTC"});
        if config.noise_kind == "excluded-json" {
            settings["exclude"] = json!(["noise/**", ".mdbase/**"]);
        }
        let result = mdbase::init::init_collection(
            &config.root,
            &json!({"config": {
                "spec_version": "0.3.0", "name": "[test] large collection benchmark", "settings": settings,
            }}),
        );
        if result.get("error").is_some() {
            return Err(format!("init failed: {result}").into());
        }
        Ok(())
    })?;
    timed("collection_open_unprovisioned", || {
        Ok(Collection::open(&config.root).map_err(|e| e.to_string())?)
    })?;
    let runtime = timed("runtime_open_unprovisioned", || {
        Ok(FilesystemRuntime::open(
            &config.root,
            Duration::from_millis(120),
        )?)
    })?;
    timed("change_feed_baseline", || {
        let ctx = context(config.deadline_ms);
        let feed = runtime.open_change_feed(&ChangeFeedOwnerId::generate(), &ctx)?;
        runtime.establish_change_feed_baseline(&feed, &ctx)?;
        Ok(())
    })?;
    let assessment = assess(&runtime, &initial, "assess_install", false)?;
    apply(
        &runtime,
        &initial,
        &assessment,
        config.deadline_ms,
        "apply_install_runtime",
    )?;
    drop(runtime);
    let runtime = timed("runtime_reopen_provisioned", || {
        Ok(FilesystemRuntime::open(
            &config.root,
            Duration::from_millis(120),
        )?)
    })?;
    query(&runtime, &config, task_count, true)?;
    query(&runtime, &config, task_count, false)?;
    assess(&runtime, &initial, "assess_current", true)?;
    let assessment = assess(&runtime, &updated, "assess_upgrade", false)?;
    apply(
        &runtime,
        &updated,
        &assessment,
        config.deadline_ms,
        "apply_upgrade_runtime",
    )?;
    assess(&runtime, &updated, "assess_upgraded_current", true)?;
    drop(runtime);
    timed("verify_fixture_unchanged", || {
        for (path, expected) in &hashes {
            if sha(&fs::read(config.root.join(path))?) != *expected {
                return Err(format!("fixture changed unexpectedly: {path}").into());
            }
        }
        Ok(())
    })?;
    emit(
        json!({"event": "complete", "notes": config.notes, "tasks": task_count,
        "verified_files": hashes.len(), "memory": memory()}),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Value {
        json!({
            "id": "dev.tasknotes.app",
            "requirements": {"contracts": [{"id": "tasknotes.task", "version": "1"}], "configuration": []},
            "provisions": {"configuration": [], "type_packs": [
                {"manifest": {"id": "tasknotes.task", "version": "1.0.0", "resources": [
                    {"source": "contracts/task.md", "digest": sha(b"original")}
                ]}, "resources": [{"source": "contracts/task.md", "document": "original"}],
                "provides": [{"id": "tasknotes.task", "version": "1"}]},
                {"manifest": {"id": "optional.scratch"}, "resources": [], "provides": []}
            ]}
        })
    }

    #[test]
    fn selects_only_required_contract_packs() {
        let setup = setup(&manifest()).unwrap();
        assert_eq!(setup.provisions.type_packs.len(), 1);
        assert_eq!(
            setup.provisions.type_packs[0].provision.manifest["id"],
            "tasknotes.task"
        );
        assert_eq!(setup.declaration_digest.len(), 71);
    }

    #[test]
    fn upgrade_changes_managed_bytes_and_digest_without_mutating_input() {
        let original = manifest();
        let next = upgrade(&original).unwrap();
        let pack = &next["provisions"]["type_packs"][0];
        assert_eq!(pack["manifest"]["version"], "99.0.0");
        let document = pack["resources"][0]["document"].as_str().unwrap();
        assert!(document.starts_with("original\n"));
        assert_eq!(
            pack["manifest"]["resources"][0]["digest"],
            sha(document.as_bytes())
        );
        assert_eq!(
            original["provisions"]["type_packs"][0]["resources"][0]["document"],
            "original"
        );
        assert_ne!(
            setup(&original).unwrap().declaration_digest,
            setup(&next).unwrap().declaration_digest
        );
    }

    #[test]
    fn missing_required_pack_is_an_error_not_a_noop() {
        let mut value = manifest();
        value["requirements"]["contracts"][0]["id"] = json!("absent");
        assert!(setup(&value).is_err());
    }
}
