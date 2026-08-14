use std::sync::Arc;

use anyhow::bail;
use proxy_tester_capture::{Direction, HttpTransaction, ReplayTurn, analyze_capture};
use proxy_tester_domain::{Protocol, Scenario};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplayRole {
    Client,
    Server,
}

#[derive(Clone)]
pub(crate) struct ReplayPlan {
    pub(crate) flows: Arc<[Arc<[ReplayTurn]>]>,
}

impl ReplayPlan {
    pub(crate) fn from_capture(
        bytes: &[u8],
        scenario: &Scenario,
        role: ReplayRole,
    ) -> anyhow::Result<Self> {
        let (_, analysis) = analyze_capture(bytes)?;
        if analysis.flows.is_empty() {
            bail!("capture contains no supported flows");
        }
        let flows: Arc<[Arc<[ReplayTurn]>]> = analysis
            .flows
            .into_iter()
            .filter_map(|flow| {
                let turns = if scenario.protocol == Protocol::Http1 {
                    (!flow.http_transactions.is_empty()).then(|| {
                        flow.http_transactions
                            .into_iter()
                            .flat_map(|transaction| http_replay_turns(transaction, scenario, role))
                            .collect()
                    })
                } else if scenario.protocol == Protocol::Http2 {
                    (!flow.http2_transactions.is_empty()).then(|| {
                        flow.http2_transactions
                            .into_iter()
                            .flat_map(|transaction| {
                                [
                                    ReplayTurn {
                                        direction: Direction::ClientToServer,
                                        payload: transaction.request_body,
                                    },
                                    ReplayTurn {
                                        direction: Direction::ServerToClient,
                                        payload: transaction.response_body,
                                    },
                                ]
                            })
                            .collect()
                    })
                } else {
                    Some(flow.turns)
                }?;
                Some(Arc::<[ReplayTurn]>::from(turns))
            })
            .collect::<Vec<_>>()
            .into();
        if flows.is_empty() {
            bail!("capture contains no supported HTTP transactions");
        }
        validate_flows(&flows)?;
        Ok(Self { flows })
    }
}

fn validate_flows(flows: &[Arc<[ReplayTurn]>]) -> anyhow::Result<()> {
    for (index, flow) in flows.iter().enumerate() {
        let Some(first) = flow.first() else {
            bail!("flow {index} has no replay turns");
        };
        if first.direction != Direction::ClientToServer || first.payload.is_empty() {
            bail!("flow {index} must start with a non-empty client turn");
        }
        for (other_index, other) in flows.iter().enumerate() {
            if index != other_index
                && other.first().is_some_and(|turn| {
                    turn.payload.starts_with(&first.payload)
                        || first.payload.starts_with(&turn.payload)
                })
            {
                bail!("flows {index} and {other_index} have ambiguous first client turns");
            }
        }
    }
    Ok(())
}

fn http_replay_turns(
    transaction: HttpTransaction,
    scenario: &Scenario,
    role: ReplayRole,
) -> [ReplayTurn; 2] {
    [
        ReplayTurn {
            direction: Direction::ClientToServer,
            payload: rewrite_http_request(&transaction.request, scenario, role),
        },
        ReplayTurn {
            direction: Direction::ServerToClient,
            payload: transaction.response,
        },
    ]
}

fn rewrite_http_request(request: &[u8], scenario: &Scenario, role: ReplayRole) -> Vec<u8> {
    let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
        return request.to_vec();
    };
    let header_end = header_end + 4;
    let headers = &request[..header_end];
    let Some(line_end) = headers.windows(2).position(|part| part == b"\r\n") else {
        return request.to_vec();
    };
    let mut fields = headers[..line_end].splitn(3, |byte| *byte == b' ');
    let (Some(method), Some(target), Some(version)) = (fields.next(), fields.next(), fields.next())
    else {
        return request.to_vec();
    };
    let path = if target.starts_with(b"http://") {
        target
            .windows(1)
            .enumerate()
            .skip(7)
            .find(|(_, byte)| *byte == b"/")
            .map(|(index, _)| &target[index..])
            .unwrap_or(b"/")
    } else {
        target
    };
    let absolute =
        role == ReplayRole::Client && scenario.is_explicit_proxy() && !scenario.tls.enabled;
    let target = if absolute {
        format!(
            "http://{}{}",
            scenario.target_addr(),
            String::from_utf8_lossy(path)
        )
        .into_bytes()
    } else {
        path.to_vec()
    };
    let mut output = Vec::with_capacity(request.len() + target.len());
    output.extend_from_slice(method);
    output.push(b' ');
    output.extend_from_slice(&target);
    output.push(b' ');
    output.extend_from_slice(version);
    output.extend_from_slice(b"\r\n");
    let mut replaced_host = false;
    for line in headers[line_end + 2..header_end - 2].split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if line
            .split(|byte| *byte == b':')
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case(b"host"))
        {
            output.extend_from_slice(format!("Host: {}\r\n", scenario.request.host).as_bytes());
            replaced_host = true;
        } else {
            output.extend_from_slice(line);
            output.extend_from_slice(b"\r\n");
        }
    }
    if !replaced_host {
        output.extend_from_slice(format!("Host: {}\r\n", scenario.request.host).as_bytes());
    }
    output.extend_from_slice(b"\r\n");
    output.extend_from_slice(&request[header_end..]);
    output
}
