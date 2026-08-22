# 十项优化 TODO 与完成记录

本文最初记录面向“64 位 Linux 服务端、单实例、现代桌面 Chromium/Edge/Firefox、HTTPS 网关转发到内网 HTTP 端口”的十项质量优化。它现在作为历史实施清单与验收索引保留；各节“完成内容”已经同步为当前工作树，包括原十项完成后继续落地的安全、并发、性能和交付整改。

因此，`[x]` 表示对应优化方向已完成并持续受当前自动化保护，不表示最初采用的具体算法永远不变。原先的 `limit + 1` 分页、串行/共享目录浏览器测试以及固定测试数量等陈述已被后续实现取代；目录 ZIP 实现及其发送期 permit 后来也已整体移除。README、功能清单和流程文档仍是当前行为的规范说明，本文不重复全部使用步骤。

状态：

- `[x]`：代码、文档和对应自动化验证已经落地；
- `[ ]`：尚未完成，不能在交付时保留。

## 总清单

- [x] 1. 收紧上传身份、正文边界与崩溃持久化事务
- [x] 2. 让全部文件发现和维护操作锚定启动时共享根 fd
- [x] 3. 建立连接、请求、上传、搜索 与磁盘资源预算
- [x] 4. 优化大目录、递归遍历、排序、Range 与 MIME 路径
- [x] 5. 将日志改为有界异步输出并统一动态字段转义
- [x] 6. 模块化并加固现代桌面浏览器前端
- [x] 7. 固定并严格校验 Argon2id 账号策略
- [x] 8. 改善 Rust 分层、请求上下文、错误边界与存储可测试性
- [x] 9. 精简依赖和部署能力，建立统一本地检查入口
- [x] 10. 完成全量故障/浏览器验证并形成可追踪、可验签的本地 Git 制品

## 1. 上传身份、正文边界与落盘事务

完成内容：

- 删除跨刷新 `localStorage` 上传恢复。重新选择文件始终生成新 upload ID，不能再仅凭文件名、大小和 `lastModified` 把另一份内容接到旧暂存文件后；
- 批次入队前把每个文件解析为最终绝对逻辑路径，再向 `POST /__dufs__/api/upload/preflight` 一次提交 1～512 个互不重复的路径；解码后的 UTF-8 路径总量最多 256 KiB，JSON wire body 最多 2 MiB。响应必须与请求路径数量、顺序和值完全一致，并为每项给出 `exists`、`replaceable` 和可选 revision，否则失败关闭。没有已存在目标时零确认直接上传；只有已存在且可替换的文件弹出覆盖/跳过/取消对话框，不可替换目标不会被自动覆盖；
- 预检只改善交互，不承担原子性。缺少 `X-Dufs-Upload-Overwrite` 或显式 `false` 的 PUT/PATCH 使用原子 no-replace；`true` 必须携带 64 位小写十六进制 target revision。revision 绑定账号摘要、规范根内目标路径和完整 replacement CAS identity，并在 rename 前再次核对，因此目标在预检后出现或变化时不会被旧确认覆盖；
- 保留同一页面、同一 `File` 对象的已知失败重试：先用 HEAD 核对绑定当前账号摘要的服务端记录。SQLite schema v3 的 `upload_sessions` 在内部严格区分 `Running/CommitStarted/AwaitingConfirmation/Committed/Rejected/Unknown`，对外映射为 `running/awaiting-confirmation/committed/rejected/unknown`，响应另接受没有持久记录的 `not-seen` 和仅描述本次请求未启动的 `not-started`。`not-started` 可以显示 Retry，但点击后仍先查询原 ID；只有 offset 未满的 `running` 才能续传 PATCH，满 offset `awaiting-confirmation` 只能明确发布或丢弃，`rejected/not-seen` 换新 ID，`committed` 精确匹配才成功，`CommitStarted/Unknown` 仍按结果未知处理并禁止盲目重试；
- 服务端要求 PUT/PATCH 声明总长度和精确 offset，最多写入本次声明的剩余字节，并继续检查正文确实结束；额外正文返回 `413`；
- 每次 PUT/PATCH 在等待路径租约前建立绝对总 deadline，覆盖租约等待、上传准备、正文读取、写入、flush、metadata 重放和等待最终提交确认。合法头先解析，随后依次取得路径租约、全局上传槽，再以受跟踪任务完成 route metadata，最后进入 owner checkpoint/上传处理；路径或 route 超时以及槽满都返回绑定的 `not-started`，并且不会改变任何旧记录。为约束慢文件系统准备工作的资源，槽满会直接 `429 not-started` 而不先查询 owner state；前端点击 Retry 后必须 HEAD，届时才会取得旧 ID 的真实 partial/terminal 状态；
- 前端把正文传输和提交确认分开计时：传输空闲 2 分钟、总计 24 小时，正文发完后清除传输计时器并进入最长 5 分钟的 `Submitting…` 阶段；
- 在读取最终长度前执行 Tokio `flush`，避免异步文件写队列尚未完成时偶发把正确上传误判为 `409`；
- 完整提交顺序固定为：`flush` 等待正文写队列、确认正文长度、覆盖时重放允许的目标 metadata、`sync_all` 暂存文件、持久化满 offset 的 `CommitStarted` 歧义屏障、在 rename 紧前复核目标 revision 与 stage 身份、同目录原子 rename、`fsync` 父目录、持久化 `Committed` 终态；前端还会核对精确长度/offset，全部成功后才显示成功。普通 rename 前确定拒绝会清理会话并尽力写入 `Rejected`；目标 CAS 冲突则保留完整 stage 并改为 `AwaitingConfirmation`。rename 后持久性或终态落盘不确定时尽力把 `CommitStarted` 改为 `Unknown`，即使该写入也失败，文件型数据库重启时仍会把遗留 `CommitStarted` 恢复为 `Unknown`；
- 上传超时、磁盘水位不足、连接取消和强制停止分别保存有效检查点或删除无效暂存，不提前覆盖旧目标；
- 最终 rename/fsync 已开始后不会因浏览器断开或 deadline 到期而被取消，后台任务继续持有路径和维护租约直到安全收尾；rename 可能已可见但父目录同步未确认时返回 `500`、upload ID 和 `unknown`，而不是声称可以安全重试；
- stage 以 `0600` 创建，新建目标最终也保持 `0600`；上传状态只查询文件型 SQLite schema v3 的 `upload_sessions`，它是唯一状态权威，共享根内不写入、读取或导入 JSON 上传状态文件，且 `state-dir` 是必填配置。部分 `Running` 使用 PATCH 实际采用的同一个可写 no-follow stage fd 校验普通文件、`nlink == 1`、长度和已记录 dev/inode，再在该 fd 上截断未确认尾部并 seek；`AwaitingConfirmation` 保留同一满 stage 和 target revision，`CommitStarted` 则是提交歧义屏障，重启恢复为 `Unknown`，不会因 stage 已只读、已 rename、缺失或被替换而降格为 `not-seen`；
- 覆盖单链接普通文件时保留 numeric uid/gid、除 setuid/setgid 外的 mode 及允许的非特权 xattr。原目标带 setuid/setgid，或存在任何 `security.*`、`trusted.*`（capability、SELinux、IMA/EVM、overlay 等）都会拒绝覆盖；`user.*` 与 `system.posix_acl_access` 可精确重放。名称列表最多 64 KiB、条目最多 1024 个、单值最多 64 KiB；先查询值的精确长度，再使索引容量、带 NUL 名称和全部值的总分配保持在 1 MiB 内，不为每项预分配 64 KiB，并移除 stage 上多余属性。提交前再次复核目标与 stage 快照；目录、多硬链接和其他已知策略/格式/权限冲突进入统一 target policy 并以带同一上传 ID/长度的 `409 rejected` 拒绝，其他 metadata 基础设施错误返回安全 `5xx`；
- 存储提交边界区分 `NotPublished`、条件冲突与 `PublishedDurabilityUnknown`：文件同步、非 CAS 复核或 rename 失败会清理旧 stage/控制记录并尽力写入 `Rejected`；CAS 冲突不发布目标，保留满 stage 为 `AwaitingConfirmation`。用户接受当前目标时以同一 ID、满 offset、空正文 PATCH 和最新 revision 发布，无需重传；目标再次变化会再次确认，跳过则调用 `POST /__dufs__/api/upload/discard`。若 stage 已带旧目标 metadata 而目标随后消失，服务返回 `upload_metadata_preservation_refused`，前端先 discard，再用新 ID 完整 create-only PUT，避免把旧属性带到新文件。rename 后父目录同步失败，或已持久化发布但 `Committed` 终态写入失败，都报告 `unknown`；
- fresh PUT 在创建祖先、stage 和 `upload_sessions` 记录前从最近存在的父目录完成空间准入，空间不足不会留下目录；准入成功后自动创建的祖先带身份记录，后续会话准备在正文前失败时，自底向上只回收仍为空、身份未变且由本请求创建的目录。拥有共享根写权限的外部进程仍属于明确的信任边界。

