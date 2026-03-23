use bytes::Bytes;
use rmcp::{
    ErrorData as McpError, Json, ServerHandler,
    handler::server::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig,
        StreamableHttpService,
        session::local::LocalSessionManager,
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    env,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;
use walkdir::WalkDir;

// HTTP 相关
use axum::{
    extract::State,
    response::Json as AxumJson,
    routing::get,
    Router,
};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone)]
pub struct Settings {
    mineru_api_base: String,
    mineru_api_key: Option<String>,
    output_dir: PathBuf,
    use_local_api: bool,
    local_mineru_api_base: String,
    poll_interval: Duration,
    max_wait: Duration,
}

impl Settings {
    pub fn from_env() -> Self {
        let mineru_api_base =
            env::var("MINERU_API_BASE").unwrap_or_else(|_| "https://mineru.net".to_string());
        let mineru_api_key = env::var("MINERU_API_KEY").ok().filter(|v| !v.is_empty());
        let output_dir = env::var("OUTPUT_DIR").unwrap_or_else(|_| "./downloads".to_string());
        let use_local_api = env::var("USE_LOCAL_API")
            .ok()
            .map(|value| matches!(value.to_lowercase().as_str(), "true" | "1" | "yes"))
            .unwrap_or(false);
        let local_mineru_api_base = env::var("LOCAL_MINERU_API_BASE")
            .unwrap_or_else(|_| "http://localhost:8080".to_string());
        let poll_interval = env::var("MINERU_POLL_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(2));
        let max_wait = env::var("MINERU_MAX_WAIT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(600));

        Self {
            mineru_api_base,
            mineru_api_key,
            output_dir: PathBuf::from(output_dir),
            use_local_api,
            local_mineru_api_base,
            poll_interval,
            max_wait,
        }
    }
}

#[derive(Debug, Error)]
pub enum MineruError {
    #[error("missing MINERU_API_KEY for remote requests")]
    MissingApiKey,
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("response error: {0}")]
    Response(String),
    #[error("timeout waiting for task completion")]
    Timeout,
    #[error("missing markdown content in output directory")]
    MissingMarkdown,
}

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    code: Option<i32>,
    data: Option<T>,
    msg: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TaskCreateData {
    task_id: String,
}

#[derive(Debug, Deserialize)]
struct TaskStatusData {
    state: String,
    full_zip_url: Option<String>,
    err_msg: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BatchCreateData {
    batch_id: String,
    file_urls: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct BatchStatusData {
    extract_result: Vec<BatchExtractResult>,
}

#[derive(Debug, Deserialize, Clone)]
struct BatchExtractResult {
    file_name: String,
    state: String,
    full_zip_url: Option<String>,
    err_msg: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ParseDocumentsParams {
    pub file_sources: String,
    #[serde(default)]
    pub enable_ocr: bool,
    #[serde(default = "default_language")]
    pub language: String,
    pub page_ranges: Option<String>,
}

fn default_language() -> String {
    "ch".to_string()
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ParseDocumentsResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extract_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub results: Option<Vec<ParseDocumentsResultItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<ParseDocumentsSummary>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ParseDocumentsSummary {
    pub total_files: usize,
    pub success_count: usize,
    pub error_count: usize,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema, Clone)]
pub struct ParseDocumentsResultItem {
    pub filename: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extract_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct LanguageResponse {
    pub status: String,
    pub languages: Vec<LanguageInfo>,
    pub link: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct LanguageInfo {
    pub name: String,
    pub description: String,
    pub code: String,
}

#[derive(Clone)]
pub struct MineruServer {
    settings: Settings,
    client: reqwest::Client,
    tool_router: ToolRouter<Self>,
    healthy: Arc<AtomicBool>,
}

#[derive(Debug, Serialize)]
pub struct HealthCheckResponse {
    pub status: String,
    pub server: String,
    pub timestamp: String,
    pub api_mode: String,
    pub api_base: String,
    pub has_api_key: bool,
    pub version: &'static str,
}

#[tool_router]
impl MineruServer {
    pub fn new(settings: Settings) -> Result<Self, MineruError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .no_proxy()
            .build()?;
        Ok(Self {
            settings,
            client,
            tool_router: Self::tool_router(),
            healthy: Arc::new(AtomicBool::new(true)),
        })
    }

    #[tool(description = "解析文档（支持本地文件和URL，自动读取内容）")]
    pub async fn parse_documents(
        &self,
        params: Parameters<ParseDocumentsParams>,
    ) -> Result<Json<ParseDocumentsResponse>, McpError> {
        let params = params.0;
        let mut sources = parse_sources(&params.file_sources);
        if sources.is_empty() {
            return Ok(Json(ParseDocumentsResponse {
                status: "error".to_string(),
                content: None,
                extract_path: None,
                error_message: Some("未提供有效的文件路径或URL".to_string()),
                message: None,
                results: None,
                summary: None,
            }));
        }

        let mut seen = HashSet::new();
        sources.retain(|item| seen.insert(item.to_lowercase()));

        let (url_paths, file_paths) = split_sources(&sources);

        let mut results = Vec::new();

        if self.settings.use_local_api {
            if file_paths.is_empty() {
                return Ok(Json(ParseDocumentsResponse {
                    status: "warning".to_string(),
                    content: None,
                    extract_path: None,
                    error_message: None,
                    message: Some(
                        "在本地API模式下，无法处理URL，且未提供有效的本地文件路径".to_string(),
                    ),
                    results: None,
                    summary: None,
                }));
            }

            info!("使用本地API处理 {} 个文件", file_paths.len());
            for path in file_paths {
                results.push(self.handle_local_file(&path, &params).await);
            }
        } else {
            if !url_paths.is_empty() {
                info!("使用远程API处理 {} 个URL", url_paths.len());
                for url in url_paths {
                    results.push(self.handle_remote_url(&url, &params).await);
                }
            }

            if !file_paths.is_empty() {
                info!("使用远程API处理 {} 个本地文件", file_paths.len());
                let batch_results = self.handle_remote_files(&file_paths, &params).await;
                results.extend(batch_results);
            }
        }

        if results.is_empty() {
            return Ok(Json(ParseDocumentsResponse {
                status: "error".to_string(),
                content: None,
                extract_path: None,
                error_message: Some("未处理任何文件".to_string()),
                message: None,
                results: None,
                summary: None,
            }));
        }

        if results.len() == 1 {
            let result = results.into_iter().next().expect("single result exists");
            return Ok(Json(ParseDocumentsResponse {
                status: result.status,
                content: result.content,
                extract_path: result.extract_path,
                error_message: result.error_message,
                message: None,
                results: None,
                summary: None,
            }));
        }

        let success_count = results
            .iter()
            .filter(|item| item.status == "success")
            .count();
        let error_count = results.iter().filter(|item| item.status == "error").count();
        let total_count = results.len();

        let overall_status = if success_count == 0 {
            "error"
        } else if error_count > 0 {
            "partial_success"
        } else {
            "success"
        };

        Ok(Json(ParseDocumentsResponse {
            status: overall_status.to_string(),
            content: None,
            extract_path: None,
            error_message: None,
            message: None,
            results: Some(results),
            summary: Some(ParseDocumentsSummary {
                total_files: total_count,
                success_count,
                error_count,
            }),
        }))
    }

    #[tool(description = "获取OCR支持的语言列表")]
    pub async fn get_ocr_languages(&self) -> Result<Json<LanguageResponse>, McpError> {
        let languages = vec![
            LanguageInfo {
                name: "中文".to_string(),
                description: "Chinese & English".to_string(),
                code: "ch".to_string(),
            },
            LanguageInfo {
                name: "英文".to_string(),
                description: "English".to_string(),
                code: "en".to_string(),
            },
            LanguageInfo {
                name: "日文".to_string(),
                description: "Japanese".to_string(),
                code: "japan".to_string(),
            },
            LanguageInfo {
                name: "韩文".to_string(),
                description: "Korean".to_string(),
                code: "korean".to_string(),
            },
            LanguageInfo {
                name: "法文".to_string(),
                description: "French".to_string(),
                code: "fr".to_string(),
            },
        ];

        Ok(Json(LanguageResponse {
            status: "success".to_string(),
            languages,
            link: "https://www.paddleocr.ai/latest/version3.x/algorithm/PP-OCRv5/PP-OCRv5_multi_languages.html"
                .to_string(),
        }))
    }
}

#[tool_handler]
impl ServerHandler for MineruServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "MCP server for MinerU document parsing: parse_documents and get_ocr_languages"
                    .to_string(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

impl MineruServer {
    async fn handle_remote_url(
        &self,
        url: &str,
        params: &ParseDocumentsParams,
    ) -> ParseDocumentsResultItem {
        let filename = source_filename(url, true);
        match self
            .process_remote_url(url, params)
            .await
            .map(|extracted| ParseDocumentsResultItem {
                filename: filename.clone(),
                source_url: Some(url.to_string()),
                source_path: None,
                status: "success".to_string(),
                content: Some(extracted.markdown),
                extract_path: Some(extracted.output_dir),
                error_message: None,
            }) {
            Ok(item) => item,
            Err(err) => {
                error!("处理URL失败: {err}");
                ParseDocumentsResultItem {
                    filename,
                    source_url: Some(url.to_string()),
                    source_path: None,
                    status: "error".to_string(),
                    content: None,
                    extract_path: None,
                    error_message: Some(err.to_string()),
                }
            }
        }
    }

    async fn handle_remote_files(
        &self,
        file_paths: &[String],
        params: &ParseDocumentsParams,
    ) -> Vec<ParseDocumentsResultItem> {
        let mut results = Vec::new();
        let mut existing_files = Vec::new();

        for path in file_paths {
            if Path::new(path).exists() {
                existing_files.push(path.clone());
            } else {
                results.push(ParseDocumentsResultItem {
                    filename: source_filename(path, false),
                    source_url: None,
                    source_path: Some(path.clone()),
                    status: "error".to_string(),
                    content: None,
                    extract_path: None,
                    error_message: Some(format!("文件不存在: {path}")),
                });
            }
        }

        if existing_files.is_empty() {
            return results;
        }

        match self.process_remote_files(&existing_files, params).await {
            Ok(processed) => {
                results.extend(processed);
            }
            Err(err) => {
                error!("处理本地文件失败: {err}");
                for path in existing_files {
                    results.push(ParseDocumentsResultItem {
                        filename: source_filename(&path, false),
                        source_url: None,
                        source_path: Some(path.clone()),
                        status: "error".to_string(),
                        content: None,
                        extract_path: None,
                        error_message: Some(err.to_string()),
                    });
                }
            }
        }

        results
    }

    async fn handle_local_file(
        &self,
        path: &str,
        params: &ParseDocumentsParams,
    ) -> ParseDocumentsResultItem {
        let filename = source_filename(path, false);
        if !Path::new(path).exists() {
            return ParseDocumentsResultItem {
                filename,
                source_url: None,
                source_path: Some(path.to_string()),
                status: "error".to_string(),
                content: None,
                extract_path: None,
                error_message: Some(format!("文件不存在: {path}")),
            };
        }

        match self.process_local_file(path, params).await {
            Ok(extracted) => ParseDocumentsResultItem {
                filename,
                source_url: None,
                source_path: Some(path.to_string()),
                status: "success".to_string(),
                content: Some(extracted.markdown),
                extract_path: Some(extracted.output_dir),
                error_message: None,
            },
            Err(err) => ParseDocumentsResultItem {
                filename,
                source_url: None,
                source_path: Some(path.to_string()),
                status: "error".to_string(),
                content: None,
                extract_path: None,
                error_message: Some(err.to_string()),
            },
        }
    }

    async fn process_remote_url(
        &self,
        url: &str,
        params: &ParseDocumentsParams,
    ) -> Result<ExtractedContent, MineruError> {
        let task_id = self
            .submit_url_task(&self.settings.mineru_api_base, url, params, true)
            .await?;
        let status = self
            .poll_task(&self.settings.mineru_api_base, &task_id, true)
            .await?;
        let full_zip_url = status
            .full_zip_url
            .ok_or_else(|| MineruError::Response("未返回 full_zip_url".to_string()))?;
        self.download_and_extract(&full_zip_url, url).await
    }

    async fn process_remote_files(
        &self,
        file_paths: &[String],
        params: &ParseDocumentsParams,
    ) -> Result<Vec<ParseDocumentsResultItem>, MineruError> {
        let batch = self
            .submit_file_batch(&self.settings.mineru_api_base, file_paths, params, true)
            .await?;
        self.upload_files(&batch.file_urls, file_paths).await?;
        let results = self
            .poll_batch(&self.settings.mineru_api_base, &batch.batch_id, true)
            .await?;

        let mut results_by_name: HashMap<String, BatchExtractResult> = HashMap::new();
        for item in results.extract_result {
            results_by_name.insert(item.file_name.clone(), item);
        }

        let mut items = Vec::new();
        for path in file_paths {
            let filename = source_filename(path, false);
            let status = results_by_name.get(&filename).cloned();
            match status {
                Some(result) if result.state == "done" => {
                    let full_zip_url = result
                        .full_zip_url
                        .ok_or_else(|| MineruError::Response("未返回 full_zip_url".to_string()))?;
                    match self.download_and_extract(&full_zip_url, path).await {
                        Ok(extracted) => items.push(ParseDocumentsResultItem {
                            filename,
                            source_url: None,
                            source_path: Some(path.clone()),
                            status: "success".to_string(),
                            content: Some(extracted.markdown),
                            extract_path: Some(extracted.output_dir),
                            error_message: None,
                        }),
                        Err(err) => items.push(ParseDocumentsResultItem {
                            filename,
                            source_url: None,
                            source_path: Some(path.clone()),
                            status: "error".to_string(),
                            content: None,
                            extract_path: None,
                            error_message: Some(err.to_string()),
                        }),
                    }
                }
                Some(result) => {
                    let message = result.err_msg.unwrap_or_else(|| "文件处理失败".to_string());
                    items.push(ParseDocumentsResultItem {
                        filename,
                        source_url: None,
                        source_path: Some(path.clone()),
                        status: "error".to_string(),
                        content: None,
                        extract_path: None,
                        error_message: Some(message),
                    });
                }
                None => items.push(ParseDocumentsResultItem {
                    filename,
                    source_url: None,
                    source_path: Some(path.clone()),
                    status: "error".to_string(),
                    content: None,
                    extract_path: None,
                    error_message: Some("未找到批量结果".to_string()),
                }),
            }
        }

        Ok(items)
    }

    async fn process_local_file(
        &self,
        file_path: &str,
        params: &ParseDocumentsParams,
    ) -> Result<ExtractedContent, MineruError> {
        let batch = self
            .submit_file_batch(
                &self.settings.local_mineru_api_base,
                &[file_path.to_string()],
                params,
                false,
            )
            .await?;
        self.upload_files(&batch.file_urls, &[file_path.to_string()])
            .await?;
        let results = self
            .poll_batch(&self.settings.local_mineru_api_base, &batch.batch_id, false)
            .await?;
        let filename = source_filename(file_path, false);
        let result = results
            .extract_result
            .into_iter()
            .find(|item| item.file_name == filename)
            .ok_or_else(|| MineruError::Response("未找到批量结果".to_string()))?;
        if result.state != "done" {
            return Err(MineruError::Response(
                result.err_msg.unwrap_or_else(|| "本地解析失败".to_string()),
            ));
        }
        let full_zip_url = result
            .full_zip_url
            .ok_or_else(|| MineruError::Response("未返回 full_zip_url".to_string()))?;
        self.download_and_extract(&full_zip_url, file_path).await
    }

    async fn submit_url_task(
        &self,
        base_url: &str,
        url: &str,
        params: &ParseDocumentsParams,
        require_auth: bool,
    ) -> Result<String, MineruError> {
        let endpoint = format!("{base_url}/api/v4/extract/task");
        let mut body = serde_json::json!({
            "url": url,
            "model_version": "pipeline",
        });
        if params.enable_ocr {
            body["is_ocr"] = serde_json::json!(true);
        }
        if let Some(page_ranges) = &params.page_ranges {
            body["page_ranges"] = serde_json::json!(page_ranges);
        }
        body["language"] = serde_json::json!(params.language.clone());

        let response: ApiResponse<TaskCreateData> = self
            .request_json(reqwest::Method::POST, &endpoint, Some(body), require_auth)
            .await?;
        let data = extract_api_data(response)?;
        Ok(data.task_id)
    }

    async fn poll_task(
        &self,
        base_url: &str,
        task_id: &str,
        require_auth: bool,
    ) -> Result<TaskStatusData, MineruError> {
        let endpoint = format!("{base_url}/api/v4/extract/task/{task_id}");
        let start = Instant::now();
        loop {
            let response: ApiResponse<TaskStatusData> = self
                .request_json(reqwest::Method::GET, &endpoint, None, require_auth)
                .await?;
            let data = extract_api_data(response)?;
            match data.state.as_str() {
                "done" => return Ok(data),
                "failed" => {
                    return Err(MineruError::Response(
                        data.err_msg.unwrap_or_else(|| "任务失败".to_string()),
                    ));
                }
                _ => {}
            }

            if start.elapsed() > self.settings.max_wait {
                return Err(MineruError::Timeout);
            }
            sleep(self.settings.poll_interval).await;
        }
    }

    async fn submit_file_batch(
        &self,
        base_url: &str,
        file_paths: &[String],
        params: &ParseDocumentsParams,
        require_auth: bool,
    ) -> Result<BatchCreateData, MineruError> {
        let files: Vec<_> = file_paths
            .iter()
            .map(|path| {
                serde_json::json!({
                    "name": source_filename(path, false),
                    "is_ocr": params.enable_ocr,
                    "page_ranges": params.page_ranges,
                })
            })
            .collect();
        let body = serde_json::json!({
            "files": files,
            "model_version": "pipeline",
            "language": params.language,
        });
        let endpoint = format!("{base_url}/api/v4/file-urls/batch");
        let response: ApiResponse<BatchCreateData> = self
            .request_json(reqwest::Method::POST, &endpoint, Some(body), require_auth)
            .await?;
        extract_api_data(response)
    }

    async fn upload_files(
        &self,
        file_urls: &[String],
        file_paths: &[String],
    ) -> Result<(), MineruError> {
        if file_urls.len() != file_paths.len() {
            return Err(MineruError::Response(
                "file_urls 与 file_paths 数量不一致".to_string(),
            ));
        }
        for (url, path) in file_urls.iter().zip(file_paths.iter()) {
            let bytes = tokio::fs::read(path).await?;
            self.request_bytes_with_retry(reqwest::Method::PUT, url, Bytes::from(bytes))
                .await?;
        }
        Ok(())
    }

    async fn poll_batch(
        &self,
        base_url: &str,
        batch_id: &str,
        require_auth: bool,
    ) -> Result<BatchStatusData, MineruError> {
        let endpoint = format!("{base_url}/api/v4/extract-results/batch/{batch_id}");
        let start = Instant::now();
        loop {
            let response: ApiResponse<BatchStatusData> = self
                .request_json(reqwest::Method::GET, &endpoint, None, require_auth)
                .await?;
            let data = extract_api_data(response)?;
            let done = data
                .extract_result
                .iter()
                .all(|item| item.state == "done" || item.state == "failed");
            if done {
                return Ok(data);
            }
            if start.elapsed() > self.settings.max_wait {
                return Err(MineruError::Timeout);
            }
            sleep(self.settings.poll_interval).await;
        }
    }

    async fn download_and_extract(
        &self,
        zip_url: &str,
        source: &str,
    ) -> Result<ExtractedContent, MineruError> {
        let response = self
            .request_with_retry(reqwest::Method::GET, zip_url, None, false)
            .await?;
        let bytes = response.bytes().await?;
        let output_dir = self.create_output_dir(source)?;
        extract_zip(&bytes, &output_dir)?;
        let markdown_path = find_markdown(&output_dir, source)?;
        let markdown = tokio::fs::read_to_string(markdown_path).await?;
        Ok(ExtractedContent {
            markdown,
            output_dir: output_dir.to_string_lossy().into_owned(),
        })
    }

    fn create_output_dir(&self, source: &str) -> Result<PathBuf, MineruError> {
        let base = &self.settings.output_dir;
        let stem = Path::new(source)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("output");
        let dir = base.join(format!("{stem}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    async fn request_json<T: for<'de> Deserialize<'de>>(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<serde_json::Value>,
        require_auth: bool,
    ) -> Result<T, MineruError> {
        let response = self
            .request_with_retry(method, url, body, require_auth)
            .await?;
        let response = response.error_for_status()?;
        let parsed = response.json::<T>().await?;
        Ok(parsed)
    }

    async fn request_bytes_with_retry(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Bytes,
    ) -> Result<reqwest::Response, MineruError> {
        let mut attempts = 0;
        let max_attempts = 3;
        loop {
            attempts += 1;
            let request = self.client.request(method.clone(), url).body(body.clone());
            match request.send().await {
                Ok(response) => {
                    if response.status().is_server_error() && attempts < max_attempts {
                        sleep(Duration::from_millis(200 * attempts)).await;
                        continue;
                    }
                    return Ok(response.error_for_status()?);
                }
                Err(err) => {
                    if attempts >= max_attempts {
                        return Err(MineruError::Http(err));
                    }
                    sleep(Duration::from_millis(200 * attempts)).await;
                }
            }
        }
    }

    async fn request_with_retry(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<serde_json::Value>,
        require_auth: bool,
    ) -> Result<reqwest::Response, MineruError> {
        let mut attempts = 0;
        let max_attempts = 3;
        loop {
            attempts += 1;
            let mut request = self.client.request(method.clone(), url);
            if require_auth {
                let api_key = self
                    .settings
                    .mineru_api_key
                    .clone()
                    .ok_or(MineruError::MissingApiKey)?;
                request = request.bearer_auth(api_key);
            }
            if let Some(body) = body.clone() {
                if !body.is_null() {
                    request = request.json(&body);
                }
            }
            match request.send().await {
                Ok(response) => {
                    if response.status().is_server_error() && attempts < max_attempts {
                        sleep(Duration::from_millis(200 * attempts)).await;
                        continue;
                    }
                    return Ok(response);
                }
                Err(err) => {
                    if attempts >= max_attempts {
                        return Err(MineruError::Http(err));
                    }
                    sleep(Duration::from_millis(200 * attempts)).await;
                }
            }
        }
    }
}

struct ExtractedContent {
    markdown: String,
    output_dir: String,
}

fn parse_sources(input: &str) -> Vec<String> {
    input
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| item.trim_matches('"').trim_matches('\'').to_string())
        .collect()
}

fn split_sources(sources: &[String]) -> (Vec<String>, Vec<String>) {
    let mut urls = Vec::new();
    let mut files = Vec::new();
    for source in sources {
        if source.starts_with("http://") || source.starts_with("https://") {
            urls.push(source.clone());
        } else {
            files.push(source.clone());
        }
    }
    (urls, files)
}

fn source_filename(source: &str, is_url: bool) -> String {
    let raw = if is_url {
        source.split('/').last().unwrap_or(source)
    } else {
        Path::new(source)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(source)
    };
    raw.split('?').next().unwrap_or(raw).to_string()
}

fn extract_zip(bytes: &[u8], output_dir: &Path) -> Result<(), MineruError> {
    let reader = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(reader)?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let outpath = output_dir.join(file.name());
        if file.name().ends_with('/') {
            std::fs::create_dir_all(&outpath)?;
        } else {
            if let Some(parent) = outpath.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut outfile = std::fs::File::create(&outpath)?;
            std::io::copy(&mut file, &mut outfile)?;
        }
    }
    Ok(())
}

fn extract_api_data<T>(response: ApiResponse<T>) -> Result<T, MineruError> {
    if let Some(code) = response.code {
        if code != 0 {
            return Err(MineruError::Response(
                response
                    .msg
                    .unwrap_or_else(|| format!("MinerU API错误: {code}")),
            ));
        }
    }
    response
        .data
        .ok_or_else(|| MineruError::Response("缺少响应数据".to_string()))
}

fn find_markdown(output_dir: &Path, source: &str) -> Result<PathBuf, MineruError> {
    let stem = Path::new(source)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_lowercase());
    let mut fallback = None;
    for entry in WalkDir::new(output_dir).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_file() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("md") {
                if let Some(stem_name) = path.file_stem().and_then(|s| s.to_str()) {
                    if let Some(expected) = &stem {
                        if stem_name.to_lowercase() == *expected {
                            return Ok(path.to_path_buf());
                        }
                    }
                }
                if fallback.is_none() {
                    fallback = Some(path.to_path_buf());
                }
            }
        }
    }
    fallback.ok_or(MineruError::MissingMarkdown)
}

fn init_tracing() {
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn health_handler(State(server): State<MineruServer>) -> AxumJson<HealthCheckResponse> {
    let is_healthy = server.healthy.load(Ordering::Relaxed);
    AxumJson(HealthCheckResponse {
        status: if is_healthy { "ok".to_string() } else { "error".to_string() },
        server: "mineru-mcp-dragonos".to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        api_mode: if server.settings.use_local_api { "local".to_string() } else { "remote".to_string() },
        api_base: if server.settings.use_local_api {
            server.settings.local_mineru_api_base.clone()
        } else {
            server.settings.mineru_api_base.clone()
        },
        has_api_key: server.settings.mineru_api_key.is_some(),
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn root_handler() -> &'static str {
    "MinerU MCP DragonOS Server\n\nEndpoints:\n  GET  /health - Health check\n  POST /mcp    - MCP JSON-RPC requests\n  GET  /mcp    - SSE event stream"
}

/// 启动 HTTP/SSE MCP 服务器
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let settings = Settings::from_env();
    if !settings.use_local_api && settings.mineru_api_key.is_none() {
        warn!("MINERU_API_KEY 未设置，远程解析将失败");
    }

    let ct = CancellationToken::new();
    let port = env::var("MCP_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(8080);

    // 创建 HTTP/SSE MCP 服务
    let mcp_service: StreamableHttpService<MineruServer, LocalSessionManager> =
        StreamableHttpService::new(
            || {
                let s = Settings::from_env();
                MineruServer::new(s).map_err(|e| std::io::Error::other(e.to_string()))
            },
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig {
                stateful_mode: true,
                sse_keep_alive: Some(Duration::from_secs(15)),
                cancellation_token: ct.child_token(),
                ..Default::default()
            },
        );

    // 创建一个用于健康检查的 server 实例
    let health_server = MineruServer::new(settings)?;

    // 统一的 Axum 路由
    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/", get(root_handler))
        .with_state(health_server)
        .nest_service("/mcp", mcp_service);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
    info!("MCP HTTP/SSE 服务启动在 http://0.0.0.0:{}", port);
    info!("端点: GET /health, POST /mcp, GET /mcp");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move { ct.cancelled().await })
        .await?;

    Ok(())
}
