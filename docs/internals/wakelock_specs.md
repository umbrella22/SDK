# WakeLock / Inhibit 规格说明

## 1. FreeDesktop ScreenSaver（Linux 首选）

```text
Service: org.freedesktop.ScreenSaver
Path  : /org/freedesktop/ScreenSaver
Interface: org.freedesktop.ScreenSaver
```

### Inhibit

```text
UInt32 Inhibit(String app_name, String reason)
```

| 参数 | 说明 |
|------|------|
| app_name | 调用方应用名 |
| reason | 申请锁的原因说明 |

返回值 `cookie: u32` 是后续 `UnInhibit` 所需的句柄。

此接口不接收 flags，不能选择系统挂起抑制。原生 `LinuxWakeLockManager`
仅支持 `PreventDisplaySleep`；`PreventSystemIdle` 在发送任何 D-Bus 请求前
返回 `UdaError::NotSupported`，供上层继续尝试其回退路径。

原生 capability 仅探测 ScreenSaver：可达时返回 `WAKE_LOCK` 和带原因的
`SupportLevel::Partial`（仅支持显示空闲抑制）；不可达时为空和 `None`。
FFI 的 `systemd-inhibit --what=idle:sleep --mode=block` 回退属于 FFI 层，
不能算入原生 manager 的能力。

协议来源：
- https://specifications.freedesktop.org/idle-inhibit-spec/latest/
- https://github.com/KDE/kscreenlocker/blob/master/dbus/org.freedesktop.ScreenSaver.xml

### UnInhibit

```text
void UnInhibit(UInt32 cookie)
```

收到 `UnInhibit(cookie)` 后，会话管理器应立即解除对应的抑制状态。

## 2. XDG Desktop Portal（备选）

Portal 未在当前 Phase 1 实现中启用，保留为下一 Phase 的可选升级路径。

## 3. RAII 语义

`WakeLockGuard` 实现 `Drop`，确保进程崩溃或提前返回时锁必然释放，避免残留 cookie 导致系统长期被抑制。
