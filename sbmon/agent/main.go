package main

import (
	"bufio"
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"log"
	"net/http"
	"net/netip"
	"net/url"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"regexp"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"syscall"
	"time"
)

const (
	version        = "0.4.0"
	configFile     = "/etc/sb-agent/config.json"
	stateFile      = "/var/lib/sb-agent/state.json"
	singBoxConfig  = "/etc/sing-box/config.json"
	apiPrefix      = "/api/sbmonitor/v1"
	nftTable       = "sbmonitor"
	defaultService = "sing-box"
)

type Config struct {
	Server            string `json:"server"`
	Name              string `json:"name"`
	RegistrationToken string `json:"registration_token,omitempty"`
	NodeID            string `json:"node_id,omitempty"`
	AgentToken        string `json:"agent_token,omitempty"`
	ConfigPath        string `json:"config_path"`
	ServiceName       string `json:"service_name"`
	IntervalSeconds   int    `json:"interval_seconds"`
	// LogFile is where sing-box writes its log on machines without journald
	// (Alpine / OpenRC, where supervise-daemon redirects it to a file). Empty
	// means the systemd journal of ServiceName.
	LogFile string `json:"log_file,omitempty"`
}

type Inbound struct {
	Port  int      `json:"port"`
	Type  string   `json:"type"`
	Tag   string   `json:"tag"`
	Users []string `json:"users"`
	// Mux marks inbounds whose connections cannot be matched to kernel TCP
	// sockets: multiplex (one socket carries many streams) and QUIC-based
	// protocols such as hysteria2 (everything rides one UDP flow). For these the
	// plugin falls back to the idle-timeout rule.
	Mux bool `json:"-"`
}

type DirectionState struct {
	LastRaw uint64 `json:"last_raw"`
	Total   uint64 `json:"total"`
	Rate    uint64 `json:"-"`
	Seen    bool   `json:"seen"`
}

type PortState struct {
	Upload   DirectionState `json:"upload"`
	Download DirectionState `json:"download"`
}

type AgentState struct {
	PortsHash        string               `json:"ports_hash"`
	Ports            map[string]PortState `json:"ports"`
	LastSample       time.Time            `json:"last_sample"`
	LastLogUnix      int64                `json:"last_log_unix"`
	LastActivityUnix int64                `json:"last_activity_unix"`
}

// InboundActivity summarises real inbound connections seen for one inbound tag
// since the previous report. It is per-tag (not per-user): a tag that serves a
// single user identifies that user, which is how per-user online detection works.
// ConnCount counts only connections to real destinations; client latency/health
// checks (e.g. to www.cloudflare.com) are excluded and reported as Probes so a
// node that only gets health-checked does not look "online".
type InboundActivity struct {
	Tag            string   `json:"tag"`
	ConnCount      int      `json:"conn_count"`
	LastActiveUnix int64    `json:"last_active_unix"`
	LastSource     string   `json:"last_source,omitempty"`
	Domains        []string `json:"domains,omitempty"`
	Probes         int      `json:"probes,omitempty"`
	// Live is the number of client TCP connections that are still established
	// right now and were routed to a real (non health-check) destination.
	// LiveTracked tells the plugin whether Live is meaningful for this tag.
	Live        int     `json:"live"`
	LiveTracked bool    `json:"live_tracked"`
	Visits      []Visit `json:"visits,omitempty"`
}

// Visit is one real destination a watched inbound connected to, with the time
// sing-box logged it.
type Visit struct {
	T    int64  `json:"t"`
	Host string `json:"host"`
}

type ReportCounter struct {
	Port          int    `json:"port"`
	UploadTotal   uint64 `json:"upload_total"`
	DownloadTotal uint64 `json:"download_total"`
	UploadRate    uint64 `json:"upload_rate"`
	DownloadRate  uint64 `json:"download_rate"`
}

type RegisterRequest struct {
	RegistrationToken string `json:"registration_token"`
	Name              string `json:"name"`
	Hostname          string `json:"hostname"`
	Arch              string `json:"arch"`
	AgentVersion      string `json:"agent_version"`
}

type RegisterResponse struct {
	OK         bool   `json:"ok"`
	Error      string `json:"error"`
	NodeID     string `json:"node_id"`
	AgentToken string `json:"agent_token"`
	Name       string `json:"name"`
}

type Report struct {
	NodeID       string            `json:"node_id"`
	Timestamp    int64             `json:"timestamp"`
	Hostname     string            `json:"hostname"`
	Arch         string            `json:"arch"`
	AgentVersion string            `json:"agent_version"`
	Singbox      SingboxStatus     `json:"singbox"`
	Inbounds     []Inbound         `json:"inbounds"`
	Counters     []ReportCounter   `json:"counters"`
	Events       []string          `json:"events,omitempty"`
	Activity     []InboundActivity `json:"activity,omitempty"`
}

type SingboxStatus struct {
	Running bool   `json:"running"`
	Version string `json:"version"`
}

type apiResponse struct {
	OK    bool   `json:"ok"`
	Error string `json:"error"`
}

var httpClient = &http.Client{
	Timeout: 15 * time.Second,
	CheckRedirect: func(req *http.Request, via []*http.Request) error {
		return errors.New("redirect refused: configure the final HTTPS endpoint")
	},
}

func main() {
	log.SetFlags(log.Ldate | log.Ltime | log.Lmicroseconds)
	if len(os.Args) < 2 {
		usage()
		os.Exit(2)
	}

	switch os.Args[1] {
	case "init":
		if err := initConfig(os.Args[2:]); err != nil {
			log.Fatal(err)
		}
	case "run":
		if err := runAgent(); err != nil {
			log.Fatal(err)
		}
	case "once":
		if err := runOnce(); err != nil {
			log.Fatal(err)
		}
	case "version", "--version", "-v":
		fmt.Println(version)
	default:
		usage()
		os.Exit(2)
	}
}

