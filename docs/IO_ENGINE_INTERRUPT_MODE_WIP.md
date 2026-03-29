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

## Current Status

**Root cause identified (2026-03-29)**: The spinning was caused by
eventfds registered as `SPDK_FD_TYPE_DEFAULT` instead of
`SPDK_FD_TYPE_EVENTFD`. The distinction is critical:

- `SPDK_FD_TYPE_EVENTFD`: `fd_group_wait()` auto-drains the eventfd
  (reads the counter to 0) before calling the callback. Next
  `epoll_wait` correctly blocks.
- `SPDK_FD_TYPE_DEFAULT`: `fd_group_wait()` calls the callback but
  does NOT drain the fd. Since eventfds are level-triggered, they
  stay readable forever, making `epoll_wait` return on every call.

The draining logic is in `fd_group.c:661`:
```c
if (ehdlr->fd_type == SPDK_FD_TYPE_EVENTFD) {
    bytes_read = read(ehdlr->fd, &count, sizeof(count));
```

**Two independent bugs, same root cause:**

1. **Reactor wakeup_fd** (reactor.rs): Registered via `spdk_fd_group_add`
   which defaults to `DEFAULT`. After `send_future()` writes 1, the
   eventfd is never drained → `epoll_wait` returns instantly forever.
   The reactor_monitor_loop sends heartbeats via `send_future()` every
   1 second, triggering permanent spinning within 1s of boot.
   SPDK's own `reactor.c:1503` uses `SPDK_FD_TYPE_EVENTFD` for its
   equivalent eventfd — we missed this.

2. **Busy poller eventfds** (thread.c): Registered via
   `spdk_interrupt_register` → `spdk_interrupt_register_for_events`
   which sets `SPDK_FD_TYPE_DEFAULT` (line 2883). When
   `busy_poller_set_interrupt_mode(true)` writes 1 to the eventfd,
   it stays permanently readable. The comment at line 1607 even says:
   "Write without read on eventfd will get it repeatedly triggered."

**What was NOT the cause** (these already work correctly):
- Thread `msg_fd`: registered as `SPDK_FD_TYPE_EVENTFD` (line 2831) → auto-drained
- Timed pollers: callback `interrupt_timerfd_process` reads the timerfd (line 1471)
- NVMf/NVMe bdev pollers: call `register_interrupt(NULL, NULL)` → removed from fd_group

**Fix applied**: Changed both registrations to use `SPDK_FD_TYPE_EVENTFD`:
- reactor.rs: `fg.add_with_fd_type(efd, ..., FD_TYPE_EVENTFD)`
- thread.c: `spdk_interrupt_register_ext(efd, ..., &opts)` with `opts.fd_type = SPDK_FD_TYPE_EVENTFD`

**Previous attempts that failed and why**:
1. Raw epoll on thread interrupt fds — wrong mechanism, not how SPDK works
2. fd_group_wait(100ms) — eventfds always hot (DEFAULT type, never drained)
3. Suppress all pollers with register_interrupt(NULL,NULL) — disabled
   Mayastor's own pollers that have no separate interrupt fds
4. Call poll_once()/spdk_thread_poll() after fd_group_wait — nested
   fd_group warning, and redundant since fd_group_wait dispatches work
5. fd_group_wait(-1) with no poll_once — correct reactor pattern but
   eventfds registered as DEFAULT type were never drained

## Key Insights

1. SPDK's fd_group_wait IS the work executor in interrupt mode. It calls
   poller callbacks directly via the interrupt dispatch mechanism. The
   reactor should NOT call spdk_thread_poll() in interrupt mode.

2. **fd_group_wait only auto-drains `SPDK_FD_TYPE_EVENTFD` fds.**
   Fds registered as `SPDK_FD_TYPE_DEFAULT` (the default from
   `spdk_fd_group_add` and `spdk_interrupt_register`) are dispatched
   but NEVER drained. Since eventfds are level-triggered, an undrained
   eventfd with counter > 0 causes `epoll_wait` to return it on every
   call — a permanent busy-spin.

3. SPDK's own reactor (`reactor.c:1503`) registers its eventfds as
   `SPDK_FD_TYPE_EVENTFD`. Any custom reactor code must do the same.

4. Timed pollers work correctly because their callback
   (`interrupt_timerfd_process`) explicitly reads the timerfd. Busy
   pollers do NOT — they rely on the fd_type to drain them.

5. Thread `msg_fd` is registered as `SPDK_FD_TYPE_EVENTFD` (line 2831)
   and works correctly. Messages are processed by
   `thread_interrupt_msg_process` via `msg_queue_run_batch`.

6. The NVMe I/O channel poller (channel.rs) has period=0 (busy poller).
   After fix #2, its eventfd fires once on mode entry then goes dormant.
   Actual NVMe I/O is handled by the NVMf TCP transport's separately
   registered interrupt fds.
