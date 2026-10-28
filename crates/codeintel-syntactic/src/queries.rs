//! Scope queries per language, written for this crate.
//!
//! Captures:
//! - `@scope`: a node that opens a scope.
//! - `@definition`: a local definition in the innermost scope. Definitions of one name in one
//!   scope are one symbol (Python assignments, Go redeclarations).
//! - `@definition.shadow`: a definition that starts a new symbol even when the scope already
//!   has the name (Rust `let`).
//! - `@definition.hoisted`: a definition visible in all of its scope, also before it (a
//!   comprehension's variable, used in the element before its `for`).
//! - `@definition.parent`: a definition in the parent of the innermost scope, visible in all
//!   of it (a nested function's name, whose own node is a scope).
//! - `@after`: in the same match, the definitions are visible only after this node (`let x =
//!   x + 1` refers to the outer `x` on the right).
//! - `@reference`: a name that may refer to a local.
//! - `@ignore`: a name that is never a reference (field and keyword argument names).
//!
//! Definitions in the file's top scope are not local (other files can use them), so neither
//! they nor references to them are reported: search-based navigation covers them.

pub(crate) const GO: &str = r#"
[
  (function_declaration)
  (method_declaration)
  (func_literal)
  (block)
  (if_statement)
  (for_statement)
  (expression_switch_statement)
  (type_switch_statement)
  (select_statement)
  (expression_case)
  (type_case)
  (communication_case)
  (default_case)
] @scope

(parameter_declaration name: (identifier) @definition)
(variadic_parameter_declaration name: (identifier) @definition)
(short_var_declaration left: (expression_list (identifier) @definition)) @after
(var_spec name: (identifier) @definition) @after
(const_spec name: (identifier) @definition) @after
(range_clause left: (expression_list (identifier) @definition))
(receive_statement left: (expression_list (identifier) @definition))
(type_switch_statement alias: (expression_list (identifier) @definition))

(identifier) @reference
"#;

pub(crate) const RUST: &str = r#"
[
  (function_item)
  (closure_expression)
  (block)
  (match_arm)
  (for_expression)
  (while_expression)
  (if_expression)
] @scope

