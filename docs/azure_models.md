# Using Azure-hosted OpenAI Models with Codex

Codex can talk to Azure OpenAI deployments through the same configuration system that is used for all model providers. This document summarizes the moving pieces—model selection, endpoint wiring, and authentication—so you can point Codex at a regional Azure deployment with minimal fuss.

## Configure the model provider

1. Pick the deployment you want to target (e.g. `gpt-5-chat`). Make sure the deployment’s model name matches what you expect Codex to use in prompts.
2. Add an Azure provider entry to `~/.codex/config.toml` (or the `CODEX_HOME` you launch Codex with). A minimal example:

```toml
model = "gpt-5-chat"
model_provider = "azure"

[model_providers.azure]
name = "Azure"
base_url = "https://YOUR_RESOURCE.cognitiveservices.azure.com/openai/deployments/gpt-5-chat"
query_params = { api-version = "2025-01-01-preview" }
wire_api = "chat"
```

Codex appends the correct `chat/completions` or `responses` suffix based on `wire_api`, so keep `base_url` at the deployment root.

## Authentication flow

Codex now mirrors the logic from `gpt5_example3.py`:

- If a provider includes an explicit API key (`env_key`) or hard-coded auth header, Codex sends that value.
- Otherwise, when the base URL matches an Azure domain (`*.cognitiveservices.azure.com`, `*.openai.azure.com`, etc.), Codex requests an Entra ID token using the Azure Identity SDK (`DefaultAzureCredential`).
- The scope is derived from the host suffix, e.g. `https://cognitiveservices.azure.com/.default` or `https://cognitiveservices.azure.us/.default`, matching Azure’s authentication requirements.

### Preparing credentials

Configure an identity that has permission to call your Azure OpenAI resource. Common options:

- **Azure CLI login** (interactive or service principal):
  ```bash
  az login --tenant <tenant-id>
  az account set --subscription <subscription-id>
  ```
- **Managed identity** on the host machine or VM.
- **Environment variables** supported by `DefaultAzureCredential` (`AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_CLIENT_SECRET`, etc.).

No manual token export is required; Codex asks `DefaultAzureCredential` for a token when it needs one. If the credential chain fails, the error in `~/.codex/log/codex-rmcp.log` will include Azure Identity’s diagnostic hints (for example, suggesting `az login --scope ...`).

## Handling regions and clouds

The scope derivation looks at the part of the host *after* the first dot. Examples:

| Host                               | Derived scope                                     |
|------------------------------------|---------------------------------------------------|
| `gladius-local-aoi.cognitiveservices.azure.com` | `https://cognitiveservices.azure.com/.default` |
| `my-resource.openai.azure.com`     | `https://openai.azure.com/.default`               |
| `contoso.openai.azure.us`          | `https://azure.us/.default`                       |

Most commercial regions use the standard `cognitiveservices.azure.com` suffix. Sovereign or air-gapped clouds may need additional configuration; adjust the base URL accordingly so the derived scope matches the resource’s documentation.

## Best practices

- Keep the deployment model name (`model = ...`) in sync with the actual Azure deployment name.
- For Responses API workloads, set `wire_api = "responses"`; Codex keeps the Azure `store=true` workaround in place automatically.
- When experimenting, tail `~/.codex/log/codex-rmcp.log` to verify that Codex is fetching tokens and hitting the expected endpoint.
- If you prefer to inject custom headers (for proxies or private gateways), use `[model_providers.azure].http_headers` or `.env_http_headers`; Codex will skip the Entra fallback when an auth header is already present.

With these pieces in place, Codex will authenticate against Azure OpenAI just as reliably as the standalone Python sample, while letting you reuse the broader CLI tooling and configuration machinery.
