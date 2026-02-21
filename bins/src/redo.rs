use std::ffi::CString;

use redo_core::builder;
use redo_core::deps;
use redo_core::{env, helpers, state};
use redo_core::logs;
use redo_core::logs::Log;
use redo_core::version::TAG;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

extern "C" fn sigint_handler(_: libc::c_int) {
    // Async-signal-safe shutdown path:
    // - propagate SIGINT to our process group so children die too
    // - exit with redo's conventional SIGINT status (200)
    unsafe {
        let _ = libc::kill(0, libc::SIGINT);
        libc::_exit(200);
    }
}

#[derive(Debug, Clone, Copy)]
struct LogReader {
    pid: libc::pid_t,
    saved_stderr: i32,
}

static LOG_READER: OnceLock<Mutex<Option<LogReader>>> = OnceLock::new();

fn isatty(fd: i32) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

fn close_stdin() {
    // Redirect fd0 to /dev/null so builds won't hang on accidental stdin reads.
    let devnull = CString::new("/dev/null").unwrap();
    unsafe {
        let fd = libc::open(devnull.as_ptr(), libc::O_RDONLY);
        if fd >= 0 {
            libc::dup2(fd, 0);
            libc::close(fd);
        }
    }
}

fn await_log_reader() {
    let lr = LOG_READER.get().and_then(|m| m.lock().ok()).and_then(|mut g| g.take());
    let Some(lr) = lr else { return; };

    unsafe {
        // Never actually close fd#1 or fd#2; replace them instead.
        libc::dup2(lr.saved_stderr, 1);
        libc::dup2(lr.saved_stderr, 2);
        libc::close(lr.saved_stderr);
        let mut status: libc::c_int = 0;
        let _ = libc::waitpid(lr.pid, &mut status as *mut _, 0);
    }
}

fn start_stdin_log_reader(
    status: bool,
    details: bool,
    pretty: bool,
    color: i32, // 0=off,1=auto,2=force
    debug_locks: bool,
    debug_pids: bool,
) -> anyhow::Result<()> {
    // Toplevel log pipe: redirect stdout/stderr to a `redo-log` subprocess reading stdin,
    // using an ack fd for a reliable startup handshake.
    // Redirect stdout/stderr to a redo-log subprocess reading stdin, using an ack fd.
    let mut main_pipe = [0i32; 2];
    let mut ack_pipe = [0i32; 2];
    if unsafe { libc::pipe(main_pipe.as_mut_ptr()) } != 0 {
        return Err(anyhow::anyhow!(std::io::Error::last_os_error()));
    }
    if unsafe { libc::pipe(ack_pipe.as_mut_ptr()) } != 0 {
        return Err(anyhow::anyhow!(std::io::Error::last_os_error()));
    }
    let r = main_pipe[0];
    let w = main_pipe[1];
    let ar = ack_pipe[0];
    let aw = ack_pipe[1];

    // Ensure ack write end is inherited by redo-log.
    helpers::close_on_exec(aw, false)?;

    // Flush before forking.
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let _ = std::io::Write::flush(&mut std::io::stderr());

    // Save the original stderr so we can restore it when redo-log exits.
    let saved_stderr = unsafe { libc::dup(2) };
    if saved_stderr < 0 {
        return Err(anyhow::anyhow!(std::io::Error::last_os_error()));
    }

    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // child: exec redo-log
        unsafe {
            libc::close(ar);
            libc::close(w);
            libc::dup2(r, 0);
            libc::close(r);
            // redo-log writes to stdout; point it at our (original) stderr.
            libc::dup2(2, 1);
        }
        let argv0 = CString::new("redo-log")?;
        let arg_recursive = CString::new("--recursive")?;
        let arg_follow = CString::new("--follow")?;
        let arg_status = CString::new(if status && isatty(2) {
            "--status"
        } else {
            "--no-status"
        })?;
        let arg_details = CString::new(if details { "--details" } else { "--no-details" })?;
        let arg_pretty = CString::new(if pretty { "--pretty" } else { "--no-pretty" })?;
        let arg_debug_locks = CString::new(if debug_locks {
            "--debug-locks"
        } else {
            "--no-debug-locks"
        })?;
        let arg_debug_pids = CString::new(if debug_pids {
            "--debug-pids"
        } else {
            "--no-debug-pids"
        })?;
        let arg_color = if color == 0 {
            Some(CString::new("--no-color")?)
        } else if color >= 2 {
            Some(CString::new("--color")?)
        } else {
            None
        };
        let arg_ack = CString::new("--ack-fd")?;
        let arg_fd = CString::new(aw.to_string())?;
        let arg_dash = CString::new("-")?;
        // argv0, recursive, follow, ack-fd, fd, '-', null + optionals
        let mut argv: Vec<CString> = vec![
            argv0,
            arg_recursive,
            arg_follow,
            arg_status,
            arg_details,
            arg_pretty,
            arg_debug_locks,
            arg_debug_pids,
        ];
        if let Some(c) = arg_color {
            argv.push(c);
        }
        argv.push(arg_ack);
        argv.push(arg_fd);
        argv.push(arg_dash);
        let mut ptrs: Vec<*const libc::c_char> = argv.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        unsafe {
            libc::execvp(ptrs[0], ptrs.as_ptr());
            libc::_exit(99);
        }
    }
    if pid < 0 {
        return Err(anyhow::anyhow!(std::io::Error::last_os_error()));
    }

    // parent
    unsafe {
        libc::close(r);
        libc::close(aw);
    }
    let mut buf = [0u8; 8];
    let n = unsafe { libc::read(ar, buf.as_mut_ptr() as *mut libc::c_void, 8) };
    unsafe { libc::close(ar) };
    if n != 8 || &buf != b"REDO-OK\n" {
        return Err(anyhow::anyhow!("failed to start redo-log subprocess"));
    }

    unsafe {
        libc::dup2(w, 1);
        libc::dup2(w, 2);
        libc::close(w);
    }
    let _ = LOG_READER.set(Mutex::new(Some(LogReader { pid, saved_stderr })));
    Ok(())
}

