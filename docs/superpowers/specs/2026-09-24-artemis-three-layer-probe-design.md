# RepairArtemisWeb —— 组件表 + 三层探针 + nginx 归因 设计

> 注：本文中所有 IP 均为 RFC 5737 文档保留段（`203.0.113.0/24`）占位地址，不指向任何真实主机；现场排障记录已按此脱敏。

日期：2026-09-24
状态：待评审
范围：`src/main.rs`（Rust）与 `cpp/RepairArtemisWeb.cpp`（C++）双版本

---

## 1. 问题陈述

2026-09-24 现场排障（目标机 203.0.113.170）暴露了现有工具的三个判定错误。现场事实：

| 观测 | 结果 |
| --- | --- |
| `https://203.0.113.170/artemis-web/` | 502 Bad Gateway（nginx 返回） |
| `https://203.0.113.170/artemis-portal/` | 502 |
| `https://203.0.113.170/artemis/api/resource/v1/cameras` | 502 |
| `https://203.0.113.170/`（平台首页） | 200 |
| 远程端口扫描 170 | 仅 `80/443/8080` 可达，`9016/9017/9018/5432/6379/9000` 均不可达 |
| 同网段正常参照机 203.0.113.17 | 上述三条 artemis 路由全部 200，`9016/9017/9018` 均监听 |

结论：故障机 nginx 本身健康，是**三个 artemis 后端全部未监听**，nginx 转发失败才回 502。

对照这份现场，现有实现（`src/main.rs`）有三处会给出错误结论：

1. **9016 网关只告警、不修复**（`src/main.rs:646-663` 把 9016 放进 `PREREQ` 仅 `Warn` + `issues += 1`，`repair_component` 只处理 web/portal）。但 nginx 的 `location ^~ /artemis` 正是转发到 9016。工具把 web/portal 修好后报"修复成功"，用户打开页面**照样 502**。
2. **`http_up()` 判定过弱**（`src/main.rs:282` 只检查响应前 512 字节里有没有 `HTTP` 这 4 个字节）。`HTTP/1.1 500`、`503` 全算通过。且它只测 `127.0.0.1:9017` 这条**直连**路径，从不测用户实际访问的 **nginx 路径**——而本次故障的形态恰恰相反：直连可能通、经 nginx 是 502。
3. **完全没有 nginx 层归因**。本次真正定位靠的是读 `conf/nginx.conf` 的 upstream 映射与 `ssl/artemis.conf` 的 `set $artemis "local"`。若某台机器该值是 `"remote"`，`/artemis-web` 会被转到 `https_artemis_remote = 127.0.0.1:443`，即 **nginx 自己**，形成回环必然 502，而此时三个后端全都是健康的。工具对这类故障无感。

附带缺陷（在本次要改动的代码路径上，一并修正）：

- `src/main.rs:374` 的 `ok()?` 位于 `for` 循环体内，会让整个 `find_node_exe()` 提前返回 `None`——遍历中碰到一个无权限目录就放弃全盘查找。
- `print_wrapper_tail()`（`src/main.rs:443`）用 `fs::read_to_string` 读整个日志文件，日志增大时卡顿/吃内存；且失败提示里叫用户去看的 `log.txt` 实际从未被读取。
- pathname 用组件名 `name` 硬凑（`http_up(port, name)`），而真实值来自 `config.properties` 的 `server.pathname`，当前只是恰好相等。

## 2. 目标与非目标

**目标**

- G1 把 9016 网关纳入修复范围，且修复动作与其真实技术栈（prunsrv）匹配。
- G2 健康判定按组件分别定义期望状态码，替换"有没有 HTTP 字样"。
- G3 新增"经本机 nginx 443"的端到端验收，并与直连结果**分开报告**。
- G4 新增 nginx 层只读归因，能区分"后端没起"与"后端健康但转发层坏了"。
- G5 失败时输出可据以行动的日志证据，而不是"请自行查看 wrapper.log"。

**非目标（明确排除）**

