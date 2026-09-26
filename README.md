# RepairArtemisWeb

> iSecure VMS OpenAPI `artemis` / `artemis-web` / `artemis-portal` 组件修复与三层归因诊断工具
> （Rust / C++ 双版本，单文件免依赖，中文界面）

当海康 iSecure VMS（综合安防管理平台）的 OpenAPI 页面打不开（典型症状：浏览器 `https://<平台IP>/artemis-web/` 返回 **502 Bad Gateway**，而服务管理器里所有服务都显示"已启动"）时，本工具用**三层探针**定位故障到底在后端服务还是在 nginx 转发层，并按组件类型自动修复，是运维人员的"服务急救包"。

> "服务已启动" 只反映 Windows 服务控制管理器的状态，**不代表端口在监听、更不代表 HTTP 可用**。
> 本工具的判据全部落在端口 / HTTP 状态码 / 经 nginx 的实际响应上。

- 开发人：余志强　QQ: 379008610　主页：https://github.com/xiaoyuzhi
- 版权：Copyright (c) 2026 余志强 (Yu Zhiqiang)
- 许可：本项目基于 [MIT License](LICENSE) 开源

---

## 功能

1. **定位** OpenAPI 安装目录与内置 `node.exe`（默认标准路径 `C:\Program Files (x86)\iSecure VMS\VSM Servers\OpenAPI\artemis`，可用 `--root` 覆盖）；
2. **检查前置组件**：redis / postgresql / minio / nginx 是否就绪；
3. **三层探针 + 逐个修复** `artemis`(Java 网关 9016) / `artemis-web`(9017) / `artemis-portal`(9018)：

   | 层级 | 判据 |
   | --- | --- |
   | L1 | 端口是否监听 |
   | L2 | 直连状态码是否落在该组件期望集合（网关接受 302/404，web/portal 要求 2xx/3xx） |
   | L3 | 经本机 nginx 443 访问 `/<pathname>/` 的实际状态码 |

   网关为 prunsrv 服务，默认只 `restart`；卸载重装需 `--reinstall` 且交互确认。

4. **nginx 层只读归因**：解析 `nginx.conf` 与 `conf/Mode/`、`../ssl/` 下的 `*.conf`，列出 `/artemis*` 各 location 实际转发到的 upstream:端口，并识别 `$artemis` 取值非 `local` 时的**回环**配置（`/artemis*` 被转给 nginx 自己 → 必然 502）。本工具**不修改、不 reload nginx 配置**。
5. **失败时输出日志证据**：按组件类型 tail 正确的日志文件（node → `daemon/<svc>.err.log` 等；网关 → `logs/artemis*.log`），反向读取、单次上限 64KB，并对已知特征给出结论（`ECONNREFUSED` → 上游依赖未就绪、`EADDRINUSE` → 端口冲突、`OutOfMemoryError` → JVM 堆不足…）。
6. 输出**路由归因表**与汇总，退出码区分"后端异常"与"nginx 转发层故障"。

修复完成后可通过以下地址验证：

```
本机:   http://127.0.0.1:9017/artemis-web/
        http://127.0.0.1:9018/artemis-portal/
对外:   https://<本机IP>/artemis-web/    (经平台 nginx 443 对外)
        https://<本机IP>/artemis-portal/
```

> 若本机直连 9017 正常但 `https://<IP>/artemis-web/` 仍 502，属 nginx 转发层问题，
> 用 `--check-only` 查看路由归因表，不要反复重启后端服务。

## 归因矩阵

对每条路由（`/artemis`、`/artemis-web`、`/artemis-portal`）独立求值，**自上而下首次匹配**。`–` = 不评估。

| # | L1 | L2 | L3 | 归因 | 动作 |
| --- | --- | --- | --- | --- | --- |
| 1 | ✓ | ✓ | ✓ | 正常 | 跳过 |
| 2 | ✗ | – | – | 后端未监听端口 | 按组件类型修复阶梯处理 |
| 3 | ✓ | 连接成功但无 HTTP 状态行 | – | 端口被**非 HTTP** 进程占用 | 报占用 PID，**不自动修** |
| 4 | ✓ | 5xx | – | 应用已启动但内部错误 | 输出日志证据与特征归因，**不重装** |
| 5 | ✓ | ✓ | ✗ | **后端健康，nginx 转发层故障** | 进 nginx 归因，**不重启后端**；退出码 3 |
| 6 | ✓ | ✓ | nginx 443 不可达 | nginx 未就绪 | 报 nginx 状态；退出码 3 |
| 7 | ✓ | ✗（其他不符合期望集合的状态码） | – | 后端响应异常 | 先修后端，修完复测 L3 |

