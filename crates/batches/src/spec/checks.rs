//! Semantic checks after schema validation.
//!
//! Errors:
//! - steps without a `changesetTemplate`;
//! - `,` in a mount path or mountpoint, or in a `files` target path (they are passed to Docker's
//!   comma-separated `--mount` syntax);
//! - `transformChanges` groups that would give two changesets of a repository the same branch,
//!   or a group branch equal to `changesetTemplate.branch` (found up front for every repository
//!   named in the groups, rather than while building changeset specs);
//! - globs that do not compile, in `changesetTemplate.published` rules and `workspaces[].in`.
//!
//! Warnings, for specs that are accepted but probably not what was meant:
//! - a `changesetTemplate` (or its `published`) that cannot have an effect because there are no
//!   steps, e.g. a spec that only imports changesets;
//! - an output name defined by more than one step (the later step's value replaces the earlier
//!   one); names within one step are unique already (duplicate YAML keys are errors);
//! - an environment variable listed twice in one step;
//! - the same changeset imported twice.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::{BatchSpec, Group};
use crate::error::{SpecIssue, with_field};
use crate::glob::Glob;
use crate::yaml::{Positions, escape_pointer_segment};

pub(super) fn check(spec: &BatchSpec, pos: &Positions) -> (Vec<SpecIssue>, Vec<SpecIssue>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let at = |path: String, message: String| {
        let location = pos.value(&path);
        SpecIssue::new(path, message).at(location)
    };
    let at_key = |path: String, message: String| {
        let location = pos.key(&path);
        SpecIssue::new(path, message).at(location)
    };

    if !spec.steps.is_empty() && spec.changeset_template.is_none() {
        errors.push(at(
            "/steps".into(),
            "bulk change spec includes steps but no changesetTemplate".into(),
        ));
    }

    let mut output_owner: HashMap<&str, usize> = HashMap::new();
    for (i, step) in spec.steps.iter().enumerate() {
        let n = i + 1;
        for (j, mount) in step.mount.iter().enumerate() {
            if mount.path.contains(',') {
                errors.push(at(
                    format!("/steps/{i}/mount/{j}/path"),
                    format!("step {n} mount path contains invalid characters"),
                ));
            }
            if mount.mountpoint.contains(',') {
                errors.push(at(
                    format!("/steps/{i}/mount/{j}/mountpoint"),
                    format!("step {n} mount mountpoint contains invalid characters"),
                ));
            }
        }
        for name in step.files.keys() {
            if name.contains(',') {
                errors.push(at_key(
                    format!("/steps/{i}/files/{}", escape_pointer_segment(name)),
                    format!("step {n} files target path contains invalid characters"),
                ));
            }
        }
        for name in step.outputs.keys() {
            if let Some(first) = output_owner.insert(name, n) {
                warnings.push(at_key(
                    format!("/steps/{i}/outputs/{}", escape_pointer_segment(name)),
                    format!(
                        "step {n} redefines output {name:?} of step {first}; outputs.{name} has this step's value from here on"
                    ),
                ));
            }
        }
        let mut env_names = HashSet::new();
        for (j, var) in step.env.vars().iter().enumerate() {
            if !env_names.insert(var.name.as_str()) {
                warnings.push(at(
                    format!("/steps/{i}/env/{j}"),
                    format!(
                        "step {n} sets the environment variable {:?} more than once; the last entry wins",
                        var.name
                    ),
                ));
            }
        }
    }

    for (i, ws) in spec.workspaces.iter().enumerate() {
        if let Some(Err(e)) = ws.in_glob() {
            errors.push(at(
                format!("/workspaces/{i}/in"),
                with_field(&format!("/workspaces/{i}/in"), &e),
            ));
        }
    }

    if let Some(tmpl) = &spec.changeset_template {
        if spec.steps.is_empty() {
            if tmpl.published.is_some() && !spec.import_changesets.is_empty() {
                warnings.push(at(
                    "/changesetTemplate/published".into(),
                    "changesetTemplate.published has no effect: the bulk change spec has no steps, and imported changesets keep their state on the code host".into(),
                ));
            } else {
                warnings.push(at(
                    "/changesetTemplate".into(),
                    "changesetTemplate has no effect: the bulk change spec has no steps".into(),
                ));
            }
        }
    }

    if let Some(tc) = &spec.transform_changes {
        let template_branch = spec.changeset_template.as_ref().map(|t| t.branch.as_str());
        check_groups(&tc.group, template_branch, pos, &mut errors);
    }

    let mut imported = HashSet::new();
    for (i, ic) in spec.import_changesets.iter().enumerate() {
        for (j, id) in ic.external_ids.iter().enumerate() {
            if !imported.insert((ic.repository.as_str(), id.to_string())) {
                warnings.push(at(
                    format!("/importChangesets/{i}/externalIDs/{j}"),
                    format!(
                        "changeset {id} of repository {} is imported more than once",
                        ic.repository
                    ),
                ));
            }
        }
    }

    (errors, warnings)
}

