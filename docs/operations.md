# 生产部署、备份、升级与回滚

本文给出当前 Linux 部署的基准操作流程。程序强制同一个共享根只能由一个 Dufs 实例持锁；这个 advisory lock 不阻止 shell、宿主机或其他服务写入。本文的一致性保证要求共享根由 Dufs 独占写入，人工修改只能在停服维护窗口进行。运维示例进一步采用一台主机一个 Dufs 进程的简化约定。示例假设：

- Dufs 二进制位于 `/opt/dufs/bin/dufs`；
- 配置位于 `/etc/dufs/dufs.yaml`；
- 必需的 SQLite 状态目录位于 `/var/lib/dufs`，固定数据库为其中的 `state.sqlite3`；
- 专用账号和组均为 `dufs`；
- 唯一共享根为 `/srv/dufs`；
- nginx 与 Dufs 位于同一主机，Dufs 只监听 `127.0.0.1:5000`。

网关样例要求 nginx 1.24.0 或更高版本、HTTP SSL/HTTP2 模块，以及仍由上游或操作系统发行商提供安全更新的 OpenSSL；新部署优先使用 OpenSSL 3.5 LTS，不能把已经结束公开安全支持的上游 OpenSSL 1.1.1 作为生产基线。源码质量门不只加载语法：部署检查会从包含空格、`&`、`#` 和反斜杠的真实 checkout fixture 读取文件，复制到安全运行名后启动隔离的真实 nginx 与 mock upstream，分别验证规范重定向、Host/SNI 拒绝、固定回源头与真实客户端 IP 覆盖、登录别名 4 KiB 限制，以及连接/请求速率限制的拒绝和恢复。脚本在创建第一个临时目录前安装清理 trap，并以首个目录创建后立即失败的内置自测验证部分初始化也会清除资源。systemd 校验会把 `ExecStart` 换为占位可执行文件；门禁不会真实启动 systemd unit 与 Dufs/nginx 组合，因此生产数据副本上的启动、readiness 和 CRUD 冒烟不能省略。

自动 CI、部署样例和发布验收的架构基线是 `x86_64-unknown-linux-gnu`。`build.rs` 允许其他 64 位 Linux 源码构建，但 aarch64 等目标在加入等价工具链、浏览器和部署矩阵前只属于未验证的 best effort；制品必须匹配 CPU、libc、动态加载器和 `openat2` 内核能力。

如果实际路径不同，必须同步修改配置、systemd 的 `ReadWritePaths`、备份任务和恢复演练，不能只替换其中一处。

## 1. 首次部署

1. 创建不可登录的专用服务账号，并确保共享根、其现有内容以及需要保留的 ACL 和扩展属性可由该账号管理。
2. 在已经校验并解压的发布包根目录中，创建目标目录并安装二进制、文档、配置和网关文件：

   ```sh
   install -d -o root -g root -m 0755 /opt/dufs/bin
   install -d -o root -g dufs -m 0750 /etc/dufs
   install -d -o root -g root -m 0755 /etc/nginx/conf.d /etc/nginx/snippets
   install -d -o root -g root -m 0755 /usr/share/doc/dufs
   install -o root -g root -m 0755 dufs /opt/dufs/bin/dufs
   install -o root -g root -m 0644 docs/operations.md /usr/share/doc/dufs/operations.md
   install -o root -g root -m 0644 LICENSE-APACHE \
     BUILD-ENVIRONMENT.txt \
     THIRD_PARTY_LICENSES.txt RUST-STANDARD-LIBRARY-COPYRIGHT.html \
     dufs.cdx.json /usr/share/doc/dufs/
   install -o root -g dufs -m 0640 config/dufs.yaml.example /etc/dufs/dufs.yaml
   install -o root -g root -m 0644 deploy/dufs.service /etc/systemd/system/dufs.service
   install -o root -g root -m 0644 deploy/nginx-dufs.conf /etc/nginx/conf.d/dufs.conf
   install -o root -g root -m 0644 deploy/dufs-proxy.conf /etc/nginx/snippets/dufs-proxy.conf
   ```

3. 使用 `/opt/dufs/bin/dufs hash-password` 生成 Argon2id PHC，替换 YAML 中的占位值。配置含登录验证材料，只允许 root 或服务进程的有效用户拥有，并精确使用 `0400`、`0440`、`0600` 或 `0640`；`0440/0640` 还要求文件 gid 等于服务进程的有效 gid，因此样例的 `root:dufs 0640` 是合法基线。配置必须是无扩展 POSIX access ACL 的单硬链接普通文件，Dufs 会在同一打开 fd 上围绕 ACL 探测和正文读取复核身份、安全属性、大小及 mtime/ctime 稳定。配置和可选日志路径都必须位于 `/srv/dufs` 之外；规范父目录、最终目标和目录实体检查会识别父目录符号链接等可解析别名，单硬链接约束会拒绝硬链接别名。配置与日志不能共享同一规范目录项或已存在 dev/inode，也不能以目录项或对象别名碰撞 `state.sqlite3` 及其 `-journal/-wal/-shm` 热 sidecar。部署样例必须提供 `state-dir: /var/lib/dufs`；对应 systemd unit 用 `StateDirectory=dufs` 和 `StateDirectoryMode=0700` 在启动命令前准备目录。若不使用该 unit，必须先创建由服务账号所有、权限为 `0700` 且不是符号链接的专用目录。状态目录不能与 `/srv/dufs` 互为祖先/后代，也不能用 bind mount 或其他挂载别名让固定的 `state.sqlite3` 从共享根内可见；启动检查普通路径关系与可辨识目录实体，但不会解析管理员在运行时建立的所有挂载别名。
4. 安装真实 TLS 证书，替换 nginx 示例域名，然后检查配置：

   ```sh
   systemd-analyze verify /etc/systemd/system/dufs.service
   nginx -t
   ```

