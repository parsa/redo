//! GNU make-compatible jobserver.
//!
//! This implementation is intentionally low-level and uses raw file
//! descriptors, because jobserver fds must be inherited and shared across a
//! process tree.

use std::collections::HashMap;
use std::env;
use std::os::fd::BorrowedFd;
use std::os::unix::io::RawFd;

use nix::fcntl::{fcntl, FcntlArg};
use nix::sys::select::{select, FdSet};
use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::time::{TimeVal, TimeValLike};

use crate::logs::Log;
use crate::state;

static mut TOPLEVEL: i32 = 0;
static mut MYTOKENS: i32 = 1;
static mut CHEATS: i32 = 0;
static mut TOKENFDS: Option<(RawFd, RawFd)> = None;
static mut CHEATFDS: Option<(RawFd, RawFd)> = None;

struct Job {
    name: String,
    pid: libc::pid_t,
    done: Box<dyn FnMut(&str, i32) + Send>,
}

static WAITFDS: once_cell::sync::Lazy<std::sync::Mutex<HashMap<RawFd, Job>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(HashMap::new()));

fn fd_exists(fd: RawFd) -> bool {
    unsafe { libc::fcntl(fd, libc::F_GETFD) >= 0 }
}

fn make_pipe(startfd: RawFd) -> anyhow::Result<(RawFd, RawFd)> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(anyhow::anyhow!(std::io::Error::last_os_error()));
    }
    let a = fds[0];
    let b = fds[1];
    let ar = fcntl(a, FcntlArg::F_DUPFD(startfd))?;
    let bw = fcntl(b, FcntlArg::F_DUPFD(startfd + 1))?;
    unsafe {
        libc::close(a);
        libc::close(b);
    }
    Ok((ar, bw))
}

fn write_all(fd: RawFd, buf: &[u8]) -> anyhow::Result<()> {
    let mut off = 0;
    while off < buf.len() {
        let n = unsafe {
            libc::write(
                fd,
                buf[off..].as_ptr() as *const libc::c_void,
                (buf.len() - off) as libc::size_t,
            )
        };
        if n < 0 {
            return Err(anyhow::anyhow!(std::io::Error::last_os_error()));
        }
        off += n as usize;
    }
    Ok(())
}

fn create_tokens(n: i32) {
    unsafe {
        for _ in 0..n {
            if CHEATS > 0 {
                CHEATS -= 1;
            } else {
                MYTOKENS += 1;
            }
        }
    }
}

fn destroy_tokens(n: i32) {
    unsafe {
        MYTOKENS -= n;
        debug_assert!(MYTOKENS >= 0);
    }
}

fn release(n: i32) -> anyhow::Result<()> {
    unsafe {
        let mut to_share = 0;
        for _ in 0..n {
            MYTOKENS -= 1;
            if CHEATS > 0 {
                CHEATS -= 1;
            } else {
                to_share += 1;
            }
        }
        if to_share > 0 {
            let (_, w) = TOKENFDS.expect("tokenfds");
            write_all(w, &vec![b't'; to_share as usize])?;
        }
    }
    Ok(())
}

pub fn has_token() -> bool {
    unsafe { MYTOKENS >= 1 }
}

pub fn release_mine() -> anyhow::Result<()> {
    release(1)
}

fn release_except_mine() -> anyhow::Result<()> {
    unsafe {
        if MYTOKENS > 1 {
            release(MYTOKENS - 1)?;
        }
    }
    Ok(())
}

fn parse_makeflags() -> Option<(RawFd, RawFd)> {
    let flags = format!(" {} ", env::var("MAKEFLAGS").unwrap_or_default());
    let find1 = " --jobserver-auth=";
    let find2 = " --jobserver-fds=";
    let (find, ofs) = if let Some(ofs) = flags.find(find1) {
        (find1, ofs)
    } else if let Some(ofs) = flags.find(find2) {
        (find2, ofs)
    } else {
        return None;
    };
    let rest = &flags[ofs + find.len()..];
    let arg = rest.split(' ').next().unwrap_or("");
    let mut it = arg.split(',');
    let a = it.next()?.parse::<i32>().ok()? as RawFd;
    let b = it.next()?.parse::<i32>().ok()? as RawFd;
    if a <= 0 || b <= 0 {
        return None;
    }
    Some((a, b))
}

