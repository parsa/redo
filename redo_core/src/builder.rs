//! Build execution engine.
//!
//! Responsibilities:
//! - Locate the appropriate `.do` file for a target.
//! - Execute the build script with correct `$1/$2/$3` semantics.
//! - Write outputs atomically and record dependencies/state.
//! - Coordinate parallelism via the GNU make jobserver and integrate with `redo-log`.

use std::ffi::CString;
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicI32, Ordering};

use crate::{action_cache, cycles, deps, env, jobserver, logs::Log, paths, remote_cache, state};
use sha2::Digest;
use std::time::{SystemTime, UNIX_EPOCH};

const STRICT_POLICY_READTRACE: &str = "strict-readtrace";

fn record_fsync_marker() {
    let Ok(p) = std::env::var("REDO_TEST_FSYNC_MARKER") else { return; };
    if p.is_empty() {
        return;
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(p) {
        let _ = writeln!(f, "fsync");
    }
}

fn fsync_path_best_effort(p: &Path) {
    if let Ok(f) = fs::File::open(p) {
        let _ = f.sync_all();
    }
}

fn durable_rename(tmp: &Path, target: &Path) -> anyhow::Result<()> {
    // Best-effort durability: sync file data, then rename, then sync directory entries.
    // This strengthens crash-safety beyond pure atomic rename semantics.
    fsync_path_best_effort(tmp);
    fs::rename(tmp, target)?;
    if let Some(d) = tmp.parent() {
        fsync_path_best_effort(d);
    }
    if let Some(d) = target.parent() {
        fsync_path_best_effort(d);
    }
    record_fsync_marker();
    Ok(())
}

fn should_test_crash_after_rename() -> bool {
    match std::env::var("REDO_TEST_CRASH_AFTER_RENAME") {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => false,
    }
}

fn persist_new_target_intent(sf: &mut state::File, abs_target: &Path) -> anyhow::Result<()> {
    if abs_target.exists() || sf.is_generated {
        return Ok(());
    }
    // DJB atomic.md: when a missing file is treated as a target, persist that decision
    // before we create/rename any output file.
    sf.is_generated = true;
    sf.is_override = false;
    sf.save()?;
    state::commit()?;
    Ok(())
}

fn try_lstat(p: &Path) -> anyhow::Result<Option<std::fs::Metadata>> {
    match fs::symlink_metadata(p) {
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!(e)),
    }
}

fn unlink_best_effort(p: &Path) {
    let _ = fs::remove_file(p);
}

fn mkstemp_unlinked() -> anyhow::Result<std::fs::File> {
    let mut template = CString::new("/tmp/redo.XXXXXX").unwrap().into_bytes_with_nul();
    let fd = unsafe { libc::mkstemp(template.as_mut_ptr() as *mut libc::c_char) };
    if fd < 0 {
        return Err(anyhow::anyhow!(std::io::Error::last_os_error()));
    }
    // unlink name immediately
    unsafe {
        libc::unlink(template.as_ptr() as *const libc::c_char);
    }
    let f = unsafe { std::fs::File::from_raw_fd(fd) };
    Ok(f)
}

fn read_first_line(p: &Path) -> anyhow::Result<Option<String>> {
    let mut f = match fs::File::open(p) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!(e)),
    };
    let mut s = String::new();
    let _ = f.read_to_string(&mut s)?;
    Ok(s.lines().next().map(|l| l.to_string()))
}

fn normalize_path_lex(p: &Path) -> PathBuf {
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

fn parse_nul_separated_strings(bytes: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for entry in bytes.split(|&b| b == 0) {
        if entry.is_empty() {
            continue;
        }
        out.push(String::from_utf8_lossy(entry).to_string());
    }
    out
}

fn read_trace_out0(path: &Path) -> anyhow::Result<Vec<String>> {
    let bytes = fs::read(path)?;
    Ok(parse_nul_separated_strings(&bytes))
}

fn read_deps_out0(path: &Path) -> anyhow::Result<Vec<(char, String)>> {
    let bytes = fs::read(path)?;
    let parts = parse_nul_separated_strings(&bytes);
    let mut out: Vec<(char, String)> = Vec::new();
    let mut i = 0usize;
    while i + 1 < parts.len() {
        let mode_s = parts[i].trim().to_string();
        let dep_s = parts[i + 1].to_string();
        let mode = mode_s.chars().next().unwrap_or('?');
        if mode == 'm' || mode == 'c' {
            out.push((mode, dep_s));
        }
        i += 2;
    }
    Ok(out)
}

#[derive(Debug)]
struct StrictReadTraceResult {
    trace_ok: bool,
    trace_reason: String,
    violations: Vec<String>,
}

fn evaluate_strict_readtrace(
    base: &Path,
    declared: &[(char, state::File)],
    trace_entries: &[String],
) -> StrictReadTraceResult {
    let redo_dir = normalize_path_lex(&base.join(".redo"));

    let mut allowed: HashSet<PathBuf> = HashSet::new();
    for (_mode, dep) in declared.iter() {
        if dep.name == state::ALWAYS {
            continue;
        }
        let abs = normalize_path_lex(&base.join(&dep.name));
        if abs.starts_with(&redo_dir) {
            continue;
        }
        allowed.insert(abs);
    }

    let mut trace_ok = true;
    let mut trace_reason = String::new();
    let mut observed: HashSet<PathBuf> = HashSet::new();
    for e in trace_entries.iter() {
        if e.is_empty() {
            continue;
        }
        if let Some(rest) = e.strip_prefix("TRACE_UNAVAILABLE:") {
            trace_ok = false;
            if trace_reason.is_empty() {
                trace_reason = rest.to_string();
            }
            continue;
        }
        if let Some(rest) = e.strip_prefix("TRACE_ERROR:") {
            trace_ok = false;
            if trace_reason.is_empty() {
                trace_reason = rest.to_string();
            }
            continue;
        }
        if e.starts_with("UNRESOLVED:") {
            trace_ok = false;
            if trace_reason.is_empty() {
                trace_reason = "unresolved".to_string();
            }
            continue;
        }
        let p = Path::new(e);
        let abs = if p.is_absolute() {
            normalize_path_lex(p)
        } else {
            normalize_path_lex(&base.join(p))
        };
        if abs.starts_with(&redo_dir) {
            continue;
        }
        if abs.starts_with(base) {
            observed.insert(abs);
        }
    }

    let mut violations: Vec<String> = Vec::new();
    for p in observed {
        if !allowed.contains(&p) {
            let rel = p
                .strip_prefix(base)
                .ok()
                .map(|rp| rp.to_string_lossy().to_string())
                .unwrap_or_else(|| p.to_string_lossy().to_string());
            violations.push(rel);
        }
    }
    violations.sort();
    violations.dedup();
    StrictReadTraceResult {
        trace_ok,
        trace_reason,
        violations,
    }
}

fn exec_dofile_in_child(
    dodir: &Path,
    dofile: &str,
    arg1: &str,
    arg2: &str,
    arg3: &str,
    stdout_fd: RawFd,
    lock_fid: i64,
    argv_override: Option<Vec<String>>,
    trace_out0: Option<&Path>,
) -> anyhow::Result<()> {
    let verbose0 = env::v().verbose;
    let xtrace0 = env::v().xtrace;
    unsafe {
        libc::unsetenv(CString::new("CDPATH").unwrap().as_ptr());
    }

    // Set runtime env vars for this .do invocation.
    let startdir = PathBuf::from(env::v().startdir);
    let real_dodir = dodir
        .canonicalize()
        .unwrap_or_else(|_| dodir.to_path_buf());
    let pwd_rel = state::relpath(&real_dodir, &startdir)
        .to_string_lossy()
        .to_string();
    std::env::set_var("REDO_PWD", pwd_rel);
    std::env::set_var("REDO_TARGET", arg1);
    std::env::set_var("REDO_DEPTH", format!("{}  ", env::v().depth));
    // Propagate cycle info so nested redo processes can detect cyclic deps
    // instead of deadlocking on locks held by ancestors.
    cycles::add(lock_fid);
    // In .do subprocesses, clamp REDO_{XTRACE,VERBOSE} from 1 -> 0 so nested
    // invocations don't automatically inherit these flags.
    if xtrace0 == 1 {
        std::env::set_var("REDO_XTRACE", "0");
    }
    if verbose0 == 1 {
        std::env::set_var("REDO_VERBOSE", "0");
    }

    if unsafe { libc::chdir(CString::new(dodir.to_string_lossy().as_ref()).unwrap().as_ptr()) } != 0 {
        unsafe { libc::_exit(99) };
    }
    unsafe {
        libc::dup2(stdout_fd, 1);
    }

    // Optional log capture: if REDO_LOG is enabled and stderr hasn't been
    // redirected away from the current log stream (tracked by REDO_LOG_INODE),
    // redirect this target's stderr to its per-target log file.
    if env::v().log != 0 {
        let cur_inode = unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstat(2, &mut st as *mut _) == 0 {
                st.st_ino as u64
            } else {
                0
            }
        };
        let env_inode_ok = env::v()
            .log_inode
            .parse::<u64>()
            .ok()
            .map(|want| want == cur_inode)
            .unwrap_or(true); // empty/unparseable -> treat as ok
        if env::v().log_inode.is_empty() || env_inode_ok {
            let logpath = state::logname(lock_fid);
            // Create the per-target log file via temp+rename so readers never see a
            // log file truncated mid-read.
            let logdir = logpath.parent().unwrap_or_else(|| Path::new("."));
            // mkstemp requires the template to end with XXXXXX.
            let tmpl = format!("{}/redo.XXXXXX", logdir.to_string_lossy().to_string());
            let mut template = CString::new(tmpl).unwrap().into_bytes_with_nul();
            let fd = unsafe { libc::mkstemp(template.as_mut_ptr() as *mut libc::c_char) };
            if fd >= 0 {
                let tmp_cstr = unsafe { std::ffi::CStr::from_ptr(template.as_ptr() as *const _) };
                let tmp_path = PathBuf::from(tmp_cstr.to_string_lossy().to_string());
                // Best-effort: try to rename into place before emitting any meta-lines
                // that would cause redo-log to open it.
                let _ = fs::rename(&tmp_path, &logpath);
                let logf = unsafe { fs::File::from_raw_fd(fd) };
                let new_inode = logf.metadata().map(|m| m.ino()).unwrap_or(0);
                std::env::set_var("REDO_LOG", "1");
                std::env::set_var("REDO_LOG_INODE", new_inode.to_string());
                unsafe {
                    libc::dup2(logf.as_raw_fd(), 2);
                }
                // Ensure stderr is inherited by subprocesses.
                let _ = crate::helpers::close_on_exec(2, false);
            }
        }
    } else {
        // Log disabled: clear inheritable vars.
        std::env::remove_var("REDO_LOG_INODE");
        std::env::set_var("REDO_LOG", "");
    }

    let mut argv: Vec<String> = if let Some(v) = argv_override {
        v
    } else {
        // argv default: /bin/sh -e[v][x] dofile arg1 arg2 arg3.
        // Using an absolute path avoids PATH shadowing and makes behavior more predictable.
        let mut shflag = "-e".to_string();
        if verbose0 > 0 {
            shflag.push('v');
        }
        if xtrace0 > 0 {
            shflag.push('x');
        }
        let mut argv: Vec<String> = vec![
            "/bin/sh".into(),
            shflag,
            dofile.into(),
            arg1.into(),
            arg2.into(),
            arg3.into(),
        ];

        // shebang override (eg. "#!/usr/bin/env perl")
        if let Ok(Some(line1)) = read_first_line(&dodir.join(dofile)) {
            if let Some(rest) = line1.strip_prefix("#!") {
                let rest = rest.trim();
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if !parts.is_empty() {
                    argv[0] = parts[0].to_string();
                    // Drop the sh -e[vx] flag when using an explicit interpreter line.
                    argv.remove(1);
                    for (i, p) in parts.iter().skip(1).enumerate() {
                        argv.insert(1 + i, p.to_string());
                    }
                }
            }
        }
        argv
    };

    if env::v().strict {
        if let Some(p) = trace_out0 {
            let mut wrapped: Vec<String> = vec![
                "redo-trace".into(),
                "--trace-out0".into(),
                p.to_string_lossy().to_string(),
                "--mode".into(),
                "read".into(),
                "--".into(),
            ];
            wrapped.extend(argv);
            argv = wrapped;
        }
    }

    let cstrs: Vec<CString> = argv
        .iter()
        .map(|s| CString::new(s.as_str()).unwrap())
        .collect();
    let mut ptrs: Vec<*const libc::c_char> = cstrs.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());

    if std::env::var("REDO_DEBUG_EXEC").ok().as_deref() == Some("1") {
        // Best-effort: write the argv we are about to exec to stderr (fd=2).
        // This is intentionally low-level so it works in the forked child.
        let msg = format!("REDO_DEBUG_EXEC argv={:?}\n", argv);
        unsafe {
            let _ = libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
        }
    }
    unsafe {
        libc::execvp(ptrs[0], ptrs.as_ptr());
        libc::_exit(99);
    }
}

