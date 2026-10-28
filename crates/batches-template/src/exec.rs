//! Execution: walks the parse tree the way Go's `text/template/exec.go` does (pipelines
//! feed their value to the next command as the last argument, `and`/`or` short-circuit,
//! variables are scoped to their control structure, `missingkey` decides what a missing
//! map key does).

use std::collections::BTreeMap;

use crate::ast::{Command, Control, Node, Operand, Pipe};
use crate::error::{ErrorKind, RawError};
use crate::funcs::{self, CallError};
use crate::gofmt;
use crate::value::Value;
use crate::{MissingKey, Options};

type Result<T> = std::result::Result<T, RawError>;

#[derive(PartialEq)]
enum Flow {
    Normal,
    Break,
    Continue,
}

pub(crate) struct State<'a> {
    names: &'a BTreeMap<String, Value>,
    opts: &'a Options,
    vars: Vec<(String, Value)>,
    out: String,
    steps: u64,
}

pub(crate) fn execute(
    nodes: &[Node],
    names: &BTreeMap<String, Value>,
    dot: &Value,
    opts: &Options,
) -> Result<String> {
    let mut s = State {
        names,
        opts,
        vars: vec![("$".to_owned(), dot.clone())],
        out: String::new(),
        steps: 0,
    };
    s.walk_list(dot, nodes)?;
    Ok(s.out)
}

