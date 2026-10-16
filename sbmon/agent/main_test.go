package main

import (
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestParseSingboxConfigDoesNotExposeSecrets(t *testing.T) {
	directory := t.TempDir()
	filename := filepath.Join(directory, "config.json")
	content := `{
  "inbounds": [
    {
      "type": "shadowsocks",
      "tag": "ss2022",
      "listen_port": 30001,
      "password": "main-secret",
      "users": [{"name":"demo-user-a","password":"user-secret"}]
    },
    {
      "type": "vless",
      "tag": "vless-in",
      "listen_port": 30002,
      "users": [{"name":"demo-user-b","uuid":"secret-uuid"}]
    }
  ]
}`
	if err := os.WriteFile(filename, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	inbounds, err := parseSingboxConfig(filename)
	if err != nil {
		t.Fatal(err)
	}
	if len(inbounds) != 2 {
		t.Fatalf("expected 2 inbounds, got %d", len(inbounds))
	}
	if inbounds[0].Port != 30001 || inbounds[0].Users[0] != "demo-user-a" {
		t.Fatalf("unexpected inbound: %#v", inbounds[0])
	}
	if inbounds[1].Port != 30002 || inbounds[1].Users[0] != "demo-user-b" {
		t.Fatalf("unexpected inbound: %#v", inbounds[1])
	}
	encoded, err := json.Marshal(inbounds)
	if err != nil {
		t.Fatal(err)
	}
	for _, secret := range []string{"main-secret", "user-secret", "secret-uuid"} {
		if strings.Contains(string(encoded), secret) {
			t.Fatal("secret leaked in report")
		}
	}
}

func TestPostJSONRefusesRedirect(t *testing.T) {
	target := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		t.Error("redirect target must never receive credentials")
	}))
	defer target.Close()
	source := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Redirect(w, r, target.URL, http.StatusTemporaryRedirect)
	}))
	defer source.Close()
	if err := postJSON(source.URL, "test-token", map[string]string{"registration_token": "test"}, nil); err == nil {
		t.Fatal("expected redirect rejection")
	}
}

func TestAdvanceDirectionHandlesCounterReset(t *testing.T) {
	value := DirectionState{LastRaw: 1000, Total: 5000, Seen: true}
	value = advanceDirection(value, 1300, 10)
	if value.Total != 5300 || value.Rate != 30 {
		t.Fatalf("normal delta failed: %#v", value)
	}
	value = advanceDirection(value, 200, 10)
	if value.Total != 5500 || value.Rate != 20 {
		t.Fatalf("reset delta failed: %#v", value)
	}
}

func TestUpdateTrafficCombinesTCPAndUDP(t *testing.T) {
	state := AgentState{Ports: map[string]PortState{}, LastSample: time.Unix(100, 0)}
	raw := map[string]uint64{
		"sbm:upload:tcp:30001":   1000,
		"sbm:upload:udp:30001":   500,
		"sbm:download:tcp:30001": 2000,
		"sbm:download:udp:30001": 250,
	}
	updateTraffic(&state, raw, time.Unix(110, 0))
	value := state.Ports["30001"]
	if value.Upload.Total != 1500 || value.Download.Total != 2250 {
		t.Fatalf("unexpected totals: %#v", value)
	}
}

func TestHostOnly(t *testing.T) {
	cases := map[string]string{
		"www.cloudflare.com:443": "www.cloudflare.com",
		"1.2.3.4:80":             "1.2.3.4",
		"[2001:db8::1]:443":      "2001:db8::1",
		"example.org":            "example.org",
	}
	for in, want := range cases {
		if got := hostOnly(in); got != want {
			t.Errorf("hostOnly(%q)=%q, want %q", in, got, want)
		}
	}
}

