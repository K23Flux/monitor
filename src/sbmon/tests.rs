//! Ported from the plugin's tests/plugin.test.js, against the engine with an
//! explicit clock instead of a patched `Date`.

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Value};

use super::engine::{parse_watch_rules, Config, Engine, Rule, State};

fn config(rules: &str) -> Config {
    let rules = rules.to_owned();
    Config::from_lookup(move |k| match k {
        "watch_rules" => Some(rules.clone()),
        "tg_bot_token" => Some("1:abc".into()),
        "tg_chat_id" => Some("42".into()),
        _ => None,
    })
}

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 12, 1, 0, 0).unwrap()
}

/// Registers one node and returns (engine, node_id, token).
fn registered(rules: &str) -> (Engine, String, String) {
    let mut e = Engine::new(State::default(), config(rules));
    let reg = e.create_registration("demo-node-01", t0()).unwrap();
    let token = reg["token"].as_str().unwrap().to_owned();
    let agent = e
        .register(
            &json!({"registration_token": token, "name": "demo-node-01", "hostname": "h", "arch": "amd64"}),
            t0(),
        )
        .unwrap();
    // A one-time key.
    assert_eq!(e.register(&json!({"registration_token": token}), t0()).unwrap_err().0, 401);
    (e, agent["node_id"].as_str().unwrap().to_owned(), agent["agent_token"].as_str().unwrap().to_owned())
}

fn report(node: &str, extra: Value) -> Value {
    let mut body = json!({
        "node_id": node,
        "hostname": "h",
        "agent_version": "0.3.2",
        "singbox": {"running": true, "version": "1.12.0"},
        "inbounds": [
            {"port": 36980, "type": "shadowsocks", "tag": "ss2022_alice", "users": ["alice"]},
            {"port": 8443, "type": "vless", "tag": "vless_lf", "users": ["LF"]}
        ],
        "counters": [],
    });
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    body
}

fn activity(conn: i64, live: i64, visits: &[(&str, DateTime<Utc>)]) -> Value {
    json!([{
        "tag": "ss2022_alice",
        "conn_count": conn,
        "live": live,
        "live_tracked": true,
        "last_source": "1.2.3.4:5555",
        "visits": visits.iter().map(|(h, t)| json!({"host": h, "t": t.timestamp()})).collect::<Vec<_>>(),
    }])
}

fn row(e: &Engine, now: DateTime<Utc>) -> Value {
    e.admin_watch(now)["watched"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["tag"] == "ss2022_alice")
        .cloned()
        .unwrap()
}

fn events(e: &Engine, kind: &str) -> Vec<super::engine::UserEvent> {
    e.state.user_events.iter().filter(|ev| ev.kind == kind).cloned().collect()
}

#[test]
fn watch_rules_parse_like_the_plugin() {
    assert_eq!(
        parse_watch_rules("ss2022_alice=alice, hy2_alice=alice\nbare,ss2022_alice=dup"),
        vec![
            Rule { tag: "ss2022_alice".into(), label: "alice".into() },
            Rule { tag: "hy2_alice".into(), label: "alice".into() },
            Rule { tag: "bare".into(), label: "bare".into() },
        ]
    );
}

