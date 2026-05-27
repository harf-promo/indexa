//! Core indexing engine: file walker, filesystem watcher, SQLite store, and graph model.

pub mod store;
pub mod walker;

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder() {
        assert_eq!(2 + 2, 4);
    }
}
