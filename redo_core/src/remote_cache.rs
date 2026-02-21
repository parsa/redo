//! Remote artifact cache client (Phase 2).
//!
//! Protocol (HTTP v1) is intentionally tiny:
//! - `GET/PUT /v1/blobs/<sha256hex>` for CAS blobs
//! - `GET/PUT /v1/actions/<actionkey>` for ActionKey -> manifest mapping
//!
//! This module is best-effort: failures must be handled by falling back to local execution.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};

fn env_s(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) => {
            let s = v.trim().to_string();
            if s.is_empty() { None } else { Some(s) }
        }
        Err(_) => None,
    }
}

fn env_flag_set(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => {
            let s = v.trim();
            !s.is_empty() && s != "0"
        }
        Err(_) => false,
    }
}

fn atoi_u64(s: &str) -> u64 {
    s.trim().parse::<u64>().unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct RemoteUrl {
    host: String,
    port: u16,
    base_path: String, // "" or "/prefix"
}

impl RemoteUrl {
    fn join(&self, suffix: &str) -> String {
        let mut p = String::new();
        if self.base_path.is_empty() {
            p.push('/');
        } else {
            p.push_str(&self.base_path);
            if !p.ends_with('/') {
                p.push('/');
            }
        }
        p.push_str(suffix.trim_start_matches('/'));
        p
    }
}

#[derive(Debug, Clone)]
pub struct RemoteConfig {
    pub url: RemoteUrl,
    pub timeout: Duration,
    pub bearer_token: Option<String>,
    pub push_enabled: bool,
}

pub fn config_from_env() -> anyhow::Result<Option<RemoteConfig>> {
    let Some(url_s) = env_s("REDO_ACTION_CACHE_REMOTE_URL") else {
        return Ok(None);
    };
    let url = parse_http_url(&url_s)?;
    let timeout = {
        let secs = env_s("REDO_ACTION_CACHE_REMOTE_TIMEOUT_SECS")
            .map(|s| atoi_u64(&s))
            .filter(|n| *n > 0)
            .unwrap_or(5);
        Duration::from_secs(secs)
    };
    Ok(Some(RemoteConfig {
        url,
        timeout,
        bearer_token: env_s("REDO_ACTION_CACHE_REMOTE_BEARER_TOKEN"),
        push_enabled: env_flag_set("REDO_ACTION_CACHE_REMOTE_PUSH"),
    }))
}

fn parse_http_url(s: &str) -> anyhow::Result<RemoteUrl> {
    let s = s.trim();
    let rest = if let Some(r) = s.strip_prefix("http://") {
        r
    } else if s.starts_with("https://") {
        anyhow::bail!("https URLs are not supported (need http://)");
    } else {
        anyhow::bail!("remote URL must start with http://");
    };

    let (hostport, path) = match rest.split_once('/') {
        Some((a, b)) => (a, format!("/{}", b.trim_end_matches('/'))),
        None => (rest, String::new()),
    };

    let (host, port) = if let Some(h) = hostport.strip_prefix('[') {
        // IPv6 in brackets: [::1]:1234
        let Some((inside, after)) = h.split_once(']') else {
            anyhow::bail!("invalid remote URL host: {:?}", hostport);
        };
        let after = after.trim_start();
        let port = if let Some(p) = after.strip_prefix(':') {
            p.parse::<u16>().unwrap_or(80)
        } else {
            80
        };
        (inside.to_string(), port)
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
                (h.to_string(), p.parse::<u16>().unwrap_or(80))
            }
            _ => (hostport.to_string(), 80),
        }
    };

    Ok(RemoteUrl {
        host,
        port,
        base_path: path,
    })
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

fn read_http_response(mut s: TcpStream) -> anyhow::Result<HttpResponse> {
    let mut r = BufReader::new(&mut s);
    let mut status_line = String::new();
    r.read_line(&mut status_line)?;
    let mut it = status_line.split_whitespace();
    let _http = it.next().unwrap_or("");
    let code_s = it.next().unwrap_or("0");
    let status = code_s.parse::<u16>().unwrap_or(0);

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

    let mut body: Vec<u8> = Vec::new();
    let clen = headers
        .get("content-length")
        .and_then(|s| s.parse::<usize>().ok());
    if let Some(n) = clen {
        body.resize(n, 0u8);
        r.read_exact(&mut body)?;
    } else {
        // Best-effort: read to EOF.
        let _ = r.read_to_end(&mut body);
    }
    Ok(HttpResponse { status, body })
}

