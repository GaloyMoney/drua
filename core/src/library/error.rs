#[derive(thiserror::Error, Debug)]
pub enum LibraryError {
    #[error("LibraryError - IO: {0}")]
    Io(String),
}