主要验证：

- 单元测试覆盖超长正文、正文空闲/总超时、磁盘预留计算和提交阶段故障注入；
- Playwright 用“相同名称、大小、mtime，不同内容”的旧 localStorage 记录验证页面使用新 ID 完整 PUT；
- Playwright 覆盖无冲突零确认、已知冲突批量选择、预检后目标出现/变化、同一 stage 空 PATCH 确认、再次变化再次确认、显式 discard、metadata 安全例外重传，以及 `unknown` 永不自动覆盖；同时继续覆盖提交等待/超时和 `not-started` 的 HEAD-first Retry；
- 真实半包 HTTP 上传持续到 30 秒停机宽限结束，force token 触发后验证 20 MiB 检查点已经持久化且最终目标尚未出现；停机实现随后只有 10 秒强制收尾窗，约 40 秒硬截止会跳过日志 flush 并立即以状态 1 退出。正常完成 tracked cleanup 后由专用命名 OS thread 只做一次、最多 5 秒的日志 flush，再显式 `exit(0)`，避免 Tokio blocking pool 或 runtime drop 等待已取消但卡在内核/FUSE 的工作；主任务在 flush 期间仍优先响应第二信号并立即以 130/143 退出。

## 2. 根 fd、符号链接别名与并发写协调

完成内容：

