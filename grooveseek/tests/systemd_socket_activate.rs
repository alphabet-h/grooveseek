//! (計画 4 段 A / 試験 A-10) The socket a service manager hands over is served
//! exactly like one groove bound itself.
//!
//! **What this is not.** Task A6's dry-run checks the same interface *before* a
//! release, in a container carrying the systemd a deployment will actually use.
//! This is the regression *after* the merge, every night, in CI. Neither
//! replaces the other: the dry-run is about the shape a deployment takes, this
//! is about the code not drifting away from it.

mod common;

/// Everything below needs systemd. `macos-latest` sits in the same test matrix
/// and has none, so the gate is `target_os = "linux"` rather than `unix` -- the
/// shape `tests/http_lock_contention.rs` already uses. Gating the module rather
/// than the crate keeps `mod common;` compiling on every platform.
#[cfg(target_os = "linux")]
mod linux {
    use crate::common::ansi::strip_ansi;
    // `wait_http_200` is deliberately not imported: its curl carries no
    // `--max-time`, so one call can outlive the deadline a caller declared
    // (`grooveseek/tests/common/mcp.rs`). The shared helper stays as it is; the
    // polling here uses `tcp_code` / `unix_code`, which do bound it.
    use crate::common::mcp::{Ready, StderrLog, build_index, drain_stderr_keeping, grooveseek_bin};
    use crate::common::temp::TempKbLayout;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// The tool that speaks a `.socket` unit's protocol with no unit file and
    /// no init system. It ships in the `systemd` package.
    ///
    /// **`--inetd` takes no argument** (`systemd-socket-activate --help`:
    /// `--inetd                 Enable inetd file descriptor passing
    /// protocol`), and it is the option groove does *not* want: without it the
    /// sockets arrive on descriptors 3 and up, which is the whole protocol
    /// here. So it is simply absent below.
    const ACTIVATE: &str = "/usr/bin/systemd-socket-activate";

    /// How many ports to try before giving up (see [`free_port`]).
    const ACTIVATE_ATTEMPTS: usize = 5;

    /// The surface whose peer rule and allow-lists this file is about.
    const ADMIN: &str = "/api/admin/status";

    /// Long enough for BGE-small to load on a cold runner.
    const STARTUP: Duration = Duration::from_secs(180);

    struct Spawned {
        child: Child,
        addr: String,
        log: StderrLog,
    }

    impl Drop for Spawned {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Start the process with both pipes captured and its stderr drained.
    ///
    /// stderr rather than stdout on purpose: `Commands::Serve` writes nothing
    /// to stdout, and the readiness line is an `eprintln!` in
    /// `grooveseek/src/transport/http.rs`. The draining is shared with the
    /// other integration tests rather than copied, because a second parser of
    /// that line starts answering a different question the moment the wording
    /// changes. A captured pipe nobody empties also blocks the process writing
    /// to it.
    fn start(mut cmd: Command) -> (Child, std::sync::mpsc::Receiver<Ready>, StderrLog) {
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("RUST_LOG", "info")
            .spawn()
            .expect("spawn the process");
        let stderr = child.stderr.take().expect("stderr was piped");
        let (rx, log) = drain_stderr_keeping(stderr, StderrLog::default());
        (child, rx, log)
    }

