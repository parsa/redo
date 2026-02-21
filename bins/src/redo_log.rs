use std::collections::{HashSet, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use redo_core::{env, logs::Log, state};

#[derive(Clone, Copy)]
struct Ansi {
    green: &'static str,
    red: &'static str,
    yellow: &'static str,
    bold: &'static str,
    plain: &'static str,
}

#[derive(Default)]
struct Progress {
    // Absolute, lexically-normalized paths.
    seen: HashSet<String>,
    running: HashSet<String>,
    done: HashSet<String>,
    failed: HashSet<String>,
    uptodate: HashSet<String>,
    // Optional plan set for stable "total steps" progress display.
    planned_steps: Option<HashSet<String>>,
    // Optional preflight plan totals (emitted by `redo` as @@REDO:plan@@).
    plan_dirty_total: Option<usize>,
    plan_total: Option<usize>,
    plan_uptodate_total: Option<usize>,
}

impl Progress {
    fn counts(&self) -> (usize, usize, usize, usize) {
        (
            self.seen.len(),
            self.running.len(),
            self.done.len(),
            self.failed.len(),
        )
    }

    fn has_dirty_plan(&self) -> bool {
        self.plan_dirty_total.is_some()
    }

    fn set_plan_totals(&mut self, dirty: usize, total: usize, uptodate: usize) {
        self.plan_dirty_total = Some(dirty);
        self.plan_total = Some(total);
        self.plan_uptodate_total = Some(uptodate);
    }

    fn note_seen(&mut self, key: String) {
        self.seen.insert(key);
    }

    fn note_started(&mut self, key: String) {
        self.seen.insert(key.clone());
        self.running.insert(key.clone());
        // If a target is re-done in a single run (rare), treat it as active again.
        self.done.remove(&key);
        self.uptodate.remove(&key);
        self.failed.remove(&key);
    }

    fn note_running(&mut self, key: String) {
        self.seen.insert(key.clone());
        self.running.insert(key);
    }

    fn note_unchanged(&mut self, key: String) {
        self.seen.insert(key.clone());
        self.running.remove(&key);
        self.done.insert(key.clone());
        self.uptodate.remove(&key);
        self.failed.remove(&key);
    }

    fn note_uptodate(&mut self, key: String) {
        self.seen.insert(key.clone());
        self.running.remove(&key);
        self.done.remove(&key);
        self.failed.remove(&key);
        self.uptodate.insert(key);
    }

    fn note_done(&mut self, key: String, rv: i32) {
        self.seen.insert(key.clone());
        self.running.remove(&key);
        self.done.insert(key.clone());
        self.uptodate.remove(&key);
        if rv != 0 {
            self.failed.insert(key);
        } else {
            self.failed.remove(&key);
        }
    }
}

fn abs_key(topdir: &Path, mydir: &Path, p: &str) -> String {
    normalize(topdir.join(mydir).join(p))
}

fn isatty(fd: i32) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

fn want_color(color: i32) -> bool {
    if color >= 2 {
        return true;
    }
    if color <= 0 {
        return false;
    }
    // auto
    let term_ok = std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true);
    isatty(1) && term_ok
}

fn ansi_for(color: i32) -> Ansi {
    if want_color(color) {
        Ansi {
            green: "\x1b[32m",
            red: "\x1b[31m",
            yellow: "\x1b[33m",
            bold: "\x1b[1m",
            plain: "\x1b[m",
        }
    } else {
        Ansi {
            green: "",
            red: "",
            yellow: "",
            bold: "",
            plain: "",
        }
    }
}

fn write_pretty_line(
    out: &mut dyn Write,
    ansi: Ansi,
    debug_pids: bool,
    pid: u32,
    color: &'static str,
    text: &str,
) -> anyhow::Result<()> {
    if debug_pids {
        // pid at column 0, then "redo".
        writeln!(
            out,
            "{}{:<6} redo  {}{}{}",
            color, pid, ansi.bold, text, ansi.plain
        )?;
    } else {
        writeln!(out, "{}redo  {}{}{}{}", color, "", ansi.bold, text, ansi.plain)?;
    }
    Ok(())
}

fn main() {
    if let Err(e) = real_main() {
        // Handle EPIPE (common when piped to `head`, `grep -q`, etc). Exit
        // 141 (=128+SIGPIPE) and redirect stdout to /dev/null to avoid a
        // second BrokenPipe error at shutdown.
        if let Some(ioe) = e.downcast_ref::<std::io::Error>() {
            if ioe.kind() == std::io::ErrorKind::BrokenPipe {
                unsafe {
                    let dn = libc::open(b"/dev/null\0".as_ptr() as *const _, libc::O_WRONLY);
                    if dn >= 0 {
                        libc::dup2(dn, 1);
                        libc::close(dn);
                    }
                }
                std::process::exit(141);
            }
        }
        Log::err(&format!("{:?}", e));
        std::process::exit(1);
    }
}