- `RootedFs` 在启动时长期打开共享根，要求 Linux `openat2`，并对根目录 fd 取得非阻塞独占 advisory `flock`；同一共享根上的第二个 Dufs 实例会启动失败；
- 路由 metadata、直接列表、递归搜索和内部维护扫描全部从该根 fd 使用 `openat2`/`*at`，不再通过启动路径字符串重新发现文件；
- 目录展示的名称、类型和 metadata 来自同一次 fd 锚定枚举；绝对链接和根外链接在发现阶段即不可见；
- 维护任务只删除从原根 fd 发现、在父目录 fd 下再次操作的严格内部名称；递归清理全程使用 `openat(..., O_NOFOLLOW)`、`statat` 和 `unlinkat`，不再通过 `/proc/self/fd` 拼回路径；
- 路径协调器同时比较词法路径和由目录设备号/inode 构成的语义键；真实路径与根内相对符号链接别名不能并发修改同一对象；
- 较早 waiter 仍在解析语义键时只阻塞与其词法路径有祖先/后代关系的后续请求，无关路径可超车，不会因一次慢解析形成全局队头阻塞。协调 epoch 变化会让 waiter 重新解析全部语义键，并在插入租约前原子核对版本、现有 lease 和更早冲突 waiter；符号链接从 A 改指 B 时不会沿用指向 A 的过期键，语义别名仍不能并发。语义解析错误会改用以共享根 inode 为锚、与所有路径冲突的保守 wildcard 租约，随后由 handler 返回原根边界/I/O 错误；既不退化成纯词法租约，也不因永久错误无限重试；
- 活跃上传暂存使用“父目录设备号/inode + 内部文件名”登记，维护任务从真实路径发现文件时也能识别经根内别名发起的上传；
- 维护扫描和 delete trash 回收只跨片保存根内相对路径与进程内 cursor，每片重新打开工作目录并关闭临时 fd，打开 fd 数不随目录深度增长。DELETE 在可见 rename 前先把含目标/trash 相对路径和源 dev/inode/类型的 `Prepared` job 写入 SQLite；rename 与父目录 `fsync` 后转为 `Ready`，worker 原子 claim 为 `Claimed`。purge I/O 错误把 job 持久化回 `Ready`，attempt 递增并从 100 ms 指数退避到最长 30 秒，没有固定重试次数上限，其他 ready job 可以越过；重启把遗留 `Claimed` 恢复为立即可重试的 `Ready`，独立 reconciler 持续按路径和 inode 收束 `Prepared`。递归打开使用 `RESOLVE_NO_XDEV`，嵌套或 bind mount 不会被越过；readdir EOF 后若目录删除仍为 `ENOTEMPTY`，丢弃 EOF 句柄并从 cursor 0 重扫并发新增项；
- 删除根目录、原子不覆盖 move/rename、上传覆盖和目录创建继续使用父目录 fd 及 Linux 原子操作。move/rename overwrite 对不同名称但相同 dev/inode 的硬链接在预检和 commit 内分别做 fd-relative 复核，返回 `409 source_equals_destination`，不会把 POSIX rename no-op 误报为 `204`。

主要验证：

- 运行中把共享根重命名并在原路径创建替换目录，列表、下载和维护仍只作用于启动时打开的原根；
- 单元测试验证共享根独占锁会拒绝第二个实例，并在首个根句柄释放后允许再次启动；
- 单元测试覆盖符号链接别名与真实路径的同对象租约冲突，以及互不相关兄弟路径仍可并行；
- 根外、绝对和根内相对符号链接集成测试保持通过；悬空或成环的根内相对链接可以列出并由 DELETE/PUT 管理，GET 不会跟随无效目标；
- 递归删除测试验证分片、取消和恢复均保持在已打开根内，符号链接不能把清理导向根外。

## 3. 统一资源预算

完成内容：

- 所有监听器共享活跃连接上限，默认 256；每个 listener 先独立 accept，再为已接受 socket 取得许可，空闲地址不会预占连接槽；空 bind 列表在启动前被拒绝；
- 后端仅启用 Hyper HTTP/1 handler（接受 HTTP/1.0/1.1），请求头读取限时 10 秒、最大缓冲 64 KiB；HTTP/2 prior-knowledge preface 和 h2c upgrade 不进入另一套缺少等价预算的协议路径；
- 普通请求处理并生成响应头默认限时 300 秒；文件和 Range 正文传输不属于该总时限，但服务端套接字连续 30 秒没有写入进展会断开。持续有进展的正文没有应用内总时长/最低速率，公网策略仍由网关补充；
- 上传具有单文件大小、正文空闲时间、从路径租约前开始的绝对总时间、并发数和最低剩余磁盘空间限制；
- 搜索具有并发数、遍历项数、真实物化内存和运行时间限制；可配置遍历上限不得超过硬上限 100000；
- 上传按实际目标所在 Linux `st_dev` 分桶跟踪磁盘空间；逻辑长度和约 1 MiB + 64 KiB 的 xattr/checkpoint/目录项等余量分别按分配单元向上取整后预留，不同文件系统互不误拒；
- 空间准入和复核所需的 `fstat`/`fstatvfs` 在 blocking worker 且在共享预留 mutex 外执行；快照返回后只在同设备 revision 未变化时记账，最多重试 8 次，持续竞争失败关闭，其他设备变化不触发重试。文件系统 block 数与 fragment size 相乘、取整或预算相加溢出时同样失败关闭。上传约每写 8 MiB 异步重新核对空间；
- 列表/搜索和上传使用 `try_acquire`，满载时返回明确的 `429`；上传超限、超时和空间不足分别返回 `413`、`408`/`504`、`507`；
- permit、空间预留、文件和路径租约通过所有权 guard 在完成、错误或取消时释放；移入 blocking worker 的许可保持到实际工作退出，请求取消不会提前释放资源；
- 普通目录和递归搜索的第一页扫描共享搜索槽，后续 cursor 页只读取有界快照，不再次占用扫描槽；
- SQLite `purge_jobs` durable outbox 的容量为全局 4096、每账号 1024，`Prepared/Ready/Claimed` 未完成 job 没有 TTL 或固定失败次数逃生口；满载会在可见 rename 前返回 `503 purge_backlog_full`。单 worker 每片最多处理 256 项或 25 ms，未完成 job 在进程内轮转；错误持久化 attempt 并从 100 ms 指数退避到最长 30 秒，其他 ready job 可越过。defer/complete 的 state-store 回复丢失时，worker 有界保留本地 claim 并在继续前回读确认；重启将 `Claimed` 恢复为 `Ready`，运行期 reconciler 收束 `Prepared`。过期内部文件维护每批最多扫描 1024 项或 100 ms，低频根扫描只兜底未记账 orphan trash，二者均可响应停机。

上传空间预留封闭了 Dufs 进程内检查后的并发写竞态。外部进程、宿主机对 virtiofs 导出目录的直接写入以及存储侧异步空间变化不受本进程锁控制，仍可能在最后一次检查后竞争空间；部署时仍应保留合理水位并监控底层文件系统。

