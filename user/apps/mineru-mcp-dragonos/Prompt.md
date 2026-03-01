你是资深 Rust 工程师。请在一个全新的 Cargo 项目中实现一个 MCP stdio server（用于 Dragon S 环境），功能等价于 “mineru-mcp”。

硬性要求
- 语言：Rust（tokio async）
- MCP SDK：使用官方 Rust SDK rmcp，stdio transport
- 提供两个 tools：
  1) parse_documents
  2) get_ocr_languages
- 代码需可编译、可运行、可测试：提供单元/集成测试（不依赖真实 MinerU 线上服务和真实 API key）

MCP 工具规格
1) parse_documents
- 入参（用 JSON schema 暴露给 MCP）：
  - file_sources: string （一个或多个来源，逗号/空格/换行分隔；每个来源要么是 URL，要么是本地文件路径）
  - enable_ocr: bool = false
  - language: string = "ch"
  - page_ranges: string? （仅远程 URL/远程上传模式支持）
- 行为：
  - 解析 file_sources，分成 urls 与 local_paths
  - 如果 USE_LOCAL_API=true：忽略 urls，只处理 local_paths（行为需与 mineru-mcp 一致）
  - 如果 USE_LOCAL_API=false：同时处理 urls 与 local_paths
  - 对每个 source 执行 MinerU 解析链路（见“MinerU API 规格”）
  - 下载 full_zip_url 的 zip，解压到 OUTPUT_DIR 下独立目录
  - 在解压目录中递归查找 md（优先：与输入文件名同名；否则第一个 .md），读取内容
- 返回值（结构化 JSON，便于测试）：
  {
    "results": [
      {
        "source": "...",
        "mode": "remote_url|remote_upload|local_api",
        "task_id": "...?" ,
        "batch_id": "...?" ,
        "markdown": "...",
        "output_dir": "...",
        "assets": ["images/xxx.jpg", ...]
      }
    ]
  }
返回值要求（强兼容模式）：parse_documents 必须返回 JSON，且 JSON 的字段结构必须与官方 Python mineru-mcp 一致。请先阅读找到 parse_documents 的返回值结构（字段名/层级/类型），在 Rust 中用 serde 定义对应 struct 并严格序列化一致；测试用例需断言返回 JSON 的字段结构与样例一致。

1) get_ocr_languages
- 返回 MinerU 支持的 OCR 语言列表（至少包含 ch/en 等常用项），并附上 PaddleOCR 多语言列表链接：
  https://www.paddleocr.ai/latest/version3.x/algorithm/PP-OCRv5/PP-OCRv5_multi_languages.html

环境变量（需实现读取与默认值）
- MINERU_API_BASE 默认 https://mineru.net
- MINERU_API_KEY 必填（远程模式下）
- OUTPUT_DIR 默认 ./downloads
- USE_LOCAL_API 默认 false
- LOCAL_MINERU_API_BASE 默认 http://localhost:8080

MinerU API 规格（远程）
A) URL 模式
- POST {MINERU_API_BASE}/api/v4/extract/task
  body: { url, model_version, is_ocr?, enable_formula?, enable_table?, language?, page_ranges? }
- GET  {MINERU_API_BASE}/api/v4/extract/task/{task_id}
  轮询直到 state=done，取 data.full_zip_url

B) 本地文件上传模式
- POST {MINERU_API_BASE}/api/v4/file-urls/batch
  body: { files:[{name,data_id?,is_ocr?,page_ranges?}], model_version, enable_formula?, enable_table?, language? }
  -> 返回 batch_id + file_urls[]
- PUT file_urls[i] 上传文件 bytes
- GET {MINERU_API_BASE}/api/v4/extract-results/batch/{batch_id}
  轮询每个文件直到 state=done，取 full_zip_url

实现建议
- HTTP client 用 reqwest；加 timeout、重试（对 5xx/网络错误），轮询间隔可配置（默认 2s），最大等待 10min
- zip 解压用 zip crate；目录操作用 std::fs / walkdir
- 日志用 tracing

测试要求（关键）
- 使用 wiremock/httpmock 在测试里模拟 MinerU API：
  - mock POST /api/v4/extract/task -> 返回 task_id
  - mock GET /api/v4/extract/task/{id} -> 先返回 running 再返回 done + full_zip_url
  - mock GET full_zip_url -> 返回你在测试里动态生成的 zip（包含 1 个 markdown 文件和 images/ 目录）
- 测试 parse_documents 能正确：
  - 解析多个 file_sources
  - 轮询并下载 zip
  - 解压并找到 md
  - 返回结构化 results，markdown 字段匹配预期
- 提供 README：如何设置 env、如何 cargo run、如何与 MCP client 对接

交付物
- Cargo.toml / src/main.rs（或模块化）
- tests/…（可直接 cargo test）
- README.md

-----
参考的仓库：[mineru-mcp](https://github.com/linxule/mineru-mcp?tab=readme-ov-file)