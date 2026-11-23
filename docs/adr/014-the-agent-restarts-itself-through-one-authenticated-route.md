# ADR-014: The agent restarts itself through one authenticated route

Status: Proposed (2026-11-22). Realized by [TRD-10](../trd/TRD-10-the-agents-own-restart.md), which rides sctl's next release. Cites `docs/upgrade.md` (the layouts, the hand-off to the helper, `upgrade.state`), `docs/http-api.md` (`GET /api/health`, `POST /api/exec`, `GET /api/upgrade`, `net.state`, `/d/{serial}/api/*`), `docs/safe-mode.md` and ADR-010, ADR-013. Lines are at sctl `a222339`.

## 1. Trigger

An agent that has stopped doing one of its jobs (a monitor that completes no round, a tunnel client that will not reconnect, a start in safe mode whose flag was cleared) is put right by starting it again. Nothing in the agent does that on request. A caller that wants it today sends a shell text through `POST /api/exec`: the layout's restart command, detached so that it outlives the process that runs it. To write that text the caller has to know the unit's layout, its init script's kill switch and its `/tmp` threshold, and it carries a root shell text for what is one fixed act. On a `ramboot` unit no such text is safe to carry, since a start may fetch the whole payload set over the uplink, so those units have no restart short of a reboot. `docs/safe-mode.md` (lines 117 to 119) sends a person who cleared the flag to "the operational notes on restarting sctl over its own tunnel", which this repository does not hold.

The managed upgrade already restarts the agent, on every layout, detached, and proves the process that comes back (`docs/upgrade.md`, steps 6 and 7). This decision gives that mechanism a second door.

## 2. What existed

Code paths are under `server/src/`; `devices/`, `docs/` and `server/files/` are at the repository root.

**How the upgrade restarts the agent.**
- The agent copies its running binary to the stage as `helper` (`upgrade/stage.rs:742-748`), waits for the Infra recovery runs in progress, at most a run's own 300 s (`:793-816`; `infra/monitor.rs:84`), writes `phase: applying` and starts `helper upgrade-apply --stage <stage>` (`:778-788`).
- The helper is started in a session of its own (`setsid`), its output in a log; through the unit's loader words where the unit runs its agent under a loader (`install.json`'s `helper_prefix`); and on the `systemd` layout as a transient unit through `systemd-run --collect`, outside the service's cgroup (`upgrade/stage.rs:822-896`).
- The helper waits 2 s so the answer and the state push leave (`upgrade/apply.rs:23`, `:243`), swaps the files, runs the install's restart command through `sh -c` (`:315`), and waits for `GET <health_url>` to answer the expected version, with the tunnel connected where the unit has a tunnel client, twice, 5 s apart, within 180 s (`:17-21`, `:163-203`). Nothing in that check tells a new process from the old one: it reads `version` and `tunnel.connected` (`:36-48`), which is enough only because an upgrade changes the version.
- One mutex covers a request from its refusals to its hand-off (`upgrade/routes.rs:96`, `:133-137`), and a state of `staging` or `applying` refuses the next request (`:104-121`). The helper holds the stage's lock file while it works (`upgrade/apply.rs:401-413`); the agent that starts after the restart reads the state file, and while the lock is held watches it with inotify (`upgrade/watch.rs:34`, `:78`; `main.rs:799-801`).
- The state is pushed as `upgrade.state` once after every register ack and on every change (`tunnel/upgrade_state.rs:1-7`), and the relay keeps the message whole, checking only its size and its connection (`tunnel/relay.rs:1438-1486`).

**What the agent knows of its own layout.** `install.json`, read once at start (`upgrade/install.rs`): the layout (`:30-48`), the restart command, which a file may override (`:180-182`, defaults `:219-288`), the health URL (`:360-366`), the rollback directory, `min_free_kb`, the systemd unit, and on `ramboot` the boot fetcher's config and cache (`:120-137`). `InstallInfo::running` gives the binary and its loader words (`:347-357`). The agent reports the layout in `GET /api/health`, in `GET /api/upgrade` and in `tunnel.register` (`routes/health.rs:205-216`; `upgrade/routes.rs:17-33`; `tunnel/client.rs:1301-1308`). With no `install.json` (an install before 0.6.7) it knows none of this.

