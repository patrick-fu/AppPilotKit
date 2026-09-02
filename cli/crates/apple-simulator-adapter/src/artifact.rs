use super::{
    AbsoluteDeadline, Cancellation, PlatformFailure, PlatformFailureKind, check_cancel_deadline,
    failure,
};
use sha2::{Digest as _, Sha256};
use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::ffi::{CStr, CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

const MAGIC: &[u8] = b"APPPILOTKIT-IOS-APP-TREE\0\x01";
const MAX_ENTRIES: usize = 65_535;
const MAX_PATH_BYTES: usize = 4_096;
const MAX_COMPONENT_BYTES: usize = 255;
const MAX_DEPTH: usize = 64;
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_TOTAL_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const DARWIN_USER_TEMP_DIR: libc::c_int = 65_537;
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

#[repr(C)]
#[derive(Clone, Copy)]
struct CfRange {
    location: isize,
    length: isize,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataCreate(
        allocator: *const libc::c_void,
        bytes: *const u8,
        length: isize,
    ) -> *const libc::c_void;
    fn CFPropertyListCreateWithData(
        allocator: *const libc::c_void,
        data: *const libc::c_void,
        options: usize,
        format: *mut isize,
        error: *mut *const libc::c_void,
    ) -> *const libc::c_void;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFGetTypeID(value: *const libc::c_void) -> usize;
    fn CFDictionaryGetValue(
        dictionary: *const libc::c_void,
        key: *const libc::c_void,
    ) -> *const libc::c_void;
    fn CFStringCreateWithBytes(
        allocator: *const libc::c_void,
        bytes: *const u8,
        length: isize,
        encoding: u32,
        external_representation: u8,
    ) -> *const libc::c_void;
    fn CFStringGetLength(value: *const libc::c_void) -> isize;
    fn CFStringGetBytes(
        value: *const libc::c_void,
        range: CfRange,
        encoding: u32,
        loss_byte: u8,
        external_representation: u8,
        buffer: *mut u8,
        max_buffer_length: isize,
        used_buffer_length: *mut isize,
    ) -> isize;
    fn CFRelease(value: *const libc::c_void);
}

struct CfOwned(*const libc::c_void);

impl CfOwned {
    fn new(value: *const libc::c_void) -> Result<Self, PlatformFailure> {
        if value.is_null() {
            Err(failure(PlatformFailureKind::Rejected))
        } else {
            Ok(Self(value))
        }
    }
}

impl Drop for CfOwned {
    fn drop(&mut self) {
        // SAFETY: this wrapper uniquely owns the create-rule reference.
        unsafe { CFRelease(self.0) };
    }
}

pub(super) struct PreparedArtifact {
    _snapshot: tempfile::TempDir,
    app_path: PathBuf,
    _canonical_path: PathBuf,
    pub(super) identity: TreeIdentity,
}

impl PreparedArtifact {
    pub(super) fn app_path(&self) -> &Path {
        &self.app_path
    }

    pub(super) fn assert_unchanged(
        &self,
        app_id: &str,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        let current = inspect_bundle(&self.app_path, app_id, cancellation, deadline)?;
        if current != self.identity {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn snapshot_root(&self) -> &Path {
        self._snapshot.path()
    }

    #[cfg(test)]
    pub(super) fn canonical_path(&self) -> &Path {
        &self._canonical_path
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TreeIdentity {
    pub(super) digest: [u8; 32],
    pub(super) executable: String,
    pub(super) build: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Entry {
    path: Vec<u8>,
    kind: EntryKind,
    executable: bool,
    size: u64,
    observation: Observation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryKind {
    Directory,
    File,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Observation {
    device: u64,
    inode: u64,
    mode: u32,
    links: u64,
    size: i64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl Observation {
    fn from_stat(stat: &libc::stat) -> Self {
        Self {
            device: u64::try_from(stat.st_dev).unwrap_or_default(),
            inode: stat.st_ino,
            mode: u32::from(stat.st_mode),
            links: u64::from(stat.st_nlink),
            size: stat.st_size,
            modified_seconds: stat.st_mtime,
            modified_nanoseconds: stat.st_mtime_nsec,
            changed_seconds: stat.st_ctime,
            changed_nanoseconds: stat.st_ctime_nsec,
        }
    }
}

pub(super) fn prepare_snapshot(
    source_path: &Path,
    app_id: &str,
    expected_digest: &[u8; 32],
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<PreparedArtifact, PlatformFailure> {
    check_cancel_deadline(cancellation, deadline)?;
    let source = open_absolute_directory(source_path)?;
    reject_resource_fork(source.as_raw_fd())?;
    let source_before = fstat(source.as_raw_fd())?;
    if file_type(source_before.st_mode) != libc::S_IFDIR {
        return Err(failure(PlatformFailureKind::Rejected));
    }

    let parent = darwin_user_temp_dir()?;
    let snapshot = tempfile::Builder::new()
        .prefix("apppilotkit-ios-app-")
        .tempdir_in(parent)
        .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    fs::set_permissions(snapshot.path(), fs::Permissions::from_mode(0o700))
        .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    let app_path = snapshot.path().join("snapshot.app");
    fs::create_dir(&app_path).map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    fs::set_permissions(&app_path, fs::Permissions::from_mode(0o700))
        .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    let destination = open_absolute_directory(&app_path)?;

    let mut budget = CopyBudget::default();
    copy_directory(
        source.as_raw_fd(),
        destination.as_raw_fd(),
        &[],
        &mut budget,
        cancellation,
        deadline,
    )?;
    destination
        .sync_all()
        .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    if Observation::from_stat(&source_before) != Observation::from_stat(&fstat(source.as_raw_fd())?)
    {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    drop(source);

    let canonical_path = snapshot.path().join("artifact.ios-app-tree-v1");
    let identity =
        inspect_bundle_and_spool(&app_path, app_id, &canonical_path, cancellation, deadline)?;
    if &identity.digest != expected_digest {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    Ok(PreparedArtifact {
        _snapshot: snapshot,
        app_path,
        _canonical_path: canonical_path,
        identity,
    })
}

#[derive(Default)]
struct CopyBudget {
    entries: usize,
    total_file_bytes: u64,
}

fn copy_directory(
    source_fd: RawFd,
    destination_fd: RawFd,
    parent_path: &[u8],
    budget: &mut CopyBudget,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(), PlatformFailure> {
    for name in directory_names(source_fd)? {
        check_cancel_deadline(cancellation, deadline)?;
        let path = join_relative(parent_path, &name)?;
        budget.entries = budget
            .entries
            .checked_add(1)
            .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
        if budget.entries > MAX_ENTRIES {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        let name = CString::new(name).map_err(|_| failure(PlatformFailureKind::Rejected))?;
        let observed = fstatat_nofollow(source_fd, &name)?;
        match file_type(observed.st_mode) {
            libc::S_IFDIR => {
                let source_child = openat_directory(source_fd, &name)?;
                reject_resource_fork(source_child.as_raw_fd())?;
                require_same_object(&observed, &fstat(source_child.as_raw_fd())?)?;
                mkdirat(destination_fd, &name)?;
                let destination_child = openat_directory(destination_fd, &name)?;
                fchmod(destination_child.as_raw_fd(), 0o700)?;
                copy_directory(
                    source_child.as_raw_fd(),
                    destination_child.as_raw_fd(),
                    &path,
                    budget,
                    cancellation,
                    deadline,
                )?;
                destination_child
                    .sync_all()
                    .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
                if Observation::from_stat(&observed)
                    != Observation::from_stat(&fstat(source_child.as_raw_fd())?)
                {
                    return Err(failure(PlatformFailureKind::Rejected));
                }
            }
            libc::S_IFREG => {
                if observed.st_nlink != 1 || observed.st_size < 0 {
                    return Err(failure(PlatformFailureKind::Rejected));
                }
                let size = u64::try_from(observed.st_size)
                    .map_err(|_| failure(PlatformFailureKind::Rejected))?;
                require_file_budget(size, &mut budget.total_file_bytes)?;
                copy_regular_file(
                    source_fd,
                    destination_fd,
                    &name,
                    &observed,
                    size,
                    cancellation,
                    deadline,
                )?;
            }
            _ => return Err(failure(PlatformFailureKind::Rejected)),
        }
    }
    Ok(())
}

fn copy_regular_file(
    source_parent: RawFd,
    destination_parent: RawFd,
    name: &CStr,
    observed: &libc::stat,
    size: u64,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(), PlatformFailure> {
    let mut source = openat_file(source_parent, name, libc::O_RDONLY | libc::O_NONBLOCK, 0)?;
    reject_resource_fork(source.as_raw_fd())?;
    require_same_object(observed, &fstat(source.as_raw_fd())?)?;
    let executable = observed.st_mode & 0o111 != 0;
    let mode = if executable { 0o700 } else { 0o600 };
    let mut destination = openat_file(
        destination_parent,
        name,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        mode,
    )?;
    fchmod(destination.as_raw_fd(), mode)?;
    let mut remaining = size;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    while remaining > 0 {
        check_cancel_deadline(cancellation, deadline)?;
        let amount = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| failure(PlatformFailureKind::Internal))?;
        source
            .read_exact(&mut buffer[..amount])
            .map_err(|_| failure(PlatformFailureKind::Rejected))?;
        destination
            .write_all(&buffer[..amount])
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
        remaining -= u64::try_from(amount).unwrap_or_default();
    }
    let mut trailing = [0_u8; 1];
    if source
        .read(&mut trailing)
        .map_err(|_| failure(PlatformFailureKind::Rejected))?
        != 0
    {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    destination
        .sync_all()
        .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    if Observation::from_stat(observed) != Observation::from_stat(&fstat(source.as_raw_fd())?) {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    Ok(())
}

pub(super) fn inspect_bundle(
    root_path: &Path,
    app_id: &str,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<TreeIdentity, PlatformFailure> {
    inspect_bundle_inner(root_path, app_id, None, cancellation, deadline)
}

fn inspect_bundle_and_spool(
    root_path: &Path,
    app_id: &str,
    canonical_path: &Path,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<TreeIdentity, PlatformFailure> {
    let mut canonical = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(canonical_path)
        .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    let identity = inspect_bundle_inner(
        root_path,
        app_id,
        Some(&mut canonical),
        cancellation,
        deadline,
    )?;
    canonical
        .sync_all()
        .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    Ok(identity)
}

fn inspect_bundle_inner(
    root_path: &Path,
    app_id: &str,
    canonical: Option<&mut File>,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<TreeIdentity, PlatformFailure> {
    check_cancel_deadline(cancellation, deadline)?;
    let root = open_absolute_directory(root_path)?;
    reject_resource_fork(root.as_raw_fd())?;
    let root_before = fstat(root.as_raw_fd())?;
    if file_type(root_before.st_mode) != libc::S_IFDIR {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let before = collect_entries(root.as_raw_fd(), cancellation, deadline)?;
    let bundle = validate_bundle(root.as_raw_fd(), &before, app_id, cancellation, deadline)?;
    let digest = hash_entries(root.as_raw_fd(), &before, canonical, cancellation, deadline)?;
    let after = collect_entries(root.as_raw_fd(), cancellation, deadline)?;
    if before != after
        || Observation::from_stat(&root_before) != Observation::from_stat(&fstat(root.as_raw_fd())?)
    {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    Ok(TreeIdentity {
        digest,
        executable: bundle.0,
        build: bundle.1,
    })
}

fn collect_entries(
    root_fd: RawFd,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<Vec<Entry>, PlatformFailure> {
    let mut entries = Vec::new();
    let mut total = 0_u64;
    collect_directory(
        root_fd,
        &[],
        &mut entries,
        &mut total,
        cancellation,
        deadline,
    )?;
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    if entries.len() > MAX_ENTRIES
        || entries
            .windows(2)
            .any(|pair| pair[0].path.cmp(&pair[1].path) != Ordering::Less)
    {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    validate_topology(&entries)?;
    Ok(entries)
}

fn collect_directory(
    directory_fd: RawFd,
    parent_path: &[u8],
    entries: &mut Vec<Entry>,
    total: &mut u64,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(), PlatformFailure> {
    for name in directory_names(directory_fd)? {
        check_cancel_deadline(cancellation, deadline)?;
        let path = join_relative(parent_path, &name)?;
        if entries.len() >= MAX_ENTRIES {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        let name = CString::new(name).map_err(|_| failure(PlatformFailureKind::Rejected))?;
        let observed = fstatat_nofollow(directory_fd, &name)?;
        match file_type(observed.st_mode) {
            libc::S_IFDIR => {
                let child = openat_directory(directory_fd, &name)?;
                reject_resource_fork(child.as_raw_fd())?;
                require_same_object(&observed, &fstat(child.as_raw_fd())?)?;
                entries.push(Entry {
                    path: path.clone(),
                    kind: EntryKind::Directory,
                    executable: false,
                    size: 0,
                    observation: Observation::from_stat(&observed),
                });
                collect_directory(
                    child.as_raw_fd(),
                    &path,
                    entries,
                    total,
                    cancellation,
                    deadline,
                )?;
                if Observation::from_stat(&observed)
                    != Observation::from_stat(&fstat(child.as_raw_fd())?)
                {
                    return Err(failure(PlatformFailureKind::Rejected));
                }
            }
            libc::S_IFREG => {
                if observed.st_nlink != 1 || observed.st_size < 0 {
                    return Err(failure(PlatformFailureKind::Rejected));
                }
                let file = openat_file(directory_fd, &name, libc::O_RDONLY | libc::O_NONBLOCK, 0)?;
                reject_resource_fork(file.as_raw_fd())?;
                require_same_object(&observed, &fstat(file.as_raw_fd())?)?;
                let size = u64::try_from(observed.st_size)
                    .map_err(|_| failure(PlatformFailureKind::Rejected))?;
                require_file_budget(size, total)?;
                entries.push(Entry {
                    path,
                    kind: EntryKind::File,
                    executable: observed.st_mode & 0o111 != 0,
                    size,
                    observation: Observation::from_stat(&observed),
                });
            }
            _ => return Err(failure(PlatformFailureKind::Rejected)),
        }
    }
    Ok(())
}

fn validate_bundle(
    root_fd: RawFd,
    entries: &[Entry],
    app_id: &str,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(String, String), PlatformFailure> {
    check_cancel_deadline(cancellation, deadline)?;
    let info = entries
        .iter()
        .find(|entry| entry.path == b"Info.plist")
        .filter(|entry| entry.kind == EntryKind::File)
        .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
    let mut file = open_relative_file(root_fd, &info.path)?;
    let before = fstat(file.as_raw_fd())?;
    if Observation::from_stat(&before) != info.observation {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let capacity =
        usize::try_from(info.size).map_err(|_| failure(PlatformFailureKind::Rejected))?;
    let mut bytes = Vec::with_capacity(capacity);
    (&mut file)
        .take(info.size.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| failure(PlatformFailureKind::Rejected))?;
    if bytes.len() != capacity
        || Observation::from_stat(&fstat(file.as_raw_fd())?) != info.observation
    {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    check_cancel_deadline(cancellation, deadline)?;
    let dictionary = parse_property_list_dictionary(&bytes)?;
    let identifier = dictionary_string(dictionary.0, b"CFBundleIdentifier", 255)?;
    let package_type = dictionary_string(dictionary.0, b"CFBundlePackageType", 4)?;
    if identifier != app_id || package_type != "APPL" {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let build = dictionary_string(dictionary.0, b"CFBundleVersion", 128)?;
    if build.is_empty() || build.len() > 128 {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let executable = dictionary_string(dictionary.0, b"CFBundleExecutable", MAX_COMPONENT_BYTES)?;
    validate_path(executable.as_bytes())?;
    if executable.as_bytes().contains(&b'/') {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    if !entries.iter().any(|entry| {
        entry.path == executable.as_bytes() && entry.kind == EntryKind::File && entry.executable
    }) {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    Ok((executable, build))
}

fn parse_property_list_dictionary(bytes: &[u8]) -> Result<CfOwned, PlatformFailure> {
    let length =
        isize::try_from(bytes.len()).map_err(|_| failure(PlatformFailureKind::Rejected))?;
    // SAFETY: CoreFoundation copies the live byte slice into the returned data.
    let data = CfOwned::new(unsafe { CFDataCreate(std::ptr::null(), bytes.as_ptr(), length) })?;
    let mut error = std::ptr::null();
    // SAFETY: arguments are valid CoreFoundation objects/output storage; option
    // zero requests an immutable property list.
    let property_list = unsafe {
        CFPropertyListCreateWithData(
            std::ptr::null(),
            data.0,
            0,
            std::ptr::null_mut(),
            &mut error,
        )
    };
    if !error.is_null() {
        // SAFETY: a non-null create error is returned with create ownership.
        unsafe {
            CFRelease(error);
            if !property_list.is_null() {
                CFRelease(property_list);
            }
        }
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let property_list = CfOwned::new(property_list)?;
    // SAFETY: both calls inspect live CoreFoundation objects/type metadata.
    if unsafe { CFGetTypeID(property_list.0) } != unsafe { CFDictionaryGetTypeID() } {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    Ok(property_list)
}

fn dictionary_string(
    dictionary: *const libc::c_void,
    key_bytes: &[u8],
    max_utf8_bytes: usize,
) -> Result<String, PlatformFailure> {
    let key_length =
        isize::try_from(key_bytes.len()).map_err(|_| failure(PlatformFailureKind::Internal))?;
    // SAFETY: key bytes are live ASCII/UTF-8 and CoreFoundation copies them.
    let key = CfOwned::new(unsafe {
        CFStringCreateWithBytes(
            std::ptr::null(),
            key_bytes.as_ptr(),
            key_length,
            CF_STRING_ENCODING_UTF8,
            0,
        )
    })?;
    // SAFETY: dictionary and key are live CoreFoundation objects.
    let value = unsafe { CFDictionaryGetValue(dictionary, key.0) };
    if value.is_null() || unsafe { CFGetTypeID(value) } != unsafe { CFStringGetTypeID() } {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    // SAFETY: value is proven to be a live CFString.
    let utf16_length = unsafe { CFStringGetLength(value) };
    if utf16_length < 0 {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let buffer_cap = max_utf8_bytes
        .checked_add(1)
        .ok_or_else(|| failure(PlatformFailureKind::Internal))?;
    let mut buffer = vec![0_u8; buffer_cap];
    let mut used = 0_isize;
    // SAFETY: the output buffer has the advertised capacity; the range spans
    // the exact live CFString and UTF-8 can represent every scalar value.
    let converted = unsafe {
        CFStringGetBytes(
            value,
            CfRange {
                location: 0,
                length: utf16_length,
            },
            CF_STRING_ENCODING_UTF8,
            0,
            0,
            buffer.as_mut_ptr(),
            isize::try_from(buffer.len()).map_err(|_| failure(PlatformFailureKind::Internal))?,
            &mut used,
        )
    };
    let used = usize::try_from(used).map_err(|_| failure(PlatformFailureKind::Rejected))?;
    if converted != utf16_length || used > max_utf8_bytes {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    buffer.truncate(used);
    String::from_utf8(buffer).map_err(|_| failure(PlatformFailureKind::Rejected))
}

fn hash_entries(
    root_fd: RawFd,
    entries: &[Entry],
    mut canonical: Option<&mut File>,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<[u8; 32], PlatformFailure> {
    let count = u32::try_from(entries.len()).map_err(|_| failure(PlatformFailureKind::Rejected))?;
    let mut hasher = Sha256::new();
    let spool_failed = Cell::new(false);
    let mut emit = |bytes: &[u8]| {
        hasher.update(bytes);
        if let Some(writer) = canonical.as_mut()
            && writer.write_all(bytes).is_err()
        {
            spool_failed.set(true);
        }
    };
    emit_prefix(&mut emit, count);
    for entry in entries {
        check_cancel_deadline(cancellation, deadline)?;
        if spool_failed.get() {
            return Err(failure(PlatformFailureKind::Unavailable));
        }
        let kind = match entry.kind {
            EntryKind::Directory => 1,
            EntryKind::File => 2,
        };
        let path_len =
            u32::try_from(entry.path.len()).map_err(|_| failure(PlatformFailureKind::Rejected))?;
        emit_record_header(
            &mut emit,
            kind,
            path_len,
            &entry.path,
            u8::from(entry.executable),
            (entry.kind == EntryKind::File).then_some(entry.size),
        );
        if entry.kind == EntryKind::File {
            let mut file = open_relative_file(root_fd, &entry.path)?;
            if Observation::from_stat(&fstat(file.as_raw_fd())?) != entry.observation {
                return Err(failure(PlatformFailureKind::Rejected));
            }
            let mut remaining = entry.size;
            let mut buffer = [0_u8; COPY_BUFFER_BYTES];
            while remaining > 0 {
                check_cancel_deadline(cancellation, deadline)?;
                let amount = usize::try_from(remaining.min(buffer.len() as u64))
                    .map_err(|_| failure(PlatformFailureKind::Internal))?;
                file.read_exact(&mut buffer[..amount])
                    .map_err(|_| failure(PlatformFailureKind::Rejected))?;
                emit(&buffer[..amount]);
                if spool_failed.get() {
                    return Err(failure(PlatformFailureKind::Unavailable));
                }
                remaining -= u64::try_from(amount).unwrap_or_default();
            }
            let mut trailing = [0_u8; 1];
            if file
                .read(&mut trailing)
                .map_err(|_| failure(PlatformFailureKind::Rejected))?
                != 0
                || Observation::from_stat(&fstat(file.as_raw_fd())?) != entry.observation
            {
                return Err(failure(PlatformFailureKind::Rejected));
            }
        }
    }
    if spool_failed.get() {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    Ok(hasher.finalize().into())
}

fn emit_prefix(emit: &mut impl FnMut(&[u8]), count: u32) {
    emit(MAGIC);
    emit(&count.to_be_bytes());
}

fn emit_record_header(
    emit: &mut impl FnMut(&[u8]),
    kind: u8,
    path_len: u32,
    path: &[u8],
    executable_class: u8,
    file_len: Option<u64>,
) {
    emit(&[kind]);
    emit(&path_len.to_be_bytes());
    emit(path);
    emit(&[executable_class]);
    if let Some(file_len) = file_len {
        emit(&file_len.to_be_bytes());
    }
}

fn validate_topology(entries: &[Entry]) -> Result<(), PlatformFailure> {
    let directories = entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Directory)
        .map(|entry| entry.path.as_slice())
        .collect::<BTreeSet<_>>();
    for entry in entries {
        if let Some(separator) = entry.path.iter().rposition(|byte| *byte == b'/') {
            let parent = &entry.path[..separator];
            if !directories.contains(parent) {
                return Err(failure(PlatformFailureKind::Rejected));
            }
        }
    }
    Ok(())
}

fn join_relative(parent: &[u8], name: &[u8]) -> Result<Vec<u8>, PlatformFailure> {
    let mut path = Vec::with_capacity(parent.len() + usize::from(!parent.is_empty()) + name.len());
    path.extend_from_slice(parent);
    if !parent.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    validate_path(&path)?;
    Ok(path)
}

fn validate_path(path: &[u8]) -> Result<(), PlatformFailure> {
    if path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || path.contains(&0)
        || std::str::from_utf8(path).is_err()
    {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let components = path.split(|byte| *byte == b'/').collect::<Vec<_>>();
    if components.len() > MAX_DEPTH
        || components.iter().any(|component| {
            component.is_empty()
                || component.len() > MAX_COMPONENT_BYTES
                || *component == b"."
                || *component == b".."
        })
    {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    Ok(())
}

fn require_file_budget(size: u64, total: &mut u64) -> Result<(), PlatformFailure> {
    if size > MAX_FILE_BYTES {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    *total = total
        .checked_add(size)
        .filter(|total| *total <= MAX_TOTAL_FILE_BYTES)
        .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
    Ok(())
}

fn open_absolute_directory(path: &Path) -> Result<File, PlatformFailure> {
    if !path.is_absolute() {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")
        .map_err(|_| failure(PlatformFailureKind::Rejected))?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                let name = CString::new(name.as_encoded_bytes())
                    .map_err(|_| failure(PlatformFailureKind::Rejected))?;
                directory = openat_directory(directory.as_raw_fd(), &name)?;
            }
            _ => return Err(failure(PlatformFailureKind::Rejected)),
        }
    }
    Ok(directory)
}

fn openat_directory(parent: RawFd, name: &CStr) -> Result<File, PlatformFailure> {
    openat_file(parent, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)
}

fn openat_file(
    parent: RawFd,
    name: &CStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> Result<File, PlatformFailure> {
    // SAFETY: `name` is NUL-terminated, `parent` is an owned open directory,
    // and a successful returned descriptor is immediately adopted by `File`.
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            libc::c_uint::from(mode),
        )
    };
    if fd < 0 {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    // SAFETY: `fd` is newly returned and uniquely owned.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_relative_file(root_fd: RawFd, relative: &[u8]) -> Result<File, PlatformFailure> {
    let components = relative.split(|byte| *byte == b'/').collect::<Vec<_>>();
    let mut opened_directories = Vec::new();
    let mut parent = root_fd;
    for component in &components[..components.len().saturating_sub(1)] {
        let name = CString::new(*component).map_err(|_| failure(PlatformFailureKind::Rejected))?;
        let directory = openat_directory(parent, &name)?;
        parent = directory.as_raw_fd();
        opened_directories.push(directory);
    }
    let name = CString::new(
        *components
            .last()
            .ok_or_else(|| failure(PlatformFailureKind::Rejected))?,
    )
    .map_err(|_| failure(PlatformFailureKind::Rejected))?;
    openat_file(parent, &name, libc::O_RDONLY | libc::O_NONBLOCK, 0)
}

fn directory_names(fd: RawFd) -> Result<Vec<Vec<u8>>, PlatformFailure> {
    // Opening `.` through the exact dirfd creates an independent directory
    // stream offset; `dup` would share and consume the caller's offset.
    let dot = c".";
    // SAFETY: `fd` is an open directory and `dot` cannot escape it.
    let duplicate = unsafe {
        libc::openat(
            fd,
            dot.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if duplicate < 0 {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    // SAFETY: `duplicate` is uniquely owned. fdopendir adopts it on success.
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        // SAFETY: fdopendir did not adopt the descriptor on failure.
        unsafe { libc::close(duplicate) };
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let stream = DirectoryStream(stream);
    let mut names = Vec::new();
    loop {
        // SAFETY: `stream` is valid and exclusively used in this loop.
        unsafe { *libc::__error() = 0 };
        // SAFETY: readdir returns storage owned by `stream`, copied immediately.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            // SAFETY: __error returns the current thread's errno pointer.
            if unsafe { *libc::__error() } != 0 {
                return Err(failure(PlatformFailureKind::Rejected));
            }
            break;
        }
        // SAFETY: d_name is guaranteed NUL-terminated for a successful entry.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(name.to_vec());
        }
    }
    Ok(names)
}

struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: this object uniquely owns the DIR pointer.
        unsafe { libc::closedir(self.0) };
    }
}

fn fstat(fd: RawFd) -> Result<libc::stat, PlatformFailure> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `stat` points to enough writable storage.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    // SAFETY: fstat initialized the value on success.
    Ok(unsafe { stat.assume_init() })
}

fn fstatat_nofollow(parent: RawFd, name: &CStr) -> Result<libc::stat, PlatformFailure> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `name` and `stat` are valid; no symlink is followed.
    if unsafe {
        libc::fstatat(
            parent,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    // SAFETY: fstatat initialized the value on success.
    Ok(unsafe { stat.assume_init() })
}

fn require_same_object(expected: &libc::stat, actual: &libc::stat) -> Result<(), PlatformFailure> {
    if Observation::from_stat(expected) != Observation::from_stat(actual) {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    Ok(())
}

fn reject_resource_fork(fd: RawFd) -> Result<(), PlatformFailure> {
    let name = c"com.apple.ResourceFork";
    // SAFETY: this is a size query on a valid fd and static xattr name.
    let result = unsafe { libc::fgetxattr(fd, name.as_ptr(), std::ptr::null_mut(), 0, 0, 0) };
    if result >= 0 {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let error = std::io::Error::last_os_error().raw_os_error();
    if error == Some(libc::ENOATTR) {
        Ok(())
    } else {
        Err(failure(PlatformFailureKind::Rejected))
    }
}

fn mkdirat(parent: RawFd, name: &CStr) -> Result<(), PlatformFailure> {
    // SAFETY: `parent` and `name` are valid and mode is private owner-only.
    if unsafe { libc::mkdirat(parent, name.as_ptr(), 0o700) } != 0 {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    Ok(())
}

fn fchmod(fd: RawFd, mode: libc::mode_t) -> Result<(), PlatformFailure> {
    // SAFETY: chmod applies to the exact owned snapshot descriptor.
    if unsafe { libc::fchmod(fd, mode) } != 0 {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    Ok(())
}

fn file_type(mode: libc::mode_t) -> libc::mode_t {
    mode & libc::S_IFMT
}

fn darwin_user_temp_dir() -> Result<PathBuf, PlatformFailure> {
    // SAFETY: confstr is queried with a null buffer to obtain its required size.
    let required = unsafe { libc::confstr(DARWIN_USER_TEMP_DIR, std::ptr::null_mut(), 0) };
    if required <= 1 {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    let mut bytes = vec![0_u8; required];
    // SAFETY: `bytes` has exactly the requested writable capacity.
    let written =
        unsafe { libc::confstr(DARWIN_USER_TEMP_DIR, bytes.as_mut_ptr().cast(), bytes.len()) };
    if written != required || bytes.pop() != Some(0) {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    let path = fs::canonicalize(PathBuf::from(OsString::from_vec(bytes)))
        .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    let directory =
        open_absolute_directory(&path).map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    let metadata =
        fstat(directory.as_raw_fd()).map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    if file_type(metadata.st_mode) != libc::S_IFDIR || metadata.st_uid != unsafe { libc::geteuid() }
    {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    Ok(path)
}

#[cfg(test)]
pub(super) fn canonical_golden_digest() -> [u8; 32] {
    let bytes = golden_literal_for_test();
    Sha256::digest(bytes).into()
}

#[cfg(test)]
pub(super) fn golden_literal_for_test() -> Vec<u8> {
    let vector: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../transport/contracts/v1/vectors/ios-app-artifact-tree.json"
    ))
    .expect("checked-in D0 golden JSON");
    let hex = vector["expected"]["canonical_hex"]
        .as_str()
        .expect("D0 canonical hex");
    (0..hex.len())
        .step_by(2)
        .map(|offset| u8::from_str_radix(&hex[offset..offset + 2], 16).expect("D0 golden hex"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_and_payload_caps_match_d0_boundaries() {
        assert!(validate_path(&vec![b'x'; MAX_COMPONENT_BYTES]).is_ok());
        assert!(validate_path(&vec![b'x'; MAX_COMPONENT_BYTES + 1]).is_err());
        let depth_64 = std::iter::repeat_n("d", MAX_DEPTH)
            .collect::<Vec<_>>()
            .join("/");
        let depth_65 = std::iter::repeat_n("d", MAX_DEPTH + 1)
            .collect::<Vec<_>>()
            .join("/");
        assert!(validate_path(depth_64.as_bytes()).is_ok());
        assert!(validate_path(depth_65.as_bytes()).is_err());
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
            &[0xff],
        ] {
            assert!(validate_path(hostile).is_err());
        }
        let mut total = MAX_FILE_BYTES;
        assert!(require_file_budget(MAX_FILE_BYTES, &mut total).is_ok());
        assert_eq!(total, MAX_TOTAL_FILE_BYTES);
        assert!(require_file_budget(1, &mut total).is_err());
        let mut empty = 0;
        assert!(require_file_budget(MAX_FILE_BYTES + 1, &mut empty).is_err());
    }

    #[test]
    fn golden_literal_is_complete_not_a_manifest_digest() {
        let bytes = golden_literal_for_test();
        assert_eq!(bytes.len(), 716);
        assert!(bytes.starts_with(MAGIC));
        assert_eq!(
            u32::from_be_bytes(bytes[MAGIC.len()..MAGIC.len() + 4].try_into().unwrap()),
            9
        );
        assert_eq!(
            canonical_golden_digest(),
            [
                0xdd, 0xb5, 0xad, 0x55, 0xf2, 0xe9, 0xc3, 0x73, 0x4d, 0xd2, 0xf5, 0x2d, 0x4c, 0xb3,
                0x8e, 0x49, 0x18, 0x3d, 0x5f, 0x4d, 0x8a, 0xa4, 0x60, 0xd7, 0x33, 0x3e, 0xa3, 0xc3,
                0xe7, 0x83, 0xf4, 0xdf,
            ]
        );

        let vector: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../transport/contracts/v1/vectors/ios-app-artifact-tree.json"
        ))
        .unwrap();
        let entries = vector["entries"].as_array().unwrap();
        let mut encoded = Vec::new();
        emit_prefix(
            &mut |chunk| encoded.extend_from_slice(chunk),
            entries.len() as u32,
        );
        for entry in entries {
            let path = entry["path_utf8"].as_str().unwrap().as_bytes();
            let kind = if entry["kind"] == "directory" { 1 } else { 2 };
            let file = entry.get("file_hex").map(|value| {
                let hex = value.as_str().unwrap();
                (0..hex.len())
                    .step_by(2)
                    .map(|offset| u8::from_str_radix(&hex[offset..offset + 2], 16).unwrap())
                    .collect::<Vec<_>>()
            });
            emit_record_header(
                &mut |chunk| encoded.extend_from_slice(chunk),
                kind,
                path.len() as u32,
                path,
                entry["executable_class"].as_u64().unwrap() as u8,
                file.as_ref().map(|bytes| bytes.len() as u64),
            );
            if let Some(file) = file {
                encoded.extend_from_slice(&file);
            }
        }
        assert_eq!(encoded, bytes);
    }
}
