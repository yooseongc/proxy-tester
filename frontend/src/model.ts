export type Agent={id:string;role:number;hostname:string;interfaces:string[];online:boolean};

export type Metrics={
 unix_ms:number;elapsed_ms:number;bytes_tx:number;bytes_rx:number;
 load_stage_index:number;desired_virtual_clients:number;included_in_results:boolean;
 connections_established:number;active_connections:number;active_connections_avg?:number;active_connections_min?:number;active_connections_max?:number;connections_failed:number;transactions:number;transaction_errors:number;timeout_errors?:number;reset_errors?:number;tls_handshake_errors?:number;proxy_connect_errors?:number;http_error_responses?:number;
 cps:number;tps:number;tx_bps:number;rx_bps:number;latency_p99_ms:number;
 tcp_connect_latency_p50_ms:number;tcp_connect_latency_p95_ms:number;tcp_connect_latency_p99_ms:number;
 http_latency_p50_ms:number;http_latency_p95_ms:number;http_latency_p99_ms:number;
 wire_tx_bytes:number;wire_rx_bytes:number;wire_tx_bps:number;wire_rx_bps:number;
 wire_tx_pps:number;wire_rx_pps:number;tcp_retransmissions:number;tcp_retransmissions_per_sec:number;
};

export type Point=Metrics&{agent_id:string;role:number};

export type ChartPoint={
 unix_ms:number;elapsed_ms:number;stage_index:number;
 cps:number;tps:number;target_vu:number;active_connections:number;
 client_app_tx:number;client_app_rx:number;server_app_tx:number;server_app_rx:number;
 client_wire_tx:number;client_wire_rx:number;server_wire_tx:number;server_wire_rx:number;
 tcp_p50:number;tcp_p95:number;tcp_p99:number;http_p50:number;http_p95:number;http_p99:number;
 connection_failures_per_sec:number;http_errors_per_sec:number;tcp_retransmissions_per_sec:number;
};

export type StageBand={name:string;mode:'ramp'|'hold';start_ms:number;end_ms:number;included:boolean};

export function closestSample(reference:Point|undefined,candidates:Point[],maxSkewMs=1500){
 if(!reference)return undefined;
 const closest=candidates.reduce<Point|undefined>((best,current)=>
  !best||Math.abs(current.unix_ms-reference.unix_ms)<Math.abs(best.unix_ms-reference.unix_ms)?current:best,undefined);
 return closest&&Math.abs(closest.unix_ms-reference.unix_ms)<=maxSkewMs?closest:undefined;
}

const deltaRate=(current:number,previous:number|undefined,intervalMs:number)=>
 previous===undefined||intervalMs<=0?0:Math.max(0,current-previous)/(intervalMs/1000);

export function buildChartPoints(points:Point[]):ChartPoint[]{
 const clients=points.filter(point=>point.role===1).sort((a,b)=>a.unix_ms-b.unix_ms);
 const servers=points.filter(point=>point.role===2).sort((a,b)=>a.unix_ms-b.unix_ms);
 return clients.map((client,index)=>{
  const previous=clients[index-1],server=closestSample(client,servers);
  const interval=previous?client.unix_ms-previous.unix_ms:0;
  return {unix_ms:client.unix_ms,elapsed_ms:client.elapsed_ms,stage_index:client.load_stage_index,
   cps:client.cps,tps:client.tps,target_vu:client.desired_virtual_clients,active_connections:client.active_connections,
   client_app_tx:client.tx_bps,client_app_rx:client.rx_bps,server_app_tx:server?.tx_bps??0,server_app_rx:server?.rx_bps??0,
   client_wire_tx:client.wire_tx_bps,client_wire_rx:client.wire_rx_bps,server_wire_tx:server?.wire_tx_bps??0,server_wire_rx:server?.wire_rx_bps??0,
   tcp_p50:client.tcp_connect_latency_p50_ms,tcp_p95:client.tcp_connect_latency_p95_ms,tcp_p99:client.tcp_connect_latency_p99_ms,
   http_p50:client.http_latency_p50_ms,http_p95:client.http_latency_p95_ms,http_p99:client.http_latency_p99_ms,
   connection_failures_per_sec:deltaRate(client.connections_failed,previous?.connections_failed,interval),
   http_errors_per_sec:deltaRate(client.transaction_errors??0,previous?.transaction_errors,interval),
   tcp_retransmissions_per_sec:client.tcp_retransmissions_per_sec??0};
 });
}

