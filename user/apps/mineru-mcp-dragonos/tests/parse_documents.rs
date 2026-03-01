use mineru_mcp::{MineruServer, ParseDocumentsParams, Settings};
use rmcp::handler::server::wrapper::Parameters;
use std::{io::Write, path::Path};
use tempfile::tempdir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn build_zip_bytes() -> Vec<u8> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(cursor);
    let options = zip::write::FileOptions::default();

    zip.start_file("document.md", options).unwrap();
    zip.write_all(b"Hello from mineru").unwrap();
    zip.add_directory("images/", options).unwrap();
    zip.start_file("images/pic.png", options).unwrap();
    zip.write_all(b"fake").unwrap();
    let cursor = zip.finish().unwrap();
    cursor.into_inner()
}

#[tokio::test]
async fn parse_documents_handles_url_and_file() {
    let server = MockServer::start().await;
    let zip_bytes = build_zip_bytes();

    Mock::given(method("POST"))
        .and(path("/api/v4/extract/task"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "data": {"task_id": "task-1"}
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v4/extract/task/task-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "data": {"state": "done", "full_zip_url": format!("{}/download/result.zip", server.uri())}
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/v4/file-urls/batch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "data": {
                "batch_id": "batch-1",
                "file_urls": [format!("{}/upload/1", server.uri())]
            }
        })))
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path("/upload/1"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v4/extract-results/batch/batch-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "data": {
                "extract_result": [
                    {
                        "file_name": "local.pdf",
                        "state": "done",
                        "full_zip_url": format!("{}/download/result.zip", server.uri())
                    }
                ]
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/download/result.zip"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_bytes.clone()))
        .expect(2)
        .mount(&server)
        .await;

    let temp = tempdir().unwrap();
    let output_dir = temp.path().join("downloads");
    std::fs::create_dir_all(&output_dir).unwrap();
    let local_path = temp.path().join("local.pdf");
    std::fs::write(&local_path, b"dummy").unwrap();

    unsafe {
        std::env::set_var("MINERU_API_BASE", server.uri());
        std::env::set_var("MINERU_API_KEY", "test-key");
        std::env::set_var("OUTPUT_DIR", output_dir.to_string_lossy().to_string());
        std::env::set_var("USE_LOCAL_API", "false");
        std::env::set_var("MINERU_POLL_INTERVAL_SECS", "1");
        std::env::set_var("MINERU_MAX_WAIT_SECS", "10");
    }

    let server_impl = MineruServer::new(Settings::from_env()).unwrap();
    let params = ParseDocumentsParams {
        file_sources: format!("https://example.com/test.pdf {}", local_path.display()),
        enable_ocr: false,
        language: "ch".to_string(),
        page_ranges: None,
    };

    let response = server_impl
        .parse_documents(Parameters(params))
        .await
        .unwrap();

    let response = response.0;
    assert_eq!(response.status, "success");
    let summary = response.summary.expect("summary");
    assert_eq!(summary.total_files, 2);
    assert_eq!(summary.success_count, 2);
    assert_eq!(summary.error_count, 0);
    let results = response.results.expect("results");
    assert_eq!(results.len(), 2);
    for item in results {
        assert_eq!(item.status, "success");
        assert!(item.content.as_deref() == Some("Hello from mineru"));
        let extract_path = item.extract_path.expect("extract_path");
        assert!(Path::new(&extract_path).exists());
    }
}
