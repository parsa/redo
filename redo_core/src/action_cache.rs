//! Local action cache (Phase 1, restricted).
//!
//! This is a best-effort content store keyed by an ActionKey. It is intentionally
//! simple: store a single output file's bytes + minimal metadata and retrieve it
//! later to avoid rerunning the producing `.do` script.

use std::cmp::Reverse;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};

use crate::{env, state, version};

const DEFAULT_MAX_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB

fn env_flag_set(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => false,
    }
}

pub fn enabled_default_on() -> bool {
    !env_flag_set("REDO_NO_ACTION_CACHE")
}

pub fn max_bytes() -> u64 {
    match std::env::var("REDO_ACTION_CACHE_MAX_BYTES") {
        Ok(v) if !v.trim().is_empty() => v.trim().parse::<u64>().unwrap_or(DEFAULT_MAX_BYTES),
        _ => DEFAULT_MAX_BYTES,
    }
}

fn root_dir() -> PathBuf {
    env::v().base.join(".redo").join("action_cache")
}

fn objects_dir() -> PathBuf {
    root_dir().join("objects")
}

fn bucket_dir(key_hex: &str) -> PathBuf {
    let aa = key_hex.get(0..2).unwrap_or("00");
    objects_dir().join(aa)
}

fn blob_path(key_hex: &str) -> PathBuf {
    bucket_dir(key_hex).join(format!("{}.blob", key_hex))
}

fn meta_path(key_hex: &str) -> PathBuf {
    bucket_dir(key_hex).join(format!("{}.meta", key_hex))
}

fn fsync_path_best_effort(p: &Path) {
    if let Ok(f) = fs::File::open(p) {
        let _ = f.sync_all();
    }
}

fn durable_rename_best_effort(tmp: &Path, target: &Path) -> anyhow::Result<()> {
    fsync_path_best_effort(tmp);
    fs::rename(tmp, target)?;
    if let Some(d) = tmp.parent() {
        fsync_path_best_effort(d);
    }
    if let Some(d) = target.parent() {
        fsync_path_best_effort(d);
    }
    Ok(())
}

fn tmp_path_near(target: &Path, suffix: &str) -> PathBuf {
    let mut p = target.to_path_buf();
    let fname = target.file_name().unwrap_or_default().to_string_lossy();
    p.set_file_name(format!(
        ".{}.{}.{}",
        fname,
        std::process::id(),
        suffix
    ));
    p
}

#[derive(Debug, Clone)]
pub struct ActionKey {
    pub hex: String,
}

impl ActionKey {
    pub fn prefix(&self) -> &str {
        self.hex.get(0..12).unwrap_or(&self.hex)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eligibility {
    Eligible,
    SkipAlways,
}

fn hash_str(h: &mut Sha256, s: &str) {
    let n = s.as_bytes().len() as u64;
    h.update(n.to_le_bytes());
    h.update(s.as_bytes());
}

pub fn compute_action_key_v0(
    target_name: &str,
    deps: &mut [(char, state::File)],
) -> anyhow::Result<(Eligibility, ActionKey)> {
    deps.sort_by(|(ma, fa), (mb, fb)| (ma, &fa.name).cmp(&(mb, &fb.name)));

    let mut h = Sha256::new();
    hash_str(&mut h, "redo-action-key-v0");
    hash_str(&mut h, version::TAG);
    hash_str(&mut h, target_name);
    h.update((deps.len() as u64).to_le_bytes());

    for (mode, dep) in deps.iter() {
        if dep.name == state::ALWAYS {
            return Ok((
                Eligibility::SkipAlways,
                ActionKey {
                    hex: String::new(),
                },
            ));
        }
        h.update([*mode as u8]);
        hash_str(&mut h, &dep.name);
        let stamp = dep.read_stamp()?;
        hash_str(&mut h, &stamp);
    }

    let digest = h.finalize();
    Ok((
        Eligibility::Eligible,
        ActionKey {
            hex: format!("{:x}", digest),
        },
    ))
}

pub fn compute_action_key_v0_policy(
    policy: &str,
    target_name: &str,
    deps: &mut [(char, state::File)],
) -> anyhow::Result<(Eligibility, ActionKey)> {
    deps.sort_by(|(ma, fa), (mb, fb)| (ma, &fa.name).cmp(&(mb, &fb.name)));

    let mut h = Sha256::new();
    hash_str(&mut h, "redo-action-key-v0");
    hash_str(&mut h, &format!("policy={}", policy));
    hash_str(&mut h, version::TAG);
    hash_str(&mut h, target_name);
    h.update((deps.len() as u64).to_le_bytes());

    for (mode, dep) in deps.iter() {
        if dep.name == state::ALWAYS {
            return Ok((
                Eligibility::SkipAlways,
                ActionKey {
                    hex: String::new(),
                },
            ));
        }
        h.update([*mode as u8]);
        hash_str(&mut h, &dep.name);
        let stamp = dep.read_stamp()?;
        hash_str(&mut h, &stamp);
    }

    let digest = h.finalize();
    Ok((
        Eligibility::Eligible,
        ActionKey {
            hex: format!("{:x}", digest),
        },
    ))
}

#[derive(Debug, Clone)]
pub struct CachedMeta {
    pub mode: u32,
    pub size: u64,
    pub sha256: String,
}

fn write_meta_atomically(path: &Path, meta: &CachedMeta) -> anyhow::Result<()> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let tmp = tmp_path_near(path, "meta.tmp");
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        writeln!(f, "mode={}", meta.mode)?;
        writeln!(f, "size={}", meta.size)?;
        writeln!(f, "sha256={}", meta.sha256)?;
        f.sync_all()?;
    }
    match durable_rename_best_effort(&tmp, path) {
        Ok(()) => Ok(()),
        Err(_e) if path.exists() => {
            let _ = fs::remove_file(&tmp);
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn copy_file_hash_sha256(src: &Path, dest: &Path) -> anyhow::Result<(u64, String)> {
    let mut r = fs::File::open(src)?;
    let mut w = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)?;

    let mut h = Sha256::new();
    let mut total: u64 = 0;
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n])?;
        h.update(&buf[..n]);
        total = total.saturating_add(n as u64);
    }
    w.sync_all()?;
    Ok((total, format!("{:x}", h.finalize())))
}

