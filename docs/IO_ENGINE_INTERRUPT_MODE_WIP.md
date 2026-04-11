# io-engine Interrupt Mode — Work in Progress

Implementation of SPDK interrupt mode for Mayastor io-engine to reduce
CPU from 100% per core (busy-polling) to near-zero when idle.

Tracking issue: openebs/mayastor#1745

## Repositories

| Repo | Fork | Branch |
|------|------|--------|
| openebs/spdk | jr42/spdk | `v25.05.x-mayastor` |
| openebs/spdk-rs | jr42/spdk-rs | `feat/interrupt-mode` |
| openebs/mayastor | jr42/mayastor | `feat/interrupt-mode` |

GitHub Actions workflow `build-custom-image.yml` on jr42/mayastor
builds and pushes `ghcr.io/jr42/mayastor-io-engine:interrupt-mode`.

## How io-engine Operates (Single Core, cpuCount=1)

### Threads on Core 3

| Thread | Created by | Purpose |
|--------|-----------|---------|
| `init_thread` | Reactors::init | Bootstrap, gRPC dispatch target via send_future() |
| `nvmx_poll_adminq_3` | NvmxSubsystem::init | NVMe controller admin queue polling |
| `mayastor_nvmf_tcp_pg_core_3` | Target::init_poll_groups | NVMe-oF TCP target — serves volumes to CSI nodes |
| `iscsi_poll_group_3` | SPDK iscsi subsystem | iSCSI target (unused by Mayastor but auto-initialized) |

Additional threads created dynamically per NVMe-oF TCP connection to
remote replicas (I/O channels with pollers).

### I/O Flow: Incoming Volume Read

```
CSI node sends NVMe-oF TCP read
  → mayastor_nvmf_tcp_pg_core_3 receives via TCP socket
    → NVMf transport dispatches to nexus bdev
      → Nexus forwards to child replica bdevs
        → NVMe initiator submits to remote replica via NVMe-oF TCP
          → I/O channel poller detects completion
            → Nexus aggregates responses
              → NVMf transport sends TCP response to CSI node
```

### Poll Mode (Default)

```
Reactor::poll_reactor() loop:
  for each thread:
    spdk_thread_poll(thread)
      → thread_poll() executes ALL pollers on thread
  receive_futures()  // Rust async from gRPC/tokio
  run_futures()
  add_incoming()     // newly scheduled threads
  // IMMEDIATELY loop — no sleep — 100% CPU
```

### Pollers on Each Thread

Pollers are registered via `spdk_poller_register()` or spdk-rs
`PollerBuilder`. Two types:

- **Busy poller** (period=0): called every poll cycle. Used for I/O hot
  paths (NVMf poll group, NVMe I/O channel).
- **Timed poller** (period>0): called at intervals via timerfd. Used for
  admin tasks (admin queue ~1ms, keepalive, nop).

## How SPDK Interrupt Mode Works

### Initialization

1. `spdk_interrupt_mode_enable()` — called BEFORE `spdk_thread_lib_init_ext()`
2. Every new poller auto-gets an interrupt fd:
   - Busy pollers: eventfd + `busy_poller_set_interrupt_mode` callback
   - Timed pollers: timerfd + `period_poller_set_interrupt_mode` callback
3. The interrupt fd is registered in the thread's `spdk_fd_group`

### Entering Interrupt Mode

`spdk_thread_set_interrupt_mode(true)` on each thread:
- Iterates all pollers, calls each `set_intr_cb_fn(poller, true)`
- Busy pollers: `write(eventfd, 1)` — makes eventfd readable (one-shot)
- Timed pollers: `timerfd_settime()` — arms the timer
- Sets `thread->in_interrupt = true`

### Reactor Blocking (SPDK's own reactor.c)

```c
reactor_interrupt_run(reactor) {
    spdk_fd_group_wait(reactor->fgrp, -1);  // BLOCKS forever
}
```

Thread fd_groups are nested into reactor fd_group via
`spdk_fd_group_nest()`. All fds end up in one epoll instance.

### How fd_group_wait Dispatches Work

```c
spdk_fd_group_wait(fgrp, timeout):
  nfds = epoll_wait(fgrp->epfd, events, max, timeout)  // BLOCKS
  for each event:
    if eventfd: read(fd) to drain counter    // ← clears the one-shot
    call callback(arg)                        // ← runs the poller fn
  return nfds
```