func TestIsHealthCheck(t *testing.T) {
	probes := []string{"www.cloudflare.com", "cp.cloudflare.com", "www.gstatic.com", "captive.apple.com", "www.msftconnecttest.com"}
	for _, h := range probes {
		if !isHealthCheck(h) {
			t.Errorf("isHealthCheck(%q) = false, want true", h)
		}
	}
	real := []string{"www.example.org", "i.instagram.com", "youtube.com", "example.com"}
	for _, h := range real {
		if isHealthCheck(h) {
			t.Errorf("isHealthCheck(%q) = true, want false", h)
		}
	}
}

func TestInboundConnRe(t *testing.T) {
	// "to" line with [user] and destination
	line := "INFO[1] [2 0ms] inbound/shadowsocks[ss2022_alice]: [alice] inbound connection to www.example.org:443"
	m := inboundConnRe.FindStringSubmatch(line)
	if m == nil || m[1] != "2" || m[2] != "ss2022_alice" || m[3] != "" || m[4] != "to" || m[5] != "www.example.org:443" {
		t.Fatalf("to-line parse failed: %#v", m)
	}
	// "from" line with source
	line2 := "INFO[1] [2 0ms] inbound/shadowsocks[ss2022_alice]: inbound connection from 1.2.3.4:5555"
	m2 := inboundConnRe.FindStringSubmatch(line2)
	if m2 == nil || m2[4] != "from" || m2[5] != "1.2.3.4:5555" {
		t.Fatalf("from-line parse failed: %#v", m2)
	}
	// UDP and multiplex lines carry a kind
	line4 := "INFO[1] [7 0ms] inbound/shadowsocks[ss2022_alice]: [alice] inbound packet connection to dns.example.org:53"
	if m4 := inboundConnRe.FindStringSubmatch(line4); m4 == nil || m4[3] != "packet " {
		t.Fatalf("packet-line parse failed: %#v", m4)
	}
	// error/scan line must NOT match
	line3 := "ERROR[1] [2 0ms] inbound/shadowsocks[ss2022_alice]: process connection from 1.2.3.4:5: shadowsocks: bad header"
	if inboundConnRe.FindStringSubmatch(line3) != nil {
		t.Fatalf("scan/error line unexpectedly matched")
	}
}

func TestParseProcTCP(t *testing.T) {
	// Real rows: IPv4-mapped peer on a dual-stack listener, a plain IPv4 row,
	// a LISTEN row and a row on a port nobody watches.
	data := `  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   6: 0000000000000000FFFF0000097100CB:9C41 0000000000000000FFFF0000057100CB:D6CA 01 00000000:00000000 02:00002D59 00000000   987        0 7250890 2 0 20 4 31 10 -1
   7: 00000000000000000000000000000000:9C41 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000   987        0 1 1 0 100 0 0 10 0
   8: 0100007F:9074 0200A8C0:1F90 01 00000000:00000000 00:00000000 00000000     0        0 2 1 0 20 4 31 10 -1
   9: 0100007F:0016 0200A8C0:1F91 01 00000000:00000000 00:00000000 00000000     0        0 3 1 0 20 4 31 10 -1`
	out := map[int]map[string]bool{}
	parseProcTCP(data, map[int]bool{40001: true, 36980: true}, out)
	if !out[40001]["203.0.113.5:54986"] {
		t.Fatalf("mapped IPv4 peer missing: %#v", out)
	}
	if !out[36980]["192.168.0.2:8080"] {
		t.Fatalf("IPv4 peer missing: %#v", out)
	}
	if len(out) != 2 || len(out[40001]) != 1 {
		t.Fatalf("listen row or unwatched port leaked in: %#v", out)
	}
}

