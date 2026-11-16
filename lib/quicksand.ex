defmodule Quicksand do
  @moduledoc """
  Sandboxed JavaScript execution via QuickJS-NG.

  Provides `eval/2` for simple evaluation and `eval/3` for evaluation
  with pre-registered Elixir callback functions callable from JS.
  Each runtime runs on a dedicated OS thread with configurable
  memory and time limits.

  ## Reserved JS globals

  When using `eval/3` in callback mode, quicksand installs four
  `globalThis.__quicksand_*` properties on the JS runtime:
  `__quicksand_make_wrapper`, `__quicksand_dispatch`,
  `__quicksand_cb_args`, `__quicksand_cb_result`. User JavaScript
  must not overwrite these or the callback machinery silently
  breaks for that runtime. See `README.md` for the full list and
  rationale.
  """

  @default_timeout 30_000
  @default_memory_limit 256 * 1024 * 1024
  @default_max_stack_size 1024 * 1024
  # The longest wait `receive ... after` accepts.
  @max_timeout 4_294_967_295
  # rquickjs silently disables the stack limit above 16 MiB, and unchecked
  # recursion overflows the worker's native stack and crashes the VM.
  @max_stack_size 16 * 1024 * 1024

  @type runtime :: reference()
  @type js_result :: {:ok, term()} | {:error, String.t()}
  @type callback :: (list() -> {:ok, term()} | {:error, term()})

  @doc """
  Start a new JavaScript runtime on a dedicated OS thread.

  ## Options

    * `:timeout`: max time per eval in milliseconds, including time spent
      in callbacks (default `30_000`)
    * `:memory_limit`: max JS heap in bytes, which also caps the size of
      a converted result (default `268_435_456`)
    * `:max_stack_size`: max JS stack in bytes, at most `16_777_216`
      (default `1_048_576`)

  Raises `ArgumentError` for unknown options and for values that are not
  positive integers.
  """
  @spec start(keyword()) :: {:ok, runtime()} | {:error, term()}
  def start(opts \\ []) do
    opts =
      Keyword.validate!(opts,
        timeout: @default_timeout,
        memory_limit: @default_memory_limit,
        max_stack_size: @default_max_stack_size
      )

    timeout = positive_integer!(opts, :timeout, @max_timeout)
    memory_limit = positive_integer!(opts, :memory_limit)
    max_stack_size = positive_integer!(opts, :max_stack_size, @max_stack_size)

    ref = make_ref()

    case Quicksand.Native.start_runtime(ref, timeout, memory_limit, max_stack_size) do
      {:ok, runtime} ->
        receive do
          {:quicksand_start, ^ref, :ok} -> {:ok, runtime}
          {:quicksand_start, ^ref, {:error, _} = error} -> error
        after
          5_000 ->
            stop(runtime)
            {:error, :start_timeout}
        end

      {:error, _} = error ->
        error
    end
  end

  @doc """
  Evaluate JavaScript code and return the result.

  A returned promise is awaited: its value is the result, and a rejection
  is an error.

      {:ok, 3} = Quicksand.eval(rt, "1 + 2")

  """
  @spec eval(runtime(), String.t()) :: js_result()
  def eval(runtime, code), do: eval(runtime, code, %{})

  @doc """
  Evaluate JavaScript code with pre-registered Elixir callbacks.

  Callback names are strings or atoms. Each callback receives its
  arguments as a list and must return `{:ok, value}` or
  `{:error, reason}`. Errors, raises, throws and exits become JS
  exceptions.

      callbacks = %{
        "add" => fn [a, b] -> {:ok, a + b} end
      }
      {:ok, 5} = Quicksand.eval(rt, "add(2, 3)", callbacks)

  """
  @spec eval(runtime(), String.t(), %{optional(String.t() | atom()) => callback()}) ::
          js_result()
  def eval(runtime, code, callbacks) when is_binary(code) and is_map(callbacks) do
    callbacks = normalize_callbacks!(callbacks)
    tag = :erlang.unique_integer([:positive])

    case Quicksand.Native.eval(runtime, code, Map.keys(callbacks), tag) do
      {:ok, timeout} ->
        deadline = System.monotonic_time(:millisecond) + timeout
        await_result(runtime, callbacks, tag, deadline)

      {:error, _} = error ->
        error
    end
  end

  @doc "Check if a runtime is alive."
  @spec alive?(runtime()) :: boolean()
  def alive?(runtime) do
    Quicksand.Native.is_alive(runtime)
  catch
    :error, :badarg -> false
  end

  @doc "Stop a runtime. Idempotent: safe to call multiple times."
  @spec stop(runtime()) :: :ok
  def stop(runtime) do
    Quicksand.Native.stop_runtime(runtime)
  catch
    :error, :badarg -> :ok
  end

  defp positive_integer!(opts, key, max \\ nil) do
    case Keyword.fetch!(opts, key) do
      value when is_integer(value) and value > 0 and (is_nil(max) or value <= max) ->
        value

      value ->
        bound = if max, do: " no greater than #{max}", else: ""

        raise ArgumentError,
              "#{inspect(key)} must be a positive integer#{bound}, got: #{inspect(value)}"
    end
  end

  defp normalize_callbacks!(callbacks) do
    Map.new(callbacks, fn
      {name, fun} when is_binary(name) ->
        {name, fun}

      {name, fun} when is_atom(name) ->
        {Atom.to_string(name), fun}

      {name, _fun} ->
        raise ArgumentError, "callback names must be strings or atoms, got: #{inspect(name)}"
    end)
  end

  defp await_result(runtime, callbacks, tag, deadline) do
    receive do
      {:quicksand_result, ^tag, result} ->
        result

      {:quicksand_callback, ^tag, id, name, args} ->
        reply = run_callback(callbacks, name, args)
        Quicksand.Native.respond_callback(runtime, id, reply)
        await_result(runtime, callbacks, tag, deadline)
    after
      max(deadline - System.monotonic_time(:millisecond), 0) ->
        Quicksand.Native.abandon(runtime, tag)
        {:error, "timeout"}
    end
  end

  defp run_callback(callbacks, name, args) do
    case Map.fetch(callbacks, name) do
      {:ok, fun} -> invoke_callback(fun, name, args)
      :error -> {:error, "Unknown callback: #{name}"}
    end
  end

  defp invoke_callback(fun, name, args) do
    fun.(args) |> normalize_reply()
  rescue
    FunctionClauseError ->
      {:error, "Callback '#{name}': no matching clause for #{length(args)} arg(s)"}

    BadArityError ->
      {:error, "Callback '#{name}': function must accept a single argument (the args list)"}

    exception ->
      {:error, Exception.message(exception)}
  catch
    :throw, value -> {:error, "Callback '#{name}' threw: #{inspect(value)}"}
    :exit, reason -> {:error, "Callback '#{name}' exited: #{inspect(reason)}"}
  end

  defp normalize_reply({:ok, _} = reply), do: reply
  defp normalize_reply({:error, reason}) when is_binary(reason), do: {:error, reason}

  defp normalize_reply({:error, reason}) when is_atom(reason),
    do: {:error, Atom.to_string(reason)}

  defp normalize_reply({:error, reason}), do: {:error, inspect(reason)}
  defp normalize_reply(other), do: {:error, "Invalid callback result: #{inspect(other)}"}
end
