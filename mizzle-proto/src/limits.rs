/// Safety limits for protocol parsing.  These cap how many items a client
/// can send in a single request, preventing pathological input from causing
/// unbounded memory allocation.
///
/// Every field has a generous default that no legitimate client should hit.
/// Override individual fields to tighten them for your deployment.
#[derive(Debug, Clone, Copy)]
pub struct ProtocolLimits {
    /// Maximum ref-update commands in a single receive-pack request.
    pub max_ref_updates: usize,
    /// Maximum `want` lines in a fetch request.
    pub max_wants: usize,
    /// Maximum `have` lines in a fetch request.
    pub max_haves: usize,
    /// Maximum `want-ref` lines in a fetch request.
    pub max_want_refs: usize,
    /// Maximum `ref-prefix` lines in a list-refs request.
    pub max_ref_prefixes: usize,
}

impl Default for ProtocolLimits {
    fn default() -> Self {
        Self {
            max_ref_updates: 10_000,
            max_wants: 100_000,
            max_haves: 100_000,
            max_want_refs: 10_000,
            max_ref_prefixes: 1_000,
        }
    }
}

/// Checks that `count` does not exceed `max`, returning a descriptive error.
pub(crate) fn check_limit(count: usize, max: usize, name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        count <= max,
        "too many {name} ({count} exceeds limit of {max})",
    );
    Ok(())
}
