# wmcterm

基于 `libghostty-vt`、`winit`、`wgpu` 和 `glyphon` 构建的 Rust 原生 GPU
终端工作区。每个终端表面拥有独立的 PTY/VT actor；UI 接收不可变的渲染快照，
并合并重绘唤醒。

渲染由脏场景驱动，帧率上限为 60 Hz。输出突发期间，每个终端只保留最新快照；
隐藏工作区或非活动标签页不会使可见场景失效。高开销帧会自适应调节节奏，未变化
的文本块则保留其已塑形字形缓冲区。光标每 0.5 秒闪烁一次，精确滚轮增量会保留
不足一个单元格的余量，工作区元数据仅在可见值变化时触发重绘。UI 布局和命中测试
始终使用逻辑坐标，而 wgpu 使用经过当前 DPI 缩放的物理尺寸。窗口 `opacity` 同时
应用于渲染背景和原生窗口透明度。

## 系统要求

- Linux 或 macOS，并已安装 `winit`/`wgpu` 所需的原生依赖。
- Windows 10 版本 1809（内部版本 17763）或更高版本，包括 Windows 11。
  必须支持 ConPTY；Windows 安装程序和可执行文件都会拒绝更早的系统版本。
- 从源码构建时需要 Rust 1.90 或更高版本。

在 Windows 上，默认 shell 是系统自带的 Windows PowerShell 5.1 可执行文件
`powershell.exe`。通过 `SHELL` 显式选择时仍支持 PowerShell 7（`pwsh.exe`），
但不会假定 Windows 10 已安装它。

运行 GPU 终端工作区：

```bash
cargo run
```

## 快捷键

- `Ctrl+Shift+D`：向右拆分
- `Ctrl+Shift+E`：向下拆分
- `Ctrl+Shift+T`：新建终端表面标签页
- `Ctrl+Shift+A`：新建工作区
- `Ctrl+Tab` / `Ctrl+Shift+Tab`：切换到下一个/上一个表面标签页
- `Ctrl+Shift+X`：关闭活动终端表面
- `Ctrl+Shift+W`：关闭活动窗格
- `Alt+Arrow`：聚焦对应方向的窗格
- `Ctrl+1` … `Ctrl+9`：直接切换到指定工作区
- `Ctrl+Shift+B`：显示或隐藏工作区侧边栏
- `Ctrl+Shift+P`：命令面板（`Up`、`Down`、`Enter`、`Esc`）
- `Ctrl+Shift+N`：通知中心
- 在通知中心上滚动鼠标滚轮：浏览更早/更新的通知
- `Mark all read` / `全部已读`：清除普通未读标记，但不删除通知，也不处理待定的
  Agent 授权请求
- `Ctrl+F`：搜索全部终端回滚历史（使用 `Enter`/`Shift+Enter` 导航）
- `Ctrl+Shift+L`：筛选活动日志表面（`level:error,warn text`）
- `Ctrl+Shift+F`：使用 `rg` 搜索活动工作区中的文件
- `Ctrl++` / `Ctrl+-` / `Ctrl+0`：增大、减小或重置终端字号
- `Ctrl+Shift+K`：清空终端屏幕和回滚历史
- `Ctrl+Shift+Z`：缩放或取消缩放活动窗格
- `Ctrl+Shift+R`：重命名活动工作区
- `Ctrl+Shift+PageUp` / `Ctrl+Shift+PageDown`：调整活动工作区顺序
- `Ctrl+Shift+U`：跳转到最新未读通知
- `Alt+1` … `Alt+9`：直接切换到活动窗格中的指定表面
- `Ctrl+,`：在系统编辑器中创建或打开用户配置
- `Ctrl+Shift+C` / `Ctrl+Shift+V`：复制终端选区/粘贴
- 单击鼠标：聚焦工作区/窗格，或激活表面标签页
- 单击表面标签页上的 `×`：关闭该标签页；关闭最后一个标签页后，将留下带有可点击
  新建终端入口的空窗格
- 拖动鼠标：选择终端文本并复制到剪贴板；拖到终端顶部或底部边缘之外时，会自动
  滚动历史记录
- 拖动窗格分隔线：调整相邻窗格大小
- 拖动终端滚动条滑块或单击其轨道：在回滚历史中定位/翻页
- 在终端中单击右键：粘贴
- 在终端中单击中键：粘贴
- 右键单击表面标签页：关闭该标签页、关闭其他标签页或新建终端标签页
- 右键单击工作区行：新建终端标签页、重命名、编辑描述、更改目录、固定/取消固定，
  或关闭工作区
