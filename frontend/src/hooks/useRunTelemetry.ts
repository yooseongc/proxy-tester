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

  useEffect(() => {
    const protocol = window.location.protocol === "https:" ? "wss" : "ws";
    const socket = new WebSocket(`${protocol}://${window.location.host}/api/events/ws`);
    socket.onmessage = (event) => {
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
    };
    return () => socket.close();
  }, []);

  return { activeRun, points, setPoints, setStatus, status };
}
