// Copyright 2026 Adrian Mârza and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

use micro_http::{Body, StatusCode};
use serde::Deserialize;
use vmm::execution::ExecutionConfig;
use vmm::rpc_interface::VmmAction;

use crate::api_server::parsed_request::{ParsedRequest, RequestError};

pub(crate) fn parse_put_execution(body: &Body) -> Result<ParsedRequest, RequestError> {
    let config = serde_json::from_slice::<ExecutionConfig>(body.raw())?;
    if config.evidence_path.as_os_str().is_empty()
        || config
            .replay_trace_path
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
    {
        return Err(RequestError::Generic(
            StatusCode::BadRequest,
            "execution paths cannot be empty".into(),
        ));
    }
    Ok(ParsedRequest::new_sync(VmmAction::ConfigureExecution(
        config,
    )))
}

pub(crate) fn parse_patch_execution(body: &Body) -> Result<ParsedRequest, RequestError> {
    let value: serde_json::Value = serde_json::from_slice(body.raw())?;
    if !value.as_object().is_some_and(|object| object.is_empty()) {
        return Err(RequestError::Generic(StatusCode::BadRequest,
            "execution flush requires an empty JSON object".into()));
    }
    Ok(ParsedRequest::new_sync(VmmAction::FlushExecutionEvidence))
}

pub(crate) fn parse_put_execution_checkpoint(body: &Body) -> Result<ParsedRequest, RequestError> {
    use vmm::checkpoint::{CreateCheckpointConfig, LoadCheckpointConfig};
    #[derive(Deserialize)]
    #[serde(tag = "action_type", deny_unknown_fields)]
    enum Action {
        Create(CreateCheckpointConfig),
        Load(LoadCheckpointConfig),
    }
    let bad = || RequestError::Generic(StatusCode::BadRequest,
        "checkpoint paths must be nonempty and identity must be lowercase SHA-256".into());
    let action = match serde_json::from_slice::<Action>(body.raw())? {
        Action::Create(config) => {
            if config.directory.as_os_str().is_empty() { return Err(bad()); }
            VmmAction::CreateExecutionCheckpoint(config)
        }
        Action::Load(config) => {
            let hash = config.checkpoint_sha256.as_bytes();
            if config.directory.as_os_str().is_empty() || config.serial_out_path.as_os_str().is_empty()
                || config.execution.evidence_path.as_os_str().is_empty()
                || config.execution.replay_trace_path.as_ref().is_some_and(|path| path.as_os_str().is_empty())
                || hash.len() != 64 || !hash.iter().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)) {
                return Err(bad());
            }
            VmmAction::LoadExecutionCheckpoint(config)
        }
    };
    Ok(ParsedRequest::new_sync(action))
}

pub(crate) fn parse_put_serial_input(body: &Body) -> Result<ParsedRequest, RequestError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input {
        data_hex: String,
    }
    let input = serde_json::from_slice::<Input>(body.raw())?;
    let hex = input.data_hex.as_bytes();
    if hex.is_empty()
        || hex.len() > 32768
        || hex.len() % 2 != 0
        || !hex
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        return Err(RequestError::Generic(
            StatusCode::BadRequest,
            "serial input must be nonempty lowercase hex, at most 16384 bytes".into(),
        ));
    }
    let digit = |byte: u8| {
        if byte <= b'9' {
            byte - b'0'
        } else {
            byte - b'a' + 10
        }
    };
    let data = hex
        .chunks_exact(2)
        .map(|pair| digit(pair[0]) * 16 + digit(pair[1]))
        .collect();
    Ok(ParsedRequest::new_sync(VmmAction::SendSerialInput(data)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_server::parsed_request::tests::vmm_action_from_request;

    #[test]
    fn checkpoint_requests_are_typed_and_cannot_supply_an_execution_origin() {
        assert!(matches!(vmm_action_from_request(parse_put_execution_checkpoint(&Body::new(
            r#"{"action_type":"Create","directory":"checkpoint"}"#)).unwrap()), VmmAction::CreateExecutionCheckpoint(_)));
        let load = serde_json::json!({"action_type":"Load", "directory":"checkpoint",
            "checkpoint_sha256":"a".repeat(64), "serial_out_path":"resumed.log",
            "execution":{"evidence_path":"execution.json", "replay_trace_path":null}});
        assert!(matches!(vmm_action_from_request(parse_put_execution_checkpoint(&Body::new(load.to_string())).unwrap()), VmmAction::LoadExecutionCheckpoint(_)));
        for field in ["directory", "serial_out_path", "checkpoint_sha256"] {
            let mut invalid = load.clone();
            invalid[field] = "".into();
            assert!(parse_put_execution_checkpoint(&Body::new(invalid.to_string())).is_err());
        }
        let mut invalid = load;
        invalid["execution"]["start"] = serde_json::json!({"kind":"checkpoint"});
        assert!(parse_put_execution_checkpoint(&Body::new(invalid.to_string())).is_err());
        assert!(parse_put_execution_checkpoint(&Body::new(r#"{"action_type":"Resume","directory":"x"}"#)).is_err());
    }

    #[test]
    fn serial_payload_is_canonical_and_bounded() {
        assert_eq!(
            vmm_action_from_request(
                parse_put_serial_input(&Body::new(r#"{"data_hex":"2a0a"}"#)).unwrap()
            ),
            VmmAction::SendSerialInput(vec![42, 10])
        );
        for hex in ["", "2", "AF", "gg", "2a 0a"] {
            let body = format!(r#"{{"data_hex":"{hex}"}}"#);
            assert!(parse_put_serial_input(&Body::new(body)).is_err());
        }
        let body = format!(r#"{{"data_hex":"{}"}}"#, "00".repeat(16385));
        assert!(parse_put_serial_input(&Body::new(body)).is_err());
        let body = format!(r#"{{"data_hex":"{}"}}"#, "00".repeat(16384));
        assert!(parse_put_serial_input(&Body::new(body)).is_ok());
    }

    #[test]
    fn flush_accepts_only_an_empty_object() {
        assert_eq!(
            vmm_action_from_request(parse_patch_execution(&Body::new("{}")).unwrap()),
            VmmAction::FlushExecutionEvidence
        );
        for body in ["null", "[]", r#"{"overwrite":true}"#] {
            assert!(parse_patch_execution(&Body::new(body)).is_err());
        }
    }
}
