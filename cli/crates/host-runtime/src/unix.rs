use std::{
    collections::HashSet,
    ffi::CString,
    fs::{self, File},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

const RUNTIME_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimePaths {
    pub runtime_dir: PathBuf,
    pub lock_file: PathBuf,
    pub control_socket: PathBuf,
}
impl RuntimePaths {
    pub fn current_user() -> io::Result<Self> {
        Self::under(&darwin_user_temp_dir()?)
    }
    fn under(base: &Path) -> io::Result<Self> {
        let runtime_dir = base.join("apppilotkit").join("broker-v1");
        Ok(Self {
            lock_file: runtime_dir.join("broker.lock"),
            control_socket: runtime_dir.join("control.sock"),
            runtime_dir,
        })
    }
    pub fn connect_verified(&self) -> io::Result<UnixStream> {
        self.connect_for_euid(unsafe { libc::geteuid() })
    }
    fn connect_for_euid(&self, expected: u32) -> io::Result<UnixStream> {
        let stream = UnixStream::connect(&self.control_socket)?;
        verify_peer_euid(&stream, expected)?;
        Ok(stream)
    }
}

pub struct BrokerInstance {
    paths: RuntimePaths,
    runtime_dir: File,
    _lock: File,
    _process_guard: ProcessInstanceGuard,
    listener: UnixListener,
    socket_identity: SocketIdentity,
    euid: u32,
    owns_lock: bool,
}

struct ProcessInstanceGuard {
    runtime_identity: (u64, u64),
}

static PROCESS_INSTANCES: OnceLock<Mutex<HashSet<(u64, u64)>>> = OnceLock::new();

impl Drop for ProcessInstanceGuard {
    fn drop(&mut self) {
        if let Ok(mut instances) = PROCESS_INSTANCES
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
        {
            instances.remove(&self.runtime_identity);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    file_type: u32,
    device: u64,
    inode: u64,
    euid: u32,
    mode: u32,
}
impl FileIdentity {
    fn from_stat(metadata: &libc::stat) -> Self {
        Self {
            file_type: u32::from(metadata.st_mode & libc::S_IFMT),
            device: metadata.st_dev as u64,
            inode: metadata.st_ino,
            euid: metadata.st_uid,
            mode: u32::from(metadata.st_mode & 0o777),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SocketIdentity {
    listener: FileIdentity,
    path: FileIdentity,
    bound_path: PathBuf,
}
impl BrokerInstance {
    pub fn acquire_current_user() -> io::Result<Self> {
        Self::acquire(RuntimePaths::current_user()?)
    }
    fn acquire(paths: RuntimePaths) -> io::Result<Self> {
        let euid = unsafe { libc::geteuid() };
        let base = paths
            .runtime_dir
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "runtime base missing"))?;
        let base_dir = open_absolute_dir(base)?;
        verify_owned_dir_fd(&base_dir, euid, RUNTIME_MODE)?;
        let app_dir = open_or_create_owned_dir(&base_dir, "apppilotkit", euid)?;
        let runtime_dir = open_or_create_owned_dir(&app_dir, "broker-v1", euid)?;
        let process_guard = reserve_process_instance(&runtime_dir)?;
        let lock = open_lock_file(&runtime_dir, euid)?;
        lock_exclusive_nonblocking(&lock)?;
        if let Some(metadata) = stat_at(&runtime_dir, "control.sock")? {
            if metadata.st_uid != euid || metadata.st_mode & libc::S_IFMT != libc::S_IFSOCK {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "unowned control socket",
                ));
            }
            unlink_at(&runtime_dir, "control.sock")?;
        }
        let listener = UnixListener::bind(&paths.control_socket)?;
        fs::set_permissions(
            &paths.control_socket,
            fs::Permissions::from_mode(PRIVATE_FILE_MODE),
        )?;
        let fd_metadata = fstat_fd(listener.as_raw_fd())?;
        let path_metadata = stat_at(&runtime_dir, "control.sock")?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "control socket disappeared"))?;
        let listener_identity = FileIdentity::from_stat(&fd_metadata);
        let path_identity = FileIdentity::from_stat(&path_metadata);
        let bound_path = listener
            .local_addr()?
            .as_pathname()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unnamed control socket"))?
            .to_path_buf();
        if listener_identity.file_type != u32::from(libc::S_IFSOCK)
            || listener_identity.euid != euid
            || path_metadata.st_mode & libc::S_IFMT != libc::S_IFSOCK
            || path_metadata.st_uid != euid
            || u32::from(path_metadata.st_mode & 0o777) != PRIVATE_FILE_MODE
            || bound_path != paths.control_socket
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe control socket",
            ));
        }
        let socket_identity = SocketIdentity {
            listener: listener_identity,
            path: path_identity,
            bound_path,
        };
        Ok(Self {
            paths,
            runtime_dir,
            _lock: lock,
            _process_guard: process_guard,
            listener,
            socket_identity,
            euid,
            owns_lock: true,
        })
    }
    pub fn paths(&self) -> &RuntimePaths {
        &self.paths
    }
    pub fn accept_verified(&self) -> io::Result<UnixStream> {
        self.accept_for_euid(self.euid)
    }
    /// Configures the private listener for a polling accept loop.
    ///
    /// The production Broker uses this while it waits for its async-signal-safe
    /// termination flag, so SIGTERM delivered to another thread cannot leave
    /// the accepting thread blocked indefinitely.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.listener.set_nonblocking(nonblocking)
    }
    fn accept_for_euid(&self, expected: u32) -> io::Result<UnixStream> {
        let (stream, _) = self.listener.accept()?;
        verify_peer_euid(&stream, expected)?;
        Ok(stream)
    }
}
impl Drop for BrokerInstance {
    fn drop(&mut self) {
        let listener_matches = fstat_fd(self.listener.as_raw_fd())
            .ok()
            .is_some_and(|metadata| {
                FileIdentity::from_stat(&metadata) == self.socket_identity.listener
            })
            && self
                .listener
                .local_addr()
                .ok()
                .and_then(|address| address.as_pathname().map(Path::to_path_buf))
                .as_ref()
                == Some(&self.socket_identity.bound_path);
        let path_matches = stat_at(&self.runtime_dir, "control.sock")
            .ok()
            .flatten()
            .is_some_and(|metadata| {
                FileIdentity::from_stat(&metadata) == self.socket_identity.path
                    && metadata.st_mode & libc::S_IFMT == libc::S_IFSOCK
                    && metadata.st_uid == self.euid
                    && u32::from(metadata.st_mode & 0o777) == PRIVATE_FILE_MODE
            });
        if self.owns_lock && listener_matches && path_matches {
            let _ = unlink_at(&self.runtime_dir, "control.sock");
        }
    }
}

