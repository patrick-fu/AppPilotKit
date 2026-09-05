use apppilotkit_host_runtime::{
    CloseReason, Closed, ControlFailure, ControlPacketDecoder, ControlRequest, ControlResult,
    ControlSuccess, ErrorKind, ErrorStage, ExchangeBody, ExchangeComplete, HandoffState,
    OpenSessionBody, Platform, PrepareBody, ReadyReference, ReadyTarget, Request, SessionOpened,
    SideEffect, decode_request_packet, decode_result_packet, encode_failure_packet,
    encode_request_packet, encode_success_packet,
};
use sha2::{Digest, Sha256};

fn hex(value: &str) -> Vec<u8> {
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).expect("hex literal"))
        .collect()
}

#[test]
fn ready_reference_has_one_canonical_argv_representation() {
    let reference = ReadyReference::from_token([0x61; 32]);
    assert_eq!(
        reference.to_string(),
        "target_YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE"
    );
    assert_eq!(
        ReadyReference::parse(&reference.to_string())
            .unwrap()
            .token(),
        [0x61; 32]
    );
    for invalid in [
        "YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE",
        "target_YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE=",
        "target_YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWF",
    ] {
        assert_eq!(
            ReadyReference::parse(invalid)
                .expect_err("non-canonical")
                .kind,
            ErrorKind::TargetSelectionRequired
        );
    }
}

#[test]
fn incremental_frame_times_out_and_never_accepts_a_second_packet() {
    let mut decoder = ControlPacketDecoder::new();
    assert!(decoder.push(&[0, 0]).expect("partial header").is_none());
    assert_eq!(
        decoder.timeout().expect_err("2s deadline").close_reason,
        CloseReason::Timeout
    );

    let packet = hex(
        "0000005ea500010150000102030405060708090a0b0c0d0e0f02192710030004a50000016373696d02676465762e617070036c2f746d702f4170702e6170700458201111111111111111111111111111111111111111111111111111111111111111",
    );
    let mut decoder = ControlPacketDecoder::new();
    assert!(decoder.push(&packet[..4]).unwrap().is_none());
    assert!(decoder.push(&packet[4..]).unwrap().is_some());
    assert_eq!(
        decoder
            .push(&packet)
            .expect_err("second packet")
            .close_reason,
        CloseReason::SequenceViolation
    );
}

#[test]
fn accepted_target_ready_literal_encodes_byte_for_byte() {
    let cbor = hex(
        "a4000101500000000000000000000000000000000002f503a60000015820000000000000000000000000000000000000000000000000000000000000000003010401051903e806197918",
    );
    let mut expected = Vec::with_capacity(cbor.len() + 4);
    expected.extend_from_slice(&(cbor.len() as u32).to_be_bytes());
    expected.extend_from_slice(&cbor);
    let actual = encode_success_packet(
        [0; 16],
        ControlSuccess::TargetReady(ReadyTarget {
            target_token: [0; 32],
            process_generation: 1,
            listener_epoch: 1,
            issued_at_unix_ms: 1_000,
            expires_at_unix_ms: 31_000,
        }),
    )
    .expect("accepted literal");
    assert_eq!(actual, expected);
    let decoded = decode_result_packet(&expected).expect("accepted result literal");
    let ControlResult::Success {
        request_id,
        result: ControlSuccess::TargetReady(ready),
    } = decoded
    else {
        panic!("target-ready result")
    };
    assert_eq!(request_id, [0; 16]);
    assert_eq!(ready.expires_at_unix_ms, 31_000);
}

#[test]
fn safe_failure_literal_decodes_without_peer_payload() {
    // {0:1,1:h'00'*16,2:false,3:{0:1,1:"expired",2:false,3:5,4:1,5:3}}
    let packet = hex(
        "0000002ca4000101500000000000000000000000000000000002f403a6000101676578706972656402f4030504010503",
    );
    let ControlResult::Failure { request_id, error } =
        decode_result_packet(&packet).expect("failure literal")
    else {
        panic!("failure")
    };
    assert_eq!(request_id, [0; 16]);
    assert_eq!(error.kind, ErrorKind::SessionExpired);
    assert_eq!(error.close_reason, CloseReason::Stale);
}

#[test]
fn stock_bootstrap_failure_packet_is_byte_exact() {
    let failure = ControlFailure {
        kind: ErrorKind::SessionExpired,
        message: "Target session expired",
        retryable: false,
        stage: ErrorStage::Bootstrap,
        handoff: HandoffState::NotHandedOff,
        close_reason: CloseReason::BindingMismatch,
    };
    let expected = hex(
        "0000003ba4000101500000000000000000000000000000000002f403a6000101765461726765742073657373696f6e206578706972656402f4030204000502",
    );
    let packet = encode_failure_packet([0; 16], &failure).expect("stock failure packet");
    assert_eq!(packet, expected);
    assert_eq!(
        decode_result_packet(&packet),
        Ok(ControlResult::Failure {
            request_id: [0; 16],
            error: failure,
        })
    );
}

