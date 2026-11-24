# Aogara 全量改名清单（Akagi → Aogara）

> 状态：**已于 2026-11-24 16:45 全量执行完成**（D1/D2 改、高风险全改、D3 保留）。
> 实测通过：窗口标题 Aogara、precheck 5 账号唯一、slot2 登录态保留、GPU 偏好新键生效、exe 内无旧标识。
> 根目录已迁至 `<安装目录>`，exe 为 `aogara.exe`。下方清单为执行依据，保留存档。
>
> （原始盘点说明）
> 目标：把所有"Akagi/akagi"痕迹改成 Aogara/aogara，包括脚本名、文件名、目录名、路径、
> 证书、注册表、localStorage 键、源码标识符与文案。
> 生成时间：2026-11-24 15:29　基准：`<安装目录>\`
> 已完成：窗口标题、产品名、identifier（见主文档 15:17 条目）——本清单覆盖**剩余全部**。

---

## 零、必须先定的三个决策（影响整个清单）

| 决策 | 说明 | 本清单暂定 |
|---|---|---|
| **D1 目录是否改名** | `<安装目录>` → `<安装目录>`？牵动所有绝对路径、快捷方式、注册表 GPU 偏好、文档。工作量最大、风险最高，但"全部改动"要求里应包含 | ✅ 改名（用户要求"所有关于 akagi 的内容"） |
| **D2 exe 是否改名** | `akagi.exe` → `aogara.exe`。牵动脚本、GPU 偏好注册表、快捷方式、任务管理器匹配 | ✅ 改名（用户明确"包括脚本名称"，exe 同属产物名） |
| **D3 是否保留上游署名** | LICENSE/NOTICE/公告历史里的 Akagi 是**法律要求**，不能抹；证书 CN 里的 Akagi 属技术标识，可改 | 保留 LICENSE/NOTICE/公告历史；其余全改 |

> 若 D1 只改 exe 不改根目录，需把清单里所有 `<安装目录>` 路径保留——请确认。

---

## 一、P0 必须先做的"改了会坏"项（功能性硬编码）

这些是**程序/脚本运行的真实依赖**，改名时必须同步改，否则程序找不到文件、进程匹配不到。

### 1.1 可执行文件名与进程名（D2）

| 位置 | 当前 | 目标 |
|---|---|---|
| `app/akagi.exe` | `akagi.exe` | `aogara.exe` |
| `Cargo.toml` | `name = "akagi"`（package + bin 两处） | `name = "aogara"` |
| `_setup/src/.../target/.../release/akagi.exe` | 编译产物名 | `aogara.exe`（随 Cargo.toml 变） |
| `tools/perf-monitor.ps1:11,23` | `Name='akagi.exe'`、`-eq 'akagi.exe'` | `aogara.exe` |
| `tools/lib/akagi.js:15,144,147` | `EXE`、`IMAGENAME eq akagi.exe`、正则 `^"akagi\.exe"` | `aogara.exe` |
| `tools/_boot-detached.js:8-11` | 路径 `akagi-stdout.log`/`akagi-stderr.log`/`akagi.exe` | `aogara-*` |
| `tools/start-akagi.js` | 脚本名 + 注释里进程名 | `start-aogara.js` |
| `启动Akagi.cmd:19-21` | `tasklist /fi "imagename eq akagi.exe"` + 路径 | 改名脚本 + `aogara.exe` |
| `build-akagi-native.cmd:39-52` | 备份/复制 `akagi.exe` | `build-aogara-native.cmd` + `aogara.exe` |
| 注册表 `HKCU\...\UserGpuPreferences` | 键 `<安装目录>\app\akagi.exe` | 删旧键、加 `<安装目录>\app\aogara.exe`（走 `gpu-pref.ps1`） |

### 1.2 根目录路径（D1，全部 `<安装目录>` → `<安装目录>`）

| 文件 | 行/位置 | 说明 |
|---|---|---|
| `_rust-only.cmd` | `SRC=<安装目录>/_setup/src/Akagi-3` | 构建脚本源路径 |
| `tools/gpu-pref.ps1` | `'<安装目录>\app\akagi.exe'` | GPU 偏好 |
| `tools/lib/akagi.js` | `APP_DIR`、`--user-data-dir` 注释 | 核心库 |
| `tools/lib/persist.js` | `CONFIG = <安装目录>\app\configs\config.toml` | 配置路径 |
| `tools/_delay-apply-watch.js` / `_delay-stats2.js` | `<安装目录>\app\logs` | 日志路径 |
| `tools/capture-screen.js` / `hover-check.js` | `<安装目录>/tools/...png` | 截图路径 |
| `tools/set-extra-args.js` | `<安装目录>/app/configs/config.toml` | 配置路径 |
| `tools/bridge.js` | 多处注释 + `start-akagi` step 名 | 桥接（已弃用但仍保留） |
| `使用手册.md`、`README.md`、`docs/*.md` | 全文路径 | 文档 |

### 1.3 前端功能性硬编码（不改会坏或暴露旧品牌）

| 类别 | 位置 | 当前值 |
|---|---|---|
| **localStorage 键**（实测 **19 个唯一键**） | `stores/*.ts`、`hooks/useSidebar.ts`、`i18n/index.ts`、`DelayScriptUpdateDialog.tsx` | 完整清单见下方 §1.4 |
| **外链** | `lib/external.ts:6-9` | `github.com/shinkuan/Akagi`、`discord.gg/Z2wjXUK8bN`、`github.com/shinkuan/AkagiMS` ×2 |
| **公告卡片** | `announcements/entries.ts:16,100-103` | `akagims` 卡片 + `akagims-fullauto.jpg` |
| **品牌组件** | `components/BrandLogo.tsx`、`routes/Overview.tsx`、`sidebar/{Sidebar,NarrowTopBar}.tsx` | `AkagiIcon`/`AkagiWordmark` 组件名 + 4 个 svg 引用 + alt="Akagi" |
| **i18n 文案** | 四语资源共 **124 处** `Akagi` | 关于、更新、Setup 向导、错误提示、AKAGIMS 卡片等 |
| **版本比较** | `lib/appVersion.ts`、`stores/updaterStore.ts` | 注释/键名 akagi |

> localStorage 键改名会让已有偏好**全部重置**（主题/侧栏/布局/公告已读/更新器状态）。你是唯一用户，可接受；若想保留，写一次性迁移读取旧键。

### 1.4 localStorage 键完整清单（19 个，`akagi.` → `aogara.`）

```
akagi.announcement.akagims.card   akagi.announcement.lastSeen
akagi.dashboard.hidden.3p         akagi.dashboard.hidden.4p
akagi.dashboard.layouts.3p        akagi.dashboard.layouts.4p
akagi.dashboard.onboarded         akagi.delayScriptOffered
akagi.history.ptRule              akagi.lang
akagi.review.gameMap              akagi.review.gameMap.kkkk
akagi.review.pendingJob           akagi.review.pendingJob.kkkk
akagi.sidebar                     akagi.theme
akagi.theme.css                   akagi.ui.scale
akagi.updater
```
其中 `.kkkk` 后缀是测试用的占位账号 ID，同样要改前缀。
另注：`i18n/index.ts` 的 `lookupLocalStorage: 'akagi.lang'` 是语言检测键，改后语言偏好重置。

---

## 二、P1 源码内部标识（不改功能，但残留品牌）

### 2.1 Rust 侧（约 568 处，其中约 199 处为纯注释）

| 类别 | 位置示例 | 当前 | 目标 | 风险 |
|---|---|---|---|---|
| **日志 target** | `bridge/*/mod.rs`、`autoplay/*`、`bot/*` 等几十处 | `target: "akagi::..."`、`const LOG: &str = "akagi::..."` | `aogara::...` | 低（仅日志过滤） |
| **tracing 前缀** | 全 src 的 `tracing::info!("[pool]...")` 等 | 部分含 akagi | aogara | 低 |
| **内置 bot ID** | `bot/native.rs:59,61` | `akagi-native`、`akagi-native3p` | `aogara-native`、`aogara-native3p` | **中**：配置 `config.toml` 里 `active_4p` 等引用要同步改，否则 bot 找不到 |
| **bot 显示名** | `bot/native.rs:84-85,794,812-813` | `"Akagi (built-in, 4p)"`、`"Akagi · Local/Online"` | Aogara | 低 |
| **bot 目录约定** | `bot/manifest.rs:34`、`bot/runtime.rs:30` | `.akagi`（venv/settings 目录） | `.aogara` | **中高**：已生成的 bot 目录要迁移，否则环境失效 |
| **bot 安装/环境** | `bot/install.rs:128,326` | `akagi-bot-{}.zip`、`akagi-extract-{}` | aogara-* | 低（临时文件名） |
| **环境变量** | `bot/manager.rs:598`、`bot/runner.rs:121` | `AKAGI_BOT_CONFIG`、`AKAGI_PLAYER_ID` | `AOGARA_*` | **中**：bot 子进程脚本若读这些变量要同步 |
| **JS 注入全局/标记** | `autoplay/tenhou/inject.rs:38,41` | `__akagiTenhou`、`/*akagi-injected*/` | `__aogaraTenhou` 等 | **中**：注入脚本与页面约定，需成对改 |
| **bot 通知协议** | `bot/runner.rs:34,384` | `@@AKAGI_NOTIFY@@ ` | `@@AOGARA_NOTIFY@@ ` | **中**：bot stdout 解析协议，两端同步 |
| **UA** | `github/mod.rs:29`、`capture/chromium/cft.rs:398` | `akagi/<version>`、`akagi-cft-downloader` | aogara | 低（外观） |
| **inspector 标记** | `capture/chromium/cdp.rs:411`、`proxy/handler.rs:267,567` | `akagi_blocked`、`akagi_unpaired` | aogara_* | 低（日志/注解键） |
| **Cargo crate/bin 名** | `Cargo.toml:2,7`、`Cargo.lock` | `akagi` | `aogara` | 随 D2 |
| **注释/测试** | 全 src 约 199 处 | 各种 akagi | aogara | 零风险但量大 |

### 2.2 前端组件名/变量

`BrandLogo.tsx` 的 `AkagiIcon`/`AkagiWordmark`、`uiPrefsStore.ts` 的 `akagimsCardDismissed`、`Sidebar.tsx` 注释、`lib/nativeBots.ts` 的 `akagi-native` 常量——改名需同步所有 import。

---

## 三、P1 视觉品牌（用户截图里最显眼的部分）

| 资源 | 当前 | 说明 |
|---|---|---|
| `frontend/src/assets/logo/akagi-{icon,logo}-{light,dark}.svg`（4 个） | 大字 "Akagi" | **截图里侧栏 + 主内容大 logo 就来自这里**，必须重画 |
| `_setup/src/Akagi-3/assets/logo/akagi-*.png/.svg`（8 个） | 同上（另一份拷贝） | 同步替换 |
| `icons/*.png/ico/icns` | 上游图标全套 | Tauri 打包图标，需按新 logo 重生成 |
| `frontend/src/assets/akagims-fullauto.jpg` | 上游推广图 | 若移除 AKAGIMS 卡片可一并删 |
| `native_bot/weights/akagi3p.safetensors`、`akagi4p.safetensors` | 模型权重文件名 | 可改名，但**内容不动**；改后 `bot/native.rs` 引用同步 |

> 注意：logo SVG 头部含上游作者署名（author: Sky, github.com/SKYisSKYisSKY）。
> 重画后若要合规，可在自己的 logo 文件里保留一行来源注释。

---

## 四、P1 证书与系统层

| 项 | 当前 | 说明 |
|---|---|---|
| `app/ca/akagi-ca.{cer,crt,der,key,key.der,pem}`（6 个文件） | 文件名 akagi-* | 改名 + 重新签发 |
| 证书 subject/issuer | `CN=Akagi Proxy CA, O=Akagi` | 重签为 `CN=Aogara Proxy CA, O=Aogara` |
| **Windows 受信任根证书存储** | 已安装 "Akagi" 根证书 | 需删除旧的、安装新的（否则 MITM 解密失败） |
| 桌面快捷方式 | `%USERPROFILE%\Desktop\Akagi.lnk` | 改名 `Aogara.lnk` + 指向新 exe |
| 注册表 GPU 偏好 | `<安装目录>\app\akagi.exe` → `GpuPreference=2;` | 删旧键、写新键 |
| 残留 WebView2 目录 | `%USERPROFILE%\AppData\Local\com.akagi.akagi` | 可删（旧的已不用）；新目录 `com.xiao.aogara` 已生成 |
| 防火墙规则（如有） | 未发现 | 需确认（`netsh advfirewall firewall show rule` 搜 akagi） |

> 证书是最容易被忽略的一项：只改文件名不改 CN，浏览器仍显示 "Akagi Proxy CA"；
> 重新签发后必须把新 CA 装进受信任根，否则 MITM 代理的 HTTPS 解密会失败。

---

## 五、P2 文档与目录名

| 类别 | 项 |
|---|---|
| 文档文件名 | `docs/Akagi-技术文档-主文档.md`、`README.md`、`使用手册.md` 中的标题；`docs/archive/2026-11-22/Akagi-*.tsx/md` |
| 文档内容 | 主文档/手册/README/方案文档里所有 `<安装目录>`、`akagi.exe`、Akagi 字样（约数百处） |
| 顶层目录 | `_setup/src/Akagi-3` → `Aogara`（改后构建脚本 `_rust-only.cmd`、`build-*.cmd` 路径同步） |
| 数据目录 | `<安装目录>` → `<安装目录>`（D1）；内部 `app/configs`、`app/logs`、`profiles/p-*` 保持不变（不暴露品牌） |
| 备份/归档目录 | `backup/2026-11-23-v3.8-single/`、`backups/antitop-*/`、`backups/aogara-*/`、`docs/archive/`、`tools/archive/` 里的历史文件 |

> 历史归档里的 Akagi（backup/、archive/）建议**保留原样**——它们是历史记录，改名会破坏可追溯性；
> 若"全部改动"字面要求，则需逐个处理，但收益低。

---

## 六、P2 更新器（品牌 + 功能风险）

| 位置 | 当前 | 说明 |
|---|---|---|
| `src/github/mod.rs:29` | `USER_AGENT = "akagi/<version>"` | 改 aogara（见 2.1） |
| `src/updater/check.rs` | 用 `CARGO_PKG_VERSION` 比对上游 release | 版本 1.1.0 < 上游 3.8.0，**会误判"有新版本"** |
| `frontend/src/stores/updaterStore.ts` | `akagi.updater` localStorage 键 | 改键 |
| `frontend/src/routes/Settings.tsx` | 更新检查入口 | 建议直接**关闭**上游更新检查（个人产物手动换 exe） |

> **现状更新（2026-11-24 18:36 精修轮）**：上表是改名时的风险盘点（历史记录）。
> 更新检查**已关闭**——`src/ipc/commands.rs` 的 `check_for_update` 直接返回 `Ok(None)`，
> `UPSTREAM_REPO` 保留为 TODO 引子；启动日志 github release 请求数 = 0，误判隐患已消除。
> 外链（`lib/external.ts` 4 项）已置空 + 空值保护，留引子待填个人 URL。
> 详见 `docs/Aogara-问题修复方案.md` v2。

---

## 七、执行顺序建议（带依赖）

1. **决策确认**：D1 根目录改名？D2 exe 改名？归档历史是否保留？
2. **备份**：整个 `<安装目录>` 目录 + 当前 exe + 证书 + 注册表导出
3. **P0 功能硬编码**：exe/bin 名、路径、进程名、脚本名、GPU 偏好、证书重签
4. **P1 前端 visible**：logo SVG 重画 → 图标重生成 → i18n 124 处 → localStorage 键（含迁移）
5. **P1 源码内部**：bot 协议/ID/环境变量（**必须成对改**）→ 日志 target → 注释
6. **P2 更新器**：关闭上游检查
7. **P2 文档/归档**：路径、文件名、历史处理
8. **编译 + 全量回归**：五开登录、防顶号、关浏览器释放、GPU 偏好、证书信任、快捷方式

---

## 八、风险清单（改名时最容易踩的坑）

| 风险 | 说明 |
|---|---|
| **证书链断裂** | 重签 CA 后忘记装进受信任根 → MITM HTTPS 全挂，游戏连不上 |
| **bot 协议不同步** | `@@AKAGI_NOTIFY@@`、`AKAGI_BOT_CONFIG`、`.akagi/venv` 只改一端 → bot 起不来 |
| **内置 bot ID 不匹配** | 改 `akagi-native` 但没改 config.toml 的 `active_4p` → 用不了内置 bot |
| **localStorage 全清** | 改 15 个键 → 主题/布局/侧栏偏好重置（可接受，但要预告） |
| **GPU 偏好丢失** | exe 改名后旧注册表键失效 → 前端可能回落核显，需重新 `-Apply` |
| **路径硬编码遗漏** | 一个 `<安装目录>` 漏改 → 脚本找不到文件；建议用全局搜索验证残留为 0 |
| **上游署名误删** | LICENSE/NOTICE/公告历史里的 Akagi 是法律要求，不能抹 |
| **WebView2 目录** | 旧 `com.akagi.akagi` 残留可删；新 `com.xiao.aogara` 已是新偏好 |
