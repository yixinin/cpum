# CPU Manager - 项目长期约定

## 权限模型（2026-09-14 确定）

- 前台应用 **不提权**：`app.manifest` 用 `asInvoker`，安装包为 per-user NSIS
  （`installMode: "currentUser"`，装到 `%LOCALAPPDATA%`），安装与运行都不弹 UAC。
- **只有 Windows 服务管理需要管理员权限**：`install_service` 通过 PowerShell
  `Start-Process -Verb RunAs -Wait` 提权跑 `sc.exe`；`uninstall/start/stop_service`
  先非提权尝试，access denied 时才提权。
- 规则与 ProBalance 配置 **必须放在用户可写目录**：`%APPDATA%\com.eason.cpum`
  （即 `app.path().app_data_dir()`）。不要改回 `%ProgramData%` 之类的机器级目录，
  未提权的 GUI 写不了。服务以 LocalSystem 运行，通过启动参数拿到该目录。

- 受保护/其他会话进程的修改走**两级升级**：`cpum_core::ipc` 命名管道交给 LocalSystem 服务
  （无弹窗，首选）→ 未装服务时才提权重跑 `cpum_service.exe --set-affinity/--set-priority`（一次 UAC）。
  批量操作（应用规则）只走服务通道，不走 UAC。
- **PPL 进程（csrss、部分杀软/反作弊）任何提权都改不了**，不是 bug，提示里要说清。

## 其他约定

- 所有用户可见文案走 `src/i18n.ts` 字典（zh-CN / en-US 同步），代码注释一律英文。
- 文件统一 UTF-8 无 BOM；写含非 ASCII 的文件不要用 PowerShell `Set-Content`。
- 校验命令：`cd src-tauri && cargo check --workspace`、`npx vue-tsc --noEmit`。
- 改 `installer-hooks.nsh` 后可用 `makensis` 编译最小 .nsi 做语法校验。
