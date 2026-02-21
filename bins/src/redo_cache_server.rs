use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
struct Config {
    root: PathBuf,
    read_only: bool,
    require_bearer: Option<String>,
    enable_exec: bool,
}

fn usage() -> &'static str {
    "redo-cache-server --listen host:port --root DIR [--read-only] [--require-bearer TOKEN] [--write-url-file PATH] [--enable-exec]\n"
}

fn parse_listen(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() {
        return "127.0.0.1:0".to_string();
    }
    if s.starts_with(':') {
        return format!("127.0.0.1{s}");
    }
    s.to_string()
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn bucket_dir(root: &Path, kind: &str, key: &str) -> PathBuf {
    let aa = key.get(0..2).unwrap_or("00");
    root.join(kind).join(aa)
}

fn blob_path(root: &Path, sha256_hex: &str) -> PathBuf {
    bucket_dir(root, "blobs", sha256_hex).join(sha256_hex)
}

fn action_path(root: &Path, action_key_hex: &str) -> PathBuf {
    bucket_dir(root, "actions", action_key_hex).join(action_key_hex)
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

fn read_exact_discard<R: Read>(r: &mut R, mut n: u64) -> anyhow::Result<()> {
    let mut buf = vec![0u8; 1024 * 1024];
    while n > 0 {
        let want = std::cmp::min(buf.len() as u64, n) as usize;
        let got = r.read(&mut buf[..want])?;
        if got == 0 {
            break;
        }
        n = n.saturating_sub(got as u64);
    }
    Ok(())
}

fn write_status(s: &mut TcpStream, code: u16, reason: &str, body: &[u8], ctype: &str) {
    let _ = write!(
        s,
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n",
        code,
        reason,
        body.len(),
        ctype
    );
    let _ = s.write_all(body);
    let _ = s.flush();
}

fn write_json(s: &mut TcpStream, code: u16, reason: &str, body: &str) {
    write_status(s, code, reason, body.as_bytes(), "application/json");
}

fn write_text(s: &mut TcpStream, code: u16, reason: &str, body: &str) {
    write_status(s, code, reason, body.as_bytes(), "text/plain");
}

fn json_get_string(body: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let i = body.find(&needle)?;
    let rest = &body[i + needle.len()..];
    let j = rest.find(':')?;
    let rest = rest[j + 1..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let k = rest.find('"')?;
    Some(rest[..k].to_string())
}

fn store_blob_bytes_known(root: &Path, sha256_hex: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let mut h = Sha256::new();
    h.update(bytes);
    let got = format!("{:x}", h.finalize());
    if got != sha256_hex {
        anyhow::bail!("digest_mismatch");
    }
    let dest = blob_path(root, sha256_hex);
    if let Some(d) = dest.parent() {
        fs::create_dir_all(d)?;
    }
    if dest.exists() {
        return Ok(());
    }
    let c = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dest.with_file_name(format!(".{}.{}.{}.tmp", sha256_hex, std::process::id(), c));
    fs::write(&tmp, bytes)?;
    match durable_rename_best_effort(&tmp, &dest) {
        Ok(()) => Ok(()),
        Err(_e) if dest.exists() => {
            let _ = fs::remove_file(&tmp);
            Ok(())
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn store_blob_bytes(root: &Path, bytes: &[u8]) -> anyhow::Result<String> {
    let mut h = Sha256::new();
    h.update(bytes);
    let hex = format!("{:x}", h.finalize());
    store_blob_bytes_known(root, &hex, bytes)?;
    Ok(hex)
}

fn store_blob_from_file_compute(root: &Path, src: &Path) -> anyhow::Result<(String, u64)> {
    let st = fs::metadata(src)?;
    let size = st.len();

    let c = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = root
        .join("blobs")
        .join(format!(".tmp.{}.{}", std::process::id(), c));
    if let Some(d) = tmp.parent() {
        fs::create_dir_all(d)?;
    }

    let mut r = fs::File::open(src)?;
    let mut w = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;

    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n])?;
        h.update(&buf[..n]);
    }
    w.sync_all()?;

    let hex = format!("{:x}", h.finalize());
    let dest = blob_path(root, &hex);
    if let Some(d) = dest.parent() {
        fs::create_dir_all(d)?;
    }
    if dest.exists() {
        let _ = fs::remove_file(&tmp);
        return Ok((hex, size));
    }
    match durable_rename_best_effort(&tmp, &dest) {
        Ok(()) => Ok((hex, size)),
        Err(_e) if dest.exists() => {
            let _ = fs::remove_file(&tmp);
            Ok((hex, size))
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn safe_rel_path(p: &str) -> bool {
    let path = Path::new(p);
    if path.is_absolute() {
        return false;
    }
    for c in path.components() {
        use std::path::Component;
        match c {
            Component::CurDir | Component::Normal(_) => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

#[derive(Debug, Deserialize)]
struct ExecInput {
    path: String,   // relative to /work
    sha256: String, // hex
    mode: u32,      // permission bits
}

#[derive(Debug, Deserialize)]
struct ExecRequest {
    schema: String,
    argv: Vec<String>,
    cwd_rel: String,
    target_rel: String,
    tmp_rel: String,
    deps_out0_rel: String,
    trace_out0_rel: String,
    inputs: Vec<ExecInput>,
}

#[cfg(target_os = "linux")]
fn handle_post_exec(cfg: &Config, r: &mut BufReader<TcpStream>, clen: u64) -> anyhow::Result<()> {
    if !cfg.enable_exec {
        let _ = read_exact_discard(r, clen);
        write_text(r.get_mut(), 404, "Not Found", "not_found\n");
        return Ok(());
    }
    if clen > 50 * 1024 * 1024 {
        let _ = read_exact_discard(r, clen);
        write_text(r.get_mut(), 413, "Payload Too Large", "too_large\n");
        return Ok(());
    }
    let mut body = vec![0u8; clen as usize];
    r.read_exact(&mut body)?;

    let req: ExecRequest = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            write_text(r.get_mut(), 400, "Bad Request", "bad_json\n");
            return Ok(());
        }
    };
    if req.schema != "redo-remote-exec:v1" {
        write_text(r.get_mut(), 400, "Bad Request", "bad_schema\n");
        return Ok(());
    }
    if req.argv.is_empty() {
        write_text(r.get_mut(), 400, "Bad Request", "missing_argv\n");
        return Ok(());
    }
    if !safe_rel_path(&req.cwd_rel)
        || !safe_rel_path(&req.target_rel)
        || !safe_rel_path(&req.tmp_rel)
        || !safe_rel_path(&req.deps_out0_rel)
        || !safe_rel_path(&req.trace_out0_rel)
    {
        write_text(r.get_mut(), 400, "Bad Request", "bad_paths\n");
        return Ok(());
    }
    for inp in &req.inputs {
        if inp.sha256.len() != 64 || !is_hex(&inp.sha256) || !safe_rel_path(&inp.path) {
            write_text(r.get_mut(), 400, "Bad Request", "bad_input\n");
            return Ok(());
        }
    }

    // Prepare an isolated work directory on the host.
    let c = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let exec_root = cfg
        .root
        .join("exec")
        .join(format!("{}.{}", std::process::id(), c));
    let work_host = exec_root.join("work");
    let _ = fs::create_dir_all(&work_host);

    // Materialize declared input files from blobs.
    for inp in &req.inputs {
        let src = blob_path(&cfg.root, &inp.sha256);
        if !src.exists() {
            let _ = fs::remove_dir_all(&exec_root);
            write_text(r.get_mut(), 400, "Bad Request", "missing_blob\n");
            return Ok(());
        }
        let dest = work_host.join(&inp.path);
        if let Some(d) = dest.parent() {
            let _ = fs::create_dir_all(d);
        }
        let _ = fs::copy(&src, &dest);
        let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(inp.mode & 0o777));
    }

    // Ensure redo internal dirs exist.
    let _ = fs::create_dir_all(work_host.join(".redo"));
    if let Some(d) = work_host.join(&req.deps_out0_rel).parent() {
        let _ = fs::create_dir_all(d);
    }
    if let Some(d) = work_host.join(&req.trace_out0_rel).parent() {
        let _ = fs::create_dir_all(d);
    }
    // Pre-create deps file (empty).
    let _ = fs::write(work_host.join(&req.deps_out0_rel), &[]);

    // Install minimal `redo-ifchange`/`redo-ifcreate` wrappers so `.do` scripts can
    // declare dependencies without touching the client sqlite DB. These wrappers
    // write NUL-separated (mode,path) pairs to `$REDO_REMOTE_DEPS_OUT0`.
    let remote_bin = work_host.join(".redo").join("remote-bin");
    let _ = fs::create_dir_all(&remote_bin);
    let ifchange_path = remote_bin.join("redo-ifchange");
    let ifcreate_path = remote_bin.join("redo-ifcreate");
    let ifchange_script = r#"#!/bin/sh
set -euo pipefail

out0="${REDO_REMOTE_DEPS_OUT0:-}"
if [ -z "$out0" ]; then
  echo "redo-ifchange(remote): missing REDO_REMOTE_DEPS_OUT0" >&2
  exit 2
fi

# Minimal option skipping: ignore flags until '--' or first non-flag.
while [ "$#" -gt 0 ]; do
  case "$1" in
    --) shift; break ;;
    -*) shift ;;
    *) break ;;
  esac
done

for t in "$@"; do
  if [ -z "$t" ]; then
    echo "cannot build the empty target (\"\")." >&2
    exit 204
  fi
  if [ ! -e "$t" ]; then
    echo "redo-ifchange(remote): missing dependency: $t" >&2
    exit 1
  fi
  printf 'm\0%s\0' "$t" >>"$out0"
done
exit 0
"#;
    let ifcreate_script = r#"#!/bin/sh
set -euo pipefail

out0="${REDO_REMOTE_DEPS_OUT0:-}"
if [ -z "$out0" ]; then
  echo "redo-ifcreate(remote): missing REDO_REMOTE_DEPS_OUT0" >&2
  exit 2
fi

for t in "$@"; do
  if [ -z "$t" ]; then
    echo "cannot build the empty target (\"\")." >&2
    exit 204
  fi
  if [ -e "$t" ]; then
    echo "redo-ifcreate: error: $t already exists" >&2
    exit 1
  fi
  printf 'c\0%s\0' "$t" >>"$out0"
done
exit 0
"#;
    let _ = fs::write(&ifchange_path, ifchange_script.as_bytes());
    let _ = fs::write(&ifcreate_path, ifcreate_script.as_bytes());
    let _ = fs::set_permissions(&ifchange_path, fs::Permissions::from_mode(0o755));
    let _ = fs::set_permissions(&ifcreate_path, fs::Permissions::from_mode(0o755));

    let (exit_code, stdout, stderr) = {
        #[cfg(target_os = "linux")]
        {
            // Require bwrap for isolation.
            if Command::new("bwrap").arg("--version").output().is_err() {
                let _ = fs::remove_dir_all(&exec_root);
                write_text(r.get_mut(), 501, "Not Implemented", "bwrap_missing\n");
                return Ok(());
            }

            let mut cmd = Command::new("bwrap");
            cmd.arg("--die-with-parent");
            cmd.arg("--unshare-net");
            cmd.arg("--proc").arg("/proc");
            cmd.arg("--dev").arg("/dev");
            cmd.arg("--tmpfs").arg("/tmp");
            cmd.arg("--bind")
                .arg(work_host.to_string_lossy().to_string())
                .arg("/work");
            for (host, guest) in [
                ("/usr", "/usr"),
                ("/bin", "/bin"),
                ("/lib", "/lib"),
                ("/lib64", "/lib64"),
                ("/etc", "/etc"),
            ] {
                if Path::new(host).exists() {
                    cmd.arg("--ro-bind").arg(host).arg(guest);
                }
            }
            cmd.arg("--chdir")
                .arg(format!("/work/{}", req.cwd_rel.trim_end_matches('/')));

            let trace_abs = format!("/work/{}", req.trace_out0_rel);
            cmd.arg("--");
            cmd.arg("redo-trace");
            cmd.arg("--trace-out0");
            cmd.arg(trace_abs);
            cmd.arg("--mode");
            cmd.arg("read");
            cmd.arg("--");
            for a in &req.argv {
                cmd.arg(a);
            }
            cmd.env_clear();
            cmd.env("PATH", "/work/.redo/remote-bin:/usr/bin:/bin");
            cmd.env("REDO_BASE", "/work");
            cmd.env(
                "REDO_REMOTE_DEPS_OUT0",
                format!("/work/{}", req.deps_out0_rel),
            );

            let out = match cmd.output() {
                Ok(o) => o,
                Err(e) => {
                    let _ = fs::remove_dir_all(&exec_root);
                    write_text(r.get_mut(), 500, "Internal Server Error", &format!("exec_failed:{e}\n"));
                    return Ok(());
                }
            };
            let code = out.status.code().unwrap_or(201);
            (code, out.stdout, out.stderr)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = fs::remove_dir_all(&exec_root);
            write_text(r.get_mut(), 501, "Not Implemented", "exec_unavailable\n");
            return Ok(());
        }
    };

    // Reproduce basic redo output rules for $3/stdout.
    let tmp_path = work_host.join(&req.tmp_rel);
    let target_path = work_host.join(&req.target_rel);
    let stdout_nonempty = !stdout.is_empty();
    let tmp_exists = tmp_path.exists();
    let mut final_exit = exit_code;
    if final_exit == 0 {
        if stdout_nonempty && tmp_exists {
            final_exit = 207;
        } else if stdout_nonempty && !tmp_exists {
            if let Some(d) = tmp_path.parent() {
                let _ = fs::create_dir_all(d);
            }
            let _ = fs::write(&tmp_path, &stdout);
        }
        if final_exit == 0 {
            if tmp_path.exists() {
                if let Some(d) = target_path.parent() {
                    let _ = fs::create_dir_all(d);
                }
                let _ = fs::rename(&tmp_path, &target_path);
            } else {
                let _ = fs::remove_file(&target_path);
            }
        }
    }

    // Ensure trace exists (redo-trace should write it; if not, write a marker).
    let trace_path = work_host.join(&req.trace_out0_rel);
    if !trace_path.exists() {
        let _ = fs::write(&trace_path, b"TRACE_UNAVAILABLE:missing_trace\0");
    }

    // Collect outputs into blobs.
    let stdout_hex = store_blob_bytes(&cfg.root, &stdout).ok();
    let stderr_hex = store_blob_bytes(&cfg.root, &stderr).ok();
    let deps_hex = fs::read(work_host.join(&req.deps_out0_rel))
        .ok()
        .and_then(|b| store_blob_bytes(&cfg.root, &b).ok());
    let trace_hex = fs::read(&trace_path)
        .ok()
        .and_then(|b| store_blob_bytes(&cfg.root, &b).ok());

    let mut out_manifest_hex: Option<String> = None;
    if final_exit == 0 && target_path.exists() {
        if let Ok(st) = fs::metadata(&target_path) {
            if st.is_file() {
                if let Ok((blob_hex, size)) = store_blob_from_file_compute(&cfg.root, &target_path) {
                    let mode = st.permissions().mode() & 0o777;
                    let manifest_json = format!(
                        "{{\"schema\":\"redo-artifact-manifest:v1\",\"kind\":\"file\",\"digest\":\"sha256:{}\",\"size\":{},\"mode\":{}}}",
                        blob_hex, size, mode
                    );
                    if let Ok(man_hex) = store_blob_bytes(&cfg.root, manifest_json.as_bytes()) {
                        out_manifest_hex = Some(man_hex);
                    }
                }
            }
        }
    }

    let body = format!(
        "{{\"schema\":\"redo-remote-exec-result:v1\",\"exit_code\":{},\"output_manifest_sha256\":{},\"deps_sha256\":{},\"trace_sha256\":{},\"stdout_sha256\":{},\"stderr_sha256\":{}}}",
        final_exit,
        out_manifest_hex.as_ref().map(|s| format!("\"{}\"", s)).unwrap_or("null".to_string()),
        deps_hex.as_ref().map(|s| format!("\"{}\"", s)).unwrap_or("null".to_string()),
        trace_hex.as_ref().map(|s| format!("\"{}\"", s)).unwrap_or("null".to_string()),
        stdout_hex.as_ref().map(|s| format!("\"{}\"", s)).unwrap_or("null".to_string()),
        stderr_hex.as_ref().map(|s| format!("\"{}\"", s)).unwrap_or("null".to_string()),
    );
    write_json(r.get_mut(), 200, "OK", &body);
    let _ = fs::remove_dir_all(&exec_root);
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn handle_post_exec(cfg: &Config, r: &mut BufReader<TcpStream>, clen: u64) -> anyhow::Result<()> {
    if !cfg.enable_exec {
        let _ = read_exact_discard(r, clen);
        write_text(r.get_mut(), 404, "Not Found", "not_found\n");
        return Ok(());
    }
    if clen > 50 * 1024 * 1024 {
        let _ = read_exact_discard(r, clen);
        write_text(r.get_mut(), 413, "Payload Too Large", "too_large\n");
        return Ok(());
    }
    let _ = read_exact_discard(r, clen);
    write_text(r.get_mut(), 501, "Not Implemented", "exec_unavailable\n");
    Ok(())
}

fn auth_ok(headers: &HashMap<String, String>, want: &Option<String>) -> bool {
    let Some(tok) = want else { return true; };
    let got = headers.get("authorization").map(|s| s.trim().to_string()).unwrap_or_default();
    got == format!("Bearer {}", tok)
}

fn handle_get_blob(cfg: &Config, stream: &mut TcpStream, sha256_hex: &str) -> anyhow::Result<()> {
    let p = blob_path(&cfg.root, sha256_hex);
    let mut f = match fs::File::open(&p) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            write_text(stream, 404, "Not Found", "not_found\n");
            return Ok(());
        }
        Err(e) => return Err(anyhow::anyhow!(e)),
    };
    let st = f.metadata().ok();
    let len = st.map(|m| m.len()).unwrap_or(0);
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
        len
    );
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        stream.write_all(&buf[..n])?;
    }
    stream.flush()?;
    Ok(())
}

