# Media Proxy RS

> 重制版的媒体代理 for Misskey

## 使用配置

### 拉取容器镜像

可以从 ghcr 拉取容器镜像

测试版本（跟随 master 主分支更新）：

```shell
docker pull ghcr.io/nyaone/media-proxy-rs:master
```

暂时没有稳定发布版本

### 环境变量

可以使用容器提供的默认值，也可以自己调整

- `RUST_LOG` 日志等级，容器模式默认 `error`
- `LISTEN` 监听的地址和端口，默认是 `0.0.0.0:3000` （容器监听来自外部请求的 3000 端口）
- `SIZE_LIMIT` 处理文件的大小限制，超过这个大小限制的会被直接重定向而非代理，单位是 Byte ，默认是 100M `100000000`

## 待办事项

- 完成 badge 模式下的图片处理
