import { test, expect } from "@playwright/test";

async function expectNoPageOverflow(page: import("@playwright/test").Page) {
  const dimensions = await page.evaluate(() => ({
    viewport: window.innerWidth,
    body: document.documentElement.scrollWidth,
    offenders: [...document.querySelectorAll<HTMLElement>("body *")]
      .filter((element) => element.getBoundingClientRect().right > window.innerWidth + 1)
      .slice(0, 8)
      .map(
        (element) =>
          `${element.tagName}.${element.className}:${Math.round(element.getBoundingClientRect().right)}`,
      ),
  }));
  expect(dimensions.body, dimensions.offenders.join("\n")).toBeLessThanOrEqual(dimensions.viewport);
}

test.beforeEach(async ({ page }) => {
  let scenarios: unknown[] = [];
  const agents = ["client-1", "server-1"].map((id, index) => ({
    id,
    hostname: id,
    role: index + 1,
    interfaces: ["eth0", "eth1"],
    online: true,
    inventory: {
      fingerprint: `${id}-fingerprint`,
      interfaces: [
        { name: "eth0", addresses: [`10.0.0.${index + 10}/24`] },
        { name: "eth1", addresses: [`10.20.0.${index + 10}/24`] },
      ],
    },
  }));
  await page.route("**/api/scenarios", async (route) => {
    if (route.request().method() === "POST") {
      const scenario = route.request().postDataJSON();
      scenarios = [scenario];
      await route.fulfill({ json: scenario });
    } else await route.fulfill({ json: scenarios });
  });
  await page.route("**/api/agents", (route) => route.fulfill({ json: agents }));
  await page.route("**/api/artifacts**", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/runs/page**", (route) =>
    route.fulfill({ json: { items: [], next_cursor: null } }),
  );
});

test("technical console setup, local fonts and themes are available", async ({ page }) => {
  const profileName = `Playwright profile ${Date.now()}`;
  await page.setViewportSize({ width: 1440, height: 1000 });
  await page.goto("/#setup");
  await expect(page.getByRole("navigation", { name: "주 메뉴" })).toBeVisible();
  await expect(page.getByLabel("시험 프로토콜")).toBeVisible();
  await expect(page.getByText("4. 부하")).toBeVisible();
  await expect(page.getByLabel("개별 시험 이름")).toHaveAttribute(
    "placeholder",
    "비워 두면 구성 이름과 시작 일시로 자동 생성",
  );
  await page.getByRole("button", { name: "+ Stage 추가" }).click();
  await expect(page.getByLabel("Stage 4 이름")).toBeVisible();
  await page.getByRole("textbox", { name: "트래픽 구성 이름", exact: true }).fill(profileName);
  await page.getByText("연결 고급 설정").click();
  await page.getByLabel("연결 경로").selectOption("explicit_proxy");
  await page.getByRole("button", { name: "현재 구성 저장" }).click();
  await expect(page.getByText("저장됨")).toBeVisible();
  await expect(
    page.getByLabel("저장된 시험 구성").locator("option", { hasText: profileName }),
  ).toHaveCount(1);
  await page.getByRole("button", { name: "새 구성" }).click();
  await expect(page.getByRole("textbox", { name: "트래픽 구성 이름", exact: true })).toHaveValue(
    "기본 TCP 시험",
  );
  await page.getByLabel("TLS 활성화").check();
  await expect(page.getByLabel("TLS 버전")).toBeVisible();
  await expect(page.getByRole("button", { name: "테스트 인증서 자동 생성" })).toBeHidden();
  await page.getByText("TLS 고급 설정").click();
  await expect(page.getByRole("button", { name: "테스트 인증서 자동 생성" })).toBeVisible();
  const font = await page
    .locator("body")
    .evaluate((element) => getComputedStyle(element).fontFamily);
  expect(font).toContain("Pretendard");
  const loadedFonts = await page.evaluate(() =>
    performance.getEntriesByType("resource").map((entry) => entry.name),
  );
  expect(loadedFonts.some((name) => name.includes("PretendardVariable"))).toBe(true);
  const before = await page.locator("html").getAttribute("data-theme"),
    next = before === "dark" ? "light" : "dark";
  await page.getByRole("button", { name: "테마 전환" }).click();
  await expect(page.locator("html")).toHaveAttribute("data-theme", next);
  await page.reload();
  await expect(page.locator("html")).toHaveAttribute("data-theme", next);
  await expectNoPageOverflow(page);
  await page.getByRole("button", { name: "실시간 모니터링" }).click();
  await expect(page.getByRole("heading", { name: "실시간 모니터링" })).toBeVisible();
  await page.getByRole("button", { name: "결과" }).click();
  await expect(page.getByRole("heading", { name: "시험 이력 및 비교" })).toBeVisible();
});

