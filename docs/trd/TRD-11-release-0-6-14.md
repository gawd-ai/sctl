# TRD-11: Release 0.6.14

Carries [ADR-014](../adr/014-the-agent-restarts-itself-through-one-authenticated-route.md) as [TRD-10](TRD-10-the-agents-own-restart.md) builds it, and two riders owed since [TRD-9](TRD-9-release-0-6-12.md): the reads its bench found outside the file gate of [ADR-011](../adr/011-file-reads-read-what-is-there-and-refuse-secrets-and-devices.md), and sctlin's pass-through of `{{agent}}`. Status: Planned. Release: sctl 0.6.14. Nothing here is tagged, pushed, released or sent to a unit.

## 1. Objective

One release, after which:

- a caller restarts a unit's agent by one route, on every layout, and the agent refuses a restart that would strand it (ADR-014, TRD-10);
- no route of the agent opens a FIFO, a socket or a device where it expects one of its own files: the playbooks, the watchdog's history and the safe-mode flag are read through the file gate's handle, and a flood of refused reads no longer empties the activity ring (ADR-011);
- a playbook that names the agent's tools by `{{agent}}` runs from sctlin;
- every unit has paid for one agent set, measured per target, and a ramboot unit pays the difference at each boot from then on.

No relay change is needed. A relay takes the release last, through the managed path (ADR-003), and refuses the restart route for itself.

## 2. What main has

Lines and counts are at `d695344`, 26 commits past `v0.6.13`.

- **Versions.** The five carriers read 0.6.13: `server/Cargo.toml:3`, `mcp/Cargo.toml:3`, `crates/sctl-comms-abi/Cargo.toml:3`, `drivers/sctl-comms-quectel/Cargo.toml:3` and `web/package.json:3`. The tag must equal `server/Cargo.toml`'s version (`docs/releasing.md`). The build number is the commit count, so the release reads `0.6.14.<n>` with `n` the count at the tag (335 at `d695344`).
- **The changelog.** `CHANGELOG.md` `[Unreleased]` holds four groups, each written with its change: Restart, File reads, sctlin, devices. They move into `[0.6.14]` as they stand. The file is CRLF.
- **0.6.13's set**, the one every unit runs and so what the new set is measured against (TRD-9 slice 10, from its `release.json`): `x86_64` 8,290,544 bytes; `aarch64` 6,774,424; `armv7` 6,537,744; `riscv64` the server 7,426,264 and the plugin 356,744 (7,783,008); `mips_24kc` the server 3,849,030 and the plugin 226,990 gzipped (4,076,020); `mipsel_24kc` the server 3,818,755 and the plugin 227,005 gzipped (4,045,760); the WE826's `libc` and `libgcc`, 1,257,347 gzipped.
- **Payloads cannot be built here.** `rundev.sh` builds a device target only inside the commands that reach a unit, and the `Release` workflow runs on a `v*` tag alone (`.github/workflows/release.yml:9-11`). So the set's sizes exist only once the tag does, as for 0.6.12 and 0.6.13.
- **CI.** As TRD-9 section 2 lists it, with one job since 0.6.13: `test-musl` runs the file gate's tests on `x86_64-unknown-linux-musl`, the libc every release target uses. 0.6.12's gate passed on glibc and opened what it judged on every release binary; that job exists so that cannot happen twice.

## 3. The version

0.6.14, a patch.

The repository keeps Semantic Versioning (`CHANGELOG.md:5-6`) below 1.0, and its own practice since 0.6.0 is the rule: every release has been a 0.6.x, those that added a route or a key included (0.6.7 added the three upgrade routes, 0.6.12 the `infra` group and `cli`). The second number moved when a consumer had to change with it: 0.6.0 itself moved the relay to forwarding any route, and a device payload from before it answers no proxied request under a 0.6.0 relay (`DEVICE_PAYLOAD_OUTDATED`, `docs/releasing.md`).

This release asks nothing of a consumer. It adds one route and three keys (`instance` in health, `restart` in `GET /api/upgrade` and `upgrade.state`, `restart_tmp_kb` in `install.json`), each additive; the relay is unchanged; an agent of this release and one of 0.6.13 are told apart by the presence of `restart`, never by a version. What changes for a caller that did nothing:

- `GET /api/playbooks/{name}`, `PUT /api/playbooks/{name}` and `GET /api/lte/watchdog/history` answer 400 `NOT_A_REGULAR_FILE` where a FIFO or a device sits at the file's name, where they waited before;
- `GET /api/safe_mode/flag` answers `flag: null` for a flag it cannot read, where it answered 500;
- `POST /api/upgrade` can answer 409 `RESTART_IN_FLIGHT`, which means busy.

None of those is a contract a caller could have relied on.

## 4. What it carries

### 4.1 TRD-10, slices 1 to 7

Built and reviewed on `main`: the record and `instance` (`61265d3`), the start's judgment (`13ba2c4`), the gate on recovery runs and what a restart ends (`5007648`), the helper (`2bc444d`), the route (`3d8176f`), the `gz-tmp` inits and each device's threshold (`d071965`), a real agent and helper on loopback (`effcfaf`), and the review's seven fixes (`7220473`, `548c69b`, `8c6639c`, `84d2273`, `5849258`, `af4f57f`, `464aacf`). TRD-10 section 7 has what each ran. Its slice 8, the bench, is this TRD's slice 7.

### 4.2 Rider A: the agent's own files are read through the file gate's handle

TRD-9 slice 10's bench of the released 0.6.13 found the failure the gate fixed still open on routes the gate does not cover. Each needed exec or local access to set up, and each is closed by the mechanism ADR-011 decided (decisions 2 and 6): a handle that opens nothing (`O_PATH`), the type read from it, and only a regular file opened through it, without waiting.

These files are the agent's own, in the agent's own directories, which the gate's path rules refuse to a caller's read. So the routes do not go through `Gate::open`; they go through `file_access::open_own`, `read_own` and `writable_own`, which are the gate's handle and its type judgment without its path rules.

| Found | Fix | Commit |
|---|---|---|
| A FIFO named `*.md` in `playbooks_dir` held `GET /api/playbooks` and `GET /api/playbooks/{name}`; 32 such reads took every handler the tunnel runs, and a SIGTERM then left the agent alive | Both read through `read_own`, at most 1 MB, on the blocking pool. The list skips an entry that is not a regular file; the read of one answers 400 `NOT_A_REGULAR_FILE` | `3e558e3` |
| A FIFO at `<data_dir>/watchdog_history.jsonl` held `GET /api/lte/watchdog/history` on a runtime worker; four stopped the agent | Read through `read_own` on the blocking pool, capped at the size the file rotates at; 400 `NOT_A_REGULAR_FILE` | `3e558e3` |
| `PUT /api/playbooks/{name}` onto a FIFO with no reader waited in its open | What is at the name is judged first (`writable_own`); then the document is written beside the name and renamed over it, so the name itself is never opened | `3e558e3` |
| About 300 refused reads evicted the activity ring's 200 entries, exec history included | Refused reads hold at most a quarter of the ring: past that share a new refusal takes the oldest refusal's place | `b46c719` |
| Not on TRD-9's list, found with it: `/api/info` read `safe_mode.flag` plainly on every poll, on a runtime worker, and so did `GET /api/safe_mode/flag` | One function reads the flag's content through `read_own`. Only an absent flag reads as not active: anything at the name that cannot be read as the flag answers `active: true`, `flag: null` and a `reason` that says what is there, on both routes (section 9, ruling 6) | `871e19e`, then the ruling's change |

**Tests.** Each makes its own FIFO in a scratch directory, and a route that waited is given the FIFO's other end and failed within 5 s, so no run hangs. Against the plain reads and the plain write, the four route tests fail that way. 1 in `file_access` (40 reads of a FIFO, a link to one, `/dev/null`, a write's judgment), 3 in `routes/playbooks.rs`, 1 in `routes/lte.rs`, 1 in `routes/safe_mode.rs`, 2 in `activity.rs`. CI's `test-musl` job now runs the three routes' tests and the restart's with the gate's, on the libc the units run. That filter, run here on `x86_64-unknown-linux-musl`: 108 passed. It ran on the build machine's stable toolchain (1.98.1), which has the musl target; the pinned 1.97.1 has none installed here, as TRD-9 slice 10 records, and CI installs it.

