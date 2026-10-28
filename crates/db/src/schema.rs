//! Schema descriptions and drift detection (`sourcegull migrate drift`).
//!
//! [`describe`] reads the live schema of the `public` schema from `pg_catalog`: extensions,
//! enums, tables with their columns (type, nullability, default, identity, generated),
//! constraints, indexes and triggers, views and functions. The migration history table is left
//! out. The result is a [`SchemaDescription`], serialized as pretty JSON with sorted keys, so
//! the descriptions committed for each release (`crates/db/schema/`, see
//! [`crate::upgrade`]) diff well in git.
//!
//! [`diff`] compares an expected description with the live one and returns every difference
//! as a [`Drift`] with SQL that would make the live schema match. The fix SQL is a suggestion
//! to review, not something to run blindly: a dropped column cannot bring its data back.
//!
//! Definitions come from Postgres' own deparsers (`pg_get_indexdef`, `pg_get_constraintdef`,
//! `pg_get_functiondef`, ...), whose output can change between Postgres major versions;
//! descriptions are generated on the supported major version (16).

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};

use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::Result;

/// The schema described.
pub const SCHEMA: &str = "public";
/// Tables left out of descriptions (the migration history).
const IGNORED_TABLES: &[&str] = &["_sqlx_migrations"];

/// The shape of a database schema.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SchemaDescription {
    /// Installed extensions (except `plpgsql`, which every database has).
    pub extensions: Vec<String>,
    /// Enum types and their labels in order.
    pub enums: BTreeMap<String, Vec<String>>,
    /// Functions and procedures by `name(identity arguments)`.
    pub functions: BTreeMap<String, Function>,
    /// Tables (plain and partitioned) by name.
    pub tables: BTreeMap<String, Table>,
    /// Views and materialized views by name.
    pub views: BTreeMap<String, View>,
}

/// A table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Table {
    /// Columns in their physical order.
    pub columns: Vec<Column>,
    /// Constraints (primary key, unique, foreign key, check, exclusion) by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub constraints: BTreeMap<String, Constraint>,
    /// Indexes by name, including those backing constraints.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub indexes: BTreeMap<String, Index>,
    /// User triggers by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub triggers: BTreeMap<String, Trigger>,
}

/// A column.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    /// Column name.
    pub name: String,
    /// Type as `format_type` prints it (`text`, `timestamp with time zone`, `integer[]`).
    #[serde(rename = "type")]
    pub type_name: String,
    /// Whether NULL is allowed.
    pub nullable: bool,
    /// Default expression (or the generation expression of a generated column).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// `always` or `by default` for identity columns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Whether this is a stored generated column (`default` holds the expression).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub generated: bool,
}

/// A table constraint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraint {
    /// `primary key`, `unique`, `foreign key`, `check` or `exclusion`.
    pub kind: String,
    /// `pg_get_constraintdef` output.
    pub definition: String,
}

/// An index.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    /// `pg_get_indexdef` output.
    pub definition: String,
    /// Whether it backs a primary key, unique or exclusion constraint (then it is created and
    /// dropped with the constraint).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub constraint: bool,
}

/// A trigger.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trigger {
    /// `pg_get_triggerdef` output.
    pub definition: String,
}

/// A view.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    /// `pg_get_viewdef` output (the query).
    pub definition: String,
    /// Whether it is a materialized view.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub materialized: bool,
}

/// A function or procedure.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Function {
    /// `pg_get_functiondef` output (`CREATE OR REPLACE FUNCTION ...`).
    pub definition: String,
}

impl SchemaDescription {
    /// Pretty JSON (keys sorted, two-space indent) with a trailing newline.
    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("schema descriptions serialize");
        s.push('\n');
        s
    }

    /// Parses [`to_json`](Self::to_json) output.
    pub fn from_json(s: &str) -> serde_json::Result<Self> {
        serde_json::from_str(s)
    }
}

