# RepairArtemisWeb

> iSecure VMS OpenAPI `artemis-web` / `artemis-portal` 组件修复工具
> （Rust / C++ 双版本，单文件免依赖，中文界面）

当海康 iSecure VMS（综合安防管理平台）的 OpenAPI 组件 `artemis-web`（端口 9017）与 `artemis-portal`（端口 9018）服务**未运行、启动失败或未安装**时，本工具可一键诊断并自动修复，是运维人员的"服务急救包"。

- 开发人：余志强　QQ: 379008610　主页：https://github.com/xiaoyuzhi
- 版权：Copyright (c) 2026 余志强 (Yu Zhiqiang)
- 许可：本项目基于 [MIT License](LICENSE) 开源

---

## 功能

复刻自 `Repair-ArtemisWeb.ps1`，独立打包为 exe：

1. **定位** OpenAPI 安装目录与内置 `node.exe`（默认标准路径 `C:\Program Files (x86)\iSecure VMS\VSM Servers\OpenAPI\artemis`，可用 `--root` 覆盖）；
2. **检查前置组件**：artemis 网关(9016) / redis / postgresql / minio / nginx 是否就绪；
3. **逐个修复** `artemis-web` / `artemis-portal`：

   | 状态 | 处理 |
   | --- | --- |
   | 服务运行中 | 跳过（除非 `--reinstall`） |
   | 服务停止 | 直接启动，端口起来即完成 |
   | 未安装 / 启动失败 / 端口未起 | 卸载(`service.uninstall.js`) → 重装(`service.install.js`) → 启动 |

4. **等待端口监听 + HTTP 健康检查**，输出修复汇总。

修复完成后可通过以下地址验证：

```
本机:   http://127.0.0.1:9017/artemis-web/
        http://127.0.0.1:9018/artemis-portal/
对外:   https://<本机IP>/artemis-web/    (经平台 nginx 443 对外)
        https://<本机IP>/artemis-portal/
```

## 特性

- **零第三方依赖**：Rust 版仅用标准库 + 少量 Win32 FFI；C++ 版静态编译，均为**单文件免运行库**，拷走即用；
- **双模式运行**：带参数 = 命令行模式；不带参数 = 中文交互式菜单（回车默认标准修复）；
- **三档操作强度**：标准修复 / 仅检查（只读）/ 强制重装；
- 分级彩色日志、管理员检测、控制台 UTF-8 自适应（中文不乱码）。

## 运行环境

| 项 | 要求 |
| --- | --- |
| 系统 | Windows 7+（x64） |
| 权限 | 标准修复 / 强制重装需**管理员**；仅检查无需 |
| 对象平台 | 已部署 iSecure VMS 的服务器（本机为平台或平台节点） |

> 在 iSecure VMS 服务器上，请右键"以管理员身份运行"。

## 使用说明

### 交互式菜单（双击运行即可）

```
============================================================
  RepairArtemisWeb — iSecure VMS OpenAPI 组件修复工具
============================================================
  [1] 标准修复  自动修复未运行的 artemis-web / artemis-portal 服务
  [2] 仅检查    只诊断不修改 (无需管理员权限)
  [3] 强制重装  即使服务运行中, 也卸载重装 (需要管理员权限)
  [0] 退出
  请输入序号并回车 (直接回车默认 1):
```

### 命令行参数

```
RepairArtemisWeb.exe [选项]
  -c, --check-only  仅诊断, 不做任何修改 (无需管理员权限)
  -r, --reinstall   强制卸载并重装服务 (即使服务正在运行)
  --root <目录>     指定 OpenAPI 根目录 (默认标准安装路径自动定位)
  -h, --help        显示帮助
```

退出码：`0` = 完成且无异常；`1` = 存在异常项；`2` = 命令行参数错误。

## 目录结构

```
.
├── src/main.rs              # Rust 版全部源码
├── Cargo.toml               # Rust 工程 (零第三方依赖)
├── .cargo/config.toml       # 静态链接 MSVC CRT, 单文件免运行库
├── cpp/
│   ├── RepairArtemisWeb.cpp # C++ 版全部源码
│   └── build.bat            # C++ 一键编译脚本
└── .gitignore               # 排除 target/ 与 *.exe 构建产物
```

## 从源码构建

### Rust 版

```powershell
cargo build --release
# 产物: target\release\RepairArtemisWeb.exe  (单文件, 免 VC 运行库)
```

### C++ 版

前置：安装 [MSYS2](https://www.msys2.org/) 的 `mingw-w64-x86_64-gcc`。

```bat
rem 双击或在命令行执行
cpp\build.bat
rem 产物: cpp\RepairArtemisWeb.exe  (单文件, 免运行库)
```

> 两个版本功能完全一致，可任选其一部署；仓库内不包含编译产物。

## 仓库镜像

- GitHub：https://github.com/xiaoyuzhi/RepairArtemisWeb
- Gitee：https://gitee.com/lovemun/repair-artemis-web

## 许可证 (License)

本项目采用 **MIT License** 开源，完整条款见 [LICENSE](LICENSE) 文件。

MIT 是一种宽松许可证：允许任何人自由使用、复制、修改、合并、发布、分发、再许可及销售本软件的副本，仅需在所有副本或实质性部分中保留版权声明与本许可声明。软件按"原样"提供，不附带任何明示或默示的担保（包括但不限于适销性、特定用途适用性及非侵权性），作者在任何情况下均不对因使用本软件而产生的索赔、损害或其他责任负责。

## 免责声明

本工具为第三方运维辅助工具，与海康威视官方无任何关联。请先在测试/维护窗口内验证后使用，本工具不对误操作导致的平台异常负责。