fn main() {
    // Put redo and its children into their own process group so we can
    // propagate SIGINT cleanly.
    let is_toplevel = std::env::var("REDO").is_err();
    unsafe {
        if is_toplevel {
            libc::setpgid(0, 0);
        }
        libc::signal(libc::SIGINT, sigint_handler as usize);
    }

    let args: Vec<String> = std::env::args().skip(1).collect();

    // Minimal option parsing: enough to avoid treating flags as targets.
    let mut targets: Vec<String> = Vec::new();
    let mut jobs: i32 = 0;
    let mut status = true;
    let mut details = true;
    let mut pretty = true;
    let mut color: i32 = 1; // 0=off,1=auto,2=force
    let mut debug_locks = false;
    let mut debug_pids = false;
    let mut plan_only = false;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--version" {
            println!("{}", TAG);
            return;
        }
        if a == "-d" || a == "--debug" {
            std::env::set_var("REDO_DEBUG", "1");
            i += 1;
            continue;
        }
        if a == "-v" || a == "--verbose" {
            std::env::set_var("REDO_VERBOSE", "1");
            i += 1;
            continue;
        }
        if a == "-x" || a == "--xtrace" {
            std::env::set_var("REDO_XTRACE", "1");
            i += 1;
            continue;
        }
        if a == "-k" || a == "--keep-going" {
            std::env::set_var("REDO_KEEP_GOING", "1");
            i += 1;
            continue;
        }
        if a == "--shuffle" {
            std::env::set_var("REDO_SHUFFLE", "1");
            i += 1;
            continue;
        }
        if a == "--debug-locks" {
            debug_locks = true;
            std::env::set_var("REDO_DEBUG_LOCKS", "1");
            i += 1;
            continue;
        }
        if a == "--debug-pids" {
            debug_pids = true;
            std::env::set_var("REDO_DEBUG_PIDS", "1");
            i += 1;
            continue;
        }
        if a == "--no-log" {
            std::env::set_var("REDO_LOG", "0");
            i += 1;
            continue;
        }
        if a == "--no-details" {
            details = false;
            i += 1;
            continue;
        }
        if a == "--no-status" {
            status = false;
            i += 1;
            continue;
        }
        if a == "--plan" {
            plan_only = true;
            i += 1;
            continue;
        }
        if a == "--no-pretty" {
            pretty = false;
            i += 1;
            continue;
        }
        if a == "--no-color" {
            color = 0;
            i += 1;
            continue;
        }
        if a == "--color" {
            color = std::cmp::max(color + 1, 2);
            i += 1;
            continue;
        }
        if a == "-j" || a == "--jobs" {
            jobs = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(0);
            i += 2;
            continue;
        }
        if let Some(rest) = a.strip_prefix("-j") {
            if !rest.is_empty() {
                jobs = rest.parse().unwrap_or(0);
                i += 1;
                continue;
            }
        }
        if let Some(rest) = a.strip_prefix("--jobs=") {
            jobs = rest.parse().unwrap_or(0);
            i += 1;
            continue;
        }
        targets.push(a.clone());
        i += 1;
    }

    // Set default ints unless already set by env/flags.
    let set_defint = |name: &str, val: i32| {
        if std::env::var_os(name).is_none() {
            std::env::set_var(name, val.to_string());
        }
    };
    set_defint("REDO_LOG", if std::env::var("REDO_LOG").ok().as_deref() == Some("0") { 0 } else { 1 });
    set_defint("REDO_PRETTY", if pretty { 1 } else { 0 });
    set_defint("REDO_COLOR", color);

    // Initialize state so env/base/runid are available before we potentially fork redo-log.
    // Don't ignore errors here: later code assumes env/state are initialized.
    if let Err(e) = state::init(&targets) {
        eprintln!("{:?}", e);
        std::process::exit(1);
    }

    // When fcntl locks are broken (eg. WSL), disable parallelism and redo-log for safety.
    if (env::is_toplevel() || jobs > 1) && env::v().locks_broken {
        eprintln!("redo: detected broken fcntl locks; parallelism disabled.");
        eprintln!("redo:   ...details: https://github.com/Microsoft/WSL/issues/1927");
        if jobs > 1 {
            jobs = 1;
        }
    }

    // Planning-only mode: compute the preflight plan (if available) and exit.
    if plan_only {
        match compute_build_plan(&targets) {
            Ok(Some(plan)) => {
                println!(
                    "dirty={} total={} uptodate={}",
                    plan.dirty, plan.total, plan.uptodate
                );
                std::process::exit(0);
            }
            Ok(None) => {
                println!("no_plan");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("{:?}", e);
                std::process::exit(1);
            }
        }
    }

    // Configure logging output mode (pretty vs raw) for non redo-log cases.
    // When env.v.log != 0, we're expected to emit @@REDO meta-lines for redo-log.
    logs::setup(env::v().log != 0, env::v().pretty, env::v().color);
    if env::is_toplevel() {
        if env::v().log != 0 || jobs > 1 {
            close_stdin();
        }
        if env::v().log != 0 {
            // Tell redo-log which top-level targets were requested.
            // This is used for best-effort progress planning/estimation.
            let log_roots: Vec<String> = if targets.is_empty() {
                vec!["all".to_string()]
            } else {
                targets.clone()
            };
            std::env::set_var("REDO_LOG_TOP_TARGETS", log_roots.join("\n"));

            // Failing to start redo-log is fatal.
            if let Err(e) = start_stdin_log_reader(
                status,
                details,
                pretty,
                color,
                debug_locks,
                debug_pids,
            ) {
                eprintln!("failed to start redo-log subprocess; cannot continue: {:?}", e);
                std::process::exit(99);
            }

            // Best-effort build plan: compute a Ninja-like dirty-step denominator
            // for generator-provided plans (eg. CMake -GRedo) and emit it as a
            // single meta-line that redo-log can use for progress display.
            if status {
                if let Ok(Some(plan)) = compute_build_plan(&targets) {
                    Log::meta(
                        "plan",
                        &format!(
                            "dirty={} total={} uptodate={}",
                            plan.dirty, plan.total, plan.uptodate
                        ),
                        None,
                    );
                    // Install the preflight checked set so the actual build can
                    // avoid re-statting the same clean nodes.
                    if let Some(ids) = plan.checked_ids {
                        deps::install_preflight_checked(ids);
                    }
                }
            }
        }
    }

    let rv = match builder::run_redo(&targets, jobs) {
        Ok(rv) => rv,
        Err(e) => {
            eprintln!("{:?}", e);
            1
        }
    };

    if env::is_toplevel() && env::v().log != 0 {
        await_log_reader();
    }

    std::process::exit(rv);
}