/// Reads the live schema.
pub async fn describe(pool: &PgPool) -> Result<SchemaDescription> {
    let mut d = SchemaDescription {
        extensions: sqlx::query_scalar(
            "SELECT extname::text FROM pg_extension WHERE extname <> 'plpgsql' ORDER BY 1",
        )
        .fetch_all(pool)
        .await?,
        ..SchemaDescription::default()
    };

    let enums: Vec<(String, Vec<String>)> = sqlx::query_as(
        "SELECT t.typname::text, array_agg(e.enumlabel::text ORDER BY e.enumsortorder)
         FROM pg_type t
         JOIN pg_enum e ON e.enumtypid = t.oid
         JOIN pg_namespace n ON n.oid = t.typnamespace
         WHERE n.nspname = $1
         GROUP BY 1",
    )
    .bind(SCHEMA)
    .fetch_all(pool)
    .await?;
    d.enums = enums.into_iter().collect();

    let functions: Vec<(String, String)> = sqlx::query_as(
        "SELECT p.proname || '(' || pg_get_function_identity_arguments(p.oid) || ')',
                pg_get_functiondef(p.oid)
         FROM pg_proc p
         JOIN pg_namespace n ON n.oid = p.pronamespace
         WHERE n.nspname = $1 AND p.prokind IN ('f', 'p')
           AND NOT EXISTS (
             SELECT 1 FROM pg_depend dep
             WHERE dep.classid = 'pg_proc'::regclass AND dep.objid = p.oid AND dep.deptype = 'e')",
    )
    .bind(SCHEMA)
    .fetch_all(pool)
    .await?;
    d.functions = functions
        .into_iter()
        .map(|(name, definition)| {
            (
                name,
                Function {
                    definition: definition.trim_end().to_owned(),
                },
            )
        })
        .collect();

    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = $1 AND c.relkind IN ('r', 'p') AND NOT c.relispartition",
    )
    .bind(SCHEMA)
    .fetch_all(pool)
    .await?;
    for t in tables {
        if !IGNORED_TABLES.contains(&t.as_str()) {
            d.tables.insert(t, Table::default());
        }
    }

    type ColumnRow = (String, String, String, bool, Option<String>, String, String);
    let columns: Vec<ColumnRow> = sqlx::query_as(
        "SELECT c.relname::text, a.attname::text, format_type(a.atttypid, a.atttypmod),
                NOT a.attnotnull, pg_get_expr(ad.adbin, ad.adrelid),
                a.attidentity::text, a.attgenerated::text
         FROM pg_attribute a
         JOIN pg_class c ON c.oid = a.attrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace
         LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
         WHERE n.nspname = $1 AND c.relkind IN ('r', 'p') AND a.attnum > 0 AND NOT a.attisdropped
         ORDER BY c.relname, a.attnum",
    )
    .bind(SCHEMA)
    .fetch_all(pool)
    .await?;
    for (table, name, type_name, nullable, default, identity, generated) in columns {
        let Some(t) = d.tables.get_mut(&table) else {
            continue;
        };
        t.columns.push(Column {
            name,
            type_name,
            nullable,
            default,
            identity: match identity.as_str() {
                "a" => Some("always".to_owned()),
                "d" => Some("by default".to_owned()),
                _ => None,
            },
            generated: generated == "s",
        });
    }

    let constraints: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT c.relname::text, con.conname::text, con.contype::text, pg_get_constraintdef(con.oid)
         FROM pg_constraint con
         JOIN pg_class c ON c.oid = con.conrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = $1 AND con.contype IN ('p', 'u', 'f', 'c', 'x')",
    )
    .bind(SCHEMA)
    .fetch_all(pool)
    .await?;
    for (table, name, kind, definition) in constraints {
        let Some(t) = d.tables.get_mut(&table) else {
            continue;
        };
        let kind = match kind.as_str() {
            "p" => "primary key",
            "u" => "unique",
            "f" => "foreign key",
            "c" => "check",
            _ => "exclusion",
        };
        t.constraints.insert(
            name,
            Constraint {
                kind: kind.to_owned(),
                definition,
            },
        );
    }

    let indexes: Vec<(String, String, String, bool)> = sqlx::query_as(
        "SELECT t.relname::text, i.relname::text, pg_get_indexdef(i.oid),
                EXISTS (SELECT 1 FROM pg_constraint con
                        WHERE con.conindid = i.oid AND con.conrelid = t.oid
                          AND con.contype IN ('p', 'u', 'x'))
         FROM pg_index x
         JOIN pg_class i ON i.oid = x.indexrelid
         JOIN pg_class t ON t.oid = x.indrelid
         JOIN pg_namespace n ON n.oid = t.relnamespace
         WHERE n.nspname = $1 AND t.relkind IN ('r', 'p')",
    )
    .bind(SCHEMA)
    .fetch_all(pool)
    .await?;
    for (table, name, definition, constraint) in indexes {
        if let Some(t) = d.tables.get_mut(&table) {
            t.indexes.insert(
                name,
                Index {
                    definition,
                    constraint,
                },
            );
        }
    }

    let triggers: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT c.relname::text, tg.tgname::text, pg_get_triggerdef(tg.oid)
         FROM pg_trigger tg
         JOIN pg_class c ON c.oid = tg.tgrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = $1 AND NOT tg.tgisinternal",
    )
    .bind(SCHEMA)
    .fetch_all(pool)
    .await?;
    for (table, name, definition) in triggers {
        if let Some(t) = d.tables.get_mut(&table) {
            t.triggers.insert(name, Trigger { definition });
        }
    }

    let views: Vec<(String, bool, String)> = sqlx::query_as(
        "SELECT c.relname::text, c.relkind = 'm', pg_get_viewdef(c.oid)
         FROM pg_class c
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = $1 AND c.relkind IN ('v', 'm')",
    )
    .bind(SCHEMA)
    .fetch_all(pool)
    .await?;
    d.views = views
        .into_iter()
        .map(|(name, materialized, definition)| {
            (
                name,
                View {
                    definition: definition.trim().to_owned(),
                    materialized,
                },
            )
        })
        .collect();

    Ok(d)
}

