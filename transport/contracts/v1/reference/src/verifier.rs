use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonschema::Retrieve;
use minicbor::{Decoder, Encoder};
use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use snow::{Builder, params::NoiseParams};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fs,
    io::{Cursor, Read, Write},
    path::Path,
};

type AnyResult<T> = Result<T, Box<dyn Error>>;

pub const IOS_APP_TREE_ENCODING: &str = "ios-app-tree-v1";
pub const RAW_FILE_ENCODING: &str = "raw-file-v1";
pub const IOS_APP_TREE_MAGIC: &[u8] = b"APPPILOTKIT-IOS-APP-TREE\0\x01";
pub const IOS_APP_TREE_MAX_ENTRIES: u32 = 65_535;
pub const IOS_APP_TREE_MAX_PATH_BYTES: usize = 4_096;
pub const IOS_APP_TREE_MAX_COMPONENT_BYTES: usize = 255;
pub const IOS_APP_TREE_MAX_DEPTH: usize = 64;
pub const IOS_APP_TREE_MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
pub const IOS_APP_TREE_MAX_TOTAL_FILE_BYTES: u64 = 1024 * 1024 * 1024;
pub const IOS_APP_TREE_MAX_CANONICAL_BYTES: u64 = IOS_APP_TREE_MAGIC.len() as u64
    + 4
    + IOS_APP_TREE_MAX_ENTRIES as u64 * (1 + 4 + 4_096 + 1 + 8)
    + IOS_APP_TREE_MAX_TOTAL_FILE_BYTES;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IosAppSourceKind {
    Directory,
    File(Vec<u8>),
    Symlink,
    HardLink,
    Special,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IosAppSourceEntry {
    pub path: Vec<u8>,
    pub kind: IosAppSourceKind,
    pub mode: u32,
    pub has_resource_fork: bool,
    pub ignored_xattrs: BTreeMap<String, Vec<u8>>,
    pub ignored_acl: Option<Vec<u8>>,
    pub ignored_mtime_ns: i128,
}

impl IosAppSourceEntry {
    pub fn directory(path: impl Into<Vec<u8>>) -> Self {
        Self {
            path: path.into(),
            kind: IosAppSourceKind::Directory,
            mode: 0o755,
            has_resource_fork: false,
            ignored_xattrs: BTreeMap::new(),
            ignored_acl: None,
            ignored_mtime_ns: 0,
        }
    }

    pub fn file(path: impl Into<Vec<u8>>, bytes: impl Into<Vec<u8>>, mode: u32) -> Self {
        Self {
            path: path.into(),
            kind: IosAppSourceKind::File(bytes.into()),
            mode,
            has_resource_fork: false,
            ignored_xattrs: BTreeMap::new(),
            ignored_acl: None,
            ignored_mtime_ns: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IosAppTreeRecord {
    pub kind: u8,
    pub path: Vec<u8>,
    pub executable_class: u8,
    pub file_len: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IosAppTreeSummary {
    pub records: Vec<IosAppTreeRecord>,
    pub total_file_bytes: u64,
}

pub fn encode_ios_app_tree(
    entries: &[IosAppSourceEntry],
    app_id: &str,
    expected_build: Option<&str>,
) -> AnyResult<Vec<u8>> {
    let mut output = Vec::new();
    write_ios_app_tree(entries, app_id, expected_build, &mut output)?;
    Ok(output)
}

pub fn write_ios_app_tree<W: Write>(
    entries: &[IosAppSourceEntry],
    app_id: &str,
    expected_build: Option<&str>,
    output: &mut W,
) -> AnyResult<()> {
    validate_ios_entry_count(entries.len() as u64)?;
    let mut sorted = entries.to_vec();
    sorted.sort_by(|left, right| left.path.cmp(&right.path));
    let mut prior: Option<&[u8]> = None;
    let mut total_file_bytes = 0_u64;
    for entry in &sorted {
        validate_ios_path(&entry.path)?;
        if prior == Some(entry.path.as_slice()) {
            return Err("ios-app-tree-v1 contains a duplicate path".into());
        }
        prior = Some(&entry.path);
        if entry.has_resource_fork || entry.ignored_xattrs.contains_key("com.apple.ResourceFork") {
            return Err("ios-app-tree-v1 rejects ResourceFork data".into());
        }
        match &entry.kind {
            IosAppSourceKind::Directory => {}
            IosAppSourceKind::File(bytes) => {
                validate_ios_declared_file_len(bytes.len() as u64)?;
                total_file_bytes =
                    checked_ios_total_file_bytes(total_file_bytes, bytes.len() as u64)?;
            }
            IosAppSourceKind::Symlink => {
                return Err("ios-app-tree-v1 rejects symbolic links".into());
            }
            IosAppSourceKind::HardLink => {
                return Err("ios-app-tree-v1 rejects hard links".into());
            }
            IosAppSourceKind::Special => {
                return Err("ios-app-tree-v1 rejects special files".into());
            }
        }
    }
    validate_ios_source_topology(&sorted)?;
    validate_ios_bundle_entries(&sorted, app_id, expected_build)?;

    output.write_all(IOS_APP_TREE_MAGIC)?;
    output.write_all(&(sorted.len() as u32).to_be_bytes())?;
    for entry in sorted {
        let (kind, file_bytes) = match entry.kind {
            IosAppSourceKind::Directory => (1_u8, None),
            IosAppSourceKind::File(bytes) => (2_u8, Some(bytes)),
            _ => unreachable!("source kinds were validated"),
        };
        output.write_all(&[kind])?;
        output.write_all(&(entry.path.len() as u32).to_be_bytes())?;
        output.write_all(&entry.path)?;
        let executable_class = u8::from(file_bytes.is_some() && entry.mode & 0o111 != 0);
        output.write_all(&[executable_class])?;
        if let Some(bytes) = file_bytes {
            output.write_all(&(bytes.len() as u64).to_be_bytes())?;
            output.write_all(&bytes)?;
        }
    }
    Ok(())
}

pub fn parse_ios_app_tree<R: Read>(
    reader: &mut R,
    app_id: &str,
    expected_build: Option<&str>,
) -> AnyResult<IosAppTreeSummary> {
    let mut magic = vec![0_u8; IOS_APP_TREE_MAGIC.len()];
    reader.read_exact(&mut magic)?;
    if magic != IOS_APP_TREE_MAGIC {
        return Err("ios-app-tree-v1 magic/version mismatch".into());
    }
    let entry_count = read_u32be(reader)?;
    validate_ios_entry_count(u64::from(entry_count))?;
    let mut records = Vec::with_capacity(entry_count as usize);
    let mut prior: Option<Vec<u8>> = None;
    let mut total_file_bytes = 0_u64;
    let mut info_plist: Option<Vec<u8>> = None;
    for _ in 0..entry_count {
        let kind = read_u8(reader)?;
        if !matches!(kind, 1 | 2) {
            return Err("ios-app-tree-v1 record kind is not directory(1) or file(2)".into());
        }
        let path_len = read_u32be(reader)? as usize;
        if path_len == 0 || path_len > IOS_APP_TREE_MAX_PATH_BYTES {
            return Err("ios-app-tree-v1 path length is outside 1..=4096".into());
        }
        let mut path = vec![0_u8; path_len];
        reader.read_exact(&mut path)?;
        validate_ios_path(&path)?;
        if prior.as_ref().is_some_and(|value| value >= &path) {
            return Err("ios-app-tree-v1 records are duplicate or not UTF-8-byte sorted".into());
        }
        prior = Some(path.clone());
        let executable_class = read_u8(reader)?;
        if executable_class > 1 || (kind == 1 && executable_class != 0) {
            return Err("ios-app-tree-v1 executable class is invalid".into());
        }
        let file_len = if kind == 2 {
            let len = read_u64be(reader)?;
            validate_ios_declared_file_len(len)?;
            total_file_bytes = checked_ios_total_file_bytes(total_file_bytes, len)?;
            let mut remaining = len;
            let mut info = (path == b"Info.plist").then(Vec::new);
            let mut buffer = [0_u8; 64 * 1024];
            while remaining > 0 {
                let amount = usize::try_from(remaining.min(buffer.len() as u64))?;
                reader.read_exact(&mut buffer[..amount])?;
                if let Some(bytes) = &mut info {
                    bytes.extend_from_slice(&buffer[..amount]);
                }
                remaining -= amount as u64;
            }
            if info.is_some() {
                info_plist = info;
            }
            Some(len)
        } else {
            None
        };
        records.push(IosAppTreeRecord {
            kind,
            path,
            executable_class,
            file_len,
        });
    }
    let mut trailing = [0_u8; 1];
    if reader.read(&mut trailing)? != 0 {
        return Err("ios-app-tree-v1 has trailing bytes".into());
    }
    validate_ios_record_topology(&records)?;
    validate_ios_bundle_records(&records, info_plist.as_deref(), app_id, expected_build)?;
    Ok(IosAppTreeSummary {
        records,
        total_file_bytes,
    })
}

pub fn validate_ios_entry_count(count: u64) -> AnyResult<()> {
    if count > u64::from(IOS_APP_TREE_MAX_ENTRIES) {
        return Err("ios-app-tree-v1 entry count exceeds 65535".into());
    }
    Ok(())
}

pub fn validate_ios_declared_file_len(len: u64) -> AnyResult<()> {
    if len > IOS_APP_TREE_MAX_FILE_BYTES {
        return Err("ios-app-tree-v1 file length exceeds 512 MiB".into());
    }
    Ok(())
}

pub fn checked_ios_total_file_bytes(current: u64, next: u64) -> AnyResult<u64> {
    validate_ios_declared_file_len(next)?;
    let total = current
        .checked_add(next)
        .ok_or("ios-app-tree-v1 total file length overflow")?;
    if total > IOS_APP_TREE_MAX_TOTAL_FILE_BYTES {
        return Err("ios-app-tree-v1 total file bytes exceed 1 GiB".into());
    }
    Ok(total)
}

fn validate_ios_path(path: &[u8]) -> AnyResult<()> {
    if path.is_empty() || path.len() > IOS_APP_TREE_MAX_PATH_BYTES || path.contains(&0) {
        return Err("ios-app-tree-v1 path byte length/NUL constraint failed".into());
    }
    std::str::from_utf8(path).map_err(|_| "ios-app-tree-v1 path is not UTF-8")?;
    let components = path.split(|byte| *byte == b'/').collect::<Vec<_>>();
    if components.len() > IOS_APP_TREE_MAX_DEPTH
        || components.iter().any(|component| {
            component.is_empty()
                || component.len() > IOS_APP_TREE_MAX_COMPONENT_BYTES
                || *component == b"."
                || *component == b".."
        })
    {
        return Err("ios-app-tree-v1 path component/depth constraint failed".into());
    }
    Ok(())
}

fn validate_ios_source_topology(entries: &[IosAppSourceEntry]) -> AnyResult<()> {
    let kinds = entries
        .iter()
        .map(|entry| (entry.path.as_slice(), &entry.kind))
        .collect::<BTreeMap<_, _>>();
    for entry in entries {
        if let Some(separator) = entry.path.iter().rposition(|byte| *byte == b'/') {
            let parent = &entry.path[..separator];
            if !matches!(kinds.get(parent), Some(IosAppSourceKind::Directory)) {
                return Err("ios-app-tree-v1 entry parent is missing or is not a directory".into());
            }
        }
    }
    Ok(())
}

fn validate_ios_record_topology(records: &[IosAppTreeRecord]) -> AnyResult<()> {
    let kinds = records
        .iter()
        .map(|record| (record.path.as_slice(), record.kind))
        .collect::<BTreeMap<_, _>>();
    for record in records {
        if let Some(separator) = record.path.iter().rposition(|byte| *byte == b'/') {
            let parent = &record.path[..separator];
            if kinds.get(parent) != Some(&1) {
                return Err(
                    "ios-app-tree-v1 record parent is missing or is not a directory".into(),
                );
            }
        }
    }
    Ok(())
}

fn validate_ios_bundle_entries(
    entries: &[IosAppSourceEntry],
    app_id: &str,
    expected_build: Option<&str>,
) -> AnyResult<()> {
    let info = entries
        .iter()
        .find(|entry| entry.path == b"Info.plist")
        .ok_or("iOS bundle lacks root Info.plist")?;
    let IosAppSourceKind::File(info_bytes) = &info.kind else {
        return Err("iOS bundle root Info.plist is not a regular file".into());
    };
    let (executable, build) = validate_info_plist(info_bytes, app_id, expected_build)?;
    let executable_entry = entries
        .iter()
        .find(|entry| entry.path == executable.as_bytes())
        .ok_or("iOS bundle root executable is absent")?;
    if !matches!(executable_entry.kind, IosAppSourceKind::File(_))
        || executable_entry.mode & 0o111 == 0
    {
        return Err("iOS bundle root executable is not a regular executable-class file".into());
    }
    if expected_build.is_some_and(|expected| expected != build) {
        return Err("iOS bundle CFBundleVersion differs from evidence build".into());
    }
    Ok(())
}

fn validate_ios_bundle_records(
    records: &[IosAppTreeRecord],
    info_plist: Option<&[u8]>,
    app_id: &str,
    expected_build: Option<&str>,
) -> AnyResult<()> {
    let info = records
        .iter()
        .find(|record| record.path == b"Info.plist")
        .ok_or("iOS bundle lacks root Info.plist")?;
    if info.kind != 2 {
        return Err("iOS bundle root Info.plist is not a regular file".into());
    }
    let (executable, build) = validate_info_plist(
        info_plist.ok_or("iOS bundle root Info.plist bytes are absent")?,
        app_id,
        expected_build,
    )?;
    let executable_entry = records
        .iter()
        .find(|record| record.path == executable.as_bytes())
        .ok_or("iOS bundle root executable is absent")?;
    if executable_entry.kind != 2 || executable_entry.executable_class != 1 {
        return Err("iOS bundle root executable is not a regular executable-class file".into());
    }
    if expected_build.is_some_and(|expected| expected != build) {
        return Err("iOS bundle CFBundleVersion differs from evidence build".into());
    }
    Ok(())
}

fn validate_info_plist(
    bytes: &[u8],
    app_id: &str,
    expected_build: Option<&str>,
) -> AnyResult<(String, String)> {
    let value = plist::Value::from_reader(Cursor::new(bytes))?;
    let dictionary = value
        .as_dictionary()
        .ok_or("iOS Info.plist root is not a dictionary")?;
    let string = |key: &str| -> AnyResult<&str> {
        dictionary
            .get(key)
            .and_then(plist::Value::as_string)
            .ok_or_else(|| format!("iOS Info.plist {key} is absent or not a string").into())
    };
    if string("CFBundleIdentifier")? != app_id {
        return Err("iOS Info.plist CFBundleIdentifier differs from app_id".into());
    }
    if string("CFBundlePackageType")? != "APPL" {
        return Err("iOS Info.plist CFBundlePackageType is not APPL".into());
    }
    let build = string("CFBundleVersion")?;
    if build.is_empty() || build.len() > 128 {
        return Err("iOS Info.plist CFBundleVersion must be 1..=128 UTF-8 bytes".into());
    }
    if expected_build.is_some_and(|expected| expected != build) {
        return Err("iOS Info.plist CFBundleVersion differs from evidence build".into());
    }
    let executable = string("CFBundleExecutable")?;
    validate_ios_path(executable.as_bytes())?;
    if executable.as_bytes().contains(&b'/') {
        return Err("iOS Info.plist CFBundleExecutable is not one safe component".into());
    }
    Ok((executable.to_owned(), build.to_owned()))
}

fn read_u8<R: Read>(reader: &mut R) -> AnyResult<u8> {
    let mut bytes = [0_u8; 1];
    reader.read_exact(&mut bytes)?;
    Ok(bytes[0])
}

fn read_u32be<R: Read>(reader: &mut R) -> AnyResult<u32> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}

fn read_u64be<R: Read>(reader: &mut R) -> AnyResult<u64> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_be_bytes(bytes))
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

struct VerifiedSessionCiphertexts {
    request_outer: Vec<u8>,
    response_outer: Vec<u8>,
}

pub struct FixtureOutcome {
    pub id: String,
    pub result: &'static str,
    pub close_reason: &'static str,
}

pub fn parse_strict_json(input: &[u8]) -> Result<Value, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_slice(input);
    let value = StrictValueSeed.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

struct StrictValueSeed;

impl<'de> DeserializeSeed<'de> for StrictValueSeed {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictValueVisitor)
    }
}

struct StrictValueVisitor;

impl<'de> Visitor<'de> for StrictValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value without duplicate object members")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("JSON numbers must be finite"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(Value::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        StrictValueSeed.deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or_default());
        while let Some(value) = sequence.next_element_seed(StrictValueSeed)? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        while let Some(key) = object.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(A::Error::custom(format!(
                    "duplicate JSON object member: {key}"
                )));
            }
            values.insert(key, object.next_value_seed(StrictValueSeed)?);
        }
        Ok(Value::Object(values))
    }
}

pub fn verify_positive_crypto(contract_root: &Path) -> AnyResult<u64> {
    verify_positive_bootstrap(&serde_json::from_slice(&fs::read(
        contract_root.join("vectors/bootstrap-nk-success.json"),
    )?)?)?;
    verify_positive_session(&serde_json::from_slice(&fs::read(
        contract_root.join("vectors/session-nnpsk0-success.json"),
    )?)?)?;
    Ok(2)
}

pub fn verify_positive_android_descriptor(contract_root: &Path) -> AnyResult<u64> {
    let android = parse_strict_json(&fs::read(
        contract_root.join("vectors/bootstrap-android-descriptor.json"),
    )?)?;
    let bootstrap = parse_strict_json(&fs::read(
        contract_root.join("vectors/bootstrap-nk-success.json"),
    )?)?;
    verify_android_descriptor_values(&android, &bootstrap)?;
    Ok(1)
}