- 悬停命令面板条目：预览单击后将执行的项目
- 单击 Prompt/Search 文本：移动支持 Unicode 的插入光标
- 鼠标滚轮：滚动终端回滚历史
- 将文件拖放到终端：把经过安全 shell 引用的路径插入指针所在终端

当 TUI 启用 DEC 鼠标跟踪（包括 SGR 模式）时，鼠标按下、释放、移动及滚轮事件
由 `libghostty-vt` 编码并转发给 PTY。按住 `Shift` 可绕过应用程序，改为选择文本或
滚动 wmcterm 自身的回滚历史。

解析 PTY 输出时，终端生成的协议回复会立即转发，不受合并渲染快照影响。在连续输出
突发期间，回复字节和待处理回复批次均有上限，因此显示不受信任的二进制数据时，不会
积累无限输入积压并在 shell 提示符返回后注入。输出流静默 250 ms 后，回复预算会自动恢复。

侧边栏中的 `+ New Workspace` 条目可以点击。相同命令也可从命令面板使用。
允许关闭最后一个工作区；空应用视图会保留一个可点击的新建工作区入口。

## CLI 和 IPC

GUI 通过 `wmcterm.sock` 提供 JSON-RPC。在 Windows 上，该路径是一个端点文件，
其中包含实例动态分配的回环地址；传输层不会占用固定的全局端口。示例：

```bash
cargo run -- workspace list
cargo run -- workspace new --name demo --cwd /tmp
cargo run -- workspace activate WORKSPACE_ID
cargo run -- workspace rename WORKSPACE_ID "Agent review"
cargo run -- workspace describe WORKSPACE_ID "Reviewing parser changes"
cargo run -- workspace cwd WORKSPACE_ID /path/to/project
cargo run -- workspace move WORKSPACE_ID -1
cargo run -- workspace pin WORKSPACE_ID [--off]
cargo run -- pane split --right
cargo run -- pane split --down --command "codex"
cargo run -- pane list
cargo run -- pane zoom [PANE_ID]
cargo run -- surface new --command "cargo test"
cargo run -- surface markdown README.md
cargo run -- surface logs /tmp/build.log
cargo run -- surface list
cargo run -- surface read [SURFACE_ID]
cargo run -- surface search error [SURFACE_ID]
cargo run -- agent codex
cargo run -- ssh user@host
cargo run -- ssh user@host --tmux SESSION
cargo run -- docker CONTAINER [COMMAND...]
cargo run -- kube POD [COMMAND...]
cargo run -- send "echo hello"
cargo run -- send-key Enter
cargo run -- screen read [--surface SURFACE_ID]
cargo run -- terminal search error [--surface SURFACE_ID]
cargo run -- terminal clear [--surface SURFACE_ID]
cargo run -- terminal scroll -20 [--surface SURFACE_ID]
cargo run -- notify --title Build --body "Needs review"
cargo run -- notification list
cargo run -- agent-event '{"event":"permission_request","message":"Approve?"}'
cargo run -- hook plan --agent codex
cargo run -- hook install --agent codex --scope project
cargo run -- hook install --agent claude --scope user
cargo run -- hook receive --agent claude --event Stop < hook.json
cargo run -- skill install --scope user
cargo run -- window new /path/to/project
cargo run -- status
```

`wmcterm send --surface ID -- TEXT` 使用终端的 bracketed-paste 协议。这样可以原子地
提交多行和长提示，并让 Claude Code、Codex 等 TUI 使用其正常的粘贴内容处理逻辑。
发送单个按键时使用 `wmcterm send-key KEY --surface ID`。对于普通 shell 命令，
`wmcterm send-line --surface ID -- COMMAND` 会将 bracketed paste 和 Enter 原子地
排入同一条 terminal-actor 命令。

`wmcterm screen read --surface ID` 返回可见快照、单调递增的 revision，以及光标和
视口元数据。添加 `--scrollback` 可读取有界的完整终端历史，而不会移动用户视口；
添加 `--lines N` 可仅保留最后 N 个逻辑行。Agent 可以使用
`wmcterm screen wait --surface ID --until TEXT --timeout 10s` 避免盲目 sleep；
`--revision N` 会等待先前读取的 revision 发生变化，而 `--change` 使用等待开始时
观察到的屏幕状态。等待轮询由 CLI 执行，因此 GUI 事件循环和 terminal actor 会保持响应。

