use serde::{Deserialize, Serialize};

/// Wire-level description of a tool. JSON-schema describes the input. The
/// `Tool` trait that implements behavior lives in `wingman-tools` to keep
/// `wingman-core` free of any IO concerns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the input object. Providers translate to their
    /// preferred wire format (Anthropic accepts JSON Schema directly).
    pub input_schema: serde_json::Value,
}

/// Serialize `v` with object keys in sorted order, so two calls whose
/// arguments differ only in property order produce the same key.
///
/// `serde_json`'s `Map` is a `BTreeMap` today and would sort anyway, but that
/// is a default-feature accident: any crate in the dependency graph enabling
/// `serde_json/preserve_order` flips it to insertion order for *everyone*,
/// and both repetition guards would quietly stop matching. Sorting explicitly
/// costs a few lines and does not depend on a transitive feature flag.
///
/// Array order is meaningful and is preserved.
///
/// Lives here rather than in `wingman-tools` because both layers that count
/// repeated calls — the tools-layer consecutive nudge and
/// [`LoopGuard`](crate::loopguard::LoopGuard) — have to agree on what "the
/// same call" means, and two copies of this function would eventually not.
pub fn canonical_args(v: &serde_json::Value) -> String {
    use serde_json::Value;
    fn write(v: &Value, out: &mut String) {
        match v {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort_unstable();
                out.push('{');
                for (i, k) in keys.into_iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&Value::String(k.clone()).to_string());
                    out.push(':');
                    write(&map[k], out);
                }
                out.push('}');
            }
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(item, out);
                }
                out.push(']');
            }
            scalar => out.push_str(&scalar.to_string()),
        }
    }
    let mut out = String::new();
    write(v, &mut out);
    out
}
