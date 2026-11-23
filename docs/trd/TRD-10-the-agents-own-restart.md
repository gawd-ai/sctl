# TRD-10: The agent's own restart

Implements [ADR-014](../adr/014-the-agent-restarts-itself-through-one-authenticated-route.md), as ruled in its section 7. Status: Planned. It rides sctl's next release, the first after 0.6.13, and never a release of its own; section 5 lists what that release also carries, in a TRD of its own. (TRD-7 is held for ADR-007's option B, and TRD-8 and TRD-9 are taken, so this is TRD-10.)

## 1. Objective

After the release that carries this:

- a caller restarts a unit's agent with `POST /api/restart`, on every layout, sending no shell text and knowing nothing of the layout;
- the agent refuses, before anything stops, a restart that would strand it or cut something that must not be cut: an upgrade or another restart in flight, a recovery run, the kill switch, a start its init would refuse, a `ramboot` start that would fetch;
- the answer leaves before the agent stops and says what the restart ends;
- the next start is tied to the request: a record names the request, the process before and the process after;
- a caller knows the route is there by one key in an answer the agent already gives, so an older agent is never asked;
- a `gz-tmp` init no longer stops the agent before its own checks have passed;
- the unit never reboots for it, its modem and its routes are not touched, and what a reconnect costs in bytes is a measured number.

No relay change is needed.

## 2. What 0.6.13 has

ADR-014's section 2 holds what the decision found. This section holds what the build needs besides. Lines are at `a222339`.

- **Main.** `main` is 3 commits past `v0.6.13`, at `a222339`: a payload build fix (`5d3c4fb`) and two commits of TRD-9's records. `server/src` is as 0.6.13 released it.
- **The upgrade's two halves.** `upgrade/stage.rs` is the running agent's half and ends in `hand_off` and `spawn_helper` (`:778-788`, `:822-896`); `upgrade/apply.rs` is the helper's, plain synchronous code with its runner and its health check behind two traits (`Runner`, `:90-106`; `Health`, `:26-48`), which its tests replace with a scripted service (`:464`). `spawn_helper` builds the `upgrade-apply --stage` arguments and the unit name `sctl-upgrade-<version>` inside itself.
- **The state.** `upgrade/state.rs` holds the one upgrade state, its file under `<state_dir>/upgrade/` (`:211`) and its handle with the in-flight mutex (`:155-160`). `tunnel/upgrade_state.rs` flattens that state into the pushed message (`:29-40`) and trims the log tail when the message passes 8 KiB (`:87-104`). `upgrade/watch.rs` watches the state file by name in that directory (`:78`).
- **Recovery runs.** `Runs` is a watch over the set of target ids with a run in progress (`infra/mod.rs:530-575`); `start` is one `send_if_modified`, so finding the set empty and closing it can be one step of the same kind. A run's cooldown and count move only after `start` answers (`infra/monitor.rs:1209-1216`). The upgrade's context takes the runs of the agent's own Infra state (`upgrade/routes.rs:69`), and a test holds that it is that set and no other (`:165-225`).
- **Sessions and transfers.** `SessionManager::list_sessions` gives each session's kind, status, name and whether a client is attached (`sessions/mod.rs:58-75`, `:727`), `kill_all` ends them (`:484`), and the transfer manager lists the transfers it holds with their phase (`gawdxfer/manager.rs:1007`).
- **The install records.** Three scripts write `install.json` for a `gz-tmp` unit, each as one `printf`: `devices/xe300/install.sh:235`, `devices/xe300/upgrade.sh:129` and `devices/rut241/install.sh:96`.
- **The shared ramboot init is pinned by its bytes.** `devices/we826-qwd/ramboot-refresh.sh` keeps a table of the SHA-256 of every released shared init and wrapper (`:178-183`), and its test holds that the current files are in it (`devices/we826-qwd/tests/ramboot-refresh-test.sh:383`). That suite runs the init under a stand-in `rc.common` in a scratch tree.
- **Tests.** The library's tests run with no root. `server/tests/cli_name.rs` starts a real agent on a staged layout through `SCTL_INSTALL_JSON` and `SCTL_CLI_PATH`, and runs shell text under the system `sh` and BusyBox's only on files in its own scratch directory, every path an argument. `server/tests/relay_events.rs` runs fake devices against the real relay router. Root-only network tests follow `server/tests/relay_route_netns.rs`.
- **CI and the gates.** As TRD-9 section 2 lists them. This build adds one route, so the HTTP API docs gate (`scripts/check-http-api-docs.py`) needs a heading for it; it adds no `pub` config field.
- **Line ends.** `CHANGELOG.md` is CRLF. Every edit to it is byte-safe and keeps each line's ending. Every other file this TRD edits is LF.