Hook 接收器会继承由 wmcterm 启动的终端所含的 wmcterm 上下文变量，因此并发 Agent
会把状态和通知报告给正确的工作区、窗格及表面。
`hook plan` 只输出用于合并的集成片段，绝不会编辑 Agent 配置。`hook install` 会在
项目或用户作用域安全合并 Codex、Claude Code 和 Gemini 的 JSON Hook 定义，重复安装
会自动去重。对于插件格式无法安全合并的其他受支持 Agent，仍会提供安装计划。Codex
新安装或发生变化的命令 Hook 必须先通过 `/hooks` 审核才能运行。

CLI 也是稳定的 Agent 控制接口。在 wmcterm 终端内启动的命令会继承 `WMCTERM=1`、
`WMCTERM_WORKSPACE_ID`、`WMCTERM_PANE_ID`、`WMCTERM_SURFACE_ID`、
`WMCTERM_TERMINAL_LIFECYCLE_ID` 和 `WMCTERM_CONTROL_SOCKET`。每当该终端的 PTY
被重新创建时，lifecycle 值都会变化，因此旧进程延迟到达的 Hook 无法更新新会话。
为兼容旧版本，还会导出 `WMCTERM_SOCKET` 别名。因此，屏幕读取、发送、控制键、搜索、
滚动、通知和表面操作默认都以调用 Agent 自己的终端为目标。要显式编排另一终端，请使用
`--surface ID`。`wmcterm capabilities` 返回机器可读的协议版本、RPC method 列表、
上下文变量、屏幕控制功能及受支持的控制键。协议版本 3 新增了 `send.line`、屏幕
revision 和非侵入式回滚历史读取。

`wmcterm codex-teams [CODEX_ARG...]` 会启动实验性的私有 Codex app-server，并连接
根 Codex TUI。子 Agent 的线程创建事件会生成原生 wmcterm 窗格且不抢夺焦点：第一个
窗格在右侧打开，后续 Agent 向下打开。自动展开默认最多两层，可通过
`WMCTERM_CODEX_MAX_AUTO_DEPTH` 限制。此集成要求 Codex CLI 提供
`app-server --listen` 和 `--remote` 选项。如果可选的 app-server 集成无法启动，
托管 Codex 会报告原因并回退到普通 Codex TUI，而不会导致 Codex 不可用。
监视器会恢复启动时已加载的线程，以及之后公告且排除了 turns 的线程。这样，初始线程或
通过 `/resume` 选择的根线程所发出的 app-server 审批，可以进入集中式 UI，而无需将
transcript 加载到监视器。它识别所有携带线程 ID 的 `thread/*` 公告，并定期校对已加载
线程列表，因此遗漏的 resume 通知不会永久绕过审批桥。
托管 Codex 默认以 `--ask-for-approval on-request --sandbox workspace-write` 启动：
普通工作区操作无需提示即可进行，沙箱外请求则路由至 wmcterm 的集中式审批 UI。
需要长时间无人干预时，可从命令面板选择“新建 Codex/Claude Code（无人值守）”，
或运行 `wmcterm agent --mode unattended codex|claude`。该模式使用 provider 原生的
自动审查能力：Codex 使用 `--approve-for-me`（该参数自身启用 `workspace-write`
沙箱），Claude 使用 `--permission-mode auto`；它不是“自动同意所有请求”，也不会
关闭沙箱。
对于这些托管 app-server 会话，通用 Codex `PermissionRequest` Hook 被视为重复遥测，
不会创建第二个可操作请求；非托管 Codex 的 Hook 通知保持正常行为。
如果根 Codex TUI 先回答同一个 app-server 请求，wmcterm 会消费 Codex 的
`serverRequest/resolved` 确认或精确匹配的 item lifecycle 事件，并且只关闭对应的
通知卡片；终端按键和无关 item 活动绝不会清除审批。
显式指定的审批或沙箱选项仍然优先。托管启动器拒绝 Codex/Claude 的 bypass 参数、
`danger-full-access` 沙箱和 CLI `--mode full-access`。命令面板另有明确标为“完全访问…”
的人工入口；选择后还必须输入 `FULL ACCESS`，且只影响新启动的 Agent 会话，不写入
配置。该会话关闭审批和沙箱，应仅在已有外部隔离边界时使用。

可操作的 Agent 请求使用有界请求/回复通道，而不是普通通知。使用
`wmcterm agent-request list` 查看请求，然后使用
`agent-request reply ID --approve|--deny|--choice ID|--text TEXT` 或
`agent-request cancel ID [REASON]` 处理。请求携带所属 Surface 和终端 lifecycle
标识，会安全过期，并拒绝陈旧或重复的响应。通知中心的审批卡片优先显示命令、
工具、工作目录、原因或权限等可读摘要；详情保留格式化 JSON，并提供滚轮、方向键、
Page Up/Down、Home/End、行数进度和可拖动滚动条。
卡片只呈现“拒绝”“允许一次”和 provider 明确提供时的“信任规则”；“无人值守”和
“完全访问”是 Agent 会话启动模式，不是某一条审批请求的升级按钮。

