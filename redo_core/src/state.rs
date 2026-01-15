//! redo state database and locking.

use std::collections::HashMap;
use std::fs;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{fork, ForkResult};
use rusqlite::{params, Connection, OptionalExtension};

use crate::cycles;
use crate::env;
use crate::helpers::close_on_exec;
use crate::logs::Log;

pub const SCHEMA_VER: i32 = 2;
pub const TIMEOUT_SECS: u64 = 60;

pub const ALWAYS: &str = "//ALWAYS";
pub const STAMP_DIR: &str = "dir";
pub const STAMP_MISSING: &str = "0";

pub const LOG_LOCK_MAGIC: i64 = 0x1000_0000;

static STATE: OnceLock<Mutex<StateInner>> = OnceLock::new();

#[derive(Debug)]
struct StateInner {
    db: Connection,
    lockfd: RawFd,
    insane: bool,
    locks_held: HashMap<i64, bool>,
    wrote: u64,
    in_tx: bool,
}

pub fn init(targets: &[String]) -> anyhow::Result<()> {
    env::init(targets)?;
    db()?;
    if env::is_toplevel() && detect_broken_locks()? {
        env::mark_locks_broken()?;
    }
    Ok(())
}

fn connect(dbfile: &Path) -> anyhow::Result<Connection> {
    let db = Connection::open(dbfile)?;
    db.busy_timeout(Duration::from_secs(TIMEOUT_SECS))?;
    db.pragma_update(None, "synchronous", "OFF")?;
    // journal mode: WAL normally, PERSIST when locks are broken (eg. WSL).
    let jmode = if env::v().locks_broken {
        "PERSIST"
    } else {
        "WAL"
    };
    db.pragma_update(None, "journal_mode", jmode)?;
    Ok(db)
}

fn ensure_state() -> &'static Mutex<StateInner> {
    STATE.get().expect("state not initialized")
}

pub fn db() -> anyhow::Result<()> {
    if STATE.get().is_some() {
        return Ok(());
    }

    let base = env::v().base.clone();
    let dbdir = base.join(".redo");
    let dbfile = dbdir.join("db.sqlite3");

    fs::create_dir_all(&dbdir)?;

    let lockfile = base.join(".redo/locks");
    let lockfd = nix::fcntl::open(
        &lockfile,
        nix::fcntl::OFlag::O_RDWR | nix::fcntl::OFlag::O_CREAT,
        nix::sys::stat::Mode::from_bits_truncate(0o666),
    )?;
    close_on_exec(lockfd, true)?;

    // Detect broken locks early (before opening sqlite too).
    if env::is_toplevel() && detect_broken_locks_fd(lockfd)? {
        env::mark_locks_broken()?;
    }

    let must_create = !dbfile.exists();
    let db = connect(&dbfile)?;

    if !must_create {
        let ver: Option<i32> = db
            .query_row("select version from Schema", [], |r| r.get(0))
            .optional()?;
        if ver != Some(SCHEMA_VER) {
            eprintln!(
                "redo: {}: found v{:?} (expected v{})",
                dbfile.to_string_lossy(),
                ver,
                SCHEMA_VER
            );
            eprintln!("redo: manually delete .redo dir to start over.");
            std::process::exit(1);
        }
    } else {
        let _ = fs::remove_file(&dbfile);
        let db = connect(&dbfile)?;
        db.execute("create table Schema (version int)", [])?;
        db.execute(
            "create table Runid (id integer primary key autoincrement)",
            [],
        )?;
        db.execute(
            "create table Files (name not null primary key, \
             is_generated int, is_override int, \
             checked_runid int, changed_runid int, failed_runid int, \
             stamp, csum)",
            [],
        )?;
        db.execute(
            "create table Deps (target int, source int, mode not null, delete_me int, \
             primary key (target,source))",
            [],
        )?;
        db.execute("insert into Schema (version) values (?)", [SCHEMA_VER])?;
        db.execute("insert into Runid values (1000000000)", [])?;
        db.execute("insert into Files (name) values (?)", [ALWAYS])?;

        let inner = StateInner {
            db,
            lockfd,
            insane: false,
            locks_held: HashMap::new(),
            wrote: 0,
            in_tx: false,
        };
        let _ = STATE.set(Mutex::new(inner));
        ensure_runid()?;
        commit()?;
        return Ok(());
    }

    let inner = StateInner {
        db,
        lockfd,
        insane: false,
        locks_held: HashMap::new(),
        wrote: 0,
        in_tx: false,
    };
    let _ = STATE.set(Mutex::new(inner));
    ensure_runid()?;
    commit()?;
    Ok(())
}