配置项和默认值集中在 `Args`，命令行与 YAML 都使用同一严格校验；零并发、零响应准备时限、不一致或超过 365 天/平台单调时钟范围的时限，以及超过 Tokio 信号量上限的并发配置都会阻止启动。

## 4. 大目录、遍历、排序、Range 与 MIME

完成内容：

- 目录 HTML 只包含页面上下文和 CSRF，不再注入完整目录 JSON/Base64；
- 受认证的 list API 默认返回 200 项，最大 500 项，并支持 `sort`、`order`、`q` 和不透明 cursor；
- 最初的“每页重扫目录并只保留 `limit + 1` 个候选”已经被不可变内存结果取代：第一页有界扫描并物化完整结果、只排序一次，后续页按 offset 做 O(page) 切片，不再产生 O(N×页数) 的重复扫描和排序；稳定的索引归并排序在构造、合并和最终置换的每个有界步骤检查取消与 deadline，不会等最坏规模的整轮排序结束才响应；
- 普通目录和递归搜索最多物化 100000 项；进程内缓存绝对存活 120 秒，总计最多 32 个/64 MiB、每账号最多 8 个/32 MiB。过期、进程重启或被容量淘汰后旧 cursor 返回 `409`，客户端从第一页重新开始；
- cursor 包含随机快照 ID、offset 和抗篡改 tag，并绑定认证账号摘要、路径、目录设备号/inode、纳秒级 mtime/ctime、排序、查询和 limit；编码/版本无效、跨账号或其他请求绑定不匹配返回 `400`，tag 不匹配、快照未知/过期/淘汰/不可用和父目录结构变化返回 `409`；
- 直接列表会在扫描前后复核当前目录；递归搜索在访问每个目录前复核已捕获身份，并在完成后再次复核所有访问过的目录。可观察变化返回可重试 `409`；前端在后续页收到 `409` 时丢弃已加载结果并从第一页重载；
- 构造完成后的各页来自同一不可变内存结果，不会因后续变化重复或混入新项；但遍历复核不是原子文件系统快照。检查间发生又恢复的变化、未反映到目录元数据的子文件原地内容/权限变化以及最终复核后的变化仍可能不可见；强一致读取必须使用只读存储快照或等价版本化源；
- active-ancestor `HashSet` 按最大深度一次性预留并保守预检；搜索结果 `Vec` 和名称字符串在扩容前同时核算旧、新缓冲区的瞬时峰值，只有峰值仍在 32 MiB 预算内才增长；
- 名称小写排序键在构造条目时只分配一次，并以原始名称形成确定的最终次序；
- 搜索在第一个 tracked blocking worker 中遍历、逐条转换 `PathItem` 并按结构/字符串/排序键真实容量有界收集，再在第二个 tracked blocking worker 中执行可中断稳定排序；同一 permit 持续到两者退出。
- 单段 Range 会把超出文件末尾的 end 截断，超长 suffix 返回完整表示的 `206`；逗号多段及重复 `Range` 请求头都返回 `416`，`If-Range` 存在时保守返回完整 `200`。构建只允许 64 位 Linux，消除 32 位长度截断边界；
- 附件 MIME 只按扩展名映射，未知名称固定为 `application/octet-stream`；不再读取内容样本、回 seek 或猜测 charset，移除了 `content_inspector` 与 `chardetng`。

主要验证：

- 1205 个新增文件以 37 项分页，断言无重复、无遗漏且顺序稳定；
- cursor 的目录项结构变化、排序条件不匹配、过期/淘汰、tag 篡改和容量边界分别有回归测试；
- 递归搜索测试会在第一页后删除旧匹配并添加新匹配，确认后续页仍返回第一次搜索的不可变结果；
- 提供默认忽略的 100000 项真实目录基准测试；每周计划工作流用 release 构建执行并以 30 秒宽松上界作为回退门，日志保留实际第一页完整扫描、排序和缓存耗时；
- Range 覆盖空文件、超长 suffix、超出末尾、非法倒序和溢出数字；
- MIME 回归覆盖已知二进制、未知二进制和文本；
- 普通 GET 测试覆盖路由分类后被 FIFO 替换时仍从同一非阻塞 fd 拒绝。

## 5. 有界异步日志

完成内容：

- 文件、stdout 和 stderr 共用容量 4096 的同步有界 channel 与独立写线程；
- HTTP 访问日志在动态字段拼接阶段就使用 16 KiB 有界构造器，重复变量不会先形成巨型临时字符串；全部日志在入队前再执行同上限的 UTF-8 安全截断并带唯一固定标记；
- 自定义格式最多 4096 字节和 128 个解析元素，配置超限会阻止启动；
- 请求线程只执行非阻塞 `try_send`，慢磁盘或日志接收端不会直接卡住 Tokio worker；
- 写线程批量写入并每 250 ms flush，不再逐行强制刷新；
- 队列满时丢弃最新记录，运行中至多每秒输出一次聚合 `dropped_newest` 告警；显式 flush 和退出仍报告尚未输出的累计数；
- 正常停止的 flush 由专用命名 OS thread 执行并最多等待 5 秒，不依赖共享 blocking pool；第二信号在等待期间仍立即强退；
- URI、用户名、普通请求头、handler 错误和连接错误都经过单行控制字符转义；
- Authorization、Proxy-Authorization、Cookie 和 CSRF 请求头继续统一脱敏；
- `--log-file` 使用 `O_NOFOLLOW|O_APPEND|O_NONBLOCK|O_CLOEXEC` 打开，要求当前服务用户拥有、单硬链接的普通文件，并设置和复核 `0600`，拒绝符号链接、异常对象和硬链接替换；
- 默认访问日志加入 mutation operation ID/state，便于把 `running/succeeded/failed/unknown/rejected` 与最终 HTTP 状态关联，而不记录凭据。