func usage() {
	fmt.Println("sb-agent init --server URL --token TOKEN --name NAME")
	fmt.Println("sb-agent run")
	fmt.Println("sb-agent once")
	fmt.Println("sb-agent version")
}

func initConfig(args []string) error {
	if _, err := os.Stat(configFile); err == nil {
		return errors.New("Agent 配置已存在；升级请使用安装脚本 --update，不覆盖节点身份")
	} else if !errors.Is(err, os.ErrNotExist) {
		return err
	}
	flags := flag.NewFlagSet("init", flag.ContinueOnError)
	serverURL := flags.String("server", "", "Komari server URL")
	token := flags.String("token", "", "one-time registration token")
	name := flags.String("name", "", "node display name")
	service := flags.String("service", defaultService, "sing-box systemd service")
	logFile := flags.String("log-file", "", "read sing-box's log from this file instead of the journal")
	if err := flags.Parse(args); err != nil {
		return err
	}
	if strings.TrimSpace(*serverURL) == "" || strings.TrimSpace(*token) == "" || strings.TrimSpace(*name) == "" {
		return errors.New("server、token 和 name 都不能为空")
	}
	parsed, err := url.Parse(strings.TrimRight(*serverURL, "/"))
	if err != nil || (parsed.Scheme != "http" && parsed.Scheme != "https") || parsed.Host == "" {
		return errors.New("Komari 地址无效，必须是完整的 http:// 或 https:// 地址")
	}
	if _, err := os.Stat(singBoxConfig); err != nil {
		return fmt.Errorf("找不到固定配置文件 %s: %w", singBoxConfig, err)
	}
	if _, err := parseSingboxConfig(singBoxConfig); err != nil {
		return fmt.Errorf("Sing-box 配置无法解析: %w", err)
	}

	cfg := Config{
		Server:            strings.TrimRight(*serverURL, "/"),
		Name:              strings.TrimSpace(*name),
		RegistrationToken: strings.TrimSpace(*token),
		ConfigPath:        singBoxConfig,
		ServiceName:       strings.TrimSpace(*service),
		IntervalSeconds:   5,
		LogFile:           strings.TrimSpace(*logFile),
	}
	if cfg.ServiceName == "" {
		cfg.ServiceName = defaultService
	}
	if err := atomicWriteJSON(configFile, cfg, 0o600); err != nil {
		return err
	}
	fmt.Printf("配置已写入 %s\n", configFile)
	return nil
}

func runAgent() error {
	cfg, err := loadConfig()
	if err != nil {
		return err
	}
	if err := ensureRegistered(&cfg); err != nil {
		return err
	}

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	interval := time.Duration(cfg.IntervalSeconds) * time.Second
	if interval < 2*time.Second || interval > 5*time.Minute {
		interval = 5 * time.Second
	}
	ticker := time.NewTicker(interval)
	defer ticker.Stop()

	state, err := loadAgentState()
	if err != nil {
		return fmt.Errorf("累计状态损坏，请先备份检查，不自动清零: %w", err)
	}
	lastSave := time.Time{}
	log.Printf("已连接节点 %s，固定配置路径 %s", cfg.Name, cfg.ConfigPath)

	for {
		if err := collectAndReport(&cfg, &state); err != nil {
			log.Printf("上报失败: %v", err)
		}
		if time.Since(lastSave) >= 30*time.Second {
			if err := atomicWriteJSON(stateFile, state, 0o600); err != nil {
				log.Printf("保存累计状态失败: %v", err)
			} else {
				lastSave = time.Now()
			}
		}

		select {
		case <-ctx.Done():
			_ = atomicWriteJSON(stateFile, state, 0o600)
			return nil
		case <-ticker.C:
		}
	}
}

func runOnce() error {
	cfg, err := loadConfig()
	if err != nil {
		return err
	}
	if err := ensureRegistered(&cfg); err != nil {
		return err
	}
	state, err := loadAgentState()
	if err != nil {
		return err
	}
	if err := collectAndReport(&cfg, &state); err != nil {
		return err
	}
	return atomicWriteJSON(stateFile, state, 0o600)
}

func loadConfig() (Config, error) {
	var cfg Config
	if err := readJSON(configFile, &cfg); err != nil {
		return cfg, fmt.Errorf("读取 %s 失败: %w", configFile, err)
	}
	if cfg.ConfigPath != singBoxConfig {
		return cfg, fmt.Errorf("配置路径必须是 %s", singBoxConfig)
	}
	if cfg.IntervalSeconds == 0 {
		cfg.IntervalSeconds = 5
	}
	if cfg.ServiceName == "" {
		cfg.ServiceName = defaultService
	}
	return cfg, nil
}

func ensureRegistered(cfg *Config) error {
	if cfg.NodeID != "" && cfg.AgentToken != "" {
		return nil
	}
	if cfg.RegistrationToken == "" {
		return errors.New("没有可用的注册密钥，请从插件页面重新生成安装命令")
	}
	hostname, _ := os.Hostname()
	request := RegisterRequest{
		RegistrationToken: cfg.RegistrationToken,
		Name:              cfg.Name,
		Hostname:          hostname,
		Arch:              runtime.GOARCH,
		AgentVersion:      version,
	}
	var response RegisterResponse
	if err := postJSON(cfg.Server+apiPrefix+"/agent/register", "", request, &response); err != nil {
		return fmt.Errorf("注册失败: %w", err)
	}
	if !response.OK || response.NodeID == "" || response.AgentToken == "" {
		return fmt.Errorf("注册失败: %s", response.Error)
	}
	cfg.NodeID = response.NodeID
	cfg.AgentToken = response.AgentToken
	cfg.RegistrationToken = ""
	if response.Name != "" {
		cfg.Name = response.Name
	}
	if err := atomicWriteJSON(configFile, cfg, 0o600); err != nil {
		return fmt.Errorf("保存正式身份失败: %w", err)
	}
	log.Printf("节点注册成功: %s", cfg.Name)
	return nil
}

