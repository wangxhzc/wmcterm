# wmcterm终端产品设计方案

> 本文前 32 节保留了项目立项时的架构推导和阶段规划。它们用于解释设计
> 取舍，不应被理解为当前实现清单；当前事实以第 33 节为准。

## 1. 产品定位

参考 cmux 的产品结构与交互思路，目标不是简单实现一个 “Rust 版 Ghostty”，而是打造一个：

> **Rust 原生、GPU 渲染、面向多终端 / 多 Agent 工作流的可编程 Terminal Workspace。**

核心技术栈：

- **Rust**：整体实现语言
- **libghostty-vt**：VT Parser、终端状态、输入编码、scrollback 等
- **winit**：窗口、输入、事件循环
- **wgpu**：GPU 渲染
- **PTY Layer**：Linux/macOS PTY、Windows ConPTY
- **Workspace Core**：工作区、Pane、Surface、通知、Agent 状态、IPC

总体分层：

```text
Rust
├── winit          OS 窗口 / 输入 / EventLoop
├── wgpu           GPU Renderer
├── libghostty-vt  VT Parser / Terminal State / Input Encoder
├── PTY Layer      forkpty / ConPTY
└── Product Core   Workspace / Pane / Agent / Notify / IPC
```

---

## 2. 产品核心理念

cmux 最值得借鉴的不是外观，而是它将终端抽象为可组合的工作空间原语。

建议整体模型：

```text
Window
  └── Workspace
        └── Pane
              └── Surface
```

产品关注点从：

```text
Terminal Tab
```

升级为：

```text
Workspace
  ├── Terminal
  ├── Agent
  ├── Dev Server
  ├── SSH
  ├── Logs
  └── Browser
```

核心问题不是：

> 我打开了多少个终端？

而是：

> 我同时运行了很多任务和 Agent，现在到底哪个需要我处理？

因此产品核心应围绕：

- Workspace
- Agent 状态
- Attention / Notification
- Pane Split
- Surface
- Command Palette
- CLI / Socket API
- Session Restore

---

## 3. 整体产品界面

推荐使用 cmux 风格的 **Vertical Workspace Sidebar**。

```text
┌───────────────────────────────────────────────────────┐
│ Terminal Workspace                                    │
│                                                       │
│ ┌────────────┐ ┌────────────────────────────────────┐ │
│ │ Workspaces │ │ Workspace: ctsp                    │ │
│ │            │ │                                    │ │
│ │ ● ctsp     │ │ ┌─────────────┬──────────────────┐ │ │
│ │   infra    │ │ │ Claude Code │ npm run dev      │ │ │
│ │ ! auth     │ │ │             │                  │ │ │
│ │ ✓ web      │ │ │             ├──────────────────┤ │ │
│ │   mysql    │ │ │             │ SSH production   │ │ │
│ │            │ │ └─────────────┴──────────────────┘ │ │
│ │ + New      │ │                                    │ │
│ └────────────┘ └────────────────────────────────────┘ │
│                                                       │
│ Ctrl+Shift+P Command Palette     Agent 2 running      │
└───────────────────────────────────────────────────────┘
```

Workspace Sidebar 可以显示：

```text
● Running
! Needs Input
✓ Completed
× Error
○ Idle
```

---

# 4. 核心数据模型

## 4.1 App 层级

建议采用：

```text
App
└── Window
    └── Workspace
        └── PaneTree
            └── Pane
                └── Surface
                    ├── Terminal
                    ├── Browser
                    └── Markdown
```

统一使用 UUID：

```rust
type WindowId = Uuid;
type WorkspaceId = Uuid;
type PaneId = Uuid;
type SurfaceId = Uuid;
type TerminalId = Uuid;
```

---

## 4.2 Workspace

```rust
struct Workspace {
    id: WorkspaceId,
    name: String,

    root: PaneNode,

    cwd: PathBuf,

    active_pane: PaneId,

    status: WorkspaceStatus,

    environment: HashMap<String, String>,

    metadata: WorkspaceMetadata,
}
```

Workspace 应作为产品的核心管理单元。

---

## 4.3 Pane Tree

不要使用二维数组表示 Pane。

推荐使用 Split Tree：

```rust
enum PaneNode {
    Leaf {
        pane: PaneId,
    },

    Split {
        axis: SplitAxis,
        ratio: f32,

        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}

enum SplitAxis {
    Horizontal,
    Vertical,
}
```

示例：

```text
        Vertical 0.65
        /           \
   Terminal A     Horizontal
                  /        \
             Terminal B   Terminal C
```

优点：

- Pane split 简单
- Resize 简单
- Close Pane 简单
- Pane Zoom 简单
- Session Restore 简单
- Drag Divider 简单

---

# 5. Surface 模型

Pane 内可以存在多个 Surface：

```rust
struct Pane {
    id: PaneId,

    surfaces: Vec<SurfaceId>,
    active_surface: usize,
}
```

Surface：

```rust
struct Surface {
    id: SurfaceId,

    title: String,

    kind: SurfaceKind,

    attention: AttentionState,

    agent: Option<AgentMetadata>,
}
```

第一阶段：

```rust
enum SurfaceKind {
    Terminal(TerminalId),
}
```

未来：

```rust
enum SurfaceKind {
    Terminal(TerminalId),

    Browser(BrowserId),

    Markdown(MarkdownId),
}
```

不建议单独设计：

```text
AgentSurface
```

更推荐：

```text
Terminal Surface
       +
Agent Metadata
```

原因是：

- Claude Code
- Codex
- OpenCode
- Gemini CLI
- Aider

本质上都还是运行在 PTY 中的 CLI / TUI。

---

# 6. Terminal Core

Terminal Core 必须独立于 GUI。

```text
terminal-core
│
├── TerminalSession
├── Pty
├── libghostty_vt::Terminal
├── TerminalEvent
├── InputEncoder
├── Selection
└── Scrollback
```

数据关系：

