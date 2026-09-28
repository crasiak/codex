use super::MultitoolCli;
use super::profile_v2_for_subcommand;
use clap::Parser;
use pretty_assertions::assert_eq;

#[test]
fn profile_v2_is_allowed_for_standalone_app_server() {
    for args in [
        vec!["codex", "--profile", "work", "app-server"],
        vec!["codex", "--profile", "work", "app-server", "--stdio"],
    ] {
        let cli = MultitoolCli::try_parse_from(args).expect("parse standalone app-server");
        let profile = profile_v2_for_subcommand(
            &cli.interactive,
            cli.subcommand.as_ref().expect("app-server subcommand"),
        )
        .expect("standalone app-server supports profile-v2");
        assert_eq!(profile.map(ToString::to_string), Some("work".to_string()));
    }
}

#[test]
fn profile_v2_is_rejected_for_app_server_tooling() {
    for args in [
        vec![
            "codex",
            "--profile",
            "work",
            "app-server",
            "daemon",
            "start",
        ],
        vec![
            "codex",
            "--profile",
            "work",
            "app-server",
            "generate-ts",
            "--out",
            "unused",
        ],
        vec![
            "codex",
            "--profile",
            "work",
            "app-server",
            "generate-json-schema",
            "--out",
            "unused",
        ],
    ] {
        let cli = MultitoolCli::try_parse_from(args).expect("parse app-server tooling");
        assert!(
            profile_v2_for_subcommand(
                &cli.interactive,
                cli.subcommand.as_ref().expect("app-server subcommand"),
            )
            .is_err()
        );
    }
}

#[test]
fn standalone_app_server_without_profile_keeps_default_selection() {
    let cli = MultitoolCli::try_parse_from(["codex", "app-server", "--stdio"])
        .expect("parse standalone app-server");
    let profile = profile_v2_for_subcommand(
        &cli.interactive,
        cli.subcommand.as_ref().expect("app-server subcommand"),
    )
    .expect("standalone app-server without profile");
    assert_eq!(profile, None);
}