5. 确认后端端口没有对外监听，再启动服务：

   ```sh
   systemctl daemon-reload
   systemctl enable --now dufs
   systemctl enable --now nginx
   systemctl reload-or-restart nginx
   ss -ltnp
   curl --fail --max-time 10 http://127.0.0.1:5000/__dufs__/health
   ```

6. 从受支持浏览器经外部 HTTPS 地址登录，完成新建、上传、单文件下载、移动和删除冒烟测试。

Dufs 启动时会对共享根目录取得非阻塞独占锁。第二个管理同一根目录的实例会拒绝启动；这不是高可用选主机制，不能把多个节点指向同一共享存储。

### 1.1 必需的文件型统一状态库

服务只使用文件型 SQLite schema revision 1 的统一 state store，其中有 `operations`、`upload_sessions` 和 `purge_jobs` 三类状态。SQLite 是上传状态的唯一权威，服务不会在共享根内写入、读取或导入 JSON 上传状态文件。CLI `--state-dir` 或 YAML `state-dir` 必须提供一个目录，数据库固定使用 `<state-dir>/state.sqlite3`；没有进程内数据库或单独文件路径配置入口。

operation 容量为全局 4096、每账号 1024，终态 TTL 为 15 分钟。启动恢复会删除未进入提交边界的 `Reserved`，把可能已经触碰文件系统的 operation `CommitStarted` 转为 `Completed/unknown` 并从恢复时开始新的 15 分钟终态 TTL；未过期的原有 `Completed` 只继续使用剩余 TTL。upload session 容量为全局 16384、每账号 4096，每次实际更新后保留 7 天；持久状态包含 `Running/CommitStarted/AwaitingConfirmation/Committed/Rejected/Unknown`，重启会把 upload `CommitStarted` 恢复为 `Unknown`，而 `AwaitingConfirmation` 保留完整 stage 等待明确发布或丢弃。首次 discard 原位写入 `Rejected` 并设置终态 TTL；对已有 `Rejected` 的幂等重试不写库、不续 TTL，只继续 identity-safe stage 清理。purge job 容量为全局 4096、每账号 1024，不使用 TTL 或固定失败次数丢弃普通 I/O 故障中的回收任务。

认证客户端应通过 `GET /__dufs__/api/jobs/<UUID>` 查询当前账号的 mutation job。响应使用 `job_id` 字段及 `running/succeeded/failed/unknown` 状态。

状态库固定使用 SQLite rollback journal `DELETE` 模式和 `synchronous=EXTRA`，由单独状态线程串行访问。数据库文件以 `0600` 使用，必须位于共享根之外；已有数据库还必须是非符号链接、单硬链接普通文件，并绑定创建时共享根的设备号和 inode。任何 SQLite 连接打开前，固定 `-journal/-wal/-shm` 都要经 `lstat`、`O_NOFOLLOW|O_NONBLOCK` 打开、`fstat` 和打开前后身份复核，拒绝符号链接、特殊文件、多硬链接、出现/消失或替换；主库不存在时不接受任何孤立 sidecar。现存主库从 no-follow fd 复制到进程私有临时目录形成不叠加 sidecar 的 raw baseline，先验证精确的五列 `product_metadata`、`dufs-ram` 应用名、当前 Cargo 版本、schema revision 1、统一 SHA-256 指纹、根绑定和完整性，再由原路径连接验证合并视图。指纹对排除 `sqlite_*` 与 `product_metadata` 后按 `type/name/tbl_name/sql` 排序的原始字段逐个编码 u64 大端长度和字段字节。只有空库会创建当前 schema；任何旧版本、无标记库、版本/指纹/对象漂移、其他应用数据库、错误共享根或非 SQLite 文件都会在 chmod、journal mode 和恢复写入前拒绝，主库及全部 sidecar 保持原字节、mode 和身份。运行服务不提供 schema migration；应先停服并使用独立升级流程，不要绕过失败或把同一文件复制给另一共享根复用。

SQLite 提交与共享根中的 mkdir、rename、文件同步和目录 `fsync` 不属于一个共同事务。operation/upload 崩溃恢复中的 `unknown` 是保守结果，不是回滚记录。DELETE 先持久化含根内相对目标/trash 路径和源 dev/inode/类型的 `Prepared` outbox，再做 checked rename 与父目录 `fsync`；成功后才把覆盖 dev/inode、类型、链接数、大小、uid/gid、完整 mode 和纳秒级 mtime/ctime 的 32 字节 trash revision 与 `Ready` 原子写入。worker 把到期 job 原子 claim 为 `Claimed`，并用 revision 与持续 fd 锚点共同复核；普通 I/O 失败持久化回 `Ready` 并从 100 ms 指数退避到最长 30 秒。若 state-store 的 defer/complete 命令瞬时失败，worker 会有界保留本地 claim，并在回读确认数据库仍为 `Claimed` 后重试；重启也会把遗留 `Claimed` 恢复为 `Ready`。`Prepared` 没有已提交 revision，reconciler 始终保留目标，把 trash 路径上的任何 occupant 移入 `.dufs-quarantine-<uuid>.hold` 后释放 intent，绝不再依据弱源 inode 推断 rename 结果。`Ready/Claimed` 缺失 revision、身份不匹配或最终删除出现 `InvalidData` 时同样 quarantine 整棵 trash 根并释放 job。每个最终 unlink/rmdir 候选先移入随机隔离名，再用既有 fd 复核；`ENOTEMPTY/EXIST` 等异常不从 cursor 0 重扫。DFS 最多保留 2048 层目录 frame，每次 push 都用 `try_reserve`；超深树返回 `InvalidData` 并把剩余 trash 根永久隔离，内存预留失败则保留游标供以后重试。未记账 orphan 在兜底通道满、取消或普通 I/O 失败时保持隐藏，等待以后 maintenance 重新发现；若 purge 判定为 `InvalidData`，整棵根立即进入永久 quarantine。quarantine 永不由 maintenance 自动清理；发现后应停止 Dufs，核对内容、owner、来源日志和状态库再手工移除。递归清理不会进入 trash 下的嵌套/bind mount，普通 mount 边界 I/O 故障保留 job 并退避，卸载后继续。能用 inotify 竞争随机隔离名的恶意同 UID writer 仍不在支持边界内。

