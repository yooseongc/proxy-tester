import React, { Suspense, lazy, useEffect, useMemo, useState } from "react";
import { createRoot } from "react-dom/client";
import {
  Activity,
  ArrowDown,
  ArrowUp,
  BarChart3,
  Box,
  Clock3,
  FilePlus2,
  Gauge,
  Globe2,
  History,
  Layers3,
  Moon,
  Network,
  Pause,
  Play,
  Radio,
  Save,
  Server,
  ShieldCheck,
  Square,
  Sun,
  Trash2,
  Users,
  Wifi,
} from "lucide-react";
import { api } from "./api";
const TelemetryCharts=lazy(()=>import("./TelemetryCharts").then(module=>({default:module.TelemetryCharts})));
const RunHistory=lazy(()=>import("./RunHistory").then(module=>({default:module.RunHistory})));
import {
  closestSample,
  formatBandwidth,
  initialScenario,
  recentActiveMaximum,
  type Agent,
  type LoadStage,
  type PayloadProfile,
  type Point,
  type Scenario,
} from "./model";
import {
  Button,
  Field,
  MetricCard,
  Panel,
  SectionTitle,
  StatusBadge,
  type Theme,
} from "./ui";
import "./styles.css";

type Tab = "setup" | "live" | "results";
type Artifact = {
  id: string;
  kind: "payload" | "pcap";
  name: string;
  sha256: string;
  size_bytes: number;
  format: string;
  analysis?: {
    supported_flow_count?: number;
    http_flow_count?: number;
    http_transaction_count?: number;
    http2_flow_count?: number;
    http2_transaction_count?: number;
    retransmitted_bytes?: number;
    exclusions?: Record<string, number>;
  };
};
const tabFromHash = (): Tab =>
  location.hash === "#live"
    ? "live"
    : location.hash === "#results"
      ? "results"
      : "setup";
const initialTheme = (): Theme => {
  const saved = localStorage.getItem("proxy-tester-theme");
  return saved === "light" || saved === "dark"
    ? saved
    : matchMedia("(prefers-color-scheme: light)").matches
      ? "light"
      : "dark";
};

