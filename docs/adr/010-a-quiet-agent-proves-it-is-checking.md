# ADR-010: A quiet agent proves it is checking

Status: Accepted (2026-10-21). Realized by [TRD-9](../trd/TRD-9-release-0-6-12.md) (release 0.6.12). Cites `docs/http-api.md` (`GET /api/info`, `POST /api/infra/config`, `GET /api/infra/results`, `POST /api/infra/check/{target_id}`, `infra.state`), `docs/safe-mode.md` and `docs/upgrade.md`. Lines are at sctl `ece8b45` and fleet `ea025687`.

## 1. Trigger

In a four-hour chat on 2026-10-20, the owner and the fleet's assistant worked on a customer site's monitoring. The site's Infra results looked stuck after a config push that changed nothing. They were not stuck. Since 0.6.6 a unit pushes `infra.state` only when a target's status changes or a config is applied or removed, and once a push has arrived, netage-server stops pulling `GET /api/infra/results`. From then on, a unit that checks and has nothing new looks the same as a unit whose monitor has stopped. The fleet's Infra page greyed every steady target about three minutes after the site's last change, and the fleet's logs showed six units with results up to 4.5 h old. The one unit whose rows looked live had a camera that answered above its latency threshold: its status flipped between up and degraded on every check, and every flip was a push.

The owner kept the push: "right, not need to waste bandwidth to report the same thing over and over". The owner also ruled: "we must also be sure it's not silence due to a problem". Silence reads as steady only while proof arrives that the unit is still checking.

Reading the monitor for this decision found four more ways a unit goes quiet while something is wrong:
- a recovery runs under the Infra lock and holds back the Down;
- a monitor started by a push has no supervisor;
- Check now changes nothing a collector reads;
- a recovery whose output passes 512 bytes can panic before its cooldown is recorded.

It also found that a recovery can restart a device on its first good answer as it comes back.

## 2. What existed

Code paths are under `server/src/`; `docs/` and `Cargo.toml` are at the repository root. Paths marked (fleet) are in the fleet repository.

**The push (0.6.6).**
- `infra.state` goes out once after the register ack, then when the infra module signals a change and the body differs from the last one sent on that connection, at most once a second (`tunnel/infra_state.rs:1-12`, `:55`, `:195-243`): "A latency reading or a counter alone is not a change."
- The message carries `ts`, `config_version`, the targets and `truncated` (`tunnel/infra_state.rs:57-102`), with no sequence and no boot id. Its `ts` is when it was built (`:63-64`, `:215`), not when a check ran. A message still over 16 KiB after the cap is logged and not sent (`:218-222`). The event stream's documented dedup key, `(relay_epoch, connection_id, state.ts, state.config_version)`, rests on that clock (`docs/http-api.md:1246-1250`).
- The forwarder is one task per connection, started only when the relay's ack advertises `infra.state` and aborted when the connection ends (`tunnel/client.rs:1245-1260`, `:1408-1414`, `:1712-1722`). Nothing restarts it while the connection stays up.
- The relay keeps the message whole and checks only its size and its connection (`tunnel/relay.rs:133-141`, `:1456-1486`). A new field needs no relay change.
- The signal is a `watch` value that counts `changed()` calls since startup, and no route or message carries the count. `seed_results` signals every time, the same config included (`infra/mod.rs:315-317`, `:350-361`, `:453`).

**What the push replaced.**
- A pulled `GET /api/infra/results` carries `ts`, which every check rewrites (`infra/monitor.rs:181`). netage-server warns when it is older than 180 s (`netage-server/src/fleet/health_poller.rs:353-366` (fleet)).
- Once one `infra.state` has arrived on a connection, netage-server stops pulling and never checks the age of what it holds (`netage-server/src/fleet/health_poller.rs:306-310`, `netage-server/src/fleet/net_state.rs:390-404` (fleet)).
- The fleet's Infra page calls a row stale when its `last_check` is older than twice the target's interval, never under 3 min (`src/lib/modules/devices/components/InfraDeviceTable.svelte:249-256` (fleet)).
- `docs/http-api.md` (line 775) tells a collector on the event stream that it "need not poll" the results route. The push carried the state and dropped the proof.