进程若在嵌套候选已改成随机隔离名、尚未 unlink 时中断，下一次 orphan 扫描可以重新捕获外层 trash，但 purge 一旦看见树内遗留的隔离名就按身份安全异常停止，并 quarantine 整棵外层根。不要手工把该嵌套名称改回普通文件名后继续运行；应按整根 quarantine 的调查流程处理。

不要在活跃 upload 或未完成 purge 的路径祖先上依赖 rename/unlink 来“顺手迁移”控制状态：SQLite 与文件系统无法在一个事务内原子 rebase。服务会在语义路径租约内，对 move/rename 的源与派生目标、DELETE 目标和 fresh PUT 目标执行有界 keyset 状态检查；根内符号链接别名也按目录身份识别。命中时分别返回 `409 move_state_conflict`、`409 rename_state_conflict`、`409 delete_state_conflict` 或 `409 upload_state_conflict`，待原任务完成后用新的 operation/upload ID 重试。检查本身暂不可用时不会开始 mutation，并返回带恢复建议的 `503`；fresh PUT 的该检查受 upload deadline 约束，超时返回绑定的 `408 not-started`。它发生在 tracked route metadata 之后、注册上传 mutation 和创建 stage/SQLite 行之前。

进入受跟踪的上传任务不再等同于“结果已经可能未知”。任务可以在持有路径租约和上传槽时只读查询 owner 会话、目标 identity/metadata 与空间状态；创建祖先或 stage、截断既有 stage、更新 SQLite 会话或接收正文等首次 mutation 必须先通过一个与总 deadline 原子竞争的边界。deadline 先赢会关闭边界并 abort 任务，返回绑定的 `408 request_timeout + not-started + retry`；只读准备中逸出的超时类错误同样返回 `408`，其他未处理 I/O 返回 `503 upload_precommit_failed + not-started + retry`。边界关闭后任务不能稍后恢复并写入。若 mutation 先赢，随后外层 deadline 或未处理错误才是 `unknown + query_upload`。运维自动化不要只按 HTTP `408/503` 重放；仍应遵守响应中的 upload state/recovery，并以原 ID 做 owner-scoped HEAD，因为 `not-started` 不排除更早请求留下的检查点或终态。

### 1.2 上传预检与条件覆盖

首方浏览器在批次入队前向 `POST /__dufs__/api/upload/preflight` 提交最终绝对逻辑路径。一次请求必须包含 1～512 个互不重复的路径，解码后的路径 UTF-8 总量最多 256 KiB，JSON wire body 最多 2 MiB；响应按原顺序返回 `path`、`exists`、`replaceable` 和可选 `revision`。没有已存在目标时不会弹出确认；已存在且可替换的文件才进入覆盖/跳过/取消对话框，不能替换的目标不会自动覆盖。预检是有界观察，不是锁定文件系统的事务；提交时仍须执行 no-replace 或 identity 条件检查。

所有 PUT/PATCH 都使用明确的覆盖策略。缺少 `X-Dufs-Upload-Overwrite` 或值为 `false` 表示原子 no-replace；目标在提交时存在就失败，不会静默覆盖，rename 成功后还会核对目的名称与已打开 stage 的 identity，无法证明时返回 unknown。值为 `true` 时必须同时提供 64 位小写十六进制 `X-Dufs-Target-Revision`。revision 绑定账号摘要、规范根内目标路径和完整 replacement identity，服务在真正 rename 前再次验证它；但已有目标覆盖随后使用普通 rename，不是能排除共享根外部 writer 的原子目录项 CAS。客户端不得把网络错误、`unknown`、非法响应或无法解析的 revision 降级为无条件覆盖，生产运行期间也不得由其他进程并发写共享根。

若完整 stage 在最后的条件检查中遇到目标出现或变化，服务保留同一 upload ID 和满 offset，并持久化为 `AwaitingConfirmation`；HEAD/冲突响应对外使用 `awaiting-confirmation`，同时给出当前 revision 和可替换提示。用户接受最新目标后，浏览器以同一 ID、满 offset、空正文 PATCH 和最新 revision 再次提交，因此通常无需重传文件；目标若再次变化会继续失败关闭并重新确认。每一个可信 target-change 都重新发出 `refresh-required`，不会因为该 uploader 先前已使列表失效就吞掉通知；两次冲突间点 Refresh 得到的新 snapshot 也会再次失效。用户跳过时，浏览器向 `POST /__dufs__/api/upload/discard` 提交同一路径和 upload ID。服务先以 owner/path/ID 绑定的原位 CAS 把 `AwaitingConfirmation` 持久化为 `Rejected`，再按已记录 stage identity 条件清理；已有 `Rejected` 的重试不续 TTL，但会继续清理。成功 `204` 表示终态已确定且本次安全清理步骤完成，可能是原 inode 已删除、已经不存在，或发现同名替换物并保留；仅由 HEAD 得到 `rejected` 只证明上传未发布和 discard 决定已持久化，不证明路径物理消失。

每个目标父目录下的 stage 都放在精确名为 `.dufs-upload-stages` 的当前私有目录中。该目录必须是服务账号所有、真实目录、与目标父目录同一设备且精确为 `0700`；stage 初建为 `0600`。覆盖提交重放目标 mode/ACL/xattr 后，stage 本身可能不再是 `0600`，但父目录仍阻止其他本机账号遍历和读取未发布内容。启动在监听前以 16 行 keyset 页验证所有数据库记录只引用这一当前布局，并核对目录权限、owner、设备和活跃 stage inode；任何其他持久 stage 路径都会失败关闭且不会移动文件或改写数据库。

有一个必须保留的 metadata 安全例外：已暂存的覆盖上传可能已经重放旧目标 uid/gid、mode 或允许的 xattr。若旧目标随后消失，服务以 `upload_metadata_preservation_refused` 拒绝用空 PATCH 把该 stage 当作全新文件发布；浏览器必须先 discard，再生成新 ID，以完整正文和 create-only PUT 重传。这样避免把旧对象的 metadata 意外赋给一个语义上新建的文件。

