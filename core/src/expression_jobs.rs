//! Durable, account-local download jobs. A checkpoint contains at most one pack;
//! completed files are reused after interruption without marking a partial pack installed.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DownloadJob {
    pub kind: String,
    pub package: String,
    pub product: Value,
    pub status: String,
    #[serde(default)]
    pub pack: Value,
    #[serde(default)]
    pub next: usize,
    #[serde(default)]
    pub error: Option<String>,
}

impl DownloadJob {
    pub fn pending(&self) -> bool {
        matches!(self.status.as_str(), "queued" | "downloading")
    }
}
