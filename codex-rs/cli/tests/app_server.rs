use std::path::Path;

use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::app_server_json_shutdown_event;
use codex_app_server_protocol::ConfigBatchWriteParams;
use codex_app_server_protocol::ConfigEdit;
use codex_app_server_protocol::ConfigLayerSource;
use codex_app_server_protocol::ConfigReadParams;
use codex_app_server_protocol::ConfigReadResponse;
use codex_app_server_protocol::ConfigValueWriteParams;
use codex_app_server_protocol::ConfigWriteResponse;
use codex_app_server_protocol::MergeStrategy;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SandboxMode;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_utils_absolute_path::AbsolutePathBuf;
use predicates::str::contains;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

const PROFILE_INITIALIZATION_TIMEOUT: Duration = Duration::from_secs(60);
const PROFILE_RPC_TIMEOUT: Duration = Duration::from_secs(20);
const PROFILE_TEST_BASE: &str = r#"model = "base-probe"
model_provider = "probe"
cli_auth_credentials_store = "file"
[model_providers.probe]
name = "probe"
base_url = "http://127.0.0.1:1/v1"
wire_api = "responses"
[features]
code_mode = false

[mcp_servers.ledger_desired]
command = "/not-executed"
enabled = false

[hooks]

[[hooks.PreToolUse]]
matcher = "Bash"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "/not-executed"
"#;

async fn start_profile_app_server(home_path: &Path, program: &Path) -> Result<TestAppServer> {
    let home_string = home_path.to_string_lossy();
    TestAppServer::builder()
        .with_program(program)
        .with_codex_home(home_path)
        .without_auto_env()
        .without_managed_config()
        // The standalone CLI does not expose the test binary's startup flag.
        .with_plugin_startup_tasks()
        .with_env_overrides(&[
            ("HOME", Some(home_string.as_ref())),
            ("OPENAI_API_KEY", None),
            ("CODEX_API_KEY", None),
            ("CODEX_APP_SERVER_TEST_USER_CONFIG_FILE", None),
        ])
        .with_args(&["--profile", "work", "app-server", "--stdio"])
        .build_initialized_with_timeout(PROFILE_INITIALIZATION_TIMEOUT)
        .await
}

fn assert_user_origin(
    config: &ConfigReadResponse,
    key: &str,
    file: &Path,
    profile: Option<&str>,
) -> Result<()> {
    assert_eq!(
        config
            .origins
            .get(key)
            .unwrap_or_else(|| panic!("{key} origin"))
            .name,
        ConfigLayerSource::User {
            file: AbsolutePathBuf::from_absolute_path(file)?,
            profile: profile.map(str::to_string),
        }
    );
    Ok(())
}

#[tokio::test]
async fn profile_v2_app_server_uses_selected_layer_for_effective_config() -> Result<()> {
    // Configuration and thread startup only: this test never sends turn/start.
    let home = TempDir::new()?;
    let home_path = home.path().canonicalize()?;
    let base_path = home_path.join("config.toml");
    let selected_path = home_path.join("work.config.toml");
    std::fs::write(&base_path, PROFILE_TEST_BASE)?;
    std::fs::write(&selected_path, "model = \"selected-probe\"\n")?;
    let project = home_path.join("repo");
    std::fs::create_dir(&project)?;
    let program = codex_utils_cargo_bin::cargo_bin("codex")?;
    let mut server = start_profile_app_server(&home_path, &program).await?;
    let id = server
        .send_config_read_request(ConfigReadParams {
            include_layers: true,
            cwd: None,
        })
        .await?;
    let config: ConfigReadResponse =
        timeout(PROFILE_RPC_TIMEOUT, server.read_response(id)).await??;
    assert_eq!(config.config.model.as_deref(), Some("selected-probe"));
    assert_eq!(
        config.origins.get("model").expect("model origin").name,
        ConfigLayerSource::User {
            file: AbsolutePathBuf::from_absolute_path(&selected_path)?,
            profile: Some("work".to_string()),
        }
    );

    let id = server
        .send_thread_start_request(ThreadStartParams {
            cwd: Some(project.to_string_lossy().into_owned()),
            sandbox: Some(SandboxMode::DangerFullAccess),
            ..Default::default()
        })
        .await?;
    let thread: ThreadStartResponse =
        timeout(PROFILE_RPC_TIMEOUT, server.read_response(id)).await??;
    assert_eq!(thread.model, "selected-probe");
    assert_eq!(std::fs::read_to_string(&base_path)?, PROFILE_TEST_BASE);
    assert!(
        timeout(PROFILE_RPC_TIMEOUT, server.shutdown_gracefully())
            .await??
            .success()
    );
    Ok(())
}

