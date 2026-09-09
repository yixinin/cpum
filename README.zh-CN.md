# CPU Manager

[English](README.md)

CPU Manager 是一个用于查看 Windows 进程并管理其 CPU 亲和性的桌面应用。项目采用 Vue 3 前端、Rust/Tauri 后端，并提供可选的 Windows 服务，用于自动重新应用已保存的亲和性规则。

> 项目依赖 Windows API，仅支持 Windows。

## 功能

- 查看 CPU 拓扑：逻辑处理器、物理核心、SMT 线程、CPU 插槽，以及可检测到的 CCD/Die 信息。
- 以层级结构浏览运行中的进程，并实时显示 CPU 和内存指标。
- 使用感知 CPU 拓扑的编辑器，为单个进程立即设置亲和性掩码。
- 新增、启用、编辑、删除及手动应用持久化亲和性规则。
- 规则按可执行文件名进行不区分大小写的匹配；规则名可省略 `.exe`。
- 可安装 `CpumAffinityService` Windows 服务：该服务随系统自动启动，并每 5 秒扫描一次匹配的进程。
- 服务共享规则保存在 `C:\\ProgramData\\cpum\\affinity_rules.json`；读取时会迁移旧版的用户目录规则。

## 使用流程

1. 启动 CPU Manager，等待 CPU 拓扑和进程列表加载完成。
2. 选择一个进程以立即设置亲和性，或者打开规则管理器保存可复用规则。
3. 使用 **应用规则**，将所有已启用规则应用到当前运行的匹配进程。
4. 如果希望在登录或重启后也自动应用规则，请在应用内安装 Windows 服务。

修改亲和性可能影响程序的响应速度和吞吐量。对于混合架构 CPU 或运行低延迟负载的电脑，请先谨慎测试掩码设置。

## 开发环境要求

- Windows 10 或更高版本
- Node.js、npm 与 Yarn（Tauri 的开发启动钩子使用 Yarn）
- 安装 MSVC 工具链的 Rust stable
- Microsoft C++ Build Tools / Visual Studio Build Tools（Rust Windows 工具链所需）
- WebView2 Runtime（通常已随较新的 Windows 系统提供）

应用会请求管理员权限。某些受 Windows 保护的进程，或属于其他安全上下文的进程，仍可能拒绝亲和性修改。

## 开发

安装 JavaScript 依赖：

```powershell
yarn install
```

以开发模式运行应用：

```powershell
npx tauri dev
```

分别执行前端类型检查和 Rust 编译检查：

```powershell
npx vue-tsc --noEmit
cd src-tauri
cargo check
```

## 生产构建

发布脚本会刷新应用图标、构建前端、编译 `cpum_service.exe`，并生成面向整台电脑安装的 NSIS 安装程序：

```powershell
.\\build.bat
```

安装程序输出位置：

```text
src-tauri\\target\\release\\bundle\\nsis\\CPU Manager_0.1.0_x64-setup.exe
```

也可以手动构建前端和 Tauri 安装包。必须先编译 `cpum_service.exe`，以便将它作为打包资源：

```powershell
npm run build
cd src-tauri
cargo build --release --bin cpum_service
cd ..
npx tauri build --bundles nsis
```

## 亲和性规则与服务

规则以 JSON 保存，包含进程名、十六进制 CPU 掩码、启用状态和可选备注。主要的共享保存位置为：

```text
C:\\ProgramData\\cpum\\affinity_rules.json
```

应用可安装、启动、停止和卸载 `CpumAffinityService`。该服务以 `LocalSystem` 身份运行、自动启动，安装时会收到共享规则目录参数。服务会尝试启用 `SeDebugPrivilege`，以便向用户会话中的进程应用规则。

可在提升权限的 PowerShell 中验证服务安装状态：

```powershell
sc.exe qc CpumAffinityService
sc.exe query CpumAffinityService
Get-Content C:\\ProgramData\\cpum\\affinity_rules.json
```

`Running` 仅表示服务正在运行，不能证明规则匹配到了进程，也不能证明 Windows 接受了亲和性掩码。排查问题时，请确认服务可执行文件路径与参数、规则文件以及目标进程的实际亲和性。

如需对规则进行一次性的诊断应用，可执行已安装的服务程序：

```powershell
& "<cpum_service.exe 的路径>" --apply-once C:\\ProgramData\\cpum
```

## 项目结构

```text
src/                         Vue 3 前端
  components/                亲和性与持久化规则对话框
  composables/               拓扑、进程、指标和进程树状态管理
  api.ts                     Tauri IPC 封装
src-tauri/                   Rust/Tauri 后端
  src/process.rs             进程枚举、指标、亲和性操作
  src/topology.rs            CPU 拓扑检测
  src/bin/cpum_service.rs    Windows 服务可执行程序
build.bat                    NSIS 发布构建脚本
```

## 注意事项与限制

- 亲和性掩码使用 64 位数值。界面会标注系统是否受限于单个处理器组；多处理器组系统会受到 Windows 亲和性机制限制。
- 网络 I/O 计数器仅在受支持的 Windows 版本上可用，其他版本可能显示为零。
- 保存规则后不会立即自动应用；请使用 **应用规则**，或等待服务检测到匹配进程。

## 许可证

本仓库尚未声明许可证。重新分发或在既定范围外使用前，请联系仓库所有者。
