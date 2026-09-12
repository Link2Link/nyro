//! Bounded, deliberately conservative lowering for Gemini tool `parameters`.
//!
//! This is not the native Schema field allowlist. The encoder must run its
//! schema-aware deletion cleanup *after* this pass. In particular, definition
//! catalogs and unsupported non-composition keywords are retained here. Only
//! referenced definitions are interpreted; unused catalogs are inert data.

use std::error::Error;
use std::fmt;

use serde_json::{Map, Value};

const MAX_DEPTH: usize = 64;
const MAX_EXPANDED_NODES: usize = 10_000;

type LoweringResult<T> = Result<T, SchemaLoweringError>;

/// A rejection local to a tool's parameter schema, not a generic codec failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SchemaLoweringError {
    /// JSON Pointer fragment rooted at `#`, with escaped property names. Paths
    /// through an expanded reference describe the expanded use site.
    pub path: String,
    pub reason: String,
}

impl fmt::Display for SchemaLoweringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unsupported Gemini parameter schema at {}: {}",
            self.path, self.reason
        )
    }
}

impl Error for SchemaLoweringError {}

/// Resolve local references and lower provably representable compositions.
///
/// All references resolve against the original parameter root, never a partially
/// lowered copy. No network or filesystem resolver exists. A reference stack is
/// local to the current expansion path, allowing independent sibling reuse.
///
/// Limits include both schema and copied literal nodes: root depth is zero,
/// each JSON child or reference expansion adds one, depth 64 is inclusive, and
/// at most 10,000 expanded/copied nodes are visited. Removed composition wrappers
/// and preserved definition catalogs also consume this conservative work budget.
/// This bounds intermediate work as well as the final output, without cloning an
/// unbounded input subtree before checking it.
///
/// Historical non-boolean primitive values are passed through, not replaced by
/// `{}`. Boolean *schemas* are rejected because the native Schema channel cannot
/// encode them; booleans in literal data or cleanup-owned keyword values survive.
pub(super) fn lower_parameters(schema: &Value) -> LoweringResult<Value> {
    Lowerer {
        root: schema,
        reference_stack: Vec::new(),
        expanded_nodes: 0,
    }
    .lower(schema, "#", 0)
}

struct Lowerer<'a> {
    root: &'a Value,
    reference_stack: Vec<&'a Value>,
    expanded_nodes: usize,
}

impl<'a> Lowerer<'a> {
    fn visit(&mut self, path: &str, depth: usize) -> LoweringResult<()> {
        if depth > MAX_DEPTH {
            return Err(reject(
                path,
                format!("maximum lowering depth {MAX_DEPTH} exceeded"),
            ));
        }
        if self.expanded_nodes >= MAX_EXPANDED_NODES {
            return Err(reject(
                path,
                format!("maximum expanded node count {MAX_EXPANDED_NODES} exceeded"),
            ));
        }
        self.expanded_nodes += 1;
        Ok(())
    }

    /// Copy annotations and literal payloads without interpreting their keys.
    /// Still walk for resource accounting, not for schema transformations.
    fn copy_literal(&mut self, value: &Value, path: &str, depth: usize) -> LoweringResult<Value> {
        self.visit(path, depth)?;
        self.copy_literal_children(value, path, depth)
    }

    fn copy_literal_children(
        &mut self,
        value: &Value,
        path: &str,
        depth: usize,
    ) -> LoweringResult<Value> {
        match value {
            Value::Object(map) => {
                let mut out = Map::new();
                for (key, value) in map {
                    out.insert(
                        key.clone(),
                        self.copy_literal(value, &child(path, key), depth + 1)?,
                    );
                }
                Ok(Value::Object(out))
            }
            Value::Array(values) => {
                let mut out = Vec::new();
                for (index, value) in values.iter().enumerate() {
                    out.push(self.copy_literal(
                        value,
                        &child(path, &index.to_string()),
                        depth + 1,
                    )?);
                }
                Ok(Value::Array(out))
            }
            _ => Ok(value.clone()),
        }
    }

