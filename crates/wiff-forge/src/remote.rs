//! Choosing which of a repository's remotes a pull request given by id lives
//! on. Resolving that id into a URL needs a forge adapter, chosen by host; the
//! choice narrows the repository's remotes to one whose host a forge is
//! configured for. It is neutral: it reads only the transport host of each clone
//! URL, generic addressing every scm shares, and leaves the owner and
//! repository past it to the adapter.

use anyhow::{Result, bail};
use wiff_core::source::Remote;

use crate::clone_url::parse_clone_url;
use crate::config::ForgeTable;

/// Choose the remote whose pull request `wiff forge pull <id>` resolves against,
/// returning it with its transport host: the one on a configured forge host,
/// preferring `origin` when several qualify. Fails, naming the hosts seen, when
/// no remote names a configured host, since then the id cannot be resolved and
/// the user must give a full URL instead.
pub fn select_pull_request_remote<'a>(
    remotes: &'a [Remote],
    table: &ForgeTable,
) -> Result<(&'a Remote, String)> {
    let configured: Vec<(&Remote, String)> = remotes
        .iter()
        .filter_map(|remote| {
            let host = transport_host(&remote.url)?;
            table.host(&host).map(|_| (remote, host))
        })
        .collect();
    if let Some(chosen) = configured
        .iter()
        .find(|(remote, _)| remote.name == "origin")
    {
        return Ok(chosen.clone());
    }
    if let Some(chosen) = configured.first() {
        return Ok(chosen.clone());
    }
    if remotes.is_empty() {
        bail!("the repository has no remotes; give a full pull-request URL instead");
    }
    bail!(
        "no remote names a configured forge host (saw {}); \
         give a full pull-request URL instead",
        seen_hosts(remotes).join(", ")
    );
}

/// The lowercased transport host of a git clone URL, or `None` for a local path
/// or a URL with no host.
fn transport_host(clone_url: &str) -> Option<String> {
    parse_clone_url(clone_url)?
        .host_str()
        .map(str::to_ascii_lowercase)
}

/// The distinct transport hosts across `remotes`, in the order first seen. A
/// remote whose URL yields no host stands for itself by its URL.
fn seen_hosts(remotes: &[Remote]) -> Vec<String> {
    let mut hosts = Vec::new();
    for remote in remotes {
        let host = transport_host(&remote.url).unwrap_or_else(|| remote.url.clone());
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    hosts
}

#[cfg(test)]
mod tests {
    use wiff_core::source::Remote;

    use super::select_pull_request_remote;
    use crate::config::{ForgeHost, ForgeTable};

    /// Build a remote with `name` pointing at `url`.
    fn remote(name: &str, url: &str) -> Remote {
        Remote {
            name: name.to_string(),
            url: url.to_string(),
        }
    }

    #[test]
    fn prefers_origin_among_several_configured_remotes() {
        let remotes = [
            remote("fork", "https://github.com/me/demo.git"),
            remote("origin", "git@github.com:octo/demo.git"),
        ];
        let chosen = select_pull_request_remote(&remotes, &ForgeTable::default()).expect("remote");
        wince::assert_eq!(
            (chosen.0.clone(), chosen.1),
            (
                remote("origin", "git@github.com:octo/demo.git"),
                "github.com".to_string()
            )
        );
    }

    #[test]
    fn takes_the_first_configured_remote_when_origin_is_not_on_a_forge() {
        let remotes = [
            remote("origin", "git@git.internal:ops/demo.git"),
            remote("hub", "https://github.com/octo/demo.git"),
        ];
        let chosen = select_pull_request_remote(&remotes, &ForgeTable::default()).expect("remote");
        wince::assert_eq!(
            (chosen.0.clone(), chosen.1),
            (
                remote("hub", "https://github.com/octo/demo.git"),
                "github.com".to_string()
            )
        );
    }

    #[test]
    fn reports_the_hosts_seen_when_none_are_configured() {
        let remotes = [
            remote("origin", "git@git.internal:ops/demo.git"),
            remote("mirror", "https://git.example.org/ops/demo.git"),
        ];
        let error = select_pull_request_remote(&remotes, &ForgeTable::default()).unwrap_err();
        wince::assert_eq!(
            error.to_string(),
            "no remote names a configured forge host (saw git.internal, git.example.org); \
             give a full pull-request URL instead"
        );
    }

    #[test]
    fn reports_a_repo_with_no_remotes() {
        let error = select_pull_request_remote(&[], &ForgeTable::default()).unwrap_err();
        wince::assert_eq!(
            error.to_string(),
            "the repository has no remotes; give a full pull-request URL instead"
        );
    }

    #[test]
    fn honors_a_configured_self_hosted_host() {
        let table = ForgeTable::from([(
            "git.example.org".to_string(),
            ForgeHost {
                provider: Some("github".to_string()),
                ..ForgeHost::default()
            },
        )]);
        let remotes = [remote("origin", "https://git.example.org/octo/demo.git")];
        let chosen = select_pull_request_remote(&remotes, &table).expect("remote");
        wince::assert_eq!(
            (chosen.0.clone(), chosen.1),
            (
                remote("origin", "https://git.example.org/octo/demo.git"),
                "git.example.org".to_string()
            )
        );
    }
}