func collectAndReport(cfg *Config, state *AgentState) error {
	inbounds, err := parseSingboxConfig(cfg.ConfigPath)
	if err != nil {
		return err
	}
	ports := uniquePorts(inbounds)
	now := time.Now()
	raw, rawErr := readNftCounters()
	if rawErr == nil {
		updateTraffic(state, raw, now)
	}
	hash := hashPorts(ports)
	if rawErr != nil || state.PortsHash != hash || !hasExpectedCounters(raw, ports) {
		if err := rebuildNft(ports); err != nil {
			return fmt.Errorf("创建 nftables 计数器失败: %w", err)
		}
		state.PortsHash = hash
		for key, value := range state.Ports {
			value.Upload.LastRaw = 0
			value.Upload.Seen = false
			value.Upload.Rate = 0
			value.Download.LastRaw = 0
			value.Download.Seen = false
			value.Download.Rate = 0
			state.Ports[key] = value
		}
		state.LastSample = now
	}

	for _, port := range ports {
		key := strconv.Itoa(port)
		if _, ok := state.Ports[key]; !ok {
			state.Ports[key] = PortState{}
		}
	}

	events := []string{}
	var activity []InboundActivity
	if cfg.LogFile != "" {
		// No journal: read what sing-box appended to its log file since the
		// last report, once, for both the warnings and the connections.
		lines := logTail.read(cfg.LogFile)
		events = warningLines(lines)
		activity = collectInboundActivityFromLines(lines, inbounds, now.Unix())
		state.LastActivityUnix = now.Unix()
	} else {
		if state.LastLogUnix == 0 || now.Unix()-state.LastLogUnix >= 30 {
			events = collectWarningLogs(cfg.ServiceName, state.LastLogUnix)
		}
		// Inbound connection activity is scanned every report so per-user online
		// detection reacts within one interval instead of waiting on the 30s log cycle.
		var activityUnix int64
		activity, activityUnix = collectInboundActivity(cfg.ServiceName, state.LastActivityUnix, inbounds)
		state.LastActivityUnix = activityUnix
	}

	hostname, _ := os.Hostname()
	report := Report{
		NodeID:       cfg.NodeID,
		Timestamp:    now.Unix(),
		Hostname:     hostname,
		Arch:         runtime.GOARCH,
		AgentVersion: version,
		Singbox: SingboxStatus{
			Running: serviceRunning(cfg.ServiceName),
			Version: singboxVersion(),
		},
		Inbounds: inbounds,
		Counters: reportCounters(ports, state),
		Events:   events,
		Activity: activity,
	}
	var response apiResponse
	if err := postJSON(cfg.Server+apiPrefix+"/agent/report", cfg.AgentToken, report, &response); err != nil {
		return err
	}
	if !response.OK {
		return errors.New(response.Error)
	}
	tracker.commit()
	if state.LastLogUnix == 0 || now.Unix()-state.LastLogUnix >= 30 {
		state.LastLogUnix = now.Unix() - 1
	}
	return nil
}

// isQUICInbound reports whether an inbound type carries its traffic over QUIC
// (UDP), so nothing shows up in /proc/net/tcp for it.
func isQUICInbound(kind string) bool {
	switch strings.ToLower(strings.TrimSpace(kind)) {
	case "hysteria", "hysteria2", "tuic":
		return true
	}
	return false
}

func parseSingboxConfig(filename string) ([]Inbound, error) {
	data, err := os.ReadFile(filename)
	if err != nil {
		return nil, fmt.Errorf("读取 Sing-box 配置失败: %w", err)
	}
	var root struct {
		Inbounds []struct {
			Type       string `json:"type"`
			Tag        string `json:"tag"`
			ListenPort int    `json:"listen_port"`
			Users      []struct {
				Name string `json:"name"`
			} `json:"users"`
			Multiplex struct {
				Enabled bool `json:"enabled"`
			} `json:"multiplex"`
		} `json:"inbounds"`
	}
	if err := json.Unmarshal(data, &root); err != nil {
		return nil, fmt.Errorf("%s 不是有效 JSON: %w", filename, err)
	}
	result := make([]Inbound, 0, len(root.Inbounds))
	for _, item := range root.Inbounds {
		if item.ListenPort < 1 || item.ListenPort > 65535 {
			continue
		}
		users := make([]string, 0, len(item.Users))
		for _, user := range item.Users {
			name := strings.TrimSpace(user.Name)
			if name != "" {
				users = append(users, name)
			}
		}
		result = append(result, Inbound{Port: item.ListenPort, Type: item.Type, Tag: item.Tag, Users: users, Mux: item.Multiplex.Enabled || isQUICInbound(item.Type)})
	}
	sort.Slice(result, func(i, j int) bool { return result[i].Port < result[j].Port })
	return result, nil
}

