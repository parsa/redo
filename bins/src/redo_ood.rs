use std::cell::RefCell;
use std::collections::HashSet;

use redo_core::{deps, state};

fn main() {
    if let Err(e) = real_main() {
        eprintln!("{:?}", e);
        std::process::exit(1);
    }
}

fn real_main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 1 {
        eprintln!("{}: no arguments expected.", args[0]);
        std::process::exit(1);
    }
    state::init(&[])?;

    let runid = redo_core::env::v().runid.unwrap_or(0);
    let cache: RefCell<HashSet<i64>> = RefCell::new(HashSet::new());
    let mut is_checked = |f: &state::File| cache.borrow().contains(&f.id);
    let mut set_checked = |f: &mut state::File| {
        cache.borrow_mut().insert(f.id);
        Ok(())
    };
    let mut log_override = |_name: &str| {};

    let cwd = std::env::current_dir()?;
    for mut f in state::files()? {
        if f.is_target()? {
            let dirty = deps::isdirty(&mut f, runid, &[], &mut is_checked, &mut set_checked, &mut log_override)?;
            if !matches!(dirty, deps::DirtyResult::Clean) {
                let p = redo_core::env::v().base.join(&f.name);
                let rel = pathdiff::diff_paths(p, &cwd).unwrap_or_else(|| f.name.clone().into());
                println!("{}", rel.to_string_lossy());
            }
        }
    }
    Ok(())
}