for (const viewport of [
  { name: "tablet", width: 900, height: 1000 },
  { name: "mobile", width: 390, height: 844 },
])
  test(`${viewport.name} console has no page overflow`, async ({ page }) => {
    await page.setViewportSize(viewport);
    await page.goto("/#setup");
    await expect(page.getByLabel("시험 프로토콜")).toBeVisible();
    await expectNoPageOverflow(page);
  });

test("managed direct without a prepared revision blocks start and explains the required action", async ({
  page,
}) => {
  await page.goto("/#setup");
  await expect(page.getByRole("button", { name: "시험 시작" })).toBeDisabled();
  await expect(page.getByRole("alert")).toContainText(
    "네트워크 구성을 저장·계획한 뒤 적용해야 합니다",
  );
  await expect(page.getByText("managed_direct requires profile_revision_id")).toHaveCount(0);
});

test("network plan exposes advanced commands and operation timeline on demand", async ({
  page,
}) => {
  const agents = ["client-1", "server-1"].map((id) => ({
    id,
    hostname: id,
    role: 0,
    online: true,
    inventory: { interfaces: [{ name: "eth0" }, { name: "eth1" }] },
  }));
  const nodePlan = (node: string, role: string, address: string) => ({
    node_id: node,
    inventory_fingerprint: `${node}-fingerprint`,
    semantic_changes: [`move eth1 into pt-test-${role}`],
    warnings: [],
    endpoints: [{ role, namespace: `pt-test-${role}`, interface: "eth1", addresses: [address] }],
    commands: [{ program: "ip", args: ["netns", "add", `pt-test-${role}`] }],
    rollback_commands: [{ program: "ip", args: ["netns", "del", `pt-test-${role}`] }],
  });
  await page.route("**/api/agents", (route) => route.fulfill({ json: agents }));
  await page.route("**/api/network/**", async (route) => {
    const url = route.request().url();
    if (url.includes("/operations/op-1")) {
      await route.fulfill({
        json: {
          id: "op-1",
          status: "planned",
          events: [
            {
              source: "network",
              node_id: "client-1",
              stage: "plan",
              status: "completed",
              detail: nodePlan("client-1", "client", "10.20.0.10/24"),
              created_at: "2026-08-14T00:00:00Z",
            },
          ],
        },
      });
    } else if (url.endsWith("/plan")) {
      await route.fulfill({
        json: {
          operation_id: "op-1",
          profile_revision_id: "revision-1",
          plan_token: "token-1",
          expires_at: "2026-08-14T01:00:00Z",
          detail: {
            plans: {
              "client-1": nodePlan("client-1", "client", "10.20.0.10/24"),
              "server-1": nodePlan("server-1", "server", "10.20.0.100/24"),
            },
          },
        },
      });
    } else {
      await route.fulfill({ json: {} });
    }
  });
  await page.goto("/#setup");
  const interfaceSelectors = page
    .locator("label.field", { hasText: "Interface" })
    .locator("select");
  await page.getByLabel("Node").nth(0).selectOption("client-1");
  await interfaceSelectors.nth(0).selectOption("eth1");
  await page.getByLabel("Node").nth(1).selectOption("server-1");
  await interfaceSelectors.nth(1).selectOption("eth1");
  await page.getByRole("button", { name: "저장 및 계획" }).click();
  await expect(page.getByText("move eth1 into pt-test-client")).toBeVisible();
  await expect(page.getByText("ip netns add pt-test-client")).toBeHidden();
  await page.getByText("실행 명령과 rollback").first().click();
  await expect(page.getByText("ip netns add pt-test-client")).toBeVisible();
  await page.getByRole("button", { name: "상세 로그" }).click();
  await expect(page.getByRole("complementary", { name: "상세 로그" })).toContainText("plan");
  await expect(page.getByRole("complementary", { name: "상세 로그" })).toContainText("client-1");
});