function App() {
  const [agents, setAgents] = useState<Agent[]>([]),
    [scenario, setScenario] = useState(initialScenario),
    [points, setPoints] = useState<Point[]>([]);
  const [savedScenarios, setSavedScenarios] = useState<Scenario[]>([]),
    [saveMessage, setSaveMessage] = useState("");
  const [artifacts, setArtifacts] = useState<Artifact[]>([]),
    [uploading, setUploading] = useState(false);
  const [runName, setRunName] = useState("");
  const [status, setStatus] = useState("대기 중"),
    [error, setError] = useState(""),
    [activeRun, setActiveRun] = useState<string | null>(null);
  const [tab, setTab] = useState<Tab>(tabFromHash),
    [theme, setTheme] = useState<Theme>(initialTheme);
  const clientPoints = useMemo(
      () => points.filter((p) => p.role === 1),
      [points],
    ),
    serverPoints = useMemo(() => points.filter((p) => p.role === 2), [points]);
  const latestClientRaw = clientPoints.at(-1);
  const activeMinuteMax = recentActiveMaximum(clientPoints);
  const latestClient = latestClientRaw
    ? {
        ...latestClientRaw,
        active_connections:
          `${latestClientRaw.active_connections.toLocaleString()} / ${activeMinuteMax.toLocaleString()}` as unknown as number,
      }
    : undefined;
  const latestServer = closestSample(latestClientRaw, serverPoints),
    currentStage = scenario.load_stages[latestClient?.load_stage_index ?? 0];
  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    localStorage.setItem("proxy-tester-theme", theme);
  }, [theme]);
  useEffect(() => {
    const hash = () => setTab(tabFromHash());
    addEventListener("hashchange", hash);
    return () => removeEventListener("hashchange", hash);
  }, []);
  useEffect(() => {
    const load = () =>
      api<Agent[]>("/api/agents")
        .then(setAgents)
        .catch(() => {});
    load();
    const timer = setInterval(load, 3000);
    const ws = new WebSocket(
      `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/api/events/ws`,
    );
    ws.onmessage = (e) => {
      const m = JSON.parse(e.data);
      if (m.type === "metrics")
        setPoints((p) =>
          [...p, { ...m.data, agent_id: m.agent_id, role: m.role }].slice(
            -7200,
          ),
        );
      if (m.type === "run_started") {
        setStatus("실행 중");
        setActiveRun(m.run_id);
      }
      if (m.type === "run_state")
        setStatus(m.status === "paused" ? "일시정지" : "실행 중");
      if (m.type === "run_finished") {
        setStatus(m.status);
        setActiveRun(null);
      }
    };
    return () => {
      clearInterval(timer);
      ws.close();
    };
  }, []);
  const refreshScenarios = () =>
    api<Scenario[]>("/api/scenarios")
      .then(setSavedScenarios)
      .catch(() => {});
  useEffect(() => {
    refreshScenarios();
  }, []);
  const refreshArtifacts = () =>
    api<Artifact[]>("/api/artifacts")
      .then(setArtifacts)
      .catch(() => {});
  useEffect(() => {
    refreshArtifacts();
  }, []);
  const navigate = (next: Tab) => {
      location.hash = next;
      setTab(next);
    },
    patch = (value: Partial<Scenario>) =>
      setScenario((s) => ({ ...s, ...value }));
  const requestPatch = (value: Partial<Scenario["request"]>) =>
    patch({ request: { ...scenario.request, ...value } });
  const syncStages = (stages: LoadStage[]) =>
    patch({
      load_stages: stages,
      duration_secs: stages.reduce((n, s) => n + s.duration_secs, 0),
      virtual_clients: Math.max(
        1,
        ...stages.map((s) => s.target_virtual_clients),
      ),
    });
  const stagePatch = (index: number, value: Partial<LoadStage>) =>
    syncStages(
      scenario.load_stages.map((s, i) =>
        i === index ? { ...s, ...value } : s,
      ),
    );
  const moveStage = (index: number, by: number) => {
    const next = [...scenario.load_stages],
      target = index + by;
    if (target < 0 || target >= next.length) return;
    [next[index], next[target]] = [next[target], next[index]];
    syncStages(next);
  };
  const start = async () => {
    setError("");
    setPoints([]);
    setStatus("준비 중");
    try {
      const preflight = await api<{ ok: boolean }>("/api/preflight", {
        method: "POST",
        body: JSON.stringify(scenario),
      });
      if (!preflight.ok) throw new Error("사전 검사에 실패했습니다.");
      await api("/api/scenarios", {
        method: "POST",
        body: JSON.stringify(scenario),
      });
      await api("/api/runs", {
        method: "POST",
        body: JSON.stringify({ scenario, run_name: runName.trim() || null }),
      });
      navigate("live");
    } catch (cause) {
      setStatus("대기 중");
      setError((cause as Error).message);
    }
  };
  const control = async (action: "pause" | "resume" | "stop") => {
    if (!activeRun) return;
    try {
      await api(`/api/runs/${activeRun}/${action}`, { method: "POST" });
      if (action === "pause") setStatus("일시정지");
      if (action === "resume") setStatus("실행 중");
    } catch (cause) {
      setError((cause as Error).message);
    }
  };
  const generateCertificate = async () => {
    setError("");
    try {
      const cert = await api<{
        ca_pem: string;
        server_cert_pem: string;
        server_key_pem: string;
      }>("/api/tls/certificates", {
        method: "POST",
        body: JSON.stringify({ server_name: scenario.tls.server_name }),
      });
      patch({ tls: { ...scenario.tls, ...cert, verify_peer: true } });
    } catch (cause) {
      setError((cause as Error).message);
    }
  };
  const saveScenario = async () => {
    setError("");
    setSaveMessage("");
    try {
      await api("/api/scenarios", {
        method: "POST",
        body: JSON.stringify(scenario),
      });
      await refreshScenarios();
      setSaveMessage("저장됨");
    } catch (cause) {
      setError((cause as Error).message);
    }
  };
  const loadScenario = (id: string) => {
    const found = savedScenarios.find((item) => item.id === id);
    if (!found) return;
    const fallback = initialScenario();
    setScenario({
      ...fallback,
      ...found,
      version: 4,
      http2: found.http2 ?? fallback.http2,
      load_stages: found.load_stages?.length
        ? found.load_stages
        : fallback.load_stages,
    });
    setSaveMessage("불러옴");
  };
  const readPem = async (
    field: "server_cert_pem" | "server_key_pem" | "ca_pem",
    file?: File,
  ) => {
    if (file) patch({ tls: { ...scenario.tls, [field]: await file.text() } });
  };
  const uploadPayload = async (file: File) => {
    setUploading(true);
    setError("");
    try {
      const form = new FormData();
      form.append("file", file);
      const artifact = await api<Artifact>("/api/artifacts?kind=payload", {
        method: "POST",
        body: form,
      });
      await refreshArtifacts();
      return artifact;
    } catch (cause) {
      setError((cause as Error).message);
      throw cause;
    } finally {
      setUploading(false);
    }
  };
  const uploadCapture = async (file: File) => {
    setUploading(true);
    setError("");
    try {
      const form = new FormData();
      form.append("file", file);
      const artifact = await api<Artifact>("/api/artifacts?kind=pcap", {
        method: "POST",
        body: form,
      });
      await refreshArtifacts();
      return artifact;
    } catch (cause) {
      setError((cause as Error).message);
      throw cause;
    } finally {
      setUploading(false);
    }
  };
  const transactionMode =
    scenario.request.transactions_per_connection === 0
      ? "continuous"
      : scenario.request.transactions_per_connection === 1
        ? "single"
        : "fixed";
  const applyPreset=(kind:"cps"|"http1"|"http2"|"bandwidth"|"dlp"|"pcap")=>{
    const next=initialScenario();
    if(kind==="cps")Object.assign(next,{name:"TCP CPS",virtual_clients:1000});
    if(kind==="http1")Object.assign(next,{name:"HTTP/1.1 TPS",protocol:"http1"});
    if(kind==="http2")Object.assign(next,{name:"HTTP/2 Multiplex TPS",protocol:"http2",tls:{...next.tls,enabled:true}});
    if(kind==="bandwidth")Object.assign(next,{name:"양방향 B/W",request_payload:{...next.request_payload!,size_bytes:1024*1024},response_payload:{...next.response_payload!,size_bytes:1024*1024}});
    if(kind==="dlp")Object.assign(next,{name:"DLP 양방향 문자열",request_payload:{...next.request_payload!,kind:"text",text:"DLP request sentinel"},response_payload:{...next.response_payload!,kind:"text",text:"DLP response sentinel"}});
    if(kind==="pcap")Object.assign(next,{name:"PCAP 세션 재현",payload_mode:"capture_replay"});
    setScenario(next);
  };
  const selectedCapture = artifacts.find(
    (artifact) => artifact.id === scenario.capture_artifact_id,
  );
  const payloadLabel = (payload: PayloadProfile | null, direction: string) => {
    if (!payload) return `${direction}: 없음`;
    const bytes =
      payload.kind === "text"
        ? new TextEncoder().encode(payload.text).length
        : payload.size_bytes;
    const kind = {
      empty: "없음",
      fixed: "고정",
      text: "문자열",
      file: "파일",
      random: `Random ${payload.random_format === "binary" ? "Binary" : "ASCII"}`,
    }[payload.kind];
    return `${direction}: ${kind}${payload.kind === "empty" ? "" : ` ${formatBytes(bytes)}`}`;
  };
  const trafficSummary = [
    scenario.protocol === "http2" ? "HTTP/2" : scenario.protocol === "http1" ? "HTTP/1.1" : "TCP",
    scenario.tls.enabled
      ? `TLS ${scenario.tls.version === "tls13" ? "1.3" : "1.2"}`
      : "평문",
    scenario.payload_mode === "capture_replay"
      ? `PCAP: ${selectedCapture?.name ?? "미선택"}`
      : `${payloadLabel(scenario.request_payload, "요청")} · ${payloadLabel(scenario.response_payload, "응답")}`,
    scenario.path.kind === "explicit_proxy" ? "명시적 Proxy" : "관리형 직접 연결",
  ].join(" · ");
  const captureBlocked = scenario.payload_mode === "capture_replay" && (!selectedCapture || (scenario.protocol === "http2" ? (selectedCapture.analysis?.http2_flow_count ?? 0) === 0 : scenario.protocol === "http1" ? (selectedCapture.analysis?.http_flow_count ?? 0) === 0 : (selectedCapture.analysis?.supported_flow_count ?? 0) === 0));
  const tabs: [Tab, string, React.ElementType][] = [
    ["setup", "시험 구성", Layers3],
    ["live", "실시간 모니터링", Activity],
    ["results", "결과", History],
  ];
  return (
    <div className="min-h-screen">
      <div className="console-grid pointer-events-none fixed inset-0 opacity-20" />
      <main className="relative mx-auto max-w-[1560px] px-3 pb-12 sm:px-6 lg:px-10">
        <header className="sticky top-0 z-50 -mx-3 mb-4 border-b border-line bg-canvas/85 px-3 py-3 backdrop-blur-2xl sm:-mx-6 sm:px-6 lg:-mx-10 lg:px-10">
          <div className="mx-auto flex max-w-[1480px] items-center justify-between gap-4">
            <div className="flex min-w-0 items-center gap-3">
              <img
                src="/favicon.svg"
                className="size-10 rounded-xl shadow-lg"
              />
              <div className="min-w-0">
                <p className="truncate font-mono text-[9px] font-bold tracking-[.18em] text-signal">
                  NETWORK PERFORMANCE LAB
                </p>
                <h1 className="m-0 text-lg font-extrabold tracking-[-.04em]">
                  Proxy Tester
                </h1>
              </div>
            </div>
            <div className="hidden items-center gap-2 lg:flex">
              {agents.map((agent) => (
                <div
                  key={agent.id}
                  className="flex items-center gap-2 rounded-xl border border-line bg-panel px-3 py-2"
                >
                  <span
                    className={`size-2 rounded-full ${agent.online ? "bg-signal shadow-[0_0_10px_var(--signal)]" : "bg-critical"}`}
                  />
                  <div>
                    <b className="block font-mono text-[10px]">{agent.id}</b>
                    <small className="block text-[8px] text-dim">
                      {agent.role === 1 ? "CLIENT" : "SERVER"} ·{" "}
                      {agent.hostname.slice(0, 8)}
                    </small>
                  </div>
                </div>
              ))}
            </div>
            <div className="flex items-center gap-2">
              <StatusBadge tone={activeRun ? "live" : "neutral"}>
                {status}
              </StatusBadge>
              <Button
                variant="ghost"
                aria-label="테마 전환"
                className="size-10 px-0"
                onClick={() =>
                  setTheme((value) => (value === "dark" ? "light" : "dark"))
                }
              >
                {theme === "dark" ? <Sun size={16} /> : <Moon size={16} />}
              </Button>
            </div>
          </div>
        </header>
        <nav
          aria-label="주 메뉴"
          className="sticky top-[65px] z-40 mb-5 flex gap-1 rounded-2xl border border-line bg-panel/90 p-1.5 shadow-panel backdrop-blur-xl"
        >
          {tabs.map(([id, label, Icon]) => (
            <button
              key={id}
              onClick={() => navigate(id)}
              className={`flex min-h-11 flex-1 items-center justify-center gap-2 rounded-xl px-3 text-xs font-bold transition ${tab === id ? "bg-signal text-on-signal shadow-lg" : "text-dim hover:bg-raised hover:text-ink"}`}
            >
              <Icon size={15} />
              <span>{label}</span>
            </button>
          ))}
        </nav>
        {tab === "setup" && (
          <Panel className="p-4 sm:p-6">
            <SectionTitle
              eyebrow="TRAFFIC PROFILE"
              title="어떤 트래픽을 생성할까요?"
              aside={
                <span className="mono-numbers rounded-xl border border-line bg-raised px-3 py-2 text-[10px] text-dim">
                  {scenario.duration_secs}s · MAX {scenario.virtual_clients} VU
                </span>
              }
            />
            <p
              aria-label="현재 트래픽 요약"
              className="mb-5 overflow-hidden text-ellipsis rounded-xl border border-signal/25 bg-signal/5 px-4 py-3 text-xs font-bold text-signal"
            >
              {trafficSummary}
            </p>
            <div aria-label="시험 프리셋" className="mb-4 flex flex-wrap gap-2">{([['cps','TCP CPS'],['http1','HTTP/1.1 TPS'],['http2','HTTP/2 TPS'],['bandwidth','양방향 B/W'],['dlp','DLP'],['pcap','PCAP']] as const).map(([id,label])=><Button key={id} onClick={()=>applyPreset(id)}>{label}</Button>)}</div>
            <div className="mb-5 grid gap-3 rounded-2xl border border-line bg-raised/60 p-3 md:grid-cols-[1fr_auto]">
              <Field label="저장된 시험 구성">
                <select
                  aria-label="저장된 시험 구성"
                  value=""
                  onChange={(e) => loadScenario(e.target.value)}
                >
                  <option value="">선택하여 불러오기</option>
                  {savedScenarios.map((item) => (
                    <option key={item.id} value={item.id}>
                      {item.name}
                    </option>
                  ))}
                </select>
              </Field>
              <div className="flex items-end gap-2">
                <Button
                  onClick={() => {
                    setScenario(initialScenario());
                    setSaveMessage("새 구성");
                  }}
                >
                  <FilePlus2 size={14} />새 구성
                </Button>
                <Button onClick={saveScenario}>
                  <Save size={14} />
                  현재 구성 저장
                </Button>
                {saveMessage && (
                  <span className="self-center text-[10px] font-bold text-signal">
                    {saveMessage}
                  </span>
                )}
              </div>
            </div>
            <div className="flex flex-col gap-4">
              <div className="contents">
                <ConfigSection
                  icon={Network}
                  title="1. 프로토콜"
                  description="구성 이름과 생성할 애플리케이션 프로토콜을 선택합니다."
                >
                  <Field label="구성 이름">
                    <input
                      aria-label="구성 이름"
                      value={scenario.name}
                      onChange={(e) => patch({ name: e.target.value })}
                    />
                  </Field>
                  <div className="grid gap-3 sm:grid-cols-2">
                    <Field label="시험 프로토콜">
                      <select
                        value={
                          scenario.protocol === "connect"
                            ? "tcp"
                            : scenario.protocol
                        }
                        onChange={(e) => {
                          const protocol=e.target.value as Scenario["protocol"];
                          patch({protocol,tls:protocol==="http2"?{...scenario.tls,enabled:true}:scenario.tls});
                        }}
                      >
                        <option value="tcp">TCP</option>
                        <option value="http1">HTTP/1.1</option>
                        <option value="http2">HTTP/2</option>
                      </select>
                    </Field>
                  </div>
                  <details className="rounded-xl border border-dashed border-line p-3">
                    <summary className="cursor-pointer text-xs font-bold text-dim">연결 고급 설정</summary>
                    <div className="mt-3 space-y-3">
                    <Field label="연결 경로"><select value={scenario.path.kind} onChange={(e)=>patch({path:e.target.value==="explicit_proxy"?{kind:"explicit_proxy",client_node_id:agents[0]?.id??"node-1",client_bind_ip:"192.0.2.10",server_node_id:agents[1]?.id??agents[0]?.id??"node-2",server_listen_ip:"192.0.2.20",server_port:8080,proxy_addr:"proxy:3128"}:{kind:"managed_direct",profile_revision_id:"00000000-0000-0000-0000-000000000000",server_port:8080}})}><option value="managed_direct">관리형 직접 연결</option><option value="explicit_proxy">명시적 HTTP Proxy</option></select></Field>
                    {scenario.path.kind === "explicit_proxy" && <Field label="HTTP Proxy 주소">
                      <input
                        value={scenario.path.proxy_addr}
                        onChange={(e) => scenario.path.kind==="explicit_proxy"&&patch({ path:{...scenario.path,proxy_addr:e.target.value} })}
                      />
                    </Field>}
                  <Field label="Server port">
                    <input
                      type="number" min="1" max="65535" value={scenario.path.server_port}
                      onChange={(e) => patch({ path:{...scenario.path,server_port:Math.max(1,+e.target.value)} })}
                    />
                  </Field>
                    <div className="grid gap-2 sm:grid-cols-3"><Field label="Connect timeout (ms)"><input type="number" min="1" value={scenario.timeouts.connect_ms} onChange={(e)=>patch({timeouts:{...scenario.timeouts,connect_ms:Math.max(1,+e.target.value)}})}/></Field><Field label="Proxy timeout (ms)"><input type="number" min="1" value={scenario.timeouts.proxy_connect_ms} onChange={(e)=>patch({timeouts:{...scenario.timeouts,proxy_connect_ms:Math.max(1,+e.target.value)}})}/></Field><Field label="Response timeout (ms)"><input type="number" min="1" value={scenario.timeouts.response_ms} onChange={(e)=>patch({timeouts:{...scenario.timeouts,response_ms:Math.max(1,+e.target.value)}})}/></Field></div>
                    </div>
                  </details>
                </ConfigSection>
                <ConfigSection
                  icon={Box}
                  title="3. Payload"
                  description="요청과 응답 payload를 독립적으로 구성합니다."
                >
                  <Field label="Payload 모드">
                    <select
                      value={scenario.payload_mode}
                      onChange={(e) =>
                        patch({
                          payload_mode: e.target
                            .value as Scenario["payload_mode"],
                        })
                      }
                    >
                      <option value="manual">직접 구성</option>
                      <option value="capture_replay">PCAP 세션 재현</option>
                    </select>
                  </Field>
                  {scenario.payload_mode === "manual" ? (
                    <div className="grid gap-3 sm:grid-cols-2">
                      <PayloadEditor
                        label="요청 · Client → Server"
                        value={scenario.request_payload!}
                        artifacts={artifacts}
                        uploading={uploading}
                        onUpload={uploadPayload}
                        onChange={(request_payload) =>
                          patch({ request_payload })
                        }
                      />
                      <PayloadEditor
                        label="응답 · Server → Client"
                        value={scenario.response_payload!}
                        artifacts={artifacts}
                        uploading={uploading}
                        onUpload={uploadPayload}
                        onChange={(response_payload) =>
                          patch({ response_payload })
                        }
                      />
                    </div>
                  ) : (
                    <CaptureEditor
                      artifacts={artifacts}
                      protocol={scenario.protocol}
                      selected={scenario.capture_artifact_id}
                      uploading={uploading}
                      onUpload={uploadCapture}
                      onSelect={(capture_artifact_id) =>
                        patch({ capture_artifact_id })
                      }
                    />
                  )}{" "}
                  {scenario.protocol === "http2" && <Field label="연결당 최대 동시 Stream"><input aria-label="연결당 최대 동시 Stream" type="number" min="1" max="1000" value={scenario.http2.max_concurrent_streams} onChange={(e)=>patch({http2:{max_concurrent_streams:Math.max(1,Math.min(1000,+e.target.value))}})}/></Field>}
                  {(scenario.protocol === "http1" || scenario.protocol === "http2") && (
                    <>
                      <Field label="Connection 사용 방식">
                        <select
                          value={transactionMode}
                          onChange={(e) =>
                            requestPatch(
                              e.target.value === "single"
                                ? { transactions_per_connection: 1 }
                                : e.target.value === "continuous"
                                  ? {
                                      transactions_per_connection: 0,
                                      keep_alive: true,
                                    }
                                  : {
                                      transactions_per_connection: 10,
                                      keep_alive: true,
                                    },
                            )
                          }
                        >
                          <option value="single">Connection마다 1회</option>
                          <option value="fixed">
                            Connection마다 지정 횟수
                          </option>
                          <option value="continuous">
                            Connection 유지 반복
                          </option>
                        </select>
                      </Field>
                      {transactionMode === "fixed" && (
                        <Field label="Connection당 Transaction 수">
                          <input
                            type="number"
                            min="2"
                            value={scenario.request.transactions_per_connection}
                            onChange={(e) =>
                              requestPatch({
                                transactions_per_connection: Math.max(
                                  2,
                                  +e.target.value,
                                ),
                              })
                            }
                          />
                        </Field>
                      )}
                    </>
                  )}
                </ConfigSection>
              </div>
              <div className="contents">
                <ConfigSection
                  icon={ShieldCheck}
                  title="2. 보안"
                  description="TLS 버전과 인증서 정책을 관리합니다."
                >
                  <label className="flex items-center gap-3 rounded-xl border border-line bg-inset p-3 text-xs font-bold">
                    <input
                      type="checkbox"
                      aria-label="TLS 활성화"
                      checked={scenario.tls.enabled}
                      disabled={scenario.protocol === "http2"}
                      onChange={(e) =>
                        patch({
                          tls: { ...scenario.tls, enabled: e.target.checked },
                        })
                      }
                      className="size-4 accent-signal"
                    />{" "}
                    TLS 활성화
                  </label>
                  {scenario.tls.enabled && (
                    <>
                      <Field label="TLS 버전">
                        <select
                          aria-label="TLS 버전"
                          value={scenario.tls.version}
                          onChange={(e) =>
                            patch({
                              tls: {
                                ...scenario.tls,
                                version: e.target
                                  .value as Scenario["tls"]["version"],
                                cipher_suite: null,
                              },
                            })
                          }
                        >
                          <option value="tls13">TLS 1.3</option>
                          <option value="tls12">TLS 1.2</option>
                        </select>
                      </Field>
                      <details className="rounded-xl border border-dashed border-line p-3">
                        <summary className="cursor-pointer text-xs font-bold text-dim">
                          TLS 고급 설정
                        </summary>
                        <div className="mt-3 space-y-3">
                          <Field label="Cipher suite">
                            <select
                              value={scenario.tls.cipher_suite ?? ""}
                              onChange={(e) =>
                                patch({
                                  tls: {
                                    ...scenario.tls,
                                    cipher_suite: e.target.value || null,
                                  },
                                })
                              }
                            >
                              <option value="">rustls 기본값</option>
                              {(scenario.tls.version === "tls13"
                                ? [
                                    "TLS13_AES_256_GCM_SHA384",
                                    "TLS13_AES_128_GCM_SHA256",
                                    "TLS13_CHACHA20_POLY1305_SHA256",
                                  ]
                                : [
                                    "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
                                    "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
                                    "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
                                    "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
                                    "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
                                    "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256",
                                  ]
                              ).map((cipher) => (
                                <option key={cipher} value={cipher}>
                                  {cipher}
                                </option>
                              ))}
                            </select>
                          </Field>
                          <Field label="Server name (SNI)">
                            <input
                              value={scenario.tls.server_name}
                              onChange={(e) =>
                                patch({
                                  tls: {
                                    ...scenario.tls,
                                    server_name: e.target.value,
                                  },
                                })
                              }
                            />
                          </Field>
                          <Button onClick={generateCertificate}>
                            <ShieldCheck size={14} />
                            테스트 인증서 자동 생성
                          </Button>
                          <Field label="Server certificate PEM">
                            <input
                              type="file"
                              accept=".pem,.crt"
                              onChange={(e) =>
                                readPem("server_cert_pem", e.target.files?.[0])
                              }
                            />
                            <textarea
                              rows={3}
                              value={scenario.tls.server_cert_pem ?? ""}
                              onChange={(e) =>
                                patch({
                                  tls: {
                                    ...scenario.tls,
                                    server_cert_pem: e.target.value || null,
                                  },
                                })
                              }
                            />
                          </Field>
                          <Field label="Server private key PEM">
                            <input
                              type="file"
                              accept=".pem,.key"
                              onChange={(e) =>
                                readPem("server_key_pem", e.target.files?.[0])
                              }
                            />
                            <textarea
                              rows={3}
                              value={scenario.tls.server_key_pem ?? ""}
                              onChange={(e) =>
                                patch({
                                  tls: {
                                    ...scenario.tls,
                                    server_key_pem: e.target.value || null,
                                  },
                                })
                              }
                            />
                          </Field>
                          <label className="flex items-center gap-3 rounded-xl border border-line bg-inset p-3 text-xs font-bold">
                            <input
                              type="checkbox"
                              checked={scenario.tls.verify_peer}
                              onChange={(e) =>
                                patch({
                                  tls: {
                                    ...scenario.tls,
                                    verify_peer: e.target.checked,
                                  },
                                })
                              }
                              className="size-4 accent-signal"
                            />{" "}
                            인증서 검증
                          </label>
                          {scenario.tls.verify_peer && (
                            <Field label="CA certificate PEM">
                              <input
                                type="file"
                                accept=".pem,.crt"
                                onChange={(e) =>
                                  readPem("ca_pem", e.target.files?.[0])
                                }
                              />
                              <textarea
                                rows={3}
                                value={scenario.tls.ca_pem ?? ""}
                                onChange={(e) =>
                                  patch({
                                    tls: {
                                      ...scenario.tls,
                                      ca_pem: e.target.value || null,
                                    },
                                  })
                                }
                              />
                            </Field>
                          )}
                        </div>
                      </details>
                    </>
                  )}
                </ConfigSection>
                <ConfigSection
                  icon={Layers3}
                  title="4. 부하"
                  description="목표 VU와 결과 집계 구간을 순서대로 구성합니다."
                >
                  {scenario.load_stages.map((stage, index) => (
                    <article
                      className="stage-title rounded-2xl border border-line bg-inset p-3"
                      key={index}
                    >
                      <div className="mb-3 flex items-center gap-2">
                        <span className="mono-numbers grid size-7 place-items-center rounded-lg bg-signal/10 text-[10px] font-bold text-signal">
                          {index + 1}
                        </span>
                        <input
                          aria-label={`Stage ${index + 1} 이름`}
                          className="min-w-0 flex-1 border-0 bg-transparent text-sm font-bold outline-none"
                          value={stage.name}
                          onChange={(e) =>
                            stagePatch(index, { name: e.target.value })
                          }
                        />
                        <Button
                          variant="ghost"
                          className="size-8 min-h-8 px-0"
                          onClick={() => moveStage(index, -1)}
                          aria-label="위로"
                        >
                          <ArrowUp size={13} />
                        </Button>
                        <Button
                          variant="ghost"
                          className="size-8 min-h-8 px-0"
                          onClick={() => moveStage(index, 1)}
                          aria-label="아래로"
                        >
                          <ArrowDown size={13} />
                        </Button>
                        <Button
                          variant="ghost"
                          className="size-8 min-h-8 px-0 text-critical"
                          disabled={scenario.load_stages.length === 1}
                          onClick={() =>
                            syncStages(
                              scenario.load_stages.filter(
                                (_, i) => i !== index,
                              ),
                            )
                          }
                          aria-label="삭제"
                        >
                          <Trash2 size={13} />
                        </Button>
                      </div>
                      <div className="grid gap-2 sm:grid-cols-3">
                        <Field label="형태">
                          <select
                            value={stage.mode}
                            onChange={(e) =>
                              stagePatch(index, {
                                mode: e.target.value as LoadStage["mode"],
                              })
                            }
                          >
                            <option value="ramp">Ramp</option>
                            <option value="hold">Hold</option>
                          </select>
                        </Field>
                        <Field label="시간 (초)">
                          <input
                            type="number"
                            min="1"
                            value={stage.duration_secs}
                            onChange={(e) =>
                              stagePatch(index, {
                                duration_secs: Math.max(1, +e.target.value),
                              })
                            }
                          />
                        </Field>
                        <Field label="목표 VU">
                          <input
                            type="number"
                            min="0"
                            value={stage.target_virtual_clients}
                            onChange={(e) =>
                              stagePatch(index, {
                                target_virtual_clients: Math.max(
                                  0,
                                  +e.target.value,
                                ),
                              })
                            }
                          />
                        </Field>
                      </div>
                      <label className="mt-3 flex items-center gap-2 text-[10px] font-bold text-dim">
                        <input
                          type="checkbox"
                          checked={stage.include_in_results}
                          onChange={(e) =>
                            stagePatch(index, {
                              include_in_results: e.target.checked,
                            })
                          }
                          className="accent-signal"
                        />{" "}
                        결과 집계에 포함
                      </label>
                    </article>
                  ))}
                  <Button
                    className="w-full"
                    onClick={() =>
                      syncStages([
                        ...scenario.load_stages,
                        {
                          name: `Stage ${scenario.load_stages.length + 1}`,
                          mode: "hold",
                          duration_secs: 10,
                          target_virtual_clients: scenario.virtual_clients,
                          include_in_results: true,
                        },
                      ])
                    }
                  >
                    + Stage 추가
                  </Button>
                </ConfigSection>
              </div>
            </div>
            <div className="mt-5 rounded-2xl border border-signal/25 bg-signal/5 p-3">
              <Field label="개별 시험 이름">
                <input
                  aria-label="개별 시험 이름"
                  placeholder="비워 두면 구성 이름과 시작 일시로 자동 생성"
                  value={runName}
                  maxLength={120}
                  onChange={(e) => setRunName(e.target.value)}
                />
              </Field>
              <p className="mt-2 text-[10px] text-dim">
                실제 시험 일시는 서버가 시작 시점에 자동 기록합니다.
              </p>
            </div>
            {error && (
              <p className="mt-4 rounded-xl border border-critical/30 bg-critical/10 p-3 text-xs text-critical">
                {error}
              </p>
            )}
            {captureBlocked && <p role="alert" className="mt-4 rounded-xl border border-warn/30 bg-warn/10 p-3 text-xs text-warn">{selectedCapture ? `선택한 capture에 지원 가능한 ${scenario.protocol === "http1" ? "HTTP/1.1 transaction" : "양방향 TCP flow"}이 없어 실행할 수 없습니다.` : "분석된 PCAP/PCAPNG를 선택해야 실행할 수 있습니다."}</p>}
            <Button
              variant="primary"
              className="mt-5 w-full py-3"
              onClick={start}
              disabled={!!activeRun || agents.length < 2 || captureBlocked}
            >
              <Play size={15} />
              시험 시작
            </Button>
          </Panel>
        )}
        {tab === "live" && (
          <Panel className="p-4 sm:p-6">
            <SectionTitle
              eyebrow="LIVE TELEMETRY"
              title="실시간 모니터링"
              aside={
                activeRun && (
                  <div className="flex gap-2">
                    {status === "일시정지" ? (
                      <Button onClick={() => control("resume")}>
                        <Play size={14} />
                        재개
                      </Button>
                    ) : (
                      <Button onClick={() => control("pause")}>
                        <Pause size={14} />
                        일시정지
                      </Button>
                    )}
                    <Button variant="danger" onClick={() => control("stop")}>
                      <Square size={13} />
                      중지
                    </Button>
                  </div>
                )
              }
            />
            <div className="mb-4 grid gap-2 sm:grid-cols-2 xl:grid-cols-4">
              {[
                ["현재 Stage", currentStage?.name ?? "고정 부하"],
                [
                  "목표 VU",
                  latestClient?.desired_virtual_clients ??
                    scenario.virtual_clients,
                ],
                ["Active", latestClient?.active_connections ?? 0],
                [
                  "집계",
                  latestClient?.included_in_results === false ? "제외" : "포함",
                ],
              ].map(([label, value]) => (
                <div
                  key={label}
                  className="rounded-xl border border-line bg-inset px-3 py-2.5"
                >
                  <span className="text-[9px] font-bold uppercase tracking-[.12em] text-dim">
                    {label}
                  </span>
                  <b className="mono-numbers mt-1 block text-sm">{value}</b>
                </div>
              ))}
            </div>
            <div className="mb-5 grid grid-cols-2 gap-2 md:grid-cols-4 xl:grid-cols-8">
              <MetricCard
                label="CPS"
                value={latestClient?.cps.toFixed(0) ?? "—"}
                icon={Gauge}
              />
              <MetricCard
                label="ACTIVE"
                value={latestClient?.active_connections.toLocaleString() ?? "—"}
                icon={Users}
                tone="info"
              />
              <MetricCard
                label="TPS"
                value={latestClient?.tps.toFixed(0) ?? "—"}
                icon={Activity}
              />
              <MetricCard
                label="HTTP P99"
                value={
                  scenario.protocol === "http1" && latestClient
                    ? latestClient.http_latency_p99_ms.toFixed(2)
                    : "N/A"
                }
                unit="ms"
                icon={Clock3}
                tone="warn"
              />
              <MetricCard
                label="CLIENT TX"
                value={
                  latestClient ? formatBandwidth(latestClient.tx_bps) : "—"
                }
                icon={Radio}
              />
              <MetricCard
                label="SERVER RX"
                value={
                  latestServer ? formatBandwidth(latestServer.rx_bps) : "—"
                }
                icon={Server}
                tone="violet"
              />
              <MetricCard
                label="SERVER TX"
                value={
                  latestServer ? formatBandwidth(latestServer.tx_bps) : "—"
                }
                icon={Wifi}
                tone="violet"
              />
              <MetricCard
                label="CLIENT RX"
                value={
                  latestClient ? formatBandwidth(latestClient.rx_bps) : "—"
                }
                icon={Globe2}
                tone="info"
              />
            </div>
            <Suspense fallback={<p role="status">차트 불러오는 중…</p>}><TelemetryCharts
              points={points}
              scenario={scenario}
              theme={theme}
              live
              running={!!activeRun}
            /></Suspense>
          </Panel>
        )}
        {tab === "results" && (
          <Suspense fallback={<p role="status">결과 불러오는 중…</p>}><RunHistory
            refreshKey={`${activeRun ?? ""}:${status}`}
            theme={theme}
          /></Suspense>
        )}
      </main>
    </div>
  );
}

