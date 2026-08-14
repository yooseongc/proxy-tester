use proxy_tester_domain::{EndpointProfile, NetworkProfileDraft};
use thiserror::Error;

use crate::v1;

#[derive(Debug, Error)]
pub enum ConversionError {
    #[error("network profile ID is invalid: {0}")]
    InvalidProfileId(#[from] uuid::Error),
    #[error("network profile is missing the {0} endpoint")]
    MissingEndpoint(&'static str),
    #[error("diagnostic port is outside the u16 range: {0}")]
    InvalidDiagnosticPort(u32),
}

fn endpoint_to_wire(value: &EndpointProfile) -> v1::EndpointProfile {
    v1::EndpointProfile {
        node_id: value.node_id.clone(),
        interface_name: value.interface_name.clone(),
        start_cidr: value.start_cidr.clone(),
        count: value.count,
    }
}

fn endpoint_from_wire(value: v1::EndpointProfile) -> EndpointProfile {
    EndpointProfile {
        node_id: value.node_id,
        interface_name: value.interface_name,
        start_cidr: value.start_cidr,
        count: value.count,
    }
}

pub fn network_draft_to_wire(value: NetworkProfileDraft) -> v1::NetworkProfileDraft {
    v1::NetworkProfileDraft {
        id: value.id.to_string(),
        name: value.name,
        client_endpoint: Some(endpoint_to_wire(&value.client_endpoint)),
        server_endpoint: Some(endpoint_to_wire(&value.server_endpoint)),
        mtu: value.mtu,
        diagnostic_port: value.diagnostic_port.into(),
        path_probe_enabled: value.path_probe_enabled,
    }
}

pub fn network_draft_from_wire(
    value: v1::NetworkProfileDraft,
) -> Result<NetworkProfileDraft, ConversionError> {
    Ok(NetworkProfileDraft {
        id: uuid::Uuid::parse_str(&value.id)?,
        name: value.name,
        client_endpoint: endpoint_from_wire(
            value
                .client_endpoint
                .ok_or(ConversionError::MissingEndpoint("client"))?,
        ),
        server_endpoint: endpoint_from_wire(
            value
                .server_endpoint
                .ok_or(ConversionError::MissingEndpoint("server"))?,
        ),
        mtu: value.mtu,
        diagnostic_port: u16::try_from(value.diagnostic_port)
            .map_err(|_| ConversionError::InvalidDiagnosticPort(value.diagnostic_port))?,
        path_probe_enabled: value.path_probe_enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_draft_round_trips() {
        let draft = NetworkProfileDraft::default();
        let decoded = network_draft_from_wire(network_draft_to_wire(draft.clone())).unwrap();
        assert_eq!(decoded, draft);
    }

    #[test]
    fn missing_endpoint_has_field_context() {
        let error = network_draft_from_wire(v1::NetworkProfileDraft {
            id: uuid::Uuid::new_v4().to_string(),
            ..Default::default()
        })
        .unwrap_err();
        assert!(error.to_string().contains("client endpoint"));
    }
}