第 2 行的"动作"仅在修复模式生效；`--check-only` 下所有行都只做归因报告，不执行任何修改。

**核心不变式：只要 L2 通过，就绝不因为 L3 失败而重装后端。**


## 特性

- **零第三方依赖**：Rust 版仅用标准库 + 少量 Win32 FFI；C++ 版静态编译，均为**单文件免运行库**，拷走即用；
- **双模式运行**：带参数 = 命令行模式；不带参数 = 中文交互式菜单（回车默认标准修复）；
- **三档操作强度**：标准修复 / 仅检查（只读）/ 强制重装；
- **不碰 nginx 配置**：转发层只做只读归因，改与不改由运维决定；
- **危险动作需授权**：Java 网关的卸载重装既要 `--reinstall`，也要交互确认（非交互环境一律不做）；
- 分级彩色日志、管理员检测、控制台 UTF-8 自适应（中文不乱码）。

## 运行环境

| 项 | 要求 |
| --- | --- |
| 系统 | Windows 7+（x64） |
| L3 端到端验收 | Windows 10 1803+（需系统内置 `System32\curl.exe`；缺失时该层降级为"跳过"，L1/L2 与 nginx 静态归因不受影响） |
| 权限 | 标准修复 / 强制重装需**管理员**；仅检查无需 |
| 对象平台 | 已部署 iSecure VMS 的服务器（本机为平台或平台节点） |

> 在 iSecure VMS 服务器上，请右键"以管理员身份运行"。

## 使用说明

### 交互式菜单（双击运行即可）

```
============================================================
  RepairArtemisWeb — iSecure VMS OpenAPI 组件修复工具
============================================================
  [1] 标准修复  三层探针 + 按类型修复 (Java 网关默认只 restart)
  [2] 仅检查    三层探针 + nginx 归因, 只读, 无需管理员
  [3] 强制重装  含网关 uninstall/install, 需交互确认
  [0] 退出
  请输入序号并回车 (直接回车默认 1):
```

### 命令行参数

```
RepairArtemisWeb.exe [选项]
  -c, --check-only        仅诊断, 不做任何修改 (无需管理员权限)
  -r, --reinstall         允许卸载并重装服务 (含 Java 网关)
      --components <a,b>  只处理指定组件, 默认 artemis,artemis-web,artemis-portal
      --root <目录>       指定 OpenAPI 根目录 (默认标准安装路径自动定位)
      --nginx-root <目录> 指定 nginx 根目录, 须含 conf/nginx.conf (覆盖自动定位)
      --no-e2e            跳过 L3 端到端验收 (无 nginx / 离线环境)
      --e2e-host <主机>   L3 目标主机, 默认 127.0.0.1
      --yes               跳过网关重装的交互确认
  -h, --help              显示帮助
```

退出码：

| 码 | 含义 |
| --- | --- |
| `0` | 无异常 |
| `1` | 存在异常或修复失败 |
| `2` | 命令行参数错误（含 `--nginx-root` 指向的目录下没有 `conf/nginx.conf`）/ 权限不足 |
| `3` | **后端健康但 nginx 转发层故障** —— 别动后端，去查 nginx 配置 |

### 出问题时先跑这一条

```bat
RepairArtemisWeb.exe --check-only
```

只读、无需管理员，输出路由归因表：每行的 L1 / L2 / L3 与结论，能直接区分"后端没起来"与"后端好好的、nginx 没转发生效"。

## 目录结构

```
.
├── src/
│   ├── main.rs              # 修复阶梯、流程组装、CLI、交互菜单
│   ├── model.rs             # 组件描述表、配置解析、三层结果与归因矩阵 (纯逻辑)
│   ├── probe.rs             # L1 端口 / L2 直连 HTTP / L3 经 nginx 端到端
│   ├── nginx.rs             # nginx 配置只读解析与回环归因
│   └── logs.rs              # 日志尾部反向读取与故障特征归因
├── assets/
│   ├── app.ico              # 程序图标 (16/32/48/64/128/256 六档)
│   └── app.rc               # 图标 + VERSIONINFO 版本资源, 两版共用
├── tests/fixtures/          # 单元用例用的假 root 与 nginx 配置
├── Cargo.toml               # Rust 工程 (零第三方依赖)
├── build.rs                 # 调 rc.exe 编 assets/app.rc, 把资源链进 exe
├── .cargo/config.toml       # 静态链接 MSVC CRT, 单文件免运行库
├── cpp/
│   ├── RepairArtemisWeb.cpp # C++ 版全部源码
│   └── build.bat            # C++ 一键编译脚本
├── docs/superpowers/        # 设计与实施计划文档
└── .gitignore               # 排除 target/ 与 *.exe 构建产物
```

