use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

fn usage() -> &'static str {
    "redo-trace --trace-out0 <path> --mode read -- <argv...>\n"
}

fn env_flag_set(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => {
            let s = v.trim();
            !s.is_empty() && s != "0"
        }
        Err(_) => false,
    }
}

fn env_keep_list() -> HashSet<String> {
    let mut keep: HashSet<String> = HashSet::new();
    keep.insert("REDO".to_string());
    keep.insert("MAKEFLAGS".to_string());
    keep.insert("REDO_CHEATFDS".to_string());
    for k in [
        "PATH",
        "TMPDIR",
        "TMP",
        "TEMP",
        "LANG",
        "LC_ALL",
        "TERM",
    ] {
        keep.insert(k.to_string());
    }

    // User escape hatch.
    if let Ok(v) = std::env::var("REDO_STRICT_ENV_ALLOW") {
        for part in v.split(',') {
            let k = part.trim();
            if !k.is_empty() {
                keep.insert(k.to_string());
            }
        }
    }

    keep
}

fn build_scrubbed_env() -> HashMap<OsString, OsString> {
    let keep = env_keep_list();
    let mut out: HashMap<OsString, OsString> = HashMap::new();
    for (k, v) in std::env::vars_os() {
        if let Some(ks) = k.to_str() {
            if ks.starts_with("REDO_") || keep.contains(ks) {
                out.insert(k, v);
            }
        }
    }

    // Default HOME isolation: unless explicitly allowed, point HOME at a private empty dir.
    // This reduces hidden inputs via ~/.config, ~/.cmake, etc.
    if !out.contains_key(&OsString::from("HOME")) {
        let home_dir = std::env::var_os("REDO_BASE")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(".redo")
            .join("strict-home")
            .join(format!("{}", std::process::id()));
        let _ = std::fs::create_dir_all(&home_dir);
        out.insert(OsString::from("HOME"), home_dir.into_os_string());
    }
    out
}

fn write_trace_out0(path: &Path, entries: &[String]) -> anyhow::Result<()> {
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let mut buf: Vec<u8> = Vec::new();
    for e in entries {
        buf.extend_from_slice(e.as_bytes());
        buf.push(0);
    }
    std::fs::write(path, buf)?;
    Ok(())
}

fn status_code(st: ExitStatus) -> i32 {
    if let Some(c) = st.code() {
        c
    } else {
        // Signaled: match common shell convention.
        201
    }
}

fn run_env_scrubbed(argv: &[String], envp: &HashMap<OsString, OsString>) -> anyhow::Result<i32> {
    let mut cmd = Command::new(&argv[0]);
    if argv.len() > 1 {
        cmd.args(&argv[1..]);
    }
    cmd.env_clear();
    for (k, v) in envp.iter() {
        cmd.env(k, v);
    }
    let st = cmd.status()?;
    Ok(status_code(st))
}

fn trace_unavailable_marker(msg: &str) -> Vec<String> {
    vec![format!("TRACE_UNAVAILABLE:{msg}")]
}

