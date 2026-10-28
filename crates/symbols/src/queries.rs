//! Definition queries per language, written for this crate.
//!
//! Captures: `@name` is the symbol's name; `@def.<kind>` the whole
//! definition, whose range nests symbols into containers; `@scope` a node
//! that only names a container (a Rust `impl` block), with `@name` as that
//! name; `@container` names the container explicitly where the definition
//! is not nested in it (Go method receivers, C++ `A::f`); `@local` a node
//! whose definitions are locals and not reported, as function bodies are
//! (the nodes the body lexer of `skip` leaves out). When one
//! definition matches several patterns, the first pattern wins, so specific
//! patterns come before general ones.

pub(crate) const RUST: &str = r#"
(mod_item name: (identifier) @name) @def.module
(struct_item name: (type_identifier) @name) @def.struct
(union_item name: (type_identifier) @name) @def.struct
(enum_item name: (type_identifier) @name) @def.enum
(enum_variant name: (identifier) @name) @def.enummember
(field_declaration name: (field_identifier) @name) @def.field
(trait_item name: (type_identifier) @name) @def.interface
(impl_item type: [
  (type_identifier) @name
  (generic_type type: (type_identifier) @name)
  (scoped_type_identifier name: (type_identifier) @name)
]) @scope
(function_item name: (identifier) @name) @def.function
(function_signature_item name: (identifier) @name) @def.method
(const_item name: (identifier) @name) @def.constant
(static_item name: (identifier) @name) @def.variable
(type_item name: (type_identifier) @name) @def.typeparameter
(associated_type name: (type_identifier) @name) @def.typeparameter
(macro_definition name: (identifier) @name) @def.function
[(closure_expression) (array_expression) (arguments)] @local
"#;

pub(crate) const GO: &str = r#"
(package_clause (package_identifier) @name) @def.package
(function_declaration name: (identifier) @name) @def.function
(method_declaration
  receiver: (parameter_list (parameter_declaration type: [
    (type_identifier) @container
    (pointer_type (type_identifier) @container)
    (generic_type type: (type_identifier) @container)
    (pointer_type (generic_type type: (type_identifier) @container))
  ]))
  name: (field_identifier) @name) @def.method
(type_spec name: (type_identifier) @name type: (struct_type)) @def.struct
(type_spec name: (type_identifier) @name type: (interface_type)) @def.interface
(type_spec name: (type_identifier) @name) @def.typeparameter
(type_alias name: (type_identifier) @name) @def.typeparameter
(field_declaration name: (field_identifier) @name) @def.field
(method_elem name: (field_identifier) @name) @def.method
(source_file (const_declaration (const_spec name: (identifier) @name) @def.constant))
(source_file (var_declaration (var_spec name: (identifier) @name) @def.variable))
(source_file (var_declaration (var_spec_list (var_spec name: (identifier) @name) @def.variable)))
[(func_literal) (literal_value) (argument_list)] @local
"#;

pub(crate) const PYTHON: &str = r#"
(class_definition name: (identifier) @name) @def.class
(function_definition name: (identifier) @name) @def.function
(module (expression_statement (assignment left: (identifier) @name) @def.variable))
"#;

pub(crate) const JAVA: &str = r#"
(package_declaration [(identifier) (scoped_identifier)] @name) @def.package
(class_declaration name: (identifier) @name) @def.class
(record_declaration name: (identifier) @name) @def.class
(interface_declaration name: (identifier) @name) @def.interface
(annotation_type_declaration name: (identifier) @name) @def.interface
(enum_declaration name: (identifier) @name) @def.enum
(enum_constant name: (identifier) @name) @def.enummember
(method_declaration name: (identifier) @name) @def.method
(constructor_declaration name: (identifier) @name) @def.constructor
(field_declaration declarator: (variable_declarator name: (identifier) @name)) @def.field
(constant_declaration declarator: (variable_declarator name: (identifier) @name)) @def.constant
[(lambda_expression) (array_initializer) (argument_list)] @local
"#;

