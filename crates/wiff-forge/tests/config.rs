#![allow(missing_docs)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use wiff_forge::{ForgeHost, ForgeTable, TokenOverride, resolve_token};

/// Look a variable up from a fixed set, as [`resolve_token`] expects.
fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |name: &str| map.get(name).cloned()
}

/// Write `contents` to a named token file inside `dir` and return its path.
fn token_file(dir: &tempfile::TempDir, name: &str, contents: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, contents).expect("write token file");
    path
}

#[test]
fn an_unknown_host_is_absent_but_a_built_in_resolves() {
    let table = ForgeTable::default();

    wince::assert_eq!(table.host("git.example.org"), None);
    wince::assert_eq!(
        table.host("github.com"),
        Some(ForgeHost {
            provider: Some("github".to_string()),
            api_base: None,
            token_env: Some("GITHUB_TOKEN".to_string()),
            token_file_env: Some("GITHUB_TOKEN_FILE".to_string()),
        })
    );
}

#[test]
fn a_user_entry_overlays_a_built_in_key_by_key() {
    let table = ForgeTable::from([(
        "github.com".to_string(),
        ForgeHost {
            token_env: Some("MY_GH_TOKEN".to_string()),
            ..ForgeHost::default()
        },
    )]);

    // Only token_env is overridden; provider and token_file_env keep the
    // built-in values.
    wince::assert_eq!(
        table.host("github.com"),
        Some(ForgeHost {
            provider: Some("github".to_string()),
            api_base: None,
            token_env: Some("MY_GH_TOKEN".to_string()),
            token_file_env: Some("GITHUB_TOKEN_FILE".to_string()),
        })
    );
}

#[test]
fn a_mixed_case_host_key_matches_the_lowercased_lookup() {
    let table = ForgeTable::from([(
        "Git.Example.Org".to_string(),
        ForgeHost {
            provider: Some("forgejo".to_string()),
            ..ForgeHost::default()
        },
    )]);

    // ForgeUrl::host() yields a lowercased host, so the verbatim TOML key is
    // lowercased on the way in and both spellings resolve.
    let expected = Some(ForgeHost {
        provider: Some("forgejo".to_string()),
        api_base: None,
        token_env: None,
        token_file_env: None,
    });
    wince::assert_eq!(table.host("git.example.org"), expected.clone());
    wince::assert_eq!(table.host("Git.Example.Org"), expected);
}

#[test]
fn an_ipv6_literal_host_resolves_by_its_bracketed_key() {
    let table = ForgeTable::from([(
        "[2001:db8::1]".to_string(),
        ForgeHost {
            provider: Some("forgejo".to_string()),
            ..ForgeHost::default()
        },
    )]);

    wince::assert_eq!(
        table.host("[2001:db8::1]"),
        Some(ForgeHost {
            provider: Some("forgejo".to_string()),
            api_base: None,
            token_env: None,
            token_file_env: None,
        })
    );
}

#[test]
fn a_self_hosted_host_resolves_to_its_own_entry() {
    let table = ForgeTable::from([(
        "git.example.org".to_string(),
        ForgeHost {
            provider: Some("forgejo".to_string()),
            api_base: Some("https://git.example.org/api/v1".to_string()),
            token_env: Some("EXAMPLE_TOKEN".to_string()),
            token_file_env: None,
        },
    )]);

    wince::assert_eq!(
        table.host("git.example.org"),
        Some(ForgeHost {
            provider: Some("forgejo".to_string()),
            api_base: Some("https://git.example.org/api/v1".to_string()),
            token_env: Some("EXAMPLE_TOKEN".to_string()),
            token_file_env: None,
        })
    );
}