/// Globs of `changesetTemplate.published` rules, checked on the schema-valid JSON before
/// deserialization (which compiles them) so that a bad glob is reported at its key.
pub(super) fn check_published(value: &serde_json::Value, pos: &Positions) -> Vec<SpecIssue> {
    let Some(serde_json::Value::Array(rules)) = value.pointer("/changesetTemplate/published")
    else {
        return Vec::new();
    };
    let mut errors = Vec::new();
    for (i, rule) in rules.iter().enumerate() {
        let Some(map) = rule.as_object() else {
            continue;
        };
        for pattern in map.keys() {
            let glob = pattern.split_once('@').map_or(pattern.as_str(), |(g, _)| g);
            if let Err(e) = Glob::new(glob) {
                let path = format!(
                    "/changesetTemplate/published/{i}/{}",
                    escape_pointer_segment(pattern)
                );
                let location = pos.key(&path);
                errors.push(
                    SpecIssue::new(path, format!("changesetTemplate.published.{i}: {e}"))
                        .at(location),
                );
            }
        }
    }
    errors
}

fn check_groups(
    groups: &[Group],
    template_branch: Option<&str>,
    pos: &Positions,
    errors: &mut Vec<SpecIssue>,
) {
    let branch_issue = |i: usize, message: String| {
        let path = format!("/transformChanges/group/{i}/branch");
        let location = pos.value(&path);
        SpecIssue::new(path, message).at(location)
    };
    // Repositories named by a group; groups without one apply to all repositories.
    let named: BTreeSet<&str> = groups
        .iter()
        .filter_map(|g| g.repository.as_deref())
        .collect();
    let mut reported = HashSet::new();
    let mut scopes: Vec<Option<&str>> = vec![None];
    scopes.extend(named.iter().map(|r| Some(*r)));
    for scope in scopes {
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for (i, g) in groups.iter().enumerate() {
            let applies = match (scope, g.repository.as_deref()) {
                (_, None) => true,
                (Some(r), Some(gr)) => r == gr,
                (None, Some(_)) => false,
            };
            if !applies {
                continue;
            }
            let where_ = match scope {
                Some(r) => format!("repository {r}"),
                None => "every repository".to_string(),
            };
            if seen.insert(g.branch.as_str(), i).is_some() && reported.insert((i, "dup")) {
                errors.push(branch_issue(
                    i,
                    format!(
                        "transformChanges would lead to multiple changesets in {where_} to have the same branch {:?}",
                        g.branch
                    ),
                ));
            }
            if Some(g.branch.as_str()) == template_branch && reported.insert((i, "tmpl")) {
                let whose = match g.repository.as_deref() {
                    Some(r) => format!("for repository {r} "),
                    None => String::new(),
                };
                errors.push(branch_issue(
                    i,
                    format!(
                        "transformChanges group branch {whose}is the same as branch {:?} in changesetTemplate",
                        g.branch
                    ),
                ));
            }
        }
    }
}
