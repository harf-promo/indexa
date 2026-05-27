//! LLM description adapter — generates human-readable summaries of files.

/// Generates a natural-language description for a file given its content sample.
#[async_trait::async_trait]
pub trait Describer: Send + Sync {
    async fn describe(&self, path: &str, content_sample: &[u8]) -> anyhow::Result<String>;
}

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder() {
        assert_eq!(2 + 2, 4);
    }
}
