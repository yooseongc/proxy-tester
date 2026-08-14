import { useCallback, useEffect, useId, useMemo, useRef, useState } from "react";
import { Activity, FastForward, Rewind } from "lucide-react";
import type { EChartsOption, LineSeriesOption } from "echarts";
import { EChart } from "./EChart";
import {
  buildChartPoints,
  formatBandwidth,
  formatLatency,
  stageBands,
  type ChartPoint,
  type Point,
  type Scenario,
} from "./model";
import { Button, type Theme } from "./ui";

type Axis = "left" | "right";
type SeriesDef = {
  key: keyof ChartPoint;
  name: string;
  color: keyof Palette;
  axis?: Axis;
  dash?: "dashed" | "dotted";
  format?: (value: number) => string;
};
type Palette = {
  signal: string;
  info: string;
  warn: string;
  critical: string;
  violet: string;
  pink: string;
  text: string;
  dim: string;
  line: string;
  grid: string;
  panel: string;
  stage: string;
  excluded: string;
};
const palettes: Record<Theme, Palette> = {
  dark: {
    signal: "#2dd4aa",
    info: "#68a0ff",
    warn: "#f2b84b",
    critical: "#ff747f",
    violet: "#ae8cff",
    pink: "#f17ab3",
    text: "#e9f2f3",
    dim: "#82979b",
    line: "#1d353a",
    grid: "#28414780",
    panel: "#0b171a",
    stage: "#68a0ff16",
    excluded: "#87999d12",
  },
  light: {
    signal: "#087f67",
    info: "#3768c9",
    warn: "#9a6707",
    critical: "#b52e3a",
    violet: "#7254b8",
    pink: "#a83f73",
    text: "#102226",
    dim: "#61777c",
    line: "#c7d6d9",
    grid: "#a9bcbf80",
    panel: "#fbfdfd",
    stage: "#3768c914",
    excluded: "#61777c10",
  },
};
const groups: { title: string; unit: string; series: SeriesDef[] }[] = [
  {
    title: "CPS",
    unit: "connections / second",
    series: [{ key: "cps", name: "CPS", color: "signal" }],
  },
  {
    title: "TPS",
    unit: "transactions / second",
    series: [{ key: "tps", name: "TPS", color: "info" }],
  },
  {
    title: "VU · Active Connection",
    unit: "virtual users / connections",
    series: [
      { key: "target_vu", name: "목표 VU", color: "warn", dash: "dashed" },
      { key: "active_connections", name: "Active connections", color: "violet" },
    ],
  },
  {
    title: "TCP Latency",
    unit: "milliseconds",
    series: [
      { key: "tcp_p50", name: "TCP P50", color: "signal", format: formatLatency },
      { key: "tcp_p95", name: "TCP P95", color: "signal", dash: "dashed", format: formatLatency },
      { key: "tcp_p99", name: "TCP P99", color: "signal", dash: "dotted", format: formatLatency },
    ],
  },
  {
    title: "HTTP Latency",
    unit: "milliseconds",
    series: [
      { key: "http_p50", name: "HTTP P50", color: "info", format: formatLatency },
      { key: "http_p95", name: "HTTP P95", color: "info", dash: "dashed", format: formatLatency },
      { key: "http_p99", name: "HTTP P99", color: "info", dash: "dotted", format: formatLatency },
    ],
  },
  {
    title: "처리량",
    unit: "Kbps / Mbps / Gbps",
    series: [
      { key: "client_app_tx", name: "Client App TX", color: "signal", format: formatBandwidth },
      { key: "client_app_rx", name: "Client App RX", color: "info", format: formatBandwidth },
      { key: "server_app_tx", name: "Server App TX", color: "violet", format: formatBandwidth },
      { key: "server_app_rx", name: "Server App RX", color: "pink", format: formatBandwidth },
      {
        key: "client_wire_tx",
        name: "Client Wire TX",
        color: "signal",
        dash: "dashed",
        format: formatBandwidth,
      },
      {
        key: "client_wire_rx",
        name: "Client Wire RX",
        color: "info",
        dash: "dashed",
        format: formatBandwidth,
      },
      {
        key: "server_wire_tx",
        name: "Server Wire TX",
        color: "violet",
        dash: "dashed",
        format: formatBandwidth,
      },
      {
        key: "server_wire_rx",
        name: "Server Wire RX",
        color: "pink",
        dash: "dashed",
        format: formatBandwidth,
      },
    ],
  },
  {
    title: "품질",
    unit: "events / second",
    series: [
      { key: "connection_failures_per_sec", name: "Connection failure/s", color: "critical" },
      { key: "http_errors_per_sec", name: "HTTP transaction error/s", color: "warn" },
      { key: "tcp_retransmissions_per_sec", name: "TCP retransmission/s", color: "violet" },
    ],
  },
];

