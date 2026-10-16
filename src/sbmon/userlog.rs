//! Per-user connection log: every connection the sb-agents report (sb-agent
//! 0.6.0 and later send `conns`), kept as one JSON line per connection under
//! `userlog/<node>/<YYYY-MM-DD>.jsonl`, for a few days only.
//!
//! The panel asks for one user on one day; user names compare without regard
//! to case, since the same person is often `ZOE` on one server and `zoe` on
//! another.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader};
use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::{json, Value};

use super::engine::{clean_name, clean_text, Config};

/// More connections than this in one report are ignored; sb-agent sends at most 3000.
const MAX_PER_REPORT: usize = 5000;
/// Connections a query returns; the totals still count every match.
pub const MAX_ITEMS: usize = 3000;
const MAX_HOSTS: usize = 150;

/// Stands for "connections no user name was found for" in a query.
pub const UNKNOWN_USER: &str = "?";

/// The lines a report adds, by day. A connection's own time places it, unless
/// the agent's clock is far off; then the receipt time does.
pub fn lines_from_report(body: &Value, config: &Config, now: DateTime<Utc>) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let stamp = |t: i64| {
        let at = Utc.timestamp_opt(t, 0).single().filter(|at| {
            let skew = (*at - now).num_seconds();
            (-2 * 86_400..=120).contains(&skew)
        });
        at.unwrap_or(now)
    };
    if let Some(conns) = body.get("conns").and_then(Value::as_array) {
        for item in conns.iter().take(MAX_PER_REPORT) {
            let field = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or_default();
            let dst = clean_text(field("dst"), 300).trim().to_owned();
            let tag = clean_text(field("tag"), 100).trim().to_owned();
            if dst.is_empty() || tag.is_empty() {
                continue;
            }
            let at = stamp(item.get("t").and_then(Value::as_i64).unwrap_or(0));
            let net = match field("net") {
                n @ ("tcp" | "udp" | "mux") => n,
                _ => "tcp",
            };
            let mut line = json!({
                "t": at.timestamp(),
                "tag": tag,
                "user": clean_name(field("user")),
                "src": clean_text(field("src"), 80).trim(),
                "dst": dst,
                "net": net,
            });
            if item.get("probe").and_then(Value::as_bool) == Some(true) {
                line["probe"] = json!(true);
            }
            out.entry(config.date_key(at)).or_default().push(line.to_string());
        }
    }
    let dropped = body.get("conns_dropped").and_then(Value::as_u64).unwrap_or(0);
    if dropped > 0 {
        out.entry(config.date_key(now))
            .or_default()
            .push(json!({"t": now.timestamp(), "dropped": dropped}).to_string());
    }
    out
}

pub struct Filter {
    /// Lowercased; empty means every user, [`UNKNOWN_USER`] the unnamed ones.
    pub user: String,
    /// Lowercased substring of the destination or source; empty for all.
    pub search: String,
    /// Include client latency checks (to cloudflare.com, gstatic.com and the like).
    pub probes: bool,
}

/// `host` of `host:port`, `[v6]:port` or a bare host.
fn host_of(dst: &str) -> &str {
    if let Some(rest) = dst.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match dst.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => dst,
    }
}