fn real_main() -> anyhow::Result<()> {
    // Implements the subset of `redo-log` functionality exercised by our integration tests
    // (including the vendored `t/` suite).
    // - redo-log -ru <target>
    // - redo-log <target>
    // - redo-log --ack-fd=N -   (pass-through stdin, used by redo's log pipe)
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    let mut ack_fd: Option<i32> = None;
    let mut recursive = false;
    let mut unchanged = false;
    let mut follow = false;
    let mut pretty = true;
    let mut details = true;
    let mut color: i32 = 1; // 0=off, 1=auto, 2+=force
    let mut debug_pids = false;
    let mut debug_locks = false;
    let mut status: i32 = 1; // 0=off, 1=auto, 2+=force
    let mut targets: Vec<String> = Vec::new();

    while let Some(a) = args.first().cloned() {
        args.remove(0);
        if a.starts_with("--ack-fd=") {
            ack_fd = a["--ack-fd=".len()..].parse::<i32>().ok();
            continue;
        }
        if a == "--ack-fd" {
            ack_fd = args.get(0).and_then(|v| v.parse::<i32>().ok());
            if !args.is_empty() {
                args.remove(0);
            }
            continue;
        }
        if a == "-r" || a == "--recursive" {
            recursive = true;
            continue;
        }
        if a == "-u" || a == "--unchanged" {
            unchanged = true;
            continue;
        }
        if a == "-f" || a == "--follow" {
            follow = true;
            continue;
        }
        if a == "--no-status" {
            status = 0;
            continue;
        }
        if a == "--status" {
            status += 1; // default=1 -> 2 (force even when stderr isn't a tty)
            continue;
        }
        if a == "--no-pretty" {
            pretty = false;
            continue;
        }
        if a == "--pretty" {
            pretty = true;
            continue;
        }
        if a == "--no-details" {
            details = false;
            continue;
        }
        if a == "--details" {
            details = true;
            continue;
        }
        if a == "--no-color" {
            color = 0;
            continue;
        }
        if a == "--color" {
            color += 1; // default=1 -> 2 (force even when stdout isn't a tty)
            continue;
        }
        if a == "--debug-pids" {
            debug_pids = true;
            continue;
        }
        if a == "--no-debug-pids" {
            debug_pids = false;
            continue;
        }
        if a == "--debug-locks" {
            debug_locks = true;
            continue;
        }
        if a == "--no-debug-locks" {
            debug_locks = false;
            continue;
        }
        if a.starts_with("-") && a != "-" {
            // allow combined flags like -ru
            for ch in a.chars().skip(1) {
                match ch {
                    'r' => recursive = true,
                    'u' => unchanged = true,
                    'f' => follow = true,
                    _ => {}
                }
            }
            continue;
        }
        // treat as target
        targets.push(a);
        targets.extend(args);
        break;
    }

    if let Some(fd) = ack_fd {
        if fd > 2 {
            let _ = unsafe {
                libc::write(
                    fd,
                    b"REDO-OK\n".as_ptr() as *const libc::c_void,
                    8,
                )
            };
            unsafe {
                libc::close(fd);
            }
        }
    }

    if targets.is_empty() {
        return Err(anyhow::anyhow!("redo-log: give at least one target; maybe \"all\"?"));
    }

    state::init(&targets)?;
    env::inherit()?; // for consistency (noop if already initialized)

    let ansi = ansi_for(color);
    let topdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let start = Instant::now();
    let mut total_lines: u64 = 0;
    let mut progress = Progress::default();
    let stdin_mode = targets.iter().any(|t| t == "-");

    // If redo (toplevel) told us the intended roots, try to load a
    // generator-provided plan manifest for a stable total.
    if stdin_mode {
        if let Ok(v) = std::env::var("REDO_LOG_TOP_TARGETS") {
            let roots: Vec<String> = v
                .split('\n')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
            if let Some(plan_all) = load_plan_targets(&env::v().base) {
                if !plan_all.is_empty() {
                    // "all" is the common case and typically spans most of the build tree.
                    // Avoid a potentially-expensive closure walk and just use the full plan.
                    let building_all = roots.iter().any(|r| r == "all");
                    if building_all || roots.is_empty() {
                        progress.planned_steps = Some(plan_all);
                    } else {
                        progress.planned_steps =
                            Some(compute_plan_scope(&env::v().base, &plan_all, &roots));
                    }
                }
            }
        }
    }

    let mut out = std::io::stdout();
    for t in targets {
        if t == "-" {
            // stdin mode (used by redo's toplevel log pipe): read meta-lines from stdin,
            // optionally recurse into per-target log files, and optionally follow locks.
            let mut already: HashSet<String> = HashSet::new();
            catlog_stdin(
                &topdir,
                "-",
                recursive,
                unchanged,
                follow,
                pretty,
                details,
                color,
                debug_pids,
                debug_locks,
                status,
                start,
                &mut total_lines,
                &mut progress,
                &mut already,
                &mut out,
            )?;
            continue;
        }
        // Emit a top-level trace line for the requested target.
        if pretty {
            write_pretty_line(&mut out, ansi, debug_pids, 0, ansi.green, &t)?;
        } else {
            writeln!(out, "@@REDO:do:0:0.0000@@ {}", t)?;
        }
        let mut already: HashSet<String> = HashSet::new();
        catlog(
            &topdir,
            &t,
            &t,
            recursive,
            unchanged,
            follow,
            pretty,
            details,
            color,
            debug_pids,
            debug_locks,
            status,
            start,
            &mut total_lines,
            &mut progress,
            &mut already,
            &mut out,
        )?;
    }
    Ok(())
}