func TestTrackerLiveAndVisits(t *testing.T) {
	tr := newConnTracker()
	feed := func(raw string, record bool) {
		ts, id, tag, kind, dir, peer, ok := parseJournalLine(raw)
		if !ok {
			t.Fatalf("line not parsed: %s", raw)
		}
		tr.handleLine(ts, id, tag, kind, dir, peer, record)
	}
	prefix := "1791765813.680464 host sing-box[1]: \x1b[36mINFO\x1b[0m[1] "
	feed(prefix+"[11 0ms] inbound/shadowsocks[ss2022_zoe]: inbound connection from 203.0.113.9:1000", true)
	feed(prefix+"[11 0ms] inbound/shadowsocks[ss2022_zoe]: [ZOE] inbound connection to video.example.org:443", true)
	feed(prefix+"[12 0ms] inbound/shadowsocks[ss2022_zoe]: inbound connection from 203.0.113.9:1001", true)
	feed(prefix+"[12 0ms] inbound/shadowsocks[ss2022_zoe]: [ZOE] inbound connection to www.apple.com:80", true)
	feed(prefix+"[13 0ms] inbound/shadowsocks[ss2022_zoe]: inbound connection from 198.51.100.4:7", true) // scanner, never routed

	acc := tr.acc["ss2022_zoe"]
	if acc == nil || acc.count != 1 || acc.probes != 1 || len(acc.visits) != 1 {
		t.Fatalf("unexpected accumulator: %#v", acc)
	}
	if acc.visits[0].Host != "video.example.org" || acc.visits[0].T != 1791765813 {
		t.Fatalf("visit should carry the logged time and host: %#v", acc.visits[0])
	}

	portOf := map[string]int{"ss2022_zoe": 36980}
	all := map[int]map[string]bool{36980: {"203.0.113.9:1000": true, "203.0.113.9:1001": true, "198.51.100.4:7": true}}
	if live := tr.liveCounts(all, portOf, 1791765815); live["ss2022_zoe"] != 1 {
		t.Fatalf("only the real destination counts as live, got %d", live["ss2022_zoe"])
	}
	// The video connection closes: nothing is live, and it is forgotten after the grace.
	if live := tr.liveCounts(map[int]map[string]bool{}, portOf, 1791765900); live["ss2022_zoe"] != 0 || len(tr.conns) != 0 {
		t.Fatalf("closed connections must stop counting: live=%v conns=%d", live, len(tr.conns))
	}

	// record=false (restart backfill) learns the connection without reporting it.
	tr.commit()
	feed(prefix+"[21 0ms] inbound/shadowsocks[ss2022_zoe]: inbound connection from 203.0.113.9:2000", false)
	feed(prefix+"[21 0ms] inbound/shadowsocks[ss2022_zoe]: [ZOE] inbound connection to video.example.org:443", false)
	if len(tr.acc) != 0 {
		t.Fatalf("backfill must not report activity: %#v", tr.acc)
	}
	if live := tr.liveCounts(map[int]map[string]bool{36980: {"203.0.113.9:2000": true}}, portOf, 1791765900); live["ss2022_zoe"] != 1 {
		t.Fatalf("backfilled connection should be live")
	}
}

