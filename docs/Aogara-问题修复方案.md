# Aogara 问题修复方案（下下轮统一修复作业指导书）

> 版本：v2（2026-11-24 18:36，**精修轮已执行**）
> 来源：`Aogara-全量测试方案.md` §五《问题记录表》P-01~P-16 + 补测（17:46–18:10）复盘。
> 用途：v1 是作业指导书；v2 记录精修轮实际改动与验证结果。
> 配套：现象/证据看测试方案的问题记录表。
> 铁律：修复前先复现、先记录原值；一次一个问题；改完必须验证；涉及凭据/证书/系统全局改动要先问用户。

## 精修轮执行结果（2026-11-24 18:15–18:36）

用户拍板：P-06 关闭更新检查留引子、P-08 外链留空留引子、P-03 允许改 HKLM。
已编译部署新 exe（`app/aogara.exe`，备份 `aogara.pre-precision.exe`），生产态已恢复。

| 问题 | 状态 | 实际改动 | 验证结果 |
|---|---|---|---|
| P-02 | ✅ 已修复 | 四语 `i18n/resources/*.json` 的 derivative 改回 Akagi | UI tooltip 实测显示「Aogara · 基于 Akagi 修改的非官方版本（Apache-2.0）」 |
| P-08 | ✅ 已修复 | `lib/external.ts` 4 常量置空 + TODO 引子；`openExternal` 加空值/非 http 保护 | 源码确认 4 常量为 `''`；空链接点击 no-op |
| P-06 | ✅ 已修复 | `commands.rs` UPSTREAM_REPO 保留 + `check_for_update` 直接 `Ok(None)` + TODO 引子 | 启动日志 github 请求数 = **0**，无更新红点 |
| P-04 | ✅ 已修复 | `session_stop_capture` 强退结果存 `reclaim_result`、状态回写与 bot 释放不再被跳过、末尾才 `?`；前端 `closeAll` 改 `Promise.all` + `closeBrowser` 返 bool 汇总失败 | 5 开→批量关闭：5 slot autoplay 全 false，4 条 release 日志，4 个配置回写 ✅ |
| P-05 | ✅ 已修复 | 拆 `browserBatchToken`、加 `queuedRef` 同步、`openingAll` 取消态与 `cancelOpenAll`、start/open 改读 ref | 0.3s 点「取消打开浏览器」后仅 1 个已启动，其余 4 个被拦住 ✅ |
| P-16a | ⚠️ 已改代码，环境受限未完整冒烟 | `enter_room` Unknown 时延长等待到 14s（慢加载 splash 也属 Unknown）→ 走 `ensure_lobby` 安全恢复 → `dismiss_popups` → 重分类；保留最后 fail-safe | 逻辑已编译生效；本机 slot3 卡「下载网络资源 10/10」启动页，资源加载慢属 ENV，未能跑到大厅验证横幅场景 |
| P-16b | ⏸ 未改代码 | 槽位匹配循环根因未定位，日志已足够（每次 Lobby 请求/响应带 ID） | 留待下轮真机观察 `cancelUnifiedMatch`/`startUnifiedMatch` 时序 |
| P-03 | ⛔ 需用户手动执行 | 已备份旧值到 `tools/WebView2-AdditionalBrowserArguments.backup-20261124-1829.json`；生成管理员一键脚本 `tools/set-debug-port.reg` | 当前进程非管理员，HKLM/HKCU Policies 写入被拒；**双击 .reg 并批准 UAC 后**重启即可生效 |

**编译备忘**：首次编译因 target 缓存里硬编码旧路径 `<安装目录>\...\tauri\16579af44bf09570\out\permissions` 失败；
删除该陈旧 hash 目录后重编成功（frontend `npm run build` 5s + cargo release ≈2–5min）。
`_rust-only.cmd` 末尾 `reg.exe` 仍被安全黑名单拦截，但产物已生成，手动复制即可。

---

## 零、下下轮开工前必读

### 0.1 修复顺序建议（按依赖与风险）