```text
             child process
                  │
                  │ stdout
                  ↓
             ┌─────────┐
             │   PTY   │
             └────┬────┘
                  │ bytes
                  ↓
        ┌──────────────────┐
        │ libghostty-vt    │
        │                  │
        │ VT Parser        │
        │ Screen/Grid      │
        │ Cursor           │
        │ Modes            │
        │ Scrollback       │
        └────────┬─────────┘
                 │
                 │ RenderState
                 ↓
        ┌──────────────────┐
        │ TerminalRenderer │
        │      wgpu        │
        └──────────────────┘
```

需要明确：

```text
libghostty-vt != Ghostty terminal
```

真正的终端是：

```text
PTY
 +
libghostty-vt
 +
Font Engine
 +
wgpu Renderer
 +
winit
```

---

# 7. 多线程模型

推荐：

```text
                    Main/UI Thread
                         │
                  winit EventLoop
                         │
                 App / Workspace
                         │
              ┌──────────┴──────────┐
              ↓                     ↓
        Render Manager          Command Bus
              │                     │
              │                     │
              ↓                     ↓
            wgpu             Terminal Threads
                                  │
                   ┌──────────────┼─────────────┐
                   ↓              ↓             ↓
                 Term A         Term B         Term C
                   │              │             │
              libghostty-vt  libghostty-vt  libghostty-vt
                   │              │             │
                  PTY            PTY           PTY
```

## UI Thread

负责：

- winit
- Workspace state
- Pane layout
- Focus
- Keyboard routing
- Mouse
- wgpu rendering

---

## Terminal Worker

每个 Terminal 使用独立 Actor：

```rust
enum TerminalCommand {
    Input(Vec<u8>),

    Resize {
        cols: u16,
        rows: u16,
    },

    Scroll(i32),

    Select(Selection),

    Shutdown,
}
```

Terminal 返回：

```rust
enum TerminalEvent {
    Dirty(TerminalId),

    TitleChanged(String),

    Bell,

    CwdChanged(PathBuf),

    Notification(Notification),

    ProcessExited(i32),
}
```

建议通信：

```text
crossbeam-channel
```

或者：

```text
flume
```

---

# 8. Frame Scheduler

不要让 PTY 每次输出都触发重绘。

错误：

```text
PTY bytes
 ↓
VT update
 ↓
request_redraw()
```

大量输出时 UI 会被 EventLoop 消息打爆。

正确：

```text
PTY bytes
 ↓
VT update
 ↓
dirty = true
 ↓
Wake UI
 ↓
FrameScheduler
 ↓
最多每 frame render 一次
```

例如：

```text
Terminal Worker

    5000 次 update
         │
         ↓
   dirty.exchange(true)
         │
         ↓
只有 false -> true 时
         │
         ↓
 EventLoopProxy::send_event()
```

---

# 9. wgpu Renderer

建议模块：

```text
renderer
│
├── Renderer
├── TerminalRenderer
├── UiRenderer
├── GlyphAtlas
├── GlyphCache
├── FontSystem
├── CellPipeline
├── ImagePipeline
└── Compositor
```

一帧：

```text
libghostty RenderState
        │
        ↓
TerminalSceneBuilder
        │
        ├── Background rects
        ├── Glyphs
        ├── Decorations
        ├── Selection
        ├── Cursor
        └── Images
        │
        ↓
GPU batches
        │
        ↓
wgpu
        │
        ↓
SurfaceTexture
```

---

# 10. Glyph Renderer

不建议直接使用 egui Text 渲染 Terminal。

推荐：

```text
fontdb
+
rustybuzz
+
swash
+
GlyphAtlas
```

MVP 也可以考虑：

```text
cosmic-text
+
swash
```

终端需要处理：

- 固定 Cell Grid
- CJK
- Emoji
- Combining characters
- Ligature
- Bold
- Italic
- Underline
- Strikethrough
- Font fallback
- Powerline
- Nerd Font

核心结构：

```rust
struct CellMetrics {
    width: f32,
    height: f32,

    baseline: f32,

    underline_y: f32,
}
```

GPU instance：

```rust
struct GlyphInstance {
    position: [f32; 2],

    size: [f32; 2],

    uv: [f32; 4],

    color: [f32; 4],
}
```

例如：

```text
200 columns × 60 rows
=
12,000 cells
```

不能产生 12,000 个 Draw Call。

建议：

```text
Backgrounds:   1 draw
Glyphs:        1~N draw
Decorations:   1 draw
Cursor:        1 draw
```

使用 GPU Instance Batching。

---

# 11. Glyph Atlas

```text
                  GlyphAtlas

┌─────────────────────────────────┐
│ A B C D 中文 glyphs Nerd Icons │
│ █ ░ ● ▶ ...                     │
│                                 │
│ Emoji atlas                     │
└─────────────────────────────────┘
```

Key：

```rust
struct GlyphKey {
    font_id: FontId,
    glyph_id: u32,
    font_size: u16,

    style: FontStyle,
}
```

缓存：

```rust
HashMap<GlyphKey, AtlasEntry>
```

建议：

```text
灰度字符
    ↓
R8Unorm Atlas

彩色 Emoji
    ↓
RGBA Atlas
```

---

# 12. Render Snapshot

Renderer 不应该直接访问 Terminal。

推荐：

```text
Terminal
    │
    │ snapshot
    ▼
RenderSnapshot
    │
    ▼
Renderer
```

不要：

```text
Renderer
   │
 lock
   ▼
Terminal
```

否则容易变成：

```text
PTY Thread
     ↓
Terminal Lock

Renderer
     ↓
Terminal Lock

Resize
     ↓
Terminal Lock
```

建议 Terminal Worker 产生 RenderSnapshot。

---

# 13. Terminal 数据流

## 输入

```text
Keyboard
   │
   ↓
winit::KeyboardInput
   │
   ↓
KeyRouter
   │
   ├── App shortcut ───────→ Workspace Command
   │
   └── Terminal input
              │
              ↓
      libghostty-vt KeyEncoder
              │
              ↓
             PTY
              │
              ↓
            shell
```

---

