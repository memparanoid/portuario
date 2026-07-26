use super::*;

use std::error::Error;
use std::net::{TcpListener, UdpSocket};
use std::path::PathBuf;

type TestResult = Result<(), Box<dyn Error>>;

/// Each test gets its own lock directory and its own dedicated port(s) so the
/// suite survives `cargo nextest` process-per-test parallelism as well as
/// in-process threads. Ports 25100.. sit outside [`DEFAULT_RANGE`].
fn lock_dir_for(test: &str) -> PathBuf {
    std::env::temp_dir().join(format!("portuario-test-{test}"))
}

fn picker(test: &str, range: Range<u16>) -> Picker {
    Picker::new().lock_dir(lock_dir_for(test)).range(range)
}

// ============================================================================
// pick_port
// ============================================================================

#[test]
fn test_pick_port_returns_a_port_from_the_default_range() -> TestResult {
    let port = pick_port()?;

    assert!(DEFAULT_RANGE.contains(&port.value()));

    Ok(())
}

// ============================================================================
// Port::release
// ============================================================================

#[test]
fn test_release_returns_ok_and_frees_the_port() -> TestResult {
    let config = picker("release", 25113..25114);

    config.pick()?.release()?;

    assert_eq!(config.pick()?.value(), 25113);

    Ok(())
}

// ============================================================================
// Port (drop)
// ============================================================================

#[test]
fn test_port_drop_frees_the_port() -> TestResult {
    let config = picker("drop", 25114..25115);

    drop(config.pick()?);

    assert_eq!(config.pick()?.value(), 25114);

    Ok(())
}

// ============================================================================
// Port (Display / Debug)
// ============================================================================

#[test]
fn test_port_display_and_debug_return_the_value() -> TestResult {
    let port = picker("display", 25115..25116).pick()?;

    assert_eq!(format!("{port}"), "25115");
    assert_eq!(format!("{port:?}"), "Port { value: 25115 }");

    Ok(())
}

// ============================================================================
// PickError (Display / Error::source)
// ============================================================================

#[test]
fn test_pick_error_display_returns_a_message_per_variant() {
    let no_lock_dir = PickError::NoLockDir;
    let io = PickError::from(io::Error::other("boom"));
    let exhausted = PickError::Exhausted { attempts: 7 };

    assert!(format!("{no_lock_dir}").contains("set Picker::lock_dir explicitly"));
    assert_eq!(format!("{io}"), "port picking failed: boom");
    assert_eq!(
        format!("{exhausted}"),
        "no free port found after 7 attempts"
    );
}

#[test]
fn test_pick_error_source_returns_the_underlying_io_error() {
    let no_lock_dir = PickError::NoLockDir;
    let io = PickError::from(io::Error::other("boom"));
    let exhausted = PickError::Exhausted { attempts: 7 };

    assert!(no_lock_dir.source().is_none());
    assert!(io.source().is_some());
    assert!(exhausted.source().is_none());
}

// ============================================================================
// Picker::pick
// ============================================================================

#[test]
#[should_panic(expected = "port range must not be empty")]
fn test_pick_panics_when_the_range_is_empty() {
    let _ = picker("empty-range", 25100..25100).pick();
}

#[test]
fn test_pick_propagates_resolve_lock_dir_error() {
    let mut config = Picker::new().range(25100..25101);
    config.env = OsEnvMap::default();

    let result = config.pick();

    assert!(matches!(result, Err(PickError::NoLockDir)));
}

#[test]
fn test_pick_propagates_create_dir_all_error() -> TestResult {
    let file = std::env::temp_dir().join("portuario-test-not-a-dir");
    fs::write(&file, b"")?;

    let result = Picker::new().lock_dir(&file).range(25100..25101).pick();

    assert!(matches!(result, Err(PickError::Io(_))));

    Ok(())
}

#[cfg(unix)]
#[test]
fn test_pick_propagates_lock_file_open_error() -> TestResult {
    use std::os::unix::fs::PermissionsExt;

    let dir = lock_dir_for("readonly-dir");

    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555))?;

    let result = Picker::new().lock_dir(&dir).range(25108..25109).pick();

    assert!(matches!(result, Err(PickError::Io(_))));

    Ok(())
}

#[test]
fn test_pick_reports_exhausted_when_the_lock_is_held() -> TestResult {
    let dir = lock_dir_for("lock-held");

    fs::create_dir_all(&dir)?;

    let held = File::create(dir.join("25109.lock"))?;
    held.try_lock()?;

    let result = picker("lock-held", 25109..25110).max_attempts(5).pick();

    assert!(matches!(result, Err(PickError::Exhausted { attempts: 5 })));

    Ok(())
}

#[test]
fn test_pick_returns_the_free_candidate_when_a_lock_is_held() -> TestResult {
    let dir = lock_dir_for("lock-retry");

    fs::create_dir_all(&dir)?;

    let held = File::create(dir.join("25110.lock"))?;
    held.try_lock()?;

    let port = picker("lock-retry", 25110..25112).pick()?;

    assert_eq!(port.value(), 25111);

    Ok(())
}

#[test]
fn test_pick_reports_exhausted_when_the_port_is_already_picked() -> TestResult {
    let reserved = picker("reserved", 25112..25113).pick()?;

    let result = picker("reserved", 25112..25113).max_attempts(5).pick();

    assert!(matches!(result, Err(PickError::Exhausted { attempts: 5 })));
    drop(reserved);

    Ok(())
}

