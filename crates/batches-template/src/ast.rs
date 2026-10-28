//! The parse tree, shaped like Go's `text/template/parse` nodes. `Display` prints a node the
//! way Go's `Node.String()` does (with the standard `{{ }}` delimiters), which partial
//! evaluation exposes.

use std::fmt;

use crate::value::Value;

#[derive(Debug, Clone)]
pub(crate) enum Node {
    Text { pos: usize, text: String },
    Action { pos: usize, pipe: Pipe },
    If(Control),
    Range(Control),
    With(Control),
    Break { pos: usize },
    Continue { pos: usize },
}

#[derive(Debug, Clone)]
pub(crate) struct Control {
    pub pos: usize,
    pub pipe: Pipe,
    pub list: Vec<Node>,
    pub else_list: Option<Vec<Node>>,
}

#[derive(Debug, Clone)]
pub(crate) struct Pipe {
    pub pos: usize,
    /// `$x = …` rather than `$x := …`.
    pub is_assign: bool,
    pub decl: Vec<(usize, String)>,
    pub cmds: Vec<Command>,
}

#[derive(Debug, Clone)]
pub(crate) struct Command {
    pub pos: usize,
    pub args: Vec<Operand>,
}

#[derive(Debug, Clone)]
pub(crate) enum Operand {
    Bool {
        pos: usize,
        value: bool,
    },
    Number {
        pos: usize,
        text: String,
        value: Value,
    },
    String {
        pos: usize,
        quoted: String,
        value: String,
    },
    Nil {
        pos: usize,
    },
    Dot {
        pos: usize,
    },
    /// `.a.b.c`
    Field {
        pos: usize,
        names: Vec<String>,
    },
    /// `$x.a.b` (`names[0]` is the variable, with its `$`).
    Variable {
        pos: usize,
        names: Vec<String>,
    },
    /// A function name.
    Identifier {
        pos: usize,
        name: String,
    },
    /// A parenthesized pipeline.
    Pipe {
        pos: usize,
        pipe: Box<Pipe>,
    },
    /// `term.a.b` where the term is not a field or variable (`repository.name`,
    /// `(f x).a`).
    Chain {
        pos: usize,
        node: Box<Operand>,
        fields: Vec<String>,
    },
}

impl Operand {
    pub(crate) fn pos(&self) -> usize {
        match self {
            Operand::Bool { pos, .. }
            | Operand::Number { pos, .. }
            | Operand::String { pos, .. }
            | Operand::Nil { pos }
            | Operand::Dot { pos }
            | Operand::Field { pos, .. }
            | Operand::Variable { pos, .. }
            | Operand::Identifier { pos, .. }
            | Operand::Pipe { pos, .. }
            | Operand::Chain { pos, .. } => *pos,
        }
    }
}

impl fmt::Display for Operand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operand::Bool { value, .. } => write!(f, "{value}"),
            Operand::Number { text, .. } => f.write_str(text),
            Operand::String { quoted, .. } => f.write_str(quoted),
            Operand::Nil { .. } => f.write_str("nil"),
            Operand::Dot { .. } => f.write_str("."),
            Operand::Field { names, .. } => {
                for n in names {
                    write!(f, ".{n}")?;
                }
                Ok(())
            }
            Operand::Variable { names, .. } => f.write_str(&names.join(".")),
            Operand::Identifier { name, .. } => f.write_str(name),
            Operand::Pipe { pipe, .. } => write!(f, "({pipe})"),
            Operand::Chain { node, fields, .. } => {
                match node.as_ref() {
                    Operand::Pipe { pipe, .. } => write!(f, "({pipe})")?,
                    other => write!(f, "{other}")?,
                }
                for n in fields {
                    write!(f, ".{n}")?;
                }
                Ok(())
            }
        }
    }
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, a) in self.args.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{a}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Pipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.decl.is_empty() {
            let names: Vec<&str> = self.decl.iter().map(|(_, n)| n.as_str()).collect();
            write!(
                f,
                "{} {} ",
                names.join(", "),
                if self.is_assign { "=" } else { ":=" }
            )?;
        }
        for (i, c) in self.cmds.iter().enumerate() {
            if i > 0 {
                f.write_str(" | ")?;
            }
            write!(f, "{c}")?;
        }
        Ok(())
    }
}

pub(crate) fn write_list(f: &mut fmt::Formatter<'_>, nodes: &[Node]) -> fmt::Result {
    for n in nodes {
        write!(f, "{n}")?;
    }
    Ok(())
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let control = |f: &mut fmt::Formatter<'_>, name: &str, c: &Control| {
            write!(f, "{{{{{name} {}}}}}", c.pipe)?;
            write_list(f, &c.list)?;
            if let Some(e) = &c.else_list {
                f.write_str("{{else}}")?;
                write_list(f, e)?;
            }
            f.write_str("{{end}}")
        };
        match self {
            Node::Text { text, .. } => f.write_str(text),
            Node::Action { pipe, .. } => write!(f, "{{{{{pipe}}}}}"),
            Node::If(c) => control(f, "if", c),
            Node::Range(c) => control(f, "range", c),
            Node::With(c) => control(f, "with", c),
            Node::Break { .. } => f.write_str("{{break}}"),
            Node::Continue { .. } => f.write_str("{{continue}}"),
        }
    }
}