## 输出

```text
shell / Agent
      │
      ↓
     PTY
      │
      ↓
Terminal Worker
      │
      ↓
libghostty-vt::Terminal
      │
      ├──── OSC event ───→ App
      │
      ↓
RenderSnapshot
      │
      ↓
FrameScheduler
      │
      ↓
wgpu Renderer
```

---

# 14. Keyboard 路由

快捷键必须两级路由。

```text
KeyboardInput
     │
     ↓
Keybinding Resolver
     │
     ├── Global action
     │
     ├── Workspace action
     │
     ├── Pane action
     │
     └── passthrough
            ↓
       libghostty-vt
            ↓
           PTY
```

配置示例：

```toml
[keybindings]

"ctrl+shift+p" = "command_palette"
"ctrl+shift+d" = "split:right"
"ctrl+shift+e" = "split:down"

"alt+1" = "workspace:1"

"ctrl+shift+w" = "surface:close"
```

---

# 15. Agent 状态模型

建议：

```rust
enum AgentState {
    Unknown,

    Idle,

    Running,

    WaitingForInput,

    WaitingForPermission,

    Completed,

    Error,
}
```

UI：

```text
● Running
! Waiting
✓ Done
× Error
```

Agent 状态检测分三级。

---

## Level 1：OSC

可以支持：

```text
OSC 9
OSC 99
OSC 777
```

用于：

- notification
- attention
- title
- task completion

---

## Level 2：Agent Hooks

例如：

```text
Claude Code hook
Codex hook
OpenCode hook
Gemini hook
```

事件：

```text
Stop
ToolUse
PermissionRequest
SessionStart
```

通过 CLI：

```text
wmcterm agent-event
```

发送到当前 Terminal。

---

## Level 3：Socket API

例如：

```json
{
  "method": "agent.update",
  "params": {
    "surface": "...",
    "state": "waiting_for_input"
  }
}
```

这样无需解析 Terminal 屏幕文本。

---

# 16. Notification Center

建议 MVP 就支持。

```text
┌────────────────────────────────┐
│ Needs Attention              3 │
├────────────────────────────────┤
│ ! Claude / ctsp                │
│   Permission required          │
│                         12:31  │
│                                │
│ ✓ Codex / wmcssh               │
│   Task completed               │
│                         12:27  │
│                                │
│ × npm test                     │
│   exited with code 1           │
└────────────────────────────────┘
```

Workspace：

```text
ctsp        ●
wmcssh      1
gluster     !
```

点击通知：

```text
Window
 ↓
Workspace
 ↓
Pane
 ↓
Surface
 ↓
Focus Terminal
```

---

# 17. Socket API

建议 MVP 就实现。

Linux/macOS：

```text
~/.local/run/wmcterm.sock
```

协议建议 JSON-RPC。

请求：

```json
{
  "id": "1",
  "method": "workspace.create",
  "params": {
    "cwd": "/code/ctsp"
  }
}
```

响应：

```json
{
  "id": "1",
  "result": {
    "workspace_id": "..."
  }
}
```

---

# 18. CLI

假设 CLI 名称为：

```bash
wmcterm
```

Workspace：

```bash
wmcterm workspace list

wmcterm workspace new \
    --cwd ~/code/ctsp \
    --name ctsp
```

Split：

```bash
wmcterm pane split --right
wmcterm pane split --down
```

创建 Surface：

```bash
wmcterm surface new \
    --command "claude"
```

输入：

```bash
wmcterm send --surface xxx "帮我检查这个项目"
wmcterm send-key --surface xxx Enter
```

读取屏幕：

```bash
wmcterm screen read --surface xxx
```

通知：

```bash
wmcterm notify \
    --title "Claude" \
    --body "Waiting for approval"
```

整体关系：

```text
Agent
   │
   ↓
wmcterm CLI
   │
   ↓
Unix Socket
   │
   ↓
App Command Bus
   │
   ↓
Workspace Manager
```

---

# 19. Command Palette

快捷键建议：

```text
Ctrl + Shift + P
```

支持：

```text
> new workspace
> split right
> split down
> open project
> new claude
> new codex
> connect ssh
> toggle sidebar
> zoom pane
> rename workspace
> close surface
```

架构：

```rust
trait Action {
    fn id(&self) -> &'static str;

    fn title(&self) -> &'static str;

    fn execute(
        &self,
        app: &mut AppContext,
        ctx: ActionContext,
    );
}
```

所有操作统一走 Action。

```text
Keyboard
Command Palette
Menu
CLI
Socket
Agent
        │
        ↓
ActionRegistry
```

---

# 20. Command Architecture

最终建议：

```text
             Keyboard
                │
              Mouse
                │
         Command Palette
                │
               CLI
                │
             Socket
                │
                ▼
        ┌──────────────┐
        │ ActionRouter │
        └───────┬──────┘
                │
                ▼
            AppCommand
                │
        ┌───────┼──────────┐
        ↓       ↓          ↓
   Workspace   Pane     Terminal
   Manager    Manager    Manager
```

Rust：

```rust
enum AppCommand {
    Workspace(WorkspaceCommand),

    Pane(PaneCommand),

    Surface(SurfaceCommand),

    Terminal(TerminalCommand),

    App(AppAction),
}
```

不要为：

- 快捷键
- CLI
- UI Button
- Socket

分别实现一套逻辑。

---

# 21. Session Restore

Session Restore 需要从项目早期设计。

保存：

```text
~/.local/share/wmcterm/state.json
```

示例：

```json
{
  "windows": [
    {
      "workspaces": [
        {
          "id": "...",
          "name": "ctsp",
          "cwd": "/code/ctsp",

          "layout": {
            "split": "horizontal",
            "ratio": 0.6
          }
        }
      ]
    }
  ]
}
```

Terminal：

```rust
struct ResumeInfo {
    cwd: PathBuf,

    command: Option<String>,

    agent: Option<AgentResumeInfo>,
}
```

Agent：

