// 多开会话 · 会话列表 + 全部运行控制。
//
// 号池页只负责账号本身（增删改）；这里负责"跑起来"：每个 slot 选号、
// 选目标场次、开关浏览器、开始/停止对局，以及顶部的全局开关与一键按钮。
// 后端每个 slot 是一个独立 SessionRuntime：独立浏览器/profile/CDP/日志/
// 历史/Bot/Autoplay/PoolDriver，单会话的操作不会牵动其他会话。

import { useCallback, useEffect, useRef, useState, type MutableRefObject } from 'react'
import {
  Activity,
  Monitor,
  Loader2,
  Play,
  Plus,
  RefreshCw,
  Square,
  Trash2,
  Users,
  X,
} from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { Switch } from '@/components/ui/switch'
import { toast } from '@/components/ui/sonner'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { invoke, listen } from '@/lib/tauri'
import type { AppConfig, EnterResult, SessionStatus } from '@/types'

type PrecheckConflict = { identity: string; slots: string[]; label: string }
type SessionPrecheck = {
  ok: boolean
  conflicts: PrecheckConflict[]
  accounts: { slot: string; account_id: string; label: string; identity: string }[]
}

const MAX_SESSIONS = 8
// Five Unity WebGL pages starting in the same tick makes a few CPU cores
// spike. Space the *start calls* by this much; do not wait for completion.
const BATCH_STAGGER_MS = 1200

function sleep(ms: number) {
  return new Promise<void>((resolve) => window.setTimeout(resolve, ms))
}

const ROOMS = [
  { value: 'copper', label: '铜之间' },
  { value: 'silver', label: '银之间' },
  { value: 'gold', label: '金之间' },
  { value: 'jade', label: '玉之间' },
  { value: 'throne', label: '王座之间' },
]
const MODES = [
  { value: 'mode4e', label: '四人東' },
  { value: 'mode4s', label: '四人南' },
  { value: 'mode3e', label: '三人東' },
  { value: 'mode3s', label: '三人南' },
]

function roomLabel(v: string) {
  return ROOMS.find((r) => r.value === v)?.label ?? v
}
function modeLabel(v: string) {
  return MODES.find((m) => m.value === v)?.label ?? v
}

function captureRunning(s: SessionStatus) {
  return s.capture_status?.state === 'running'
}

function stateLabel(s: SessionStatus): string {
  if (s.in_game) return '对局中'
  if (s.capture_status?.state === 'error') return '抓包错误'
  if (s.paused) return '已停止'
  if (captureRunning(s)) return s.autoplay ? '大厅 · 自动打牌' : '浏览器已开'
  if (s.autoplay) return '自动打牌'
  return '未启动'
}

function stateVariant(s: SessionStatus): 'default' | 'secondary' | 'destructive' | 'outline' {
  if (s.in_game) return 'default'
  if (s.capture_status?.state === 'error') return 'destructive'
  if (captureRunning(s)) return 'secondary'
  return 'outline'
}