**The monitor** (`infra/monitor.rs`).
- It ticks every 5 s and runs at most 3 checks at once, each under a 90 s deadline that starts when the check takes its permit (`:27-29`, `:49-50`, `:71-86`).
- A tick waits for every check it started (`:91-92`), so one slow check holds back the next tick. The tick discards a panicked task's error; the panic hook still prints it and rewrites `last_panic.log` (`main.rs:496-518`). A panic in a check leaves its target alone; one in a recovery comes after the new state is written and before it is signalled.
- Due times live in a map inside the task (`:44-45`), so a respawned monitor checks every target at once. They come from the wall clock (`:54`, `:59-63`; `infra/mod.rs:544-550`): if the clock steps back, no target is due until it catches up, while the ticks go on.
- A result is applied under the Infra mutex (`:87-88`). `apply_result` writes the target's state (`:166-182`), then fires any recovery (`:202-209`), and only then signals a status change (`:217-228`).

**The state machine** (`infra/monitor.rs:232-281`).
- `consecutive_ok` counts every good answer, slow ones included (`:148-154`).
- Up becomes Degraded on a failure or on a good answer above `degraded_threshold_ms` (`:251-263`). Degraded and Down return to Up after `up_after` good answers in a row, whatever their latency (`:264-279`). Unknown becomes Up on any good answer (`:244-250`).
- With the defaults (200 ms, `down_after` 3, `up_after` 2; `infra/mod.rs:79-87`), a target that answers steadily above its threshold flips Up, Degraded, Up on every check, and each flip is pushed.

**The recovery.**
- It fires whenever a check leaves the target Down, within `cooldown_secs` (300) and `max_retries` (2) (`infra/monitor.rs:202-209`, `:306-336`; `infra/mod.rs:180-185`). That includes a good answer that leaves it Down, the first of `up_after` (`:273-279`): a device coming back is restarted again once the cooldown has passed. The docs say it fires when a target "transitions to DOWN" (`infra/mod.rs:13-14`) or "enters DOWN" (`:160`).
- The count resets on Down to Up (`infra/monitor.rs:211-215`) and is cleared when the config is removed (`infra/routes.rs:342`); a target that a push removes keeps its entry. The tracker is written after the command returns and lives in memory only (`infra/monitor.rs:378-380`; `infra/mod.rs:298-299`, `:338`), so an agent restart loses it.
- It runs inside `apply_result`, so under the Infra mutex: `sh -c`, the credentials as environment, a 300 s limit (`infra/monitor.rs:340-359`). While it runs, everything that takes the mutex waits: the forwarder (`tunnel/infra_state.rs:204`, `:214`), every other target's result, and the config, results, check, history, credentials and discovery-progress routes (`infra/routes.rs:44`, `:91`, `:109`, `:147`, `:180`, `:218`, `:260`, `:273`, `:297`, `:330`). The Down is signalled only after the recovery returns (`infra/monitor.rs:206`, `:227`).
- The output is cut with `&stdout[..512]` (`infra/monitor.rs:361-366`), and `stdout` comes from `String::from_utf8_lossy` (`infra/checks.rs:599`). When byte 512 falls inside a character, the slice panics before the log entry, the cooldown, the count (`infra/monitor.rs:370-380`) and the Down's signal (`:227`). The tick discards it (`:92`), the tokio mutex does not poison, and the monitor carries on. The next Down check finds the tracker as it was before the run (no entry for a first run) and no status change, so the command runs and panics again, at every Down check, with no cooldown, no limit and no Down pushed. The binary unwinds by choice (`Cargo.toml:49-57`). `at.rs` already fixed the same panic (`at.rs:145-149`, `:166-176`).
- stderr is dropped. stdout is kept, masked: each password of at least 4 bytes is replaced, and usernames are not (`infra/monitor.rs:299-304`, `:349-358`).
- `exec_with_env` spawns with `kill_on_drop(true)` (`infra/checks.rs:555`), and a push aborts the running monitor (`infra/routes.rs:46-49`). Today the recovery's hold on the lock keeps a push from reaching it; out of the lock, the next push would kill it halfway. The agent's own stop kills it too: its tasks drop, and a systemd stop kills the service's whole group (`server/files/sctl.service` sets no `KillMode`).
- The recovery log (a ring of 50) is left out of `infra.state` (`infra/mod.rs:265-266`, `:320`; `tunnel/infra_state.rs:10-11`), so a collector that has stopped pulling never sees it.