#[cfg(target_os = "linux")]
fn trace_run_linux(
    argv: &[String],
    envp: &HashMap<OsString, OsString>,
) -> anyhow::Result<(Vec<String>, i32)> {
    #[cfg(target_arch = "x86_64")]
    {
        return trace_run_linux_x86_64(argv, envp);
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let rv = match run_env_scrubbed(argv, envp) {
            Ok(rv) => rv,
            Err(e) => {
                eprintln!("redo-trace: exec failed: {:?}", e);
                201
            }
        };
        return Ok((trace_unavailable_marker("linux_unsupported_arch"), rv));
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn trace_run_linux_x86_64(
    argv: &[String],
    envp: &HashMap<OsString, OsString>,
) -> anyhow::Result<(Vec<String>, i32)> {
    use std::io;
    use std::os::unix::process::CommandExt;

    #[derive(Debug, Clone)]
    struct ProcState {
        cwd: PathBuf,
        in_syscall: bool,
    }

    fn wif_exited(st: i32) -> bool {
        (st & 0x7f) == 0
    }
    fn wexit_status(st: i32) -> i32 {
        (st >> 8) & 0xff
    }
    fn wif_signaled(st: i32) -> bool {
        (st & 0x7f) != 0 && (st & 0x7f) != 0x7f
    }
    fn wterm_sig(st: i32) -> i32 {
        st & 0x7f
    }
    fn wif_stopped(st: i32) -> bool {
        (st & 0xff) == 0x7f
    }
    fn wstop_sig(st: i32) -> i32 {
        (st >> 8) & 0xff
    }
    fn wstop_event(st: i32) -> i32 {
        (st >> 16) & 0xffff
    }

    fn normalize_path_lex(p: &Path) -> PathBuf {
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

    fn resolve_path(cwd: &Path, raw: &str) -> PathBuf {
        let p = Path::new(raw);
        if p.is_absolute() {
            normalize_path_lex(p)
        } else {
            normalize_path_lex(&cwd.join(p))
        }
    }

    unsafe fn ptrace_geteventmsg(pid: libc::pid_t) -> io::Result<u64> {
        let mut msg: u64 = 0;
        let r = libc::ptrace(
            libc::PTRACE_GETEVENTMSG,
            pid,
            std::ptr::null_mut(),
            (&mut msg as *mut u64) as *mut libc::c_void,
        );
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(msg)
    }

    unsafe fn errno_get() -> i32 {
        *libc::__errno_location()
    }
    unsafe fn errno_set(v: i32) {
        *libc::__errno_location() = v;
    }

    unsafe fn ptrace_read_cstring(pid: libc::pid_t, addr: u64, max: usize) -> io::Result<String> {
        if addr == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "null ptr"));
        }
        let mut out: Vec<u8> = Vec::new();
        let word_size = std::mem::size_of::<libc::c_long>();
        let mut off: usize = 0;
        while out.len() < max {
            errno_set(0);
            let w = libc::ptrace(
                libc::PTRACE_PEEKDATA,
                pid,
                (addr as usize + off) as *mut libc::c_void,
                std::ptr::null_mut(),
            );
            let e = errno_get();
            if e != 0 {
                return Err(io::Error::from_raw_os_error(e));
            }
            let bytes = (w as u64).to_ne_bytes();
            for b in bytes.iter().take(word_size) {
                if *b == 0 {
                    return Ok(String::from_utf8_lossy(&out).to_string());
                }
                out.push(*b);
                if out.len() >= max {
                    break;
                }
            }
            off += word_size;
        }
        Ok(String::from_utf8_lossy(&out).to_string())
    }

    unsafe fn ptrace_getregs(pid: libc::pid_t) -> io::Result<libc::user_regs_struct> {
        let mut regs: libc::user_regs_struct = std::mem::zeroed();
        let r = libc::ptrace(
            libc::PTRACE_GETREGS,
            pid,
            std::ptr::null_mut(),
            (&mut regs as *mut libc::user_regs_struct) as *mut libc::c_void,
        );
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(regs)
    }

    unsafe fn ptrace_setoptions(pid: libc::pid_t, opts: u64) -> io::Result<()> {
        let r = libc::ptrace(
            libc::PTRACE_SETOPTIONS,
            pid,
            std::ptr::null_mut(),
            opts as *mut libc::c_void,
        );
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    unsafe fn ptrace_syscall(pid: libc::pid_t, sig: i32) -> io::Result<()> {
        let r = libc::ptrace(
            libc::PTRACE_SYSCALL,
            pid,
            std::ptr::null_mut(),
            (sig as isize) as *mut libc::c_void,
        );
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    unsafe fn ptrace_cont(pid: libc::pid_t, sig: i32) -> io::Result<()> {
        let r = libc::ptrace(
            libc::PTRACE_CONT,
            pid,
            std::ptr::null_mut(),
            (sig as isize) as *mut libc::c_void,
        );
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    let cwd0 = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));

    let mut cmd = Command::new(&argv[0]);
    if argv.len() > 1 {
        cmd.args(&argv[1..]);
    }
    cmd.env_clear();
    for (k, v) in envp.iter() {
        cmd.env(k, v);
    }
    unsafe {
        cmd.pre_exec(|| {
            if libc::ptrace(
                libc::PTRACE_TRACEME,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            libc::kill(libc::getpid(), libc::SIGSTOP);
            Ok(())
        });
    }

    let child = cmd.spawn()?;
    let root_pid = child.id() as libc::pid_t;

    // Wait for initial SIGSTOP.
    let mut st0: libc::c_int = 0;
    loop {
        let r = unsafe { libc::waitpid(root_pid, &mut st0 as *mut _, 0) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            let rv = 201;
            return Ok((trace_unavailable_marker(&format!("linux_waitpid:{e}")), rv));
        }
        break;
    }

    let opts: u64 = (libc::PTRACE_O_TRACESYSGOOD
        | libc::PTRACE_O_TRACEFORK
        | libc::PTRACE_O_TRACEVFORK
        | libc::PTRACE_O_TRACECLONE
        | libc::PTRACE_O_TRACEEXEC
        | libc::PTRACE_O_EXITKILL) as u64;

    let mut trace_markers: HashSet<String> = HashSet::new();
    let mut observed: HashSet<String> = HashSet::new();
    let mut procs: HashMap<libc::pid_t, ProcState> = HashMap::new();
    procs.insert(
        root_pid,
        ProcState {
            cwd: cwd0.clone(),
            in_syscall: false,
        },
    );

    unsafe {
        if let Err(e) = ptrace_setoptions(root_pid, opts) {
            trace_markers.insert(format!("TRACE_UNAVAILABLE:linux_setoptions:{e}"));
            let _ = ptrace_cont(root_pid, 0);
        } else {
            let _ = ptrace_syscall(root_pid, 0);
        }
    }

    let mut root_exit: Option<i32> = None;
    while !procs.is_empty() {
        let mut st: libc::c_int = 0;
        let pid = unsafe { libc::waitpid(-1, &mut st as *mut _, 0) };
        if pid < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            trace_markers.insert(format!("TRACE_ERROR:waitpid:{e}"));
            break;
        }

        let st_i = st as i32;
        if wif_exited(st_i) {
            let code = wexit_status(st_i);
            if pid == root_pid {
                root_exit = Some(code);
            }
            procs.remove(&pid);
            continue;
        }
        if wif_signaled(st_i) {
            let sig = wterm_sig(st_i);
            if pid == root_pid {
                root_exit = Some(128 + sig);
            }
            procs.remove(&pid);
            continue;
        }
        if !wif_stopped(st_i) {
            continue;
        }

        let sig = wstop_sig(st_i);
        let event = wstop_event(st_i);

        // Ptrace events.
        if sig == libc::SIGTRAP && event != 0 {
            match event {
                libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK | libc::PTRACE_EVENT_CLONE => {
                    unsafe {
                        if let Ok(msg) = ptrace_geteventmsg(pid) {
                            let new_pid = msg as libc::pid_t;
                            if let Some(ps) = procs.get(&pid).cloned() {
                                procs.insert(
                                    new_pid,
                                    ProcState {
                                        cwd: ps.cwd,
                                        in_syscall: false,
                                    },
                                );
                            }
                            let _ = ptrace_setoptions(new_pid, opts);
                        }
                        let _ = ptrace_syscall(pid, 0);
                    }
                    continue;
                }
                libc::PTRACE_EVENT_EXEC => {
                    if let Some(ps) = procs.get_mut(&pid) {
                        ps.in_syscall = false;
                    }
                    unsafe {
                        let _ = ptrace_syscall(pid, 0);
                    }
                    continue;
                }
                _ => {
                    unsafe {
                        let _ = ptrace_syscall(pid, 0);
                    }
                    continue;
                }
            }
        }

        // Syscall stop (TRACESYSGOOD adds 0x80).
        if sig == (libc::SIGTRAP | 0x80) {
            let entering = procs.get(&pid).map(|ps| !ps.in_syscall).unwrap_or(true);
            if let Some(ps) = procs.get_mut(&pid) {
                ps.in_syscall = entering;
            }
            if entering {
                unsafe {
                    match ptrace_getregs(pid) {
                        Ok(regs) => {
                            let scno = regs.orig_rax as i64;
                            let a0 = regs.rdi as u64;
                            let a1 = regs.rsi as u64;
                            let a2 = regs.rdx as u64;
                            let a3 = regs.r10 as u64;

                            let cwd = procs
                                .get(&pid)
                                .map(|ps| ps.cwd.clone())
                                .unwrap_or_else(|| cwd0.clone());

                            if scno == libc::SYS_chdir as i64 {
                                if let Ok(s) = ptrace_read_cstring(pid, a0, 4096) {
                                    let new = resolve_path(&cwd, &s);
                                    if let Some(ps) = procs.get_mut(&pid) {
                                        ps.cwd = new;
                                    }
                                }
                            } else if scno == libc::SYS_execve as i64 {
                                if let Ok(s) = ptrace_read_cstring(pid, a0, 4096) {
                                    let p = resolve_path(&cwd, &s);
                                    observed.insert(p.to_string_lossy().to_string());
                                }
                            } else if scno == libc::SYS_open as i64 {
                                let flags = a1 as i32;
                                let accmode = flags & libc::O_ACCMODE;
                                if accmode != libc::O_WRONLY {
                                    if let Ok(s) = ptrace_read_cstring(pid, a0, 4096) {
                                        let p = resolve_path(&cwd, &s);
                                        observed.insert(p.to_string_lossy().to_string());
                                    }
                                }
                            } else if scno == libc::SYS_openat as i64 {
                                let dirfd = a0 as i64;
                                let flags = a2 as i32;
                                let accmode = flags & libc::O_ACCMODE;
                                if accmode != libc::O_WRONLY {
                                    if dirfd != libc::AT_FDCWD as i64 {
                                        trace_markers.insert("UNRESOLVED:openat_dirfd".to_string());
                                    } else if let Ok(s) = ptrace_read_cstring(pid, a1, 4096) {
                                        let p = resolve_path(&cwd, &s);
                                        observed.insert(p.to_string_lossy().to_string());
                                    }
                                }
                            } else if scno == libc::SYS_access as i64 {
                                if let Ok(s) = ptrace_read_cstring(pid, a0, 4096) {
                                    let p = resolve_path(&cwd, &s);
                                    observed.insert(p.to_string_lossy().to_string());
                                }
                            } else if scno == libc::SYS_newfstatat as i64 {
                                let dirfd = a0 as i64;
                                if dirfd != libc::AT_FDCWD as i64 {
                                    trace_markers.insert("UNRESOLVED:newfstatat_dirfd".to_string());
                                } else if let Ok(s) = ptrace_read_cstring(pid, a1, 4096) {
                                    let p = resolve_path(&cwd, &s);
                                    observed.insert(p.to_string_lossy().to_string());
                                }
                            } else if scno == libc::SYS_faccessat as i64 {
                                let dirfd = a0 as i64;
                                if dirfd != libc::AT_FDCWD as i64 {
                                    trace_markers.insert("UNRESOLVED:faccessat_dirfd".to_string());
                                } else if let Ok(s) = ptrace_read_cstring(pid, a1, 4096) {
                                    let p = resolve_path(&cwd, &s);
                                    observed.insert(p.to_string_lossy().to_string());
                                }
                            } else if scno == libc::SYS_stat as i64 {
                                if let Ok(s) = ptrace_read_cstring(pid, a0, 4096) {
                                    let p = resolve_path(&cwd, &s);
                                    observed.insert(p.to_string_lossy().to_string());
                                }
                            } else if scno == libc::SYS_lstat as i64 {
                                if let Ok(s) = ptrace_read_cstring(pid, a0, 4096) {
                                    let p = resolve_path(&cwd, &s);
                                    observed.insert(p.to_string_lossy().to_string());
                                }
                            } else if scno == libc::SYS_fstat as i64 {
                                // Ignore: no path.
                                let _ = a0;
                            } else if scno == libc::SYS_statx as i64 {
                                let dirfd = a0 as i64;
                                if dirfd != libc::AT_FDCWD as i64 {
                                    trace_markers.insert("UNRESOLVED:statx_dirfd".to_string());
                                } else if let Ok(s) = ptrace_read_cstring(pid, a1, 4096) {
                                    let p = resolve_path(&cwd, &s);
                                    observed.insert(p.to_string_lossy().to_string());
                                }
                            } else {
                                let _ = a3;
                            }
                        }
                        Err(e) => {
                            trace_markers.insert(format!("TRACE_ERROR:getregs:{e}"));
                        }
                    }
                }
            }
            unsafe {
                let _ = ptrace_syscall(pid, 0);
            }
            continue;
        }

        // Other stops: keep going. Avoid re-delivering SIGSTOP/SIGTRAP which can stall the trace.
        let deliver = if sig == libc::SIGSTOP || sig == libc::SIGTRAP {
            0
        } else {
            sig
        };
        unsafe {
            let _ = ptrace_syscall(pid, deliver);
        }
    }

    let rv = root_exit.unwrap_or(201);
    let mut out: Vec<String> = Vec::new();
    out.extend(trace_markers.into_iter());
    out.extend(observed.into_iter());
    out.sort();
    Ok((out, rv))
}

#[cfg(target_os = "macos")]
fn trace_run_macos(
    argv: &[String],
    envp: &HashMap<OsString, OsString>,
) -> anyhow::Result<(Vec<String>, i32)> {
    // Best-effort tracer using `dtruss` when available/allowed. When unavailable,
    // run normally and emit a clear sentinel marker.
    use std::collections::HashMap as StdHashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn normalize_path_lex(p: &Path) -> PathBuf {
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

    fn resolve_path(cwd: &Path, raw: &str) -> PathBuf {
        let p = Path::new(raw);
        if p.is_absolute() {
            normalize_path_lex(p)
        } else {
            normalize_path_lex(&cwd.join(p))
        }
    }

    fn extract_first_quoted(s: &str) -> Option<String> {
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            if c == '"' {
                let mut out = String::new();
                while let Some(cc) = it.next() {
                    if cc == '"' {
                        return Some(out);
                    }
                    if cc == '\\' {
                        if let Some(n) = it.next() {
                            out.push(n);
                        }
                        continue;
                    }
                    out.push(cc);
                }
                return None;
            }
        }
        None
    }

    // `dtruss` generally requires elevated privileges and may be restricted by SIP.
    // Only attempt it when running as root; otherwise, run the command untraced.
    if unsafe { libc::geteuid() } != 0 {
        let rv = run_env_scrubbed(argv, envp).unwrap_or_else(|_| 201);
        return Ok((trace_unavailable_marker("macos_dtruss_requires_root"), rv));
    }

    let cwd0 = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let out_path = std::env::temp_dir().join(format!("redo-trace.{}.{}.dtruss", std::process::id(), ts));

    let mut dtr = Command::new("dtruss");
    dtr.arg("-f");
    dtr.arg("-o").arg(out_path.to_string_lossy().to_string());
    dtr.arg("-t").arg("open,open_nocancel,openat,openat_nocancel,stat64,lstat64,access,execve,chdir");
    dtr.arg(&argv[0]);
    if argv.len() > 1 {
        dtr.args(&argv[1..]);
    }
    dtr.env_clear();
    for (k, v) in envp.iter() {
        dtr.env(k, v);
    }

    let st = match dtr.status() {
        Ok(st) => st,
        Err(_e) => {
            let rv = run_env_scrubbed(argv, envp).unwrap_or_else(|_| 201);
            return Ok((trace_unavailable_marker("macos_dtruss_not_found"), rv));
        }
    };
    let rv = status_code(st);

    let bytes = std::fs::read(&out_path).unwrap_or_default();
    let _ = std::fs::remove_file(&out_path);
    let s = String::from_utf8_lossy(&bytes).to_string();

    // If dtruss produced no usable output, treat as unavailable.
    if s.trim().is_empty()
        || s.contains("dtrace:") && (s.contains("Operation not permitted") || s.contains("System Integrity Protection") || s.contains("restricted"))
    {
        return Ok((trace_unavailable_marker("macos_dtruss_unavailable"), rv));
    }

    let mut cwd_by_pid: StdHashMap<String, PathBuf> = StdHashMap::new();
    let mut observed: HashSet<String> = HashSet::new();
    let mut markers: HashSet<String> = HashSet::new();

    for line in s.lines() {
        // pid prefix shape: "<pid>/<tid>: ..."
        let pid = line
            .split('/')
            .next()
            .unwrap_or("")
            .trim()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>();
        let cwd = cwd_by_pid.get(&pid).cloned().unwrap_or_else(|| cwd0.clone());

        let is_openat = line.contains("openat(") || line.contains("openat_nocancel(");
        if is_openat && !line.contains("AT_FDCWD") {
            markers.insert("UNRESOLVED:openat_dirfd".to_string());
        }

        if line.contains("chdir(") {
            if let Some(p) = extract_first_quoted(line) {
                let new = resolve_path(&cwd, &p);
                if !pid.is_empty() {
                    cwd_by_pid.insert(pid.clone(), new);
                }
            }
            continue;
        }

        let interesting = line.contains("open(")
            || line.contains("open_nocancel(")
            || line.contains("openat(")
            || line.contains("openat_nocancel(")
            || line.contains("stat64(")
            || line.contains("lstat64(")
            || line.contains("access(")
            || line.contains("execve(");
        if !interesting {
            continue;
        }
        if let Some(p) = extract_first_quoted(line) {
            let abs = resolve_path(&cwd, &p);
            observed.insert(abs.to_string_lossy().to_string());
        }
    }

    if observed.is_empty() {
        return Ok((trace_unavailable_marker("macos_dtruss_empty"), rv));
    }

    let mut out: Vec<String> = Vec::new();
    out.extend(markers.into_iter());
    out.extend(observed.into_iter());
    out.sort();
    Ok((out, rv))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn trace_run_other(
    argv: &[String],
    envp: &HashMap<OsString, OsString>,
) -> anyhow::Result<(Vec<String>, i32)> {
    let rv = match run_env_scrubbed(argv, envp) {
        Ok(rv) => rv,
        Err(_e) => 201,
    };
    Ok((trace_unavailable_marker("unsupported_platform"), rv))
}

fn main() {
    if env_flag_set("REDO_TRACE_DEBUG") {
        eprintln!("redo-trace: starting (pid={})", std::process::id());
    }

    let mut trace_out0: Option<PathBuf> = None;
    let mut mode: String = "read".to_string();
    let mut argv: Vec<String> = Vec::new();

    let mut it = std::env::args().skip(1).peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--trace-out0" => {
                trace_out0 = Some(PathBuf::from(it.next().unwrap_or_default()));
            }
            "--mode" => {
                mode = it.next().unwrap_or_else(|| "read".to_string());
            }
            "--" => {
                argv.extend(it.map(|s| s.to_string()));
                break;
            }
            "--help" | "-h" => {
                eprintln!("{}", usage());
                std::process::exit(0);
            }
            _ => {}
        }
    }

    let Some(trace_out0) = trace_out0 else {
        eprintln!("{}", usage());
        std::process::exit(2);
    };
    if argv.is_empty() {
        eprintln!("{}", usage());
        std::process::exit(2);
    }
    if mode != "read" {
        eprintln!("redo-trace: unsupported mode {:?}", mode);
        eprintln!("{}", usage());
        std::process::exit(2);
    }

    let envp = build_scrubbed_env();

    // Trace (best-effort) and execute the command.
    let (observed, rv) = {
        #[cfg(target_os = "linux")]
        {
            trace_run_linux(&argv, &envp).unwrap_or_else(|e| {
                let rv = run_env_scrubbed(&argv, &envp).unwrap_or_else(|_| 201);
                (trace_unavailable_marker(&format!("linux_error:{e}")), rv)
            })
        }
        #[cfg(target_os = "macos")]
        {
            trace_run_macos(&argv, &envp).unwrap_or_else(|e| {
                let rv = run_env_scrubbed(&argv, &envp).unwrap_or_else(|_| 201);
                (trace_unavailable_marker(&format!("macos_error:{e}")), rv)
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            trace_run_other(&argv, &envp).unwrap_or_else(|e| {
                let rv = run_env_scrubbed(&argv, &envp).unwrap_or_else(|_| 201);
                (trace_unavailable_marker(&format!("other_error:{e}")), rv)
            })
        }
    };

    // Always write trace output, even if the command fails; the engine uses this to decide cacheability.
    let _ = write_trace_out0(&trace_out0, &observed);
    std::process::exit(rv);
}