/// Shared by JavaScript and TypeScript.
const JS_COMMON: &str = r#"
(function_declaration name: (identifier) @name) @def.function
(generator_function_declaration name: (identifier) @name) @def.function
(class_declaration name: (_) @name) @def.class
(method_definition name: [(property_identifier) (private_property_identifier)] @name) @def.method
(variable_declarator
  name: (identifier) @name
  value: [(arrow_function) (function_expression)]) @def.function
(program (lexical_declaration (variable_declarator name: (identifier) @name) @def.variable))
(program (variable_declaration (variable_declarator name: (identifier) @name) @def.variable))
(program (export_statement (lexical_declaration (variable_declarator name: (identifier) @name) @def.variable)))
[
  (arrow_function) (function_expression) (generator_function)
  (if_statement) (for_statement) (for_in_statement) (while_statement) (do_statement)
  (switch_statement) (call_expression)
] @local
"#;

pub(crate) fn javascript() -> String {
    format!(
        "{JS_COMMON}{}",
        r#"
(field_definition property: [(property_identifier) (private_property_identifier)] @name) @def.field
"#
    )
}

pub(crate) fn typescript() -> String {
    format!(
        "{JS_COMMON}{}",
        r#"
(abstract_class_declaration name: (type_identifier) @name) @def.class
(interface_declaration name: (type_identifier) @name) @def.interface
(type_alias_declaration name: (type_identifier) @name) @def.typeparameter
(enum_declaration name: (identifier) @name) @def.enum
(enum_body [(property_identifier) @name @def.enummember (enum_assignment name: (property_identifier) @name) @def.enummember])
(internal_module name: (_) @name) @def.namespace
(module name: (_) @name) @def.module
(function_signature name: (identifier) @name) @def.function
(method_signature name: (property_identifier) @name) @def.method
(abstract_method_signature name: (property_identifier) @name) @def.method
(property_signature name: (property_identifier) @name) @def.property
(public_field_definition name: [(property_identifier) (private_property_identifier)] @name) @def.field
"#
    )
}

/// Shared by C and C++.
const C_COMMON: &str = r#"
(function_definition declarator: (function_declarator declarator: (identifier) @name)) @def.function
(function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name))) @def.function
(struct_specifier name: (type_identifier) @name body: (_)) @def.struct
(union_specifier name: (type_identifier) @name body: (_)) @def.struct
(enum_specifier name: (type_identifier) @name body: (_)) @def.enum
(enumerator name: (identifier) @name) @def.enummember
(type_definition declarator: (type_identifier) @name) @def.typeparameter
(field_declaration declarator: (field_identifier) @name) @def.field
(field_declaration declarator: (pointer_declarator declarator: (field_identifier) @name)) @def.field
(field_declaration declarator: (array_declarator declarator: (field_identifier) @name)) @def.field
(preproc_def name: (identifier) @name) @def.constant
(preproc_function_def name: (identifier) @name) @def.function
(translation_unit (declaration declarator: (identifier) @name) @def.variable)
(translation_unit (declaration declarator: (init_declarator declarator: (identifier) @name)) @def.variable)
[(initializer_list) (argument_list)] @local
"#;

pub(crate) fn c() -> String {
    C_COMMON.to_owned()
}

pub(crate) fn cpp() -> String {
    format!(
        "{}{C_COMMON}",
        r#"
(function_definition declarator: (function_declarator declarator: (qualified_identifier
  scope: (_) @container
  name: (identifier) @name))) @def.method
(function_definition declarator: (function_declarator declarator: (field_identifier) @name)) @def.method
(field_declaration declarator: (function_declarator declarator: (field_identifier) @name)) @def.method
(class_specifier name: (type_identifier) @name body: (_)) @def.class
(namespace_definition name: (namespace_identifier) @name) @def.namespace
(alias_declaration name: (type_identifier) @name) @def.typeparameter
"#
    )
}

pub(crate) const SHELL: &str = r#"
(function_definition name: (word) @name) @def.function
"#;

