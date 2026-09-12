//! Private usbmux ListDevices/Connect compatibility seam.
//!
//! This is not an Apple SDK API. Callers must not leak usbmux fields into the
//! product model. USB absence is Unavailable, never Rejected.

use apppilotkit_host_runtime::adapter::{
    AbsoluteDeadline, Cancellation, PlatformFailure, PlatformFailureKind, RawDuplex,
};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{check_cancel_deadline, failure, remaining};

pub(crate) const DEFAULT_USBMUXD: &str = "/var/run/usbmuxd";
const USBMUX_VERSION: u32 = 1;
const USBMUX_PLIST: u32 = 8;
const HEADER_LEN: usize = 16;
const PLIST_CAP: usize = 1_048_576;
const CONNECT_OK: i64 = 0;
const CONNECT_CONNREFUSED: i64 = 3;

pub(crate) enum UsbMuxConnectError {
    ConnectionRefused,
    Failed(PlatformFailure),
}

impl From<PlatformFailure> for UsbMuxConnectError {
    fn from(failure: PlatformFailure) -> Self {
        Self::Failed(failure)
    }
}

pub(crate) trait UsbMux: Send + Sync {
    fn find_usb_device(
        &self,
        serial: &str,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<u32, PlatformFailure>;

    fn connect(
        &self,
        device_id: u32,
        port: u16,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<Arc<dyn RawDuplex>, UsbMuxConnectError>;
}

pub(crate) struct SystemUsbMux {
    socket_path: PathBuf,
}

impl SystemUsbMux {
    pub(crate) fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }
}

impl UsbMux for SystemUsbMux {
    fn find_usb_device(
        &self,
        serial: &str,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<u32, PlatformFailure> {
        check_cancel_deadline(cancellation, deadline)?;
        let mut stream = connect_usbmux(&self.socket_path, cancellation, deadline)?;
        write_plist(
            &mut stream,
            1,
            &list_devices_plist(),
            cancellation,
            deadline,
        )?;
        let (_tag, body) = read_plist(&mut stream, cancellation, deadline)?;
        parse_usb_device_id(&body, serial)
    }

    fn connect(
        &self,
        device_id: u32,
        port: u16,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<Arc<dyn RawDuplex>, UsbMuxConnectError> {
        check_cancel_deadline(cancellation, deadline)?;
        let mut stream = connect_usbmux(&self.socket_path, cancellation, deadline)?;
        const CONNECT_TAG: u32 = 1;
        write_plist(
            &mut stream,
            CONNECT_TAG,
            &connect_plist(device_id, port),
            cancellation,
            deadline,
        )?;
        let (tag, body) = read_plist(&mut stream, cancellation, deadline)?;
        let number = connect_result_number(&body, tag, CONNECT_TAG)?;
        if number == CONNECT_CONNREFUSED {
            return Err(UsbMuxConnectError::ConnectionRefused);
        }
        if number != CONNECT_OK {
            return Err(UsbMuxConnectError::Failed(failure(
                PlatformFailureKind::Unavailable,
            )));
        }
        Ok(Arc::new(UnixRawDuplex::new(stream)?))
    }
}

pub(crate) fn usbmux_port_number(port: u16) -> u32 {
    u32::from(port.to_be())
}

fn connect_usbmux(
    path: &Path,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<UnixStream, PlatformFailure> {
    check_cancel_deadline(cancellation, deadline)?;
    let timeout = remaining(deadline)?;
    let stream =
        UnixStream::connect(path).map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    Ok(stream)
}

fn write_plist(
    stream: &mut UnixStream,
    tag: u32,
    plist: &str,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(), PlatformFailure> {
    check_cancel_deadline(cancellation, deadline)?;
    let body = plist.as_bytes();
    let length = HEADER_LEN
        .checked_add(body.len())
        .ok_or_else(|| failure(PlatformFailureKind::Internal))?;
    let length = u32::try_from(length).map_err(|_| failure(PlatformFailureKind::Internal))?;
    let mut packet = Vec::with_capacity(length as usize);
    packet.extend_from_slice(&length.to_le_bytes());
    packet.extend_from_slice(&USBMUX_VERSION.to_le_bytes());
    packet.extend_from_slice(&USBMUX_PLIST.to_le_bytes());
    packet.extend_from_slice(&tag.to_le_bytes());
    packet.extend_from_slice(body);
    stream
        .write_all(&packet)
        .map_err(|_| failure(PlatformFailureKind::Unavailable))
}

fn read_plist(
    stream: &mut UnixStream,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(u32, Vec<u8>), PlatformFailure> {
    check_cancel_deadline(cancellation, deadline)?;
    let mut header = [0_u8; HEADER_LEN];
    stream
        .read_exact(&mut header)
        .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    let length = u32::from_le_bytes(header[0..4].try_into().expect("header length")) as usize;
    let version = u32::from_le_bytes(header[4..8].try_into().expect("header version"));
    let packet_type = u32::from_le_bytes(header[8..12].try_into().expect("header type"));
    let tag = u32::from_le_bytes(header[12..16].try_into().expect("header tag"));
    if version != USBMUX_VERSION || packet_type != USBMUX_PLIST || length < HEADER_LEN {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    let body_len = length - HEADER_LEN;
    if body_len == 0 || body_len > PLIST_CAP {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    let mut body = vec![0_u8; body_len];
    stream
        .read_exact(&mut body)
        .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    Ok((tag, body))
}

fn list_devices_plist() -> String {
    plist_dict(&[
        ("ClientVersionString", PlistValue::String("usbmuxd-1.1.0")),
        ("MessageType", PlistValue::String("ListDevices")),
        ("ProgName", PlistValue::String("apppilotkit")),
        ("kLibUSBMuxVersion", PlistValue::Integer(3)),
    ])
}

fn connect_plist(device_id: u32, port: u16) -> String {
    plist_dict(&[
        ("DeviceID", PlistValue::Integer(i64::from(device_id))),
        ("MessageType", PlistValue::String("Connect")),
        (
            "PortNumber",
            PlistValue::Integer(i64::from(usbmux_port_number(port))),
        ),
        ("ProgName", PlistValue::String("apppilotkit")),
    ])
}

enum PlistValue<'a> {
    String(&'a str),
    Integer(i64),
}

fn plist_dict(entries: &[(&str, PlistValue<'_>)]) -> String {
    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    for (key, value) in entries {
        body.push_str("<key>");
        body.push_str(&xml_escape(key));
        body.push_str("</key>");
        match value {
            PlistValue::String(text) => {
                body.push_str("<string>");
                body.push_str(&xml_escape(text));
                body.push_str("</string>\n");
            }
            PlistValue::Integer(number) => {
                body.push_str("<integer>");
                body.push_str(&number.to_string());
                body.push_str("</integer>\n");
            }
        }
    }
    body.push_str("</dict>\n</plist>\n");
    body
}

fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn parse_usb_device_id(bytes: &[u8], serial: &str) -> Result<u32, PlatformFailure> {
    let plist = parse_plist(bytes).map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    let devices = match plist.get("DeviceList") {
        Some(Plist::Array(entries)) => entries,
        _ => return Err(failure(PlatformFailureKind::Unavailable)),
    };
    let mut usb_ids = Vec::new();
    let mut network_only = false;
    for entry in devices {
        let dict = match entry {
            Plist::Dict(dict) => dict,
            _ => continue,
        };
        let properties = match dict.get("Properties") {
            Some(Plist::Dict(properties)) => properties,
            _ => continue,
        };
        let Some(found_serial) = optional_string_field(properties, "SerialNumber")
            .or_else(|| optional_string_field(dict, "SerialNumber"))
        else {
            continue;
        };
        if found_serial != serial {
            continue;
        }
        let Some(connection) = optional_string_field(properties, "ConnectionType")
            .or_else(|| optional_string_field(dict, "ConnectionType"))
        else {
            continue;
        };
        let Ok(device_id) =
            integer_field(dict, "DeviceID").or_else(|_| integer_field(properties, "DeviceID"))
        else {
            continue;
        };
        let Ok(device_id) = u32::try_from(device_id) else {
            continue;
        };
        if connection == "USB" {
            usb_ids.push(device_id);
        } else if connection == "Network" {
            network_only = true;
        } else {
            return Err(failure(PlatformFailureKind::Unavailable));
        }
    }
    match usb_ids.as_slice() {
        [device_id] => Ok(*device_id),
        [] if network_only => Err(failure(PlatformFailureKind::Unavailable)),
        [] => Err(failure(PlatformFailureKind::Unavailable)),
        _ => Err(failure(PlatformFailureKind::Rejected)),
    }
}

fn connect_result_number(
    bytes: &[u8],
    response_tag: u32,
    request_tag: u32,
) -> Result<i64, PlatformFailure> {
    if response_tag != request_tag {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let plist = parse_plist(bytes).map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    match plist.get("MessageType") {
        Some(Plist::String(value)) if value == "Result" => {}
        _ => return Err(failure(PlatformFailureKind::Rejected)),
    }
    integer_field(&plist, "Number")
}

fn optional_string_field<'a>(dict: &'a BTreeMap<String, Plist>, key: &str) -> Option<&'a str> {
    match dict.get(key) {
        Some(Plist::String(value)) => Some(value.as_str()),
        _ => None,
    }
}

fn integer_field(dict: &BTreeMap<String, Plist>, key: &str) -> Result<i64, PlatformFailure> {
    match dict.get(key) {
        Some(Plist::Integer(value)) => Ok(*value),
        _ => Err(failure(PlatformFailureKind::Unavailable)),
    }
}

#[derive(Debug)]
#[allow(dead_code)]
enum Plist {
    Dict(BTreeMap<String, Plist>),
    Array(Vec<Plist>),
    String(String),
    Integer(i64),
    Boolean(bool),
    Ignored,
}

struct XmlParser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> XmlParser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn rest(&self) -> &'a str {
        &self.input[self.pos..]
    }

    fn skip_ws(&mut self) {
        let rest = self.rest();
        let trim = rest.trim_start();
        self.pos += rest.len() - trim.len();
    }

    fn consume(&mut self, prefix: &str) -> bool {
        self.skip_ws();
        if self.rest().starts_with(prefix) {
            self.pos += prefix.len();
            true
        } else {
            false
        }
    }

    fn take_until(&mut self, delimiter: &str) -> Option<&'a str> {
        let rest = self.rest();
        let index = rest.find(delimiter)?;
        self.pos += index + delimiter.len();
        Some(&rest[..index])
    }

    fn skip_prolog(&mut self) -> Result<(), ()> {
        self.skip_ws();
        if self.consume("<?xml") {
            self.take_until("?>").ok_or(())?;
        }
        self.skip_ws();
        if self.consume("<!DOCTYPE") {
            self.take_until(">").ok_or(())?;
        }
        self.skip_ws();
        if !self.consume("<plist") {
            return Err(());
        }
        self.take_until(">").ok_or(())?;
        Ok(())
    }

    fn parse_value(&mut self) -> Result<Plist, ()> {
        self.skip_ws();
        if self.consume("<dict>") {
            return self.parse_dict();
        }
        if self.consume("<array>") {
            return self.parse_array();
        }
        if self.consume("<string>") {
            let text = self.take_until("</string>").ok_or(())?;
            return Ok(Plist::String(xml_unescape(text)?));
        }
        if self.consume("<integer>") {
            let text = self.take_until("</integer>").ok_or(())?;
            let value = text.trim().parse::<i64>().map_err(|_| ())?;
            return Ok(Plist::Integer(value));
        }
        if self.consume("<true/>") {
            return Ok(Plist::Boolean(true));
        }
        if self.consume("<false/>") {
            return Ok(Plist::Boolean(false));
        }
        if self.consume("<dict/>") {
            return Ok(Plist::Dict(BTreeMap::new()));
        }
        if self.consume("<string/>") {
            return Ok(Plist::String(String::new()));
        }
        if self.consume("<array/>") {
            return Ok(Plist::Array(Vec::new()));
        }
        if self.consume("<data>") {
            self.take_until("</data>").ok_or(())?;
            return Ok(Plist::Ignored);
        }
        if self.consume("<data/>") {
            return Ok(Plist::Ignored);
        }
        if self.consume("<real>") {
            self.take_until("</real>").ok_or(())?;
            return Ok(Plist::Ignored);
        }
        if self.consume("<real/>") {
            return Ok(Plist::Ignored);
        }
        if self.consume("<date>") {
            self.take_until("</date>").ok_or(())?;
            return Ok(Plist::Ignored);
        }
        if self.consume("<date/>") {
            return Ok(Plist::Ignored);
        }
        Err(())
    }

    fn parse_dict(&mut self) -> Result<Plist, ()> {
        let mut dict = BTreeMap::new();
        loop {
            self.skip_ws();
            if self.consume("</dict>") {
                return Ok(Plist::Dict(dict));
            }
            if !self.consume("<key>") {
                return Err(());
            }
            let key = xml_unescape(self.take_until("</key>").ok_or(())?)?;
            if dict.contains_key(&key) {
                return Err(());
            }
            let value = self.parse_value()?;
            dict.insert(key, value);
        }
    }

    fn parse_array(&mut self) -> Result<Plist, ()> {
        let mut values = Vec::new();
        loop {
            self.skip_ws();
            if self.consume("</array>") {
                return Ok(Plist::Array(values));
            }
            values.push(self.parse_value()?);
        }
    }
}

fn xml_unescape(value: &str) -> Result<String, ()> {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '&' {
            out.push(ch);
            continue;
        }
        let mut entity = String::new();
        loop {
            match chars.next() {
                Some(';') => break,
                Some(next) => entity.push(next),
                None => return Err(()),
            }
        }
        match entity.as_str() {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => return Err(()),
        }
    }
    Ok(out)
}

#[allow(dead_code)]
pub(crate) fn xml_plist_string(bytes: &[u8], key: &str) -> Result<String, ()> {
    match parse_plist(bytes)?.get(key) {
        Some(Plist::String(value)) => Ok(value.clone()),
        _ => Err(()),
    }
}

fn parse_plist(bytes: &[u8]) -> Result<BTreeMap<String, Plist>, ()> {
    let text = std::str::from_utf8(bytes).map_err(|_| ())?;
    let mut parser = XmlParser::new(text);
    parser.skip_prolog()?;
    match parser.parse_value()? {
        Plist::Dict(dict) => {
            parser.skip_ws();
            let _ = parser.consume("</plist>");
            parser.skip_ws();
            if parser.rest().is_empty() {
                Ok(dict)
            } else {
                Err(())
            }
        }
        _ => Err(()),
    }
}

pub(crate) struct UnixRawDuplex {
    reader: std::sync::Mutex<UnixStream>,
    writer: std::sync::Mutex<UnixStream>,
    shutdown: UnixStream,
    cancelled: AtomicBool,
}

impl UnixRawDuplex {
    pub(crate) fn new(stream: UnixStream) -> Result<Self, PlatformFailure> {
        let writer = stream
            .try_clone()
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
        let shutdown = stream
            .try_clone()
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
        Ok(Self {
            reader: std::sync::Mutex::new(stream),
            writer: std::sync::Mutex::new(writer),
            shutdown,
            cancelled: AtomicBool::new(false),
        })
    }

    fn ensure_active(&self) -> Result<(), PlatformFailure> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(failure(PlatformFailureKind::Cancelled))
        } else {
            Ok(())
        }
    }
}

impl RawDuplex for UnixRawDuplex {
    fn read(
        &self,
        output: &mut [u8],
        absolute_deadline: AbsoluteDeadline,
    ) -> Result<usize, PlatformFailure> {
        self.ensure_active()?;
        let timeout = remaining(absolute_deadline)?;
        let mut stream = self
            .reader
            .lock()
            .map_err(|_| failure(PlatformFailureKind::Internal))?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
        match stream.read(output) {
            Ok(count) => {
                self.ensure_active()?;
                Ok(count)
            }
            Err(error) => Err(io_timeout_or_unavailable(&error, &self.cancelled)),
        }
    }

    fn write(
        &self,
        input: &[u8],
        absolute_deadline: AbsoluteDeadline,
    ) -> Result<usize, PlatformFailure> {
        self.ensure_active()?;
        let timeout = remaining(absolute_deadline)?;
        let mut stream = self
            .writer
            .lock()
            .map_err(|_| failure(PlatformFailureKind::Internal))?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
        match stream.write(input) {
            Ok(count) => {
                self.ensure_active()?;
                Ok(count)
            }
            Err(error) => Err(io_timeout_or_unavailable(&error, &self.cancelled)),
        }
    }

    fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            let _ = self.shutdown.shutdown(std::net::Shutdown::Both);
        }
    }
}

fn io_timeout_or_unavailable(error: &std::io::Error, cancelled: &AtomicBool) -> PlatformFailure {
    if cancelled.load(Ordering::Acquire) {
        failure(PlatformFailureKind::Cancelled)
    } else if matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) {
        failure(PlatformFailureKind::TimedOut)
    } else if matches!(
        error.kind(),
        std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::NotConnected
    ) {
        failure(PlatformFailureKind::Eof)
    } else {
        failure(PlatformFailureKind::Unavailable)
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

    #[test]
    fn hardware_usb_serial_is_selected_and_network_only_is_unavailable() {
        let usb = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
<key>DeviceList</key>
<array>
<dict>
<key>DeviceID</key>
<integer>7</integer>
<key>Properties</key>
<dict>
<key>ConnectionType</key>
<string>USB</string>
<key>SerialNumber</key>
<string>00008150-001E60460130401C</string>
</dict>
</dict>
<dict>
<key>DeviceID</key>
<integer>8</integer>
<key>Properties</key>
<dict>
<key>ConnectionType</key>
<string>Network</string>
<key>SerialNumber</key>
<string>00008150-001E60460130401C</string>
</dict>
</dict>
</array>
</dict>
</plist>
"#;
        assert_eq!(
            match parse_usb_device_id(usb, "00008150-001E60460130401C") {
                Ok(id) => id,
                Err(_) => panic!("usb serial must resolve"),
            },
            7
        );

        let network = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
<key>DeviceList</key>
<array>
<dict>
<key>DeviceID</key>
<integer>8</integer>
<key>Properties</key>
<dict>
<key>ConnectionType</key>
<string>Network</string>
<key>SerialNumber</key>
<string>00008150-001E60460130401C</string>
</dict>
</dict>
</array>
</dict>
</plist>
"#;
        match parse_usb_device_id(network, "00008150-001E60460130401C") {
            Err(error) => assert_eq!(error.kind(), PlatformFailureKind::Unavailable),
            Ok(_) => panic!("network-only"),
        }
        match parse_usb_device_id(network, "00000000-0000000000000000") {
            Err(error) => assert_eq!(error.kind(), PlatformFailureKind::Unavailable),
            Ok(_) => panic!("missing"),
        }
    }

    #[test]
    fn list_devices_skips_data_tags_and_missing_serials() {
        let body = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
<key>DeviceList</key>
<array>
<dict>
<key>DeviceID</key>
<integer>1</integer>
<key>Properties</key>
<dict>
<key>ConnectionType</key>
<string>USB</string>
<key>NetworkAddress</key>
<data>QQ==</data>
<key>Empty</key>
<string/>
</dict>
</dict>
<dict>
<key>DeviceID</key>
<integer>7</integer>
<key>Properties</key>
<dict>
<key>ConnectionType</key>
<string>USB</string>
<key>SerialNumber</key>
<string>00008150-001E60460130401C</string>
<key>NetworkAddress</key>
<data>Qg==</data>
<key>EmptyDict</key>
<dict/>
<key>Latency</key>
<real>1.5</real>
<key>Attached</key>
<date>2020-01-01T00:00:00Z</date>
</dict>
</dict>
</array>
</dict>
</plist>
"#;
        assert_eq!(
            match parse_usb_device_id(body, "00008150-001E60460130401C") {
                Ok(id) => id,
                Err(_) => panic!("usb serial must resolve past data tags"),
            },
            7
        );
    }

    #[test]
    fn connect_port_uses_network_byte_order() {
        assert_eq!(usbmux_port_number(22), 0x1600);
        assert_eq!(usbmux_port_number(49_152), 0x00C0);
        let plist = connect_plist(7, 49_152);
        assert!(plist.contains("<integer>7</integer>"));
        assert!(plist.contains("<integer>192</integer>"));
        assert!(plist.contains("<string>Connect</string>"));
    }

    #[test]
    fn connect_result_requires_message_type_result_and_matching_tag() {
        let ok = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
<key>MessageType</key>
<string>Result</string>
<key>Number</key>
<integer>0</integer>
</dict>
</plist>
"#;
        assert_eq!(
            match connect_result_number(ok, 1, 1) {
                Ok(number) => number,
                Err(_) => panic!("valid connect result"),
            },
            0
        );

        let missing_type = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
<key>Number</key>
<integer>0</integer>
</dict>
</plist>
"#;
        match connect_result_number(missing_type, 1, 1) {
            Err(error) => assert_eq!(error.kind(), PlatformFailureKind::Rejected),
            Ok(_) => panic!("missing MessageType"),
        }

        let wrong_type = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
<key>MessageType</key>
<string>Connect</string>
<key>Number</key>
<integer>0</integer>
</dict>
</plist>
"#;
        match connect_result_number(wrong_type, 1, 1) {
            Err(error) => assert_eq!(error.kind(), PlatformFailureKind::Rejected),
            Ok(_) => panic!("non-Result MessageType"),
        }

        match connect_result_number(ok, 2, 1) {
            Err(error) => assert_eq!(error.kind(), PlatformFailureKind::Rejected),
            Ok(_) => panic!("tag mismatch"),
        }
    }
}
