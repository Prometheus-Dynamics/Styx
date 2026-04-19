#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("watcher io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend watch error: {0}")]
    Backend(String),
}