| 批次 | 问题 | 理由 |
|---|---|---|
| 第 1 批（纯前端/文案，改动小） | P-02、P-08 | 无 Rust 逻辑，改完重新 `npm run build` 即可 |
| 第 2 批（Rust 逻辑，核心体验） | P-04、P-05、P-16 | 需重编译，互相独立，可一次编译覆盖 |
| 第 3 批（系统/外部依赖） | P-03、P-06 | 改注册表和Updater策略，需用户确认后做 |
| 第 4 批（确认后再定） | P-01、P-10、P-11、P-12、P-13、P-14 | 先按本文「验证」步骤确认是否真问题，避免白改 |

### 0.2 每条修复的标准动作

1. **复现**：按问题记录表的复现步骤跑一遍，确认现象仍在。
2. **定位**：本文已给到 `文件:行号`，先读一遍当前代码，确认行号未漂移。
3. **记录原值**：把要改的原文/原配置值抄进测试方案问题记录表的「原值」列。
4. **改**：只改本文列出的方法，不顺手重构。
5. **验证**：按本文「验证方法」跑；涉及重编译的走 `_rust-only.cmd`。
6. **回填**：测试方案问题记录表「状态」改为「已修复（版本号）」，并写明验证结果。

### 0.3 编译与启动约定

- 编译：`<安装目录>\_rust-only.cmd`（SRC 指向 `<安装目录>/_setup/src/Aogara-3`，工具链在 `<安装目录>BuildTools`，**有意保留**不要改）。
- 调试启动（带 UI 调试口，用于 CDP/IPC 验证）：
  ```
  WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=63100 aogara.exe
  ```
- IPC 调用：`node <安装目录>\tools\_pool-ipc.js <command> '<jsonArgs>'`
- 日志：stdout 重定向到 `<安装目录>\app\aogara-*.log`；会话日志在 `app/logs/slotN/<时间戳>/`。

---

## 一、逐问题修复方案

### P-02｜i18n 衍生标识自我指代（文案错误）

- **根因**：全量改名把四语文案里的上游名 `Akagi` 一并替换成 `Aogara`，形成「Aogara 基于 Aogara 修改」的自指句。
- **位置**：
  - `frontend/src/i18n/resources/en.json:318` → `"Aogara · unofficial derivative of Aogara (Apache-2.0)"`
  - `frontend/src/i18n/resources/ja.json:298` → `"Aogara · Aogara を改変した非公式ビルド（Apache-2.0）"`
  - `frontend/src/i18n/resources/zh-CN.json:326` → `"Aogara · 基于 Aogara 修改的非官方版本（Apache-2.0）"`
  - `frontend/src/i18n/resources/zh-TW.json:298` → `"Aogara · 基於 Aogara 修改的非官方版本（Apache-2.0）"`
- **修改方案**：只把「基于/derivative of/改変した」**后面**的那个名字改回上游 `Akagi`，前面的品牌名 `Aogara` 保持不变：
  - en: `"Aogara · unofficial derivative of Akagi (Apache-2.0)"`
  - ja: `"Aogara · Akagi を改変した非公式ビルド（Apache-2.0）"`
  - zh-CN: `"Aogara · 基于 Akagi 修改的非官方版本（Apache-2.0）"`
  - zh-TW: `"Aogara · 基於 Akagi 修改的非官方版本（Apache-2.0）"`
- **注意**：`NOTICE` 里的衍生声明已经写对了（Based on Akagi v3），**不要动**；这里只改 i18n 四语。
- **验证**：`npm run build` → 启动 → 侧栏悬停版本徽标，四语切换各看一次 tooltip，均显示「基于 Akagi」。

### P-08｜外链仍指向上游（待用户决策后改）

- **根因**：全量改名时把 URL 里的 `Akagi` 改成 `Aogara`，但这些链接指向的是真实存在的上游仓库/社群，改名后变成死链；产物化方案里已标「待决策」。
- **位置**：`frontend/src/lib/external.ts:6-9`
  ```
  AOGARA_GITHUB_URL     = https://github.com/shinkuan/Aogara
  AOGARA_DISCORD_URL    = https://discord.gg/Z2wjXUK8bN
  AOGARAMS_GITHUB_URL   = https://github.com/shinkuan/AogaraMS
  AOGARAMS_DOWNLOAD_URL = https://github.com/shinkuan/AogaraMS/releases/latest
  ```