```rust
enum AgentResumeInfo {
    Claude {
        session_id: String,
    },

    Codex {
        session_id: String,
    },

    OpenCode {
        session_id: String,
    },
}
```

恢复流程：

```text
Restore Layout
     ↓
Restore Terminal
     ↓
存在 Agent Session
     ↓
Agent Specific Resume
```

不要尝试恢复 PTY 进程内存。

---

# 22. 配置系统

配置文件：

```text
~/.config/wmcterm/config.toml
```

示例：

```toml
[terminal]

font-family = "Maple Mono NF CN"
font-size = 14

# Bytes, matching Ghostty's scrollback-limit semantics (128 MB by default).
scrollback = 128000000
# Logical history row limit; the byte or row limit may prune first.
scrollback-lines = 100000


[window]

decorations = true

opacity = 1.0


[sidebar]

position = "left"

width = 260


[workspace]

inherit-cwd = true


[agent]

notifications = true


[keybindings]

"ctrl+shift+p" = "command_palette"

"ctrl+shift+d" = "split:right"

"ctrl+shift+e" = "split:down"
```

---

# 23. 项目配置

项目目录中支持：

```text
project/
└── .wmcterm.toml
```

例如：

```toml
[workspace]

name = "ctsp"


[[command]]

name = "Claude"

command = "claude"


[[command]]

name = "Dev Server"

command = "docker compose up"
```

进入项目：

```bash
wmcterm .
```

自动产生：

```text
Workspace: ctsp

├─ Claude
├─ Dev Server
└─ Shell
```

---

# 24. Rust Workspace 工程结构

建议：

```text
wmcterm/
│
├── Cargo.toml
│
├── crates/
│
│   ├── wmcterm-app/
│   │   └── winit ApplicationHandler
│   │
│   ├── wmcterm-core/
│   │   ├── Workspace
│   │   ├── Pane
│   │   ├── Surface
│   │   └── Command
│   │
│   ├── wmcterm-terminal/
│   │   └── libghostty-vt wrapper
│   │
│   ├── wmcterm-pty/
│   │   └── PTY abstraction
│   │
│   ├── wmcterm-renderer/
│   │   ├── wgpu
│   │   ├── glyph atlas
│   │   └── terminal renderer
│   │
│   ├── wmcterm-ui/
│   │   ├── Sidebar
│   │   ├── Tabbar
│   │   ├── CommandPalette
│   │   └── Notifications
│   │
│   ├── wmcterm-agent/
│   │   ├── Claude
│   │   ├── Codex
│   │   └── OpenCode
│   │
│   ├── wmcterm-ipc/
│   │   ├── Unix Socket
│   │   └── JSON RPC
│   │
│   ├── wmcterm-config/
│   │
│   └── wmcterm-session/
│
└── src/
    └── main.rs
```

建议避免整个项目依赖：

```rust
Arc<Mutex<App>>
```

到处互相调用。

---

# 25. 依赖建议

示例：

```toml
[dependencies]

# window
winit = "0.30"

# GPU
wgpu = "30"

# VT
libghostty-vt = "0.2"

# runtime
tokio = { version = "...", features = ["rt-multi-thread"] }

# channels
flume = "..."

# font
fontdb = "..."
rustybuzz = "..."
swash = "..."

# config
serde = { version = "...", features = ["derive"] }
toml = "..."

# id
uuid = { version = "...", features = ["v4", "serde"] }

# logs
tracing = "..."
tracing-subscriber = "..."

# ipc
interprocess = "..."

# data
slotmap = "..."
```

建议针对 `libghostty-vt` 再封装一层：

```text
wmcterm-terminal
```

不要让：

```rust
libghostty_vt::*
```

扩散到整个项目。

这样未来 libghostty-vt API 变化时：

```text
只修改 wmcterm-terminal
```

---

# 26. 总体架构

```text
┌────────────────────────────────────────────────────────────┐
│                         wmcterm                              │
│                                                            │
│ ┌──────────────── Product Layer ─────────────────────────┐ │
│ │                                                       │ │
│ │ Workspace │ Pane │ Surface │ Agent │ Notification     │ │
│ │                                                       │ │
│ │ Command Palette │ Session │ Config │ Actions          │ │
│ └───────────────────────┬───────────────────────────────┘ │
│                         │                                  │
│                 ┌───────▼────────┐                         │
│                 │ Command Router │◄──── CLI                │
│                 └───────┬────────┘      Unix Socket        │
│                         │                                  │
├─────────────────────────┼──────────────────────────────────┤
│                         │                                  │
│             Terminal Runtime Layer                         │
│                         │                                  │
│          ┌──────────────▼──────────────┐                   │
│          │      TerminalManager       │                   │
│          └──────────────┬──────────────┘                   │
│                         │                                  │
│             ┌───────────┼───────────┐                      │
│             ▼           ▼           ▼                      │
│          Term #1     Term #2      Term #N                  │
│             │           │           │                      │
│       libghostty-vt libghostty-vt libghostty-vt           │
│             │           │           │                      │
│            PTY         PTY         PTY                     │
│                                                            │
├────────────────────────────────────────────────────────────┤
│                    Presentation Layer                      │
│                                                            │
│                      winit                                 │
│                        │                                   │
│                  Layout Engine                             │
│                        │                                   │
│                  Scene Builder                             │
│                        │                                   │
│                       wgpu                                 │
│                                                            │
│       Glyph │ Background │ Cursor │ UI │ Images           │
└────────────────────────────────────────────────────────────┘
```

---

# 27. MVP 范围

第一阶段不要做：

```text
Browser
SSH Manager UI
File Browser
AI Chat
Markdown
Settings GUI
Plugin Marketplace
Cloud Sync
```

MVP 只实现：

1. PTY
2. libghostty-vt
3. wgpu Terminal Rendering
4. winit Window/Input
5. Vertical Workspace Sidebar
6. Pane Split
7. Notification / Attention
8. CLI + Unix Socket

这样就已经不是普通 Rust Terminal Demo，而是：

> **cmux-style Agent Terminal for Rust**

---

