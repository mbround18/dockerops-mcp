//! End-to-end: the tools as an MCP client sees them, against a `docker` that is a script.

mod harness;

use harness::{Fake, stamped};

#[test]
fn every_tool_is_advertised_with_the_parameters_that_replace_a_pipeline() {
    let fake = Fake::new();
    let mut server = fake.server();
    let tools = server.tools();
    let listed = tools["tools"].as_array().expect("tools");
    let mut names: Vec<&str> = listed
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "compose",
            "config",
            "events",
            "exec",
            "images",
            "inspect",
            "logs",
            "passthrough",
            "ps",
            "run",
            "stats",
            "usage",
        ]
    );

    let logs = listed.iter().find(|tool| tool["name"] == "logs").unwrap();
    let schema = logs["inputSchema"].to_string();
    for parameter in [
        "targets",
        "tail",
        "since",
        "until",
        "grep",
        "exclude",
        "level",
        "context",
        "collapse",
        "max_lines",
        "max_bytes",
        "project",
        "files",
    ] {
        assert!(
            schema.contains(parameter),
            "`{parameter}` missing from the logs schema: {schema}"
        );
    }
    // There is no follow mode, and advertising one would be a promise the server cannot
    // keep: a streaming tool call never returns.
    assert!(!schema.contains("follow"), "{schema}");
    assert!(
        server.instructions.contains("Prefer these tools over"),
        "the server tells the client what it is for: {}",
        server.instructions
    );
}

#[test]
fn one_logs_call_replaces_the_pipeline_it_was_written_for() {
    // `docker compose logs api 2>&1 | grep -iE 'error|panic' | tail -40`, as one call.
    let fake = Fake::new().reply(
        "logs",
        &format!(
            "{}{}{}",
            stamped("12:04:10", "GET /healthz 200"),
            stamped("12:04:11", "ERROR failed to reach db: connection refused"),
            stamped("12:04:12", "GET /healthz 200"),
        ),
    );
    let mut server = fake.server();

    let result = server.call(
        "logs",
        serde_json::json!({"targets": ["api"], "level": "error"}),
    );

    assert!(!result.is_error(), "{}", result.text());
    assert_eq!(
        result.text(),
        "\
#1 api 1/3 (2 filtered)
ERROR failed to reach db: connection refused
[1 target, 1 line, 2 filtered out]
"
    );
    assert_eq!(result.structured()["lines_shown"], 1);
    assert_eq!(result.structured()["targets"][0]["filtered_out"], 2);
    // Timestamps were fetched to merge the streams and then paid for by nobody.
    assert!(
        fake.calls()[0].contains("--timestamps"),
        "{:?}",
        fake.calls()
    );
}

#[test]
fn both_halves_of_a_response_carry_the_rendered_text() {
    // Hosts disagree about which half they display: Claude Code renders
    // `structuredContent` and drops the text content, others do the reverse. A tool that
    // fills in only one answers half its callers with metadata and no content. That
    // regression shipped once in the sibling server; this is why it cannot ship here.
    let fake = Fake::new()
        .reply("ps", "{\"Names\":\"api\",\"State\":\"running\",\"Status\":\"Up 3 hours\",\"Image\":\"api:1\",\"Ports\":\"\"}\n")
        .reply("logs", &stamped("12:04:11", "ready"));
    let mut server = fake.server();

    for (tool, arguments) in [
        ("ps", serde_json::json!({})),
        ("logs", serde_json::json!({"targets": ["api"]})),
    ] {
        let result = server.call(tool, arguments);
        assert!(!result.is_error(), "{tool}: {}", result.text());
        let carried = result.structured()["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{tool} left its text out of the structured copy"));
        assert_eq!(
            carried,
            result.text(),
            "{tool}: the two halves of the response disagree"
        );
        assert!(!carried.is_empty(), "{tool} rendered nothing");
    }
}

#[test]
fn several_targets_and_one_missing_container_are_still_one_answer() {
    let fake = Fake::new()
        .reply(
            "logs --timestamps --tail 80 api",
            &stamped("12:04:11", "ready"),
        )
        .fail(
            "logs --timestamps --tail 80 gone",
            "Error response from daemon: No such container: gone\n",
        );
    let mut server = fake.server();

    let result = server.call("logs", serde_json::json!({"targets": ["api", "gone"]}));

    assert!(!result.is_error(), "{}", result.text());
    assert_eq!(
        result.text(),
        "\
#1 api 1/1
ready
#2 gone — No such container: gone
[2 targets, 1 line]
"
    );
}

