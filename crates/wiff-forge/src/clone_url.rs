//! Turning a git remote's clone URL into a [`Url`]. An scm hands wiff a
//! remote's clone URL as its own text, in either URL or scp-style form; reading
//! a host, owner, and repository out of it is the forge layer's concern. The
//! neutral remote selection and each forge adapter begin from the same parse, so
//! it lives here rather than in either.

use url::Url;

/// Parse a git clone URL into a [`Url`], accepting both a `scheme://...` URL and
/// git's scp-style `[user@]host:owner/repo`, which is not a URL on its own but
/// addresses the same host as `ssh://[user@]host/owner/repo`. Returns `None` for
/// a local filesystem path, which names no host.
pub(crate) fn parse_clone_url(clone_url: &str) -> Option<Url> {
    let url = if clone_url.contains("://") {
        Url::parse(clone_url).ok()?
    } else {
        let (authority, path) = clone_url.split_once(':')?;
        let is_windows_drive =
            authority.len() == 1 && authority.chars().all(|byte| byte.is_ascii_alphabetic());
        if authority.contains('/') || is_windows_drive {
            return None;
        }
        Url::parse(&format!("ssh://{authority}/{path}")).ok()?
    };
    // A clone URL names a host to reach; an empty scp authority or a hostless
    // scheme like `file://` is a local path, so require a host before returning.
    url.host_str().filter(|host| !host.is_empty())?;
    Some(url)
}

#[cfg(test)]
mod tests {
    use super::parse_clone_url;

    /// The host and path of each clone-URL form, or a marker when the text names
    /// no host and so is not a remote.
    fn host_and_path(clone_url: &str) -> String {
        match parse_clone_url(clone_url) {
            Some(url) => format!("{}{}", url.host_str().unwrap_or("<none>"), url.path()),
            None => "<not a remote>".to_string(),
        }
    }

    #[test]
    fn parses_url_and_scp_forms_and_rejects_local_paths() {
        let mapped: Vec<String> = [
            "https://github.com/octo/demo.git",
            "ssh://git@ghe.corp:2222/octo/demo",
            "git@github.com:octo/demo.git",
            "/home/me/repo",
            "C:/repos/demo",
            "../sibling",
            "file:///srv/git/repo",
            ":owner/repo",
        ]
        .into_iter()
        .map(|clone_url| format!("{clone_url} -> {}", host_and_path(clone_url)))
        .collect();
        wince::assert_eq!(
            mapped.join("\n"),
            "https://github.com/octo/demo.git -> github.com/octo/demo.git\n\
             ssh://git@ghe.corp:2222/octo/demo -> ghe.corp/octo/demo\n\
             git@github.com:octo/demo.git -> github.com/octo/demo.git\n\
             /home/me/repo -> <not a remote>\n\
             C:/repos/demo -> <not a remote>\n\
             ../sibling -> <not a remote>\n\
             file:///srv/git/repo -> <not a remote>\n\
             :owner/repo -> <not a remote>"
        );
    }
}
