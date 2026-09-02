use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonschema::Retrieve;
use minicbor::Encoder;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use snow::{Builder, HandshakeState, TransportState, params::NoiseParams};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

mod verifier;

type AnyResult<T> = Result<T, Box<dyn Error>>;

const CONTRACT_VERSION: &str = "1.0";
const NK: &str = "Noise_NK_25519_ChaChaPoly_SHA256";
const NNPSK0: &str = "Noise_NNpsk0_25519_ChaChaPoly_SHA256";
const PROCESS_GENERATION: u64 = 4_503_599_627_370_123;
const LISTENER_EPOCH: u64 = 1;
const EXPIRY_UNIX_MS: u64 = 1_893_456_000_000;
const REQUEST_LIMIT: u64 = 16_777_216;
const RESPONSE_LIMIT: u64 = 67_108_864;
const HANDSHAKE_LIMIT: u64 = 8_192;
const MAX_BROKER_CBOR_BYTES: u64 = 67_109_120;
// Every constant below is synthetic TEST-ONLY material. Production use is forbidden.
const TEST_CANARY: &str = "APPPILOTKIT_TEST_ONLY_SECRET_CANARY_7f9c4b2e";
const EXECUTION_CANARY_EXAMPLE: &str = "APPPILOTKIT_EXECUTION_CANARY_0123456789abcdef";
const PBS: [u8; 32] = [0x41; 32];
const BROKER_STATIC_PRIVATE: [u8; 32] = [0x11; 32];
const NK_TARGET_EPHEMERAL: [u8; 32] = [0x21; 32];
const NK_BROKER_EPHEMERAL: [u8; 32] = [0x31; 32];
const SESSION_TARGET_EPHEMERAL: [u8; 32] = [0x91; 32];
const SESSION_TARGET_EPHEMERAL_B: [u8; 32] = [0x92; 32];
const SESSION_BROKER_EPHEMERAL: [u8; 32] = [0xa1; 32];
const TARGET_REFERENCE_RANDOM: [u8; 32] = [0x61; 32];
const ALT_TARGET_REFERENCE_RANDOM: [u8; 32] = [0x62; 32];
const LEASE_ID: [u8; 16] = [0x51; 16];
const ALT_LEASE_ID: [u8; 16] = [0x52; 16];
const TARGET_NONCE: [u8; 32] = [0x71; 32];
const APP_DIGEST: [u8; 32] = [0x81; 32];
const ANDROID_LOCALABSTRACT: &str = "apppilotkit-android-bootstrap-0123456789abcdef";

#[derive(Clone)]
struct CryptoVectors {
    bootstrap: Value,
    android_descriptor: Value,
    session: Value,
    binding: Value,
    lifecycle: Value,
}

#[derive(Debug)]
struct RejectExternal;

impl Retrieve for RejectExternal {
    fn retrieve(
        &self,
        uri: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn Error + Send + Sync>> {
        Err(format!("external schema retrieval disabled: {uri}").into())
    }
}

fn main() -> AnyResult<()> {
    let mut args = env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "verify".to_owned());
    let root = contract_root()?;
    match command.as_str() {
        "generate" => generate(&root),
        "verify" => match args.next().as_deref() {
            None => verify(&root),
            Some("--fixture") => {
                let fixture = args.next().ok_or("--fixture requires a path")?;
                if args.next().is_some() {
                    return Err("verify --fixture accepts exactly one path".into());
                }
                verifier::verify_fixture(Path::new(&fixture), MAX_BROKER_CBOR_BYTES)?;
                println!("verified fixture");
                Ok(())
            }
            Some("--evidence") => {
                let evidence = args.next().ok_or("--evidence requires a path")?;
                if args.next().is_some() {
                    return Err("verify --evidence accepts exactly one path".into());
                }
                verify_evidence(&root, Path::new(&evidence))
            }
            Some(flag) => Err(format!(
                "usage: apppilotkit-transport-contract-reference verify [--fixture <path> | --evidence <path>], got {flag}"
            )
            .into()),
        },
        _ => Err(format!(
            "usage: apppilotkit-transport-contract-reference [generate|verify], got {command}"
        )
        .into()),
    }
}

fn contract_root() -> AnyResult<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("reference crate has no contract parent")?
        .to_path_buf())
}

fn generate(root: &Path) -> AnyResult<()> {
    let crypto = crypto_vectors()?;
    write_json(
        &root.join("vectors/bootstrap-nk-success.json"),
        &crypto.bootstrap,
    )?;
    write_json(
        &root.join("vectors/bootstrap-android-descriptor.json"),
        &crypto.android_descriptor,
    )?;
    write_json(
        &root.join("vectors/session-nnpsk0-success.json"),
        &crypto.session,
    )?;
    write_json(
        &root.join("vectors/binding-replay-failures.json"),
        &crypto.binding,
    )?;
    write_json(
        &root.join("vectors/broker-ipc-boundaries.json"),
        &broker_ipc_vectors(),
    )?;
    write_json(&root.join("vectors/frame-failures.json"), &frame_vectors())?;
    write_json(
        &root.join("vectors/lifecycle-dispatch.json"),
        &crypto.lifecycle,
    )?;
    write_json(
        &root.join("vectors/secret-surface-canaries.json"),
        &canary_vectors(),
    )?;
    write_json(
        &root.join("vectors/ios-app-artifact-tree.json"),
        &ios_app_artifact_vector()?,
    )?;
    write_vector_manifest(root)?;
    write_root_manifest(root)
}

fn verify(root: &Path) -> AnyResult<()> {
    verify_dependency_pin(root)?;
    verify_schemas(root)?;
    verify_cddl(root)?;
    let _crypto_suites = verifier::verify_positive_crypto(root)?;
    let _descriptor_suites = verifier::verify_positive_android_descriptor(root)?;
    let _artifact_suites = verifier::verify_positive_ios_app_artifact(root)?;
    let _negative_cases = verify_vectors(root)?;
    verify_manifests(root)?;
    println!("verified transport contract v1");
    Ok(())
}

fn ios_app_artifact_vector() -> AnyResult<Value> {
    let info_plist = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>dev.apppilotkit.smoke</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleVersion</key><string>62.1</string>
<key>CFBundleExecutable</key><string>SmokeHost</string>
</dict></plist>
"#;
    let entries = vec![
        ("Cafe\u{301}.txt", 2, 0, Some(b"decomposed".as_slice())),
        ("Caf\u{e9}.txt", 2, 0, Some(b"composed".as_slice())),
        ("Info.plist", 2, 0, Some(info_plist.as_slice())),
        ("SmokeHost", 2, 1, Some(b"MACHO\0\xff".as_slice())),
        ("_CodeSignature", 1_u8, 0_u8, None),
        (
            "_CodeSignature/CodeResources",
            2,
            0,
            Some(b"signed\n".as_slice()),
        ),
        ("assets", 1, 0, None),
        ("assets/Icon.png", 2, 0, Some(b"PNG-UPPER".as_slice())),
        ("assets/icon.png", 2, 0, Some(b"png-lower".as_slice())),
    ];
    if entries
        .windows(2)
        .any(|pair| pair[0].0.as_bytes() >= pair[1].0.as_bytes())
    {
        return Err("iOS app artifact golden entries are not raw UTF-8-byte sorted".into());
    }
    let mut canonical = Vec::new();
    canonical.extend_from_slice(b"APPPILOTKIT-IOS-APP-TREE\0\x01");
    canonical.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    let mut json_entries = Vec::new();
    for (path, kind, executable_class, file_bytes) in entries {
        canonical.push(kind);
        canonical.extend_from_slice(&(path.len() as u32).to_be_bytes());
        canonical.extend_from_slice(path.as_bytes());
        canonical.push(executable_class);
        if let Some(bytes) = file_bytes {
            canonical.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            canonical.extend_from_slice(bytes);
            json_entries.push(json!({
                "kind":"file", "path_utf8":path, "executable_class":executable_class,
                "file_hex":hex::encode(bytes)
            }));
        } else {
            json_entries.push(json!({
                "kind":"directory", "path_utf8":path, "executable_class":executable_class
            }));
        }
    }
    Ok(json!({
        "schema_version":"1.0",
        "suite":"ios-app-artifact-tree",
        "encoding":"ios-app-tree-v1",
        "oracle":"independent-stream-parser-and-encoder",
        "format":{
            "magic_hex":hex::encode(b"APPPILOTKIT-IOS-APP-TREE\0\x01"),
            "entry_count":"u32be",
            "record":"kind:u8 || path_len:u32be || path:utf8 || executable_class:u8 || [file_len:u64be || exact_file_bytes]",
            "root":"implicit",
            "ordering":"strict ascending raw UTF-8 bytes",
            "normalization":"none; NFC and case are preserved"
        },
        "bundle":{
            "app_id":"dev.apppilotkit.smoke",
            "build":"62.1",
            "package_type":"APPL",
            "executable":"SmokeHost"
        },
        "entries":json_entries,
        "expected":{
            "entry_count":9,
            "total_file_bytes":info_plist.len() + 7 + 10 + 8 + 7 + 9 + 9,
            "canonical_byte_count":canonical.len(),
            "canonical_hex":hex::encode(&canonical),
            "artifact_sha256":sha_field(&canonical)
        },
        "test_only_material":{
            "classification":"TEST-ONLY",
            "production_use":"forbidden"
        }
    }))
}

fn verify_evidence(root: &Path, path: &Path) -> AnyResult<()> {
    let evidence = verifier::parse_strict_json(&fs::read(path)?)?;
    let schema = read_json(&root.join("schema/transport-evidence.schema.json"))?;
    jsonschema::draft202012::meta::validate(&schema).map_err(|error| error.to_string())?;
    let validator = jsonschema::draft202012::options()
        .with_retriever(RejectExternal)
        .build(&schema)
        .map_err(|error| error.to_string())?;
    validator
        .validate(&evidence)
        .map_err(|error| error.to_string())?;
    verifier::validate_evidence_semantics(root, &evidence)?;
    println!("verified retained transport evidence v1");
    Ok(())
}