fn open_absolute_dir(path: &Path) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "absolute path required",
        ));
    }
    let mut current = open_dir_at(libc::AT_FDCWD, "/")?;
    for component in path.components() {
        let std::path::Component::Normal(name) = component else {
            continue;
        };
        current = open_dir_at(current.as_raw_fd(), &name.to_string_lossy())?;
    }
    Ok(current)
}

fn open_or_create_owned_dir(parent: &File, name: &str, euid: u32) -> io::Result<File> {
    match open_dir_at(parent.as_raw_fd(), name) {
        Ok(directory) => {
            verify_owned_dir_fd(&directory, euid, RUNTIME_MODE)?;
            Ok(directory)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let name = c_name(name)?;
            let result = unsafe {
                libc::mkdirat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    RUNTIME_MODE as libc::mode_t,
                )
            };
            if result == -1 {
                return Err(io::Error::last_os_error());
            }
            let directory = open_dir_at(parent.as_raw_fd(), name.to_str().expect("ASCII"))?;
            let chmod =
                unsafe { libc::fchmod(directory.as_raw_fd(), RUNTIME_MODE as libc::mode_t) };
            if chmod == -1 {
                return Err(io::Error::last_os_error());
            }
            verify_owned_dir_fd(&directory, euid, RUNTIME_MODE)?;
            Ok(directory)
        }
        Err(error) if error.kind() == io::ErrorKind::NotADirectory => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime component is not a directory",
        )),
        Err(error) => Err(error),
    }
}

