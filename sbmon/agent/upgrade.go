package main

import (
	"fmt"
	"log"
	"os"
	"os/exec"
	"sync"
	"syscall"
	"time"
)

// upgradeScript is the whole of what the hub can make this machine run: the
// hub's own installer in its upgrade mode, which reuses the credentials this
// machine already holds and upgrades monitor-agent and sb-agent together. The
// hub's reply to a report carries a yes or no, never a command; the address
// comes from this machine's own config and reaches the shell as an argument,
// not as part of the text.
const upgradeScript = `curl -fsSL --max-time 60 "$1/install.sh" | sh -s -- --upgrade`

const (
	upgradeLog = "/var/log/sb-agent-upgrade.log"
	// A hub that keeps asking, or an upgrade that keeps failing, must not turn
	// into a loop on the machine.
	upgradeGap = 10 * time.Minute
)

var upgrading struct {
	mu   sync.Mutex
	last time.Time
}

// upgradeCommand builds the command that runs the upgrade apart from this
// process. The installer restarts sb-agent itself, so it cannot be a child of it:
// under systemd the restart kills everything left in this service's control
// group, a setsid child included, which would cut the installer off at its last
// step. A transient unit lives outside the group. Without systemd (OpenRC) there
// is no such sweep, and a new session is enough.
func upgradeCommand(server string, systemd bool, now time.Time) *exec.Cmd {
	script := upgradeScript + " >>" + upgradeLog + " 2>&1"
	if systemd {
		unit := fmt.Sprintf("sb-agent-upgrade-%d", now.Unix())
		return exec.Command("systemd-run", "--quiet", "--collect", "--unit", unit, "sh", "-c", script, "sh", server)
	}
	cmd := exec.Command("sh", "-c", script, "sh", server)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
	return cmd
}

// startUpgrade acts on the hub's request, at most once per upgradeGap.
func startUpgrade(server string) {
	upgrading.mu.Lock()
	defer upgrading.mu.Unlock()
	now := time.Now()
	if !upgrading.last.IsZero() && now.Sub(upgrading.last) < upgradeGap {
		return
	}
	upgrading.last = now
	_, err := os.Stat("/run/systemd/system")
	cmd := upgradeCommand(server, err == nil, now)
	if err := cmd.Start(); err != nil {
		log.Printf("面板要求升级，但启动升级命令失败: %v", err)
		return
	}
	log.Printf("面板要求升级，已启动升级，输出在 %s", upgradeLog)
	go func() { _ = cmd.Wait() }()
}
