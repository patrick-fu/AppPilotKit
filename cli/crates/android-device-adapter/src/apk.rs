use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::raw::{c_char, c_int, c_uint, c_ulong, c_void},
    path::Path,
    ptr,
};

const MANIFEST_NAME: &[u8] = b"AndroidManifest.xml";
const MANIFEST_LIMIT: usize = 2 * 1024 * 1024;
const CENTRAL_DIRECTORY_LIMIT: usize = 8 * 1024 * 1024;
const ENTRY_LIMIT: usize = 4096;
// Parser-owned growth is capped independently of the compressed APK: decoded
// string payload plus a 65,536-entry String table, a 65,536-entry resource map,
// one 1,024-attribute element, and this u32 index stack.
const STRING_POOL_DECODED_BYTES_LIMIT: usize = 2 * 1024 * 1024;
const XML_DEPTH_LIMIT: usize = 256;
const NO_INDEX: u32 = u32::MAX;
const ANDROID_NS: &str = "http://schemas.android.com/apk/res/android";
const BOOTSTRAP_CLASS: &str = "AppPilotKitBootstrapActivity";

const ZIP_LOCAL: u32 = 0x0403_4b50;
const ZIP_CENTRAL: u32 = 0x0201_4b50;
const ZIP_EOCD: u32 = 0x0605_4b50;

const RES_XML: u16 = 0x0003;
const RES_STRING_POOL: u16 = 0x0001;
const RES_XML_START_NAMESPACE: u16 = 0x0100;
const RES_XML_END_NAMESPACE: u16 = 0x0101;
const RES_XML_START_ELEMENT: u16 = 0x0102;
const RES_XML_END_ELEMENT: u16 = 0x0103;
const RES_XML_RESOURCE_MAP: u16 = 0x0180;
const STRING_POOL_UTF8: u32 = 0x0000_0100;
const TYPE_STRING: u8 = 0x03;
const TYPE_INT_BOOLEAN: u8 = 0x12;
const ANDROID_ATTR_NAME: u32 = 0x0101_0003;
const ANDROID_ATTR_ENABLED: u32 = 0x0101_000e;
const ANDROID_ATTR_EXPORTED: u32 = 0x0101_0010;

pub(crate) fn validate_apk_manifest(path: &Path, expected_package: &str) -> Result<(), ()> {
    let manifest = extract_manifest(path)?;
    let facts = parse_binary_manifest(&manifest, expected_package)?;
    if facts.package == expected_package
        && facts.application_count == 1
        && facts.bootstrap_activity_count == 1
        && facts.bootstrap_exported
        && facts.bootstrap_enabled
    {
        Ok(())
    } else {
        Err(())
    }
}

