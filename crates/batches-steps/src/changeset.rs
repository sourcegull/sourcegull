//! Changeset specs from a workspace's final result.
//!
//! The `changesetTemplate` fields are rendered with the [`ChangesetTemplateContext`] (outputs,
//! changed files and the workspace path of the final result) and trimmed. Without
//! `transformChanges` groups for the repository there is one spec on the rendered branch;
//! with groups the diff is split per file: a file goes to the branch of the **last** group
//! whose `directory` is a substring of its path (not a prefix match), and to the template's
//! branch when none matches. `published` comes from the
//! template's flag or rules for the repository and branch; rules that match nothing give
//! `false`; without `published` it is left to the UI (`None`).
//!
//! Branches whose diff is empty get no spec, including the template's branch when every file
//! went to a group. Specs come out with the template branch first, then group branches in the
//! order of `transformChanges.group`.
//!
//! This needs no container: a fully cached workspace builds its specs from the cached
//! [`AfterStepResult`] alone.

use std::collections::BTreeMap;

use sourcegull_batches::spec::{BatchSpec, CommitAuthor, Group, TransformChanges};
use sourcegull_batches::{
    BranchChangesetSpec, ChangesetSpec, GitCommitDescription, PublishedValue,
};
use sourcegull_batches_template::{
    BatchChangeAttributes, ChangesetTemplateContext, Repository, StepsContext,
};

use crate::error::StepsError;
use crate::result::AfterStepResult;

/// The commit author when the template has none.
pub fn default_author() -> CommitAuthor {
    CommitAuthor {
        name: "Sourcegull".into(),
        email: "batch-changes@sourcegull.invalid".into(),
    }
}

/// The repository a workspace belongs to, as changeset specs name it.
#[derive(Debug, Clone, Default)]
pub struct SpecRepository {
    /// GraphQL ID (`baseRepository`/`headRepository`).
    pub id: String,
    /// Name (`gitlab.example.com/group/project`), for `published` rules and templates.
    pub name: String,
    /// `refs/heads/<branch>` the changesets target.
    pub base_ref: String,
    /// The commit the steps ran on.
    pub base_rev: String,
    /// Search result paths in the workspace (`repository.search_result_paths`).
    pub file_matches: Vec<String>,
}

/// Input of [`build_changeset_specs`].
#[derive(Debug, Clone)]
pub struct ChangesetSpecInput<'a> {
    pub spec: &'a BatchSpec,
    pub repository: &'a SpecRepository,
    /// The workspace path (`steps.path`): `""` for the repository root.
    pub path: &'a str,
    /// The final result of the workspace's steps.
    pub result: &'a AfterStepResult,
    /// Author when `changesetTemplate.commit.author` is unset; [`default_author`] when `None`.
    pub fallback_author: Option<&'a CommitAuthor>,
}