fn catlog_stdin(
    topdir: &Path,
    top_arg: &str,
    recursive: bool,
    unchanged: bool,
    follow: bool,
    pretty: bool,
    details: bool,
    color: i32,
    debug_pids: bool,
    debug_locks: bool,
    status: i32,
    start: Instant,
    total_lines: &mut u64,
    progress: &mut Progress,
    already: &mut HashSet<String>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let ansi = ansi_for(color);
    let mydir = Path::new("");
    let mut last_status = Instant::now();
    let mut status_active = false;
    let mut line_head = String::new();

    let mut reader = BufReader::new(std::io::stdin());
    loop {
        let mut buf = String::new();
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            break;
        }
        // In follow mode, some tools can emit unterminated lines; buffer until '\n'.
        if !buf.ends_with('\n') {
            line_head.push_str(&buf);
            continue;
        }
        if !line_head.is_empty() {
            line_head.push_str(&buf);
            buf = std::mem::take(&mut line_head);
        }
        *total_lines += 1;
        let line = buf.trim_end_matches('\n').to_string();
        if let Some(meta) = parse_redo_meta_line(&line) {
            if status_active {
                clear_status_line();
                status_active = false;
            }

            // Meta-line handling + inline recursion.
            match meta.kind {
                "plan" => {
                    if let Some((dirty, total, uptodate)) = parse_plan_meta(meta.msg) {
                        progress.set_plan_totals(dirty, total, uptodate);
                    }
                    // Hide in pretty mode; pass-through in raw mode.
                    if !pretty {
                        writeln!(out, "{}", line)?;
                    }
                }
                "cache_hit" | "cache_miss" | "cache_store" | "cache_skip" | "cache_stats" => {
                    // Debug-only: cache events can be verbose.
                    if !pretty {
                        writeln!(out, "{}", line)?;
                    } else if env::v().debug >= 1 {
                        let color = match meta.kind {
                            "cache_hit" | "cache_store" => ansi.green,
                            "cache_miss" | "cache_skip" => ansi.yellow,
                            _ => "",
                        };
                        write_pretty_line(
                            out,
                            ansi,
                            debug_pids,
                            meta.pid,
                            color,
                            &format!("{} {}", meta.kind, meta.msg),
                        )?;
                    }
                }
                "done" => {
                    if let Some((rv, name)) = meta.msg.split_once(' ') {
                        let rv_i = rv.parse::<i32>().unwrap_or(0);
                        let key = abs_key(topdir, mydir, name);
                        progress.note_done(key, rv_i);

                        let rel = state::relpath(&topdir.join(mydir).join(name), topdir);
                        let relname = rel.to_string_lossy().to_string();
                        if pretty {
                            if rv_i != 0 {
                                write_pretty_line(
                                    out,
                                    ansi,
                                    debug_pids,
                                    meta.pid,
                                    ansi.red,
                                    &format!("{} (exit {})", relname, rv_i),
                                )?;
                            } else {
                                write_pretty_line(
                                    out,
                                    ansi,
                                    debug_pids,
                                    meta.pid,
                                    ansi.green,
                                    &format!("{} (done)", relname),
                                )?;
                            }
                        } else {
                            writeln!(out, "{}", line)?;
                        }
                    } else if pretty {
                        write_pretty_line(out, ansi, debug_pids, meta.pid, ansi.green, meta.msg)?;
                    } else {
                        writeln!(out, "{}", line)?;
                    }
                }
                "do" | "locked" | "waiting" | "unlocked" => {
                    let child = mydir.join(meta.msg);
                    let fixname = normalize(child.clone());
                    let rel = state::relpath(&topdir.join(&child), topdir);
                    let relname = rel.to_string_lossy().to_string();

                    // Progress accounting.
                    let key = abs_key(topdir, mydir, meta.msg);
                    match meta.kind {
                        "do" => progress.note_started(key),
                        "locked" => progress.note_running(key),
                        "waiting" | "unlocked" => progress.note_seen(key),
                        _ => {}
                    }

                    if pretty {
                        if debug_locks && meta.kind != "do" {
                            write_pretty_line(
                                out,
                                ansi,
                                debug_pids,
                                meta.pid,
                                "",
                                &format!("{} {}", meta.kind, relname),
                            )?;
                        } else if !already.contains(&fixname) {
                            write_pretty_line(out, ansi, debug_pids, meta.pid, ansi.green, &relname)?;
                        }
                    } else {
                        writeln!(out, "{}", line)?;
                    }

                    if recursive {
                        let child_arg = child.to_string_lossy().to_string();
                        catlog(
                            topdir,
                            top_arg,
                            &child_arg,
                            recursive,
                            unchanged,
                            follow,
                            pretty,
                            details,
                            color,
                            debug_pids,
                            debug_locks,
                            status,
                            start,
                            total_lines,
                            progress,
                            already,
                            out,
                        )?;
                    }
                    already.insert(fixname);
                }
                "unchanged" => {
                    let key = abs_key(topdir, mydir, meta.msg);
                    if progress.has_dirty_plan() {
                        progress.note_uptodate(key);
                    } else {
                        progress.note_unchanged(key);
                    }

                    if unchanged {
                        let child = mydir.join(meta.msg);
                        let fixname = normalize(child.clone());
                        let rel = state::relpath(&topdir.join(&child), topdir);
                        let relname = rel.to_string_lossy().to_string();

                        if pretty {
                            if debug_locks {
                                write_pretty_line(
                                    out,
                                    ansi,
                                    debug_pids,
                                    meta.pid,
                                    "",
                                    &format!("unchanged {}", relname),
                                )?;
                            } else if !already.contains(&fixname) {
                                write_pretty_line(out, ansi, debug_pids, meta.pid, ansi.green, &relname)?;
                            }
                        } else {
                            writeln!(out, "{}", line)?;
                        }

                        if recursive {
                            let child_arg = child.to_string_lossy().to_string();
                            catlog(
                                topdir,
                                top_arg,
                                &child_arg,
                                recursive,
                                unchanged,
                                follow,
                                pretty,
                                details,
                                color,
                                debug_pids,
                                debug_locks,
                                status,
                                start,
                                total_lines,
                                progress,
                                already,
                                out,
                            )?;
                        }
                        already.insert(fixname);
                    }
                }
                "error" => {
                    if pretty {
                        write_pretty_line(
                            out,
                            ansi,
                            debug_pids,
                            meta.pid,
                            ansi.red,
                            &format!("redo: {}", meta.msg),
                        )?;
                    } else {
                        writeln!(out, "{}", line)?;
                    }
                }
                "warning" => {
                    if pretty {
                        write_pretty_line(
                            out,
                            ansi,
                            debug_pids,
                            meta.pid,
                            ansi.yellow,
                            &format!("redo: {}", meta.msg),
                        )?;
                    } else {
                        writeln!(out, "{}", line)?;
                    }
                }
                "debug" => {
                    if pretty {
                        write_pretty_line(out, ansi, debug_pids, meta.pid, "", meta.msg)?;
                    } else {
                        writeln!(out, "{}", line)?;
                    }
                }
                _ => {
                    if pretty {
                        write_pretty_line(out, ansi, debug_pids, meta.pid, "", meta.msg)?;
                    } else {
                        writeln!(out, "{}", line)?;
                    }
                }
            }
        } else {
            // Non-meta line: in details mode, print it; otherwise suppress.
            if details {
                if status_active {
                    clear_status_line();
                    status_active = false;
                }
                writeln!(out, "{}", line)?;
            }
        }

        if follow {
            if maybe_print_status(status, start, &mut last_status, *total_lines, top_arg, progress)? {
                status_active = true;
            }
        }
    }

    // partial line never got terminated
    if !line_head.is_empty() && details {
        if status_active {
            clear_status_line();
            status_active = false;
        }
        writeln!(out, "{}", line_head)?;
    }
    if status_active {
        clear_status_line();
    }
    Ok(())
}