## 6. 现代桌面浏览器前端

完成内容：

- 动态文件名、路径、用户和错误只通过 `textContent`、属性 API、`URL` 和 `DocumentFragment` 进入 DOM；禁止 `innerHTML`、`insertAdjacentHTML` 等动态 HTML 接口；
- 上传、新建、重命名、移动、删除、加载更多、取消和注销使用原生按钮、英文可访问名称、可见焦点和 `aria-live` 状态；点击新建会直接原子创建 `newfolder`/`newfile`（确定重名才追加 `(2)` 等后缀），随后与 Rename 共用名称列中的单一行内编辑器。Enter 或合法失焦提交，Escape 取消编辑但保留已经创建的默认项。Move、覆盖、删除和操作错误继续复用具有标题、说明和显式表单标签的原生 `<dialog>`，所有场景都不调用浏览器 `prompt`、`confirm` 或 `alert`；
- 应用自定义的用户可见界面、状态、提示和错误统一为英文；浏览器原生错误仍可能跟随浏览器语言；
- 生产脚本不引入打包器，拆分为 `api.js`、`path.js`、`dom.js`、`http_headers.js`、`response_buffer.js`、`listing.js`、`operations.js`、`upload.js`、`upload_preflight.js`、`upload_protocol.js`、`app.js`；严格无符号 HTTP 头解析由 `http_headers.js` 共享，Fetch 正文上限、取消与重放流由 `response_buffer.js` 实现，`upload_preflight.js` 严格绑定预检请求/响应的路径与类型，上传头名、允许状态码及按当前文件总长度绑定的单一状态解析由 `upload_protocol.js` 集中定义；
- 目录页 JavaScript 主动发起的 Fetch 统一经过 `api.js` 的 30 秒 AbortController deadline，并委托 `response_buffer.js` 读取正文；原生导航、登录表单和文件下载不在该边界内。调用方 signal 在调用时已经取消会于分发前明确返回 `client_cancelled` 且不调用 `fetch`；进入 `fetch` 后的取消、deadline 或网络中断无法证明写请求未到达服务端，mutation 仍保守为 outcome unknown。读取正文前通过 `http_headers.js` 检查严格 `Content-Length`，随后以 `ReadableStream` 逐块累计，错误/成功正文分别以 16 KiB/16 MiB 为硬字节上限，超限立即 cancel；允许范围内直接以保留的已校验分块重放正文，不再先合并为第二份连续 `Uint8Array`，重建的 `Response` 仅保留 body/status/statusText/headers。Problem Details 的 `detail`/`title` 最多接受 1024 个 UTF-16 code units，且只接受 canonical `application/problem+json` 和平铺 snake_case 扩展；16 MiB 成功上限留足 500 项、接近 PATH_MAX 且经 JSON 转义的合法列表。上传 XHR 在响应头、下载 progress 和最终 UTF-8 长度三个阶段拒绝任何超过 16 KiB 的响应（正常成功响应应为空），并区分 authentication、CSRF、conflict、network、timeout 和 outcome unknown，但不证明浏览器事件前没有内部缓冲额外网络块；
- 普通 operation 与上传使用不同且严格的状态词汇：前者响应接受 `running/succeeded/failed/rejected/unknown`，其中 `rejected` 是冲突/容量等已知未执行拒绝，成功还必须回显同一 ID 和 `succeeded`；状态查询记录本身只会是 `running/succeeded/failed/unknown`。上传另接受 `awaiting-confirmation`，且只在精确 `409`、同一 ID/长度、满 offset、稳定错误码以及合法 target revision/replaceable 头共同成立时信任。fresh PUT 只有 `200/201`、同一 ID、`committed` 和精确 length/offset 才成功；新建零字节文件也使用 create-only。服务端绑定的 `not-started` 保留已知未启动语义；`running/rejected/not-started` 的 Retry 仍先 HEAD。直接 `not-seen`、显式 `unknown`、缺失/非法状态、revision 异常或 committed 不匹配都失败关闭，绝不降级为覆盖；
- 上传队列支持进度、速度、剩余时间、已知失败重试、传输取消、独立提交等待、二次冲突确认、结果未知、认证暂停和顺序恢复；每批、pending DOM 和终态历史都有独立上限；
- mkdir、move、rename 和 DELETE 使用 UUID operation ID；网络失败或 `504` 不会自动重发写请求，而是只查询一次受认证 job 端点并严格验证 `job_id`/state；查询本身的认证、协议、网络或超时错误仍保守为 unknown；
- operation ID 被另一请求指纹复用时返回 `409 operation_id_conflict` 和统一的 `rejected` 状态；内部 API 与摘要资源路径只接受唯一规范 URI，外层 timeout/operation 分类和 handler 共用同一次解析；
- 已知摘要资源对 GET/HEAD 返回相同 metadata，HEAD 不发送正文；目录请求中只要存在已移除的 `zip` 查询 key，GET/HEAD 都稳定返回 `410 Gone`，且 HEAD 无正文；
- 拖放仅被阻止触发浏览器导航，不再构成上传入口；
- `Permissions-Policy` 在登录页和目录页统一关闭相机、麦克风、地理位置、支付和 USB；
- `scripts/check-js.mjs` 对生产和测试 JavaScript 执行 Node 语法与确定性格式检查，并固定使用 Acorn 8.17.0 解析 AST；生产模块的有界词法常量分析覆盖字符串拼接、模板、数组 `join`、别名、反射和动态全局属性访问，动态 computed 解构的属性名无法静态求值时，在变量声明、赋值表达式和默认参数（含嵌套及 const alias）中都失败关闭；内置正负对抗样例验证规则。它禁止动态 HTML/eval，禁止除 `api.js` 外直接 `fetch`，并把 XHR 限定到上传 transport。独立的 TypeScript 5.9.3 `allowJs + checkJs + strict + noEmit` 门覆盖全部生产 JavaScript，外部/解析输入以 `unknown` 配合类型守卫收窄，并拒绝显式或隐式 `any`；该门无需迁移 `.ts`，但不等价于完整跨过程污点证明或 ESLint；
- Playwright 启动器动态分配端口，Chromium 与 Firefox 为必需矩阵，正式 Edge 为可选明确矩阵；Node 网关只呈现一个客户端地址，因此配置固定单 worker 串行执行，并保留失败重试 1 次和 `failOnFlakyTests: true`，所以重试通过仍会阻断门禁；每项测试使用随机 UUID 目录，不再共享可互相污染的固定数据目录；
- 主页面移除固定 538 px 最小宽度，并为不超过 537 CSS 像素的桌面缩放视口提供工具栏换行、文件列表两行网格回流和长文本折行/截断；名称与操作位于首行，修改时间和大小移到第二行并保持可见。Playwright 在 320 CSS 像素断言这些字段与核心操作可见、操作区在视口内且页面没有横向溢出，对应 1280 px 桌面 400% 缩放；另在 `forced-colors: active` 下验证关键控件、焦点和对话框仍有可见边界。手机 Web 仍不在产品支持范围；
- Rust HTTP 集成测试精确断言 CSP、frame、referrer、nosniff、Permissions-Policy 和 no-store；浏览器测试验证 Secure Cookie，并在每项测试后检查 page error 与 CSP violation。固定 `@axe-core/playwright 4.12.1` 还按 WCAG 2.0/2.1/2.2 A/AA 标签扫描登录页、文件页、打开的行内名称编辑器和操作对话框；这属于自动化缺陷检测，不是 WCAG 合规声明。两层测试共同覆盖响应策略和真实浏览器执行结果。

