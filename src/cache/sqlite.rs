//! SQLite setup and helpers.

#[cfg(test)]
use rusqlite::OpenFlags;
use rusqlite::{Connection, Result};
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct CacheDbIdentity(same_file::Handle);

impl CacheDbIdentity {
    pub(crate) fn capture(path: &Path) -> std::io::Result<Self> {
        // Keep the identity handle alive with the SQLite connection. This makes
        // Unix device/inode and Windows volume/file-index comparison robust
        // against path removal, replacement, and identifier reuse.
        same_file::Handle::from_path(path).map(Self)
    }
}

pub(crate) fn cache_db_path(root: &Path, cache_folder: &str) -> PathBuf {
    root.join(cache_folder).join("cache.db")
}

const CACHE_LIFECYCLE_LOCK: &str = "cache.lifecycle.lock";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LifecycleLockMode {
    Shared,
    Exclusive,
}

#[derive(Debug)]
struct LifecycleWaiter {
    id: u64,
    mode: LifecycleLockMode,
}

#[derive(Debug, Default)]
struct LifecycleState {
    shared_owners: usize,
    exclusive_owner: bool,
    next_waiter_id: u64,
    waiters: VecDeque<LifecycleWaiter>,
}

#[derive(Debug)]
struct LifecycleEntry {
    identity: PathBuf,
    state: Mutex<LifecycleState>,
    changed: Condvar,
}

impl LifecycleEntry {
    fn new(identity: PathBuf) -> Self {
        Self {
            identity,
            state: Mutex::new(LifecycleState::default()),
            changed: Condvar::new(),
        }
    }
}

impl Drop for LifecycleEntry {
    fn drop(&mut self) {
        let mut registry = lock_unpoisoned(&LIFECYCLE_REGISTRY);
        let registered_here = registry
            .get(&self.identity)
            .is_some_and(|entry| std::ptr::eq(entry.as_ptr(), self));
        if registered_here {
            registry.remove(&self.identity);
        }
    }
}

#[derive(Debug)]
struct EnqueuedLifecycleWaiter {
    entry: Arc<LifecycleEntry>,
    id: Option<u64>,
}

impl EnqueuedLifecycleWaiter {
    fn new(entry: Arc<LifecycleEntry>) -> Self {
        Self { entry, id: None }
    }

    fn disarm(&mut self) {
        self.id = None;
    }
}

impl Drop for EnqueuedLifecycleWaiter {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else {
            return;
        };
        let mut state = lock_unpoisoned(&self.entry.state);
        if let Some(position) = state.waiters.iter().position(|waiter| waiter.id == id) {
            state.waiters.remove(position);
            self.entry.changed.notify_all();
        }
    }
}

#[derive(Debug)]
struct InProcessLifecycleGuard {
    entry: Arc<LifecycleEntry>,
    mode: LifecycleLockMode,
}

impl Drop for InProcessLifecycleGuard {
    fn drop(&mut self) {
        let mut state = lock_unpoisoned(&self.entry.state);
        match self.mode {
            LifecycleLockMode::Shared => {
                state.shared_owners = state
                    .shared_owners
                    .checked_sub(1)
                    .expect("shared lifecycle owner count underflow");
            }
            LifecycleLockMode::Exclusive => {
                debug_assert!(state.exclusive_owner);
                state.exclusive_owner = false;
            }
        }
        self.entry.changed.notify_all();
    }
}

static LIFECYCLE_REGISTRY: LazyLock<Mutex<HashMap<PathBuf, Weak<LifecycleEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lifecycle_entry(identity: PathBuf) -> Arc<LifecycleEntry> {
    let mut registry = lock_unpoisoned(&LIFECYCLE_REGISTRY);
    // Weak entries avoid keeping unused cache identities alive. The entry's
    // destructor removes its own key when the final owner releases it.
    registry.retain(|_, entry| entry.strong_count() != 0);
    if let Some(entry) = registry.get(&identity).and_then(Weak::upgrade) {
        // An upgraded Arc can become the final owner concurrently. Release the
        // registry before it can be dropped and run LifecycleEntry::drop.
        drop(registry);
        return entry;
    }
    let entry = Arc::new(LifecycleEntry::new(identity.clone()));
    registry.insert(identity, Arc::downgrade(&entry));
    drop(registry);
    entry
}

