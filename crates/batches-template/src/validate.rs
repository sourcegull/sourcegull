//! Dry-run validation of a whole bulk change spec's templates: the spec text is rendered with empty step
//! and changeset contexts and `missingkey=error`, after removing actions that use `outputs` or
//! `index` (their validity depends on execution). Unknown names are reported as
//! `unknown templating variable: '<name>'`.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::context::{ChangesetTemplateContext, StepContext};
use crate::error::{ErrorKind, TemplateError};
use crate::value::Value;
use crate::{Options, Template};

static OUTPUTS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\$\{\{\s*[^}]*\s*outputs\.[^}]*\}\}").expect("valid regex"));
static INDEX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\$\{\{\s*index\s*[^}]*\}\}").expect("valid regex"));

const PREFIX: &str = "validating bulk change spec template";

/// Blanks out matches, keeping newlines so that error positions stay those of `spec`.
fn blank(spec: &str, re: &Regex) -> String {
    re.replace_all(spec, |c: &regex::Captures<'_>| {
        c[0].chars()
            .map(|ch| if ch == '\n' { '\n' } else { ' ' })
            .collect::<String>()
    })
    .into_owned()
}

/// Validates the templates in a bulk change spec's text. Errors carry a message
/// (`validating bulk change spec template: unknown templating variable: 'resipotory'`) and the
/// position in the spec.
pub fn validate_batch_spec_template(spec: &str) -> Result<(), TemplateError> {
    let stripped = blank(&blank(spec, &OUTPUTS_RE), &INDEX_RE);

    // The step names, overridden by the changeset template's, with a step context as the dot.
    let mut names: BTreeMap<String, Value> = BTreeMap::new();
    for v in [
        StepContext::default().names(),
        ChangesetTemplateContext::default().names(),
    ] {
        if let Value::Map(m) = v {
            names.extend(m);
        }
    }
    let names = Value::Map(names);
    let dot = StepContext::default().dot();

    let wrap = |e: TemplateError| -> TemplateError {
        let message = match &e.kind {
            ErrorKind::UndefinedFunction(name) => {
                format!("{PREFIX}: unknown templating variable: '{name}'")
            }
            ErrorKind::MissingKey { path, .. } => {
                format!("{PREFIX}: unknown templating variable: '{path}'")
            }
            _ => format!("{PREFIX}: {}", e.message),
        };
        TemplateError { message, ..e }
    };
    let t = Template::parse(&stripped).map_err(wrap)?;
    t.execute(&names, &dot, &Options::default())
        .map(|_| ())
        .map_err(wrap)
}