pub fn verify_positive_ios_app_artifact(contract_root: &Path) -> AnyResult<u64> {
    let vector = parse_strict_json(&fs::read(
        contract_root.join("vectors/ios-app-artifact-tree.json"),
    )?)?;
    exact_json_keys(
        &vector,
        &[
            "bundle",
            "encoding",
            "entries",
            "expected",
            "format",
            "oracle",
            "schema_version",
            "suite",
            "test_only_material",
        ],
        "iOS app artifact vector",
    )?;
    exact_json_keys(
        &vector["bundle"],
        &["app_id", "build", "executable", "package_type"],
        "iOS app artifact bundle",
    )?;
    exact_json_keys(
        &vector["format"],
        &[
            "entry_count",
            "magic_hex",
            "normalization",
            "ordering",
            "record",
            "root",
        ],
        "iOS app artifact format",
    )?;
    exact_json_keys(
        &vector["expected"],
        &[
            "artifact_sha256",
            "canonical_byte_count",
            "canonical_hex",
            "entry_count",
            "total_file_bytes",
        ],
        "iOS app artifact expected values",
    )?;
    exact_json_keys(
        &vector["test_only_material"],
        &["classification", "production_use"],
        "iOS app artifact TEST-ONLY metadata",
    )?;
    if vector["schema_version"] != "1.0"
        || vector["suite"] != "ios-app-artifact-tree"
        || vector["encoding"] != IOS_APP_TREE_ENCODING
        || vector["oracle"] != "independent-stream-parser-and-encoder"
        || vector["test_only_material"]["classification"] != "TEST-ONLY"
        || vector["test_only_material"]["production_use"] != "forbidden"
        || vector["format"]["magic_hex"] != hex::encode(IOS_APP_TREE_MAGIC)
        || vector["format"]["entry_count"] != "u32be"
        || vector["format"]["record"]
            != "kind:u8 || path_len:u32be || path:utf8 || executable_class:u8 || [file_len:u64be || exact_file_bytes]"
        || vector["format"]["root"] != "implicit"
        || vector["format"]["ordering"] != "strict ascending raw UTF-8 bytes"
        || vector["format"]["normalization"] != "none; NFC and case are preserved"
    {
        return Err("iOS app artifact vector envelope/format mismatch".into());
    }
    let app_id = vector["bundle"]["app_id"]
        .as_str()
        .ok_or("iOS app artifact app_id missing")?;
    let build = vector["bundle"]["build"]
        .as_str()
        .ok_or("iOS app artifact build missing")?;
    if vector["bundle"]["package_type"] != "APPL" || vector["bundle"]["executable"] != "SmokeHost" {
        return Err("iOS app artifact bundle oracle mismatch".into());
    }
    let raw_entries = vector["entries"]
        .as_array()
        .ok_or("iOS app artifact entries missing")?;
    let mut source_entries = Vec::with_capacity(raw_entries.len());
    let mut prior: Option<&[u8]> = None;
    for entry in raw_entries {
        let path = entry["path_utf8"]
            .as_str()
            .ok_or("iOS app artifact vector path missing")?;
        if prior.is_some_and(|value| value >= path.as_bytes()) {
            return Err("iOS app artifact vector source entries are not UTF-8-byte sorted".into());
        }
        prior = Some(path.as_bytes());
        let executable_class = entry["executable_class"]
            .as_u64()
            .ok_or("iOS app artifact vector executable class missing")?;
        let source = match entry["kind"].as_str() {
            Some("directory") => {
                exact_json_keys(
                    entry,
                    &["executable_class", "kind", "path_utf8"],
                    "iOS app artifact directory entry",
                )?;
                if executable_class != 0 {
                    return Err("iOS app artifact directory executable class is not zero".into());
                }
                IosAppSourceEntry::directory(path.as_bytes().to_vec())
            }
            Some("file") => {
                exact_json_keys(
                    entry,
                    &["executable_class", "file_hex", "kind", "path_utf8"],
                    "iOS app artifact file entry",
                )?;
                let bytes = hex::decode(
                    entry["file_hex"]
                        .as_str()
                        .ok_or("iOS app artifact file_hex missing")?,
                )?;
                let mode = if executable_class == 1 { 0o100 } else { 0 };
                if executable_class > 1 {
                    return Err("iOS app artifact file executable class is invalid".into());
                }
                IosAppSourceEntry::file(path.as_bytes().to_vec(), bytes, mode)
            }
            _ => return Err("iOS app artifact vector entry kind is invalid".into()),
        };
        source_entries.push(source);
    }
    let canonical = hex::decode(
        vector["expected"]["canonical_hex"]
            .as_str()
            .ok_or("iOS app artifact canonical hex missing")?,
    )?;
    if vector["expected"]["canonical_byte_count"].as_u64() != Some(canonical.len() as u64)
        || vector["expected"]["artifact_sha256"] != format!("sha256:{}", sha256_hex(&canonical))
    {
        return Err("iOS app artifact golden bytes/hash mismatch".into());
    }
    let independently_encoded = encode_ios_app_tree(&source_entries, app_id, Some(build))?;
    if independently_encoded != canonical {
        return Err("iOS app artifact independent encoder differs from golden bytes".into());
    }
    let summary = parse_ios_app_tree(&mut Cursor::new(&canonical), app_id, Some(build))?;
    if vector["expected"]["entry_count"].as_u64() != Some(summary.records.len() as u64)
        || vector["expected"]["total_file_bytes"].as_u64() != Some(summary.total_file_bytes)
    {
        return Err("iOS app artifact parsed counts differ from golden".into());
    }
    for kind in [
        IosAppSourceKind::Symlink,
        IosAppSourceKind::HardLink,
        IosAppSourceKind::Special,
    ] {
        let mut hostile = source_entries.clone();
        hostile.push(IosAppSourceEntry {
            path: b"hostile-source-entry".to_vec(),
            kind,
            mode: 0,
            has_resource_fork: false,
            ignored_xattrs: BTreeMap::new(),
            ignored_acl: None,
            ignored_mtime_ns: 0,
        });
        if encode_ios_app_tree(&hostile, app_id, Some(build)).is_ok() {
            return Err("iOS app artifact encoder accepted a prohibited source type".into());
        }
    }
    Ok(1)
}

fn verify_android_descriptor_values(android: &Value, bootstrap: &Value) -> AnyResult<()> {
    exact_json_keys(
        android,
        &[
            "canonical_input",
            "expected",
            "oracle",
            "schema_version",
            "shared_bootstrap_vector",
            "suite",
            "test_only_material",
        ],
        "Android descriptor vector",
    )?;
    exact_json_keys(
        &android["test_only_material"],
        &["classification", "material", "production_use"],
        "Android descriptor TEST-ONLY material",
    )?;
    exact_json_keys(
        &android["test_only_material"]["material"],
        &[],
        "Android descriptor duplicated material",
    )?;
    exact_json_keys(
        &android["canonical_input"],
        &[
            "launch_descriptor_cbor_hex",
            "launch_endpoint",
            "launch_platform",
        ],
        "Android descriptor canonical input",
    )?;
    exact_json_keys(
        &android["canonical_input"]["launch_endpoint"],
        &["localabstract_name"],
        "Android localabstract endpoint",
    )?;
    exact_json_keys(
        &android["expected"],
        &["close_reason", "result"],
        "Android descriptor expected result",
    )?;
    if android["schema_version"] != "1.0"
        || android["suite"] != "bootstrap-android-descriptor"
        || android["oracle"] != "independent-deterministic-cbor"
        || android["shared_bootstrap_vector"] != "vectors/bootstrap-nk-success.json"
        || android["test_only_material"]["classification"] != "TEST-ONLY"
        || android["test_only_material"]["production_use"] != "forbidden"
        || android["expected"]["result"] != "accepted"
        || !android["expected"]["close_reason"].is_null()
    {
        return Err("Android descriptor vector envelope mismatch".into());
    }
    let mut bound_canonical = bootstrap["canonical_input"].clone();
    bound_canonical["launch_platform"] = android["canonical_input"]["launch_platform"].clone();
    bound_canonical["launch_endpoint"] = android["canonical_input"]["launch_endpoint"].clone();
    bound_canonical["launch_descriptor_cbor_hex"] =
        android["canonical_input"]["launch_descriptor_cbor_hex"].clone();
    verify_bootstrap_material_binding(
        &bound_canonical,
        &bootstrap["test_only_material"]["material"],
    )
}

fn verify_positive_bootstrap(vector: &Value) -> AnyResult<()> {
    let canonical = &vector["canonical_input"];
    let material = &vector["test_only_material"]["material"];
    verify_bootstrap_material_binding(canonical, material)?;
    let prologue = hex::decode(
        canonical["prologue_cbor_hex"]
            .as_str()
            .ok_or("NK prologue missing")?,
    )?;
    let static_public = hex::decode(
        material["broker_static_public_hex"]
            .as_str()
            .ok_or("NK static public missing")?,
    )?;
    let static_private = hex::decode(
        material["broker_static_private_hex"]
            .as_str()
            .ok_or("NK static private missing")?,
    )?;
    let target_key = hex::decode(
        material["target_ephemeral_private_hex"]
            .as_str()
            .ok_or("NK target key missing")?,
    )?;
    let broker_key = hex::decode(
        material["broker_ephemeral_private_hex"]
            .as_str()
            .ok_or("NK broker key missing")?,
    )?;
    let m1_payload = hex::decode(
        canonical["m1_payload_cbor_hex"]
            .as_str()
            .ok_or("NK M1 payload missing")?,
    )?;
    let m2_payload = hex::decode(
        canonical["m2_payload_cbor_hex"]
            .as_str()
            .ok_or("NK M2 payload missing")?,
    )?;
    let ack_payload = hex::decode(
        canonical["ack_payload_cbor_hex"]
            .as_str()
            .ok_or("NK ack payload missing")?,
    )?;
    let params: NoiseParams = canonical["noise_name"]
        .as_str()
        .ok_or("NK name missing")?
        .parse()?;
    let mut target = Builder::new(params.clone())
        .prologue(&prologue)?
        .remote_public_key(&static_public)?
        .fixed_ephemeral_key_for_testing_only(&target_key)
        .build_initiator()?;
    let mut broker = Builder::new(params)
        .prologue(&prologue)?
        .local_private_key(&static_private)?
        .fixed_ephemeral_key_for_testing_only(&broker_key)
        .build_responder()?;
    let expected_m1 = unframe_u16_hex(
        vector["expected"]["m1_outer_hex"]
            .as_str()
            .ok_or("NK M1 outer missing")?,
    )?;
    let expected_m2 = unframe_u16_hex(
        vector["expected"]["m2_outer_hex"]
            .as_str()
            .ok_or("NK M2 outer missing")?,
    )?;
    let mut generated = vec![0_u8; 65_535];
    let mut plaintext = vec![0_u8; 65_535];
    let len = target.write_message(&m1_payload, &mut generated)?;
    if generated[..len] != expected_m1 {
        return Err("positive NK M1 bytes mismatch".into());
    }
    let len = broker.read_message(&expected_m1, &mut plaintext)?;
    if plaintext[..len] != m1_payload {
        return Err("positive NK M1 plaintext mismatch".into());
    }
    let len = broker.write_message(&m2_payload, &mut generated)?;
    if generated[..len] != expected_m2 {
        return Err("positive NK M2 bytes mismatch".into());
    }
    let len = target.read_message(&expected_m2, &mut plaintext)?;
    if plaintext[..len] != m2_payload {
        return Err("positive NK M2 plaintext mismatch".into());
    }
    let hash = target.get_handshake_hash();
    if hex::encode(hash) != vector["expected"]["noise_handshake_hash_hex"]
        || format!("sha256:{}", sha256_hex(hash))
            != vector["expected"]["noise_handshake_hash_sha256"]
    {
        return Err("positive NK handshake hash mismatch".into());
    }
    let mut target_transport = target.into_transport_mode()?;
    let mut broker_transport = broker.into_transport_mode()?;
    let ack = unframe_u16_hex(
        vector["expected"]["ack_outer_hex"]
            .as_str()
            .ok_or("NK ack outer missing")?,
    )?;
    let len = broker_transport.read_message(&ack, &mut plaintext)?;
    let expected_plain = expected_record_plaintext(2, &ack_payload);
    if plaintext[..len] != expected_plain {
        return Err("positive NK ack plaintext mismatch".into());
    }
    let mut generated_ack = vec![0_u8; 65_535];
    let generated_len = target_transport.write_message(&expected_plain, &mut generated_ack)?;
    if generated_ack[..generated_len] != ack {
        return Err("positive NK ack ciphertext mismatch".into());
    }
    verify_transcript(vector, &["m1_outer_hex", "m2_outer_hex", "ack_outer_hex"])
}

fn verify_positive_session(vector: &Value) -> AnyResult<()> {
    verify_positive_session_ciphertexts(vector).map(|_| ())
}

fn verify_positive_session_ciphertexts(vector: &Value) -> AnyResult<VerifiedSessionCiphertexts> {
    let canonical = &vector["canonical_input"];
    let material = &vector["test_only_material"]["material"];
    let prologue = hex::decode(
        canonical["prologue_cbor_hex"]
            .as_str()
            .ok_or("session prologue missing")?,
    )?;
    let psk: [u8; 32] = hex::decode(
        material["process_bootstrap_secret_hex"]
            .as_str()
            .ok_or("session PSK missing")?,
    )?
    .try_into()
    .map_err(|_| "session PSK is not 32 bytes")?;
    let target_key = hex::decode(
        material["target_ephemeral_private_hex"]
            .as_str()
            .ok_or("session target key missing")?,
    )?;
    let broker_key = hex::decode(
        material["broker_ephemeral_private_hex"]
            .as_str()
            .ok_or("session broker key missing")?,
    )?;
    let params: NoiseParams = canonical["noise_name"]
        .as_str()
        .ok_or("session Noise name missing")?
        .parse()?;
    let mut target = Builder::new(params.clone())
        .prologue(&prologue)?
        .psk(0, &psk)?
        .fixed_ephemeral_key_for_testing_only(&target_key)
        .build_initiator()?;
    let mut broker = Builder::new(params)
        .prologue(&prologue)?
        .psk(0, &psk)?
        .fixed_ephemeral_key_for_testing_only(&broker_key)
        .build_responder()?;
    let mut generated = vec![0_u8; 65_535];
    let mut plaintext = vec![0_u8; 65_535];
    let m1 = unframe_u16_hex(
        vector["expected"]["m1_outer_hex"]
            .as_str()
            .ok_or("session M1 missing")?,
    )?;
    let m2 = unframe_u16_hex(
        vector["expected"]["m2_outer_hex"]
            .as_str()
            .ok_or("session M2 missing")?,
    )?;
    let len = target.write_message(&[], &mut generated)?;
    if generated[..len] != m1 || broker.read_message(&m1, &mut plaintext)? != 0 {
        return Err("positive session M1 mismatch".into());
    }
    let len = broker.write_message(&[], &mut generated)?;
    if generated[..len] != m2 || target.read_message(&m2, &mut plaintext)? != 0 {
        return Err("positive session M2 mismatch".into());
    }
    let hash = target.get_handshake_hash();
    if hex::encode(hash) != vector["expected"]["noise_handshake_hash_hex"] {
        return Err("positive session handshake hash mismatch".into());
    }
    let mut target_transport = target.into_transport_mode()?;
    let mut broker_transport = broker.into_transport_mode()?;
    let mut verified_request_outer = None;
    let mut verified_response_outer = None;
    for (field, receiver_target, payload_field, kind) in [
        (
            "target_finished_outer_hex",
            false,
            "target_finished_cbor_hex",
            2_u8,
        ),
        (
            "broker_finished_outer_hex",
            true,
            "broker_finished_cbor_hex",
            2_u8,
        ),
        (
            "session_open_outer_hex",
            true,
            "session_open_utf8_hex",
            1_u8,
        ),
        (
            "session_open_response_outer_hex",
            false,
            "session_open_response_utf8_hex",
            1_u8,
        ),
    ] {
        let ciphertext = unframe_u16_hex(
            vector["expected"][field]
                .as_str()
                .ok_or("session outer field missing")?,
        )?;
        let expected_payload = hex::decode(
            canonical[payload_field]
                .as_str()
                .ok_or("session payload field missing")?,
        )?;
        let len = if receiver_target {
            target_transport.read_message(&ciphertext, &mut plaintext)?
        } else {
            broker_transport.read_message(&ciphertext, &mut plaintext)?
        };
        if plaintext[..len] != expected_record_plaintext(kind, &expected_payload) {
            return Err(format!("positive session plaintext mismatch for {field}").into());
        }
        if field == "session_open_outer_hex" {
            verified_request_outer = Some(hex::decode(
                vector["expected"][field]
                    .as_str()
                    .ok_or("session request outer missing")?,
            )?);
        } else if field == "session_open_response_outer_hex" {
            verified_response_outer = Some(hex::decode(
                vector["expected"][field]
                    .as_str()
                    .ok_or("session response outer missing")?,
            )?);
        }
    }
    verify_transcript(
        vector,
        &[
            "m1_outer_hex",
            "m2_outer_hex",
            "target_finished_outer_hex",
            "broker_finished_outer_hex",
            "session_open_outer_hex",
            "session_open_response_outer_hex",
        ],
    )?;
    verify_session_response_binding(vector)?;
    Ok(VerifiedSessionCiphertexts {
        request_outer: verified_request_outer.ok_or("verified session request missing")?,
        response_outer: verified_response_outer.ok_or("verified session response missing")?,
    })
}

fn expected_record_plaintext(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut value = Vec::with_capacity(12 + payload.len());
    value.push(kind);
    value.push(3);
    value.extend_from_slice(&0_u16.to_be_bytes());
    value.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    value.extend_from_slice(&0_u32.to_be_bytes());
    value.extend_from_slice(payload);
    value
}

