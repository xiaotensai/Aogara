# Aogara 跨设备干净分发与 GitHub 开源方案

> 状态：**本轮只出方案，不执行**。未创建 git 仓库、未清理任何文件、未上传任何内容。
> 目标：把当前这台机器上的 Aogara 拆成「公开可分发部分」与「本地私有部分」，
> 使其他设备下载后能完整运行，且**账号、登录态、对局历史、本机设置一概不外泄**。
> 基准盘点时间：2026-11-24 19:20。基准目录：`<安装目录>`。
>
> **两项决策已拍板（2026-11-24 19:30，用户确认）**：
> ① **增强首次运行向导**——用户可在向导里直接填账号，程序自动建 profile 并写回配置（取代"模板手填"）；
> ② **Release zip 打包 bot 运行时**（+约 210 MB）——解压即用，不要求用户另行下载。
> 本文已按此更新 §5.2/§5.3/§5.4/§5.5/§六/§八/§九。
>
> **实施进度（2026-11-24 20:05）**：第 2 批「增强首次运行向导」**已实现并编译部署**——
> 新增 `setup_add_account` IPC + 前端 `AccountStep`（WizardAccount 多账号）+ 四语 `setup.account.*`，
> 核心逻辑单测 `setup_wizard_add_account_logic` 通过。V14 的 GUI 端到端部分因本机 CDP 调试口
> 无法 bind 未能实测（环境问题，非代码），详见 `docs/开源包预演验证报告.md` 更新节。
> exe 已更新，H 盘预演 zip 已重打包（含新 exe）。

---

## 一、结论先行

**可行，且不需要重写程序。** 程序本身已经内置了"干净首次运行"所需的三件关键机制：

| 已有机制 | 源码位置 | 对分发的意义 |
|---|---|---|
| 无配置时自动写默认配置 | `src/config/mod.rs::write_default_config` | 分发包**不含** `config.toml`，首次启动自动生成 |
| `general.first_run_completed=false` 触发首次运行向导 | `src/config/general.rs`、`config/mod.rs::migrate_first_run_marker` | 新用户第一次打开有引导，不是直接面对空配置 |
| CA 证书自动生成 | `src/proxy/ca.rs::generate_ca` | 每台设备首次运行生成**自己的**证书，无需分发私钥 |
| 用户配置目录优先于 exe 目录 | `config/mod.rs::resolve_config_path_inner` | 配置/数据可以放在用户目录，与程序目录解耦 |
| `user_data_dir=""` → 可移植的 `./chrome-profile` | `capture/chromium/profile.rs::resolve_profile_dir` | 模板里留空即可，不硬编码 `<安装目录>` |

因此整体策略是：**不改造程序主体，只做「数据分离 + 模板替换 + 文档脱敏 + 打包脚本」**。
唯一需要小改的程序点是"首次运行向导是否支持填账号"——见 §5.3，属可选增强。

---

## 二、现状盘点：哪些数据必须留在本地

### 2.1 敏感数据清单（禁止进入仓库与分发包）

| # | 位置 | 大小 | 内容 | 风险 |
|---|---|---|---|---|
| S1 | `app/configs/config.toml` + `slot-*.toml` | 106 KB | **5 个账号的 email / password / uid** | 凭据直接泄露 |
| S2 | `profiles/p-*/`（5 个目录） | **4.2 GB** | 浏览器 Cookie、登录态、浏览记录 | 可被直接复用登录 |
| S3 | `tools/accounts.json` | 5 KB | 账号昵称（如「玩家A」）、`<安装目录>\profiles\*` 绝对路径、坐标 | 身份 + 路径暴露 |
| S4 | `tools/credentials/*.key`（3 个） | 2 KB | 密钥文件 | 直接泄露 |
| S5 | `backups/aogara-ca-old/aogara-ca.pem` | — | **CA 私钥** | 可伪造 HTTPS 证书，最高危 |
| S6 | `app/logs/` + `app/history/` + `app/runtime/` | 78 M + 1.9 M + 210 M | 对局日志、战绩、下载的 python/uv | 隐私 + 体积 |
| S7 | `app/*.log`（8 个 stdout/stderr） | ~1.3 M | 含账号邮箱、UID、进房记录 | 身份暴露 |
| S8 | `app/aogara.pre-*.exe`（8 个） | ~400 M | 历史版本 exe | 体积 + 混淆 |
| S9 | `tools/shots/` | — | 截图含游戏昵称、ID | 身份暴露 |
| S10 | `tools/*backup*.json`、`fix-*.json`、`sess.tmp.json` 等 | — | 本机 GPU/注册表/会话调试数据 | 本机指纹 |
| S11 | `docs/` + `使用手册.md` + `README.md` | — | **72 处**个人信息（邮箱、UID、昵称、`%USERPROFILE%`、`<安装目录>`） | 公开仓库直接泄露 |

