# Aogara 全量测试方案（改名后回归 + 功能盘点 + 三轮分工）

> **修复方案另见 [`Aogara-问题修复方案.md`](./Aogara-问题修复方案.md)** —— 本表记录「什么问题」，那份文档写「怎么改」，下下轮按它统一修复。

> 版本：v1（方案稿，2026-11-24 17:15）
> 状态：**方案轮已完成；排查 + 简单修复轮已于 2026-11-24 17:40 执行完毕**，结果见 §五《问题记录表》。
> 本轮完成：PRE 全过、A/B/C/D/E1/E2/E6/F1/G1/G2/H2/I1-I3 通过；简单修复 2 处（使用手册）；
> 配置副作用 5 处已全部还原；剩余问题留「下下一轮」批量完善。生产态已恢复（单实例、无游戏浏览器）。
> **补测（17:46-18:10）**：用户告知 accountA 已注册 → 实测确认登录成功（UID <UID>），
> 回填 config.toml 空 UID（简单修复第 3 处）；五开同时进游戏实测 3/5 进房成功、无顶号无崩溃，
> slot1 被大厅横幅遮挡触发安全保护、slot2 未进房，记为 P-16 留批量完善。
> **精修轮（18:15-18:36）**：按《Aogara-问题修复方案.md》修 P-02/04/05/06/08/16a，已编译部署新 exe；
> P-03 因非管理员需用户双击 `tools/set-debug-port.reg`；P-16b/P-01 待下轮。详见修复方案文档 v2。
> 背景：2026-11-24 16:45 完成 Akagi→Aogara 全量改名（根目录、exe、248 文件、系统层），
> 需要一次系统回归确认各功能是否仍正常，并把历史遗留问题一并盘点出来。
> 依据文档：`Aogara-技术文档-主文档.md`（§13/§18/§19/§20）、`使用手册.md`（§8/§9）、
> `个人产物化改造方案.md`、`Aogara-全量改名清单.md`、`代码逻辑优化方案.md`。

---

## 零、三轮分工与铁律（最重要，先读）

| 轮次 | 范围 | 允许做 | 禁止做 |
|---|---|---|---|
| **本轮**（当前） | 只出方案 | 只读调研、写本文档 | 不改代码/配置、不启停程序、不跑测试 |
| **下一轮**（排查+简单修复） | 按本方案逐项测，记录问题 | ① 只读观察 ② **简单修复**（见判定标准）③ 填《问题记录表》 | 不改架构、不做多文件重构、不在单个问题上超过时间盒 |
| **下下一轮**（批量完善） | 汇总所有记录的问题，批量修 | 重构、多文件改动、重新编译部署 | — |

### 0.1 简单修复判定标准（满足全部才算"简单"，否则只记录）

1. **单文件、单处**改动（改一个字符串 / 一个常量 / 一行配置 / 一个路径）；
2. **不需要重新编译 Rust**（改配置 .toml / 文档 / 脚本 .js/.ps1/.bat 可以；
   改 .rs 或前端 .ts/.tsx 需要重编译 → **只记录，不下轮修**）；
3. 改动**明显可逆**（改前值记进《问题记录表》的"原值"列）；
4. 不涉及账号凭据、网络镜像、证书信任等外部依赖。

> 举例：某个脚本里漏改的 `<安装目录>` 路径 → 简单修复（改 .js，单处）。
> 反例：某功能逻辑 bug 需要改 `pool/mod.rs` → 只记录，留给下下一轮。

### 0.2 时间盒（不纠结原则）

- 单个测试项：**默认 5 分钟**，需要真局的最多 **10 分钟**（扩到 15 分钟必须记录原因）。
- 单个问题排查：**最多 2 次尝试**，仍无结论就填表跳过。
- 遇到"卡资源加载/网络慢/90s 登录超时"这类**环境问题**：立即停止等待，标记为 `ENV` 环境问题，不算功能失败。
- 遇到需要真局才能验证、但本机资源初始化慢的项：用测试模式（§20.1）的 `force_exit` 快进，或标记 `DEFER` 待下下一轮。
- **总原则：记录优先于修复，广度优先于深度。** 先把所有项扫一遍，问题攒够一批再统一处理。