fn extract_manifest(path: &Path) -> Result<Vec<u8>, ()> {
    let mut file = File::open(path).map_err(|_| ())?;
    let length = file.metadata().map_err(|_| ())?.len();
    let tail_length = usize::try_from(length.min(65_557)).map_err(|_| ())?;
    if tail_length < 22 {
        return Err(());
    }
    file.seek(SeekFrom::End(-i64::try_from(tail_length).map_err(|_| ())?))
        .map_err(|_| ())?;
    let mut tail = vec![0_u8; tail_length];
    file.read_exact(&mut tail).map_err(|_| ())?;
    let candidates: Vec<usize> = (0..=tail.len() - 22)
        .filter(|offset| {
            le_u32(&tail, *offset) == Some(ZIP_EOCD)
                && le_u16(&tail, *offset + 20).is_some_and(|comment| {
                    offset
                        .checked_add(22 + usize::from(comment))
                        .is_some_and(|end| end == tail.len())
                })
        })
        .collect();
    if candidates.len() != 1 {
        return Err(());
    }
    let eocd_in_tail = candidates[0];
    let eocd_absolute = length
        .checked_sub(u64::try_from(tail_length).map_err(|_| ())?)
        .and_then(|start| start.checked_add(u64::try_from(eocd_in_tail).ok()?))
        .ok_or(())?;
    if le_u16(&tail, eocd_in_tail + 4) != Some(0) || le_u16(&tail, eocd_in_tail + 6) != Some(0) {
        return Err(());
    }
    let disk_entries = usize::from(le_u16(&tail, eocd_in_tail + 8).ok_or(())?);
    let total_entries = usize::from(le_u16(&tail, eocd_in_tail + 10).ok_or(())?);
    if total_entries == 0
        || total_entries > ENTRY_LIMIT
        || disk_entries != total_entries
        || total_entries == usize::from(u16::MAX)
    {
        return Err(());
    }
    let central_size =
        usize::try_from(le_u32(&tail, eocd_in_tail + 12).ok_or(())?).map_err(|_| ())?;
    let central_offset = u64::from(le_u32(&tail, eocd_in_tail + 16).ok_or(())?);
    if central_size == 0
        || central_size > CENTRAL_DIRECTORY_LIMIT
        || central_offset.checked_add(u64::try_from(central_size).map_err(|_| ())?)
            != Some(eocd_absolute)
    {
        return Err(());
    }
    file.seek(SeekFrom::Start(central_offset)).map_err(|_| ())?;
    let mut central = vec![0_u8; central_size];
    file.read_exact(&mut central).map_err(|_| ())?;
    let mut cursor = 0_usize;
    let mut manifest = None;
    for _ in 0..total_entries {
        if le_u32(&central, cursor) != Some(ZIP_CENTRAL) {
            return Err(());
        }
        let flags = le_u16(&central, cursor + 8).ok_or(())?;
        let method = le_u16(&central, cursor + 10).ok_or(())?;
        let crc = le_u32(&central, cursor + 16).ok_or(())?;
        let compressed =
            usize::try_from(le_u32(&central, cursor + 20).ok_or(())?).map_err(|_| ())?;
        let uncompressed =
            usize::try_from(le_u32(&central, cursor + 24).ok_or(())?).map_err(|_| ())?;
        let name_length = usize::from(le_u16(&central, cursor + 28).ok_or(())?);
        let extra_length = usize::from(le_u16(&central, cursor + 30).ok_or(())?);
        let comment_length = usize::from(le_u16(&central, cursor + 32).ok_or(())?);
        let disk = le_u16(&central, cursor + 34).ok_or(())?;
        let local_offset = u64::from(le_u32(&central, cursor + 42).ok_or(())?);
        let entry_end = cursor
            .checked_add(46)
            .and_then(|value| value.checked_add(name_length))
            .and_then(|value| value.checked_add(extra_length))
            .and_then(|value| value.checked_add(comment_length))
            .filter(|end| *end <= central.len())
            .ok_or(())?;
        let name_start = cursor + 46;
        let name = &central[name_start..name_start + name_length];
        if disk != 0 || flags & !0x080e != 0 || flags & 1 != 0 {
            return Err(());
        }
        if name == MANIFEST_NAME {
            if manifest.is_some()
                || compressed > MANIFEST_LIMIT
                || uncompressed == 0
                || uncompressed > MANIFEST_LIMIT
            {
                return Err(());
            }
            manifest = Some(read_local_entry(
                &mut file,
                local_offset,
                central_offset,
                flags,
                method,
                crc,
                compressed,
                uncompressed,
                name,
            )?);
        }
        cursor = entry_end;
    }
    if cursor != central.len() {
        return Err(());
    }
    manifest.ok_or(())
}

