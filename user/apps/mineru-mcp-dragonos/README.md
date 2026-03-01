# mineru-mcp (Rust)

该项目在 **Rust + Tokio** 上实现 MinerU MCP stdio server，功能对齐官方 `mineru-mcp` 的 `parse_documents` 与 `get_ocr_languages`。

## 功能

- MCP stdio server（基于 `rmcp`）
- 支持 URL 与本地文件解析
- 可选择远程 MinerU API 或本地部署 API
- 自动下载解析结果 zip、解压并读取 Markdown

## 环境变量

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `MINERU_API_BASE` | `https://mineru.net` | 远程 MinerU API 基址 |
| `MINERU_API_KEY` | (必填，远程模式) | 远程 API Key |
| `OUTPUT_DIR` | `./downloads` | 解压输出目录 |
| `USE_LOCAL_API` | `false` | 是否启用本地 API |
| `LOCAL_MINERU_API_BASE` | `http://localhost:8080` | 本地 API 基址 |
| `MINERU_POLL_INTERVAL_SECS` | `2` | 轮询间隔（秒） |
| `MINERU_MAX_WAIT_SECS` | `600` | 最大等待时间（秒） |

## 运行

```bash
cd Availiable_Mcp/mineru-mcp
export MINERU_API_KEY=your-api-key
cargo run
```

默认通过 stdio transport 提供 MCP 服务，可直接被 MCP client 启动/托管。

### MCP Client 对接示例（Claude Desktop）

```json
{
  "mcpServers": {
    "mineru": {
      "command": "cargo",
      "args": ["run", "--manifest-path", "Availiable_Mcp/mineru-mcp/Cargo.toml"],
      "env": {
        "MINERU_API_KEY": "your-api-key"
      }
    }
  }
}
```

## 工具

### parse_documents

入参：

- `file_sources`: 以逗号/空格/换行分隔的 URL 或本地路径
- `enable_ocr`: 是否启用 OCR（默认 `false`）
- `language`: 语言（默认 `ch`）
- `page_ranges`: 页码范围（可选）

返回：与官方 Python `mineru-mcp` 保持一致的 JSON 结构（单结果或批量结果）。

### get_ocr_languages

返回 OCR 语言列表，并附带 PaddleOCR 多语言支持链接。

## 测试

```bash
cargo test
```