fn crypto_vectors() -> AnyResult<CryptoVectors> {
    let static_public = dh_public(&BROKER_STATIC_PRIVATE)?;
    let target_reference_value = target_reference(&TARGET_REFERENCE_RANDOM);
    let target_digest = digest_target_reference(&target_reference_value);
    let alt_target_reference = target_reference(&ALT_TARGET_REFERENCE_RANDOM);
    let alt_target_digest = digest_target_reference(&alt_target_reference);
    let launch_descriptor =
        encode_launch_descriptor(&static_public, &target_digest, GeneratedLaunchEndpoint::Ios);
    let android_launch_descriptor = encode_launch_descriptor(
        &static_public,
        &target_digest,
        GeneratedLaunchEndpoint::Android(ANDROID_LOCALABSTRACT),
    );
    let bootstrap_prologue = encode_bootstrap_prologue(&target_digest);
    let m1_payload = encode_bootstrap_m1(&target_digest);
    let m2_payload = encode_bootstrap_m2(&target_digest);
    let mut target = builder(NK, &bootstrap_prologue)?
        .remote_public_key(&static_public)?
        .fixed_ephemeral_key_for_testing_only(&NK_TARGET_EPHEMERAL)
        .build_initiator()?;
    let mut broker = builder(NK, &bootstrap_prologue)?
        .local_private_key(&BROKER_STATIC_PRIVATE)?
        .fixed_ephemeral_key_for_testing_only(&NK_BROKER_EPHEMERAL)
        .build_responder()?;
    let m1 = write_handshake(&mut target, &m1_payload)?;
    read_handshake_exact(&mut broker, &m1, &m1_payload)?;
    let m2 = write_handshake(&mut broker, &m2_payload)?;
    read_handshake_exact(&mut target, &m2, &m2_payload)?;
    let nk_hash = target.get_handshake_hash().to_vec();
    if nk_hash != broker.get_handshake_hash() {
        return Err("NK handshake hashes differ".into());
    }
    let mut target_transport = target.into_transport_mode()?;
    let mut broker_transport = broker.into_transport_mode()?;
    let ack_payload = encode_bootstrap_ack(&nk_hash, &target_digest);
    let ack_plaintext = record(2, 3, ack_payload.len() as u32, 0, &ack_payload);
    let ack_ciphertext = write_transport(&mut target_transport, &ack_plaintext)?;
    read_transport_exact(&mut broker_transport, &ack_ciphertext, &ack_plaintext)?;
    let bootstrap_transcript = concat_outer(&[&m1, &m2, &ack_ciphertext]);

    let session_prologue =
        encode_session_prologue(&nk_hash, &LEASE_ID, PROCESS_GENERATION, LISTENER_EPOCH);
    let (session_success, session_artifacts) = make_session(&session_prologue)?;

    let mut tampered_m2 = m2.clone();
    let last = tampered_m2.last_mut().ok_or("empty NK m2")?;
    *last ^= 0x01;

    let binding_cases = [
        (
            "cross-lease",
            ALT_LEASE_ID,
            PROCESS_GENERATION,
            LISTENER_EPOCH,
        ),
        (
            "cross-generation",
            LEASE_ID,
            PROCESS_GENERATION + 1,
            LISTENER_EPOCH,
        ),
        (
            "old-epoch",
            LEASE_ID,
            PROCESS_GENERATION,
            LISTENER_EPOCH + 1,
        ),
    ];
    let mut negative = vec![
        negative_case(
            "equal-generation-control",
            "snow",
            json!({"original_handshake_m1_outer_hex":hex::encode(outer(&session_artifacts.handshake_m1)),"mismatched_prologue_hex":hex::encode(&session_prologue),"stage":"handshake_aead","local_binding_difference":"none","authenticated_session_opened":false}),
            Some(&outer(&session_artifacts.handshake_m1)),
            "accepted",
            "none",
        ),
        negative_case(
            "equal-role-control",
            "snow",
            json!({"session_vector":"vectors/session-nnpsk0-success.json","original_handshake_m1_outer_hex":hex::encode(outer(&session_artifacts.handshake_m1))}),
            Some(&outer(&session_artifacts.handshake_m1)),
            "accepted",
            "none",
        ),
        negative_case(
            "cross-target",
            "snow",
            json!({"original_handshake_m1_outer_hex":hex::encode(outer(&m1)),"mismatched_prologue_hex":hex::encode(encode_bootstrap_prologue(&alt_target_digest)),"broker_static_private_hex":hex::encode(BROKER_STATIC_PRIVATE)}),
            Some(&outer(&m1)),
            "rejected",
            "authenticationFailed",
        ),
        negative_case(
            "tamper-nk-message-2",
            "snow",
            json!({"bootstrap_vector":"vectors/bootstrap-nk-success.json","raw_outer_hex":hex::encode(outer(&tampered_m2))}),
            Some(&outer(&tampered_m2)),
            "rejected",
            "authenticationFailed",
        ),
        negative_case(
            "replay-session-finished",
            "snow",
            json!({"session_vector":"vectors/session-nnpsk0-success.json","raw_outer_hex":hex::encode(outer(&session_artifacts.initiator_finished_ciphertext)),"repeat_count":2,"replay_timing":"immediate_at_expected_nonce"}),
            Some(&outer(&session_artifacts.initiator_finished_ciphertext)),
            "rejected",
            "authenticationFailed",
        ),
    ];
    for (id, lease, generation, epoch) in binding_cases {
        let wrong_prologue = encode_session_prologue(&nk_hash, &lease, generation, epoch);
        negative.push(negative_case(
            id,
            "snow",
            json!({
                "original_handshake_m1_outer_hex": hex::encode(outer(&session_artifacts.handshake_m1)),
                "mismatched_prologue_hex": hex::encode(&wrong_prologue),
                "stage":"handshake_aead","local_binding_difference":id,"authenticated_session_opened":false
            }),
            Some(&outer(&session_artifacts.handshake_m1)),
            "rejected",
            "authenticationFailed",
        ));
    }
    negative.push(negative_case(
        "wrong-role",
        "snow",
        json!({"session_vector":"vectors/session-nnpsk0-success.json","original_handshake_m1_outer_hex":hex::encode(outer(&session_artifacts.handshake_m1))}),
        Some(&outer(&session_artifacts.handshake_m1)),
        "rejected",
        "authenticationFailed",
    ));
    negative.push(negative_case(
        "authenticated-session-binding-mismatch",
        "snow",
        json!({"session_vector":"vectors/session-nnpsk0-success.json","target_finished_outer_hex":hex::encode(outer(&session_artifacts.initiator_finished_ciphertext)),"stored_generation":PROCESS_GENERATION + 1}),
        Some(&outer(&session_artifacts.initiator_finished_ciphertext)),
        "rejected",
        "bindingMismatch",
    ));

    let bootstrap = json!({
        "schema_version": CONTRACT_VERSION,
        "suite": "bootstrap-nk-success",
        "oracle": "snow-0.10.0",
        "test_only_material": test_material(json!({
            "target_reference_random_hex": hex::encode(TARGET_REFERENCE_RANDOM),
            "broker_static_private_hex": hex::encode(BROKER_STATIC_PRIVATE),
            "broker_static_public_hex": hex::encode(&static_public),
            "target_ephemeral_private_hex": hex::encode(NK_TARGET_EPHEMERAL),
            "broker_ephemeral_private_hex": hex::encode(NK_BROKER_EPHEMERAL),
            "process_bootstrap_secret_hex": hex::encode(PBS)
        })),
        "canonical_input": {
            "noise_name": NK,
            "target_reference": target_reference_value,
            "target_reference_digest_hex": hex::encode(target_digest),
            "launch_platform": "ios_simulator",
            "launch_endpoint": {"host":"127.0.0.1","port":55001},
            "launch_descriptor_cbor_hex": hex::encode(&launch_descriptor),
            "prologue_cbor_hex": hex::encode(&bootstrap_prologue),
            "m1_payload_cbor_hex": hex::encode(&m1_payload),
            "m2_payload_cbor_hex": hex::encode(&m2_payload),
            "ack_payload_cbor_hex": hex::encode(&ack_payload)
        },
        "expected": {
            "m1_outer_hex": hex::encode(outer(&m1)),
            "m2_outer_hex": hex::encode(outer(&m2)),
            "ack_outer_hex": hex::encode(outer(&ack_ciphertext)),
            "noise_handshake_hash_hex": hex::encode(&nk_hash),
            "noise_handshake_hash_sha256": sha_field(&nk_hash),
            "transcript_hex": hex::encode(&bootstrap_transcript),
            "transcript_sha256": sha_field(&bootstrap_transcript),
            "result": "process_bootstrap_acknowledged",
            "close_reason": Value::Null
        }
    });
    let binding = json!({
        "schema_version": CONTRACT_VERSION,
        "suite": "binding-replay-failures",
        "oracle": "snow-0.10.0 or explicitly parser_lifecycle_contract",
        "test_only_material": test_material(json!({"process_bootstrap_secret_hex": hex::encode(PBS)})),
        "vectors": negative
    });
    let android_descriptor = json!({
        "schema_version": CONTRACT_VERSION,
        "suite": "bootstrap-android-descriptor",
        "oracle": "independent-deterministic-cbor",
        "test_only_material": test_material(json!({})),
        "shared_bootstrap_vector": "vectors/bootstrap-nk-success.json",
        "canonical_input": {
            "launch_platform": "android_emulator",
            "launch_endpoint": {"localabstract_name": ANDROID_LOCALABSTRACT},
            "launch_descriptor_cbor_hex": hex::encode(android_launch_descriptor)
        },
        "expected": {
            "result": "accepted",
            "close_reason": Value::Null
        }
    });
    let lifecycle = lifecycle_vectors(&session_prologue, &session_success, &bootstrap)?;
    Ok(CryptoVectors {
        bootstrap,
        android_descriptor,
        session: session_success,
        binding,
        lifecycle,
    })
}

struct SessionArtifacts {
    handshake_m1: Vec<u8>,
    initiator_finished_ciphertext: Vec<u8>,
}

fn make_session(prologue: &[u8]) -> AnyResult<(Value, SessionArtifacts)> {
    let mut target = builder(NNPSK0, prologue)?
        .psk(0, &PBS)?
        .fixed_ephemeral_key_for_testing_only(&SESSION_TARGET_EPHEMERAL)
        .build_initiator()?;
    let mut broker = builder(NNPSK0, prologue)?
        .psk(0, &PBS)?
        .fixed_ephemeral_key_for_testing_only(&SESSION_BROKER_EPHEMERAL)
        .build_responder()?;
    let m1 = write_handshake(&mut target, &[])?;
    read_handshake_exact(&mut broker, &m1, &[])?;
    let m2 = write_handshake(&mut broker, &[])?;
    read_handshake_exact(&mut target, &m2, &[])?;
    let session_hash = target.get_handshake_hash().to_vec();
    if session_hash != broker.get_handshake_hash() {
        return Err("NNpsk0 handshake hashes differ".into());
    }
    let mut target_transport = target.into_transport_mode()?;
    let mut broker_transport = broker.into_transport_mode()?;

    let target_finished_payload = encode_finished(0, &session_hash);
    let target_finished_plaintext = record(
        2,
        3,
        target_finished_payload.len() as u32,
        0,
        &target_finished_payload,
    );
    let target_finished = write_transport(&mut target_transport, &target_finished_plaintext)?;
    read_transport_exact(
        &mut broker_transport,
        &target_finished,
        &target_finished_plaintext,
    )?;

    let broker_finished_payload = encode_finished(1, &session_hash);
    let broker_finished_plaintext = record(
        2,
        3,
        broker_finished_payload.len() as u32,
        0,
        &broker_finished_payload,
    );
    let broker_finished = write_transport(&mut broker_transport, &broker_finished_plaintext)?;
    read_transport_exact(
        &mut target_transport,
        &broker_finished,
        &broker_finished_plaintext,
    )?;

    let open_request = br#"{"jsonrpc":"2.0","id":"open-contract","method":"session.open","params":{"client":{"name":"apppilotkit","version":"0.1.0"},"protocol":{"major":1,"minMinor":2,"maxMinor":2},"requiredCapabilities":["semantic.catalog"]}}"#;
    let open_plaintext = record(1, 3, open_request.len() as u32, 0, open_request);
    let open_ciphertext = write_transport(&mut broker_transport, &open_plaintext)?;
    read_transport_exact(&mut target_transport, &open_ciphertext, &open_plaintext)?;

    let open_response_string = format!(
        r#"{{"jsonrpc":"2.0","id":"open-contract","result":{{"context":{{"id":"session_test_0123456789abcdef","generation":{PROCESS_GENERATION}}},"protocol":{{"major":1,"minor":2}},"capabilities":["semantic.catalog","session.core"],"limits":{{"maxRequestBytes":16777216,"maxResponseBytes":67108864,"maxPageItems":10000}}}}}}"#
    );
    let open_response = open_response_string.as_bytes();
    let response_plaintext = record(1, 3, open_response.len() as u32, 0, open_response);
    let response_ciphertext = write_transport(&mut target_transport, &response_plaintext)?;
    read_transport_exact(
        &mut broker_transport,
        &response_ciphertext,
        &response_plaintext,
    )?;

    let transcript = concat_outer(&[
        &m1,
        &m2,
        &target_finished,
        &broker_finished,
        &open_ciphertext,
        &response_ciphertext,
    ]);
    let vector = json!({
        "schema_version": CONTRACT_VERSION,
        "suite": "session-nnpsk0-success",
        "oracle": "snow-0.10.0",
        "test_only_material": test_material(json!({
            "process_bootstrap_secret_hex": hex::encode(PBS),
            "target_ephemeral_private_hex": hex::encode(SESSION_TARGET_EPHEMERAL),
            "broker_ephemeral_private_hex": hex::encode(SESSION_BROKER_EPHEMERAL)
        })),
        "canonical_input": {
            "noise_name": NNPSK0,
            "prologue_cbor_hex": hex::encode(prologue),
            "handshake_m1_payload_hex": "",
            "handshake_m2_payload_hex": "",
            "target_finished_cbor_hex": hex::encode(&target_finished_payload),
            "broker_finished_cbor_hex": hex::encode(&broker_finished_payload),
            "session_open_utf8_hex": hex::encode(open_request),
            "session_open_response_utf8_hex": hex::encode(open_response)
        },
        "expected": {
            "m1_outer_hex": hex::encode(outer(&m1)),
            "m2_outer_hex": hex::encode(outer(&m2)),
            "target_finished_outer_hex": hex::encode(outer(&target_finished)),
            "broker_finished_outer_hex": hex::encode(outer(&broker_finished)),
            "session_open_outer_hex": hex::encode(outer(&open_ciphertext)),
            "session_open_response_outer_hex": hex::encode(outer(&response_ciphertext)),
            "noise_handshake_hash_hex": hex::encode(&session_hash),
            "noise_handshake_hash_sha256": sha_field(&session_hash),
            "transcript_hex": hex::encode(&transcript),
            "transcript_sha256": sha_field(&transcript),
            "result": "opened_protocol_session",
            "target_issued_session_id": "session_test_0123456789abcdef",
            "target_process_generation": PROCESS_GENERATION,
            "negotiated_protocol": {"major":1,"minor":2},
            "negotiated_capabilities": ["semantic.catalog","session.core"],
            "negotiated_limits": {
                "maxRequestBytes": REQUEST_LIMIT,
                "maxResponseBytes": RESPONSE_LIMIT,
                "maxPageItems": 10000
            },
            "close_reason": Value::Null
        }
    });
    Ok((
        vector,
        SessionArtifacts {
            handshake_m1: m1,
            initiator_finished_ciphertext: target_finished,
        },
    ))
}

fn frame_vectors() -> Value {
    let mut cases = vec![
        parser_case(
            "outer-header-timeout",
            json!({"raw_outer_hex":"00","elapsed_ms":2000}),
            "rejected",
            "timeout",
        ),
        parser_case(
            "outer-body-timeout",
            json!({"raw_outer_hex":"00100102","elapsed_ms":2000}),
            "rejected",
            "timeout",
        ),
        parser_case(
            "outer-zero-length",
            json!({"raw_outer_hex":"0000","elapsed_ms":0}),
            "rejected",
            "malformed",
        ),
        parser_case(
            "outer-oversize",
            json!({"declared_ciphertext_length":65536}),
            "rejected",
            "oversize",
        ),
        parser_case(
            "record-truncated-header",
            json!({"records_hex":["0103000000000001000000"],"max_message_bytes":16777216}),
            "rejected",
            "malformed",
        ),
        parser_case(
            "record-trailing-after-end",
            json!({"records_hex":["0103000000000001000000000001"],"max_message_bytes":16777216}),
            "rejected",
            "malformed",
        ),
        parser_case(
            "record-reorder",
            json!({"records_hex":["0102000000000000000000040506","01010000000000060000000001020304"],"max_message_bytes":16777216}),
            "rejected",
            "sequenceViolation",
        ),
        parser_case(
            "record-gap",
            json!({"records_hex":["0101000000000004000000000102","01020000000000000000000304"],"max_message_bytes":16777216}),
            "rejected",
            "sequenceViolation",
        ),
        parser_case(
            "record-overlap",
            json!({"records_hex":["010100000000000400000000010203","0102000000000000000000020304"],"max_message_bytes":16777216}),
            "rejected",
            "sequenceViolation",
        ),
        parser_case(
            "record-interleave",
            json!({"records_hex":["0101000000000004000000000102","0202000000000000000000020304"],"max_message_bytes":16777216}),
            "rejected",
            "sequenceViolation",
        ),
        parser_case(
            "half-duplex-peer-turn",
            json!({"owner":"broker","incoming_application_role":"target","records_hex":["01030000000000010000000001"],"max_message_bytes":16777216}),
            "rejected",
            "sequenceViolation",
        ),
        parser_case(
            "record-unknown-flags",
            json!({"records_hex":["01040000000000010000000001"],"max_message_bytes":16777216}),
            "rejected",
            "sequenceViolation",
        ),
        parser_case(
            "record-nonzero-reserved",
            json!({"records_hex":["01030001000000010000000001"],"max_message_bytes":16777216}),
            "rejected",
            "sequenceViolation",
        ),
        parser_case(
            "record-non-start-total-len",
            json!({"records_hex":["01020000000000010000000001"],"max_message_bytes":16777216}),
            "rejected",
            "sequenceViolation",
        ),
        parser_case(
            "close-record-valid",
            json!({"records_hex":["040300000000000700000000a30001010a0201"],"max_message_bytes":8192}),
            "accepted",
            "none",
        ),
        parser_case(
            "close-record-invalid-reason",
            json!({"records_hex":["040300000000000700000000a30001010e0201"],"max_message_bytes":8192}),
            "rejected",
            "malformed",
        ),
        parser_case(
            "close-record-missing-handoff",
            json!({"records_hex":["040300000000000500000000a20001010a"],"max_message_bytes":8192}),
            "rejected",
            "malformed",
        ),
        parser_case(
            "close-record-non-shortest-cbor",
            json!({"records_hex":["040300000000000800000000a3000101180a0201"],"max_message_bytes":8192}),
            "rejected",
            "malformed",
        ),
        parser_case(
            "cbor-duplicate-key",
            json!({"raw_hex":"a200010002"}),
            "rejected",
            "malformed",
        ),
        parser_case(
            "cbor-non-shortest-integer",
            json!({"raw_hex":"1817"}),
            "rejected",
            "malformed",
        ),
        parser_case(
            "cbor-out-of-order-key",
            json!({"raw_hex":"a201000000"}),
            "rejected",
            "malformed",
        ),
        parser_case(
            "request-oversize",
            json!({"total_len":16777217}),
            "rejected",
            "oversize",
        ),
        parser_case(
            "response-oversize",
            json!({"total_len":67108865}),
            "rejected",
            "oversize",
        ),
        parser_case(
            "session-open-oversize",
            json!({"total_len":65537}),
            "rejected",
            "oversize",
        ),
        parser_case(
            "nonce-record-limit",
            json!({"accepted_records":4_294_967_295_u64,"next_record":4_294_967_296_u64}),
            "rejected",
            "recordLimit",
        ),
        parser_case(
            "plaintext-byte-limit",
            json!({"accepted_plaintext_bytes":1_099_511_627_775_u64,"next_plaintext_bytes":1}),
            "rejected",
            "recordLimit",
        ),
    ];
    for case in &mut cases {
        if matches!(
            case["expected_close_reason"].as_str(),
            Some("malformed") | Some("sequenceViolation")
        ) {
            case["expected_error_kind"] = Value::String("sessionExpired".to_owned());
        }
    }
    json!({
        "schema_version": CONTRACT_VERSION,
        "suite": "frame-failures",
        "oracle": "parser_lifecycle_contract",
        "test_only_material": test_material(json!({"process_bootstrap_secret_hex": hex::encode(PBS)})),
        "vectors": cases
    })
}