**The watchdog's own append** (section 9, ruling 4; built 2026-11-23). It opened its history for appending (`util.rs`, `append_rotating`), and a FIFO at the name held one thread of the blocking pool per watchdog event. The name is judged with `writable_own` before the open: a FIFO or a device there is one error to the watchdog, which logs it, and no thread. 1 new test, with its own FIFO, released and failed within 5 s if an append waits.

### 4.3 Rider B: sctlin leaves `{{agent}}` as written

`renderPlaybookScript` refused any `{{word}}` left after the declared parameters, `{{agent}}` among them, so a playbook that names the agent's tools that way could not be run from sctlin (TRD-9 section 4 kept the pass-through out of 0.6.12, and 0.6.13 changed nothing in `web/` but its version). `{{agent}}` is not a parameter: it stands for the path that runs the agent's tools on the unit (`GET /api/health`'s `cli`), and the server that sends the script fills it.

`d695344` left `{{agent}}` as written everywhere. The review found that it then reached the unit's shell unfilled wherever no server fills it (sctlin used by itself, and "Run in terminal" even behind a server that fills jobs and execs), where 0.6.13 refused it before anything was sent, and that a playbook declaring its own `agent` parameter lost its field. **Ruled (2026-11-23): the pass-through is the host's choice, off by default.** A host whose server fills the reference names it and the paths it fills it on with one prop, `serverReferences={{ names: ['agent'], paths: ['job', 'exec'] }}`, on `PlaybookExecutor`, `PlaybookPanel` or `PlaybookWidget` (`web/README.md`). Only on a named path is the reference left as written and its form field dropped. With no option, and on any path not named, sctlin behaves as 0.6.13. A script sent to a terminal session is rendered with no reference filled, and is refused when one is left; in 0.6.13 that button sent the unrendered script when rendering failed, and it now sends nothing. 4 new tests in `playbook-parser.test.ts`, 84 in sctlin; `npm run check` has no error. `playbook-parser.ts` and its test are CRLF and were edited byte for byte.

The MCP's renderer is left refusing `{{agent}}`: nothing stands between the MCP and the unit to fill it (section 9, question 5).

### 4.4 The upgrade's helper: the health read is bounded, the command's deadline is open

TRD-10's review bounded every step of the restart's helper and left the upgrade's alone. As it stands (`upgrade/apply.rs`): `Shell::run` waits for the restart command with no deadline, and `http_get` bounds each read of health, not the whole exchange. A restart command that hangs, or a process that takes the health connection and never ends its answer, leaves the upgrade's helper holding its lock, and every later upgrade and restart answers in flight until a reboot.

**Ruled (section 9, ruling 1): the read's deadline in this release; the command's once the bench has timed it.** As recommended: The upgrade's helper gets the restart helper's two step bounds, the health read's total deadline (5 s) and the command's deadline, and no hard limit of its own: between its swap and its rollback it must never be ended from outside.

- The read's deadline carries no risk: a health answer that takes over 5 s on loopback is not a healthy agent.
- The command's deadline is the one a bench has to earn. A command killed between its `stop` and its `start` leaves no agent; the upgrade then waits its window and rolls back, which runs the command again, so a kill is recovered, but only if the deadline is not the cause. What the bench must show, on each layout and under load: how long the restart command takes at its slowest (a `gz-tmp` init expands about 9 MB from flash inside it), and that the deadline is at least three times that. TRD-10's bench times the same command on every layout for the restart's helper, so the number costs nothing more.
- If the bench does not run before the tag, only the read's deadline ships, and the command's waits for the release after.

To leave it instead: nothing has to be proven, and the weakness stays as TRD-10 section 4 records it.

### 4.5 An installed unit keeps the record and the init it has

A managed upgrade places the files `install.json` names and runs no install script. So after 0.6.14 rolls out:

- **The init.** A `gz-tmp` unit keeps an init whose `restart` stops before it checks. The route's own checks and the helper's second run are what protect it; the new init reaches a unit only when `install.sh` runs there again.
- **The XE300's record** has no `restart_tmp_kb`. The agent takes 16384, which is what that init demands. Nothing to do.
- **A RUT241 installed as `gz-tmp`** has no `restart_tmp_kb` either, and the agent takes 16384 where its init demands 8192. With free `/tmp` between the two (each plus the size of the running binary), the route refuses `no_space` a restart the init would have made. A RUT241 moved to `ramboot` is not concerned: its threshold is 0.

How such a record is rewritten without a reinstall, proposed and not built:

1. **The agent reads its init's own threshold.** Where a `gz-tmp` record gives no `restart_tmp_kb`, the agent reads the line `MIN_TMP_KB=<digits>` from `/etc/init.d/sctl`, as text, through `read_own`, and takes that; 16384 only when there is no such line. No record is rewritten, every installed unit is right at its next start, and the number is the init's by construction. It is one line of a shell script read by its shape; a line the agent cannot read as digits leaves the default. **Ruled in** (section 9, ruling 2), as a slice of this release.
2. **An operator rewrites one unit's record** with `PUT /api/files` to `/etc/sctl/install.json` (writes there keep their rules; the record is read at start, so it counts from the next start). It works today on any agent with the route, one unit at a time, and a wrong byte leaves a unit with no install record until it is put right. For a unit that matters before option 1 ships.
3. **The upgrade's helper patches the record** as it swaps files. It reaches every unit, and it gives the helper a write it never had, to a file the next upgrade depends on. Not recommended.

## 5. The size per target

A unit fetches the whole set once, never a difference: what the rollout costs it is the set's size, section 2's figures plus what this release adds. The difference against 0.6.13 is what a `ramboot` unit pays more at every boot from then on, since it fetches its set again each time, over whatever path is up.

No number is claimed before the tag (section 2). What is known: no dependency is added or moved (`Cargo.lock` is unchanged but for the version at the release), and the code added is the restart's modules and three small readers. Slice 6 measures every target from the tag's `release.json` against the figures of section 2, and records, for each: the server, the plugin, the set, and the difference in bytes and in percent. On a WE826, whether `libc` and `libgcc` keep their hashes, since 0.6.13 was the first release to gzip them with `-n`: if they do, that unit fetches 1,257,347 bytes less than it did for 0.6.13.

## 6. The bench

Two benches, both before any unit in service takes the release.

**6.1 TRD-10's checklist, on one unit of each layout** (TRD-10 3.8, with the owner's go for each unit): `usr-bin`, `gz-tmp` (the XE300 and a RUT241), `ramboot` under procd, `ramboot` with no procd and a loader, and `systemd` for an agent that is not a relay if one exists. It needs the release's payloads, so it runs between the tag and the rollout: the release exists, and no ring has started. It also times the restart command on each layout, for section 4.4.

