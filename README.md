# wmcterm

`wmcterm diagnostics` 可检查 GUI/CLI 可执行文件路径、Codex/Claude 版本、最近
实际收到的 Hook 来源，以及每个终端的鼠标、键盘、焦点和粘贴协议状态。
诊断仅查询状态，不修改 Hook 安装或用户配置。

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

wmcterm 会为新建或恢复的 Shell、Codex 和 Claude 读取当前桌面代理：Linux 支持
GNOME 与 KDE 的静态手动代理，Windows 读取当前用户的 WinINet 设置。代理缓存每
5 秒在后台刷新，创建进程前也会请求刷新；已经运行的进程不会被重启或修改环境。
现有 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY`、`NO_PROXY`（不区分大小写）以及
RPC 显式环境变量始终优先，桌面设置只补缺且 `NO_PROXY` 不合并。PAC/WPAD 无法
无损转换为静态环境变量，因此不会自动注入。
托管 Claude 启动同时支持原生 `claude.exe` 和 npm 安装生成的 `claude.cmd`/`claude.bat`
shim；npm 布局会直接解析包内原生程序或 Node 入口，不经过命令 shell。

运行 GPU 终端工作区：

```bash
cargo run
```

## 快捷键

终端中的 `Ctrl+C` 默认传给前台程序（包括 Codex），鼠标选区不会拦截它；复制选区使用 `Ctrl+Shift+C`。

- `Ctrl+Shift+D`：向右拆分
- `Ctrl+Shift+E`：向下拆分
- `Ctrl+Shift+T`：新建终端表面标签页
- `Ctrl+Shift+A`：新建工作区
- `Ctrl+Tab` / `Ctrl+Shift+Tab`：切换到下一个/上一个表面标签页
- `Ctrl+Shift+X`：关闭活动终端表面
- `Ctrl+Shift+W`：关闭活动窗格
- `Ctrl+Alt+Arrow`：聚焦对应方向的窗格；单独的 `Alt+Arrow` 传给终端程序
- `Ctrl+1` … `Ctrl+9`：直接切换到指定工作区
- `Ctrl+Shift+B`：显示或隐藏工作区侧边栏
- `Ctrl+Shift+P`：命令面板（`Up`、`Down`、`Enter`、`Esc`）
- `Ctrl+Shift+N`：通知中心
- 在通知中心上滚动鼠标滚轮：浏览更早/更新的通知
- `Mark all read` / `全部已读`：清除普通未读标记，但不删除通知，也不处理待定的
  Agent 授权请求
- `Ctrl+Alt+F`：搜索全部终端回滚历史（使用 `Enter`/`Shift+Enter` 导航）；`Ctrl+Alt+G` 查找下一处
- 搜索栏打开时，点击终端可恢复终端输入并保留搜索高亮；点击搜索栏可继续编辑。
  在其他窗格的标签栏上滚动只切换其标签，不转移当前键盘焦点。通知、审批面板和菜单
  占用键盘时，输入法提交不会写入底层终端。
- 将文件拖入终端会插入经过 shell 转义的路径，并自动聚焦接收路径的窗格，便于继续
  输入；不会自动执行命令。落点被菜单或模态弹窗遮挡时不会写入底层终端。
- `Ctrl+Shift+L`：筛选活动日志表面（`level:error,warn text`）
- `Ctrl+Shift+F`：使用 `rg` 搜索活动工作区中的文件
- `Ctrl++` / `Ctrl+-` / `Ctrl+0`：增大、减小或重置终端字号
- `Ctrl+Shift+K`：清空终端屏幕和回滚历史
- `Ctrl+Shift+Z`：缩放或取消缩放活动窗格
- `Ctrl+Shift+R`：重命名活动工作区
- `F2`：传给终端程序（如 Codex），不用于重命名工作区
- `Ctrl+Shift+PageUp` / `Ctrl+Shift+PageDown`：调整活动工作区顺序
- `Ctrl+Shift+U`：跳转到最新未读通知
- `Alt+1` … `Alt+9`：直接切换到活动窗格中的指定表面
- `Ctrl+,`：在系统编辑器中创建或打开用户配置
- `Ctrl+Shift+C` / `Ctrl+Shift+V`：复制终端选区/粘贴
- 单击鼠标：聚焦工作区/窗格，或激活表面标签页
- 标签页在空间充足时等宽扩展；空间不足时，单击标签栏中的 `⋯N` 可从完整标题列表
  切换到被隐藏的标签页
- 单击表面标签页上的 `×`：关闭该标签页；关闭最后一个标签页后，将留下带有可点击
  新建终端入口的空窗格
- 拖动鼠标：选择终端文本；Linux 写入 `PRIMARY` 选中区，其他平台写入系统剪贴板；
  拖到终端顶部或底部边缘之外时，会自动滚动历史记录
- 拖动窗格分隔线：调整相邻窗格大小
- 拖动终端滚动条滑块或单击其轨道：在回滚历史中定位/翻页
- 在普通终端中单击右键：打开终端上下文菜单；TUI 启用鼠标跟踪后，普通右键转发给
  TUI，`Shift+右键` 强制打开 wmcterm 上下文菜单
- 在终端中单击中键：Linux 粘贴 `PRIMARY` 选中区，其他平台粘贴系统剪贴板
- 右键单击表面标签页：关闭该标签页、关闭其他标签页或新建终端标签页
- 右键单击工作区行：新建终端标签页、重命名、编辑描述、更改目录、固定/取消固定，
  或关闭工作区
- 悬停命令面板条目：预览单击后将执行的项目
- 单击 Prompt/Search 文本：移动支持 Unicode 的插入光标
- 鼠标滚轮：滚动终端回滚历史
- 将文件拖放到终端：把经过安全 shell 引用的路径插入指针所在终端

当 TUI 启用 DEC 鼠标跟踪（包括 SGR 模式）时，鼠标按下、释放、移动及滚轮事件
由 `libghostty-vt` 编码并转发给 PTY。按住 `Shift` 可绕过应用程序，改为选择文本、
打开右键菜单或滚动 wmcterm 自身的回滚历史。

解析 PTY 输出时，终端生成的协议回复会立即转发，不受合并渲染快照影响。在连续输出
突发期间，回复字节和待处理回复批次均有上限，因此显示不受信任的二进制数据时，不会
积累无限输入积压并在 shell 提示符返回后注入。输出流静默 250 ms 后，回复预算会自动恢复。

侧边栏工作区列表超出可见范围时显示滚动条，支持滚轮、拖动滑块和点击轨道定位；
滚动条独占右侧留白，不覆盖文字，也不占用侧边栏宽度调整边界。
轨道延伸到侧边栏底部；用于右键新建工作区的 32px 留白位于列表末尾，随列表滚动。
侧边栏中的 `+ New Workspace` 条目可以点击。相同命令也可从命令面板使用。
允许关闭最后一个工作区；空应用视图会保留一个可点击的新建工作区入口。
工作区条目支持按住整行拖动排序；移动超过少量阈值后才进入拖动状态，拖动时会显示
跟随指针的条目预览和插入位置线。搜索工作区时暂不启用排序，以免过滤结果改变持久顺序。

## 本地 Agent 操作 SSH

本地 Agent 可以在指定的空闲 Bash 终端中连接远端并获取命令结果。需要本地和远端
Bash 4.2.46+，本地系统 OpenSSH，以及远端 `mktemp`、`od`、`tr` 等基础工具。
普通新建终端的默认 Shell 为 Bash 时自动启用会话集成；已运行的旧终端、显式启动
命令和其他 Shell 不会被自动接管。Agent 自己正在运行的终端不是空闲 Shell，应先
使用下面的一步连接入口创建后台操作终端，或手动创建专用终端。

```bash
wmcterm ssh dev-server --pane CALLER_PANE_UUID
wmcterm ssh dev-server --surface SURFACE_UUID --wait
wmcterm ssh exec --surface SURFACE_UUID --connection CONNECTION_ID --host dev-server --command-file /absolute/check.sh --wait
wmcterm ssh status --surface SURFACE_UUID
wmcterm ssh cancel --surface SURFACE_UUID --connection CONNECTION_ID --command-id COMMAND_ID --wait
```

`wmcterm ssh HOST` 默认从 `WMCTERM_PANE_ID` 指定的调用者 Pane 向右分屏；
可用 `--pane UUID` 显式指定，`--new` 可省略。缺少调用者 ID 时要求显式指定，
不会使用 UI 当前焦点。新分屏不切换焦点，自动等待本地初始化和远端
连接，返回 `surface_id`、`pane_id`、`workspace_id`、`host` 和 `connection_id`。
默认总等待 30 秒，可用 `--timeout-ms N` 调整，最大 300000 毫秒。已有专用终端的
连接命令加 `--wait` 也会等待本地 Shell 初始化，无需 Agent 单独轮询。
本地初始化超时返回 `wait_timed_out=true`、`wait_phase=local_ready`、`requested_host`
和已创建的 Surface ID；检查屏幕后用该 ID 继续连接，不要再次 `--new`。
连接已提交后的超时保留连接 ID，不取消连接，应查询同一连接的状态。
保存返回的连接信息，确认 `state=ready` 后才能执行命令。
后续执行和取消始终传入保存的连接 ID，连接变化时停止，避免误操作另一台服务器。
执行时同时传入保存的 `--host HOST` 和 `--connection ID`，终端 actor 会原子校验
主机、连接 ID 和就绪状态，因此复用前无需额外查询状态。主机按原始别名精确匹配。

`ssh exec` 必须选择且只能选择一种输入：`--command TEXT`、`--command-file PATH`
或 `--command-stdin`。文件和标准输入按 UTF-8 原样读取，最大 16384 字节且不能含
NUL；内容是远端 Bash 脚本。复杂引号和多行检查优先使用文件或标准输入，避免手工
嵌套本地 Shell 引号。可在同一脚本合并独立只读检查，但同一连接的命令须串行执行。

```bash
wmcterm ssh exec --surface SURFACE_UUID --connection CONNECTION_ID --host dev-server --command-stdin --wait <<'REMOTE_SCRIPT'
printf '%s\n' '--- directory ---'
pwd
printf '%s\n' '--- git status ---'
git status --short
REMOTE_SCRIPT
```

内置 Skill 只在用户明确要求 `wmcterm ssh` 或通过 wmcterm 连接 SSH 时使用托管
SSH；普通远端任务默认通过 Agent 的 Shell 工具使用系统 SSH。启用后默认创建
不切换焦点的独立分屏，或复用当前任务已记录且主机和连接 ID 均匹配的专用连接。
Hook 不会拦截或改写普通系统 `ssh`。
正常流程一次调用完成分屏和连接，后续直接使用返回的连接信息执行命令；
`exec --wait` 已返回状态、退出码和输出时，不再额外读取屏幕或查询状态。

端口、密钥、跳板机使用 `~/.ssh/config` 中的主机别名配置。连接不会切换焦点。
`wmcterm ssh --help`（或 `-h`）仅显示帮助，不创建工作区或连接服务器；
`ssh exec/status/cancel --help` 同样可用。主机名必须放在 SSH 选项之前。
兼容旧版本时可使用 `wmcterm --help`。
密码、主机指纹确认和 MFA 仍由 OpenSSH 在该终端交互；`connecting` 包含等待认证
和等待远端集成就绪，不能单凭此状态区分两者。需要时读取该 Surface 的屏幕并由用户
完成认证。显式传入传统 SSH 参数的 `wmcterm ssh TARGET SSH_ARG...` 仍创建普通
SSH 工作区，`--tmux` 也保留旧行为，不提供上述结构化命令接口。

远端只在 `~/.wmcterm/tmp/` 创建权限为 `0600` 的独立临时脚本；两个目录为 `0700`。
脚本加载时立即删除，不修改 `.bashrc` 或安装服务。集成先加载已有 `.bashrc`，保留
原 `PROMPT_COMMAND`，启动的是交互式非登录 Bash，不自动加载 `.bash_profile`。
异常启动可能留下临时文件，不会扫描或删除其他连接的文件。

RPC 为 `ssh.connect/status/exec/cancel`，都要求 `surface_id` UUID。`connect` 接受
`host`；`exec` 接受 `connection_id`、`command`、可选 `max_output_bytes`；`cancel`
还要求 `command_id`。CLI 未传 `--connection` 时先查询并绑定当前连接，RPC 不做隐式
绑定。每个 Surface 只允许一个执行中的命令，状态保留最近一次结果，下一条命令会替换它。
命令以当前 Bash 中的函数内 `eval` 执行，cwd 和普通变量可延续，不等同于独立进程
或顶层脚本；修改集成函数、Readline 绑定和提示符 hook 会影响后续自动化。

返回值包含 `state`、`connection_id`、远端 `cwd` 和 `command`，后者包含
`command_id/state/output/exit_code/truncated`。输出是合并 stdout/stderr 的 PTY
内容，保留 ANSI 和 CRLF，后台进程输出也可能混入。默认最多 64 KiB，可通过
`--max-output-bytes` 提高到 1 MiB；不在 UTF-8 字符中间返回截断片段。
`--wait` 默认最多等待 30 秒，`--timeout-ms` 上限 300000；超时返回
`wait_timed_out: true`，不停止或重跑命令，后续通过 `ssh status` 查询。
取消先发送 Ctrl+C，确认恢复提示符前仍为 `cancelling`；忽略中断的程序可能继续运行。
人工输入使执行结果变为 `outcome_unknown`；可用已有 `send/send-key/screen read`
接管交互程序，回到提示符后再继续。断线、`exit` 或 `exec` 不会被误报为命令成功。
连接和命令 ID 不跨应用重启恢复，也不会自动重连或重放命令。

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
项目或用户作用域安全合并 Codex、Claude Code 的 JSON Hook 定义，重复安装
会自动去重。Agent 启动、进程识别、Hook 和会话恢复仅支持 Codex 与 Claude Code。Codex
新安装或发生变化的命令 Hook 必须先通过 `/hooks` 审核才能运行。
Codex 0.150.0 的 `Interrupt` Hook 会在用户中断当前 turn 时立即清除运行中
状态，但不会把仍在等待下一次输入的 Codex 进程标记为已退出。
Claude Code 的 `StopFailure` Hook 会将 API 错误、限流或认证失败导致结束的回合
标记为错误，并保留会话及后台子 Agent；下一次活动可继续更新状态。旧 session 的
延迟 Hook 不会覆盖当前会话。已有安装需要显式重新执行
`wmcterm hook install --agent claude --scope user` 才会注册此事件。
Claude 的 MCP 表单、URL 授权及额度恢复后需按 Enter 的通知会标记为等待输入；
额度自动续跑恢复运行状态，取消自动等待则回到空闲状态，不结束会话。
Codex 0.151.0 的 `SubagentStart`/`SubagentStop` Hook 会按 Surface、session 和
agent ID 跟踪后台子 Agent；只要任一子 Agent 仍活动，Workspace 就保持运行中，
父 turn 完成不会提前显示为已完成。Stop 会留下每个 session 最近 256 个有界墓碑，
阻止乱序到达的同键 Start 重新激活。升级和普通运行不会自动修改 Codex 配置；只有用户
显式执行 `wmcterm hook install --agent codex --scope user` 才会写入 `CODEX_HOME`（默认 `~/.codex`），
随后仍需在 Codex `/hooks` 中审核变化。项目级安装使用 `--scope project`。

CLI 也是稳定的 Agent 控制接口。在 wmcterm 终端内启动的命令会继承 `WMCTERM=1`、
`WMCTERM_WORKSPACE_ID`、`WMCTERM_PANE_ID`、`WMCTERM_SURFACE_ID`、
`WMCTERM_TERMINAL_LIFECYCLE_ID` 和 `WMCTERM_CONTROL_SOCKET`。每当该终端的 PTY
被重新创建时，lifecycle 值都会变化，因此旧进程延迟到达的 Hook 无法更新新会话。
为兼容旧版本，还会导出 `WMCTERM_SOCKET` 别名。因此，屏幕读取、发送、控制键、搜索、
滚动、通知和表面操作默认都以调用 Agent 自己的终端为目标。要显式编排另一终端，请使用
`--surface ID`。`wmcterm capabilities` 返回机器可读的协议版本、RPC method 列表、
上下文变量、屏幕控制功能及受支持的控制键。协议版本 4 移除了无排序效果的
`workspace.pin`；工作区顺序统一由 `workspace.move` 控制。版本 3 新增了
`send.line`、屏幕 revision 和非侵入式回滚历史读取。

`wmcterm codex-teams [CODEX_ARG...]` 会启动实验性的私有 Codex app-server，并连接
根 Codex TUI。子 Agent 的线程创建事件会生成原生 wmcterm 窗格且不抢夺焦点：第一个
窗格在右侧打开，后续 Agent 向下打开。自动展开默认最多两层，可通过
`WMCTERM_CODEX_MAX_AUTO_DEPTH` 限制。此集成要求 Codex CLI 提供
`app-server --listen` 和 `--remote` 选项，以及带 `approvalsReviewer` 的线程恢复协议
（已核验 Codex 0.163.0）。集成或权限核验失败会显示错误，不再自动回退到普通 TUI。
退出后的终端可按 Enter 重试；需要独立恢复时，用户可另开 Shell 执行
`codex resume <会话ID>`，或显式添加 `--approve-for-me` 选择自动审查。
如果长时间运行期间最后创建的子 Agent 窗格已被关闭，后续线程会自动回退到仍存活的
根 Codex 表面继续分割，不会因缓存的表面 ID 失效而放弃创建窗格。
监视器会恢复启动时已加载的线程，以及之后公告且排除了 turns 的线程。这样，初始线程或
通过 `/resume` 选择的根线程所发出的 app-server 审批，可以进入集中式 UI，而无需将
transcript 加载到监视器。它识别所有携带线程 ID 的 `thread/*` 公告，并定期校对已加载
线程列表，因此遗漏的 resume 通知不会永久绕过审批桥。
托管 Codex 默认以 `--ask-for-approval on-request --sandbox workspace-write` 启动：
普通工作区操作无需提示即可进行，沙箱外请求则路由至 wmcterm 的集中式审批 UI。
需要自动审查权限请求时，可从命令面板选择“新建 Codex/Claude Code（自动审查）”，
或运行 `wmcterm agent --mode unattended codex|claude`。该模式使用 provider 原生的
自动审查能力：Codex 使用 `--approve-for-me`（该参数自身启用 `workspace-write`
沙箱），Claude 使用 `--permission-mode auto`；它不是“自动同意所有请求”，也不会
关闭沙箱。
Claude Code 2.1.268 起，`-p`/`--print` 也会触发 `PermissionRequest` Hook。
托管 Claude 交互启动默认显式传入 `--permission-mode manual`，避免新版 Claude
默认 auto 模式改变交互入口的行为；用户显式指定的安全权限模式仍保留。
已核验 Claude Code 2.1.304；旧名称 `default` 仍可显式使用。
Claude 2.1.295+ 的审批 Hook 使用 `onFailure: "block"`，启动失败、崩溃或超时会
拒绝请求；普通状态 Hook 不阻断操作。`UserPromptSubmit` 会及时更新运行状态，
不保存用户提示内容。托管入口拒绝 `--bg` / `--background`、`--cloud`、`--desktop`
和 `--tmux`，避免会话脱离当前终端；原生后台会话暂不纳入托管恢复。
托管入口拒绝 `--bare`、`--safe-mode`、`--restricted` 和自定义 `--setting-sources`，
并检查 `--settings` JSON/文件以及发现的用户、项目设置，拒绝启用
`disableAllHooks` 或 `allowManagedHooksOnly`。继承的 `CLAUDE_CODE_SIMPLE` /
`CLAUDE_CODE_SAFE_MODE` 开关也会阻止启动。这些检查不修改 Agent 配置；仍须安装
wmcterm Hook，且管理员策略、运行期间修改配置或插件行为可能影响 Hook 的实际执行。
原生 Claude 的 auto / bypass 启动参数也会显示对应模式；当前会话经过校验的
Hook `permission_mode` 优先于启动参数，反映运行期间的切换，仅用于状态展示。
新会话和终端重启会清理旧模式缓存；子任务不能通过 `SessionStart` / `SessionEnd`
改变主会话，带序列号的重复或过期 Hook 也不会重写模式。
输入等待通知不会创建审批按钮，恢复执行后会退出等待；`agent_completed`、认证成功
等普通通知保留任务状态。真实权限请求仍由独立审批通道处理，不会被这些通知清除。

Codex 0.163.0 的普通 `requestUserInput` 问题和简单 MCP `form` 请求可在请求面板
点击“回答”，逐字段提交；字段支持文本、整数、有限数字、布尔值和字符串枚举。
密文问题、URL/设备验证及带额外约束的复杂表单继续使用 Codex 原生界面。
可用 `wmcterm agent-request reply ID --text '{"字段ID":"答案"}'` 回答，所有值
均为字符串；数值和布尔字段会在提交时校验并转换。取消、过期、原生界面已回答、
轮次结束或断连都会清理面板请求；普通通知的已读状态不会影响它们。
Claude Code 的简单 MCP `form` 同样可在请求面板回答，使用 `Elicitation` Hook
接管受支持的表单，回复格式与上述 CLI 相同。该 Hook 阻塞等待面板回答，成功后替代
原生对话框；取消、超时或 IPC 失败返回 `cancel`，不会继续提交。URL、密文和复杂
schema 不接管，直接保留原生界面。`ElicitationResult` 仅在携带明确的 elicitation ID
时校对同一 session、server、Surface 和 lifecycle 的未完成请求，不能按服务器名称
批量取消其他表单。受支持表单被 Hook 回答后，Claude 不再触发 `ElicitationResult`。
Claude `PermissionDenied` 记录 auto 模式拒绝的工具及原因，只生成普通通知，不创建
审批按钮，也不会自动要求重试。`CwdChanged` 和 `DirectoryAdded` 更新主会话目录元数据；
子 Agent 的目录不改写主会话。额外目录按会话去重并最多保留 128 项，会话替换时清空，
`wmcterm diagnostics` 可查看 `agent_session.cwd` 和 `additional_directories`。
这些 Hook 需要显式重新安装；Claude 计划使用可执行文件与 `args` 直接启动，无需 shell，
重装会清理旧 wmcterm shell 条目并保留其他 Hook。不会安装接管 worktree 创建/删除的 Hook。
Codex 权限申请另提供“严格审核”按钮（`--choice strict`）：授权仅作用于本轮，
后续命令继续接受审核；它与会话级“始终允许”和启动时自动审批模式独立。

终端支持 OSC 7501 状态报告和能力查询，解析分层记录并去重；它只作为没有 Hook
身份信息时的 Agent 状态回退，不生成审批按钮，也不会覆盖尚未解决的请求。
升级核验可运行 `python3 scripts/agent-protocol-smoke.py`，检查本机 Codex 生成的
协议 schema 和 wmcterm Hook 计划，不调用模型、不修改用户配置。
Claude 的单条权限建议会显示为“添加规则”“切换模式”等实际操作，摘要和详情展示
会话、项目或用户作用域、保存位置以及具体规则或目录。协议中的 `always` 仍表示
应用这条建议，并不保证永久授权；多条建议不会提供含糊的批量应用按钮。
在 wmcterm 内运行的此类调用会使用相同的集中式审批，可能等待用户处理；自动审查
不等于永不等待。接受审批结果的 9 分钟时限包含注册及轮询，超时或取消后到达的
允许结果不会执行；正在执行的 IPC 和取消清理可能使 Hook 稍晚返回。在 wmcterm
外运行时，Hook 不接管 Claude 的原生权限流程。
从 GUI 首次为含 `.codex` 目录的工作区启动自动审查模式的 Codex 时，wmcterm 会在启动前
检查 Codex 用户配置中的精确项目路径。未信任的项目必须由用户选择“信任并启动”或
“不信任并继续”；前者安全合并 `~/.codex/config.toml`，后者保持项目级 config、
Hooks 和 exec policies 禁用。该人工信任入口不会暴露给 CLI、RPC 或 Agent。
Codex 原生 `codex resume` 默认按当前 cwd 过滤（`--all` 才显示全部）。wmcterm 会把
Workspace cwd 显式传给私有 app-server 的远程 TUI。Codex 0.149.1 至 0.154.0 会话内部的
`/resume` 会新建一个未继承 remote cwd 的 picker 连接，因此仍可能固定显示
`Filter: All`；这个 provider 缺陷无法由外部启动参数修复。调用者显式传入
`-C`/`--cd` 时以该目录为准。
托管根 TUI 不携带审批、沙箱或自动审查覆盖参数；权限代理在服务端的线程创建、恢复
和 fork 请求中应用所选模式，核验返回的实际权限后才允许启动 turn，因此重启恢复和
会话内 `/resume` 不再触发远程恢复的 permission overrides 错误。当前托管入口不支持
`--add-dir`、外部 `--remote` 及自定义 `permissions` 配置覆盖，遇到这些参数会明确报错。
Codex 0.158.0 的 `thread/settings/update` 和 `turn/settings/update` 同样受代理约束：
权限、审批 reviewer 和 cwd 的非空更新会在转发前被拒绝，需通过 wmcterm 以所需模式和
工作目录重新启动。模型等普通设置仍可更新；未核验的线程不能更新设置。
`turn/start` 同样不能改变已核验的 cwd 和 `runtimeWorkspaceRoots`；相同值和 null
会在转发前移除，保留服务端已核验的线程设置。旧协议未返回根路径时按空列表处理。
已对照 Codex 0.163.0 协议核验恢复订阅：watcher 不覆盖线程保存的 cwd、权限或
worktree 配置。子 Agent 窗格使用其线程的 cwd，保持 `focus: false`；只读线程
仍交由原生 TUI 展示。受支持的提问使用独立输入表单，不映射为允许/拒绝审批。
对于这些托管 app-server 会话，通用 Codex `PermissionRequest` Hook 被视为重复遥测，
不会创建第二个可操作请求。手动启动的非托管 Codex 没有审批回复通道，因此其
`PermissionRequest` Hook 只更新等待状态并发送普通通知；用户仍需回到 Codex TUI
完成原生审批，通知中心不会显示无法生效的允许/拒绝按钮。
Codex 0.167.0 的原生会话还通过 `PreToolUse` / `PostToolUse` Hook 跟踪工具活动：
匹配的工具返回后会清除原生权限等待，并恢复运行状态。并行工具不会清除其他工具的
等待，真实审批请求始终优先；旧会话、旧生命周期、子 Agent 和迟到事件不能覆盖当前状态。
拒绝审批时 Codex 不触发工具完成 Hook，后续同轮次的 `PreModelRequest` 会结束原生等待；
中断则由 `Interrupt` 结束当前轮次，保留会话。
升级后需显式运行 `wmcterm hook install --agent codex --scope user`，并在 Codex 中
审阅新增 Hook。已运行的旧实例需保存会话后重新启动，安装包不会自动更新个人 Hook。
`codex update/login/logout`、帮助、版本查询及 Claude 的更新、认证、诊断、
MCP/插件管理命令不会被标记为空闲 Agent；普通会话、resume 和 exec 仍参与识别。
前台进程识别会把手动 `codex --approve-for-me` 标记为“自动审查”，把手动
`codex --dangerously-bypass-approvals-and-sandbox` 标记为“完全访问”；运行模式
与空闲、运行中、等待和完成等 turn 状态相互独立。工作区侧栏显示其中最敏感的模式，
`UserPromptSubmit` Hook 则确认包括粘贴、RPC 输入和自动续跑在内的运行状态。
命令面板启动的托管 Codex 还通过 app-server 的 `turn/started` 和
`turn/completed` 通知同步运行、完成、中断和失败状态，远程 TUI 不依赖任务开始 Hook。
自动审查创建的临时线程不会替换工作区主会话；其 Hook 和任务状态也不会覆盖主任务状态。
如果根 Codex TUI 先回答同一个 app-server 请求，wmcterm 会消费 Codex 的
`serverRequest/resolved` 确认或精确匹配的 item lifecycle 事件，并且只关闭对应的
通知卡片；终端按键和无关 item 活动绝不会清除审批。
Codex 0.150.0 把向已运行命令发送输入表示为同一 app-server method 下的
`kind: "writeStdin"` 审批。wmcterm 会单独展示输入和进程标识，并且只允许
拒绝或本次允许。当 provider 提供 stdin 或进程标识时，技术详情会保留并
展示它们；该请求不会生成持久或会话级授权。
显式指定的审批或沙箱选项仍然优先。托管启动器拒绝 Codex/Claude 的 bypass 参数、
`danger-full-access` 沙箱和 CLI `--mode full-access`。命令面板另有明确标为“完全访问…”
的人工入口；选择后必须在危险说明弹窗中明确确认，且只影响新启动的 Agent 会话，不写入
配置。该会话关闭审批和沙箱，应仅在已有外部隔离边界时使用。

可操作的 Agent 请求使用有界请求/回复通道，而不是普通通知。使用
`wmcterm agent-request list` 查看请求，然后使用
`agent-request reply ID --approve|--deny|--choice ID|--text TEXT` 或
`agent-request cancel ID [REASON]` 处理。请求携带所属 Surface 和终端 lifecycle
标识，会安全过期，并拒绝陈旧或重复的响应。通知中心的审批卡片优先显示命令、
工具、工作目录、原因或权限等可读摘要；详情保留格式化 JSON，并提供滚轮、方向键、
Page Up/Down、Home/End、行数进度和可拖动滚动条。
卡片只呈现“拒绝”“允许一次”和 provider 明确提供时的“信任规则”；“自动审查”和
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
设置 `CODEX_HOME` 或 `CLAUDE_CONFIG_DIR` 后，对应 Agent 的 Skill 副本和用户 Hook
配置会使用该目录；更新脚本也使用同一路径，项目级安装不受影响。除非提供
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
回滚历史会保留；PTY 进程内存绝不会被序列化，命令以及 Claude/Codex 的
resume 标识符会按启动来源恢复。由 wmcterm Agent 入口启动并已取得 session ID 的
会话恢复原线程，不会重新执行初始提示词；Shell 中手动
启动的 Agent 仅在 wmcterm 关闭时生命周期仍在运行且前台进程仍是该 Agent 才恢复；
已经退出回到 Shell、明确结束或失败的手动 Agent 重开后回到普通终端。
已恢复的手动会话在没有新交互或 Hook 时继续保留恢复命令，连续重启不会因此丢失会话。
仍在运行的手动 Agent 恢复时保留交互式 Shell 语义：resume 作为 Shell 子命令运行，
退出 Agent 后回到 Shell，Surface、滚动历史和终端会话不会随 Agent 一同退出。
新版本会保存托管 Codex 的线程 ID、权限模式、权限配置、工作目录与 Teams 父子关系。
普通交互、自动审查和危险全权限均按原模式恢复，重建私有 app-server 和审批桥。
危险全权限沿用首次 GUI 确认；确认文案明确说明此会话重启后仍按该模式恢复。
CLI/RPC 仍不能直接选择危险全权限。子窗格连接新服务，不复用旧端口，不重复创建，
其权限和工作目录保持独立；缺少父会话的子窗格停留在 Shell 并显示原因。
旧会话没有托管恢复记录时仍按普通 provider resume 恢复，旧 Teams 子窗格降级为
独立会话。Shell 手动启动的 Codex 和其他 provider 保持原来的恢复策略。
默认交互模式不显示权限标签，仅为其他模式显示“自动审查”或“完全访问”，与运行中、
等待等任务状态分开。CLI 参数与会话中的 `interactive`、`unattended`、`full-access`
值保持不变。
托管模式以服务端核验结果为准；Shell 模式由进程识别，均不把权限模式当作运行状态。
设计与验证说明见 [Codex 托管恢复设计](docs/rust-libghostty-vt-winit-wgpu-terminal-design.md)。

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

Codex、Claude 等 TUI 开启鼠标协议时，滚轮、悬停和鼠标按钮优先发送给应用；
按住 Shift 可使用终端本地选择、滚动、右键菜单和中键粘贴。
`Ctrl+F` / `Ctrl+G` 默认透传给应用，自定义 `[keybindings]` 仍优先。
功能键、增强键盘的按下/重复/释放及焦点通知按终端协议处理。

此响铃策略仅适用于终端 BEL 字符。权限请求、完成和错误的显式 Agent 与 OSC 通知
不会被抑制。
托管 Codex 以及已接入 Hook 且仍有 Agent 元数据的终端使用显式通知，不再为 BEL
额外生成“终端响铃”通知，避免一次任务完成出现两条提醒。

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
# Debian/Ubuntu 依赖：xvfb xdotool imagemagick xclip
scripts/ui-smoke.sh
scripts/runtime-control-smoke.sh
python3 scripts/codex-permissions-smoke.py
python3 scripts/codex-release-smoke.py
# Checked with Codex 0.163.0; launch and restore tests never start model turns:
xvfb-run -a python3 scripts/codex-tui-launch-smoke.py
xvfb-run -a node scripts/claude-notifications-smoke.mjs target/debug/wmcterm
xvfb-run -a node scripts/claude-input-smoke.mjs target/debug/wmcterm
# 输入桥故障/容量/IME 事件由 workspace 单测覆盖；真实 PTY、表单和 GUI：
xvfb-run -a node scripts/codex-input-smoke.mjs target/debug/wmcterm
WINIT_X11_SCALE_FACTOR=2 xvfb-run -a --server-args='-screen 0 2560x1440x24' node scripts/codex-input-smoke.mjs target/debug/wmcterm /tmp/wmcterm-input-hidpi.png
# 已安装的 Codex/Claude CLI + 本地模型响应，无付费 API 调用：
xvfb-run -a python3 scripts/agent-live-contract-smoke.py target/debug/wmcterm
# Requires Codex 0.154.0+ and Node.js 22+; isolated real server, no model turns:
xvfb-run -a --server-args='-screen 0 1280x800x24' python3 scripts/codex-managed-restore-smoke.py
xvfb-run -a python3 scripts/ssh-smoke.py target/debug/wmcterm
# 指定旧版 Bash，同时验证本地与远端（不替换系统 Shell）：
WMCTERM_TEST_BASH=/path/to/bash-4.2.46/bash xvfb-run -a python3 scripts/ssh-smoke.py target/debug/wmcterm
```

生成的截图写入 `target/ui-smoke-en.png` 和 `target/ui-smoke-zh-CN.png`。运行时控制
smoke test 会通过 winit 向 shell 发送真实的键盘空格事件，通过本地 IPC socket 创建
窗格，并验证子终端是否收到 wmcterm 控制环境变量。

Codex 自动会话恢复在保存和加载时检查当前 `CODEX_HOME`（默认 `~/.codex`）
的会话记录。仅取得会话 ID、尚未落盘的空会话不会生成自动恢复命令；恢复为普通
Shell，并清除对应的旧 Agent 状态，保留工作区布局和终端历史。
