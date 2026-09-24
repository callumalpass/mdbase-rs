# Watcher readiness ownership and budget review

The engine owns watcher queues and deadlines. Hosts receive only replaceable,
coalescible readiness hints through `WatchWakeup`; registration also emits a hint
to cover already-queued observations. Callbacks execute outside the slot lock and
must be nonblocking and nonpanicking. No event is consumed by a hint. Active
watcher debounce deadlines are unchanged; idle waits no longer need short ticks.

The reviewed source inventory grows from 175 to 179 Rust files: the callback slot,
its watcher-level regression tests, a cohesive filesystem readiness adapter, and
extracted rescan test helpers. The latter two extractions keep the existing large
filesystem/watcher modules within their unchanged per-file budgets rather than
raising legacy exceptions. The total line allowance grows from 101,660 to 101,850
for the callback, wiring, documentation and readiness race tests. There is no new
persistence, dependency, collection interpretation, or alternate watcher.

Validation: full library tests and strict Clippy; callback reentrancy/installation
coverage; real watcher readiness without consuming observations. Connect keeps a
recovery poll as a safety net and does not interpret readiness as a durable event.