## 7. Argon2id 账号策略

完成内容：

- 账号只接受完整 Argon2id PHC，不接受明文、其他算法或旧格式兼容分支；
- 固定 PHC 版本 `v=19`、`m=19456`、`t=2`、`p=1`、16 字节 salt 和 32 字节输出；
- `hash-password` 生成与启动校验使用同一策略；
- 原始密码必须非空且最多 1024 个 UTF-8 字节；CLI、公开哈希入口、服务端表单解析和浏览器 `TextEncoder` 校验共用该边界。登录页不再用按 UTF-16 code unit 计数的 `maxlength` 冒充字节上限；唯一内联校验脚本由精确 CSP SHA-256 hash 授权；
- 配置端与浏览器登录端复用 128 字节用户名上限；重复、空或超长用户名，空哈希、异常参数和格式错误都会阻止启动，错误不回显账号或凭据；
- 最多两个 Argon2 校验 blocking 任务并发，permit 持续到计算真正结束；
- 登录在读取正文前同时经过全局 burst 16/每秒补充 1 个和客户端 IP burst 8/每秒补充 1 个的 token bucket；正文读取另受全局 32/每 IP 4 个并发许可、4 KiB 上限和 10 秒总时限约束。解析用户名后再按“客户端 IP + 用户名 SHA-256 摘要”组合键退避；同一组合连续凭据校验失败 5 次后指数退避 1–60 秒，记录 15 分钟过期，成功只清除对应组合状态，其他来源不会被一个攻击者定向锁号。`Retry-After` 对剩余时间向上取整，并只出现在 PRG 重定向后的最终 `429` 错误页；
- 未认证 GET/HEAD 只有在逐字段、逐逗号项解析 `Accept` 后发现精确 `text/html` 且可选 `q` 合法并大于 0 时才 `303` 到登录页；`text/htmlx`、`q=0`、重复或畸形 `q` 都保持接口式 `401`；
- 只有 TCP peer 为 loopback 时才接受单值、无逗号且可解析的 `X-Forwarded-For` 作为登录限流来源；其他情况使用真实 TCP peer，网关仍需独立限流；
- 会话容量为全局 1024、每账号 32；达到账号上限或全局已满时优先公平淘汰同账号最久未活动会话，避免一个账号挤掉全部其他用户；
- 写请求来源检查在存在 Origin 时同时比较 scheme 与 authority；外部 scheme 只接受唯一的 `X-Forwarded-Proto: http|https`，多值、逗号列表和非法值失败关闭，并继续与每会话 CSRF token 组合使用。

## 8. Rust 分层与存储可测试性

完成内容：