func uniquePorts(inbounds []Inbound) []int {
	seen := map[int]bool{}
	ports := make([]int, 0, len(inbounds))
	for _, inbound := range inbounds {
		if !seen[inbound.Port] {
			seen[inbound.Port] = true
			ports = append(ports, inbound.Port)
		}
	}
	sort.Ints(ports)
	return ports
}

func hashPorts(ports []int) string {
	values := make([]string, len(ports))
	for index, port := range ports {
		values[index] = strconv.Itoa(port)
	}
	sum := sha256.Sum256([]byte(strings.Join(values, ",")))
	return hex.EncodeToString(sum[:])
}

func rebuildNft(ports []int) error {
	var rules strings.Builder
	// Delete and replace in one nft transaction: failure leaves existing rules intact.
	if exec.Command("nft", "list", "table", "inet", nftTable).Run() == nil {
		rules.WriteString("delete table inet " + nftTable + "\n")
	}
	rules.WriteString("table inet " + nftTable + " {\n")
	rules.WriteString(" chain input { type filter hook input priority filter; policy accept;\n")
	for _, port := range ports {
		fmt.Fprintf(&rules, "  tcp dport %d counter comment \"sbm:upload:tcp:%d\"\n", port, port)
		fmt.Fprintf(&rules, "  udp dport %d counter comment \"sbm:upload:udp:%d\"\n", port, port)
	}
	rules.WriteString(" }\n")
	rules.WriteString(" chain output { type filter hook output priority filter; policy accept;\n")
	for _, port := range ports {
		fmt.Fprintf(&rules, "  tcp sport %d counter comment \"sbm:download:tcp:%d\"\n", port, port)
		fmt.Fprintf(&rules, "  udp sport %d counter comment \"sbm:download:udp:%d\"\n", port, port)
	}
	rules.WriteString(" }\n}\n")
	command := exec.Command("nft", "-f", "-")
	command.Stdin = strings.NewReader(rules.String())
	output, err := command.CombinedOutput()
	if err != nil {
		return fmt.Errorf("%v: %s", err, strings.TrimSpace(string(output)))
	}
	return nil
}

var nftComment = regexp.MustCompile(`^sbm:(upload|download):(tcp|udp):(\d+)$`)

func readNftCounters() (map[string]uint64, error) {
	output, err := exec.Command("nft", "-j", "list", "table", "inet", nftTable).Output()
	if err != nil {
		return nil, err
	}
	var document struct {
		Nftables []map[string]json.RawMessage `json:"nftables"`
	}
	if err := json.Unmarshal(output, &document); err != nil {
		return nil, err
	}
	result := map[string]uint64{}
	for _, entry := range document.Nftables {
		rawRule, ok := entry["rule"]
		if !ok {
			continue
		}
		var rule struct {
			Comment string                       `json:"comment"`
			Expr    []map[string]json.RawMessage `json:"expr"`
		}
		if json.Unmarshal(rawRule, &rule) != nil || nftComment.FindStringSubmatch(rule.Comment) == nil {
			continue
		}
		for _, expression := range rule.Expr {
			rawCounter, ok := expression["counter"]
			if !ok {
				continue
			}
			var counter struct {
				Bytes uint64 `json:"bytes"`
			}
			if json.Unmarshal(rawCounter, &counter) == nil {
				result[rule.Comment] = counter.Bytes
			}
		}
	}
	return result, nil
}

func hasExpectedCounters(raw map[string]uint64, ports []int) bool {
	if len(ports) == 0 {
		return true
	}
	for _, port := range ports {
		for _, direction := range []string{"upload", "download"} {
			for _, protocol := range []string{"tcp", "udp"} {
				if _, ok := raw[fmt.Sprintf("sbm:%s:%s:%d", direction, protocol, port)]; !ok {
					return false
				}
			}
		}
	}
	return true
}

func updateTraffic(state *AgentState, raw map[string]uint64, now time.Time) {
	if state.Ports == nil {
		state.Ports = map[string]PortState{}
	}
	elapsed := now.Sub(state.LastSample).Seconds()
	if elapsed <= 0 || elapsed > 3600 {
		elapsed = 0
	}
	combined := map[string]struct{ upload, download uint64 }{}
	for comment, bytesValue := range raw {
		parts := nftComment.FindStringSubmatch(comment)
		if parts == nil {
			continue
		}
		port := parts[3]
		value := combined[port]
		if parts[1] == "upload" {
			value.upload += bytesValue
		} else {
			value.download += bytesValue
		}
		combined[port] = value
	}
	for port, current := range combined {
		value := state.Ports[port]
		value.Upload = advanceDirection(value.Upload, current.upload, elapsed)
		value.Download = advanceDirection(value.Download, current.download, elapsed)
		state.Ports[port] = value
	}
	state.LastSample = now
}

func advanceDirection(previous DirectionState, current uint64, elapsed float64) DirectionState {
	delta := uint64(0)
	if previous.Seen {
		if current >= previous.LastRaw {
			delta = current - previous.LastRaw
		} else {
			delta = current
		}
	} else {
		delta = current
	}
	previous.Total += delta
	previous.LastRaw = current
	previous.Seen = true
	if elapsed > 0 {
		previous.Rate = uint64(float64(delta) / elapsed)
	} else {
		previous.Rate = 0
	}
	return previous
}

func reportCounters(ports []int, state *AgentState) []ReportCounter {
	result := make([]ReportCounter, 0, len(ports))
	for _, port := range ports {
		value := state.Ports[strconv.Itoa(port)]
		result = append(result, ReportCounter{
			Port:          port,
			UploadTotal:   value.Upload.Total,
			DownloadTotal: value.Download.Total,
			UploadRate:    value.Upload.Rate,
			DownloadRate:  value.Download.Rate,
		})
	}
	return result
}

