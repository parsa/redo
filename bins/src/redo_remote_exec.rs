use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use redo_core::{action_cache, remote_cache, remote_exec};

fn usage() -> &'static str {
    "redo-remote-exec --out <path> --deps-out0 <path> --trace-out0 <path> \\\n+  --cwd-rel <path> --target-rel <path> --tmp-rel <path> \\\n+  --input <relpath> [--input <relpath> ...] -- <argv...>\n"
}

fn write_file_create_parent(p: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(d) = p.parent() {
        fs::create_dir_all(d)?;
    }
    fs::write(p, bytes)?;
    Ok(())
}

fn main() {
    let rv = match real_main() {
        Ok(rv) => rv,
        Err(e) => {
            eprintln!("remote_exec: {:?}", e);
            201
        }
    };
    std::process::exit(rv);
}

fn real_main() -> anyhow::Result<i32> {
    let mut out_path: Option<PathBuf> = None;
    let mut deps_out0_path: Option<PathBuf> = None;
    let mut trace_out0_path: Option<PathBuf> = None;
    let mut remote_ok_path: Option<PathBuf> = None;
    let mut cwd_rel = String::new();
    let mut target_rel = String::new();
    let mut tmp_rel = String::new();
    let mut inputs: Vec<String> = Vec::new();
    let mut argv: Vec<String> = Vec::new();

    let mut it = std::env::args().skip(1).peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => out_path = Some(PathBuf::from(it.next().unwrap_or_default())),
            "--deps-out0" => deps_out0_path = Some(PathBuf::from(it.next().unwrap_or_default())),
            "--trace-out0" => trace_out0_path = Some(PathBuf::from(it.next().unwrap_or_default())),
            "--remote-ok" => remote_ok_path = Some(PathBuf::from(it.next().unwrap_or_default())),
            "--cwd-rel" => cwd_rel = it.next().unwrap_or_default(),
            "--target-rel" => target_rel = it.next().unwrap_or_default(),
            "--tmp-rel" => tmp_rel = it.next().unwrap_or_default(),
            "--input" => inputs.push(it.next().unwrap_or_default()),
            "--help" | "-h" => {
                eprintln!("{}", usage());
                return Ok(0);
            }
            "--" => {
                argv.extend(it.map(|s| s.to_string()));
                break;
            }
            _ => {}
        }
    }

    let Some(out_path) = out_path else {
        anyhow::bail!("missing --out\n{}", usage());
    };
    let Some(deps_out0_path) = deps_out0_path else {
        anyhow::bail!("missing --deps-out0\n{}", usage());
    };
    let Some(trace_out0_path) = trace_out0_path else {
        anyhow::bail!("missing --trace-out0\n{}", usage());
    };
    if cwd_rel.is_empty() || target_rel.is_empty() || tmp_rel.is_empty() {
        anyhow::bail!("missing --cwd-rel/--target-rel/--tmp-rel\n{}", usage());
    }
    if argv.is_empty() {
        anyhow::bail!("missing argv after --\n{}", usage());
    }

    let base = std::env::var_os("REDO_BASE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let strict = {
        let v = std::env::var("REDO_STRICT").unwrap_or_default();
        let v = v.trim();
        !v.is_empty() && v != "0"
    };

    let cfg = match remote_cache::config_from_env() {
        Ok(Some(cfg)) => Some(cfg),
        Ok(None) => None,
        Err(e) => {
            eprintln!("remote_exec: remote_cfg error: {:?}", e);
            None
        }
    };

    if cfg.is_none() {
        eprintln!("remote_exec: fallback to local (no remote url configured)");
        // local fallback: run the original argv directly (or under redo-trace in strict mode)
        let code = if strict {
            Command::new("redo-trace")
                .arg("--trace-out0")
                .arg(&trace_out0_path)
                .arg("--mode")
                .arg("read")
                .arg("--")
                .args(&argv)
                .status()?
                .code()
                .unwrap_or(201)
        } else {
            Command::new(&argv[0])
                .args(&argv[1..])
                .status()?
                .code()
                .unwrap_or(201)
        };
        return Ok(code);
    }
    let cfg = cfg.unwrap();

    // Upload inputs to CAS, and build request input list.
    let mut req_inputs: Vec<remote_exec::ExecInput> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for rel in inputs {
        let rel = rel.trim().to_string();
        if rel.is_empty() {
            continue;
        }
        if !seen.insert(rel.clone()) {
            continue;
        }
        let abs = base.join(&rel);
        let meta = fs::symlink_metadata(&abs)?;
        if !meta.is_file() {
            anyhow::bail!("remote_exec: unsupported input type (not file): {}", rel);
        }
        let mode = meta.permissions().mode() & 0o777;
        let sha = action_cache::sha256_file_hex(&abs)?;
        remote_cache::put_blob_from_file(&cfg, &sha, &abs)?;
        req_inputs.push(remote_exec::ExecInput {
            path: rel,
            sha256: sha,
            mode,
        });
    }

    // Remote-side output bookkeeping (paths are workspace-relative).
    let deps_out0_rel = pathdiff::diff_paths(&deps_out0_path, &base)
        .ok_or_else(|| anyhow::anyhow!("remote_exec: deps_out0 not under base"))?
        .to_string_lossy()
        .to_string();
    let trace_out0_rel = pathdiff::diff_paths(&trace_out0_path, &base)
        .ok_or_else(|| anyhow::anyhow!("remote_exec: trace_out0 not under base"))?
        .to_string_lossy()
        .to_string();

    let req = remote_exec::ExecRequest {
        schema: "redo-remote-exec:v1".to_string(),
        argv: argv.clone(),
        cwd_rel,
        target_rel,
        tmp_rel,
        deps_out0_rel,
        trace_out0_rel,
        inputs: req_inputs,
    };

    let resp = match remote_exec::exec_v1(&cfg, &req) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("remote_exec: fallback to local ({:?})", e);
            // local fallback: run the original argv directly (or under redo-trace in strict mode)
            let code = if strict {
                Command::new("redo-trace")
                    .arg("--trace-out0")
                    .arg(&trace_out0_path)
                    .arg("--mode")
                    .arg("read")
                    .arg("--")
                    .args(&argv)
                    .status()?
                    .code()
                    .unwrap_or(201)
            } else {
                Command::new(&argv[0])
                    .args(&argv[1..])
                    .status()?
                    .code()
                    .unwrap_or(201)
            };
            return Ok(code);
        }
    };

    // If the remote action failed, treat remote-exec as best-effort and fall back to local.
    if resp.exit_code != 0 {
        eprintln!("remote_exec: fallback to local (remote exit_code={})", resp.exit_code);
        let code = if strict {
            Command::new("redo-trace")
                .arg("--trace-out0")
                .arg(&trace_out0_path)
                .arg("--mode")
                .arg("read")
                .arg("--")
                .args(&argv)
                .status()?
                .code()
                .unwrap_or(201)
        } else {
            Command::new(&argv[0])
                .args(&argv[1..])
                .status()?
                .code()
                .unwrap_or(201)
        };
        return Ok(code);
    }

    if let Some(dep_hex) = &resp.deps_sha256 {
        let b = remote_cache::get_blob_bytes_verified(&cfg, dep_hex)?;
        write_file_create_parent(&deps_out0_path, &b)?;
    } else {
        let _ = write_file_create_parent(&deps_out0_path, &[]);
    }

    if let Some(tr_hex) = &resp.trace_sha256 {
        let b = remote_cache::get_blob_bytes_verified(&cfg, tr_hex)?;
        write_file_create_parent(&trace_out0_path, &b)?;
    } else {
        let _ = write_file_create_parent(&trace_out0_path, b"TRACE_UNAVAILABLE:missing_trace\0");
    }

    let Some(man_hex) = &resp.output_manifest_sha256 else {
        eprintln!("remote_exec: fallback to local (missing output manifest)");
        let code = if strict {
            Command::new("redo-trace")
                .arg("--trace-out0")
                .arg(&trace_out0_path)
                .arg("--mode")
                .arg("read")
                .arg("--")
                .args(&argv)
                .status()?
                .code()
                .unwrap_or(201)
        } else {
            Command::new(&argv[0])
                .args(&argv[1..])
                .status()?
                .code()
                .unwrap_or(201)
        };
        return Ok(code);
    };

    if let Some(p) = &remote_ok_path {
        let _ = write_file_create_parent(p, b"ok\n");
    }

    {
        let man_bytes = remote_cache::get_blob_bytes_verified(&cfg, man_hex)?;
        let man_s = String::from_utf8_lossy(&man_bytes).to_string();
        let m = remote_cache::parse_artifact_manifest_v1(&man_s)?;

        // Download verified output blob directly to local $3.
        if let Some(d) = out_path.parent() {
            fs::create_dir_all(d)?;
        }
        let _ = fs::remove_file(&out_path);
        remote_cache::download_blob_to_file_verified(&cfg, &m.blob_sha256, &out_path)?;
        let _ = fs::set_permissions(&out_path, fs::Permissions::from_mode(m.mode & 0o777));
    }

    Ok(0)
}

