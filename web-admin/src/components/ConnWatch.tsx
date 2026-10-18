// The connection monitor page: watched users (an inbound tag each), what
// they are doing right now and their sessions, fed by the hub's sbmon module
// (src/sbmon). The page loads `admin/watch` once and then follows
// `admin/watch/stream`, a WebSocket carrying the watched rows whenever an
// sb-agent reports -- every second while a watched user is online.

import { useCallback, useEffect, useMemo, useRef, useState } from "react"
import { ChevronRight, Search, X } from "lucide-react"

import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { UserLogDialog, WatchSettings } from "@/components/SingBox"
import { api } from "@/lib/api"
import { bytes } from "@/lib/format"

const SB = "/sbmonitor/v1/admin"

// ---- data --------------------------------------------------------------------

type Visit = { host: string; first: string; last: string; count: number }
type OpenConn = { dst: string; net: string; start: number; up: number; down: number }
type WatchRow = {
  label: string
  tag: string
  port: number | null
  nodeId: string
  nodeName: string
  nodeOnline: boolean
  online: boolean
  pending?: boolean
  live: number
  liveTracked: boolean
  sessionStart: string | null
  sessionConn: number
  lastActive: string | null
  lastSource: string
  connTotal: number
  domains: string[]
  rateUp?: number
  rateDown?: number
  sessionUp?: number
  sessionDown?: number
  open?: OpenConn[]
  sessionVisits?: Visit[]
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
  sessionUp?: number
  sessionDown?: number
}
type WatchState = {
  server_time: string
  timezone_offset: number
  telegram_configured: boolean
  rules: { tag: string; label: string }[]
  watched: WatchRow[]
  events: UserEvent[]
  events_rev?: string
}
type LiveFrame = { server_time: string; events_rev: string; watched: WatchRow[] }

/** A frame is due at least every 5 s; this long without one means the stream died. */
const STALE_MS = 12_000

/**
 * `admin/watch` once, then the live stream. A dropped stream is reopened, and
 * polled meanwhile; the events (sessions) are fetched again whenever a frame
 * says they changed.
 */