## 3. Deliverables

Each change says what changes, in which files, how it is tested and when it is accepted. Section 6 runs the checks that cover them all.

### 3.1 The record, and `instance` where the helper reads

**Change.**

- `GET /api/health` gains `instance`: an opaque value made once when the process starts, 32 hex characters. Its draw never blocks: std's hash keys, which the kernel gives without a wait (as `net_tools/snmp.rs` draws its request ids), mixed with the pid, the monotonic clock and the wall clock, and those three alone where the keys cannot be had. No generator that can wait for the kernel's pool is on the start path. The app's state holds it beside `boot`, the start time `net.state` and the Infra digest already take (`main.rs:742-747`).
- The restart's record: `v`, `phase` (`idle`, `accepted`, `restarting`, `done`), `request_id`, `boot_before`, `boot_after`, `instance_before`, `instance_after`, `outcome` (`ok`, `not_restarted`, `not_healthy`, `not_started`, `interrupted`), `detail`, `attempts`, `accepted_at`, `ended_at`, `log_tail` (the last 1 KiB of the helper's log, cut on a character boundary). A key with nothing to say is left out, so an idle record is `{"v": 1, "phase": "idle"}`. It lives at `<state_dir>/upgrade/restart.json`, written through a temporary file and a rename, and is read at start.
- `GET /api/upgrade` gains `restart`, the record. The pushed `upgrade.state` gains the same key; the forwarder sends when either the upgrade's state or the record changes, still at most once a second, and drops the record's log tail before the upgrade's when the message passes 8 KiB.
- At start: a record left `accepted` (the agent stopped before the hand-off) ends as `not_started`; `restarting` with the helper's lock held is waited on, with the bound of 3.5 step 9; `restarting` with the lock free and no outcome, read by a process whose `instance` is not `instance_before`, ends as `interrupted`: the agent did start again, by what is not known (a crash between the record's write and the helper's start, or a power cut, leaves exactly this), and `ok` is written only by the helper; `restarting` with the lock free and the same `instance` ends as `not_restarted`, helper lost. A record the helper ended as `ok` with this process's `instance` gains this process's `boot` as `boot_after`.
- An outcome whose `attempts` is 2 is logged at warn by the agent that reads it.

**Files.** `server/src/upgrade/restart.rs` (new: the record, its file, its handle, the settle at start), `server/src/upgrade/mod.rs`, `server/src/upgrade/routes.rs` (`report`), `server/src/upgrade/watch.rs` (the watch takes the file's name), `server/src/tunnel/upgrade_state.rs`, `server/src/tunnel/client.rs` (the forwarder's second channel), `server/src/routes/health.rs`, `server/src/state.rs`, `server/src/main.rs`.

**Tests.**

- The record round-trips, an idle record is the two keys, an unknown key in a stored record is ignored, and a file that is not a record reads as idle.
- Each start above settles as written, and the stamp of `boot_after` is written once.
- `report` carries `restart` on an agent never asked; health carries `instance`, the app state's.
- The pushed message carries `restart`; a change of the record alone sends one message; a message over 8 KiB loses the record's log tail first.
- `server/tests/relay_events.rs`: the relay keeps an `upgrade.state` that carries `restart`, publishes it and replays it whole.

**Accepted when.** An agent never asked shows `restart` idle in `GET /api/upgrade`, and two reads of its health show one `instance`.

### 3.2 What the agent knows of its own start

**Change.**

- `install.rs` names the kill switch (`/etc/sctl/disabled`) beside the hold file, and reads the optional `install.json` key `restart_tmp_kb`. Where a record gives none: 16384 on `gz-tmp`, 0 elsewhere. `sctl install-info` and `GET /api/upgrade`'s `install` report it. `SCTL_DISABLED_PATH` moves the kill switch for tests and benches, as `SCTL_INSTALL_JSON` moves `install.json`.
- One function judges whether a start would be refused, from the install and the unit's files, with no side effect, and answers the first reason in this order: `no_install_json`; `disabled`; `loader_unknown` (a `ramboot` unit whose `ramboot.conf` names a loader, or cannot be read, and whose `install.json` has no `helper_prefix`, as `cli_name` already judges it); `start_unreadable` (the agent's config; on `gz-tmp` each file `install.json`'s `files` names; on `ramboot` the wrapper, executable, and `ramboot.conf`); `cache_invalid` (`ramboot`: the boot fetcher's own rule, `devices/common/sctl-ramboot.sh:120-131`: the server's payload and each other payload whose URL `ramboot.conf` names must be in the cache with its SHA-256, or, named with none, be there under `ALLOW_UNSIGNED=1`); `no_space` (a layout that runs from `/tmp`: free space in the run directory's filesystem below `restart_tmp_kb`, or `min_free_kb` where that is larger, plus what the helper pins: the running binary and its loader. The helper is the old binary running from `/tmp`, so its room stays taken while the init writes the new copy, on the first run and on the second).
- A path is judged by its metadata before anything is opened: one that is not a regular file is never opened, so a FIFO placed there cannot hold the request (section 5, rider 1).