fn broker_ipc_vectors() -> Value {
    json!({
        "schema_version": CONTRACT_VERSION,
        "suite": "broker-ipc-boundaries",
        "oracle": "independent_broker_packet_verifier",
        "test_only_material": test_material(json!({})),
        "goldens": {
            "max_exchange_request_body_bytes": 16_777_216,
            "max_exchange_request_cbor_overhead_bytes": 264,
            "max_exchange_request_cbor_bytes": 16_777_480,
            "max_exchange_request_packet_bytes": 16_777_484,
            "max_exchange_response_body_bytes": 67_108_864,
            "max_exchange_response_cbor_overhead_bytes": 256,
            "max_exchange_response_cbor_bytes": 67_109_120,
            "max_exchange_response_packet_bytes": 67_109_124,
            "max_open_session_cbor_bytes": 73_728,
            "max_open_session_packet_bytes": 73_732,
            "global_cbor_cap": 67_109_120,
            "global_packet_cap": 67_109_124
        },
        "broker_target_ready": {
            "target_reference_token_hex": hex::encode(TARGET_REFERENCE_RANDOM),
            "process_generation": PROCESS_GENERATION,
            "listener_epoch": LISTENER_EPOCH,
            "issued_at_unix_ms": EXPIRY_UNIX_MS - 30_000,
            "expires_at_unix_ms": EXPIRY_UNIX_MS,
            "prepare_projection": "byte_identical"
        },
        "vectors": [
            parser_case("broker-packet-cap-plus-one", json!({"declared_cbor_length":67_109_121}), "rejected", "oversize"),
            parser_case("broker-control-operation-cap-plus-one", json!({"operation":"prepare","declared_cbor_length":8_193}), "rejected", "oversize"),
            parser_case("broker-open-session-cap-plus-one", json!({"operation":"open_session","declared_cbor_length":73_729}), "rejected", "oversize")
        ]
    })
}

fn lifecycle_vectors(
    session_prologue: &[u8],
    session_vector: &Value,
    bootstrap_vector: &Value,
) -> AnyResult<Value> {
    let session_m1_a = independent_session_m1(session_prologue, &SESSION_TARGET_EPHEMERAL)?;
    let session_m1_b = independent_session_m1(session_prologue, &SESSION_TARGET_EPHEMERAL_B)?;
    let request_outer = session_vector["expected"]["session_open_outer_hex"]
        .as_str()
        .ok_or("session request outer missing")?;
    let response_outer = session_vector["expected"]["session_open_response_outer_hex"]
        .as_str()
        .ok_or("session response outer missing")?;
    let request_total = hex::decode(request_outer)?.len() as u64;
    let response_total = hex::decode(response_outer)?.len() as u64;
    let bootstrap_transcript_sha256 = bootstrap_vector["expected"]["transcript_sha256"]
        .as_str()
        .ok_or("bootstrap transcript digest missing")?;
    let mut cases = vec![
        parser_case(
            "ready-timestamps-inconsistent-window",
            json!({"broker_issued_at_unix_ms":1000,"broker_expires_at_unix_ms":30000,"projected_issued_at_unix_ms":1000,"projected_expires_at_unix_ms":30000,"now_unix_ms":1001}),
            "rejected",
            "internalError",
        ),
        parser_case(
            "ready-timestamps-expired",
            json!({"broker_issued_at_unix_ms":1000,"broker_expires_at_unix_ms":31000,"projected_issued_at_unix_ms":1000,"projected_expires_at_unix_ms":31000,"now_unix_ms":31000}),
            "rejected",
            "stale",
        ),
        parser_case(
            "ready-timestamps-client-rewrite",
            json!({"broker_issued_at_unix_ms":1000,"broker_expires_at_unix_ms":31000,"projected_issued_at_unix_ms":1001,"projected_expires_at_unix_ms":31001,"now_unix_ms":1001}),
            "rejected",
            "bindingMismatch",
        ),
        parser_case(
            "target-ref-second-redeem",
            json!({"issued_at_unix_ms":1000,"expires_at_unix_ms":31000,"events":[{"at_unix_ms":2000,"operation":"target_only_open","dispatch_boundary_crossed":false},{"at_unix_ms":3000,"operation":"target_only_open","dispatch_boundary_crossed":false}]}),
            "rejected",
            "stale",
        ),
        parser_case(
            "atomic-close-wins-before-dispatch",
            json!({"issued_at_unix_ms":1000,"expires_at_unix_ms":31000,"events":[{"at_unix_ms":2000,"operation":"target_only_open","dispatch_boundary_crossed":false},{"at_unix_ms":3000,"operation":"exchange","dispatch_boundary_crossed":false},{"at_unix_ms":3000,"operation":"close","dispatch_boundary_crossed":false}]}),
            "rejected",
            "stale",
        ),
        parser_case(
            "ready-reference-expired",
            json!({"age_ms":30000}),
            "rejected",
            "stale",
        ),
        parser_case(
            "session-idle-expired",
            json!({"idle_ms":30000}),
            "rejected",
            "stale",
        ),
        parser_case(
            "lease-idle-expired",
            json!({"idle_ms":120000}),
            "rejected",
            "stale",
        ),
        parser_case(
            "lease-absolute-expired",
            json!({"age_ms":900000}),
            "rejected",
            "stale",
        ),
        parser_case(
            "heartbeat-timeout",
            json!({"missed":4,"elapsed_ms":120000}),
            "rejected",
            "brokerLost",
        ),
        parser_case(
            "cleanup-failure",
            json!({"stage":"adapter_owned_forward","elapsed_ms":2000}),
            "rejected",
            "cleanupFailed",
        ),
        dispatch_case(
            "pre-dispatch-authentication",
            "app_mutation",
            false,
            "authentication_failed",
            "transport.authenticationRequired",
            "not_dispatched",
            "authenticationFailed",
        ),
        dispatch_case(
            "pre-dispatch-timeout",
            "app_mutation",
            false,
            "deadline",
            "timeout",
            "not_dispatched",
            "timeout",
        ),
        dispatch_case(
            "post-dispatch-read-timeout",
            "read_only",
            true,
            "deadline",
            "timeout",
            "dispatched",
            "timeout",
        ),
        dispatch_case(
            "post-dispatch-mutation-eof",
            "app_mutation",
            true,
            "eof",
            "action.outcomeUnknown",
            "ambiguous",
            "peerClosed",
        ),
    ];
    for case in [
        contract_case(
            "prepare-no-lease-launch-bootstrap",
            json!({"bootstrap_vector":"vectors/bootstrap-nk-success.json","observed_bootstrap_transcript_sha256":[bootstrap_transcript_sha256],
                "second_prepare_bootstrap_transcript_sha256":[],"live_lease":false,"launch_count":1,"nk_count":1,"pbs_replaced":true,"minted_refs":1}),
            "accepted",
            "none",
        ),
        contract_case(
            "prepare-eligible-owned-lease-mints-ref-no-launch-no-bootstrap",
            json!({"bootstrap_vector":"vectors/bootstrap-nk-success.json","observed_bootstrap_transcript_sha256":[bootstrap_transcript_sha256],
                "second_prepare_bootstrap_transcript_sha256":[],"live_lease":true,"broker_owned":true,"prepare_key_match":true,"generation_match":true,"epoch_match":true,"eligible":true,"heartbeat_authenticated":true,"launch_count":0,"nk_count":0,"pbs_replaced":false,"minted_refs":1}),
            "accepted",
            "none",
        ),
        contract_case(
            "prepare-live-conflicting-build-fails-no-relaunch",
            json!({"bootstrap_vector":"vectors/bootstrap-nk-success.json","observed_bootstrap_transcript_sha256":[bootstrap_transcript_sha256],
                "second_prepare_bootstrap_transcript_sha256":[],"live_lease":true,"broker_owned":true,"prepare_key_match":false,"generation_match":true,"epoch_match":true,"eligible":true,"heartbeat_authenticated":true,"launch_count":0,"nk_count":0,"pbs_replaced":false,"minted_refs":0}),
            "rejected",
            "bindingMismatch",
        ),
        contract_case(
            "prepare-reuse-new-bootstrap-transcript-rejected",
            json!({"bootstrap_vector":"vectors/bootstrap-nk-success.json","observed_bootstrap_transcript_sha256":[bootstrap_transcript_sha256],
                "second_prepare_bootstrap_transcript_sha256":[bootstrap_transcript_sha256],"live_lease":true,"broker_owned":true,
                "prepare_key_match":true,"generation_match":true,"epoch_match":true,"eligible":true,"heartbeat_authenticated":true,
                "launch_count":0,"nk_count":1,"pbs_replaced":true,"minted_refs":0}),
            "rejected",
            "bindingMismatch",
        ),
        contract_case(
            "two-fresh-refs-independent-redemption",
            json!({"lease_id_hex":hex::encode(LEASE_ID),"references":[{"token_hex":hex::encode(TARGET_REFERENCE_RANDOM),"issued_at":1000,"expires_at":31000,"redeem_at":2000,"redeem_count":1},{"token_hex":hex::encode(ALT_TARGET_REFERENCE_RANDOM),"issued_at":1500,"expires_at":31500,"redeem_at":2500,"redeem_count":1}]}),
            "accepted",
            "none",
        ),
        contract_case(
            "two-agent-fresh-noise-and-target-session-ids",
            json!({"noise_name":NNPSK0,"prologue_cbor_hex":hex::encode(session_prologue),"psk_hex":hex::encode(PBS),"m1_a_outer_hex":hex::encode(outer(&session_m1_a)),"m1_b_outer_hex":hex::encode(outer(&session_m1_b)),"session_id_a":"session_test_agent_a_0001","session_id_b":"session_test_agent_b_0002"}),
            "accepted",
            "none",
        ),
        contract_case(
            "concurrent-read-both-complete",
            json!({"sessions":[{"id":"a","runtime_instance":"runtime-a","operation":"read","state":"complete"},{"id":"b","runtime_instance":"runtime-b","operation":"read","state":"complete"}],"shared_catalog":true,"shared_action_coordinator":true}),
            "accepted",
            "none",
        ),
        contract_case(
            "close-session-a-session-b-remains-open",
            json!({"terminal_scope":"session","terminal_session":"a","sessions_after":{"a":"closed","b":"open"},"all_session_invalidate_calls":0}),
            "accepted",
            "none",
        ),
        contract_case(
            "session-a-idle-expiry-session-b-remains-open",
            json!({"terminal_scope":"session","terminal_session":"a","reason":"idle_expiry","sessions_after":{"a":"stale","b":"open"},"all_session_invalidate_calls":0}),
            "accepted",
            "none",
        ),
        contract_case(
            "session-a-auth-failure-session-b-remains-open",
            json!({"terminal_scope":"session","terminal_session":"a","reason":"authentication_failure","sessions_after":{"a":"stale","b":"open"},"all_session_invalidate_calls":0}),
            "accepted",
            "none",
        ),
        contract_case(
            "lease-loss-stales-both",
            json!({"terminal_scope":"lease","cause":"lease_loss","sessions_after":{"a":"stale","b":"stale"},"refs_after":{"a":"stale","b":"stale"},"all_session_invalidate_calls":2}),
            "rejected",
            "stale",
        ),
        contract_case(
            "epoch-loss-stales-both",
            json!({"terminal_scope":"lease","cause":"epoch_loss","sessions_after":{"a":"stale","b":"stale"},"refs_after":{"a":"stale","b":"stale"},"all_session_invalidate_calls":2}),
            "rejected",
            "stale",
        ),
        contract_case(
            "process-loss-stales-both",
            json!({"terminal_scope":"lease","cause":"process_loss","sessions_after":{"a":"stale","b":"stale"},"refs_after":{"a":"stale","b":"stale"},"all_session_invalidate_calls":2}),
            "rejected",
            "stale",
        ),
        contract_case(
            "broker-heartbeat-loss-stales-both",
            json!({"terminal_scope":"lease","cause":"broker_heartbeat_loss","sessions_after":{"a":"stale","b":"stale"},"refs_after":{"a":"stale","b":"stale"},"all_session_invalidate_calls":2}),
            "rejected",
            "stale",
        ),
        contract_case(
            "catalog-complete-nonempty-projects-show",
            json!({"item_count":1,"truncated":false,"capability":"smoke.ready","declaration_revision":1,"session":"session_test_agent_a_0001","target":target_reference(&TARGET_REFERENCE_RANDOM),"next_action_id":"catalog.show","argv":["/prefix/bin/apppilotkit","catalog","show","--capability","smoke.ready","--declaration-revision","1","--session=session_test_agent_a_0001",format!("--target={}",target_reference(&TARGET_REFERENCE_RANDOM)),"--output","json","--non-interactive"]}),
            "accepted",
            "none",
        ),
        contract_case(
            "catalog-truncated-projects-continuation",
            json!({"item_count":1,"truncated":true,"cursor":"cursor_opaque_2","session":"session_test_agent_a_0001","target":target_reference(&TARGET_REFERENCE_RANDOM),"next_action_id":"catalog.list.continue","argv":["/prefix/bin/apppilotkit","catalog","list","--session=session_test_agent_a_0001",format!("--target={}",target_reference(&TARGET_REFERENCE_RANDOM)),"--cursor","cursor_opaque_2","--output","json","--non-interactive"]}),
            "accepted",
            "none",
        ),
        contract_case(
            "catalog-complete-empty-projects-list-selector",
            json!({"item_count":0,"truncated":false,"session":"session_test_agent_a_0001","target":target_reference(&TARGET_REFERENCE_RANDOM),"next_action_id":"catalog.list","argv":["/prefix/bin/apppilotkit","catalog","list","--session=session_test_agent_a_0001",format!("--target={}",target_reference(&TARGET_REFERENCE_RANDOM)),"--output","json","--non-interactive"]}),
            "accepted",
            "none",
        ),
    ] {
        cases.push(case);
    }
    for (id, side_effect, emitted, end, response, error, handoff) in [
        (
            "broker-lost-pre-send-read",
            "read_only",
            0,
            false,
            0,
            "sessionExpired",
            "not_handed_off",
        ),
        (
            "broker-lost-pre-send-invoke",
            "app_mutation",
            0,
            false,
            0,
            "sessionExpired",
            "not_handed_off",
        ),
        (
            "broker-lost-partial-read",
            "read_only",
            request_total / 2,
            false,
            0,
            "sessionExpired",
            "not_handed_off",
        ),
        (
            "broker-lost-partial-invoke",
            "app_mutation",
            request_total / 2,
            false,
            0,
            "sessionExpired",
            "not_handed_off",
        ),
        (
            "broker-lost-full-before-response-read",
            "read_only",
            request_total,
            true,
            0,
            "sessionExpired",
            "handoff_possible_or_confirmed",
        ),
        (
            "broker-lost-full-before-response-invoke",
            "app_mutation",
            request_total,
            true,
            0,
            "action.outcomeUnknown",
            "handoff_possible_or_confirmed",
        ),
        (
            "broker-lost-safe-response-lost-read",
            "read_only",
            request_total,
            true,
            0,
            "sessionExpired",
            "handoff_possible_or_confirmed",
        ),
        (
            "broker-lost-safe-response-lost-invoke",
            "app_mutation",
            request_total,
            true,
            0,
            "action.outcomeUnknown",
            "handoff_possible_or_confirmed",
        ),
        (
            "broker-lost-response-partial-eof-read",
            "read_only",
            request_total,
            true,
            8,
            "sessionExpired",
            "handoff_possible_or_confirmed",
        ),
        (
            "broker-lost-response-partial-eof-invoke",
            "app_mutation",
            request_total,
            true,
            8,
            "action.outcomeUnknown",
            "handoff_possible_or_confirmed",
        ),
    ] {
        let mut case = contract_case(
            id,
            json!({"session_vector":"vectors/session-nnpsk0-success.json","request_outer_hex":request_outer,
                "response_outer_hex":response_outer,"side_effect":side_effect,"request_bytes_emitted":emitted,
                "request_total_bytes":request_total,"request_end_emitted":end,"response_bytes_reassembled":response,
                "response_total_bytes":response_total,"response_end_reassembled":false,"failure":"brokerLost"}),
            "rejected",
            "brokerLost",
        );
        case["expected_error_kind"] = Value::String(error.to_owned());
        case["expected_handoff"] = Value::String(handoff.to_owned());
        cases.push(case);
    }
    for (id, side_effect) in [
        ("broker-response-complete-read", "read_only"),
        ("broker-response-complete-invoke", "app_mutation"),
    ] {
        let mut case = contract_case(
            id,
            json!({"session_vector":"vectors/session-nnpsk0-success.json","request_outer_hex":request_outer,
                "response_outer_hex":response_outer,"side_effect":side_effect,"request_bytes_emitted":request_total,
                "request_total_bytes":request_total,"request_end_emitted":true,"response_bytes_reassembled":response_total,
                "response_total_bytes":response_total,"response_end_reassembled":true,"failure":"none"}),
            "accepted",
            "none",
        );
        case["expected_error_kind"] = Value::String("none".to_owned());
        case["expected_handoff"] = Value::String("handoff_possible_or_confirmed".to_owned());
        cases.push(case);
    }
    Ok(json!({
        "schema_version": CONTRACT_VERSION,
        "suite": "lifecycle-dispatch",
        "oracle": "pinned_case_state_machine",
        "test_only_material": test_material(json!({"process_bootstrap_secret_hex": hex::encode(PBS)})),
        "vectors": cases
    }))
}

