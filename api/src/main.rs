mod http_parser;
mod json_parser;
mod knn;
mod mmap;
mod vectorizer;

use monoio::io::{AsyncReadRent, AsyncWriteRentExt};
use std::io::Write;
use std::rc::Rc;
use std::time::Duration;

use crate::http_parser::{HttpRoute, parse_http_request};
use crate::json_parser::parse_json_payload;
use crate::knn::IvfIndex;
use crate::mmap::{LookupData, load_dataset, load_lookups};
use crate::vectorizer::vectorize;

macro_rules! debug_log {
    ($($arg:tt)*) => {
        #[cfg(debug_assertions)]
        {
            println!($($arg)*);
            let _ = std::io::stdout().flush();
        }
    };
}
fn main() {
    // 1. Wait for shared memory files
    debug_log!("Waiting for datasets in /dev/shm...");

    let (dataset, lookups) = loop {
        match (load_dataset(), load_lookups()) {
            (Ok(d), Ok(l)) => break (d, l),
            _ => {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    };

    debug_log!("Building IVF index...");
    let index = IvfIndex::build(dataset.records);

    let lookups = Rc::new(lookups);
    let index = Rc::new(index);
    let dataset_records = dataset.records;

    // 2. Start monoio runtime
    let mut rt = monoio::RuntimeBuilder::<monoio::FusionDriver>::new()
        .with_entries(1024)
        .enable_timer()
        .build()
        .unwrap();

    rt.block_on(async {
        let sock_path = std::env::var("SOCK").expect("SOCK env var must be set");
        let _ = std::fs::remove_file(&sock_path);
        let std_listener = std::os::unix::net::UnixListener::bind(&sock_path)
            .expect("Failed to bind std unix socket");
        std_listener
            .set_nonblocking(true)
            .expect("Failed to set non-blocking");
        let listener = monoio::net::UnixListener::from_std(std_listener)
            .expect("Failed to convert std unix socket to monoio");
        debug_log!("Ready on unix:{}", sock_path);
        loop {
            let (conn, _) = listener.accept().await.unwrap();
            debug_log!("Accepted connection");
            let lookups = lookups.clone();
            let index = index.clone();
            monoio::spawn(async move {
                handle_connection(conn, lookups, index, dataset_records).await;
            });
        }
    })
}

trait Stream: AsyncReadRent + AsyncWriteRentExt {}
impl Stream for monoio::net::UnixStream {}

async fn handle_connection<S: Stream>(
    mut conn: S,
    lookups: Rc<LookupData>,
    index: Rc<IvfIndex>,
    records: &'static [crate::mmap::Record],
) {
    debug_log!("New connection received");
    let start = std::time::Instant::now();
    let buf = vec![0u8; 4096];
    let (res, buf) = conn.read(buf).await;
    let n = match res {
        Ok(n) if n > 0 => {
            debug_log!("Read {} bytes", n);
            n
        }
        _ => return,
    };

    let route = parse_http_request(&buf[..n]);
    match route {
        HttpRoute::Ready => {
            debug_log!("Handling /ready");
            let response = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
            let _ = conn.write_all(response).await;
        }
        HttpRoute::FraudScore(body) => {
            debug_log!("Handling /fraud-score with body size {}", body.len());
            if body.is_empty() {
                let response = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
                let _ = conn.write_all(response).await;
                return;
            }

            if let Some(tx) = parse_json_payload(body) {
                debug_log!("Parsed payload successfully");
                if let Some(vec) = vectorize(&tx, &lookups) {
                    debug_log!("Vectorized payload successfully");
                    let (approved, score) = index.search(&vec, records);
                    debug_log!("Search completed: approved={}, score={}", approved, score);

                    let resp_body =
                        format!("{{\"approved\":{},\"fraud_score\":{:.1}}}", approved, score);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        resp_body.len(),
                        resp_body
                    );
                    let _ = conn.write_all(response.into_bytes()).await;
                } else {
                    debug_log!("Failed to vectorize payload");
                    let response =
                        b"HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: 0\r\n\r\n";
                    let _ = conn.write_all(response).await;
                }
            } else {
                debug_log!("Failed to parse payload");
                let response = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
                let _ = conn.write_all(response).await;
            }
        }
        HttpRoute::NotFound => {
            debug_log!("Handling Not Found");
            let response = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
            let _ = conn.write_all(response).await;
        }
    }
    debug_log!("Request processed in {:?}", start.elapsed());
}