> 注意：S11 最容易被忽略——代码清理干净但文档带着真实邮箱上传，是开源最常见的翻车点。

### 2.2 可公开分发部分

| 类别 | 路径 | 说明 |
|---|---|---|
| Rust 源码 | `_setup/src/Aogara-3/src/`、`Cargo.toml`、`Cargo.lock`、`build.rs` | 主体逻辑 |
| 前端源码 | `_setup/src/Aogara-3/frontend/src/`、`index.html`、`package.json` | UI |
| bot 定义 | `mjai_bot/`、`native_bot/`（排除 `settings.toml`、`out/`、`.venv/`） | 自动打牌逻辑 |
| 资源 | `icons/`、`assets/`、`capabilities/`、`tauri.conf.json`、`minisign.pub` | 打包资源与公钥 |
| 构建/打包脚本 | `scripts/fetch-runtime.sh`、`scripts/package-zip.sh` | 上游已有 |
| 法律文件 | `LICENSE.txt`、`NOTICE` | **必须保留**，Apache-2.0 要求 |
| 文档（脱敏后） | `docs/`、`使用手册.md`、`README.md` | 需按 §6 处理 |
| 配置模板（新建） | `config.example.toml` | 见 §5.1 |
| 忽略规则（新建） | `.gitignore` | 见 §4 |

---

## 三、三层分离模型

```
┌─────────────────────────────────────────────────────────────┐
│ 第 1 层：GitHub 公开仓库（源码）                            │
│   源码 + 脱敏文档 + 配置模板 + 构建脚本 + LICENSE/NOTICE    │
│   ❌ 无 config.toml / profiles / logs / 任何凭据            │
└──────────────────────────┬──────────────────────────────────┘
                           │ CI 或本地构建
┌──────────────────────────▼──────────────────────────────────┐
│ 第 2 层：Release 分发包（zip，公开下载）                    │
│   aogara.exe + bot 运行时 + 空 configs/ 模板 + 使用说明     │
│   ❌ 无账号 / 无登录态 / 无历史 / 无本机路径                │
└──────────────────────────┬──────────────────────────────────┘
                           │ 用户下载解压首次运行
┌──────────────────────────▼──────────────────────────────────┐
│ 第 3 层：设备本地私有数据（永不上传）                       │
│   configs/config.toml（用户自己填）+ profiles/ + logs/      │
│   + history/ + ca/（每台设备自生成）+ credentials/          │
└─────────────────────────────────────────────────────────────┘
```

**铁律**：第 1、2 层里不允许出现任何第 3 层的内容；第 3 层数据只在用户自己的设备上产生。

---

## 四、仓库建设方案

### 4.1 关键决策：不要在原目录直接 `git init`

`<安装目录>` 有 **15 GB+**（其中 profiles 4.2 G、_setup 11 G），且敏感文件混杂。
一旦 `git add -A` 误加凭据/私钥，即使删除也会留在 git 历史里，**清洗历史非常麻烦**（需要 `git filter-repo` + force push + 通知所有协作者 + 立刻改密）。

推荐做法：**建一个干净的发布目录，用白名单复制，只提交白名单内容。**

```
F:\aogara-release\          ← 新建的干净仓库目录（本方案不执行创建）
├── .git/
├── .gitignore
├── LICENSE.txt             ← 从 _setup/src/Aogara-3/LICENSE.txt 复制
├── NOTICE                  ← 从 _setup/src/Aogara-3/NOTICE 复制
├── README.md               ← 重写为开源版（§6.3）
├── docs/                   ← 脱敏后的文档（§6）
├── config.example.toml     ← 模板（§5.1）
├── src/  frontend/  ...    ← 从 _setup/src/Aogara-3/ 复制源码
└── scripts/                ← 构建/打包脚本
```

