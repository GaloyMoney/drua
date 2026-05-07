use oauth2::{
    basic::BasicClient, AuthUrl, ClientId, ClientSecret, EndpointNotSet, EndpointSet, RedirectUrl,
    RevocationErrorResponseType, StandardErrorResponse, StandardRevocableToken,
    StandardTokenIntrospectionResponse, StandardTokenResponse, TokenUrl,
};
use serde::{Deserialize, Serialize};

const GITHUB_AUTH_URL: &str = "https://github.com/login/oauth/authorize";
const GITHUB_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

pub type OAuthClient = oauth2::Client<
    StandardErrorResponse<oauth2::basic::BasicErrorResponseType>,
    StandardTokenResponse<oauth2::EmptyExtraTokenFields, oauth2::basic::BasicTokenType>,
    StandardTokenIntrospectionResponse<
        oauth2::EmptyExtraTokenFields,
        oauth2::basic::BasicTokenType,
    >,
    StandardRevocableToken,
    StandardErrorResponse<RevocationErrorResponseType>,
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointSet,
>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoginMethod {
    Dev,
    #[default]
    GitHub,
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub login: LoginMethod,
    pub github_client_id: String,
    pub github_client_secret: String,
    pub github_redirect_uri: String,
    pub github_allowed_teams: Vec<String>,
    pub dev_mode_agent_tokens: bool,
}

impl AuthConfig {
    pub fn oauth_client(&self) -> OAuthClient {
        BasicClient::new(ClientId::new(self.github_client_id.clone()))
            .set_client_secret(ClientSecret::new(self.github_client_secret.clone()))
            .set_auth_uri(AuthUrl::new(GITHUB_AUTH_URL.to_string()).expect("invalid auth URL"))
            .set_token_uri(TokenUrl::new(GITHUB_TOKEN_URL.to_string()).expect("invalid token URL"))
            .set_redirect_uri(
                RedirectUrl::new(self.github_redirect_uri.clone()).expect("invalid redirect URI"),
            )
    }
}
