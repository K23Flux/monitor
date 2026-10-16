// sing-box monitoring pages: per-port traffic of the sb-agents on the landing
// nodes, and the connection monitor for watched users. Backed by the hub's
// sbmon module under /api/sbmonitor/v1 (src/sbmon).

import { useCallback, useEffect, useMemo, useState } from "react"
import { ChevronDown, ChevronRight, Copy, LineChart, Pencil, Plus, RefreshCw, Send, Trash2 } from "lucide-react"
import { toast } from "sonner"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Switch } from "@/components/ui/switch"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { api, type Node as HubNode } from "@/lib/api"
import { bytes } from "@/lib/format"

const SB = "/sbmonitor/v1/admin"

type Traffic = { upload: number; download: number; total: number }
type Port = {
  port: number
  type: string
  tag: string
  users: string[]
  alias: string
  displayName: string
  uploadRate: number
  downloadRate: number
  today: Traffic
  month: Traffic
}
type SbNode = {
  id: string
  name: string
  hostname: string
  arch: string
  agentVersion: string
  lastSeen: string | null
  online: boolean
  singbox: { running: boolean; version: string }
  ports: Port[]
  hubNodeId: number | null
  watchUsers: boolean
  singboxDownSince: string | null
}
type SbEvent = { id: string; nodeName: string; time: string; level: string; message: string }
type SbState = {
  version: string
  config: { offlineSeconds: number; timezoneOffset: number }
  nodes: SbNode[]
  events: SbEvent[]
  pending: number
}

type Visit = { host: string; first: string; last: string; count: number }
type WatchRow = {
  label: string
  tag: string
  port: number | null
  nodeId: string
  nodeName: string
  nodeOnline: boolean
  online: boolean
  live: number
  liveTracked: boolean
  sessionStart: string | null
  sessionConn: number
  lastActive: string | null
  lastSource: string
  connTotal: number
  domains: string[]
}
type UserEvent = {
  id: string
  kind: "online" | "offline"
  label: string
  tag: string
  nodeName: string
  source: string
  domains: string[]
  time: string
  sessionStart?: string
  sessionConn?: number
  visits?: Visit[]
  note?: string
}
type WatchState = {
  server_time: string
  telegram_configured: boolean
  rules: { tag: string; label: string }[]
  watched: WatchRow[]
  events: UserEvent[]
}

/** Polls `path` every `ms` while the page is open. */
function usePoll<T>(path: string, ms = 5000) {
  const [data, setData] = useState<T | null>(null)
  const [error, setError] = useState("")
  const load = useCallback(() => {
    api<T>(path)
      .then((d) => { setData(d); setError("") })
      .catch((e: Error) => setError(e.message))
  }, [path])
  useEffect(() => {
    load()
    const timer = setInterval(() => { if (!document.hidden) load() }, ms)
    return () => clearInterval(timer)
  }, [load, ms])
  return { data, error, reload: load }
}

async function post(path: string, body: unknown) {
  return api(`${SB}${path}`, { method: "POST", body: JSON.stringify(body) })
}

function speed(bytesPerSecond: number) {
  const bits = (bytesPerSecond || 0) * 8
  if (bits >= 1e9) return `${(bits / 1e9).toFixed(2)} Gbps`
  if (bits >= 1e6) return `${(bits / 1e6).toFixed(2)} Mbps`
  if (bits >= 1e3) return `${(bits / 1e3).toFixed(1)} Kbps`
  return `${Math.round(bits)} bps`
}

const when = (iso: string | null) => (iso ? new Date(iso).toLocaleString("zh-CN", { hour12: false }) : "从未")
const clock = (iso: string) => new Date(iso).toLocaleTimeString("zh-CN", { hour12: false })

function ago(iso: string | null, now: number) {
  if (!iso) return "从未"
  const s = Math.max(0, Math.round((now - new Date(iso).getTime()) / 1000))
  if (s < 60) return `${s} 秒前`
  if (s < 3600) return `${Math.floor(s / 60)} 分钟前`
  if (s < 86400) return `${Math.floor(s / 3600)} 小时前`
  return `${Math.floor(s / 86400)} 天前`
}

function span(from: string | null, to: number) {
  if (!from) return "—"
  const s = Math.max(0, Math.round((to - new Date(from).getTime()) / 1000))
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  return h > 0 ? `${h}h${m}m` : m > 0 ? `${m}m` : `${s}s`
}