这样做的额外好处：仓库体积从 15 GB 降到约 20–50 MB。

> 备选方案：若坚持在原目录 `git init`，必须先写全 `.gitignore`、再逐个 `git add` 明确路径，
> 并在首次提交前用 `git status --ignored` 与 `git ls-files` 双重确认。不推荐。

### 4.2 `.gitignore`（建议内容，融合上游规则 + Aogara 特有）

上游 `_setup/src/Aogara-3/.gitignore` 已经覆盖 `target/`、`node_modules/`、`ca/`、`logs/`、
`runtime/*`、`secrets/`、`gen/`、`dist/` 等，可直接沿用。需在**仓库根目录**额外补充：

```gitignore
# ---- 构建产物 ----
target/
node_modules/
dist/
gen/
*.exe
*.msi
*.zip

# ---- 运行时私有数据（Aogara 关键）----
config.toml                 # 真配置含账号密码，绝不入库
configs/
!configs/.gitkeep
config.*.toml
profiles/
logs/
history/
runtime/
ca/
credentials/
*.key
*.pem                       # 含 CA 私钥
!minisign.pub               # 公钥可以提交

# ---- 本机调试产物 ----
tools/shots/
tools/*.log
tools/*backup*.json
tools/fix-*.json
tools/*-start*.json
tools/sess.tmp.json
tools/accounts.json         # 含账号昵称与绝对路径
*.log
*.stdout
*.stderr

# ---- 文档附件 ----
docs/archive/               # 上游原文归档，版权归属上游，不随仓库分发

# ---- 编辑器/系统 ----
.vscode/
.idea/
Thumbs.db
Desktop.ini
```

### 4.3 提交前强制检查（每次提交都跑）

```bash
# 1. 看将要提交的文件清单（必须全是白名单）
git add -A && git status --short

# 2. 搜索敏感串（应为空）
git diff --cached | grep -inE 'password\s*=|<邮箱域名>|小萧|<本机目录号>|<安装目录>|BEGIN .*PRIVATE KEY'

# 3. 确认没有大文件
git diff --cached --stat | tail -5

# 4. 有敏感串则立刻 git reset，处理后再提交
```

有条件的建议装 `gitleaks` 做一次全历史扫描（开源免费，不涉及付费资源）。

---

## 五、让其他设备"完整运行"的关键设计

### 5.1 配置模板 `config.example.toml`（新建，脱敏）

> 说明：已选「增强向导」，账号改由向导填写，模板只作为**兜底/参考**存在，
> 普通用户不需要编辑它。下表仍按脱敏规则处理，供想手动配置的进阶用户与向导默认值的来源。

从当前 `config.toml` 复制后按以下规则处理：

| 字段 | 当前值 | 模板值 | 理由 |
|---|---|---|---|
| `[pool.accounts]` 整段 | 5 个真实账号 | **全部删除或改为一个空示例** | 账号由用户自己填 |
| `email` / `password` / `uid` | 真实值 | `""` | 不留任何个人信息 |
| `capture.chromium.user_data_dir` | `<安装目录>/profiles/p-*` | **`""`** | 留空会自动用可移植的 `./chrome-profile` |
| `logging.dir` / `history.dir` | `./logs/...` | 保持相对路径 | 可移植 |
| `capture.proxy.ca_dir` | `./ca` | 保持相对路径 | 首次运行自动生成证书 |
| `[general] first_run_completed` | `true` | **`false`** | 触发新用户向导 |
| 账号显示名 `label` | 「玩家A」等 | `"账号1"` | 脱敏 |
| bot 坐标 `coords` | 已标定的真实坐标 | 可保留（账号无关，是 UI 坐标） | 保留能开箱即用 |

> 关键：`resolve_profile_dir("")` → `resolve_dir("./chrome-profile")`，源码注释明确
> "a portable zip keeps everything (config, logs, profile) in one folder"。
> 所以**模板里 profile 路径必须留空**，这是跨设备可移植的核心开关。