/// Reads one day of the log for `nodes` (id, name) and answers the panel.
pub fn query(dir: &Path, nodes: &[(String, String)], day: &str, filter: &Filter) -> Value {
    let mut items: Vec<Value> = Vec::new();
    let mut total = 0u64;
    let mut probes = 0u64;
    let mut dropped = 0u64;
    let mut hosts: HashMap<String, (u64, i64)> = HashMap::new();
    let mut per_node: Vec<Value> = Vec::new();
    // Every user seen that day, lowercased → (name as written, connections).
    let mut users: BTreeMap<String, (String, u64)> = BTreeMap::new();
    for (id, name) in nodes {
        let Ok(file) = std::fs::File::open(dir.join(id).join(format!("{day}.jsonl"))) else { continue };
        let mut count = 0u64;
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(entry) = serde_json::from_str::<Value>(&line) else { continue };
            if !entry.is_object() {
                continue;
            }
            if let Some(n) = entry.get("dropped").and_then(Value::as_u64) {
                dropped += n;
                continue;
            }
            let s = |k: &str| entry.get(k).and_then(Value::as_str).unwrap_or_default();
            let user = s("user");
            let key = user.to_lowercase();
            let seen = users.entry(key.clone()).or_insert_with(|| (user.to_owned(), 0));
            seen.1 += 1;
            let wanted = match filter.user.as_str() {
                "" => true,
                UNKNOWN_USER => key.is_empty(),
                other => key == other,
            };
            if !wanted {
                continue;
            }
            let probe = entry.get("probe").and_then(Value::as_bool) == Some(true);
            if probe {
                probes += 1;
                if !filter.probes {
                    continue;
                }
            }
            let dst = s("dst");
            if !filter.search.is_empty()
                && !dst.to_lowercase().contains(&filter.search)
                && !s("src").to_lowercase().contains(&filter.search)
            {
                continue;
            }
            total += 1;
            count += 1;
            let t = entry.get("t").and_then(Value::as_i64).unwrap_or(0);
            let host = hosts.entry(host_of(dst).to_owned()).or_default();
            host.0 += 1;
            host.1 = host.1.max(t);
            let mut item = entry.clone();
            item["nodeId"] = json!(id);
            item["node"] = json!(name);
            items.push(item);
        }
        if count > 0 {
            per_node.push(json!({"id": id, "name": name, "count": count}));
        }
    }
    let t = |v: &Value| v.get("t").and_then(Value::as_i64).unwrap_or(0);
    items.sort_by_key(|v| std::cmp::Reverse(t(v)));
    let truncated = items.len() > MAX_ITEMS;
    items.truncate(MAX_ITEMS);
    let mut hosts: Vec<(String, (u64, i64))> = hosts.into_iter().collect();
    hosts.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(a.0.cmp(&b.0)));
    let hosts: Vec<Value> = hosts
        .into_iter()
        .take(MAX_HOSTS)
        .map(|(host, (count, last))| json!({"host": host, "count": count, "last": last}))
        .collect();
    let users: Vec<Value> =
        users.into_values().map(|(name, count)| json!({"user": name, "count": count})).collect();
    json!({
        "ok": true,
        "date": day,
        "total": total,
        "probes": probes,
        "dropped": dropped,
        "truncated": truncated,
        "items": items,
        "hosts": hosts,
        "nodes": per_node,
        "users": users,
    })
}