- N1 不解决 Rust/C++ 双版本重复维护问题——本次改动两版各写一遍。已就此向用户提示成本，用户选择不纳入本轮。
- N2 不写通用 nginx 配置解析器，不做 nginx 配置的修改、reload 或任何写操作。
- N3 不修复 nginx / PostgreSQL / redis / minio 本身，只探测并报告。
- N4 不引入任何第三方 crate / 库 / 外部可执行文件，保持单文件免依赖特性。

## 3. 现场核实到的事实（设计依据）

全部来自 203.0.113.17（正常参照机）的只读勘察。

**Java 网关不是 node 服务。** 它是 Apache Commons Daemon（prunsrv）：

```
SERVICE_NAME: artemis
BINARY_PATH_NAME: "...\OpenAPI\artemis\bin\artemis\bin\windows\artemis.exe" //RS//artemis
```

生命周期脚本 `bin/artemis/bin/__service.bat` 接受 `{console|start|stop|restart|install|uninstall}`，内部用 `//IS// //DS// //ES// //SS//`；脚本自身有 `CD /D "%~dp0"` + `CD /D ../`，会定位到 `bin/artemis`，因此调用方无需预设工作目录。服务名取自脚本内 `set _ServerName=artemis`。

**网关端口与路径**在 `bin/artemis/application.properties`：

```
server.context-path=/artemis
server.port=9016
server.ssl.enabled=false
```

实测 `http://127.0.0.1:9016/artemis` → **302**，`/` → 404。即网关"健康"的表现是 3xx/404，**不是** 200。

**node 组件**在 `bin/<name>/<name>/`，含 `koa-app.js`、`service.install.js`、`service.uninstall.js`、`daemon/`；`config.properties` 提供 `server.port`、`server.pathname`（如 `/artemis-web`）、`service.name`、`log.path`（如 `../../../logs/artemis-web`）。

**node 组件日志分工**（`daemon/` 下）：`<svc>.err.log` 存崩溃栈（实测有 `Error: read ECONNRESET`），`<svc>.out.log` 存带状态码的访问日志（实测 `GET /artemis-web/ 200 1ms`），`<svc>.wrapper.log` 只有进程启动器信息。现有代码只 tail 了最没用的那个。

**网关日志**：prunsrv `--LogPath=.\logs --LogPrefix=artemis` → `bin/artemis/logs/artemis*.log`，实测有 `artemis.2026-09-24.log`、`artemis-stdout.2026-09-24.log`、`gc.log.0.current`。

**nginx 路由映射**（`Web Service/nginx/conf/`）：

```nginx
upstream http_artemis        { server 127.0.0.1:9016; }
upstream http_artemis_web    { server 127.0.0.1:9017; }
upstream http_artemis_portal { server 127.0.0.1:9018; }
upstream https_artemis_remote{ server 127.0.0.1:443; }   # ← nginx 自身
```

`conf/Mode/nginx_artemis.conf` 三条 `location ^~ /artemis{|-web|-portal}` 结构相同：

```nginx
if ($scheme = "http") { break; }          # HTTP 不转发，只走 HTTPS
if ($artemis = "local")  { proxy_pass http://http_artemis_web; }
if ($artemis = "remote") { proxy_pass https://https_artemis_remote; }
```

`$artemis` 由 `nginx.conf:290` 的 `include ../../ssl/artemis.conf;` 引入，该文件内容为 `set $artemis "local";`（相对 conf 目录即 `Web Service/ssl/artemis.conf`）。

两个关键推论写进设计：

- **HTTP(80) 无法验证 artemis 路由**（`if ($scheme = "http") { break; }` 直接跳出，不转发）。端到端验收**必须**走 443/TLS。
- `$artemis = "remote"` 时上游是 nginx 自己的 443 → 回环 → 必然 502，且此时三个后端全健康。这是"后端健康但 502"的一个确定成因。

## 4. 决策记录

### D1 端到端 HTTPS 用 WinHTTP，不用 curl.exe

