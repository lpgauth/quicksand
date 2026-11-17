# Quicksand

Sandboxed JavaScript execution for Elixir via [QuickJS-NG](https://github.com/quickjs-ng/quickjs).

Quicksand embeds the QuickJS-NG engine as a Rustler NIF, giving you in-process JS evaluation with strict memory and time limits. Each runtime runs on a dedicated OS thread — JS execution never blocks BEAM schedulers.

## Features

- Sandboxed JS with no filesystem, network, or OS access
- Standard ECMAScript built-ins only: no `console`, timers, `fetch`, `TextEncoder` or `URL` (expose what you need as callbacks). `queueMicrotask` and promises work.
- Configurable memory limit, execution timeout, and stack size
- Pre-registered Elixir callbacks callable from JS
- Direct Erlang term <-> JS value conversion (no JSON serialization)
- Resource-only API (no GenServer overhead)

## Requirements

- Elixir >= 1.15
- Precompiled NIF binaries are provided for macOS (ARM/Intel) and Linux (x86/ARM, glibc and musl)
- Rust toolchain only needed if building from source

## Installation

Add to your `mix.exs`:

```elixir
def deps do
  [
    {:quicksand, "~> 0.2.0"}
  ]
end
```

Precompiled binaries will be downloaded automatically. To build from source instead:

```bash
QUICKSAND_BUILD=true mix deps.compile quicksand
```

## Usage

### Basic Evaluation

```elixir
{:ok, rt} = Quicksand.start()

{:ok, 3} = Quicksand.eval(rt, "1 + 2")
{:ok, "hello"} = Quicksand.eval(rt, "'hello'")
{:ok, %{"a" => 1}} = Quicksand.eval(rt, "({a: 1})")

:ok = Quicksand.stop(rt)
```

### Promises

Pending jobs run before `eval` returns, and a returned promise is awaited:

```elixir
{:ok, rt} = Quicksand.start()

{:ok, 42} = Quicksand.eval(rt, "(async () => 42)()")
{:error, "nope\n" <> _stack} = Quicksand.eval(rt, "Promise.reject(new Error('nope'))")
{:error, "Promise did not settle"} = Quicksand.eval(rt, "new Promise(() => {})")
```

### Resource Limits

```elixir
{:ok, rt} = Quicksand.start(
  timeout: 5_000,            # 5 seconds max execution time
  memory_limit: 10_000_000,  # ~10 MB heap limit
  max_stack_size: 512_000    # 512 KB stack
)

# Infinite loops are interrupted
{:error, "timeout"} = Quicksand.eval(rt, "while(true) {}")

# Runtime remains usable after timeout
{:ok, 42} = Quicksand.eval(rt, "42")
```

`:timeout` covers the whole eval, including time spent in callbacks and in promise jobs. `:memory_limit` also caps the size of the converted result, so a script cannot return more data than its heap allows. `:max_stack_size` can be at most 16 MiB. Unknown options and values that are not positive integers raise `ArgumentError`.

### Callbacks

Register Elixir functions that JS code can call synchronously:

```elixir
{:ok, rt} = Quicksand.start()

callbacks = %{
  "fetch_user" => fn [id] ->
    user = MyApp.Repo.get!(User, id)
    {:ok, %{"name" => user.name, "email" => user.email}}
  end,
  "log" => fn [message] ->
    Logger.info("JS: #{message}")
    {:ok, nil}
  end
}

{:ok, "Alice"} = Quicksand.eval(rt, """
  {
    const user = fetch_user(1);
    log("Found user: " + user.name);
    user.name;
  }
""", callbacks)
```

Callback names can be strings or atoms. Callbacks must return `{:ok, value}` or `{:error, reason}`:

- `{:ok, value}`: the value is converted to JS and returned to the caller.
- `{:error, reason}`: throws a JS `Error` with the reason as its message. An atom reason becomes its name, and any other non-string reason its `inspect/1` form.

A callback that raises, throws or exits throws a JS `Error` too.

```elixir
callbacks = %{
  "risky" => fn [n] ->
    if n > 0, do: {:ok, n * 2}, else: {:error, "must be positive"}
  end
}

# JS can catch callback errors
{:ok, "must be positive"} = Quicksand.eval(rt, """
  try { risky(-1); } catch(e) { e.message; }
""", callbacks)
```

A callback can share its name with an existing global, such as `JSON`. The global is shadowed for that eval only and restored afterwards.

A runtime runs one eval at a time. Evals from other processes queue behind it, and their timeout includes the wait. A callback therefore cannot eval on the runtime that is running it; that call returns an error instead of waiting for itself.

### Lifecycle

```elixir
{:ok, rt} = Quicksand.start()

Quicksand.alive?(rt)  # true

# Global state persists across evals
{:ok, 42} = Quicksand.eval(rt, "globalThis.x = 42")
{:ok, 42} = Quicksand.eval(rt, "x")

# So do top-level let, const and class declarations
{:ok, 1} = Quicksand.eval(rt, "const y = 1; y")
{:error, "redeclaration of 'y'" <> _} = Quicksand.eval(rt, "const y = 1; y")

# Stop is idempotent
:ok = Quicksand.stop(rt)
:ok = Quicksand.stop(rt)

Quicksand.alive?(rt)  # false

# Eval on stopped runtime returns error (doesn't raise)
{:error, "dead_runtime"} = Quicksand.eval(rt, "1")
```

To run the same script more than once, wrap it in a block (`{ ... }`) or a function so its declarations stay local.

## API

| Function | Description |
|----------|-------------|
| `Quicksand.start(opts)` | Start a new JS runtime on a dedicated OS thread |
| `Quicksand.eval(runtime, code)` | Evaluate JS code, return the result |
| `Quicksand.eval(runtime, code, callbacks)` | Evaluate with pre-registered Elixir callbacks |
| `Quicksand.alive?(runtime)` | Check if a runtime is alive |
| `Quicksand.stop(runtime)` | Stop a runtime (idempotent) |

### Start Options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `:timeout` | integer (ms) | `30_000` | Max time per eval, including callbacks; at most `4_294_967_295` |
| `:memory_limit` | integer (bytes) | `268_435_456` (256 MB) | Max JS heap, which also caps the converted result |
| `:max_stack_size` | integer (bytes) | `1_048_576` (1 MB) | Max JS call stack; at most `16_777_216` (16 MB) |

### Errors

`eval` returns `{:error, message}` with:

| Message | Cause |
|---------|-------|
| `"timeout"` | The eval ran past `:timeout` |
| `"dead_runtime"` | The runtime was stopped before or during the eval |
| `"out of memory"` | The JS heap or the converted result exceeded `:memory_limit` |
| `"Maximum nesting depth exceeded"` | The result nests deeper than 64 levels, or refers to itself |
| `"Promise did not settle"` | The returned promise was still pending once all jobs had run |
| `"JavaScript source must be valid UTF-8"` | The code binary is not UTF-8 |
| `"Cannot eval on a runtime from inside one of its own callbacks"` | A callback called `eval` on the runtime that is running it |
| anything else | The thrown value: an `Error`'s message and stack trace followed by each `Caused by:`, a string as is, any other value as JSON or its `String()` form |

QuickJS throws `null` when it cannot allocate the out-of-memory error itself, so a script that throws `null` also reports `"out of memory"`.

## Type Conversion

### JS to Elixir

| JavaScript | Elixir |
|------------|--------|
| `null`, `undefined` | `nil` |
| `true`, `false` | `true`, `false` |
| integer | integer |
| float | float (integer if it has no fractional part and is within ±2^53) |
| BigInt | integer |
| string | binary string (lone surrogates become U+FFFD) |
| Array | list |
| Object | map of its own enumerable string keys |
| Promise | its resolved value (a rejection is an error) |
| function, Symbol | `nil` |
| `NaN`, `Infinity` | `nil` |
| Date, Map, Set, typed array | map of their own enumerable keys (`%{}` for Date, Map and Set) |

Getters run during conversion, and one that throws fails the eval.

### Elixir to JS (callback results)

| Elixir | JavaScript |
|--------|------------|
| `nil` | `null` |
| `true`, `false` | `true`, `false` |
| integer | number (BigInt beyond 64 bits; numbers above 2^53 lose precision) |
| float | number |
| binary string | string |
| atom | string |
| list | Array |
| map | Object (keys must be strings, atoms or integers) |
| tuple, pid, reference, function | not supported (throws a JS `Error`) |

## License

MIT
