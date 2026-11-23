# TRD-10: The agent's own restart

Implements [ADR-014](../adr/014-the-agent-restarts-itself-through-one-authenticated-route.md). Status: Planned. It rides sctl's next release, the first after 0.6.13, and never a release of its own; section 5 lists what that release also carries. (TRD-7 is held for ADR-007's option B, and TRD-8 and TRD-9 are taken, so this is TRD-10.)

## 1. Objective

After the release that carries this:

- a caller restarts a unit's agent with `POST /api/restart`, on every layout, sending no shell text and knowing nothing of the layout;
- the agent refuses, before anything stops, a restart that would strand it or cut something that must not be cut: an upgrade or another restart in flight, a recovery run, the kill switch, a start its init would refuse, a `ramboot` start that would fetch;
- the answer leaves before the agent stops and says what the restart ends;
- the next start is tied to the request: `boot` differs, and a record names the request, the `boot` before and the `boot` after;
- a caller knows the route is there by one key in an answer the agent already gives, so an older agent is never asked;
- the unit never reboots for it, its modem and its routes are not touched, and what a reconnect costs in bytes is a measured number.

No relay change is needed.

## 2. What 0.6.13 has

ADR-014's section 2 holds what the decision found. This section holds what the build needs besides. Lines are at `a222339`.

- **Main.** `main` is 3 commits past `v0.6.13`, at `a222339`: a payload build fix (`5d3c4fb`) and two commits of TRD-9's records. `server/src` is as 0.6.13 released it.
- **The upgrade's two halves.** `upgrade/stage.rs` is the running agent's half and ends in `hand_off` and `spawn_helper` (`:778-788`, `:822-896`); `upgrade/apply.rs` is the helper's, plain synchronous code with its runner and its health check behind two traits (`Runner`, `:90-106`; `Health`, `:26-48`), which its tests replace with a scripted service (`:464`). `spawn_helper` builds the `upgrade-apply --stage` arguments and the unit name `sctl-upgrade-<version>` inside itself.
- **The state.** `upgrade/state.rs` holds the one upgrade state, its file under `<state_dir>/upgrade/` (`:211`) and its handle with the in-flight mutex (`:155-160`). `tunnel/upgrade_state.rs` flattens that state into the pushed message (`:29-40`) and trims the log tail when the message passes 8 KiB (`:87-104`).
- **Recovery runs.** `Runs` is a watch over the set of target ids with a run in progress (`infra/mod.rs:530-575`); `start` is one `send_if_modified`, so finding the set empty and closing it can be one step of the same kind. The upgrade's context takes the runs of the agent's own Infra state (`upgrade/routes.rs:69`), and a test holds that it is that set and no other (`:165-225`).
- **Sessions and transfers.** `SessionManager::list_sessions` gives each session's kind, status and attached count (`sessions/mod.rs:58-75`, `:727`), `kill_all` ends them (`:484`), and the transfer manager lists the transfers it holds (`gawdxfer/manager.rs:1007`).
- **Tests.** The library's tests run with no root. `server/tests/cli_name.rs` starts a real agent on a staged layout through `SCTL_INSTALL_JSON` and `SCTL_CLI_PATH`, and runs shell text under the system `sh` and BusyBox's only on files in its own scratch directory, every path an argument. `server/tests/relay_events.rs` runs fake devices against the real relay router. Root-only network tests follow `server/tests/relay_route_netns.rs`: `#[ignore]`, root and `SCTL_NETNS_TEST=1`, built with `--no-run` and run as the test binary under sudo, every command an argument vector. `devices/we826-qwd/tests/` holds the shell tests of that device's scripts.
- **CI and the gates.** As TRD-9 section 2 lists them. This build adds one route, so the HTTP API docs gate (`scripts/check-http-api-docs.py`) needs a heading for it; it adds no `pub` config field.
- **Line ends.** `CHANGELOG.md` is CRLF, and so is `web/src/lib/utils/playbook-parser.ts`, which a rider edits (section 5). Every edit to them is byte-safe and keeps each line's ending. Every other file named here is LF.