fn catlog(
    topdir: &Path,
    top_arg: &str,
    t: &str,
    recursive: bool,
    unchanged: bool,
    follow: bool,
    pretty: bool,
    details: bool,
    color: i32,
    debug_pids: bool,
    debug_locks: bool,
    status: i32,
    start: Instant,
    total_lines: &mut u64,
    progress: &mut Progress,
    already: &mut HashSet<String>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let ansi = ansi_for(color);
    if already.contains(t) {
        return Ok(());
    }
    already.insert(t.to_string());
    let mydir = Path::new(t).parent().unwrap_or(Path::new(""));
    let stdin_mode = top_arg == "-";

    // Follow-mode race: redo-log might start just before the target becomes locked
    // (especially when there's already an old log file from a previous run).
    // Give a short grace period before treating an unlocked target as "idle".
    let follow_grace_start = Instant::now();
    let mut saw_locked = false;
    // 0.05s isn't always enough for a background `redo` to lock the target on
    // slower machines / under load; keep this comfortably above that.
    const FOLLOW_START_GRACE: Duration = Duration::from_secs(3);

    // In follow mode, also tolerate starting before the target is even recorded in the DB.
    // This can happen if `redo` is launched in the background and `redo-log -f` starts
    // immediately after.
    let mut last_status = Instant::now();
    let f = loop {
        match state::File::by_name(t, false) {
            Ok(f) => break f,
            Err(_) if follow && follow_grace_start.elapsed() < FOLLOW_START_GRACE => {
                let _ = maybe_print_status(status, start, &mut last_status, *total_lines, top_arg, progress);
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
            Err(_) => {
                writeln!(
                    std::io::stderr(),
                    "redo-log: [{}] {:?}: not known to redo.",
                    std::env::current_dir()?.to_string_lossy(),
                    top_arg
                )?;
                std::process::exit(24);
            }
        }
    };

    // Hold a shared "log lock" while reading the log file.
    // This coordinates with writers (and avoids some truncation/race issues).
    let mut loglock = state::Lock::new(f.id + state::LOG_LOCK_MAGIC)?;
    let _ = loglock.waitlock(true);

    let logpath = state::logname(f.id);
    let mut status_active = false;
    let mut line_head = String::new();
    let mut lf = loop {
        match File::open(&logpath) {
            Ok(fh) => break fh,
            Err(_) => {
                // No logs yet. In follow mode, wait while the target is still locked.
                if !follow {
                    return Ok(());
                }
                if maybe_print_status(status, start, &mut last_status, *total_lines, top_arg, progress)? {
                    status_active = true;
                }
                let locked = is_locked(f.id)?;
                if locked {
                    saw_locked = true;
                }
                if !locked {
                    if !stdin_mode && !saw_locked && follow_grace_start.elapsed() < FOLLOW_START_GRACE
                    {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        continue;
                    }
                    if status_active {
                        clear_status_line();
                    }
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    };
    let mut reader = BufReader::new(&mut lf);
    loop {
        let mut buf = String::new();
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            if !follow {
                break;
            }
            // follow: keep going until target lock is released
            if maybe_print_status(status, start, &mut last_status, *total_lines, top_arg, progress)? {
                status_active = true;
            }
            let locked = is_locked(f.id)?;
            if locked {
                saw_locked = true;
            } else {
                if !stdin_mode && !saw_locked && follow_grace_start.elapsed() < FOLLOW_START_GRACE {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }
        if !buf.ends_with('\n') {
            line_head.push_str(&buf);
            continue;
        }
        if !line_head.is_empty() {
            line_head.push_str(&buf);
            buf = std::mem::take(&mut line_head);
        }
        *total_lines += 1;
        let line = buf.trim_end_matches('\n').to_string();
        if let Some(meta) = parse_redo_meta_line(&line) {
            // meta-line
            if status_active {
                clear_status_line();
                status_active = false;
            }

            match meta.kind {
                "plan" => {
                    if let Some((dirty, total, uptodate)) = parse_plan_meta(meta.msg) {
                        progress.set_plan_totals(dirty, total, uptodate);
                    }
                    if !pretty {
                        writeln!(out, "{}", line)?;
                    }
                }
                "cache_hit" | "cache_miss" | "cache_store" | "cache_skip" | "cache_stats" => {
                    // Debug-only: cache events can be verbose.
                    if !pretty {
                        writeln!(out, "{}", line)?;
                    } else if env::v().debug >= 1 {
                        let color = match meta.kind {
                            "cache_hit" | "cache_store" => ansi.green,
                            "cache_miss" | "cache_skip" => ansi.yellow,
                            _ => "",
                        };
                        write_pretty_line(
                            out,
                            ansi,
                            debug_pids,
                            meta.pid,
                            color,
                            &format!("{} {}", meta.kind, meta.msg),
                        )?;
                    }
                }
                "done" => {
                    if let Some((rv, name)) = meta.msg.split_once(' ') {
                        let rv_i = rv.parse::<i32>().unwrap_or(0);
                        let key = abs_key(topdir, mydir, name);
                        progress.note_done(key, rv_i);

                        let rel = state::relpath(&topdir.join(mydir).join(name), topdir);
                        let relname = rel.to_string_lossy().to_string();
                        if pretty {
                            if rv_i != 0 {
                                write_pretty_line(
                                    out,
                                    ansi,
                                    debug_pids,
                                    meta.pid,
                                    ansi.red,
                                    &format!("{} (exit {})", relname, rv_i),
                                )?;
                            } else {
                                write_pretty_line(
                                    out,
                                    ansi,
                                    debug_pids,
                                    meta.pid,
                                    ansi.green,
                                    &format!("{} (done)", relname),
                                )?;
                            }
                        } else {
                            writeln!(out, "{}", line)?;
                        }
                    } else if pretty {
                        write_pretty_line(out, ansi, debug_pids, meta.pid, ansi.green, meta.msg)?;
                    } else {
                        writeln!(out, "{}", line)?;
                    }
                }
                "do" | "locked" | "waiting" | "unlocked" => {
                    let child = mydir.join(meta.msg);
                    let fixname = normalize(child.clone());
                    let rel = state::relpath(&topdir.join(&child), topdir);
                    let relname = rel.to_string_lossy().to_string();

                    // Progress accounting.
                    let key = abs_key(topdir, mydir, meta.msg);
                    match meta.kind {
                        "do" => progress.note_started(key),
                        "locked" => progress.note_running(key),
                        "waiting" | "unlocked" => progress.note_seen(key),
                        _ => {}
                    }

                    if pretty {
                        if debug_locks && meta.kind != "do" {
                            write_pretty_line(
                                out,
                                ansi,
                                debug_pids,
                                meta.pid,
                                "",
                                &format!("{} {}", meta.kind, relname),
                            )?;
                        } else if !already.contains(&fixname) {
                            write_pretty_line(out, ansi, debug_pids, meta.pid, ansi.green, &relname)?;
                        }
                    } else {
                        writeln!(out, "{}", line)?;
                    }

                    if recursive {
                        let child_arg = child.to_string_lossy().to_string();
                        // Release loglock while recursing.
                        let _ = loglock.unlock();
                        catlog(
                            topdir,
                            top_arg,
                            &child_arg,
                            recursive,
                            unchanged,
                            follow,
                            pretty,
                            details,
                            color,
                            debug_pids,
                            debug_locks,
                            status,
                            start,
                            total_lines,
                            progress,
                            already,
                            out,
                        )?;
                        let _ = loglock.waitlock(true);
                    }
                    already.insert(fixname);
                }
                "unchanged" => {
                    let key = abs_key(topdir, mydir, meta.msg);
                    if progress.has_dirty_plan() {
                        progress.note_uptodate(key);
                    } else {
                        progress.note_unchanged(key);
                    }

                    if unchanged {
                        let child = mydir.join(meta.msg);
                        let fixname = normalize(child.clone());
                        let rel = state::relpath(&topdir.join(&child), topdir);
                        let relname = rel.to_string_lossy().to_string();

                        if pretty {
                            if debug_locks {
                                write_pretty_line(
                                    out,
                                    ansi,
                                    debug_pids,
                                    meta.pid,
                                    "",
                                    &format!("unchanged {}", relname),
                                )?;
                            } else if !already.contains(&fixname) {
                                write_pretty_line(out, ansi, debug_pids, meta.pid, ansi.green, &relname)?;
                            }
                        } else {
                            writeln!(out, "{}", line)?;
                        }

                        if recursive {
                            let child_arg = child.to_string_lossy().to_string();
                            let _ = loglock.unlock();
                            catlog(
                                topdir,
                                top_arg,
                                &child_arg,
                                recursive,
                                unchanged,
                                follow,
                                pretty,
                                details,
                                color,
                                debug_pids,
                                debug_locks,
                                status,
                                start,
                                total_lines,
                                progress,
                                already,
                                out,
                            )?;
                            let _ = loglock.waitlock(true);
                        }
                        already.insert(fixname);
                    }
                }
                "error" => {
                    if pretty {
                        write_pretty_line(
                            out,
                            ansi,
                            debug_pids,
                            meta.pid,
                            ansi.red,
                            &format!("redo: {}", meta.msg),
                        )?;
                    } else {
                        writeln!(out, "{}", line)?;
                    }
                }
                "warning" => {
                    if pretty {
                        write_pretty_line(
                            out,
                            ansi,
                            debug_pids,
                            meta.pid,
                            ansi.yellow,
                            &format!("redo: {}", meta.msg),
                        )?;
                    } else {
                        writeln!(out, "{}", line)?;
                    }
                }
                "debug" => {
                    if pretty {
                        write_pretty_line(out, ansi, debug_pids, meta.pid, "", meta.msg)?;
                    } else {
                        writeln!(out, "{}", line)?;
                    }
                }
                _ => {
                    if pretty {
                        write_pretty_line(out, ansi, debug_pids, meta.pid, "", meta.msg)?;
                    } else {
                        writeln!(out, "{}", line)?;
                    }
                }
            }
        } else {
            // non-meta build output
            if details {
                if status_active {
                    clear_status_line();
                    status_active = false;
                }
                writeln!(out, "{}", line)?;
            }
        }
    }
    if !line_head.is_empty() && details {
        if status_active {
            clear_status_line();
            status_active = false;
        }
        writeln!(out, "{}", line_head)?;
    }
    if status_active {
        clear_status_line();
    }
    Ok(())
}

struct MetaLine<'a> {
    kind: &'a str,
    pid: u32,
    msg: &'a str,
}

fn parse_redo_meta_line(line: &str) -> Option<MetaLine<'_>> {
    // Accept: @@REDO:kind:pid:ts@@ msg
    let s = line.strip_prefix("@@REDO:")?;
    let (hdr, msg) = s.split_once("@@ ")?;
    let mut parts = hdr.split(':');
    let kind = parts.next()?;
    let pid = parts
        .next()
        .and_then(|p| p.parse::<u32>().ok())
        .unwrap_or(0);
    Some(MetaLine { kind, pid, msg })
}

fn is_locked(fid: i64) -> anyhow::Result<bool> {
    // Try to acquire the target lock.
    let mut l = state::Lock::new(fid)?;
    let ok = l.trylock()?;
    if ok {
        l.unlock()?;
        Ok(false)
    } else {
        Ok(true)
    }
}

fn clear_status_line() {
    let mut err = std::io::stderr();
    // Best-effort: wipe a typical terminal line.
    let _ = write!(err, "\r{:<200}\r", "");
    let _ = err.flush();
}

fn maybe_print_status(
    status: i32,
    start: Instant,
    last_status: &mut Instant,
    total_lines: u64,
    top_arg: &str,
    progress: &Progress,
) -> anyhow::Result<bool> {
    if status <= 0 {
        return Ok(false);
    }
    // Don't print for extremely short runs.
    if start.elapsed() < Duration::from_secs(1) {
        return Ok(false);
    }
    // Only print in non-tty when forced.
    if status < 2 && !isatty(2) {
        return Ok(false);
    }
    if last_status.elapsed() < Duration::from_millis(100) {
        return Ok(false);
    }
    *last_status = Instant::now();
    let mut err = std::io::stderr();
    let (seen, running, done, failed) = progress.counts();
    let (started2, done2, running2, failed2) = if let Some(plan) = &progress.planned_steps {
        let done_p = progress.done.iter().filter(|k| plan.contains(*k)).count();
        let running_p = progress
            .running
            .iter()
            .filter(|k| plan.contains(*k))
            .count();
        let failed_p = progress.failed.iter().filter(|k| plan.contains(*k)).count();
        let started_p = done_p + running_p;
        (started_p, done_p, running_p, failed_p)
    } else {
        let started = done + running;
        (started, done, running, failed)
    };
    let dirty_total = progress.plan_dirty_total;
    let plan_total = progress.plan_total;
    let plan_uptodate_total = progress.plan_uptodate_total;
    let elapsed = start.elapsed();
    let secs = elapsed.as_secs();
    let mins = secs / 60;
    let sec2 = secs % 60;
    let t = if mins > 0 {
        format!("{mins}:{sec2:02}")
    } else {
        format!("{secs}s")
    };
    // Include the old `total_lines` counter so there is always visible motion,
    // even when progress meta-lines are sparse.
    let suffix = if top_arg != "-" && !top_arg.is_empty() {
        format!(" {}", top_arg)
    } else {
        String::new()
    };
    if let Some(dirty) = dirty_total {
        let mut denom = dirty;
        let mut discovered: usize = 0;
        if started2 > denom {
            discovered = started2 - denom;
            denom = started2;
        }
        let upd = plan_uptodate_total.unwrap_or(0);
        let tot = plan_total.unwrap_or_else(|| denom + upd);
        let disc_suffix = if discovered > 0 {
            format!(", +{discovered} discovered")
        } else {
            String::new()
        };
        write!(
            err,
            "\rredo {started2}/{denom} steps ({done2} done), {running2} running, {failed2} failed, {upd} up-to-date ({}) [{} lines]{}{}\r",
            t,
            total_lines,
            suffix,
            disc_suffix
        )?;
        let _ = tot; // kept for potential future UI tweaks
    } else if let Some(plan) = &progress.planned_steps {
        let total = plan.len();
        write!(
            err,
            "\rredo {started2}/{total} steps ({done2} done), {running2} running, {failed2} failed ({}) [{} lines]{}\r",
            t,
            total_lines,
            suffix
        )?;
    } else {
        write!(
            err,
            "\rredo {started2}/{seen} started ({done2} done), {running2} running, {failed2} failed ({}) [{} lines]{}\r",
            t,
            total_lines,
            suffix
        )?;
    }
    err.flush()?;
    Ok(true)
}

fn parse_plan_meta(msg: &str) -> Option<(usize, usize, usize)> {
    // Accept: "dirty=N total=M uptodate=K" (order-insensitive; may contain extras).
    let mut dirty: Option<usize> = None;
    let mut total: Option<usize> = None;
    let mut uptodate: Option<usize> = None;
    for w in msg.split_whitespace() {
        let Some((k, v)) = w.split_once('=') else { continue; };
        match k {
            "dirty" => dirty = v.parse::<usize>().ok(),
            "total" => total = v.parse::<usize>().ok(),
            "uptodate" => uptodate = v.parse::<usize>().ok(),
            _ => {}
        }
    }
    Some((dirty?, total?, uptodate?))
}

fn compute_plan_scope(base: &Path, plan_all: &HashSet<String>, roots: &[String]) -> HashSet<String> {
    // Best-effort: parse `redo-ifchange` lines from generated .do scripts
    // to compute the closure of planned outputs reachable from roots.
    //
    // This is intentionally conservative:
    // - it only follows dependencies that are also in `plan_all`
    // - it ignores dynamic forms like `--from-file` or `$var` arguments
    //
    // If anything looks odd, fall back to the full plan (better stable than wrong-small).
    let mut root_keys: Vec<String> = Vec::new();
    for r in roots {
        let p = Path::new(r);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            base.join(p)
        };
        let k = normalize(abs);
        if plan_all.contains(&k) {
            root_keys.push(k);
        }
    }
    if root_keys.is_empty() {
        return plan_all.clone();
    }

    let mut scope: HashSet<String> = HashSet::new();
    let mut q: VecDeque<String> = VecDeque::new();
    for k in root_keys {
        if scope.insert(k.clone()) {
            q.push_back(k);
        }
    }

    // Guardrail: if the closure gets large, stop trying to be clever.
    // This keeps redo-log startup responsive for big builds.
    //
    // Note: we cap the walk well below `plan_all.len()` for large plans because
    // walking thousands of .do files can noticeably delay redo-log startup, and
    // in that regime using the full plan is usually "good enough".
    let max_scope = std::cmp::min(plan_all.len(), 4096usize);

    while let Some(k) = q.pop_front() {
        if scope.len() >= max_scope {
            return plan_all.clone();
        }
        let deps = deps_from_dofile(base, &k);
        for d in deps {
            let dk = if Path::new(&d).is_absolute() {
                normalize(PathBuf::from(&d))
            } else {
                normalize(base.join(&d))
            };
            if !plan_all.contains(&dk) {
                continue;
            }
            if scope.insert(dk.clone()) {
                q.push_back(dk);
            }
        }
    }
    scope
}

fn deps_from_dofile(base: &Path, output_key: &str) -> Vec<String> {
    let mut deps: Vec<String> = Vec::new();
    let do_path = PathBuf::from(format!("{}.do", output_key));
    let f = match File::open(&do_path) {
        Ok(f) => f,
        Err(_) => return deps,
    };
    let mut r = BufReader::new(f);
    let mut line = String::new();

    // Avoid parsing non-generated scripts too aggressively.
    // CMake's Redo generator emits a stable header line we can key off.
    let mut first = String::new();
    if r.read_line(&mut first).is_ok() {
        if !first.contains("Generated by CMake Redo generator") {
            return deps;
        }
    }

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
            // Only keep deps that look like paths; ignore obvious shell expansions.
            if d.contains('$') || d.contains('`') {
                continue;
            }
            // Strip a leading base prefix back to relative when possible.
            // This keeps the returned deps consistent with plan.targets entries.
            let p = Path::new(&d);
            if p.is_absolute() {
                if let Ok(rel) = p.strip_prefix(base) {
                    deps.push(rel.to_string_lossy().to_string());
                    continue;
                }
            }
            deps.push(d);
        }
    }
    deps
}

