//! Checks of a policy's fields that need no database, shared by the API and the resources file.

use crate::store::{GitObjectType as DbObjectType, PolicyFields};
use sourcegull_types::glob::Glob;

/// Longest duration a policy may name (ten years).
const MAX_DURATION_HOURS: i32 = 87_600;

/// Compiles repository patterns, or the validation messages.
pub fn compile_globs(patterns: &[String]) -> std::result::Result<Vec<Glob>, Vec<String>> {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for p in patterns {
        match Glob::new(p.trim()) {
            Ok(_) if p.trim().is_empty() => {
                errors.push("Repository patterns must not be empty.".into())
            }
            Ok(g) => out.push(g),
            Err(e) => errors.push(e.to_string()),
        }
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

/// Checks a policy's fields (`None` type: not one of GIT_COMMIT, GIT_TREE and GIT_TAG), for the
/// API and the resources file.
#[allow(clippy::too_many_arguments)]
pub fn check_policy_fields(
    repository_patterns: Option<Vec<String>>,
    name: &str,
    object_type: Option<DbObjectType>,
    pattern: &str,
    retention_enabled: bool,
    retention_duration_hours: Option<i32>,
    retain_intermediate_commits: bool,
    indexing_enabled: bool,
    syntactic_indexing_enabled: bool,
    index_commit_max_age_hours: Option<i32>,
    index_intermediate_commits: bool,
) -> std::result::Result<PolicyFields, Vec<String>> {
    let mut errors: Vec<String> = Vec::new();
    let name = name.trim();
    if name.is_empty() {
        errors.push("The name must not be empty.".into());
    }
    let db_type = object_type;
    let pattern = pattern.trim();
    match db_type {
        None => errors.push("The type must be GIT_COMMIT, GIT_TREE or GIT_TAG.".into()),
        Some(_) if pattern.is_empty() => errors.push("The pattern must not be empty.".into()),
        Some(DbObjectType::Commit) => {}
        Some(_) => {
            if let Err(e) = Glob::new(pattern) {
                errors.push(e.to_string());
            }
        }
    }
    let repository_patterns = match repository_patterns {
        None => None,
        Some(p) if p.is_empty() => {
            errors.push("Give at least one repository pattern, or none at all.".into());
            None
        }
        Some(p) => {
            if let Err(e) = compile_globs(&p) {
                errors.extend(e);
            }
            Some(p.into_iter().map(|s| s.trim().to_owned()).collect())
        }
    };
    for (label, hours) in [
        ("retentionDurationHours", retention_duration_hours),
        ("indexCommitMaxAgeHours", index_commit_max_age_hours),
    ] {
        if let Some(h) = hours
            && !(1..=MAX_DURATION_HOURS).contains(&h)
        {
            errors.push(format!(
                "{label} must be between 1 and {MAX_DURATION_HOURS}, or null for no limit."
            ));
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(PolicyFields {
        repository_patterns,
        name: name.to_owned(),
        object_type: db_type.unwrap_or(DbObjectType::Branch),
        pattern: pattern.to_owned(),
        retention_enabled,
        retention_duration_hours,
        retain_intermediate_commits,
        indexing_enabled,
        index_commit_max_age_hours,
        index_intermediate_commits,
        syntactic_indexing_enabled,
    })
}
