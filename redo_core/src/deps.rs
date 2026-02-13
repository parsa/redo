//! Dependency dirtiness algorithm.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use crate::env;
use crate::state::{self, File};

pub const CLEAN: i32 = 0;
pub const DIRTY: i32 = 1;

/// Optional per-process cache of File IDs known clean for the current run.
///
/// This is populated by the toplevel `redo` preflight planner so the actual
/// build can avoid re-statting a large portion of the graph without having to
/// write `checked_runid` back to sqlite.
static PREFLIGHT_CHECKED: OnceLock<Arc<HashSet<i64>>> = OnceLock::new();

pub fn install_preflight_checked(checked: HashSet<i64>) {
    // Best-effort: ignore if already installed.
    let _ = PREFLIGHT_CHECKED.set(Arc::new(checked));
}

fn is_preflight_checked(fid: i64) -> bool {
    PREFLIGHT_CHECKED
        .get()
        .map(|s| s.contains(&fid))
        .unwrap_or(false)
}

#[derive(Debug)]
pub enum DirtyResult {
    Clean,
    Dirty,
    MustBuild(Vec<File>),
}

pub fn isdirty(
    f: &mut File,
    max_changed: i64,
    already_checked: &[i64],
    is_checked: &mut dyn FnMut(&File) -> bool,
    set_checked: &mut dyn FnMut(&mut File) -> anyhow::Result<()>,
    log_override: &mut dyn FnMut(&str),
) -> anyhow::Result<DirtyResult> {
    if already_checked.contains(&f.id) {
        return Err(anyhow::anyhow!(crate::cycles::CyclicDependencyError));
    }
    let mut checked = already_checked.to_vec();
    checked.push(f.id);

    if f.failed_runid.is_some() {
        return Ok(DirtyResult::Dirty);
    }
    if f.changed_runid.is_none() {
        return Ok(DirtyResult::Dirty);
    }
    if f.changed_runid.unwrap_or(0) > max_changed {
        return Ok(DirtyResult::Dirty);
    }
    if is_checked(f) {
        return Ok(DirtyResult::Clean);
    }
    if f.stamp.is_none() {
        return Ok(DirtyResult::Dirty);
    }

    let newstamp = f.read_stamp()?;
    if f.stamp.as_deref() != Some(&newstamp) {
        if newstamp == state::STAMP_MISSING {
            if f.stamp.is_some() && f.is_generated {
                // target vanished: convert target -> source (reduces override warnings)
                f.is_generated = false;
                f.failed_runid = None;
                f.save()?;
                f.refresh()?;
            }
        }
        if f.csum.is_some() {
            return Ok(DirtyResult::MustBuild(vec![f.clone()]));
        } else {
            return Ok(DirtyResult::Dirty);
        }
    }

    let mut must_build: Vec<File> = vec![];
    for (mode, mut f2) in f.deps()? {
        let dirty = match mode {
            'c' => {
                let path = env::v().base.join(&f2.name);
                if path.exists() {
                    DirtyResult::Dirty
                } else {
                    DirtyResult::Clean
                }
            }
            'm' => {
                let mx = std::cmp::max(f.changed_runid.unwrap_or(0), f.checked_runid.unwrap_or(0));
                isdirty(&mut f2, mx, &checked, is_checked, set_checked, log_override)?
            }
            _ => DirtyResult::Clean,
        };

        if f.csum.is_none() {
            match dirty {
                DirtyResult::Dirty => return Ok(DirtyResult::Dirty),
                DirtyResult::MustBuild(mut v) => must_build.append(&mut v),
                DirtyResult::Clean => {}
            }
        } else {
            match dirty {
                DirtyResult::Dirty => return Ok(DirtyResult::MustBuild(vec![f.clone()])),
                DirtyResult::MustBuild(mut v) => must_build.append(&mut v),
                DirtyResult::Clean => {}
            }
        }
    }

    if !must_build.is_empty() {
        return Ok(DirtyResult::MustBuild(must_build));
    }

    // clean
    if f.is_override {
        log_override(&f.name);
    }
    set_checked(f)?;
    Ok(DirtyResult::Clean)
}

pub fn isdirty_default(f: &mut File, max_changed: i64) -> anyhow::Result<DirtyResult> {
    let mut is_checked = |ff: &File| ff.is_checked() || is_preflight_checked(ff.id);
    let mut set_checked = |ff: &mut File| ff.set_checked_save();
    let mut log_override = |name: &str| state::warn_override(name);
    isdirty(
        f,
        max_changed,
        &[],
        &mut is_checked,
        &mut set_checked,
        &mut log_override,
    )
}

