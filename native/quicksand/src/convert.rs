//! Conversion between QuickJS values and Erlang terms.
//!
//! JS to Erlang happens on the worker thread in one step. Erlang to JS takes
//! two: NIF threads can read terms but not JS values, and the worker thread
//! the reverse, so terms are first decoded into a `TermValue`.

use std::borrow::Cow;

use rquickjs::{Array, Ctx, Object, Value};
use rustler::types::atom;
use rustler::{Encoder, Env, MapIterator, Term};

/// Containers nested deeper than this fail to convert, which also stops cycles.
const MAX_DEPTH: u32 = 64;

/// The largest integer a JS number holds exactly (`Number.MAX_SAFE_INTEGER`).
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// A callback's reply: the value to return to JS, or the message to throw.
pub type CallbackReply = Result<TermValue, String>;

// ── JS → Erlang ─────────────────────────────────────────────────────────────

/// A JS value copied out of the runtime, ready to encode as an Erlang term.
#[derive(Clone, Debug)]
pub enum JsValue {
    Null,
    Bool(bool),
    Int(i64),
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

pub fn js_to_term<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> Result<JsValue, String> {
    js_to_term_at_depth(ctx, value, 0)
}

fn js_to_term_at_depth<'js>(
    ctx: &Ctx<'js>,
    value: Value<'js>,
    depth: u32,
) -> Result<JsValue, String> {
    if depth > MAX_DEPTH {
        return Err("Maximum nesting depth exceeded".to_string());
    }

    if value.is_undefined() || value.is_null() || value.is_function() {
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

    if let Some(string) = value.as_string() {
        let string = string
            .to_string()
            .map_err(|error| format!("String conversion error: {error}"))?;
        return Ok(JsValue::String(string));
    }

    let Some(object) = value.as_object() else {
        return Ok(JsValue::Null);
    };

    if value.is_array() {
        array_to_term(ctx, object, depth).map(JsValue::Array)
    } else {
        object_to_term(ctx, object, depth).map(JsValue::Object)
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

fn array_to_term<'js>(
    ctx: &Ctx<'js>,
    array: &Object<'js>,
    depth: u32,
) -> Result<Vec<JsValue>, String> {
    let length: i32 = array.get("length").unwrap_or(0);
    let length = u32::try_from(length).unwrap_or(0);
    let mut items = Vec::with_capacity(length.min(1024) as usize);
    for index in 0..length {
        let item: Value = array
            .get(index)
            .unwrap_or_else(|_| Value::new_undefined(ctx.clone()));
        items.push(js_to_term_at_depth(ctx, item, depth + 1)?);
    }
    Ok(items)
}

fn object_to_term<'js>(
    ctx: &Ctx<'js>,
    object: &Object<'js>,
    depth: u32,
) -> Result<Vec<(String, JsValue)>, String> {
    let mut pairs = Vec::new();
    let keys = object.keys::<String>().flatten();
    for key in keys {
        let value: Value = object
            .get(key.as_str())
            .unwrap_or_else(|_| Value::new_undefined(ctx.clone()));
        if !value.is_function() {
            pairs.push((key, js_to_term_at_depth(ctx, value, depth + 1)?));
        }
    }
    Ok(pairs)
}

// ── Erlang → JS ─────────────────────────────────────────────────────────────

/// An Erlang term decoded on a NIF thread, ready to become a JS value on the
/// worker thread.
#[derive(Clone, Debug)]
pub enum TermValue {
    Nil,
    Bool(bool),
    Int(i64),
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
                object
                    .set(key.as_ref(), value)
                    .map_err(|error| format!("Failed to set object property: {error}"))?;
            }
            Ok(object.into_value())
        }
    }
}
