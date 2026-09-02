#![allow(dead_code)]

#[path = "../src/verifier.rs"]
mod verifier;

use sha2::{Digest, Sha256};
use std::io::Cursor;
use verifier::{
    IOS_APP_TREE_ENCODING, IOS_APP_TREE_MAGIC, IOS_APP_TREE_MAX_CANONICAL_BYTES,
    IOS_APP_TREE_MAX_ENTRIES, IOS_APP_TREE_MAX_FILE_BYTES, IOS_APP_TREE_MAX_TOTAL_FILE_BYTES,
    IosAppSourceEntry, IosAppSourceKind, checked_ios_total_file_bytes, encode_ios_app_tree,
    literal_match_count, parse_ios_app_tree, validate_artifact_encoded_len,
    validate_ios_declared_file_len, validate_ios_entry_count,
};

const APP_ID: &str = "dev.apppilotkit.smoke";
const BUILD: &str = "62.1";
const EXECUTABLE: &str = "SmokeHost";
const FIXED_CANARY: &[u8] = b"APPPILOTKIT_TEST_ONLY_SECRET_CANARY_7f9c4b2e";
type BorrowedRawRecord<'a> = (u8, &'a [u8], u8, Option<&'a [u8]>);
type OwnedRawRecord = (u8, Vec<u8>, u8, Option<Vec<u8>>);

fn plist(app_id: &str, package_type: &str, build: &str, executable: &str) -> Vec<u8> {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>{app_id}</string><key>CFBundlePackageType</key><string>{package_type}</string><key>CFBundleVersion</key><string>{build}</string><key>CFBundleExecutable</key><string>{executable}</string></dict></plist>"
    )
    .into_bytes()
}

fn valid_entries() -> Vec<IosAppSourceEntry> {
    vec![
        IosAppSourceEntry::file(
            "Info.plist",
            plist(APP_ID, "APPL", BUILD, EXECUTABLE),
            0o644,
        ),
        IosAppSourceEntry::file(EXECUTABLE, b"MACHO".as_slice(), 0o755),
        IosAppSourceEntry::directory("assets"),
        IosAppSourceEntry::file("assets/icon.png", b"PNG".as_slice(), 0o644),
    ]
}