#[cfg(test)]
fn with_registered_lifecycle_entry<T>(
    identity: &Path,
    operation: impl FnOnce(&LifecycleEntry) -> T,
) -> Option<T> {
    let entry = {
        let registry = lock_unpoisoned(&LIFECYCLE_REGISTRY);
        registry.get(identity).and_then(Weak::upgrade)
    };
    entry.as_deref().map(operation)
}

fn acquire_in_process_lifecycle(
    identity: PathBuf,
    mode: LifecycleLockMode,
    deadline: Instant,
) -> std::io::Result<InProcessLifecycleGuard> {
    let entry = lifecycle_entry(identity);
    // Declare cleanup before the state guard so unwind always releases the
    // mutex before cleanup tries to remove the queued waiter.
    let mut queued = EnqueuedLifecycleWaiter::new(Arc::clone(&entry));
    let mut state = lock_unpoisoned(&entry.state);
    let waiter_id = state.next_waiter_id;
    state.next_waiter_id = waiter_id
        .checked_add(1)
        .ok_or_else(|| std::io::Error::other("cache lifecycle waiter ticket space exhausted"))?;
    queued.id = Some(waiter_id);
    state.waiters.push_back(LifecycleWaiter {
        id: waiter_id,
        mode,
    });

    loop {
        let is_front = state.waiters.front().map(|waiter| waiter.id) == Some(waiter_id);
        let compatible = match mode {
            LifecycleLockMode::Shared => !state.exclusive_owner,
            LifecycleLockMode::Exclusive => !state.exclusive_owner && state.shared_owners == 0,
        };
        if is_front && compatible {
            let waiter = state.waiters.pop_front().expect("front waiter disappeared");
            queued.disarm();
            debug_assert_eq!(waiter.mode, mode);
            match mode {
                LifecycleLockMode::Shared => {
                    state.shared_owners = state
                        .shared_owners
                        .checked_add(1)
                        .expect("shared lifecycle owner count overflow");
                }
                LifecycleLockMode::Exclusive => state.exclusive_owner = true,
            }
            // Let consecutive shared waiters join immediately while preserving
            // FIFO order in front of later exclusive owners.
            entry.changed.notify_all();
            drop(state);
            return Ok(InProcessLifecycleGuard { entry, mode });
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for in-process cache lifecycle lock",
            ));
        }
        let remaining = deadline - now;
        state = entry
            .changed
            .wait_timeout(state, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0;
    }
}

#[derive(Debug)]
pub(crate) struct CacheLifecycleGuard {
    file: File,
    in_process: Option<InProcessLifecycleGuard>,
}

impl Drop for CacheLifecycleGuard {
    fn drop(&mut self) {
        // Release the cross-process side before allowing the next local owner
        // to attempt it.
        let _ = fs2::FileExt::unlock(&self.file);
        drop(self.in_process.take());
    }
}

pub(crate) fn lock_cache_lifecycle_shared(
    root: &Path,
    cache_folder: &str,
    timeout: Duration,
) -> std::io::Result<CacheLifecycleGuard> {
    lock_cache_lifecycle(root, cache_folder, timeout, false)
}

pub(crate) fn lock_cache_lifecycle_exclusive(
    root: &Path,
    cache_folder: &str,
    timeout: Duration,
) -> std::io::Result<CacheLifecycleGuard> {
    lock_cache_lifecycle(root, cache_folder, timeout, true)
}

#[cfg(test)]
pub(crate) fn cache_lifecycle_waiter_count(root: &Path, cache_folder: &str) -> usize {
    let identity = match std::fs::canonicalize(root.join(cache_folder).join(CACHE_LIFECYCLE_LOCK)) {
        Ok(identity) => identity,
        Err(_) => return 0,
    };
    with_registered_lifecycle_entry(&identity, |entry| {
        lock_unpoisoned(&entry.state).waiters.len()
    })
    .unwrap_or(0)
}