const axisLabel = (value: number) =>
  value >= 1e9
    ? `${(value / 1e9).toFixed(1)}G`
    : value >= 1e6
      ? `${(value / 1e6).toFixed(1)}M`
      : value >= 1e3
        ? `${(value / 1e3).toFixed(1)}K`
        : String(Math.round(value));
const tooltipHtml = (params: unknown, defs: SeriesDef[], palette: Palette) => {
  const rows = (Array.isArray(params) ? params : []) as Array<{
    seriesName: string;
    value: [number, number];
    color: string;
  }>;
  if (!rows.length) return "";
  const elapsed = rows[0].value[0];
  return `<div class="echarts-tooltip" style="min-width:180px;padding:10px 12px;background:${palette.panel}f2;border:1px solid ${palette.line};border-radius:12px;color:${palette.text};box-shadow:0 18px 50px #0006"><div style="margin-bottom:8px;color:${palette.dim};font:600 10px 'JetBrains Mono Variable'">T + ${(elapsed / 1000).toFixed(1)}s</div>${rows
    .map((row) => {
      const def = defs.find((item) => item.name === row.seriesName);
      const value = def?.format
        ? def.format(Number(row.value[1]))
        : Number(row.value[1]).toFixed(2);
      return `<div style="display:flex;justify-content:space-between;gap:20px;margin:5px 0;font-size:11px"><span><i style="display:inline-block;width:7px;height:7px;border-radius:50%;background:${row.color};margin-right:7px"></i>${row.seriesName}</span><b style="font-family:'JetBrains Mono Variable'">${value}</b></div>`;
    })
    .join("")}</div>`;
};

export function buildTelemetryOption(
  data: ChartPoint[],
  scenario: Scenario,
  defs: SeriesDef[],
  theme: Theme,
  hidden: Set<string>,
  range: [number, number],
): EChartsOption {
  defs = defs.filter((item) => !hidden.has(String(item.key)));
  const palette = palettes[theme],
    bands = stageBands(scenario.load_stages ?? []);
  const series: LineSeriesOption[] = defs.map((def, index) => ({
    name: def.name,
    type: "line",
    showSymbol: false,
    symbol: "none",
    animation: false,
    silent: false,
    yAxisIndex: def.axis === "right" ? 1 : 0,
    lineStyle: { color: palette[def.color], width: 1.8, type: def.dash ?? "solid" },
    itemStyle: { color: palette[def.color] },
    data: data.map((point) => [point.elapsed_ms, Number(point[def.key])]),
    sampling: "none",
    connectNulls: false,
    ...(index === 0
      ? {
          markArea: {
            silent: true,
            label: {
              show: true,
              position: "insideTop",
              color: palette.dim,
              fontSize: 9,
              formatter: (value: { name?: string }) => value.name ?? "",
            },
            data: bands.map((stage) => [
              {
                name: `${stage.name} · ${stage.mode === "ramp" ? "Ramp" : "Hold"}${stage.included ? "" : " · 집계 제외"}`,
                xAxis: stage.start_ms,
                itemStyle: {
                  color: stage.included ? palette.stage : palette.excluded,
                  decal: stage.included
                    ? undefined
                    : {
                        symbol: "rect",
                        dashArrayX: [1, 3],
                        dashArrayY: [3, 3],
                        color: palette.line,
                      },
                },
              },
              { xAxis: stage.end_ms },
            ]),
          },
          markLine: {
            silent: true,
            symbol: "none",
            label: { show: false },
            lineStyle: { color: palette.line, type: "dashed", width: 1 },
            data: bands.flatMap((stage) => [{ xAxis: stage.start_ms }, { xAxis: stage.end_ms }]),
          },
        }
      : {}),
  }));
  return {
    animation: false,
    aria: { enabled: true },
    grid: {
      top: 16,
      left: 60,
      right: defs.some((item) => item.axis === "right") ? 58 : 18,
      bottom: 38,
    },
    tooltip: {
      trigger: "axis",
      axisPointer: {
        type: "line",
        snap: true,
        lineStyle: { color: palette.text, width: 1, opacity: 0.45 },
      },
      borderWidth: 0,
      backgroundColor: "transparent",
      padding: 0,
      extraCssText: "box-shadow:none",
      formatter: (value) => tooltipHtml(value, defs, palette),
    },
    xAxis: {
      type: "value",
      min: "dataMin",
      max: "dataMax",
      axisLine: { lineStyle: { color: palette.line } },
      axisTick: { show: false },
      axisLabel: {
        color: palette.dim,
        fontFamily: "JetBrains Mono Variable",
        fontSize: 9,
        formatter: (value: number) => `${Math.round(value / 1000)}s`,
      },
      splitLine: { show: false },
    },
    yAxis: [
      {
        type: "value",
        scale: true,
        min: 0,
        axisLine: { show: false },
        axisTick: { show: false },
        axisLabel: {
          color: palette.dim,
          fontFamily: "JetBrains Mono Variable",
          fontSize: 9,
          formatter: axisLabel,
        },
        splitLine: { lineStyle: { color: palette.grid } },
      },
      {
        type: "value",
        scale: true,
        min: 0,
        show: defs.some((item) => item.axis === "right"),
        axisLine: { show: false },
        axisTick: { show: false },
        axisLabel: {
          color: palette.dim,
          fontFamily: "JetBrains Mono Variable",
          fontSize: 9,
          formatter: axisLabel,
        },
        splitLine: { show: false },
      },
    ],
    dataZoom: [
      {
        type: "inside",
        xAxisIndex: 0,
        filterMode: "none",
        start: range[0],
        end: range[1],
        zoomOnMouseWheel: true,
        moveOnMouseMove: true,
        moveOnMouseWheel: true,
      },
    ],
    series: series.map((item) => ({ ...item, show: !hidden.has(String(item.name)) })),
  };
}

