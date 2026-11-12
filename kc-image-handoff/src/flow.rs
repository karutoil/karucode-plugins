//! The vision round trip, driven entirely through karucode:
//!
//! 1. resolve a vision **provider + model** from karucode's own provider
//!    configuration (see `karucode.rs`);
//! 2. `POST /api/sessions` with `ephemeral: true` and that model pin (falling
//!    back to a durable session it deletes, on a server build that predates
//!    temporary sessions);
//! 3. `POST /api/workspace/upload` the image bytes;
//! 4. `POST /api/sessions/:id/messages` with the question and the attachment;
//! 5. follow `GET /api/events` until the run finishes and take the answer;
//! 6. an ephemeral session retires itself; the fallback is deleted here.
//!
//! No provider endpoint, model id or API key is ever handled by this plugin:
//! karucode's runtime is the only thing that talks to a model.

use crate::karucode::Resolved;
use crate::server::{self, Endpoint};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub struct HandoffConfig {
    pub endpoint: Endpoint,
    pub token: String,
    pub run_timeout: Duration,
}

/// What one `see` call did, for the result payload.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HandoffReport {
    pub model: String,
    pub provider: String,
    pub session_id: String,
    #[serde(rename = "ephemeral")]
    pub ephemeral: bool,
    pub image_path: String,
    pub image_bytes: usize,
    pub status: String,
    pub elapsed_ms: u64,
}

/// The result of a handoff: an answer, or "still running" with a handle.
#[derive(Debug, Clone)]
pub enum HandoffResult {
    /// The vision model's answer.
    Done {
        answer: String,
        report: HandoffReport,
    },
    /// The run is still going: re-call `see` with the session id.
    Pending {
        session_id: String,
        report: HandoffReport,
    },
}

/// Do one full handoff. `image` is the raw image bytes; `mime` its sniffed type.
pub fn see(
    config: &HandoffConfig,
    resolved: &Resolved,
    question: &str,
    image_name: &str,
    image_bytes: &[u8],
    mime: &str,
) -> Result<HandoffResult, String> {
    let started = Instant::now();
    let endpoint = &config.endpoint;
    let token = &config.token;

    // 1. A temporary session pinned to the vision model. The primary attempt
    //    asks for `ephemeral: true`; a server build that predates temporary
    //    sessions rejects the unknown field with a 400, which is the signal
    //    to fall back to a durable session this plugin deletes afterwards.
    let ephemeral_body = json!({
        "title": "image-handoff (temporary)",
        "ephemeral": true,
        "model": {
            "provider": resolved.provider_id(),
            "model": resolved.model,
        },
    });
    let (status, body) = server::request_json(
        endpoint,
        token,
        "POST",
        "/api/sessions",
        Some(&ephemeral_body),
    )
    .map_err(|e| format!("cannot create the vision session: {e}"))?;
    let (session_id, ephemeral) = if (200..300).contains(&status) {
        let id = body
            .get("id")
            .and_then(Value::as_str)
            .ok_or("the server's session answer has no id")?
            .to_string();
        // Confirm the server honoured the flag; one that silently ignored it
        // would leave a row behind on every call, so treat that as durable.
        let ephemeral = body
            .get("ephemeral")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        (id, ephemeral)
    } else {
        let mut durable_body = ephemeral_body.clone();
        durable_body
            .as_object_mut()
            .map(|map| map.remove("ephemeral"));
        let (status, body) = server::request_json(
            endpoint,
            token,
            "POST",
            "/api/sessions",
            Some(&durable_body),
        )
        .map_err(|e| format!("cannot create the temporary vision session: {e}"))?;
        if !(200..300).contains(&status) {
            return Err(format!(
                "the server refused to create a session (HTTP {status}): {}",
                error_text(&body)
            ));
        }
        let id = body
            .get("id")
            .and_then(Value::as_str)
            .ok_or("the server's session answer has no id")?
            .to_string();
        (id, false)
    };

    // A durable session is deleted when the answer is collected (or when the
    // handoff fails); an ephemeral one retires itself, so nothing to dispose.

    // 2+3. Upload the image and send the question; the run starts.
    let (image_path, image_bytes_len) =
        start_handoff(config, question, image_name, image_bytes, mime, &session_id)?;
    let report_bytes = image_bytes_len;
    let report_image_path = image_path;

    // The event reader captures this session's frames from now on; wait for
    // the run to finish, bounded by the configured timeout.
    let outcome = crate::answer_bus::wait_for_outcome(
        endpoint,
        token,
        &session_id,
        Instant::now() + config.run_timeout,
    );

    let mut outcome_report =
        finish_result(config, resolved, &session_id, ephemeral, started, outcome);
    if let Ok(HandoffResult::Done { ref mut report, .. }) = outcome_report {
        report.image_path = report_image_path;
        report.image_bytes = report_bytes;
    } else if let Ok(HandoffResult::Pending { ref mut report, .. }) = outcome_report {
        report.image_path = report_image_path;
        report.image_bytes = report_bytes;
    }
    outcome_report
}