fn write_http_request(
    mut s: TcpStream,
    method: &str,
    host: &str,
    path: &str,
    headers: &[(&str, String)],
    body: Option<&[u8]>,
) -> anyhow::Result<TcpStream> {
    let body_len = body.map(|b| b.len()).unwrap_or(0);
    let mut req = String::new();
    req.push_str(&format!("{method} {path} HTTP/1.1\r\n"));
    req.push_str(&format!("Host: {host}\r\n"));
    req.push_str("Connection: close\r\n");
    if body.is_some() {
        req.push_str(&format!("Content-Length: {body_len}\r\n"));
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes())?;
    if let Some(b) = body {
        s.write_all(b)?;
    }
    s.flush()?;
    Ok(s)
}

fn open_stream(cfg: &RemoteConfig) -> anyhow::Result<TcpStream> {
    let addr = (cfg.url.host.as_str(), cfg.url.port);
    let s = TcpStream::connect(addr)?;
    s.set_read_timeout(Some(cfg.timeout))?;
    s.set_write_timeout(Some(cfg.timeout))?;
    Ok(s)
}

fn auth_headers(cfg: &RemoteConfig) -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = Vec::new();
    if let Some(t) = &cfg.bearer_token {
        out.push(("Authorization", format!("Bearer {t}")));
    }
    out
}

fn json_get_string(body: &str, key: &str) -> Option<String> {
    // Minimal JSON extractor: looks for `"key"` then the next quoted string value.
    let needle = format!("\"{}\"", key);
    let i = body.find(&needle)?;
    let rest = &body[i + needle.len()..];
    let j = rest.find(':')?;
    let rest = rest[j + 1..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let k = rest.find('"')?;
    Some(rest[..k].to_string())
}

fn json_get_u64(body: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{}\"", key);
    let i = body.find(&needle)?;
    let rest = &body[i + needle.len()..];
    let j = rest.find(':')?;
    let rest = rest[j + 1..].trim_start();
    let mut end = 0usize;
    for (idx, ch) in rest.char_indices() {
        if !ch.is_ascii_digit() {
            break;
        }
        end = idx + ch.len_utf8();
    }
    if end == 0 {
        return None;
    }
    rest[..end].parse::<u64>().ok()
}

pub fn get_action_manifest_sha256(cfg: &RemoteConfig, action_key_hex: &str) -> anyhow::Result<Option<String>> {
    let path = cfg.url.join(&format!("v1/actions/{action_key_hex}"));
    let s = open_stream(cfg)?;
    let s = write_http_request(s, "GET", &cfg.url.host, &path, &auth_headers(cfg), None)?;
    let resp = read_http_response(s)?;
    match resp.status {
        200 => {
            let body = String::from_utf8_lossy(&resp.body).to_string();
            let v = json_get_string(&body, "manifest_sha256")
                .or_else(|| json_get_string(&body, "artifact_manifest_sha256"))
                .or_else(|| json_get_string(&body, "artifact_manifest_digest"));
            Ok(v)
        }
        404 => Ok(None),
        _ => anyhow::bail!("remote actions GET failed: HTTP {}", resp.status),
    }
}

pub fn get_blob_bytes_verified(cfg: &RemoteConfig, sha256_hex: &str) -> anyhow::Result<Vec<u8>> {
    let path = cfg.url.join(&format!("v1/blobs/{sha256_hex}"));
    let s = open_stream(cfg)?;
    let s = write_http_request(s, "GET", &cfg.url.host, &path, &auth_headers(cfg), None)?;
    let resp = read_http_response(s)?;
    if resp.status != 200 {
        anyhow::bail!("remote blobs GET failed: HTTP {}", resp.status);
    }
    let mut h = Sha256::new();
    h.update(&resp.body);
    let got = format!("{:x}", h.finalize());
    if got != sha256_hex {
        anyhow::bail!("digest mismatch for blob {} (got {})", sha256_hex, got);
    }
    Ok(resp.body)
}

pub fn download_blob_to_file_verified(
    cfg: &RemoteConfig,
    sha256_hex: &str,
    dest: &Path,
) -> anyhow::Result<u64> {
    let path = cfg.url.join(&format!("v1/blobs/{sha256_hex}"));
    let s = open_stream(cfg)?;
    let s = write_http_request(s, "GET", &cfg.url.host, &path, &auth_headers(cfg), None)?;

    // Stream response so large blobs don't live in RAM.
    let mut stream = s;
    let mut r = BufReader::new(&mut stream);
    let mut status_line = String::new();
    r.read_line(&mut status_line)?;
    let mut it = status_line.split_whitespace();
    let _http = it.next().unwrap_or("");
    let code_s = it.next().unwrap_or("0");
    let status = code_s.parse::<u16>().unwrap_or(0);
    if status != 200 {
        // Consume headers/body best-effort for debugging.
        anyhow::bail!("remote blobs GET failed: HTTP {}", status);
    }

    let mut clen: Option<u64> = None;
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
            if k.trim().eq_ignore_ascii_case("content-length") {
                clen = v.trim().parse::<u64>().ok();
            }
            if k.trim().eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked") {
                anyhow::bail!("chunked transfer encoding not supported");
            }
        }
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dest)?;

    let mut h = Sha256::new();
    let mut total: u64 = 0;
    let mut buf = vec![0u8; 1024 * 1024];
    let mut remaining = clen.unwrap_or(u64::MAX);
    while remaining > 0 {
        let want = if remaining == u64::MAX {
            buf.len()
        } else {
            std::cmp::min(buf.len() as u64, remaining) as usize
        };
        let n = r.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n])?;
        h.update(&buf[..n]);
        total = total.saturating_add(n as u64);
        if remaining != u64::MAX {
            remaining = remaining.saturating_sub(n as u64);
        }
    }
    f.sync_all()?;

    let got = format!("{:x}", h.finalize());
    if got != sha256_hex {
        let _ = fs::remove_file(dest);
        anyhow::bail!("digest mismatch for blob {} (got {})", sha256_hex, got);
    }
    Ok(total)
}

