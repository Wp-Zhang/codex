use super::*;
use pretty_assertions::assert_eq;

#[cfg(unix)]
#[tokio::test]
async fn concurrent_publication_is_content_addressed_and_never_replaces_existing_inode() {
    use std::os::unix::fs::MetadataExt;
    let home = tempfile::tempdir().unwrap();
    let bytes =
        serde_json::to_vec(&serde_json::json!({"retained_output":"real result ".repeat(1024)}))
            .unwrap();
    let (first, second) = tokio::join!(
        store_panng_result_blob(
            home.path(),
            ResultEncoding::CodexResponseEnvelopeV1,
            bytes.clone()
        ),
        store_panng_result_blob(
            home.path(),
            ResultEncoding::CodexResponseEnvelopeV1,
            bytes.clone()
        ),
    );
    let first = first.unwrap();
    assert_eq!(first, second.unwrap());
    let (_, digest, _) = first.artifact_reference().unwrap();
    let path = home.path().join(NAMESPACE).join(digest.as_str());
    let inode = fs::metadata(&path).unwrap().ino();
    assert_eq!(
        store_panng_result_blob(
            home.path(),
            ResultEncoding::CodexResponseEnvelopeV1,
            bytes.clone()
        )
        .await
        .unwrap(),
        first
    );
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(
        load_panng_result_blob(home.path(), &first).await.unwrap(),
        bytes
    );
    assert_eq!(
        fs::read_dir(home.path().join(NAMESPACE)).unwrap().count(),
        1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn corrupted_blob_is_rejected_by_reads_and_duplicate_publication_without_overwrite() {
    let home = tempfile::tempdir().unwrap();
    let original = b"retained result bytes".to_vec();
    let reference = store_panng_result_blob(
        home.path(),
        ResultEncoding::CodexResponseEnvelopeV1,
        original.clone(),
    )
    .await
    .unwrap();
    let (_, digest, length) = reference.artifact_reference().unwrap();
    let path = home.path().join(NAMESPACE).join(digest.as_str());
    let corrupt = vec![b'x'; original.len()];
    fs::write(&path, &corrupt).unwrap();
    assert!(
        load_panng_result_blob(home.path(), &reference)
            .await
            .is_err()
    );
    assert!(
        store_panng_result_blob(
            home.path(),
            ResultEncoding::CodexResponseEnvelopeV1,
            original.clone()
        )
        .await
        .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), corrupt);
    fs::write(&path, &original).unwrap();
    let wrong_length = StoredResult::artifact(
        ResultEncoding::CodexResponseEnvelopeV1,
        digest.clone(),
        length - 1,
    )
    .unwrap();
    assert!(
        load_panng_result_blob(home.path(), &wrong_length)
            .await
            .is_err()
    );
    fs::remove_file(path).unwrap();
    assert!(
        load_panng_result_blob(home.path(), &reference)
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn exact_contract_limit_round_trips_and_oversize_does_not_publish() {
    let home = tempfile::tempdir().unwrap();
    assert!(
        store_panng_result_blob(
            home.path(),
            ResultEncoding::CodexResponseEnvelopeV1,
            vec![0; MAX_BLOB_BYTES + 1]
        )
        .await
        .is_err()
    );
    assert!(!home.path().join(NAMESPACE).exists());
    let bytes = vec![b'a'; MAX_BLOB_BYTES];
    let reference = store_panng_result_blob(
        home.path(),
        ResultEncoding::CodexResponseEnvelopeV1,
        bytes.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        load_panng_result_blob(home.path(), &reference)
            .await
            .unwrap(),
        bytes
    );
}

#[cfg(unix)]
#[tokio::test]
async fn namespace_and_artifact_symlinks_are_not_followed() {
    use std::os::unix::fs::symlink;
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), home.path().join(NAMESPACE)).unwrap();
    assert!(
        store_panng_result_blob(
            home.path(),
            ResultEncoding::CodexResponseEnvelopeV1,
            b"bytes".to_vec()
        )
        .await
        .is_err()
    );
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    fs::remove_file(home.path().join(NAMESPACE)).unwrap();
    let reference = store_panng_result_blob(
        home.path(),
        ResultEncoding::CodexResponseEnvelopeV1,
        b"bytes".to_vec(),
    )
    .await
    .unwrap();
    let (_, digest, _) = reference.artifact_reference().unwrap();
    let target = home.path().join(NAMESPACE).join(digest.as_str());
    let outside_file = outside.path().join("data");
    fs::rename(&target, &outside_file).unwrap();
    symlink(&outside_file, &target).unwrap();
    assert!(
        load_panng_result_blob(home.path(), &reference)
            .await
            .is_err()
    );
    assert!(
        store_panng_result_blob(
            home.path(),
            ResultEncoding::CodexResponseEnvelopeV1,
            b"bytes".to_vec()
        )
        .await
        .is_err()
    );
    assert_eq!(fs::read(outside_file).unwrap(), b"bytes");
}

#[tokio::test]
async fn inline_and_unknown_are_not_interpreted_as_paths() {
    let home = tempfile::tempdir().unwrap();
    let inline =
        StoredResult::inline(ResultEncoding::CodexResponseEnvelopeV1, "{}".into()).unwrap();
    let unknown =
        StoredResult::unknown(codex_panng_runtime::UnknownResultReason::ExecutorDisconnected);
    assert!(load_panng_result_blob(home.path(), &inline).await.is_err());
    assert!(load_panng_result_blob(home.path(), &unknown).await.is_err());
}

#[cfg(not(unix))]
#[tokio::test]
async fn unsupported_sync_platform_rejects_before_filesystem_changes() {
    let home = tempfile::tempdir().unwrap();
    let error = store_panng_result_blob(
        home.path(),
        ResultEncoding::CodexResponseEnvelopeV1,
        b"bytes".to_vec(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert!(!home.path().join(NAMESPACE).exists());
}