仓库 systemd 样例的 `TimeoutStopSec=120s` 大于应用内置停机边界：首次信号后普通工作和提交共用 30 秒宽限，随后仅有 10 秒强制收尾窗；约 40 秒仍卡住时 Dufs 不再刷新日志，立即以状态 1 退出，不能保证该提交已落盘或尾部日志已写出。正常完成 tracked cleanup 后只执行一次、最多 5 秒的日志 flush，再显式 `exit(0)`；flush 由专用命名 OS thread 执行，不依赖可能被故障文件系统工作占满的 Tokio blocking pool，也不会让 runtime drop 继续等待卡在内核/FUSE 的已取消任务。主任务在 flush 期间继续优先监听第二信号，收到后跳过等待并立即以 130/143 退出。调大 systemd 超时不会延长这个应用硬截止；应监控并演练最慢文件/目录同步，使常见提交能在窗口内完成。SIGKILL 也会越过全部保证。

## 2. 网关边界

仓库内 nginx 示例固定 HTTP/1.1 回源，传递单值 `Host`、`X-Forwarded-For` 和 `X-Forwarded-Proto`，关闭请求重放与缓存，并对登录路由族同时使用来源 IP 请求速率、连接数和短正文时限。未知 HTTP Host 由默认 server 拒绝，合法 HTTP server 只跳转到配置中的固定规范 HTTPS 域名；未知 HTTPS SNI/Host 在默认 server 拒绝，不能借 `$host` 形成外部跳转。Dufs 的内部路由本身也只接受规范 URI，尾斜杠、重复斜杠和非规范百分号编码不会成为绕过 exact gateway location 的等价别名。

Dufs 在读取登录正文前同时消耗全局 burst 16/每秒补充 1 个和来源 IP burst 8/每秒补充 1 个的 token bucket；正在读取的 4 KiB 正文另受全局 32、每 IP 4 个并发许可和 10 秒总 deadline 约束。解析账号后仍继续执行“来源 IP + 账号摘要”组合键失败退避和最多两个 Argon2id 计算槽；一个来源不能借错误密码把同一账号在其他来源全局锁定。`Retry-After` 会向上取整剩余秒数，并只由 PRG 重定向后的最终 `429` 登录错误页返回；POST 的 `303` 不携带该字段。应用只在直连 TCP peer 匹配显式 `--trusted-proxy` / `trusted-proxies` IP 或 CIDR 时，才采用合法单值 `X-Forwarded-For` 作为登录限流地址并用单值 `X-Forwarded-Proto` 证明外部 scheme；默认列表为空。网关若位于另一台主机，必须同时配置其窄来源网段并保留网关侧真实来源 IP 限流。

Dufs 的普通文件和 Range 正文没有总时长/最低速率限制，但每个源文件分块的门控等待及读取连续 30 秒未完成会使正文报错，已经取得的分块在套接字连续 30 秒没有写入进展也会关闭连接。两项 idle deadline 独立重置；公网网关仍应设置符合业务容量的响应总时长、最低速率和空闲策略，不能把它们当作完整的慢客户端或总时长防护。

后端必须由主机防火墙或网络 ACL 限制为仅网关可达。受信代理列表不是身份验证；`127.0.0.1/32` 仍允许任何能连接该回环端口的本机进程声明代理头，所以不可信本机进程必须再由容器/网络命名空间、进程级防火墙或等效机制隔离。TLS 私钥、会话 Cookie、CSRF token、完整 Argon2id PHC 和文件内容均不得进入诊断工单或公开日志。

## 3. 健康检查和监控

- `GET` 或 `HEAD /__dufs__/health` 是公开 liveness，只表明进程仍能处理 HTTP，不访问文件内容，也不泄露账号或路径。
- `GET` 或 `HEAD /__dufs__/ready` 是受认证 readiness；它通过启动时锚定的共享根 fd 创建一个保留形状的隐藏文件、写入并同步文件、删除该目录项，再同步根目录 fd，从而真实覆盖创建/写入/文件 `fsync`/删除/目录 `fsync` 路径。同时，统一 state-store actor 在当前 SQLite 连接上读取数据库身份和元数据，执行 `BEGIN IMMEDIATE`、写入探针行并显式 `ROLLBACK`，因此启动后变为只读、不可写或不可访问的数据库不会被缓存的 healthy 标志掩盖。readiness 还要求当前文件系统在计入进程内空间预留后满足 `min-free-space`，且进程未进入任一停机阶段；任一探针失败返回 `503`。purge 容量在 DELETE 预备阶段单独执行，readiness 不是对每种业务配额的完整接受性预测。外部负载均衡器若无法安全维护会话 Cookie，应只用 liveness，并通过独立的登录冒烟任务验证 readiness。
- 告警至少覆盖进程重启、HTTP 5xx/429/507、登录限流、磁盘空间、inode、共享根挂载状态、备份年龄和备份恢复演练结果。

普通写请求返回成功只表示其规定的原子发布和目录同步步骤已返回成功。硬件、固件、网络文件系统或宿主机错误兑现同步请求仍可能破坏数据，因此监控不能替代备份。

## 4. 备份

备份范围至少包括：

- 共享根中的全部普通文件、目录、符号链接、所有权、模式、ACL、扩展属性和硬链接关系；
- `/etc/dufs/dufs.yaml`；
- 当前二进制、发布包的 `SHA256SUMS`、`BUILD-ENVIRONMENT.txt`、CycloneDX SBOM、`THIRD_PARTY_LICENSES.txt`、`RUST-STANDARD-LIBRARY-COPYRIGHT.html`、外层 checksum、签名及独立取得的公钥；
- systemd、nginx、防火墙和备份任务配置。

