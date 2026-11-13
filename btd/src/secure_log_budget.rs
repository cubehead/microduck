//! Enforce an upper bound before consuming an outbound Noise nonce.
const MAX_PLAINTEXT: usize = 40 * 1024;

pub(crate) fn bound(line: &str) -> Option<std::borrow::Cow<'_, str>> {
    use std::borrow::Cow;
    if line.len() <= MAX_PLAINTEXT {
        return Some(Cow::Borrowed(line));
    }
    let mut response: duck_ipc_proto::Response = serde_json::from_str(line).ok()?;
    let mut tail: duck_ipc_proto::LogsResult =
        serde_json::from_value(response.result.take()?).ok()?;
    tail.truncated = true;
    loop {
        let encoded =
            serde_json::to_string(&duck_ipc_proto::Response::ok(response.id.clone(), &tail))
                .ok()?;
        if encoded.len() <= MAX_PLAINTEXT {
            return Some(Cow::Owned(encoded));
        }
        if tail.lines.is_empty() {
            return None;
        }
        tail.lines.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use duck_ipc_proto::{Id, LogsResult, Response};

    #[test]
    fn trims_oldest_preserving_correlation_and_actual_wire_budget() {
        let mut lines = vec!["x".repeat(2000); 24];
        lines.push("newest".into());
        let line = serde_json::to_string(&Response::ok(
            Some(Id::Number(42)),
            &LogsResult {
                unit: "robotd.service".into(),
                lines,
                truncated: false,
            },
        ))
        .unwrap();
        let reduced = bound(&line).unwrap();
        let decoded: Response = serde_json::from_str(&reduced).unwrap();
        assert_eq!(decoded.id, Some(Id::Number(42)));
        let tail: LogsResult = serde_json::from_value(decoded.result.unwrap()).unwrap();
        assert!(tail.truncated);
        assert_eq!(tail.lines.last().unwrap(), "newest");
        let framed = duck_auth::wire::Frame::Data(vec![0; reduced.len() + 16]);
        assert!(framed.encode(duck_auth::wire::MAX_ROBOT_LINE).is_ok());
    }
}
