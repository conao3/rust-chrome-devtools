use crate::batch::*;
use crate::cli::*;
use crate::config::*;
use crate::daemon::*;
use crate::lock::*;
use crate::router::*;
use std::collections::HashMap;
use std::env;
use std::io::BufRead;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

#[test]
fn parse_config_accepts_single_minimal_profile() {
    let config = parse_config("[[profiles]]\nname = \"default\"\n").unwrap();
    assert_eq!(config.profiles.len(), 1);
    let profile = &config.profiles[0];
    assert_eq!(profile.name, "default");
    assert_eq!(
        profile.user_data_dir,
        "~/.config/chrome-devtools/profiles/default"
    );
}

#[test]
fn parse_config_accepts_explicit_user_data_dir() {
    let config =
        parse_config("[[profiles]]\nname = \"work\"\nuser_data_dir = \"/tmp/work\"\n").unwrap();
    assert_eq!(config.profiles[0].user_data_dir, "/tmp/work");
}

#[test]
fn parse_config_ignores_deprecated_port_field() {
    let config = parse_config("[[profiles]]\nname = \"legacy\"\nport = 9222\n").unwrap();
    assert_eq!(config.profiles[0].name, "legacy");
}

#[test]
fn parse_config_accepts_multiple_profiles_with_comments() {
    let toml = "
        # leading comment
        [[profiles]]
        name = \"a\"

        [[profiles]]
        name = \"b\"
    ";
    let config = parse_config(toml).unwrap();
    let names: Vec<_> = config
        .profiles
        .iter()
        .map(|profile| &profile.name)
        .collect();
    assert_eq!(names, vec!["a", "b"]);
}

#[test]
fn parse_config_rejects_empty_config() {
    assert!(parse_config("").is_err());
}

#[test]
fn parse_config_rejects_duplicate_profile_names() {
    let toml = "[[profiles]]\nname = \"a\"\n[[profiles]]\nname = \"a\"\n";
    let error = parse_config(toml).unwrap_err();
    assert!(error.contains("duplicate profile name"));
}

#[test]
fn parse_config_rejects_field_outside_profile_block() {
    assert!(parse_config("name = \"x\"\n").is_err());
}

#[test]
fn parse_config_rejects_unknown_key() {
    let toml = "[[profiles]]\nname = \"a\"\nweird = \"x\"\n";
    assert!(parse_config(toml).is_err());
}

#[test]
fn parse_toml_string_handles_escapes() {
    assert_eq!(
        parse_toml_string("\"line\\nbreak\"", 1).unwrap(),
        "line\nbreak"
    );
    assert_eq!(parse_toml_string("\"a\\\\b\"", 1).unwrap(), "a\\b");
    assert_eq!(parse_toml_string("\"a\\\"b\"", 1).unwrap(), "a\"b");
}

#[test]
fn parse_toml_string_rejects_unquoted() {
    assert!(parse_toml_string("foo", 1).is_err());
}

#[test]
fn parse_toml_string_rejects_unknown_escape() {
    assert!(parse_toml_string("\"\\q\"", 1).is_err());
}

#[test]
fn safe_lock_name_keeps_allowed_characters() {
    assert_eq!(safe_lock_name("abc_def-123"), "abc_def-123");
}

#[test]
fn safe_lock_name_replaces_disallowed_characters() {
    assert_eq!(safe_lock_name("a/b c.d"), "a_b_c_d");
}

#[test]
fn parse_lock_pid_extracts_pid_line() {
    assert_eq!(parse_lock_pid("pid=4242\nprofile=default\n"), Some(4242));
    assert_eq!(parse_lock_pid("profile=default\npid=7\n"), Some(7));
}

#[test]
fn parse_lock_pid_returns_none_without_pid() {
    assert_eq!(parse_lock_pid("profile=default\n"), None);
}

