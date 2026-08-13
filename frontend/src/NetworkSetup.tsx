import { useMemo, useState } from "react";
import { api } from "./api";
import { Button, Field, Panel, SectionTitle, StatusBadge } from "./ui";
import type { Agent } from "./model";

type Endpoint={node_id:string;interface_name:string;start_cidr:string;count:number};
type Draft={id:string;name:string;client_endpoint:Endpoint;server_endpoint:Endpoint;mtu:number;diagnostic_port:number;path_probe_enabled:boolean};
type Plan={operation_id:string;profile_revision_id:string;plan_token:string;expires_at:string;detail:{plans:Record<string,{semantic_changes?:string[];warnings?:string[]}>}};
type Revision={id:string;revision:number;sha256:string;body:Draft};

const endpoint=(node="",iface="eth1",cidr="10.20.0.10/24"):Endpoint=>({node_id:node,interface_name:iface,start_cidr:cidr,count:16});

export function NetworkSetup({agents,onPrepared}:{agents:Agent[];onPrepared:(revisionId:string)=>void}){
 const first=agents[0]?.id??"", second=agents[1]?.id??first;
 const [draft,setDraft]=useState<Draft>(()=>({id:crypto.randomUUID(),name:"Managed direct network",client_endpoint:endpoint(first,"eth1","10.20.0.10/24"),server_endpoint:endpoint(second,"eth1","10.20.0.100/24"),mtu:1370,diagnostic_port:39000,path_probe_enabled:true}));
 const [plan,setPlan]=useState<Plan|null>(null),[revision,setRevision]=useState<Revision|null>(null),[busy,setBusy]=useState(false),[message,setMessage]=useState("초안"),[error,setError]=useState("");
 const interfaces=useMemo(()=>Object.fromEntries(agents.map(a=>[a.id,(a.inventory?.interfaces??[]).map(i=>i.name)])),[agents]);
 const patchEndpoint=(side:"client_endpoint"|"server_endpoint",value:Partial<Endpoint>)=>setDraft(d=>({...d,[side]:{...d[side],...value}}));
 const execute=async(action:()=>Promise<void>)=>{setBusy(true);setError("");try{await action()}catch(e){setError(e instanceof Error?e.message:String(e))}finally{setBusy(false)}};
 const createPlan=()=>execute(async()=>{await api("/api/network/profiles",{method:"POST",body:JSON.stringify(draft)});const value=await api<Plan>(`/api/network/profiles/${draft.id}/plan`,{method:"POST"});setPlan(value);setMessage("계획 검토")});
 const apply=()=>plan&&execute(async()=>{await api(`/api/network/operations/${plan.operation_id}/apply`,{method:"POST",body:JSON.stringify({plan_token:plan.plan_token})});const revisions=await api<Revision[]>(`/api/network/revisions?profile_id=${draft.id}`);const selected=revisions.find(r=>r.id===plan.profile_revision_id)??null;setRevision(selected);onPrepared(plan.profile_revision_id);setMessage("준비됨")});
 const diagnose=()=>revision&&execute(async()=>{const value=await api<{ok:boolean;checks:{name:string;ok:boolean}[]}>("/api/network/diagnose",{method:"POST",body:JSON.stringify({profile_revision_id:revision.id})});setMessage(value.ok?"진단 통과":`진단 실패 (${value.checks.filter(c=>!c.ok).map(c=>c.name).join(", ")})`)});
 const teardown=()=>revision&&execute(async()=>{await api(`/api/network/revisions/${revision.id}/teardown`,{method:"POST"});setRevision(null);setPlan(null);setMessage("해제됨")});
 return <Panel className="mb-4 p-4 sm:p-6">
  <SectionTitle eyebrow="NETWORK PROFILE" title="시험 네트워크 구성" aside={<StatusBadge tone={revision?"live":"neutral"}>{message}</StatusBadge>} />
  <p className="mb-4 text-xs text-dim">관리 인터페이스는 보호됩니다. 적용 전 실제 명령과 롤백 계획을 검토하고, 준비된 revision만 시험에 고정됩니다.</p>
  <div className="grid gap-3 md:grid-cols-2">
   <Field label="구성 이름"><input value={draft.name} disabled={!!revision} onChange={e=>setDraft(d=>({...d,name:e.target.value}))}/></Field>
   <Field label="MTU"><input type="number" min={576} max={9216} value={draft.mtu} disabled={!!revision} onChange={e=>setDraft(d=>({...d,mtu:Number(e.target.value)}))}/></Field>
   {(["client_endpoint","server_endpoint"] as const).map((side)=><div key={side} className="rounded-xl border border-line p-3">
    <strong className="mb-3 block text-xs">{side==="client_endpoint"?"Client endpoint":"Server endpoint"}</strong>
    <div className="grid gap-2 sm:grid-cols-2">
     <Field label="Node"><select value={draft[side].node_id} disabled={!!revision} onChange={e=>patchEndpoint(side,{node_id:e.target.value,interface_name:interfaces[e.target.value]?.[0]??""})}><option value="">선택</option>{agents.map(a=><option key={a.id} value={a.id}>{a.id} · {a.hostname}</option>)}</select></Field>
     <Field label="Interface"><select value={draft[side].interface_name} disabled={!!revision} onChange={e=>patchEndpoint(side,{interface_name:e.target.value})}><option value={draft[side].interface_name}>{draft[side].interface_name||"선택"}</option>{(interfaces[draft[side].node_id]??[]).filter(i=>i!==draft[side].interface_name).map(i=><option key={i}>{i}</option>)}</select></Field>
     <Field label="첫 IP/CIDR"><input value={draft[side].start_cidr} disabled={!!revision} onChange={e=>patchEndpoint(side,{start_cidr:e.target.value})}/></Field>
     <Field label="주소 수"><input type="number" min={1} max={4096} value={draft[side].count} disabled={!!revision} onChange={e=>patchEndpoint(side,{count:Number(e.target.value)})}/></Field>
    </div>
   </div>)}
  </div>
  {plan&&!revision&&<div className="my-4 rounded-xl border border-signal/30 bg-signal/5 p-3 text-xs"><strong>적용 계획</strong>{Object.entries(plan.detail.plans).map(([node,value])=><div key={node} className="mt-2"><span className="font-bold">{node}</span><ul>{value.semantic_changes?.map(change=><li key={change}>{change}</li>)}</ul>{value.warnings?.map(w=><p className="text-warn" key={w}>{w}</p>)}</div>)}<p className="mt-2 text-dim">토큰 만료: {new Date(plan.expires_at).toLocaleString()}</p></div>}
  {error&&<p role="alert" className="mt-3 rounded-xl border border-warn/30 p-3 text-xs text-warn">{error}</p>}
  <div className="mt-4 flex flex-wrap gap-2">{!revision&&<Button variant="primary" disabled={busy} onClick={plan?apply:createPlan}>{plan?"계획 적용":"저장 및 계획"}</Button>}{revision&&<><Button variant="primary" disabled={busy} onClick={diagnose}>진단</Button><Button disabled={busy} onClick={teardown}>구성 해제</Button></>}</div>
 </Panel>
}