#[allow(clippy::too_many_arguments)]
fn read_local_entry(
    file: &mut File,
    offset: u64,
    central_offset: u64,
    expected_flags: u16,
    expected_method: u16,
    expected_crc: u32,
    compressed_length: usize,
    uncompressed_length: usize,
    expected_name: &[u8],
) -> Result<Vec<u8>, ()> {
    file.seek(SeekFrom::Start(offset)).map_err(|_| ())?;
    let mut header = [0_u8; 30];
    file.read_exact(&mut header).map_err(|_| ())?;
    if le_u32(&header, 0) != Some(ZIP_LOCAL)
        || le_u16(&header, 6) != Some(expected_flags)
        || le_u16(&header, 8) != Some(expected_method)
    {
        return Err(());
    }
    let name_length = usize::from(le_u16(&header, 26).ok_or(())?);
    let extra_length = usize::from(le_u16(&header, 28).ok_or(())?);
    if name_length != expected_name.len() {
        return Err(());
    }
    let mut name = vec![0_u8; name_length];
    file.read_exact(&mut name).map_err(|_| ())?;
    if name != expected_name {
        return Err(());
    }
    let data_offset = offset
        .checked_add(30)
        .and_then(|value| value.checked_add(u64::try_from(name_length).ok()?))
        .and_then(|value| value.checked_add(u64::try_from(extra_length).ok()?))
        .ok_or(())?;
    let data_end = data_offset
        .checked_add(u64::try_from(compressed_length).map_err(|_| ())?)
        .filter(|end| *end <= central_offset)
        .ok_or(())?;
    if data_end < data_offset {
        return Err(());
    }
    file.seek(SeekFrom::Start(data_offset)).map_err(|_| ())?;
    let mut compressed = vec![0_u8; compressed_length];
    file.read_exact(&mut compressed).map_err(|_| ())?;
    let output = match expected_method {
        0 if compressed_length == uncompressed_length => compressed,
        8 => inflate_raw(&compressed, uncompressed_length)?,
        _ => return Err(()),
    };
    if output.len() != uncompressed_length || crc32(&output) != expected_crc {
        return Err(());
    }
    Ok(output)
}

struct ManifestFacts {
    package: String,
    application_count: usize,
    bootstrap_activity_count: usize,
    bootstrap_exported: bool,
    bootstrap_enabled: bool,
}

fn parse_binary_manifest(input: &[u8], expected_package: &str) -> Result<ManifestFacts, ()> {
    if input.len() < 8
        || le_u16(input, 0) != Some(RES_XML)
        || le_u16(input, 2) != Some(8)
        || usize::try_from(le_u32(input, 4).ok_or(())?).map_err(|_| ())? != input.len()
    {
        return Err(());
    }
    let expected_activity = format!("{expected_package}.{BOOTSTRAP_CLASS}");
    let mut strings = None;
    let mut stack: Vec<u32> = Vec::new();
    let mut package = None;
    let mut application_count = 0_usize;
    let mut bootstrap_activity_count = 0_usize;
    let mut bootstrap_exported = false;
    let mut bootstrap_enabled = false;
    let mut resource_map = None;
    let mut cursor = 8_usize;
    while cursor < input.len() {
        let kind = le_u16(input, cursor).ok_or(())?;
        let header_size = usize::from(le_u16(input, cursor + 2).ok_or(())?);
        let size = usize::try_from(le_u32(input, cursor + 4).ok_or(())?).map_err(|_| ())?;
        let end = cursor
            .checked_add(size)
            .filter(|end| header_size >= 8 && size >= header_size && *end <= input.len())
            .ok_or(())?;
        match kind {
            RES_STRING_POOL if strings.is_none() && stack.is_empty() => {
                strings = Some(parse_string_pool(&input[cursor..end])?);
            }
            RES_XML_RESOURCE_MAP
                if strings.is_some() && resource_map.is_none() && stack.is_empty() =>
            {
                resource_map = Some(parse_resource_map(
                    &input[cursor..end],
                    strings.as_ref().ok_or(())?.strings.len(),
                )?);
            }
            RES_XML_START_NAMESPACE | RES_XML_END_NAMESPACE => {
                if strings.is_none() || header_size != 16 || size != 24 {
                    return Err(());
                }
            }
            RES_XML_START_ELEMENT => {
                if stack.len() >= XML_DEPTH_LIMIT {
                    return Err(());
                }
                let pool = strings.as_ref().ok_or(())?;
                let element = parse_start_element(
                    input,
                    cursor,
                    end,
                    header_size,
                    pool,
                    resource_map.as_deref().ok_or(())?,
                )?;
                let parent = stack.last().map(|index| pool.get(*index)).transpose()?;
                match element.name {
                    "manifest" if stack.is_empty() && package.is_none() => {
                        let value = element
                            .identity_string_attribute(None, "package", None)?
                            .ok_or(())?
                            .to_owned();
                        if value != expected_package {
                            return Err(());
                        }
                        package = Some(value);
                    }
                    "application" if parent == Some("manifest") => {
                        application_count += 1;
                    }
                    "activity" if parent == Some("application") => {
                        if let Some(name) = element.identity_string_attribute(
                            Some(ANDROID_NS),
                            "name",
                            Some(ANDROID_ATTR_NAME),
                        )? {
                            let normalized = normalize_activity(expected_package, name)?;
                            if normalized == expected_activity {
                                bootstrap_activity_count += 1;
                                if bootstrap_activity_count != 1 {
                                    return Err(());
                                }
                                bootstrap_exported = element
                                    .identity_boolean_attribute(
                                        Some(ANDROID_NS),
                                        "exported",
                                        ANDROID_ATTR_EXPORTED,
                                    )?
                                    .ok_or(())?;
                                bootstrap_enabled = element
                                    .identity_boolean_attribute(
                                        Some(ANDROID_NS),
                                        "enabled",
                                        ANDROID_ATTR_ENABLED,
                                    )?
                                    .unwrap_or(true);
                            }
                        }
                    }
                    _ if stack.is_empty() => return Err(()),
                    _ => {}
                }
                stack.push(element.name_index);
            }
            RES_XML_END_ELEMENT => {
                if header_size != 16 || size != 24 {
                    return Err(());
                }
                let name_index = le_u32(input, cursor + 20).ok_or(())?;
                strings.as_ref().ok_or(())?.get(name_index)?;
                if stack.pop() != Some(name_index) {
                    return Err(());
                }
            }
            _ => return Err(()),
        }
        cursor = end;
    }
    if cursor != input.len() || !stack.is_empty() {
        return Err(());
    }
    Ok(ManifestFacts {
        package: package.ok_or(())?,
        application_count,
        bootstrap_activity_count,
        bootstrap_exported,
        bootstrap_enabled,
    })
}