/// One difference between the expected and the live schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift {
    /// The object, e.g. `table "repo"`, `column "repo.name"`, `index "repo_name_idx"`.
    pub object: String,
    /// What is wrong, e.g. `missing`, `unexpected`, `type is text, expected integer`.
    pub problem: String,
    /// Expected and actual definitions, when they differ (for the report).
    pub expected: Option<String>,
    /// The live definition.
    pub actual: Option<String>,
    /// SQL statements that would make the live schema match (review before running).
    pub fix: Vec<String>,
}

impl fmt::Display for Drift {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}: {}", self.object, self.problem)?;
        if let Some(e) = &self.expected {
            writeln!(f, "    expected: {}", one_line(e))?;
        }
        if let Some(a) = &self.actual {
            writeln!(f, "    actual:   {}", one_line(a))?;
        }
        for sql in &self.fix {
            for (i, line) in sql.lines().enumerate() {
                let prefix = if i == 0 {
                    "    fix:      "
                } else {
                    "              "
                };
                writeln!(f, "{prefix}{line}")?;
            }
        }
        Ok(())
    }
}

/// Compresses whitespace so long definitions fit one report line.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Renders drifts as a readable report with a summary line.
pub fn report(drifts: &[Drift]) -> String {
    let mut out = String::new();
    for d in drifts {
        let _ = write!(out, "{d}");
    }
    out
}

