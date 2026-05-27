use std::time::{Duration, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use rmux_proto::{CommandOutput, WebShareSummary};

pub(super) fn created_output(
    read_url: &str,
    pairing_code: Option<&str>,
    expires_at_unix: Option<u64>,
    kill_session_on_expire: bool,
) -> CommandOutput {
    let mut output = String::new();
    output.push_str("read ");
    output.push_str(read_url);
    output.push('\n');
    if let Some(expires_at_unix) = expires_at_unix {
        output.push_str("share expires at ");
        output.push_str(&format_unix_rfc3339(expires_at_unix));
        output.push('\n');
    }
    if kill_session_on_expire {
        output.push_str("session will be killed on expiry\n");
    }
    if let Some(pairing_code) = pairing_code {
        output.push_str("pin ");
        output.push_str(pairing_code);
        output.push('\n');
    }
    CommandOutput::from_stdout(output)
}

pub(super) fn list_output(shares: &[WebShareSummary]) -> CommandOutput {
    let mut output = String::new();
    for share in shares {
        output.push_str(&share.share_id);
        output.push(' ');
        output.push_str(&share.scope.to_string());
        output.push(' ');
        output.push_str(share.read_url.as_deref().unwrap_or("-"));
        output.push('\n');
    }
    CommandOutput::from_stdout(output)
}

pub(super) fn lookup_output(share: Option<&WebShareSummary>) -> CommandOutput {
    match share {
        Some(share) => list_output(std::slice::from_ref(share)),
        None => CommandOutput::from_stdout(Vec::new()),
    }
}

pub(super) fn stopped_output(share_id: &str, stopped: bool) -> CommandOutput {
    let status = if stopped { "stopped" } else { "missing" };
    CommandOutput::from_stdout(format!("{status} {share_id}\n"))
}

fn format_unix_rfc3339(value: u64) -> String {
    DateTime::<Utc>::from(UNIX_EPOCH + Duration::from_secs(value)).to_rfc3339()
}