(function_item name: (identifier) @definition.parent)
(parameter pattern: (identifier) @definition.shadow)
(closure_parameters (identifier) @definition.shadow)
(let_declaration pattern: (identifier) @definition.shadow) @after
(let_declaration pattern: (_ (identifier) @definition.shadow (#match? @definition.shadow "^[a-z_]"))) @after
(let_declaration pattern: (_ (_ (identifier) @definition.shadow (#match? @definition.shadow "^[a-z_]")))) @after
(let_condition pattern: (identifier) @definition.shadow)
(for_expression pattern: (identifier) @definition.shadow)
(match_pattern (identifier) @definition.shadow (#match? @definition.shadow "^[a-z_]"))
(tuple_pattern (identifier) @definition.shadow (#match? @definition.shadow "^[a-z_]"))
(tuple_struct_pattern (identifier) @definition.shadow (#match? @definition.shadow "^[a-z_]"))
(slice_pattern (identifier) @definition.shadow (#match? @definition.shadow "^[a-z_]"))
(or_pattern (identifier) @definition.shadow (#match? @definition.shadow "^[a-z_]"))
(captured_pattern (identifier) @definition.shadow)
(ref_pattern (identifier) @definition.shadow)
(mut_pattern (identifier) @definition.shadow)
(reference_pattern (identifier) @definition.shadow)
(field_pattern name: (shorthand_field_identifier) @definition.shadow)
(field_pattern pattern: (identifier) @definition.shadow)

(tuple_struct_pattern type: (identifier) @ignore)
(scoped_identifier name: (identifier) @ignore)
(scoped_identifier path: (identifier) @ignore)
(macro_invocation macro: (identifier) @ignore)

(identifier) @reference
"#;

pub(crate) const PYTHON: &str = r#"
[
  (function_definition)
  (lambda)
  (list_comprehension)
  (set_comprehension)
  (dictionary_comprehension)
  (generator_expression)
] @scope

(function_definition name: (identifier) @definition.parent)
(parameters (identifier) @definition)
(lambda_parameters (identifier) @definition)
(default_parameter name: (identifier) @definition)
(typed_parameter (identifier) @definition)
(typed_default_parameter name: (identifier) @definition)
(list_splat_pattern (identifier) @definition)
(dictionary_splat_pattern (identifier) @definition)
(assignment left: (identifier) @definition)
(assignment left: (pattern_list (identifier) @definition))
(assignment left: (tuple_pattern (identifier) @definition))
(for_statement left: (identifier) @definition)
(for_statement left: (pattern_list (identifier) @definition))
(for_statement left: (tuple_pattern (identifier) @definition))
(for_in_clause left: (identifier) @definition.hoisted)
(for_in_clause left: (pattern_list (identifier) @definition.hoisted))
(for_in_clause left: (tuple_pattern (identifier) @definition.hoisted))
(named_expression name: (identifier) @definition)
(as_pattern alias: (as_pattern_target (identifier) @definition))

(attribute attribute: (identifier) @ignore)
(keyword_argument name: (identifier) @ignore)

(identifier) @reference
"#;

pub(crate) const JAVA: &str = r#"
[
  (method_declaration)
  (constructor_declaration)
  (lambda_expression)
  (block)
  (for_statement)
  (enhanced_for_statement)
  (catch_clause)
  (try_with_resources_statement)
  (switch_block_statement_group)
] @scope

(formal_parameter name: (identifier) @definition)
(spread_parameter (variable_declarator name: (identifier) @definition))
(local_variable_declaration declarator: (variable_declarator name: (identifier) @definition)) @after
(enhanced_for_statement name: (identifier) @definition)
(catch_formal_parameter name: (identifier) @definition)
(lambda_expression parameters: (identifier) @definition)
(inferred_parameters (identifier) @definition)
(resource name: (identifier) @definition)
(instanceof_expression name: (identifier) @definition)

(field_access field: (identifier) @ignore)
(method_invocation name: (identifier) @ignore)

(identifier) @reference
"#;

/// JavaScript and the scope/definition patterns TypeScript shares with it.
const JS_COMMON: &str = r#"
[
  (function_declaration)
  (function_expression)
  (generator_function_declaration)
  (generator_function)
  (arrow_function)
  (method_definition)
  (statement_block)
  (for_statement)
  (for_in_statement)
  (catch_clause)
] @scope

(function_declaration name: (identifier) @definition.parent)
(generator_function_declaration name: (identifier) @definition.parent)
(variable_declarator name: (identifier) @definition) @after
(object_pattern (shorthand_property_identifier_pattern) @definition)
(pair_pattern value: (identifier) @definition)
(array_pattern (identifier) @definition)
(assignment_pattern left: (identifier) @definition)
(rest_pattern (identifier) @definition)
(arrow_function parameter: (identifier) @definition)
(catch_clause parameter: (identifier) @definition)
(for_in_statement left: (identifier) @definition)

(shorthand_property_identifier) @reference
(identifier) @reference
"#;

pub(crate) fn javascript() -> String {
    format!("{JS_COMMON}\n(formal_parameters (identifier) @definition)\n")
}

pub(crate) fn typescript() -> String {
    format!(
        "{JS_COMMON}
(required_parameter pattern: (identifier) @definition)
(optional_parameter pattern: (identifier) @definition)
"
    )
}

const C_COMMON: &str = r#"
(parameter_declaration declarator: (identifier) @definition)
(parameter_declaration declarator: (pointer_declarator declarator: (identifier) @definition))
(parameter_declaration declarator: (array_declarator declarator: (identifier) @definition))
(declaration declarator: (identifier) @definition)
(declaration declarator: (init_declarator declarator: (identifier) @definition)) @after
(declaration declarator: (pointer_declarator declarator: (identifier) @definition))
(declaration declarator: (init_declarator declarator: (pointer_declarator declarator: (identifier) @definition))) @after
(declaration declarator: (array_declarator declarator: (identifier) @definition))
(declaration declarator: (init_declarator declarator: (array_declarator declarator: (identifier) @definition))) @after

(identifier) @reference
"#;

pub(crate) fn c() -> String {
    format!(
        "[
  (function_definition)
  (compound_statement)
  (for_statement)
] @scope
{C_COMMON}"
    )
}

pub(crate) fn cpp() -> String {
    format!(
        "[
  (function_definition)
  (compound_statement)
  (for_statement)
  (for_range_loop)
  (lambda_expression)
  (catch_clause)
] @scope
{C_COMMON}
(parameter_declaration declarator: (reference_declarator (identifier) @definition))
(declaration declarator: (init_declarator declarator: (reference_declarator (identifier) @definition))) @after
(for_range_loop declarator: (identifier) @definition)
(for_range_loop declarator: (reference_declarator (identifier) @definition))
(optional_parameter_declaration declarator: (identifier) @definition)
"
    )
}