fn parse_resource_map(input: &[u8], string_count: usize) -> Result<Vec<u32>, ()> {
    if input.len() < 8
        || le_u16(input, 0) != Some(RES_XML_RESOURCE_MAP)
        || le_u16(input, 2) != Some(8)
        || usize::try_from(le_u32(input, 4).ok_or(())?).map_err(|_| ())? != input.len()
        || !(input.len() - 8).is_multiple_of(4)
    {
        return Err(());
    }
    let count = (input.len() - 8) / 4;
    if count == 0 || count > string_count {
        return Err(());
    }
    (0..count)
        .map(|index| le_u32(input, 8 + index * 4).ok_or(()))
        .collect()
}

struct StartElement<'a> {
    name_index: u32,
    name: &'a str,
    attributes: Vec<Attribute<'a>>,
}

impl StartElement<'_> {
    fn identity_attribute(
        &self,
        namespace: Option<&str>,
        name: &str,
        resource_id: Option<u32>,
    ) -> Result<Option<&Attribute<'_>>, ()> {
        if let Some(expected_id) = resource_id
            && self.attributes.iter().any(|attribute| {
                (attribute.name == name || attribute.resource_id == Some(expected_id))
                    && (attribute.namespace != namespace
                        || attribute.name != name
                        || attribute.resource_id != Some(expected_id))
            })
        {
            return Err(());
        }
        let mut matches = self.attributes.iter().filter(|attribute| {
            attribute.namespace == namespace
                && attribute.name == name
                && resource_id.is_none_or(|expected| attribute.resource_id == Some(expected))
        });
        let first = matches.next();
        if matches.next().is_some() {
            return Err(());
        }
        Ok(first)
    }

    fn identity_string_attribute(
        &self,
        namespace: Option<&str>,
        name: &str,
        resource_id: Option<u32>,
    ) -> Result<Option<&str>, ()> {
        let Some(attribute) = self.identity_attribute(namespace, name, resource_id)? else {
            return Ok(None);
        };
        let AttributeValue::String(value) = attribute.typed else {
            return Err(());
        };
        if attribute.raw.is_some_and(|raw| raw != value) {
            return Err(());
        }
        Ok(Some(value))
    }

    fn identity_boolean_attribute(
        &self,
        namespace: Option<&str>,
        name: &str,
        resource_id: u32,
    ) -> Result<Option<bool>, ()> {
        let Some(attribute) = self.identity_attribute(namespace, name, Some(resource_id))? else {
            return Ok(None);
        };
        let AttributeValue::Boolean(value) = attribute.typed else {
            return Err(());
        };
        if attribute
            .raw
            .is_some_and(|raw| raw != if value { "true" } else { "false" })
        {
            return Err(());
        }
        Ok(Some(value))
    }
}

