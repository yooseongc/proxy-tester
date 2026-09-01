import { useMemo, useState } from "react";
import { api } from "./api";
import { NetworkPlanDetails, type NetworkNodePlan } from "./Diagnostics";
import { Button, Field, Panel, SectionTitle, StatusBadge } from "./ui";
import type { Agent } from "./model";

type Endpoint = { node_id: string; interface_name: string; start_cidr: string; count: number };
type Draft = {
  id: string;
  name: string;
  provisioning: "managed_namespace" | "operator_managed";
  allow_virtual_interfaces: boolean;
  client_endpoint: Endpoint;
  server_endpoint: Endpoint;
  mtu: number;
  diagnostic_port: number;
  path_probe_enabled: boolean;
};
type Plan = {
  operation_id: string;
  profile_revision_id: string;
  plan_token: string;
  expires_at: string;
  detail: { plans: Record<string, NetworkNodePlan> };
};
type Revision = { id: string; revision: number; sha256: string; body: Draft };

const endpoint = (node = "", iface = "eth1", cidr = "10.20.0.10/24"): Endpoint => ({
  node_id: node,
  interface_name: iface,
  start_cidr: cidr,
  count: 16,
});

export function NetworkSetup({
  agents,
  onPrepared,
  onOpenDiagnostics,
}: {
  agents: Agent[];
  onPrepared: (revisionId: string) => void;
  onOpenDiagnostics: (operationId: string) => void;
}) {
  const first = agents[0]?.id ?? "",
    second = agents[1]?.id ?? first;
  const [draft, setDraft] = useState<Draft>(() => ({
    id: crypto.randomUUID(),
    name: "Managed direct network",
    provisioning: "managed_namespace",
    allow_virtual_interfaces: false,
    client_endpoint: endpoint(first, "eth1", "10.20.0.10/24"),
    server_endpoint: endpoint(second, "eth1", "10.20.0.100/24"),
    mtu: 1370,
    diagnostic_port: 39000,
    path_probe_enabled: true,
  }));
  const [plan, setPlan] = useState<Plan | null>(null),
    [revision, setRevision] = useState<Revision | null>(null),
    [busy, setBusy] = useState(false),
    [message, setMessage] = useState("초안"),
    [error, setError] = useState("");
  const interfaces = useMemo(
    () =>
      Object.fromEntries(
        agents.map((a) => [a.id, (a.inventory?.interfaces ?? []).map((i) => i.name)]),
      ),
    [agents],
  );
  const invalidatePlan = () => {
    if (plan) {
      setPlan(null);
      setMessage("초안 변경 · 재계획 필요");
    }
  };
  const updateDraft = (update: (current: Draft) => Draft) => {
    invalidatePlan();
    setDraft(update);
  };
  const patchEndpoint = (side: "client_endpoint" | "server_endpoint", value: Partial<Endpoint>) =>
    updateDraft((current) => ({
      ...current,
      [side]: { ...current[side], ...value },
    }));
  const planIsCurrent =
    !plan ||
    Object.entries(plan.detail.plans).every(([nodeId, nodePlan]) => {
      const agent = agents.find((candidate) => candidate.id === nodeId);
      const current = agent?.inventory?.fingerprint;
      return !!agent && (!current || current === nodePlan.inventory_fingerprint);
    });
  const execute = async (action: () => Promise<void>) => {
    setBusy(true);
    setError("");
    try {
      await action();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  const createPlan = () =>
    execute(async () => {
      setPlan(null);
      await api("/api/network/profiles", { method: "POST", body: JSON.stringify(draft) });
      const value = await api<Plan>(`/api/network/profiles/${draft.id}/plan`, { method: "POST" });
      setPlan(value);
      setMessage("계획 검토");
    });
  const apply = () =>
    plan &&
    planIsCurrent &&
    execute(async () => {
      try {
        await api(`/api/network/operations/${plan.operation_id}/apply`, {
          method: "POST",
          body: JSON.stringify({ plan_token: plan.plan_token }),
        });
      } catch (cause) {
        setPlan(null);
        setMessage("적용 실패 · 재계획 필요");
        throw cause;
      }
      const revisions = await api<Revision[]>(`/api/network/revisions?profile_id=${draft.id}`);
      const selected = revisions.find((r) => r.id === plan.profile_revision_id) ?? null;
      setRevision(selected);
      onPrepared(plan.profile_revision_id);
      setMessage("준비됨");
    });
  const diagnose = () =>
    revision &&
    execute(async () => {
      const value = await api<{ ok: boolean; checks: { name: string; ok: boolean }[] }>(
        "/api/network/diagnose",
        { method: "POST", body: JSON.stringify({ profile_revision_id: revision.id }) },
      );
      setMessage(
        value.ok
          ? "진단 통과"
          : `진단 실패 (${value.checks
              .filter((c) => !c.ok)
              .map((c) => c.name)
              .join(", ")})`,
      );
    });
  const teardown = () =>
    revision &&
    execute(async () => {
      await api(`/api/network/revisions/${revision.id}/teardown`, { method: "POST" });
      setRevision(null);
      setPlan(null);
      setMessage("해제됨");
    });
  return (
    <Panel className="mb-4 p-4 sm:p-6">
      <SectionTitle
        eyebrow="NETWORK PROFILE"
        title="시험 네트워크 구성"
        aside={
          <StatusBadge tone={revision ? "live" : "neutral"}>
            {plan && !planIsCurrent ? "Agent 상태 변경 · 재계획 필요" : message}
          </StatusBadge>
        }
      />
      <p className="mb-4 text-xs text-dim">
        관리 인터페이스는 보호됩니다. 적용 전 실제 명령과 롤백 계획을 검토하고, 준비된 revision만
        시험에 고정됩니다.
      </p>
      <div className="grid gap-3 md:grid-cols-2">
        <Field label="네트워크 구성 이름">
          <input
            value={draft.name}
            disabled={!!revision}
            onChange={(e) => updateDraft((current) => ({ ...current, name: e.target.value }))}
          />
        </Field>
        <Field label="네트워크 소유권">
          <select
            value={draft.provisioning}
            disabled={!!revision}
            onChange={(event) =>
              updateDraft((current) => ({
                ...current,
                provisioning: event.target.value as Draft["provisioning"],
              }))
            }
          >
            <option value="managed_namespace">프로그램 관리 namespace</option>
            <option value="operator_managed">기존 토폴로지 사용 (변경 없음)</option>
          </select>
        </Field>
        {draft.provisioning === "managed_namespace" && (
          <label className="flex items-center gap-2 rounded-xl border border-warn/30 p-3 text-xs text-warn">
            <input
              type="checkbox"
              checked={draft.allow_virtual_interfaces}
              disabled={!!revision}
              onChange={(event) =>
                updateDraft((current) => ({
                  ...current,
                  allow_virtual_interfaces: event.target.checked,
                }))
              }
            />
            격리된 테스트용 virtual interface 이동 허용
          </label>
        )}
        <Field label="MTU">
          <input
            type="number"
            min={576}
            max={9216}
            value={draft.mtu}
            disabled={!!revision}
            onChange={(e) =>
              updateDraft((current) => ({ ...current, mtu: Number(e.target.value) }))
            }
          />
        </Field>
        {(["client_endpoint", "server_endpoint"] as const).map((side) => (
          <div key={side} className="rounded-xl border border-line p-3">
            <strong className="mb-3 block text-xs">
              {side === "client_endpoint" ? "Client endpoint" : "Server endpoint"}
            </strong>
            <div className="grid gap-2 sm:grid-cols-2">
              <Field label="Node">
                <select
                  value={draft[side].node_id}
                  disabled={!!revision}
                  onChange={(e) =>
                    patchEndpoint(side, {
                      node_id: e.target.value,
                      interface_name: interfaces[e.target.value]?.[0] ?? "",
                    })
                  }
                >
                  <option value="">선택</option>
                  {agents.map((a) => (
                    <option key={a.id} value={a.id}>
                      {a.id} · {a.hostname}
                    </option>
                  ))}
                </select>
              </Field>
              <Field label="Interface">
                <select
                  value={draft[side].interface_name}
                  disabled={!!revision}
                  onChange={(e) => patchEndpoint(side, { interface_name: e.target.value })}
                >
                  <option value={draft[side].interface_name}>
                    {draft[side].interface_name || "선택"}
                  </option>
                  {(interfaces[draft[side].node_id] ?? [])
                    .filter((i) => i !== draft[side].interface_name)
                    .map((i) => (
                      <option key={i}>{i}</option>
                    ))}
                </select>
              </Field>
              <Field label="첫 IP/CIDR">
                <input
                  value={draft[side].start_cidr}
                  disabled={!!revision}
                  onChange={(e) => patchEndpoint(side, { start_cidr: e.target.value })}
                />
              </Field>
              <Field label="주소 수">
                <input
                  type="number"
                  min={1}
                  max={4096}
                  value={draft[side].count}
                  disabled={!!revision}
                  onChange={(e) => patchEndpoint(side, { count: Number(e.target.value) })}
                />
              </Field>
            </div>
          </div>
        ))}
      </div>
      {draft.provisioning === "operator_managed" && !revision && (
        <p className="mt-3 rounded-xl border border-signal/30 bg-signal/5 p-3 text-xs text-dim">
          선택한 interface와 IP를 그대로 사용합니다. veth·bridge·VXLAN을 이동하거나 삭제하지 않으며,
          입력한 모든 IPv4 주소가 이미 구성되어 있어야 합니다.
        </p>
      )}
      {plan && planIsCurrent && !revision && (
        <div className="my-4 rounded-xl border border-signal/30 bg-signal/5 p-3 text-xs">
          <strong>적용 계획</strong>
          <NetworkPlanDetails plans={plan.detail.plans} />
          <p className="mt-2 text-dim">토큰 만료: {new Date(plan.expires_at).toLocaleString()}</p>
        </div>
      )}
      {error && (
        <p role="alert" className="mt-3 rounded-xl border border-warn/30 p-3 text-xs text-warn">
          {error}
        </p>
      )}
      <div className="mt-4 flex flex-wrap gap-2">
        {!revision && (
          <Button
            variant="primary"
            disabled={busy}
            onClick={plan && planIsCurrent ? apply : createPlan}
          >
            {plan && planIsCurrent ? "계획 적용" : "저장 및 계획"}
          </Button>
        )}
        {revision && (
          <>
            <Button variant="primary" disabled={busy} onClick={diagnose}>
              진단
            </Button>
            <Button disabled={busy} onClick={teardown}>
              구성 해제
            </Button>
          </>
        )}
        {plan && <Button onClick={() => onOpenDiagnostics(plan.operation_id)}>상세 로그</Button>}
      </div>
    </Panel>
  );
}
