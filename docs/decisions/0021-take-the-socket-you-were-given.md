# 21. Take the socket you were given

- Status: accepted
- Date: 2026-09-18
- Deciders: project owner
- Applies to: v1.11.0

## Context and problem

GrooveSeek authenticates nobody, and says so in the refusal it prints when a
non-loopback bind is asked for without `--i-know`
(`grooveseek/src/transport/mod.rs:397-406`): anything that can reach the port
can read the entire knowledge base. Reachability is therefore the whole of the
access control — and until v1.10.0, reachability meant a TCP port. A loopback
port is open to every account on the host, so several daemons on one machine,
one per body of documents and each meant for one caller, were separated by
nothing stronger than a convention about which port belonged to whom.

A socket in the file system moves that question somewhere the kernel already
answers it. Its owner and its mode are checked at `connect(2)`, before the
first byte arrives, by the same code that guards every other file. What it
needs is somebody to create it with the right owner and mode before the daemon
starts.

The question this record answers is not whether to serve over `AF_UNIX`. It is
**who creates the socket, and what `serve` does when it cannot have the one it
was promised.**

## Decision drivers

- Whatever decides who may connect has to sit outside a process that
  authenticates nobody.
- A socket's owner and mode are fixed when it is created. "Who creates it" is
  therefore part of the access control, not an implementation detail.
- The same command line must not listen in two different places depending on
  what it inherited from its parent.
- A daemon listening somewhere other than where the operator configured it is
  worse than a daemon that refuses to start: the operator reads the address
  they wrote and believes it.
- `sd_listen_fds(3)` already specifies the handover and the checks that make it
  safe. A second invention would be a second thing to get wrong.
- The deployment this was built for gives each unit one `ListenStream=`, so
  "exactly one descriptor" costs nothing there and keeps every refusal able to
  name what it read (`grooveseek/src/transport/systemd_fd.rs:78-82`).

## Options considered

1. **Take the descriptor a `.socket` unit already bound, on explicit opt-in,
   and refuse to start when it cannot be taken.** Taken.

2. **Detect socket activation from the environment** — use `LISTEN_FDS`
   wherever it happens to be set. Rejected. It makes one command line mean two
   different things, and the variable that decides which is inherited: a shell
   started from an activated service, a supervisor passing its own descriptors
   on, or a parent that set the variables for somebody else all reach `serve`
   the same way. The check `sd_listen_fds(3)` puts first exists precisely
   because the environment travels further than the descriptors do
   (`grooveseek/src/transport/systemd_fd.rs:57-71`). Auto-detection would make
   surviving that check the ordinary path rather than the opted-into one.

3. **Bind a filesystem path of groove's own** (`--unix-socket <path>`).
   Rejected. It would work, and it would add a second creator of sockets: the
   owner and the mode would then be settled by whichever of groove, the unit
   file and the surrounding `umask` got there first, differently per
   deployment. Creating the socket belongs to the service manager in this
   design, and leaving it there keeps one answer to "who decided who can reach
   this".