### 5.2 首次运行流程（目标行为 · 已按"增强向导"定稿）

其他设备上的用户体验：

1. 下载 Release zip → 解压到**任意目录/任意盘符**（运行时已打包，无需额外下载）
2. 双击 `aogara.exe`
3. 程序发现无 `configs/config.toml` → 自动写默认配置（`write_default_config`）
4. `first_run_completed=false` → 路由跳到 `/setup`（`frontend/src/main.tsx` 现有机制）
5. 向导 6 步：`welcome → platform → mode → config → configure → finish`
   （现有 `Setup.tsx` 的 `STEPS`，`bots` 步已注释掉）
6. **新增「账号」步骤**（见 §5.3）：用户填自己的邮箱/密码 → 程序建 profile 并写回配置
7. 点「完成」→ `update_config` 写回、`first_run_completed=true` → 进入主界面
8. 首次抓包时自动生成该设备自己的 CA 证书（`generate_ca`），用户按提示安装信任

### 5.3 账号接入：已选「增强首次运行向导」

> **决策（2026-11-24 19:30）**：采用 B 方案，放弃"模板手填"。
> 代价是需改前端 + Rust 并重编译；换来的是普通用户开箱即用。

**要改的源码位置（基于实际盘点）：**

| 位置 | 现状 | 改动 |
|---|---|---|
| `frontend/src/routes/Setup.tsx` | `Step` 联合类型、`STEPS` 数组（6 步）、`step` 状态、`finish()` 里 `invoke('update_config')` | 新增 `'account'` 步骤类型，插入 `STEPS`（建议放在 `config` 与 `configure` 之间）；新增 `AccountStep` 组件渲染邮箱/密码/显示名输入 |
| `frontend/src/i18n/resources/*.json`（4 语） | 向导各步已有 `setup.*` 键 | 新增 `setup.account.*` 一组键（标题、邮箱、密码、显示名、跳过、说明"账号仅存本机"） |
| `src/ipc/commands.rs` | 已有 `create_session`（1070 行）、`update_config` | 新增 `setup_add_account` 命令：接收账号信息 → 校验 → 建 profile → 追加 `[[pool.accounts]]` → 返回会话信息；复用 `create_session` 的 profile 创建逻辑，不重写 |
| `src/config/multi.rs` | `MultiSession` 结构、`sessions: Vec` | 向导加账号时同步往 `multi.sessions` 追加一项（多开用户可继续加第 2~5 个） |

**关键约束：profile 路径必须由程序拼成绝对路径**（源码 `resolve_profile_dir` 要求非空值必须 absolute）：
- 向导在运行时用 `std::env::current_exe()` 的父目录拼出 `<程序目录>/profiles/account-N`；
- 禁止让用户输入该路径，也禁止写死 `<安装目录>`；
- 用户若勾选"跳过，以后再填"，则 `user_data_dir` 保持 `""`（走可移植的 `./chrome-profile`），
  配置里 `[[pool.accounts]]` 为空数组，进入主界面后可从「设置」再添加。

**安全要求：**
- 密码只在向导里输入一次，写进本机 `config.toml`；**不上传、不写日志、不进 any telemetry**；
- `AccountStep` 明示"账号信息仅保存在你自己的设备上，不会随程序分发"；
- 向导页不预填任何本机真实账号（公开版向导从空白开始）。

> 验证方式见 §八新增 V14（向导加账号全链路）。

### 5.4 多开 profiles 的处理（向导自动建）

- **不分发任何 profile 目录**；
- 单账号用户：向导加 1 个账号 → 程序生成 `<程序目录>/profiles/account-1` 并写绝对路径；
- 多开用户：在向导「账号」步点"再加一个"，重复添加 2~5 个账号，每个账号独立 profile；
  或首次运行后从「设置/多开会话」页继续添加（复用现有 `create_session`）；
- 向导**不预设 5 个空账号**——默认只建用户实际填写的数量，避免新用户看到 5 个空槽困惑；
- 账号名 `label` 由用户自填，程序默认给 `账号1`、`账号2`……不含任何本机真实昵称。

### 5.5 Bot 运行时依赖（已定：打包进 Release zip）

