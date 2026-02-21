//! Remote execution client protocol (Phase 4).
//!
//! This uses the same HTTP server as the remote artifact cache (`redo-cache-server`)
//! with an additional `POST /v1/exec` endpoint when enabled.

use serde::{Deserialize, Serialize};

use crate::remote_cache;

#[derive(Debug, Clone, Serialize)]
pub struct ExecInput {
    pub path: String,   // workspace-relative
    pub sha256: String, // hex
    pub mode: u32,      // permission bits
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecRequest {
    pub schema: String,
    pub argv: Vec<String>,
    pub cwd_rel: String,
    pub target_rel: String,
    pub tmp_rel: String,
    pub deps_out0_rel: String,
    pub trace_out0_rel: String,
    pub inputs: Vec<ExecInput>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExecResponse {
    pub schema: String,
    pub exit_code: i32,
    pub output_manifest_sha256: Option<String>,
    pub deps_sha256: Option<String>,
    pub trace_sha256: Option<String>,
    pub stdout_sha256: Option<String>,
    pub stderr_sha256: Option<String>,
}

pub fn exec_v1(cfg: &remote_cache::RemoteConfig, req: &ExecRequest) -> anyhow::Result<ExecResponse> {
    let body = serde_json::to_vec(req)?;
    let resp_body = remote_cache::post_exec(cfg, &body)?;
    let resp: ExecResponse = serde_json::from_slice(&resp_body)?;
    if resp.schema != "redo-remote-exec-result:v1" {
        anyhow::bail!("unsupported exec response schema {:?}", resp.schema);
    }
    Ok(resp)
}