会话 Cookie 状态只存在内存中，不需要备份。每个目标父目录中的 `.dufs-upload-stages` 保存当前未完成上传的 stage，`.dufs-upload-delete-<uuid>.trash` 保存等待状态机确认或清理的删除对象；两者都受当前内部命名空间保护。`.dufs-quarantine-<uuid>.hold` 是永久保留的人工调查对象，永不自动清理。备份不得过滤任何当前内部项，否则 stage/outbox/quarantine 恢复时间点会不自洽。

状态目录包含短 TTL operation 重放、7 天 upload session 和无 TTL 的未完成 purge outbox，但仍不是共享文件数据的替代备份。SQLite 与共享根没有跨域事务，因此最佳备份是在 Dufs 停止后同时复制共享根和状态目录，或对两者取同一受控时间点的存储快照；rollback journal 模式下不支持在事务进行时只复制主 `.sqlite3` 文件。恢复到新的共享根通常会得到不同的设备号或 inode，此时应使用新的空状态目录，并明确接受旧 Operation ID 不可重放、持久上传恢复记录丢失、隐藏 trash 只能由 orphan 扫描兜底；不能把绑定旧根的数据库强行接到新根。

首选底层文件系统或存储提供的原子快照：

1. 监控 `/__dufs__/health` 并确认共享根挂载正常。
2. 创建单一时间点快照。
3. 从快照复制数据，保留 numeric uid/gid、模式、ACL、xattr、稀疏文件和硬链接。
4. 对备份清单和内容做校验，记录源主机、Git SHA、时间、快照 ID 和工具版本。
5. 按保留策略把至少一份副本放到不同故障域，并启用不可变或离线保护。

如果底层没有一致性快照，应先进入维护窗口：

```sh
systemctl stop dufs
# 确认进程已退出，再执行能够保留 ACL/xattr/hardlink 的文件级备份。
systemctl start dufs
```

不要用未经验证的普通 `cp -r` 作为唯一备份方式。备份成功的判据是能够恢复，而不是命令退出码为零。

## 5. 恢复演练

至少按季度在隔离主机或隔离挂载点做一次完整恢复：

1. 校验备份清单、发布包 checksum 和签名。
2. 恢复到新的空目录，不覆盖生产根。
3. 比较文件数量、总字节数、内容摘要抽样、numeric uid/gid、模式、ACL、xattr、符号链接目标和硬链接 inode 关系。
4. 用与生产相同的服务账号启动 Dufs，并为这个新根配置新的空状态目录；根目录锁、登录、分页、上传覆盖、单文件下载、移动和删除均应通过。
5. 记录恢复点目标（RPO）、恢复耗时（RTO）、缺失项和修正措施。

发生真实恢复时，先保全故障卷和日志。不要在原因未知时直接把备份覆盖回原目录。

## 6. 升级

仓库的 `.github/workflows/read-only-ci.yml` 只提供远程回归反馈：权限为 `contents: read`，checkout 不保留凭据，静态、Rust、质量和 Chromium/Firefox 层不会创建 tag/release 或签名，也不会上传制品。质量层分别运行覆盖率、部署行为、发布脚本自测和 release binary smoke；各步骤只在自己的前置条件成功时运行，一项实质检查失败不会跳过其余独立检查。唯一当前 Node 24.8.0、Rust 1.97.1、ShellCheck 0.11.0、锁定的 npm 工具和 Action commit SHA 在工作流中固定；`ubuntu-24.04` 托管镜像的实际版本及宿主工具写入日志。合并前应查看全部矩阵结果，但它不包含正式签名边界，也不替代目标 exact tag 上的完整本地门和下述发布流程。

仓库另有 `.github/workflows/release-binary.yml`，只在推送 `v<version>` tag 后运行。它复核 tag、Cargo 版本和 workflow commit 一致，等待同一 tag/SHA 的全部质量门成功，并生成绑定当前版本与完整源码 SHA 的确定性发布说明。唯一的 `contents: write` job 不 checkout、不调用仓库脚本或执行下载的二进制，只消费并复核不可变发布输入。

`.github/workflows/formal-release-e2e.yml` 在版本 tag、每周计划或人工触发时使用临时 Ed25519 密钥调用未缩短的 `scripts/package-release.sh` 正式入口。它在含空格和 shell 元字符的隔离 clone 中建立精确本地 tag，实际经过完整质量门、vendor、release build、SBOM、checksum、签名和原子目录发布，再从外部复核四项制品、签名、公钥、包内 `SHA256SUMS` 以及二进制完整版本/SHA。该 job 只有 `contents: read`，不引用仓库或环境中的生产/自定义 secrets，只使用只读 GitHub token checkout，也不上传输出；临时密钥和制品随 runner 销毁，因此它验证路径正确性而不产生可分发的正式信任根。

自动 GitHub Release 是面向直接下载运行的便捷通道：其二进制在 `ubuntu-24.04` 托管 runner 上构建，必须匹配目标 CPU、glibc、动态加载器与 `openat2` 内核能力。需要 CycloneDX SBOM、第三方和标准库许可证清单、构建环境记录、可重复归档及独立公钥签名时，仍必须执行下述本地正式发布流程；不能把同一 Release 中的 checksum 当作独立信任根。

发布包由 `scripts/package-release.sh` 从干净 Git 提交构建。`Cargo.toml` 的版本必须存在精确的 `v<version>` tag，且该 tag 必须指向当前 HEAD。脚本强制执行完整 `scripts/check.sh`，不是依赖调用者事先声称检查通过；门禁后、签名前和发布前都会再次核对 HEAD、tag、版本与干净状态。启动检查显式拒绝 `refs/replace/*`、legacy grafts 和仓库私有 `info/attributes`。

进入构建前，Git 索引和目标 commit tree 中的条目必须都是 mode `100644/100755` 的普通 blob；tracked symlink（`120000`）、submodule/gitlink（`160000`）及其他类型一律拒绝。权限为 `0700` 的私有 bare façade 只含由脚本摘要锁定的最小 local config，并通过 object alternates 读取目标对象库；所有决定源码身份的 Git 命令都清空 HOME、system/global 配置并禁用额外 attributes/replace。

