//! Parsing of `Authorization` header values carrying access tokens.

use crate::AuthError;

/// A credential parsed from an `Authorization` header.
#[derive(Clone, PartialEq, Eq)]
pub enum TokenCredential {
    /// `token <t>`.
    Token(String),
    /// `token-sudo user="<username>",token="<t>"`.
    Sudo {
        /// Username to act as.
        user: String,
        /// A token with the `site-admin:sudo` scope.
        token: String,
    },
}

impl std::fmt::Debug for TokenCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenCredential::Token(_) => f.write_str("Token(<redacted>)"),
            TokenCredential::Sudo { user, .. } => f
                .debug_struct("Sudo")
                .field("user", user)
                .finish_non_exhaustive(),
        }
    }
}

/// Parses an `Authorization` header value. Returns `Ok(None)` for other
/// schemes (e.g. `Basic`), and [`AuthError::TokenInvalid`] for a malformed
/// `token`/`token-sudo` value. Scheme names are case-insensitive.
pub fn parse_authorization(value: &str) -> Result<Option<TokenCredential>, AuthError> {
    let value = value.trim();
    let (scheme, rest) = value
        .split_once(char::is_whitespace)
        .map_or((value, ""), |(s, r)| (s, r.trim()));
    if scheme.eq_ignore_ascii_case("token") {
        if rest.is_empty() || rest.contains(char::is_whitespace) {
            return Err(AuthError::TokenInvalid);
        }
        return Ok(Some(TokenCredential::Token(rest.to_owned())));
    }
    if !scheme.eq_ignore_ascii_case("token-sudo") {
        return Ok(None);
    }
    let (mut user, mut token) = (None, None);
    for part in rest.split(',') {
        let (key, val) = part.trim().split_once('=').ok_or(AuthError::TokenInvalid)?;
        let val = val.trim();
        let val = match val.strip_prefix('"') {
            Some(v) => v.strip_suffix('"').ok_or(AuthError::TokenInvalid)?,
            None => val,
        };
        if val.is_empty() || val.contains('"') {
            return Err(AuthError::TokenInvalid);
        }
        let slot = match key.trim() {
            "user" => &mut user,
            "token" => &mut token,
            _ => return Err(AuthError::TokenInvalid),
        };
        if slot.replace(val.to_owned()).is_some() {
            return Err(AuthError::TokenInvalid);
        }
    }
    match (user, token) {
        (Some(user), Some(token)) => Ok(Some(TokenCredential::Sudo { user, token })),
        _ => Err(AuthError::TokenInvalid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headers() {
        assert_eq!(
            parse_authorization("token skp_abc").unwrap(),
            Some(TokenCredential::Token("skp_abc".into()))
        );
        assert_eq!(
            parse_authorization("Token   skp_abc ").unwrap(),
            Some(TokenCredential::Token("skp_abc".into()))
        );
        assert_eq!(
            parse_authorization(r#"token-sudo user="alice",token="skp_x""#).unwrap(),
            Some(TokenCredential::Sudo {
                user: "alice".into(),
                token: "skp_x".into()
            })
        );
        assert_eq!(
            parse_authorization(r#"token-sudo token="skp_x", user=bob"#).unwrap(),
            Some(TokenCredential::Sudo {
                user: "bob".into(),
                token: "skp_x".into()
            })
        );
        assert_eq!(parse_authorization("Basic Zm9vOmJhcg==").unwrap(), None);
        assert_eq!(parse_authorization("Bearer x").unwrap(), None);
        for bad in [
            "token",
            "token a b",
            "token-sudo",
            r#"token-sudo user="alice""#,
            r#"token-sudo user="alice",token="x",user="bob""#,
            r#"token-sudo user="alice,token="x""#,
            r#"token-sudo user="",token="x""#,
            r#"token-sudo foo="a",token="x""#,
        ] {
            assert!(parse_authorization(bad).is_err(), "{bad}");
        }
    }
}