    fn lower(&mut self, schema: &Value, path: &str, depth: usize) -> LoweringResult<Value> {
        self.visit(path, depth)?;
        let map = match schema {
            Value::Object(map) => map,
            Value::Bool(_) => {
                return Err(reject(
                    path,
                    "boolean schemas cannot be represented by native Gemini parameters",
                ));
            }
            _ => return self.copy_literal_children(schema, path, depth),
        };

        for key in [
            "not",
            "if",
            "then",
            "else",
            "$dynamicRef",
            "$recursiveRef",
            "prefixItems",
        ] {
            if map.contains_key(key) {
                return Err(reject(
                    &child(path, key),
                    format!("{key} cannot be lowered without losing schema semantics"),
                ));
            }
        }

        let mut siblings = Map::new();
        for (key, value) in map {
            if matches!(key.as_str(), "$ref" | "ref" | "allOf" | "oneOf" | "const") {
                continue;
            }
            let field_path = child(path, key);
            let lowered = match key.as_str() {
                // These are maps of author-chosen names, not schema keyword maps.
                "properties" | "patternProperties" | "dependentSchemas" => {
                    self.visit(&field_path, depth + 1)?;
                    let entries = value.as_object().ok_or_else(|| {
                        reject(&field_path, "expected a map of property names to schemas")
                    })?;
                    let mut out = Map::new();
                    for (name, value) in entries {
                        out.insert(
                            name.clone(),
                            self.lower(value, &child(&field_path, name), depth + 2)?,
                        );
                    }
                    Value::Object(out)
                }
                "anyOf" => {
                    self.visit(&field_path, depth + 1)?;
                    let branches = nonempty_branches(value, &field_path)?;
                    let mut out = Vec::new();
                    for (index, branch) in branches.iter().enumerate() {
                        let branch_path = child(&field_path, &index.to_string());
                        if !branch.is_object() {
                            return Err(reject(
                                &branch_path,
                                "anyOf branches must be object schemas",
                            ));
                        }
                        out.push(self.lower(branch, &branch_path, depth + 2)?);
                    }
                    Value::Array(out)
                }
                "items" | "contains" | "propertyNames" => {
                    if value.is_array() {
                        return Err(reject(
                            &field_path,
                            "tuple schemas cannot be represented by native Gemini parameters",
                        ));
                    }
                    self.lower(value, &field_path, depth + 1)?
                }
                "additionalProperties"
                | "additionalItems"
                | "unevaluatedProperties"
                | "unevaluatedItems" => {
                    if value.is_object() {
                        self.lower(value, &field_path, depth + 1)?
                    } else {
                        self.copy_literal(value, &field_path, depth + 1)?
                    }
                }
                // Includes $defs/definitions, default, enum, examples, required,
                // and extensions: none of their inner keys are schema keywords.
                _ => self.copy_literal(value, &field_path, depth + 1)?,
            };
            siblings.insert(key.clone(), lowered);
        }

        if let Some(constant) = map.get("const") {
            let const_path = child(path, "const");
            if !constant.is_string() {
                return Err(reject(
                    &const_path,
                    "only string const values have a supported native enum lowering",
                ));
            }
            reject_nullable_constant(&siblings, path)?;
            self.visit(&child(path, "type"), depth + 1)?;
            self.visit(&child(path, "enum"), depth + 1)?;
            let constant =
                self.copy_literal(constant, &child(&child(path, "enum"), "0"), depth + 2)?;
            let mut constant_schema = Map::new();
            constant_schema.insert("type".into(), Value::String("string".into()));
            constant_schema.insert("enum".into(), Value::Array(vec![constant]));
            siblings = merge(siblings, constant_schema, &const_path)?;
        }

        if let Some(all_of) = map.get("allOf") {
            let all_of_path = child(path, "allOf");
            self.visit(&all_of_path, depth + 1)?;
            let branches = nonempty_branches(all_of, &all_of_path)?;
            if !object_intersection_branch(&siblings) {
                return Err(reject(
                    &all_of_path,
                    "allOf lowering only supports compatible object schemas",
                ));
            }
            for (index, branch) in branches.iter().enumerate() {
                let branch_path = child(&all_of_path, &index.to_string());
                if !branch.is_object() {
                    return Err(reject(
                        &branch_path,
                        "allOf branches must be object schemas",
                    ));
                }
                let lowered = self.lower(branch, &branch_path, depth + 2)?;
                let Value::Object(branch) = lowered else {
                    unreachable!("object lowering returns an object")
                };
                if !object_intersection_branch(&branch) {
                    return Err(reject(
                        &branch_path,
                        "allOf lowering only supports compatible object schemas",
                    ));
                }
                siblings = merge(siblings, branch, &branch_path)?;
            }
            // Even a single branch contributes a set of required names. Keep
            // its first-occurrence ordering instead of preserving duplicates.
            if let Some(required) = siblings.get_mut("required") {
                let required_path = child(path, "required");
                let names = required.as_array_mut().ok_or_else(|| {
                    reject(&required_path, "required must be an array of strings")
                })?;
                if names.iter().any(|name| !name.is_string()) {
                    return Err(reject(
                        &required_path,
                        "required must be an array of strings",
                    ));
                }
                let mut union = Vec::new();
                for name in std::mem::take(names) {
                    if !union.contains(&name) {
                        union.push(name);
                    }
                }
                *names = union;
            }
        }

        if let Some(one_of) = map.get("oneOf") {
            let one_of_path = child(path, "oneOf");
            self.visit(&one_of_path, depth + 1)?;
            let branches = nonempty_branches(one_of, &one_of_path)?;
            let mut constants = Vec::new();
            let mut common_constraints: Option<Map<String, Value>> = None;
            for (index, branch) in branches.iter().enumerate() {
                let branch_path = child(&one_of_path, &index.to_string());
                if !branch.is_object() {
                    return Err(reject(
                        &branch_path,
                        "oneOf alternatives must be distinct string constants",
                    ));
                }
                let lowered = self.lower(branch, &branch_path, depth + 2)?;
                let Value::Object(mut branch) = lowered else {
                    unreachable!("object lowering returns an object")
                };
                if branch.remove("type") != Some(Value::String("string".into())) {
                    return Err(reject(
                        &branch_path,
                        "oneOf alternatives must be distinct string constants",
                    ));
                }
                let Some(Value::Array(mut values)) = branch.remove("enum") else {
                    return Err(reject(
                        &branch_path,
                        "oneOf alternatives must be distinct string constants",
                    ));
                };
                if values.len() != 1 || !values[0].is_string() {
                    return Err(reject(
                        &branch_path,
                        "oneOf alternatives must be distinct string constants",
                    ));
                }
                let constant = values.remove(0);
                if constants.contains(&constant) {
                    return Err(reject(
                        &branch_path,
                        "duplicate oneOf constants are not equivalent to an enum",
                    ));
                }
                // Mutually exclusive singletons allow *identical* residual
                // constraints/annotations to be hoisted, never discarded.
                match &common_constraints {
                    Some(common) if common != &branch => {
                        return Err(reject(
                            &branch_path,
                            "oneOf alternatives have differing constraints or annotations",
                        ));
                    }
                    None => common_constraints = Some(branch),
                    _ => {}
                }
                constants.push(constant);
            }
            let mut alternatives = common_constraints.expect("nonempty branches checked above");
            reject_nullable_constant(&alternatives, &one_of_path)?;
            reject_nullable_constant(&siblings, path)?;
            alternatives.insert("type".into(), Value::String("string".into()));
            alternatives.insert("enum".into(), Value::Array(constants));
            siblings = merge(siblings, alternatives, &one_of_path)?;
        }

        let mut referenced = Map::new();
        for key in ["$ref", "ref"] {
            if let Some(reference) = map.get(key) {
                let ref_path = child(path, key);
                self.visit(&ref_path, depth + 1)?;
                let reference = reference.as_str().ok_or_else(|| {
                    reject(&ref_path, "reference must be a local JSON Pointer string")
                })?;
                let target = self.resolve(reference, &ref_path)?;
                if self
                    .reference_stack
                    .iter()
                    .any(|active| std::ptr::eq(*active, target))
                {
                    return Err(reject(
                        &ref_path,
                        format!("circular reference {reference:?}"),
                    ));
                }
                if !target.is_object() {
                    return Err(reject(
                        &ref_path,
                        format!("reference {reference:?} does not target an object schema"),
                    ));
                }
                self.reference_stack.push(target);
                let lowered = self.lower(target, path, depth + 1);
                self.reference_stack.pop();
                let Value::Object(target) = lowered? else {
                    unreachable!("object lowering returns an object")
                };
                referenced = merge(referenced, target, &ref_path)?;
            }
        }
        Ok(Value::Object(merge(referenced, siblings, path)?))
    }

