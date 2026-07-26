# portuario

Concurrency-safe free-port picking for test suites that spawn real servers.

When parallel tests (e.g. `cargo nextest`, one process per test) need ports
for the servers they spawn, checking that a port is free is not enough: two
tests can pick the same port before either binds it. `portuario` closes
that race with OS advisory file locks, so every caller on the machine is handed
a port that is both **verified free** and **reserved against everyone else**.

```rust
let port = portuario::pick_port()?;
let url = format!("127.0.0.1:{port}");

// spawn your server on `port`; the reservation is released
// when `port` drops, or explicitly:
port.release()?;
```

## How it works

1. A candidate is drawn at random from `15000..25000` — a range that sits
   *below* the ephemeral port range of Linux (32768–60999), macOS and Windows
   (49152–65535). The kernel never assigns these ports to outgoing connections,
   so the OS itself can never race you for one.
2. An advisory lock on `<lock_dir>/<port>.lock` is taken **first**
   ([`File::try_lock`], flock on Unix, `LockFileEx` on Windows). A candidate
   already locked by another picker is skipped without ever binding it, so
   pickers never trample a port its owner is about to use.
3. Only while holding the lock is the candidate verified: bound on TCP and UDP
   over the IPv4 and IPv6 wildcard addresses. Then the probe sockets are
   dropped and the locked `Port` is returned.

The lock is owned by the kernel and tied to the process: it is released the
moment the process exits — including on panic or `SIGKILL` — so a crashed test
can never leak a reservation. Stale `.lock` files are inert and intentionally
never deleted (unlinking lock files that another process may have already
opened is a classic double-lock race).

[`File::try_lock`]: https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock

## The lock directory

Every process coordinating over ports must resolve the **same** directory, so
`portuario` refuses to guess:

- An explicit `Picker::lock_dir(...)` always wins.
- Otherwise it is derived from `CARGO_MANIFEST_DIR` (set by cargo and nextest
  for every test process) as `<workspace root>/target/portuario`, where the
  workspace root is the nearest ancestor holding a `Cargo.lock`. All members of
  a workspace resolve the same directory.
- With neither, `pick()` fails with `PickError::NoLockDir` instead of falling
  back to something that could silently diverge between subprocesses.

## Configuration

```rust
use portuario::Picker;

let port = Picker::new()
    .range(20000..21000)        // candidate range (keep it below the ephemeral range)
    .lock_dir("/tmp/my-locks")  // shared lock directory
    .max_attempts(500)          // candidates tried before giving up
    .ipv6(false)                // IPv6 checking (auto-detected by default)
    .pick()?;
```

## Guarantees and limits

- The returned port was free on TCP and UDP, IPv4 and IPv6, at pick time, and
  stays reserved against every other `portuario` user until released.
- The reservation is advisory: an unrelated process binding arbitrary ports is
  only excluded by the freeness check, not by the lock. Picking from below the
  ephemeral range makes the remaining window between pick and bind a
  1-in-thousands coincidence of explicit binds, not something the OS does.
- The contention suite bakes this in: 500 nextest processes fighting over an
  8-port arena, repeatedly — a test can only fail if two concurrent holders
  are ever handed the same port.

## Coverage

![functions](badges/functions.svg)
![lines](badges/lines.svg)
![regions](badges/regions.svg)
![branches](badges/branches.svg)

Measured with `cargo llvm-cov` under nextest; regenerate the badges with
`./badge.sh`. The two uncovered regions are unreachable-by-construction and
documented in-source with `// Uncovered:` comments.

## Credit where it is due

`portuario` is inspired by [portpicker](https://crates.io/crates/portpicker) —
the sub-ephemeral candidate range and the TCP+UDP/IPv4+IPv6 freeness probe
come straight from its playbook. If you are not running tests in parallel
processes (e.g. not using `cargo nextest`), portpicker is strongly
recommended instead: it does the job with none of the locking machinery.

## MSRV

Rust 1.89 (`File::try_lock`).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