fn ensure_runid() -> anyhow::Result<()> {
    if env::v().runid.is_some() {
        return Ok(());
    }
    // Use the write path so transaction bookkeeping stays consistent.
    write(
        "insert into Runid values ((select max(id)+1 from Runid))",
        &[],
    )?;
    // last_insert_rowid is per-connection; safe to query after the insert.
    let runid: i64 = {
        let st = ensure_state();
        let s = st.lock().unwrap();
        s.db.query_row("select last_insert_rowid()", [], |r| r.get(0))?
    };
    std::env::set_var("REDO_RUNID", runid.to_string());
    env::inherit()?; // refresh cached env
    Ok(())
}

pub fn commit() -> anyhow::Result<()> {
    let st = ensure_state();
    let mut s = st.lock().unwrap();
    if s.insane {
        return Ok(());
    }
    if s.wrote == 0 {
        return Ok(());
    }
    if s.in_tx {
        // Commit only when writes occurred.
        s.db.execute_batch("COMMIT")?;
        s.in_tx = false;
    }
    s.wrote = 0;
    Ok(())
}

pub fn rollback() -> anyhow::Result<()> {
    let st = ensure_state();
    let mut s = st.lock().unwrap();
    if s.insane {
        return Ok(());
    }
    if s.wrote == 0 {
        return Ok(());
    }
    if s.in_tx {
        s.db.execute_batch("ROLLBACK")?;
        s.in_tx = false;
    }
    s.wrote = 0;
    Ok(())
}

pub fn is_flushed() -> bool {
    let st = ensure_state();
    let s = st.lock().unwrap();
    s.wrote == 0
}

pub fn check_sane() -> bool {
    let base = env::v().base.clone();
    let st = ensure_state();
    let mut s = st.lock().unwrap();
    if !s.insane {
        s.insane = !base.join(".redo").exists();
    }
    !s.insane
}

fn write(sql: &str, p: &[&dyn rusqlite::ToSql]) -> anyhow::Result<()> {
    let st = ensure_state();
    let mut s = st.lock().unwrap();
    if s.insane {
        return Ok(());
    }
    // sqlite starts an implicit transaction on first write; we emulate
    // that so commit()/rollback()/is_flushed() have real meaning.
    if !s.in_tx {
        // Use deferred BEGIN like sqlite3 default behavior.
        s.db.execute_batch("BEGIN")?;
        s.in_tx = true;
    }
    s.wrote = s.wrote.saturating_add(1);
    s.db.execute(sql, p)?;
    Ok(())
}

/// Compute a relative path from `base` to `t`.
///
/// Semantics:
/// - interpret relative paths relative to the current directory
/// - normalize `.`/`..`
/// - resolve symlinks for *directory components* but not the final path element
pub fn relpath(t: &Path, base: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let t_abs = if t.is_absolute() {
        t.to_path_buf()
    } else {
        cwd.join(t)
    };
    let base_abs = if base.is_absolute() {
        base.to_path_buf()
    } else {
        cwd.join(base)
    };

    let t_norm = normalize_path(&realdirpath(&t_abs));
    let b_norm = normalize_path(&realdirpath(&base_abs));
    pathdiff::diff_paths(&t_norm, &b_norm).unwrap_or(t_norm)
}

/// Return a relative path for `t` that will work after a `.do` script does:
/// `cd $(dirname(REDO_TARGET))`.
pub fn target_relpath(t: &Path) -> PathBuf {
    let e = env::v();
    if e.startdir.is_empty() || e.target.is_empty() {
        return relpath(t, &e.base);
    }
    let dofile_dir = PathBuf::from(e.startdir).join(PathBuf::from(e.pwd));
    let target_dir = dofile_dir
        .join(PathBuf::from(e.target))
        .parent()
        .unwrap_or(dofile_dir.as_path())
        .to_path_buf();
    relpath(t, &target_dir)
}

pub fn logname(fid: i64) -> PathBuf {
    env::v().base.join(".redo").join(format!("log.{}", fid))
}

pub fn detect_override(stamp1: &str, stamp2: &str) -> bool {
    if stamp1 == stamp2 {
        return false;
    }
    let crit1: Vec<&str> = stamp1.splitn(3, '-').take(2).collect();
    let crit2: Vec<&str> = stamp2.splitn(3, '-').take(2).collect();
    crit1 != crit2
}

