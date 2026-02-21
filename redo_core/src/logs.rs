use std::io::{self, Write};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::env;

#[derive(Clone, Copy)]
struct Ansi {
    red: &'static str,
    green: &'static str,
    yellow: &'static str,
    bold: &'static str,
    plain: &'static str,
}

#[derive(Clone, Copy)]
enum Mode {
    Raw,
    Pretty,
}

#[derive(Clone, Copy)]
struct Config {
    mode: Mode,
    ansi: Ansi,
}

static CONFIG: std::sync::OnceLock<Mutex<Config>> = std::sync::OnceLock::new();

fn isatty(fd: i32) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

fn ansi_for(color: i32) -> Ansi {
    // color: 0=off, 1=auto, 2+=force
    let term_ok = std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true);
    let ok = (color >= 2) || (color == 1 && isatty(2) && term_ok);
    if ok {
        Ansi {
            red: "\x1b[31m",
            green: "\x1b[32m",
            yellow: "\x1b[33m",
            bold: "\x1b[1m",
            plain: "\x1b[m",
        }
    } else {
        Ansi {
            red: "",
            green: "",
            yellow: "",
            bold: "",
            plain: "",
        }
    }
}

/// Log mode selection/formatting.
///
/// - If `pretty != 0` and `parent_logs == false`, we emit human-readable logs.
/// - Otherwise, we emit raw `@@REDO:` meta-lines suitable for `redo-log`.
pub fn setup(parent_logs: bool, pretty: i32, color: i32) {
    let mode = if pretty != 0 && !parent_logs {
        Mode::Pretty
    } else {
        Mode::Raw
    };
    let cfg = Config {
        mode,
        ansi: ansi_for(color),
    };
    let _ = CONFIG.set(Mutex::new(cfg));
    if let Some(m) = CONFIG.get() {
        *m.lock().unwrap() = cfg;
    }
}

fn cfg() -> Config {
    *CONFIG
        .get_or_init(|| {
            Mutex::new(Config {
                mode: Mode::Raw,
                ansi: ansi_for(0),
            })
        })
        .lock()
        .unwrap()
}

fn pretty_write(pid: u32, color: &str, s: &str) {
    let e = env::v();
    let cfg = cfg();
    let ansi = cfg.ansi;
    let redo = if e.debug_pids {
        format!("{:<6} redo  ", pid)
    } else {
        "redo  ".to_string()
    };
    let _ = writeln!(
        io::stderr(),
        "{}{}{}{}{}{}",
        color,
        redo,
        e.depth,
        if color.is_empty() { "" } else { ansi.bold },
        s,
        ansi.plain
    );
}

/// Minimal logger with `@@REDO:` meta-line format.
pub struct Log;

impl Log {
    pub fn meta(kind: &str, msg: &str, pid: Option<u32>) {
        let pid = pid.unwrap_or_else(std::process::id);
        let msg = msg.trim_end();
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        let raw = format!("@@REDO:{}:{}:{:.4}@@ {}", kind, pid, ts, msg);
        match cfg().mode {
            Mode::Raw => {
                let _ = writeln!(io::stderr(), "{}", raw);
            }
            Mode::Pretty => {
                // Pretty formatting subset.
                match kind {
                    "unchanged" => {
                        if env::v().log != 0 || env::v().debug >= 1 {
                            pretty_write(pid, "", &format!("{} (unchanged)", msg));
                        }
                    }
                    "check" => pretty_write(pid, cfg().ansi.green, &format!("({})", msg)),
                    "do" => pretty_write(pid, cfg().ansi.green, msg),
                    "done" => {
                        if let Some((rv, name)) = msg.split_once(' ') {
                            let rv_i = rv.parse::<i32>().unwrap_or(0);
                            if rv_i != 0 {
                                pretty_write(pid, cfg().ansi.red, &format!("{} (exit {})", name, rv_i));
                            } else if env::v().verbose > 0 || env::v().xtrace > 0 || env::v().debug >= 1 {
                                pretty_write(pid, cfg().ansi.green, &format!("{} (done)", name));
                                let _ = writeln!(io::stderr());
                            }
                        } else {
                            pretty_write(pid, cfg().ansi.green, msg);
                        }
                    }
                    "locked" | "waiting" | "unlocked" => {
                        if env::v().debug_locks {
                            pretty_write(pid, cfg().ansi.green, msg);
                        }
                    }
                    "cache_hit" | "cache_miss" | "cache_store" | "cache_skip" | "cache_stats" => {
                        // Keep normal output clean; only show cache details in debug mode.
                        if env::v().debug >= 1 {
                            pretty_write(pid, "", &format!("{} {}", kind, msg));
                        }
                    }
                    "error" => {
                        let cfg = cfg();
                        let _ = writeln!(
                            io::stderr(),
                            "{}redo: {}{}{}",
                            cfg.ansi.red, cfg.ansi.bold, msg, cfg.ansi.plain
                        );
                    }
                    "warning" => {
                        let cfg = cfg();
                        let _ = writeln!(
                            io::stderr(),
                            "{}redo: {}{}{}",
                            cfg.ansi.yellow, cfg.ansi.bold, msg, cfg.ansi.plain
                        );
                    }
                    "debug" => pretty_write(pid, "", msg),
                    _ => {
                        // Fallback: emit raw.
                        let _ = writeln!(io::stderr(), "{}", raw);
                    }
                }
            }
        }
        let _ = io::stderr().flush();
    }

    pub fn err(msg: &str) {
        Self::meta("error", msg, None);
    }

    pub fn warn(msg: &str) {
        Self::meta("warning", msg, None);
    }

    pub fn cache_hit(target: &str, keyprefix: &str, bytes: u64) {
        Self::meta("cache_hit", &format!("{} {} {}", target, keyprefix, bytes), None);
    }

    pub fn cache_miss(target: &str, reason: &str) {
        Self::meta("cache_miss", &format!("{} {}", target, reason), None);
    }

    pub fn cache_store(target: &str, keyprefix: &str, bytes: u64) {
        Self::meta("cache_store", &format!("{} {} {}", target, keyprefix, bytes), None);
    }

    pub fn cache_skip(target: &str, reason: &str) {
        Self::meta("cache_skip", &format!("{} {}", target, reason), None);
    }

    pub fn cache_stats(msg: &str) {
        Self::meta("cache_stats", msg, None);
    }

    pub fn debug(msg: &str) {
        if env::v().debug >= 1 {
            Self::meta("debug", msg, None);
        }
    }

    pub fn debug2(msg: &str) {
        if env::v().debug >= 2 {
            Self::meta("debug", msg, None);
        }
    }

    pub fn debug3(msg: &str) {
        if env::v().debug >= 3 {
            Self::meta("debug", msg, None);
        }
    }
}
