//! Bounded, deliberately conservative lowering for Gemini tool `parameters`.
//!
//! This is not the native Schema field allowlist. The encoder must run its
//! schema-aware deletion cleanup *after* this pass. In particular, definition
//! catalogs and unsupported non-composition keywords are retained here. Only
//! referenced definitions are interpreted; unused catalogs are inert data.

use std::collections::BTreeSet;
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
            let mut lowered_branches = Vec::new();
            for (index, branch) in branches.iter().enumerate() {
                let branch_path = child(&one_of_path, &index.to_string());
                if !branch.is_object() {
                    return Err(reject(
                        &branch_path,
                        "oneOf branches must be object schemas",
                    ));
                }
                let Value::Object(branch) = self.lower(branch, &branch_path, depth + 2)? else {
                    unreachable!("object lowering returns an object")
                };
                lowered_branches.push(branch);
            }

            if lowered_branches.iter().all(finite_string_enum) {
                // Pairwise-disjoint finite domains prove oneOf is their union,
                // even when the explicit types overlap. Consts and references
                // have already been lowered, and every original value budgeted.
                let mut union = Vec::new();
                let mut accepted_values = BTreeSet::new();
                let mut common_constraints: Option<Map<String, Value>> = None;
                for (index, mut branch) in lowered_branches.into_iter().enumerate() {
                    let branch_path = child(&one_of_path, &index.to_string());
                    branch.remove("type");
                    let Some(Value::Array(values)) = branch.remove("enum") else {
                        unreachable!("finite string enum checked above")
                    };
                    let mut branch_values = BTreeSet::new();
                    for value in values {
                        let text = value.as_str().expect("string enum checked above");
                        if accepted_values.contains(text) {
                            return Err(reject(
                                &branch_path,
                                "overlapping oneOf enum values (duplicate oneOf alternatives) are not equivalent to an enum",
                            ));
                        }
                        // Enum has set semantics within a branch. Only values
                        // shared by different oneOf branches break exclusivity.
                        if branch_values.insert(text.to_owned()) {
                            union.push(value);
                        }
                    }
                    accepted_values.extend(branch_values);
                    // Identical residual constraints/annotations may be
                    // hoisted; branch-specific data must never be discarded.
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
                }
                let mut alternatives = common_constraints.expect("nonempty branches checked above");
                reject_nullable_constant(&alternatives, &one_of_path)?;
                reject_nullable_constant(&siblings, path)?;
                // Move the visited values to a shallower, smaller schema; no
                // expansion or unaccounted generated child nodes are needed.
                alternatives.insert("type".into(), Value::String("string".into()));
                alternatives.insert("enum".into(), Value::Array(union));
                siblings = merge(siblings, alternatives, &one_of_path)?;
            } else {
                // Explicit types give upper bounds on the values accepted by
                // each *lowered* branch. Pairwise-disjoint bounds prove exactly
                // one branch can match, without inspecting or removing any of
                // its other constraints. Do not infer types from nested unions,
                // properties, enums, or sibling constraints.
                nullable(&siblings, path)?;
                let mut accepted_types = 0;
                for (index, branch) in lowered_branches.iter().enumerate() {
                    let branch_path = child(&one_of_path, &index.to_string());
                    let types = explicit_type_domain(branch, &branch_path)?;
                    if accepted_types & types != 0 {
                        return Err(reject(
                            &branch_path,
                            "oneOf alternatives have overlapping explicit types; anyOf equivalence is not proven",
                        ));
                    }
                    accepted_types |= types;
                }
                // The removed oneOf wrapper was counted above. Account for
                // the generated anyOf wrapper too; branch nodes were already
                // visited at the same depth and are moved, not expanded again.
                self.visit(&child(path, "anyOf"), depth + 1)?;
                let mut alternatives = Map::new();
                alternatives.insert(
                    "anyOf".into(),
                    Value::Array(lowered_branches.into_iter().map(Value::Object).collect()),
                );
                // An existing anyOf (including one supplied by allOf or a ref)
                // is another conjunct, never a field that may be overwritten.
                siblings = merge(siblings, alternatives, &one_of_path)?;
            }
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

