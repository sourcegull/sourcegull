//! Matching a template against a file.
//!
//! A depth-first search over the template's nodes. Holes grow lazily one
//! unit at a time, where a unit is a character, a whole balanced group or a
//! whole string or comment ([`Scan::unit_end`]), so a hole never ends
//! inside a group and never crosses an unmatched closing delimiter. A group
//! in the template matches a paired group in the source, its body matched
//! between the delimiters. A step budget per start position bounds
//! pathological templates.
//!
//! Layout: whitespace in the template matches one or more whitespace
//! characters or comments, all of them; elsewhere whitespace is
//! significant, but comments before a literal are skipped. A hole that ends
//! the template takes everything up to the end of its group or of the file.

use std::ops::Range;

use crate::scan::{Kind, Scan, char_len};
use crate::template::{Hole, HoleKind, Node};

/// Steps one start position may take before it is abandoned.
const STEP_BUDGET: u32 = 200_000;

/// Hole bindings: name and byte range.
pub type Env = Vec<(String, Range<usize>)>;

pub(crate) struct Ctx<'a> {
    pub src: &'a [u8],
    pub scan: &'a Scan,
    pub steps: u32,
    /// Where the match starts (comments are not skipped there).
    pub start: usize,
}

pub(crate) fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn is_ws(b: u8) -> bool {
    b.is_ascii_whitespace()
}

/// The text a binding refers to, without surrounding whitespace.
pub(crate) fn trimmed<'s>(src: &'s [u8], r: &Range<usize>) -> &'s [u8] {
    src[r.clone()].trim_ascii()
}

/// How a match may end: anchored at a group's end, or at the top level,
/// where `accept` checks the whole match (word boundary, rule).
pub(crate) enum End<'f> {
    Anchored(usize),
    Free {
        limit: usize,
        accept: &'f mut dyn FnMut(usize, &Env) -> bool,
    },
}

