use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

#[derive(Debug, PartialEq, Eq)]
pub struct ArtifactReceipt {
    pub bytes: u64,
    pub sha256: String,
    pub directory_synced: bool,
}

#[derive(Debug)]
pub enum ArtifactError {
    Cancelled,
    DestinationExists,
    Io(std::io::Error),
}

impl std::fmt::Display for ArtifactError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("artifact write was cancelled"),
            Self::DestinationExists => formatter.write_str("artifact destination already exists"),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ArtifactError {}

pub async fn write_artifact(
    destination: &Path,
    mut source: impl AsyncRead + Unpin,
    cancellation: CancellationToken,
) -> Result<ArtifactReceipt, ArtifactError> {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact");
    let temporary = tempfile::Builder::new()
        .prefix(&format!(".{file_name}.partial."))
        .tempfile_in(parent)
        .map_err(ArtifactError::Io)?;
    let writer = temporary.reopen().map_err(ArtifactError::Io)?;
    let mut writer = tokio::fs::File::from_std(writer);
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];

    loop {
        let read = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ArtifactError::Cancelled),
            result = source.read(&mut buffer) => result.map_err(ArtifactError::Io)?,
        };
        if read == 0 {
            break;
        }
        writer
            .write_all(&buffer[..read])
            .await
            .map_err(ArtifactError::Io)?;
        hasher.update(&buffer[..read]);
        bytes += read as u64;
    }

    if cancellation.is_cancelled() {
        return Err(ArtifactError::Cancelled);
    }
    writer.flush().await.map_err(ArtifactError::Io)?;
    writer.sync_all().await.map_err(ArtifactError::Io)?;
    drop(writer);
    if cancellation.is_cancelled() {
        return Err(ArtifactError::Cancelled);
    }

    let published = temporary.persist_noclobber(destination).map_err(|error| {
        if error.error.kind() == std::io::ErrorKind::AlreadyExists {
            ArtifactError::DestinationExists
        } else {
            ArtifactError::Io(error.error)
        }
    })?;
    drop(published);
    let directory_synced = std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .is_ok();
    let digest = hasher.finalize();

    Ok(ArtifactReceipt {
        bytes,
        sha256: digest.iter().map(|byte| format!("{byte:02x}")).collect(),
        directory_synced,
    })
}