fn find_do_for_target(sf: &state::File) -> anyhow::Result<Option<(PathBuf, String, String, String)>> {
    // Returns (dodir, dofile, basename, ext) where:
    // - basename may include subdir relative to dodir
    // - ext is the extension portion including leading '.', or "".
    let base = env::v().base;
    for cand in paths::possible_do_files(&sf.name, &base) {
        let dopath = cand.dodir.join(&cand.dofile);
        if dopath.exists() {
            // Track dependency on the .do file itself.
            sf.add_dep('m', dopath.to_string_lossy().as_ref())?;
            // Mark the .do file itself as a static source so redo-ifchange
            // won't treat it as perpetually dirty.
            let mut dof = state::File::by_name(dopath.to_string_lossy().as_ref(), true)?;
            dof.set_static()?;
            dof.save()?;
            return Ok(Some((
                cand.dodir,
                cand.dofile,
                cand.basename.clone(),
                cand.ext.clone(),
            )));
        } else {
            sf.add_dep('c', dopath.to_string_lossy().as_ref())?;
        }
    }
    Ok(None)
}

#[derive(Debug, Clone)]
struct PoolSpec {
    name: String,
    depth: usize,
}

// Pool slot lock fid mapping.
//
// We map `poolName -> base fid` using a stable hash and then use
// `base+slotIndex` for each slot.
//
// This intentionally lives in a fid range far away from normal file ids and
// log-lock fids, to avoid collisions.
const POOL_LOCK_MAGIC: i64 = 0x2000_0000;
const POOL_LOCK_STRIDE: i64 = 1 << 16; // supports depths up to 65536

fn fnv1a64(s: &str) -> u64 {
    // Stable non-crypto hash.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn pool_slot_fid(name: &str, slot: usize) -> anyhow::Result<i64> {
    if slot as i64 >= POOL_LOCK_STRIDE {
        anyhow::bail!(
            "pool depth too large: slot={} (max={})",
            slot,
            POOL_LOCK_STRIDE - 1
        );
    }
    // Use 32 bits of hash as a bucket. Collision probability is ~1/2^32.
    let bucket = (fnv1a64(name) & 0xffff_ffff) as i64;
    Ok(POOL_LOCK_MAGIC + bucket.saturating_mul(POOL_LOCK_STRIDE) + slot as i64)
}

fn parse_redo_pool_directive(dodir: &Path, dofile: &str) -> anyhow::Result<Option<PoolSpec>> {
    let p = dodir.join(dofile);
    let f = fs::File::open(&p)?;
    // Read a small prefix; directives must appear at the top of the file.
    let mut s = String::new();
    let mut limited = f.take(16 * 1024);
    limited.read_to_string(&mut s)?;

    for line in s.lines().take(64) {
        let l = line.trim_start();
        let Some(rest) = l.strip_prefix('#') else { continue; };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix("redo-pool:") else { continue; };
        let mut it = rest.split_whitespace();
        let name = it
            .next()
            .ok_or_else(|| anyhow::anyhow!("invalid redo pool directive in {:?}: missing name", p))?
            .to_string();
        let depth_s = it.next().ok_or_else(|| {
            anyhow::anyhow!("invalid redo pool directive in {:?}: missing depth", p)
        })?;
        let depth = depth_s.parse::<usize>().map_err(|e| {
            anyhow::anyhow!(
                "invalid redo pool directive in {:?}: bad depth {:?}: {}",
                p,
                depth_s,
                e
            )
        })?;
        if depth == 0 {
            anyhow::bail!("invalid redo pool directive in {:?}: depth must be > 0", p);
        }
        if depth as i64 > POOL_LOCK_STRIDE {
            anyhow::bail!(
                "invalid redo pool directive in {:?}: depth {} exceeds max {}",
                p,
                depth,
                POOL_LOCK_STRIDE
            );
        }
        return Ok(Some(PoolSpec { name, depth }));
    }
    Ok(None)
}

fn try_acquire_pool(spec: &PoolSpec) -> anyhow::Result<Option<state::Lock>> {
    for slot in 0..spec.depth {
        let fid = pool_slot_fid(&spec.name, slot)?;
        let mut l = match state::Lock::new(fid) {
            Ok(l) => l,
            Err(e) => {
                // If we already created a lock object for this fid in this process
                // (eg. another scheduled job is currently holding the pool slot),
                // treat it as unavailable.
                if e.to_string().starts_with("Lock already created for fid=") {
                    continue;
                }
                return Err(e);
            }
        };
        if l.trylock()? {
            return Ok(Some(l));
        }
        // Drop `l` (releasing local bookkeeping) and try next slot.
    }
    Ok(None)
}

fn find_do_for_target_no_deps(
    sf: &state::File,
) -> anyhow::Result<Option<(PathBuf, String, String, String)>> {
    // Like `find_do_for_target`, but does not record dependencies or mutate state.
    // This is used to read metadata directives (eg. job pools) without opening
    // sqlite write transactions while waiting.
    let base = env::v().base;
    for cand in paths::possible_do_files(&sf.name, &base) {
        let dopath = cand.dodir.join(&cand.dofile);
        if dopath.exists() {
            return Ok(Some((
                cand.dodir,
                cand.dofile,
                cand.basename.clone(),
                cand.ext.clone(),
            )));
        }
    }
    Ok(None)
}