/// Deletes day files older than `keep_days` and directories of nodes not in `known`.
pub fn cleanup(dir: &Path, known: &std::collections::HashSet<String>, cutoff_day: &str) {
    let Ok(nodes) = std::fs::read_dir(dir) else { return };
    for node in nodes.flatten() {
        if !known.contains(node.file_name().to_string_lossy().as_ref()) {
            let _ = std::fs::remove_dir_all(node.path());
            continue;
        }
        let Ok(files) = std::fs::read_dir(node.path()) else { continue };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            if let Some(day) = name.strip_suffix(".jsonl") {
                if day.len() == 10 && day < cutoff_day {
                    let _ = std::fs::remove_file(file.path());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 16, 6, 0, 0).unwrap()
    }

    #[test]
    fn report_lines_are_cleaned_and_dated() {
        let config = Config::default(); // UTC+8
        let body = json!({
            "conns": [
                {"t": now().timestamp() - 60, "tag": "ss", "user": "ZOE", "src": "1.2.3.4:5", "dst": "a.example:443", "net": "tcp"},
                {"t": 1, "tag": "hy2", "user": "", "dst": "1.1.1.1:53", "net": "weird", "probe": true},
                {"t": now().timestamp(), "tag": "", "dst": "x:1"},
                {"t": now().timestamp(), "tag": "ss", "dst": ""},
                // 16:05 UTC is the next day at UTC+8.
                {"t": now().timestamp() + 10 * 3600 + 300, "tag": "ss", "dst": "late:1"},
            ],
            "conns_dropped": 7,
        });
        let lines = lines_from_report(&body, &config, now());
        let today: Vec<Value> =
            lines["2026-10-16"].iter().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(today.len(), 4, "{today:?}");
        assert_eq!(today[0]["user"], "ZOE");
        // A wild clock is booked at receipt time; an unknown net is tcp.
        assert_eq!(today[1]["t"], now().timestamp());
        assert_eq!(today[1]["net"], "tcp");
        assert_eq!(today[1]["probe"], true);
        // Ten hours ahead is more skew than allowed too.
        assert_eq!(today[2]["dst"], "late:1");
        assert_eq!(today[3]["dropped"], 7);
        assert!(!lines.contains_key("2026-10-17"));
    }

    #[test]
    fn query_matches_user_ignoring_case() {
        let dir = std::env::temp_dir().join(format!("userlog-test-{}", super::super::engine::random_hex(6)));
        let write = |node: &str, lines: &[Value]| {
            std::fs::create_dir_all(dir.join(node)).unwrap();
            let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
            std::fs::write(dir.join(node).join("2026-10-16.jsonl"), text).unwrap();
        };
        write(
            "n1",
            &[
                json!({"t": 10, "tag": "ss", "user": "ZOE", "src": "1.2.3.4:5", "dst": "video.example.org:443", "net": "tcp"}),
                json!({"t": 20, "tag": "ss", "user": "Kim", "dst": "kim.example:443", "net": "tcp"}),
                json!({"t": 30, "tag": "ss", "user": "ZOE", "dst": "www.gstatic.com:443", "net": "tcp", "probe": true}),
                json!({"t": 31, "dropped": 4}),
                Value::String("not json".into()),
            ],
        );
        write(
            "n2",
            &[
                json!({"t": 40, "tag": "hy2_zoe", "user": "zoe", "dst": "video.example.org:443", "net": "udp"}),
                json!({"t": 50, "tag": "hy2", "user": "", "dst": "[2001:db8::1]:443", "net": "udp"}),
            ],
        );
        let nodes = vec![("n1".to_owned(), "Bitsflow JP".to_owned()), ("n2".to_owned(), "EQ JP".to_owned())];
        let filter =
            |user: &str, search: &str, probes| Filter { user: user.into(), search: search.into(), probes };

        let r = query(&dir, &nodes, "2026-10-16", &filter("zoe", "", false));
        assert_eq!(r["total"], 2);
        assert_eq!(r["probes"], 1);
        assert_eq!(r["dropped"], 4);
        assert_eq!(r["items"][0]["node"], "EQ JP", "newest first");
        assert_eq!(r["hosts"][0], json!({"host": "video.example.org", "count": 2, "last": 40}));
        assert_eq!(r["nodes"].as_array().unwrap().len(), 2);
        // The users seen that day, one entry per name ignoring case.
        let users: Vec<&str> =
            r["users"].as_array().unwrap().iter().map(|u| u["user"].as_str().unwrap()).collect();
        assert_eq!(users, ["", "Kim", "ZOE"]);

        assert_eq!(query(&dir, &nodes, "2026-10-16", &filter("zoe", "", true))["total"], 3);
        assert_eq!(query(&dir, &nodes, "2026-10-16", &filter("zoe", "1.2.3", false))["total"], 1);
        let unknown = query(&dir, &nodes, "2026-10-16", &filter(UNKNOWN_USER, "", false));
        assert_eq!(unknown["total"], 1);
        assert_eq!(unknown["hosts"][0]["host"], "2001:db8::1");
        assert_eq!(query(&dir, &nodes, "2026-10-15", &filter("", "", false))["total"], 0);
        assert_eq!(query(&dir, &nodes[..1], "2026-10-16", &filter("", "", true))["total"], 3);

        let known = ["n1".to_owned()].into_iter().collect();
        std::fs::write(dir.join("n1").join("2026-10-01.jsonl"), "").unwrap();
        cleanup(&dir, &known, "2026-10-10");
        assert!(!dir.join("n2").exists());
        assert!(!dir.join("n1").join("2026-10-01.jsonl").exists());
        assert!(dir.join("n1").join("2026-10-16.jsonl").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn host_of_strips_ports() {
        assert_eq!(host_of("a.example:443"), "a.example");
        assert_eq!(host_of("[2001:db8::1]:53"), "2001:db8::1");
        assert_eq!(host_of("2001:db8::1"), "2001:db8::1");
        assert_eq!(host_of("plain"), "plain");
    }
}
