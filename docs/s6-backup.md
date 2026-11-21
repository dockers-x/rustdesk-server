# S6 离线备份与恢复

备份入口为 S6 镜像中的 `/usr/bin/rustdesk-s6-backup`。适用于 HBBS 与 Console 都使用默认路径上的 SQLite 数据库的部署。执行前停止所有可能写入这两套数据库、配置及密钥的服务和其他容器。`--offline` 表明已完成停机确认。请用单次运行的容器执行命令，避免启动 S6 服务。

## 备份

归档包含 HBBS 的 `db_v2.sqlite3`、`id_ed25519` 密钥对及可选的 `/data/.env`，以及 Console 的 `rustdeskapi.db`、`config.yaml` 和可选的自有签名密钥文件 `features/self-signed-client.json`。工具通过 SQLite 生成两份独立快照，检查可读性，并将程序和架构版本、UTC 时间和 SHA-256 校验值写入归档。归档权限为 `0600`，含私钥，请保存在受限目录。

停机前运行 `docker compose exec -T rustdesk /app/rustdesk-console database status --json`，确认 Console 的 `database_type` 为 `sqlite`。若生产环境设置了环境变量覆盖，请用同一份受保护的环境配置核验数据库后端。MySQL/MariaDB 和 PostgreSQL 使用各自的原生备份工具。使用自定义 `DB_URL` 的 HBBS 部署需从实际数据库位置单独备份。

以下示例使用现有 Compose 服务 `rustdesk` 和标准数据挂载 `/data`、`/app/data`。先检查生产容器的实际挂载，确认两个目录均持久化。

```sh
set -eu
umask 077
mkdir -p "$PWD/backup" "$PWD/backup-source/config"
container=$(docker compose ps --all --quiet rustdesk)
image=$(docker inspect --format '{{.Image}}' "$container")
docker compose stop -t 60 rustdesk
test "$(docker inspect --format '{{.State.Running}}' "$container")" = false
docker cp "$container:/app/conf/." "$PWD/backup-source/config/"
docker run --rm --network none --volumes-from "$container:ro" \
  --mount "type=bind,src=$PWD/backup-source/config,dst=/saved-config,readonly" \
  --mount "type=bind,src=$PWD/backup,dst=/backup" \
  --entrypoint /usr/bin/rustdesk-s6-backup "$image" \
  backup --offline /backup/rustdesk-2026-11-21.tar /data /app/data /saved-config
```

备份完成后可重启原服务。`backup-source/config` 是暂存的配置副本，按密钥材料保管。整个归档中只保存上述固定文件。如有以下数据，请随同该备份保留独立副本及对应时间点。

- Compose 文件、镜像引用、环境变量、Docker Secrets、TLS 证书、自定义配置引用文件及外部签名材料。
- 本地录屏文件及录屏目录的挂载信息。
- 外部对象存储数据、版本和访问配置。数据库快照包含已登记的录屏元数据。

## 恢复与验证

在隔离环境中选用备份时的 S6 镜像。任何仍在写入目标数据位置的进程都须停止。恢复命令要求目标目录不存在，验证归档成员、SHA-256 和两套 SQLite 数据库，并写入新目录。

```sh
set -eu
umask 077
mkdir -p "$PWD/restore"
docker run --rm --network none \
  --mount "type=bind,src=$PWD/backup,dst=/backup,readonly" \
  --mount "type=bind,src=$PWD/restore,dst=/restore" \
  --entrypoint /usr/bin/rustdesk-s6-backup "$image" \
  restore --offline /backup/rustdesk-2026-11-21.tar /restore/recovered
```

将 `recovered/server` 挂载到 `/data`，将 `recovered/console` 挂载到 `/app/data`，并将已补齐引用文件的 `recovered/config` 挂载到 `/app/conf`。按原部署恢复外部环境和 Secrets。确认 Console 的公钥路径指向恢复后的 HBBS 公钥。自定义运行账号的部署需在启动前调整目录与文件的 UID/GID，并保持目录 `0700` 和敏感文件 `0600`。

对照 `manifest.txt` 核对程序和 Console 架构版本。对照 `SHA256SUMS` 及生产侧记录比较 `server/id_ed25519.pub` 的 SHA-256。启动隔离实例，检查 Console 登录、地址簿及自有签名状态，再用测试客户端验证经 HBBS 注册和经 HBBR 中继。确认后切换正式服务的挂载，保留旧数据库、边车文件、配置和密钥所在的整套目录供回滚。

## 将离线归档发送到多个远程目标

整套归档完成后，可用同一镜像中的 Console 命令推送到 S3 和 WebDAV。
将远程凭据放在独立文件，并在主配置中通过 `backup-config-file` 引用。
离线采集只读取主配置。执行上传时再挂载远程凭据文件。主配置和密钥保留在
私有卷。远程目标须限制访问，因为整套归档含数据库和服务密钥。

```sh
/app/rustdesk-console -c /app/conf/config.yaml database upload-backup /backup/rustdesk-2026-11-21.tar --json
```

命令独立上传每个目标并输出逐项结果。任何单项失败会使最终退出码为 4。
可从宿主机在离线备份完成后定时调用。Console 诊断页的一键及间隔备份适用于
运行中的 Console SQLite，可与这套 S6 离线流程共用目标配置。