#[derive(Debug)]
struct BuildPlan {
    dirty: usize,
    total: usize,
    uptodate: usize,
    checked_ids: Option<HashSet<i64>>,
}

fn compute_build_plan(targets: &[String]) -> anyhow::Result<Option<BuildPlan>> {
    // Only plan when a generator manifest exists.
    let base = env::v().base;
    let plan_targets_path = base.join(".redo").join("plan.targets");
    if !plan_targets_path.exists() {
        return Ok(None);
    }

    let roots: Vec<String> = if targets.is_empty() {
        vec!["all".to_string()]
    } else {
        targets.to_vec()
    };

    let plan_all = load_plan_targets(&base, &plan_targets_path)?;
    if plan_all.is_empty() {
        return Ok(None);
    }

    let scope = compute_plan_scope(&base, &plan_all, &roots);
    if scope.is_empty() {
        return Ok(None);
    }

    let runid = env::v().runid.unwrap_or(0);
    let mut cache = deps::PlanCache::default();
    let mut dirty: usize = 0;
    let mut uptodate: usize = 0;

    for out_rel in &scope {
        let abs = base.join(out_rel);
        let abs_s = abs.to_string_lossy();
        let mut f = match state::File::by_name(abs_s.as_ref(), false) {
            Ok(f) => f,
            Err(_) => {
                // Unknown file -> treat as dirty (likely first build / not yet recorded).
                dirty += 1;
                continue;
            }
        };
        match deps::isdirty_readonly_default(&mut f, runid, &mut cache)? {
            deps::DirtyResult::Clean => uptodate += 1,
            deps::DirtyResult::Dirty | deps::DirtyResult::MustBuild(_) => dirty += 1,
        }
    }

    let total = scope.len();
    let checked_ids = cache.take_checked_ids();
    Ok(Some(BuildPlan {
        dirty,
        total,
        uptodate,
        checked_ids: Some(checked_ids),
    }))
}

