import { create } from 'zustand'
import { invoke } from '@/lib/tauri'
import type {
  AnalysisResult,
  BotStatus,
  CaptureStatus,
  GameStateSnapshot,
  MahgenView,
  SessionStatus,
} from '@/types'
import { useGameStore } from './gameStore'
import { useAnalysisStore } from './analysisStore'
import { useBotStore } from './botStore'
import { useCaptureStore } from './captureStore'

// 对局界面当前显示哪个会话。slot1 的事件走全局频道（mjai-event），
// 其他会话走 `mjai-event:slotN` 等带后缀的频道；本 store 只存"看谁"，
// 事件路由在 useTauriBridge 里按这个值过滤。
interface SessionView {
  activeSlot: string
  setActiveSlot: (slot: string) => void
}

export const useSessionStore = create<SessionView>((set) => ({
  activeSlot: 'slot1',
  setActiveSlot: (slot) => {
    set({ activeSlot: slot })
    void hydrateSession(slot)
  },
}))

/** 把一个会话的实时状态拉进对局界面的各 store（切换会话时调用）。 */
export async function hydrateSession(slot: string): Promise<void> {
  try {
    const [snap, view, analysis] = await Promise.all([
      invoke<GameStateSnapshot | null>('get_game_snapshot', { slot }),
      invoke<MahgenView | null>('get_mahgen_view', { slot }),
      invoke<AnalysisResult | null>('get_analysis', { slot }),
    ])
    useGameStore.getState().setGame(snap)
    useGameStore.getState().setView(view)
    useAnalysisStore.getState().set(analysis)
    const st = await invoke<SessionStatus>('get_session_status', { slot })
    useBotStore.getState().setStatus(st.bot_status as BotStatus)
    useCaptureStore.getState().set(st.capture_status as CaptureStatus)
  } catch {
    /* 后端未就绪或会话不存在：保留当前画面 */
  }
}