fn is_lifecycle_lock_contention(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    // `LockFileEx` reports ERROR_LOCK_VIOLATION, which Rust currently leaves
    // as `Uncategorized` rather than mapping to `WouldBlock`.
    #[cfg(windows)]
    {
        error.raw_os_error() == Some(33)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn lock_cache_lifecycle(
    root: &Path,
    cache_folder: &str,
    timeout: Duration,
    exclusive: bool,
) -> std::io::Result<CacheLifecycleGuard> {
    let deadline = Instant::now() + timeout;
    let cache_root = root.join(cache_folder);
    std::fs::create_dir_all(&cache_root)?;
    let lock_path = cache_root.join(CACHE_LIFECYCLE_LOCK);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    // Canonicalization gives aliases one exact key and preserves the filesystem's
    // canonical spelling/case on Windows. It is intentionally the lock path,
    // not the caller's potentially case-variant cache path.
    let identity = std::fs::canonicalize(&lock_path)?;
    let mode = if exclusive {
        LifecycleLockMode::Exclusive
    } else {
        LifecycleLockMode::Shared
    };
    let in_process = acquire_in_process_lifecycle(identity, mode, deadline)?;
    // Once the OS reports contention, preserve that first platform error (and
    // its raw OS code) if the single end-to-end deadline expires.
    let mut first_contention_error = None;
    loop {
        let result = if exclusive {
            fs2::FileExt::try_lock_exclusive(&file)
        } else {
            fs2::FileExt::try_lock_shared(&file)
        };
        match result {
            Ok(()) => {
                return Ok(CacheLifecycleGuard {
                    file,
                    in_process: Some(in_process),
                });
            }
            Err(error) if is_lifecycle_lock_contention(&error) => {
                if first_contention_error.is_none() {
                    first_contention_error = Some(error);
                }
                let now = Instant::now();
                if now >= deadline {
                    return Err(first_contention_error.expect("lock failure was recorded"));
                }
                std::thread::sleep(Duration::from_millis(10).min(deadline - now));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Open (or create) the cache database at `<root>/<cache_folder>/cache.db`.
pub(crate) fn open_cache_db(root: &Path, cache_folder: &str) -> Result<Connection> {
    let db_dir = root.join(cache_folder);
    std::fs::create_dir_all(&db_dir)
        .map_err(|_e| rusqlite::Error::InvalidPath(db_dir.join("cache.db")))?;
    let db_path = db_dir.join("cache.db");
    let conn = Connection::open(&db_path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
    )?;
    init_schema(&conn)?;
    Ok(conn)
}

/// Test the behavior of opening an existing cache read-only without schema
/// initialization, DDL, or logical cache writes. Production seal validation
/// deliberately reuses its committing connection instead of taking this path.
#[cfg(test)]
pub(crate) fn open_cache_db_read_only_existing(
    root: &Path,
    cache_folder: &str,
) -> Result<Connection> {
    let db_path = cache_db_path(root, cache_folder);
    if !db_path.is_file() {
        return Err(rusqlite::Error::InvalidPath(db_path));
    }
    let connection = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    #[cfg(test)]
    run_read_only_open_hook(&db_path);
    Ok(connection)
}

#[cfg(test)]
type ReadOnlyOpenHooks = std::collections::BTreeMap<PathBuf, Box<dyn FnOnce() + Send>>;

#[cfg(test)]
static READ_ONLY_OPEN_HOOKS: LazyLock<Mutex<ReadOnlyOpenHooks>> =
    LazyLock::new(|| Mutex::new(std::collections::BTreeMap::new()));

#[cfg(test)]
pub(crate) fn set_read_only_open_hook(
    root: &Path,
    cache_folder: &str,
    hook: impl FnOnce() + Send + 'static,
) {
    READ_ONLY_OPEN_HOOKS
        .lock()
        .unwrap()
        .insert(cache_db_path(root, cache_folder), Box::new(hook));
}

#[cfg(test)]
pub(crate) fn clear_read_only_open_hook(root: &Path, cache_folder: &str) -> bool {
    READ_ONLY_OPEN_HOOKS
        .lock()
        .unwrap()
        .remove(&cache_db_path(root, cache_folder))
        .is_some()
}

#[cfg(test)]
fn run_read_only_open_hook(path: &Path) {
    let hook = READ_ONLY_OPEN_HOOKS.lock().unwrap().remove(path);
    if let Some(hook) = hook {
        hook();
    }
}

/// Open an in-memory cache database (for testing).
#[allow(dead_code)]
pub(crate) fn open_cache_db_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    init_schema(&conn)?;
    Ok(conn)
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(include_str!("schema.sql"))?;
    if !table_has_column(conn, "files", "ctime_ns")? {
        conn.execute_batch("ALTER TABLE files ADD COLUMN ctime_ns INTEGER;")?;
    }
    if !table_has_column(conn, "files", "source_revision")? {
        conn.execute_batch(
            "ALTER TABLE files ADD COLUMN source_revision TEXT NOT NULL DEFAULT '';",
        )?;
    }
    if !table_has_column(conn, "files", "failure_reason")? {
        conn.execute_batch("ALTER TABLE files ADD COLUMN failure_reason TEXT;")?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_files_failure_reason ON files(failure_reason) WHERE failure_reason IS NOT NULL;",
    )?;
    if !table_has_column(conn, "links", "source_revision")? {
        conn.execute_batch(
            "ALTER TABLE links ADD COLUMN source_revision TEXT NOT NULL DEFAULT '';",
        )?;
    }
    if !table_has_column(conn, "links", "resolved")? {
        conn.execute_batch("ALTER TABLE links ADD COLUMN resolved INTEGER NOT NULL DEFAULT 0;")?;
    }
    Ok(())
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    for candidate in columns {
        if candidate? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_existing_open_does_not_initialize_or_change_logical_tokens() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join(".mdbase/cache");
        assert!(open_cache_db_read_only_existing(root.path(), ".mdbase/cache").is_err());
        assert!(!cache.exists());

        let connection = open_cache_db(root.path(), ".mdbase/cache").unwrap();
        connection
            .execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('query_snapshot', 'before')",
                [],
            )
            .unwrap();
        let schema_before: i64 = connection
            .query_row("PRAGMA schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_unique_values_path'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        drop(connection);
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(cache.join(format!("cache.db{suffix}")));
        }
        let read_only = open_cache_db_read_only_existing(root.path(), ".mdbase/cache").unwrap();
        assert_eq!(
            read_only
                .query_row(
                    "SELECT value FROM meta WHERE key = 'query_snapshot'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "before"
        );
        assert_eq!(
            read_only
                .query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            schema_before
        );
        drop(read_only);
        let verify = open_cache_db_read_only_existing(root.path(), ".mdbase/cache").unwrap();
        assert_eq!(
            verify
                .query_row(
                    "SELECT value FROM meta WHERE key = 'query_snapshot'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "before"
        );
        assert_eq!(
            verify
                .query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            schema_before
        );
    }

    #[test]
    fn read_only_transaction_holds_one_logical_snapshot_across_live_wal_writer() {
        let root = tempfile::tempdir().unwrap();
        let writer = open_cache_db(root.path(), ".mdbase/cache").unwrap();
        writer
            .execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('query_snapshot', 'first')",
                [],
            )
            .unwrap();
        let mut reader = open_cache_db_read_only_existing(root.path(), ".mdbase/cache").unwrap();
        let read = reader
            .transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)
            .unwrap();
        let first: String = read
            .query_row(
                "SELECT value FROM meta WHERE key = 'query_snapshot'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        writer
            .execute(
                "UPDATE meta SET value = 'second' WHERE key = 'query_snapshot'",
                [],
            )
            .unwrap();
        let coherent: String = read
            .query_row(
                "SELECT value FROM meta WHERE key = 'query_snapshot'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(first, "first");
        assert_eq!(coherent, "first");
        drop(read);
        assert_eq!(
            reader
                .query_row(
                    "SELECT value FROM meta WHERE key = 'query_snapshot'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "second"
        );
    }

    fn wait_until_queued(root: &Path, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while cache_lifecycle_waiter_count(root, ".cache") < expected {
            assert!(Instant::now() < deadline, "lifecycle waiter was not queued");
            std::thread::yield_now();
        }
    }

    #[test]
    fn cache_lifecycle_shared_guards_coexist() {
        let directory = tempfile::tempdir().unwrap();
        let first = lock_cache_lifecycle_shared(directory.path(), ".cache", Duration::from_secs(1))
            .unwrap();
        let root = directory.path().to_path_buf();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            sender
                .send(lock_cache_lifecycle_shared(
                    &root,
                    ".cache",
                    Duration::from_secs(1),
                ))
                .unwrap();
        });

        let second = receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        drop(first);
        drop(second);
        thread.join().unwrap();
    }

    #[test]
    fn cache_lifecycle_exclusive_guards_serialize_fifo() {
        let directory = tempfile::tempdir().unwrap();
        let first =
            lock_cache_lifecycle_exclusive(directory.path(), ".cache", Duration::from_secs(1))
                .unwrap();
        let root = directory.path().to_path_buf();
        let waiter_root = root.clone();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            let guard =
                lock_cache_lifecycle_exclusive(&waiter_root, ".cache", Duration::from_secs(1));
            sender.send(guard).unwrap();
        });

        wait_until_queued(&root, 1);
        assert!(matches!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        drop(first);
        drop(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap(),
        );
        thread.join().unwrap();
    }

    #[test]
    fn cache_lifecycle_wait_timeout_recovers_without_stranding_queue() {
        let directory = tempfile::tempdir().unwrap();
        let shared =
            lock_cache_lifecycle_shared(directory.path(), ".cache", Duration::from_secs(1))
                .unwrap();
        let error =
            lock_cache_lifecycle_exclusive(directory.path(), ".cache", Duration::from_millis(25))
                .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);

        drop(shared);
        drop(
            lock_cache_lifecycle_exclusive(directory.path(), ".cache", Duration::from_secs(1))
                .unwrap(),
        );
    }

    #[test]
    #[ignore = "helper process for cache_lifecycle_cross_process_preserves_os_error"]
    fn cache_lifecycle_cross_process_lock_holder() {
        let Some(root) = std::env::var_os("MDBASE_LIFECYCLE_TEST_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let guard = lock_cache_lifecycle_exclusive(&root, ".cache", Duration::from_secs(1))
            .expect("helper must acquire lifecycle lock");
        File::create(root.join("holder-ready")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !root.join("holder-release").exists() {
            assert!(Instant::now() < deadline, "parent did not release helper");
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(guard);
    }

    #[test]
    fn cache_lifecycle_cross_process_preserves_os_error() {
        let directory = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("cache::sqlite::tests::cache_lifecycle_cross_process_lock_holder")
            .arg("--ignored")
            .arg("--test-threads=1")
            .env("MDBASE_LIFECYCLE_TEST_ROOT", directory.path())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let ready_deadline = Instant::now() + Duration::from_secs(5);
        while !directory.path().join("holder-ready").exists() {
            assert!(
                Instant::now() < ready_deadline,
                "helper did not acquire lock"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        let probe = OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.path().join(".cache").join(CACHE_LIFECYCLE_LOCK))
            .unwrap();
        let expected = fs2::FileExt::try_lock_shared(&probe).unwrap_err();
        let started = Instant::now();
        let error =
            lock_cache_lifecycle_shared(directory.path(), ".cache", Duration::from_millis(40))
                .unwrap_err();
        let elapsed = started.elapsed();
        File::create(directory.path().join("holder-release")).unwrap();
        assert!(child.wait().unwrap().success());

        assert_eq!(error.kind(), expected.kind());
        assert_eq!(error.raw_os_error(), expected.raw_os_error());
        assert!(error.raw_os_error().is_some());
        assert!(elapsed >= Duration::from_millis(40));
        assert!(elapsed < Duration::from_secs(1));
    }

    #[test]
    fn cache_lifecycle_alias_uses_one_local_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let shared =
            lock_cache_lifecycle_shared(directory.path(), ".cache", Duration::from_secs(1))
                .unwrap();
        let alias = directory.path().join(".");
        let started = Instant::now();
        let error = lock_cache_lifecycle_exclusive(&alias, ".cache", Duration::from_millis(40))
            .unwrap_err();
        let elapsed = started.elapsed();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(elapsed >= Duration::from_millis(40));
        assert!(elapsed < Duration::from_secs(1));
        assert_eq!(cache_lifecycle_waiter_count(directory.path(), ".cache"), 0);
        drop(shared);
    }

    #[test]
    fn cache_lifecycle_waiter_ticket_exhaustion_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join(".cache")).unwrap();
        let lock_path = directory.path().join(".cache").join(CACHE_LIFECYCLE_LOCK);
        File::create(&lock_path).unwrap();
        let identity = std::fs::canonicalize(lock_path).unwrap();
        let entry = lifecycle_entry(identity.clone());
        lock_unpoisoned(&entry.state).next_waiter_id = u64::MAX;

        let error = acquire_in_process_lifecycle(
            identity,
            LifecycleLockMode::Shared,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(lock_unpoisoned(&entry.state).waiters.is_empty());
    }

    #[test]
    fn cache_lifecycle_enqueued_waiter_is_removed_on_panic() {
        let directory = tempfile::tempdir().unwrap();
        let identity = directory.path().join("panic-safe-lifecycle-entry");
        let entry = lifecycle_entry(identity);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut queued = EnqueuedLifecycleWaiter::new(Arc::clone(&entry));
            let mut state = lock_unpoisoned(&entry.state);
            let id = state.next_waiter_id;
            state.next_waiter_id = id.checked_add(1).unwrap();
            queued.id = Some(id);
            state.waiters.push_back(LifecycleWaiter {
                id,
                mode: LifecycleLockMode::Exclusive,
            });
            panic!("exercise queued waiter unwind");
        }));

        assert!(result.is_err());
        assert!(lock_unpoisoned(&entry.state).waiters.is_empty());
    }

    #[test]
    fn cache_lifecycle_final_upgraded_arc_drops_after_registry_unlock() {
        let directory = tempfile::tempdir().unwrap();
        let guard = lock_cache_lifecycle_shared(directory.path(), ".cache", Duration::from_secs(1))
            .unwrap();
        let identity =
            std::fs::canonicalize(directory.path().join(".cache").join(CACHE_LIFECYCLE_LOCK))
                .unwrap();
        let (upgraded_sender, upgraded_receiver) = std::sync::mpsc::sync_channel(0);
        let (continue_sender, continue_receiver) = std::sync::mpsc::sync_channel(0);
        let (done_sender, done_receiver) = std::sync::mpsc::sync_channel(0);
        let thread = std::thread::spawn(move || {
            let found = with_registered_lifecycle_entry(&identity, |_| {
                upgraded_sender.send(()).unwrap();
                continue_receiver.recv().unwrap();
            });
            done_sender.send(found.is_some()).unwrap();
        });

        upgraded_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        drop(guard);
        continue_sender.send(()).unwrap();
        assert!(done_receiver.recv_timeout(Duration::from_secs(2)).unwrap());
        thread.join().unwrap();
    }

    #[test]
    fn cache_lifecycle_registry_key_is_removed_with_last_guard() {
        let directory = tempfile::tempdir().unwrap();
        let guard = lock_cache_lifecycle_shared(directory.path(), ".cache", Duration::from_secs(1))
            .unwrap();
        let identity =
            std::fs::canonicalize(directory.path().join(".cache").join(CACHE_LIFECYCLE_LOCK))
                .unwrap();
        assert!(lock_unpoisoned(&LIFECYCLE_REGISTRY).contains_key(&identity));

        drop(guard);

        assert!(!lock_unpoisoned(&LIFECYCLE_REGISTRY).contains_key(&identity));
    }

    #[test]
    fn cache_lifecycle_distinct_caches_do_not_block_each_other() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let retained =
            lock_cache_lifecycle_exclusive(first.path(), ".cache", Duration::from_secs(1)).unwrap();
        let independent =
            lock_cache_lifecycle_exclusive(second.path(), ".cache", Duration::from_millis(100))
                .unwrap();
        drop(independent);
        drop(retained);
    }

    #[test]
    fn cache_lifecycle_guard_releases_during_panic_unwind() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            let _guard =
                lock_cache_lifecycle_shared(&root, ".cache", Duration::from_secs(1)).unwrap();
            sender.send(()).unwrap();
            panic!("exercise lifecycle guard unwind");
        });
        receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(thread.join().is_err());

        drop(
            lock_cache_lifecycle_exclusive(directory.path(), ".cache", Duration::from_secs(1))
                .unwrap(),
        );
    }

    #[test]
    fn cache_lifecycle_queued_exclusive_precedes_later_shared() {
        let directory = tempfile::tempdir().unwrap();
        let retained =
            lock_cache_lifecycle_shared(directory.path(), ".cache", Duration::from_secs(1))
                .unwrap();
        let root = directory.path().to_path_buf();
        let exclusive_root = root.clone();
        let (order_sender, order_receiver) = std::sync::mpsc::channel();
        let exclusive_sender = order_sender.clone();
        let exclusive = std::thread::spawn(move || {
            let guard =
                lock_cache_lifecycle_exclusive(&exclusive_root, ".cache", Duration::from_secs(2))
                    .unwrap();
            exclusive_sender.send("exclusive").unwrap();
            drop(guard);
        });
        wait_until_queued(&root, 1);

        let shared_root = root.clone();
        let shared = std::thread::spawn(move || {
            let _guard =
                lock_cache_lifecycle_shared(&shared_root, ".cache", Duration::from_secs(2))
                    .unwrap();
            order_sender.send("shared").unwrap();
        });
        wait_until_queued(&root, 2);
        drop(retained);

        assert_eq!(
            order_receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            "exclusive"
        );
        assert_eq!(
            order_receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            "shared"
        );
        exclusive.join().unwrap();
        shared.join().unwrap();
    }
}