fn contract_case(id: &str, input: Value, result: &str, close: &str) -> Value {
    negative_case(id, "pinned_case_state_machine", input, None, result, close)
}

fn independent_session_m1(prologue: &[u8], ephemeral: &[u8; 32]) -> AnyResult<Vec<u8>> {
    let mut initiator = builder(NNPSK0, prologue)?
        .psk(0, &PBS)?
        .fixed_ephemeral_key_for_testing_only(ephemeral)
        .build_initiator()?;
    write_handshake(&mut initiator, &[])
}

fn canary_vectors() -> Value {
    let surfaces = [
        "argv",
        "environment",
        "activity_extras",
        "stdout",
        "stderr",
        "product_logs",
        "diagnostics",
        "machine_result",
        "next_actions",
        "artifacts",
        "smoke_host_build_artifact",
        "production_build_artifact",
        "release_build_artifact",
    ];
    let mut cases = surfaces
        .into_iter()
        .map(|surface| {
            let artifact = format!("prefix{TEST_CANARY}suffix").into_bytes();
            parser_case(
                &format!("secret-surface-{surface}"),
                json!({
                    "surface":surface,
                    "scanner":"apppilotkit-reference-byte-scanner","scanner_version":"1.0",
                    "operation":"literal-byte-subsequence-count","fixed_canary_utf8":TEST_CANARY,
                    "execution_canary_utf8":EXECUTION_CANARY_EXAMPLE,
                    "artifact_identity":format!("fixture:{surface}"),
                    "artifact_path":format!("reference/vector-inline/{surface}"),
                    "artifact_sha256":sha_field(&artifact),"artifact_hex":hex::encode(&artifact),
                    "declared_byte_count":artifact.len(),"declared_fixed_match_count":1,
                    "declared_execution_match_count":0,"complete":true
                }),
                "rejected",
                "internalError",
            )
        })
        .collect::<Vec<_>>();
    let dishonest_artifact = format!("prefix{TEST_CANARY}suffix").into_bytes();
    cases.push(parser_case(
        "secret-surface-dishonest-count",
        json!({"surface":"stdout","scanner":"apppilotkit-reference-byte-scanner","scanner_version":"1.0","operation":"literal-byte-subsequence-count","fixed_canary_utf8":TEST_CANARY,"execution_canary_utf8":EXECUTION_CANARY_EXAMPLE,"artifact_identity":"fixture:dishonest-count","artifact_path":"reference/vector-inline/dishonest-count","artifact_sha256":sha_field(&dishonest_artifact),"artifact_hex":hex::encode(&dishonest_artifact),"declared_byte_count":dishonest_artifact.len(),"declared_fixed_match_count":0,"declared_execution_match_count":0,"complete":true}),
        "rejected",
        "internalError",
    ));
    json!({
        "schema_version": CONTRACT_VERSION,
        "suite": "secret-surface-canaries",
        "oracle": "parser_lifecycle_contract",
        "warning": "The fixed synthetic TEST-ONLY canary is vector input only. Actual output, logs, diagnostics, artifacts, and build products require zero occurrences.",
        "test_only_material": test_material(json!({"synthetic_canary":TEST_CANARY,"execution_unique_example":EXECUTION_CANARY_EXAMPLE})),
        "vectors": cases
    })
}

fn negative_case(
    id: &str,
    oracle: &str,
    input: Value,
    crypto: Option<&[u8]>,
    result: &str,
    close: &str,
) -> Value {
    let canonical = canonical_json(&input);
    json!({
        "id": id,
        "oracle": oracle,
        "validator": validator_for_case(id),
        "test_only_secret_hex": hex::encode(PBS),
        "classification": "TEST-ONLY",
        "canonical_input": input,
        "expected_transcript_encoding": "canonical_json_utf8",
        "expected_transcript_hex": hex::encode(canonical.as_bytes()),
        "expected_transcript_sha256": sha_field(canonical.as_bytes()),
        "cryptographic_expected_bytes_hex": crypto.map(hex::encode),
        "expected_result": result,
        "expected_close_reason": close
    })
}

fn validator_for_case(id: &str) -> &'static str {
    if id == "tamper-nk-message-2" {
        "noise_nk_tamper"
    } else if id == "replay-session-finished" {
        "noise_finished_replay"
    } else if id.starts_with("cross-") || id == "old-epoch" {
        "noise_cross_binding"
    } else if id == "wrong-role" || id == "authenticated-session-binding-mismatch" {
        "noise_failure_classification"
    } else if id.starts_with("ready-timestamps-") {
        "ready_timestamps"
    } else if id.starts_with("target-ref-") || id.starts_with("atomic-") {
        "lifecycle"
    } else if id.starts_with("broker-packet") || id.starts_with("broker-control") {
        "broker_packet"
    } else if id.starts_with("outer-") {
        "outer_frame"
    } else if id.starts_with("record-") || id == "half-duplex-peer-turn" {
        "record_reassembly"
    } else if id.starts_with("cbor-") {
        "deterministic_cbor"
    } else if id.starts_with("secret-surface-") {
        "secret_surface_scanner"
    } else if id.starts_with("pre-dispatch-") || id.starts_with("post-dispatch-") {
        "dispatch_classification"
    } else {
        "limit_lifecycle"
    }
}

fn parser_case(id: &str, input: Value, result: &str, close: &str) -> Value {
    negative_case(id, "parser_lifecycle_contract", input, None, result, close)
}

fn dispatch_case(
    id: &str,
    side_effect: &str,
    crossed: bool,
    failure: &str,
    error: &str,
    dispatch: &str,
    close: &str,
) -> Value {
    let input =
        json!({"side_effect":side_effect,"dispatch_boundary_crossed":crossed,"failure":failure});
    let mut value = parser_case(id, input, "rejected", close);
    value["expected_error_kind"] = Value::String(error.to_owned());
    value["expected_dispatch"] = Value::String(dispatch.to_owned());
    value
}

fn test_material(material: Value) -> Value {
    json!({
        "classification": "TEST-ONLY",
        "production_use": "forbidden",
        "material": material
    })
}

fn builder<'a>(name: &str, prologue: &'a [u8]) -> AnyResult<Builder<'a>> {
    let params: NoiseParams = name.parse()?;
    Ok(Builder::new(params).prologue(prologue)?)
}

fn dh_public(private: &[u8; 32]) -> AnyResult<Vec<u8>> {
    use snow::resolvers::{CryptoResolver, DefaultResolver};
    let params: NoiseParams = NK.parse()?;
    let mut dh = DefaultResolver
        .resolve_dh(&params.dh)
        .ok_or("snow X25519 resolver unavailable")?;
    dh.set(private);
    Ok(dh.pubkey().to_vec())
}

fn write_handshake(state: &mut HandshakeState, payload: &[u8]) -> AnyResult<Vec<u8>> {
    let mut out = vec![0_u8; 65_535];
    let len = state.write_message(payload, &mut out)?;
    out.truncate(len);
    Ok(out)
}

fn read_handshake_exact(
    state: &mut HandshakeState,
    message: &[u8],
    expected: &[u8],
) -> AnyResult<()> {
    let mut out = vec![0_u8; 8_192];
    let len = state.read_message(message, &mut out)?;
    if &out[..len] != expected {
        return Err("Noise handshake payload mismatch".into());
    }
    Ok(())
}

fn write_transport(state: &mut TransportState, plaintext: &[u8]) -> AnyResult<Vec<u8>> {
    let mut out = vec![0_u8; 65_535];
    let len = state.write_message(plaintext, &mut out)?;
    out.truncate(len);
    Ok(out)
}

fn read_transport_exact(
    state: &mut TransportState,
    ciphertext: &[u8],
    expected: &[u8],
) -> AnyResult<()> {
    let mut out = vec![0_u8; 65_535];
    let len = state.read_message(ciphertext, &mut out)?;
    if &out[..len] != expected {
        return Err("Noise transport plaintext mismatch".into());
    }
    Ok(())
}

fn encode_bootstrap_prologue(target_digest: &[u8; 32]) -> Vec<u8> {
    encode(|e| {
        e.array(10)?
            .str("apppilotkit.transport")?
            .u8(1)?
            .str("bootstrap")?
            .u8(0)?
            .u8(1)?
            .bytes(target_digest)?
            .bytes(&LEASE_ID)?
            .bytes(&TARGET_NONCE)?
            .bytes(&APP_DIGEST)?
            .u64(EXPIRY_UNIX_MS)?;
        Ok(())
    })
}

#[derive(Clone, Copy)]
enum GeneratedLaunchEndpoint<'a> {
    Ios,
    Android(&'a str),
}

fn encode_launch_descriptor(
    static_public: &[u8],
    target_digest: &[u8; 32],
    endpoint: GeneratedLaunchEndpoint<'_>,
) -> Vec<u8> {
    encode(|e| {
        e.map(9)?.u8(0)?.u8(1)?.u8(1)?;
        match endpoint {
            GeneratedLaunchEndpoint::Ios => e.u8(0)?,
            GeneratedLaunchEndpoint::Android(_) => e.u8(1)?,
        };
        e.u8(2)?
            .bytes(&LEASE_ID)?
            .u8(3)?
            .bytes(&TARGET_NONCE)?
            .u8(4)?
            .bytes(&APP_DIGEST)?
            .u8(5)?
            .bytes(static_public)?
            .u8(6)?;
        match endpoint {
            GeneratedLaunchEndpoint::Ios => {
                e.map(2)?.u8(0)?.str("127.0.0.1")?.u8(1)?.u16(55_001)?;
            }
            GeneratedLaunchEndpoint::Android(name) => {
                e.map(1)?.u8(0)?.str(name)?;
            }
        }
        e.u8(7)?.u64(EXPIRY_UNIX_MS)?.u8(8)?.bytes(target_digest)?;
        Ok(())
    })
}

fn encode_bootstrap_m1(target_digest: &[u8; 32]) -> Vec<u8> {
    encode(|e| {
        e.map(4)?
            .u8(0)?
            .u8(1)?
            .u8(1)?
            .bytes(target_digest)?
            .u8(2)?
            .bytes(&LEASE_ID)?
            .u8(3)?
            .bytes(&TARGET_NONCE)?;
        Ok(())
    })
}

fn encode_bootstrap_m2(target_digest: &[u8; 32]) -> Vec<u8> {
    encode(|e| {
        e.map(7)?
            .u8(0)?
            .u8(1)?
            .u8(1)?
            .bytes(&PBS)?
            .u8(2)?
            .bytes(target_digest)?
            .u8(3)?
            .bytes(&LEASE_ID)?
            .u8(4)?
            .bytes(&TARGET_NONCE)?
            .u8(5)?
            .u64(EXPIRY_UNIX_MS)?
            .u8(6)?
            .bytes(&APP_DIGEST)?;
        Ok(())
    })
}