- **修改方案（需用户先决策）**：两条路线二选一：
  - **路线 A（推荐，若暂不发布）**：Aogara 自身链接保留但标注「非官方分支」，Discord 与 AogaraMS 链接改为上游真实地址 `https://github.com/shinkuan/Akagi` / `https://github.com/shinkuan/AkagiMS`（这些是真实存在的），避免死链；UI 上把卡片文案改为「上游社区」。
  - **路线 B（若用户已有自己的仓库/社群）**：把 4 个常量整体替换为用户自己的 URL，同时更新 `announcements/entries.ts` 里引用这些常量的卡片（D3 保留的历史条目只改引用名，不改历史文案）。
- **验证**：逐条点击外链，确认浏览器打开的页面存在（非 404）。
- **风险**：涉及对外展示，改前必须问用户走哪条路线。

### P-04｜一键关闭浏览器不释放 autoplay/bot（并发写配置）

- **现象**：单个会话「关闭浏览器」会写 `autoplay=false` 并打印 `[capture] browser closed for slotN; autoplay and bot released`；但「一键关闭浏览器」批量关闭后，slot1/4/5 的 autoplay 仍为 true，配置文件未回写。
- **根因（两处候选，已定位到代码，下下轮用日志二选一确认）**：
  1. **候选 A（更可能）——强退阶段提前返回跳过释放**：`session_stop_capture`（`src/ipc/commands.rs:1397-1485`）在 ③ 强退阶段 `reclaim_singleton`（1443 行）出错时 `return Err(...)` **直接返回**，1455-1483 行的 autoplay 释放逻辑被跳过。批量并发强退时日志出现多次 `chromium did not exit after SIGTERM` / `did not exit after taskkill, forcing /F`，说明该阶段确有失败，单个关闭时无竞争则正常。
  2. **候选 B——并发写覆盖**：`update_config_inner`（`src/ipc/commands.rs:279`）里 `persist_config(&new_config, &state.config_path)`，批量时 5 个调用并发写；需确认 `session_stop_capture` 里构造的 `sstate = state.with_session(s.clone())`（1466 行）其 `config_path` 确实指向各自 session 配置。单个关闭 slot3/4/5 已验证正确，故此候选可能性较低，但仍需确认。
- **修改方案**：
  - **主修（针对候选 A）**：把 autoplay 释放改成「无论强退成功与否都执行」的 finally 语义。具体：把 1433-1449 的强退块结果先存成 `let reclaim_result: Result<(), String>`，不要在其中 `return Err`；随后照常执行 1450-1483 的释放逻辑；最后 `reclaim_result?` 再决定是否向上返回错误。这样即使强退失败，机器人占用也一定被释放（宁可浏览器没关干净，也不能留着 bot 空转）。
  - **加固（针对候选 B）**：在 1475 行 `update_config_inner` 调用前打印 `tracing::debug!("[capture] releasing autoplay for {} via {:?}", s.id, sstate.config_path)`，下下轮跑一次批量关闭看日志里 5 个路径是否各不相同；若有重复，改为在 `session_stop_capture` 里显式传目标 config_path。
  - **前端兜底**：`closeAll`（`frontend/src/routes/Sessions.tsx:366-376`）目前 `for (const row of targets) void closeBrowser(row)` 并发且无结果汇总。改为 `await Promise.allSettled(...)` 后统一 `refresh()`，并把失败的 slot 用 toast 列出，便于发现「没关干净」的会话。
- **验证方法**：
  1. 带调试口启动 → 开 5 浏览器 → IPC 确认各 `autoplay=true`。
  2. 点「一键关闭浏览器」→ 等全部关闭。
  3. 读 5 个 slot 配置的 `[autoplay] enabled` 均为 false；`list_sessions` 各 `autoplay=false`。
  4. 日志里出现 5 条 `[capture] browser closed for slotN; autoplay and bot released`。