## 3. Deliverables

Each change says what changes, in which files, how it is tested and when it is accepted. Section 6 runs the checks that cover them all.

### 3.1 The record, and `boot` where the helper reads

**Change.**

- `GET /api/health` gains `boot`: when this process started, unix ms, the value `net.state` and the Infra digest already take (`main.rs:742-747`). It joins the app's state so the route and the record read one number.
- The restart's record: `v`, `phase` (`idle`, `accepted`, `restarting`, `done`), `request_id`, `boot_before`, `boot_after`, `outcome` (`ok`, `not_restarted`, `not_healthy`, `not_started`), `reason`, `detail`, `attempts`, `accepted_at`, `ended_at`, `log_tail` (the last 1 KiB of the helper's log, cut on a character boundary). It lives at `<state_dir>/upgrade/restart.json`, written through a temporary file and a rename, and is read at start.
- `GET /api/upgrade` gains `restart`, the record, `{"v": 1, "phase": "idle"}` on an agent never asked. The pushed `upgrade.state` gains the same key; the forwarder sends when either the upgrade's state or the record changes, still at most once a second, and drops the record's log tail before the upgrade's when the message passes 8 KiB.
- At start: a record left `accepted` (the agent stopped before the hand-off) ends as `not_started`; `restarting` with the helper's lock held is watched as the upgrade's state is; `restarting` with the lock free, read by a process whose `boot` is not `boot_before`, ends as `ok` with `boot_after` its own `boot` and a detail saying the helper did not report; `restarting` with the lock free and the same `boot` ends as `not_started`.

**Files.** `server/src/upgrade/restart.rs` (new: the record, its file, its handle), `server/src/upgrade/mod.rs`, `server/src/upgrade/routes.rs` (`report`), `server/src/upgrade/watch.rs`, `server/src/tunnel/upgrade_state.rs`, `server/src/routes/health.rs`, `server/src/state.rs`, `server/src/main.rs`.

**Tests.**

- The record round-trips, an unknown key in a stored record is ignored, and a file that is not a record reads as idle with a log line.
- Each of the four starts above settles as written.
- `report` carries `restart` on an agent never asked; health carries `boot`, equal to the `boot` its `net.state` is built with.
- The pushed message carries `restart`; a change of the record alone sends one message; a message over 8 KiB loses the record's log tail first.
- `server/tests/relay_events.rs`: the relay keeps an `upgrade.state` that carries `restart`, publishes it and replays it whole.

**Accepted when.** An agent never asked shows `restart` idle in `GET /api/upgrade` and in its first `upgrade.state` on a relay's event stream, and its health shows the `boot` its `net.state` carries.

### 3.2 What the agent knows of its own start

**Change.**

- `install.rs` names the kill switch (`/etc/sctl/disabled`) beside the hold file, and reads the optional `install.json` key `restart_tmp_kb`: 16384 by default on `gz-tmp`, 0 elsewhere. `sctl install-info` and `GET /api/upgrade`'s `install` report it.
- One function judges whether a start would be refused, from the install and the unit's files, with no side effect, and answers the first reason in this order: `no_install_json`; `disabled`; `loader_unknown` (a `ramboot` unit whose `ramboot.conf` names a loader, or cannot be read, and whose `install.json` has no `helper_prefix`, as `cli_name` already judges it); `start_unreadable` (the agent's config; on `gz-tmp` each file `install.json`'s `files` names; on `ramboot` the wrapper, executable, and `ramboot.conf`); `cache_invalid` (`ramboot`: each payload `ramboot.conf` names with a SHA-256 must be in the cache with that hash; a payload named with none makes the start unjudgeable, and is refused as `cache_invalid` too); `no_space` (a layout that runs from `/tmp`: free space in the run directory's filesystem below `restart_tmp_kb`, or below the running binary's and plugin's size plus `min_free_kb`).
- The files it reads are opened as regular files only, through the file gate's handle, so a FIFO placed at one of those paths cannot hold the request (section 5, rider 1).