pub fn put_blob_from_file(cfg: &RemoteConfig, sha256_hex: &str, src: &Path) -> anyhow::Result<()> {
    let body = fs::read(src)?;
    put_blob_bytes(cfg, sha256_hex, &body)
}

pub fn put_blob_bytes(cfg: &RemoteConfig, sha256_hex: &str, body: &[u8]) -> anyhow::Result<()> {
    let path = cfg.url.join(&format!("v1/blobs/{sha256_hex}"));
    let mut headers = auth_headers(cfg);
    headers.push(("Content-Type", "application/octet-stream".to_string()));
    let s = open_stream(cfg)?;
    let s = write_http_request(s, "PUT", &cfg.url.host, &path, &headers, Some(body))?;
    let resp = read_http_response(s)?;
    match resp.status {
        200 | 201 | 204 => Ok(()),
        _ => anyhow::bail!("remote blobs PUT failed: HTTP {}", resp.status),
    }
}

pub fn put_action_mapping(cfg: &RemoteConfig, action_key_hex: &str, manifest_sha256_hex: &str) -> anyhow::Result<()> {
    let path = cfg.url.join(&format!("v1/actions/{action_key_hex}"));
    let body = format!(
        "{{\"schema\":\"redo-action-result:v1\",\"manifest_sha256\":\"{}\"}}",
        manifest_sha256_hex
    );
    let mut headers = auth_headers(cfg);
    headers.push(("Content-Type", "application/json".to_string()));
    let s = open_stream(cfg)?;
    let s = write_http_request(s, "PUT", &cfg.url.host, &path, &headers, Some(body.as_bytes()))?;
    let resp = read_http_response(s)?;
    match resp.status {
        200 | 201 | 204 => Ok(()),
        409 => anyhow::bail!("remote action mapping conflict (409)"),
        403 => anyhow::bail!("remote action mapping forbidden (403)"),
        _ => anyhow::bail!("remote actions PUT failed: HTTP {}", resp.status),
    }
}

pub fn post_exec(cfg: &RemoteConfig, body: &[u8]) -> anyhow::Result<Vec<u8>> {
    let path = cfg.url.join("v1/exec");
    let mut headers = auth_headers(cfg);
    headers.push(("Content-Type", "application/json".to_string()));
    let s = open_stream(cfg)?;
    let s = write_http_request(s, "POST", &cfg.url.host, &path, &headers, Some(body))?;
    let resp = read_http_response(s)?;
    match resp.status {
        200 => Ok(resp.body),
        403 => anyhow::bail!("remote exec forbidden (403)"),
        404 => anyhow::bail!("remote exec endpoint not found (404)"),
        501 => anyhow::bail!("remote exec unavailable (501)"),
        _ => anyhow::bail!("remote exec failed: HTTP {}", resp.status),
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactManifest {
    pub blob_sha256: String,
    pub size: u64,
    pub mode: u32,
}

pub fn parse_artifact_manifest_v1(body: &str) -> anyhow::Result<ArtifactManifest> {
    let schema = json_get_string(body, "schema").unwrap_or_default();
    if schema != "redo-artifact-manifest:v1" {
        anyhow::bail!("unsupported manifest schema {:?}", schema);
    }
    let kind = json_get_string(body, "kind").unwrap_or_default();
    if kind != "file" {
        anyhow::bail!("unsupported manifest kind {:?}", kind);
    }
    let digest = json_get_string(body, "digest").unwrap_or_default();
    let blob_sha256 = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow::anyhow!("invalid manifest digest {:?}", digest))?
        .to_string();
    let size = json_get_u64(body, "size").unwrap_or(0);
    let mode = json_get_u64(body, "mode").unwrap_or(0) as u32;
    Ok(ArtifactManifest {
        blob_sha256,
        size,
        mode,
    })
}

