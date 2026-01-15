use std::ffi::CString;

use redo_core::builder;
use redo_core::{env, helpers, state};
use redo_core::logs;
use redo_core::version::TAG;

use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};

static GOT_SIGINT: AtomicBool = AtomicBool::new(false);

extern "C" fn sigint_handler(_: libc::c_int) {
    GOT_SIGINT.store(true, Ordering::SeqCst);
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
    unsafe {
        libc::setpgid(0, 0);
        libc::signal(libc::SIGINT, sigint_handler as usize);
    }
    std::thread::spawn(|| {
        while !GOT_SIGINT.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        unsafe {
            // Ignore SIGINT in this process, then deliver it to the process group.
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            libc::kill(0, libc::SIGINT);
            libc::_exit(200);
        }
    });

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
    let _ = state::init(&targets);

    // When fcntl locks are broken (eg. WSL), disable parallelism and redo-log for safety.
    if (env::is_toplevel() || jobs > 1) && env::v().locks_broken {
        eprintln!("redo: detected broken fcntl locks; parallelism disabled.");
        eprintln!("redo:   ...details: https://github.com/Microsoft/WSL/issues/1927");
        if jobs > 1 {
            jobs = 1;
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

    // If SIGINT was seen, don't let main race the SIGINT watcher thread.
    // The watcher is responsible for propagating SIGINT to the whole process group
    // and exiting with status code 200.
    if GOT_SIGINT.load(Ordering::SeqCst) {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    std::process::exit(rv);
}
