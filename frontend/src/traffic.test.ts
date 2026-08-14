import { describe, expect, it } from "vitest";
import { initialScenario, type Artifact } from "./model";
import { captureReplayBlocked, payloadLabel, scenarioPreset, trafficSummary } from "./traffic";

describe("traffic helpers", () => {
  it("builds presets without mutating a shared scenario", () => {
    const http2 = scenarioPreset("http2");
    const tcp = scenarioPreset("cps");
    expect(http2).toMatchObject({ protocol: "http2", tls: { enabled: true } });
    expect(tcp).toMatchObject({ protocol: "tcp", virtual_clients: 1_000 });
  });

  it("uses UTF-8 byte length in payload summaries", () => {
    expect(
      payloadLabel(
        { kind: "text", text: "가", size_bytes: 0, artifact_id: null, random_format: "binary" },
        "요청",
      ),
    ).toBe("요청: 문자열 3B");
  });

  it("selects capture support counts by protocol", () => {
    const scenario = initialScenario();
    scenario.payload_mode = "capture_replay";
    const capture = {
      id: crypto.randomUUID(),
      kind: "pcap",
      name: "fixture.pcap",
      sha256: "digest",
      size_bytes: 1,
      format: "pcap",
      analysis: { supported_flow_count: 1, http_flow_count: 0 },
    } satisfies Artifact;
    expect(captureReplayBlocked(scenario, capture)).toBe(false);
    scenario.protocol = "http1";
    expect(captureReplayBlocked(scenario, capture)).toBe(true);
    expect(trafficSummary(scenario, capture)).toContain("PCAP: fixture.pcap");
  });
});