fn load_plan_targets(base: &Path, plan_path: &Path) -> anyhow::Result<HashSet<String>> {
    let f = File::open(plan_path)?;
    let mut r = BufReader::new(f);
    let mut out: HashSet<String> = HashSet::new();
    let mut line = String::new();
    loop {
        line.clear();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        let s = line.trim_end_matches(|c| c == '\n' || c == '\r').trim();
        if s.is_empty() || s.starts_with('#') {
            continue;
        }
        // Normalize absolute paths to base-relative when possible.
        let p = Path::new(s);
        if p.is_absolute() {
            if let Ok(rel) = p.strip_prefix(base) {
                out.insert(rel.to_string_lossy().to_string());
                continue;
            }
        }
        out.insert(s.to_string());
    }
    Ok(out)
}

fn compute_plan_scope(base: &Path, plan_all: &HashSet<String>, roots: &[String]) -> HashSet<String> {
    // Parse generated .do files and follow dependencies that are also in plan_all.
    // This is best-effort and intentionally conservative.
    let mut root_keys: Vec<String> = Vec::new();
    for r in roots {
        let s = r.trim();
        if s.is_empty() {
            continue;
        }
        let p = Path::new(s);
        let rel = if p.is_absolute() {
            p.strip_prefix(base)
                .ok()
                .map(|rp| rp.to_string_lossy().to_string())
        } else {
            Some(s.to_string())
        };
        let Some(rel) = rel else { continue; };
        if plan_all.contains(&rel) {
            root_keys.push(rel);
        }
    }
    if root_keys.is_empty() {
        return HashSet::new();
    }

    let mut scope: HashSet<String> = HashSet::new();
    let mut q: VecDeque<String> = VecDeque::new();
    for k in root_keys {
        if scope.insert(k.clone()) {
            q.push_back(k);
        }
    }

    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    while let Some(k) = q.pop_front() {
        let deps = if let Some(v) = adj.get(&k) {
            v.clone()
        } else {
            let v = deps_from_dofile(base, &k, plan_all);
            adj.insert(k.clone(), v.clone());
            v
        };
        for d in deps {
            if !plan_all.contains(&d) {
                continue;
            }
            if scope.insert(d.clone()) {
                q.push_back(d);
            }
        }
    }
    scope
}

