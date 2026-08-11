import {describe,expect,it} from 'vitest';
import {buildChartPoints,closestSample,formatBandwidth,formatLatency,recentActiveMaximum,stageBands,type Point} from './model';

const point=(unix_ms:number)=>({unix_ms} as Point);
describe('closestSample',()=>{
 it('matches telemetry by timestamp rather than arrival order',()=>{
  expect(closestSample(point(2_000),[point(900),point(2_050),point(3_100)])?.unix_ms).toBe(2_050);
 });
 it('rejects a stale peer sample',()=>expect(closestSample(point(5_000),[point(1_000)])).toBeUndefined());
});

const metricPoint=(role:number,unix_ms:number,values:Partial<Point>={}):Point=>({
 agent_id:`agent-${role}`,role,unix_ms,elapsed_ms:unix_ms-1000,load_stage_index:0,desired_virtual_clients:10,included_in_results:true,
 bytes_tx:0,bytes_rx:0,connections_established:0,active_connections:3,connections_failed:0,transactions:0,transaction_errors:0,
 cps:1,tps:2,tx_bps:1000,rx_bps:2000,latency_p99_ms:0,tcp_connect_latency_p50_ms:1,tcp_connect_latency_p95_ms:2,tcp_connect_latency_p99_ms:3,
 http_latency_p50_ms:4,http_latency_p95_ms:5,http_latency_p99_ms:6,wire_tx_bytes:0,wire_rx_bytes:0,wire_tx_bps:3000,wire_rx_bps:4000,
 wire_tx_pps:0,wire_rx_pps:0,tcp_retransmissions:0,tcp_retransmissions_per_sec:0,...values});

describe('chart model',()=>{
 it('aligns each client with the nearest server sample',()=>{
  const result=buildChartPoints([metricPoint(2,2050,{tx_bps:9000}),metricPoint(1,2000)]);
  expect(result[0].server_app_tx).toBe(9000);
 });
 it('derives counter rates using the real interval and clamps resets',()=>{
  const result=buildChartPoints([metricPoint(1,1000,{connections_failed:10,transaction_errors:4}),metricPoint(1,3000,{connections_failed:14,transaction_errors:2})]);
  expect(result[1].connection_failures_per_sec).toBe(2);
  expect(result[1].http_errors_per_sec).toBe(0);
 });
 it('computes cumulative stage boundaries and exclusion state',()=>expect(stageBands([
  {name:'Ramp',mode:'ramp',duration_secs:2,target_virtual_clients:1,include_in_results:false},
  {name:'Hold',mode:'hold',duration_secs:3,target_virtual_clients:1,include_in_results:true}
 ])).toEqual([{name:'Ramp',mode:'ramp',start_ms:0,end_ms:2000,included:false},{name:'Hold',mode:'hold',start_ms:2000,end_ms:5000,included:true}]));
 it('formats bandwidth and latency units',()=>{expect(formatBandwidth(2_500_000)).toBe('2.50 Mbps');expect(formatBandwidth(1_500_000_000)).toBe('1.50 Gbps');expect(formatLatency(1.236)).toBe('1.24 ms')});
 it('uses per-sample peaks from only the latest one-minute ACTIVE window',()=>{
  const samples=[metricPoint(1,1_000,{active_connections_max:99}),metricPoint(1,61_001,{active_connections_max:7}),metricPoint(1,120_000,{active_connections_max:12})];
  expect(recentActiveMaximum(samples)).toBe(12);
 });
});