交互式关闭操作会在终止活动进程前发出警告。明确指定工作区、窗格或表面 UUID 的
Agent 和 CLI 控制调用会直接执行，不弹出 UI 确认；显式目标和调用 Agent 的审批边界
具有最终决定权。

内置 `wmcterm` Agent Skill 记录了此控制接口。唯一签入的源文件是
`integrations/skills/wmcterm/SKILL.md`；隐藏的 Agent 目录是生成的发现目标，而非
源文件位置。`wmcterm skill install --scope user` 会为所有仓库将内置源注册到
`~/.agents/skills`、`~/.codex/skills` 和 Claude Code 的个人 `~/.claude/skills`
目录；默认使用项目作用域，并将忽略的副本写入 `.agents/skills` 和 Claude Code 的
`.claude/skills` 发现目录。Hook 计划和 JSON 片段由 `crates/wmcterm-agent` 生成，
而 `.codex/hooks.json` 及其他 Agent 配置文件仅作为合并目标。
设置 `CLAUDE_CONFIG_DIR` 后，Claude 副本和用户 Hook 配置会使用该目录。除非提供
`--force`，否则会保留已有的不同内容。

Claude Code 会继承 `WMCTERM_WORKSPACE_ID`、`WMCTERM_PANE_ID`、
`WMCTERM_SURFACE_ID` 和 `WMCTERM_CONTROL_SOCKET`。由于 Claude 的 Bash 权限
匹配器在 shell 展开之前运行，Agent 编写的控制命令应包含来自该上下文或先前 JSON
响应的字面 UUID，而不是 `$WMCTERM_*` 表达式或复合 shell 管道。对于需要单独显示的
委派任务，请创建窗格并在其中启动新的托管 `wmcterm agent claude` 进程。启动器在该
窗格的终端内运行，不会分配第二个标签页。Claude 自己的内部子 Agent 保留在所属的
Claude 进程内；Hook 会公开其生命周期，但不会将已有内部子 Agent 迁移到另一个终端窗格。

替换已安装的 wmcterm 二进制文件后，请以桌面/登录用户（不要使用 root）刷新用户级
Skill 和 Codex Hook：

```bash
/usr/lib/wmcterm/update-agent-integration
```

更新程序会刷新 wmcterm Skill，删除陈旧的 wmcterm Codex 和 Claude Hook 接收器
（同时保留无关设置和 Hook），安装当前接收器，并验证全部三个 Skill 位置以及当前
Codex 和 Claude Hook 计划中的每个事件。Debian 维护脚本有意不写入推测的用户主目录。
新发生变化的 Codex 命令 Hook 需要通过 `/hooks` 审核一次。

发布版可执行文件名为 `wmcterm`。原始 RPC 仍可通过
`wmcterm rpc METHOD PARAMS_JSON` 使用。

## 配置和项目

用户配置从平台配置目录下的 `wmcterm/config.toml` 加载。项目可以定义
`.wmcterm.toml`：

使用以下配置为新建工作区设置默认目录：

```toml
[workspace]
inherit-cwd = false
default-cwd = "~/code"
```

当 `inherit-cwd` 为 true 且存在活动工作区时，新工作区会继承该目录。否则使用
`default-cwd`，再回退到 wmcterm 进程目录。配置的路径必须是现有目录。

```toml
[workspace]
name = "wmcterm"

[[command]]
name = "Tests"
command = "cargo test"
```

工作区布局、表面、终端命令和受支持的 Agent 会话 ID 会从平台数据目录恢复。可见的
回滚历史会保留；PTY 进程内存绝不会被序列化，命令以及 Claude/Codex/OpenCode 的
resume 标识符会被重新启动。

可使用 `wmcterm config init`、`config show`、`config validate` 或
`config import-ghostty [PATH]`。UI 每秒重新加载一次有效的配置变更。支持主题颜色、
自定义快捷键、字号、回滚历史大小、侧边栏宽度、工作区 cwd 继承、通知、窗口装饰和
背景透明度。终端 `scrollback` 是字节上限，默认为 128 MB，与 Ghostty 的
`scrollback-limit` 语义一致；`scrollback-lines` 独立地将逻辑历史限制为 100,000 行。
任一限制先达到时，都会裁剪最早的可访问历史。`config import-ghostty` 只打印合并预览，
不会覆盖任一配置。