pub fn sha256_file_hex(path: &Path) -> anyhow::Result<String> {
    let mut r = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

fn write_blob_from_file_atomically(dest: &Path, src: &Path) -> anyhow::Result<(u64, String)> {
    if let Some(d) = dest.parent() {
        fs::create_dir_all(d)?;
    }
    let tmp = tmp_path_near(dest, "blob.tmp");
    let (bytes, sha256) = copy_file_hash_sha256(src, &tmp)?;
    match durable_rename_best_effort(&tmp, dest) {
        Ok(()) => Ok((bytes, sha256)),
        Err(_e) if dest.exists() => {
            // Another process won the race; treat as success.
            let _ = fs::remove_file(&tmp);
            Ok((bytes, sha256))
        }
        Err(e) => Err(e),
    }
}

pub fn lookup(key: &ActionKey) -> Option<(PathBuf, CachedMeta)> {
    if key.hex.is_empty() {
        return None;
    }
    let bp = blob_path(&key.hex);
    let mp = meta_path(&key.hex);
    if !bp.exists() || !mp.exists() {
        return None;
    }
    let meta_s = fs::read_to_string(&mp).ok()?;
    let mut mode: Option<u32> = None;
    let mut size: Option<u64> = None;
    let mut sha256: Option<String> = None;
    for line in meta_s.lines() {
        if let Some(v) = line.strip_prefix("mode=") {
            mode = v.trim().parse::<u32>().ok();
        } else if let Some(v) = line.strip_prefix("size=") {
            size = v.trim().parse::<u64>().ok();
        } else if let Some(v) = line.strip_prefix("sha256=") {
            let vv = v.trim();
            if !vv.is_empty() {
                sha256 = Some(vv.to_string());
            }
        }
    }
    Some((
        bp,
        CachedMeta {
            mode: mode.unwrap_or(0o644),
            size: size.unwrap_or(0),
            sha256: sha256.unwrap_or_default(),
        },
    ))
}

pub fn materialize(blob: &Path, meta: &CachedMeta, dest: &Path) -> anyhow::Result<u64> {
    let bytes = fs::copy(blob, dest)?;
    let perm = fs::Permissions::from_mode(meta.mode);
    let _ = fs::set_permissions(dest, perm);
    if let Ok(f) = fs::File::open(dest) {
        let _ = f.sync_all();
    }
    Ok(bytes)
}

pub fn store(key: &ActionKey, src: &Path, mode: u32) -> anyhow::Result<u64> {
    if key.hex.is_empty() {
        anyhow::bail!("empty action key");
    }
    let bp = blob_path(&key.hex);
    let mp = meta_path(&key.hex);
    if bp.exists() && mp.exists() {
        return Ok(fs::metadata(&bp).map(|m| m.len()).unwrap_or(0));
    }

    let st = fs::metadata(src)?;
    let size = st.len();
    let (wrote, sha256) = write_blob_from_file_atomically(&bp, src)?;
    let _ = write_meta_atomically(
        &mp,
        &CachedMeta {
            mode,
            size,
            sha256,
        },
    );

    // Best-effort GC.
    let _ = maybe_gc();
    Ok(wrote)
}

pub fn compute_action_key_v1_remote(
    target_name: &str,
    deps: &mut [(char, state::File)],
) -> anyhow::Result<(Eligibility, ActionKey)> {
    // Portable key intended for remote artifact caches:
    // - hash dependency *contents* (sha256) rather than machine-local stamps
    // - include OS/ARCH so different platforms do not collide by default
    deps.sort_by(|(ma, fa), (mb, fb)| (ma, &fa.name).cmp(&(mb, &fb.name)));

    let mut h = Sha256::new();
    hash_str(&mut h, "redo-action-key-v1-remote");
    hash_str(&mut h, version::TAG);
    hash_str(&mut h, std::env::consts::OS);
    hash_str(&mut h, std::env::consts::ARCH);
    hash_str(&mut h, target_name);
    h.update((deps.len() as u64).to_le_bytes());

    for (mode, dep) in deps.iter() {
        if dep.name == state::ALWAYS {
            return Ok((
                Eligibility::SkipAlways,
                ActionKey {
                    hex: String::new(),
                },
            ));
        }
        h.update([*mode as u8]);
        hash_str(&mut h, &dep.name);

        let p = env::v().base.join(&dep.name);
        let meta = match fs::symlink_metadata(&p) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(anyhow::anyhow!(e)),
        };

        match meta {
            None => {
                hash_str(&mut h, "MISSING");
            }
            Some(m) if m.is_dir() => {
                // Match redo's directory stamp behavior: directory existence only.
                hash_str(&mut h, "DIR");
            }
            Some(m) if m.file_type().is_symlink() => {
                // Match redo's symlink stamp spirit: include link target and the referent state.
                hash_str(&mut h, "SYMLINK");
                let link = fs::read_link(&p).ok();
                if let Some(l) = link {
                    hash_str(&mut h, &l.to_string_lossy());
                } else {
                    hash_str(&mut h, "READLINK_FAILED");
                }

                // Follow and hash referent if it's a regular file.
                let ref_meta = match fs::metadata(&p) {
                    Ok(m) => Some(m),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => return Err(anyhow::anyhow!(e)),
                };
                match ref_meta {
                    None => hash_str(&mut h, "REF_MISSING"),
                    Some(rm) if rm.is_dir() => hash_str(&mut h, "REF_DIR"),
                    Some(rm) if rm.is_file() => {
                        hash_str(&mut h, "REF_FILE");
                        let d = sha256_file_hex(&p)?;
                        hash_str(&mut h, &d);
                        // Include executable bit (cheap) to reduce accidental collisions.
                        let mode_bits = rm.permissions().mode() & 0o777;
                        h.update((mode_bits as u64).to_le_bytes());
                    }
                    Some(_) => hash_str(&mut h, "REF_OTHER"),
                }
            }
            Some(m) if m.is_file() => {
                hash_str(&mut h, "FILE");
                let d = sha256_file_hex(&p)?;
                hash_str(&mut h, &d);
                let mode_bits = m.permissions().mode() & 0o777;
                h.update((mode_bits as u64).to_le_bytes());
            }
            Some(_) => {
                // FIFOs/devices/etc: avoid caching actions that depend on these.
                anyhow::bail!("unsupported dependency type: {}", dep.name);
            }
        }
    }

    let digest = h.finalize();
    Ok((
        Eligibility::Eligible,
        ActionKey {
            hex: format!("{:x}", digest),
        },
    ))
}