// End-to-end over a real socket: a fake journalctl supplies the log lines, the
// kernel socket table supplies liveness.
func TestCollectInboundActivityLiveSocket(t *testing.T) {
	if _, err := os.ReadFile("/proc/net/tcp"); err != nil {
		t.Skip("no /proc/net/tcp")
	}
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Skip(err)
	}
	defer listener.Close()
	port := listener.Addr().(*net.TCPAddr).Port
	client, err := net.Dial("tcp", listener.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	accepted, err := listener.Accept()
	if err != nil {
		t.Fatal(err)
	}
	peer := client.LocalAddr().String()

	dir := t.TempDir()
	now := time.Now().Unix()
	journal := fmt.Sprintf("%d.000001 host sing-box[1]: INFO[1] [77 0ms] inbound/shadowsocks[ss2022_zoe]: inbound connection from %s\n"+
		"%d.000001 host sing-box[1]: INFO[1] [77 0ms] inbound/shadowsocks[ss2022_zoe]: [ZOE] inbound connection to video.example.org:443\n", now, peer, now)
	if err := os.WriteFile(filepath.Join(dir, "journal.txt"), []byte(journal), 0o600); err != nil {
		t.Fatal(err)
	}
	script := "#!/bin/sh\ncase \"$*\" in *--after-cursor*) exit 0;; esac\ncat " + filepath.Join(dir, "journal.txt") + "\necho '-- cursor: c1'\n"
	if err := os.WriteFile(filepath.Join(dir, "journalctl"), []byte(script), 0o700); err != nil {
		t.Fatal(err)
	}
	t.Setenv("PATH", dir+":"+os.Getenv("PATH"))
	tracker = newConnTracker()
	defer func() { tracker = newConnTracker() }()
	inbounds := []Inbound{{Port: port, Type: "shadowsocks", Tag: "ss2022_zoe"}}

	first, _ := collectInboundActivity("sing-box", now-5, inbounds)
	if len(first) != 1 || first[0].ConnCount != 1 || first[0].Live != 1 || !first[0].LiveTracked || len(first[0].Visits) != 1 {
		t.Fatalf("first scan: %#v", first)
	}
	tracker.commit()

	// No new log lines, socket still open: reported as live with nothing new.
	second, _ := collectInboundActivity("sing-box", now, inbounds)
	if len(second) != 1 || second[0].ConnCount != 0 || second[0].Live != 1 || len(second[0].Visits) != 0 {
		t.Fatalf("held connection: %#v", second)
	}

	// Both ends close: nothing left to report.
	client.Close()
	accepted.Close()
	deadline := time.Now().Add(3 * time.Second)
	for {
		third, _ := collectInboundActivity("sing-box", now, inbounds)
		if len(third) == 0 {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("closed connection still reported: %#v", third)
		}
		time.Sleep(50 * time.Millisecond)
	}
}

func TestQUICInboundNotLiveTracked(t *testing.T) {
	dir := t.TempDir()
	file := filepath.Join(dir, "config.json")
	cfg := `{"inbounds":[{"type":"hysteria2","tag":"hy2_alice","listen_port":36981,"users":[{"name":"alice"}]},{"type":"shadowsocks","tag":"ss_alice","listen_port":36980}]}`
	if err := os.WriteFile(file, []byte(cfg), 0o600); err != nil {
		t.Fatal(err)
	}
	inbounds, err := parseSingboxConfig(file)
	if err != nil {
		t.Fatal(err)
	}
	mux := map[string]bool{}
	for _, inbound := range inbounds {
		mux[inbound.Tag] = inbound.Mux
	}
	if len(inbounds) != 2 || !mux["hy2_alice"] || mux["ss_alice"] {
		t.Fatalf("unexpected inbounds: %+v", inbounds)
	}
	line := "INFO[1] [77 0ms] inbound/hysteria2[hy2_alice]: [alice] inbound packet connection to www.example.org:443"
	if m := inboundConnRe.FindStringSubmatch(line); m == nil || m[2] != "hy2_alice" {
		t.Fatalf("hysteria2 packet line not matched: %v", m)
	}
}

func TestParseJournalLineWithSingboxTimestamp(t *testing.T) {
	raw := "1760231917.123456 test sing-box[25718]: +0900 2026-10-12 10:18:37 \x1b[36mINFO\x1b[0m [467747243 0ms] inbound/hysteria2[hy2_alice]: [alice] inbound connection to example.org:443"
	ts, id, tag, kind, dir, peer, ok := parseJournalLine(raw)
	if !ok || ts != 1760231917 || id != "467747243" || tag != "hy2_alice" || kind != "" || dir != "to" || peer != "example.org:443" {
		t.Fatalf("unexpected parse: %v %v %v %q %v %v %v", ts, id, tag, kind, dir, peer, ok)
	}
	plain := "1760231917.123456 host sing-box[1]: INFO[0001] [5 0ms] inbound/shadowsocks[ss_alice]: inbound connection from 1.2.3.4:5555"
	if _, _, tag, _, dir, _, ok := parseJournalLine(plain); !ok || tag != "ss_alice" || dir != "from" {
		t.Fatalf("plain line no longer parsed")
	}
}