impl State<'_> {
    fn tick(&mut self, pos: usize) -> Result<()> {
        self.steps += 1;
        if self.steps > self.opts.max_steps {
            return Err(RawError {
                kind: ErrorKind::Limit,
                pos,
                message: format!("template execution exceeded {} steps", self.opts.max_steps),
            });
        }
        Ok(())
    }

    fn check_size(&self, pos: usize, len: usize) -> Result<()> {
        if len > self.opts.max_output {
            return Err(RawError {
                kind: ErrorKind::Limit,
                pos,
                message: format!("template output exceeds {} bytes", self.opts.max_output),
            });
        }
        Ok(())
    }

    fn write(&mut self, pos: usize, s: &str) -> Result<()> {
        self.check_size(pos, self.out.len() + s.len())?;
        self.out.push_str(s);
        Ok(())
    }

    fn walk_list(&mut self, dot: &Value, nodes: &[Node]) -> Result<Flow> {
        for n in nodes {
            let flow = self.walk(dot, n)?;
            if flow != Flow::Normal {
                return Ok(flow);
            }
        }
        Ok(Flow::Normal)
    }

    fn walk(&mut self, dot: &Value, node: &Node) -> Result<Flow> {
        match node {
            Node::Text { pos, text } => {
                self.tick(*pos)?;
                self.write(*pos, text)?;
                Ok(Flow::Normal)
            }
            Node::Action { pos, pipe } => {
                self.tick(*pos)?;
                let v = self.eval_pipeline(dot, pipe)?;
                // An action that declares variables prints nothing.
                if pipe.decl.is_empty() {
                    let s = gofmt::print_top(&v);
                    self.write(*pos, &s)?;
                }
                Ok(Flow::Normal)
            }
            Node::If(c) => self.walk_if_or_with(dot, c, false),
            Node::With(c) => self.walk_if_or_with(dot, c, true),
            Node::Range(c) => self.walk_range(dot, c),
            Node::Break { pos } => {
                self.tick(*pos)?;
                Ok(Flow::Break)
            }
            Node::Continue { pos } => {
                self.tick(*pos)?;
                Ok(Flow::Continue)
            }
        }
    }

    fn walk_if_or_with(&mut self, dot: &Value, c: &Control, with: bool) -> Result<Flow> {
        self.tick(c.pos)?;
        let mark = self.vars.len();
        let v = self.eval_pipeline(dot, &c.pipe)?;
        let flow = if v.is_true() {
            if with {
                self.walk_list(&v, &c.list)?
            } else {
                self.walk_list(dot, &c.list)?
            }
        } else if let Some(e) = &c.else_list {
            self.walk_list(dot, e)?
        } else {
            Flow::Normal
        };
        self.vars.truncate(mark);
        Ok(flow)
    }

    fn walk_range(&mut self, dot: &Value, c: &Control) -> Result<Flow> {
        self.tick(c.pos)?;
        let outer = self.vars.len();
        let v = self.eval_pipeline(dot, &c.pipe)?;
        let mark = self.vars.len();
        // Integers are ranged lazily; collections are already in memory.
        let (items, count): (Vec<(Value, Value)>, i64) = match &v {
            Value::List(l) => (
                l.iter()
                    .enumerate()
                    .map(|(i, e)| (Value::Int(i as i64), e.clone()))
                    .collect(),
                0,
            ),
            Value::PathList(l) => (
                l.iter()
                    .enumerate()
                    .map(|(i, e)| (Value::Int(i as i64), Value::String(e.clone())))
                    .collect(),
                0,
            ),
            Value::Map(m) => (
                m.iter()
                    .map(|(k, e)| (Value::String(k.clone()), e.clone()))
                    .collect(),
                0,
            ),
            Value::Int(n) => {
                if c.pipe.decl.len() > 1 {
                    return Err(RawError::exec(
                        c.pos,
                        format!("can't use {n} to iterate over more than one variable"),
                    ));
                }
                if *n > 0 {
                    self.check_steps_for(c.pos, *n as u64)?;
                }
                (Vec::new(), (*n).max(0))
            }
            Value::Nil => (Vec::new(), 0),
            other => {
                return Err(RawError::exec(
                    c.pos,
                    format!("range can't iterate over {other}"),
                ));
            }
        };
        let mut flow_out = Flow::Normal;
        if items.is_empty() && count == 0 {
            if let Some(e) = &c.else_list {
                flow_out = self.walk_list(dot, e)?;
            }
            self.vars.truncate(outer);
            return Ok(flow_out);
        }
        let ints = (0..count).map(|i| (Value::Int(i), Value::Int(i)));
        for (index, elem) in items.into_iter().chain(ints) {
            self.tick(c.pos)?;
            let decl = &c.pipe.decl;
            if !decl.is_empty() {
                if c.pipe.is_assign {
                    if decl.len() > 1 {
                        self.set_var(decl[0].0, &decl[0].1, index.clone())?;
                    } else {
                        self.set_var(decl[0].0, &decl[0].1, elem.clone())?;
                    }
                } else {
                    self.set_top_var(1, elem.clone());
                }
            }
            if decl.len() > 1 {
                if c.pipe.is_assign {
                    self.set_var(decl[1].0, &decl[1].1, elem.clone())?;
                } else {
                    self.set_top_var(2, index);
                }
            }
            let flow = self.walk_list(&elem, &c.list)?;
            self.vars.truncate(mark);
            if flow == Flow::Break {
                break;
            }
        }
        self.vars.truncate(outer);
        Ok(flow_out)
    }

    fn check_steps_for(&self, pos: usize, n: u64) -> Result<()> {
        if self.steps.saturating_add(n) > self.opts.max_steps {
            return Err(RawError {
                kind: ErrorKind::Limit,
                pos,
                message: format!("template execution exceeded {} steps", self.opts.max_steps),
            });
        }
        Ok(())
    }

    fn set_top_var(&mut self, n: usize, v: Value) {
        let len = self.vars.len();
        if let Some(slot) = len.checked_sub(n).and_then(|i| self.vars.get_mut(i)) {
            slot.1 = v;
        }
    }

    fn set_var(&mut self, pos: usize, name: &str, v: Value) -> Result<()> {
        for slot in self.vars.iter_mut().rev() {
            if slot.0 == name {
                slot.1 = v;
                return Ok(());
            }
        }
        Err(RawError::exec(pos, format!("undefined variable: {name}")))
    }

    fn var_value(&self, pos: usize, name: &str) -> Result<Value> {
        self.vars
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| RawError::exec(pos, format!("undefined variable: {name}")))
    }

    fn eval_pipeline(&mut self, dot: &Value, pipe: &Pipe) -> Result<Value> {
        let mut value: Option<Value> = None;
        for cmd in &pipe.cmds {
            self.tick(cmd.pos)?;
            value = Some(self.eval_command(dot, cmd, value)?);
        }
        let value = value.unwrap_or_default();
        for (pos, name) in &pipe.decl {
            if pipe.is_assign {
                self.set_var(*pos, name, value.clone())?;
            } else {
                self.vars.push((name.clone(), value.clone()));
            }
        }
        Ok(value)
    }

    fn not_a_function(&self, cmd: &Command, final_: &Option<Value>) -> Result<()> {
        if cmd.args.len() > 1 || final_.is_some() {
            return Err(RawError::exec(
                cmd.args[0].pos(),
                format!(
                    "at <{}>: can't give argument to non-function {}",
                    cmd.args[0], cmd.args[0]
                ),
            ));
        }
        Ok(())
    }

    fn eval_command(&mut self, dot: &Value, cmd: &Command, final_: Option<Value>) -> Result<Value> {
        let first = &cmd.args[0];
        match first {
            Operand::Field { names, .. } => {
                self.eval_field_chain(dot, first, "", names, &cmd.args, final_)
            }
            Operand::Chain { node, fields, .. } => {
                self.eval_chain(dot, first, node, fields, &cmd.args, final_)
            }
            Operand::Identifier { pos, name } => {
                self.eval_function(dot, *pos, name, &cmd.args, final_)
            }
            Operand::Pipe { pipe, .. } => {
                self.not_a_function(cmd, &final_)?;
                self.eval_pipeline(dot, pipe)
            }
            Operand::Variable { pos, names } => {
                let value = self.var_value(*pos, &names[0])?;
                if names.len() == 1 {
                    self.not_a_function(cmd, &final_)?;
                    return Ok(value);
                }
                self.eval_field_chain(&value, first, &names[0], &names[1..], &cmd.args, final_)
            }
            Operand::Nil { pos } => Err(RawError::exec(*pos, "nil is not a command")),
            Operand::Bool { value, .. } => {
                self.not_a_function(cmd, &final_)?;
                Ok(Value::Bool(*value))
            }
            Operand::Dot { .. } => {
                self.not_a_function(cmd, &final_)?;
                Ok(dot.clone())
            }
            Operand::Number { value, .. } => {
                self.not_a_function(cmd, &final_)?;
                Ok(value.clone())
            }
            Operand::String { value, .. } => {
                self.not_a_function(cmd, &final_)?;
                Ok(Value::String(value.clone()))
            }
        }
    }

    /// Evaluates an operand used as an argument (or as the head of a chain).
    fn eval_arg(&mut self, dot: &Value, arg: &Operand) -> Result<Value> {
        match arg {
            Operand::Dot { .. } => Ok(dot.clone()),
            Operand::Nil { .. } => Ok(Value::Nil),
            Operand::Field { names, .. } => self.eval_field_chain(dot, arg, "", names, &[], None),
            Operand::Variable { pos, names } => {
                let value = self.var_value(*pos, &names[0])?;
                if names.len() == 1 {
                    return Ok(value);
                }
                self.eval_field_chain(&value, arg, &names[0], &names[1..], &[], None)
            }
            Operand::Pipe { pipe, .. } => self.eval_pipeline(dot, pipe),
            Operand::Identifier { pos, name } => {
                self.eval_function(dot, *pos, name, std::slice::from_ref(arg), None)
            }
            Operand::Chain { node, fields, .. } => {
                self.eval_chain(dot, arg, node, fields, &[], None)
            }
            Operand::Bool { value, .. } => Ok(Value::Bool(*value)),
            Operand::Number { value, .. } => Ok(value.clone()),
            Operand::String { value, .. } => Ok(Value::String(value.clone())),
        }
    }

    fn eval_chain(
        &mut self,
        dot: &Value,
        whole: &Operand,
        node: &Operand,
        fields: &[String],
        args: &[Operand],
        final_: Option<Value>,
    ) -> Result<Value> {
        if let Operand::Nil { pos } = node {
            return Err(RawError::exec(
                *pos,
                format!("indirection through explicit nil in {whole}"),
            ));
        }
        let head = self.eval_arg(dot, node)?;
        let prefix = match node {
            Operand::Pipe { pipe, .. } => format!("({pipe})"),
            other => other.to_string(),
        };
        self.eval_field_chain(&head, whole, &prefix, fields, args, final_)
    }

    /// Looks up `idents` one after the other starting at `receiver`; only the last one gets
    /// `args` and `final_` (and refuses them, since maps have no methods).
    fn eval_field_chain(
        &mut self,
        receiver: &Value,
        node: &Operand,
        prefix: &str,
        idents: &[String],
        args: &[Operand],
        final_: Option<Value>,
    ) -> Result<Value> {
        let mut path = prefix.to_owned();
        let n = idents.len();
        // `entry`: the current value is a map entry that exists. A nil entry is a nil
        // `interface{}` in Go and looking into it fails, while a missing key (with
        // missingkey=default) or a nil the chain started from yields nil again.
        let mut cur: Option<(Value, bool)> = None;
        for (i, name) in idents.iter().enumerate() {
            path.push('.');
            path.push_str(name);
            let has_args = i + 1 == n && (args.len() > 1 || final_.is_some());
            let (recv, entry) = match &cur {
                Some((v, entry)) => (v, *entry),
                None => (receiver, false),
            };
            let next = self.eval_field(node, &path, name, has_args, recv, entry)?;
            cur = Some(next);
        }
        Ok(cur.map_or_else(|| receiver.clone(), |(v, _)| v))
    }

    fn eval_field(
        &self,
        node: &Operand,
        path: &str,
        name: &str,
        has_args: bool,
        receiver: &Value,
        from_entry: bool,
    ) -> Result<(Value, bool)> {
        let pos = node.pos();
        match receiver {
            Value::Nil if from_entry => Err(RawError::exec(
                pos,
                format!("at <{node}>: nil pointer evaluating interface {{}}.{name}"),
            )),
            Value::Nil => {
                if self.opts.missing_key == MissingKey::Error {
                    return Err(RawError {
                        kind: ErrorKind::MissingKey {
                            path: path.to_owned(),
                            key: name.to_owned(),
                        },
                        pos,
                        message: format!("at <{node}>: nil data; no entry for key {name:?}"),
                    });
                }
                Ok((Value::Nil, false))
            }
            Value::Map(m) => {
                if has_args {
                    return Err(RawError::exec(
                        pos,
                        format!("at <{node}>: {name} is not a method but has arguments"),
                    ));
                }
                match m.get(name) {
                    Some(v) => Ok((v.clone(), true)),
                    None if self.opts.missing_key == MissingKey::Error => Err(RawError {
                        kind: ErrorKind::MissingKey {
                            path: path.to_owned(),
                            key: name.to_owned(),
                        },
                        pos,
                        message: format!("at <{node}>: map has no entry for key {name:?}"),
                    }),
                    None => Ok((Value::Nil, false)),
                }
            }
            other => Err(RawError::exec(
                pos,
                format!(
                    "at <{node}>: can't evaluate field {name} in type {}",
                    other.go_type()
                ),
            )),
        }
    }

    fn eval_function(
        &mut self,
        dot: &Value,
        pos: usize,
        name: &str,
        args: &[Operand],
        final_: Option<Value>,
    ) -> Result<Value> {
        let argc = args.len() - 1 + usize::from(final_.is_some());
        let at = |msg: String| RawError::exec(pos, format!("at <{name}>: {msg}"));
        if let Some(v) = self.names.get(name) {
            if argc != 0 {
                return Err(at(format!(
                    "wrong number of args for {name}: want 0 got {argc}"
                )));
            }
            return Ok(v.clone());
        }
        if !funcs::is_builtin(name) {
            return Err(RawError {
                kind: ErrorKind::UndefinedFunction(name.to_owned()),
                pos,
                message: format!("function {name:?} not defined"),
            });
        }
        funcs::check_arity(name, argc).map_err(at)?;
        if name == "and" || name == "or" {
            let want = name == "or";
            let mut v = Value::Nil;
            for a in &args[1..] {
                v = self.eval_arg(dot, a)?;
                if v.is_true() == want {
                    return Ok(v);
                }
            }
            if let Some(f) = final_ {
                v = f;
            }
            return Ok(v);
        }
        let mut vals = Vec::with_capacity(argc);
        for a in &args[1..] {
            vals.push(self.eval_arg(dot, a)?);
        }
        if let Some(f) = final_ {
            vals.push(f);
        }
        let result = funcs::call(name, &vals).map_err(|e| match e {
            CallError::Type(m) => at(m),
            CallError::Call(m) => at(format!("error calling {name}: {m}")),
        })?;
        if let Value::String(s) = &result {
            self.check_size(pos, s.len())?;
        }
        Ok(result)
    }
}