struct Attribute<'a> {
    namespace: Option<&'a str>,
    name: &'a str,
    resource_id: Option<u32>,
    raw: Option<&'a str>,
    typed: AttributeValue<'a>,
}

enum AttributeValue<'a> {
    String(&'a str),
    Boolean(bool),
    Other,
}

fn parse_start_element<'a>(
    input: &'a [u8],
    cursor: usize,
    end: usize,
    header_size: usize,
    strings: &'a StringPool,
    resource_map: &[u32],
) -> Result<StartElement<'a>, ()> {
    if header_size != 16 || end < cursor + 36 {
        return Err(());
    }
    let name_index = le_u32(input, cursor + 20).ok_or(())?;
    let name = strings.get(name_index)?;
    let attribute_start = usize::from(le_u16(input, cursor + 24).ok_or(())?);
    let attribute_size = usize::from(le_u16(input, cursor + 26).ok_or(())?);
    let attribute_count = usize::from(le_u16(input, cursor + 28).ok_or(())?);
    if attribute_start != 20 || attribute_size != 20 || attribute_count > 1024 {
        return Err(());
    }
    let first = cursor.checked_add(16 + attribute_start).ok_or(())?;
    let attributes_end = first
        .checked_add(attribute_count.checked_mul(attribute_size).ok_or(())?)
        .filter(|value| *value == end)
        .ok_or(())?;
    if attributes_end != end {
        return Err(());
    }
    let mut attributes = Vec::with_capacity(attribute_count);
    for index in 0..attribute_count {
        let offset = first + index * attribute_size;
        let namespace_index = le_u32(input, offset).ok_or(())?;
        let namespace = if namespace_index == NO_INDEX {
            None
        } else {
            Some(strings.get(namespace_index)?)
        };
        let name_index = le_u32(input, offset + 4).ok_or(())?;
        let attribute_name = strings.get(name_index)?;
        let resource_id = usize::try_from(name_index)
            .ok()
            .and_then(|index| resource_map.get(index))
            .copied()
            .filter(|value| *value != 0);
        let raw_index = le_u32(input, offset + 8).ok_or(())?;
        if le_u16(input, offset + 12) != Some(8) || input.get(offset + 14) != Some(&0) {
            return Err(());
        }
        let data_type = *input.get(offset + 15).ok_or(())?;
        let data = le_u32(input, offset + 16).ok_or(())?;
        let raw = if raw_index == NO_INDEX {
            None
        } else {
            Some(strings.get(raw_index)?)
        };
        let typed = match data_type {
            TYPE_STRING => AttributeValue::String(strings.get(data)?),
            TYPE_INT_BOOLEAN => AttributeValue::Boolean(data != 0),
            0x00..=0x08 | 0x10..=0x11 | 0x13..=0x1f => AttributeValue::Other,
            _ => return Err(()),
        };
        attributes.push(Attribute {
            namespace,
            name: attribute_name,
            resource_id,
            raw,
            typed,
        });
    }
    Ok(StartElement {
        name_index,
        name,
        attributes,
    })
}

struct StringPool {
    strings: Vec<String>,
}

impl StringPool {
    fn get(&self, index: u32) -> Result<&str, ()> {
        self.strings
            .get(usize::try_from(index).map_err(|_| ())?)
            .map(String::as_str)
            .ok_or(())
    }
}

struct DecodedStringBudget {
    remaining: usize,
}

impl DecodedStringBudget {
    const fn new() -> Self {
        Self {
            remaining: STRING_POOL_DECODED_BYTES_LIMIT,
        }
    }