function navigatorOption(data: ChartPoint[], theme: Theme, range: [number, number]): EChartsOption {
  const p = palettes[theme];
  return {
    animation: false,
    grid: { left: 16, right: 16, top: 3, bottom: 30 },
    tooltip: { show: false },
    xAxis: { type: "value", show: false, min: "dataMin", max: "dataMax" },
    yAxis: { type: "value", show: false },
    series: [
      {
        type: "line",
        showSymbol: false,
        silent: true,
        data: data.map((point) => [point.elapsed_ms, point.cps]),
        lineStyle: { color: p.signal, width: 1 },
        areaStyle: { color: p.stage },
        animation: false,
      },
    ],
    dataZoom: [
      {
        type: "slider",
        xAxisIndex: 0,
        filterMode: "none",
        start: range[0],
        end: range[1],
        height: 18,
        bottom: 4,
        borderColor: p.line,
        backgroundColor: p.panel,
        fillerColor: p.stage,
        handleStyle: { color: p.signal, borderColor: p.signal },
        moveHandleStyle: { color: p.signal },
        textStyle: { color: p.dim, fontFamily: "JetBrains Mono Variable", fontSize: 9 },
        showDetail: false,
      },
      { type: "inside", start: range[0], end: range[1], filterMode: "none" },
    ],
  };
}

