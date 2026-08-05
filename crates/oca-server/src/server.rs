use std::{
    fmt,
    fs::{self, OpenOptions},
    io::{self, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
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

/// The startup operation that rejected one candidate port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartupStage {
    Availability,
    Spawn,
    Readiness,
}

impl fmt::Display for StartupStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Availability => formatter.write_str("availability"),
            Self::Spawn => formatter.write_str("spawn"),
            Self::Readiness => formatter.write_str("readiness"),
        }
    }
}

/// A retained explanation for one rejected startup candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupDiagnostic {
    pub port: u16,
    pub stage: StartupStage,
    pub reason: String,
}

impl StartupDiagnostic {
    fn new(port: u16, stage: StartupStage, reason: impl Into<String>) -> Self {
        Self {
            port,
            stage,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for StartupDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "port {} failed during {}: {}",
            self.port, self.stage, self.reason
        )
    }
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

    /// Waits for a successful loopback connection, retaining the last failure.
    ///
    /// # Errors
    ///
    /// Returns the final concrete connection error when `timeout` expires.
    fn wait_until_ready(&self, port: u16, timeout: Duration) -> Result<(), String>;

    fn warn(&self, message: &str);
}

/// The production implementation of [`ServerRuntime`].
#[derive(Clone, Debug)]
pub struct SystemRuntime {
    executable: PathBuf,
    version: Arc<OnceLock<Result<String, String>>>,
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
            version: Arc::new(OnceLock::new()),
        }
    }

    fn probe_opencode_version(&self) -> Result<String, String> {
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
}