**The config push** (`infra/routes.rs:24-76`).
- It takes the mutex, aborts the monitor, drops the targets that left, seeds the results (a signal), saves the config without checking the save (`:63`), and spawns a new monitor with `monitor::spawn_monitor` directly (`:44-67`). It answers `{status, config_version, target_count}` (`:71-75`). It takes any version, 0 included.
- It never compares the push with the config the unit holds. An identical push restarts the monitor, which re-checks every target at once, and sends one message, since latencies, counters and `last_check` have moved since the last one.
- The config types derive no `PartialEq` (`infra/mod.rs:38`, `:56`, `:90`, `:161`). `InfraConfig.version` is documented as "Monotonically increasing" (`infra/mod.rs:40-42`); the fleet sends a fingerprint of the config, never 0 (`src/lib/modules/devices/domain/infra.reconcile.ts:118-128` (fleet)).

**Supervision.**
- At boot the monitor runs under `spawn_supervised` and `supervise_spawned`, from the boot config, and not at all in safe mode (`main.rs:1191-1210`). A panic there is rebuilt after 5 s (`main.rs:549-601`).
- Both helpers are private to the binary, and `infra` is a library module (`lib.rs:26`), so `infra::routes` cannot call them. A monitor started by a push has no supervisor: a panic in its loop ends it for good. Nothing checks whether `monitor_handle` has finished (`infra/routes.rs:47`, `:67`, `:333`).
- Safe mode is decided once, at start, and nothing else holds it (`main.rs:631-632`). The push has no safe-mode check (`infra/routes.rs:24-67`), so it starts the monitor that `docs/safe-mode.md` (lines 36-43) says safe mode skips. `/api/info`'s `safe_mode.active` reads the flag file per request (`routes/info.rs:133-145`), so it reads false once the flag is deleted, while no monitor runs (`docs/safe-mode.md:104-106`).

**Check now** (`infra/routes.rs:95-169`). It copies the spec under the mutex, releases it, and runs the check without the 90 s deadline, even while the monitor checks the same target (`:109-142`). Afterwards it only keeps or drops the login session (`:144-156`). It writes no `last_check`, latency, detail, counter or status, and it signals nothing.

**Where the fleet already polls.**
- netage-server polls each unit every 30 s by default: `GET /api/health`, then, when that answers, `GET /api/info` with no groups (`netage-server/src/fleet/health_poller.rs:214`, `:245`; `netage-server/src/config.rs:219-221`, `:380` (fleet)).
- `/api/info` needs the API key (`main.rs:847-848`, `:945`). It answers the groups asked for, all by default: `core`, `interfaces`, `disk`, `tunnel`, `gps`, `lte` (`routes/info.rs:37-93`), with the core fields at the top level (`:146-159`). It ignores a group it does not know. Nothing in it is about Infra, and it never takes the Infra mutex.
- ADR-007 (decision 4) already named `/api/info` as the carrier for a value the fleet reads on every poll. ADR-005 added an optional field and kept `net.state` at v1.
- `net.state` carries `boot`, when the agent started in unix ms, and `seq`, +1 for every distinct state since `boot`, from 1 (`netwatch/mod.rs:129-132`, `:269-276`; the stamp, `main.rs:768-771`). netage-server deduplicates `net.state` on (connection, boot, seq) (`netage-server/src/fleet/net_state.rs:587-605` (fleet)).

## 3. Decision

1. **Push on change stays.** `infra.state` is still sent only when something changes. A latency reading or a counter alone is still not a change; a logged recovery run now is one (decision 9).