### 0.3 问题记录格式（下一轮逐条填）

在本文档末尾《问题记录表》追加行，字段：

```
编号 | 测试项 | 严重度(P0/P1/P2) | 类型(BUG/ENV/DEFER/DOC) | 现象 | 复现步骤 |
证据(日志行/截图/端口) | 原值(若改) | 简单修复内容(或"留批量") | 状态(已简单修/待批量/环境限制)
```

严重度定义：
- **P0** 功能完全不可用（程序起不来、五开全崩、账号全登录失败）
- **P1** 主要功能受损（某会话顶号、进房失败、自动打牌不动）
- **P2** 体验/文案/边角（按钮文案、日志前缀、历史文件残留）

---

## 一、测试前置检查（下一轮开始时先做，5 分钟）

| 编号 | 检查项 | 方法 | 通过标准 |
|---|---|---|---|
| PRE-1 | 当前生产实例状态 | `Get-Process aogara` | 有且仅有一个 aogara.exe；无 akagi.exe |
| PRE-2 | 目录与 exe | `ls <安装目录>\app\aogara.exe` | 存在，版本 1.1.0 |
| PRE-3 | 无活局 | 查 63503–63507 端口 | 全部 down（从干净状态开始） |
| PRE-4 | 配置路径一致 | grep `akagi` in `configs/*.toml` | 0 处（构建工具链除外） |
| PRE-5 | 备份可用 | `ls F:\aogara-pre-rename-backup` | 存在 |
| PRE-6 | 调试端口方案 | 决定用哪种启动方式 | 见 §1.1 |

### 1.1 启动方式选择（测试需要 CDP 观察）

- **方式 A（推荐，观察用）**：`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=63100 aogara.exe`
  —— 可直连 UI 页做 CDP/RT.evaluate，`_pool-ipc.js` 可用；测完停掉换方式 B。
- **方式 B（生产态）**：直接 `aogara.exe` —— 不暴露 UI 调试口，验证真实用户路径。
- 两者都要测：A 用于深入排查，B 用于确认默认启动没问题。

---

## 二、测试用例矩阵（按模块，标注优先级与预计耗时）

> 优先级：**P0 必测**（改名直接影响或核心功能）；**P1 应测**；**P2 有余力测**。
> 每项给出：编号 / 测什么 / 怎么测 / 通过标准 / 失败时看什么 / 优先级。

### 模块 A：品牌与改名完整性（改名直接风险，全 P0）