    fn claim(&mut self, bytes: usize) -> Result<(), ()> {
        self.remaining = self.remaining.checked_sub(bytes).ok_or(())?;
        Ok(())
    }
}

fn parse_string_pool(input: &[u8]) -> Result<StringPool, ()> {
    if input.len() < 28 || input.len() > MANIFEST_LIMIT || le_u16(input, 0) != Some(RES_STRING_POOL)
    {
        return Err(());
    }
    let header_size = usize::from(le_u16(input, 2).ok_or(())?);
    let string_count = usize::try_from(le_u32(input, 8).ok_or(())?).map_err(|_| ())?;
    let style_count = le_u32(input, 12).ok_or(())?;
    let flags = le_u32(input, 16).ok_or(())?;
    let strings_start = usize::try_from(le_u32(input, 20).ok_or(())?).map_err(|_| ())?;
    let styles_start = le_u32(input, 24).ok_or(())?;
    if header_size != 28
        || string_count > 65_536
        || style_count != 0
        || styles_start != 0
        || flags & !STRING_POOL_UTF8 != 0
        || strings_start < header_size + string_count * 4
        || strings_start > input.len()
    {
        return Err(());
    }
    let utf8 = flags & STRING_POOL_UTF8 != 0;
    let mut strings = Vec::with_capacity(string_count);
    let mut budget = DecodedStringBudget::new();
    for index in 0..string_count {
        let relative =
            usize::try_from(le_u32(input, header_size + index * 4).ok_or(())?).map_err(|_| ())?;
        let offset = strings_start.checked_add(relative).ok_or(())?;
        let value = if utf8 {
            decode_utf8_string(input, offset, &mut budget)?
        } else {
            decode_utf16_string(input, offset, &mut budget)?
        };
        strings.push(value);
    }
    Ok(StringPool { strings })
}

fn decode_utf8_string(
    input: &[u8],
    mut offset: usize,
    budget: &mut DecodedStringBudget,
) -> Result<String, ()> {
    let (utf16_length, next) = length8(input, offset)?;
    offset = next;
    let (byte_length, next) = length8(input, offset)?;
    offset = next;
    if byte_length > STRING_POOL_DECODED_BYTES_LIMIT {
        return Err(());
    }
    let end = offset
        .checked_add(byte_length)
        .filter(|end| *end < input.len())
        .ok_or(())?;
    if input[end] != 0 {
        return Err(());
    }
    let value = std::str::from_utf8(&input[offset..end]).map_err(|_| ())?;
    if value.encode_utf16().count() != utf16_length {
        return Err(());
    }
    budget.claim(byte_length)?;
    Ok(value.to_owned())
}

fn length8(input: &[u8], offset: usize) -> Result<(usize, usize), ()> {
    let first = *input.get(offset).ok_or(())?;
    if first & 0x80 == 0 {
        Ok((usize::from(first), offset + 1))
    } else {
        let second = *input.get(offset + 1).ok_or(())?;
        Ok((
            (usize::from(first & 0x7f) << 8) | usize::from(second),
            offset + 2,
        ))
    }
}

