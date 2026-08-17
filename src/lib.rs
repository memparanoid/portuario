//! Concurrency-safe free-port picking for test suites that spawn real servers.
//!
//! `portuario` hands out ports that are verified free and reserved against
//! every other `portuario` user on the machine (e.g. parallel `cargo nextest`
//! processes) at the same instant:
//!
//! 1. A candidate port is drawn at random from a range that sits *below* the
//!    ephemeral port range of every mainstream OS, so the kernel never hands
//!    these ports to outgoing connections behind your back.
//! 2. An OS advisory lock on `<lock_dir>/<port>.lock` is taken **first**: a
//!    candidate already locked by another picker is skipped without ever
//!    binding it, so pickers never trample a port its owner is about to use.
//! 3. Only while holding the lock is the candidate verified — bound on TCP
//!    and UDP over IPv4 and IPv6 wildcards — and only then are the probe
//!    sockets dropped and the locked [`Port`] returned. Reservation and
//!    freeness check overlap, with no window in between.
//!
//! The advisory lock ([`std::fs::File::try_lock`]) is owned by the kernel and
//! tied to the process: it is released the moment the process exits, even on
//! panic or `SIGKILL`. Orphaned `.lock` files are therefore inert — they are
//! never deleted and never need cleaning up, because deleting lock files that
//! another process may have already opened is a classic double-lock race.
//!
//! ```no_run
//! let port = portuario::pick_port().expect("no free port");
//! let url = format!("127.0.0.1:{port}");
//! // spawn your server on `port`; the reservation is dropped
//! // (best effort) when `port` goes out of scope, or explicitly:
//! port.release().expect("release failed");
//! ```

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::net::{Ipv6Addr, SocketAddr, TcpListener, UdpSocket};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

use socket2::{Domain, Socket, Type};

/// Default candidate range, chosen to sit below the ephemeral port range of
/// Linux (32768..=60999), macOS and Windows (49152..=65535): the kernel never
/// assigns these ports to outgoing connections.
const DEFAULT_RANGE: Range<u16> = 15000..25000;
const DEFAULT_MAX_ATTEMPTS: u32 = 1_000;

/// Snapshot of the OS environment, taken when the [`Picker`] is built.
///
/// Dependency inversion over `std::env`: `pick()` consults this map instead of
/// the process environment, so tests can hand a [`Picker`] an empty map and
/// reach the branches that fire when a variable is absent — impossible to do
/// through the real environment, which cargo always populates.
#[derive(Debug, Clone, Default)]
struct OsEnvMap(HashMap<OsString, OsString>);

impl OsEnvMap {
    fn from_os_env() -> Self {
        Self(std::env::vars_os().collect())
    }

    fn get(&self, key: &str) -> Option<OsString> {
        self.0.get(OsStr::new(key)).cloned()
    }
}

/// Picks a free port using the [`Picker`] defaults.
///
/// Shorthand for `Picker::new().pick()`.
///
/// # Errors
///
/// See [`Picker::pick`].
pub fn pick_port() -> Result<Port, PickError> {
    Picker::new().pick()
}

/// A reserved free port.
///
/// While this value is alive, the OS advisory lock on `<lock_dir>/<port>.lock`
/// is held and no other `portuario` user will be handed the same port.
/// Dropping it releases the lock best-effort (the kernel releases it when the
/// lock file's descriptor closes — including on panic or process death); use
/// [`Port::release`] to release explicitly and observe failures.
#[must_use = "the reservation is released the moment this Port is dropped — \
              bind the value and keep it alive while the port is in use"]
pub struct Port {
    value: u16,
    lock: File,
}

impl Port {
    /// The reserved port number.
    ///
    /// Never call this on a temporary — `pick_port()?.value()` compiles
    /// without warnings, but the `Port` drops at the end of the statement and
    /// the lock is released immediately, leaving a number with no
    /// reservation behind it. Bind the guard first and keep it alive while
    /// the port is in use:
    ///
    /// ```no_run
    /// let port = portuario::pick_port()?;       // guard lives...
    /// let addr = format!("127.0.0.1:{port}");
    /// // ...until the end of scope, where the reservation is released
    /// # Ok::<(), portuario::PickError>(())
    /// ```
    #[must_use]
    pub fn value(&self) -> u16 {
        self.value
    }

    /// Releases the reservation explicitly, reporting failures.
    ///
    /// The lock file itself is intentionally left in place: unlinking lock
    /// files invites double-lock races for processes that already opened them.
    ///
    /// # Errors
    ///
    /// Propagates the error returned by the OS when releasing the lock.
    pub fn release(self) -> io::Result<()> {
        self.lock.unlock()
    }
}

impl fmt::Display for Port {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.value)
    }
}

impl fmt::Debug for Port {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Port").field("value", &self.value).finish()
    }
}

/// Error returned by [`Picker::pick`] / [`pick_port`].
#[derive(Debug)]
pub enum PickError {
    /// No lock directory was configured and none could be derived from the
    /// environment.
    ///
    /// Every process coordinating over ports must resolve the *same*
    /// directory, so instead of falling back to a guess that could silently
    /// diverge between subprocesses, picking refuses to run.
    NoLockDir,
    /// An I/O error while preparing the lock directory or lock file.
    Io(io::Error),
    /// Every attempt found its candidate port either bound or already locked.
    Exhausted {
        /// Number of candidate ports that were tried.
        attempts: u32,
    },
}