export function stageBands(stages:LoadStage[]):StageBand[]{
 let cursor=0;
 return stages.map(stage=>{const start=cursor;cursor+=stage.duration_secs*1000;return {name:stage.name,mode:stage.mode,start_ms:start,end_ms:cursor,included:stage.include_in_results};});
}

export function formatBandwidth(value:number){
 const absolute=Math.abs(value);const [divisor,unit]=absolute>=1e9?[1e9,'Gbps']:absolute>=1e6?[1e6,'Mbps']:[1e3,'Kbps'];
 return `${(value/divisor).toFixed(absolute>=divisor*100?0:2)} ${unit}`;
}
export const formatLatency=(value:number)=>`${value.toFixed(2)} ms`;

export function recentActiveMaximum(points:Point[],windowMs=60_000){
 const latest=points.at(-1);if(!latest)return 0;
 return Math.max(...points.filter(point=>point.unix_ms>latest.unix_ms-windowMs).map(point=>point.active_connections_max??point.active_connections));
}

export type Scenario={
 version:number;id:string;name:string;topology:'explicit_proxy'|'transparent_proxy';
 protocol:'tcp'|'http1'|'connect';client_agent_id:string;server_agent_id:string;
 proxy_addr:string|null;target_addr:string;source_ips:string[];virtual_clients:number;
 duration_secs:number;warmup_secs:number;
 load_stages:LoadStage[];
 request:{method:string;path:string;host:string;request_body_bytes:number;response_body_bytes:number;keep_alive:boolean;transactions_per_connection:number;think_time_ms:number};
 tcp:{tx_bytes:number;rx_bytes:number};
 payload_mode:'manual'|'capture_replay';capture_artifact_id:string|null;request_payload:PayloadProfile|null;response_payload:PayloadProfile|null;
 tls:{enabled:boolean;verify_peer:boolean;version:'tls12'|'tls13';cipher_suite:string|null;server_name:string;ca_pem:string|null;server_cert_pem:string|null;server_key_pem:string|null};
 timeouts:{connect_ms:number;proxy_connect_ms:number;response_ms:number};
 observation_interfaces:string[];
};
export type PayloadProfile={kind:'empty'|'fixed'|'text'|'file'|'random';size_bytes:number;text:string;artifact_id:string|null;random_format:'binary'|'printable_ascii'};

export type LoadStage={name:string;mode:'ramp'|'hold';duration_secs:number;target_virtual_clients:number;include_in_results:boolean};

export const initialScenario=():Scenario=>({
 version:2,id:crypto.randomUUID(),name:'기본 TCP 시험',topology:'transparent_proxy',protocol:'tcp',
 client_agent_id:'client-1',server_agent_id:'server-1',proxy_addr:null,target_addr:'server:8080',
 source_ips:[],virtual_clients:100,duration_secs:50,warmup_secs:0,
 load_stages:[
  {name:'Ramp-up',mode:'ramp',duration_secs:10,target_virtual_clients:100,include_in_results:false},
  {name:'Steady state',mode:'hold',duration_secs:30,target_virtual_clients:100,include_in_results:true},
  {name:'Ramp-down',mode:'ramp',duration_secs:10,target_virtual_clients:0,include_in_results:true}
 ],
 request:{method:'GET',path:'/',host:'proxy-tester.local',request_body_bytes:0,response_body_bytes:128,keep_alive:true,transactions_per_connection:1,think_time_ms:0},
 tcp:{tx_bytes:64,rx_bytes:64},payload_mode:'manual',capture_artifact_id:null,request_payload:{kind:'fixed',size_bytes:64,text:'',artifact_id:null,random_format:'binary'},response_payload:{kind:'fixed',size_bytes:64,text:'',artifact_id:null,random_format:'binary'},tls:{enabled:false,verify_peer:false,version:'tls13',cipher_suite:null,server_name:'proxy-tester.local',ca_pem:null,server_cert_pem:null,server_key_pem:null},
 timeouts:{connect_ms:3000,proxy_connect_ms:3000,response_ms:5000},observation_interfaces:[]
});
