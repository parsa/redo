use std::collections::HashSet;
use std::env;

#[derive(Debug, thiserror::Error)]
#[error("cyclic dependency detected")]
pub struct CyclicDependencyError;

fn get() -> HashSet<String> {
    env::var("REDO_CYCLES")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

pub fn add(fid: i64) {
    let mut items = get();
    items.insert(fid.to_string());
    env::set_var(
        "REDO_CYCLES",
        items.into_iter().collect::<Vec<_>>().join(":"),
    );
}

pub fn check(fid: i64) -> Result<(), CyclicDependencyError> {
    if get().contains(&fid.to_string()) {
        Err(CyclicDependencyError)
    } else {
        Ok(())
    }
}
