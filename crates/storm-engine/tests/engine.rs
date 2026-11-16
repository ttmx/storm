//! Offline end-to-end test: seed a locally created torrent through the `Backend` trait.

use std::time::Duration;

use futures::StreamExt;
use storm_engine::{EngineConfig, LocalBackend};
use storm_proto::{AddTorrentRequest, Backend, Event, SettingsPatch, TorrentState};

#[tokio::test(flavor = "multi_thread")]
async fn add_seed_inspect_remove() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let file = src.join("hello.bin");
    std::fs::write(&file, (0..1_000_000u32).map(|i| (i % 251) as u8).collect::<Vec<_>>()).unwrap();

    let torrent = librqbit::create_torrent(
        &file,
        librqbit::CreateTorrentOptions { name: None, trackers: vec![], piece_length: None },
        &librqbit::spawn_utils::BlockingSpawner::new(1),
    )
    .await
    .unwrap()
    .as_bytes()
    .unwrap()
    .to_vec();

    let backend = LocalBackend::start(EngineConfig {
        data_dir: tmp.path().join("data"),
        default_download_dir: tmp.path().join("dl"),
    })
    .await
    .unwrap();
    assert!(backend.info().is_local());

    let mut req = AddTorrentRequest::file(torrent);
    req.output_folder = Some(src.to_string_lossy().into_owned());
    let added = backend.add_torrent(req).await.unwrap();
    assert!(!added.already_existed);

    // Existing data gets verified, so the torrent should end up seeding.
    let mut summary = None;
    for _ in 0..100 {
        let list = backend.list_torrents().await.unwrap();
        assert_eq!(list.len(), 1);
        if list[0].state == TorrentState::Seeding {
            summary = Some(list[0].clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let summary = summary.expect("torrent never reached seeding");
    assert_eq!(summary.name.as_deref(), Some("hello.bin"));
    assert_eq!(summary.progress_bytes, 1_000_000);
    assert!(summary.completed_at.is_some());

    let details = backend.torrent_details(added.id).await.unwrap();
    assert_eq!(details.files.len(), 1);
    assert_eq!(details.files[0].path, "hello.bin");
    assert!(details.files[0].included);
    assert!(details.piece_count.unwrap() > 0);
    let have = details.have_pieces.expect("a seeding torrent knows its pieces");
    assert!((0..details.piece_count.unwrap()).all(|i| have.has(i)), "seeding means every piece is there");

    let mut events = backend.subscribe().await.unwrap();
    match events.next().await.unwrap() {
        Event::Snapshot { torrents, .. } => assert_eq!(torrents[0].id, added.id),
        e => panic!("expected snapshot first, got {e:?}"),
    }

    let s = backend
        .update_settings(SettingsPatch { down_limit_bps: Some(Some(1_000_000)), ..Default::default() })
        .await
        .unwrap();
    assert_eq!(s.down_limit_bps, Some(1_000_000));
    assert_eq!(backend.settings().await.unwrap().down_limit_bps, Some(1_000_000));

    backend.pause(added.id).await.unwrap();
    assert_eq!(backend.list_torrents().await.unwrap()[0].state, TorrentState::Paused);
    backend.resume(added.id).await.unwrap();

    backend.remove(added.id, false).await.unwrap();
    assert!(backend.list_torrents().await.unwrap().is_empty());
    assert!(file.exists(), "remove without delete_files must keep data");

    backend.shutdown().await;
}

async fn start(tmp: &std::path::Path) -> LocalBackend {
    LocalBackend::start(EngineConfig { data_dir: tmp.join("data"), default_download_dir: tmp.join("dl") })
        .await
        .unwrap()
}

/// Multi-file torrents must land in `<download dir>/<torrent name>/`, never loose in the
/// download dir (where `overwrite` could clobber unrelated files).
#[tokio::test(flavor = "multi_thread")]
async fn multi_file_torrent_gets_its_own_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let content = tmp.path().join("seed").join("my-album");
    std::fs::create_dir_all(content.join("sub")).unwrap();
    std::fs::write(content.join("a.bin"), vec![7u8; 300_000]).unwrap();
    std::fs::write(content.join("sub").join("b.txt"), b"hello").unwrap();

    let torrent = librqbit::create_torrent(
        &content,
        librqbit::CreateTorrentOptions { name: None, trackers: vec![], piece_length: None },
        &librqbit::spawn_utils::BlockingSpawner::new(1),
    )
    .await
    .unwrap()
    .as_bytes()
    .unwrap()
    .to_vec();

    let backend = start(tmp.path()).await;
    // Point at the *parent*: the engine must append "my-album" itself.
    let mut req = AddTorrentRequest::file(torrent);
    req.output_folder = Some(tmp.path().join("seed").to_string_lossy().into_owned());
    let added = backend.add_torrent(req).await.unwrap();

    let mut seeding = false;
    for _ in 0..100 {
        let t = &backend.list_torrents().await.unwrap()[0];
        if t.state == TorrentState::Seeding {
            assert_eq!(std::path::Path::new(&t.output_folder), content);
            seeding = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(seeding, "existing data in the subfolder should be verified and seeded");
    let files = backend.torrent_details(added.id).await.unwrap().files;
    assert_eq!(files.len(), 2);
    backend.shutdown().await;
}

/// Adding a magnet returns immediately with a pending row; removing it cancels the fetch.
#[tokio::test(flavor = "multi_thread")]
async fn magnet_add_is_non_blocking_and_cancellable() {
    let tmp = tempfile::tempdir().unwrap();
    let backend = start(tmp.path()).await;
    // Random hash nobody seeds: resolution can never finish.
    let magnet = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Pending%20Thing";

    let added = tokio::time::timeout(Duration::from_secs(5), backend.add_torrent(AddTorrentRequest::url(magnet)))
        .await
        .expect("add must not block on metadata")
        .unwrap();
    let list = backend.list_torrents().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, added.id);
    assert_eq!(list[0].state, TorrentState::FetchingMetadata);
    assert_eq!(list[0].name.as_deref(), Some("Pending Thing"));
    assert_eq!(list[0].info_hash, "0123456789abcdef0123456789abcdef01234567");

    // Same magnet again is deduplicated.
    let again = backend.add_torrent(AddTorrentRequest::url(magnet)).await.unwrap();
    assert!(again.already_existed);
    assert_eq!(again.id, added.id);

    assert!(backend.pause(added.id).await.is_err());
    assert!(backend.torrent_details(added.id).await.unwrap().files.is_empty());
    backend.remove(added.id, false).await.unwrap();
    assert!(backend.list_torrents().await.unwrap().is_empty());
    backend.shutdown().await;
}

/// Comment / creator / creation date come from the .torrent metainfo, and survive a
/// restart (librqbit re-reads the stored .torrent file).
#[tokio::test(flavor = "multi_thread")]
async fn details_expose_metainfo_comment() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let file = src.join("commented.bin");
    std::fs::write(&file, vec![42u8; 200_000]).unwrap();

    let mut created = librqbit::create_torrent(
        &file,
        librqbit::CreateTorrentOptions { name: None, trackers: vec![], piece_length: None },
        &librqbit::spawn_utils::BlockingSpawner::new(1),
    )
    .await
    .unwrap();
    created.meta.comment = Some(b"Hello from storm \xE2\x9C\x93"[..].into());
    created.meta.created_by = Some(b"storm-tests/1.0"[..].into());
    created.meta.creation_date = Some(1_700_000_000);
    let torrent = created.as_bytes().unwrap().to_vec();

    let data = tmp.path();
    let backend = start(data).await;
    let mut req = AddTorrentRequest::file(torrent);
    req.output_folder = Some(src.to_string_lossy().into_owned());
    let added = backend.add_torrent(req).await.unwrap();

    let check = |d: storm_proto::TorrentDetails| {
        assert_eq!(d.comment.as_deref(), Some("Hello from storm \u{2713}"));
        assert_eq!(d.created_by.as_deref(), Some("storm-tests/1.0"));
        assert_eq!(d.creation_date, Some(1_700_000_000));
    };
    check(backend.torrent_details(added.id).await.unwrap());

    // A torrent with no peers has an empty (but valid) peer list.
    assert!(backend.torrent_peers(added.id).await.unwrap().is_empty());
    backend.shutdown().await;
    drop(backend);

    let backend = start(data).await;
    let mut details = None;
    for _ in 0..100 {
        if let Some(t) = backend.list_torrents().await.unwrap().first() {
            if let Ok(d) = backend.torrent_details(t.id).await {
                if d.piece_count.is_some() {
                    details = Some(d);
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    check(details.expect("torrent not restored after restart"));
    backend.shutdown().await;
}

/// Torrents created without the optional fields report `None`, not empty strings.
#[tokio::test(flavor = "multi_thread")]
async fn details_without_comment_are_none() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("plain.bin");
    std::fs::write(&file, vec![1u8; 50_000]).unwrap();
    let mut created = librqbit::create_torrent(
        &file,
        librqbit::CreateTorrentOptions { name: None, trackers: vec![], piece_length: None },
        &librqbit::spawn_utils::BlockingSpawner::new(1),
    )
    .await
    .unwrap();
    created.meta.comment = Some(b"   "[..].into());
    let backend = start(tmp.path()).await;
    let mut req = AddTorrentRequest::file(created.as_bytes().unwrap().to_vec());
    req.output_folder = Some(tmp.path().to_string_lossy().into_owned());
    let added = backend.add_torrent(req).await.unwrap();
    let d = backend.torrent_details(added.id).await.unwrap();
    assert_eq!((d.comment, d.created_by, d.creation_date), (None, None, None));
    backend.shutdown().await;
}

/// Sequential mode and file priorities apply right away, show up in the summary and
/// details, and survive a restart (storm keeps them; librqbit doesn't persist them).
#[tokio::test(flavor = "multi_thread")]
async fn piece_picking_settings_apply_and_persist() {
    use storm_proto::FilePriority;

    let tmp = tempfile::tempdir().unwrap();
    let content = tmp.path().join("seed").join("show");
    std::fs::create_dir_all(&content).unwrap();
    std::fs::write(content.join("e01.mkv"), vec![1u8; 200_000]).unwrap();
    std::fs::write(content.join("e02.mkv"), vec![2u8; 200_000]).unwrap();
    let torrent = librqbit::create_torrent(
        &content,
        librqbit::CreateTorrentOptions { name: None, trackers: vec![], piece_length: None },
        &librqbit::spawn_utils::BlockingSpawner::new(1),
    )
    .await
    .unwrap()
    .as_bytes()
    .unwrap()
    .to_vec();

    let backend = start(tmp.path()).await;
    let mut req = AddTorrentRequest::file(torrent);
    req.sequential = true;
    req.paused = true;
    let id = backend.add_torrent(req).await.unwrap().id;
    assert!(backend.list_torrents().await.unwrap()[0].sequential, "requested at add time");

    let files = backend.torrent_details(id).await.unwrap().files;
    assert!(files.iter().all(|f| f.priority == FilePriority::Normal));
    let e02 = files.iter().find(|f| f.path.ends_with("e02.mkv")).unwrap().index;
    backend.set_file_priority(id, vec![e02], FilePriority::High).await.unwrap();
    assert!(backend.set_file_priority(id, vec![99], FilePriority::Low).await.is_err(), "bad index");
    backend.set_sequential(id, false).await.unwrap();
    backend.set_sequential(id, true).await.unwrap();

    let check = |d: storm_proto::TorrentDetails| {
        assert!(d.summary.sequential);
        for f in d.files {
            let want = if f.index == e02 { FilePriority::High } else { FilePriority::Normal };
            assert_eq!(f.priority, want, "{}", f.path);
        }
    };
    check(backend.torrent_details(id).await.unwrap());
    backend.shutdown().await;
    drop(backend);

    let backend = start(tmp.path()).await;
    let id = backend.list_torrents().await.unwrap()[0].id;
    check(backend.torrent_details(id).await.unwrap());

    // Back to the defaults: the stored row goes away, and that sticks too.
    backend.set_sequential(id, false).await.unwrap();
    backend.set_file_priority(id, vec![e02], FilePriority::Normal).await.unwrap();
    backend.shutdown().await;
    drop(backend);
    let backend = start(tmp.path()).await;
    let d = backend.torrent_details(backend.list_torrents().await.unwrap()[0].id).await.unwrap();
    assert!(!d.summary.sequential);
    assert!(d.files.iter().all(|f| f.priority == FilePriority::Normal));
    backend.shutdown().await;
}

/// Sequential can be toggled while a magnet is still fetching metadata.
#[tokio::test(flavor = "multi_thread")]
async fn sequential_toggles_on_pending_magnets() {
    let tmp = tempfile::tempdir().unwrap();
    let backend = start(tmp.path()).await;
    let magnet = "magnet:?xt=urn:btih:89abcdef0123456789abcdef0123456789abcdef&dn=Later";
    let id = backend.add_torrent(AddTorrentRequest::url(magnet)).await.unwrap().id;
    assert!(!backend.list_torrents().await.unwrap()[0].sequential);
    backend.set_sequential(id, true).await.unwrap();
    assert!(backend.list_torrents().await.unwrap()[0].sequential);
    assert!(backend.set_file_priority(id, vec![0], storm_proto::FilePriority::High).await.is_err(), "no file list yet");
    backend.remove(id, false).await.unwrap();
    backend.shutdown().await;
}
