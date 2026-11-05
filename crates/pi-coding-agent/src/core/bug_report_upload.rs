//! Port of `packages/coding-agent/src/core/bug-report-upload.ts`.

use crate::core::bug_report::{BugReportBundle, bug_report_files};
use crate::core::radius::get_radius_gateway_url;
use pi_ai::utils::abort::SignalAbortError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Default)]
pub struct UploadBugReportOptions {
    pub token: Option<String>,
    pub signal: Option<CancellationToken>,
    pub gateway_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadBugReportResult {
    pub id: String,
}

/// `multipart/form-data` body with one file part per bug report file.
fn multipart_body(bundle: &BugReportBundle, boundary: &str) -> Vec<u8> {
    let mut body = Vec::new();
    for file in bug_report_files(bundle) {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{0}\"; filename=\"{0}\"\r\nContent-Type: {1}\r\n\r\n",
                file.name, file.content_type
            )
            .as_bytes(),
        );
        body.extend_from_slice(file.data.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

/// Upload a report as multipart form data, anonymously or attributed to a Radius account.
pub async fn upload_bug_report(
    bundle: &BugReportBundle,
    options: Option<&UploadBugReportOptions>,
) -> anyhow::Result<UploadBugReportResult> {
    let options = options.cloned().unwrap_or_default();
    let gateway = options.gateway_url.clone().unwrap_or_else(get_radius_gateway_url);
    let url = url::Url::parse(&gateway)?.join("/v1/bug-reports")?;
    let boundary = format!("----pi-bug-report-{}", uuid::Uuid::new_v4().simple());
    let client = pi_ai::utils::http::client_for(url.as_str(), None)?;
    let mut request = client
        .post(url.as_str())
        .header("Content-Type", format!("multipart/form-data; boundary={boundary}"))
        .body(multipart_body(bundle, &boundary));
    if let Some(token) = options.token.as_deref().filter(|t| !t.is_empty()) {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let send = async {
        let response = request.send().await?;
        let status = response.status();
        let json: Option<Value> = response.json().await.ok();
        anyhow::Ok((status, json))
    };
    let (status, json) = match &options.signal {
        Some(signal) => tokio::select! {
            result = send => result?,
            _ = signal.cancelled() => return Err(SignalAbortError.into()),
        },
        None => send.await?,
    };
    let ok = json.as_ref().and_then(|j| j.get("ok")).and_then(Value::as_bool);
    if status.is_success()
        && ok == Some(true)
        && let Some(id) = json
            .as_ref()
            .and_then(|j| j.pointer("/bug_report/id"))
            .and_then(Value::as_str)
    {
        return Ok(UploadBugReportResult { id: id.to_string() });
    }
    let detail = json
        .as_ref()
        .filter(|_| ok != Some(true))
        .and_then(|j| {
            j.get("description")
                .and_then(Value::as_str)
                .filter(|d| !d.is_empty())
                .or_else(|| j.get("error").and_then(Value::as_str))
        })
        .filter(|d| !d.is_empty())
        .map(str::to_string);
    let fallback = status
        .canonical_reason()
        .map(str::to_string)
        .unwrap_or_else(|| status.as_u16().to_string());
    anyhow::bail!("Bug report upload failed: {}", detail.unwrap_or(fallback))
}