脚本从 façade 解析一次完整 commit ID。它先生成并验证一份质量门 archive，在没有 `.git` 的 `0700` 私有副本中运行检查；门禁结束后用独立 snapshot index 比较 tracked 内容和 mode，并拒绝任何非忽略新增路径。随后整棵质量树及其缓存被删除，再从同一 commit 分别生成全新的签名构建归档和打包归档。每份 tar 都作为独立文件保存到私有 stage 并立即验证，再解包并用目标 commit tree 建立独立临时 index；解包树会以 no-follow 方式拒绝 symlink 及任何非普通文件/目录条目，缺失、额外、类型、mode 或内容不同都会失败。后两份 tar 的 SHA-256 还必须完全相同。因此本地 replace object、private attributes、质量工具或构建期间改变 worktree/Git 元数据，不能让同一声明 SHA 对应另一棵检查、构建或打包树。只有最后一份重新验证的树提供文档和部署材料。同 UID 恶意进程仍属于必须用身份/主机隔离解决的边界。

隔离质量门以 `env -i` 启动，固定 PATH、Rust 工具链和完整源码 SHA，并使用私有 HOME、Cargo home/target、npm cache、XDG 目录与临时目录。Cargo 先从锁文件 vendor，再以 offline source replacement 运行；这与之后签名构建使用的独立 vendor 树相互隔离。npm cache 播种器只接受 `package-lock.json` 中带 HTTPS resolved URL 与 SHA-512 integrity 的条目，并重新散列宿主 cache 内容后写入私有 cache；`npm ci` 使用 `prefer-offline`，缺失包以及 `npm audit` 仍可能访问网络。宿主 RustSec Git 数据库只有在 canonical origin、`HEAD=FETCH_HEAD`、实体 `FETCH_HEAD` 时间戳不得比当前时间早超过 7 天或晚超过 300 秒，并通过完整物理/Git/内容检查后才可复用；alternates、不安全元数据、symlink/submodule/特殊项、untracked 路径和 tracked 内容/mode 漂移均拒绝。合格输入以无硬链接私有 clone 封存 revision、fetch epoch、index/config 校验和；不合格、过期或缺失时，在任何项目或依赖代码前用 dummy lockfile 在私有数据库联网刷新，离线失败关闭。发布入口先执行 `cargo audit --db ... --no-fetch --no-yanked` sealed pre-audit；随后用私有 Cargo home 执行 `cargo fetch --locked`，保证 yanked 检查拥有完整锁图所需的 crates.io 索引项，再以同一封存数据库运行 `cargo audit --no-fetch --deny yanked`。索引缺失、抓取失败或锁图含已撤回 crate 都失败关闭；该 Cargo home 每次全新创建，因此当前正式发布要求 registry 网络可达，宿主 Cargo 缓存不能替代这一步。之后通过必填 `DUFS_QUALITY_AUDIT_DB` 把同一数据库交给隔离 `scripts/check.sh`，该脚本也在其他项目/依赖步骤前先审计。封存时校验 seal 与新鲜度，pre-audit 和 yanked 检查后重验 seal；完整门禁后重验 seal 与新鲜度，随后销毁质量树和该 RustSec 数据库。包内环境清单只记录 advisory revision/fetch epoch，不记录内部 seal 摘要。Playwright 只复用显式浏览器 cache，不让测试依赖用户 npm/Cargo 配置。JavaScript 安全门固定使用 Acorn 8.17.0 AST 与有界词法常量分析，并以内置正负对抗样例校验关键规则；动态 computed 解构的属性名无法静态求值时，在变量声明、赋值表达式和默认参数（含嵌套及 const alias）中都失败关闭。TypeScript 5.9.3 另以 `allowJs + checkJs + strict + noEmit` 检查全部生产 JavaScript，外部/解析输入保持为 `unknown` 并经守卫收窄，生产源码不保留显式或隐式 `any`。该门无需迁移 `.ts`，但仍不等价于 ESLint 或完整跨过程污点证明。本地有 ShellCheck 时统一门执行 warning 检查，缺失时明确跳过且不联网安装；远程 CI 固定并强制执行 0.11.0。

脚本严格校验 Rust/rustc/Cargo 1.97.1、`cargo-cyclonedx 0.5.9` 与 `cargo-audit 0.22.2`。固定工具链 sysroot 的 `share/doc/rust/COPYRIGHT-library.html` 必须是 sysroot 内 no-follow 普通文件，并精确匹配已审核 SHA-256 `0a65bb747c49c7bb816cbc7188319bd6e4e8d08091c1190b8a3c0971c47968ed`；未知工具链没有审核摘要时直接拒绝。验证后的副本以 `RUST-STANDARD-LIBRARY-COPYRIGHT.html` 打包。签名构建另用锁文件 vendor 依赖，随后以清空环境、私有 Cargo home、离线 source replacement、关闭增量编译和显式编译器运行 release 构建；完整 Git SHA 嵌入版本字符串，私有构建路径经过 remap 并在二进制中复查。`SOURCE_DATE_EPOCH` 同时传给 Rust 构建、SBOM 和归档；未显式设置时使用提交时间。

SBOM 递归把本地 Dufs `bom-ref`/`purl` 规范化为绑定完整源码 SHA 的稳定 Cargo 标识；source revision 只接受恰为 40 或 64 位的小写十六进制对象 ID，并拒绝明文或百分号解码后出现的本地 `file:`、POSIX/Windows 绝对路径与构建根。它要求元数据中恰有一个本地 Dufs root 和一个依赖 root；这是项目所需的结构/无路径泄漏检查，不替代完整 CycloneDX schema validator。

