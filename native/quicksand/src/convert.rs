//! Conversion between QuickJS values and Erlang terms.
//!
//! JS to Erlang happens on the worker thread in one step. Erlang to JS takes
//! two: NIF threads can read terms but not JS values, and the worker thread
//! the reverse, so terms are first decoded into a `TermValue`.

use std::borrow::Cow;
use std::mem;

use rquickjs::function::This;
use rquickjs::object::Property;
use rquickjs::{Array, Coerced, Ctx, FromJs, Function, IntoAtom, Object, Value};
use rustler::types::atom;
use rustler::{BigInt, Encoder, Env, MapIterator, Term};

use crate::interrupt::Interrupt;

/// Containers nested deeper than this fail to convert, which also stops cycles.
const MAX_DEPTH: u32 = 64;

/// The largest integer a JS number holds exactly (`Number.MAX_SAFE_INTEGER`).
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// How many values to convert between checks of the eval's deadline.
const INTERRUPT_CHECK_INTERVAL: usize = 1024;

const OUT_OF_MEMORY: &str = "out of memory";

/// A callback's reply: the value to return to JS, or the message to throw.
pub type CallbackReply = Result<TermValue, String>;

// ── JS → Erlang ─────────────────────────────────────────────────────────────

/// A JS value copied out of the runtime, ready to encode as an Erlang term.
#[derive(Clone, Debug)]
pub enum JsValue {
    Null,
    Bool(bool),
    Int(i64),
    BigInt(BigInt),
    Float(f64),
    String(String),
    Array(Vec<JsValue>),
    Object(Vec<(String, JsValue)>),
}

impl Encoder for JsValue {
    fn encode<'a>(&self, env: Env<'a>) -> Term<'a> {
        match self {
            JsValue::Null => atom::nil().encode(env),
            JsValue::Bool(boolean) => boolean.encode(env),
            JsValue::Int(integer) => integer.encode(env),
            JsValue::BigInt(integer) => integer.encode(env),
            JsValue::Float(float) => float.encode(env),
            JsValue::String(string) => string.encode(env),
            JsValue::Array(items) => items.encode(env),
            JsValue::Object(pairs) => {
                let keys: Vec<Term> = pairs.iter().map(|(key, _)| key.encode(env)).collect();
                let values: Vec<Term> = pairs.iter().map(|(_, value)| value.encode(env)).collect();
                Term::map_from_arrays(env, &keys, &values)
                    .unwrap_or_else(|_| atom::nil().encode(env))
            }
        }
    }
}

/// Copies JS values out of the runtime. Every value is charged against a byte
/// budget, so results count toward the memory limit, and the conversion stops
/// when the eval is interrupted. Without both, a sparse array or a value that
/// shares references could expand far past the JS heap.
pub struct Converter<'a, 'js> {
    ctx: &'a Ctx<'js>,
    interrupt: &'a Interrupt,
    budget: usize,
    converted: usize,
}

impl<'a, 'js> Converter<'a, 'js> {
    pub fn new(ctx: &'a Ctx<'js>, interrupt: &'a Interrupt, budget: usize) -> Self {
        Self {
            ctx,
            interrupt,
            budget,
            converted: 0,
        }
    }

    pub fn convert(&mut self, value: Value<'js>) -> Result<JsValue, String> {
        self.convert_at_depth(value, 0)
    }

    fn convert_at_depth(&mut self, value: Value<'js>, depth: u32) -> Result<JsValue, String> {
        if depth > MAX_DEPTH {
            return Err("Maximum nesting depth exceeded".to_string());
        }
        self.charge(mem::size_of::<JsValue>())?;

        if value.is_undefined() || value.is_null() || value.is_function() || value.is_symbol() {
            return Ok(JsValue::Null);
        }

        if let Some(boolean) = value.as_bool() {
            return Ok(JsValue::Bool(boolean));
        }

        if let Some(integer) = value.as_int() {
            return Ok(JsValue::Int(i64::from(integer)));
        }

        if let Some(float) = value.as_float() {
            return Ok(float_to_term(float));
        }

        if value.is_big_int() {
            return self.big_int(value);
        }

        if let Some(string) = value.as_string() {
            return self.string(string).map(JsValue::String);
        }

        let Some(object) = value.as_object() else {
            return Ok(JsValue::Null);
        };

        if value.is_array() {
            self.array(object, depth).map(JsValue::Array)
        } else {
            self.object(object, depth).map(JsValue::Object)
        }
    }