fn encode_bootstrap_ack(nk_hash: &[u8], target_digest: &[u8; 32]) -> Vec<u8> {
    encode(|e| {
        e.map(6)?
            .u8(0)?
            .u8(1)?
            .u8(1)?
            .bytes(target_digest)?
            .u8(2)?
            .bytes(&LEASE_ID)?
            .u8(3)?
            .u64(PROCESS_GENERATION)?
            .u8(4)?
            .u64(LISTENER_EPOCH)?
            .u8(5)?
            .bytes(nk_hash)?;
        Ok(())
    })
}

fn encode_session_prologue(
    nk_hash: &[u8],
    lease: &[u8; 16],
    generation: u64,
    epoch: u64,
) -> Vec<u8> {
    encode(|e| {
        e.array(12)?
            .str("apppilotkit.transport")?
            .u8(1)?
            .str("session")?
            .u8(0)?
            .u8(1)?
            .bytes(lease)?
            .u64(generation)?
            .u64(epoch)?
            .u64(REQUEST_LIMIT)?
            .u64(RESPONSE_LIMIT)?
            .u64(HANDSHAKE_LIMIT)?
            .bytes(nk_hash)?;
        Ok(())
    })
}

fn encode_finished(role: u8, session_hash: &[u8]) -> Vec<u8> {
    encode(|e| {
        e.map(6)?
            .u8(0)?
            .u8(1)?
            .u8(1)?
            .u8(role)?
            .u8(2)?
            .bytes(&LEASE_ID)?
            .u8(3)?
            .u64(PROCESS_GENERATION)?
            .u8(4)?
            .u64(LISTENER_EPOCH)?
            .u8(5)?
            .bytes(session_hash)?;
        Ok(())
    })
}

fn encode<F>(f: F) -> Vec<u8>
where
    F: FnOnce(
        &mut Encoder<&mut Vec<u8>>,
    ) -> Result<(), minicbor::encode::Error<std::convert::Infallible>>,
{
    let mut bytes = Vec::new();
    let mut encoder = Encoder::new(&mut bytes);
    f(&mut encoder).expect("encoding fixed deterministic CBOR cannot fail");
    bytes
}

fn record(kind: u8, flags: u8, total_len: u32, offset: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + data.len());
    out.push(kind);
    out.push(flags);
    out.extend_from_slice(&0_u16.to_be_bytes());
    out.extend_from_slice(&total_len.to_be_bytes());
    out.extend_from_slice(&offset.to_be_bytes());
    out.extend_from_slice(data);
    out
}

fn outer(message: &[u8]) -> Vec<u8> {
    assert!(!message.is_empty() && message.len() <= u16::MAX as usize);
    let mut out = Vec::with_capacity(2 + message.len());
    out.extend_from_slice(&(message.len() as u16).to_be_bytes());
    out.extend_from_slice(message);
    out
}

fn concat_outer(messages: &[&[u8]]) -> Vec<u8> {
    messages.iter().flat_map(|message| outer(message)).collect()
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn target_reference(random: &[u8; 32]) -> String {
    format!("target_{}", URL_SAFE_NO_PAD.encode(random))
}

fn digest_target_reference(reference: &str) -> [u8; 32] {
    let digest = Sha256::digest(reference.as_bytes());
    let mut bytes = [0_u8; 32];
    bytes.copy_from_slice(&digest);
    bytes
}

fn sha_field(bytes: &[u8]) -> String {
    format!("sha256:{}", sha256(bytes))
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => serde_json::to_string(value).expect("string encoding"),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Object(map) => {
            let sorted = map.iter().collect::<BTreeMap<_, _>>();
            let members = sorted
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("key encoding"),
                        canonical_json(value)
                    )
                })
                .collect::<Vec<_>>();
            format!("{{{}}}", members.join(","))
        }
    }
}

fn write_json(path: &Path, value: &Value) -> AnyResult<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    fs::write(path, bytes)?;
    Ok(())
}

fn read_json(path: &Path) -> AnyResult<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn write_vector_manifest(root: &Path) -> AnyResult<()> {
    let names = vector_names();
    let files = names
        .iter()
        .map(|name| {
            let relative = format!("vectors/{name}");
            let bytes = fs::read(root.join(&relative)).expect("generated vector exists");
            json!({"path":relative,"bytes":bytes.len(),"sha256":sha256(&bytes)})
        })
        .collect::<Vec<_>>();
    write_json(
        &root.join("vectors/manifest.json"),
        &json!({
            "schema_version": CONTRACT_VERSION,
            "algorithm": "SHA-256",
            "files": files
        }),
    )
}

fn write_root_manifest(root: &Path) -> AnyResult<()> {
    let mut paths = contract_source_inventory(root)?;
    let actual = paths
        .iter()
        .map(|(relative, _)| relative.as_str())
        .collect::<BTreeSet<_>>();
    let expected = expected_source_paths();
    if actual != expected.iter().map(String::as_str).collect() {
        return Err("contract source inventory differs from the pinned root inventory".into());
    }
    paths.sort_by(|left, right| left.0.cmp(&right.0));
    let files = paths
        .into_iter()
        .map(|(relative, path)| {
            let bytes = fs::read(&path)
                .unwrap_or_else(|error| panic!("manifest input {}: {error}", path.display()));
            json!({"path":relative,"bytes":bytes.len(),"sha256":sha256(&bytes)})
        })
        .collect::<Vec<_>>();
    write_json(
        &root.join("manifest.json"),
        &json!({
            "schema_version": CONTRACT_VERSION,
            "algorithm": "SHA-256",
            "self_hash": "excluded_by_definition_to_avoid_a_self-referential_digest",
            "files": files
        }),
    )
}

fn contract_source_inventory(root: &Path) -> AnyResult<Vec<(String, PathBuf)>> {
    let mut paths = vec![(
        "../../../docs/adr/0009-private-broker-bootstrap-and-target-transport.md".to_owned(),
        root.join("../../../docs/adr/0009-private-broker-bootstrap-and-target-transport.md"),
    )];
    collect_contract_files(root, root, &mut paths)?;
    paths.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(paths)
}

fn collect_contract_files(
    root: &Path,
    directory: &Path,
    paths: &mut Vec<(String, PathBuf)>,
) -> AnyResult<()> {
    let mut entries = fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    for path in entries {
        let relative = path
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        if relative == "manifest.json" {
            continue;
        }
        if path.is_dir() {
            collect_contract_files(root, &path, paths)?;
        } else if path.is_file() {
            paths.push((relative, path));
        }
    }
    Ok(())
}

fn vector_names() -> [&'static str; 9] {
    [
        "binding-replay-failures.json",
        "bootstrap-android-descriptor.json",
        "bootstrap-nk-success.json",
        "broker-ipc-boundaries.json",
        "frame-failures.json",
        "ios-app-artifact-tree.json",
        "lifecycle-dispatch.json",
        "secret-surface-canaries.json",
        "session-nnpsk0-success.json",
    ]
}

fn fixture_names() -> [&'static str; 22] {
    [
        "d0-1-packet-cap-plus-one.json",
        "d0-2-impossible-catalog-list-evidence.json",
        "d0-3-client-rewritten-ready-timestamps.json",
        "d0-4-unicode-path-byte-cap.json",
        "d0-4b-broker-json-cbor-roundtrip.json",
        "d0-4c-error-message-byte-cap.json",
        "d0-5-cross-binding-handshake-aead.json",
        "d0-5b-cross-target-wrong-prologue.json",
        "d0-6a-cbor-duplicate-key.json",
        "d0-6b-outer-frame-truncated.json",
        "d0-6c-record-gap.json",
        "d0-6d-target-ref-second-redeem.json",
        "d0-6e-secret-surface-canary-hit.json",
        "d0-6e2-secret-surface-both-canaries-absent.json",
        "d0-6f-immediate-finished-replay.json",
        "d0-7-missing-helper-and-smoke-artifact.json",
        "d0-7b-zero-byte-build-artifacts.json",
        "d0-final-cbor-depth.json",
        "d0-final-dishonest-canary-count.json",
        "d0-final-inconsistent-evidence.json",
        "d0-final-validator-reroute.json",
        "d0-final-wrong-psk.json",
    ]
}

fn expected_source_paths() -> BTreeSet<String> {
    let mut paths = BTreeSet::from([
        "../../../docs/adr/0009-private-broker-bootstrap-and-target-transport.md".to_owned(),
        "README.md".to_owned(),
        "dependencies.lock.json".to_owned(),
        "reference/Cargo.lock".to_owned(),
        "reference/Cargo.toml".to_owned(),
        "reference/src/main.rs".to_owned(),
        "reference/src/verifier.rs".to_owned(),
        "reference/tests/d0_four_p1_remediation.rs".to_owned(),
        "reference/tests/final_remediation.rs".to_owned(),
        "reference/tests/ios_app_artifact_tree.rs".to_owned(),
        "reference/tests/remediation.rs".to_owned(),
        "vectors/manifest.json".to_owned(),
    ]);
    paths.extend(
        fixture_names()
            .into_iter()
            .map(|name| format!("reference/fixtures/{name}")),
    );
    paths.extend(
        [
            "broker-control.schema.json",
            "target-prepare.schema.json",
            "target-ready.schema.json",
            "transport-evidence.schema.json",
        ]
        .into_iter()
        .map(|name| format!("schema/{name}")),
    );
    paths.extend(
        ["bootstrap.cddl", "broker-ipc.cddl", "session.cddl"]
            .into_iter()
            .map(|name| format!("wire/{name}")),
    );
    paths.extend(
        vector_names()
            .into_iter()
            .map(|name| format!("vectors/{name}")),
    );
    paths
}

fn verify_dependency_pin(root: &Path) -> AnyResult<()> {
    let lock = read_json(&root.join("dependencies.lock.json"))?;
    if lock["noise"]["revision"] != 34
        || lock["noise"]["sha256"]
            != "44f249557aa2a21f819ba3dde54a677476d585660036e4a52f83a8a781eddcf6"
        || lock["snow"]["version"] != "0.10.0"
        || lock["snow"]["crate_sha256"]
            != "599b506ccc4aff8cf7844bc42cf783009a434c1e26c964432560fb6d6ad02d82"
    {
        return Err("Noise/snow dependency pin mismatch".into());
    }
    let cargo_lock = fs::read_to_string(root.join("reference/Cargo.lock"))?;
    if !cargo_lock.contains("name = \"snow\"\nversion = \"0.10.0\"")
        || !cargo_lock.contains(
            "checksum = \"599b506ccc4aff8cf7844bc42cf783009a434c1e26c964432560fb6d6ad02d82\"",
        )
    {
        return Err("reference Cargo.lock snow pin mismatch".into());
    }
    if !cargo_lock.contains("name = \"plist\"\nversion = \"1.8.0\"") {
        return Err("reference Cargo.lock plist pin mismatch".into());
    }
    let cargo_toml = fs::read_to_string(root.join("reference/Cargo.toml"))?;
    for required in [
        "publish = false",
        "default-features = false",
        "\"default-resolver\"",
        "\"use-chacha20poly1305\"",
        "\"use-curve25519\"",
        "\"use-getrandom\"",
        "\"use-sha2\"",
        "plist = \"=1.8.0\"",
        "[workspace]",
    ] {
        if !cargo_toml.contains(required) {
            return Err(format!("reference Cargo.toml lacks {required}").into());
        }
    }
    let production_workspace = fs::read_to_string(root.join("../../../cli/Cargo.toml"))?;
    if production_workspace.contains("apppilotkit-transport-contract-reference") {
        return Err("production workspace depends on reference crate".into());
    }
    Ok(())
}