**6.2 The release bench, in the form of TRD-9 slice 10's.** The released `x86_64` binary, the musl one a relay runs, on a loopback bench: a bench relay and a bench agent, no root, staged as each layout in turn with `SCTL_INSTALL_JSON`, `SCTL_CLI_PATH` and `SCTL_DISABLED_PATH`. 0.6.12 passed its tests and failed on its released binary; the bench reads the binary that ships.

- 40 FIFOs named `*.md` in the playbooks directory: `GET /api/playbooks` and each `GET /api/playbooks/{name}`, 40 at once, through the relay and directly, each answered at once; relayed health, exec and a session start keep answering meanwhile. The same with 200 at once.
- A FIFO at the watchdog's history and one at the safe-mode flag: 8 reads of each, and `GET /api/info` 8 times, each at once.
- `PUT /api/playbooks/{name}` onto a FIFO: refused, the FIFO left.
- Under `strace`: every open of those paths is `O_PATH`, and only a regular file is reopened through `/proc/self/fd/<n>`.
- 300 refused reads after 200 execs: 150 exec entries are still in `GET /api/activity`.
- A SIGTERM after the 40 FIFO reads ends the agent promptly.
- A restart by the route, with a restart command that stops and starts the bench agent: the 202 arrives through the relay before the connection drops, and on the event stream `device.disconnected`, `device.connected`, a `net.state` with a new `boot` and an `upgrade.state` whose `restart` reads `ok` with that `boot` arrive in that order. A relay's own agent answers `relay`.
- The bytes of that reconnect, counted on the link in a network namespace, as `relay_route_netns` is run.
- Each refusal by its code through the relay; a helper killed after its start settles as `not_restarted`, helper lost, and the next request is taken; `sctl upgrade` from a shell is refused while a restart's helper holds its lock.
- From sctlin against that bench: a playbook holding `{{agent}}` reaches the unit with `{{agent}}` in it, and its form shows no field for it.
- Two reviewers on benches of their own, as for 0.6.13, told only what the release claims.

