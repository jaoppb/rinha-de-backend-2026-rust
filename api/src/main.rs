mod http_parser;
mod json_parser;
mod knn;
mod mmap;
mod vectorizer;

use monoio::io::{AsyncReadRent, AsyncWriteRentExt};
use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;
use std::time::Duration;

use crate::http_parser::{HttpRoute, parse_http_request};
use crate::json_parser::parse_json_payload;
use crate::knn::IvfIndex;
use crate::mmap::{load_dataset, load_ivf_data, load_lookups, Record};
use crate::vectorizer::vectorize;

macro_rules! debug_log {
    ($($arg:tt)*) => {
        #[cfg(any(debug_assertions, feature = "verbose"))]
        {
            println!($($arg)*);
            let _ = std::io::stdout().flush();
        }
    };
}

type SharedState = Rc<RefCell<Option<(Rc<IvfIndex>, &'static [Record])>>>;

fn main() {
    println!("Starting API with embedded lookups...");
    let lookups = Rc::new(load_lookups());

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

        let state: SharedState = Rc::new(RefCell::new(None));

        // Background loader
        let loader_state = state.clone();
        monoio::spawn(async move {
            println!("Waiting for shared datasets in /data/shared (dataset.bin, centroids.bin, indices.bin, offsets.bin)...");
            loop {
                match (load_dataset(), load_ivf_data()) {
                    (Ok(dataset), Ok(ivf_data)) => {
                        println!("Successfully loaded all datasets.");
                        let index = Rc::new(IvfIndex::new(ivf_data));
                        let records = dataset.records;
                        *loader_state.borrow_mut() = Some((index, records));
                        break;
                    }
                    (Err(e), _) if e.kind() == std::io::ErrorKind::NotFound => {
                        monoio::time::sleep(Duration::from_millis(100)).await;
                    }
                    (_, Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                        monoio::time::sleep(Duration::from_millis(100)).await;
                    }
                    (Err(e), _) => {
                        println!("Error loading dataset: {:?}", e);
                        monoio::time::sleep(Duration::from_millis(500)).await;
                    }
                    (_, Err(e)) => {
                        println!("Error loading IVF data: {:?}", e);
                        monoio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        });

        loop {
            let (conn, _) = listener.accept().await.unwrap();
            let lookups = lookups.clone();
            let state = state.clone();
            monoio::spawn(async move {
                handle_connection(conn, lookups, state).await;
            });
        }
    })
}

trait Stream: AsyncReadRent + AsyncWriteRentExt {}
impl Stream for monoio::net::UnixStream {}

async fn handle_connection<S: Stream>(
    mut conn: S,
    lookups: Rc<crate::mmap::LookupData>,
    state: SharedState,
) {
    debug_log!("New connection received");
    let mut main_buf = Vec::with_capacity(4096);
    let mut read_buf = vec![0u8; 4096];

    loop {
        let (res, buf_returned) = conn.read(read_buf).await;
        read_buf = buf_returned;

        match res {
            Ok(n) if n > 0 => {
                main_buf.extend_from_slice(&read_buf[..n]);
            }
            _ => {
                debug_log!("Connection closed or error");
                return;
            }
        }

        // Process all complete requests in the buffer
        while !main_buf.is_empty() {
            #[cfg(any(debug_assertions, feature = "verbose"))]
            let t_req_start = std::time::Instant::now();

            let (route, consumed) = parse_http_request(&main_buf);

            match route {
                HttpRoute::Incomplete => {
                    break;
                }
                HttpRoute::Ready => {
                    debug_log!("Handling /ready");
                    if state.borrow().is_some() {
                        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
                        let _ = conn.write_all(response).await;
                    } else {
                        let response = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";
                        let _ = conn.write_all(response).await;
                    }
                }
                HttpRoute::FraudScore(body) => {
                    let state_borrow = state.borrow();
                    if let Some((index, records)) = state_borrow.as_ref() {
                        if body.is_empty() {
                            let response = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
                            let _ = conn.write_all(response).await;
                        } else {
                            let tx = parse_json_payload(body);
                            #[cfg(any(debug_assertions, feature = "verbose"))]
                            let t_parse = t_req_start.elapsed();

                            if let Some(tx) = tx {
                                let vec = vectorize(&tx, &lookups);
                                #[cfg(any(debug_assertions, feature = "verbose"))]
                                let t_vectorize = t_req_start.elapsed();

                                if let Some(vec) = vec {
                                    let (approved, score) = index.search(&vec, records);
                                    #[cfg(any(debug_assertions, feature = "verbose"))]
                                    let t_search = t_req_start.elapsed();

                                    let resp_body =
                                        format!("{{\"approved\":{},\"fraud_score\":{:.1}}}", approved, score);
                                    let response = format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                                        resp_body.len(),
                                        resp_body
                                    );
                                    let _ = conn.write_all(response.into_bytes()).await;

                                    #[cfg(any(debug_assertions, feature = "verbose"))]
                                    {
                                        let t_total = t_req_start.elapsed();
                                        println!(
                                            "REQ: proc={:?}, parse={:?}, vec={:?}, search={:?}, write={:?}",
                                            t_total,
                                            t_parse,
                                            t_vectorize - t_parse,
                                            t_search - t_vectorize,
                                            t_total - t_search
                                        );
                                    }
                                } else {
                                    let response =
                                        b"HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: 0\r\n\r\n";
                                    let _ = conn.write_all(response).await;
                                }
                            } else {
                                let response = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
                                let _ = conn.write_all(response).await;
                            }
                        }
                    } else {
                        let response = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";
                        let _ = conn.write_all(response).await;
                    }
                }
                HttpRoute::NotFound => {
                    let response = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
                    let _ = conn.write_all(response).await;
                }
            }

            main_buf.drain(..consumed);
        }
    }
}