#[test]
fn test_pick_reports_exhausted_when_tcp4_is_bound() -> TestResult {
    let _tcp4 = TcpListener::bind("0.0.0.0:25103")?;

    let result = picker("tcp4-bound", 25103..25104).max_attempts(5).pick();

    assert!(matches!(result, Err(PickError::Exhausted { attempts: 5 })));

    Ok(())
}

#[test]
fn test_pick_reports_exhausted_when_udp4_is_bound() -> TestResult {
    let _udp4 = UdpSocket::bind("0.0.0.0:25104")?;

    let result = picker("udp4-bound", 25104..25105).max_attempts(5).pick();

    assert!(matches!(result, Err(PickError::Exhausted { attempts: 5 })));

    Ok(())
}

#[test]
fn test_pick_reports_exhausted_when_tcp6_is_bound() -> TestResult {
    if !ipv6_available() {
        return Ok(());
    }

    let _tcp6 = bind_v6(25105, Type::STREAM).ok_or("cannot bind [::]:25105")?;
    let result = picker("tcp6-bound", 25105..25106)
        .ipv6(true)
        .max_attempts(5)
        .pick();

    assert!(matches!(result, Err(PickError::Exhausted { attempts: 5 })));

    Ok(())
}

#[test]
fn test_pick_reports_exhausted_when_udp6_is_bound() -> TestResult {
    if !ipv6_available() {
        return Ok(());
    }

    let _udp6 = bind_v6(25106, Type::DGRAM).ok_or("cannot bind [::]:25106")?;
    let result = picker("udp6-bound", 25106..25107)
        .ipv6(true)
        .max_attempts(5)
        .pick();

    assert!(matches!(result, Err(PickError::Exhausted { attempts: 5 })));

    Ok(())
}

#[test]
fn test_pick_returns_a_port_when_ipv6_checks_are_disabled() -> TestResult {
    if !ipv6_available() {
        return Ok(());
    }

    let _tcp6 = bind_v6(25107, Type::STREAM).ok_or("cannot bind [::]:25107/tcp")?;
    let _udp6 = bind_v6(25107, Type::DGRAM).ok_or("cannot bind [::]:25107/udp")?;

    let port = picker("ipv6-disabled", 25107..25108).ipv6(false).pick()?;

    assert_eq!(port.value(), 25107);

    Ok(())
}

#[test]
fn test_pick_returns_a_locked_port_within_the_range() -> TestResult {
    let port = picker("happy-path", 25101..25103).pick()?;

    assert!((25101..25103).contains(&port.value()));
    assert!(
        lock_dir_for("happy-path")
            .join(format!("{port}.lock"))
            .exists()
    );

    Ok(())
}

// ============================================================================
// resolve_lock_dir
// ============================================================================

#[test]
fn test_resolve_lock_dir_returns_the_explicit_dir() -> TestResult {
    let explicit = Path::new("/explicit/locks");
    let dir = resolve_lock_dir(Some(explicit), Some("/ignored".into()))?;

    assert_eq!(dir, explicit);

    Ok(())
}

#[test]
fn test_resolve_lock_dir_reports_no_lock_dir_error() {
    let result = resolve_lock_dir(None, None);

    assert!(matches!(result, Err(PickError::NoLockDir)));
}

#[test]
fn test_resolve_lock_dir_returns_the_workspace_target() -> TestResult {
    let root = std::env::temp_dir().join("portuario-test-workspace");
    let member = root.join("crates").join("member");

    fs::create_dir_all(&member)?;
    fs::write(root.join("Cargo.lock"), b"")?;

    let dir = resolve_lock_dir(None, Some(member.into_os_string()))?;

    assert_eq!(dir, root.join("target").join("portuario"));

    Ok(())
}

#[test]
fn test_resolve_lock_dir_returns_the_manifest_target_without_a_workspace_root() -> TestResult {
    let manifest = std::env::temp_dir().join("portuario-test-lonely-crate");

    fs::create_dir_all(&manifest)?;

    let dir = resolve_lock_dir(None, Some(manifest.clone().into_os_string()))?;

    assert_eq!(dir, manifest.join("target").join("portuario"));

    Ok(())
}

// ╔══════════════════════════════════════════════════════════════════════════╗
// ║ CONCURRENCY — sustained cross-process contention                         ║
// ╚══════════════════════════════════════════════════════════════════════════╝

// 40 identical tests generated from one template: under nextest each runs in
// its own process, exercising the real cross-process coordination the lock
// exists for. They all fight over an 8-port arena with ~16 running at once,
// so pickers queue on the retry path thousands of times while holders sleep.
// The huge attempt budget means a test can only fail if the lock hands the
// same port to two concurrent holders — never because of scheduling luck.
seq_macro::seq!(N in 0..40 {
    #[test]
    fn test_pick_returns_a_bindable_port_under_contention_~N() -> TestResult {
        let port = picker("contention", 25200..25208)
            .max_attempts(1_000_000)
            .pick()?;

        let listener = TcpListener::bind(("0.0.0.0", port.value()))?;
        std::thread::sleep(std::time::Duration::from_millis(25));
        drop(listener);

        Ok(())
    }
});
