//! Build execution engine.
//!
//! Responsibilities:
//! - Locate the appropriate `.do` file for a target.
//! - Execute the build script with correct `$1/$2/$3` semantics.
//! - Write outputs atomically and record dependencies/state.
//! - Coordinate parallelism via the GNU make jobserver and integrate with `redo-log`.

use std::ffi::CString;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicI32, Ordering};

use crate::{cycles, deps, env, jobserver, logs::Log, paths, state};
use std::time::{SystemTime, UNIX_EPOCH};

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

fn exec_dofile_in_child(
    dodir: &Path,
    dofile: &str,
    arg1: &str,
    arg2: &str,
    arg3: &str,
    stdout_fd: RawFd,
    lock_fid: i64,
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

struct PendingBuild {
    t: String,
    trp: String,
    abs_target: PathBuf,
    tmpname: PathBuf,
    outfile: fs::File,
    before_t: Option<std::fs::Metadata>,
    dofile: String,
    sf: state::File,
    lock: Option<state::Lock>,
}

fn schedule_build_one(
    t: &str,
    mut sf: state::File,
    mut lock: Option<state::Lock>,
    rc_flag: Arc<AtomicI32>,
) -> anyhow::Result<()> {
    let abs_target = env::v().base.join(&sf.name);

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
        return Ok(());
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
        return Ok(());
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
            return Ok(());
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
        return Ok(());
    };

    // Args are expressed relative to the .do file directory.
    let arg1 = format!("{}{}", basename, ext); // target name (including extension)
    let arg2 = basename; // target name (without extension)

    // Temp name is in the .do directory.
    let tmpname = dodir.join(format!("{}.redo.tmp", arg1));
    unlink_best_effort(&tmpname);

    let outfile = mkstemp_unlinked()?;
    let outfd = outfile.as_raw_fd();

    // Run script; it will typically write to stdout or to $3.
    // $3 is expressed relative to dodir.
    let rel_arg3 = pathdiff::diff_paths(&tmpname, &dodir).unwrap_or(tmpname.clone());

    let pb = PendingBuild {
        t: t.to_string(),
        trp: trp.clone(),
        abs_target: abs_target.clone(),
        tmpname: tmpname.clone(),
        outfile,
        before_t,
        dofile: dofile.clone(),
        sf: sf.clone(),
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
    let pb2 = pb.clone();
    let rc2 = rc_flag.clone();

    jobserver::start(
        t,
        move || exec_dofile_in_child(&dodir2, &dofile2, &arg1_2, &arg2_2, &arg3_2, outfd, lock_fid),
        move |_name, rv| {
            let mut pb = pb2.lock().unwrap();

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
                        let _ = pb.sf.zap_deps2();
                        let _ = pb.sf.save();
                        let _ = state::commit();
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

    Ok(())
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

    // Args are expressed relative to the .do file directory.
    let arg1 = format!("{}{}", basename, ext); // target name (including extension)
    let arg2 = basename; // target name (without extension)

    // Temp name is in the .do directory.
    let tmpname = dodir.join(format!("{}.redo.tmp", arg1));
    unlink_best_effort(&tmpname);

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
        // Must be flushed (no open sqlite transaction) before we fork/exec.
        state::commit()?;
        jobserver::start(
            t,
            move || exec_dofile_in_child(&dodir, &dofile, &arg1, &arg2, &arg3, outfd, lock_fid),
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
        sf.save()?;
        state::commit()?;
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
                        schedule_build_one(t, f, Some(lock), rc_flag.clone())?;
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
    while !locked.is_empty() || jobserver::running() {
        state::commit()?;
        jobserver::wait_all()?;

        jobserver::ensure_token_or_cheat("self", &cheat)?;
        if rc_flag.load(Ordering::Relaxed) != 0 && !env::v().keep_going {
            break;
        }
        if locked.is_empty() {
            continue;
        }
        if !state::check_sane() {
            Log::err(".redo directory disappeared; cannot continue.");
            return Ok(205);
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
                schedule_build_one(&lt.t_arg, f, Some(lock), rc_flag.clone())?;
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