pub fn setup(maxjobs: i32) -> anyhow::Result<()> {
    if unsafe { TOKENFDS }.is_some() {
        return Ok(());
    }

    let inherited = parse_makeflags();
    let mut tokenfds: Option<(RawFd, RawFd)> = None;

    if let Some((a, b)) = inherited {
        if !fd_exists(a) || !fd_exists(b) {
            Log::err("broken --jobserver-auth from parent process");
            return Err(anyhow::anyhow!("broken jobserver fds"));
        }
        if maxjobs == 0 {
            tokenfds = Some((a, b));
        } else if maxjobs > 1 {
            Log::warn(&format!(
                "warning: -j{} forced in sub-redo; starting new jobserver.",
                maxjobs
            ));
        }
    }

    // Cheatfds are only meaningful when inheriting a parent jobserver.
    let cheatfds = if maxjobs != 0 {
        None
    } else {
        env::var("REDO_CHEATFDS").ok().and_then(|s| {
            let mut it = s.split(',');
            let a = it.next()?.parse::<i32>().ok()? as RawFd;
            let b = it.next()?.parse::<i32>().ok()? as RawFd;
            if a > 2 && b > 2 && fd_exists(a) && fd_exists(b) {
                Some((a, b))
            } else {
                None
            }
        })
    };

    let cheatfds = if let Some(cf) = cheatfds {
        cf
    } else {
        let cf = make_pipe(102)?;
        env::set_var("REDO_CHEATFDS", format!("{},{}", cf.0, cf.1));
        cf
    };

    unsafe {
        CHEATFDS = Some(cheatfds);
    }

    if tokenfds.is_none() {
        let realmax = if maxjobs > 0 { maxjobs } else { 1 };
        let tf = make_pipe(100)?;
        unsafe {
            TOPLEVEL = realmax;
            TOKENFDS = Some(tf);
        }
        create_tokens(realmax - 1);
        release_except_mine()?;
        env::set_var(
            "MAKEFLAGS",
            format!(
                " -j --jobserver-auth={},{} --jobserver-fds={},{}",
                tf.0, tf.1, tf.0, tf.1
            ),
        );
    } else {
        unsafe {
            TOKENFDS = tokenfds;
        }
    }

    Ok(())
}

extern "C" fn timeout_handler(_: i32) {}

fn set_alarm_10ms() -> anyhow::Result<SigAction> {
    let act = SigAction::new(
        SigHandler::Handler(timeout_handler),
        SaFlags::empty(),
        SigSet::empty(),
    );
    let old = unsafe { signal::sigaction(Signal::SIGALRM, &act)? };
    let mut it: libc::itimerval = unsafe { std::mem::zeroed() };
    it.it_value.tv_sec = 0;
    it.it_value.tv_usec = 10_000;
    it.it_interval.tv_sec = 0;
    it.it_interval.tv_usec = 10_000;
    if unsafe { libc::setitimer(libc::ITIMER_REAL, &it, std::ptr::null_mut()) } != 0 {
        return Err(anyhow::anyhow!(std::io::Error::last_os_error()));
    }
    Ok(old)
}

fn clear_alarm(old: &SigAction) {
    let it: libc::itimerval = unsafe { std::mem::zeroed() };
    let _ = unsafe { libc::setitimer(libc::ITIMER_REAL, &it, std::ptr::null_mut()) };
    let _ = unsafe { signal::sigaction(Signal::SIGALRM, old) };
}

fn try_read_token(fd: RawFd) -> anyhow::Result<Option<u8>> {
    let mut rfds = FdSet::new();
    let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
    rfds.insert(bfd);
    let mut tv = TimeVal::milliseconds(0);
    let _ = select(fd + 1, Some(&mut rfds), None, None, Some(&mut tv))?;
    if !rfds.contains(bfd) {
        return Ok(None);
    }

    let old = set_alarm_10ms()?;
    let mut byte = [0u8; 1];
    let n = unsafe { libc::read(fd, byte.as_mut_ptr() as *mut libc::c_void, 1) };
    clear_alarm(&old);

    if n == 0 {
        return Err(anyhow::anyhow!("unexpected EOF on token read"));
    }
    if n < 0 {
        let e = std::io::Error::last_os_error();
        if matches!(e.raw_os_error(), Some(libc::EINTR) | Some(libc::EAGAIN)) {
            return Ok(None);
        }
        return Err(anyhow::anyhow!(e));
    }
    Ok(Some(byte[0]))
}

fn wait(want_token: bool, max_delay_ms: Option<i64>) -> anyhow::Result<()> {
    let tokenfd = unsafe { TOKENFDS.expect("tokenfds").0 };
    let mut rfds = FdSet::new();
    let mut maxfd = tokenfd;
    if want_token {
        rfds.insert(unsafe { BorrowedFd::borrow_raw(tokenfd) });
    }
    {
        let waitfds = WAITFDS.lock().unwrap();
        for fd in waitfds.keys() {
            rfds.insert(unsafe { BorrowedFd::borrow_raw(*fd) });
            if *fd > maxfd {
                maxfd = *fd;
            }
        }
    }

    let mut tv = max_delay_ms.map(TimeVal::milliseconds);
    // On macOS, select() effectively only supports fds < FD_SETSIZE (typically 1024).
    // Use maxfd+1 (like the traditional select() contract) rather than an arbitrary constant.
    let nfds = maxfd + 1;
    let _ = select(nfds, Some(&mut rfds), None, None, tv.as_mut())?;

    let done_fds: Vec<RawFd> = {
        let waitfds = WAITFDS.lock().unwrap();
        waitfds
            .keys()
            .copied()
            .filter(|fd| rfds.contains(unsafe { BorrowedFd::borrow_raw(*fd) }))
            .collect()
    };

    for fd in done_fds {
        let mut waitfds = WAITFDS.lock().unwrap();
        if let Some(mut job) = waitfds.remove(&fd) {
            unsafe {
                libc::close(fd);
            }
            let mut status: libc::c_int = 0;
            let _ = unsafe { libc::waitpid(job.pid, &mut status as *mut _, 0) };
            let rv = if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else if libc::WIFSIGNALED(status) {
                -libc::WTERMSIG(status)
            } else {
                201
            };

            let b = unsafe {
                let cf = CHEATFDS.expect("cheatfds").0;
                try_read_token(cf).ok().flatten()
            };
            if b.is_none() {
                create_tokens(1);
                if has_token() {
                    let _ = release_except_mine();
                }
            }

            (job.done)(&job.name, rv);
        }
    }
    Ok(())
}