impl fmt::Display for PickError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoLockDir => write!(
                f,
                "cannot determine a lock directory: CARGO_MANIFEST_DIR is unset \
                 (not running under cargo?) — set Picker::lock_dir explicitly"
            ),
            Self::Io(err) => write!(f, "port picking failed: {err}"),
            Self::Exhausted { attempts } => {
                write!(f, "no free port found after {attempts} attempts")
            }
        }
    }
}

impl std::error::Error for PickError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::NoLockDir | Self::Exhausted { .. } => None,
        }
    }
}

impl From<io::Error> for PickError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Configurable port picker.
///
/// The defaults ([`Picker::new`]) are right for `cargo nextest` suites: the
/// [`DEFAULT_RANGE`] candidate range, locks in `<workspace root>/target/portuario`
/// of the consuming project (derived from `CARGO_MANIFEST_DIR` at pick time),
/// and IPv6 checking auto-detected from the host.
#[derive(Debug, Clone)]
pub struct Picker {
    range: Range<u16>,
    lock_dir: Option<PathBuf>,
    max_attempts: u32,
    ipv6: bool,
    env: OsEnvMap,
}

impl Default for Picker {
    fn default() -> Self {
        Self {
            range: DEFAULT_RANGE,
            lock_dir: None,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            ipv6: ipv6_available(),
            env: OsEnvMap::from_os_env(),
        }
    }
}

impl Picker {
    /// Creates a picker with the default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the candidate port range.
    ///
    /// Keep it below the OS ephemeral range (see [`DEFAULT_RANGE`]) unless the
    /// host reserves the ports via `net.ipv4.ip_local_reserved_ports`.
    #[must_use]
    pub fn range(mut self, range: Range<u16>) -> Self {
        self.range = range;
        self
    }

    /// Sets the directory holding the `<port>.lock` files.
    ///
    /// Everyone coordinating over the same ports must resolve the same
    /// directory. When unset, it is derived deterministically from
    /// `CARGO_MANIFEST_DIR` at pick time; if that is impossible, picking
    /// fails with [`PickError::NoLockDir`] rather than guessing.
    #[must_use]
    pub fn lock_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.lock_dir = Some(dir.into());
        self
    }

    /// Sets how many candidate ports to try before giving up.
    #[must_use]
    pub fn max_attempts(mut self, attempts: u32) -> Self {
        self.max_attempts = attempts;
        self
    }

    /// Forces IPv6 checking on or off (auto-detected by default).
    ///
    /// When enabled, a candidate must also be bindable on `[::]` for both TCP
    /// and UDP.
    #[must_use]
    pub fn ipv6(mut self, ipv6: bool) -> Self {
        self.ipv6 = ipv6;
        self
    }

    /// Picks a free port.
    ///
    /// # Errors
    ///
    /// - [`PickError::NoLockDir`] reports that no lock directory was
    ///   configured and none could be derived from `CARGO_MANIFEST_DIR`.
    /// - [`PickError::Io`] propagates failures creating the lock directory or
    ///   opening a lock file.
    /// - [`PickError::Exhausted`] reports that every attempt found its
    ///   candidate either bound by someone else or already locked.
    ///
    /// # Panics
    ///
    /// Panics if the configured range is empty.
    pub fn pick(&self) -> Result<Port, PickError> {
        assert!(!self.range.is_empty(), "port range must not be empty");

        let lock_dir =
            resolve_lock_dir(self.lock_dir.as_deref(), self.env.get("CARGO_MANIFEST_DIR"))?;
        fs::create_dir_all(&lock_dir)?;

        let mut rng = Rng::from_entropy();

        for _ in 0..self.max_attempts {
            let candidate = rng.pick_in(&self.range);
            let lock = File::options()
                .create(true)
                .truncate(false)
                .write(true)
                .open(lock_dir.join(format!("{candidate}.lock")))?;
            // Lock first, verify second: a candidate owned by someone else is
            // skipped without ever binding it, so pickers never trample the
            // port its owner is about to bind. `try_lock` failures other than
            // `WouldBlock` are vanishingly rare and always mean "this
            // candidate is unusable" — discarding is correct for both, so
            // they share one arm.
            if lock.try_lock().is_err() {
                continue;
            }
            // Verified while the lock is held; dropping `lock` on the failing
            // path releases the reservation for whoever comes next.
            if bind_all(candidate, self.ipv6) && probes_are_gone(candidate, self.ipv6) {
                return Ok(Port {
                    value: candidate,
                    lock,
                });
            }
        }

        Err(PickError::Exhausted {
            attempts: self.max_attempts,
        })
    }
}

