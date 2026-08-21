# TraceDB Watch Daemon

TraceDB 提供了一个 `daemon` 子命令，用于自动化定时入库。该功能支持增量入库，当检测到没有变化时会跳过计算。

## 功能特性

- **自动化定时入库**：无需手动运行 `watch` 命令
- **增量入库**：只处理有变化的 session，跳过未变化的部分
- **智能检测**：文件系统监听 + 定时回退，确保不遗漏变化
- **一键安装**：自动生成并安装系统服务
- **跨平台支持**：macOS 使用 launchd，Linux 使用 systemd user service，Windows 使用 Task Scheduler

## 使用方法

### 安装 daemon

```bash
# 使用默认配置（每 30 分钟入库一次）
trace-db daemon install

# 自定义间隔（例如每 10 分钟）
trace-db daemon install --interval 600

# 指定要监听的 agent
trace-db daemon install --agent codex,claude

# 指定数据库路径
trace-db daemon install --db /path/to/custom/trace.db

# 指定捕获模式
trace-db daemon install --mode full
```

安装后，daemon 会立即启动并在系统启动时自动运行。

### 查看 daemon 状态

```bash
trace-db daemon status
```

输出示例：
```
Status: Running

Daemon details:
{
  "PID" = 27163;
  "LastExitStatus" = 0;
  ...
}

Plist file: /Users/hsw/Library/LaunchAgents/com.tracedb.watch-daemon.plist
```

### 停止 daemon

```bash
trace-db daemon stop
```

注意：停止后 daemon 仍会保持安装状态，在下次定时触发或系统重启时会重新启动。

### 启动 daemon

```bash
trace-db daemon start
```

### 卸载 daemon

```bash
trace-db daemon uninstall
```

这会从系统中完全移除 daemon 服务。

## 日志查看

日志位置取决于平台：

- **macOS**: `~/.config/trace-db/daemon.log`（launchd stdout/stderr）。
- **Linux**: systemd user journal；使用
  `journalctl --user -u tracedb-watch.service -f` 查看。
- **Windows**: Task Scheduler 启动的进程继承宿主输出；建议通过 wrapper 将
  stdout/stderr 分别重定向到文件。安装命令会写入带有 3 次失败重试策略的任务定义。

```bash
# 查看最近的日志
tail -f ~/.config/trace-db/daemon.log

# 查看最近 50 行
tail -50 ~/.config/trace-db/daemon.log
```

日志示例：
```
watch startup: ingested 166, unchanged 4585, skipped 46, failed 35 (27706 ms)
watch filesystem: ingested 5, unchanged 4747, skipped 46, failed 34 (11616 ms)
```

## 工作原理

### macOS (launchd)

daemon 通过 launchd 服务管理，配置文件位于 `~/Library/LaunchAgents/com.tracedb.watch-daemon.plist`。

关键配置：
- `StartInterval`: 定时触发间隔（秒）
- `RunAtLoad`: 登录时自动启动
- `KeepAlive`: true（watch 异常退出后由 launchd 自动重启）

### watch 命令的增量入库

daemon 实际上是定时运行 `watch` 命令。watch 命令的增量入库逻辑：

1. **首次启动**：执行完整的 startup ingest
2. **文件系统监听**：通过 `notify` crate 监听文件变化
3. **智能去重**：
   - 检测每个 session 的修改时间
   - 只解析有变化的文件
   - 跳过 `unchanged` 的 session
4. **定时回退**：如果文件系统监听失败，定时执行 periodic ingest 作为后备

这意味着即使每 30 分钟触发一次，如果没有变化，实际的计算开销非常小。

## 配置建议

### 推荐间隔

- **轻量使用**：`--interval 3600`（1 小时）
- **标准使用**：`--interval 1800`（30 分钟，默认）
- **频繁使用**：`--interval 600`（10 分钟）
- **开发/测试**：`--interval 300`（5 分钟）

### 与 watch 命令的对比

| 特性 | `watch` 命令 | `daemon` |
|------|------------|----------|
| 运行方式 | 前台阻塞，需手动启动 | 后台自动运行 |
| 持久性 | Ctrl+C 退出 | 系统启动自动运行 |
| 文件系统监听 | ✓ | ✓ |
| 定时回退 | ✓ | ✓ |
| 适用场景 | 临时监听、调试 | 日常使用、生产环境 |

## 故障排查

### daemon 状态显示 "Not installed"

运行 `trace-db daemon install` 安装。

### daemon 无法启动

1. 检查二进制文件路径是否正确
2. 查看日志文件：`cat ~/.config/trace-db/daemon.log`
3. 手动测试 watch 命令：`trace-db watch --once`

### 日志文件不存在或无输出

1. 确认 daemon 已启动：`trace-db daemon status`
2. 检查 plist 文件权限：`ls -l ~/Library/LaunchAgents/com.tracedb.watch-daemon.plist`
3. 重新加载配置：
   ```bash
   launchctl unload ~/Library/LaunchAgents/com.tracedb.watch-daemon.plist
   launchctl load ~/Library/LaunchAgents/com.tracedb.watch-daemon.plist
   ```

### 修改间隔或参数

重新运行 `install` 命令，它会自动覆盖旧配置：

```bash
trace-db daemon install --interval 600
```

## 平台实现

- **macOS**: `~/Library/LaunchAgents/com.tracedb.watch-daemon.plist`
- **Linux**: `~/.config/systemd/user/tracedb-watch.service`。安装后使用
  `systemctl --user status tracedb-watch.service` 检查状态；若需要在未登录时继续运行，
  请为用户启用 lingering (`loginctl enable-linger "$USER"`)。
- **Windows**: Task Scheduler 任务 `TraceDB-Watch`，触发器为用户登录。安装命令会
  生成 `TraceDB-Watch.xml`，包含低权限运行、无限执行时限和每分钟最多 3 次失败重启。

服务管理器只负责进程生命周期；`watch` 自身负责增量扫描、文件系统通知和定时回退。
