//! Disposable per-run facts shared by Insights readers. Callers persist only normalized
//! period-independent timings, counts and bounded categorical values. Paths, IDs, source
//! JSON, message or result text, and working data from open runs stay out of the store.
//! Source validation checks run linkage and lifecycle fields: when a run ended, whether its
//! projected and payload statuses agree, and whether its thread still exists and is active.
//! Selective reuse additionally verifies item membership and SQLite page/overflow metadata.
//! Facts that survive a WAL change depend only on the run's turn_items rows. Readers
//! compute cross-run and request data from the live snapshot, including run/thread metadata.
mod wal;
mod witness;

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, ffi, params};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fs::{self, DirBuilder, File, Metadata, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use wal::WalView;

const APPLICATION_ID: u32 = u32::from_be_bytes(*b"TSAC");
const FORMAT: u32 = 2;
// The installer checks this ownership marker independently of the SQLite schema version.
const OWNER: &[u8] = b"t3-agents insights cache v1";
const MAX_PAYLOAD: usize = 1024 * 1024;
const MAX_WITNESS: usize = 128 * 1024;
const MAX_WAL_STAMP: usize = 128;
const MAX_REVALIDATE: usize = 32_768;
const SIDECARS: &[&str] = &["-journal", "-wal", "-shm"];
const SOURCES_V1: &str = "CREATE TABLE sources (
    source BLOB PRIMARY KEY CHECK(length(source)=16),
    generation BLOB NOT NULL CHECK(length(generation)=16)
) STRICT, WITHOUT ROWID";
const FACTS_V1: &str = "CREATE TABLE facts (
    source BLOB NOT NULL CHECK(length(source)=16),
    kind BLOB NOT NULL CHECK(length(kind)=16),
    version INTEGER NOT NULL,
    run BLOB NOT NULL CHECK(length(run)=16),
    payload BLOB NOT NULL,
    integrity BLOB NOT NULL CHECK(length(integrity)=16),
    PRIMARY KEY(source, kind, version, run)
) STRICT, WITHOUT ROWID";
const SOURCES: &str = "CREATE TABLE sources (
    source BLOB PRIMARY KEY CHECK(length(source)=16),
    generation BLOB NOT NULL CHECK(length(generation)=16),
    wal_stamp BLOB CHECK(length(wal_stamp)<=128),
    state_integrity BLOB NOT NULL CHECK(length(state_integrity)=16)
) STRICT, WITHOUT ROWID";
const FACTS: &str = "CREATE TABLE facts (
    source BLOB NOT NULL CHECK(length(source)=16),
    kind BLOB NOT NULL CHECK(length(kind)=16),
    version INTEGER NOT NULL,
    run BLOB NOT NULL CHECK(length(run)=16),
    payload BLOB NOT NULL,
    integrity BLOB NOT NULL CHECK(length(integrity)=16),
    locator INTEGER,
    witness BLOB CHECK(length(witness)<=131072),
    witness_integrity BLOB CHECK(length(witness_integrity)=16),
    CHECK((locator IS NULL)=(witness IS NULL) AND (witness_integrity IS NULL)=(witness IS NULL)),
    PRIMARY KEY(source, kind, version, run)
) STRICT, WITHOUT ROWID";
const FACTS_BY_RUN: &str = "CREATE INDEX facts_by_run ON facts (source, locator, run)";
const LOCATOR_OF_RUN: &str = "SELECT rowid FROM main.orchestration_v2_projection_runs WHERE run_id=?1";
const RUN_AT_LOCATOR: &str = "SELECT r.run_id FROM main.orchestration_v2_projection_runs r
    JOIN main.orchestration_v2_projection_threads t ON t.thread_id=r.thread_id WHERE r.rowid=?1";
const REVALIDATE: &str = "SELECT kind,version,run,locator,CASE WHEN length(witness)<=?2 THEN witness END,witness_integrity
    FROM facts INDEXED BY facts_by_run WHERE source=?1 AND locator IS NOT NULL ORDER BY locator,run LIMIT ?3";
const ELIGIBLE: &str = "julianday(r.completed_at) IS NOT NULL
    AND r.status IN ('completed','failed','interrupted','cancelled','rolled_back')
    AND CASE WHEN json_valid(r.payload_json)
             THEN json_extract(r.payload_json,'$.status')=r.status END
    AND t.archived_at IS NULL AND t.deleted_at IS NULL";

type Key = [u8; 16];

fn digest<T: Hash>(domain: &str, value: &T) -> Key {
    // Private, disposable keys and a corruption check. A hasher change in a new build
    // makes existing entries miss. Raw paths and run IDs never go into SQLite.
    let lane = |number: u8| {
        let mut hash = DefaultHasher::new();
        (domain, number, value).hash(&mut hash);
        hash.finish().to_be_bytes()
    };
    let [a, b] = [lane(0), lane(1)];
    let mut key = [0; 16];
    key[..8].copy_from_slice(&a);
    key[8..].copy_from_slice(&b);
    key
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn metadata(path: &Path) -> Option<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Some(Some(meta)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(None),
        Err(_) => None,
    }
}

fn identity(meta: &Metadata) -> (u64, u64) {
    (meta.dev(), meta.ino())
}

/// Fail closed on remote mounts, platforms without the coarse-clock check, and timestamps
/// close enough to now that consecutive writes might get the same filesystem timestamp.
#[cfg(target_os = "linux")]
fn coarse_time(path: &Path) -> Option<(i64, i64)> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;

    let native = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut fs = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: Both pointers are live, writable where required and correctly aligned.
    if unsafe { libc::statfs(native.as_ptr(), fs.as_mut_ptr()) } != 0 {
        return None;
    }
    let kind = unsafe { fs.assume_init() }.f_type as u32;
    const EXT: u32 = 0xEF53;
    const BTRFS: u32 = 0x9123_683E;
    const XFS: u32 = 0x5846_5342;
    const TMPFS: u32 = 0x0102_1994;
    if !matches!(kind, EXT | BTRFS | XFS | TMPFS) {
        return None;
    }
    let mut now = MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: The kernel fills the timespec before returning success.
    if unsafe { libc::clock_gettime(libc::CLOCK_REALTIME_COARSE, now.as_mut_ptr()) } != 0 {
        return None;
    }
    let now = unsafe { now.assume_init() };
    Some((now.tv_sec as i64, now.tv_nsec as i64))
}

#[cfg(not(target_os = "linux"))]
fn coarse_time(_path: &Path) -> Option<(i64, i64)> {
    None
}