/// Every difference between `expected` and `actual`, in dependency order (extensions and
/// enums, then tables and their parts, functions, views, triggers).
pub fn diff(expected: &SchemaDescription, actual: &SchemaDescription) -> Vec<Drift> {
    let mut out = Vec::new();
    for ext in &expected.extensions {
        if !actual.extensions.contains(ext) {
            out.push(drift(
                format!("extension {}", quote_lit_name(ext)),
                "missing",
                vec![format!("CREATE EXTENSION IF NOT EXISTS {};", ident(ext))],
            ));
        }
    }
    // Extra extensions (pg_stat_statements, ...) are the operator's business, not drift.

    diff_maps(
        &expected.enums,
        &actual.enums,
        |name, labels| {
            drift(
                format!("enum {}", quote_lit_name(name)),
                "missing",
                vec![format!(
                    "CREATE TYPE {} AS ENUM ({});",
                    ident(name),
                    labels
                        .iter()
                        .map(|l| literal(l))
                        .collect::<Vec<_>>()
                        .join(", ")
                )],
            )
        },
        |name, _| {
            drift(
                format!("enum {}", quote_lit_name(name)),
                "unexpected",
                vec![format!("DROP TYPE {};", ident(name))],
            )
        },
        |name, want, have| {
            let only_appended = have.iter().all(|l| want.contains(l)) && {
                // The live labels must keep the expected relative order.
                let order: Vec<&String> = want.iter().filter(|l| have.contains(l)).collect();
                order.iter().copied().eq(have.iter())
            };
            let fix = if only_appended {
                // Add each missing label next to a neighbour that exists by then.
                let mut present: Vec<&String> = have.iter().collect();
                let mut fix = Vec::new();
                for (pos, l) in want.iter().enumerate() {
                    if present.contains(&l) {
                        continue;
                    }
                    let place = match want[..pos].iter().rev().find(|p| present.contains(p)) {
                        Some(after) => format!("AFTER {}", literal(after)),
                        None => match want[pos + 1..].iter().find(|n| present.contains(n)) {
                            Some(before) => format!("BEFORE {}", literal(before)),
                            None => String::new(),
                        },
                    };
                    fix.push(
                        format!(
                            "ALTER TYPE {} ADD VALUE {} {place}",
                            ident(name),
                            literal(l)
                        )
                        .trim_end()
                        .to_owned()
                            + ";",
                    );
                    present.push(l);
                }
                fix
            } else {
                vec![format!(
                    "-- enum labels cannot be removed or reordered in place: recreate type {} with ({})",
                    ident(name),
                    want.iter()
                        .map(|l| literal(l))
                        .collect::<Vec<_>>()
                        .join(", ")
                )]
            };
            vec![Drift {
                object: format!("enum {}", quote_lit_name(name)),
                problem: "labels differ".to_owned(),
                expected: Some(want.join(", ")),
                actual: Some(have.join(", ")),
                fix,
            }]
        },
        &mut out,
    );

    diff_maps(
        &expected.tables,
        &actual.tables,
        |name, t| {
            drift(
                format!("table {}", quote_lit_name(name)),
                "missing",
                create_table(name, t),
            )
        },
        |name, _| {
            drift(
                format!("table {}", quote_lit_name(name)),
                "unexpected",
                vec![format!("DROP TABLE {};", ident(name))],
            )
        },
        diff_table,
        &mut out,
    );

    diff_maps(
        &expected.functions,
        &actual.functions,
        |name, f| {
            drift(
                format!("function {}", quote_lit_name(name)),
                "missing",
                vec![format!("{};", f.definition)],
            )
        },
        |name, _| {
            drift(
                format!("function {}", quote_lit_name(name)),
                "unexpected",
                vec![format!("DROP FUNCTION {name};")],
            )
        },
        |name, want, have| {
            vec![Drift {
                object: format!("function {}", quote_lit_name(name)),
                problem: "definition differs".to_owned(),
                expected: Some(want.definition.clone()),
                actual: Some(have.definition.clone()),
                fix: vec![format!("{};", want.definition)],
            }]
        },
        &mut out,
    );

    diff_maps(
        &expected.views,
        &actual.views,
        |name, v| {
            drift(
                format!("view {}", quote_lit_name(name)),
                "missing",
                create_view(name, v),
            )
        },
        |name, v| {
            drift(
                format!("view {}", quote_lit_name(name)),
                "unexpected",
                vec![format!(
                    "DROP {}VIEW {};",
                    if v.materialized { "MATERIALIZED " } else { "" },
                    ident(name)
                )],
            )
        },
        |name, want, have| {
            let mut fix = Vec::new();
            if want.materialized || have.materialized {
                fix.push(format!(
                    "DROP {}VIEW {};",
                    if have.materialized {
                        "MATERIALIZED "
                    } else {
                        ""
                    },
                    ident(name)
                ));
            }
            fix.extend(create_view(name, want));
            vec![Drift {
                object: format!("view {}", quote_lit_name(name)),
                problem: "definition differs".to_owned(),
                expected: Some(want.definition.clone()),
                actual: Some(have.definition.clone()),
                fix,
            }]
        },
        &mut out,
    );
    out
}

fn drift(object: String, problem: &str, fix: Vec<String>) -> Drift {
    Drift {
        object,
        problem: problem.to_owned(),
        expected: None,
        actual: None,
        fix,
    }
}