fn decode_utf16_string(
    input: &[u8],
    offset: usize,
    budget: &mut DecodedStringBudget,
) -> Result<String, ()> {
    let first = le_u16(input, offset).ok_or(())?;
    let (length, cursor) = if first & 0x8000 == 0 {
        (usize::from(first), offset + 2)
    } else {
        let second = le_u16(input, offset + 2).ok_or(())?;
        (
            (usize::from(first & 0x7fff) << 16) | usize::from(second),
            offset + 4,
        )
    };
    let terminator = cursor
        .checked_add(length.checked_mul(2).ok_or(())?)
        .filter(|end| end.checked_add(2).is_some_and(|after| after <= input.len()))
        .ok_or(())?;
    if le_u16(input, terminator) != Some(0) {
        return Err(());
    }
    let units = || {
        input[cursor..terminator]
            .chunks_exact(2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
    };
    let decoded_length = char::decode_utf16(units()).try_fold(0_usize, |total, decoded| {
        let character = decoded.map_err(|_| ())?;
        total
            .checked_add(character.len_utf8())
            .filter(|length| *length <= budget.remaining)
            .ok_or(())
    })?;
    budget.claim(decoded_length)?;
    let mut value = String::with_capacity(decoded_length);
    for decoded in char::decode_utf16(units()) {
        let character = decoded.map_err(|_| ())?;
        value.push(character);
    }
    Ok(value)
}

fn normalize_activity(package: &str, value: &str) -> Result<String, ()> {
    if value.is_empty() || value.bytes().any(|byte| !byte.is_ascii_graphic()) {
        return Err(());
    }
    if value.starts_with('.') {
        Ok(format!("{package}{value}"))
    } else if value.contains('.') {
        Ok(value.to_owned())
    } else {
        Ok(format!("{package}.{value}"))
    }
}

fn le_u16(input: &[u8], offset: usize) -> Option<u16> {
    let bytes: [u8; 2] = input.get(offset..offset.checked_add(2)?)?.try_into().ok()?;
    Some(u16::from_le_bytes(bytes))
}

fn le_u32(input: &[u8], offset: usize) -> Option<u32> {
    let bytes: [u8; 4] = input.get(offset..offset.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn crc32(input: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in input {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0_u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

#[repr(C)]
struct ZStream {
    next_in: *mut u8,
    avail_in: c_uint,
    total_in: c_ulong,
    next_out: *mut u8,
    avail_out: c_uint,
    total_out: c_ulong,
    msg: *mut c_char,
    state: *mut c_void,
    zalloc: Option<unsafe extern "C" fn(*mut c_void, c_uint, c_uint) -> *mut c_void>,
    zfree: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
    opaque: *mut c_void,
    data_type: c_int,
    adler: c_ulong,
    reserved: c_ulong,
}

#[link(name = "z")]
unsafe extern "C" {
    #[link_name = "zlibVersion"]
    fn zlib_version() -> *const c_char;
    #[link_name = "inflateInit2_"]
    fn inflate_init2(
        stream: *mut ZStream,
        window_bits: c_int,
        version: *const c_char,
        stream_size: c_int,
    ) -> c_int;
    fn inflate(stream: *mut ZStream, flush: c_int) -> c_int;
    #[link_name = "inflateEnd"]
    fn inflate_end(stream: *mut ZStream) -> c_int;
}

fn inflate_raw(input: &[u8], output_length: usize) -> Result<Vec<u8>, ()> {
    if input.is_empty() || output_length == 0 {
        return Err(());
    }
    let mut output = vec![0_u8; output_length];
    let mut stream = ZStream {
        next_in: input.as_ptr().cast_mut(),
        avail_in: c_uint::try_from(input.len()).map_err(|_| ())?,
        total_in: 0,
        next_out: output.as_mut_ptr(),
        avail_out: c_uint::try_from(output.len()).map_err(|_| ())?,
        total_out: 0,
        msg: ptr::null_mut(),
        state: ptr::null_mut(),
        zalloc: None,
        zfree: None,
        opaque: ptr::null_mut(),
        data_type: 0,
        adler: 0,
        reserved: 0,
    };
    // SAFETY: `stream` points to initialized storage and its input/output
    // buffers remain alive and exclusive for the complete zlib call sequence.
    let initialized = unsafe {
        inflate_init2(
            &mut stream,
            -15,
            zlib_version(),
            c_int::try_from(std::mem::size_of::<ZStream>()).map_err(|_| ())?,
        )
    };
    if initialized != 0 {
        return Err(());
    }
    // SAFETY: successful initialization gives zlib sole use of this stream
    // until `inflate_end`; the backing slices have not moved.
    let result = unsafe { inflate(&mut stream, 4) };
    // SAFETY: every successful `inflate_init2` is paired exactly once.
    let ended = unsafe { inflate_end(&mut stream) };
    if result != 1
        || ended != 0
        || usize::try_from(stream.total_out).map_err(|_| ())? != output_length
        || stream.avail_in != 0
    {
        return Err(());
    }
    Ok(output)
}