- **风险**：改 Rust 需重编译；释放逻辑前移后，强退失败仍会返回错误，前端需正确提示（已在兜底里处理）。

### P-05｜一键打开后「一键取消启动」未立即停住排队

- **现象**：「一键打开浏览器」后 2s 点「一键取消启动」，仍连续打开 3 个浏览器。
- **根因**：两个批量操作共用同一个 `batchToken`（`Sessions.tsx:111`）：
  - `runStaggered`（188-203）每轮检查 `if (batchToken.current !== token) return` 来中断错峰。
  - `cancelAllBusy`（388-404）`batchToken.current += 1` 想中断队列，但它同时把 `rows.filter(r => busy.has(r.slot))` 的会话送去 `stop(row)`（即 `session_pool_stop`，停用**对局**）。
  - 结果：取消动作主要面向「对局批量」，对「浏览器批量」的取消只靠共享 token 的副作用，且 `setQueued(new Set())` 是异步 state 更新，与 `openBrowser` 里的 `if (queued.has(row.slot))`（251 行）检查存在竞态；已 launch 的 action 无法撤回。
- **修改方案**：
  - **拆分 token**：新增 `browserBatchToken = useRef(0)`，让 `openAll`/`closeAll` 的错峰用独立 token；`startAll`/`cancelAllBusy` 继续用 `batchToken`。两个批量互不影响。
  - **按钮语义补齐**：给「一键打开浏览器」按钮加取消态——当 `openingAll` 为 true 时按钮变「取消打开浏览器」，点击只 `browserBatchToken.current += 1` + `setQueued(new Set())`，**不调用** `stop(row)`（不碰对局）。
  - **竞态修复**：`runStaggered` 里 `markQueued` 与 `openBrowser` 的 `queued.has` 检查改用 ref 同步值（如 `queuedRef.current`），避免 React state 异步导致漏判。
  - **错峰粒度**：`BATCH_STAGGER_MS=1200` 与后端浏览器启动（约 2s/个）叠加，2s 内可能已启动 2 个；这是预期，取消只能拦住「尚未启动」的，UI 文案要写清「取消剩余排队」，不要承诺立即全停。
- **验证方法**：关闭全部浏览器 → 点「一键打开浏览器」→ 1s 内点「取消打开浏览器」→ 观察端口只出现 ≤1 个；再点一次正常打开，5 个依次出现。
- **风险**：纯前端改动需重新 build；注意别破坏 `startAll` 的 precheck 门禁（P-01 相关逻辑不要动）。

### P-16｜五开同时进游戏：横幅遮挡 + slot2 未进房

#### 16a. slot1 被「新赛季开始」横幅挡住 →「无法确认当前大厅画面」

- **根因**：`enter_room`（`src/pool/mod.rs:830-852`）先 `probe_ui_screen` 分类画面；横幅覆盖了 `UI_SAMPLE_POINTS`（1011-1017）的采样点，使 `classify_ui`（1074-1115）算出的距离不满足 `best<=52 且 best<=second*0.85`，返回 `UiScreen::Unknown`，触发 850 行的 fail-safe「已取消点击以避免点错」。这是**安全设计生效**，不是 bug，但缺少「遇到未知画面先尝试关弹窗再重试」的容错。
- **修改方案（逻辑增补）**：
  1. `UiScreen` 新增变体 `Banner`（赛季/活动横幅），在 `ui_signature`（1022）里加一组从真实横幅截图采样的 5 点颜色；`classify_ui` 的候选列表（1082-1089）加入 `Banner`。
  2. `enter_room` 的 match（840-852）分支：`UiScreen::Banner | UiScreen::Lecture | UiScreen::Activity` 统一走「先关弹窗再重试」——调用已有的 `dismiss_popups`（约 1217 行附近的两步 reset）后 `pause`，再 `probe_ui_screen` 一次；仍非 Lobby/Game 才报错。
  3. **通用兜底**（更省事、推荐）：对 `UiScreen::Unknown` 也先试一次 `dismiss_popups` + 重新 probe；因为 Unknown 的常见来源就是临时横幅/过场，直接放弃太保守。保留最后仍 Unknown 才报错的 fail-safe，不降低安全性（关弹窗用的是已验证的安全坐标/Escape，不是盲点 ranked）。
  4. 增加横幅特征样例的可维护性：把签名颜色抽成常量表并加注释「采样自 2026-11-24 新赛季横幅」，方便以后版本更新时替换。