/// Walks two maps: `missing` for keys only in `want`, `extra` for keys only in `have`,
/// `changed` for keys in both whose values differ.
fn diff_maps<V: PartialEq>(
    want: &BTreeMap<String, V>,
    have: &BTreeMap<String, V>,
    missing: impl Fn(&str, &V) -> Drift,
    extra: impl Fn(&str, &V) -> Drift,
    changed: impl Fn(&str, &V, &V) -> Vec<Drift>,
    out: &mut Vec<Drift>,
) {
    for (name, w) in want {
        match have.get(name) {
            None => out.push(missing(name, w)),
            Some(h) if h != w => out.extend(changed(name, w, h)),
            Some(_) => {}
        }
    }
    for (name, h) in have {
        if !want.contains_key(name) {
            out.push(extra(name, h));
        }
    }
}

fn diff_table(name: &str, want: &Table, have: &Table) -> Vec<Drift> {
    let mut out = Vec::new();
    let t = ident(name);
    let col_obj = |c: &str| format!("column {}", quote_lit_name(&format!("{name}.{c}")));
    for w in &want.columns {
        let Some(h) = have.columns.iter().find(|h| h.name == w.name) else {
            out.push(drift(
                col_obj(&w.name),
                "missing",
                vec![format!("ALTER TABLE {t} ADD COLUMN {};", column_def(w))],
            ));
            continue;
        };
        let c = ident(&w.name);
        if w.type_name != h.type_name {
            out.push(Drift {
                object: col_obj(&w.name),
                problem: "type differs".to_owned(),
                expected: Some(w.type_name.clone()),
                actual: Some(h.type_name.clone()),
                fix: vec![format!(
                    "ALTER TABLE {t} ALTER COLUMN {c} TYPE {ty} USING {c}::{ty};",
                    ty = w.type_name
                )],
            });
        }
        if w.nullable != h.nullable {
            out.push(drift(
                col_obj(&w.name),
                if w.nullable {
                    "is NOT NULL, expected nullable"
                } else {
                    "is nullable, expected NOT NULL"
                },
                vec![format!(
                    "ALTER TABLE {t} ALTER COLUMN {c} {} NOT NULL;",
                    if w.nullable { "DROP" } else { "SET" }
                )],
            ));
        }
        if w.identity != h.identity || w.generated != h.generated {
            out.push(Drift {
                object: col_obj(&w.name),
                problem: "identity or generation differs".to_owned(),
                expected: Some(column_def(w)),
                actual: Some(column_def(h)),
                fix: vec![format!(
                    "-- recreate column {c} of {t} as: {}",
                    column_def(w)
                )],
            });
        } else if w.default != h.default && !w.generated {
            out.push(Drift {
                object: col_obj(&w.name),
                problem: "default differs".to_owned(),
                expected: Some(w.default.clone().unwrap_or_else(|| "(none)".to_owned())),
                actual: Some(h.default.clone().unwrap_or_else(|| "(none)".to_owned())),
                fix: vec![match &w.default {
                    Some(d) => format!("ALTER TABLE {t} ALTER COLUMN {c} SET DEFAULT {d};"),
                    None => format!("ALTER TABLE {t} ALTER COLUMN {c} DROP DEFAULT;"),
                }],
            });
        }
    }
    for h in &have.columns {
        if !want.columns.iter().any(|w| w.name == h.name) {
            out.push(drift(
                col_obj(&h.name),
                "unexpected",
                vec![format!("ALTER TABLE {t} DROP COLUMN {};", ident(&h.name))],
            ));
        }
    }

    diff_maps(
        &want.constraints,
        &have.constraints,
        |n, c| {
            drift(
                format!("constraint {}", quote_lit_name(n)),
                &format!("missing on table {}", quote_lit_name(name)),
                vec![add_constraint(name, n, c)],
            )
        },
        |n, _| {
            drift(
                format!("constraint {}", quote_lit_name(n)),
                &format!("unexpected on table {}", quote_lit_name(name)),
                vec![format!("ALTER TABLE {t} DROP CONSTRAINT {};", ident(n))],
            )
        },
        |n, w, h| {
            vec![Drift {
                object: format!("constraint {}", quote_lit_name(n)),
                problem: format!("differs on table {}", quote_lit_name(name)),
                expected: Some(w.definition.clone()),
                actual: Some(h.definition.clone()),
                fix: vec![
                    format!("ALTER TABLE {t} DROP CONSTRAINT {};", ident(n)),
                    add_constraint(name, n, w),
                ],
            }]
        },
        &mut out,
    );

    // Constraint-backed indexes come and go with their constraints (reported above).
    let plain = |m: &BTreeMap<String, Index>| -> BTreeMap<String, Index> {
        m.iter()
            .filter(|(_, i)| !i.constraint)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };
    diff_maps(
        &plain(&want.indexes),
        &plain(&have.indexes),
        |n, i| {
            drift(
                format!("index {}", quote_lit_name(n)),
                &format!("missing on table {}", quote_lit_name(name)),
                vec![format!("{};", i.definition)],
            )
        },
        |n, _| {
            drift(
                format!("index {}", quote_lit_name(n)),
                &format!("unexpected on table {}", quote_lit_name(name)),
                vec![format!("DROP INDEX {};", ident(n))],
            )
        },
        |n, w, h| {
            vec![Drift {
                object: format!("index {}", quote_lit_name(n)),
                problem: format!("differs on table {}", quote_lit_name(name)),
                expected: Some(w.definition.clone()),
                actual: Some(h.definition.clone()),
                fix: vec![
                    format!("DROP INDEX {};", ident(n)),
                    format!("{};", w.definition),
                ],
            }]
        },
        &mut out,
    );

    diff_maps(
        &want.triggers,
        &have.triggers,
        |n, tr| {
            drift(
                format!("trigger {}", quote_lit_name(n)),
                &format!("missing on table {}", quote_lit_name(name)),
                vec![format!("{};", tr.definition)],
            )
        },
        |n, _| {
            drift(
                format!("trigger {}", quote_lit_name(n)),
                &format!("unexpected on table {}", quote_lit_name(name)),
                vec![format!("DROP TRIGGER {} ON {t};", ident(n))],
            )
        },
        |n, w, h| {
            vec![Drift {
                object: format!("trigger {}", quote_lit_name(n)),
                problem: format!("differs on table {}", quote_lit_name(name)),
                expected: Some(w.definition.clone()),
                actual: Some(h.definition.clone()),
                fix: vec![
                    format!("DROP TRIGGER {} ON {t};", ident(n)),
                    format!("{};", w.definition),
                ],
            }]
        },
        &mut out,
    );
    out
}