fn open_dir_at(parent_fd: libc::c_int, name: &str) -> io::Result<File> {
    let name = c_name(name)?;
    let fd = unsafe {
        libc::openat(
            parent_fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn open_lock_file(runtime_dir: &File, euid: u32) -> io::Result<File> {
    let name = c_name("broker.lock")?;
    let fd = unsafe {
        libc::openat(
            runtime_dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            PRIVATE_FILE_MODE as libc::c_uint,
        )
    };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = fstat_fd(file.as_raw_fd())?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG
        || metadata.st_uid != euid
        || u32::from(metadata.st_mode & 0o777) != PRIVATE_FILE_MODE
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe lock file",
        ));
    }
    Ok(file)
}

fn verify_owned_dir_fd(file: &File, euid: u32, mode: u32) -> io::Result<()> {
    let metadata = fstat_fd(file.as_raw_fd())?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFDIR
        || metadata.st_uid != euid
        || u32::from(metadata.st_mode & 0o777) != mode
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe runtime directory",
        ));
    }
    Ok(())
}

fn fstat_fd(fd: libc::c_int) -> io::Result<libc::stat> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, metadata.as_mut_ptr()) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { metadata.assume_init() })
    }
}

fn stat_at(parent: &File, name: &str) -> io::Result<Option<libc::stat>> {
    let name = c_name(name)?;
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(Some(unsafe { metadata.assume_init() }))
    } else {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error)
        }
    }
}