fn verify_transcript(vector: &Value, fields: &[&str]) -> AnyResult<()> {
    let mut transcript = Vec::new();
    for field in fields {
        transcript.extend(hex::decode(
            vector["expected"][field]
                .as_str()
                .ok_or("transcript field missing")?,
        )?);
    }
    if hex::encode(&transcript) != vector["expected"]["transcript_hex"]
        || format!("sha256:{}", sha256_hex(&transcript)) != vector["expected"]["transcript_sha256"]
    {
        return Err("positive transcript literal/hash mismatch".into());
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
enum LaunchEndpoint {
    Ios { host: String, port: u64 },
    Android { name: String },
}

#[derive(Debug, Eq, PartialEq)]
struct LaunchDescriptor {
    lease_id: Vec<u8>,
    target_nonce: Vec<u8>,
    app_digest: Vec<u8>,
    broker_static_public: Vec<u8>,
    endpoint: LaunchEndpoint,
    expiry_ms: u64,
    target_reference_digest: Vec<u8>,
}

#[derive(Debug, Eq, PartialEq)]
struct BootstrapFacts {
    target_reference_digest: Vec<u8>,
    lease_id: Vec<u8>,
    target_nonce: Vec<u8>,
    app_digest: Option<Vec<u8>>,
    expiry_ms: Option<u64>,
    pbs: Option<Vec<u8>>,
}

fn cbor_key(decoder: &mut Decoder<'_>, expected: u8) -> AnyResult<()> {
    if decoder.u8()? != expected {
        return Err(format!("unexpected CBOR key, wanted {expected}").into());
    }
    Ok(())
}

fn exact_bytes(decoder: &mut Decoder<'_>, length: usize, field: &str) -> AnyResult<Vec<u8>> {
    let bytes = decoder.bytes()?;
    if bytes.len() != length {
        return Err(format!("{field} must be {length} bytes").into());
    }
    Ok(bytes.to_vec())
}

fn strict_cbor(bytes: &[u8], field: &str) -> AnyResult<()> {
    validate_deterministic_cbor(bytes).map_err(|error| format!("{field}: {error}"))?;
    Ok(())
}

fn decode_launch_descriptor(bytes: &[u8]) -> AnyResult<LaunchDescriptor> {
    strict_cbor(bytes, "launch descriptor")?;
    let mut d = Decoder::new(bytes);
    if d.map()? != Some(9) {
        return Err("launch descriptor must be a nine-entry map".into());
    }
    cbor_key(&mut d, 0)?;
    if d.u8()? != 1 {
        return Err("launch descriptor version mismatch".into());
    }
    cbor_key(&mut d, 1)?;
    let platform = d.u8()?;
    cbor_key(&mut d, 2)?;
    let lease_id = exact_bytes(&mut d, 16, "launch lease id")?;
    cbor_key(&mut d, 3)?;
    let target_nonce = exact_bytes(&mut d, 32, "launch Target nonce")?;
    cbor_key(&mut d, 4)?;
    let app_digest = exact_bytes(&mut d, 32, "launch App digest")?;
    cbor_key(&mut d, 5)?;
    let broker_static_public = exact_bytes(&mut d, 32, "launch Broker static public key")?;
    cbor_key(&mut d, 6)?;
    let endpoint = match platform {
        0 => {
            if d.map()? != Some(2) {
                return Err("iOS launch endpoint must be a two-entry map".into());
            }
            cbor_key(&mut d, 0)?;
            let host = d.str()?.to_owned();
            cbor_key(&mut d, 1)?;
            let port = d.u64()?;
            if host != "127.0.0.1" || !(49_152..=65_535).contains(&port) {
                return Err(
                    "iOS launch endpoint is not exact loopback/dynamic-port material".into(),
                );
            }
            LaunchEndpoint::Ios { host, port }
        }
        1 => {
            if d.map()? != Some(1) {
                return Err("Android launch endpoint must be a one-entry map".into());
            }
            cbor_key(&mut d, 0)?;
            let name = d.str()?.to_owned();
            if !(32..=96).contains(&name.len()) {
                return Err("Android launch endpoint name length is out of range".into());
            }
            LaunchEndpoint::Android { name }
        }
        _ => return Err("launch platform is outside the closed inventory".into()),
    };
    cbor_key(&mut d, 7)?;
    let expiry_ms = d.u64()?;
    cbor_key(&mut d, 8)?;
    let target_reference_digest = exact_bytes(&mut d, 32, "launch Target Reference digest")?;
    if d.position() != bytes.len() {
        return Err("trailing launch descriptor bytes".into());
    }
    Ok(LaunchDescriptor {
        lease_id,
        target_nonce,
        app_digest,
        broker_static_public,
        endpoint,
        expiry_ms,
        target_reference_digest,
    })
}

fn decode_bootstrap_prologue(bytes: &[u8]) -> AnyResult<BootstrapFacts> {
    strict_cbor(bytes, "bootstrap prologue")?;
    let mut d = Decoder::new(bytes);
    if d.array()? != Some(10)
        || d.str()? != "apppilotkit.transport"
        || d.u8()? != 1
        || d.str()? != "bootstrap"
        || d.u8()? != 0
        || d.u8()? != 1
    {
        return Err("bootstrap prologue prefix/roles mismatch".into());
    }
    let facts = BootstrapFacts {
        target_reference_digest: exact_bytes(&mut d, 32, "prologue Target Reference digest")?,
        lease_id: exact_bytes(&mut d, 16, "prologue lease id")?,
        target_nonce: exact_bytes(&mut d, 32, "prologue Target nonce")?,
        app_digest: Some(exact_bytes(&mut d, 32, "prologue App digest")?),
        expiry_ms: Some(d.u64()?),
        pbs: None,
    };
    if d.position() != bytes.len() {
        return Err("trailing bootstrap prologue bytes".into());
    }
    Ok(facts)
}

fn decode_bootstrap_map(bytes: &[u8], m2: bool) -> AnyResult<BootstrapFacts> {
    strict_cbor(bytes, if m2 { "bootstrap M2" } else { "bootstrap M1" })?;
    let mut d = Decoder::new(bytes);
    if d.map()? != Some(if m2 { 7 } else { 4 }) {
        return Err("bootstrap payload map size mismatch".into());
    }
    cbor_key(&mut d, 0)?;
    if d.u8()? != 1 {
        return Err("bootstrap payload version mismatch".into());
    }
    let pbs = if m2 {
        cbor_key(&mut d, 1)?;
        Some(exact_bytes(&mut d, 32, "M2 PBS")?)
    } else {
        None
    };
    cbor_key(&mut d, if m2 { 2 } else { 1 })?;
    let target_reference_digest = exact_bytes(&mut d, 32, "payload Target Reference digest")?;
    cbor_key(&mut d, if m2 { 3 } else { 2 })?;
    let lease_id = exact_bytes(&mut d, 16, "payload lease id")?;
    cbor_key(&mut d, if m2 { 4 } else { 3 })?;
    let target_nonce = exact_bytes(&mut d, 32, "payload Target nonce")?;
    let (expiry_ms, app_digest) = if m2 {
        cbor_key(&mut d, 5)?;
        let expiry = d.u64()?;
        cbor_key(&mut d, 6)?;
        (
            Some(expiry),
            Some(exact_bytes(&mut d, 32, "M2 App digest")?),
        )
    } else {
        (None, None)
    };
    if d.position() != bytes.len() {
        return Err("trailing bootstrap payload bytes".into());
    }
    Ok(BootstrapFacts {
        target_reference_digest,
        lease_id,
        target_nonce,
        app_digest,
        expiry_ms,
        pbs,
    })
}

fn verify_bootstrap_material_binding(canonical: &Value, material: &Value) -> AnyResult<()> {
    let descriptor = decode_launch_descriptor(&hex::decode(
        canonical["launch_descriptor_cbor_hex"]
            .as_str()
            .ok_or("launch descriptor missing")?,
    )?)?;
    let prologue = decode_bootstrap_prologue(&hex::decode(
        canonical["prologue_cbor_hex"]
            .as_str()
            .ok_or("bootstrap prologue missing")?,
    )?)?;
    let m1 = decode_bootstrap_map(
        &hex::decode(
            canonical["m1_payload_cbor_hex"]
                .as_str()
                .ok_or("bootstrap M1 missing")?,
        )?,
        false,
    )?;
    let m2 = decode_bootstrap_map(
        &hex::decode(
            canonical["m2_payload_cbor_hex"]
                .as_str()
                .ok_or("bootstrap M2 missing")?,
        )?,
        true,
    )?;
    let target_reference = canonical["target_reference"]
        .as_str()
        .ok_or("Target Reference missing")?;
    validate_target_reference_roundtrip(target_reference)?;
    let digest = hex::decode(
        canonical["target_reference_digest_hex"]
            .as_str()
            .ok_or("Target Reference digest missing")?,
    )?;
    if digest.len() != 32 || sha256_hex(target_reference.as_bytes()) != hex::encode(&digest) {
        return Err("Target Reference digest is not derived from the full UTF-8 reference".into());
    }
    let random = hex::decode(
        material["target_reference_random_hex"]
            .as_str()
            .ok_or("Target Reference random material missing")?,
    )?;
    if random.len() != 32
        || format!("target_{}", URL_SAFE_NO_PAD.encode(random)) != target_reference
    {
        return Err("Target Reference does not match accepted random material".into());
    }
    let static_public = hex::decode(
        material["broker_static_public_hex"]
            .as_str()
            .ok_or("Broker static public material missing")?,
    )?;
    let pbs = hex::decode(
        material["process_bootstrap_secret_hex"]
            .as_str()
            .ok_or("PBS material missing")?,
    )?;
    let expected_endpoint = match canonical["launch_platform"].as_str() {
        Some("ios_simulator") => LaunchEndpoint::Ios {
            host: canonical["launch_endpoint"]["host"]
                .as_str()
                .ok_or("iOS endpoint host missing")?
                .to_owned(),
            port: canonical["launch_endpoint"]["port"]
                .as_u64()
                .ok_or("iOS endpoint port missing")?,
        },
        Some("android_emulator") => LaunchEndpoint::Android {
            name: canonical["launch_endpoint"]["localabstract_name"]
                .as_str()
                .ok_or("Android endpoint name missing")?
                .to_owned(),
        },
        _ => return Err("accepted launch platform missing or unknown".into()),
    };
    if descriptor.endpoint != expected_endpoint
        || descriptor.broker_static_public != static_public
        || descriptor.target_reference_digest != digest
        || descriptor.target_reference_digest != prologue.target_reference_digest
        || descriptor.target_reference_digest != m1.target_reference_digest
        || descriptor.target_reference_digest != m2.target_reference_digest
        || descriptor.lease_id != prologue.lease_id
        || descriptor.lease_id != m1.lease_id
        || descriptor.lease_id != m2.lease_id
        || descriptor.target_nonce != prologue.target_nonce
        || descriptor.target_nonce != m1.target_nonce
        || descriptor.target_nonce != m2.target_nonce
        || Some(&descriptor.app_digest) != prologue.app_digest.as_ref()
        || Some(&descriptor.app_digest) != m2.app_digest.as_ref()
        || Some(descriptor.expiry_ms) != prologue.expiry_ms
        || Some(descriptor.expiry_ms) != m2.expiry_ms
        || m2.pbs.as_deref() != Some(pbs.as_slice())
    {
        return Err("launch descriptor/bootstrap material binding mismatch".into());
    }
    Ok(())
}

struct SessionPrologueFacts {
    generation: u64,
    request_limit: u64,
    response_limit: u64,
}

fn decode_session_prologue(bytes: &[u8]) -> AnyResult<SessionPrologueFacts> {
    strict_cbor(bytes, "session prologue")?;
    let mut d = Decoder::new(bytes);
    if d.array()? != Some(12)
        || d.str()? != "apppilotkit.transport"
        || d.u8()? != 1
        || d.str()? != "session"
        || d.u8()? != 0
        || d.u8()? != 1
    {
        return Err("session prologue prefix/roles mismatch".into());
    }
    exact_bytes(&mut d, 16, "session lease id")?;
    let generation = d.u64()?;
    let epoch = d.u64()?;
    let request_limit = d.u64()?;
    let response_limit = d.u64()?;
    if d.u64()? != 8_192 {
        return Err("session prologue handshake cap mismatch".into());
    }
    exact_bytes(&mut d, 32, "NK handshake hash")?;
    if generation == 0 || epoch == 0 || d.position() != bytes.len() {
        return Err("session prologue generation/epoch/trailing bytes invalid".into());
    }
    Ok(SessionPrologueFacts {
        generation,
        request_limit,
        response_limit,
    })
}

fn exact_json_keys(value: &Value, expected: &[&str], field: &str) -> AnyResult<()> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{field} is not an object"))?;
    let keys = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    if keys != expected.iter().copied().collect::<BTreeSet<_>>() {
        return Err(format!("{field} member inventory mismatch").into());
    }
    Ok(())
}

fn verify_session_response_binding(vector: &Value) -> AnyResult<()> {
    let canonical = &vector["canonical_input"];
    let expected = &vector["expected"];
    let request = parse_strict_json(&hex::decode(
        canonical["session_open_utf8_hex"]
            .as_str()
            .ok_or("session.open request missing")?,
    )?)?;
    let response = parse_strict_json(&hex::decode(
        canonical["session_open_response_utf8_hex"]
            .as_str()
            .ok_or("session.open response missing")?,
    )?)?;
    exact_json_keys(
        &response,
        &["id", "jsonrpc", "result"],
        "session.open response",
    )?;
    exact_json_keys(
        &response["result"],
        &["capabilities", "context", "limits", "protocol"],
        "session.open result",
    )?;
    exact_json_keys(
        &response["result"]["context"],
        &["generation", "id"],
        "session context",
    )?;
    exact_json_keys(
        &response["result"]["protocol"],
        &["major", "minor"],
        "session protocol",
    )?;
    exact_json_keys(
        &response["result"]["limits"],
        &["maxPageItems", "maxRequestBytes", "maxResponseBytes"],
        "session limits",
    )?;
    let prologue = decode_session_prologue(&hex::decode(
        canonical["prologue_cbor_hex"]
            .as_str()
            .ok_or("session prologue missing")?,
    )?)?;
    let capabilities = response["result"]["capabilities"]
        .as_array()
        .ok_or("session capabilities are not an array")?;
    let required = request["params"]["requiredCapabilities"]
        .as_array()
        .ok_or("required capabilities are not an array")?;
    if response["jsonrpc"] != "2.0"
        || response["id"] != request["id"]
        || response["result"]["context"]["id"] != expected["target_issued_session_id"]
        || response["result"]["context"]["generation"] != expected["target_process_generation"]
        || response["result"]["context"]["generation"].as_u64() != Some(prologue.generation)
        || response["result"]["protocol"] != expected["negotiated_protocol"]
        || response["result"]["capabilities"] != expected["negotiated_capabilities"]
        || response["result"]["limits"] != expected["negotiated_limits"]
        || response["result"]["limits"]["maxRequestBytes"].as_u64() != Some(prologue.request_limit)
        || response["result"]["limits"]["maxResponseBytes"].as_u64()
            != Some(prologue.response_limit)
        || required
            .iter()
            .any(|capability| !capabilities.contains(capability))
        || response["result"]["protocol"]["major"] != request["params"]["protocol"]["major"]
        || response["result"]["protocol"]["minor"]
            .as_u64()
            .is_none_or(|minor| {
                minor
                    < request["params"]["protocol"]["minMinor"]
                        .as_u64()
                        .unwrap_or(u64::MAX)
                    || minor
                        > request["params"]["protocol"]["maxMinor"]
                            .as_u64()
                            .unwrap_or_default()
            })
    {
        return Err("session.open response fact binding mismatch".into());
    }
    Ok(())
}

pub fn verify_fixture(path: &Path, max_broker_cbor_bytes: u64) -> AnyResult<FixtureOutcome> {
    let fixture: Value = serde_json::from_slice(&fs::read(path)?)?;
    verify_document(path, &fixture, max_broker_cbor_bytes)
}

pub fn verify_vector_case(
    contract_root: &Path,
    case: &Value,
    max_broker_cbor_bytes: u64,
) -> AnyResult<FixtureOutcome> {
    let document = serde_json::json!({
        "id": case["id"],
        "validator": case["validator"],
        "input": case["canonical_input"],
        "expected_result": case["expected_result"],
        "expected_close_reason": case["expected_close_reason"],
        "expected_error_kind": case["expected_error_kind"],
        "expected_dispatch": case["expected_dispatch"]
        ,"expected_handoff": case["expected_handoff"]
    });
    verify_document(
        &contract_root.join("reference/fixtures/vector-case.json"),
        &document,
        max_broker_cbor_bytes,
    )
}

fn verify_document(
    path: &Path,
    fixture: &Value,
    max_broker_cbor_bytes: u64,
) -> AnyResult<FixtureOutcome> {
    let id = fixture["id"]
        .as_str()
        .ok_or("fixture id missing")?
        .to_owned();
    let validator = validator_for_case_id(&id)?;
    let (result, close_reason) = match validator {
        "broker_packet" => verify_broker_packet(fixture, max_broker_cbor_bytes)?,
        "catalog_list_evidence" => verify_catalog_list_evidence(fixture)?,
        "ready_timestamps" => verify_ready_timestamps(fixture)?,
        "semantic_encoding" => verify_semantic_encoding(fixture)?,
        "json_semantics" => verify_json_semantics_fixture(fixture)?,
        "broker_diagnostic_roundtrip" => verify_broker_diagnostic_roundtrip(fixture)?,
        "noise_failure_classification" => verify_noise_failure_classification(fixture)?,
        "noise_wrong_role" => verify_noise_wrong_role(path, fixture)?,
        "noise_correct_role" => verify_noise_correct_role(path, fixture)?,
        "noise_finished_binding" => verify_noise_finished_binding(path, fixture)?,
        "noise_nk_cross_target" => verify_noise_nk_cross_target(path, fixture)?,
        "noise_cross_binding" => verify_noise_cross_binding(path, fixture)?,
        "deterministic_cbor" => verify_deterministic_cbor_fixture(fixture)?,
        "outer_frame" => verify_outer_frame(fixture)?,
        "record_reassembly" => verify_record_reassembly(fixture)?,
        "lifecycle" => verify_lifecycle(fixture)?,
        "secret_surface_scanner" => verify_secret_surface_scanner(fixture)?,
        "noise_finished_replay" => verify_noise_finished_replay(path, fixture)?,
        "evidence_completeness" => verify_evidence_completeness(fixture)?,
        "limit_lifecycle" => verify_limit_lifecycle(fixture)?,
        "dispatch_classification" => verify_dispatch_classification(fixture)?,
        "noise_nk_tamper" => verify_noise_nk_tamper(path, fixture)?,
        "noise_wrong_psk" => verify_noise_wrong_psk(path, fixture)?,
        "retained_evidence" => verify_retained_evidence(fixture)?,
        "lifecycle_v2" => verify_lifecycle_v2(path, fixture)?,
        "fresh_sessions_crypto" => verify_fresh_sessions_crypto(fixture)?,
        "handoff_classification" => verify_handoff_classification(path, fixture)?,
        "catalog_projection" => verify_catalog_projection(fixture)?,
        _ => return Err(format!("unknown fixture validator {validator}").into()),
    };
    let expected_result = fixture["expected_result"]
        .as_str()
        .ok_or("expected_result missing")?;
    let expected_close = fixture["expected_close_reason"]
        .as_str()
        .ok_or("expected_close_reason missing")?;
    if frame_error_kind(&id, expected_close)
        .is_some_and(|expected| fixture["expected_error_kind"].as_str() != Some(expected))
    {
        return Err(format!(
            "{id}: expected_error_kind must be sessionExpired for authenticated frame failure"
        )
        .into());
    }
    if result != expected_result || close_reason != expected_close {
        return Err(format!(
            "{id}: expected {expected_result}/{expected_close}, got {result}/{close_reason}"
        )
        .into());
    }
    Ok(FixtureOutcome {
        id,
        result,
        close_reason,
    })
}

fn frame_error_kind(id: &str, close_reason: &str) -> Option<&'static str> {
    let affected = matches!(
        id,
        "outer-zero-length"
            | "record-truncated-header"
            | "record-trailing-after-end"
            | "record-reorder"
            | "record-gap"
            | "record-overlap"
            | "record-interleave"
            | "half-duplex-peer-turn"
            | "record-unknown-flags"
            | "record-nonzero-reserved"
            | "record-non-start-total-len"
            | "close-record-invalid-reason"
            | "close-record-missing-handoff"
            | "close-record-non-shortest-cbor"
            | "cbor-duplicate-key"
            | "cbor-non-shortest-integer"
            | "cbor-out-of-order-key"
    );
    (affected && matches!(close_reason, "malformed" | "sequenceViolation"))
        .then_some("sessionExpired")
}

fn validator_for_case_id(id: &str) -> AnyResult<&'static str> {
    let validator = match id {
        "tamper-nk-message-2" => "noise_nk_tamper",
        "replay-session-finished" | "d0-6f-immediate-finished-replay" => "noise_finished_replay",
        "cross-target"
        | "d0-5-cross-binding-handshake-aead"
        | "d0-5b-cross-target-wrong-prologue" => "noise_nk_cross_target",
        "equal-generation-control" | "cross-lease" | "cross-generation" | "old-epoch" => {
            "noise_cross_binding"
        }
        "equal-role-control" => "noise_correct_role",
        "wrong-role" => "noise_wrong_role",
        "authenticated-session-binding-mismatch" => "noise_finished_binding",
        "wrong-psk" => "noise_wrong_psk",
        "broker-packet-cap-plus-one"
        | "broker-control-operation-cap-plus-one"
        | "broker-open-session-cap-plus-one"
        | "d0-1-packet-cap-plus-one" => "broker_packet",
        "outer-header-timeout"
        | "outer-body-timeout"
        | "outer-zero-length"
        | "outer-oversize"
        | "d0-6b-outer-frame-truncated" => "outer_frame",
        "record-truncated-header"
        | "record-trailing-after-end"
        | "record-reorder"
        | "record-gap"
        | "record-overlap"
        | "record-interleave"
        | "half-duplex-peer-turn"
        | "record-unknown-flags"
        | "record-nonzero-reserved"
        | "record-non-start-total-len"
        | "close-record-valid"
        | "close-record-invalid-reason"
        | "close-record-missing-handoff"
        | "close-record-non-shortest-cbor"
        | "d0-6c-record-gap" => "record_reassembly",
        "cbor-duplicate-key"
        | "cbor-non-shortest-integer"
        | "cbor-out-of-order-key"
        | "d0-6a-cbor-duplicate-key"
        | "cbor-depth-limit" => "deterministic_cbor",
        "request-oversize"
        | "response-oversize"
        | "session-open-oversize"
        | "nonce-record-limit"
        | "plaintext-byte-limit"
        | "ready-reference-expired"
        | "session-idle-expired"
        | "lease-idle-expired"
        | "lease-absolute-expired"
        | "heartbeat-timeout"
        | "cleanup-failure" => "limit_lifecycle",
        "ready-timestamps-inconsistent-window"
        | "ready-timestamps-expired"
        | "ready-timestamps-client-rewrite"
        | "d0-3-client-rewritten-ready-timestamps" => "ready_timestamps",
        "target-ref-second-redeem"
        | "atomic-close-wins-before-dispatch"
        | "d0-6d-target-ref-second-redeem" => "lifecycle",
        "pre-dispatch-authentication"
        | "pre-dispatch-timeout"
        | "post-dispatch-read-timeout"
        | "post-dispatch-mutation-eof" => "dispatch_classification",
        "secret-surface-argv"
        | "secret-surface-environment"
        | "secret-surface-activity_extras"
        | "secret-surface-stdout"
        | "secret-surface-stderr"
        | "secret-surface-product_logs"
        | "secret-surface-diagnostics"
        | "secret-surface-machine_result"
        | "secret-surface-next_actions"
        | "secret-surface-artifacts"
        | "secret-surface-smoke_host_build_artifact"
        | "secret-surface-production_build_artifact"
        | "secret-surface-release-build-artifact"
        | "secret-surface-dishonest-count"
        | "secret-surface-release_build_artifact"
        | "d0-6e-secret-surface-canary-hit"
        | "d0-6e2-secret-surface-both-canaries-absent" => "secret_surface_scanner",
        "d0-2-impossible-catalog-list-evidence" => "catalog_list_evidence",
        "d0-4-unicode-path-byte-cap" => "semantic_encoding",
        "d0-4c-error-message-byte-cap" => "json_semantics",
        "d0-4b-broker-json-cbor-roundtrip" => "broker_diagnostic_roundtrip",
        "d0-7-missing-helper-and-smoke-artifact" | "d0-7b-zero-byte-build-artifacts" => {
            "evidence_completeness"
        }
        "inconsistent-retained-evidence" => "retained_evidence",
        "prepare-no-lease-launch-bootstrap"
        | "prepare-eligible-owned-lease-mints-ref-no-launch-no-bootstrap"
        | "prepare-live-conflicting-build-fails-no-relaunch"
        | "prepare-reuse-new-bootstrap-transcript-rejected"
        | "two-fresh-refs-independent-redemption"
        | "concurrent-read-both-complete"
        | "close-session-a-session-b-remains-open"
        | "session-a-idle-expiry-session-b-remains-open"
        | "session-a-auth-failure-session-b-remains-open"
        | "lease-loss-stales-both"
        | "epoch-loss-stales-both"
        | "process-loss-stales-both" => "lifecycle_v2",
        "broker-heartbeat-loss-stales-both" => "lifecycle_v2",
        "catalog-complete-nonempty-projects-show"
        | "catalog-truncated-projects-continuation"
        | "catalog-complete-empty-projects-list-selector" => "catalog_projection",
        "two-agent-fresh-noise-and-target-session-ids" => "fresh_sessions_crypto",
        "broker-lost-pre-send-read"
        | "broker-lost-pre-send-invoke"
        | "broker-lost-partial-read"
        | "broker-lost-partial-invoke"
        | "broker-lost-full-before-response-read"
        | "broker-lost-full-before-response-invoke"
        | "broker-lost-safe-response-lost-read"
        | "broker-lost-safe-response-lost-invoke"
        | "broker-lost-response-partial-eof-read"
        | "broker-lost-response-partial-eof-invoke"
        | "broker-response-complete-read"
        | "broker-response-complete-invoke" => "handoff_classification",
        _ => return Err(format!("case id is not in the pinned validator inventory: {id}").into()),
    };
    Ok(validator)
}

