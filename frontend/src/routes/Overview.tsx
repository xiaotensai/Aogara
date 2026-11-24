import { useEffect, useState } from 'react'
import { Link } from 'react-router-dom'
import { useTranslation } from 'react-i18next'
import { Bot, Shield, ScrollText, Gamepad2, Settings as SettingsIcon, Users } from 'lucide-react'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { useBotStore } from '@/stores/botStore'
import { useCaptureStore } from '@/stores/captureStore'
import { useConfigStore } from '@/stores/configStore'
import { useAnalysisStore } from '@/stores/analysisStore'
import { invoke, listen } from '@/lib/tauri'
import { fmtTime } from '@/lib/format'
import { AogaraWordmark } from '@/components/BrandLogo'
import type { SessionStatus } from '@/types'

const DOT: Record<string, string> = {
  ready:    'bg-emerald-500',
  running:  'bg-emerald-500',
  loading:  'bg-amber-500',
  starting: 'bg-amber-500',
  idle:     'bg-zinc-500',
  stopped:  'bg-zinc-500',
  error:    'bg-red-500',
}

function sessionStateLabel(s: SessionStatus): string {
  if (s.in_game) return '对局中'
  if (s.capture_status?.state === 'error') return '抓包错误'
  if (s.paused) return '已停止'
  if (s.capture_status?.state === 'running') return s.autoplay ? '大厅 · 自动打牌' : '浏览器已开'
  if (s.autoplay) return '自动打牌'
  return '未启动'
}

function sessionStateVariant(s: SessionStatus): 'default' | 'secondary' | 'destructive' | 'outline' {
  if (s.in_game) return 'default'
  if (s.capture_status?.state === 'error') return 'destructive'
  if (s.capture_status?.state === 'running') return 'secondary'
  return 'outline'
}

export function Overview() {
  const { t } = useTranslation()
  const bot = useBotStore((s) => s.status)
  const capture = useCaptureStore((s) => s.status)
  const logDir = useConfigStore((s) => s.logDir)
  const lastAnalysis = useAnalysisStore((s) => s.updatedAt)

  const captureTitle = 'kind' in capture && capture.kind === 'chromium' ? t('overview.capture_chromium') : t('overview.capture_mitm')
  const captureDetail = 'descriptor' in capture && capture.descriptor ? capture.descriptor : '—'

  return (
    <div className="p-6 flex flex-col gap-6 w-full">
      <div className="flex justify-center pt-2 pb-1">
        <AogaraWordmark className="h-28" />
      </div>
      <header>
        <h1 className="text-2xl font-semibold">{t('overview.title')}</h1>
        <p className="text-sm text-muted-foreground">{t('overview.description')}</p>
      </header>

      <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
        <StatusCard
          icon={Bot}
          title={t('overview.bot_card_title')}
          state={bot.state}
          detail={'bot' in bot && bot.bot ? bot.bot : '—'}
          extra={'actor_id' in bot ? `actor_id ${bot.actor_id}` : 'error' in bot ? bot.error : undefined}
        />
        <StatusCard
          icon={Shield}
          title={captureTitle}
          state={capture.state}
          detail={captureDetail}
          extra={'error' in capture ? capture.error : undefined}
        />
        <Card>
          <CardHeader className="flex flex-row items-center gap-2">
            <ScrollText className="h-4 w-4 text-muted-foreground" />
            <CardTitle className="text-sm uppercase tracking-wider">{t('overview.log_session')}</CardTitle>
          </CardHeader>
          <CardContent>
            <div className="font-mono text-xs break-all">{logDir || '—'}</div>
          </CardContent>
        </Card>
      </div>

      <SessionsOverviewCard />

      <Card>
        <CardHeader>
          <CardTitle className="text-sm uppercase tracking-wider">{t('overview.last_analysis')}</CardTitle>
        </CardHeader>
        <CardContent>
          <span className="font-mono text-sm">
            {lastAnalysis ? fmtTime(new Date(lastAnalysis)) : '—'}
          </span>
        </CardContent>
      </Card>

      <div className="flex gap-2">
        <Button asChild>
          <Link to="/game" className="gap-1.5">
            <Gamepad2 className="h-4 w-4" />
            {t('overview.open_dashboard')}
          </Link>
        </Button>
        <Button asChild variant="outline">
          <Link to="/sessions" className="gap-1.5">
            <Users className="h-4 w-4" />
            多开会话
          </Link>
        </Button>
        <Button asChild variant="outline">
          <Link to="/settings" className="gap-1.5">
            <SettingsIcon className="h-4 w-4" />
            {t('settings.title')}
          </Link>
        </Button>
      </div>
    </div>
  )
}

