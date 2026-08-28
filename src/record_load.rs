//! Canonical byte-first record loading boundary.

use std::io::Read;
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde_json::{json, Value};

use crate::frontmatter::parser::{parse_document, yaml_mapping_to_json, FrontmatterState};
use crate::Collection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InvalidRecordReason {
    InvalidYaml,
    NonMappingFrontmatter,
    InvalidUtf8,
}

impl InvalidRecordReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidYaml => "invalid_yaml",
            Self::NonMappingFrontmatter => "non_mapping_frontmatter",
            Self::InvalidUtf8 => "invalid_utf8",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RecordFileFacts {
    pub revision: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: Option<i64>,
}

#[derive(Debug, Clone)]
pub(crate) enum RecordLoadOutcome {
    Parsed {
        path: String,
        facts: RecordFileFacts,
        document: String,
        raw_frontmatter: Value,
        effective_frontmatter: Value,
        body: String,
        type_names: Vec<String>,
    },
    Invalid {
        path: String,
        facts: RecordFileFacts,
        document: Option<String>,
        type_names: Vec<String>,
        reason: InvalidRecordReason,
    },
}

impl RecordLoadOutcome {
    pub(crate) fn facts(&self) -> &RecordFileFacts {
        match self {
            Self::Parsed { facts, .. } | Self::Invalid { facts, .. } => facts,
        }
    }

    pub(crate) fn document(&self) -> Option<&str> {
        match self {
            Self::Parsed { document, .. } => Some(document),
            Self::Invalid { document, .. } => document.as_deref(),
        }
    }
}

pub(crate) fn load_record(
    collection: &Collection,
    abs_path: &Path,
    rel_path: &str,
) -> std::io::Result<RecordLoadOutcome> {
    load_open_record(collection, std::fs::File::open(abs_path)?, rel_path)
}

/// Load a record through the collection's capability-relative, no-follow
/// boundary. Missing, non-regular, and symlinked paths are bounded absence;
/// failures that may recover remain errors for the caller to retry.
pub(crate) fn load_record_no_follow(
    collection: &Collection,
    rel_path: &str,
) -> std::io::Result<Option<RecordLoadOutcome>> {
    crate::operations::open_regular_record_no_follow(collection.root(), rel_path)?
        .map(|file| load_open_record(collection, file, rel_path))
        .transpose()
}

fn load_open_record(
    collection: &Collection,
    mut file: std::fs::File,
    rel_path: &str,
) -> std::io::Result<RecordLoadOutcome> {
    let before = file.metadata()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
        || bytes.len() as u64 != after.len()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "record changed while it was being loaded",
        ));
    }
    let facts = facts(&bytes, &after);
    Ok(classify_bytes(collection, rel_path, bytes, facts))
}

fn facts(bytes: &[u8], metadata: &std::fs::Metadata) -> RecordFileFacts {
    RecordFileFacts {
        revision: crate::v03::revision(bytes),
        size: bytes.len() as u64,
        mtime_ns: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos() as i64)
            .unwrap_or(0),
        ctime_ns: metadata
            .created()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos() as i64),
    }
}