#[tokio::test]
async fn profile_v2_app_server_preserves_ledger_runtime_facade() -> Result<()> {
    // Configuration only: no thread or turn is started.
    let home = TempDir::new()?;
    let home_path = home.path().canonicalize()?;
    if let Some(root) = std::env::var_os("CRA357_DISPOSABLE_ROOT") {
        let root = std::path::PathBuf::from(root).canonicalize()?;
        assert!(
            home_path.starts_with(&root),
            "fixture home escaped disposable root"
        );
    }
    let active_path = home_path.join("active");
    std::fs::create_dir(&active_path)?;
    let immutable_base_path = active_path.join("ledger.config.toml");
    std::fs::write(&immutable_base_path, PROFILE_TEST_BASE)?;
    let base_path = home_path.join("config.toml");
    #[cfg(unix)]
    std::os::unix::fs::symlink("active/ledger.config.toml", &base_path)?;
    #[cfg(not(unix))]
    std::fs::write(&base_path, PROFILE_TEST_BASE)?;

    let selected_path = home_path.join("work.config.toml");
    std::fs::write(
        &selected_path,
        r#"[hooks.state.fixture]
enabled = true

[tui.model_availability_nux]
"gpt-5" = 3
"#,
    )?;
    let project = home_path.join("repo");
    std::fs::create_dir(&project)?;
    let program = codex_utils_cargo_bin::cargo_bin("codex")?;

    // Restart after the edits to verify selection and persistence together.
    for restarted in [false, true] {
        let mut server = start_profile_app_server(&home_path, &program).await?;
        let id = server
            .send_config_read_request(ConfigReadParams {
                include_layers: true,
                cwd: None,
            })
            .await?;
        let config: ConfigReadResponse =
            timeout(PROFILE_RPC_TIMEOUT, server.read_response(id)).await??;
        assert_eq!(config.config.model.as_deref(), Some("base-probe"));
        assert_user_origin(&config, "model", &base_path, None)?;
        assert_user_origin(
            &config,
            "mcp_servers.ledger_desired.command",
            &base_path,
            None,
        )?;
        assert_user_origin(
            &config,
            "hooks.PreToolUse.0.hooks.0.command",
            &base_path,
            None,
        )?;
        assert_user_origin(
            &config,
            "hooks.state.fixture.enabled",
            &selected_path,
            Some("work"),
        )?;
        let effective = serde_json::to_value(&config.config)?;
        assert_eq!(
            effective["mcp_servers"]["ledger_desired"]["enabled"],
            json!(false)
        );
        assert_eq!(
            effective["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            json!("/not-executed")
        );

        assert_eq!(std::fs::read_to_string(&base_path)?, PROFILE_TEST_BASE);
        assert_eq!(
            std::fs::read_to_string(&immutable_base_path)?,
            PROFILE_TEST_BASE
        );
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_link(&base_path)?,
            Path::new("active/ledger.config.toml")
        );

        if !restarted {
            let project_key = format!("projects.{}.trust_level", json!(project));
            let id = server
                .send_config_value_write_request(ConfigValueWriteParams {
                    key_path: project_key,
                    value: json!("trusted"),
                    merge_strategy: MergeStrategy::Upsert,
                    file_path: None,
                    expected_version: None,
                })
                .await?;
            let _: ConfigWriteResponse =
                timeout(PROFILE_RPC_TIMEOUT, server.read_response(id)).await??;

            let id = server
                .send_config_value_write_request(ConfigValueWriteParams {
                    key_path: "hooks.state.fixture.enabled".to_string(),
                    value: json!(false),
                    merge_strategy: MergeStrategy::Replace,
                    file_path: None,
                    expected_version: None,
                })
                .await?;
            let _: ConfigWriteResponse =
                timeout(PROFILE_RPC_TIMEOUT, server.read_response(id)).await??;

            let project_key = format!("projects.{}.trust_level", json!(home_path.join("rpc-repo")));
            let id = server
                .send_config_batch_write_request(ConfigBatchWriteParams {
                    edits: vec![ConfigEdit {
                        key_path: project_key,
                        value: json!("trusted"),
                        merge_strategy: MergeStrategy::Upsert,
                    }],
                    file_path: None,
                    expected_version: None,
                    reload_user_config: true,
                })
                .await?;
            let _: ConfigWriteResponse =
                timeout(PROFILE_RPC_TIMEOUT, server.read_response(id)).await??;

            let id = server
                .send_config_read_request(ConfigReadParams {
                    include_layers: true,
                    cwd: None,
                })
                .await?;
            let reloaded: ConfigReadResponse =
                timeout(PROFILE_RPC_TIMEOUT, server.read_response(id)).await??;
            assert_user_origin(
                &reloaded,
                "hooks.state.fixture.enabled",
                &selected_path,
                Some("work"),
            )?;
            assert_eq!(
                serde_json::to_value(&reloaded.config)?["hooks"]["state"]["fixture"]["enabled"],
                json!(false)
            );
            assert_user_origin(
                &reloaded,
                "mcp_servers.ledger_desired.command",
                &base_path,
                None,
            )?;
            assert_user_origin(
                &reloaded,
                "hooks.PreToolUse.0.hooks.0.command",
                &base_path,
                None,
            )?;

            let id = server
                .send_config_value_write_request(ConfigValueWriteParams {
                    key_path: "model".to_string(),
                    value: json!("must-not-replace-base"),
                    merge_strategy: MergeStrategy::Replace,
                    file_path: Some(base_path.to_string_lossy().into_owned()),
                    expected_version: None,
                })
                .await?;
            let error = timeout(
                PROFILE_RPC_TIMEOUT,
                server.read_stream_until_error_message(RequestId::Integer(id)),
            )
            .await??;
            assert_eq!(
                error.error.data,
                Some(json!({
                    "config_write_error_code": "configLayerReadonly",
                }))
            );

            let id = server
                .send_config_batch_write_request(ConfigBatchWriteParams {
                    edits: vec![ConfigEdit {
                        key_path: "hooks.state.fixture.enabled".to_string(),
                        value: json!(true),
                        merge_strategy: MergeStrategy::Replace,
                    }],
                    file_path: Some(base_path.to_string_lossy().into_owned()),
                    expected_version: None,
                    reload_user_config: true,
                })
                .await?;
            let error = timeout(
                PROFILE_RPC_TIMEOUT,
                server.read_stream_until_error_message(RequestId::Integer(id)),
            )
            .await??;
            assert_eq!(
                error.error.data,
                Some(json!({
                    "config_write_error_code": "configLayerReadonly",
                }))
            );
        }

        let selected: toml::Value = toml::from_str(&std::fs::read_to_string(&selected_path)?)?;
        let project = project.to_string_lossy().into_owned();
        let rpc_project = home_path.join("rpc-repo").to_string_lossy().into_owned();
        assert_eq!(
            serde_json::to_value(selected)?,
            json!({
                "hooks": {"state": {"fixture": {"enabled": false}}},
                "tui": {"model_availability_nux": {"gpt-5": 3}},
                "projects": {
                    (project): {"trust_level": "trusted"},
                    (rpc_project): {"trust_level": "trusted"},
                },
            })
        );
        assert_eq!(std::fs::read_to_string(&base_path)?, PROFILE_TEST_BASE);
        assert_eq!(
            std::fs::read_to_string(&immutable_base_path)?,
            PROFILE_TEST_BASE
        );
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_link(&base_path)?,
            Path::new("active/ledger.config.toml")
        );
        assert!(
            timeout(PROFILE_RPC_TIMEOUT, server.shutdown_gracefully())
                .await??
                .success()
        );
    }
    Ok(())
}

