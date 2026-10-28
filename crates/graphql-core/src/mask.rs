//! Masked forms of write-only secrets as reads return them: enough to recognize a stored value
//! without revealing it. An update that sends the masked form back keeps the stored value.

/// How many trailing characters a masked secret shows, and how long the secret part must be
/// for them to be shown.
const MASK_TAIL: usize = 4;
const MASK_MIN_LEN: usize = 12;

/// The last characters of `secret` after `…`, or just `…` when it is short.
#[must_use]
pub fn mask_tail(secret: &str) -> String {
    let n = secret.chars().count();
    if n < MASK_MIN_LEN {
        return "…".to_owned();
    }
    let tail: String = secret.chars().skip(n - MASK_TAIL).collect();
    format!("…{tail}")
}

/// A URL as reads return it: the scheme, host and port, then the masked rest
/// (`https://hooks.slack.com/…a9f3`). Credentials in the URL are dropped.
#[must_use]
pub fn mask_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) if u.host_str().is_some() => {
            let origin = u.origin().ascii_serialization();
            let rest = &u[url::Position::BeforePath..];
            format!("{origin}/{}", mask_tail(rest.trim_start_matches('/')))
        }
        _ => mask_tail(url),
    }
}

/// `input`, or `stored` when `input` is empty or `stored`'s masked form (`mask`).
#[must_use]
pub fn keep_masked(input: &str, stored: &str, mask: impl Fn(&str) -> String) -> String {
    let input = input.trim();
    if input.is_empty() || input == mask(stored) {
        stored.to_owned()
    } else {
        input.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_urls() {
        let slack = "https://hooks.slack.com/services/T000/B000/XXXXa9f3";
        assert_eq!(mask_url(slack), "https://hooks.slack.com/…a9f3");
        assert_eq!(
            mask_url("http://user:pw@127.0.0.1:8080/hook?k=1"),
            "http://127.0.0.1:8080/…"
        );
        assert_eq!(mask_url("not a url at all"), "… all");
        assert_eq!(keep_masked("", slack, mask_url), slack);
        assert_eq!(keep_masked(&mask_url(slack), slack, mask_url), slack);
        assert_eq!(keep_masked(" https://x/y ", slack, mask_url), "https://x/y");
    }
}