fn classify_bytes(
    collection: &Collection,
    rel_path: &str,
    bytes: Vec<u8>,
    facts: RecordFileFacts,
) -> RecordLoadOutcome {
    let document = match String::from_utf8(bytes) {
        Ok(document) => document,
        Err(_) => {
            return RecordLoadOutcome::Invalid {
                path: rel_path.to_string(),
                facts,
                document: None,
                type_names: collection.determine_types_for_path_only(rel_path),
                reason: InvalidRecordReason::InvalidUtf8,
            };
        }
    };
    let parsed = parse_document(&document);
    let raw_frontmatter = match parsed.frontmatter_state() {
        FrontmatterState::Absent => json!({}),
        FrontmatterState::Mapping(mapping) => yaml_mapping_to_json(mapping),
        FrontmatterState::InvalidYaml => {
            return RecordLoadOutcome::Invalid {
                path: rel_path.to_string(),
                facts,
                document: Some(document),
                type_names: collection.determine_types_for_path_only(rel_path),
                reason: InvalidRecordReason::InvalidYaml,
            };
        }
        FrontmatterState::Null | FrontmatterState::NonMapping(_) => {
            return RecordLoadOutcome::Invalid {
                path: rel_path.to_string(),
                facts,
                document: Some(document),
                type_names: collection.determine_types_for_path_only(rel_path),
                reason: InvalidRecordReason::NonMappingFrontmatter,
            };
        }
    };
    let type_names = collection.determine_types_for_path(&raw_frontmatter, Some(rel_path));
    let effective_frontmatter = collection.coerce_types(
        &collection.apply_defaults(&raw_frontmatter, &type_names),
        &type_names,
    );
    RecordLoadOutcome::Parsed {
        path: rel_path.to_string(),
        facts,
        document,
        raw_frontmatter,
        effective_frontmatter,
        body: parsed.body,
        type_names,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn fixture() -> Collection {
        let root = tempfile::tempdir().unwrap().keep();
        std::fs::write(root.join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        Collection::open(&root).unwrap()
    }

    fn classify(source: &[u8]) -> RecordLoadOutcome {
        let facts = RecordFileFacts {
            revision: crate::v03::revision(source),
            size: source.len() as u64,
            mtime_ns: 0,
            ctime_ns: None,
        };
        classify_bytes(&fixture(), "note.md", source.to_vec(), facts)
    }

    #[test]
    fn byte_first_loader_matrix_has_closed_reasons_and_byte_revisions() {
        let parsed = [
            b"Body only".as_slice(),
            b"---\n---\nBody".as_slice(),
            b"\xef\xbb\xbf---\ntitle: yes\n---\nBody".as_slice(),
        ];
        for source in parsed {
            let outcome = classify(source);
            assert!(matches!(outcome, RecordLoadOutcome::Parsed { .. }));
            assert_eq!(
                outcome.facts().revision,
                format!("sha256:{:x}", Sha256::digest(source))
            );
        }
        let invalid = [
            (
                b"---\na: 1\na: 2\n---\n".as_slice(),
                InvalidRecordReason::InvalidYaml,
            ),
            (
                b"---\n\ttab: bad\n---\n".as_slice(),
                InvalidRecordReason::InvalidYaml,
            ),
            (
                b"---\na: [broken\n---\n".as_slice(),
                InvalidRecordReason::InvalidYaml,
            ),
            (
                b"---\nnull\n---\n".as_slice(),
                InvalidRecordReason::NonMappingFrontmatter,
            ),
            (
                b"---\n- item\n---\n".as_slice(),
                InvalidRecordReason::NonMappingFrontmatter,
            ),
            (
                b"---\nscalar\n---\n".as_slice(),
                InvalidRecordReason::NonMappingFrontmatter,
            ),
            (
                b"bad\xffutf8.md".as_slice(),
                InvalidRecordReason::InvalidUtf8,
            ),
        ];
        for (source, expected) in invalid {
            let outcome = classify(source);
            match &outcome {
                RecordLoadOutcome::Invalid { reason, .. } => assert_eq!(*reason, expected),
                _ => panic!("expected invalid record"),
            }
            assert_eq!(outcome.facts().revision, crate::v03::revision(source));
        }
    }

    #[cfg(unix)]
    #[test]
    fn opened_bytes_and_metadata_stay_on_one_file_version_during_replacement() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, SystemTime};

        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        let path = root.path().join("note.md");
        let versions = [
            (
                b"first version\n".as_slice(),
                UNIX_EPOCH + Duration::from_secs(10),
            ),
            (
                b"a distinct and longer second version\n".as_slice(),
                UNIX_EPOCH + Duration::from_secs(20),
            ),
        ];
        let write_version = |index: usize| {
            let temporary = root.path().join(format!("note-{index}.tmp"));
            std::fs::write(&temporary, versions[index].0).unwrap();
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&temporary)
                .unwrap();
            file.set_times(std::fs::FileTimes::new().set_modified(versions[index].1))
                .unwrap();
            std::fs::rename(temporary, &path).unwrap();
        };
        write_version(0);
        let collection = Collection::open(root.path()).unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let writer_running = Arc::clone(&running);
        let writer_root = root.path().to_path_buf();
        let writer = std::thread::spawn(move || {
            let mut index = 1;
            while writer_running.load(Ordering::Relaxed) {
                let temporary = writer_root.join(format!("writer-{index}.tmp"));
                std::fs::write(&temporary, versions[index].0).unwrap();
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&temporary)
                    .unwrap();
                file.set_times(std::fs::FileTimes::new().set_modified(versions[index].1))
                    .unwrap();
                std::fs::rename(temporary, writer_root.join("note.md")).unwrap();
                index = 1 - index;
            }
        });
        for _ in 0..500 {
            let outcome = load_record(&collection, &path, "note.md").unwrap();
            let facts = outcome.facts();
            let index = versions
                .iter()
                .position(|(bytes, _)| crate::v03::revision(bytes) == facts.revision)
                .expect("loaded one complete known version");
            assert_eq!(facts.size, versions[index].0.len() as u64);
            let expected_mtime = versions[index]
                .1
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as i64;
            assert_eq!(facts.mtime_ns, expected_mtime);
        }
        running.store(false, Ordering::Relaxed);
        writer.join().unwrap();
    }
}