function ConfigSection({
  icon: Icon,
  title,
  description,
  children,
}: {
  icon: React.ElementType;
  title: string;
  description: string;
  children: React.ReactNode;
}) {
  const order = title.startsWith("1.") ? "order-1" : title.startsWith("2.") ? "order-2" : title.startsWith("3.") ? "order-3" : "order-4";
  return (
    <section className={`${order} min-w-0 overflow-hidden rounded-2xl border border-line bg-raised/45 p-4`}>
      <div className="mb-4 flex min-w-0 gap-3">
        <span className="grid size-9 shrink-0 place-items-center rounded-xl bg-signal/10 text-signal">
          <Icon size={16} />
        </span>
        <div className="min-w-0">
          <h3 className="m-0 text-sm font-bold">{title}</h3>
          <p className="mt-1 text-[10px] leading-relaxed text-dim">
            {description}
          </p>
        </div>
      </div>
      <div className="min-w-0 space-y-3">{children}</div>
    </section>
  );
}

function formatBytes(bytes: number) {
  if (bytes >= 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(bytes % (1024 * 1024) ? 1 : 0)}MB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(bytes % 1024 ? 1 : 0)}KB`;
  return `${bytes}B`;
}

function PayloadEditor({
  label,
  value,
  artifacts,
  uploading,
  onUpload,
  onChange,
}: {
  label: string;
  value: PayloadProfile;
  artifacts: Artifact[];
  uploading: boolean;
  onUpload: (file: File) => Promise<Artifact>;
  onChange: (value: PayloadProfile) => void;
}) {
  const patch = (p: Partial<PayloadProfile>) => onChange({ ...value, ...p });
  const payloads = artifacts.filter((a) => a.kind === "payload");
  const bytes =
    value.kind === "text"
      ? new TextEncoder().encode(value.text).length
      : value.kind === "empty"
        ? 0
        : value.size_bytes;
  const choose = (id: string) => {
    const artifact = payloads.find((a) => a.id === id);
    patch({ artifact_id: id || null, size_bytes: artifact?.size_bytes ?? 0 });
  };
  return (
    <fieldset className="min-w-0 rounded-xl border border-line bg-inset p-3">
      <legend className="px-1 text-xs font-bold">{label}</legend>
      <Field label="종류">
        <select
          aria-label={`${label} 종류`}
          value={value.kind}
          onChange={(e) =>
            patch({ kind: e.target.value as PayloadProfile["kind"] })
          }
        >
          <option value="empty">없음</option>
          <option value="fixed">고정 byte</option>
          <option value="text">UTF-8 문자열</option>
          <option value="random">Random</option>
          <option value="file">파일 artifact</option>
        </select>
      </Field>
      {value.kind === "text" && (
        <Field label="문자열">
          <textarea
            aria-label={`${label} 문자열`}
            rows={4}
            value={value.text}
            onChange={(e) => patch({ text: e.target.value })}
          />
        </Field>
      )}
      {(value.kind === "fixed" || value.kind === "random") && (
        <Field label="크기 (bytes)">
          <input
            aria-label={`${label} 크기 (bytes)`}
            type="number"
            min="0"
            max={64 * 1024 * 1024}
            value={value.size_bytes}
            onChange={(e) =>
              patch({
                size_bytes: Math.max(
                  0,
                  Math.min(64 * 1024 * 1024, +e.target.value),
                ),
              })
            }
          />
        </Field>
      )}
      {value.kind === "random" && (
        <Field label="Random 형식">
          <select
            aria-label={`${label} Random 형식`}
            value={value.random_format}
            onChange={(e) =>
              patch({
                random_format: e.target
                  .value as PayloadProfile["random_format"],
              })
            }
          >
            <option value="binary">Binary</option>
            <option value="printable_ascii">Printable ASCII</option>
          </select>
        </Field>
      )}
      {value.kind === "file" && (
        <>
          <Field label="업로드">
            <input
              type="file"
              disabled={uploading}
              onChange={async (e) => {
                const file = e.target.files?.[0];
                if (file) {
                  const artifact = await onUpload(file);
                  choose(artifact.id);
                }
              }}
            />
          </Field>
          <Field label="Payload artifact">
            <select
              value={value.artifact_id ?? ""}
              onChange={(e) => choose(e.target.value)}
            >
              <option value="">선택하세요</option>
              {payloads.map((a) => (
                <option key={a.id} value={a.id}>
                  {a.name} · {a.size_bytes.toLocaleString()} bytes
                </option>
              ))}
            </select>
          </Field>
        </>
      )}
      <p className="mt-2 font-mono text-[10px] text-dim">
        실제 크기 {bytes.toLocaleString()} bytes
      </p>
    </fieldset>
  );
}

function CaptureEditor({
  artifacts,
  protocol,
  selected,
  uploading,
  onUpload,
  onSelect,
}: {
  artifacts: Artifact[];
  protocol: Scenario["protocol"];
  selected: string | null;
  uploading: boolean;
  onUpload: (file: File) => Promise<Artifact>;
  onSelect: (id: string | null) => void;
}) {
  const captures = artifacts.filter((a) => a.kind === "pcap"),
    current = captures.find((a) => a.id === selected),
    excluded = Object.entries(current?.analysis?.exclusions ?? {}).filter(
      ([, count]) => count > 0,
    );
  const supported = current ? protocol === "http2" ? current.analysis?.http2_flow_count ?? 0 : protocol === "http1" ? current.analysis?.http_flow_count ?? 0 : current.analysis?.supported_flow_count ?? 0 : 0;
  return (
    <div className="space-y-3 rounded-xl border border-dashed border-line bg-inset p-3">
      <Field label="PCAP / PCAPNG 업로드">
        <input
          type="file"
          accept=".pcap,.pcapng"
          disabled={uploading}
          onChange={async (e) => {
            const file = e.target.files?.[0];
            if (file) {
              const artifact = await onUpload(file);
              onSelect(artifact.id);
            }
          }}
        />
      </Field>
      {uploading && <p role="status" className="rounded-lg bg-signal/10 p-2 text-xs font-bold text-signal">업로드 및 분석 중…</p>}
      <Field label="분석된 Capture">
        <select
          value={selected ?? ""}
          onChange={(e) => onSelect(e.target.value || null)}
        >
          <option value="">선택하세요</option>
          {captures.map((a) => (
            <option key={a.id} value={a.id}>
              {a.name} · {a.format} · {a.analysis?.supported_flow_count ?? 0}{" "}
              flows · {a.analysis?.http_transaction_count ?? 0} HTTP tx
            </option>
          ))}
        </select>
      </Field>
      {current && (
        <div className="grid gap-2 text-[10px] sm:grid-cols-2" aria-label="Capture 분석 요약">
          <p className={`rounded-lg p-2 sm:col-span-2 ${supported > 0 ? "bg-signal/10 text-signal" : "bg-warn/10 text-warn"}`}><b className="block">현재 프로토콜 지원</b>{supported}개 {protocol === "http2" ? "HTTP/2 흐름" : protocol === "http1" ? "HTTP 흐름" : "TCP 흐름"}</p>
          <p className="rounded-lg bg-raised p-2">
            <b className="block text-signal">지원 TCP 흐름</b>
            {current.analysis?.supported_flow_count ?? 0}개
          </p>
          <p className="rounded-lg bg-raised p-2">
            <b className="block text-signal">HTTP/1.1</b>
            {current.analysis?.http_flow_count ?? 0}개 흐름 ·{" "}
            {current.analysis?.http_transaction_count ?? 0} transactions
          </p>
          <p className="rounded-lg bg-raised p-2"><b className="block text-signal">HTTP/2</b>{current.analysis?.http2_flow_count ?? 0}개 흐름 · {current.analysis?.http2_transaction_count ?? 0} transactions</p>
          <p className="rounded-lg bg-raised p-2">
            <b className="block text-signal">Retransmission</b>
            {(current.analysis?.retransmitted_bytes ?? 0).toLocaleString()}{" "}
            bytes
          </p>
          {excluded.length > 0 && (
            <p className="rounded-lg bg-raised p-2 sm:col-span-2">
              <b className="block text-dim">제외 요약</b>
              {excluded
                .map(([reason, count]) => `${reason}: ${count}`)
                .join(" · ")}
            </p>
          )}
        </div>
      )}
    </div>
  );
}

createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