func serviceRunning(service string) bool {
	if exec.Command("systemctl", "is-active", "--quiet", service).Run() == nil {
		return true
	}
	if exec.Command("rc-service", service, "status").Run() == nil {
		return true
	}
	return exec.Command("pgrep", "-x", "sing-box").Run() == nil
}

var cachedVersion string
var cachedVersionAt time.Time

func singboxVersion() string {
	if cachedVersion != "" && time.Since(cachedVersionAt) < 5*time.Minute {
		return cachedVersion
	}
	output, err := exec.Command("sing-box", "version").CombinedOutput()
	if err != nil {
		return ""
	}
	line := strings.TrimSpace(strings.SplitN(string(output), "\n", 2)[0])
	cachedVersion = line
	cachedVersionAt = time.Now()
	return cachedVersion
}

func collectWarningLogs(service string, sinceUnix int64) []string {
	if sinceUnix <= 0 {
		sinceUnix = time.Now().Add(-30 * time.Second).Unix()
	}
	args := []string{"-u", service, "--since", "@" + strconv.FormatInt(sinceUnix, 10), "--no-pager", "-n", "200", "-o", "short-iso"}
	output, err := exec.Command("journalctl", args...).CombinedOutput()
	if err != nil {
		return nil
	}
	return warningLines(strings.Split(string(output), "\n"))
}

// warningLines keeps the WARN and worse lines, cut to 2000 bytes each.
func warningLines(lines []string) []string {
	result := make([]string, 0)
	for _, line := range lines {
		line = ansiRe.ReplaceAllString(line, "")
		line = strings.TrimSpace(line)
		if line == "" || strings.HasPrefix(line, "-- No entries --") {
			continue
		}
		upper := strings.ToUpper(line)
		if !strings.Contains(upper, "WARN") && !strings.Contains(upper, "ERROR") && !strings.Contains(upper, "FATAL") && !strings.Contains(upper, "PANIC") {
			continue
		}
		if len(line) > 2000 {
			line = line[:2000]
		}
		result = append(result, line)
	}
	return result
}

// sing-box writes ANSI colour codes to the journal (e.g. "\x1b[36mINFO\x1b[0m..."),
// so lines are stripped before matching.
var ansiRe = regexp.MustCompile("\x1b\\[[0-9;]*m")

// inboundConnRe matches sing-box INFO connection lines and captures: the
// connection id, the inbound tag, the kind ("multiplex ", "packet " or empty for
// a plain TCP connection), the preposition (from = client accept, to = routed
// destination) and the peer. An optional "[user] " between the tag and "inbound
// connection" is tolerated (sing-box prints it on the "to" line). Error lines
// ("process connection ... invalid request", port-scan noise) never contain this
// phrase and are ignored.
var inboundConnRe = regexp.MustCompile(`\[(\d+) [^\]]*\] inbound/[a-zA-Z0-9_]+\[([a-zA-Z0-9_.\-]+)\]: (?:\[[^\]]*\] )?inbound (multiplex |packet )?connection (from|to) (\S+)`)

// healthCheckHosts are common client latency/connectivity-probe targets. Traffic
// to these is a node health check, not real browsing, so it must not mark a user
// online. Matched by exact host or dot-suffix.
var healthCheckHosts = []string{
	"cloudflare.com", "cp.cloudflare.com", "gstatic.com", "connectivitycheck.gstatic.com",
	"clients3.google.com", "clients4.google.com", "connectivitycheck.android.com",
	"captive.apple.com", "www.apple.com", "detectportal.firefox.com",
	"msftconnecttest.com", "msftncsi.com", "edge.microsoft.com",
}

func hostOnly(peer string) string {
	// Strip the :port. IPv6 literals arrive as "[addr]:port".
	if strings.HasPrefix(peer, "[") {
		if end := strings.Index(peer, "]"); end > 0 {
			return peer[1:end]
		}
	}
	if i := strings.LastIndex(peer, ":"); i > 0 {
		return peer[:i]
	}
	return peer
}

func isHealthCheck(host string) bool {
	host = strings.ToLower(strings.TrimSuffix(host, "."))
	for _, h := range healthCheckHosts {
		if host == h || strings.HasSuffix(host, "."+h) {
			return true
		}
	}
	return false
}

// tagAccumulator collects what happened on one inbound tag since the last report
// the server accepted.
type tagAccumulator struct {
	count   int
	probes  int
	source  string
	domains []string
	domSeen map[string]bool
	visits  []Visit
}

// trackedConn is one accepted client TCP connection, keyed by the client
// address sing-box logged, so it can be matched against the kernel socket table.
type trackedConn struct {
	id    string
	tag   string
	peer  string
	start int64
	real  bool // routed to a real (non health-check) destination
}

// connTracker joins the journal ("connection from <peer>" / "connection to
// <dest>", linked by the connection id) with the established sockets on each
// inbound port. That gives a true "is anything still connected" signal instead
// of guessing from how long ago the last new connection was logged.
type connTracker struct {
	conns      map[string]*trackedConn // tag|peer
	byID       map[string]*trackedConn
	acc        map[string]*tagAccumulator
	cursor     string
	backfilled bool
}

func newConnTracker() *connTracker {
	return &connTracker{
		conns: map[string]*trackedConn{},
		byID:  map[string]*trackedConn{},
		acc:   map[string]*tagAccumulator{},
	}
}

var tracker = newConnTracker()

const (
	maxVisitsPerReport = 500
	backfillSeconds    = 12 * 3600
	closedConnGrace    = 30
)