- **验证方法**：让 slot1 停在有横幅的大厅 → 单独 `session_pool_start`（dryRun=false）→ 应能自动关横幅后进房，`in_game=true`；再用旧存档确认普通大厅仍正常，不会误关。
- **风险**：`dismiss_popups` 的 Escape 在某些页面可能打开别的菜单，需回归验证低风险的「大厅干净时」场景不受影响。

#### 16b. slot2 在大厅但未进房（匹配循环）

- **现象**：slot2 大厅画面干净、已登录（UID <UID>），一键开始后日志持续出现 `.lq.Lobby.cancelUnifiedMatch` / `fetchCurrentMatchInfo`，但不进房，`last_enter=null`。
- **根因（未完全定位，下下轮按此路径查）**：可能是上一次强退遗留的匹配请求在服务端仍挂起，进房代码先 `cancelUnifiedMatch` 清理再 `startUnifiedMatch`，清理与新请求竞争导致一直被取消。
- **排查/修改路径**：
  1. 在 `enter_room` 附近（约 880-960）给 `cancelUnifiedMatch` 与 `startUnifiedMatch` 的调用加 `tracing::debug`，记录每次请求 ID 与响应。
  2. 检查是否需要等待 `fetchCurrentMatchInfo` 返回「无进行中匹配」后再发 `startUnifiedMatch`；若是竞态，加一个显式等待/重试（最多 2 次）。
  3. 若服务端确实残留匹配，考虑进房前固定等待 1–2s 或按账号错峰（已有 1.2s 错峰，可能不够）。
- **验证方法**：单独对 slot2 连续发起 3 次进房，均能进入；5 开时 slot2 不再停留匹配循环。
- **风险**：涉及匹配协议时序，改动需在真局环境验证，时间盒 10 分钟，卡住就记录不纠结。

### P-06｜更新检查指向不存在的仓库（版本误判/检查失败）

- **根因**：全量改名把 `UPSTREAM_REPO` 从 `shinkuan/Akagi` 改成 `shinkuan/Aogara`，后者不存在（404），直连+5 镜像全失败。原始隐患是 1.1.0 < 上游 3.8.0 会误判「有更新」，并把 fork 覆盖成原版。
- **位置**：`src/ipc/commands.rs:3004 const UPSTREAM_REPO: &str = "shinkuan/Aogara";`；调用在 3012-3027 的 `check_for_update`。
- **修改方案（需用户先决策，产物化方案里 P2 已列待定）**：两条路线：
  - **路线 A（推荐，个人产物）**：**关闭自动更新检查**。加配置项 `[updater] enabled = false`（默认 false），`check_for_update` 在 disabled 时直接返回 `Ok(None)`；前端 `updaterStore` 不再弹更新提示、侧栏不显示红点。彻底避免误判和覆盖风险。
  - **路线 B（保留上游检查）**：把 `UPSTREAM_REPO` 改回 `shinkuan/Akagi`，同时在 `build_update_info`（`src/updater/check.rs`）加「fork 版本不参与上游版本比较」的判断——当 `CARGO_PKG_VERSION` 主版本与上游不同源（1.x vs 3.x）时直接返回 None，只提示「可前往上游查看」不提供自动覆盖。
- **验证方法**：启动后看日志不再有 github release 404/403 请求；侧栏无更新红点；`check_for_update` 返回 null。
- **风险**：路线 A 改配置结构需同步 `config.toml` 与前端 store；路线 B 需重编译且要防止误更新。

### P-03｜WebView2 调试端口注册表仍是旧键（系统层）

