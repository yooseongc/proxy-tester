import { useCallback, useEffect, useMemo, useState } from "react";
import { Clipboard, RefreshCw, X } from "lucide-react";
import { api } from "./api";
import { Button, StatusBadge } from "./ui";

export type DiagnosticContext = { kind: "network" | "run"; id: string };
export type CommandSpec = { program: string; args: string[] };
export type NetworkNodePlan = {
  node_id: string;
  inventory_fingerprint: string;
  semantic_changes: string[];
  warnings: string[];
  endpoints: { role: string; namespace: string; interface: string; addresses: string[] }[];
  commands: CommandSpec[];
  rollback_commands: CommandSpec[];
};
export type DiagnosticEvent = {
  source: string;
  agent_id?: string | null;
  node_id?: string | null;
  stage: string;
  status: string;
  detail: unknown;
  created_at: string;
};
type DiagnosticResponse = {
  id: string;
  status: string;
  kind?: string;
  error?: string | null;
  detail?: unknown;
  participants?: {
    agent_id: string;
    role: number;
    phase: string;
    error?: string | null;
    updated_at: string;
  }[];
  events: DiagnosticEvent[];
};

const commandLine = (command: CommandSpec) =>
  [
    command.program,
    ...command.args.map((value) => (value.includes(" ") ? `"${value}"` : value)),
  ].join(" ");

export function NetworkPlanDetails({ plans }: { plans: Record<string, NetworkNodePlan> }) {
  return (
    <div className="mt-3 space-y-3">
      {Object.entries(plans).map(([node, plan]) => (
        <article className="rounded-xl border border-line bg-inset p-3" key={node}>
          <div className="flex items-center justify-between gap-3">
            <strong>{node}</strong>
            <span className="font-mono text-[9px] text-dim">
              {plan.inventory_fingerprint?.slice(0, 12) || "fingerprint 없음"}
            </span>
          </div>
          <ul className="mt-2 list-disc space-y-1 pl-4 text-dim">
            {plan.semantic_changes?.map((change) => (
              <li key={change}>{change}</li>
            ))}
          </ul>
          {plan.endpoints?.map((endpoint) => (
            <div className="mt-2 rounded-lg border border-line p-2" key={endpoint.namespace}>
              <b>{endpoint.role}</b> · {endpoint.interface} → {endpoint.namespace}
              <p className="mt-1 break-all font-mono text-[9px] text-dim">
                {endpoint.addresses.join(", ")}
              </p>
            </div>
          ))}
          {plan.warnings?.map((warning) => (
            <p className="mt-2 text-warn" key={warning}>
              {warning}
            </p>
          ))}
          <details className="mt-3 rounded-lg border border-dashed border-line p-2">
            <summary className="cursor-pointer font-bold">실행 명령과 rollback</summary>
            <CommandList title="실행 명령" commands={plan.commands ?? []} />
            <CommandList title="Rollback 명령" commands={plan.rollback_commands ?? []} />
          </details>
        </article>
      ))}
    </div>
  );
}

function CommandList({ title, commands }: { title: string; commands: CommandSpec[] }) {
  return (
    <div className="mt-3">
      <b className="text-[10px] text-dim">{title}</b>
      <pre className="mt-1 max-h-52 overflow-auto whitespace-pre-wrap break-all rounded-lg bg-canvas p-2 font-mono text-[9px] leading-relaxed">
        {commands.length ? commands.map(commandLine).join("\n") : "명령 없음"}
      </pre>
    </div>
  );
}