fn verify_json_semantics_fixture(fixture: &Value) -> AnyResult<(&'static str, &'static str)> {
    match validate_json_semantics(&fixture["input"]) {
        Ok(()) => Ok(("accepted", "none")),
        Err(error) if error.to_string().contains("UTF-8 bytes") => Ok(("rejected", "oversize")),
        Err(_) => Ok(("rejected", "malformed")),
    }
}

fn verify_broker_diagnostic_roundtrip(fixture: &Value) -> AnyResult<(&'static str, &'static str)> {
    let expected = &fixture["input"]["diagnostic"];
    validate_json_semantics(expected)?;
    let raw = hex::decode(
        fixture["input"]["raw_cbor_hex"]
            .as_str()
            .ok_or("Broker diagnostic CBOR missing")?,
    )?;
    validate_deterministic_cbor(&raw)
        .map_err(|error| format!("Broker diagnostic CBOR: {error}"))?;

    let mut decoder = Decoder::new(&raw);
    if decoder.map()? != Some(4) || decoder.u8()? != 0 || decoder.u8()? != 1 {
        return Ok(("rejected", "malformed"));
    }
    if decoder.u8()? != 1 {
        return Ok(("rejected", "malformed"));
    }
    let request_id_bytes = decoder.bytes()?;
    if request_id_bytes.len() != 16 || decoder.u8()? != 2 || !decoder.bool()? {
        return Ok(("rejected", "malformed"));
    }
    if decoder.u8()? != 3 || decoder.map()? != Some(6) {
        return Ok(("rejected", "malformed"));
    }
    if decoder.u8()? != 0 || decoder.u8()? != 0 || decoder.u8()? != 1 {
        return Ok(("rejected", "malformed"));
    }
    let target_token = decoder.bytes()?;
    if target_token.len() != 32 || decoder.u8()? != 3 {
        return Ok(("rejected", "malformed"));
    }
    let process_generation = decoder.u64()?;
    if decoder.u8()? != 4 {
        return Ok(("rejected", "malformed"));
    }
    let listener_epoch = decoder.u64()?;
    if decoder.u8()? != 5 {
        return Ok(("rejected", "malformed"));
    }
    let issued_at_unix_ms = decoder.u64()?;
    if decoder.u8()? != 6 {
        return Ok(("rejected", "malformed"));
    }
    let expires_at_unix_ms = decoder.u64()?;
    if decoder.position() != raw.len() {
        return Ok(("rejected", "malformed"));
    }
    let diagnostic = serde_json::json!({
        "schema_version":"1.0",
        "request_id":URL_SAFE_NO_PAD.encode(request_id_bytes),
        "status":"succeeded",
        "result":{
            "kind":"target_ready",
            "target":format!("target_{}", URL_SAFE_NO_PAD.encode(target_token)),
            "process_generation":process_generation,
            "listener_epoch":listener_epoch,
            "issued_at_unix_ms":issued_at_unix_ms,
            "expires_at_unix_ms":expires_at_unix_ms
        }
    });
    if diagnostic != *expected {
        return Ok(("rejected", "bindingMismatch"));
    }

    let mut encoded = Encoder::new(Vec::new());
    encoded
        .map(4)?
        .u8(0)?
        .u8(1)?
        .u8(1)?
        .bytes(request_id_bytes)?
        .u8(2)?
        .bool(true)?
        .u8(3)?
        .map(6)?
        .u8(0)?
        .u8(0)?
        .u8(1)?
        .bytes(target_token)?
        .u8(3)?
        .u64(process_generation)?
        .u8(4)?
        .u64(listener_epoch)?
        .u8(5)?
        .u64(issued_at_unix_ms)?
        .u8(6)?
        .u64(expires_at_unix_ms)?;
    if encoded.into_writer() != raw {
        return Ok(("rejected", "bindingMismatch"));
    }
    Ok(("accepted", "none"))
}

fn verify_noise_cross_binding(
    fixture_path: &Path,
    fixture: &Value,
) -> AnyResult<(&'static str, &'static str)> {
    let relative = fixture["input"]
        .get("binding_vector")
        .and_then(Value::as_str)
        .unwrap_or("vectors/binding-replay-failures.json");
    let case_id = fixture["input"]
        .get("case_id")
        .and_then(Value::as_str)
        .or_else(|| fixture["id"].as_str())
        .ok_or("cross-binding case id missing")?;
    let contract_root = fixture_path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or("fixture is not under reference/fixtures")?;
    let vector: Value = serde_json::from_slice(&fs::read(contract_root.join(relative))?)?;
    let case = vector["vectors"]
        .as_array()
        .ok_or("binding vector cases missing")?
        .iter()
        .find(|candidate| candidate["id"] == case_id)
        .ok_or("cross-binding vector case missing")?;
    let input = &case["canonical_input"];
    let wrong_prologue = hex::decode(
        input["mismatched_prologue_hex"]
            .as_str()
            .ok_or("mismatched prologue missing")?,
    )?;
    let original_m1 = unframe_u16_hex(
        input["original_handshake_m1_outer_hex"]
            .as_str()
            .ok_or("original handshake M1 missing")?,
    )?;
    let psk: [u8; 32] = hex::decode(
        vector["test_only_material"]["material"]["process_bootstrap_secret_hex"]
            .as_str()
            .ok_or("binding vector PSK missing")?,
    )?
    .try_into()
    .map_err(|_| "binding vector PSK is not 32 bytes")?;
    let params: NoiseParams = "Noise_NNpsk0_25519_ChaChaPoly_SHA256".parse()?;
    let mut wrong_responder = Builder::new(params)
        .prologue(&wrong_prologue)?
        .psk(0, &psk)?
        .build_responder()?;
    let mut plaintext = vec![0_u8; 8_192];
    if wrong_responder
        .read_message(&original_m1, &mut plaintext)
        .is_err()
    {
        Ok(("rejected", "authenticationFailed"))
    } else {
        Ok(("accepted", "none"))
    }
}

fn resolve_binding_case(fixture_path: &Path, fixture: &Value) -> AnyResult<Value> {
    if fixture["input"].get("mismatched_prologue_hex").is_some() {
        return Ok(fixture["input"].clone());
    }
    let relative = fixture["input"]["binding_vector"]
        .as_str()
        .ok_or("binding vector path missing")?;
    let case_id = fixture["input"]["case_id"]
        .as_str()
        .ok_or("binding case id missing")?;
    let contract_root = fixture_path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or("fixture is not under reference/fixtures")?;
    let vector: Value = serde_json::from_slice(&fs::read(contract_root.join(relative))?)?;
    vector["vectors"]
        .as_array()
        .ok_or_else(|| Box::<dyn Error>::from("binding vector cases missing"))?
        .iter()
        .find(|candidate| candidate["id"] == case_id)
        .map(|case| case["canonical_input"].clone())
        .ok_or_else(|| Box::<dyn Error>::from("binding case missing"))
}

fn verify_noise_nk_cross_target(
    fixture_path: &Path,
    fixture: &Value,
) -> AnyResult<(&'static str, &'static str)> {
    let input = resolve_binding_case(fixture_path, fixture)?;
    let prologue = hex::decode(
        input["mismatched_prologue_hex"]
            .as_str()
            .ok_or("wrong NK prologue missing")?,
    )?;
    let private = hex::decode(
        input["broker_static_private_hex"]
            .as_str()
            .ok_or("Broker static private missing")?,
    )?;
    let m1 = unframe_u16_hex(
        input["original_handshake_m1_outer_hex"]
            .as_str()
            .ok_or("NK M1 missing")?,
    )?;
    let params: NoiseParams = "Noise_NK_25519_ChaChaPoly_SHA256".parse()?;
    let mut responder = Builder::new(params)
        .prologue(&prologue)?
        .local_private_key(&private)?
        .build_responder()?;
    let mut plaintext = vec![0_u8; 8_192];
    Ok(if responder.read_message(&m1, &mut plaintext).is_err() {
        ("rejected", "authenticationFailed")
    } else {
        ("accepted", "none")
    })
}

fn verify_noise_wrong_role(
    fixture_path: &Path,
    fixture: &Value,
) -> AnyResult<(&'static str, &'static str)> {
    let (vector, input) = session_vector_and_input(fixture_path, fixture)?;
    let prologue = hex::decode(
        vector["canonical_input"]["prologue_cbor_hex"]
            .as_str()
            .ok_or("session prologue missing")?,
    )?;
    let psk: [u8; 32] = hex::decode(
        vector["test_only_material"]["material"]["process_bootstrap_secret_hex"]
            .as_str()
            .ok_or("PSK missing")?,
    )?
    .try_into()
    .map_err(|_| "PSK is not 32 bytes")?;
    let m1 = unframe_u16_hex(
        input["original_handshake_m1_outer_hex"]
            .as_str()
            .ok_or("session M1 missing")?,
    )?;
    let params: NoiseParams = "Noise_NNpsk0_25519_ChaChaPoly_SHA256".parse()?;
    let mut wrong_initiator = Builder::new(params)
        .prologue(&prologue)?
        .psk(0, &psk)?
        .build_initiator()?;
    let mut plaintext = vec![0_u8; 8_192];
    Ok(
        if wrong_initiator.read_message(&m1, &mut plaintext).is_err() {
            ("rejected", "authenticationFailed")
        } else {
            ("accepted", "none")
        },
    )
}

fn verify_noise_correct_role(
    fixture_path: &Path,
    fixture: &Value,
) -> AnyResult<(&'static str, &'static str)> {
    let (vector, input) = session_vector_and_input(fixture_path, fixture)?;
    let prologue = hex::decode(
        vector["canonical_input"]["prologue_cbor_hex"]
            .as_str()
            .ok_or("session prologue missing")?,
    )?;
    let psk: [u8; 32] = hex::decode(
        vector["test_only_material"]["material"]["process_bootstrap_secret_hex"]
            .as_str()
            .ok_or("PSK missing")?,
    )?
    .try_into()
    .map_err(|_| "PSK is not 32 bytes")?;
    let m1 = unframe_u16_hex(
        input["original_handshake_m1_outer_hex"]
            .as_str()
            .ok_or("session M1 missing")?,
    )?;
    let params: NoiseParams = "Noise_NNpsk0_25519_ChaChaPoly_SHA256".parse()?;
    let mut responder = Builder::new(params)
        .prologue(&prologue)?
        .psk(0, &psk)?
        .build_responder()?;
    let mut plaintext = vec![0_u8; 8_192];
    Ok(if responder.read_message(&m1, &mut plaintext).is_ok() {
        ("accepted", "none")
    } else {
        ("rejected", "authenticationFailed")
    })
}

fn verify_noise_finished_binding(
    fixture_path: &Path,
    fixture: &Value,
) -> AnyResult<(&'static str, &'static str)> {
    let (vector, input) = session_vector_and_input(fixture_path, fixture)?;
    let prologue = hex::decode(
        vector["canonical_input"]["prologue_cbor_hex"]
            .as_str()
            .ok_or("session prologue missing")?,
    )?;
    let material = &vector["test_only_material"]["material"];
    let psk: [u8; 32] = hex::decode(
        material["process_bootstrap_secret_hex"]
            .as_str()
            .ok_or("PSK missing")?,
    )?
    .try_into()
    .map_err(|_| "PSK is not 32 bytes")?;
    let target_key = hex::decode(
        material["target_ephemeral_private_hex"]
            .as_str()
            .ok_or("Target key missing")?,
    )?;
    let broker_key = hex::decode(
        material["broker_ephemeral_private_hex"]
            .as_str()
            .ok_or("Broker key missing")?,
    )?;
    let params: NoiseParams = "Noise_NNpsk0_25519_ChaChaPoly_SHA256".parse()?;
    let mut target = Builder::new(params.clone())
        .prologue(&prologue)?
        .psk(0, &psk)?
        .fixed_ephemeral_key_for_testing_only(&target_key)
        .build_initiator()?;
    let mut broker = Builder::new(params)
        .prologue(&prologue)?
        .psk(0, &psk)?
        .fixed_ephemeral_key_for_testing_only(&broker_key)
        .build_responder()?;
    let m1 = unframe_u16_hex(
        vector["expected"]["m1_outer_hex"]
            .as_str()
            .ok_or("M1 missing")?,
    )?;
    let m2 = unframe_u16_hex(
        vector["expected"]["m2_outer_hex"]
            .as_str()
            .ok_or("M2 missing")?,
    )?;
    let mut buffer = vec![0_u8; 65_535];
    let mut generated = vec![0_u8; 65_535];
    target.write_message(&[], &mut generated)?;
    broker.read_message(&m1, &mut buffer)?;
    broker.write_message(&[], &mut generated)?;
    target.read_message(&m2, &mut buffer)?;
    let mut broker_transport = broker.into_transport_mode()?;
    let finished = unframe_u16_hex(
        input["target_finished_outer_hex"]
            .as_str()
            .ok_or("Finished missing")?,
    )?;
    let len = broker_transport.read_message(&finished, &mut buffer)?;
    if len < 12 || buffer[0] != 2 || buffer[1] != 3 {
        return Ok(("rejected", "malformed"));
    }
    let mut decoder = Decoder::new(&buffer[12..len]);
    if decoder.map()? != Some(6)
        || decoder.u8()? != 0
        || decoder.u8()? != 1
        || decoder.u8()? != 1
        || decoder.u8()? != 0
        || decoder.u8()? != 2
    {
        return Ok(("rejected", "malformed"));
    }
    let _lease = decoder.bytes()?;
    if decoder.u8()? != 3 {
        return Ok(("rejected", "malformed"));
    }
    let observed_generation = decoder.u64()?;
    let stored_generation = input["stored_generation"]
        .as_u64()
        .ok_or("stored generation missing")?;
    Ok(if observed_generation != stored_generation {
        ("rejected", "bindingMismatch")
    } else {
        ("accepted", "none")
    })
}

fn session_vector_and_input(fixture_path: &Path, fixture: &Value) -> AnyResult<(Value, Value)> {
    let input = if fixture["input"].get("session_vector").is_some() {
        fixture["input"].clone()
    } else {
        return Err("session binding input missing".into());
    };
    let contract_root = fixture_path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or("fixture is not under reference/fixtures")?;
    let vector: Value = serde_json::from_slice(&fs::read(
        contract_root.join(
            input["session_vector"]
                .as_str()
                .ok_or("session vector path missing")?,
        ),
    )?)?;
    Ok((vector, input))
}

fn verify_limit_lifecycle(fixture: &Value) -> AnyResult<(&'static str, &'static str)> {
    let id = fixture["id"]
        .as_str()
        .ok_or("limit/lifecycle case id missing")?;
    let input = &fixture["input"];
    let outcome = match id {
        "request-oversize"
            if input["total_len"]
                .as_u64()
                .is_some_and(|value| value > 16_777_216) =>
        {
            ("rejected", "oversize")
        }
        "response-oversize"
            if input["total_len"]
                .as_u64()
                .is_some_and(|value| value > 67_108_864) =>
        {
            ("rejected", "oversize")
        }
        "session-open-oversize"
            if input["total_len"]
                .as_u64()
                .is_some_and(|value| value > 65_536) =>
        {
            ("rejected", "oversize")
        }
        "nonce-record-limit"
            if input["next_record"]
                .as_u64()
                .is_some_and(|value| value >= 4_294_967_296) =>
        {
            ("rejected", "recordLimit")
        }
        "plaintext-byte-limit"
            if input["accepted_plaintext_bytes"]
                .as_u64()
                .unwrap_or(0)
                .saturating_add(input["next_plaintext_bytes"].as_u64().unwrap_or(0))
                >= 1_099_511_627_776 =>
        {
            ("rejected", "recordLimit")
        }
        "ready-reference-expired"
            if input["age_ms"]
                .as_u64()
                .is_some_and(|value| value >= 30_000) =>
        {
            ("rejected", "stale")
        }
        "session-idle-expired"
            if input["idle_ms"]
                .as_u64()
                .is_some_and(|value| value >= 30_000) =>
        {
            ("rejected", "stale")
        }
        "lease-idle-expired"
            if input["idle_ms"]
                .as_u64()
                .is_some_and(|value| value >= 120_000) =>
        {
            ("rejected", "stale")
        }
        "lease-absolute-expired"
            if input["age_ms"]
                .as_u64()
                .is_some_and(|value| value >= 900_000) =>
        {
            ("rejected", "stale")
        }
        "heartbeat-timeout"
            if input["missed"].as_u64().is_some_and(|value| value >= 4)
                && input["elapsed_ms"]
                    .as_u64()
                    .is_some_and(|value| value >= 120_000) =>
        {
            ("rejected", "brokerLost")
        }
        "cleanup-failure"
            if input["elapsed_ms"]
                .as_u64()
                .is_some_and(|value| value >= 2_000) =>
        {
            ("rejected", "cleanupFailed")
        }
        _ => ("accepted", "none"),
    };
    Ok(outcome)
}

fn verify_dispatch_classification(fixture: &Value) -> AnyResult<(&'static str, &'static str)> {
    let input = &fixture["input"];
    let crossed = input["dispatch_boundary_crossed"]
        .as_bool()
        .ok_or("dispatch boundary marker missing")?;
    let side_effect = input["side_effect"].as_str().ok_or("side effect missing")?;
    let failure = input["failure"]
        .as_str()
        .ok_or("dispatch failure missing")?;
    let (error_kind, dispatch, close) = match (crossed, side_effect, failure) {
        (false, _, "authentication_failed") => (
            "transport.authenticationRequired",
            "not_dispatched",
            "authenticationFailed",
        ),
        (false, _, "deadline") => ("timeout", "not_dispatched", "timeout"),
        (true, "read_only", "deadline") => ("timeout", "dispatched", "timeout"),
        (true, "app_mutation" | "device_mutation", "eof") => {
            ("action.outcomeUnknown", "ambiguous", "peerClosed")
        }
        _ => ("internalError", "ambiguous", "internalError"),
    };
    if fixture["expected_error_kind"]
        .as_str()
        .is_some_and(|expected| expected != error_kind)
        || fixture["expected_dispatch"]
            .as_str()
            .is_some_and(|expected| expected != dispatch)
    {
        return Ok(("rejected", "internalError"));
    }
    Ok(("rejected", close))
}