/// `name type [GENERATED ...] [DEFAULT ...] [NOT NULL]`.
fn column_def(c: &Column) -> String {
    let mut s = format!("{} {}", ident(&c.name), c.type_name);
    if let Some(id) = &c.identity {
        let _ = write!(s, " GENERATED {} AS IDENTITY", id.to_uppercase());
    } else if c.generated {
        let _ = write!(
            s,
            " GENERATED ALWAYS AS ({}) STORED",
            c.default.as_deref().unwrap_or("NULL")
        );
    } else if let Some(d) = &c.default {
        let _ = write!(s, " DEFAULT {d}");
    }
    if !c.nullable {
        s.push_str(" NOT NULL");
    }
    s
}

fn add_constraint(table: &str, name: &str, c: &Constraint) -> String {
    format!(
        "ALTER TABLE {} ADD CONSTRAINT {} {};",
        ident(table),
        ident(name),
        c.definition
    )
}

/// `CREATE TABLE` with the columns and non-foreign-key constraints, then foreign keys, plain
/// indexes and triggers.
fn create_table(name: &str, t: &Table) -> Vec<String> {
    let mut parts: Vec<String> = t.columns.iter().map(column_def).collect();
    for (n, c) in &t.constraints {
        if c.kind != "foreign key" {
            parts.push(format!("CONSTRAINT {} {}", ident(n), c.definition));
        }
    }
    let mut out = vec![format!(
        "CREATE TABLE {} (\n    {}\n);",
        ident(name),
        parts.join(",\n    ")
    )];
    for (n, c) in &t.constraints {
        if c.kind == "foreign key" {
            out.push(add_constraint(name, n, c));
        }
    }
    for i in t.indexes.values().filter(|i| !i.constraint) {
        out.push(format!("{};", i.definition));
    }
    for tr in t.triggers.values() {
        out.push(format!("{};", tr.definition));
    }
    out
}

fn create_view(name: &str, v: &View) -> Vec<String> {
    let body = v.definition.trim_end_matches(';');
    vec![if v.materialized {
        format!("CREATE MATERIALIZED VIEW {} AS\n{body};", ident(name))
    } else {
        format!("CREATE OR REPLACE VIEW {} AS\n{body};", ident(name))
    }]
}