struct PendingBuild {
    t: String,
    trp: String,
    dodir: PathBuf,
    abs_target: PathBuf,
    tmpname: PathBuf,
    outfile: fs::File,
    before_t: Option<std::fs::Metadata>,
    dofile: String,
    sf: state::File,
    cache_key: Option<action_cache::ActionKey>,
    policy_domain: Option<String>,
    trace_out0: Option<PathBuf>,
    deps_out0: Option<PathBuf>,
    remote_ok: Option<PathBuf>,
    remote_exec: bool,
    strict: bool,
    strict_fail: bool,
    pool_lock: Option<state::Lock>,
    lock: Option<state::Lock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScheduleBuildOneResult {
    Scheduled,
    DeferredPool,
}

fn schedule_build_one(
    t: &str,
    mut sf: state::File,
    mut lock: Option<state::Lock>,
    rc_flag: Arc<AtomicI32>,
) -> anyhow::Result<ScheduleBuildOneResult> {
    let abs_target = env::v().base.join(&sf.name);
    let strict = env::v().strict;
    let strict_fail = env::v().strict_fail;
    let remote_exec_enabled = {
        let v = std::env::var("REDO_REMOTE_EXEC").unwrap_or_default();
        let v = v.trim();
        !v.is_empty() && v != "0"
    };
    let remote_platform_id = std::env::var("REDO_REMOTE_PLATFORM_ID").unwrap_or_default();
    let remote_exec_wanted = remote_exec_enabled && !remote_platform_id.trim().is_empty();
    let policy_domain: Option<String> = if remote_exec_wanted {
        if strict {
            Some(format!(
                "{};remote_platform={}",
                STRICT_POLICY_READTRACE,
                remote_platform_id.trim()
            ))
        } else {
            Some(format!(
                "policy=remote-exec;platform={}",
                remote_platform_id.trim()
            ))
        }
    } else if strict {
        Some(STRICT_POLICY_READTRACE.to_string())
    } else {
        None
    };

    let trp = state::target_relpath(&abs_target).to_string_lossy().to_string();
    Log::meta("do", &trp, None);

    // Override detection: if the user modified a previously-generated file,
    // treat it as a source and refuse to rebuild it.
    let newstamp = sf.read_stamp()?;
    let have_prev_stamp = sf.stamp.is_some();
    if sf.is_generated
        && newstamp != state::STAMP_MISSING
        && (sf.is_override
            || (have_prev_stamp
                && state::detect_override(sf.stamp.as_deref().unwrap_or(""), &newstamp)))
    {
        state::warn_override(&sf.name);
        if !sf.is_override {
            sf.set_override()?;
            sf.save()?;
            state::commit()?;
        }
        if let Some(l) = lock.as_mut() {
            l.unlock()?;
        }
        Log::meta("done", &format!("0 {}", sf.name), None);
        return Ok(ScheduleBuildOneResult::Scheduled);
    }

    // If target exists and is not generated, treat as static and skip.
    if abs_target.exists() && !sf.is_generated {
        sf.set_static()?;
        sf.save()?;
        state::commit()?;
        if let Some(l) = lock.as_mut() {
            l.unlock()?;
        }
        Log::meta("done", &format!("0 {}", trp), None);
        return Ok(ScheduleBuildOneResult::Scheduled);
    }

    // Pool scheduling: acquire a slot *before* we mutate sqlite state.
    // This avoids holding write transactions open when we have to wait for pool
    // availability.
    let mut pool_lock: Option<state::Lock> = None;
    if let Some((dodir0, dofile0, _, _)) = find_do_for_target_no_deps(&sf)? {
        if let Some(spec) = parse_redo_pool_directive(&dodir0, &dofile0)? {
            match try_acquire_pool(&spec)? {
                Some(l) => {
                    pool_lock = Some(l);
                }
                None => {
                    if let Some(l) = lock.as_mut() {
                        l.unlock()?;
                    }
                    return Ok(ScheduleBuildOneResult::DeferredPool);
                }
            }
        }
    }

    // If we're about to build a missing file, persist the "this is a target" decision
    // before any output is created.
    persist_new_target_intent(&mut sf, &abs_target)?;

    sf.zap_deps1()?;

    let before_t = try_lstat(&abs_target)?;
    let Some((dodir, dofile, basename, ext)) = find_do_for_target(&sf)? else {
        if abs_target.exists() {
            sf.set_static()?;
            sf.save()?;
            state::commit()?;
            if let Some(l) = lock.as_mut() {
                l.unlock()?;
            }
            Log::meta("done", &format!("0 {}", trp), None);
            return Ok(ScheduleBuildOneResult::Scheduled);
        }
        Log::err(&format!("no rule to redo {:?}", t));
        sf.set_failed()?;
        sf.save()?;
        state::commit()?;
        if let Some(l) = lock.as_mut() {
            l.unlock()?;
        }
        Log::meta("done", &format!("1 {}", trp), None);
        rc_flag.store(1, Ordering::Relaxed);
        return Ok(ScheduleBuildOneResult::Scheduled);
    };

    // Args are expressed relative to the .do file directory.
    let arg1 = format!("{}{}", basename, ext); // target name (including extension)
    let arg2 = basename; // target name (without extension)

    // Temp name is in the .do directory.
    let tmpname = dodir.join(format!("{}.redo.tmp", arg1));
    unlink_best_effort(&tmpname);

    // Read trace output (NUL-separated list). This is used by strict-mode cache gating,
    // and is also required for remote-exec so we can reuse strict-mode evaluation.
    let trace_out0: Option<PathBuf> = if strict || remote_exec_wanted {
        let runid = env::v().runid.unwrap_or(0);
        let d = env::v().base.join(".redo").join("trace");
        let _ = fs::create_dir_all(&d);
        Some(d.join(format!("readtrace.{}.{}.{}.out0", runid, sf.id, std::process::id())))
    } else {
        None
    };
    if let Some(p) = &trace_out0 {
        let _ = fs::remove_file(p);
    }

    let (deps_out0, remote_ok): (Option<PathBuf>, Option<PathBuf>) = if remote_exec_wanted {
        let runid = env::v().runid.unwrap_or(0);
        let d = env::v().base.join(".redo").join("remote");
        let _ = fs::create_dir_all(&d);
        let deps = d.join(format!("deps.{}.{}.{}.out0", runid, sf.id, std::process::id()));
        let ok = d.join(format!(
            "ok.{}.{}.{}.flag",
            runid,
            sf.id,
            std::process::id()
        ));
        let _ = fs::remove_file(&deps);
        let _ = fs::remove_file(&ok);
        (Some(deps), Some(ok))
    } else {
        (None, None)
    };

    // Phase 1 local action cache (restricted): consult only under redo-ifchange.
    // Note: schedule_build_one is only used by redo-ifchange's scheduler.
    let mut cache_key: Option<action_cache::ActionKey> = None;
    if action_cache::enabled_default_on() {
        if sf.csum.is_some() {
            // If a rule is using redo-stamp checksums, we currently don't try to cache it.
            Log::cache_skip(&trp, "csum");
        } else {
            let mut deps_for_key = sf.deps()?;

            // Hybrid dep semantics: ensure missing mode=m generated deps exist.
            // (Missing non-generated deps are treated as an error.)
            for (mode, dep) in deps_for_key.iter() {
                if *mode != 'm' || dep.id == sf.id || dep.name == state::ALWAYS {
                    continue;
                }
                let dep_abs = env::v().base.join(&dep.name);
                if dep_abs.exists() {
                    continue;
                }
                if dep.is_generated {
                    // Preserve cycle detection: while holding `sf`'s lock, temporarily
                    // add it to the cycles set for any nested builds.
                    let saved_cycles = std::env::var("REDO_CYCLES").unwrap_or_default();
                    std::env::set_var("REDO_CYCLES", saved_cycles.clone());
                    cycles::add(sf.id);
                    let rv = build_one(dep_abs.to_string_lossy().as_ref())?;
                    std::env::set_var("REDO_CYCLES", saved_cycles);
                    if rv != 0 {
                        Log::err(&format!(
                            "{}: failed to build missing dependency {}",
                            trp, dep.name
                        ));
                        sf.set_failed()?;
                        sf.save()?;
                        state::commit()?;
                        if let Some(l) = lock.as_mut() {
                            l.unlock()?;
                        }
                        Log::meta("done", &format!("1 {}", trp), None);
                        rc_flag.store(1, Ordering::Relaxed);
                        return Ok(ScheduleBuildOneResult::Scheduled);
                    }
                } else {
                    Log::err(&format!(
                        "{}: missing required dependency {}",
                        trp, dep.name
                    ));
                    sf.set_failed()?;
                    sf.save()?;
                    state::commit()?;
                    if let Some(l) = lock.as_mut() {
                        l.unlock()?;
                    }
                    Log::meta("done", &format!("1 {}", trp), None);
                    rc_flag.store(1, Ordering::Relaxed);
                    return Ok(ScheduleBuildOneResult::Scheduled);
                }
            }

            let (elig, key) = if let Some(p) = policy_domain.as_deref() {
                action_cache::compute_action_key_v0_policy(p, &sf.name, deps_for_key.as_mut_slice())?
            } else {
                action_cache::compute_action_key_v0(&sf.name, deps_for_key.as_mut_slice())?
            };
            match elig {
                action_cache::Eligibility::SkipAlways => {
                    Log::cache_skip(&trp, "always");
                }
                action_cache::Eligibility::Eligible => {
                    if let Some((blob, meta)) = action_cache::lookup(&key) {
                        // Cache hit: materialize to $3 then rename into place.
                        Log::cache_hit(&trp, key.prefix(), meta.size);
                        match action_cache::materialize(&blob, &meta, &tmpname) {
                            Ok(_bytes) => match durable_rename(&tmpname, &abs_target) {
                                Ok(()) => {
                                    // Update state similarly to a successful build.
                                    sf.refresh()?;
                                    sf.is_generated = true;
                                    sf.is_override = false;
                                    sf.failed_runid = None;
                                    sf.csum = None;
                                    sf.update_stamp(false)?;

                                    // We called zap_deps1() earlier; re-add deps to clear delete_me,
                                    // then delete any truly-stale entries.
                                    for (mode, dep) in deps_for_key.iter() {
                                        let dep_arg = if dep.name == state::ALWAYS {
                                            state::ALWAYS.to_string()
                                        } else {
                                            env::v()
                                                .base
                                                .join(&dep.name)
                                                .to_string_lossy()
                                                .to_string()
                                        };
                                        sf.add_dep(*mode, &dep_arg)?;
                                    }
                                    sf.zap_deps2()?;
                                    sf.save()?;
                                    state::commit()?;

                                    if let Some(l) = lock.as_mut() {
                                        l.unlock()?;
                                    }
                                    Log::meta("done", &format!("0 {}", trp), None);
                                    return Ok(ScheduleBuildOneResult::Scheduled);
                                }
                                Err(e) => {
                                    Log::cache_skip(&trp, &format!("rename {}", e));
                                    unlink_best_effort(&tmpname);
                                    cache_key = Some(key);
                                }
                            },
                            Err(e) => {
                                Log::cache_skip(&trp, &format!("materialize {}", e));
                                unlink_best_effort(&tmpname);
                                cache_key = Some(key);
                            }
                        }
                    } else {
                        Log::cache_miss(&trp, "no_entry");
                        cache_key = Some(key.clone());

                        // Phase 2 remote artifact cache (best-effort): consult remote
                        // CAS/index on local miss. Keep all failures as a fallback
                        // to local execution.
                        match remote_cache::config_from_env() {
                            Ok(Some(cfg)) => {
                                // Avoid holding sqlite write transactions while waiting on network.
                                let _ = state::commit();
                                match if let Some(p) = policy_domain.as_deref() {
                                    action_cache::compute_action_key_v1_remote_policy(
                                        p,
                                        &sf.name,
                                        deps_for_key.as_mut_slice(),
                                    )
                                } else {
                                    action_cache::compute_action_key_v1_remote(
                                        &sf.name,
                                        deps_for_key.as_mut_slice(),
                                    )
                                } {
                                    Ok((action_cache::Eligibility::Eligible, rkey)) => {
                                        match remote_cache::get_action_manifest_sha256(
                                            &cfg,
                                            &rkey.hex,
                                        ) {
                                            Ok(Some(man_hex)) => {
                                                // Download+verify manifest blob.
                                                match remote_cache::get_blob_bytes_verified(
                                                    &cfg,
                                                    &man_hex,
                                                ) {
                                                    Ok(man_bytes) => {
                                                        let man_s = String::from_utf8_lossy(&man_bytes).to_string();
                                                        match remote_cache::parse_artifact_manifest_v1(&man_s) {
                                                            Ok(m) => {
                                                                // Download+verify output blob to tmpname, then rename into place.
                                                                Log::cache_hit(&trp, rkey.prefix(), m.size);
                                                                match remote_cache::download_blob_to_file_verified(
                                                                    &cfg,
                                                                    &m.blob_sha256,
                                                                    &tmpname,
                                                                ) {
                                                                    Ok(_bytes) => {
                                                                        let perm = fs::Permissions::from_mode(m.mode & 0o777);
                                                                        let _ = fs::set_permissions(&tmpname, perm);
                                                                        match durable_rename(&tmpname, &abs_target) {
                                                                            Ok(()) => {
                                                                                // Update state similarly to a successful build.
                                                                                sf.refresh()?;
                                                                                sf.is_generated = true;
                                                                                sf.is_override = false;
                                                                                sf.failed_runid = None;
                                                                                sf.csum = None;
                                                                                sf.update_stamp(false)?;

                                                                                // We called zap_deps1() earlier; re-add deps to clear delete_me,
                                                                                // then delete any truly-stale entries.
                                                                                for (mode, dep) in deps_for_key.iter() {
                                                                                    let dep_arg = if dep.name == state::ALWAYS {
                                                                                        state::ALWAYS.to_string()
                                                                                    } else {
                                                                                        env::v()
                                                                                            .base
                                                                                            .join(&dep.name)
                                                                                            .to_string_lossy()
                                                                                            .to_string()
                                                                                    };
                                                                                    sf.add_dep(*mode, &dep_arg)?;
                                                                                }
                                                                                sf.zap_deps2()?;
                                                                                sf.save()?;
                                                                                state::commit()?;

                                                                                if let Some(l) = lock.as_mut() {
                                                                                    l.unlock()?;
                                                                                }
                                                                                Log::meta("done", &format!("0 {}", trp), None);

                                                                                // Best-effort: populate local cache so future runs don't need the network.
                                                                                let _ = action_cache::store(&key, &abs_target, m.mode & 0o777);
                                                                                return Ok(ScheduleBuildOneResult::Scheduled);
                                                                            }
                                                                            Err(e) => {
                                                                                Log::cache_skip(&trp, &format!("rename {}", e));
                                                                                unlink_best_effort(&tmpname);
                                                                            }
                                                                        }
                                                                    }
                                                                    Err(e) => {
                                                                        Log::cache_skip(&trp, &format!("remote_blob {}", e));
                                                                        unlink_best_effort(&tmpname);
                                                                    }
                                                                }
                                                            }
                                                            Err(e) => {
                                                                Log::cache_skip(&trp, &format!("remote_manifest {}", e));
                                                            }
                                                        }
                                                    }
                                                    Err(e) => {
                                                        Log::cache_skip(&trp, &format!("remote_manifest_blob {}", e));
                                                    }
                                                }
                                            }
                                            Ok(None) => {
                                                Log::cache_miss(&trp, "remote_no_entry");
                                            }
                                            Err(e) => {
                                                Log::cache_skip(&trp, &format!("remote_lookup {}", e));
                                            }
                                        }
                                    }
                                    Ok((action_cache::Eligibility::SkipAlways, _)) => {
                                        Log::cache_skip(&trp, "always");
                                    }
                                    Err(e) => {
                                        Log::cache_skip(&trp, &format!("remote_key {}", e));
                                    }
                                }
                            }
                            Ok(None) => {}
                            Err(e) => {
                                Log::cache_skip(&trp, &format!("remote_cfg {}", e));
                            }
                        }
                    }
                }
            }
        }
    }

    // If we didn't take a cache hit, we're about to start a jobserver job. If we
    // synchronously built any missing deps above, we may have released our token;
    // reacquire one before calling jobserver::start().
    //
    // $3 is expressed relative to dodir.
    let rel_arg3 = pathdiff::diff_paths(&tmpname, &dodir).unwrap_or(tmpname.clone());
    let mut argv_override: Option<Vec<String>> = None;
    let mut trace_out0_for_child: Option<PathBuf> = trace_out0.clone();
    let mut remote_exec_job = false;
    let mut deps_out0_use: Option<PathBuf> = None;
    let mut remote_ok_use: Option<PathBuf> = None;
    if remote_exec_wanted && cache_key.is_some() {
        // Only attempt remote exec if the remote cache URL is configured.
        match remote_cache::config_from_env() {
            Ok(Some(_cfg)) => {
                // Collect mode=m deps (workspace-relative) to upload as inputs.
                let mut inputs: Vec<String> = Vec::new();
                let mut bad = false;
                if let Ok(deps_now) = sf.deps() {
                    for (mode, dep) in deps_now.iter() {
                        if *mode == 'm' {
                            if dep.id == sf.id || dep.name == state::ALWAYS {
                                continue;
                            }
                            let abs = env::v().base.join(&dep.name);
                            match fs::symlink_metadata(&abs) {
                                Ok(st) if st.is_file() => inputs.push(dep.name.clone()),
                                Ok(_) | Err(_) => {
                                    bad = true;
                                    break;
                                }
                            }
                        } else if *mode == 'c' {
                            // For correctness parity with local: ensure the negative dep
                            // is still missing in the client workspace.
                            let abs = env::v().base.join(&dep.name);
                            if abs.exists() {
                                bad = true;
                                break;
                            }
                        }
                    }
                } else {
                    bad = true;
                }

                if !bad {
                    let trace_p = trace_out0.as_ref();
                    let deps_p = deps_out0.as_ref();
                    let ok_p = remote_ok.as_ref();
                    if trace_p.is_none() || deps_p.is_none() || ok_p.is_none() {
                        bad = true;
                    }
                    if bad {
                        // Fall back to local execution.
                        // (Do not return here; continue below.)
                    } else {
                        let trace_p = trace_p.unwrap();
                        let deps_p = deps_p.unwrap();
                        let ok_p = ok_p.unwrap();

                    let base = env::v().base;
                    let cwd_rel = pathdiff::diff_paths(&dodir, &base)
                        .unwrap_or(dodir.clone())
                        .to_string_lossy()
                        .to_string();
                    let tmp_rel = pathdiff::diff_paths(&tmpname, &base)
                        .unwrap_or(tmpname.clone())
                        .to_string_lossy()
                        .to_string();

                    // Action argv as it would run locally (without local strict tracing).
                    let verbose0 = env::v().verbose;
                    let xtrace0 = env::v().xtrace;
                    let mut shflag = "-e".to_string();
                    if verbose0 > 0 {
                        shflag.push('v');
                    }
                    if xtrace0 > 0 {
                        shflag.push('x');
                    }
                    let rel_arg3_s = rel_arg3.to_string_lossy().to_string();
                    let mut action_argv: Vec<String> = vec![
                        "/bin/sh".into(),
                        shflag,
                        dofile.clone(),
                        arg1.clone(),
                        arg2.clone(),
                        rel_arg3_s,
                    ];
                    if let Ok(Some(line1)) = read_first_line(&dodir.join(&dofile)) {
                        if let Some(rest) = line1.strip_prefix("#!") {
                            let rest = rest.trim();
                            let parts: Vec<&str> = rest.split_whitespace().collect();
                            if !parts.is_empty() {
                                action_argv[0] = parts[0].to_string();
                                action_argv.remove(1);
                                for (i, p) in parts.iter().skip(1).enumerate() {
                                    action_argv.insert(1 + i, p.to_string());
                                }
                            }
                        }
                    }

                    let mut wargv: Vec<String> = Vec::new();
                    wargv.push("redo-remote-exec".to_string());
                    wargv.push("--out".to_string());
                    wargv.push(tmpname.to_string_lossy().to_string());
                    wargv.push("--deps-out0".to_string());
                    wargv.push(deps_p.to_string_lossy().to_string());
                    wargv.push("--trace-out0".to_string());
                    wargv.push(trace_p.to_string_lossy().to_string());
                    wargv.push("--remote-ok".to_string());
                    wargv.push(ok_p.to_string_lossy().to_string());
                    wargv.push("--cwd-rel".to_string());
                    wargv.push(cwd_rel);
                    wargv.push("--target-rel".to_string());
                    wargv.push(sf.name.clone());
                    wargv.push("--tmp-rel".to_string());
                    wargv.push(tmp_rel);
                    for inp in inputs {
                        wargv.push("--input".to_string());
                        wargv.push(inp);
                    }
                    wargv.push("--".to_string());
                    wargv.extend(action_argv);

                    remote_exec_job = true;
                    deps_out0_use = Some(deps_p.clone());
                    remote_ok_use = Some(ok_p.clone());
                    argv_override = Some(wargv);
                    // Do not wrap the local wrapper with redo-trace; remote tracing is
                    // performed by the server, and local fallback (if needed) is done by
                    // the wrapper itself.
                    trace_out0_for_child = None;
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                Log::warn(&format!("remote_exec: remote_cfg error: {:?}", e));
            }
        }
    }

    jobserver::ensure_token_or_cheat(t, || 0)?;

    let outfile = mkstemp_unlinked()?;
    let outfd = outfile.as_raw_fd();

    // Run script; it will typically write to stdout or to $3.

    let pb = PendingBuild {
        t: t.to_string(),
        trp: trp.clone(),
        dodir: dodir.clone(),
        abs_target: abs_target.clone(),
        tmpname: tmpname.clone(),
        outfile,
        before_t,
        dofile: dofile.clone(),
        sf: sf.clone(),
        cache_key,
        policy_domain: policy_domain.clone(),
        trace_out0: trace_out0.clone(),
        deps_out0: deps_out0_use.clone(),
        remote_ok: remote_ok_use.clone(),
        remote_exec: remote_exec_job,
        strict,
        strict_fail,
        pool_lock,
        lock,
    };
    let pb = Arc::new(Mutex::new(pb));

    // Must be flushed (no open sqlite transaction) before we fork/exec.
    state::commit()?;

    let dodir2 = dodir.clone();
    let dofile2 = dofile.clone();
    let arg1_2 = arg1.clone();
    let arg2_2 = arg2.clone();
    let arg3_2 = rel_arg3.to_string_lossy().to_string();
    let lock_fid = sf.id;
    let argv_override_2 = argv_override.clone();
    let trace_out0_2 = trace_out0_for_child.clone();
    let pb2 = pb.clone();
    let rc2 = rc_flag.clone();

    jobserver::start(
        t,
        move || {
            exec_dofile_in_child(
                &dodir2,
                &dofile2,
                &arg1_2,
                &arg2_2,
                &arg3_2,
                outfd,
                lock_fid,
                argv_override_2,
                trace_out0_2.as_deref(),
            )
        },
        move |_name, rv| {
            let mut pb = pb2.lock().unwrap();

            // Job is no longer running; release any pool slot promptly.
            pb.pool_lock = None;

            // Parent: examine outputs.
            let after_t = try_lstat(&pb.abs_target).unwrap_or(None);
            let st_out = pb.outfile.metadata().ok();
            let st_tmp = try_lstat(&pb.tmpname).unwrap_or(None);

            let mut final_rv = rv;
            if rv == 0 {
                if let Some(a) = &after_t {
                    let changed = pb
                        .before_t
                        .as_ref()
                        .map(|b| b.mtime() != a.mtime())
                        .unwrap_or(true);
                    if changed && !a.is_dir() {
                        Log::err(&format!("{} modified {} directly!", pb.dofile, pb.t));
                        Log::err("...you should update $3 (a temp file) or stdout, not $1.");
                        final_rv = 206;
                    }
                }
                if st_tmp.is_some()
                    && st_out
                        .as_ref()
                        .map(|m| m.size() > 0)
                        .unwrap_or(false)
                {
                    Log::err(&format!("{} wrote to stdout *and* created $3.", pb.dofile));
                    Log::err("...you should write status messages to stderr, not stdout.");
                    final_rv = 207;
                }
            }

            if final_rv == 0 {
                let out_size = st_out.as_ref().map(|m| m.size()).unwrap_or(0);
                if out_size > 0 && st_tmp.is_none() {
                    // Copy stdout capture to tmpname.
                    match fs::File::create(&pb.tmpname) {
                        Ok(mut newf) => {
                            let mut buf = vec![];
                            let _ = pb.outfile.seek(SeekFrom::Start(0));
                            let _ = pb.outfile.read_to_end(&mut buf);
                            let _ = newf.write_all(&buf);
                            let _ = newf.sync_all();
                        }
                        Err(e) => {
                            let dnt = pb.abs_target.parent().unwrap_or(Path::new("."));
                            if !dnt.exists() {
                                Log::err(&format!(
                                    "{}: target dir {:?} does not exist!",
                                    pb.t, dnt
                                ));
                            } else {
                                Log::err(&format!("{}: copy stdout: {}", pb.t, e));
                            }
                            final_rv = 209;
                        }
                    }
                }
                if final_rv == 0 {
                    if pb.tmpname.exists() {
                        if let Err(e) = durable_rename(&pb.tmpname, &pb.abs_target) {
                            Log::err(&format!("{}: rename {:?}: {}", pb.t, pb.tmpname, e));
                            final_rv = 209;
                        }
                    } else {
                        unlink_best_effort(&pb.abs_target);
                    }
                }

                if final_rv == 0 {
                    if should_test_crash_after_rename() {
                        // Deterministic crash window for tests: target exists, but state hasn't been recorded.
                        unsafe { libc::_exit(217) };
                    }
                    if let Err(e) = pb.sf.refresh() {
                        Log::err(&format!("{}: refresh: {:?}", pb.t, e));
                        final_rv = 209;
                    } else {
                        pb.sf.is_generated = true;
                        pb.sf.is_override = false;
                        if pb.sf.is_checked() || pb.sf.is_changed() {
                            pb.sf.stamp = pb.sf.read_stamp().ok();
                        } else {
                            pb.sf.csum = None;
                            let _ = pb.sf.update_stamp(false);
                            pb.sf.set_changed();
                        }

                        // If this job ran remotely, declared deps were recorded into
                        // `deps.out0` instead of the local sqlite DB. Apply them now
                        // (before zap_deps2 / strict evaluation / caching).
                        let remote_ran = pb
                            .remote_ok
                            .as_ref()
                            .map(|p| p.exists())
                            .unwrap_or(false);
                        if remote_ran {
                            match pb.deps_out0.as_ref() {
                                Some(p) => match read_deps_out0(p) {
                                    Ok(deps_pairs) => {
                                        for (mode, dep_s) in deps_pairs {
                                            if dep_s.is_empty() {
                                                continue;
                                            }
                                            let dep_arg = if dep_s == state::ALWAYS {
                                                state::ALWAYS.to_string()
                                            } else if Path::new(&dep_s).is_absolute() {
                                                dep_s
                                            } else {
                                                pb.dodir
                                                    .join(dep_s)
                                                    .to_string_lossy()
                                                    .to_string()
                                            };
                                            if let Err(e) = pb.sf.add_dep(mode, &dep_arg) {
                                                Log::err(&format!(
                                                    "{}: remote deps apply failed: {:?}",
                                                    pb.t, e
                                                ));
                                                final_rv = 209;
                                                break;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        Log::err(&format!("{}: remote deps read failed: {:?}", pb.t, e));
                                        final_rv = 209;
                                    }
                                },
                                None => {
                                    Log::err(&format!("{}: remote deps missing deps_out0 path", pb.t));
                                    final_rv = 209;
                                }
                            }
                        }
                        if let Some(p) = pb.deps_out0.as_ref() {
                            let _ = fs::remove_file(p);
                        }
                        if let Some(p) = pb.remote_ok.as_ref() {
                            let _ = fs::remove_file(p);
                        }

                        let _ = pb.sf.zap_deps2();
                        let mut strict_cache_ok = true;
                        if pb.strict {
                            let base = env::v().base;
                            let trace_entries: Vec<String> = match pb.trace_out0.as_ref() {
                                Some(p) => read_trace_out0(p).unwrap_or_else(|_| {
                                    vec!["TRACE_UNAVAILABLE:read_failed".to_string()]
                                }),
                                None => vec!["TRACE_UNAVAILABLE:missing_path".to_string()],
                            };
                            if let Some(p) = pb.trace_out0.as_ref() {
                                let _ = fs::remove_file(p);
                            }

                            let declared = pb.sf.deps().unwrap_or_default();
                            let res = evaluate_strict_readtrace(&base, &declared, &trace_entries);
                            if !res.trace_ok {
                                strict_cache_ok = false;
                                let why = if res.trace_reason.is_empty() {
                                    "unknown".to_string()
                                } else {
                                    res.trace_reason.clone()
                                };
                                let msg = format!("strict: read trace unavailable ({})", why);
                                if pb.strict_fail {
                                    Log::err(&msg);
                                    final_rv = 218;
                                } else {
                                    Log::warn(&msg);
                                }
                            } else if !res.violations.is_empty() {
                                strict_cache_ok = false;
                                let msg = format!(
                                    "strict: undeclared reads: {}",
                                    res.violations.join(", ")
                                );
                                if pb.strict_fail {
                                    Log::err(&msg);
                                    final_rv = 218;
                                } else {
                                    Log::warn(&msg);
                                }
                            }
                        }
                        if !pb.strict {
                            // Best-effort cleanup: remote-exec can produce a trace file even when strict
                            // mode is disabled (for compatibility / future reuse).
                            if let Some(p) = pb.trace_out0.as_ref() {
                                let _ = fs::remove_file(p);
                            }
                        }

                        if final_rv == 0 {
                            let _ = pb.sf.save();
                            let commit_ok = state::commit().is_ok();
                            if commit_ok {
                                if pb.cache_key.is_some() {
                                    if pb.strict && !strict_cache_ok {
                                        Log::cache_skip(&pb.trp, "strict");
                                    } else {
                                        match fs::symlink_metadata(&pb.abs_target) {
                                            Ok(st) if st.is_file() => {
                                                let mode = st.mode();
                                                if pb.sf.csum.is_some() {
                                                    Log::cache_skip(&pb.trp, "csum");
                                                } else if let Ok(mut deps_for_key) = pb.sf.deps() {
                                                    let key_res = if let Some(p) = pb.policy_domain.as_deref() {
                                                        action_cache::compute_action_key_v0_policy(
                                                            p,
                                                            &pb.sf.name,
                                                            deps_for_key.as_mut_slice(),
                                                        )
                                                    } else {
                                                        action_cache::compute_action_key_v0(
                                                            &pb.sf.name,
                                                            deps_for_key.as_mut_slice(),
                                                        )
                                                    };
                                                    match key_res {
                                                        Ok((action_cache::Eligibility::SkipAlways, _)) => {
                                                            Log::cache_skip(&pb.trp, "always");
                                                        }
                                                        Ok((action_cache::Eligibility::Eligible, k)) => {
                                                            match action_cache::store(&k, &pb.abs_target, mode) {
                                                                Ok(bytes) => {
                                                                    Log::cache_store(&pb.trp, k.prefix(), bytes);

                                                                    // Phase 2 remote artifact cache push (best-effort).
                                                                    if let Ok(Some(cfg)) =
                                                                        remote_cache::config_from_env()
                                                                    {
                                                                        if cfg.push_enabled {
                                                                            if let Some((blob, meta)) =
                                                                                action_cache::lookup(&k)
                                                                            {
                                                                                if !meta.sha256.is_empty() {
                                                                                    let manifest_json = format!(
                                                                                        "{{\"schema\":\"redo-artifact-manifest:v1\",\"kind\":\"file\",\"digest\":\"sha256:{}\",\"size\":{},\"mode\":{}}}",
                                                                                        meta.sha256,
                                                                                        meta.size,
                                                                                        meta.mode & 0o777
                                                                                    );
                                                                                    let mut hh = sha2::Sha256::new();
                                                                                    hh.update(manifest_json.as_bytes());
                                                                                    let man_hex = format!(
                                                                                        "{:x}",
                                                                                        hh.finalize()
                                                                                    );

                                                                                    let rkey_res = if let Some(p) = pb.policy_domain.as_deref() {
                                                                                        action_cache::compute_action_key_v1_remote_policy(
                                                                                            p,
                                                                                            &pb.sf.name,
                                                                                            deps_for_key.as_mut_slice(),
                                                                                        )
                                                                                    } else {
                                                                                        action_cache::compute_action_key_v1_remote(
                                                                                            &pb.sf.name,
                                                                                            deps_for_key.as_mut_slice(),
                                                                                        )
                                                                                    };
                                                                                    if let Ok((action_cache::Eligibility::Eligible, rkey)) = rkey_res {
                                                                                        let _ = remote_cache::put_blob_from_file(
                                                                                            &cfg,
                                                                                            &meta.sha256,
                                                                                            &blob,
                                                                                        );
                                                                                        let _ = remote_cache::put_blob_bytes(
                                                                                            &cfg,
                                                                                            &man_hex,
                                                                                            manifest_json.as_bytes(),
                                                                                        );
                                                                                        let _ = remote_cache::put_action_mapping(
                                                                                            &cfg,
                                                                                            &rkey.hex,
                                                                                            &man_hex,
                                                                                        );
                                                                                    }
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    Log::cache_skip(
                                                                        &pb.trp,
                                                                        &format!("store {}", e),
                                                                    );
                                                                }
                                                            }
                                                        }
                                                        Err(e) => {
                                                            Log::cache_skip(&pb.trp, &format!("key {}", e));
                                                        }
                                                    }
                                                } else {
                                                    Log::cache_skip(&pb.trp, "deps_failed");
                                                }
                                            }
                                            Ok(_) => {
                                                Log::cache_skip(&pb.trp, "nonfile");
                                            }
                                            Err(e) => {
                                                Log::cache_skip(&pb.trp, &format!("stat {}", e));
                                            }
                                        }
                                    }
                                }
                            } else if pb.cache_key.is_some() {
                                Log::cache_skip(&pb.trp, "commit_failed");
                            }
                        }
                    }
                }
            }

            if final_rv != 0 {
                unlink_best_effort(&pb.tmpname);
                let _ = pb.sf.set_failed();
                let _ = pb.sf.zap_deps2();
                let _ = pb.sf.save();
                let _ = state::commit();
                rc2.store(1, Ordering::Relaxed);
            }

            if let Some(l) = pb.lock.as_mut() {
                let _ = l.unlock();
            }
            Log::meta("done", &format!("{} {}", final_rv, pb.trp), None);
        },
    )?;

    Ok(ScheduleBuildOneResult::Scheduled)
}

#[derive(Debug, Clone)]
struct LockedTarget {
    fid: i64,
    t_arg: String,
    name: String,
}

fn start_redo_unlocked_job(
    reason: &str,
    args: Vec<String>,
    lock: state::Lock,
    rc_flag: Arc<AtomicI32>,
) -> anyhow::Result<()> {
    // Flush any sqlite writes before we fork/exec.
    state::commit()?;

    let lock = Arc::new(Mutex::new(lock));
    let lock2 = lock.clone();
    let rc2 = rc_flag.clone();
    let reason2 = reason.to_string();

    jobserver::start(
        &reason2,
        move || {
            // Increase nesting depth for child processes.
            std::env::set_var("REDO_DEPTH", format!("{}  ", env::v().depth));
            unsafe {
                // Ensure SIGPIPE default in child.
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            }
            let mut argv: Vec<String> = Vec::new();
            argv.push("redo-unlocked".to_string());
            argv.extend(args);
            let cstrs: Vec<CString> = argv
                .iter()
                .map(|s| CString::new(s.as_str()).unwrap())
                .collect();
            let mut ptrs: Vec<*const libc::c_char> = cstrs.iter().map(|c| c.as_ptr()).collect();
            ptrs.push(std::ptr::null());
            unsafe {
                libc::execvp(ptrs[0], ptrs.as_ptr());
                libc::_exit(99);
            }
        },
        move |_name, rv| {
            if rv != 0 {
                rc2.store(1, Ordering::Relaxed);
            }
            if let Ok(mut l) = lock2.lock() {
                let _ = l.unlock();
            }
        },
    )
}

fn build_one(t: &str) -> anyhow::Result<i32> {
    let mut sf = state::File::by_name(t, true)?;
    let abs_target = env::v().base.join(&sf.name);
    let strict = env::v().strict;
    let strict_fail = env::v().strict_fail;
    // In "unlocked" mode (used by redo-unlocked), our caller already holds the
    // target lock, so trying to reacquire it would deadlock.
    let mut lock: Option<state::Lock> = if env::v().unlocked {
        None
    } else {
        let mut l = state::Lock::new(sf.id)?;
        if let Err(e) = l.waitlock(false) {
            if e.downcast_ref::<cycles::CyclicDependencyError>().is_some() {
                Log::err(&format!("cyclic dependency while checking {}", sf.name));
                return Ok(208);
            }
            return Err(e);
        }
        Some(l)
    };
    // Once we own the lock, refresh state from the DB.
    // This prevents false override detection when we had to wait for another
    // process to finish building the same target.
    if lock.is_some() {
        sf.refresh()?;
    }
    let trp = state::target_relpath(&abs_target).to_string_lossy().to_string();
    Log::meta("do", &trp, None);

    // Override detection: if the user modified a previously-generated file,
    // treat it as a source and refuse to rebuild it.
    let newstamp = sf.read_stamp()?;
    let have_prev_stamp = sf.stamp.is_some();
    if sf.is_generated
        && newstamp != state::STAMP_MISSING
        && (sf.is_override
            || (have_prev_stamp
                && state::detect_override(sf.stamp.as_deref().unwrap_or(""), &newstamp)))
    {
        state::warn_override(&sf.name);
        if !sf.is_override {
            sf.set_override()?;
            sf.save()?;
            state::commit()?;
        }
        if let Some(l) = lock.as_mut() {
            l.unlock()?;
        }
        Log::meta("done", &format!("0 {}", sf.name), None);
        return Ok(0);
    }

    // If target exists and is not generated, treat as static and skip.
    if abs_target.exists() && !sf.is_generated {
        sf.set_static()?;
        sf.save()?;
        state::commit()?;
        if let Some(l) = lock.as_mut() {
            l.unlock()?;
        }
        Log::meta("done", &format!("0 {}", trp), None);
        return Ok(0);
    }

    // If we're about to build a missing file, persist the "this is a target" decision
    // before any output is created.
    persist_new_target_intent(&mut sf, &abs_target)?;

    sf.zap_deps1()?;

    let before_t = try_lstat(&abs_target)?;
    let Some((dodir, dofile, basename, ext)) = find_do_for_target(&sf)? else {
        if abs_target.exists() {
            sf.set_static()?;
            sf.save()?;
            state::commit()?;
            if let Some(l) = lock.as_mut() {
                l.unlock()?;
            }
            return Ok(0);
        }
        Log::err(&format!("no rule to redo {:?}", t));
        sf.set_failed()?;
        sf.save()?;
        state::commit()?;
        if let Some(l) = lock.as_mut() {
            l.unlock()?;
        }
        return Ok(1);
    };

    // Pool scheduling (blocking path): wait for a slot *without* holding a jobserver
    // token (we only need a token when we actually start the .do script).
    //
    // Note: this can still be invoked inside parallel builds (eg. `redo -jN a b`),
    // so we release our token while waiting to avoid starving unrelated work.
    let _pool_lock = match parse_redo_pool_directive(&dodir, &dofile)? {
        Some(spec) => {
            // Flush any sqlite writes before we might block.
            state::commit()?;
            let mut backoff_ms: u64 = 10;
            let mut released = false;
            loop {
                if let Some(l) = try_acquire_pool(&spec)? {
                    break Some(l);
                }
                if !released {
                    let _ = jobserver::release_mine();
                    released = true;
                }
                Log::meta("waiting", &trp, None);
                std::thread::sleep(std::time::Duration::from_millis(std::cmp::min(
                    backoff_ms, 1000,
                )));
                backoff_ms = std::cmp::min(backoff_ms * 2, 1000);
            }
        }
        None => None,
    };

    // Args are expressed relative to the .do file directory.
    let arg1 = format!("{}{}", basename, ext); // target name (including extension)
    let arg2 = basename; // target name (without extension)

    // Temp name is in the .do directory.
    let tmpname = dodir.join(format!("{}.redo.tmp", arg1));
    unlink_best_effort(&tmpname);

    let trace_out0: Option<PathBuf> = if strict {
        let runid = env::v().runid.unwrap_or(0);
        let d = env::v().base.join(".redo").join("trace");
        let _ = fs::create_dir_all(&d);
        Some(d.join(format!("readtrace.{}.{}.{}.out0", runid, sf.id, std::process::id())))
    } else {
        None
    };
    if let Some(p) = &trace_out0 {
        let _ = fs::remove_file(p);
    }

    let mut outfile = mkstemp_unlinked()?;
    let outfd = outfile.as_raw_fd();

    // Run script; it will typically write to stdout or to $3.
    // $3 is expressed relative to dodir.
    let rel_arg3 = pathdiff::diff_paths(&tmpname, &dodir).unwrap_or(tmpname.clone());
    // Consume a jobserver token, then run the .do script as a jobserver job.
    jobserver::ensure_token_or_cheat(t, || 0)?;
    let rv_cell = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(201));
    {
        let dodir = dodir.clone();
        let dofile = dofile.clone();
        let arg1 = arg1.clone();
        let arg2 = arg2.clone();
        let arg3 = rel_arg3.to_string_lossy().to_string();
        let rv_cell2 = rv_cell.clone();
        let lock_fid = sf.id;
        let trace_out0_2 = trace_out0.clone();
        // Must be flushed (no open sqlite transaction) before we fork/exec.
        state::commit()?;
        jobserver::start(
            t,
            move || {
                exec_dofile_in_child(
                    &dodir,
                    &dofile,
                    &arg1,
                    &arg2,
                    &arg3,
                    outfd,
                    lock_fid,
                    None,
                    trace_out0_2.as_deref(),
                )
            },
            move |_name, rv| {
                rv_cell2.store(rv, std::sync::atomic::Ordering::Relaxed);
            },
        )?;
    }
    jobserver::wait_all()?;
    let rv = rv_cell.load(std::sync::atomic::Ordering::Relaxed);

    // Parent: examine outputs.
    let after_t = try_lstat(&abs_target)?;
    let st_out = outfile.metadata()?;
    let st_tmp = try_lstat(&tmpname)?;

    let mut final_rv = rv;
    if rv == 0 {
        // detect writes to $1 directly
        if let Some(a) = &after_t {
            let changed = before_t
                .as_ref()
                .map(|b| b.mtime() != a.mtime())
                .unwrap_or(true);
            if changed && !a.is_dir() {
                Log::err(&format!("{} modified {} directly!", dofile, t));
                Log::err("...you should update $3 (a temp file) or stdout, not $1.");
                final_rv = 206;
            }
        }
        if st_tmp.is_some() && st_out.size() > 0 {
            Log::err(&format!("{} wrote to stdout *and* created $3.", dofile));
            Log::err("...you should write status messages to stderr, not stdout.");
            final_rv = 207;
        }
    }

    if final_rv == 0 {
        if st_out.size() > 0 && st_tmp.is_none() {
            // Copy stdout capture to tmpname.
            match fs::File::create(&tmpname) {
                Ok(mut newf) => {
                    let mut buf = vec![];
                    outfile.seek(SeekFrom::Start(0))?;
                    outfile.read_to_end(&mut buf)?;
                    newf.write_all(&buf)?;
                    let _ = newf.sync_all();
                }
                Err(e) => {
                    let dnt = abs_target.parent().unwrap_or(Path::new("."));
                    if !dnt.exists() {
                        Log::err(&format!("{}: target dir {:?} does not exist!", t, dnt));
                    } else {
                        Log::err(&format!("{}: copy stdout: {}", t, e));
                    }
                    final_rv = 209;
                }
            }
        }
        if final_rv == 0 && tmpname.exists() {
            if let Err(e) = durable_rename(&tmpname, &abs_target) {
                Log::err(&format!("{}: rename {:?}: {}", t, tmpname, e));
                final_rv = 209;
            }
        } else {
            unlink_best_effort(&abs_target);
        }
        if final_rv == 0 && should_test_crash_after_rename() {
            // Deterministic crash window for tests: target exists, but state hasn't been recorded.
            unsafe { libc::_exit(217) };
        }
        if final_rv == 0 {
            sf.refresh()?;
        }
        sf.is_generated = true;
        sf.is_override = false;
        // Record new state:
        // If redo-stamp already checked/changed this file during the run, avoid
        // forcing set_changed; otherwise mark it changed for this run even if
        // the stamp didn't change (eg. target missing before and after).
        if sf.is_checked() || sf.is_changed() {
            sf.stamp = Some(sf.read_stamp()?);
        } else {
            sf.csum = None;
            sf.update_stamp(false)?;
            sf.set_changed();
        }
        sf.zap_deps2()?;
        if strict {
            let base = env::v().base;
            let trace_entries: Vec<String> = match trace_out0.as_ref() {
                Some(p) => read_trace_out0(p)
                    .unwrap_or_else(|_| vec!["TRACE_UNAVAILABLE:read_failed".to_string()]),
                None => vec!["TRACE_UNAVAILABLE:missing_path".to_string()],
            };
            if let Some(p) = trace_out0.as_ref() {
                let _ = fs::remove_file(p);
            }
            let declared = sf.deps().unwrap_or_default();
            let res = evaluate_strict_readtrace(&base, &declared, &trace_entries);
            if !res.trace_ok {
                let why = if res.trace_reason.is_empty() {
                    "unknown".to_string()
                } else {
                    res.trace_reason.clone()
                };
                let msg = format!("strict: read trace unavailable ({})", why);
                if strict_fail {
                    Log::err(&msg);
                    final_rv = 218;
                } else {
                    Log::warn(&msg);
                }
            } else if !res.violations.is_empty() {
                let msg = format!("strict: undeclared reads: {}", res.violations.join(", "));
                if strict_fail {
                    Log::err(&msg);
                    final_rv = 218;
                } else {
                    Log::warn(&msg);
                }
            }
        }

        if final_rv == 0 {
            sf.save()?;
            state::commit()?;
        } else {
            unlink_best_effort(&tmpname);
            sf.set_failed()?;
            sf.zap_deps2()?;
            sf.save()?;
            state::commit()?;
        }
    } else {
        unlink_best_effort(&tmpname);
        sf.set_failed()?;
        sf.zap_deps2()?;
        sf.save()?;
        state::commit()?;
    }

    if let Some(l) = lock.as_mut() {
        l.unlock()?;
    }
    Log::meta("done", &format!("{} {}", final_rv, trp), None);
    Ok(final_rv)
}

#[cfg(any())]
pub fn run_ifchange(targets: &[String]) -> anyhow::Result<i32> {
    state::init(targets)?;
    jobserver::setup(0)?;
    let mut rc: i32 = 0;

    // If called from inside a .do, record deps on behalf of the current target.
    let e = env::v();
    if !e.target.is_empty() && !e.unlocked {
        let me = PathBuf::from(e.startdir)
            .join(PathBuf::from(e.pwd))
            .join(PathBuf::from(e.target));
        let f = state::File::by_name(me.to_string_lossy().as_ref(), true)?;
        for t in targets {
            f.add_dep('m', t)?;
        }
        f.save()?;
        state::commit()?;
    }

    // Optional deep parallelism: if multiple targets are passed, and there is
    // no checksum uncertainty (OOB), schedule builds concurrently using the
    // jobserver model.
    // Safety valve: only enable this parallel scheduler when redo-ifchange is
    // invoked as a toplevel command. Nested redo-ifchange (from inside .do)
    // can multiply concurrency in ways that increase sqlite contention.
    let want_parallel = env::is_toplevel() && targets.len() > 1 && !env::v().locks_broken;
    let mut can_parallel = want_parallel;
    if want_parallel && !env::v().no_oob {
        // Dry-run scan for OOB cases; rollback any writes.
        for t in targets {
            let mut f = state::File::by_name(t, true)?;
            let runid = env::v().runid.unwrap_or(0);
            let dirty = deps::isdirty_default(&mut f, runid)?;
            if let deps::DirtyResult::MustBuild(list) = dirty {
                if !(list.len() == 1 && list[0].id == f.id) {
                    can_parallel = false;
                    break;
                }
            }
        }
        // Undo any "checked" bookkeeping from the scan.
        state::rollback()?;
    }

    let mut scheduled_any = false;
    let rc_flag = Arc::new(AtomicI32::new(0));

    for t in targets {
        let mut f = state::File::by_name(t, true)?;
        let runid = env::v().runid.unwrap_or(0);
        let dirty = deps::isdirty_default(&mut f, runid)?;
        match dirty {
            deps::DirtyResult::Clean => {}
            deps::DirtyResult::MustBuild(list) if !env::v().no_oob => {
                // If the "maybe dirty"
                // list is exactly [f], treat it as definitively dirty (no OOB),
                // otherwise do OOB to resolve uncertainty.
                if list.len() == 1 && list[0].id == f.id {
                    let rv = build_one(t)?;
                    if rv != 0 {
                        rc = 1;
                    }
                    continue;
                }
                // Out-of-band build for checksum uncertainty.
                // Hold the target lock while we run redo-unlocked, which will rebuild deps and
                // then reconsider rebuilding `t` without grabbing its lock.
                let mut lock = state::Lock::new(f.id)?;
                lock.waitlock(false)?;

                let cwd = std::env::current_dir()?;
                let base = env::v().base;
                let fix = |p: &str| {
                    state::relpath(&base.join(p), &cwd)
                        .to_string_lossy()
                        .to_string()
                };

                let mut args: Vec<String> = Vec::new();
                args.push(fix(&f.name));
                for d in list {
                    if d.id != f.id {
                        args.push(fix(&d.name));
                    }
                }

                // Critical: flush any sqlite writes (eg. set_checked_save from deps::isdirty)
                // before spawning a subprocess. Otherwise we can hold a write transaction
                // open while redo-unlocked runs, causing other processes to hit
                // "database is locked" after busy_timeout.
                state::commit()?;
                let status = std::process::Command::new("redo-unlocked")
                    .args(&args)
                    .status()?;
                lock.unlock()?;
                if !status.success() {
                    rc = 1;
                    if !env::v().keep_going {
                        break;
                    }
                }
            }
            _ => {
                if can_parallel && !env::v().unlocked {
                    if rc_flag.load(Ordering::Relaxed) != 0 && !env::v().keep_going {
                        break;
                    }
                    schedule_build_one(t, rc_flag.clone())?;
                    scheduled_any = true;
                } else {
                    let rv = build_one(t)?;
                    if rv != 0 {
                        rc = 1;
                        if !env::v().keep_going {
                            break;
                        }
                    }
                }
            }
        }
    }

    if scheduled_any {
        jobserver::wait_all()?;
        rc = std::cmp::max(rc, rc_flag.load(Ordering::Relaxed));
    }
    // Commit at the end; important because deps.isdirty
    // can write (set_checked_save), and leaving an open transaction can block
    // other redo processes and lead to "database is locked".
    state::commit()?;
    Ok(rc)
}

/// Scheduler for `redo-ifchange`.
///
/// Key properties:
/// - trylock-first pass with `locked/waiting/unlocked` meta-lines
/// - nested deep parallelism under the jobserver (no toplevel-only gating)
/// - OOB (`redo-unlocked`) runs as a jobserver job, not synchronously
/// - emits `unchanged` meta-lines for clean generated targets
pub fn run_ifchange(targets: &[String]) -> anyhow::Result<i32> {
    state::init(targets)?;
    jobserver::setup(0)?;
    let mut rc: i32 = 0;

    // If called from inside a .do, record deps on behalf of the current target.
    let e = env::v();
    if !e.target.is_empty() && !e.unlocked {
        let me = PathBuf::from(e.startdir)
            .join(PathBuf::from(e.pwd))
            .join(PathBuf::from(e.target));
        let f = state::File::by_name(me.to_string_lossy().as_ref(), true)?;
        for t in targets {
            f.add_dep('m', t)?;
        }
        f.save()?;
        state::commit()?;
    }

    // Toplevel with no args defaults to "all".
    let mut use_targets: Vec<String> = if env::is_toplevel() && targets.is_empty() {
        vec!["all".to_string()]
    } else {
        targets.to_vec()
    };

    if env::v().shuffle && use_targets.len() > 1 {
        shuffle_in_place(&mut use_targets);
    }

    for t in &use_targets {
        if t.contains('\n') {
            Log::err(&format!("{:?}: filenames containing newlines are not allowed.", t));
            return Ok(204);
        }
    }

    // If redo-unlocked told us to skip locking, keep behavior simple.
    if env::v().unlocked {
        for t in &use_targets {
            if t.is_empty() {
                Log::err("cannot build the empty target (\"\").");
                return Ok(204);
            }
            let mut f = state::File::by_name(t, true)?;
            let runid = env::v().runid.unwrap_or(0);
            let dirty = deps::isdirty_default(&mut f, runid)?;
            if matches!(dirty, deps::DirtyResult::Clean) {
                if f.is_generated {
                    let abs = env::v().base.join(&f.name);
                    let trp = state::target_relpath(&abs).to_string_lossy().to_string();
                    Log::meta("unchanged", &trp, None);
                }
            } else {
                let rv = build_one(t)?;
                if rv != 0 {
                    rc = 1;
                    if !env::v().keep_going {
                        break;
                    }
                }
            }
        }
        state::commit()?;
        return Ok(rc);
    }

    // Foreground-cheat token behavior.
    let cheat_lock: Option<Mutex<state::Lock>> = if !env::v().target.is_empty() && !env::v().unlocked {
        let me = PathBuf::from(env::v().startdir)
            .join(PathBuf::from(env::v().pwd))
            .join(PathBuf::from(env::v().target));
        let myfile = state::File::by_name(me.to_string_lossy().as_ref(), true)?;
        Some(Mutex::new(state::Lock::new(state::LOG_LOCK_MAGIC + myfile.id)?))
    } else {
        None
    };
    let cheat = || -> i32 {
        let Some(m) = &cheat_lock else { return 0; };
        let mut l = m.lock().unwrap();
        match l.trylock() {
            Ok(true) => {
                let _ = l.unlock();
                0
            }
            Ok(false) => 1,
            Err(_) => 0,
        }
    };

    let rc_flag = Arc::new(AtomicI32::new(0));
    let mut locked: Vec<LockedTarget> = Vec::new();
    let mut pool_blocked: Vec<LockedTarget> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // First pass: trylock everything we can; don't block on locked targets.
    for t in &use_targets {
        if t.is_empty() {
            Log::err("cannot build the empty target (\"\").");
            return Ok(204);
        }
        if !seen.insert(t.clone()) {
            continue;
        }

        if !jobserver::has_token() {
            state::commit()?;
        }
        jobserver::ensure_token_or_cheat(t, &cheat)?;

        if rc_flag.load(Ordering::Relaxed) != 0 && !env::v().keep_going {
            break;
        }
        if !state::check_sane() {
            Log::err(".redo directory disappeared; cannot continue.");
            return Ok(205);
        }

        let mut f = state::File::by_name(t, true)?;
        let mut lock = state::Lock::new(f.id)?;
        match lock.trylock() {
            Ok(false) => {
                let abs = env::v().base.join(&f.name);
                let trp = state::target_relpath(&abs).to_string_lossy().to_string();
                Log::meta("locked", &trp, None);
                locked.push(LockedTarget {
                    fid: f.id,
                    t_arg: t.clone(),
                    name: f.name.clone(),
                });
            }
            Ok(true) => {
                f.refresh()?;
                let runid = env::v().runid.unwrap_or(0);
                let dirty = deps::isdirty_default(&mut f, runid)?;
                match dirty {
                    deps::DirtyResult::Clean => {
                        if f.is_generated {
                            let abs = env::v().base.join(&f.name);
                            let trp = state::target_relpath(&abs).to_string_lossy().to_string();
                            Log::meta("unchanged", &trp, None);
                        }
                        lock.unlock()?;
                    }
                    deps::DirtyResult::MustBuild(list)
                        if !env::v().no_oob && !(list.len() == 1 && list[0].id == f.id) =>
                    {
                        let abs = env::v().base.join(&f.name);
                        let trp = state::target_relpath(&abs).to_string_lossy().to_string();
                        Log::meta("check", &trp, None);

                        let cwd = std::env::current_dir()?;
                        let base = env::v().base;
                        let fix = |p: &str| {
                            state::relpath(&base.join(p), &cwd)
                                .to_string_lossy()
                                .to_string()
                        };
                        let mut args: Vec<String> = Vec::new();
                        args.push(fix(&f.name));
                        for d in list {
                            if d.id != f.id {
                                args.push(fix(&d.name));
                            }
                        }

                        start_redo_unlocked_job(t, args, lock, rc_flag.clone())?;
                    }
                    _ => {
                        let fid = f.id;
                        let name = f.name.clone();
                        match schedule_build_one(t, f, Some(lock), rc_flag.clone())? {
                            ScheduleBuildOneResult::DeferredPool => {
                                let abs = env::v().base.join(&name);
                                let trp = state::target_relpath(&abs).to_string_lossy().to_string();
                                Log::meta("waiting", &trp, None);
                                pool_blocked.push(LockedTarget {
                                    fid,
                                    t_arg: t.clone(),
                                    name,
                                });
                                // We acquired a token but did not start a job; release it.
                                let _ = jobserver::release_mine();
                            }
                            ScheduleBuildOneResult::Scheduled => {}
                        }
                    }
                }
            }
            Err(e) => {
                if e.downcast_ref::<cycles::CyclicDependencyError>().is_some() {
                    Log::err(&format!("cyclic dependency while checking {}", f.name));
                    return Ok(208);
                }
                return Err(e);
            }
        }

        state::commit()?;
    }

    // Second pass: wait for remaining locks one-by-one, releasing our token while blocking.
    let mut pool_backoff_ms: u64 = 10;
    while !locked.is_empty() || !pool_blocked.is_empty() || jobserver::running() {
        state::commit()?;
        jobserver::wait_all()?;

        jobserver::ensure_token_or_cheat("self", &cheat)?;
        if rc_flag.load(Ordering::Relaxed) != 0 && !env::v().keep_going {
            break;
        }
        if locked.is_empty() && pool_blocked.is_empty() {
            continue;
        }
        if !state::check_sane() {
            Log::err(".redo directory disappeared; cannot continue.");
            return Ok(205);
        }

        // Prefer resolving target lock contention first.
        if locked.is_empty() {
            let lt = pool_blocked.remove(0);
            let abs = env::v().base.join(&lt.name);
            let trp = state::target_relpath(&abs).to_string_lossy().to_string();

            // Someone else may have started building this target since we deferred it.
            let mut lock = state::Lock::new(lt.fid)?;
            match lock.trylock()? {
                false => {
                    Log::meta("locked", &trp, None);
                    locked.push(lt);
                }
                true => {
                    let fcheck = state::File::by_name(&lt.t_arg, true)?;
                    if fcheck.is_failed() {
                        Log::err(&format!("{}: failed in another thread", lt.name));
                        rc_flag.store(2, Ordering::Relaxed);
                        lock.unlock()?;
                    } else {
                        let mut f = state::File::by_name(&lt.t_arg, true)?;
                        f.refresh()?;
                        let runid = env::v().runid.unwrap_or(0);
                        let dirty = deps::isdirty_default(&mut f, runid)?;
                        match dirty {
                            deps::DirtyResult::Clean => {
                                if f.is_generated {
                                    Log::meta("unchanged", &trp, None);
                                }
                                lock.unlock()?;
                                pool_backoff_ms = 10;
                            }
                            deps::DirtyResult::MustBuild(list)
                                if !env::v().no_oob && !(list.len() == 1 && list[0].id == f.id) =>
                            {
                                Log::meta("check", &trp, None);
                                let cwd = std::env::current_dir()?;
                                let base = env::v().base;
                                let fix = |p: &str| {
                                    state::relpath(&base.join(p), &cwd)
                                        .to_string_lossy()
                                        .to_string()
                                };
                                let mut args: Vec<String> = Vec::new();
                                args.push(fix(&f.name));
                                for d in list {
                                    if d.id != f.id {
                                        args.push(fix(&d.name));
                                    }
                                }
                                start_redo_unlocked_job(&lt.t_arg, args, lock, rc_flag.clone())?;
                                pool_backoff_ms = 10;
                            }
                            _ => {
                                let fid = f.id;
                                let name = f.name.clone();
                                match schedule_build_one(&lt.t_arg, f, Some(lock), rc_flag.clone())? {
                                    ScheduleBuildOneResult::DeferredPool => {
                                        Log::meta("waiting", &trp, None);
                                        pool_blocked.push(LockedTarget {
                                            fid,
                                            t_arg: lt.t_arg,
                                            name,
                                        });
                                        // Critical: avoid holding an open sqlite write transaction
                                        // while we back off waiting for a pool slot.
                                        state::commit()?;
                                        let _ = jobserver::release_mine();
                                        std::thread::sleep(std::time::Duration::from_millis(
                                            std::cmp::min(pool_backoff_ms, 1000),
                                        ));
                                        pool_backoff_ms = std::cmp::min(pool_backoff_ms * 2, 1000);
                                    }
                                    ScheduleBuildOneResult::Scheduled => {
                                        pool_backoff_ms = 10;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            continue;
        }

        let lt = locked.remove(0);
        let mut lock = state::Lock::new(lt.fid)?;
        let mut backoff_ms: u64 = 10;
        let mut owned = lock.trylock()?;
        while !owned {
            std::thread::sleep(std::time::Duration::from_millis(std::cmp::min(
                backoff_ms, 1000,
            )));
            backoff_ms = std::cmp::min(backoff_ms * 2, 1000);

            let abs = env::v().base.join(&lt.name);
            let trp = state::target_relpath(&abs).to_string_lossy().to_string();
            Log::meta("waiting", &trp, None);

            if let Err(e) = lock.check() {
                if e.downcast_ref::<cycles::CyclicDependencyError>().is_some() {
                    Log::err(&format!("cyclic dependency while building {}", lt.name));
                    return Ok(208);
                }
                return Err(e);
            }

            let _ = jobserver::release_mine();
            lock.waitlock(false)?;
            lock.unlock()?;
            jobserver::ensure_token_or_cheat(&lt.t_arg, &cheat)?;
            owned = lock.trylock()?;
        }

        let abs = env::v().base.join(&lt.name);
        let trp = state::target_relpath(&abs).to_string_lossy().to_string();
        Log::meta("unlocked", &trp, None);

        let fcheck = state::File::by_name(&lt.t_arg, true)?;
        if fcheck.is_failed() {
            Log::err(&format!("{}: failed in another thread", lt.name));
            rc_flag.store(2, Ordering::Relaxed);
            lock.unlock()?;
            continue;
        }

        let mut f = state::File::by_name(&lt.t_arg, true)?;
        f.refresh()?;
        let runid = env::v().runid.unwrap_or(0);
        let dirty = deps::isdirty_default(&mut f, runid)?;
        match dirty {
            deps::DirtyResult::Clean => {
                if f.is_generated {
                    Log::meta("unchanged", &trp, None);
                }
                lock.unlock()?;
            }
            deps::DirtyResult::MustBuild(list)
                if !env::v().no_oob && !(list.len() == 1 && list[0].id == f.id) =>
            {
                Log::meta("check", &trp, None);
                let cwd = std::env::current_dir()?;
                let base = env::v().base;
                let fix = |p: &str| {
                    state::relpath(&base.join(p), &cwd)
                        .to_string_lossy()
                        .to_string()
                };
                let mut args: Vec<String> = Vec::new();
                args.push(fix(&f.name));
                for d in list {
                    if d.id != f.id {
                        args.push(fix(&d.name));
                    }
                }
                start_redo_unlocked_job(&lt.t_arg, args, lock, rc_flag.clone())?;
            }
            _ => {
                let fid = f.id;
                let name = f.name.clone();
                match schedule_build_one(&lt.t_arg, f, Some(lock), rc_flag.clone())? {
                    ScheduleBuildOneResult::DeferredPool => {
                        Log::meta("waiting", &trp, None);
                        pool_blocked.push(LockedTarget {
                            fid,
                            t_arg: lt.t_arg,
                            name,
                        });
                        let _ = jobserver::release_mine();
                    }
                    ScheduleBuildOneResult::Scheduled => {}
                }
            }
        }
    }

    state::commit()?;
    rc = std::cmp::max(rc, rc_flag.load(Ordering::Relaxed));
    Ok(rc)
}

pub fn run_redo(targets: &[String], jobs: i32) -> anyhow::Result<i32> {
    state::init(targets)?;
    jobserver::setup(jobs)?;
    let mut rc = 0;
    let mut use_targets: Vec<String> = if targets.is_empty() {
        if env::is_toplevel() {
            vec!["all".to_string()]
        } else {
            vec![]
        }
    } else {
        targets.to_vec()
    };

    if env::v().shuffle && use_targets.len() > 1 {
        shuffle_in_place(&mut use_targets);
    }

    // Parallelism: for multiple independent toplevel targets, run separate
    // child `redo <target>` processes under the jobserver token model.
    // This avoids a deep refactor of the builder while still achieving true -j>1
    // concurrency (needed for barrier-style tests).
    if jobs > 1 && use_targets.len() > 1 {
        let rc_flag = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
        let mut todo: Vec<String> = Vec::new();

        for t in &use_targets {
            if t.contains('\n') {
                Log::err(&format!("{:?}: filenames containing newlines are not allowed.", t));
                return Ok(204);
            }
            if t.is_empty() {
                Log::err("cannot build the empty target (\"\").");
                return Ok(204);
            }
            if env::v().base.join(t).exists() {
                let f = state::File::by_name(t, true)?;
                if !f.is_generated {
                    Log::warn(&format!(
                        "{}: exists and not marked as generated; not redoing.",
                        f.name
                    ));
                    continue;
                }
            }
            todo.push(t.clone());
        }

        for t in todo {
            if rc_flag.load(std::sync::atomic::Ordering::Relaxed) != 0 && !env::v().keep_going {
                break;
            }
            jobserver::ensure_token_or_cheat(&t, || 0)?;
            let rc2 = rc_flag.clone();
            let t2 = t.clone();
            jobserver::start(
                &t,
                move || {
                    // Increase nesting depth for child redo processes.
                    std::env::set_var("REDO_DEPTH", format!("{}  ", env::v().depth));
                    unsafe {
                        // Ensure SIGPIPE default in child.
                        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                    }
                    let argv0 = std::ffi::CString::new("redo")?;
                    let arg1 = std::ffi::CString::new(t2.as_str())?;
                    let args: [*const libc::c_char; 3] =
                        [argv0.as_ptr(), arg1.as_ptr(), std::ptr::null()];
                    unsafe {
                        libc::execvp(argv0.as_ptr(), args.as_ptr());
                        libc::_exit(99);
                    }
                },
                move |_name, rv| {
                    if rv != 0 {
                        rc2.store(1, std::sync::atomic::Ordering::Relaxed);
                    }
                },
            )?;
        }

        jobserver::wait_all()?;
        return Ok(rc_flag.load(std::sync::atomic::Ordering::Relaxed));
    }

    for t in &use_targets {
        if t.contains('\n') {
            Log::err(&format!("{:?}: filenames containing newlines are not allowed.", t));
            return Ok(204);
        }
        if t.is_empty() {
            Log::err("cannot build the empty target (\"\").");
            return Ok(204);
        }
        // `redo` always forces a rebuild of requested
        // targets (redo-ifchange is the command that checks dirtiness).
        if env::v().base.join(t).exists() {
            let f = state::File::by_name(t, true)?;
            if !f.is_generated {
                Log::warn(&format!(
                    "{}: exists and not marked as generated; not redoing.",
                    f.name
                ));
                continue;
            }
        }
        let rv = build_one(t)?;
        if rv != 0 {
            rc = 1;
            if !env::v().keep_going {
                break;
            }
        }
    }
    Ok(rc)
}

fn shuffle_in_place_with_seed(v: &mut [String], mut x: u64) {
    if x == 0 {
        x = 0x1234_5678_9abc_def0;
    }
    for i in (1..v.len()).rev() {
        // xorshift64*
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        x = x.wrapping_mul(0x2545F4914F6CDD1D);
        let j = (x as usize) % (i + 1);
        v.swap(i, j);
    }
}

fn shuffle_in_place(v: &mut [String]) {
    // Simple xorshift RNG; good enough for testing `--shuffle` behavior.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let seed = now ^ (unsafe { libc::getpid() as u64 } << 1);
    shuffle_in_place_with_seed(v, seed);
}

#[cfg(test)]
mod tests {
    use super::shuffle_in_place_with_seed;

    #[test]
    fn shuffle_is_permutation_for_many_seeds() {
        // Basic correctness: shuffle should never drop/duplicate elements.
        for seed in 1u64..=10_000 {
            let mut v: Vec<String> = (0..16).map(|i| i.to_string()).collect();
            let orig = v.clone();
            shuffle_in_place_with_seed(&mut v, seed);
            let mut v_sorted = v.clone();
            let mut orig_sorted = orig.clone();
            v_sorted.sort();
            orig_sorted.sort();
            assert_eq!(v_sorted, orig_sorted, "seed={seed}");
        }
    }

    #[test]
    fn shuffle_distribution_sanity_check() {
        // This is *not* a cryptographic/statistical certification (eg. NIST SP 800-22).
        // It's a deterministic “smoke test” to catch obvious bias/regressions.
        //
        // We shuffle 8 items across many fixed seeds and check that item "0" lands
        // in each position roughly equally often.
        let n = 8usize;
        let samples = 50_000u64;
        let mut counts = vec![0u64; n];
        for seed in 1..=samples {
            let mut v: Vec<String> = (0..n).map(|i| i.to_string()).collect();
            shuffle_in_place_with_seed(&mut v, seed);
            let pos0 = v.iter().position(|s| s == "0").unwrap();
            counts[pos0] += 1;
        }
        let expected = samples as f64 / n as f64;
        // Allow a generous relative deviation so this stays stable across minor RNG tweaks.
        // (If this trips, it likely indicates a bug like using the wrong modulo range.)
        let max_rel_dev = 0.05; // 5%
        for (i, c) in counts.iter().enumerate() {
            let rel = (*c as f64 - expected).abs() / expected;
            assert!(
                rel <= max_rel_dev,
                "position {i} count={c} expected~{expected} rel_dev={rel}"
            );
        }
    }
}