test("traffic-first payload, summary, capture analysis and advanced fields react to selections", async ({
  page,
}) => {
  let artifacts: unknown[] = [];
  const capture = {
    id: "capture-1",
    kind: "pcap",
    name: "sessions.pcap",
    sha256: "abc",
    size_bytes: 100,
    format: "pcap",
    analysis: {
      supported_flow_count: 2,
      http_flow_count: 1,
      http_transaction_count: 3,
      retransmitted_bytes: 12,
      exclusions: { non_http_flows: 1 },
    },
  };
  await page.route("**/api/artifacts**", async (route) => {
    if (route.request().method() === "POST") {
      await new Promise((resolve) => setTimeout(resolve, 150));
      artifacts = [capture];
      await route.fulfill({ json: capture });
    } else await route.fulfill({ json: artifacts });
  });
  await page.goto("/#setup");
  const headings = page.locator("section h3");
  await expect(headings).toHaveCount(4);
  const visualOrder = await headings.evaluateAll((elements) =>
    elements
      .map((element) => ({ text: element.textContent, top: element.getBoundingClientRect().top }))
      .sort((a, b) => a.top - b.top)
      .map((item) => item.text),
  );
  expect(visualOrder).toEqual(["1. 프로토콜", "2. 보안", "3. Payload", "4. 부하"]);
  await page.getByLabel("요청 · Client → Server 종류").selectOption("text");
  await page.getByLabel("요청 · Client → Server 문자열").fill("DLP-가");
  await page.getByLabel("응답 · Server → Client 종류").selectOption("random");
  await page.getByLabel("응답 · Server → Client Random 형식").selectOption("printable_ascii");
  await page.getByLabel("응답 · Server → Client 크기 (bytes)").fill("10485760");
  await expect(page.getByLabel("현재 트래픽 요약")).toContainText("요청: 문자열 7B");
  await expect(page.getByLabel("현재 트래픽 요약")).toContainText("응답: Random ASCII 10MB");
  await page.getByText("연결 고급 설정").click();
  await expect(page.getByLabel("Connect timeout (ms)")).toBeVisible();
  await expect(page.getByLabel("Wire 계측 인터페이스")).toHaveValue("eth0");
  await page.getByLabel("Wire 계측 인터페이스").fill("eth0, ens5");
  await page.getByLabel("연결 경로").selectOption("explicit_proxy");
  await expect(page.getByLabel("HTTP Proxy 주소")).toBeVisible();
  await expect(page.getByLabel("현재 트래픽 요약")).toContainText("명시적 Proxy");
  await page.getByLabel("Payload 모드").selectOption("capture_replay");
  await expect(page.getByRole("alert").filter({ hasText: "선택해야" })).toBeVisible();
  const upload = page.getByLabel("PCAP / PCAPNG 업로드");
  await upload.setInputFiles({
    name: "sessions.pcap",
    mimeType: "application/vnd.tcpdump.pcap",
    buffer: Buffer.from("pcap"),
  });
  await expect(page.getByRole("status")).toContainText("분석 중");
  await expect(page.getByLabel("Capture 분석 요약")).toContainText("2개 TCP 흐름");
  await expect(page.getByLabel("Capture 분석 요약")).toContainText("non_http_flows: 1");
  await expect(page.getByRole("alert")).toHaveCount(0);
  await page.getByLabel("시험 프로토콜").selectOption("http1");
  await expect(page.getByLabel("Capture 분석 요약")).toContainText("1개 HTTP 흐름");
  await expect(page.getByLabel("현재 트래픽 요약")).toContainText("HTTP/1.1");
  await expectNoPageOverflow(page);
});

