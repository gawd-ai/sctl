# TRD-9: Release 0.6.12

Implements [ADR-010](../adr/010-a-quiet-agent-proves-it-is-checking.md), [ADR-011](../adr/011-file-reads-read-what-is-there-and-refuse-secrets-and-devices.md) and [ADR-012](../adr/012-net-tools-send-only-on-the-lan-to-the-device-expected.md), and carries two changes that need no decision of their own: one name for the agent's tools on every unit, and an attach that keeps the shared output stream. Status: Planned. Release: sctl 0.6.12. (TRD-7 is held for ADR-007's option B, and ADR-008 and ADR-009 for fleet TRD-28 and TRD-29, so the decisions are ADR-010 to ADR-012 and this is TRD-9.)

## 1. Objective

One release, after which:

- a unit that pushes its Infra results only when they change also proves, on the poll netage-server already makes, that its monitor is checking: silence over a live tunnel reads as steady, and a stopped monitor is one reported fact (ADR-010);
- the monitor keeps a slow target Degraded instead of flapping, pushes a Down before it runs a recovery, never fires a recovery on a good answer, keeps every monitor supervised, ignores a config it already holds, and records a reading a person asks for (ADR-010);
- a file read returns what the file holds, the kernel's neighbour and route tables included, and refuses the agent's keys, process secrets, kernel streams and anything that is not a regular file before it opens anything (ADR-011);
- `sctl net` sends only on the unit's LAN side, bound to the interface it checked, to the device whose MAC the command names, and takes the SNMP community from the environment only (ADR-012);
- a scan sends only on the LAN side, reports only MACs it saw, and marks the unit's own addresses (ADR-012);
- every unit runs the agent's tools as `/usr/bin/sctl`, whatever its layout;
- a client that attaches to a session no longer cuts another watcher's output, and no client shows a line twice or loses one.

Every fleet change keeps working on 0.6.11 until the rollout reaches a unit, once section 5's prerequisites hold. No relay change is needed.

## 2. What 0.6.11 has

Each ADR's section 2 holds what its decision found. This section holds what the release needs besides. Lines are at `ece8b45`.

- **Versions and toolchain.** The five version carriers read 0.6.11: `server/Cargo.toml:3`, `mcp/Cargo.toml:3`, `crates/sctl-comms-abi/Cargo.toml:3`, `drivers/sctl-comms-quectel/Cargo.toml:3` and `web/package.json:3`. The 0.6.11 release commit (`9926bfd`) also moved the four entries of this repository's crates in `Cargo.lock` and lines 3 and 9 of `web/package-lock.json`. The tag must equal `server/Cargo.toml`'s version (`docs/releasing.md:24-27,251-253`). `rust-toolchain.toml` pins 1.97.1 with rustfmt and clippy, and the root `Cargo.toml` denies clippy `all` and warns on `pedantic` (`Cargo.toml:23-25`).
- **Main.** `main` is 10 commits past `v0.6.11`, at `ece8b45` (2026-10-13). `server/src` changed only through the public-name scrub (9 files, 40 insertions, 39 deletions), so the agent on `main` behaves as 0.6.11. `CHANGELOG.md` `[Unreleased]` holds only TRD-8's devices entry; `f1999ac` (relay tooling keeps every key on the relay) and `5cdf6bf` (the WE826's WAN preference probes public anycast targets by default) are not in it.
- **Line ends.** The files this release edits that use CRLF: `CHANGELOG.md` (the only changelog, `CHANGELOG.md:8-9`), `web/package.json`, `web/package-lock.json`, `web/src/lib/utils/ws-client.ts`, `web/src/lib/types/terminal.types.ts`, `SECURITY.md` and `server/docs/REVIEW.md`. `mcp/Cargo.toml` is mixed: lines 1 to 30 CRLF, 31 to 33 LF. Every edit to them is byte-safe and keeps each line's ending, and a new file in `web/src/lib/utils/` takes its neighbours' CRLF. Every other file this release edits is LF.
- **CI.** On 1.97.1 (`.github/workflows/ci.yml`): `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`; the HTTP API docs gate (a `docs/http-api.md` heading for every `.route()`) and the config docs gate (every `pub` field of `server/src/config.rs` in `docs/config.md` and `server/sctl.toml.example`); the ts-rs bindings diff; `cargo doc`; sctlin's check, test and package. The `Release` workflow, on `v*` tags, signs the bundle, proves that every server artifact embeds the manifest's version, and only then publishes (`.github/workflows/release.yml:136-181`, ADR-001). `docs/releasing.md:268` says the proof comes before signing.
- **Tests.** Root-only network tests follow `server/tests/relay_route_netns.rs`: `#[ignore]`, root and `SCTL_NETNS_TEST=1`, built with `--no-run` and run as the test binary under sudo, every command an argument vector. The `scripts/netns-test.sh` that TRD-5 names never existed in git. `server/tests/relay_events.rs` runs fake devices against the real relay router, so it cannot test the device's own attach handler. `server/tests/xe300_relay_route_sh.rs` runs a device script under BusyBox with stand-ins named by absolute path in a scratch directory. Nothing tests `gawdxfer/*.rs`, `infra/discovery.rs` or `infra/routes.rs`; no test in `tunnel/client.rs` covers attach; the tests in `routes/files.rs` cover only `Content-Disposition`; sctlin's four util test files cover neither the terminal nor the WebSocket client.
- **Payloads.** Device payloads are built with `devices/build.sh <target>`, never with cargo or cross by hand. Only the two mips targets ship gzipped (`.github/workflows/release.yml:150-159`). A 0.6.11-era `mips_24kc` set is 3,923,078 bytes (the server 3,694,561, TRD-6; the plugin 228,517), and a WE826 also holds musl `libc` and `libgcc` (1,257,347 bytes gzipped). `x86_64`, `aarch64`, `armv7` and `riscv64` ship raw: a `riscv64` server and plugin built on 2026-09-30 are 7,363,856 bytes. ADR-002 counted a 0.6.7 mips set at 3.8 MB.

## 3. Deliverables

Each change says what changes, in which files, how it is tested and accepted, and what the fleet does with it. Section 6 runs the checks that cover them all.

### 3.1 File reads read what is there, and refuse secrets and devices (ADR-011)

**The gate.** `server/src/file_access.rs` (new) is the one gate before a file's content is read, for `GET /api/files` (and the legacy `tunnel.file.read`, which calls it), `GET /api/files/raw` and gawdxfer's download init (`POST /api/stp/download`, `gx.download.init`). `TransferManager` is given it when it is built. In order:

1. The path as given, split into components with `.` and empty ones dropped, is matched against the rules below. A match is `PROTECTED_PATH` before anything else, so a path the rules refuse gets that code on every route.
2. A handle that opens nothing: `O_PATH | O_CLOEXEC`, links followed. No FIFO waits and no driver's open runs.
3. From the handle, through calls every unit kernel takes: the real path (`readlink` of `/proc/self/fd/<n>`), the type and link count (`fstatat` with `AT_EMPTY_PATH`) and the filesystem (`statfs` of `/proc/self/fd/<n>`). The WE826 runs Linux 3.3.8, and the kernel takes an `O_PATH` descriptor in `fstat` only from 3.6 and in `fstatfs` only from 3.12 (open(2)).
4. A file with no link left (`st_nlink` 0) is `FILE_NOT_FOUND`, whatever its real path says.
5. The real path is matched against the rules. On procfs and sysfs only ADR-011's short list is read, matched on the path as given; anything else there, and any file on debugfs or tracefs, is `PROTECTED_PATH`. The rules come before the file's type, so a path they refuse gets `PROTECTED_PATH` on every route.
6. A FIFO, a socket, a character device or a block device is `NOT_A_REGULAR_FILE` and is never opened. On `files/raw` and in a transfer, so is a listed pseudo file (below).
7. The file is opened through `/proc/self/fd/<n>`, read-only, with `O_NONBLOCK`, `O_NOCTTY` and `O_CLOEXEC`: the file read is the file judged.
8. A regular file is read up to the size its stat gave (from the offset, within the cap), or until it ends. A listed pseudo file is read until `read()` returns 0 or the cap. Short reads are continued.

**The rules.** Each matches its components anywhere in the path, so the same file reached through `/overlay/upper/...`, `/rom/...` or `/proc/<pid>/root/...` is refused too.

- The agent's directories, taken as configured and as real paths, resolved again at each read so a directory made after start is covered once it exists: `/etc/sctl`; `state_dir`; `data_dir` when it is also `state_dir`, else `<data_dir>/sessions`; the rollback directory `install.json` names.
- Names, wherever they sit: the config file the agent was started with, by real path; any file whose name begins with that file's name, with `sctl.toml` (the rollback copy's name), with `infra-secrets.json` or with `infra-monitor.json`. The last holds the SNMP communities a pushed config carries, and the WE826's one-time move leaves a copy of `state_dir` in `/tmp` until the next boot (TRD-8).
- A process's secrets and the kernel's streams, `<pid>` being a number, `self` or `thread-self`: `/proc/<pid>/environ`, `mem`, `cwd`, `fd` and `map_files`, each also under `task/<tid>`; `/proc/kcore`, `/proc/kmsg` and `/proc/sysrq-trigger`. A test pins that no entry of the short list can match them.