G3 要求发 HTTPS 请求到 443 并忽略自签证书。项目宣称"零第三方依赖 / 纯 Rust 标准库"，而 **Rust 标准库没有 TLS 客户端**。候选：

| 方案 | 评估 |
| --- | --- |
| **WinHTTP（选定）** | 系统自带 `winhttp.dll`，Vista+；`WINHTTP_OPTION_SECURITY_FLAGS` 可忽略证书校验；不 spawn 进程；C++ 侧 `winhttp.h` 原生，Rust 侧沿用项目既有的 `#[link]` FFI 风格。 |
| 调用 `curl.exe` | System32 确有（Win10 1803+），但 README 声称支持 Windows 7+，且为取一个状态码 spawn 进程不如 FFI 干净。 |
| 引入 `ureq`/`reqwest` | 直接违反 N4 与项目"零依赖"卖点。排除。 |

Rust 侧需声明：`WinHttpOpen`、`WinHttpConnect`、`WinHttpOpenRequest`、`WinHttpSetOption`、`WinHttpSendRequest`、`WinHttpReceiveResponse`、`WinHttpQueryHeaders`、`WinHttpCloseHandle`。C++ 侧 `cpp/build.bat` 增加 `winhttp.lib`。

Win7 的 WinHTTP 默认只启用 TLS 1.0，故显式设置 `WINHTTP_OPTION_SECURE_PROTOCOLS` 为 TLS1.0|1.1|1.2|1.3 的按位或（不存在的位被拒绝时回退）。

### D2 网关重装需要显式授权，默认只到 restart

prunsrv `uninstall`+`install` 会重建 Java 服务的注册信息，在运行的安防平台上属于比 node 组件更大的爆炸半径。因此网关的修复阶梯比 node 保守一级（见 §6）。

### D3 新增退出码 3 = 后端健康但转发层故障

让调用方脚本能区分"该修服务"（1）与"该查 nginx 配置"（3），避免无脑循环重跑修复。

## 5. 组件描述表

新增静态表，两版结构完全对齐：

| 字段 | artemis | artemis-web | artemis-portal |
| --- | --- | --- | --- |
| `key` | `artemis` | `artemis-web` | `artemis-portal` |
| `kind` | `Prunsrv` | `Node` | `Node` |
| `dir`（相对 root） | `bin/artemis` | `bin/artemis-web/artemis-web` | `bin/artemis-portal/artemis-portal` |
| 服务名来源 | `bin/artemis/bin/__service.bat` 的 `set _ServerName=` | `config.properties` 的 `service.name` | 同左 |
| 服务名兜底 | `artemis` | `artemis-web` | `artemis-portal` |
| 端口来源 | `application.properties` 的 `server.port` | `config.properties` 的 `server.port` | 同左 |
| 端口兜底 | 9016 | 9017 | 9018 |
| pathname 来源 | `application.properties` 的 `server.context-path` | `config.properties` 的 `server.pathname` | 同左 |
| pathname 兜底 | `/artemis` | `/artemis-web` | `/artemis-portal` |
| L2 期望状态码 | `2xx ∪ 3xx ∪ {404}` | `2xx ∪ 3xx` | `2xx ∪ 3xx` |
| L3 期望状态码 | `2xx ∪ 3xx` | `2xx ∪ 3xx` | `2xx ∪ 3xx` |
| `start_timeout` | 90s | 60s | 60s |
| 存在性判据 | `bin/windows/artemis.exe` 存在 | `koa-app.js` 存在 | `koa-app.js` 存在 |

说明：

- 网关 L2 额外接受 404，因为 `/artemis` 在无会话时可能直接回 404 而非 302。**期望集合就是 `2xx ∪ 3xx ∪ {404}`，其余（含 401/403 等其他 4xx 与全部 5xx）一律判失败**——不设"只要不是 5xx 就算活"这种宽泛规则，否则两版会各自发挥。L3 不接受 404：经 nginx 后 404 意味着请求根本没被转发、被当静态文件处理了（§3 里 170 的历史日志正是这个形态），属于故障。
- `start_timeout` 差异源于 Spring Boot 启动要连 PostgreSQL/redis，实测明显慢于 node。
- 端口/pathname 解析失败时**用兜底值并 Warn**，不中断。