fn parse_redo_ifchange_deps(line: &str) -> Vec<String> {
    // Best-effort shell-ish word splitting for generated scripts.
    // Supports:
    // - single quotes: '...'
    // - double quotes: "..." with backslash escapes
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
            // skip the filename arg
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

fn load_plan_targets(base: &Path) -> Option<HashSet<String>> {
    let plan_path = base.join(".redo").join("plan.targets");
    let f = File::open(&plan_path).ok()?;
    let mut r = BufReader::new(f);
    let mut out: HashSet<String> = HashSet::new();
    let mut line = String::new();
    loop {
        line.clear();
        let n = r.read_line(&mut line).ok()?;
        if n == 0 {
            break;
        }
        let s = line
            .trim_end_matches(|c| c == '\n' || c == '\r')
            .trim();
        if s.is_empty() || s.starts_with('#') {
            continue;
        }
        let p = Path::new(s);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            base.join(p)
        };
        out.insert(normalize(abs));
    }
    Some(out)
}

fn normalize(p: PathBuf) -> String {
    // purely lexical normalization (no filesystem)
    let mut out: Vec<String> = Vec::new();
    let mut absolute = false;
    for c in p.components() {
        use std::path::Component;
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.is_empty() && out.last().map(|s| s.as_str()) != Some("..") {
                    out.pop();
                } else if !absolute {
                    out.push("..".to_string());
                }
            }
            Component::RootDir => {
                out.clear();
                absolute = true;
            }
            Component::Normal(s) => out.push(s.to_string_lossy().to_string()),
            Component::Prefix(_) => {}
        }
    }
    if absolute {
        format!("/{}", out.join("/"))
    } else {
        out.join("/")
    }
}