**What it does not know.** Each of these lives only in an init script:
- The kill switch. `/etc/sctl/disabled` keeps the agent down: the XE300's init returns without starting (`devices/xe300/sctl.init:12`, `:35`), and the shared ramboot init refuses and ends its own respawn loop (`devices/common/sctl-ramboot.init:9`, `:17-20`, `:65-68`). The RUT241's `gz-tmp` init and the `usr-bin` init do not read it (`devices/rut241/sctl.init`, `server/files/sctl.init`). No line of the agent names the file.
- The `/tmp` threshold. A `gz-tmp` init refuses to start below 16384 KB free on the XE300 and 8192 KB on the RUT241, measured before it removes the old run directory (`devices/xe300/sctl.init:13`, `:37-39`; `devices/rut241/sctl.init:12`, `:33-35`). `install.json`'s `min_free_kb` (4096 on `gz-tmp`) is the upgrade's margin for a swap, another number for another purpose.
- What a `ramboot` start costs. The boot fetcher reuses a cached payload whose SHA-256 matches `ramboot.conf` and fetches any other from the relay's mirror, 24576 KB of free `/tmp` required only then (`devices/common/sctl-ramboot.sh:31`, `:120-155`, `:182-197`). A restart with a valid cache sends nothing; one without downloads the set.
- What starts it again. See the table in decision 5.

**What a start reports.** `boot`, when the agent started in unix ms, is stamped once (`main.rs:742-747`) and carried by `net.state`, by `infra.state` and by `GET /api/infra/results` (`netwatch/mod.rs:130`; `tunnel/net_state.rs:74`; ADR-010 decision 3); the Infra digest's hash covers it. A new `boot` in a report means the agent restarted. `GET /api/health`, the one route that needs no key and the one the upgrade's helper reads, carries `uptime_secs` and no `boot` (`routes/health.rs:208`). A relay's own agent runs no `net.state` (`main.rs:773-775`).

**What a stop does.** On SIGTERM the server stops taking requests (`main.rs:1283-1305`), switches the comms provider's location reports off (`:1337-1345`) and ends every session: SIGTERM to each session's process group, SIGKILL after 3 s (`:1355`; `sessions/mod.rs:484`). A job is a non-PTY session and ends the same way. The next start brings each journal back as an archived session with no process, closed to AI clients (ADR-013), and its start-up sweep removes it (TRD-9 section 6). An Infra recovery run ends with the agent, and the next start logs it with its outcome unknown (ADR-010 decision 9). The agent keeps no list of the `POST /api/exec` calls in flight.

**Who may call.** Every route but `GET /api/health` sits behind the unit's one API key (`main.rs:834-934`; `auth.rs:24`). There are no scopes, and the agent cannot tell a person's request from an AI client's on that key (ADR-013 decision 2). A relay forwards any route under `/d/{serial}/api/` it does not name itself (`tunnel/relay.rs:653-659`, since 0.6.0).

**The supervisor inside the agent.** `sctl supervise` starts `sctl serve` again with a backoff, writes the safe-mode flag after three exits inside 180 s, and stops for good on a clean exit (`supervisor.rs:1-18`, `:104-235`). No device init runs it by default (`server/files/sctl.init:9-11`); `server/files/sctl.service` does.

## 3. Decision