impl Ctx<'_> {
    /// Past whitespace and comments.
    fn skip_layout(&self, mut pos: usize, limit: usize) -> usize {
        while pos < limit {
            if is_ws(self.src[pos]) {
                pos += 1;
            } else if let Some(e) = self.scan.comment_end(pos).filter(|&e| e <= limit) {
                pos = e;
            } else {
                break;
            }
        }
        pos
    }

    /// Past comments, where a literal may follow.
    fn skip_comments(&self, mut pos: usize, limit: usize) -> usize {
        if pos == self.start {
            return pos;
        }
        while pos < limit
            && let Some(e) = self.scan.comment_end(pos).filter(|&e| e <= limit)
        {
            pos = e;
        }
        pos
    }

    /// The ends of the successive units from `pos` whose first byte passes
    /// `pred`, up to a closing delimiter or `limit`.
    fn units(&self, pos: usize, limit: usize, pred: impl Fn(u8) -> bool) -> Vec<usize> {
        let mut ends = Vec::new();
        let mut q = pos;
        while q < limit && pred(self.src[q]) {
            match self.scan.unit_end(self.src, q) {
                Some(u) if u <= limit => q = u,
                _ => break,
            }
            ends.push(q);
        }
        ends
    }

    fn limit(end: &End<'_>) -> usize {
        match end {
            End::Anchored(e) => *e,
            End::Free { limit, .. } => *limit,
        }
    }

    /// Matches `nodes[i..]` at `pos`; returns where the match ends.
    pub(crate) fn seq(
        &mut self,
        nodes: &[Node],
        i: usize,
        pos: usize,
        end: &mut End<'_>,
        env: &mut Env,
    ) -> Option<usize> {
        self.steps += 1;
        if self.steps > STEP_BUDGET {
            return None;
        }
        let limit = Ctx::limit(end);
        let Some(node) = nodes.get(i) else {
            return match end {
                End::Anchored(e) => (pos == *e).then_some(*e),
                End::Free { accept, .. } => accept(pos, env).then_some(pos),
            };
        };
        match node {
            Node::Lit(l) => {
                let pos = self.skip_comments(pos, limit);
                if pos + l.len() <= limit && self.src[pos..].starts_with(l) {
                    self.seq(nodes, i + 1, pos + l.len(), end, env)
                } else {
                    None
                }
            }
            Node::Space => {
                let after = self.skip_layout(pos, limit);
                if after == pos {
                    return None;
                }
                self.seq(nodes, i + 1, after, end, env)
            }
            Node::Group { open, close, body } => {
                if pos >= limit || self.src[pos] != *open || self.scan.kind(pos) != Kind::Open {
                    return None;
                }
                let group_end = self.scan.end(pos);
                if group_end > limit || self.src[group_end - 1] != *close {
                    return None;
                }
                let mark = env.len();
                let mut inner = End::Anchored(group_end - 1);
                self.seq(body, 0, pos + 1, &mut inner, env)?;
                let r = self.seq(nodes, i + 1, group_end, end, env);
                if r.is_none() {
                    env.truncate(mark);
                }
                r
            }
            Node::Hole(h) => self.hole(h, nodes, i, pos, end, env),
        }
    }

    /// Binds `h` to `pos..to` (checking a same-named binding) and matches
    /// the rest.
    fn bind_then(
        &mut self,
        h: &Hole,
        nodes: &[Node],
        i: usize,
        range: Range<usize>,
        end: &mut End<'_>,
        env: &mut Env,
    ) -> Option<usize> {
        if h.named()
            && let Some((_, prev)) = env.iter().find(|(n, _)| *n == h.name)
            && trimmed(self.src, prev) != trimmed(self.src, &range)
        {
            return None;
        }
        let next = range.end;
        env.push((h.name.clone(), range));
        let r = self.seq(nodes, i + 1, next, end, env);
        if r.is_none() {
            env.pop();
        }
        r
    }

    fn hole(
        &mut self,
        h: &Hole,
        nodes: &[Node],
        i: usize,
        pos: usize,
        end: &mut End<'_>,
        env: &mut Env,
    ) -> Option<usize> {
        let limit = Ctx::limit(end);
        let src = self.src;
        let run = |pred: &dyn Fn(u8) -> bool| {
            let mut q = pos;
            while q < limit && pred(src[q]) && self.scan.kind(q) != Kind::Close {
                q += 1;
            }
            q
        };
        match &h.kind {
            // Ending the template: everything to the end of the group or file.
            HoleKind::Everything if i + 1 == nodes.len() && matches!(end, End::Free { .. }) => {
                let ends = self.units(pos, limit, |_| true);
                self.steps = self.steps.saturating_add(ends.len() as u32);
                let q = ends.last().copied().unwrap_or(pos);
                self.bind_then(h, nodes, i, pos..q, end, env)
            }
            HoleKind::Everything => {
                // A following literal can only start where it occurs.
                let next_lit = match nodes.get(i + 1) {
                    Some(Node::Lit(l)) => Some(l.as_slice()),
                    _ => None,
                };
                let mut q = pos;
                loop {
                    if next_lit
                        .is_none_or(|l| src[self.skip_comments(q, limit)..limit].starts_with(l))
                    {
                        if let Some(e) = self.bind_then(h, nodes, i, pos..q, end, env) {
                            return Some(e);
                        }
                        if self.steps > STEP_BUDGET {
                            return None;
                        }
                    }
                    if q >= limit {
                        return None;
                    }
                    match self.scan.unit_end(src, q) {
                        Some(u) if u <= limit => q = u,
                        _ => return None,
                    }
                }
            }
            // The longest run of units without whitespace, then shorter ones.
            HoleKind::Expression => {
                for q in self.units(pos, limit, |b| !is_ws(b)).into_iter().rev() {
                    if let Some(e) = self.bind_then(h, nodes, i, pos..q, end, env) {
                        return Some(e);
                    }
                }
                None
            }
            HoleKind::Identifier => {
                let q = run(&is_word);
                if q == pos {
                    return None;
                }
                self.bind_then(h, nodes, i, pos..q, end, env)
            }
            HoleKind::Blank => {
                let q = run(&|b| b == b' ' || b == b'\t');
                if q == pos {
                    return None;
                }
                self.bind_then(h, nodes, i, pos..q, end, env)
            }
            // Up to and including the newline, which must be there.
            HoleKind::Line => {
                let q = pos + memchr::memchr(b'\n', &src[pos..limit])? + 1;
                self.bind_then(h, nodes, i, pos..q, end, env)
            }
            // Greedy: the longest run first.
            HoleKind::NonSpace => {
                let max = run(&|b| {
                    !is_ws(b) && crate::scan::is_open(b).is_none() && !crate::scan::is_close(b)
                });
                let mut ends = Vec::new();
                let mut q = pos;
                while q < max {
                    q = (q + char_len(src[q])).min(max);
                    ends.push(q);
                }
                for q in ends.into_iter().rev() {
                    if let Some(e) = self.bind_then(h, nodes, i, pos..q, end, env) {
                        return Some(e);
                    }
                }
                None
            }
            HoleKind::Regex(re) => {
                let m = re.find(&src[pos..limit])?;
                self.bind_then(h, nodes, i, pos..pos + m.end(), end, env)
            }
        }
    }
}