fn verify_noise_nk_tamper(
    fixture_path: &Path,
    fixture: &Value,
) -> AnyResult<(&'static str, &'static str)> {
    let relative = fixture["input"]["bootstrap_vector"]
        .as_str()
        .ok_or("bootstrap vector path missing")?;
    let contract_root = fixture_path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or("fixture is not under reference/fixtures")?;
    let vector: Value = serde_json::from_slice(&fs::read(contract_root.join(relative))?)?;
    let canonical = &vector["canonical_input"];
    let material = &vector["test_only_material"]["material"];
    let prologue = hex::decode(
        canonical["prologue_cbor_hex"]
            .as_str()
            .ok_or("bootstrap prologue missing")?,
    )?;
    let static_public = hex::decode(
        material["broker_static_public_hex"]
            .as_str()
            .ok_or("Broker static public missing")?,
    )?;
    let static_private = hex::decode(
        material["broker_static_private_hex"]
            .as_str()
            .ok_or("Broker static private missing")?,
    )?;
    let target_key = hex::decode(
        material["target_ephemeral_private_hex"]
            .as_str()
            .ok_or("Target ephemeral missing")?,
    )?;
    let m1_payload = hex::decode(
        canonical["m1_payload_cbor_hex"]
            .as_str()
            .ok_or("bootstrap m1 payload missing")?,
    )?;
    let m2_payload = hex::decode(
        canonical["m2_payload_cbor_hex"]
            .as_str()
            .ok_or("bootstrap m2 payload missing")?,
    )?;
    let params: NoiseParams = canonical["noise_name"]
        .as_str()
        .ok_or("bootstrap Noise name missing")?
        .parse()?;
    let mut target = Builder::new(params.clone())
        .prologue(&prologue)?
        .remote_public_key(&static_public)?
        .fixed_ephemeral_key_for_testing_only(&target_key)
        .build_initiator()?;
    let mut broker = Builder::new(params)
        .prologue(&prologue)?
        .local_private_key(&static_private)?
        .build_responder()?;
    let expected_m1 = unframe_u16_hex(
        vector["expected"]["m1_outer_hex"]
            .as_str()
            .ok_or("bootstrap m1 outer missing")?,
    )?;
    let tampered_m2 = unframe_u16_hex(
        fixture["input"]["raw_outer_hex"]
            .as_str()
            .ok_or("tampered m2 outer missing")?,
    )?;
    let mut out = vec![0_u8; 65_535];
    let mut generated = vec![0_u8; 65_535];
    let len = target.write_message(&m1_payload, &mut generated)?;
    if generated[..len] != expected_m1
        || broker.read_message(&expected_m1, &mut out)? != m1_payload.len()
    {
        return Ok(("rejected", "internalError"));
    }
    broker.write_message(&m2_payload, &mut generated)?;
    if target.read_message(&tampered_m2, &mut out).is_err() {
        Ok(("rejected", "authenticationFailed"))
    } else {
        Ok(("accepted", "none"))
    }
}

fn verify_noise_wrong_psk(
    fixture_path: &Path,
    fixture: &Value,
) -> AnyResult<(&'static str, &'static str)> {
    let relative = fixture["input"]["session_vector"]
        .as_str()
        .ok_or("wrong-PSK session vector path missing")?;
    let contract_root = fixture_path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or("fixture is not under reference/fixtures")?;
    let vector: Value = serde_json::from_slice(&fs::read(contract_root.join(relative))?)?;
    let prologue = hex::decode(
        vector["canonical_input"]["prologue_cbor_hex"]
            .as_str()
            .ok_or("session prologue missing")?,
    )?;
    let m1 = unframe_u16_hex(
        vector["expected"]["m1_outer_hex"]
            .as_str()
            .ok_or("session M1 missing")?,
    )?;
    let wrong_psk: [u8; 32] = hex::decode(
        fixture["input"]["wrong_psk_hex"]
            .as_str()
            .ok_or("wrong PSK bytes missing")?,
    )?
    .try_into()
    .map_err(|_| "wrong PSK must be exactly 32 bytes")?;
    let params: NoiseParams = vector["canonical_input"]["noise_name"]
        .as_str()
        .ok_or("session Noise name missing")?
        .parse()?;
    let mut responder = Builder::new(params)
        .prologue(&prologue)?
        .psk(0, &wrong_psk)?
        .build_responder()?;
    let mut plaintext = vec![0_u8; 8_192];
    if responder.read_message(&m1, &mut plaintext).is_err() {
        Ok(("rejected", "authenticationFailed"))
    } else {
        Ok(("accepted", "none"))
    }
}

fn verify_retained_evidence(fixture: &Value) -> AnyResult<(&'static str, &'static str)> {
    let input = &fixture["input"];
    let stdout = hex::decode(
        input["stdout_hex"]
            .as_str()
            .ok_or("retained stdout bytes missing")?,
    )?;
    let machine_result = hex::decode(
        input["machine_result_hex"]
            .as_str()
            .ok_or("retained Machine Result bytes missing")?,
    )?;
    let stdout_digest = format!("sha256:{}", sha256_hex(&stdout));
    let machine_digest = format!("sha256:{}", sha256_hex(&machine_result));
    let prefix = input["installed_prefix"]
        .as_str()
        .ok_or("installed prefix missing")?;
    let invoked = input["invoked_executable"]
        .as_str()
        .ok_or("invoked executable missing")?;
    let expected_executable = format!("{prefix}/bin/apppilotkit");
    let consistent = invoked == expected_executable
        && stdout == machine_result
        && input["declared_stdout_sha256"].as_str() == Some(stdout_digest.as_str())
        && input["declared_machine_result_sha256"].as_str() == Some(machine_digest.as_str())
        && input["target_generation"] == input["catalog_generation"]
        && matches!(
            (
                input["platform"].as_str(),
                input["tool"].as_str(),
                input["transport"].as_str()
            ),
            (
                Some("ios-simulator"),
                Some("simctl"),
                Some("ios_simulator_loopback_nk")
            ) | (
                Some("android-emulator"),
                Some("adb"),
                Some("android_emulator_adb_forward_localabstract_nk")
            )
        );
    Ok(if consistent {
        ("accepted", "none")
    } else {
        ("rejected", "bindingMismatch")
    })
}

fn verify_lifecycle_v2(
    fixture_path: &Path,
    fixture: &Value,
) -> AnyResult<(&'static str, &'static str)> {
    let id = fixture["id"].as_str().ok_or("lifecycle-v2 id missing")?;
    let input = &fixture["input"];
    if id.starts_with("prepare-") {
        let contract_root = fixture_path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .ok_or("prepare fixture is not under reference/fixtures")?;
        let relative = input["bootstrap_vector"]
            .as_str()
            .ok_or("prepare bootstrap vector missing")?;
        let bootstrap: Value = serde_json::from_slice(&fs::read(contract_root.join(relative))?)?;
        verify_positive_bootstrap(&bootstrap)?;
        let expected_digest = &bootstrap["expected"]["transcript_sha256"];
        let observed = input["observed_bootstrap_transcript_sha256"]
            .as_array()
            .ok_or("observed bootstrap transcript inventory missing")?;
        let second = input["second_prepare_bootstrap_transcript_sha256"]
            .as_array()
            .ok_or("second prepare transcript inventory missing")?;
        if observed.len() != 1 || observed[0] != *expected_digest {
            return Ok(("rejected", "bindingMismatch"));
        }
        if id == "prepare-reuse-new-bootstrap-transcript-rejected" {
            return Ok(if second.is_empty() {
                ("accepted", "none")
            } else {
                ("rejected", "bindingMismatch")
            });
        }
        if !second.is_empty() {
            return Ok(("rejected", "bindingMismatch"));
        }
    }
    let outcome = match id {
        "prepare-no-lease-launch-bootstrap"
            if input["live_lease"] == false
                && input["launch_count"] == 1
                && input["nk_count"] == 1
                && input["pbs_replaced"] == true
                && input["minted_refs"] == 1 =>
        {
            ("accepted", "none")
        }
        "prepare-eligible-owned-lease-mints-ref-no-launch-no-bootstrap"
            if input["live_lease"] == true
                && input["broker_owned"] == true
                && input["prepare_key_match"] == true
                && input["generation_match"] == true
                && input["epoch_match"] == true
                && input["eligible"] == true
                && input["heartbeat_authenticated"] == true
                && input["launch_count"] == 0
                && input["nk_count"] == 0
                && input["pbs_replaced"] == false
                && input["minted_refs"] == 1 =>
        {
            ("accepted", "none")
        }
        "prepare-live-conflicting-build-fails-no-relaunch"
            if input["live_lease"] == true
                && input["prepare_key_match"] == false
                && input["launch_count"] == 0
                && input["nk_count"] == 0
                && input["minted_refs"] == 0 =>
        {
            ("rejected", "bindingMismatch")
        }
        "two-fresh-refs-independent-redemption" => {
            let refs = input["references"].as_array().ok_or("references missing")?;
            let valid = refs.len() == 2
                && refs[0]["token_hex"] != refs[1]["token_hex"]
                && refs.iter().all(|reference| {
                    reference["issued_at"]
                        .as_u64()
                        .and_then(|issued| issued.checked_add(30_000))
                        == reference["expires_at"].as_u64()
                        && reference["redeem_at"]
                            .as_u64()
                            .zip(reference["expires_at"].as_u64())
                            .is_some_and(|(redeem, expires)| redeem < expires)
                        && reference["redeem_count"] == 1
                        && reference["token_hex"]
                            .as_str()
                            .and_then(|value| hex::decode(value).ok())
                            .is_some_and(|bytes| bytes.len() == 32)
                });
            if valid {
                ("accepted", "none")
            } else {
                ("rejected", "stale")
            }
        }
        "concurrent-read-both-complete" => {
            let sessions = input["sessions"].as_array().ok_or("sessions missing")?;
            if sessions.len() == 2
                && sessions[0]["runtime_instance"] != sessions[1]["runtime_instance"]
                && sessions
                    .iter()
                    .all(|session| session["operation"] == "read" && session["state"] == "complete")
                && input["shared_catalog"] == true
                && input["shared_action_coordinator"] == true
            {
                ("accepted", "none")
            } else {
                ("rejected", "internalError")
            }
        }
        "close-session-a-session-b-remains-open"
        | "session-a-idle-expiry-session-b-remains-open"
        | "session-a-auth-failure-session-b-remains-open"
            if input["terminal_scope"] == "session"
                && input["sessions_after"]["b"] == "open"
                && input["all_session_invalidate_calls"] == 0 =>
        {
            ("accepted", "none")
        }
        "lease-loss-stales-both"
        | "epoch-loss-stales-both"
        | "process-loss-stales-both"
        | "broker-heartbeat-loss-stales-both"
            if input["terminal_scope"] == "lease"
                && input["sessions_after"]["a"] == "stale"
                && input["sessions_after"]["b"] == "stale"
                && input["refs_after"]["a"] == "stale"
                && input["refs_after"]["b"] == "stale"
                && input["all_session_invalidate_calls"] == 2 =>
        {
            ("rejected", "stale")
        }
        _ => ("rejected", "internalError"),
    };
    Ok(outcome)
}

fn verify_fresh_sessions_crypto(fixture: &Value) -> AnyResult<(&'static str, &'static str)> {
    let input = &fixture["input"];
    let prologue = hex::decode(
        input["prologue_cbor_hex"]
            .as_str()
            .ok_or("prologue missing")?,
    )?;
    let psk: [u8; 32] = hex::decode(input["psk_hex"].as_str().ok_or("PSK missing")?)?
        .try_into()
        .map_err(|_| "PSK is not 32 bytes")?;
    let m1_a = unframe_u16_hex(input["m1_a_outer_hex"].as_str().ok_or("M1 A missing")?)?;
    let m1_b = unframe_u16_hex(input["m1_b_outer_hex"].as_str().ok_or("M1 B missing")?)?;
    if m1_a == m1_b || input["session_id_a"] == input["session_id_b"] {
        return Ok(("rejected", "bindingMismatch"));
    }
    let params: NoiseParams = input["noise_name"]
        .as_str()
        .ok_or("Noise name missing")?
        .parse()?;
    let mut plaintext = vec![0_u8; 8_192];
    for m1 in [&m1_a, &m1_b] {
        let mut responder = Builder::new(params.clone())
            .prologue(&prologue)?
            .psk(0, &psk)?
            .build_responder()?;
        if responder.read_message(m1, &mut plaintext).is_err() {
            return Ok(("rejected", "authenticationFailed"));
        }
    }
    Ok(("accepted", "none"))
}

fn verify_handoff_classification(
    fixture_path: &Path,
    fixture: &Value,
) -> AnyResult<(&'static str, &'static str)> {
    let input = &fixture["input"];
    let contract_root = fixture_path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or("handoff fixture is not under reference/fixtures")?;
    let session_relative = input["session_vector"]
        .as_str()
        .ok_or("handoff session vector missing")?;
    let session: Value = serde_json::from_slice(&fs::read(contract_root.join(session_relative))?)?;
    let verified = verify_positive_session_ciphertexts(&session)?;
    let request_outer = input["request_outer_hex"]
        .as_str()
        .ok_or("request ciphertext missing")?;
    let response_outer = input["response_outer_hex"]
        .as_str()
        .ok_or("response ciphertext missing")?;
    if session["expected"]["session_open_outer_hex"] != request_outer
        || session["expected"]["session_open_response_outer_hex"] != response_outer
    {
        return Ok(("rejected", "internalError"));
    }
    let request_outer_bytes = hex::decode(request_outer)?;
    let response_outer_bytes = hex::decode(response_outer)?;
    if request_outer_bytes != verified.request_outer
        || response_outer_bytes != verified.response_outer
    {
        return Ok(("rejected", "internalError"));
    }
    unframe_u16_hex(request_outer)?;
    unframe_u16_hex(response_outer)?;
    let emitted = input["request_bytes_emitted"]
        .as_u64()
        .ok_or("emitted bytes missing")?;
    let total = input["request_total_bytes"]
        .as_u64()
        .ok_or("request total missing")?;
    let end = input["request_end_emitted"]
        .as_bool()
        .ok_or("request END missing")?;
    let response_end = input["response_end_reassembled"]
        .as_bool()
        .ok_or("response END missing")?;
    let response_bytes = input["response_bytes_reassembled"]
        .as_u64()
        .ok_or("response bytes missing")?;
    let response_total = input["response_total_bytes"]
        .as_u64()
        .ok_or("response total missing")?;
    if total != request_outer_bytes.len() as u64
        || response_total != response_outer_bytes.len() as u64
        || emitted > total
        || response_bytes > response_total
        || end != (emitted == total)
        || response_end != (response_bytes == response_total)
    {
        return Ok(("rejected", "internalError"));
    }
    let handed_off = emitted == total && end;
    let handoff = if handed_off {
        "handoff_possible_or_confirmed"
    } else {
        "not_handed_off"
    };
    let side_effect = input["side_effect"].as_str().ok_or("side effect missing")?;
    let failure = input["failure"].as_str().ok_or("failure missing")?;
    if response_end {
        if !handed_off
            || failure != "none"
            || fixture["expected_handoff"] != handoff
            || fixture["expected_error_kind"] != "none"
        {
            return Ok(("rejected", "internalError"));
        }
        return Ok(("accepted", "none"));
    }
    if failure != "brokerLost" {
        return Ok(("rejected", "internalError"));
    }
    let error = if handed_off && !response_end && side_effect == "app_mutation" {
        "action.outcomeUnknown"
    } else {
        "sessionExpired"
    };
    if fixture["expected_handoff"] != handoff || fixture["expected_error_kind"] != error {
        return Ok(("rejected", "internalError"));
    }
    Ok(("rejected", "brokerLost"))
}