4. **Put `systemd-socket-proxyd` between the unit and groove**, the arrangement
   the systemd manual's own namespace example uses. Rejected on four properties
   of the proxy, each from a primary source:

   - it accepts a connection whether or not the upstream is alive, so a
     caller's liveness check reports a connection and then waits, rather than
     failing;
   - it has no per-connection timeout and no `SO_KEEPALIVE`
     (<https://github.com/systemd/systemd/issues/23320>), while MCP over
     Streamable HTTP holds connections open — half-dead ones would sit in its
     `--connections-max=` budget;
   - it forwards no credentials: `systemd-socket-proxyd(8)` says it "will not
     forward `SCM_RIGHTS`, `SCM_CREDENTIALS`, `SCM_SECURITY`, `SO_PEERCRED`,
     `SO_PEERPIDFD`, `SO_PEERSEC`, `SO_PEERGROUPS` and similar";
   - one proxy serves one socket
     (<https://github.com/systemd/systemd/issues/15599>), so each added
     deployment adds a unit.

   It also puts a second process in front of each daemon, where taking the
   descriptor directly puts none.

5. **Check the connecting uid with `SO_PEERCRED`.** Rejected — and option 4 is
   what makes it available to reject, since not going through the proxy is
   exactly what leaves the credentials reachable. `axum`'s `Connected` trait is
   not sealed (`axum-0.8.9/src/extract/connect_info.rs:80-83`), axum carries a
   `UnixListener` implementation of it as a compile test
   (`axum-0.8.9/src/serve/mod.rs:503-513`), and `IncomingStream::io()`
   (`axum-0.8.9/src/serve/mod.rs:436-439`) hands back the `UnixStream` whose
   `peer_cred()` answers the question.

   It is still not taken. The socket's mode has the kernel ask the same
   question earlier, and asking it again in the application splits one
   condition across separate places — the socket's mode and the application's
   own code — which can drift apart. And
   `Connected::connect_info` returns `Self` rather than an `io::Result`
   (`axum-0.8.9/src/extract/connect_info.rs:82`), so a `peer_cred()` that fails
   has nowhere to say so: the default would be either fail-open or a refusal
   with no stated cause.

   The condition that would reopen it: one socket that has to be opened to more
   than one principal — a monitoring agent beside the intended caller, or a
   read-only user from elsewhere. The mode would then be widened to a group,
   and telling the uids apart would have to happen here.

## Decision

**`groove serve` accepts on the socket it was handed, and creates none.**

- **Explicit opt-in.** `--systemd-socket`, or `[transport.http].systemd_socket
  = true`. Without one of them nothing reads `LISTEN_FDS`, and a daemon that
  does not ask behaves as every release before v1.11.0 did.
- **No fallback to TCP.** Where the socket cannot be taken, `serve` exits. The
  descriptor is checked in the order `sd_listen_fds(3)` sets: `LISTEN_PID`
  against this process's own pid first, because a variable inherited from a
  parent otherwise hands over somebody else's descriptor; `LISTEN_FDS` at
  exactly one; then `SO_TYPE` before `SO_ACCEPTCONN`, so a `ListenDatagram=`
  unit is reported for its type instead of for not listening
  (`grooveseek/src/transport/systemd_fd.rs:52-130`).
- **Exclusive with an address of our own.** `--bind`, `--port` and
  `[transport.http].bind` each refuse to stand beside it
  (`grooveseek/src/transport/mod.rs:330-360`). Two listening addresses is not a
  configuration, and picking one silently leaves the operator reading an
  address that nothing answers on.
- **Where it works is where the protocol exists**: a Unix host whose service
  manager passes `LISTEN_FDS` — systemd on Linux, and anything else that speaks
  the same protocol. A Windows build refuses both the flag and the key
  (`grooveseek/src/transport/mod.rs:307-309`).
- **The family is read off the descriptor**, not declared. A TCP socket a unit
  bound is served exactly like one groove bound itself, peer check and derived
  defaults included; a Unix socket becomes a listener with no address at all
  (`grooveseek/src/transport/systemd_fd.rs:213-223`).
- **The socket file is not groove's to manage.** It is never `shutdown(2)`n and
  its path is never unlinked: the service manager keeps its own copy of the
  descriptor, and `systemd.socket(5)` says a service "must not unlink the
  socket from a file system".
- **No new dependency.** `getsockopt`, `getsockname` and `fcntl` through the
  `libc` this crate already carries on `cfg(unix)`, rather than `libsystemd` or
  `listenfd`.

## Consequences

- **A listener can now have no address at all**
  (`grooveseek/src/transport/http.rs:898-956`). What reads that is the peer
  rule, the `Host` default, the `Origin` default and the startup line, and each
  of them reads the same `Option<SocketAddr>` that `open_listener` returned
  (`grooveseek/src/transport/http.rs:1046-1055`, `:1236`). So the case that is
  easy to miss — a unit passing a *TCP* descriptor — cannot be handled one way
  in one of them and another way in the next.
- **The peer check is split by type rather than by a flag.** A `UnixListener`
  carries no `ConnectInfo<SocketAddr>`, so the boolean this replaces read as
  "on" while the condition it guarded quietly fell through.
  `PeerRule::UnixLocal` is the Unix case, and it means "the socket's owner and
  mode already decided this", not "we cannot tell who this is"
  (`grooveseek/src/transport/http.rs:1681-1714`).
- **What a test can hold here is narrower than the decision.** Over a Unix
  listener the three `PeerRule` values are observationally identical, because
  the extension the check reads is never attached at all. A behavioural test
  against such a listener pins the `Host` and `Origin` wiring; the value
  `run_http` passes for `peer` is held by the unit test of `admin_peer_rule`
  and by review.
- **The `Origin` default becomes the port-less loopback spellings**, because
  there is no port to name. An allow-list entry with no port matches every port
  on that host (`grooveseek/src/transport/http.rs:673-681`), so an `Origin`
  naming `localhost`, `127.0.0.1` or `[::1]` passes whatever port it carries,
  and every other `Origin` is refused. The list is deliberately not empty:
  empty is how "do not validate `Origin` at all" is spelled.
- **`unsafe` enters the transport layer**, confined to `systemd_fd.rs`, where
  it is the `libc` calls plus the one line that turns a raw descriptor number
  into an owner (`grep -n "unsafe" grooveseek/src/transport/systemd_fd.rs`
  answers `:93`, `:136`, `:139`, `:157`, `:273`; from `:336` on it is
  `#[cfg(test)]`). **Ownership is created only at `take_listener`'s `:273`**,
  and only after the checks that can be made without owning the descriptor have
  run (`grooveseek/src/transport/systemd_fd.rs:253-274`). `adopt`
  (`grooveseek/src/transport/systemd_fd.rs:213-223`) takes an `OwnedFd`, so no
  `unsafe` appears inside it.
- **A version floor the operator meets before the unit file does.** v1.10.0 and
  earlier reject an unknown key, so a `groove.toml` carrying `systemd_socket`
  stops those releases from starting at all. Upgrade groove first, then change
  the unit.
- **Reachability becomes something groove does not know.** The socket's path,
  owner and mode, and whatever else the unit sets around it, are the access
  control, and none of it is visible from inside the process. groove does not
  check it, does not report it, and cannot warn that it is wrong — the
  non-loopback bind warning has nothing to look at on this listener.
- **The refusals are the interface.** With no fallback, every shape that cannot
  be served is a startup failure, and the sentence printed is what the operator
  has to work from. They stay ASCII, and they name the value that was read
  alongside the one expected.

## References

- `sd_listen_fds(3)` for the handover protocol and the order of its checks;
  `systemd.socket(5)` for `Accept=no` and for not unlinking the socket;
  `systemd-socket-proxyd(8)` for what the proxy does not forward.
- [ADR-0009](0009-one-dns-rebinding-gate.md) for the gate these defaults feed,
  and for why GrooveSeek answers `Host` and `Origin` itself.
- [deployment-topologies.md](../deployment-topologies.md) for what each route
  asks on each kind of listener.
- `grooveseek/src/transport/systemd_fd.rs` (the handover),
  `grooveseek/src/transport/mod.rs` (`HttpListen`, `resolve_systemd_listen`,
  `systemd_socket_supported`), `grooveseek/src/transport/http.rs`
  (`open_listener`, `PeerRule`, `admin_peer_rule`,
  `effective_allowed_hosts_unix`, `effective_allowed_origins_unix`).
- Japanese version:
  [0021-take-the-socket-you-were-given.ja.md](0021-take-the-socket-you-were-given.ja.md)
