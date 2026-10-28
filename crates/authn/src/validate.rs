//! Input validation for sign-up: usernames, email addresses and passwords.

use crate::AuthError;
use sourcegull_config::PasswordPolicy;

/// Maximum username length.
pub const MAX_USERNAME_LEN: usize = 255;
/// Maximum password length (characters), bounding hashing cost.
pub const MAX_PASSWORD_LEN: usize = 256;
const MAX_EMAIL_LEN: usize = 320;

/// Validates a username: ASCII letters, digits, `-`, `.` and `_`; at most 255
/// characters; not starting or ending with `-` or `.`; no consecutive `-`/`.`.
pub fn validate_username(username: &str) -> Result<(), AuthError> {
    let invalid = |why: &str| {
        Err(AuthError::InvalidUsername(format!(
            "Invalid username: {why}."
        )))
    };
    if username.is_empty() {
        return invalid("it must not be empty");
    }
    if username.len() > MAX_USERNAME_LEN {
        return invalid("it must be at most 255 characters");
    }
    let is_sep = |b: u8| b == b'-' || b == b'.';
    let bytes = username.as_bytes();
    if !bytes
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || is_sep(b) || b == b'_')
    {
        return invalid("only letters, digits, '-', '.' and '_' are allowed");
    }
    if is_sep(bytes[0]) || is_sep(bytes[bytes.len() - 1]) {
        return invalid("it must not start or end with '-' or '.'");
    }
    if bytes.windows(2).any(|w| is_sep(w[0]) && is_sep(w[1])) {
        return invalid("'-' and '.' must not appear consecutively");
    }
    Ok(())
}

/// Light syntactic check of an email address: `local@domain`, no whitespace,
/// a dot in the domain, at most 320 characters.
pub fn validate_email(email: &str) -> Result<(), AuthError> {
    let ok = email.len() <= MAX_EMAIL_LEN
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && !domain.is_empty()
                && !domain.contains('@')
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        });
    if ok {
        Ok(())
    } else {
        Err(AuthError::InvalidEmail)
    }
}

/// Checks a password against the policy. The minimum length (and the
/// 256-character maximum) always apply; character-class rules only when the
/// policy is enabled.
pub fn validate_password(password: &str, policy: &PasswordPolicy) -> Result<(), AuthError> {
    let weak = |msg: String| Err(AuthError::WeakPassword(msg));
    let len = password.chars().count();
    let min = policy.minimum_length as usize;
    if len < min {
        return weak(format!("Password must be at least {min} characters long."));
    }
    if len > MAX_PASSWORD_LEN {
        return weak(format!(
            "Password must be at most {MAX_PASSWORD_LEN} characters long."
        ));
    }
    if !policy.enabled {
        return Ok(());
    }
    let specials = password
        .chars()
        .filter(|c| !c.is_alphanumeric() && !c.is_whitespace())
        .count();
    let needed = policy.number_of_special_characters as usize;
    if specials < needed {
        return weak(format!(
            "Password must contain at least {needed} special character(s)."
        ));
    }
    if policy.require_at_least_one_number && !password.chars().any(|c| c.is_ascii_digit()) {
        return weak("Password must contain at least one number.".into());
    }
    if policy.require_upperand_lower_case
        && !(password.chars().any(char::is_uppercase) && password.chars().any(char::is_lowercase))
    {
        return weak("Password must contain both upper- and lower-case letters.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames() {
        for ok in ["alice", "a", "Bob_1", "a.b-c", "a_", "_a", "x1.y2"] {
            assert!(validate_username(ok).is_ok(), "{ok}");
        }
        let long = "a".repeat(256);
        for bad in [
            "",
            "-a",
            "a-",
            ".a",
            "a.",
            "a..b",
            "a.-b",
            "a b",
            "é",
            "a@b",
            long.as_str(),
        ] {
            let e = validate_username(bad).unwrap_err();
            assert_eq!(e.code(), "INVALID_USERNAME", "{bad}");
        }
        assert!(validate_username(&"a".repeat(255)).is_ok());
    }

    #[test]
    fn emails() {
        assert!(validate_email("a@b.co").is_ok());
        for bad in [
            "", "a", "a@", "@b.c", "a@b", "a b@c.d", "a@b@c.d", "a@.b", "a@b.",
        ] {
            assert!(validate_email(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn passwords() {
        let mut p = PasswordPolicy::default();
        assert!(validate_password("short", &p).is_err());
        assert!(validate_password("longenoughpw", &p).is_ok());
        assert!(validate_password(&"x".repeat(257), &p).is_err());
        p.enabled = true;
        p.number_of_special_characters = 1;
        p.require_at_least_one_number = true;
        p.require_upperand_lower_case = true;
        assert!(validate_password("longenoughpw", &p).is_err());
        assert!(validate_password("longenoughpw!", &p).is_err());
        assert!(validate_password("longenoughpw!1", &p).is_err());
        assert!(validate_password("Longenoughpw!1", &p).is_ok());
    }
}