function useWatchLive() {
  const [data, setData] = useState<WatchState | null>(null)
  const [error, setError] = useState("")
  const [frameAt, setFrameAt] = useState(0)
  const [streaming, setStreaming] = useState(false)
  const rev = useRef<string | undefined>(undefined)

  const load = useCallback(() => {
    api<WatchState>(`${SB}/watch`)
      .then((d) => {
        rev.current = d.events_rev
        setData(d)
        setFrameAt(Date.now())
        setError("")
      })
      .catch((e: Error) => setError(e.message))
  }, [])

  useEffect(() => {
    load()
    let socket: WebSocket | null = null
    let poll: ReturnType<typeof setInterval> | null = null
    let silent: ReturnType<typeof setTimeout> | null = null
    let retry: ReturnType<typeof setTimeout> | null = null
    let stopped = false
    const startPoll = () => {
      poll ??= setInterval(() => { if (!document.hidden) load() }, 5000)
    }
    const stopPoll = () => {
      if (poll) clearInterval(poll)
      poll = null
    }
    const connect = () => {
      if (stopped) return
      const url = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/api${SB}/watch/stream`
      let opened: WebSocket
      try {
        opened = new WebSocket(url)
      } catch {
        startPoll()
        return
      }
      socket = opened
      const arm = () => {
        if (silent) clearTimeout(silent)
        silent = setTimeout(() => {
          setStreaming(false)
          startPoll()
          opened.onclose = null
          opened.close()
          retry = setTimeout(connect, 1000)
        }, STALE_MS)
      }
      arm()
      opened.onmessage = (event) => {
        let frame: LiveFrame
        try {
          frame = JSON.parse(event.data)
        } catch {
          return
        }
        arm()
        stopPoll()
        setStreaming(true)
        setFrameAt(Date.now())
        setError("")
        setData((prev) => (prev ? { ...prev, watched: frame.watched, server_time: frame.server_time } : prev))
        if (frame.events_rev !== rev.current) {
          rev.current = frame.events_rev
          load()
        }
      }
      opened.onclose = () => {
        if (silent) clearTimeout(silent)
        setStreaming(false)
        startPoll()
        if (!stopped) retry = setTimeout(connect, 3000)
      }
    }
    connect()
    return () => {
      stopped = true
      if (socket) {
        socket.onclose = null
        socket.close()
      }
      stopPoll()
      if (silent) clearTimeout(silent)
      if (retry) clearTimeout(retry)
    }
  }, [load])

  return { data, error, frameAt, streaming, reload: load }
}

/** Re-renders every second, for the clocks. */
function useTick() {
  const [, set] = useState(0)
  useEffect(() => {
    const timer = setInterval(() => set((n) => n + 1), 1000)
    return () => clearInterval(timer)
  }, [])
}

// ---- formatting --------------------------------------------------------------

const HOUR = 3_600_000
const DAY = 24 * HOUR

const ms = (iso: string | null | undefined) => (iso ? new Date(iso).getTime() : NaN)

/** Wall-clock parts in the hub's statistics time zone. */
function wall(t: number, tz: number) {
  return new Date(t + tz * HOUR)
}
const pad = (n: number) => String(n).padStart(2, "0")
const clockOf = (t: number, tz: number) => {
  const d = wall(t, tz)
  return `${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}`
}
/** Start of the day containing `t`, as a UTC instant. */
const dayStart = (t: number, tz: number) => Math.floor((t + tz * HOUR) / DAY) * DAY - tz * HOUR

const WEEK = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"]
function dayTitle(start: number, today: number, tz: number) {
  const d = wall(start, tz)
  const date = `${d.getUTCMonth() + 1}月${d.getUTCDate()}日`
  if (start === today) return `今天 · ${date}`
  if (start === today - DAY) return `昨天 · ${date}`
  return `${date} ${WEEK[d.getUTCDay()]}`
}

/** "1 小时 18 分", "41 分钟", "48 秒". */
function duration(span: number) {
  const s = Math.max(0, Math.round(span / 1000))
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  if (h > 0) return `${h} 小时${m ? ` ${m} 分` : ""}`
  if (m > 0) return `${m} 分钟`
  return `${s} 秒`
}

function ago(t: number, now: number) {
  const s = Math.max(0, Math.round((now - t) / 1000))
  if (s < 60) return "刚刚"
  if (s < 3600) return `${Math.floor(s / 60)} 分钟前`
  if (s < 86400) return `${Math.floor(s / 3600)} 小时前`
  return `${Math.floor(s / 86400)} 天前`
}

/** A size split into number and unit, for the large figures. */
function sized(n: number) {
  const text = bytes(n)
  const i = text.lastIndexOf(" ")
  return [text.slice(0, i), text.slice(i + 1)] as const
}

// Second-level suffixes under which a site is the third label (bbc.co.uk).
const SUFFIX2 = new Set([
  "co.jp", "ne.jp", "or.jp", "ac.jp", "go.jp", "com.cn", "net.cn", "org.cn", "gov.cn", "edu.cn", "com.hk", "org.hk",
  "net.hk", "com.tw", "org.tw", "net.tw", "co.uk", "org.uk", "ac.uk", "com.au", "net.au", "org.au", "co.kr", "or.kr",
  "com.sg", "com.my", "co.nz", "com.br", "co.in", "co.th", "com.vn", "com.ph", "co.id", "com.tr", "com.mx", "com.ar",
])

/** The site a host belongs to: googlevideo.com for rr3---sn-xx.googlevideo.com. */
export function siteOf(raw: string) {
  let host = raw.trim().toLowerCase()
  if (host.startsWith("[")) return host.slice(1, host.indexOf("]") > 0 ? host.indexOf("]") : undefined)
  const colon = host.lastIndexOf(":")
  if (colon > 0 && host.indexOf(":") === colon) host = host.slice(0, colon)
  if (/^[\d.]+$/.test(host) || host.includes(":")) return host
  const parts = host.split(".").filter(Boolean)
  if (parts.length <= 2) return parts.join(".")
  const two = parts.slice(-2).join(".")
  return SUFFIX2.has(two) ? parts.slice(-3).join(".") : two
}

// Connections a phone makes by itself: push, time, certificate checks,
// captive-portal and DNS probes. Hidden unless asked for.
const BACKGROUND = [
  "push.apple.com", "time.apple.com", "time-ios.apple.com", "time-macos.apple.com", "mesu.apple.com", "gdmf.apple.com",
  "ocsp.apple.com", "ocsp2.apple.com", "valid.apple.com", "crl.apple.com", "captive.apple.com", "gsp-ssl.ls.apple.com",
  "configuration.apple.com", "init.itunes.apple.com", "bag.itunes.apple.com", "gateway.icloud.com",
  "keyvalueservice.icloud.com", "init.push.apple.com", "mtalk.google.com", "connectivitycheck.gstatic.com",
  "connectivitycheck.android.com", "clients3.google.com", "time.android.com", "android.clients.google.com",
  "ocsp.pki.goog", "ocsp.digicert.com", "ocsp.sectigo.com", "ocsp.globalsign.com", "crl3.digicert.com",
  "dns.google", "cloudflare-dns.com", "dns.alidns.com", "doh.pub", "time.windows.com", "msftconnecttest.com",
  "ctldl.windowsupdate.com",
]
export function isBackground(host: string) {
  const h = host.toLowerCase().replace(/:\d+$/, "")
  return BACKGROUND.some((b) => h === b || h.endsWith(`.${b}`))
}

type Site = { site: string; count: number; first: string; last: string }

/** Visits merged by site, most visited first, background split off. */
function bySite(visits: Visit[]) {
  const map = new Map<string, Site>()
  let background = 0
  const hidden = new Map<string, Site>()
  for (const v of visits) {
    const target = isBackground(v.host) ? hidden : map
    if (target === hidden) background++
    const site = siteOf(v.host)
    const s = target.get(site)
    if (!s) target.set(site, { site, count: v.count, first: v.first, last: v.last })
    else {
      s.count += v.count
      if (v.first < s.first) s.first = v.first
      if (v.last > s.last) s.last = v.last
    }
  }
  const order = (a: Site, b: Site) => b.count - a.count || b.last.localeCompare(a.last)
  return { sites: [...map.values()].sort(order), hidden: [...hidden.values()].sort(order), background }
}

// Colours per server, in order of appearance: calm, distinct in both themes.
const NODE_COLORS = [
  "bg-emerald-600 dark:bg-emerald-400",
  "bg-indigo-400 dark:bg-indigo-300",
  "bg-amber-500 dark:bg-amber-300",
  "bg-sky-500 dark:bg-sky-300",
  "bg-rose-400 dark:bg-rose-300",
]

// ---- sessions ----------------------------------------------------------------

type Session = {
  key: string
  label: string
  nodeName: string
  tag: string
  start: number
  end: number | null
  conn: number
  traffic: number | null
  visits: Visit[]
  note: string
  source: string
}

function sessionsOf(data: WatchState, now: number): Session[] {
  const list: Session[] = []
  for (const e of data.events) {
    if (e.kind !== "offline" || !e.sessionStart) continue
    list.push({
      key: e.id,
      label: e.label,
      nodeName: e.nodeName,
      tag: e.tag,
      start: ms(e.sessionStart),
      end: ms(e.time),
      conn: e.sessionConn ?? 0,
      traffic: e.sessionUp != null || e.sessionDown != null ? (e.sessionUp ?? 0) + (e.sessionDown ?? 0) : null,
      visits: e.visits ?? [],
      note: e.note ?? "",
      source: e.source,
    })
  }
  for (const r of data.watched) {
    if (!r.online || !r.sessionStart) continue
    list.push({
      key: `live:${r.nodeId}:${r.tag}`,
      label: r.label,
      nodeName: r.nodeName,
      tag: r.tag,
      start: ms(r.sessionStart),
      end: null,
      conn: r.sessionConn,
      traffic: (r.sessionUp ?? 0) + (r.sessionDown ?? 0),
      visits: r.sessionVisits ?? [],
      note: "",
      source: r.lastSource,
    })
  }
  return list
    .filter((s) => Number.isFinite(s.start))
    .map((s) => ({ ...s, end: s.end != null && Number.isFinite(s.end) ? Math.min(s.end, now) : s.end }))
    .sort((a, b) => (b.end ?? Infinity) - (a.end ?? Infinity) || b.start - a.start)
}

/** Time a session spent inside [from, to). */
const overlap = (s: Session, from: number, to: number, now: number) =>
  Math.max(0, Math.min(s.end ?? now, to) - Math.max(s.start, from))

function dayStats(sessions: Session[], from: number, now: number) {
  const to = from + DAY
  const inDay = sessions.filter((s) => overlap(s, from, to, now) > 0 || (s.end === null && s.start < to))
  return {
    count: inDay.length,
    online: inDay.reduce((sum, s) => sum + overlap(s, from, to, now), 0),
    traffic: inDay.reduce((sum, s) => sum + (s.traffic ?? 0), 0),
    sessions: inDay,
  }
}

// ---- page --------------------------------------------------------------------

export function ConnWatch() {
  const { data, error, frameAt, streaming, reload } = useWatchLive()
  const [settings, setSettings] = useState(false)
  const [logOf, setLogOf] = useState<string | null>(null)
  useTick()

  if (!data) {
    return error ? <p className="text-sm text-destructive">{error}</p> : <p className="text-sm text-muted-foreground">加载中…</p>
  }
  // The hub's clock, advanced by the time since its last frame, so the
  // counters keep running between frames and do not depend on this machine's clock.
  const now = ms(data.server_time) + (Date.now() - frameAt)
  const tz = Number.isFinite(data.timezone_offset) ? data.timezone_offset : 8
  const age = Date.now() - frameAt
  const labels = [...new Set([...data.rules.map((r) => r.label), ...data.watched.map((r) => r.label)])]
  const sessions = sessionsOf(data, now)

  return (
    <div className="mx-auto max-w-[1040px] space-y-6">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h2 className="text-[22px] font-semibold tracking-wide">连接监控</h2>
          <p className="mt-0.5 text-[13px] text-muted-foreground">按入站监控指定用户的上线、下线和访问</p>
        </div>
        <div className="flex items-center gap-5 text-[13px] text-muted-foreground">
          {streaming && age < STALE_MS ? (
            <span className="flex items-center gap-1.5" title="页面跟着 sb-agent 的上报实时更新">
              <span className="size-[7px] rounded-full bg-emerald-600 dark:bg-emerald-400" />
              实时 · {age < 1500 ? "刚刚" : `${Math.round(age / 1000)} 秒前`}
            </span>
          ) : (
            <span className="flex items-center gap-1.5 text-amber-700 dark:text-amber-400" title="实时连接断开，正在重连，期间每 5 秒刷新">
              <span className="size-[7px] rounded-full bg-amber-500" />
              重新连接中
            </span>
          )}
          <button className="hover:text-foreground" onClick={() => setSettings(true)}>设置</button>
        </div>
      </div>
      {error && <p className="text-sm text-destructive">{error}</p>}

      {!data.rules.length ? (
        <Empty text="还没有配置监控用户。" action={() => setSettings(true)} />
      ) : !data.watched.length ? (
        <Empty text="已配置监控用户，但还没有节点上报对应的入站。确认落地机装了 sb-agent，且 sing-box 配置里有该 tag。" action={() => setSettings(true)} />
      ) : (
        labels.map((label) => (
          <UserCard
            key={label}
            label={label}
            rows={data.watched.filter((r) => r.label === label)}
            sessions={sessions.filter((s) => s.label === label)}
            now={now}
            tz={tz}
            onLog={() => setLogOf(label)}
          />
        ))
      )}

      <SessionList sessions={sessions} multiUser={labels.length > 1} now={now} tz={tz} />

      <Dialog open={settings} onOpenChange={setSettings}>
        <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-2xl">
          <DialogHeader>
            <DialogTitle>监控设置</DialogTitle>
            <DialogDescription>
              Telegram {data.telegram_configured ? "已配置" : "未配置"}。这里的机器人发连接监控的上线、下线通知，以及 sing-box 停止、sb-agent 失联的提醒；和「通知」页的整机告警分开。
            </DialogDescription>
          </DialogHeader>
          <WatchSettings embedded onSaved={reload} />
        </DialogContent>
      </Dialog>
      {logOf && <UserLogDialog user={logOf} onClose={() => setLogOf(null)} />}
    </div>
  )
}

function Empty({ text, action }: { text: string; action: () => void }) {
  return (
    <div className="rounded-2xl border bg-card p-8 text-sm text-muted-foreground">
      {text}
      <Button variant="link" className="h-auto px-1.5 py-0" onClick={action}>打开设置</Button>
    </div>
  )
}

const Sep = () => <span className="mx-1.5 text-muted-foreground/50">·</span>
const Dot = ({ className }: { className: string }) => <span className={`inline-block size-[7px] shrink-0 rounded-full ${className}`} />

// ---- one user ----------------------------------------------------------------

function UserCard({ label, rows, sessions, now, tz, onLog }: {
  label: string
  rows: WatchRow[]
  sessions: Session[]
  now: number
  tz: number
  onLog: () => void
}) {
  const active = rows.find((r) => r.online) ?? rows.find((r) => r.pending)
  const others = rows.filter((r) => r !== active)
  const last = [...rows].sort((a, b) => (ms(b.lastActive) || 0) - (ms(a.lastActive) || 0))[0]
  const today = dayStart(now, tz)
  const stats = dayStats(sessions, today, now)
  const nodes = [...new Set([...stats.sessions.map((s) => s.nodeName), ...(active ? [active.nodeName] : [])])]
  const color = (node: string) => NODE_COLORS[Math.max(0, nodes.indexOf(node)) % NODE_COLORS.length]

  const start = active?.online ? ms(active.sessionStart) : NaN
  const elapsed = Number.isFinite(start) ? Math.max(0, now - start) : 0
  const h = Math.floor(elapsed / HOUR)
  const m = Math.floor((elapsed % HOUR) / 60_000)
  const s = Math.floor((elapsed % 60_000) / 1000)

  return (
    <div className="overflow-hidden rounded-2xl border bg-card">
      <div className="px-5 pt-6 pb-5 sm:px-9 sm:pt-8 sm:pb-7">
        <div className="flex flex-wrap items-start justify-between gap-x-6 gap-y-4">
          <div className="min-w-0">
            <div className="text-[26px] leading-tight font-semibold tracking-wide sm:text-[28px]">{label}</div>
            <div className="mt-1.5 flex flex-wrap items-center text-sm">
              {active?.online ? (
                <>
                  <Dot className="mr-2 bg-emerald-600 dark:bg-emerald-400" />
                  在线<Sep />{active.nodeName}<Sep /><span className="text-muted-foreground">{active.tag}</span>
                </>
              ) : active?.pending ? (
                <>
                  <Dot className="mr-2 bg-emerald-600/50 dark:bg-emerald-400/50" />
                  <span>刚连上，确认中</span><Sep />{active.nodeName}
                </>
              ) : (
                <>
                  <Dot className="mr-2 bg-muted-foreground/40" />
                  <span className="text-muted-foreground">
                    离线{last?.lastActive ? `，上次在 ${last.nodeName}，${ago(ms(last.lastActive), now)}` : ""}
                  </span>
                </>
              )}
            </div>
          </div>
          {active?.online && (
            <div className="sm:text-right">
              <div className="text-[30px] leading-none font-light tabular-nums sm:text-[40px]">
                {h > 0 && <>{h}<Unit>时</Unit></>}
                {h > 0 ? pad(m) : m}<Unit>分</Unit>
                {h === 0 && <>{pad(s)}<Unit last>秒</Unit></>}
              </div>
              <div className="mt-1.5 text-[13px] text-muted-foreground tabular-nums">{clockOf(start, tz)} 上线</div>
            </div>
          )}
        </div>

        {active && (
          <div className="mt-6 flex flex-wrap gap-x-11 gap-y-4 border-t pt-5 sm:mt-7">
            <Fact label="下载" value={(active.rateDown ?? 0) > 0 ? sized(active.rateDown ?? 0) : ["0", "B"]} suffix="/s" />
            <Fact label="上传" value={(active.rateUp ?? 0) > 0 ? sized(active.rateUp ?? 0) : ["0", "B"]} suffix="/s" />
            <Fact label="本次流量" value={sized((active.sessionUp ?? 0) + (active.sessionDown ?? 0))} />
            <Fact label="正在连接" value={[String(active.live), "条"]} />
          </div>
        )}
      </div>

      <div className="border-t px-5 py-5 sm:px-9 sm:py-6">
        <div className="flex flex-wrap items-center justify-between gap-2 text-[13px]">
          <span>
            <span className="font-semibold">今天</span>
            <span className="ml-3.5 text-muted-foreground">
              {stats.count ? (
                <>上线 {stats.count} 次<Sep />在线 {duration(stats.online)}{stats.traffic > 0 && <><Sep />{bytes(stats.traffic)}</>}</>
              ) : "还没有上线"}
            </span>
          </span>
          {nodes.length > 0 && (
            <span className="hidden gap-4 text-muted-foreground sm:flex">
              {nodes.map((n) => <span key={n} className="flex items-center gap-1.5"><Dot className={color(n)} />{n}</span>)}
            </span>
          )}
        </div>
        <div className="relative mt-4 mb-2 h-2 rounded bg-muted">
          {stats.sessions.map((sn) => {
            const from = Math.max(sn.start, today)
            const to = Math.min(sn.end ?? now, today + DAY)
            const left = ((from - today) / DAY) * 100
            const width = Math.max(((to - from) / DAY) * 100, 0.5)
            return (
              <span
                key={sn.key}
                className={`absolute inset-y-0 rounded ${color(sn.nodeName)}`}
                style={{ left: `${left}%`, width: `${width}%` }}
                title={`${clockOf(sn.start, tz)} – ${sn.end === null ? "现在" : clockOf(sn.end, tz)} · ${duration((sn.end ?? now) - sn.start)} · ${sn.nodeName}`}
              />
            )
          })}
          <span className="absolute -inset-y-1.5 w-0.5 rounded bg-foreground" style={{ left: `${((now - today) / DAY) * 100}%` }} />
        </div>
        <div className="flex justify-between text-xs text-muted-foreground/70 tabular-nums">
          <span>0 时</span><span>6</span><span>12</span><span>18</span><span>24</span>
        </div>
      </div>

      {active?.online && <NowBlock row={active} now={now} />}

      {rows.length > (active ? 1 : 0) && (
        <div className="flex flex-wrap items-center justify-between gap-3 border-t px-5 py-4 text-[13px] text-muted-foreground sm:px-9">
          <span className="min-w-0">
            {active ? `另有 ${others.length} 个入站未在使用` : `${rows.length} 个入站`}
            <span className="hidden sm:inline">
              ：{(active ? others : rows).map((r, i) => (
                <span key={`${r.nodeId}:${r.tag}`}>
                  {i > 0 && "、"}{r.nodeName} {r.tag}
                  {!r.nodeOnline ? "（节点离线）" : r.lastActive ? `（${ago(ms(r.lastActive), now)}）` : ""}
                </span>
              ))}
            </span>
          </span>
          <button className="flex items-center hover:text-foreground" onClick={onLog}>连接日志<ChevronRight className="size-3.5" /></button>
        </div>
      )}
    </div>
  )
}

function Unit({ children, last }: { children: string; last?: boolean }) {
  return <span className={`text-base font-normal text-muted-foreground ${last ? "ml-0.5" : "mx-1.5 ml-0.5"}`}>{children}</span>
}

function Fact({ label, value, suffix = "" }: { label: string; value: readonly [string, string]; suffix?: string }) {
  return (
    <div>
      <div className="text-xs tracking-wider text-muted-foreground">{label}</div>
      <div className="mt-0.5 text-xl font-medium tabular-nums">
        {value[0]}<span className="ml-1 text-[13px] font-normal text-muted-foreground">{value[1]}{suffix}</span>
      </div>
    </div>
  )
}

function NowBlock({ row, now }: { row: WatchRow; now: number }) {
  const open = (row.open ?? []).filter((c) => !isBackground(c.dst))
  const { sites, background } = bySite(row.sessionVisits ?? [])
  return (
    <div className="grid gap-6 border-t px-5 py-5 sm:grid-cols-2 sm:gap-12 sm:px-9 sm:py-6">
      <div>
        <div className="mb-1 text-xs tracking-wider text-muted-foreground">正在访问</div>
        {open.length === 0 ? (
          <p className="py-2 text-sm text-muted-foreground">{row.liveTracked ? "暂时没有打开的连接" : "这台没开 clash_api，看不到正在打开的连接"}</p>
        ) : (
          <>
            {open.slice(0, 6).map((c, i) => (
              <Item key={`${c.dst}-${c.start}-${i}`} name={c.dst} value={duration(now - c.start)} title={`${c.net.toUpperCase()} · ↓ ${bytes(c.down)} ↑ ${bytes(c.up)}`} />
            ))}
            {open.length > 6 && <p className="pt-2 text-[13px] text-muted-foreground">还有 {open.length - 6} 条</p>}
          </>
        )}
      </div>
      <div>
        <div className="mb-1 flex justify-between text-xs tracking-wider text-muted-foreground">
          <span>本次常用</span>
          {background > 0 && <span className="tracking-normal">已隐藏 {background} 个后台连接</span>}
        </div>
        {sites.length === 0 ? (
          <p className="py-2 text-sm text-muted-foreground">还没有访问记录</p>
        ) : (
          sites.slice(0, 6).map((s) => <Item key={s.site} name={s.site} value={`${s.count} 次`} />)
        )}
      </div>
    </div>
  )
}

function Item({ name, value, title }: { name: string; value: string; title?: string }) {
  return (
    <div className="flex justify-between gap-4 border-b py-2.5 text-sm last:border-b-0" title={title}>
      <span className="truncate">{name}</span>
      <span className="shrink-0 text-muted-foreground tabular-nums">{value}</span>
    </div>
  )
}

// ---- session list ------------------------------------------------------------

const PAGE = 30

function SessionList({ sessions, multiUser, now, tz }: { sessions: Session[]; multiUser: boolean; now: number; tz: number }) {
  const [node, setNode] = useState("")
  const [query, setQuery] = useState("")
  const [searching, setSearching] = useState(false)
  const [open, setOpen] = useState<string | null>(null)
  const [shown, setShown] = useState(PAGE)

  const nodes = useMemo(() => [...new Set(sessions.map((s) => s.nodeName))].sort(), [sessions])
  const q = query.trim().toLowerCase()
  const list = sessions.filter(
    (s) => (!node || s.nodeName === node) && (!q || s.visits.some((v) => v.host.toLowerCase().includes(q))),
  )
  const today = dayStart(now, tz)
  const days: { start: number; items: Session[] }[] = []
  for (const s of list.slice(0, shown)) {
    const d = dayStart(s.end ?? now, tz)
    const group = days.find((g) => g.start === d)
    if (group) group.items.push(s)
    else days.push({ start: d, items: [s] })
  }

  return (
    <div className="rounded-2xl border bg-card pb-2">
      <div className="flex flex-wrap items-end justify-between gap-4 px-5 pt-6 pb-2 sm:px-9">
        <h3 className="text-[17px] font-semibold">会话记录</h3>
        <div className="flex flex-wrap items-center gap-5 text-[13px] text-muted-foreground">
          {nodes.length > 1 && ["", ...nodes].map((n) => (
            <button
              key={n || "all"}
              onClick={() => setNode(n)}
              className={`relative pb-1 ${node === n ? "font-medium text-foreground after:absolute after:inset-x-0 after:-bottom-0.5 after:h-0.5 after:rounded after:bg-foreground" : "hover:text-foreground"}`}
            >
              {n || "全部"}
            </button>
          ))}
          {searching ? (
            <span className="flex items-center gap-1">
              <Input
                autoFocus
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                placeholder="网站或域名"
                className="h-7 w-40 text-[13px]"
              />
              <button title="清除" onClick={() => { setQuery(""); setSearching(false) }} className="hover:text-foreground">
                <X className="size-4" />
              </button>
            </span>
          ) : (
            <button onClick={() => setSearching(true)} className="flex items-center gap-1 pb-1 hover:text-foreground">
              <Search className="size-3.5" />搜索
            </button>
          )}
        </div>
      </div>

      {days.length === 0 ? (
        <p className="px-5 py-6 text-sm text-muted-foreground sm:px-9">{q || node ? "没有符合的会话" : "暂无会话"}</p>
      ) : (
        days.map((g) => {
          const st = dayStats(list, g.start, now)
          return (
            <div key={g.start}>
              <div className="flex justify-between px-5 pt-5 pb-1.5 text-[13px] sm:px-9">
                <b className="font-semibold">{dayTitle(g.start, today, tz)}</b>
                <span className="text-muted-foreground tabular-nums">{st.count} 次 · {duration(st.online)}</span>
              </div>
              {g.items.map((s) => (
                <SessionRow
                  key={s.key}
                  s={s}
                  multiUser={multiUser}
                  open={open === s.key}
                  onToggle={() => setOpen(open === s.key ? null : s.key)}
                  now={now}
                  tz={tz}
                  query={q}
                />
              ))}
            </div>
          )
        })
      )}
      {list.length > shown && (
        <div className="border-t px-5 py-3 text-center sm:px-9">
          <button className="text-[13px] text-muted-foreground hover:text-foreground" onClick={() => setShown(shown + PAGE)}>
            显示更早的 {Math.min(PAGE, list.length - shown)} 次
          </button>
        </div>
      )}
    </div>
  )
}

function SessionRow({ s, multiUser, open, onToggle, now, tz, query }: {
  s: Session
  multiUser: boolean
  open: boolean
  onToggle: () => void
  now: number
  tz: number
  query: string
}) {
  const ongoing = s.end === null
  const { sites } = bySite(s.visits)
  const startsEarlier = dayStart(s.start, tz) !== dayStart(s.end ?? now, tz)
  return (
    <>
      <button
        onClick={onToggle}
        className="grid w-full grid-cols-[1fr_auto] items-baseline gap-x-5 gap-y-0.5 border-t px-5 py-3.5 text-left transition-colors hover:bg-muted/40 sm:grid-cols-[150px_1fr_auto] sm:px-9"
      >
        <span className={`col-start-1 row-start-1 text-base font-medium tabular-nums ${ongoing ? "text-emerald-700 dark:text-emerald-400" : ""}`}>
          {startsEarlier && <span className="text-[13px] font-normal text-muted-foreground">前一天 </span>}
          {clockOf(s.start, tz)}
          <span className="font-normal text-muted-foreground"> – {ongoing ? "现在" : clockOf(s.end ?? now, tz)}</span>
        </span>
        <span className="col-span-2 col-start-1 row-start-2 text-sm sm:col-span-1 sm:col-start-2 sm:row-start-1">
          {multiUser && <>{s.label}<Sep /></>}
          {duration((s.end ?? now) - s.start)}
          <span className="text-[13px] text-muted-foreground">
            <Sep />{s.nodeName}<Sep />{sites.length} 个网站
            {s.traffic != null && s.traffic > 0 && <><Sep />{bytes(s.traffic)}</>}
            {s.note && <><Sep /><span className="text-amber-700 dark:text-amber-400">{s.note}</span></>}
          </span>
        </span>
        <span className="col-start-2 row-start-1 text-right text-[13px] text-muted-foreground sm:col-start-3">{ongoing ? "进行中" : open ? "收起" : "展开"}</span>
      </button>
      {open && <SessionDetail s={s} tz={tz} query={query} />}
    </>
  )
}

function SessionDetail({ s, tz, query }: { s: Session; tz: number; query: string }) {
  const [all, setAll] = useState(false)
  const [showHidden, setShowHidden] = useState(false)
  const { sites, hidden, background } = bySite(s.visits)
  const rows = showHidden ? [...sites, ...hidden] : sites
  const visible = all || query ? rows : rows.slice(0, 8)
  const time = (iso: string) => clockOf(ms(iso), tz)
  return (
    <div className="px-5 pt-1 pb-5 sm:pr-9 sm:pl-[206px]">
      {rows.length === 0 ? (
        <p className="text-[13px] text-muted-foreground">这次没有记下访问的网站</p>
      ) : (
        <div className="grid grid-cols-[1fr_auto_auto] text-[13.5px]">
          <div className="border-b pb-1.5 text-xs text-muted-foreground">网站</div>
          <div className="border-b pb-1.5 pl-4 text-right text-xs text-muted-foreground">次数</div>
          <div className="border-b pb-1.5 pl-4 text-right text-xs text-muted-foreground">时间</div>
          {visible.map((v) => {
            const hit = query && v.site.includes(query)
            const muted = hidden.includes(v)
            return (
              <div key={v.site} className="contents">
                <div className={`truncate border-b py-1.5 ${muted ? "text-muted-foreground" : ""} ${hit ? "font-medium" : ""}`}>{v.site}</div>
                <div className="border-b py-1.5 pl-4 text-right tabular-nums">{v.count}</div>
                <div className="border-b py-1.5 pl-4 text-right whitespace-nowrap text-muted-foreground tabular-nums">
                  {time(v.first)}{time(v.last) !== time(v.first) && ` – ${time(v.last)}`}
                </div>
              </div>
            )
          })}
        </div>
      )}
      <div className="mt-2.5 flex flex-wrap gap-x-3 text-[13px] text-muted-foreground">
        {!all && !query && rows.length > 8 && (
          <button className="hover:text-foreground" onClick={() => setAll(true)}>还有 {rows.length - 8} 个网站 ›</button>
        )}
        {background > 0 && (
          <button className="hover:text-foreground" onClick={() => setShowHidden(!showHidden)}>
            {showHidden ? "隐藏系统后台连接" : `已隐藏 ${background} 个系统后台连接`}
          </button>
        )}
        {s.source && <span className="ml-auto">来源 {s.source.replace(/:\d+$/, "")}</span>}
      </div>
    </div>
  )
}