1. **One route, the upgrade's mechanism.** `POST /api/restart`, behind the unit's key, restarts the agent as the managed upgrade does, less everything an upgrade moves. Shared with the upgrade, used as it is:
   - `install.json`: the layout, the restart command with a file's override, the health URL, the loader words, the systemd unit;
   - the detached start: a session of its own, its output in a log, the loader words, `systemd-run --collect` on the `systemd` layout (`spawn_helper`, given the helper's subcommand and its unit name);
   - the 2 s the helper waits before it touches anything, so the answer and the state push leave;
   - the restart command run through `sh -c`, and the health check's reader, interval, passes and window: two answers 5 s apart within 180 s;
   - one mutex and one in-flight rule across both: while either an upgrade or a restart is in flight, the other is refused;
   - the helper's lock file and the reconcile at start (the restart's record is looked at once a second while its helper works, where the upgrade's state is watched: a restart's wait has to be bounded, decision 6);
   - the `upgrade.state` push and the relay's keeping of it.

   Not shared: no manifest, no fetch, no stage, no swap, no rollback set, and no copy of the binary. Nothing is replaced, so the helper is the running binary started again (`InstallInfo::running`), as `sctl restart-apply`. A caller sends no shell text and needs no knowledge of the layout.

2. **The request and the answer.** The body is `{"request_id": "...", "unless": [...]}`, both optional. `request_id` is the caller's name for this restart, at most 128 bytes, echoed in the record. The agent answers before anything stops:

   ```json
   {"accepted": true,
    "restart": {"v": 1, "phase": "accepted", "request_id": "r-41",
                "boot_before": 1790000000000,
                "instance_before": "5f0c2a9e...",
                "accepted_at": "2026-11-22T15:02:11Z"},
    "window_secs": 180,
    "safe_mode": false,
    "ends": {"sessions": 2, "attached": 1, "jobs": ["pb:outlet-cycle"], "transfers": 0}}
   ```

   - 202 with `accepted: true`: the restart will be tried. A caller behind a proxy that turns every success into 200 reads the field, never the status.
   - `boot_before` is this process's `boot`, the value its `net.state` and `infra.state` carry. `instance_before` is this process's `instance` (decision 6). A key with nothing to say is left out of the record.
   - `ends` names what the restart ends (decision 4). `safe_mode` says whether the safe-mode flag is present now, in which case the next process starts in safe mode again.
   - Any answer without `accepted: true` is a refusal in the agent's error shape (`docs/errors.md`): a `code`, and for `RESTART_REFUSED` a `detail.reason`.

