use std::io::Read;
use std::path::PathBuf;

use redo_core::{env, logs::Log, state};
use sha1::{Digest, Sha1};

fn current_target_abs() -> anyhow::Result<PathBuf> {
    let e = env::v();
    if e.startdir.is_empty() || e.target.is_empty() {
        anyhow::bail!("missing REDO_STARTDIR/REDO_TARGET");
    }
    Ok(PathBuf::from(e.startdir).join(PathBuf::from(e.pwd)).join(e.target))
}

fn main() {
    if let Err(e) = real_main() {
        Log::err(&format!("{:?}", e));
        std::process::exit(1);
    }
}

fn real_main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        eprintln!("{}: no arguments expected.", args[0]);
        std::process::exit(1);
    }
    unsafe {
        if libc::isatty(0) == 1 {
            eprintln!("{}: you must provide the data to stamp on stdin", args[0]);
            std::process::exit(1);
        }
    }

    env::inherit()?;
    state::db()?;

    // Read stdin and hash.
    let mut buf = Vec::new();
    std::io::stdin().read_to_end(&mut buf)?;
    let mut hasher = Sha1::new();
    hasher.update(&buf);
    let csum = format!("{:x}", hasher.finalize());

    // If not inside a target build, nothing to do.
    if env::v().target.is_empty() {
        return Ok(());
    }

    let me = current_target_abs()?;
    let mut f = state::File::by_name(me.to_string_lossy().as_ref(), true)?;

    let changed = f.csum.as_deref() != Some(&csum);
    f.is_generated = true;
    f.is_override = false;
    f.failed_runid = None;
    if changed {
        f.set_changed();
        f.csum = Some(csum);
    } else {
        f.set_checked();
    }
    f.save()?;
    state::commit()?;
    Ok(())
}