**Critical**: fd_group_wait IS the work executor. It calls poller
callbacks directly. There is no separate `spdk_thread_poll()` call in
SPDK's interrupt reactor path.

### After First Dispatch

1. Busy poller eventfd was written once on mode entry
2. fd_group_wait reads it (drains to 0), calls poller callback
3. Next epoll_wait: eventfd is no longer readable → **BLOCKS**
4. Wakes only on: real I/O (socket fd), timer expiry, or message (msg_fd)

### The spdk_poller_register_interrupt(poller, NULL, NULL) Pattern

Pollers with separate interrupt fds (e.g., NVMf TCP has socket fds)
call this to:
1. Remove the auto-created busy eventfd from fd_group (`poller_interrupt_fini`)
2. Set callback to NULL (mode transitions become no-ops)

The poller then runs via its separately registered interrupt fd
(e.g., `nvmf_tcp_poll_group_intr` registered via `SPDK_INTERRUPT_REGISTER_FOR_EVENTS`).

**Do NOT call this on pollers without separate interrupt fds** — it
disables the poller entirely in interrupt mode.

### Who Calls spdk_poller_register_interrupt(NULL, NULL)

| Subsystem | File | Has separate interrupt fd? |
|-----------|------|--------------------------|
| NVMf transport | transport.c:606 | Yes — socket group fd |
| NVMf TCP acceptor | tcp.c:870 | Yes — listen socket fd |
| NVMe bdev IO | bdev_nvme.c:3902 | Yes — NVMe poll group fd |
| NVMe bdev admin | bdev_nvme.c:6180 | Yes — admin qpair fd |
| AIO bdev | bdev_aio.c:982 | Yes — AIO eventfd |
| iSCSI poll group | **iscsi_subsystem.c** | Yes — socket group fd |

The iSCSI one was missing in the OpenEBS fork — we added it.

### Who Does NOT (and should not)

Mayastor's own pollers via PollerBuilder:
- NVMe I/O channel poller (channel.rs) — period=0, no separate interrupt fd
- NVMe admin queue poller (controller.rs) — period=1ms (timed, uses timerfd)
- NVMe qpair connect poller (qpair.rs) — temporary, period=1ms
- NVMe probe poller (uri.rs) — temporary, period=1ms

These MUST keep their auto-created eventfds/timerfds for interrupt mode.

## Our Implementation

### Changes in jr42/spdk (SPDK fork)

**File**: `lib/iscsi/iscsi_subsystem.c`

Added 1 line after iSCSI poll group poller registration:
```c
pg->poller = SPDK_POLLER_REGISTER(iscsi_poll_group_poll, pg, 0);
spdk_poller_register_interrupt(pg->poller, NULL, NULL);  // ← added
```

### Changes in jr42/spdk-rs

**wrapper.h**: Added `#include <spdk/fd_group.h>`

**src/thread.rs**:
- `Thread::set_interrupt_mode(bool)` — wraps `spdk_thread_set_interrupt_mode`
- `Thread::get_interrupt_fd_group()` — wraps `spdk_thread_get_interrupt_fd_group`
- `Thread::interrupt_mode_enable()` — wraps `spdk_interrupt_mode_enable`
- `Thread::poll_counted()` — returns completion count from `spdk_thread_poll`
- `FdGroup` struct — wraps `spdk_fd_group` with create/add/wait/nest/unnest/Drop

**src/poller.rs**: No changes (pollers keep their default interrupt behavior).

**nix/pkgs/libspdk/default.nix**: Points to jr42/spdk fork with iSCSI fix.

### Changes in jr42/mayastor

**io-engine/src/core/env.rs**:
- `--enable-interrupt-mode` / `ENABLE_INTERRUPT_MODE` CLI flag
- Threaded through MayastorEnvironment → Reactors::init

