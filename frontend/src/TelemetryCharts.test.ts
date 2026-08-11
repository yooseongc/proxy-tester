import {describe,expect,it} from 'vitest';
import {buildTelemetryOption} from './TelemetryCharts';
import type {ChartPoint,Scenario} from './model';

const scenario={load_stages:[{name:'Warm-up',mode:'ramp',duration_secs:10,target_virtual_clients:10,include_in_results:false},{name:'Measure',mode:'hold',duration_secs:20,target_virtual_clients:10,include_in_results:true}]} as Scenario;
const point={elapsed_ms:1000,cps:12} as ChartPoint;
describe('ECharts telemetry options',()=>{it('creates raw line data, stage areas and a 60-second-compatible zoom',()=>{const option=buildTelemetryOption([point],scenario,[{key:'cps',name:'CPS',color:'signal'}],'dark',new Set(),[20,100]);const series=(option.series as Array<{data:unknown[];sampling:string;markArea:{data:unknown[]}}>)[0];expect(series.data).toEqual([[1000,12]]);expect(series.sampling).toBe('none');expect(series.markArea.data).toHaveLength(2);expect((option.dataZoom as Array<{start:number;end:number}>)[0]).toMatchObject({start:20,end:100})});it('omits hidden series from the canvas option',()=>{const option=buildTelemetryOption([point],scenario,[{key:'cps',name:'CPS',color:'signal'}],'light',new Set(['cps']),[0,100]);expect(option.series).toEqual([])})});