    fn resolve(&self, reference: &str, path: &str) -> LoweringResult<&'a Value> {
        let Some(fragment) = reference.strip_prefix('#') else {
            return Err(reject(
                path,
                format!(
                    "external reference {reference:?} is unsupported; no external fetching is performed"
                ),
            ));
        };
        let pointer = decode_fragment(fragment).ok_or_else(|| {
            reject(
                path,
                format!("invalid JSON Pointer URI fragment {reference:?}"),
            )
        })?;
        if !pointer.is_empty() && !pointer.starts_with('/') {
            return Err(reject(
                path,
                format!("reference {reference:?} is not a local JSON Pointer"),
            ));
        }
        // serde_json::Value::pointer handles ~0, ~1, and strict array indices;
        // validate escapes first because invalid ~ sequences are otherwise literal.
        for token in pointer.split('/').skip(1) {
            let mut chars = token.chars();
            while let Some(ch) = chars.next() {
                if ch == '~' && !matches!(chars.next(), Some('0' | '1')) {
                    return Err(reject(
                        path,
                        format!("invalid JSON Pointer escape in {reference:?}"),
                    ));
                }
            }
        }
        self.root
            .pointer(&pointer)
            .ok_or_else(|| reject(path, format!("unresolved local reference {reference:?}")))
    }
}

fn reject(path: &str, reason: impl Into<String>) -> SchemaLoweringError {
    SchemaLoweringError {
        path: path.to_owned(),
        reason: reason.into(),
    }
}

