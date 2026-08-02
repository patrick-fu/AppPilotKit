use apppilotkit_rust_foundation_spike::{ArtifactError, write_artifact};
use std::io::Cursor;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn artifact_is_hashed_synced_and_published_without_partial_files() {
    let directory = tempfile::tempdir().expect("artifact directory");
    let destination = directory.path().join("snapshot.json");
    let receipt = write_artifact(
        &destination,
        Cursor::new(b"abc".to_vec()),
        CancellationToken::new(),
    )
    .await
    .expect("artifact should publish");

    assert_eq!(receipt.bytes, 3);
    assert_eq!(
        receipt.sha256,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert!(receipt.directory_synced);
    assert_eq!(
        std::fs::read(&destination).expect("artifact contents"),
        b"abc"
    );
    assert_eq!(
        std::fs::read_dir(directory.path())
            .expect("artifact directory")
            .count(),
        1
    );
}

#[tokio::test]
async fn existing_destination_is_never_replaced() {
    let directory = tempfile::tempdir().expect("artifact directory");
    let destination = directory.path().join("snapshot.json");
    std::fs::write(&destination, b"existing").expect("existing artifact");
    let result = write_artifact(
        &destination,
        Cursor::new(b"replacement".to_vec()),
        CancellationToken::new(),
    )
    .await;

    assert!(matches!(result, Err(ArtifactError::DestinationExists)));
    assert_eq!(
        std::fs::read(&destination).expect("existing artifact contents"),
        b"existing"
    );
    assert_eq!(
        std::fs::read_dir(directory.path())
            .expect("artifact directory")
            .count(),
        1
    );
}

#[tokio::test]
async fn cancellation_removes_the_sibling_partial_and_publishes_nothing() {
    let directory = tempfile::tempdir().expect("artifact directory");
    let destination = directory.path().join("snapshot.json");
    let (_writer, reader) = tokio::io::duplex(64);
    let cancellation = CancellationToken::new();
    let trigger = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.cancel();
    });
    let result = write_artifact(&destination, reader, cancellation).await;

    assert!(matches!(result, Err(ArtifactError::Cancelled)));
    assert!(!destination.exists());
    assert_eq!(
        std::fs::read_dir(directory.path())
            .expect("artifact directory")
            .count(),
        0
    );
}

#[tokio::test]
async fn concurrent_publishers_cannot_clobber_each_other() {
    let directory = tempfile::tempdir().expect("artifact directory");
    let destination = directory.path().join("snapshot.json");
    let first_destination = destination.clone();
    let second_destination = destination.clone();
    let first = tokio::spawn(async move {
        write_artifact(
            &first_destination,
            Cursor::new(b"first".to_vec()),
            CancellationToken::new(),
        )
        .await
    });
    let second = tokio::spawn(async move {
        write_artifact(
            &second_destination,
            Cursor::new(b"second".to_vec()),
            CancellationToken::new(),
        )
        .await
    });
    let results = [
        first.await.expect("first writer"),
        second.await.expect("second writer"),
    ];

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(ArtifactError::DestinationExists)))
            .count(),
        1
    );
    let contents = std::fs::read(&destination).expect("published artifact");
    assert!(contents == b"first" || contents == b"second");
}