#[cfg(feature = "internal-diagnostics")]
#[test]
fn diagnostic_markers_are_the_only_extra_stock_messages() {
    for marker in [
        "bootstrap_adapter_rejected",
        "bootstrap_ack_binding_mismatch",
        "target_no_session_frames",
        "lease_terminal_before_session_commit",
    ] {
        let failure = ControlFailure {
            kind: ErrorKind::SessionExpired,
            message: marker,
            retryable: false,
            stage: ErrorStage::Bootstrap,
            handoff: HandoffState::NotHandedOff,
            close_reason: CloseReason::BindingMismatch,
        };
        let packet = encode_failure_packet([0x44; 16], &failure).expect("marker packet");
        assert_eq!(
            decode_result_packet(&packet),
            Ok(ControlResult::Failure {
                request_id: [0x44; 16],
                error: failure,
            })
        );
    }
}

#[cfg(not(feature = "internal-diagnostics"))]
#[test]
fn diagnostic_markers_are_not_stock_messages_by_default() {
    for marker in [
        "bootstrap_adapter_rejected",
        "bootstrap_ack_binding_mismatch",
        "target_no_session_frames",
        "lease_terminal_before_session_commit",
    ] {
        let failure = ControlFailure {
            kind: ErrorKind::SessionExpired,
            message: marker,
            retryable: false,
            stage: ErrorStage::Bootstrap,
            handoff: HandoffState::NotHandedOff,
            close_reason: CloseReason::BindingMismatch,
        };
        let packet = encode_failure_packet([0x45; 16], &failure).expect("marker packet");
        let ControlResult::Failure { error, .. } =
            decode_result_packet(&packet).expect("marker packet decodes")
        else {
            panic!("marker packet must remain a failure");
        };
        assert_eq!(error.message, "Peer returned a safe Broker failure");
    }
}

#[test]
fn packet_rejects_duplicate_key_before_dispatch() {
    let cbor = hex("a200010002");
    let mut packet = Vec::new();
    packet.extend_from_slice(&(cbor.len() as u32).to_be_bytes());
    packet.extend_from_slice(&cbor);
    let error = decode_request_packet(&packet).expect_err("duplicate key");
    assert_eq!(error.close_reason, CloseReason::Malformed);
}

#[test]
fn packet_rejects_trailing_bytes_and_cap_plus_one() {
    let mut trailing = vec![0, 0, 0, 1, 0, 0];
    let error = decode_request_packet(&trailing).expect_err("trailing byte");
    assert_eq!(error.close_reason, CloseReason::Malformed);

    trailing[..4].copy_from_slice(&67_109_121_u32.to_be_bytes());
    trailing.truncate(4);
    let error = decode_request_packet(&trailing).expect_err("global cap");
    assert_eq!(error.close_reason, CloseReason::Oversize);
}

#[test]
fn prepare_literal_decodes_to_typed_request() {
    // {0:1,1:h'000102030405060708090a0b0c0d0e0f',2:10000,3:0,
    //  4:{0:0,1:"sim",2:"dev.app",3:"/tmp/App.app",4:h'11'*32}}
    let packet = hex(
        "0000005ea500010150000102030405060708090a0b0c0d0e0f02192710030004a50000016373696d02676465762e617070036c2f746d702f4170702e6170700458201111111111111111111111111111111111111111111111111111111111111111",
    );
    let request = decode_request_packet(&packet).expect("prepare literal");
    assert_eq!(
        encode_request_packet(&request).expect("canonical request encoder"),
        packet
    );
    let ControlRequest::Prepare(request) = request else {
        panic!("expected prepare request");
    };
    assert_eq!(
        request.request_id,
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
    );
    assert_eq!(request.deadline_unix_ms, 10_000);
    assert_eq!(request.body.device_selector, "sim");
    assert_eq!(request.body.app_artifact, "/tmp/App.app");
}

