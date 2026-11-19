# 桌面旁白记录器

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

## 开发

```bash
pnpm install
pnpm tauri dev
```

## 构建

```bash
pnpm tauri build
```

产物：`src-tauri/target/release/bundle/nsis/桌面旁白记录器_0.1.0_x64-setup.exe`（未签名）

## 数据位置

`%LOCALAPPDATA%\桌面旁白记录器\`

- `data\YYYY-MM-DD.md` 每日记录（06:00 跨天；允许 `#` 标题行）
- `output\YYYY-MM-DD.md` 次日 07:00 生成的旁白
- `logs\` 运行日志
- `config.json` 仅存本机 API Key，不入库

## 使用

1. 安装后首次启动，填入自己的 DeepSeek API Key（`platform.deepseek.com`）
2. 页头写记录；`#` 出标题；右键记录行加心情
3. 保持应用运行，次日 07:00 自动生成；或下次启动自动补跑

## 许可证

MIT