## 7. Rollout notes for an operator

- **Order.** Units before relays (`docs/releasing.md`), the relay last through the managed path. A unit of 0.6.14 behind a relay of 0.6.13 holds: the route is forwarded like any other, and the relay keeps `upgrade.state` whole.
- **Under the caller's rollout rules, never on LTE.** The set is fetched once per unit, at the sizes slice 6 records.
- **No prerequisite on the caller's side.** A caller asks the restart route only of an agent whose `GET /api/upgrade` or `upgrade.state` carries `restart`. Until it reads that key, nothing changes for it.
- **A caller that schedules upgrades** reads 409 `RESTART_IN_FLIGHT` on `POST /api/upgrade` as busy, and asks again once the restart's record has ended.
- **A second run is a defect to chase.** A record whose `attempts` is 2 says a `gz-tmp` init started nothing at its first run. The unit came back; the cause is still there.
- **Installed units** keep their init and their record (section 4.5). Neither an XE300 nor a RUT241 still on `gz-tmp` needs anything: with no `restart_tmp_kb` in its record, the agent takes the threshold from the unit's own init, and `GET /api/upgrade`'s `install.restart_tmp_kb_from` reads `init`. A bench confirms that on each model before the rollout.
- **The kill switch** refuses a restart on every layout. With it in place, the agent is stopped with the init's `stop`; an init installed from this release no longer stops it on `restart`.
- **Mixed versions.** An agent before 0.6.14 has no `restart` key and no `instance`; it is never asked, and nothing else about it changes. A unit its own helper rolls back to 0.6.13 loses the route and its record's file stays, unread.
- **A relay** refuses the route for itself (`relay`). It is restarted at its host.

## 8. Slices and status

Build order. Each is accepted as said; the TRD's status flips in the change that ships the release.