- 增加 `lib.rs`，服务实现可由进程入口和进程内测试共同使用；
- `main.rs` 只负责配置、监听、连接生命周期和 Linux 信号；
- `server.rs` 负责共享状态与模块协调；请求边界/路由、内置资源注册/摘要、DELETE 提交事务和回收调度分别位于 `router.rs`、`assets.rs`、`delete.rs`、`purge.rs`。进程级列表快照/游标缓存与有界递归遍历位于 `listing/{snapshot,walk}.rs`，fd-relative 删除执行器位于 `rooted_fs/purge.rs`，上传内部名称、头/选项、状态记录持久化与维护扫描位于 `upload/{internal_names,protocol,record,maintenance}.rs`；大段内联单元测试已移至各模块的 `tests.rs`，仍保持原隐私边界。登录限流和幂等 mutation 状态继续分别位于 `login_rate_limit.rs` 与 `operation_registry.rs`；此次拆分不改变 HTTP 或上传协议，也未新增第三方依赖；
- `RequestContext` 在 HTTP 边界统一保存 peer 和访问日志上下文，认证成功后才写入用户名；
- `AppError` 分离公开状态/消息与内部诊断来源，递归识别底层 I/O kind 并稳定映射 `400/403/404/409/504/507`；非预期错误返回不泄露路径和 errno 的 `500`，内部日志保留完整诊断；
- `StorageDurability` 把最终上传的文件同步和原子发布从 HTTP handler 注入；替换阶段以 `Published`、`Rejected`、`NotPublished` 和 `PublishedDurabilityUnknown` 分类，测试可确定性地区分 rename 前失败、rename 失败与发布后父目录同步失败；
- 列表、搜索和维护等长时间 blocking 工作通过 `TaskTracker` 登记；RootedFs 的短 metadata/open 调用由请求间接持有，取消后可能短暂收尾，但不会失去根 fd 和所有权边界；
- owner-scoped operation registry 在路径等待/业务校验前以 UUID 和请求指纹建立 `Reserved` 记录；已知 pre-commit 错误完成为 `failed`，pre-commit guard 异常丢弃会移除记录并允许安全重试。只有显式 `mark_commit_started` 后的异常丢弃才终结为 `unknown`；完成结果保留 15 分钟。容量全局 4096、每账号 1024，满载在 mutation 前拒绝；mkdir、move、rename、DELETE 的实际受跟踪 mutation task 另共用 64 个全局 admission permit；
- 删除不再为每个请求生成无限后台任务：schema v3 `purge_jobs` 以全局 4096、每账号 1024 的 durable outbox 保存 `Prepared/Ready/Claimed`，单 purge worker 和可从已记账 trash 根重建的进程内分片状态把清理并发、CPU 时间、fd 数与停机响应都变成显式边界。v2 是唯一支持事务迁移的旧 schema。轮转调度、ready-job bypass、无固定次数上限的持久指数退避、`Claimed` 重启恢复和持续 `Prepared` reconciliation 共同避免丢 job；永久故障会保留记录并占用配额，必须修复底层文件系统问题，而不是静默释放槽位。

## 9. 依赖、部署与检查入口

完成内容：

- Rust 版本固定为 1.97.1、edition 2024，构建脚本拒绝非 64 位 Linux；
- YAML 迁移到维护中的 `serde_yaml_ng`，删除 `smart-default`、`if-addrs`、`walkdir`、`urlencoding` 等不再使用的直接依赖；
- URL 组件统一使用 `percent-encoding`；
- 用户的唯一部署模型已确定为“浏览器 HTTPS → 网关 → Dufs 内网 HTTP/TCP”，因此删除内置 TLS 参数、feature、Rust TLS 依赖和专用集成测试；
- 生产 Hyper 只启用 HTTP/1 server feature，依赖图不含 `h2`；Playwright 仍用 Node HTTPS 网关代理到动态 HTTP/1.1 后端，验证 Secure Cookie 和真实网关路径；
- 仓库提供经生产 `Args` 解析器检查的 YAML，以及经过 `systemd-analyze verify` 和 `nginx -t` 加载的 systemd/nginx 示例；部署检查从包含空格、`&`、`#` 与反斜杠的真实 checkout fixture 读取文件，复制到安全运行名后启动隔离的真实 nginx 与 mock upstream，主动且分别验证规范 HTTP 重定向保留路径/查询、未知 SNI、合法 SNI 下未知 Host、固定 Host/转发协议、伪造入站 XFF 被 `$remote_addr` 覆盖、HTTP/1.1 回源、Connection 清理、全部登录别名 4 KiB 限制，以及连接/请求速率限制先出现 `429`、等待补充后恢复 `200`；
- `scripts/check.sh` 统一执行工具版本、五个 Bash 源的 `bash -n`、可用时的 ShellCheck warning 门、Shell/部署语法、发布 no-clobber、Git replace/private attributes 来源替换、SPDX notice 与 npm cache 播种自测、Rustfmt、`-D warnings` Clippy、Rust 全 targets/features 测试、固定行覆盖率基线、Cargo 审计、确定性 npm 安装、Acorn JS 门、全生产 JavaScript strict `checkJs`、Markdown 门禁、Chromium/Firefox、可选 Edge、npm 审计、diff 和 Git 清洁检查；本地缺少 ShellCheck 时明确跳过且不联网安装，远程 CI 固定并强制使用 0.11.0；
- `scripts/check-docs.mjs` 以保守源码解析检查全部 Markdown 的文本格式、inline/reference-style 本地链接和标题锚点，忽略围栏代码并拒绝检查树中的 symlink；它能对暂存发布树执行同一检查，但不宣称是完整 CommonMark parser。`scripts/check-js.mjs` 使用 Acorn AST、词法常量模型和内置正负对抗样例提供防御纵深基线，本身不宣称完整跨过程污点分析或 ESLint；类型边界由独立 TypeScript `checkJs` 门承担；
- `.github/workflows/read-only-ci.yml` 以 `contents: read`、无持久化 checkout 凭据和完整 Action commit SHA 建立静态、Node 最低版本兼容、Rust、质量、Chromium/Firefox 五个逻辑层；固定 Node 24.8.0、Rust 1.97.1、TypeScript 5.9.3、Playwright 1.61.1、`@axe-core/playwright` 4.12.1 和经 SHA-256 校验的 ShellCheck 0.11.0，并记录托管 runner 的实际镜像/工具版本。质量层的覆盖率、部署、发布脚本自测和 release binary smoke 以明确前置条件独立报告；该门不签名或发布，也不替代 exact tag 上的完整本地发布门；
- `Cargo.toml` 和仓库文件明确 MIT OR Apache-2.0 双许可证，`SECURITY.md` 定义当前版本和私密报告边界，`docs/operations.md` 给出部署、健康检查、备份恢复、升级与回滚流程；
- 构建把 Git SHA 写入 `dufs --version`；`scripts/package-release.sh` 只接受干净、由与 Cargo 版本一致的精确 tag 标记的提交。完整 `scripts/check.sh` 在已验证 commit archive 的无 Git 私有副本中以 `env -i`、固定工具链和独立 HOME/Cargo/npm/target/tmp 运行：Cargo vendor 后离线；npm 只从 lockfile HTTPS+SHA-512 验证宿主 cache 后播种私有 cache 并 prefer-offline，缺失包/审计仍可能联网；可用 RustSec DB 以无硬链接私有 clone 配合 `--no-fetch`。门禁后 snapshot index 复验 tracked 内容/mode 和非忽略新增路径，质量树随即丢弃，再 fresh extract 用于签名构建。检查后、签名前及发布前继续重验 exact source；预检、隔离快照和解包树都拒绝 symlink、submodule 与特殊文件，并继续拒绝 replace refs、legacy grafts 和私有 attributes。构建/打包两份独立源码 archive 会校验 commit、树、mode、额外路径并比较 SHA-256；
- 发布固定使用 `cargo-cyclonedx 0.5.9` 离线生成 SBOM；规范化器只接受完整 40 或 64 位小写十六进制 source revision，稳定化本地 Dufs 引用并拒绝构建路径泄漏，但不替代完整 CycloneDX schema 校验。`THIRD_PARTY_LICENSES.txt` 只从 vendored、可达的非开发依赖生成：每个包必须声明非空、经审核的 SPDX `license` 表达式，`license_file` 只收集上游正文，不能替代缺失表达式或作为分类 fallback；真实 SPDX AST 还验证 identifier/exception 与完整 permissive 分支。`license_file` 和包根 LICENSE/COPYING/NOTICE 必须是依赖自身/vendor 内 no-follow、非空 UTF-8 普通文件，项目许可证不作 fallback，正文按 SHA-256 去重。固定 Rust 1.97.1 sysroot 标准库 copyright 还须匹配审核摘要，并以 `RUST-STANDARD-LIBRARY-COPYRIGHT.html` 入包。`BUILD-ENVIRONMENT.txt` 记录源码 SHA/版本/epoch/target 与本次实际工具版本，只用于复现诊断而不表示宿主链已全部钉死；该清单、SBOM、项目许可证和两类 notice 均进入包内 checksum。Cargo/Node/SBOM/notice/文档、归档和 checksum 全部完成后才短暂打开签名密钥。key allowlist 为 Ed25519、Ed448、RSA ≥3072 bit 和 `prime256v1`/`secp384r1`/`secp521r1` ECDSA；弱 RSA、DSA、未审核曲线及非签名 key 失败关闭，关键 Shell 调用显式传播失败。最终 release 目录以一次 rename 原子 no-clobber 发布，正式签名仍须置于独立账号、主机或 HSM；
- 上述最终 release 目录的原子 no-clobber 发布要求底层文件系统支持 Linux `RENAME_NOREPLACE`；发布脚本还会用移动前后的设备号/inode 验证 destination 身份，静默碰撞不会被当作成功；
- `cargo tree --edges normal` 的生产依赖图不再包含 TLS、h2、WalkDir 或重复 URL 编码依赖；开发依赖 `assert_fs` 仍会间接使用 WalkDir，但不会进入生产可执行文件。