fn handle_put_blob(
    cfg: &Config,
    r: &mut BufReader<TcpStream>,
    sha256_hex: &str,
    clen: u64,
) -> anyhow::Result<()> {
    let dest = blob_path(&cfg.root, sha256_hex);
    if let Some(d) = dest.parent() {
        fs::create_dir_all(d)?;
    }

    // Write to a temp file in the same directory, hash as we stream.
    let c = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dest.with_file_name(format!(".{}.{}.{}.tmp", sha256_hex, std::process::id(), c));
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;

    let mut h = Sha256::new();
    let mut remaining = clen;
    let mut buf = vec![0u8; 1024 * 1024];
    while remaining > 0 {
        let want = std::cmp::min(buf.len() as u64, remaining) as usize;
        let n = r.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n])?;
        h.update(&buf[..n]);
        remaining = remaining.saturating_sub(n as u64);
    }
    f.sync_all()?;

    let got = format!("{:x}", h.finalize());
    if got != sha256_hex {
        let _ = fs::remove_file(&tmp);
        write_text(r.get_mut(), 400, "Bad Request", "digest_mismatch\n");
        return Ok(());
    }

    // Idempotent: if another writer already stored it, accept.
    if dest.exists() {
        let _ = fs::remove_file(&tmp);
        write_text(r.get_mut(), 200, "OK", "ok\n");
        return Ok(());
    }

    match durable_rename_best_effort(&tmp, &dest) {
        Ok(()) => {
            write_text(r.get_mut(), 200, "OK", "ok\n");
        }
        Err(_e) if dest.exists() => {
            let _ = fs::remove_file(&tmp);
            write_text(r.get_mut(), 200, "OK", "ok\n");
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
    }
    Ok(())
}