# 28. 开发阶段

## Phase 0：libghostty-vt PoC

目标：

```text
PTY
 ↓
libghostty-vt
 ↓
Dump Screen
```

验证：

- VT parser
- Screen state
- resize
- keyboard encoding

---

## Phase 1：单 Terminal

```text
PTY
 ↓
libghostty-vt
 ↓
wgpu
 ↓
Single Terminal
```

验收：

```text
bash
vim
tmux
htop
Claude Code
Codex
```

---

## Phase 2：Workspace

实现：

```text
Workspace
PaneTree
Surface
```

支持：

- Vertical Workspace Sidebar
- Split Right
- Split Down
- Resize Pane
- Surface Tabs

---

## Phase 3：Command System

实现：

```text
Action Registry
Command Router
CLI
Unix Socket
Command Palette
```

---

## Phase 4：Agent

实现：

```text
OSC Notification
Agent Hooks
Agent State
Attention State
Notification Center
```

---

## Phase 5：Session

实现：

```text
Session Restore
Project Config
Agent Resume
```

---

## Phase 6：扩展 Surface

未来：

```text
Browser
Markdown
SSH
Docker Exec
Kubernetes Exec
Logs
Remote
```

---

# 29. 最早需要验证的三个技术风险

## 29.1 libghostty-vt 生命周期和线程模型

优先验证：

```text
PTY Thread
+
Terminal Owner Thread
+
Render Snapshot
+
UI Channel
```

不要等工程很大以后再调整。

---

## 29.2 Font / Glyph Rendering

重点验证：

```text
中文
Maple Mono
Nerd Font
Emoji
Combining Characters
Ligatures
```

这是 wgpu Terminal Renderer 的核心难点。

---

## 29.3 高吞吐输出

测试：

```bash
yes
```

以及：

```bash
cat large.log
```

需要保证：

```text
PTY Output
     ↓
VT Parsing
     ↓
Dirty Coalescing
     ↓
60 / 120 FPS UI
```

而不是：

```text
每几十字节 repaint 一次
```

---

# 30. 最核心的架构原则

不要设计成：

```text
Rust Ghostty Clone
       +
Sidebar
```

应该设计成：

```text
              Workspace Core
                    │
       ┌────────────┼────────────┐
       ↓            ↓            ↓
    Terminal       Agent       Browser
       │
 libghostty-vt
       │
      PTY
```

即：

> **Workspace 是产品核心，Terminal 是一种 Surface。**

未来才能自然增加：

```text
Terminal
Browser
Markdown
Logs
Agent Dashboard
Remote Shell
SSH
Docker Exec
Kubernetes Exec
Serial
```

而无需推翻整个架构。

---

# 31. 最终推荐产品形态

```text
                         WMCTERM

 ┌──────────────────────────────────────────────────────────┐
 │ Workspaces        │  ctsp                     Claude ●   │
 │───────────────────│──────────────────────────────────────│
 │ ● CTSP            │ Surface: Claude │ Shell │ Server    │
 │   Claude Running  │──────────────────────────────────────│
 │                   │                                      │
 │ ! GlusterFS       │ $ claude                             │
 │   Needs Input     │                                      │
 │                   │ > 帮我分析当前项目...                │
 │ ✓ wmcssh          │                                      │
 │   Completed       │                                      │
 │                   │                                      │
 │   monitoring      │                                      │
 │                   │                                      │
 │───────────────────│──────────────────────────────────────│
 │ + New Workspace   │ Agent: Running   cwd: ~/code/ctsp   │
 └──────────────────────────────────────────────────────────┘

             Ctrl+Shift+P Command Palette
                         │
                         ↓
                    Action System
                         │
             ┌───────────┼───────────┐
             ↓           ↓           ↓
            CLI        Socket       UI
```

---

# 32. 项目目标总结

第一版明确限定为：

> **cmux 的 Workspace / Agent 体验 + Ghostty 级 VT 兼容性 + Rust / wgpu 跨平台底座。**

核心优先级：

```text
Terminal Compatibility
        ↓
GPU Performance
        ↓
Workspace UX
        ↓
Agent Awareness
        ↓
Programmability
```

而不是：

```text
Browser
IDE
AI Chat
Plugin Marketplace
```

先把 Terminal Workspace 做深，再向完整的 Agent Workspace 扩展。

---

# 33. 当前实现基线（2026-08-19）

本节记录能够从当前源码、测试和公开命令直接验证的事实。前文使用“建议”、
“第一阶段”或“未来”的内容若与本节冲突，以本节和代码为准。

## 33.1 已落地的产品与线程模型

- 产品层级已实现为 `AppState -> Workspace -> PaneNode -> Pane -> Surface`，
  全部实体使用 UUID；Pane 使用可递归的 Split Tree，支持四向分割、比例调整、
  关闭、聚焦与缩放。
- 每个 Terminal 由独立 PTY/VT actor 持有可变状态，`wmcterm-app` 的 winit UI
  消费不可变快照并合并唤醒，避免 UI 线程直接读写 VT 状态。
- Linux/macOS/Windows 进程抽象由 `portable-pty` 提供，终端解析和输入编码由
  `libghostty-vt` 提供；渲染由 `wgpu` 矩形和 `glyphon` 字形管线完成。
- Terminal 支持主/备用屏、scrollback、ANSI/SGR 颜色、鼠标协议、选择、搜索、
  Unicode 宽度、CJK/emoji/组合字符以及主题颜色查询；拖选越过内容区上下边界时
  会连续滚动，越界距离决定滚动速度，回到内容区或松开鼠标后立即停止。
- VT 生成的 PTY 协议应答按输出块立即转发，不等待输出洪峰和渲染快照合并结束；
  连续输出期间的应答字节与待写批次数量均有界，静默 250 ms 后恢复预算。历史
  回放只恢复屏幕内容，禁止向新 PTY 重放查询应答或其他 VT side effect。
