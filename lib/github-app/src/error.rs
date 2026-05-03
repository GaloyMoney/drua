use thiserror::Error;

#[derive(Error, Debug)]
pub enum GitHubAppError {
    #[error("GitHubAppError - Jwt: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("GitHubAppError - Reqwest: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("GitHubAppError - ApiError: {status} {message}")]
    ApiError { status: u16, message: String },
    #[error("GitHubAppError - PrivateKeyRead: {0}")]
    PrivateKeyRead(std::io::Error),
}
