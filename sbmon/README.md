# sing-box 监控（sbmon）

K23Flux/monitor 在上游 [monitor-probe/monitor](https://github.com/monitor-probe/monitor) 的基础上加的模块，把原来的 Komari 插件 [komari-singbox-monitor](https://github.com/K23Flux/komari-singbox-monitor)（0.3.2）搬进 hub：

- **sing-box 页**：每台落地机各入站端口的实时速度、今日 / 本月流量、每分钟历史、sing-box 版本和运行状态、WARN 以上的异常日志。
- **连接监控页**：按入站 tag 监控指定用户（如 `ss2022_alice=alice`），真正连上才算上线、连接全断才算下线，下线通知里列出这段时间访问的全部域名和时间。
- **独立的 Telegram 机器人**：只发连接监控的上线 / 下线通知，和 hub「通知」页的节点掉线、流量、到期告警分开配置。

## 组成

| 位置 | 说明 |
| --- | --- |
| `src/sbmon/engine.rs` | 状态机：注册、上报、流量记账、上线 / 下线判定、通知文本。纯逻辑，不碰文件和网络 |
| `src/sbmon/mod.rs` | 文件读写、Telegram 发送、定时任务、HTTP 接口 |
| `src/sbmon/tests.rs` | 从插件的 `tests/plugin.test.js` 移植的用例 |
| `web-admin/src/components/SingBox.tsx` | 后台的两个页面 |
| `sbmon/agent/` | sb-agent（Go），和插件 0.3.2 的探针一字不差 |
| `sbmon/installer/` | 落地机的安装 / 卸载脚本 |
| `sbmon/build-agent.sh` | 编译 sb-agent 到 `sbmon/bin/`，hub 编译时把它嵌进去 |

## 和 Komari 插件的兼容

- 接口路径不变，都在 `/api/sbmonitor/v1/` 下。已经装好的 sb-agent 只要还连同一个域名，**不用重装、不用改配置**，节点 ID 和 Token 照用。
- 数据格式不变：插件的 `state.json` 和 `history/` 原样复制到 hub 数据目录的 `sbmonitor/` 下即可（默认是数据库旁边的 `sbmonitor/`，可用 `--sbmon-dir` 改）。
- 设置存在 hub 数据库的 `setting` 表里，键名是插件的键加 `sbmon_` 前缀（如 `sbmon_watch_rules`），在「连接监控」页底部修改。Bot Token 只写不读。

## 和插件相比的改动

- 「连接监控」页在连接全部断开后的确认期里不再显示断开前的在连数（插件的已知问题）：某个监控入站这次没出现在上报里，在连数立即清零。
- 每日流量记录保留 400 天（插件不清理）。
- 删除节点时一起删掉它的分钟历史文件。
- Telegram 请求失败时日志里不带 Bot Token。

## 接口

给 sb-agent（公开，JSON 回复）：`POST agent/register`、`POST agent/report`、`GET install.sh`、`GET uninstall.sh`、`GET bin/{sb-agent-linux-amd64,sb-agent-linux-arm64,checksums.txt}`。

后台（要登录，和 hub 其他接口一样拒绝跨站写请求）：`GET admin/state`、`GET admin/watch`、`GET admin/history?node_id=&date=`、`GET/PUT admin/config`、`DELETE admin/config/token`、`POST admin/registration`、`POST admin/node/{rename,alias,delete}`、`POST admin/telegram/test`。

## 编译

```bash
./scripts/theme.sh
(cd web-admin && npm ci && npm run build)
./sbmon/build-agent.sh          # 要 Go 1.22+
cargo build --release
cargo test
```

`sbmon/bin/` 里没有程序时 hub 也能编译运行，只是落地机安装脚本下载不到 sb-agent；这时可以把程序放到数据目录的 `sbmonitor/bin/` 下，hub 优先用那里的文件。