fn child(path: &str, token: &str) -> String {
    format!("{path}/{}", token.replace('~', "~0").replace('/', "~1"))
}

fn decode_fragment(fragment: &str) -> Option<String> {
    let mut out = Vec::new();
    let mut bytes = fragment.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = (bytes.next()? as char).to_digit(16)?;
            let low = (bytes.next()? as char).to_digit(16)?;
            out.push((high * 16 + low) as u8);
        } else {
            out.push(byte);
        }
    }
    String::from_utf8(out).ok()
}

fn nonempty_branches<'a>(value: &'a Value, path: &str) -> LoweringResult<&'a Vec<Value>> {
    value
        .as_array()
        .filter(|values| !values.is_empty())
        .ok_or_else(|| {
            reject(
                path,
                "composition must contain a nonempty array of schema branches",
            )
        })
}

fn annotation(key: &str) -> bool {
    matches!(
        key,
        "$defs"
            | "definitions"
            | "$schema"
            | "$id"
            | "$comment"
            | "title"
            | "description"
            | "default"
            | "examples"
            | "example"
            | "deprecated"
            | "readOnly"
            | "writeOnly"
    ) || key.starts_with("x-")
}

fn object_intersection_branch(map: &Map<String, Value>) -> bool {
    match map.get("type") {
        Some(Value::String(kind)) => kind.eq_ignore_ascii_case("object"),
        Some(_) => false,
        None => map.keys().all(|key| {
            annotation(key)
                || matches!(
                    key.as_str(),
                    "properties"
                        | "required"
                        | "additionalProperties"
                        | "patternProperties"
                        | "propertyNames"
                        | "dependentSchemas"
                        | "dependentRequired"
                        | "unevaluatedProperties"
                        | "minProperties"
                        | "maxProperties"
                        | "propertyOrdering"
                )
        }),
    }
}

fn reject_nullable_constant(map: &Map<String, Value>, path: &str) -> LoweringResult<()> {
    if map.get("nullable") == Some(&Value::Bool(true)) {
        return Err(reject(
            &child(path, "nullable"),
            "nullable string constants cannot be lowered to an exact native enum",
        ));
    }
    Ok(())
}

fn has_validation(map: &Map<String, Value>) -> bool {
    map.keys().any(|key| !annotation(key))
}

fn restricts_object_members(map: &Map<String, Value>) -> bool {
    ["additionalProperties", "unevaluatedProperties"]
        .iter()
        .any(|key| {
            map.get(*key)
                .is_some_and(|value| value != &Value::Bool(true))
        })
}