impl ServerRuntime for SystemRuntime {
    fn opencode_version(&self) -> Result<String, String> {
        self.version
            .get_or_init(|| self.probe_opencode_version())
            .clone()
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

    fn wait_until_ready(&self, port: u16, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let address = SocketAddr::new(LOOPBACK, port);
        loop {
            match TcpStream::connect_timeout(&address, Duration::from_millis(50)) {
                Ok(_) => return Ok(()),
                Err(error) if Instant::now() >= deadline => return Err(error.to_string()),
                Err(_) => {}
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
    /// damaged cache cannot prevent a fresh server from starting. This reader
    /// stays silent about the damage; [`Self::connect_or_start`] is what warns
    /// with the parse failure before recovering.
    ///
    /// # Errors
    ///
    /// Returns an error when the hint cannot be read for a reason other than
    /// absence or invalid JSON.
    pub fn read_record(&self) -> io::Result<Option<ServerRecord>> {
        match self.read_record_state()? {
            RecordRead::Found(record) => Ok(Some(record)),
            RecordRead::Absent | RecordRead::Corrupt(_) => Ok(None),
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
    /// Returns the request failure, a state I/O error, or typed startup
    /// diagnostics when every configured port fails.
    pub async fn connect_or_start<R, Q>(
        &self,
        runtime: &R,
        request: &mut Q,
    ) -> Result<Q::Output, ConnectError<Q::Error>>
    where
        R: ServerRuntime,
        Q: OpenCodeRequest,
    {
        let mut warned_about_corrupt_record = false;
        let initial_record = self
            .read_record_for_connect(runtime, &mut warned_about_corrupt_record)
            .map_err(ConnectError::State)?;
        if let Some(record) = initial_record.as_ref() {
            Self::warn_if_environment_mismatched(runtime, record);
            match Self::send(request, record.port).await {
                Ok(output) => return Ok(output),
                Err(RequestFailure::Connection(_)) => {}
                Err(failure) => return Err(ConnectError::from_failure(failure)),
            }
        }

        let lock = self.acquire_start_lock().map_err(ConnectError::State)?;
        let record_after_lock = self
            .read_record_for_connect(runtime, &mut warned_about_corrupt_record)
            .map_err(ConnectError::State)?;
        let recovery: Result<ServerRecord, ConnectError<Q::Error>> = match record_after_lock {
            Some(record) if Self::record_is_live(&record) => Ok(record),
            replaced_record => {
                match self
                    .start_record(runtime, replaced_record.as_ref())
                    .map_err(ConnectError::State)?
                {
                    StartOutcome::Started(record) => Ok(record),
                    StartOutcome::Failed(diagnostics) => Err(ConnectError::Startup(diagnostics)),
                }
            }
        };
        lock.unlock().map_err(ConnectError::State)?;
        let record = recovery?;

        Self::warn_if_environment_mismatched(runtime, &record);
        Self::send(request, record.port)
            .await
            .map_err(ConnectError::from_failure)
    }

    fn start_record<R>(
        &self,
        runtime: &R,
        replaced_record: Option<&ServerRecord>,
    ) -> io::Result<StartOutcome>
    where
        R: ServerRuntime,
    {
        let version = runtime.opencode_version().ok();
        if let (Some(version), Some(record)) = (&version, replaced_record)
            && version != &record.version
        {
            runtime.warn(&format!(
                "installed OpenCode version {version} differs from version {} recorded in server.json",
                record.version
            ));
        }
        let environment_hash = runtime.start_environment_hash();
        let mut diagnostics = Vec::new();
        for port in self.candidate_ports() {
            if !runtime.port_is_available(port) {
                diagnostics.push(StartupDiagnostic::new(
                    port,
                    StartupStage::Availability,
                    "loopback port is unavailable",
                ));
                continue;
            }
            if let Err(reason) = runtime.spawn(port, &self.log_path()) {
                diagnostics.push(StartupDiagnostic::new(port, StartupStage::Spawn, reason));
                continue;
            }
            if let Err(reason) = runtime.wait_until_ready(port, self.start_timeout) {
                diagnostics.push(StartupDiagnostic::new(
                    port,
                    StartupStage::Readiness,
                    reason,
                ));
                continue;
            }
            let record = ServerRecord::new(
                port,
                version.clone().unwrap_or_else(|| "unknown".to_owned()),
                environment_hash.clone(),
            );
            self.write_record(&record)?;
            for diagnostic in diagnostics {
                runtime.warn(&format!("discarded startup diagnostic: {diagnostic}"));
            }
            return Ok(StartOutcome::Started(record));
        }
        Ok(StartOutcome::Failed(diagnostics))
    }

    fn record_is_live(record: &ServerRecord) -> bool {
        let address = SocketAddr::new(LOOPBACK, record.port);
        TcpStream::connect_timeout(&address, Duration::from_millis(50)).is_ok()
    }

    fn read_record_state(&self) -> io::Result<RecordRead> {
        match fs::read(self.record_path()) {
            Ok(contents) => match serde_json::from_slice(&contents) {
                Ok(record) => Ok(RecordRead::Found(record)),
                Err(error) => Ok(RecordRead::Corrupt(error)),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(RecordRead::Absent),
            Err(error) => Err(error),
        }
    }

    fn read_record_for_connect<R>(
        &self,
        runtime: &R,
        warned_about_corrupt_record: &mut bool,
    ) -> io::Result<Option<ServerRecord>>
    where
        R: ServerRuntime,
    {
        match self.read_record_state()? {
            RecordRead::Found(record) => Ok(Some(record)),
            RecordRead::Absent => Ok(None),
            RecordRead::Corrupt(error) => {
                if !*warned_about_corrupt_record {
                    runtime.warn(&format!(
                        "ignoring corrupt discovery hint {}: {error}",
                        self.record_path().display()
                    ));
                    *warned_about_corrupt_record = true;
                }
                Ok(None)
            }
        }
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

    fn warn_if_environment_mismatched<R>(runtime: &R, record: &ServerRecord)
    where
        R: ServerRuntime,
    {
        let environment_mismatch = runtime.start_environment_hash() != record.environment_hash;
        if environment_mismatch {
            runtime.warn("server.json was created with a different OpenCode start environment");
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

enum StartOutcome {
    Started(ServerRecord),
    Failed(Vec<StartupDiagnostic>),
}

enum RecordRead {
    Absent,
    Found(ServerRecord),
    Corrupt(serde_json::Error),
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
    Startup(Vec<StartupDiagnostic>),
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

    use super::{
        ConnectError, ConnectOrStart, OpenCodeRequest, RequestFailure, ServerRecord, ServerRuntime,
        StartupStage, SystemRuntime,
    };

    struct Runtime {
        requests: Cell<u8>,
        version_probes: Cell<u8>,
    }

    impl ServerRuntime for Runtime {
        fn opencode_version(&self) -> Result<String, String> {
            self.version_probes.set(self.version_probes.get() + 1);
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
            version_probes: Cell::new(0),
        };
        let mut request = Request { runtime: &runtime };

        block_on(
            ConnectOrStart::new(directory.path(), 4096, [], Duration::from_millis(1))
                .connect_or_start(&runtime, &mut request),
        )
        .expect("warm request succeeds");

        assert_eq!(runtime.requests.get(), 1);
        assert_eq!(runtime.version_probes.get(), 0);
    }

    #[test]
    fn racing_cold_starts_spawn_once_and_the_loser_adopts_the_winner_record() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let port_reservation = reserve_loopback_port();
        let port = reserved_port(&port_reservation);
        let runtime = Arc::new(RacingRuntime::default());
        let first_directory = directory.path().to_path_buf();
        let first_runtime = Arc::clone(&runtime);
        drop(port_reservation);
        let first = std::thread::spawn(move || {
            let mut request = ReadyRequest;
            block_on(
                ConnectOrStart::new(first_directory, port, [], Duration::from_millis(50))
                    .connect_or_start(first_runtime.as_ref(), &mut request),
            )
        });
        while runtime.spawned.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }

        let mut request = ReadyRequest;
        block_on(
            ConnectOrStart::new(directory.path(), port, [], Duration::from_millis(50))
                .connect_or_start(runtime.as_ref(), &mut request),
        )
        .expect("loser adopts the winner");
        first.join().expect("winner thread").expect("winner starts");

        assert_eq!(runtime.spawned.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn racing_stale_field_equal_records_spawn_once_and_both_callers_recover() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let port_reservation = reserve_loopback_port();
        let port = reserved_port(&port_reservation);
        let manager = ConnectOrStart::new(directory.path(), port, [], Duration::from_millis(50));
        manager
            .write_record(&ServerRecord::new(port, "1.18.10", "environment"))
            .expect("stale field-equal discovery hint");
        let runtime = Arc::new(RacingRuntime::default());
        let barrier = Arc::new(Barrier::new(2));
        let first_directory = directory.path().to_path_buf();
        let first_runtime = Arc::clone(&runtime);
        let first_barrier = Arc::clone(&barrier);
        drop(port_reservation);
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
        let stale_port_reservation = reserve_loopback_port();
        let stale_port = reserved_port(&stale_port_reservation);
        let recovery_port_reservation = reserve_loopback_port();
        let recovery_port = reserved_port(&recovery_port_reservation);
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
        let runtime = Arc::new(RacingRuntime::default());
        let (failed_tx, failed_rx) = mpsc::channel();
        let worker_manager = manager.clone();
        let worker_runtime = Arc::clone(&runtime);
        drop((stale_port_reservation, recovery_port_reservation));
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
        assert_eq!(diagnostics[2].stage, StartupStage::Availability);
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
        let error = SystemRuntime::default()
            .wait_until_ready(0, Duration::ZERO)
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
    fn corrupt_discovery_hint_warns_once_and_attempts_one_fresh_start() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let manager = ConnectOrStart::new(directory.path(), 4096, [], Duration::from_millis(1));
        let hint_path = directory.path().join("server.json");
        std::fs::write(&hint_path, b"{").expect("corrupt discovery hint");
        let runtime = ColdRuntime::new([4096]);
        let mut request = ReadyRequest;

        block_on(manager.connect_or_start(&runtime, &mut request))
            .expect("corrupt hint recovers with a fresh start");

        assert_eq!(runtime.spawned.borrow().as_slice(), &[4096]);
        let warnings = runtime.warnings.borrow();
        assert_eq!(warnings.len(), 1, "one warning line, got {warnings:?}");
        assert!(warnings[0].contains(&hint_path.display().to_string()));
        assert!(warnings[0].contains("EOF while parsing an object"));
    }

    /// The control for the corrupt case above: an absent hint takes the same
    /// fresh-start path in silence, so the warning is what distinguishes
    /// corruption from absence.
    #[test]
    fn absent_discovery_hint_starts_fresh_without_any_warning() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let manager = ConnectOrStart::new(directory.path(), 4096, [], Duration::from_millis(1));
        let runtime = ColdRuntime::new([4096]);
        let mut request = ReadyRequest;

        block_on(manager.connect_or_start(&runtime, &mut request))
            .expect("absent hint starts fresh");

        assert_eq!(runtime.spawned.borrow().as_slice(), &[4096]);
        assert!(runtime.warnings.borrow().is_empty());
    }

    #[test]
    fn cold_start_warns_when_installed_version_differs_from_replaced_record() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let manager = ConnectOrStart::new(directory.path(), 4096, [], Duration::from_millis(1));
        manager
            .write_record(&ServerRecord::new(0, "1.18.10", "environment"))
            .expect("stale discovery hint");
        let runtime = ColdRuntime::with_version([4096], "1.19.0");
        let mut request = ConnectionFailureOnceRequest::default();

        block_on(manager.connect_or_start(&runtime, &mut request))
            .expect("cold recovery succeeds after the stale warm dispatch");

        assert_eq!(runtime.version_probes.get(), 1);
        let warnings = runtime.warnings.borrow();
        assert_eq!(warnings.len(), 1, "one warning line, got {warnings:?}");
        assert!(warnings[0].contains("installed OpenCode version 1.19.0"));
        assert!(warnings[0].contains("version 1.18.10 recorded in server.json"));
    }

    #[cfg(unix)]
    #[test]
    fn system_runtime_probes_during_cold_start_but_not_warm_dispatches() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary state directory");
        let executable = directory.path().join("counted-opencode");
        let probe_count = executable.with_extension("count");
        let serve_marker = executable.with_extension("serve");
        std::fs::write(
            &executable,
            concat!(
                "#!/bin/sh\n",
                "case \"$1\" in\n",
                "  --version) printf 'probe\\n' >> \"$0.count\"; printf '1.19.0\\n' ;;\n",
                "  serve) : > \"$0.serve\" ;;\n",
                "  *) exit 2 ;;\n",
                "esac\n",
            ),
        )
        .expect("counted executable");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
            .expect("counted executable permissions");

        let port_reservation = reserve_loopback_port();
        let port = reserved_port(&port_reservation);
        let (release_tx, release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !serve_marker.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "counted executable did not receive serve"
                );
                std::thread::yield_now();
            }
            let listener = TcpListener::bind(("127.0.0.1", port))
                .expect("test server claims the released port");
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
            drop(listener);
        });
        let runtime = RecordingSystemRuntime::new(SystemRuntime::new(&executable));
        let manager = ConnectOrStart::new(
            directory.path().join("state"),
            port,
            [],
            Duration::from_secs(2),
        );
        drop(port_reservation);

        let mut cold_request = ReadyRequest;
        block_on(manager.connect_or_start(&runtime, &mut cold_request))
            .expect("cold start succeeds");
        assert_eq!(probe_launches(&probe_count), 1);

        for _ in 0..3 {
            let mut warm_request = ReadyRequest;
            block_on(manager.connect_or_start(&runtime, &mut warm_request))
                .expect("warm dispatch succeeds");
        }
        assert_eq!(probe_launches(&probe_count), 1);

        manager
            .write_record(&ServerRecord::new(
                port,
                "1.18.10",
                runtime.start_environment_hash(),
            ))
            .expect("version-mismatched discovery hint");
        let mut mismatch_request = ReadyRequest;
        block_on(manager.connect_or_start(&runtime, &mut mismatch_request))
            .expect("version mismatch does not trigger a warm probe");

        assert_eq!(probe_launches(&probe_count), 1);
        assert!(runtime.warnings.borrow().is_empty());

        release_tx.send(()).expect("release test server");
        server.join().expect("test server thread");
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
            version_probes: Cell::new(0),
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
        version: String,
        version_probes: Cell<u8>,
        spawned: RefCell<Vec<u16>>,
        warnings: RefCell<Vec<String>>,
    }

    impl ColdRuntime {
        fn new(available: impl IntoIterator<Item = u16>) -> Self {
            Self::with_version(available, "1.18.10")
        }

        fn with_version(available: impl IntoIterator<Item = u16>, version: &str) -> Self {
            Self {
                available: available.into_iter().collect(),
                version: version.to_owned(),
                version_probes: Cell::new(0),
                spawned: RefCell::new(Vec::new()),
                warnings: RefCell::new(Vec::new()),
            }
        }
    }

    impl ServerRuntime for ColdRuntime {
        fn opencode_version(&self) -> Result<String, String> {
            self.version_probes.set(self.version_probes.get() + 1);
            Ok(self.version.clone())
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

        fn warn(&self, message: &str) {
            self.warnings.borrow_mut().push(message.to_owned());
        }
    }

    #[derive(Default)]
    struct RacingRuntime {
        spawned: AtomicU8,
        listeners: Mutex<Vec<TcpListener>>,
    }

    impl ServerRuntime for RacingRuntime {
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
        let initial_listener = TcpListener::bind(("127.0.0.1", 0)).expect("live initial record");
        let initial_port = initial_listener
            .local_addr()
            .expect("initial loopback address")
            .port();
        let replacement_listener = changed
            .then(|| TcpListener::bind(("127.0.0.1", 0)).expect("live changed replacement record"));
        let post_lock_port = replacement_listener
            .as_ref()
            .map_or(initial_port, |listener| {
                listener
                    .local_addr()
                    .expect("replacement loopback address")
                    .port()
            });
        if changed {
            assert_ne!(initial_port, post_lock_port, "record port must change");
        }
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
        manager
            .write_record(&ServerRecord::new(post_lock_port, "1.18.10", "environment"))
            .expect("post-lock record");
        lock.unlock().expect("release start lock");

        worker
            .join()
            .expect("recovery thread")
            .expect("live post-lock record is adopted");
        drop((initial_listener, replacement_listener));
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

    fn reserve_loopback_port() -> TcpListener {
        TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral loopback listener")
    }

    fn reserved_port(listener: &TcpListener) -> u16 {
        listener.local_addr().expect("loopback address").port()
    }

    #[cfg(unix)]
    struct RecordingSystemRuntime {
        inner: SystemRuntime,
        warnings: RefCell<Vec<String>>,
    }

    #[cfg(unix)]
    impl RecordingSystemRuntime {
        fn new(inner: SystemRuntime) -> Self {
            Self {
                inner,
                warnings: RefCell::new(Vec::new()),
            }
        }
    }

    #[cfg(unix)]
    impl ServerRuntime for RecordingSystemRuntime {
        fn opencode_version(&self) -> Result<String, String> {
            self.inner.opencode_version()
        }

        fn start_environment_hash(&self) -> String {
            self.inner.start_environment_hash()
        }

        fn port_is_available(&self, port: u16) -> bool {
            self.inner.port_is_available(port)
        }

        fn spawn(&self, port: u16, log_path: &Path) -> Result<(), String> {
            self.inner.spawn(port, log_path)
        }

        fn wait_until_ready(&self, port: u16, timeout: Duration) -> Result<(), String> {
            self.inner.wait_until_ready(port, timeout)
        }

        fn warn(&self, message: &str) {
            self.warnings.borrow_mut().push(message.to_owned());
        }
    }

    #[cfg(unix)]
    fn probe_launches(path: &Path) -> usize {
        std::fs::read_to_string(path)
            .expect("version probe count")
            .lines()
            .count()
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

    #[derive(Default)]
    struct ConnectionFailureOnceRequest {
        attempts: u8,
    }

    impl OpenCodeRequest for ConnectionFailureOnceRequest {
        type Output = ();
        type Error = &'static str;

        fn send(
            &mut self,
            _client: &oca_opencode::OpenCodeClient,
        ) -> impl Future<Output = Result<Self::Output, RequestFailure<Self::Error>>> + Send
        {
            self.attempts += 1;
            std::future::ready(if self.attempts == 1 {
                Err(RequestFailure::Connection("stale record"))
            } else {
                Ok(())
            })
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

    /// A warm-path runtime recording version probes and warning lines.
    #[derive(Default)]
    struct RecordingWarnRuntime {
        version: String,
        environment: String,
        version_probes: Cell<u8>,
        warnings: RefCell<Vec<String>>,
    }

    impl RecordingWarnRuntime {
        fn new(version: &str, environment: &str) -> Self {
            Self {
                version: version.to_owned(),
                environment: environment.to_owned(),
                version_probes: Cell::new(0),
                warnings: RefCell::new(Vec::new()),
            }
        }
    }

    impl ServerRuntime for RecordingWarnRuntime {
        fn opencode_version(&self) -> Result<String, String> {
            self.version_probes.set(self.version_probes.get() + 1);
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

    #[test]
    fn warm_dispatch_warns_on_environment_hash_mismatch_without_version_probe() {
        let runtime = RecordingWarnRuntime::new("1.19.0", "current-environment");

        let warnings = warnings_for_recorded_start(&runtime);

        assert_eq!(warnings.len(), 1, "one warning line, got {warnings:?}");
        assert!(warnings[0].contains("OpenCode start environment"));
        assert_eq!(runtime.version_probes.get(), 0);
    }

    #[test]
    fn warm_dispatch_ignores_recorded_version_without_probing() {
        let runtime = RecordingWarnRuntime::new("1.19.0", "recorded-environment");

        assert!(warnings_for_recorded_start(&runtime).is_empty());
        assert_eq!(runtime.version_probes.get(), 0);
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
