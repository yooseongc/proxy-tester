use proxy_tester_proto::{
    network_draft_to_wire,
    v1::{
        CommandSpec, EmptyNetworkAction, EndpointPlan, NetworkPlan, PlanNetwork, StageNetwork,
        network_command::Action,
    },
};

use crate::error::ApiError;

pub(crate) fn network_action(action: &str, payload: serde_json::Value) -> Result<Action, ApiError> {
    Ok(match action {
        "plan" => Action::Plan(PlanNetwork {
            profile_revision_id: payload["profile_revision_id"]
                .as_str()
                .ok_or_else(|| ApiError::internal("plan revision is missing"))?
                .into(),
            draft: Some(network_draft_to_wire(serde_json::from_value(
                payload["draft"].clone(),
            )?)),
        }),
        "stage" => Action::Stage(StageNetwork {
            plan: Some(json_to_plan(payload)?),
        }),
        "commit" => Action::Commit(EmptyNetworkAction {}),
        "rollback" => Action::Rollback(EmptyNetworkAction {}),
        "teardown" => Action::Teardown(EmptyNetworkAction {}),
        "reconcile" => Action::Reconcile(EmptyNetworkAction {}),
        _ => {
            return Err(ApiError::internal(format!(
                "unknown network action {action}"
            )));
        }
    })
}

fn json_to_plan(value: serde_json::Value) -> Result<NetworkPlan, ApiError> {
    let commands = |name: &str| -> Result<Vec<CommandSpec>, ApiError> {
        Ok(value[name]
            .as_array()
            .ok_or_else(|| ApiError::internal(format!("plan {name} is missing")))?
            .iter()
            .map(|command| CommandSpec {
                program: command["program"].as_str().unwrap_or_default().into(),
                args: string_array(command, "args"),
            })
            .collect())
    };
    Ok(NetworkPlan {
        profile_revision_id: string_field(&value, "profile_revision_id"),
        node_id: string_field(&value, "node_id"),
        inventory_fingerprint: string_field(&value, "inventory_fingerprint"),
        endpoints: value["endpoints"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|endpoint| EndpointPlan {
                role: string_field(endpoint, "role"),
                namespace: string_field(endpoint, "namespace"),
                interface: string_field(endpoint, "interface"),
                addresses: string_array(endpoint, "addresses"),
            })
            .collect(),
        semantic_changes: string_array(&value, "semantic_changes"),
        commands: commands("commands")?,
        rollback_commands: commands("rollback_commands")?,
        warnings: string_array(&value, "warnings"),
    })
}

fn string_field(value: &serde_json::Value, name: &str) -> String {
    value[name].as_str().unwrap_or_default().into()
}

fn string_array(value: &serde_json::Value, name: &str) -> Vec<String> {
    value[name]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str().map(str::to_owned))
        .collect()
}

pub(crate) fn plan_to_json(value: NetworkPlan) -> serde_json::Value {
    serde_json::json!({
        "profile_revision_id": value.profile_revision_id,
        "node_id": value.node_id,
        "inventory_fingerprint": value.inventory_fingerprint,
        "endpoints": value.endpoints.into_iter().map(|endpoint| serde_json::json!({"role":endpoint.role,"namespace":endpoint.namespace,"interface":endpoint.interface,"addresses":endpoint.addresses})).collect::<Vec<_>>(),
        "semantic_changes": value.semantic_changes,
        "commands": value.commands.into_iter().map(|command| serde_json::json!({"program":command.program,"args":command.args})).collect::<Vec<_>>(),
        "rollback_commands": value.rollback_commands.into_iter().map(|command| serde_json::json!({"program":command.program,"args":command.args})).collect::<Vec<_>>(),
        "warnings": value.warnings,
    })
}

pub(crate) fn inventory_to_json(value: proxy_tester_proto::v1::NodeInventory) -> serde_json::Value {
    serde_json::json!({
        "interfaces": value.interfaces.into_iter().map(|interface| serde_json::json!({"name":interface.name,"mac":interface.mac,"mtu":interface.mtu,"state":interface.state,"master":interface.master,"addresses":interface.addresses,"link_up":interface.link_up,"offloads":interface.offloads})).collect::<Vec<_>>(),
        "capabilities": value.capabilities,
        "protected_interfaces": value.protected_interfaces,
        "fingerprint": value.fingerprint,
    })
}