2. **The agent publishes a digest of its monitor where the fleet already polls.** `GET /api/info` gains the group `infra`. It is part of `all`, so netage-server's call with no groups gets it, and `?groups=infra` answers it alone. Every agent of this release answers it, a unit that holds no config included.

   ```json
   "infra": {"rounds": 17280, "seq": 42, "hash": "3f9a0c1d2e4b5a69"}
   ```

   - `rounds`: monitor rounds since the agent started (decision 5). It never goes back while the agent runs.
   - `seq`: 1 when the agent starts, +1 at every change it pushes: a target's status, a config applied or removed, a recovery run logged. A push or a removal that changes nothing is not a change.
   - `hash`: the first 16 hex characters, lowercase, of the SHA-256 of this text: the agent's `boot` (decision 3) in decimal, a newline, the config version in decimal (0 for none), a newline, then per target in byte order of its id: the id, a space, its status (`unknown`, `up`, `degraded` or `down`), a newline. 64 bits tell pictures apart, at 48 bytes less per poll than the whole hash. While the agent runs, `hash` changes only when `seq` moves, and it can return to an earlier value. TRD-9 pins the bytes with a test vector shared with the fleet's tests, like the config fixture (`infra/mod.rs:556-560`). A release that hashes other text names the field anew.
   - `off`: present only when no monitor is expected: `safe_mode` in an agent that started in safe mode, else `no_config` when the unit holds none. When it is absent, a monitor is expected and `rounds` must move.

   The digest lives in a small cell of its own that every writer of the results updates. `GET /api/info` reads that cell and never waits for the Infra mutex, so a lock held for good shows as a stalled `rounds`, never as a slow answer. A steady unit's group is about 60 bytes, and no new message.

3. **Reports name the picture they carry.** Each `infra.state` gains `boot` (when the agent started, unix ms, the value `net.state` carries), `seq`, `hash` and `recoveries` (recovery runs logged since `boot`), as they stood when it was built. A message goes out whenever `seq` has moved since the last one sent, still at most one a second. `GET /api/infra/results` gains the same four, so a pull settles a comparison. The message stays v1: the fields are additive, and the relay stores it whole. Its dedup key on the event stream becomes `(relay_epoch, connection_id, state.boot, state.seq)`, as for `net.state`.

4. **What the readings mean.** The agent promises these meanings; netage-server's reaction (a pull on a miss, a "monitoring stopped" event) is fleet ADR-0041's (TRD-38). The fleet's copy is its last report or pull, hashed by the text above.
   - The same `rounds` at two polls (30 s apart by default), with no `off`: the monitor completed no round in that time. It is not checking.
   - A `hash` that differs from the copy's: the copy does not show the unit's picture. A change is pushed within about a second, so a difference that outlasts that is a report lost or never sent (over 16 KiB, or a forwarder that died), a copy from before a restart or of another config, or a wrong fold (a truncated report).
   - The same `hash` and a `seq` above the copy's: changes the copy has not seen that left the statuses as they were (a flap, a recovery run). Past about a second, a report was missed.
   - A matching `hash` also proves the copy's config version, so a push of the same config can be skipped.
   - `recoveries` moved in a report: a recovery ran. Its log entry is read once from `GET /api/infra/results`.
   - A new `boot` in a report: the agent restarted, and `rounds` and `seq` began again.
   - No `infra` group: an agent from before this decision. Its silence proves nothing.

5. **The monitor keeps the promise.**
   - The tick no longer waits for its checks. It starts every due target that has no check in flight and skips one that has, so a target is never in flight twice. At most 3 checks run at once, and permits are served in order.
   - A round is counted at the end of a tick, and only while every running check is within 90 s plus one tick of taking its permit. A monitor that is gone, blocked, stuck on a check or panicking in its tick moves no `rounds`; one that checks moves it every 5 s.
   - Due times and cooldowns use the monotonic clock. A step of the wall clock cannot stop the checks.
   - A check that panics is logged, and its target becomes `unknown` with a detail saying the agent failed to check it. It never fires a recovery.

6. **Every monitor is supervised.** `spawn_supervised` and `supervise_spawned` move from the binary into the library. A monitor started by a push runs under the same supervisor as the one started at boot, and is rebuilt 5 s after a panic, from the config the unit then holds. In an agent that started in safe mode, a push stores the config and starts nothing; `docs/safe-mode.md` gains that line.

7. **An identical push changes nothing.** A push equal to the config the unit holds, every field and the version included, keeps the running monitor. Nothing is aborted, seeded or signalled, and the config is written again only if its last save failed. Outside safe mode, if no monitor runs for that config, the push starts one, supervised. The answer gains two booleans, `unchanged` and `started`. A push with version 0 is refused (400): 0 means the unit holds no config. The version is documented as the caller's name for a config, not as monotonically increasing.

