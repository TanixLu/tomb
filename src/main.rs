mod smtp;

use std::{sync::Arc, time::Duration};

use axum::{
    extract::{Path, State},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

use smtp::MailBox;

fn spawn_cleaning_task(mailbox: Arc<MailBox>) -> JoinHandle<()> {
    const PERIOD: Duration = std::time::Duration::from_secs(60);
    let mut interval = tokio::time::interval(PERIOD);
    tokio::spawn(async move {
        loop {
            interval.tick().await;
            mailbox.retain(|_, (t, _)| t.elapsed() < PERIOD);
            mailbox.shrink_to_fit();
        }
    })
}

fn spawn_smtp_task(stream: TcpStream, mailbox: Arc<MailBox>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let _ = smtp::Server::new("mail.xiwata.com", stream, mailbox)
            .serve()
            .await;
    })
}

async fn spawn_smtp_server(mailbox: Arc<MailBox>) -> JoinHandle<()> {
    const ADDR: &str = "0.0.0.0:25";
    let listener = TcpListener::bind(ADDR).await.unwrap();
    println!("SMTP server listening on: {}", ADDR);
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let _ = spawn_smtp_task(stream, mailbox.clone());
        }
    })
}

async fn get_email_handler(
    Path(username): Path<String>,
    State(mailbox): State<Arc<MailBox>>,
) -> Response {
    match mailbox.remove(&username) {
        Some((_, data)) => data.1.into_response(),
        None => "".into_response(),
    }
}

async fn spawn_http_server(mailbox: Arc<MailBox>) -> JoinHandle<()> {
    let app = Router::new()
        .route("/:username", get(get_email_handler))
        .with_state(mailbox.clone());
    const ADDR: &str = "0.0.0.0:63221";
    let http_listener = TcpListener::bind(ADDR).await.unwrap();
    println!("HTTP server listening on: {}", ADDR);
    let http_server = axum::serve(http_listener, app);
    tokio::spawn(async move {
        let _ = http_server.await;
    })
}

#[tokio::main]
async fn main() {
    let mailbox = Arc::new(MailBox::new());

    let cleaning_task = spawn_cleaning_task(mailbox.clone());
    let smtp_server = spawn_smtp_server(mailbox.clone()).await;
    let http_server = spawn_http_server(mailbox.clone()).await;

    let _ = tokio::join!(cleaning_task, smtp_server, http_server);
}
