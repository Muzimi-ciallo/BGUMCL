# BGUMCL 1.3.30 (test) PCL 下载源与切换逻辑对齐设计

## 目标

为 BGUMCL 制作一个仅用于测试的 `1.3.30 (test)` Windows x64 便携版，使其在大陆网络环境下尽可能复现 PCL 的下载源组合、源优先级、失败切换、动态并发和尾部恢复行为。

本次构建只交付便携版，不生成安装版、Linux/macOS 版本，不推送 GitHub，不创建 Release，也不修改远程更新清单。

## 当前问题

当前 BGUMCL 的无 BMCLAPI 测试版在 Forge 官方 Maven 速度较低时会直接触发低速失败；MOD 分段下载在连接被重置后会重复重试，剩余少量文件容易长时间停留在恢复阶段。现有源健康状态主要按单个下载任务维护，不能像 PCL 一样让同一任务组快速避开表现不佳的源。

## 设计原则

1. 复现 PCL 的行为和源映射，不直接移植 VB.NET 实现。
2. 官方源和镜像源始终形成候选集合，源切换不依赖重新生成整合包清单。
3. 低速只用于触发切换；在没有备用源或仍有数据流入时不直接失败。
4. 首次成功的源在同一文件内保持优先，只有连接错误、超时、范围响应异常、校验失败或持续停滞时才切换。
5. 取消、断点续传、SHA1 校验和任务清理行为保持兼容。

## 源映射

| 资源 | 官方候选 | 镜像候选 |
| --- | --- | --- |
| Minecraft 清单、版本 JSON | Mojang 官方 | BMCLAPI 对应路径 |
| Assets | `resources.download.minecraft.net` | BMCLAPI `/assets` |
| Minecraft Libraries | `libraries.minecraft.net` | BMCLAPI `/maven`、`/libraries` |
| Java 运行库 | Mojang 官方 | BMCLAPI |
| Forge/Fabric/NeoForge | 官方 Maven/元数据 | BMCLAPI Maven/元数据 |
| OptiFine 列表与文件 | `optifine.net` | BMCLAPI OptiFine API/文件路径 |
| CurseForge | ForgeCDN/API | MCIM |
| Modrinth | Modrinth CDN/API | MCIM |
| GitHub 文件 | 直连 | Gitee、gh-proxy、CDN 候选 |

默认排序与 PCL 设置保持一致：官方优先，连接缓慢或失败时切换镜像；镜像优先设置则反转官方和镜像顺序。

## 下载流程

### 候选选择

- 为每个下载参数生成有去重、可标识来源类型的候选列表。
- 对 CurseForge/Modrinth 保留官方 CDN 变体与 MCIM 映射。
- 对 Mojang/Forge/Loader 资源生成官方地址和 BMCLAPI 对应地址。
- 对 OptiFine 保留 BMCLAPI 专用路径，并实现官方 `adloadx/downloadx` 解析作为备用。
- 版本清单响应解析失败时继续下一个候选，全部失败后使用有效缓存。

### 源健康与切换

- 在任务组级别维护按主机或源类型聚合的成功、超时、连接重置、HTTP 错误、响应停滞和校验失败统计。
- 对短暂 403/429 采用延迟重试和降权，不立即永久屏蔽源。
- 对持续无数据、连接超时、Range 响应错误或校验失败立即切换候选。
- 对持续低速但仍有数据的连接先降低优先级或切换新文件；没有备用候选时继续当前连接。
- 一个源被任务组判定为不健康后，剩余新任务不再重复优先尝试该源。
- 单个文件在同一源成功建立连接后，默认保持该源完成所有分段。

### 分段、并发与恢复

- 仅对适合 Range 的 MOD/资源文件使用分段。
- 分段数量根据文件大小和当前主机健康状态动态调整。
- 出现 `ConnectionReset` 或连续 Range 失败时按 4 段、2 段、单流逐级降级。
- Forge 等小型 Maven 文件默认单流，避免无意义的 Range 请求。
- 任务组采用受限的动态工作池，依据总体速度和连接错误调整并发，不固定对所有文件同时建立最大连接数。
- 最后少量文件进入尾部阶段后自动降低并发并延长等待时间。
- 失败恢复只重试失败文件，保留可验证的断点和已完成文件。

## 错误与进度处理

- 有持续字节流的下载不能仅因瞬时速度低而立即失败。
- 低速状态必须在日志中区分为“触发切换”和“最终失败”。
- 任务组仅在所有候选源、恢复模式和重试次数都耗尽后失败。
- 进度以实际写入字节为准，分段失败、源切换和单流恢复不得重复累加。
- 取消任务时停止所有子请求、清理无效临时文件，并阻止后续自动复活。
- 日志包含文件名、当前源、候选源、切换原因、重试次数、HTTP 状态和最终错误。

## 代码范围

- `src-tauri/src/utils/web.rs`：统一源 URL 映射、MCIM 映射和候选去重。
- `src-tauri/src/resource/helpers/misc.rs`：资源源优先级和官方/BMCLAPI API 映射。
- `src-tauri/src/resource/helpers/version_manifest.rs`：多源解析失败回退和缓存回退。
- `src-tauri/src/resource/helpers/loader_meta/`：Forge、NeoForge、OptiFine 元数据回退。
- `src-tauri/src/instance/helpers/loader/`：安装器和运行库候选源。
- `src-tauri/src/tasks/download.rs`：候选源选择、源健康、低速处理、分段降级和恢复。
- `src-tauri/src/tasks/monitor.rs`：动态并发、连接池和任务组级调度。
- `package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`、`src-tauri/Cargo.lock`：版本号 `1.3.30`。

不会删除现有的 BMCLAPI 测试特性；它保留作为对照构建入口，但本次 `1.3.30 (test)` 使用完整的官方 + BMCLAPI 源策略。

## 验证计划

1. 对源映射、优先级、候选去重和 MCIM URL 转换做单元级检查。
2. 模拟 JSON 损坏、HTTP 502/403/429、连接重置、Range 不支持、低速持续传输和 SHA1 不一致。
3. 验证同一任务组共享源健康状态，并验证只重试失败文件。
4. 使用近期日志中的 Forge 运行库和 MOD 文件验证尾部恢复。
5. 执行 `cargo check`、前端生产构建和 Windows x64 release 构建。
6. 仅封装便携版，检查 PE 头、便携标记、文件大小和 SHA256。

## 验收标准

- Forge 官方源速度较低但仍有数据时，不再立即导致任务组失败。
- Forge 官方源不可用时，能够切换 BMCLAPI Maven。
- MOD 出现连接重置时能够降低分段级别并继续完成。
- 最后几个文件完成后任务组能正常结束，不需要手动停止/重新启动。
- CurseForge、Modrinth、湾大整合包和 `.mrpack` 导入流程保持可用。
- 只生成 `BGUMCL_1.3.30_test_windows_x86_64_portable.exe`。