    fn charge(&mut self, bytes: usize) -> Result<(), String> {
        self.budget = self
            .budget
            .checked_sub(bytes)
            .ok_or_else(|| OUT_OF_MEMORY.to_string())?;
        self.converted += 1;
        if self.converted.is_multiple_of(INTERRUPT_CHECK_INTERVAL) && self.interrupt.is_due() {
            return Err("interrupted".to_string());
        }
        Ok(())
    }

    fn string(&mut self, string: &rquickjs::String<'js>) -> Result<String, String> {
        let string = to_rust_string(self.ctx, string).map_err(|error| self.describe(error))?;
        self.charge(string.len())?;
        Ok(string)
    }

    fn big_int(&mut self, value: Value<'js>) -> Result<JsValue, String> {
        let Coerced(digits) =
            Coerced::<String>::from_js(self.ctx, value).map_err(|error| self.describe(error))?;
        self.charge(digits.len())?;
        digits
            .parse::<BigInt>()
            .map(JsValue::BigInt)
            .map_err(|error| format!("Failed to convert BigInt {digits}: {error}"))
    }

    fn array(&mut self, array: &Object<'js>, depth: u32) -> Result<Vec<JsValue>, String> {
        let length = self.property(array, "length")?;
        let length = length.as_number().map_or(0, array_length);
        let mut items = Vec::with_capacity(length.min(1024) as usize);
        for index in 0..length {
            let item = self.property(array, index)?;
            items.push(self.convert_at_depth(item, depth + 1)?);
        }
        Ok(items)
    }

    fn object(
        &mut self,
        object: &Object<'js>,
        depth: u32,
    ) -> Result<Vec<(String, JsValue)>, String> {
        let mut pairs = Vec::new();
        for key in object.keys::<rquickjs::String>() {
            let key = key.map_err(|error| self.describe(error))?;
            let value = self.property(object, key.clone())?;
            if !value.is_function() {
                let name = self.string(&key)?;
                pairs.push((name, self.convert_at_depth(value, depth + 1)?));
            }
        }
        Ok(pairs)
    }

    fn property<K: IntoAtom<'js>>(
        &self,
        object: &Object<'js>,
        key: K,
    ) -> Result<Value<'js>, String> {
        object
            .get(key)
            .map_err(|error| format!("Failed to read property: {}", self.describe(error)))
    }

    fn describe(&self, error: rquickjs::Error) -> String {
        describe_error(self.ctx, error)
    }
}

fn float_to_term(float: f64) -> JsValue {
    if !float.is_finite() {
        JsValue::Null
    } else if float.fract() == 0.0 && float.abs() <= MAX_SAFE_INTEGER {
        JsValue::Int(float as i64)
    } else {
        JsValue::Float(float)
    }
}

fn array_length(length: f64) -> u32 {
    if length.is_finite() && length > 0.0 {
        length.min(f64::from(u32::MAX)) as u32
    } else {
        0
    }
}

/// Converts a JS string to UTF-8, replacing lone surrogates with U+FFFD.
fn to_rust_string<'js>(ctx: &Ctx<'js>, string: &rquickjs::String<'js>) -> rquickjs::Result<String> {
    if let Ok(converted) = string.to_string() {
        return Ok(converted);
    }
    let prototype: Object = ctx.globals().get::<_, Object>("String")?.get("prototype")?;
    let to_well_formed: Function = prototype.get("toWellFormed")?;
    let well_formed: rquickjs::String = to_well_formed.call((This(string.clone()),))?;
    well_formed.to_string()
}