**Files.** `server/src/upgrade/install.rs`, `server/src/upgrade/restart.rs`, `docs/upgrade.md` (the key, and the kill switch as something the agent reads).

**Tests.**

- In a scratch root, per layout: each reason above, alone and in order; a valid `ramboot` cache passes; a cached payload with one byte changed is `cache_invalid`; free space is injected, as the upgrade's stage tests inject it.
- `server/tests/restart_inits.rs` (new) reads the init scripts as text, and never runs them, since they name fixed paths and remove a run directory: every `MIN_TMP_KB` in a `gz-tmp` init under `devices/` is at most the agent's default, and every `DISABLED=` there is the path the agent checks. A new init with a higher threshold fails the test.

**Accepted when.** On a unit staged as each layout, the function's answer matches what that layout's init would do with the same files, for every row of ADR-014's table.

### 3.3 What a restart ends, and the gate on recovery runs

**Change.**

- `Runs` gains a gate: `close_if_idle` closes the set in the step that finds it empty and answers whether it did; while it is closed, `start` answers none; `reopen` opens it. A monitor check that finds the gate closed records and signals its Down as today and starts no run, and neither the cooldown nor the count moves: the run is the next process's.
- `ends` is built from what the agent holds: the running terminals, those with a client attached, the running jobs' names (at most 16, each cut to 64 bytes on a character boundary), the transfers in flight.
- Slice 3 also reads what the stop does to a `POST /api/exec` in flight and to a session whose process ignores SIGTERM, and records both here. ADR-014 lists neither in `ends`.

**Files.** `server/src/infra/mod.rs`, `server/src/infra/monitor.rs`, `server/src/sessions/mod.rs`, `server/src/gawdxfer/manager.rs`, `server/src/upgrade/restart.rs`.

**Tests.**

- A run in progress keeps the gate open and `close_if_idle` answers false; with none it closes, and a Down then starts no run, moves `seq` and is pushed; `reopen` lets the next failed check start it, the count as it was.
- Two tasks, one starting a run and one closing: never both.
- `ends` for no session, a detached terminal, an attached one, an exited one (not counted), a job, 20 jobs (16 names), a name over 64 bytes.

**Accepted when.** With a recovery of about 30 s on a bench agent, a restart asked during the run is refused, and one accepted just before a target goes Down starts no run before the stop; the next process's first failed check starts it.

### 3.4 The helper

**Change.**

- `sctl restart-apply --request <file>`: the helper. It takes the record's lock, waits the upgrade's 2 s, runs the install's restart command through the upgrade's `Shell`, and waits for health with the upgrade's interval, passes and window. An answer counts only when its `boot` is not the record's `boot_before`. It writes `ok` with `boot_after`, `not_restarted` when the last answer inside the window still carried `boot_before`, or `not_healthy`, then exits. It runs nothing else and restores nothing.
- `Health::check` answers the `boot` beside the version and the tunnel, and `wait_healthy` takes the rule an answer must meet; the upgrade passes its version rule and behaves as before.
- `spawn_helper` takes the helper's arguments and its unit name from its caller. The upgrade passes what it passes today; the restart passes `restart-apply --request <file>` and `sctl-restart-<accepted_at in unix seconds>`. The helper is `InstallInfo::running`'s binary through its loader words, not a copy.
- The helper's log is `<state_dir>/upgrade/restart.log`, started afresh for each request.
- If the lead rules for ADR-014's open question 4: on `gz-tmp` only, when no answer at all came inside the first window, the helper runs the restart command once more and waits one more window; `attempts` says which run was answered.

**Files.** `server/src/upgrade/apply.rs`, `server/src/upgrade/stage.rs`, `server/src/upgrade/restart.rs`, `server/src/main.rs` (the subcommand, beside `upgrade-apply`).

