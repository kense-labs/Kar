# Kar 工作台认证

外部 MCP 客户端向工作台根地址发现 RFC 9728 资源元数据和授权服务元数据。注册、授权、同意、拒绝、兑换及撤销接口位于 /oauth/mcp。浏览器直接打开授权地址即可看到客户端自报名称、工作台地址、实际回调和选定访问级别。一次性 Kar 配对码或拥有相应权限的浏览器会话可以授权。回调可以使用 HTTPS 或环回 HTTP。PKCE 使用 S256，授权码有效期一分钟，每次兑换尝试都会消耗有效的已签发码。

工作台直接从绑定的环回地址确定 issuer。通过 HTTPS 转发访问时在 Kar 服务进程设置 KAR_PUBLIC_ORIGIN 为稳定的外部 origin，例如 https://kar.example.org。授权响应、访问令牌资源绑定及 DPoP 的 htu 使用该值。环境令牌传输和回调都应使用该对外地址。重新启动时保持相同 origin 和 state 即可继续已有授权。

## 环境会话

宿主可调用 POST /api/auth/pairing-token，指定 scopes，或用 access 生成一个 MCP 授权码。POST /api/auth/browser-session 用一次性 credential 换取 HttpOnly Cookie。GET /api/auth/session 显示会话状态。POST /oauth/token 接受 RFC 8693 表单，bootstrap 类型为 urn:kar:params:oauth:token-type:environment-bootstrap。可用 ES256 DPoP 将令牌绑定到客户端 P-256 密钥，续期需使用同一密钥，并轮换旧令牌。工作台 REST、宿主管理、本地模型工具和外部 MCP 保留各自的凭据检查。缩小范围的环境会话在未分类的操作上返回 403。

POST /api/auth/websocket-ticket/rpc 签发用于聊天事件、Git 事件、终端输出或预览配置报告的票据。POST /api/auth/websocket-ticket/media 针对设备画面和预览隧道。传输参数为 wsTicket。RPC 票据首次使用后消费，绑定密钥的 RPC 票据还需请求证明。媒体票据在一分钟有效期内可以重复使用。旧式无用途路径可为这两类资源签发短时票据。客户端列表、单独撤销、撤销其它客户端位于 /api/auth/clients。撤销可结束已建立的事件流，并使尚未用的票据失效。

## 复验

运行 cargo test --locked --test workbench_oauth。测试启动本地 Kar 二进制，经 HTTP 完成注册、同意与拒绝、回调和 PKCE、凭据隔离、真实 P-256 签名校验、用途票据、服务重启和撤销。测试只需要本机，协议参与方由测试扮演。统一验收前本批仅进行 macOS all-targets 编译。

固定工作台源码 2808e878de91aacc7fb82887f18414976be7ae34 的对应位置为 apps/server/src/auth/McpOAuth.ts、mcpOAuthHttp.ts、http.ts、EnvironmentAuth.ts、packages/contracts/src/auth.ts、packages/shared/src/dpop.ts。