8. **Degraded stays Degraded.** A good answer above `degraded_threshold_ms` is slow, and a slow answer never makes a target Up. The agent counts good answers within the threshold in a row, beside `consecutive_ok` and `consecutive_fail`; that count stays inside the agent.
   - Up: a failure or a slow answer makes it Degraded, as today.
   - Degraded: `up_after` answers within the threshold in a row make it Up. `down_after` failures in a row make it Down, as today.
   - Down: `up_after` good answers in a row make it Up if all were within the threshold, Degraded if not.
   - Unknown: a good answer within the threshold makes it Up. A slow answer or a failure makes it Degraded.

9. **A recovery never holds the lock, never holds back the Down, and never meets a device coming back.**
   - It fires only on a failed monitor check that leaves the target Down, within `cooldown_secs` and `max_retries`: the first Down and every failed check after it, never a good answer. The docs that say it fires on the transition are corrected.
   - The Down is recorded and signalled first. The recovery then starts in a task of its own, which takes its command and the credentials it names under the lock and releases it. Nothing waits for the run.
   - One run per target at a time. Before the command starts, the run is recorded: the cooldown begins, the count goes up, and the record, marked as not ended, is written to `state_dir` and synced. A restart, one the recovery caused included, keeps the count, and after a start a target with a saved count waits a full cooldown.
   - The count clears when a good answer moves the target out of Down or `unknown`, so a target that answers after a restart starts afresh. A removed config clears every count, and a target that a push removes loses its own.
   - A run ends by itself or at its 300 s limit. A push, a rebuilt monitor or a removed config does not end it: none of them may cut a power cycle between off and on. A managed upgrade waits for a running recovery, at most that limit, before it hands off to its helper (`upgrade/stage.rs:783-792`). Any other stop (a person's restart, a crash, a power loss, or the upgrade's restart for a run begun after the handoff) can still end a run with the agent. The next start logs each run still marked as not ended, its outcome unknown.
   - Its stdout and stderr are masked, cut to 512 bytes each on a character boundary as `at.rs` cuts, then logged. stderr is kept because a failed command says why there; it gets stdout's mask and its limits (a password under 4 bytes and a username stay as they are).
   - A logged run moves `seq` and `recoveries` and is pushed, so a collector that has stopped pulling learns of every run, one logged at start included.

10. **Check now is a reading.** `POST /api/infra/check/{target_id}` runs under the same 90 s deadline as the monitor's checks, and its result is applied as a monitor check is: `last_check`, latency, detail, the counters and the status. A `last_check` that moved while the status stayed would say a target was seen up when the reading failed. A status change is signalled and pushed like any other. Check now counts no round, never starts a recovery, and answers as today.
    - A target has at most one check in flight, the monitor's or a person's. Check now on a target whose check is in flight waits for that check, at most its deadline, and answers its result.
    - A result is applied only while the unit still holds the config the check ran under and the target is in it; otherwise it is answered and nothing is applied. The same holds for a monitor check whose monitor was replaced.

## 4. Consequences