fn verify_schemas(root: &Path) -> AnyResult<()> {
    let names = [
        "broker-control.schema.json",
        "target-prepare.schema.json",
        "target-ready.schema.json",
        "transport-evidence.schema.json",
    ];
    let mut registry = jsonschema::Registry::new().retriever(RejectExternal);
    let mut schemas = Vec::new();
    for name in names {
        let schema = read_json(&root.join("schema").join(name))?;
        jsonschema::draft202012::meta::validate(&schema).map_err(|error| error.to_string())?;
        if schema["$schema"] != "https://json-schema.org/draft/2020-12/schema" {
            return Err(format!("{name} is not Draft 2020-12").into());
        }
        let id = schema["$id"]
            .as_str()
            .ok_or("schema missing $id")?
            .to_owned();
        registry = registry.add(id, schema.clone())?;
        schemas.push((name, schema));
    }
    let registry = registry.prepare()?;
    for (_, schema) in &schemas {
        jsonschema::draft202012::options()
            .with_registry(&registry)
            .with_retriever(RejectExternal)
            .build(schema)
            .map_err(|error| error.to_string())?;
    }
    let ready = json!({"schema_version":"1.0","target":format!("target_{}", "A".repeat(43)),"issued_at_unix_ms":1000,"expires_at_unix_ms":31000});
    validate_schema(
        &registry,
        "https://apppilotkit.dev/transport/contracts/v1/schema/target-ready.schema.json",
        &ready,
    )?;
    verifier::validate_json_semantics(&ready)?;
    validate_ready_semantics(&ready)?;
    let invalid_ready = json!({"schema_version":"1.0","target":format!("target_{}", "A".repeat(43)),"issued_at_unix_ms":1000,"expires_at_unix_ms":30000});
    if validate_ready_semantics(&invalid_ready).is_ok() {
        return Err("ready-target semantic validator accepted a non-30000ms window".into());
    }
    let prepare = json!({"schema_version":"1.0","platform":"ios-simulator","device_selector":"00000000-0000-0000-0000-000000000000","app_id":"dev.apppilotkit.smoke","app_artifact":"/tmp/TransportSmokeHost.app","artifact_encoding":"ios-app-tree-v1"});
    validate_schema(
        &registry,
        "https://apppilotkit.dev/transport/contracts/v1/schema/target-prepare.schema.json",
        &prepare,
    )?;
    let mut wrong_prepare_encoding = prepare.clone();
    wrong_prepare_encoding["artifact_encoding"] = json!("raw-file-v1");
    assert_schema_rejected(
        &registry,
        "https://apppilotkit.dev/transport/contracts/v1/schema/target-prepare.schema.json",
        &wrong_prepare_encoding,
        "iOS raw-file artifact encoding",
    )?;
    let prepare_success = json!({"schema_version":"1.0","status":"succeeded","ready_target":ready});
    validate_schema(
        &registry,
        "https://apppilotkit.dev/transport/contracts/v1/schema/target-prepare.schema.json",
        &prepare_success,
    )?;
    let session_open = br#"{"jsonrpc":"2.0","id":"open-contract","method":"session.open","params":{"client":{"name":"apppilotkit","version":"0.1.0"},"protocol":{"major":1,"minMinor":2,"maxMinor":2},"requiredCapabilities":["semantic.catalog"]}}"#;
    let broker = json!({
        "schema_version":"1.0","request_id":"AAAAAAAAAAAAAAAAAAAAAA","deadline_unix_ms":31000,"operation":"open_session",
        "body":{"target":format!("target_{}", "A".repeat(43)),"required_capabilities":["semantic.catalog"],
        "session_open_request_base64url":URL_SAFE_NO_PAD.encode(session_open),"session_open_request_sha256":sha_field(session_open)}
    });
    validate_schema(
        &registry,
        "https://apppilotkit.dev/transport/contracts/v1/schema/broker-control.schema.json",
        &broker,
    )?;
    let broker_prepare = json!({
        "schema_version":"1.0","request_id":"AAAAAAAAAAAAAAAAAAAAAA","deadline_unix_ms":31000,"operation":"prepare",
        "body":{"platform":"ios-simulator","device_selector":"00000000-0000-0000-0000-000000000000",
        "app_id":"dev.apppilotkit.smoke","app_artifact":"/tmp/TransportSmokeHost.app",
        "artifact_encoding":"ios-app-tree-v1","app_artifact_sha256":sha_field(b"canonical-app-stream")}
    });
    validate_schema(
        &registry,
        "https://apppilotkit.dev/transport/contracts/v1/schema/broker-control.schema.json",
        &broker_prepare,
    )?;
    verifier::validate_json_semantics(&broker)?;
    let broker_ready = json!({
        "schema_version":"1.0","request_id":"AAAAAAAAAAAAAAAAAAAAAA","status":"succeeded",
        "result":{"kind":"target_ready","target":format!("target_{}", "A".repeat(43)),"process_generation":PROCESS_GENERATION,
        "listener_epoch":1,"issued_at_unix_ms":1000,"expires_at_unix_ms":31000}
    });
    validate_schema(
        &registry,
        "https://apppilotkit.dev/transport/contracts/v1/schema/broker-control.schema.json",
        &broker_ready,
    )?;
    validate_ready_semantics(&broker_ready["result"])?;
    let evidence = evidence_example();
    validate_schema(
        &registry,
        "https://apppilotkit.dev/transport/contracts/v1/schema/transport-evidence.schema.json",
        &evidence,
    )?;
    verifier::validate_evidence_semantics(root, &evidence)?;
    let mut equal_session_ids = evidence.clone();
    let primary_id = equal_session_ids["concurrent_sessions"][0]["id_digest"].clone();
    equal_session_ids["concurrent_sessions"][1]["id_digest"] = primary_id.clone();
    equal_session_ids["concurrent_sessions"][1]["request"]["session_id_digest"] =
        primary_id.clone();
    equal_session_ids["concurrent_sessions"][1]["response"]["session_id_digest"] =
        primary_id.clone();
    equal_session_ids["concurrent_sessions"][1]["runtime"]["session_id_digest"] = primary_id;
    assert_evidence_rejected(
        &registry,
        root,
        &equal_session_ids,
        "equal session ids despite fresh_session_ids",
    )?;
    for fact in ["request", "response", "runtime"] {
        let mut swapped = evidence.clone();
        let left = swapped["concurrent_sessions"][0][fact].clone();
        swapped["concurrent_sessions"][0][fact] = swapped["concurrent_sessions"][1][fact].clone();
        swapped["concurrent_sessions"][1][fact] = left;
        assert_evidence_rejected(
            &registry,
            root,
            &swapped,
            &format!("swapped concurrent session {fact} facts"),
        )?;
    }
    let mut missing_fact = evidence.clone();
    missing_fact["concurrent_sessions"][0]
        .as_object_mut()
        .ok_or("session evidence is not an object")?
        .remove("request");
    assert_evidence_rejected(
        &registry,
        root,
        &missing_fact,
        "missing session request facts",
    )?;
    let mut missing_inner_id = evidence.clone();
    missing_inner_id["concurrent_sessions"][0]["response"]
        .as_object_mut()
        .ok_or("session response evidence is not an object")?
        .remove("session_id_digest");
    assert_evidence_rejected(
        &registry,
        root,
        &missing_inner_id,
        "missing inner session digest",
    )?;
    let mut unmatched_primary = evidence.clone();
    let unmatched_request = Value::String(sha_field(b"unmatched-primary-request"));
    unmatched_primary["session"]["request"]["sha256"] = unmatched_request.clone();
    unmatched_primary["session"]["runtime"]["request_sha256"] = unmatched_request;
    assert_evidence_rejected(
        &registry,
        root,
        &unmatched_primary,
        "primary session unlike either concurrent session",
    )?;
    for (label, mutation) in [
        (
            "non-SemVer CLI version",
            ("/cli_version", json!("not-semver")),
        ),
        (
            "wrong CLI contract version",
            ("/schema_version", json!("1.1")),
        ),
        (
            "integer CLI contract version",
            ("/schema_version", json!(1)),
        ),
        (
            "top-level Machine Result extra",
            ("/unexpected", json!(true)),
        ),
        ("Disclosure extra", ("/disclosure/unexpected", json!(true))),
        (
            "catalog capability extra",
            ("/data/capabilities/0/unexpected", json!(true)),
        ),
        (
            "Next Action extra",
            ("/next_actions/0/unexpected", json!(true)),
        ),
    ] {
        let mut hostile = evidence.clone();
        let mut machine = retained_machine_result(&hostile)?;
        let (pointer, replacement) = mutation;
        let (parent_pointer, key) = pointer
            .rsplit_once('/')
            .ok_or_else(|| format!("invalid Machine Result mutation pointer: {pointer}"))?;
        machine
            .pointer_mut(parent_pointer)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| format!("Machine Result mutation parent missing: {parent_pointer}"))?
            .insert(key.to_owned(), replacement);
        replace_retained_machine_result(&mut hostile, &machine)?;
        if verifier::validate_evidence_semantics(root, &hostile).is_ok() {
            return Err(format!("evidence accepted {label}").into());
        }
    }
    let mut wrong_cli_identity = evidence.clone();
    let mut wrong_cli_machine = retained_machine_result(&wrong_cli_identity)?;
    wrong_cli_machine["cli_version"] = Value::String("0.1.1".to_owned());
    replace_retained_machine_result(&mut wrong_cli_identity, &wrong_cli_machine)?;
    if verifier::validate_evidence_semantics(root, &wrong_cli_identity).is_ok() {
        return Err(
            "evidence accepted a Machine Result CLI version unlike the installed CLI".into(),
        );
    }
    let mut fake_machine_result = evidence.clone();
    let fake_stdout = br#"{"status":"succeeded"}
"#;
    fake_machine_result["command"]["retained_stdout"]["bytes_base64url"] =
        Value::String(URL_SAFE_NO_PAD.encode(fake_stdout));
    fake_machine_result["command"]["retained_stdout"]["byte_count"] =
        Value::from(fake_stdout.len());
    fake_machine_result["command"]["retained_stdout"]["sha256"] =
        Value::String(sha_field(fake_stdout));
    fake_machine_result["terminal"]["machine_result_sha256"] =
        Value::String(sha_field(fake_stdout));
    if verifier::validate_evidence_semantics(root, &fake_machine_result).is_ok() {
        return Err("evidence accepted semantically fake Machine Result bytes".into());
    }
    for hostile_date in [
        "2026-02-29T00:00:00Z",
        "2024-04-31T00:00:00Z",
        "2026-08-30T24:00:00Z",
        "2026-08-30T00:00:00+24:00",
    ] {
        let mut hostile = evidence.clone();
        hostile["recorded_at"] = Value::String(hostile_date.to_owned());
        if verifier::validate_evidence_semantics(root, &hostile).is_ok() {
            return Err(format!(
                "evidence semantic validator accepted hostile date {hostile_date}"
            )
            .into());
        }
    }
    let mut platform_mismatch = evidence.clone();
    platform_mismatch["tool"]["name"] = Value::String("adb".to_owned());
    if validate_schema(
        &registry,
        "https://apppilotkit.dev/transport/contracts/v1/schema/transport-evidence.schema.json",
        &platform_mismatch,
    )
    .is_ok()
    {
        return Err("evidence schema accepted a platform/tool mismatch".into());
    }
    let mut incomplete_scan = evidence.clone();
    incomplete_scan["secret_surface"]["surfaces"]
        .as_array_mut()
        .ok_or("evidence surface array missing")?
        .pop();
    if validate_schema(
        &registry,
        "https://apppilotkit.dev/transport/contracts/v1/schema/transport-evidence.schema.json",
        &incomplete_scan,
    )
    .is_ok()
    {
        return Err("evidence schema accepted an incomplete secret-surface scan".into());
    }
    let mut missing_helper = evidence.clone();
    missing_helper["installed"]["executables"]
        .as_array_mut()
        .ok_or("installed identities missing")?
        .pop();
    assert_schema_rejected(
        &registry,
        "https://apppilotkit.dev/transport/contracts/v1/schema/transport-evidence.schema.json",
        &missing_helper,
        "missing target-prepare helper",
    )?;
    let mut missing_smoke = evidence.clone();
    missing_smoke["secret_surface"]["surfaces"]
        .as_array_mut()
        .ok_or("scan surfaces missing")?
        .retain(|surface| surface["name"] != "smoke_host_build_artifact");
    assert_schema_rejected(
        &registry,
        "https://apppilotkit.dev/transport/contracts/v1/schema/transport-evidence.schema.json",
        &missing_smoke,
        "missing Smoke Host build artifact",
    )?;
    let mut missing_scanner = evidence.clone();
    missing_scanner["secret_surface"]["surfaces"][0]
        .as_object_mut()
        .ok_or("scan surface missing")?
        .remove("scanner");
    assert_schema_rejected(
        &registry,
        "https://apppilotkit.dev/transport/contracts/v1/schema/transport-evidence.schema.json",
        &missing_scanner,
        "missing scanner",
    )?;
    let mut missing_artifact_hash = evidence.clone();
    missing_artifact_hash["secret_surface"]["surfaces"][0]["capture"]
        .as_object_mut()
        .ok_or("scan capture missing")?
        .remove("sha256");
    assert_schema_rejected(
        &registry,
        "https://apppilotkit.dev/transport/contracts/v1/schema/transport-evidence.schema.json",
        &missing_artifact_hash,
        "missing artifact hash",
    )?;
    let mut mixed_canary = evidence.clone();
    mixed_canary["secret_surface"]["execution_canary_digest"] =
        mixed_canary["secret_surface"]["fixed_canary_digest"].clone();
    if verifier::validate_evidence_semantics(root, &mixed_canary).is_ok() {
        return Err("evidence semantic validator accepted mixed canaries".into());
    }
    let mut wrong_installed_path = evidence.clone();
    wrong_installed_path["installed"]["executables"][0]["path"] =
        Value::String("/tmp/unrelated/apppilotkit".to_owned());
    if verifier::validate_evidence_semantics(root, &wrong_installed_path).is_ok() {
        return Err(
            "evidence accepted an invoked executable outside the canonical package root".into(),
        );
    }
    let mut unrelated_machine_result = evidence.clone();
    unrelated_machine_result["terminal"]["machine_result_sha256"] =
        Value::String(sha_field(b"unrelated-machine-result"));
    if verifier::validate_evidence_semantics(root, &unrelated_machine_result).is_ok() {
        return Err("evidence accepted unrelated stdout and Machine Result bytes".into());
    }
    let mut generation_mismatch = evidence.clone();
    generation_mismatch["terminal"]["catalog"]["generation"] =
        Value::Number((PROCESS_GENERATION + 1).into());
    if verifier::validate_evidence_semantics(root, &generation_mismatch).is_ok() {
        return Err("evidence accepted Target/catalog generation mismatch".into());
    }
    let mut dishonest_scan = evidence.clone();
    let canary_bytes = TEST_CANARY.as_bytes();
    dishonest_scan["secret_surface"]["surfaces"][0]["capture"]["bytes_base64url"] =
        Value::String(URL_SAFE_NO_PAD.encode(canary_bytes));
    dishonest_scan["secret_surface"]["surfaces"][0]["capture"]["byte_count"] =
        Value::Number(canary_bytes.len().into());
    dishonest_scan["secret_surface"]["surfaces"][0]["capture"]["sha256"] =
        Value::String(sha_field(canary_bytes));
    if verifier::validate_evidence_semantics(root, &dishonest_scan).is_ok() {
        return Err("evidence accepted dishonest canary match_count=0 for a real hit".into());
    }
    let mut impossible_terminal = evidence.clone();
    impossible_terminal["terminal"]["output_schema_digest"] =
        Value::String(format!("sha256:{}", "0".repeat(64)));
    impossible_terminal["terminal"]["value"] = json!({"ready":true});
    assert_schema_rejected(
        &registry,
        "https://apppilotkit.dev/transport/contracts/v1/schema/transport-evidence.schema.json",
        &impossible_terminal,
        "catalog list value/schema digest",
    )?;
    let unicode_path = format!("/{}", "界".repeat(1366));
    let unicode_prepare = json!({"schema_version":"1.0","platform":"ios-simulator","device_selector":"device","app_id":"dev.apppilotkit.smoke","app_artifact":unicode_path,"artifact_encoding":"ios-app-tree-v1"});
    validate_schema(
        &registry,
        "https://apppilotkit.dev/transport/contracts/v1/schema/target-prepare.schema.json",
        &unicode_prepare,
    )?;
    if verifier::validate_json_semantics(&unicode_prepare).is_ok() {
        return Err("semantic validator accepted Unicode path above UTF-8 byte cap".into());
    }
    for hostile in [
        json!({"schema_version":"1.0","request_id":"AAAAAAAAAAAAAAAAAAAAAB","deadline_unix_ms":31000,"operation":"open_session","body":{"target":format!("target_{}B", "A".repeat(42)),"required_capabilities":["semantic.catalog"]}}),
        json!({"schema_version":"1.0","request_id":"AAAAAAAAAAAAAAAAAAAAAA","deadline_unix_ms":31000,"operation":"exchange","body":{"target":format!("target_{}", "A".repeat(43)),"session":"session_test_0123456789abcdef","process_generation":1,"listener_epoch":1,"message_base64url":"AAAAA","message_sha256":format!("sha256:{}", "0".repeat(64)),"side_effect":"read_only"}}),
    ] {
        assert_schema_rejected(
            &registry,
            "https://apppilotkit.dev/transport/contracts/v1/schema/broker-control.schema.json",
            &hostile,
            "noncanonical base64url",
        )?;
    }
    let mut secret_injected = broker.clone();
    secret_injected["body"]["process_bootstrap_secret"] = Value::String(hex::encode(PBS));
    if validate_schema(
        &registry,
        "https://apppilotkit.dev/transport/contracts/v1/schema/broker-control.schema.json",
        &secret_injected,
    )
    .is_ok()
    {
        return Err("broker schema accepted a secret field".into());
    }
    Ok(())
}

fn assert_schema_rejected(
    registry: &jsonschema::Registry<'_>,
    id: &str,
    instance: &Value,
    label: &str,
) -> AnyResult<()> {
    if validate_schema(registry, id, instance).is_ok() {
        return Err(format!("schema accepted hostile {label}").into());
    }
    Ok(())
}