> **决策（2026-11-24 19:30）**：用户选择打包。代价是 zip 体积 +约 210 MB、
> 下载耗时长（本机实测 81.7 MB 约 27 分钟，210 MB 可能 1 小时以上）；
> 换来的是其他设备解压即用，无需懂 `fetch-runtime.sh`。

| 层 | 是否含运行时 | 说明 |
|---|---|---|
| GitHub 源码仓库 | **不含** | 仓库保持小、合规清晰；`.gitignore` 的 `runtime/` 规则不变 |
| Release zip | **含**（约 210 MB） | 解压即完整可运行包 |

**打包脚本改造（基于现有 `scripts/package-zip.sh`）：**

现有脚本已具备正确布局，只需改品牌名与补齐许可证：

```
dist/aogara-<version>-windows-x64.zip
└── aogara-<version>-windows-x64/
    ├── aogara.exe                    ← 从 target/<triple>/release/aogara.exe
    ├── runtime/python/<triple>/...   ← 本机 app/runtime/python 的内容
    ├── runtime/uv/<triple>/...       ← 本机 app/runtime/uv 的内容
    ├── LICENSE.txt  NOTICE           ← Apache-2.0 + 上游署名（必须）
    ├── THIRD_PARTY_NOTICES.txt       ← 新增：python/uv/mortal 的许可证文本
    ├── config.example.toml           ← §5.1 脱敏模板
    └── README.txt                    ← 简短启动说明，指向向导
```

改动点：
1. `PKG="akagi-..."` → `"aogara-..."`；`EXE=akagi.exe` → `aogara.exe`（脚本内 3 处）；
2. 确认 `PY_SRC`/`UV_SRC` 指向本机的 `app/runtime/python`、`app/runtime/uv`（当前脚本用仓库根下 `runtime/<triple>/`，
   与本机布局不同——**打包前需先同步到脚本期望路径，或改脚本读 `app/runtime/`**）；
3. 新增 `THIRD_PARTY_NOTICES.txt` 收集 python/uv/mortal 许可证（合规要求，见 §七第 5 条）；
4. **zip 内不含**：`config.toml`、`profiles/`、`logs/`、`history/`、`ca/`、`credentials/`、`tools/`——
   这是"干净程序"的最后一道闸，打包脚本里要显式排除，不能靠 `.gitignore`（`.gitignore` 不作用于 zip）。

### 5.6 更新器与外链的现状衔接

之前精修轮已做：更新检查关闭（`check_for_update` 返回 None）、4 个外链置空留引子。
这正好符合开源分发：
- 个人仓库地址确定后，把 `external.ts` 的 4 个常量填成自己的 GitHub / 社群地址；
- 若将来要恢复更新检查，把 `UPSTREAM_REPO` 改成自己的仓库，并注意版本号语义（当前 1.1.0，
  自己的 release 应从 1.x 递增，不要再与上游 3.8.0 比较）。

---

## 六、文档脱敏（72 处，上传前必做）

### 6.1 替换规则表

| 类型 | 原文示例 | 替换为 | 出现位置 |
|---|---|---|---|
| 邮箱 | `user@example.com` | `user1@example.com` | 主文档 §20 变更史、§13 号池表、账号表 |
| 昵称 | `玩家A`、`<登录名占位>`、`玩家A` | `玩家A` / `<你的昵称>` | 主文档 slot 表 |
| UID | `<UID>`、`<UID>` 等 | `<UID1>` 或 `12345678`（示例值） | 主文档 §20.5 诊断节 |
| Windows 用户目录 | `%USERPROFILE%\...` | `%USERPROFILE%\...` | 改名清单、使用手册 |
| 安装目录 | `<安装目录>\...` | `<安装目录>\...` | 主文档、使用手册、改名清单 |
| 本机 GPU/注册表备份 | 具体设备名、GPU 型号 | `<本机配置>` | 使用手册 |
| 截图附件 | `tools/shots/*.png` | 不入库 | 全部 |

### 6.2 历史诊断章节的处理原则

主文档 §20.4 / §20.5 / §20.6 记录了大量真实邮箱、UID、错误日志作为证据链。
这里有**双轨策略**：

