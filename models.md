# Model and Provider Configuration

## Current Defaults
- Codex boots with the built-in `openai` provider (`codex-rs/core/src/model_provider_info.rs`) and the default model `gpt-5-codex` (`codex-rs/core/src/config.rs`).
- Providers determine the base endpoint, authentication mode, and wire protocol (`chat` vs `responses`). The provider also supplies retry tuning and custom headers (`codex-rs/core/src/model_provider_info.rs`).
- The `openai` provider talks to `/v1/responses` by default and can be redirected with `OPENAI_BASE_URL`, but query parameters and headers must still come from the provider definition (`codex-rs/core/src/model_provider_info.rs`).

## Override Mechanisms Already Supported
- Add entries under `[model_providers.<id>]` in `config.toml` to register new providers. Because the built-in map is merged with `or_insert`, new IDs extend the list without mutating OpenAI defaults (`codex-rs/core/src/config.rs`).
- Each provider can configure:
  - `base_url` and optional `query_params`, which are composed into the final REST target (`codex-rs/core/src/model_provider_info.rs`).
  - Static headers via `http_headers` and environment-sourced headers via `env_http_headers`.
  - An API key environment variable through `env_key`; this is wrapped in a bearer token unless overridden with headers.
- Azure-style endpoints are auto-detected when `wire_api = "responses"` and the provider name or URL hints at Azure, which triggers the `store=true` workaround for Responses (`codex-rs/core/src/client.rs`, `codex-rs/core/src/model_provider_info.rs`).

## Proposal: Azure OpenAI Endpoint for `gpt-5-codex`
1. Create a dedicated deployment (for example `gpt-5-codex`) inside the Azure OpenAI resource. Record the resource host, deployment name, and API version.
2. Place the following in `~/.codex_dev_home/config.toml` so the `CODEX_HOME` from the launch command picks it up:

```toml
model = "gpt-5-codex"            # must match your Azure deployment name
model_provider = "azure"         # switch away from the built-in OpenAI provider

[model_providers.azure]
name = "Azure"
base_url = "https://YOUR_RESOURCE.openai.azure.com/openai/deployments/gpt-5-codex"
query_params = { api-version = "2025-04-01-preview" }
wire_api = "responses"           # ensures Responses payloads + Azure store workaround

# Authentication: send the Azure key as both Authorization and api-key headers.
env_key = "AZURE_OPENAI_API_KEY"     # enables fallback bearer auth if supported
env_http_headers = { "api-key" = "AZURE_OPENAI_API_KEY" }
```

3. Export the Azure key before starting Codex:

```bash
export AZURE_OPENAI_API_KEY="<your key>"
```

4. Launch Codex using the provided command. With `CODEX_HOME=~/.codex_dev_home`, the CLI will read the profile above and target the Azure deployment:

```bash
RUST_LOG=trace CODEX_HOME=~/.codex_dev_home/ ./target/debug/codex -s danger-full-access mcp-server --port 8000
```

## Additional Notes
- The base URL must end at `/deployments/<deployment>` so Codex can append `/responses` automatically; avoid embedding query parameters directly in the base URL because `model_provider_info::get_full_url` handles them separately.
- Keeping the provider `name` as `"Azure"` allows Codex to detect the Azure Responses API and set `store=true`, preventing 400 errors when the service requires IDs (`codex-rs/core/src/client.rs`).
- If the deployment name differs from the public model slug, update both `model=` and the last path segment of `base_url` to match Azure’s deployment identifier. The slug is still used for Codex features (e.g., tool availability via `codex-rs/core/src/model_family.rs`).
- To revert to OpenAI’s public endpoint, remove `model_provider = "azure"` or point it back to `openai`; the rest of the configuration can remain for future use.
