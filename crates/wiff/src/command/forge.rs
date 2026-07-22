//! `wiff forge`: mirror a pull request into a session and publish the review
//! back to its host. The credential overrides and the step that turns a pull
//! request's host into a connected adapter are common to every subcommand, so
//! they live here; each subcommand owns the round-trips it drives through that
//! adapter.

use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use wiff_config::Config;
use wiff_core::record::ForgeUrl;
use wiff_forge::{Forge, GithubForge, TokenOverride, resolve_token};

/// Arguments for `wiff forge`.
#[derive(Debug, Args)]
pub struct ForgeArgs {
    #[command(flatten)]
    token: ForgeToken,
    #[command(subcommand)]
    command: ForgeCommand,
}

impl ForgeArgs {
    /// Dispatch the selected `wiff forge` subcommand.
    pub async fn run(self) -> anyhow::Result<()> {
        match self.command {
            ForgeCommand::Pull => {
                bail!("wiff forge pull is not implemented yet")
            }
            ForgeCommand::Push => {
                bail!("wiff forge push is not implemented yet")
            }
        }
    }
}

/// The credentials for the forge, given directly or as a file to read the
/// token from. The two are mutually exclusive.
#[derive(Debug, Args)]
struct ForgeToken {
    /// Read the forge token from this file, using its trimmed contents.
    #[arg(long)]
    forge_token_file: Option<PathBuf>,
    /// The forge token, given directly.
    #[arg(long, conflicts_with = "forge_token_file")]
    forge_token: Option<String>,
}

impl ForgeToken {
    /// The command-line token overrides these arguments express. Consumed by the
    /// `pull` and `push` handlers, which arrive in the following steps.
    #[allow(dead_code)]
    fn overrides(&self) -> TokenOverride {
        TokenOverride {
            token_file: self.forge_token_file.clone(),
            token: self.forge_token.clone(),
        }
    }
}

/// The `wiff forge` subcommands.
#[derive(Debug, Subcommand)]
enum ForgeCommand {
    /// Fetch a pull request into a session and open it.
    Pull,
    /// Publish the local review to the bound pull request.
    Push,
}

/// Build the forge adapter for the pull request at `url`: look its host up in
/// the effective forge table, resolve the token from the command-line overrides
/// or the host's configured variables, and construct the adapter the host's
/// provider names. Consumed by the `pull` and `push` handlers, which arrive in
/// the following steps.
#[allow(dead_code)]
pub(crate) fn connect_forge(
    config: &Config,
    url: &ForgeUrl,
    cli: &TokenOverride,
) -> anyhow::Result<Box<dyn Forge>> {
    let host = url.host();
    let row = config.forge.host(&host).with_context(|| {
        format!(
            "no forge is configured for {host}; add a [forge.\"{host}\"] entry naming its provider"
        )
    })?;
    let provider = row
        .provider
        .as_deref()
        .with_context(|| format!("the forge entry for {host} names no provider"))?;
    // Match the provider before resolving the token: an adapter wiff cannot
    // build should say so rather than first demand a credential it will not use.
    match provider {
        "github" => {
            let token = resolve_token(&row, cli, |name| std::env::var(name).ok())?;
            Ok(Box::new(GithubForge::new(&token, row.api_base.as_deref())?))
        }
        "forgejo" => bail!("the forgejo forge adapter is not available yet"),
        other => bail!("host {host} names an unknown forge provider {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use wiff_forge::{ForgeHost, ForgeTable};

    use super::*;

    /// A config whose forge table is `table` and whose other fields are the
    /// defaults, for exercising `connect_forge` without a config file.
    fn config_with(table: ForgeTable) -> Config {
        Config {
            forge: table,
            ..Config::default()
        }
    }

    /// The token override that hands the token over directly, so the resolution
    /// never consults the environment.
    fn direct_token(token: &str) -> TokenOverride {
        TokenOverride {
            token_file: None,
            token: Some(token.to_string()),
        }
    }

    // Building the octocrab-backed adapter spawns a background service, so it
    // needs a tokio runtime even though no request is made.
    #[tokio::test]
    async fn a_github_host_builds_an_adapter() {
        let config = config_with(ForgeTable::default());
        let url = ForgeUrl::parse("https://github.com/octo/demo/pull/7").unwrap();
        connect_forge(&config, &url, &direct_token("t")).expect("github adapter");
    }

    #[test]
    fn an_unconfigured_host_is_reported_with_its_name() {
        let config = config_with(ForgeTable::default());
        let url = ForgeUrl::parse("https://git.example.org/octo/demo/pull/7").unwrap();
        let error = connect_forge(&config, &url, &TokenOverride::default())
            .map(|_| ())
            .unwrap_err();
        wince::assert_eq!(
            error.to_string(),
            "no forge is configured for git.example.org; add a \
             [forge.\"git.example.org\"] entry naming its provider"
        );
    }

    #[test]
    fn a_forgejo_host_reports_the_adapter_is_unavailable() {
        let config = config_with(ForgeTable::default());
        let url = ForgeUrl::parse("https://codeberg.org/octo/demo/pulls/7").unwrap();
        // No token is supplied: an adapter wiff cannot build is reported before
        // any credential is demanded.
        let error = connect_forge(&config, &url, &TokenOverride::default())
            .map(|_| ())
            .unwrap_err();
        wince::assert_eq!(
            error.to_string(),
            "the forgejo forge adapter is not available yet"
        );
    }

    #[test]
    fn an_unknown_provider_is_reported_with_the_host() {
        let table = ForgeTable::from([(
            "git.example.org".to_string(),
            ForgeHost {
                provider: Some("bitbucket".to_string()),
                ..ForgeHost::default()
            },
        )]);
        let config = config_with(table);
        let url = ForgeUrl::parse("https://git.example.org/octo/demo/pull/7").unwrap();
        let error = connect_forge(&config, &url, &TokenOverride::default())
            .map(|_| ())
            .unwrap_err();
        wince::assert_eq!(
            error.to_string(),
            "host git.example.org names an unknown forge provider \"bitbucket\""
        );
    }
}
