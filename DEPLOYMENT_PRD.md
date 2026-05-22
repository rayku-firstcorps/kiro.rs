# kiro-rs 运维部署 PRD

## 1. 背景

kiro-rs 是一个兼容 Anthropic Claude API 的 Kiro API 代理服务。当前部署目标是使用 Docker Compose 启动单实例服务，并通过环境变量配置客户端访问密钥和管理端登录密钥。

本 PRD 面向运维部署和交付，不包含业务功能开发。

## 2. 部署目标

- 使用 Docker Compose 一键部署 kiro-rs 服务。
- 对外暴露 `8990` 端口。
- 支持通过环境变量设置：
  - `APP_API_KEY`：客户端调用 API 使用的密钥。
  - `ADMIN_API_KEY`：Web 管理端和 Admin API 使用的登录密钥。
- 挂载本地 `config/` 目录，保存运行配置、凭据文件和运行缓存。
- 默认提供一条禁用状态的假凭据，便于管理页面初始化展示。

## 3. 部署范围

本次部署包含：

- 后端代理服务。
- Web 管理页面。
- 本地配置目录挂载。
- Docker 镜像本地构建。

本次部署不包含：

- 数据库。
- Redis。
- 多实例高可用。
- HTTPS 证书终止。
- 监控告警系统接入。
- 全局代理的 Web 配置功能。

## 4. 运行环境要求

### 4.1 主机要求

- Linux / Windows Server / Windows Docker Desktop 环境均可。
- 已安装 Docker。
- 已安装 Docker Compose v2。
- 主机可访问构建依赖源：
  - npm/pnpm 依赖源。
  - crates.io 或可用的 Rust crate 镜像源。

### 4.2 端口要求

服务默认监听：

```text
8990
```

部署前确认端口未被占用。

### 4.3 目录要求

项目根目录必须包含：

```text
docker-compose.yml
Dockerfile
config/
src/
admin-ui/
```

运行配置和凭据文件位于：

```text
config/config.json
config/credentials.json
```

## 5. 配置说明

### 5.1 环境变量

| 变量名 | 必填 | 默认值 | 说明 |
|---|---:|---|---|
| `APP_API_KEY` | 否 | `sk-kiro-rs-qazWSXedcRFV123456` | 客户端调用代理 API 的认证密钥 |
| `ADMIN_API_KEY` | 否 | `sk-admin-your-secret-key` | Web 管理端和 Admin API 的认证密钥 |

生产环境必须显式设置这两个变量，不建议使用默认值。

### 5.2 配置文件生成机制

当前 `docker-compose.yml` 会在容器启动时根据环境变量生成：

```text
/app/config/config.json
```

其中：

```json
"apiKey": "${APP_API_KEY}",
"adminApiKey": "${ADMIN_API_KEY}"
```

注意：由于 `config/` 是宿主机挂载目录，容器启动后会覆盖宿主机上的 `config/config.json`。

### 5.3 凭据文件

当前默认凭据文件：

```text
config/credentials.json
```

包含一条禁用的假凭据：

```json
[
  {
    "id": 1,
    "authMethod": "api_key",
    "kiroApiKey": "ksk_fake_credential_for_local_ui_testing_only",
    "email": "fake@example.local",
    "subscriptionTitle": "FAKE TEST CREDENTIAL",
    "priority": 0,
    "disabled": true,
    "endpoint": "ide"
  }
]
```

该凭据仅用于页面展示和初始化，不可用于真实上游调用。

真实部署时，运维可通过 Web 管理端添加真实凭据，或直接替换 `config/credentials.json`。

## 6. 部署步骤

### 6.1 拉取代码

```powershell
git clone <repo-url>
cd kiro.rs
```

如果已经存在项目目录，进入项目根目录即可。

### 6.2 设置密钥

PowerShell 示例：

```powershell
$env:APP_API_KEY="sk-your-client-api-key"
$env:ADMIN_API_KEY="sk-your-admin-api-key"
```

Linux shell 示例：

```bash
export APP_API_KEY="sk-your-client-api-key"
export ADMIN_API_KEY="sk-your-admin-api-key"
```

### 6.3 构建并启动

```powershell
docker compose up -d --build
```

### 6.4 查看容器状态