fn handle_get_action(cfg: &Config, stream: &mut TcpStream, action_key_hex: &str) -> anyhow::Result<()> {
    let p = action_path(&cfg.root, action_key_hex);
    let s = match fs::read_to_string(&p) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            write_text(stream, 404, "Not Found", "not_found\n");
            return Ok(());
        }
        Err(e) => return Err(anyhow::anyhow!(e)),
    };
    let man = s.lines().next().unwrap_or("").trim();
    if man.is_empty() || !is_hex(man) {
        write_text(stream, 500, "Internal Server Error", "bad_mapping\n");
        return Ok(());
    }
    let body = format!(
        "{{\"schema\":\"redo-action-result:v1\",\"manifest_sha256\":\"{}\"}}",
        man
    );
    write_json(stream, 200, "OK", &body);
    Ok(())
}

fn handle_put_action(
    cfg: &Config,
    r: &mut BufReader<TcpStream>,
    action_key_hex: &str,
    clen: u64,
) -> anyhow::Result<()> {
    let dest = action_path(&cfg.root, action_key_hex);
    if let Some(d) = dest.parent() {
        fs::create_dir_all(d)?;
    }

    let mut body = vec![0u8; clen as usize];
    r.read_exact(&mut body)?;
    let body_s = String::from_utf8_lossy(&body).to_string();
    let man = json_get_string(&body_s, "manifest_sha256")
        .or_else(|| json_get_string(&body_s, "artifact_manifest_sha256"))
        .or_else(|| json_get_string(&body_s, "artifact_manifest_digest"))
        .unwrap_or_default();
    if man.len() != 64 || !is_hex(&man) {
        write_text(r.get_mut(), 400, "Bad Request", "invalid_manifest_sha256\n");
        return Ok(());
    }

    if let Ok(existing) = fs::read_to_string(&dest) {
        let cur = existing.lines().next().unwrap_or("").trim();
        if cur == man {
            write_text(r.get_mut(), 200, "OK", "ok\n");
            return Ok(());
        } else {
            write_text(r.get_mut(), 409, "Conflict", "conflict\n");
            return Ok(());
        }
    }

    let tmp = dest.with_file_name(format!(
        ".{}.{}.{}.tmp",
        action_key_hex,
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        writeln!(f, "{}", man)?;
        f.sync_all()?;
    }

    match durable_rename_best_effort(&tmp, &dest) {
        Ok(()) => write_text(r.get_mut(), 200, "OK", "ok\n"),
        Err(_e) if dest.exists() => {
            // Race: first writer won; enforce immutability.
            let cur = fs::read_to_string(&dest).unwrap_or_default();
            let cur = cur.lines().next().unwrap_or("").trim().to_string();
            let _ = fs::remove_file(&tmp);
            if cur == man {
                write_text(r.get_mut(), 200, "OK", "ok\n");
            } else {
                write_text(r.get_mut(), 409, "Conflict", "conflict\n");
            }
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
    }
    Ok(())
}

fn handle_one(cfg: Arc<Config>, stream: TcpStream) -> anyhow::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let mut r = BufReader::new(stream);

    let mut req_line = String::new();
    let n = r.read_line(&mut req_line)?;
    if n == 0 {
        return Ok(());
    }
    let req_line = req_line.trim_end_matches(|c| c == '\n' || c == '\r');
    let mut it = req_line.split_whitespace();
    let method = it.next().unwrap_or("");
    let path = it.next().unwrap_or("");

    let mut headers: HashMap<String, String> = HashMap::new();
    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        if line == "\r\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let clen = headers
        .get("content-length")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    if method == "PUT" || method == "POST" {
        if cfg.read_only {
            let _ = read_exact_discard(&mut r, clen);
            write_text(r.get_mut(), 403, "Forbidden", "read_only\n");
            return Ok(());
        }
        if !auth_ok(&headers, &cfg.require_bearer) {
            let _ = read_exact_discard(&mut r, clen);
            write_text(r.get_mut(), 403, "Forbidden", "forbidden\n");
            return Ok(());
        }
    }

    if path.trim_end_matches('/') == "/v1/exec" {
        match method {
            "POST" => handle_post_exec(&cfg, &mut r, clen)?,
            _ => {
                if method == "PUT" {
                    let _ = read_exact_discard(&mut r, clen);
                }
                write_text(r.get_mut(), 405, "Method Not Allowed", "method_not_allowed\n");
            }
        }
        return Ok(());
    }

    if let Some(rest) = path.strip_prefix("/v1/blobs/") {
        let key = rest.trim_end_matches('/');
        if key.len() != 64 || !is_hex(key) {
            if method == "PUT" {
                let _ = read_exact_discard(&mut r, clen);
            }
            write_text(r.get_mut(), 400, "Bad Request", "invalid_digest\n");
            return Ok(());
        }
        match method {
            "GET" => handle_get_blob(&cfg, r.get_mut(), key)?,
            "PUT" => handle_put_blob(&cfg, &mut r, key, clen)?,
            _ => write_text(r.get_mut(), 405, "Method Not Allowed", "method_not_allowed\n"),
        }
        return Ok(());
    }

    if let Some(rest) = path.strip_prefix("/v1/actions/") {
        let key = rest.trim_end_matches('/');
        if key.len() != 64 || !is_hex(key) {
            if method == "PUT" {
                let _ = read_exact_discard(&mut r, clen);
            }
            write_text(r.get_mut(), 400, "Bad Request", "invalid_actionkey\n");
            return Ok(());
        }
        match method {
            "GET" => handle_get_action(&cfg, r.get_mut(), key)?,
            "PUT" => handle_put_action(&cfg, &mut r, key, clen)?,
            _ => write_text(r.get_mut(), 405, "Method Not Allowed", "method_not_allowed\n"),
        }
        return Ok(());
    }

    if method == "PUT" {
        let _ = read_exact_discard(&mut r, clen);
    }
    write_text(r.get_mut(), 404, "Not Found", "not_found\n");
    Ok(())
}

fn url_for_addr(bind: &str, addr: SocketAddr) -> String {
    // Prefer a connectable loopback host in common test setups.
    let host = if bind.starts_with("0.0.0.0:") || bind.starts_with("::") {
        "127.0.0.1".to_string()
    } else if bind.starts_with(':') {
        "127.0.0.1".to_string()
    } else {
        addr.ip().to_string()
    };
    format!("http://{}:{}", host, addr.port())
}

fn main() {
    let mut listen = "127.0.0.1:0".to_string();
    let mut root: Option<PathBuf> = None;
    let mut read_only = false;
    let mut require_bearer: Option<String> = None;
    let mut write_url_file: Option<PathBuf> = None;
    let mut enable_exec = false;

    let mut args = std::env::args().skip(1).peekable();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--listen" => {
                listen = parse_listen(&args.next().unwrap_or_default());
            }
            "--root" => {
                root = Some(PathBuf::from(args.next().unwrap_or_default()));
            }
            "--read-only" => read_only = true,
            "--require-bearer" => {
                require_bearer = Some(args.next().unwrap_or_default());
            }
            "--write-url-file" => {
                write_url_file = Some(PathBuf::from(args.next().unwrap_or_default()));
            }
            "--enable-exec" => enable_exec = true,
            "--help" | "-h" => {
                eprintln!("{}", usage());
                std::process::exit(0);
            }
            _ => {}
        }
    }

    let Some(root) = root else {
        eprintln!("{}", usage());
        std::process::exit(2);
    };

    let l = match TcpListener::bind(&listen) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("bind {} failed: {}", listen, e);
            std::process::exit(2);
        }
    };

    let addr = l.local_addr().unwrap_or_else(|_| "127.0.0.1:0".parse().unwrap());
    let url = url_for_addr(&listen, addr);
    if let Some(p) = write_url_file {
        let _ = fs::write(&p, format!("{}\n", url));
    } else {
        eprintln!("listening {}", url);
    }

    let cfg = Arc::new(Config {
        root,
        read_only,
        require_bearer,
        enable_exec,
    });

    for conn in l.incoming() {
        match conn {
            Ok(s) => {
                let cfg2 = cfg.clone();
                std::thread::spawn(move || {
                    let _ = handle_one(cfg2, s);
                });
            }
            Err(_) => {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

