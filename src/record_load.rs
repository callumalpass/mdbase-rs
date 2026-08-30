//! Canonical byte-first record loading boundary.

use std::io::Read;
use std::time::UNIX_EPOCH;

use serde_json::{json, Value};

use crate::frontmatter::parser::{
    parse_document_layout, yaml_mapping_to_json, FrontmatterState, ParsedDocumentLayout,
};
use crate::runtime::{OperationContext, ProviderError};
use crate::{Collection, OperationCancellation};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InvalidFrontmatterReason {
    InvalidYaml,
    NonMappingFrontmatter,
}

impl From<InvalidFrontmatterReason> for InvalidRecordReason {
    fn from(reason: InvalidFrontmatterReason) -> Self {
        match reason {
            InvalidFrontmatterReason::InvalidYaml => Self::InvalidYaml,
            InvalidFrontmatterReason::NonMappingFrontmatter => Self::NonMappingFrontmatter,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum InvalidRecordState {
    Frontmatter {
        document: String,
        layout: ParsedDocumentLayout,
        effective_frontmatter: Value,
        reason: InvalidFrontmatterReason,
    },
    InvalidUtf8,
}

impl InvalidRecordState {
    pub(crate) fn reason(&self) -> InvalidRecordReason {
        match self {
            Self::Frontmatter { reason, .. } => (*reason).into(),
            Self::InvalidUtf8 => InvalidRecordReason::InvalidUtf8,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum RecordLoadOutcome {
    Parsed {
        path: String,
        facts: RecordFileFacts,
        document: String,
        layout: ParsedDocumentLayout,
        raw_frontmatter: Value,
        effective_frontmatter: Value,
        type_names: Vec<String>,
    },
    Invalid {
        path: String,
        facts: RecordFileFacts,
        type_names: Vec<String>,
        state: InvalidRecordState,
    },
}

#[derive(Clone, Copy)]
pub(crate) struct ParsedRecordView<'a> {
    pub path: &'a str,
    pub facts: &'a RecordFileFacts,
    pub document: &'a str,
    pub layout: &'a ParsedDocumentLayout,
    pub raw_frontmatter: &'a Value,
    pub effective_frontmatter: &'a Value,
    pub type_names: &'a [String],
}

pub(crate) enum RecordLoadView<'a> {
    Parsed(ParsedRecordView<'a>),
    Invalid(InvalidRecordView<'a>),
}

pub(crate) enum InvalidRecordView<'a> {
    Frontmatter {
        path: &'a str,
        facts: &'a RecordFileFacts,
        document: &'a str,
        layout: &'a ParsedDocumentLayout,
        effective_frontmatter: &'a Value,
        type_names: &'a [String],
        reason: InvalidRecordReason,
    },
    InvalidUtf8 {
        path: &'a str,
        facts: &'a RecordFileFacts,
        type_names: &'a [String],
    },
}

impl RecordLoadOutcome {
    pub(crate) fn view(&self) -> RecordLoadView<'_> {
        match self {
            Self::Parsed {
                path,
                facts,
                document,
                layout,
                raw_frontmatter,
                effective_frontmatter,
                type_names,
            } => RecordLoadView::Parsed(ParsedRecordView {
                path,
                facts,
                document,
                layout,
                raw_frontmatter,
                effective_frontmatter,
                type_names,
            }),
            Self::Invalid {
                path,
                facts,
                type_names,
                state:
                    InvalidRecordState::Frontmatter {
                        document,
                        layout,
                        effective_frontmatter,
                        reason,
                    },
            } => RecordLoadView::Invalid(InvalidRecordView::Frontmatter {
                path,
                facts,
                document,
                layout,
                effective_frontmatter,
                type_names,
                reason: (*reason).into(),
            }),
            Self::Invalid {
                path,
                facts,
                type_names,
                state: InvalidRecordState::InvalidUtf8,
            } => RecordLoadView::Invalid(InvalidRecordView::InvalidUtf8 {
                path,
                facts,
                type_names,
            }),
        }
    }

    pub(crate) fn path(&self) -> &str {
        match self.view() {
            RecordLoadView::Parsed(record) => record.path,
            RecordLoadView::Invalid(InvalidRecordView::Frontmatter { path, .. })
            | RecordLoadView::Invalid(InvalidRecordView::InvalidUtf8 { path, .. }) => path,
        }
    }

    pub(crate) fn parsed(&self) -> Option<ParsedRecordView<'_>> {
        match self.view() {
            RecordLoadView::Parsed(record) => Some(record),
            RecordLoadView::Invalid(_) => None,
        }
    }

    pub(crate) fn invalid(&self) -> Option<InvalidRecordView<'_>> {
        match self.view() {
            RecordLoadView::Parsed(_) => None,
            RecordLoadView::Invalid(record) => Some(record),
        }
    }

    pub(crate) fn reason(&self) -> Option<InvalidRecordReason> {
        match self.view() {
            RecordLoadView::Parsed(_) => None,
            RecordLoadView::Invalid(InvalidRecordView::Frontmatter { reason, .. }) => Some(reason),
            RecordLoadView::Invalid(InvalidRecordView::InvalidUtf8 { .. }) => {
                Some(InvalidRecordReason::InvalidUtf8)
            }
        }
    }

    pub(crate) fn facts(&self) -> &RecordFileFacts {
        match self.view() {
            RecordLoadView::Parsed(record) => record.facts,
            RecordLoadView::Invalid(InvalidRecordView::Frontmatter { facts, .. })
            | RecordLoadView::Invalid(InvalidRecordView::InvalidUtf8 { facts, .. }) => facts,
        }
    }

    pub(crate) fn type_names(&self) -> &[String] {
        match self.view() {
            RecordLoadView::Parsed(record) => record.type_names,
            RecordLoadView::Invalid(InvalidRecordView::Frontmatter { type_names, .. })
            | RecordLoadView::Invalid(InvalidRecordView::InvalidUtf8 { type_names, .. }) => {
                type_names
            }
        }
    }

    pub(crate) fn effective_frontmatter(&self) -> Option<&Value> {
        match self.view() {
            RecordLoadView::Parsed(record) => Some(record.effective_frontmatter),
            RecordLoadView::Invalid(InvalidRecordView::Frontmatter {
                effective_frontmatter,
                ..
            }) => Some(effective_frontmatter),
            RecordLoadView::Invalid(InvalidRecordView::InvalidUtf8 { .. }) => None,
        }
    }

    pub(crate) fn document(&self) -> Option<&str> {
        match self.view() {
            RecordLoadView::Parsed(record) => Some(record.document),
            RecordLoadView::Invalid(InvalidRecordView::Frontmatter { document, .. }) => {
                Some(document)
            }
            RecordLoadView::Invalid(InvalidRecordView::InvalidUtf8 { .. }) => None,
        }
    }

    pub(crate) fn body(&self) -> Option<&str> {
        match self.view() {
            RecordLoadView::Parsed(record) => Some(record.layout.body(record.document)),
            RecordLoadView::Invalid(InvalidRecordView::Frontmatter {
                document, layout, ..
            }) => Some(layout.body(document)),
            RecordLoadView::Invalid(InvalidRecordView::InvalidUtf8 { .. }) => None,
        }
    }

    pub(crate) fn had_bom(&self) -> Option<bool> {
        match self.view() {
            RecordLoadView::Parsed(record) => Some(record.layout.had_bom()),
            RecordLoadView::Invalid(InvalidRecordView::Frontmatter { layout, .. }) => {
                Some(layout.had_bom())
            }
            RecordLoadView::Invalid(InvalidRecordView::InvalidUtf8 { .. }) => None,
        }
    }
}

pub(crate) fn load_record(
    collection: &Collection,
    rel_path: &str,
) -> std::io::Result<RecordLoadOutcome> {
    load_record_no_follow(collection, rel_path)?
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
}

/// Load a record through the collection's capability-relative, no-follow
/// boundary. Missing, non-regular, and symlinked paths are bounded absence;
/// failures that may recover remain errors for the caller to retry.
pub(crate) fn load_record_no_follow(
    collection: &Collection,
    rel_path: &str,
) -> std::io::Result<Option<RecordLoadOutcome>> {
    if let Some(context) = OperationContext::current() {
        return load_record_no_follow_context(collection, rel_path, &context)
            .map_err(std::io::Error::other);
    }
    // Intentional context-free compatibility seam.
    load_record_no_follow_cancellable(
        collection,
        rel_path,
        OperationContext::internal().cancellation(),
    )
}

pub(crate) fn load_record_no_follow_cancellable(
    collection: &Collection,
    rel_path: &str,
    cancellation: &OperationCancellation,
) -> std::io::Result<Option<RecordLoadOutcome>> {
    #[cfg(test)]
    SNAPSHOT_RECORD_LOADS.with(|loads| loads.set(loads.get() + 1));
    crate::operations::open_regular_record_no_follow(collection, rel_path)?
        .map(|file| load_open_record(collection, file, rel_path, cancellation))
        .transpose()
}

/// Budgeted record load used by canonical runtime paths.
pub(crate) fn load_record_no_follow_context(
    collection: &Collection,
    rel_path: &str,
    context: &OperationContext,
) -> Result<Option<RecordLoadOutcome>, ProviderError> {
    #[cfg(test)]
    SNAPSHOT_RECORD_LOADS.with(|loads| loads.set(loads.get() + 1));
    context.check()?;
    crate::operations::open_regular_record_no_follow(collection, rel_path)
        .map_err(|error| {
            ProviderError::CollectionOpen(format!(
                "failed to open collection record '{rel_path}': {error}"
            ))
        })?
        .map(|file| load_open_record_context(collection, file, rel_path, context))
        .transpose()
}

fn load_open_record(
    collection: &Collection,
    mut file: std::fs::File,
    rel_path: &str,
    cancellation: &OperationCancellation,
) -> std::io::Result<RecordLoadOutcome> {
    let before = file.metadata()?;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        cancellation.check().map_err(|_| cancelled_io())?;
        let read = file.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        #[cfg(test)]
        maybe_cancel_read_for_test(cancellation);
    }
    cancellation.check().map_err(|_| cancelled_io())?;
    finish_open_record(collection, file, rel_path, before, bytes)
}

fn load_open_record_context(
    collection: &Collection,
    mut file: std::fs::File,
    rel_path: &str,
    context: &OperationContext,
) -> Result<RecordLoadOutcome, ProviderError> {
    let before = file.metadata().map_err(record_provider_error(rel_path))?;
    context.check_file_bytes(before.len())?;
    let capacity =
        usize::try_from(before.len()).map_err(|_| crate::runtime::CaptureLimitExceeded {
            kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
            limit: usize::MAX as u64,
            attempted: before.len(),
        })?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).map_err(|_| {
        ProviderError::CaptureLimitExceeded(crate::runtime::CaptureLimitExceeded {
            kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
            limit: usize::MAX as u64,
            attempted: before.len(),
        })
    })?;
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        context.check()?;
        let read = file
            .read(&mut chunk)
            .map_err(record_provider_error(rel_path))?;
        if read == 0 {
            break;
        }
        let attempted = u64::try_from(bytes.len())
            .ok()
            .and_then(|value| value.checked_add(read as u64))
            .ok_or({
                ProviderError::CaptureLimitExceeded(crate::runtime::CaptureLimitExceeded {
                    kind: crate::runtime::CaptureLimitKind::ArithmeticOverflow,
                    limit: u64::MAX,
                    attempted: u64::MAX,
                })
            })?;
        context.check_file_bytes(attempted)?;
        context.charge_read(read as u64)?;
        context.charge_retained(read as u64)?;
        bytes.extend_from_slice(&chunk[..read]);
        #[cfg(test)]
        maybe_cancel_read_for_test(context.cancellation());
        context.check()?;
    }
    context.check()?;
    finish_open_record(collection, file, rel_path, before, bytes)
        .map_err(record_provider_error(rel_path))
}