pub fn compute_action_key_v1_remote_policy(
    policy: &str,
    target_name: &str,
    deps: &mut [(char, state::File)],
) -> anyhow::Result<(Eligibility, ActionKey)> {
    deps.sort_by(|(ma, fa), (mb, fb)| (ma, &fa.name).cmp(&(mb, &fb.name)));

    let mut h = Sha256::new();
    hash_str(&mut h, "redo-action-key-v1-remote");
    hash_str(&mut h, &format!("policy={}", policy));
    hash_str(&mut h, version::TAG);
    hash_str(&mut h, std::env::consts::OS);
    hash_str(&mut h, std::env::consts::ARCH);
    hash_str(&mut h, target_name);
    h.update((deps.len() as u64).to_le_bytes());

    for (mode, dep) in deps.iter() {
        if dep.name == state::ALWAYS {
            return Ok((
                Eligibility::SkipAlways,
                ActionKey {
                    hex: String::new(),
                },
            ));
        }
        h.update([*mode as u8]);
        hash_str(&mut h, &dep.name);

        let p = env::v().base.join(&dep.name);
        let meta = match fs::symlink_metadata(&p) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(anyhow::anyhow!(e)),
        };

        match meta {
            None => {
                hash_str(&mut h, "MISSING");
            }
            Some(m) if m.is_dir() => {
                hash_str(&mut h, "DIR");
            }
            Some(m) if m.file_type().is_symlink() => {
                hash_str(&mut h, "SYMLINK");
                let link = fs::read_link(&p).ok();
                if let Some(l) = link {
                    hash_str(&mut h, &l.to_string_lossy());
                } else {
                    hash_str(&mut h, "READLINK_FAILED");
                }

                let ref_meta = match fs::metadata(&p) {
                    Ok(m) => Some(m),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => return Err(anyhow::anyhow!(e)),
                };
                match ref_meta {
                    None => hash_str(&mut h, "REF_MISSING"),
                    Some(rm) if rm.is_dir() => hash_str(&mut h, "REF_DIR"),
                    Some(rm) if rm.is_file() => {
                        hash_str(&mut h, "REF_FILE");
                        let d = sha256_file_hex(&p)?;
                        hash_str(&mut h, &d);
                        let mode_bits = rm.permissions().mode() & 0o777;
                        h.update((mode_bits as u64).to_le_bytes());
                    }
                    Some(_) => hash_str(&mut h, "REF_OTHER"),
                }
            }
            Some(m) if m.is_file() => {
                hash_str(&mut h, "FILE");
                let d = sha256_file_hex(&p)?;
                hash_str(&mut h, &d);
                let mode_bits = m.permissions().mode() & 0o777;
                h.update((mode_bits as u64).to_le_bytes());
            }
            Some(_) => {
                anyhow::bail!("unsupported dependency type: {}", dep.name);
            }
        }
    }

    let digest = h.finalize();
    Ok((
        Eligibility::Eligible,
        ActionKey {
            hex: format!("{:x}", digest),
        },
    ))
}