    /// A server that binds its own address: wait for the address it prints.
    ///
    /// One deadline for the whole wait, not one per message. `Ready` has a
    /// second variant (`Armed`), and re-arming `recv_timeout` on every
    /// non-address message would let the total wait grow without a bound.
    fn spawn_bound(cmd: Command) -> Spawned {
        let (child, rx, log) = start(cmd);
        let deadline = Instant::now() + STARTUP;
        let addr = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let seen = strip_ansi(&log.lines().join("\n"));
                panic!("no address on stderr within {STARTUP:?}. stderr so far:\n{seen}");
            }
            match rx.recv_timeout(left) {
                Ok(Ready::Addr(a)) => break a,
                Ok(_) => continue,
                Err(e) => {
                    let seen = strip_ansi(&log.lines().join("\n"));
                    panic!("no address on stderr within {STARTUP:?}: {e}. stderr so far:\n{seen}");
                }
            }
        };
        Spawned { child, addr, log }
    }

    /// An address the OS was willing to hand out a moment ago.
    ///
    /// `systemd-socket-activate` will not take port 0 -- measured on systemd
    /// 249: `-l 127.0.0.1:0` answers `Failed to open '127.0.0.1:0': Invalid
    /// argument` -- so the port has to be named, and the window between
    /// letting go of it and the activator binding it is real.
    /// [`spawn_activated`] closes the window by retrying on the activator's own
    /// failure line rather than by pretending there is none.
    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .expect("the OS can spare a loopback port")
            .local_addr()
            .expect("a bound listener has an address")
            .port()
    }

    /// A server started **through** `systemd-socket-activate`, which is a
    /// different shape from [`spawn_bound`] in two ways that matter.
    ///
    /// It does not fork: it binds, logs `Listening on <addr> as 3.`, waits in
    /// `epoll_wait` for the **first connection**, and only then replaces
    /// itself with the child (measured on systemd 249: with no connection the
    /// child never runs; with one, the log reads `Communication attempt on fd
    /// 3.` then `Execing ...`). So nothing can wait for groove's readiness
    /// line first -- the connection has to come before there is a groove at
    /// all. The polling below supplies it: its first request is the trigger
    /// and its later ones are the wait.
    ///
    /// The activator's own line is `Listening on` with a capital L, while
    /// `drain_stderr_keeping` splits on lowercase `listening on ` (the
    /// wording of groove's own line), so the two cannot be confused.
    fn spawn_activated(bin: &std::path::Path, kb_arg: &str) -> Spawned {
        for _ in 0..ACTIVATE_ATTEMPTS {
            let port = free_port();
            let mut cmd = Command::new(ACTIVATE);
            cmd.args(["-l", &format!("127.0.0.1:{port}")])
                .arg(bin)
                .args([
                    "serve",
                    "--kb-path",
                    kb_arg,
                    "--no-watch",
                    "--transport",
                    "http",
                    "--systemd-socket",
                ]);
            let (child, _rx, log) = start(cmd);
            let addr = format!("127.0.0.1:{port}");
            let spawned = Spawned {
                child,
                addr: addr.clone(),
                log: log.clone(),
            };

            // The activator says which of the two happened before it waits.
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut bound = None;
            while Instant::now() < deadline && bound.is_none() {
                if log.contains(&format!("Listening on {addr}")) {
                    bound = Some(true);
                } else if log.contains("Failed to open") {
                    bound = Some(false);
                } else {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            match bound {
                Some(true) => {
                    // Not `wait_http_200`: its curl carries no `--max-time`
                    // and its loop tests the deadline only before a try, so
                    // the first request -- the one that sits in the backlog
                    // until the model is loaded -- can run past `STARTUP` on
                    // its own. The shared helper is left alone; this is the
                    // same shape `unix_code` below uses.
                    let deadline = Instant::now() + STARTUP;
                    let mut up = false;
                    while Instant::now() < deadline && !up {
                        up = tcp_code(&addr, "/healthz", &[]) == "200";
                        if !up {
                            std::thread::sleep(Duration::from_millis(500));
                        }
                    }
                    assert!(
                        up,
                        "no 200 from http://{addr}/healthz within {STARTUP:?}; the first request is what makes the activator exec, so this covers both the trigger and the model load. stderr:\n{}",
                        strip_ansi(&spawned.log.lines().join("\n"))
                    );
                    return spawned;
                }
                Some(false) => continue, // something took the port; pick another
                None => panic!(
                    "the activator neither bound nor refused within 15s. stderr:\n{}",
                    strip_ansi(&spawned.log.lines().join("\n"))
                ),
            }
        }
        panic!("could not get a free port for the activator in {ACTIVATE_ATTEMPTS} attempts");
    }

    /// `curl`'s status code for one request over TCP, the way every other
    /// integration test here reaches a running server, with a `--max-time` so
    /// one call cannot outlive the deadline its caller declared.
    ///
    /// A refused connection comes back as `000` rather than as an error: the
    /// caller is asking what the server answered, and "nothing yet" is an
    /// answer worth polling on.
    ///
    /// The header list is a slice, the same shape [`unix_code`] takes, so the
    /// two really are one form rather than two that look alike.
    fn tcp_code(addr: &str, path: &str, headers: &[(&str, &str)]) -> String {
        let mut cmd = Command::new("curl");
        cmd.args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "10",
        ]);
        for (name, value) in headers {
            cmd.args(["-H", &format!("{name}: {value}")]);
        }
        let out = cmd.arg(format!("http://{addr}{path}")).output().expect("curl");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// `curl` over a Unix socket. `Host: localhost` is what the gate's
    /// allow-list expects, and what the gateway sends in the real deployment.
    ///
    /// The path is a parameter because two different Host lists answer here:
    /// the admin routes read `allowed_admin_hosts` and `/mcp` reads the list
    /// `run_http` built. Asking only one of them would attribute its 403 to
    /// the wrong function.
    fn unix_code(sock: &str, path: &str, headers: &[(&str, &str)]) -> String {
        let mut cmd = Command::new("curl");
        cmd.args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "10",
            "--unix-socket",
            sock,
        ]);
        for (name, value) in headers {
            cmd.args(["-H", &format!("{name}: {value}")]);
        }
        let out = cmd
            .arg(format!("http://localhost{path}"))
            .output()
            .expect("curl");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// The probes whose answers must not depend on who called `bind(2)`.
    fn answers(addr: &str) -> Vec<(&'static str, String)> {
        let port = addr.rsplit_once(':').expect("the address carries a port").1;
        vec![
            ("no header", tcp_code(addr, ADMIN, &[])),
            (
                "loopback Host",
                tcp_code(addr, ADMIN, &[("Host", &format!("127.0.0.1:{port}"))]),
            ),
            (
                "foreign Host",
                tcp_code(addr, ADMIN, &[("Host", "evil.example")]),
            ),
            (
                "loopback Origin",
                tcp_code(addr, ADMIN, &[("Origin", &format!("http://127.0.0.1:{port}"))]),
            ),
            (
                "foreign Origin",
                tcp_code(addr, ADMIN, &[("Origin", "http://evil.example")]),
            ),
        ]
    }

    /// (試験 A-10) The `Host` and `Origin` defaults are derived from the
    /// address the listener actually has, so a TCP descriptor systemd handed
    /// over has to produce the same answers as a bind of our own.
    #[test]
    #[ignore = "starts `groove serve`, so it loads the embedding model; the nightly ignored-tests job runs it on ubuntu"]
    fn a_tcp_descriptor_from_a_service_manager_is_served_like_our_own_bind() {
        assert!(
            std::path::Path::new(ACTIVATE).exists(),
            "{ACTIVATE} is missing; it ships in the systemd package, so either the runner image changed or this test's premise did. Failing rather than skipping: a check that quietly passes when its tool is absent has stopped being a check."
        );

        let kb = TempKbLayout::new("groove-sda");
        kb.write("a.md", "---\ntitle: Hello\n---\n\n# Body\n\nplain text.\n");
        build_index(kb.kb());
        let bin = grooveseek_bin();
        let kb_arg = kb.kb().to_str().expect("a UTF-8 scratch path").to_string();

        // (1) groove binds the address itself. `:0` is fine here -- it is
        // groove asking the OS, and groove reports what it got.
        let mut own_cmd = Command::new(&bin);
        own_cmd.args([
            "serve",
            "--kb-path",
            &kb_arg,
            "--no-watch",
            "--transport",
            "http",
            "--bind",
            "127.0.0.1:0",
        ]);
        let own = spawn_bound(own_cmd);
        let from_bind = answers(&own.addr);

        // (2) the same server, over a descriptor a service manager bound.
        let passed = spawn_activated(&bin, &kb_arg);
        let from_descriptor = answers(&passed.addr);

        let seen = strip_ansi(&passed.log.lines().join("\n"));
        assert_eq!(
            from_bind, from_descriptor,
            "who called bind(2) must not change the Host and Origin defaults; own bind on {}, descriptor on {}. stderr of the second:\n{seen}",
            own.addr, passed.addr
        );
        assert!(
            from_bind.contains(&("no header", "200".to_string())),
            "the admin surface has to answer at all, or the comparison above is vacuous: {from_bind:?}"
        );
        assert!(
            from_bind.contains(&("foreign Host", "403".to_string())),
            "a foreign Host has to be refused, or the comparison above is vacuous: {from_bind:?}"
        );
    }

    /// The wiring no unit test reaches: that `run_http`, for a listener with
    /// no address, builds the **Unix defaults** --
    /// `effective_allowed_hosts_unix` and `effective_allowed_origins_unix` --
    /// and hands each one to the surface that reads it.
    ///
    /// **Which surface reads which list is not the same for the two.** The
    /// Host list `run_http` derives goes to `/mcp` (`shared_hosts`); the admin
    /// routes keep their own, `allowed_admin_hosts`, built in
    /// `grooveseek/src/server.rs`, which is not an `effective_allowed_*` list
    /// at all. The Origin list *is* shared (`shared_origins`), so the admin
    /// surface is where `effective_allowed_origins_unix` can be measured. The
    /// probes below are split accordingly: a foreign `Host` on `/mcp` names
    /// `effective_allowed_hosts_unix`, the same header on the admin path names
    /// `allowed_admin_hosts`, and a foreign `Origin` on the admin path names
    /// `effective_allowed_origins_unix`.
    ///
    /// `run_http` is not callable from a test -- its one caller is
    /// `server.rs` -- and a `DnsRebindingGate` a test builds by hand agrees
    /// with whatever the test wrote in it. This repo already knows that shape:
    /// `build_router_for_test`'s own documentation says it "is not the
    /// production router" and that Origin validation "is exercised where it
    /// exists -- through a running server". So this does the same: a real
    /// server, on a real Unix socket, answering real requests. A `hosts` field
    /// left as `Arc::new(None)` makes `evil.example` answer 200 here; an
    /// `origins` field left empty makes the foreign-Origin request answer 200.
    ///
    /// **It does not pin the `peer` field, and no behavioural test can.** A
    /// Unix listener carries no `ConnectInfo<SocketAddr>`, so `decide`'s peer
    /// block is skipped whichever `PeerRule` it holds -- `UnixLocal` fails its
    /// first condition, `LoopbackTcp` fails the `let Some(...)` that follows,
    /// and both reach the Host check unchanged. The two are observationally
    /// identical today. What holds that field is `admin_peer_rule`'s own unit
    /// test plus the fact that `run_http` has one place to call it from; the
    /// day `Connected` is implemented for `UnixListener`, they stop being
    /// identical and this test starts covering it too.
    #[test]
    #[ignore = "starts `groove serve` through systemd-socket-activate; the nightly ignored-tests job runs it on ubuntu"]
    fn a_unix_listener_serves_the_admin_surface_with_the_unix_host_and_origin_defaults() {
        assert!(
            std::path::Path::new(ACTIVATE).exists(),
            "{ACTIVATE} is missing; it ships in the systemd package. Failing rather than skipping."
        );

        let kb = TempKbLayout::new("groove-sda-uds");
        kb.write("a.md", "---\ntitle: Hello\n---\n\n# Body\n\nplain text.\n");
        build_index(kb.kb());
        let sock = kb.root().join("groove.sock");
        let sock_arg = sock.to_str().expect("a UTF-8 scratch path").to_string();
        let kb_arg = kb.kb().to_str().expect("a UTF-8 scratch path").to_string();

        let mut cmd = Command::new(ACTIVATE);
        cmd.args(["-l", &sock_arg])
            .arg(grooveseek_bin())
            .args([
                "serve",
                "--kb-path",
                &kb_arg,
                "--no-watch",
                "--transport",
                "http",
                "--systemd-socket",
            ]);
        let (child, _rx, log) = start(cmd);
        // Named rather than `_guard`: the socket path is read back from it
        // below, so the struct earns its `addr` field as well as its `Drop`.
        let server = Spawned {
            child,
            addr: sock_arg.clone(),
            log: log.clone(),
        };

        // Tell a bind failure from a slow start before spending `STARTUP` on
        // it, the same way `spawn_activated` does for the TCP leg.
        let bind_deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < bind_deadline && !log.contains(&format!("Listening on {sock_arg}")) {
            assert!(
                !log.contains("Failed to open"),
                "the activator could not bind {sock_arg}. stderr:\n{}",
                strip_ansi(&log.lines().join("\n"))
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        // Neither line inside the window is its own answer, and the TCP leg
        // says so rather than falling through to a 180-second wait.
        assert!(
            log.contains(&format!("Listening on {sock_arg}")),
            "the activator neither bound nor refused within 15s. stderr:\n{}",
            strip_ansi(&log.lines().join("\n"))
        );

        // The first request is what makes the activator exec, so the retry
        // loop is the trigger as well as the wait.
        let deadline = Instant::now() + STARTUP;
        let mut up = false;
        while Instant::now() < deadline && !up {
            up = unix_code(&server.addr, ADMIN, &[("host", "localhost")]) == "200";
            if !up {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        assert!(
            up,
            "no 200 from /api/admin/status over {} within {STARTUP:?}. stderr:\n{}",
            server.addr,
            strip_ansi(&log.lines().join("\n"))
        );

        // The admin routes' own list, which is not one of the
        // `effective_allowed_*` pair (see this test's documentation).
        assert_eq!(
            unix_code(&server.addr, ADMIN, &[("host", "evil.example")]),
            "403",
            "the admin routes carry allowed_admin_hosts over a Unix listener too; a foreign Host has to be refused"
        );
        // The list `run_http` derived, asked of the surface that reads it.
        assert_eq!(
            unix_code(&server.addr, "/mcp", &[("host", "evil.example")]),
            "403",
            "run_http must hand /mcp effective_allowed_hosts_unix; an empty or None host list would not answer 403 here"
        );
        assert_ne!(
            unix_code(&server.addr, "/mcp", &[("host", "localhost")]),
            "403",
            "a loopback Host must not be refused, or the line above is vacuous"
        );
        assert_eq!(
            unix_code(
                &server.addr,
                ADMIN,
                &[("host", "localhost"), ("origin", "http://evil.example")]
            ),
            "403",
            "run_http must hand the gate effective_allowed_origins_unix; an empty origin list turns Origin validation off and would answer 200 here"
        );
        assert_eq!(
            unix_code(
                &server.addr,
                ADMIN,
                &[("host", "localhost"), ("origin", "http://localhost:3101")]
            ),
            "200",
            "an allow-list entry with no port matches every port on that host, so a loopback Origin passes whatever port it names"
        );
    }
}