**io-engine/src/core/reactor.rs**:
- `ReactorState::Interrupt` variant
- `Reactor` fields: `interrupt_enabled`, `fgrp: Option<FdGroup>`, `wakeup_fd: RawFd`
- `Reactors::init()`: calls `spdk_interrupt_mode_enable()` before `spdk_thread_lib_init_ext()`
- `Reactor::new()`: creates FdGroup + eventfd, registers eventfd in fd_group
- `enter_interrupt_mode()`: nests thread fd_groups, switches threads to interrupt mode
- `exit_interrupt_mode()`: unnests, switches back to poll mode
- `wait_for_events()`: calls `fgrp.wait(-1)` — blocks until events
- `send_future()`: writes to wakeup eventfd to wake from fd_group_wait
- `poll_reactor()` Interrupt handler: `wait_for_events()` then `receive_futures/run_futures/add_incoming` — NO `poll_once()`/`spdk_thread_poll()`

**docs/interrupt-mode-architecture.md**: Architecture documentation.

## Testing Procedure

### Build

Trigger via GitHub Actions:
```bash
gh workflow run build-custom-image.yml \
  --repo jr42/mayastor \
  --ref feat/interrupt-mode \
  -f tag=interrupt-mode
```

Check status:
```bash
gh run list --repo jr42/mayastor \
  --workflow=build-custom-image.yml --limit 1 \
  --json status,conclusion --jq '.[0]'
```

### Deploy to Single Node (talos3)

The DaemonSet uses `updateStrategy: OnDelete` and ArgoCD sync is Manual,
so we can patch safely without auto-revert.

```bash
# Patch DaemonSet (no pods restart due to OnDelete)
kubectl -n openebs patch ds openebs-io-engine --type=json -p='[
  {"op": "replace", "path": "/spec/template/spec/containers/1/image",
   "value": "ghcr.io/jr42/mayastor-io-engine:interrupt-mode"},
  {"op": "replace", "path": "/spec/template/spec/containers/1/imagePullPolicy",
   "value": "Always"}
]'

# Ensure env vars are set on container index 1 (io-engine, not metrics-exporter)
# ENABLE_INTERRUPT_MODE=true should already be there from previous patches

# Delete just talos3's pod to trigger recreation
kubectl -n openebs delete pod $(kubectl -n openebs get pods \
  -l app=io-engine --field-selector spec.nodeName=talos3 \
  -o jsonpath='{.items[0].metadata.name}')
```

### Verify

```bash
# Check pod is running
kubectl -n openebs get pods -l app=io-engine -o wide

# Check interrupt mode logs
kubectl -n openebs logs <pod> -c io-engine | grep -i interrupt

# Check CPU — expect near 0 instead of 1000m
kubectl -n openebs top pods -l app=io-engine

# Check volumes healthy
kubectl -n openebs exec kubectl-mayastor-<pod> -- \
  kubectl-mayastor get volumes -r http://openebs-api-rest:8081
```

### Rollback

```bash
kubectl -n openebs patch ds openebs-io-engine --type=json -p='[
  {"op": "replace", "path": "/spec/template/spec/containers/1/image",
   "value": "docker.io/openebs/mayastor-io-engine:v2.8.0"},
  {"op": "replace", "path": "/spec/template/spec/containers/1/imagePullPolicy",
   "value": "IfNotPresent"}
]'
kubectl -n openebs delete pod <talos3-pod>
```

## Earlier Work (2026-03-29)

Initial investigation found the reactor was busy-spinning because some
eventfds were registered as `SPDK_FD_TYPE_DEFAULT` instead of
`SPDK_FD_TYPE_EVENTFD`. The distinction:

- `SPDK_FD_TYPE_EVENTFD`: `fd_group_wait()` auto-drains the eventfd
  (reads the counter to 0) before calling the callback. Next
  `epoll_wait` correctly blocks.
- `SPDK_FD_TYPE_DEFAULT`: `fd_group_wait()` calls the callback but
  does NOT drain the fd. Level-triggered eventfds then stay readable
  forever, making `epoll_wait` return on every call.

The draining logic is in `fd_group.c:661`:
```c
if (ehdlr->fd_type == SPDK_FD_TYPE_EVENTFD) {
    bytes_read = read(ehdlr->fd, &count, sizeof(count));
```

Two distinct fds were miscategorised:

1. **Reactor wakeup_fd** (`reactor.rs`) — registered via `spdk_fd_group_add`
   which defaults to `DEFAULT`. Fix: `add_with_fd_type(efd, …, FD_TYPE_EVENTFD)`.