- GUI 单行编辑器与终端输入路径相互独立。GUI 光标是覆盖文本的块状矩形，
  选区使用主题的 selection foreground/background 反色，不向字符串插入光标
  字符，因此移动光标不会改变文字布局。

## 33.2 Surface 与界面能力

当前核心状态允许 `Terminal`、`Markdown`、`Logs` 和 `Browser` 四种 Surface
类型，但成熟度不同：

- Terminal 是完整的交互式 Surface，可运行 shell、Agent、SSH、Docker 和
  Kubernetes 命令。
- Markdown 支持文件刷新、结构化块渲染与搜索。
- Logs 支持有界历史、文件 tail、截断/替换检测、级别过滤和保留源行号的搜索。
- Browser 只有平台无关的占位后端，没有嵌入式 WebView；产品行为是由外部系统
  应用打开 URL，嵌入式浏览器和自动更新仍明确不在当前范围内。

无边框 winit 窗口已经包含工作区侧栏、Surface 标签、Pane 分隔条、导航栏、
命令面板、终端查找、通知中心、Agent 授权卡片和双语（英文/简体中文）界面。
工作区筛选、命令面板查询、终端/文档查找和通用提示框共用单行 UTF-8 编辑模型；
当内容宽于输入区域时，界面按终端显示单元格建立光标跟随的水平视口。文本、选区、
鼠标命中、拖选、光标覆盖层和 IME 候选位置使用同一视口偏移，完整输入仍保留在模型
中，不因可见宽度截断，并避免在 CJK、emoji 或组合字符的 UTF-8 边界内切分。
点击 GUI 标题栏关闭按钮或其他交互式关闭入口时，如果会终止活动进程、Agent
或待处理请求，会显示确认；明确指定 UUID 的 CLI/RPC Agent 操作不弹出页面确认。

## 33.3 CLI、IPC 与 Agent 控制面

- 本地控制面使用 JSON-RPC，协议能力由 `wmcterm capabilities` 自描述；当前
  协议版本为 2。
- CLI/RPC 覆盖 workspace、pane、surface、screen、terminal、notification、
  Agent request/approval、配置、Hook、Skill 和远程命令入口。
- wmcterm 创建的终端导出 `WMCTERM_WORKSPACE_ID`、`WMCTERM_PANE_ID`、
  `WMCTERM_SURFACE_ID`、`WMCTERM_TERMINAL_LIFECYCLE_ID` 和控制 socket。
  显式目标优先于 UI 焦点；`--no-focus` 创建不会偷取调用者焦点。
- `wmcterm send` 使用 bracketed-paste 协议传送普通或长文本；Enter 等独立按键
  由 `wmcterm send-key` 编码，避免粘贴文本和控制键语义混淆。
- 普通通知与可操作授权请求是两条数据通道。“全部已读”只改变普通通知的未读
  状态，不删除通知，也不解决或隐藏 Agent 请求。
- Agent 授权采用有界、可过期、幂等且 fail-closed 的请求/回复通道。请求绑定
  Surface、进程和 Terminal lifecycle，拒绝旧进程 Hook、重复或含糊回复以及
  bypass/yolo 模式。通知中心可查看完整请求详情并执行受支持的选择。
- 待处理请求队列是审批状态的权威来源。每次注册、回复、取消、超时或进程退出
  都按 Surface 聚合剩余请求并重新派生 Agent、attention 和 Workspace 状态；权限
  请求优先显示 `WaitingForPermission`，其他可操作请求显示 `WaitingForInput`，仅在
  最后一个请求结束后恢复 AgentSession 的真实状态。普通通知已读操作不会清除仍
  在等待的审批状态。
- Codex TUI 与通知中心可同时看到同一 app-server 审批。若用户先在终端完成选择，
  watcher 在收到 Codex 的 `serverRequest/resolved` 明确回执，或 thread/turn/item ID
  精确匹配的 item started/completed 生命周期事件后取消对应待处理卡片；不会根据
  键盘输入或无关 item 活动推断审批结果。
- watcher 初始化时恢复已加载 thread，并在收到携带 thread ID 的 `thread/*` 通知后立即通过
  `thread/resume { excludeTurns: true }` 恢复新建或由 `/resume` 选中的 thread；
  WebSocket 重连后重新建立这些订阅。`thread/read` 只能观察通知，不能让 watcher
  接收发往 thread 客户端的 app-server 审批请求，因此不能用于审批桥。
- watcher 从所有携带 thread ID 的 `thread/*` 通知发现新目标，而不是只依赖
  `thread/started`；同时周期性回查 `thread/loaded/list` 并同步确认每次
  `thread/resume` 成功，以恢复丢失或版本差异导致的 resume 通知。
- 受管 `codex-teams` 的 app-server 会为 Hook 子进程导出内部 approval-bridge 标记；
  此时通用 `PermissionRequest` Hook 仅作为重复遥测确认，不再创建无法关联完成回执的
  `permission:*` 请求。非受管 Codex Hook 仍按原语义上报权限提醒。
- 已实现 Claude Code Hook、Codex app-server、OSC 9/99、wmcterm OSC 777，
  并可归一化 Claude、Codex、OpenCode、Gemini 等 Agent 生命周期事件。
  Claude 内部 subagent 仍属于其 Claude 进程；独立可见 Agent 通过受管命令在
  新 Pane/Surface 启动。实验性 `codex-teams` 可将 Codex 子线程映射为原生 Pane。

## 33.4 配置、恢复与发布

- 配置由用户级 `wmcterm/config.toml` 与项目级 `.wmcterm.toml` 分层加载；
  支持主题、256 色 palette、cursor/selection 颜色、字体、
  快捷键、scrollback、侧栏、工作区 cwd、通知、语言和窗口选项。
- `config import-ghostty` 读取 Ghostty 配置并输出合并预览，不覆盖原配置。
- 会话恢复持久化 Workspace/Pane/Surface 布局、可见 scrollback、命令和支持的
  Agent resume ID；不会序列化 PTY 进程内存，恢复时重新启动对应命令。