3. **Refusals, each before anything stops.**

   | Status | `code` | `detail.reason` | When |
   |---|---|---|---|
   | 409 | `UPGRADE_IN_FLIGHT` | | An upgrade is staging or its helper has the unit. |
   | 409 | `RESTART_IN_FLIGHT` | | A restart is accepted or its helper is still watching; `detail.restart` holds its record. |
   | 400 | `RESTART_BAD_REQUEST` | | A `request_id` over 128 bytes, or an unknown word in `unless`. |
   | 422 | `RESTART_REFUSED` | `no_install_json` | The agent does not know how it is started. |
   | 422 | `RESTART_REFUSED` | `disabled` | `/etc/sctl/disabled` exists, the one switch every init that has a kill switch reads: an agent stopped now would not start again. The agent cannot tell which init it runs under, so the file refuses on every layout, those whose init does not read it included (decision 5's table says which). |
   | 422 | `RESTART_REFUSED` | `recovery_running` | An Infra recovery run is in progress (`Runs::in_progress`, `infra/mod.rs:565`). The route does not wait for it: a caller counts its deadline from the send, and a run may be a power cycle between off and on. |
   | 422 | `RESTART_REFUSED` | `no_space` | A layout that runs from `/tmp`: less free space there than the init needs to start again (decision 5). |
   | 422 | `RESTART_REFUSED` | `cache_invalid` | `ramboot`: a payload `ramboot.conf` names is not in the cache with its SHA-256, so the start would download it over the uplink. |
   | 422 | `RESTART_REFUSED` | `start_unreadable` | A file the init needs to start is missing or unreadable: the agent's config, a `gz-tmp` payload, the ramboot wrapper or `ramboot.conf`. |
   | 422 | `RESTART_REFUSED` | `loader_unknown` | A `ramboot` unit that runs its agent under a loader `install.json` does not name (no `helper_prefix`): the helper cannot be started. |
   | 422 | `RESTART_REFUSED` | `relay` | The agent is a relay. Its restart drops every unit's tunnel at once, each of which then pays a reconnect, and a person at the host has `systemctl restart`. |
   | 422 | `RESTART_REFUSED` | `sessions_open`, `jobs_running`, `transfers_running` | Only when the request's `unless` names that kind (decision 4). `detail.ends` says what is running. |

   From the accept to the stop, no recovery run starts: the accept closes the set of runs in the same step that finds it empty, a Down is still recorded and pushed, and its run is left to the next process. A restart on request therefore never cuts a run, with no second judgment at the hand-off.

4. **A person's console does not refuse a restart; the answer says what ends.** A session can stay open for days, and a restart is asked for when something is stuck, a session included.
   - `ends.sessions` counts the running terminals, `ends.attached` those with a client attached now, `ends.jobs` names the running jobs (at most 16 names, each cut to 64 bytes), `ends.transfers` counts the file transfers in flight.
   - A caller that wants the refusal decided in the agent, at the instant of the accept, names kinds in `unless`: `sessions` (a running terminal with a client attached), `jobs` (a running job), `transfers`. A playbook a client runs as a job is one of those jobs; the agent stores playbooks and runs none itself, so it cannot tell a playbook's `POST /api/exec` from any other.
   - After the answer the agent ends every session and job itself (`kill_all`), before the helper is started, so none is left to a supervisor's kill timeout. A journal comes back as section 2 says; on a `gz-tmp` unit whose `data_dir` is under `/tmp/sctl`, the init removes it with the run directory.
   - A `POST /api/exec` in flight is not listed and not ended by the route: the agent keeps no list of them. It ends as it does at any stop.

5. **How each layout is restarted, and what starts the agent again when the new process fails.** The helper runs the install's restart command. What that command does, and what the agent checks first so that the init's own refusals cannot strand it:

   | Layout | The command does | The init's kill switch | Checked before the accept | If the new process exits, it is started again by |
   |---|---|---|---|---|
   | `usr-bin` | `/etc/init.d/sctl restart`: procd stops the instance and registers it again | none (`server/files/sctl.init`) | the kill switch file | procd, 5 s later; procd stops trying after ten quick exits, each within an hour of its start (`respawn 3600 5 10`, `server/files/sctl.init:24`), and after that nothing does |
   | `gz-tmp` | the same command; the init checks its kill switch where it has one, the config and free `/tmp`, removes `/tmp/sctl`, expands the payloads from flash and registers the instance | the XE300's reads it (`devices/xe300/sctl.init:35`); the RUT241's has none (`devices/rut241/sctl.init`) | the kill switch file, the config and both payloads readable, free `/tmp`: the device's `restart_tmp_kb` plus the size of the running binary, which the helper pins | procd, 10 s later, without limit (`respawn 5 10 0`). If the init returns before it registers an instance (a check that failed at that moment), nothing does until the next boot: hence the second run below |
   | `ramboot` under procd | the same command; the init checks the kill switch, the wrapper and `ramboot.conf`; the wrapper expands the cached payloads and runs the agent, through the loader where there is one | read (`devices/common/sctl-ramboot.init:17`) | the kill switch file, the wrapper and its config, every payload cached and valid, free `/tmp` (the size of the running binary and its loader, which the helper pins, plus `min_free_kb`), the loader words | procd, 30 s later, without limit (`respawn 10 30 0`), each time through the wrapper, which fetches a payload that is no longer valid |
   | `ramboot` with no procd | the init's own `stop` and `start`: it ends its supervisor loop and the agent, then starts a new loop (`devices/common/sctl-ramboot.init:43-76`, `:133-145`) | read, at start and at every respawn (`:65`) | the same | the init's loop, 10 s later, without limit, until the kill switch exists |
   | `systemd` | `systemctl restart <unit>`, from a transient unit | none (`server/files/sctl.service`) | the kill switch file | systemd, 5 s later, on a failed exit only (`Restart=on-failure`); a unit with a start limit stays failed once it is reached (`server/files/sctl.service:7-8`) |

   - Free `/tmp`, on a layout that runs from there: `restart_tmp_kb` (never less than `min_free_kb`) plus the size of what the helper pins, the running binary and its loader where there is one. The helper is the old binary running from `/tmp`, so that file's room stays taken while the init writes the new copy, on the first run and on a second; and the init measures its own threshold while the old copy is still there, counting none of it.
   - `restart_tmp_kb` is a new optional `install.json` key. It belongs to the device's install record, written per device from `devices/`, beside the restart command, and equals what that device's own init demands: 16384 on the XE300, 8192 on a RUT241 installed as `gz-tmp`. Where a `gz-tmp` record gives none (a unit installed before the key existed, since a managed upgrade writes no record), the agent reads the threshold from that unit's own init, one line of the form `MIN_TMP_KB=<digits>` read as text and never run, and takes 16384 only when the init says nothing it can read (TRD-11, ruled 2026-11-23); elsewhere it takes 0. The agent reports where the number came from. A threshold stricter than the init's would refuse every restart on a model with less room. Each device's value is confirmed on the bench.
   - The kill switch's path joins `install.rs` beside the hold file's. TRD-10 holds a test that reads the init scripts in `devices/` and fails when a threshold there differs from what that device's install script writes, or a kill switch there is another path.
   - **A second run on `gz-tmp`, never quiet.** There the init can return without registering an instance, and nothing else starts the agent before the next boot. When nothing answers at the end of the first window and no new process answered in it, the helper runs the restart command once more and waits one more window. "Nothing answers" is not one failed read: every health read of the window's last 30 s failed, at least three of them. Never on another layout (a supervisor is already trying, and on `ramboot` a second restart would cut a fetch), and never while the old process still answers. The record says so: `attempts` is 2 and `detail` names the second run, the helper logs it, and the agent that reads the outcome logs it at warn, so a caller reports it as a defect to chase.
   - **The cause is removed where it can be.** The `gz-tmp` inits in `devices/` run their own checks (the kill switch, the config, the payloads, free `/tmp`) before a `restart` stops the running agent: a check that fails leaves the agent running and says why. An init reaches a unit only when its install script runs, never by a managed upgrade, so the route's checks and the second run stay.
   - With no `install.json` the route refuses. The agent is not restarted by a guess.

6. **What proves the restart.**
   - `GET /api/health` gains `instance`: an opaque value made when the process starts. It is an identifier, not a secret, and it only has to differ from the last start's. Drawing it never blocks: it is random where the kernel gives randomness without waiting, mixed with the pid and the clocks, and made of those alone where the kernel gives none. It is drawn before the listener binds, and a start that waited for the kernel's pool would be silent on health, on the API and on the tunnel while the helper's window ran out. It is not a time and not a counter, so the route that needs no key tells a reader only that two answers came from the same process or from two. A later narrowing of what that route answers keeps this value, or gives the helper a route of its own that answers on loopback only.
   - The helper counts an answer only when its `instance` differs from the record's `instance_before`; two such answers 5 s apart within the window are `ok`. The tunnel is not required: the helper proves the process, and the caller proves its reach.
   - The helper writes the record's outcome and exits. It never reboots the unit, and it restores nothing, since nothing was replaced.
   - Every step of the helper is bounded, since it holds the lock that keeps every later restart and upgrade out: the restart command runs under a deadline and the helper's own child is killed at it, each health read has a total deadline, and the helper as a whole ends by a hard limit derived from its windows, its outcome written first.

     | `outcome` | Meaning |
     |---|---|
     | `ok` | A new process answered health twice. `instance_after` is its `instance`; `attempts` says which run of the command it answered. |
     | `not_restarted` | The last health read answered in the window was answered by the process that accepted the request, and it is not established that nothing answers: the restart command did not stop it. A single failed read at the end changes nothing. |
     | `not_healthy` | No new process answered twice within the window, or the two on `gz-tmp`. What starts it later, if anything, is the table above. |
     | `interrupted` | The agent started again while the restart was in flight, and its helper left no outcome: a new process found the record at `restarting` with no helper's lock held. By what it started is not known: the restart command, a crash between the record's write and the helper's start, a power cut. `ok` is written only by the helper, which read the new process's health. |
     | `not_started` | The helper never ran: the agent could not start it and kept running, the gate on recovery runs open again, or the agent stopped some other way before the hand-off. |

   - A process that is still running when its helper ends the record was not restarted, and opens the gate on recovery runs again. It does not wait on its helper for ever: a helper found gone without an outcome, or silent past its own time and a margin, is settled `not_restarted` (the detail says the helper was lost), and requests are taken again. The in-flight mutex is held for the hand-off only, as the upgrade holds it for its staging.
   - The agent that starts reads the record. `restarting` with the helper's lock held is waited on, with the same bound; `restarting` with the lock free and no outcome, read by a process whose `instance` is not `instance_before`, is settled as `interrupted`, never as `ok`. The process the helper proved (`instance_after` is its own) writes its `boot` into the record as `boot_after`.
   - The caller's proof is the next start, which the agent already reports: a `net.state` or `infra.state` whose `boot` is not `boot_before`. The record ties that start to the request: `request_id`, `boot_before`, and `boot_after` equal to the new `boot`.

7. **Where the record is read, and how a caller knows the route is there.** The record lives beside the upgrade's state (`<state_dir>/upgrade/restart.json`).
   - `GET /api/upgrade` gains the key `restart`: the record, `{"v": 1, "phase": "idle"}` when no restart was ever asked. Its presence is how a caller knows the agent has the route. An agent without the key is never asked.
   - The pushed `upgrade.state` gains the same key, so a collector on the relay's event stream learns the presence and every outcome with no request of its own. The message stays v1: the key is additive, and the relay keeps the message whole.
   - No `features` entry is added: `features` names what a device pushes and a relay must advertise.

8. **What a restart never does.**
   - It never reboots the unit and never falls back to a reboot.
   - It changes no file the unit is configured by: not `sctl.toml`, not `install.json`, not the Infra config, not the safe-mode flag. It writes its record and its log tail under `state_dir`, and it clears nothing. Clearing the safe-mode flag stays its own act; `docs/safe-mode.md` names this route in place of the detached text.
   - The route and its helper send nothing to the modem and change no route, address or interface. The site's traffic does not pass through the agent. The agent-owned relay route stays in the kernel across the restart, and the new process adopts it as it finds it (`netwatch/owner.rs:474`, `:770-771`).
   - The new process does what every start of the agent does, no more: it opens the comms provider (location reports, switched off at the stop, come back), and the LTE watchdog's start-up check acts only on a modem left at reduced functionality or deauthorized (`lte_watchdog.rs:453-455`, `:740`), then begins its grace again.
   - It does not count against a crash loop: the init's `restart` and `systemctl restart` start a fresh instance with no respawn history, and `sctl supervise` exits on the signal and starts with an empty history.

9. **Authorization.** The unit's API key, as for every route but health. The agent has one key and no scopes, so whoever may run `POST /api/exec` may call this route, and no caller gains anything by it that the key did not already give. The agent cannot tell an AI client on that key (ADR-013 decision 2), so, as there, the rule holds through the clients sctl ships: the MCP gets no restart tool, and sctlin gets no control for it in this release. The MCP stays a root channel whose `device_exec` can run any command, a restart included (ADR-013 decision 3); this decision does not change that. A session's Allow AI flag has no part in it: a restart is not typed into a session. A relay forwards the route to a device like any other, with the device's key.

10. **Metered links.** The helper reads health on loopback and sends nothing. What a restart costs a unit is one reconnect: the TLS handshake with the relay and the WebSocket upgrade, `tunnel.register` and its ack (`tunnel/client.rs:1299-1330`), then, once each, `net.state` and `infra.state` (each capped at 16 KiB) and `upgrade.state` (capped at 8 KiB, now with the record), and after that whatever a caller chooses to ask a unit that has just connected. A `gz-tmp` start expands from flash. A `ramboot` start is accepted only with a valid cache, so it fetches nothing. No number is claimed here: TRD-10 measures the reconnect in bytes.

## 4. Consequences

- A caller restarts any unit that carries the route with one request and no layout knowledge. The units a carried text could not serve, `ramboot` first, get a restart.
- The agent refuses the restarts that would strand it where it can know so beforehand. Where it cannot (a check that fails between the accept and the init's start on `gz-tmp`, a new process that keeps exiting past its supervisor's limit on `usr-bin` or under a start limit on `systemd`), the table in decision 5 says so, and the record ends as `not_healthy`.
- A restart ends every session and job on the unit, and a client finds none afterwards. The answer said so before it happened.
- A restart can be refused for as long as a recovery run lasts, 300 s at most. A caller that wants both asks again.
- `GET /api/health` gains `instance`; `GET /api/upgrade` and `upgrade.state` gain `restart`; `install.json` gains the optional `restart_tmp_kb`, which the XE300's and the RUT241's install scripts write. Each is additive: older readers ignore what they do not know. The relay needs no change, so no relay upgrade waits on this (ADR-003).
- One route is added, so the HTTP API docs gate needs its heading. No `pub` config field is added.
- An agent before this release has no `restart` key and is never asked. A caller that still carries a restart text for such agents keeps it until they are gone.
- `docs/upgrade.md` gains a section on the restart beside the hand-off it shares; `docs/http-api.md` gains the route, `instance` and `restart`; `docs/safe-mode.md` names the route; `docs/errors.md` gains the three codes.
- A unit already installed keeps the init it has: its `gz-tmp` init still stops before it checks, and a RUT241 installed as `gz-tmp` has no `restart_tmp_kb` in its record, so the agent's 16384 applies there until that record is rewritten.
- It rides sctl's next release with what TRD-9 left owed (TRD-10 section 5), and never ships alone, since each release costs every unit its agent set again. That release and its riders get a TRD of their own.

## 5. Rejected alternatives

- **Exit and let the supervisor start the agent again.** It needs no helper, and it fails in four ways. Where nothing supervises (a process started by hand, a procd instance past its limit), the exit is the end. A clean exit stops `sctl supervise` and is not restarted under `Restart=on-failure`, and a failed exit counts toward procd's limit on `usr-bin`, the unit's start limit under systemd and the crash loop that writes the safe-mode flag. A respawn is not the init's start: on `gz-tmp` and `ramboot` nothing is expanded again, so a damaged binary in `/tmp` stays. And nobody proves the process that comes back.
- **Replace the process image in place** (`exec` of the same binary). The init's checks and its expansion do not run, a loader has to be found again, procd sees nothing, and every open socket and child has to be closed by hand first.
- **Keep the restart a shell text the caller sends.** The caller has to learn each layout's init, and it holds a root text for a fixed act; a `ramboot` unit cannot be served; nothing ties the next start to the request.
- **Wait for a recovery run, as the upgrade does.** The upgrade has minutes to spend and files already fetched. A restart on request is answered now; a caller's deadline starts at its send.
- **A copy of the binary as the helper.** The upgrade needs it because the installed file is replaced. Here it would cost a write of several megabytes to flash, or the same room in `/tmp`, on every restart.
- **A second attempt on every layout, or a reboot when the agent does not come back.** A reboot takes the site's uplink with it, which is what a restart exists to avoid. Where a supervisor is already trying, a second run of the command adds nothing and on `ramboot` can cut a fetch; it is kept for `gz-tmp` alone (decision 5).
- **The start time as the value health carries.** It is what `net.state` already calls `boot`, but the route needs no key, and the helper needs only to tell two processes apart.
- **Refuse while any session is open.** One forgotten console would hold the restart off for days (decision 4).
- **Fold it into `POST /api/upgrade`** as an upgrade to the running version. That request answers `accepted: false` for the version a unit already runs, and callers rely on it.
- **A WebSocket frame or a tunnel message of its own.** The relay forwards any REST route; a new message needs relay code, and the relay upgrades last (ADR-003).
- **A key or a scope of its own.** The agent has one key; per-caller keys are a later record (ADR-013 section 5).

## 6. What TRD-10 proves

- `cargo test`, no root: each refusal with its code and nothing stopped; an upgrade in flight refuses a restart and a restart in flight refuses an upgrade; the accept closes the recovery runs and a failed hand-off opens them again; the helper with a scripted runner and health, as `upgrade/apply.rs`'s tests do it: a new `instance` twice is `ok`, the old one to the end is `not_restarted`, silence is `not_healthy`, and on `gz-tmp` silence brings one second run that the record names; the record survives the restart and a start with the lock free settles it; `restart` is present and idle on an agent never asked; the relay keeps and replays an `upgrade.state` that carries it.
- The init scripts and install scripts in `devices/`, read as text and never run: each `gz-tmp` init checks before its `restart` stops, its threshold is the one its install script writes, and its kill switch is the path the agent checks.
- A real agent and a real helper on loopback, with no relay: the request is answered 202, the helper runs a restart command that only writes a marker, reads a health server the test stands in, and the agent reports the outcome the helper wrote.
- Left to a bench, since each needs a relay or a unit: the 202 arriving through a relay before the stop; the next `net.state` with a new `boot` and the record's `boot_after` equal to it; the bytes a reconnect costs; one restart on a unit of each layout, the unit never rebooting and its uplink never dropping.

## 7. Ruled by the lead (2026-11-22)

The ten questions this ADR left open, each with its ruling. The decisions above are written as ruled. The ADR stays Proposed until the owner accepts it.

1. **Open sessions.** Accept and list what ends; `unless` lets a caller ask the agent to refuse (decision 4).
2. **Sessions and jobs are ended by the agent before the stop.** Yes (decision 4).
3. **A relay refuses** (`relay`). Yes (decision 3).
4. **A second run of the restart command on `gz-tmp`.** Yes: once, only when nothing answers after the first window, and never quiet: the outcome carries that the second run was needed, and it is logged at warn, so a caller reports it as a defect to chase. And the cause is removed where it can be: no device init stops the agent before its own checks have passed. The route's checks stay; the init must not be able to strand a unit on its own (decision 5). It is built for the two `gz-tmp` inits, the only ones with a check that can fail between a stop and a start by itself; the shared ramboot init is left as it is, for the reasons TRD-10 3.6 gives, until the lead says otherwise.
5. **A `ramboot` unit with an invalid cache refuses** (`cache_invalid`), with no override. Yes.
6. **A value on the route that needs no key.** Yes, as an opaque value made at process start: not a time, not a counter. Amended on the review (2026-11-22), in place of "random, derived from nothing on the unit": it never blocks; it is random where the kernel gives it without waiting; it only has to differ from the last start's. A later narrowing of the keyless health answer keeps it or moves the helper's check to loopback (decision 6). The ruling called the value `boot`; it is built as `instance`, so that `boot` keeps its one meaning, the start time `net.state` carries. The name is the builder's and stands until the lead says otherwise.
7. **An upgrade hold does not refuse a restart.** Yes: a hold (`[upgrade] hold`, `/etc/sctl/upgrade-hold`) is the unit's word on changing versions.
8. **No MCP tool and no sctlin control in this release.** Yes (decision 9).
9. **`restart_tmp_kb`.** Not one default for every `gz-tmp` model. The value belongs to the device's install record, written per device from `devices/` beside the restart command, equal to what that device's own init demands; 16384 only where a device gives none. Each device's value is confirmed on the bench (decision 5).
10. **The release and its riders get a TRD of their own.** Yes. It is not written with this ADR.

Also ruled:

- No `cancelled` outcome and no second judgment at the hand-off (decision 3's gate); the helper is the running binary, not a copy (decision 1); the tunnel is not required for `ok` (decision 6).
- The kill switch: `disabled` reads the same file on every layout, and decision 5's table says which inits have a switch. No init gains one by this decision.
- `usr-bin`'s `respawn 3600 5 10`: noted in the table, not changed.
- Whether the shared ramboot init's own `stop` runs in place of procd's on a unit that has procd is a bench item (TRD-10 3.8): if it did, `restart` there would not stop the instance, and the route would report `not_restarted`.
