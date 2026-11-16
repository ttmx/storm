//! End-to-end: engine + stormd router on a real socket, driven through `RemoteBackend`.

use std::time::Duration;

use futures::StreamExt;
use storm_client::RemoteBackend;
use storm_engine::{EngineConfig, LocalBackend};
use storm_proto::{AddTorrentRequest, Backend, BackendKind, ErrorKind, Event, SettingsPatch, TorrentState};

const TOKEN: &str = "test-token";

async fn serve(tmp: &std::path::Path) -> (LocalBackend, String) {
    let engine = LocalBackend::start(EngineConfig { data_dir: tmp.join("data"), default_download_dir: tmp.join("dl") })
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = storm_server::router(engine.clone(), TOKEN);
    tokio::spawn(async move { axum_serve(listener, app).await });
    (engine, format!("127.0.0.1:{}", addr.port()))
}

async fn axum_serve(listener: tokio::net::TcpListener, app: axum::Router) {
    axum::serve(listener, app).await.unwrap();
}

async fn make_torrent(dir: &std::path::Path) -> (std::path::PathBuf, Vec<u8>, Vec<u8>) {
    std::fs::create_dir_all(dir).unwrap();
    let file = dir.join("movie.bin");
    let data: Vec<u8> = (0..700_000u32).map(|i| (i % 253) as u8).collect();
    std::fs::write(&file, &data).unwrap();
    let bytes = librqbit::create_torrent(
        &file,
        librqbit::CreateTorrentOptions { name: None, trackers: vec![], piece_length: None },
        &librqbit::spawn_utils::BlockingSpawner::new(1),
    )
    .await
    .unwrap()
    .as_bytes()
    .unwrap()
    .to_vec();
    (file, data, bytes)
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_backend_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let (_engine, addr) = serve(tmp.path()).await;

    // Wrong token is rejected clearly.
    let err = RemoteBackend::connect(&addr, "nope").await.err().expect("bad token must fail");
    assert_eq!(err.kind, ErrorKind::Unauthorized);

    let remote = RemoteBackend::connect(&addr, TOKEN).await.unwrap();
    let info = remote.info();
    assert!(!info.is_local());
    assert_eq!(info.kind, BackendKind::Remote { url: format!("http://{addr}") });

    let mut events = remote.subscribe().await.unwrap();
    match tokio::time::timeout(Duration::from_secs(5), events.next()).await.unwrap().unwrap() {
        Event::Snapshot { torrents, .. } => assert!(torrents.is_empty()),
        e => panic!("expected snapshot, got {e:?}"),
    }

    // Add (seeding from existing data) and wait for it to be verified.
    let seed_dir = tmp.path().join("seed");
    let (_file, data, torrent) = make_torrent(&seed_dir).await;
    let mut req = AddTorrentRequest::file(torrent);
    req.output_folder = Some(seed_dir.to_string_lossy().into_owned());
    let added = remote.add_torrent(req).await.unwrap();
    let mut seeding = false;
    for _ in 0..100 {
        if remote.list_torrents().await.unwrap()[0].state == TorrentState::Seeding {
            seeding = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(seeding);

    // Events keep flowing and include the torrent.
    let mut saw = false;
    for _ in 0..5 {
        if let Some(Event::Snapshot { torrents, .. }) =
            tokio::time::timeout(Duration::from_secs(3), events.next()).await.unwrap()
            && torrents.iter().any(|t| t.id == added.id)
        {
            saw = true;
            break;
        }
    }
    assert!(saw, "snapshot events should include the new torrent");

    let details = remote.torrent_details(added.id).await.unwrap();
    assert_eq!(details.files[0].path, "movie.bin");
    assert!(remote.torrent_peers(added.id).await.unwrap().is_empty());

    // Piece picking round trip.
    remote.set_sequential(added.id, true).await.unwrap();
    remote.set_file_priority(added.id, vec![0], storm_proto::FilePriority::High).await.unwrap();
    let details = remote.torrent_details(added.id).await.unwrap();
    assert!(details.summary.sequential);
    assert_eq!(details.files[0].priority, storm_proto::FilePriority::High);
    let err = remote.set_file_priority(added.id, vec![7], storm_proto::FilePriority::Low).await.unwrap_err();
    assert_eq!(err.kind, storm_proto::ErrorKind::BadRequest);

    // Download to "this PC".
    let dest = tmp.path().join("downloaded.bin");
    remote.download_file(added.id, 0, dest.clone()).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);

    // Settings round trip, including clearing a limit.
    let s = remote
        .update_settings(SettingsPatch { up_limit_bps: Some(Some(123_456)), ..Default::default() })
        .await
        .unwrap();
    assert_eq!(s.up_limit_bps, Some(123_456));
    let s = remote.update_settings(SettingsPatch { up_limit_bps: Some(None), ..Default::default() }).await.unwrap();
    assert_eq!(s.up_limit_bps, None);

    // Folder browser.
    let new_dir = tmp.path().join("browse").join("Fresh");
    remote.create_dir(new_dir.to_string_lossy().into_owned()).await.unwrap();
    let listing = remote.list_dirs(Some(tmp.path().join("browse").to_string_lossy().into_owned())).await.unwrap();
    assert_eq!(listing.dirs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["Fresh"]);

    let hist = remote.stats_history(0).await.unwrap();
    assert!(hist.iter().all(|s| s.ts > 0));
    remote.session_stats().await.unwrap();

    // Errors carry their kind across the wire.
    let err = remote.torrent_details(999_999).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotFound);

    remote.pause(added.id).await.unwrap();
    remote.resume(added.id).await.unwrap();
    remote.remove(added.id, false).await.unwrap();
    assert!(remote.list_torrents().await.unwrap().is_empty());
}

/// The client must be usable from a non-tokio executor (that's how gpui drives it).
#[test]
fn works_without_a_tokio_context() {
    let server_rt = tokio::runtime::Runtime::new().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let (_engine, addr) = server_rt.block_on(serve(tmp.path()));
    futures::executor::block_on(async {
        let remote = RemoteBackend::connect(&addr, TOKEN).await.unwrap();
        assert!(remote.list_torrents().await.unwrap().is_empty());
        let mut ev = remote.subscribe().await.unwrap();
        assert!(matches!(ev.next().await, Some(Event::Snapshot { .. })));
        drop(remote); // dropping the owned runtime off-tokio must not panic
    });
}
