//! Keeps a process's shared keyring current after key rotations elsewhere.
//!
//! A rotation (a process starting with a new current KEK, or `sourcegull keys rotate`) writes new
//! data keys and sends `NOTIFY encryption_keys`. Every process then re-reads its key file (a
//! mounted Secret is updated in place) and reloads the data keys, so it encrypts with the new
//! keys and can read values the re-encryption migration has already moved. Keys from
//! `SOURCEGULL_KEYS` are fixed for the life of the process: a new key there needs a restart.

use std::sync::Arc;

use sourcegull_crypto::{KeyProvider, Keyring};
use sourcegull_db::Db;
use sourcegull_db::notify::channels;
use tokio_util::sync::CancellationToken;

use crate::startup::KeySource;

/// Reloads `keyring` on every `encryption_keys` notification until `cancel` fires.
pub async fn run_reloader(
    db: Db,
    keyring: Arc<Keyring>,
    source: KeySource,
    cancel: CancellationToken,
) {
    let mut sub = tokio::select! {
        () = cancel.cancelled() => return,
        s = sourcegull_pubsub::subscribe(db.pool(), &[channels::ENCRYPTION_KEYS]) => s,
    };
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            // A (re)connect may have missed notifications: reload then too.
            _ = sub.recv() => {}
        }
        reload(&db, &keyring, &source).await;
    }
}

/// Re-reads the key file (keeping the current KEKs if it cannot be read) and reloads the data
/// keys from `encryption_keys`. Inline keys keep the current KEKs.
pub async fn reload(db: &Db, keyring: &Keyring, source: &KeySource) {
    let provider = match source {
        KeySource::Inline(_) => None,
        KeySource::File { path, .. } => match source.provider() {
            Ok(p) => Some(Arc::new(p) as Arc<dyn KeyProvider>),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = format!("{e:#}"), "re-reading the key file failed");
                None
            }
        },
    };
    match db.encryption_keys().reload(keyring, provider).await {
        Ok(()) => tracing::debug!(
            current_key = keyring.current_kek_id(),
            "reloaded the data encryption keys"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            "reloading the data encryption keys failed; restart with the current key file"
        ),
    }
}

/// `sourcegull keys status|rotate`.
pub async fn run_command(
    db: &Db,
    source: &KeySource,
    action: crate::cli::KeysAction,
) -> anyhow::Result<String> {
    use std::fmt::Write as _;

    use anyhow::Context as _;

    let provider: Arc<dyn KeyProvider> = Arc::new(source.provider()?);
    let mut out = String::new();
    let ring = match action {
        // Read-only: no keys are created or rotated.
        crate::cli::KeysAction::Status => {
            Keyring::from_wrapped(provider, db.encryption_keys().list().await?)
                .context("unwrapping the data keys with the key-encryption keys")?
        }
        crate::cli::KeysAction::Rotate => {
            let (ring, changed) = db.encryption_keys().rotate(provider).await?;
            let _ = writeln!(
                out,
                "rotated: {changed} data key row(s) written; the workers re-encrypt secrets in \
                 the background (Site admin > Migrations shows the progress)\n"
            );
            ring
        }
    };
    let _ = writeln!(
        out,
        "current key-encryption key: {}\n",
        ring.current_kek_id()
    );
    let _ = writeln!(out, "{:<26} {:<8} {:<8} KEK", "CLASS", "VERSION", "STATE");
    for k in ring.wrapped_keys() {
        let _ = writeln!(
            out,
            "{:<26} {:<8} {:<8} {}",
            k.class.as_str(),
            k.version,
            k.state.as_str(),
            k.kek_id().unwrap_or("?")
        );
    }
    let counts = sourcegull_oobmigration::reencrypt::ReencryptSecrets::new(db.clone(), None)
        .counts()
        .await?;
    let _ = writeln!(
        out,
        "\n{:<44} {:>8} {:>16}",
        "COLUMN", "VALUES", "ON RETIRED KEYS"
    );
    for (c, n) in counts {
        let _ = writeln!(
            out,
            "{:<44} {:>8} {:>16}",
            format!("{}.{}", c.table, c.column),
            n.total,
            n.stale
        );
    }
    Ok(out.trim_end().to_owned())
}