## 10. 自动化验证与可验证制品

持续交付验收要求：

- Rustfmt 和 `-D warnings` Clippy 通过；
- Rust 全量测试通过，包括根替换、分页、Range、上传限制、故障注入，以及 30 秒宽限 + 10 秒硬收尾边界；
- Chromium 与 Firefox 全量 Playwright 通过；未安装正式 Edge 时必须明确报告跳过；
- `cargo audit` 与 `npm audit --audit-level=high` 无未处理漏洞；
- `git diff --check` 与 staged diff 检查通过；
- 新增、修改、删除全部进入本地提交，不含构建产物、临时文件、生产凭据或生产私钥；仓库中的固定私钥仅供 localhost Playwright 测试，明确禁止部署；
- 在最终验收通过后创建与 Cargo 版本一致的本地 `v0.48.0` tag，并从该提交确认版本号和内置资源一致；
- 最终 `git status --short` 为空。

当前基线与历史数字解释：

- 原始清单曾记录的 `92/92`、每浏览器 `28/28`、250 个 crate 和 2.63 秒是十项优化完成时的历史快照；后续新增了分页快照、后来已移除的 ZIP、安全协议、operation registry、限流、维护、部署和浏览器隔离测试，固定数字已不再描述当前套件，因此不作为持续验收标准；
- 当前持续标准是 `scripts/check.sh` 中声明的全部命令按实际发现的测试数量通过；审查报告记录的一次 `0.48.0` 验收快照中，Rust 行覆盖率为 77.40%（13,165 行中 2,975 行未覆盖），后续代码不能沿用该固定数字作当前通过证明，门禁底线为 70%。Chromium 与 Firefox 均为必需矩阵，flaky 重试通过仍视为失败；串行覆盖多次 Argon2 登录、注销和 Cookie 重放的复合认证场景使用 Playwright slow 测试预算，但不放宽产品请求时限。Edge 未安装时明确跳过；依赖审计必须以运行时数据库和锁文件的即时结果为准；
- 100000 项手工基准仍默认忽略，但算法已从 `limit + 1` 候选改为“第一页完整有界快照、后续 O(page)”，必须重新记录目标文件系统上的首次扫描/排序/缓存成本，不能沿用历史 2.63 秒；
- 本地 `v0.47.0` tag 只标识历史版本；本轮整改使用 `0.48.0`，正式制品必须来自干净、已验收并由 `v0.48.0` 精确标记的提交，以 `dufs --version` 中的 Git SHA、归档 checksum、SBOM、第三方许可证 notice、经摘要审核的 Rust 标准库 notice 和签名共同确定来源；
- 日常开发中的脏工作树不等于已验收制品；只有最终检查和 `scripts/package-release.sh` 才要求所有预期修改已提交、工作树干净。仓库仍不自动向远程推送。
