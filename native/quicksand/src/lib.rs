//! QuickJS-NG runtimes for the `Quicksand` Elixir library.
//!
//! Each runtime is owned by a `worker` thread. The NIF calls in `nif` talk to
//! it over a channel, and `convert` translates between JS values and Erlang
//! terms.

mod atoms;
mod convert;
mod interrupt;
mod nif;
mod runtime;
mod worker;

rustler::init!("Elixir.Quicksand.Native");
