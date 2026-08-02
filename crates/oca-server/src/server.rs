use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use fs2::FileExt;
use oca_opencode::OpenCodeClient;
use oca_state::ServerConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

const SERVER_FILE: &str = "server.json";
const SERVER_LOCK: &str = "server.lock";
const SERVER_LOG: &str = "opencode.log";
const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The durable hint pointing at an `OpenCode` server started by `oca`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServerRecord {
    pub port: u16,
    pub version: String,
    pub environment_hash: String,
}

impl ServerRecord {
    #[must_use]
    pub fn new(port: u16, version: impl Into<String>, environment_hash: impl Into<String>) -> Self {
        Self {
            port,
            version: version.into(),
            environment_hash: environment_hash.into(),
        }
    }
}

/// The result of sending an application request through `OpenCode`.
///
/// The distinction between [`Self::Connection`] and [`Self::Transmitted`] is
/// intentional: only the former is safe for this layer to replay after a
/// server restart.
#[derive(Debug)]
pub enum RequestFailure<E> {
    /// A connection could not be made before any request bytes were sent.
    Connection(E),
    /// The request may have reached the server and must not be replayed.
    Transmitted(E),
    /// `OpenCode` received the request and returned an application failure.
    Application(E),
}

/// One real `OpenCode` command request.
///
/// Implementations call an operation on the supplied [`OpenCodeClient`]. The
/// manager never substitutes a health request for this operation on the warm
/// path.
pub trait OpenCodeRequest {
    type Output;
    type Error;

    fn send(
        &mut self,
        client: &OpenCodeClient,
    ) -> impl Future<Output = Result<Self::Output, RequestFailure<Self::Error>>> + Send;
}

/// Effects involved in locating and starting a server.
///
/// This seam keeps tests independent from a real `opencode` executable while
/// preserving the production process and socket behavior in [`SystemRuntime`].
pub trait ServerRuntime {
    /// Returns the installed `OpenCode` version.
    ///
    /// # Errors
    ///
    /// Returns an error when the version cannot be determined.
    fn opencode_version(&self) -> Result<String, String>;

    fn start_environment_hash(&self) -> String;

    fn port_is_available(&self, port: u16) -> bool;

    /// Starts a loopback-only server and redirects its output to `log_path`.
    ///
    /// # Errors
    ///
    /// Returns an error when the server process cannot be created.
    fn spawn(&self, port: u16, log_path: &Path) -> Result<(), String>;

    fn wait_until_ready(&self, port: u16, timeout: Duration) -> bool;

    fn warn(&self, message: &str);
}

/// The production implementation of [`ServerRuntime`].
#[derive(Clone, Debug)]
pub struct SystemRuntime {
    executable: PathBuf,
}

impl Default for SystemRuntime {
    fn default() -> Self {
        Self::new("opencode")
    }
}

impl SystemRuntime {
    #[must_use]
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
        }
    }
}