fn finite_string_enum(map: &Map<String, Value>) -> bool {
    map.get("type").and_then(Value::as_str) == Some("string")
        && matches!(map.get("enum"), Some(Value::Array(values)) if !values.is_empty() && values.iter().all(Value::is_string))
}

fn nullable(map: &Map<String, Value>, path: &str) -> LoweringResult<bool> {
    match map.get("nullable") {
        None | Some(Value::Bool(false)) => Ok(false),
        Some(Value::Bool(true)) => Ok(true),
        Some(_) => Err(reject(
            &child(path, "nullable"),
            "nullable must be a boolean for safe schema lowering",
        )),
    }
}

/// An overapproximation, not a schema satisfiability solver. Number occupies
/// both integer and non-integer partitions: JSON Schema integer is a subset of
/// number, so differently spelled types alone do not prove their disjointness.
/// Unknown, missing, or array-valued types do not provide this limited proof.
fn explicit_type_domain(map: &Map<String, Value>, path: &str) -> LoweringResult<u8> {
    const NULL: u8 = 1;
    const INTEGER: u8 = 1 << 1;
    const NON_INTEGER: u8 = 1 << 2;
    let mut types = match map.get("type").and_then(Value::as_str) {
        Some("null") => NULL,
        Some("integer") => INTEGER,
        Some("number") => INTEGER | NON_INTEGER,
        Some("string") => 1 << 3,
        Some("object") => 1 << 4,
        Some("array") => 1 << 5,
        Some("boolean") => 1 << 6,
        _ => {
            return Err(reject(
                &child(path, "type"),
                "oneOf alternatives require a supported explicit atomic JSON type for anyOf lowering",
            ));
        }
    };
    if nullable(map, path)? {
        types |= NULL;
    }
    Ok(types)
}