pub(crate) const CSHARP: &str = r#"
(namespace_declaration name: (_) @name) @def.namespace
(file_scoped_namespace_declaration name: (_) @name) @def.namespace
(class_declaration name: (identifier) @name) @def.class
(record_declaration name: (identifier) @name) @def.class
(struct_declaration name: (identifier) @name) @def.struct
(interface_declaration name: (identifier) @name) @def.interface
(enum_declaration name: (identifier) @name) @def.enum
(enum_member_declaration name: (identifier) @name) @def.enummember
(delegate_declaration name: (identifier) @name) @def.typeparameter
(method_declaration name: (identifier) @name) @def.method
(constructor_declaration name: (identifier) @name) @def.constructor
(property_declaration name: (identifier) @name) @def.property
(event_declaration name: (identifier) @name) @def.event
(event_field_declaration (variable_declaration (variable_declarator name: (identifier) @name) @def.event))
(field_declaration (modifier) @_const (#eq? @_const "const")
  (variable_declaration (variable_declarator name: (identifier) @name) @def.constant))
(field_declaration (variable_declaration (variable_declarator name: (identifier) @name) @def.field))
[
  (lambda_expression) (anonymous_method_expression) (argument_list) (initializer_expression)
  (operator_declaration) (conversion_operator_declaration) (destructor_declaration)
  (accessor_declaration) (indexer_declaration)
] @local
"#;

pub(crate) const KOTLIN: &str = r#"
(package_header (identifier) @name) @def.package
(class_declaration "interface" (type_identifier) @name) @def.interface
(class_declaration (type_identifier) @name (enum_class_body)) @def.enum
(class_declaration (type_identifier) @name) @def.class
(object_declaration (type_identifier) @name) @def.class
(enum_entry (simple_identifier) @name) @def.enummember
(function_declaration (simple_identifier) @name) @def.function
(secondary_constructor "constructor" @name) @def.constructor
(source_file (property_declaration (modifiers (property_modifier) @_const (#eq? @_const "const"))
  (variable_declaration (simple_identifier) @name)) @def.constant)
(source_file (property_declaration (variable_declaration (simple_identifier) @name)) @def.variable)
(class_body (property_declaration (variable_declaration (simple_identifier) @name)) @def.property)
(class_parameter (binding_pattern_kind) (simple_identifier) @name) @def.property
(type_alias (type_identifier) @name) @def.typeparameter
[(lambda_literal) (anonymous_function) (value_arguments)] @local
"#;

pub(crate) const RUBY: &str = r#"
(module name: [(constant) @name (scope_resolution name: (_) @name)]) @def.module
(class name: [(constant) @name (scope_resolution name: (_) @name)]) @def.class
(program (method name: (_) @name) @def.function)
(method name: (_) @name) @def.method
(singleton_method name: (_) @name) @def.method
(assignment left: (constant) @name) @def.constant
[(block) (do_block) (lambda)] @local
"#;

pub(crate) const SCALA: &str = r#"
(package_clause name: (package_identifier) @name) @def.package
(object_definition name: (identifier) @name) @def.class
(class_definition name: (identifier) @name) @def.class
(trait_definition name: (identifier) @name) @def.interface
(enum_definition name: (identifier) @name) @def.enum
(simple_enum_case name: (identifier) @name) @def.enummember
(full_enum_case name: (identifier) @name) @def.enummember
(function_definition name: (identifier) @name) @def.function
(function_declaration name: (identifier) @name) @def.function
(val_definition pattern: (identifier) @name) @def.variable
(var_definition pattern: (identifier) @name) @def.variable
(val_declaration name: (identifier) @name) @def.variable
(var_declaration name: (identifier) @name) @def.variable
(given_definition name: (identifier) @name) @def.variable
(type_definition name: (type_identifier) @name) @def.typeparameter
[(block) (lambda_expression) (arguments)] @local
"#;

pub(crate) const PHP: &str = r#"
(namespace_definition name: (namespace_name) @name) @def.namespace
(class_declaration name: (name) @name) @def.class
(interface_declaration name: (name) @name) @def.interface
(trait_declaration name: (name) @name) @def.interface
(enum_declaration name: (name) @name) @def.enum
(enum_case name: (name) @name) @def.enummember
(function_definition name: (name) @name) @def.function
(method_declaration name: (name) @name) @def.method
(const_element (name) @name) @def.constant
(property_element name: (variable_name (name) @name)) @def.property
[(anonymous_function) (arrow_function) (arguments)] @local
"#;

pub(crate) const SWIFT: &str = r#"
(class_declaration declaration_kind: "struct" name: (type_identifier) @name) @def.struct
(class_declaration declaration_kind: "enum" name: (type_identifier) @name) @def.enum
(class_declaration declaration_kind: "extension" name: (user_type (type_identifier) @name)) @scope
(class_declaration name: (type_identifier) @name) @def.class
(protocol_declaration name: (type_identifier) @name) @def.interface
(enum_entry name: (simple_identifier) @name @def.enummember)
(function_declaration name: (simple_identifier) @name) @def.function
(protocol_function_declaration name: (simple_identifier) @name) @def.method
(init_declaration "init" @name) @def.constructor
(source_file (property_declaration name: (pattern bound_identifier: (simple_identifier) @name)) @def.variable)
(property_declaration name: (pattern bound_identifier: (simple_identifier) @name)) @def.property
(protocol_property_declaration name: (pattern bound_identifier: (simple_identifier) @name)) @def.property
(typealias_declaration name: (type_identifier) @name) @def.typeparameter
[(lambda_literal) (value_arguments)] @local
"#;

pub(crate) const LUA: &str = r#"
(function_declaration name: (identifier) @name) @def.function
(function_declaration name: (dot_index_expression table: (identifier) @container field: (identifier) @name)) @def.function
(function_declaration name: (method_index_expression table: (identifier) @container method: (identifier) @name)) @def.method
(assignment_statement
  (variable_list . name: (dot_index_expression table: (identifier) @container field: (identifier) @name @def.function))
  (expression_list . value: (function_definition)))
(chunk (variable_declaration (assignment_statement
  (variable_list . name: (identifier) @name @def.function)
  (expression_list . value: (function_definition)))))
(chunk (assignment_statement
  (variable_list . name: (identifier) @name @def.function)
  (expression_list . value: (function_definition))))
(chunk (variable_declaration (assignment_statement (variable_list name: (identifier) @name @def.variable))))
(chunk (assignment_statement (variable_list name: (identifier) @name @def.variable)))
(function_definition) @local
"#;

pub(crate) const ZIG: &str = r#"
(variable_declaration (identifier) @name (struct_declaration)) @def.struct
(variable_declaration (identifier) @name (union_declaration)) @def.struct
(variable_declaration (identifier) @name (enum_declaration)) @def.enum
(variable_declaration (identifier) @name (opaque_declaration)) @def.struct
(variable_declaration (identifier) @name (error_set_declaration)) @def.enum
(variable_declaration "const" (identifier) @name) @def.constant
(variable_declaration (identifier) @name) @def.variable
(enum_declaration (container_field name: (identifier) @name) @def.enummember)
(container_field name: (identifier) @name) @def.field
(function_declaration name: (identifier) @name) @def.function
[(test_declaration) (comptime_declaration) (block)] @local
"#;

pub(crate) const ELIXIR: &str = r#"
(call target: (identifier) @_kw (#eq? @_kw "defmodule") (arguments (alias) @name)) @def.module
(call target: (identifier) @_kw (#eq? @_kw "defprotocol") (arguments (alias) @name)) @def.interface
(call target: (identifier) @_kw (#eq? @_kw "defimpl") (arguments (alias) @name)) @scope
(call target: (identifier) @_kw
  (#any-of? @_kw "def" "defp" "defmacro" "defmacrop" "defguard" "defguardp" "defdelegate" "defn" "defnp")
  (arguments [
    (identifier) @name
    (call target: (identifier) @name)
    (binary_operator left: [(identifier) @name (call target: (identifier) @name)])
  ])) @def.function
(anonymous_function) @local
"#;
