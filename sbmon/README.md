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
- 后台 sing-box 页的「用户」卡片、各入站下的用户名、各服务器下按用户的流量都能点开日志；连接监控页每个监控用户旁有「连接日志」按钮。弹窗里可以换日期、服务器、用户（含「全部」「未识别」），搜目标或来源，按目标汇总；今天的记录每 10 秒刷新。用户名不分大小写（`ZOE` 和 `zoe` 算一个人）。
- 接口：`GET /api/sbmonitor/v1/admin/userlog?user=&date=&node_id=&q=&probes=1`（`user=?` 查未识别的）。
- 升级前的连接不会补，日志从各机 sb-agent 升到 0.6.0 那一刻开始。

## 连接监控页实时更新

hub 1.4.1-sb.6 / sb-agent 0.7.0 起，连接监控页不再每 5 秒刷新一次，而是实时跟着上报走：

- **推送**：页面打开后连 WebSocket `admin/watch/stream`，sb-agent 每次上报后 hub 立刻推一帧（最多每秒一帧，没变化时 5 秒一帧当心跳）。连接断了自动重连，期间每 5 秒拉一次；右上角显示「实时 · N 秒前」或「重新连接中」。
- **在线时每秒上报**：hub 回复上报时告诉 sb-agent 要列哪些监控入站（`watch_tags`），以及这台机器上是否有被监控的用户在线（`fast`）。在线期间 sb-agent 每秒多发一次轻量的 `agent/live`（只读 clash_api 已有的数据，不扫日志、不读 nftables），没人在线就停，15 秒没收到 hub 的确认也自动停。旧 hub 不回这两个字段，0.7.0 的 sb-agent 照旧只做 5 秒一次的常规上报。
- **新数据**（要开 clash_api）：被监控入站上正在打开的连接（目标、TCP/UDP、开了多久、已用流量，最多 100 条）、每个入站自 sb-agent 启动以来的累计流量。hub 据此算下载 / 上传速度（最近约 4 秒）、本次会话流量；下线记录和 Telegram 下线通知里带上这次的流量。
- **页面**：每个监控用户一张卡片（在线状态和本次时长按秒走、速度、本次流量、正在连接数、今天的时间轴、正在访问、本次常用），下面是按天分组的会话记录（按服务器筛选、按域名搜索，点开看按网站合并的访问记录）。网站按主域合并（`rr3---sn-xx.googlevideo.com` 算 `googlevideo.com`），推送、校时、证书检查这类系统后台连接默认隐藏。设置收进右上角「设置」。

## 后台一键升级

hub 1.4.1-sb.7 / sb-agent 0.8.0 起，后台「更新」页可以直接升级 hub 和节点，不用再逐台 SSH：

- **hub**：hub 在容器里以低权限跑，改不了自己的 compose 文件，也碰不到 docker，所以点「升级 hub」只是在数据目录留一个 `upgrade.request`（里面只有目标版本号），由宿主机上 root 的 systemd 路径单元接手，执行 `scripts/upgrade-hub-docker.sh`：备份数据目录和 compose 文件 → 把 compose 里唯一的 `image: ghcr.io/k23flux/monitor:<版本>` 改成新版本 → `docker compose pull` → `up -d` → 等新 hub 应答；拉不到镜像就还原文件、不动现有 hub，起不来就回滚并重新 `up -d`。结果在数据目录的 `upgrade.status`，过程在 `/var/log/monitor-hub-upgrade.log`。请求文件里除了符合 `1.4.1` / `1.4.1-sb.7` 形式的版本号，其它内容一概不认，镜像名固定。版本检查和升级源都是 `K23Flux/monitor` 的发布，不是上游。**宿主机上装一次**：把脚本放到机器上（不能用管道），`sudo sh upgrade-hub-docker.sh install --dir /opt/apps/monitor`（`--health` 默认 `http://127.0.0.1:28080/api/me`，`--backups` 默认 `/root`）；装好后后台才会出现「升级 hub」按钮。卸载：`sudo sh upgrade-hub-docker.sh uninstall`。测试：`scripts/test-upgrade-hub-docker.sh`（用假的 docker 跑安装、升级、拉取失败、回滚、非法请求）。
- **节点**：hub 在 sb-agent 上报的回复里带 `upgrade: true`，sb-agent 在本机跑 hub 的 `install.sh --upgrade`（monitor-agent 和 sb-agent 一起升级）。回复里只有一个布尔值，命令固定写在 sb-agent 里，地址取自本机配置；systemd 机器用 `systemd-run` 起独立单元（安装器会重启 sb-agent，不能是它的子进程），其它走 `setsid`，输出在 `/var/log/sb-agent-upgrade.log`，10 分钟内只执行一次。
- **限制**：sb-agent 低于 0.8.0 听不懂这个字段，要先用页面上的命令手动升一次；没装 sing-box 的机器没有 sb-agent，一直用命令。顺序是先升 hub，再升节点，因为节点的 sb-agent 二进制是 hub 内嵌下发的。
- 接口：`POST /api/hub/upgrade`；`POST /api/sbmonitor/v1/admin/upgrade`，body `{"hub_node_ids":[1,2]}`，不带则请求所有能升级的节点。

## 组成

| 位置 | 说明 |
| --- | --- |
| `src/sbmon/engine.rs` | 状态机：注册、上报、流量记账、上线 / 下线判定、通知文本。纯逻辑，不碰文件和网络 |
| `src/sbmon/mod.rs` | 文件读写、Telegram 发送、定时任务、HTTP 接口 |
| `src/sbmon/userlog.rs` | 连接日志：按天写入、按用户查询、过期清理 |
| `src/sbmon/tests.rs` | 从插件的 `tests/plugin.test.js` 移植的用例 |
| `web-admin/src/components/SingBox.tsx` | 后台的 sing-box 页、连接日志弹窗、监控设置 |
| `web-admin/src/components/ConnWatch.tsx` | 后台的连接监控页 |
| `sbmon/agent/` | sb-agent（Go），和插件 0.3.2 的探针一字不差 |
| `sbmon/installer/` | 落地机的安装 / 卸载脚本 |
| `scripts/upgrade-hub-docker.sh` | 宿主机上的 hub 升级助手（Docker 部署），后台「升级 hub」靠它；`scripts/test-upgrade-hub-docker.sh` 是它的测试 |
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

给 sb-agent（公开，JSON 回复）：`POST agent/register`、`POST agent/report`、`POST agent/live`、`GET install.sh`、`GET uninstall.sh`、`GET bin/{sb-agent-linux-amd64,sb-agent-linux-arm64,checksums.txt}`。

`agent/register` 的 `registration_token` 可以是后台生成的一次性密钥，也可以是 hub 节点的 Token（`install.sh` 用的就是这个），失败次数和 hub 自己的注册一起限流。

后台（要登录，和 hub 其他接口一样拒绝跨站写请求）：`GET admin/state`、`GET admin/watch`、`GET admin/watch/stream`（WebSocket，只接受面板自己的页面）、`GET admin/history?node_id=&date=`、`GET/PUT admin/config`、`DELETE admin/config/token`、`POST admin/registration`、`POST admin/node/{rename,alias,delete,link}`、`POST admin/telegram/test`。

## 编译

```bash
./scripts/theme.sh
(cd web-admin && npm ci && npm run build)
./sbmon/build-agent.sh          # 要 Go 1.22+
cargo build --release
cargo test
```

`sbmon/bin/` 里没有程序时 hub 也能编译运行，只是落地机安装脚本下载不到 sb-agent；这时可以把程序放到数据目录的 `sbmonitor/bin/` 下，hub 优先用那里的文件。