#[test]
fn reports_need_the_node_token_and_book_traffic_by_day() {
    let (mut e, node, token) = registered("");
    assert_eq!(e.report("wrong", &report(&node, json!({})), t0()).unwrap_err().0, 401);
    assert_eq!(e.report(&token, &report("zz", json!({})), t0()).unwrap_err().0, 401);
    let counters = |up: u64, down: u64| json!({"counters": [{"port": 36980, "upload_total": up, "download_total": down, "upload_rate": 10, "download_rate": 20}]});
    e.report(&token, &report(&node, counters(1000, 2000)), t0()).unwrap();
    e.report(&token, &report(&node, counters(1500, 3100)), t0() + Duration::seconds(5)).unwrap();
    let state = e.admin_state(t0() + Duration::seconds(6));
    let port =
        state["nodes"][0]["ports"].as_array().unwrap().iter().find(|p| p["port"] == 36980).unwrap().clone();
    assert_eq!(port["displayName"], "alice");
    assert_eq!(port["today"]["total"], 1600.0);
    assert_eq!(port["downloadRate"], 20.0);
    // A counter reset (agent or kernel restart) books the new total, not a negative.
    e.report(&token, &report(&node, counters(100, 0)), t0() + Duration::seconds(10)).unwrap();
    assert_eq!(e.admin_state(t0())["nodes"][0]["ports"][1]["today"]["total"], 1700.0);
    // Offline after the threshold, and its rates drop to zero.
    let later = e.admin_state(t0() + Duration::minutes(5));
    assert_eq!(later["nodes"][0]["online"], false);
    assert_eq!(later["nodes"][0]["ports"][1]["downloadRate"], 0.0);
    let text = later.to_string();
    assert!(!text.contains(&token) && !text.contains(&e.state.nodes[&node].agent_token_hash));
}

#[test]
fn odd_reports_do_not_crash() {
    let (mut e, node, token) = registered("ss2022_alice=alice");
    let body = report(
        &node,
        json!({"inbounds": [null, 1, {"port": 70000}], "counters": [null, {"port": "x"}], "activity": [null, {"tag": "ss2022_alice"}, {"conn_count": 1}], "events": [null, "", "warn!"]}),
    );
    e.report(&token, &body, t0()).unwrap();
    assert_eq!(e.state.events.len(), 1);
    // The same message is not repeated.
    e.report(&token, &body, t0()).unwrap();
    assert_eq!(e.state.events.len(), 1);
}

#[test]
fn a_real_session_goes_online_and_offline_with_its_visits() {
    let (mut e, node, token) = registered("ss2022_alice=alice");
    let start = t0();

    // A lone short request (one connection, already closed) is never announced.
    e.report(
        &token,
        &report(&node, json!({"activity": activity(1, 0, &[("ocsp.example.net", start)])})),
        start,
    )
    .unwrap();
    assert_eq!(row(&e, start)["online"], false);
    // The agent keeps reporting, so the closed blip is judged rather than held.
    e.report(&token, &report(&node, json!({})), start + Duration::seconds(115)).unwrap();
    e.check_offline(start + Duration::seconds(120));
    assert!(!e.state.watch.values().next().unwrap().pending, "the blip is dropped");
    assert!(e.state.user_events.is_empty(), "an unconfirmed blip produces no event");
    assert!(e.state.pending.is_empty());

    // One connection that stays open is confirmed after online_confirm_seconds.
    let s = start + Duration::minutes(10);
    e.report(&token, &report(&node, json!({"activity": activity(1, 1, &[("video.example.org", s)])})), s)
        .unwrap();
    assert_eq!(row(&e, s)["online"], false, "pending until confirmed");
    let t = s + Duration::seconds(20);
    e.report(&token, &report(&node, json!({"activity": activity(0, 1, &[])})), t).unwrap();
    assert_eq!(row(&e, t)["online"], true);
    let online = events(&e, "online");
    assert_eq!(online.len(), 1);
    assert_eq!(online[0].time, super::engine::iso(s), "online time is the first connection's log time");
    assert_eq!(e.state.pending.len(), 1);
    assert!(e.state.pending[0].text.contains("alice 上线"), "{}", e.state.pending[0].text);

    // A long stream over one open connection never goes offline.
    let mut now = t;
    for _ in 0..360 {
        now += Duration::seconds(5);
        e.report(&token, &report(&node, json!({"activity": activity(0, 1, &[])})), now).unwrap();
        e.check_offline(now);
    }
    assert_eq!(row(&e, now)["online"], true, "open connection keeps the session online");
    // Two more connections, one to a known host.
    e.report(
        &token,
        &report(
            &node,
            json!({"activity": activity(2, 2, &[("video.example.org", now), ("cdn.example.org", now)])}),
        ),
        now,
    )
    .unwrap();
    let last_active = now;

    // Everything closes: the agent stops listing the tag.
    now += Duration::seconds(5);
    e.report(&token, &report(&node, json!({})), now).unwrap();
    assert_eq!(row(&e, now)["live"], 0, "the panel stops counting closed connections at once");
    for _ in 0..10 {
        now += Duration::seconds(5);
        e.report(&token, &report(&node, json!({})), now).unwrap();
        e.check_offline(now);
    }
    assert_eq!(row(&e, now)["online"], true, "inside the offline grace");
    for _ in 0..4 {
        now += Duration::seconds(5);
        e.report(&token, &report(&node, json!({})), now).unwrap();
        e.check_offline(now);
    }
    assert_eq!(row(&e, now)["online"], false, "offline once nothing is open past the grace");
    let offline = events(&e, "offline");
    assert_eq!(offline.len(), 1);
    let ev = &offline[0];
    assert_eq!(ev.time, super::engine::iso(last_active), "offline at the last active moment");
    assert_eq!(ev.session_start.as_deref(), Some(super::engine::iso(s).as_str()));
    assert_eq!(ev.session_conn, Some(3));
    let visits: Vec<(String, i64)> =
        ev.visits.as_ref().unwrap().iter().map(|v| (v.host.clone(), v.count)).collect();
    assert_eq!(visits, vec![("video.example.org".into(), 2), ("cdn.example.org".into(), 1)]);

    let text = e.state.pending.last().unwrap().text.clone();
    assert!(text.contains("alice 下线"));
    assert!(text.contains("上线  09:10:00") && text.contains("下线  09:40:20"), "{text}");
    assert!(text.contains("时长  30m"), "{text}");
    assert!(text.contains("09:10:00  video.example.org ×2（至 09:40:20）"), "{text}");
    assert!(text.contains("09:40:20  cdn.example.org"), "{text}");
}