fn verify_catalog_projection(fixture: &Value) -> AnyResult<(&'static str, &'static str)> {
    let id = fixture["id"]
        .as_str()
        .ok_or("catalog projection id missing")?;
    let input = &fixture["input"];
    let session = input["session"]
        .as_str()
        .ok_or("projection session missing")?;
    let target = input["target"]
        .as_str()
        .ok_or("projection target missing")?;
    let expected = match id {
        "catalog-complete-nonempty-projects-show"
            if input["item_count"].as_u64().is_some_and(|count| count > 0)
                && input["truncated"] == false
                && input["capability"] == "smoke.ready"
                && input["declaration_revision"] == 1
                && input["next_action_id"] == "catalog.show" =>
        {
            vec![
                "/prefix/bin/apppilotkit".to_owned(),
                "catalog".to_owned(),
                "show".to_owned(),
                "--capability".to_owned(),
                "smoke.ready".to_owned(),
                "--declaration-revision".to_owned(),
                "1".to_owned(),
                format!("--session={session}"),
                format!("--target={target}"),
                "--output".to_owned(),
                "json".to_owned(),
                "--non-interactive".to_owned(),
            ]
        }
        "catalog-truncated-projects-continuation"
            if input["truncated"] == true && input["next_action_id"] == "catalog.list.continue" =>
        {
            let cursor = input["cursor"]
                .as_str()
                .ok_or("projection cursor missing")?;
            vec![
                "/prefix/bin/apppilotkit".to_owned(),
                "catalog".to_owned(),
                "list".to_owned(),
                format!("--session={session}"),
                format!("--target={target}"),
                "--cursor".to_owned(),
                cursor.to_owned(),
                "--output".to_owned(),
                "json".to_owned(),
                "--non-interactive".to_owned(),
            ]
        }
        "catalog-complete-empty-projects-list-selector"
            if input["item_count"] == 0
                && input["truncated"] == false
                && input["next_action_id"] == "catalog.list" =>
        {
            vec![
                "/prefix/bin/apppilotkit".to_owned(),
                "catalog".to_owned(),
                "list".to_owned(),
                format!("--session={session}"),
                format!("--target={target}"),
                "--output".to_owned(),
                "json".to_owned(),
                "--non-interactive".to_owned(),
            ]
        }
        _ => return Ok(("rejected", "internalError")),
    };
    let actual = input["argv"]
        .as_array()
        .ok_or("projection argv missing")?
        .iter()
        .map(|item| {
            item.as_str()
                .ok_or("projection argv item is not a string")
                .map(str::to_owned)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(if actual == expected {
        ("accepted", "none")
    } else {
        ("rejected", "bindingMismatch")
    })
}

fn verify_evidence_completeness(fixture: &Value) -> AnyResult<(&'static str, &'static str)> {
    let input = &fixture["input"];
    let installed = string_set(&input["installed_names"])?;
    let required_installed = BTreeSet::from([
        "apppilotkit",
        "apppilotkit-broker",
        "apppilotkit-target-prepare",
    ]);
    let surfaces = string_set(&input["surface_names"])?;
    let required_surfaces = BTreeSet::from([
        "activity_extras",
        "argv",
        "artifacts",
        "diagnostics",
        "environment",
        "machine_result",
        "next_actions",
        "product_logs",
        "production_build_artifact",
        "release_build_artifact",
        "smoke_host_build_artifact",
        "stderr",
        "stdout",
    ]);
    let fixed = input["fixed_canary_digest"]
        .as_str()
        .ok_or("fixed canary digest missing")?;
    let execution = input["execution_canary_digest"]
        .as_str()
        .ok_or("execution canary digest missing")?;
    let build_counts = input["build_artifact_byte_counts"].as_object();
    let build_captures_nonempty = [
        "smoke_host_build_artifact",
        "production_build_artifact",
        "release_build_artifact",
    ]
    .into_iter()
    .all(|name| {
        build_counts
            .and_then(|counts| counts.get(name))
            .and_then(Value::as_u64)
            .is_some_and(|bytes| bytes > 0)
    });
    if installed != required_installed
        || surfaces != required_surfaces
        || fixed == execution
        || input["all_surfaces_have_scanner"] != true
        || input["all_surfaces_have_artifact_hash"] != true
        || !build_captures_nonempty
        || input["complete"] != true
    {
        return Ok(("rejected", "malformed"));
    }
    Ok(("accepted", "none"))
}

fn string_set(value: &Value) -> AnyResult<BTreeSet<&str>> {
    value
        .as_array()
        .ok_or_else(|| Box::<dyn Error>::from("string set is not an array"))?
        .iter()
        .map(|item| {
            item.as_str()
                .ok_or_else(|| Box::<dyn Error>::from("string set member is not a string"))
        })
        .collect()
}

fn verify_noise_finished_replay(
    fixture_path: &Path,
    fixture: &Value,
) -> AnyResult<(&'static str, &'static str)> {
    if fixture["input"]["repeat_count"] != 2
        || fixture["input"]["replay_timing"] != "immediate_at_expected_nonce"
    {
        return Ok(("rejected", "internalError"));
    }
    let relative = fixture["input"]["session_vector"]
        .as_str()
        .ok_or("session vector path missing")?;
    let contract_root = fixture_path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or("fixture is not under reference/fixtures")?;
    let vector: Value = serde_json::from_slice(&fs::read(contract_root.join(relative))?)?;
    let prologue = hex::decode(
        vector["canonical_input"]["prologue_cbor_hex"]
            .as_str()
            .ok_or("session prologue missing")?,
    )?;
    let material = &vector["test_only_material"]["material"];
    let pbs: [u8; 32] = hex::decode(
        material["process_bootstrap_secret_hex"]
            .as_str()
            .ok_or("session PSK missing")?,
    )?
    .try_into()
    .map_err(|_| "session PSK is not 32 bytes")?;
    let target_key = hex::decode(
        material["target_ephemeral_private_hex"]
            .as_str()
            .ok_or("Target ephemeral missing")?,
    )?;
    let broker_key = hex::decode(
        material["broker_ephemeral_private_hex"]
            .as_str()
            .ok_or("Broker ephemeral missing")?,
    )?;
    let params: NoiseParams = vector["canonical_input"]["noise_name"]
        .as_str()
        .ok_or("Noise name missing")?
        .parse()?;
    let mut target = Builder::new(params.clone())
        .prologue(&prologue)?
        .psk(0, &pbs)?
        .fixed_ephemeral_key_for_testing_only(&target_key)
        .build_initiator()?;
    let mut broker = Builder::new(params)
        .prologue(&prologue)?
        .psk(0, &pbs)?
        .fixed_ephemeral_key_for_testing_only(&broker_key)
        .build_responder()?;
    let m1 = unframe_u16_hex(
        vector["expected"]["m1_outer_hex"]
            .as_str()
            .ok_or("session m1 outer missing")?,
    )?;
    let m2 = unframe_u16_hex(
        vector["expected"]["m2_outer_hex"]
            .as_str()
            .ok_or("session m2 outer missing")?,
    )?;
    let finished = unframe_u16_hex(
        vector["expected"]["target_finished_outer_hex"]
            .as_str()
            .ok_or("Target Finished outer missing")?,
    )?;
    let mut buffer = vec![0_u8; 65_535];
    let mut generated = vec![0_u8; 65_535];
    let generated_len = target.write_message(&[], &mut generated)?;
    if generated[..generated_len] != m1 || broker.read_message(&m1, &mut buffer)? != 0 {
        return Ok(("rejected", "internalError"));
    }
    let generated_len = broker.write_message(&[], &mut generated)?;
    if generated[..generated_len] != m2 || target.read_message(&m2, &mut buffer)? != 0 {
        return Ok(("rejected", "internalError"));
    }
    let mut broker_transport = broker.into_transport_mode()?;
    broker_transport.read_message(&finished, &mut buffer)?;
    if broker_transport
        .read_message(&finished, &mut buffer)
        .is_err()
    {
        Ok(("rejected", "authenticationFailed"))
    } else {
        Ok(("accepted", "none"))
    }
}

fn unframe_u16_hex(encoded: &str) -> AnyResult<Vec<u8>> {
    let bytes = hex::decode(encoded)?;
    if bytes.len() < 2 {
        return Err("outer ciphertext prefix missing".into());
    }
    let declared = usize::from(u16::from_be_bytes([bytes[0], bytes[1]]));
    if declared == 0 || bytes.len() != declared + 2 {
        return Err("outer ciphertext length mismatch".into());
    }
    Ok(bytes[2..].to_vec())
}

fn verify_secret_surface_scanner(fixture: &Value) -> AnyResult<(&'static str, &'static str)> {
    let input = &fixture["input"];
    if input["scanner"] != "apppilotkit-reference-byte-scanner"
        || input["scanner_version"] != "1.0"
        || input["operation"] != "literal-byte-subsequence-count"
        || input["complete"] != true
        || input["artifact_identity"].as_str().is_none()
        || input["artifact_path"].as_str().is_none()
    {
        return Ok(("rejected", "internalError"));
    }
    let fixed_canary = input["fixed_canary_utf8"]
        .as_str()
        .ok_or("scanner fixed canary missing")?
        .as_bytes();
    let execution_canary = input["execution_canary_utf8"]
        .as_str()
        .ok_or("scanner execution canary missing")?
        .as_bytes();
    if fixed_canary.is_empty() || execution_canary.is_empty() || fixed_canary == execution_canary {
        return Ok(("rejected", "internalError"));
    }
    let artifact = hex::decode(
        input["artifact_hex"]
            .as_str()
            .ok_or("scanner artifact bytes missing")?,
    )?;
    let declared_bytes = input["declared_byte_count"]
        .as_u64()
        .ok_or("scanner byte count missing")?;
    let declared_fixed_matches = input["declared_fixed_match_count"]
        .as_u64()
        .ok_or("scanner fixed match count missing")?;
    let declared_execution_matches = input["declared_execution_match_count"]
        .as_u64()
        .ok_or("scanner execution match count missing")?;
    let declared_sha = input["artifact_sha256"]
        .as_str()
        .ok_or("scanner artifact hash missing")?;
    let actual_sha = format!("sha256:{}", sha256_hex(&artifact));
    let fixed_matches = artifact
        .windows(fixed_canary.len())
        .filter(|window| *window == fixed_canary)
        .count() as u64;
    let execution_matches = artifact
        .windows(execution_canary.len())
        .filter(|window| *window == execution_canary)
        .count() as u64;
    if declared_bytes != artifact.len() as u64
        || declared_fixed_matches != fixed_matches
        || declared_execution_matches != execution_matches
        || declared_sha != actual_sha
        || fixed_matches != 0
        || execution_matches != 0
    {
        return Ok(("rejected", "internalError"));
    }
    Ok(("accepted", "none"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

fn verify_lifecycle(fixture: &Value) -> AnyResult<(&'static str, &'static str)> {
    let input = &fixture["input"];
    let issued = input["issued_at_unix_ms"]
        .as_u64()
        .ok_or("lifecycle issued timestamp missing")?;
    let expires = input["expires_at_unix_ms"]
        .as_u64()
        .ok_or("lifecycle expiry timestamp missing")?;
    if issued.checked_add(30_000) != Some(expires) {
        return Ok(("rejected", "internalError"));
    }
    let events = input["events"]
        .as_array()
        .ok_or("lifecycle events missing")?;
    for terminal_event in events
        .iter()
        .filter(|event| matches!(event["operation"].as_str(), Some("close" | "mark_stale")))
    {
        let at = terminal_event["at_unix_ms"]
            .as_u64()
            .ok_or("terminal event time missing")?;
        if let Some(competing) = events.iter().find(|event| {
            event["at_unix_ms"].as_u64() == Some(at)
                && !matches!(event["operation"].as_str(), Some("close" | "mark_stale"))
        }) {
            return Ok((
                "rejected",
                if competing["dispatch_boundary_crossed"] == true {
                    "peerClosed"
                } else {
                    "stale"
                },
            ));
        }
    }
    let mut reference_consumed = false;
    let mut opened_session = false;
    let mut terminal = false;
    let mut previous_at = issued;
    for event in events {
        let at = event["at_unix_ms"]
            .as_u64()
            .ok_or("lifecycle event time missing")?;
        let operation = event["operation"]
            .as_str()
            .ok_or("lifecycle operation missing")?;
        if at < previous_at {
            return Ok(("rejected", "sequenceViolation"));
        }
        previous_at = at;
        if terminal {
            return Ok(("rejected", "stale"));
        }
        match operation {
            "target_only_open" => {
                if at >= expires || reference_consumed {
                    return Ok(("rejected", "stale"));
                }
                reference_consumed = true;
                opened_session = true;
            }
            "explicit_session_reuse" if opened_session => {}
            "explicit_session_reuse" | "new_session_without_prepare" => {
                return Ok(("rejected", "stale"));
            }
            "exchange" | "heartbeat" if opened_session => {}
            "exchange" | "heartbeat" => return Ok(("rejected", "stale")),
            "close" | "mark_stale" => terminal = true,
            _ => return Ok(("rejected", "internalError")),
        }
    }
    Ok(("accepted", "none"))
}

fn verify_record_reassembly(fixture: &Value) -> AnyResult<(&'static str, &'static str)> {
    if fixture["input"]["owner"].as_str().is_some()
        && fixture["input"]["owner"] != fixture["input"]["incoming_application_role"]
    {
        return Ok(("rejected", "sequenceViolation"));
    }
    let records = fixture["input"]["records_hex"]
        .as_array()
        .ok_or("record list missing")?;
    let max = fixture["input"]["max_message_bytes"]
        .as_u64()
        .ok_or("message cap missing")?;
    let mut active: Option<(u8, u64, u64)> = None;
    let mut completed = false;
    let mut message = Vec::new();
    for encoded in records {
        if completed {
            return Ok(("rejected", "malformed"));
        }
        let raw = hex::decode(encoded.as_str().ok_or("record hex is not a string")?)?;
        if raw.len() < 12 {
            return Ok(("rejected", "malformed"));
        }
        let kind = raw[0];
        let flags = raw[1];
        let reserved = u16::from_be_bytes([raw[2], raw[3]]);
        let total = u64::from(u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]));
        let offset = u64::from(u32::from_be_bytes([raw[8], raw[9], raw[10], raw[11]]));
        let data_len = (raw.len() - 12) as u64;
        if !(1..=4).contains(&kind) || flags & !3 != 0 || reserved != 0 || data_len > 65_507 {
            return Ok(("rejected", "sequenceViolation"));
        }
        let start = flags & 1 != 0;
        let end = flags & 2 != 0;
        if start {
            if active.is_some() || offset != 0 || total == 0 {
                return Ok(("rejected", "sequenceViolation"));
            }
            if total > max {
                return Ok(("rejected", "oversize"));
            }
            active = Some((kind, total, 0));
            message.clear();
        } else if total != 0 || active.is_none() {
            return Ok(("rejected", "sequenceViolation"));
        }
        let (active_kind, expected_total, expected_offset) =
            active.ok_or("active record missing")?;
        if kind != active_kind || offset != expected_offset {
            return Ok(("rejected", "sequenceViolation"));
        }
        let next = expected_offset
            .checked_add(data_len)
            .ok_or("record offset overflow")?;
        if next > expected_total {
            return Ok((
                "rejected",
                if end {
                    "malformed"
                } else {
                    "sequenceViolation"
                },
            ));
        }
        message.extend_from_slice(&raw[12..]);
        if (end && next != expected_total) || (!end && next >= expected_total) {
            return Ok(("rejected", "sequenceViolation"));
        }
        if end {
            if active_kind == 4 && validate_close_record_payload(&message).is_err() {
                return Ok(("rejected", "malformed"));
            }
            active = None;
            completed = true;
        } else {
            active = Some((active_kind, expected_total, next));
        }
    }
    if active.is_some() || !completed {
        Ok(("rejected", "malformed"))
    } else {
        Ok(("accepted", "none"))
    }
}

fn validate_close_record_payload(bytes: &[u8]) -> AnyResult<()> {
    validate_deterministic_cbor(bytes)
        .map_err(|error| format!("close record deterministic CBOR: {error}"))?;
    let mut decoder = Decoder::new(bytes);
    if decoder.map()? != Some(3) || decoder.u8()? != 0 || decoder.u8()? != 1 || decoder.u8()? != 1 {
        return Err("close record version/map mismatch".into());
    }
    let reason = decoder.u8()?;
    if reason > 13 || decoder.u8()? != 2 {
        return Err("close record reason/key mismatch".into());
    }
    let handoff = decoder.u8()?;
    if handoff > 1 || decoder.position() != bytes.len() {
        return Err("close record handoff/trailing mismatch".into());
    }
    Ok(())
}

fn verify_outer_frame(fixture: &Value) -> AnyResult<(&'static str, &'static str)> {
    if let Some(declared) = fixture["input"]["declared_ciphertext_length"].as_u64() {
        return Ok(if declared > 65_535 {
            ("rejected", "oversize")
        } else {
            ("accepted", "none")
        });
    }
    let raw = hex::decode(
        fixture["input"]["raw_outer_hex"]
            .as_str()
            .ok_or("raw outer frame hex missing")?,
    )?;
    let elapsed = fixture["input"]["elapsed_ms"].as_u64().unwrap_or(0);
    if raw.len() < 2 {
        return Ok((
            "rejected",
            if elapsed >= 2_000 {
                "timeout"
            } else {
                "malformed"
            },
        ));
    }
    let declared = usize::from(u16::from_be_bytes([raw[0], raw[1]]));
    if declared == 0 {
        return Ok(("rejected", "malformed"));
    }
    if raw.len() != declared + 2 {
        return Ok((
            "rejected",
            if raw.len() < declared + 2 && elapsed >= 2_000 {
                "timeout"
            } else {
                "malformed"
            },
        ));
    }
    Ok(("accepted", "none"))
}

fn verify_deterministic_cbor_fixture(fixture: &Value) -> AnyResult<(&'static str, &'static str)> {
    let raw = hex::decode(
        fixture["input"]["raw_hex"]
            .as_str()
            .ok_or("raw CBOR hex missing")?,
    )?;
    match validate_deterministic_cbor(&raw) {
        Ok(()) => Ok(("accepted", "none")),
        Err(_) => Ok(("rejected", "malformed")),
    }
}

pub fn validate_deterministic_cbor(bytes: &[u8]) -> Result<(), String> {
    let mut offset = 0;
    parse_cbor_item(bytes, &mut offset, 0)?;
    if offset != bytes.len() {
        return Err("trailing CBOR bytes".to_owned());
    }
    Ok(())
}

pub fn validate_target_reference_roundtrip(reference: &str) -> AnyResult<()> {
    let encoded = reference
        .strip_prefix("target_")
        .ok_or("Target Reference prefix missing")?;
    if canonical_base64url(encoded, Some(32))? != ("accepted", "none") {
        return Err("Target Reference is not canonical base64url of 32 bytes".into());
    }
    let token = URL_SAFE_NO_PAD.decode(encoded)?;
    let mut cbor = vec![0x58, 0x20];
    cbor.extend_from_slice(&token);
    validate_deterministic_cbor(&cbor).map_err(|error| format!("Target token CBOR: {error}"))?;
    if cbor.get(2..) != Some(token.as_slice())
        || format!("target_{}", URL_SAFE_NO_PAD.encode(&cbor[2..])) != reference
    {
        return Err("Target Reference JSON/CBOR round-trip mismatch".into());
    }
    Ok(())
}

pub fn validate_json_semantics(value: &Value) -> AnyResult<()> {
    match value {
        Value::Array(items) => {
            for item in items {
                validate_json_semantics(item)?;
            }
        }
        Value::Object(object) => {
            for (key, item) in object {
                if let Some(text) = item.as_str() {
                    match key.as_str() {
                        "target" => validate_target_reference_roundtrip(text)?,
                        "request_id" => require_canonical_base64url(text, 16, key)?,
                        "message_base64url" => require_canonical_base64url_any(text, key)?,
                        "message" if text.len() > 256 => {
                            return Err("message exceeds 256 UTF-8 bytes".into());
                        }
                        "app_artifact" | "prefix" | "path" | "artifact_path"
                            if text.len() > 4_096 =>
                        {
                            return Err(format!("{key} exceeds 4096 UTF-8 bytes").into());
                        }
                        _ => {}
                    }
                }
                validate_json_semantics(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn validate_evidence_semantics(contract_root: &Path, value: &Value) -> AnyResult<()> {
    validate_json_semantics(value)?;
    validate_rfc3339(
        value["recorded_at"]
            .as_str()
            .ok_or("evidence recorded_at missing")?,
    )?;
    let installed = value["installed"]["executables"]
        .as_array()
        .ok_or("installed executable identities missing")?;
    let installed_names = installed
        .iter()
        .filter_map(|identity| identity["name"].as_str())
        .collect::<BTreeSet<_>>();
    if installed_names
        != BTreeSet::from([
            "apppilotkit",
            "apppilotkit-broker",
            "apppilotkit-target-prepare",
        ])
    {
        return Err("installed executable identity set is incomplete".into());
    }
    let prefix = value["installed"]["prefix"]
        .as_str()
        .ok_or("installed prefix missing")?;
    let expected_paths = [
        ("apppilotkit", format!("{prefix}/bin/apppilotkit")),
        (
            "apppilotkit-broker",
            format!("{prefix}/libexec/apppilotkit-broker"),
        ),
        (
            "apppilotkit-target-prepare",
            format!("{prefix}/libexec/apppilotkit-target-prepare"),
        ),
    ];
    let mut package_bytes = Vec::new();
    for (name, expected_path) in expected_paths {
        let identity = installed
            .iter()
            .find(|identity| identity["name"] == name)
            .ok_or("installed identity missing")?;
        if identity["path"] != expected_path {
            return Err(format!("installed {name} path is not canonical").into());
        }
        let bytes = decode_capture_bytes(&identity["bytes_base64url"], "installed executable")?;
        require_digest(&bytes, &identity["sha256"], "installed executable")?;
        package_bytes.extend_from_slice(name.as_bytes());
        package_bytes.push(0);
        package_bytes.extend_from_slice(
            identity["sha256"]
                .as_str()
                .ok_or("installed digest missing")?
                .as_bytes(),
        );
        package_bytes.push(b'\n');
    }
    require_digest(
        &package_bytes,
        &value["installed"]["package_sha256"],
        "installed package root",
    )?;
    let platform = value["platform"]
        .as_str()
        .ok_or("evidence platform missing")?;
    let expected_artifact_encoding = match platform {
        "ios-simulator" => IOS_APP_TREE_ENCODING,
        "android-emulator" => RAW_FILE_ENCODING,
        _ => return Err("evidence platform is unsupported".into()),
    };
    if value["app"]["artifact_encoding"] != expected_artifact_encoding {
        return Err("app artifact encoding differs from platform".into());
    }
    let app_bytes = decode_artifact_bytes(
        &value["app"]["artifact_bytes_base64url"],
        "app artifact",
        expected_artifact_encoding,
    )?;
    require_digest(&app_bytes, &value["app"]["artifact_sha256"], "app artifact")?;
    if platform == "ios-simulator" {
        parse_ios_app_tree(
            &mut Cursor::new(&app_bytes),
            value["app"]["id"].as_str().ok_or("app id missing")?,
            Some(value["app"]["build"].as_str().ok_or("app build missing")?),
        )?;
    }
    let argv0 = value["command"]["redacted_argv"][0]
        .as_str()
        .ok_or("command argv executable missing")?;
    if argv0 != format!("{prefix}/bin/apppilotkit") {
        return Err("invoked executable is not the installed CLI".into());
    }
    let stdout = validate_capture(&value["command"]["retained_stdout"])?;
    require_digest(
        &stdout,
        &value["terminal"]["machine_result_sha256"],
        "Machine Result",
    )?;
    if !stdout.ends_with(b"\n") {
        return Err("retained Machine Result is not newline terminated".into());
    }
    let machine = parse_strict_json(&stdout)?;
    validate_machine_result(contract_root, &machine)?;
    let installed_cli_version = installed
        .iter()
        .find(|identity| identity["name"] == "apppilotkit")
        .and_then(|identity| identity["version"].as_str())
        .ok_or("installed CLI version missing")?;
    if machine["cli_version"].as_str() != Some(installed_cli_version) {
        return Err("Machine Result CLI version differs from installed CLI identity".into());
    }
    let machine_capabilities = machine["data"]["capabilities"]
        .as_array()
        .ok_or("Machine Result catalog capabilities missing")?;
    let machine_actions = machine["next_actions"]
        .as_array()
        .ok_or("Machine Result Next Actions missing")?;
    if machine["schema_version"] != "1.0"
        || machine["status"] != value["terminal"]["status"]
        || machine["command"] != serde_json::json!(["catalog", "list"])
        || machine["side_effect"] != "read_only"
        || machine["retry_safety"] != "safe"
        || machine["data"]["catalog"] != value["terminal"]["catalog"]
        || machine_capabilities.len() != 1
        || machine_capabilities[0] != value["terminal"]["smoke_ready_declaration"]
        || machine["disclosure"]["truncated"] != false
        || machine["disclosure"]["returned_items"] != 1
        || machine["artifacts"] != serde_json::json!([])
        || machine_actions.len() != 1
        || machine_actions[0]["id"] != value["terminal"]["next_action"]["kind"]
        || machine_actions[0]["argv"] != value["terminal"]["next_action"]["redacted_argv"]
        || machine_actions[0]["side_effect"] != "read_only"
        || machine_actions[0]["retry_safety"] != "safe"
    {
        return Err("retained Machine Result content is not the declared catalog verdict".into());
    }
    let redactions = value["command"]["stdout_redactions"]
        .as_array()
        .ok_or("Machine Result redactions missing")?;
    if redactions.len() != 2
        || machine.pointer("/next_actions/0/argv/7")
            != Some(&Value::String("--session=<redacted>".to_owned()))
        || machine.pointer("/next_actions/0/argv/8")
            != Some(&Value::String("--target=<redacted>".to_owned()))
        || redactions[0]["json_pointer"] != "/next_actions/0/argv/7"
        || redactions[0]["original_sha256"] != value["session"]["id_digest"]
        || redactions[1]["json_pointer"] != "/next_actions/0/argv/8"
        || redactions[1]["original_sha256"] != value["target"]["reference_digest"]
    {
        return Err("Machine Result selector redaction binding mismatch".into());
    }
    if stdout.windows(7).any(|window| window == b"target_")
        || stdout.windows(8).any(|window| window == b"session_")
    {
        return Err(
            "retained Machine Result contains plaintext Target Reference/session id".into(),
        );
    }
    if value["target"]["process_generation"] != value["terminal"]["catalog"]["generation"] {
        return Err("Target and catalog generation differ".into());
    }
    if value["terminal"]["next_action"]["target_reference_digest"]
        != value["target"]["reference_digest"]
        || value["terminal"]["next_action"]["session_id_digest"] != value["session"]["id_digest"]
        || value["terminal"]["smoke_ready_declaration"]["id"] != "smoke.ready"
    {
        return Err("terminal selector/session/Target binding mismatch".into());
    }
    let surfaces = value["secret_surface"]["surfaces"]
        .as_array()
        .ok_or("scan surfaces missing")?;
    let surface_bytes = |name: &str| -> AnyResult<Vec<u8>> {
        let surface = surfaces
            .iter()
            .find(|surface| surface["name"] == name)
            .ok_or_else(|| Box::<dyn Error>::from(format!("scan surface {name} missing")))?;
        validate_capture(&surface["capture"])
    };
    for name in [
        "smoke_host_build_artifact",
        "production_build_artifact",
        "release_build_artifact",
    ] {
        let surface = surfaces
            .iter()
            .find(|surface| surface["name"] == name)
            .ok_or_else(|| Box::<dyn Error>::from(format!("scan surface {name} missing")))?;
        let identity = &surface["artifact_identity"];
        if identity["artifact_encoding"] != expected_artifact_encoding {
            return Err(format!("{name} artifact encoding differs from platform").into());
        }
        if identity["artifact_sha256"] != surface["capture"]["sha256"] {
            return Err(format!("{name} capture differs from its build artifact identity").into());
        }
        let captured_build =
            validate_artifact_capture(&surface["capture"], expected_artifact_encoding)?;
        if platform == "ios-simulator" {
            parse_ios_app_tree(
                &mut Cursor::new(&captured_build),
                identity["app_id"]
                    .as_str()
                    .ok_or("build artifact app_id missing")?,
                Some(
                    identity["build"]
                        .as_str()
                        .ok_or("build artifact build missing")?,
                ),
            )?;
        }
        if name == "smoke_host_build_artifact"
            && (identity["app_id"] != value["app"]["id"]
                || identity["build"] != value["app"]["build"]
                || identity["artifact_sha256"] != value["app"]["artifact_sha256"]
                || captured_build != app_bytes)
        {
            return Err("Smoke Host scan is not bound to the launched app artifact".into());
        }
        if name != "smoke_host_build_artifact" && captured_build == app_bytes {
            return Err(format!("{name} reuses the Smoke Host app artifact").into());
        }
    }
    let next_actions_bytes = serde_json::to_vec(&machine["next_actions"])?;
    if surface_bytes("stdout")? != stdout
        || surface_bytes("machine_result")? != stdout
        || surface_bytes("next_actions")? != next_actions_bytes
        || !surface_bytes("stderr")?.is_empty()
        || value["command"]["stderr_sha256"] != format!("sha256:{}", sha256_hex(&[]))
    {
        return Err("terminal capture surfaces are not bound to retained bytes".into());
    }
    let concurrent = value["concurrent_sessions"]
        .as_array()
        .ok_or("concurrent session evidence missing")?;
    if concurrent.len() != 2 {
        return Err("two-session isolation evidence is inconsistent".into());
    }
    let primary = validate_session_evidence(&value["session"])?;
    let session_a = validate_session_evidence(&concurrent[0])?;
    let session_b = validate_session_evidence(&concurrent[1])?;
    if session_a.id_digest == session_b.id_digest
        || session_a.handshake_hash == session_b.handshake_hash
        || session_a.request_sha256 == session_b.request_sha256
        || session_a.response_sha256 == session_b.response_sha256
        || session_a.runtime_instance_digest == session_b.runtime_instance_digest
        || !concurrent
            .iter()
            .any(|candidate| candidate == &value["session"])
        || primary.id_digest != value["session"]["id_digest"]
        || value["session_isolation"]["fresh_handshakes"] != true
        || value["session_isolation"]["fresh_session_ids"] != true
        || value["session_isolation"]["close_a_b_remained_open"] != true
        || value["session_isolation"]["idle_a_b_remained_open"] != true
        || value["session_isolation"]["auth_a_b_remained_open"] != true
        || value["session_isolation"]["lease_loss_staled_both"] != true
    {
        return Err("two-session isolation evidence is inconsistent".into());
    }
    let secret = &value["secret_surface"];
    let fixed = secret["fixed_canary_digest"]
        .as_str()
        .ok_or("fixed canary digest missing")?;
    let execution = secret["execution_canary_digest"]
        .as_str()
        .ok_or("execution canary digest missing")?;
    if fixed == execution {
        return Err("fixed and execution-unique canaries are not distinct".into());
    }
    let fixed_bytes = decode_capture_bytes(&secret["fixed_canary_base64url"], "fixed canary")?;
    let execution_bytes =
        decode_capture_bytes(&secret["execution_canary_base64url"], "execution canary")?;
    require_digest(&fixed_bytes, &secret["fixed_canary_digest"], "fixed canary")?;
    require_digest(
        &execution_bytes,
        &secret["execution_canary_digest"],
        "execution canary",
    )?;
    if fixed_bytes == execution_bytes {
        return Err("fixed and execution canary bytes are identical".into());
    }
    let names = surfaces
        .iter()
        .filter_map(|surface| surface["name"].as_str())
        .collect::<BTreeSet<_>>();
    let required = BTreeSet::from([
        "activity_extras",
        "argv",
        "artifacts",
        "diagnostics",
        "environment",
        "machine_result",
        "next_actions",
        "product_logs",
        "production_build_artifact",
        "release_build_artifact",
        "smoke_host_build_artifact",
        "stderr",
        "stdout",
    ]);
    if names != required {
        return Err("scan surface inventory is incomplete".into());
    }
    for surface in surfaces {
        if surface["scanner"] != "apppilotkit-reference-byte-scanner"
            || surface["scanner_version"] != "1.0"
            || surface["operation"] != "literal-byte-subsequence-count"
            || surface["complete"] != true
        {
            return Err("scan surface metadata is not canonical/complete".into());
        }
        let bytes = validate_capture(&surface["capture"])?;
        let fixed_matches = literal_match_count(&bytes, &fixed_bytes);
        let execution_matches = literal_match_count(&bytes, &execution_bytes);
        if surface["fixed_canary_match_count"].as_u64() != Some(fixed_matches)
            || surface["execution_canary_match_count"].as_u64() != Some(execution_matches)
            || fixed_matches != 0
            || execution_matches != 0
        {
            return Err("scan surface canary count differs from retained bytes".into());
        }
        if surface["name"]
            .as_str()
            .is_some_and(|name| name.ends_with("_build_artifact"))
            && bytes.is_empty()
        {
            return Err("build artifact capture is empty".into());
        }
    }
    Ok(())
}

struct SessionEvidence<'a> {
    id_digest: &'a str,
    handshake_hash: &'a str,
    request_sha256: &'a str,
    response_sha256: &'a str,
    runtime_instance_digest: &'a str,
}

fn validate_session_evidence(value: &Value) -> AnyResult<SessionEvidence<'_>> {
    let id_digest = value["id_digest"]
        .as_str()
        .ok_or("session id digest missing")?;
    let handshake_hash = value["noise_handshake_hash_hex"]
        .as_str()
        .ok_or("session handshake hash missing")?;
    let request_sha256 = value["request"]["sha256"]
        .as_str()
        .ok_or("session request digest missing")?;
    let response_sha256 = value["response"]["sha256"]
        .as_str()
        .ok_or("session response digest missing")?;
    let runtime_instance_digest = value["runtime"]["instance_digest"]
        .as_str()
        .ok_or("session runtime instance digest missing")?;
    if value["request"]["session_id_digest"].as_str() != Some(id_digest)
        || value["response"]["session_id_digest"].as_str() != Some(id_digest)
        || value["runtime"]["session_id_digest"].as_str() != Some(id_digest)
        || value["runtime"]["request_sha256"].as_str() != Some(request_sha256)
        || value["runtime"]["response_sha256"].as_str() != Some(response_sha256)
    {
        return Err("session request/response/runtime facts are not bound".into());
    }
    Ok(SessionEvidence {
        id_digest,
        handshake_hash,
        request_sha256,
        response_sha256,
        runtime_instance_digest,
    })
}

fn validate_machine_result(contract_root: &Path, machine: &Value) -> AnyResult<()> {
    let schema_root = contract_root.join("../../../cli/contracts/v1/schema");
    let names = [
        "artifact.schema.json",
        "catalog.schema.json",
        "disclosure.schema.json",
        "error.schema.json",
        "machine-result.schema.json",
        "next-action.schema.json",
    ];
    let dependency_lock: Value =
        serde_json::from_slice(&fs::read(contract_root.join("dependencies.lock.json"))?)?;
    let pins = dependency_lock["frozen_cli_schemas"]
        .as_array()
        .ok_or("frozen CLI schema pins missing")?;
    if pins.len() != names.len() {
        return Err("frozen CLI schema pin inventory is not exact".into());
    }
    let mut registry = jsonschema::Registry::new().retriever(RejectExternal);
    for (name, pin) in names.into_iter().zip(pins) {
        let pin = pin
            .as_object()
            .ok_or("frozen CLI schema pin is not an object")?;
        if pin.keys().map(String::as_str).collect::<BTreeSet<_>>()
            != BTreeSet::from(["path", "sha256"])
            || pin.get("path").and_then(Value::as_str) != Some(name)
        {
            return Err("frozen CLI schema pin collection/order is not exact".into());
        }
        let bytes = fs::read(schema_root.join(name))?;
        let actual_sha256 = sha256_hex(&bytes);
        if pin.get("sha256").and_then(Value::as_str) != Some(actual_sha256.as_str()) {
            return Err(format!("frozen CLI schema hash mismatch for {name}").into());
        }
        let schema: Value = serde_json::from_slice(&bytes)?;
        jsonschema::draft202012::meta::validate(&schema).map_err(|error| error.to_string())?;
        let id = schema["$id"]
            .as_str()
            .ok_or_else(|| format!("frozen CLI schema {name} has no $id"))?
            .to_owned();
        registry = registry.add(id, schema)?;
    }
    let registry = registry.prepare()?;
    validate_registered_schema(
        &registry,
        "https://apppilotkit.dev/cli/v1/machine-result.schema.json",
        machine,
    )?;
    if machine["command"] != serde_json::json!(["catalog", "list"]) {
        return Err("retained Machine Result is not catalog list".into());
    }
    validate_registered_schema(
        &registry,
        "https://apppilotkit.dev/cli/v1/catalog.schema.json#/$defs/list",
        &machine["data"],
    )
}

fn validate_registered_schema(
    registry: &jsonschema::Registry<'_>,
    id: &str,
    instance: &Value,
) -> AnyResult<()> {
    let validator = jsonschema::draft202012::options()
        .with_registry(registry)
        .with_retriever(RejectExternal)
        .build(&serde_json::json!({"$ref":id}))
        .map_err(|error| error.to_string())?;
    validator
        .validate(instance)
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn decode_capture_bytes(value: &Value, field: &str) -> AnyResult<Vec<u8>> {
    let encoded = value
        .as_str()
        .ok_or_else(|| Box::<dyn Error>::from(format!("{field} bytes missing")))?;
    let bytes = URL_SAFE_NO_PAD.decode(encoded)?;
    if URL_SAFE_NO_PAD.encode(&bytes) != encoded {
        return Err(format!("{field} bytes are not canonical base64url").into());
    }
    Ok(bytes)
}

fn decode_artifact_bytes(value: &Value, field: &str, encoding: &str) -> AnyResult<Vec<u8>> {
    let encoded = value
        .as_str()
        .ok_or_else(|| Box::<dyn Error>::from(format!("{field} bytes missing")))?;
    let max_bytes = artifact_max_bytes(encoding)?;
    validate_artifact_encoded_len(encoded.len() as u64, encoding)?;
    let bytes = decode_capture_bytes(value, field)?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!("{field} exceeds the platform artifact byte cap").into());
    }
    Ok(bytes)
}

pub fn validate_artifact_encoded_len(encoded_len: u64, encoding: &str) -> AnyResult<()> {
    let max_bytes = artifact_max_bytes(encoding)?;
    if encoded_len > base64url_unpadded_len(max_bytes) {
        return Err("artifact base64url exceeds the platform artifact byte cap".into());
    }
    Ok(())
}

fn artifact_max_bytes(encoding: &str) -> AnyResult<u64> {
    Ok(match encoding {
        IOS_APP_TREE_ENCODING => IOS_APP_TREE_MAX_CANONICAL_BYTES,
        RAW_FILE_ENCODING => IOS_APP_TREE_MAX_TOTAL_FILE_BYTES,
        _ => return Err("artifact encoding is unsupported".into()),
    })
}

fn validate_artifact_capture(value: &Value, encoding: &str) -> AnyResult<Vec<u8>> {
    let bytes = decode_artifact_bytes(&value["bytes_base64url"], "artifact capture", encoding)?;
    require_digest(&bytes, &value["sha256"], "artifact capture")?;
    if value["byte_count"].as_u64() != Some(bytes.len() as u64) {
        return Err("artifact capture byte count mismatch".into());
    }
    Ok(bytes)
}

const fn base64url_unpadded_len(bytes: u64) -> u64 {
    (bytes / 3) * 4
        + match bytes % 3 {
            0 => 0,
            1 => 2,
            _ => 3,
        }
}

fn require_digest(bytes: &[u8], value: &Value, field: &str) -> AnyResult<()> {
    let expected = value
        .as_str()
        .ok_or_else(|| Box::<dyn Error>::from(format!("{field} digest missing")))?;
    let actual = format!("sha256:{}", sha256_hex(bytes));
    if actual != expected {
        return Err(format!("{field} digest mismatch").into());
    }
    Ok(())
}

fn validate_capture(value: &Value) -> AnyResult<Vec<u8>> {
    let bytes = decode_capture_bytes(&value["bytes_base64url"], "capture")?;
    if value["byte_count"].as_u64() != Some(bytes.len() as u64) {
        return Err("capture byte count mismatch".into());
    }
    require_digest(&bytes, &value["sha256"], "capture")?;
    if value["identity"].as_str().is_none() || value["path"].as_str().is_none() {
        return Err("capture identity/path missing".into());
    }
    Ok(bytes)
}

pub fn literal_match_count(haystack: &[u8], needle: &[u8]) -> u64 {
    if needle.is_empty() {
        return 0;
    }
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count() as u64
}

fn validate_rfc3339(value: &str) -> AnyResult<()> {
    if value.len() < 20
        || value.as_bytes().get(4) != Some(&b'-')
        || value.as_bytes().get(7) != Some(&b'-')
        || value.as_bytes().get(10) != Some(&b'T')
        || value.as_bytes().get(13) != Some(&b':')
        || value.as_bytes().get(16) != Some(&b':')
    {
        return Err("recorded_at is not RFC3339".into());
    }
    let number = |range: std::ops::Range<usize>| -> AnyResult<u32> {
        Ok(value.get(range).ok_or("RFC3339 field missing")?.parse()?)
    };
    let year = number(0..4)?;
    let month = number(5..7)?;
    let day = number(8..10)?;
    let hour = number(11..13)?;
    let minute = number(14..16)?;
    let second = number(17..19)?;
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return Err("recorded_at month is out of range".into()),
    };
    if day == 0 || day > days || hour > 23 || minute > 59 || second > 59 {
        return Err("recorded_at date/time is out of range".into());
    }
    let suffix = value.get(19..).ok_or("RFC3339 suffix missing")?;
    let suffix = if let Some(rest) = suffix.strip_prefix('.') {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || digits > 9 {
            return Err("RFC3339 fraction is invalid".into());
        }
        &rest[digits..]
    } else {
        suffix
    };
    if suffix != "Z" {
        let sign = suffix.as_bytes().first().copied();
        if !matches!(sign, Some(b'+' | b'-')) || suffix.len() != 6 || suffix.as_bytes()[3] != b':' {
            return Err("RFC3339 offset is invalid".into());
        }
        let offset_hour: u32 = suffix[1..3].parse()?;
        let offset_minute: u32 = suffix[4..6].parse()?;
        if offset_hour > 23 || offset_minute > 59 {
            return Err("RFC3339 offset is out of range".into());
        }
    }
    Ok(())
}

fn require_canonical_base64url(value: &str, bytes: usize, field: &str) -> AnyResult<()> {
    if canonical_base64url(value, Some(bytes))? != ("accepted", "none") {
        return Err(format!("{field} is not canonical base64url of {bytes} bytes").into());
    }
    Ok(())
}

fn require_canonical_base64url_any(value: &str, field: &str) -> AnyResult<()> {
    if canonical_base64url(value, None)? != ("accepted", "none") {
        return Err(format!("{field} is not canonical unpadded base64url").into());
    }
    Ok(())
}

const MAX_CBOR_NESTING_DEPTH: usize = 32;

fn parse_cbor_item(bytes: &[u8], offset: &mut usize, depth: usize) -> Result<Option<u64>, String> {
    if depth > MAX_CBOR_NESTING_DEPTH {
        return Err("CBOR nesting depth exceeds 32".to_owned());
    }
    let initial = *bytes.get(*offset).ok_or("truncated CBOR item")?;
    *offset += 1;
    let major = initial >> 5;
    let additional = initial & 0x1f;
    if additional == 31 {
        return Err("indefinite CBOR item".to_owned());
    }
    let argument = read_cbor_argument(bytes, offset, additional)?;
    match major {
        0 => Ok(Some(argument)),
        1 => Ok(None),
        2 => {
            take_cbor_bytes(bytes, offset, argument)?;
            Ok(None)
        }
        3 => {
            let text = take_cbor_bytes(bytes, offset, argument)?;
            std::str::from_utf8(text).map_err(|_| "invalid CBOR UTF-8")?;
            Ok(None)
        }
        4 => {
            for _ in 0..argument {
                parse_cbor_item(bytes, offset, depth + 1)?;
            }
            Ok(None)
        }
        5 => {
            let mut previous = None;
            for _ in 0..argument {
                let key = parse_cbor_item(bytes, offset, depth + 1)?
                    .ok_or("deterministic contract map key is not unsigned")?;
                if previous.is_some_and(|prior| prior >= key) {
                    return Err("duplicate or out-of-order CBOR map key".to_owned());
                }
                previous = Some(key);
                parse_cbor_item(bytes, offset, depth + 1)?;
            }
            Ok(None)
        }
        6 => Err("CBOR tags are forbidden".to_owned()),
        7 if additional == 20 || additional == 21 || additional == 22 => Ok(None),
        7 => Err("CBOR float or unsupported simple value".to_owned()),
        _ => Err("unknown CBOR major type".to_owned()),
    }
}

fn read_cbor_argument(bytes: &[u8], offset: &mut usize, additional: u8) -> Result<u64, String> {
    match additional {
        0..=23 => Ok(u64::from(additional)),
        24 => {
            let value = u64::from(*bytes.get(*offset).ok_or("truncated CBOR uint8")?);
            *offset += 1;
            if value < 24 {
                Err("non-shortest CBOR argument".to_owned())
            } else {
                Ok(value)
            }
        }
        25 => read_fixed_argument(bytes, offset, 2, 256),
        26 => read_fixed_argument(bytes, offset, 4, 65_536),
        27 => read_fixed_argument(bytes, offset, 8, 4_294_967_296),
        _ => Err("reserved CBOR additional information".to_owned()),
    }
}

fn read_fixed_argument(
    bytes: &[u8],
    offset: &mut usize,
    width: usize,
    minimum: u64,
) -> Result<u64, String> {
    let end = offset.checked_add(width).ok_or("CBOR offset overflow")?;
    let raw = bytes.get(*offset..end).ok_or("truncated CBOR argument")?;
    *offset = end;
    let value = raw
        .iter()
        .fold(0_u64, |value, byte| (value << 8) | u64::from(*byte));
    if value < minimum {
        Err("non-shortest CBOR argument".to_owned())
    } else {
        Ok(value)
    }
}

fn take_cbor_bytes<'a>(
    bytes: &'a [u8],
    offset: &mut usize,
    length: u64,
) -> Result<&'a [u8], String> {
    let length = usize::try_from(length).map_err(|_| "CBOR length overflow")?;
    let end = offset.checked_add(length).ok_or("CBOR length overflow")?;
    let value = bytes.get(*offset..end).ok_or("truncated CBOR bytes")?;
    *offset = end;
    Ok(value)
}

fn verify_noise_failure_classification(fixture: &Value) -> AnyResult<(&'static str, &'static str)> {
    let input = &fixture["input"];
    let stage = input["stage"]
        .as_str()
        .ok_or("Noise failure stage missing")?;
    let authenticated = input["authenticated_session_opened"]
        .as_bool()
        .ok_or("authenticated_session_opened missing")?;
    if stage == "handshake_aead" {
        return Ok(("rejected", "authenticationFailed"));
    }
    if authenticated && stage == "session_binding_check" {
        return Ok(("rejected", "bindingMismatch"));
    }
    Ok(("rejected", "internalError"))
}

fn verify_semantic_encoding(fixture: &Value) -> AnyResult<(&'static str, &'static str)> {
    let input = &fixture["input"];
    let kind = input["kind"]
        .as_str()
        .ok_or("semantic encoding kind missing")?;
    match kind {
        "utf8_bytes" => {
            let value = input["value"].as_str().ok_or("UTF-8 value missing")?;
            let max = input["max_bytes"]
                .as_u64()
                .ok_or("UTF-8 byte cap missing")?;
            if value.len() as u64 > max {
                Ok(("rejected", "oversize"))
            } else {
                Ok(("accepted", "none"))
            }
        }
        "target_reference" => {
            let value = input["value"].as_str().ok_or("Target Reference missing")?;
            let encoded = value
                .strip_prefix("target_")
                .ok_or("Target Reference prefix missing")?;
            canonical_base64url(encoded, Some(32))
        }
        "base64url" => {
            let value = input["value"].as_str().ok_or("base64url value missing")?;
            let exact = input
                .get("exact_decoded_bytes")
                .and_then(Value::as_u64)
                .map(|value| value as usize);
            canonical_base64url(value, exact)
        }
        _ => Err(format!("unknown semantic encoding kind {kind}").into()),
    }
}

fn canonical_base64url(
    encoded: &str,
    exact_decoded_bytes: Option<usize>,
) -> AnyResult<(&'static str, &'static str)> {
    if encoded.is_empty() || encoded.len() % 4 == 1 || encoded.contains('=') {
        return Ok(("rejected", "malformed"));
    }
    let decoded = match URL_SAFE_NO_PAD.decode(encoded) {
        Ok(value) => value,
        Err(_) => return Ok(("rejected", "malformed")),
    };
    if exact_decoded_bytes.is_some_and(|expected| decoded.len() != expected)
        || URL_SAFE_NO_PAD.encode(&decoded) != encoded
    {
        return Ok(("rejected", "malformed"));
    }
    Ok(("accepted", "none"))
}

fn verify_ready_timestamps(fixture: &Value) -> AnyResult<(&'static str, &'static str)> {
    let input = &fixture["input"];
    let broker_issued = input["broker_issued_at_unix_ms"]
        .as_u64()
        .ok_or("broker issued timestamp missing")?;
    let broker_expires = input["broker_expires_at_unix_ms"]
        .as_u64()
        .ok_or("broker expiry timestamp missing")?;
    let projected_issued = input["projected_issued_at_unix_ms"]
        .as_u64()
        .ok_or("projected issued timestamp missing")?;
    let projected_expires = input["projected_expires_at_unix_ms"]
        .as_u64()
        .ok_or("projected expiry timestamp missing")?;
    let now = input["now_unix_ms"]
        .as_u64()
        .ok_or("current timestamp missing")?;
    if broker_issued.checked_add(30_000) != Some(broker_expires) {
        return Ok(("rejected", "internalError"));
    }
    if projected_issued != broker_issued || projected_expires != broker_expires {
        return Ok(("rejected", "bindingMismatch"));
    }
    if now >= broker_expires {
        return Ok(("rejected", "stale"));
    }
    Ok(("accepted", "none"))
}

fn verify_catalog_list_evidence(fixture: &Value) -> AnyResult<(&'static str, &'static str)> {
    let input = fixture["input"]
        .as_object()
        .ok_or("catalog-list evidence input missing")?;
    let allowed = ["catalog", "declaration"];
    if input.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Ok(("rejected", "malformed"));
    }
    let catalog = input["catalog"]
        .as_object()
        .ok_or("catalog identity missing")?;
    let declaration = input["declaration"]
        .as_object()
        .ok_or("catalog declaration missing")?;
    if catalog.get("id").and_then(Value::as_str).is_none()
        || catalog.get("generation").and_then(Value::as_u64).is_none()
        || declaration.get("id").and_then(Value::as_str) != Some("smoke.ready")
        || declaration.get("kind").and_then(Value::as_str) != Some("resource")
        || declaration
            .get("declaration_revision")
            .and_then(Value::as_u64)
            != Some(1)
    {
        return Ok(("rejected", "malformed"));
    }
    Ok(("accepted", "none"))
}

fn verify_broker_packet(
    fixture: &Value,
    max_broker_cbor_bytes: u64,
) -> AnyResult<(&'static str, &'static str)> {
    let declared = fixture["input"]["declared_cbor_length"]
        .as_u64()
        .ok_or("declared_cbor_length missing")?;
    if declared == 0 {
        return Ok(("rejected", "malformed"));
    }
    if declared > max_broker_cbor_bytes {
        return Ok(("rejected", "oversize"));
    }
    let operation_limit = match fixture["input"]["operation"].as_str() {
        None | Some("exchange") => max_broker_cbor_bytes,
        Some("open_session") => 73_728,
        Some(_) => 8_192,
    };
    if declared > operation_limit {
        return Ok(("rejected", "oversize"));
    }
    Ok(("accepted", "none"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn positive_vector(name: &str) -> Value {
        serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../vectors/",
            "bootstrap-nk-success.json"
        )))
        .map(|bootstrap| {
            if name == "bootstrap-nk-success.json" {
                bootstrap
            } else {
                serde_json::from_str(include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../vectors/session-nnpsk0-success.json"
                )))
                .expect("session vector")
            }
        })
        .expect("bootstrap vector")
    }

    fn android_descriptor_vector() -> Value {
        parse_strict_json(
            &fs::read(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../vectors/bootstrap-android-descriptor.json"),
            )
            .expect("generated Android descriptor vector"),
        )
        .expect("strict Android descriptor JSON")
    }

    fn replace_hex_subsequence(value: &mut Value, field: &str, needle: &[u8], replacement: &[u8]) {
        let mut bytes = hex::decode(value["canonical_input"][field].as_str().expect("hex field"))
            .expect("valid hex");
        let offset = bytes
            .windows(needle.len())
            .position(|window| window == needle)
            .expect("subsequence present");
        bytes[offset..offset + needle.len()].copy_from_slice(replacement);
        value["canonical_input"][field] = Value::String(hex::encode(bytes));
    }

    fn replace_hex_byte(value: &mut Value, field: &str, offset: usize, replacement: u8) {
        let mut bytes = hex::decode(value["canonical_input"][field].as_str().expect("hex field"))
            .expect("valid hex");
        bytes[offset] = replacement;
        value["canonical_input"][field] = Value::String(hex::encode(bytes));
    }

    fn rewrite_session_response_ciphertext(vector: &mut Value, response: &[u8]) {
        let canonical = &vector["canonical_input"];
        let material = &vector["test_only_material"]["material"];
        let prologue = hex::decode(canonical["prologue_cbor_hex"].as_str().expect("prologue"))
            .expect("prologue hex");
        let psk: [u8; 32] = hex::decode(
            material["process_bootstrap_secret_hex"]
                .as_str()
                .expect("PSK"),
        )
        .expect("PSK hex")
        .try_into()
        .expect("PSK size");
        let target_key = hex::decode(
            material["target_ephemeral_private_hex"]
                .as_str()
                .expect("target key"),
        )
        .expect("target key hex");
        let broker_key = hex::decode(
            material["broker_ephemeral_private_hex"]
                .as_str()
                .expect("broker key"),
        )
        .expect("broker key hex");
        let params: NoiseParams = canonical["noise_name"]
            .as_str()
            .expect("Noise name")
            .parse()
            .expect("Noise params");
        let mut target = Builder::new(params.clone())
            .prologue(&prologue)
            .expect("target prologue")
            .psk(0, &psk)
            .expect("target PSK")
            .fixed_ephemeral_key_for_testing_only(&target_key)
            .build_initiator()
            .expect("target initiator");
        let mut broker = Builder::new(params)
            .prologue(&prologue)
            .expect("broker prologue")
            .psk(0, &psk)
            .expect("broker PSK")
            .fixed_ephemeral_key_for_testing_only(&broker_key)
            .build_responder()
            .expect("broker responder");
        let mut ciphertext = vec![0_u8; 65_535];
        let mut plaintext = vec![0_u8; 65_535];
        let m1_len = target
            .write_message(&[], &mut ciphertext)
            .expect("write M1");
        broker
            .read_message(&ciphertext[..m1_len], &mut plaintext)
            .expect("read M1");
        let m2_len = broker
            .write_message(&[], &mut ciphertext)
            .expect("write M2");
        target
            .read_message(&ciphertext[..m2_len], &mut plaintext)
            .expect("read M2");
        let mut target_transport = target.into_transport_mode().expect("target transport");
        let mut broker_transport = broker.into_transport_mode().expect("broker transport");
        for (sender_target, payload_field, kind) in [
            (true, "target_finished_cbor_hex", 2_u8),
            (false, "broker_finished_cbor_hex", 2_u8),
            (false, "session_open_utf8_hex", 1_u8),
        ] {
            let payload = hex::decode(canonical[payload_field].as_str().expect("payload"))
                .expect("payload hex");
            let plain = expected_record_plaintext(kind, &payload);
            let len = if sender_target {
                target_transport
                    .write_message(&plain, &mut ciphertext)
                    .expect("target write")
            } else {
                broker_transport
                    .write_message(&plain, &mut ciphertext)
                    .expect("broker write")
            };
            if sender_target {
                broker_transport
                    .read_message(&ciphertext[..len], &mut plaintext)
                    .expect("broker read");
            } else {
                target_transport
                    .read_message(&ciphertext[..len], &mut plaintext)
                    .expect("target read");
            }
        }
        let response_plain = expected_record_plaintext(1, response);
        let response_len = target_transport
            .write_message(&response_plain, &mut ciphertext)
            .expect("target response write");
        let response_outer = {
            let mut outer = Vec::with_capacity(response_len + 2);
            outer.extend_from_slice(&(response_len as u16).to_be_bytes());
            outer.extend_from_slice(&ciphertext[..response_len]);
            outer
        };
        vector["canonical_input"]["session_open_response_utf8_hex"] =
            Value::String(hex::encode(response));
        vector["expected"]["session_open_response_outer_hex"] =
            Value::String(hex::encode(&response_outer));
        let mut transcript = Vec::new();
        for field in [
            "m1_outer_hex",
            "m2_outer_hex",
            "target_finished_outer_hex",
            "broker_finished_outer_hex",
            "session_open_outer_hex",
            "session_open_response_outer_hex",
        ] {
            transcript.extend(
                hex::decode(
                    vector["expected"][field]
                        .as_str()
                        .expect("transcript field"),
                )
                .expect("transcript hex"),
            );
        }
        vector["expected"]["transcript_hex"] = Value::String(hex::encode(&transcript));
        vector["expected"]["transcript_sha256"] =
            Value::String(format!("sha256:{:x}", Sha256::digest(&transcript)));
    }

    #[test]
    fn bootstrap_descriptor_is_closed_over_independent_accepted_material() {
        let base = positive_vector("bootstrap-nk-success.json");
        verify_positive_bootstrap(&base).expect("accepted bootstrap vector");

        for (offset, replacement, label) in [(2, 2, "version"), (4, 1, "platform")] {
            let mut mutated = base.clone();
            replace_hex_byte(
                &mut mutated,
                "launch_descriptor_cbor_hex",
                offset,
                replacement,
            );
            assert!(
                verify_positive_bootstrap(&mutated).is_err(),
                "accepted wrong descriptor {label}"
            );
        }

        for (needle, replacement, label) in [
            (vec![0x51; 16], vec![0x52; 16], "lease"),
            (vec![0x71; 32], vec![0x72; 32], "nonce"),
            (vec![0x81; 32], vec![0x82; 32], "App digest"),
        ] {
            let mut mutated = base.clone();
            replace_hex_subsequence(
                &mut mutated,
                "launch_descriptor_cbor_hex",
                &needle,
                &replacement,
            );
            assert!(
                verify_positive_bootstrap(&mutated).is_err(),
                "accepted wrong descriptor {label}"
            );
        }

        let static_public = hex::decode(
            base["test_only_material"]["material"]["broker_static_public_hex"]
                .as_str()
                .expect("static public"),
        )
        .expect("static public hex");
        let mut wrong_key = static_public.clone();
        wrong_key[0] ^= 1;
        let mut mutated = base.clone();
        replace_hex_subsequence(
            &mut mutated,
            "launch_descriptor_cbor_hex",
            &static_public,
            &wrong_key,
        );
        assert!(verify_positive_bootstrap(&mutated).is_err());

        let digest = hex::decode(
            base["canonical_input"]["target_reference_digest_hex"]
                .as_str()
                .expect("Target Reference digest"),
        )
        .expect("Target Reference digest hex");
        let mut wrong_digest = digest.clone();
        wrong_digest[0] ^= 1;
        let mut mutated = base.clone();
        replace_hex_subsequence(
            &mut mutated,
            "launch_descriptor_cbor_hex",
            &digest,
            &wrong_digest,
        );
        assert!(verify_positive_bootstrap(&mutated).is_err());

        let mut mutated = base.clone();
        let expiry = 1_893_456_000_000_u64.to_be_bytes();
        let wrong_expiry = 1_893_456_000_001_u64.to_be_bytes();
        replace_hex_subsequence(
            &mut mutated,
            "launch_descriptor_cbor_hex",
            &expiry,
            &wrong_expiry,
        );
        assert!(verify_positive_bootstrap(&mutated).is_err());

        let mut mutated = base;
        replace_hex_subsequence(
            &mut mutated,
            "launch_descriptor_cbor_hex",
            b"127.0.0.1",
            b"126.0.0.1",
        );
        assert!(verify_positive_bootstrap(&mutated).is_err());
    }

    #[test]
    fn session_response_facts_survive_ciphertext_and_transcript_recomputation() {
        let base = positive_vector("session-nnpsk0-success.json");
        verify_positive_session(&base).expect("accepted session vector");
        for (needle, replacement) in [
            (
                "session_test_0123456789abcdef",
                "session_test_fedcba9876543210",
            ),
            ("4503599627370123", "4503599627370124"),
            ("semantic.catalog", "semantic.catalox"),
            ("session.core", "session.corf"),
            (r#""major":1"#, r#""major":2"#),
            (r#""minor":2"#, r#""minor":1"#),
            ("16777216", "16777215"),
            ("67108864", "67108863"),
            ("10000", "10001"),
        ] {
            let mut mutated = base.clone();
            let response = String::from_utf8(
                hex::decode(
                    mutated["canonical_input"]["session_open_response_utf8_hex"]
                        .as_str()
                        .expect("response"),
                )
                .expect("response hex"),
            )
            .expect("response UTF-8")
            .replace(needle, replacement);
            assert_ne!(
                response,
                String::from_utf8(
                    hex::decode(
                        base["canonical_input"]["session_open_response_utf8_hex"]
                            .as_str()
                            .expect("response"),
                    )
                    .expect("response hex"),
                )
                .expect("response UTF-8")
            );
            rewrite_session_response_ciphertext(&mut mutated, response.as_bytes());
            assert!(
                verify_positive_session(&mutated).is_err(),
                "accepted mutated response fact: {needle}"
            );
        }
    }

    #[test]
    fn android_descriptor_positive_closes_platform_and_localabstract_shape() {
        let bootstrap = positive_vector("bootstrap-nk-success.json");
        let base = android_descriptor_vector();
        verify_android_descriptor_values(&base, &bootstrap)
            .expect("accepted Android launch descriptor");
        let name = base["canonical_input"]["launch_endpoint"]["localabstract_name"]
            .as_str()
            .expect("localabstract name")
            .as_bytes();
        let descriptor = hex::decode(
            base["canonical_input"]["launch_descriptor_cbor_hex"]
                .as_str()
                .expect("descriptor hex"),
        )
        .expect("descriptor bytes");
        let name_offset = descriptor
            .windows(name.len())
            .position(|window| window == name)
            .expect("localabstract name in descriptor");
        let endpoint_map_offset = name_offset.checked_sub(4).expect("endpoint prefix");
        assert_eq!(descriptor[endpoint_map_offset], 0xa1);

        let mut wrong_platform = base.clone();
        replace_hex_byte(&mut wrong_platform, "launch_descriptor_cbor_hex", 4, 0);
        assert!(verify_android_descriptor_values(&wrong_platform, &bootstrap).is_err());

        let mut wrong_name = base.clone();
        let mut wrong_name_bytes = name.to_vec();
        wrong_name_bytes[0] = b'b';
        replace_hex_subsequence(
            &mut wrong_name,
            "launch_descriptor_cbor_hex",
            name,
            &wrong_name_bytes,
        );
        assert!(verify_android_descriptor_values(&wrong_name, &bootstrap).is_err());

        let mut wrong_shape = base.clone();
        replace_hex_byte(
            &mut wrong_shape,
            "launch_descriptor_cbor_hex",
            endpoint_map_offset,
            0xa0,
        );
        assert!(verify_android_descriptor_values(&wrong_shape, &bootstrap).is_err());

        let mut extra_endpoint_field = descriptor;
        extra_endpoint_field[endpoint_map_offset] = 0xa2;
        extra_endpoint_field.splice(name_offset + name.len()..name_offset + name.len(), [1, 0]);
        validate_deterministic_cbor(&extra_endpoint_field)
            .expect("extra endpoint field mutation remains canonical CBOR");
        let mut extra_field = base;
        extra_field["canonical_input"]["launch_descriptor_cbor_hex"] =
            Value::String(hex::encode(extra_endpoint_field));
        assert!(verify_android_descriptor_values(&extra_field, &bootstrap).is_err());
    }
}
