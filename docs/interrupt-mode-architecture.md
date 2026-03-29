# SPDK Interrupt Mode Architecture for Mayastor

This document explains how SPDK's interrupt mode works and how Mayastor
integrates with it to reduce CPU usage when idle.

## SPDK Polling Model

SPDK uses a three-level hierarchy:

```
Reactor (1 per CPU core)
  └── Thread (1 or more per reactor)
        └── Poller (1 or more per thread)
              - Busy poller (period=0): called on every poll cycle
              - Timed poller (period>0): called at a fixed interval
```

In **poll mode** (the default), a reactor busy-loops calling
`spdk_thread_poll()` on each thread. `spdk_thread_poll()` executes all
pollers on that thread. This gives the lowest latency but burns 100% CPU
even when no I/O is in flight.

## SPDK Interrupt Mode

SPDK supports an alternative **interrupt mode** where the reactor sleeps
in `epoll_wait()` until an event arrives, instead of busy-looping.

### Initialization

1. `spdk_interrupt_mode_enable()` must be called **before**
   `spdk_thread_lib_init_ext()`. This sets a global flag.

2. When interrupt mode is enabled, every newly created poller
   auto-initializes an interrupt file descriptor:
   - **Busy pollers** (period=0): create an `eventfd` and register it
     in the thread's `fd_group`. The default handler
     (`busy_poller_set_interrupt_mode`) **writes to the eventfd** on
     entering interrupt mode, keeping it permanently triggered.
   - **Timed pollers** (period>0): create a `timerfd` that fires at
     the poller's interval.

3. Pollers that support interrupt mode call
   `spdk_poller_register_interrupt(poller, cb_fn, cb_arg)`:
   - If `cb_fn` is non-NULL: replaces the default handler with a
     custom callback.
   - If `cb_fn` is NULL: calls `poller_interrupt_fini()` which
     **removes the auto-created eventfd/timerfd** from the thread's
     fd_group, then sets the callback to NULL. The poller opts out of
     the default interrupt mechanism entirely.

### The Busy Poller Trap

A busy poller that does NOT call `spdk_poller_register_interrupt()`:
- Keeps its default `eventfd` in the thread's `fd_group`
- On interrupt mode entry, `busy_poller_set_interrupt_mode` writes to
  the eventfd: `write(busy_efd, &notify, sizeof(notify))`
- This eventfd is level-triggered and stays readable forever
- Any `spdk_fd_group_wait()` on a group containing this fd returns
  immediately

A busy poller that DOES call `spdk_poller_register_interrupt(p, NULL, NULL)`:
- `poller_interrupt_fini()` removes the eventfd from the fd_group
- The callback is set to NULL, so mode transitions are no-ops
- The poller relies on **separately registered** interrupt fds (e.g.,
  socket fds, NVMe controller fds) to signal when work arrives

### Thread Mode Transition

`spdk_thread_set_interrupt_mode(bool enable)` (operates on the current
thread):
1. Iterates all pollers (active, timed, paused)
2. Calls each poller's `set_intr_cb_fn` if non-NULL
3. Sets `thread->in_interrupt = enable`

When `thread->in_interrupt` is true, `spdk_thread_poll()` calls
`spdk_fd_group_wait(thread->fgrp, 0)` (non-blocking) instead of
executing pollers directly. This is designed for use by the reactor,
not standalone.

### Reactor-Level Blocking

SPDK's built-in reactor (`lib/event/reactor.c`) provides the actual
blocking:

```c
// reactor_interrupt_run - called when reactor->in_interrupt is true
static void reactor_interrupt_run(struct spdk_reactor *reactor) {
    int block_timeout = -1; /* EPOLL_WAIT_FOREVER */
    spdk_fd_group_wait(reactor->fgrp, block_timeout);
}
```

The reactor creates its own `fd_group` and **nests** each thread's
`fd_group` into it via `spdk_fd_group_nest()`. When any thread has
events, the reactor wakes up.

### Event Flow in Interrupt Mode

```
TCP data arrives on socket
  → socket fd fires in thread's fd_group
    → thread fd_group fires in reactor's fd_group (nested)
      → spdk_fd_group_wait(reactor->fgrp, -1) returns
        → reactor polls threads
          → spdk_thread_poll() dispatches to poller callbacks
```

## Mayastor's Reactor

Mayastor **bypasses** SPDK's built-in reactor framework by passing `0`
as the third argument to `spdk_thread_lib_init_ext()`. It implements its
own reactor in `io-engine/src/core/reactor.rs`.

The Mayastor reactor:
- Manages SPDK threads in a `VecDeque<spdk_rs::Thread>`
- Polls threads via `spdk_thread_poll()` in a loop
- Handles cross-core message passing via `crossbeam` channels
- Runs Rust futures alongside SPDK polling
- The master core's reactor is called directly from `main()`; tokio
  runs on separate unaffinitized threads

### Interrupt Mode Integration

To add interrupt mode to Mayastor's reactor:

1. Call `spdk_interrupt_mode_enable()` before `spdk_thread_lib_init_ext()`
2. Create a reactor-level `FdGroup` via `spdk_fd_group_create()`
3. Add an `eventfd` to the reactor's fd_group for waking on incoming
   Rust futures (since those arrive via channels, not SPDK mechanisms)
4. On entering interrupt mode:
   - Nest each thread's fd_group into the reactor's fd_group
   - Switch threads to interrupt mode via `spdk_thread_set_interrupt_mode(true)`
5. Call `spdk_fd_group_wait(reactor_fgrp, -1)` to block until events
6. On wake:
   - Switch threads back to poll mode
   - Poll threads via `spdk_thread_poll()`
   - Re-enter interrupt mode

## spdk-rs Bindings

The `spdk-rs` crate wraps SPDK's C API for Rust:

- `Thread::set_interrupt_mode(bool)` → `spdk_thread_set_interrupt_mode()`
- `Thread::get_interrupt_fd_group()` → `spdk_thread_get_interrupt_fd_group()`
- `Thread::interrupt_mode_enable()` → `spdk_interrupt_mode_enable()`
- `FdGroup::create/wait/nest/unnest` → `spdk_fd_group_*`

## Poller Interrupt Support Status

For interrupt mode to work, ALL busy pollers on a thread must call
`spdk_poller_register_interrupt()` to clean up their default eventfds.

| Subsystem | Poller | Calls register_interrupt? |
|-----------|--------|--------------------------|
| NVMf TCP transport | `nvmf_tgroup_poll` | Yes (transport.c:606) |
| NVMf TCP acceptor | accept_poller | Yes (tcp.c:870) |
| NVMe bdev | group poller | Yes (bdev_nvme.c:3902) |
| NVMe bdev | adminq_timer | Yes (bdev_nvme.c:6180) |
| AIO bdev | channel poller | Yes (bdev_aio.c:982) |
| **iSCSI** | **iscsi_poll_group_poll** | **No** ← blocks interrupt mode |
| iSCSI | nop_poller (period=1s) | N/A (timed, uses timerfd) |

The iSCSI poll group poller is the only busy poller that doesn't
register interrupt support. Adding one line fixes it:
```c
spdk_poller_register_interrupt(pg->poller, NULL, NULL);
```