- A steady unit sends nothing new and still proves, on every 30 s poll, that it checks. A stopped monitor shows within a minute: two polls 30 s apart read the same `rounds`. A lost report or a wrong copy shows at the next poll, and a miss costs one pull (fleet ADR-0041).
- The group costs about 0.17 MB a day per unit at the 30 s poll. The answer it rides is larger; TRD-9 measures both.
- The relay needs no change, so no relay upgrade waits on this (ADR-003).
- `infra.state` stays v1, `/api/info` keeps its groups, and the results and the config answer only gain fields. Older readers ignore what they do not know.
- An agent before 0.6.12 has no `infra` group. The fleet keeps a bounded pull for it (fleet ADR-0041, TRD-38).
- Order: the within-limit rule and the identical-push rule make units quieter, and under the fleet's old freshness rule a quieter unit greys out sooner. So 0.6.12 reaches a unit only once the fleet reads the digest (fleet ADR-0041).
- A push of the same config no longer brings a fresh message. A collector that wants a fresh reading asks Check now.
- One slow target no longer holds back the other targets' checks.
- The Down leaves the unit within about a second, even when a recovery follows, and the other targets and the routes no longer freeze during a run. A device coming back is not restarted on its first good answer. A recovery that cuts the unit's own power stops at `max_retries` across the restarts it causes, and starts afresh once the target answers. A managed upgrade can wait up to 300 s for a run.
- A person's Check now moves the record, and its failures count toward Down like the monitor's. A few quick clicks on a failing target bring the Down sooner, and with it the recovery on the monitor's next failed check.
- One release, 0.6.12, carries this with ADR-011 and ADR-012. What a unit downloads depends on its target (TRD-9 section 5): about 3.9 MB of gzipped payloads for a `mips_24kc` unit (the XE300, the WE826), plus 1.3 MB of loader libraries on a WE826 when they change, and about 7.4 MB of raw binaries for a `riscv64` unit. The other targets are measured with the release.
- `docs/http-api.md` (`GET /api/info`, the config, results and check routes, `infra.state` and its dedup key), `docs/safe-mode.md`, `docs/upgrade.md` and the `infra` module docs change with the code. `docs/adr/README.md` gains this row and notes that 008 and 009 are reserved for fleet TRD-28 and TRD-29.

## 5. Rejected alternatives

- **A periodic push** (`infra.state` on a timer, or a heartbeat frame of its own): it reports the same thing over and over, on every unit. A new frame type also needs relay code, and the relay upgrades last (ADR-003). If a unit's poll is ever stretched on a metered link, a pulse that carries this digest is the fallback, decided then (fleet ADR-0041).
- **Pulling `GET /api/infra/results` on every poll again:** it brings back the load and the lag the push removed (fleet TRD-28, appendix A). With the digest, a pull happens only on a miss.
- **The digest in `GET /api/health`:** that route needs no key (`main.rs:845`), and Infra state stays behind the key.
- **`boot`, the config version and a recovery count in every answer:** each repeats on every poll what the reports carry. The hash covers `boot` and the version, and a logged run is a change that is pushed.
- **A time as the proof** (the newest `last_check`, a pushed `ts`, or the current round's start and the checks it waits on): it reads the unit's wall clock, which can step, against another machine's, and a round that waits for its slowest check has no fixed length. With the tick no longer waiting, a count shows a stall within a minute.
- **The tunnel ping as the proof** (every 5 s, `tunnel/client.rs:43-44`; `config.rs:624-626`): it proves the tunnel task, not the monitor.
- **One signal instead of three:** a dead monitor's hash does not move, and neither does a steady site's. A counter alone cannot see a lost or misread report.
- **Signalling the Down first, with the recovery still under the lock:** the forwarder needs the lock to build its message (`tunnel/infra_state.rs:213-216`). The Down would still wait, and every other target and route would still freeze for up to 300 s.
- **Detaching a recovery from the agent**, as the upgrade detaches its helper: the run would outlive the agent's limit, mask and log.
- **Counting a check's panic as the target's failure:** the agent's own defect would count against the target and could fire its recovery.
- **Firing a recovery only on the transition to Down,** as the docs said: a target still down after the cooldown would get no second attempt. `cooldown_secs` and `max_retries` exist for that.

## 6. Settled questions

Each question this ADR left open, with its answer (2026-10-21).

1. **The digest's fields and size.** Settled: `rounds`, `seq`, `hash` and `off`, about 60 bytes on a steady unit, in the `infra` group of `GET /api/info` (decision 2). A stall is `rounds` unchanged over two polls, with no `off`. Fleet ADR-0041 reads this digest as it stands here, so this ADR needs no change for it.
2. **A recovery's stderr.** Settled: it is kept, with stdout's mask and its 512-byte cut (decision 9).
3. **A managed upgrade while a recovery runs.** Settled: the upgrade waits for the run, at most its 300 s limit, before it hands off to its helper (decision 9).
4. **Check now and Down.** Settled: a failed Check now counts toward Down, like a failed monitor check (decision 10).
5. **What the release costs a unit.** Settled: each target's own download, as TRD-9 section 5 measures it, not one number for every unit.
