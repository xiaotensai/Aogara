import { invoke } from '@/lib/tauri'

// Project-wide community / source links. Used by the first-run wizard
// banners and the sidebar footer; kept here so the URLs aren't
// duplicated across components.
//
// TODO(aogara): These are intentionally EMPTY placeholders. Aogara is a
// personal fork and has no published repo/community yet. Fill in your own
// URLs when you publish; every consumer goes through openExternal(), which
// no-ops on an empty string, so leaving them blank is safe.
// 引子：上轮全量改名把上游链接改成不存在的地址（404）。用户决定暂时留空，
// 后续接个人仓库/社群。填入时保持四个常量结构不变即可。
export const AOGARA_GITHUB_URL = 'https://github.com/xiaotensai/Aogara'
export const AOGARA_DISCORD_URL = ''
export const AOGARAMS_GITHUB_URL = ''
export const AOGARAMS_DOWNLOAD_URL = ''

// Tauri 2's webview doesn't reliably honour `<a target="_blank">`
// without the opener plugin, so route external links through the
// `open_external_url` backend command, which hands the URL to the OS
// default-browser handler (ShellExecuteW / open / xdg-open).
export function openExternal(url: string): void {
  // Empty placeholder links are a no-op — don't hand "" (or a bare
  // "/releases" built from an empty base) to the OS shell.
  if (!url || !/^https?:\/\//.test(url)) return
  invoke('open_external_url', { url }).catch(() => {
    /* surfaced via Sonner toast hooked into the tauri error bridge */
  })
}
