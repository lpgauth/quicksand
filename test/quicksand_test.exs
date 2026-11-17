defmodule QuicksandTest do
  use ExUnit.Case
  use ExUnitProperties

  describe "start/stop" do
    test "start and stop runtime" do
      {:ok, rt} = Quicksand.start()
      assert :ok = Quicksand.stop(rt)
    end

    test "start with custom options" do
      {:ok, rt} = Quicksand.start(memory_limit: 10_000_000, timeout: 5_000)
      assert :ok = Quicksand.stop(rt)
    end

    test "stop is idempotent" do
      {:ok, rt} = Quicksand.start()
      assert :ok = Quicksand.stop(rt)
      assert :ok = Quicksand.stop(rt)
    end

    test "eval on stopped runtime returns error" do
      {:ok, rt} = Quicksand.start()
      Quicksand.stop(rt)
      assert {:error, "dead_runtime"} = Quicksand.eval(rt, "1")
    end

    test "eval with callbacks on stopped runtime returns error" do
      {:ok, rt} = Quicksand.start()
      Quicksand.stop(rt)
      callbacks = %{"f" => fn [] -> {:ok, nil} end}

      {elapsed, result} = :timer.tc(fn -> Quicksand.eval(rt, "f()", callbacks) end, :millisecond)
      assert {:error, "dead_runtime"} = result
      assert elapsed < 100
    end

    test "stop interrupts an eval running in another process" do
      {:ok, rt} = Quicksand.start(timeout: 5_000)
      task = Task.async(fn -> Quicksand.eval(rt, "while(true) {}") end)
      Process.sleep(50)
      assert :ok = Quicksand.stop(rt)
      assert {:error, "dead_runtime"} = Task.await(task, 1_000)
    end

    test "start rejects unknown options" do
      assert_raise ArgumentError, ~r/unknown keys \[:timout\]/, fn ->
        Quicksand.start(timout: 1_000)
      end
    end

    test "the NIF rejects a zero memory limit even if called directly" do
      assert {:error, "memory_limit must be positive"} =
               Quicksand.Native.start_runtime(make_ref(), 1_000, 0, 1024 * 1024)
    end

    test "start rejects limits that are not positive or too large" do
      for opts <- [
            [timeout: 0],
            [timeout: -1],
            [timeout: "1000"],
            [timeout: 4_294_967_296],
            [memory_limit: 0],
            [max_stack_size: 0],
            [max_stack_size: 16 * 1024 * 1024 + 1]
          ] do
        assert_raise ArgumentError, fn -> Quicksand.start(opts) end
      end
    end

    test "alive? returns true for running runtime" do
      {:ok, rt} = Quicksand.start()
      assert Quicksand.alive?(rt)
      Quicksand.stop(rt)
    end

    test "alive? returns false after stop" do
      {:ok, rt} = Quicksand.start()
      Quicksand.stop(rt)
      refute Quicksand.alive?(rt)
    end
  end

  describe "eval/2" do
    setup do
      {:ok, rt} = Quicksand.start()
      on_exit(fn -> Quicksand.stop(rt) end)
      %{rt: rt}
    end

    test "arithmetic", %{rt: rt} do
      assert {:ok, 3} = Quicksand.eval(rt, "1 + 2")
      assert {:ok, 42} = Quicksand.eval(rt, "21 * 2")
    end

    test "strings", %{rt: rt} do
      assert {:ok, "hello"} = Quicksand.eval(rt, "'hello'")
      assert {:ok, "hello world"} = Quicksand.eval(rt, "'hello' + ' ' + 'world'")
    end

    test "booleans", %{rt: rt} do
      assert {:ok, true} = Quicksand.eval(rt, "true")
      assert {:ok, false} = Quicksand.eval(rt, "false")
    end

    test "null and undefined", %{rt: rt} do
      assert {:ok, nil} = Quicksand.eval(rt, "null")
      assert {:ok, nil} = Quicksand.eval(rt, "undefined")
    end

    test "arrays", %{rt: rt} do
      assert {:ok, [1, 2, 3]} = Quicksand.eval(rt, "[1, 2, 3]")
    end

    test "objects", %{rt: rt} do
      assert {:ok, %{"a" => 1, "b" => 2}} = Quicksand.eval(rt, "({a: 1, b: 2})")
    end

    test "floats", %{rt: rt} do
      assert {:ok, 3.14} = Quicksand.eval(rt, "3.14")
    end

    test "large integers", %{rt: rt} do
      assert {:ok, 9_007_199_254_740_991} = Quicksand.eval(rt, "Number.MAX_SAFE_INTEGER")
      assert {:ok, -9_007_199_254_740_991} = Quicksand.eval(rt, "-Number.MAX_SAFE_INTEGER")
    end

    test "NaN and Infinity become nil", %{rt: rt} do
      assert {:ok, nil} = Quicksand.eval(rt, "NaN")
      assert {:ok, nil} = Quicksand.eval(rt, "Infinity")
      assert {:ok, nil} = Quicksand.eval(rt, "-Infinity")
    end

    test "functions in objects are stripped", %{rt: rt} do
      assert {:ok, %{"x" => 1}} = Quicksand.eval(rt, "({x: 1, fn: function() {}})")
    end

    test "empty array and object", %{rt: rt} do
      assert {:ok, []} = Quicksand.eval(rt, "[]")
      assert {:ok, %{}} = Quicksand.eval(rt, "({})")
    end

    test "nested structures", %{rt: rt} do
      assert {:ok, %{"items" => [1, 2], "name" => "test"}} =
               Quicksand.eval(rt, "({name: 'test', items: [1, 2]})")
    end

    test "deeply nested structure", %{rt: rt} do
      # Build a 60-level deep nested object (under MAX_DEPTH of 64)
      code = "var o = {v: 1}; for (var i = 0; i < 59; i++) { o = {n: o}; } o"
      assert {:ok, result} = Quicksand.eval(rt, code)
      assert is_map(result)
    end

    test "syntax error", %{rt: rt} do
      assert {:error, msg} = Quicksand.eval(rt, "function {")
      assert is_binary(msg)
    end

    test "runtime error", %{rt: rt} do
      assert {:error, msg} = Quicksand.eval(rt, "undefinedVar.prop")
      assert is_binary(msg)
    end

    test "thrown error", %{rt: rt} do
      assert {:error, msg} = Quicksand.eval(rt, "throw new Error('boom')")
      assert msg =~ "boom"
    end

    test "global state persists", %{rt: rt} do
      assert {:ok, 42} = Quicksand.eval(rt, "globalThis.x = 42")
      assert {:ok, 42} = Quicksand.eval(rt, "x")
    end
  end

  describe "eval/2 results" do
    setup do
      {:ok, rt} = Quicksand.start()
      on_exit(fn -> Quicksand.stop(rt) end)
      %{rt: rt}
    end

    test "thrown values that are not errors", %{rt: rt} do
      assert {:error, "42"} = Quicksand.eval(rt, "throw 42")
      assert {:error, "oops"} = Quicksand.eval(rt, "throw 'oops'")
      assert {:error, ~s({"code":1})} = Quicksand.eval(rt, "throw {code: 1}")
      assert {:error, "[1,2]"} = Quicksand.eval(rt, "throw [1, 2]")
      assert {:error, "undefined"} = Quicksand.eval(rt, "throw undefined")
      assert {:error, "true"} = Quicksand.eval(rt, "throw true")
    end

    test "errors without a string message", %{rt: rt} do
      assert {:error, "42\n" <> _} =
               Quicksand.eval(rt, "throw Object.assign(new Error('x'), {message: 42})")

      assert {:error, "Error\n" <> _} = Quicksand.eval(rt, "throw new Error()")
      assert {:error, "TypeError\n" <> _} = Quicksand.eval(rt, "throw new TypeError()")
    end

    test "error causes are included", %{rt: rt} do
      code = "throw new Error('outer', {cause: new Error('inner', {cause: 'root'})})"
      assert {:error, msg} = Quicksand.eval(rt, code)
      assert msg =~ ~r/\Aouter\n.*\nCaused by: inner\n.*\nCaused by: root\z/s

      code = "const e = new Error('loop'); e.cause = e; throw e"
      assert {:error, msg} = Quicksand.eval(rt, code)
      assert length(String.split(msg, "Caused by:")) == 9
    end

    test "returned promises are awaited", %{rt: rt} do
      assert {:ok, 42} = Quicksand.eval(rt, "Promise.resolve(42)")
      assert {:ok, 42} = Quicksand.eval(rt, "(async () => { await null; return 42 })()")
      assert {:error, msg} = Quicksand.eval(rt, "Promise.reject(new Error('nope'))")
      assert msg =~ "nope"
      assert {:error, "Promise did not settle"} = Quicksand.eval(rt, "new Promise(() => {})")
    end

    test "jobs run before the result is converted", %{rt: rt} do
      code = "const o = {}; Promise.resolve().then(() => { o.x = 1 }); o"
      assert {:ok, %{"x" => 1}} = Quicksand.eval(rt, code)
    end

    test "BigInt becomes an integer", %{rt: rt} do
      assert {:ok, 10} = Quicksand.eval(rt, "10n")
      assert {:ok, -10} = Quicksand.eval(rt, "-10n")
      assert {:ok, n} = Quicksand.eval(rt, "2n ** 100n")
      assert n == Integer.pow(2, 100)
    end

    test "values with no plain-data form", %{rt: rt} do
      assert {:ok, nil} = Quicksand.eval(rt, "Symbol('s')")
      assert {:ok, %{}} = Quicksand.eval(rt, "new Date(0)")
      assert {:ok, %{}} = Quicksand.eval(rt, "new Map([[1, 2]])")
      assert {:ok, %{"0" => 7, "1" => 8}} = Quicksand.eval(rt, "new Uint8Array([7, 8])")
      assert {:ok, %{"a" => 1}} = Quicksand.eval(rt, "({a: 1, [Symbol('s')]: 2})")
    end

    test "cyclic result fails", %{rt: rt} do
      assert {:error, "Maximum nesting depth exceeded"} =
               Quicksand.eval(rt, "var o = {}; o.self = o; o")
    end

    test "a getter that throws fails the result", %{rt: rt} do
      assert {:error, msg} = Quicksand.eval(rt, "({get a() { throw new Error('getter') }, b: 1})")
      assert msg =~ "getter"
      assert {:ok, 1} = Quicksand.eval(rt, "1")
    end

    test "non-ASCII strings round-trip", %{rt: rt} do
      assert {:ok, "héllo 世界 😀"} = Quicksand.eval(rt, "'héllo 世界 😀'")
      assert {:ok, 2} = Quicksand.eval(rt, "'😀'.length")
      assert {:ok, %{"ключ" => "😀"}} = Quicksand.eval(rt, "({'ключ': '😀'})")
    end

    test "lone surrogates become U+FFFD", %{rt: rt} do
      assert {:ok, "a\uFFFDb"} = Quicksand.eval(rt, "'a\\uD800b'")
      assert {:ok, "\uFFFD"} = Quicksand.eval(rt, "'😀'.slice(0, 1)")
      assert {:ok, %{"\uFFFD" => 1}} = Quicksand.eval(rt, "({'\\uDC00': 1})")
    end

    test "keys that become the same after surrogate replacement", %{rt: rt} do
      assert {:ok, %{"\uFFFD" => 2, "ok" => 3}} =
               Quicksand.eval(rt, "({'\\uD800': 1, '\\uDC00': 2, ok: 3})")
    end

    test "NUL bytes in source", %{rt: rt} do
      nul = <<0>>
      assert {:ok, 3} = Quicksand.eval(rt, "'a" <> nul <> "b'.length")
      assert {:ok, 1} = Quicksand.eval(rt, "'\\" <> nul <> "'.length")
      assert {:ok, 2} = Quicksand.eval(rt, "'\\\\" <> nul <> "'.length")
      assert {:ok, true} = Quicksand.eval(rt, "/\\" <> nul <> "/.test('\\0')")
    end

    test "source that is not UTF-8", %{rt: rt} do
      assert {:error, "JavaScript source must be valid UTF-8"} = Quicksand.eval(rt, <<0xFF>>)
    end
  end

  describe "eval/3 callbacks" do
    setup do
      {:ok, rt} = Quicksand.start()
      on_exit(fn -> Quicksand.stop(rt) end)
      %{rt: rt}
    end

    test "single callback", %{rt: rt} do
      callbacks = %{
        "greet" => fn [name] -> {:ok, "Hello, #{name}!"} end
      }

      assert {:ok, "Hello, Alice!"} = Quicksand.eval(rt, "greet('Alice')", callbacks)
    end

    test "callback with multiple args", %{rt: rt} do
      callbacks = %{
        "add" => fn [a, b] -> {:ok, a + b} end
      }

      assert {:ok, 5} = Quicksand.eval(rt, "add(2, 3)", callbacks)
    end

    test "callback returning object", %{rt: rt} do
      callbacks = %{
        "get_user" => fn [id] -> {:ok, %{"id" => id, "name" => "User #{id}"}} end
      }

      assert {:ok, "User 1"} = Quicksand.eval(rt, "get_user(1).name", callbacks)
    end

    test "callback returning list", %{rt: rt} do
      callbacks = %{
        "get_items" => fn [] -> {:ok, [1, 2, 3]} end
      }

      assert {:ok, 3} = Quicksand.eval(rt, "get_items().length", callbacks)
    end

    test "callback called multiple times", %{rt: rt} do
      callbacks = %{
        "double" => fn [n] -> {:ok, n * 2} end
      }

      assert {:ok, 12} = Quicksand.eval(rt, "double(2) + double(4)", callbacks)
    end

    test "multiple callbacks", %{rt: rt} do
      callbacks = %{
        "first" => fn [list] -> {:ok, List.first(list)} end,
        "last" => fn [list] -> {:ok, List.last(list)} end
      }

      code = """
      const arr = [10, 20, 30];
      first(arr) + last(arr);
      """

      assert {:ok, 40} = Quicksand.eval(rt, code, callbacks)
    end

    test "callback error propagates as JS exception", %{rt: rt} do
      callbacks = %{
        "fail" => fn _args -> {:error, "something went wrong"} end
      }

      assert {:error, msg} = Quicksand.eval(rt, "fail()", callbacks)
      assert msg =~ "something went wrong"
    end

    test "callback exception is caught", %{rt: rt} do
      callbacks = %{
        "blow_up" => fn _args -> raise "kaboom" end
      }

      assert {:error, msg} = Quicksand.eval(rt, "blow_up()", callbacks)
      assert msg =~ "kaboom"
    end

    test "callbacks cleaned up after eval", %{rt: rt} do
      callbacks = %{"temp" => fn [] -> {:ok, "hi"} end}
      assert {:ok, "hi"} = Quicksand.eval(rt, "temp()", callbacks)

      # temp should no longer exist
      assert {:error, _} = Quicksand.eval(rt, "temp()")
    end

    test "normal eval works after callback eval", %{rt: rt} do
      callbacks = %{"inc" => fn [n] -> {:ok, n + 1} end}
      assert {:ok, 6} = Quicksand.eval(rt, "inc(5)", callbacks)
      assert {:ok, 42} = Quicksand.eval(rt, "21 * 2")
    end

    test "invalid callback return shape becomes error", %{rt: rt} do
      callbacks = %{"bad" => fn [] -> "bare string" end}
      assert {:error, msg} = Quicksand.eval(rt, "bad()", callbacks)
      assert msg =~ "Invalid callback result"
    end

    test "callback arity mismatch gives clear error", %{rt: rt} do
      callbacks = %{"greet" => fn [name] -> {:ok, "Hi #{name}"} end}
      assert {:error, msg} = Quicksand.eval(rt, "greet('a', 'b')", callbacks)
      assert msg =~ "Callback 'greet'"
      assert msg =~ "no matching clause for 2 arg(s)"
    end

    test "callback with wrong fun arity gives clear error", %{rt: rt} do
      # fn that takes two args instead of one (the args list)
      callbacks = %{"bad" => fn _a, _b -> {:ok, nil} end}
      assert {:error, msg} = Quicksand.eval(rt, "bad(1)", callbacks)
      assert msg =~ "Callback 'bad'"
      assert msg =~ "function must accept a single argument"
    end

    test "callback returning nil", %{rt: rt} do
      callbacks = %{"nothing" => fn [] -> {:ok, nil} end}
      assert {:ok, nil} = Quicksand.eval(rt, "nothing()", callbacks)
    end

    test "callback returning boolean", %{rt: rt} do
      callbacks = %{"check" => fn [n] -> {:ok, n > 0} end}
      assert {:ok, true} = Quicksand.eval(rt, "check(5)", callbacks)
      assert {:ok, false} = Quicksand.eval(rt, "check(-1)", callbacks)
    end

    test "callback returning nested structure", %{rt: rt} do
      callbacks = %{
        "data" => fn [] ->
          {:ok, %{"users" => [%{"name" => "Alice"}, %{"name" => "Bob"}]}}
        end
      }

      assert {:ok, "Bob"} = Quicksand.eval(rt, "data().users[1].name", callbacks)
    end

    test "callback returning atom becomes string", %{rt: rt} do
      callbacks = %{"status" => fn [] -> {:ok, :active} end}
      assert {:ok, "active"} = Quicksand.eval(rt, "status()", callbacks)
    end

    test "callback returning large integer", %{rt: rt} do
      callbacks = %{"big" => fn [] -> {:ok, 9_007_199_254_740_991} end}
      assert {:ok, 9_007_199_254_740_991} = Quicksand.eval(rt, "big()", callbacks)
    end

    test "JS try/catch around failing callback", %{rt: rt} do
      callbacks = %{"risky" => fn [] -> {:error, "nope"} end}

      code = """
      try { risky(); } catch(e) { e.message || String(e); }
      """

      assert {:ok, "nope"} = Quicksand.eval(rt, code, callbacks)
    end
  end

  describe "eval/3 callback edge cases" do
    setup do
      {:ok, rt} = Quicksand.start()
      on_exit(fn -> Quicksand.stop(rt) end)
      %{rt: rt}
    end

    test "callback errors are Error objects", %{rt: rt} do
      callbacks = %{
        "risky" => fn [n] ->
          if n > 0, do: {:ok, n * 2}, else: {:error, "must be positive"}
        end
      }

      assert {:ok, "must be positive"} =
               Quicksand.eval(rt, "try { risky(-1); } catch(e) { e.message; }", callbacks)

      assert {:ok, true} =
               Quicksand.eval(rt, "try { risky(-1); } catch(e) { e instanceof Error }", callbacks)
    end

    test "callback names may be atoms", %{rt: rt} do
      assert {:ok, 4} = Quicksand.eval(rt, "double(2)", %{double: fn [n] -> {:ok, n * 2} end})
    end

    test "callback names that are not strings or atoms raise", %{rt: rt} do
      assert_raise ArgumentError, ~r/callback names must be strings or atoms/, fn ->
        Quicksand.eval(rt, "1", %{1 => fn [] -> {:ok, 1} end})
      end
    end

    test "error reasons that are not strings", %{rt: rt} do
      assert {:error, msg} =
               Quicksand.eval(rt, "f()", %{"f" => fn [] -> {:error, :not_found} end})

      assert msg =~ "not_found"

      assert {:error, msg} = Quicksand.eval(rt, "f()", %{"f" => fn [] -> {:error, {:bad, 1}} end})
      assert msg =~ "{:bad, 1}"
    end

    test "callback throws and exits become JS errors", %{rt: rt} do
      callbacks = %{
        "t" => fn [] -> throw(:boom) end,
        "e" => fn [] -> exit(:boom) end
      }

      assert {:error, msg} = Quicksand.eval(rt, "t()", callbacks)
      assert msg =~ "Callback 't' threw: :boom"
      assert {:error, msg} = Quicksand.eval(rt, "e()", callbacks)
      assert msg =~ "Callback 'e' exited: :boom"
      assert {:ok, "caught"} = Quicksand.eval(rt, "try { t() } catch (e) { 'caught' }", callbacks)

      assert {:ok, 1} = Quicksand.eval(rt, "1")
      refute_received {:quicksand_result, _, _}
    end

    test "callback results that cannot be converted", %{rt: rt} do
      for {value, expected} <- [
            {%{[1] => 2}, "Map keys must be strings, atoms, or integers"},
            {%{1.5 => 2}, "Map keys must be strings, atoms, or integers"},
            {{1, 2}, "Unsupported Elixir term type"},
            {self(), "Unsupported Elixir term type"}
          ] do
        assert {:error, msg} = Quicksand.eval(rt, "f()", %{"f" => fn [] -> {:ok, value} end})
        assert msg =~ expected
      end
    end

    test "callback arguments that cannot be converted", %{rt: rt} do
      test = self()

      callbacks = %{
        "f" => fn _ ->
          send(test, :called)
          {:ok, nil}
        end
      }

      assert {:error, msg} = Quicksand.eval(rt, "const o = {}; o.o = o; f(o)", callbacks)
      assert msg =~ "Failed to convert arguments for 'f'"
      refute_received :called
    end

    test "bignum callback results become BigInt", %{rt: rt} do
      callbacks = %{"big" => fn [] -> {:ok, Integer.pow(2, 70)} end}
      assert {:ok, "bigint"} = Quicksand.eval(rt, "typeof big()", callbacks)
      assert {:ok, n} = Quicksand.eval(rt, "big() + 1n", callbacks)
      assert n == Integer.pow(2, 70) + 1
    end

    test "callbacks restore the globals they shadow", %{rt: rt} do
      assert {:ok, nil} = Quicksand.eval(rt, "globalThis.helper = () => 'mine'; null")
      noop = fn _ -> {:ok, nil} end
      callbacks = %{"JSON" => noop, "helper" => noop, "toString" => noop, "fresh" => noop}

      assert {:ok, nil} = Quicksand.eval(rt, "JSON(); helper(); toString(); fresh()", callbacks)

      assert {:ok, "[1]"} = Quicksand.eval(rt, "JSON.stringify([1])")
      assert {:ok, "mine"} = Quicksand.eval(rt, "helper()")
      assert {:ok, false} = Quicksand.eval(rt, "Object.hasOwn(globalThis, 'toString')")
      assert {:ok, "undefined"} = Quicksand.eval(rt, "typeof fresh")
    end

    test "a callback named __proto__ leaves the global prototype alone", %{rt: rt} do
      same_prototype = "Object.getPrototypeOf(globalThis) === Object.prototype"
      assert {:ok, true} = Quicksand.eval(rt, same_prototype)
      assert {:ok, 7} = Quicksand.eval(rt, "__proto__()", %{"__proto__" => fn [] -> {:ok, 7} end})
      assert {:ok, true} = Quicksand.eval(rt, same_prototype)
    end

    test "invalid and duplicate callback names raise", %{rt: rt} do
      noop = fn _ -> {:ok, nil} end

      assert_raise ArgumentError, ~r/valid UTF-8/, fn ->
        Quicksand.eval(rt, "1", %{<<0xFF>> => noop})
      end

      assert_raise ArgumentError, ~r/unique/, fn ->
        Quicksand.eval(rt, "1", %{:f => noop, "f" => noop})
      end
    end

    test "callbacks named like the globals JS relies on", %{rt: rt} do
      callbacks = %{
        "Array" => fn [] -> {:ok, "array"} end,
        "globalThis" => fn [] -> {:ok, "global"} end,
        "__quicksand_dispatch" => fn [] -> {:ok, "dispatch"} end
      }

      assert {:ok, ["array", "global", "dispatch"]} =
               Quicksand.eval(rt, "[Array(), globalThis(), __quicksand_dispatch()]", callbacks)

      assert {:ok, true} =
               Quicksand.eval(rt, "Array.isArray([]) && typeof globalThis === 'object'")
    end

    test "callbacks keep working after a script replaces builtins", %{rt: rt} do
      callbacks = %{"f" => fn [x] -> {:ok, x + 1} end}
      code = "Array.from = () => { throw new Error('mine') }; delete globalThis.Object; f(1)"
      assert {:ok, 2} = Quicksand.eval(rt, code, callbacks)
      assert {:ok, 3} = Quicksand.eval(rt, "f(2)", callbacks)
    end

    test "a callback kept past its eval cannot be called", %{rt: rt} do
      callbacks = %{"f" => fn [] -> {:ok, 1} end}
      assert {:ok, nil} = Quicksand.eval(rt, "globalThis.kept = f; null", callbacks)
      assert {:error, msg} = Quicksand.eval(rt, "kept()")
      assert msg =~ "Callback 'f' called after its eval ended"
      assert {:error, msg} = Quicksand.eval(rt, "kept()", callbacks)
      assert msg =~ "Callback 'f' called after its eval ended"
    end

    test "a __proto__ key in a callback result is an own property", %{rt: rt} do
      callbacks = %{"f" => fn [] -> {:ok, %{"__proto__" => %{"polluted" => 1}}} end}
      code = "const r = f(); [Object.keys(r), Object.getPrototypeOf(r) === Object.prototype]"
      assert {:ok, [["__proto__"], true]} = Quicksand.eval(rt, code, callbacks)

      assert {:ok, %{"__proto__" => %{"polluted" => 1}}} = Quicksand.eval(rt, "f()", callbacks)
    end

    test "errors raised inside a callback body keep their own message", %{rt: rt} do
      only_atoms = fn atom when is_atom(atom) -> atom end
      unary = fn x -> x end

      callbacks = %{
        "clause" => fn [x] -> {:ok, only_atoms.(x)} end,
        "arity" => fn [x] -> {:ok, apply(unary, [x, x])} end
      }

      assert {:error, msg} = Quicksand.eval(rt, "clause(1)", callbacks)
      assert msg =~ "no function clause matching"
      refute msg =~ "Callback 'clause'"

      assert {:error, msg} = Quicksand.eval(rt, "arity(1)", callbacks)
      assert msg =~ "with arity 1 called with 2 arguments"
      refute msg =~ "must accept a single argument"
    end

    test "a callback cannot eval on its own runtime", %{rt: rt} do
      {:ok, other} = Quicksand.start()

      callbacks = %{
        "same" => fn [] -> {:ok, inspect(Quicksand.eval(rt, "1"))} end,
        "other" => fn [] -> {:ok, inspect(Quicksand.eval(other, "2"))} end
      }

      {elapsed, result} =
        :timer.tc(fn -> Quicksand.eval(rt, "same()", callbacks) end, :millisecond)

      assert {:ok, ~s({:error, "Cannot eval on a runtime from inside one of its own callbacks"})} =
               result

      assert elapsed < 1_000
      assert {:ok, "{:ok, 2}"} = Quicksand.eval(rt, "other()", callbacks)
      Quicksand.stop(other)
    end

    test "callbacks can be used from async code", %{rt: rt} do
      callbacks = %{"double" => fn [n] -> {:ok, n * 2} end}
      code = "(async () => (await Promise.resolve(double(2))) + 1)()"
      assert {:ok, 5} = Quicksand.eval(rt, code, callbacks)
    end

    test "concurrent callers on one runtime get their own results", %{rt: rt} do
      callbacks = %{"times_ten" => fn [n] -> {:ok, n * 10} end}

      results =
        1..20
        |> Enum.map(fn i ->
          Task.async(fn -> Quicksand.eval(rt, "times_ten(#{i})", callbacks) end)
        end)
        |> Task.await_many(5_000)

      assert results == Enum.map(1..20, &{:ok, &1 * 10})
    end
  end

  describe "pending jobs" do
    test "timeout inside a job" do
      for code <- [
            "Promise.resolve().then(() => { while(true) {} }); 1",
            "(async () => { await Promise.resolve(); while(true) {} })(); 1"
          ] do
        {:ok, rt} = Quicksand.start(timeout: 100)
        assert {:error, "timeout"} = Quicksand.eval(rt, code)
        assert {:ok, 42} = Quicksand.eval(rt, "21 * 2")
        assert :ok = Quicksand.stop(rt)
      end
    end

    test "timeout inside a job with callbacks" do
      {:ok, rt} = Quicksand.start(timeout: 100)
      callbacks = %{"noop" => fn [] -> {:ok, nil} end}
      code = "Promise.resolve().then(() => { noop(); while(true) {} }); 1"
      assert {:error, "timeout"} = Quicksand.eval(rt, code, callbacks)
      assert {:ok, 42} = Quicksand.eval(rt, "21 * 2")
      assert :ok = Quicksand.stop(rt)
    end

    test "jobs left by a timeout do not run in the next eval" do
      {:ok, rt} = Quicksand.start(timeout: 100)
      loop = "Promise.resolve().then(() => { while(true) {} });"
      assert {:error, "timeout"} = Quicksand.eval(rt, loop <> loop <> "1")
      assert {:ok, 42} = Quicksand.eval(rt, "21 * 2")
      assert :ok = Quicksand.stop(rt)
    end

    test "callbacks are not called after a timeout" do
      {:ok, rt} = Quicksand.start(timeout: 100)
      callbacks = %{"late" => fn [] -> {:ok, nil} end}

      code =
        "Promise.resolve().then(() => { while(true) {} }); Promise.resolve().then(() => late()); 1"

      assert {:error, "timeout"} = Quicksand.eval(rt, code, callbacks)
      assert {:ok, 42} = Quicksand.eval(rt, "21 * 2")
      refute_received {:quicksand_callback, _, _, _, _}
      assert :ok = Quicksand.stop(rt)
    end

    test "a throwing job does not stop later jobs" do
      {:ok, rt} = Quicksand.start()

      code =
        "globalThis.x = 0; queueMicrotask(() => { throw new Error('boom') }); queueMicrotask(() => { x = 1 }); 1"

      assert {:ok, 1} = Quicksand.eval(rt, code)
      assert {:ok, 1} = Quicksand.eval(rt, "x")
      assert :ok = Quicksand.stop(rt)
    end

    test "out of memory inside a job" do
      {:ok, rt} = Quicksand.start(memory_limit: 2 * 1024 * 1024)

      code =
        "Promise.resolve().then(() => { const arr = []; while(true) { arr.push('x'.repeat(1000)); } }); 1"

      assert {:ok, 1} = Quicksand.eval(rt, code)
      assert {:ok, 42} = Quicksand.eval(rt, "21 * 2")
      assert :ok = Quicksand.stop(rt)
    end

    test "a job storm after a timeout does not hold the worker" do
      {:ok, rt} = Quicksand.start(timeout: 200)
      code = "function f() { Promise.resolve().then(f); Promise.resolve().then(f) } f(); 1"
      assert {:error, "timeout"} = Quicksand.eval(rt, code)
      {elapsed, :ok} = :timer.tc(fn -> Quicksand.stop(rt) end, :millisecond)
      assert elapsed < 100
    end
  end

  describe "resource limits" do
    test "timeout" do
      {:ok, rt} = Quicksand.start(timeout: 100)
      assert {:error, "timeout"} = Quicksand.eval(rt, "while(true) {}")
      Quicksand.stop(rt)
    end

    test "runtime usable after timeout" do
      {:ok, rt} = Quicksand.start(timeout: 100)
      assert {:error, "timeout"} = Quicksand.eval(rt, "while(true) {}")
      assert {:ok, 42} = Quicksand.eval(rt, "21 * 2")
      Quicksand.stop(rt)
    end

    test "timeout with callbacks" do
      {:ok, rt} = Quicksand.start(timeout: 100)

      callbacks = %{
        "noop" => fn [] -> {:ok, nil} end
      }

      assert {:error, "timeout"} =
               Quicksand.eval(rt, "noop(); while(true) {}", callbacks)

      assert {:ok, 1} = Quicksand.eval(rt, "1")
      Quicksand.stop(rt)
    end

    test "timed-out eval with callbacks does not leak its result" do
      {:ok, rt} = Quicksand.start(timeout: 100)
      callbacks = %{"noop" => fn [] -> {:ok, nil} end}
      assert {:error, "timeout"} = Quicksand.eval(rt, "while(true) {}", callbacks)
      assert {:ok, 42} = Quicksand.eval(rt, "21 * 2", callbacks)
      assert {:ok, 42} = Quicksand.eval(rt, "21 * 2")
      refute_received {:quicksand_result, _, _}
      assert :ok = Quicksand.stop(rt)
    end

    test "memory limit" do
      {:ok, rt} = Quicksand.start(memory_limit: 256 * 1024)

      assert {:error, _msg} =
               Quicksand.eval(rt, "const arr = []; while(true) { arr.push('x'.repeat(1000)); }")

      Quicksand.stop(rt)
    end

    test "memory limit error is always reported as out of memory" do
      for limit <- [2 * 1024 * 1024, 4 * 1024 * 1024],
          code <- [
            "const arr = []; while(true) { arr.push('x'.repeat(1000)); }",
            "(() => { const arr = []; while(true) { arr.push('x'.repeat(1000)); } })()"
          ] do
        {:ok, rt} = Quicksand.start(memory_limit: limit)
        assert {:error, msg} = Quicksand.eval(rt, code)
        assert msg =~ "out of memory"
        assert :ok = Quicksand.stop(rt)
      end
    end

    test "runtime usable after memory limit exceeded" do
      # Use a larger limit so QuickJS can GC and recover after OOM
      {:ok, rt} = Quicksand.start(memory_limit: 2 * 1024 * 1024)

      # A top-level `const` would stay reachable as a global binding after the throw.
      assert {:error, _} =
               Quicksand.eval(
                 rt,
                 "(() => { const arr = []; while(true) { arr.push('x'.repeat(1000)); } })()"
               )

      assert {:ok, 1} = Quicksand.eval(rt, "1")
      Quicksand.stop(rt)
    end

    test "stack overflow" do
      {:ok, rt} = Quicksand.start()
      assert {:error, msg} = Quicksand.eval(rt, "function f() { return f(); } f()")
      assert is_binary(msg)
      Quicksand.stop(rt)
    end

    test "runtime usable after stack overflow" do
      {:ok, rt} = Quicksand.start()
      assert {:error, _} = Quicksand.eval(rt, "function f() { return f(); } f()")
      assert {:ok, 1} = Quicksand.eval(rt, "1")
      Quicksand.stop(rt)
    end
  end

  describe "limits on untrusted code" do
    test "eval with callbacks times out on total time, not time between callbacks" do
      {:ok, rt} = Quicksand.start(timeout: 200)
      callbacks = %{"noop" => fn [] -> {:ok, nil} end}

      for code <- [
            "const end = Date.now() + 2000; while (Date.now() < end) noop(); 'done'",
            "while (true) { try { noop() } catch (e) {} }"
          ] do
        {elapsed, result} = :timer.tc(fn -> Quicksand.eval(rt, code, callbacks) end, :millisecond)
        assert {:error, "timeout"} = result
        assert elapsed < 1_000
      end

      Quicksand.stop(rt)
    end

    test "a caller that dies does not leave its eval running" do
      {:ok, rt} = Quicksand.start(timeout: 300)
      test = self()

      callbacks = %{
        "block" => fn [] ->
          send(test, :blocked)
          Process.sleep(:infinity)
        end
      }

      caller =
        spawn(fn ->
          Quicksand.eval(rt, "try { block() } catch (e) {} while (true) {}", callbacks)
        end)

      assert_receive :blocked, 1_000
      Process.exit(caller, :kill)
      Process.sleep(400)
      assert {:ok, 1} = Quicksand.eval(rt, "1")
      Quicksand.stop(rt)
    end

    test "late results of several abandoned evals are dropped" do
      {:ok, rt} = Quicksand.start(timeout: 50)
      # Parsing is not interruptible, so the worker misses both deadlines.
      code = "[" <> Enum.map_join(1..2_000_000, ",", &Integer.to_string/1) <> "].length"
      test = self()

      for _ <- 1..2 do
        spawn(fn ->
          result = Quicksand.eval(rt, code)
          Process.sleep(2_000)
          {:messages, messages} = Process.info(self(), :messages)
          send(test, {result, messages})
        end)
      end

      for _ <- 1..2 do
        assert_receive {{:error, "timeout"}, []}, 10_000
      end

      Quicksand.stop(rt)
    end

    test "cyclic garbage is collected under small memory limits" do
      {:ok, rt} = Quicksand.start(memory_limit: 1024 * 1024)
      code = "for (let i = 0; i < 200000; i++) { const a = {}; a.self = a } 'done'"
      assert {:ok, "done"} = Quicksand.eval(rt, code)
      Quicksand.stop(rt)
    end

    test "results count toward the memory limit" do
      {:ok, rt} = Quicksand.start(memory_limit: 1024 * 1024)
      assert {:error, "out of memory"} = Quicksand.eval(rt, "var a = []; a.length = 5e6; a")
      assert {:ok, 1} = Quicksand.eval(rt, "1")
      Quicksand.stop(rt)
    end

    test "converting a result stops at the timeout" do
      {:ok, rt} = Quicksand.start(timeout: 100)
      code = "let o = 1; for (let i = 0; i < 40; i++) o = [o, o]; o"

      {elapsed, result} = :timer.tc(fn -> Quicksand.eval(rt, code) end, :millisecond)
      assert {:error, _} = result
      assert elapsed < 1_000
      assert {:ok, 1} = Quicksand.eval(rt, "1")
      Quicksand.stop(rt)
    end

    test "deep recursion at the largest stack limit does not crash the VM" do
      {:ok, rt} = Quicksand.start(max_stack_size: 16 * 1024 * 1024)
      code = "function f(n) { return n === 0 ? 0 : 1 + f(n - 1) } f(1e7)"
      assert {:error, msg} = Quicksand.eval(rt, code)
      assert msg =~ "Maximum call stack size exceeded"
      Quicksand.stop(rt)
    end

    test "running evals do not hold dirty I/O schedulers" do
      count = :erlang.system_info(:dirty_io_schedulers) + 2
      runtimes = for _ <- 1..count, do: elem(Quicksand.start(timeout: 1_000), 1)
      tasks = for rt <- runtimes, do: Task.async(fn -> Quicksand.eval(rt, "while(true) {}") end)
      Process.sleep(100)

      {elapsed, {:ok, _}} = :timer.tc(fn -> File.read(__ENV__.file) end, :millisecond)
      assert elapsed < 500

      assert Enum.all?(Task.await_many(tasks, 5_000), &(&1 == {:error, "timeout"}))
      Enum.each(runtimes, &Quicksand.stop/1)
    end
  end

  describe "isolation" do
    test "separate runtimes are isolated" do
      {:ok, rt1} = Quicksand.start()
      {:ok, rt2} = Quicksand.start()

      Quicksand.eval(rt1, "globalThis.shared = 'from_rt1'")
      assert {:error, _} = Quicksand.eval(rt2, "shared")

      Quicksand.stop(rt1)
      Quicksand.stop(rt2)
    end
  end

  describe "property-based: type round-trip" do
    setup do
      {:ok, rt} = Quicksand.start()
      on_exit(fn -> Quicksand.stop(rt) end)
      %{rt: rt}
    end

    # Generator for values that survive Elixir → JS → Elixir round-trip
    defp js_safe_value do
      tree(js_leaf(), fn leaf ->
        one_of([
          list_of(leaf, max_length: 5),
          map_of(string(:alphanumeric, min_length: 1, max_length: 8), leaf, max_length: 5)
        ])
      end)
    end

    defp js_leaf do
      one_of([
        constant(nil),
        boolean(),
        integer(-1_000_000..1_000_000),
        float(),
        string(:printable, max_length: 50)
      ])
    end

    property "eval round-trips Elixir values through JS callbacks", %{rt: rt} do
      check all(value <- js_safe_value(), max_runs: 200) do
        callbacks = %{"echo" => fn [v] -> {:ok, v} end}
        {:ok, result} = Quicksand.eval(rt, "echo(#{js_encode(value)})", callbacks)
        assert js_equal?(value, result)
      end
    end

    property "callback return values survive JS → Elixir", %{rt: rt} do
      check all(value <- js_safe_value(), max_runs: 200) do
        callbacks = %{"get" => fn [] -> {:ok, value} end}
        {:ok, result} = Quicksand.eval(rt, "get()", callbacks)
        assert js_equal?(value, result)
      end
    end

    # Encode Elixir value as JS literal for eval
    defp js_encode(nil), do: "null"
    defp js_encode(true), do: "true"
    defp js_encode(false), do: "false"
    defp js_encode(n) when is_integer(n), do: Integer.to_string(n)

    defp js_encode(f) when is_float(f) do
      Float.to_string(f)
    end

    defp js_encode(s) when is_binary(s) do
      escaped =
        s
        |> String.replace("\\", "\\\\")
        |> String.replace("\"", "\\\"")
        |> String.replace("\n", "\\n")
        |> String.replace("\r", "\\r")

      "\"#{escaped}\""
    end

    defp js_encode(list) when is_list(list) do
      "[#{Enum.map_join(list, ",", &js_encode/1)}]"
    end

    defp js_encode(map) when is_map(map) do
      pairs = Enum.map_join(map, ",", fn {k, v} -> "#{js_encode(k)}:#{js_encode(v)}" end)
      "({#{pairs}})"
    end

    # Compare values accounting for JS type coercion:
    # - JS doesn't distinguish int/float, so 1.0 == 1
    # - NaN/Infinity become nil
    defp js_equal?(a, b) when is_float(a) and is_integer(b) do
      Float.round(a, 0) == a and trunc(a) == b
    end

    defp js_equal?(a, b) when is_integer(a) and is_float(b) do
      Float.round(b, 0) == b and a == trunc(b)
    end

    defp js_equal?(a, b) when is_float(a) and is_float(b) do
      abs(a - b) < 1.0e-9
    end

    defp js_equal?(a, b) when is_list(a) and is_list(b) do
      length(a) == length(b) and Enum.all?(Enum.zip(a, b), fn {x, y} -> js_equal?(x, y) end)
    end

    defp js_equal?(a, b) when is_map(a) and is_map(b) do
      Map.keys(a) == Map.keys(b) and
        Enum.all?(Map.keys(a), fn k -> js_equal?(Map.get(a, k), Map.get(b, k)) end)
    end

    defp js_equal?(a, b), do: a === b
  end
end
