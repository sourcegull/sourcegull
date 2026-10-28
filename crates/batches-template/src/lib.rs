//! Bulk change spec templating: an evaluator for the `${{ … }}` expressions in bulk change specs with the
//! syntax and semantics of Go's `text/template`, plus the bulk change spec contexts and functions.
//!
//! Templates are evaluated as Go's `text/template` would with:
//! - delimiters `${{` and `}}` (so `{{ }}` in scripts is plain text),
//! - `missingkey=error`, so a mistyped `${{ repository.nmae }}` fails instead of printing
//!   `<no value>`,
//! - the context exposed as niladic functions (`repository`, `batch_change`, `steps`,
//!   `outputs`, `previous_step`, `step`, and `batch_change_link` in changeset templates),
//!   which is why `repository.name` works without a leading dot,
//! - the extra functions `join`, `split`, `replace`, `join_if` and `matches` on top of Go's
//!   builtins.
//!
//! The supported syntax is Go's: text and actions, trim markers (`${{- ` / ` -}}`),
//! comments, pipelines, parenthesized pipelines, field chains, variables (`$x := …`,
//! `$x = …`, `$`), `if`/`else if`/`else`, `range` (lists, maps in key order, integers;
//! `$i, $v :=`; `else`; `break`/`continue`), `with`/`else with`, and literals (strings,
//! raw strings, characters, Go number syntax, booleans, `nil`). `define`, `template`,
//! `block` and `call` are rejected with [`ErrorKind::Unsupported`]. Values print with Go's
//! `fmt` rules (`[a b]`, `map[k:v]`, `1e+06`, `<no value>`).
//!
//! Deliberate differences from Go: a list whose elements are all strings is accepted where
//! Go wants `[]string` (`join outputs.list ","` fails in Go when the list came from JSON),
//! ranging over an integer works as in Go 1.24, and execution is bounded by [`Options`].
//!
//! Entry points: [`render`] / [`render_with`], [`evaluate_bool`] for `if:` conditions,
//! [`Template::parse`] for parse-only validation, [`validate_batch_spec_template`] (a dry run
//! over a whole spec), the contexts [`StepContext`] and [`ChangesetTemplateContext`],
//! and the partial evaluation [`StepContext::is_static_bool`] / [`StepContext::is_static_string`].

mod ast;
mod context;
mod error;
mod exec;
mod funcs;
mod glob;
mod gofmt;
mod lex;
mod parse;
mod partial;
mod validate;
mod value;

use std::collections::{BTreeMap, BTreeSet};

pub use context::{
    BatchChangeAttributes, Changes, ChangesetTemplateContext, Repository, StepContext, StepResult,
    StepsContext,
};
pub use error::{ErrorKind, TemplateError};
pub use funcs::{GO_BUILTINS, SRC_CLI_FUNCTIONS, UNSUPPORTED_BUILTINS};
pub use validate::validate_batch_spec_template;
pub use value::Value;

use ast::{Node, Operand, Pipe};
use error::RawError;

/// What a field lookup does when the map has no such key (Go's `missingkey` option).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MissingKey {
    /// `missingkey=error`, used everywhere in bulk change specs: fail naming the key.
    #[default]
    Error,
    /// Go's default (`missingkey=default`, and `zero` for `map[string]any`): the lookup
    /// yields nil, which prints as `<no value>`.
    NoValue,
}

/// Execution options.
#[derive(Debug, Clone)]
pub struct Options {
    pub missing_key: MissingKey,
    /// Upper bound on evaluation steps (nodes, commands, loop iterations), so a template
    /// like `${{ range 100000000000 }}` fails instead of hanging the worker.
    pub max_steps: u64,
    /// Upper bound on the rendered output and on any string a function returns, in bytes.
    pub max_output: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            missing_key: MissingKey::Error,
            max_steps: 10_000_000,
            max_output: 16 << 20,
        }
    }
}

/// A parsed template.
#[derive(Debug, Clone)]
pub struct Template {
    source: String,
    nodes: Vec<Node>,
}

/// Well-known template functions from Sprig/Helm that are not available; naming them in the
/// error helps people coming from GitHub Actions or Helm.
const SPRIG_HINTS: &[&str] = &[
    "upper",
    "lower",
    "title",
    "trim",
    "trimPrefix",
    "trimSuffix",
    "trimAll",
    "contains",
    "hasPrefix",
    "hasSuffix",
    "default",
    "empty",
    "quote",
    "squote",
    "toJson",
    "fromJson",
    "toYaml",
    "regexMatch",
    "regexReplaceAll",
    "repeat",
    "substr",
    "list",
    "dict",
    "first",
    "last",
    "uniq",
    "sortAlpha",
    "cat",
    "nospace",
    "indent",
    "nindent",
    "b64enc",
    "b64dec",
    "sha256sum",
    "env",
    "toString",
    "atoi",
    "add",
    "sub",
    "mul",
    "div",
    "mod",
];

