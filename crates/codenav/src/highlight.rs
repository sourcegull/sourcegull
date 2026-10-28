//! Process-wide highlighting cache shared by the file view and code navigation: (blob oid,
//! highlighter version, language) → result.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use sourcegull_highlight::{Highlighted, PlainReason};
use sourcegull_rpc::repo::v1 as pb;

type HighlightKey = (String, u32, Option<&'static str>);

static HIGHLIGHT_CACHE: LazyLock<moka::future::Cache<HighlightKey, Arc<Highlighted>>> =
    LazyLock::new(|| {
        moka::future::Cache::builder()
            .weigher(|_, v: &Arc<Highlighted>| {
                u32::try_from(v.occurrences.len() * 16 + 128).unwrap_or(u32::MAX)
            })
            .max_capacity(256 * 1024 * 1024)
            .time_to_idle(Duration::from_secs(3600))
            .build()
    });

/// The language id of `path` with content `content`.
pub fn language(path: &str, content: &[u8]) -> Option<&'static str> {
    let head = &content[..content.len().min(16 * 1024)];
    sourcegull_languages::detect(path, Some(head)).map(|l| l.id)
}

/// Highlights `file` at `path`, cached by blob OID, language and highlighter version.
/// `disable_timeout` raises the time limit to 30 s (and ignores a cached timeout).
pub async fn highlight_cached(
    path: &str,
    file: Arc<pb::ReadFileResponse>,
    disable_timeout: bool,
) -> Result<Arc<Highlighted>, tokio::task::JoinError> {
    let lang = language(path, &file.content);
    let key: HighlightKey = (file.oid.clone(), sourcegull_highlight::VERSION, lang);
    if let Some(hit) = HIGHLIGHT_CACHE.get(&key).await
        && !(disable_timeout && hit.plain_reason == Some(PlainReason::Timeout))
    {
        return Ok(hit);
    }
    let path_owned = path.to_owned();
    let result = tokio::task::spawn_blocking(move || {
        let mut req = sourcegull_highlight::Request::new(&path_owned, &file.content);
        if file.truncated {
            // Never highlight a prefix: report the file as too large instead.
            req.limits.max_bytes = 0;
        }
        if disable_timeout {
            req.limits.timeout = Duration::from_secs(30);
        }
        sourcegull_highlight::highlight(&req)
    })
    .await?;
    let result = Arc::new(result);
    if result.plain_reason != Some(PlainReason::Timeout) {
        HIGHLIGHT_CACHE.insert(key, result.clone()).await;
    }
    Ok(result)
}
