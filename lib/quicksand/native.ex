defmodule Quicksand.Native do
  @moduledoc false

  version = Mix.Project.config()[:version]

  use RustlerPrecompiled,
    otp_app: :quicksand,
    crate: "quicksand",
    base_url: "https://github.com/lpgauth/quicksand/releases/download/v#{version}",
    force_build: System.get_env("QUICKSAND_BUILD") in ["1", "true"],
    targets: ~w(
      aarch64-apple-darwin
      aarch64-unknown-linux-gnu
      aarch64-unknown-linux-musl
      x86_64-apple-darwin
      x86_64-unknown-linux-gnu
      x86_64-unknown-linux-musl
    ),
    nif_versions: ["2.15"],
    version: version

  def start_runtime(_ref, _timeout_ms, _memory_limit, _max_stack_size),
    do: :erlang.nif_error(:nif_not_loaded)

  def eval(_resource, _code, _callback_names, _tag),
    do: :erlang.nif_error(:nif_not_loaded)

  def respond_callback(_resource, _callback_id, _result),
    do: :erlang.nif_error(:nif_not_loaded)

  def abandon(_resource, _tag),
    do: :erlang.nif_error(:nif_not_loaded)

  def is_alive(_resource),
    do: :erlang.nif_error(:nif_not_loaded)

  def stop_runtime(_resource),
    do: :erlang.nif_error(:nif_not_loaded)
end