impl Template {
    /// Parses a template; this checks the syntax and variables but not function names.
    pub fn parse(source: &str) -> Result<Template, TemplateError> {
        let toks =
            lex::lex(source).map_err(|(pos, msg)| RawError::syntax(pos, msg).place(source))?;
        let nodes = parse::parse(&toks).map_err(|e| e.place(source))?;
        Ok(Template {
            source: source.to_owned(),
            nodes,
        })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether the template is plain text (no actions survive parsing).
    pub fn is_static(&self) -> bool {
        self.nodes.iter().all(|n| matches!(n, Node::Text { .. }))
    }

    /// The tree printed the way Go's `Tree.Root.String()` prints it (standard `{{ }}`
    /// delimiters, normalized spacing).
    pub fn tree_string(&self) -> String {
        nodes_string(&self.nodes)
    }

    /// The function names the template calls, in order of first use.
    pub fn function_names(&self) -> Vec<String> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        visit_identifiers(&self.nodes, &mut |_, name| {
            if seen.insert(name.to_owned()) {
                out.push(name.to_owned());
            }
        });
        out
    }

    /// Checks that every function the template calls is a builtin or `defined` (Go does this
    /// at parse time: `function "x" not defined`).
    pub fn check_functions(&self, defined: impl Fn(&str) -> bool) -> Result<(), TemplateError> {
        let mut err: Option<RawError> = None;
        visit_identifiers(&self.nodes, &mut |pos, name| {
            if err.is_some() || defined(name) {
                return;
            }
            if UNSUPPORTED_BUILTINS.contains(&name) {
                err = Some(RawError {
                    kind: ErrorKind::Unsupported(name.to_owned()),
                    pos,
                    message: format!(
                        "function {name:?} is not supported in bulk change spec templates"
                    ),
                });
            } else if !funcs::is_builtin(name) {
                let hint = if SPRIG_HINTS.contains(&name) {
                    format!(
                        " (Sprig functions are not available; bulk change spec templates add only {} to Go's builtins)",
                        SRC_CLI_FUNCTIONS.join(", ")
                    )
                } else {
                    String::new()
                };
                err = Some(RawError {
                    kind: ErrorKind::UndefinedFunction(name.to_owned()),
                    pos,
                    message: format!("function {name:?} not defined{hint}"),
                });
            }
        });
        match err {
            Some(e) => Err(e.place(&self.source)),
            None => Ok(()),
        }
    }

    /// Executes the template. `names` (a map) holds the context names callable as niladic
    /// functions (`repository`, …); `dot` is the value of `.` and `$`.
    pub fn execute(
        &self,
        names: &Value,
        dot: &Value,
        opts: &Options,
    ) -> Result<String, TemplateError> {
        let empty = BTreeMap::new();
        let names = match names {
            Value::Map(m) => m,
            _ => &empty,
        };
        self.check_functions(|n| names.contains_key(n))?;
        exec::execute(&self.nodes, names, dot, opts).map_err(|e| e.place(&self.source))
    }
}

fn nodes_string(nodes: &[Node]) -> String {
    let mut s = String::new();
    for n in nodes {
        s.push_str(&n.to_string());
    }
    s
}

fn visit_identifiers(nodes: &[Node], f: &mut dyn FnMut(usize, &str)) {
    fn pipe(p: &Pipe, f: &mut dyn FnMut(usize, &str)) {
        for c in &p.cmds {
            for a in &c.args {
                operand(a, f);
            }
        }
    }
    fn operand(o: &Operand, f: &mut dyn FnMut(usize, &str)) {
        match o {
            Operand::Identifier { pos, name } => f(*pos, name),
            Operand::Pipe { pipe: p, .. } => pipe(p, f),
            Operand::Chain { node, .. } => operand(node, f),
            _ => {}
        }
    }
    for n in nodes {
        match n {
            Node::Action { pipe: p, .. } => pipe(p, f),
            Node::If(c) | Node::Range(c) | Node::With(c) => {
                pipe(&c.pipe, f);
                visit_identifiers(&c.list, f);
                if let Some(e) = &c.else_list {
                    visit_identifiers(e, f);
                }
            }
            Node::Text { .. } | Node::Break { .. } | Node::Continue { .. } => {}
        }
    }
}

/// Parses a template (parse-only validation).
pub fn parse(template: &str) -> Result<Template, TemplateError> {
    Template::parse(template)
}

/// Renders `template` with `ctx` as both the context names (`${{ repository.name }}`) and
/// the dot (`${{ .repository.name }}`), with `missingkey=error`.
pub fn render(template: &str, ctx: &Value) -> Result<String, TemplateError> {
    render_with(template, ctx, ctx, &Options::default())
}

/// Renders with separate context names and dot, and explicit options.
pub fn render_with(
    template: &str,
    names: &Value,
    dot: &Value,
    opts: &Options,
) -> Result<String, TemplateError> {
    Template::parse(template)?.execute(names, dot, opts)
}

/// Evaluates an `if:` condition: an empty condition is true;
/// otherwise the condition is rendered and is true iff the output, trimmed, is `true`.
pub fn evaluate_bool(condition: &str, ctx: &Value) -> Result<bool, TemplateError> {
    if condition.is_empty() {
        return Ok(true);
    }
    Ok(is_true_output(&render(condition, ctx)?))
}

pub(crate) fn is_true_output(s: &str) -> bool {
    s.trim() == "true"
}

/// Whether the text contains a template action at all.
pub fn contains_template_action(s: &str) -> bool {
    s.contains(lex::LEFT_DELIM)
}

/// Whether the text is static, i.e. has no `${{`.
pub fn is_static(s: &str) -> bool {
    !contains_template_action(s)
}
