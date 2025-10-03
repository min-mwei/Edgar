use clap::Parser;
use codex_arg0::arg0_dispatch_or_else;
use codex_common::CliConfigOverrides;
use codex_mcp_server::run_http_server;
use codex_mcp_server::run_main;

#[derive(Debug, Parser)]
#[clap(author, version, bin_name = "codex mcp-server")]
struct Cli {
    #[clap(flatten)]
    config_overrides: CliConfigOverrides,

    /// Bind and listen on this port for HTTP mode. If omitted, runs in stdio mode.
    #[arg(short = 'p', long = "port")]
    port: Option<u16>,

    /// Host/interface to bind for HTTP mode (default 127.0.0.1)
    #[arg(long = "host", default_value = "127.0.0.1")]
    host: String,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    arg0_dispatch_or_else(|codex_linux_sandbox_exe| async move {
        match cli.port {
            Some(port) => {
                run_http_server(
                    codex_linux_sandbox_exe,
                    cli.config_overrides,
                    cli.host,
                    port,
                )
                .await?;
            }
            None => {
                run_main(codex_linux_sandbox_exe, cli.config_overrides).await?;
            }
        }
        Ok(())
    })
}