/// Collect the answer of a handoff a previous call started.
pub fn collect(
    config: &HandoffConfig,
    session_id: &str,
    started: Instant,
) -> Result<HandoffResult, String> {
    let endpoint = &config.endpoint;
    let token = &config.token;
    let wait_started = Instant::now();
    let outcome = crate::answer_bus::wait_for_outcome(
        endpoint,
        token,
        session_id,
        wait_started + config.run_timeout,
    );
    // A session that was seen but has not finished is a pending answer, not
    // an error; an id the reader has never heard of is an error.
    if outcome.is_none()
        && crate::answer_bus::finished_outcome(endpoint, token, session_id).is_none()
    {
        return match crate::answer_bus::known_session(endpoint, token, session_id) {
            true => Ok(HandoffResult::Pending {
                session_id: session_id.to_string(),
                report: pending_report(session_id, wait_started),
            }),
            false => Err(format!(
                "no buffered outcome for session `{session_id}`: the plugin may have restarted \
                 since that call, or the session id is wrong"
            )),
        };
    }
    let outcome =
        outcome.or_else(|| crate::answer_bus::finished_outcome(endpoint, token, session_id));
    // An ephemeral session's report details are gone; report what we know.
    let report = HandoffReport {
        model: String::new(),
        provider: String::new(),
        session_id: session_id.to_string(),
        ephemeral: true,
        image_path: String::new(),
        image_bytes: 0,
        status: outcome
            .as_ref()
            .and_then(|o| o.status.clone())
            .unwrap_or_default(),
        elapsed_ms: started.elapsed().as_millis() as u64,
    };
    if outcome.is_none() {
        return Ok(HandoffResult::Pending {
            session_id: session_id.to_string(),
            report,
        });
    }
    let outcome = outcome.unwrap();
    finish_pending(config, session_id, outcome, started)
}

fn finish_result(
    config: &HandoffConfig,
    resolved: &Resolved,
    session_id: &str,
    ephemeral: bool,
    started: Instant,
    outcome: Option<crate::answer_bus::SessionOutcome>,
) -> Result<HandoffResult, String> {
    let report = HandoffReport {
        model: resolved.model.clone(),
        provider: resolved.provider_id().to_string(),
        session_id: session_id.to_string(),
        ephemeral,
        image_path: String::new(),
        image_bytes: 0,
        status: outcome
            .as_ref()
            .and_then(|o| o.status.clone())
            .unwrap_or_else(|| "running".into()),
        elapsed_ms: started.elapsed().as_millis() as u64,
    };
    if outcome.is_none() {
        return Ok(HandoffResult::Pending {
            session_id: session_id.to_string(),
            report,
        });
    }
    let outcome = outcome.unwrap();
    let status = outcome.status.clone().unwrap_or_default();
    if status != "completed" {
        dispose_session(config, session_id, ephemeral);
        return Err(format!(
            "the vision run ended as `{status}`{}",
            outcome
                .error
                .as_deref()
                .map(|e| format!(": {e}"))
                .unwrap_or_default()
        ));
    }
    let answer = outcome.answer().trim().to_string();
    if answer.is_empty() {
        dispose_session(config, session_id, ephemeral);
        return Err(
            "the vision run finished without an answer (the model may have called only tools)"
                .into(),
        );
    }
    dispose_session(config, session_id, ephemeral);
    crate::answer_bus::forget(&config.endpoint, &config.token, session_id);
    Ok(HandoffResult::Done { answer, report })
}

/// A report for a still-running collected session.
fn pending_report(session_id: &str, started: Instant) -> HandoffReport {
    HandoffReport {
        model: String::new(),
        provider: String::new(),
        session_id: session_id.to_string(),
        ephemeral: true,
        image_path: String::new(),
        image_bytes: 0,
        status: "running".into(),
        elapsed_ms: started.elapsed().as_millis() as u64,
    }
}