pub fn warn_override(name: &str) {
    Log::warn(&format!("{} - you modified it; skipping", name));
}

#[derive(Debug, Clone)]
pub struct FileRow {
    pub id: i64,
    pub name: String,
    pub is_generated: Option<i64>,
    pub is_override: Option<i64>,
    pub checked_runid: Option<i64>,
    pub changed_runid: Option<i64>,
    pub failed_runid: Option<i64>,
    pub stamp: Option<String>,
    pub csum: Option<String>,
}

#[derive(Debug, Clone)]
pub struct File {
    pub id: i64,
    pub name: String,
    pub is_generated: bool,
    pub is_override: bool,
    pub checked_runid: Option<i64>,
    pub changed_runid: Option<i64>,
    pub failed_runid: Option<i64>,
    pub stamp: Option<String>,
    pub csum: Option<String>,
}

impl File {
    pub fn by_name(name: &str, allow_add: bool) -> anyhow::Result<Self> {
        let name = if name == ALWAYS {
            ALWAYS.to_string()
        } else {
            // Store names in db relative to BASE.
            let abs = if Path::new(name).is_absolute() {
                PathBuf::from(name)
            } else {
                std::env::current_dir()?.join(name)
            };
            // normpath(realdirpath(join(cwd, t))) with special handling to resolve symlinks
            // for the directory part but not the final element.
            let abs = normalize_path(&realdirpath(&abs));
            let base = normalize_path(&realdirpath(&env::v().base));
            let rel = pathdiff::diff_paths(&abs, &base).unwrap_or(abs);
            rel.to_string_lossy().to_string()
        };

        let st = ensure_state();
        let mut s = st.lock().unwrap();
        let mut row: Option<FileRow> = s
            .db
            .query_row(
                "select rowid, name, is_generated, is_override, checked_runid, changed_runid, failed_runid, stamp, csum \
                 from Files where name=?",
                params![name],
                |r| {
                    Ok(FileRow {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        is_generated: r.get(2)?,
                        is_override: r.get(3)?,
                        checked_runid: r.get(4)?,
                        changed_runid: r.get(5)?,
                        failed_runid: r.get(6)?,
                        stamp: r.get(7)?,
                        csum: r.get(8)?,
                    })
                },
            )
            .optional()?;
        if row.is_none() && allow_add && !name.is_empty() {
            drop(s);
            // Multiple redo processes can discover the same new filename concurrently.
            // Make insertion idempotent to avoid UNIQUE constraint races.
            write("insert or ignore into Files (name) values (?)", &[&name])?;
            s = st.lock().unwrap();
            row = s
                .db
                .query_row(
                    "select rowid, name, is_generated, is_override, checked_runid, changed_runid, failed_runid, stamp, csum \
                     from Files where name=?",
                    params![name],
                    |r| {
                        Ok(FileRow {
                            id: r.get(0)?,
                            name: r.get(1)?,
                            is_generated: r.get(2)?,
                            is_override: r.get(3)?,
                            checked_runid: r.get(4)?,
                            changed_runid: r.get(5)?,
                            failed_runid: r.get(6)?,
                            stamp: r.get(7)?,
                            csum: r.get(8)?,
                        })
                    },
                )
                .optional()?;
        }

        let row = row.ok_or_else(|| anyhow::anyhow!("No file with name={}", name))?;
        Ok(Self::from_row(row))
    }

    fn from_row(r: FileRow) -> Self {
        let runid = env::v().runid;
        let mut changed_runid = r.changed_runid;
        if r.name == ALWAYS && (changed_runid.is_none() || changed_runid < runid) {
            changed_runid = runid;
        }
        Self {
            id: r.id,
            name: r.name,
            is_generated: r.is_generated.unwrap_or(0) != 0,
            is_override: r.is_override.unwrap_or(0) != 0,
            checked_runid: r.checked_runid,
            changed_runid,
            failed_runid: r.failed_runid,
            stamp: r.stamp,
            csum: r.csum,
        }
    }