- 仓库包含 Linux x86_64 Debian 与 Windows x86_64 NSIS 打包脚本、桌面入口、
  应用图标，以及作为默认字体随包分发的 Maple Mono Normal NF（含 OFL 许可）。
- Agent Skill 的唯一源码位于 `integrations/skills/wmcterm/SKILL.md`，Hook
  协议、事件清单与配置片段生成器位于 `crates/wmcterm-agent`。`.agents`、
  `.codex` 和 `.claude` 下的 wmcterm 文件只是安装/发现目标，不作为源码维护；
  更新脚本同步用户级 Codex/Claude Skill 与 Hook，同时保留不属于
  wmcterm 的已有配置。

## 33.5 当前 Rust workspace 与验证基线

当前 workspace 共有根包和 12 个内部 crate：

```text
wmcterm
├── wmcterm-core       状态模型、Split Tree、命令与 Agent 生命周期
├── wmcterm-pty        PTY / ConPTY actor
├── wmcterm-terminal   libghostty-vt actor 与 render snapshot
├── wmcterm-renderer   wgpu + glyphon
├── wmcterm-app        winit UI、输入、调度与 RPC 分发
├── wmcterm-ipc        本地 JSON-RPC 与 actionable request
├── wmcterm-agent      OSC、Hook 与 Agent 事件归一化
├── wmcterm-codex      Codex app-server / approval bridge
├── wmcterm-claude     Claude Hook approval bridge
├── wmcterm-config     分层配置与 Ghostty 导入
├── wmcterm-session    会话持久化
└── wmcterm-surface    Markdown、Logs 与 Browser 占位后端
```