// 每个账号一行：slot / 账号 / 状态 / 完成局数 / 最近开局与结束。
// 5 秒兜底 + session-status 事件刷新，页面隐藏时完全停轮询。
function SessionsOverviewCard() {
  const [rows, setRows] = useState<SessionStatus[]>([])
  const [labels, setLabels] = useState<Record<string, string>>({})

  useEffect(() => {
    let alive = true
    let unlisten: (() => void) | undefined
    const tick = async () => {
      if (document.hidden) return
      try {
        const list = await invoke<SessionStatus[]>('list_sessions')
        if (!alive) return
        setRows(list)
      } catch {
        // backend not ready — keep the last snapshot
      }
    }
    void tick()
    void listen<{ slot?: string }>('session-status', () => void tick()).then((fn) => {
      if (alive) unlisten = fn
      else fn()
    })
    const id = window.setInterval(() => void tick(), 5000)
    const onVisible = () => {
      if (!document.hidden) void tick()
    }
    document.addEventListener('visibilitychange', onVisible)
    return () => {
      alive = false
      window.clearInterval(id)
      document.removeEventListener('visibilitychange', onVisible)
      unlisten?.()
    }
  }, [])

  // 账号昵称：从各会话配置的账号表里找 label（sessions 页也是这么显示的）。
  // 简化处理：selected_id 本身已经足够辨认，label 通过 get_config 一次性取。
  useEffect(() => {
    void invoke<{ pool?: { accounts?: { id: string; label: string }[] } }>('get_config')
      .then((cfg) => {
        const map: Record<string, string> = {}
        for (const a of cfg.pool?.accounts ?? []) map[a.id] = a.label || a.id
        setLabels(map)
      })
      .catch(() => {})
  }, [])

  const playing = rows.filter((r) => r.in_game).length
  const totalGames = rows.reduce((n, r) => n + r.finish_count, 0)

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between">
        <CardTitle className="flex items-center gap-2 text-sm uppercase tracking-wider">
          <Users className="h-4 w-4 text-muted-foreground" />
          账号对局
        </CardTitle>
        <span className="text-xs text-muted-foreground">
          对局中 {playing}/{rows.length} · 累计 {totalGames} 局
        </span>
      </CardHeader>
      <CardContent className="space-y-2">
        {rows.length === 0 ? (
          <p className="text-sm text-muted-foreground">没有会话，去「多开会话」页添加。</p>
        ) : (
          rows.map((s) => (
            <div key={s.slot} className="flex items-center gap-3 rounded-lg border px-3 py-2 text-sm">
              <span className="font-mono text-xs text-muted-foreground">{s.slot}</span>
              <span className="min-w-0 flex-1 truncate font-medium">
                {labels[s.selected_id] || s.selected_id || '未选账号'}
              </span>
              <span className="text-xs text-muted-foreground">
                {s.finish_count} 局
                {s.last_finish ? ` · 上局 ${s.last_finish.slice(11, 19)}` : ''}
              </span>
              <Badge variant={sessionStateVariant(s)}>{sessionStateLabel(s)}</Badge>
            </div>
          ))
        )}
      </CardContent>
    </Card>
  )
}

function StatusCard({
  icon: Icon, title, state, detail, extra,
}: {
  icon: typeof Bot
  title: string
  state: string
  detail: string
  extra?: string
}) {
  return (
    <Card>
      <CardHeader className="flex flex-row items-center gap-2">
        <Icon className="h-4 w-4 text-muted-foreground" />
        <CardTitle className="text-sm uppercase tracking-wider">{title}</CardTitle>
      </CardHeader>
      <CardContent>
        <div className="flex items-center gap-2">
          <span className={`h-2 w-2 rounded-full ${DOT[state] ?? 'bg-zinc-500'}`} />
          <span className="capitalize text-sm font-medium">{state}</span>
        </div>
        <div className="text-xs font-mono text-muted-foreground mt-1">{detail}</div>
        {extra && <div className="text-xs text-muted-foreground mt-1">{extra}</div>}
      </CardContent>
    </Card>
  )
}