```powershell
docker compose ps
```

预期状态：

```text
kiro-rs   Up
```

### 6.5 查看日志

```powershell
docker logs --tail=120 kiro-rs
```

预期日志应包含服务监听 `8990` 端口的信息。

## 7. 访问方式

### 7.1 Web 管理端

浏览器访问：

```text
http://<服务器IP>:8990
```

登录密钥使用：

```text
ADMIN_API_KEY
```

### 7.2 Anthropic 兼容 API

客户端 Base URL：

```text
http://<服务器IP>:8990
```

客户端认证密钥使用：

```text
APP_API_KEY
```

## 8. 验证清单

部署完成后按以下顺序验证：

1. 容器状态为 `Up`。
2. `docker logs --tail=120 kiro-rs` 无启动失败日志。
3. 浏览器可打开 `http://<服务器IP>:8990`。
4. 使用 `ADMIN_API_KEY` 可登录管理页面。
5. 管理页面可看到默认假凭据，状态为禁用。
6. 添加真实凭据后，凭据列表可正常展示。
7. 使用 `APP_API_KEY` 发起客户端请求时认证通过。

## 9. 日常运维

### 9.1 重启服务

```powershell
docker compose restart kiro-rs
```

### 9.2 停止服务

```powershell
docker compose down
```

### 9.3 更新镜像并重启

```powershell
docker compose up -d --build
```

### 9.4 查看配置文件

```powershell
Get-Content .\config\config.json
```

### 9.5 查看凭据文件

```powershell
Get-Content .\config\credentials.json
```

## 10. 变更和回滚

### 10.1 修改 API Key

重新设置环境变量后重启：

```powershell
$env:APP_API_KEY="sk-new-client-api-key"
$env:ADMIN_API_KEY="sk-new-admin-api-key"
docker compose up -d
```

容器启动时会重新生成 `config/config.json`。

### 10.2 回滚到上一个镜像

如果使用本地构建镜像，建议在生产发布前给镜像打版本标签：

```powershell
docker tag kiro-rs:local kiro-rs:backup-YYYYMMDDHHmm
```

回滚时修改 `docker-compose.yml` 中的镜像名，或重新打回 `kiro-rs:local`。

### 10.3 凭据回滚

部署前备份：

```powershell
Copy-Item .\config\credentials.json .\config\credentials.json.bak
```

回滚：

```powershell
Copy-Item .\config\credentials.json.bak .\config\credentials.json -Force
docker compose restart kiro-rs
```

## 11. 安全要求

- 生产环境必须修改默认 `APP_API_KEY` 和 `ADMIN_API_KEY`。
- 不要将真实 `config/credentials.json` 提交到 Git。
- 不要在公开日志、截图、工单中暴露 API Key。
- 管理端口 `8990` 建议只对可信网络开放。
- 如需公网访问，建议在前置网关或反向代理上配置 HTTPS 和访问控制。

## 12. 常见问题

### 12.1 管理页面登录失败

检查当前容器环境变量：

```powershell
docker compose config
```

确认 `ADMIN_API_KEY` 是否为预期值。

### 12.2 客户端请求返回认证失败

确认客户端使用的是 `APP_API_KEY`，不是 `ADMIN_API_KEY`。

### 12.3 修改 config.json 后重启被覆盖

这是当前部署设计：容器启动时会根据 `docker-compose.yml` 的环境变量重新生成 `config/config.json`。

需要修改持久配置时，应优先修改 `docker-compose.yml` 中的启动模板或环境变量。

### 12.4 需要配置代理

当前部署模板默认不启用全局代理：

```json
"proxyUrl": null
```

如果需要代理，可修改 `docker-compose.yml` 中生成 `config.json` 的模板，例如：

```json
"proxyUrl": "http://host.docker.internal:7890"
```

然后重启：

```powershell
docker compose up -d
```

## 13. 验收标准

部署验收通过条件：

- Docker Compose 可成功解析。
- 容器可启动并保持运行。
- Web 管理端可访问。
- `ADMIN_API_KEY` 可用于登录。
- `APP_API_KEY` 可用于客户端认证。
- 默认假凭据存在且处于禁用状态。
- 服务重启后配置仍由环境变量正确生成。