#[test]
fn a_silent_agent_holds_the_session_then_closes_it_with_a_note() {
    let (mut e, node, token) = registered("ss2022_alice=alice");
    let s = t0();
    e.report(&token, &report(&node, json!({"activity": activity(2, 2, &[("a.example", s)])})), s).unwrap();
    assert_eq!(row(&e, s)["online"], true, "two connections confirm at once");
    e.check_offline(s + Duration::minutes(5));
    assert_eq!(row(&e, s + Duration::minutes(5))["online"], true, "silent agent: session is held");
    e.check_offline(s + Duration::minutes(11));
    let offline = events(&e, "offline");
    assert_eq!(offline.len(), 1);
    assert_eq!(offline[0].note.as_deref(), Some("探针失联，下线时间按最后一次上报计"));
}

#[test]
fn untracked_inbounds_use_the_idle_rule() {
    let (mut e, node, token) = registered("ss2022_alice=alice");
    let s = t0();
    let body = json!([{"tag": "ss2022_alice", "conn_count": 1, "live": 0, "live_tracked": false, "domains": ["x.example"]}]);
    e.report(&token, &report(&node, json!({"activity": body})), s).unwrap();
    assert_eq!(row(&e, s)["online"], true, "first real connection is enough without live tracking");
    let mut now = s;
    while now < s + Duration::seconds(290) {
        now += Duration::seconds(5);
        e.report(&token, &report(&node, json!({})), now).unwrap();
        e.check_offline(now);
    }
    assert_eq!(row(&e, now)["online"], true);
    now += Duration::seconds(30);
    e.report(&token, &report(&node, json!({})), now).unwrap();
    e.check_offline(now);
    assert_eq!(row(&e, now)["online"], false);
}