#[test]
fn the_two_container_streams_come_back_in_order() {
    let fake = Fake::new().reply_both(
        "logs",
        &format!(
            "{}{}",
            stamped("12:04:11", "starting"),
            stamped("12:04:14", "still here")
        ),
        &stamped("12:04:12", "cannot reach db"),
    );
    let mut server = fake.server();

    let result = server.call("logs", serde_json::json!({"targets": ["api"]}));
    assert_eq!(
        result.text(),
        "\
#1 api 3/3
starting
cannot reach db
still here
[1 target, 3 lines]
"
    );
    assert_eq!(result.structured()["targets"][0]["stderr_lines"], 1);
}

#[test]
fn a_compose_project_routes_through_compose_without_being_asked_twice() {
    let fake = Fake::new().reply(
        "compose",
        "{\"Name\":\"ens-wine-1\",\"Service\":\"wine\",\"State\":\"running\",\"Status\":\"Up 3 hours (unhealthy)\",\"Image\":\"ens:1.4\",\"Publishers\":[]}\n",
    );
    let mut server = fake.server();

    let result = server.call(
        "ps",
        serde_json::json!({"project": "ens", "files": ["docker-compose.yml"]}),
    );

    assert!(!result.is_error(), "{}", result.text());
    assert_eq!(
        result.text(),
        "\
ens-wine-1  up 3h unhealthy  ens:1.4  -
[1 container: 1 up]
"
    );
    let call = &fake.calls()[0];
    assert!(
        call.contains("compose -f docker-compose.yml -p ens ps"),
        "{call}"
    );
}

#[test]
fn a_bad_request_comes_back_as_a_readable_tool_error() {
    let fake = Fake::new();
    let mut server = fake.server();

    let result = server.call("logs", serde_json::json!({}));
    assert!(result.is_error());
    assert!(
        result.text().contains("name at least one container"),
        "{}",
        result.text()
    );

    let result = server.call(
        "logs",
        serde_json::json!({"targets": ["api"], "level": "spicy"}),
    );
    assert!(result.is_error());
    assert!(
        result.text().contains("must be `error` or `warn`"),
        "{}",
        result.text()
    );

    // Nothing was run on the way to either refusal.
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}

#[test]
fn a_crash_loop_costs_one_line_and_a_count() {
    let repeated = stamped(
        "12:04:11",
        "steamcmd: failed to install app 1203420 (0x202)",
    )
    .repeat(6);
    let fake = Fake::new().reply("logs", &repeated);
    let mut server = fake.server();

    let result = server.call("logs", serde_json::json!({"targets": ["wine"]}));
    assert_eq!(
        result.text(),
        "\
#1 wine 1/6
steamcmd: failed to install app 1203420 (0x202) ×6
[1 target, 1 line, 5 repeats folded]
"
    );
}

#[test]
fn an_inspect_preset_turns_a_document_into_the_six_lines_meant_by_it() {
    let document = serde_json::json!({
        "Name": "/ens-wine-1",
        "RestartCount": 3,
        "State": {
            "Status": "running",
            "ExitCode": 0,
            "OOMKilled": false,
            "Health": {"Status": "unhealthy", "FailingStreak": 7, "Log": []},
        },
    })
    .to_string();
    let fake = Fake::new().reply("inspect", &format!("{document}\n"));
    let mut server = fake.server();

    let result = server.call(
        "inspect",
        serde_json::json!({"targets": ["ens-wine-1"], "preset": "health"}),
    );

    assert!(!result.is_error(), "{}", result.text());
    assert_eq!(
        result.text(),
        "\
#1 ens-wine-1
State.Status: running
State.Health.Status: unhealthy
State.Health.FailingStreak: 7
State.ExitCode: 0
State.OOMKilled: false
RestartCount: 3
[not present: State.Health.Log.*.Output, State.Error]
[1 target, 6 fields]
"
    );
    // A Go template was not written, and no JSON reached the caller.
    let call = &fake.calls()[0];
    assert!(
        call.contains("inspect --format {{json .}} ens-wine-1"),
        "{call}"
    );
}