fn reject_nullable_constant(map: &Map<String, Value>, path: &str) -> LoweringResult<()> {
    if nullable(map, path)? {
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
        // A reference can supply the composition while its use site supplies
        // nullable (or vice versa). Validate both sides before reasoning about
        // their null domains, including non-boolean values from those siblings.
        let left_nullable = nullable(&left, path)?;
        let right_nullable = nullable(&right, path)?;
        if restricts_object_members(&left) || restricts_object_members(&right) {
            return Err(reject(
                path,
                "closed-object intersection equivalence is not proven",
            ));
        }
        // Hoisting nullable:true from just one conjunct could make the other,
        // non-nullable type constraint accept null. Do not conflate this with
        // required membership, which is preserved independently below.
        if (left_nullable || right_nullable) && left.get("nullable") != right.get("nullable") {
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
    fn one_of_string_and_object_schedule_fixture_preserves_order_and_constraints() {
        let description = "Absolute target: an RFC 3339 date-time with offset, or a local date, time, and IANA time_zone.";
        let branches = vec![
            json!({"type": "string", "description": "Absolute timestamp", "pattern": "T", "default": "2030-01-02T03:04:05Z"}),
            json!({
                "type": "object", "additionalProperties": false,
                "description": "Local target", "default": {"date": "2030-01-02", "time": "03:04:05", "time_zone": "UTC"},
                "properties": {
                    "date": {"type": "string", "minLength": 10},
                    "time": {"type": "string", "description": "Local time"},
                    "time_zone": {"type": "string", "enum": ["UTC", "Asia/Shanghai"]}
                },
                "required": ["date", "time", "time_zone"]
            }),
        ];
        for branches in [branches.clone(), branches.into_iter().rev().collect()] {
            let output = lowered(json!({
                "type": "object", "properties": {"at": {
                    "description": description, "default": "2030-01-02T03:04:05Z", "oneOf": branches
                }}, "required": ["at"]
            }));
            assert_eq!(
                output,
                json!({
                    "type": "object", "properties": {"at": {
                        "description": description, "default": "2030-01-02T03:04:05Z", "anyOf": branches
                    }}, "required": ["at"]
                })
            );
        }
    }

    #[test]
    fn one_of_distinct_atomic_types_lower_to_any_of() {
        // Integer and number each exclude all the nonnumeric JSON types, but
        // are deliberately never placed together in this positive fixture.
        for numeric in ["integer", "number"] {
            let branches = json!([
                {"type": "object", "required": ["id"]},
                {"type": "string", "pattern": "^target"},
                {"type": numeric, "minimum": 1, "maximum": 10},
                {"type": "array", "items": {"type": "string"}, "maxItems": 2},
                {"type": "null"},
                {"type": "boolean", "default": false, "nullable": false}
            ]);
            assert_eq!(
                lowered(json!({"oneOf": branches})),
                json!({"anyOf": branches})
            );
        }
        assert_eq!(
            lowered(json!({"oneOf": [{"type": "string"}]})),
            json!({"anyOf": [{"type": "string"}]})
        );
    }

    #[test]
    fn one_of_disjoint_type_proof_uses_lowered_refs_and_intersections() {
        let output = lowered(json!({
            "$defs": {
                "timestamp": {"type": "string", "pattern": "T"},
                "local": {"allOf": [
                    {"type": "object", "properties": {"date": {"type": "string"}}, "required": ["date"]},
                    {"type": "object", "properties": {"time": {"type": "string"}}, "required": ["time"]}
                ]}
            },
            "properties": {"at": {"oneOf": [
                {"$ref": "#/$defs/timestamp", "description": "Absolute timestamp"},
                {"ref": "#/$defs/local", "description": "Local timestamp"}
            ]}}
        }));
        assert_eq!(
            output["properties"]["at"],
            json!({"anyOf": [
                {"type": "string", "pattern": "T", "description": "Absolute timestamp"},
                {"type": "object", "properties": {"date": {"type": "string"}, "time": {"type": "string"}}, "required": ["date", "time"], "description": "Local timestamp"}
            ]})
        );
        let error = rejects(
            json!({
                "$defs": {"number": {"type": "number"}, "integer": {"type": "integer"}},
                "oneOf": [{"$ref": "#/$defs/number"}, {"$ref": "#/$defs/integer"}]
            }),
            "overlapping explicit types",
        );
        assert_eq!(error.path, "#/oneOf/1");
    }

    #[test]
    fn one_of_nullable_type_domains_include_null_without_changing_required() {
        let branches = json!([
            {"type": "string", "nullable": true, "default": null},
            {"type": "object", "nullable": false, "required": ["date"]}
        ]);
        assert_eq!(
            lowered(json!({"properties": {"at": {"oneOf": branches}}, "required": ["at"]})),
            json!({"properties": {"at": {"anyOf": branches}}, "required": ["at"]})
        );
        assert_eq!(
            lowered(
                json!({"oneOf": [{"type": "string", "nullable": false}, {"type": "null", "nullable": true}]})
            ),
            json!({"anyOf": [{"type": "string", "nullable": false}, {"type": "null", "nullable": true}]})
        );
        for branches in [
            json!([{"type": "string", "nullable": true}, {"type": "object", "nullable": true}]),
            json!([{"type": "null"}, {"type": "string", "nullable": true}]),
            json!([{"type": "integer", "nullable": true}, {"type": "number"}]),
        ] {
            rejects(json!({"oneOf": branches}), "overlapping explicit types");
        }
        // A parent nullable flag is not a branch-local flag and must not
        // silently broaden the intersection with non-nullable alternatives.
        rejects(
            json!({"nullable": true, "oneOf": [{"type": "string"}, {"type": "object"}]}),
            "nullable intersection",
        );
        for invalid in [json!(null), json!("true"), json!(1), json!([])] {
            let error = rejects(
                json!({"oneOf": [{"type": "string", "nullable": invalid}, {"type": "object"}]}),
                "nullable must be a boolean",
            );
            assert_eq!(error.path, "#/oneOf/0/nullable");
            let error = rejects(
                json!({"nullable": invalid, "oneOf": [{"type": "string"}, {"type": "object"}]}),
                "nullable must be a boolean",
            );
            assert_eq!(error.path, "#/nullable");
        }
    }

    #[test]
    fn one_of_reference_sibling_nullable_constraints_are_not_ignored() {
        let union = json!({"oneOf": [{"type": "string"}, {"type": "object"}]});
        for (flag, reason) in [
            (json!(true), "nullable intersection"),
            (json!("true"), "nullable must be a boolean"),
            (json!(null), "nullable must be a boolean"),
        ] {
            let error = rejects(
                json!({"$defs": {"union": union}, "$ref": "#/$defs/union", "nullable": flag}),
                reason,
            );
            assert_eq!(error.path, "#/nullable");
            let error = rejects(
                json!({"$defs": {"flag": {"nullable": flag}}, "$ref": "#/$defs/flag", "oneOf": union["oneOf"]}),
                reason,
            );
            assert_eq!(error.path, "#/nullable");
        }
        let output = lowered(json!({
            "$defs": {"union": union}, "$ref": "#/$defs/union", "nullable": false
        }));
        assert_eq!(output["anyOf"], union["oneOf"]);
        assert_eq!(output["nullable"], false);
    }

    #[test]
    fn one_of_overlapping_or_unproven_atomic_types_are_rejected() {
        for branches in [
            json!([{"type": "number"}, {"type": "integer"}]),
            json!([{"type": "integer"}, {"type": "number"}]),
            json!([{"type": "string"}, {"type": "string", "pattern": "^x"}]),
            json!([{"type": "boolean"}, {"type": "boolean"}]),
            json!([{"type": "array", "maxItems": 0}, {"type": "array", "minItems": 1}]),
            json!([{"type": "integer", "minimum": 1}, {"type": "number", "maximum": 0}]),
            json!([
                {"type": "object", "additionalProperties": false, "properties": {"a": {"type": "string"}}, "required": ["a"]},
                {"type": "object", "additionalProperties": false, "properties": {"b": {"type": "string"}}, "required": ["b"]}
            ]),
        ] {
            let error = rejects(json!({"oneOf": branches}), "overlapping explicit types");
            assert_eq!(error.path, "#/oneOf/1");
        }
        for branch in [
            json!({}),
            json!({"properties": {"date": {"type": "string"}}}),
            json!({"enum": ["fixed"]}),
            json!({"type": ["string"]}),
            json!({"type": ["string", "null"]}),
            json!({"type": "unknown"}),
            json!({"type": "STRING"}),
            json!({"type": null}),
            json!({"type": 1}),
            json!({"anyOf": [{"type": "string"}, {"type": "object"}]}),
            json!({"oneOf": [{"type": "string"}, {"type": "object"}]}),
        ] {
            let error = rejects(
                json!({"oneOf": [branch, {"type": "boolean"}]}),
                "supported explicit atomic JSON type",
            );
            assert_eq!(error.path, "#/oneOf/0/type");
        }
        for branch in [json!(true), json!(null), json!(1), json!([])] {
            let error = rejects(
                json!({"oneOf": [branch, {"type": "string"}]}),
                "oneOf branches must be object schemas",
            );
            assert_eq!(error.path, "#/oneOf/0");
        }
    }

    #[test]
    fn one_of_nested_compositions_remain_within_their_schema_boundaries() {
        let inner_branches =
            json!([{"type": "boolean"}, {"type": "array", "items": {"type": "integer"}}]);
        let output = lowered(json!({"anyOf": [{"type": "array", "items": {"oneOf": [
            {"type": "string", "anyOf": [{"type": "string", "pattern": "^a"}, {"type": "string", "pattern": "^b"}]},
            {"type": "object", "properties": {"choice": {"oneOf": inner_branches}}, "required": ["choice"]}
        ]}}]}));
        assert_eq!(
            output,
            json!({"anyOf": [{"type": "array", "items": {"anyOf": [
                {"type": "string", "anyOf": [{"type": "string", "pattern": "^a"}, {"type": "string", "pattern": "^b"}]},
                {"type": "object", "properties": {"choice": {"anyOf": inner_branches}}, "required": ["choice"]}
            ]}}]})
        );
    }

    #[test]
    fn one_of_does_not_overwrite_an_existing_sibling_or_reference_any_of() {
        let branches = json!([{"type": "string"}, {"type": "object", "required": ["date"]}]);
        assert_eq!(
            lowered(json!({"description": "Shared", "anyOf": branches, "oneOf": branches})),
            json!({"description": "Shared", "anyOf": branches})
        );
        let error = rejects(
            json!({"anyOf": [{"type": "boolean"}], "oneOf": branches}),
            "conflicting",
        );
        assert_eq!(error.path, "#/oneOf/anyOf");
        let error = rejects(
            json!({"$defs": {"existing": {"anyOf": [{"type": "boolean"}]}}, "$ref": "#/$defs/existing", "oneOf": branches}),
            "conflicting",
        );
        assert_eq!(error.path, "#/anyOf");
        let output = lowered(
            json!({"$defs": {"existing": {"anyOf": branches}}, "$ref": "#/$defs/existing", "oneOf": branches}),
        );
        assert_eq!(output["anyOf"], branches);
    }

    #[test]
    fn one_of_singleton_enums_keep_the_existing_constant_strategy() {
        assert_eq!(
            lowered(json!({"oneOf": [
                {"type": "string", "enum": ["b"], "description": "Shared", "default": "b"},
                {"type": "string", "enum": ["a"], "description": "Shared", "default": "b"}
            ]})),
            json!({"type": "string", "enum": ["b", "a"], "description": "Shared", "default": "b"})
        );
        let output = lowered(json!({
            "$defs": {"a": {"const": "a"}, "b": {"type": "string", "enum": ["b"]}},
            "oneOf": [{"$ref": "#/$defs/b"}, {"$ref": "#/$defs/a"}]
        }));
        assert_eq!(output["enum"], json!(["b", "a"]));
        assert!(output.get("anyOf").is_none());
        assert_eq!(
            lowered(
                json!({"oneOf": [{"type": "string", "enum": ["a", "b"]}, {"type": "string", "enum": ["c", "d"]}]})
            ),
            json!({"type": "string", "enum": ["a", "b", "c", "d"]})
        );
        rejects(
            json!({"oneOf": [{"type": "string", "enum": ["a"], "nullable": true}, {"type": "string", "enum": ["b"], "nullable": true}]}),
            "nullable string constants",
        );
        rejects(
            json!({"nullable": "true", "oneOf": [{"const": "a"}, {"const": "b"}]}),
            "nullable must be a boolean",
        );
    }

    #[test]
    fn one_of_finite_string_enums_lower_permission_request_fixture() {
        let branches = vec![
            json!({"type": "string", "enum": ["read-only", "workspace-write", "danger-full-access"]}),
            json!({"type": "string", "const": ""}),
        ];
        for branches in [branches.clone(), branches.into_iter().rev().collect()] {
            let expected: Vec<Value> = branches
                .iter()
                .flat_map(|branch| {
                    branch
                        .get("enum")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_else(|| vec![branch["const"].clone()])
                })
                .collect();
            let output = lowered(json!({
                "type": "object",
                "properties": {"permission": {
                    "description": "Execution permission; an empty string keeps the current setting.",
                    "default": "",
                    "oneOf": branches
                }},
                "required": ["permission"]
            }));
            assert_eq!(
                output,
                json!({
                    "type": "object",
                    "properties": {"permission": {
                        "description": "Execution permission; an empty string keeps the current setting.",
                        "default": "",
                        "type": "string", "enum": expected
                    }},
                    "required": ["permission"]
                })
            );
        }
    }

    #[test]
    fn one_of_finite_string_enums_preserve_shared_constraints_and_sibling_intersections() {
        let output = lowered(json!({
            "type": "string", "enum": ["b", "c", "outside"],
            "oneOf": [
                {"type": "string", "enum": ["a", "b"], "pattern": "^[a-d]$", "description": "Shared", "default": "b", "nullable": false},
                {"type": "string", "enum": ["c", "d"], "pattern": "^[a-d]$", "description": "Shared", "default": "b", "nullable": false}
            ]
        }));
        assert_eq!(
            output,
            json!({
                "type": "string", "enum": ["b", "c"], "pattern": "^[a-d]$",
                "description": "Shared", "default": "b", "nullable": false
            })
        );
        rejects(
            json!({
                "enum": ["outside"], "oneOf": [
                    {"type": "string", "enum": ["a", "b"]}, {"const": "c"}
                ]
            }),
            "empty intersection",
        );
    }

    #[test]
    fn one_of_finite_string_enums_lower_refs_and_nested_unions_before_proving_disjointness() {
        let output = lowered(json!({
            "$defs": {
                "permissions": {"type": "string", "enum": ["read-only", "workspace-write"]},
                "empty": {"const": ""}
            },
            "properties": {"permission": {"oneOf": [
                {"$ref": "#/$defs/permissions"},
                {"oneOf": [{"const": "danger-full-access"}, {"ref": "#/$defs/empty"}]}
            ]}}
        }));
        assert_eq!(
            output["properties"]["permission"],
            json!({
                "type": "string", "enum": ["read-only", "workspace-write", "danger-full-access", ""]
            })
        );
        assert!(
            output.get("$defs").is_some(),
            "definition catalogs are still cleanup-owned"
        );
        let narrowed = lowered(json!({
            "$defs": {
                "left": {"type": "string", "enum": ["a", "b"]},
                "right": {"type": "string", "enum": ["b", "c"]}
            },
            "oneOf": [{"$ref": "#/$defs/left", "enum": ["a"]}, {"$ref": "#/$defs/right"}]
        }));
        assert_eq!(narrowed["enum"], json!(["a", "b", "c"]));
        assert_eq!(
            lowered(json!({"oneOf": [
                {"type": "string", "enum": ["a", "b"]}, {"type": "object"}
            ]})),
            json!({"anyOf": [
                {"type": "string", "enum": ["a", "b"]}, {"type": "object"}
            ]})
        );
    }

    #[test]
    fn one_of_finite_string_enums_deduplicate_within_branches_but_reject_cross_branch_overlap() {
        assert_eq!(
            lowered(json!({"oneOf": [
                {"type": "string", "enum": ["a", "a", "b"]},
                {"type": "string", "enum": ["c", "c"]}
            ]})),
            json!({"type": "string", "enum": ["a", "b", "c"]})
        );
        for branches in [
            json!([{"type": "string", "enum": ["a", "b"]}, {"const": "b"}]),
            json!([{"type": "string", "enum": ["a", "b"]}, {"type": "string", "enum": ["b", "c"]}]),
            json!([{"type": "string", "enum": ["a", "b"]}, {"type": "string", "enum": ["a", "b"]}]),
        ] {
            let error = rejects(
                json!({"properties": {"a/b~c": {"oneOf": branches}}}),
                "overlapping oneOf enum values",
            );
            assert_eq!(error.path, "#/properties/a~1b~0c/oneOf/1");
        }
        let error = rejects(
            json!({"oneOf": [
                {"type": "string", "enum": ["a", "b"]}, {"const": "c"}, {"const": "a"}
            ]}),
            "overlapping oneOf enum values",
        );
        assert_eq!(error.path, "#/oneOf/2");
    }

    #[test]
    fn one_of_finite_string_enums_keep_conservative_constraint_and_nullable_rejections() {
        for (key, first, second) in [
            ("description", json!("First"), json!("Second")),
            ("pattern", json!("^a"), json!("^b")),
            ("default", json!("a"), json!("b")),
            ("nullable", json!(false), json!(true)),
        ] {
            let mut first_branch = json!({"type": "string", "enum": ["a", "aa"]});
            let mut second_branch = json!({"type": "string", "enum": ["b", "bb"]});
            first_branch[key] = first;
            second_branch[key] = second;
            rejects(
                json!({"oneOf": [first_branch, second_branch]}),
                "differing constraints or annotations",
            );
        }
        for nullable in [json!(true), json!("true"), json!(null)] {
            let reason = if nullable == true {
                "nullable string constants"
            } else {
                "nullable must be a boolean"
            };
            rejects(
                json!({"nullable": nullable, "oneOf": [
                    {"type": "string", "enum": ["a", "b"]}, {"const": "c"}
                ]}),
                reason,
            );
            rejects(
                json!({"oneOf": [
                    {"type": "string", "enum": ["a", "b"], "nullable": nullable},
                    {"type": "string", "enum": ["c", "d"], "nullable": nullable}
                ]}),
                reason,
            );
        }
        for invalid_enum in [json!([]), json!(["a", 1]), json!([null]), json!("a")] {
            rejects(
                json!({"oneOf": [
                    {"type": "string", "enum": invalid_enum}, {"const": "b"}
                ]}),
                "overlapping explicit types",
            );
        }
        rejects(
            json!({"oneOf": [
                {"enum": ["a", "b"]}, {"const": "c"}
            ]}),
            "supported explicit atomic JSON type",
        );
    }

    #[test]
    fn one_of_finite_string_enums_match_exactly_one_branch_for_all_small_value_sets() {
        let values = ["", "a", "b"];
        for first_mask in 1..8 {
            for second_mask in 1..8 {
                let domain = |mask| {
                    values
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| mask & (1 << index) != 0)
                        .map(|(_, value)| *value)
                        .collect::<Vec<_>>()
                };
                let first = domain(first_mask);
                let second = domain(second_mask);
                let schema = json!({"oneOf": [
                    {"type": "string", "enum": first}, {"type": "string", "enum": second}
                ]});
                if first_mask & second_mask != 0 {
                    rejects(schema, "overlapping oneOf enum values");
                } else {
                    let output = lowered(schema);
                    let actual = output["enum"].as_array().unwrap();
                    for candidate in [
                        json!(""),
                        json!("a"),
                        json!("b"),
                        json!("outside"),
                        json!(null),
                        json!(1),
                    ] {
                        let matches = candidate.as_str().map_or(0, |value| {
                            usize::from(first.contains(&value))
                                + usize::from(second.contains(&value))
                        });
                        assert_eq!(actual.contains(&candidate), matches == 1);
                    }
                }
            }
        }
    }

    #[test]
    fn one_of_finite_string_enums_respect_exact_depth_and_node_budgets() {
        let union = json!({"oneOf": [
            {"type": "string", "enum": ["a", "b"]}, {"type": "string", "enum": ["c", "d"]}
        ]});
        // oneOf + branch + enum + value consume four levels.
        let mut at_limit = union.clone();
        for _ in 0..MAX_DEPTH - 4 {
            at_limit = json!({"items": at_limit});
        }
        let output = lowered(at_limit.clone());
        let pointer = format!("{}/enum", "/items".repeat(MAX_DEPTH - 4));
        assert_eq!(output.pointer(&pointer), Some(&json!(["a", "b", "c", "d"])));
        rejects(json!({"items": at_limit}), "maximum lowering depth 64");

        // root + default array + oneOf array + two (branch, type, enum, two values) = 13.
        let at_limit =
            json!({"default": vec![Value::Null; MAX_EXPANDED_NODES - 13], "oneOf": union["oneOf"]});
        assert_eq!(lowered(at_limit)["enum"], json!(["a", "b", "c", "d"]));
        rejects(
            json!({"default": vec![Value::Null; MAX_EXPANDED_NODES - 12], "oneOf": union["oneOf"]}),
            "maximum expanded node count 10000",
        );
        rejects(
            json!({"oneOf": [
                {"type": "string", "enum": vec!["a"; MAX_EXPANDED_NODES]}, {"const": "b"}
            ]}),
            "maximum expanded node count 10000",
        );
    }

    #[test]
    fn arbitrary_duplicate_or_conflicting_constant_alternatives_are_rejected() {
        // String/integer now has a disjoint-type proof. String/string remains
        // overlapping and must not be weakened from oneOf into anyOf.
        rejects(
            json!({"oneOf": [{"type": "string"}, {"type": "string"}]}),
            "overlapping explicit types",
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
    fn one_of_any_of_lowering_counts_depth_and_the_generated_wrapper() {
        // At the deepest union, oneOf + branch + type consume three levels.
        let union = json!({"oneOf": [{"type": "string"}, {"type": "object"}]});
        let mut at_limit = union.clone();
        for _ in 0..MAX_DEPTH - 3 {
            at_limit = json!({"items": at_limit});
        }
        let output = lower_parameters(&at_limit).expect("depth 64 is inclusive");
        let pointer = format!("{}/anyOf", "/items".repeat(MAX_DEPTH - 3));
        assert_eq!(
            output.pointer(&pointer),
            Some(&json!([{"type": "string"}, {"type": "object"}]))
        );
        rejects(json!({"items": at_limit}), "maximum lowering depth 64");

        // root + copied default array + oneOf array + two (branch, type)
        // pairs + generated anyOf array = eight nodes, before literal members.
        let members = MAX_EXPANDED_NODES - 8;
        let at_limit = json!({"default": vec![Value::Null; members], "oneOf": union["oneOf"]});
        let output = lower_parameters(&at_limit).expect("10,000 visited nodes are inclusive");
        assert_eq!(output["default"].as_array().unwrap().len(), members);
        assert!(output.get("oneOf").is_none());
        assert_eq!(output["anyOf"], union["oneOf"]);
        let error = rejects(
            json!({"default": vec![Value::Null; members + 1], "oneOf": union["oneOf"]}),
            "maximum expanded node count 10000",
        );
        assert_eq!(
            error.path, "#/anyOf",
            "the generated array must consume budget"
        );
        rejects(
            json!({"oneOf": [{"type": "string", "default": vec![Value::Null; MAX_EXPANDED_NODES]}, {"type": "object"}]}),
            "maximum expanded node count 10000",
        );
    }

    #[test]
    fn one_of_lowering_still_bounds_reference_expansion() {
        let mut at_limit = json!({"oneOf": [{"$ref": "#/$defs/text"}, {"type": "object"}]});
        // The referenced branch adds one expansion level before its type.
        for _ in 0..MAX_DEPTH - 4 {
            at_limit = json!({"items": at_limit});
        }
        at_limit
            .as_object_mut()
            .unwrap()
            .insert("$defs".into(), json!({"text": {"type": "string"}}));
        assert!(lower_parameters(&at_limit).is_ok());
        let definitions = at_limit.as_object_mut().unwrap().remove("$defs").unwrap();
        rejects(
            json!({"$defs": definitions, "items": at_limit}),
            "maximum lowering depth 64",
        );

        // Even branches with a simple explicit type still walk and account for
        // every expanded child, rather than skipping validation after proof.
        let mut definitions = Map::new();
        definitions.insert("n0".into(), json!({"type": "object"}));
        for index in 1..=13 {
            let reference = format!("#/$defs/n{}", index - 1);
            definitions.insert(
                format!("n{index}"),
                json!({
                    "type": "object", "properties": {
                        "left": {"$ref": reference}, "right": {"$ref": reference}
                    }
                }),
            );
        }
        rejects(
            json!({"$defs": definitions, "oneOf": [{"type": "string"}, {"$ref": "#/$defs/n13"}]}),
            "maximum expanded node count 10000",
        );
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