#[derive(Debug, Eq, Hash, PartialEq)]
struct FileStamp {
    id: (u64, u64),
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl FileStamp {
    fn of(meta: Metadata, now: (i64, i64)) -> Option<Self> {
        // A hard-linked database can have a different WAL depending on its name.
        if !meta.is_file() || meta.nlink() != 1 {
            return None;
        }
        let stamp = Self {
            id: identity(&meta),
            size: meta.len(),
            modified: (meta.mtime(), meta.mtime_nsec()),
            changed: (meta.ctime(), meta.ctime_nsec()),
        };
        (stamp.modified < now && stamp.changed < now).then_some(stamp)
    }
}

#[derive(Debug, Eq, Hash, PartialEq)]
struct Generation {
    main: FileStamp,
    wal: Option<FileStamp>,
    shm: Option<(u64, u64)>,
}

impl Generation {
    fn capture(path: &Path) -> Option<Self> {
        let now = coarse_time(path)?;
        let main = FileStamp::of(metadata(path)??, now)?;
        let wal = match metadata(&sidecar(path, "-wal"))? {
            Some(meta) => Some(FileStamp::of(meta, now)?),
            None => None,
        };
        let shm = match metadata(&sidecar(path, "-shm"))? {
            Some(meta) if meta.is_file() && meta.nlink() == 1 && meta.dev() == main.id.0 => Some(identity(&meta)),
            Some(_) => return None,
            None => None,
        };
        // A rollback journal may be hot or an in-flight write. A WAL without its index may
        // be in recovery. Either is reason to read directly and try the cache another time.
        if metadata(&sidecar(path, "-journal"))?.is_some()
            || wal.is_some() != shm.is_some()
            || wal.as_ref().is_some_and(|f| f.id.0 != main.id.0)
        {
            return None;
        }
        Some(Self { main, wal, shm })
    }
}

#[derive(Debug, Eq, Hash, PartialEq)]
struct PinnedGeneration {
    files: Generation,
    commit: Option<[u8; 48]>,
}

impl PinnedGeneration {
    fn capture(db: &Connection, path: &Path) -> Option<Self> {
        let files = Generation::capture(path)?;
        let commit = match &files.wal {
            Some(wal) if wal.size != 0 => Some(wal_header(db)?),
            _ => None,
        };
        (Generation::capture(path)? == files).then_some(Self { files, commit })
    }
}

/// A writer can first write the WAL frames and later publish the commit to the WAL index.
/// Include the published header to distinguish these states even if main/WAL stat is equal.
#[cfg(target_os = "linux")]
fn wal_header(db: &Connection) -> Option<[u8; 48]> {
    use std::ptr::null_mut;

    let mut file = null_mut::<ffi::sqlite3_file>();
    // SAFETY: The connection owns the sqlite3_file, and FILE_POINTER fills the pointer.
    // xShmMap uses its existing read-transaction mapping and is called with extend=0.
    let region = unsafe {
        if ffi::sqlite3_txn_state(db.handle(), c"main".as_ptr()) != ffi::SQLITE_TXN_READ {
            return None;
        }
        let result = ffi::sqlite3_file_control(
            db.handle(), c"main".as_ptr(), ffi::SQLITE_FCNTL_FILE_POINTER,
            (&mut file as *mut *mut ffi::sqlite3_file).cast(),
        );
        if result != ffi::SQLITE_OK || file.is_null() {
            return None;
        }
        let methods = (*file).pMethods.as_ref()?;
        if methods.iVersion < 2 {
            return None;
        }
        let map = methods.xShmMap?;
        let mut region = null_mut();
        if map(file, 0, 32768, 0, &mut region) != ffi::SQLITE_OK || region.is_null() {
            return None;
        }
        region
    };
    let mut raw = [0; 96];
    let destination = libc::iovec { iov_base: raw.as_mut_ptr().cast(), iov_len: raw.len() };
    let source = libc::iovec { iov_base: region, iov_len: raw.len() };
    // SAFETY: The kernel copies mapped bytes into our private buffer. This is a fallible
    // read across process/mmap synchronization; Rust never dereferences shared bytes.
    // Opening a second ordinary fd for the same inode and closing it could release SQLite's locks.
    if unsafe { libc::process_vm_readv(libc::getpid(), &destination, 1, &source, 1, 0) } != raw.len() as isize {
        return None;
    }
    // Bundled SQLite sqlite3.c: WalIndexHdr, WALINDEX_PGSZ, walIndexTryHdr and
    // walChecksumBytes. Unknown layout/version, or either half written mid-update, misses.
    let (first, second) = raw.split_at(48);
    let word = |index: usize| u32::from_ne_bytes(first[index..index + 4].try_into().unwrap());
    if first != second || word(0) != 3_007_000 || first[12] != 1 {
        return None;
    }
    let mut a = 0u32;
    let mut b = 0u32;
    for i in (0..40).step_by(8) {
        a = a.wrapping_add(word(i)).wrapping_add(b);
        b = b.wrapping_add(word(i + 4)).wrapping_add(a);
    }
    (a == word(40) && b == word(44)).then(|| first.try_into().unwrap())
}

#[cfg(not(target_os = "linux"))]
fn wal_header(_db: &Connection) -> Option<[u8; 48]> {
    None
}

pub(crate) fn handle_matches(db: &Connection, path: &Path) -> bool {
    if db.path().is_none_or(|name| Path::new(name) != path) {
        return false;
    }
    let mut moved: libc::c_int = 1;
    // SAFETY: The live Connection owns this handle for the call's duration. HAS_MOVED
    // writes only the provided int. Unsupported VFSes fail closed.
    let result = unsafe {
        ffi::sqlite3_file_control(db.handle(), c"main".as_ptr(), ffi::SQLITE_FCNTL_HAS_MOVED, (&mut moved as *mut libc::c_int).cast::<c_void>())
    };
    result == ffi::SQLITE_OK && moved == 0
}

fn data_version(db: &Connection) -> Option<i64> {
    db.query_row("PRAGMA main.data_version", [], |row| row.get(0)).ok()
}

#[derive(PartialEq, Eq)]
struct SourceProof {
    canonical: PathBuf,
    generation: PinnedGeneration,
}

/// Source identity and generation only. Workers can share this without sharing a
/// SQLite connection or opening the cache store.
#[derive(Clone)]
pub(crate) struct SourceToken(Arc<SourceProof>);

/// Source proof captured before the caller begins its read transaction.
pub(crate) struct Snapshot<'db> {
    db: &'db Connection,
    original: PathBuf,
    store: PathBuf,
    source: SourceToken,
    data_version: i64,
}

impl<'db> Snapshot<'db> {
    pub(crate) fn probe(db: &'db Connection, path: &Path) -> Option<Self> {
        let store = crate::cache_path::insights_file()?;
        Self::probe_with_store(db, path, store)
    }

    pub(crate) fn probe_with_store(db: &'db Connection, path: &Path, store: PathBuf) -> Option<Self> {
        if !db.is_autocommit() || !db.is_readonly("main").ok()? {
            return None;
        }
        let canonical = fs::canonicalize(path).ok()?;
        if !handle_matches(db, &canonical) {
            return None;
        }
        let files = Generation::capture(&canonical)?;
        let probe = db.unchecked_transaction().ok()?;
        // This probe and the later read transaction must see the same connection-local
        // data_version. Metadata alone cannot bind the proof to the connection's snapshot.
        let data_version = data_version(&probe)?;
        let generation = PinnedGeneration::capture(&probe, &canonical)?;
        probe.rollback().ok()?;
        if generation.files != files {
            return None;
        }
        let source = SourceToken(Arc::new(SourceProof { canonical, generation }));
        let snapshot = Self { db, original: path.to_owned(), store, source, data_version };
        Some(snapshot)
    }

    fn unchanged(&self) -> bool {
        let SourceProof { canonical, generation } = self.source.0.as_ref();
        handle_matches(self.db, canonical)
            && fs::canonicalize(&self.original).ok().as_ref() == Some(canonical)
            && PinnedGeneration::capture(self.db, canonical).as_ref() == Some(generation)
    }

