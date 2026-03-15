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

Each runtime spawns a dedicated OS thread running a QuickJS worker. Communication happens via `mpsc` channels. The BEAM process never blocks on JS execution directly.

```
BEAM Process
  ├─ eval/2 ──► DirtyIo NIF ──► mpsc channel ──► Worker Thread ──► QuickJS
  │              blocks on rx     Eval message     clears interrupt, evals
  │              ◄── result channel ◄──────────────┘
  │
  └─ eval/3 ──► NIF sends EvalWithCallbacks, returns {:ok, nil}
       │         Worker installs dispatch + wrappers, evals
       ├─ receive {:quicksand_callback, id, name, args} ──► call Elixir fun
       │    └─► NIF respond_callback ──► CallbackRegistry channel ──► Worker resumes
       └─ receive {:quicksand_result, result} ──► return
```

### Callback Mechanism

- `__quicksand_dispatch`: Rust native function installed per eval/3 call. Reads args from `__quicksand_cb_args` global, sends to Elixir via `send_to_pid`, blocks on `CallbackRegistry` channel for response.
- `__quicksand_make_wrapper`: Persistent JS factory (installed once in `Worker::new`, frozen via `Object.defineProperty`). Creates per-callback wrapper functions that set args global, call dispatch, retrieve result from `__quicksand_cb_result` global.
- Callback names are passed as JS function parameters to the factory, never interpolated into eval'd code (prevents JS injection).

### Type Conversion

Two-phase for Elixir→JS (needed because NIF thread has `Env` access but not `rquickjs::Ctx`, worker thread has the reverse):
1. NIF thread: `Term` → `TermValue` (intermediate enum, `convert.rs:term_to_intermediate`)
2. Worker thread: `TermValue` → `rquickjs::Value` (`convert.rs:intermediate_to_js`)

JS→Elixir is single-phase: `rquickjs::Value` → `JsValue` enum → Erlang `Term` (via `Encoder` impl).

### Interrupt / Timeout

The `interrupt` `Arc<AtomicBool>` is shared between the NIF side and the worker. It is always cleared on the **worker side** when starting a new eval (not the NIF side) to avoid a race condition where the flag is cleared before a previous timed-out eval has been interrupted. The QuickJS interrupt handler checks this flag on every loop iteration.

### Key Files

- `lib/quicksand.ex` — public API, callback receive loop, validation
- `lib/quicksand/native.ex` — RustlerPrecompiled NIF stubs (set `QUICKSAND_BUILD=true` to compile from source)
- `native/quicksand/src/lib.rs` — NIF entry points
- `native/quicksand/src/worker.rs` — worker thread, QuickJS eval, callback dispatch
- `native/quicksand/src/convert.rs` — bidirectional type conversion (JS ↔ Erlang)
- `native/quicksand/src/runtime.rs` — Runtime resource, CallbackRegistry, `send_to_pid`
