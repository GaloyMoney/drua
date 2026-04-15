use thiserror::Error;

#[derive(Error, Debug)]
pub enum McpJwtError {
    #[error("McpJwtError - Jwt: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("McpJwtError - PrivateKeyRead: {0}")]
    PrivateKeyRead(std::io::Error),
    #[error("McpJwtError - PrivateKeyParse: {0}")]
    PrivateKeyParse(String),
}