| 编号 | 测什么 | 怎么测 | 通过标准 | 失败看 |
|---|---|---|---|---|
| A1 | 窗口标题与进程名 | CDP `/json/list` 看 title；tasklist 看进程名 | title=Aogara、进程=aogara.exe | tauri.conf/index.html |
| A2 | logo 视觉 | 打开 UI 截图侧栏+总览大字 | 显示 Aogara 字样，无 Akagi | logo SVG wordmark |
| A3 | 版本号与衍生标识 | 看侧栏底部徽标、hover tooltip | v1.1.0、tooltip 含"基于 Akagi 修改" | Sidebar.tsx / i18n |
| A4 | 配置路径全部可用 | 启动后 `get_config`，看 multi.sessions 加载数 | 5 会话加载、无路径报错 | configs/*.toml 路径 |
| A5 | 脚本路径可用 | 跑 `_pool-ipc.js list_sessions`、`gpu-pref.ps1 -Status` | 无 ENOENT、无旧路径 | 脚本里 <安装目录> 路径 |
| A6 | 桌面启动器 | 双击 `Aogara.bat` | 能启动 aogara.exe（或验证内容正确） | bat 内容 |
| A7 | GPU 偏好 | `gpu-pref.ps1 -Status` | 新 exe 键存在、GpuPreference=2 | 注册表 |
| A8 | exe 无旧标识 | grep 二进制字符串 | 无 `akagi/`/`Akagi Proxy CA`/`@@AKAGI_NOTIFY@@` | 需重编译→记录 |
| A9 | 构建脚本可用 | 检查 `_rust-only.cmd` 的 SRC/TB | SRC 指 Aogara-3、TB 指 AkagiBuildTools（有意保留） | cmd 路径 |
| A10 | 残留 akagi 盘点 | find 文件名 + grep 内容（排除历史/构建工具） | 活跃文件 0 处；历史/归档保留项符合清单 | 全量改名清单 |

### 模块 B：启动与单实例

| 编号 | 测什么 | 怎么测 | 通过标准 | 优先级 |
|---|---|---|---|---|
| B1 | 干净启动 | 方式 B 启动，看日志无 error | 起到位、无 panic、无 WebView2 错误 | P0 |
| B2 | 单实例保护 | 启动后再双击一次 | 第二实例聚焦已有窗口后退出，不双开 | P0 |
| B3 | 首次设置向导 | 若 `first_run_complete=false`，走一遍 6 步 | 能完成、写回配置 | P1 |
| B4 | 生产无测试日志 | 启动后查 `logs/test/run-*` 新增 | 不生成新 test run | P0 |
| B5 | 重启复用浏览器 | 开一个浏览器→停 aogara→重启 | 日志 `reusing idle browser`，不重复拉起 | P1 |

### 模块 C：抓包（capture）

| 编号 | 测什么 | 怎么测 | 通过标准 | 优先级 |
|---|---|---|---|---|
| C1 | Chromium 后端启动 | 开 slot2 浏览器，看端口 63504 | 浏览器起来、CDP 可连 | P0 |
| C2 | 帧捕获 | 进大厅后看 inspector.jsonl / all.log 增长 | 有 mjai/bot 事件（lite 模式） | P0 |
| C3 | 登录态保留 | 读 slot2 localStorage | account/uid/token 齐全（UID <UID>） | P0 |
| C4 | 强退浏览器释放 bot | 关 slot2 浏览器前后看 autoplay | true→false，日志 `autoplay and bot released` | P0 |
| C5 | MITM 模式（可选） | 切 `mode="mitm"` 启动，验证证书 CN | ca 目录生成 aogara-ca.*、CN=Aogara Proxy CA；`/ping` 返 pong | P1 |
| C6 | 证书信任 | 若测 MITM，检查游戏能否加载 | 无证书错误（需装 ca/crt 到受信任根） | P1 |

> C5/C6 注意：切 MITM 会改配置，测完必须切回 `mode="chromium"`；证书重装属"外部依赖"，本轮只验证 CN 生成，不强求信任链（标记 ENV 也可）。

### 模块 D：号池与账号隔离（改名后 bot ID/协议改动的风险区）

| 编号 | 测什么 | 怎么测 | 通过标准 | 优先级 |
|---|---|---|---|---|
| D1 | precheck 门禁 | `_pool-ipc.js precheck_sessions` | ok=true、5 账号身份唯一、无 conflicts | P0 |
| D2 | 内置 bot ID 匹配 | 看配置 active_4p/3p 与源码常量 | 都是 aogara-native / aogara-native3p；Bot 页能选中 | P0 |
| D3 | 账号身份判据 | 读各 slot UID，与配置比对 | 4 个已注册账号 UID 各不相同、无顶号 | P0 |
| D4 | 一键开浏览器错峰 | UI 点"一键打开浏览器"，计时端口出现 | 5 个依次出现、间隔约 1.2s、profile 不串 | P0 |
| D5 | 一键取消排队 | 启动后 2s 点取消 | 未启动 slot 不再启动、已启动按 busy 语义取消 | P1 |
| D6 | 顶号门禁（并发） | 两个 slot 同时请求同一账号 | 第二个被拒、日志 `账号正在被 X 启动` | P1 |
| D7 | 换号/登录 | 对某 slot 触发换号 | 表单自动填入、换号成功或明确报错（1002=未注册属正常） | P1 |
| D8 | accountA 状态 | 观察 slot1 | 原为「账号未注册无法登录 ENV」——**18:10 已过期**：用户补注册，实测登录成功（UID <UID>），详见 P-15 | P2 |

### 模块 E：进房与自动打牌（需真局，环境受限时 DEFER）

| 编号 | 测什么 | 怎么测 | 通过标准 | 优先级 |
|---|---|---|---|---|
| E1 | dryRun 进房走位 | `session_pool_start dryRun=true` | 停在铜之间模式列表，不误入教程/活动/客服/商店 | P0 |
| E2 | 真实进房 | 一键开始对局，观察 in_game | 有账号进入对局；卡资源加载则标 ENV/DEFER | P1 |
| E3 | HUD 建议 | 真局中看浮窗 | 显示向听/危险度/推荐切牌 | P1 |
| E4 | 自动打牌点击 | 真局中看 bot 是否出牌 | 牌河增长、无 dead_click/stale | P1 |
| E5 | 结算与连打 | 一局结束看 finish_count、auto_next | 结算后自动开下一局 | P1 |
| E6 | 软停 | 对局中点停止 | stop_after_game=true，本局结束再停 | P1 |
| E7 | 定时停止 | 配短时长测试 | 到点等本局结束再收尾 | P2 |

> E2–E7 依赖本机 majsoul 资源初始化速度。若卡"正在初始化游戏资源 XX%"，**立即标 ENV/DEFER 跳过**，
> 改用测试模式 `test-mode.five.toml` 的 `force_exit` 机制（§20.1）在下一轮快速覆盖阶段跳转。

### 模块 F：多开会话与动态管理

| 编号 | 测什么 | 怎么测 | 通过标准 | 优先级 |
|---|---|---|---|---|
| F1 | 会话列表 | 多开页看 5 个 slot | 账号/状态显示正确 | P0 |
| F2 | 动态添加/删除会话 | create_session / remove_session | 端口分配、注册表、墓碑恢复闭环 | P1 |
| F3 | 事件总线隔离 | slot3 启动看 slot1 是否被连带 | 无连带开局 | P1 |
| F4 | 对局页多账号切换 | /game 顶部下拉切 slot | 各 slot 快照独立 | P1 |
| F5 | 前端轮询降载 | 观察空闲时 IPC 频率 | 5s 兜底 + 事件驱动，隐藏页暂停 | P2 |

### 模块 G：日志 / Inspector / 测试模式

| 编号 | 测什么 | 怎么测 | 通过标准 | 优先级 |
|---|---|---|---|---|
| G1 | Inspector lite 内容 | 真局后看 inspector.jsonl | 有 mjai/bot/http，无原始 ws_frame | P1 |
| G2 | 双文件 flush | 看 inspector.jsonl 与 sensitive/inspector.jsonl | full 不滞后（独立 last_flush） | P1 |
| G3 | 测试模式冒烟 | 用 test-mode.smoke.toml 启动 | events.jsonl/state.json 生成，state=finished | P1 |
| G4 | 测试模式续跑 | 用 resume 配置中途重启 | 从 state.json 续跑、已完成局不重跑 | P2 |
| G5 | 日志打包 | 日志页"打包日志" | zip 生成且剔除 sensitive/ | P2 |

### 模块 H：更新器与网络（改名后遗留风险）

| 编号 | 测什么 | 怎么测 | 通过标准 | 优先级 |
|---|---|---|---|---|
| H1 | 更新检查行为 | 启动后看日志 github API 请求 | 记录是否提示"有新版本"（1.1.0<3.8.0 隐患） | P1 |
| H2 | 镜像配置 | 设置页看镜像 | 大陆环境下 GitHub 请求走镜像 | P2 |
| H3 | bot 安装/下载 | 装一个 bot（可选） | 走镜像成功 | P2 |

### 模块 I：文档一致性（改名副作用的重灾区）

| 编号 | 测什么 | 怎么测 | 通过标准 | 优先级 |
|---|---|---|---|---|
| I1 | 主文档历史记录 | 读变更记录 15:17/16:45 条目 | 语义准确（没被批量替换篡改） | P1 |
| I2 | 方案文档状态 | 读产物化方案状态表 | 已完成/待定与事实一致 | P2 |
| I3 | 清单文档 | 读全量改名清单 | 标注"已完成"、保留项说明正确 | P2 |
| I4 | 使用手册路径 | grep `<安装目录>`、桌面 Aogara | 路径/启动说明正确 | P1 |
| I5 | 外链指向 | external.ts / Setup 页 | 记录 AkagiMS 推广卡片外链仍指上游（待决策） | P2 |

---

## 三、下一轮执行顺序（建议）

1. **PRE-1~6** 前置检查（5 分钟）
2. **模块 A** 品牌完整性（全 P0，约 15 分钟）——改名直接风险，先扫
3. **模块 B/C/D** 启动、抓包、号池（约 30 分钟，C5/C6 可选）
4. **模块 E** 进房（时间盒 10 分钟，卡住就 DEFER，转测试模式）
5. **模块 F/G/H/I** 其余（约 20 分钟）
6. 收尾：停测试实例、恢复生产态（方式 B 启动）、汇总《问题记录表》

**预计下一轮总耗时**：约 1.5–2 小时（含环境问题等待的保守估计；严格执行时间盒可压到 1 小时内）。

---

## 四、测试期间的"简单修复"操作规范

1. **改前记录**：任何简单修复，先把原值填进《问题记录表》"原值"列。
2. **只改一类**：一次只处理一个问题，改完立即复测该项，通过就标记"已简单修"。
3. **配置改动要还原**：为测试临时改的配置（如 mode=mitm、close_idle_browser_secs），
   测完必须改回，并在表里注明"已还原"。
4. **需要重编译的一律不修**：改 .rs / .ts / .tsx 才能解决的问题，只写现象+证据+建议方向，留给下下一轮。
5. **不动账号凭据**：邮箱/密码/UID 一律不改；账号未注册标 ENV（注：accountA 已于 18:10 确认注册，UID <UID> 已回填，此例不再适用）。
6. **不装软件/改系统**：证书信任、WebView2 安装等属外部依赖，记录不执行。

---

## 五、问题记录表（排查 + 简单修复轮已填写，2026-11-24 17:40）

| 编号 | 测试项 | 严重度 | 类型 | 现象 | 复现步骤 | 证据 | 原值 | 简单修复内容 | 状态 |
|---|---|---|---|---|---|---|---|---|---|
| P-01 | A1 窗口标题 | P2 | BUG | CDP 页面标题正确为 Aogara，但 Windows 进程 MainWindowTitle 是 `com.xiao.aogara-siw` | 启动后取 `Get-Process aogara .MainWindowTitle`；CDP `/json/list` | CDP title=`Aogara`；OS title=`com.xiao.aogara-siw` | — | 留批量（疑似 WebView2 宿主窗口，需查 .rs/窗口配置） | 待批量 |
| P-02 | A3 衍生 tooltip | P2 | BUG | 四语 `sidebar.derivative` 被批量替换成「Aogara 基于 Aogara 修改」，上游名丢失 | 悬停侧栏版本徽标；读 i18n resources | `zh-CN.json:326` 等四语均有此自指文案 | `Aogara · 基于 Aogara 修改的非官方版本（Apache-2.0）` | 四语改回 Akagi，已重新 build | 已修复（18:36） |
| P-03 | PRE-6/A5 调试端口策略 | P1 | ENV | HKLM `AdditionalBrowserArguments` 仍是 `akagi.exe`/`akagi`/`com.akagi.akagi` 三个旧键，无 `aogara` 键；默认启动不带 63100 | 读注册表；`netstat` 查 63100 | 旧键三项目前仍生效；本轮只能靠环境变量兜底 | 旧键三项 | 已生成管理员脚本 `tools/set-debug-port.reg` + 备份 JSON | 待用户手动执行（非管理员，写入被拒） |
| P-04 | C4 批量关浏览器释放 | P1 | BUG | 单会话 `session_stop_capture` 会写 `autoplay=false` 并打印释放日志；但「一键关闭浏览器」只关浏览器，slot1/4/5 的 autoplay 仍为 true、配置未回写 | slot1/4/5 autoplay=true → 点「一键关闭浏览器」→ 查 list_sessions 与配置 mtime | 17:29:43 有 5 次 capture shutdown，但无 `[capture] browser closed...released`；17:31:08 单关 slot4 则正常 | — | `session_stop_capture` 改 finally 语义 + 前端 `closeAll` Promise.all 汇总 | 已修复（18:36，5 开批量关闭全释放） |
| P-05 | D5 取消排队 | P1 | BUG | 一键打开浏览器后 2s 点「一键取消启动」，仍连续打开 3 个浏览器；预期应立即停住排队 | 关全部→点「一键打开浏览器」→2s→点「一键取消启动」 | 17:31:59 开始，t=4s 时 63503/63504/63505 均已监听 | — | 拆 browserBatchToken + queuedRef + 取消按钮态 | 已修复（18:36，0.3s 取消拦住 4 个） |
| P-06 | H1 更新检查 | P1 | BUG | 更新源被批量改成不存在的 `shinkuan/Aogara`，直连+5 个镜像全部 404/403/超时；原先「1.1.0 < 3.8.0 误判」变成「检查彻底失败」 | 启动后看 stdout 中 github 请求 | `api.github.com/repos/shinkuan/Aogara/releases/latest` → 404；`gh-proxy.com` → 404；其余超时/403 | `shinkuan/Aogara` | `check_for_update` 直接 Ok(None) 关闭检查，留 TODO 引子 | 已修复（18:36，github 请求数=0） |
| P-07 | I4 使用手册速览表 | P2 | DOC | 表内版本写 v3.8.0、桌面入口写 `Aogara.lnk`，与实际 v1.1.0 和 `Aogara.bat` 不符 | 读 `使用手册.md` §0 | 第 18、23 行 | `Aogara **v3.8.0**（官方最新 Release）`；`%USERPROFILE%\Desktop\Aogara.lnk` | 改为 `Aogara **v1.1.0**（个人分支，基于 Akagi v3.8.0）`；改为 `Aogara.bat（因系统策略无法创建 .lnk）` | 已简单修 |
| P-08 | I5 外链指向 | P2 | DOC | `external.ts` 4 个外链仍指向上游 `shinkuan/Aogara` 与 `shinkuan/AogaraMS`；产物化方案已标「待决策」 | 读 `frontend/src/lib/external.ts` | `AOGARA_GITHUB_URL` 等 4 项 | — | 4 常量置空 + openExternal 空值保护，留 TODO 引子 | 已修复（18:36，用户决定留空） |
| P-09 | 测试副作用还原 | — | ENV | 逐个关浏览器会写 `autoplay=false`，真实进房会写 slot2 `autoplay=true`；测试后已全部手工还原 | 对比测试前后各 slot 配置 | `config.toml`/`slot-{accountA,alt3,accountB}.toml` 改 true；`slot-main.toml` 改回 false | 测试前：slot1/3/4/5=true，slot2=false | 5 处配置已还原，生产实例已重启读取 | 已完成 |
| P-10 | E3/E4/E5/E7 真局细节 | P1 | DEFER | E2 真实进房成功（bot=aogara-native，state=ready）；但观察时 `turn_count=0`，未等到牌河增长与结算 | `session_pool_start dryRun=false` 后进房；`get_session_snapshot` | 17:34:00 `entered=true`；软停返回 `stop_after_game=true`、`paused=true` | — | 下轮用 `test-mode.five.toml` 的 force_exit 快进 | 环境/时间限制 |
| P-11 | B3/B5/F2/F3/G3/G4 | P2 | DEFER | 首次向导、空闲浏览器复用、动态增删会话、事件总线隔离、测试模式 smoke/resume 本轮未触发 | — | — | — | 留批量 | 未测 |
| P-12 | C5/C6 MITM | P1 | DEFER | 未切 `mode="mitm"`，避免证书信任等外部依赖 | — | 当前 `mode="chromium"` | `mode="chromium"` | 留批量 | 未测 |
| P-13 | G5 日志打包 | P2 | DEFER | 点「打包日志」后弹出原生保存对话框，无头环境无法完成，未验证 zip 内容与 sensitive 剔除 | 日志页 → 打包日志 → 打包… | 对话框存在，选中运行记录列表正常 | — | 留批量（需人工交互） | 未测 |
| P-14 | D6 并发同账号锁 | P1 | DEFER | precheck 显示 5 账号身份唯一（4 UID + 1 未注册邮箱），无法构造同账号并发请求 | `precheck_sessions` | `ok=true`、`conflicts=[]` | — | 留批量 | 未测 |
| P-15 | D8 accountA | P2 | ENV→已修 | 原记录「未注册」已过期：用户补注册后复验，账号登录成功（大厅昵称「玩家A」），真实 UID=<UID>；但 config.toml 中该账号 `uid=""` 为空，precheck 只能退回邮箱判重 | 单独开 slot1 → 看大厅截图 → 读 `[pool] identity verified for accountA (uid=Some("<UID>"))` | `config.toml:294 uid=""`；日志 `all.jsonl` 有 identity verified 行 | `uid = ""` | 回填 `uid = "<UID>"`，重启后 precheck 5 账号全部为 `uid:*`、无冲突 | 已简单修 |
| P-16 | 五开同时进游戏（补测） | P1 | 部分BUG+ENV | 5 浏览器全开后一键开始对局：slot3/4/5 成功进房且 bot=aogara-native ready；slot1 被游戏「新赛季开始」大横幅遮挡，程序报「无法确认当前大厅画面，已取消点击避免点错」而拒绝进房（安全保护生效）；slot2 大厅干净但未进房（匹配/cancelUnifiedMatch 循环，原因未定位）。无顶号、无 5 开崩溃 | 一键打开 5 浏览器 → 一键开始对局 → 观察 list_sessions + 截图 | 18:06 时 slot3/4/5 `in_game=true` bot ready；slot1/2 `in_game=false`；slot1 截图有大横幅 | — | ①enter_room Unknown 延等待+ensure_lobby 恢复+dismiss 重试（已编译）；②slot2 匹配循环留待下轮 | 部分修复（横幅容错已改；本机资源加载慢未完整冒烟；slot2 待查） |

> 通过项摘要：PRE-1~6；A1~A10（除 P-01/P-02）、B1/B2/B4、C1~C4、D1~D4/D7、E1/E2/E6、
> F1/F5（静态）、G1/G2、H2、I1~I3。G1 实测 `inspector.jsonl` 15 行（http 11 + mjai_event 4，无 ws_frame），
> 普通与 sensitive 双文件行数一致。

---

## 六、本轮（方案轮）明确不做

- 不改任何代码 / 配置 / 文档（除本方案文档本身）；
- 不启停 aogara、不开浏览器、不跑任何测试；
- 不连接口、不改注册表、不动证书；
- 不清理任何历史文件。

等下一轮按本方案开始排查与简单修复。
