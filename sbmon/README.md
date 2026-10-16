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
- systemd 的机器从 journal 读 sing-box 日志；OpenRC（Alpine）的机器从 `/etc/init.d/sing-box` 里 `output_log` 指向的文件读（默认 `/var/log/sing-box.log`），sb-agent 以 `GOMEMLIMIT=16MiB` 跑在 OpenRC 服务 `sb-agent` 下；没有 bash 的 Alpine 会先 `apk add bash`。
- sb-agent 装失败不影响 monitor-agent，安装命令会提示重跑。

单独装 sing-box 监控（不装整机探针）仍可以在「sing-box」页点「单独安装」生成一次性命令。原来从 Komari 迁过来的 sb-agent 也是这种，可以在 sing-box 页用「关联到节点」把它和整机探针的节点对上。

## 每个用户的流量和准确的在连（clash_api）

sing-box 配置里开了只监听本机的 `experimental.clash_api`（如 `"external_controller": "127.0.0.1:19090"` 加 `secret`）时，sb-agent 0.5.0 每秒读一次它的 `/connections`：

- 在连数直接取自 sing-box 打开着的连接，**Hysteria2 / TUIC / mux 也准确**，不再靠「多久没有新连接」猜下线。
- 每条连接的上下行字节按用户累计。clash_api 本身不给用户名，sb-agent 用日志里 `[用户] inbound connection to` 这一行把连接的来源地址对到用户，所以日志级别要是 `info`。两次上报之间开了又关的短连接也算在内（每秒采一次，最后的字节数留着）。
- sing-box 页每台服务器下面列出各用户今天和本月的流量，顶部有所有服务器的合计。对不上用户的连接记在「未识别」下。
- 只用回环地址上的 clash_api，secret 不会离开本机。没开 clash_api 的机器照旧按日志和 `/proc/net/tcp` 判断。

## 每个用户的连接日志

sb-agent 0.6.0 起每次上报带上这段时间的连接明细（时间、入站 tag、用户、来源、目标、tcp / udp / mux、是否测速探测），一次最多 3000 条，多出的只报条数。用户取自日志里的 `[用户]`；入站只有一个用户、日志没写用户名时记成这个用户。不需要 clash_api，日志级别是 `info` 即可。

- hub 存在数据目录 `sbmonitor/userlog/<节点>/<日期>.jsonl`，按「统计时区」分天，保留天数在「连接监控 → 监控设置 → 连接日志保留天数」（默认 7，1–90）。删节点时一起删。
- 后台 sing-box 页的「用户」卡片、各入站下的用户名、各服务器下按用户的流量都能点开日志；连接监控页每个监控用户旁有「连接日志」按钮。弹窗里可以换日期、服务器、用户（含「全部」「未识别」），搜目标或来源，按目标汇总；今天的记录每 10 秒刷新。用户名不分大小写（`WTY` 和 `wty` 算一个人）。
- 接口：`GET /api/sbmonitor/v1/admin/userlog?user=&date=&node_id=&q=&probes=1`（`user=?` 查未识别的）。
- 升级前的连接不会补，日志从各机 sb-agent 升到 0.6.0 那一刻开始。

## 组成

| 位置 | 说明 |
| --- | --- |
| `src/sbmon/engine.rs` | 状态机：注册、上报、流量记账、上线 / 下线判定、通知文本。纯逻辑，不碰文件和网络 |
| `src/sbmon/mod.rs` | 文件读写、Telegram 发送、定时任务、HTTP 接口 |
| `src/sbmon/userlog.rs` | 连接日志：按天写入、按用户查询、过期清理 |
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
