# wmcterm 终端产品设计与演进记录

本文记录产品模型、早期设计和实现演进，按以下顺序阅读：

- [第 33 节](#33-实现基线与演进记录)：产品能力、组件职责和历次修复记录。
- [第 1 至 32 节](#1-产品定位)：立项时的设计推导、接口草案和阶段规划。
- [架构与实现记录](rust-libghostty-vt-winit-wgpu-terminal-design.md)：运行时资源边界、
  持久化机制、托管 Agent 权限与兼容性验证。
- [README](../README.md)：当前命令、配置和构建方法。

早期设计中的 Rust 类型、CLI、RPC 和配置示例表达当时的方案，使用时请查阅
README 和 `wmcterm capabilities` 中的现行接口。演进记录中的日期和版本用于标明
各次变更的背景。

## 1. 产品定位

wmcterm 参考 cmux 的工作区组织和交互方式，采用 Rust 原生实现与 GPU 渲染，
面向多终端、多 Agent 的可编程工作流。

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

产品将终端抽象为可组合的工作空间单元，通过工作区、窗格和表面组织任务。

建议整体模型：

```text
Window
  └── Workspace
        └── Pane
              └── Surface
```

工作区组织的任务包括：

```text
Workspace
  ├── Terminal
  ├── Agent
  ├── Dev Server
  ├── SSH
  ├── Logs
  └── Browser
```

界面汇总任务和 Agent 的活动状态，帮助用户定位需要处理的输入、审批、完成和
错误事件。对应的产品能力包括：

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

## 4. 核心数据模型

### 4.1 App 层级

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

### 4.2 Workspace

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

### 4.3 Pane Tree

Pane 布局采用递归 Split Tree，记录分割方向、比例和两个子节点：

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

## 5. Surface 模型

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

Agent 通过 Terminal Surface 上的元数据表达：

```text
Terminal Surface
       +
Agent Metadata
```

Claude Code 和 Codex 作为 CLI/TUI 运行在 PTY 中，复用终端的输入、渲染和
进程生命周期管理。

---

## 6. Terminal Core

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

libghostty-vt 提供终端协议解析、状态维护和输入编码。完整终端由以下组件协作实现：

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

## 7. 多线程模型

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

### UI Thread

负责：

- winit
- Workspace state
- Pane layout
- Focus
- Keyboard routing
- Mouse
- wgpu rendering

---

### Terminal Worker

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

## 8. Frame Scheduler

调度器合并终端更新，在帧预算内重绘最新状态，控制高吞吐输出产生的 UI 唤醒量：

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

## 9. wgpu Renderer

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

## 10. Glyph Renderer

早期字形渲染方案考虑以下组件：

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

渲染器按背景、字形和装饰批量提交，将绘制调用数量与单元格数量解耦：

```text
Backgrounds:   1 draw
Glyphs:        1~N draw
Decorations:   1 draw
Cursor:        1 draw
```

使用 GPU Instance Batching。

---

## 11. Glyph Atlas

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

## 12. Render Snapshot

Terminal Worker 发布不可变 RenderSnapshot，Renderer 消费快照：

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

可变 VT 状态集中在 Terminal Worker 中，输入、输出和 resize 在 actor 内按序处理。
快照传递使 GPU 绘制与 VT 更新独立调度。

---

## 13. Terminal 数据流

### 输入

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

### 输出

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

## 14. Keyboard 路由

快捷键必须两级路由。

当前默认窗格方向聚焦使用 `Ctrl+Alt+方向键`；单独的 `Alt+方向键`
透传给终端程序，以支持程序自身的交互快捷键。

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

## 15. Agent 状态模型

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

### Level 1：OSC

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

### Level 2：Agent Hooks

例如：

```text
Claude Code hook
Codex hook
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

### Level 3：Socket API

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

结构化事件携带状态和目标标识，直接进入 Agent 状态归约流程。

---

## 16. Notification Center

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

## 17. Socket API

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

## 18. CLI

CLI 入口：

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

## 19. Command Palette

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

## 20. Command Architecture

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

快捷键、CLI、UI 按钮和 Socket 复用命令语义，统一执行目标解析和状态变更。

---

## 21. Session Restore

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

恢复时创建新的 PTY 进程，并根据保存的目录、命令和 Agent 会话标识重新启动。

---

## 22. 配置系统

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

## 23. 项目配置

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

## 24. Rust Workspace 工程结构

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
│   │   └── Codex
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

各 crate 通过命令、事件和快照协作，按职责管理状态所有权和依赖关系。

---

## 25. 依赖建议

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

`wmcterm-terminal` 对外提供终端命令和渲染快照，将 libghostty-vt 的类型与生命周期
管理集中在该 crate 内。升级 VT API 时，可在这一边界适配调用方。

---

## 26. 总体架构

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

## 27. MVP 范围

立项时的 MVP 交付项：

1. PTY
2. libghostty-vt
3. wgpu Terminal Rendering
4. winit Window/Input
5. Vertical Workspace Sidebar
6. Pane Split
7. Notification / Attention
8. CLI + Unix Socket

上述能力构成面向 Agent 工作流的终端工作区初版。

---

## 28. 开发阶段

### Phase 0：libghostty-vt PoC

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

### Phase 1：单 Terminal

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

### Phase 2：Workspace

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

### Phase 3：Command System

实现：

```text
Action Registry
Command Router
CLI
Unix Socket
Command Palette
```

---

### Phase 4：Agent

实现：

```text
OSC Notification
Agent Hooks
Agent State
Attention State
Notification Center
```

---

### Phase 5：Session

实现：

```text
Session Restore
Project Config
Agent Resume
```

---

### Phase 6：扩展 Surface

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

## 29. 最早需要验证的三个技术风险

### 29.1 libghostty-vt 生命周期和线程模型

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

在 PoC 阶段验证状态所有权、消息传递和关闭顺序，再扩展工作区功能。

---

### 29.2 Font / Glyph Rendering

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

### 29.3 高吞吐输出

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

测试记录吞吐量、输入延迟和 UI 帧耗时，以评估脏状态合并与重绘调度。

---

## 30. 工作区与终端的职责分层

Workspace Core 管理布局和表面，终端后端提供 PTY/VT 能力：

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

Surface 模型为以下扩展方向提供统一的布局和生命周期入口：

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

---

## 31. 最终推荐产品形态

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

## 32. 项目目标总结

第一版以工作区组织、Agent 状态感知、VT 兼容性和 Rust/wgpu 跨平台运行时为目标。
终端兼容性通过真实 PTY/VT 测试和 GUI 输入协议测试验证。

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

---

## 33. 实现基线与演进记录

本节从 2026-08-19 的实现基线开始，持续记录产品能力和后续修复。33.1 至 33.5
介绍已落地的组件与接口，33.6 起记录具体变更及其验证背景。

### 33.1 已落地的产品与线程模型

- 产品层级已实现为 `AppState -> Workspace -> PaneNode -> Pane -> Surface`，
  全部实体使用 UUID；Pane 使用可递归的 Split Tree，支持四向分割、比例调整、
  关闭、聚焦与缩放。
- 每个 Terminal 由独立 PTY/VT actor 持有可变状态，`wmcterm-app` 的 winit UI
  消费不可变快照并合并唤醒，避免 UI 线程直接读写 VT 状态。
- Linux/macOS/Windows 进程抽象由 `portable-pty` 提供，终端解析和输入编码由
  `libghostty-vt` 提供；渲染由 `wgpu` 矩形和 `glyphon` 字形管线完成。
- Windows 最低运行环境是 Windows 10 1809（build 17763），同时支持 Windows 11；
  安装器和程序入口均拒绝更旧系统。Windows 默认启动系统内置的 Windows
  PowerShell 5.1（`powershell.exe`），只有用户显式配置时才使用 PowerShell 7
 （`pwsh.exe`）或其他 shell。
- Terminal 支持主/备用屏、scrollback、ANSI/SGR 颜色、鼠标协议、选择、搜索、
  Unicode 宽度、CJK/emoji/组合字符以及主题颜色查询；拖选越过内容区上下边界时
  会连续滚动，越界距离决定滚动速度，回到内容区或松开鼠标后立即停止。
- VT 生成的 PTY 协议应答按输出块立即转发，不等待输出洪峰和渲染快照合并结束；
  连续输出期间的应答字节与待写批次数量均有界，静默 250 ms 后恢复预算。历史
  回放只恢复屏幕内容，禁止向新 PTY 重放查询应答或其他 VT side effect。
- GUI 单行编辑器与终端输入路径相互独立。GUI 光标是覆盖文本的块状矩形，
  选区使用主题的 selection foreground/background 反色，不向字符串插入光标
  字符，因此移动光标不会改变文字布局。

### 33.2 Surface 与界面能力

当前核心状态允许 `Terminal`、`Markdown`、`Logs` 和 `Browser` 四种 Surface
类型，但成熟度不同：

- Terminal 是完整的交互式 Surface，可运行 shell、Agent、SSH、Docker 和
  Kubernetes 命令。
- Markdown 支持文件刷新、结构化块渲染与搜索。
- Logs 支持有界历史、文件 tail、截断/替换检测、级别过滤和保留源行号的搜索。
- Browser 提供平台无关的占位后端；URL 由外部系统应用打开。

无边框 winit 窗口已经包含工作区侧栏、Surface 标签、Pane 分隔条、导航栏、
命令面板、终端查找、通知中心、Agent 授权卡片和双语（英文/简体中文）界面。
工作区侧栏允许按住整行拖动排序：超过小幅移动阈值后显示跟随指针的预览和明确的
插入位置，释放时复用核心 `WorkspaceCommand::Move` 提交顺序。筛选工作区期间禁用
拖动排序，避免可见过滤顺序与持久顺序不一致。
工作区筛选、命令面板查询、终端/文档查找和通用提示框共用单行 UTF-8 编辑模型；
当内容宽于输入区域时，界面按终端显示单元格建立光标跟随的水平视口。文本、选区、
鼠标命中、拖选、光标覆盖层和 IME 候选位置使用同一视口偏移，完整输入仍保留在模型
中，不因可见宽度截断，并避免在 CJK、emoji 或组合字符的 UTF-8 边界内切分。
点击 GUI 标题栏关闭按钮或其他交互式关闭入口时，如果会终止活动进程、Agent
或待处理请求，会显示确认；明确指定 UUID 的 CLI/RPC Agent 操作不弹出页面确认。

### 33.3 CLI、IPC 与 Agent 控制面

- 本地控制面使用 JSON-RPC，协议能力由 `wmcterm capabilities` 自描述；当前
  协议版本为 4；无排序语义的 `workspace.pin` 已移除，顺序调整统一使用
  `workspace.move`。
- CLI/RPC 覆盖 workspace、pane、surface、screen、terminal、notification、
  Agent request/approval、配置、Hook、Skill 和远程命令入口。
- wmcterm 创建的终端导出 `WMCTERM_WORKSPACE_ID`、`WMCTERM_PANE_ID`、
  `WMCTERM_SURFACE_ID`、`WMCTERM_TERMINAL_LIFECYCLE_ID` 和控制 socket。
  显式目标优先于 UI 焦点；`--no-focus` 创建不会偷取调用者焦点。
- `wmcterm send` 使用 bracketed-paste 协议传送普通或长文本；Enter 等独立按键
  由 `wmcterm send-key` 编码，避免粘贴文本和控制键语义混淆。
- `wmcterm send-line` 在 terminal actor 内把 bracketed paste 与 Enter 合并成单个
  有序命令，避免 Agent 在两次 RPC 之间留下半输入状态。`screen.read` 返回单调
  revision、光标和 viewport 元数据，并可通过 `--scrollback --lines N` 从 actor
  导出有界历史而不改变用户当前滚动位置。`screen wait` 在 CLI 侧轮询 revision
  或目标文本，不阻塞 winit UI 线程和 PTY/VT actor。
- 普通通知与可操作授权请求是两条数据通道。“全部已读”只改变普通通知的未读
  状态，不删除通知，也不解决或隐藏 Agent 请求。
- Agent 授权采用有界、可过期、幂等且 fail-closed 的请求/回复通道。请求绑定
  Surface、进程和 Terminal lifecycle，拒绝旧进程 Hook、重复或含糊回复以及
  普通审批回复中的 bypass/yolo 模式。审批卡仅提供拒绝、单次允许，以及 provider
  明确给出精确规则或会话决策时的信任规则；不把“全部工具”或绕过伪装成单条请求
  的批准选项。通知中心先按 Agent 和请求类型提取命令、工具、目录、原因与
  权限等可读摘要，格式化 JSON 只作为技术详情保留；长详情提供行数进度、边界钳制、
  滚轮和完整键盘导航以及可拖动滚动条。展示层不得改变受支持的决策或回复映射。
- Agent 运行策略与单条审批分层：`interactive` 维持按需审批；`unattended` 使用
  Codex `--approve-for-me`（自身启用 `workspace-write`）或 Claude `--permission-mode auto`，
  允许原生低风险自动判断但保留隔离；`full-access` 关闭审批和沙箱，只能从命令面板
  的显式动作启动，并要求在危险说明弹窗中再次确认。完全访问在启动时授权给目标会话，
  启动选择保存在托管 Codex 的会话恢复记录中，重启时沿用；全局配置与
  CLI/Agent 自动化入口继续拒绝危险全权限参数。
- GUI 为含 `.codex` 目录的 Workspace 启动自动审查模式的 Codex 前，按规范化的精确项目根
  检查用户级 Codex 信任配置。未信任时由人工选择取消、不信任并继续，或信任并启动；
  信任操作保留未知 TOML 和文件权限并原子替换配置，解析或写入失败时 fail closed。
  该入口不属于 CLI/RPC/Agent 能力，防止无人值守进程自行提升项目 Hook 权限。
- 原生 `codex resume` 默认按 cwd 过滤、`--all` 才展示全局会话。私有 app-server 的
  远程 TUI 无法自行推断服务端目录，因此 wmcterm 会把 Workspace cwd 作为稳定的
  `--cd` 参数传给根 TUI。Codex 0.149.1 至 0.154.0 会话内的 `/resume` 会新建一个没有
  继承 remote cwd override 的 picker 连接，因此仍可能锁定在全部会话；该
  provider 内部行为无法由 wmcterm 的外部启动参数修复。调用者显式传入
  `-C`/`--cd` 时优先使用该目录；过滤不改变线程归属或审批路由。
- 待处理请求队列是审批状态的权威来源。每次注册、回复、取消、超时或进程退出
  都按 Surface 聚合剩余请求并重新派生 Agent、attention 和 Workspace 状态；权限
  请求优先显示 `WaitingForPermission`，其他可操作请求显示 `WaitingForInput`，仅在
  最后一个请求结束后恢复 AgentSession 的真实状态。普通通知已读操作不会清除仍
  在等待的审批状态。
- Codex TUI 与通知中心可同时看到同一 app-server 审批。若用户先在终端完成选择，
  watcher 在收到 Codex 的 `serverRequest/resolved` 明确回执，或 thread/turn/item ID
  精确匹配的 item started/completed 生命周期事件后取消对应待处理卡片；不会根据
  键盘输入或无关 item 活动推断审批结果。
- Codex 0.150.0 的 `item/commandExecution/requestApproval` 使用
  `kind: "writeStdin"` 表示向已有命令发送输入。审批桥将其作为独立类型，
  优先使用 `approvalId` 精确路由，仅提供拒绝与单次允许；即使收到持久
  允许回复也必须 fail closed。通知摘要始终标明这是已有命令的输入审批；
  stdin 或 process ID 只在 provider 实际传入时展示，不将它们视为 wire schema 必填字段。
- watcher 初始化时恢复已加载 thread，并在收到携带 thread ID 的 `thread/*` 通知后立即通过
  `thread/resume { excludeTurns: true }` 恢复新建或由 `/resume` 选中的 thread；
  WebSocket 重连后重新建立这些订阅。`thread/read` 只能观察通知，不能让 watcher
  接收发往 thread 客户端的 app-server 审批请求，因此不能用于审批桥。
- 已对照 Codex 0.154.0 的 `resume_running_thread` 和 thread lifecycle 实现核验
  多客户端订阅仍受支持。订阅只传 `threadId`、`excludeTurns`，不覆盖已保存的
  cwd、权限或 worktree 配置。回归测试覆盖只读子线程的 cwd、显式目标及
  `focus: false`，以及交错提问使用独立输入通道。受支持的问题和简单 MCP 表单由
  请求面板处理，其他输入继续使用原生 TUI。
- watcher 从所有携带 thread ID 的 `thread/*` 通知发现新目标，同时周期性回查
  `thread/loaded/list` 并同步确认每次
  `thread/resume` 成功，以恢复丢失或版本差异导致的 resume 通知。
- watcher 可以沿最后创建的子 Agent 表面继续分割；若该缓存表面已被用户关闭且
  `pane.split` 明确返回 `target surface not found`，则清除缓存并立即从根 Codex
  表面重试一次。其他控制错误不得被该回退吞掉。
- 受管 `codex-teams` 的 app-server 会为 Hook 子进程导出内部 approval-bridge 标记；
  此时通用 `PermissionRequest` Hook 仅作为重复遥测确认，不再创建无法关联完成回执的
  `permission:*` 请求。非受管 Codex Hook 没有决策回传通道，只作为等待权限的状态
  观测和普通通知处理；通知可将用户引回对应终端，但不得创建允许/拒绝审批卡。真正
  可操作的 Codex 审批只来自 app-server 的 `agent.approval.register` 路径。
- 已实现 Claude Code Hook、Codex app-server、OSC 9/99、wmcterm OSC 777，
  并可归一化 Claude、Codex 的 Agent 生命周期事件。
  Claude 内部 subagent 仍属于其 Claude 进程；独立可见 Agent 通过受管命令在
  新 Pane/Surface 启动。实验性 `codex-teams` 可将 Codex 子线程映射为原生 Pane。
- Codex 0.150.0 `Interrupt` Hook 归一化为当前 turn 完成且不继续，使
  Workspace 从运行中恢复 Idle；该事件不是 SessionEnd，不得结束仍存活的
  Codex surface 或 PTY lifecycle。
- Claude `StopFailure`（2.1.78 起）归一化为独立的 `TurnFailed`，显示错误和
  attention，但保留会话 lifecycle 与后台子 Agent。结构化 Claude Hook 在写入
  任何状态之前检查 session 归属和事件投影，防止旧 session 的迟到失败污染
  当前会话。Hook 安装计划包含该事件，用户显式更新安装后生效。
- Claude 2.1.268 修复 `-p`/`--print` 不触发 `PermissionRequest` 的问题；
  本集成对交互和 headless 请求使用同一审批协议，不根据缺失的模式标记推断
  自动授权。审批从注册开始共用 9 分钟预算，并在 RPC 返回后再次检查超时与
  取消，拒绝过期允许结果。该预算限制接受审批的时间；进行中的 IPC 和取消清理
  可能使 Hook 稍晚返回。用户级 Hook 在 wmcterm 外仍交回 provider 原生流程。
- Codex 0.151.0 `SubagentStart`/`SubagentStop` 使用官方顶层
  `session_id`、`turn_id`、`agent_id`、`agent_type` 字段，并以
  `(Surface, session_id, agent_id)` 幂等跟踪。子 Agent Hook 只表示运行活动，
  不会完成父 Surface；活动集合对父状态施加 Running 覆盖，但 Error 与
  NeedsInput 仍保持更高优先级。`SubagentStop` 把精确匹配项标记为 Stopped，并按
  Surface/session 有序保留最近 256 个有界墓碑；活动项不参与容量淘汰，超出界限后
  最旧墓碑定义了可防护的乱序重放窗口。SessionEnd、PTY 退出或 Surface 关闭清理
  对应状态；成功路由的新 Codex SessionStart 也会清理该 Surface 上所有旧 session，
  同时保留新 session 及其他 Surface。升级和普通运行不会自动修改 Codex 配置；只有用户显式执行
  `wmcterm hook install --agent codex --scope user` 才写入 `~/.codex`，并须在
  `/hooks` 审核；项目级安装使用 `--scope project`。

### 33.4 配置、恢复与发布

- 配置由用户级 `wmcterm/config.toml` 与项目级 `.wmcterm.toml` 分层加载；
  支持主题、256 色 palette、cursor/selection 颜色、字体、
  快捷键、scrollback、侧栏、工作区 cwd、通知、语言和窗口选项。
- `config import-ghostty` 读取 Ghostty 配置并输出合并预览，不覆盖原配置。
- 会话恢复持久化 Workspace/Pane/Surface 布局、可见 scrollback、命令和支持的
  Agent resume ID；不会序列化 PTY 进程内存，恢复时重新启动对应命令。
- 系统代理由 `wmcterm-app` 的后台缓存统一提供给 PTY 创建入口，因此新建和恢复的
  Shell、Codex、Claude 使用同一规则。Linux 读取 GNOME/KDE 静态手动代理，Windows
  读取当前用户 WinINet；PAC/WPAD 安全跳过。缓存低频刷新，创建前执行有界刷新，
  UI 超时后使用最近有效快照，检测失败不清空缓存。父进程及 RPC 显式代理变量按协议
  优先，桌面代理只补缺；现有进程的环境和生命周期不受缓存变化影响。
- `codex-teams` 的私有 loopback app-server endpoint 属于单次运行时状态。
  带托管恢复记录的会话在重启时创建新 app-server，并将保存的子表面连接到新服务。
  旧记录中的子分区命令在 thread ID 与 Agent resume ID 一致时降级为独立
  `codex resume` 会话。Claude Code 使用稳定的 session ID 恢复。
- 仓库包含 Linux x86_64 Debian 与 Windows x86_64 NSIS 打包脚本、桌面入口、
  应用图标，以及作为默认字体随包分发的 Maple Mono Normal NF（含 OFL 许可）。
  Windows 安装器检查 build 17763 门槛、注册 App Paths，并维护系统 `PATH`；
  Windows 本地控制传输为写入端点文件的动态 loopback 地址，不占用固定端口。
  安装包同时提供 GUI subsystem 的 `wmcterm-gui.exe` 轻量启动器；桌面/开始菜单
  快捷方式经该启动器以无控制台模式启动 `wmcterm.exe gui`，而主 EXE 保留 CUI
  subsystem，保证 CLI 与 Agent Hook 的标准输入输出可用。
- Agent Skill 的唯一源码位于 `integrations/skills/wmcterm/SKILL.md`，Hook
  协议、事件清单与配置片段生成器位于 `crates/wmcterm-agent`。`.agents`、
  `.codex` 和 `.claude` 下的 wmcterm 文件只是安装/发现目标，不作为源码维护；
  更新脚本同步用户级 Codex/Claude Skill 与 Hook，同时保留不属于
  wmcterm 的已有配置。

### 33.5 当前 Rust workspace 与验证基线

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

### 33.6 已修复缺陷：跨工作区审批阻塞模态编辑

修复状态（2026-08-20）：审批注册仅在请求属于当前 Surface 且没有活动文本
编辑器时自动展开通知面板；键盘路由同时保证 `TextPrompt`、工作区搜索、命令
面板和终端查找优先于异步展开的通知面板。非当前 Surface 的请求仍更新对应
Agent/Workspace attention，并按配置发送原生通知，但不会抢占键盘或 IME。

修复前现象：用户正在修改当前工作区路径时，如果其他工作区存在待审批的
Agent 请求，当前路径编辑窗口可能卡死或无法继续输入、提交、取消及执行
其他操作。这是错误的跨工作区交互耦合：非当前工作区的待处理请求可以触发
attention 和通知更新，但不应抢占当前模态编辑器的输入归属，也不应使其进入
不可退出状态。

同类交互的回归检查范围：

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

### 33.7 已修复缺陷：异步诊断输出覆盖 Codex 输入框

修复状态（2026-08-20）：确认示例中的日志来自 `wmcterm codex-teams` 启动的私有
Codex app-server；该辅助进程此前继承了交互式 PTY 的 stderr。现在其 stdin、
stdout 和 stderr 均不连接前台 PTY，延迟模型刷新诊断不会再注入根 Codex TUI
或其他终端。根 Codex 进程本身仍保留标准流继承，以维持正常交互。

修复前现象：Codex 完成一次任务并恢复输入框后，后台模型刷新子进程的延迟
错误输出会直接写入当前终端，例如：

```text
2026-08-20T03:08:09.769966Z ERROR codex_models_manager::manager: failed to refresh available models: timeout waiting for child process to exit
```

该行覆盖了 Codex 输入框。排查确认私有 app-server 继承了前台 PTY 的 stderr，
修复将辅助进程标准流与交互式终端隔离。诊断输出按子进程归属路由，目标由显式
Surface 和 Terminal lifecycle 标识确定。

回归检查关注辅助进程延迟输出、watcher 重连和进程退出时的标准流归属，以及
多终端切换后的输入文本、光标和提示区。属于 TUI 自身的输出继续由 VT 解析和
快照渲染路径处理。

### 33.8 已修复缺陷：Codex 完成本轮后 Workspace 仍显示运行中

修复状态（2026-08-20）：Codex Hook `Stop` 归一化为本轮状态观测
`Completed`/`Error`，保留长生命周期进程状态。PTY 退出负责结束该终端进程；
交互式 Codex 恢复输入框后可以显示完成状态，
下一次活动又能在同一 lifecycle 中回到运行中。

修复前现象：Codex 已经完成输出并恢复可输入提示符后，Workspace 状态仍长时间
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

### 33.9 已修复缺陷：Agent 动态标题前缀使终端标签正文不可见

修复状态（2026-08-20）：Surface 继续保存完整原始 OSC title。受管 Agent 标签为
“单个 braille spinner + 空白”预留固定两列：运行时保留并显示 Codex/Claude 自带
spinner，空闲帧使用等宽空槽，因此动态帧不会改变标签宽度或挤掉稳定标题。
标题、编号、间距和关闭按钮使用同一组几何常量计算显示与命中区域；英文、CJK、
动态 spinner 及运行/空闲等宽切换已有共同回归用例。

修复前现象：终端标签根据已生成的标题确定长度后，Codex 在运行期间会向终端
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

#### 33.9.1 Surface 标签弹性布局与溢出导航

实现状态（2026-09-01）：Surface 标签不再按标题内容生成狭窄且固定的命中区域。
同一可见页中的标签平均分配可用宽度，单个标签以 80px 为最小舒适宽度、240px
为扩展上限；长标题保存完整内容，仅在最终文本边界确定后按 Unicode 显示列截断。
空间不足时，标签栏为 `⋯N` 保留独立的 44px 区域，显示隐藏数量，并通过完整标题
列表切换任意 Surface。可见标签按固定页分组，组内切换不会改变标签位置；切换到
其他页时仍保证活动标签可见。新增按钮、溢出按钮、标签和关闭按钮共享无重叠的
绘制与命中几何。

### 33.10 已修复缺陷：新启动 Codex 被误标为运行中

修复状态（2026-08-20）：Agent lifecycle `Started`、前台 Agent 进程自动识别和
`SessionStart` 只表示交互式进程已经建立，初始 turn 状态统一为 `Idle`。用户提交
Enter、收到 ToolUse/Activity 或明确的运行状态观测后才进入 `Running`；审批、输入、
完成、错误和真实进程退出仍沿用各自独立的状态转换。

生命周期 `Running` 仅表示 Codex/Claude 进程仍存活，不能直接聚合为工作区“运行中”。
新打开 Agent 并停在输入框时工作区显示空闲，避免把等待首次输入误报为正在执行任务。

### 33.11 已修复缺陷：Claude Goal 续跑被误标为已完成

修复状态（2026-08-23）：Claude Code 的 `Stop` Hook 是可被 `/goal` 或用户 Stop Hook
拒绝的本轮停止提议，不再映射为根会话 `Completed` 或释放会话路由。wmcterm 保留
Hook 的 `stop_hook_active` 字段用于保留诊断上下文，但该字段只说明此前发生过 Hook
续跑，不能表示本次 Stop 的最终裁决。因此所有 `Stop` 都保守映射为 `Idle`；如果
Goal 继续执行，后续工具或活动事件会重新映射为 `Running`。只有 `SessionEnd` 才表示
Claude 会话真正完成，并清理该会话的子 Agent 状态。

该区分避免 Goal 尚未满足时 Workspace 短暂或持续显示“已完成”，也避免一次可撤销
的 `Stop` 提前清除仍属于活动会话的子 Agent 路由。回归测试覆盖普通 Stop、Goal
续跑 Stop 和 SessionEnd 清理边界。

### 33.12 性能与显示修复：cmux 参考边界

修复状态（2026-08-21）：高频终端快照只标记 scene dirty，UI 合并唤醒并在
60 Hz 上限内消费最新状态。每个终端的待处理队列只保留最新快照；隐藏工作区、
非活动标签页的快照继续更新 RPC revision，但不再使可见 scene 失效。渲染器按
文字块复用 glyphon 排版结果，并根据最近的 scene 构建与提交耗时在 60–20 FPS
之间自适应留出事件处理余量。闪烁光标每 0.5 秒切换显隐；触控板精确滚动保留
不足一行的余量，不再丢失小幅输入。布局、输入命中和终端行列使用
winit 逻辑坐标，wgpu surface 与字形渲染使用当前 DPI 换算的物理像素。
`window.opacity` 同时作用于背景 alpha 和透明窗口合成；工作区 Git 分支、
端口等元数据只在可见值变化时使 scene 失效。

`example-cmux` 中的实现适合校验产品语义，但不适合直接移植：

- `TerminalCursorBlinkState` 将 0.5 秒半周期、单调时间与“只在可见性
  变化时失效”分离，wmcterm 采用这一可测试状态机原则。
- `GhosttyTerminalScrollBoost` 区分精确、手势与惯性滚动，但其 AppKit 特有的
  2 倍 boost 不是跨平台契约；wmcterm 保留 winit delta 语义并累计小数行余量。
- `GhosttyTerminalView` 的 backing scale 处理证明了“逻辑布局、物理渲染”的
  边界；其最新值覆盖、单次异步 flush 也适合作为渲染失效原则。
- `RenderedFrameDeliveryCoordinator` 与 `RenderPumpGate` 的 demand gate、
  `bufferingNewest(1)` 和 clean→dirty 边沿唤醒适合映射为 wmcterm 的可见性门控、
  每终端最新快照与空闲零轮询；Swift actor/Metal 提交代码本身不直接移植。

cmux 依赖 Swift/AppKit 事件、`NSView`/`CAMetalLayer` 生命周期和
GhosttyKit 的 surface/renderer；wmcterm 需要在 Linux、macOS 和 Windows 上共用
Rust actor、winit 事件循环、wgpu surface 和 glyphon 缓存。因此只借鉴上述时序、
坐标和失效原则，不引入 AppKit/CAMetalLayer/GhosttyKit 适配层。

### 33.13 手动 Codex 的运行模式与 turn 状态解耦

实现状态（2026-09-01）：前台进程识别在保持 Agent 种类判断的同时记录运行模式。
手动 `--approve-for-me`/`--full-auto` 归类为自动审查，手动
`--dangerously-bypass-approvals-and-sandbox`/`--yolo` 归类为危险全权限，未显式指定
则为交互模式。该字段随 Surface Agent 元数据持久化并通过状态接口输出；旧会话缺少
字段时安全回退为交互模式。工作区中存在多个 Agent 时，侧栏显示最敏感的运行模式。

运行模式只描述权限和沙箱策略，不参与 turn 状态转换。进程出现或 `SessionStart`
仍为 `Idle`；GUI 提交可乐观进入 `Running`，Codex `UserPromptSubmit`、工具和活动 Hook
用于确认运行，`Stop`/`Interrupt` 和真实 PTY 退出分别结束 turn 或生命周期。托管启动器
仍拒绝危险 bypass 参数；识别手动全权限进程不会产生任何授权能力。

### 33.14 Agent 恢复按启动来源和退出状态决策

2026-09-15 更新：带托管恢复记录的 Codex 已改为按原权限模式重建 Teams 服务并恢复
原线程，三种模式均保留。下述普通 resume 降级规则仅适用于旧记录、手动 Agent 和
其他 provider。详见 [Codex 托管恢复设计](rust-libghostty-vt-winit-wgpu-terminal-design.md)。

实现状态（2026-09-01）：会话持久化区分 wmcterm Agent 入口启动和 Shell 手动启动。
前者在取得 provider session ID 后始终保存 resume 信息，并在重开时直接执行 Codex、
Claude 的 resume 命令，不再重放原始启动命令。后者只有在 wmcterm 关闭时
生命周期仍为 `Running` 且终端前台进程仍被识别为同一种 Agent 才保存 resume；已经
退出回到 Shell、收到 `SessionEnd`、PTY 退出或失败状态的手动 Agent 不再自动连接旧
会话，Surface 恢复为普通终端。

仍在运行的手动 Agent 恢复时不能把 provider resume 命令作为 PTY 根进程，否则用户
退出 Agent 会同时结束终端并丢失可见历史。恢复器使用带来源标记的 Shell 子命令执行
resume，并在其结束后回到交互式 Shell；该标记同时防止下一次持久化把手动恢复误判为
wmcterm 托管启动。Linux/macOS 使用 `exec` 回到用户 Shell，Windows 分别适配
PowerShell、cmd 和兼容 Shell。

启动来源通过 AgentSession 中捕获的初始终端命令与已识别 Agent 类型精确匹配，不能只
依据 Surface 当前标题、UI 焦点或残留 session ID 推断。这样，显式以 `bash` 等命令
创建的终端后来手动运行 Codex，也仍遵循手动 Agent 的状态判断规则。

终端键盘路由默认将 `Ctrl+C` 交给前台 PTY，即使存在鼠标选区也不拦截为复制，
以保证 Codex 等前台程序能够接收中断。复制选区使用 `Ctrl+Shift+C`。

侧边栏溢出时显示独立滚动轨道：行内容右侧预留 18px，轨道位于宽度边界内
8–16px，避开边界两侧 5px 的宽度调整命中区。轨道从工作区列表顶部开始，
保留顶部搜索/新建控件；底部 32px 空白属于列表末尾，滚动到底时位于最后一个
工作区下方，轨道延伸到侧边栏底部。布局统一提供行、轨道和滑块矩形，
供绘制、悬停和点击使用；拖动与点击定位沿用工作区行索引滚动语义，筛选或
窗口增大使内容不再溢出时隐藏轨道，失焦或释放鼠标终止拖动。

侧边栏边界普通线为 1px，悬停和拖动时以该像素为中心、左右各扩 1px，形成
3px 高亮。统一在窗格背景之后、浮层之前绘制，并从标题栏下方连续延伸到底部，
避免一侧被终端背景盖住而在底部露出短线；不再叠加额外拖动光晕。通用列表滑块
高度不得超过轨道高度；空间不足以容纳轨道内边距时不绘制滚动条。

恢复未带 Codex 托管恢复记录的 Agent 时，生成的普通 resume 命令与 Surface AgentMetadata.run_mode 必须
保持一致：在构造 UI 和接收 Hook 前，将对应 Surface 的旧自动审批/完全访问模式
重置为 Interactive。命令面板托管启动、手动 Shell 包装恢复、旧直接恢复以及失效
remote 子会话降级均适用；按原样重放的显式启动命令不由此规则改写。手动 Agent
前台检测可以独立更新同一 provider 的 run_mode，保留 Hook 维护的状态和会话 ID。

Codex 的会话 ID 不代表 rollout 已持久化。会话保存和加载阶段检查当前 Codex
用户目录的 sessions/archived_sessions 中匹配线程 UUID 的非空 rollout 文件，
不在渲染循环或终端 actor 内进行目录扫描。缺失记录的生成式恢复回退为 Shell，
同时移除对应 Surface AgentMetadata 与旧 AgentSession；按原样重放的显式命令
不被此检查改写。前台 Agent 消失后，在重新计算状态前清除旧会话和子 Agent 状态，
避免把已结束的 Running/Waiting 状态投射回工作区。

### 33.15 本地 Agent 的 Bash SSH 闭环

SSH 结构化操作由 `wmcterm-terminal::ssh` 实现，PTY 读取、协议状态和输入提交均在
终端 actor 内执行。UI/RPC 仅显式定位 Surface 并发送有期限的请求，不根据焦点选终端。
本地普通 Bash 通过临时进程替换 rc 加载会话集成；远端通过同一 SSH 连接创建
`~/.wmcterm/tmp/bash.XXXXXXXXXX`，加载时删除。目录 0700、脚本 0600，不部署 daemon。
支持 Bash 4.2.46+，保留用户 `.bashrc` 和原提示符命令，不采用登录 Shell 语义。
Bash 5.1+ 使用数组式 PROMPT_COMMAND，旧版在原字符串前插入独立提示符 hook，保留
原命令收到的退出码。Readline 探测使用两字节按键序列，兼容 Bash 4.2 的 bind -x
仅支持单层 keymap 间接查找的限制；WMCTERM_TEST_BASH 可指定测试用 Bash 可执行文件。

每个 actor 有随机本地 token，每次连接生成新的 connection ID，执行生成 command ID。
有界 OSC 777 帧报告 ready/start/done/rejected 及十六进制 UTF-8 cwd，跨 PTY chunk
解析并校验 token、命令 ID 和状态顺序。该协议不具有身份认证能力，远端进程可能伪造
终端输出，绝不能用作 Agent 审批依据。输出缓存位于 start/done 边界之间，默认 64 KiB、
最多 1 MiB，保留原 PTY ANSI/CRLF；它不是隔离的 stdout/stderr，后台输出也可能混入。

actor 首先检查本地/远端就绪状态，只发送 Readline 探测键；绑定函数确认输入行为空并
应答后，actor 才发送编码命令，执行函数内 eval，避免将命令交给尚未结束的提示符 hook
或拼接用户未提交的输入。人工介入会撤销尚未发送的内容。支持 Emacs 和 vi keymap。
人工输入令当前命令结果变为 outcome_unknown；提示符恢复后方可再次执行。单连接仅一个
执行中的命令，保留最近一次结果，不实现命令重放或无界历史。超时只结束 CLI 轮询；
取消发送 Ctrl+C 并等待提示符确认，不假定进程已经停止。断线后旧连接 ID 失效。

`ssh.connect/status/exec/cancel` 要求 surface_id UUID；exec/cancel 必须匹配 connection_id，
cancel 还要匹配 command_id。CLI 可先查询再绑定连接，但 Agent Skill 要求保存连接 ID，
后续 exec/cancel 显式携带，发现连接变化时停止。Skill 默认将 wmcterm 内的远端 Shell
任务路由到不切换焦点的专用终端，仅复用当前任务记录且主机、连接 ID 均匹配的连接。
CLI `ssh HOST --new --pane UUID` 组合已有 surface.new(focus=false)、ssh.status 和
ssh.connect，在一次 Agent 工具调用内完成创建和本地/远端等待。总等待默认 30 秒，
最大 300 秒；CLI 负责限时轮询，不阻塞 UI 线程。`--surface UUID --wait` 同样等待
local_ready。返回值包含 Surface ID；本地等待超时附带 wait_phase=local_ready 和
requested_host，连接提交后保留 connection_id。超时不会自动创建替代终端或重连。
exec 的 expected_host 可选参数在 actor 内与 connection_id、ready 原子校验；CLI
`--host` 要求显式连接 ID，Skill 使用这两个参数复用已记录连接，省去单独状态查询。
CLI 支持互斥的 --command、--command-file 和 --command-stdin；后两者有界读取
UTF-8 脚本并拒绝 NUL/超过 16384 字节的输入，沿用同一 ssh.exec RPC 和远端转义。
协议仍串行保留最近一次命令；100 毫秒轮询保持不变，事件等待留待单独设计。
本地 IPC 错误附带套接字路径与权限/沙箱诊断提示，不自动扩大 Agent 权限。
Hook 不拦截普通 ssh。SSH 正在认证与尚未收到远端握手
均显示 connecting，由终端完成密码、MFA 和主机验证交互。应用重启后不恢复此类连接。
旧无 --surface/--new 的 SSH/tmux 工作区入口保留原行为。验收脚本 `scripts/ssh-smoke.py` 在
Xvfb、临时用户目录、独立 IPC 和回环 sshd 下运行，不操作调用者的窗口或终端。