export function DiagnosticDrawer({
  context,
  refreshKey,
  onClose,
}: {
  context: DiagnosticContext | null;
  refreshKey: number;
  onClose: () => void;
}) {
  const [data, setData] = useState<DiagnosticResponse | null>(null);
  const [error, setError] = useState("");
  const [agent, setAgent] = useState("all");
  const [failuresOnly, setFailuresOnly] = useState(false);
  const load = useCallback(() => {
    if (!context) return Promise.resolve();
    const path =
      context.kind === "network"
        ? `/api/network/operations/${context.id}?limit=200`
        : `/api/runs/${context.id}/diagnostics?limit=200`;
    return api<DiagnosticResponse>(path)
      .then((value) => {
        setData(value);
        setError("");
      })
      .catch((cause) => setError(cause instanceof Error ? cause.message : String(cause)));
  }, [context]);
  useEffect(() => {
    void load();
  }, [load]);
  useEffect(() => {
    void load();
  }, [refreshKey, load]);
  const agents = useMemo(
    () =>
      [
        ...new Set(data?.events.map((event) => event.agent_id ?? event.node_id).filter(Boolean)),
      ] as string[],
    [data],
  );
  const events = (data?.events ?? []).filter((event) => {
    const eventAgent = event.agent_id ?? event.node_id;
    const failed = ["failed", "error", "timeout", "quarantined"].includes(event.status);
    return (agent === "all" || eventAgent === agent) && (!failuresOnly || failed);
  });
  if (!context) return null;
  return (
    <>
      <button
        aria-label="상세 로그 닫기"
        className="fixed inset-0 z-[100] cursor-default bg-black/45"
        onClick={onClose}
      />
      <aside
        aria-label="상세 로그"
        className="fixed inset-y-0 right-0 z-[101] flex h-full w-full max-w-2xl flex-col border-l border-line bg-panel shadow-2xl"
      >
        <header className="flex items-start justify-between gap-3 border-b border-line p-4">
          <div>
            <p className="font-mono text-[9px] font-bold tracking-widest text-signal">
              DIAGNOSTICS
            </p>
            <h2 className="mt-1 text-lg font-bold">
              {context.kind === "network" ? "네트워크 작업" : "Run"} 상세 로그
            </h2>
            <p className="mt-1 break-all font-mono text-[9px] text-dim">{context.id}</p>
          </div>
          <div className="flex gap-2">
            <Button aria-label="로그 새로고침" onClick={() => void load()}>
              <RefreshCw size={14} />
            </Button>
            <Button aria-label="상세 로그 닫기" onClick={onClose}>
              <X size={14} />
            </Button>
          </div>
        </header>
        <div className="flex flex-wrap items-center gap-2 border-b border-line p-3">
          <StatusBadge tone={data?.status === "failed" ? "danger" : "neutral"}>
            {data?.status ?? "불러오는 중"}
          </StatusBadge>
          <select
            aria-label="Agent 로그 필터"
            value={agent}
            onChange={(e) => setAgent(e.target.value)}
          >
            <option value="all">모든 Agent</option>
            {agents.map((value) => (
              <option key={value}>{value}</option>
            ))}
          </select>
          <label className="flex items-center gap-2 text-xs">
            <input
              type="checkbox"
              checked={failuresOnly}
              onChange={(e) => setFailuresOnly(e.target.checked)}
            />
            오류만
          </label>
          <Button
            className="ml-auto"
            disabled={!data}
            onClick={() => data && navigator.clipboard.writeText(JSON.stringify(data, null, 2))}
          >
            <Clipboard size={13} />
            JSON 복사
          </Button>
        </div>
        <div className="flex-1 overflow-auto p-4">
          {error && (
            <p
              role="alert"
              className="rounded-xl border border-critical/30 p-3 text-xs text-critical"
            >
              {error}
            </p>
          )}
          {data?.participants?.length ? (
            <details className="mb-4 rounded-xl border border-line p-3">
              <summary className="cursor-pointer text-xs font-bold">Participant 상태</summary>
              <pre className="mt-2 overflow-auto text-[9px]">
                {JSON.stringify(data.participants, null, 2)}
              </pre>
            </details>
          ) : null}
          <ol className="space-y-2">
            {events.map((event, index) => {
              const failed = ["failed", "error", "timeout", "quarantined"].includes(event.status);
              return (
                <li
                  className={`rounded-xl border p-3 text-xs ${failed ? "border-critical/40 bg-critical/5" : "border-line bg-raised/40"}`}
                  key={`${event.created_at}-${index}`}
                >
                  <div className="flex flex-wrap items-center gap-2">
                    <b>{event.stage}</b>
                    <StatusBadge tone={failed ? "danger" : "neutral"}>{event.status}</StatusBadge>
                    <span className="text-dim">
                      {event.agent_id ?? event.node_id ?? event.source}
                    </span>
                    <time className="ml-auto font-mono text-[9px] text-dim">
                      {new Date(event.created_at).toLocaleString()}
                    </time>
                  </div>
                  <details className="mt-2" open={failed}>
                    <summary className="cursor-pointer text-[10px] text-dim">이벤트 상세</summary>
                    <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap break-all rounded-lg bg-canvas p-2 text-[9px]">
                      {JSON.stringify(event.detail, null, 2)}
                    </pre>
                  </details>
                </li>
              );
            })}
          </ol>
          {data && !events.length && (
            <p className="py-12 text-center text-xs text-dim">표시할 이벤트가 없습니다.</p>
          )}
        </div>
      </aside>
    </>
  );
}
