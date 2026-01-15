use std::os::unix::io::RawFd;

use nix::fcntl::{fcntl, FcntlArg, FdFlag};

pub fn close_on_exec(fd: RawFd, yes: bool) -> anyhow::Result<()> {
    let mut flags = FdFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFD)?);
    if yes {
        flags.insert(FdFlag::FD_CLOEXEC);
    } else {
        flags.remove(FdFlag::FD_CLOEXEC);
    }
    fcntl(fd, FcntlArg::F_SETFD(flags))?;
    Ok(())
}

pub fn unlink_best_effort(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

#[derive(Debug)]
pub struct ImmediateReturn {
    pub rv: i32,
}
