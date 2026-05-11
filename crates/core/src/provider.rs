use crate::model::{ProviderId, UsageSnapshot};
use async_trait::async_trait;

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn enabled(&self) -> bool {
        true
    }
    async fn poll(&self) -> anyhow::Result<UsageSnapshot>;
}