## 6. 三层探针与归因矩阵

### 探针定义

- **L1 端口监听**：沿用 `port_listening()`（netstat 优先、TCP 兜底）。
- **L2 直连 HTTP**：`http_probe(port, pathname)` → `Probe { reachable: bool, status: Option<u16> }`。明文 HTTP over `TcpStream`（三组件 `server.ssl.enabled=false` / node http）。**必须正确解析状态行** `HTTP/1.x <code>`，不再用"有没有 HTTP 字样"。请求头带 `Connection: close`，读取到状态行即可返回。
- **L3 经 nginx 端到端**：`https_probe(host, 443, pathname)`，WinHTTP，忽略证书。默认 `host = 127.0.0.1`（实测健康机 `https://127.0.0.1/artemis-web/` → 200，与走局域网 IP 命中同一 server 块，故无需枚举本机 IP）。

### 归因矩阵

对每条路由独立求值。`–` = 不评估。**按行序自上而下首次匹配**（行 4 的 5xx 是行 7 的子集，顺序即优先级）：

| # | L1 | L2 | L3 | 归因 | 动作 |
| --- | --- | --- | --- | --- | --- |
| 1 | ✓ | ✓ | ✓ | 正常 | 跳过 |
| 2 | ✗ | – | – | 后端未监听端口 | 按 `kind` 修复（下表） |
| 3 | ✓ | 连接成功但无 HTTP 状态行 | – | 端口被**非 HTTP** 进程占用 | 报占用 PID，**不自动修** |
| 4 | ✓ | 5xx | – | 应用已启动但内部错误 | 出日志证据（§8），**不重装** |
| 5 | ✓ | ✓ | ✗ | **后端健康，nginx 转发层故障** | 进 nginx 归因（§7），**不重启后端**；退出码 3 |
| 6 | ✓ | ✓ | nginx 443 不可达 | nginx 未就绪 | 报 nginx 状态；退出码 3 |
| 7 | ✓ | ✗（其他不符合期望集合的状态码） | – | 后端响应异常 | 先修后端，修完复测 L3 |

行 2 的"动作"仅在修复模式生效；`--check-only` 下所有行都只做归因报告，不执行动作。

**核心不变式：只要 L2 通过，就绝不因为 L3 失败而重装后端。** 这条正是现有工具在最像本次故障的场景上会做错的地方。

**可测试性要求**：归因必须是纯函数 `attribute(l1, l2, l3) -> (归因, 动作, 退出码贡献)`，探针调用在其外完成。否则矩阵无法用注入结果做单元测试（见 §11）。两版都要按这个边界切分，把矩阵写成数据表而非散落的 `if`。

### 修复动作按 kind 分派

**Node**（保留现有流程）：
`service.uninstall.js` → `sc stop` → `sc delete` → 以 `dir` 为 cwd 跑 `service.install.js` → 需要时 `sc start` → 等 L1（上限 `start_timeout`）→ 复测 L2/L3。

**Prunsrv**（新增，比 Node 保守一级，依 D2）：

1. 服务 `Missing` → `cmd /C "<dir>\bin\__service.bat" install`，再 `sc start`
2. 服务 `Stopped` → `sc start`
3. 服务 `Running` 但 L1 ✗ → 等待至 `start_timeout`，仍 ✗ 则 `__service.bat restart`
4. restart 后仍 ✗ → **仅当 `--reinstall`** 才 `__service.bat uninstall` + `install`；否则报失败并出日志证据

`__service.bat` 的 stdout/stderr 按现有 `run_node_logged()` 的方式并入日志。

## 7. nginx 层归因（只读）

### 定位