1. **Rider A** (4.2): Planned. Built (2026-11-23, `3e558e3`, `b46c719`, `871e19e`): 8 new tests in the library. Accepted when the release bench (6.2) reads the same on the released binary.
2. **Rider B** (4.3): Planned. Built (2026-11-23, `d695344`): 4 new tests, 84 in sctlin. Accepted when a playbook holding `{{agent}}` runs from sctlin against the bench.
3. **The upgrade helper's health read** (4.4): Planned. Built (2026-11-23): `HttpHealth` reads through the reader the restart's helper uses, the whole exchange within 5 s (`HEALTH_READ_TOTAL`); 2 new tests, one that a health answer which never ends is one failed read, one that pins the judgment (2 answers in a row, 5 s apart, 180 s, a failed read starting the count again), and the upgrade's 8 own tests unchanged. The read's total deadline of 5 s. Accepted when the upgrade's own tests pass unchanged, its judgment is shown unchanged (the answers, their spacing, the window), and a health answer that never ends is one failed read, not a held helper. **Open, not built:** the command's deadline, until slice 7 has timed the restart command on every layout; it is then at least three times the slowest measured, and accepted when a stand-in command that never returns ends as a rollback, not as a held lock.
4. **An installed `gz-tmp` unit's threshold** (4.5, option 1): Planned. Built (2026-11-23): `InstallInfo::load` reads the init the record's restart command names when a `gz-tmp` record gives no `restart_tmp_kb` (`install.rs`, `read_init_threshold`, `init_tmp_kb`), through `read_own`, 64 KiB at most; the report carries `restart_tmp_kb_from`. 3 new tests in the library (the line's shape, a staged unit through `record`, `init` and `default`, a FIFO and `/dev/zero` as the init answered at once) and 1 in `restart_inits.rs`: this repository's two `gz-tmp` inits read as 16384 and 8192. Accepted when a staged `gz-tmp` unit with no `restart_tmp_kb` in its record and an init that says 8192 is judged at 8192, and `GET /api/upgrade` says the value came from the init.
5. **The release commit.** The five version carriers, `Cargo.lock` and `web/package-lock.json`, each edit byte-safe; `CHANGELOG.md` `[0.6.14]` with `[Unreleased]`'s four groups; the index rows of ADR-014, TRD-10 and this TRD. Before it: `main` fetched; a sweep of what this release added for anything a public tree must not name. Accepted when the workspace, clippy, fmt, both docs gates, the bindings diff, `cargo doc` and sctlin's check, test and package are green on the pinned toolchain, as CI runs them, `test-musl` among them.
6. **The tag and its measures.** `v0.6.14` on the release commit; the `Release` workflow signs, proves every server artifact's version (TRD-1) and publishes. Every target measured against 0.6.13 (section 5) and recorded here. Accepted when the proof is `ok` for the six server artifacts and the sizes are written.
7. **The benches** (section 6): 6.2 on the released binary, then 6.1 on the units the owner names. Accepted when every line of both is recorded here, and each `gz-tmp` model's threshold is confirmed. A line that fails holds the rollout, as 0.6.12.303 was held.
8. **The rollout**, under section 7's notes: the bundle on the relays, the previous one kept, the units, the relay last. Accepted as TRD-9 section 5 accepts one.

**What ran with the riders (2026-11-23, at `d695344`, on the pinned 1.97.1).** `cargo test --workspace`, nothing skipped: 672 passed in the library, 11 in the binary, 51 in the MCP, 17 in `relay_events.rs`, 9 in `cli_name.rs` (1 ignored, root), 4 in `restart.rs`, 5 in `restart_inits.rs`, 3 in `xe300_relay_route_sh.rs`, 1 and 4 in the two comms crates; the two netns tests ignored, as without root. `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all --check` clean; both docs gates pass; `cargo doc` has the five warnings it had; no stray ts-rs output. sctlin: `npm run test`, 84 passed, and `npm run check`, no error; `npm run package` was not run.

**What ran after the rulings' four builds (2026-11-23, at `4539048`, on the pinned 1.97.1).** `19f199b` (the upgrade helper's health read), `80155ed` (an installed `gz-tmp` unit's threshold), `4635bab` (the watchdog's append), `4539048` (a flag that cannot be read is active). `cargo test --workspace`, nothing skipped: 679 passed in the library, 11 in the binary, 51 in the MCP, 17 in `relay_events.rs`, 9 in `cli_name.rs` (1 ignored, root), 4 in `restart.rs`, 6 in `restart_inits.rs`, 3 in `xe300_relay_route_sh.rs`, 1 and 4 in the two comms crates; the two netns tests ignored, as without root. `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all --check` clean; both docs gates pass; no stray ts-rs output. sctlin: 84 passed, `npm run check` with no error.

## 9. Ruled by the lead (2026-11-23)

The six questions this TRD left open, each with its ruling. The version, 0.6.14, stands.

1. **The upgrade helper's bounds** (4.4). As recommended. The health read's total deadline of 5 s ships in this release, with a test that the upgrade's judgment is otherwise unchanged: the same number of answers, the same spacing, the same window. The command's deadline is not built until the bench has timed the restart command on every layout; it stays an open item here, with the rule that the deadline is at least three times the slowest command measured.
2. **An installed `gz-tmp` unit's threshold** (4.5). Built as a slice. When the install record gives no `restart_tmp_kb`, the agent reads one line of the form `MIN_TMP_KB=<digits>` from the init script its record names, through the same no-wait regular-file handle as rider A, size-bounded, the first match only, digits only. Anything else (no such line, not a regular file, unreadable, a value of 0 or absurdly large) gives the default, and `GET /api/upgrade` says where the value came from (record, init or default), so a bench can see it. Nothing in the init is executed or sourced.
3. **Sizes per target only after the tag.** Accepted. The benches hold the rollout. No workflow change.
4. **The watchdog's append to its history.** The name is judged the same way before the open, in this release: a FIFO or a device at that name costs one refusal and no thread.
5. **The MCP's renderer keeps refusing `{{agent}}`.** Agreed.
6. **The safe-mode flag's reads stay**, with one change: something at the flag's name that is not a regular file, or that cannot be read, must not read as "no flag". The answer says so in a field of its own, and `/api/info` does the same. A unit in safe mode whose flag cannot be read must never look like a unit out of safe mode.
