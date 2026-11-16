# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

```bash
QUICKSAND_BUILD=true mix deps.get  # fetch deps + force local Rust build
QUICKSAND_BUILD=true mix compile   # build (includes Rust NIF compilation)
QUICKSAND_BUILD=true mix test      # run all tests
mix test test/quicksand_test.exs:42  # run single test by line number
mix compile --warnings-as-errors   # build with strict warnings
mix format                         # format Elixir code
mix format --check-formatted       # check Elixir formatting
mix dialyzer                       # static type analysis
cargo fmt                          # format Rust code
cargo fmt --check                  # check Rust formatting
cargo clippy -- -D warnings        # Rust linter
```

`QUICKSAND_BUILD=true` is required for local development to force compilation from Rust source instead of downloading precompiled binaries. Without it, `RustlerPrecompiled` will try to fetch binaries from GitHub releases.

## Releasing

```bash
./scripts/release.sh  # tags, pushes, waits for CI, generates checksums
```

The script reads the version from `mix.exs`, creates a git tag, waits for the release workflow to build precompiled NIFs for all targets, then generates checksums. After it completes, commit the checksum file and run `mix hex.publish`.

## Architecture

Quicksand is an Elixir NIF wrapping QuickJS-NG (via `rquickjs` crate) for sandboxed JS execution. Resource-only API (no GenServer).

### Thread Model

Each runtime spawns a dedicated OS thread running a QuickJS worker, with a stack sized from `max_stack_size`. NIF calls reach it over an `mpsc` channel and never block on JS execution, so no scheduler (dirty or not) is held while code runs.

```
BEAM Process
  └─ eval/2, eval/3 ──► eval NIF queues an EvalRequest, returns {:ok, timeout_ms}
       │                 Worker installs callbacks, evals, drains jobs, settles a
       │                 returned promise, converts the result, restores globals
       ├─ receive {:quicksand_callback, tag, id, name, args} ──► call Elixir fun
       │    └─► NIF respond_callback ──► CallbackRegistry channel ──► Worker resumes
       └─ receive {:quicksand_result, tag, result} ──► return
```

`eval/2` is `eval/3` with no callbacks. Each call passes a unique `tag` that the worker puts in every message, so a late result from a timed-out call is never taken for the next call's.

### Callback Mechanism

- `__quicksand_dispatch`: Rust native function installed per eval/3 call. Reads args from `__quicksand_cb_args` global, sends to Elixir via `send_to_pid`, blocks on `CallbackRegistry` channel for response.
- `__quicksand_make_wrapper`: Persistent JS factory (installed once in `Worker::try_new`, frozen via `Object.defineProperty`). Creates per-callback wrapper functions that set args global, call dispatch, retrieve result from `__quicksand_cb_result` global.
- Callback names are passed as JS function parameters to the factory, never interpolated into eval'd code (prevents JS injection).
- A callback name can shadow an existing global. `install_callbacks` saves its property descriptor and `remove_callbacks` restores it, using `Object.getOwnPropertyDescriptor`/`defineProperty` captured before any user code runs.
- Callback errors, conversion failures and timeouts are thrown as JS `Error` objects (`Exception::throw_message`).

### Type Conversion

Two-phase for Elixir→JS (needed because NIF thread has `Env` access but not `rquickjs::Ctx`, worker thread has the reverse):
1. NIF thread: `Term` → `TermValue` (intermediate enum, `convert.rs:term_to_intermediate`)
2. Worker thread: `TermValue` → `rquickjs::Value` (`convert.rs:intermediate_to_js`)

JS→Elixir is single-phase: `rquickjs::Value` → `JsValue` enum → Erlang `Term` (via `Encoder` impl). `Converter` charges every value against a budget of `memory_limit` bytes and checks the deadline as it goes, so neither a sparse array nor a value that shares references can expand past the limits.

### Interrupt / Timeout

Each eval carries a deadline, set when the `eval` NIF queues it. The worker enforces it itself through `Interrupt` (`interrupt.rs`): the QuickJS interrupt handler, the job drain, result conversion and callback waits all stop once it passes, and the result becomes `"timeout"`. No other caller's timeout can interrupt an eval, and a caller that dies cannot leave its eval running past the deadline.

The Elixir side waits until the same deadline. If it gives up first, `abandon/2` marks the tag and the worker drops that result instead of sending it.

`stop` sets a separate flag that never clears: it interrupts the running eval, fails pending callbacks, and makes queued evals return `"dead_runtime"`.

### Key Files

- `lib/quicksand.ex` — public API, callback receive loop, validation
- `lib/quicksand/native.ex` — RustlerPrecompiled NIF stubs (set `QUICKSAND_BUILD=true` to compile from source)
- `native/quicksand/src/lib.rs` — module declarations and `rustler::init!`
- `native/quicksand/src/nif.rs` — NIF entry points
- `native/quicksand/src/interrupt.rs` — per-eval deadline and stop flag
- `native/quicksand/src/worker.rs` — worker thread, QuickJS eval, callback dispatch
- `native/quicksand/src/convert.rs` — bidirectional type conversion (JS ↔ Erlang)
- `native/quicksand/src/runtime.rs` — Runtime resource, CallbackRegistry, `send_to_pid`

### Rust Style

Rust code follows the [Google Rust style guide](https://google.github.io/styleguide/rustguide.html). Clippy enforces the rules it can check through `[lints.clippy]` in `native/quicksand/Cargo.toml`.