**Files.** `server/src/upgrade/install.rs`, `server/src/upgrade/restart_checks.rs` (new), `server/src/main.rs` (`install-info`), `docs/upgrade.md` (the key, and the kill switch as something the agent reads).

**Tests.** In a scratch root, per layout: each reason above, alone and in order; a valid `ramboot` cache passes; a cached payload with one byte changed is `cache_invalid`; an unsigned payload passes only under `ALLOW_UNSIGNED=1`; a FIFO at a payload's path is refused at once; free space is injected.

**Accepted when.** On a unit staged as each layout, the function's answer matches what that layout's init would do with the same files, for every row of ADR-014's table.

### 3.3 What a restart ends, and the gate on recovery runs

**Change.**

- `Runs` gains a gate: `close_if_idle` closes the set in the step that finds it empty and answers whether it is closed; while it is closed, `start` answers none; `reopen` opens it. A monitor check that finds the gate closed records and signals its Down as today and starts no run, and neither the cooldown nor the count moves: the run is the next process's.
- `ends` is built from what the agent holds: the running terminals, those with a client attached, the running jobs' names (at most 16, each cut to 64 bytes on a character boundary; a job with no name is named by its session id), the transfers in flight (`init`, `transferring`, `paused`, `verifying`).

**Files.** `server/src/infra/mod.rs`, `server/src/infra/monitor.rs` (its test), `server/src/upgrade/restart.rs`.

**Tests.**

- A run in progress keeps the gate open and `close_if_idle` answers false; with none it closes, `start` then answers none and a recovery that is due starts no run and moves no count; `reopen` lets the next failed check start it.
- Two threads, one starting runs and one closing: never a run started after the close answered true.
- `ends` for no session, a detached terminal, an attached one, an exited one (not counted), a job, 20 jobs (16 names), a name over 64 bytes.

**Accepted when.** With a recovery of about 30 s on a bench agent, a restart asked during the run is refused, and one accepted just before a target goes Down starts no run before the stop; the next process's first failed check starts it.

### 3.4 The helper

**Change.**