/// Verifies each stack in its own scope, closing one probe before opening the
/// next. Keeping all four sockets alive together needlessly widens the window
/// in which a concurrently forked child can inherit a probe before `exec`.
fn bind_all(port: u16, ipv6: bool) -> bool {
    if TcpListener::bind(("0.0.0.0", port)).is_err() {
        return false;
    }
    if UdpSocket::bind(("0.0.0.0", port)).is_err() {
        return false;
    }
    if ipv6 && bind_v6(port, Type::STREAM).is_none() {
        return false;
    }
    if ipv6 && bind_v6(port, Type::DGRAM).is_none() {
        return false;
    }

    true
}

/// Waits for forked children to release inherited verification sockets.
///
/// `FD_CLOEXEC` closes a descriptor at `exec`, not at `fork`: another thread
/// can fork while a probe is live, inherit it, and keep the port bound briefly
/// after this process drops its copy. Reading the kernel's socket tables does
/// not create another descriptor that could repeat the race.
#[cfg(target_os = "linux")]
fn probes_are_gone(port: u16, ipv6: bool) -> bool {
    const IPV4_TABLES: [&str; 2] = ["/proc/net/tcp", "/proc/net/udp"];
    const ALL_TABLES: [&str; 4] = [
        "/proc/net/tcp",
        "/proc/net/tcp6",
        "/proc/net/udp",
        "/proc/net/udp6",
    ];
    const DEADLINE: Duration = Duration::from_millis(10);

    let started = Instant::now();
    let tables: &[&str] = if ipv6 { &ALL_TABLES } else { &IPV4_TABLES };

    loop {
        let wanted = format!(":{port:04X}");
        let still_bound = tables.iter().any(|table| {
            fs::read_to_string(table).is_ok_and(|rows| {
                rows.lines().skip(1).any(|row| {
                    row.split_whitespace()
                        .nth(1)
                        .is_some_and(|address| address.ends_with(&wanted))
                })
            })
        });

        if !still_bound {
            return true;
        }
        if started.elapsed() >= DEADLINE {
            return false;
        }

        std::thread::yield_now();
    }
}

#[cfg(not(target_os = "linux"))]
fn probes_are_gone(_port: u16, _ipv6: bool) -> bool {
    true
}

/// Binds `[::]:port` with `IPV6_V6ONLY` set, so the socket never claims the
/// IPv4 wildcard too (Linux binds dual-stack by default, which would collide
/// with the IPv4 sockets we already hold).
fn bind_v6(port: u16, socket_type: Type) -> Option<Socket> {
    // Uncovered: creating an AF_INET6 socket only fails (EAFNOSUPPORT) on
    // hosts with IPv6 disabled at the kernel — where this branch is what
    // makes ipv6_available() return false. On IPv6 hosts it cannot fire, and
    // a test can't unmake the host's IPv6 stack without a root netns.
    let socket = Socket::new(Domain::IPV6, socket_type, None).ok()?;
    let addr = SocketAddr::from((Ipv6Addr::UNSPECIFIED, port));

    // Uncovered: setsockopt(IPV6_V6ONLY) on a freshly created, unbound
    // AF_INET6 socket can only fail on EBADF/ENOTSOCK (we own the fd) or on
    // OSes predating the option — unreachable from any caller input. If it
    // ever fired, the candidate is discarded conservatively, which is the
    // correct reaction anyway.
    socket.set_only_v6(true).ok()?;
    socket.bind(&addr.into()).ok()?;

    Some(socket)
}

fn ipv6_available() -> bool {
    bind_v6(0, Type::STREAM).is_some()
}

/// The lock directory must resolve identically in every coordinating
/// process: an explicit dir wins, otherwise it is derived from the consuming
/// project's `CARGO_MANIFEST_DIR` (set by cargo and nextest for test
/// processes) as `<workspace root>/target/portuario`. With neither, resolution
/// fails — a guessed fallback could silently diverge between subprocesses.
fn resolve_lock_dir(
    explicit: Option<&Path>,
    manifest_dir: Option<OsString>,
) -> Result<PathBuf, PickError> {
    if let Some(dir) = explicit {
        return Ok(dir.to_path_buf());
    }
    let manifest_dir = manifest_dir.ok_or(PickError::NoLockDir)?;

    Ok(workspace_root(&PathBuf::from(manifest_dir))
        .join("target")
        .join("portuario"))
}

/// Nearest ancestor holding a `Cargo.lock` — the workspace root. In a
/// workspace, `CARGO_MANIFEST_DIR` names the member crate, but its siblings
/// run tests concurrently under one `cargo nextest` invocation and must all
/// coordinate over the same lock directory (next to the shared `target/`).
fn workspace_root(manifest_dir: &Path) -> PathBuf {
    manifest_dir
        .ancestors()
        .find(|dir| dir.join("Cargo.lock").exists())
        .unwrap_or(manifest_dir)
        .to_path_buf()
}

/// `xorshift64*` — enough randomness to spread parallel pickers across the
/// range without pulling in a dependency.
struct Rng(u64);

impl Rng {
    fn from_entropy() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;

        Self(((u64::from(std::process::id()) << 32) ^ nanos) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn pick_in(&mut self, range: &Range<u16>) -> u16 {
        let span = u64::from(range.end - range.start);
        range.start + (self.next() % span) as u16
    }
}