- **本地工作版**（当前 `<安装目录>\docs\`）：保留原文 + "已过期"注记，继续作为排障证据；
- **公开仓库版**：整体改写为"脱敏案例"——把邮箱/UID/昵称替换为占位符，保留**机制与结论**（顶号判据、1002 错误、横幅遮挡），删掉具体凭据。

> 原则：**机制可以公开，身份不能公开。** 不改写历史结论，只替换身份字段。

### 6.3 README 重写要点

公开版 README 应包含：
- 项目定位："基于 Akagi（上游）修改的 Aogara，个人非官方分支"
- Apache-2.0 声明 + 保留上游 LICENSE/NOTICE 的说明
- 与其他设备运行的前置条件（Windows 10/11、WebView2、雀魂账号自行注册）
- 快速开始：下载 Release → 解压 → 双击 `aogara.exe` → **首次运行向导里填自己的账号** → 完成
  （`config.example.toml` 只作为进阶手动配置参考，普通用户无需编辑）
- 明确声明："本项目不含也不分发任何账号凭据；用户需自行注册并在向导里填写自己的账号，账号仅存本机"

---

## 七、法律与合规（不能省）

1. **保留** `LICENSE.txt`（Apache-2.0）与 `NOTICE`，不删除上游版权声明。
2. 在 README / NOTICE 显著位置说明"基于 Akagi 修改的非官方分支"——之前已做
   （i18n 四语 `sidebar.derivative`，本轮已修正为"基于 Akagi"）。
3. 上游商标/Logo 使用：当前 wordmark 已重画，保留 `mark` 图标与作者署名注释；
   若上游对 Logo 有商标主张，README 里加"Logo 及相关权利归原作者所有"。
4. **不要上传** `docs/archive/` 里的上游原文 README 全文归档——那是上游文档，版权归上游，
   只保留本地参考即可（这也符合 `.gitignore` 里的排除）。
5. bot 运行时（python/uv/mortal）各有自己的许可证，若打进 Release zip，需附带其许可证文本。

---

## 八、跨设备验证清单（发布前逐项确认）

| # | 验证项 | 通过标准 |
|---|---|---|
| V1 | 仓库 `git ls-files` | 无 `config.toml`、`*.key`、`*.pem`、`profiles/`、`logs/`、`accounts.json` |
| V2 | 敏感串扫描 | `git diff --cached \| grep` 无命中（邮箱/密码/昵称/本机路径/私钥头） |
| V3 | 仓库体积 | < 100 MB（排除运行时与 profile） |
| V4 | 干净设备启动 | 解压到新目录（如 `D:\test\`），双击 exe 能启动、出首次运行向导 |
| V5 | 自动生成配置 | 首次启动后生成 `configs/config.toml`，且 `first_run_completed=false` |
| V6 | 自动证书 | 首次抓包生成 `ca/` 下证书，非复制自本机 |
| V7 | profile 路径 | 向导建账号时生成的是 `<程序目录>/profiles/account-N` 绝对路径，无 `<安装目录>` 残留；跳过账号时 `user_data_dir=""` 走 `./chrome-profile` |
| V8 | 账号隔离 | 新设备在向导填自己的账号，能登录进房；本机账号在该设备不可见 |
| V9 | 多开可用 | 向导里加 2 个账号 → 生成 2 个独立 profile，进房不顶号；账号数>1 时 `multi.sessions` 同步追加 |
| V10 | 文档无个人信息 | 公开版文档 72 处已脱敏，`grep` 无真实邮箱/UID/昵称 |
| V11 | 更新器静默 | 启动日志 github 请求 = 0 |
| V12 | 外链安全 | 未填个人 URL 时点击无动作，不打开死链 |
| V13 | 许可证齐全 | Release 包含 LICENSE.txt + NOTICE + 上游署名 + `THIRD_PARTY_NOTICES.txt` |
| V14 | **向导加账号全链路** | 干净 zip 首次运行 → 向导填测试账号 → 完成 → 配置写入且 `first_run_completed=true` → 重开不再弹向导 → 能打开浏览器登录 |
| V15 | **zip 干净性** | 解压 zip 后 `find` 确认无 `config.toml`/`profiles`/`logs`/`ca`/`*.key`/`*.pem`/`tools/accounts.json`；体积约 210 MB 且运行时目录齐全 |
| V16 | **打包运行时可用** | zip 解压到无 python/uv 的环境，bot 能正常加载运行（证明运行时确实被打包且路径正确） |

---

## 九、实施顺序（供下轮执行，本轮不执行）

| 批次 | 内容 | 是否改程序 | 风险 |
|---|---|---|---|
| 第 1 批 | 建干净仓库目录 + `.gitignore` + 白名单复制源码 | 否 | 低 |
| 第 2 批 | **增强首次运行向导**（§5.3：Setup.tsx 加账号步 + i18n 四语 + `setup_add_account` IPC + multi.sessions 同步），重编译，跑 V14 | **是**（需重编译） | **中高**（动向导主链路，回归要覆盖旧 6 步不回归） |
| 第 3 批 | 生成 `config.example.toml` 模板 + 重写 README（向导流程版） | 否 | 低 |
| 第 4 批 | 文档脱敏（72 处，双轨：本地保留 / 仓库脱敏） | 否 | 中（量大，需逐份核对） |
| 第 5 批 | 改造 `scripts/package-zip.sh`（§5.5：aogara 命名 + 运行时路径 + 第三方许可证 + 显式排除私有数据），打 zip，跑 V15/V16 | 否 | 中（210 MB，注意路径与排除） |
| 第 6 批 | 提交前检查 + 首次 git commit | 否 | **高**（一旦误提交需清洗历史） |
| 第 7 批 | 干净目录完整验证 V4–V16 | 否 | 中（耗时，含真实进房） |

> **顺序理由**：向导增强（第 2 批）必须先做——后续的 README、zip、验证都依赖"向导能加账号"这个前提。
> 第 6 批是最关键的一道闸：**提交前必须跑完 §4.3 的四步检查**。
> 宁可推迟提交，也不要带着凭据做第一次 commit。

---

## 十、风险提示

1. **Git 历史不可逆**：凭据一旦进入历史，删除文件无效。必须靠提交前检查拦截。
2. **GitHub 是公开爬虫靶场**：哪怕只有 1 个邮箱/UID，也可能被关联到真实身份。
3. **账号共享条款**：雀魂等游戏通常禁止账号共享/自动化，公开分发可能引来风险；
   建议在 README 加"仅供个人学习与本地自动化研究使用，账号风险自负"。
4. **本机路径残留**：`tools/` 下多个调试脚本硬编码 `<安装目录>`，这些脚本**不要进仓库**；
   若要进，先改成相对路径。
5. **CA 私钥最高危**：`backups/aogara-ca-old/aogara-ca.pem` 与 `tools/credentials/*.key`
   必须确认不进仓库，也不进任何截图/日志附件。
6. **更新器引子**：外链留空是暂时的，填个人 URL 前先确认地址可用，避免重蹈死链覆辙。
7. **向导改动回归风险（已选增强向导）**：`Setup.tsx` 是首次运行主链路，插步后必须回归验证旧 6 步
   （尤其 `finish()` 的 `update_config` 与 `first_run_completed=true` 置位）；改坏会导致所有新用户卡在向导。
8. **打包体积与下载体验（已选打包）**：zip 约 210 MB，下载可能 1 小时以上；README 提前告知用户，
   并提供"已有 python/uv 可跳过下载"的备选包（可选）。同时 zip 排除私有数据必须**显式**写，
   `.gitignore` 不作用于 zip 内容——这是最易漏的一条。
9. **向导里的密码处理**：密码落盘到本机 `config.toml`（明文，与现有机制一致），
   必须确保向导不把它写进日志、不进任何上报；账号仅存本机的说明要写在输入框旁。

---

## 十一、与现有文档的关系

- 本文是**分发/开源**主题，与 `个人产物化改造方案.md`（品牌改造）互补：
  前者解决"怎么安全地给别人"，后者解决"怎么变成自己的产物"。
- 品牌与法律合规结论沿用 `Aogara-全量改名清单.md` 与 `个人产物化改造方案.md` §3。
- 测试结果沿用 `Aogara-全量测试方案.md`、`Aogara-问题修复方案.md`；本方案不改变已验证的功能行为。