/// Intersection of already-lowered schemas. Differing scalar constraints and
/// overlapping nonidentical properties are rejected rather than overwritten.
/// This is intentionally not a general-purpose JSON Schema intersection solver.
fn merge(
    mut left: Map<String, Value>,
    right: Map<String, Value>,
    path: &str,
) -> LoweringResult<Map<String, Value>> {
    if left == right {
        return Ok(left);
    }
    if has_validation(&left) && has_validation(&right) {
        if restricts_object_members(&left) || restricts_object_members(&right) {
            return Err(reject(
                path,
                "closed-object intersection equivalence is not proven",
            ));
        }
        // Hoisting nullable:true from just one conjunct could make the other,
        // non-nullable type constraint accept null. Do not conflate this with
        // required membership, which is preserved independently below.
        if (left.get("nullable") == Some(&Value::Bool(true))
            || right.get("nullable") == Some(&Value::Bool(true)))
            && left.get("nullable") != right.get("nullable")
        {
            return Err(reject(
                &child(path, "nullable"),
                "nullable intersection equivalence is not proven",
            ));
        }
    }
    for (key, value) in right {
        let field_path = child(path, &key);
        let Some(existing) = left.get_mut(&key) else {
            left.insert(key, value);
            continue;
        };
        match key.as_str() {
            "properties" => {
                let (Value::Object(existing), Value::Object(properties)) = (existing, value) else {
                    return Err(reject(&field_path, "properties must be an object"));
                };
                for (name, schema) in properties {
                    if let Some(previous) = existing.get(&name) {
                        if previous != &schema {
                            return Err(reject(
                                &child(&field_path, &name),
                                "conflicting allOf/reference property schemas",
                            ));
                        }
                    } else {
                        existing.insert(name, schema);
                    }
                }
            }
            "required" => {
                let (Value::Array(existing), Value::Array(names)) = (existing, value) else {
                    return Err(reject(&field_path, "required must be an array of strings"));
                };
                if existing.iter().chain(&names).any(|name| !name.is_string()) {
                    return Err(reject(&field_path, "required must be an array of strings"));
                }
                // Deduplicate only when intersecting required constraints, not in
                // untouched primitive schemas or arbitrary literal arrays.
                let mut union = Vec::new();
                for name in std::mem::take(existing).into_iter().chain(names) {
                    if !union.contains(&name) {
                        union.push(name);
                    }
                }
                *existing = union;
            }
            "enum" if existing != &value => {
                let (Value::Array(existing), Value::Array(values)) = (existing, value) else {
                    return Err(reject(&field_path, "enum must be an array"));
                };
                existing.retain(|entry| values.contains(entry));
                if existing.is_empty() {
                    return Err(reject(
                        &field_path,
                        "conflicting enum constraints have an empty intersection",
                    ));
                }
            }
            _ if existing == &value => {}
            _ => {
                return Err(reject(
                    &field_path,
                    "conflicting allOf/reference constraints cannot be safely merged",
                ));
            }
        }
    }
    for (minimum, maximum) in [
        ("minimum", "maximum"),
        ("minItems", "maxItems"),
        ("minProperties", "maxProperties"),
        ("minLength", "maxLength"),
    ] {
        if let (Some(min), Some(max)) = (
            left.get(minimum).and_then(Value::as_f64),
            left.get(maximum).and_then(Value::as_f64),
        ) && min > max
        {
            return Err(reject(
                &child(path, maximum),
                format!("conflicting {minimum}/{maximum} bounds"),
            ));
        }
    }
    Ok(left)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lowered(schema: Value) -> Value {
        lower_parameters(&schema).expect("supported lowering")
    }

    fn rejects(schema: Value, reason: &str) -> SchemaLoweringError {
        let error = lower_parameters(&schema).expect_err("lossy schema must be rejected");
        assert!(error.reason.contains(reason), "{error}");
        assert!(error.path.starts_with('#'), "{error}");
        error
    }

    #[test]
    fn resolves_nested_local_definition_request_fixture() {
        let definition = json!({
            "type": "object",
            "properties": {"city": {"type": "string", "enum": ["Paris", "Rome"]}},
            "required": ["city"]
        });
        let output = lowered(json!({
            "type": "object",
            "properties": {"destination": {"$ref": "#/$defs/address"}},
            "required": ["destination"],
            "$defs": {"address": definition}
        }));
        assert_eq!(output["properties"]["destination"], definition);
        assert_eq!(output["required"], json!(["destination"]));
        assert_eq!(
            output["$defs"]["address"], definition,
            "catalog removal belongs to the sanitizer"
        );
    }

    #[test]
    fn repeated_sibling_refs_are_not_cycles() {
        let city = json!({"type": "string", "enum": ["Paris", "Rome"]});
        let output = lowered(json!({
            "type": "object",
            "properties": {
                "origin": {"$ref": "#/$defs/city"},
                "destination": {"$ref": "#/$defs/city"}
            },
            "required": ["origin", "destination"],
            "$defs": {"city": city}
        }));
        assert_eq!(output["properties"]["origin"], city);
        assert_eq!(output["properties"]["destination"], city);
        assert_eq!(output["required"], json!(["origin", "destination"]));
    }

    #[test]
    fn resolves_legacy_refs_and_pointer_escapes_against_original_root() {
        let output = lowered(json!({
            "properties": {
                "slash": {"ref": "#/definitions/a~1b"},
                "tilde": {"$ref": "#/$defs/m~0n"},
                "literal_escape": {"$ref": "#/$defs/~01"},
                "percent": {"$ref": "#/%24defs/with%20space"},
                "alias": {"$ref": "#/properties/slash"}
            },
            "definitions": {"a/b": {"type": "string"}},
            "$defs": {"m~n": {"type": "integer"}, "~1": {"type": "boolean"}, "with space": {"type": "number"}}
        }));
        assert_eq!(output["properties"]["slash"], json!({"type": "string"}));
        assert_eq!(output["properties"]["tilde"], json!({"type": "integer"}));
        assert_eq!(
            output["properties"]["literal_escape"],
            json!({"type": "boolean"})
        );
        assert_eq!(output["properties"]["percent"], json!({"type": "number"}));
        assert_eq!(output["properties"]["alias"], output["properties"]["slash"]);
    }

    #[test]
    fn compatible_reference_siblings_survive() {
        let output = lowered(json!({
            "$defs": {"number": {"type": "integer", "minimum": 1}},
            "properties": {"limit": {"$ref": "#/$defs/number", "maximum": 10, "description": "Result count"}}
        }));
        assert_eq!(
            output["properties"]["limit"],
            json!({"type": "integer", "minimum": 1, "maximum": 10, "description": "Result count"})
        );
        rejects(
            json!({"$defs": {"number": {"type": "integer"}}, "$ref": "#/$defs/number", "type": "string"}),
            "conflicting",
        );
        rejects(
            json!({"$defs": {"number": {"minimum": 10}}, "$ref": "#/$defs/number", "maximum": 1}),
            "bounds",
        );
        rejects(
            json!({"$defs": {"s": {"description": "Original"}}, "$ref": "#/$defs/s", "description": "Different"}),
            "conflicting",
        );
    }

    #[test]
    fn dual_local_reference_spellings_are_constraints_not_overwrites() {
        let output = lowered(
            json!({"$defs": {"s": {"type": "string"}}, "$ref": "#/$defs/s", "ref": "#/$defs/s"}),
        );
        assert_eq!(output["type"], "string");
        rejects(
            json!({"$defs": {"s": {"type": "string"}}, "$ref": "#/$defs/s", "ref": "legacy"}),
            "external reference",
        );
    }

    #[test]
    fn cycles_are_rejected_and_unused_recursive_catalogs_are_not_expanded() {
        rejects(json!({"$ref": "#"}), "circular reference");
        rejects(
            json!({"$ref": "#/$defs/a", "$defs": {"a": {"$ref": "#/$defs/a"}}}),
            "circular reference",
        );
        rejects(
            json!({"$ref": "#/$defs/a", "$defs": {"a": {"$ref": "#/$defs/b"}, "b": {"ref": "#/$defs/a"}}}),
            "circular reference",
        );
        rejects(
            json!({"$ref": "#/$defs/a", "$defs": {"a": {"properties": {"next": {"$ref": "#/$defs/a"}}}}}),
            "circular reference",
        );
        let unused = json!({"type": "string", "$defs": {"unused": {"$ref": "#/$defs/unused"}}});
        assert_eq!(lowered(unused.clone()), unused);
    }

    #[test]
    fn rejects_missing_external_and_invalid_pointers_without_network_access() {
        for reference in [
            "https://example.invalid/schema.json",
            "http://127.0.0.1:9/schema.json",
            "file:///etc/passwd",
            "other.json#/$defs/item",
        ] {
            let error = rejects(
                json!({"properties": {"a/b~c": {"$ref": reference}}}),
                "external reference",
            );
            assert_eq!(error.path, "#/properties/a~1b~0c/$ref");
            assert!(error.reason.contains("no external fetching"));
        }
        rejects(json!({"$ref": "#/$defs/missing"}), "unresolved");
        rejects(json!({"$ref": "#anchor"}), "not a local JSON Pointer");
        for reference in ["#/$defs/a~2b", "#/$defs/a~", "#/%ZZ", "#/%ff"] {
            rejects(json!({"$ref": reference}), "invalid JSON Pointer");
        }
        rejects(json!({"$ref": 12}), "reference must be");
        rejects(
            json!({"$ref": "#/$defs/a", "$defs": {"a": false}}),
            "does not target an object schema",
        );
    }

    #[test]
    fn flattens_disjoint_objects_and_preserves_required_first_appearance() {
        let output = lowered(json!({
            "type": "object", "description": "Search", "required": ["first", "city"],
            "allOf": [
                {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
                {"type": "object", "properties": {"limit": {"type": "integer"}, "label": {"type": "string"}}, "required": ["limit", "first"]}
            ]
        }));
        assert_eq!(
            output,
            json!({
                "type": "object", "description": "Search", "required": ["first", "city", "limit"],
                "properties": {"city": {"type": "string"}, "limit": {"type": "integer"}, "label": {"type": "string"}}
            })
        );
    }

    #[test]
    fn single_all_of_branch_required_union_keeps_first_occurrence() {
        assert_eq!(
            lowered(json!({"allOf": [{"type": "object", "required": ["b", "a", "b"]}]})),
            json!({"type": "object", "required": ["b", "a"]})
        );
        rejects(
            json!({"allOf": [{"type": "object", "required": [12]}]}),
            "required must be",
        );
    }

    #[test]
    fn all_of_supports_nested_refs_and_identical_overlapping_properties() {
        let output = lowered(json!({
            "$defs": {"branch": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}},
            "allOf": [
                {"$ref": "#/$defs/branch"},
                {"type": "object", "allOf": [{"properties": {"city": {"type": "string"}, "note": {"type": "string", "nullable": true}}}], "required": ["note"]}
            ]
        }));
        assert_eq!(output["required"], json!(["city", "note"]));
        assert_eq!(output["properties"]["city"], json!({"type": "string"}));
        assert_eq!(output["properties"]["note"]["nullable"], true);
        assert!(output.get("allOf").is_none());
    }

    #[test]
    fn conflicting_and_unproven_intersections_are_rejected() {
        let error = rejects(
            json!({"allOf": [
                {"type": "object", "properties": {"x": {"type": "string"}}},
                {"type": "object", "properties": {"x": {"type": "integer"}}}
            ]}),
            "conflicting",
        );
        assert_eq!(error.path, "#/allOf/1/properties/x");
        rejects(
            json!({"allOf": [{"type": "object", "maxProperties": 1}, {"type": "object", "maxProperties": 2}]}),
            "conflicting",
        );
        rejects(
            json!({"allOf": [{"type": "object", "minProperties": 3}, {"type": "object", "maxProperties": 1}]}),
            "bounds",
        );
        rejects(
            json!({"allOf": [{"type": "object", "additionalProperties": false, "properties": {"a": {}}}, {"type": "object", "properties": {"b": {}}}]}),
            "closed-object",
        );
        rejects(
            json!({"allOf": [{"type": "object", "unevaluatedProperties": false}, {"type": "object", "properties": {"b": {}}}]}),
            "closed-object",
        );
        rejects(
            json!({"type": "string", "allOf": [{"type": "object"}]}),
            "only supports compatible object",
        );
        rejects(
            json!({"allOf": [{"type": "string"}, {"type": "string"}]}),
            "only supports compatible object",
        );
    }

    #[test]
    fn nullable_intersections_cannot_broaden_a_nonnullable_constraint() {
        rejects(
            json!({"$defs": {"s": {"type": "string"}}, "$ref": "#/$defs/s", "nullable": true}),
            "nullable intersection",
        );
        rejects(
            json!({"allOf": [{"type": "object", "nullable": true}, {"type": "object"}]}),
            "nullable intersection",
        );
        assert_eq!(
            lowered(json!({"allOf": [
                {"type": "object", "nullable": true, "properties": {"a": {"type": "string"}}},
                {"type": "object", "nullable": true, "properties": {"b": {"type": "string"}}}
            ]})),
            json!({"type": "object", "nullable": true, "properties": {"a": {"type": "string"}, "b": {"type": "string"}}})
        );
    }

    #[test]
    fn closed_reference_with_only_annotations_does_not_invent_an_intersection() {
        let output = lowered(
            json!({"$defs": {"closed": {"type": "object", "additionalProperties": false}}, "$ref": "#/$defs/closed", "description": "No properties"}),
        );
        assert_eq!(output["additionalProperties"], false);
        assert_eq!(output["description"], "No properties");
    }

    #[test]
    fn lowers_distinct_string_constant_request_fixture_and_standalone_const() {
        assert_eq!(
            lowered(json!({
                "type": "object", "properties": {"mode": {"type": "string", "description": "Mode", "oneOf": [{"const": "fast"}, {"const": "safe"}]}}, "required": ["mode"]
            })),
            json!({
                "type": "object", "properties": {"mode": {"type": "string", "description": "Mode", "enum": ["fast", "safe"]}}, "required": ["mode"]
            })
        );
        assert_eq!(
            lowered(json!({"const": "only", "description": "Fixed"})),
            json!({"type": "string", "enum": ["only"], "description": "Fixed"})
        );
        assert_eq!(
            lowered(json!({"const": "a", "enum": ["a", "b"]})),
            json!({"type": "string", "enum": ["a"]})
        );
        assert_eq!(
            lowered(
                json!({"oneOf": [{"const": "a", "description": "Shared"}, {"const": "b", "description": "Shared"}]})
            ),
            json!({"type": "string", "enum": ["a", "b"], "description": "Shared"})
        );
    }

    #[test]
    fn arbitrary_duplicate_or_conflicting_constant_alternatives_are_rejected() {
        rejects(
            json!({"oneOf": [{"type": "string"}, {"type": "integer"}]}),
            "distinct string constants",
        );
        rejects(
            json!({"oneOf": [{"const": "a"}, {"const": "a"}]}),
            "duplicate oneOf",
        );
        rejects(
            json!({"oneOf": [{"const": "a", "description": "First"}, {"const": "b", "description": "Second"}]}),
            "differing constraints or annotations",
        );
        rejects(json!({"const": 1}), "only string const");
        rejects(json!({"const": true}), "only string const");
        rejects(json!({"const": null}), "only string const");
        rejects(json!({"type": "integer", "const": "a"}), "conflicting");
        rejects(json!({"const": "a", "enum": ["b"]}), "empty intersection");
        rejects(
            json!({"nullable": true, "oneOf": [{"const": "a"}, {"const": "b"}]}),
            "nullable string constants",
        );
    }

    #[test]
    fn supported_any_of_lowers_children_without_becoming_one_of() {
        let output = lowered(json!({"anyOf": [{"const": "a"}, {"type": "null"}]}));
        assert_eq!(
            output,
            json!({"anyOf": [{"type": "string", "enum": ["a"]}, {"type": "null"}]})
        );
    }

    #[test]
    fn rejects_unsupported_composition_in_schema_positions_only() {
        for keyword in [
            "not",
            "if",
            "then",
            "else",
            "$dynamicRef",
            "$recursiveRef",
            "prefixItems",
        ] {
            let mut schema = Map::new();
            schema.insert(keyword.into(), json!({}));
            let error = rejects(Value::Object(schema), "cannot be lowered");
            assert_eq!(error.path, format!("#/{keyword}"));
        }
        for keyword in ["allOf", "oneOf", "anyOf"] {
            for value in [json!([]), json!({}), json!(null)] {
                let mut schema = Map::new();
                schema.insert(keyword.into(), value);
                rejects(Value::Object(schema), "nonempty array");
            }
        }
        rejects(json!({"items": [{"type": "string"}]}), "tuple schemas");
        rejects(
            json!({"properties": {"x": {"not": {"type": "string"}}}}),
            "cannot be lowered",
        );
    }

    #[test]
    fn preserves_keyword_property_names_and_literal_payloads() {
        let literal = json!({"$ref": "https://example.invalid/not-a-schema", "allOf": [true], "oneOf": [{"const": "x"}], "properties": {"not": false}});
        let schema = json!({
            "type": "object", "properties": {
                "allOf": {"type": "string"}, "$ref": {"type": "integer"}, "ref": {"type": "string"}, "not": {"type": "boolean"}, "const": {"type": "string"}
            },
            "required": ["allOf", "$ref"], "default": literal, "enum": [literal], "examples": [literal], "example": literal,
            "x-payload": literal
        });
        assert_eq!(lowered(schema.clone()), schema);
    }

    #[test]
    fn nullable_required_independence_and_simple_schema_controls_are_unchanged() {
        let schema = json!({
            "type": "object", "properties": {
                "required_note": {"type": "string", "nullable": true},
                "optional_note": {"type": "string", "nullable": true},
                "limit": {"type": "integer", "minimum": 1, "maximum": 10},
                "tags": {"type": "array", "items": {"type": "string"}}
            }, "required": ["required_note"]
        });
        assert_eq!(lowered(schema.clone()), schema);
        for primitive in [
            Value::Null,
            json!("string"),
            json!(123),
            json!(["legacy", null]),
        ] {
            assert_eq!(lowered(primitive.clone()), primitive);
        }
        rejects(json!(true), "boolean schemas");
        rejects(json!(false), "boolean schemas");
        rejects(json!({"properties": {"flag": false}}), "boolean schemas");
        assert_eq!(
            lowered(json!({"additionalProperties": false})),
            json!({"additionalProperties": false})
        );
    }

    fn nested_items(depth: usize) -> Value {
        let mut schema = json!({});
        for _ in 0..depth {
            schema = json!({"items": schema});
        }
        schema
    }

    fn reference_chain(length: usize) -> Value {
        let mut definitions = Map::new();
        for index in 0..length {
            let value = if index + 1 == length {
                json!({})
            } else {
                json!({"$ref": format!("#/$defs/n{}", index + 1)})
            };
            definitions.insert(format!("n{index}"), value);
        }
        json!({"$ref": "#/$defs/n0", "$defs": definitions})
    }

    #[test]
    fn exact_depth_boundary_includes_schema_children_and_reference_expansion() {
        assert!(lower_parameters(&nested_items(MAX_DEPTH)).is_ok());
        rejects(nested_items(MAX_DEPTH + 1), "maximum lowering depth 64");
        assert!(lower_parameters(&reference_chain(MAX_DEPTH)).is_ok());
        rejects(reference_chain(MAX_DEPTH + 1), "maximum lowering depth 64");
    }

    #[test]
    fn exact_node_budget_boundary_is_inclusive_for_copied_literal_nodes() {
        // root object + default array + its scalar members = 10,000 nodes.
        let at_limit = json!({"default": vec![Value::Null; MAX_EXPANDED_NODES - 2]});
        assert_eq!(lower_parameters(&at_limit).unwrap(), at_limit);
        let over_limit = json!({"default": vec![Value::Null; MAX_EXPANDED_NODES - 1]});
        let error = rejects(over_limit, "maximum expanded node count 10000");
        assert_eq!(error.path, "#/default/9998");
    }

    #[test]
    fn repeated_reference_expansion_is_budgeted_not_just_unique_input_nodes() {
        let mut definitions = Map::new();
        definitions.insert("n0".into(), json!({"type": "string"}));
        for index in 1..=13 {
            let reference = format!("#/$defs/n{}", index - 1);
            definitions.insert(
                format!("n{index}"),
                json!({"properties": {"left": {"$ref": reference}, "right": {"$ref": reference}}}),
            );
        }
        rejects(
            json!({"$defs": definitions, "$ref": "#/$defs/n13"}),
            "maximum expanded node count 10000",
        );
    }

    #[test]
    fn typed_error_has_a_useful_display_and_standard_error_interface() {
        let error = rejects(
            json!({"properties": {"bad": {"$ref": "#/missing"}}}),
            "unresolved",
        );
        let as_error: &dyn Error = &error;
        assert!(as_error.to_string().contains("#/properties/bad/$ref"));
        assert!(as_error.to_string().contains("#/missing"));
    }
}