    fn pin(&self, db: &Connection) -> bool {
        std::ptr::eq(db, self.db)
            && !db.is_autocommit()
            && data_version(db) == Some(self.data_version)
            && self.unchanged()
    }

    /// A worker probes its own read-only connection before BEGIN and calls this inside
    /// the transaction. A true result pins that worker to the token's source generation.
    /// On mismatch, the caller must read serially from the main transaction.
    pub(crate) fn matches(self, worker_db: &Connection, token: &SourceToken) -> bool {
        self.source.0 == token.0 && self.pin(worker_db)
    }

    /// Attach the optional cache to the caller's transaction. Reading the version pins
    /// a deferred BEGIN, so subsequent source rows share the snapshot checked here.
    pub(crate) fn open(self, db: &Connection) -> Option<Cache<'db>> {
        if !self.pin(db) {
            return None;
        }
        let SourceProof { canonical, generation } = self.source.0.as_ref();
        let (store, lock) = open_store(&self.store, canonical, &generation.files)?;
        let source = digest("source", canonical);
        let generation = digest("generation", generation);
        let mut cache = Cache { snapshot: self, store, _lock: lock, source, generation, wal_stamp: None, eligible: HashMap::new() };
        cache.activate()?;
        Some(cache)
    }
}

fn owner_uid() -> u32 {
    // SAFETY: geteuid takes no pointers or arguments.
    unsafe { libc::geteuid() }
}

fn private_file(meta: &Metadata) -> bool {
    meta.is_file() && meta.uid() == owner_uid() && meta.nlink() == 1 && meta.mode() & 0o077 == 0
}

fn read_file(path: &Path) -> Option<File> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path).ok()?;
    private_file(&file.metadata().ok()?).then_some(file)
}

fn create_file(path: &Path) -> Option<File> {
    OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).ok()
}

fn open_store(path: &Path, source: &Path, files: &Generation) -> Option<(Connection, File)> {
    let dir = path.parent()?;
    let base = dir.parent()?;
    fs::create_dir_all(base).ok()?;
    if !metadata(base)??.is_dir() {
        return None;
    }
    let _ = DirBuilder::new().mode(0o700).create(dir);
    let meta = metadata(dir)??;
    if !meta.is_dir() || meta.uid() != owner_uid() || meta.mode() & 0o022 != 0 {
        return None;
    }
    let dir = fs::canonicalize(dir).ok()?;
    let path = dir.join(path.file_name()?);
    let owner = dir.join("insights.owner");
    let lock_path = dir.join("insights.lock");
    let names = SIDECARS.iter().map(|s| sidecar(&path, s));
    let reserved = [path.clone(), owner.clone(), lock_path.clone()].into_iter().chain(names);
    let source_paths: Vec<_> = std::iter::once(source.to_owned()).chain(SIDECARS.iter().map(|s| sidecar(source, s))).collect();
    let source_ids = [Some(files.main.id), files.wal.as_ref().map(|f| f.id), files.shm];
    for entry in reserved {
        if source_paths.contains(&entry) || metadata(&entry)?.is_some_and(|m| source_ids.contains(&Some(identity(&m)))) {
            return None;
        }
    }
    let lock = match metadata(&lock_path)? {
        Some(meta) if private_file(&meta) && meta.len() == 0 => read_file(&lock_path)?,
        Some(_) => return None,
        None => create_file(&lock_path)?,
    };
    // SAFETY: The File owns a valid descriptor. Holding this same nonblocking lock through
    // close also excludes the installer and separate header probes in the same process.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return None;
    }
    for suffix in SIDECARS {
        if metadata(&sidecar(&path, suffix))?.is_some() {
            // Never recover an unverified journal. The installer's cache cleanup, with
            // its lock held, can remove insights.sqlite3 and all three of its sidecars.
            return None;
        }
    }
    let existing = metadata(&path)?.is_some();
    match metadata(&owner)? {
        Some(meta) if meta.len() == OWNER.len() as u64 => {
            let mut text = [0; OWNER.len()];
            read_file(&owner)?.read_exact(&mut text).ok()?;
            if text != OWNER {
                return None;
            }
        }
        None if !existing => create_file(&owner)?.write_all(OWNER).ok()?,
        _ => return None,
    }
    let version = if existing {
        let mut header = [0; 100];
        read_file(&path)?.read_exact(&mut header).ok()?;
        if header[..16] != *b"SQLite format 3\0"
            || header[18..20] != [1, 1]
            || header[68..72] != APPLICATION_ID.to_be_bytes()
        {
            return None;
        }
        let version = u32::from_be_bytes(header[60..64].try_into().ok()?);
        if !matches!(version, 1 | FORMAT) {
            return None;
        }
        version
    } else {
        create_file(&path)?;
        0
    };
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let mut db = Connection::open_with_flags(&path, flags).ok()?;
    db.busy_timeout(Duration::ZERO).ok()?;
    db.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA secure_delete=ON").ok()?;
    if version != FORMAT {
        if version == 1 {
            check_schema(&db, &[FACTS_V1, SOURCES_V1])?;
        }
        let tx = db.transaction().ok()?;
        if version == 1 {
            tx.execute_batch("DROP TABLE facts; DROP TABLE sources").ok()?;
        }
        tx.pragma_update(None, "application_id", APPLICATION_ID).ok()?;
        tx.pragma_update(None, "user_version", FORMAT).ok()?;
        tx.execute(SOURCES, []).ok()?;
        tx.execute(FACTS, []).ok()?;
        tx.execute(FACTS_BY_RUN, []).ok()?;
        tx.commit().ok()?;
    }
    check_schema(&db, &[FACTS, FACTS_BY_RUN, SOURCES])?;
    Some((db, lock))
}

fn check_schema(db: &Connection, expected: &[&str]) -> Option<()> {
    let mut stmt = db.prepare("SELECT sql FROM sqlite_schema ORDER BY name").ok()?;
    let mut rows = stmt.query([]).ok()?;
    for expected in expected {
        let sql = rows.next().ok()??.get_ref(0).ok()?.as_str().ok()?;
        if sql != *expected {
            return None;
        }
    }
    rows.next().ok()?.is_none().then_some(())
}

/// Optional facts from a single pinned, read-only source snapshot.
pub(crate) struct Cache<'db> {
    snapshot: Snapshot<'db>,
    store: Connection,
    _lock: File,
    source: Key,
    generation: Key,
    wal_stamp: Option<Vec<u8>>,
    eligible: HashMap<&'static str, (i64, HashSet<String>)>,
}

struct CachedSource {
    source: Key,
    generation: Key,
    stamp: Option<Vec<u8>>,
    integrity: Key,
}

fn source_integrity(source: Key, generation: Key, stamp: Option<&[u8]>) -> Key {
    digest("source-state-v2", &(source, generation, stamp))
}

fn witness_integrity(source: Key, locator: i64, run: Key, proof: &[u8]) -> Key {
    digest("run-witness-v2", &(source, locator, run, proof))
}

