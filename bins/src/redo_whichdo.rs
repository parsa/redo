use std::path::PathBuf;

use redo_core::logs::Log;
use redo_core::{env, paths::possible_do_files};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("{}: exactly one argument expected.", args[0]);
        std::process::exit(1);
    }
    let want = &args[1];
    if want.is_empty() {
        Log::err("cannot build the empty target (\"\").");
        std::process::exit(204);
    }

    // Init env, then enumerate possible `.do` files.
    if let Err(e) = env::init_no_state() {
        eprintln!("{:?}", e);
        std::process::exit(1);
    }
    let base = env::v().base;

    // Print paths relative to '.' for each candidate.
    // For now, we print the candidate dofile paths as computed, and exit 0
    // when the first existing one is found, else exit 1.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let abswant = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(want);
    let mut found = false;
    for df in possible_do_files(abswant.to_string_lossy().as_ref(), &base) {
        let dopath = df.dodir.join(&df.dofile);
        let rel = pathdiff::diff_paths(&dopath, &cwd).unwrap_or(dopath.clone());
        println!("{}", rel.to_string_lossy());
        if dopath.exists() {
            found = true;
            break;
        }
    }
    std::process::exit(if found { 0 } else { 1 });
}
