# Aogara

基于 Akagi（上游，Apache-2.0）修改的非官方分支，主要面向雀魂（Majsoul）的多开与自动化辅助。

> 本项目是个人非官方分支，**不含也不分发任何账号凭据**。
> 账号、登录态、对局历史、证书私钥只在用户自己的设备上产生和保存。

## 功能

- 多会话并行（号池），账号彼此独立，避免顶号
- WebView2 CDP 抓包 + Inspector（lite/full）
- 自动打牌（bot）与软停
- 性能降载与错峰启动

## 前置条件

- Windows 10/11（含 WebView2 Runtime）
- 一个自行注册的雀魂账号（本项目不提供账号）

## 快速开始

1. 下载 Release zip（`aogara-<版本>-windows-x64.zip`），解压到任意目录（可放任意盘符）
2. 双击 `aogara.exe`
3. 首次运行会弹出向导：按提示选择平台、抓包模式等
4. 添加账号：填写你自己的邮箱与密码；程序会在 `<程序目录>/profiles/` 下创建独立数据目录
5. 点「完成」，进入主界面即可使用

> 账号信息仅保存在本机的 `configs/config.toml`，不会随程序分发，也不会上传。
> 进阶用户可参考 `config.example.toml` 手动配置（普通用户无需编辑）。

## 目录说明

| 路径 | 用途 | 是否含个人信息 |
|---|---|---|
| `aogara.exe` | 主程序 | 否 |
| `runtime/` | 打包的 python/uv 运行时 | 否 |
| `configs/` | 首次运行后生成，**本地保存账号** | 是（仅本机） |
| `profiles/` | 各账号独立浏览器数据 | 是（仅本机） |
| `ca/` | 首次抓包自动生成证书 | 是（仅本机） |
| `logs/`、`history/` | 运行与对局记录 | 是（仅本机） |
| `config.example.toml` | 脱敏配置参考模板 | 否 |

## 构建（开发者）

```bash
# 1. 准备运行时（python + uv）
bash scripts/fetch-runtime.sh x86_64-pc-windows-msvc

# 2. 编译
cargo tauri build --no-bundle --target x86_64-pc-windows-msvc

# 3. 打包 Release zip
bash scripts/package-zip.sh x86_64-pc-windows-msvc
```

## 法律与署名

- 本项目基于 Akagi 修改，遵循 Apache-2.0 许可证，保留上游 `LICENSE.txt` 与 `NOTICE`。
- 第三方组件（python/uv/mortal 等）的许可证见 `THIRD_PARTY_NOTICES.txt`。
- Logo 及相关权利归原作者所有。
- 仅供个人学习与本地自动化研究使用；账号与自动化风险自负。
