use std::time::Duration;

use hysteria_extras::speedtest::{Client, spawn_server_conn};

#[tokio::test(flavor = "current_thread")]
async fn size_based_download_and_upload_work() {
    let mut download_client = Client::new(spawn_server_conn());
    let download = download_client
        .download(128 * 1024, Duration::ZERO, |_| {})
        .await
        .expect("download should succeed");
    assert_eq!(download.bytes, 128 * 1024);

    let mut upload_client = Client::new(spawn_server_conn());
    let upload = upload_client
        .upload(96 * 1024, Duration::ZERO, |_| {})
        .await
        .expect("upload should succeed");
    assert_eq!(upload.bytes, 96 * 1024);
}

#[tokio::test(flavor = "current_thread")]
async fn time_based_upload_reports_peer_received_bytes() {
    let mut upload_client = Client::new(spawn_server_conn());
    let upload = upload_client
        .upload(0, Duration::from_millis(200), |_| {})
        .await
        .expect("time-based upload should succeed");
    assert!(upload.bytes > 0);
    assert!(upload.elapsed >= Duration::from_millis(150));
}
