use std::collections::HashMap;

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::payload::CompletedArtifact;

pub(crate) struct IncomingArtifact {
    path: std::path::PathBuf,
    file: tokio::fs::File,
    received: u64,
    digest: Sha256,
    total_size: u64,
    sha256: String,
    kind: String,
}

#[allow(clippy::map_entry)]
pub(crate) async fn accept_chunk(
    incoming: &mut HashMap<Uuid, IncomingArtifact>,
    completed: &mut HashMap<Uuid, CompletedArtifact>,
    chunk: proxy_tester_proto::v1::ArtifactChunk,
) -> anyhow::Result<()> {
    let id = Uuid::parse_str(&chunk.artifact_id)?;
    let limit = if chunk.artifact_kind == "pcap" {
        512 * 1024 * 1024
    } else {
        proxy_tester_domain::MAX_PAYLOAD_BYTES as u64
    };
    if chunk.total_size > limit {
        bail!("artifact {id} exceeds the agent limit");
    }
    if completed.contains_key(&id) {
        // Artifact IDs are immutable in Control. A later run can reference the
        // same artifact, so reuse the verified local copy and consume the
        // repeated stream idempotently instead of terminating the gRPC session.
        return Ok(());
    }
    if !incoming.contains_key(&id) && chunk.offset != 0 {
        bail!("artifact {id} first chunk offset must be zero");
    }
    if !incoming.contains_key(&id) {
        let temporary_dir = std::env::temp_dir();
        tokio::fs::create_dir_all(&temporary_dir)
            .await
            .with_context(|| format!("create temporary directory {temporary_dir:?}"))?;
        let path = temporary_dir.join(format!("proxy-tester-{id}.artifact.part"));
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .with_context(|| format!("create temporary artifact {path:?}"))?;
        incoming.insert(
            id,
            IncomingArtifact {
                path,
                file,
                received: 0,
                digest: Sha256::new(),
                total_size: chunk.total_size,
                sha256: chunk.sha256.clone(),
                kind: chunk.artifact_kind.clone(),
            },
        );
    }
    let state = incoming.get_mut(&id).context("artifact transfer state")?;
    if state.total_size != chunk.total_size
        || state.sha256 != chunk.sha256
        || state.kind != chunk.artifact_kind
    {
        bail!("artifact {id} metadata changed during transfer");
    }
    if chunk.offset != state.received {
        bail!("artifact {id} chunk offset mismatch");
    }
    if state.received.saturating_add(chunk.data.len() as u64) > state.total_size {
        bail!("artifact {id} exceeds declared size");
    }
    state.file.write_all(&chunk.data).await?;
    state.digest.update(&chunk.data);
    state.received += chunk.data.len() as u64;
    if chunk.eof {
        finish(id, incoming, completed).await?;
    }
    Ok(())
}

async fn finish(
    id: Uuid,
    incoming: &mut HashMap<Uuid, IncomingArtifact>,
    completed: &mut HashMap<Uuid, CompletedArtifact>,
) -> anyhow::Result<()> {
    let state = incoming.get_mut(&id).context("artifact transfer state")?;
    if state.received != state.total_size {
        bail!("artifact {id} ended before declared size");
    }
    state.file.flush().await?;
    let finished = incoming.remove(&id).context("artifact transfer state")?;
    let actual = format!("{:x}", finished.digest.finalize());
    if actual != finished.sha256 {
        let _ = tokio::fs::remove_file(&finished.path).await;
        bail!("artifact {id} SHA-256 mismatch");
    }
    if finished.kind == "pcap" {
        completed.insert(id, CompletedArtifact::Capture(finished.path));
    } else {
        let bytes = tokio::fs::read(&finished.path).await?;
        tokio::fs::remove_file(&finished.path).await?;
        completed.insert(id, CompletedArtifact::Payload(bytes.into()));
    }
    Ok(())
}
