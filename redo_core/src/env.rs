use std::env;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

// REDO_UNLOCKED and REDO_NO_OOB are latched and then cleared from the environment
// so they aren't inherited by subprocesses. Because `v()` reads from the
// environment (not a global cached struct), we remember these flags in-process.
static UNLOCKED_LATCH: AtomicBool = AtomicBool::new(false);
static NO_OOB_LATCH: AtomicBool = AtomicBool::new(false);

/// Environment snapshot.
#[derive(Debug, Clone)]
pub struct Env {
    pub base: PathBuf,
    pub pwd: String,
    pub target: String,
    pub depth: String,
    pub debug: i32,
    pub debug_locks: bool,
    pub debug_pids: bool,
    pub locks_broken: bool,
    pub verbose: i32,
    pub xtrace: i32,
    pub keep_going: bool,
    pub log: i32,
    pub log_inode: String,
    pub color: i32,
    pub pretty: i32,
    pub shuffle: bool,
    pub startdir: String,
    pub runid: Option<i64>,
    pub unlocked: bool,
    pub no_oob: bool,
}

static IS_TOPLEVEL: AtomicBool = AtomicBool::new(false);

fn atoi(s: &str) -> i32 {
    s.parse().unwrap_or(0)
}

fn get_bool(name: &str) -> bool {
    !env::var(name).unwrap_or_default().is_empty()
}

impl Env {
    pub fn from_env() -> anyhow::Result<Self> {
        let base = env::var("REDO_BASE").unwrap_or_else(|_| "NOT_DEFINED".to_string());
        Ok(Self {
            base: PathBuf::from(base.trim_end_matches('/')),
            pwd: env::var("REDO_PWD").unwrap_or_default(),
            target: env::var("REDO_TARGET").unwrap_or_default(),
            depth: env::var("REDO_DEPTH").unwrap_or_default(),
            debug: atoi(&env::var("REDO_DEBUG").unwrap_or_default()),
            debug_locks: get_bool("REDO_DEBUG_LOCKS"),
            debug_pids: get_bool("REDO_DEBUG_PIDS"),
            locks_broken: get_bool("REDO_LOCKS_BROKEN"),
            verbose: atoi(&env::var("REDO_VERBOSE").unwrap_or_default()),
            xtrace: atoi(&env::var("REDO_XTRACE").unwrap_or_default()),
            keep_going: get_bool("REDO_KEEP_GOING"),
            // Default is enabled (REDO_LOG defaults to "1").
            log: atoi(&env::var("REDO_LOG").unwrap_or_else(|_| "1".to_string())),
            log_inode: env::var("REDO_LOG_INODE").unwrap_or_default(),
            color: atoi(&env::var("REDO_COLOR").unwrap_or_default()),
            pretty: atoi(&env::var("REDO_PRETTY").unwrap_or_default()),
            shuffle: get_bool("REDO_SHUFFLE"),
            startdir: env::var("REDO_STARTDIR").unwrap_or_default(),
            runid: env::var("REDO_RUNID").ok().and_then(|s| s.parse().ok()),
            unlocked: get_bool("REDO_UNLOCKED"),
            no_oob: get_bool("REDO_NO_OOB"),
        })
    }

    pub fn inherit() -> anyhow::Result<Self> {
        if env::var("REDO").is_err() {
            anyhow::bail!("must be run from inside a .do");
        }
        Self::from_env()
    }
}

pub fn is_toplevel() -> bool {
    IS_TOPLEVEL.load(Ordering::Relaxed)
}

/// Read the current environment snapshot (cheap and avoids stale caching).
pub fn v() -> Env {
    let mut e = Env::from_env().expect("env not available");
    // If the vars were cleared after an earlier inherit/init, preserve their
    // values via the latches.
    if !e.unlocked && UNLOCKED_LATCH.load(Ordering::Relaxed) {
        e.unlocked = true;
    }
    if !e.no_oob && NO_OOB_LATCH.load(Ordering::Relaxed) {
        e.no_oob = true;
    }
    e
}

pub fn inherit() -> anyhow::Result<()> {
    let e = Env::inherit()?;
    // Latch flags before clearing env vars, and never unlatch (process lifetime).
    if e.unlocked {
        UNLOCKED_LATCH.store(true, Ordering::Relaxed);
    }
    if e.no_oob {
        NO_OOB_LATCH.store(true, Ordering::Relaxed);
    }
    // not inheritable by subprocesses
    env::set_var("REDO_UNLOCKED", "");
    env::set_var("REDO_NO_OOB", "");
    Ok(())
}

pub fn init_no_state() -> anyhow::Result<()> {
    if env::var("REDO").is_err() {
        IS_TOPLEVEL.store(true, Ordering::Relaxed);
        env::set_var("REDO", "NOT_DEFINED");
    }
    if env::var("REDO_BASE").is_err() {
        env::set_var("REDO_BASE", "NOT_DEFINED");
    }
    inherit()
}