/// In-memory cache for read-only dirtiness planning.
///
/// This is used by `redo` to estimate how many *planned outputs* are dirty
/// without writing `checked_runid` updates back to sqlite.
#[derive(Default)]
pub struct PlanCache {
    checked: HashSet<i64>,
    stamp: HashMap<i64, String>,
    deps: HashMap<i64, Vec<(char, File)>>,
}

impl PlanCache {
    pub fn checked_ids(&self) -> &HashSet<i64> {
        &self.checked
    }

    pub fn take_checked_ids(&mut self) -> HashSet<i64> {
        std::mem::take(&mut self.checked)
    }

    fn is_checked(&self, f: &File) -> bool {
        self.checked.contains(&f.id)
    }

    fn set_checked(&mut self, f: &File) {
        self.checked.insert(f.id);
    }

    fn read_stamp_cached(&mut self, f: &File) -> anyhow::Result<&str> {
        if !self.stamp.contains_key(&f.id) {
            let s = f.read_stamp()?;
            self.stamp.insert(f.id, s);
        }
        Ok(self.stamp.get(&f.id).map(|s| s.as_str()).unwrap_or(""))
    }

    fn deps_cached(&mut self, f: &File) -> anyhow::Result<Vec<(char, File)>> {
        if let Some(v) = self.deps.get(&f.id) {
            return Ok(v.clone());
        }
        let v = f.deps()?;
        self.deps.insert(f.id, v.clone());
        Ok(v)
    }
}

pub fn isdirty_readonly_default(
    f: &mut File,
    max_changed: i64,
    cache: &mut PlanCache,
) -> anyhow::Result<DirtyResult> {
    let mut noop_override = |_name: &str| {};
    isdirty_readonly(
        f,
        max_changed,
        &[],
        cache,
        &mut noop_override,
    )
}

pub fn isdirty_readonly(
    f: &mut File,
    max_changed: i64,
    already_checked: &[i64],
    cache: &mut PlanCache,
    log_override: &mut dyn FnMut(&str),
) -> anyhow::Result<DirtyResult> {
    // Mirrors `isdirty`, but uses `PlanCache` instead of sqlite writes.
    if already_checked.contains(&f.id) {
        return Err(anyhow::anyhow!(crate::cycles::CyclicDependencyError));
    }
    let mut checked = already_checked.to_vec();
    checked.push(f.id);

    if f.failed_runid.is_some() {
        return Ok(DirtyResult::Dirty);
    }
    if f.changed_runid.is_none() {
        return Ok(DirtyResult::Dirty);
    }
    if f.changed_runid.unwrap_or(0) > max_changed {
        return Ok(DirtyResult::Dirty);
    }
    if cache.is_checked(f) {
        return Ok(DirtyResult::Clean);
    }
    if f.stamp.is_none() {
        return Ok(DirtyResult::Dirty);
    }

    let newstamp = cache.read_stamp_cached(f)?;
    if f.stamp.as_deref() != Some(newstamp) {
        // Read-only: do *not* convert vanished targets back into sources here.
        if f.csum.is_some() {
            return Ok(DirtyResult::MustBuild(vec![f.clone()]));
        } else {
            return Ok(DirtyResult::Dirty);
        }
    }

    let mut must_build: Vec<File> = vec![];
    for (mode, mut f2) in cache.deps_cached(f)? {
        let dirty = match mode {
            'c' => {
                let path = env::v().base.join(&f2.name);
                if path.exists() {
                    DirtyResult::Dirty
                } else {
                    DirtyResult::Clean
                }
            }
            'm' => {
                let mx = std::cmp::max(f.changed_runid.unwrap_or(0), f.checked_runid.unwrap_or(0));
                isdirty_readonly(&mut f2, mx, &checked, cache, log_override)?
            }
            _ => DirtyResult::Clean,
        };

        if f.csum.is_none() {
            match dirty {
                DirtyResult::Dirty => return Ok(DirtyResult::Dirty),
                DirtyResult::MustBuild(mut v) => must_build.append(&mut v),
                DirtyResult::Clean => {}
            }
        } else {
            match dirty {
                DirtyResult::Dirty => return Ok(DirtyResult::MustBuild(vec![f.clone()])),
                DirtyResult::MustBuild(mut v) => must_build.append(&mut v),
                DirtyResult::Clean => {}
            }
        }
    }

    if !must_build.is_empty() {
        return Ok(DirtyResult::MustBuild(must_build));
    }

    // clean
    if f.is_override {
        log_override(&f.name);
    }
    cache.set_checked(f);
    Ok(DirtyResult::Clean)
}