2. **Busy poller eventfds** (SPDK `thread.c`) — registered via
   `spdk_interrupt_register` which also defaults to `DEFAULT`. Fix:
   `spdk_interrupt_register_ext(efd, …, &opts)` with
   `opts.fd_type = SPDK_FD_TYPE_EVENTFD`.

**These two fixes were necessary but not sufficient.** They got the
reactor into interrupt mode without busy-spinning, but broader testing
(2026-04-11) revealed that the resulting build is still broken in
several ways — see below.

## Reality Check (2026-04-11)

Comprehensive end-to-end testing with a docker-compose + pytest harness
on a Colima x86_64/aarch64 VM. Drove io-engine directly via
`io-engine-client` gRPC calls and via `test/python/tests/publish/test_nexus_publish.py`.

### What works

- `ENABLE_INTERRUPT_MODE=true NVME_IOQ_POLL_PERIOD=1000us`
  on a **single-core** io-engine (`-l 1`) starts cleanly: reactor enters
  interrupt mode, NVMf TCP target comes up, gRPC server listens,
  `fd_group_wait` loop runs with ~4 ms cadence, `bdev create`/`bdev share`/
  `nexus create` all complete in ~100 ms.
- Pure polling mode (no interrupt-mode env vars) passes all 15 tests of
  `test_nexus_publish.py` in ~4 s — the parent branch is functional for
  polling, baseline holds.

### What doesn't (confirmed bugs on `feat/interrupt-mode`, not just the WIP tip)

1. **Multi-core interrupt mode deadlocks at init.**
   With `-l 1,2` (or `-l 3,4`, or `-l 1,2,3`) plus
   `ENABLE_INTERRUPT_MODE=true`, every reactor enters interrupt mode
   and schedules its NVMx/NVMf threads, then init_thread fires a single
   `fd_group_wait` and goes silent forever. gRPC server is never
   configured. Reproduced on both the aarch64 fresh build and the
   published x86_64 `ghcr.io/jr42/mayastor-io-engine:interrupt-mode`
   image under Rosetta. Hits `feat/interrupt-mode` and
   `feat/interrupt-mode-fdgroup-wip` identically. **Prod (`-l3`, single
   core) masks this entirely.**

2. **Nexus destroy hangs indefinitely in single-core interrupt mode.**
   Direct `io-engine-client` reproducer, no pytest involved:
   ```
   bdev create malloc:///test0?size_mb=16&blk_size=4096   # OK, ~50 ms
   nexus create … 16MiB bdev:///test0                     # OK, ~50 ms
   nexus destroy …                                         # hangs forever
   ```
   Logs show the destroy reaches `all children closed` (nexus_bdev_children.rs:447)
   and then nothing. The nexus stays in `faulted` state, its bdev
   remains orphaned. The reactor is still alive — other gRPC calls
   (`nexus list`, `bdev list`) respond instantly — but the
   `DestroyNexus` future never wakes. Same behaviour with a multi-child
   nexus (bdev + nvmf), or with the pytest harness's 4-child nexus
   (bdev + nvmf + aio + uring). **The stuck point is between
   `all children closed` and the next expected log line
   `nexus destroyed ok` (nexus_bdev.rs:863)** — most likely inside
   `unregister_bdev_async()` in `spdk-rs/src/bdev_async.rs:101-124`
   waiting on the oneshot from `inner_unregister_callback`, which in
   turn depends on an SPDK bdev-layer poller that isn't firing. Not
   narrowed to an exact line yet.

3. **fd_group nesting (the WIP tip, commits `80432c79` + `9b4ba776`)
   produces ENXIO on NVMe-oF TCP I/O qpair connect.**
   `nvme_tcp.c:2341: Failed to construct the tqpair via correct icresp`
   fires after `icreq_timeout_tsc` expires. Root cause: the nesting
   code sets the NVMe I/O channel's poller to `period=0` +
   `register_interrupt_none()`, expecting events from the nested NVMe
   poll group fd_group. But that fd_group is empty for TCP qpairs
   because SPDK `nvme_poll_group_add_qpair_fd()` short-circuits on
   `group->enable_interrupts == false`, which in turn is captured from
   `qpair->ctrlr->opts.enable_interrupts`, which SPDK (through v26.01
   upstream) explicitly refuses to set for non-PCIe transports
   (`bdev_nvme.c:6770-6778` and the TCP transport ops struct at
   `nvme_tcp.c:2993` has no `.qpair_get_fd` entry at all).
   Admin qpair is unaffected because it's driven by a separate timerfd
   poller (`nvmx_poll_adminq_N`). Only I/O qpairs break.