fn deps_from_dofile(base: &Path, output_rel: &str, plan_all: &HashSet<String>) -> Vec<String> {
    let mut deps: Vec<String> = Vec::new();
    let do_path = base.join(format!("{output_rel}.do"));
    let f = match File::open(&do_path) {
        Ok(f) => f,
        Err(_) => return deps,
    };
    let mut r = BufReader::new(f);

    // Trust-but-verify: only parse generated scripts.
    let mut first = String::new();
    if r.read_line(&mut first).is_ok() {
        if !first.contains("Generated by CMake Redo generator") {
            return deps;
        }
    }

    let mut line = String::new();
    loop {
        line.clear();
        let n = match r.read_line(&mut line) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        let s = line.trim_start();
        if !s.starts_with("redo-ifchange") {
            continue;
        }
        for d in parse_redo_ifchange_deps(s) {
            if d.contains('$') || d.contains('`') {
                continue;
            }
            let p = Path::new(&d);
            let rel = if p.is_absolute() {
                p.strip_prefix(base)
                    .ok()
                    .map(|rp| rp.to_string_lossy().to_string())
            } else {
                Some(d.clone())
            };
            let Some(rel) = rel else { continue; };
            if plan_all.contains(&rel) {
                deps.push(rel);
            }
        }
    }
    deps.sort();
    deps.dedup();
    deps
}

fn parse_redo_ifchange_deps(line: &str) -> Vec<String> {
    // Best-effort shell-ish word splitting for generated scripts.
    // Supports:
    // - single quotes: '...'
    // - double quotes: \"...\" with backslash escapes
    // - backslash escapes outside quotes
    let mut s = line.trim_start();
    if !s.starts_with("redo-ifchange") {
        return Vec::new();
    }
    s = &s["redo-ifchange".len()..];

    fn is_ws(b: u8) -> bool {
        matches!(b, b' ' | b'\t' | b'\n' | b'\r')
    }

    let bytes = s.as_bytes();
    let mut i: usize = 0;
    let mut words: Vec<String> = Vec::new();
    while i < bytes.len() {
        while i < bytes.len() && is_ws(bytes[i]) {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b'#' {
            break;
        }
        let mut w = String::new();
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    w.push(bytes[i] as char);
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'\'' {
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' if i + 1 < bytes.len() => {
                            i += 1;
                            w.push(bytes[i] as char);
                            i += 1;
                        }
                        b'"' => {
                            i += 1;
                            break;
                        }
                        c => {
                            w.push(c as char);
                            i += 1;
                        }
                    }
                }
            }
            _ => {
                while i < bytes.len() && !is_ws(bytes[i]) {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 1;
                        w.push(bytes[i] as char);
                        i += 1;
                        continue;
                    }
                    w.push(bytes[i] as char);
                    i += 1;
                }
            }
        }
        if !w.is_empty() {
            words.push(w);
        }
    }

    let mut deps: Vec<String> = Vec::new();
    let mut j: usize = 0;
    while j < words.len() {
        let w = &words[j];
        if w == "--from-file" || w == "--from-file0" {
            // Skip the filename arg.
            j = std::cmp::min(j + 2, words.len());
            continue;
        }
        if w.starts_with('-') {
            j += 1;
            continue;
        }
        deps.push(w.clone());
        j += 1;
    }
    deps
}