// commit drops the accumulated activity once the server has accepted a report.
// Until then it keeps growing, so a failed upload never loses visits.
func (t *connTracker) commit() {
	t.acc = map[string]*tagAccumulator{}
}

func (t *connTracker) accumulator(tag string) *tagAccumulator {
	entry := t.acc[tag]
	if entry == nil {
		entry = &tagAccumulator{domSeen: map[string]bool{}}
		t.acc[tag] = entry
	}
	return entry
}

var singboxStampRe = regexp.MustCompile(`^[+-]\d{4} \d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} `)

// parseJournalLine reads one `journalctl -o short-unix` line.
func parseJournalLine(raw string) (ts int64, id, tag, kind, dir, peer string, ok bool) {
	line := ansiRe.ReplaceAllString(raw, "")
	space := strings.IndexByte(line, ' ')
	if space <= 0 {
		return
	}
	stamp, err := strconv.ParseFloat(line[:space], 64)
	if err != nil {
		return
	}
	colon := strings.Index(line, ": ")
	if colon < 0 {
		return
	}
	// With log.timestamp enabled sing-box prefixes its own "+0900 2006-01-02
	// 15:04:05 " before the level.
	message := singboxStampRe.ReplaceAllString(line[colon+2:], "")
	if !strings.HasPrefix(message, "INFO") {
		return
	}
	match := inboundConnRe.FindStringSubmatch(message)
	if match == nil {
		return
	}
	return int64(stamp), match[1], match[2], strings.TrimSpace(match[3]), match[4], match[5], true
}

// handleLine feeds one parsed connection line into the tracker. record=false is
// used when rebuilding the connection table after an agent restart: the
// connections are learned, but nothing is reported as new activity.
func (t *connTracker) handleLine(ts int64, id, tag, kind, dir, peer string, record bool) {
	if dir == "from" {
		if kind == "" {
			key := tag + "|" + peer
			if old := t.conns[key]; old != nil {
				delete(t.byID, old.id)
			}
			conn := &trackedConn{id: id, tag: tag, peer: peer, start: ts}
			t.conns[key] = conn
			t.byID[id] = conn
		}
		if record {
			// Behind a frontend this is the relay, not the user.
			t.accumulator(tag).source = peer
		}
		return
	}
	host := hostOnly(peer)
	probe := isHealthCheck(host)
	if conn := t.byID[id]; conn != nil && kind == "" {
		conn.real = !probe
	}
	if !record {
		return
	}
	entry := t.accumulator(tag)
	if probe {
		entry.probes++
		return
	}
	entry.count++
	if !entry.domSeen[host] && len(entry.domains) < 30 {
		entry.domSeen[host] = true
		entry.domains = append(entry.domains, host)
	}
	if len(entry.visits) < maxVisitsPerReport {
		entry.visits = append(entry.visits, Visit{T: ts, Host: host})
	}
}

// scanJournal streams journalctl output line by line and returns the cursor of
// the last entry shown (empty when nothing was shown).
func scanJournal(service string, extra []string, handle func(raw string)) (string, error) {
	args := append([]string{"-u", service, "--no-pager", "-o", "short-unix", "--show-cursor"}, extra...)
	command := exec.Command("journalctl", args...)
	stdout, err := command.StdoutPipe()
	if err != nil {
		return "", err
	}
	if err := command.Start(); err != nil {
		return "", err
	}
	cursor := ""
	scanner := bufio.NewScanner(stdout)
	scanner.Buffer(make([]byte, 64*1024), 1024*1024)
	for scanner.Scan() {
		line := scanner.Text()
		if strings.HasPrefix(line, "-- cursor: ") {
			cursor = strings.TrimSpace(strings.TrimPrefix(line, "-- cursor: "))
			continue
		}
		handle(line)
	}
	if err := command.Wait(); err != nil {
		return "", err
	}
	return cursor, scanner.Err()
}

// parseProcTCP adds the remote address of every ESTABLISHED socket whose local
// port is watched. data is the content of /proc/net/tcp or /proc/net/tcp6.
func parseProcTCP(data string, ports map[int]bool, out map[int]map[string]bool) {
	for _, line := range strings.Split(data, "\n") {
		fields := strings.Fields(line)
		if len(fields) < 4 || fields[3] != "01" {
			continue
		}
		_, localPort, ok := parseProcAddr(fields[1])
		if !ok || !ports[int(localPort)] {
			continue
		}
		addr, remotePort, ok := parseProcAddr(fields[2])
		if !ok {
			continue
		}
		if out[int(localPort)] == nil {
			out[int(localPort)] = map[string]bool{}
		}
		out[int(localPort)][netip.AddrPortFrom(addr, remotePort).String()] = true
	}
}

// parseProcAddr decodes "HEXADDR:HEXPORT". The kernel prints the address as
// 32-bit words in host byte order (little-endian on amd64/arm64).
func parseProcAddr(value string) (netip.Addr, uint16, bool) {
	colon := strings.LastIndexByte(value, ':')
	if colon < 0 {
		return netip.Addr{}, 0, false
	}
	port, err := strconv.ParseUint(value[colon+1:], 16, 16)
	if err != nil {
		return netip.Addr{}, 0, false
	}
	raw, err := hex.DecodeString(value[:colon])
	if err != nil || (len(raw) != 4 && len(raw) != 16) {
		return netip.Addr{}, 0, false
	}
	for i := 0; i+4 <= len(raw); i += 4 {
		raw[i], raw[i+1], raw[i+2], raw[i+3] = raw[i+3], raw[i+2], raw[i+1], raw[i]
	}
	addr, ok := netip.AddrFromSlice(raw)
	if !ok {
		return netip.Addr{}, 0, false
	}
	return addr.Unmap(), uint16(port), true
}

