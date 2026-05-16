use crate::model::{ProviderId, UsageSnapshot};
use async_trait::async_trait;

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn enabled(&self) -> bool {
        true
    }
    /// Window labels this provider treats as *derived sub-views* of a
    /// primary window (e.g. Anthropic's per-model weekly split). The
    /// runtime stamps `WindowUsage::subview` on these in the cached
    /// snapshot before `merge_stale_from`, so a cache written by an
    /// older build (no `subview` field → deserialised as `false`)
    /// still gets skipped instead of grafted back as a false-stale
    /// ghost. Default: none.
    fn subview_labels(&self) -> &'static [&'static str] {
        &[]
    }
    async fn poll(&self) -> anyhow::Result<UsageSnapshot>;
}
