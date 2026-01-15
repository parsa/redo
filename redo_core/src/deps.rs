//! Dependency dirtiness algorithm.

use crate::env;
use crate::state::{self, File};

pub const CLEAN: i32 = 0;
pub const DIRTY: i32 = 1;

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
    let mut is_checked = |ff: &File| ff.is_checked();
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