#[test]
fn heavy_mcp_tools_get_longer_timeout() {
    assert!(is_heavy_mcp_tool("take_snapshot"));
    assert!(is_heavy_mcp_tool("take_screenshot"));
    assert!(!is_heavy_mcp_tool("click"));
    let heavy = request_timeout_for_line(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"take_snapshot","arguments":{}}}"#,
    );
    let normal = request_timeout_for_line(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"click","arguments":{}}}"#,
    );
    assert!(heavy >= MCP_HEAVY_REQUEST_TIMEOUT);
    assert_eq!(normal, mcp_request_timeout());
}

#[test]
fn json_method_starts_with_matches_notifications() {
    assert!(json_method_starts_with(
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{}}"#,
        "notifications/"
    ));
    assert!(!json_method_starts_with(
        r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
        "notifications/"
    ));
}

#[test]
fn forward_line_drops_server_notifications_from_client_stream() {
    let input = concat!(
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":9}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":10000,"result":{"content":[]}}"#,
        "\n",
    );
    let mut reader = std::io::BufReader::new(std::io::Cursor::new(input));
    let mut written: Vec<u8> = Vec::new();
    let mut next_id = 10_000;
    let mut abandoned = HashMap::new();
    let lines = forward_line(
        &mut written,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"click","arguments":{}}}"#,
        &mut next_id,
        &mut abandoned,
    )
    .unwrap();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("\"id\":1"));
}

#[test]
fn read_mcp_response_line_keeps_partial_line_across_would_block() {
    struct ChunkedReader {
        chunks: Vec<Vec<u8>>,
        blocked: bool,
    }
    impl std::io::Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.blocked {
                self.blocked = true;
                return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock));
            }
            self.blocked = false;
            let Some(chunk) = self.chunks.first_mut() else {
                return Ok(0);
            };
            let n = chunk.len().min(buf.len());
            buf[..n].copy_from_slice(&chunk[..n]);
            chunk.drain(..n);
            if chunk.is_empty() {
                self.chunks.remove(0);
            }
            Ok(n)
        }
    }
    let payload = r#"{"jsonrpc":"2.0","id":42,"result":{}}"#;
    let (head, tail) = payload.split_at(20);
    let reader = ChunkedReader {
        chunks: vec![head.as_bytes().to_vec(), format!("{tail}\n").into_bytes()],
        blocked: false,
    };
    let mut buf_reader = std::io::BufReader::new(reader);
    let mut written: Vec<u8> = Vec::new();
    let line = read_mcp_response_line_until(
        &mut written,
        &mut buf_reader,
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(line, payload);
}

#[test]
fn extract_jsonrpc_id_ignores_nested_id() {
    assert_eq!(
        extract_jsonrpc_id(
            r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"id":5}}"#
        ),
        None
    );
    assert_eq!(
        extract_jsonrpc_id(
            r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"text":"\"id\":9"}]}}"#
        ),
        Some(3)
    );
}

#[test]
fn json_has_method_ignores_method_in_string_values() {
    assert!(!json_has_method(
        r#"{"jsonrpc":"2.0","id":1,"result":{"text":"\"method\":\"roots/list\""}}"#,
        "roots/list"
    ));
}