/// Builds the changeset specs of one workspace. No `changesetTemplate`, or an empty diff,
/// gives none.
pub fn build_changeset_specs(
    input: &ChangesetSpecInput<'_>,
) -> Result<Vec<ChangesetSpec>, StepsError> {
    let Some(template) = &input.spec.changeset_template else {
        return Ok(Vec::new());
    };
    if input.result.diff.is_empty() {
        return Ok(Vec::new());
    }
    let repo = input.repository;
    let base_branch = repo
        .base_ref
        .strip_prefix("refs/heads/")
        .unwrap_or(&repo.base_ref);
    let ctx = ChangesetTemplateContext {
        batch_change_attributes: BatchChangeAttributes {
            name: input.spec.name.clone(),
            description: input.spec.description.clone(),
        },
        steps: StepsContext {
            changes: input.result.changed_files.to_template(),
            path: input.path.to_owned(),
        },
        outputs: input.result.outputs.clone(),
        repository: Repository {
            name: repo.name.clone(),
            branch: base_branch.to_owned(),
            file_matches: repo.file_matches.clone(),
        },
    };
    let render = |field: &str, text: &str| {
        ctx.render_field(text)
            .map_err(|error| StepsError::ChangesetTemplate {
                field: field.to_owned(),
                error: Box::new(error),
            })
    };
    let (author_name, author_email) = match &template.commit.author {
        Some(a) => (
            render("commit.author.name", &a.name)?,
            render("commit.author.email", &a.email)?,
        ),
        None => {
            let a = input
                .fallback_author
                .cloned()
                .unwrap_or_else(default_author);
            (a.name, a.email)
        }
    };
    let title = render("title", &template.title)?;
    let body = render("body", &template.body)?;
    let message = render("commit.message", &template.commit.message)?;
    let default_branch = render("branch", &template.branch)?;

    let new_spec = |branch: &str, diff: Vec<u8>| {
        let published = template.published.as_ref().map(|p| {
            p.value_for_branch(&repo.name, branch)
                .unwrap_or(PublishedValue::Unpublished)
        });
        ChangesetSpec::Branch(BranchChangesetSpec {
            version: None,
            base_repository: repo.id.clone(),
            base_ref: repo.base_ref.clone(),
            base_rev: repo.base_rev.clone(),
            head_repository: repo.id.clone(),
            head_ref: ensure_ref_prefix(branch),
            title: title.clone(),
            body: body.clone(),
            fork: template.fork,
            commits: vec![GitCommitDescription {
                version: 2,
                message: message.clone(),
                diff,
                author_name: author_name.clone(),
                author_email: author_email.clone(),
            }],
            published,
        })
    };

    let groups = input
        .spec
        .transform_changes
        .as_ref()
        .map(|t| t.groups_for_repository(&repo.name))
        .unwrap_or_default();
    if groups.is_empty() {
        return Ok(vec![new_spec(&default_branch, input.result.diff.clone())]);
    }
    TransformChanges::validate_groups(&repo.name, &template.branch, &groups)
        .map_err(StepsError::TransformChanges)?;
    let by_branch = group_file_diffs(&input.result.diff, &default_branch, &groups);
    Ok(by_branch
        .into_iter()
        .filter(|(_, diff)| !diff.is_empty())
        .map(|(branch, diff)| new_spec(&branch, diff))
        .collect())
}

/// `refs/heads/` prepended unless the name already is a ref.
pub fn ensure_ref_prefix(branch: &str) -> String {
    if branch.starts_with("refs/heads/") {
        branch.to_owned()
    } else {
        format!("refs/heads/{branch}")
    }
}

/// Splits a multi-file diff by `transformChanges` groups: the
/// branch of the last group whose directory is contained in the file's path, else
/// `default_branch`. Returns `(branch, diff)` with the default branch first, then the group
/// branches in group order; file diffs keep their order.
pub fn group_file_diffs(
    patch: &[u8],
    default_branch: &str,
    groups: &[&Group],
) -> Vec<(String, Vec<u8>)> {
    let mut order: Vec<String> = vec![default_branch.to_owned()];
    for g in groups {
        if !order.contains(&g.branch) {
            order.push(g.branch.clone());
        }
    }
    let mut by_branch: BTreeMap<String, Vec<u8>> =
        order.iter().map(|b| (b.clone(), Vec::new())).collect();
    for file in split_file_diffs(patch) {
        let name = file_diff_name(file);
        let branch = groups
            .iter()
            .rev()
            .find(|g| name.contains(g.directory.as_str()))
            .map(|g| g.branch.as_str())
            .unwrap_or(default_branch);
        if let Some(out) = by_branch.get_mut(branch) {
            out.extend_from_slice(file);
        }
    }
    order
        .into_iter()
        .map(|b| {
            let diff = by_branch.remove(&b).unwrap_or_default();
            (b, diff)
        })
        .collect()
}