Staff then read these with exec, not through the file routes: where `data_dir` is `state_dir` (the XE300, the BPIs, the relay), `last_panic.log`, the tunnel history and, on the relay, its artifact store and snapshots; everywhere, the upgrade helper's log and `state.json` under `<state_dir>/upgrade`. The relay's own agent follows the same rules. The secrets of other services on the relay host, like every firmware secret, are outside the gate (ADR-011 decision 8).

**The routes.**

- `GET /api/files` reads through the gate, under a cap: `max_file_size`, or the request's `limit` below it. On a regular file `size` stays the file's size, and a file over the cap with no range is still `FILE_TOO_LARGE` before it is read. On a listed pseudo file `size` counts the bytes to where the read stopped, and `truncated` is true when the cap stopped it. Through the relay the cap is at most 4 MiB, half the tunnel's 8 MiB body (`server/src/tunnel/client.rs:2394,2459-2473`): the tunnel marks the requests it dispatches, and an answer that would still not fit is `FILE_TOO_LARGE`, never the tunnel's 502.
- `GET /api/files/raw` moves only regular files on a filesystem that reports their size, so not on procfs or sysfs: exactly the size its stat gave, with that `Content-Length`, so a log that grows during the download is cut where its size was read. A listed pseudo file is `NOT_A_REGULAR_FILE`, the message naming its filesystem.
- A gawdxfer download passes the gate at init and refuses a listed pseudo file as the raw route does. It takes its place under `max_concurrent_transfers` before its hash (`MAX_TRANSFERS` past it), so at most that many hashes run, and the hash reads exactly the size the init's stat gave. A chunk is read only from the file that was hashed (the same device and inode, still regular), else `FILE_CHANGED`. An empty file is still one empty chunk.
- Two codes: `PROTECTED_PATH` (403) and `NOT_A_REGULAR_FILE` (400), in `server/src/error.rs`, both catalogs of `docs/errors.md`, the STP status map (`server/src/routes/stp.rs:147-156`) and the tunnel's (`gx_error_response`, `server/src/tunnel/client.rs:3192-3203`, which today answers 400 for any code it does not know). The message names the rule or the filesystem, never the content.
- A refusal is one `FileRead` activity entry whose summary says the read was refused and whose detail names the rule.
- Writes, uploads, deletes and listings keep today's rules (ADR-011 decision 8).

