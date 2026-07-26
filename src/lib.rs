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
//! // spawn your server/container on `port`; the reservation is dropped
//! // (best effort) when `port` goes out of scope, or explicitly:
//! port.release().expect("release failed");
//! ```

#[cfg(test)]
mod tests;

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::net::{Ipv6Addr, SocketAddr, TcpListener, UdpSocket};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use socket2::{Domain, Socket, Type};

/// Default candidate range, chosen to sit below the ephemeral port range of
/// Linux (32768..=60999), macOS and Windows (49152..=65535): the kernel never
/// assigns these ports to outgoing connections.
const DEFAULT_RANGE: Range<u16> = 15000..25000;
const DEFAULT_MAX_ATTEMPTS: u32 = 100;

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
pub struct Port {
    value: u16,
    lock: File,
}

impl Port {
    /// The reserved port number.
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
}

impl Default for Picker {
    fn default() -> Self {
        Self {
            range: DEFAULT_RANGE,
            lock_dir: None,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            ipv6: ipv6_available(),
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

        // Uncovered: cargo and nextest always set CARGO_MANIFEST_DIR for test
        // processes, so this `?` can only fire when the consumer runs outside
        // cargo. The branch is covered directly in resolve_lock_dir's tests.
        let lock_dir = resolve_lock_dir(
            self.lock_dir.as_deref(),
            std::env::var_os("CARGO_MANIFEST_DIR"),
        )?;
        fs::create_dir_all(&lock_dir)?;

        let mut rng = Rng::from_entropy();

        for _ in 0..self.max_attempts {
            let value = rng.pick_in(&self.range);
            let lock = File::options()
                .create(true)
                .truncate(false)
                .write(true)
                .open(lock_dir.join(format!("{value}.lock")))?;
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
            if bind_all(value, self.ipv6).is_some() {
                return Ok(Port { value, lock });
            }
        }

        Err(PickError::Exhausted {
            attempts: self.max_attempts,
        })
    }
}

/// Sockets verifying a candidate on every stack — bound only while its lock
/// is already held, so no other picker's port is ever touched.
struct Bound {
    _tcp4: TcpListener,
    _udp4: UdpSocket,
    _v6: Option<(Socket, Socket)>,
}

fn bind_all(port: u16, ipv6: bool) -> Option<Bound> {
    let tcp4 = TcpListener::bind(("0.0.0.0", port)).ok()?;
    let udp4 = UdpSocket::bind(("0.0.0.0", port)).ok()?;
    let v6 = if ipv6 {
        Some((bind_v6(port, Type::STREAM)?, bind_v6(port, Type::DGRAM)?))
    } else {
        None
    };

    Some(Bound {
        _tcp4: tcp4,
        _udp4: udp4,
        _v6: v6,
    })
}

/// Binds `[::]:port` with `IPV6_V6ONLY` set, so the socket never claims the
/// IPv4 wildcard too (Linux binds dual-stack by default, which would collide
/// with the IPv4 sockets we already hold).
fn bind_v6(port: u16, ty: Type) -> Option<Socket> {
    let socket = Socket::new(Domain::IPV6, ty, None).ok()?;
    let addr = SocketAddr::from((Ipv6Addr::UNSPECIFIED, port));

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