impl ServerRuntime for SystemRuntime {
    fn opencode_version(&self) -> Result<String, String> {
        let output = Command::new(&self.executable)
            .arg("--version")
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "`opencode --version` exited with {}",
                output.status
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn start_environment_hash(&self) -> String {
        default_start_environment_hash()
    }

    fn port_is_available(&self, port: u16) -> bool {
        TcpListener::bind(SocketAddr::new(LOOPBACK, port)).is_ok()
    }

    fn spawn(&self, port: u16, log_path: &Path) -> Result<(), String> {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        set_private_file_creation_mode(&mut options);
        let log = options.open(log_path).map_err(|error| error.to_string())?;
        set_private_file_permissions(log_path).map_err(|error| error.to_string())?;
        let error_log = log.try_clone().map_err(|error| error.to_string())?;
        Command::new(&self.executable)
            .args(["serve", "--hostname", "127.0.0.1", "--port"])
            .arg(port.to_string())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(error_log))
            .spawn()
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn wait_until_ready(&self, port: u16, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let address = SocketAddr::new(LOOPBACK, port);
        loop {
            if TcpStream::connect_timeout(&address, Duration::from_millis(50)).is_ok() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn warn(&self, message: &str) {
        eprintln!("warning: {message}");
    }
}

/// Hashes the environment values that affect how `opencode serve` starts.
#[must_use]
pub fn default_start_environment_hash() -> String {
    const KEYS: [&str; 5] = [
        "HOME",
        "OPENCODE_CONFIG",
        "OPENCODE_CONFIG_DIR",
        "PATH",
        "XDG_CONFIG_HOME",
    ];
    let mut hasher = Sha256::new();
    for key in KEYS {
        hasher.update(key.as_bytes());
        hasher.update([0]);
        if let Some(value) = std::env::var_os(key) {
            hasher.update(value.to_string_lossy().as_bytes());
        }
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

/// A server discovery manager rooted at one `~/.oca`-style state directory.
#[derive(Clone, Debug)]
pub struct ConnectOrStart {
    state_directory: PathBuf,
    primary_port: u16,
    alternate_ports: Vec<u16>,
    start_timeout: Duration,
}

impl ConnectOrStart {
    #[must_use]
    pub fn new<I>(
        state_directory: impl Into<PathBuf>,
        primary_port: u16,
        alternate_ports: I,
        start_timeout: Duration,
    ) -> Self
    where
        I: IntoIterator<Item = u16>,
    {
        Self {
            state_directory: state_directory.into(),
            primary_port,
            alternate_ports: alternate_ports.into_iter().collect(),
            start_timeout,
        }
    }

    #[must_use]
    pub fn from_home(home: impl AsRef<Path>, config: &ServerConfig) -> Self {
        Self::new(
            home.as_ref().join(".oca"),
            config.port,
            config.alt_ports.iter().copied(),
            Duration::from_millis(config.start_timeout_ms),
        )
    }

    /// Reads the current discovery hint. A malformed hint is ignored so a
    /// damaged cache cannot prevent a fresh server from starting.
    ///
    /// # Errors
    ///
    /// Returns an error when the hint cannot be read for a reason other than
    /// absence or invalid JSON.
    pub fn read_record(&self) -> io::Result<Option<ServerRecord>> {
        match fs::read(self.record_path()) {
            Ok(contents) => Ok(serde_json::from_slice(&contents).ok()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Atomically replaces the discovery hint after a server is ready.
    ///
    /// # Errors
    ///
    /// Returns an error when the state directory or durable hint cannot be
    /// written.
    pub fn write_record(&self, record: &ServerRecord) -> io::Result<()> {
        ensure_private_directory(&self.state_directory)?;
        let contents = serde_json::to_vec_pretty(record)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = self.state_directory.join(format!(
            ".{SERVER_FILE}.{}.{sequence}.tmp",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        set_private_file_creation_mode(&mut options);
        let mut file = options.open(&temporary)?;
        set_private_file_permissions(&temporary)?;
        file.write_all(&contents)?;
        file.sync_all()?;
        fs::rename(temporary, self.record_path())
    }

    /// Sends the actual command through a discovered server or starts one.
    ///
    /// On a warm record this issues no pre-flight request: it sends the caller's
    /// request directly. A connection failure is replayed only when the request
    /// explicitly reports that no bytes were transmitted.
    ///
    /// # Errors
    ///
    /// Returns the request failure, a state I/O error, or
    /// [`ConnectError::ServerUnavailable`] when every configured port fails.
    pub async fn connect_or_start<R, Q>(
        &self,
        runtime: &R,
        request: &mut Q,
    ) -> Result<Q::Output, ConnectError<Q::Error>>
    where
        R: ServerRuntime,
        Q: OpenCodeRequest,
    {
        let initial_record = self.read_record().map_err(ConnectError::State)?;
        if let Some(record) = initial_record.as_ref() {
            Self::warn_if_mismatched(runtime, record);
            match Self::send(request, record.port).await {
                Ok(output) => return Ok(output),
                Err(RequestFailure::Connection(_)) => {}
                Err(failure) => return Err(ConnectError::from_failure(failure)),
            }
        }

        let lock = self.acquire_start_lock().map_err(ConnectError::State)?;
        let record_after_lock = self.read_record().map_err(ConnectError::State)?;
        let record = if initial_record.as_ref() == record_after_lock.as_ref() {
            self.start_record(runtime).map_err(ConnectError::State)?
        } else {
            record_after_lock
        };
        lock.unlock().map_err(ConnectError::State)?;

        if let Some(record) = record {
            Self::warn_if_mismatched(runtime, &record);
            return Self::send(request, record.port)
                .await
                .map_err(ConnectError::from_failure);
        }
        Err(ConnectError::ServerUnavailable)
    }

    fn start_record<R>(&self, runtime: &R) -> io::Result<Option<ServerRecord>>
    where
        R: ServerRuntime,
    {
        let version = runtime.opencode_version().ok();
        let environment_hash = runtime.start_environment_hash();
        for port in self.candidate_ports() {
            if !runtime.port_is_available(port) {
                continue;
            }
            if runtime.spawn(port, &self.log_path()).is_err()
                || !runtime.wait_until_ready(port, self.start_timeout)
            {
                continue;
            }
            let record = ServerRecord::new(
                port,
                version.clone().unwrap_or_else(|| "unknown".to_owned()),
                environment_hash.clone(),
            );
            self.write_record(&record)?;
            return Ok(Some(record));
        }
        Ok(None)
    }

    async fn send<Q>(request: &mut Q, port: u16) -> Result<Q::Output, RequestFailure<Q::Error>>
    where
        Q: OpenCodeRequest,
    {
        let url = Url::parse(&format!("http://127.0.0.1:{port}"))
            .expect("a loopback URL constructed from a u16 is valid");
        request.send(&OpenCodeClient::new(url)).await
    }

    fn acquire_start_lock(&self) -> io::Result<std::fs::File> {
        ensure_private_directory(&self.state_directory)?;
        let lock_path = self.lock_path();
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        set_private_file_creation_mode(&mut options);
        let lock = options.open(&lock_path)?;
        set_private_file_permissions(&lock_path)?;
        lock.lock_exclusive()?;
        Ok(lock)
    }

    fn warn_if_mismatched<R>(runtime: &R, record: &ServerRecord)
    where
        R: ServerRuntime,
    {
        let version_mismatch = runtime
            .opencode_version()
            .is_ok_and(|version| version != record.version);
        let environment_mismatch = runtime.start_environment_hash() != record.environment_hash;
        if version_mismatch || environment_mismatch {
            runtime.warn(
                "server.json was created with a different OpenCode version or start environment",
            );
        }
    }

    fn candidate_ports(&self) -> impl Iterator<Item = u16> + '_ {
        std::iter::once(self.primary_port).chain(self.alternate_ports.iter().copied())
    }

    fn record_path(&self) -> PathBuf {
        self.state_directory.join(SERVER_FILE)
    }

    fn lock_path(&self) -> PathBuf {
        self.state_directory.join(SERVER_LOCK)
    }

    fn log_path(&self) -> PathBuf {
        self.state_directory.join(SERVER_LOG)
    }
}

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    set_private_directory_permissions(path)
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_creation_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_private_file_creation_mode(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Failure returned by [`ConnectOrStart::connect_or_start`].
#[derive(Debug)]
pub enum ConnectError<E> {
    Request(E),
    RequestMayHaveBeenTransmitted(E),
    ServerUnavailable,
    State(io::Error),
}

impl<E> ConnectError<E> {
    fn from_failure(failure: RequestFailure<E>) -> Self {
        match failure {
            RequestFailure::Connection(error) | RequestFailure::Application(error) => {
                Self::Request(error)
            }
            RequestFailure::Transmitted(error) => Self::RequestMayHaveBeenTransmitted(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        future::Future,
        net::TcpListener,
        path::Path,
        sync::{
            Arc, Barrier, Mutex,
            atomic::{AtomicU8, Ordering},
            mpsc,
        },
        task::{Context, Poll, Waker},
        time::Duration,
    };

    use fs2::FileExt;

    use super::{
        ConnectError, ConnectOrStart, OpenCodeRequest, RequestFailure, ServerRecord, ServerRuntime,
        StartupStage, SystemRuntime,
    };

    struct Runtime {
        requests: Cell<u8>,
    }

    impl ServerRuntime for Runtime {
        fn opencode_version(&self) -> Result<String, String> {
            Ok("1.18.10".to_owned())
        }

        fn start_environment_hash(&self) -> String {
            "environment".to_owned()
        }

        fn port_is_available(&self, _port: u16) -> bool {
            true
        }

        fn spawn(&self, _port: u16, _log_path: &Path) -> Result<(), String> {
            panic!("the warm path must not spawn")
        }

        fn wait_until_ready(&self, _port: u16, _timeout: Duration) -> Result<(), String> {
            panic!("the warm path must not probe")
        }

        fn warn(&self, _message: &str) {}
    }

    struct Request<'a> {
        runtime: &'a Runtime,
    }

    impl OpenCodeRequest for Request<'_> {
        type Output = ();
        type Error = String;

        fn send(
            &mut self,
            _client: &oca_opencode::OpenCodeClient,
        ) -> impl Future<Output = Result<Self::Output, RequestFailure<Self::Error>>> + Send
        {
            self.runtime.requests.set(self.runtime.requests.get() + 1);
            std::future::ready(Ok(()))
        }
    }

    #[test]
    fn warm_path_sends_only_the_real_request_without_a_preflight_probe() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        ConnectOrStart::new(directory.path(), 4096, [], Duration::from_millis(1))
            .write_record(&ServerRecord::new(4096, "1.18.10", "environment"))
            .expect("discovery hint");
        let runtime = Runtime {
            requests: Cell::new(0),
        };
        let mut request = Request { runtime: &runtime };

        block_on(
            ConnectOrStart::new(directory.path(), 4096, [], Duration::from_millis(1))
                .connect_or_start(&runtime, &mut request),
        )
        .expect("warm request succeeds");

        assert_eq!(runtime.requests.get(), 1);
    }

    #[test]
    fn racing_cold_starts_spawn_once_and_the_loser_adopts_the_winner_record() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let port = available_loopback_port();
        let manager = ConnectOrStart::new(directory.path(), port, [], Duration::from_millis(50));
        manager
            .write_record(&ServerRecord::new(port, "1.18.10", "environment"))
            .expect("stale field-equal discovery hint");
        let runtime = Arc::new(LoopbackRacingRuntime::default());
        let barrier = Arc::new(Barrier::new(2));
        let first_directory = directory.path().to_path_buf();
        let first_runtime = Arc::clone(&runtime);
        let first_barrier = Arc::clone(&barrier);
        let first = std::thread::spawn(move || {
            let mut request = StaleOnceRequest::new(first_barrier);
            block_on(
                ConnectOrStart::new(first_directory, port, [], Duration::from_millis(50))
                    .connect_or_start(first_runtime.as_ref(), &mut request),
            )
        });
        let mut request = StaleOnceRequest::new(barrier);
        let second = block_on(manager.connect_or_start(runtime.as_ref(), &mut request));

        second.expect("second caller recovers");
        first.join().expect("winner thread").expect("winner starts");

        assert_eq!(runtime.spawned.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn equal_post_lock_record_is_adopted_only_when_its_port_is_live() {
        post_lock_record_is_adopted_when_live(false);
    }

    #[test]
    fn changed_post_lock_record_is_adopted_only_when_its_port_is_live() {
        post_lock_record_is_adopted_when_live(true);
    }

    #[test]
    fn dead_post_lock_record_is_not_adopted_and_starts_recovery() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let stale_port = available_loopback_port();
        let recovery_port = available_loopback_port();
        let manager = ConnectOrStart::new(
            directory.path(),
            recovery_port,
            [],
            Duration::from_millis(50),
        );
        manager
            .write_record(&ServerRecord::new(stale_port, "1.18.10", "environment"))
            .expect("stale discovery hint");
        let lock = manager.acquire_start_lock().expect("held start lock");
        let runtime = Arc::new(LoopbackRacingRuntime::default());
        let (failed_tx, failed_rx) = mpsc::channel();
        let worker_manager = manager.clone();
        let worker_runtime = Arc::clone(&runtime);
        let worker = std::thread::spawn(move || {
            let mut request = SignalledStaleRequest::new(failed_tx);
            block_on(worker_manager.connect_or_start(worker_runtime.as_ref(), &mut request))
        });

        failed_rx.recv().expect("initial request failed");
        manager
            .write_record(&ServerRecord::new(stale_port, "1.18.10", "environment"))
            .expect("still-dead field-equal record");
        lock.unlock().expect("release start lock");

        worker
            .join()
            .expect("recovery thread")
            .expect("dead record triggers recovery");
        assert_eq!(runtime.spawned.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn startup_error_retains_every_candidate_diagnostic() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let runtime = DiagnosticRuntime::all_fail();
        let mut request = ReadyRequest;

        let error = block_on(
            ConnectOrStart::new(directory.path(), 4096, [4097, 4098], Duration::ZERO)
                .connect_or_start(&runtime, &mut request),
        )
        .expect_err("all startup candidates fail");

        let ConnectError::Startup(diagnostics) = error else {
            panic!("expected typed startup diagnostics")
        };
        assert_eq!(diagnostics.len(), 3);
        assert_eq!(diagnostics[0].port, 4096);
        assert_eq!(diagnostics[0].stage, StartupStage::Spawn);
        assert_eq!(diagnostics[0].reason, "permission denied");
        assert_eq!(diagnostics[1].port, 4097);
        assert_eq!(diagnostics[1].stage, StartupStage::Readiness);
        assert_eq!(diagnostics[1].reason, "final connect error: refused");
        assert_eq!(diagnostics[2].port, 4098);
        assert_eq!(diagnostics[2].stage, StartupStage::Spawn);
        assert!(diagnostics[2].reason.contains("unavailable"));
    }

    #[test]
    fn successful_alternate_warns_with_the_discarded_primary_diagnostic() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let runtime = DiagnosticRuntime::alternate_succeeds();
        let mut request = ReadyRequest;

        block_on(
            ConnectOrStart::new(directory.path(), 4096, [4097], Duration::ZERO)
                .connect_or_start(&runtime, &mut request),
        )
        .expect("alternate starts");

        let warnings = runtime.warnings.borrow();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("4096"));
        assert!(warnings[0].contains("spawn"));
        assert!(warnings[0].contains("permission denied"));
    }

    #[test]
    fn system_readiness_timeout_returns_its_final_connection_error() {
        let port = available_loopback_port();

        let error = SystemRuntime::default()
            .wait_until_ready(port, Duration::ZERO)
            .expect_err("unused port never becomes ready");

        assert!(!error.is_empty());
    }

    #[test]
    fn occupied_primary_port_falls_back_without_touching_the_existing_listener() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let runtime = ColdRuntime::new([4097]);
        let mut request = ReadyRequest;

        block_on(
            ConnectOrStart::new(directory.path(), 4096, [4097], Duration::from_millis(1))
                .connect_or_start(&runtime, &mut request),
        )
        .expect("alternate port starts");

        assert_eq!(runtime.spawned.borrow().as_slice(), &[4097]);
        assert_eq!(
            ConnectOrStart::new(directory.path(), 4096, [4097], Duration::from_millis(1))
                .read_record()
                .expect("record read")
                .expect("record written")
                .port,
            4097
        );
    }

    #[test]
    fn possibly_transmitted_requests_are_not_replayed_or_restarted() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let manager = ConnectOrStart::new(directory.path(), 4096, [], Duration::from_millis(1));
        manager
            .write_record(&ServerRecord::new(4096, "1.18.10", "environment"))
            .expect("discovery hint");
        let runtime = Runtime {
            requests: Cell::new(0),
        };
        let mut request = TransmittedRequest;

        let error = block_on(manager.connect_or_start(&runtime, &mut request))
            .expect_err("transmitted request must not be replayed");

        assert!(matches!(
            error,
            ConnectError::RequestMayHaveBeenTransmitted(_)
        ));
        assert_eq!(runtime.requests.get(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn discovery_hint_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary state directory");
        let state_directory = directory.path().join(".oca");
        let manager = ConnectOrStart::new(&state_directory, 4096, [], Duration::from_millis(1));
        manager
            .write_record(&ServerRecord::new(4096, "1.18.10", "environment"))
            .expect("discovery hint");

        assert_eq!(
            std::fs::metadata(&state_directory)
                .expect("state directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(state_directory.join("server.json"))
                .expect("hint metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    struct ColdRuntime {
        available: Vec<u16>,
        spawned: RefCell<Vec<u16>>,
    }

    impl ColdRuntime {
        fn new(available: impl IntoIterator<Item = u16>) -> Self {
            Self {
                available: available.into_iter().collect(),
                spawned: RefCell::new(Vec::new()),
            }
        }
    }

    impl ServerRuntime for ColdRuntime {
        fn opencode_version(&self) -> Result<String, String> {
            Ok("1.18.10".to_owned())
        }

        fn start_environment_hash(&self) -> String {
            "environment".to_owned()
        }

        fn port_is_available(&self, port: u16) -> bool {
            self.available.contains(&port)
        }

        fn spawn(&self, port: u16, log_path: &Path) -> Result<(), String> {
            assert_eq!(
                log_path.file_name().and_then(|name| name.to_str()),
                Some("opencode.log")
            );
            self.spawned.borrow_mut().push(port);
            Ok(())
        }

        fn wait_until_ready(&self, _port: u16, _timeout: Duration) -> Result<(), String> {
            Ok(())
        }

        fn warn(&self, _message: &str) {}
    }

    #[derive(Default)]
    struct LoopbackRacingRuntime {
        spawned: AtomicU8,
        listeners: Mutex<Vec<TcpListener>>,
    }

    impl ServerRuntime for LoopbackRacingRuntime {
        fn opencode_version(&self) -> Result<String, String> {
            Ok("1.18.10".to_owned())
        }

        fn start_environment_hash(&self) -> String {
            "environment".to_owned()
        }

        fn port_is_available(&self, _port: u16) -> bool {
            true
        }

        fn spawn(&self, port: u16, _log_path: &Path) -> Result<(), String> {
            self.spawned.fetch_add(1, Ordering::SeqCst);
            self.listeners
                .lock()
                .expect("listener lock")
                .push(TcpListener::bind(("127.0.0.1", port)).map_err(|error| error.to_string())?);
            Ok(())
        }

        fn wait_until_ready(&self, _port: u16, _timeout: Duration) -> Result<(), String> {
            Ok(())
        }

        fn warn(&self, _message: &str) {}
    }

    struct StaleOnceRequest {
        first_failure: Arc<Barrier>,
        attempts: u8,
    }

    impl StaleOnceRequest {
        fn new(first_failure: Arc<Barrier>) -> Self {
            Self {
                first_failure,
                attempts: 0,
            }
        }
    }

    impl OpenCodeRequest for StaleOnceRequest {
        type Output = ();
        type Error = &'static str;

        fn send(
            &mut self,
            _client: &oca_opencode::OpenCodeClient,
        ) -> impl Future<Output = Result<Self::Output, RequestFailure<Self::Error>>> + Send
        {
            self.attempts += 1;
            if self.attempts == 1 {
                self.first_failure.wait();
                std::future::ready(Err(RequestFailure::Connection("stale record")))
            } else {
                std::future::ready(Ok(()))
            }
        }
    }

    struct SignalledStaleRequest {
        failed: Option<mpsc::Sender<()>>,
    }

    impl SignalledStaleRequest {
        fn new(failed: mpsc::Sender<()>) -> Self {
            Self {
                failed: Some(failed),
            }
        }
    }

    impl OpenCodeRequest for SignalledStaleRequest {
        type Output = ();
        type Error = &'static str;

        fn send(
            &mut self,
            _client: &oca_opencode::OpenCodeClient,
        ) -> impl Future<Output = Result<Self::Output, RequestFailure<Self::Error>>> + Send
        {
            if let Some(failed) = self.failed.take() {
                failed.send(()).expect("signal initial failure");
                std::future::ready(Err(RequestFailure::Connection("stale record")))
            } else {
                std::future::ready(Ok(()))
            }
        }
    }

    struct NeverSpawnRuntime;

    impl ServerRuntime for NeverSpawnRuntime {
        fn opencode_version(&self) -> Result<String, String> {
            Ok("1.18.10".to_owned())
        }

        fn start_environment_hash(&self) -> String {
            "environment".to_owned()
        }

        fn port_is_available(&self, _port: u16) -> bool {
            panic!("a live post-lock record must be adopted")
        }

        fn spawn(&self, _port: u16, _log_path: &Path) -> Result<(), String> {
            panic!("a live post-lock record must be adopted")
        }

        fn wait_until_ready(&self, _port: u16, _timeout: Duration) -> Result<(), String> {
            panic!("a live post-lock record must be adopted")
        }

        fn warn(&self, _message: &str) {}
    }

    fn post_lock_record_is_adopted_when_live(changed: bool) {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let initial_port = available_loopback_port();
        let post_lock_port = if changed {
            available_loopback_port()
        } else {
            initial_port
        };
        let manager = ConnectOrStart::new(
            directory.path(),
            initial_port,
            [],
            Duration::from_millis(50),
        );
        manager
            .write_record(&ServerRecord::new(initial_port, "1.18.10", "environment"))
            .expect("initial stale record");
        let lock = manager.acquire_start_lock().expect("held start lock");
        let runtime = Arc::new(NeverSpawnRuntime);
        let (failed_tx, failed_rx) = mpsc::channel();
        let worker_manager = manager.clone();
        let worker_runtime = Arc::clone(&runtime);
        let worker = std::thread::spawn(move || {
            let mut request = SignalledStaleRequest::new(failed_tx);
            block_on(worker_manager.connect_or_start(worker_runtime.as_ref(), &mut request))
        });

        failed_rx.recv().expect("initial request failed");
        let listener = TcpListener::bind(("127.0.0.1", post_lock_port)).expect("live replacement");
        manager
            .write_record(&ServerRecord::new(post_lock_port, "1.18.10", "environment"))
            .expect("post-lock record");
        lock.unlock().expect("release start lock");

        worker
            .join()
            .expect("recovery thread")
            .expect("live post-lock record is adopted");
        drop(listener);
    }

    struct DiagnosticRuntime {
        alternate_succeeds: bool,
        warnings: RefCell<Vec<String>>,
    }

    impl DiagnosticRuntime {
        fn all_fail() -> Self {
            Self {
                alternate_succeeds: false,
                warnings: RefCell::new(Vec::new()),
            }
        }

        fn alternate_succeeds() -> Self {
            Self {
                alternate_succeeds: true,
                warnings: RefCell::new(Vec::new()),
            }
        }
    }

    impl ServerRuntime for DiagnosticRuntime {
        fn opencode_version(&self) -> Result<String, String> {
            Ok("1.18.10".to_owned())
        }

        fn start_environment_hash(&self) -> String {
            "environment".to_owned()
        }

        fn port_is_available(&self, port: u16) -> bool {
            port != 4098
        }

        fn spawn(&self, port: u16, _log_path: &Path) -> Result<(), String> {
            if port == 4096 {
                Err("permission denied".to_owned())
            } else {
                Ok(())
            }
        }

        fn wait_until_ready(&self, port: u16, _timeout: Duration) -> Result<(), String> {
            if port == 4097 && !self.alternate_succeeds {
                Err("final connect error: refused".to_owned())
            } else {
                Ok(())
            }
        }

        fn warn(&self, message: &str) {
            self.warnings.borrow_mut().push(message.to_owned());
        }
    }

    fn available_loopback_port() -> u16 {
        TcpListener::bind(("127.0.0.1", 0))
            .expect("ephemeral loopback listener")
            .local_addr()
            .expect("loopback address")
            .port()
    }

    struct ReadyRequest;

    impl OpenCodeRequest for ReadyRequest {
        type Output = ();
        type Error = String;

        fn send(
            &mut self,
            _client: &oca_opencode::OpenCodeClient,
        ) -> impl Future<Output = Result<Self::Output, RequestFailure<Self::Error>>> + Send
        {
            std::future::ready(Ok(()))
        }
    }

    struct TransmittedRequest;

    impl OpenCodeRequest for TransmittedRequest {
        type Output = ();
        type Error = &'static str;

        fn send(
            &mut self,
            _client: &oca_opencode::OpenCodeClient,
        ) -> impl Future<Output = Result<Self::Output, RequestFailure<Self::Error>>> + Send
        {
            std::future::ready(Err(RequestFailure::Transmitted("connection closed")))
        }
    }

    /// A runtime whose installed version and start environment both differ from
    /// whatever is recorded, recording every warning it is asked to print.
    #[derive(Default)]
    struct RecordingWarnRuntime {
        version: String,
        environment: String,
        warnings: RefCell<Vec<String>>,
    }

    impl RecordingWarnRuntime {
        fn new(version: &str, environment: &str) -> Self {
            Self {
                version: version.to_owned(),
                environment: environment.to_owned(),
                warnings: RefCell::new(Vec::new()),
            }
        }
    }

    impl ServerRuntime for RecordingWarnRuntime {
        fn opencode_version(&self) -> Result<String, String> {
            Ok(self.version.clone())
        }

        fn start_environment_hash(&self) -> String {
            self.environment.clone()
        }

        fn port_is_available(&self, _port: u16) -> bool {
            true
        }

        fn spawn(&self, _port: u16, _log_path: &Path) -> Result<(), String> {
            panic!("a warm record must not spawn")
        }

        fn wait_until_ready(&self, _port: u16, _timeout: Duration) -> Result<(), String> {
            panic!("a warm record must not probe")
        }

        fn warn(&self, message: &str) {
            self.warnings.borrow_mut().push(message.to_owned());
        }
    }

    fn warnings_for_recorded_start(runtime: &RecordingWarnRuntime) -> Vec<String> {
        let directory = tempfile::tempdir().expect("temporary state directory");
        ConnectOrStart::new(directory.path(), 4096, [], Duration::from_millis(1))
            .write_record(&ServerRecord::new(4096, "1.18.10", "recorded-environment"))
            .expect("discovery hint");
        let mut request = ReadyRequest;

        block_on(
            ConnectOrStart::new(directory.path(), 4096, [], Duration::from_millis(1))
                .connect_or_start(runtime, &mut request),
        )
        .expect("a mismatch warns but never fails the request");

        runtime.warnings.borrow().clone()
    }

    /// T14 (#14), criterion 5: a recorded version or start-environment mismatch
    /// prints one warning line, and the request still goes through.
    #[test]
    fn a_recorded_version_or_environment_mismatch_prints_exactly_one_warning_line() {
        for runtime in [
            RecordingWarnRuntime::new("1.19.0", "recorded-environment"),
            RecordingWarnRuntime::new("1.18.10", "current-environment"),
            RecordingWarnRuntime::new("1.19.0", "current-environment"),
        ] {
            let warnings = warnings_for_recorded_start(&runtime);

            assert_eq!(warnings.len(), 1, "one warning line, got {warnings:?}");
            assert!(
                warnings[0].contains("version") || warnings[0].contains("environment"),
                "the warning names what drifted: {}",
                warnings[0]
            );
        }
    }

    /// T14 (#14), criterion 5: a record that matches the current environment
    /// warns about nothing, so the warning cannot be unconditional noise.
    #[test]
    fn a_matching_server_record_prints_no_warning_line() {
        let runtime = RecordingWarnRuntime::new("1.18.10", "recorded-environment");

        assert!(warnings_for_recorded_start(&runtime).is_empty());
    }

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        let mut future = std::pin::pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }
}