在 `SEARCH_BASE`（`C:\Program Files (x86)\iSecure VMS`）下查找 `conf\nginx.conf`，其所在目录的父目录即 nginx 根。实测命中深度 5：`VSM Servers\Web Service\nginx\conf\nginx.conf`。目录名大小写不敏感（`nginx` / `Nginx` 均出现于现场）。`--nginx-root` 可覆盖。

### 解析（行式，非通用 nginx 语法）

1. `upstream <NAME> {` … `}` → 取块内首个 `server <HOST>:<PORT>`。
2. `location <修饰符> <PATH> {` … 配对 `}`（花括号计数）→ 取块内 `proxy_pass <TARGET>`。若 TARGET 为 `http://<NAME>` 且 `<NAME>` 是已知 upstream → 解析出端口；若 TARGET 含 `$`（如 `$scheme://$cookie_transpondvsm`）→ 标记"动态路由，静态不可判定"，不报错。
3. 在 `conf/`、`conf/Mode/`、`../ssl/` 的 `*.conf` 中查找 `set $artemis "<V>"`，报告值与 `文件:行号`。
4. 若 `$artemis != "local"`：明确报"local 分支失效，`/artemis*` 三条路由落到 `https_artemis_remote = 127.0.0.1:443`（nginx 自身）→ 回环 502"，并给出该行位置。这是**结论**，不是猜测。

### 输出形态

```
---- nginx 路由归因 (只读) ----
nginx 根: C:\...\VSM Servers\Web Service\nginx
$artemis = "local"   (来自 ..\ssl\artemis.conf:1)

路由              upstream              目标              L1     L2直连   L3经nginx
/artemis          http_artemis          127.0.0.1:9016    ✓      302 ✓    200 ✓
/artemis-web      http_artemis_web      127.0.0.1:9017    ✓      200 ✓    200 ✓
/artemis-portal   http_artemis_portal   127.0.0.1:9018    ✓      200 ✓    200 ✓
```

L2/L3 列复用 §6 的探针结果，因此这张表就是归因矩阵的呈现载体；不一致行在表下方展开为结论 + 建议动作。

### 明确边界

不修改任何 nginx 文件、不 reload、不递归展开 `include`（只扫 §7.3 列出的固定三处）。若配置结构超出上述假设，输出"无法静态判定"并继续，不崩。

## 8. 日志证据提取

失败时按 `kind` tail **正确**的文件：

- Node：`daemon/<svc>.err.log`（崩溃栈）、`daemon/<svc>.out.log`（访问日志含状态码）、`daemon/<svc>.wrapper.log`（启动器），以及 `config.properties` 的 `log.path` 按 `dir` 解析出的目录。
- Prunsrv：`<dir>\logs\artemis*.log`、`artemis-stdout*.log`；`gc.log*` 仅在出现 OOM 特征时提。

实现约束：

- **反向分块读取尾部**，单次上限 64KB，禁止整文件 `read_to_string`（修掉 `src/main.rs:443` 的问题）。
- 对尾部内容做**特征匹配**，命中即输出一行归因，把"你自己去看日志"变成结论：

| 特征 | 归因 |
| --- | --- |
| `ECONNREFUSED` | 上游依赖（PG/redis/网关）未就绪 |
| `EADDRINUSE` / `already in use` | 端口冲突 |
| `OutOfMemoryError` / `heap size` | JVM 堆不足 |
| `FATAL.*database` / `Connection refused.*5432` | PostgreSQL 不可用 |
| `ECONNRESET` | 与网关/DB 的连接被重置 |

未命中特征时只输出尾部原文，不编造结论。

## 9. CLI 与交互

新增参数：

```
--components <a,b,c>   默认 artemis,artemis-web,artemis-portal
                       （--components artemis-web,artemis-portal 可还原旧行为）
--nginx-root <目录>    覆盖 nginx 自动定位
--no-e2e               跳过 L3（离线/无 nginx 环境用）
--e2e-host <主机>      L3 目标，默认 127.0.0.1
--yes                  对网关 reinstall 跳过交互确认
```

保留：`-c/--check-only`、`-r/--reinstall`、`--root`、`-h/--help`。