fn prune_unproven(
    tx: &rusqlite::Transaction<'_>, db: &Connection, source: Key, wal: &WalView<'_>, changed: &HashSet<u32>,
) -> Option<()> {
    let mut active = db.prepare(&format!("{RUN_AT_LOCATOR} AND {ELIGIBLE}")).ok()?;
    let mut rejected = Vec::new();
    {
        let mut scan = tx.prepare(REVALIDATE).ok()?;
        let mut rows = scan.query(params![source, MAX_WITNESS as i64, MAX_REVALIDATE as i64 + 1]).ok()?;
        let mut decisions = HashMap::new();
        let mut seen = 0;
        while let Some(row) = rows.next().ok()? {
            if seen == MAX_REVALIDATE {
                return None;
            }
            seen += 1;
            let run: Key = row.get(2).ok()?;
            let locator: i64 = row.get(3).ok()?;
            let proof = row.get_ref(4).ok()?.as_blob_or_null().ok()?;
            let integrity: Option<Key> = row.get(5).ok()?;
            let mut retain = false;
            if let Some((proof, integrity)) = proof.zip(integrity) {
                if witness_integrity(source, locator, run, proof) == integrity {
                    retain = *decisions.entry((locator, run, integrity)).or_insert_with(|| {
                        let id = active.query_row([locator], |r| r.get::<_, String>(0)).ok();
                        id.is_some_and(|id| digest("run", &id) == run && witness::unchanged(db, wal, &id, proof, changed))
                    });
                }
            }
            if !retain {
                rejected.push((row.get::<_, Key>(0).ok()?, row.get::<_, i64>(1).ok()?, run));
            }
        }
    }
    tx.execute("DELETE FROM facts WHERE source<>?1 OR locator IS NULL", [source]).ok()?;
    let mut delete = tx.prepare("DELETE FROM facts WHERE source=?1 AND kind=?2 AND version=?3 AND run=?4").ok()?;
    for (kind, version, run) in rejected {
        delete.execute(params![source, kind, version, run]).ok()?;
    }
    Some(())
}