export function TelemetryCharts({
  points,
  scenario,
  theme,
  live = false,
  running = false,
}: {
  points: Point[];
  scenario: Scenario;
  theme: Theme;
  live?: boolean;
  running?: boolean;
}) {
  const data = useMemo(() => buildChartPoints(points), [points]),
    groupId = `telemetry-${useId().replaceAll(":", "")}`;
  const [hiddenKeys, setHiddenKeys] = useState<string[]>(() => {
      try {
        return JSON.parse(sessionStorage.getItem("telemetry-hidden-series") ?? "[]");
      } catch {
        return [];
      }
    }),
    [following, setFollowing] = useState(live),
    [range, setRange] = useState<[number, number]>([0, 100]);
  const wasRunning = useRef(running),
    hidden = new Set(hiddenKeys),
    latest = data.at(-1)?.elapsed_ms ?? 0,
    first = data[0]?.elapsed_ms ?? 0;
  const latestRange = useCallback(() => {
    const duration = Math.max(1, latest - first),
      startValue = Math.max(first, latest - 59_000);
    return [Math.max(0, ((startValue - first) / duration) * 100), 100] as [number, number];
  }, [first, latest]);
  const displayedRange = live && following && data.length ? latestRange() : range;
  const previousWindow = () => {
    const width = Math.min(100, (59_000 / Math.max(1, latest - first)) * 100),
      end = Math.max(width, displayedRange[1] - width);
    setRange([Math.max(0, end - width), end]);
    setFollowing(false);
  };
  useEffect(() => {
    if (live) {
      if (!wasRunning.current && running) setFollowing(true);
      else if (wasRunning.current && !running) setFollowing(false);
    }
    wasRunning.current = running;
  }, [running, live]);
  const onZoom = useCallback(
    (start: number, end: number) => {
      setRange((previous) =>
        Math.abs(previous[0] - start) < 0.05 && Math.abs(previous[1] - end) < 0.05
          ? previous
          : [start, end],
      );
      if (live) setFollowing(end >= 99.5);
    },
    [live],
  );
  const toggle = (key: string) =>
    setHiddenKeys((current) => {
      const next = current.includes(key)
        ? current.filter((item) => item !== key)
        : [...current, key];
      sessionStorage.setItem("telemetry-hidden-series", JSON.stringify(next));
      return next;
    });
  if (!data.length)
    return (
      <div className="grid min-h-52 place-items-center rounded-2xl border border-dashed border-line bg-inset/50 text-xs text-dim">
        <div className="text-center">
          <Activity className="mx-auto mb-3 text-signal" />
          <p>아직 표시할 계측 표본이 없습니다.</p>
        </div>
      </div>
    );
  return (
    <section className="space-y-3" data-telemetry-board>
      <div className="flex min-h-10 flex-wrap items-center justify-end gap-3">
        <span
          className={`font-mono text-[10px] font-bold ${following ? "text-signal" : "text-warn"}`}
        >
          {following ? "● 최신 60초 자동 추적" : "● 과거 구간 확인 중"}
        </span>
        {live && latest - first > 59_000 && (
          <Button onClick={previousWindow}>
            <Rewind size={14} />
            이전 60초
          </Button>
        )}
        {live && !following && (
          <Button
            onClick={() => {
              setRange(latestRange());
              setFollowing(true);
            }}
          >
            <FastForward size={14} />
            최신으로
          </Button>
        )}
      </div>
      <nav
        aria-label="차트 바로가기"
        className="sticky top-[122px] z-30 flex gap-1.5 overflow-x-auto rounded-xl border border-line bg-panel/90 p-1.5 shadow-panel backdrop-blur-xl"
      >
        {groups.map((group) => (
          <button
            key={group.title}
            onClick={() =>
              document
                .querySelector(`[data-chart="${group.title}"]`)
                ?.scrollIntoView({ behavior: "smooth", block: "center" })
            }
            className="shrink-0 rounded-lg border border-line bg-raised px-3 py-2 font-mono text-[9px] font-bold text-dim transition hover:border-signal/50 hover:text-signal"
          >
            {group.title}
          </button>
        ))}
      </nav>
      <div className="grid grid-cols-1 gap-3 xl:grid-cols-2">
        {groups.map((group) => (
          <article
            key={group.title}
            data-chart={group.title}
            className="min-w-0 rounded-2xl border border-line bg-raised/45 p-3.5"
          >
            <div className="mb-2 flex items-start justify-between gap-3">
              <div>
                <h3 className="m-0 text-sm font-bold text-ink">{group.title}</h3>
                <p className="mt-1 font-mono text-[9px] uppercase tracking-[.12em] text-dim">
                  {group.unit} · raw 1s
                </p>
              </div>
            </div>
            <div
              className="chart-legend mb-1 flex gap-1.5 overflow-x-auto pb-1"
              aria-label={`${group.title} 범례`}
            >
              {group.series.map((item) => (
                <button
                  key={String(item.key)}
                  aria-pressed={!hidden.has(String(item.key))}
                  onClick={() => toggle(String(item.key))}
                  className={`shrink-0 rounded-md border px-2 py-1 font-mono text-[9px] transition ${hidden.has(String(item.key)) ? "border-line bg-inset text-dim opacity-50" : "border-line bg-panel text-ink"}`}
                >
                  <i
                    className="mr-1.5 inline-block size-1.5 rounded-full"
                    style={{ background: palettes[theme][item.color] }}
                  />
                  {item.name}
                </button>
              ))}
            </div>
            <EChart
              label={`${group.title} 시계열 차트`}
              group={groupId}
              option={buildTelemetryOption(
                data,
                scenario,
                group.series,
                theme,
                hidden,
                displayedRange,
              )}
              onZoom={onZoom}
            />
          </article>
        ))}
      </div>
      <div className="rounded-2xl border border-line bg-raised/45 px-3 pt-2">
        <div className="flex items-center justify-between px-1">
          <span className="font-mono text-[9px] font-bold uppercase tracking-[.14em] text-dim">
            Time navigator
          </span>
          <span className="font-mono text-[9px] text-dim">
            T+{Math.round(first / 1000)}s — T+{Math.round(latest / 1000)}s
          </span>
        </div>
        <EChart
          label="전체 시간 범위 탐색기"
          group={groupId}
          className="echart-nav"
          option={navigatorOption(data, theme, displayedRange)}
          onZoom={onZoom}
        />
      </div>
    </section>
  );
}