test("selected run renders seven connected ECharts and accessible legends", async ({ page }) => {
  const scenario = {
    name: "Chart run",
    protocol: "http1",
    load_stages: [
      {
        name: "Warm-up",
        mode: "ramp",
        duration_secs: 10,
        target_virtual_clients: 10,
        include_in_results: false,
      },
      {
        name: "Measure",
        mode: "hold",
        duration_secs: 70,
        target_virtual_clients: 10,
        include_in_results: true,
      },
    ],
  };
  const metrics = (elapsed_ms: number, value: number) => ({
    elapsed_ms,
    load_stage_index: elapsed_ms < 10000 ? 0 : 1,
    included_in_results: elapsed_ms >= 10000,
    desired_virtual_clients: 10,
    cps: value,
    tps: value - 1,
    active_connections: 8,
    connections_established: value,
    connections_failed: 1,
    transactions: value - 1,
    transaction_errors: 1,
    tx_bps: value * 1e5,
    rx_bps: value * 8e4,
    wire_tx_bps: value * 11e4,
    wire_rx_bps: value * 9e4,
    tcp_connect_latency_p50_ms: 1,
    tcp_connect_latency_p95_ms: 2,
    tcp_connect_latency_p99_ms: 3,
    http_latency_p50_ms: 4,
    http_latency_p95_ms: 5,
    http_latency_p99_ms: 6,
    tcp_retransmissions_per_sec: 0,
  });
  const samples = [];
  for (let second = 0; second < 80; second++)
    samples.push(
      {
        agent_id: "client",
        role: 1,
        unix_ms: 1000 + second * 1000,
        metrics: metrics(second * 1000, 10 + second),
      },
      {
        agent_id: "server",
        role: 2,
        unix_ms: 1050 + second * 1000,
        metrics: metrics(second * 1000, 10 + second),
      },
    );
  await page.route("**/api/runs**", (route) => {
    const url = route.request().url(),
      run = {
        id: "run-1",
        run_name: "Chart run",
        status: "completed",
        started_at: new Date().toISOString(),
        finished_at: new Date().toISOString(),
        error: null,
        scenario,
      };
    if (url.includes("/samples"))
      return route.fulfill({ json: { samples, downsampled: false, stride: 1 } });
    if (url.includes("/summary")) return route.fulfill({ json: { ...run, samples: [] } });
    return route.fulfill({ json: { items: [run], next_cursor: null } });
  });
  await page.goto("/#results");
  await page.getByText("Chart run").click();
  await expect(page.locator("[data-chart]")).toHaveCount(7);
  for (const title of [
    "CPS",
    "TPS",
    "VU · Active Connection",
    "TCP Latency",
    "HTTP Latency",
    "처리량",
    "품질",
  ])
    await expect(page.locator(`[data-chart="${title}"] canvas`)).toBeVisible();
  await expect(page.getByRole("img", { name: "전체 시간 범위 탐색기" })).toBeVisible();
  await expect(page.getByRole("navigation", { name: "차트 바로가기" })).toBeVisible();
  await page
    .getByRole("navigation", { name: "차트 바로가기" })
    .getByRole("button", { name: "HTTP Latency" })
    .click();
  await expect(page.getByRole("cell", { name: "최대 App B/W" })).toBeVisible();
  await expect(page.getByRole("cell", { name: "최대 Wire B/W" })).toBeVisible();
  const legend = page.getByRole("button", { name: "Client App TX" });
  await expect(legend).toHaveAttribute("aria-pressed", "true");
  await legend.click();
  await expect(legend).toHaveAttribute("aria-pressed", "false");
  await expectNoPageOverflow(page);
});

test("live ECharts stop following after pan and return to latest", async ({ page }) => {
  await page.addInitScript(() => {
    class FakeWebSocket {
      static OPEN = 1;
      readyState = 1;
      onmessage: ((event: MessageEvent) => void) | null = null;
      constructor() {
        setTimeout(() => {
          this.emit({ type: "run_started", run_id: "live-run" });
          for (let second = 0; second < 90; second++)
            for (const role of [1, 2])
              this.emit({
                type: "metrics",
                agent_id: role === 1 ? "client-1" : "server-1",
                role,
                data: {
                  unix_ms: 1000 + second * 1000,
                  elapsed_ms: second * 1000,
                  load_stage_index: second < 10 ? 0 : 1,
                  included_in_results: second >= 10,
                  desired_virtual_clients: 10,
                  cps: second,
                  tps: second,
                  active_connections: 8,
                  connections_established: second,
                  connections_failed: 0,
                  transactions: second,
                  transaction_errors: 0,
                  tx_bps: second * 1e5,
                  rx_bps: second * 8e4,
                  wire_tx_bps: second * 11e4,
                  wire_rx_bps: second * 9e4,
                  tcp_connect_latency_p50_ms: 1,
                  tcp_connect_latency_p95_ms: 2,
                  tcp_connect_latency_p99_ms: 3,
                  http_latency_p50_ms: 4,
                  http_latency_p95_ms: 5,
                  http_latency_p99_ms: 6,
                  tcp_retransmissions_per_sec: 0,
                },
              });
        }, 50);
      }
      emit(value: unknown) {
        this.onmessage?.({ data: JSON.stringify(value) } as MessageEvent);
      }
      close() {}
    }
    Object.defineProperty(window, "WebSocket", { value: FakeWebSocket });
  });
  await page.goto("/#live");
  await expect(page.locator("[data-chart]")).toHaveCount(7);
  await expect(page.getByText("최신 60초 자동 추적")).toBeVisible();
  await page.getByRole("button", { name: "이전 60초" }).click();
  await expect(page.getByText("과거 구간 확인 중")).toBeVisible();
  await page.getByRole("button", { name: "최신으로" }).click();
  await expect(page.getByText("최신 60초 자동 추적")).toBeVisible();
});