async function copy(text: string) {
  try {
    await navigator.clipboard.writeText(text)
    toast.success("已复制")
  } catch {
    toast.error("复制失败，请手动选择复制")
  }
}

const shellQuote = (v: string) => `'${v.replace(/'/g, `'"'"'`)}'`

// ---- Sing-box: nodes, ports, traffic ----------------------------------------

function AddNodeDialog({ onClose, onDone }: { onClose: () => void; onDone: () => void }) {
  const [name, setName] = useState("")
  const [command, setCommand] = useState("")
  const [busy, setBusy] = useState(false)

  async function generate() {
    setBusy(true)
    try {
      const r = await post("/registration", { name }) as { token: string; name: string }
      const origin = location.origin
      setCommand(
        `curl -fsSL ${shellQuote(`${origin}/api/sbmonitor/v1/install.sh`)} | bash -s -- --server ${shellQuote(origin)} --token ${shellQuote(r.token)} --name ${shellQuote(r.name)}`,
      )
      onDone()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>单独安装 sing-box 监控</DialogTitle>
          <DialogDescription>
            {"一般不用这里：在「节点」页添加机器时，安装命令发现 /etc/sing-box/config.json 就会顺带装上 sing-box 监控。" +
              "这里给只想装 sing-box 监控、不装整机探针的机器用：生成一条 10 分钟内有效、只能用一次的命令，以 root 执行。"}
          </DialogDescription>
        </DialogHeader>
        {command ? (
          <pre className="max-h-48 overflow-auto rounded-md bg-muted p-3 font-mono text-xs break-all whitespace-pre-wrap">{command}</pre>
        ) : (
          <div className="space-y-2">
            <Label htmlFor="sb-name">节点名称</Label>
            <Input id="sb-name" value={name} maxLength={80} placeholder="例如 node-01" onChange={(e) => setName(e.target.value)} />
          </div>
        )}
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>关闭</Button>
          {command ? (
            <Button onClick={() => copy(command)}><Copy className="size-4" /> 复制</Button>
          ) : (
            <Button onClick={generate} disabled={busy || !name.trim()}>生成安装命令</Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function TextDialog({ title, label, initial, hint, onClose, onSave }: {
  title: string
  label: string
  initial: string
  hint?: string
  onClose: () => void
  onSave: (value: string) => Promise<void>
}) {
  const [value, setValue] = useState(initial)
  const [busy, setBusy] = useState(false)
  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          {hint && <DialogDescription>{hint}</DialogDescription>}
        </DialogHeader>
        <div className="space-y-2">
          <Label htmlFor="sb-text">{label}</Label>
          <Input id="sb-text" value={value} maxLength={80} onChange={(e) => setValue(e.target.value)} />
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>取消</Button>
          <Button
            disabled={busy}
            onClick={async () => {
              setBusy(true)
              try {
                await onSave(value)
                onClose()
              } catch (e) {
                toast.error((e as Error).message)
              } finally {
                setBusy(false)
              }
            }}
          >
            保存
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

type Point = { time: string; points: { port: number; upload_rate: number; download_rate: number }[] }

/** A day's minute rates for one port, drawn as two lines. */
function HistoryDialog({ node, port, onClose }: { node: SbNode; port: Port; onClose: () => void }) {
  const [rows, setRows] = useState<Point[] | null>(null)
  const [error, setError] = useState("")
  useEffect(() => {
    api<{ points: Point[] }>(`${SB}/history?node_id=${node.id}`)
      .then((r) => setRows(r.points))
      .catch((e: Error) => setError(e.message))
  }, [node.id])

  const series = useMemo(() => {
    if (!rows) return null
    return rows.map((r) => {
      const p = r.points.find((x) => x.port === port.port)
      return { t: new Date(r.time).getTime(), up: p?.upload_rate ?? 0, down: p?.download_rate ?? 0 }
    })
  }, [rows, port.port])

  const W = 720
  const H = 220
  let chart = null
  if (series && series.length > 1) {
    const t0 = series[0].t
    const t1 = series[series.length - 1].t
    const max = Math.max(1, ...series.map((s) => Math.max(s.up, s.down)))
    const x = (t: number) => ((t - t0) / Math.max(1, t1 - t0)) * W
    const y = (v: number) => H - (v / max) * (H - 10)
    const line = (key: "up" | "down") => series.map((s, i) => `${i ? "L" : "M"}${x(s.t).toFixed(1)},${y(s[key]).toFixed(1)}`).join("")
    chart = (
      <svg viewBox={`0 0 ${W} ${H}`} className="h-56 w-full rounded-md border bg-muted/30">
        <path d={line("down")} fill="none" stroke="var(--color-chart-1, #2563eb)" strokeWidth="1.5" />
        <path d={line("up")} fill="none" stroke="var(--color-chart-2, #10b981)" strokeWidth="1.5" />
        <text x="6" y="14" className="fill-muted-foreground text-[11px]">{speed(max)}</text>
      </svg>
    )
  }

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="sm:max-w-3xl">
        <DialogHeader>
          <DialogTitle>{node.name} · {port.displayName}（{port.port}）</DialogTitle>
          <DialogDescription>今天每分钟的速度。蓝色下载，绿色上传。</DialogDescription>
        </DialogHeader>
        {error ? <p className="text-sm text-destructive">{error}</p> : !series ? <p className="text-sm text-muted-foreground">加载中…</p> : chart ?? <p className="text-sm text-muted-foreground">今天还没有记录。</p>}
        {series && series.length > 1 && (
          <p className="text-xs text-muted-foreground">
            {clock(new Date(series[0].t).toISOString())} – {clock(new Date(series[series.length - 1].t).toISOString())}
          </p>
        )}
      </DialogContent>
    </Dialog>
  )
}

/** Ties a node registered on its own to one in the hub's node list. */
function LinkSelect({ node, hubNodes, onDone }: { node: SbNode; hubNodes: HubNode[]; onDone: () => void }) {
  return (
    <select
      className="h-8 rounded-md border bg-transparent px-2 text-xs"
      value=""
      title="关联后名字跟着「节点」页走，删掉节点时一起删"
      onChange={async (e) => {
        const id = Number(e.target.value)
        if (!id) return
        try {
          await post("/node/link", { node_id: node.id, hub_node_id: id })
          toast.success("已关联")
          onDone()
        } catch (err) {
          toast.error((err as Error).message)
        }
      }}
    >
      <option value="">关联到节点…</option>
      {hubNodes.map((n) => <option key={n.id} value={n.id}>{n.name}</option>)}
    </select>
  )
}

export function SingBox({ hubNodes }: { hubNodes: HubNode[] }) {
  const { data, error, reload } = usePoll<SbState>(`${SB}/state`)
  const [adding, setAdding] = useState(false)
  const [renaming, setRenaming] = useState<SbNode | null>(null)
  const [aliasing, setAliasing] = useState<{ node: SbNode; port: Port } | null>(null)
  const [charting, setCharting] = useState<{ node: SbNode; port: Port } | null>(null)
  const [deleting, setDeleting] = useState<SbNode | null>(null)

  if (!data) return error ? <p className="text-sm text-destructive">{error}</p> : <p className="text-sm text-muted-foreground">加载中…</p>

  const totals = data.nodes.flatMap((n) => n.ports).reduce(
    (a, p) => ({ today: a.today + p.today.total, month: a.month + p.month.total, down: a.down + p.downloadRate, up: a.up + p.uploadRate }),
    { today: 0, month: 0, down: 0, up: 0 },
  )
  const stats: [string, string, string][] = [
    ["在线节点", `${data.nodes.filter((n) => n.online).length} / ${data.nodes.length}`, `${data.config.offlineSeconds} 秒未上报判为离线`],
    ["今日流量", bytes(totals.today), "所有端口合计"],
    ["本月流量", bytes(totals.month), `按 UTC${data.config.timezoneOffset >= 0 ? "+" : ""}${data.config.timezoneOffset} 划分`],
    ["当前速度", `↓ ${speed(totals.down)}`, `↑ ${speed(totals.up)}`],
  ]

  return (
    <div className="space-y-5">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="text-lg font-semibold">sing-box</h2>
          <p className="text-sm text-muted-foreground">落地机上各入站端口的流量、运行状态和异常日志</p>
        </div>
        <div className="flex gap-2">
          <Button variant="outline" size="sm" onClick={reload}><RefreshCw className="size-4" /> 刷新</Button>
          <Button size="sm" variant="outline" onClick={() => setAdding(true)}><Plus className="size-4" /> 单独安装</Button>
        </div>
      </div>
      {error && <p className="text-sm text-destructive">{error}</p>}

      <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
        {stats.map(([label, value, hint]) => (
          <Card key={label} className="gap-1 p-4">
            <span className="text-xs text-muted-foreground">{label}</span>
            <span className="tnum text-xl font-semibold">{value}</span>
            <span className="text-xs text-muted-foreground">{hint}</span>
          </Card>
        ))}
      </div>

      {data.nodes.length === 0 && (
        <Card className="p-8 text-center text-sm text-muted-foreground">
          {"还没有机器。在「节点」页添加机器，跑过安装命令的机器只要有 /etc/sing-box/config.json 就会出现在这里；已经装过探针的机器再跑一次安装命令即可。"}
        </Card>
      )}

      {data.nodes.map((node) => (
        <Card key={node.id} className="gap-0 overflow-hidden p-0">
          <div className="flex flex-wrap items-center justify-between gap-3 border-b bg-muted/40 px-4 py-3">
            <div className="flex min-w-0 items-center gap-2.5">
              <span className={`size-2 shrink-0 rounded-full ${node.online ? "bg-emerald-500" : "bg-red-500"}`} />
              <div className="min-w-0">
                <div className="flex items-center gap-2 font-medium">
                  {node.name}
                  {node.online && !node.singbox.running && <Badge variant="destructive">sing-box 未运行</Badge>}
                </div>
                <div className="truncate text-xs text-muted-foreground">
                  {node.hostname || "等待首次上报"} · {node.singbox.version || "sing-box 版本未知"}
                  {" · sb-agent"} {node.agentVersion || "?"} · 最后上报 {when(node.lastSeen)}
                </div>
              </div>
            </div>
            <div className="flex flex-wrap items-center gap-1">
              <label className="mr-2 flex items-center gap-1.5 text-xs" title="关掉后这台只统计端口流量，不判断用户上线 / 下线，也不发相关通知">
                <Switch
                  checked={node.watchUsers}
                  onCheckedChange={async (v) => {
                    try {
                      await post("/node/watch", { node_id: node.id, enabled: v })
                      toast.success(v ? "已开启用户监测" : "已关闭用户监测")
                      reload()
                    } catch (e) {
                      toast.error((e as Error).message)
                    }
                  }}
                />
                监测用户
              </label>
              {node.hubNodeId != null ? (
                <Badge variant="secondary" title="名字跟着「节点」页走">随探针安装</Badge>
              ) : (
                <>
                  <LinkSelect node={node} hubNodes={hubNodes.filter((h) => !data.nodes.some((n) => n.hubNodeId === h.id))} onDone={reload} />
                  <Button variant="ghost" size="sm" onClick={() => setRenaming(node)}><Pencil className="size-4" /> 改名</Button>
                </>
              )}
              <Button variant="ghost" size="sm" className="text-destructive" onClick={() => setDeleting(node)}><Trash2 className="size-4" /> 删除</Button>
            </div>
          </div>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>名称</TableHead>
                <TableHead>端口</TableHead>
                <TableHead>协议</TableHead>
                <TableHead className="text-right">↓ 当前</TableHead>
                <TableHead className="text-right">↑ 当前</TableHead>
                <TableHead className="text-right">今日</TableHead>
                <TableHead className="text-right">本月</TableHead>
                <TableHead />
              </TableRow>
            </TableHeader>
            <TableBody>
              {node.ports.length === 0 && (
                <TableRow><TableCell colSpan={8} className="py-6 text-center text-muted-foreground">还没发现入站端口</TableCell></TableRow>
              )}
              {node.ports.map((port) => (
                <TableRow key={port.port}>
                  <TableCell>
                    <div className="font-medium">{port.displayName}</div>
                    <div className="text-xs text-muted-foreground">{port.tag || "无 tag"}{port.users.length > 1 && ` · ${port.users.length} 个用户`}</div>
                  </TableCell>
                  <TableCell className="tnum">{port.port}</TableCell>
                  <TableCell>{port.type || "未知"}</TableCell>
                  <TableCell className="tnum text-right">{speed(port.downloadRate)}</TableCell>
                  <TableCell className="tnum text-right">{speed(port.uploadRate)}</TableCell>
                  <TableCell className="tnum text-right">{bytes(port.today.total)}</TableCell>
                  <TableCell className="tnum text-right">{bytes(port.month.total)}</TableCell>
                  <TableCell className="text-right whitespace-nowrap">
                    <Button variant="ghost" size="icon" title="今天的速度" onClick={() => setCharting({ node, port })}><LineChart className="size-4" /></Button>
                    <Button variant="ghost" size="icon" title="改显示名" onClick={() => setAliasing({ node, port })}><Pencil className="size-4" /></Button>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </Card>
      ))}

      <Card className="gap-3 p-5">
        <div className="flex items-baseline justify-between">
          <h3 className="text-sm font-medium">异常日志</h3>
          <span className="text-xs text-muted-foreground">sing-box 的 WARN 及以上，最近 100 条</span>
        </div>
        {data.events.length === 0 ? (
          <p className="text-sm text-muted-foreground">暂无</p>
        ) : (
          <div className="max-h-96 divide-y overflow-auto">
            {data.events.map((e) => (
              <div key={e.id} className="grid gap-1 py-2 text-sm sm:grid-cols-[10rem_8rem_1fr]">
                <span className="tnum text-xs text-muted-foreground">{when(e.time)}</span>
                <span className="text-xs">{e.nodeName}</span>
                <span className="font-mono text-xs break-all">{e.message}</span>
              </div>
            ))}
          </div>
        )}
      </Card>

      {adding && <AddNodeDialog onClose={() => setAdding(false)} onDone={reload} />}
      {renaming && (
        <TextDialog
          title="修改节点名"
          label="节点名称"
          initial={renaming.name}
          onClose={() => setRenaming(null)}
          onSave={async (name) => { await post("/node/rename", { node_id: renaming.id, name }); reload() }}
        />
      )}
      {aliasing && (
        <TextDialog
          title={`端口 ${aliasing.port.port} 的显示名`}
          label="显示名"
          hint="留空恢复默认（入站第一个用户名，或 tag）。"
          initial={aliasing.port.alias}
          onClose={() => setAliasing(null)}
          onSave={async (alias) => { await post("/node/alias", { node_id: aliasing.node.id, port: aliasing.port.port, alias }); reload() }}
        />
      )}
      {charting && <HistoryDialog node={charting.node} port={charting.port} onClose={() => setCharting(null)} />}
      {deleting && (
        <Dialog open onOpenChange={(open) => !open && setDeleting(null)}>
          <DialogContent className="sm:max-w-md">
            <DialogHeader>
              <DialogTitle>删除 {deleting.name}？</DialogTitle>
              <DialogDescription>
                {"它的流量统计、历史、连接监控记录都会删掉，sb-agent 之后的上报会被拒绝。落地机上的 sb-agent 要另外卸载。"}
              </DialogDescription>
            </DialogHeader>
            <DialogFooter>
              <Button variant="ghost" onClick={() => setDeleting(null)}>取消</Button>
              <Button
                variant="destructive"
                onClick={async () => {
                  try {
                    await post("/node/delete", { node_id: deleting.id })
                    toast.success("已删除")
                    setDeleting(null)
                    reload()
                  } catch (e) {
                    toast.error((e as Error).message)
                  }
                }}
              >
                删除
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
      )}
    </div>
  )
}

// ---- 连接监控: watched users -------------------------------------------------

function EventRow({ event }: { event: UserEvent }) {
  const [open, setOpen] = useState(false)
  const online = event.kind === "online"
  const visits = event.visits ?? []
  return (
    <div className="py-2.5">
      <button className="flex w-full items-start gap-2 text-left text-sm" onClick={() => setOpen(!open)} disabled={online || !visits.length}>
        {!online && visits.length ? (open ? <ChevronDown className="mt-0.5 size-4 shrink-0" /> : <ChevronRight className="mt-0.5 size-4 shrink-0" />) : <span className="size-4 shrink-0" />}
        <span className="tnum w-36 shrink-0 text-xs text-muted-foreground">{when(event.time)}</span>
        <Badge variant={online ? "default" : "secondary"}>{online ? "上线" : "下线"}</Badge>
        <span className="min-w-0 flex-1">
          <span className="font-medium">{event.label}</span>
          <span className="text-muted-foreground"> · {event.nodeName}</span>
          {!online && event.sessionStart && (
            <span className="text-muted-foreground"> · 时长 {span(event.sessionStart, new Date(event.time).getTime())} · {event.sessionConn ?? 0} 次连接 · {visits.length} 个域名</span>
          )}
          {online && event.domains.length > 0 && <span className="text-muted-foreground"> · {event.domains.slice(0, 3).join(", ")}</span>}
          {event.note && <span className="text-amber-600"> · {event.note}</span>}
        </span>
      </button>
      {open && (
        <div className="mt-2 ml-6 max-h-72 overflow-auto rounded-md bg-muted/40 p-3 font-mono text-xs">
          {visits.map((v) => (
            <div key={v.host}>
              {clock(v.first)}  {v.host}{v.count > 1 && ` ×${v.count}（至 ${clock(v.last)}）`}
            </div>
          ))}
        </div>
      )}
    </div>
  )
}

type Detected = { tag: string; users: string[]; type: string; nodes: string[] }

/** Watch rules as `tag → label`, in the order written. */
function parseRules(raw: string): [string, string][] {
  const out: [string, string][] = []
  for (const piece of raw.split(/[,\n]/)) {
    const t = piece.trim()
    if (!t) continue
    const i = t.indexOf("=")
    const tag = (i >= 0 ? t.slice(0, i) : t).trim()
    if (tag && !out.some(([x]) => x === tag)) out.push([tag, (i >= 0 ? t.slice(i + 1) : tag).trim() || tag])
  }
  return out
}

/**
 * Every inbound the sb-agents report, with the users sing-box lists for it. A
 * click watches the tag under its first user's name, or stops watching it. An
 * inbound shared by several users cannot single one out: the agent sees
 * connections per inbound, so such a tag reports whoever of them connects.
 */
function DetectedInbounds({ rules, onChange }: { rules: string; onChange: (v: string) => void }) {
  const [found, setFound] = useState<Detected[] | null>(null)
  useEffect(() => {
    api<SbState>(`${SB}/state`).then((st) => {
      const map = new Map<string, Detected>()
      for (const n of st.nodes) {
        for (const p of n.ports) {
          if (!p.tag) continue
          const d = map.get(p.tag) ?? { tag: p.tag, users: [], type: p.type, nodes: [] }
          for (const u of p.users) if (!d.users.includes(u)) d.users.push(u)
          if (!d.nodes.includes(n.name)) d.nodes.push(n.name)
          map.set(p.tag, d)
        }
      }
      setFound([...map.values()].sort((a, b) => a.tag.localeCompare(b.tag)))
    }).catch(() => setFound([]))
  }, [])
  if (!found?.length) return null
  const current = parseRules(rules)
  const toggle = (d: Detected) => {
    const next = current.some(([t]) => t === d.tag)
      ? current.filter(([t]) => t !== d.tag)
      : [...current, [d.tag, d.users[0] || d.tag] as [string, string]]
    onChange(next.map(([t, l]) => `${t}=${l}`).join("\n"))
  }
  return (
    <div className="flex flex-wrap gap-1.5 pt-1">
      {found.map((d) => {
        const on = current.some(([t]) => t === d.tag)
        return (
          <button
            key={d.tag}
            type="button"
            onClick={() => toggle(d)}
            title={`${d.type} · ${d.nodes.join("、")}${d.users.length > 1 ? " · 多个用户共用，按入站统计" : ""}`}
            className={`rounded-md border px-2 py-1 text-left text-xs transition-colors ${on ? "border-foreground bg-secondary" : "text-muted-foreground hover:bg-muted"}`}
          >
            <span className="font-mono">{d.tag}</span>
            <span className="ml-1.5">{d.users.length ? d.users.join(", ") : "无用户名"}</span>
          </button>
        )
      })}
    </div>
  )
}

function WatchSettings({ onSaved }: { onSaved: () => void }) {
  const [form, setForm] = useState<Record<string, string> | null>(null)
  const [tokenSet, setTokenSet] = useState(false)
  const [busy, setBusy] = useState(false)

  const load = useCallback(() => {
    api<Record<string, unknown>>(`${SB}/config`).then((c) => {
      const next: Record<string, string> = {}
      for (const [k, v] of Object.entries(c)) if (typeof v === "string") next[k] = v
      next.tg_bot_token = ""
      setForm(next)
      setTokenSet(c.tg_bot_token_set === true)
    }).catch((e: Error) => toast.error(e.message))
  }, [])
  useEffect(load, [load])
  if (!form) return null

  const set = (k: string, v: string) => setForm({ ...form, [k]: v })
  const field = (k: string, label: string, placeholder: string, hint?: string) => (
    <div className="space-y-1.5">
      <Label htmlFor={`sb-${k}`}>{label}</Label>
      <Input id={`sb-${k}`} value={form[k] ?? ""} placeholder={placeholder} onChange={(e) => set(k, e.target.value)} />
      {hint && <p className="text-xs text-muted-foreground">{hint}</p>}
    </div>
  )

  async function save() {
    setBusy(true)
    try {
      await api(`${SB}/config`, { method: "PUT", body: JSON.stringify(form) })
      toast.success("已保存")
      load()
      onSaved()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  async function test() {
    try {
      await post("/telegram/test", {})
      toast.success("测试消息已发送")
    } catch (e) {
      toast.error((e as Error).message)
    }
  }

  return (
    <Card className="gap-5 p-5">
      <div>
        <h3 className="text-sm font-medium">监控设置</h3>
        <p className="mt-1 text-xs text-muted-foreground">
          {"这里的 Telegram 机器人发连接监控的上线、下线通知，以及 sing-box 停止、sb-agent 失联的提醒；和「通知」页的整机告警分开。"}
        </p>
      </div>
      <div className="space-y-1.5">
        <Label htmlFor="sb-watch_rules">监控用户</Label>
        <textarea
          id="sb-watch_rules"
          className="min-h-20 w-full rounded-md border bg-transparent px-3 py-2 font-mono text-xs"
          value={form.watch_rules ?? ""}
          placeholder={"ss2022_alice=alice\nhy2_alice=alice"}
          onChange={(e) => set("watch_rules", e.target.value)}
        />
        <p className="text-xs text-muted-foreground">一行或逗号分隔一条，格式「入站 tag=显示名」。按 tag 匹配，所有节点上同名的入站都算。也可以直接点下面探测到的入站。</p>
        <DetectedInbounds rules={form.watch_rules ?? ""} onChange={(v) => set("watch_rules", v)} />
      </div>
      <div className="grid gap-4 sm:grid-cols-2">
        <div className="space-y-1.5">
          <Label htmlFor="sb-tg_bot_token">Bot Token</Label>
          <div className="flex gap-2">
            <Input
              id="sb-tg_bot_token"
              type="password"
              autoComplete="off"
              value={form.tg_bot_token ?? ""}
              placeholder={tokenSet ? "已设置，留空不改" : "123456:ABC…"}
              onChange={(e) => set("tg_bot_token", e.target.value)}
            />
            {tokenSet && (
              <Button
                variant="ghost"
                size="icon"
                title="清除 Token"
                onClick={async () => {
                  try {
                    await api(`${SB}/config/token`, { method: "DELETE" })
                    toast.success("已清除")
                    load()
                    onSaved()
                  } catch (e) {
                    toast.error((e as Error).message)
                  }
                }}
              >
                <Trash2 className="size-4" />
              </Button>
            )}
          </div>
        </div>
        {field("tg_chat_id", "Chat ID", "123456789")}
        {field("online_confirm_seconds", "上线确认（秒）", "15", "只有一条连接时，保持这么久才算上线")}
        {field("offline_grace_seconds", "下线确认（秒）", "60", "连接全断后等这么久没有新连接才算下线")}
        {field("session_idle_seconds", "空闲判定（秒）", "300", "Hysteria2、TUIC、mux 这类数不出在连数的入站，多久没有新连接算下线")}
        {field("offline_seconds", "节点离线判定（秒）", "60")}
        {field("timezone_offset", "统计时区（UTC 偏移）", "8")}
        {field("history_days", "分钟历史保留天数", "30")}
      </div>
      <div className="flex flex-wrap gap-6">
        {(["notify_online", "notify_offline", "notify_status"] as const).map((k) => (
          <label key={k} className="flex items-center gap-2 text-sm">
            <Switch checked={(form[k] ?? "") !== "false"} onCheckedChange={(v) => set(k, v ? "true" : "false")} />
            {k === "notify_online" ? "上线时通知" : k === "notify_offline" ? "下线时通知" : "sing-box 停止 / sb-agent 失联时通知"}
          </label>
        ))}
      </div>
      <div className="flex gap-2">
        <Button onClick={save} disabled={busy}>保存</Button>
        <Button variant="outline" onClick={test}><Send className="size-4" /> 发测试消息</Button>
      </div>
    </Card>
  )
}

export function ConnWatch() {
  const { data, error, reload } = usePoll<WatchState>(`${SB}/watch`)
  if (!data) return error ? <p className="text-sm text-destructive">{error}</p> : <p className="text-sm text-muted-foreground">加载中…</p>
  const now = new Date(data.server_time).getTime()
  const groups = new Map<string, WatchRow[]>()
  for (const row of data.watched) groups.set(row.label, [...(groups.get(row.label) ?? []), row])

  return (
    <div className="space-y-5">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="text-lg font-semibold">连接监控</h2>
          <p className="text-sm text-muted-foreground">按入站监控指定用户的上线、下线和访问的域名</p>
        </div>
        <div className="flex items-center gap-2">
          <Badge variant={data.telegram_configured ? "secondary" : "outline"}>
            Telegram {data.telegram_configured ? "已配置" : "未配置"}
          </Badge>
          <Button variant="outline" size="sm" onClick={reload}><RefreshCw className="size-4" /> 刷新</Button>
        </div>
      </div>
      {error && <p className="text-sm text-destructive">{error}</p>}

      {!data.rules.length ? (
        <Card className="p-6 text-sm text-muted-foreground">还没有配置监控用户，在下面的设置里填写。</Card>
      ) : !data.watched.length ? (
        <Card className="p-6 text-sm text-muted-foreground">已配置监控用户，但还没有节点上报对应的入站。确认落地机装了 sb-agent，且 sing-box 配置里有该 tag。</Card>
      ) : (
        [...groups.entries()].map(([label, rows]) => (
          <Card key={label} className="gap-0 overflow-hidden p-0">
            <div className="flex items-center gap-2 border-b bg-muted/40 px-4 py-3">
              <span className={`size-2 rounded-full ${rows.some((r) => r.online) ? "bg-emerald-500" : "bg-muted-foreground/40"}`} />
              <span className="font-medium">{label}</span>
              <span className="text-xs text-muted-foreground">{rows.some((r) => r.online) ? "在线" : "离线"}</span>
            </div>
            <div className="divide-y">
              {rows.map((r) => (
                <div key={`${r.nodeId}:${r.tag}`} className="space-y-2 px-4 py-3">
                  <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-sm">
                    {r.online ? (
                      <Badge>在线</Badge>
                    ) : r.nodeOnline ? (
                      <Badge variant="secondary">离线</Badge>
                    ) : (
                      <Badge variant="destructive">节点掉线</Badge>
                    )}
                    <span className="font-medium">{r.nodeName}</span>
                    <span className="font-mono text-xs text-muted-foreground">{r.tag}{r.port ? ` :${r.port}` : ""}</span>
                    <span className="text-xs text-muted-foreground">
                      {r.online ? `本次会话 ${span(r.sessionStart, now)}` : `上次活跃 ${ago(r.lastActive, now)}`}
                    </span>
                    <span className="text-xs text-muted-foreground">
                      {r.online
                        ? r.liveTracked ? `在连 ${r.live} / 本次 ${r.sessionConn} 次` : `本次 ${r.sessionConn} 次连接`
                        : `累计 ${r.connTotal} 次连接`}
                    </span>
                    {r.lastSource && <span className="font-mono text-xs text-muted-foreground">来自 {r.lastSource}</span>}
                  </div>
                  {r.domains.length > 0 ? (
                    <div className="flex flex-wrap gap-1">
                      {r.domains.slice(0, 12).map((d) => (
                        <span key={d} className="rounded bg-muted px-1.5 py-0.5 font-mono text-[11px]">{d}</span>
                      ))}
                    </div>
                  ) : (
                    <p className="text-xs text-muted-foreground">暂无真实访问（测速探测已过滤）</p>
                  )}
                </div>
              ))}
            </div>
          </Card>
        ))
      )}

      <Card className="gap-1 p-5">
        <div className="flex items-baseline justify-between">
          <h3 className="text-sm font-medium">上线 / 下线记录</h3>
          <span className="text-xs text-muted-foreground">最近 120 条，点下线记录展开访问的域名</span>
        </div>
        {data.events.length === 0 ? (
          <p className="text-sm text-muted-foreground">暂无</p>
        ) : (
          <div className="max-h-[32rem] divide-y overflow-auto">
            {data.events.map((e) => <EventRow key={e.id} event={e} />)}
          </div>
        )}
      </Card>

      <WatchSettings onSaved={reload} />
    </div>
  )
}