**Tests.** With a scripted runner and health, as `apply.rs`'s tests are built:

- a new `boot` twice is `ok` with that `boot`; one new answer followed by silence is `not_healthy`;
- the old `boot` to the end of the window is `not_restarted`; silence is `not_healthy`;
- the same version before and after restarts and is `ok`: the version is not judged;
- a held lock ends a second helper at once, exit 4, the record untouched;
- the upgrade's own tests pass unchanged through the widened `Health` and `wait_healthy`;
- `spawn_helper` builds the same command line for an upgrade as before (a test that pins it first, in the commit before the change), and for a restart the loader words, the binary and the two arguments, on `systemd` inside `systemd-run --collect --unit=sctl-restart-<n>`.

**Accepted when.** On a loopback agent, a helper started by hand against a record restarts the agent through a scratch restart command and writes `ok` with the new process's `boot`.

### 3.5 The route

**Change.** `POST /api/restart`, behind the key, in the order ADR-014 decision 3 gives:

1. The body: `request_id` at most 128 bytes, `unless` a list of `sessions`, `jobs`, `transfers`; otherwise 400 `RESTART_BAD_REQUEST`.
2. The in-flight mutex the upgrade takes, tried and not waited for; an upgrade's state in flight is 409 `UPGRADE_IN_FLIGHT`, a record `accepted` or `restarting` is 409 `RESTART_IN_FLIGHT` with the record. A `restarting` record whose lock is free is settled first, as the upgrade clears a lost helper. `POST /api/upgrade` refuses in the same way while a restart is in flight.
3. A relay is refused (`relay`), if the lead rules so.
4. The start's judgment (3.2).
5. `unless`, against `ends`.
6. The gate: `close_if_idle`, or `recovery_running`.
7. The record `accepted`, written and pushed; the answer, 202.
8. In a task that holds the mutex: every session and job ended (`kill_all`); the record `restarting`; the helper started. A helper that cannot be started ends the record as `not_started`, reopens the gate, and the agent keeps running.

**Files.** `server/src/upgrade/routes.rs` (or a `restart` module beside it), `server/src/main.rs` (the route), `server/src/error.rs` (the three codes), `docs/http-api.md` (the route under "Managed upgrades", `boot` in `GET /api/health`, `restart` in `GET /api/upgrade` and in `upgrade.state`), `docs/upgrade.md` (a section "Restarting the agent" beside the hand-off, and the per-layout table), `docs/safe-mode.md` (lines 117 to 119 name the route), `docs/errors.md` (the codes and the reasons), `CHANGELOG.md` `[Unreleased]`.

**Tests.**

- Each refusal: its status, its code, its reason, and nothing stopped: the gate open, the sessions alive, the record as it was.
- An upgrade staging refuses a restart; a restart accepted refuses an upgrade, and a second restart with the first's record in the detail.
- `unless` with each kind refuses only when that kind is running, and `detail.ends` names it; without `unless` the same unit is accepted.
- An accept writes the record before the answer; the task ends the sessions, then starts the helper, in that order (a helper stood in by a recorder).
- A helper that cannot be started: `not_started`, the gate open again, a recovery able to run.
- Both docs gates pass.

**Accepted when.** Through a bench relay, each refusal reads as its code and reason, and an accepted request's 202 arrives before the device's connection drops.

### 3.6 A real restart on loopback, and its bytes

**Change.** No product code: the harness that proves the whole path.

- `server/tests/restart.rs` (new), in the pattern of `cli_name.rs`: a real agent started by the test on a loopback port, staged as each layout in turn through `SCTL_INSTALL_JSON`, `SCTL_CLI_PATH` and a `state_dir` and `data_dir` in the test's scratch directory. Its `install.json` names a scratch restart command, a script the test writes, which signals the one process the test started, by the pid the test recorded, and starts the same binary again with the same arguments. On the `systemd` staging a stand-in `systemd-run`, first in `PATH`, runs its command detached. The test signals nothing it did not start and removes nothing outside its scratch directory.
- `server/tests/restart_bytes_netns.rs` (new), in the pattern of `relay_route_netns.rs`: an agent in one namespace, a bench relay in another, one veth pair between them; the bytes each way on that link from the accept to the second healthy answer, read from the interface's counters.