/// SQL keywords that cannot be used as bare identifiers.
const RESERVED: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "both",
    "case",
    "cast",
    "check",
    "collate",
    "column",
    "constraint",
    "create",
    "current_catalog",
    "current_date",
    "current_role",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "false",
    "fetch",
    "for",
    "foreign",
    "from",
    "grant",
    "group",
    "having",
    "in",
    "initially",
    "intersect",
    "into",
    "lateral",
    "leading",
    "limit",
    "localtime",
    "localtimestamp",
    "not",
    "null",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "placing",
    "primary",
    "references",
    "returning",
    "select",
    "session_user",
    "some",
    "symmetric",
    "table",
    "then",
    "to",
    "trailing",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "variadic",
    "when",
    "where",
    "window",
    "with",
];

/// Quotes an identifier when needed.
pub fn ident(s: &str) -> String {
    let simple = s
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$')
        && !RESERVED.contains(&s);
    if simple {
        s.to_owned()
    } else {
        format!("\"{}\"", s.replace('"', "\"\""))
    }
}

fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A name in a report line: `"repo"`.
fn quote_lit_name(s: &str) -> String {
    format!("\"{s}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str, nullable: bool, default: Option<&str>) -> Column {
        Column {
            name: name.to_owned(),
            type_name: ty.to_owned(),
            nullable,
            default: default.map(str::to_owned),
            identity: None,
            generated: false,
        }
    }

    fn base() -> SchemaDescription {
        let mut d = SchemaDescription::default();
        d.extensions.push("pg_trgm".to_owned());
        d.enums.insert(
            "state".to_owned(),
            vec!["queued".to_owned(), "done".to_owned()],
        );
        let mut repo = Table {
            columns: vec![
                Column {
                    identity: Some("by default".to_owned()),
                    ..col("id", "integer", false, None)
                },
                col("name", "text", false, None),
                col("stars", "integer", false, Some("0")),
            ],
            ..Table::default()
        };
        repo.constraints.insert(
            "repo_pkey".to_owned(),
            Constraint {
                kind: "primary key".to_owned(),
                definition: "PRIMARY KEY (id)".to_owned(),
            },
        );
        repo.indexes.insert(
            "repo_pkey".to_owned(),
            Index {
                definition: "CREATE UNIQUE INDEX repo_pkey ON public.repo USING btree (id)"
                    .to_owned(),
                constraint: true,
            },
        );
        repo.indexes.insert(
            "repo_name_idx".to_owned(),
            Index {
                definition: "CREATE INDEX repo_name_idx ON public.repo USING btree (name)"
                    .to_owned(),
                constraint: false,
            },
        );
        d.tables.insert("repo".to_owned(), repo);
        d
    }

    #[test]
    fn identical_descriptions_have_no_drift() {
        assert!(diff(&base(), &base()).is_empty());
        let json = base().to_json();
        assert_eq!(SchemaDescription::from_json(&json).unwrap(), base());
    }

    #[test]
    fn column_drift_and_fixes() {
        let want = base();
        let mut have = base();
        let repo = have.tables.get_mut("repo").unwrap();
        repo.columns.retain(|c| c.name != "name");
        repo.columns[1].default = None; // stars
        repo.columns[1].nullable = true;
        repo.columns
            .push(col("order", "character varying(10)", true, None));
        let drifts = diff(&want, &have);
        let fixes: Vec<&str> = drifts
            .iter()
            .flat_map(|d| d.fix.iter().map(String::as_str))
            .collect();
        assert_eq!(
            fixes,
            [
                "ALTER TABLE repo ADD COLUMN name text NOT NULL;",
                "ALTER TABLE repo ALTER COLUMN stars SET NOT NULL;",
                "ALTER TABLE repo ALTER COLUMN stars SET DEFAULT 0;",
                "ALTER TABLE repo DROP COLUMN \"order\";",
            ]
        );
        assert_eq!(drifts[0].object, "column \"repo.name\"");
        assert_eq!(drifts[0].problem, "missing");
    }

    #[test]
    fn type_change_uses_a_cast() {
        let want = base();
        let mut have = base();
        have.tables.get_mut("repo").unwrap().columns[2].type_name = "bigint".to_owned();
        let drifts = diff(&want, &have);
        assert_eq!(drifts.len(), 1);
        assert_eq!(
            drifts[0].fix,
            ["ALTER TABLE repo ALTER COLUMN stars TYPE integer USING stars::integer;"]
        );
        assert_eq!(drifts[0].expected.as_deref(), Some("integer"));
        assert_eq!(drifts[0].actual.as_deref(), Some("bigint"));
    }

    #[test]
    fn missing_table_is_recreated_with_its_parts() {
        let want = base();
        let mut have = base();
        have.tables.clear();
        let drifts = diff(&want, &have);
        assert_eq!(drifts.len(), 1);
        assert_eq!(
            drifts[0].fix,
            [
                "CREATE TABLE repo (\n    id integer GENERATED BY DEFAULT AS IDENTITY NOT NULL,\n    name text NOT NULL,\n    stars integer DEFAULT 0 NOT NULL,\n    CONSTRAINT repo_pkey PRIMARY KEY (id)\n);",
                "CREATE INDEX repo_name_idx ON public.repo USING btree (name);",
            ]
        );
    }

    #[test]
    fn indexes_constraints_and_extra_tables() {
        let want = base();
        let mut have = base();
        let repo = have.tables.get_mut("repo").unwrap();
        repo.indexes.get_mut("repo_name_idx").unwrap().definition =
            "CREATE INDEX repo_name_idx ON public.repo USING btree (lower(name))".to_owned();
        repo.constraints.remove("repo_pkey");
        repo.indexes.remove("repo_pkey");
        have.tables.insert("scratch".to_owned(), Table::default());
        have.extensions.push("pg_stat_statements".to_owned());
        let drifts = diff(&want, &have);
        let summary: Vec<(String, String)> = drifts
            .iter()
            .map(|d| (d.object.clone(), d.problem.clone()))
            .collect();
        assert_eq!(
            summary,
            [
                (
                    "constraint \"repo_pkey\"".to_owned(),
                    "missing on table \"repo\"".to_owned()
                ),
                (
                    "index \"repo_name_idx\"".to_owned(),
                    "differs on table \"repo\"".to_owned()
                ),
                ("table \"scratch\"".to_owned(), "unexpected".to_owned()),
            ]
        );
        assert_eq!(
            drifts[0].fix,
            ["ALTER TABLE repo ADD CONSTRAINT repo_pkey PRIMARY KEY (id);"]
        );
        assert_eq!(
            drifts[1].fix,
            [
                "DROP INDEX repo_name_idx;",
                "CREATE INDEX repo_name_idx ON public.repo USING btree (name);"
            ]
        );
        assert_eq!(drifts[2].fix, ["DROP TABLE scratch;"]);
    }

    #[test]
    fn enums_and_extensions() {
        let want = base();
        let mut have = base();
        have.extensions.clear();
        have.enums
            .insert("state".to_owned(), vec!["done".to_owned()]);
        let drifts = diff(&want, &have);
        assert_eq!(drifts[0].fix, ["CREATE EXTENSION IF NOT EXISTS pg_trgm;"]);
        assert_eq!(
            drifts[1].fix,
            ["ALTER TYPE state ADD VALUE 'queued' BEFORE 'done';"]
        );
        have.enums.insert(
            "state".to_owned(),
            vec!["done".to_owned(), "queued".to_owned()],
        );
        let drifts = diff(&want, &have);
        assert!(drifts[1].fix[0].starts_with("-- enum labels cannot be removed or reordered"));
    }

    #[test]
    fn identifiers_are_quoted_when_needed() {
        assert_eq!(ident("repo"), "repo");
        assert_eq!(ident("user"), "\"user\"");
        assert_eq!(ident("Mixed"), "\"Mixed\"");
        assert_eq!(ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(ident("1x"), "\"1x\"");
    }

    #[test]
    fn report_shows_expected_actual_and_fix() {
        let mut have = base();
        have.tables.get_mut("repo").unwrap().columns[2].type_name = "bigint".to_owned();
        let text = report(&diff(&base(), &have));
        assert_eq!(
            text,
            "column \"repo.stars\": type differs\n    expected: integer\n    actual:   bigint\n    fix:      ALTER TABLE repo ALTER COLUMN stars TYPE integer USING stars::integer;\n"
        );
    }
}
