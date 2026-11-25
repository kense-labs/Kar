use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    io::{BufRead, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener},
};
pub fn service() {
    let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    fs::write(
        ".kar-preview-port",
        socket.local_addr().unwrap().port().to_string(),
    )
    .unwrap();
    socket.set_nonblocking(true).unwrap();
    let started = std::time::Instant::now();
    while Path::new(".kar-preview-port").exists() && started.elapsed() < Duration::from_secs(210) {
        let mut stream = match socket.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(30));
                continue;
            }
            Err(_) => break,
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let reader = std::io::BufReader::new(stream.try_clone().unwrap());
        for line in reader.lines() {
            if line.is_err() || line.unwrap().is_empty() {
                break;
            }
        }
        let body = fs::read("notes.md").unwrap();
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
        stream.write_all(&body).unwrap();
    }
}
pub async fn open(host: &Client, scope: &str, tree: &Path) -> (Uuid, Value) {
    let terminal = Uuid::new_v4();
    let route = format!("{scope}/terminals/{terminal}");
    ok(host
        .post(format!("{scope}/terminals"))
        .json(&json!({"id":terminal})))
    .await;
    let exe = std::env::current_exe().unwrap();
    let command = format!(
        "'{}' preview-server\n",
        exe.to_str().unwrap().replace('\'', "'\"'\"'")
    );
    ok(host
        .post(format!("{route}/input"))
        .json(&json!({"id":Uuid::new_v4(),"data_base64":STANDARD.encode(command)})))
    .await;
    let target = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(port) = fs::read_to_string(tree.join(".kar-preview-port"))
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
            {
                let list = ok(host.get(format!("{route}/ports"))).await;
                if let Some(t) = list.as_array().unwrap().iter().find(|t| t["port"] == port) {
                    break t.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
    })
    .await
    .expect("no listener from the actual test PTY");
    let preview = ok(host
        .post(format!("{scope}/previews"))
        .json(&json!({"terminal":terminal,"target":target})))
    .await;
    let url = url::Url::parse(preview["url"].as_str().unwrap()).unwrap();
    let browser = Client::builder()
        .no_proxy()
        .resolve(
            url.host_str().unwrap(),
            SocketAddr::from((Ipv4Addr::LOCALHOST, url.port().unwrap())),
        )
        .build()
        .unwrap();
    let text = browser
        .get(url)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(text, BASE_TEXT);
    (terminal, preview)
}
