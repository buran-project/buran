<p align="center">
  <img src=".github/assets/cover.jpg" alt="Buran Application Server">
</p>

# 🚀 Buran Application Server

[English](README.md) · [Русский](README.ru.md) · **中文**

[![Project Status: Active](https://www.repostatus.org/badges/latest/active.svg)](https://www.repostatus.org/#active)
[![Tests](https://github.com/buran-project/buran/actions/workflows/tests.yml/badge.svg)](https://github.com/buran-project/buran/actions/workflows/tests.yml)
[![Security Scan](https://github.com/buran-project/buran/actions/workflows/scan.yml/badge.svg)](https://github.com/buran-project/buran/actions/workflows/scan.yml)
[![Release](https://github.com/buran-project/buran/actions/workflows/release.yml/badge.svg)](https://github.com/buran-project/buran/actions/workflows/release.yml)
[![Rust](https://img.shields.io/badge/Rust-1.88+-CE422B?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![Container](https://img.shields.io/badge/ghcr.io-buran--project%2Fburan-2496ED?logo=docker&logoColor=white)](https://github.com/buran-project/buran/pkgs/container/buran)
[![License](https://img.shields.io/github/license/buran-project/buran)](LICENSE)

**Buran** 是一个用 Rust 编写的应用服务器。它负责提供静态文件、路由请求，并在一组
内嵌的工作进程池中运行你的应用代码——整个入口层仅由一份声明式 YAML 配置描述。

其核心与语言无关：各语言运行时以独立的可插拔模块形式接入，而非编译进服务器。
**PHP 是首个受支持的运行时**，后续还会加入更多。

如果你用过 **nginx + FastCGI** 或 **NGINX Unit**，会觉得这套模型很熟悉：网络与
路由在同一个进程中，工作进程执行应用代码，两者之间没有额外的守护进程。

## ✨ 为什么选择 Buran？

- 📦 **一份配置，一个入口** — 路由器、静态文件处理和进程监督都在同一个原生服务器
  二进制中；监听器、路由和应用全部写在一份 YAML 文件里。
- 🔌 **可插拔运行时** — 运行时以独立的模块二进制 `buran-<runtime>` 接入，因此多个
  版本可以并存运行（`buran-php83`、`buran-php84`……），新增语言无需改动核心。
- ⚡ **进程内执行** — 工作进程直接在服务器的监督下执行应用代码——没有 FastCGI
  跳转，也没有外部的进程管理器。
- 🛡️ **默认安全** — 被运行时声明为可执行的源文件**永远不会**作为静态内容提供，
  即便某条规则本会匹配到它们。
- 🧊 **静态配置** — 没有实时管理 API。修改配置意味着 reload/restart（在容器中则
  是新建容器）——运行状态保持可预测、可审计。
- 🐳 **容器原生** — 作为 PID 1 正确运行（回收孤儿进程，按 `SIGTERM`/`SIGINT`
  优雅关闭）。官方运行时镜像按版本发布。

## 🧩 受支持的语言

运行时以独立模块接入，因此这个列表可以在不改动核心的情况下扩展。目前可用：

| 语言 | 镜像风味 | 状态 |
|------|----------|------|
| [![PHP](https://img.shields.io/badge/PHP-7.3_–_8.5-777BB4?logo=php&logoColor=white)](https://www.php.net/) | Debian、Alpine | ✅ 已支持 |

多个版本可以并存（`buran-php83`、`buran-php84`……），因此一个镜像即可服务锁定在
不同分支的应用。

## 📋 前置条件

选择你的方式：

- **Docker** — 只需要一个容器运行时。官方镜像已内置 Buran、一个 PHP 模块和
  opcache。
- **从源码构建** — Rust **1.88+**。PHP 模块另需一个以 `--enable-embed` 构建的
  `libphp`，外加 `php-config` 和 C 工具链（核心服务器本身**不**依赖 PHP）。

## ⚡ 快速开始

### 1. 编写配置

创建 `buran.yaml`——先提供静态文件，回退到 PHP 前端控制器：

```yaml
settings:
  modules: /usr/lib/buran/modules   # 镜像安装运行时模块的位置

listeners:
  "*:8080":
    route: main

routes:
  main:
    - action:
        share: /www$uri            # 先尝试真实文件
        fallback:
          application: app         # 其余全部 → PHP

applications:
  app:
    module: php85                  # → /usr/lib/buran/modules/buran-php85
    root: /www
    index: index.php
    processes: 2
```

### 2. 运行 🐳

```bash
docker run --rm -p 8080:8080 \
  -v "$PWD/buran.yaml:/etc/buran/buran.yaml:ro" \
  -v "$PWD/public:/www:ro" \
  ghcr.io/buran-project/buran:php
```

打开 <http://localhost:8080>。完成。🎉

不确定该用哪个模块名？列出镜像自带的模块：

```bash
docker run --rm ghcr.io/buran-project/buran:php buran --modules
```

### 3. …或从源码构建 🦀

```bash
git clone https://github.com/buran-project/buran
cd buran

# 核心服务器（无需 PHP 工具链）：
cargo run -p buran -- --config examples/buran.yaml
```

### 4. 部署前先校验 ✅

```bash
buran --check-config --config /etc/buran/buran.yaml
```

它会校验配置模式**并**逐一探测每个运行时模块的协议兼容性，遇到任何问题即以非零
退出码退出——可放心作为部署前的门禁。

## 🧭 工作原理

一个请求会流经四类配置对象：

```
   TCP :8080  ─►  listener  ─►  route  ─►  action  ─►  application
                (bind addr)   (match →    (share /     (运行时模块
                              action)     return /      + 工作进程池)
                                          app / route)
```

- **listener** 绑定一个 `host:port` 并进入某个路由（或提供内置的状态端点）。
- **route** 是一串有序的 `match → action` 步骤；第一个匹配者胜出。
- **action** 提供静态文件（`share`）、返回状态码（`return`）、跳转到另一个路由
  （`route`），或分派给某个 **application**。
- **application** 将一个运行时**模块**绑定到文档根目录和一个工作进程池。

## 📚 文档

完整文档位于 [`docs/`](docs/)：

| 文档 | 内容 |
|------|------|
| 📖 [Getting started](docs/getting-started.md) | 几分钟内用 Docker 或源码运行 Buran。 |
| ⚙️ [Configuration reference](docs/configuration.md) | `settings`、`listeners`、`access_log`、env 替换、状态端点。 |
| 🧭 [Routing](docs/routing.md) | 路由、`match` 条件、动作、模式语法、rewrite。 |
| 📁 [Static files](docs/static-files.md) | `share` 动作、index 文件、MIME 类型、源码泄露防护。 |
| 🧩 [Applications & runtimes](docs/applications.md) | 进程池、限制、PHP 模块、模块如何工作（BWP）。 |
| 🚢 [Deployment](docs/deployment.md) | Docker 镜像、发行版软件包、systemd、作为 PID 1 运行。 |
| 🖥️ [CLI reference](docs/cli.md) | 命令行参数、环境变量、信号。 |

## 🐳 容器镜像

官方镜像发布在 GitHub Container Registry：

| 镜像 | 内容 |
|------|------|
| `ghcr.io/buran-project/buran:php` | Buran + 最新 PHP 运行时模块 + opcache。 |
| `ghcr.io/buran-project/buran:php-alpine` | 同上，Alpine 风味。 |
| `ghcr.io/buran-project/buran:minimal` | 仅 Buran——静态文件与路由，无运行时模块。 |

PHP 分支 **7.3 → 8.5** 以矩阵方式构建，提供 Debian 和 Alpine 两种风味。预构建的
**发行版软件包**（Alpine / Debian / RPM）会附加到每个
[发布](https://github.com/buran-project/buran/releases)。完整列表与标签见
[Deployment](docs/deployment.md)。

## 🛠️ 开发

Buran 是一个 Rust workspace：

| Crate | 职责 |
|-------|------|
| `buran` | 主进程：CLI、配置加载、模块检查、进程监督。 |
| `buran-router` | HTTP/1.1、路由、rewrite、静态文件、分派、WebSocket。 |
| `buran-config` | 配置模式、校验、`${ENV}` 替换。 |
| `buran-ipc` | Buran Worker Protocol（BWP）：分帧与扁平化请求编码。 |
| `buran-worker` | 工作进程侧 SDK，用于构建运行时模块。 |
| `buran-php` | PHP 运行时模块：通过自定义 SAPI 内嵌 `libphp`。 |
| `buran-echo` | 参考用的事件循环模块（并发 BWP profile）。 |

```bash
cargo build            # 构建 workspace
cargo test             # 运行测试
cargo run -p buran -- --config examples/buran.yaml
```

## 📄 许可证

基于 [Apache License 2.0](LICENSE) 授权。

---

**仓库**：[github.com/buran-project/buran](https://github.com/buran-project/buran)
**容器镜像库**：[ghcr.io/buran-project/buran](https://github.com/buran-project/buran/pkgs/container/buran)

♥️ 欢迎提交 issue 和 pull request！