// readEstablished returns the established client sockets per watched port.
// ok=false means the socket table could not be read at all.
func readEstablished(ports map[int]bool) (map[int]map[string]bool, bool) {
	out := map[int]map[string]bool{}
	ok := false
	for _, name := range []string{"/proc/net/tcp", "/proc/net/tcp6"} {
		data, err := os.ReadFile(name)
		if err != nil {
			continue
		}
		ok = true
		parseProcTCP(string(data), ports, out)
	}
	return out, ok
}

// liveCounts counts, per tag, tracked connections to real destinations whose
// socket is still established, and forgets connections that have closed.
func (t *connTracker) liveCounts(established map[int]map[string]bool, portOf map[string]int, now int64) map[string]int {
	live := map[string]int{}
	for key, conn := range t.conns {
		if established[portOf[conn.tag]][conn.peer] {
			if conn.real {
				live[conn.tag]++
			}
			continue
		}
		if now-conn.start > closedConnGrace {
			delete(t.conns, key)
			delete(t.byID, conn.id)
		}
	}
	return live
}

func collectInboundActivity(service string, sinceUnix int64, inbounds []Inbound) ([]InboundActivity, int64) {
	now := time.Now().Unix()
	tags := map[string]bool{}
	portOf := map[string]int{}
	muxOf := map[string]bool{}
	ports := map[int]bool{}
	for _, inbound := range inbounds {
		if inbound.Tag != "" {
			tags[inbound.Tag] = true
			portOf[inbound.Tag] = inbound.Port
			muxOf[inbound.Tag] = inbound.Mux
			ports[inbound.Port] = true
		}
	}
	if len(tags) == 0 {
		return nil, now
	}
	t := tracker
	established, procOK := readEstablished(ports)

	// After a restart, relearn which destination each still-open socket belongs
	// to, so a long-running connection is not forgotten.
	if !t.backfilled {
		t.backfilled = true
		if procOK && len(established) > 0 {
			_, _ = scanJournal(service, []string{"--since", "@" + strconv.FormatInt(now-backfillSeconds, 10)}, func(raw string) {
				if ts, id, tag, kind, dir, peer, ok := parseJournalLine(raw); ok && tags[tag] {
					t.handleLine(ts, id, tag, kind, dir, peer, false)
				}
			})
		}
	}

	var extra []string
	if t.cursor != "" {
		extra = []string{"--after-cursor", t.cursor}
	} else {
		// First run or a long gap: only look at the recent window so we never
		// replay the whole journal or invent a backlog of "connections".
		if sinceUnix <= 0 || now-sinceUnix > 3600 {
			sinceUnix = now - 15
		}
		extra = []string{"--since", "@" + strconv.FormatInt(sinceUnix, 10)}
	}
	cursor, err := scanJournal(service, extra, func(raw string) {
		if ts, id, tag, kind, dir, peer, ok := parseJournalLine(raw); ok && tags[tag] {
			t.handleLine(ts, id, tag, kind, dir, peer, true)
		}
	})
	nextSince := now
	if err != nil {
		// A stale cursor (journal rotated) fails here; fall back to --since.
		t.cursor = ""
		nextSince = sinceUnix
	} else if cursor != "" {
		t.cursor = cursor
	} else if t.cursor == "" {
		// Nothing logged in the window: pin the cursor to the newest entry so
		// the next scan continues exactly after it instead of by wall clock.
		if latest, latestErr := scanJournal(service, []string{"-n", "1"}, func(string) {}); latestErr == nil {
			t.cursor = latest
		}
	}

	return t.summarize(tags, portOf, muxOf, established, procOK, now), nextSince
}

// summarize turns the accumulated activity and the open sockets into the
// per-tag report entries.
func (t *connTracker) summarize(tags map[string]bool, portOf map[string]int, muxOf map[string]bool, established map[int]map[string]bool, procOK bool, now int64) []InboundActivity {
	live := t.liveCounts(established, portOf, now)
	result := make([]InboundActivity, 0, len(t.acc)+len(live))
	for tag := range tags {
		entry := t.acc[tag]
		if entry == nil {
			entry = &tagAccumulator{}
		}
		if entry.count == 0 && entry.probes == 0 && live[tag] == 0 {
			continue
		}
		result = append(result, InboundActivity{
			Tag:            tag,
			ConnCount:      entry.count,
			LastActiveUnix: now,
			LastSource:     entry.source,
			Domains:        entry.domains,
			Probes:         entry.probes,
			Live:           live[tag],
			LiveTracked:    procOK && !muxOf[tag],
			Visits:         entry.visits,
		})
	}
	sort.Slice(result, func(i, j int) bool { return result[i].Tag < result[j].Tag })
	return result
}

// watchedInbounds indexes the tagged inbounds for activity tracking.
func watchedInbounds(inbounds []Inbound) (tags map[string]bool, portOf map[string]int, muxOf map[string]bool, ports map[int]bool) {
	tags, portOf, muxOf, ports = map[string]bool{}, map[string]int{}, map[string]bool{}, map[int]bool{}
	for _, inbound := range inbounds {
		if inbound.Tag != "" {
			tags[inbound.Tag] = true
			portOf[inbound.Tag] = inbound.Port
			muxOf[inbound.Tag] = inbound.Mux
			ports[inbound.Port] = true
		}
	}
	return
}

