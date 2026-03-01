use anyhow::Result;
use indicatif::MultiProgress;
use std::fs;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tempfile::TempDir;
use transfer_plan::queue::TransferQueue;
use transfer_plan::worker::transfer_worker_pool;

fn prepare_roots() -> Result<(TempDir, std::path::PathBuf, std::path::PathBuf)> {
    let temp = tempfile::tempdir()?;
    let source_root = temp.path().join("source");
    let destination_root = temp.path().join("destination");
    fs::create_dir_all(&source_root)?;
    fs::create_dir_all(&destination_root)?;
    Ok((temp, source_root, destination_root))
}

#[tokio::test]
async fn end_to_end_transfer_copies_files_and_keeps_sources() -> Result<()> {
    let (_temp, source_root, destination_root) = prepare_roots()?;

    let source_file = source_root.join("hello.txt");
    fs::write(&source_file, b"hello transfer-plan")?;

    let source_dir = source_root.join("album");
    fs::create_dir_all(&source_dir)?;
    let nested_file = source_dir.join("photo.bin");
    fs::write(&nested_file, vec![7u8; 4096])?;

    let queue = Arc::new(TransferQueue::new());
    queue.add_file_with_policy(source_file.clone(), &destination_root, false)?;
    queue.add_directory_with_policy(&source_dir, &destination_root, false)?;

    let outcome = transfer_worker_pool(
        queue.clone(),
        MultiProgress::new(),
        Arc::new(AtomicBool::new(false)),
        2,
        true,
        "none".to_string(),
    )
    .await?;

    assert!(!outcome.stopped_by_user);
    assert_eq!(outcome.files_failed, 0);
    assert_eq!(outcome.files_completed, 2);
    assert!(queue.is_empty());

    assert_eq!(
        fs::read(destination_root.join("hello.txt"))?,
        b"hello transfer-plan"
    );
    assert_eq!(
        fs::read(destination_root.join("album").join("photo.bin"))?,
        vec![7u8; 4096]
    );

    // cleanup mode "none" must keep sources
    assert!(source_file.exists());
    assert!(nested_file.exists());

    Ok(())
}

#[tokio::test]
async fn end_to_end_transfer_with_delete_cleanup_removes_sources() -> Result<()> {
    let (_temp, source_root, destination_root) = prepare_roots()?;

    let source_file = source_root.join("delete_me.txt");
    fs::write(&source_file, b"delete after transfer")?;

    let queue = Arc::new(TransferQueue::new());
    queue.add_file_with_policy(source_file.clone(), &destination_root, false)?;

    let outcome = transfer_worker_pool(
        queue.clone(),
        MultiProgress::new(),
        Arc::new(AtomicBool::new(false)),
        1,
        false,
        "delete".to_string(),
    )
    .await?;

    assert!(!outcome.stopped_by_user);
    assert_eq!(outcome.files_failed, 0);
    assert_eq!(outcome.files_completed, 1);
    assert!(queue.is_empty());

    assert_eq!(
        fs::read(destination_root.join("delete_me.txt"))?,
        b"delete after transfer"
    );
    assert!(!source_file.exists());

    Ok(())
}

#[tokio::test]
async fn folder_transfer_is_alphabetical_and_removes_empty_source_folder() -> Result<()> {
    let (_temp, source_root, destination_root) = prepare_roots()?;

    let source_dir = source_root.join("docs");
    fs::create_dir_all(source_dir.join("nested"))?;
    fs::write(source_dir.join("zeta.txt"), b"zeta")?;
    fs::write(source_dir.join("alpha.txt"), b"alpha")?;
    fs::write(source_dir.join("nested").join("beta.txt"), b"beta")?;

    let queue = Arc::new(TransferQueue::new());
    queue.add_directory_with_policy(&source_dir, &destination_root, false)?;

    let ordered = queue
        .snapshot_items()
        .into_iter()
        .map(|item| {
            item.source
                .strip_prefix(&source_dir)
                .expect("source must remain under root")
                .to_string_lossy()
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ordered,
        vec![
            "alpha.txt".to_string(),
            "nested/beta.txt".to_string(),
            "zeta.txt".to_string()
        ]
    );

    let outcome = transfer_worker_pool(
        queue.clone(),
        MultiProgress::new(),
        Arc::new(AtomicBool::new(false)),
        1,
        false,
        "delete".to_string(),
    )
    .await?;

    assert!(!outcome.stopped_by_user);
    assert_eq!(outcome.files_failed, 0);
    assert_eq!(outcome.files_completed, 3);
    assert!(queue.is_empty());
    assert!(!source_dir.exists());

    assert_eq!(
        fs::read(destination_root.join("docs").join("alpha.txt"))?,
        b"alpha"
    );
    assert_eq!(
        fs::read(
            destination_root
                .join("docs")
                .join("nested")
                .join("beta.txt")
        )?,
        b"beta"
    );
    assert_eq!(
        fs::read(destination_root.join("docs").join("zeta.txt"))?,
        b"zeta"
    );

    Ok(())
}
