use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct DoFile {
    pub dodir: PathBuf,
    pub dofile: String,
    pub basedir: String,
    pub basename: String,
    pub ext: String,
}

fn default_do_files(filename: &str) -> Vec<(String, String, String)> {
    let parts: Vec<&str> = filename.split('.').collect();
    let mut out = Vec::new();
    for i in 1..=parts.len() {
        let basename = parts[..i].join(".");
        let ext = if i < parts.len() {
            format!(".{}", parts[i..].join("."))
        } else {
            String::new()
        };
        let dofile = format!("default{}.do", ext);
        out.push((dofile, basename, ext));
    }
    out
}

/// Enumerate candidate `.do` files for a target (ordering matters).
pub fn possible_do_files(target_rel: &str, base: &Path) -> Vec<DoFile> {
    let mut out = Vec::new();

    // 1) <dir>/<filename>.do
    let (dir, filename) = match target_rel.rsplit_once('/') {
        Some((d, f)) => (d.to_string(), f.to_string()),
        None => ("".to_string(), target_rel.to_string()),
    };
    out.push(DoFile {
        dodir: base.join(&dir),
        dofile: format!("{}.do", filename),
        basedir: "".to_string(),
        basename: filename.clone(),
        ext: "".to_string(),
    });

    // 2) default*.do walk up (uses normpath(join(base, t)))
    let t = normalize_join(base, target_rel);
    let dirname = t.parent().unwrap_or(base);
    let filename = t.file_name().and_then(|s| s.to_str()).unwrap_or(target_rel);
    let dirname_s = dirname.to_string_lossy().to_string();
    let dirbits: Vec<&str> = dirname_s.split('/').collect();
    for i in (1..=dirbits.len()).rev() {
        let basedir_str = dirbits[..i].join("/");
        let basedir = PathBuf::from(if basedir_str.is_empty() {
            "/".to_string()
        } else {
            basedir_str
        });
        let subdir = dirbits[i..].join("/");
        for (dofile, basename, ext) in default_do_files(filename) {
            let base_join = if subdir.is_empty() {
                basename.clone()
            } else {
                format!("{}/{}", subdir, basename)
            };
            out.push(DoFile {
                dodir: basedir.clone(),
                dofile,
                basedir: subdir.clone(),
                basename: base_join,
                ext,
            });
        }
    }

    out
}

pub fn find_do_file(target_rel: &str, base: &Path) -> Option<DoFile> {
    for cand in possible_do_files(target_rel, base) {
        if cand.dodir.join(&cand.dofile).exists() {
            return Some(cand);
        }
    }
    None
}

fn normalize_join(base: &Path, rel: &str) -> PathBuf {
    // We implement a simple, deterministic version of `os.path.normpath(base/rel)`.
    // We intentionally do NOT resolve symlinks here.
    let joined = base.join(rel);
    let mut parts: Vec<String> = Vec::new();
    for comp in joined.components() {
        use std::path::Component;
        match comp {
            Component::Prefix(_) => {} // not expected on unix
            Component::RootDir => {
                parts.clear();
                parts.push(String::new()); // leading "" gives us a leading '/'
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.len() > 1 {
                    parts.pop();
                }
            }
            Component::Normal(s) => parts.push(s.to_string_lossy().to_string()),
        }
    }
    let s = if parts.is_empty() {
        "/".to_string()
    } else if parts[0].is_empty() {
        format!("/{}", parts[1..].join("/"))
    } else {
        parts.join("/")
    };
    PathBuf::from(s)
}