`THIRD_PARTY_LICENSES.txt` 从 Cargo metadata 中 Dufs 可达的非开发依赖生成，依赖源码必须位于本轮 vendor 根。每个包必须声明非空、经审核的 SPDX `license` 表达式；metadata `license_file` 只用于收集上游正文，不能替代表达式或作为分类 fallback。生成器按 `WITH > AND > OR` 优先级解析真实 SPDX AST，只接受审核清单内的 license identifier/exception，并要求表达式存在一条完整 permissive 选择：`OR` 任一分支可行，`AND` 两侧都必须 permissive；只对明确列出的 Cargo 遗留 `MIT/Apache-2.0` 和 `Unlicense/MIT` 写法映射为 `OR`。例如 `LGPL AND (MIT OR Apache-2.0)` 会拒绝，而 `(LGPL AND Apache-2.0) OR MIT` 可选择完整 MIT 分支。

生成器同时收集 metadata `license_file` 与包根下所有匹配 LICENSE/COPYING/NOTICE 的常规文件；每个候选都必须是对应依赖真实源码目录内、同时仍在 vendor real root 内的 no-follow 普通文件。项目自身 `LICENSE-APACHE` 不能替代缺失的上游文本。

包内 `BUILD-ENVIRONMENT.txt`、SBOM、第三方 notice、Rust 标准库 notice 和项目 Apache-2.0 许可证均纳入 `SHA256SUMS`。

二进制包同时按仓库层次保留完整 `docs/`，并携带教程本地链接引用的 `clients/web/`、`src/`、`tests/`、`scripts/`、部署样例和构建配置，使文档在离线解压后仍可导航到对应实现。发布脚本先用包内 `scripts/check-docs.mjs` 检查最终布局，再对除清单自身外的全部普通文件生成 `SHA256SUMS`，此后只读复核清单覆盖；`--self-test` 还放入深层 sentinel、验证篡改失败、两次归档一致并解包往返检查，避免源码树检查通过但最终制品或 checksum 失效。

输出目录必须由当前发布账号拥有且不能让 group/other 写入；它会被解析为物理路径并通过已验证 fd 持有独占 `flock`。stage 创建、构建、清理、最终 rename 和目录同步均从锁定的目录 fd 路径派生，公开字符串路径在此后只用于身份复核和结果展示，祖先目录换绑不能重定向 mutation。发布后还会核对公开路径、锁定目录和最终 release 的 dev/inode；若公开路径被换绑则报告失败，但不会回滚已经完整提交到锁定目录的制品。

签名 key 参数在构建阶段只作为尚未解析的调用输入保存。Cargo、rustc、依赖构建脚本、Node、SBOM、第三方与标准库 notice、最后一份源码验证、包内文档检查、归档和 checksum 全部完成，并再次通过 exact-source gate 后，脚本才进入短生命周期签名子进程：在其中解析并要求私钥是当前账号拥有、mode `0400`/`0600`、单硬链接的普通文件，打开 fd 后复核 dev/inode，完成签名和验签，随后由进程退出关闭 fd。密钥算法还必须属于明确的发布 allowlist：Ed25519、Ed448、至少 3072 bit 的 RSA，或曲线为 `prime256v1`、`secp384r1`、`secp521r1` 的 ECDSA。弱 RSA、DSA、`secp256k1` 等未审核曲线、X25519 等非签名密钥，以及无法确定类型/强度的 key 都在签名前失败关闭。构建工具不会继承私钥 fd。但这不是同 UID 恶意代码隔离；正式签名应在独立账号、隔离主机或 HSM 中执行。

发布输出文件系统必须支持 Linux `RENAME_NOREPLACE`。脚本使用 GNU `mv --update=none --no-copy`，并且只有 source 消失、destination 是实体目录且设备号/inode 与移动前 source 相同时才确认发布；静默碰撞或身份不符都会失败。

脚本在同一文件系统的 `0700` 私有 stage 中构造一个完整的 `<release-name>.release` 目录。归档、外层 checksum、签名和公钥全部写完、验签并同步后，只用一次 no-clobber 目录 rename 公开该目录，再同步输出目录；rename 与输出目录同步组成一个暂时忽略 HUP/INT/TERM 的短提交段，普通信号不会卡在两者之间。公开名称因此不会经历“只有部分 sidecar”的状态，也不会覆盖已有文件、目录或 symlink。若在 rename 前遭遇 SIGKILL 或掉电，可能留下不可见的 `.dufs-release-stage.*` 私有目录；若在 rename 后、输出目录同步确认前遭遇 SIGKILL、掉电或同步错误，公开目录仍是一次 rename 产生的完整目录，但该目录项跨重启的持久性尚未确认，必须把该次发布视为失败并人工核验，不能仅凭“已经可见”判定发布成功：

```sh
cargo install cargo-cyclonedx --version 0.5.9 --locked
cargo install cargo-audit --version 0.22.2 --locked
chmod 0600 /secure/offline/dufs-release-key.pem
install -d -m 0700 ./dist
./scripts/package-release.sh \
  --signing-key /secure/offline/dufs-release-key.pem \
  --output-dir ./dist
```

固定时间戳、权限、经 commit-tree 复核的源码快照、离线 Cargo vendoring、隔离 Git/Cargo/npm 质量环境、编译路径映射、审核过的标准库 notice 摘要和 SBOM 根引用消除了脚本已知的路径、时间、replace object、private attributes 和用户 Git/Cargo/npm 配置非确定性。逐字节可重复归档仍要求相同源码、`SOURCE_DATE_EPOCH`、Rust/Node/npm/cargo-cyclonedx/coreutils/OpenSSL 工具版本、host target 及相同的已验证依赖内容；应在不同长度的 checkout 路径各构建一次并比较归档 SHA-256。外层签名是否逐字节相同还取决于所用密钥算法；验收依据是签名能验证同一归档 checksum，而不是签名字节相等。

生产主机验证时，公钥必须通过独立可信渠道取得并固定，不能只相信与归档从同一位置下载的临时公钥：