fn push_file_with_parents(entries: &mut Vec<IosAppSourceEntry>, path: &str) {
    let mut offset = 0;
    while let Some(relative) = path[offset..].find('/') {
        offset += relative;
        entries.push(IosAppSourceEntry::directory(
            path.as_bytes()[..offset].to_vec(),
        ));
        offset += 1;
    }
    entries.push(IosAppSourceEntry::file(
        path.as_bytes().to_vec(),
        b"x",
        0o644,
    ));
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn raw_stream(records: &[BorrowedRawRecord<'_>]) -> Vec<u8> {
    let mut bytes = IOS_APP_TREE_MAGIC.to_vec();
    bytes.extend_from_slice(&(records.len() as u32).to_be_bytes());
    for (kind, path, executable_class, file) in records {
        bytes.push(*kind);
        bytes.extend_from_slice(&(path.len() as u32).to_be_bytes());
        bytes.extend_from_slice(path);
        bytes.push(*executable_class);
        if let Some(file) = file {
            bytes.extend_from_slice(&(file.len() as u64).to_be_bytes());
            bytes.extend_from_slice(file);
        }
    }
    bytes
}

fn valid_raw_records() -> Vec<OwnedRawRecord> {
    vec![
        (
            2,
            b"Info.plist".to_vec(),
            0,
            Some(plist(APP_ID, "APPL", BUILD, EXECUTABLE)),
        ),
        (
            2,
            EXECUTABLE.as_bytes().to_vec(),
            1,
            Some(b"MACHO".to_vec()),
        ),
        (1, b"assets".to_vec(), 0, None),
        (2, b"assets/icon.png".to_vec(), 0, Some(b"PNG".to_vec())),
    ]
}

fn encode_raw_records(records: &[OwnedRawRecord]) -> Vec<u8> {
    let borrowed = records
        .iter()
        .map(|(kind, path, executable_class, file)| {
            (*kind, path.as_slice(), *executable_class, file.as_deref())
        })
        .collect::<Vec<_>>();
    raw_stream(&borrowed)
}

#[test]
fn checked_in_golden_has_exact_literal_bytes_and_digest() {
    let vector: serde_json::Value =
        serde_json::from_str(include_str!("../../vectors/ios-app-artifact-tree.json"))
            .expect("golden JSON");
    let bytes = hex::decode(
        vector["expected"]["canonical_hex"]
            .as_str()
            .expect("canonical hex"),
    )
    .expect("canonical bytes");
    assert_eq!(
        vector["expected"]["artifact_sha256"],
        format!("sha256:{}", digest(&bytes))
    );
    assert_eq!(
        vector["expected"]["artifact_sha256"],
        "sha256:ddb5ad55f2e9c3734dd2f52d4cb38e49183d5f4d8aa460d7333ea3c3e783f4df"
    );
    assert_eq!(vector["expected"]["entry_count"], 9);
    assert!(bytes.starts_with(IOS_APP_TREE_MAGIC));
    assert_eq!(
        vector["format"]["record"],
        "kind:u8 || path_len:u32be || path:utf8 || executable_class:u8 || [file_len:u64be || exact_file_bytes]"
    );
    let summary = parse_ios_app_tree(&mut Cursor::new(bytes), APP_ID, Some(BUILD))
        .expect("golden independently parses");
    assert_eq!(summary.records.len(), 9);
}

#[test]
fn ordering_is_canonical_and_excluded_metadata_is_invariant() {
    let mut left = valid_entries();
    left.reverse();
    let mut right = valid_entries();
    for entry in &mut right {
        entry.ignored_mtime_ns = i128::MAX;
        entry.ignored_acl = Some(b"acl-not-copied".to_vec());
        entry
            .ignored_xattrs
            .insert("user.not-copied".to_owned(), b"value".to_vec());
        if matches!(entry.kind, IosAppSourceKind::File(_)) && entry.mode & 0o111 == 0 {
            entry.mode = 0o600;
        }
    }
    assert_eq!(
        encode_ios_app_tree(&left, APP_ID, Some(BUILD)).unwrap(),
        encode_ios_app_tree(&right, APP_ID, Some(BUILD)).unwrap()
    );

    let mut execute_classes = Vec::new();
    for execute_bit in [0o100, 0o010, 0o001] {
        let mut entries = valid_entries();
        entries[1].mode = 0o644 | execute_bit;
        execute_classes.push(encode_ios_app_tree(&entries, APP_ID, Some(BUILD)).unwrap());
    }
    assert!(execute_classes.windows(2).all(|pair| pair[0] == pair[1]));
}

#[test]
fn content_path_kind_and_execute_class_each_change_identity() {
    let baseline = encode_ios_app_tree(&valid_entries(), APP_ID, Some(BUILD)).unwrap();
    let baseline_digest = digest(&baseline);

    let mut content = valid_entries();
    content[3].kind = IosAppSourceKind::File(b"PNH".to_vec());
    let mut path = valid_entries();
    path[3].path = b"assets/icon-2.png".to_vec();
    let mut kind = valid_entries();
    kind[3].kind = IosAppSourceKind::Directory;
    let mut executable = valid_entries();
    executable[3].mode = 0o744;

    for changed in [content, path, kind, executable] {
        let encoded = encode_ios_app_tree(&changed, APP_ID, Some(BUILD)).unwrap();
        assert_ne!(digest(&encoded), baseline_digest);
    }
}

#[test]
fn nfc_nfd_and_case_distinct_paths_are_preserved_without_normalization() {
    let mut entries = valid_entries();
    entries.extend([
        IosAppSourceEntry::file("Cafe\u{301}.txt", b"nfd", 0o644),
        IosAppSourceEntry::file("Caf\u{e9}.txt", b"nfc", 0o644),
        IosAppSourceEntry::file("assets/Icon.png", b"upper", 0o644),
    ]);
    let encoded = encode_ios_app_tree(&entries, APP_ID, Some(BUILD)).unwrap();
    let summary = parse_ios_app_tree(&mut Cursor::new(encoded), APP_ID, Some(BUILD)).unwrap();
    let paths = summary
        .records
        .iter()
        .map(|record| record.path.as_slice())
        .collect::<Vec<_>>();
    assert!(paths.contains(&"Cafe\u{301}.txt".as_bytes()));
    assert!(paths.contains(&"Caf\u{e9}.txt".as_bytes()));
    assert!(paths.contains(&b"assets/Icon.png".as_slice()));
    assert!(paths.contains(&b"assets/icon.png".as_slice()));
}

#[test]
fn prohibited_source_types_resource_forks_and_duplicates_fail_closed() {
    for kind in [
        IosAppSourceKind::Symlink,
        IosAppSourceKind::HardLink,
        IosAppSourceKind::Special,
    ] {
        let mut entries = valid_entries();
        entries.push(IosAppSourceEntry {
            path: b"hostile".to_vec(),
            kind,
            mode: 0,
            has_resource_fork: false,
            ignored_xattrs: Default::default(),
            ignored_acl: None,
            ignored_mtime_ns: 0,
        });
        assert!(encode_ios_app_tree(&entries, APP_ID, Some(BUILD)).is_err());
    }
    let mut resource_fork = valid_entries();
    resource_fork[3].has_resource_fork = true;
    assert!(encode_ios_app_tree(&resource_fork, APP_ID, Some(BUILD)).is_err());
    let mut resource_fork_xattr = valid_entries();
    resource_fork_xattr[3]
        .ignored_xattrs
        .insert("com.apple.ResourceFork".to_owned(), b"fork".to_vec());
    assert!(encode_ios_app_tree(&resource_fork_xattr, APP_ID, Some(BUILD)).is_err());

    let mut duplicate = valid_entries();
    duplicate.push(IosAppSourceEntry::file(
        "assets/icon.png",
        b"duplicate",
        0o644,
    ));
    assert!(encode_ios_app_tree(&duplicate, APP_ID, Some(BUILD)).is_err());
}

#[test]
fn missing_directory_parent_and_file_parent_child_fail_closed() {
    let mut missing_parent = valid_entries();
    missing_parent.push(IosAppSourceEntry::file("missing/child", b"x", 0o644));
    assert!(encode_ios_app_tree(&missing_parent, APP_ID, Some(BUILD)).is_err());

    let mut file_parent = valid_entries();
    file_parent.push(IosAppSourceEntry::file("file-parent", b"parent", 0o644));
    file_parent.push(IosAppSourceEntry::file(
        "file-parent/child",
        b"child",
        0o644,
    ));
    assert!(encode_ios_app_tree(&file_parent, APP_ID, Some(BUILD)).is_err());

    let info = plist(APP_ID, "APPL", BUILD, EXECUTABLE);
    let missing_parent_raw = raw_stream(&[
        (2, b"Info.plist", 0, Some(&info)),
        (2, EXECUTABLE.as_bytes(), 1, Some(b"MACHO")),
        (2, b"missing/child", 0, Some(b"x")),
    ]);
    assert!(parse_ios_app_tree(&mut Cursor::new(missing_parent_raw), APP_ID, Some(BUILD)).is_err());

    let file_parent_raw = raw_stream(&[
        (2, b"Info.plist", 0, Some(&info)),
        (2, EXECUTABLE.as_bytes(), 1, Some(b"MACHO")),
        (2, b"file-parent", 0, Some(b"parent")),
        (2, b"file-parent/child", 0, Some(b"child")),
    ]);
    assert!(parse_ios_app_tree(&mut Cursor::new(file_parent_raw), APP_ID, Some(BUILD)).is_err());
}

#[test]
fn parser_rejects_traversal_invalid_utf8_disorder_duplicate_kind_exec_and_truncation() {
    let info = plist(APP_ID, "APPL", BUILD, EXECUTABLE);
    let traversal = raw_stream(&[
        (2, b"../escape", 0, Some(b"x")),
        (2, b"Info.plist", 0, Some(&info)),
        (2, EXECUTABLE.as_bytes(), 1, Some(b"MACHO")),
    ]);
    assert!(parse_ios_app_tree(&mut Cursor::new(traversal), APP_ID, Some(BUILD)).is_err());

    let invalid_utf8 = raw_stream(&[
        (2, &[0xff], 0, Some(b"x")),
        (2, b"Info.plist", 0, Some(&info)),
        (2, EXECUTABLE.as_bytes(), 1, Some(b"MACHO")),
    ]);
    assert!(parse_ios_app_tree(&mut Cursor::new(invalid_utf8), APP_ID, Some(BUILD)).is_err());

    let mut disorder = valid_raw_records();
    disorder.swap(0, 1);
    assert!(
        parse_ios_app_tree(
            &mut Cursor::new(encode_raw_records(&disorder)),
            APP_ID,
            Some(BUILD)
        )
        .is_err()
    );

    let mut duplicate = valid_raw_records();
    duplicate.insert(1, duplicate[0].clone());
    assert!(
        parse_ios_app_tree(
            &mut Cursor::new(encode_raw_records(&duplicate)),
            APP_ID,
            Some(BUILD)
        )
        .is_err()
    );

    let mut invalid_kind = valid_raw_records();
    invalid_kind[0].0 = 3;
    assert!(
        parse_ios_app_tree(
            &mut Cursor::new(encode_raw_records(&invalid_kind)),
            APP_ID,
            Some(BUILD)
        )
        .is_err()
    );

    let mut invalid_exec = valid_raw_records();
    invalid_exec[2].2 = 1;
    assert!(
        parse_ios_app_tree(
            &mut Cursor::new(encode_raw_records(&invalid_exec)),
            APP_ID,
            Some(BUILD)
        )
        .is_err()
    );

    let mut truncated = encode_raw_records(&valid_raw_records());
    truncated.pop();
    assert!(parse_ios_app_tree(&mut Cursor::new(truncated), APP_ID, Some(BUILD)).is_err());
}

#[test]
fn exact_entry_file_total_path_component_and_depth_caps_accept_max_and_reject_plus_one() {
    assert!(validate_ios_entry_count(u64::from(IOS_APP_TREE_MAX_ENTRIES)).is_ok());
    assert!(validate_ios_entry_count(u64::from(IOS_APP_TREE_MAX_ENTRIES) + 1).is_err());
    assert!(validate_ios_declared_file_len(IOS_APP_TREE_MAX_FILE_BYTES).is_ok());
    assert!(validate_ios_declared_file_len(IOS_APP_TREE_MAX_FILE_BYTES + 1).is_err());
    let max_encoded = (IOS_APP_TREE_MAX_CANONICAL_BYTES / 3) * 4
        + match IOS_APP_TREE_MAX_CANONICAL_BYTES % 3 {
            0 => 0,
            1 => 2,
            _ => 3,
        };
    assert!(validate_artifact_encoded_len(max_encoded, IOS_APP_TREE_ENCODING).is_ok());
    assert!(validate_artifact_encoded_len(max_encoded + 1, IOS_APP_TREE_ENCODING).is_err());
    assert_eq!(
        checked_ios_total_file_bytes(IOS_APP_TREE_MAX_FILE_BYTES, IOS_APP_TREE_MAX_FILE_BYTES)
            .unwrap(),
        IOS_APP_TREE_MAX_TOTAL_FILE_BYTES
    );
    assert!(
        checked_ios_total_file_bytes(IOS_APP_TREE_MAX_FILE_BYTES, IOS_APP_TREE_MAX_FILE_BYTES + 1)
            .is_err()
    );

    let mut count_plus_one = IOS_APP_TREE_MAGIC.to_vec();
    count_plus_one.extend_from_slice(&(IOS_APP_TREE_MAX_ENTRIES + 1).to_be_bytes());
    assert!(parse_ios_app_tree(&mut Cursor::new(count_plus_one), APP_ID, Some(BUILD)).is_err());

    let mut file_plus_one = IOS_APP_TREE_MAGIC.to_vec();
    file_plus_one.extend_from_slice(&1_u32.to_be_bytes());
    file_plus_one.push(2);
    file_plus_one.extend_from_slice(&10_u32.to_be_bytes());
    file_plus_one.extend_from_slice(b"Info.plist");
    file_plus_one.push(0);
    file_plus_one.extend_from_slice(&(IOS_APP_TREE_MAX_FILE_BYTES + 1).to_be_bytes());
    assert!(parse_ios_app_tree(&mut Cursor::new(file_plus_one), APP_ID, Some(BUILD)).is_err());

    let exact_component = "x".repeat(255);
    let mut entries = valid_entries();
    entries.push(IosAppSourceEntry::file(exact_component, b"x", 0o644));
    assert!(encode_ios_app_tree(&entries, APP_ID, Some(BUILD)).is_ok());
    let mut entries = valid_entries();
    entries.push(IosAppSourceEntry::file("x".repeat(256), b"x", 0o644));
    assert!(encode_ios_app_tree(&entries, APP_ID, Some(BUILD)).is_err());

    let exact_path = format!(
        "{}/{}/x",
        std::iter::repeat_n("x".repeat(255), 15)
            .collect::<Vec<_>>()
            .join("/"),
        "y".repeat(254)
    );
    assert_eq!(exact_path.len(), 4_096);
    let mut entries = valid_entries();
    push_file_with_parents(&mut entries, &exact_path);
    assert!(encode_ios_app_tree(&entries, APP_ID, Some(BUILD)).is_ok());

    let depth_64 = std::iter::repeat_n("d", 64).collect::<Vec<_>>().join("/");
    let depth_65 = std::iter::repeat_n("d", 65).collect::<Vec<_>>().join("/");
    let mut entries = valid_entries();
    push_file_with_parents(&mut entries, &depth_64);
    assert!(encode_ios_app_tree(&entries, APP_ID, Some(BUILD)).is_ok());
    let mut entries = valid_entries();
    push_file_with_parents(&mut entries, &depth_65);
    assert!(encode_ios_app_tree(&entries, APP_ID, Some(BUILD)).is_err());

    for hostile in [
        b"".as_slice(),
        b"/absolute",
        b"trailing/",
        b"a//b",
        b".",
        b"..",
        b"a/./b",
        b"a/../b",
        b"nul\0component",
    ] {
        let mut entries = valid_entries();
        entries.push(IosAppSourceEntry::file(hostile, b"x", 0o644));
        assert!(encode_ios_app_tree(&entries, APP_ID, Some(BUILD)).is_err());
    }
}

#[test]
fn bundle_info_fields_and_root_executable_are_enforced() {
    for (app_id, package_type, build, executable) in [
        ("dev.apppilotkit.other", "APPL", BUILD, EXECUTABLE),
        (APP_ID, "BNDL", BUILD, EXECUTABLE),
        (APP_ID, "APPL", "", EXECUTABLE),
        (APP_ID, "APPL", BUILD, "bin/SmokeHost"),
    ] {
        let mut entries = valid_entries();
        entries[0].kind = IosAppSourceKind::File(plist(app_id, package_type, build, executable));
        assert!(encode_ios_app_tree(&entries, APP_ID, None).is_err());
    }
    let mut long_build = valid_entries();
    long_build[0].kind =
        IosAppSourceKind::File(plist(APP_ID, "APPL", &"b".repeat(129), EXECUTABLE));
    assert!(encode_ios_app_tree(&long_build, APP_ID, None).is_err());
    let mut exact_build = valid_entries();
    exact_build[0].kind =
        IosAppSourceKind::File(plist(APP_ID, "APPL", &"b".repeat(128), EXECUTABLE));
    assert!(encode_ios_app_tree(&exact_build, APP_ID, None).is_ok());
    let mut utf8_build = valid_entries();
    utf8_build[0].kind =
        IosAppSourceKind::File(plist(APP_ID, "APPL", &"界".repeat(43), EXECUTABLE));
    assert!(encode_ios_app_tree(&utf8_build, APP_ID, None).is_err());

    let mut info_directory = valid_entries();
    info_directory[0].kind = IosAppSourceKind::Directory;
    assert!(encode_ios_app_tree(&info_directory, APP_ID, Some(BUILD)).is_err());
    let mut non_executable = valid_entries();
    non_executable[1].mode = 0o644;
    assert!(encode_ios_app_tree(&non_executable, APP_ID, Some(BUILD)).is_err());
    let mut missing = valid_entries();
    missing.remove(1);
    assert!(encode_ios_app_tree(&missing, APP_ID, Some(BUILD)).is_err());
}

#[test]
fn canary_scan_operates_on_complete_canonical_stream_bytes() {
    let mut entries = valid_entries();
    entries.push(IosAppSourceEntry::file("canary.bin", FIXED_CANARY, 0o644));
    let bytes = encode_ios_app_tree(&entries, APP_ID, Some(BUILD)).unwrap();
    assert_eq!(literal_match_count(&bytes, FIXED_CANARY), 1);
    assert_eq!(literal_match_count(&bytes, b"not-present"), 0);
}
