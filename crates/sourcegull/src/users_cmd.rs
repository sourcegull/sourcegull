//! `sourcegull users`: account recovery from the command line, for when no site admin can sign
//! in and email is not configured.

use anyhow::Context as _;
use sourcegull_crypto::session::{generate_session_id, hash_session_id};
use sourcegull_db::Db;

use crate::cli::{UsersAction, UsersArgs};
use crate::site_config::SiteConfigLoader;

/// Runs `action` and returns the text to print.
pub async fn run(db: &Db, args: &UsersArgs) -> anyhow::Result<String> {
    match &args.action {
        UsersAction::ResetPassword { username } => {
            let external_url = args.external_url.clone().filter(|u| !u.is_empty());
            let file = crate::cli::non_empty(&args.site_config_file);
            let loader = SiteConfigLoader::load(db, file.as_deref(), external_url)
                .await
                .context("loading the site config")?;
            let config = loader.handle().load();
            let user = db
                .users()
                .get_by_username(username)
                .await?
                .with_context(|| format!("no active user named {username:?}"))?;
            let code = generate_session_id();
            let expiry = config.value.auth_password_reset_link_expiry;
            db.password_resets()
                .create(user.id, &hash_session_id(&code), expiry)
                .await?;
            db.users().reset_failed_logins(user.id).await?;
            let origin = config
                .value
                .external_url
                .as_deref()
                .unwrap_or("")
                .trim_end_matches('/');
            let id = sourcegull_services::relay::marshal_id("User", user.id.get());
            let mut out = format!("{origin}/reset-password?userID={id}&code={code}\n");
            if origin.is_empty() {
                out.push_str(
                    "externalURL is not set: open the path above on the server's address\n",
                );
            }
            out.push_str(&format!(
                "single-use link for {}{}, valid for {}; sign-in lockouts cleared",
                user.username,
                if user.site_admin { " (site admin)" } else { "" },
                humantime(expiry),
            ));
            Ok(out)
        }
    }
}

/// `3600` → `1h`, `5400` → `90m`, `45` → `45s`.
fn humantime(secs: u32) -> String {
    match secs {
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}
