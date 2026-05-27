use serde::Deserialize;

#[derive(Clone, Deserialize)]
#[serde(tag = "type")]
pub enum Credentials {
    Basic { username: String, password: String },
    Bearer { username: String, token: String },
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credentials::Basic { username, .. } => f
                .debug_struct("Basic")
                .field("username", username)
                .field("password", &"[REDACTED]")
                .finish(),
            Credentials::Bearer { username, .. } => f
                .debug_struct("Bearer")
                .field("username", username)
                .field("token", &"[REDACTED]")
                .finish(),
        }
    }
}

impl Credentials {
    pub fn username(&self) -> &str {
        match self {
            Credentials::Basic { username, .. } | Credentials::Bearer { username, .. } => username,
        }
    }

    pub fn apply(
        &self,
        builder: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        match self {
            Credentials::Basic { username, password } => {
                builder.basic_auth(username, Some(password))
            }
            Credentials::Bearer { token, .. } => builder.bearer_auth(token),
        }
    }

    pub fn secret(&self) -> &str {
        match self {
            Credentials::Basic { password, .. } => password,
            Credentials::Bearer { token, .. } => token,
        }
    }

    pub fn is_bearer(&self) -> bool {
        matches!(self, Credentials::Bearer { .. })
    }
}