fn ensure_token(reason: &str, max_delay_ms: Option<i64>) -> anyhow::Result<()> {
    unsafe {
        while MYTOKENS < 1 {
            wait(true, max_delay_ms)?;
            if MYTOKENS >= 1 {
                break;
            }
            let (r, _) = TOKENFDS.expect("tokenfds");
            if let Some(_b) = try_read_token(r)? {
                MYTOKENS += 1;
                break;
            }
            if max_delay_ms.is_some() {
                break;
            }
        }
    }
    // silence unused for now
    let _ = reason;
    Ok(())
}

pub fn running() -> bool {
    !WAITFDS.lock().unwrap().is_empty()
}

pub fn ensure_token_or_cheat<F: Fn() -> i32>(reason: &str, cheatfunc: F) -> anyhow::Result<()> {
    let mut backoff_ms = 10;
    while !has_token() {
        while running() && !has_token() {
            ensure_token(reason, None)?;
        }
        ensure_token(reason, Some(backoff_ms))?;
        backoff_ms = std::cmp::min(backoff_ms * 2, 1000);
        if !has_token() {
            unsafe {
                if MYTOKENS != 0 {
                    continue;
                }
                let n = cheatfunc();
                if n > 0 {
                    MYTOKENS += n;
                    CHEATS += n;
                    break;
                }
            }
        }
    }
    Ok(())
}

pub fn start(
    reason: &str,
    jobfunc: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
    mut donefunc: impl FnMut(&str, i32) + Send + 'static,
) -> anyhow::Result<()> {
    if !state::is_flushed() {
        return Err(anyhow::anyhow!("state is not flushed"));
    }
    if !has_token() {
        return Err(anyhow::anyhow!("start() requires a token"));
    }
    unsafe {
        debug_assert!(MYTOKENS == 1);
    }

    destroy_tokens(1);
    let (r, w) = make_pipe(50)?;

    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // Child: keep the write end open so the parent can select() on the read
        // end and observe EOF when the job (and its descendants) exits.
        // Rely on pipe-EOF as the completion signal.
        unsafe {
            libc::close(r);
            // Ensure the write end is inheritable across exec.
            let _ = libc::fcntl(w, libc::F_SETFD, 0);
        }
        let rv = match jobfunc() {
            Ok(()) => 0,
            Err(_) => 201,
        };
        unsafe { libc::_exit(rv) };
    }
    if pid < 0 {
        return Err(anyhow::anyhow!(std::io::Error::last_os_error()));
    }
    unsafe {
        libc::close(w);
    }

    // Ensure wait fd isn't inherited.
    let _ = unsafe { libc::fcntl(r, libc::F_SETFD, libc::FD_CLOEXEC) };

    WAITFDS.lock().unwrap().insert(
        r,
        Job {
            name: reason.to_string(),
            pid,
            done: Box::new(move |n, rv| donefunc(n, rv)),
        },
    );
    Ok(())
}

pub fn wait_all() -> anyhow::Result<()> {
    while running() {
        unsafe {
            while MYTOKENS >= 2 {
                let _ = release(1);
            }
        }
        if unsafe { MYTOKENS >= 1 } {
            let _ = release_mine();
        }
        wait(false, None)?;
    }

    unsafe {
        if TOPLEVEL > 0 {
            if MYTOKENS >= 1 {
                let _ = release_mine();
            }
            let (r, w) = TOKENFDS.expect("tokenfds");
            let mut tokens = vec![];
            loop {
                match try_read_token(r) {
                    Ok(Some(b)) => tokens.push(b),
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            let mut cheats = vec![];
            let cf = CHEATFDS.expect("cheatfds").0;
            loop {
                match try_read_token(cf) {
                    Ok(Some(b)) => cheats.push(b),
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            if (tokens.len() as i32) - (cheats.len() as i32) != TOPLEVEL {
                return Err(anyhow::anyhow!("token accounting mismatch on exit"));
            }
            write_all(w, &tokens)?;
        }
    }

    Ok(())
}

pub fn force_return_tokens() -> anyhow::Result<()> {
    let n = WAITFDS.lock().unwrap().len() as i32;
    create_tokens(n);
    if has_token() {
        release_except_mine()?;
    }
    unsafe {
        if CHEATS > 0 {
            let wcf = CHEATFDS.expect("cheatfds").1;
            write_all(wcf, &vec![b't'; CHEATS as usize])?;
            destroy_tokens(CHEATS);
            CHEATS = 0;
        }
    }
    Ok(())
}

