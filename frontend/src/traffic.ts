import { initialScenario, type Artifact, type PayloadProfile, type Scenario } from "./model";

export type TrafficPreset = "cps" | "http1" | "http2" | "bandwidth" | "dlp" | "pcap";

export function scenarioPreset(kind: TrafficPreset): Scenario {
  const scenario = initialScenario();
  switch (kind) {
    case "cps":
      return { ...scenario, name: "TCP CPS", virtual_clients: 1_000 };
    case "http1":
      return { ...scenario, name: "HTTP/1.1 TPS", protocol: "http1" };
    case "http2":
      return {
        ...scenario,
        name: "HTTP/2 Multiplex TPS",
        protocol: "http2",
        tls: { ...scenario.tls, enabled: true },
      };
    case "bandwidth":
      return {
        ...scenario,
        name: "대용량 B/W",
        request_payload: { ...scenario.request_payload, size_bytes: 1024 * 1024 },
        response_payload: { ...scenario.response_payload, size_bytes: 1024 * 1024 },
      };
    case "dlp":
      return {
        ...scenario,
        name: "DLP 요청·응답 문자열",
        request_payload: {
          ...scenario.request_payload,
          kind: "text",
          text: "DLP request sentinel",
        },
        response_payload: {
          ...scenario.response_payload,
          kind: "text",
          text: "DLP response sentinel",
        },
      };
    case "pcap":
      return { ...scenario, name: "PCAP 세션 재현", payload_mode: "capture_replay" };
  }
}

export function formatBytes(bytes: number): string {
  if (bytes >= 1024 * 1024) {
    return `${(bytes / 1024 / 1024).toFixed(bytes % (1024 * 1024) ? 1 : 0)}MB`;
  }
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(bytes % 1024 ? 1 : 0)}KB`;
  return `${bytes}B`;
}

export function payloadLabel(payload: PayloadProfile | null, direction: string): string {
  if (!payload) return `${direction}: 없음`;
  const bytes =
    payload.kind === "text"
      ? new TextEncoder().encode(payload.text).length
      : payload.kind === "empty"
        ? 0
        : payload.size_bytes;
  const kind = {
    empty: "없음",
    fixed: "고정",
    text: "문자열",
    file: "파일",
    random: `Random ${payload.random_format === "binary" ? "Binary" : "ASCII"}`,
  }[payload.kind];
  return `${direction}: ${kind}${payload.kind === "empty" ? "" : ` ${formatBytes(bytes)}`}`;
}

export function trafficSummary(scenario: Scenario, selectedCapture?: Artifact): string {
  return [
    scenario.protocol === "http2" ? "HTTP/2" : scenario.protocol === "http1" ? "HTTP/1.1" : "TCP",
    scenario.tls.enabled ? `TLS ${scenario.tls.version === "tls13" ? "1.3" : "1.2"}` : "평문",
    scenario.payload_mode === "capture_replay"
      ? `PCAP: ${selectedCapture?.name ?? "미선택"}`
      : `${payloadLabel(scenario.request_payload, "요청")} · ${payloadLabel(scenario.response_payload, "응답")}`,
    scenario.path.kind === "explicit_proxy" ? "명시적 Proxy" : "관리형 직접 연결",
  ].join(" · ");
}

export function captureReplayBlocked(scenario: Scenario, selectedCapture?: Artifact): boolean {
  if (scenario.payload_mode !== "capture_replay") return false;
  if (!selectedCapture) return true;
  if (scenario.protocol === "http2") {
    return (selectedCapture.analysis?.http2_flow_count ?? 0) === 0;
  }
  if (scenario.protocol === "http1") {
    return (selectedCapture.analysis?.http_flow_count ?? 0) === 0;
  }
  return (selectedCapture.analysis?.supported_flow_count ?? 0) === 0;
}
