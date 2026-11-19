# 桌面旁白记录器（DesktopEasyNote）

常驻 Windows 托盘的极简记录器：白天在一页文档里记「干了啥 + 心情」，
次日 07:00 由 DeepSeek 用旁白口吻把昨天写成一段故事，静默导出为 Markdown。
记录时安静，收尾时「开口」。

## 功能

- 整页文档式输入（Typora 风格）：点空白处续写，回车落定并带时间戳
- `#` 标题（1–6 级），页头「＋ 标题」一键插入常用标题，可增删改
- 右键心情标签，可自定义、可隐藏，历史记录不受影响
- 无边框窗口、托盘常驻，关闭/最小化收进托盘
- 次日 07:00 自动生成前一天的旁白；错过则下次启动补跑
- 旁白风格与字数上限由提示词文档定义（见 `docs/旁白提示词-v3.0.md`）

## 技术栈

Tauri 2 + Rust + Vite + TypeScript（无 UI 框架）

## 安装使用（普通用户）

1. 打开 [Releases](https://github.com/Yshuinuan/DesktopEasyNote/releases) 页，下载最新版本的 zip
   （v0.1.0 为 `DesktopEasyNote_0.1.0_trial_2026-11-18.zip`）。
2. 解压后运行其中的 `*_x64-setup.exe`，按提示装到当前用户目录
   `%LOCALAPPDATA%\Programs\DesktopEasyNote`，不需要管理员权限。
3. 若出现「Windows 已保护你的电脑」（SmartScreen），点「更多信息」→「仍要运行」。
4. 首次启动会弹出设置窗口，填入**你自己的** DeepSeek API Key（在
   <https://platform.deepseek.com> 申请）并点「存入」。生成旁白会消耗你自己账号的 API 额度。

用法要点：

- 点文档末尾的空白处开始写；按回车这一行落定，自动带上时间。
- 行首输入 `#` 就是标题（1–6 级）；平时不显示 `#`，光标进入该行才显示。
- 页头「＋ 标题」一键插入常用标题（问题 / 思路 / 解决 / 复盘，可自己增删改）。
- 在记录行上右键选择心情（可自定义、可隐藏，历史记录不受影响）。
- 页头「—」把窗口收进托盘；左键托盘图标重新打开；用完请从托盘右键「退出」。

## 从源码运行（开发者）

前置：

- Windows 10/11 x64
- WebView2 运行时（Windows 10/11 一般已自带）
- Node.js 20+ 与 pnpm
- Rust 工具链 `stable-x86_64-pc-windows-msvc`
- Visual Studio Build Tools，勾选「使用 C++ 的桌面开发」工作负载

```bash
git clone https://github.com/Yshuinuan/DesktopEasyNote.git
cd DesktopEasyNote
pnpm install
pnpm tauri dev
```

构建安装包：

```bash
pnpm tauri build
```

产物在 `src-tauri/target/release/bundle/nsis/`。首次构建会联网下载 Rust 依赖与 NSIS
打包工具；产物未签名，首次运行同样会有 SmartScreen 提示。

## 数据与隐私

应用数据保存在 `%LOCALAPPDATA%\DesktopEasyNote\`：

- `data\YYYY-MM-DD.md` 每日记录
- `output\YYYY-MM-DD.md` 次日 07:00 生成的旁白
- `logs\` 运行日志
- `config.json` 设置与 API Key（仅存本机）

程序文件安装在 `%LOCALAPPDATA%\Programs\DesktopEasyNote\`，与数据目录分开；
卸载应用不会删除数据。

API Key 只保存在本机 `config.json`，不会上传，也不会写入仓库；
除调用 DeepSeek API 生成旁白外，应用不联网。

## 已知限制

- 仅支持 Windows（10/11 x64）。
- 安装包未签名，首次运行会有 SmartScreen 提示。
- 次日 07:00 的生成需要应用正在运行；若当时没运行，会在下次启动时补跑。
- 旧版本数据目录 `%LOCALAPPDATA%\桌面旁白记录器\` 会在首次启动时**一次性复制**到新目录
  `%LOCALAPPDATA%\DesktopEasyNote\`（只复制、不删除旧目录，`logs/` 不迁移）；确认新版正常后可自行清理旧目录。

## 文档

- [桌面旁白记录器-策划案](docs/桌面旁白记录器-策划案.md)
- [开发契约](docs/开发契约.md)
- [旁白提示词 v3.0](docs/旁白提示词-v3.0.md)

## 许可证

[MIT](LICENSE)
