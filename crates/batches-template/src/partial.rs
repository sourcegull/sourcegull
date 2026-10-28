//! Partial evaluation of step templates before any step has run: top-level actions that
//! only use static information (`repository.name`, `batch_change.name`,
//! `batch_change.description`, literals, and `eq`/`ne`/`not` and the bulk change spec functions over
//! them) are replaced by their output. It is best effort: anything else leaves the whole
//! action as it is. The step runner uses it to decide `if:` conditions (and so cache keys)
//! ahead of execution.

use crate::ast::{Node, Operand, Pipe};
use crate::context::StepContext;
use crate::error::TemplateError;
use crate::funcs::{self, SRC_CLI_FUNCTIONS};
use crate::gofmt;
use crate::value::Value;
use crate::{Template, is_true_output};

impl StepContext {
    /// Partially evaluates `input` and reports whether only text is left, and if so whether
    /// that text is `true`.
    pub fn is_static_bool(&self, input: &str) -> Result<(bool, bool), TemplateError> {
        let nodes = self.partial_eval(input)?;
        if !nodes.iter().all(|n| matches!(n, Node::Text { .. })) {
            return Ok((false, false));
        }
        Ok((true, is_true_output(&join_text(&nodes))))
    }

    /// Partially evaluates `input`; if only text is left returns it.
    pub fn is_static_string(&self, input: &str) -> Result<Option<String>, TemplateError> {
        let nodes = self.partial_eval(input)?;
        if !nodes.iter().all(|n| matches!(n, Node::Text { .. })) {
            return Ok(None);
        }
        Ok(Some(join_text(&nodes)))
    }

    /// The template after partial evaluation, printed like Go's `Tree.Root.String()`
    /// (remaining actions use the standard `{{ }}` delimiters).
    pub fn partial_eval_string(&self, input: &str) -> Result<String, TemplateError> {
        let nodes = self.partial_eval(input)?;
        Ok(nodes.iter().map(|n| n.to_string()).collect())
    }

    fn partial_eval(&self, input: &str) -> Result<Vec<Node>, TemplateError> {
        let t = Template::parse(input)?;
        let names = self.names();
        if let Value::Map(m) = &names {
            t.check_functions(|n| m.contains_key(n))?;
        }
        Ok(t.nodes
            .into_iter()
            .map(|n| match n {
                Node::Action { pos, pipe } => match eval_pipe(self, &pipe) {
                    Some(v) => {
                        let mut text = String::new();
                        gofmt::write_v(&v, &mut text);
                        Node::Text { pos, text }
                    }
                    None => Node::Action { pos, pipe },
                },
                other => other,
            })
            .collect())
    }
}

fn join_text(nodes: &[Node]) -> String {
    nodes
        .iter()
        .map(|n| match n {
            Node::Text { text, .. } => text.as_str(),
            _ => "",
        })
        .collect()
}

fn eval_pipe(ctx: &StepContext, p: &Pipe) -> Option<Value> {
    if !p.decl.is_empty() {
        return None;
    }
    // Commands are evaluated on their own: a previous command's value is
    // not passed on as the final argument.
    let mut last = None;
    for c in &p.cmds {
        last = Some(eval_cmd(ctx, &c.args)?);
    }
    last
}

fn eval_cmd(ctx: &StepContext, args: &[Operand]) -> Option<Value> {
    match &args[0] {
        Operand::Bool { .. }
        | Operand::Number { .. }
        | Operand::String { .. }
        | Operand::Chain { .. } => {
            if args.len() == 1 {
                eval_node(ctx, &args[0])
            } else {
                None
            }
        }
        Operand::Identifier { name, .. } => eval_function(ctx, name, &args[1..]),
        _ => None,
    }
}

fn eval_node(ctx: &StepContext, n: &Operand) -> Option<Value> {
    match n {
        Operand::Bool { value, .. } => Some(Value::Bool(*value)),
        Operand::Number { value, .. } => Some(value.clone()),
        Operand::String { value, .. } => Some(Value::String(value.clone())),
        Operand::Chain { node, fields, .. } => {
            // Only one level deep.
            if fields.len() != 1 {
                return None;
            }
            let Operand::Identifier { name, .. } = node.as_ref() else {
                return None;
            };
            match (name.as_str(), fields[0].as_str()) {
                // search_result_paths is a list; turning it into text is not useful.
                ("repository", "name") => Some(Value::from(ctx.repository.name.as_str())),
                ("batch_change", "name") => Some(Value::from(ctx.batch_change.name.as_str())),
                ("batch_change", "description") => {
                    Some(Value::from(ctx.batch_change.description.as_str()))
                }
                _ => None,
            }
        }
        Operand::Pipe { pipe, .. } => eval_pipe(ctx, pipe),
        _ => None,
    }
}

fn eval_function(ctx: &StepContext, name: &str, args: &[Operand]) -> Option<Value> {
    match name {
        "eq" | "ne" => {
            // Only two arguments, compared like Go's `==` on interfaces.
            if args.len() != 2 {
                return None;
            }
            let a = eval_node(ctx, &args[0])?;
            let b = eval_node(ctx, &args[1])?;
            let equal = match (&a, &b) {
                (Value::List(_) | Value::Map(_) | Value::PathList(_), _)
                | (_, Value::List(_) | Value::Map(_) | Value::PathList(_)) => return None,
                _ => a == b,
            };
            Some(Value::Bool(if name == "eq" { equal } else { !equal }))
        }
        "not" => {
            if args.len() != 1 {
                return None;
            }
            Some(Value::Bool(!eval_node(ctx, &args[0])?.is_true()))
        }
        _ if SRC_CLI_FUNCTIONS.contains(&name) => {
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(eval_node(ctx, a)?);
            }
            funcs::check_arity(name, vals.len()).ok()?;
            funcs::call(name, &vals).ok()
        }
        _ => None,
    }
}