fn finish_open_record(
    collection: &Collection,
    file: std::fs::File,
    rel_path: &str,
    before: std::fs::Metadata,
    bytes: Vec<u8>,
) -> std::io::Result<RecordLoadOutcome> {
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

fn record_provider_error(path: &str) -> impl FnOnce(std::io::Error) -> ProviderError + '_ {
    move |error| {
        ProviderError::CollectionOpen(format!(
            "failed to read collection record '{path}': {error}"
        ))
    }
}

fn cancelled_io() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Interrupted, "record load cancelled")
}

#[cfg(test)]
thread_local! {
    static CANCEL_AFTER_READ_CHUNKS: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static SNAPSHOT_RECORD_LOADS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_snapshot_record_loads_for_test() {
    SNAPSHOT_RECORD_LOADS.with(|loads| loads.set(0));
}

#[cfg(test)]
pub(crate) fn snapshot_record_loads_for_test() -> usize {
    SNAPSHOT_RECORD_LOADS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn cancel_after_read_chunks_for_test(chunks: Option<usize>) {
    CANCEL_AFTER_READ_CHUNKS.with(|remaining| remaining.set(chunks));
}

#[cfg(test)]
fn maybe_cancel_read_for_test(cancellation: &OperationCancellation) {
    CANCEL_AFTER_READ_CHUNKS.with(|remaining| {
        if let Some(value) = remaining.get() {
            if value <= 1 {
                remaining.set(None);
                cancellation.cancel();
            } else {
                remaining.set(Some(value - 1));
            }
        }
    });
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
                type_names: collection.determine_types_for_path_only(rel_path),
                state: InvalidRecordState::InvalidUtf8,
            };
        }
    };
    let layout = parse_document_layout(&document);
    let raw_frontmatter = match layout.frontmatter_state() {
        FrontmatterState::Absent => json!({}),
        FrontmatterState::Mapping(mapping) => yaml_mapping_to_json(mapping),
        FrontmatterState::InvalidYaml
        | FrontmatterState::Null
        | FrontmatterState::NonMapping(_) => {
            let reason = if matches!(layout.frontmatter_state(), FrontmatterState::InvalidYaml) {
                InvalidFrontmatterReason::InvalidYaml
            } else {
                InvalidFrontmatterReason::NonMappingFrontmatter
            };
            let type_names = collection.determine_types_for_path_only(rel_path);
            let empty = json!({});
            let effective_frontmatter = collection
                .coerce_types(&collection.apply_defaults(&empty, &type_names), &type_names);
            return RecordLoadOutcome::Invalid {
                path: rel_path.to_string(),
                facts,
                type_names,
                state: InvalidRecordState::Frontmatter {
                    document,
                    layout,
                    effective_frontmatter,
                    reason,
                },
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
        layout,
        raw_frontmatter,
        effective_frontmatter,
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
    fn parsed_outcome_retains_rewrite_parse_and_bom() {
        let outcome = classify(b"\xef\xbb\xbf---\ntitle: retained\n---\nBody\n");
        let RecordLoadOutcome::Parsed {
            document, layout, ..
        } = outcome
        else {
            panic!("expected parsed record");
        };
        assert!(layout.had_bom());
        assert!(document.starts_with('\u{feff}'));
        assert_eq!(layout.body(&document), "Body\n");
    }

    #[test]
    fn invalid_owned_states_have_closed_borrowed_views() {
        let frontmatter = classify(b"\xef\xbb\xbf---\na: [broken\n---\nBody\n");
        match frontmatter.invalid() {
            Some(InvalidRecordView::Frontmatter {
                document,
                layout,
                reason,
                ..
            }) => {
                assert_eq!(reason, InvalidRecordReason::InvalidYaml);
                assert!(layout.had_bom());
                assert_eq!(layout.body(document), "Body\n");
                assert_eq!(frontmatter.document(), Some(document));
                assert_eq!(frontmatter.body(), Some("Body\n"));
                assert_eq!(frontmatter.had_bom(), Some(true));
            }
            Some(InvalidRecordView::InvalidUtf8 { .. }) | None => {
                panic!("expected authored invalid state")
            }
        }

        let invalid_utf8 = classify(b"bad\xffutf8");
        assert!(matches!(
            invalid_utf8.invalid(),
            Some(InvalidRecordView::InvalidUtf8 { .. })
        ));
        assert_eq!(
            invalid_utf8.reason(),
            Some(InvalidRecordReason::InvalidUtf8)
        );
        assert_eq!(invalid_utf8.document(), None);
        assert_eq!(invalid_utf8.body(), None);
        assert_eq!(invalid_utf8.had_bom(), None);
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
            assert_eq!(outcome.reason(), Some(expected));
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
            let outcome = load_record(&collection, "note.md").unwrap();
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