- `sctl restart-apply --dir <state_dir>/upgrade/restart`: the helper. The directory holds its plan (`plan.json`: the restart command, the health URL, the layout, the record's path and `instance_before`), its lock and its log. It takes the lock, waits the upgrade's 2 s, runs the restart command through the upgrade's `Shell`, and reads health through the upgrade's reader, with the upgrade's interval, passes and window. An answer counts only when its `instance` is not the plan's. It writes `ok` with `instance_after`, `not_restarted` when the last answer inside the window still carried the old `instance`, or `not_healthy`, then exits. It restores nothing.
- **The window's judgment.** A window that ends with no new process proven is judged from its reads. Nothing answers when every read of its last 30 s failed, at least three of them. Otherwise the last read that was answered decides: answered by the old process, it is `not_restarted`; by a new one that never answered twice in a row, or with too few reads to say, `not_healthy` with a detail that says which. One failed read at the end of the window does not turn a process that still answers into silence.
- **The second run.** On `gz-tmp` only, when nothing answers at the end of the first window and no new process answered in it, the helper logs that, runs the restart command once more and waits one more window. `attempts` is 2 from then on, and the outcome's `detail` says that the first run started nothing.
- Its wait is its own loop: it has to remember whether the old process still answers, which the upgrade's wait does not. The upgrade's `wait_healthy` and its `Health` are unchanged.
- **Every step is bounded**, because the helper holds the lock that keeps every later restart and upgrade out. The restart command runs under a deadline of 60 s, and a command still running at it is killed: the shell the helper spawned, nothing else; the helper then reads health all the same, since the restart may have happened before the command hung. Each health read ends by a total deadline of 5 s, connect to last byte. And the helper as a whole ends by a hard limit derived from those steps (2 s; for each run of the command, 60 s, the window and one read; 30 s more: 277 s, 522 s on `gz-tmp`): a watchdog writes `not_healthy`, with a detail that names the hard limit, and exits 3. The run and the watchdog end the record through one guarded write, so an outcome is never written over. The agent that waits on the helper gives it its hard limit and 30 s (3.5 step 9).
- `spawn_helper` takes the helper's arguments and its unit name from its caller. The upgrade passes what it passed before; the restart passes `restart-apply --dir <dir>` and `sctl-restart-<accepted_at in unix seconds>`. The helper is `InstallInfo::running`'s binary through its loader words, not a copy.

**Files.** `server/src/upgrade/restart_apply.rs` (new), `server/src/upgrade/apply.rs` (the reader, the log and the settle time shared), `server/src/upgrade/stage.rs`, `server/src/main.rs` (the subcommand, beside `upgrade-apply`).

**Tests.** With a scripted runner and health, as `apply.rs`'s tests are built:

- a new `instance` twice is `ok` with that `instance` and `attempts` 1; one new answer followed by silence is `not_healthy`;
- the old `instance` to the end of the window is `not_restarted`, on every layout, with one run of the command;
- silence on `usr-bin`, `ramboot` and `systemd` is `not_healthy` after one run; on `gz-tmp` the command runs a second time, and an answer then is `ok` with `attempts` 2 and the detail, silence again `not_healthy` with `attempts` 2;
- a held lock ends a second helper at once, exit 4, the record untouched;
- the command line `spawn_helper` builds: for an upgrade the words it built before the change, pinned literally, on each layout and through a loader; for a restart the loader words, the binary and the two arguments, on `systemd` inside `systemd-run --collect --unit=sctl-restart-<n>`.

**Accepted when.** A helper started by hand against a plan runs a stand-in restart command and writes the outcome its health reads.

### 3.5 The route

**Change.** `POST /api/restart`, behind the key, in this order:

1. The body: `request_id` at most 128 bytes, `unless` a list of `sessions`, `jobs`, `transfers`; otherwise 400 `RESTART_BAD_REQUEST`. An empty body is a request with neither.
2. The in-flight mutex the upgrade takes, waited for 250 ms at most (a hand-off that has just ended holds it for an instant after its record reads done); an upgrade's state in flight is 409 `UPGRADE_IN_FLIGHT`, a record `accepted` or `restarting` is 409 `RESTART_IN_FLIGHT` with the record. A `restarting` record whose lock is free is settled first, as the upgrade clears a lost helper. `POST /api/upgrade` refuses in the same way while a restart is in flight.
   - **For a caller that schedules upgrades:** an upgrade asked while a restart is in flight is answered 409 `RESTART_IN_FLIGHT`. That is "busy", not a failure of the upgrade: the caller asks again once the restart's record has ended, as it would after `UPGRADE_IN_FLIGHT`.
   - **Across processes the exclusion is read from disk.** `sctl upgrade` from a shell is another process than the agent, and neither sees the other's memory. The route refuses (409 `UPGRADE_IN_FLIGHT`, the state as it is on disk) while the upgrade's state file says an upgrade is in flight and its helper holds the lock of its stage; `sctl upgrade` refuses, before it stages anything, while the restart's record says a restart is in flight and its helper holds its lock. A state or a record left on disk with no lock held refuses nothing. One window stays: `sctl upgrade` holds no lock while it stages, so a restart accepted during that staging and the upgrade's hand-off after it are not kept apart; a person at the unit's shell runs one at a time.
3. A relay is refused (`relay`).
4. The start's judgment (3.2).
5. `unless`, against `ends`.
6. The gate: `close_if_idle`, or `recovery_running`.
7. The record `accepted`, written and pushed; the answer, 202, with `window_secs`, `second_run` (true on `gz-tmp`, where the helper may wait a second window), `safe_mode` and `ends`.
8. In a task, under the mutex: every session and job ended (`kill_all`); the plan written; the record `restarting`; the helper started. A helper that cannot be started ends the record as `not_started`, reopens the gate, and the agent keeps running. The mutex is released once the helper is started, as the upgrade releases it after its staging: from there the record refuses the next request.
9. The wait on the helper, bounded. The agent looks at the record every second. A record the helper ends while this process still runs (`not_restarted`, `not_healthy`) reopens the gate. So does a helper that left no outcome: found with its lock free 15 s or more after its start, or still silent once its own time has passed. It is settled `not_restarted` with a detail that begins `helper lost`, and requests are taken again. ("Its own time" is the helper's hard limit of 3.4, and the agent waits 30 s past it.) A helper that died after its start (out of memory, an exec that failed inside its transient unit, a record it could not write) therefore never leaves the old process refusing restarts and upgrades, or running no recovery. The process a restart started waits on the helper the same way.

**Files.** `server/src/upgrade/restart_route.rs` (new), `server/src/upgrade/routes.rs` (`POST /api/upgrade`'s refusal), `server/src/main.rs` (the route), `docs/http-api.md` (the route under "Managed upgrades", `instance` in `GET /api/health`, `restart` in `GET /api/upgrade` and in `upgrade.state`), `docs/upgrade.md` (a section "Restarting the agent" beside the hand-off, and the per-layout table), `docs/safe-mode.md` (lines 117 to 119 name the route), `docs/errors.md` (the codes and the reasons), `CHANGELOG.md` `[Unreleased]`.

**Tests.**

- Each refusal: its status, its code, its reason, and nothing stopped: the gate open, the sessions alive, the record as it was.
- An upgrade staging refuses a restart; a restart accepted refuses an upgrade, and a second restart with the first's record in the detail.
- `unless` with each kind refuses only when that kind is running, and `detail.ends` names it; without `unless` the same unit is accepted.
- An accept writes the record before the answer and closes the gate; the task ends the sessions, then starts the helper, in that order (a helper stood in by a recorder).
- A helper that cannot be started: `not_started`, the gate open again.
- Both docs gates pass.

**Accepted when.** Each refusal reads as its code and reason, and an accepted request's 202 is answered before the helper is started.

### 3.6 The device tree: a `gz-tmp` init checks before it stops, and each install record names its threshold

**Change.**

- `devices/xe300/sctl.init` and `devices/rut241/sctl.init`: the checks a start makes (the kill switch on the XE300, the config, the two payloads, free `/tmp`) move into one function, `start_checks`. `start_service` calls it, as before. A `restart` function, which `rc.common` leaves to the script, calls it first and stops the running agent only when it passes; otherwise it says why on stderr and exits 1, the agent left running.
- The install scripts write the threshold their init demands: `"restart_tmp_kb":16384` in the XE300's two `printf` lines, `"restart_tmp_kb":8192` in the RUT241's.
- Not changed, and why:
  - `devices/common/sctl-ramboot.init`. Its checks (the kill switch, the wrapper, `ramboot.conf`) fail only when a person removed a file, and with a valid cache the wrapper asks for no room, so it has no check that can fail between a stop and a start by itself. Its bytes are pinned by the WE826 refresh's table, whose suite runs the init; changing it belongs with that suite, on a bench.
  - `server/files/sctl.init` (`usr-bin`): it has no check before its start.
  - No init gains a kill switch (ADR-014 section 7).
- An init reaches a unit when its install script runs, never by a managed upgrade. A unit already installed keeps the init and the install record it has.

**Files.** `devices/xe300/sctl.init`, `devices/rut241/sctl.init`, `devices/xe300/install.sh`, `devices/xe300/upgrade.sh`, `devices/rut241/install.sh`, `devices/xe300/README.md` and `devices/rut241/README.md` where they describe the init.

**Tests.** `server/tests/restart_inits.rs` (new). It reads the scripts as text and parses them with `sh -n`; it never runs one, since they name fixed paths and remove a run directory.

- In each `gz-tmp` init: `restart()` exists, and inside it `start_checks` comes before `stop`; `start_service` calls `start_checks` before it removes the run directory.
- Each `gz-tmp` init's `MIN_TMP_KB` is the `restart_tmp_kb` its device's install script writes, in every `printf` that writes `install.json` there.
- Every `DISABLED=` in an init under `devices/` is the path the agent checks.
- `sh -n` passes on each init and each install script it read.

**Accepted when.** The test is green, and on a bench unit of each `gz-tmp` model `/etc/init.d/sctl restart` with a failing check (a full `/tmp`, stood in on the bench) leaves the agent running (3.8).

### 3.7 A real agent and a real helper on loopback

**Change.** No product code: the harness that proves the path with real processes and no relay.

- `server/tests/restart.rs` (new), in the pattern of `cli_name.rs`: an agent started by the test on a loopback port, with no `[tunnel]` section, staged through `SCTL_INSTALL_JSON`, `SCTL_CLI_PATH` and `SCTL_DISABLED_PATH`, its `state_dir` and `data_dir` in the test's scratch directory.
- Its `install.json` names a restart command that only writes a marker file in the scratch directory, and a `health_url` on a loopback port where the test itself answers: the agent's own `instance` until the marker exists, another after.
- The test stops only the agent it started, at its end. It runs no init script, no `systemctl`, and signals no process it did not start.

**Tests.**

- The request is answered 202 with `instance_before` equal to health's, before the marker exists; a second request during the first is 409 `RESTART_IN_FLIGHT`; the marker appears; `GET /api/upgrade` then reads `restart` `done`, `ok`, `attempts` 1, `instance_after` the stand-in's value, no `boot_after`, and the helper's log tail; the agent the test started still answers with its own `instance`.
- With the kill switch's stand-in present: 422 `disabled`, no marker and no plan.
- With no `install.json`: 422 `no_install_json`; a bad body is 400; without the key, 401.
- Sessions are not opened here: that takes a WebSocket client. 3.5's test ends real sessions and a real job, the test's own children, before its stand-in helper is started.

**Accepted when.** `restart.rs` is green on the pinned toolchain with no root.

### 3.8 The bench: a relay, the bytes, and one restart on a unit of each layout

**Change.** None. This slice needs a relay and units, and is run by a person. It needs the release's payloads, so it runs between the release's build and its rollout, on bench units the owner names: one of each layout, `usr-bin`, `gz-tmp` (each model), `ramboot` under procd, `ramboot` with no procd and a loader, and `systemd` for an agent that is not a relay if one exists.

**Checks.**

- Behind a bench relay, with the event stream open: an accepted request's 202 arrives before the device's connection drops; `device.disconnected`, `device.connected`, a `net.state` with a new `boot` and an `upgrade.state` whose `restart` reads `ok` with that `boot` as `boot_after` arrive in that order; a relay's own agent refuses (`relay`).
- The bytes of one reconnect, counted on the link: in a network namespace as `server/tests/relay_route_netns.rs` is run, and on each unit's uplink interface before the request and after the record reads `ok`.
- Per unit: the unit's system uptime keeps counting (no reboot); its `net.state` shows the same uplink before and after, and no interface dropped; a console session open before is gone after; with the kill switch placed by hand the route refuses and the agent keeps running; on the `ramboot` units the wrapper's log shows every payload served from the cache, and with one cached payload removed the route refuses `cache_invalid`.
- A recovery of about 30 s: a restart asked during the run is refused; one accepted just before a target goes Down starts no run before the stop.
- Each `gz-tmp` model: free `/tmp` against the init's threshold and the install record's `restart_tmp_kb`, the two equal; a restart with `/tmp` filled below it is refused `no_space`; with the new init installed, `/etc/init.d/sctl restart` with `/tmp` filled below it leaves the agent running.
- **The shared ramboot init under procd.** On a `ramboot` unit that has procd, whether `/etc/init.d/sctl restart` stops the instance: the init defines a `stop` of its own for units with no procd, and if `rc.common` did not put procd's in its place, the restart would stop nothing and the route would report `not_restarted`.
- What the stop does to a `POST /api/exec` in flight and to a session whose process ignores SIGTERM, read once and recorded here.

**Accepted when.** Every layout has one recorded restart, the cost in bytes is written beside each, and each `gz-tmp` model's threshold is confirmed.

## 4. Decisions inside the build

- **The helper is the running binary, not a copy.** Nothing is replaced, so nothing the helper runs from is replaced. On a layout that runs from `/tmp` the init removes or overwrites the file the helper runs from; a running process keeps its image, and the room for it is what 3.2's `no_space` counts.
- **The record has its own file.** The upgrade's state file is written whole by two processes that expect its shape. One directory, two records, one lock each.
- **The record, the plan and the log are under `state_dir`.** `data_dir` is in `/tmp` on the units whose init removes `/tmp/sctl` at every start; the outcome has to be there after the start. The log is a few hundred bytes and starts afresh for each request.
- **`instance`, not `boot`, on the route that needs no key.** The ruling asks for an opaque value there. `boot` already means the start time in `net.state`, `infra.state` and the Infra digest, so the opaque value takes another name, and the record carries both: `instance` for the helper, `boot` for a caller that matches reports.
- **The gate is closed in the route, and the sessions are ended in the task.** The gate must be closed in the step that finds no run, before the answer. Ending the sessions takes up to 3 s and must not hold the answer.
- **The accepted order is fixed.** A refusal that costs nothing comes before one that reads files, and the gate is closed last, so a refusal leaves nothing to undo.
- **The version is not judged.** An upgrade proves its version; a restart proves a new process.
- **The helper's wait is its own.** Sharing the upgrade's loop would mean widening it for a rule it has no use for; the reader, the runner, the log, the lock and the three constants are shared.
- **Scripts are read as text and never run in a test.** They name fixed paths under `/tmp` and `/etc`, and a `gz-tmp` init removes its run directory. The one test that starts processes starts an agent with no tunnel and a helper whose restart command writes a marker.
- **Known weakness, left for the release's own TRD: the upgrade's helper is not bounded.** `Shell::run` waits for the restart command with no deadline (`upgrade/apply.rs`, `Runner for Shell`), and its health read bounded each read, not the whole exchange (TRD-11 bounds the read for 0.6.14 and leaves the command's deadline open until a bench has timed the command). A restart command that hangs leaves the upgrade's helper holding its lock, and every later upgrade and restart answering in flight until a reboot. The restart's helper bounds both for itself and leaves the upgrade's behaviour as it is.
- **Not in this build.** No MCP tool and no sctlin control for the restart; no `features` entry; no WebSocket frame; no restart of a relay; no change to the shared ramboot init or to the `usr-bin` init; no kill switch added to an init.

## 5. What the release that carries this must also carry

These ride the same release. They are not slices of this TRD: the release and its riders have a TRD of their own, [TRD-11](TRD-11-release-0-6-14.md) (ADR-014 section 7, ruling 10), which also takes this TRD's bench (3.8) as one of its slices.

1. **The FIFO reads outside the file gate.** TRD-9 slice 10's bench (2026-11-20) found the failure 0.6.13 fixed in the gate still open on routes the gate does not cover, each older than 0.6.12 and each needing exec or local access to set up, "owed by the next release":
   - a FIFO named `*.md` in `playbooks_dir` makes `GET /api/playbooks` and `GET /api/playbooks/{name}` open it plainly (`server/src/routes/playbooks.rs:182`, `:224`); 32 such reads take every one of the tunnel client's handler permits, and a SIGTERM then leaves the agent alive;
   - a FIFO at `<data_dir>/watchdog_history.jsonl` makes `GET /api/lte/watchdog/history` hold a runtime worker (`server/src/routes/lte.rs:210`, a blocking read inside an async handler), and four of them stop the whole agent;
   - `PUT /api/playbooks/{name}` onto a FIFO with no reader waits in its open;
   - a flood of about 300 refused reads evicts the activity ring's 200 entries, exec history included.

   It belongs with this TRD for a second reason: an agent that a SIGTERM leaves alive is an agent whose restart ends as `not_restarted`.
2. **sctlin passes `{{agent}}` through in a playbook's script.** TRD-9 section 4 kept it out of 0.6.12 ("no sctlin pass-through of `{{agent}}` in a runbook"), and 0.6.13's `web/` differs from 0.6.12's only in its version line. `renderPlaybookScript` (`web/src/lib/utils/playbook-parser.ts:174-200`, CRLF) refuses any `{{name}}` left after the declared parameters as an undeclared parameter. `{{agent}}` names the path that runs the agent's tools on the unit (TRD-9 3.6, `GET /api/health`'s `cli`) and is filled by the server that sends the job, so sctlin must leave it in the text; every other undeclared name stays refused. Its test is in `playbook-parser.test.ts`.
3. **The release itself**, as `docs/releasing.md` and TRD-9 3.9 describe one: the five version carriers and the lock files, `CHANGELOG.md` (CRLF), the tag equal to `server/Cargo.toml`'s version, TRD-1's proof in the `Release` workflow, every target's artifacts measured against 0.6.13, the units before the relay.

## 6. Verification

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` on the pinned toolchain, as CI runs them; the ts-rs bindings diff and `cargo doc`.
- `scripts/check-http-api-docs.py` (one new heading) and `scripts/check-config-docs.py` (unchanged).
- `server/tests/restart.rs` and `server/tests/restart_inits.rs` with no root; `server/tests/relay_events.rs` with its new case.
- Device payloads are built only through `rundev.sh` and `devices/build.sh`, never with cargo or cross by hand; no payload is built for slices 1 to 7.
- 3.8, on the bench, with the owner's go for each unit.
- Nothing here runs on a unit in service. A unit in service is restarted by this route only after the release's rollout reaches it, by whoever operates it.

## 7. Slices and status

Build order. Each slice is one commit. The TRD's status flips in the change that ships it; until then every slice reads Planned, and a slice that is built says so, with its commit and what it ran. Nothing here is released, no payload was built, and nothing was sent to a unit.

1. The record, and `instance` in health (3.1): Planned. Built (2026-11-22, `61265d3`): 12 new tests in the library, 1 in `relay_events.rs`.
2. What the agent knows of its own start (3.2): Planned. Built (2026-11-22, `13ba2c4`): 11 new tests in the library.
3. What a restart ends, and the gate on recovery runs (3.3): Planned. Built (2026-11-22, `5007648`): 5 new tests in the library. Reading what the stop does to a `POST /api/exec` in flight and to a session whose process ignores SIGTERM is left to the bench (3.8).
4. The helper (3.4): Planned. Built (2026-11-22, `2bc444d`): 10 new tests in the library, 1 in the binary. The words an upgrade's helper is started by are pinned in the same commit that widens the start, not in one before it: each slice is one commit.
5. The route and the docs (3.5): Planned. Built (2026-11-22, `3d8176f`): 7 new tests in the library; both docs gates pass, the HTTP API gate with one new heading (69 routes, 68 sections).
6. The device tree (3.6): Planned. Built (2026-11-22, `d071965`): 5 tests in `server/tests/restart_inits.rs`.
7. A real agent and a real helper on loopback (3.7): Planned. Built (2026-11-22, `effcfaf`): 3 tests in `server/tests/restart.rs`, about 7 s. It found that a hand-off that has just ended still holds the in-flight lock for an instant after its record reads done; the route now waits up to 250 ms for the lock (3.5).
8. The bench (3.8): Planned. It needs a relay, units and the release's payloads.

**What slices 1 to 7 ran (2026-11-22, at `effcfaf`, on the pinned 1.97.1).** `cargo test --workspace`: 644 passed in the library and 1 not run (below), 11 in the binary, 51 in the MCP, 17 in `relay_events.rs`, 9 in `cli_name.rs` (1 ignored, root), 3 in `restart.rs`, 5 in `restart_inits.rs`, 3 in `xe300_relay_route_sh.rs`, 1 and 4 in the two comms crates; the two netns tests ignored, as without root. `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all --check` clean. `cargo doc --workspace --no-deps`: the five warnings it had before, none from these files. `scripts/check-http-api-docs.py` and `scripts/check-config-docs.py` pass. No stray ts-rs output.

**The review of slices 1 to 7 (2026-11-22 and 23).** Eight findings, one commit each, each with a test that fails without it:

1. The instance is drawn without a wait for entropy (`7220473`). ADR-014 ruling 6 is amended.
2. The agent does not wait on its helper for ever: the lock is released once the helper is started, and the wait is bounded (`548c69b`).
3. Every step of the restart's helper is bounded: the command, each health read, and the helper as a whole (`8c6639c`). The upgrade's helper is left as it is; its unbounded run is in section 4.
4. The start's room counts the binary the helper pins (`84d2273`).
5. A window is judged by its last answered read, and silence by its tail (`5849258`).
6. `sctl upgrade` from a shell and a restart read each other's lock on disk (`af4f57f`).
7. `ok` is written only by the helper; a new process writes `interrupted` (`464aacf`).
8. Not part of this TRD: `netwatch::owner::tests::a_preferred_wire_holds_the_route_over_a_lower_metric_lte` waits on the paused clock, as its siblings do (`7b0b793`). It was the test the first run could not end: on the real clock its `next_round` slept the schedule's minutes.

**What ran after the review (2026-11-23, at `7b0b793`, on the pinned 1.97.1).** `cargo test --workspace`, nothing skipped: 664 passed in the library, 11 in the binary, 51 in the MCP, 17 in `relay_events.rs`, 9 in `cli_name.rs` (1 ignored, root), 4 in `restart.rs`, 5 in `restart_inits.rs`, 3 in `xe300_relay_route_sh.rs`, 1 and 4 in the two comms crates; the two netns tests ignored, as without root. `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all --check` clean; both docs gates pass.

One run on the build machine failed once outside this TRD's code and is left as it is: `infra::monitor::tests::a_run_whose_record_did_not_reach_the_disk_does_not_run` stopped at "at 10s: the command started" in a run of the suite that took 74 s where it takes half a second, then passed alone 12 times of 12 and in the next full run. The test gives a recovery's record 2 s to fail its write; the machine's disk stalled past that, as it did for three recovery tests in TRD-9 slice 9's record. The gate of slice 3 is not in that path: `Runs::start` answers at once and waits for nothing, a closed gate starts no run at all, and `run_recovery` never reads the gate.