export function Sessions() {
  const [rows, setRows] = useState<SessionStatus[]>([])
  const [accounts, setAccounts] = useState<{ id: string; label: string }[]>([])
  const [pick, setPick] = useState('')
  const [autoStart, setAutoStart] = useState(false)
  const [autoNext, setAutoNext] = useState(false)
  // Per-slot busy set: one session starting must never grey out the others.
  const [busy, setBusy] = useState<Set<string>>(new Set())
  // Slots queued by a one-key action but not started yet. Cancel invalidates
  // the batch token and clears this set so queued launches never fire later.
  // aogara: browser-open and game-start are SEPARATE batches with separate
  // tokens. They used to share one token, so cancelling a game start also
  // (unintentionally) had to double as cancelling a browser-open queue, and
  // the two queues fought over the same `queued` set. Now each has its own
  // token; `queued` still tracks membership for UI, but cancellation targets
  // only the relevant batch.
  const [queued, setQueued] = useState<Set<string>>(new Set())
  const queuedRef = useRef<Set<string>>(new Set())
  const batchToken = useRef(0)          // game start/stop batch
  const browserBatchToken = useRef(0)   // browser open batch
  const browserQueuedSlots = useRef<Set<string>>(new Set())
  const [openingAll, setOpeningAll] = useState(false)
  const [adding, setAdding] = useState(false)
  const [error, setError] = useState<string | null>(null)

  // Cheap refresh: session list only. Used by the 5s fallback timer and by
  // backend session-status events; never pulls the full config.
  const refreshSessions = useCallback(async () => {
    if (document.hidden) return
    try {
      const list = await invoke<SessionStatus[]>('list_sessions')
      setRows(list)
      setError(null)
    } catch (e) {
      setError(String(e))
    }
  }, [])

  // Full refresh: config/accounts/global switches. Only on mount, manual
  // refresh and actions that actually change config.
  const refresh = useCallback(async () => {
    await refreshSessions()
    try {
      const cfg = await invoke<AppConfig>('get_config')
      setAccounts(
        (cfg.pool?.accounts ?? []).map((a) => ({ id: a.id, label: a.label || a.id })),
      )
      setAutoStart(Boolean(cfg.multi?.start_capture_on_launch))
      setAutoNext(Boolean(cfg.pool?.auto_next))
      setError(null)
    } catch (e) {
      setError(String(e))
    }
  }, [refreshSessions])

  useEffect(() => {
    void refresh()
    let stopped = false
    let unlisten: (() => void) | undefined
    void listen<{ slot?: string }>('session-status', () => {
      void refreshSessions()
    }).then((fn) => {
      if (stopped) fn()
      else unlisten = fn
    })
    const timer = window.setInterval(() => void refreshSessions(), 5000)
    const onVisible = () => {
      if (!document.hidden) void refreshSessions()
    }
    document.addEventListener('visibilitychange', onVisible)
    return () => {
      stopped = true
      window.clearInterval(timer)
      document.removeEventListener('visibilitychange', onVisible)
      unlisten?.()
    }
  }, [refresh, refreshSessions])

  const markBusy = (slot: string, on: boolean) => {
    setBusy((prev) => {
      const next = new Set(prev)
      if (on) next.add(slot)
      else next.delete(slot)
      return next
    })
  }

  const markQueued = (slot: string, on: boolean) => {
    // Keep a ref in lockstep with state: async callbacks (openBrowser etc.)
    // check queuedRef.current synchronously, so a stale React state snapshot
    // can't let a cancelled slot slip through (the P-05 race).
    const next = new Set(queuedRef.current)
    if (on) next.add(slot)
    else next.delete(slot)
    queuedRef.current = next
    setQueued(next)
  }

  const clearQueued = () => {
    queuedRef.current = new Set()
    setQueued(new Set())
  }

  // Stagger only the launch moments. Actions themselves stay concurrent so a
  // slow session never blocks the rest of the batch.
  const runStaggered = async (
    targets: SessionStatus[],
    label: string,
    action: (row: SessionStatus) => Promise<void> | void,
    tokenRef: MutableRefObject<number> = batchToken,
  ) => {
    if (!targets.length) return
    const token = ++tokenRef.current
    targets.forEach((row) => markQueued(row.slot, true))
    toast.info(`正在${label} ${targets.length} 个会话…（错峰启动）`)
    for (const [index, row] of targets.entries()) {
      if (tokenRef.current !== token) return
      markQueued(row.slot, false)
      void action(row)
      if (index < targets.length - 1) await sleep(BATCH_STAGGER_MS)
    }
  }

  const start = async (row: SessionStatus) => {
    if (queuedRef.current.has(row.slot)) {
      toast.info(`${row.slot}: 已在一键启动队列中`)
      return
    }
    if (!row.selected_id) {
      toast.error(`${row.slot} 未选择账号`)
      return
    }
    markBusy(row.slot, true)
    try {
      const r = await invoke<EnterResult>('session_pool_start', {
        slot: row.slot,
        accountId: row.selected_id,
        room: row.target_room || 'copper',
        mode: row.target_mode || 'mode4e',
        dryRun: false,
      })
      if (r.entered) toast.success(`${row.slot}: ${r.message}`)
      else toast.error(`${row.slot}: ${r.message}`)
      await refresh()
    } catch (e) {
      toast.error(`${row.slot}: ${String(e)}`)
    } finally {
      markBusy(row.slot, false)
    }
  }

  const stop = async (row: SessionStatus) => {
    markBusy(row.slot, true)
    try {
      const r = await invoke<{ stop_after_game: boolean }>('session_pool_stop', {
        slot: row.slot,
      })
      toast.success(
        r.stop_after_game ? `${row.slot}: 打完本局后停止代打、释放机器人` : `${row.slot}: 已停止代打`,
      )
      await refresh()
    } catch (e) {
      toast.error(String(e))
    } finally {
      markBusy(row.slot, false)
    }
  }

  const openBrowser = async (row: SessionStatus) => {
    if (queuedRef.current.has(row.slot)) {
      toast.info(`${row.slot}: 已在一键启动队列中`)
      return
    }
    markBusy(row.slot, true)
    try {
      await invoke('session_start_capture', { slot: row.slot })
      toast.success(`${row.slot}: 浏览器已打开`)
      await refresh()
    } catch (e) {
      toast.error(`${row.slot}: ${String(e)}`)
    } finally {
      markBusy(row.slot, false)
    }
  }

  const closeBrowser = async (row: SessionStatus): Promise<boolean> => {
    markBusy(row.slot, true)
    try {
      await invoke('session_stop_capture', { slot: row.slot })
      toast.success(`${row.slot}: 浏览器已关闭`)
      await refresh()
      return true
    } catch (e) {
      toast.error(`${row.slot}: ${String(e)}`)
      return false
    } finally {
      markBusy(row.slot, false)
    }
  }

  const toggleBrowser = (row: SessionStatus, on: boolean) => {
    if (on) void openBrowser(row)
    else void closeBrowser(row)
  }

  const patchSession = async (slot: string, patch: Record<string, string>) => {
    markBusy(slot, true)
    try {
      await invoke('update_session_settings', { slot, ...patch })
      await refresh()
    } catch (e) {
      toast.error(`${slot}: ${String(e)}`)
    } finally {
      markBusy(slot, false)
    }
  }

  const toggleAutoStart = async (v: boolean) => {
    setAutoStart(v)
    try {
      await invoke('set_start_capture_on_launch', { enabled: v })
      toast.success(v ? '启动 Aogara 时会打开浏览器' : '启动 Aogara 时不再打开浏览器')
      await refresh()
    } catch (e) {
      toast.error(String(e))
      setAutoStart(!v)
    }
  }

  const toggleAutoNext = async (v: boolean) => {
    setAutoNext(v)
    try {
      await invoke('set_pool_auto_next', { enabled: v })
      toast.success(
        v ? '自动连打已开启（对所有会话生效）' : '自动连打已关闭：各会话打完当前局即停，释放机器人',
      )
      await refresh()
    } catch (e) {
      toast.error(String(e))
      setAutoNext(!v)
    }
  }

  // ---- 一键操作：每个会话独立并发，互不阻塞 ----
  const openAll = () => {
    const targets = rows.filter(
      (r) => !captureRunning(r) && !busy.has(r.slot) && !queuedRef.current.has(r.slot),
    )
    if (!targets.length) {
      toast.info('所有会话的浏览器都已打开或排队')
      return
    }
    setOpeningAll(true)
    browserQueuedSlots.current = new Set(targets.map((r) => r.slot))
    void runStaggered(targets, '打开浏览器', openBrowser, browserBatchToken).finally(() => {
      browserQueuedSlots.current = new Set()
      setOpeningAll(false)
    })
  }

  // aogara: cancel ONLY the browser-open queue. Uses its own token so a
  // game-start batch (and in-play sessions) are untouched — that coupling
  // was the P-05 bug. Already-launched browsers can't be un-launched; this
  // stops the remaining queued slots.
  const cancelOpenAll = () => {
    browserBatchToken.current += 1
    // Remove ONLY the slots this browser batch queued, leaving any
    // game-start queue intact.
    for (const slot of browserQueuedSlots.current) {
      const next = new Set(queuedRef.current)
      next.delete(slot)
      queuedRef.current = next
      setQueued(next)
    }
    browserQueuedSlots.current = new Set()
    setOpeningAll(false)
    toast.info('已取消剩余排队打开浏览器')
  }

  const startAll = async () => {
    const targets = rows.filter(
      (r) =>
        !r.in_game &&
        r.selected_id &&
        !busy.has(r.slot) &&
        !queued.has(r.slot) &&
        !r.paused,
    )
    if (!targets.length) {
      toast.info('没有可开始对局的会话')
      return
    }
    // ---- Anti-top-out precheck: never batch-start duplicate accounts ----
    try {
      const pre = await invoke<SessionPrecheck>('precheck_sessions')
      if (!pre.ok) {
        const lines = pre.conflicts.map(
          (c) => `${c.label || c.identity}: ${c.slots.join(' / ')}`,
        )
        toast.error(`账号重复，已阻止一键启动（会顶号）：\n${lines.join('\n')}`)
        return
      }
    } catch (e) {
      // Precheck is a safety net; if it is unavailable (older backend) still
      // let the per-session account lock do its job, but warn the user.
      toast.info(`启动前检查不可用，依赖后端账号锁：${String(e)}`)
    }
    void runStaggered(targets, '开始对局', start)
  }

  const closeAll = async () => {
    const targets = rows.filter(
      (r) => captureRunning(r) && !r.in_game && !busy.has(r.slot),
    )
    if (!targets.length) {
      toast.info('没有可关闭的浏览器（对局中的会话先软停）')
      return
    }
    toast.info(`正在关闭 ${targets.length} 个浏览器…`)
    // aogara: await every close and surface the failures. Previously this
    // fired `void closeBrowser(row)` for each slot with no aggregation, so
    // a hard-kill failure (and its bot-release) went unnoticed — the
    // "batch close leaves autoplay reserved" bug. closeBrowser returns a
    // boolean and toasts its own errors; this names the affected slots.
    const results = await Promise.all(targets.map((row) => closeBrowser(row)))
    const failed = targets.filter((_, i) => results[i] === false)
    if (failed.length) {
      toast.error(`以下会话关闭失败：${failed.map((r) => r.slot).join('、')}`)
    }
    await refresh()
  }

  const stopAll = () => {
    const targets = rows.filter((r) => r.autoplay || r.in_game || busy.has(r.slot))
    if (!targets.length) {
      toast.info('所有会话都已经停止')
      return
    }
    toast.info(`正在停止 ${targets.length} 个会话…`)
    for (const row of targets) void stop(row)
  }

  // "一键取消启动"：先让排队项失效，再取消真正 busy 的会话；不碰正常打牌中的对局。
  const cancelAllBusy = () => {
    const hadQueue = queuedRef.current.size > 0
    batchToken.current += 1
    if (hadQueue) clearQueued()
    const targets = rows.filter((r) => busy.has(r.slot))
    if (!targets.length) {
      toast.info(hadQueue ? '已取消剩余排队启动' : '没有正在启动的会话')
      return
    }
    toast.info(
      hadQueue
        ? `已取消排队，并正在取消 ${targets.length} 个会话的启动…`
        : `正在取消 ${targets.length} 个会话的启动…`,
    )
    for (const row of targets) void stop(row)
  }

  const usedAccounts = new Set(rows.map((r) => r.selected_id).filter(Boolean))
  const available = accounts.filter((a) => !usedAccounts.has(a.id))
  const full = rows.length >= MAX_SESSIONS

  const add = async () => {
    if (!pick) {
      toast.error('先选择一个账号')
      return
    }
    setAdding(true)
    try {
      const r = await invoke<{ slot: string; cdp_port: number }>('create_session', {
        accountId: pick,
      })
      toast.success(`已添加 ${pick} → ${r.slot}（端口 ${r.cdp_port}）`)
      setPick('')
      await refresh()
    } catch (e) {
      toast.error(String(e))
    } finally {
      setAdding(false)
    }
  }

  const remove = async (row: SessionStatus) => {
    markBusy(row.slot, true)
    try {
      await invoke('remove_session', { slot: row.slot })
      toast.success(`${row.slot} 已移除`)
      await refresh()
    } catch (e) {
      toast.error(String(e))
    } finally {
      markBusy(row.slot, false)
    }
  }

  const anyBusy = busy.size > 0 || queued.size > 0
  const runningCount = rows.filter(captureRunning).length
  const playingCount = rows.filter((r) => r.in_game).length

  return (
    <div className="space-y-4 p-4">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="flex items-center gap-2 text-xl font-semibold">
            <Users className="h-5 w-5" /> 多开会话
          </h1>
          <p className="text-sm text-muted-foreground">
            {rows.length} 个会话 · 浏览器 {runningCount} 个已开 · 对局中 {playingCount} 个。
            每个会话独立浏览器 / profile / CDP / 日志，操作互不影响。
          </p>
        </div>
        <Button variant="outline" size="sm" onClick={() => void refresh()}>
          <RefreshCw className="mr-1 h-4 w-4" /> 刷新
        </Button>
      </div>

      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="text-base">全部会话</CardTitle>
          <CardDescription>下面两个开关是进程级单值，一键操作对所有会话同时生效。</CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="grid gap-3 sm:grid-cols-2">
            <div className="flex items-center justify-between gap-3 rounded-lg border p-3">
              <div>
                <p className="text-sm font-medium">自动连打</p>
                <p className="text-xs text-muted-foreground">
                  开着：每局打完自动开下一局。关掉：各会话打完当前局即停，不再调用机器人（释放算力）。
                </p>
              </div>
              <Switch checked={autoNext} onCheckedChange={(v) => void toggleAutoNext(v)} />
            </div>
            <div className="flex items-center justify-between gap-3 rounded-lg border p-3">
              <div>
                <p className="text-sm font-medium">启动 Aogara 时打开浏览器</p>
                <p className="text-xs text-muted-foreground">
                  关闭时启动程序不碰雀魂，浏览器由下面的开关或一键按钮拉起。
                </p>
              </div>
              <Switch checked={autoStart} onCheckedChange={(v) => void toggleAutoStart(v)} />
            </div>
          </div>

          <div className="flex flex-wrap gap-2">
            <Button
              variant={openingAll ? 'destructive' : 'secondary'}
              className="flex-1"
              onClick={openingAll ? cancelOpenAll : openAll}
            >
              {openingAll ? (
                <X className="mr-2 h-4 w-4" />
              ) : (
                <Monitor className="mr-2 h-4 w-4" />
              )}
              {openingAll ? '取消打开浏览器' : '一键打开浏览器'}
              {!openingAll && (
                <span className="ml-2 text-xs opacity-70">
                  {runningCount}/{rows.length}
                </span>
              )}
            </Button>
            <Button variant="outline" className="flex-1" onClick={closeAll}>
              <Monitor className="mr-2 h-4 w-4" />
              一键关闭浏览器
            </Button>
            <Button
              className="flex-1"
              variant={anyBusy ? 'destructive' : 'default'}
              onClick={anyBusy ? cancelAllBusy : startAll}
            >
              {anyBusy ? <X className="mr-2 h-4 w-4" /> : <Play className="mr-2 h-4 w-4" />}
              {anyBusy ? '一键取消启动' : '一键开始对局'}
              {!anyBusy && (
                <span className="ml-2 text-xs opacity-70">
                  {playingCount}/{rows.length}
                </span>
              )}
            </Button>
            <Button variant="outline" className="flex-1" onClick={stopAll}>
              <Square className="mr-2 h-4 w-4" />
              一键停止对局
            </Button>
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">从号池添加会话</CardTitle>
          <CardDescription>
            每个账号一个会话（独立 profile / 端口 / 日志），最多 {MAX_SESSIONS} 个。添加只建会话，不自动开浏览器。
          </CardDescription>
        </CardHeader>
        <CardContent className="flex flex-wrap items-end gap-3">
          <div className="min-w-[220px] flex-1 space-y-1">
            <label className="text-xs text-muted-foreground">号池账号</label>
            <Select value={pick} onValueChange={setPick}>
              <SelectTrigger>
                <SelectValue placeholder={available.length ? '选择要加入的账号' : '没有可用账号'} />
              </SelectTrigger>
              <SelectContent>
                {available.map((a) => (
                  <SelectItem key={a.id} value={a.id}>
                    {a.label}（{a.id}）
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <Button disabled={adding || full || !pick} onClick={() => void add()}>
            {adding ? (
              <Loader2 className="mr-1 h-4 w-4 animate-spin" />
            ) : (
              <Plus className="mr-1 h-4 w-4" />
            )}
            添加到多开
          </Button>
          {full && <span className="text-xs text-destructive">已达上限 {MAX_SESSIONS}</span>}
        </CardContent>
      </Card>

      {error && (
        <p className="rounded-md border border-destructive/40 p-3 text-sm text-destructive">
          {error}
        </p>
      )}

      <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-3">
        {rows.map((row) => {
          const isBusy = busy.has(row.slot)
          const running = captureRunning(row)
          // 会话是否在"自动代打"中：启动中 / 对局中 / autoplay 开着都算，
          // 这一个开关同时承担原来的"开始/取消/软停"三态。
          const active = isBusy || row.in_game || row.autoplay
          const othersUse = new Set(
            rows.filter((r) => r.slot !== row.slot).map((r) => r.selected_id),
          )
          const options = accounts.filter(
            (a) => a.id === row.selected_id || !othersUse.has(a.id),
          )
          return (
            <Card key={row.slot}>
              <CardHeader className="pb-2">
                <div className="flex items-start justify-between gap-2">
                  <div>
                    <CardTitle className="flex items-center gap-2 text-base">
                      {row.slot}
                      {isBusy && <Loader2 className="h-3.5 w-3.5 animate-spin text-muted-foreground" />}
                    </CardTitle>
                    <CardDescription className="font-mono text-xs">
                      {row.selected_id || '未选账号'}
                    </CardDescription>
                  </div>
                  <Badge variant={stateVariant(row)}>{stateLabel(row)}</Badge>
                </div>
              </CardHeader>
              <CardContent className="space-y-2.5 text-sm">
                <div className="space-y-1">
                  <label className="text-xs text-muted-foreground">账号</label>
                  <Select
                    value={row.selected_id}
                    disabled={isBusy || row.in_game}
                    onValueChange={(v) => void patchSession(row.slot, { accountId: v })}
                  >
                    <SelectTrigger className="h-8">
                      <SelectValue placeholder="选择账号" />
                    </SelectTrigger>
                    <SelectContent>
                      {options.map((a) => (
                        <SelectItem key={a.id} value={a.id}>
                          {a.label}（{a.id}）
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </div>

                <div className="grid grid-cols-2 gap-2">
                  <div className="space-y-1">
                    <label className="text-xs text-muted-foreground">段位场</label>
                    <Select
                      value={row.target_room || 'copper'}
                      disabled={isBusy || row.in_game}
                      onValueChange={(v) => void patchSession(row.slot, { room: v })}
                    >
                      <SelectTrigger className="h-8">
                        <SelectValue>{roomLabel(row.target_room || 'copper')}</SelectValue>
                      </SelectTrigger>
                      <SelectContent>
                        {ROOMS.map((r) => (
                          <SelectItem key={r.value} value={r.value}>
                            {r.label}
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                  </div>
                  <div className="space-y-1">
                    <label className="text-xs text-muted-foreground">模式</label>
                    <Select
                      value={row.target_mode || 'mode4e'}
                      disabled={isBusy || row.in_game}
                      onValueChange={(v) => void patchSession(row.slot, { mode: v })}
                    >
                      <SelectTrigger className="h-8">
                        <SelectValue>{modeLabel(row.target_mode || 'mode4e')}</SelectValue>
                      </SelectTrigger>
                      <SelectContent>
                        {MODES.map((m) => (
                          <SelectItem key={m.value} value={m.value}>
                            {m.label}
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                  </div>
                </div>

                <div className="flex items-center justify-between rounded-md border px-2 py-1.5">
                  <span className="flex items-center gap-1 text-xs">
                    <Monitor className="h-3.5 w-3.5" />
                    浏览器
                    <span className={running ? 'text-emerald-600' : 'text-muted-foreground'}>
                      {running ? '已打开' : '未打开'}
                    </span>
                  </span>
                  <Switch
                    checked={running}
                    disabled={row.in_game}
                    onCheckedChange={(v) => toggleBrowser(row, v)}
                  />
                </div>

                <div className="space-y-1 text-xs text-muted-foreground">
                  <div className="flex justify-between gap-2">
                    <span>端口 / profile</span>
                    <span className="truncate font-mono">
                      {row.cdp_port ?? '—'} · {row.profile || '—'}
                    </span>
                  </div>
                  <div className="flex justify-between gap-2">
                    <span>完成局数</span>
                    <span className="tabular-nums">{row.finish_count}</span>
                  </div>
                  <div className="flex justify-between gap-2">
                    <span>最近开局 / 结束</span>
                    <span className="truncate">
                      {row.last_enter ?? '—'} / {row.last_finish ?? '—'}
                    </span>
                  </div>
                  <div className="flex justify-between gap-2">
                    <span className="flex items-center gap-1">
                      <Activity className="h-3 w-3" /> Bot / 抓包
                    </span>
                    <span>
                      {row.bot_status?.state ?? '—'} / {row.capture_status?.state ?? '—'}
                    </span>
                  </div>
                </div>

                <div className="flex gap-2 pt-1">
                  {/* 单会话只有一个开关：开启对局（开始自动代打）/ 停止对局
                      （停止自动代打）。"已启动"含启动中、对局中、autoplay 开着。
                      停止走 stop()：对局中则打完本局后关 autoplay 释放机器人，
                      启动中则 pool_cancel 立刻中断。软停概念已并入此按钮。 */}
                  <Button
                    size="sm"
                    className="flex-1"
                    variant={active ? 'outline' : 'default'}
                    onClick={() => void (active ? stop(row) : start(row))}
                  >
                    {active ? (
                      <Square className="mr-1 h-3 w-3" />
                    ) : (
                      <Play className="mr-1 h-3 w-3" />
                    )}
                    {active ? '停止对局' : '开启对局'}
                  </Button>
                  <Button
                    size="sm"
                    variant="ghost"
                    disabled={isBusy || row.in_game || row.slot === 'slot1'}
                    onClick={() => void remove(row)}
                    title={row.slot === 'slot1' ? '主会话不能移除' : '移除该会话'}
                  >
                    <Trash2 className="h-3 w-3" />
                  </Button>
                </div>
              </CardContent>
            </Card>
          )
        })}
        {rows.length === 0 && (
          <Card>
            <CardContent className="p-6 text-sm text-muted-foreground">
              没有会话。检查 `[multi].sessions` 配置。
            </CardContent>
          </Card>
        )}
      </div>

      {anyBusy && (
        <p className="text-center text-xs text-muted-foreground">
          正在处理：{[...busy].join('、')}（其他会话不受影响）
        </p>
      )}
    </div>
  )
}