    pub fn refresh(&mut self) -> anyhow::Result<()> {
        let st = ensure_state();
        let s = st.lock().unwrap();
        let row: FileRow = s.db.query_row(
            "select rowid, name, is_generated, is_override, checked_runid, changed_runid, failed_runid, stamp, csum from Files where rowid=?",
            params![self.id],
            |r| {
                Ok(FileRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    is_generated: r.get(2)?,
                    is_override: r.get(3)?,
                    checked_runid: r.get(4)?,
                    changed_runid: r.get(5)?,
                    failed_runid: r.get(6)?,
                    stamp: r.get(7)?,
                    csum: r.get(8)?,
                })
            },
        )?;
        *self = Self::from_row(row);
        Ok(())
    }

    pub fn save(&self) -> anyhow::Result<()> {
        write(
            "update Files set is_generated=?, is_override=?, checked_runid=?, changed_runid=?, failed_runid=?, stamp=?, csum=? where rowid=?",
            &[
                &(self.is_generated as i64),
                &(self.is_override as i64),
                &self.checked_runid,
                &self.changed_runid,
                &self.failed_runid,
                &self.stamp,
                &self.csum,
                &self.id,
            ],
        )
    }

    pub fn set_checked(&mut self) {
        self.checked_runid = env::v().runid;
    }

    pub fn set_checked_save(&mut self) -> anyhow::Result<()> {
        self.set_checked();
        self.save()
    }

    pub fn set_changed(&mut self) {
        self.changed_runid = env::v().runid;
        self.failed_runid = None;
        self.is_override = false;
    }

    pub fn set_failed(&mut self) -> anyhow::Result<()> {
        self.update_stamp(false)?;
        self.failed_runid = env::v().runid;
        if self.stamp.as_deref() != Some(STAMP_MISSING) {
            // If we failed and the target file still exists, then it's generated.
            self.is_generated = true;
        } else {
            // If the target file now does *not* exist, treat this as a source again.
            // Since it doesn't exist, trying to rebuild will reclassify it as a target,
            // but if the file is manually created before that, we avoid a manual-override
            // warning.
            self.is_generated = false;
        }
        Ok(())
    }

    pub fn set_static(&mut self) -> anyhow::Result<()> {
        self.update_stamp(true)?;
        self.failed_runid = None;
        self.is_override = false;
        self.is_generated = false;
        Ok(())
    }

    pub fn set_override(&mut self) -> anyhow::Result<()> {
        self.update_stamp(false)?;
        self.failed_runid = None;
        self.is_override = true;
        Ok(())
    }

    pub fn is_checked(&self) -> bool {
        match (self.checked_runid, env::v().runid) {
            (Some(a), Some(b)) => a >= b,
            _ => false,
        }
    }

    pub fn is_changed(&self) -> bool {
        match (self.changed_runid, env::v().runid) {
            (Some(a), Some(b)) => a >= b,
            _ => false,
        }
    }

    pub fn is_failed(&self) -> bool {
        match (self.failed_runid, env::v().runid) {
            (Some(a), Some(b)) => a >= b,
            _ => false,
        }
    }

    pub fn update_stamp(&mut self, must_exist: bool) -> anyhow::Result<()> {
        let newstamp = self.read_stamp()?;
        if must_exist && newstamp == STAMP_MISSING {
            anyhow::bail!("{} does not exist", self.name);
        }
        if self.stamp.as_deref().unwrap_or("") != newstamp {
            self.stamp = Some(newstamp);
            self.set_changed();
        }
        Ok(())
    }

    fn read_stamp_st(path: &Path, follow: bool) -> (bool, String) {
        use std::os::unix::fs::MetadataExt;
        let meta = if follow {
            fs::metadata(path)
        } else {
            fs::symlink_metadata(path)
        };
        let meta = match meta {
            Ok(m) => m,
            Err(_) => return (false, STAMP_MISSING.to_string()),
        };
        if meta.is_dir() {
            return (false, STAMP_DIR.to_string());
        }
        let is_link = meta.file_type().is_symlink();
        let mt = meta.mtime() as f64 + (meta.mtime_nsec() as f64) / 1e9;
        let stamp = format!(
            "{:.6}-{}-{}-{}-{}-{}",
            mt,
            meta.size(),
            meta.ino(),
            meta.mode(),
            meta.uid(),
            meta.gid()
        );
        (is_link, stamp)
    }

    pub fn read_stamp(&self) -> anyhow::Result<String> {
        let path = env::v().base.join(&self.name);
        let (is_link, pre) = Self::read_stamp_st(&path, false);
        if is_link {
            let (_, post) = Self::read_stamp_st(&path, true);
            Ok(format!("{}+{}", pre, post))
        } else {
            Ok(pre)
        }
    }

    pub fn is_source(&self) -> anyhow::Result<bool> {
        if self.name.starts_with("//") {
            return Ok(false);
        }
        let newstamp = self.read_stamp()?;
        if self.is_generated
            && (!self.is_failed() || newstamp != STAMP_MISSING)
            && !self.is_override
            && self.stamp.as_deref() == Some(&newstamp)
        {
            return Ok(false);
        }
        if (!self.is_generated || self.stamp.as_deref() != Some(&newstamp))
            && newstamp == STAMP_MISSING
        {
            return Ok(false);
        }
        Ok(true)
    }

    pub fn is_target(&self) -> anyhow::Result<bool> {
        if !self.is_generated {
            return Ok(false);
        }
        Ok(!self.is_source()?)
    }

    pub fn deps(&self) -> anyhow::Result<Vec<(char, File)>> {
        if self.is_override || !self.is_generated {
            return Ok(vec![]);
        }
        let st = ensure_state();
        let s = st.lock().unwrap();
        let mut stmt = s.db.prepare(
            "select Deps.mode, Files.rowid, Files.name, Files.is_generated, Files.is_override, \
             Files.checked_runid, Files.changed_runid, Files.failed_runid, Files.stamp, Files.csum \
             from Files join Deps on Files.rowid = Deps.source where target=?",
        )?;
        let rows = stmt.query_map(params![self.id], |r| {
            let mode: String = r.get(0)?;
            let cols = FileRow {
                id: r.get(1)?,
                name: r.get(2)?,
                is_generated: r.get(3)?,
                is_override: r.get(4)?,
                checked_runid: r.get(5)?,
                changed_runid: r.get(6)?,
                failed_runid: r.get(7)?,
                stamp: r.get(8)?,
                csum: r.get(9)?,
            };
            Ok((mode.chars().next().unwrap_or('m'), File::from_row(cols)))
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    pub fn zap_deps1(&self) -> anyhow::Result<()> {
        write(
            "update Deps set delete_me=? where target=?",
            &[&1i64, &self.id],
        )
    }

    pub fn zap_deps2(&self) -> anyhow::Result<()> {
        write(
            "delete from Deps where target=? and delete_me=1",
            &[&self.id],
        )
    }

    pub fn add_dep(&self, mode: char, dep: &str) -> anyhow::Result<()> {
        let src = File::by_name(dep, true)?;
        write(
            "insert or replace into Deps (target, mode, source, delete_me) values (?,?,?,?)",
            &[&self.id, &mode.to_string(), &src.id, &0i64],
        )
    }
}

fn normalize_path(p: &Path) -> PathBuf {
    // Lexical normalization: removes '.' and resolves '..' without touching the filesystem.
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for c in p.components() {
        use std::path::Component;
        match c {
            Component::Prefix(pre) => parts.push(pre.as_os_str().to_os_string()),
            Component::RootDir => {
                parts.clear();
                parts.push(std::ffi::OsString::from("/"));
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.len() > 1 {
                    parts.pop();
                }
            }
            Component::Normal(s) => parts.push(s.to_os_string()),
        }
    }
    if parts.is_empty() {
        return PathBuf::new();
    }
    let mut out = PathBuf::new();
    for (i, p) in parts.iter().enumerate() {
        if i == 0 && p == "/" {
            out.push(Path::new("/"));
        } else {
            out.push(p);
        }
    }
    out
}

fn realdirpath(t: &Path) -> PathBuf {
    // Like realpath(), but don't follow symlinks for the last element.
    let dname = t.parent().unwrap_or(Path::new(""));
    let fname = t.file_name().unwrap_or_default();
    if dname.as_os_str().is_empty() {
        return PathBuf::from(fname);
    }
    let dreal = std::fs::canonicalize(dname).unwrap_or_else(|_| normalize_path(dname));
    dreal.join(fname)
}

pub fn files() -> anyhow::Result<Vec<File>> {
    let st = ensure_state();
    let s = st.lock().unwrap();
    let mut stmt = s.db.prepare(
        "select rowid, name, is_generated, is_override, checked_runid, changed_runid, failed_runid, stamp, csum \
         from Files order by name",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(File::from_row(FileRow {
            id: r.get(0)?,
            name: r.get(1)?,
            is_generated: r.get(2)?,
            is_override: r.get(3)?,
            checked_runid: r.get(4)?,
            changed_runid: r.get(5)?,
            failed_runid: r.get(6)?,
            stamp: r.get(7)?,
            csum: r.get(8)?,
        }))
    })?;
    Ok(rows.filter_map(Result::ok).collect())
}

pub struct Lock {
    fid: i64,
    owned: bool,
}

impl Lock {
    pub fn new(fid: i64) -> anyhow::Result<Self> {
        let st = ensure_state();
        let mut s = st.lock().unwrap();
        if s.locks_held.get(&fid) == Some(&true) {
            anyhow::bail!("Lock already created for fid={}", fid);
        }
        s.locks_held.insert(fid, false);
        Ok(Self { fid, owned: false })
    }

    pub fn check(&self) -> anyhow::Result<()> {
        if self.owned {
            return Ok(());
        }
        cycles::check(self.fid).map_err(|e| anyhow::anyhow!(e))?;
        Ok(())
    }

    pub fn trylock(&mut self) -> anyhow::Result<bool> {
        self.check()?;
        let st = ensure_state();
        let s = st.lock().unwrap();
        let ok = lock_region(s.lockfd, self.fid, false, true)?;
        drop(s);
        self.owned = ok;
        if ok {
            let st = ensure_state();
            let mut s = st.lock().unwrap();
            s.locks_held.insert(self.fid, true);
        }
        Ok(ok)
    }

    pub fn waitlock(&mut self, shared: bool) -> anyhow::Result<()> {
        self.check()?;
        let st = ensure_state();
        let s = st.lock().unwrap();
        lock_region(s.lockfd, self.fid, shared, false)?;
        drop(s);
        self.owned = true;
        let st = ensure_state();
        let mut s = st.lock().unwrap();
        s.locks_held.insert(self.fid, true);
        Ok(())
    }

    pub fn unlock(&mut self) -> anyhow::Result<()> {
        if !self.owned {
            anyhow::bail!("can't unlock fid={} - not owned", self.fid);
        }
        let st = ensure_state();
        let s = st.lock().unwrap();
        unlock_region(s.lockfd, self.fid)?;
        drop(s);
        self.owned = false;
        let st = ensure_state();
        let mut s = st.lock().unwrap();
        s.locks_held.insert(self.fid, false);
        Ok(())
    }

    pub fn fid(&self) -> i64 {
        self.fid
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let st = STATE.get();
        if st.is_none() {
            return;
        }
        if self.owned {
            let _ = self.unlock();
        }
        if let Some(st) = st {
            let mut s = st.lock().unwrap();
            s.locks_held.remove(&self.fid);
        }
    }
}

fn lock_region(fd: RawFd, fid: i64, shared: bool, nonblock: bool) -> anyhow::Result<bool> {
    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = if shared {
        libc::F_RDLCK as i16
    } else {
        libc::F_WRLCK as i16
    };
    fl.l_whence = libc::SEEK_SET as i16;
    fl.l_start = fid as libc::off_t;
    fl.l_len = 1;
    let cmd = if nonblock {
        libc::F_SETLK
    } else {
        libc::F_SETLKW
    };
    let rc = unsafe { libc::fcntl(fd, cmd, &fl) };
    if rc == 0 {
        Ok(true)
    } else {
        let e = std::io::Error::last_os_error();
        if nonblock {
            if matches!(e.raw_os_error(), Some(libc::EACCES) | Some(libc::EAGAIN)) {
                return Ok(false);
            }
        }
        Err(anyhow::anyhow!(e))
    }
}

fn unlock_region(fd: RawFd, fid: i64) -> anyhow::Result<()> {
    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = libc::F_UNLCK as i16;
    fl.l_whence = libc::SEEK_SET as i16;
    fl.l_start = fid as libc::off_t;
    fl.l_len = 1;
    let rc = unsafe { libc::fcntl(fd, libc::F_SETLK, &fl) };
    if rc == 0 {
        Ok(())
    } else {
        Err(anyhow::anyhow!(std::io::Error::last_os_error()))
    }
}

pub fn detect_broken_locks() -> anyhow::Result<bool> {
    let st = ensure_state();
    let s = st.lock().unwrap();
    detect_broken_locks_fd(s.lockfd)
}

fn detect_broken_locks_fd(lockfd: RawFd) -> anyhow::Result<bool> {
    // Parent holds exclusive lock on fid=0, child tries to acquire; if it succeeds => broken.
    // We waitlock to avoid concurrent tests.
    lock_region(lockfd, 0, false, false)?;
    match unsafe { fork()? } {
        ForkResult::Parent { child } => {
            let status = waitpid(child, None)?;
            unlock_region(lockfd, 0)?;
            Ok(!matches!(status, WaitStatus::Exited(_, 0)))
        }
        ForkResult::Child => {
            // Try to get the same lock; should fail if locks work.
            let ok = lock_region(lockfd, 0, false, true).unwrap_or(false);
            if ok {
                std::process::exit(1);
            } else {
                std::process::exit(0);
            }
        }
    }
}