### Upstream status (checked 2026-04-11)

- v24.09: NVMe-oF TCP interrupt mode **target-side**
- v25.05: NVMe driver interrupt mode **PCIe only**
- v25.09: nothing relevant
- v26.01 (latest release): **NVMe target RDMA** interrupt mode
- master (v26.05.0-pre): still no TCP initiator `.qpair_get_fd`,
  still has the PCIe-only gate

**No upstream SPDK release has NVMe driver (initiator) interrupt mode
for TCP.** SPDK issue #3443 was about PCIe (Samsung, completed);
Dell's nvmf/tcp work in the issue body refers to target-side. A
client-side TCP interrupt patch is not on any release track I can find.

## Status Update (2026-04-12) — Single-Core Interrupt Mode Resolved

The single-core blocker (`nexus destroy` / `bdev destroy` hang) has been
root-caused and fixed. Single-core interrupt mode now passes the entire
pytest harness with **zero regressions vs poll mode**.

### Root cause of the destroy hang

Our earlier "fix" commit `bdff48d1` ("fix(bdev): register interrupt for
bdev_wait_for_examine poller") added
`spdk_poller_register_interrupt(ctx->poller, NULL, NULL)` to silence
the busy-spin from `bdev_wait_for_examine_cb` (a `period=0` busy
poller). With `set_intr_cb_fn = NULL`, the call path through
`spdk_poller_register_interrupt` calls `poller_interrupt_fini()`,
which **closes the busy-poller's auto-fire eventfd** and removes it
from the thread's `fd_group`. The poller is then left with no
interrupt source at all. Once the thread is in interrupt mode,
`thread_interrupt_msg_process` only services messages, not pollers —
so `bdev_wait_for_examine_cb` never fires again.

That has a non-obvious downstream effect: `spdk_bdev_register` opens
a *temporary* descriptor for the bdev's examine pass and only closes
it inside `bdev_register_finished`, which is what
`bdev_wait_for_examine_cb` calls when the examine completes. With
the poller dead, `bdev_register_finished` is never called, so the
temp desc lingers in `bdev->internal.open_descs` forever.

Every later `spdk_bdev_unregister` then takes this branch in
`bdev_unregister_unsafe`:

```c
TAILQ_FOREACH_SAFE(desc, &bdev->internal.open_descs, link, tmp) {
    rc = -EBUSY;
    event_notify(desc, _remove_notify);  // dispatched to _tmp_bdev_event_cb
}
```

`_tmp_bdev_event_cb` is a no-op log line, so the desc is never
closed. `bdev_unregister_unsafe` returns `-EBUSY`, the bdev's
status is set to `REMOVING` but the bdev is **not** removed from
`g_bdev_mgr.bdevs`, `bdev_destroy_cb` is never invoked, and the
oneshot the Rust side is awaiting in `unregister_bdev_async` never
fires. Hang.

This is purely a single-core-reactor / nested-fd_group consequence:
upstream SPDK's stock reactor model doesn't hit it because its busy
pollers fire on every `spdk_thread_poll` round regardless of
interrupt mode, so `wait_for_examine_cb` runs and `bdev_register_finished`
fires normally. Our wrapper (`io-engine/src/core/reactor.rs`) blocks
in `spdk_fd_group_wait` on a single root fgrp and only services
events that come through that fgrp — pollers without an interrupt
source are dead.

### Fix

`spdk-fork` commit `d1a7adab` (`v25.05.x-mayastor`):

```c
/* Use a periodic poller (1 ms) instead of a busy poller. */
ctx->poller = SPDK_POLLER_REGISTER(bdev_wait_for_examine_cb, ctx, 1000);
```

A periodic poller installs a *timerfd* that's registered as an
interrupt source by SPDK's normal poller-register path. It fires in
both poll and interrupt modes, doesn't busy-spin the reactor, and 1 ms
poll latency is fine for examine completion (normally a few µs of
real work).

### Validation — pytest harness, single-core interrupt mode

`ENABLE_INTERRUPT_MODE=true NVME_IOQ_POLL_PERIOD=1000us`, all suites
converted to `-l 1` / `-l 2` (multi-core deadlock still open),
compared against poll-mode baseline:

| Suite | Result | Notes |
|---|---|---|
| publish | **15/15 PASS** | nexus + NVMe-oF TCP publish lifecycle |
| rebuild | **6/6 PASS** | rebuild start/stop/pause/resume |
| replica | **20/20 PASS** + 2 skip | pool + replica + share |
| cli_controller | **2/2 PASS** | |
| replica_uuid | **3/3 PASS** | |
| nexus | 27 PASS, 7F+3E | failures identical in poll mode (env: host nvme-tcp) |
| nexus_fault | 0/2 | failures identical in poll mode (env) |
| nexus_multipath | 0/6 | failures identical in poll mode (env) |
| ana_client | 0/2 | failures identical in poll mode (env) |
| rpc | 0/1 | failure identical in poll mode (pre-existing test bug) |

**Total: 73 pass, 0 interrupt-mode regressions** across the entire
harness. Every failing test fails identically in poll mode and is
attributable to (a) Colima dev VM having no kernel `nvme-tcp` host
stack for the host-side `nvme connect` calls some tests need, or
(b) one stale `rpc::test_rpc_timeout` that's broken in both modes.

### Why this fix isn't in upstream and why Longhorn's fork doesn't carry it

The bug only exists when the SPDK reactor loop is replaced by a
custom Rust loop that nests thread `fd_group`s into one root and
blocks in `spdk_fd_group_wait` (mayastor). Upstream `spdk_tgt` and
Longhorn's spdk-engine both use SPDK's stock reactor, which calls
`spdk_thread_poll` per round, which services busy pollers regardless
of interrupt-mode state. The `wait_for_examine_cb` busy-spin during
examine in upstream is a deliberate trade-off they accept (examine
is microseconds long). It only becomes a bug under our reactor
model.

### Open work after this fix

The two remaining multi-core blockers from the earlier reality
check are unchanged:

- **Multi-core interrupt-mode init deadlock** (item 3 in revised
  goals below) — first `fd_group_wait` on init_thread fires once,
  then everything stops. Reproduced on every multi-core config.
  Workaround: stay on `-l 1` per io-engine container.
- **fd_group nesting / NVMe-oF TCP qpair ENXIO** on the WIP branch
  (item 1 below) — architectural; defer or lift the Longhorn-style
  hybrid initiator (see "Revised Goal" section below).

## Revised Goal — Port the Longhorn Approach

Longhorn v2 shipped "interrupt mode" for their SPDK-based data engine
in Longhorn 1.10 (LEP dated 2025-07-21,
`longhorn/longhorn/enhancements/20250721-v2-engine-interrupt-mode.md`).
Their explicit design, verbatim:

> In the initial phase, interrupt mode will be implemented for the
> SPDK NVMe/TCP transport using a **hybrid approach**:
> - The NVMe-oF **target** will use `epoll` to wait for socket
>   readiness events.
> - The NVMe-oF **initiator** will continue to poll periodically to
>   flush I/O completions.

Concretely:

- **Enable SPDK global interrupt mode** (`spdk_interrupt_mode_enable`
  + thread `set_interrupt_mode(true)`) so the reactor blocks in
  `fd_group_wait` and the NVMf target runs fully event-driven via the
  sock-group epoll fd.
- **Keep the NVMe I/O channel poller as a timerfd** with
  `nvme_ioq_poll_period_us > 0` (default `100`). Do NOT call
  `register_interrupt_none()` on it. Do NOT nest the poll-group
  fd_group. The NVMe initiator stays "polled" — just at a
  human-scale cadence instead of busy-spinning.
- **Keep the NVMe admin poller** at its current periodic cadence so
  keep-alives still fire.
- Longhorn parks the "truly event-driven initiator" path as
  "future investigate" — they explicitly chose not to wire
  `spdk_sock_group_register_interrupt` into the NVMe TCP poll group,
  because upstream SPDK's NVMe driver layer doesn't plumb socket fds
  through to the NVMe poll group's fd_group for TCP.

Longhorn's benchmark data (t2.2xlarge, 8 vCPU, 3x gp2 EBS): interrupt
mode at `1000` µs poll period hits **9077 random read IOPS** vs. 6996
for busy polling — the 1 ms timerfd initiator actually outperforms
busy polling on random I/O because batching benefits amortise over the
wait. Sequential read bandwidth still favours busy polling
(370 vs 248 MiB/s). Our homelab workload is nearly all random I/O, so
the trade is strongly positive.

### Concrete action items for porting

1. **Revert commits `80432c79` (fd_group nesting) and `9b4ba776` (env
   var gate)** from `feat/interrupt-mode-fdgroup-wip`. The nesting
   approach is architecturally incompatible with TCP as long as
   upstream SPDK doesn't expose TCP socket fds through the NVMe
   driver layer. Do not carry them.

2. **Fix the `feat/interrupt-mode` nexus-destroy hang.** ✓ DONE
   2026-04-12 in spdk-fork commit `d1a7adab` — see "Status Update"
   section above. Single-core interrupt mode now passes the entire
   pytest harness with zero regressions.

3. **Fix the multi-core reactor init deadlock.** First `fd_group_wait`
   on init_thread fires once, then the reactor stops responding.
   Other SPDK interrupt mode consumers (Longhorn's `spdk_tgt`) use
   SPDK's own reactor directly and apparently don't hit this — so the
   bug is probably specific to our `reactor.rs` wrapper's handling of
   cross-core wake-up or nested fd_group setup. Prod workaround
   `-l3` is fine for now but it blocks multi-core evaluation.

4. **Add a proper regression test suite** for interrupt mode. The
   `test/python/tests/publish/` suite is the right shape, plus
   `rebuild/`, `replica/`, `nexus_fault/`. All of these need to pass
   with `ENABLE_INTERRUPT_MODE=true NVME_IOQ_POLL_PERIOD=1000us` on a
   single-core io-engine before we can say "interrupt mode works."
   Today, none of them do.

5. **Drop `NVME_FD_GROUP_NESTING` env var** entirely — it only gates
   dead code after the revert.

## Open Questions

- ~~**Exact stuck point of nexus destroy in interrupt mode.**~~
  Resolved 2026-04-12. The hang is in `unregister_bdev_async` →
  `spdk_bdev_unregister` → blocked on `-EBUSY` from a lingering
  temp desc that `bdev_register_finished` should have closed but
  couldn't because `bdev_wait_for_examine_cb` never fires in our
  interrupt-mode reactor. Fixed in spdk-fork `d1a7adab` by switching
  the wait-for-examine poller from `period=0` (busy) to
  `period=1000us` (timerfd). See "Status Update (2026-04-12)" above.

- **Multi-core deadlock root cause.** Happens before gRPC comes up,
  so we can't query state. Needs source-level debugging of
  `enter_interrupt_mode` in `reactor.rs` and cross-core thread
  scheduling. Question: does SPDK's stock reactor loop handle
  multi-core interrupt mode correctly? If yes, can we use it
  directly? If no, what does Longhorn's spdk_tgt do differently?

- **Does Longhorn v2 actually run multi-core in production?** Their
  LEP doesn't specify. Their architecture typically runs one
  `instance-manager` pod per disk/node, and the SPDK reactor CPU
  mask is configurable but often defaults to a single core. Worth
  checking their helm values / issue tracker before assuming
  "Longhorn works multi-core."

- **Is mayastor's custom `reactor.rs` wrapper necessary?**
  Longhorn uses `spdk_tgt` directly. Mayastor wraps SPDK's thread
  and reactor layer in its own Rust structures for lifecycle reasons.
  If the wrapper is the source of the multi-core bug, the fix may
  be smaller than a full rewrite: align the wrapper's `enter_interrupt_mode`
  with what SPDK's `reactor.c:reactor_interrupt_run` does. But this
  needs a careful diff first.

- ~~**Does prod really work?**~~ Partially answered. The destroy
  hang we feared in prod was real and was being masked by
  control-plane retries — and is exactly what the HA republish bug
  in `docs/MAYASTOR_HA.md` describes. Once the spdk-fork fix from
  `d1a7adab` ships in the prod image, volume delete / nexus
  republish should stop hanging. Worth re-validating the HA
  republish path against an upgraded io-engine before declaring
  the HA bug closed.