- **根因**：HKLM `SOFTWARE\Policies\Microsoft\Edge\WebView2\AdditionalBrowserArguments` 下只有 `akagi.exe`/`akagi`/`com.akagi.akagi` 三个旧值，没有 `aogara` 新键；默认启动不带 63100 调试口，只能靠环境变量兜底。
- **修改方案（改系统注册表，需用户确认后执行）**：
  - 用 `gpu-pref.ps1` 同款的注册表写法，新增三个值：`aogara.exe`、`aogara`、`com.xiao.aogara`，数据均为 `--remote-debugging-port=63100`。
  - 旧键可保留（指向不存在的 exe，无害）或一并删除；删除前先导出备份。
  - 同步更新 `docs/Aogara-全量改名清单.md` 系统层表格（现缺此项）。
- **验证方法**：重启 aogara（不带环境变量）→ `curl http://127.0.0.1:63100/json/version` 返回 200。
- **风险**：改 HKLM 需管理员权限；**属于系统全局改动，本轮和下下轮动手前必须先问用户**。

### P-01｜OS 层窗口标题 `com.xiao.aogara-siw`（先确认是否误报）

- **现象**：CDP `/json/list` 的 title 正确为 `Aogara`，但 `Get-Process aogara .MainWindowTitle` 得到 `com.xiao.aogara-siw`。
- **判断**：`-siw` 疑似 WebView2 的隐藏宿主窗口（Shell Integration Window），`MainWindowTitle` 取到了它而非真实窗口；用户实际看到的窗口标题可能是正确的（截图/UI 显示 Aogara）。**大概率是误报，不要急着改。**
- **验证方法（先做这一步）**：用 Win32 `EnumWindows` 枚举该进程所有**可见**窗口的标题，确认是否存在标题为 `Aogara` 的可见窗口。若有 → 关闭本条，不改代码。
- **仅当确认真实窗口标题错误时的修改方案**：在 `src/lib.rs` 窗口创建后显式 `window.set_title("Aogara")`（Tauri 2 API），或检查 `tauri.conf.json` 的 `app.windows[0].title`（当前已是 "Aogara"，19-28 行）。
- **风险**：低；先验证再决定是否动。

### P-10 / P-11 / P-12 / P-13 / P-14｜未测/DEFER 项的补充测试方案

这些不是已确认的 bug，而是「还没验证到」，下下轮按下列方法补测，**测出问题再开新编号**：

| 编号 | 补测方法 | 通过标准 |
|---|---|---|
| P-10 E3/E4/E5/E7 | 用 `test-mode.five.toml` 的 `force_exit` 快进；真局中看 HUD 浮窗、牌河增长、结算 finish_count、auto_next | 有建议/危险度/推荐切牌；牌河增长；结算后自动下一局 |
| P-11 B3 首次向导 | 临时把 `first_run_completed=false`（**测完改回**），走 6 步 | 能完成并写回配置 |
| P-11 B5 复用浏览器 | 开一浏览器→停 aogara→重启 | 日志 `reusing idle browser`，不重复拉起 |
| P-11 F2 动态会话 | `create_session`/`remove_session` | 端口分配、墓碑恢复闭环（**测完删掉测试会话**） |
| P-11 F3 事件总线隔离 | slot3 启动看 slot1 是否被连带 | 无连带开局 |
| P-11 G3/G4 测试模式 | `test-mode.smoke.toml` / `resume.toml` | events.jsonl/state.json 生成、续跑不重跑 |
| P-12 C5/C6 MITM | 切 `mode="mitm"` 看 CA 生成（`mode="chromium"` 原值记录，**测完切回**） | CN=Aogara Proxy CA；游戏无证书错误（信任链属外部依赖，不强求） |
| P-13 G5 日志打包 | **需人工交互**：日志页点「打包日志」→ 在原生保存对话框选路径 | zip 生成且 `sensitive/` 被剔除 |
| P-14 D6 并发同账号锁 | 构造两个 slot 同时请求同账号（可临时改配置，测完还原） | 第二个被拒、日志 `账号正在被 X 启动` |

