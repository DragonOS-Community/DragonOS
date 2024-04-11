use actix_web::{web, App, HttpServer, HttpRequest, HttpResponse};
use std::io;

async fn index(req: HttpRequest) -> HttpResponse {
    // 获取请求方法
    let method = req.method().to_string();
    // 获取请求路径
    let path = req.path().to_string();
    // 获取请求头部信息
    let headers = req.headers().clone();
    // 获取查询参数
    let query_params = req.query_string().to_string();

    // 打印请求信息
    println!("Received {} request to {}", method, path);
    println!("Headers: {:?}", headers);
    println!("Query params: {}", query_params);

    // 返回响应
    HttpResponse::Ok().body("Hello, World!")
}

#[actix_web::main]
async fn main() -> io::Result<()> {
    // 设置 TCP backlog 大小为 5
    let backlog_size = 5;

    HttpServer::new(|| {
        App::new()
            .route("/", web::get().to(index))
    })
    .backlog(backlog_size) // 设置 TCP backlog 大小
    .bind("0.0.0.0:12580")?
    .run()
    .await
}
