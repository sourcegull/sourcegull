//! Single-dash long flags. `gull` takes `-name`, `-name=value` and `--name` alike, so scripts
//! and CI templates can use the single dash. [`normalize`] rewrites `-name…` to `--name…` so
//! clap accepts both spellings.

/// Rewrites single-dash long flags (`-file=x`, `-no-progress`) to double-dash ones. Single
/// letters (`-h`), negative numbers (`-1`) and everything after `--` are left alone.
pub fn normalize<I, S>(args: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut out = Vec::new();
    let mut rest = false;
    for (i, arg) in args.into_iter().enumerate() {
        let arg: String = arg.into();
        if i == 0 || rest {
            out.push(arg);
            continue;
        }
        if arg == "--" {
            rest = true;
            out.push(arg);
            continue;
        }
        let long_single_dash = arg.strip_prefix('-').is_some_and(|name| {
            let name = name.split('=').next().unwrap_or_default();
            !name.starts_with('-')
                && name.chars().count() > 1
                && !name.starts_with(|c: char| c.is_ascii_digit())
        });
        out.push(if long_single_dash {
            format!("-{arg}")
        } else {
            arg
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::normalize;

    #[test]
    fn single_dash_long_flags_become_double_dash() {
        let got = normalize([
            "gull",
            "code-intel",
            "upload",
            "-file=index.scip",
            "-repo",
            "gitlab.com/a/b",
            "--root=lib",
            "-h",
            "-max-concurrency",
            "-1",
            "-json=false",
            "--",
            "-not-a-flag",
        ]);
        assert_eq!(
            got,
            [
                "gull",
                "code-intel",
                "upload",
                "--file=index.scip",
                "--repo",
                "gitlab.com/a/b",
                "--root=lib",
                "-h",
                "--max-concurrency",
                "-1",
                "--json=false",
                "--",
                "-not-a-flag",
            ]
        );
    }
}
