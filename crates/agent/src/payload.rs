use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use proxy_tester_domain::{PayloadKind, PayloadProfile, RandomFormat, Scenario};
use rand::RngCore;
use sha2::{Digest, Sha256};
use tracing::info;
use uuid::Uuid;

#[derive(Clone)]
pub(crate) struct PreparedPayloads {
    pub(crate) request: Arc<[u8]>,
    pub(crate) response: Arc<[u8]>,
}

impl PreparedPayloads {
    pub(crate) fn new(
        scenario: &Scenario,
        artifacts: &HashMap<Uuid, CompletedArtifact>,
    ) -> Result<Self> {
        Ok(Self {
            request: materialize(&scenario.request_payload, artifacts)?,
            response: materialize(&scenario.response_payload, artifacts)?,
        })
    }
}

pub(crate) enum CompletedArtifact {
    Payload(Arc<[u8]>),
    Capture(PathBuf),
}

fn materialize(
    profile: &PayloadProfile,
    artifacts: &HashMap<Uuid, CompletedArtifact>,
) -> Result<Arc<[u8]>> {
    let bytes = match profile.kind {
        PayloadKind::Empty => Vec::new(),
        PayloadKind::Fixed => vec![0; profile.size_bytes],
        PayloadKind::Text => profile.text.as_bytes().to_vec(),
        PayloadKind::Random => {
            let mut data = vec![0; profile.size_bytes];
            rand::rng().fill_bytes(&mut data);
            if profile.random_format == RandomFormat::PrintableAscii {
                for byte in &mut data {
                    *byte = 0x20 + (*byte % 95);
                }
            }
            data
        }
        PayloadKind::File => {
            let id = profile.artifact_id.context("file payload artifact ID")?;
            return artifacts
                .get(&id)
                .and_then(|artifact| match artifact {
                    CompletedArtifact::Payload(bytes) => Some(bytes.clone()),
                    CompletedArtifact::Capture(_) => None,
                })
                .with_context(|| format!("artifact {id} was not transferred"));
        }
    };
    info!(
        kind = ?profile.kind,
        bytes = bytes.len(),
        sha256 = %format!("{:x}", Sha256::digest(&bytes)),
        "payload prepared"
    );
    Ok(bytes.into())
}