---

## 二、上轮踩坑清单（避免重蹈覆辙）

> 这些是补测（17:46–18:10）中踩过的操作坑，写在这里让下下轮少走弯路。

1. **UI 不在目标页时按钮点击无效**：`_pool-ipc.js --eval` 里 `find(b=>/一键打开/.test(...))` 若 UI 停在总览页会返回 undefined，静默失败。**先 `location.hash='#/sessions'` 再点**，并二次确认按钮存在。
2. **资源加载慢导致误判**：5 开同时初始化会触发「获取版本资讯失败」弹窗，别当成账号/程序 bug；先独占网络或等缓存。slot1 曾卡在 94% 下载。
3. **强退对局后重启会自动恢复对局**：`game_state::restore` 重放 mjai 日志并自动重开浏览器（slot3/4/5）。收尾时只 kill 浏览器进程、保留生产实例即可，不要因此反复重启。
4. **测试会改配置，务必还原**：关浏览器写 `autoplay=false`；真实进房写 `autoplay=true`（slot2）。测完对照测试前值逐个还原（`config.toml`/`slot-main`/`slot-accountA`/`slot-alt3`/`slot-accountB`）。
5. **PowerShell 保留变量 `$pid` 只读**：关闭端口占用进程时用自定义变量名（如 `$owner`），别写 `$pid = ...`，否则杀掉自己。
6. **从 Bash 调 PowerShell 会被安全策略拦截**：查版本/注册表/进程用 PowerShell 工具，不要 `bash` 里 `powershell -Command`。
7. **账号身份判据已确立**：`webgl_crash_user_v1` 的 UID 才是权威；`localStorage.account` 可能是邮箱/昵称。`accountA` 真实 UID=<UID>，已回填 config.toml（本轮简单修复）。
8. **误报要验证再改**：P-01 窗口标题、P-04 批量释放，都要先用日志/枚举确认根因，按本文「验证方法」走，别凭单一现象下结论。

---

## 三、通用排查工具箱（下次遇到问题直接套用）

| 需求 | 命令/方法 |
|---|---|
| 看 UI 真实标题/元素 | `node tools/_pool-ipc.js --eval "({title:document.title,...})"` |
| 调 Tauri 命令 | `node tools/_pool-ipc.js <cmd> '<json>'`（如 `precheck_sessions`/`list_sessions`/`get_session_snapshot`） |
| 看游戏页面画面 | `AOGARA_CDP_PORT=<端口> node tools/_cdp-shot.js <out.png>` + 看图 |
| 点游戏内坐标 | `AOGARA_CDP_PORT=<端口> node tools/_cdp-click.js <x> <y>` |
| 读游戏 localStorage | `AOGARA_UI_PORT=<端口> node tools/_pool-ipc.js --eval "({uid:localStorage.getItem('webgl_crash_user_v1'),...})"` |
| 看会话状态 | `list_sessions` / `get_session_snapshot` / 日志 `app/logs/slotN/<时间戳>/` |
| 看画面分类 | 日志里 `[pool] UI samples ... -> <screen>`（debug 级） |
| 看协议交互 | 日志 `aogara::bridge::majsoul` 的 `up REQUEST`/`down RESPONSE`/`down NOTIFY` |
| 查端口占用 | `netstat -ano | grep :<端口>`；Windows 侧 `Get-NetTCPConnection -LocalPort <p>` |
| 强退会话 | IPC `session_stop_capture`；对局中需先 `session_pool_stop`；必要时 `Get-Process aogara -Force`（用户允许强退时） |

---

## 四、本方案明确不做

- 本轮不改任何代码/配置/注册表（除本文档本身）；
- 不替用户决策外链去向（P-08）、更新器路线（P-06）、系统注册表（P-03）；
- 不在单个问题上超过测试方案规定的时间盒（单项 5 分钟、真局 10 分钟、最多 2 次尝试）。

下下轮按「第 1→4 批」顺序修复，每修一条回填测试方案问题记录表的「状态」列。