/// The per-file parts of a `git diff`: each starts at a `diff --git ` line (text before the
/// first one is dropped).
pub fn split_file_diffs(patch: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut at = 0;
    while at < patch.len() {
        if patch[at..].starts_with(b"diff --git ") {
            starts.push(at);
        }
        match patch[at..].iter().position(|&b| b == b'\n') {
            Some(nl) => at += nl + 1,
            None => break,
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (i, &s) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(patch.len());
        out.push(&patch[s..end]);
    }
    out
}

/// The path a file diff is about: the new name, or the old one for deletions (go-diff's
/// `NewName`/`OrigName` as `groupFileDiffs` uses them). From the `+++`/`---` lines, else the
/// `rename to`/`copy to` lines, else the `diff --git` line (binary changes, mode changes).
pub fn file_diff_name(file: &[u8]) -> String {
    let text = String::from_utf8_lossy(file);
    let mut old = None;
    let mut new = None;
    let mut renamed = None;
    for line in text.lines().skip(1) {
        if let Some(rest) = line.strip_prefix("--- ") {
            old = Some(header_path(rest));
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            new = Some(header_path(rest));
            break;
        } else if let Some(rest) = line
            .strip_prefix("rename to ")
            .or_else(|| line.strip_prefix("copy to "))
        {
            renamed = Some(unquote(rest));
        } else if line.starts_with("@@") || line.starts_with("GIT binary patch") {
            break;
        }
    }
    match (new, old) {
        (Some(n), Some(o)) if n == "/dev/null" => o,
        (Some(n), _) => n,
        _ => renamed.unwrap_or_else(|| {
            let first = text.lines().next().unwrap_or_default();
            let names = first.strip_prefix("diff --git ").unwrap_or_default();
            // Without prefixes, `diff --git x x` names the same path twice.
            let half = names.len() / 2;
            if names.is_char_boundary(half) && names.len() % 2 == 1 {
                unquote(&names[half + 1..])
            } else {
                unquote(names)
            }
        }),
    }
}

/// The path of a `---`/`+++` header line (a tab and timestamp may follow).
fn header_path(rest: &str) -> String {
    unquote(rest.split('\t').next().unwrap_or(rest))
}

/// Git's C-style quoting of unusual paths, removed for the common escapes.
fn unquote(s: &str) -> String {
    let Some(inner) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return s.to_owned();
    };
    let mut out = Vec::new();
    let bytes = inner.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            let c = bytes[i + 1];
            if i + 4 <= bytes.len()
                && bytes[i + 1..i + 4]
                    .iter()
                    .all(|b| (b'0'..=b'7').contains(b))
            {
                let v = bytes[i + 1..i + 4]
                    .iter()
                    .fold(0u32, |acc, b| acc * 8 + u32::from(b - b'0'));
                out.push(v as u8);
                i += 4;
                continue;
            }
            out.push(match c {
                b'n' => b'\n',
                b't' => b'\t',
                other => other,
            });
            i += 2;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "diff --git a/x.go a/x.go\nindex 1..2 100644\n--- a/x.go\n+++ a/x.go\n@@ -1 +1 @@\n-a\n+b\ndiff --git b/y.go b/y.go\ndeleted file mode 100644\nindex 1..0\n--- b/y.go\n+++ /dev/null\n@@ -1 +0,0 @@\n-a\ndiff --git c.bin c.bin\nnew file mode 100644\nindex 0..1\nGIT binary patch\nliteral 1\nIcmZpX000310RR91\n\nliteral 0\nHcmV?d00001\n\ndiff --git old.txt new.txt\nsimilarity index 100%\nrename from old.txt\nrename to new.txt\n";

    #[test]
    fn names() {
        let files = split_file_diffs(PATCH.as_bytes());
        let names: Vec<String> = files.iter().map(|f| file_diff_name(f)).collect();
        assert_eq!(names, ["a/x.go", "b/y.go", "c.bin", "new.txt"]);
        assert_eq!(files.concat(), PATCH.as_bytes());
        assert_eq!(unquote("\"caf\\303\\251 x\""), "café x");
    }

    #[test]
    fn grouping() {
        let a = Group {
            directory: "a/".into(),
            branch: "branch-a".into(),
            repository: None,
        };
        let b = Group {
            directory: "b".into(),
            branch: "branch-b".into(),
            repository: None,
        };
        // "b" also occurs in "c.bin"; the last matching group wins.
        let groups = vec![&a, &b];
        let out = group_file_diffs(PATCH.as_bytes(), "main-branch", &groups);
        let branches: Vec<&str> = out.iter().map(|(b, _)| b.as_str()).collect();
        assert_eq!(branches, ["main-branch", "branch-a", "branch-b"]);
        let text = |i: usize| String::from_utf8(out[i].1.clone()).unwrap();
        assert!(text(0).starts_with("diff --git old.txt new.txt"));
        assert!(text(1).starts_with("diff --git a/x.go"));
        assert!(text(2).contains("b/y.go") && text(2).contains("c.bin"));
    }
}