#[tokio::test]
async fn profile_v2_app_server_skill_write_targets_selected_layer() -> Result<()> {
    // This characterizes native Codex semantics. Ledger does not currently
    // admit skills.config as writable runtime-facade state.
    let home = TempDir::new()?;
    let home_path = home.path().canonicalize()?;
    if let Some(root) = std::env::var_os("CRA357_DISPOSABLE_ROOT") {
        let root = std::path::PathBuf::from(root).canonicalize()?;
        assert!(
            home_path.starts_with(&root),
            "fixture home escaped disposable root"
        );
    }
    let base_path = home_path.join("config.toml");
    let selected_path = home_path.join("work.config.toml");
    std::fs::write(&base_path, PROFILE_TEST_BASE)?;
    std::fs::write(&selected_path, "")?;
    let program = codex_utils_cargo_bin::cargo_bin("codex")?;
    let mut server = start_profile_app_server(&home_path, &program).await?;

    let id = server
        .send_request(
            "skills/config/write",
            Some(json!({
                "name": "fixture-skill", "enabled": false,
            })),
        )
        .await?;
    let response: serde_json::Value =
        timeout(PROFILE_RPC_TIMEOUT, server.read_response(id)).await??;
    assert_eq!(response, json!({"effectiveEnabled": false}));

    let selected: toml::Value = toml::from_str(&std::fs::read_to_string(&selected_path)?)?;
    assert_eq!(
        serde_json::to_value(selected)?,
        json!({
            "skills": {"config": [{"name": "fixture-skill", "enabled": false}]},
        })
    );
    assert_eq!(std::fs::read_to_string(&base_path)?, PROFILE_TEST_BASE);
    assert!(
        timeout(PROFILE_RPC_TIMEOUT, server.shutdown_gracefully())
            .await??
            .success()
    );
    Ok(())
}