fn assert_evidence_rejected(
    registry: &jsonschema::Registry<'_>,
    root: &Path,
    instance: &Value,
    label: &str,
) -> AnyResult<()> {
    let schema_id =
        "https://apppilotkit.dev/transport/contracts/v1/schema/transport-evidence.schema.json";
    if validate_schema(registry, schema_id, instance).is_ok()
        && verifier::validate_evidence_semantics(root, instance).is_ok()
    {
        return Err(format!("evidence accepted hostile {label}").into());
    }
    Ok(())
}

fn validate_ready_semantics(value: &Value) -> AnyResult<()> {
    let issued = value["issued_at_unix_ms"]
        .as_u64()
        .ok_or("ready target issued_at_unix_ms missing")?;
    let expires = value["expires_at_unix_ms"]
        .as_u64()
        .ok_or("ready target expires_at_unix_ms missing")?;
    if issued.checked_add(30_000) != Some(expires) {
        return Err("ready target redemption window is not exactly 30000ms".into());
    }
    Ok(())
}

fn evidence_example() -> Value {
    let digest = |bytes: &[u8]| sha_field(bytes);
    let fixed_canary = TEST_CANARY.as_bytes();
    let execution_canary = EXECUTION_CANARY_EXAMPLE.as_bytes();
    let make_app = |build: &str, executable_bytes: &[u8]| {
        let info = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>dev.apppilotkit.smoke</string><key>CFBundlePackageType</key><string>APPL</string><key>CFBundleVersion</key><string>{build}</string><key>CFBundleExecutable</key><string>SmokeHost</string></dict></plist>"
        );
        verifier::encode_ios_app_tree(
            &[
                verifier::IosAppSourceEntry::file("Info.plist", info.into_bytes(), 0o644),
                verifier::IosAppSourceEntry::file("SmokeHost", executable_bytes, 0o755),
            ],
            "dev.apppilotkit.smoke",
            Some(build),
        )
        .expect("valid iOS app evidence fixture")
    };
    let smoke_app = make_app("test-build", b"smoke-app");
    let production_app = make_app("production-build", b"production-app");
    let release_app = make_app("release-build", b"release-app");
    let redacted_command_argv = json!([
        "/tmp/apppilotkit-prefix/bin/apppilotkit",
        "catalog",
        "list",
        "--target=<redacted>",
        "--output=json",
        "--non-interactive"
    ]);
    let redacted_next_argv = json!([
        "/tmp/apppilotkit-prefix/bin/apppilotkit",
        "catalog",
        "show",
        "--capability",
        "smoke.ready",
        "--declaration-revision",
        "1",
        "--session=<redacted>",
        "--target=<redacted>",
        "--output",
        "json",
        "--non-interactive"
    ]);
    let machine_result = json!({
        "schema_version":"1.0","cli_version":"0.1.0","status":"succeeded",
        "command":["catalog","list"],"side_effect":"read_only","retry_safety":"safe",
        "data":{"catalog":{"id":"catalog_smoke_01234567","generation":PROCESS_GENERATION},
            "capabilities":[{"id":"smoke.ready","kind":"resource","declaration_revision":1}]},
        "disclosure":{"truncated":false,"returned_items":1},"artifacts":[],
        "next_actions":[{"id":"catalog.show","argv":redacted_next_argv,"side_effect":"read_only",
            "retry_safety":"safe","preconditions":["session is still valid"],
            "reason":"Inspect the first Semantic Capability using the same Target-issued Session"}]
    });
    let mut stdout = serde_json::to_vec(&machine_result).expect("Machine Result encodes");
    stdout.push(b'\n');
    let next_actions_bytes =
        serde_json::to_vec(&machine_result["next_actions"]).expect("Next Actions encode");
    let argv_bytes = serde_json::to_vec(&redacted_command_argv).expect("argv encodes");
    let executable_material = [
        (
            "apppilotkit",
            "/tmp/apppilotkit-prefix/bin/apppilotkit",
            b"cli-binary".as_slice(),
        ),
        (
            "apppilotkit-broker",
            "/tmp/apppilotkit-prefix/libexec/apppilotkit-broker",
            b"broker-binary".as_slice(),
        ),
        (
            "apppilotkit-target-prepare",
            "/tmp/apppilotkit-prefix/libexec/apppilotkit-target-prepare",
            b"prepare-binary".as_slice(),
        ),
    ];
    let mut package_bytes = Vec::new();
    for (name, _, bytes) in &executable_material {
        package_bytes.extend_from_slice(name.as_bytes());
        package_bytes.push(0);
        package_bytes.extend_from_slice(digest(bytes).as_bytes());
        package_bytes.push(b'\n');
    }
    let surfaces = [
        "argv",
        "environment",
        "activity_extras",
        "stdout",
        "stderr",
        "product_logs",
        "diagnostics",
        "machine_result",
        "next_actions",
        "artifacts",
        "smoke_host_build_artifact",
        "production_build_artifact",
        "release_build_artifact",
    ]
    .into_iter()
    .map(|name| {
        let bytes = match name {
            "stdout" | "machine_result" => stdout.clone(),
            "stderr" => Vec::new(),
            "next_actions" => next_actions_bytes.clone(),
            "argv" => argv_bytes.clone(),
            "smoke_host_build_artifact" => smoke_app.clone(),
            "production_build_artifact" => production_app.clone(),
            "release_build_artifact" => release_app.clone(),
            _ => format!("capture:{name}").into_bytes(),
        };
        let mut surface = json!({
            "name":name,"scanner":"apppilotkit-reference-byte-scanner","scanner_version":"1.0",
            "operation":"literal-byte-subsequence-count",
            "capture":{"identity":format!("capture:{name}"),"path":format!("/tmp/apppilotkit-evidence/{name}.capture"),
            "sha256":digest(&bytes),"byte_count":bytes.len(),"bytes_base64url":URL_SAFE_NO_PAD.encode(&bytes)},"fixed_canary_match_count":0,
            "execution_canary_match_count":0,"complete":true
        });
        if name.ends_with("_build_artifact") {
            let build = match name {
                "smoke_host_build_artifact" => "test-build",
                "production_build_artifact" => "production-build",
                "release_build_artifact" => "release-build",
                _ => unreachable!(),
            };
            let configuration = match name {
                "smoke_host_build_artifact" => "debug_internal",
                "production_build_artifact" => "production",
                "release_build_artifact" => "release",
                _ => unreachable!(),
            };
            surface["artifact_identity"] = json!({
                "app_id":"dev.apppilotkit.smoke","build":build,"configuration":configuration,
                "artifact_encoding":"ios-app-tree-v1",
                "artifact_sha256":digest(&bytes)
            });
        }
        surface
    })
    .collect::<Vec<_>>();
    let installed = executable_material
    .into_iter()
    .map(|(name, path, bytes)| {
        json!({
            "name":name,"path":path,"version":"0.1.0","sha256":digest(bytes),"bytes_base64url":URL_SAFE_NO_PAD.encode(bytes),
            "signature":"unsigned-local-checkpoint","build":"test-build","arch":"arm64"
        })
    })
    .collect::<Vec<_>>();
    json!({
        "schema_version":"1.0",
        "base_commit":"09c846d86d0a18b0ccc6ca2e3fc6f00c305425b3",
        "recorded_at":"2026-08-30T00:00:00Z",
        "platform":"ios-simulator",
        "form_factor":"phone",
        "os_version":"iOS 26.3",
        "host":{"os":"macOS 26.6.1","arch":"arm64"},
        "tool":{"name":"simctl","version":"Xcode 26.2"},
        "app":{"id":"dev.apppilotkit.smoke","build":"test-build","artifact_encoding":"ios-app-tree-v1","artifact_sha256":digest(&smoke_app),"artifact_bytes_base64url":URL_SAFE_NO_PAD.encode(&smoke_app),"release_excluded":true},
        "installed":{
            "prefix":"/tmp/apppilotkit-prefix","package_sha256":digest(&package_bytes),"executables":installed
        },
        "broker":{"pid":123,"euid":501,"start_mode":"on_demand_current_user","runtime_dir_mode":"0700","socket_mode":"0600","peer_euid_verified":true},
        "target":{"reference_digest":digest(b"target-ref"),"transport":"ios_simulator_loopback_nk","lease_digest":digest(b"lease"),"process_generation":PROCESS_GENERATION,"listener_epoch":1},
        "session":{"id_digest":digest(b"session-id"),"noise_handshake_hash_hex":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","target_issued":true,
            "request":{"session_id_digest":digest(b"session-id"),"sha256":digest(b"session-a-request")},
            "response":{"session_id_digest":digest(b"session-id"),"sha256":digest(b"session-a-response")},
            "runtime":{"session_id_digest":digest(b"session-id"),"instance_digest":digest(b"session-a-runtime"),
                "request_sha256":digest(b"session-a-request"),"response_sha256":digest(b"session-a-response")}},
        "concurrent_sessions":[
            {"id_digest":digest(b"session-id"),"noise_handshake_hash_hex":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","target_issued":true,
                "request":{"session_id_digest":digest(b"session-id"),"sha256":digest(b"session-a-request")},
                "response":{"session_id_digest":digest(b"session-id"),"sha256":digest(b"session-a-response")},
                "runtime":{"session_id_digest":digest(b"session-id"),"instance_digest":digest(b"session-a-runtime"),
                    "request_sha256":digest(b"session-a-request"),"response_sha256":digest(b"session-a-response")}},
            {"id_digest":digest(b"session-id-b"),"noise_handshake_hash_hex":"1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","target_issued":true,
                "request":{"session_id_digest":digest(b"session-id-b"),"sha256":digest(b"session-b-request")},
                "response":{"session_id_digest":digest(b"session-id-b"),"sha256":digest(b"session-b-response")},
                "runtime":{"session_id_digest":digest(b"session-id-b"),"instance_digest":digest(b"session-b-runtime"),
                    "request_sha256":digest(b"session-b-request"),"response_sha256":digest(b"session-b-response")}}
        ],
        "session_isolation":{"fresh_handshakes":true,"fresh_session_ids":true,"close_a_b_remained_open":true,"idle_a_b_remained_open":true,"auth_a_b_remained_open":true,"lease_loss_staled_both":true},
        "protocol":{"major":1,"minor":2,"capabilities":["semantic.catalog","session.core"],"max_request_bytes":16777216,"max_response_bytes":67108864,"max_page_items":10000},
        "command":{"redacted_argv":redacted_command_argv,
            "retained_stdout":{"identity":"terminal:stdout","path":"/tmp/apppilotkit-evidence/stdout.capture","sha256":digest(&stdout),"byte_count":stdout.len(),"bytes_base64url":URL_SAFE_NO_PAD.encode(&stdout)},
            "stdout_redactions":[
                {"json_pointer":"/next_actions/0/argv/7","original_sha256":digest(b"session-id")},
                {"json_pointer":"/next_actions/0/argv/8","original_sha256":digest(b"target-ref")}],
            "stderr_sha256":digest(b""),"exit_status":0},
        "terminal":{
            "status":"succeeded","machine_result_sha256":digest(&stdout),
            "catalog":{"id":"catalog_smoke_01234567","generation":PROCESS_GENERATION},
            "smoke_ready_declaration":{"id":"smoke.ready","kind":"resource","declaration_revision":1},
            "next_action":{"kind":"catalog.show","target_reference_digest":digest(b"target-ref"),"session_id_digest":digest(b"session-id"),"capability":"smoke.ready","declaration_revision":1,
                "redacted_argv":["/tmp/apppilotkit-prefix/bin/apppilotkit","catalog","show","--capability","smoke.ready","--declaration-revision","1","--session=<redacted>","--target=<redacted>","--output","json","--non-interactive"]},
            "transport_handoff":"handoff_possible_or_confirmed"
        },
        "cleanup":{"status":"complete","owned_resources_remaining":0,"duration_ms":100},
        "secret_surface":{"fixed_canary_digest":digest(fixed_canary),"fixed_canary_base64url":URL_SAFE_NO_PAD.encode(fixed_canary),
            "execution_canary_digest":digest(execution_canary),"execution_canary_base64url":URL_SAFE_NO_PAD.encode(execution_canary),"surfaces":surfaces,"complete":true},
        "evidence_class":"real_installed_smoke_host_journey"
    })
}

fn retained_machine_result(evidence: &Value) -> AnyResult<Value> {
    let encoded = evidence["command"]["retained_stdout"]["bytes_base64url"]
        .as_str()
        .ok_or("retained Machine Result bytes missing")?;
    Ok(serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded)?)?)
}

fn replace_retained_machine_result(evidence: &mut Value, machine: &Value) -> AnyResult<()> {
    let mut stdout = serde_json::to_vec(machine)?;
    stdout.push(b'\n');
    let encoded = Value::String(URL_SAFE_NO_PAD.encode(&stdout));
    let digest = Value::String(sha_field(&stdout));
    for pointer in [
        "/command/retained_stdout",
        "/secret_surface/surfaces/3/capture",
        "/secret_surface/surfaces/7/capture",
    ] {
        let capture = evidence
            .pointer_mut(pointer)
            .ok_or_else(|| format!("capture mutation pointer missing: {pointer}"))?;
        capture["bytes_base64url"] = encoded.clone();
        capture["byte_count"] = Value::from(stdout.len());
        capture["sha256"] = digest.clone();
    }
    evidence["terminal"]["machine_result_sha256"] = Value::String(sha_field(&stdout));
    Ok(())
}