项目使用 Rust 2024 edition，声明的最低 Rust 版本为 1.90。代码变更的完整本地
门禁为：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release --bin wmcterm
```

涉及真实窗口、IPC、窗口状态、Agent 更新器或跨平台发布时，再分别运行
`scripts/ui-smoke.sh`、`scripts/runtime-control-smoke.sh`、
`scripts/window-state-smoke.sh`、`scripts/test-update-agent-integration.sh`、
Windows cross-check 或 `scripts/package-release.sh`。这些属于环境相关验证，不能
用纯单元测试的通过替代。

## 33.6 已修复缺陷：跨工作区审批阻塞模态编辑

修复状态（2026-08-20）：审批注册仅在请求属于当前 Surface 且没有活动文本
编辑器时自动展开通知面板；键盘路由同时保证 `TextPrompt`、工作区搜索、命令
面板和终端查找优先于异步展开的通知面板。非当前 Surface 的请求仍更新对应
Agent/Workspace attention，并按配置发送原生通知，但不会抢占键盘或 IME。

已知问题：用户正在修改当前工作区路径时，如果其他工作区存在待审批的
Agent 请求，当前路径编辑窗口可能卡死或无法继续输入、提交、取消及执行
其他操作。这是错误的跨工作区交互耦合：非当前工作区的待处理请求可以触发
attention 和通知更新，但不应抢占当前模态编辑器的输入归属，也不应使其进入
不可退出状态。

修复时需要同时检查同类问题，不应只覆盖“修改工作区路径”一个入口：

- 审计待审批请求注册、更新、超时、取消和 Agent 状态重算时的焦点与模态层
  切换，确保状态更新按请求所属 Workspace/Pane/Surface 隔离。
- 覆盖重命名、描述、路径、目录查找、日志过滤等共用 `TextPrompt` 的入口，以及
  工作区搜索、命令面板和终端查找等其他文本编辑状态。
- 检查通知中心、Agent 审批卡片、关闭确认和其他模态层在非当前工作区事件到达时
  是否也会错误抢占键盘、IME、鼠标或 Escape/Enter 路由。
- 增加至少两个工作区的回归测试：在工作区 A 保持编辑器打开并输入未提交文本，
  在工作区 B 注册、更新和解决审批请求；验证 A 的焦点、文本、IME 预编辑、光标与
  提交/取消行为始终不受影响，而 B 的审批状态仍正确可见、可操作。

验收条件是：任意非当前工作区的 Agent 请求生命周期变化都不会中断当前文本
编辑会话；当前工作区的审批仍按明确的交互规则呈现，且所有审批决策继续
fail closed。

## 33.7 已修复缺陷：异步诊断输出覆盖 Codex 输入框

修复状态（2026-08-20）：确认示例中的日志来自 `wmcterm codex-teams` 启动的私有
Codex app-server；该辅助进程此前继承了交互式 PTY 的 stderr。现在其 stdin、
stdout 和 stderr 均不连接前台 PTY，延迟模型刷新诊断不会再注入根 Codex TUI
或其他终端。根 Codex 进程本身仍保留标准流继承，以维持正常交互。

已知问题：Codex 完成一次任务并恢复输入框后，后台模型刷新子进程的延迟
错误输出会直接写入当前终端，例如：

```text
2026-08-20T03:08:09.769966Z ERROR codex_models_manager::manager: failed to refresh available models: timeout waiting for child process to exit
```

该行会覆盖 Codex 的输入框，造成输入区域视觉损坏，并可能使 TUI 内部布局与
wmcterm 当前显示的 VT 状态不一致。需先区分该日志是 Codex 本身向 PTY 写入的
普通字节，还是 wmcterm 的 Agent 集成、watcher 或子进程管理将诊断误路由到了前台
Terminal Surface。

后续观察到相同的 `codex_models_manager::manager` 超时日志会在等待一段时间后出现在
其他终端中。因此不能把该问题仅归因于单个 Codex TUI 未重绘；还需优先检查
共享或全局模型刷新任务的输出是否被广播到多个 Surface，或子进程是否继承了
错误的 PTY/stdout/stderr 文件描述符。调查时应记录日志实际出现的 Workspace、
Pane、Surface、Terminal lifecycle 和前台进程，并与发起刷新任务的 Agent 归属对比。

修复与排查范围：

- 确认受管 Codex、app-server watcher 及模型刷新子进程的 stdout/stderr 归属，
  防止辅助进程的日志注入交互式 PTY。
- 审计子进程 spawn 时的 stdin/stdout/stderr 文件描述符继承、全局日志 subscriber/writer 和
  跨 Surface 消息分发，确保诊断只进入明确指定的日志目标，不依赖当时 UI 焦点。
- 如果该字节确属 Codex TUI 输出，检查 VT 光标保存/恢复、备用屏、擦除、换行、
  延迟重绘和快照合并的处理，并确认 TUI 能在异步输出后完整重绘输入区。
- 检查完成、审批解决、超时、模型列表刷新、watcher 重连和子进程退出等延迟
  事件是否会在前台 TUI 进入输入态后产生同类输出。
- 增加多终端回归测试：在 Codex 恢复输入框后注入延迟 stderr 或诊断事件，
  验证所属 Surface 的输入文本、光标、提示区和后续键盘操作都正确，且其他
  Workspace/Pane/Surface 不会收到该字节；同时覆盖焦点切换、窗口 resize、长行
  换行和连续异步输出。

验收条件是：辅助进程诊断不会泄漏到任何前台交互式 PTY，也不会跨终端、
跨 Pane 或跨 Workspace 广播；对于应当显示的异步
TUI 输出，wmcterm 能正确解析和重绘，不会留下被覆盖的输入框、错位光标或
不可操作状态。

## 33.8 已修复缺陷：Codex 完成本轮后 Workspace 仍显示运行中

修复状态（2026-08-20）：Hook `Stop` 现在归一化为本轮状态观测
`Completed`/`Error`，不再错误映射为长生命周期进程 `Exited`。真实 PTY 退出仍是
关闭 Agent lifecycle 的唯一入口；交互式 Codex 恢复输入框后可以显示完成状态，
下一次活动又能在同一 lifecycle 中回到运行中。

已知问题：Codex 已经完成输出并恢复可输入提示符后，Workspace 状态仍长时间
显示“运行中”。这不符合当前状态模型：用户提交输入时可先乐观设置
`AgentState::Running`，但本轮完成事件到达后应转为 `AgentState::Completed`，
并聚合为 `WorkspaceStatus::Completed`。

排查与修复范围：

- 确认 Codex `Stop`/`AgentTurnComplete` Hook 以及 app-server `turn/completed` 等完成信号是否
  都能归一化为当前 Surface 的本轮完成事件。
- 检查完成事件是否因 Terminal lifecycle、session/thread ID、sequence 或重复事件校验而被
  拒绝，并让被拒绝原因可诊断。
- 区分“Codex 进程仍存活”与“当前 turn 正在执行”；交互式进程留在输入框
  不应使 Workspace 永久保持运行中。
- 增加“提交提示 -> 执行 -> 完成输出 -> 恢复输入框”回归测试，覆盖 Hook、
  app-server watcher、有无待审批请求和完成信号重复到达。

验收条件是：Codex 每次开始执行时及时显示运行中，每次恢复等待下一次用户
输入时及时离开运行中；进程存活状态与 turn 状态不再混用。

## 33.9 已修复缺陷：Agent 动态标题前缀使终端标签正文不可见

修复状态（2026-08-20）：Surface 继续保存完整原始 OSC title。受管 Agent 标签为
“单个 braille spinner + 空白”预留固定两列：运行时保留并显示 Codex/Claude 自带
spinner，空闲帧使用等宽空槽，因此动态帧不会改变标签宽度或挤掉稳定标题。
标题、编号、间距和关闭按钮使用同一组几何常量计算显示与命中区域；英文、CJK、
动态 spinner 及运行/空闲等宽切换已有共同回归用例。

已知问题：终端标签根据已生成的标题确定长度后，Codex 在运行期间会向终端
标题最前面加入类似转圈的动态进度字符，此后标签中原本应显示的后续文字可能
全部不可见。修复前实现会将每次 VT title change 整段替换到 `Surface.title`，
然后固定从字符串开头截断到 14 个显示列；没有区分稳定标题和瞬时进度前缀。

Claude 与 Codex 在 wmcterm 中共用完全相同的 VT title 接收、`Surface.title` 替换、
显示宽度计算和截断路径。因此，如果 Claude Code 也发送“动态字符 + 长标题”
的 OSC title，同一缺陷会出现。当前代码与测试只能确认 Claude 具有相同的受影响
条件，尚不能证明当前 Claude 版本必然会发送这种标题序列。

排查与修复范围：

- 捕获 Codex 和 Claude 在 idle、running、waiting 和 completed 阶段发送的原始 OSC title
  字节、解析后字符串、Unicode 显示宽度与实际字形边界。
- 检查 braille/block spinner、variation selector、emoji、组合字符、零宽字符与 ANSI/控制字符
  对 `truncate_display_width`、标签宽度和 glyphon clip bounds 的影响。
- 产品策略采用固定进度槽，保留 Agent 自带 spinner，同时保证稳定标题拥有独立、
  不随 spinner 帧变化的显示宽度。
- 增加 Codex 和 Claude 共用回归用例，覆盖短/长英文、CJK、emoji、组合字符、
  动态 spinner 帧和高频 title change，验证标签宽度、命中区域、关闭按钮与可见文本
  始终一致。

验收条件是：无论 Agent 是否动态更新终端标题，标签都至少保留可识别的
稳定标题文本；动态状态不会改变或破坏标签的布局与可操作区域。

## 33.10 已修复缺陷：新启动 Codex 被误标为运行中

修复状态（2026-08-20）：Agent lifecycle `Started`、前台 Agent 进程自动识别和
`SessionStart` 只表示交互式进程已经建立，初始 turn 状态统一为 `Idle`。用户提交
Enter、收到 ToolUse/Activity 或明确的运行状态观测后才进入 `Running`；审批、输入、
完成、错误和真实进程退出仍沿用各自独立的状态转换。

生命周期 `Running` 仅表示 Codex/Claude 进程仍存活，不能直接聚合为工作区“运行中”。
新打开 Agent 并停在输入框时工作区显示空闲，避免把等待首次输入误报为正在执行任务。
