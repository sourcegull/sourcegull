//! The syntactic tier: locals from `sourcegull-codeintel-syntactic`, cached by blob
//! OID, language and resolver version.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use sourcegull_codeintel_syntactic::Locals;

type Key = (String, u32, &'static str);

static CACHE: LazyLock<moka::future::Cache<Key, Arc<Locals>>> = LazyLock::new(|| {
    moka::future::Cache::builder()
        .weigher(|_, v: &Arc<Locals>| {
            u32::try_from(v.occurrences.len() * 20 + 64).unwrap_or(u32::MAX)
        })
        .max_capacity(64 * 1024 * 1024)
        .time_to_idle(Duration::from_secs(3600))
        .build()
});

/// The locals of blob `oid` with `text` in language `lang`; `None` for languages without a
/// scope query and for files the resolver skips.
pub async fn locals(oid: &str, lang: Option<&'static str>, text: Arc<str>) -> Option<Arc<Locals>> {
    let lang = lang.filter(|l| sourcegull_codeintel_syntactic::supported(l))?;
    let key: Key = (
        oid.to_owned(),
        sourcegull_codeintel_syntactic::VERSION,
        lang,
    );
    if let Some(hit) = CACHE.get(&key).await {
        return Some(hit);
    }
    let locals = tokio::task::spawn_blocking(move || {
        sourcegull_codeintel_syntactic::analyze(lang, text.as_bytes())
    })
    .await
    .ok()??;
    let locals = Arc::new(locals);
    CACHE.insert(key, locals.clone()).await;
    Some(locals)
}
