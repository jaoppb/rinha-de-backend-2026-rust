use monoio::io::{AsyncReadRent, AsyncWriteRentExt, Splitable};
use monoio::net::{TcpListener, UnixStream};
use monoio::buf::IoBuf;
use std::rc::Rc;
use std::cell::Cell;

async fn proxy<R, W>(mut reader: R, mut writer: W) -> std::io::Result<()>
where
    R: AsyncReadRent + 'static,
    W: AsyncWriteRentExt + 'static,
{
    let mut buf = vec![0u8; 16384];
    loop {
        let (res, b) = reader.read(buf).await;
        buf = b;
        let n = res?;
        if n == 0 {
            break;
        }
        let (res, b) = writer.write_all(buf.slice(..n)).await;
        buf = b.into_inner();
        res?;
    }
    Ok(())
}

fn main() {
    let mut rt = monoio::RuntimeBuilder::<monoio::FusionDriver>::new()
        .with_entries(1024)
        .build()
        .expect("Failed to build monoio runtime");

    rt.block_on(async {
        let listener = TcpListener::bind("0.0.0.0:9999").expect("Failed to bind to 0.0.0.0:9999");
        let counter = Rc::new(Cell::new(0usize));
        let backends = ["/data/shared/api1.sock", "/data/shared/api2.sock"];

        println!("Rust LB listening on 0.0.0.0:9999 (Edition 2024)");

        loop {
            let (conn, _) = match listener.accept().await {
                Ok(res) => res,
                Err(e) => {
                    eprintln!("Accept error: {}", e);
                    continue;
                }
            };

            let idx = counter.get();
            counter.set((idx + 1) % backends.len());
            let backend_path = backends[idx];

            monoio::spawn(async move {
                if let Ok(backend) = UnixStream::connect(backend_path).await {
                    let (client_rd, client_wr) = conn.into_split();
                    let (backend_rd, backend_wr) = backend.into_split();

                    let t1 = monoio::spawn(proxy(client_rd, backend_wr));
                    let t2 = monoio::spawn(proxy(backend_rd, client_wr));

                    let _ = t1.await;
                    let _ = t2.await;
                } else {
                    eprintln!("Failed to connect to backend: {}", backend_path);
                }
            });
        }
    })
}