/// Describes an rquickjs error, taking the pending exception if it is one.
pub fn describe_error(ctx: &Ctx<'_>, error: rquickjs::Error) -> String {
    if error.is_exception() {
        describe_thrown(ctx, ctx.catch())
    } else {
        error.to_string()
    }
}

/// Describes a thrown JS value as an `{:error, message}` string: an `Error`'s
/// message and stack, a string as itself, and anything else as JSON or its
/// `String()` form.
pub fn describe_thrown<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> String {
    // QuickJS throws null when it cannot allocate the out-of-memory error
    // itself. A script throwing null literally is reported the same way.
    if value.is_null() {
        return OUT_OF_MEMORY.to_string();
    }

    if let Some(string) = value.as_string() {
        return to_rust_string(ctx, string).unwrap_or_default();
    }

    if let Some(object) = value.as_object().filter(|_| value.is_error()) {
        let mut message = string_property(ctx, object, "message");
        if message.is_empty() {
            message = string_property(ctx, object, "name");
        }
        let stack = string_property(ctx, object, "stack");
        return if stack.is_empty() {
            message
        } else {
            format!("{message}\n{stack}")
        };
    }

    if value.is_object() {
        match ctx.json_stringify(value.clone()) {
            Ok(Some(json)) => {
                if let Ok(json) = to_rust_string(ctx, &json) {
                    return json;
                }
            }
            Ok(None) => {}
            Err(_) => {
                ctx.catch();
            }
        }
    }

    match Coerced::<String>::from_js(ctx, value.clone()) {
        Ok(Coerced(string)) => string,
        Err(_) => {
            ctx.catch();
            value.type_name().to_string()
        }
    }
}

/// Reads `key` as a string, coercing non-string values the way `String()`
/// does. Missing values and failures read as empty.
fn string_property<'js>(ctx: &Ctx<'js>, object: &Object<'js>, key: &str) -> String {
    let value = match object.get::<_, Value>(key) {
        Ok(value) => value,
        Err(_) => {
            ctx.catch();
            return String::new();
        }
    };
    if value.is_undefined() || value.is_null() {
        return String::new();
    }
    if let Some(string) = value.as_string() {
        return to_rust_string(ctx, string).unwrap_or_default();
    }
    match Coerced::<String>::from_js(ctx, value) {
        Ok(Coerced(string)) => string,
        Err(_) => {
            ctx.catch();
            String::new()
        }
    }
}

// ── Erlang → JS ─────────────────────────────────────────────────────────────

/// An Erlang term decoded on a NIF thread, ready to become a JS value on the
/// worker thread.
#[derive(Clone, Debug)]
pub enum TermValue {
    Nil,
    Bool(bool),
    Int(i64),
    BigInt(BigInt),
    Float(f64),
    String(String),
    Atom(String),
    List(Vec<TermValue>),
    Map(Vec<(TermValue, TermValue)>),
}

pub fn term_to_intermediate(term: Term<'_>) -> Result<TermValue, String> {
    term_to_intermediate_at_depth(term, 0)
}