**Files.** `server/src/file_access.rs` (new), `server/src/lib.rs`, `server/src/routes/files.rs` (also its comments at 14, "50 MB", and 69, "the path as given"), `server/src/gawdxfer/manager.rs`, `server/src/gawdxfer/hasher.rs`, `server/src/routes/stp.rs`, `server/src/tunnel/client.rs` (the tunnel's mark, `gx_error_response`), `server/src/error.rs`, `server/src/main.rs` (the gate for `TransferManager`), `server/src/infra/mod.rs:152` (the credential store is in `state_dir`); `docs/http-api.md` (Files, raw and STP: the codes, `size` on a pseudo file, the relay's 4 MiB), `docs/errors.md`, `docs/config.md` (`max_file_size` also ends a pseudo file's read), `docs/safe-mode.md` (the panic log through exec), `docs/guide.md:107` (the key in the config file, not in a systemd `Environment` line), `SECURITY.md` and `server/docs/REVIEW.md` (the gate), and TRD-4's BPI recipe (it reads `sctl.toml` with exec).

**Tests** (`cargo test`, no root). Each refusal expects the code the gate gives in its order.

- The read loop over a reader that returns a few bytes at a time; and, called below the gate, on `/proc/self/smaps`, several pages long, which it returns whole.
- The handle's facts (`fstatat`, `readlink`, `statfs`) against `std::fs` for a regular file, a FIFO, a directory and `/dev/null`.
- Through the gate: a regular file whole and by range; `/proc/net/arp` and `/proc/meminfo` to their end, `size` equal to the bytes returned; `/sys/class/net/lo/address`, its 18 bytes, `size` 18, `truncated` false.
- `PROTECTED_PATH` on the path as given, before any handle: `/proc/self/environ`, `/proc/thread-self/environ`, `/proc/self/task/<tid>/environ`, `/proc/self/fd/0`, `/proc/1/root/proc/self/environ`, `/proc/kmsg`, `/proc/kcore`. Off the list, from the handle's filesystem: `/proc/self/maps`, `/proc/self/cmdline`.
- `NOT_A_REGULAR_FILE` at once: a FIFO made in a temp directory, with no writer; `/dev/null`.
- A temp config given as the agent's: refused directly, through a symbolic link, through `/proc/self/root/...`, and as a copy named `sctl.toml.bak` in another temp directory. A temp `state_dir`: refused, and so are its `infra-secrets.json` and `infra-monitor.json` copied elsewhere. A `data_dir` apart from `state_dir`: `sessions/` refused, `last_panic.log` read. A rollback directory created after the gate was built: refused.
- The matcher on paths alone: `/overlay/upper/etc/sctl/sctl.toml` refused, `/home/dev/x` allowed, and no list entry matches a process rule.
- Through the relay's mark: a 5 MiB file with no range is `FILE_TOO_LARGE` before it is read; with a 1 MiB `limit` it reads; on the LAN port the whole file reads.
- Raw: a file appended to during the download sends exactly its `Content-Length`; `/proc/net/arp` is `NOT_A_REGULAR_FILE`.
- gawdxfer: a FIFO refused at init without waiting; `/proc/net/arp` refused at init; with four hashes held by a test hook, a fifth init is `MAX_TRANSFERS`; a file replaced by another inode with the same mtime between init and a chunk is `FILE_CHANGED`.
- Both status maps give 403 and 400; one activity entry per refusal.

**Accepted when.**

- Through a relay, `GET /api/files?path=/proc/net/arp` returns the table.
- 40 FIFO reads in a row answer at once, and relayed REST keeps answering.
- Reads of the agent's config, its credential store, its monitor config and `/proc/self/environ` are `PROTECTED_PATH` with the rule, directly, through a link and through `/proc/self/root`, each with one activity entry. On each install layout (a bench may stage one with `SCTL_INSTALL_JSON`), that layout's key directories are refused, and no refusal opens the file.
- On a WE826 (Linux 3.3.8), the gate answers as it does on the bench's kernel.
- `PUT /api/files` to `/etc/sctl/watchdog.sh` still works.

**The fleet.** Its deny-list (fleet TRD-35 slice 1) stays: it is the gate members meet for every agent version, and it words the refusal. It reads `PROTECTED_PATH` and `NOT_A_REGULAR_FILE` as refusals, never as errors worth a retry. On 0.6.12 a listed procfs file reads through the file API; its exec read of `/proc/net/arp` stays for older agents.

### 3.2 A quiet agent proves it is checking (ADR-010)

**Change.**

- `GET /api/info` gains the group `infra`, part of `all`, so the poll netage-server already makes carries it with no change to its request; `?groups=infra` answers it alone. Every agent of this release answers it, a unit that holds no config included. It is not in `GET /api/health`, which answers without the key.

  ```json
  "infra": {"rounds": 17280, "seq": 42, "hash": "3f9a0c1d2e4b5a69"}
  ```

  - `rounds`: monitor rounds since the agent started. A round is counted at the end of a tick (every 5 s), and only while every running check is within 90 s plus one tick of taking its permit. It never goes back while the agent runs.
  - `seq`: 1 when the agent starts, +1 at every change it pushes: a target's status, a config applied or removed, a recovery run logged. A push or a removal that changes nothing is not a change.
  - `hash`: the first 16 hex characters, lowercase, of the SHA-256 of this text: the agent's `boot` in decimal, a newline, the config version in decimal (0 for none), a newline, then per target in byte order of its id: the id, a space, its status (`unknown`, `up`, `degraded` or `down`), a newline. While the agent runs it changes only when `seq` moves, and it can return to an earlier value. A test vector (a text and its digest) sits in `docs/http-api.md` and in the tests, for the fleet's tests to assert too. A release that hashes other text names the field anew.
  - `off`: present only when no monitor is expected: `safe_mode` in an agent that started in safe mode, whatever the flag file says now, else `no_config`.

  A steady unit's group is about 60 bytes.
- The group lives in a small cell that every writer of the results updates. `GET /api/info` reads the cell and never waits for the Infra lock, so a lock held for good shows as a stalled `rounds`, never as a slow answer.
- The tick no longer waits for its checks. It starts every due target with no check in flight and skips one with a check in flight; at most 3 checks run at once, and permits are served in order. The in-flight set lives in the Infra state, so a rebuilt monitor and Check now (3.3) see it too. Due times and cooldowns use the monotonic clock.
- A check that panics is logged, and its target reads `unknown` with a detail saying the agent failed to check it. It never fires a recovery.
- Each `infra.state` gains `boot` (the agent's start, unix ms, as `net.state` carries it), `seq`, `hash` and `recoveries` (recovery runs logged since `boot`), as they stood when it was built, and goes out whenever `seq` has moved since the last one sent, still at most one a second; `v` stays 1. `GET /api/infra/results` gains the same four, so a pull settles a comparison. The event stream's dedup key for `infra.state` becomes `(relay_epoch, connection_id, state.boot, state.seq)` (`docs/http-api.md:1246-1250`).
- `SCTL_INFRA_TEST_STALL_SECS`, for tests and benches as `SCTL_MWAN3_STATE_DIR` is (TRD-5): the monitor loop stops ticking that many seconds after it starts, with a warning at start.

**Files.** `server/src/infra/mod.rs` (the counters, the hash, the cell, the in-flight set), `server/src/infra/monitor.rs` (the tick, the monotonic clock, panics, the stall hook), `server/src/routes/info.rs` (the group), `server/src/tunnel/infra_state.rs` (the four fields, a send when `seq` moved), `server/src/infra/routes.rs` (`get_results`), `docs/http-api.md` (`GET /api/info`, `GET /api/infra/results`, `infra.state` and its dedup key).

**Tests.**

- The group: `groups=infra` alone, `all` with it, `core` without it; `off` reads `no_config` with no config, is absent while a monitor runs, and reads `safe_mode` in safe mode; the example above serializes to about 60 bytes.
- `rounds` moves every tick while a `custom_script` check sleeps 12 s, and that target does not start again before its check ends.
- The round rule as a function of the in-flight checks and their permit times; a tick that panics counts no round.
- With the Infra lock held by the test for 12 s, `/api/info` answers at once and `rounds` stands still.
- A panicking check (a test hook): its target reads `unknown` with the detail, no recovery runs, and `rounds` goes on.
- `seq` moves by one on a status change and on a logged run, not on a latency reading and not on an identical push; `hash` is the same whatever the order of the targets; the test vector.
- `infra.state` goes out when `seq` moves and not on a latency reading, and carries `boot`, `seq`, `hash` and `recoveries`; so does `GET /api/infra/results`.
- The stall hook stops `rounds` within one tick.

**Accepted when.**

- On a unit holding a config, two `GET /api/info?groups=infra` 30 s apart show `rounds` up by at least 5, also while one check takes 60 s.
- A status change moves `seq` and `hash`, and the next `infra.state`, within a second, carries the same values.
- The stall hook stops `rounds` within one tick while `/api/info` keeps answering.

**The fleet** (fleet ADR-0041, TRD-38). netage-server reads the group from the `/api/info` answer it already fetches, keeps as its copy the last report or pull, hashed by the text above, and reacts to the readings ADR-010 decision 4 defines: one "monitoring stopped" event when `rounds` stands still with no `off`, one pull on a miss. A matching `hash` also proves the copy's config version, so a push of the same config can be skipped. A unit whose `/api/info` has no `infra` group runs an agent before 0.6.12 and gets fleet ADR-0041's bounded pull, never a stall. No separate pulse is built: no unit's health poll is stretched today. If one ever is, or the poll is slimmed, `GET /api/info?groups=infra` is the cheap read of the proof, and a pulse that carries the group is the fallback decided then (fleet ADR-0041).

### 3.3 The monitor (ADR-010)

**Change.**

- **A config the agent holds is not applied again.** `InfraConfig` and its parts derive `PartialEq`. A push equal to the held config, every field and the version included, aborts, seeds and signals nothing, and writes the config again only if its last save failed (today `push_config` ignores the save's result, `infra/routes.rs:63`). Outside safe mode, if no monitor runs for that config, the push starts one. The answer gains two booleans, `unchanged` and `started`. A push with version 0 is refused (400, `INVALID_REQUEST`): 0 means the unit holds no config, and the fleet's fingerprint is never 0. The version is documented as the caller's name for a config, not as monotonically increasing. Today a re-push of the same config restarts the monitor and sends one `infra.state` whenever a reading has moved since the last one (`infra/mod.rs:453`, `tunnel/infra_state.rs:213-217`).
- **Every monitor is supervised.** `spawn_supervised` and `supervise_spawned` move from `server/src/main.rs:549-601` to `server/src/supervise.rs` in the library. `infra::monitor::start` is the one start path, at boot and for a push, and the supervisor rebuilds a panicked loop after 5 s from the config the unit then holds, not from a copy.
- **Safe mode keeps the monitor off.** `InfraState` knows that the agent started in safe mode (`main.rs:631-633`). A push then stores and saves the config, answers as usual and starts nothing; `docs/safe-mode.md` gains that line.
- **Degraded stays Degraded while answers are slow.** A good answer above `degraded_threshold_ms` is slow, and a slow answer never makes a target Up. The agent counts good answers within the threshold in a row, beside `consecutive_ok` and `consecutive_fail`; that count stays inside the agent.

  | From | Good, within | Good, slow | Failure |
  |---|---|---|---|
  | Unknown | Up | Degraded | Degraded |
  | Up | Up | Degraded | Degraded |
  | Degraded | Up at `up_after` such answers in a row | Degraded | Down at `down_after` failures in a row |
  | Down | see below | see below | Down |

  From Down, `up_after` good answers in a row lead to Up when all were within the threshold, and to Degraded otherwise; until then the target stays Down, so one slow answer leaves it Down.
- **A recovery fires only on a failed monitor check that leaves the target Down**, within `cooldown_secs` and `max_retries`: the first Down and every failed check after it, never a good answer. Today it also fires on a good answer that leaves the target Down (`infra/monitor.rs:202-209,273-279`), and can restart a device as it comes back. The module docs that say it fires on the transition are corrected.
- **A recovery never holds the lock, and the Down goes first.** Under the lock the result is folded and the Down signalled, so its push leaves first. The recovery starts in a task of its own, outside the monitor's task set, which takes its command and the credentials it names under the lock and releases it; the check's permit is released too. One run per target at a time: the target stays in flight until its run ends. Before the command starts, the run is recorded: the cooldown begins, the count goes up, and the record, marked as not ended, is written to `<state_dir>/infra-recovery.json` and synced.
- **A run is not cut by the monitor's life.** It ends by itself or at its 300 s limit. A push, a rebuilt monitor or a removed config does not end it. A managed upgrade waits for a running recovery, at most that limit, before it hands off to its helper (`server/src/upgrade/stage.rs:783-792`). Any other stop (a person's restart, a crash, a power loss, or the upgrade's restart for a run begun after the hand-off) can still end a run with the agent; the next start logs each run still marked as not ended, its outcome unknown.
- **Its output is masked and cut safely.** stdout and stderr are masked as today, each cut to 512 bytes on a character boundary as `at.rs` cuts, then logged under the lock. stderr is kept because a failed command says why there; it gets stdout's mask and its limits: a password under 4 bytes and a username stay as they are, and a shell trace writes expanded commands to stderr. A logged run moves `seq` and `recoveries` and is pushed.
- **The count survives restarts and clears on an answer.** After a start, a target with a saved count waits a full cooldown, and its count stands, so `max_retries` holds across the restarts a recovery causes. The count clears when a good answer moves the target out of Down or `unknown`, so a target that answers after a restart starts afresh. A removed config clears every count, and a target that a push removes loses its own.
- **Check now records its reading.** `POST /api/infra/check/{target_id}` runs under the 90 s deadline, and its result is applied as a monitor check's is: `last_check`, latency, detail, the counters, the status and, for a profile check, a data sample. A status change is signalled and pushed. Check now counts no round, never starts a recovery, and answers as today. A target has at most one check in flight: Check now on a target whose check is in flight waits for that check, at most its deadline, and answers its result. A result is applied only while the unit still holds the config the check ran under and the target is in it; otherwise it is answered and nothing is applied, and the same holds for a monitor check whose monitor was replaced.

**Files.** `server/src/infra/routes.rs`, `server/src/infra/monitor.rs`, `server/src/infra/mod.rs` (`PartialEq`, the saved runs, the module docs), `server/src/infra/checks.rs` (stderr), `server/src/supervise.rs` (new), `server/src/lib.rs`, `server/src/main.rs`, `server/src/upgrade/stage.rs` (the wait before the hand-off), `docs/http-api.md` (`POST /api/infra/config`, `GET /api/infra/results`, `POST /api/infra/check/{target_id}`), `docs/safe-mode.md`, `docs/upgrade.md` (the wait).

**Tests.**

- An identical push answers `unchanged: true`, sends no `infra.state` and keeps the same monitor task; one community changed under the same version is applied; an identical push after the monitor ended starts it (`started: true`); after a failed save it writes the file; version 0 is refused.
- A panic injected in a monitor a push started is rebuilt after 5 s, from the config held then.
- In safe mode a push saves the config and starts nothing, and the group reads `off: safe_mode`.
- `compute_status` as the table: a target answering steadily at 1.5 times its threshold reads Degraded for 50 answers, with one change and one push; `up_after` answers within the threshold bring it Up; one slow good answer leaves Down as Down; `up_after` good answers with one slow take Down to Degraded; a slow first answer takes Unknown to Degraded.
- A good answer while Down fires no recovery; a failed one past the cooldown does.
- A recovery `sleep 3` with the forwarder running: the Down `infra.state` leaves before the command ends; a Down check during the run starts no second run; another target's checks go on; a config push during the run lets it finish, logged once; the saved record is on disk before the command starts; 600 bytes of three-byte characters on stdout and on stderr are cut without a panic.
- The upgrade's hand-off, given a run in flight, waits for its end (the wait alone; no helper is started).
- At start, a record marked as not ended is logged with its outcome unknown and moves `seq` and `recoveries`; the target waits a full cooldown and its count stands; the first good answer out of `unknown` clears it; a target dropped by a push loses its record.
- Check now moves `last_check` and the latency, moves the status on the failure that crosses `down_after`, pushes that change and starts no recovery; on a target whose monitor check is in flight it waits for that check and answers its result, and no second check runs; a result whose config changed during the check is not applied.

**Accepted when.**

- The fleet's push of an unchanged config costs the unit nothing.
- A slow, steady target is one Degraded span, not a stream of flaps.
- A Down reaches the relay within a second, whatever its recovery takes, and no push, rebuild or removed config cuts a recovery.
- A recovery's multi-byte output never runs the recovery again.

**The fleet.** Its push skip for an unchanged fingerprint (fleet TRD-35 slice 6) stays for older agents; on 0.6.12 an identical push is a no-op either way, and `unchanged` may be ignored. A unit whose group reads `off: safe_mode` reads "monitoring off: safe mode". Fewer Degraded and Up transitions reach the record. An automatic recovery reaches the fleet through `seq`, `recoveries` and one pull. Check now stays its own fact (fleet TRD-35 slice 4) and now also moves the record, its status included: a few quick clicks on a failing target bring the Down, and with it the recovery, sooner.

### 3.4 Net tools send only on the LAN, to the device expected (ADR-012)

**Change.** The guard covers `sctl net snmp`, `http` and `tcp`, and the two Infra checks that send a stored secret: the SNMP check (its community, every interval) and the Peplink check (its login). Off the LAN such a check reads down with the guard's reason. `POST /api/fetch` and the checks that send no secret (ping, HTTP through curl, TCP) keep today's behaviour.

- **The guard** (`server/src/net_tools/guard.rs`, new) runs in the process that sends, before anything is sent. A host name is resolved once; the guard judges its first IPv4 address, and the tool connects to that address only (HTTP keeps the name for `Host` and TLS). It asks the kernel for the route (`source::route_get`, which now also returns the route's type) and sends only when it is a unicast route with no gateway that leaves by a LAN-side interface holding an IPv4 address, or when the destination is the unit's own (loopback or one of its addresses), since nothing leaves the unit. Otherwise it refuses: `not_on_lan`, or `no_address` when that interface holds none; `not_unicast` for broadcast or multicast; `ipv6` for an IPv6 destination; `guard_failed` when it cannot read the routes or bind. It never sends unguarded.
- **The LAN side.** An interface is on the LAN side when it is up, not loopback, takes ARP (`IFF_NOARP` clear), is not point-to-point, and is not the LTE netdev. WireGuard, tun, PPP and a raw-IP modem fall out by their flags; a modem framed like Ethernet, as the XE300's `wwan0`, by its name in the config. A wired WAN port is on the LAN side: on a probe, the site's network is there. The tool reads every address with its prefix and each link's flags over netlink. Each interface's role (`lan`, `lte`, or `tunnel` for a point-to-point or no-ARP link) appears in refusals and in the discovery subnet list.
- **The config a tool reads.** The LTE netdev is the agent's `[lte] interface`. A tool reads the config from `--config <path>`, today taken only by `http` and undocumented, now documented and taken by all three; else from `SCTL_AGENT_CONFIG`, which the agent sets to the absolute path of the file it loaded, in the environment of every command a caller or a pushed config gives it to run: exec, sessions and their jobs, Infra checks and recoveries. It never reads `./sctl.toml` from its working directory (`/tmp` on the units, writable by any local process). A config that cannot be read or parsed is a refusal (`config_unreadable`) naming the path and quoting nothing from it, never a panic (`Config::load` panics today, `config.rs:850-858`) and never a quiet fall back to defaults. With no config, one without `[lte]`, or one whose `[lte] interface` the unit lacks, an interface named like a supported model's modem bearer (`wwan*`, `qmimux*`, `usb*`) counts as LTE, and the answer says the name decided it. `sctl net http` then keeps its TLS pins in the agent's `state_dir`; a pin recorded under the default directory is asked for again.
- **Bound to what the guard approved.** The socket is bound to the approved interface (`SO_BINDTODEVICE`) and to the source address the route gives, before anything is sent, so an address removed between the check and the send makes the send fail rather than move, and mwan3's marks cannot move it. `tcp` binds its own socket. `http` passes the checked address and interface to `routes::fetch::execute` in `FetchRequest` fields the wire never sets (`#[serde(skip)]`); `POST /api/fetch` leaves them empty.
- **SNMP from a bound socket.** The agent encodes the requests it sends (GET, GETNEXT and SET, v1 and v2c) itself and sends them from a UDP socket the guard bound and connected. `snmp2` still decodes the answer (`Pdu::from_bytes`, public, its `pdu.rs:567`), and the agent checks what `snmp2` checks only inside its crate (`pdu.rs:654`): the request id, the community and the PDU type; anything else is ignored until the timeout. `snmp2` 0.5.2 binds its own socket and keeps its encoders to itself (its `asyncsession.rs:69-110`, `pdu.rs:311-506`).
- **`--dev <iface>`.** The interface must be on the LAN side and hold an address (a bridge port is named by its bridge), and the destination must lie under a route in the main table through it with no gateway: its own network, or a host or network route a runbook added (`not_on_lan`, `no_address`). This is judged from the routes, never from a lookup tied to the interface, which the kernel answers as if any address were on the link.
- **`--expect-mac <mac>`.** After the route check and before any payload, the tool sends one ARP request for the destination on the bound interface (a packet socket) and waits within its timeout. It sends only when every answer names the expected MAC, and so does the kernel's neighbour entry for the address if it holds one (`RTM_GETNEIGH`, read after the answers). Another MAC, or two, is `mac_mismatch`; no answer is `no_answer`. With the flag, a destination the unit holds itself is `own_address`. No packet socket is `guard_failed`, never a skipped check. MACs compare without regard to case, with `:` or `-` between bytes; anything else is a usage error. The check proves that the device answering holds that MAC, no more: a generated, locally administered or shared (VRRP) MAC passes for whichever device holds it.
- **HTTP redirects.** Every hop passes the guard on its own, `--dev` included. A redirect to another scheme, host or port drops the Basic credentials, every header the caller set and the body, and is not followed when it would resend a body (a 307 or 308 after a request with one), nor, with `--expect-mac`, to another address. A redirect not followed, for these reasons or because its hop is refused, ends the request: the answer is that redirect (status and `Location`) with `not_followed` and the reason, exit 1, since the first request was sent.
- **The SNMP community comes from `SCTL_SNMP_COMMUNITY` only.** `-c` and `--community` are a usage error (exit 2) whose message shows the environment form, `SCTL_SNMP_COMMUNITY="${SCTL_CRED_PASS_1}" sctl net snmp ...`, without the value. A variable set but empty is a usage error too, never `public`. Unset, `public` stays the default for get and walk, and set still needs a community. The Infra SNMP check's detail never quotes a community (`infra/checks.rs:150-163` quotes an invalid one today).
- **`sctl net tcp` refuses an argument it does not know**, as `snmp` and `http` already do.
- **Answers.** Exit 0 done, 1 failed (the JSON says why), 2 usage, 3 refused (new): the request never left, at most a name lookup and the ARP request did. A refusal prints one JSON object: `ok: false`, `refused` (one of `not_on_lan`, `no_address`, `not_unicast`, `ipv6`, `own_address`, `mac_mismatch`, `no_answer`, `config_unreadable`, `guard_failed`), a one-line `reason`, `host` (the address judged), the route the kernel would have used (interface, role, gateway), the unit's LAN-side networks by interface and, after an ARP request, the MACs that answered. It never carries a secret, and no usage error repeats an option's value. `sctl help net` gains `--dev`, `--expect-mac`, `--config`, exit 3 and the codes, and loses `-c`.

**Files.** `server/src/net_tools/guard.rs` (new: the LAN side, the route check, `--dev`, the ARP request, the neighbour read), `server/src/net_tools/snmp.rs` (the encoder, the bound socket, the answer's checks), `server/src/net_tools/mod.rs` (the options, `--config`, exit 3, `sctl help net`), `server/src/net_tools/tcp.rs`, `server/src/net_tools/http.rs`, `server/src/routes/fetch.rs`, `server/src/netwatch/source.rs` (the route's type), `server/src/netwatch/netlink.rs` (neighbours), `server/src/infra/checks.rs` (the SNMP check through the guard, its detail), `server/src/infra/profiles/peplink.rs`, `server/src/shell/process.rs`, `server/src/shell/pty.rs` and `server/src/infra/checks.rs` (`SCTL_AGENT_CONFIG` for exec, sessions, jobs and Infra commands), `server/tests/net_tools_netns.rs` (new), a test SNMP agent in `server/tests/` that takes a bind address and an answer delay (TRD-6's `echo_agent`, `net_tools/snmp.rs:268-288`, is a private helper bound to 127.0.0.1 that stops after 500 ms idle), `docs/guide.md` (a "Network tools" section, which it lacks) and `docs/config.md` (`SCTL_AGENT_CONFIG` with the other variables).

**Tests.**

- Unit (`cargo test`, no root): MAC parsing and comparison; the parser for every option (`-c` refused with the environment form and no value echoed, an empty variable refused, `tcp` refusing an unknown argument); the LAN-side decision over injected link flags, addresses, routes and configs (on the link; through a gateway; by the configured LTE netdev; by a `wg`-named and a point-to-point interface with no gateway; broadcast; IPv6; the unit's own address); `--dev` over injected routes, an address under no route through the interface refused; the config rules (`--config` over the variable, no variable, no `[lte]` section, a missing `[lte] interface`, a JSON file given as the config); the SNMP encoder byte for byte against what `snmp2` 0.5.2 sends for the same request id, community and varbinds, captured on loopback, and the answer's checks; the refusal objects; the redirect rules.
- `server/tests/net_tools_netns.rs` (new; `#[ignore]`, root and `SCTL_NETNS_TEST=1`, built with `--no-run` and run as the test binary under sudo, as `relay_route_netns.rs` is). Every command it runs takes an argument vector, never a shell string, and acts only in network namespaces it creates and removes. The unit has a LAN veth; a veth named as the injected config's `[lte] interface`, whose default route leads to a gateway namespace that counts every packet it receives; and a `wg`-named veth with a route and no gateway. A target host on the LAN answers TCP, HTTP and SNMP, and a twin host can take the target's address with another MAC. Cases:
  1. `tcp`, `http` and `snmp get` to the target: sent.
  2. An SNMP set to an address behind the gateway: exit 3, `not_on_lan`, and a capture on every veth sees nothing for it.
  3. A destination reached by the `wg`-named veth, and one reached by a gatewayless default route on the LTE veth: refused, nothing sent.
  4. A /32 address on the LAN veth with a host route to the target: sent by the LAN veth. With `--dev`, the address removed mid-run never sends by the LTE veth, and an address under no route through the named veth exits 3 with nothing on the wire, ARP included.
  5. fwmark rules and a policy table that steer the unit's traffic to the LTE veth, as mwan3 does, do not move a bound request, SNMP included.
  6. `--expect-mac` with the target's MAC: sent, also after the neighbour table was flushed. Another MAC, two answers and no answer: refused, with only the ARP request on the wire. A STALE entry naming the twin: nothing reaches the twin; the payload goes to the target's MAC, or nothing is sent.
  7. A redirect to another host arrives without `Authorization`, the caller's headers or the body; a 307 to another host after a POST with `-F` is not followed; a redirect to an address behind the gateway is not followed.
  8. `snmp set` with `SCTL_SNMP_COMMUNITY`: the community reaches the agent, and it is in no argv (`/proc/<pid>/cmdline` while the agent delays its answer), stdout, stderr, check detail or activity-log line, refusals and the `-c` usage error included.
  9. A default scan, and a named subnet the unit holds no address on (3.5): nothing is sent by the LTE veth.

**Accepted when.** The netns cases pass as root. On a 0.6.12 unit, a stored power command in the environment form, sent with `--expect-mac`, runs. An SNMP check whose target the unit does not reach on the link reads down with the guard's reason. Nothing about `POST /api/fetch` changes.

**The fleet** (fleet ADR-0040). It stores commands in the environment form, `SCTL_SNMP_COMMUNITY="..." {{agent}} net snmp ...`, which 0.6.11 and 0.6.12 both run, and adds `--dev` and `--expect-mac` only when it sends to an agent from 0.6.12, before the positional arguments, through a version gate in `devices/server/infra.server.ts` (fleet) in the pattern of `RECOVERY_REFS_SINCE`. A unit that its own upgrade helper rolls back to 0.6.11 (`docs/upgrade.md:304-306`) therefore still runs every stored command. It reads exit 3, with `refused` in the tool's JSON, as "not sent" only when the command is the tool alone, and words each code for people and for the assistant. Its MAC check for 0.6.11 (one neighbour read before the send) stays for older agents. `--expect-mac` is only as good as the record's MAC, which fleet TRD-35 slice 3 and fleet ADR-0039 keep right.

### 3.5 Discovery (ADR-012)

**Change.**

- `SubnetEntry.role`: `lan`, `lte` or `tunnel`, by the guard's LAN-side rule. `GET /api/infra/discover/subnets` and the tunnel's `tunnel.infra.discover.subnets`, which calls `auto_detect_subnets` directly (`tunnel/client.rs:4415-4431`), list every subnet with its role and source, from the agent's config.
- With no subnets in the request, a scan sweeps the networks the unit holds an address on, on LAN-side interfaces. A /24 guessed from a default route and a subnet the sniff heard are swept only when a request names them.
- Whatever the request names, every send follows the guard: nmap and the ping fallback sweep a subnet only when it lies wholly under a no-gateway route through a LAN-side interface, and only on it; arp-scan, which stays on the link, runs only on LAN-side interfaces, so a saved subnet the unit has lost its address on is searched by ARP alone; the port probe judges each host by the guard and binds to the interface it approved. An LTE or tunnel network, or one behind a gateway, is never swept. The answer's `not_swept` names each subnet the scan did not sweep and why (`not_on_lan`, `too_large`).
- Phase 1 reads the neighbour table over netlink with each entry's interface and state, and keeps the LAN side only.
- A host's MAC is one seen answering during the scan: an ARP answer the sweep received (arp-scan's, or nmap's report of one), or the kernel's entry for the host on the interface that reaches it, read after the sweep and the port probe and REACHABLE then (or permanent). A host still without one gets the guard's ARP request on that interface: one MAC, else none. The sweep's answer no longer gives way to an older entry, and an address two MACs answered for gets none.
- A host at an address the unit holds now, or with one of its MACs, carries `own` with the name of its interface. The ping fallback skips the unit's addresses.
- The ping fallback pings the hosts of the subnet's own prefix, without the network and broadcast addresses (a /31 and a /32 have none) and without the unit's own, 32 at a time and never more than the 254 addresses it pings today. A larger subnet is swept only by nmap or arp-scan, else is `not_swept` as `too_large`. Each subnet's interface is found once per scan, without a second sniff.
- Phase 4's reverse lookups are unchanged and go wherever the unit's resolver sends them (ADR-012 section 7).
- The comment at `infra/discovery.rs:38-41` and `docs/http-api.md` say what the default is, and the docs give `own`, `role` and `not_swept`.

**Files.** `server/src/infra/discovery.rs`, `server/src/infra/routes.rs` (`discover_subnets` takes the config), `server/src/tunnel/client.rs` (the two discovery handlers pass it), `server/src/net_tools/guard.rs` (shared), `docs/http-api.md` (`POST /api/infra/discover`, `GET /api/infra/discover/subnets`).

**Tests** (the first in `discovery.rs`; every probe is a fake and nothing is sent).

- The neighbour reader over netlink fixtures: states, interfaces, an address with two MACs.
- Each interface's role with an injected `[lte] interface` (`usb0`, `qmimux0`), with flags (`IFF_NOARP`, `IFF_POINTOPOINT`) and with plain names.
- The default pick: the unit's own `lan` networks only. A named subnet behind a gateway: `not_swept`, `not_on_lan`.
- The hosts a prefix yields: 254 for a /24, 2 for a /30, 1 for a /32 and none when that address is the unit's own; a /23 is `too_large` for the fallback.
- The fallback never has more than 32 probes in flight.
- The MAC rules: a sweep's answer over an older entry; a STALE entry leads to an ARP request; two MACs give none.
- `own` by address and by MAC.
- In the netns rehearsal (3.4): a default scan sweeps no LTE or mesh subnet; a named subnet the unit holds no address on sends nothing by the LTE veth and is `not_swept`; a host the sweep found first, and one whose entry was STALE before the scan, come back with their MACs; the unit's own addresses carry `own`.

**Accepted when.** A scan given no subnets, on a unit with a wired network, an LTE bearer and a WireGuard interface, sweeps only the wired network. The subnets route lists all three with their role. Every host that answered carries its MAC, or none when two answered. The unit's own address comes back with `own`.

**The fleet.** On 0.6.12 it reads `role` instead of choosing LAN subnets itself, and stops its own neighbour read after a scan (fleet TRD-35 slice 3, which takes any complete entry, STALE included), which would put back the MAC the scan withholds. `own` feeds the addresses it remembers for the unit (fleet ADR-0039).

### 3.6 One name for the agent's tools

**What exists.**

- The layouts (`docs/upgrade.md:71-76`, `server/src/upgrade/install.rs:24-38`): `usr-bin` runs `/usr/bin/sctl` (the BPIs, `server/files/sctl.init:7`); `gz-tmp` expands gzipped payloads to `/tmp/sctl` at every start (the XE300, `devices/xe300/sctl.init:7-13,32-56`, and a RUT241 as installed, `devices/rut241/install.sh:96`); `ramboot` fetches into `/tmp/sctl` at boot (the RUT241 after `ramboot-migrate.sh`, and the WE826, whose uClibc firmware runs the agent through the musl loader that `install.json`'s `helper_prefix` names, `devices/common/sctl-ramboot.sh:232-236`); `systemd` runs `/usr/local/bin/sctl` (the relay, `server/files/sctl-relay.service`). The doc comments at `install.rs:30-35` give the RUT241 as `gz-tmp` only.
- A managed upgrade runs no install script: the helper places the files `install.json` names and runs the layout's restart (`docs/upgrade.md:286-306`). A stable name on a deployed unit must come from the agent.
- `GET /api/health` reports `target` and `layout` (`routes/health.rs:209-211`), and `sctl install-info` the install report. Nothing names what runs the agent's tools, and `sctl` alone is not found on an XE300 (exit 127, fleet TRD-32). The upgrade helper is a copy of `current_exe()`, or under a loader of `ramboot.conf`'s `BIN` (`upgrade/stage.rs:746-770`), so a link or a wrapper at another name does not change upgrades.
- `rundev.sh` writes `/usr/bin/sctl` in four places: `device deploy` copies a binary there (1517); `device upgrade` removes it, copies a generic binary there and writes a `usr-bin` `install.json` (1603-1606); the watchdog's rollback copies over it (1659-1660, 1731); and `device upgrade-remote` copies over it for a unit its layout probe calls `other` (2418), which the probe answers for a RUT241 on `ramboot` (2977).
- Overlays: about 100 MB on an XE300 (`devices/xe300/README.md:14`), 4 MB on a RUT241, 832 KB on a WE826.

**Change.** This gives every unit a stable name for the tools, not a stable install path: `gz-tmp` and `ramboot` units still run the agent from `/tmp`, `ramboot` by ADR-002's choice.

- At `serve` start the agent makes `/usr/bin/sctl` run its tools: on `gz-tmp`, and on `ramboot` without a `helper_prefix`, a symbolic link to the running binary; on `ramboot` with a `helper_prefix`, a short shell wrapper (`#!/bin/sh`, a marker line, then `exec` of the loader's words, the binary and `"$@"`, each word shell-quoted by the code), its binary `ramboot.conf`'s `BIN`; on `usr-bin` and `systemd`, nothing; with no `install.json`, nothing.
- It writes only when the name is absent, is a link, or is its own wrapper (known by the marker) with other content, through a temporary entry renamed over the name. It never replaces a regular file that is not its wrapper. A name already right is left alone, so a restart writes nothing to flash.
- `GET /api/health` and `sctl install-info` report `cli`: the path that runs the agent's tools on this unit (`/usr/bin/sctl`, or `/usr/local/bin/sctl` on the relay). It is absent when the agent could not make the name, and a log line says why.
- `SCTL_CLI_PATH` moves the name for tests and benches, as `SCTL_INSTALL_JSON` moves `install.json`.
- The install scripts do not change. A rollback to 0.6.11 leaves a link to `/tmp/sctl/sctl-server`, which 0.6.11 also runs from, or a wrapper with the same loader words.
- The code that finds the running binary and its loader words moves from `stage.rs` into `install.rs`, shared by the helper and the name.
- `rundev.sh`: `device deploy`, `device upgrade`, the watchdog's rollback and the `other` path of `device upgrade-remote` refuse a unit whose `/usr/bin/sctl` is a link or the agent's wrapper, rather than write through it.

**Files.** `server/src/upgrade/cli_name.rs` (new), `server/src/upgrade/install.rs` (and its doc comments), `server/src/upgrade/stage.rs`, `server/src/main.rs`, `server/src/routes/health.rs`, `rundev.sh`, `devices/we826-qwd/tests/upgrade-remote-test.sh`, `docs/upgrade.md` (the name per layout, and `cli` among what the agent reports), `docs/http-api.md` (`GET /api/health`).

**Tests.**

- In a temp root through `SCTL_CLI_PATH`: an absent name gets the link; a link elsewhere is replaced; the right link is left alone, with the same inode and mtime; a regular file that is not the wrapper is left, and `cli` is absent; a loader prefix gets the wrapper's exact text, mode 0755; the right wrapper is not written again; health and `install-info` carry `cli`.
- Under the system `sh` and BusyBox's, in the pattern of `server/tests/xe300_relay_route_sh.rs`: the wrapper the code writes runs a stand-in binary through a stand-in loader, both named by absolute path in a scratch directory, and both only print their arguments. The test passes arguments as an argument vector, never through a shell string: spaces, quotes, globs, `;`, backticks and `$(printf EXPANDED)`. Each arrives unchanged, and `EXPANDED` never appears. No argument names a command that acts.
- `upgrade-remote-test.sh`: the probe cases gain an XE300 with the link, a `ramboot` RUT241 with the link and a WE826 with the wrapper; none takes a path that writes through `/usr/bin/sctl`.

**Accepted when.** On each layout, after one start, `/usr/bin/sctl net tcp 127.0.0.1 <the agent's port>` exits 0 from a shell, and `GET /api/health` names that path as `cli`. A restart writes nothing.

**The fleet** (fleet ADR-0040). `{{agent}}` resolves to `/api/health`'s `cli` when the unit reports one. For an older agent the fleet maps the layout and the loader words the unit reports (`helper_prefix`), and refuses a unit that reports no layout (an agent before 0.6.7), naming its version.

### 3.7 Attach keeps the shared output stream

**What exists.**

- On `session.attach` the tunnel client aborts the session's subscriber, replays from `since`, and starts a new subscriber at the last replayed seq (`server/src/tunnel/client.rs:3619-3674`). One subscriber per session per tunnel connection feeds every relay client (`client.rs:4611-4677`; `server/src/sessions/mod.rs:525-537`), so a second client's attach drops what the old subscriber had not yet sent to the first.
- Live output is batched: one message carries up to 32 entries or 8 KiB under the seq of its last entry (`client.rs:53-54,3931-3988`). The subscriber moves its cursor only after it has sent a whole read.
- The replay is read under the buffer lock, and the lock is released before `session.attached` is queued (`client.rs:3629-3648`). The writer sends its priority lane, then its request lane, then its stream lane (`client.rs:1350-1358`).
- The relay adds an attaching client to the session's subscribers, then forwards the request, and fans session output to every subscribed client (`server/src/tunnel/relay.rs:2152-2163`). A direct `/api/ws` attach keeps a subscriber per connection (`server/src/ws/mod.rs:1118-1186`).
- sctlin listens for a session's output only once the attach has answered (`web/src/lib/components/TerminalContainer.svelte:294-336`), and `ws-client.ts` drops output nothing listens for (`web/src/lib/utils/ws-client.ts:280-309`). It writes every entry with no seq check, and attaches again on a `session.gap`.
- The MCP's buffer keeps duplicates (`mcp/src/websocket.rs:70-83`). Its exit and gap markers take the daemon's next seq and advance `last_seq` (`websocket.rs:827,955`), so a seq check would drop the real entry with that seq, and a re-attach from `last_seq` (`websocket.rs:1000-1002`) skips it.
- The fleet's Console tab uses sctlin's `TerminalContainer`, and the fleet pins sctlin 0.6.11.

**Change.**

- `session.attach` takes `dedupes: true`, sent by a client that writes an entry only when its seq is above the highest it holds for the session, and that listens for the session's output before it sends the attach.
- With `dedupes`, when a subscriber already runs for the session on this connection, the tunnel client keeps it and only answers the replay. The replay ends at that subscriber's read cursor: the seq of the last entry it took from the buffer, now kept beside its handle and moved under the buffer lock. Every batch the subscriber sends later starts above the replay's end, and every earlier one ends at or below it, so no batch spans it. The client reserves a slot on the request lane, then holds the buffer lock across the replay read and the queuing of `session.attached`, so no batch above the replay's end is queued before the reply. With no subscriber running, or without `dedupes`, the agent does as today. The relay and a direct `/api/ws` attach do not change.
- sctlin, in the same tag, listens before it attaches, holds live entries until the replay is written, then writes the replay and the held entries through one pure function that writes an entry only when its seq is above the session's highest; the live path and the buffered flush use the same function (`web/src/lib/utils/session-seq.ts` and `session-seq.test.ts`, new). It sends `dedupes: true`.
- The MCP: its markers no longer take the daemon's seq or move `last_seq`; the buffer orders its entries by a counter of its own; `last_seq` follows only the daemon's entries and is where a re-attach starts; `push` drops a daemon entry whose seq is not above it; its attaches send `dedupes: true`.

**Files.** `server/src/tunnel/client.rs`, `web/src/lib/components/TerminalContainer.svelte`, `web/src/lib/utils/ws-client.ts` (CRLF), `web/src/lib/types/terminal.types.ts` (CRLF), `web/src/lib/utils/session-seq.ts` and `session-seq.test.ts` (new, CRLF), `mcp/src/websocket.rs`, `docs/http-api.md` (the WebSocket's `session.attach`, and the relay's device socket).

**Tests.**

- The attach handler in `tunnel/client.rs`, with a real `OutputBuffer`, a running subscriber and the writer's three lanes: with `dedupes`, a second attach keeps the subscriber. With the stream lane full, so the subscriber lags, and a session printing a numbered line every 10 ms, the new client, keeping entries by seq, has every line exactly once and in order, judged by line content; the stream carries every line once for the clients already watching; no batch spans the replay's end. Without `dedupes`, today's restart, pinned, since older clients rely on it.
- sctlin vitest: a live entry held before the reply, a replay that overlaps it and later live output: each line written once, in order; the listener is registered before the attach is sent.
- The MCP: a gap marker followed by the real next entry keeps both; a repeated entry is dropped; a re-attach starts from the daemon's last seq.
- End to end through the real tunnel client and a bench relay, in section 6.

**Accepted when.** Opening a console on a session another person watches no longer cuts that person's output, and no client shows a line twice or loses one.

**The fleet.** It moves its sctlin pin (`gawd.vendor.json` (fleet)) to the 0.6.12 tag for the Console tab. The assistant's console read (fleet TRD-35 slice 7) already listens before it attaches and does not count the replay twice; it sends `dedupes: true` once it keeps entries by seq. The order is free: an older client is served as today.

### 3.8 The release

- `main` is fetched first. The line numbers here were checked at `ece8b45`, and other work is changing the gawd repositories.
- Version 0.6.12 in the five carriers, in `Cargo.lock` (the four entries of this repository's crates) and in `web/package-lock.json` (lines 3 and 9), as `9926bfd` did, each edit byte-safe.
- `CHANGELOG.md` `[0.6.12]`: every change above, `f1999ac`, `5cdf6bf`, and the `[Unreleased]` TRD-8 entry moved in.
- The index rows, added with these documents: this TRD in `docs/trd/README.md`, the first whose Implements cell names three ADRs; ADR-010 to ADR-012 in `docs/adr/README.md`, each `Accepted (2026-10-21)` and naming TRD-9, with a line that ADR-008 and ADR-009 are reserved for fleet TRD-28 and TRD-29, so a reader sees why 010 follows 007. This TRD's row flips to `Implemented (<date>)` in the change that ships the release.
- The tag `v0.6.12` equals `server/Cargo.toml`'s version, and the `Release` workflow runs TRD-1's proof. `docs/releasing.md:268` is corrected: the proof runs after signing and before the publish.
- Every target's artifacts, from `devices/build.sh` and from the workflow, are measured against 0.6.11 and recorded in slice 8.
- Fleet documents write "sctl TRD-9", since the fleet has a TRD-9 of its own. Fleet TRD-10 section 4 drops "(0.6.12)" from its sctl TRD-7 line: TRD-7 stays the option-B TRD, and this release carries no power fields.
- The scrub is its own commit, before the release: comments, test strings and documents, no behaviour, as `ece8b45` was. It replaces what the public tree still names with a role ("the production relay", "a customer's coach", "the field unit", "one XE300", "the site"). In the accepted ADR-002, ADR-003 and ADR-005 it changes wording, never a decision, as `ece8b45` did. The Rust tests it touches run under `cargo test`; the two WE826 shell tests change only in comments. Lines are at `ece8b45`:
  - the production relay's host name: `docs/adr/002-flash-constrained-units-boot-from-the-relays-mirror.md:12` (with a directory on that host), `docs/adr/003-the-relay-upgrades-itself-through-the-managed-path-last.md:7`, `docs/trd/TRD-2-ramboot-on-loader-run-units-and-cache-staging.md:24,33`, `docs/trd/TRD-3-managed-relay-upgrade.md:21,27`, `docs/trd/TRD-5-mwan3-verdict-in-net-state.md:25`;
  - paths on the relay's host: `docs/trd/TRD-1-release-integrity-guard.md:18`, `devices/rut241/README.md:54-56`;
  - unit serial fragments: `docs/adr/004-relay-route-ownership-on-units-with-a-wireguard-pin.md:13`, `docs/trd/TRD-1-release-integrity-guard.md:18`, `docs/trd/TRD-5-mwan3-verdict-in-net-state.md:24`, `devices/rut241/README.md:57`;
  - a site's venue type: `docs/adr/004-relay-route-ownership-on-units-with-a-wireguard-pin.md:7`, `docs/adr/005-the-agent-reports-the-units-own-uplink-verdict.md:7`, `docs/trd/TRD-5-mwan3-verdict-in-net-state.md:24`;
  - a customer vehicle's name: `CHANGELOG.md:14,132` (CRLF, byte-safe; line 132 also loses its em dash), `docs/trd/TRD-2-ramboot-on-loader-run-units-and-cache-staging.md:25,33`, `docs/trd/TRD-8-we826-guarded-move-to-the-current-ramboot-layout.md:3,9,13,15-18,60,62,63,71`, `devices/we826-qwd/README.md:103,112`, the comments at `devices/we826-qwd/tests/ramboot-refresh-test.sh:71`, `devices/we826-qwd/tests/refresh-fixtures.sh:12,382`, `server/src/infra/mod.rs:561` and `server/src/infra/profiles/peplink.rs:437,507,512,698,999`, and the test serials of `server/src/tunnel/history.rs:424-426,446,449,453,457,507,508,510,516,522,526`, replaced by strings of the same length so that rustfmt's layout stays as it is.

## 4. Decisions inside the release

- **The gate matches the path as given before it takes a handle, then the real path.** A rule on the path as given costs no open, and the real path catches a link, the overlay's upper directory and a process's root. `/proc/self/fd/0` or `/proc/kmsg` is answered by the first rule that sees it, never by an open that waits or fails.
- **`infra-monitor.json` is refused by name too.** It holds the SNMP communities of a pushed config, and a copy of `state_dir` can sit outside the protected directories. A hard link to a key is not compared by inode: no file route makes a link, exec reads the key anyway, and every save renames a new inode in (ADR-011 section 5).
- **The SNMP requests are the agent's own.** `snmp2` 0.5.2 binds its own socket and keeps its encoders to itself, so a bound send needs either a fork, kept up by this repository, or the agent's encoder. The three requests are a short piece of BER, pinned byte for byte against what `snmp2` sends, and `snmp2` still decodes.
- **The ping fallback stays small.** 254 shells at once on a one-core unit is the load the monitor's semaphore avoids elsewhere: 32 at a time, never more addresses than today's 254.
- **A round counts ticks, not checks.** Check intervals are per target, 60 s by default, so a count of checks would stand still on a healthy unit between two polls; a tick comes every 5 s. A target overdue for its check does not stop the count: a site whose targets all time out at 90 s each would read as stopped exactly when its targets are down. A check held past its deadline does stop it.
- **The bench stalls the loop through the environment.** No XE300 is on the bench (owner, 2026-10-07), and stopping the agent resets every counter. Proving "monitoring stopped" needs a loop that stops while the agent answers. `SCTL_INFRA_TEST_STALL_SECS` is read at monitor start and logged, as `SCTL_MWAN3_STATE_DIR` is (TRD-5).
- **A held config is compared whole.** The agent cannot recompute the fleet's fingerprint, and the version covers a credential's reference, not the bytes sent. Comparing the whole config costs nothing, so a config whose bytes differ is applied whatever its version.
- **Safe mode keeps the monitor off.** Safe mode keeps optional subsystems down after a crash loop (`docs/safe-mode.md`), and a push that started the monitor would undo it. The config is still stored, so a restart out of safe mode monitors at once.
- **The stable name is `/usr/bin/sctl`, made by the agent.** The BPIs already run from there, it is on every shell's `PATH`, and an install script never runs on a managed upgrade. The name costs one inode, or about 150 bytes for the WE826's wrapper, written only when it differs. It is a name, not an install path: running the XE300's agent from flash (its overlay has about 100 MB) would change a layout, which takes an ADR of its own, and no flash install is in this release. With the fleet's `{{agent}}` reference (fleet ADR-0040), which resolves to `cli`, a stored command carries no layout's path, so the name is what a command needs.
- **Attach takes a flag, and the replay ends where the running subscriber is.** Only the client that attaches can see an entry twice; a flag lets an older client keep today's behaviour, so the agent and its clients ship in any order. A batch carries many entries under one seq, so only a replay that ends at the subscriber's cursor keeps every batch on one side of it.
- **Not in 0.6.12.** sctl ADR-008 (a router's WAN digest, fleet TRD-28 appendix A) and ADR-009 (an uplink's throughput, fleet TRD-29) ship later with their own TRDs. HTTP credentials from the environment are a later decision (ADR-012 section 7). The file routes' writes keep today's rules, `/dev` and `/proc` included, and the agent refuses no firmware key file (the WireGuard key, the SSH keys, the password hashes): ADR-011 decision 8 stands, and the fleet refuses them. The Infra SNMP check still takes its community from the pushed config. There is no UDP 161 probe in discovery (fleet ADR-0039) and no sctlin pass-through of `{{agent}}` in a runbook (fleet ADR-0040); each waits for a later release. There is no flash install for the XE300 (the stable name, above).

## 5. The rollout

- **What it costs.** Every unit fetches the release once, and what it fetches depends on its target. The figures are 0.6.11-era; slice 8 records 0.6.12's for every target.
  - `mips_24kc` (the XE300, the WE826): the server and the plugin, gzipped, 3,923,078 bytes, about the 3.8 MB ADR-002 counted. A WE826 also fetches musl `libc` and `libgcc` (1,257,347 bytes gzipped) unless their SHA-256 is unchanged, since staging keeps a file whose hash is the manifest's (`docs/upgrade.md:90-97`).
  - `mipsel_24kc` (the RUT241): the server and the plugin, gzipped; slice 8 measures them.
  - `riscv64` (the BPI-RV2): the server and the plugin, raw: 7,363,856 bytes for a set built on 2026-09-30.
  - `armv7` (the BPI-R2): the server and the plugin, raw; slice 8 measures them.
  - `x86_64` (the relay): raw; slice 8 measures it.
  - A ramboot unit (the WE826, the RUT241) fetches its set again at every boot from its relay's mirror, over whatever path is up, LTE included and outside the rollout's rules; 0.6.12 changes that cost by its size difference.
- **Prerequisites, before the first ring.**
  1. The fleet's file deny-list (fleet TRD-35 slice 1) is live in production. An agent that reads `/proc` to its end would otherwise hand over `/proc/<pid>/environ`, where a recovery's credentials sit. 0.6.12 refuses that itself, but the fleet's list is the gate for every agent version and words the refusal. It shipped on 2026-10-21.
  2. netage-server reads the `infra` group, with the bounded pull for older agents (fleet ADR-0041, TRD-38), so the first 0.6.12 unit, quieter than before, reads as steady and every other unit is still read.
  3. The stored commands are ready (ADR-012 sections 4 and 7). A read-only list gives every stored command, recovery (in the fleet's records and in each unit's own Infra config) and runbook kept on a unit that calls `sctl net` or the agent's path, each with its destination judged against its unit's LAN-side networks. A second list gives every SNMP or Peplink target its unit does not reach on the link. Every `-c` moves to the environment form, and the change reaches the units: fleet ADR-0040's rewrite carries the commands the fleet stores, and each unit's Infra config is pushed again; a runbook kept on a unit is written again by a person's save through the fleet, since that rewrite writes none, or its unit is held. A destination the unit does not reach on the link is changed, or its unit held, before that unit takes 0.6.12: no flag turns the guard off.
  4. The fleet sends `--dev` and `--expect-mac` only to 0.6.12 or later, and stops its neighbour read after a scan for those units (3.4, 3.5).
  5. The rollout engine fails closed on the path. Today it reads only a location's open uplink intervals (`agent-updates/server/rollout-engine.server.ts` (fleet)): a unit on a vehicle or placed nowhere has no path fact and is never busy for LTE, a path that reads unknown is not busy, and a failed read of the facts leaves every unit not busy. Before 0.6.12 rolls out, the engine also reads the unit's own elected route: a ring device whose path is not known to be wired is busy, and a failed read makes every device busy. Until that ships, every unit on a vehicle or placed nowhere is held for 0.6.12, at vehicle or device scope.
- **Under the fleet's agent-update rules, never on LTE.** Rings, windows and holds are each tenant's. The engine counts a ring device busy while its path is LTE, backup or unknown, its wired uplink is down, it is on a departure or a console is open (ADR-002 section 4). A unit that is only ever on cellular gets the release only when a person releases its hold.
- **Steps.**
  1. Confirm the prerequisites in production, read-only: the deny-list refuses `/proc/self/environ`; netage-server reads the group from a bench agent; the list of stored commands is clean; the engine fails closed, or the holds are in place.
  2. Tag `v0.6.12`. The `Release` workflow signs, proves and publishes the bundle.
  3. Put the bundle and its plain-HTTP mirror on every relay (`rundev.sh relay artifacts`), keeping the previous bundle while a ramboot unit may roll back to it (ADR-002 decision 2; `docs/releasing.md:251-266`).
  4. A bench agent first. No XE300 is on the bench (owner, 2026-10-07), so proofs run in network namespaces, against an x86 bench relay, or on a field unit after hours with the owner's go.
  5. The fleet's rings, under its rules.
  6. The relay itself last, through the managed path (ADR-003), once every ring of the version is done on it.
- **No relay change.** The relay passes `infra.state` through with only its 16 KiB check, keeps a device's feature strings within its limits, and forwards REST and session frames as they come (`server/src/tunnel/relay.rs:299-311,1438-1494,2128-2194`). A new device on an old relay holds (`docs/releasing.md:213-218`). Nothing is changed on the relay by hand before it takes 0.6.12: no systemd stopgap guards its files, since 0.6.12's gate makes one needless (ADR-011 section 7).
- **Mixed versions.** TRD-5 slice 4 and TRD-6 slice 3 are still open, so a unit may run an agent older than 0.6.11. The fleet gates each feature by version and reads a missing `infra` group as an older agent, never as a stall. A unit that its own helper rolls back to 0.6.11 runs every stored command and gets the bounded pull.

## 6. Verification

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` on the pinned 1.97.1, as CI runs them; the ts-rs bindings diff and `cargo doc`; sctlin's `npm run check`, `npm run test` and `npm run package`. The HTTP API and config docs gates pass unchanged, since no route and no `pub` config field is added; the new fields, codes and variables are reviewed by hand in `docs/http-api.md`, `docs/errors.md` and `docs/config.md`.
- As root, each built with `--no-run` and run with `sudo -n env SCTL_NETNS_TEST=1 <test binary> --ignored`: `net_tools_netns` (new), whose run is recorded here since CI has no privileged job, and `relay_route_netns`, unchanged and still green.
- The BusyBox wrapper test (3.6) and `upgrade-remote-test.sh`.
- Payloads built with `devices/build.sh`; every target measured against 0.6.11 and recorded in slice 8.
- The release: `proof ok` for the six server artifacts in the `Assemble` step, and the bundle verifies.
- A bench agent (x86) behind a bench relay, with a bench netage-server whose `[wg]` section is off (a local netage-server with a live `[wg]` reconcile removes the production relay's peers):
  - through the relay, `/proc/net/arp` reads its table; a read of the agent's config is `PROTECTED_PATH` with one activity entry; 40 FIFO reads leave relayed REST answering;
  - `GET /api/info?groups=infra` shows `rounds` moving; with `SCTL_INFRA_TEST_STALL_SECS=60` the count stops at 60 s, and netage-server raises one "monitoring stopped" within a minute (fleet TRD-38's acceptance); the group's bytes and the whole `/api/info` answer's are recorded;
  - an identical config push answers `unchanged`, and no `infra.state` follows;
  - the name made through `SCTL_CLI_PATH` runs `net tcp 127.0.0.1 <port>` with exit 0, and health reports `cli`;
  - through the real tunnel client, two watchers keep every line while a third attaches with `dedupes`.
- A WE826 on the bench, once it is back: the gate's calls on Linux 3.3.8 and the reopen through `/proc/self/fd/<n>`; the wrapper; a `sctl net` child reading `[lte] interface` through `SCTL_AGENT_CONFIG`; whether the neighbour read after the port probe fills the sweep's hosts.
- Before the rollout, on a bench unit or, read-only and with the owner's go, on a field unit: whether `/var` is a link to `/tmp` on the BPIs (the credential store, asked by both paths, is refused by both); whether `/proc/kcore` exists on each unit kernel and on the relay host (a listing, never a read); which units ship `nmap`, `arp-scan` and `tcpdump`.
- After the rollout, on one unit of each model the fleet runs, with the owner's go: health reports 0.6.12 and `cli`; `/proc/net/arp` reads its table; the agent's config is refused; the proof moves; a stored power action's read-only test, sent with `--expect-mac`, passes; on an XE300 with mwan3, a refused request names `wwan0` as `lte`, and a `--dev` request on the wired WAN port passes; the fleet reads a steady site as steady for 24 h with no grey rows (fleet TRD-38).

## 7. Slices and status

1. File reads (3.1): Implemented (2026-10-21, `0c0d520`; 36 new tests, 401 in the library, the workspace, clippy and fmt green on 1.97.1 as CI runs them). Its acceptance on hardware (through a relay, 40 FIFO reads with relayed REST answering, each install layout's directories, the WE826's kernel) runs with section 6.
2. The proof (3.2): Planned.
3. The monitor (3.3): Planned.
4. Net tools and `net_tools_netns` (3.4): Planned.
5. Discovery (3.5): Planned.
6. One name for the tools (3.6): Planned.
7. Attach, sctlin and the MCP (3.7): Planned.
8. Release 0.6.12 (3.8): the scrub first, in a commit of its own, then the release, every target measured: Planned.
9. The rollout (section 5), its prerequisites confirmed first: Planned.