// collectInboundActivityFromLines is collectInboundActivity for a log file:
// the lines are what sing-box appended since the last report.
func collectInboundActivityFromLines(lines []string, inbounds []Inbound, now int64) []InboundActivity {
	tags, portOf, muxOf, ports := watchedInbounds(inbounds)
	if len(tags) == 0 {
		return nil
	}
	t := tracker
	for _, raw := range lines {
		if ts, id, tag, kind, dir, peer, ok := parseLogFileLine(raw, now); ok && tags[tag] {
			t.handleLine(ts, id, tag, kind, dir, peer, true)
		}
	}
	established, procOK := readEstablished(ports)
	return t.summarize(tags, portOf, muxOf, established, procOK, now)
}

// singboxFileStampRe is sing-box's own timestamp ("+0800 2006-01-02 15:04:05")
// at the start of a line it writes to a file.
var singboxFileStampRe = regexp.MustCompile(`^([+-]\d{4} \d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}) `)

// parseLogFileLine reads one line of sing-box's own log output. Without
// log.timestamp a line carries no time and is taken as written just now.
func parseLogFileLine(raw string, now int64) (ts int64, id, tag, kind, dir, peer string, ok bool) {
	line := strings.TrimSpace(ansiRe.ReplaceAllString(raw, ""))
	ts = now
	if m := singboxFileStampRe.FindStringSubmatch(line); m != nil {
		if parsed, err := time.Parse("-0700 2006-01-02 15:04:05", m[1]); err == nil {
			ts = parsed.Unix()
		}
		line = line[len(m[0]):]
	}
	if !strings.HasPrefix(line, "INFO") {
		return
	}
	match := inboundConnRe.FindStringSubmatch(line)
	if match == nil {
		return
	}
	return ts, match[1], match[2], strings.TrimSpace(match[3]), match[4], match[5], true
}

// fileTail follows a log file across reports: it starts at the end (nothing
// from before the agent started is replayed), starts over when the file is
// replaced or truncated, and never splits a line still being written.
type fileTail struct {
	path   string
	offset int64
	ino    uint64
	opened bool
}

var logTail = &fileTail{}

const maxTailRead = 4 << 20

func (f *fileTail) read(path string) []string {
	info, err := os.Stat(path)
	if err != nil {
		return nil
	}
	var ino uint64
	if st, ok := info.Sys().(*syscall.Stat_t); ok {
		ino = st.Ino
	}
	size := info.Size()
	if !f.opened || f.path != path {
		f.path, f.offset, f.ino, f.opened = path, size, ino, true
		return nil
	}
	if ino != f.ino || size < f.offset {
		f.offset, f.ino = 0, ino
	}
	if size == f.offset {
		return nil
	}
	file, err := os.Open(path)
	if err != nil {
		return nil
	}
	defer file.Close()
	length := size - f.offset
	if length > maxTailRead {
		// A burst larger than this is skipped to its last part rather than
		// read whole into a small machine's memory.
		f.offset = size - maxTailRead
		length = maxTailRead
	}
	buf := make([]byte, length)
	n, err := file.ReadAt(buf, f.offset)
	if n <= 0 {
		return nil
	}
	buf = buf[:n]
	end := bytes.LastIndexByte(buf, '\n')
	if end < 0 {
		return nil
	}
	f.offset += int64(end + 1)
	return strings.Split(string(buf[:end]), "\n")
}

func postJSON(endpoint, token string, input any, output any) error {
	payload, err := json.Marshal(input)
	if err != nil {
		return err
	}
	request, err := http.NewRequest(http.MethodPost, endpoint, bytes.NewReader(payload))
	if err != nil {
		return err
	}
	request.Header.Set("Content-Type", "application/json")
	request.Header.Set("User-Agent", "sb-agent/"+version)
	if token != "" {
		request.Header.Set("Authorization", "Bearer "+token)
	}
	response, err := httpClient.Do(request)
	if err != nil {
		return err
	}
	defer response.Body.Close()
	body, err := io.ReadAll(io.LimitReader(response.Body, 2<<20))
	if err != nil {
		return err
	}
	if response.StatusCode < 200 || response.StatusCode >= 300 {
		var apiError apiResponse
		if json.Unmarshal(body, &apiError) == nil && apiError.Error != "" {
			return fmt.Errorf("HTTP %d: %s", response.StatusCode, apiError.Error)
		}
		return fmt.Errorf("HTTP %d: %s", response.StatusCode, strings.TrimSpace(string(body)))
	}
	if output != nil {
		if err := json.Unmarshal(body, output); err != nil {
			return err
		}
	}
	return nil
}

func loadAgentState() (AgentState, error) {
	state := AgentState{Ports: map[string]PortState{}}
	if err := readJSON(stateFile, &state); err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return state, nil
		}
		return state, err
	}
	if state.Ports == nil {
		state.Ports = map[string]PortState{}
	}
	return state, nil
}

func readJSON(filename string, target any) error {
	data, err := os.ReadFile(filename)
	if err != nil {
		return err
	}
	return json.Unmarshal(data, target)
}

func atomicWriteJSON(filename string, value any, mode os.FileMode) error {
	data, err := json.MarshalIndent(value, "", "  ")
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(filename), 0o700); err != nil {
		return err
	}
	temp, err := os.CreateTemp(filepath.Dir(filename), ".sb-agent-*")
	if err != nil {
		return err
	}
	tempName := temp.Name()
	defer os.Remove(tempName)
	if err := temp.Chmod(mode); err != nil {
		temp.Close()
		return err
	}
	if _, err := temp.Write(append(data, '\n')); err != nil {
		temp.Close()
		return err
	}
	if err := temp.Sync(); err != nil {
		temp.Close()
		return err
	}
	if err := temp.Close(); err != nil {
		return err
	}
	return os.Rename(tempName, filename)
}