#[test]
fn the_token_is_drawn_in_order_of_precedence() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cli_file = token_file(&dir, "cli", "  cli-file-token\n");
    let env_file = token_file(&dir, "env", "env-file-token\n");
    let host = ForgeHost {
        provider: Some("github".to_string()),
        api_base: None,
        token_env: Some("TOKEN".to_string()),
        token_file_env: Some("TOKEN_FILE".to_string()),
    };
    let env = env_from(&[
        ("TOKEN", "env-token"),
        ("TOKEN_FILE", env_file.to_str().expect("utf-8 path")),
    ]);

    // The command-line file wins over every other source.
    wince::assert_eq!(
        resolve_token(
            &host,
            &TokenOverride {
                token_file: Some(cli_file.clone()),
                token: Some("cli-token".to_string()),
            },
            &env,
        )
        .expect("cli file resolves"),
        "cli-file-token".to_string()
    );

    // Without a file, the command-line token wins over the environment.
    wince::assert_eq!(
        resolve_token(
            &host,
            &TokenOverride {
                token_file: None,
                token: Some("cli-token".to_string()),
            },
            &env,
        )
        .expect("cli token resolves"),
        "cli-token".to_string()
    );

    // With no command-line source, the file named by token_file_env wins over
    // token_env.
    wince::assert_eq!(
        resolve_token(&host, &TokenOverride::default(), &env).expect("env file resolves"),
        "env-file-token".to_string()
    );

    // With neither command-line nor token_file_env, token_env is used.
    let env_no_file = env_from(&[("TOKEN", "env-token")]);
    wince::assert_eq!(
        resolve_token(&host, &TokenOverride::default(), &env_no_file).expect("env token resolves"),
        "env-token".to_string()
    );
}

#[test]
fn an_empty_command_line_token_is_an_error() {
    let error = resolve_token(
        &ForgeHost::default(),
        &TokenOverride {
            token_file: None,
            token: Some("   ".to_string()),
        },
        env_from(&[]),
    )
    .expect_err("an empty --forge-token is rejected");

    wince::assert_eq!(
        error.to_string(),
        "the forge token given with --forge-token is empty".to_string()
    );
}

#[test]
fn an_empty_token_env_value_is_an_error() {
    let host = ForgeHost {
        token_env: Some("TOKEN".to_string()),
        ..ForgeHost::default()
    };

    let error = resolve_token(
        &host,
        &TokenOverride::default(),
        env_from(&[("TOKEN", "  ")]),
    )
    .expect_err("an empty token_env value is rejected");

    wince::assert_eq!(
        error.to_string(),
        "the forge token in $TOKEN is empty".to_string()
    );
}

#[test]
fn an_empty_token_file_env_value_is_an_error() {
    let host = ForgeHost {
        token_env: Some("TOKEN".to_string()),
        token_file_env: Some("TOKEN_FILE".to_string()),
        ..ForgeHost::default()
    };

    // An empty token_file_env value errors rather than falling through to
    // token_env, matching the token_env and file sources.
    let error = resolve_token(
        &host,
        &TokenOverride::default(),
        env_from(&[("TOKEN", "env-token"), ("TOKEN_FILE", "  ")]),
    )
    .expect_err("an empty token_file_env value is rejected");

    wince::assert_eq!(
        error.to_string(),
        "the forge token file path in $TOKEN_FILE is empty".to_string()
    );
}

#[test]
fn a_named_but_empty_token_file_is_an_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let empty = token_file(&dir, "token", "   \n");
    let host = ForgeHost::default();

    let error = resolve_token(
        &host,
        &TokenOverride {
            token_file: Some(empty.clone()),
            token: None,
        },
        env_from(&[]),
    )
    .expect_err("an empty file is rejected");

    wince::assert_eq!(
        error.to_string(),
        format!("the forge token file {} is empty", empty.display())
    );
}

#[test]
fn a_named_but_missing_token_file_is_an_error() {
    let host = ForgeHost::default();
    let missing = PathBuf::from("/does/not/exist/token");

    let error = resolve_token(
        &host,
        &TokenOverride {
            token_file: Some(missing.clone()),
            token: None,
        },
        env_from(&[]),
    )
    .expect_err("a missing file is rejected");

    wince::assert_eq!(
        error.to_string(),
        format!("reading forge token from {}", missing.display())
    );
}

#[test]
fn no_source_at_all_is_an_error() {
    let host = ForgeHost {
        token_env: Some("TOKEN".to_string()),
        token_file_env: Some("TOKEN_FILE".to_string()),
        ..ForgeHost::default()
    };

    let error = resolve_token(&host, &TokenOverride::default(), env_from(&[]))
        .expect_err("no token anywhere is rejected");

    wince::assert_eq!(
        error.to_string(),
        "no forge token found; pass --forge-token-file or --forge-token, or set the host's token variable".to_string()
    );
}