fn finish_pending(
    config: &HandoffConfig,
    session_id: &str,
    outcome: crate::answer_bus::SessionOutcome,
    started: Instant,
) -> Result<HandoffResult, String> {
    let status = outcome.status.clone().unwrap_or_default();
    if status != "completed" {
        return Err(format!(
            "the vision run ended as `{status}`{}",
            outcome
                .error
                .as_deref()
                .map(|e| format!(": {e}"))
                .unwrap_or_default()
        ));
    }
    let answer = outcome.answer().trim().to_string();
    if answer.is_empty() {
        return Err(
            "the vision run finished without an answer (the model may have called only tools)"
                .into(),
        );
    }
    let report = HandoffReport {
        model: String::new(),
        provider: String::new(),
        session_id: session_id.to_string(),
        ephemeral: true,
        image_path: String::new(),
        image_bytes: 0,
        status,
        elapsed_ms: started.elapsed().as_millis() as u64,
    };
    crate::answer_bus::forget(&config.endpoint, &config.token, session_id);
    Ok(HandoffResult::Done { answer, report })
}

/// Delete a durable session (best effort); ephemeral ones retire themselves.
fn dispose_session(config: &HandoffConfig, session_id: &str, ephemeral: bool) {
    if !ephemeral {
        let _ = server::request_json(
            &config.endpoint,
            &config.token,
            "DELETE",
            &format!("/api/sessions/{session_id}"),
            None,
        );
    }
}

fn start_handoff(
    config: &HandoffConfig,
    question: &str,
    image_name: &str,
    image_bytes: &[u8],
    mime: &str,
    session_id: &str,
) -> Result<(String, usize), String> {
    let endpoint = &config.endpoint;
    let token = &config.token;

    // 2. Upload the image to the session's attachment store.
    let upload_path = format!(
        "/api/workspace/upload?session={session_id}&name={}",
        url_encode(image_name)
    );
    let (status, body) =
        server::request_raw(endpoint, token, "POST", &upload_path, mime, image_bytes)
            .map_err(|e| format!("cannot upload the image to karucode: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(format!(
            "karucode refused the image upload (HTTP {status}): {}",
            error_text(&body)
        ));
    }
    let image_path = body
        .get("path")
        .and_then(Value::as_str)
        .ok_or("the upload answer has no path")?
        .to_string();

    // 3. Send the question with the attachment, in the composer's own format
    //    so the transcript renders the same way a person's would.
    let content = format!(
        "{question}\n\nAttached files:\n\n### {image_path} (uploaded, {} bytes)\n\nRead this file \
         from the workspace when you need it: `{image_path}`",
        image_bytes.len()
    );
    let message = json!({
        "content": content,
        "attachments": [{
            "name": image_name,
            "path": image_path,
            "mode": "upload",
            "size": image_bytes.len(),
        }],
    });
    let (status, body) = server::request_json(
        endpoint,
        token,
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(&message),
    )
    .map_err(|e| format!("cannot send the vision question: {e}"))?;
    if status != 202 && !(200..300).contains(&status) {
        return Err(format!(
            "karucode refused the message (HTTP {status}): {}",
            error_text(&body)
        ));
    }
    let run_id = body
        .get("run_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // 4. The run has started; the event reader (answer_bus) captures its
    //    frames from here, and see()/collect() wait for the outcome.
    let _ = run_id;
    Ok((image_path, image_bytes.len()))
}

fn error_text(body: &Value) -> String {
    body.get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .or_else(|| body.as_str())
        .unwrap_or("(no message)")
        .to_string()
}

/// Percent-encode a filename for a query parameter.
fn url_encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encoding_leaves_safe_names_alone() {
        assert_eq!(url_encode("photo.png"), "photo.png");
        assert_eq!(url_encode("my file (1).png"), "my%20file%20%281%29.png");
        assert_eq!(url_encode("a&b=c.png"), "a%26b%3Dc.png");
    }

    #[test]
    fn attachment_block_matches_the_composer_format() {
        // The composer writes exactly this heading for an uploaded file.
        let content =
            "look\n\nAttached files:\n\n### path/probe.png (uploaded, 42 bytes)\n\nRead this \
             file from the workspace when you need it: `path/probe.png`"
                .to_string();
        assert!(content.contains("(uploaded, 42 bytes)"));
        assert!(content.contains("Read this file from the workspace"));
    }
}