fn validate_schema(
    registry: &jsonschema::Registry<'_>,
    id: &str,
    instance: &Value,
) -> AnyResult<()> {
    let validator = jsonschema::draft202012::options()
        .with_registry(registry)
        .with_retriever(RejectExternal)
        .build(&json!({"$ref":id}))
        .map_err(|error| error.to_string())?;
    validator
        .validate(instance)
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn verify_cddl(root: &Path) -> AnyResult<()> {
    let broker = fs::read_to_string(root.join("wire/broker-ipc.cddl"))?;
    let bootstrap = fs::read_to_string(root.join("wire/bootstrap.cddl"))?;
    let session = fs::read_to_string(root.join("wire/session.cddl"))?;
    for (name, source) in [
        ("broker-ipc.cddl", broker.as_str()),
        ("bootstrap.cddl", bootstrap.as_str()),
        ("session.cddl", session.as_str()),
    ] {
        cddl::cddl_from_str(source, false)
            .map_err(|error| format!("{name} syntax error: {error}"))?;
    }
    for required in [
        "67109120",
        "target-reference-token",
        "session-open-request",
        "session-open-response",
        "issued-at-unix-ms",
        "expires-at-unix-ms",
        "deterministic CBOR",
        "close-reason",
        "artifact-encoding",
    ] {
        if !broker.contains(required) {
            return Err(format!("broker-ipc.cddl lacks {required}").into());
        }
    }
    if !broker.contains("error-kind = 0 / 1 / 2 / 3 / 4\n") {
        return Err("broker-ipc.cddl error-kind inventory is not pinned".into());
    }
    for required in [
        "Noise_NK_25519_ChaChaPoly_SHA256",
        "process-bootstrap-secret",
        "process-generation",
        "listener-epoch",
        "ios-app-tree-v1",
    ] {
        if !bootstrap.contains(required) {
            return Err(format!("bootstrap.cddl lacks {required}").into());
        }
    }
    for required in [
        "Noise_NNpsk0_25519_ChaChaPoly_SHA256",
        "12-byte",
        "16777216",
        "67108864",
        "2^32",
        "2^40",
        "session.open",
    ] {
        if !session.contains(required) {
            return Err(format!("session.cddl lacks {required}").into());
        }
    }
    Ok(())
}

fn verify_vectors(root: &Path) -> AnyResult<u64> {
    let bootstrap = read_json(&root.join("vectors/bootstrap-nk-success.json"))?;
    let session = read_json(&root.join("vectors/session-nnpsk0-success.json"))?;
    for field in [
        "launch_descriptor_cbor_hex",
        "prologue_cbor_hex",
        "m1_payload_cbor_hex",
        "m2_payload_cbor_hex",
        "ack_payload_cbor_hex",
    ] {
        let bytes = hex::decode(
            bootstrap["canonical_input"][field]
                .as_str()
                .ok_or("bootstrap CBOR field missing")?,
        )?;
        verifier::validate_deterministic_cbor(&bytes)
            .map_err(|error| format!("bootstrap {field}: {error}"))?;
    }
    for field in [
        "prologue_cbor_hex",
        "target_finished_cbor_hex",
        "broker_finished_cbor_hex",
    ] {
        let bytes = hex::decode(
            session["canonical_input"][field]
                .as_str()
                .ok_or("session CBOR field missing")?,
        )?;
        verifier::validate_deterministic_cbor(&bytes)
            .map_err(|error| format!("session {field}: {error}"))?;
    }
    verifier::validate_target_reference_roundtrip(
        bootstrap["canonical_input"]["target_reference"]
            .as_str()
            .ok_or("bootstrap Target Reference missing")?,
    )?;
    for field in ["m1_outer_hex", "m2_outer_hex", "ack_outer_hex"] {
        verify_outer_hex_literal(&bootstrap["expected"][field], field)?;
    }
    for field in [
        "m1_outer_hex",
        "m2_outer_hex",
        "target_finished_outer_hex",
        "broker_finished_outer_hex",
        "session_open_outer_hex",
        "session_open_response_outer_hex",
    ] {
        verify_outer_hex_literal(&session["expected"][field], field)?;
    }

    let boundaries = read_json(&root.join("vectors/broker-ipc-boundaries.json"))?;
    let goldens = &boundaries["goldens"];
    for (field, literal) in [
        ("max_exchange_request_body_bytes", 16_777_216_u64),
        ("max_exchange_request_cbor_overhead_bytes", 264),
        ("max_exchange_request_cbor_bytes", 16_777_480),
        ("max_exchange_request_packet_bytes", 16_777_484),
        ("max_exchange_response_body_bytes", 67_108_864),
        ("max_exchange_response_cbor_overhead_bytes", 256),
        ("max_exchange_response_cbor_bytes", 67_109_120),
        ("max_exchange_response_packet_bytes", 67_109_124),
        ("max_open_session_cbor_bytes", 73_728),
        ("max_open_session_packet_bytes", 73_732),
        ("global_cbor_cap", MAX_BROKER_CBOR_BYTES),
        ("global_packet_cap", MAX_BROKER_CBOR_BYTES + 4),
    ] {
        if goldens[field].as_u64() != Some(literal) {
            return Err(format!("Broker packet boundary golden {field} mismatch").into());
        }
    }
    let target_ready = &boundaries["broker_target_ready"];
    if hex::decode(
        target_ready["target_reference_token_hex"]
            .as_str()
            .ok_or("target-ready token missing")?,
    )?
    .len()
        != 32
        || target_ready["issued_at_unix_ms"]
            .as_u64()
            .and_then(|issued| issued.checked_add(30_000))
            != target_ready["expires_at_unix_ms"].as_u64()
        || target_ready["prepare_projection"] != "byte_identical"
    {
        return Err("Broker target-ready timestamp/token golden mismatch".into());
    }

    let mut executed = 0_u64;
    for name in vector_names() {
        let value = read_json(&root.join("vectors").join(name))?;
        verify_case_inventory(name, &value)?;
        if value["test_only_material"]["classification"] != "TEST-ONLY" {
            return Err(format!("{name} lacks TEST-ONLY classification").into());
        }
        if let Some(cases) = value["vectors"].as_array() {
            for case in cases {
                let canonical = canonical_json(&case["canonical_input"]);
                if case["expected_transcript_sha256"] != sha_field(canonical.as_bytes()) {
                    return Err(format!("{} case transcript hash drift", case["id"]).into());
                }
                if case["oracle"] == "parser_lifecycle_contract"
                    && !case["cryptographic_expected_bytes_hex"].is_null()
                {
                    return Err(format!("{} fabricates crypto bytes", case["id"]).into());
                }
                verifier::verify_vector_case(root, case, MAX_BROKER_CBOR_BYTES)?;
                executed += 1;
            }
        }
    }
    let mut fixture_paths = fs::read_dir(root.join("reference/fixtures"))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    fixture_paths.sort();
    for path in fixture_paths {
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            verifier::verify_fixture(&path, MAX_BROKER_CBOR_BYTES)?;
            executed += 1;
        }
    }
    if executed < 50 {
        return Err(format!("independent negative coverage too small: {executed}").into());
    }
    Ok(executed)
}

fn verify_case_inventory(name: &str, value: &Value) -> AnyResult<()> {
    let expected = expected_case_ids(name);
    let actual = value["vectors"]
        .as_array()
        .map(|cases| {
            cases
                .iter()
                .map(|case| case["id"].as_str().ok_or("vector case id missing"))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    if actual != expected {
        return Err(format!(
            "{name} case inventory mismatch: expected={expected:?} actual={actual:?}"
        )
        .into());
    }
    Ok(())
}

fn expected_case_ids(name: &str) -> Vec<&'static str> {
    match name {
        "binding-replay-failures.json" => vec![
            "equal-generation-control",
            "equal-role-control",
            "cross-target",
            "tamper-nk-message-2",
            "replay-session-finished",
            "cross-lease",
            "cross-generation",
            "old-epoch",
            "wrong-role",
            "authenticated-session-binding-mismatch",
        ],
        "broker-ipc-boundaries.json" => vec![
            "broker-packet-cap-plus-one",
            "broker-control-operation-cap-plus-one",
            "broker-open-session-cap-plus-one",
        ],
        "frame-failures.json" => vec![
            "outer-header-timeout",
            "outer-body-timeout",
            "outer-zero-length",
            "outer-oversize",
            "record-truncated-header",
            "record-trailing-after-end",
            "record-reorder",
            "record-gap",
            "record-overlap",
            "record-interleave",
            "half-duplex-peer-turn",
            "record-unknown-flags",
            "record-nonzero-reserved",
            "record-non-start-total-len",
            "close-record-valid",
            "close-record-invalid-reason",
            "close-record-missing-handoff",
            "close-record-non-shortest-cbor",
            "cbor-duplicate-key",
            "cbor-non-shortest-integer",
            "cbor-out-of-order-key",
            "request-oversize",
            "response-oversize",
            "session-open-oversize",
            "nonce-record-limit",
            "plaintext-byte-limit",
        ],
        "lifecycle-dispatch.json" => vec![
            "ready-timestamps-inconsistent-window",
            "ready-timestamps-expired",
            "ready-timestamps-client-rewrite",
            "target-ref-second-redeem",
            "atomic-close-wins-before-dispatch",
            "ready-reference-expired",
            "session-idle-expired",
            "lease-idle-expired",
            "lease-absolute-expired",
            "heartbeat-timeout",
            "cleanup-failure",
            "pre-dispatch-authentication",
            "pre-dispatch-timeout",
            "post-dispatch-read-timeout",
            "post-dispatch-mutation-eof",
            "prepare-no-lease-launch-bootstrap",
            "prepare-eligible-owned-lease-mints-ref-no-launch-no-bootstrap",
            "prepare-live-conflicting-build-fails-no-relaunch",
            "prepare-reuse-new-bootstrap-transcript-rejected",
            "two-fresh-refs-independent-redemption",
            "two-agent-fresh-noise-and-target-session-ids",
            "concurrent-read-both-complete",
            "close-session-a-session-b-remains-open",
            "session-a-idle-expiry-session-b-remains-open",
            "session-a-auth-failure-session-b-remains-open",
            "lease-loss-stales-both",
            "epoch-loss-stales-both",
            "process-loss-stales-both",
            "broker-heartbeat-loss-stales-both",
            "catalog-complete-nonempty-projects-show",
            "catalog-truncated-projects-continuation",
            "catalog-complete-empty-projects-list-selector",
            "broker-lost-pre-send-read",
            "broker-lost-pre-send-invoke",
            "broker-lost-partial-read",
            "broker-lost-partial-invoke",
            "broker-lost-full-before-response-read",
            "broker-lost-full-before-response-invoke",
            "broker-lost-safe-response-lost-read",
            "broker-lost-safe-response-lost-invoke",
            "broker-lost-response-partial-eof-read",
            "broker-lost-response-partial-eof-invoke",
            "broker-response-complete-read",
            "broker-response-complete-invoke",
        ],
        "secret-surface-canaries.json" => vec![
            "secret-surface-argv",
            "secret-surface-environment",
            "secret-surface-activity_extras",
            "secret-surface-stdout",
            "secret-surface-stderr",
            "secret-surface-product_logs",
            "secret-surface-diagnostics",
            "secret-surface-machine_result",
            "secret-surface-next_actions",
            "secret-surface-artifacts",
            "secret-surface-smoke_host_build_artifact",
            "secret-surface-production_build_artifact",
            "secret-surface-release_build_artifact",
            "secret-surface-dishonest-count",
        ],
        "bootstrap-android-descriptor.json"
        | "bootstrap-nk-success.json"
        | "ios-app-artifact-tree.json"
        | "session-nnpsk0-success.json" => Vec::new(),
        _ => Vec::new(),
    }
}

fn verify_outer_hex_literal(value: &Value, field: &str) -> AnyResult<()> {
    let bytes = hex::decode(value.as_str().ok_or("outer hex is not a string")?)?;
    if bytes.len() < 2 {
        return Err(format!("{field} lacks u16 prefix").into());
    }
    let declared = usize::from(u16::from_be_bytes([bytes[0], bytes[1]]));
    if declared == 0 || bytes.len() != declared + 2 {
        return Err(format!("{field} has invalid u16 prefix").into());
    }
    Ok(())
}

fn verify_manifests(root: &Path) -> AnyResult<()> {
    let vector_manifest = read_json(&root.join("vectors/manifest.json"))?;
    verify_manifest_entries(root, &vector_manifest)?;
    let vector_listed = vector_manifest["files"]
        .as_array()
        .ok_or("vector manifest files missing")?
        .iter()
        .map(|entry| entry["path"].as_str().ok_or("vector manifest path missing"))
        .collect::<Result<Vec<_>, _>>()?;
    let vector_expected = vector_names()
        .iter()
        .map(|name| format!("vectors/{name}"))
        .collect::<Vec<_>>();
    if vector_listed
        != vector_expected
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    {
        return Err("vector manifest inventory is not the exact nine pinned vectors".into());
    }
    let manifest = read_json(&root.join("manifest.json"))?;
    verify_manifest_entries(root, &manifest)?;
    let listed = manifest["files"]
        .as_array()
        .ok_or("root manifest files missing")?
        .iter()
        .filter_map(|entry| entry["path"].as_str())
        .collect::<BTreeSet<_>>();
    let expected = expected_source_paths();
    let actual_source = contract_source_inventory(root)?
        .into_iter()
        .map(|(relative, _)| relative)
        .collect::<BTreeSet<_>>();
    if actual_source != expected {
        return Err(format!(
            "root source reverse inventory mismatch: expected={expected:?} actual={actual_source:?}"
        )
        .into());
    }
    if listed != expected.iter().map(String::as_str).collect() {
        let missing = expected
            .iter()
            .filter(|path| !listed.contains(path.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        let unexpected = listed
            .iter()
            .filter(|path| !expected.contains(**path))
            .copied()
            .collect::<Vec<_>>();
        return Err(format!(
            "root manifest inventory mismatch: missing={missing:?} unexpected={unexpected:?}"
        )
        .into());
    }
    Ok(())
}

fn verify_manifest_entries(root: &Path, manifest: &Value) -> AnyResult<()> {
    if manifest["schema_version"] != CONTRACT_VERSION || manifest["algorithm"] != "SHA-256" {
        return Err("manifest header mismatch".into());
    }
    let entries = manifest["files"]
        .as_array()
        .ok_or("manifest files missing")?;
    let mut prior = None;
    for entry in entries {
        let relative = entry["path"].as_str().ok_or("manifest path missing")?;
        if prior.is_some_and(|value: &str| value >= relative) {
            return Err("manifest paths not strictly sorted".into());
        }
        prior = Some(relative);
        let bytes = fs::read(root.join(relative))?;
        if entry["bytes"] != bytes.len() || entry["sha256"] != sha256(&bytes) {
            return Err(format!("manifest mismatch for {relative}").into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_contract_verifies() {
        verify(&contract_root().expect("contract root")).expect("contract verification");
    }

    #[test]
    fn crypto_vectors_are_deterministic() {
        let first = crypto_vectors().expect("first vectors");
        let second = crypto_vectors().expect("second vectors");
        assert_eq!(first.bootstrap, second.bootstrap);
        assert_eq!(first.android_descriptor, second.android_descriptor);
        assert_eq!(first.session, second.session);
        assert_eq!(first.binding, second.binding);
        assert_eq!(first.lifecycle, second.lifecycle);
    }

    #[test]
    fn generated_contract_includes_an_android_launch_descriptor_positive() {
        assert!(
            vector_names().contains(&"bootstrap-android-descriptor.json"),
            "generator has no checked-in Android launch descriptor positive"
        );
    }

    #[test]
    fn root_manifest_rejects_duplicate_paths_even_when_bytes_and_hash_match() {
        let root = contract_root().expect("contract root");
        let mut manifest = read_json(&root.join("manifest.json")).expect("root manifest");
        let duplicate = manifest["files"]
            .as_array()
            .and_then(|files| files.last())
            .cloned()
            .expect("manifest entry");
        manifest["files"]
            .as_array_mut()
            .expect("manifest files")
            .push(duplicate);
        assert!(verify_manifest_entries(&root, &manifest).is_err());
    }
}
