import { useCallback, useEffect, useState } from "react";
import { api } from "../api";
import type { Agent, Artifact, Scenario } from "../model";

export function useControlResources() {
  const [agents, setAgents] = useState<Agent[]>([]);
  const [scenarios, setScenarios] = useState<Scenario[]>([]);
  const [artifacts, setArtifacts] = useState<Artifact[]>([]);

  const refreshScenarios = useCallback(async () => {
    setScenarios(await api<Scenario[]>("/api/scenarios"));
  }, []);

  const refreshArtifacts = useCallback(async () => {
    setArtifacts(await api<Artifact[]>("/api/artifacts"));
  }, []);

  useEffect(() => {
    let active = true;
    const refreshAgents = async () => {
      try {
        const result = await api<Agent[]>("/api/agents");
        if (active) setAgents(result);
      } catch {
        // Agent polling is best effort; command APIs surface actionable failures.
      }
    };
    void refreshAgents();
    const timer = window.setInterval(refreshAgents, 3_000);
    return () => {
      active = false;
      window.clearInterval(timer);
    };
  }, []);

  useEffect(() => {
    void Promise.all([api<Scenario[]>("/api/scenarios"), api<Artifact[]>("/api/artifacts")])
      .then(([nextScenarios, nextArtifacts]) => {
        setScenarios(nextScenarios);
        setArtifacts(nextArtifacts);
      })
      .catch(() => {});
  }, []);

  return { agents, artifacts, refreshArtifacts, refreshScenarios, scenarios };
}