fn codex_command(codex_home: &Path) -> Result<assert_cmd::Command> {
    let mut cmd = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    cmd.env("CODEX_HOME", codex_home);
    Ok(cmd)
}

#[test]
fn strict_config_rejects_unknown_config_fields_for_app_server() -> Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        r#"
foo = "bar"
"#,
    )?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args(["app-server", "--strict-config", "--listen", "off"])
        .assert()
        .failure()
        .stderr(contains("unknown configuration field"));

    Ok(())
}

#[test]
fn agents_accept_interactive_configuration_overrides() -> Result<()> {
    let codex_home = TempDir::new()?;

    for args in [
        ["-c", "features.multi_agent_mode=true", "agents"].as_slice(),
        ["--enable", "multi_agent_mode", "agents"].as_slice(),
        ["--yolo", "agents"].as_slice(),
        ["--search", "agents"].as_slice(),
        ["--model", "gpt-5", "agents"].as_slice(),
        ["--approve-for-me", "agents"].as_slice(),
        ["--cd", ".", "agents"].as_slice(),
    ] {
        let mut cmd = codex_command(codex_home.path())?;
        cmd.env("TERM", "xterm-256color").args(args);
        #[cfg(not(any(unix, windows)))]
        cmd.args(["--remote", "ws://127.0.0.1:4512"]);

        cmd.assert()
            .failure()
            .stderr(contains("stdin is not a terminal"));
    }

    Ok(())
}

#[test]
fn agents_reject_inputs_that_cannot_be_applied() -> Result<()> {
    let codex_home = TempDir::new()?;

    for (args, expected_error) in [
        (
            ["--image=image.png", "agents"].as_slice(),
            "does not accept an initial prompt or images",
        ),
        (
            ["--oss", "agents", "--remote", "ws://127.0.0.1:4512"].as_slice(),
            "cannot apply local provider or additional-directory overrides",
        ),
        (
            [
                "--add-dir",
                ".",
                "agents",
                "--remote",
                "ws://127.0.0.1:4512",
            ]
            .as_slice(),
            "cannot apply local provider or additional-directory overrides",
        ),
        (
            [
                "-c",
                "sandbox_workspace_write.writable_roots=[\"../shared\"]",
                "agents",
                "--remote",
                "ws://127.0.0.1:4512",
            ]
            .as_slice(),
            "cannot apply local provider or additional-directory overrides",
        ),
    ] {
        let mut cmd = codex_command(codex_home.path())?;
        cmd.args(args)
            .assert()
            .failure()
            .stderr(contains(expected_error));
    }

    Ok(())
}

#[test]
fn app_server_emits_json_info_events() -> Result<()> {
    let codex_home = TempDir::new()?;
    let event = app_server_json_shutdown_event("codex", &["app-server"], codex_home.path())?;

    assert_eq!(
        event,
        json!({
            "level": "INFO",
            "fields": {
                "message": "processor task exited",
                "exit_reason": "stdio_connection_closed",
                "remaining_connection_count": 0,
                "shutdown_forced": false,
            },
            "target": "codex_app_server",
        })
    );

    Ok(())
}
