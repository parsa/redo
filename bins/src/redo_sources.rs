use redo_core::{env, state};

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

    let cwd = std::env::current_dir()?;
    for f in state::files()? {
        if f.is_source()? {
            let p = env::v().base.join(&f.name);
            let rel = pathdiff::diff_paths(p, &cwd).unwrap_or_else(|| f.name.into());
            println!("{}", rel.to_string_lossy());
        }
    }
    Ok(())
}