#[test]
fn long_sessions_split_into_several_messages() {
    let (mut e, node, token) = registered("ss2022_alice=alice");
    let s = t0();
    let hosts: Vec<String> =
        (0..150).map(|i| format!("host-{i:03}.some-long-domain-name.example.com")).collect();
    let visits: Vec<(&str, DateTime<Utc>)> = hosts.iter().map(|h| (h.as_str(), s)).collect();
    e.report(&token, &report(&node, json!({"activity": activity(150, 0, &visits)})), s).unwrap();
    e.report(&token, &report(&node, json!({})), s + Duration::seconds(115)).unwrap();
    e.check_offline(s + Duration::minutes(2));
    let offline: Vec<&str> =
        e.state.pending.iter().map(|p| p.text.as_str()).filter(|t| !t.contains("上线</b>")).collect();
    assert!(offline.len() >= 2, "{} messages", offline.len());
    assert!(offline.iter().all(|t| t.chars().count() <= super::engine::TG_CHUNK + 80));
    assert!(offline[1].starts_with("<b>alice 访问记录（续）</b>"));
}

#[test]
fn the_plugins_state_file_loads_and_round_trips() {
    // Shape written by komari-singbox-monitor 0.3.2 (trimmed).
    let raw = r#"{"schema":2,"nodes":{"0123456789abcdef0123456789abcdef":{"id":"0123456789abcdef0123456789abcdef","name":"node-jp","hostname":"jp","arch":"amd64","agentVersion":"0.3.2","agentTokenHash":"ab","createdAt":"2026-10-12T01:00:00.000Z","lastSeen":"2026-10-12T01:20:31.123Z","singbox":{"running":true,"version":"sing-box version 1.12.0"},"inbounds":[{"port":36981,"type":"hysteria2","tag":"hy2_alice","users":["alice"]}],"aliases":{"36981":"alice-hy2"},"current":{"36981":{"port":36981,"uploadTotal":10,"downloadTotal":20,"uploadRate":1,"downloadRate":2}},"lastTotals":{"36981":{"upload":10,"download":20}},"lastActivity":{"at":"2026-10-12T01:20:31.123Z","items":[]}}},"registrations":{"ff":{"name":"x","expiresAt":"2026-10-12T01:10:00.000Z","used":true,"createdAt":"2026-10-12T01:00:00.000Z","nodeId":"0123456789abcdef0123456789abcdef"}},"daily":{"2026-10-12":{"0123456789abcdef0123456789abcdef":{"36981":{"upload":123,"download":456}}}},"events":[],"watch":{"0123456789abcdef0123456789abcdef:hy2_alice":{"nodeId":"0123456789abcdef0123456789abcdef","tag":"hy2_alice","online":false,"sessionStart":"2026-10-12T01:20:00.000Z","sessionConn":3,"lastActive":"2026-10-12T01:20:31.123Z","lastSource":"127.0.0.1","connCount":9,"domains":["example.org"],"label":"alice","liveTracked":false,"live":0,"pending":false,"visits":[{"host":"example.org","first":"2026-10-12T01:20:00.000Z","last":"2026-10-12T01:20:00.000Z","count":1}],"visitsDropped":0}},"userEvents":[{"id":"1","kind":"online","label":"alice","tag":"hy2_alice","nodeId":"0123456789abcdef0123456789abcdef","nodeName":"node-jp","source":"127.0.0.1","domains":["example.org"],"time":"2026-10-12T01:20:31.000Z"}],"pending":[],"updatedAt":"2026-10-12T01:20:31.200Z"}"#;
    let state: State = serde_json::from_str(raw).unwrap();
    let node = &state.nodes["0123456789abcdef0123456789abcdef"];
    assert_eq!(node.name, "node-jp");
    assert_eq!(node.agent_token_hash, "ab");
    assert_eq!(state.daily["2026-10-12"]["0123456789abcdef0123456789abcdef"]["36981"].download, 456.0);
    assert_eq!(state.watch.len(), 1);
    let again: State = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
    assert_eq!(again, state);
    // A key the hub does not know is not an error, and a missing one has its default.
    let sparse: State = serde_json::from_str(r#"{"nodes":{},"future":1}"#).unwrap();
    assert_eq!(sparse.schema, 2);
}

#[test]
fn registration_keys_expire() {
    let mut e = Engine::new(State::default(), Config::default());
    let token = e.create_registration("n", t0()).unwrap()["token"].as_str().unwrap().to_owned();
    let refused =
        e.register(&json!({"registration_token": token}), t0() + Duration::minutes(11)).unwrap_err();
    assert_eq!(refused.0, 401);
    assert!(e.create_registration("  ", t0()).is_err());
}

#[test]
fn deleting_a_node_drops_its_traces() {
    let (mut e, node, token) = registered("ss2022_alice=alice");
    e.report(
        &token,
        &report(&node, json!({"activity": activity(2, 2, &[("a.example", t0())]), "events": ["x"]})),
        t0(),
    )
    .unwrap();
    e.delete_node(&node).unwrap();
    assert!(
        e.state.nodes.is_empty()
            && e.state.watch.is_empty()
            && e.state.user_events.is_empty()
            && e.state.events.is_empty()
    );
    assert!(e.delete_node(&node).is_err());
}

#[test]
fn an_agent_installed_with_the_hub_node_is_linked_and_reinstalls_in_place() {
    let mut e = Engine::new(State::default(), config("ss2022_alice=alice"));
    let body = json!({"registration_token": "node-token", "hostname": "jp", "arch": "amd64", "agent_version": "0.3.2"});
    let first = e.register_linked(7, "JP", &body, t0());
    let id = first["node_id"].as_str().unwrap().to_owned();
    assert_eq!(e.state.nodes[&id].hub_node_id, Some(7));
    assert_eq!(e.state.nodes[&id].name, "JP");
    // A reinstall keeps the sb node and replaces its token.
    let second = e.register_linked(7, "JP", &body, t0());
    assert_eq!(second["node_id"], first["node_id"]);
    assert_eq!(e.state.nodes.len(), 1);
    let old = first["agent_token"].as_str().unwrap();
    let new = second["agent_token"].as_str().unwrap();
    assert_eq!(e.report(old, &report(&id, json!({})), t0()).unwrap_err().0, 401);
    e.report(new, &report(&id, json!({})), t0()).unwrap();
    // Linked nodes are renamed in the node list, not here.
    assert!(e.rename_node(&id, "x").is_err());
}

#[test]
fn linked_nodes_follow_the_hubs_node_list() {
    let mut e = Engine::new(State::default(), Config::default());
    let id = e.register_linked(7, "JP", &json!({}), t0())["node_id"].as_str().unwrap().to_owned();
    let key = e.create_registration("own", t0()).unwrap()["token"].as_str().unwrap().to_owned();
    let own = e.register(&json!({"registration_token": key}), t0()).unwrap()["node_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut names = std::collections::BTreeMap::from([(7, "Tokyo".to_owned())]);
    assert!(e.sync_hub_nodes(&names));
    assert_eq!(e.state.nodes[&id].name, "Tokyo");
    assert!(!e.sync_hub_nodes(&names), "nothing left to change");
    names.clear();
    assert!(e.sync_hub_nodes(&names));
    assert!(!e.state.nodes.contains_key(&id), "deleted with its hub node");
    assert!(e.state.nodes.contains_key(&own), "a node registered on its own stays");
}

#[test]
fn an_existing_node_can_be_linked_by_hand() {
    let (mut e, node, _) = registered("");
    e.link_node(&node, Some((3, "Hytron"))).unwrap();
    assert_eq!(e.state.nodes[&node].hub_node_id, Some(3));
    assert_eq!(e.state.nodes[&node].name, "Hytron");
    let other = e.register_linked(4, "DE", &json!({}), t0())["node_id"].as_str().unwrap().to_owned();
    assert!(e.link_node(&other, Some((3, "Hytron"))).is_err(), "one sing-box per hub node");
    e.link_node(&node, None).unwrap();
    assert_eq!(e.state.nodes[&node].hub_node_id, None);
}