wmcterm 提供英文和简体中文界面。默认为 `auto`，即跟随操作系统区域设置。也可显式选择：

```toml
[ui]
language = "zh-CN" # 也可设为 "en" 或 "auto"

# 如果终端应用程序应接收 [keybindings] 中未配置的所有快捷键，请设为 false。
legacy_shortcuts = true

[navigation]
# 设为 false 可隐藏可点击的导航栏。位置可设为 "top" 或 "bottom"。
visible = true
position = "bottom"
# 可重新排序或移除：sidebar、new-workspace、command-palette、notifications。
items = ["sidebar", "notifications"]

[terminal]
# 终端的普通 BEL 事件："never"、"background" 或 "always"。
# 默认设置会避免为当前正在使用的终端中的 Backspace/Tab 边界铃声发送通知，
# 但仍会通知后台任务。
bell-notifications = "background"
bell-debounce-ms = 1000
```

此响铃策略仅适用于终端 BEL 字符。权限请求、完成和错误的显式 Agent 与 OSC 通知
不会被抑制。

内置默认字体为 `Maple Mono Normal NF`，复制自 `MapleMonoNormal-NF-unhinted`
发行版；字体资产中包含其 OFL 许可证。

应用程序/窗口图标位于 `assets/icons`，基于同级 wmcssh 项目的图标调整了圆角边框和
透明外区。PNG 在运行时由 winit 加载，多分辨率 ICO 则嵌入 Windows 构建。

支持 OSC 9/99 通知和 wmcterm OSC 777 Agent 协议。OSC 777 载荷使用
`wmcterm;STATE;MESSAGE`，其中 state 为 `running`、`input`、`permission`、
`completed` 或 `error`。

## 工作区 crate

- `wmcterm-core`：Workspace、Pane tree、Surface、action 和 notification
- `wmcterm-pty`：可移植 PTY/ConPTY actor
- `wmcterm-terminal`：Ghostty VT owner actor 和渲染快照
- `wmcterm-renderer`：wgpu 矩形及 glyphon 字体/图集渲染
- `wmcterm-app`：winit UI、帧调度和命令路由
- `wmcterm-ipc`：JSON-RPC 本地传输
- `wmcterm-agent`：OSC 和 Agent 状态转换
- `wmcterm-codex`：隔离的实验性 Codex app-server/子 Agent 桥
- `wmcterm-config`、`wmcterm-session`：分层配置和恢复
- `wmcterm-surface`：Markdown 文档及有界、可筛选、可跟踪追加的 Log 表面

自动更新和嵌入式浏览器有意不在 wmcterm 的范围内。URL 仍可通过外部系统应用程序打开。

Phase 0 无头 VT 快照工具仍然可用：

```bash
cargo run -- dump --cols 80 --rows 24 -- sh -lc 'printf "\033[32mhello\033[0m\n"'
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release --workspace
cargo check --workspace --target x86_64-pc-windows-gnu
```

使用以下命令构建受支持的安装包（Linux x86_64 Deb 和 Windows x86_64 NSIS
安装程序）：

```bash
scripts/package-release.sh
```

产物写入 `target/dist/`。打包需要 `dpkg-deb`、`x86_64-w64-mingw32-gcc`、
`x86_64-w64-mingw32-strip` 和 `makensis`。Windows 安装程序要求 Windows 10
1809 或更高版本（支持 Windows 11），会将 `wmcterm.exe` 安装到 Program Files，
注册 App Paths，并将安装目录添加到系统 `PATH`。Shell 和桌面快捷方式使用 GUI
子系统的 `wmcterm-gui.exe` 启动器，因此打开应用时不会闪现或保留控制台窗口；
`wmcterm.exe` 仍是用于 CLI 和 Hook 输出的控制台子系统可执行文件。

纯布局、命中测试、本地化和渲染器色彩空间行为由 `cargo test --workspace` 覆盖。
在 Linux 上，真实窗口 smoke test 会在 Xvfb 下渲染英文和中文界面并验证截图：

```bash
# Debian/Ubuntu 依赖：xvfb xdotool imagemagick
scripts/ui-smoke.sh
scripts/runtime-control-smoke.sh
```

生成的截图写入 `target/ui-smoke-en.png` 和 `target/ui-smoke-zh-CN.png`。运行时控制
smoke test 会通过 winit 向 shell 发送真实的键盘空格事件，通过本地 IPC socket 创建
窗格，并验证子终端是否收到 wmcterm 控制环境变量。