fn unlink_at(parent: &File, name: &str) -> io::Result<()> {
    let name = c_name(name)?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn c_name(value: &str) -> io::Result<CString> {
    CString::new(value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}
fn lock_exclusive_nonblocking(file: &File) -> io::Result<()> {
    let mut contract_lock: libc::flock = unsafe { std::mem::zeroed() };
    contract_lock.l_type = libc::F_WRLCK as _;
    contract_lock.l_whence = libc::SEEK_SET as _;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &contract_lock) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn reserve_process_instance(runtime_dir: &File) -> io::Result<ProcessInstanceGuard> {
    let metadata = fstat_fd(runtime_dir.as_raw_fd())?;
    let runtime_identity = (metadata.st_dev as u64, metadata.st_ino);
    let mut instances = PROCESS_INSTANCES
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .map_err(|_| io::Error::other("process instance registry poisoned"))?;
    if !instances.insert(runtime_identity) {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "broker already acquired in this process",
        ));
    }
    Ok(ProcessInstanceGuard { runtime_identity })
}
fn verify_peer_euid(stream: &UnixStream, expected: u32) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let mut uid = 0;
        let mut gid = 0;
        let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        if uid != expected {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "peer euid mismatch",
            ));
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (stream, expected);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "getpeereid requires Darwin",
        ))
    }
}
fn darwin_user_temp_dir() -> io::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        const CS_DARWIN_USER_TEMP_DIR: libc::c_int = 65_537;
        let length = unsafe { libc::confstr(CS_DARWIN_USER_TEMP_DIR, std::ptr::null_mut(), 0) };
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut bytes = vec![0_u8; length];
        let written = unsafe {
            libc::confstr(
                CS_DARWIN_USER_TEMP_DIR,
                bytes.as_mut_ptr().cast(),
                bytes.len(),
            )
        };
        if written != length {
            return Err(io::Error::last_os_error());
        }
        let value = std::ffi::CStr::from_bytes_until_nul(&bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid Darwin temp dir"))?;
        let path =
            PathBuf::from(value.to_str().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "non-UTF8 Darwin temp dir")
            })?);
        fs::canonicalize(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Darwin runtime directory required",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::symlink,
        time::{SystemTime, UNIX_EPOCH},
    };
    fn temporary_base(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = PathBuf::from("/private/tmp")
            .join(format!("apk-{label}-{}-{nonce:x}", std::process::id()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(RUNTIME_MODE)).unwrap();
        path
    }
    #[test]
    fn owned_instance_uses_private_modes_and_only_removes_its_socket() {
        let base = temporary_base("owned");
        let paths = RuntimePaths::under(&base).unwrap();
        let instance = BrokerInstance::acquire(paths.clone()).unwrap();
        assert_eq!(
            fs::metadata(&paths.runtime_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&paths.lock_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&paths.control_socket)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(instance);
        assert!(!paths.control_socket.exists());
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn same_process_cannot_acquire_a_second_broker_instance() {
        let base = temporary_base("same-process-singleton");
        let paths = RuntimePaths::under(&base).unwrap();
        let instance = BrokerInstance::acquire(paths.clone()).unwrap();
        let pinned = stat_at(&instance.runtime_dir, "control.sock")
            .unwrap()
            .map(|metadata| FileIdentity::from_stat(&metadata))
            .expect("first socket identity");

        let error = BrokerInstance::acquire(paths.clone())
            .err()
            .expect("second acquisition must fail");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(
            stat_at(&instance.runtime_dir, "control.sock")
                .unwrap()
                .map(|metadata| FileIdentity::from_stat(&metadata)),
            Some(pinned)
        );

        drop(instance);
        assert!(!paths.control_socket.exists());
        fs::remove_dir_all(base).unwrap();
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn frozen_fcntl_lock_rejects_a_distinct_process() {
        let base = temporary_base("fcntl-contract");
        let paths = RuntimePaths::under(&base).unwrap();
        let instance = BrokerInstance::acquire(paths.clone()).unwrap();
        let lock_path = CString::new(paths.lock_file.as_os_str().as_encoded_bytes()).unwrap();

        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork failed");
        if child == 0 {
            let fd = unsafe { libc::open(lock_path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
            if fd == -1 {
                unsafe { libc::_exit(2) };
            }
            let mut lock: libc::flock = unsafe { std::mem::zeroed() };
            lock.l_type = libc::F_WRLCK as _;
            lock.l_whence = libc::SEEK_SET as _;
            let result = unsafe { libc::fcntl(fd, libc::F_SETLK, &lock) };
            let error = io::Error::last_os_error().raw_os_error();
            let expected_conflict =
                result == -1 && matches!(error, Some(libc::EACCES) | Some(libc::EAGAIN));
            unsafe { libc::_exit(if expected_conflict { 0 } else { 1 }) };
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);

        drop(instance);
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn drop_uses_pinned_runtime_dir_after_path_component_replacement() {
        let base = temporary_base("replaced");
        let paths = RuntimePaths::under(&base).unwrap();
        let instance = BrokerInstance::acquire(paths.clone()).unwrap();
        let original = base.join("original-runtime");
        fs::rename(&paths.runtime_dir, &original).unwrap();
        fs::create_dir(&paths.runtime_dir).unwrap();
        fs::set_permissions(&paths.runtime_dir, fs::Permissions::from_mode(RUNTIME_MODE)).unwrap();
        let replacement = UnixListener::bind(&paths.control_socket).unwrap();
        drop(instance);
        assert!(paths.control_socket.exists());
        assert!(!original.join("control.sock").exists());
        drop(replacement);
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn drop_does_not_remove_a_socket_whose_mode_changed() {
        let base = temporary_base("socket-mode");
        let paths = RuntimePaths::under(&base).unwrap();
        let instance = BrokerInstance::acquire(paths.clone()).unwrap();
        fs::set_permissions(&paths.control_socket, fs::Permissions::from_mode(0o666)).unwrap();
        drop(instance);
        assert!(paths.control_socket.exists());
        fs::remove_dir_all(base).unwrap();
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_listener_fd_and_path_have_distinct_kernel_identity_but_exact_path_is_pinned() {
        let base = temporary_base("darwin-identity");
        let paths = RuntimePaths::under(&base).unwrap();
        let instance = BrokerInstance::acquire(paths.clone()).unwrap();
        let listener = FileIdentity::from_stat(
            &fstat_fd(instance.listener.as_raw_fd()).expect("listener fstat"),
        );
        let path = FileIdentity::from_stat(
            &stat_at(&instance.runtime_dir, "control.sock")
                .expect("path fstatat")
                .expect("socket path"),
        );
        assert_ne!((listener.device, listener.inode), (path.device, path.inode));
        assert_eq!(
            instance.listener.local_addr().unwrap().as_pathname(),
            Some(paths.control_socket.as_path())
        );
        drop(instance);
        assert!(!paths.control_socket.exists());
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn symlinked_runtime_component_is_rejected_without_deletion() {
        let base = temporary_base("symlink");
        let outside = temporary_base("outside");
        symlink(&outside, base.join("apppilotkit")).unwrap();
        let paths = RuntimePaths::under(&base).unwrap();
        let error = BrokerInstance::acquire(paths)
            .err()
            .expect("symlink rejected");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(outside.exists());
        fs::remove_file(base.join("apppilotkit")).unwrap();
        fs::remove_dir_all(base).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }
    #[test]
    fn non_socket_control_path_is_rejected_without_deletion() {
        let base = temporary_base("regular-control");
        let paths = RuntimePaths::under(&base).unwrap();
        fs::create_dir_all(&paths.runtime_dir).unwrap();
        fs::set_permissions(
            base.join("apppilotkit"),
            fs::Permissions::from_mode(RUNTIME_MODE),
        )
        .unwrap();
        fs::set_permissions(&paths.runtime_dir, fs::Permissions::from_mode(RUNTIME_MODE)).unwrap();
        File::create(&paths.control_socket).unwrap();
        fs::set_permissions(
            &paths.control_socket,
            fs::Permissions::from_mode(PRIVATE_FILE_MODE),
        )
        .unwrap();
        let error = BrokerInstance::acquire(paths.clone())
            .err()
            .expect("regular file rejected");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(paths.control_socket.is_file());
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn nonblocking_broker_listener_reports_would_block_without_a_peer() {
        let base = temporary_base("nonblocking-listener");
        let paths = RuntimePaths::under(&base).unwrap();
        let instance = BrokerInstance::acquire(paths).unwrap();
        instance.set_nonblocking(true).unwrap();
        assert_eq!(
            instance
                .accept_verified()
                .expect_err("no peer is ready")
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(instance);
        fs::remove_dir_all(base).unwrap();
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn unix_peer_euid_is_verified_with_real_kernel_credentials() {
        let (left, right) = UnixStream::pair().unwrap();
        verify_peer_euid(&left, unsafe { libc::geteuid() }).unwrap();
        let error = verify_peer_euid(&right, unsafe { libc::geteuid() }.saturating_add(1))
            .expect_err("wrong euid");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn verified_accept_and_connect_are_the_only_control_stream_entry_points() {
        let base = temporary_base("verified-control");
        let paths = RuntimePaths::under(&base).unwrap();
        let instance = BrokerInstance::acquire(paths.clone()).unwrap();
        std::thread::scope(|scope| {
            let client = scope.spawn(|| paths.connect_verified());
            instance.accept_verified().expect("verified server peer");
            client
                .join()
                .expect("client thread")
                .expect("verified broker peer");

            let rejected = scope
                .spawn(|| paths.connect_for_euid(unsafe { libc::geteuid() }.saturating_add(1)));
            instance
                .accept_verified()
                .expect("server accepts real peer");
            assert_eq!(
                rejected
                    .join()
                    .expect("rejected client thread")
                    .expect_err("wrong peer euid")
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
        });
        drop(instance);
        fs::remove_dir_all(base).unwrap();
    }
}