**Tests.**

- Per staged layout: the answer is 202 with `boot_before`; the process the test started is gone and another answers; health's `boot` differs; the record reads `ok` with `boot_after` equal to it; a session opened before the request is gone; `request_id` is echoed.
- A restart command that does nothing: `not_restarted`, the first process still answering, its sessions ended, the gate open again.
- A restart command that stops the agent and starts nothing: `not_healthy`.
- The staged `ramboot` layout with one cached payload removed: refused `cache_invalid`, nothing stopped.
- The netns test asserts only that the count is above zero and under 256 KiB, and prints it; the number is recorded in section 7.

**Accepted when.** `restart.rs` is green on the pinned toolchain with no root, and the netns run is recorded here, since CI has no privileged job.

### 3.7 One restart on a bench unit of each layout

**Change.** None. With the release's payloads on bench units the owner names, one of each layout: `usr-bin`, `gz-tmp`, `ramboot` under procd, `ramboot` with no procd and a loader, and `systemd` for an agent that is not a relay if one exists.

**Checks, per unit.** The request through the relay answers 202; the unit's system uptime keeps counting (no reboot); its `net.state` shows the same uplink before and after, and no interface dropped; the next `net.state` carries a new `boot` and the record reads `ok`; a console session open before is gone after; with the kill switch placed by hand the route refuses and the agent keeps running; the bytes of the reconnect, counted on the unit's uplink interface before the request and after the record reads `ok`. On the `ramboot` units, the wrapper's log shows every payload served from the cache.

**Accepted when.** Every layout has one recorded restart, and the cost in bytes is written beside each.

## 4. Decisions inside the build

- **The helper is the running binary, not a copy.** Nothing is replaced, so nothing the helper runs from is replaced. On a layout that runs from `/tmp` the init removes or overwrites the file the helper runs from; a running process keeps its image, and the room for it is what 3.2's `no_space` counts.
- **The record has its own file.** The upgrade's state file is written whole by two processes that expect its shape; a second writer for another purpose would have to be taught to both. One directory, two files, one lock each.
- **The record and its log are under `state_dir`.** `data_dir` is in `/tmp` on the units whose init removes `/tmp/sctl` at every start; the outcome has to be there after the start. The log is a few hundred bytes and starts afresh for each request.
- **The gate is closed in the route, and the sessions are ended in the task.** The gate must be closed in the step that finds no run, before the answer. Ending the sessions takes up to 3 s and must not hold the answer.
- **The accepted order is fixed.** A refusal that costs nothing comes before one that reads files, and the gate is closed last, so a refusal leaves nothing to undo.
- **The version is not judged.** An upgrade proves its version; a restart proves a new process. On `gz-tmp` and `ramboot` the files that start are the ones on flash or in the cache, which the last upgrade left.
- **Init scripts are read as text and never run in a test.** They name fixed paths under `/tmp` and `/etc`, and a `gz-tmp` init removes its run directory.
- **Not in this build.** No MCP tool and no sctlin control for the restart; no `features` entry; no change to an init script or an install script; no WebSocket frame; no restart of a relay if the lead rules as recommended. The RUT241's `gz-tmp` init keeps its lower threshold and gains no kill switch here.

## 5. What the release that carries this must also carry

These ride the same release. They are not slices of this TRD: each is built and reviewed on its own, and the release's own record lists them.

