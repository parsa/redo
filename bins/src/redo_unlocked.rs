use redo_core::{env, logs::Log};

fn main() {
    if let Err(e) = real_main() {
        Log::err(&format!("{:?}", e));
        std::process::exit(1);
    }
}

fn real_main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("{}: at least 2 arguments expected.", args[0]);
        std::process::exit(1);
    }

    env::inherit()?;

    let target = args[1].clone();
    let deps: Vec<String> = args[2..]
        .iter()
        .filter(|d| *d != &target)
        .cloned()
        .collect();

    // First build deps with locks.
    std::env::set_var("REDO_NO_OOB", "1");
    let status = std::process::Command::new("redo-ifchange")
        .args(&deps)
        .status()?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    // Then build the primary target without acquiring lock (caller already holds it).
    std::env::set_var("REDO_UNLOCKED", "1");
    let status = std::process::Command::new("redo-ifchange")
        .arg(&target)
        .status()?;
    std::process::exit(status.code().unwrap_or(1));
}