#[test]
fn incremental_decoder_rejects_declared_operation_cap_before_body_allocation() {
    let requests = [
        (
            ControlRequest::Prepare(Request {
                request_id: [1; 16],
                deadline_unix_ms: 10_000,
                body: PrepareBody {
                    platform: Platform::IosSimulator,
                    device_selector: "sim".to_owned(),
                    app_id: "dev.app".to_owned(),
                    app_artifact: "/tmp/App.app".to_owned(),
                    app_artifact_sha256: [0x11; 32],
                },
            }),
            8_193_u32,
        ),
        (
            ControlRequest::OpenSession(Request {
                request_id: [2; 16],
                deadline_unix_ms: 10_000,
                body: OpenSessionBody {
                    target_token: [0x22; 32],
                    session_id: None,
                    required_capabilities: vec!["semantic.catalog".to_owned()],
                    session_open_request: Some(b"open".to_vec()),
                    session_open_request_sha256: Some(Sha256::digest(b"open").into()),
                },
            }),
            73_729_u32,
        ),
        (
            ControlRequest::Exchange(Request {
                request_id: [3; 16],
                deadline_unix_ms: 10_000,
                body: ExchangeBody {
                    target_token: [0x22; 32],
                    session_id: "session-agent-a-01".to_owned(),
                    process_generation: 1,
                    listener_epoch: 1,
                    message: b"read".to_vec(),
                    message_sha256: Sha256::digest(b"read").into(),
                    side_effect: SideEffect::ReadOnly,
                },
            }),
            16_777_481_u32,
        ),
    ];
    for (request, declared) in requests {
        let mut prefix = encode_request_packet(&request).unwrap();
        prefix[..4].copy_from_slice(&declared.to_be_bytes());
        let error = ControlPacketDecoder::new()
            .push(&prefix)
            .expect_err("declared operation cap rejected from header prefix");
        assert_eq!(error.close_reason, CloseReason::Oversize);
    }
}

#[test]
fn success_encoder_rejects_invalid_typed_values_before_emitting_bytes() {
    let invalid = [
        ControlSuccess::TargetReady(ReadyTarget {
            target_token: [0; 32],
            process_generation: 0,
            listener_epoch: 1,
            issued_at_unix_ms: 1_000,
            expires_at_unix_ms: 31_001,
        }),
        ControlSuccess::SessionOpened(SessionOpened {
            target_token: [0; 32],
            response: Vec::new(),
            response_sha256: [0; 32],
            process_generation: 1,
            listener_epoch: 1,
            handoff: HandoffState::NotHandedOff,
        }),
        ControlSuccess::ExchangeComplete(ExchangeComplete {
            target_token: [0; 32],
            session_id: "bad".to_owned(),
            process_generation: 1,
            listener_epoch: 1,
            message: b"response".to_vec(),
            message_sha256: [0; 32],
            handoff: HandoffState::NotHandedOff,
        }),
        ControlSuccess::Closed(Closed {
            target_token: [0; 32],
            session_id: Some("bad".to_owned()),
            handoff: HandoffState::NotHandedOff,
        }),
    ];
    for value in invalid {
        assert!(encode_success_packet([0; 16], value).is_err());
    }
}

#[test]
fn worked_success_literals_are_byte_exact() {
    let values = [
        ControlSuccess::SessionOpened(SessionOpened {
            target_token: [0x22; 32],
            response: b"open".to_vec(),
            response_sha256: hex(
                "2348f998744212575d85959674f9607ab26f67708a917157472832386337c904",
            )
            .try_into()
            .unwrap(),
            process_generation: 1,
            listener_epoch: 1,
            handoff: HandoffState::NotHandedOff,
        }),
        ControlSuccess::ExchangeComplete(ExchangeComplete {
            target_token: [0x22; 32],
            session_id: "session-agent-a-01".to_owned(),
            process_generation: 1,
            listener_epoch: 1,
            message: b"response".to_vec(),
            message_sha256: hex("a9f4b3d22a523fdada41c85c175425bcd15b32b4cd0f54d9433accd52d7195a1")
                .try_into()
                .unwrap(),
            handoff: HandoffState::HandoffPossibleOrConfirmed,
        }),
        ControlSuccess::Closed(Closed {
            target_token: [0x22; 32],
            session_id: Some("session-agent-a-01".to_owned()),
            handoff: HandoffState::NotHandedOff,
        }),
    ];
    let literals = [
        "0000006da4000101500000000000000000000000000000000002f503a70001015820222222222222222222222222222222222222222222222222222222222222222202446f70656e0358202348f998744212575d85959674f9607ab26f67708a917157472832386337c904040105010600",
        "00000085a4000101500000000000000000000000000000000002f503a800020158202222222222222222222222222222222222222222222222222222222222222222027273657373696f6e2d6167656e742d612d3031030104010548726573706f6e7365065820a9f4b3d22a523fdada41c85c175425bcd15b32b4cd0f54d9433accd52d7195a10701",
        "00000054a4000101500000000000000000000000000000000002f503a400030158202222222222222222222222222222222222222222222222222222222222222222027273657373696f6e2d6167656e742d612d30310700",
    ];
    for (value, literal) in values.into_iter().zip(literals) {
        let packet = encode_success_packet([0; 16], value.clone()).unwrap();
        assert_eq!(packet, hex(literal));
        assert_eq!(
            decode_result_packet(&packet).unwrap(),
            ControlResult::Success {
                request_id: [0; 16],
                result: value,
            }
        );
    }
}