pub fn init(targets: &[String]) -> anyhow::Result<()> {
    if env::var("REDO").is_err() {
        IS_TOPLEVEL.store(true, Ordering::Relaxed);
        // Use current_exe() to find the real executable location so we can
        // prepend its sibling helper dirs to PATH without accidentally
        // prepending the current working directory when invoked via PATH.
        let exe = std::env::current_exe().unwrap_or_else(|_| {
            // Fallback: best-effort, may be relative.
            PathBuf::from(std::env::args_os().next().unwrap_or_default())
        });
        let real_exe = std::fs::canonicalize(&exe).unwrap_or_else(|_| exe.clone());

        let mut trynames: Vec<PathBuf> = Vec::new();
        for e in [exe.clone(), real_exe] {
            if let Some(d) = e.parent() {
                trynames.push(d.join("../lib/redo"));
                trynames.push(d.join("../redo"));
                trynames.push(d.to_path_buf());
            }
        }

        // De-dup while preserving order, then prepend to PATH.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut dirs: Vec<String> = Vec::new();
        for p in trynames {
            let s = p.to_string_lossy().to_string();
            if seen.insert(s.clone()) {
                dirs.push(s);
            }
        }
        // Allow tests (and power users) to override PATH ordering. When set,
        // we do not prepend our helper dirs, so an injected `redo-log` can be
        // found before the real one.
        let no_prepend = !env::var("REDO_NO_PATH_PREPEND").unwrap_or_default().is_empty();
        if !no_prepend && !dirs.is_empty() {
            let old = env::var("PATH").unwrap_or_default();
            if old.is_empty() {
                env::set_var("PATH", dirs.join(":"));
            } else {
                env::set_var("PATH", format!("{}:{}", dirs.join(":"), old));
            }
        }
        env::set_var("REDO", exe.to_string_lossy().to_string());
    }

    if env::var("REDO_BASE").is_err() {
        let base = find_base(targets)?;
        env::set_var("REDO_BASE", base.to_string_lossy().to_string());
        env::set_var(
            "REDO_STARTDIR",
            env::current_dir()?.to_string_lossy().to_string(),
        );
    }
    inherit()
}

pub fn mark_locks_broken() -> anyhow::Result<()> {
    env::set_var("REDO_LOCKS_BROKEN", "1");
    // Safety: redo-log doesn't work when locks are broken.
    env::set_var("REDO_LOG", "0");
    inherit()
}

fn find_base(targets: &[String]) -> anyhow::Result<PathBuf> {
    // Base selection:
    //   base = commonprefix([abspath(dirname(t)) for t in targets] + [cwd])
    //   then walk up from `base` looking for an existing `.redo/` directory.
    let cwd = env::current_dir()?;

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

    fn posix_dirname(p: &str) -> String {
        // Rough equivalent of posix dirname(), sufficient for redo targets.
        if p.is_empty() {
            return String::new();
        }
        let i = p.rfind('/').map(|pos| pos + 1).unwrap_or(0);
        let mut head = &p[..i];
        if !head.is_empty() {
            let all_slashes = head.bytes().all(|b| b == b'/');
            if !all_slashes {
                head = head.trim_end_matches('/');
            }
        }
        head.to_string()
    }

    fn abspath(p: &str, cwd: &Path) -> PathBuf {
        // Equivalent to: normpath(join(cwd, p)) with special-case p="".
        if p.is_empty() {
            return cwd.to_path_buf();
        }
        let pp = Path::new(p);
        let joined = if pp.is_absolute() {
            PathBuf::from(pp)
        } else {
            cwd.join(pp)
        };
        normalize_path(&joined)
    }

    fn commonprefix(strings: &[String]) -> String {
        // Lexical commonprefix: compare min/max strings lexicographically.
        if strings.is_empty() {
            return String::new();
        }
        let s1 = strings.iter().min().unwrap();
        let s2 = strings.iter().max().unwrap();
        let mut nbytes = 0usize;
        for (c1, c2) in s1.chars().zip(s2.chars()) {
            if c1 != c2 {
                break;
            }
            nbytes += c1.len_utf8();
        }
        s1[..nbytes].to_string()
    }

    let mut dirs: Vec<String> = Vec::new();
    let use_targets: Vec<&str> = if targets.is_empty() {
        // Use ["all"] here, which collapses to cwd when taking dirname+abspath.
        vec!["all"]
    } else {
        targets.iter().map(|s| s.as_str()).collect()
    };
    for t in use_targets {
        let d = posix_dirname(t);
        let absd = abspath(&d, &cwd);
        dirs.push(absd.to_string_lossy().to_string());
    }
    dirs.push(cwd.to_string_lossy().to_string());

    let mut base = commonprefix(&dirs);

    // Walk up looking for an existing .redo (check base's ancestors, not base itself).
    let parts: Vec<&str> = base.split('/').collect();
    for i in (1..parts.len()).rev() {
        let newbase = parts[..i].join("/");
        if Path::new(&(newbase.clone() + "/.redo")).exists() {
            base = newbase;
            break;
        }
    }

    Ok(PathBuf::from(base))
}
