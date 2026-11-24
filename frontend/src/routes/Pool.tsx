// 号池 · 只有账号本身。
//
// 这里只维护"有哪些号"：添加 / 编辑 / 删除。目标场次、连打、开关浏览器、
// 开始对局都属于运行控制，全部在「多开会话」页，一个页面管一件事。
//
// 页面只做「视图 + 配置写入」，业务状态权威在后端 config（磁盘 config.toml）。
// 写配置走 patchPool：取当前 config → 替换 pool 段 → update_config → 更新 store。

import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import { Save, Plus, Trash2, Pencil, X, Users } from 'lucide-react'

import { Button } from '@/components/ui/button'
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { toast } from '@/components/ui/sonner'
import { invoke } from '@/lib/tauri'
import { useConfigStore } from '@/stores/configStore'
import type { PoolConfig, PoolAccount } from '@/types'

const EMPTY_POOL: PoolConfig = {
  accounts: [],
  selected_id: '',
  target: { room: 'copper', mode: 'mode4e' },
  auto_next: false,
  timer_enabled: false,
  timer_hours: 4,
  start_capture_on_launch: false,
}

const FORM_EMPTY = { id: '', label: '', uid: '', email: '', password: '' }

export function Pool() {
  const { t } = useTranslation()
  const config = useConfigStore((s) => s.config)
  const setConfig = useConfigStore((s) => s.setConfig)
  const pool: PoolConfig = config?.pool ?? EMPTY_POOL
  const [saving, setSaving] = useState(false)
  const [showAdd, setShowAdd] = useState(false)
  const [editingId, setEditingId] = useState<string | null>(null)
  const [form, setForm] = useState(FORM_EMPTY)

  const openAdd = () => {
    setEditingId(null)
    setForm(FORM_EMPTY)
    setShowAdd(true)
  }

  const openEdit = (acc: PoolAccount) => {
    setEditingId(acc.id)
    setForm({
      id: acc.id,
      label: acc.label ?? '',
      uid: acc.uid ?? '',
      email: acc.email ?? '',
      password: acc.password ?? '',
    })
    setShowAdd(true)
  }

  // Patch just the pool section and persist through the existing command.
  const patchPool = async (patch: Partial<PoolConfig>): Promise<boolean> => {
    if (!config) {
      toast.error(t('pool.no_config', '配置尚未加载'))
      return false
    }
    const next = { ...pool, ...patch }
    const nextConfig = { ...config, pool: next }
    setSaving(true)
    try {
      await invoke('update_config', { newConfig: nextConfig })
      setConfig(nextConfig)
      return true
    } catch (e) {
      toast.error(String(e))
      return false
    } finally {
      setSaving(false)
    }
  }

  const saveAccount = async () => {
    const email = form.email.trim().toLowerCase()
    if (!email || !form.password) {
      toast.error(t('pool.add_required', '请填写邮箱和密码'))
      return
    }

    // 编辑：保留原坐标表，只改基本信息
    if (editingId) {
      const editing = pool.accounts.find((a) => a.id === editingId)
      if (!editing) {
        toast.error('账号不存在，可能已被删除')
        setShowAdd(false)
        setEditingId(null)
        return
      }
      const id = form.id.trim() || editingId
      const updated: PoolAccount = {
        ...editing,
        id,
        label: form.label.trim() || email,
        uid: form.uid.trim(),
        email,
        password: form.password,
      }
      const ok = await patchPool({
        accounts: pool.accounts.map((a) => (a.id === editingId ? updated : a)),
        selected_id: pool.selected_id === editingId ? id : pool.selected_id,
      })
      if (ok) {
        setForm(FORM_EMPTY)
        setShowAdd(false)
        setEditingId(null)
        toast.success(`${t('pool.edit_done', '已保存修改')}: ${updated.label}`)
      }
      return
    }

    // 新增：ID 留空时自动用邮箱前缀生成，并保证唯一
    let id = form.id.trim()
    if (!id) {
      const base = email.split('@')[0] || 'acc'
      id = base
      let n = 1
      while (pool.accounts.some((a) => a.id === id)) id = `${base}_${++n}`
    }
    if (pool.accounts.some((a) => a.id === id)) {
      toast.error(t('pool.add_dup', '账号 ID 已存在'))
      return
    }
    const acc: PoolAccount = {
      id,
      label: form.label.trim() || email,
      uid: form.uid.trim(),
      email,
      password: form.password,
      coords: {}, // 留空 → 后端 resolve_account 自动继承 main 的画布坐标
    }
    const ok = await patchPool({
      accounts: [...pool.accounts, acc],
      selected_id: pool.selected_id || acc.id,
    })
    if (ok) {
      setForm(FORM_EMPTY)
      setShowAdd(false)
      toast.success(`${t('pool.add_done', '已添加账号')}: ${acc.label}`)
    }
  }

  const removeAccount = async (id: string) => {
    const next = pool.accounts.filter((a) => a.id !== id)
    const ok = await patchPool({
      accounts: next,
      selected_id: pool.selected_id === id ? (next[0]?.id ?? '') : pool.selected_id,
    })
    if (ok) toast.success(t('pool.remove_done', '已删除账号'))
  }

  return (
    <div className="mx-auto w-full max-w-4xl space-y-6 p-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">
          {t('nav.pool', '号池')}
        </h1>
        <p className="text-sm text-muted-foreground">
          {t('pool.description_only', '账号仓库：在这里增删改账号；开浏览器、选场次、开始对局请到「多开会话」。')}
        </p>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Users className="h-5 w-5" />
            {t('pool.accounts', '账号池')}
          </CardTitle>
          <CardDescription>
            {t('pool.accounts_hint_only', '账号只保存登录信息与画布坐标；每个账号可以在多开会话里占用一个独立会话。')}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          {pool.accounts.length === 0 ? (
            <p className="text-sm text-muted-foreground">
              {t('pool.no_accounts', '尚无账号，点下方按钮添加。')}
            </p>
          ) : (
            <div className="space-y-2">
              {pool.accounts.map((a) => (
                <div key={a.id} className="flex items-center gap-2">
                  <div className="flex flex-1 items-center gap-2 rounded-lg border px-3 py-2">
                    <span className="font-medium">{a.label || a.id}</span>
                    <span className="font-mono text-xs text-muted-foreground">{a.id}</span>
                    {a.uid && (
                      <span className="ml-auto text-xs text-muted-foreground">uid {a.uid}</span>
                    )}
                  </div>
                  <Button
                    variant="ghost"
                    size="icon"
                    title={t('pool.edit', '编辑账号')}
                    onClick={() => openEdit(a)}
                  >
                    <Pencil className="h-4 w-4 text-muted-foreground" />
                  </Button>
                  <Button
                    variant="ghost"
                    size="icon"
                    title={t('pool.remove', '删除账号')}
                    onClick={() => removeAccount(a.id)}
                  >
                    <Trash2 className="h-4 w-4 text-muted-foreground" />
                  </Button>
                </div>
              ))}
            </div>
          )}

          {showAdd ? (
            <div className="space-y-3 rounded-lg border p-3">
              <div className="flex items-center justify-between">
                <span className="text-sm font-medium">
                  {editingId ? t('pool.edit_title', '编辑账号') : t('pool.add_title', '添加账号')}
                </span>
                <Button
                  variant="ghost"
                  size="icon"
                  onClick={() => {
                    setShowAdd(false)
                    setEditingId(null)
                    setForm(FORM_EMPTY)
                  }}
                >
                  <X className="h-4 w-4" />
                </Button>
              </div>
              <div className="grid gap-3 sm:grid-cols-2">
                <div className="space-y-1">
                  <Label>{t('pool.field_id', '账号 ID（留空自动生成）')}</Label>
                  <Input
                    value={form.id}
                    placeholder="留空则用邮箱前缀"
                    disabled={editingId !== null}
                    onChange={(e) => setForm({ ...form, id: e.target.value })}
                  />
                  {editingId !== null && (
                    <p className="text-xs text-muted-foreground">
                      ID 是多开会话与配置的引用键，创建后不可改
                    </p>
                  )}
                </div>
                <div className="space-y-1">
                  <Label>{t('pool.field_label', '昵称')}</Label>
                  <Input
                    value={form.label}
                    placeholder="显示名称"
                    onChange={(e) => setForm({ ...form, label: e.target.value })}
                  />
                </div>
                <div className="space-y-1">
                  <Label>{t('pool.field_uid', 'UID')}</Label>
                  <Input
                    value={form.uid}
                    placeholder="数字 UID（可选）"
                    onChange={(e) => setForm({ ...form, uid: e.target.value })}
                  />
                </div>
                <div className="space-y-1">
                  <Label>{t('pool.field_email', '登录邮箱')} *</Label>
                  <Input
                    type="email"
                    value={form.email}
                    placeholder="name@example.com"
                    onChange={(e) => setForm({ ...form, email: e.target.value })}
                  />
                </div>
                <div className="space-y-1 sm:col-span-2">
                  <Label>{t('pool.field_password', '密码')} *</Label>
                  <Input
                    type="password"
                    value={form.password}
                    placeholder="登录密码（明文保存到 config.toml）"
                    onChange={(e) => setForm({ ...form, password: e.target.value })}
                  />
                </div>
              </div>
              <p className="text-xs text-muted-foreground">
                {t('pool.add_hint', '画布坐标留空，自动继承主号 main；邮箱和密码必填，ID 留空自动生成。')}
              </p>
              <Button size="sm" disabled={saving} onClick={saveAccount}>
                {editingId ? (
                  <Save className="mr-1 h-3 w-3" />
                ) : (
                  <Plus className="mr-1 h-3 w-3" />
                )}
                {editingId ? t('pool.edit_save', '保存修改') : t('pool.add_save', '保存账号')}
              </Button>
            </div>
          ) : (
            <Button variant="outline" size="sm" className="w-full" onClick={openAdd}>
              <Plus className="mr-1 h-3 w-3" />
              {t('pool.add_account', '添加账号')}
            </Button>
          )}
        </CardContent>
      </Card>
    </div>
  )
}