```sh
set -eu

bundle=/secure/releases/dufs-0.50.0-x86_64-unknown-linux-gnu-0123456789ab.release
pinned_public_key=/secure/trust/dufs-release-public.pem
test -d "$bundle"
test ! -L "$bundle"
test -f "$pinned_public_key"
test ! -L "$pinned_public_key"
cd -- "$bundle"
release_name="${bundle##*/}"
release_name="${release_name%.release}"
archive="${release_name}.tar.gz"
checksum="${archive}.sha256"
signature="${checksum}.sig"
openssl dgst -sha256 -verify "$pinned_public_key" \
  -signature "$signature" "$checksum"
sha256sum --check "$checksum"

verify_root="$(mktemp -d)"
chmod 0700 "$verify_root"
tar --extract --gzip --file "$archive" --directory "$verify_root"
release_dir="$verify_root/$release_name"
test -d "$release_dir"
test ! -L "$release_dir"
(cd "$release_dir" && sha256sum --check SHA256SUMS)

# 从独立可信的发布记录填写完整值，不从同一下载目录自行推断。
expected_version=0.50.0
expected_sha=0123456789abcdef0123456789abcdef01234567
expected_target=x86_64-unknown-linux-gnu
test "$("$release_dir/dufs" --version)" = \
  "dufs $expected_version (git $expected_sha)"
grep -Fx "format=dufs-build-environment-v2" \
  "$release_dir/BUILD-ENVIRONMENT.txt"
grep -Fx "source_sha=$expected_sha" "$release_dir/BUILD-ENVIRONMENT.txt"
grep -Fx "source_version=$expected_version" \
  "$release_dir/BUILD-ENVIRONMENT.txt"
grep -Fx "target=$expected_target" "$release_dir/BUILD-ENVIRONMENT.txt"
grep -Eq '^source_date_epoch=[0-9]+$' "$release_dir/BUILD-ENVIRONMENT.txt"
for key in \
  bash rustc cargo cargo_cyclonedx cargo_audit \
  rustsec_advisory_db_revision rustsec_advisory_db_fetch_epoch \
  node npm git openssl tar gzip mv sha256sum
do
  grep -Eq "^${key}=.+$" "$release_dir/BUILD-ENVIRONMENT.txt"
done
# 验证和升级完成后，只删除本次 mktemp 返回的精确目录。
# rm -rf -- "$verify_root"
```

上面的 `openssl dgst` 适用于发布策略允许的 RSA（至少 3072 bit）和 ECDSA（`prime256v1`、`secp384r1`、`secp521r1`）digest 签名密钥。若固定公钥是 Ed25519 或 Ed448，发布脚本会改用 EdDSA 原始消息模式，验签命令应替换为：

```sh
openssl pkeyutl -verify -rawin -pubin \
  -inkey "$pinned_public_key" \
  -sigfile "$signature" \
  -in "$checksum"
```

升级步骤：

1. 审阅目标版本提交和发布说明，确认配置、网关和文件系统行为变化。
2. 验证签名、checksum、`BUILD-ENVIRONMENT.txt` 的 SHA/版本/target/工具字段、SBOM、`THIRD_PARTY_LICENSES.txt`、`RUST-STANDARD-LIBRARY-COPYRIGHT.html` 和二进制嵌入 SHA；在隔离环境运行完整检查及数据副本冒烟测试。
3. 创建共享根一致性快照，并备份旧二进制和配置。
4. `systemctl stop dufs`，确认优雅停机完成。
5. 确认 `/opt/dufs/bin` 由 root 拥有且不允许不可信用户写入。把新二进制先安装到该目录中的私有临时路径，因此临时文件与目标必在同一文件系统；同步文件后用一次 rename 原子替换，再同步父目录。rename 与父目录同步必须作为一个暂时忽略 HUP/INT/TERM 的短提交段；配置有变化时另行生成新文件并审阅差异：

   ```sh
   set -eu

   replacement="$(mktemp --tmpdir=/opt/dufs/bin .dufs.new.XXXXXXXX)"
   cleanup_replacement() {
     [ -n "$replacement" ] || return 0
     rm -f -- "$replacement"
   }
   trap cleanup_replacement EXIT
   trap 'exit 129' HUP
   trap 'exit 130' INT
   trap 'exit 143' TERM

   install -o root -g root -m 0755 "$release_dir/dufs" "$replacement"
   sync "$replacement"

   commit_status=0
   trap '' HUP INT TERM
   if mv --no-target-directory -- "$replacement" /opt/dufs/bin/dufs; then
     replacement=
     sync /opt/dufs/bin || commit_status=$?
   else
     commit_status=$?
   fi
   trap 'exit 129' HUP
   trap 'exit 130' INT
   trap 'exit 143' TERM

   if [ "$commit_status" -ne 0 ]; then
     printf '%s\n' \
       'Binary rename or parent-directory sync failed; keep Dufs stopped.' >&2
     exit "$commit_status"
   fi
   trap - EXIT HUP INT TERM
   ```

   普通信号不会在 rename 与父目录同步之间终止这段命令；SIGKILL、掉电和真实同步错误仍无法被 shell 屏蔽。后一类故障可能留下已经可见但持久性未确认的新二进制，应保持服务停止并先核验目标 inode、内容摘要和文件系统状态。

6. 启动后检查 journal、liveness、登录和文件操作，再恢复流量。

## 7. 回滚

如果新进程无法启动或只出现与二进制/配置有关的回归：

1. 停止服务并保存新版本日志。
2. 恢复经过 checksum 验证的旧二进制和对应配置。
3. 启动并完成同一组冒烟测试。

回滚二进制不会撤销升级后用户已经完成的文件写入、移动或删除。只有确认数据被新版本错误修改时，才应在保全现场后按恢复流程从升级前一致性快照恢复。禁止把“恢复旧程序”和“覆盖共享根”合并成一个无确认脚本。

## 8. 事件响应

发现疑似入侵时，先限制入口和后端网络访问，保全 journal、网关日志、配置、二进制摘要和共享根快照，再轮换网关密钥、账号密码及其他可能暴露的凭据。漏洞使用 GitHub Private Vulnerability Reporting 私密提交；公开 issue 不得包含生产路径、账号、密码、密钥、共享文件或日志中的敏感内容。安全修复只面向当前版本与当前 `main`。