#[test]
fn a_disk_question_is_answered_with_the_totals_line_docker_omits() {
    let fake = Fake::new().reply(
        "system df",
        "{\"Type\":\"Images\",\"TotalCount\":14,\"Active\":3,\"Size\":\"24.6GB\",\"Reclaimable\":\"18.2GB (73%)\"}\n",
    );
    let mut server = fake.server();

    let result = server.call("usage", serde_json::json!({}));
    assert_eq!(
        result.text(),
        "Images  24G  3 active  18G reclaimable\n[24G total, 18G reclaimable (73%)]\n"
    );
    assert_eq!(result.structured()["reclaimable"], 19_542_101_196u64);
}

#[test]
fn a_preset_that_does_not_exist_names_the_ones_that_do() {
    let fake = Fake::new();
    let mut server = fake.server();
    let result = server.call(
        "inspect",
        serde_json::json!({"targets": ["api"], "preset": "vibes"}),
    );
    assert!(result.is_error());
    assert!(
        result.text().contains("no preset `vibes`"),
        "{}",
        result.text()
    );
    assert!(fake.calls().is_empty());
}

#[test]
fn the_lifecycle_verbs_that_throw_things_away_need_confirming() {
    let fake = Fake::new().reply("compose -p ens down", "");
    let mut server = fake.server();

    let refused = server.call(
        "compose",
        serde_json::json!({"project": "ens", "verb": "down"}),
    );
    assert!(refused.is_error(), "{}", refused.text());
    assert!(
        refused.text().contains("confirm: true"),
        "{}",
        refused.text()
    );
    assert!(fake.calls().is_empty(), "nothing ran: {:?}", fake.calls());

    let done = server.call(
        "compose",
        serde_json::json!({"project": "ens", "verb": "down", "confirm": true}),
    );
    assert!(!done.is_error(), "{}", done.text());
    assert_eq!(fake.calls().len(), 1, "{:?}", fake.calls());
}

#[test]
fn a_command_that_would_never_return_is_refused_with_the_bounded_form_named() {
    let fake = Fake::new();
    let mut server = fake.server();
    for (args, expected) in [
        (serde_json::json!(["attach", "api"]), "logs"),
        (serde_json::json!(["logs", "-f", "api"]), "--follow"),
        (serde_json::json!(["stats"]), "--no-stream"),
        (serde_json::json!(["compose", "up"]), "--detach"),
    ] {
        let result = server.call("passthrough", serde_json::json!({"args": args}));
        assert!(result.is_error(), "{args} ran: {}", result.text());
        assert!(
            result.text().contains(expected),
            "{args} refusal did not name {expected}: {}",
            result.text()
        );
    }
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}

#[test]
fn the_exec_pipeline_is_one_call_and_keeps_the_exit_status() {
    // `docker exec ens-wine-1 enshrouded install | grep -v '^Update state'`, as one call.
    let mut stdout = String::new();
    for percent in 0..120 {
        stdout.push_str(&format!(
            "Update state (0x61) downloading, progress: {percent}.00\n"
        ));
    }
    stdout.push_str("Error! App '2278520' state is 0x202 after update job.\n");
    let fake = Fake::new().reply("exec", &stdout);
    let mut server = fake.server();

    let result = server.call(
        "exec",
        serde_json::json!({
            "target": "ens-wine-1",
            "argv": ["enshrouded", "install"],
            "exclude": "^Update state",
        }),
    );
    assert!(!result.is_error(), "{}", result.text());
    assert_eq!(
        result.text(),
        "Error! App '2278520' state is 0x202 after update job.\n\
         [1 line of 121 lines, 120 filtered, exit 0]\n"
    );
    // Both halves, as every tool here must: hosts disagree over which one they render.
    assert_eq!(
        result.structured()["text"].as_str().unwrap(),
        result.text(),
        "the rendered text must ship in the structured half too"
    );
}

#[test]
fn run_defaults_to_rm_for_one_off_containers() {
    let fake = Fake::new().reply("run --rm hello-world", "Hello from Docker!\n");
    let mut server = fake.server();

    let result = server.call("run", serde_json::json!({"image": "hello-world"}));
    assert!(!result.is_error(), "{}", result.text());
    assert_eq!(
        result.text(),
        "Hello from Docker!\n[run hello-world, 1 line, exit 0]\n"
    );
    assert!(fake.calls()[0].contains("run --rm hello-world"));
}