fn term_to_intermediate_at_depth(term: Term<'_>, depth: u32) -> Result<TermValue, String> {
    if depth > MAX_DEPTH {
        return Err("Maximum nesting depth exceeded".to_string());
    }

    if term.is_atom() {
        let name = term
            .atom_to_string()
            .map_err(|_| "Failed to convert atom".to_string())?;
        return Ok(match name.as_str() {
            "nil" => TermValue::Nil,
            "true" => TermValue::Bool(true),
            "false" => TermValue::Bool(false),
            _ => TermValue::Atom(name),
        });
    }

    if let Ok(integer) = term.decode::<i64>() {
        return Ok(TermValue::Int(integer));
    }

    if let Ok(integer) = term.decode::<BigInt>() {
        return Ok(TermValue::BigInt(integer));
    }

    if let Ok(float) = term.decode::<f64>() {
        return Ok(TermValue::Float(float));
    }

    if term.is_binary() {
        let string: String = term
            .decode()
            .map_err(|_| "Failed to decode binary as UTF-8 string".to_string())?;
        return Ok(TermValue::String(string));
    }

    if term.is_list() {
        let items: Vec<Term> = term
            .decode()
            .map_err(|_| "Failed to decode list".to_string())?;
        let mut values = Vec::with_capacity(items.len());
        for item in items {
            values.push(term_to_intermediate_at_depth(item, depth + 1)?);
        }
        return Ok(TermValue::List(values));
    }

    if term.is_map() {
        let entries: MapIterator = term
            .decode()
            .map_err(|_| "Failed to decode map".to_string())?;
        let mut pairs = Vec::new();
        for (key, value) in entries {
            let key = term_to_intermediate_at_depth(key, depth + 1)?;
            let value = term_to_intermediate_at_depth(value, depth + 1)?;
            pairs.push((key, value));
        }
        return Ok(TermValue::Map(pairs));
    }

    Err("Unsupported Elixir term type".to_string())
}

pub fn intermediate_to_js<'js>(
    ctx: &Ctx<'js>,
    term_value: &TermValue,
) -> Result<Value<'js>, String> {
    intermediate_to_js_at_depth(ctx, term_value, 0)
}

fn intermediate_to_js_at_depth<'js>(
    ctx: &Ctx<'js>,
    term_value: &TermValue,
    depth: u32,
) -> Result<Value<'js>, String> {
    if depth > MAX_DEPTH {
        return Err("Maximum nesting depth exceeded".to_string());
    }

    match term_value {
        TermValue::Nil => Ok(Value::new_null(ctx.clone())),
        TermValue::Bool(boolean) => Ok(Value::new_bool(ctx.clone(), *boolean)),
        TermValue::Int(integer) => Ok(match i32::try_from(*integer) {
            Ok(small) => Value::new_int(ctx.clone(), small),
            Err(_) => Value::new_float(ctx.clone(), *integer as f64),
        }),
        TermValue::BigInt(integer) => {
            let construct: Function = ctx
                .globals()
                .get("BigInt")
                .map_err(|error| describe_error(ctx, error))?;
            construct
                .call((integer.to_string(),))
                .map_err(|error| format!("Failed to create BigInt: {}", describe_error(ctx, error)))
        }
        TermValue::Float(float) => Ok(Value::new_float(ctx.clone(), *float)),
        TermValue::String(string) | TermValue::Atom(string) => {
            let string = rquickjs::String::from_str(ctx.clone(), string)
                .map_err(|error| format!("Failed to create JS string: {error}"))?;
            Ok(string.into_value())
        }
        TermValue::List(items) => {
            let array = Array::new(ctx.clone())
                .map_err(|error| format!("Failed to create JS array: {error}"))?;
            for (index, item) in items.iter().enumerate() {
                let value = intermediate_to_js_at_depth(ctx, item, depth + 1)?;
                array
                    .set(index, value)
                    .map_err(|error| format!("Failed to set array element: {error}"))?;
            }
            Ok(array.into_value())
        }
        TermValue::Map(pairs) => {
            let object = Object::new(ctx.clone())
                .map_err(|error| format!("Failed to create JS object: {error}"))?;
            for (key, value) in pairs {
                let key: Cow<'_, str> = match key {
                    TermValue::String(string) | TermValue::Atom(string) => Cow::Borrowed(string),
                    TermValue::Int(integer) => Cow::Owned(integer.to_string()),
                    _ => return Err("Map keys must be strings, atoms, or integers".to_string()),
                };
                let value = intermediate_to_js_at_depth(ctx, value, depth + 1)?;
                // Defined rather than assigned, so a `__proto__` key becomes an
                // own property (as with JSON.parse) instead of the prototype.
                let property = Property::from(value).writable().enumerable().configurable();
                object
                    .prop(key.as_ref(), property)
                    .map_err(|error| format!("Failed to set object property: {error}"))?;
            }
            Ok(object.into_value())
        }
    }
}
