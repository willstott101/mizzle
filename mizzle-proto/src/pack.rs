use anyhow::anyhow;

/// Partial clone filter specification.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    /// `blob:none` — omit all blobs.
    BlobNone,
    /// `tree:0` — omit all trees (and blobs); only commits are included.
    TreeNone,
}

impl Filter {
    /// Parse a filter spec string as sent by the git client.
    pub fn parse(spec: &str) -> anyhow::Result<Self> {
        match spec {
            "blob:none" => Ok(Filter::BlobNone),
            "tree:0" => Ok(Filter::TreeNone),
            _ => Err(anyhow!("unsupported filter: {spec}")),
        }
    }
}
