use azure_core::auth::TokenCredential;
use azure_identity::DefaultAzureCredential;
use azure_identity::TokenCredentialOptions;

use crate::error::CodexErr;
use crate::error::Result;

const DEFAULT_AZURE_SCOPE: &str = "https://cognitiveservices.azure.com/.default";

/// Acquire an Azure Active Directory access token using the default credential chain.
///
/// The scope should be the resource URI with a `/.default` suffix, for example
/// `https://<resource>.cognitiveservices.azure.com/.default`.
pub async fn acquire_access_token(scope: Option<&str>) -> Result<String> {
    let scope = scope.unwrap_or(DEFAULT_AZURE_SCOPE);
    let credential =
        DefaultAzureCredential::create(TokenCredentialOptions::default()).map_err(|error| {
            CodexErr::Fatal(format!(
                "failed to initialise Azure default credential: {error}"
            ))
        })?;

    credential
        .get_token(&[scope])
        .await
        .map(|token| token.token.secret().to_owned())
        .map_err(|error| {
            CodexErr::Fatal(format!(
                "failed to acquire Azure AD access token for scope `{scope}`: {error}"
            ))
        })
}

pub fn scope_from_host(host: &str) -> String {
    host.split_once('.')
        .map(|(_, suffix)| format!("https://{suffix}/.default"))
        .unwrap_or_else(|| DEFAULT_AZURE_SCOPE.to_string())
}

pub fn default_scope() -> &'static str {
    DEFAULT_AZURE_SCOPE
}