#[derive(Debug)]
struct ObjEntry {
    blob: PathBuf,
    meta: PathBuf,
    size: u64,
    mtime_secs: u64,
}

fn collect_objects() -> anyhow::Result<Vec<ObjEntry>> {
    let mut out: Vec<ObjEntry> = Vec::new();
    let od = objects_dir();
    let rd = match fs::read_dir(&od) {
        Ok(r) => r,
        Err(_) => return Ok(out),
    };
    for bucket in rd.flatten() {
        let p = bucket.path();
        if !p.is_dir() {
            continue;
        }
        let ents = match fs::read_dir(&p) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for ent in ents.flatten() {
            let ep = ent.path();
            let Some(name) = ep.file_name().map(|s| s.to_string_lossy().to_string()) else {
                continue;
            };
            if !name.ends_with(".blob") {
                continue;
            }
            let st = match fs::metadata(&ep) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let size = st.len();
            let mtime_secs = st
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let key_hex = name.trim_end_matches(".blob");
            out.push(ObjEntry {
                blob: ep.clone(),
                meta: ep.with_file_name(format!("{}.meta", key_hex)),
                size,
                mtime_secs,
            });
        }
    }
    Ok(out)
}

fn maybe_gc() -> anyhow::Result<()> {
    let maxb = max_bytes();
    if maxb == 0 {
        return Ok(());
    }
    let mut objs = collect_objects()?;
    let mut total: u64 = objs.iter().map(|o| o.size).sum();
    if total <= maxb {
        return Ok(());
    }
    // Delete oldest first.
    objs.sort_by_key(|o| (o.mtime_secs, Reverse(o.size)));
    for o in objs {
        if total <= maxb {
            break;
        }
        let _ = fs::remove_file(&o.blob);
        let _ = fs::remove_file(&o.meta);
        total = total.saturating_sub(o.size);
    }
    Ok(())
}

