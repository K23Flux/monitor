# sing-box 监控（sbmon）

K23Flux/monitor 在上游 [monitor-probe/monitor](https://github.com/monitor-probe/monitor) 的基础上加的模块，把原来的 Komari 插件 [komari-singbox-monitor](https://github.com/K23Flux/komari-singbox-monitor)（0.3.2）搬进 hub：

- **sing-box 页**：每台落地机各入站端口的实时速度、今日 / 本月流量、每分钟历史、sing-box 版本和运行状态、WARN 以上的异常日志。
- **连接监控页**：按入站 tag 监控指定用户（如 `ss2022_alice=alice`），真正连上才算上线、连接全断才算下线，下线通知里列出这段时间访问的全部域名和时间。
- **独立的 Telegram 机器人**：只发连接监控的上线 / 下线通知，和 hub「通知」页的节点掉线、流量、到期告警分开配置。

## 安装：跟着整机探针一起装

在后台「节点」页添加机器（单台或批量窗口都行），复制安装命令到机器上执行。`install.sh` 装完 monitor-agent 后，发现 `/etc/sing-box/config.json` 就从 hub 下载 sb-agent 一起装上，并用这台机器的节点 Token 注册，不用另外生成注册密钥：

- sb-agent 自己读 sing-box 配置，**入站、端口、协议、用户名自动识别**，在「sing-box」页按机器列出；「连接监控」页的设置里会列出探测到的全部入站，点一下就加进监控。
- sing-box 页里这台机器显示「随探针安装」，名字跟着「节点」页走；在「节点」页删掉机器，sing-box 记录一起删。
- 已经装过探针的机器，**再跑一次同样的命令**（或 `--upgrade`）就会补装 / 升级 sb-agent。
- 不想装就在命令末尾加 `--no-singbox`；`--uninstall` 会一起卸载 sb-agent（保留 `/etc/sb-agent` 里的配置）。
- 只支持 systemd 的机器（sb-agent 读 journal）；OpenRC 的机器只装 monitor-agent。
- sb-agent 装失败不影响 monitor-agent，安装命令会提示重跑。

单独装 sing-box 监控（不装整机探针）仍可以在「sing-box」页点「单独安装」生成一次性命令。原来从 Komari 迁过来的 sb-agent 也是这种，可以在 sing-box 页用「关联到节点」把它和整机探针的节点对上。

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

`agent/register` 的 `registration_token` 可以是后台生成的一次性密钥，也可以是 hub 节点的 Token（`install.sh` 用的就是这个），失败次数和 hub 自己的注册一起限流。

后台（要登录，和 hub 其他接口一样拒绝跨站写请求）：`GET admin/state`、`GET admin/watch`、`GET admin/history?node_id=&date=`、`GET/PUT admin/config`、`DELETE admin/config/token`、`POST admin/registration`、`POST admin/node/{rename,alias,delete,link}`、`POST admin/telegram/test`。

## 编译

```bash
./scripts/theme.sh
(cd web-admin && npm ci && npm run build)
./sbmon/build-agent.sh          # 要 Go 1.22+
cargo build --release
cargo test
```

`sbmon/bin/` 里没有程序时 hub 也能编译运行，只是落地机安装脚本下载不到 sb-agent；这时可以把程序放到数据目录的 `sbmonitor/bin/` 下，hub 优先用那里的文件。