#[test]
fn extract_jsonrpc_id_reads_numeric_id() {
    assert_eq!(
        extract_jsonrpc_id(r#"{"jsonrpc":"2.0","id":42,"result":{}}"#),
        Some(42)
    );
    assert_eq!(extract_jsonrpc_id(r#"{ "id" : 7 }"#), Some(7));
}

#[test]
fn extract_jsonrpc_id_value_reads_string_id() {
    assert_eq!(
        extract_jsonrpc_id_value(r#"{"jsonrpc":"2.0","id":"abc","result":{}}"#),
        Some(serde_json::json!("abc"))
    );
}

#[test]
fn rewrite_jsonrpc_id_replaces_id_value() {
    let output = rewrite_jsonrpc_id(
        r#"{"jsonrpc":"2.0","id":"abc","result":{}}"#,
        serde_json::json!(9),
    )
    .unwrap();
    let value: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["id"], 9);
}

#[test]
fn extract_jsonrpc_id_returns_none_when_missing() {
    assert_eq!(extract_jsonrpc_id(r#"{"result":{}}"#), None);
}

#[test]
fn json_has_method_detects_method() {
    assert!(json_has_method(
        r#"{"jsonrpc":"2.0","method":"initialize","params":{}}"#,
        "initialize"
    ));
    assert!(!json_has_method(
        r#"{"jsonrpc":"2.0","method":"tools/list"}"#,
        "initialize"
    ));
}

#[test]
fn parse_session_arg_accepts_session_assignment() {
    assert_eq!(parse_session_arg("session=abc").unwrap(), "abc");
    assert_eq!(parse_session_arg("session=foo other=bar").unwrap(), "foo");
}

#[test]
fn parse_session_arg_rejects_missing_session() {
    assert!(parse_session_arg("").is_err());
    assert!(parse_session_arg("other=bar").is_err());
    assert!(parse_session_arg("session=").is_err());
}

#[test]
fn format_session_line_renders_all_fields() {
    let now = SystemTime::now();
    let state = SessionState {
        id: "sess-test".to_string(),
        created_at: now,
        last_used_at: now,
        owned: true,
        page_id: Some(7),
        page_created_by_daemon: true,
        page_url: Some("about:blank".to_string()),
        target_id: Some("TARGET1".to_string()),
        snapshot_epoch: 0,
        uid_bindings: std::collections::HashMap::new(),
    };
    let line = format_session_line(&state);
    assert!(line.starts_with("session=sess-test "));
    assert!(line.contains(" owned=true"));
    assert!(line.contains(" created="));
    assert!(line.contains(" last_used="));
    assert!(line.contains(" target_id=TARGET1"));
}

#[test]
fn set_page_drops_target_id_when_the_page_changes() {
    let mut registry = SessionRegistry::default();
    let session = registry.create();
    registry
        .set_page(&session.id, 1, true, Some("about:blank".to_string()))
        .unwrap();
    registry.set_target_id(&session.id, Some("TARGET1".to_string()));
    assert_eq!(
        registry.sessions[&session.id].target_id.as_deref(),
        Some("TARGET1")
    );

    // 同じ page への再記録では target id を保つ (goto でページ内遷移しただけのケース)。
    registry
        .set_page(
            &session.id,
            1,
            true,
            Some("https://example.com/".to_string()),
        )
        .unwrap();
    assert_eq!(
        registry.sessions[&session.id].target_id.as_deref(),
        Some("TARGET1")
    );

    // 別 page に移ったら target id は無効になる。
    registry.set_page(&session.id, 2, false, None).unwrap();
    assert_eq!(registry.sessions[&session.id].target_id, None);
}

#[test]
fn clear_page_drops_target_id() {
    let mut registry = SessionRegistry::default();
    let session = registry.create();
    registry
        .set_page(&session.id, 1, true, Some("about:blank".to_string()))
        .unwrap();
    registry.set_target_id(&session.id, Some("TARGET1".to_string()));
    registry.clear_page(&session.id);
    assert_eq!(registry.sessions[&session.id].target_id, None);
}

#[test]
fn allowed_input_path_requires_an_existing_file_under_home_or_temp() {
    let dir = std::env::temp_dir().join(format!("cdt-input-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("resume.pdf");
    std::fs::write(&file, b"%PDF-1.7\n").unwrap();

    let resolved = crate::cdp::allowed_input_path(&file).unwrap();
    assert!(resolved.is_absolute());

    assert!(crate::cdp::allowed_input_path(std::path::Path::new("resume.pdf")).is_err());
    assert!(crate::cdp::allowed_input_path(&dir).is_err());
    assert!(crate::cdp::allowed_input_path(std::path::Path::new("/etc/hosts")).is_err());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn generate_session_id_produces_unique_ids() {
    let mut ids = std::collections::HashSet::new();
    for _ in 0..100 {
        assert!(ids.insert(generate_session_id()));
    }
}

#[test]
fn session_registry_create_and_list() {
    let mut registry = SessionRegistry::default();
    let first = registry.create();
    let second = registry.create();
    assert_ne!(first.id, second.id);
    let listed = registry.list();
    assert_eq!(listed.len(), 2);
    assert!(listed[0].created_at <= listed[1].created_at);
}

#[test]
fn session_registry_close_removes_known_and_errors_on_unknown() {
    let mut registry = SessionRegistry::default();
    let state = registry.create();
    assert!(registry.close(&state.id).is_ok());
    assert!(registry.list().is_empty());
    let error = registry.close("sess-missing").unwrap_err();
    assert!(error.contains("unknown session"));
}

#[test]
fn session_registry_close_rejects_owned_session() {
    let mut registry = SessionRegistry::default();
    let state = registry.create();
    registry.bind(&state.id).unwrap();
    let error = registry.close(&state.id).unwrap_err();
    assert!(error.contains("session in use"));
}

#[test]
fn session_registry_bind_marks_owned_and_rejects_second_bind() {
    let mut registry = SessionRegistry::default();
    let state = registry.create();
    assert!(registry.bind(&state.id).is_ok());
    assert!(
        registry
            .sessions
            .get(&state.id)
            .map(|session| session.owned)
            .unwrap_or(false)
    );
    let error = registry.bind(&state.id).unwrap_err();
    assert!(error.contains("session in use"));
}

#[test]
fn session_registry_bind_allows_another_session_to_bind() {
    let mut registry = SessionRegistry::default();
    let first = registry.create();
    let second = registry.create();
    registry.bind(&first.id).unwrap();
    registry.bind(&second.id).unwrap();
    assert!(registry.sessions.get(&first.id).unwrap().owned);
    assert!(registry.sessions.get(&second.id).unwrap().owned);
}

#[test]
fn session_registry_unbind_clears_owned() {
    let mut registry = SessionRegistry::default();
    let state = registry.create();
    registry.bind(&state.id).unwrap();
    registry.unbind(&state.id);
    let session = registry.sessions.get(&state.id).unwrap();
    assert!(!session.owned);
}

#[test]
fn session_registry_touch_updates_last_used_at() {
    let mut registry = SessionRegistry::default();
    let state = registry.create();
    let before = registry.sessions.get(&state.id).unwrap().last_used_at;
    std::thread::sleep(Duration::from_millis(5));
    registry.touch(&state.id);
    let after = registry.sessions.get(&state.id).unwrap().last_used_at;
    assert!(after > before);
}

#[test]
fn session_registry_reap_expired_drops_only_expired_unowned_sessions() {
    let mut registry = SessionRegistry::default();
    let fresh = registry.create();
    let expired_idle = registry.create();
    let expired_owned = registry.create();
    let stale = SystemTime::now() - SESSION_IDLE_TTL - Duration::from_secs(60);
    registry
        .sessions
        .get_mut(&expired_idle.id)
        .unwrap()
        .last_used_at = stale;
    let owned = registry.sessions.get_mut(&expired_owned.id).unwrap();
    owned.last_used_at = stale;
    owned.owned = true;
    registry.reap_expired(SESSION_IDLE_TTL);
    let remaining: Vec<_> = registry
        .list()
        .into_iter()
        .map(|session| session.id)
        .collect();
    assert!(remaining.contains(&fresh.id));
    assert!(remaining.contains(&expired_owned.id));
    assert!(!remaining.contains(&expired_idle.id));
}

#[test]
fn resolve_refs_substitutes_ref_with_previous_result() {
    let results = vec![serde_json::json!({
        "label": "snap",
        "result": {"content": [{"text": "hello"}]}
    })];
    let template = serde_json::json!({
        "value": {"$ref": "snap.result.content.0.text"}
    });
    let resolved = resolve_refs(template, &results).unwrap();
    assert_eq!(resolved, serde_json::json!({"value": "hello"}));
}

#[test]
fn resolve_refs_returns_null_for_unknown_label() {
    let results: Vec<serde_json::Value> = vec![];
    let template = serde_json::json!({"value": {"$ref": "missing.x"}});
    let resolved = resolve_refs(template, &results).unwrap();
    assert_eq!(resolved, serde_json::json!({"value": null}));
}

#[test]
fn resolve_refs_passes_through_non_refs() {
    let template = serde_json::json!({"a": 1, "b": [2, 3]});
    let resolved = resolve_refs(template.clone(), &[]).unwrap();
    assert_eq!(resolved, template);
}

#[test]
fn wants_help_detects_help_flags() {
    assert!(wants_help(&args(&["--help"])));
    assert!(wants_help(&args(&["-h"])));
    assert!(wants_help(&args(&["help"])));
    assert!(!wants_help(&args(&["--profile", "default"])));
}

fn dummy_config() -> Config {
    Config {
        profiles: vec![Profile {
            name: "default".to_string(),
            user_data_dir: "/tmp/x".to_string(),
        }],
    }
}

#[test]
fn require_profile_returns_named_profile() {
    let profile = require_profile(&dummy_config(), &args(&["--profile", "default"])).unwrap();
    assert_eq!(profile.name, "default");
}

#[test]
fn require_profile_rejects_missing_flag_and_unknown_name() {
    assert!(require_profile(&dummy_config(), &[]).is_err());
    assert!(require_profile(&dummy_config(), &args(&["--profile", "other"])).is_err());
}

#[test]
fn require_profile_and_session_returns_both() {
    let (profile, session) = require_profile_and_session(
        &dummy_config(),
        &args(&["--profile", "default", "--session", "sess-x"]),
    )
    .unwrap();
    assert_eq!(profile.name, "default");
    assert_eq!(session, "sess-x");
}

#[test]
fn require_profile_and_session_rejects_missing_session() {
    assert!(
        require_profile_and_session(&dummy_config(), &args(&["--profile", "default"]),).is_err()
    );
}

#[test]
fn parse_batch_args_collects_all_flags() {
    let options = parse_batch_args(&args(&[
        "--profile",
        "default",
        "--session",
        "sess-1",
        "--script",
        "/tmp/x.json",
        "--output",
        "/tmp/out.json",
        "--fail-fast",
    ]))
    .unwrap();
    assert_eq!(options.profile_name, "default");
    assert_eq!(options.session_id, "sess-1");
    assert_eq!(options.script_path, "/tmp/x.json");
    assert_eq!(options.output_path.as_deref(), Some("/tmp/out.json"));
    assert!(options.fail_fast);
}

#[test]
fn parse_batch_args_rejects_missing_required_flags() {
    assert!(parse_batch_args(&args(&["--session", "sess-1", "--script", "/tmp/x.json"])).is_err());
    assert!(parse_batch_args(&args(&["--profile", "default", "--script", "/tmp/x.json"])).is_err());
    assert!(parse_batch_args(&args(&["--profile", "default", "--session", "sess-1"])).is_err());
}

#[test]
fn expand_home_resolves_tilde_paths() {
    let prev = env::var("HOME").ok();
    env::set_var("HOME", "/tmp/fakehome");
    assert_eq!(expand_home("~").unwrap(), PathBuf::from("/tmp/fakehome"));
    assert_eq!(
        expand_home("~/foo").unwrap(),
        PathBuf::from("/tmp/fakehome/foo")
    );
    assert_eq!(
        expand_home("/abs/path").unwrap(),
        PathBuf::from("/abs/path")
    );
    if let Some(value) = prev {
        env::set_var("HOME", value);
    } else {
        env::remove_var("HOME");
    }
}

#[test]
fn daemon_initialize_response_includes_id() {
    let response = daemon_initialize_response(7);
    assert!(response.contains("\"id\":7"));
    assert!(response.contains("\"protocolVersion\""));
}

#[test]
fn default_user_data_dir_for_profile_follows_prefix() {
    assert_eq!(
        default_user_data_dir_for_profile("conao3"),
        "~/.config/chrome-devtools/profiles/conao3"
    );
}

#[test]
fn find_profile_returns_named_profile_or_error() {
    let config = dummy_config();
    assert_eq!(find_profile(&config, "default").unwrap().name, "default");
    assert!(find_profile(&config, "missing").is_err());
}

#[test]
fn unix_secs_returns_zero_for_unix_epoch() {
    assert_eq!(unix_secs(UNIX_EPOCH), 0);
}

#[test]
fn chrome_extra_args_splits_on_whitespace() {
    assert_eq!(
        chrome_extra_args(Some("--disable-gpu  --lang=ja")),
        vec!["--disable-gpu".to_string(), "--lang=ja".to_string()]
    );
}

#[test]
fn chrome_extra_args_is_empty_without_value() {
    assert!(chrome_extra_args(None).is_empty());
    assert!(chrome_extra_args(Some("  ")).is_empty());
}

#[test]
fn extract_remote_debugging_port_reads_port_number() {
    let cmd = "/path/chrome --foo --remote-debugging-port=39277 --user-data-dir=/x";
    assert_eq!(extract_remote_debugging_port(cmd), Some(39277));
}

#[test]
fn extract_remote_debugging_port_returns_none_when_missing() {
    assert_eq!(extract_remote_debugging_port("/path/chrome --foo"), None);
}

#[test]
fn extract_remote_debugging_port_stops_at_next_flag() {
    let cmd = "chrome --remote-debugging-port=8080 --type=renderer";
    assert_eq!(extract_remote_debugging_port(cmd), Some(8080));
}

#[test]
fn extract_remote_debugging_port_returns_none_for_invalid_value() {
    let cmd = "chrome --remote-debugging-port=abc --user-data-dir=/x";
    assert_eq!(extract_remote_debugging_port(cmd), None);
}

#[test]
fn sanitize_outgoing_request_strips_isolated_context_from_new_page() {
    let input = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"new_page","arguments":{"url":"https://example.com","isolatedContext":"foo"}}}"#;
    let output = sanitize_outgoing_request(input);
    let value: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert!(
        value["params"]["arguments"]
            .get("isolatedContext")
            .is_none()
    );
    assert_eq!(value["params"]["arguments"]["url"], "https://example.com");
}

#[test]
fn sanitize_outgoing_request_keeps_other_tools_untouched() {
    let input = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"navigate_page","arguments":{"isolatedContext":"foo"}}}"#;
    let output = sanitize_outgoing_request(input);
    assert_eq!(output, input);
}

#[test]
fn sanitize_outgoing_request_passes_through_non_tools_call() {
    let input = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let output = sanitize_outgoing_request(input);
    assert_eq!(output, input);
}

#[test]
fn sanitize_outgoing_request_passes_through_invalid_json() {
    let input = "not json";
    assert_eq!(sanitize_outgoing_request(input), input);
}

#[test]
fn split_command_args_accepts_leading_global_flags() {
    let (positional, rest) = split_command_args(args(&["--profile", "conao3", "session", "list"]));
    assert_eq!(positional, args(&["session", "list"]));
    assert_eq!(rest, args(&["--profile", "conao3"]));
}

#[test]
fn split_command_args_accepts_flags_between_object_and_action() {
    let (positional, rest) = split_command_args(args(&[
        "mcp",
        "--profile",
        "x",
        "--session",
        "sess-1",
        "batch",
        "--script",
        "/tmp/a.json",
    ]));
    assert_eq!(positional, args(&["mcp", "batch"]));
    assert_eq!(
        rest,
        args(&[
            "--profile",
            "x",
            "--session",
            "sess-1",
            "--script",
            "/tmp/a.json"
        ])
    );
}

#[test]
fn split_command_args_keeps_plain_invocation_unchanged() {
    let (positional, rest) = split_command_args(args(&[
        "session",
        "close",
        "--profile",
        "x",
        "--session",
        "sess-1",
    ]));
    assert_eq!(positional, args(&["session", "close"]));
    assert_eq!(rest, args(&["--profile", "x", "--session", "sess-1"]));
}

#[test]
fn find_profile_error_lists_available_profiles() {
    let error = find_profile(&dummy_config(), "missing").unwrap_err();
    assert!(error.contains("unknown profile: missing"));
    assert!(error.contains("available: default"));
}

#[test]
fn roots_list_response_exposes_home_and_tmp_roots() {
    let response = roots_list_response(3);
    assert!(response.contains("\"id\":3"));
    let value: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(value["result"]["roots"][0]["name"], "home");
    assert!(
        value["result"]["roots"][0]["uri"]
            .as_str()
            .unwrap()
            .starts_with("file://")
    );
    assert_eq!(value["result"]["roots"][1]["name"], "tmp");
    assert_eq!(value["result"]["roots"][1]["uri"], "file:///tmp");
}

#[test]
fn timeout_from_env_parses_override_and_falls_back() {
    env::remove_var("CHROME_DEVTOOLS_TEST_TIMEOUT_SECS");
    assert_eq!(
        timeout_from_env("CHROME_DEVTOOLS_TEST_TIMEOUT_SECS", 42),
        Duration::from_secs(42)
    );
    env::set_var("CHROME_DEVTOOLS_TEST_TIMEOUT_SECS", "7");
    assert_eq!(
        timeout_from_env("CHROME_DEVTOOLS_TEST_TIMEOUT_SECS", 42),
        Duration::from_secs(7)
    );
    env::set_var("CHROME_DEVTOOLS_TEST_TIMEOUT_SECS", "abc");
    assert_eq!(
        timeout_from_env("CHROME_DEVTOOLS_TEST_TIMEOUT_SECS", 42),
        Duration::from_secs(42)
    );
    env::remove_var("CHROME_DEVTOOLS_TEST_TIMEOUT_SECS");
}

#[test]
fn read_mcp_response_line_answers_roots_list_inline() {
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":9,"method":"roots/list"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":5,"result":{}}"#,
        "\n",
    );
    let mut reader = std::io::Cursor::new(input);
    let mut written: Vec<u8> = Vec::new();
    let line = read_mcp_response_line(&mut written, &mut reader).unwrap();
    assert_eq!(line, r#"{"jsonrpc":"2.0","id":5,"result":{}}"#);
    let sent = String::from_utf8(written).unwrap();
    assert!(sent.contains("\"id\":9"));
    assert!(sent.contains("\"roots\""));
}

#[test]
fn read_mcp_response_line_fails_fatal_on_closed_stdout() {
    let mut reader = std::io::Cursor::new("");
    let mut written: Vec<u8> = Vec::new();
    let error = read_mcp_response_line(&mut written, &mut reader).unwrap_err();
    assert!(matches!(error, DaemonError::Fatal(_)));
}

#[test]
fn drain_pending_mcp_response_consumes_until_pending_id() {
    let input = concat!(
        r#"{"jsonrpc":"2.0","method":"notifications/progress"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":7,"result":{"content":[]}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":8,"result":{}}"#,
        "\n",
    );
    let mut reader = std::io::Cursor::new(input);
    let mut written: Vec<u8> = Vec::new();
    drain_pending_mcp_response(&mut written, &mut reader, 7, "junk").unwrap();
    let mut remaining = String::new();
    reader.read_line(&mut remaining).unwrap();
    assert!(remaining.contains("\"id\":8"));
}

#[test]
fn extract_flag_detects_and_removes_flag() {
    let (found, rest) = extract_flag(&args(&["--profile", "x", "--force"]), "--force");
    assert!(found);
    assert_eq!(rest, args(&["--profile", "x"]));
    let (found, rest) = extract_flag(&args(&["--profile", "x"]), "--force");
    assert!(!found);
    assert_eq!(rest, args(&["--profile", "x"]));
}

#[test]
fn parse_status_field_extracts_named_fields() {
    let response = "daemon=ready version=0.3.1 sessions=2 mcp_port=46071";
    assert_eq!(parse_status_field(response, "version="), Some("0.3.1"));
    assert_eq!(parse_status_field(response, "sessions="), Some("2"));
    assert_eq!(parse_status_field(response, "mcp_port="), Some("46071"));
    assert_eq!(parse_status_field("daemon=ready", "version="), None);
}

#[test]
fn process_exists_detects_live_and_dead_processes() {
    assert!(process_exists(std::process::id()));
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    assert!(!process_exists(pid));
}

#[test]
fn drain_pending_mcp_response_skips_read_when_already_consumed() {
    let mut reader = std::io::Cursor::new("");
    let mut written: Vec<u8> = Vec::new();
    drain_pending_mcp_response(&mut written, &mut reader, 7, r#"{"id":7,"result":{}}"#).unwrap();
}
