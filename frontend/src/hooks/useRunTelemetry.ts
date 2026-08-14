import { useEffect, useState } from "react";
import type { Metrics, Point } from "../model";

type EventMessage = {
  type: string;
  run_id?: string;
  status?: string;
  agent_id?: string;
  role?: number;
  data?: Metrics;
};

export function useRunTelemetry() {
  const [points, setPoints] = useState<Point[]>([]);
  const [status, setStatus] = useState("대기 중");
  const [activeRun, setActiveRun] = useState<string | null>(null);
  const [diagnosticRevision, setDiagnosticRevision] = useState(0);

  useEffect(() => {
    const protocol = window.location.protocol === "https:" ? "wss" : "ws";
    let socket: WebSocket | null = null;
    let reconnectTimer: number | undefined;
    let disposed = false;
    const onMessage = (event: MessageEvent) => {
      let message: EventMessage;
      try {
        message = JSON.parse(event.data) as EventMessage;
      } catch {
        return;
      }
      if (message.type === "metrics" && message.data && message.agent_id && message.role) {
        const point: Point = {
          ...message.data,
          agent_id: message.agent_id,
          role: message.role,
        };
        setPoints((current) => [...current, point].slice(-7_200));
      } else if (message.type === "run_started" && message.run_id) {
        setStatus("실행 중");
        setActiveRun(message.run_id);
      } else if (message.type === "run_state") {
        setStatus(message.status === "paused" ? "일시 정지" : "실행 중");
      } else if (message.type === "run_finished") {
        setStatus(message.status ?? "완료");
        setActiveRun(null);
      }
      if (
        ["network_progress", "agent_event", "run_state", "run_started", "run_finished"].includes(
          message.type,
        )
      ) {
        setDiagnosticRevision((current) => current + 1);
      }
    };
    const connect = () => {
      socket = new WebSocket(`${protocol}://${window.location.host}/api/events/ws`);
      socket.onmessage = onMessage;
      socket.onopen = () => setDiagnosticRevision((current) => current + 1);
      socket.onclose = () => {
        if (!disposed) reconnectTimer = window.setTimeout(connect, 1_000);
      };
    };
    connect();
    return () => {
      disposed = true;
      if (reconnectTimer) window.clearTimeout(reconnectTimer);
      socket?.close();
    };
  }, []);

  return { activeRun, diagnosticRevision, points, setPoints, setStatus, status };
}
