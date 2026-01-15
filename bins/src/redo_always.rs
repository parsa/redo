use std::path::PathBuf;

use redo_core::{env, logs::Log, state};

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
    env::inherit()?;
    state::db()?;

    let me = current_target_abs()?;
    let f = state::File::by_name(me.to_string_lossy().as_ref(), true)?;
    f.add_dep('m', state::ALWAYS)?;
    f.save()?;

    let mut always = state::File::by_name(state::ALWAYS, true)?;
    always.stamp = Some(state::STAMP_MISSING.to_string());
    always.is_generated = false;
    always.set_changed();
    always.save()?;

    state::commit()?;
    Ok(())
}