Rust 版有 `cargo test` 覆盖纯逻辑（归因矩阵、配置解析、nginx 解析、探针归类、CLI 解析、报告表）。C++ 版无自动化测试，靠下面的手工比对保证两版一致。

## 从源码构建

### Rust 版

前置：Rust 工具链（MSVC 目标）+ Windows 10/11 SDK（`build.rs` 要用其中的 `rc.exe` 编图标与版本资源；找不到时设环境变量 `RC` 指向 `rc.exe`）。

```powershell
cargo build --release
# 产物: target\release\RepairArtemisWeb.exe  (单文件, 免 VC 运行库)
cargo test --bin RepairArtemisWeb     # 跑纯逻辑单元测试
```

### C++ 版

前置：安装 [MSYS2](https://www.msys2.org/) 的 `mingw-w64-x86_64-gcc`（含 `windres`，用于同一份 `assets/app.rc`）。

```bat
rem 双击或在命令行执行
cpp\build.bat
rem 产物: cpp\RepairArtemisWeb.exe  (单文件, 免运行库)
```

> 两个版本功能完全一致，可任选其一部署；仓库内不包含编译产物。
> 图标与版本号由 `assets/app.rc` 单点定义，两版共用；`Cargo.toml` 与该文件里的版本号由单元测试钉住不分叉。

## 双版本一致性验证

C++ 版没有自动化测试。发布前在任意一台**已部署平台**的机器上分别运行两版：

```bat
RepairArtemisWeb-rs.exe  --check-only > out-rs.txt  2>&1
RepairArtemisWeb-cpp.exe --check-only > out-cpp.txt 2>&1
fc out-rs.txt out-cpp.txt
```

每行开头的 `[HH:MM:SS]` 时间戳必然不同，比对时忽略该前缀；其余文本须逐行一致。
差异若不能归因到时间戳，即为两版行为分歧，按缺陷处理。

比对必须覆盖到**第 7 步汇总**那一行才算数：只跑 `--check-only` 而本机没装平台时，两版都会在第 1 步"未找到 OpenAPI 目录"就退出，归因表、nginx 解析、退出码这些路径一条都没走到。没有已部署平台时，用一份合成的 OpenAPI 根目录补上覆盖——只需 `<root>\bin\artemis\application.properties`、`<root>\bin\artemis-web\artemis-web\config.properties`（portal 同理）与各组件的存在标记文件，再加同级的 `OpenAPI\nodejs\node-*\node.exe`，然后：

```bat
RepairArtemisWeb-rs.exe  --check-only --root <合成根> --nginx-root tests\fixtures\nginx-remote\nginx > out-rs.txt  2>&1
RepairArtemisWeb-cpp.exe --check-only --root <合成根> --nginx-root tests\fixtures\nginx-remote\nginx > out-cpp.txt 2>&1
```

`--nginx-root` 要指到含 `conf\` 的那一层（夹具是 `tests\fixtures\nginx-remote\nginx`），传错外层目录会被判为参数错误、退出码 2。

## 素材致谢

程序图标（`assets/app.ico`，六档尺寸）取自 [ico5.net](https://www.ico5.net/) 提供的免费图标集，仅用于本工具的桌面/任务栏标识。

## 仓库镜像

- GitHub：https://github.com/xiaoyuzhi/RepairArtemisWeb
- Gitee：https://gitee.com/lovemun/repair-artemis-web

## 许可证 (License)

本项目采用 **MIT License** 开源，完整条款见 [LICENSE](LICENSE) 文件。

MIT 是一种宽松许可证：允许任何人自由使用、复制、修改、合并、发布、分发、再许可及销售本软件的副本，仅需在所有副本或实质性部分中保留版权声明与本许可声明。软件按"原样"提供，不附带任何明示或默示的担保（包括但不限于适销性、特定用途适用性及非侵权性），作者在任何情况下均不对因使用本软件而产生的索赔、损害或其他责任负责。

## 免责声明

本工具为第三方运维辅助工具，与海康威视官方无任何关联。请先在测试/维护窗口内验证后使用，本工具不对误操作导致的平台异常负责。
