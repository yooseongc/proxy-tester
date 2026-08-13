use anyhow::{Context, bail};
use proxy_tester_domain::{EndpointProfile, NetworkProfileDraft};
use proxy_tester_proto::v1 as wire;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, process::Stdio};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeInventory {
    pub interfaces: Vec<InterfaceInventory>,
    pub capabilities: BTreeMap<String, bool>,
    pub protected_interfaces: Vec<String>,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InterfaceInventory {
    pub name: String,
    pub mac: Option<String>,
    pub mtu: Option<u32>,
    pub state: Option<String>,
    pub master: Option<String>,
    pub addresses: Vec<String>,
    pub link_up: bool,
    pub offloads: BTreeMap<String, bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkPlan {
    pub profile_revision_id: String,
    pub node_id: String,
    pub inventory_fingerprint: String,
    pub endpoints: Vec<EndpointPlan>,
    pub semantic_changes: Vec<String>,
    pub commands: Vec<CommandSpec>,
    pub rollback_commands: Vec<CommandSpec>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EndpointPlan {
    pub role: String,
    pub namespace: String,
    pub interface: String,
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
}
impl CommandSpec {
    fn new(program: &str, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkJournal {
    pub operation_id: String,
    pub profile_revision_id: String,
    pub phase: String,
    pub lease_expires_unix_ms: i64,
    pub rollback_commands: Vec<CommandSpec>,
    pub completed_commands: usize,
}

#[derive(Clone)]
pub struct NetworkManager {
    journal_path: PathBuf,
}

impl From<NodeInventory> for wire::NodeInventory {
    fn from(value: NodeInventory) -> Self {
        Self {
            interfaces: value
                .interfaces
                .into_iter()
                .map(|v| wire::InterfaceInventory {
                    name: v.name,
                    mac: v.mac,
                    mtu: v.mtu,
                    state: v.state,
                    master: v.master,
                    addresses: v.addresses,
                    link_up: v.link_up,
                    offloads: v.offloads.into_iter().collect(),
                })
                .collect(),
            capabilities: value.capabilities.into_iter().collect(),
            protected_interfaces: value.protected_interfaces,
            fingerprint: value.fingerprint,
        }
    }
}
impl From<NetworkPlan> for wire::NetworkPlan {
    fn from(value: NetworkPlan) -> Self {
        Self {
            profile_revision_id: value.profile_revision_id,
            node_id: value.node_id,
            inventory_fingerprint: value.inventory_fingerprint,
            endpoints: value
                .endpoints
                .into_iter()
                .map(|v| wire::EndpointPlan {
                    role: v.role,
                    namespace: v.namespace,
                    interface: v.interface,
                    addresses: v.addresses,
                })
                .collect(),
            semantic_changes: value.semantic_changes,
            commands: value
                .commands
                .into_iter()
                .map(|v| wire::CommandSpec {
                    program: v.program,
                    args: v.args,
                })
                .collect(),
            rollback_commands: value
                .rollback_commands
                .into_iter()
                .map(|v| wire::CommandSpec {
                    program: v.program,
                    args: v.args,
                })
                .collect(),
            warnings: value.warnings,
        }
    }
}
impl From<wire::NetworkPlan> for NetworkPlan {
    fn from(value: wire::NetworkPlan) -> Self {
        Self {
            profile_revision_id: value.profile_revision_id,
            node_id: value.node_id,
            inventory_fingerprint: value.inventory_fingerprint,
            endpoints: value
                .endpoints
                .into_iter()
                .map(|v| EndpointPlan {
                    role: v.role,
                    namespace: v.namespace,
                    interface: v.interface,
                    addresses: v.addresses,
                })
                .collect(),
            semantic_changes: value.semantic_changes,
            commands: value
                .commands
                .into_iter()
                .map(|v| CommandSpec {
                    program: v.program,
                    args: v.args,
                })
                .collect(),
            rollback_commands: value
                .rollback_commands
                .into_iter()
                .map(|v| CommandSpec {
                    program: v.program,
                    args: v.args,
                })
                .collect(),
            warnings: value.warnings,
        }
    }
}
impl NetworkManager {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            journal_path: path.into(),
        }
    }

    pub async fn inventory(&self) -> anyhow::Result<NodeInventory> {
        let ip = command_output("ip", &["-j", "link", "show"]).await;
        let addresses = command_output("ip", &["-j", "address", "show"]).await;
        let mut capabilities = BTreeMap::new();
        capabilities.insert("ip".into(), ip.is_ok());
        capabilities.insert(
            "ethtool".into(),
            command_status("ethtool", &["--version"]).await,
        );
        capabilities.insert("tc".into(), command_status("tc", &["-V"]).await);
        capabilities.insert("arping".into(), command_status("arping", &["-V"]).await);
        capabilities.insert(
            "ovs".into(),
            command_status("ovs-vsctl", &["--version"]).await,
        );
        let mut interfaces = Vec::new();
        if let Ok(raw) = ip {
            for value in serde_json::from_str::<Vec<serde_json::Value>>(&raw).unwrap_or_default() {
                interfaces.push(InterfaceInventory {
                    name: value["ifname"].as_str().unwrap_or_default().into(),
                    mac: value["address"].as_str().map(str::to_owned),
                    mtu: value["mtu"].as_u64().map(|v| v as u32),
                    state: value["operstate"].as_str().map(str::to_owned),
                    master: value["master"].as_str().map(str::to_owned),
                    addresses: Vec::new(),
                    link_up: value["flags"]
                        .as_array()
                        .is_some_and(|flags| flags.iter().any(|flag| flag.as_str() == Some("UP"))),
                    offloads: BTreeMap::new(),
                });
            }
        }
        if capabilities.get("ethtool").copied().unwrap_or(false) {
            for interface in &mut interfaces {
                if let Ok(raw) = command_output("ethtool", &["-k", &interface.name]).await {
                    interface.offloads = parse_offloads(&raw);
                }
            }
        }
        if let Ok(raw) = addresses {
            for value in serde_json::from_str::<Vec<serde_json::Value>>(&raw).unwrap_or_default() {
                if let Some(found) = interfaces
                    .iter_mut()
                    .find(|i| Some(i.name.as_str()) == value["ifname"].as_str())
                {
                    found.addresses = value["addr_info"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|a| {
                            Some(format!(
                                "{}/{}",
                                a["local"].as_str()?,
                                a["prefixlen"].as_u64()?
                            ))
                        })
                        .collect();
                }
            }
        }
        interfaces.sort_by(|a, b| a.name.cmp(&b.name));
        let protected_interfaces = command_output("ip", &["-j", "route", "show", "default"])
            .await
            .ok()
            .and_then(|raw| serde_json::from_str::<Vec<serde_json::Value>>(&raw).ok())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|route| route["dev"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        let canonical = serde_json::to_vec(&(interfaces.clone(), protected_interfaces.clone()))?;
        use sha2::{Digest, Sha256};
        let fingerprint = format!("{:x}", Sha256::digest(canonical));
        Ok(NodeInventory {
            interfaces,
            capabilities,
            protected_interfaces,
            fingerprint,
        })
    }

    pub fn plan(
        &self,
        node_id: &str,
        revision_id: &str,
        draft: &NetworkProfileDraft,
        inventory: &NodeInventory,
    ) -> anyhow::Result<NetworkPlan> {
        let mut endpoint_plans = Vec::new();
        let mut commands = Vec::new();
        let mut rollback = Vec::new();
        let mut semantic = Vec::new();
        let mut warnings = Vec::new();
        for (role, endpoint) in [
            ("client", &draft.client_endpoint),
            ("server", &draft.server_endpoint),
        ] {
            if endpoint.node_id != node_id {
                continue;
            }
            validate_identifier(&endpoint.interface_name)?;
            let current = inventory
                .interfaces
                .iter()
                .find(|i| i.name == endpoint.interface_name)
                .with_context(|| format!("interface {} not found", endpoint.interface_name))?;
            if inventory
                .protected_interfaces
                .contains(&endpoint.interface_name)
            {
                bail!(
                    "interface {} carries the management route",
                    endpoint.interface_name
                )
            }
            if !current.addresses.is_empty() {
                bail!(
                    "interface {} already has addresses",
                    endpoint.interface_name
                )
            }
            let namespace = format!("pt-{}-{}", short(revision_id), role);
            validate_identifier(&namespace)?;
            let addresses = expand_pool(endpoint)?;
            semantic.push(format!(
                "move {} into {} and assign {} IPv4 addresses",
                endpoint.interface_name,
                namespace,
                addresses.len()
            ));
            if inventory
                .capabilities
                .get("arping")
                .copied()
                .unwrap_or(false)
            {
                for address in &addresses {
                    commands.push(CommandSpec::new(
                        "arping",
                        [
                            "-D",
                            "-c",
                            "2",
                            "-w",
                            "2",
                            "-I",
                            endpoint.interface_name.as_str(),
                            address.split('/').next().unwrap_or(address),
                        ],
                    ));
                }
            } else {
                warnings.push(format!(
                    "arping is unavailable on {node_id}; address conflict probing is skipped"
                ));
            }
            commands.push(CommandSpec::new("ip", ["netns", "add", namespace.as_str()]));
            commands.push(CommandSpec::new(
                "ip",
                [
                    "link",
                    "set",
                    endpoint.interface_name.as_str(),
                    "netns",
                    namespace.as_str(),
                ],
            ));
            commands.push(CommandSpec::new(
                "ip",
                ["-n", namespace.as_str(), "link", "set", "lo", "up"],
            ));
            commands.push(CommandSpec::new(
                "ip",
                [
                    "-n",
                    namespace.as_str(),
                    "link",
                    "set",
                    endpoint.interface_name.as_str(),
                    "mtu",
                    &draft.mtu.to_string(),
                    "up",
                ],
            ));
            for address in &addresses {
                commands.push(CommandSpec::new(
                    "ip",
                    [
                        "-n",
                        namespace.as_str(),
                        "address",
                        "add",
                        address.as_str(),
                        "dev",
                        endpoint.interface_name.as_str(),
                    ],
                ));
            }
            if inventory
                .capabilities
                .get("ethtool")
                .copied()
                .unwrap_or(false)
            {
                for feature in ["gro", "gso", "tso", "lro", "rx", "tx"] {
                    commands.push(CommandSpec::new(
                        "ip",
                        [
                            "netns",
                            "exec",
                            namespace.as_str(),
                            "ethtool",
                            "-K",
                            endpoint.interface_name.as_str(),
                            feature,
                            "off",
                        ],
                    ));
                }
            }
            rollback.push(CommandSpec::new(
                "ip",
                [
                    "-n",
                    namespace.as_str(),
                    "link",
                    "set",
                    endpoint.interface_name.as_str(),
                    "netns",
                    "1",
                ],
            ));
            // Checksum restoration must precede segmentation features: several
            // drivers reject TSO/GSO enablement while TX checksum is disabled.
            for feature in ["tx", "rx", "tso", "gso", "gro", "lro"] {
                if let Some(enabled) = current.offloads.get(feature) {
                    rollback.push(CommandSpec::new(
                        "ethtool",
                        [
                            "-K",
                            endpoint.interface_name.as_str(),
                            feature,
                            if *enabled { "on" } else { "off" },
                        ],
                    ));
                }
            }
            if let Some(mtu) = current.mtu {
                rollback.push(CommandSpec::new(
                    "ip",
                    [
                        "link",
                        "set",
                        endpoint.interface_name.as_str(),
                        "mtu",
                        &mtu.to_string(),
                    ],
                ));
            }
            rollback.push(CommandSpec::new(
                "ip",
                [
                    "link",
                    "set",
                    endpoint.interface_name.as_str(),
                    if current.link_up { "up" } else { "down" },
                ],
            ));
            rollback.push(CommandSpec::new("ip", ["netns", "del", namespace.as_str()]));
            endpoint_plans.push(EndpointPlan {
                role: role.into(),
                namespace,
                interface: endpoint.interface_name.clone(),
                addresses,
            });
        }
        if !inventory
            .capabilities
            .get("ethtool")
            .copied()
            .unwrap_or(false)
        {
            warnings.push("ethtool is unavailable; offload state cannot be changed".into());
        }
        Ok(NetworkPlan {
            profile_revision_id: revision_id.into(),
            node_id: node_id.into(),
            inventory_fingerprint: inventory.fingerprint.clone(),
            endpoints: endpoint_plans,
            semantic_changes: semantic,
            commands,
            rollback_commands: rollback,
            warnings,
        })
    }

    pub async fn apply(
        &self,
        operation_id: &str,
        plan: &NetworkPlan,
        lease_expires_unix_ms: i64,
    ) -> anyhow::Result<()> {
        let current = self.inventory().await?;
        if current.fingerprint != plan.inventory_fingerprint {
            bail!("inventory changed after plan; create a new plan")
        }
        let mut journal = NetworkJournal {
            operation_id: operation_id.into(),
            profile_revision_id: plan.profile_revision_id.clone(),
            phase: "applying".into(),
            lease_expires_unix_ms,
            rollback_commands: plan.rollback_commands.clone(),
            completed_commands: 0,
        };
        self.write_journal(&journal).await?;
        for command in &plan.commands {
            if let Err(error) = execute(command).await {
                journal.phase = "rolling_back".into();
                self.write_journal(&journal).await?;
                let _ = self.rollback(&journal).await;
                return Err(error);
            }
            journal.completed_commands += 1;
            self.write_journal(&journal).await?;
        }
        journal.phase = "staged".into();
        self.write_journal(&journal).await?;
        Ok(())
    }
    pub async fn commit(&self) -> anyhow::Result<()> {
        let mut journal = self
            .read_journal()
            .await?
            .context("no staged network operation")?;
        if journal.phase != "staged" {
            bail!("network operation is not staged")
        }
        journal.phase = "prepared".into();
        self.write_journal(&journal).await
    }
    pub async fn enforce_lease(&self, operation_id: String, lease_expires_unix_ms: i64) {
        let delay = (lease_expires_unix_ms - chrono::Utc::now().timestamp_millis()).max(0) as u64;
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        if let Ok(Some(journal)) = self.read_journal().await
            && journal.operation_id == operation_id
            && matches!(journal.phase.as_str(), "applying" | "staged")
        {
            let _ = self.rollback(&journal).await;
        }
    }
    pub async fn recover(&self) -> anyhow::Result<()> {
        if let Some(journal) = self.read_journal().await? {
            self.rollback(&journal).await?;
        }
        Ok(())
    }
    pub async fn rollback(&self, journal: &NetworkJournal) -> anyhow::Result<()> {
        let mut failures = Vec::new();
        for command in &journal.rollback_commands {
            let mut ok = false;
            for delay in [1, 2, 4] {
                if execute_rollback(command).await.is_ok() {
                    ok = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            }
            if !ok {
                failures.push(format!("{} {:?}", command.program, command.args));
            }
        }
        if failures.is_empty() {
            match tokio::fs::remove_file(&self.journal_path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            Ok(())
        } else {
            bail!("rollback failed: {}", failures.join(", "))
        }
    }
    async fn read_journal(&self) -> anyhow::Result<Option<NetworkJournal>> {
        match tokio::fs::read(&self.journal_path).await {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    async fn write_journal(&self, value: &NetworkJournal) -> anyhow::Result<()> {
        if let Some(parent) = self.journal_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let temporary = self.journal_path.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(value)?;
        let mut file = tokio::fs::File::create(&temporary).await?;
        use tokio::io::AsyncWriteExt;
        file.write_all(&bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(temporary, &self.journal_path).await?;
        Ok(())
    }
}

fn validate_identifier(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        bail!("unsafe Linux network identifier")
    };
    Ok(())
}
fn short(value: &str) -> &str {
    &value[..value.len().min(8)]
}
fn expand_pool(endpoint: &EndpointProfile) -> anyhow::Result<Vec<String>> {
    let (address, prefix) = endpoint
        .start_cidr
        .split_once('/')
        .context("invalid start_cidr")?;
    let start: u32 = address.parse::<std::net::Ipv4Addr>()?.into();
    Ok((0..endpoint.count)
        .map(|n| format!("{}/{}", std::net::Ipv4Addr::from(start + n), prefix))
        .collect())
}
fn parse_offloads(raw: &str) -> BTreeMap<String, bool> {
    [
        ("rx", "rx-checksumming:"),
        ("tx", "tx-checksumming:"),
        ("tso", "tcp-segmentation-offload:"),
        ("gso", "generic-segmentation-offload:"),
        ("gro", "generic-receive-offload:"),
        ("lro", "large-receive-offload:"),
    ]
    .into_iter()
    .filter_map(|(feature, label)| {
        let value = raw
            .lines()
            .find(|line| line.trim_start().starts_with(label))?;
        Some((feature.to_owned(), value.split_whitespace().nth(1)? == "on"))
    })
    .collect()
}
async fn execute(spec: &CommandSpec) -> anyhow::Result<()> {
    let output = tokio::process::Command::new(&spec.program)
        .args(&spec.args)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("execute {}", spec.program))?;
    if !output.status.success() {
        bail!(
            "{} {:?} failed: {}",
            spec.program,
            spec.args,
            String::from_utf8_lossy(&output.stderr)
        )
    }
    Ok(())
}
async fn execute_rollback(spec: &CommandSpec) -> anyhow::Result<()> {
    let output = tokio::process::Command::new(&spec.program)
        .args(&spec.args)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("execute rollback {}", spec.program))?;
    if output.status.success() {
        return Ok(());
    }
    let error = String::from_utf8_lossy(&output.stderr);
    if [
        "Cannot open network namespace",
        "No such file or directory",
        "Cannot find device",
        "does not exist",
    ]
    .iter()
    .any(|expected| error.contains(expected))
    {
        return Ok(());
    }
    bail!("{} {:?} failed: {}", spec.program, spec.args, error)
}
async fn command_output(program: &str, args: &[&str]) -> anyhow::Result<String> {
    let out = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await?;
    if !out.status.success() {
        bail!("{program} failed")
    };
    Ok(String::from_utf8(out.stdout)?)
}
async fn command_status(program: &str, args: &[&str]) -> bool {
    tokio::process::Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plan_is_deterministic_and_rejects_management_nic() {
        let mut inventory = NodeInventory {
            interfaces: vec![
                InterfaceInventory {
                    name: "eth1".into(),
                    ..Default::default()
                },
                InterfaceInventory {
                    name: "eth2".into(),
                    ..Default::default()
                },
            ],
            fingerprint: "abc".into(),
            ..Default::default()
        };
        let draft = NetworkProfileDraft::default();
        let manager = NetworkManager::new("unused");
        let plan = manager
            .plan("node-1", "12345678-revision", &draft, &inventory)
            .unwrap();
        let wire: wire::NetworkPlan = plan.clone().into();
        assert_eq!(NetworkPlan::from(wire), plan);
        assert_eq!(plan.endpoints.len(), 2);
        assert!(plan.commands.iter().all(|c| c.program == "ip"));
        assert_eq!(plan.endpoints[0].addresses.len(), 16);
        assert!(plan.rollback_commands[0].args.contains(&"link".to_owned()));
        assert_eq!(
            plan.rollback_commands[2].args,
            ["netns", "del", "pt-12345678-client"]
        );
        assert!(parse_offloads("rx-checksumming: on\ngeneric-receive-offload: off\n")["rx"]);
        inventory.protected_interfaces.push("eth1".into());
        assert!(
            manager
                .plan("node-1", "revision", &draft, &inventory)
                .unwrap_err()
                .to_string()
                .contains("management")
        );
    }
}
