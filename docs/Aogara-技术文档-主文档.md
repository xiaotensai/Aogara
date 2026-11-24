# Aogara v3.8.0 技术文档（原生号池整合版）

> **文档定位：当前唯一主文档。** 后续开发、排错、交接先看这份。
> 旧的设计稿、阶段性方案、bridge 时代笔记已归档到 `docs/archive/2026-11-22/`，仅作历史追溯。
>
> | 项 | 值 |
> |---|---|
> | 适用源码 | `<安装目录>\_setup\src\Aogara-3`（Rust 2021 + Tauri 2 + React 19） |
> | 适用程序 | `<安装目录>\app\aogara.exe`（原生整合号池，已弃用外部 bridge） |
> | 文档日期 | 2026-11-23（初版 2026-11-22） |
> | 事实优先级 | **源码 > 真机日志 > 本文档 > 历史归档文档** |
> | 配套操作手册 | `<安装目录>\使用手册.md`（部署/日常操作向） |
> | 文档索引 | `<安装目录>\docs\README.md` |

---

## 目录

1. [项目概览](#1-项目概览)
2. [技术栈](#2-技术栈)
3. [目录与运行形态](#3-目录与运行形态)
4. [总体架构与数据流](#4-总体架构与数据流)
5. [启动装配流程](#5-启动装配流程)
6. [配置系统](#6-配置系统)
7. [抓包层 capture](#7-抓包层-capture)
8. [协议桥层 bridge](#8-协议桥层-bridge)
9. [牌局状态 game_state](#9-牌局状态-game_state)
10. [分析引擎 analysis](#10-分析引擎-analysis)
11. [Bot 层 bot](#11-bot-层-bot)
12. [自动打牌 autoplay](#12-自动打牌-autoplay)
13. [号池 pool（原生整合重点）](#13-号池-pool原生整合重点)
14. [IPC 与前端](#14-ipc-与前端)
15. [历史、日志与 Inspector](#15-历史日志与-inspector)
16. [代理、更新与镜像](#16-代理更新与镜像)
17. [构建与发布](#17-构建与发布)
18. [运行手册与重启接手](#18-运行手册与重启接手)
19. [已知限制与风险](#19-已知限制与风险)
20. [本 fork 变更记录](#20-本-fork-变更记录)
21. [文档索引与归档策略](#21-文档索引与归档策略)

---

## 1. 项目概览

### 1.1 Aogara 是什么

Aogara 是麻将平台的**实时对局分析 + 自动打牌**工具。它从游戏客户端截取 WebSocket 通信，翻译成统一的事件协议（mjai），维护一份实时牌局镜像，然后：

- 计算向听数、进张/听牌、和了率、放铳危险度；
- 给出推荐切牌，并通过 HUD 浮窗叠加在游戏画面上；
- 可调用内置/云端/外部 AI Bot，把决策落成真实点击，实现自动打牌；
- 记录历史战绩、协议级时间线，供复盘和排错。

支持平台（官方口径）：

| 平台 | 四麻 | 三麻 | AutoPlay |
|---|:---:|:---:|:---:|
| 雀魂 Mahjong Soul | ✓ | ✓ | ✓ |
| 天凤 Tenhou | ✓ | ✓ | ✓ |
| Riichi City（麻雀一番街） | ✓ | ✓ | ✗ |
| 天月 Amatsuki | 计划中 | 计划中 | ✗ |

### 1.2 本机形态：原生整合号池的 fork

官方 exe 只带分析/HUD/单账号自动打牌。本机版本在官方 v3.8.0 源码上做了 fork，把原先外部 Node 进程 `tools/bridge.js` 的**号池能力**全部搬进 exe：

- 多账号配置、登录态探测、登出/换号；
- 从大厅走位到指定房间/模式并开始匹配；
- 自动连打（一局结束自动下一局）；
- 定时停止（到点等本局结束再收尾）；
- 重启后复用已开的游戏浏览器，并对进行中对局做接手。

**最终形态：只启动 `aogara.exe`，不再启动 bridge，不再向 WebView 注入面板。**

### 1.3 与官方原版的功能差异

| 能力 | 官方原版 | 本机 fork |
|---|---|---|
| 号池/多账号 | 无 | 原生 `[pool]` 配置 + `/pool` 页面 |
| 换号 | 手动在网页登出登录 | CDP 驱动 Unity 画布自动登出/登录 |
| 进房 | 手动点 | `pool_start` 自动走位（段位场→房间→模式） |
| 连打/定时 | 无 | `spawn_pool_driver` 常驻状态机 |
| 重启行为 | 启动时 reclaim profile、重启浏览器 | 先探测已有 CDP 端点，能复用就复用 |
| 进行中对局接手 | 不支持 | 牌谱离线恢复 + 条件 reload 重握手 |
| 外部 bridge | 不需要 | **已弃用**（43110 停用） |

---

## 2. 技术栈

### 2.1 后端

| 组件 | 版本/说明 | 用途 |
|---|---|---|
| Rust | edition 2021，MSVC target | 全部核心逻辑 |
| Tauri | 2.x（`custom-protocol` feature） | 桌面壳、WebView、IPC |
| tokio | 1.x（multi-thread） | 异步运行时、后台任务 |
| chromiumoxide | 0.x | CDP 客户端（页面、点击、求值） |
| hudsucker | 0.24 | MITM 代理后端 |
| prost / prost-reflect | 0.14 / 0.16 | 雀魂 protobuf 编解码 |
| riichienv-core | 0.4.8 | 麻将规则/状态引擎 |
| native_bot | 本地 path 依赖 | 内置 AI 推理 |
| tracing / tracing-appender | — | 结构化日志、分文件输出 |
| toml / toml_edit | 1.1 / 0.25 | 配置读取与**保格式**写回 |
| reqwest / rustls | 0.13 | 更新、云端 Bot、镜像下载 |
| zip / flate2 | — | Bot 安装包、日志打包、review 上传 |

### 2.2 前端

React 19 + TypeScript + Vite，UI 为 Tailwind + shadcn 风格组件，状态用 Zustand，路由为 react-router（hash 路由），i18n 为 i18next（中/繁/英/日）。

### 2.3 构建环境

| 项 | 位置/版本 |
|---|---|
| Rust | `<安装目录>BuildTools\rustup` / `cargo`，含 `x86_64-pc-windows-msvc` target |
| VS BuildTools 2022 | `<安装目录>BuildTools\VSBuildTools`（MSVC 14.44 + Win10/11 SDK 10.0.26100） |
| Node | `F:\nodejs\node.exe`（v26）+ npm |
| protoc | `<安装目录>BuildTools\msys2\mingw64\bin\protoc.exe` |
| windres | 同目录（tauri-winres 依赖） |
| crates 镜像 | USTC sparse |

> 构建必须用 **MSVC target**。Windows GNU target 在 Tauri 2 下不受官方支持，且 `embed-resource` 会失败。

---

## 3. 目录与运行形态

### 3.1 源码目录（`_setup/src/Aogara-3`）

```
src/
├─ main.rs                 # 入口，仅调用 aogara::run()
├─ lib.rs                  # 启动装配：配置/日志/总线/Tauri/后台任务
├─ cli.rs                  # --config 参数
├─ platform.rs             # 平台相关初始化
├─ event_bus.rs            # 8 条 broadcast 总线定义
├─ capture/                # 抓包传输层
│  ├─ mod.rs               # CaptureBackend trait / CaptureCtx / ShutdownToken
│  ├─ flow.rs              # FlowBridges：每条 WS 一个 Bridge
│  ├─ chromium/            # 受控浏览器 + CDP
│  │  ├─ mod.rs            # 生命周期：复用/重拉、handoff、shutdown
│  │  ├─ cdp.rs            # CDP 连接、订阅 WS 帧、条件 reload
│  │  ├─ launch.rs         # 启动、端点探测、DevTools 探活
│  │  ├─ profile.rs        # profile 目录、reclaim_singleton
│  │  ├─ detect.rs / cft.rs# 系统浏览器探测 / Chrome-for-Testing
│  ├─ hudsucker_backend.rs # MITM 代理后端
│  ├─ http.rs              # HTTP 流量记录
│  └─ telemetry.rs         # 阿里云 SLS 信标拦截
├─ proxy/                  # MITM：CA、证书存储、上游转发、雀魂证书重写
├─ bridge/                 # 协议 ↔ mjai
│  ├─ mod.rs               # Bridge trait / Direction / ParseResult / BridgeHooks
│  ├─ majsoul/             # 雀魂桥（最大单文件）
│  ├─ tenhou/              # 天凤桥
│  └─ riichi_city/         # 一番街桥
├─ game_state/             # 牌局镜像
│  ├─ tracker.rs           # GameTracker + drive_loop
│  ├─ snapshot.rs          # GameStateSnapshot
│  ├─ restore.rs           # 启动时牌谱离线恢复（fork 新增）
│  ├─ score.rs / convert.rs / mahgen_view.rs
├─ analysis/               # 向听/进张/和了率/危险度
│  ├─ runner.rs            # post_tracker 订阅者
│  ├─ shanten.rs / waits.rs / agari_rate.rs / tenpai_rate.rs / risk/
├─ bot/                    # Bot 管理
│  ├─ manager.rs           # 决策点路由
│  ├─ native.rs / api.rs / runner.rs
│  ├─ install.rs / registry.rs / manifest.rs / purchase.rs
├─ autoplay/               # 决策 → UI 动作
│  ├─ manager.rs           # 生命周期与热开关
│  ├─ platform.rs          # Step 枚举
│  ├─ cdp_input.rs         # CDP 四段点击 / DOM 点击（键盘在 pool，需复用先迁入）
│  ├─ majsoul/coords.rs    # 16×9 逻辑网格
│  ├─ tenhou/ / riichi_city/
│  ├─ verify.rs / delay/
├─ pool/                   # 号池（fork 新增）
│  ├─ mod.rs               # CDP 连接、登录态、换号、进房、结算
│  └─ session.rs           # 日志态、坐标继承、房间/模式归一
├─ config/                 # AppConfig 各段
│  └─ pool.rs              # [pool] 段
├─ ipc/
│  ├─ commands.rs          # 71 个 #[tauri::command]
│  ├─ state.rs             # AppState
│  ├─ mod.rs               # 总线 → 前端事件 forwarder
│  ├─ capture_supervisor.rs# 抓包启停/重启
│  └─ overlay.rs           # HUD 浮窗
├─ history/                # 战绩存储与聚合
├─ logger/                 # 会话日志、分文件、打包
├─ inspector/              # 协议级时间线
├─ schema/                 # IPC/日志/mjai 数据结构
├─ updater/                # 自更新（SHA-256 + minisign）
└─ github/                 # 镜像、签名校验
frontend/src/
├─ main.tsx                # 路由表（含 /pool）
├─ components/sidebar/     # 侧栏菜单
├─ lib/menuList.ts         # 菜单项（含号池 Layers）
├─ routes/                 # Overview/Game/Bots/History/Review/Logs/Pool/Settings
├─ stores/                 # Zustand：config/game/bot/logs/...
├─ tiles/                  # 对局 HUD 卡片
└─ types.ts                # 前端类型（镜像 Rust 结构）
```

### 3.2 运行时目录（便携式）

```
<安装目录>\
├─ app\
│  ├─ aogara.exe                  # 主程序（约 46 MB）
│  ├─ aogara.exe.pre-native.bak   # 官方原版备份（回滚用）
│  ├─ configs\config.toml        # 所有配置（含 [pool]）
│  ├─ ca\                        # MITM 根证书（勿外传）
│  ├─ logs\slot1..slotN\          # 多会话日志（slot1 为主，slot2..N 无 all.log）
│  ├─ history\                   # 战绩索引与单局 mjai
│  ├─ mjai_bot\                  # 外部 bot（可选）
│  └─ runtime\                   # 自带 Python + uv
├─ profiles\main\                # 游戏浏览器 user-data-dir（登录态）
├─ tools\                        # bridge 时代脚本（已归档/停用）
└─ docs\                         # 文档（本文件为当前主文档）
```

**便携含义**：配置、证书、日志、历史都在 exe 同级；备份=复制目录，卸载=删目录。

---

## 4. 总体架构与数据流

### 4.1 分层

| 层 | 位置 | 职责 |
|---|---|---|
| 入口/装配 | `main.rs` → `lib.rs::run()` | 配置、日志、总线、Tauri、后台任务 |
| 抓包 | `capture/` | 把游戏 WebSocket 帧送进桥 |
| 桥 | `bridge/` | 平台协议 ↔ mjai 事件 |
| 牌局状态 | `game_state/` | `riichienv-core` 镜像局面 + 快照 |
| 分析 | `analysis/` | 向听、进张、和了率、危险度 |
| Bot | `bot/` | 本地/云端/外部 mjai 子进程 |
| 自动打牌 | `autoplay/` | 决策 → 平台动作 |
| 号池 | `pool/` | 换号、进房、连打、定时 |
| IPC | `ipc/` | 总线 → 前端事件；前端 → 命令 |
| 记录 | `history/`、`logger/`、`inspector/` | 战绩、日志、帧时间线 |
| 辅助 | `proxy/`、`updater/`、`github/` | MITM、自更新、镜像安装 |

### 4.2 八条事件总线

`lib.rs` 在 Tauri 启动前创建，`AppState` 持有发送端：

| 总线 | 类型 | 生产者 | 主要消费者 |
|---|---|---|---|
| `mjai_bus` | `broadcast<MjaiEvent>` | 桥（每条 WS） | GameTracker、HistoryRecorder、Inspector 镜像 |
| `post_tracker_bus` | `broadcast<TrackedEvent>` | GameTracker | BotManager、AnalysisRunner |
| `bot_response_bus` | `broadcast<BotResponse>` | BotManager | AutoplayManager、前端 HUD |
| `bot_status_bus` | `broadcast<BotStatus>` | BotManager | 前端状态栏（并镜像进 AppState） |
| `capture_status_bus` | `broadcast<CaptureStatus>` | 抓包 supervisor | 前端（并镜像进 AppState） |
| `notify_bus` | `broadcast<Notification>` | 各模块 | 前端 toast |
| `analysis_bus` | `broadcast<AnalysisResult>` | AnalysisRunner | 前端 HUD（并写 analysis_cache） |
| `history_bus` | `broadcast<HistoryEvent>` | HistoryRecorder | 前端历史页 |

`TrackedEvent` 是「已被 tracker 应用的事件 + 当时算出的 `can_act`」；Bot 只订阅它，保证决策基于已生效的局面。

### 4.3 主数据流

```
游戏客户端 WebSocket
      │  (CDP Network.webSocketFrame* / MITM 解密)
      ▼
capture::FlowBridges  ── 每条 WS 懒创建一个 Bridge
      ▼
bridge::parse(direction, payload) → Vec<MjaiEvent> + ParsedFrame
      ▼
mjai_bus ──┬──► GameTracker::handle() ──► post_tracker_bus(TrackedEvent)
           │                                      ├──► BotManager（can_act 才决策）
           │                                      └──► AnalysisRunner（每事件重算）
           │
           ├──► HistoryRecorder（EndGame 定稿一局）
           └──► Inspector（mjai.jsonl 时间线）

BotResponse ──► AutoplayManager::plan() ──► Vec<Step> ──► 平台执行
                                                   ├─ 雀魂：CDP 点击画布
                                                   ├─ 天凤：DOM/注入
                                                   └─ 一番街：MITM 注入帧

analysis_bus / bot_response_bus / notify_bus ──► Tauri emit ──► React HUD
```

### 4.4 三条设计铁律

1. **新功能挂在总线或 IPC 上，不要去改抓包层。**
2. **`GameTracker` 是座位、手牌、合法动作的唯一权威**；UI、分析、Bot 都从它取数。
3. **结论必须有实测支撑**；接口 `code 0` / 函数返回 `Ok` 不等于界面上真的生效。

---

## 5. 启动装配流程

`src/lib.rs::run()` 的实际顺序：

1. `platform::setup()`。
2. `cli::Cli::parse()`，读取 `--config`。
3. `config::load_config()`：用户配置目录 → exe 同级 `configs/config.toml` → 当前目录，逐级查找。
4. `logger::init()`：在 `<log_dir>/<YYYYMMDD-HHMMSS>/` 建会话；失败时回退到用户配置目录（只读安装场景）。
5. 创建 8 条总线（见 4.2）。
6. 创建 `HistoryStore`（`<config_root>/history/`）；失败即致命退出。
7. 创建 `game_tracker` 句柄（`Arc<Mutex<GameTracker>>`）。
8. **启动恢复**：`game_state::restore::try_restore(log_dir, &mut tracker)`
   - 扫描所有历史会话目录下的 `majsoul/*.mjai.jsonl`；
   - 找最近修改、未以 `end_game` 结尾、5 分钟内仍在写的牌谱；
   - 逐行 `tracker.handle()` 重放，**不发总线**，避免历史事件触发误出牌；
   - 若恢复后正好轮到我方，暂存最后一个事件，等 Bot/Autoplay 起来后（1.5 秒）补一个 `TrackedEvent{can_act:true}` 唤醒决策。
9. 创建 `analysis_cache`。
10. `tauri::Builder`：注册 dialog / window-state 插件，`invoke_handler(ipc_handlers!())`。
11. `.setup()` 内：
    1. 定位 Python/uv runtime（外部 bot 用，缺失只告警）；
    2. 构造 `AppState`；
    3. `ipc::install()`：`app.manage(state)` + 启动总线 forwarder；
    4. `ipc::overlay::reconcile()`：按配置恢复 HUD；
    5. 主窗关闭时同步关闭 overlay（否则无边框浮窗会吊住进程）；
    6. spawn `game_state::tracker::drive_loop`；
    7. spawn `analysis::runner::drive_loop`；
    8. spawn `history::recorder::drive_loop`；
    9. `bot.enabled` 为真 → spawn `bot::run_bot_manager`；
    10. `autoplay.enabled` 为真 → spawn `autoplay::run_autoplay_manager`；
    11. 恢复点唤醒任务（1.5 秒后检查 `our_seat_can_act()`，仍为真才补事件）；
    12. spawn Inspector 的 mjai 镜像任务；
    13. `proxy.enabled` 为真 → spawn 抓包 supervisor；
    14. **`ipc::commands::spawn_pool_driver(state)`：号池常驻驱动（fork 新增）**；
    15. 注册 Ctrl+C 优雅停机。
12. `tauri::run()`。

> 注意：`lib::run()` 是同步函数，所有需要 tokio 的任务都在 `.setup()` 里用 `tauri::async_runtime::spawn` 启动。

---

## 6. 配置系统

### 6.1 AppConfig 结构

`src/config/mod.rs`：

```rust
pub struct AppConfig {
    pub general: GeneralConfig,
    pub logging: LoggingConfig,
    pub platform: PlatformConfig,
    pub proxy: ProxyConfig,
    pub bot: BotConfig,
    pub capture: CaptureConfig,
    pub autoplay: AutoplayConfig,
    pub overlay: OverlayConfig,
    pub network: NetworkConfig,
    pub pool: PoolConfig,        // fork 新增
}
```

所有段均 `#[serde(default)]`，缺字段自动补默认值，旧配置可直接升级。

### 6.2 读取、写回与热更新

- **读取**：`config::load_config` 按 用户配置目录 → exe 同级 → cwd 查找；`--config` 可显式指定。
- **写回**：`ipc::commands::persist_config` 使用 `toml_edit` **只改已知键，保留未知段/注释**。这是硬性回归要求：`config.toml` 可能被其他 build 或手写内容共享，整份序列化会删掉不认识的键。
- **热更新**：前端 `update_config(newConfig)` → 校验 → 落盘 → 替换 `Arc<RwLock<AppConfig>>` → 按需触发：
  - `bot.enabled` false→true：spawn BotManager（每进程一次）；
  - `autoplay.enabled` false→true：spawn AutoplayManager（每进程一次）；
  - 抓包相关字段变化：自动 `restart_capture`；
  - 平台切换：更新历史记录的 platform tag。
- `autoplay.enabled` 的管理器虽然只 spawn 一次，但 **manager 每轮都重读配置**，所以关掉立即变 no-op，再开不需重启。

### 6.3 `[pool]` 段

`src/config/pool.rs`：

```toml
[pool]
selected_id = "alt3"      # 当前选中账号 id
auto_next = false          # 一局结束自动下一局（运行时以 auto_next_global 为准，仅此处持久化）
timer_enabled = false      # 定时停止
timer_hours = 4.0
start_capture_on_launch = false  # 兼容字段；生效位置在 [multi]

[pool.target]
room = "copper"            # copper/silver/gold/jade/throne
mode = "mode4e"            # mode4e/4s/3e/3s

[[pool.accounts]]
id = "main"
label = "主号"
uid = "..."                # 数字 UID，仅展示/人工核对
email = "..."              # 登录邮箱（明文）
password = "..."           # 登录密码（明文，见安全说明）
login_name = "FKU!!!"      # 可选：用户名注册的账号，localStorage.account 存的是它而非邮箱
[pool.accounts.coords]
loginEmail = [0.5, 0.6]    # 0..1 归一化，相对 canvas
loginPassword = [0.5, 0.7]
loginSubmit = [0.5, 0.8]
settings = [0.97, 0.06]
logout = [0.5, 0.75]
logoutConfirm = [0.6, 0.62]
ranked = [0.5, 0.45]
confirm = [0.5, 0.8]
cancel = [0.5, 0.8]
[pool.accounts.coords.rooms]
"铜之间" = [0.3, 0.5]
[pool.accounts.coords.modes]
mode4e = [0.5, 0.6]

# 多开会话（第二阶段）
[multi]
max_sessions = 8                   # 上限（1..8）
start_on_launch = true             # 已不控制启动，仅兼容保留
start_capture_on_launch = false    # 启动 Aogara 时是否打开各会话浏览器（进程级单开关）

[[multi.sessions]]                 # 静态会话条目（重启自动装配）
slot = "slot2"
config = "<安装目录>/app/configs/slot-main.toml"
enabled = true

# UI 添加的会话登记在 configs/sessions.dynamic.json（重启恢复）；
# 从多开页移除的 slot 写入 configs/sessions.disabled.json（墓碑，防止静态条目复活）。
```

字段说明：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | String | 稳定主键，前端按它选择 |
| `label` | String | 显示昵称 |
| `uid` | String | 数字 UID，**不参与自动校验**（见 13.10 限制） |
| `email` / `password` | String | 明文；换号时逐字符输入 Unity 画布 |
| `coords` | map | 画布归一化坐标；空表则继承 `main` |
| `target.room/mode` | String | 自动进房目标 |
| `auto_next` | bool | 连打开关 |
| `timer_enabled/hours` | bool/f64 | 定时停止 |

坐标键同时接受 camelCase（`loginEmail`）与 snake_case（`login_email`），由 `session::canon_key` 归一。

### 6.4 安全说明

- `[pool]` 的密码是**明文**（按用户选择，较原 DPAPI 是安全降级）。`config.toml` 不要外传、不要放同步盘。
- `ca/aogara-ca.key` 是 MITM 根证书私钥，泄露等于可解密本机 HTTPS。
- `logs/<session>/sensitive/` 含完整原始帧（登录 token 等）。分享日志必须走「打包日志」，它会剔除该目录。

---

## 7. 抓包层 capture

### 7.1 统一接口

```rust
#[async_trait]
pub trait CaptureBackend: Send {
    async fn run(self: Box<Self>, ctx: CaptureCtx, shutdown: ShutdownToken) -> Result<()>;
    fn descriptor(&self) -> CaptureDescriptor;
}
```

`CaptureCtx` 携带：`Session`、`Platform`、`mjai_bus`、`notify_bus`、可选的 `AutoplayContext`（浏览器页句柄槽）、HTTP 记录配置。

`ShutdownToken` 内部是 `Arc<Notify>`，后端把它当协作取消点。

### 7.2 MITM 后端

`hudsucker_backend.rs` + `proxy/`：

- 在本机 `127.0.0.1:23410` 起中间人代理；
- 自签 CA，需要用户把 `ca/aogara-ca.crt` 装进「受信任的根证书颁发机构」；
- 游戏流量导流到代理后解密 WebSocket；
- 必须排除 loopback，否则代理会劫持自己，日志出现 `refusing CONNECT to loopback`；
- 一番街只有这条路径（无网页客户端）。

### 7.3 Chromium 后端

`capture/chromium/`：

```
resolve_executable()      显式路径 → 系统浏览器探测 → Chrome-for-Testing → 报错
resolve_profile_dir()     必须绝对路径
mkdir -p profile
probe_existing_endpoint() 先看有没有活着的 CDP 端点（fork 新增）
  ├─ 有 → 复用，child = None
  └─ 无 → reclaim_singleton() 杀旧实例 → launch::spawn() → wait_for_devtools_endpoint()
cdp::run(endpoint, ..., reload_on_attach)
```

关键事实：

1. **profile 是登录态单位，不是进程单位。** 一个 `user_data_dir` 一套 cookies/IndexedDB。
2. `reclaim_singleton` 在 Windows 上按**命令行里的 `--user-data-dir` 字符串**匹配进程（忽略大小写、`/`≡`\`、容忍一个尾分隔符，**不做 realpath**）。
3. **launcher handoff**：Edge/Chrome 可能用同样参数重启自己、原进程以 0 退出。子进程干净退出时会先探测 CDP 端口；还活着就打 `launcher handoff; continuing` 并继续。
4. **退出时故意不关复用的浏览器**：用户可能正在打，浏览器留着游戏还连着；下次启动再判断复用或回收。
5. `extra_args` 固定 `--remote-debugging-port=63503`，号池靠这个端口找游戏页。

### 7.4 复用浏览器与条件 reload（fork 核心改动）

`mod.rs`：

```rust
let existing = launch::probe_existing_endpoint(&profile_dir, &cfg.extra_args).await;
let reused = existing.is_some();
let reload_on_attach = reused && find_latest_unfinished(&logs_dir).is_some();
```

- `probe_existing_endpoint` 先读 `<profile>/DevToolsActivePort`，再试 `extra_args` 里配置的端口（Edge 不总写该文件），最后用 HTTP `/json/version` 探活。
- **有新鲜未结束牌谱 = 对局中** → attach 后 reload 游戏页一次，让客户端重连、重发 `authGame` + `syncGame`，从而拿到座位和 `game_restore` 全量牌局。
- **没有对局** → 直接复用，打印 `reusing idle browser (no live game) — skipping reload`，不闪页面。
- 只对 `maj-soul.com` / `mahjongsoul.com` 页面 reload，拿不到 URL 就跳过。
- reload 后约 **17 秒重连空窗**：`syncGame` 可能短暂返回空 actions、`inGame=false`、`inputOperation=0`，属正常，不要在此窗口判定失败。

> 机制边界：CDP 只能收到订阅之后的新 WS 帧，无法回溯已发生的 `authGame`；页面 JS 也不暴露 WS 引用。因此「中途接手」必须让客户端重连一次，reload 是最可靠的手段。见第 19 节。

### 7.5 帧路由

`capture/flow.rs::FlowBridges<K>`：

- 按 flow key（CDP 的 target/session 或 MITM 的连接）懒创建 `Bridge`；
- 引用计数归零销毁；
- 每条 WebSocket 独立桥实例，因为协议解析的请求/响应关联是「每连接」的。

---

## 8. 协议桥层 bridge

### 8.1 Bridge trait

```rust
pub trait Bridge: Send {
    fn parse(&mut self, direction: Direction, content: &[u8]) -> ParseResult;
    fn build(&mut self, command: &MjaiEvent) -> Option<Vec<u8>>;
}
```

- `Direction::Up`：客户端 → 服务端（请求）。
- `Direction::Down`：服务端 → 客户端（响应/通知）。
- `ParseResult { events, parsed, shareable }`：
  - `events`：翻译出的 mjai 事件（0..n）；
  - `parsed`：给 Inspector 看的结构化帧（雀魂为 method+payload，天凤为 tag 字典）；
  - `shareable`：只有白名单帧可进可分享日志，其余完整帧写 `sensitive/`。

`BridgeHooks` 把平台专用槽位打包传给桥：雀魂的决策时间预算、`inputOperation` 计数器；天凤的手牌/窗口状态；一番街的注入闸门。

### 8.2 雀魂桥（`bridge/majsoul/`）

覆盖的方法（核心）：

| 方法 | 作用 |
|---|---|
| `.lq.Lobby.login` / `heatbeat` | 登录、会话维持 |
| `.lq.FastTest.authGame` | 开局握手，**座位只在这里出现** |
| `.lq.FastTest.enterGame` | 进桌 |
| `.lq.FastTest.syncGame` | 断线重同步，`game_restore.actions[]` 含庄家/积分/手牌/牌河 |
| `.lq.ActionPrototype` | 动作原型（打/吃碰杠/立直/和了） |
| `.lq.FastTest.inputOperation` | 客户端自己点的动作，**点击是否生效的硬证据** |
| `.lq.FastTest.inputChiPengGang` | 吃碰杠输入 |
| `.lq.NotifyGameEndResult` / `NotifyGameTerminate` | 结算 / 终止 |

其他要点：

- 三麻/四麻在桥内按 `num_players` / `mode_id` 分流，拔北、换牌河、罚符都在这一层翻译。
- `stable_game_id`：断线重连带稳定桌号，历史记录据此「替换而非重复插入」。
- protobuf 解码用 `prost-reflect`，方法名 + payload 一起进 `parsed`。

### 8.3 天凤 / 一番街

- 天凤：`bridge/tenhou/`，维护 journal/state/rebuild；**只观察**，中途重连只给快照，拿不到完整手牌时会暂停分析到下一局，且不入历史。
- 一番街：`bridge/riichi_city/`，仅 MITM；自动打牌通过代理注入协议帧（`autoplay/inject.rs`），没有可点的页面。

---

## 9. 牌局状态 game_state

### 9.1 GameTracker

```rust
pub type GameTrackerHandle = Arc<Mutex<GameTracker>>;
```

- `drive_loop(tracker, mjai_rx, post_tx)`：订阅 `mjai_bus`，对每个事件调用 `tracker.handle()`；
- 首个 `StartGame` 建新局（`riichienv-core` 默认东 1），后续走 `apply_mjai_event`；
- 应用成功后把 `TrackedEvent { event, can_act }` 发到 `post_tracker_bus`；
- `can_act` 是引擎算出的「当前是否轮到我方行动」，是 Bot 与 autoplay 的唯一权威判据。

### 9.2 GameStateSnapshot

暴露给前端/分析/Bot 的字段包括：

- 每家手牌、副露、牌河、分数；
- 立直状态、宝牌/里宝、自风/场风、本场、供托；
- 当前摸到的牌、剩余牌山、合法动作。

`score.rs` 自带 3p/4p 和了点计算（`evaluate_hora_3p/4p`），不依赖外部库。

### 9.3 启动恢复 restore.rs（fork 新增）

目的：Aogara 重启后，不依赖页面 reload 也能先把「这是哪一局、我是谁、手牌是什么」恢复出来。

算法：

1. `find_latest_unfinished(logs_dir)`：
   - 遍历 `<logs>/<session>/majsoul/*.jsonl`；
   - 首行必须是 `start_game`；
   - 末行不能是 `end_game`；
   - mtime 必须在 `RESTORE_MAX_AGE = 5 分钟`内（进行中的局每几秒写一行，陈旧半局会被排除）。
2. `restore_into(path, tracker)`：
   - 逐行反序列化 `MjaiEvent`，坏行跳过并告警；
   - 直接 `tracker.handle(&ev)`，**绝不发 `mjai_bus`**；
   - 记录 `seat`、`num_players`、最后一个事件。
3. `lib.rs` 在 Tauri 起来前调用；若恢复后 `our_seat_can_act() == Some(true)`，暂存最后事件；setup 完成 1.5 秒后如果仍轮到我方，向 `post_tracker_bus` 补发一条 `TrackedEvent{can_act:true}`，让 Bot 重新评估。

意义：即使 CDP 还没接上、或接手还在 17 秒空窗里，前端 HUD 和 Bot 也先有一份可用局面。它是兜底，不替代实时帧。

### 9.4 双轨状态源

| 场景 | 用哪个 |
|---|---|
| 换号前判断是否在对局 | `AppState.game_tracker` 的 `state()/state_3p()`（内存权威） |
| `pool_start` / driver 判断是否在对局 | `session_state()` 读 `all.log`（跨重启仍有效） |
| Bot 出牌 | `GameTracker` 的 `can_act` + snapshot |
| 重启后判断是否有局可接 | `find_latest_unfinished()` 的新鲜牌谱 |

三个来源各有边界，不要混用。

---

## 10. 分析引擎 analysis

- 纯 Rust 移植自 mahjong-helper 的算法与数值表（许可证说明见 `analysis/mod.rs`）。
- `runner::drive_loop` 订阅 `post_tracker_bus`，**每个事件重算一次**：
  - 13 张向听/进张约 200µs；
  - 14 张弃牌搜索约 1ms（release）。
- 输出 `AnalysisResult`：向听、进张/听牌、和了率、听牌率、点数期望、危险度（`risk/base.rs` + `risk/wall.rs`）。
- 结果写 `analysis_bus` 与 `analysis_cache`；前端 `get_analysis` 可取一次性快照。
- **不要在 UI 里重算**，前端只负责画。

---

## 11. Bot 层 bot

### 11.1 三种来源

| 来源 | 说明 | 关键点 |
|---|---|---|
| `aogara-native` / `aogara-native3p` | 权重内嵌 exe，纯 Rust 推理 | 行为克隆，棋力中等是有意的默认值 |
| 云端推理 | 默认 `https://mjapi.shinkuan.me`，需密钥 | 单步超时默认 3s，超时/失败**自动回退本地** |
| 外部 mjai bot | `mjai_bot/<name>/` 子进程 + JSONL | 进程隔离是 AGPL 边界 |

### 11.2 决策路由

`bot/manager.rs`：

- 只订阅 `post_tracker_bus`，只在**决策点**发请求；
- `is_decision_point(event, can_act)`：`can_act == Some(false)` 时一律不动；`can_act == None` 时按事件形态（自摸、打牌、鸣牌窗口等）判断；
- `bot.active_4p` / `active_3p` 独立，开局按人数路由；
- 每次决策重新读取 active bot，切换 bot 不需重启（`BotManager` 内部按名字懒加载 runner）。

### 11.3 运行时

- 外部 bot 需要 Python + uv：`PythonRuntime::locate(resource_dir)` 先找打包 runtime，再找 PATH。
- `uv sync` 由 `sync_guard` 串行化，避免同一 venv 并发写。
- 内置 bot 不依赖 Python；缺 runtime 只影响外部 bot。

---

## 12. 自动打牌 autoplay

### 12.1 数据流

```
BotResponse + GameStateSnapshot + legal_actions
        ▼
PlatformAutoplay::plan() → Vec<Step>
        ▼
执行器
```

`Step`（`platform.rs`）：

| Step | 含义 |
|---|---|
| `Sleep(ms)` | 思考延迟 |
| `AwaitReady` | 等可操作窗口 |
| `DomClick { selectors, label }` | DOM 点击（天凤按钮、设置页） |
| `Discard { tile_index }` | 打某张牌 |
| `Click { x_norm, y_norm }` | 画布归一化点击（雀魂） |

### 12.2 雀魂：画布点击

- `majsoul/coords.rs` 是**手工标定的 16×9 逻辑网格**（手牌 14 槽、按钮 3×3、鸣牌候选 11 槽），注释明确 "years of production validation — DO NOT change"。
- 换算：`px = rect.x + (x16 / 16) * rect.width`，`py = rect.y + (y16 / 9) * rect.height`；摸牌位还有 `TSUMO_SPACE = 0.2469` 的偏移。
- `cdp_input.rs::dispatch_click` 发**四段事件**：`moved → hover 150~300ms → pressed → hold 60~140ms → released`。因为 Laya 输入系统在 `mousedown` 时才采样 hover，move/press 同帧会被丢弃。
- `verify.rs`：点击前对 `inputOperation` 计数取票，点后看是否增长；没涨就 jiggle/加长按住重试，再不行 reload 兜底。
- `CanvasRect` 有 30 秒 TTL（Aogara 内部缓存）。**号池层不要学它缓存几何**，每次点击现取。

### 12.3 天凤 / 一番街

- 天凤：按钮是真 DOM（`button.s7[name="c22-N"]`）→ `element.click()`；打牌走客户端自己的 discard 处理函数（注入脚本）。
- 一番街：没有可点页面 → 计划一个协议帧，交给 MITM 代理注入。

### 12.4 热开关

- `autoplay.enabled` 只在启动时决定是否 spawn manager；之后 manager 每轮重读配置。
- `pool_set_autoplay` / `update_config` 把 false→true 翻转时，通过 `claim_autoplay_manager_spawn` 只 spawn 一次。
- 关闭立即 no-op，重新打开不需重启。

### 12.5 出牌延迟

`autoplay/delay/`：`legacy` 固定区间或 `script` Lua 模型（对数正态）；`delay.lua` 可改，升级时未改动的旧文件会被刷新为默认，改过的保留并生成 `.bak`。

---

## 13. 号池 pool（原生整合重点）

### 13.1 目标与边界

**目标**：只启动 exe，就能完成「选账号 → 换号 → 进指定房间/模式 → 自动打 → 一局结束自动下一局 → 到点收尾」。

**明确不做**：

- 不做截图模板匹配/像素采样来判断界面（用户硬性要求）。判据只用 `localStorage` + 日志 + `GameTracker`。
- 不调用任何付费接口（电池、打赏、上船等）。
- 不额外启动 Node 进程、不额外开浏览器、不注入面板。

### 13.2 文件与职责

| 文件 | 职责 |
|---|---|
| `src/pool/mod.rs` | CDP 短连接、登录态探测、Unity 键鼠输入、换号状态机、进房、结算 |
| `src/pool/session.rs` | 日志态 `session_state()`、坐标继承、房间/模式归一、`latest_session_dir` |
| `src/config/pool.rs` | `PoolConfig` / `PoolAccount` / `PoolCoord` / `PoolTarget` |
| `src/ipc/commands.rs` | `pool_probe_login` / `pool_switch_account` / `pool_status` / `pool_set_autoplay` / `pool_start` / `spawn_pool_driver` |
| `frontend/src/routes/Pool.tsx` | 账号增删选、目标场次、连打/定时开关、开始对局 |
| `frontend/src/lib/menuList.ts` | 侧栏「号池」入口（Layers 图标） |
| `frontend/src/main.tsx` | `/pool` 路由 |

### 13.3 CDP 短连接 `GameCdp`

```rust
pub struct GameCdp { pub browser: Browser, pub page: Page, pump: JoinHandle<()> }
```

- `connect(port)`：`Browser::connect("http://127.0.0.1:<port>")`；
- 必须持续 poll handler，否则 oneshot 响应永远不返回；`pump` 在 `Drop` 时 abort；
- attach 模式下 `pages()` 可能前几百毫秒为空：12 次 × 250ms 重试；
- 页面选择：URL 含 `maj-soul.com` / `mahjongsoul.com` 优先，否则取第一个页面；
- 每次命令独立建连，**不依赖 autoplay 的页句柄**（那个只在游戏 WS 出现后才绑定）。

端口来源：`game_cdp_port(&cfg.capture.chromium.extra_args)`（一行转发到公共函数 `util::parse_fixed_cdp_port`，与 `launch.rs::parse_fixed_debug_port`、`lib.rs` 启动日志共用同一解析逻辑），支持 `--remote-debugging-port=63503` 和分离参数两种写法；没有配置就直接报错，提示去改 config。

### 13.4 登录态探测 `probe_login`

读页面 `localStorage`：

```js
({ account: localStorage.getItem('account'),
   token: !!localStorage.getItem('access_token') })
```

判定：

- `account` 为空或等于 `"FKU!!!"` → 未登录；
- 否则登录，`account` 统一转小写（雀魂存的是邮箱）；
- `access_token` 只作为辅助提示，**不能单独证明已登录**（登出后可能残留）。

### 13.5 换号状态机 `switch_account`

```
probe_login
  ├─ 已是目标邮箱 → 返回 switched=false "无需切换"
  └─ 否则
       ├─ in_game == true → 直接报错（拒绝掉线托管）
       ├─ logout(): 结算确认×3 → 设置齿轮 → 等 2.5s → 登出 → 等 2.5s → 确认 → 轮询登出（45s）
       ├─ login(): 预热点击 → 邮箱框×2 → Ctrl+A/Backspace → 逐字符输入
       │           → 密码框×2 → 清空 → 输入 → 提交 → 轮询目标邮箱（90s）
       └─ 复核 probe_login，邮箱不符则报错
```

细节：

- Unity 画布输入框不是 DOM，只能用 CDP `dispatchKeyEvent` 逐字符输入；支持 `@ . - _ + 空格`、字母（含 Shift）、数字。
- 登出前先点 3 次结算确认点：上一局刚结束会停在结算页，设置齿轮不存在；大厅/登录页点它是 no-op。
- 所有坐标来自账号的 `coords`，空则继承 `main`。
- `in_game` 由调用方传入：`pool_switch_account` 用 `GameTracker` 判断；`pool_start` 用日志判断。

### 13.6 进房状态机 `enter_room`

```
normalize_room / normalize_mode
  → 取 back / ranked / rooms.<room> / modes.<mode> 坐标
  → 先点 back（900ms）：大厅上是空白天空，等于 no-op；
    在友人场/活动页等子页面则返回上一层
  → dry_run? 只点 ranked + room，停在模式列表（不点开始）
  → 正式：
      click ranked      → 等 2.8s
      click room        → 等 2.8s
      click mode        → 等最多 20s 看日志 in_game
        失败则用 y 偏移 [0, +0.03, -0.03, +0.05, -0.05] + jiggle 重试 5 次
      ／全部失败 → 返回 entered=false（**不再点 cancel**：那个坐标在大厅会打开友人场）
```

成功判据：`session_state(logs_dir).in_game == true`，即日志出现 `FastTest.enterGame` 或 `ActionNewRound`，且晚于最近一次 `NotifyGameFinishReward`。

**人数校验**：`pool_start` 在 `entered=true` 后等 2 秒，从 `GameTracker` 读实际 3/4 人，与请求模式比对；不符时仍返回 `entered=true`（对局已经在跑，停 autoplay 会挂机），但 `message` 明确提示「人数不符，模式坐标需校准」。实测 `mode3e` 曾点到四人东卡片，因此这道校验是必需的。

房间/模式归一：

- 房间：`铜之间/银之间/金之间/玉之间/王座之间`，也接受 `copper/silver/gold/jade/throne`；
- 模式：`mode4e/mode4s/mode3e/mode3s`；`east/tonpu/e` 按人数映射，`south/hanchan/s` 同理。

### 13.7 结算 `settle`

- 取 `confirm` 坐标；
- 若日志 `in_game == true`（真的在打）→ 立即返回，绝不清屏；
- 先等 3 秒：结算页动画期间点击会被吞掉（实测立刻点 4 次无效）；
- 最多点 6 次，每次间隔 2.2s；大厅上该点是空白，等于 no-op；
- 用途：连打前清掉「終局」页。第一次点击可能只是把大结算面板收成段位变化面板，后续点击才回大厅。

### 13.8 常驻驱动 `spawn_pool_driver`

每 5 秒一轮，读 `config.pool`、日志态、`autoplay.enabled`、抓包端口。

**定时停止**

```
timer_enabled && !timer_fired
  → 首次进入时记录 timer_started
  → elapsed >= timer_hours*3600 且 !in_game
      → 落盘 autoplay.enabled=false，替换内存配置
      → timer_fired=true，写日志 "[pool] timer fired"
timer_enabled == false → 重置 timer_started/timer_fired
```

**自动连打（含 finish 竞态处理）**

```
auto_next && !in_game && pending_finish
  → selected_id 非空
  → 距上次任务 >= 60s
  → 解析账号（坐标继承 main）
  → GameCdp::connect(port)
  → settle() 清结算页（3s + 6 次点击）
  → switch_account(..., in_game=false)
  → enter_room(target.room, target.mode)
  → 若 entered=false 且仍不在局中：
       ensure_lobby()（Escape ×2 + reload + 等 15s）
       → 复核登录态 → enter_room 重试一次
  → 若 autoplay 原为关，重新置 true
```

**两个必须记住的竞态**：

1. `NotifyGameFinishReward` 刚写入时，同一 tick 的 `in_game` 可能仍为 true。旧实现会在这个 tick 把 `new_finish` 消费掉并 `continue`，导致之后永远不再触发。现在用 `pending_finish` 标记，直到某个 tick 真正空闲才消费。
2. 结算页动画会吞点击。`settle` 先等 3 秒、再点 6 次；仍失败则由 `ensure_lobby` reload 兜底。

### 13.9 坐标体系（最容易错的一环）

项目里同时存在两套归一化坐标，**不能混用**：

| 体系 | 来源 | 换算 | 用途 |
|---|---|---|---|
| 0..1 画布坐标 | `tools/accounts.json` → `[pool.accounts.coords]` | `px = rect.x + nx * rect.width` | 号池换号/进房 |
| 16×9 逻辑网格 | autoplay `majsoul/coords.rs` | `px = rect.x + (x16/16) * rect.width` | 局内出牌/鸣牌 |

两套坐标现在共用同一条执行链：`autoplay::cdp_input::evaluate_canvas_rect(page)` 实时读 `CanvasRect`，
再由 `CanvasRect::pixel01`（0..1）或 `CanvasRect::pixel`（16×9）换算，最后都走
`dispatch_click_shaped` 的四段式 CDP 鼠标事件。号池的 `pool::click_norm` 只是
`click_canvas_norm` 的薄封装，不再保留私有换算；若误套乘法，点会落到左上角
（历史上把 settings 算成 `(161,6)` 而不是 `(1411,52)`，换号/进房全失败）。

### 13.9.1 弹窗状态检测（不盲点固定坐标）

活动页/讲座面板没有 DOM 可控元素，过去 `dismiss_popups` 每轮都盲点两个硬编码坐标；
在普通大厅上其中一个正好是学士帽教程入口、另一个是帮助按钮，因此「清弹窗」本身会
把教程点开，改窗口大小后偏移更明显。现在改为：在 `requestAnimationFrame` 内用
WebGL `readPixels` 采样 5 个**画布归一化**点，和已验证页面指纹做最近邻判定
（大厅/讲座/活动/登录/对局/未知），只有识别到讲座才点真实关闭 X `(0.939,0.149)`，
只有识别到活动页才按 Esc；大厅、对局、登录页、未知画面一律不点固定坐标。
这套判定和打牌共用 canvas rect，因此分辨率/黑边变化不会导致点错。`requestAnimationFrame`
带 2.5s 超时，后台/最小化页面不会把命令卡死；识别到「账号已在另一处登入」弹窗时直接报错，
不会把大厅点击送到弹窗上。`enter_room` 在第一次点击前也会再验一次大厅状态（Unknown 重试两次），
只有确认是 Lobby/Game 才执行 ranked → room → mode 的固定坐标链，登录页/弹窗/未知画面一律取消点击。

### 13.10 账号与坐标继承

- `session::resolve_account(cfg, acc)`：`acc.coords` 为空时整表复制 `main` 的坐标。
- 三个账号共用同一套 Unity UI 布局，所以只需要 `main` 做完整标定。
- 前端新增账号时 `coords: {}`，即自动继承。
- `canon_key`：`login_email→loginEmail`、`login_password→loginPassword`、`login_submit→loginSubmit`、`logout_confirm→logoutConfirm`，其余原样。

### 13.11 IPC 命令（号池）

| 命令 | 参数 | 返回 | 说明 |
|---|---|---|---|
| `pool_probe_login` | — | `LoginState{account,logged_in,token_present}` | 连 63503 读 localStorage |
| `pool_switch_account` | `accountId` | `SwitchResult{account,switched,message}` | 对局中拒绝 |
| `pool_status` | — | `{session,in_game,last_enter,last_finish,finish_count,autoplay,auto_next,selected_id,stop_after_game}` | 日志态；前端每 2s 轮询 |
| `pool_set_autoplay` | `enabled` | `()` | 走 `update_config`，热开 manager |
| `pool_start` | `accountId, room, mode, dryRun` | `EnterResult{room,mode,entered,message}` | 一条龙：开 autoplay → 换号 → 等 9s → 进房 |
| `pool_stop` | — | `{stopped,in_game,stop_after_game}` | 置取消标志 + 关 auto_next；对局中只安排「本局后停」，不动当前局 |
| `list_sessions` | — | `SessionStatus[]` | 多开会话列表（每会话独立状态） |
| `get_session_status` | `slot` | `SessionStatus` | 单个会话状态 |
| `get_session_snapshot` | `slot` | `Snapshot\|null` | 单个会话牌局快照 |
| `session_pool_start` / `session_pool_stop` | `slot,…` | 同 pool_start/stop | 按会话开局/软停 |
| `create_session` | `accountId` | `{slot,profile,cdp_port}` | 把号池账号加入多开（上限 8，按需开浏览器） |
| `remove_session` | `slot` | `{removed}` | 移除会话；写 `sessions.disabled.json` 墓碑，重启不再出现 |

Tauri 参数命名：Rust `account_id` / `dry_run` 对应前端 `accountId` / `dryRun`。

**`pool_start` 顺序**（顺序很重要）：

1. 先开 autoplay，保证对局开始时 BotManager 已就绪，否则会漏掉最初几个决策；
2. 清空上一次的 `pool_cancel` / `pool_stop_after_game`；
3. 读日志，若 `in_game && !dry_run` → 直接报错，不换号；
4. 连游戏浏览器；
5. `switch_account`（若不在局中）；
6. **固定等 9 秒**：`localStorage.account` 比 Unity 大厅画面先就绪，过早点击会落在半绘制场景；
7. `enter_room`；
8. 若 `entered=false` 且不在局中 → `ensure_lobby`（Escape + reload + 15s）+ 复核登录 + 重试一次；
9. 若 `entered=true` → 等 2 秒，用 `GameTracker` 校验实际 3/4 人是否与请求模式一致。

### 13.12 前端号池页

`routes/Pool.tsx`：

- **账号池**：列表、选中、删除；「添加账号」展开表单（ID/昵称/UID/邮箱/密码）。
  - ID 留空 → 用邮箱前缀；重名自动 `_2`、`_3`…；
  - 昵称留空 → 用邮箱；
  - 坐标留空 → 继承 main；
  - 保存走 `patchPool`：取当前 `config` → 替换 `pool` 段 → `update_config` → 更新 store。
- **目标场次**：房间 5 选 1，模式 4 选 1。
- **运行选项**：自动连打、定时停止（可填小时数）。
- **开始对局**：调 `pool_start`，根据 `entered` 弹 toast；`message` 里的人数不符警告会一并显示。
- **停止任务**：调 `pool_stop`。不在对局 → 立即关 autoplay；对局中 → `stop_after_game=true`，本局打完后由 driver 关 autoplay。
- **真实状态**：每 2 秒 `pool_status` 轮询，徽章显示「对局中 / 自动打牌 / 本局后停止 / 空闲」，按钮禁用依据后端状态而不是本地 `running`。
- **探测**：调 `pool_probe_login` 显示当前登录邮箱。
- 页面只做「视图 + 配置写入」，业务权威在后端 `config.toml`。

### 13.13 号池时序

**开始一局**

```
React Pool.tsx
  └─ invoke pool_start(accountId, room, mode, dryRun=false)
       ├─ update_config(autoplay.enabled=true)   # 热 spawn BotManager
       ├─ session_state(logs).in_game? → 拒绝
       ├─ GameCdp::connect(63503)
       ├─ switch_account(page, acc, in_game=false)
       │    ├─ probe_login
       │    ├─ logout（结算确认×3 → 设置 → 登出 → 确认）
       │    └─ login（邮箱/密码 → 提交 → 轮询）
       ├─ sleep 9s
       └─ enter_room
            ├─ click back（子页面返回；大厅 no-op）
            ├─ click ranked
            ├─ click room
            ├─ click mode → wait_in_game(20s) ×5 组偏移
            └─ 失败 → ensure_lobby(Escape + reload + 15s) → 重试一次
       └─ 成功后 2s：GameTracker 校验实际人数 vs 请求模式
```

**一局结束 → 自动下一局**

```
all.log 出现 NotifyGameFinishReward
  → session_state: finish_count +1, in_game=false
  → driver(5s tick): new_finish && auto_next
      → settle() 点掉结算页
      → switch_account（通常已是对号，no-op）
      → enter_room
```

**Aogara 重启**

```
lib.rs
  ├─ restore::try_restore(logs)         # 5 分钟内未结束牌谱 → 重放 tracker
  ├─ 若轮到我方 → 暂存 kick
  └─ Tauri setup
       ├─ 恢复 kick（1.5s 后，仍轮到我方才补 TrackedEvent）
       └─ spawn_pool_driver

capture/chromium
  ├─ probe_existing_endpoint(profile, extra_args)
  │    ├─ 有端点 + 有新鲜未结束牌谱 → reuse + reload 游戏页
  │    └─ 有端点 + 无对局 → reuse，不 reload
  └─ 无端点 → reclaim + spawn + wait endpoint
```

### 13.14 错误与边界

| 情况 | 行为 |
|---|---|
| 未配置固定调试端口 | 命令直接报错，提示加 `--remote-debugging-port=63503` |
| 游戏浏览器没起 | `GameCdp::connect` 超时（5s）→ 清晰错误，不崩溃 |
| 对局中换号 | 拒绝，提示会掉线托管 |
| 对局中 `pool_start` | 拒绝 |
| 缺少坐标 | 报「缺少 xxx 坐标」；账号空坐标会先继承 main |
| 登录后卡活动页 | 首次进房失败后 Escape + reload（被动兜底），再重试一次 |
| 点模式没进局 | 5 组 y 偏移重试；仍失败返回 `entered=false`，不再点 cancel |

### 13.15 多账号并行会话（第二阶段核心）

单进程、2–8 个会话并行对局。每个 slot 是一个完整的 `SessionRuntime`（§14.3），
拥有独立浏览器/profile/CDP 端口/事件总线/Tracker/日志/历史/Bot/Autoplay/PoolDriver。

**槽位与资源分配**

| 项 | 规则 |
|---|---|
| 槽位名 | `slot1`…`slot8`；slot1 = 启动配置（`config.toml`），不可移除 |
| 端口 | slotN → `63502+N`（slot1=63503、slot2=63504、slot3=63505…） |
| profile | `profiles/p-<account>`（slot1 曾用名 `alt3` 已改名 `p-accountA` 对齐账号） |
| 日志/历史 | `./logs/slotN`、`./history/slotN`（slot2..N 无 tracing subscriber，`all.log` 为空，排障靠 inspector + toast） |
| 会话配置 | `configs/slot-<account>.toml`（完整 AppConfig 副本，`selected_id` 指向该账号） |
| 注册表 | `sessions.dynamic.json`（UI 添加的会话，重启恢复）+ `sessions.disabled.json`（移除墓碑，防止静态条目复活） |

**当前第五会话（2026-11-23 19:20 起）**

| slot | 账号 | CDP 端口 | profile | 日志/历史 |
|---|---|---|---|---|
| slot1 | accountA（<登录名占位>） | 63503 | `profiles/p-accountA` | `./logs/slot1` |
| slot2 | main（玩家A，登录名 FKU!!!） | 63504 | `profiles/p-main` | `./logs/slot2` |
| slot3 | accountA（accountA） | 63505 | `profiles/p-accountA` | `./logs/slot3` |
| slot4 | alt3（玩家A） | 63506 | `profiles/p-alt3` | `./logs/slot4` |
| slot5 | accountB | 63507 | `profiles/p-accountB` | `./logs/slot5` |

新增会话的两种方式：① 在多开页点「从号池添加」（`create_session`，自动取最小空闲 slot），
② 直接写 `configs/slot-<account>.toml` + 注册进 `sessions.dynamic.json`（本次 slot5 采用，
重启后自动装配，日志已验证五个会话全部启动）。

**进房兜底链（2026-11-23 修复 slot4 卡活动页）**

`enter_room_with_recovery()` 是进房的唯一入口（手动开局与自动连打 driver 共用），三级：
① 直接 `enter_room`；② 失败先 `dismiss_popups`（WebGL 像素指纹判定当前页面：识别到講座才点真实 X `0.939/0.149`，识别到活動才 Esc，**不盲点、不 reload**）；③ 仍失败才 `ensure_lobby`（Esc + reload + 15s）后再按状态清一次弹窗、复核登录态、
最后进房。关键：Majsoul **每次 reload 都会重新弹全屏活動页**，所以「失败即 reload」的旧逻辑
永远卡在活動页——这正是 slot4 点开始对局报异常的真正原因（inspector.jsonl 里
`majWorkshopActivityFetchData` + 多次 `prepareLogin` 即为证据）。

**重启自动接手未结算对局（2026-11-23）**：启动时 `try_restore` 在该会话日志树里找最近
5 分钟修改、末行非 `end_game` 的 `majsoul_*.mjai.jsonl`，重建 tracker。命中时置
`SessionRuntime.restore_pending_capture=true`，启动开抓包条件为
`start_capture_on_launch || restore_pending_capture`——**即使"启动不开浏览器"开关关着，
有活局未打的会话也会自动重连浏览器接手**，空闲/已结算会话仍不开。实测：重启时 slot5
末行 `dahai`（活局）→ cap=running + bot 续打；slot4 末行 `end_game` → 不自动开。
这是"重启后谁能自动接着打"的决定机制，与账号配置无关。

**账号表与登录检测的统一规则**

- 每个 slot 的配置是创建时的快照，号池后加的账号只存在于 slot1 主配置；
  `pool_start_inner` / `session_start_capture` / driver 全部加了 **slot1 账号表回退**，
  新账号（如 accountB）在任何会话都能被找到、登录、自动连打。
- 「开浏览器」开关：`session_start_capture` = 起抓包 → 等页面就绪 → 探测登录态 →
  不是目标账号就 `switch_account_retry`（2 次）；已是则 no-op。
- 「开始对局」开关：`pool_start_inner` 复用同一套链路（起抓包 → wait_page_ready →
  `switch_account_retry` → `enter_room_with_recovery`），再保留 autoplay/Bot 打牌逻辑，
  不会「只登录不开打」。
- driver 换号也从 `switch_account` 改成 `switch_account_retry`（2 次），避免偶发点击被动画吞掉。
- `login_name` 只属于用户名注册账号（如 main = `<登录名占位>`）；邮箱注册账号（alt3/accountB）
  没有该字段，登录判定用邮箱，不能混用。

**事件总线隔离（关键修复）**：`create_session` 给新会话建总线必须**全新创建**
（`event_bus::mjai_bus()` 等），绝不能克隆 slot1 的——曾经克隆导致 slot3 的牌局帧
流进 slot1 的 tracker，slot1 的 driver 误以为打完一局而自行开局。静态
`[[multi.sessions]]` 与 dynamic 注册表指向同一 slot 时用 `launched` 集合去重，
一个 slot 只装配一次（两套 driver 会互相打架）。

**页面职责**（2026-11-23 重排）：

- **号池页 `/pool`** = 账号仓库：列表 / 添加 / 编辑（铅笔）/ 删除，`ID` 创建后不可改。
- **多开会话页 `/sessions`** = 全部运行控制：
  - 全局开关（进程级单值）：自动连打 `set_pool_auto_next`（写启动配置一份，driver 读
    `auto_next_global`）、启动 Aogara 时打开浏览器 `set_start_capture_on_launch`（`[multi]`）；
  - 一键按钮：一键打开浏览器 / 一键关闭浏览器 / 一键开始对局 / 一键停止对局，
    各会话独立并发执行、互不阻塞；
  - 每会话卡片：账号下拉（换号，排除被其他会话占用的号）、段位场/模式下拉
    （`update_session_settings` 即时写该 slot 配置）、浏览器开关（开=启动+自动登录，
    关=强退）、开始对局 / 软停（busy 中可点，文案变「取消」，打断进行中的启动）/ 移除；
  - busy 按 slot 记录（`Set<slot>`），一个会话进房不再灰掉其他会话的按钮。
- **对局界面 `/game`** = 每账号独立视图：顶部会话切换下拉（`sessionStore.activeSlot`），
  数据按 slot 路由（见 §14.1 per-slot 事件）；切换时经
  `get_game_snapshot` / `get_mahgen_view` / `get_analysis` / `get_session_status`
  按 slot 拉取快照恢复画面。

**登录可靠性**：打开浏览器 → `wait_page_ready`（`readyState=complete` + canvas 存在，
60s 超时）→ `switch_account_retry`（失败 2s 后重试一次，每次重试先 re-probe，
登录半成功也能被识别）。profile 内登录态与 `selected_id` 一致时跳过换号——
换号是一次性成本，成功后 profile 即固化该账号。slot 配置里的账号表落后于号池时，
`pool_start_inner` 回退到启动配置的账号表查找。

**关闭浏览器（三段式）**：优雅停 supervisor（内含 CDP `Browser.close` 兜底）→
等 supervisor 退出（≤5s）→ `profile::reclaim_singleton` 按命令行 `--user-data-dir`
强退该 profile 的浏览器进程（其他会话不受影响）→ 手动置 `CaptureStatus::Stopped`
保证 UI 立即刷新。强退的必要性：Edge launcher handoff 后后端没有活子进程句柄，
`terminate(child)` 只能杀尸体。

**单实例保护**：`tauri-plugin-single-instance` 必须是 Builder 的第一个插件——
双实例会对同一个浏览器跑两套 autoplay/driver，点击互相打架（实测点进商店）。
第二实例启动即退出并聚焦已有窗口。

---

## 14. IPC 与前端

### 14.1 出站事件（后端 → 前端）

| 事件名 | 载荷 | 说明 |
|---|---|---|
| `mjai-event` | `MjaiEvent` | 每个翻译出的事件 |
| `bot-response` | `BotResponse` | Bot 决策，HUD 用 |
| `bot-status` | `BotStatus` | 同时镜像进 AppState |
| `capture-status` | `CaptureStatus` | 同时镜像进 AppState |
| `notify` | `Notification` | toast |
| `analysis-result` | `AnalysisResult` | 分析结果 |
| `history-recorded` | `HistoryEvent` | 新战绩 |
| `overlay-config` | `OverlayConfig` | 只发给 overlay 窗口 |

**per-slot 事件（多账号）**：slot2..N 的事件由 `spawn_slot_forwarders` 转发到
`<事件名>:<slotN>` 频道（如 `mjai-event:slot3`、`bot-status:slot2`），载荷不变；
slot1 保持上表的无后缀名（兼容 overlay 与外部监听者）。`notify` 始终全局广播。
对局界面（`useTauriBridge`）订阅全部 8 组频道，只把 `sessionStore.activeSlot`
的事件写入各 store，其余丢弃；`history-recorded(:slotN)` 例外——战绩全局合并。

### 14.2 入站命令（前端 → 后端）

共 **76 个** `#[tauri::command]`，注册在 `ipc_handlers!()`。分组：

| 分组 | 代表命令 |
|---|---|
| 配置 | `get_config`、`update_config` |
| 抓包 | `start_capture`、`stop_capture`、`restart_capture`、`get_capture_status`、`detect_system_chrome`、`list_cft_installed`、`download_chrome_for_testing`、`remove_chrome_for_testing` |
| 号池 | `pool_probe_login`、`pool_switch_account`、`pool_status`、`pool_set_autoplay`、`pool_start`、`pool_stop` |
| 多开会话 | `list_sessions`、`get_session_status`、`get_session_snapshot`、`session_pool_start`、`session_pool_stop`、`create_session`、`remove_session`、`update_session_settings`、`session_start_capture`（启动+自动登录）、`session_stop_capture`（强退）、`set_pool_auto_next`、`set_start_capture_on_launch` |
| Bot | `list_bots`、`set_active_bot`、`get_bot_settings`、`update_bot_settings`、`install_bot_from_github`、`install_bot_from_zip`、`update_bot_from_manifest`、`sync_bot_deps`、`delete_bot` |
| 状态 | `get_status`、`get_game_snapshot`、`get_mahgen_view`、`get_analysis`、`compute_bot_hora_score` |
| 日志 | `get_log_dir`、`open_log_folder`、`list_log_sessions`、`read_log_session`、`subscribe_log_events`、`read_inspector`、`subscribe_inspector`、`pack_logs` |
| 历史 | `list_game_history`、`get_game_history_record`、`get_game_history_events`、`delete_game_history_entry` |
| HUD | `set_overlay_enabled` |
| 更新 | `check_for_update`、`apply_update` |
| 云端/购买 | `native_api_*`（密钥、模型、健康检查、复盘、订单、订阅等） |
| 延时脚本 | `delay_script_status`、`replace_delay_script` |
| 杂项 | `open_external_url` |

### 14.3 AppState

> P1 多账号准备：每会话状态已抽到 `session::SessionRuntime`。`AppState` 新增
> `sessions: Arc<SessionManager>` 与 `session: Arc<SessionRuntime>`；下面列出的
> `config` / `log_session` / 8 条总线 / `game_tracker` / `capture_control` 等字段
> **仍是同一批 `Arc` 的别名**，64 个 IPC 命令暂不改。P2 会改为按 slot 路由。

`ipc/state.rs` 持有：

- `config: Arc<RwLock<AppConfig>>`、`config_path`、`log_session`；
- 8 条总线的发送端；
- `bot_status`、`capture_control`、`game_tracker`、`analysis_cache`、`history_store`、`history_platform`；
- `runtime: Option<PythonRuntime>`、`syncs_in_flight`；
- `bot_manager_started` / `autoplay_manager_started` 原子标志；
- `pool_cancel`（停止标志，长任务在每个检查点检查）与 `pool_stop_after_game`（软停标志）；
- `auto_next_global: Arc<AtomicBool>`——自动连打的**进程级单值**：切一次只写启动配置
  的 `[pool].auto_next` 一份，所有会话的 driver 读同一个标志（`with_session` 克隆时共享）；
  每会话的停止是运行时 `paused` 标志（`SessionRuntime.paused`），不写任何配置；
- `autoplay_context`（页面句柄/时间预算/输入计数共享槽）；
- `updater_lock`、`pending_update`。

### 14.4 前端结构

- 路由：`/`（Overview）、`/game`（对局，顶部会话切换）、`/bots`、`/history`、`/review`、`/logs`、`/sessions`（多开会话）、`/pool`（号池=账号仓库）、`/settings`，另有 overlay 独立窗口。
- 侧栏：`lib/menuList.ts`；fork 已删除侧栏底部的公告按钮与 GitHub/Discord/AogaraMS 外链，启动自动弹公告也已禁用（`AnnouncementsDialog` 只保留手动入口）。
- 状态：Zustand stores（config/game/bot/logs/history/inspector/capture/session/...）；
  `sessionStore.activeSlot` 决定对局界面显示哪个会话，`hydrateSession(slot)` 切换时拉快照。
- Overview：AogaraMS 推广卡已替换为「账号对局」卡片（2s 轮询 `list_sessions`，每会话一行：
  slot / 昵称 / 完成局数 / 上局时间 / 状态徽章）；默认窗口尺寸 1560×980 → 1150×760
  （缓解 WebView2 高 DPI 下"先大后小"，无法根除）。
- HUD：`tiles/` 下一组卡片（牌桌、手牌、对手、推荐、危险度、Bot 动作/响应、事件、通知等），由 `tiles/registry.tsx` 注册。
- i18n：`zh-CN` / `zh-TW` / `en` / `ja`；号池新增文案走 `t('pool.*')`。
- 覆盖窗：独立 label，`always_on_top`，透明度可调；自己恢复位置尺寸（避免插件把无边框改回有边框）。

---

## 15. 历史、日志与 Inspector

### 15.1 目录

| 路径 | 内容 |
|---|---|
| `logs/<session>/all.log` | 全量日志（号池判断局面就读它） |
| `logs/<session>/all.jsonl` | 结构化版本 |
| `logs/<session>/proxy.log` / `bot.log` | 分模块 |
| `logs/<session>/proxy.binlog` | 原始 WS 二进制帧 |
| `logs/<session>/majsoul/<flow>.mjai.jsonl` | 每局事件流（**启动恢复的数据源**） |
| `logs/<session>/inspector.jsonl` | 可分享时间线（白名单帧） |
| `logs/<session>/sensitive/inspector.jsonl` | 完整帧（含 token），禁止外传 |
| `history/index.jsonl` | 每行一条战绩（ULID） |
| `history/games/<ulid>.mjai.jsonl` | 单局完整事件流 |

### 15.2 定稿与替换

- 只有 `NotifyGameEndResult` 才把一局定稿；`NotifyGameTerminate`、换局、关机都会丢弃不完整缓冲。
- 雀魂断线重连带 `stable_game_id`，记录器会用服务端恢复的那份**替换**已有记录，而不是重复插入。
- 天凤中途重连的局不入历史。

### 15.3 日志打包

界面「日志 → 打包日志」生成 zip，自动剔除 `sensitive/`；报 bug 用这个，不要直接发 logs 目录。

---

## 16. 代理、更新与镜像

### 16.1 MITM 代理

- 监听 `127.0.0.1:23410`（可配），只本机可见；
- 自签 CA；雀魂证书重写在 `proxy/rewrite/majsoul_cert.rs`；
- `GET /ping` 返回 `pong` 用于健康检查；
- 必须排除 loopback。

### 16.2 自更新

`updater/`：

- `check` 拉 GitHub Release（可走镜像）；
- `apply` 下载 zip → 校验 SHA-256 + minisign（走镜像时强制验签）→ 原地替换 → 重启；
- 只读安装/不支持平台返回类型化错误，前端退化为「打开发布页」。

> **fork 注意**：编译版若走官方 updater，会被官方原版覆盖，号池功能丢失。升级前先备份，或禁用自动更新。

### 16.3 GitHub 镜像

`network.github_mirror_mode`：

- `auto`：短超时直连，失败再试内置镜像；
- `direct`：只直连；
- `mirror`：镜像优先（大陆网络推荐）；
- `github_custom_mirror`：自定义前缀，如 `https://ghfast.top`。

Chrome-for-Testing 下载失败时会自动改用 npmmirror。

---

## 17. 构建与发布

### 17.1 一键构建

`<安装目录>\build-aogara-native.cmd`：

1. 注入 MSVC + SDK 环境（`vcvars64.bat`，并手动补 `INCLUDE/LIB` 的 SDK 路径）；
2. `cd frontend && npm run build`；
3. 备份现有 `app\aogara.exe` 到 `aogara.exe.bak`；
4. `cargo build --release --target x86_64-pc-windows-msvc --features custom-protocol`。

产物：`_setup\src\Aogara-3\target\x86_64-pc-windows-msvc\release\aogara.exe`。脚本**不自动覆盖**运行中的 exe，确认后手动复制。

### 17.2 必须知道的坑

| 坑 | 现象 | 处理 |
|---|---|---|
| 少 `--features custom-protocol` | exe 启动后白屏/找 `localhost:1420` | 必须加该 feature，前端才会嵌入 |
| MSVC vs GNU | GNU target 在 Tauri 2 不受支持，windres 报错 | 只用 MSVC |
| `vcvars64` 不认新 SDK | `cl.exe` exit code 2 | 脚本手动补 SDK Include/Lib |
| 缺 `windres` | `tauri-winres` 失败 | PATH 加 `msys2\mingw64\bin` |
| 缺 `protoc` | `prost-build` 失败 | PATH 加同目录 |
| cargo 锁残留 | 编译卡住 | 杀净 cargo，删 `target/.cargo-lock` |
| 代理变量大小写并存 | VS 安装器 .NET 抛「已添加项」 | 只留大写一份 |
| **改前端后 cargo 不重建** | exe 还是旧界面（cargo 不知道 `frontend/dist` 变了） | `touch build.rs` 后再 `cargo build`，前端资源才会重新嵌入 |
| 启动时设 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` | 窗口创建失败、进程直接退出 | 63100 调试口由 HKLM WebView2 策略提供，**不要再设该环境变量**（参数重复会让 WebView2 起不来） |
| AI/脚本会话派生的子进程 | detached 启动的 aogara 几分钟后消失 | 长驻 GUI 必须由用户双击启动；沙箱/工具会话结束会清理其子进程 |

### 17.3 回滚

- 官方原版：`app\aogara.exe.pre-native.bak`
- 中间版（有号池页但未整合）：`app\aogara.exe.pre-integrate.bak`
- 源码目录无 git，改动不可自动回滚；改前先备份文件。

---

## 18. 运行手册与重启接手

### 18.1 日常启动

1. **只启动 `<安装目录>\app\aogara.exe`**（或桌面快捷方式）；单实例保护生效——重复双击只会聚焦已有窗口，**不允许双开**（两套自动化点同一个浏览器会互相打架）。
2. 不要启动 `tools\bridge.js`。根目录 `启动Aogara.cmd` 已改为只启动 exe（可用可不用）；bridge 时代的启动/辅助脚本已归档到 `tools\archive\2026-11-22\`。
3. 首次/换配置后确认 `capture.mode = "chromium"`，`extra_args` 含 `--remote-debugging-port=63503`。
4. 多开流程：号池页维护账号 → 多开会话页「从号池添加会话」→ 按会话选场次/账号 → 开浏览器开关（自动登录）或直接「一键开始对局」。
5. 对局界面（`/game`）顶部下拉切换查看哪个账号的牌局；HUD 自动出建议；autoplay 开着就由 Bot 出牌。
6. 收尾：「一键停止对局」等全部软停 → 「一键关闭浏览器」逐个强退（对局中的会话会跳过）。

### 18.2 重启行为

| 场景 | 行为 | 日志关键字 |
|---|---|---|
| 大厅重启 | 复用浏览器，不 reload | `reusing idle browser (no live game) — skipping reload` |
| 对局中重启 | 复用浏览器，reload 游戏页一次，重握手接手 | `reusing running browser` → `restart-reload: reload issued` → `seat resolved` |
| 浏览器已关 | reclaim profile + 重新拉起 | `chromium backend starting` → `chromium CDP endpoint` |
| 牌谱可恢复 | 启动时先重放 | `Startup game restore: N events ... seat ...` |
| 恢复到轮到我方 | 1.5s 后补一条决策事件 | `Startup restore: nudging bot at restored decision point` |

### 18.3 排错索引

| 日志/现象 | 含义 | 处理 |
|---|---|---|
| `terminating existing controlled chromium pid N` | reclaim 杀了旧浏览器 | 正常；不想被杀就说明在做进程复用 |
| `timed out reading chromium CDP endpoint` | 浏览器没起来/端口被占 | 查端口是否在系统保留区间、profile 是否被锁 |
| `browser exited (status ...)` | 浏览器被关 | 点「开始对局」或重启 Aogara |
| `WS Connection error: ResetWithoutClosingHandshake` | 页面/浏览器被关 | 同上 |
| `seat unresolved` 持续 | 没收到 `authGame` | 确认在对局中；必要时重启触发 reload 重握手 |
| `号池需要固定游戏调试端口` | extra_args 没配 | 加 `--remote-debugging-port=63503` |
| `连接游戏浏览器失败` | 抓包没起/端口不通 | 先启动抓包；确认 Edge 在跑 |
| `当前在对局中，不能换号` | 保护逻辑 | 等本局结束 |
| `点模式后未进入对局` | 坐标偏/未配对 | 校准 `modes.<mode>`，或看是否被弹窗挡住 |
| `登录后账号对不上` | 登录失败/邮箱错 | 检查 email/password 与验证码/风控；`switch_account_retry` 已自动重试一次 |
| `自动登录失败` / `等待游戏页面加载失败` | 页面未就绪或换号失败 | 重开浏览器开关再试；失败细节查 slot1 日志（slot2..N 无 all.log，只有 toast） |
| 多开页开关点不动 | 该会话 busy 或对局中保护 | 对局中浏览器开关锁定（防掉线）；busy 时软停仍可点（=取消） |
| 点进奇怪界面（商店等） | **双开实例互相点击** | 任务管理器确认只有一个 aogara.exe；单实例保护已加，旧版 exe 需更新 |
| slot1 显示的账号与配置不符 | profile 旧登录态残留 | 打开浏览器开关触发自动登录换号（重试已内置）；成功一次后即固化 |
| 侧栏号池页崩溃 `removeChild` | 旧 bridge 仍在注入面板 | 停 bridge；确认没有 `node bridge.js` 进程 |

### 18.4 已知的「伪失败」窗口

reload 后有约 17 秒：`syncGame` 可能空 actions、`inGame=false`、`inputOperation=0`。此时不要判定接手失败，等重连完成。

### 18.5 本轮整改验收记录（2026-11-23 00:02）

| 项 | 实测证据 | 结果 |
|---|---|---|
| A 空闲停止 | `pool_stop` → `auto_next=false, autoplay=false, stop_after=false` | ✅ |
| A 对局中软停 | 第二局中调用 `pool_stop` → `stop_after_game=true`，`autoplay` 保持 true；00:02:26 本局结束后 `autoplay=false`、`stop_after_game=false` | ✅ |
| B 真实状态 | 号池页右上角由后端 `pool_status` 驱动，对局中显示「对局中」；2s 轮询 | ✅ |
| C 失败兜底 | 23:41:52 自动连打首次进房失败 → `Escape + reload` → 23:42:52 `[pool] auto-next: 已进入 铜之间 mode4e` | ✅（结算页场景） |
| D 自动连打 | 第一局 23:39:39 结束 → 兜底后 23:42:52 开第二局 → 第二局 00:02:26 结束，`finish_count=2` | ✅ |
| 模式校验 | 请求 `mode3e` 实际进了四人东；已加 `GameTracker` 人数校验，本次未重新标定三人东坐标（按用户要求跳过） | ⚠️ 有校验、未校准 |

### 18.5.1 第二阶段验收记录（2026-11-23，多账号并行 + 界面重排）

| 项 | 实测证据 | 结果 |
|---|---|---|
| 三会话并行 | slot1/2/3 各自 `in_game=true`、牌河 30s 内均增长、`paused` 独立 | ✅ |
| 动态会话 | `create_session alt3`→slot4/63506；`remove_session slot4`→注册表清空；墓碑/恢复闭环 | ✅ |
| bus 隔离修复 | 「点 slot3 启动、slot1 被连带开局」根因=新会话克隆 slot1 总线；改独立总线后连带消失 | ✅ |
| 对局界面多账号 | `/game` 顶部切换 slot；per-slot 事件（`mjai-event:slotN`）+ 按 slot 拉快照 | ✅ |
| 自动登录 | 打开浏览器开关 → `wait_page_ready` → 自动登录该会话账号（重试 1 次） | ✅（偶发失败场景已重试覆盖） |
| 强退浏览器 | 关闭走 Browser.close + reclaim_singleton 按 profile 强杀，其他会话不受影响；状态立即刷新 | ✅ |
| 软停可打断 | 启动进行中按钮显示「取消」，`pool_cancel` 在检查点退出 | ✅ |
| 单实例保护 | 双击第二次 → 聚焦已有窗口、第二实例退出 | ✅ |
| Fork 界面清理 | 侧栏外链/公告删除、启动弹窗禁用、总览换账号对局卡 | ✅ |
| slot1 profile 对齐 | `profiles/alt3` → `p-accountA`，登录态保留 | ✅ |

### 18.6 调试工具（`tools/`）

三个开发期小工具，仅在需要时手动运行，不参与业务逻辑：

| 工具 | 用途 | 前提 |
|---|---|---|
| `_pool-ipc.js` | 通过 CDP 调 Tauri 命令：`node _pool-ipc.js pool_status`；`--eval "<js>"` 可执行任意页面表达式 | Aogara 启动时带 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=63100` |
| `_cdp-shot.js` | 截游戏页/UI 页到 PNG，仅用于人工诊断 | 端口 63503 / 63100 |
| `_cdp-click.js` | 按 CSS 像素点一次游戏页，用于坐标标定 | 端口 63503 |

> 截图文件只用于人工排查和坐标标定；业务判定不依赖截图文件或外部模板匹配。
> 弹窗状态例外：直接在 `requestAnimationFrame` 内对 WebGL 画布做 `readPixels` 指纹采样，
> 这是实时画布状态探测，不读磁盘图片，也不做图像模板匹配。

---

## 19. 已知限制与风险

### 19.1 机制限制

1. **中途接手必须 reload 一次游戏页。**
   CDP 只能收订阅后的新 WS 帧，无法回溯已发生的 `authGame`；页面 JS 不暴露 WS 引用；牌谱恢复只能重建 tracker 快照，给不了后续实时事件流。因此「完全不刷新 + 对局中接手」在当前架构下做不到。现在做到的是：大厅零刷新；对局中刷新一次页面（不重开浏览器、不回登录页）。
2. **座位只在 `authGame` 握手帧里。** 中途帧、页面 JS 都没有。
3. **牌谱恢复是兜底，不是主路径。** 要求 5 分钟内仍在写、首行 `start_game`、末行非 `end_game`；强杀导致牌谱不完整时可能恢复不出一致状态。

### 19.2 号池功能限制

1. **uid 不做服务端校验。** v3.8.0 日志把 `account_id` 脱敏成 `<withheld>`；`localStorage`/`sessionStorage` 只有邮箱和 `access_token`，uid 仅作为多个键名的后缀出现，且旧账号的键会残留，不能证明当前登录者。结论：**保持邮箱唯一判据，uid 仅展示**。
2. **坐标是人工标定。** 实测 `mode3e` 会点到四人东；已加人数校验（会提示但不回滚），但三人东坐标尚未重新标定。UI 改版仍需重标；单纯窗口分辨率/黑边变化已由 canvas rect 统一换算覆盖。
3. **活动页兜底仍是被动触发。** `dismiss_popups` 被调起时会先用 WebGL 像素指纹判断当前页面，识别到活動/講座才做对应动作；但登录成功后不会主动轮询直到 `enter_room` 失败才进入这条兜底链。
4. **结算清屏依赖 `confirm` 坐标。** 大厅上该点是空白（no-op），但 UI 改版后需重新确认。
5. **密码明文。** `config.toml` 泄露即账号泄露。
6. **同一 profile 不能双开。** 两个 Aogara 实例会互相 reclaim；程序已有单实例保护兜底。
7. **slot2..N 没有 `all.log`**（不装 tracing subscriber）：排障只能靠 inspector.jsonl、
   toast 错误和该会话调试口（63504/63505/…）直连浏览器检查。
8. **overlay 悬浮建议只跟随 slot1**（全局 `bot-response` 事件），多账号悬浮跟随未做。
9. **点击坐标依赖 canvas rect，不依赖窗口绝对像素。** 号池与打牌现在共用
   `evaluate_canvas_rect` + `dispatch_click_shaped`，缩放/黑边变化不会让两者之一漂移；
   但雀魂 UI 若在极端长宽比下重排元素相对位置，人工标定的 `ranked`/`rooms`/`modes`
   仍需重标。弹窗类操作已改为状态判定，不再盲点固定坐标。
10. **slot 配置是创建时的快照。** 号池后加的账号靠 `update_session_settings` /
    `pool_start_inner` 的账号表回退解析；直接手改 slot 配文件仍可能不同步。

### 19.3 平台限制

- 天凤中途重连只给快照，会暂停分析到下一局，且不入历史。
- 一番街只有 MITM，且不支持 AutoPlay。
- 天月未支持。

### 19.4 合规与账号风险

官方声明仅供学习/研究；使用 AutoPlay、多账号轮换可能违反平台 ToS，封号风险自担。本项目不调用任何付费接口。

---

## 20. 本 fork 变更记录

| 日期 | 变更 | 关键文件 |
|---|---|---|
| 2026-11-24 18:36 | **精修轮（全量测试后的批量修复）**：按《问题修复方案》修 6 项并编译部署新 exe（备份 `aogara.pre-precision.exe`）。① 四语 i18n `sidebar.derivative` 改回「基于 Akagi」（自指 bug）；② `check_for_update` 直接 `Ok(None)` 关闭更新检查（用户决定留引子，后续接个人仓库），日志 github 请求=0；③ `external.ts` 4 外链置空 + `openExternal` 空值保护（用户决定留引子）；④ `session_stop_capture` 强退结果存 `reclaim_result`、状态回写与 bot 释放改 finally 语义（修批量关浏览器不释放），前端 `closeAll` 改 `Promise.all` + `closeBrowser` 返 bool；⑤ 前端拆 `browserBatchToken`/`queuedRef`/取消按钮态（修一键取消排队无效）；⑥ `enter_room` 画面 Unknown 时延长等待到 14s + `ensure_lobby` 恢复 + `dismiss_popups` 重试（横幅遮挡容错，保留 fail-safe）。验证：P-02/04/05/06/08 全通过，P-16a 逻辑编译生效但本机资源加载慢未完整冒烟，P-16b slot2 匹配循环未改。**P-03** 调试口注册表因非管理员未写入，已生成 `tools/set-debug-port.reg` 待用户双击。详见 `docs/Aogara-问题修复方案.md` v2 | `frontend/src/i18n/resources/*.json`、`frontend/src/lib/external.ts`、`frontend/src/routes/Sessions.tsx`、`src/ipc/commands.rs`、`src/pool/mod.rs`、`tools/set-debug-port.reg` |
| 2026-11-24 18:10 | **全量测试执行 + 五开补测**：按《全量测试方案》跑完 9 模块；PRE 全过、品牌/启动/抓包/号池/日志/文档主干通过；发现并简单修：使用手册 v3.8.0→v1.1.0、`.lnk`→`.bat`；测试副作用 5 处 autoplay 全还原。补测确认 accountA 已注册（真实 UID **<UID>**，大厅昵称「玩家A」），回填 `config.toml` 空 UID（此前 precheck 退回邮箱判重）；五开同时进游戏实测 3/5 进房成功、bot ready、无顶号无崩溃，slot1 被游戏「新赛季开始」横幅遮挡触发安全保护、slot2 匹配循环未进房。问题 P-01~P-16 记入测试方案 §五 | `docs/Aogara-全量测试方案.md`、`app/configs/config.toml`、`app/aogara.exe` |
| 2026-11-24 | **测试模式**：`[test]` 配置门控、五账号顺序多轮压测、`test_start/stop/force_exit/status` IPC、页面状态监控与 JSONL 日志、`state.json` 重启续跑、random/immediate 任意阶段强退；生产默认不启用 | `config/test_mode.rs`、`testing/`、`ipc/state.rs`、`ipc/commands.rs`、`lib.rs`、`configs/test-mode.smoke.toml`、主文档 §20.1 |
| 2026-11-24 03:12 | **五开卡顿诊断**：实测 CPU 总量 11.9–21.5%、内存余 10.5 GB、磁盘/总 GPU 不满，但单核峰值 86%；单开浏览器树 11 进程/1.13 GB、canvas backing 2560×1440。结论：总占用不是判据，少数核心 + WebGL 渲染排队/进程开销才是方向；现场仅 63503 在线，非五开直接采样。新增排错建议，不改运行逻辑 | 主文档 §20.2 |
| 2026-11-24 03:30 | **五开卡顿优化落地**：一键开浏览器/开对局按 1.2s 错峰且可取消剩余排队；新增 `tools/perf-monitor.ps1` 每核+Edge 进程+GPU 监控；测试模式采样默认/配置统一 1000ms；新增可逆 `tools/gpu-pref.ps1` 并为 Aogara/Edge 写入 `GpuPreference=2;`（备份已留） | `frontend/src/routes/Sessions.tsx`、`config/test_mode.rs`、`configs/test-mode.*.toml`、`tools/perf-monitor.ps1`、`tools/gpu-pref.ps1`、主文档 §20.1/§20.2 |
| 2026-11-24 04:28 | **系统占用最低化 P0+P1**：去 occlusion 保活参数、`capture.http.bodies=false`、`close_idle_browser_secs=120`；Inspector 改 lite/BufWriter/双文件独立 flush；五会话 5s driver 合并为单全局 2s driver；已打完且空闲的会话自动释放浏览器；前端 5s 兜底 + `session-status` 事件驱动 + 隐藏暂停 | `configs/*.toml`、`config/{multi,logging,mod}.rs`、`inspector/mod.rs`、`logger/{mod,session}.rs`、`ipc/{commands,mod}.rs`、`lib.rs`、`frontend/src/routes/{Sessions,Overview,GameDashboard}.tsx`、主文档 §20.3 |
| 2026-11-24 13:02 | **一键多开顶号修复**：根因是 slot1 profile 被污染成 accountB 的 UID(<UID>) 且 `account=null` 命中旧"跳过换号"兜底。新增 uid 权威判据 `Identity/probe_identity`、删除跳过兜底、`force_clear_login_storage` 清残留 token、顶号弹窗自愈、`precheck_sessions` 启动前去重、`AppState.account_locks` 全局账号互斥锁；补 accountA/accountB 真实 uid、修正被写成密码的 label。实测：污染识别/强制清理/幂等跳过/并发拒绝四项正确 | `pool/mod.rs`、`ipc/{commands,state}.rs`、`frontend/src/routes/Sessions.tsx`、`configs/*.toml`、主文档 §20.4 |
| 2026-11-24 13:32 | **关浏览器释放机器人**：`session_stop_capture` 关浏览器后同步关该会话 autoplay（经 `state.with_session` 定位目标会话，修复首版误用全局 state 导致 slot3/4/5 不释放的坑）。实测 slot3 `autoplay=true→false`；从零重启 aogara + 五账号一键全开：5 浏览器 8.4s 全开、UID 全不同无顶号；进对局因客户端资源初始化慢未成功，按要求强退 | `ipc/commands.rs`、主文档 §20.5 |
| 2026-11-24 14:37 | **accountA 登录失败诊断**：确认自动登录逻辑正常执行（表单已自动填入提交），失败在服务端——登录页弹错误码 1002「帳號不存在」，用户确认账号尚未注册。旧代码兜底见 token 就跳过换号，掩盖了该账号从未登录的事实。非代码问题。更新 §20.4 已知限制、§20.5 slot1 异常、新增 §20.6 完整诊断（含「账号自助注册」功能引子） | 主文档 §20.4/§20.5/§20.6 |
| 2026-11-24 14:57 | **版本号改 1.1.0**：区别上游脚本版本（3.8.0）。改 `Cargo.toml`/`Cargo.lock`/前端 `VERSION_FALLBACK`；公告历史条目保留 3.8.0 不动。前端构建+Rust 编译通过，exe 内 `aogara/1.1.0` 确认；备份 `aogara.pre-v110.exe`，生产实例 14:57:21 启动（PID 14604） | `Cargo.toml`、`Cargo.lock`、`frontend/src/lib/appVersion.ts` |
| 2026-11-24 16:45 | **全量改名 Akagi → Aogara（D1/D2/高风险全改，D3 保留）**：根目录 `<安装目录>` → `<安装目录>`；exe `akagi.exe` → `aogara.exe`（Cargo package/bin 同步）；批量替换 248 文件约 2579 处（bot 协议 `@@AOGARA_NOTIFY@@`/环境变量 `AOGARA_BOT_CONFIG`/内置 bot ID `aogara-native`/日志 target/UA/19 个 localStorage 键/i18n 四语/所有脚本与配置路径）；logo SVG wordmark 重画为 Aogara；证书代码 CN 改 `Aogara Proxy CA` + BASENAME `aogara-ca`，旧 CA 文件清空待自动生成；清理 4G 旧 `akagi` 编译缓存。系统层：GPU 偏好注册表改指新 exe、桌面建 `Aogara.bat` 启动器、旧 WebView2 目录归档为 `.old-backup`。**实测**：窗口标题 Aogara、precheck 5 账号唯一、slot2 登录态保留（UID <UID>）、GPU 偏好新键生效、exe 内无旧标识。**保留**：LICENSE/NOTICE 上游署名、公告历史 entries.ts、`backup/`/`docs/archive/` 历史文件（10 个）、构建工具链目录 `<安装目录>BuildTools`（改名会断链）。备份 `F:\aogara-pre-rename-backup`。详见 `docs/Aogara-全量改名清单.md` | 全仓库 + 系统层，见清单文档 |
| 2026-11-24 15:17 | **品牌定名 Aogara（P0+P1 应用标识）**：用户定名「Aogara」（青雀的日文训读 aogara，非拼音）。`tauri.conf.json` productName/identifier/title → Aogara / `com.xiao.aogara` / Aogara；`index.html` `<title>`、侧栏 aria-label/版本徽标同步；`NOTICE` 顶部追加衍生声明（Aogara 1.1.0 / Copyright 2026 Xiao / Based on Akagi v3 / Apache-2.0），原全文与第三方清单保留。i18n 四语加 `sidebar.derivative` 标识。**当时保留未动**：logo SVG（含上游作者署名）、exe 文件名 `akagi.exe`、公告历史条目——后于 16:45 全量改名中处理。**已确认** identifier 改动不影响配置定位（`current_exe()`）与游戏登录态（独立 `profiles/p-*`）。备份 `akagi.pre-aogara.exe` | `tauri.conf.json`、`NOTICE`、`frontend/index.html`、`frontend/src/components/sidebar/Sidebar.tsx`、`frontend/src/i18n/resources/*.json` |
| 2026-11-21 | 原生号池第一阶段：`/pool` 页面、`PoolConfig`、登录探测、换号 | `routes/Pool.tsx`、`config/pool.rs`、`pool/mod.rs` |
| 2026-11-22 | 进房、`pool_start` 一条龙、autoplay 热开 | `pool/mod.rs`、`ipc/commands.rs` |
| 2026-11-22 | `pool_status`、`spawn_pool_driver`（连打/定时） | `pool/session.rs`、`ipc/commands.rs` |
| 2026-11-22 | 全量 MSVC release 编译，替换 exe，账号迁移 | `build-aogara-native.cmd`、`config.toml` |
| 2026-11-22 | 弃用 bridge；修面板注入导致的 React `removeChild` 崩溃 | `bridge.js`（`AOGARA_NO_PANEL=1`） |
| 2026-11-22 | 重启复用浏览器（`probe_existing_endpoint`） | `capture/chromium/launch.rs`、`mod.rs` |
| 2026-11-22 | 启动牌谱恢复（`restore.rs`）+ 决策点唤醒 | `game_state/restore.rs`、`lib.rs` |
| 2026-11-22 | attach 后条件 reload 重握手，接手中途对局 | `capture/chromium/cdp.rs` |
| 2026-11-22 | 条件 reload：仅对局中刷新，大厅跳过 | `capture/chromium/mod.rs` |
| 2026-11-22 | 号池页支持添加/删除账号、ID 自动生成 | `routes/Pool.tsx` |
| 2026-11-23 | 新增 `pool_stop`：取消标志 + 关 auto_next；对局中软停（本局打完再关 autoplay） | `ipc/state.rs`、`commands.rs`、`pool/mod.rs` |
| 2026-11-23 | 号池页接 `pool_status` 2s 轮询；徽章/按钮改用后端真实状态 | `routes/Pool.tsx`、`types.ts` |
| 2026-11-23 | `settle` 适配结算动画：3s 预热 + 6 次点击；`enter_room` 先点 back、失败不再点 cancel | `pool/mod.rs` |
| 2026-11-23 | 修 finish 竞态：`pending_finish` 保留结算事件，避免 in_game 同 tick 吃掉 | `ipc/commands.rs` |
| 2026-11-23 | 连打进房失败兜底：Escape + reload + 15s，复核登录后重试一次 | `ipc/commands.rs`、`pool/mod.rs` |
| 2026-11-23 | P1 多账号机械抽取：`SessionRuntime` / `SessionManager`；`AppState` 保留单会话别名 | `session/mod.rs`、`ipc/state.rs`、`lib.rs` |
| 2026-11-23 | 号池新增 `accountA`（user@example.com），实测登录/进房/出牌成功 | `config.toml` |
| 2026-11-23 | 进房后校验实际 3/4 人；不符时返回警告（mode3e 曾误进四人东） | `ipc/commands.rs` |
| 2026-11-23 | uid 探测结论：无可靠来源，关闭该需求，保持邮箱唯一判据 | 本文档 §19.2 |
| 2026-11-23 | 真机验收：软停、自动连打两局、兜底进房均通过；最终 exe 已替换 | `app/aogara.exe` |
| 2026-11-23 | **第二阶段**：多会话动态创建/移除（`create_session`/`remove_session`、dynamic 注册表+墓碑） | `ipc/commands.rs`、`lib.rs` |
| 2026-11-23 | 连打改进程级单值 `auto_next_global`；每会话停止用运行时 `paused` | `ipc/state.rs`、`session/mod.rs` |
| 2026-11-23 | 页面职责重排：号池页只留账号仓库，运行控制全部移入多开会话页（全局开关+四个一键按钮+每会话卡片） | `routes/Pool.tsx`、`routes/Sessions.tsx` |
| 2026-11-23 | 修「点 slot3 启动 slot1 也开局」：`create_session` 改独立事件总线；静态+dynamic 双 spawn 去重 | `ipc/commands.rs`、`lib.rs` |
| 2026-11-23 | 新增 `update_session_settings`（slot 级账号/场次）+ 启动配置账号表回退 | `ipc/commands.rs` |
| 2026-11-23 | 对局界面每账号独立：per-slot 事件转发（`event:{slot}`）、快照命令加 slot 参数、`sessionStore` 切换 | `ipc/mod.rs`、`lib.rs`、`hooks/useTauriBridge.ts`、`stores/sessionStore.ts`、`routes/GameDashboard.tsx` |
| 2026-11-23 | 打开浏览器自动登录：`wait_page_ready` + `switch_account_retry`；修复「页面没加载完就点登录表单」卡登录界面 | `pool/mod.rs`、`ipc/commands.rs` |
| 2026-11-23 | 关闭浏览器三段式强退（Browser.close + reclaim_singleton 按 profile 杀进程），状态立即置 Stopped | `capture/chromium/launch.rs`、`ipc/commands.rs` |
| 2026-11-23 | 单实例保护 `tauri-plugin-single-instance`（双开会互相点击，实测点进商店） | `Cargo.toml`、`src/lib.rs` |
| 2026-11-23 | Fork 界面清理：删侧栏外链/公告、禁启动弹窗、总览换「账号对局」卡、默认窗口 1150×760 | `Sidebar.tsx`、`Overview.tsx`、`tauri.conf.json` |
| 2026-11-23 | slot1 profile 对齐：`profiles/alt3` → `p-accountA`（登录态保留）；号池新增 alt3 会话 slot4 | `config.toml` |
| 2026-11-23 19:20 | 新增 slot5=accountB（63507/p-accountB，独立日志历史）；接通 `enter_room_with_recovery`（修 slot4 活動页卡死）；driver 换号重试 + slot1 账号表回退 | `slot-accountB.toml`、`sessions.dynamic.json`、`ipc/commands.rs` |
| 2026-11-23 19:45 | 实机验证五会话装配；slot5 首次换号 retry 第2次成功；修复多級弹窗（活動→講座→大厅，`dismiss_popups` 加 X 0.89/0.15 跑两轮） | `pool/mod.rs` |
| 2026-11-23 20:16 | 重启自动接手未结算对局：`restore_pending_capture` 标志，try_restore 命中活局置位，开抓包条件 `start_capture_on_launch || 该标志`；实测 slot5 活局自动续打、slot4 已结算不开 | `session/mod.rs`、`ipc/state.rs`、`lib.rs` |
| 2026-11-23 20:26 | 会话主按钮改三态：空闲「开始对局」/启动中红色「取消」/对局中「软停」，取消就在点开始的原位；一键开始按钮在 busy 时变「一键取消启动」（只取消 busy 会话）。CDP 实测点击→变红取消→点取消中断（paused=true）→恢复 | `routes/Sessions.tsx` |
| 2026-11-23 20:49 | 软停并入自动连打：单会话按钮简化为「开启对局/停止对局」两态（active=busy‖in_game‖autoplay）；关闭自动连打后 driver 在本局结束时自动关 autoplay 释放 bot（日志 `auto-next off: autoplay disabled after match, bot released`）。实测 finish 1→bot=stopped→不再续局 | `routes/Sessions.tsx`、`ipc/commands.rs` |
| 2026-11-23 23:57 | 修「一键启动点到客服/讲座」：`GameCdp::connect` 改用 location.href 选真游戏页（Page::url() 在此 fork 返回 ":"）；`ensure_game_url` 在 wait_page_ready 前把非游戏页拉回；`dismiss_popups` 讲座 X 改 0.893/0.245（旧 0.89/0.15 是"?"入口反而打开讲座）、去掉盲点中心（误点段位场）；logout UI 超时则 JS 强清 localStorage 兜底。实测 slot5 进房成功 | `pool/mod.rs`、`ipc/commands.rs` |
| 2026-11-24 | **自适应点击统一**：号池 `click_norm` 改为调用打牌同一条 `cdp_input::click_canvas_norm`（共用 `CanvasRect::pixel01` + 四段式 CDP 事件），不再保留私有换算；`dismiss_popups` 改为 WebGL `readPixels` 像素指纹判定页面，只有识别到講座/活動才做对应动作，普通大厅/对局/登录/未知画面不再盲点固定坐标（修「改分辨率后点到学士帽/客服/讲座」）；「账号已在另一处登入」弹窗明确报错，不再当大厅继续点 | `autoplay/context.rs`、`autoplay/cdp_input.rs`、`pool/mod.rs`、主文档 §13.9.1 |
| 2026-11-23 19:30 | **代码逻辑优化**：端口解析统一（`util::parse_fixed_cdp_port` 公共函数，`pool::game_cdp_port`/`launch::parse_fixed_debug_port`/`lib.rs` 三处转发，支持 `=` 与分离两写法）；号池端口错误串提常量 + `pool_cdp_port` 辅助函数（3 处 `ok_or_else` 精简）；`key_map` 返回结构体化 `KeyStroke`；键盘输入唯一实现注释约定（pool 拥有，cdp_input 待迁入） | `util/mod.rs`、`pool/mod.rs`、`capture/chromium/launch.rs`、`lib.rs`、`ipc/commands.rs`、`autoplay/cdp_input.rs` |
| 2026-11-23 19:30 | **磁盘清理**：删废弃 chrome-profile、123 个时间戳日志、2 个旧 exe 备份、setup 压缩包、12 个 config 小备份（释放 0.80 GB）；保留 target/工具链/profiles/backup | — |
| 2026-11-22 | 本文档建立；旧文档归档 | `docs/` |

### 20.1 测试模式（仅测试配置启用）

测试模式由配置 `[test]` 控制，默认 `enabled = false`。生产 `config.toml` 不写这一段时，
不会创建测试日志、页面监控器或压测 runner；正常号池/自动连打代码完全不经过测试模块。

**启动**

```toml
[test]
enabled = true
auto_start = true          # 启动 2s 后自动开跑；false 时用 IPC test_start
resume = true              # 重启后从 state.json 续跑
rounds = 3                 # 每个账号跑几轮
accounts = ["slot1", "slot2", "slot3", "slot4", "slot5"]  # 空 = 全部会话按 slot 排序
room = "copper"            # 空 = 用会话自己的 target
mode = "mode4e"
force_exit = "random"      # none | immediate | random
exit_phases = ["page_load", "login", "lobby", "entering", "in_game", "settlement"]
exit_delay_min_ms = 200
exit_delay_max_ms = 4000
monitor_interval_ms = 1000          # WebGL readPixels 是同步读回，五开不要低于 1000
heartbeat_secs = 10
max_match_secs = 900
log_dir = "./logs/test"
seed = 12345               # 可选，固定随机相位/延迟
```

**流程**

1. 按 `accounts` 顺序（默认五个会话 slot1…slot5）逐账号 `pool_start_inner`；
2. 每个账号跑满 `rounds` 轮，轮内按账号顺序串行，不并发开多局；
3. 每个会话独立页面监控器持续采样 URL / readyState / canvas / 大厅/登录/讲座/活动/对局状态；
4. `force_exit`：
   - `none`：正常打完；手动 `test_force_exit` 仍可立即退出；
   - `immediate`：观察到 `exit_phases` 中第一个阶段就强退；
   - `random`：每局随机选一个 phase + `exit_delay_*` 延迟后强退；
5. 强退走 `close_slot_browser`：先停 capture，再按 profile 杀浏览器，**不经过**
   `session_stop_capture` 的“对局中禁止关闭”检查。

**日志**

每次运行创建 `logs/test/run-<timestamp>/`：

| 文件 | 内容 |
|---|---|
| `events.jsonl` | 全局事件流，每条含 `seq/ts/ts_ms/run_id/round/slot/account/match_id/phase/screen/url/ready_state/in_game/finish_count/detail/error` |
| `accounts/<slot>.jsonl` | 同一账号的页面事件切片，便于按账号定位 |
| `state.json` | 重启检查点：`next_round/next_account/current/completed_matches/status` |
| `summary.json` | 结束时总场次、事件数、最后错误 |
| `latest.json` | 指向最近一次 run 目录 |

事件类型：`test_started/test_resumed/test_stopped/test_finished`、`round_started/round_progress/round_finished`、
`match_starting/match_entered/match_exited/match_timeout`、`page_navigated/page_loading/page_loaded/page_state/page_heartbeat/page_closed`、
`screen_changed/phase_changed`、`force_exit_requested/force_exit_completed/force_exit_failed`、`error`。

**重启续跑**

`state.json` 在每次 `match_starting` 和每个 match 完成后原子写入。重启且 `resume = true` 时：

- 若上次 `status=running` 且还有 `current`，先记录 `test_recovered`，强制关闭残留浏览器，
  再从同一 `round/account` 重试；
- 已完成的匹配不会重跑；
- `status=finished/stopped` 则新建 run。

**IPC 控制**

| 命令 | 作用 |
|---|---|
| `test_start` | 手动启动（仍需 `[test].enabled=true`） |
| `test_status` | 返回 running/run_dir/current/completed/events/last_error |
| `test_force_exit {slot?, reason?}` | 立即强退当前或指定 slot |
| `test_stop` | 停止整轮压测并强退当前对局 |

冒烟配置在 `app/configs/test-mode.smoke.toml`（只跑 slot1/page_load，尽快验证日志链路）；
`app/configs/test-mode.five.toml` 是五账号顺序压测示例，`test-mode.resume.toml` 用于重启续测，
`test-mode.ingame.toml` 用于 in_game 阶段强退。正式压测把 `rounds` 调大、`force_exit` 改 `random` 即可。

### 20.2 五开卡顿诊断（2026-11-24 03:12 现场）

用户反馈“同时启动 5 个账号后，电脑总占用没满但很卡”。现场采样结论：**总占用没满是真的，
但总占用不是这类卡顿的正确判据**。

| 指标 | 现场值 | 结论 |
|---|---|---|
| CPU 总占用 | 11.85% / 11.90% / 21.48% | 总量确实不满 |
| 单核占用 | core13 平均 56.6%、峰值 86%；core14 平均 53%、峰值 78.2% | 少数核心接近饱和，足以造成界面卡顿 |
| 内存 | 31.78 GB 总量，约 10.5 GB 可用 | 不是瓶颈 |
| 磁盘 | `% Disk Time` 0.23–0.35%，队列 0 | 不是瓶颈 |
| GPU 3D | 最高 3.84%（单个 Edge GPU 进程） | 总占用不高，但 WebGL 是逐帧同步任务 |
| 电源计划 | 高性能 | 非降频策略问题 |

现场只有一个 Aogara 游戏浏览器：`63503` 页面在线，`63504`–`63507` 无 CDP 响应；
日志也只有 `slot1` 在 03:09 开抓包。因此这组数值是**单开外推依据，不是五开同时运行的直接采样**。

单开浏览器树已有 11 个进程、约 1134 MB、5384 个句柄；游戏画布 CSS 可见区域仅
`1116×628`，但 Unity canvas backing store 固定为 `2560×1440`，DPR 1.5。
按五开线性外推，仅游戏浏览器就有约 55 个进程、5.7 GB 内存、5 份 2560×1440 WebGL 画布；
现场另有 45 个普通 Chrome/Edge 进程约 3.6 GB。五个 Unity WebGL 页面同时运行时，
容易把 4–5 个逻辑核心推到高占用并产生渲染排队，但 16 核总量仍可能显示不满。

**已落实的低风险处置**：

1. **五开启动错峰**：`routes/Sessions.tsx` 的 `openAll()` / `startAll()` 改为按索引错峰
   1.2 秒发起启动，不等待上一会话完成；批量 token + `queued` 集合让“一键取消启动”
   能阻止剩余排队项，不影响非队列会话。
2. **每核/进程判据**：新增只读脚本 `tools/perf-monitor.ps1`，按 IntervalSec 采样每核 CPU、
   `aogara`/Edge renderer/GPU 进程 CPU、内存/句柄与 GPU Engine；用它替代“只看 CPU 总量”。
3. **测试采样降频**：`TestConfig::monitor_interval_ms` 默认 400 → 1000；所有
   `test-mode.*.toml` 及 `slot-accountB.toml` 的 `[test]` 覆盖值同步改为 1000。
   生产配置仍不启用 `[test]`。
4. 关掉无关的普通 Chrome/Edge 标签页/进程：本轮**未执行**，避免影响用户现有页面。
5. **GPU 高性能偏好**：新增可逆脚本 `tools/gpu-pref.ps1`；已备份
   `UserGpuPreferences.backup-*.json`，并为 `aogara.exe` 与 `msedge.exe` 写入
   `GpuPreference=2;`。不禁用 Oray/MuMu 虚拟显示适配器；应用改动需重启后生效。
6. 若后续要改窗口尺寸/DPR，先确认 `CanvasRect::pixel01` + `click_canvas_norm` 仍生效；
   当前点击链与 CSS/DPI 解耦，但 UI 元素在极端长宽比下仍可能重排。

**2026-11-24 03:40 验证结果**

| 项目 | 结果 |
|---|---|
| 前端构建 | `tsc -b && vite build` 通过；仅有 chunk-size 提示 |
| Rust 构建 | `Finished release` 03:35:47；紧随其后的 `reg.exe` 仍被安全策略拦截，但不影响产物 |
| 部署 | 旧 exe 备份为 `app/aogara.pre-perf.exe`；新 exe 03:35:56 部署 |
| 错峰启动 T1 | 63503–63507 首次响应为 1.93 / 3.40 / 4.87 / 6.22 / 7.66 秒，相邻间隔 1.35–1.47 秒 |
| 取消排队 T3 | 第一项已启动后点取消，最终只有 63503/63504 在线，剩余三项未启动；第二项是在取消前 1.2s 已发起 |
| 性能监控 T4 | `perf-monitor.ps1` 8 秒试运行正常，输出每核、进程 CPU、GPU Engine；能识别 ≥80% 热核 |
| 采样降频 T5 | `test-mode.smoke.toml` 使用 `monitor_interval_ms=1000` 跑通；`run-20261124-033909` 关键事件完整、`state=finished` |
| GPU 偏好 T6 | 备份 `tools/UserGpuPreferences.backup-20261124-032935.json`；Aogara/Edge 均为 `GpuPreference=2;`；游戏页 renderer = NVIDIA RTX 4060 |
| 生产模式 T7 | 默认配置启动后 63503–63507 全关；03:39:38 后无新 `run-*` 测试目录 |
| 未执行 | 第 4 项（关闭无关浏览器）按用户要求不做；`startAll` 真实开局完整轮次未跑，避免留下活局，复用同一 `runStaggered()` 已由 T1/T3 覆盖 |

### 20.3 系统占用最低化（P0 + P1 第一批）

源码热点：`lib.rs` 每会话启动一整套后台任务且各自 `spawn_pool_driver`（5s）；`InspectorWriter` 每条 WS 帧同步序列化并双写两个文件；`analysis/runner.rs` 每个事件全量分析；前端 `Sessions/Overview/GameDashboard` 均 2s 轮询；浏览器带 occlusion 保活参数，空闲不释放。

已落实：

| 项 | 改动 | 文件 |
|---|---|---|
| 配置降载 | 删除 `--disable-backgrounding-occluded-windows` / `--disable-features=CalculateNativeWinOcclusion`；`capture.http.bodies=false`；新增 `[multi] close_idle_browser_secs=120`；新增 `[logging.inspector] mode="lite" flush_ms=500 raw_ws=false http_bodies=false` | `configs/*.toml` |
| 配置结构 | `MultiConfig.close_idle_browser_secs`（0=关闭）；`InspectorConfig/InspectorMode` | `config/multi.rs`、`config/logging.rs`、`config/mod.rs` |
| Inspector 分级 | `InspectorWriter` 改 `BufWriter` 定时 flush；lite 仅保留 mjai/bot/有注解 HTTP；`raw_ws/http_bodies` 默认关；`off` 完全不写不广播 | `inspector/mod.rs`、`logger/session.rs`、`logger/mod.rs` |
| 全局 driver | 每会话 5s driver → 单全局 2s driver；`PoolDriverState` 按 slot 保存 finish/timer/last_job/idle 状态；避免五份 config clone | `ipc/commands.rs`、`lib.rs` |
| 空闲关浏览器 | 有 finish 记录、非对局、无 autoplay、auto_next 关、浏览器 running 且空闲超过阈值时按 profile 强退；首次登录/尚未进过局的浏览器不会被误关 | `ipc/commands.rs` |
| 前端降轮询 | `get_config` 不再随列表轮询；`list_sessions` 5s 兜底；隐藏页面暂停；后端 `session-status` 事件驱动即时刷新 | `ipc/mod.rs`、`frontend/src/routes/{Sessions,Overview,GameDashboard}.tsx` |

**2026-11-24 04:28 实测结果**

| 项目 | 结果 |
|---|---|
| 配置加载 | `get_config` 实测：`close_idle_browser_secs=120`、`inspector.mode=lite/flush_ms=500/raw_ws=false/http_bodies=false`、`bodies=false`、`extra_args` 仅剩调试端口+窗口尺寸 |
| 构建 | 前端 `tsc -b && vite build` 通过；Rust `Finished release`；末尾 `reg.exe` 仍被安全策略拦截，不影响产物 |
| 部署 | 旧 exe 备份 `app/aogara.pre-lowload.exe`；最终 exe 04:26:55 部署；生产实例 04:28:38 启动 |
| 去除 occlusion 参数 | `pool_start dryRun=true` 实测停在铜之间模式列表，未误入教程/活动/客服/商店；截图确认 |
| 首次开浏览器 | 空闲阈值为 3s 的临时配置下，slot1 浏览器连续 12s 在线并完成登录，证明“未进过局不会被误关”修正生效 |
| Inspector lite | 真局中 `inspector.jsonl` 362 行：mjai 302 / bot 44 / http 16 / 注解 32；无原始 `ws_frame`。发现 shareable/full 共用 flush 时间戳导致 full 文件短暂滞后，已拆成两个独立 `last_flush` 并重编译部署 |
| 空闲关浏览器 | 实局 12 分钟未自然结束，未能完成“finish 后自动关闭”的端到端验证；逻辑已加 `finish_count>0` 门禁，避免登录/首局阶段误关 |
| 续打链清理 | 中断测试产生的 `logs/slot1/20261124-{040101,040736,042703,042747}` 移至 `logs/_aborted/`；最终生产启动后 63503–63507 全关，无新 test run |
| 未做 | 五开空闲/对局的 perf-monitor 前后对比尚未跑；P2 shared-browser 与 DPR 实验未做 |

最终结论：P0 + P1 第一批已落地并部署；配置降载、点击稳定性、首次登录保护、Inspector lite、生产启动洁净度已验证。剩余需要自然打完一局后确认 idle-close 端到端，并用五开 perf-monitor 量化收益。

### 20.4 一键多开顶号修复（账号独立化）

**根因（2026-11-24 只读实测坐实）**：截图「您的帳號已在另一處登入」= 同一 UID 在两个 profile 同时在线。

| slot | 期望账号 | 实测 `localStorage.account` | 实测 `webgl_crash_user_v1`(UID) |
|---|---|---|---|
| slot1 (p-accountA) | accountA | **null** | **<UID>**（= accountB 的 UID，被污染） |
| slot2 (p-main) | main | FKU!!! | <UID> ✓ |
| slot3 (p-accountA) | accountA | user@example.com | <UID> |
| slot4 (p-alt3) | alt3 | user@example.com | <UID> ✓ |
| slot5 (p-accountB) | accountB | user@example.com | **<UID>** |

slot1 与 slot5 实际是同一 UID <UID> → 互顶。旧 `switch_account()` 的兜底
`account==null && token_present → 跳过换号` 让被污染的 slot1 一直用错误身份运行；
`logout()` 又在 `account==null` 时误判"已登出"直接返回，留下旧 token。

**关键判据**：`localStorage.webgl_crash_user_v1`（数字 UID）比 `account` 稳定，
后者在新客户端登录后可能为 null。UID 与 email 交叉验证得到：
accountA=<UID>、accountB=<UID>、main=<UID>、alt3=<UID>。

已落实（P0 门禁 + P1 判据 + P2 并发保护）：

| 项 | 改动 | 文件 |
|---|---|---|
| 可靠身份判据 | 新增 `Identity`/`probe_identity`（uid 权威 + account 次要）；`identity_is_target` 两边都有 uid 时只信 uid；`LoginState.uid` | `pool/mod.rs` |
| 删除致命兜底 | `switch_account` 去掉 `account=null→跳过换号`，改为身份不符就强制 logout/login；登录后 uid 校验 | `pool/mod.rs` |
| 强制清理 | `force_clear_login_storage` 清 account/access_token/uid/带 UID 后缀键 + reload；`logout` 在 token 残留但无 account 时调用 | `pool/mod.rs` |
| 顶号弹窗自愈 | `switch_account` 开头 `probe_ui_screen==AccountConflict` → 强制清理重登（不点弹窗固定坐标） | `pool/mod.rs` |
| 登录判据升级 | `wait_login(want)` 改用 `probe_identity`+`identity_is_target` | `pool/mod.rs` |
| 启动前门禁 | 新增 `precheck_sessions` 命令：所有会话目标账号身份去重，重复则前端阻止批量启动 | `ipc/commands.rs`、`Sessions.tsx` |
| 账号互斥锁 | `AppState.account_locks`（全局 account→slot）；`pool_start_inner` 用 RAII `AccountLockGuard` 持有整个启动序列，同一身份并发启动 fast-fail | `ipc/state.rs`、`ipc/commands.rs` |
| 配置修正 | accountA/accountB 补真实 uid；被误写成密码的 `label="<登录名占位>"` 改回 `accountA` | `configs/*.toml` |

**2026-11-24 验证结果**

| 项目 | 结果 |
|---|---|
| 构建 | 前端 `tsc -b && vite build` 通过；Rust `Finished release in 7m27s`；末尾 `reg.exe` 仍被安全策略拦截不影响产物 |
| 部署 | 旧 exe 备份 `app/aogara.pre-antitop.exe`；新 exe 12:49:19 部署；生产实例 13:02:14 启动（PID 11216） |
| precheck | 5 个账号身份全唯一（`uid:<UID>/<UID>/<UID>/<UID>` + `mail:user@example.com`），`ok=true, conflicts=[]` |
| 污染识别 | 实测 slot1 被污染态 `uid=<UID> account=null`：新逻辑日志 `observed uid=Some("<UID>") account=None` 正确识别，旧逻辑此处会跳过换号 |
| 强制清理 | 日志 `force-clearing stale login`；slot1 localStorage 从污染态变为 `account=null uid=null token=false`，错误身份已清除 |
| 幂等性 | slot2(main) dryRun：`identity verified for main (uid=Some("<UID>"))` 跳过换号，正常会话不被误伤 |
| 账号锁 | 并发对 main 发起 slot2+slot1：slot2 获锁，slot1 被拒 `账号正在被 slot2 启动（同一账号不能同时登录两个会话，会顶号）` |
| 已知限制（**已过期，见下方更新**） | accountA 账号登录反复 `等待登录态超时`——2026-11-24 14:37 确认当时该账号未在雀魂注册（错误码 1002「帳號不存在」），非顶号/自动登录逻辑问题，UID 未知故用 email 身份降级。**更新 18:10：用户已补注册，实测登录成功，真实 UID=<UID>，config.toml 已回填、precheck 用 UID 判重；此限制已解除** |
| 未做 | 五账号一键全开后跑完整真实对局的长时间稳定性观察；外部设备登录同号触发弹窗自愈的真机验证 |

最终结论：顶号三根因（判据失真 / 跳过换号 / 并发无锁）已全部修复并实测生效；
污染 profile 会被自动识别并强制重登，重复账号在启动前和启动时双重拦截。
生产版本已部署，五个游戏浏览器 profile 各自独立。

### 20.5 关闭浏览器释放机器人 + 一键全开实测

**问题**：截图显示 slot 浏览器「未打开」，但「停止对局」按钮仍在、`autoplay=true`，
机器人/自动打牌占用没释放。根因：`session_stop_capture()` 只关浏览器，从不碰
`autoplay.enabled`；对局被强退时也不会产生 `EndGame`，bot runner 无法自行释放。

**修复**（`ipc/commands.rs::session_stop_capture`）：
- 增加 `app: AppHandle` 参数；
- 关浏览器成功后，若该会话 `autoplay.enabled=true`，走 `update_config_inner`
  关掉 autoplay（幂等、持久化、同步 autoplay manager）；
- **关键坑**：必须用目标会话的 `state.with_session(s)` 而非 Tauri 全局 state
  （那是 slot1）。首版误用全局 state，读到的永远是 slot1 的 autoplay，
  对 slot3/4/5 静默不释放——已修正并复测。

**2026-11-24 13:32 实测**

| 项目 | 结果 |
|---|---|
| 关浏览器释放 | slot3 BEFORE `autoplay=true, browser=running` → AFTER `autoplay=false, browser=stopped`；日志 `browser closed for slot3; autoplay and bot released` |
| 构建 | Rust `Finished release`；`reg.exe` 仍被安全策略拦截不影响产物 |
| 部署 | 备份 `app/aogara.pre-botrelease.exe`；修正版 exe 13:27:40 部署；生产实例 13:32:24 启动（PID 30380） |
| 重启+一键全开 | 从零重启 aogara → 一键打开浏览器：5 个 8.4s 内全开（错峰正常） |
| 五账号登录 | UID 各不相同：`main=<UID>`、`accountA=<UID>`、`alt3=<UID>`、`accountB=<UID>`，**无顶号**（日志顶号计数 0）；slot3 `account=FKU!!!` 但 UID 正确，uid 判据有效识别 |
| slot1 异常（**已解除**） | 当时 accountA 未登录（`account=null uid=null`）系账号未注册（见 §20.6 历史诊断）；**18:10 补测：账号已注册并登录成功（UID <UID>），此异常不再存在** |
| 进对局 | 未成功：客户端卡在「正在初始化游戏资源 36%」（资源下载/初始化极慢，非逻辑问题），无账号进入对局；按用户「不等待直接强退」指示强退五个浏览器 |
| 环境限制 | 本机 majsoul 客户端资源初始化慢，多次 reload 会重新下载；一键全开后进房点击无处下手，需在网络/磁盘条件好时重测进对局环节 |

### 20.6 accountA 无法登录诊断（2026-11-24 14:37 历史结论：账号未注册）

> **现状更新（2026-11-24 18:10）**：用户已补注册该账号。实测 slot1 单独登录后进入大厅
> （昵称「玩家A」），程序日志 `identity verified for accountA (uid=Some("<UID>"))`，
> 真实 UID=**<UID>**；`config.toml` 中该账号空 `uid=""` 已回填，`precheck` 5 账号现全部用 UID 判重、
> 无冲突。下面保留当时的诊断证据链作为历史记录，**结论已不再适用**。

**现象**：slot1（profile `p-accountA`，目标账号 accountA）反复登录失败，
`等待登录态超时（目标 user@example.com）`。用户怀疑"打开浏览器自动登录"逻辑失效。

**排查结论**：自动登录逻辑**正常执行**，失败在服务端账号验证。

- 截图铁证：登录页邮箱 `user@example.com` + 密码已自动填入并提交，但页面弹出
  「**錯誤碼 1002：帳號不存在，請檢查帳號和信箱網名的字母大小寫，如需協助請聯絡遊戲客服 cs@maj-soul.com**」；
- 证据链：`13:37:35` 自动登录开始（`switching accountA`）→ 服务端 1002 拒绝 →
  客户端 localStorage 始终无 token → `13:39:16` 第一次 90s 超时 → `13:39:19` 第二次重试；
- 用户确认：**该账号尚未在雀魂注册**（用户本人操作失误，非程序问题）。

**历史误导的根源**：旧代码有 `account key absent but token valid → skipping re-login` 兜底，
profile 里残留着他人 token（accountB 的 UID <UID>），旧逻辑见 token 就跳过换号，
所以 accountA **从未真正登录过**；token 其实是错号的。上一轮删除该兜底后首次真登录，
服务端立刻回"账号不存在"。

**配置现状**（账号未注册，故 uid 为空、用 email 作身份降级）：

```toml
id = "accountA"
uid = ""                       # 从未成功登录，拿不到数字 UID
email = "user@example.com"
password = "<登录名占位>"
```

**处置（历史）**：非代码问题，无需修改程序。待用户注册该账号后，用真实邮箱/密码更新配置，
首次成功登录会自动写入 `webgl_crash_user_v1`（UID）；届时可补 `uid` 让身份判据升级为 uid 权威。
当前 slot1 不参与多开对局，不影响其余四账号。
**已完成（18:10）**：用户已注册，UID <UID> 已回填，slot1 恢复参与多开；五开实测 3/5 进房成功。

**排错经验**：`等待登录态超时` 不等于"自动登录没跑"——先看游戏页截图有无服务端错误弹窗
（1002 账号不存在 / 密码错误等），再看 localStorage token。表单已填 = 逻辑已执行，问题在凭据。

> **后续扩展引子：账号自助注册功能**
>
> 本次诊断暴露了一个能力缺口：号池账号是人工预置的，账号没注册时程序只能在登录页
> 卡 90s 超时，用户需要手动去官网注册、再把邮箱/密码填回 `config.toml`。
> 计划后续新增「账号自助注册」功能：在号池页提供入口，自动完成雀魂注册流程
> （邮箱验证/验证码需设计人工介入点），注册成功后把 email/password 回写号池，
> 并自动登录一次抓取 `webgl_crash_user_v1` 作为 uid 固化。届时可移除本节的
> "账号未注册"人工处置步骤。相关设计见后续 §20.7（待实现）。

## 21. 文档索引与归档策略

### 21.1 当前有效文档

| 文档 | 角色 |
|---|---|
| `docs/Aogara-技术文档-主文档.md` | **当前主文档（本文件）**：架构、模块、接口、流程、多账号并行、排错 |
| `docs/README.md` | 文档索引：主文档 / 归档边界 |
| `docs/个人产物化改造方案.md` | 从上游 Akagi 派生为个人产物的品牌/法律/发布改造方案（P0/P1 已完成，更新器待定） |
| `docs/Aogara-全量改名清单.md` | Akagi→Aogara 全量改名的执行依据与保留项说明（2026-11-24 已完成） |
| `docs/Aogara-全量测试方案.md` | 全量测试方案 + 执行结果：三轮分工、测试矩阵、简单修复判定、问题记录表 P-01~P-16（已执行） |
| `docs/Aogara-问题修复方案.md` | **问题修复作业书 v2**：P-01~P-16 的代码级修复方案、精修轮执行结果、踩坑清单、排查工具箱 |
| `docs/代码逻辑优化方案.md` | 代码去重/复用/精简方案（未执行） |
| `使用手册.md` | 部署与日常操作手册（与主文档配套） |
| `app/configs/config.toml` | 运行配置的唯一事实源 |
| `docs/NOTICE` | 第三方许可证声明 |

### 21.2 历史归档（只读追溯）

位于 `docs/archive/2026-11-22/`：

| 文档 | 历史价值 |
|---|---|
| `Aogara-源码理解笔记.md` | 初次源码通读笔记；部分结论已被后续实测修正 |
| `bridge整合进原生exe-方案.md` | bridge → 原生整合的全过程与踩坑记录 |
| `号池功能-原生整合方案.md` | 原生号池的设计与阶段验收 |
| `号池功能方案.md` | 外部 bridge 路线的设计稿（已废弃） |
| `官方README-中文.md` / `官方README-English.md` | 官方 v3.8.0 文档快照 |
| `Aogara-Menu.tsx` / `Aogara-Sidebar.tsx` | 早期侧栏改造参考代码 |
| `tools-README.md` / `tools-CANVAS-CLICK.md` | bridge 工具集与 Canvas 点击方案（bridge 已弃用） |

位于 `docs/archive/2026-11-23/`：

| 文档 | 历史价值 |
|---|---|
| `多账号并行-重构方案.md` | 多账号并行的设计稿与实施记录（P1 抽取、动态会话、连打全局开关、页面重排、bus 隔离修复）；全部内容已并入本主文档 §13.15 / §14 / §18.5.1，仅作过程追溯 |

### 21.3 归档规则

1. 新文档先进 `docs/`，在 `docs/README.md` 登记角色。
2. 被取代的文档移入 `docs/archive/<日期>/`，保留原文，不改写历史结论。
3. 归档文档顶部加一行「历史归档，结论可能已过时」提示。
4. 任何与源码冲突的说法，以源码为准；本文档优先于归档文档。

---

*本文档由源码逐文件核对 + 真机日志验证生成。若发现与代码不一致，先改文档，再改代码。*