`--check-only` 升级为完整的三层 + nginx 归因报告（只读、无需管理员权限），即本次故障下**先跑这一条就能定位**。

交互菜单维持三项，语义更新：

```
[1] 标准修复   三层探针 + 按 kind 修复（网关默认只 restart）
[2] 仅检查     三层探针 + nginx 归因，只读，无需管理员
[3] 强制重装   含网关 uninstall/install，需确认或 --yes
```

退出码：`0` 无异常 · `1` 存在异常/修复失败 · `2` 参数错误 · `3` 后端健康但 nginx 转发层故障（依 D3）。

## 10. 顺带修正

在改动路径上，一并处理（各 ≤ 数行，不扩散）：

- `find_node_exe` 的 `ok()?` 提前返回：改为 `match`/`unwrap_or_else`，跳过单个条目而不中断遍历。
- pathname 改为从配置读（已并入 §5 表），不再用组件名硬凑。
- `http_up` 由 `http_probe` 取代并删除。

## 11. 测试策略

- 在 `tests/fixtures/` 放真实脱敏样本：`config.properties`、`application.properties`、`__service.bat`、`nginx.conf`、`Mode/nginx_artemis.conf`、`ssl/artemis.conf`、以及一段含 `ECONNREFUSED` 的 `.err.log`。
- **Rust**：`#[cfg(test)]` 单元测试覆盖 —— properties 解析（含注释行 `#server.port` 不得误匹配）、upstream/location 提取、`$artemis` 取值、归因矩阵每条分支、反向 tail 的边界（空文件 / 小于 64KB / 多块）。
- **C++**：仓库内无测试框架，**本轮不引入**。
- 双版本一致性：以"两版在同一台真实平台上跑 `--check-only`，输出逐行 diff"作为验收步骤，写入 README 的验证章节。

**已接受的风险**：C++ 版无自动化测试，解析器与归因矩阵的行为漂移只能靠人工 diff 发现。这是用户选择保留双版本手写（N1）的直接后果，spec 阶段如实记录，不假装已解决。

## 12. 受影响文件

| 文件 | 变更 |
| --- | --- |
| `src/main.rs` | 组件表、`http_probe`、WinHTTP FFI、归因矩阵、prunsrv 修复分支、nginx 解析、反向 tail、`find_node_exe` 修正、新参数、退出码 |
| `cpp/RepairArtemisWeb.cpp` | 同上，结构逐条对齐 |
| `cpp/build.bat` | 链接 `winhttp.lib` |
| `tests/fixtures/*` | 新增 |
| `README.md` | 功能表加入 9016、三层探针与归因矩阵、新参数、退出码 3、双版本 diff 验证步骤 |
| `Cargo.toml` | `version` 递增 |

## 13. 验收标准

以本次 203.0.113.170 故障为回归用例：

1. 在"三后端全挂、nginx 正常"的机器上，`--check-only` 必须在 L1 列全部标 ✗，并把 9016 列为**待修组件**而非警告项。
2. 在"三后端全健康、`$artemis = remote`"的场景上，必须归因为"后端健康，nginx 回环"，退出码 `3`，且**不发起任何后端重启**。落地方式：`tests/fixtures/nginx-remote/` 下放一份 `conf/nginx.conf` + `conf/Mode/nginx_artemis.conf` + `ssl/artemis.conf`（`set $artemis "remote"`），归因逻辑通过**注入探针结果**（L1✓/L2✓/L3✗）的单元测试覆盖，不依赖真机端口——开发机上 9016/9017/9018 已被真实服务占用，起桩会冲突。`--nginx-root` 指向该 fixture 仅用于验证 §7 的解析与 `$artemis` 取值。
3. 修复完成后，工具必须报告 L3（经 nginx 443）的实际状态码；只有 L1+L2+L3 全通过才输出"修复成功"。
4. `--components artemis-web,artemis-portal` 时行为与当前版本一致（不触碰网关）。
5. 非管理员运行 `--check-only` 必须完整可用且不报错。
