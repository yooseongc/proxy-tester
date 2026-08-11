import {useEffect,useRef} from 'react';
import * as echarts from 'echarts/core';
import {LineChart} from 'echarts/charts';
import {AriaComponent,DataZoomComponent,DatasetComponent,GridComponent,LegendComponent,MarkAreaComponent,MarkLineComponent,TooltipComponent} from 'echarts/components';
import {CanvasRenderer} from 'echarts/renderers';
import type {EChartsOption} from 'echarts';

echarts.use([LineChart,AriaComponent,DataZoomComponent,DatasetComponent,GridComponent,LegendComponent,MarkAreaComponent,MarkLineComponent,TooltipComponent,CanvasRenderer]);

export function EChart({option,group,className='echart',label,onZoom}:{option:EChartsOption;group:string;className?:string;label:string;onZoom?:(start:number,end:number)=>void}){
 const element=useRef<HTMLDivElement>(null),chart=useRef<echarts.ECharts|null>(null);
 useEffect(()=>{
  if(!element.current)return;
  const instance=echarts.init(element.current,undefined,{renderer:'canvas'});chart.current=instance;instance.group=group;echarts.connect(group);
  const observer=new ResizeObserver(()=>instance.resize());observer.observe(element.current);
  const zoom=(raw:unknown)=>{const event=raw as {start?:number;end?:number;batch?:Array<{start?:number;end?:number}>};const value=event.batch?.[0]??event;if(value.start!==undefined&&value.end!==undefined)onZoom?.(value.start,value.end)};
  instance.on('datazoom',zoom);
  return()=>{observer.disconnect();instance.off('datazoom',zoom);instance.dispose();chart.current=null};
 },[group,onZoom]);
 useEffect(()=>{chart.current?.setOption(option,{notMerge:true,lazyUpdate:true})},[option]);
 return <div ref={element} className={className} role="img" aria-label={label}/>;
}