impl Cache<'_> {
    /// Give a worker the exact source proof for this cache's pinned transaction.
    pub(crate) fn source_token(&self) -> SourceToken {
        self.snapshot.source.clone()
    }

    fn valid(&self) -> bool {
        !self.snapshot.db.is_autocommit() && self.snapshot.unchanged()
    }

    fn activate(&mut self) -> Option<()> {
        if !self.valid() {
            return None;
        }
        let prior = self.store.query_row(
            "SELECT source,generation,wal_stamp,state_integrity FROM sources
             WHERE wal_stamp IS NULL OR length(wal_stamp)<=?1 LIMIT 1",
            [MAX_WAL_STAMP as i64],
            |r| Ok(CachedSource { source: r.get(0)?, generation: r.get(1)?, stamp: r.get(2)?, integrity: r.get(3)? }),
        ).optional().ok()?.filter(|row| {
            row.source == self.source && row.integrity == source_integrity(row.source, row.generation, row.stamp.as_deref())
        });
        if prior.as_ref().is_some_and(|row| row.generation == self.generation) {
            self.wal_stamp = prior.and_then(|row| row.stamp);
            return Some(());
        }
        let db = self.snapshot.db;
        let wal = WalView::open(db, &self.snapshot.source.0.generation);
        let stamp = wal.as_ref().map(WalView::stamp).filter(|stamp| stamp.len() <= MAX_WAL_STAMP);
        let tx = self.store.transaction_with_behavior(TransactionBehavior::Immediate).ok()?;
        if !self.snapshot.unchanged() {
            return None;
        }
        // The old stamp describes the WAL baseline for every stored proof. Advance it
        // atomically with pruning, including runs outside the period and other rules.
        let retained = (|| {
            let old = prior.as_ref()?.stamp.as_deref()?;
            stamp.as_ref()?;
            let wal = wal.as_ref()?;
            let changed = wal.changes_since(old)?;
            prune_unproven(&tx, db, self.source, wal, &changed)
        })();
        if retained.is_none() {
            tx.execute("DELETE FROM facts", []).ok()?;
        }
        tx.execute("DELETE FROM sources", []).ok()?;
        let integrity = source_integrity(self.source, self.generation, stamp.as_deref());
        tx.execute("INSERT INTO sources VALUES (?1, ?2, ?3, ?4)", params![self.source, self.generation, stamp, integrity]).ok()?;
        if !self.snapshot.unchanged() {
            return None;
        }
        tx.commit().ok()?;
        self.wal_stamp = stamp;
        Some(())
    }

    fn allowed(&self, requested: &[&str]) -> Option<HashSet<String>> {
        let mut q = self.snapshot.db.prepare(&format!(
            "SELECT r.run_id FROM json_each(?1) w
             CROSS JOIN main.orchestration_v2_projection_runs r ON r.run_id=w.value
             JOIN main.orchestration_v2_projection_threads t ON t.thread_id=r.thread_id
             WHERE {ELIGIBLE}",
        )).ok()?;
        let ids = serde_json::to_string(requested).ok()?;
        let rows = q.query_map([ids], |row| row.get(0)).ok()?;
        rows.collect::<rusqlite::Result<_>>().ok()
    }

    /// Validate the supplied source runs once for use with this fact family. Unknown,
    /// malformed, open, removed and archived runs miss even if a caller supplies their IDs.
    pub(crate) fn lookup_many(&mut self, kind: &'static str, version: i64, requested: &[&str]) -> HashMap<String, Vec<u8>> {
        self.eligible.remove(kind);
        let kind_key = digest("kind", &kind);
        if !self.valid() || self.retire(kind_key, version).is_none() || requested.is_empty() {
            return HashMap::new();
        }
        let Some(allowed) = self.allowed(requested) else {
            return HashMap::new();
        };
        let hits = self.lookup(kind_key, version, &allowed).unwrap_or_default();
        if !self.valid() {
            return HashMap::new();
        }
        self.eligible.insert(kind, (version, allowed));
        hits
    }

    fn retire(&mut self, kind: Key, version: i64) -> Option<()> {
        let older: bool = self.store.query_row(
            "SELECT EXISTS (SELECT 1 FROM facts WHERE source=?1 AND kind=?2 AND version<?3)",
            params![self.source, kind, version], |r| r.get(0),
        ).ok()?;
        if !older {
            return Some(());
        }
        let tx = self.store.transaction_with_behavior(TransactionBehavior::Immediate).ok()?;
        tx.execute("DELETE FROM facts WHERE source=?1 AND kind=?2 AND version<?3", params![self.source, kind, version]).ok()?;
        if !self.snapshot.unchanged() {
            return None;
        }
        tx.commit().ok()
    }

    fn lookup(&self, kind: Key, version: i64, allowed: &HashSet<String>) -> Option<HashMap<String, Vec<u8>>> {
        let mut hits = HashMap::new();
        let mut q = self.store.prepare(
            "SELECT payload, integrity, locator, witness_integrity FROM facts
             WHERE source=?1 AND kind=?2 AND version=?3 AND run=?4 AND length(payload)<=?5",
        ).ok()?;
        for id in allowed {
            let run = digest("run", id);
            let mut rows = q.query(params![self.source, kind, version, run, MAX_PAYLOAD as i64]).ok()?;
            if let Some(row) = rows.next().ok()? {
                let payload = row.get_ref(0).ok()?.as_blob().ok()?;
                let integrity = row.get::<_, Key>(1).ok()?;
                let locator = row.get::<_, Option<i64>>(2).ok()?;
                let proof = row.get::<_, Option<Key>>(3).ok()?;
                if integrity == digest("payload-v2", &(self.source, kind, version, run, locator, proof, payload)) {
                    hits.insert(id.clone(), payload.to_vec());
                }
            }
        }
        Some(hits)
    }

    /// Write facts for runs validated by lookup_many and return the number of distinct
    /// rows committed. A failed commit reports zero. The caller serializes only
    /// period-independent sanitized values derived from that run's turn_items rows.
    pub(crate) fn write_many(&mut self, kind: &'static str, version: i64, entries: &[(&str, Vec<u8>)]) -> usize {
        if entries.is_empty() || !self.valid() {
            return 0;
        }
        self.write(kind, version, entries).unwrap_or(0)
    }

    fn write(&mut self, kind: &'static str, version: i64, entries: &[(&str, Vec<u8>)]) -> Option<usize> {
        let (validated_version, allowed) = self.eligible.get(kind)?;
        if *validated_version != version {
            return None;
        }
        // Only the last eligible payload for each key is visible after commit. Retain
        // references for the batch and compute/drop one potentially large witness at a time.
        let mut accepted = HashMap::new();
        for (id, payload) in entries {
            if allowed.contains(*id) && payload.len() <= MAX_PAYLOAD {
                accepted.insert(digest("run", id), (*id, payload.as_slice()));
            }
        }
        if accepted.is_empty() {
            return Some(0);
        }
        let kind = digest("kind", &kind);
        let db = self.snapshot.db;
        let wal = self.wal_stamp.as_ref().and_then(|stamp| {
            WalView::open(db, &self.snapshot.source.0.generation).filter(|wal| wal.stamp() == *stamp)
        });
        let mut locator = wal.as_ref().and_then(|_| db.prepare(LOCATOR_OF_RUN).ok());
        let tx = self.store.transaction_with_behavior(TransactionBehavior::Immediate).ok()?;
        if !self.snapshot.unchanged() {
            return None;
        }
        let mut written = 0;
        {
            let mut q = tx.prepare(
                "INSERT INTO facts VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(source,kind,version,run) DO UPDATE
                 SET payload=excluded.payload, integrity=excluded.integrity, locator=excluded.locator,
                     witness=excluded.witness, witness_integrity=excluded.witness_integrity",
            ).ok()?;
            for (run, (id, payload)) in accepted {
                let witness = (|| {
                    let locator = locator.as_mut()?.query_row([id], |r| r.get(0)).ok()?;
                    let proof = witness::capture(db, wal.as_ref()?, id)?;
                    if proof.len() > MAX_WITNESS {
                        return None;
                    }
                    let integrity = witness_integrity(self.source, locator, run, &proof);
                    Some((locator, proof, integrity))
                })();
                let (locator, witness, proof) = witness.as_ref().map_or((None, None, None), |(id, proof, integrity)| {
                    (Some(*id), Some(proof.as_slice()), Some(*integrity))
                });
                let integrity = digest("payload-v2", &(self.source, kind, version, run, locator, proof, payload));
                written += q.execute(params![self.source, kind, version, run, payload, integrity, locator, witness, proof]).ok()?;
            }
        }
        if !self.snapshot.unchanged() {
            return None;
        }
        tx.commit().ok()?;
        Some(written)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    const KIND: &str = "cache-test";
    const FIRST: &str = "sensitive-run-one";
    const SECOND: &str = "sensitive-run-two";

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let name = std::env::temp_dir().join("t3-agents-cache-XXXXXX");
            let mut name = std::ffi::CString::new(name.as_os_str().as_bytes()).unwrap().into_bytes_with_nul();
            // SAFETY: mkdtemp receives a writable, NUL-terminated pathname.
            assert!(!unsafe { libc::mkdtemp(name.as_mut_ptr().cast()) }.is_null());
            name.pop();
            Self(PathBuf::from(std::ffi::OsStr::from_bytes(&name)))
        }

        fn source(&self) -> PathBuf {
            self.0.join("source.db")
        }

        fn store(&self) -> PathBuf {
            self.0.join("cache").join("insights.sqlite3")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn writer(path: &Path, wal: bool) -> Connection {
        let db = Connection::open(path).unwrap();
        if wal {
            db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0").unwrap();
        }
        db.execute_batch(
            r#"CREATE TABLE orchestration_v2_projection_threads (thread_id TEXT PRIMARY KEY, archived_at TEXT, deleted_at TEXT);
             CREATE TABLE orchestration_v2_projection_runs (run_id TEXT PRIMARY KEY, thread_id TEXT, status TEXT, completed_at TEXT, payload_json TEXT);
             INSERT INTO orchestration_v2_projection_threads VALUES ('r',NULL,NULL), ('s',NULL,NULL), ('a','2026-11-09',NULL), ('d',NULL,'2026-11-09');
             INSERT INTO orchestration_v2_projection_runs VALUES
              ('sensitive-run-one','r','completed','2026-11-09','{"status":"completed"}'),
              ('sensitive-run-two','s','completed','2026-11-10','{"status":"completed"}'),
              ('open','r','running',NULL,'{"status":"running"}'),
              ('conflicting','r','completed','2026-11-09','{"status":"running"}'),
              ('missing-thread','n','completed','2026-11-09','{"status":"completed"}'),
              ('closed-unknown','r','future','2026-11-09','{"status":"future"}'),
              ('archived','a','completed','2026-11-09','{"status":"completed"}'),
              ('deleted','d','completed','2026-11-09','{"status":"completed"}');
             CREATE TABLE orchestration_v2_projection_turn_items (run_id TEXT, ordinal INTEGER, updated_at TEXT, payload_json TEXT, type TEXT NOT NULL DEFAULT 'checkpoint');
             CREATE INDEX orchestration_v2_projection_turn_items_run_ordinal_idx
                 ON orchestration_v2_projection_turn_items(run_id,ordinal);
             CREATE TABLE unrelated_events (thread_id TEXT, message TEXT);
             INSERT INTO orchestration_v2_projection_turn_items(run_id,ordinal,updated_at,payload_json) VALUES
              ('sensitive-run-one',1,'2026-11-09','{"additions":1}'),
              ('sensitive-run-one',2,'2026-11-10','{"additions":9}');"#,
        ).unwrap();
        db
    }

    fn reader(path: &Path) -> Connection {
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
    }

    fn stable(path: &Path) {
        // The source stamp waits out a filesystem tick before trusting fresh timestamps.
        for _ in 0..200 {
            if Generation::capture(path).is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("source stamp failed to settle");
    }

    fn using<T>(path: &Path, store: &Path, action: impl FnOnce(&mut Cache<'_>) -> T) -> T {
        stable(&fs::canonicalize(path).unwrap());
        let db = reader(path);
        let probe = Snapshot::probe_with_store(&db, path, store.to_owned()).unwrap();
        db.execute_batch("BEGIN").unwrap();
        let output = {
            let mut cache = probe.open(&db).unwrap();
            action(&mut cache)
        };
        db.execute_batch("ROLLBACK").unwrap();
        output
    }

    fn count_facts(path: &Path) -> i64 {
        reader(path).query_row("SELECT count(*) FROM facts", [], |row| row.get(0)).unwrap()
    }

    fn match_worker(path: &Path, unused_store: &Path, token: &SourceToken) -> bool {
        let worker = reader(path);
        let probe = Snapshot::probe_with_store(&worker, path, unused_store.to_owned()).unwrap();
        assert!(worker.is_autocommit());
        worker.execute_batch("BEGIN").unwrap();
        let matched = probe.matches(&worker, token);
        worker.execute_batch("ROLLBACK").unwrap();
        matched
    }

    #[test]
    fn source_token_matches_an_independent_read_only_worker() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SourceToken>();
        for wal in [false, true] {
            let dir = Scratch::new();
            let path = dir.source();
            let store = dir.store();
            let _writer = writer(&path, wal);
            let other_path = dir.0.join("other.db");
            writer(&other_path, false);
            stable(&other_path);
            let foreign_store = dir.0.join("unused").join("worker.sqlite3");
            using(&path, &store, |cache| {
                let token = cache.source_token();
                std::thread::scope(|scope| {
                    let token = token.clone();
                    let (path, foreign_store) = (&path, &foreign_store);
                    assert!(scope.spawn(move || match_worker(path, foreign_store, &token)).join().unwrap());
                });
                assert!(!foreign_store.exists());
                assert!(!match_worker(&other_path, &foreign_store, &token));

                let worker = reader(&path);
                let wrong_connection = reader(&path);
                let probe = Snapshot::probe_with_store(&worker, &path, foreign_store.clone()).unwrap();
                wrong_connection.execute_batch("BEGIN").unwrap();
                assert!(data_version(&wrong_connection).is_some());
                assert!(!probe.matches(&wrong_connection, &token));
                assert!(Snapshot::probe_with_store(&wrong_connection, &path, foreign_store.clone()).is_none());
                wrong_connection.execute_batch("ROLLBACK").unwrap();
            });
        }
    }

    #[test]
    fn cache_enforces_run_lifecycle_and_retires_only_older_rules() {
        let dir = Scratch::new();
        writer(&dir.source(), false);
        let wanted = [FIRST, SECOND, "missing", "open", "conflicting", "missing-thread", "closed-unknown", "archived", "deleted"];
        let attempted: Vec<_> = wanted.into_iter().map(|run| (run, vec![1])).collect();
        using(&dir.source(), &dir.store(), |cache| {
            assert!(cache.lookup_many(KIND, 1, &wanted).is_empty());
            assert_eq!(cache.write_many(KIND, 1, &attempted), 2);
            let hits = cache.lookup_many(KIND, 1, &wanted);
            assert_eq!(hits.into_keys().collect::<HashSet<_>>(), [FIRST.to_owned(), SECOND.to_owned()].into_iter().collect());
            cache.lookup_many("other-feature", 1, &[FIRST]);
            cache.write_many("other-feature", 1, &[(FIRST, vec![2])]);
            assert!(cache.lookup_many(KIND, 2, &[FIRST]).is_empty());
            assert_eq!(cache.write_many(KIND, 1, &[(FIRST, vec![1])]), 0);
            assert_eq!(cache.lookup_many("other-feature", 1, &[FIRST])[FIRST], [2]);
            cache.write_many(KIND, 2, &[(FIRST, vec![3])]);
        });
        assert_eq!(count_facts(&dir.store()), 2);
        let raw = fs::read(dir.store()).unwrap();
        for text in [FIRST, SECOND, dir.source().to_str().unwrap()] {
            assert!(!raw.windows(text.len()).any(|bytes| bytes == text.as_bytes()));
        }
    }

    #[test]
    fn payload_only_update_prunes_all_previous_rollback_generation_facts() {
        let dir = Scratch::new();
        let db = writer(&dir.source(), false);
        let token = using(&dir.source(), &dir.store(), |cache| {
            cache.lookup_many(KIND, 1, &[FIRST, SECOND]);
            cache.write_many(KIND, 1, &[(FIRST, vec![1]), (SECOND, vec![2])]);
            cache.source_token()
        });
        assert_eq!(count_facts(&dir.store()), 2);
        db.execute(
            r#"UPDATE orchestration_v2_projection_turn_items SET payload_json='{"additions":2}' WHERE ordinal=1"#,
            [],
        ).unwrap();
        stable(&dir.source());
        assert!(!match_worker(&dir.source(), &dir.store(), &token));
        using(&dir.source(), &dir.store(), |cache| {
            assert!(cache.lookup_many(KIND, 1, &[FIRST]).is_empty());
        });
        assert_eq!(count_facts(&dir.store()), 0, "outside-period rows also retire");
    }

    #[test]
    fn older_wal_snapshot_cannot_write_after_a_concurrent_commit() {
        let dir = Scratch::new();
        let path = dir.source();
        let store = dir.store();
        let writer = writer(&path, true);
        stable(&path);
        using(&path, &store, |cache| {
            cache.lookup_many(KIND, 1, &[FIRST, SECOND]);
            cache.write_many(KIND, 1, &[(FIRST, vec![1]), (SECOND, vec![2])]);
        });
        let db = reader(&path);
        let probe = Snapshot::probe_with_store(&db, &path, store.clone()).unwrap();
        db.execute_batch("BEGIN").unwrap();
        let mut cache = probe.open(&db).unwrap();
        assert_eq!(cache.lookup_many(KIND, 1, &[FIRST, SECOND]).len(), 2);
        let contender = reader(&path);
        let contender_probe = Snapshot::probe_with_store(&contender, &path, store.clone()).unwrap();
        contender.execute_batch("BEGIN").unwrap();
        assert!(contender_probe.open(&contender).is_none());
        contender.execute_batch("ROLLBACK").unwrap();
        let old_version = data_version(&db).unwrap();
        let old_header = wal_header(&db).unwrap();
        let token = cache.source_token();
        let worker = reader(&path);
        let worker_probe = Snapshot::probe_with_store(&worker, &path, store.clone()).unwrap();
        writer.execute_batch(
            r#"BEGIN;
             UPDATE orchestration_v2_projection_turn_items SET payload_json='{"additions":2}' WHERE ordinal=1;
             UPDATE orchestration_v2_projection_threads SET archived_at='2026-11-18' WHERE thread_id='s';
             COMMIT;"#,
        ).unwrap();
        // The reader keeps its snapshot even as the writer publishes a new WAL header.
        // This public state can change after the final WAL file write and without a new stat.
        assert_eq!(data_version(&db), Some(old_version));
        assert_ne!(wal_header(&db), Some(old_header));
        stable(&path);
        worker.execute_batch("BEGIN").unwrap();
        assert!(!worker_probe.matches(&worker, &token));
        worker.execute_batch("ROLLBACK").unwrap();
        assert!(!match_worker(&path, &store, &token));
        assert_eq!(cache.write_many(KIND, 1, &[(FIRST, vec![99]), (SECOND, vec![99])]), 0);
        assert!(cache.lookup_many(KIND, 1, &[FIRST]).is_empty());
        drop(cache);
        db.execute_batch("ROLLBACK").unwrap();
        assert_eq!(count_facts(&store), 2);
        using(&path, &store, |cache| {
            assert!(cache.lookup_many(KIND, 1, &[FIRST, SECOND]).is_empty());
            cache.write_many(KIND, 1, &[(FIRST, vec![3]), (SECOND, vec![99])]);
            assert_eq!(cache.lookup_many(KIND, 1, &[FIRST, SECOND]), [(FIRST.into(), vec![3])].into_iter().collect());
        });
        assert_eq!(count_facts(&store), 1);
    }

    #[test]
    fn stale_handle_cannot_adopt_a_replaced_source_and_symlink_aliases_share_facts() {
        let dir = Scratch::new();
        let path = dir.source();
        let store = dir.store();
        writer(&path, false);
        let token = using(&path, &store, |cache| {
            cache.lookup_many(KIND, 1, &[FIRST]);
            cache.write_many(KIND, 1, &[(FIRST, vec![1])]);
            cache.source_token()
        });
        let alias = dir.0.join("alias.db");
        symlink(&path, &alias).unwrap();
        using(&alias, &store, |cache| {
            assert_eq!(cache.lookup_many(KIND, 1, &[FIRST])[FIRST], [1]);
        });
        let stale = reader(&path);
        let probe = Snapshot::probe_with_store(&stale, &path, store.clone()).unwrap();
        fs::rename(&path, dir.0.join("old.db")).unwrap();
        writer(&path, false);
        stable(&path);
        assert!(!match_worker(&path, &store, &token));
        assert!(Snapshot::probe_with_store(&stale, &path, store.clone()).is_none());
        stale.execute_batch("BEGIN").unwrap();
        assert!(probe.open(&stale).is_none());
        stale.execute_batch("ROLLBACK").unwrap();
        using(&path, &store, |cache| {
            assert!(cache.lookup_many(KIND, 1, &[FIRST]).is_empty());
        });
        assert_eq!(count_facts(&store), 0);
    }

    #[test]
    fn cache_rejects_foreign_future_and_linked_files() {
        let dir = Scratch::new();
        let path = dir.source();
        let store = dir.store();
        writer(&path, false);
        stable(&path);
        let before = fs::read(&path).unwrap();
        let db = reader(&path);
        let miss = |location: &Path| {
            let probe = Snapshot::probe_with_store(&db, &path, location.to_owned()).unwrap();
            db.execute_batch("BEGIN").unwrap();
            assert!(probe.open(&db).is_none());
            db.execute_batch("ROLLBACK").unwrap();
        };
        miss(&path);
        fs::create_dir_all(store.parent().unwrap()).unwrap();
        symlink(&path, &store).unwrap();
        miss(&store);
        assert_eq!(fs::read(&path).unwrap(), before);
        fs::remove_file(&store).unwrap();
        fs::hard_link(&path, &store).unwrap();
        assert!(Snapshot::probe_with_store(&db, &path, store.clone()).is_none());
        assert_eq!(fs::read(&path).unwrap(), before);
        fs::remove_file(&store).unwrap();
        stable(&path);
        create_file(&store).unwrap().write_all(b"foreign bytes").unwrap();
        miss(&store);
        assert_eq!(fs::read(&store).unwrap(), b"foreign bytes");
        fs::remove_file(&store).unwrap();
        using(&path, &store, |_| {});
        Connection::open(&store).unwrap().pragma_update(None, "user_version", 100).unwrap();
        let future = fs::read(&store).unwrap();
        miss(&store);
        assert_eq!(fs::read(&store).unwrap(), future);
    }

    #[test]
    fn cache_payload_integrity_and_size_limit_fall_back_to_a_miss() {
        let dir = Scratch::new();
        let path = dir.source();
        let store = dir.store();
        writer(&path, false);
        using(&path, &store, |cache| {
            cache.lookup_many(KIND, 1, &[FIRST, SECOND]);
            let entries = [(FIRST, vec![1]), (SECOND, vec![0; MAX_PAYLOAD + 1])];
            assert_eq!(cache.write_many(KIND, 1, &entries), 1);
        });
        assert_eq!(count_facts(&store), 1);
        Connection::open(&store).unwrap().execute("UPDATE facts SET payload=x'02'", []).unwrap();
        using(&path, &store, |cache| {
            assert!(cache.lookup_many(KIND, 1, &[FIRST]).is_empty());
            cache.write_many(KIND, 1, &[(FIRST, vec![3])]);
            assert_eq!(cache.lookup_many(KIND, 1, &[FIRST])[FIRST], [3]);
        });
    }

    #[test]
    fn cache_write_count_reflects_committed_rows() {
        let dir = Scratch::new();
        let path = dir.source();
        let store = dir.store();
        writer(&path, false);
        using(&path, &store, |cache| {
            let entries = [(FIRST, vec![1]), (FIRST, vec![2])];
            cache.lookup_many(KIND, 1, &[FIRST]);
            let observer = reader(&store);
            let tx = observer.unchecked_transaction().unwrap();
            // A pinned observer lets the writer reserve the journal, then blocks commit.
            let before: i64 = tx.query_row("SELECT count(*) FROM facts", [], |row| row.get(0)).unwrap();
            assert_eq!(before, 0);
            assert_eq!(cache.write_many(KIND, 1, &entries), 0);
            tx.rollback().unwrap();
            assert!(cache.lookup_many(KIND, 1, &[FIRST]).is_empty());
            assert_eq!(cache.write_many(KIND, 1, &entries), 1);
            assert_eq!(cache.lookup_many(KIND, 1, &[FIRST])[FIRST], [2]);
        });
    }

    #[test]
    fn active_wal_preserves_unchanged_runs_and_rejects_an_interior_overflow_edit() {
        let dir = Scratch::new();
        let path = dir.source();
        let store = dir.store();
        let writer = writer(&path, true);
        let payload = format!(r#"{{"summary":"{}"}}"#, "a".repeat(24_000));
        writer.execute(
            "INSERT INTO orchestration_v2_projection_turn_items(run_id,ordinal,updated_at,payload_json)
             VALUES (?1,1,'2026-11-09',?2)", params![SECOND, payload],
        ).unwrap();
        let original_stamp = using(&path, &store, |cache| {
            cache.lookup_many(KIND, 1, &[FIRST, SECOND]);
            assert_eq!(cache.write_many(KIND, 1, &[(FIRST, vec![1]), (SECOND, vec![2])]), 2);
            cache.lookup_many("other-reader", 1, &[FIRST, SECOND]);
            assert_eq!(cache.write_many("other-reader", 1, &[(FIRST, vec![3]), (SECOND, vec![4])]), 2);
            let with_overflow: i64 = cache.store.query_row(
                "SELECT count(*) FROM facts WHERE run=?1 AND length(witness)>16", [digest("run", &SECOND)], |row| row.get(0),
            ).unwrap();
            assert_eq!(with_overflow, 2);
            cache.wal_stamp.clone().unwrap()
        });
        writer.execute_batch(
            "BEGIN;
             UPDATE orchestration_v2_projection_turn_items SET payload_json=substr(payload_json,1,9500)||'b'||substr(payload_json,9502)
             WHERE run_id='sensitive-run-two' AND ordinal=1;
             INSERT INTO unrelated_events VALUES ('unrelated-thread','unrelated-message');
             COMMIT;",
        ).unwrap();
        using(&path, &store, |cache| {
            assert_ne!(cache.wal_stamp.as_ref(), Some(&original_stamp));
            let hits = cache.lookup_many(KIND, 1, &[FIRST, SECOND]);
            assert_eq!(hits, [(FIRST.to_owned(), vec![1])].into_iter().collect());
            let other_hits = cache.lookup_many("other-reader", 1, &[FIRST, SECOND]);
            assert_eq!(other_hits, [(FIRST.to_owned(), vec![3])].into_iter().collect());
        });
        assert_eq!(count_facts(&store), 2);
        // The next generation can still trust the carried proof, but a damaged copy
        // does not match its stored integrity even when an unrelated commit is all that changed.
        Connection::open(&store).unwrap().execute(
            "UPDATE facts SET witness=zeroblob(length(witness)) WHERE kind=?1",
            [digest("kind", &"other-reader")],
        ).unwrap();
        writer.execute("INSERT INTO unrelated_events VALUES ('another-thread','another-message')", []).unwrap();
        using(&path, &store, |cache| {
            assert_eq!(cache.lookup_many(KIND, 1, &[FIRST])[FIRST], [1]);
            assert!(cache.lookup_many("other-reader", 1, &[FIRST]).is_empty());
        });
        assert_eq!(count_facts(&store), 1);
    }

    #[test]
    fn active_wal_prunes_out_of_period_membership_and_thread_changes() {
        let dir = Scratch::new();
        let path = dir.source();
        let store = dir.store();
        let writer = writer(&path, true);
        writer.execute_batch(
            r#"DELETE FROM orchestration_v2_projection_turn_items;
             INSERT INTO orchestration_v2_projection_threads VALUES ('outside',NULL,NULL);
             INSERT INTO orchestration_v2_projection_runs VALUES
             ('outside','outside','completed','2026-11-10','{"status":"completed"}');"#,
        ).unwrap();
        using(&path, &store, |cache| {
            cache.lookup_many(KIND, 1, &[FIRST, SECOND, "outside"]);
            let entries = [(FIRST, vec![1]), (SECOND, vec![2]), ("outside", vec![3])];
            assert_eq!(cache.write_many(KIND, 1, &entries), 3);
        });
        writer.execute_batch(
            "BEGIN;
             INSERT INTO orchestration_v2_projection_turn_items(run_id,ordinal,updated_at,payload_json)
             VALUES ('sensitive-run-two',9,'2026-11-01','{}');
             DELETE FROM orchestration_v2_projection_threads WHERE thread_id='outside';
             COMMIT;",
        ).unwrap();
        using(&path, &store, |cache| {
            assert_eq!(cache.lookup_many(KIND, 1, &[FIRST])[FIRST], [1]);
        });
        assert_eq!(count_facts(&store), 1);
    }

    #[test]
    fn a_checkpoint_ends_selective_reuse() {
        let dir = Scratch::new();
        let path = dir.source();
        let store = dir.store();
        let writer = writer(&path, true);
        using(&path, &store, |cache| {
            assert!(cache.wal_stamp.is_some());
            cache.lookup_many(KIND, 1, &[FIRST]);
            assert_eq!(cache.write_many(KIND, 1, &[(FIRST, vec![1])]), 1);
        });
        writer.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        writer.execute("INSERT INTO unrelated_events VALUES ('unrelated','checkpoint')", []).unwrap();
        using(&path, &store, |cache| {
            assert!(cache.lookup_many(KIND, 1, &[FIRST]).is_empty());
        });
        assert_eq!(count_facts(&store), 0);
    }

    #[test]
    fn a_source_without_the_required_run_index_uses_generation_only() {
        let dir = Scratch::new();
        let path = dir.source();
        let store = dir.store();
        let writer = writer(&path, true);
        writer.execute_batch("DROP INDEX orchestration_v2_projection_turn_items_run_ordinal_idx").unwrap();
        using(&path, &store, |cache| {
            cache.lookup_many(KIND, 1, &[FIRST]);
            assert_eq!(cache.write_many(KIND, 1, &[(FIRST, vec![1])]), 1);
            let locator: Option<i64> = cache.store.query_row("SELECT locator FROM facts", [], |row| row.get(0)).unwrap();
            assert!(locator.is_none());
        });
        using(&path, &store, |cache| {
            assert_eq!(cache.lookup_many(KIND, 1, &[FIRST])[FIRST], [1]);
        });
        writer.execute("INSERT INTO unrelated_events VALUES ('unrelated','write')", []).unwrap();
        using(&path, &store, |cache| {
            assert!(cache.lookup_many(KIND, 1, &[FIRST]).is_empty());
        });
        assert_eq!(count_facts(&store), 0);
    }

    #[test]
    fn selective_queries_use_locators_and_run_indexes() {
        let dir = Scratch::new();
        let db = writer(&dir.source(), false);
        let plan = |db: &Connection, sql: &str, args: &[&dyn rusqlite::ToSql]| {
            let mut stmt = db.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let rows = stmt.query_map(args, |r| r.get::<_, String>(3)).unwrap();
            rows.collect::<rusqlite::Result<Vec<_>>>().unwrap().join("; ")
        };
        let locator = plan(&db, LOCATOR_OF_RUN, &[&FIRST]);
        assert!(locator.contains("run_id=?"), "{locator}");
        let run = plan(&db, &format!("{RUN_AT_LOCATOR} AND {ELIGIBLE}"), &[&1i64]);
        assert!(run.contains("SEARCH r USING INTEGER PRIMARY KEY (rowid=?)"), "{run}");
        assert!(run.contains("SEARCH t") && run.contains("thread_id=?"), "{run}");
        using(&dir.source(), &dir.store(), |cache| {
            let scan = plan(&cache.store, REVALIDATE, &[&cache.source, &128_i64, &10_i64]);
            assert!(scan.contains("SEARCH facts USING INDEX facts_by_run"), "{scan}");
            assert!(!scan.contains("TEMP B-TREE"), "{scan}");
        });
    }

    #[test]
    fn an_exact_owned_v1_store_is_rebuilt_for_the_new_format() {
        let dir = Scratch::new();
        let path = dir.source();
        let store = dir.store();
        writer(&path, false);
        fs::create_dir(store.parent().unwrap()).unwrap();
        create_file(&store.with_file_name("insights.owner")).unwrap().write_all(OWNER).unwrap();
        create_file(&store).unwrap();
        {
            let legacy = Connection::open(&store).unwrap();
            legacy.pragma_update(None, "application_id", APPLICATION_ID).unwrap();
            legacy.pragma_update(None, "user_version", 1).unwrap();
            legacy.execute(SOURCES_V1, []).unwrap();
            legacy.execute(FACTS_V1, []).unwrap();
            legacy.execute("INSERT INTO facts VALUES(zeroblob(16),zeroblob(16),1,zeroblob(16),x'01',zeroblob(16))", []).unwrap();
        }
        using(&path, &store, |cache| {
            let version: u32 = cache.store.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
            assert_eq!(version, FORMAT);
            assert!(cache.lookup_many(KIND, 1, &[FIRST]).is_empty());
        });
        assert_eq!(count_facts(&store), 0);
    }
}