1. **The FIFO reads outside the file gate.** TRD-9 slice 10's bench (2026-11-20) found the failure 0.6.13 fixed in the gate still open on routes the gate does not cover, each older than 0.6.12 and each needing exec or local access to set up, "owed by the next release":
   - a FIFO named `*.md` in `playbooks_dir` makes `GET /api/playbooks` and `GET /api/playbooks/{name}` open it plainly (`server/src/routes/playbooks.rs:182`, `:224`); 32 such reads take every one of the tunnel client's handler permits, and a SIGTERM then leaves the agent alive;
   - a FIFO at `<data_dir>/watchdog_history.jsonl` makes `GET /api/lte/watchdog/history` hold a runtime worker (`server/src/routes/lte.rs:210`, a blocking read inside an async handler), and four of them stop the whole agent;
   - `PUT /api/playbooks/{name}` onto a FIFO with no reader waits in its open;
   - a flood of about 300 refused reads evicts the activity ring's 200 entries, exec history included.

   It belongs with this TRD for a second reason: an agent that a SIGTERM leaves alive is an agent whose restart ends as `not_restarted`.
2. **sctlin passes `{{agent}}` through in a playbook's script.** TRD-9 section 4 kept it out of 0.6.12 ("no sctlin pass-through of `{{agent}}` in a runbook"), and 0.6.13's `web/` differs from 0.6.12's only in its version line. `renderPlaybookScript` (`web/src/lib/utils/playbook-parser.ts:174-200`, CRLF) refuses any `{{name}}` left after the declared parameters as an undeclared parameter. `{{agent}}` names the path that runs the agent's tools on the unit (TRD-9 3.6, `GET /api/health`'s `cli`) and is filled by the server that sends the job, so sctlin must leave it in the text; every other undeclared name stays refused. Its test is in `playbook-parser.test.ts`.
3. **The release itself**, as `docs/releasing.md` and TRD-9 3.9 describe one: the five version carriers and the lock files, `CHANGELOG.md` (CRLF), the tag equal to `server/Cargo.toml`'s version, TRD-1's proof in the `Release` workflow, every target's artifacts measured against 0.6.13, the units before the relay. ADR-014 recommends that a TRD of its own record it (open question 10).

## 6. Verification

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` on the pinned toolchain, as CI runs them; the `test-musl` job; the ts-rs bindings diff and `cargo doc`.
- `scripts/check-http-api-docs.py` (one new heading) and `scripts/check-config-docs.py` (unchanged).
- `server/tests/restart.rs` and `server/tests/restart_inits.rs` with no root; `server/tests/relay_events.rs` with its new case.
- As root, built with `--no-run` and run with `sudo -n env SCTL_NETNS_TEST=1 <test binary> --ignored`: `restart_bytes_netns`, its number recorded in section 7; `relay_route_netns` and `net_tools_netns`, unchanged and still green.
- A bench agent (x86) behind a bench relay on loopback, staged as each layout in turn as TRD-9 section 6 stages them: each refusal by its code through the relay; an accepted restart with the event stream open, where `device.disconnected`, `device.connected`, a `net.state` with a new `boot` and an `upgrade.state` whose `restart` reads `ok` arrive in that order; a recovery of about 30 s refusing a restart, and a restart accepted first starting no run before the stop.
- 3.7's bench units, with the owner's go for each.
- Nothing here runs on a unit in service. A unit in service is restarted by this route only after the release's rollout reaches it, by whoever operates it.

## 7. Slices and status

Build order. Each slice is one or more commits, reviewed before the next begins; the status flips in the change that ships it, with the verification it ran.

1. The record, and `boot` in health (3.1): Planned.
2. What the agent knows of its own start (3.2): Planned.
3. What a restart ends, and the gate on recovery runs (3.3): Planned.
4. The helper (3.4): Planned. Its first commit pins `spawn_helper`'s command line for an upgrade, before the function is widened.
5. The route and the docs (3.5): Planned.
6. A real restart on loopback, and its bytes (3.6): Planned.
7. One restart on a bench unit of each layout (3.7): Planned. It needs the release's payloads, so it runs between the release's build and its rollout.
